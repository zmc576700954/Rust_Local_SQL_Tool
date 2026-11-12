//! S3 `CREATE INDEX CONCURRENTLY` 执行器的 PostgreSQL 真库验证（opt-in）。
//!
//! ```bash
//! FIELD_MIGRATION_PG_URL='postgres://postgres:password@127.0.0.1:55432/postgres' \
//!   cargo test -p core_lib --test field_migration_index_pg --no-fail-fast -- --test-threads=1
//! ```
//! 未设置环境变量时打印原因并返回，不 `#[ignore]`。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use sqlx::postgres::PgPool;
use sqlx::Executor;

use core_lib::db::client::DbClient;
use core_lib::ops::field_migration::migration_store::{IndexPhase, MigrationHub, MigrationStatus};
use core_lib::ops::field_migration::pg_index;
use core_lib::ops::policy::OpsPolicy;
use core_lib::ops::registry::OpsContext;

const DB_ID: &str = "field-index-pg-e2e";

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

async fn exec_raw(pool: &PgPool, sql: &str) -> Result<(), sqlx::Error> {
    pool.execute(sql).await.map(|_| ())
}

async fn ctx_for(url: &str, db_name: &str) -> OpsContext {
    let policy = Arc::new(OpsPolicy::default());
    let client = DbClient::new_default(url)
        .await
        .expect("创建 DbClient 失败");
    OpsContext::new(client, DB_ID.into(), db_name.to_string(), false).with_policy(policy)
}

async fn index_state(pool: &PgPool, name: &str) -> Option<(bool, bool)> {
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT i.indisvalid::text, i.indisready::text FROM pg_index i \
         JOIN pg_class c ON c.oid = i.indexrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = 'public' AND c.relname = $1",
    )
    .bind(name)
    .fetch_optional(pool)
    .await
    .expect("查询 pg_index 失败");
    row.map(|(valid, ready)| (valid == "true", ready == "true"))
}

fn env_url() -> Option<String> {
    match std::env::var("FIELD_MIGRATION_PG_URL") {
        Ok(url) => Some(url),
        Err(_) => {
            eprintln!("跳过 CONCURRENTLY 索引真库验证：未设置 FIELD_MIGRATION_PG_URL");
            None
        }
    }
}

async fn new_index_job(
    ctx: &OpsContext,
    hub: &MigrationHub,
    table: &str,
    name: &str,
    columns: &[&str],
    unique: bool,
) -> core_lib::ops::field_migration::migration_store::IndexJob {
    pg_index::new_job(
        ctx,
        hub,
        pg_index::IndexRequest {
            schema: None,
            table: table.into(),
            index_name: name.into(),
            columns: columns.iter().map(|value| value.to_string()).collect(),
            unique,
            production_confirmed: false,
        },
    )
    .await
    .expect("创建索引任务失败")
}

// ── 用例 ───────────────────────────────────────────────────

#[tokio::test]
async fn create_plain_index() {
    let Some(base_url) = env_url() else { return };
    let (db, admin) = isolated_database(&base_url, "it_ix_plain").await;
    let url = with_database(&base_url, &db);
    let pool = PgPool::connect(&url).await.expect("连接失败");
    exec(
        &pool,
        "CREATE TABLE orders (id bigint PRIMARY KEY, payload text)",
    )
    .await;
    exec(
        &pool,
        "INSERT INTO orders SELECT g, 'v' || g FROM generate_series(1, 2000) g",
    )
    .await;

    let ctx = ctx_for(&url, &db).await;
    let hub = MigrationHub::new(None);
    let policy = OpsPolicy::default();
    let job = new_index_job(
        &ctx,
        &hub,
        "orders",
        "idx_orders_payload",
        &["payload"],
        false,
    )
    .await;
    pg_index::run_job(&job.job_id, &ctx, &hub, &policy)
        .await
        .expect("执行失败");

    let done = hub.get_index(&job.job_id).await.unwrap();
    assert_eq!(
        done.status,
        MigrationStatus::Completed,
        "notes={:?} err={:?} blocking={:?}",
        done.notes,
        done.last_error,
        done.blocking
    );
    assert_eq!(done.phase, IndexPhase::Done);
    assert_eq!(
        index_state(&pool, "idx_orders_payload").await,
        Some((true, true))
    );
    assert!(done.notes.iter().any(|note| note.starts_with("verify: ok")));

    cleanup_database(&admin, &db).await;
}

#[tokio::test]
async fn create_unique_index() {
    let Some(base_url) = env_url() else { return };
    let (db, admin) = isolated_database(&base_url, "it_ix_unique").await;
    let url = with_database(&base_url, &db);
    let pool = PgPool::connect(&url).await.expect("连接失败");
    exec(
        &pool,
        "CREATE TABLE orders (id bigint PRIMARY KEY, code text)",
    )
    .await;
    exec(
        &pool,
        "INSERT INTO orders SELECT g, 'c' || g FROM generate_series(1, 500) g",
    )
    .await;

    let ctx = ctx_for(&url, &db).await;
    let hub = MigrationHub::new(None);
    let policy = OpsPolicy::default();
    let job = new_index_job(&ctx, &hub, "orders", "idx_orders_code", &["code"], true).await;
    pg_index::run_job(&job.job_id, &ctx, &hub, &policy)
        .await
        .expect("执行失败");

    let done = hub.get_index(&job.job_id).await.unwrap();
    assert_eq!(
        done.status,
        MigrationStatus::Completed,
        "{:?}",
        done.last_error
    );
    assert_eq!(
        index_state(&pool, "idx_orders_code").await,
        Some((true, true))
    );

    cleanup_database(&admin, &db).await;
}

#[tokio::test]
async fn unique_violation_blocks_and_cleans_up() {
    let Some(base_url) = env_url() else { return };
    let (db, admin) = isolated_database(&base_url, "it_ix_dupe").await;
    let url = with_database(&base_url, &db);
    let pool = PgPool::connect(&url).await.expect("连接失败");
    exec(
        &pool,
        "CREATE TABLE orders (id bigint PRIMARY KEY, code text)",
    )
    .await;
    exec(
        &pool,
        "INSERT INTO orders SELECT g, CASE WHEN g <= 10 THEN 'dup' ELSE 'c' || g END \
         FROM generate_series(1, 100) g",
    )
    .await;

    let ctx = ctx_for(&url, &db).await;
    let hub = MigrationHub::new(None);
    let policy = OpsPolicy::default();
    let job = new_index_job(&ctx, &hub, "orders", "idx_orders_code", &["code"], true).await;
    pg_index::run_job(&job.job_id, &ctx, &hub, &policy)
        .await
        .expect("执行失败");

    let blocked = hub.get_index(&job.job_id).await.unwrap();
    assert_eq!(
        blocked.status,
        MigrationStatus::Blocked,
        "{:?}",
        blocked.last_error
    );
    assert_eq!(
        blocked.blocking.as_ref().unwrap().phase,
        "build:unique_violation"
    );
    assert_eq!(
        index_state(&pool, "idx_orders_code").await,
        None,
        "残留索引未清理"
    );

    cleanup_database(&admin, &db).await;
}

#[tokio::test]
async fn invalid_leftover_dropped_and_rebuilt() {
    let Some(base_url) = env_url() else { return };
    let (db, admin) = isolated_database(&base_url, "it_ix_invalid").await;
    let url = with_database(&base_url, &db);
    let pool = PgPool::connect(&url).await.expect("连接失败");
    exec(
        &pool,
        "CREATE TABLE orders (id bigint PRIMARY KEY, code text)",
    )
    .await;
    exec(
        &pool,
        "INSERT INTO orders SELECT g, CASE WHEN g <= 5 THEN 'dup' ELSE 'c' || g END \
         FROM generate_series(1, 50) g",
    )
    .await;
    // 并发建唯一索引遇重复值失败，会留下同名 INVALID 索引。
    let _ = exec_raw(
        &pool,
        "CREATE UNIQUE INDEX CONCURRENTLY idx_orders_code ON orders (code)",
    )
    .await;
    let leftover = index_state(&pool, "idx_orders_code").await;
    assert!(
        matches!(leftover, Some((false, _))),
        "预期留下 INVALID 索引，实际 {leftover:?}"
    );

    let ctx = ctx_for(&url, &db).await;
    let hub = MigrationHub::new(None);
    let policy = OpsPolicy::default();
    let job = new_index_job(&ctx, &hub, "orders", "idx_orders_code", &["code"], false).await;
    pg_index::run_job(&job.job_id, &ctx, &hub, &policy)
        .await
        .expect("执行失败");

    let done = hub.get_index(&job.job_id).await.unwrap();
    assert_eq!(
        done.status,
        MigrationStatus::Completed,
        "notes={:?} err={:?}",
        done.notes,
        done.last_error
    );
    assert!(done.notes.iter().any(|note| note.contains("INVALID")));
    assert_eq!(
        index_state(&pool, "idx_orders_code").await,
        Some((true, true))
    );

    cleanup_database(&admin, &db).await;
}

#[tokio::test]
async fn existing_valid_index_is_noop() {
    let Some(base_url) = env_url() else { return };
    let (db, admin) = isolated_database(&base_url, "it_ix_noop").await;
    let url = with_database(&base_url, &db);
    let pool = PgPool::connect(&url).await.expect("连接失败");
    exec(
        &pool,
        "CREATE TABLE orders (id bigint PRIMARY KEY, code text)",
    )
    .await;
    exec(&pool, "CREATE INDEX idx_orders_code ON orders (code)").await;

    let ctx = ctx_for(&url, &db).await;
    let hub = MigrationHub::new(None);
    let policy = OpsPolicy::default();
    let job = new_index_job(&ctx, &hub, "orders", "idx_orders_code", &["code"], false).await;
    pg_index::run_job(&job.job_id, &ctx, &hub, &policy)
        .await
        .expect("执行失败");

    let done = hub.get_index(&job.job_id).await.unwrap();
    assert_eq!(done.status, MigrationStatus::Completed);
    assert!(done.notes.iter().any(|note| note.contains("no-op")));
    assert_eq!(
        index_state(&pool, "idx_orders_code").await,
        Some((true, true))
    );

    cleanup_database(&admin, &db).await;
}

#[tokio::test]
async fn missing_table_or_column_blocks() {
    let Some(base_url) = env_url() else { return };
    let (db, admin) = isolated_database(&base_url, "it_ix_missing").await;
    let url = with_database(&base_url, &db);
    let pool = PgPool::connect(&url).await.expect("连接失败");
    exec(
        &pool,
        "CREATE TABLE orders (id bigint PRIMARY KEY, code text)",
    )
    .await;

    let ctx = ctx_for(&url, &db).await;
    let hub = MigrationHub::new(None);
    let policy = OpsPolicy::default();

    let no_table = new_index_job(&ctx, &hub, "absent", "idx_absent", &["code"], false).await;
    pg_index::run_job(&no_table.job_id, &ctx, &hub, &policy)
        .await
        .expect("执行失败");
    let blocked = hub.get_index(&no_table.job_id).await.unwrap();
    assert_eq!(blocked.status, MigrationStatus::Blocked);
    assert_eq!(blocked.blocking.as_ref().unwrap().phase, "preflight");

    let no_column = new_index_job(&ctx, &hub, "orders", "idx_orders_x", &["nope"], false).await;
    pg_index::run_job(&no_column.job_id, &ctx, &hub, &policy)
        .await
        .expect("执行失败");
    let blocked = hub.get_index(&no_column.job_id).await.unwrap();
    assert_eq!(blocked.status, MigrationStatus::Blocked);
    assert!(blocked.last_error.as_deref().unwrap_or("").contains("nope"));
    assert_eq!(index_state(&pool, "idx_orders_x").await, None);

    cleanup_database(&admin, &db).await;
}

#[tokio::test]
async fn name_too_long_blocks() {
    let Some(base_url) = env_url() else { return };
    let (db, admin) = isolated_database(&base_url, "it_ix_long").await;
    let url = with_database(&base_url, &db);
    let pool = PgPool::connect(&url).await.expect("连接失败");
    exec(
        &pool,
        "CREATE TABLE orders (id bigint PRIMARY KEY, code text)",
    )
    .await;

    let ctx = ctx_for(&url, &db).await;
    let hub = MigrationHub::new(None);
    let policy = OpsPolicy::default();
    let long_name = "a".repeat(64);
    let job = new_index_job(&ctx, &hub, "orders", &long_name, &["code"], false).await;
    pg_index::run_job(&job.job_id, &ctx, &hub, &policy)
        .await
        .expect("执行失败");

    let blocked = hub.get_index(&job.job_id).await.unwrap();
    assert_eq!(blocked.status, MigrationStatus::Blocked);
    assert_eq!(
        blocked.blocking.as_ref().unwrap().phase,
        "preflight:name_too_long"
    );
    // 不静默截断：库里不应出现以该长名前缀命名的索引（orders_pkey 不算）。
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = 'public' AND c.relkind = 'i' AND c.relname LIKE 'aaaa%'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 0);

    cleanup_database(&admin, &db).await;
}

#[tokio::test]
async fn partitioned_index_recipe() {
    // S5 行为变更依据：分区表从 BLOCK 改为官方配方执行
    // （ON ONLY 父表占位 → 逐分区 CONCURRENTLY → ATTACH），
    // PG 不支持对分区父表直接 CREATE INDEX CONCURRENTLY。
    let Some(base_url) = env_url() else { return };
    let (db, admin) = isolated_database(&base_url, "it_ix_part").await;
    let url = with_database(&base_url, &db);
    let pool = PgPool::connect(&url).await.expect("连接失败");
    exec(
        &pool,
        "CREATE TABLE parted (id bigint NOT NULL, code text, PRIMARY KEY (id)) \
         PARTITION BY RANGE (id)",
    )
    .await;
    exec(
        &pool,
        "CREATE TABLE parted_p1 PARTITION OF parted FOR VALUES FROM (1) TO (1000)",
    )
    .await;
    exec(
        &pool,
        "CREATE TABLE parted_p2 PARTITION OF parted FOR VALUES FROM (1000) TO (2000)",
    )
    .await;
    exec(
        &pool,
        "INSERT INTO parted SELECT g, 'v' || g FROM generate_series(1, 1500) g",
    )
    .await;

    let ctx = ctx_for(&url, &db).await;
    let hub = MigrationHub::new(None);
    let policy = OpsPolicy::default();
    let job = new_index_job(&ctx, &hub, "parted", "idx_parted_code", &["code"], false).await;
    pg_index::run_job(&job.job_id, &ctx, &hub, &policy)
        .await
        .expect("执行失败");

    let done = hub.get_index(&job.job_id).await.expect("任务不可见");
    println!(
        "分区索引: status={:?} last_error={:?}",
        done.status, done.last_error
    );
    assert_eq!(done.status, MigrationStatus::Completed);
    assert!(
        index_state(&pool, "idx_parted_code").await.is_some(),
        "父索引必须存在"
    );

    // 全部分区索引有效且已 ATTACH 到父索引。
    let attached: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pg_inherits i \
         JOIN pg_class parent ON parent.oid = i.inhparent \
         JOIN pg_class child ON child.oid = i.inhrelid \
         JOIN pg_index ix ON ix.indexrelid = child.oid \
         WHERE parent.relname = 'idx_parted_code' \
           AND child.relname LIKE 'idx_parted_code_p_%' AND ix.indisvalid",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(attached, 2, "两个分区索引都应有效并已 attach");

    // 重跑幂等：不重复建、不报错。
    pg_index::run_job(&job.job_id, &ctx, &hub, &policy)
        .await
        .expect("重跑失败");
    let rerun = hub.get_index(&job.job_id).await.unwrap();
    assert_eq!(rerun.status, MigrationStatus::Completed);

    cleanup_database(&admin, &db).await;
}

#[tokio::test]
async fn progress_polling_does_not_block_completion() {
    let Some(base_url) = env_url() else { return };
    let (db, admin) = isolated_database(&base_url, "it_ix_progress").await;
    let url = with_database(&base_url, &db);
    let pool = PgPool::connect(&url).await.expect("连接失败");
    exec(
        &pool,
        "CREATE TABLE orders (id bigint PRIMARY KEY, payload text)",
    )
    .await;
    exec(
        &pool,
        "INSERT INTO orders SELECT g, 'v' || g FROM generate_series(1, 20000) g",
    )
    .await;

    let ctx = ctx_for(&url, &db).await;
    let hub = MigrationHub::new(None);
    let policy = OpsPolicy::default();
    let job = new_index_job(
        &ctx,
        &hub,
        "orders",
        "idx_orders_payload",
        &["payload"],
        false,
    )
    .await;

    // 进度轮询是 best-effort：无论视图是否可读，主流程必须完成。
    let run = tokio::spawn({
        let ctx = ctx.clone();
        let hub = hub.clone();
        let policy = policy.clone();
        let job_id = job.job_id.clone();
        async move { pg_index::run_job(&job_id, &ctx, &hub, &policy).await }
    });
    run.await.expect("任务 panic").expect("执行失败");

    let done = hub.get_index(&job.job_id).await.unwrap();
    assert_eq!(
        done.status,
        MigrationStatus::Completed,
        "{:?}",
        done.last_error
    );
    assert_eq!(
        index_state(&pool, "idx_orders_payload").await,
        Some((true, true))
    );

    cleanup_database(&admin, &db).await;
}

#[tokio::test]
async fn spec_mismatch_refuses_resume() {
    let Some(base_url) = env_url() else { return };
    let (db, admin) = isolated_database(&base_url, "it_ix_spec").await;
    let url = with_database(&base_url, &db);
    let pool = PgPool::connect(&url).await.expect("连接失败");
    exec(
        &pool,
        "CREATE TABLE orders (id bigint PRIMARY KEY, code text, other text)",
    )
    .await;

    let ctx = ctx_for(&url, &db).await;
    let hub = MigrationHub::new(None);
    let policy = OpsPolicy::default();
    let job = new_index_job(&ctx, &hub, "orders", "idx_orders_code", &["code"], false).await;
    hub.update_index(&job.job_id, |job| job.spec.columns = vec!["other".into()])
        .await
        .unwrap();
    pg_index::run_job(&job.job_id, &ctx, &hub, &policy)
        .await
        .expect("执行失败");

    let blocked = hub.get_index(&job.job_id).await.unwrap();
    assert_eq!(blocked.status, MigrationStatus::Blocked);
    assert_eq!(
        blocked.blocking.as_ref().unwrap().phase,
        "preflight:spec_mismatch"
    );
    assert_eq!(index_state(&pool, "idx_orders_code").await, None);

    cleanup_database(&admin, &db).await;
}
