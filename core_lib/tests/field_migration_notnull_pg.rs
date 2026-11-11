//! S3 `SET NOT NULL` 执行器的 PostgreSQL 真库验证（opt-in）。
//!
//! ```bash
//! FIELD_MIGRATION_PG_URL='postgres://postgres:password@127.0.0.1:55432/postgres' \
//!   cargo test -p core_lib --test field_migration_notnull_pg --no-fail-fast -- --test-threads=1
//! ```
//! 未设置环境变量时打印原因并返回，不 `#[ignore]`。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use sqlx::postgres::PgPool;
use sqlx::Executor;

use core_lib::db::client::DbClient;
use core_lib::ops::field_migration::migration_store::{
    BackfillConfig, MigrationHub, MigrationStatus, NotNullPhase,
};
use core_lib::ops::field_migration::pg_notnull;
use core_lib::ops::policy::OpsPolicy;
use core_lib::ops::registry::OpsContext;

const DB_ID: &str = "field-notnull-pg-e2e";

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

async fn nullable(pool: &PgPool, table: &str, column: &str) -> bool {
    let value: Option<String> = sqlx::query_scalar(
        "SELECT is_nullable FROM information_schema.columns \
         WHERE table_schema = 'public' AND table_name = $1 AND column_name = $2",
    )
    .bind(table)
    .bind(column)
    .fetch_optional(pool)
    .await
    .expect("查询 information_schema 失败");
    value.as_deref() == Some("YES")
}

async fn constraint_exists(pool: &PgPool, table: &str, name: &str) -> bool {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pg_constraint con \
         JOIN pg_class c ON c.oid = con.conrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = 'public' AND c.relname = $1 AND con.conname = $2",
    )
    .bind(table)
    .bind(name)
    .fetch_one(pool)
    .await
    .expect("查询 pg_constraint 失败");
    count > 0
}

fn env_url() -> Option<String> {
    match std::env::var("FIELD_MIGRATION_PG_URL") {
        Ok(url) => Some(url),
        Err(_) => {
            eprintln!("跳过 SET NOT NULL 真库验证：未设置 FIELD_MIGRATION_PG_URL");
            None
        }
    }
}

// ── 用例 ───────────────────────────────────────────────────

#[tokio::test]
async fn notnull_happy_path() {
    let Some(base_url) = env_url() else { return };
    let (db, admin) = isolated_database(&base_url, "it_nn_ok").await;
    let url = with_database(&base_url, &db);
    let pool = PgPool::connect(&url).await.expect("连接失败");
    exec(
        &pool,
        "CREATE TABLE orders (id bigint PRIMARY KEY, payload text)",
    )
    .await;
    exec(
        &pool,
        "INSERT INTO orders SELECT g, 'v' || g FROM generate_series(1, 300) g",
    )
    .await;

    let ctx = ctx_for(&url, &db).await;
    let hub = MigrationHub::new(None);
    let policy = OpsPolicy::default();
    let job = pg_notnull::new_job(
        &ctx,
        &hub,
        pg_notnull::NotNullRequest {
            schema: None,
            table: "orders".into(),
            column: "payload".into(),
            production_confirmed: false,
        },
    )
    .await
    .expect("创建任务失败");
    pg_notnull::run_job(&job.job_id, &ctx, &hub, &policy)
        .await
        .expect("执行失败");

    let done = hub.get_notnull(&job.job_id).await.unwrap();
    assert_eq!(
        done.status,
        MigrationStatus::Completed,
        "notes={:?} err={:?} blocking={:?}",
        done.notes,
        done.last_error,
        done.blocking
    );
    assert_eq!(done.phase, NotNullPhase::Done);
    assert!(!nullable(&pool, "orders", "payload").await);
    assert!(!constraint_exists(&pool, "orders", "fm_not_null_payload").await);
    assert!(done.notes.iter().any(|note| note == "verify: ok"));

    cleanup_database(&admin, &db).await;
}

#[tokio::test]
async fn null_rows_block_then_fix_and_rerun() {
    let Some(base_url) = env_url() else { return };
    let (db, admin) = isolated_database(&base_url, "it_nn_null").await;
    let url = with_database(&base_url, &db);
    let pool = PgPool::connect(&url).await.expect("连接失败");
    exec(
        &pool,
        "CREATE TABLE orders (id bigint PRIMARY KEY, payload text)",
    )
    .await;
    exec(
        &pool,
        "INSERT INTO orders SELECT g, CASE WHEN g IN (7, 19, 31) THEN NULL ELSE 'v' END \
         FROM generate_series(1, 40) g",
    )
    .await;

    let ctx = ctx_for(&url, &db).await;
    let hub = MigrationHub::new(None);
    let policy = OpsPolicy::default();
    let job = pg_notnull::new_job(
        &ctx,
        &hub,
        pg_notnull::NotNullRequest {
            schema: None,
            table: "orders".into(),
            column: "payload".into(),
            production_confirmed: false,
        },
    )
    .await
    .unwrap();
    pg_notnull::run_job(&job.job_id, &ctx, &hub, &policy)
        .await
        .expect("执行失败");

    let blocked = hub.get_notnull(&job.job_id).await.unwrap();
    assert_eq!(
        blocked.status,
        MigrationStatus::Blocked,
        "err={:?}",
        blocked.last_error
    );
    let blocking = blocked.blocking.as_ref().expect("缺少 blocking");
    assert_eq!(blocking.phase, "scan:null_rows");
    assert_eq!(blocking.total_diffs, 3);
    assert_eq!(blocking.pks.len(), 3);
    assert_eq!(blocked.phase, NotNullPhase::Scan);
    assert!(blocked.scan.last_pk.is_none(), "阻断后必须从头重扫");
    assert!(nullable(&pool, "orders", "payload").await);

    exec(
        &pool,
        "UPDATE orders SET payload = 'fixed' WHERE payload IS NULL",
    )
    .await;
    pg_notnull::run_job(&job.job_id, &ctx, &hub, &policy)
        .await
        .expect("修复后执行失败");
    let done = hub.get_notnull(&job.job_id).await.unwrap();
    assert_eq!(
        done.status,
        MigrationStatus::Completed,
        "notes={:?} err={:?} blocking={:?}",
        done.notes,
        done.last_error,
        done.blocking
    );
    assert!(!nullable(&pool, "orders", "payload").await);

    cleanup_database(&admin, &db).await;
}

#[tokio::test]
async fn scan_resume_after_terminate() {
    let Some(base_url) = env_url() else { return };
    let (db, admin) = isolated_database(&base_url, "it_nn_resume").await;
    let url = with_database(&base_url, &db);
    let pool = PgPool::connect(&url).await.expect("连接失败");
    exec(
        &pool,
        "CREATE TABLE big_orders (id bigint PRIMARY KEY, payload text)",
    )
    .await;
    exec(
        &pool,
        "INSERT INTO big_orders SELECT g, 'v' FROM generate_series(1, 30000) g",
    )
    .await;

    let ctx = ctx_for(&url, &db).await;
    let hub = MigrationHub::new(None);
    let policy = OpsPolicy::default();
    let job = pg_notnull::new_job(
        &ctx,
        &hub,
        pg_notnull::NotNullRequest {
            schema: None,
            table: "big_orders".into(),
            column: "payload".into(),
            production_confirmed: false,
        },
    )
    .await
    .unwrap();
    hub.update_notnull(&job.job_id, |job| {
        job.config = BackfillConfig {
            batch_size: 100,
            batch_interval_ms: 0,
            max_rows_per_second: 3000,
            single_transaction: false,
        };
    })
    .await
    .unwrap();

    let run = tokio::spawn({
        let ctx = ctx.clone();
        let hub = hub.clone();
        let policy = policy.clone();
        let job_id = job.job_id.clone();
        async move { pg_notnull::run_job(&job_id, &ctx, &hub, &policy).await }
    });

    // 确定性地在扫描中途注入中断：改列名让下一批查询失败。
    // （pg_terminate_backend 打到的是空闲连接，sqlx 会自动换新连接，无法稳定中断。）
    let mut progressed = 0u64;
    for _ in 0..200 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        progressed = hub
            .get_notnull(&job.job_id)
            .await
            .unwrap()
            .scan
            .scanned_rows;
        if progressed >= 500 {
            break;
        }
    }
    assert!(
        progressed >= 500 && progressed < 30000,
        "扫描未在预期区间推进: {progressed}"
    );
    exec(
        &pool,
        "ALTER TABLE big_orders RENAME COLUMN payload TO payload_tmp",
    )
    .await;
    let _ = run.await;

    let interrupted = hub.get_notnull(&job.job_id).await.unwrap();
    assert_ne!(
        interrupted.status,
        MigrationStatus::Completed,
        "中断后不应完成"
    );
    let scanned_before = interrupted.scan.scanned_rows;
    assert!(scanned_before > 0 && scanned_before < 30000);
    hub.update_notnull(&job.job_id, |job| {
        job.status = MigrationStatus::Interrupted;
        job.last_error = Some("测试注入中断".into());
    })
    .await
    .unwrap();
    exec(
        &pool,
        "ALTER TABLE big_orders RENAME COLUMN payload_tmp TO payload",
    )
    .await;

    pg_notnull::run_job(&job.job_id, &ctx, &hub, &policy)
        .await
        .expect("续跑失败");
    let done = hub.get_notnull(&job.job_id).await.unwrap();
    assert_eq!(
        done.status,
        MigrationStatus::Completed,
        "notes={:?} err={:?} blocking={:?}",
        done.notes,
        done.last_error,
        done.blocking
    );
    // 续跑从 checkpoint 继续，累计扫描行数等于表总行数，不重复。
    assert_eq!(done.scan.scanned_rows, 30000);
    assert!(!nullable(&pool, "big_orders", "payload").await);

    cleanup_database(&admin, &db).await;
}

#[tokio::test]
async fn residual_not_valid_constraint_resumes_at_validate() {
    let Some(base_url) = env_url() else { return };
    let (db, admin) = isolated_database(&base_url, "it_nn_residual").await;
    let url = with_database(&base_url, &db);
    let pool = PgPool::connect(&url).await.expect("连接失败");
    exec(
        &pool,
        "CREATE TABLE orders (id bigint PRIMARY KEY, payload text)",
    )
    .await;
    exec(
        &pool,
        "INSERT INTO orders SELECT g, 'v' FROM generate_series(1, 50) g",
    )
    .await;
    exec(
        &pool,
        "ALTER TABLE orders ADD CONSTRAINT fm_not_null_payload \
         CHECK (payload IS NOT NULL) NOT VALID",
    )
    .await;

    let ctx = ctx_for(&url, &db).await;
    let hub = MigrationHub::new(None);
    let policy = OpsPolicy::default();
    let job = pg_notnull::new_job(
        &ctx,
        &hub,
        pg_notnull::NotNullRequest {
            schema: None,
            table: "orders".into(),
            column: "payload".into(),
            production_confirmed: false,
        },
    )
    .await
    .unwrap();
    pg_notnull::run_job(&job.job_id, &ctx, &hub, &policy)
        .await
        .expect("执行失败");

    let done = hub.get_notnull(&job.job_id).await.unwrap();
    assert_eq!(
        done.status,
        MigrationStatus::Completed,
        "notes={:?} err={:?} blocking={:?}",
        done.notes,
        done.last_error,
        done.blocking
    );
    assert!(
        done.scan.scanned_rows == 0,
        "已有 NOT VALID 约束时应跳过扫描"
    );
    assert!(!nullable(&pool, "orders", "payload").await);
    assert!(!constraint_exists(&pool, "orders", "fm_not_null_payload").await);

    cleanup_database(&admin, &db).await;
}

#[tokio::test]
async fn residual_validated_constraint_resumes_at_set_not_null() {
    let Some(base_url) = env_url() else { return };
    let (db, admin) = isolated_database(&base_url, "it_nn_validated").await;
    let url = with_database(&base_url, &db);
    let pool = PgPool::connect(&url).await.expect("连接失败");
    exec(
        &pool,
        "CREATE TABLE orders (id bigint PRIMARY KEY, payload text)",
    )
    .await;
    exec(
        &pool,
        "INSERT INTO orders SELECT g, 'v' FROM generate_series(1, 20) g",
    )
    .await;
    exec(
        &pool,
        "ALTER TABLE orders ADD CONSTRAINT fm_not_null_payload \
         CHECK (payload IS NOT NULL) NOT VALID",
    )
    .await;
    exec(
        &pool,
        "ALTER TABLE orders VALIDATE CONSTRAINT fm_not_null_payload",
    )
    .await;

    let ctx = ctx_for(&url, &db).await;
    let hub = MigrationHub::new(None);
    let policy = OpsPolicy::default();
    let job = pg_notnull::new_job(
        &ctx,
        &hub,
        pg_notnull::NotNullRequest {
            schema: None,
            table: "orders".into(),
            column: "payload".into(),
            production_confirmed: false,
        },
    )
    .await
    .unwrap();
    pg_notnull::run_job(&job.job_id, &ctx, &hub, &policy)
        .await
        .expect("执行失败");

    let done = hub.get_notnull(&job.job_id).await.unwrap();
    assert_eq!(
        done.status,
        MigrationStatus::Completed,
        "notes={:?} err={:?} blocking={:?}",
        done.notes,
        done.last_error,
        done.blocking
    );
    assert!(!done.notes.iter().any(|note| note == "validate: ok"));
    assert!(!nullable(&pool, "orders", "payload").await);
    assert!(!constraint_exists(&pool, "orders", "fm_not_null_payload").await);

    cleanup_database(&admin, &db).await;
}

#[tokio::test]
async fn already_not_null_is_noop() {
    let Some(base_url) = env_url() else { return };
    let (db, admin) = isolated_database(&base_url, "it_nn_noop").await;
    let url = with_database(&base_url, &db);
    let pool = PgPool::connect(&url).await.expect("连接失败");
    exec(
        &pool,
        "CREATE TABLE orders (id bigint PRIMARY KEY, payload text NOT NULL)",
    )
    .await;

    let ctx = ctx_for(&url, &db).await;
    let hub = MigrationHub::new(None);
    let policy = OpsPolicy::default();
    let job = pg_notnull::new_job(
        &ctx,
        &hub,
        pg_notnull::NotNullRequest {
            schema: None,
            table: "orders".into(),
            column: "payload".into(),
            production_confirmed: false,
        },
    )
    .await
    .unwrap();
    pg_notnull::run_job(&job.job_id, &ctx, &hub, &policy)
        .await
        .expect("执行失败");

    let done = hub.get_notnull(&job.job_id).await.unwrap();
    assert_eq!(done.status, MigrationStatus::Completed);
    assert!(done.notes.iter().any(|note| note.contains("no-op")));
    assert!(!constraint_exists(&pool, "orders", "fm_not_null_payload").await);

    cleanup_database(&admin, &db).await;
}

#[tokio::test]
async fn validate_failure_returns_to_scan() {
    let Some(base_url) = env_url() else { return };
    let (db, admin) = isolated_database(&base_url, "it_nn_validate").await;
    let url = with_database(&base_url, &db);
    let pool = PgPool::connect(&url).await.expect("连接失败");
    exec(
        &pool,
        "CREATE TABLE orders (id bigint PRIMARY KEY, payload text)",
    )
    .await;
    exec(
        &pool,
        "INSERT INTO orders SELECT g, 'v' FROM generate_series(1, 10) g",
    )
    .await;
    // NOT VALID 只跳过存量行校验：先写入 NULL，再加约束，VALIDATE 一定失败。
    exec(&pool, "INSERT INTO orders (id, payload) VALUES (999, NULL)").await;
    exec(
        &pool,
        "ALTER TABLE orders ADD CONSTRAINT fm_not_null_payload \
         CHECK (payload IS NOT NULL) NOT VALID",
    )
    .await;

    let ctx = ctx_for(&url, &db).await;
    let hub = MigrationHub::new(None);
    let policy = OpsPolicy::default();
    let job = pg_notnull::new_job(
        &ctx,
        &hub,
        pg_notnull::NotNullRequest {
            schema: None,
            table: "orders".into(),
            column: "payload".into(),
            production_confirmed: false,
        },
    )
    .await
    .unwrap();
    pg_notnull::run_job(&job.job_id, &ctx, &hub, &policy)
        .await
        .expect("执行失败");

    let blocked = hub.get_notnull(&job.job_id).await.unwrap();
    assert_eq!(
        blocked.status,
        MigrationStatus::Blocked,
        "err={:?}",
        blocked.last_error
    );
    assert_eq!(
        blocked.blocking.as_ref().unwrap().phase,
        "validate:violations"
    );
    assert_eq!(blocked.phase, NotNullPhase::Scan, "失败后应退回扫描阶段");
    assert!(blocked.scan.last_pk.is_none());
    assert!(constraint_exists(&pool, "orders", "fm_not_null_payload").await);

    // 修数据后续跑：约束仍在 NOT VALID，preflight 直接走 VALIDATE。
    exec(&pool, "DELETE FROM orders WHERE payload IS NULL").await;
    pg_notnull::run_job(&job.job_id, &ctx, &hub, &policy)
        .await
        .expect("修复后执行失败");
    let done = hub.get_notnull(&job.job_id).await.unwrap();
    assert_eq!(
        done.status,
        MigrationStatus::Completed,
        "notes={:?} err={:?} blocking={:?}",
        done.notes,
        done.last_error,
        done.blocking
    );
    assert!(!nullable(&pool, "orders", "payload").await);

    cleanup_database(&admin, &db).await;
}

#[tokio::test]
async fn text_pk_scan_uses_c_collation() {
    let Some(base_url) = env_url() else { return };
    let (db, admin) = isolated_database(&base_url, "it_nn_text").await;
    let url = with_database(&base_url, &db);
    let pool = PgPool::connect(&url).await.expect("连接失败");
    exec(
        &pool,
        "CREATE TABLE codes (id text PRIMARY KEY, payload text)",
    )
    .await;
    // 混合大小写：若排序规则不是 C，keyset 分页的边界会不一致。
    exec(
        &pool,
        "INSERT INTO codes \
         SELECT CASE WHEN g % 2 = 0 THEN 'A' ELSE 'a' END || lpad(g::text, 4, '0'), 'v' \
         FROM generate_series(1, 400) g",
    )
    .await;

    let ctx = ctx_for(&url, &db).await;
    let hub = MigrationHub::new(None);
    let policy = OpsPolicy::default();
    let job = pg_notnull::new_job(
        &ctx,
        &hub,
        pg_notnull::NotNullRequest {
            schema: None,
            table: "codes".into(),
            column: "payload".into(),
            production_confirmed: false,
        },
    )
    .await
    .unwrap();
    hub.update_notnull(&job.job_id, |job| {
        job.config = BackfillConfig {
            batch_size: 37,
            batch_interval_ms: 0,
            max_rows_per_second: 0,
            single_transaction: false,
        };
    })
    .await
    .unwrap();
    pg_notnull::run_job(&job.job_id, &ctx, &hub, &policy)
        .await
        .expect("执行失败");

    let done = hub.get_notnull(&job.job_id).await.unwrap();
    assert_eq!(
        done.status,
        MigrationStatus::Completed,
        "notes={:?} err={:?} blocking={:?}",
        done.notes,
        done.last_error,
        done.blocking
    );
    assert_eq!(
        done.scan.pk_kind,
        Some(core_lib::ops::field_migration::migration_store::PkKind::Text)
    );
    assert_eq!(done.scan.scanned_rows, 400);
    assert!(!nullable(&pool, "codes", "payload").await);

    cleanup_database(&admin, &db).await;
}

#[tokio::test]
async fn missing_pk_or_partitioned_blocks() {
    let Some(base_url) = env_url() else { return };
    let (db, admin) = isolated_database(&base_url, "it_nn_block").await;
    let url = with_database(&base_url, &db);
    let pool = PgPool::connect(&url).await.expect("连接失败");
    exec(&pool, "CREATE TABLE no_pk (payload text)").await;
    exec(
        &pool,
        "CREATE TABLE parted (id bigint NOT NULL, payload text, PRIMARY KEY (id)) \
         PARTITION BY RANGE (id)",
    )
    .await;

    let ctx = ctx_for(&url, &db).await;
    let hub = MigrationHub::new(None);
    let policy = OpsPolicy::default();

    let no_pk = pg_notnull::new_job(
        &ctx,
        &hub,
        pg_notnull::NotNullRequest {
            schema: None,
            table: "no_pk".into(),
            column: "payload".into(),
            production_confirmed: false,
        },
    )
    .await
    .unwrap();
    pg_notnull::run_job(&no_pk.job_id, &ctx, &hub, &policy)
        .await
        .expect("执行失败");
    let blocked = hub.get_notnull(&no_pk.job_id).await.unwrap();
    assert_eq!(
        blocked.status,
        MigrationStatus::Blocked,
        "err={:?}",
        blocked.last_error
    );
    assert_eq!(blocked.blocking.as_ref().unwrap().phase, "preflight:pk");
    assert!(nullable(&pool, "no_pk", "payload").await);

    let parted = pg_notnull::new_job(
        &ctx,
        &hub,
        pg_notnull::NotNullRequest {
            schema: None,
            table: "parted".into(),
            column: "payload".into(),
            production_confirmed: false,
        },
    )
    .await
    .unwrap();
    pg_notnull::run_job(&parted.job_id, &ctx, &hub, &policy)
        .await
        .expect("执行失败");
    let blocked = hub.get_notnull(&parted.job_id).await.unwrap();
    assert_eq!(
        blocked.status,
        MigrationStatus::Blocked,
        "err={:?}",
        blocked.last_error
    );
    assert_eq!(
        blocked.blocking.as_ref().unwrap().phase,
        "preflight:partitioned"
    );
    assert!(!constraint_exists(&pool, "parted", "fm_not_null_payload").await);

    cleanup_database(&admin, &db).await;
}

#[tokio::test]
async fn spec_mismatch_refuses_resume() {
    let Some(base_url) = env_url() else { return };
    let (db, admin) = isolated_database(&base_url, "it_nn_spec").await;
    let url = with_database(&base_url, &db);
    let pool = PgPool::connect(&url).await.expect("连接失败");
    exec(
        &pool,
        "CREATE TABLE orders (id bigint PRIMARY KEY, payload text, other text)",
    )
    .await;

    let ctx = ctx_for(&url, &db).await;
    let hub = MigrationHub::new(None);
    let policy = OpsPolicy::default();
    let job = pg_notnull::new_job(
        &ctx,
        &hub,
        pg_notnull::NotNullRequest {
            schema: None,
            table: "orders".into(),
            column: "payload".into(),
            production_confirmed: false,
        },
    )
    .await
    .unwrap();
    hub.update_notnull(&job.job_id, |job| job.column = "other".into())
        .await
        .unwrap();
    pg_notnull::run_job(&job.job_id, &ctx, &hub, &policy)
        .await
        .expect("执行失败");

    let blocked = hub.get_notnull(&job.job_id).await.unwrap();
    assert_eq!(
        blocked.status,
        MigrationStatus::Blocked,
        "err={:?}",
        blocked.last_error
    );
    assert_eq!(
        blocked.blocking.as_ref().unwrap().phase,
        "preflight:spec_mismatch"
    );
    assert!(nullable(&pool, "orders", "other").await);

    cleanup_database(&admin, &db).await;
}
