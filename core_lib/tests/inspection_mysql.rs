//! 真实 MySQL 巡检验证（opt-in）。
//!
//! 运行方式：
//! ```bash
//! INSPECTION_MYSQL_URL='mysql://root:password@127.0.0.1:13306/e2e' \
//!   cargo test -p core_lib --test inspection_mysql -- --nocapture
//! ```
//! 未设置环境变量时自动跳过，不影响常规单测。

use sqlx::mysql::MySqlPool;

use core_lib::db::client::DbClient;
use core_lib::ops::inspection::presets::all_check_ids;
use core_lib::ops::inspection::runner::run_inspection;
use core_lib::ops::inspection::store::InspectionStore;
use core_lib::ops::inspection::types::{
    AlertState, CheckResult, CheckStatus, InspectionSnapshot, InspectionThresholds,
    CHECK_UNUSED_INDEX,
};
use core_lib::ops::types::Severity;
use core_lib::ops::OpsContext;
use serde_json::json;

const DB_ID: &str = "inspection-e2e";

async fn exec(conn: &mut sqlx::pool::PoolConnection<sqlx::MySql>, sql: &str) {
    if let Err(err) = sqlx::query(sql).execute(&mut **conn).await {
        panic!("执行 SQL 失败: {err}\nSQL: {sql}");
    }
}

async fn setup(conn: &mut sqlx::pool::PoolConnection<sqlx::MySql>, db_name: &str) {
    // 清理旧样本，保证重复运行结果稳定。
    exec(conn, "SET FOREIGN_KEY_CHECKS=0").await;
    for table in [
        "inspection_child",
        "inspection_parent",
        "redundant_demo",
        "unused_demo",
        "charset_demo",
        "no_pk",
        "no_pk_second",
    ] {
        exec(conn, &format!("DROP TABLE IF EXISTS {table}")).await;
    }
    exec(conn, "SET FOREIGN_KEY_CHECKS=1").await;

    exec(
        conn,
        "CREATE TABLE no_pk (a INT, b INT) ENGINE=InnoDB",
    )
    .await;
    exec(
        conn,
        "CREATE TABLE redundant_demo (\
           id INT PRIMARY KEY, a INT, b INT, \
           KEY idx_a (a), KEY idx_a_copy (a), KEY idx_ab (a, b)\
         ) ENGINE=InnoDB",
    )
    .await;
    exec(
        conn,
        "CREATE TABLE unused_demo (\
           id INT PRIMARY KEY, payload VARCHAR(64), KEY idx_unused (payload)\
         ) ENGINE=InnoDB",
    )
    .await;
    exec(
        conn,
        "CREATE TABLE charset_demo (id INT PRIMARY KEY, name VARCHAR(32)) \
         ENGINE=InnoDB DEFAULT CHARSET=latin1 COLLATE=latin1_swedish_ci",
    )
    .await;
    exec(
        conn,
        "CREATE TABLE inspection_parent (id INT PRIMARY KEY, name VARCHAR(32)) ENGINE=InnoDB",
    )
    .await;
    exec(
        conn,
        "CREATE TABLE inspection_child (\
           id INT PRIMARY KEY, parent_id INT NULL, \
           CONSTRAINT fk_inspection_child_parent FOREIGN KEY (parent_id) \
             REFERENCES inspection_parent(id)\
         ) ENGINE=InnoDB",
    )
    .await;

    // 绕过外键检查写入一条孤儿行（NULL 行不应被计为孤儿）。
    exec(conn, "SET FOREIGN_KEY_CHECKS=0").await;
    exec(
        conn,
        "INSERT INTO inspection_child VALUES (1, 99), (2, NULL)",
    )
    .await;
    exec(conn, "SET FOREIGN_KEY_CHECKS=1").await;

    // 触发一次表扫描，让 performance_schema 建立该表的索引使用记录。
    exec(conn, "SELECT COUNT(*) FROM unused_demo").await;

    // 账号样本：一个无密码账号、一个高权限账号。
    exec(conn, "DROP USER IF EXISTS 'inspection_nopass'@'%'").await;
    exec(conn, "DROP USER IF EXISTS 'inspection_admin'@'%'").await;
    exec(conn, "DROP USER IF EXISTS 'inspection_limited'@'%'").await;
    exec(conn, "CREATE USER 'inspection_nopass'@'%' IDENTIFIED BY ''").await;
    exec(
        conn,
        "CREATE USER 'inspection_admin'@'%' IDENTIFIED BY 'password'",
    )
    .await;
    exec(
        conn,
        "GRANT ALL PRIVILEGES ON *.* TO 'inspection_admin'@'%' WITH GRANT OPTION",
    )
    .await;

    // 只授予目标库 SELECT：用于验证读取 mysql.user 失败时的优雅降级。
    exec(
        conn,
        "CREATE USER 'inspection_limited'@'%' IDENTIFIED BY 'limited_password'",
    )
    .await;
    exec(
        conn,
        &format!("GRANT SELECT ON `{}`.* TO 'inspection_limited'@'%'", db_name.replace('`', "``")),
    )
    .await;
}

fn with_credentials(url: &str, user: &str, password: &str) -> String {
    let rest = url.strip_prefix("mysql://").unwrap_or(url);
    let after_at = rest.split_once('@').map(|(_, value)| value).unwrap_or(rest);
    format!("mysql://{user}:{password}@{after_at}")
}

async fn cleanup(conn: &mut sqlx::pool::PoolConnection<sqlx::MySql>) {
    exec(conn, "DROP USER IF EXISTS 'inspection_nopass'@'%'").await;
    exec(conn, "DROP USER IF EXISTS 'inspection_admin'@'%'").await;
    exec(conn, "DROP USER IF EXISTS 'inspection_limited'@'%'").await;
    exec(conn, "SET FOREIGN_KEY_CHECKS=0").await;
    for table in [
        "inspection_child",
        "inspection_parent",
        "redundant_demo",
        "unused_demo",
        "charset_demo",
        "no_pk",
        "no_pk_second",
    ] {
        exec(conn, &format!("DROP TABLE IF EXISTS {table}")).await;
    }
    exec(conn, "SET FOREIGN_KEY_CHECKS=1").await;
}

fn find_check<'a>(snapshot: &'a InspectionSnapshot, check_id: &str) -> &'a CheckResult {
    snapshot
        .checks
        .iter()
        .find(|check| check.check_id == check_id)
        .unwrap_or_else(|| panic!("缺少检查项: {check_id}"))
}

fn evidence_keys(check: &CheckResult) -> Vec<String> {
    check
        .evidence
        .iter()
        .map(|item| item.key.clone())
        .collect()
}

fn print_snapshot(snapshot: &InspectionSnapshot) {
    println!(
        "\n=== 巡检快照 {} ({:?}) ===",
        snapshot.snapshot_id, snapshot.status
    );
    for check in &snapshot.checks {
        println!(
            "[{}] {} status={:?} checked_objects={} evidence={}",
            check.check_id,
            check.title,
            check.status,
            check.checked_objects,
            check.evidence.len()
        );
        if let Some(reason) = &check.skipped_reason {
            println!("    skipped_reason: {reason}");
        }
        for item in &check.evidence {
            println!("    - [{:?}] {} :: {}", item.severity, item.key, item.message);
        }
    }
}

async fn print_raw_queries(pool: &MySqlPool) {
    println!("\n=== 实际查询结果 ===");

    let indexes: Vec<(String, String, String, i64)> = sqlx::query_as(
        "SELECT CAST(TABLE_NAME AS CHAR) AS table_name, \
           CAST(INDEX_NAME AS CHAR) AS index_name, \
           CAST(GROUP_CONCAT(COLUMN_NAME ORDER BY SEQ_IN_INDEX) AS CHAR) AS cols, \
           NON_UNIQUE \
         FROM information_schema.STATISTICS \
         WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME IN \
           ('redundant_demo','unused_demo','no_pk','charset_demo', \
            'inspection_parent','inspection_child') \
         GROUP BY TABLE_NAME, INDEX_NAME, NON_UNIQUE \
         ORDER BY TABLE_NAME, INDEX_NAME",
    )
    .fetch_all(pool)
    .await
    .expect("查询 information_schema.STATISTICS 失败");
    println!("information_schema.STATISTICS:");
    for row in indexes {
        println!("  {}.{} cols=({}) non_unique={}", row.0, row.1, row.2, row.3);
    }

    let usage: Vec<(String, String, u64)> = sqlx::query_as(
        "SELECT CAST(OBJECT_NAME AS CHAR) AS object_name, \
           CAST(INDEX_NAME AS CHAR) AS index_name, COUNT_STAR \
         FROM performance_schema.table_io_waits_summary_by_index_usage \
         WHERE OBJECT_SCHEMA = DATABASE() AND INDEX_NAME <> 'PRIMARY' \
         ORDER BY OBJECT_NAME, INDEX_NAME",
    )
    .fetch_all(pool)
    .await
    .expect("查询 performance_schema 索引使用失败");
    println!("performance_schema.table_io_waits_summary_by_index_usage:");
    for row in usage {
        println!("  {}.{} count_star={}", row.0, row.1, row.2);
    }

    let orphan_rows: Vec<(i64, Option<i64>)> = sqlx::query_as(
        "SELECT id, parent_id FROM inspection_child \
         WHERE parent_id IS NOT NULL \
           AND parent_id NOT IN (SELECT id FROM inspection_parent)",
    )
    .fetch_all(pool)
    .await
    .expect("查询孤儿外键失败");
    println!("孤儿外键行 (inspection_child.id, parent_id): {orphan_rows:?}");

    let collations: Vec<(String, String)> = sqlx::query_as(
        "SELECT CAST(TABLE_NAME AS CHAR) AS table_name, \
           CAST(TABLE_COLLATION AS CHAR) AS table_collation FROM information_schema.TABLES \
         WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'charset_demo'",
    )
    .fetch_all(pool)
    .await
    .expect("查询表 collation 失败");
    let default_collation: String =
        sqlx::query_scalar("SELECT CAST(DEFAULT_COLLATION_NAME AS CHAR) FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = DATABASE()")
            .fetch_one(pool)
            .await
            .expect("查询库默认 collation 失败");
    println!("库默认 collation={default_collation}, charset_demo={collations:?}");

    let accounts: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT CAST(User AS CHAR) AS user_name, CAST(Host AS CHAR) AS host_name, \
           CAST(authentication_string AS CHAR) AS auth_string, \
           CAST(Super_priv AS CHAR) AS super_priv FROM mysql.user \
         WHERE User IN ('inspection_nopass','inspection_admin') ORDER BY User",
    )
    .fetch_all(pool)
    .await
    .expect("查询 mysql.user 失败");
    println!("mysql.user 样本 (User, Host, auth_string, Super_priv): {accounts:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn inspection_against_real_mysql() {
    let Ok(url) = std::env::var("INSPECTION_MYSQL_URL") else {
        eprintln!("跳过真实库巡检验证：未设置 INSPECTION_MYSQL_URL");
        return;
    };

    let db_name = DbClient::extract_db_name(&url).unwrap_or_else(|| "e2e".to_string());
    let pool = MySqlPool::connect(&url)
        .await
        .expect("连接 MySQL 失败，请确认 compose 服务已就绪");
    let mut conn = pool.acquire().await.expect("获取 MySQL 连接失败");
    setup(&mut conn, &db_name).await;

    let client = DbClient::new_default(&url)
        .await
        .expect("创建 DbClient 失败");
    let ctx = OpsContext::new(client, DB_ID.into(), db_name.clone(), true);

    let thresholds = InspectionThresholds {
        // 容器 uptime 不足 24 小时，把窗口阈值设为 0 以便真实环境也能判定为 warning。
        unused_index_min_uptime_hours: 0,
        ..InspectionThresholds::default()
    };
    let check_ids = all_check_ids();

    // 第一次巡检。
    let snapshot1 = run_inspection(&ctx, &check_ids, &thresholds, "manual", &[], &[])
        .await
        .expect("第一次巡检失败");
    print_snapshot(&snapshot1);
    print_raw_queries(&pool).await;

    // 逐项断言：规则必须真的识别样本。
    let redundant = find_check(&snapshot1, "redundant_index");
    let redundant_keys = evidence_keys(redundant);
    assert!(
        redundant_keys.iter().any(|key| key == "redundant_demo.idx_a"),
        "冗余索引未识别 idx_a: {redundant_keys:?}"
    );
    assert!(
        redundant_keys.iter().any(|key| key == "redundant_demo.idx_a_copy"),
        "冗余索引未识别 idx_a_copy: {redundant_keys:?}"
    );

    let performance_schema_on = sqlx::query_scalar::<_, i64>("SELECT @@performance_schema")
        .fetch_one(&pool)
        .await
        .unwrap_or(0)
        == 1;

    let unused = find_check(&snapshot1, "unused_index");
    if performance_schema_on {
        assert!(
            evidence_keys(unused)
                .iter()
                .any(|key| key == "unused_demo.idx_unused"),
            "未使用索引未识别: {:?}",
            evidence_keys(unused)
        );
    } else {
        // MariaDB 默认关闭 performance_schema：必须显式降级并说明原因。
        assert_eq!(unused.status, CheckStatus::Skipped);
        assert!(
            unused
                .skipped_reason
                .as_deref()
                .unwrap_or_default()
                .contains("performance_schema 未开启"),
            "禁用 performance_schema 时应给出可读降级原因: {:?}",
            unused.skipped_reason
        );
    }

    let missing_pk = find_check(&snapshot1, "missing_primary_key");
    assert!(
        evidence_keys(missing_pk)
            .iter()
            .any(|key| key == "table:no_pk"),
        "无主键表未识别: {:?}",
        evidence_keys(missing_pk)
    );

    let charset = find_check(&snapshot1, "charset_collation_mismatch");
    assert!(
        evidence_keys(charset)
            .iter()
            .any(|key| key == "table:charset_demo"),
        "字符集/排序规则不一致未识别: {:?}",
        evidence_keys(charset)
    );

    let orphan = find_check(&snapshot1, "orphan_foreign_key");
    assert!(
        evidence_keys(orphan)
            .iter()
            .any(|key| key == "constraint:inspection_child.fk_inspection_child_parent"),
        "孤儿外键未识别: {:?}",
        evidence_keys(orphan)
    );

    let over = find_check(&snapshot1, "over_privileged_account");
    assert!(
        evidence_keys(over)
            .iter()
            .any(|key| key == "inspection_admin@%"),
        "高权限账号未识别: {:?}",
        evidence_keys(over)
    );

    let nopass = find_check(&snapshot1, "passwordless_account");
    assert!(
        evidence_keys(nopass)
            .iter()
            .any(|key| key == "inspection_nopass@%"),
        "无密码账号未识别: {:?}",
        evidence_keys(nopass)
    );

    let inactive = find_check(&snapshot1, "inactive_account");
    assert_eq!(inactive.status, CheckStatus::Skipped);
    let inactive_reason = inactive.skipped_reason.as_deref().unwrap_or_default();
    if performance_schema_on {
        assert!(
            inactive_reason.contains("低于"),
            "长期未登录检查应说明统计窗口不足: {:?}",
            inactive.skipped_reason
        );
    } else {
        // MariaDB 默认关闭 performance_schema：必须显式说明降级原因，而不是静默跳过。
        assert!(
            inactive_reason.contains("performance_schema 未开启"),
            "performance_schema 关闭时应给出可读降级原因: {:?}",
            inactive.skipped_reason
        );
    }

    // 快照落库 + 重新打开文件确认持久化。
    let path = std::env::temp_dir().join(format!(
        "inspection_mysql_{}_{}.json",
        std::process::id(),
        chrono::Local::now().timestamp_millis()
    ));
    let first = {
        let mut store = InspectionStore::new(Some(path.clone()));
        store
            .record_snapshot(snapshot1.clone())
            .expect("第一次快照落库失败")
    };
    println!(
        "\n第一次 diff: added={} resolved={} changed={}",
        first.diff.added.len(),
        first.diff.resolved.len(),
        first.diff.changed.len()
    );
    drop(first);

    let reopened = InspectionStore::new(Some(path.clone()));
    let persisted = reopened
        .latest_for_db_id(DB_ID)
        .expect("重新打开快照文件后找不到最近快照");
    assert_eq!(persisted.snapshot_id, snapshot1.snapshot_id);

    // 改变样本：删除一个冗余索引（应消失），新增一个无主键表（应新增）。
    exec(&mut conn, "ALTER TABLE redundant_demo DROP INDEX idx_a_copy").await;
    exec(&mut conn, "CREATE TABLE no_pk_second (x INT) ENGINE=InnoDB").await;

    let snapshot2 = run_inspection(&ctx, &check_ids, &thresholds, "manual", &[], &[])
        .await
        .expect("第二次巡检失败");
    print_snapshot(&snapshot2);

    let mut store = reopened;
    let second = store
        .record_snapshot(snapshot2.clone())
        .expect("第二次快照落库失败");
    println!(
        "\n第二次 diff: added={} resolved={} changed={}",
        second.diff.added.len(),
        second.diff.resolved.len(),
        second.diff.changed.len()
    );
    println!(
        "diff JSON:\n{}",
        serde_json::to_string_pretty(&second.diff).expect("序列化 diff 失败")
    );

    assert!(
        second
            .diff
            .resolved
            .iter()
            .any(|item| item.evidence_key == "redundant_demo.idx_a_copy"),
        "第二次 diff 未报告消失的索引 idx_a_copy: {:?}",
        second.diff.resolved
    );
    assert!(
        second
            .diff
            .added
            .iter()
            .any(|item| item.check_id == "missing_primary_key"
                && item.evidence_key == "table:no_pk_second"),
        "第二次 diff 未报告新增的无主键表 no_pk_second: {:?}",
        second.diff.added
    );

    // 第二次快照也应持久化。
    let reopened_again = InspectionStore::new(Some(path.clone()));
    assert!(reopened_again.latest_for_db_id(DB_ID).is_some());

    // 权限不足：只授予目标库 SELECT 的账号读不到 mysql.user，检查必须显式跳过并说明原因。
    let limited_url = with_credentials(&url, "inspection_limited", "limited_password");
    let limited_client = DbClient::new_default(&limited_url)
        .await
        .expect("创建受限账号连接失败");
    let limited_ctx = OpsContext::new(
        limited_client,
        "inspection-limited".into(),
        db_name.clone(),
        true,
    );
    let limited_checks = vec![
        "over_privileged_account".to_string(),
        "passwordless_account".to_string(),
        "inactive_account".to_string(),
    ];
    let limited_snapshot = run_inspection(&limited_ctx, &limited_checks, &thresholds, "limited", &[], &[])
        .await
        .expect("受限账号巡检失败");
    println!("\n=== 受限账号巡检（mysql.user 不可读）===");
    print_snapshot(&limited_snapshot);
    for check in &limited_snapshot.checks {
        assert_eq!(
            check.status,
            CheckStatus::Skipped,
            "权限不足时 {} 应显式跳过",
            check.check_id
        );
        assert!(
            check
                .skipped_reason
                .as_deref()
                .unwrap_or_default()
                .contains("mysql.user"),
            "{} 的跳过原因应说明 mysql.user 不可读: {:?}",
            check.check_id,
            check.skipped_reason
        );
    }

    // ── 未使用索引判定窗口真库验证 ─────────────────────────────
    // MariaDB 默认关闭 performance_schema 时无法读取索引计数，
    // 这里只验证“显式降级”，窗口连续性需要 P_S 可用才能验证。
    if !performance_schema_on {
        let degraded = run_inspection(
            &ctx,
            &[CHECK_UNUSED_INDEX.to_string()],
            &thresholds,
            "degraded",
            &[],
            &[],
        )
        .await
        .expect("降级巡检失败");
        let unused = find_check(&degraded, CHECK_UNUSED_INDEX);
        assert_eq!(unused.status, CheckStatus::Skipped);
        assert!(
            unused
                .skipped_reason
                .as_deref()
                .unwrap_or_default()
                .contains("performance_schema 未开启"),
            "禁用 performance_schema 时应给出可读降级原因: {:?}",
            unused.skipped_reason
        );
        cleanup(&mut conn).await;
        drop(conn);
        let _ = std::fs::remove_file(path);
        let _ = pool.close().await;
        println!("\nMariaDB（performance_schema 关闭）巡检降级验证通过。");
        return;
    }

    // 用 TRUNCATE P_S 计数器模拟“所有索引 COUNT_STAR 归零”，验证：
    // 第 1/2 次只记 info，第 3 次升级 warning 并告警；uptime 回退视为重启，重置窗口且不 resolve 告警；
    // 真正使用索引（COUNT_STAR>0）后才 resolve。
    exec(&mut conn, "DROP TABLE IF EXISTS unused_window_demo").await;
    exec(
        &mut conn,
        "CREATE TABLE unused_window_demo (id INT PRIMARY KEY, v INT, KEY idx_never_used (v)) ENGINE=InnoDB",
    )
    .await;
    exec(
        &mut conn,
        "TRUNCATE performance_schema.table_io_waits_summary_by_index_usage",
    )
    .await;
    // 打开表以让 P_S 生成索引行（不走二级索引）。
    exec(&mut conn, "SELECT COUNT(*) FROM unused_window_demo").await;
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    let window_key = "unused_window_demo.idx_never_used";
    let window_path = std::env::temp_dir().join(format!(
        "inspection_window_{}_{}.json",
        std::process::id(),
        chrono::Local::now().timestamp_millis()
    ));
    let mut window_store = InspectionStore::new(Some(window_path.clone()));
    let window_thresholds = InspectionThresholds {
        unused_index_min_uptime_hours: 0,
        unused_index_min_snapshots: 3,
        ..InspectionThresholds::default()
    };
    let window_checks = vec![CHECK_UNUSED_INDEX.to_string()];

    for round in 1..=3 {
        let history = window_store.snapshots_for_fingerprint(&ctx.db_fingerprint, 30);
        let active =
            window_store.active_alert_evidence_keys(&ctx.db_fingerprint, CHECK_UNUSED_INDEX);
        let snapshot = run_inspection(
            &ctx,
            &window_checks,
            &window_thresholds,
            "window",
            &history,
            &active,
        )
        .await
        .expect("窗口巡检失败");
        print_snapshot(&snapshot);
        let unused = find_check(&snapshot, CHECK_UNUSED_INDEX);
        let item = unused
            .evidence
            .iter()
            .find(|item| item.key == window_key)
            .unwrap_or_else(|| panic!("缺少未使用索引证据: {window_key}；{:?}", evidence_keys(unused)));
        assert_eq!(item.fields["consecutive_zero_snapshots"], round);
        assert_eq!(item.fields["required_snapshots"], 3);
        assert_eq!(
            item.severity,
            if round >= 3 {
                Severity::Warn
            } else {
                Severity::Info
            },
            "第 {round} 次快照的严重级别"
        );
        window_store
            .record_snapshot(snapshot)
            .expect("窗口快照落库失败");
        let active_after =
            window_store.active_alert_evidence_keys(&ctx.db_fingerprint, CHECK_UNUSED_INDEX);
        assert_eq!(
            active_after.iter().any(|key| key == window_key),
            round >= 3,
            "第 {round} 次快照后的告警状态"
        );
    }

    // 模拟实例重启：把最近一份快照记录的 uptime 抬高到当前 uptime 之上。
    let uptime_seconds: u64 = sqlx::query_scalar(
        "SELECT CAST(VARIABLE_VALUE AS UNSIGNED) FROM performance_schema.global_status \
         WHERE VARIABLE_NAME = 'Uptime'",
    )
    .fetch_one(&mut *conn)
    .await
    .expect("读取 Uptime 失败");
    let current_uptime_hours = uptime_seconds / 3600;
    let mut restart_history = window_store.snapshots_for_fingerprint(&ctx.db_fingerprint, 30);
    {
        let latest = restart_history.last_mut().expect("缺少窗口历史快照");
        let check = latest
            .checks
            .iter_mut()
            .find(|check| check.check_id == CHECK_UNUSED_INDEX)
            .expect("最近快照缺少 unused_index 检查");
        for item in check.evidence.iter_mut() {
            item.fields["uptime_hours"] = json!(current_uptime_hours + 24);
        }
    }
    let active = window_store.active_alert_evidence_keys(&ctx.db_fingerprint, CHECK_UNUSED_INDEX);
    let snapshot = run_inspection(
        &ctx,
        &window_checks,
        &window_thresholds,
        "window",
        &restart_history,
        &active,
    )
    .await
    .expect("重启窗口巡检失败");
    print_snapshot(&snapshot);
    let unused = find_check(&snapshot, CHECK_UNUSED_INDEX);
    let item = unused
        .evidence
        .iter()
        .find(|item| item.key == window_key)
        .expect("重启后缺少未使用索引证据");
    assert_eq!(item.severity, Severity::Info);
    assert_eq!(item.fields["window_reset"], true);
    assert_eq!(item.fields["window_reset_reason"], "instance_restart");
    assert_eq!(item.fields["consecutive_zero_snapshots"], 0);
    window_store
        .record_snapshot(snapshot)
        .expect("重启快照落库失败");
    let active = window_store.active_alert_evidence_keys(&ctx.db_fingerprint, CHECK_UNUSED_INDEX);
    assert!(
        active.iter().any(|key| key == window_key),
        "重启导致窗口重置时不能 resolve 已有告警"
    );

    // 真正使用该索引后应产生 COUNT_STAR>0 证据，并恢复告警。
    exec(
        &mut conn,
        "SELECT v FROM unused_window_demo FORCE INDEX (idx_never_used) WHERE v = 1",
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    let history = window_store.snapshots_for_fingerprint(&ctx.db_fingerprint, 30);
    let active = window_store.active_alert_evidence_keys(&ctx.db_fingerprint, CHECK_UNUSED_INDEX);
    let snapshot = run_inspection(
        &ctx,
        &window_checks,
        &window_thresholds,
        "window",
        &history,
        &active,
    )
    .await
    .expect("索引使用后巡检失败");
    print_snapshot(&snapshot);
    let unused = find_check(&snapshot, CHECK_UNUSED_INDEX);
    let item = unused
        .evidence
        .iter()
        .find(|item| item.key == window_key)
        .expect("索引被使用后缺少恢复证据");
    assert_eq!(item.fields["used"], true);
    assert!(
        item.fields["count_star"].as_u64().unwrap_or(0) > 0,
        "恢复证据必须带回真实 COUNT_STAR: {:?}",
        item.fields
    );
    window_store
        .record_snapshot(snapshot)
        .expect("恢复快照落库失败");
    let alerts = window_store.alerts(Some(&ctx.db_fingerprint), 50);
    let target_alert = alerts
        .iter()
        .find(|alert| alert.evidence_key == window_key)
        .expect("缺少未使用索引告警记录");
    assert_eq!(target_alert.state, AlertState::Resolved);
    println!("\n未使用索引判定窗口真库验证通过。");

    exec(&mut conn, "DROP TABLE IF EXISTS unused_window_demo").await;
    let _ = std::fs::remove_file(window_path);

    cleanup(&mut conn).await;
    drop(conn);
    let _ = std::fs::remove_file(path);
    let _ = pool.close().await;

    println!("\n真实库巡检验证通过。");
}
