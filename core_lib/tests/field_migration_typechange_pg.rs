//! S4 改类型执行器的 PostgreSQL 真库验证（opt-in）。
//!
//! ```bash
//! FIELD_MIGRATION_PG_URL='postgres://postgres:password@127.0.0.1:55432/postgres' \
//!   cargo test -p core_lib --test field_migration_typechange_pg --no-fail-fast -- --test-threads=1
//! ```
//! 未设置环境变量时打印原因并返回，不 `#[ignore]`。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use sqlx::postgres::PgPool;
use sqlx::Executor;

use core_lib::db::client::DbClient;
use core_lib::ops::field_migration::migration_store::{
    FkCounterpartSpec, MigrationHub, MigrationStatus, TypeChangeClass, TypeChangeJob,
};
use core_lib::ops::field_migration::pg_typechange;
use core_lib::ops::policy::OpsPolicy;
use core_lib::ops::registry::OpsContext;

const DB_ID: &str = "field-typechange-pg-e2e";

static DB_SEQ: AtomicU64 = AtomicU64::new(0);

fn unique_db(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "{prefix}_{}_{}_{}",
        std::process::id(),
        DB_SEQ.fetch_add(1, Ordering::Relaxed),
        nanos
    )
}

fn with_database(url: &str, db: &str) -> String {
    match url.rsplit_once('/') {
        Some((base, tail)) => {
            let query = tail.find('?').map(|i| &tail[i..]).unwrap_or("");
            format!("{base}/{db}{query}")
        }
        None => format!("{url}/{db}"),
    }
}

async fn admin_exec(pool: &PgPool, sql: &str) {
    if let Err(err) = pool.execute(sql).await {
        panic!("执行管理 SQL 失败: {err}\nSQL: {sql}");
    }
}

async fn cleanup_database(pool: &PgPool, db: &str) {
    let terminate = format!(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '{db}' AND pid <> pg_backend_pid()"
    );
    let _ = pool.execute(terminate.as_str()).await;
    let sql = format!("DROP DATABASE IF EXISTS \"{db}\" WITH (FORCE)");
    if let Err(err) = pool.execute(sql.as_str()).await {
        panic!("清理失败: 数据库 \"{db}\": {err}");
    }
}

async fn isolated_database(base_url: &str, prefix: &str) -> (String, PgPool) {
    let admin_pool = PgPool::connect(base_url)
        .await
        .expect("连接 PostgreSQL 管理库失败，请确认 phase3-pg 已就绪");
    let db = unique_db(prefix);
    admin_exec(
        &admin_pool,
        &format!("DROP DATABASE IF EXISTS \"{db}\" WITH (FORCE)"),
    )
    .await;
    admin_exec(&admin_pool, &format!("CREATE DATABASE \"{db}\"")).await;
    (db, admin_pool)
}

async fn exec(pool: &PgPool, sql: &str) {
    if let Err(err) = pool.execute(sql).await {
        panic!("执行 SQL 失败: {err}\nSQL: {sql}");
    }
}

async fn ctx_for(url: &str, db_name: &str) -> OpsContext {
    let policy = Arc::new(OpsPolicy::default());
    let client = DbClient::new_default(url)
        .await
        .expect("创建 DbClient 失败");
    OpsContext::new(client, DB_ID.into(), db_name.to_string(), false).with_policy(policy)
}

fn env_url() -> Option<String> {
    match std::env::var("FIELD_MIGRATION_PG_URL") {
        Ok(url) => Some(url),
        Err(_) => {
            eprintln!("跳过改类型真库验证：未设置 FIELD_MIGRATION_PG_URL");
            None
        }
    }
}

/// 从 env 推导带角色的连接串：保留主机端口，替换凭据与库名。
fn url_with_role(base_url: &str, db: &str, role: &str, password: &str) -> String {
    let after_scheme = base_url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(base_url);
    let host_port = after_scheme
        .split('@')
        .nth(1)
        .unwrap_or(after_scheme)
        .split('/')
        .next()
        .unwrap_or("127.0.0.1:5432");
    format!("postgres://{role}:{password}@{host_port}/{db}")
}

/// 列的 format_type 全名。
async fn column_type(pool: &PgPool, table: &str, column: &str) -> String {
    sqlx::query_scalar(
        "SELECT format_type(a.atttypid, a.atttypmod)::text \
         FROM pg_attribute a \
         WHERE a.attrelid = ($1 || '.\"' || $2 || '\"')::regclass AND a.attname = $3",
    )
    // to_regclass 不好与 pg_attribute join 的输入拼混合引号，改走显式 regclass 构造。
    .bind("public")
    .bind(table)
    .bind(column)
    .fetch_one(pool)
    .await
    .unwrap_or_default()
}

async fn row_count(pool: &PgPool, table: &str) -> i64 {
    sqlx::query_scalar(&format!("SELECT COUNT(*) FROM \"{table}\""))
        .fetch_one(pool)
        .await
        .unwrap_or(-1)
}

async fn pkey_valid(pool: &PgPool, table: &str) -> bool {
    sqlx::query_scalar(
        "SELECT i.indisvalid::text FROM pg_index i \
         WHERE i.indrelid = ($1 || '.\"' || $2 || '\"')::regclass AND i.indisprimary",
    )
    .bind("public")
    .bind(table)
    .fetch_one(pool)
    .await
    .map(|v: String| v == "true")
    .unwrap_or(false)
}

async fn new_job(
    ctx: &OpsContext,
    hub: &MigrationHub,
    table: &str,
    column: &str,
    target: &str,
    allow_rewrite: bool,
) -> TypeChangeJob {
    pg_typechange::new_job(
        ctx,
        hub,
        pg_typechange::TypeChangeRequest {
            schema: None,
            table: table.into(),
            column: column.into(),
            target_type: target.into(),
            allow_rewrite,
            production_confirmed: false,
            using_expr: None,
            fk_counterparts: Vec::new(),
        },
    )
    .await
    .expect("创建改类型任务失败")
}

/// 断言 job 被阻断且 phase 命中；返回 job 快照。
async fn expect_blocked(hub: &MigrationHub, job_id: &str, phase: &str) -> TypeChangeJob {
    let job = hub.get_typechange(job_id).await.unwrap();
    assert_eq!(
        job.status,
        MigrationStatus::Blocked,
        "期望 Blocked: phase={:?} err={:?}",
        job.blocking.as_ref().map(|b| b.phase.clone()),
        job.last_error
    );
    let actual = job
        .blocking
        .as_ref()
        .map(|b| b.phase.clone())
        .unwrap_or_default();
    assert!(
        actual == phase,
        "期望阻断 phase={phase}，实际 {actual}；err={:?}",
        job.last_error
    );
    job
}

// ── 免重写路径 ────────────────────────────────────────────

#[tokio::test]
async fn metadata_only_varchar_grow() {
    let Some(base_url) = env_url() else { return };
    let (db, admin) = isolated_database(&base_url, "it_tc_grow").await;
    let url = with_database(&base_url, &db);
    let pool = PgPool::connect(&url).await.expect("连接失败");
    exec(
        &pool,
        "CREATE TABLE orders (id bigint PRIMARY KEY, payload varchar(50))",
    )
    .await;
    exec(
        &pool,
        "INSERT INTO orders SELECT g, 'v' || g FROM generate_series(1, 100) g",
    )
    .await;

    let ctx = ctx_for(&url, &db).await;
    let hub = MigrationHub::new(None);
    let policy = OpsPolicy::default();
    let job = new_job(&ctx, &hub, "orders", "payload", "varchar(200)", false).await;
    pg_typechange::run_job(&job.job_id, &ctx, &hub, &policy)
        .await
        .expect("执行失败");

    let done = hub.get_typechange(&job.job_id).await.unwrap();
    assert_eq!(done.status, MigrationStatus::Completed, "{:?}", done.notes);
    assert_eq!(done.classification, TypeChangeClass::MetadataOnly);
    assert_eq!(done.verify.rows_before, 100);
    assert_eq!(done.verify.rows_after, 100);
    assert!(done.verify.indexes_valid);
    assert_eq!(
        column_type(&pool, "orders", "payload").await,
        "character varying(200)"
    );
    assert_eq!(row_count(&pool, "orders").await, 100);

    cleanup_database(&admin, &db).await;
}

#[tokio::test]
async fn metadata_only_varchar_to_text() {
    let Some(base_url) = env_url() else { return };
    let (db, admin) = isolated_database(&base_url, "it_tc_text").await;
    let url = with_database(&base_url, &db);
    let pool = PgPool::connect(&url).await.expect("连接失败");
    exec(
        &pool,
        "CREATE TABLE orders (id bigint PRIMARY KEY, payload varchar(50))",
    )
    .await;
    exec(
        &pool,
        "INSERT INTO orders VALUES (1, 'a'), (2, repeat('x', 50))",
    )
    .await;

    let ctx = ctx_for(&url, &db).await;
    let hub = MigrationHub::new(None);
    let policy = OpsPolicy::default();
    let job = new_job(&ctx, &hub, "orders", "payload", "text", false).await;
    pg_typechange::run_job(&job.job_id, &ctx, &hub, &policy)
        .await
        .expect("执行失败");

    let done = hub.get_typechange(&job.job_id).await.unwrap();
    assert_eq!(done.status, MigrationStatus::Completed);
    assert_eq!(done.classification, TypeChangeClass::MetadataOnly);
    assert_eq!(done.verify.rows_after, 2);
    assert!(done.verify.indexes_valid);
    assert_eq!(column_type(&pool, "orders", "payload").await, "text");
    let longest: i64 = sqlx::query_scalar("SELECT max(length(payload))::bigint FROM orders")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(longest, 50, "变长数据必须原样保留");

    cleanup_database(&admin, &db).await;
}

#[tokio::test]
async fn metadata_only_numeric_relax() {
    let Some(base_url) = env_url() else { return };
    let (db, admin) = isolated_database(&base_url, "it_tc_num").await;
    let url = with_database(&base_url, &db);
    let pool = PgPool::connect(&url).await.expect("连接失败");
    exec(
        &pool,
        "CREATE TABLE prices (id bigint PRIMARY KEY, amount numeric(10,2))",
    )
    .await;
    exec(&pool, "INSERT INTO prices VALUES (1, 12345.67), (2, 0.01)").await;

    let ctx = ctx_for(&url, &db).await;
    let hub = MigrationHub::new(None);
    let policy = OpsPolicy::default();
    let job = new_job(&ctx, &hub, "prices", "amount", "numeric(14,4)", false).await;
    pg_typechange::run_job(&job.job_id, &ctx, &hub, &policy)
        .await
        .expect("执行失败");

    let done = hub.get_typechange(&job.job_id).await.unwrap();
    assert_eq!(done.status, MigrationStatus::Completed);
    assert_eq!(done.classification, TypeChangeClass::MetadataOnly);
    assert_eq!(
        column_type(&pool, "prices", "amount").await,
        "numeric(14,4)"
    );
    // 值不漂移；numeric(14,4) 的 text 输出按 scale 补零（12345.67 -> 12345.6700）。
    let amount: String = sqlx::query_scalar("SELECT amount::text FROM prices WHERE id = 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(amount, "12345.6700");
    let numeric_equal: bool =
        sqlx::query_scalar("SELECT (amount = 12345.67::numeric)::bool FROM prices WHERE id = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(numeric_equal, "数值必须等于原值 12345.67");

    cleanup_database(&admin, &db).await;
}

// ── 重写路径 ──────────────────────────────────────────────

#[tokio::test]
async fn rewrite_gate_blocks() {
    let Some(base_url) = env_url() else { return };
    let (db, admin) = isolated_database(&base_url, "it_tc_gate").await;
    let url = with_database(&base_url, &db);
    let pool = PgPool::connect(&url).await.expect("连接失败");
    exec(
        &pool,
        "CREATE TABLE orders (id bigint PRIMARY KEY, payload text)",
    )
    .await;
    exec(&pool, "INSERT INTO orders VALUES (1, 'abc')").await;

    let ctx = ctx_for(&url, &db).await;
    let hub = MigrationHub::new(None);
    let policy = OpsPolicy::default();
    let job = new_job(&ctx, &hub, "orders", "payload", "varchar(10)", false).await;
    pg_typechange::run_job(&job.job_id, &ctx, &hub, &policy)
        .await
        .expect("执行失败");
    let blocked = expect_blocked(&hub, &job.job_id, "preflight:rewrite_requires_gate").await;
    assert_eq!(blocked.classification, TypeChangeClass::Rewrite);
    // 未开闸不得动列。
    assert_eq!(column_type(&pool, "orders", "payload").await, "text");

    cleanup_database(&admin, &db).await;
}

#[tokio::test]
async fn rewrite_int_to_bigint() {
    let Some(base_url) = env_url() else { return };
    let (db, admin) = isolated_database(&base_url, "it_tc_i8").await;
    let url = with_database(&base_url, &db);
    let pool = PgPool::connect(&url).await.expect("连接失败");
    exec(
        &pool,
        "CREATE TABLE orders (id integer PRIMARY KEY, payload text)",
    )
    .await;
    exec(
        &pool,
        "INSERT INTO orders SELECT g, 'v' || g FROM generate_series(1, 500) g",
    )
    .await;

    let ctx = ctx_for(&url, &db).await;
    let hub = MigrationHub::new(None);
    let policy = OpsPolicy::default();
    let job = new_job(&ctx, &hub, "orders", "id", "bigint", true).await;
    pg_typechange::run_job(&job.job_id, &ctx, &hub, &policy)
        .await
        .expect("执行失败");

    let done = hub.get_typechange(&job.job_id).await.unwrap();
    assert_eq!(
        done.status,
        MigrationStatus::Completed,
        "notes={:?} err={:?}",
        done.notes,
        done.last_error
    );
    assert_eq!(done.classification, TypeChangeClass::Rewrite);
    // 主键整型推广；数据与索引健康。
    assert_eq!(column_type(&pool, "orders", "id").await, "bigint");
    assert_eq!(row_count(&pool, "orders").await, 500);
    assert!(pkey_valid(&pool, "orders").await);
    assert_eq!(done.verify.rows_before, 500);
    assert_eq!(done.verify.rows_after, 500);
    assert!(done.verify.indexes_valid);

    cleanup_database(&admin, &db).await;
}

#[tokio::test]
async fn rewrite_conversion_failure() {
    let Some(base_url) = env_url() else { return };
    let (db, admin) = isolated_database(&base_url, "it_tc_conv").await;
    let url = with_database(&base_url, &db);
    let pool = PgPool::connect(&url).await.expect("连接失败");
    exec(
        &pool,
        "CREATE TABLE orders (id bigint PRIMARY KEY, code text)",
    )
    .await;
    exec(
        &pool,
        "INSERT INTO orders VALUES (1, '100'), (2, 'abc'), (3, '300')",
    )
    .await;

    let ctx = ctx_for(&url, &db).await;
    let hub = MigrationHub::new(None);
    let policy = OpsPolicy::default();
    let job = new_job(&ctx, &hub, "orders", "code", "integer", true).await;
    pg_typechange::run_job(&job.job_id, &ctx, &hub, &policy)
        .await
        .expect("执行失败");
    let blocked = expect_blocked(&hub, &job.job_id, "apply:conversion_failed").await;
    let message = blocked.last_error.unwrap_or_default();
    assert!(message.contains("abc"), "DETAIL/报错应含违规值: {message}");
    // DDL 原子回滚：列保持 text。
    assert_eq!(column_type(&pool, "orders", "code").await, "text");

    // 修复数据后重跑（续跑 = 重跑语句）。
    exec(&pool, "UPDATE orders SET code = '200' WHERE id = 2").await;
    pg_typechange::run_job(&job.job_id, &ctx, &hub, &policy)
        .await
        .expect("续跑失败");
    let done = hub.get_typechange(&job.job_id).await.unwrap();
    assert_eq!(
        done.status,
        MigrationStatus::Completed,
        "{:?}",
        done.last_error
    );
    assert_eq!(column_type(&pool, "orders", "code").await, "integer");
    let sum: i64 = sqlx::query_scalar("SELECT sum(code)::bigint FROM orders")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(sum, 600);

    cleanup_database(&admin, &db).await;
}

// ── 依赖与确定性阻断 ──────────────────────────────────────

#[tokio::test]
async fn dependent_view_block() {
    let Some(base_url) = env_url() else { return };
    let (db, admin) = isolated_database(&base_url, "it_tc_view").await;
    let url = with_database(&base_url, &db);
    let pool = PgPool::connect(&url).await.expect("连接失败");
    exec(
        &pool,
        "CREATE TABLE orders (id bigint PRIMARY KEY, payload varchar(50))",
    )
    .await;
    exec(&pool, "INSERT INTO orders VALUES (1, 'abc')").await;
    exec(&pool, "CREATE VIEW v_orders AS SELECT payload FROM orders").await;

    let ctx = ctx_for(&url, &db).await;
    let hub = MigrationHub::new(None);
    let policy = OpsPolicy::default();
    // varchar(50)->varchar(200) 本可免重写执行，但视图依赖一律阻断。
    let job = new_job(&ctx, &hub, "orders", "payload", "varchar(200)", false).await;
    pg_typechange::run_job(&job.job_id, &ctx, &hub, &policy)
        .await
        .expect("执行失败");
    let blocked = expect_blocked(&hub, &job.job_id, "preflight:dependent_views").await;
    assert!(blocked.last_error.unwrap_or_default().contains("v_orders"));
    assert_eq!(
        column_type(&pool, "orders", "payload").await,
        "character varying(50)"
    );

    cleanup_database(&admin, &db).await;
}

#[tokio::test]
async fn foreign_key_block() {
    let Some(base_url) = env_url() else { return };
    let (db, admin) = isolated_database(&base_url, "it_tc_fk").await;
    let url = with_database(&base_url, &db);
    let pool = PgPool::connect(&url).await.expect("连接失败");
    exec(
        &pool,
        "CREATE TABLE parents (id integer PRIMARY KEY, note text)",
    )
    .await;
    exec(
        &pool,
        "CREATE TABLE children (id bigint PRIMARY KEY, parent_id integer \
         REFERENCES parents(id))",
    )
    .await;

    let ctx = ctx_for(&url, &db).await;
    let hub = MigrationHub::new(None);
    let policy = OpsPolicy::default();
    // 被引用主键 int -> bigint，开闸也阻断（外键）。
    // 断言依据 specs/pg-field-governance-s5/spec.md §7：S5 起外键由「一律阻断」改为
    // 「显式对端编排」，阻断条件收窄为「列参与外键且未声明 fk_counterparts」，
    // 故 phase 由 preflight:foreign_keys 改为 preflight:fk_counterparts_missing；
    // 约束名仍必须进入 detail，否则用户无从知道该声明哪个对端。
    let job = new_job(&ctx, &hub, "parents", "id", "bigint", true).await;
    pg_typechange::run_job(&job.job_id, &ctx, &hub, &policy)
        .await
        .expect("执行失败");
    let blocked = expect_blocked(&hub, &job.job_id, "preflight:fk_counterparts_missing").await;
    let message = blocked.last_error.unwrap_or_default();
    assert!(
        message.contains("children_parent_id_fkey"),
        "约束名应入 detail: {message}"
    );

    cleanup_database(&admin, &db).await;
}

#[tokio::test]
async fn non_deterministic_block() {
    let Some(base_url) = env_url() else { return };
    let (db, admin) = isolated_database(&base_url, "it_tc_tz").await;
    let url = with_database(&base_url, &db);
    let pool = PgPool::connect(&url).await.expect("连接失败");
    exec(
        &pool,
        "CREATE TABLE events (id bigint PRIMARY KEY, at timestamp without time zone)",
    )
    .await;
    exec(
        &pool,
        "INSERT INTO events VALUES (1, '2026-01-01 12:00:00')",
    )
    .await;

    let ctx = ctx_for(&url, &db).await;
    let hub = MigrationHub::new(None);
    let policy = OpsPolicy::default();
    // 开闸也不许：timestamp -> timestamptz 依赖会话时区。
    let job = new_job(&ctx, &hub, "events", "at", "timestamptz", true).await;
    pg_typechange::run_job(&job.job_id, &ctx, &hub, &policy)
        .await
        .expect("执行失败");
    expect_blocked(&hub, &job.job_id, "preflight:non_deterministic_cast").await;
    assert_eq!(
        column_type(&pool, "events", "at").await,
        "timestamp without time zone"
    );

    cleanup_database(&admin, &db).await;
}

// ── 续跑语义 ──────────────────────────────────────────────

#[tokio::test]
async fn resume_self_heal() {
    let Some(base_url) = env_url() else { return };
    let (db, admin) = isolated_database(&base_url, "it_tc_heal").await;
    let url = with_database(&base_url, &db);
    let pool = PgPool::connect(&url).await.expect("连接失败");
    exec(
        &pool,
        "CREATE TABLE orders (id bigint PRIMARY KEY, payload text)",
    )
    .await;
    exec(&pool, "INSERT INTO orders VALUES (1, 'abc')").await;

    let ctx = ctx_for(&url, &db).await;
    let hub = MigrationHub::new(None);
    let policy = OpsPolicy::default();
    // 第一次运行：Rewrite 未开闸 → 阻断，但 source_type/指纹已记录。
    let job = new_job(&ctx, &hub, "orders", "payload", "varchar(50)", false).await;
    pg_typechange::run_job(&job.job_id, &ctx, &hub, &policy)
        .await
        .expect("执行失败");
    let blocked = expect_blocked(&hub, &job.job_id, "preflight:rewrite_requires_gate").await;
    assert_eq!(blocked.source_type, "text");
    assert!(!blocked.spec_hash.is_empty());

    // 模拟中断期间外部把列改到了目标类型。
    exec(
        &pool,
        "ALTER TABLE orders ALTER COLUMN payload TYPE varchar(50) USING payload::varchar(50)",
    )
    .await;

    // 续跑：live == target → 自愈为 no-op 完成，不重复执行 DDL。
    pg_typechange::run_job(&job.job_id, &ctx, &hub, &policy)
        .await
        .expect("续跑失败");
    let done = hub.get_typechange(&job.job_id).await.unwrap();
    assert_eq!(
        done.status,
        MigrationStatus::Completed,
        "{:?}",
        done.last_error
    );
    assert!(done.notes.iter().any(|n| n.contains("no-op")));
    assert_eq!(
        done.verify.live_type.as_deref(),
        Some("character varying(50)")
    );

    cleanup_database(&admin, &db).await;
}

#[tokio::test]
async fn lock_timeout_mapping() {
    let Some(base_url) = env_url() else { return };
    let (db, admin) = isolated_database(&base_url, "it_tc_lock").await;
    let url = with_database(&base_url, &db);
    let pool = PgPool::connect(&url).await.expect("连接失败");
    exec(
        &pool,
        "CREATE TABLE orders (id bigint PRIMARY KEY, payload varchar(50))",
    )
    .await;
    exec(&pool, "INSERT INTO orders VALUES (1, 'abc')").await;

    let ctx = ctx_for(&url, &db).await;
    let hub = MigrationHub::new(None);
    let mut policy = OpsPolicy::default();
    policy.lock_wait_timeout_secs = 2;
    let job = new_job(&ctx, &hub, "orders", "payload", "varchar(200)", false).await;

    // 另一会话持 SHARE 锁：preflight 的 COUNT(*)（ACCESS SHARE）不受影响，
    // 而 ALTER 需要 ACCESS EXCLUSIVE，冲突等锁 → 超时报 55P03。
    // （持 ACCESS EXCLUSIVE 会让 preflight 的 COUNT 快照同样永等，故用 SHARE。）
    let mut lock_conn = pool.acquire().await.unwrap();
    lock_conn.execute("BEGIN").await.unwrap();
    lock_conn
        .execute("LOCK TABLE orders IN SHARE MODE")
        .await
        .unwrap();
    pg_typechange::run_job(&job.job_id, &ctx, &hub, &policy)
        .await
        .expect("执行失败");
    expect_blocked(&hub, &job.job_id, "apply:lock_timeout").await;
    assert_eq!(
        column_type(&pool, "orders", "payload").await,
        "character varying(50)"
    );

    // 释放锁后续跑完成。
    lock_conn.execute("ROLLBACK").await.unwrap();
    drop(lock_conn);
    pg_typechange::run_job(&job.job_id, &ctx, &hub, &policy)
        .await
        .expect("续跑失败");
    let done = hub.get_typechange(&job.job_id).await.unwrap();
    assert_eq!(
        done.status,
        MigrationStatus::Completed,
        "{:?}",
        done.last_error
    );
    assert_eq!(
        column_type(&pool, "orders", "payload").await,
        "character varying(200)"
    );

    cleanup_database(&admin, &db).await;
}

#[tokio::test]
async fn spec_hash_mismatch_rejects_tampered_job() {
    let Some(base_url) = env_url() else { return };
    let (db, admin) = isolated_database(&base_url, "it_tc_hash").await;
    let url = with_database(&base_url, &db);
    let pool = PgPool::connect(&url).await.expect("连接失败");
    exec(
        &pool,
        "CREATE TABLE orders (id bigint PRIMARY KEY, payload text)",
    )
    .await;
    exec(&pool, "INSERT INTO orders VALUES (1, 'abc')").await;

    let ctx = ctx_for(&url, &db).await;
    let hub = MigrationHub::new(None);
    let policy = OpsPolicy::default();
    let job = new_job(&ctx, &hub, "orders", "payload", "varchar(50)", false).await;
    // 第一次运行：阻断但已记录 source_type + 指纹。
    pg_typechange::run_job(&job.job_id, &ctx, &hub, &policy)
        .await
        .expect("执行失败");
    assert!(
        hub.get_typechange(&job.job_id)
            .await
            .unwrap()
            .spec_hash
            .len()
            > 8
    );

    // 篡改目标类型后续跑 → 指纹不一致拒绝。
    hub.update_typechange(&job.job_id, |job| job.target_type = "varchar(100)".into())
        .await
        .unwrap();
    pg_typechange::run_job(&job.job_id, &ctx, &hub, &policy)
        .await
        .expect("执行失败");
    expect_blocked(&hub, &job.job_id, "preflight:spec_mismatch").await;
    assert_eq!(column_type(&pool, "orders", "payload").await, "text");

    cleanup_database(&admin, &db).await;
}

// ── 属主 ──────────────────────────────────────────────────

#[tokio::test]
async fn not_owner_block() {
    let Some(base_url) = env_url() else { return };
    let (db, admin) = isolated_database(&base_url, "it_tc_owner").await;
    let url = with_database(&base_url, &db);
    let pool = PgPool::connect(&url).await.expect("连接失败");
    exec(
        &pool,
        "CREATE TABLE orders (id bigint PRIMARY KEY, payload varchar(50))",
    )
    .await;
    exec(&pool, "INSERT INTO orders VALUES (1, 'abc')").await;
    // 非属主、非超级用户角色（带密码，走 TCP 登录）。表属主保持 postgres：
    // 连接用户既不是属主、也不是属主角色成员 → 属主校验必须阻断。
    let role = unique_db("tc_role");
    exec(
        &pool,
        &format!("CREATE ROLE \"{role}\" LOGIN PASSWORD 'tc_pass' NOSUPERUSER"),
    )
    .await;

    let user_url = url_with_role(&base_url, &db, &role, "tc_pass");
    let ctx = ctx_for(&user_url, &db).await;
    let hub = MigrationHub::new(None);
    let policy = OpsPolicy::default();
    let job = new_job(&ctx, &hub, "orders", "payload", "varchar(200)", false).await;
    pg_typechange::run_job(&job.job_id, &ctx, &hub, &policy)
        .await
        .expect("执行失败");
    expect_blocked(&hub, &job.job_id, "preflight:not_owner").await;
    assert_eq!(
        column_type(&pool, "orders", "payload").await,
        "character varying(50)"
    );

    exec(&pool, &format!("DROP ROLE \"{role}\"")).await;

    cleanup_database(&admin, &db).await;
}

// ── S5：freeform USING / 分区表 / FK 编排 ──────────────────────

/// 带 `using_expr` 的创建入口（S5 新增两个入参）。
#[allow(clippy::too_many_arguments)]
async fn new_job_ex(
    ctx: &OpsContext,
    hub: &MigrationHub,
    table: &str,
    column: &str,
    target: &str,
    allow_rewrite: bool,
    using_expr: Option<&str>,
    fk_counterparts: Vec<FkCounterpartSpec>,
) -> TypeChangeJob {
    pg_typechange::new_job(
        ctx,
        hub,
        pg_typechange::TypeChangeRequest {
            schema: None,
            table: table.into(),
            column: column.into(),
            target_type: target.into(),
            allow_rewrite,
            production_confirmed: false,
            using_expr: using_expr.map(str::to_string),
            fk_counterparts,
        },
    )
    .await
    .expect("创建改类型任务失败")
}

async fn fk_validated(pool: &PgPool, table: &str, constraint: &str) -> bool {
    sqlx::query_scalar(
        "SELECT c.convalidated::text FROM pg_constraint c \
         WHERE c.conrelid = ($1 || '.\"' || $2 || '\"')::regclass AND c.conname = $3",
    )
    .bind("public")
    .bind(table)
    .bind(constraint)
    .fetch_one(pool)
    .await
    .map(|v: String| v == "true")
    .unwrap_or(false)
}

/// 自定义 USING：空串经 NULLIF 归一后转 bigint。
/// 依据 specs/pg-field-governance-s5/spec.md §8（门 1 解析干跑 + 门 2 强制开闸）。
#[tokio::test]
async fn custom_using_expr() {
    let Some(base_url) = env_url() else { return };
    let (db, admin) = isolated_database(&base_url, "it_tc_using").await;
    let url = with_database(&base_url, &db);
    let pool = PgPool::connect(&url).await.expect("连接失败");
    exec(
        &pool,
        "CREATE TABLE orders (id bigint PRIMARY KEY, amount text)",
    )
    .await;
    exec(
        &pool,
        "INSERT INTO orders (id, amount) VALUES (1, '10'), (2, '20'), (3, ''), (4, NULL)",
    )
    .await;

    let ctx = ctx_for(&url, &db).await;
    let hub = MigrationHub::new(None);
    let policy = OpsPolicy::default();
    let job = new_job_ex(
        &ctx,
        &hub,
        "orders",
        "amount",
        "bigint",
        true,
        Some("NULLIF(amount, '')::bigint"),
        Vec::new(),
    )
    .await;
    pg_typechange::run_job(&job.job_id, &ctx, &hub, &policy)
        .await
        .expect("执行失败");

    let done = hub.get_typechange(&job.job_id).await.unwrap();
    assert_eq!(
        done.status,
        MigrationStatus::Completed,
        "notes={:?} err={:?}",
        done.notes,
        done.last_error
    );
    assert_eq!(done.classification, TypeChangeClass::Rewrite);
    assert_eq!(
        done.using_expr.as_deref(),
        Some("NULLIF(amount, '')::bigint")
    );
    assert_eq!(column_type(&pool, "orders", "amount").await, "bigint");
    let sum: i64 = sqlx::query_scalar("SELECT COALESCE(SUM(amount), 0)::bigint FROM orders")
        .fetch_one(&pool)
        .await
        .expect("求和失败");
    assert_eq!(sum, 30, "10 + 20，空串与 NULL 都转成 NULL");
    assert_eq!(row_count(&pool, "orders").await, 4);

    cleanup_database(&admin, &db).await;
}

/// USING 表达式解析失败：门 1 干跑必须拦在写之前。
/// 依据 specs/pg-field-governance-s5/spec.md §8。
#[tokio::test]
async fn bad_using_expr_blocks() {
    let Some(base_url) = env_url() else { return };
    let (db, admin) = isolated_database(&base_url, "it_tc_badusing").await;
    let url = with_database(&base_url, &db);
    let pool = PgPool::connect(&url).await.expect("连接失败");
    exec(
        &pool,
        "CREATE TABLE orders (id bigint PRIMARY KEY, amount text)",
    )
    .await;
    exec(&pool, "INSERT INTO orders VALUES (1, '10')").await;

    let ctx = ctx_for(&url, &db).await;
    let hub = MigrationHub::new(None);
    let policy = OpsPolicy::default();
    let job = new_job_ex(
        &ctx,
        &hub,
        "orders",
        "amount",
        "bigint",
        true,
        Some("amount::no_such_type"),
        Vec::new(),
    )
    .await;
    pg_typechange::run_job(&job.job_id, &ctx, &hub, &policy)
        .await
        .expect("执行失败");

    let blocked = expect_blocked(&hub, &job.job_id, "preflight:bad_using_expr").await;
    let message = blocked.last_error.unwrap_or_default();
    assert!(
        message.contains("USING"),
        "错误信息需指向 USING 表达式: {message}"
    );
    // 类型必须保持原样，且数据未被触碰。
    assert_eq!(column_type(&pool, "orders", "amount").await, "text");
    assert_eq!(row_count(&pool, "orders").await, 1);

    cleanup_database(&admin, &db).await;
}

/// timestamp -> timestamptz：无 USING 时非确定性拦截，显式 AT TIME ZONE 后放行。
/// 依据 specs/pg-field-governance-s5/spec.md §8。
#[tokio::test]
async fn timestamp_tz_explicit() {
    let Some(base_url) = env_url() else { return };
    let (db, admin) = isolated_database(&base_url, "it_tc_tz2").await;
    let url = with_database(&base_url, &db);
    let pool = PgPool::connect(&url).await.expect("连接失败");
    exec(
        &pool,
        "CREATE TABLE events (id bigint PRIMARY KEY, at timestamp without time zone)",
    )
    .await;
    exec(
        &pool,
        "INSERT INTO events VALUES (1, '2024-01-01 12:00:00')",
    )
    .await;

    let ctx = ctx_for(&url, &db).await;
    let hub = MigrationHub::new(None);
    let policy = OpsPolicy::default();

    // 第一次：不带 USING，开闸也挡在非确定性判定上。
    let blocked_job = new_job_ex(
        &ctx,
        &hub,
        "events",
        "at",
        "timestamptz",
        true,
        None,
        Vec::new(),
    )
    .await;
    pg_typechange::run_job(&blocked_job.job_id, &ctx, &hub, &policy)
        .await
        .expect("执行失败");
    expect_blocked(
        &hub,
        &blocked_job.job_id,
        "preflight:non_deterministic_cast",
    )
    .await;
    assert_eq!(
        column_type(&pool, "events", "at").await,
        "timestamp without time zone"
    );

    // 第二次：显式 AT TIME ZONE，三重门全过。
    let job = new_job_ex(
        &ctx,
        &hub,
        "events",
        "at",
        "timestamptz",
        true,
        Some("at AT TIME ZONE 'UTC'"),
        Vec::new(),
    )
    .await;
    pg_typechange::run_job(&job.job_id, &ctx, &hub, &policy)
        .await
        .expect("执行失败");
    let done = hub.get_typechange(&job.job_id).await.unwrap();
    assert_eq!(
        done.status,
        MigrationStatus::Completed,
        "notes={:?} err={:?}",
        done.notes,
        done.last_error
    );
    assert_eq!(
        column_type(&pool, "events", "at").await,
        "timestamp with time zone"
    );
    let hour: i32 = sqlx::query_scalar(
        "SELECT EXTRACT(HOUR FROM at AT TIME ZONE 'UTC')::int FROM events WHERE id = 1",
    )
    .fetch_one(&pool)
    .await
    .expect("取值失败");
    assert_eq!(hour, 12, "UTC 解释下小时不变");

    cleanup_database(&admin, &db).await;
}

/// FK 显式对端编排：DROP → ALTER 双端 → ADD NOT VALID → VALIDATE。
/// 依据 specs/pg-field-governance-s5/spec.md §9。
#[tokio::test]
async fn fk_orchestration() {
    let Some(base_url) = env_url() else { return };
    let (db, admin) = isolated_database(&base_url, "it_tc_fkok").await;
    let url = with_database(&base_url, &db);
    let pool = PgPool::connect(&url).await.expect("连接失败");
    exec(
        &pool,
        "CREATE TABLE parents (id integer PRIMARY KEY, note text)",
    )
    .await;
    exec(
        &pool,
        "CREATE TABLE children (id bigint PRIMARY KEY, \
         parent_id integer REFERENCES parents(id))",
    )
    .await;
    exec(
        &pool,
        "INSERT INTO parents (id, note) VALUES (1, 'a'), (2, 'b')",
    )
    .await;
    exec(
        &pool,
        "INSERT INTO children (id, parent_id) VALUES (10, 1), (11, 2)",
    )
    .await;

    let ctx = ctx_for(&url, &db).await;
    let hub = MigrationHub::new(None);
    let policy = OpsPolicy::default();
    let job = new_job_ex(
        &ctx,
        &hub,
        "parents",
        "id",
        "bigint",
        true,
        None,
        vec![FkCounterpartSpec {
            schema: Some("public".into()),
            table: "children".into(),
            column: "parent_id".into(),
        }],
    )
    .await;
    pg_typechange::run_job(&job.job_id, &ctx, &hub, &policy)
        .await
        .expect("执行失败");

    let done = hub.get_typechange(&job.job_id).await.unwrap();
    assert_eq!(
        done.status,
        MigrationStatus::Completed,
        "notes={:?} err={:?}",
        done.notes,
        done.last_error
    );
    assert_eq!(done.fk_plan.len(), 1, "必须编排一个外键");
    let entry = &done.fk_plan[0];
    assert_eq!(entry.constraint, "children_parent_id_fkey");
    assert!(entry.dropped && entry.altered && entry.recreated && entry.validated);
    // 双端同型、数据与约束都在。
    assert_eq!(column_type(&pool, "parents", "id").await, "bigint");
    assert_eq!(column_type(&pool, "children", "parent_id").await, "bigint");
    assert_eq!(row_count(&pool, "parents").await, 2);
    assert_eq!(row_count(&pool, "children").await, 2);
    assert!(
        fk_validated(&pool, "children", "children_parent_id_fkey").await,
        "外键必须以 VALIDATE 收尾（convalidated=true）"
    );

    cleanup_database(&admin, &db).await;
}

/// 分区父表改类型：单语句级联全部分区。
/// 依据 specs/pg-field-governance-s5/spec.md §7.4。
#[tokio::test]
async fn partitioned_table_typechange() {
    let Some(base_url) = env_url() else { return };
    let (db, admin) = isolated_database(&base_url, "it_tc_part").await;
    let url = with_database(&base_url, &db);
    let pool = PgPool::connect(&url).await.expect("连接失败");
    exec(
        &pool,
        "CREATE TABLE parted (id integer NOT NULL, note text, PRIMARY KEY (id)) \
         PARTITION BY RANGE (id)",
    )
    .await;
    exec(
        &pool,
        "CREATE TABLE parted_p1 PARTITION OF parted FOR VALUES FROM (0) TO (800)",
    )
    .await;
    exec(
        &pool,
        "CREATE TABLE parted_p2 PARTITION OF parted FOR VALUES FROM (800) TO (2000)",
    )
    .await;
    exec(
        &pool,
        "INSERT INTO parted SELECT g, 'n' || g FROM generate_series(1, 1500) g",
    )
    .await;

    let ctx = ctx_for(&url, &db).await;
    let hub = MigrationHub::new(None);
    let policy = OpsPolicy::default();

    // 非分区键列（note）走单语句级联：父表与两个分区同步改型。
    let job = new_job(&ctx, &hub, "parted", "note", "varchar(200)", true).await;
    pg_typechange::run_job(&job.job_id, &ctx, &hub, &policy)
        .await
        .expect("执行失败");
    let done = hub.get_typechange(&job.job_id).await.unwrap();
    assert_eq!(
        done.status,
        MigrationStatus::Completed,
        "notes={:?} err={:?}",
        done.notes,
        done.last_error
    );
    assert_eq!(
        column_type(&pool, "parted", "note").await,
        "character varying(200)"
    );
    assert_eq!(
        column_type(&pool, "parted_p1", "note").await,
        "character varying(200)"
    );
    assert_eq!(
        column_type(&pool, "parted_p2", "note").await,
        "character varying(200)"
    );
    assert_eq!(row_count(&pool, "parted").await, 1_500);
    assert!(done.verify.indexes_valid, "分区索引必须全部有效");

    // 分区键列（id）PG 硬禁止改类型：必须挡在 preflight，不进入 Applying。
    // 依据：实测 `cannot alter column "id" because it is part of the partition key`。
    let blocked = new_job(&ctx, &hub, "parted", "id", "bigint", true).await;
    pg_typechange::run_job(&blocked.job_id, &ctx, &hub, &policy)
        .await
        .expect("执行失败");
    expect_blocked(&hub, &blocked.job_id, "preflight:partition_key").await;
    assert_eq!(column_type(&pool, "parted", "id").await, "integer");
    assert_eq!(row_count(&pool, "parted").await, 1_500);
    assert!(pkey_valid(&pool, "parted").await);

    cleanup_database(&admin, &db).await;
}

/// 对端声明与实际 FK 不匹配：有声明也不能放行，必须逐个 FK 覆盖。
/// 依据 specs/pg-field-governance-s5/spec.md §9（显式对端编排的前置条件）。
#[tokio::test]
async fn fk_counterparts_mismatch_blocks() {
    let Some(base_url) = env_url() else { return };
    let (db, admin) = isolated_database(&base_url, "it_tc_fkmm").await;
    let url = with_database(&base_url, &db);
    let pool = PgPool::connect(&url).await.expect("连接失败");
    exec(
        &pool,
        "CREATE TABLE parents (id integer PRIMARY KEY, note text)",
    )
    .await;
    exec(
        &pool,
        "CREATE TABLE children (id bigint PRIMARY KEY, \
         parent_id integer REFERENCES parents(id))",
    )
    .await;

    let ctx = ctx_for(&url, &db).await;
    let hub = MigrationHub::new(None);
    let policy = OpsPolicy::default();
    // 声明的是 children.id（错的列），真实 FK 在 children.parent_id。
    let job = new_job_ex(
        &ctx,
        &hub,
        "parents",
        "id",
        "bigint",
        true,
        None,
        vec![FkCounterpartSpec {
            schema: Some("public".into()),
            table: "children".into(),
            column: "id".into(),
        }],
    )
    .await;
    pg_typechange::run_job(&job.job_id, &ctx, &hub, &policy)
        .await
        .expect("执行失败");

    let blocked = expect_blocked(&hub, &job.job_id, "preflight:fk_counterparts_missing").await;
    let message = blocked.last_error.unwrap_or_default();
    assert!(
        message.contains("children.parent_id"),
        "错误信息需指出真实对端列: {message}"
    );
    // 双端类型都不能变。
    assert_eq!(column_type(&pool, "parents", "id").await, "integer");
    assert_eq!(column_type(&pool, "children", "parent_id").await, "integer");

    cleanup_database(&admin, &db).await;
}
