//! PostgreSQL 脚本导入「原子路径」真实库集成测试（opt-in）。
//!
//! 运行方式：
//! ```bash
//! IMPORT_SQL_PG_URL='postgres://postgres:password@127.0.0.1:55432/e2e' \
//!   cargo test -p web-server --bin web-server import_sql_integration_pg -- --nocapture
//! ```
//! 未设置环境变量时逐条打印跳过信息并直接返回，不影响常规 `cargo test`。
//!
//! 断言一律回查数据库真实状态（表是否存在、行数、列是否存在、主键集合），
//! 不只信接口返回的 atomic/rolled_back 字段。

use crate::handlers::go_live::run_import_sql_job;
use crate::state::test_state_with_config;
use crate::ImportSqlJobStartRequest;
use core_lib::config::AppConfig;
use core_lib::db::DbClient;
use core_lib::error::AppError;
use serde_json::Value;
use sqlx::PgPool;
use std::sync::atomic::{AtomicU64, Ordering};

static DB_SEQ: AtomicU64 = AtomicU64::new(0);

fn pg_url() -> Option<String> {
    match std::env::var("IMPORT_SQL_PG_URL") {
        Ok(url) if !url.trim().is_empty() => Some(url),
        _ => {
            eprintln!("跳过 PostgreSQL 脚本导入集成测试：未设置 IMPORT_SQL_PG_URL");
            None
        }
    }
}

/// 运行内唯一的库名后缀：进程号 + 自增序号 + 纳秒时间，避免并行用例互相干扰。
fn unique_suffix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "{}_{}_{}",
        std::process::id(),
        DB_SEQ.fetch_add(1, Ordering::Relaxed),
        nanos
    )
}

fn unique_db(prefix: &str) -> String {
    format!("{prefix}_{}", unique_suffix())
}

/// 换掉 URL 里的库名，保留账号、主机、端口与查询参数。
fn with_database(url: &str, db: &str) -> String {
    match url.rsplit_once('/') {
        Some((base, tail)) => {
            let query = tail.find('?').map(|i| &tail[i..]).unwrap_or("");
            format!("{base}/{db}{query}")
        }
        None => format!("{url}/{db}"),
    }
}

/// 创建本用例独占的库；同名库若存在（极端情况下的残留）先整体清掉。
async fn create_database(admin_pool: &PgPool, db: &str) {
    exec(
        admin_pool,
        &format!("DROP DATABASE IF EXISTS \"{db}\" WITH (FORCE)"),
    )
    .await;
    exec(admin_pool, &format!("CREATE DATABASE \"{db}\"")).await;
}

/// 只删除本用例创建的库；失败必须显式报错，不能静默吞掉。
async fn cleanup_database(admin_pool: &PgPool, db: &str) {
    let sql = format!("DROP DATABASE IF EXISTS \"{db}\" WITH (FORCE)");
    use sqlx::Executor;
    if let Err(err) = Executor::execute(admin_pool, sqlx::raw_sql(&sql)).await {
        eprintln!("清理失败: 数据库 \"{db}\": {err}\nSQL: {sql}");
        panic!("清理失败: 数据库 \"{db}\": {err}");
    }
}

/// 连接管理库并创建本用例独占的库，返回库名与管理连接。
async fn isolated_database(base_url: &str, prefix: &str) -> (String, PgPool) {
    let admin_pool = PgPool::connect(base_url)
        .await
        .expect("连接 PostgreSQL 失败，请确认 compose 服务已就绪");
    let db = unique_db(prefix);
    create_database(&admin_pool, &db).await;
    (db, admin_pool)
}

async fn connect(url: &str) -> (DbClient, PgPool) {
    let client = DbClient::new_default(url)
        .await
        .expect("创建 DbClient 失败，请确认 PostgreSQL 已就绪");
    let pool = PgPool::connect(url)
        .await
        .expect("连接 PostgreSQL 失败，请确认 compose 服务已就绪");
    (client, pool)
}

async fn exec(pool: &PgPool, sql: &str) {
    use sqlx::Executor;
    if let Err(err) = Executor::execute(pool, sqlx::raw_sql(sql)).await {
        panic!("执行 SQL 失败: {err}\nSQL: {sql}");
    }
}

async fn drop_tables(pool: &PgPool, tables: &[&str]) {
    for table in tables {
        exec(pool, &format!("DROP TABLE IF EXISTS {table}")).await;
    }
}

async fn table_exists(pool: &PgPool, table: &str) -> bool {
    sqlx::query_scalar::<_, bool>(&format!(
        "SELECT to_regclass('public.{table}') IS NOT NULL"
    ))
    .fetch_one(pool)
    .await
    .unwrap_or_else(|err| panic!("查询表是否存在失败: {err}\n表: {table}"))
}

async fn column_exists(pool: &PgPool, table: &str, column: &str) -> bool {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
         WHERE table_schema = 'public' AND table_name = $1 AND column_name = $2)",
    )
    .bind(table)
    .bind(column)
    .fetch_one(pool)
    .await
    .unwrap_or_else(|err| panic!("查询列是否存在失败: {err}\n表: {table}.{column}"))
}

async fn row_count(pool: &PgPool, table: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(&format!("SELECT COUNT(*) FROM {table}"))
        .fetch_one(pool)
        .await
        .unwrap_or_else(|err| panic!("统计行数失败: {err}\n表: {table}"))
}

async fn ids(pool: &PgPool, table: &str) -> Vec<i32> {
    sqlx::query_scalar::<_, i32>(&format!("SELECT id FROM {table} ORDER BY id"))
        .fetch_all(pool)
        .await
        .unwrap_or_else(|err| panic!("读取主键失败: {err}\n表: {table}"))
}

async fn run_script(client: &DbClient, sql: &str, skip_errors: bool) -> Result<Value, AppError> {
    let state = test_state_with_config(AppConfig::default());
    let req = ImportSqlJobStartRequest {
        sql: sql.to_string(),
        force: Some(true),
        db_id: None,
        skip_errors,
    };
    run_import_sql_job(client, &state, "integration-test-job", req).await
}

/// 逐语句状态，形如 `["1:executed", "2:rolled_back"]`。
fn statuses(result: &Value) -> Vec<String> {
    result["statements"]
        .as_array()
        .expect("statements 应为数组")
        .iter()
        .map(|entry| {
            format!(
                "{}:{}",
                entry["index"].as_u64().unwrap_or(0),
                entry["status"].as_str().unwrap_or("?")
            )
        })
        .collect()
}

fn message(result: &Value) -> String {
    result["message"].as_str().unwrap_or_default().to_string()
}

/// 不可事务语句必须在执行任何语句之前被拒绝，且响应里能看出是哪一条、哪一类。
fn assert_non_transactional_rejection(
    err: AppError,
    expected_index: u64,
    expected_kind: &str,
) {
    match err {
        AppError::BadRequest(body) => {
            let value: Value = serde_json::from_str(&body).expect("拒绝响应应为 JSON");
            assert_eq!(value["error"], "NON_TRANSACTIONAL_STATEMENT", "{body}");
            assert_eq!(value["statement_index"], expected_index, "{body}");
            assert_eq!(value["statement_kind"], expected_kind, "{body}");
            assert!(
                value["message"].as_str().unwrap_or("").contains("不能在事务块中执行"),
                "{body}"
            );
        }
        other => panic!("应以 BadRequest 拒绝不可事务语句，实际: {other:?}"),
    }
}

// ------------------------- 建表 / DDL + DML 回滚 -------------------------

#[tokio::test(flavor = "multi_thread")]
async fn pg_create_table_then_failure_leaves_no_table() {
    let Some(base_url) = pg_url() else { return };
    let (db, admin_pool) = isolated_database(&base_url, "it_pg").await;
    let url = with_database(&base_url, &db);
    let (client, pool) = connect(&url).await;
    drop_tables(&pool, &["it_pg_ddl"]).await;

    let result = run_script(
        &client,
        "CREATE TABLE it_pg_ddl (id INT PRIMARY KEY, note TEXT);
         INSERT INTO it_pg_ddl VALUES (1, 'a');
         INSERT INTO it_pg_ddl VALUES (2, 'b');
         INSERT INTO it_pg_ddl VALUES (1, 'dup');
         INSERT INTO it_pg_ddl VALUES (3, 'c');",
        false,
    )
    .await
    .expect("脚本导入应返回结构化结果");

    assert_eq!(result["atomic"], Value::Bool(true));
    assert_eq!(result["rolled_back"], Value::Bool(true));
    assert_eq!(result["applied_before_rollback"], Value::from(3));
    assert_eq!(result["failed_statement"], Value::from(4));
    assert_eq!(result["remaining_statements"], Value::from(1));
    assert_eq!(
        statuses(&result),
        vec![
            "1:rolled_back",
            "2:rolled_back",
            "3:rolled_back",
            "4:failed",
            "5:skipped"
        ]
    );
    assert!(message(&result).contains("均未生效"), "{}", message(&result));

    // 回查真实状态：建表也随事务回滚，表必须不存在。
    assert!(
        !table_exists(&pool, "it_pg_ddl").await,
        "建表成功后失败，整段回滚，表不应存在"
    );
    cleanup_database(&admin_pool, &db).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_ddl_dml_mix_failure_rolls_back_everything() {
    let Some(base_url) = pg_url() else { return };
    let (db, admin_pool) = isolated_database(&base_url, "it_pg").await;
    let url = with_database(&base_url, &db);
    let (client, pool) = connect(&url).await;
    drop_tables(&pool, &["it_pg_mix"]).await;
    exec(
        &pool,
        "CREATE TABLE it_pg_mix (id INT PRIMARY KEY, note TEXT)",
    )
    .await;
    exec(&pool, "INSERT INTO it_pg_mix VALUES (1, 'pre')").await;

    let result = run_script(
        &client,
        "INSERT INTO it_pg_mix VALUES (2, 'b');
         ALTER TABLE it_pg_mix ADD COLUMN extra TEXT;
         UPDATE it_pg_mix SET note = 'changed' WHERE id = 2;
         INSERT INTO it_pg_mix VALUES (2, 'dup');",
        false,
    )
    .await
    .expect("脚本导入应返回结构化结果");

    assert_eq!(result["atomic"], Value::Bool(true));
    assert_eq!(result["rolled_back"], Value::Bool(true));
    assert_eq!(result["failed_statement"], Value::from(4));
    assert_eq!(
        statuses(&result),
        vec![
            "1:rolled_back",
            "2:rolled_back",
            "3:rolled_back",
            "4:failed"
        ]
    );

    // 回查真实状态：DML 与 DDL 都没留下痕迹。
    assert_eq!(ids(&pool, "it_pg_mix").await, vec![1]);
    assert!(
        !column_exists(&pool, "it_pg_mix", "extra").await,
        "ALTER TABLE 加列必须随事务回滚"
    );

    drop_tables(&pool, &["it_pg_mix"]).await;
    cleanup_database(&admin_pool, &db).await;
}

// ------------------------- 不可事务语句前置拒绝 -------------------------

#[tokio::test(flavor = "multi_thread")]
async fn pg_rejects_create_index_concurrently_before_execution() {
    let Some(base_url) = pg_url() else { return };
    let (db, admin_pool) = isolated_database(&base_url, "it_pg").await;
    let url = with_database(&base_url, &db);
    let (client, pool) = connect(&url).await;
    drop_tables(&pool, &["it_pg_reject_cic"]).await;

    let err = run_script(
        &client,
        "CREATE TABLE it_pg_reject_cic (id INT);
         CREATE INDEX CONCURRENTLY it_pg_reject_cic_idx ON it_pg_reject_cic (id);",
        false,
    )
    .await
    .expect_err("CREATE INDEX CONCURRENTLY 必须被前置拒绝");

    assert_non_transactional_rejection(err, 2, "CREATE INDEX CONCURRENTLY");
    // 前置拒绝意味着一条都没执行：表与索引都不存在。
    assert!(!table_exists(&pool, "it_pg_reject_cic").await);
    cleanup_database(&admin_pool, &db).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_rejects_vacuum_before_execution() {
    let Some(base_url) = pg_url() else { return };
    let (db, admin_pool) = isolated_database(&base_url, "it_pg").await;
    let url = with_database(&base_url, &db);
    let (client, pool) = connect(&url).await;
    drop_tables(&pool, &["it_pg_reject_vacuum"]).await;

    let err = run_script(
        &client,
        "CREATE TABLE it_pg_reject_vacuum (id INT);
         VACUUM it_pg_reject_vacuum;",
        false,
    )
    .await
    .expect_err("VACUUM 必须被前置拒绝");

    assert_non_transactional_rejection(err, 2, "VACUUM");
    assert!(!table_exists(&pool, "it_pg_reject_vacuum").await);
    cleanup_database(&admin_pool, &db).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_rejects_create_database_before_execution() {
    let Some(base_url) = pg_url() else { return };
    let (db, admin_pool) = isolated_database(&base_url, "it_pg").await;
    let url = with_database(&base_url, &db);
    let (client, pool) = connect(&url).await;
    let forbidden_db = unique_db("it_pg_forbidden");
    cleanup_database(&admin_pool, &forbidden_db).await;

    let err = run_script(
        &client,
        &format!(
            "CREATE TABLE it_pg_reject_db (id INT);
             CREATE DATABASE \"{forbidden_db}\";"
        ),
        false,
    )
    .await
    .expect_err("CREATE DATABASE 必须被前置拒绝");

    assert_non_transactional_rejection(err, 2, "CREATE DATABASE");
    let created = sqlx::query_scalar::<_, bool>(&format!(
        "SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = '{forbidden_db}')"
    ))
    .fetch_one(&pool)
    .await
    .expect("查询 pg_database 失败");
    assert!(!created, "CREATE DATABASE 不应被执行");
    assert!(!table_exists(&pool, "it_pg_reject_db").await);
    cleanup_database(&admin_pool, &forbidden_db).await;
    cleanup_database(&admin_pool, &db).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_rejects_alter_system_before_execution() {
    let Some(base_url) = pg_url() else { return };
    let (db, admin_pool) = isolated_database(&base_url, "it_pg").await;
    let url = with_database(&base_url, &db);
    let (client, pool) = connect(&url).await;
    drop_tables(&pool, &["it_pg_reject_alter"]).await;

    let err = run_script(
        &client,
        "CREATE TABLE it_pg_reject_alter (id INT);
         ALTER SYSTEM SET work_mem = '8MB';",
        false,
    )
    .await
    .expect_err("ALTER SYSTEM 必须被前置拒绝");

    assert_non_transactional_rejection(err, 2, "ALTER SYSTEM");
    assert!(!table_exists(&pool, "it_pg_reject_alter").await);
    cleanup_database(&admin_pool, &db).await;
}

// ------------------------- PREPARE / EXECUTE -------------------------

#[tokio::test(flavor = "multi_thread")]
async fn pg_prepare_execute_runs_in_transaction_and_rolls_back() {
    let Some(base_url) = pg_url() else { return };
    let (db, admin_pool) = isolated_database(&base_url, "it_pg").await;
    let url = with_database(&base_url, &db);
    let (client, pool) = connect(&url).await;
    drop_tables(&pool, &["it_pg_dyn"]).await;
    exec(&pool, "CREATE TABLE it_pg_dyn (id INT PRIMARY KEY, note TEXT)").await;

    let result = run_script(
        &client,
        "PREPARE ins_first AS INSERT INTO it_pg_dyn VALUES (1, 'a');
         EXECUTE ins_first;
         PREPARE ins_dup AS INSERT INTO it_pg_dyn VALUES (1, 'dup');
         EXECUTE ins_dup;
         INSERT INTO it_pg_dyn VALUES (2, 'never');",
        false,
    )
    .await
    .expect("PREPARE/EXECUTE 不应被前置拒绝，应返回结构化结果");

    assert_eq!(result["atomic"], Value::Bool(true), "PREPARE/EXECUTE 在 PG 事务内合法");
    assert_eq!(result["rolled_back"], Value::Bool(true));
    assert_eq!(result["executed_statements"], Value::from(0));
    assert_eq!(result["failed_statement"], Value::from(4));
    assert_eq!(
        statuses(&result),
        vec![
            "1:rolled_back",
            "2:rolled_back",
            "3:rolled_back",
            "4:failed",
            "5:skipped"
        ]
    );

    // 动态 SQL 真的执行过、也真的随事务回滚了：表里一行不剩。
    assert_eq!(row_count(&pool, "it_pg_dyn").await, 0);

    drop_tables(&pool, &["it_pg_dyn"]).await;
    cleanup_database(&admin_pool, &db).await;
}

// ------------------------- 保存点跳过模式 -------------------------

#[tokio::test(flavor = "multi_thread")]
async fn pg_skip_errors_keeps_only_successful_statements() {
    let Some(base_url) = pg_url() else { return };
    let (db, admin_pool) = isolated_database(&base_url, "it_pg").await;
    let url = with_database(&base_url, &db);
    let (client, pool) = connect(&url).await;
    drop_tables(&pool, &["it_pg_skip"]).await;
    exec(
        &pool,
        "CREATE TABLE it_pg_skip (id INT PRIMARY KEY, note TEXT)",
    )
    .await;

    let result = run_script(
        &client,
        "INSERT INTO it_pg_skip VALUES (1, 'a');
         INSERT INTO it_pg_skip VALUES (1, 'dup');
         INSERT INTO it_pg_skip VALUES (2, 'b');
         INSERT INTO it_pg_skip VALUES (2, 'dup2');
         INSERT INTO it_pg_skip VALUES (3, 'c');",
        true,
    )
    .await
    .expect("应返回结构化结果");

    assert_eq!(result["atomic"], Value::Bool(true));
    assert_eq!(result["rolled_back"], Value::Bool(false));
    assert_eq!(result["success"], Value::Bool(false));
    assert_eq!(result["completed"], Value::Bool(true));
    assert_eq!(result["executed_statements"], Value::from(3));
    assert_eq!(result["failed_statements"], Value::from(2));
    assert_eq!(
        statuses(&result),
        vec![
            "1:executed",
            "2:failed",
            "3:executed",
            "4:failed",
            "5:executed"
        ]
    );
    let failure_positions: Vec<u64> = result["failure_details"]
        .as_array()
        .expect("failure_details 应为数组")
        .iter()
        .map(|item| item["index"].as_u64().unwrap())
        .collect();
    assert_eq!(failure_positions, vec![2, 4]);

    // 回查真实状态：保存点只回滚失败的语句，成功语句已提交。
    assert_eq!(ids(&pool, "it_pg_skip").await, vec![1, 2, 3]);

    drop_tables(&pool, &["it_pg_skip"]).await;
    cleanup_database(&admin_pool, &db).await;
}

// ------------------------- 脚本自带事务控制 -------------------------

#[tokio::test(flavor = "multi_thread")]
async fn pg_script_with_explicit_begin_commit_is_rejected() {
    let Some(base_url) = pg_url() else { return };
    let (db, admin_pool) = isolated_database(&base_url, "it_pg").await;
    let url = with_database(&base_url, &db);
    let (client, pool) = connect(&url).await;
    drop_tables(&pool, &["it_pg_tx"]).await;

    let err = run_script(
        &client,
        "BEGIN;
         CREATE TABLE it_pg_tx (id INT);
         INSERT INTO it_pg_tx VALUES (1);
         COMMIT;",
        false,
    )
    .await
    .expect_err("脚本自带 BEGIN/COMMIT 必须被拒绝");

    match err {
        AppError::BadRequest(body) => assert!(
            body.contains("事务控制"),
            "应说明事务控制冲突，实际: {body}"
        ),
        other => panic!("应返回 BadRequest，实际: {other:?}"),
    }
    assert!(!table_exists(&pool, "it_pg_tx").await);
    cleanup_database(&admin_pool, &db).await;
}

// ------------------------- 脚本形态 -------------------------

#[tokio::test(flavor = "multi_thread")]
async fn pg_empty_script_is_rejected() {
    let Some(base_url) = pg_url() else { return };
    let (db, admin_pool) = isolated_database(&base_url, "it_pg").await;
    let url = with_database(&base_url, &db);
    let (client, _pool) = connect(&url).await;

    let err = run_script(&client, "   \n -- only comment\n /* x */ ", false)
        .await
        .expect_err("空脚本应直接拒绝");
    assert!(
        matches!(err, AppError::BadRequest(_)),
        "空脚本应返回 BadRequest，实际：{err:?}"
    );
    cleanup_database(&admin_pool, &db).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_single_statement_script_executes_atomically() {
    let Some(base_url) = pg_url() else { return };
    let (db, admin_pool) = isolated_database(&base_url, "it_pg").await;
    let url = with_database(&base_url, &db);
    let (client, pool) = connect(&url).await;
    drop_tables(&pool, &["it_pg_single"]).await;
    exec(
        &pool,
        "CREATE TABLE it_pg_single (id INT PRIMARY KEY, note TEXT)",
    )
    .await;

    let result = run_script(&client, "INSERT INTO it_pg_single VALUES (1, 'only')", false)
        .await
        .expect("应返回结构化结果");

    assert_eq!(result["success"], Value::Bool(true));
    assert_eq!(result["atomic"], Value::Bool(true));
    assert_eq!(result["total_statements"], Value::from(1));
    assert_eq!(ids(&pool, "it_pg_single").await, vec![1]);

    drop_tables(&pool, &["it_pg_single"]).await;
    cleanup_database(&admin_pool, &db).await;
}

// ------------------------- 美元引用的函数体 -------------------------

#[tokio::test(flavor = "multi_thread")]
async fn pg_tagged_dollar_quoted_function_body_is_not_split() {
    let Some(base_url) = pg_url() else { return };
    let (db, admin_pool) = isolated_database(&base_url, "it_pg").await;
    let url = with_database(&base_url, &db);
    let (client, pool) = connect(&url).await;
    exec(&pool, "DROP FUNCTION IF EXISTS it_pg_dollar_fn(INT)").await;

    // 函数体内含分号与注释里的分号；$fn$ 标签必须保护整体不被拆开。
    let script = "CREATE OR REPLACE FUNCTION it_pg_dollar_fn(x INT) RETURNS INT AS $fn$\n\
                  BEGIN\n\
                    -- body comment with ; inside\n\
                    RETURN x + 1;\n\
                  END;\n\
                  $fn$ LANGUAGE plpgsql;\n\
                  SELECT it_pg_dollar_fn(41);";
    let result = run_script(&client, script, false)
        .await
        .expect("美元引用的函数体应能成功导入");

    assert_eq!(result["success"], Value::Bool(true), "{result}");
    assert_eq!(result["total_statements"], Value::from(2), "{result}");
    assert_eq!(
        statuses(&result),
        vec!["1:executed", "2:executed"],
        "{result}"
    );

    let value: i32 = sqlx::query_scalar("SELECT it_pg_dollar_fn(41)")
        .fetch_one(&pool)
        .await
        .expect("调用函数失败");
    assert_eq!(value, 42);

    exec(&pool, "DROP FUNCTION IF EXISTS it_pg_dollar_fn(INT)").await;
    cleanup_database(&admin_pool, &db).await;
}
