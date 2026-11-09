//! 真实 MySQL 性能诊断验证（opt-in）。
//!
//! 运行方式：
//! ```bash
//! PERF_MYSQL_URL='mysql://root:password@127.0.0.1:13306/e2e' \
//!   cargo test -p core_lib --test perf_diagnostics_mysql -- --nocapture
//! ```
//! 未设置环境变量时自动跳过，不影响常规单测。

use std::sync::Arc;

use sqlx::mysql::MySqlPool;

use core_lib::db::client::DbClient;
use core_lib::ops::approval::ApprovalStore;
use core_lib::ops::audit::AuditLog;
use core_lib::ops::executor;
use core_lib::ops::perf_diagnostics::{self, PerfDiagnosticsInput};
use core_lib::ops::policy::OpsPolicy;
use core_lib::ops::registry::OpsContext;
use core_lib::ops::types::ActionSpec;

const DB_ID: &str = "perf-e2e";
const TABLE: &str = "perf_orders";
const TARGET_SQL: &str = "SELECT id, status FROM perf_orders WHERE user_id = 12345 AND status = 'paid'";

async fn exec(conn: &mut sqlx::pool::PoolConnection<sqlx::MySql>, sql: &str) {
    if let Err(err) = sqlx::query(sql).execute(&mut **conn).await {
        panic!("执行 SQL 失败: {err}\nSQL: {sql}");
    }
}

async fn setup(conn: &mut sqlx::pool::PoolConnection<sqlx::MySql>) {
    exec(conn, &format!("DROP TABLE IF EXISTS {TABLE}")).await;
    exec(
        conn,
        &format!(
            "CREATE TABLE {TABLE} (\
               id INT NOT NULL AUTO_INCREMENT, \
               user_id INT NOT NULL, \
               status VARCHAR(16) NOT NULL, \
               amount DECIMAL(10,2) NOT NULL, \
               created_at DATETIME NOT NULL, \
               payload VARCHAR(64), \
               PRIMARY KEY (id)\
             ) ENGINE=InnoDB"
        ),
    )
    .await;

    let digits = "(SELECT 0 AS n UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 \
                  UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 \
                  UNION ALL SELECT 8 UNION ALL SELECT 9)";
    exec(
        conn,
        &format!(
            "INSERT INTO {TABLE} (user_id, status, amount, created_at, payload) \
             SELECT ((a.n * 10000 + b.n * 1000 + c.n * 100 + d.n * 10 + e.n) % 1000) + 1, \
                    CASE WHEN (a.n + e.n) % 3 = 0 THEN 'paid' ELSE 'pending' END, \
                    (a.n * 10 + b.n) * 1.25, \
                    TIMESTAMPADD(MINUTE, a.n * 10000 + b.n * 1000 + c.n * 100 + d.n * 10 + e.n, '2026-01-01 00:00:00'), \
                    REPEAT('x', 20) \
             FROM {digits} a CROSS JOIN {digits} b CROSS JOIN {digits} c \
             CROSS JOIN {digits} d CROSS JOIN {digits} e"
        ),
    )
    .await;
    exec(conn, &format!("ANALYZE TABLE {TABLE}")).await;
    exec(
        conn,
        "TRUNCATE performance_schema.events_statements_summary_by_digest",
    )
    .await;
}

async fn run_target_once(conn: &mut sqlx::pool::PoolConnection<sqlx::MySql>) {
    // 使用文本协议执行，确保语句进入 performance_schema digest（二进制预处理协议不产生语句摘要行）。
    let _ = sqlx::raw_sql(TARGET_SQL)
        .fetch_all(&mut **conn)
        .await
        .expect("执行目标慢查询失败");
}

#[tokio::test]
async fn perf_explain_analyze_timeout_degrades_and_audits() {
    let Ok(url) = std::env::var("PERF_MYSQL_URL") else {
        eprintln!("跳过 EXPLAIN ANALYZE 超时验证：未设置 PERF_MYSQL_URL");
        return;
    };
    let db_name = DbClient::extract_db_name(&url).unwrap_or_else(|| "e2e".to_string());
    let pool = MySqlPool::connect(&url)
        .await
        .expect("连接 MySQL 失败");
    let mut conn = pool.acquire().await.expect("获取 MySQL 连接失败");
    exec(&mut conn, "DROP TABLE IF EXISTS perf_timeout_demo").await;
    exec(
        &mut conn,
        "CREATE TABLE perf_timeout_demo (id INT PRIMARY KEY AUTO_INCREMENT, amount DECIMAL(10,2))",
    )
    .await;
    exec(
        &mut conn,
        "INSERT INTO perf_timeout_demo (amount) SELECT 1.0 FROM information_schema.columns LIMIT 1000",
    )
    .await;
    exec(&mut conn, "ANALYZE TABLE perf_timeout_demo").await;

    let audit_path = std::env::temp_dir().join(format!(
        "perf-e2e-timeout-audit-{}.jsonl",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&audit_path);
    let audit = Arc::new(AuditLog::new(Some(audit_path.clone())));
    let client = DbClient::new_default(&url)
        .await
        .expect("创建 DbClient 失败");
    let mariadb = matches!(client.db_type, core_lib::config::DbType::MariaDB);
    let ctx = OpsContext::new(client, DB_ID.into(), db_name.clone(), false)
        .with_audit(Some(audit.clone()), Some("perf-timeout-session".into()));
    let registry = core_lib::ops::registry::ScenarioRegistry::builtin();
    let scenario = registry.find("perf-diagnostics").expect("场景未注册");

    let input = serde_json::to_value(PerfDiagnosticsInput {
        sql: Some("SELECT SUM(amount) FROM perf_timeout_demo WHERE SLEEP(1) = 0".into()),
        explain_analyze: true,
        explain_analyze_timeout_ms: Some(500),
        ..Default::default()
    })
    .unwrap();
    let evidence = scenario
        .collect_evidence(&ctx, &input)
        .await
        .expect("采集证据失败");
    let perf = evidence.perf.as_ref().expect("缺少 perf 证据");
    println!("\n=== 超时降级 ===");
    println!("degraded={:?}", perf.degraded_reasons);
    println!("notes={:?}", perf.notes);
    if mariadb {
        // MariaDB 不支持 EXPLAIN ANALYZE：必须显式降级，不产生实际耗时，也不伪造超时审计。
        assert!(
            perf
                .degraded_reasons
                .iter()
                .any(|r| r.starts_with("explain_analyze_unsupported")),
            "MariaDB 应显式声明不支持 EXPLAIN ANALYZE: {:?}",
            perf.degraded_reasons
        );
        assert!(perf.actual_latency_ms.is_none());
        assert!(perf
            .notes
            .iter()
            .any(|n| n.contains("MariaDB 不支持 EXPLAIN ANALYZE")));
        let records = audit.recent(20).expect("读取审计失败");
        assert!(
            !records
                .iter()
                .any(|r| r.event == "perf.explain_analyze" && r.outcome == "timeout"),
            "MariaDB 不应产生 EXPLAIN ANALYZE 超时审计"
        );
    } else {
        assert!(
            perf
                .degraded_reasons
                .iter()
                .any(|r| r.starts_with("explain_analyze_timeout")),
            "应按超时降级: {:?}",
            perf.degraded_reasons
        );
        assert!(perf.actual_latency_ms.is_none());
        assert!(perf.notes.iter().any(|n| n.contains("max_execution_time")));

        let records = audit.recent(20).expect("读取审计失败");
        let timeout: Vec<_> = records
            .iter()
            .filter(|r| r.event == "perf.explain_analyze" && r.outcome == "timeout")
            .collect();
        println!("超时审计条数: {}", timeout.len());
        for record in &timeout {
            println!("audit: {}", record.detail);
        }
        assert!(
            timeout
                .iter()
                .any(|r| r.detail["session_id"] == "perf-timeout-session"
                    && r.detail["timeout_ms"] == 500),
            "审计缺少超时记录"
        );
    }
    exec(&mut conn, "DROP TABLE IF EXISTS perf_timeout_demo").await;
    let _ = std::fs::remove_file(&audit_path);
}

#[tokio::test]
async fn perf_diagnostics_real_mysql_flow() {
    let Ok(url) = std::env::var("PERF_MYSQL_URL") else {
        eprintln!("跳过真实库性能诊断验证：未设置 PERF_MYSQL_URL");
        return;
    };

    let db_name = DbClient::extract_db_name(&url).unwrap_or_else(|| "e2e".to_string());
    let pool = MySqlPool::connect(&url)
        .await
        .expect("连接 MySQL 失败，请确认 compose 服务已就绪");
    let mut conn = pool.acquire().await.expect("获取 MySQL 连接失败");
    setup(&mut conn).await;
    run_target_once(&mut conn).await;
    let digest_hits: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM performance_schema.events_statements_summary_by_digest \
         WHERE DIGEST_TEXT LIKE '%perf_orders%'",
    )
    .fetch_one(&mut *conn)
    .await
    .expect("统计 digest 失败");
    eprintln!("目标语句 digest 命中数: {digest_hits}");

    let audit_path = std::env::temp_dir().join(format!("perf-e2e-audit-{}.jsonl", std::process::id()));
    let _ = std::fs::remove_file(&audit_path);
    let audit = Arc::new(AuditLog::new(Some(audit_path.clone())));
    let client = DbClient::new_default(&url)
        .await
        .expect("创建 DbClient 失败");
    let mariadb = matches!(client.db_type, core_lib::config::DbType::MariaDB);
    let ctx = OpsContext::new(client, DB_ID.into(), db_name.clone(), false)
        .with_audit(Some(audit.clone()), Some("perf-e2e-session".into()));

    let registry = core_lib::ops::registry::ScenarioRegistry::builtin();
    let scenario = registry.find("perf-diagnostics").expect("场景未注册");

    // 1) 慢查询列表：可读、有统计窗口、候选里有目标 digest。
    let scan = perf_diagnostics::list_slow_queries(&ctx, "total_latency", 20)
        .await
        .expect("读取慢查询失败");
    println!("\n=== P_S 慢查询列表 ===");
    println!("available={} degraded={:?}", scan.available, scan.degraded_reasons);
    println!(
        "统计窗口: {}",
        scan.window.as_ref().map(|w| w.note.clone()).unwrap_or_else(|| "-".into())
    );
    if mariadb {
        // MariaDB 默认关闭 performance_schema：必须显式降级并给出可读原因。
        assert!(
            !scan.available,
            "MariaDB 未开启 performance_schema 时应降级: {:?}",
            scan.degraded_reasons
        );
        assert!(
            scan
                .degraded_reasons
                .iter()
                .any(|r| r.starts_with("performance_schema_disabled")),
            "降级原因应点名 performance_schema: {:?}",
            scan.degraded_reasons
        );
    } else {
        let candidate = scan
            .candidates
            .iter()
            .find(|c| c.digest_text.contains(TABLE))
            .expect("未在 P_S 候选中找到目标语句");
        println!("候选摘要: {}", candidate.summary);
        assert!(scan.available, "performance_schema 应可用: {:?}", scan.degraded_reasons);
        assert!(scan.window.as_ref().map(|w| w.cumulative).unwrap_or(false));
        assert!(candidate.scan_flag.unwrap_or(false), "P_S 应标记疑似扫描: {candidate:?}");
    }

    // 2) 证据采集（真实执行 EXPLAIN ANALYZE）。
    let input = serde_json::to_value(PerfDiagnosticsInput {
        sql: Some(TARGET_SQL.into()),
        explain_analyze: true,
        explain_analyze_timeout_ms: Some(5000),
        ..Default::default()
    })
    .unwrap();
    let evidence = scenario
        .collect_evidence(&ctx, &input)
        .await
        .expect("采集证据失败");
    let perf = evidence.perf.as_ref().expect("缺少 perf 证据");
    println!("\n=== 证据 ===");
    println!("统计窗口: {}", perf.window.as_ref().map(|w| w.note.clone()).unwrap_or_else(|| "-".into()));
    println!(
        "行数估算: value={} source={} trusted={} reason={}",
        perf.row_estimate.as_ref().map(|r| r.value).unwrap_or(-1),
        perf.row_estimate.as_ref().map(|r| r.source.as_str()).unwrap_or("-"),
        perf.row_estimate.as_ref().map(|r| r.trusted).unwrap_or(false),
        perf.row_estimate.as_ref().map(|r| r.reason.clone()).unwrap_or_default(),
    );
    println!("EXPLAIN 样本:");
    for sample in &perf.explain {
        println!("  {}", sample.summary());
    }
    println!("实际耗时: {:?} ms", perf.actual_latency_ms);
    println!("降级原因: {:?}", perf.degraded_reasons);
    if mariadb {
        // MariaDB 不支持 EXPLAIN ANALYZE：必须声明降级，且不能伪造实际耗时。
        assert!(
            perf
                .degraded_reasons
                .iter()
                .any(|r| r.starts_with("explain_analyze_unsupported")),
            "MariaDB 应显式声明不支持 EXPLAIN ANALYZE: {:?}",
            perf.degraded_reasons
        );
        assert!(
            perf.actual_latency_ms.is_none(),
            "MariaDB 不应产生 EXPLAIN ANALYZE 实际耗时"
        );
    } else {
        assert!(perf.actual_latency_ms.is_some(), "EXPLAIN ANALYZE 应成功: {:?}", perf.degraded_reasons);
    }
    assert!(perf.row_estimate.as_ref().map(|r| r.trusted).unwrap_or(false));

    // 3) 方案：必须只有 CreateIndex/AnalyzeTable。
    let policy = OpsPolicy {
        allow_apply: true,
        ..OpsPolicy::default()
    };
    let plan = scenario
        .build_plan(&ctx.identity(), &evidence, &input, &policy)
        .expect("生成方案失败");
    println!("\n=== 方案 ===");
    println!("diagnosis: {}", plan.diagnosis);
    for action in &plan.actions {
        println!("action: {:?}", action);
    }
    for suggestion in &plan.rewrite_suggestions {
        println!("rewrite: {} | {}", suggestion.title, suggestion.suggestion);
    }
    assert!(
        plan.actions
            .iter()
            .any(|a| matches!(a, ActionSpec::CreateIndex { .. })),
        "可信大表应生成 CreateIndex"
    );
    assert!(plan
        .actions
        .iter()
        .all(|a| matches!(a, ActionSpec::CreateIndex { .. } | ActionSpec::AnalyzeTable { .. })));

    let action_ids: Vec<String> = plan.actions.iter().map(|a| a.action_id()).collect();

    if mariadb {
        // MariaDB 没有 EXPLAIN ANALYZE，复测与审计均无法按 MySQL 口径断言；
        // 这里只要求降级是显式的，不能出现伪造的“已执行”审计。
        let records = audit.recent(50).expect("读取审计失败");
        assert!(
            !records
                .iter()
                .any(|r| r.event == "perf.explain_analyze" && r.outcome == "executed"),
            "MariaDB 不应产生 EXPLAIN ANALYZE 已执行审计"
        );
        exec(&mut conn, &format!("DROP TABLE IF EXISTS {TABLE}")).await;
        let _ = std::fs::remove_file(&audit_path);
        return;
    }

    // 4) dry-run → 审批 → 执行 → 复测。
    let dry = executor::dry_run(&ctx, &plan, &action_ids, &policy, &audit)
        .await
        .expect("dry-run 失败");
    println!("\nDry-run SQL: {:?}", dry.compiled_sql);

    let mut approvals = ApprovalStore::new(None);
    let (token, _) = approvals
        .issue(&plan, &action_ids, 600, Some("perf-e2e".into()))
        .expect("签发审批令牌失败");
    let report = executor::execute(
        &ctx,
        &plan,
        &action_ids,
        &mut approvals,
        &token,
        &policy,
        &audit,
    )
    .await
    .expect("执行失败");
    let verification = report.verification.as_ref().expect("缺少复测结果");
    println!("\n=== 复测 ===");
    println!("summary: {}", verification.summary);
    for item in &verification.comparisons {
        println!(
            "{}: key {} -> {} | rows {:?} -> {:?} | cost {:?} -> {:?} | actual {:?} -> {:?} | {}",
            item.label,
            item.before_key.as_deref().unwrap_or("-"),
            item.after_key.as_deref().unwrap_or("-"),
            item.before_rows,
            item.after_rows,
            item.before_cost,
            item.after_cost,
            item.before_actual_ms,
            item.after_actual_ms,
            item.verdict
        );
    }
    assert!(
        verification
            .comparisons
            .iter()
            .any(|c| c.improved && c.after_key.is_some()),
        "建索引后应命中索引并改善: {:?}",
        verification.comparisons
    );

    // 5) 审计必须包含真实 EXPLAIN ANALYZE 记录。
    let records = audit.recent(50).expect("读取审计失败");
    let analyze_records: Vec<_> = records
        .iter()
        .filter(|r| r.event == "perf.explain_analyze")
        .collect();
    println!("\nEXPLAIN ANALYZE 审计条数: {}", analyze_records.len());
    for record in &analyze_records {
        println!("audit: outcome={} detail={}", record.outcome, record.detail);
    }
    assert!(
        analyze_records
            .iter()
            .any(|r| r.outcome == "executed" && r.detail["session_id"] == "perf-e2e-session"),
        "审计缺少成功的 EXPLAIN ANALYZE 记录"
    );

    // 6) 回滚。
    let rollback = executor::rollback(&ctx, &plan, "e2e 验证完成后回滚", &policy, &audit)
        .await
        .expect("回滚失败");
    assert!(!rollback.compiled_sql.is_empty());
    let remaining = sqlx::query(&format!(
        "SELECT COUNT(*) AS n FROM information_schema.STATISTICS \
         WHERE TABLE_SCHEMA = '{db_name}' AND TABLE_NAME = '{TABLE}' AND INDEX_NAME LIKE 'idx_%'"
    ))
    .fetch_one(&pool)
    .await
    .expect("查询索引失败");
    use sqlx::Row;
    let n: i64 = remaining.get("n");
    assert_eq!(n, 0, "回滚后不应残留新增索引");
    println!("\n回滚 SQL: {:?}", rollback.compiled_sql);

    exec(&mut conn, &format!("DROP TABLE IF EXISTS {TABLE}")).await;
    let _ = std::fs::remove_file(&audit_path);
}
