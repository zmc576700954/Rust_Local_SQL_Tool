//! MySQL/MariaDB 脚本导入「非原子路径」真实库集成测试（opt-in）。
//!
//! 运行方式：
//! ```bash
//! IMPORT_SQL_MYSQL_URL='mysql://root:password@127.0.0.1:33306/e2e' \
//!   cargo test -p web-server --bin web-server import_sql_integration_mysql -- --nocapture
//! ```
//! 未设置环境变量时逐条打印跳过信息并直接返回，不影响常规 `cargo test`。
//!
//! 同一套用例已验证 MySQL 8.0 与 MariaDB 11：隐式提交判定清单（DDL、LOCK TABLES、
//! SET autocommit、CALL、事务控制）、PREPARE/EXECUTE、DDL 失败残留、纯 DML 回滚、
//! 跳过继续模式与引擎降级行为均一致。MariaDB 额外支持 Aria 引擎，由「非 InnoDB
//! 引擎一律降级」的判定覆盖，未单独构造 Aria 用例。
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

async fn current_database(pool: &MySqlPool) -> String {
    sqlx::query_scalar::<_, String>("SELECT DATABASE()")
        .fetch_one(pool)
        .await
        .expect("读取当前库名失败")
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

async fn ids_of(pool: &MySqlPool, table: &str) -> Vec<i64> {
    sqlx::query_scalar::<_, i64>(&format!("SELECT id FROM {table} ORDER BY id"))
        .fetch_all(pool)
        .await
        .unwrap_or_else(|err| panic!("读取主键失败: {err}\n表: {table}"))
}

/// 换掉 URL 里的账号密码，用于构造受限账号连接。
fn url_with_credentials(url: &str, user: &str, password: &str) -> String {
    match url.split_once("://").and_then(|(scheme, rest)| {
        rest.split_once('@').map(|(_, host)| (scheme, host))
    }) {
        Some((scheme, host)) => format!("{scheme}://{user}:{password}@{host}"),
        None => url.to_string(),
    }
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
async fn mysql_explicit_transaction_control_uses_non_atomic_path() {
    let Some(url) = mysql_url() else { return };
    let (client, pool) = connect(&url).await;
    drop_tables(&pool, &["it_tx"]).await;
    exec(
        &pool,
        "CREATE TABLE it_tx (id INT PRIMARY KEY, note VARCHAR(32)) ENGINE=InnoDB",
    )
    .await;

    // 脚本自带 BEGIN/ROLLBACK：不能在外层再包事务，走非原子路径逐条执行。
    let result = run_script(
        &client,
        "BEGIN;
         INSERT INTO it_tx VALUES (1, 'a');
         ROLLBACK;",
        false,
    )
    .await
    .expect("应返回结构化结果");

    assert_eq!(result["success"], Value::Bool(true));
    assert_eq!(result["completed"], Value::Bool(true));
    assert_eq!(result["atomic"], Value::Bool(false));
    assert_eq!(result["executed_statements"], Value::from(3));
    let reason = reason(&result);
    assert!(
        reason.contains("隐式提交") && reason.contains("BEGIN"),
        "no_transaction_reason 应指向事务控制语句：{reason}"
    );
    // 脚本自己的 ROLLBACK 生效：库内没有残留。
    assert_eq!(row_count(&pool, "it_tx").await, 0);

    drop_tables(&pool, &["it_tx"]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn mysql_myisam_script_downgrades_full_rollback_claim() {
    let Some(url) = mysql_url() else { return };
    let (client, pool) = connect(&url).await;
    drop_tables(&pool, &["it_myisam"]).await;
    exec(
        &pool,
        "CREATE TABLE it_myisam (id INT PRIMARY KEY, note VARCHAR(32)) ENGINE=MyISAM",
    )
    .await;

    // 纯 DML → 会走事务路径；但 MyISAM 不支持事务，承诺必须降级。
    let result = run_script(
        &client,
        "INSERT INTO it_myisam VALUES (1, 'a');
         INSERT INTO it_myisam VALUES (2, 'b');
         INSERT INTO it_myisam VALUES (1, 'dup');",
        false,
    )
    .await
    .expect("应返回结构化结果");

    // 新行为：不再声称 atomic/rolled_back，明确只覆盖事务引擎。
    assert_eq!(result["atomic"], Value::Bool(false));
    assert_eq!(result["rolled_back"], Value::Bool(false));
    assert_eq!(result["rollback_scope"], "transactional_engines_only");
    let listed = result["non_transactional_tables"]
        .as_array()
        .expect("non_transactional_tables 应为数组");
    assert_eq!(listed.len(), 1, "{result}");
    assert_eq!(listed[0]["schema"], current_database(&pool).await);
    assert_eq!(listed[0]["table"], "it_myisam");
    assert_eq!(listed[0]["engine"], "MyISAM");
    assert_eq!(result["verification_status"], "downgraded_non_transactional");
    assert!(
        result["engine_note"]
            .as_str()
            .unwrap_or_default()
            .contains("it_myisam(MyISAM)"),
        "engine_note 必须点名命中的非事务表：{}",
        result["engine_note"]
    );
    assert!(
        message(&result).contains("不参与事务回滚"),
        "{}",
        message(&result)
    );
    // 已执行语句不能标成 rolled_back（否则又变成与库内事实不符的结论）。
    assert_eq!(
        statuses(&result),
        vec!["1:executed", "2:executed", "3:failed"]
    );

    // 回查真实状态：MyISAM 上两行确实留下。
    assert_eq!(
        row_count(&pool, "it_myisam").await,
        2,
        "MyISAM 不回滚，残留必须如实存在"
    );

    drop_tables(&pool, &["it_myisam"]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn mysql_mixed_engine_dml_lists_only_non_transactional_targets() {
    let Some(url) = mysql_url() else { return };
    let (client, pool) = connect(&url).await;
    drop_tables(&pool, &["it_mix_innodb", "it_mix_myisam"]).await;
    exec(
        &pool,
        "CREATE TABLE it_mix_innodb (id INT PRIMARY KEY, note VARCHAR(32)) ENGINE=InnoDB",
    )
    .await;
    exec(
        &pool,
        "CREATE TABLE it_mix_myisam (id INT PRIMARY KEY, note VARCHAR(32)) ENGINE=MyISAM",
    )
    .await;

    let result = run_script(
        &client,
        "INSERT INTO it_mix_innodb VALUES (1, 'a');
         INSERT INTO it_mix_myisam VALUES (1, 'm');
         INSERT INTO it_mix_innodb VALUES (2, 'b');
         INSERT INTO it_mix_myisam VALUES (1, 'dup');",
        false,
    )
    .await
    .expect("应返回结构化结果");

    assert_eq!(result["atomic"], Value::Bool(false));
    assert_eq!(result["rolled_back"], Value::Bool(false));
    assert_eq!(result["rollback_scope"], "transactional_engines_only");
    // 精准识别：只列出被 DML 命中的非事务表，而不是全库扫出来的所有表。
    let listed = result["non_transactional_tables"]
        .as_array()
        .expect("non_transactional_tables 应为数组");
    assert_eq!(listed.len(), 1, "{result}");
    assert_eq!(listed[0]["schema"], current_database(&pool).await);
    assert_eq!(listed[0]["table"], "it_mix_myisam");
    assert_eq!(listed[0]["engine"], "MyISAM");
    assert_eq!(result["verification_status"], "downgraded_non_transactional");
    assert_eq!(
        statuses(&result),
        vec!["1:executed", "2:executed", "3:executed", "4:failed"]
    );

    // 回查真实状态：InnoDB 的回滚了，MyISAM 的留下了。
    assert_eq!(row_count(&pool, "it_mix_innodb").await, 0);
    assert_eq!(row_count(&pool, "it_mix_myisam").await, 1);
    assert_eq!(ids_of(&pool, "it_mix_myisam").await, vec![1]);

    drop_tables(&pool, &["it_mix_innodb", "it_mix_myisam"]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn mysql_memory_engine_script_downgrades_rollback_claim() {
    let Some(url) = mysql_url() else { return };
    let (client, pool) = connect(&url).await;
    drop_tables(&pool, &["it_memory"]).await;
    exec(
        &pool,
        "CREATE TABLE it_memory (id INT PRIMARY KEY, note VARCHAR(32)) ENGINE=MEMORY",
    )
    .await;

    let result = run_script(
        &client,
        "INSERT INTO it_memory VALUES (1, 'a');
         INSERT INTO it_memory VALUES (2, 'b');
         INSERT INTO it_memory VALUES (1, 'dup');",
        false,
    )
    .await
    .expect("应返回结构化结果");

    assert_eq!(result["atomic"], Value::Bool(false));
    assert_eq!(result["rolled_back"], Value::Bool(false));
    let listed = result["non_transactional_tables"]
        .as_array()
        .expect("non_transactional_tables 应为数组");
    assert_eq!(listed.len(), 1, "{result}");
    assert_eq!(listed[0]["schema"], current_database(&pool).await);
    assert_eq!(listed[0]["table"], "it_memory");
    assert_eq!(listed[0]["engine"], "MEMORY");
    assert_eq!(result["verification_status"], "downgraded_non_transactional");
    assert_eq!(row_count(&pool, "it_memory").await, 2);

    drop_tables(&pool, &["it_memory"]).await;
}

// ------------------------- 跨库 DML 引擎核查 -------------------------

/// 跨库 MyISAM 表：必须降级，并且返回里要能看出是哪个库的表、什么引擎。
#[tokio::test(flavor = "multi_thread")]
async fn mysql_cross_database_myisam_table_downgrades_with_schema() {
    let Some(url) = mysql_url() else { return };
    let (client, pool) = connect(&url).await;
    exec(&pool, "CREATE DATABASE IF NOT EXISTS it_cross_mydb").await;
    drop_tables(
        &pool,
        &["it_xdb_local", "it_cross_mydb.it_xdb_myisam"],
    )
    .await;
    exec(
        &pool,
        "CREATE TABLE it_xdb_local (id INT PRIMARY KEY, note VARCHAR(32)) ENGINE=InnoDB",
    )
    .await;
    exec(
        &pool,
        "CREATE TABLE it_cross_mydb.it_xdb_myisam (id INT PRIMARY KEY, note VARCHAR(32)) ENGINE=MyISAM",
    )
    .await;

    let result = run_script(
        &client,
        "INSERT INTO it_xdb_local VALUES (1, 'a');
         INSERT INTO it_cross_mydb.it_xdb_myisam VALUES (1, 'm');
         INSERT INTO it_xdb_local VALUES (2, 'b');
         INSERT INTO it_cross_mydb.it_xdb_myisam VALUES (1, 'dup');",
        false,
    )
    .await
    .expect("应返回结构化结果");

    assert_eq!(result["atomic"], Value::Bool(false), "{result}");
    assert_eq!(result["rolled_back"], Value::Bool(false), "{result}");
    assert_eq!(result["rollback_scope"], "transactional_engines_only");
    assert_eq!(result["verification_status"], "downgraded_non_transactional");

    let listed = result["non_transactional_tables"]
        .as_array()
        .expect("non_transactional_tables 应为数组");
    assert_eq!(listed.len(), 1, "{result}");
    // 跨库表必须能区分库名。
    assert_eq!(listed[0]["schema"], "it_cross_mydb");
    assert_eq!(listed[0]["table"], "it_xdb_myisam");
    assert_eq!(listed[0]["engine"], "MyISAM");

    let cross = result["cross_database_tables"]
        .as_array()
        .expect("cross_database_tables 应为数组");
    assert_eq!(cross.len(), 1, "{result}");
    assert_eq!(cross[0]["schema"], "it_cross_mydb");
    assert_eq!(cross[0]["table"], "it_xdb_myisam");
    assert_eq!(cross[0]["engine"], "MyISAM");
    assert_eq!(cross[0]["transactional"], Value::Bool(false));

    assert!(
        result["engine_note"]
            .as_str()
            .unwrap_or_default()
            .contains("it_cross_mydb.it_xdb_myisam(MyISAM)"),
        "engine_note 必须点名跨库非事务表：{}",
        result["engine_note"]
    );
    assert_eq!(
        statuses(&result),
        vec!["1:executed", "2:executed", "3:executed", "4:failed"]
    );

    // 回查真实状态：当前库 InnoDB 回滚，跨库 MyISAM 残留。
    assert_eq!(row_count(&pool, "it_xdb_local").await, 0);
    assert_eq!(row_count(&pool, "it_cross_mydb.it_xdb_myisam").await, 1);

    drop_tables(&pool, &["it_xdb_local", "it_cross_mydb.it_xdb_myisam"]).await;
}

/// 跨库 InnoDB 表：同实例内能查到真实引擎 → 维持 atomic=true。
#[tokio::test(flavor = "multi_thread")]
async fn mysql_cross_database_innodb_table_keeps_atomic_promise() {
    let Some(url) = mysql_url() else { return };
    let (client, pool) = connect(&url).await;
    exec(&pool, "CREATE DATABASE IF NOT EXISTS it_cross_mydb").await;
    drop_tables(&pool, &["it_cross_mydb.it_xdb_innodb"]).await;
    exec(
        &pool,
        "CREATE TABLE it_cross_mydb.it_xdb_innodb (id INT PRIMARY KEY, note VARCHAR(32)) ENGINE=InnoDB",
    )
    .await;

    let result = run_script(
        &client,
        "INSERT INTO it_cross_mydb.it_xdb_innodb VALUES (1, 'a');
         INSERT INTO it_cross_mydb.it_xdb_innodb VALUES (2, 'b');
         INSERT INTO it_cross_mydb.it_xdb_innodb VALUES (1, 'dup');",
        false,
    )
    .await
    .expect("应返回结构化结果");

    // 查得到引擎且全部支持事务 → 不允许一律降级。
    assert_eq!(result["atomic"], Value::Bool(true), "{result}");
    assert_eq!(result["rolled_back"], Value::Bool(true), "{result}");
    assert_eq!(result["rollback_scope"], Value::Null);
    assert_eq!(result["verification_status"], "verified");
    assert_eq!(
        result["non_transactional_tables"]
            .as_array()
            .map(Vec::len),
        Some(0)
    );
    let cross = result["cross_database_tables"]
        .as_array()
        .expect("cross_database_tables 应为数组");
    assert_eq!(cross.len(), 1, "{result}");
    assert_eq!(cross[0]["schema"], "it_cross_mydb");
    assert_eq!(cross[0]["table"], "it_xdb_innodb");
    assert_eq!(cross[0]["engine"], "InnoDB");
    assert_eq!(cross[0]["transactional"], Value::Bool(true));
    let note = result["engine_note"].as_str().unwrap_or_default();
    assert!(note.contains("跨库表引擎已核实"), "{note}");
    assert!(note.contains("it_cross_mydb.it_xdb_innodb=InnoDB"), "{note}");

    assert_eq!(
        statuses(&result),
        vec!["1:rolled_back", "2:rolled_back", "3:failed"]
    );
    assert_eq!(row_count(&pool, "it_cross_mydb.it_xdb_innodb").await, 0);

    drop_tables(&pool, &["it_cross_mydb.it_xdb_innodb"]).await;
}

/// 受限账号读不到目标库元数据：无法验证 → 降级，并说明是哪条语句、哪个库表、为什么。
#[tokio::test(flavor = "multi_thread")]
async fn mysql_cross_database_unverified_metadata_downgrades() {
    let Some(url) = mysql_url() else { return };
    let (_root_client, root_pool) = connect(&url).await;
    let limit_user = "it_xdb_limited";
    let limit_pw = "limited_pw";
    exec(&root_pool, "CREATE DATABASE IF NOT EXISTS it_cross_mydb").await;
    drop_tables(
        &root_pool,
        &["it_xdb_local", "it_cross_mydb.it_xdb_hidden"],
    )
    .await;
    exec(
        &root_pool,
        "CREATE TABLE it_xdb_local (id INT PRIMARY KEY, note VARCHAR(32)) ENGINE=InnoDB",
    )
    .await;
    exec(
        &root_pool,
        "CREATE TABLE it_cross_mydb.it_xdb_hidden (id INT PRIMARY KEY, note VARCHAR(32)) ENGINE=InnoDB",
    )
    .await;
    // 只授权当前库；目标库的表存在但该账号看不到其元数据。
    exec(
        &root_pool,
        &format!("DROP USER IF EXISTS '{limit_user}'@'%'"),
    )
    .await;
    exec(
        &root_pool,
        &format!("CREATE USER '{limit_user}'@'%' IDENTIFIED BY '{limit_pw}'"),
    )
    .await;
    exec(
        &root_pool,
        &format!("GRANT SELECT, INSERT, UPDATE, DELETE ON `{}`.* TO '{limit_user}'@'%'", current_database(&root_pool).await),
    )
    .await;

    let limited_url = url_with_credentials(&url, limit_user, limit_pw);
    let (limited_client, _limited_pool) = connect(&limited_url).await;

    let result = run_script(
        &limited_client,
        "INSERT INTO it_xdb_local VALUES (1, 'a');
         INSERT INTO it_cross_mydb.it_xdb_hidden VALUES (1, 'h');",
        false,
    )
    .await
    .expect("应返回结构化结果");

    assert_eq!(result["atomic"], Value::Bool(false), "{result}");
    assert_eq!(result["rolled_back"], Value::Bool(false), "{result}");
    assert_eq!(result["rollback_scope"], "transactional_engines_only");
    assert_eq!(result["verification_status"], "downgraded_unverified");
    assert_eq!(
        result["non_transactional_tables"]
            .as_array()
            .map(Vec::len),
        Some(0),
        "查不到引擎不等于非事务引擎，不能混报：{result}"
    );

    let unverified = result["unverified_tables"]
        .as_array()
        .expect("unverified_tables 应为数组");
    assert_eq!(unverified.len(), 1, "{result}");
    assert_eq!(unverified[0]["statement"], 2);
    assert_eq!(unverified[0]["schema"], "it_cross_mydb");
    assert_eq!(unverified[0]["table"], "it_xdb_hidden");
    let reason = unverified[0]["reason"].as_str().unwrap_or_default();
    assert!(
        reason.contains("information_schema.TABLES") && reason.contains("无权"),
        "原因必须说明为什么无法验证：{reason}"
    );
    let note = result["engine_note"].as_str().unwrap_or_default();
    assert!(note.contains("无法核实引擎"), "{note}");
    assert!(note.contains("第 2 条语句"), "{note}");
    assert!(note.contains("it_cross_mydb.it_xdb_hidden"), "{note}");

    // 降级时不能出现 rolled_back=true；语句状态也不允许标 rolled_back。
    assert_eq!(statuses(&result), vec!["1:executed", "2:failed"]);

    exec(
        &root_pool,
        &format!("DROP USER IF EXISTS '{limit_user}'@'%'"),
    )
    .await;
    drop_tables(
        &root_pool,
        &["it_xdb_local", "it_cross_mydb.it_xdb_hidden"],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn mysql_all_innodb_script_keeps_full_rollback_promise() {
    let Some(url) = mysql_url() else { return };
    let (client, pool) = connect(&url).await;
    drop_tables(&pool, &["it_all_innodb"]).await;
    exec(
        &pool,
        "CREATE TABLE it_all_innodb (id INT PRIMARY KEY, note VARCHAR(32)) ENGINE=InnoDB",
    )
    .await;

    let result = run_script(
        &client,
        "INSERT INTO it_all_innodb VALUES (1, 'a');
         INSERT INTO it_all_innodb VALUES (2, 'b');
         INSERT INTO it_all_innodb VALUES (1, 'dup');",
        false,
    )
    .await
    .expect("应返回结构化结果");

    // 对照组：命中的表全部支持事务时，仍给完整承诺。
    assert_eq!(result["atomic"], Value::Bool(true));
    assert_eq!(result["rolled_back"], Value::Bool(true));
    assert_eq!(result["rollback_scope"], Value::Null);
    assert_eq!(result["verification_status"], "verified");
    assert_eq!(
        result["non_transactional_tables"]
            .as_array()
            .map(Vec::len),
        Some(0)
    );
    assert_eq!(
        result["cross_database_tables"]
            .as_array()
            .map(Vec::len),
        Some(0),
        "同库脚本不应产生跨库条目：{result}"
    );
    assert!(
        result["engine_note"]
            .as_str()
            .unwrap_or_default()
            .contains("均支持事务"),
        "{}",
        result["engine_note"]
    );
    assert_eq!(
        statuses(&result),
        vec!["1:rolled_back", "2:rolled_back", "3:failed"]
    );
    assert_eq!(row_count(&pool, "it_all_innodb").await, 0);

    drop_tables(&pool, &["it_all_innodb"]).await;
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
