//! 真实 PostgreSQL 性能诊断验证（opt-in）。
//!
//! 使用独立临时容器（预加载 pg_stat_statements、单独端口），不触碰 phase3-pg：
//!
//! ```bash
//! docker run -d --rm --name perf-pg-stats \
//!   -p 127.0.0.1:55433:5432 \
//!   -e POSTGRES_PASSWORD=password -e POSTGRES_DB=perf_pg \
//!   postgres:16 -c shared_preload_libraries=pg_stat_statements
//!
//! 注意：不要在启动参数里加 `-c track_counts=on`。命令行参数的优先级高于
//! ALTER SYSTEM，会导致 track_counts 降级用例无法切换而失败。
//!
//! PERF_PG_URL='postgres://postgres:password@127.0.0.1:55433/perf_pg' \
//!   cargo test -p core_lib --test perf_diagnostics_pg -- --nocapture --test-threads=1
//! ```
//!
//! 未设置环境变量时自动跳过，不影响常规单测。每个用例使用独立 schema，结束后清理。

use std::sync::atomic::{AtomicU64, Ordering};

use sqlx::postgres::PgPool;
use sqlx::Row;

use core_lib::db::client::DbClient;
use core_lib::ops::index_advisor::{self, IndexAdvisorInput};
use core_lib::ops::perf_diagnostics::{self, PerfDiagnosticsInput};
use core_lib::ops::policy::OpsPolicy;
use core_lib::ops::registry::{OpsContext, ScenarioRegistry};

const TABLE: &str = "perf_pg_orders";
const TARGET_SQL: &str =
    "SELECT id, status FROM perf_pg_orders WHERE user_id = 12345 AND status = 'paid'";

static SEQ: AtomicU64 = AtomicU64::new(0);

fn base_url() -> Option<String> {
    std::env::var("PERF_PG_URL")
        .ok()
        .filter(|url| !url.trim().is_empty())
}

fn unique_suffix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "{}_{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed) + nanos as u64 % 1_000_000
    )
}

fn database_name(url: &str) -> String {
    let tail = url.rsplit('/').next().unwrap_or("");
    tail.split('?').next().unwrap_or("").to_string()
}

fn with_search_path(url: &str, schema: &str) -> String {
    let separator = if url.contains('?') { '&' } else { '?' };
    // 保留 public：pg_stat_statements 扩展默认安装在 public，正常连接也依赖它。
    format!("{url}{separator}options=-csearch_path%3D{schema}%2Cpublic")
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

struct Fixture {
    schema: String,
    admin: PgPool,
    client_url: String,
    db_name: String,
}

impl Fixture {
    async fn setup(url: &str) -> Self {
        let schema = format!("perf_pg_{}", unique_suffix());
        let admin = PgPool::connect(url)
            .await
            .expect("连接 PostgreSQL 失败，请确认独立 pg_stat_statements 容器已就绪");
        let mut conn = admin.acquire().await.expect("获取管理连接失败");

        let statements = vec![
            format!("DROP SCHEMA IF EXISTS {schema} CASCADE"),
            "CREATE EXTENSION IF NOT EXISTS pg_stat_statements".to_string(),
            format!("CREATE SCHEMA {schema}"),
            format!("SET search_path TO {schema}, public"),
            format!(
                "CREATE TABLE {TABLE} (\
                   id bigint PRIMARY KEY, \
                   user_id int NOT NULL, \
                   status text NOT NULL, \
                   amount numeric(10,2) NOT NULL, \
                   created_at timestamptz NOT NULL\
                 )"
            ),
            format!(
                "INSERT INTO {TABLE} (id, user_id, status, amount, created_at) \
                 SELECT g, (g % 1000) + 1, CASE WHEN g % 3 = 0 THEN 'paid' ELSE 'pending' END, \
                        (g % 100)::numeric / 4, now() - (g || ' minutes')::interval \
                 FROM generate_series(1, 5000) g"
            ),
            format!("ANALYZE {TABLE}"),
        ];
        for sql in statements {
            if let Err(err) = sqlx::query(&sql).execute(&mut *conn).await {
                panic!("执行管理 SQL 失败: {err}\nSQL: {sql}");
            }
        }

        // 只保留目标语句的统计，避免 setup 语句干扰候选匹配。
        if let Err(err) = sqlx::query("SELECT pg_stat_statements_reset()")
            .execute(&mut *conn)
            .await
        {
            panic!("重置 pg_stat_statements 失败: {err}");
        }
        for _ in 0..3 {
            if let Err(err) = sqlx::raw_sql(TARGET_SQL).fetch_all(&mut *conn).await {
                panic!("执行目标 SQL 失败: {err}");
            }
        }
        drop(conn);

        Fixture {
            client_url: with_search_path(url, &schema),
            db_name: database_name(url),
            schema,
            admin,
        }
    }

    async fn cleanup(self) {
        let mut conn = self.admin.acquire().await.expect("获取清理连接失败");
        let sql = format!("DROP SCHEMA IF EXISTS {} CASCADE", self.schema);
        if let Err(err) = sqlx::query(&sql).execute(&mut *conn).await {
            eprintln!("清理失败: {err}\nSQL: {sql}");
            panic!("清理失败: {err}");
        }
    }
}

async fn context(fixture: &Fixture) -> OpsContext {
    let client = DbClient::new_default(&fixture.client_url)
        .await
        .expect("创建 PG 连接池失败");
    OpsContext::new(
        client,
        "perf-pg-e2e".into(),
        fixture.db_name.clone(),
        false,
    )
}

#[tokio::test]
async fn perf_pg_slow_query_list_reads_pg_stat_statements() {
    let Some(url) = base_url() else {
        eprintln!("PERF_PG_URL 未设置，跳过 PG 慢查询列表验证");
        return;
    };
    let fixture = Fixture::setup(&url).await;
    let ctx = context(&fixture).await;

    for sort in ["total_latency", "avg_latency", "rows_examined"] {
        let scan = perf_diagnostics::list_slow_queries(&ctx, sort, 20)
            .await
            .expect("读取 PG 慢查询失败");
        println!("\n=== PG 慢查询（sort={sort}）===");
        println!(
            "available={} degraded={:?}",
            scan.available, scan.degraded_reasons
        );
        println!(
            "统计窗口: {}",
            scan.window
                .as_ref()
                .map(|w| w.note.clone())
                .unwrap_or_else(|| "-".into())
        );
        assert!(
            scan.available,
            "pg_stat_statements 应可用: {:?}",
            scan.degraded_reasons
        );
        let window = scan.window.as_ref().expect("缺少统计窗口");
        assert!(window.cumulative, "PG 统计是累计值");
        assert!(
            window.source.contains("server_uptime"),
            "窗口来源应包含实例运行时长: {}",
            window.source
        );
        assert!(
            scan.degraded_reasons
                .iter()
                .any(|r| r.starts_with("rows_examined_unavailable")),
            "必须显式声明扫描行不可用: {:?}",
            scan.degraded_reasons
        );
        let candidate = scan
            .candidates
            .iter()
            .find(|c| c.digest_text.contains(TABLE))
            .unwrap_or_else(|| panic!("未找到目标语句候选: {scan:?}"));
        println!("候选: {}", candidate.summary);
        assert!(candidate.count_star >= 3, "目标语句应至少执行 3 次");
        assert_eq!(candidate.rows_examined_sum, 0, "不得用近似值顶替扫描行");
    }
    fixture.cleanup().await;
}

#[tokio::test]
async fn perf_pg_digest_only_requires_actual_sql() {
    let Some(url) = base_url() else {
        eprintln!("PERF_PG_URL 未设置，跳过 PG digest 定向验证");
        return;
    };
    let fixture = Fixture::setup(&url).await;
    let ctx = context(&fixture).await;
    let scan = perf_diagnostics::list_slow_queries(&ctx, "total_latency", 20)
        .await
        .expect("读取 PG 慢查询失败");
    let digest = scan
        .candidates
        .iter()
        .find(|c| c.digest_text.contains(TABLE))
        .map(|c| c.digest.clone())
        .expect("未找到目标 digest");

    let registry = ScenarioRegistry::builtin();
    let scenario = registry.find("perf-diagnostics").expect("场景未注册");
    let input = serde_json::to_value(PerfDiagnosticsInput {
        digest: Some(digest),
        ..Default::default()
    })
    .unwrap();
    let err = scenario
        .collect_evidence(&ctx, &input)
        .await
        .expect_err("PG digest 定向诊断应要求实际 SQL");
    let message = err.to_string();
    println!("\n=== digest 定向降级 ===\n{message}");
    assert!(
        message.contains("粘贴实际 SQL"),
        "错误需说明 PG 只有 $1 规范化 SQL: {message}"
    );
    fixture.cleanup().await;
}

#[tokio::test]
async fn perf_pg_explain_and_readonly_plan() {
    let Some(url) = base_url() else {
        eprintln!("PERF_PG_URL 未设置，跳过 PG 证据与计划验证");
        return;
    };
    let fixture = Fixture::setup(&url).await;
    let ctx = context(&fixture).await;
    let registry = ScenarioRegistry::builtin();
    let scenario = registry.find("perf-diagnostics").expect("场景未注册");

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
        .expect("采集 PG 证据失败");
    let perf = evidence.perf.as_ref().expect("缺少 perf 证据");
    println!("\n=== PG 证据 ===");
    println!(
        "行数估算: value={} source={} trusted={} reason={}",
        perf.row_estimate.as_ref().map(|r| r.value).unwrap_or(-1),
        perf.row_estimate
            .as_ref()
            .map(|r| r.source.as_str())
            .unwrap_or("-"),
        perf.row_estimate
            .as_ref()
            .map(|r| r.trusted)
            .unwrap_or(false),
        perf.row_estimate
            .as_ref()
            .map(|r| r.reason.clone())
            .unwrap_or_default(),
    );
    for sample in &perf.explain {
        println!("  {}", sample.summary());
    }
    println!("实际耗时: {:?} ms", perf.actual_latency_ms);
    println!("降级原因: {:?}", perf.degraded_reasons);
    println!("notes: {:?}", perf.notes);

    assert!(
        !perf.explain.is_empty(),
        "PG EXPLAIN 应产出样本: {:?}",
        perf.degraded_reasons
    );
    assert!(
        perf.explain
            .iter()
            .any(|s| s.table_name.as_deref() == Some(TABLE)),
        "EXPLAIN 样本应包含目标表: {:?}",
        perf.explain
    );
    assert!(
        perf.explain
            .iter()
            .any(|s| s.access_type.as_deref() == Some("ALL")),
        "无索引等值查询应表现为顺序扫描"
    );
    assert!(
        perf.actual_latency_ms.is_some(),
        "PG EXPLAIN ANALYZE 应成功: {:?}",
        perf.degraded_reasons
    );
    let estimate = perf.row_estimate.as_ref().expect("缺少行数估算");
    assert!(estimate.trusted, "ANALYZE 后 reltuples 应可信: {estimate:?}");
    assert!(
        estimate.value >= 5000,
        "reltuples 应接近真实行数: {estimate:?}"
    );
    assert!(
        perf.notes.iter().any(|n| n.contains("未做隐式类型转换判定")),
        "PG 应显式说明不做隐式转换判定"
    );

    // 只读标记：前端据此禁用 apply。
    let extra = evidence.extra.as_ref().expect("缺少只读标记");
    assert_eq!(
        extra.get("read_only").and_then(|v| v.as_bool()),
        Some(true)
    );

    let policy = OpsPolicy {
        allow_apply: true,
        ..OpsPolicy::default()
    };
    let plan = scenario
        .build_plan(&ctx.identity(), &evidence, &input, &policy)
        .expect("生成 PG 只读方案失败");
    println!("\n=== PG 只读方案 ===\n{}", plan.diagnosis);
    assert!(plan.actions.is_empty(), "PG 方案不得包含可执行动作");
    assert!(!plan.requires_approval, "PG 只读方案不应进入审批");
    assert!(!plan.verification_explain_analyze);
    assert!(plan.diagnosis.contains("PostgreSQL 只读"));
    assert!(plan
        .code_advice
        .iter()
        .any(|advice| advice.contains("不代执行")));
    fixture.cleanup().await;
}

#[tokio::test]
async fn perf_pg_degrades_when_pg_stat_statements_not_preloaded() {
    let Ok(url) = std::env::var("PERF_PG_PLAIN_URL") else {
        eprintln!("PERF_PG_PLAIN_URL 未设置，跳过未预加载降级验证");
        return;
    };
    let client = DbClient::new_default(&url)
        .await
        .expect("创建 PG 连接池失败");
    let ctx = OpsContext::new(client, "perf-pg-plain".into(), database_name(&url), false);

    let scan = perf_diagnostics::list_slow_queries(&ctx, "total_latency", 10)
        .await
        .expect("读取慢查询不应报错，只应降级");
    println!("\n=== 未预加载 pg_stat_statements ===\n{:?}", scan.degraded_reasons);
    assert!(!scan.available, "未预加载时必须降级");
    assert!(
        scan.degraded_reasons
            .iter()
            .any(|r| r.starts_with("pg_stat_statements_not_preloaded")),
        "降级原因必须点名未预加载: {:?}",
        scan.degraded_reasons
    );
    assert!(
        scan.candidates.is_empty(),
        "降级时不得返回候选冒充可用数据"
    );
    assert!(
        scan.notes.iter().any(|n| n.contains("不代表没有慢查询")),
        "必须说明空结果不代表没有慢查询: {:?}",
        scan.notes
    );
}

#[tokio::test]
async fn perf_pg_degrades_when_extension_missing() {
    let Some(url) = base_url() else {
        eprintln!("PERF_PG_URL 未设置，跳过扩展缺失降级验证");
        return;
    };
    // 已预加载，但 postgres 库未 CREATE EXTENSION。
    let url = with_database(&url, "postgres");
    let client = DbClient::new_default(&url)
        .await
        .expect("创建 PG 连接池失败");
    let ctx = OpsContext::new(client, "perf-pg-noext".into(), "postgres".into(), false);

    let scan = perf_diagnostics::list_slow_queries(&ctx, "total_latency", 10)
        .await
        .expect("读取慢查询不应报错，只应降级");
    println!("\n=== 扩展缺失 ===\n{:?}", scan.degraded_reasons);
    assert!(!scan.available, "扩展缺失时必须降级");
    assert!(
        scan.degraded_reasons
            .iter()
            .any(|r| r.starts_with("pg_stat_statements_extension_missing")),
        "降级原因必须点名扩展缺失: {:?}",
        scan.degraded_reasons
    );
    assert!(scan.candidates.is_empty());
    assert!(scan.notes.iter().any(|n| n.contains("CREATE EXTENSION")));
}

#[tokio::test]
async fn index_advisor_pg_notes_track_counts_off() {
    let Some(url) = base_url() else {
        eprintln!("PERF_PG_URL 未设置，跳过 PG track_counts 降级验证");
        return;
    };
    let fixture = Fixture::setup(&url).await;
    let mut conn = fixture.admin.acquire().await.expect("获取管理连接失败");
    let original: String = sqlx::query("SHOW track_counts")
        .fetch_one(&mut *conn)
        .await
        .expect("读取 track_counts 失败")
        .get(0);
    sqlx::query("ALTER SYSTEM SET track_counts = off")
        .execute(&mut *conn)
        .await
        .expect("关闭 track_counts 失败");
    sqlx::query("SELECT pg_reload_conf()")
        .execute(&mut *conn)
        .await
        .expect("重载配置失败");
    let mut toggled = false;
    for _ in 0..30 {
        let value: String = sqlx::query("SHOW track_counts")
            .fetch_one(&mut *conn)
            .await
            .expect("读取 track_counts 失败")
            .get(0);
        if value.eq_ignore_ascii_case("off") {
            toggled = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    let ctx = context(&fixture).await;
    let evidence = index_advisor::collect_evidence(
        &ctx,
        IndexAdvisorInput {
            table: TABLE.into(),
            workload_sql: vec![TARGET_SQL.into()],
            notes: None,
            use_llm: false,
        },
    )
    .await
    .expect("采集证据失败");
    println!("\n=== track_counts=off 索引优化 notes ===\n{:?}", evidence.notes);

    sqlx::query(&format!("ALTER SYSTEM SET track_counts = {original}"))
        .execute(&mut *conn)
        .await
        .expect("还原 track_counts 失败");
    sqlx::query("SELECT pg_reload_conf()")
        .execute(&mut *conn)
        .await
        .expect("重载配置失败");
    for _ in 0..30 {
        let value: String = sqlx::query("SHOW track_counts")
            .fetch_one(&mut *conn)
            .await
            .expect("读取 track_counts 失败")
            .get(0);
        if value.eq_ignore_ascii_case(&original) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    assert!(toggled, "track_counts 未成功切换为 off");
    assert!(
        evidence.notes.iter().any(|note| note.contains("track_counts=off")),
        "索引优化必须显式说明 track_counts 降级: {:?}",
        evidence.notes
    );
    fixture.cleanup().await;
}

#[tokio::test]
async fn index_advisor_pg_is_read_only() {
    let Some(url) = base_url() else {
        eprintln!("PERF_PG_URL 未设置，跳过 PG 索引优化只读验证");
        return;
    };
    let fixture = Fixture::setup(&url).await;
    let ctx = context(&fixture).await;

    let input = IndexAdvisorInput {
        table: TABLE.into(),
        workload_sql: vec![TARGET_SQL.into()],
        notes: None,
        use_llm: false,
    };
    let evidence = index_advisor::collect_evidence(&ctx, input.clone())
        .await
        .expect("PG 索引优化采集证据失败");
    println!("\n=== PG 索引优化证据 ===");
    println!("notes: {:?}", evidence.notes);
    for sample in &evidence.explain {
        println!("  {}", sample.summary());
    }
    assert!(
        !evidence.explain.is_empty(),
        "PG 索引优化应有 EXPLAIN 样本"
    );
    let extra = evidence.extra.as_ref().expect("缺少只读标记");
    assert_eq!(
        extra.get("read_only").and_then(|v| v.as_bool()),
        Some(true)
    );

    let policy = OpsPolicy {
        allow_apply: true,
        ..OpsPolicy::default()
    };
    let plan = index_advisor::build_plan(
        &ctx.identity(),
        &evidence,
        input.clone(),
        &policy,
    )
    .expect("生成 PG 索引只读方案失败");
    println!("\n=== PG 索引只读方案 ===\n{}", plan.diagnosis);
    assert!(plan.actions.is_empty(), "PG 不得生成可执行 DDL");
    assert!(!plan.requires_approval);
    assert!(!plan.verification_explain_analyze);
    assert!(plan.diagnosis.contains("只读"));
    fixture.cleanup().await;
}
