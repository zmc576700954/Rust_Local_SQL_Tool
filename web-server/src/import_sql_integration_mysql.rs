//! MySQL/MariaDB 脚本导入「非原子路径」真实库集成测试（opt-in）。
//!
//! 运行方式：
//! ```bash
//! IMPORT_SQL_MYSQL_URL='mysql://root:password@127.0.0.1:13306/e2e' \
//!   cargo test -p web-server --bin web-server import_sql_mysql -- --nocapture
//! ```
//! 未设置环境变量时逐条打印跳过信息并直接返回，不影响常规 `cargo test`。
//!
//! 断言一律回查数据库真实状态（表是否存在、行数、残留主键），不只信接口返回的
//! atomic/rolled_back 字段。

use crate::handlers::go_live::run_import_sql_job;
use crate::state::test_state_with_config;
use crate::ImportSqlJobStartRequest;
use core_lib::config::AppConfig;
use core_lib::db::DbClient;
use core_lib::error::AppError;
use serde_json::Value;
use sqlx::MySqlPool;

fn mysql_url() -> Option<String> {
    match std::env::var("IMPORT_SQL_MYSQL_URL") {
        Ok(url) if !url.trim().is_empty() => Some(url),
        _ => {
            eprintln!("跳过 MySQL 脚本导入集成测试：未设置 IMPORT_SQL_MYSQL_URL");
            None
        }
    }
}

async fn connect(url: &str) -> (DbClient, MySqlPool) {
    let client = DbClient::new_default(url)
        .await
        .expect("创建 DbClient 失败，请确认 MySQL 已就绪");
    let pool = MySqlPool::connect(url)
        .await
        .expect("连接 MySQL 失败，请确认 compose 服务已就绪");
    (client, pool)
}

/// 用简单查询协议执行（与生产 `EngineConnection::execute_command` 一致），
/// 避免 `DROP PROCEDURE` / `CREATE PROCEDURE` 走 prepared 协议时报 1295。
async fn exec(pool: &MySqlPool, sql: &str) {
    use sqlx::Executor;
    if let Err(err) = Executor::execute(pool, sqlx::raw_sql(sql)).await {
        panic!("执行 SQL 失败: {err}\nSQL: {sql}");
    }
}

async fn drop_tables(pool: &MySqlPool, tables: &[&str]) {
    for table in tables {
        exec(pool, &format!("DROP TABLE IF EXISTS {table}")).await;
    }
}

async fn scalar_i64(pool: &MySqlPool, sql: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(sql)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|err| panic!("查询失败: {err}\nSQL: {sql}"))
}

async fn table_exists(pool: &MySqlPool, table: &str) -> bool {
    scalar_i64(
        pool,
        &format!(
            "SELECT COUNT(*) FROM information_schema.tables \
             WHERE table_schema = DATABASE() AND table_name = '{table}'"
        ),
    )
    .await
        > 0
}

async fn row_count(pool: &MySqlPool, table: &str) -> i64 {
    scalar_i64(pool, &format!("SELECT COUNT(*) FROM {table}")).await
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

/// 逐语句状态，形如 `["1:executed", "2:failed"]`，用于和库内实际对照。
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

fn reason(result: &Value) -> String {
    result["no_transaction_reason"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

// ------------------------- 失败位置 -------------------------

#[tokio::test(flavor = "multi_thread")]
async fn mysql_ddl_script_failure_reports_partial_state_and_keeps_rows() {
    let Some(url) = mysql_url() else { return };
    let (client, pool) = connect(&url).await;
    drop_tables(&pool, &["it_ddl_fail"]).await;

    let result = run_script(
        &client,
        "CREATE TABLE it_ddl_fail (id INT PRIMARY KEY, note VARCHAR(32)) ENGINE=InnoDB;
         INSERT INTO it_ddl_fail VALUES (1, 'a');
         INSERT INTO it_ddl_fail VALUES (2, 'b');
         INSERT INTO it_ddl_fail VALUES (1, 'dup');
         INSERT INTO it_ddl_fail VALUES (3, 'c');",
        false,
    )
    .await
    .expect("脚本导入应返回结构化结果而不是直接报错");

    assert_eq!(result["atomic"], Value::Bool(false), "含 DDL 必须走非原子路径");
    assert_eq!(result["rolled_back"], Value::Bool(false), "DDL 脚本不能声称已回滚");
    assert_eq!(result["executed_statements"], Value::from(3));
    assert_eq!(result["failed_statement"], Value::from(4));
    assert_eq!(result["remaining_statements"], Value::from(1));
    assert_eq!(
        statuses(&result),
        vec![
            "1:executed",
            "2:executed",
            "3:executed",
            "4:failed",
            "5:skipped"
        ]
    );

    // 回查真实状态：表存在、残留两行、第三行（失败行）未写入。
    assert!(table_exists(&pool, "it_ddl_fail").await, "建表已生效");
    assert_eq!(row_count(&pool, "it_ddl_fail").await, 2);
    assert!(!message(&result).is_empty());

    drop_tables(&pool, &["it_ddl_fail"]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn mysql_failure_at_first_statement_leaves_no_new_rows() {
    let Some(url) = mysql_url() else { return };
    let (client, pool) = connect(&url).await;
    drop_tables(&pool, &["it_fail_first"]).await;
    exec(
        &pool,
        "CREATE TABLE it_fail_first (id INT PRIMARY KEY, note VARCHAR(32)) ENGINE=InnoDB",
    )
    .await;
    exec(&pool, "INSERT INTO it_fail_first VALUES (1, 'pre')").await;

    let result = run_script(
        &client,
        "INSERT INTO it_fail_first VALUES (1, 'dup');
         INSERT INTO it_fail_first VALUES (2, 'b');
         INSERT INTO it_fail_first VALUES (3, 'c');",
        false,
    )
    .await
    .expect("应返回结构化结果");

    assert_eq!(result["executed_statements"], Value::from(0));
    assert_eq!(result["failed_statement"], Value::from(1));
    assert_eq!(result["remaining_statements"], Value::from(2));
    assert_eq!(
        statuses(&result),
        vec!["1:failed", "2:skipped", "3:skipped"]
    );
    // 纯 DML 走事务路径：失败后整体回滚，只剩预置的那一行。
    assert_eq!(result["atomic"], Value::Bool(true));
    assert_eq!(row_count(&pool, "it_fail_first").await, 1);

    drop_tables(&pool, &["it_fail_first"]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn mysql_failure_at_middle_statement_keeps_only_earlier_rows() {
    let Some(url) = mysql_url() else { return };
    let (client, pool) = connect(&url).await;
    drop_tables(&pool, &["it_fail_mid"]).await;
    exec(
        &pool,
        "CREATE TABLE it_fail_mid (id INT PRIMARY KEY, note VARCHAR(32)) ENGINE=InnoDB",
    )
    .await;

    // 第 2 条 CREATE TABLE 使脚本不可事务化，因此成功语句会真正落库。
    let result = run_script(
        &client,
        "INSERT INTO it_fail_mid VALUES (1, 'a');
         CREATE TABLE it_fail_mid_aux (id INT);
         INSERT INTO it_fail_mid VALUES (1, 'dup');
         INSERT INTO it_fail_mid VALUES (3, 'c');",
        false,
    )
    .await
    .expect("应返回结构化结果");

    assert_eq!(result["atomic"], Value::Bool(false));
    assert_eq!(result["executed_statements"], Value::from(2));
    assert_eq!(result["failed_statement"], Value::from(3));
    assert_eq!(result["remaining_statements"], Value::from(1));
    assert_eq!(
        statuses(&result),
        vec![
            "1:executed",
            "2:executed",
            "3:failed",
            "4:skipped"
        ]
    );
    assert_eq!(row_count(&pool, "it_fail_mid").await, 1);
    assert!(table_exists(&pool, "it_fail_mid_aux").await);

    drop_tables(&pool, &["it_fail_mid", "it_fail_mid_aux"]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn mysql_failure_at_last_statement_reports_zero_remaining() {
    let Some(url) = mysql_url() else { return };
    let (client, pool) = connect(&url).await;
    drop_tables(&pool, &["it_fail_last"]).await;
    exec(
        &pool,
        "CREATE TABLE it_fail_last (id INT PRIMARY KEY, note VARCHAR(32)) ENGINE=InnoDB",
    )
    .await;

    let result = run_script(
        &client,
        "INSERT INTO it_fail_last VALUES (1, 'a');
         INSERT INTO it_fail_last VALUES (2, 'b');
         INSERT INTO it_fail_last VALUES (1, 'dup');",
        false,
    )
    .await
    .expect("应返回结构化结果");

    // 纯 DML → 事务路径，整体回滚。
    assert_eq!(result["atomic"], Value::Bool(true));
    assert_eq!(result["rolled_back"], Value::Bool(true));
    assert_eq!(result["executed_statements"], Value::from(0));
    assert_eq!(result["failed_statement"], Value::from(3));
    assert_eq!(result["remaining_statements"], Value::from(0));
    assert_eq!(
        statuses(&result),
        vec!["1:rolled_back", "2:rolled_back", "3:failed"]
    );
    assert_eq!(row_count(&pool, "it_fail_last").await, 0);

    drop_tables(&pool, &["it_fail_last"]).await;
}

// ------------------------- 各类非原子触发语句 -------------------------

#[tokio::test(flavor = "multi_thread")]
async fn mysql_dynamic_sql_prepare_execute_uses_non_atomic_path() {
    let Some(url) = mysql_url() else { return };
    let (client, pool) = connect(&url).await;
    drop_tables(&pool, &["it_dyn_rows", "it_dyn_created"]).await;
    exec(
        &pool,
        "CREATE TABLE it_dyn_rows (id INT PRIMARY KEY, note VARCHAR(32)) ENGINE=InnoDB",
    )
    .await;

    let result = run_script(
        &client,
        "INSERT INTO it_dyn_rows VALUES (1, 'a');
         PREPARE stmt FROM 'CREATE TABLE it_dyn_created (id INT PRIMARY KEY)';
         EXECUTE stmt;
         DEALLOCATE PREPARE stmt;
         INSERT INTO it_dyn_rows VALUES (1, 'dup');
         INSERT INTO it_dyn_rows VALUES (3, 'c');",
        false,
    )
    .await
    .expect("应返回结构化结果");

    assert_eq!(result["atomic"], Value::Bool(false));
    assert_eq!(result["rolled_back"], Value::Bool(false));
    assert_eq!(result["executed_statements"], Value::from(4));
    assert_eq!(result["failed_statement"], Value::from(5));
    assert_eq!(
        statuses(&result),
        vec![
            "1:executed",
            "2:executed",
            "3:executed",
            "4:executed",
            "5:failed",
            "6:skipped"
        ]
    );
    let reason = reason(&result);
    assert!(
        reason.contains("PREPARE") && reason.contains("动态 SQL"),
        "原因应点明动态 SQL：{reason}"
    );
    // 库内实际：动态建的表真的存在，前面的 DML 也真的落库。
    assert!(table_exists(&pool, "it_dyn_created").await);
    assert_eq!(row_count(&pool, "it_dyn_rows").await, 1);

    drop_tables(&pool, &["it_dyn_rows", "it_dyn_created"]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn mysql_call_statement_uses_non_atomic_path() {
    let Some(url) = mysql_url() else { return };
    let (client, pool) = connect(&url).await;
    drop_tables(&pool, &["it_call_rows"]).await;
    exec(&pool, "DROP PROCEDURE IF EXISTS it_call_proc").await;
    exec(
        &pool,
        "CREATE TABLE it_call_rows (id INT PRIMARY KEY, note VARCHAR(32)) ENGINE=InnoDB",
    )
    .await;
    exec(
        &pool,
        "CREATE PROCEDURE it_call_proc() INSERT INTO it_call_rows VALUES (99, 'proc')",
    )
    .await;

    let result = run_script(
        &client,
        "INSERT INTO it_call_rows VALUES (1, 'a');
         CALL it_call_proc();
         INSERT INTO it_call_rows VALUES (1, 'dup');",
        false,
    )
    .await
    .expect("应返回结构化结果");

    assert_eq!(result["atomic"], Value::Bool(false), "CALL 可能自带 COMMIT");
    assert_eq!(result["executed_statements"], Value::from(2));
    assert_eq!(result["failed_statement"], Value::from(3));
    assert!(reason(&result).contains("CALL"), "{}", reason(&result));
    // 事务路径下这两行会被回滚；非原子路径下必须真实存在。
    assert_eq!(row_count(&pool, "it_call_rows").await, 2);
    assert_eq!(
        scalar_i64(&pool, "SELECT COUNT(*) FROM it_call_rows WHERE id = 99").await,
        1
    );

    drop_tables(&pool, &["it_call_rows"]).await;
    exec(&pool, "DROP PROCEDURE IF EXISTS it_call_proc").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn mysql_set_autocommit_uses_non_atomic_path() {
    let Some(url) = mysql_url() else { return };
    let (client, pool) = connect(&url).await;
    drop_tables(&pool, &["it_autocommit"]).await;
    exec(
        &pool,
        "CREATE TABLE it_autocommit (id INT PRIMARY KEY, note VARCHAR(32)) ENGINE=InnoDB",
    )
    .await;

    let result = run_script(
        &client,
        "INSERT INTO it_autocommit VALUES (1, 'a');
         SET autocommit = 1;
         INSERT INTO it_autocommit VALUES (2, 'b');
         INSERT INTO it_autocommit VALUES (1, 'dup');",
        false,
    )
    .await
    .expect("应返回结构化结果");

    assert_eq!(result["atomic"], Value::Bool(false));
    assert_eq!(result["executed_statements"], Value::from(3));
    assert_eq!(result["failed_statement"], Value::from(4));
    let reason_upper = reason(&result).to_uppercase();
    assert!(
        reason_upper.contains("SET AUTOCOMMIT"),
        "{}",
        reason(&result)
    );
    assert_eq!(row_count(&pool, "it_autocommit").await, 2);

    drop_tables(&pool, &["it_autocommit"]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn mysql_lock_tables_uses_non_atomic_path() {
    let Some(url) = mysql_url() else { return };
    let (client, pool) = connect(&url).await;
    drop_tables(&pool, &["it_lock"]).await;
    exec(
        &pool,
        "CREATE TABLE it_lock (id INT PRIMARY KEY, note VARCHAR(32)) ENGINE=InnoDB",
    )
    .await;

    // 失败语句放在 UNLOCK TABLES 之后，确保锁在脚本内释放，
    // 否则连接池里那条会话会一直持锁，清理时等元数据锁会挂住。
    let result = run_script(
        &client,
        "LOCK TABLES it_lock WRITE;
         INSERT INTO it_lock VALUES (1, 'a');
         UNLOCK TABLES;
         INSERT INTO it_lock VALUES (1, 'dup');",
        false,
    )
    .await
    .expect("应返回结构化结果");

    assert_eq!(result["atomic"], Value::Bool(false));
    assert_eq!(result["executed_statements"], Value::from(3));
    assert_eq!(result["failed_statement"], Value::from(4));
    assert_eq!(result["remaining_statements"], Value::from(0));
    assert!(reason(&result).contains("LOCK TABLES"), "{}", reason(&result));
    assert_eq!(row_count(&pool, "it_lock").await, 1);

    drop_tables(&pool, &["it_lock"]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn mysql_reason_points_to_first_offending_statement() {
    let Some(url) = mysql_url() else { return };
    let (client, pool) = connect(&url).await;
    drop_tables(&pool, &["it_reason", "it_reason_aux"]).await;
    exec(
        &pool,
        "CREATE TABLE it_reason (id INT PRIMARY KEY, note VARCHAR(32)) ENGINE=InnoDB",
    )
    .await;

    let result = run_script(
        &client,
        "INSERT INTO it_reason VALUES (1, 'a');
         PREPARE stmt FROM 'SELECT 1';
         CREATE TABLE it_reason_aux (id INT);
         DROP TABLE IF EXISTS it_reason_aux;",
        false,
    )
    .await
    .expect("应返回结构化结果");

    let reason = reason(&result);
    assert!(reason.contains("第 2 条"), "应指向第 2 条：{reason}");
    assert!(reason.contains("PREPARE"), "应包含语句预览：{reason}");
    assert_eq!(result["atomic"], Value::Bool(false));

    drop_tables(&pool, &["it_reason", "it_reason_aux"]).await;
}

// ------------------------- 跳过继续 -------------------------

#[tokio::test(flavor = "multi_thread")]
async fn mysql_skip_errors_summarizes_failures_and_continues() {
    let Some(url) = mysql_url() else { return };
    let (client, pool) = connect(&url).await;
    drop_tables(&pool, &["it_skip", "it_skip_aux"]).await;
    exec(
        &pool,
        "CREATE TABLE it_skip (id INT PRIMARY KEY, note VARCHAR(32)) ENGINE=InnoDB",
    )
    .await;

    let result = run_script(
        &client,
        "CREATE TABLE it_skip_aux (id INT PRIMARY KEY);
         INSERT INTO it_skip VALUES (1, 'a');
         INSERT INTO it_skip VALUES (1, 'dup');
         INSERT INTO it_skip VALUES (2, 'b');
         INSERT INTO it_skip VALUES (2, 'dup2');
         INSERT INTO it_skip VALUES (3, 'c');",
        true,
    )
    .await
    .expect("应返回结构化结果");

    assert_eq!(result["atomic"], Value::Bool(false));
    assert_eq!(result["success"], Value::Bool(false));
    assert_eq!(result["completed"], Value::Bool(true), "跳过模式应跑完全部语句");
    assert_eq!(result["executed_statements"], Value::from(4), "{result}");
    assert_eq!(result["failed_statements"], Value::from(2));
    assert_eq!(
        statuses(&result),
        vec![
            "1:executed",
            "2:executed",
            "3:failed",
            "4:executed",
            "5:failed",
            "6:executed"
        ]
    );
    let failure_positions: Vec<u64> = result["failure_details"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["index"].as_u64().unwrap())
        .collect();
    assert_eq!(failure_positions, vec![3, 5]);
    let msg = message(&result);
    assert!(msg.contains("位置：3, 5"), "{}", msg);
    // 库内实际：3 条成功语句都真实存在。
    assert_eq!(row_count(&pool, "it_skip").await, 3);

    drop_tables(&pool, &["it_skip", "it_skip_aux"]).await;
}

// ------------------------- 事务引擎边界 -------------------------

#[tokio::test(flavor = "multi_thread")]
async fn mysql_myisam_rows_survive_reported_transaction_rollback() {
    let Some(url) = mysql_url() else { return };
    let (client, pool) = connect(&url).await;
    drop_tables(&pool, &["it_myisam"]).await;
    exec(
        &pool,
        "CREATE TABLE it_myisam (id INT PRIMARY KEY, note VARCHAR(32)) ENGINE=MyISAM",
    )
    .await;

    // 纯 DML → 会走事务路径；但 MyISAM 不支持事务，ROLLBACK 不会撤销写入。
    let result = run_script(
        &client,
        "INSERT INTO it_myisam VALUES (1, 'a');
         INSERT INTO it_myisam VALUES (2, 'b');
         INSERT INTO it_myisam VALUES (1, 'dup');",
        false,
    )
    .await
    .expect("应返回结构化结果");

    assert_eq!(result["atomic"], Value::Bool(true));
    assert_eq!(result["rolled_back"], Value::Bool(true));
    assert!(
        result["engine_note"].as_str().unwrap_or_default().contains("InnoDB"),
        "必须说明事务只对支持事务的引擎生效"
    );
    // 关键断言：接口声称已回滚，但 MyISAM 上两行确实留下了。
    assert_eq!(
        row_count(&pool, "it_myisam").await,
        2,
        "MyISAM 不回滚，残留必须如实存在"
    );

    drop_tables(&pool, &["it_myisam"]).await;
}

// ------------------------- 脚本形态 -------------------------

#[tokio::test(flavor = "multi_thread")]
async fn mysql_empty_script_is_rejected() {
    let Some(url) = mysql_url() else { return };
    let (client, _pool) = connect(&url).await;

    let err = run_script(&client, "   \n -- only comment\n /* x */ ", false)
        .await
        .expect_err("空脚本应直接拒绝");
    assert!(
        matches!(err, AppError::BadRequest(_)),
        "空脚本应返回 BadRequest，实际：{err:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn mysql_single_statement_script_executes_atomically() {
    let Some(url) = mysql_url() else { return };
    let (client, pool) = connect(&url).await;
    drop_tables(&pool, &["it_single"]).await;
    exec(
        &pool,
        "CREATE TABLE it_single (id INT PRIMARY KEY, note VARCHAR(32)) ENGINE=InnoDB",
    )
    .await;

    let result = run_script(
        &client,
        "INSERT INTO it_single VALUES (1, 'only')",
        false,
    )
    .await
    .expect("应返回结构化结果");

    assert_eq!(result["success"], Value::Bool(true));
    assert_eq!(result["atomic"], Value::Bool(true));
    assert_eq!(result["total_statements"], Value::from(1));
    assert_eq!(row_count(&pool, "it_single").await, 1);

    drop_tables(&pool, &["it_single"]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn mysql_ddl_keywords_in_comments_and_literals_do_not_trigger_non_atomic() {
    let Some(url) = mysql_url() else { return };
    let (client, pool) = connect(&url).await;
    drop_tables(&pool, &["it_literal", "it_literal_phantom"]).await;
    exec(
        &pool,
        "CREATE TABLE it_literal (id INT PRIMARY KEY, note VARCHAR(64)) ENGINE=InnoDB",
    )
    .await;

    let result = run_script(
        &client,
        "-- DROP TABLE it_literal_phantom\n\
         INSERT INTO it_literal VALUES (1, 'DROP TABLE it_literal_phantom'); /* CREATE TABLE x */\n\
         INSERT INTO it_literal VALUES (2, 'a;b');",
        false,
    )
    .await
    .expect("应返回结构化结果");

    assert_eq!(
        result["atomic"],
        Value::Bool(true),
        "注释与字符串里的 DDL 关键字不应触发非原子路径"
    );
    assert_eq!(result["success"], Value::Bool(true));
    assert_eq!(row_count(&pool, "it_literal").await, 2);
    assert!(!table_exists(&pool, "it_literal_phantom").await);

    drop_tables(&pool, &["it_literal"]).await;
}
