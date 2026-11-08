//! 性能诊断场景：慢 SQL → 证据 → 结构化计划。
//!
//! 约束：
//! - 执行动作仍只有 `CreateIndex` / `AnalyzeTable`，全部走 `ddl_compiler` 与既有审批链路；
//! - SQL 改写只输出 `RewriteSuggestion` 文本，永不转换为 `ActionSpec`；
//! - 首期只支持 MySQL/MariaDB。

use serde::{Deserialize, Serialize};
use sqlparser::ast::{
    BinaryOperator, Expr, JoinConstraint, JoinOperator, ObjectName, ObjectNamePart, Query,
    SetExpr, Statement, TableFactor, Value,
};
use sqlparser::dialect::MySqlDialect;
use sqlparser::parser::Parser;
use std::collections::{HashMap, HashSet};

use crate::db::protocol::BoxFuture;
use crate::ops::error::OpsError;
use crate::ops::index_advisor::extract_candidate_columns;
use crate::ops::mysql_rows::{
    convert_mysql_rows, fetch_rows, float_value, int_value, str_value, uint_value, RowMap,
};
use crate::ops::registry::{
    OpsContext, OpsScenario, PlanIdentity, ScenarioDescriptor, ScenarioStatus,
};
use crate::ops::types::{
    ActionSpec, AuditRecord, ColumnSnapshot, DigestInfo, EvidencePack, EvidenceSummary,
    ExplainSample, Finding, ImplicitCastFinding, OpsPlan, PerfEvidence, PerfTableEvidence,
    PerfWindowInfo, PlanStatus, RewriteSuggestion, RiskClass, RiskFinding, RollbackPlan,
    RowEstimateEvidence, Severity, StatsFreshness, TableSnapshot, TableStats, ValidationCheck,
    ValidationStatus,
};
use crate::sql::util::{escape_sql_string, is_read_only_statement, validate_identifier};

const MAX_SQL_LEN: usize = 5000;
const SLOW_QUERY_LIMIT_DEFAULT: usize = 20;
const SLOW_QUERY_LIMIT_MAX: usize = 100;
const STATS_STALE_DAYS: i64 = 7;
const MAX_INDEX_ACTIONS: usize = 3;
const MAX_SCAN_ROWS_ALERT: i64 = 10_000;
const INDEX_MIN_TRUSTED_ROWS: i64 = 1000;
const DEFAULT_ANALYZE_TIMEOUT_MS: u64 = 5000;
const MAX_ANALYZE_TIMEOUT_MS: u64 = 60_000;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PerfDiagnosticsInput {
    #[serde(default)]
    pub sql: Option<String>,
    /// performance_schema digest（64 位 hex）；仅用于关联指标与样本。
    #[serde(default)]
    pub digest: Option<String>,
    /// 从 digest 关联到的可执行 SQL 样本。
    #[serde(default)]
    pub sample_sql: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    /// 是否执行 EXPLAIN ANALYZE（只读 SQL 才会真正执行）。
    #[serde(default)]
    pub explain_analyze: bool,
    /// EXPLAIN ANALYZE 的会话超时（毫秒），默认 5000。
    #[serde(default)]
    pub explain_analyze_timeout_ms: Option<u64>,
    #[serde(default)]
    pub use_llm: bool,
}

impl PerfDiagnosticsInput {
    fn normalized(mut self) -> Self {
        self.sql = normalize_sql_option(self.sql);
        self.sample_sql = normalize_sql_option(self.sample_sql);
        self.digest = normalize_digest(self.digest);
        self.notes = self
            .notes
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        self.explain_analyze_timeout_ms = Some(
            self.explain_analyze_timeout_ms
                .unwrap_or(DEFAULT_ANALYZE_TIMEOUT_MS)
                .clamp(500, MAX_ANALYZE_TIMEOUT_MS),
        );
        self
    }

    fn analyze_timeout_ms(&self) -> u64 {
        self.explain_analyze_timeout_ms
            .unwrap_or(DEFAULT_ANALYZE_TIMEOUT_MS)
            .clamp(500, MAX_ANALYZE_TIMEOUT_MS)
    }
}

fn normalize_sql_option(sql: Option<String>) -> Option<String> {
    sql.map(|s| s.trim().trim_end_matches(';').trim().to_string())
        .filter(|s| !s.is_empty())
}

fn normalize_digest(digest: Option<String>) -> Option<String> {
    let digest = digest?.trim().to_ascii_lowercase();
    if digest.len() < 16 || digest.len() > 64 || !digest.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(digest)
}

// ── 慢查询候选 ─────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SlowQueryCandidate {
    pub digest: String,
    pub digest_text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_sql: Option<String>,
    pub count_star: u64,
    pub total_latency_ms: f64,
    pub avg_latency_ms: f64,
    pub rows_examined_sum: u64,
    pub rows_examined_avg: u64,
    pub rows_sent_avg: u64,
    pub rows_affected_avg: u64,
    pub created_tmp_tables: u64,
    pub created_tmp_disk_tables: u64,
    pub sort_rows: u64,
    /// P_S 统计的疑似全表扫描标记；列不可用时为 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scan_flag: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_index_used: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub select_scan: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub select_full_join: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_seen: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<String>,
    pub summary: String,
}

impl SlowQueryCandidate {
    fn from_row(row: &RowMap) -> Option<Self> {
        let digest = str_value(row, "digest")?;
        let digest_text = str_value(row, "digest_text").unwrap_or_default();
        let count_star = uint_value(row, "count_star").unwrap_or(0);
        let sum_timer = uint_value(row, "sum_timer_wait").unwrap_or(0);
        let avg_timer = uint_value(row, "avg_timer_wait").unwrap_or(0);
        let rows_examined_sum = uint_value(row, "sum_rows_examined").unwrap_or(0);
        let rows_examined_avg = if count_star > 0 {
            rows_examined_sum / count_star
        } else {
            0
        };
        let rows_sent_avg = uint_value(row, "sum_rows_sent")
            .map(|v| if count_star > 0 { v / count_star } else { 0 })
            .unwrap_or(0);
        let rows_affected_avg = uint_value(row, "sum_rows_affected")
            .map(|v| if count_star > 0 { v / count_star } else { 0 })
            .unwrap_or(0);
        let total_latency_ms = sum_timer as f64 / 1_000_000_000.0;
        let avg_latency_ms = avg_timer as f64 / 1_000_000_000.0;
        let created_tmp_tables = uint_value(row, "sum_created_tmp_tables").unwrap_or(0);
        let created_tmp_disk_tables =
            uint_value(row, "sum_created_tmp_disk_tables").unwrap_or(0);
        let sort_rows = uint_value(row, "sum_sort_rows").unwrap_or(0);
        let no_index_used = uint_value(row, "sum_no_index_used");
        let select_scan = uint_value(row, "sum_select_scan");
        let select_full_join = uint_value(row, "sum_select_full_join");
        let scan_flag = match (no_index_used, select_scan, select_full_join) {
            (Some(no_index), Some(scan), Some(full_join)) => {
                Some(no_index > 0 || scan > 0 || full_join > 0)
            }
            _ => None,
        };
        let first_seen = str_value(row, "first_seen");
        let last_seen = str_value(row, "last_seen");
        let summary = format!(
            "{} | 执行 {} 次 | 总 {:.1}ms / 平均 {:.2}ms | 扫描 {}/次 | 返回 {}/次 | 临时表 {}/磁盘 {} | 排序行 {} | 最近 {}",
            truncate(&digest_text, 160),
            count_star,
            total_latency_ms,
            avg_latency_ms,
            rows_examined_avg,
            rows_sent_avg,
            created_tmp_tables,
            created_tmp_disk_tables,
            sort_rows,
            last_seen.as_deref().unwrap_or("-")
        );
        Some(Self {
            digest,
            digest_text,
            schema_name: str_value(row, "schema_name"),
            sample_sql: None,
            count_star,
            total_latency_ms,
            avg_latency_ms,
            rows_examined_sum,
            rows_examined_avg,
            rows_sent_avg,
            rows_affected_avg,
            created_tmp_tables,
            created_tmp_disk_tables,
            sort_rows,
            scan_flag,
            no_index_used,
            select_scan,
            select_full_join,
            first_seen,
            last_seen,
            summary,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SlowQueryScan {
    /// 数据源是否可信可用；false 时 candidates 一定为空，不能当作“没有慢查询”。
    pub available: bool,
    pub degraded_reasons: Vec<String>,
    pub sort: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<PerfWindowInfo>,
    pub candidates: Vec<SlowQueryCandidate>,
    pub notes: Vec<String>,
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

fn slow_query_order(sort: &str) -> &'static str {
    match sort.to_ascii_lowercase().as_str() {
        "avg_latency" => "AVG_TIMER_WAIT DESC",
        "rows_examined" => "SUM_ROWS_EXAMINED DESC",
        _ => "SUM_TIMER_WAIT DESC",
    }
}

/// 从 performance_schema 读取慢查询摘要。P_S 未开启/无权限时返回 available=false
/// 并附带原因；空结果也会显式说明“不代表没有慢查询”。
pub async fn list_slow_queries(
    ctx: &OpsContext,
    sort: &str,
    limit: usize,
) -> Result<SlowQueryScan, OpsError> {
    if !ctx.is_mysql_family() {
        return Err(OpsError::Validation(
            "首期仅支持 MySQL/MariaDB 的 performance_schema".into(),
        ));
    }
    validate_identifier(&ctx.db_name)
        .map_err(|e| OpsError::Validation(format!("数据库名非法: {e}")))?;
    let sort = sort.to_ascii_lowercase();
    let limit = if limit == 0 {
        SLOW_QUERY_LIMIT_DEFAULT
    } else {
        limit
    }
    .clamp(1, SLOW_QUERY_LIMIT_MAX);
    let mut scan = SlowQueryScan {
        available: false,
        degraded_reasons: vec![],
        sort: sort.clone(),
        window: None,
        candidates: vec![],
        notes: vec![],
    };

    let ps_enabled = match fetch_rows(ctx, "SELECT @@performance_schema AS enabled").await {
        Ok(rows) => rows
            .first()
            .and_then(|r| str_value(r, "enabled"))
            .map(|v| v == "1" || v.eq_ignore_ascii_case("ON"))
            .unwrap_or(false),
        Err(err) => {
            scan.degraded_reasons.push(format!(
                "performance_schema_disabled: 无法读取 @@performance_schema（{err}）"
            ));
            scan.notes
                .push("这不代表没有慢查询；请检查 performance_schema 是否开启及当前账号权限".into());
            return Ok(scan);
        }
    };
    if !ps_enabled {
        scan.degraded_reasons.push(
            "performance_schema_disabled: performance_schema 未开启，无法读取慢查询摘要".into(),
        );
        scan.notes
            .push("这不代表没有慢查询；开启 performance_schema 后重新读取".into());
        return Ok(scan);
    }

    let window = load_perf_window(ctx, &ctx.db_name).await;
    scan.notes.push(window.note.clone());
    scan.window = Some(window);

    match fetch_rows(
        ctx,
        "SELECT ENABLED FROM performance_schema.setup_consumers WHERE NAME = 'statements_digest'",
    )
    .await
    {
        Ok(rows) => {
            let enabled = rows
                .first()
                .and_then(|r| str_value(r, "enabled"))
                .map(|v| v.eq_ignore_ascii_case("YES") || v.eq_ignore_ascii_case("ON") || v == "1");
            if enabled == Some(false) {
                scan.degraded_reasons.push(
                    "digest_consumer_disabled: statements_digest consumer 未启用，digest 表不会更新"
                        .into(),
                );
                scan.notes
                    .push("这不代表没有慢查询；启用 statements_digest consumer 后重新读取".into());
                return Ok(scan);
            }
        }
        Err(err) => scan.notes.push(format!(
            "无法读取 setup_consumers（{err}），继续尝试 digest 表"
        )),
    }

    let db = escape_sql_string(&ctx.db_name);
    let order = slow_query_order(&sort);
    let base_select = "SELECT SCHEMA_NAME, DIGEST, DIGEST_TEXT, COUNT_STAR, SUM_TIMER_WAIT, \
                AVG_TIMER_WAIT, SUM_ROWS_EXAMINED, SUM_ROWS_SENT, SUM_ROWS_AFFECTED, \
                SUM_CREATED_TMP_TABLES, SUM_CREATED_TMP_DISK_TABLES, SUM_SORT_ROWS";
    let scan_select = ", SUM_NO_INDEX_USED, SUM_SELECT_SCAN, SUM_SELECT_FULL_JOIN";
    let tail = format!(
        "FROM performance_schema.events_statements_summary_by_digest \
         WHERE SCHEMA_NAME = '{db}' AND DIGEST_TEXT IS NOT NULL AND DIGEST_TEXT <> '' AND COUNT_STAR > 0 \
         AND DIGEST_TEXT NOT LIKE 'SHOW %' AND DIGEST_TEXT NOT LIKE 'EXPLAIN %' \
         AND DIGEST_TEXT NOT LIKE 'SET %' AND DIGEST_TEXT NOT LIKE 'USE %' \
         AND DIGEST_TEXT NOT LIKE 'SELECT @@%' \
         AND DIGEST_TEXT NOT LIKE 'SELECT * FROM performance_schema.%' \
         AND DIGEST_TEXT NOT LIKE 'SELECT * FROM information_schema.%' \
         ORDER BY {order}, COUNT_STAR DESC, LAST_SEEN DESC LIMIT {limit}"
    );
    let sql_with_scan = format!(
        "{base_select}{scan_select}, CAST(FIRST_SEEN AS CHAR) AS first_seen, \
         CAST(LAST_SEEN AS CHAR) AS last_seen {tail}"
    );
    let sql_without_scan = format!(
        "{base_select}, CAST(FIRST_SEEN AS CHAR) AS first_seen, \
         CAST(LAST_SEEN AS CHAR) AS last_seen {tail}"
    );
    let rows = match fetch_rows(ctx, &sql_with_scan).await {
        Ok(rows) => rows,
        Err(first_err) => match fetch_rows(ctx, &sql_without_scan).await {
            Ok(rows) => {
                scan.degraded_reasons.push(format!(
                    "ps_scan_flags_unavailable: SUM_NO_INDEX_USED/SUM_SELECT_SCAN/SUM_SELECT_FULL_JOIN 不可用（{first_err}），列表不回填扫描标记"
                ));
                rows
            }
            Err(err) => {
                scan.degraded_reasons.push(format!(
                    "permission_denied: 读取 performance_schema.events_statements_summary_by_digest 失败（{err}）"
                ));
                scan.notes.push(
                    "这不代表没有慢查询；请检查当前账号对 performance_schema 的 SELECT 权限".into(),
                );
                return Ok(scan);
            }
        },
    };

    scan.available = true;
    if rows.is_empty() {
        scan.degraded_reasons.push(
            "empty: performance_schema 已启用且 consumer 已开启，但当前库暂无 digest 记录".into(),
        );
        scan.notes.push(
            "空 digest 不代表没有慢查询；可能刚启动、统计已清空、语句尚未进入 digest，或语句通过服务端预处理协议执行而未被 digest 记录".into(),
        );
        return Ok(scan);
    }

    let mut candidates: Vec<SlowQueryCandidate> =
        rows.iter().filter_map(SlowQueryCandidate::from_row).collect();
    let mut any_sample = false;
    for candidate in &mut candidates {
        candidate.sample_sql = lookup_sample_sql(ctx, &candidate.digest, &ctx.db_name).await;
        any_sample |= candidate.sample_sql.is_some();
    }
    if !any_sample {
        scan.notes.push(
            "未从 events_statements_history(_long) 回查到 SQL_TEXT；选择 digest 后需要粘贴实际 SQL（digest_text 中的 ? 不能直接执行）".into(),
        );
    }
    scan.candidates = candidates;
    Ok(scan)
}

async fn load_perf_window(ctx: &OpsContext, schema: &str) -> PerfWindowInfo {
    let uptime_seconds = fetch_rows(
        ctx,
        "SELECT VARIABLE_VALUE AS uptime FROM performance_schema.global_status WHERE VARIABLE_NAME = 'Uptime'",
    )
    .await
    .ok()
    .and_then(|rows| rows.first().and_then(|r| str_value(r, "uptime")))
    .and_then(|v| v.parse::<i64>().ok());
    let first_seen = {
        let db = escape_sql_string(schema);
        let sql = format!(
            "SELECT CAST(MIN(FIRST_SEEN) AS CHAR) AS first_seen \
             FROM performance_schema.events_statements_summary_by_digest \
             WHERE SCHEMA_NAME = '{db}'"
        );
        fetch_rows(ctx, &sql)
            .await
            .ok()
            .and_then(|rows| rows.first().and_then(|r| str_value(r, "first_seen")))
            .and_then(|v| parse_mysql_datetime(&v))
    };

    let now = chrono::Local::now().naive_local();
    let server_start = uptime_seconds
        .filter(|s| *s >= 0)
        .and_then(|s| now.checked_sub_signed(chrono::Duration::seconds(s)));
    let start = match (server_start, first_seen) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    };
    let source = match (server_start.is_some(), first_seen.is_some()) {
        (true, true) => "server_uptime+digest_first_seen",
        (true, false) => "server_uptime",
        (false, true) => "digest_first_seen",
        (false, false) => "unknown",
    };
    let window_hours = start.map(|s| (now - s).num_seconds().max(0) as f64 / 3600.0);
    let note = match (start, window_hours) {
        (Some(start), Some(hours)) => format!(
            "统计窗口：P_S 已运行 {hours:.1} 小时（起点 {}，来源 {source}）；延迟与扫描数为累计值，不是最近该时长的增量。",
            start.format("%Y-%m-%d %H:%M:%S")
        ),
        _ => "统计窗口不可知（未能取得 Uptime / FIRST_SEEN）；延迟与扫描数为累计值。".to_string(),
    };
    PerfWindowInfo {
        window_start: start.map(|s| s.format("%Y-%m-%d %H:%M:%S").to_string()),
        window_hours,
        source: source.into(),
        cumulative: true,
        note,
    }
}

async fn lookup_sample_sql(ctx: &OpsContext, digest: &str, schema: &str) -> Option<String> {
    let digest = normalize_digest(Some(digest.to_string()))?;
    let schema = escape_sql_string(schema);
    for table in [
        "performance_schema.events_statements_history_long",
        "performance_schema.events_statements_history",
    ] {
        let sql = format!(
            "SELECT SQL_TEXT FROM {table} WHERE DIGEST = '{digest}' AND SCHEMA_NAME = '{schema}' \
             ORDER BY TIMER_WAIT DESC LIMIT 1"
        );
        if let Ok(rows) = fetch_rows(ctx, &sql).await {
            if let Some(text) = rows.first().and_then(|r| str_value(r, "sql_text")) {
                return Some(text);
            }
        }
    }
    None
}

async fn load_digest_info(ctx: &OpsContext, digest: &str) -> Option<DigestInfo> {
    let digest = normalize_digest(Some(digest.to_string()))?;
    let db = escape_sql_string(&ctx.db_name);
    for filter in [format!("AND SCHEMA_NAME = '{db}'"), String::new()] {
        let sql = format!(
            "SELECT SCHEMA_NAME, DIGEST, DIGEST_TEXT, COUNT_STAR, SUM_TIMER_WAIT, AVG_TIMER_WAIT, \
                    SUM_ROWS_EXAMINED, SUM_ROWS_SENT, SUM_ROWS_AFFECTED, \
                    SUM_CREATED_TMP_TABLES, SUM_CREATED_TMP_DISK_TABLES, SUM_SORT_ROWS, \
                    CAST(FIRST_SEEN AS CHAR) AS first_seen, CAST(LAST_SEEN AS CHAR) AS last_seen \
             FROM performance_schema.events_statements_summary_by_digest \
             WHERE DIGEST = '{digest}' {filter} LIMIT 1"
        );
        if let Ok(rows) = fetch_rows(ctx, &sql).await {
            if let Some(row) = rows.first() {
                let count_star = uint_value(row, "count_star").unwrap_or(0);
                return Some(DigestInfo {
                    digest: str_value(row, "digest").unwrap_or(digest.clone()),
                    digest_text: str_value(row, "digest_text").unwrap_or_default(),
                    schema_name: str_value(row, "schema_name"),
                    count_star,
                    total_latency_ms: uint_value(row, "sum_timer_wait").unwrap_or(0) as f64
                        / 1_000_000_000.0,
                    avg_latency_ms: uint_value(row, "avg_timer_wait").unwrap_or(0) as f64
                        / 1_000_000_000.0,
                    rows_examined_sum: uint_value(row, "sum_rows_examined").unwrap_or(0),
                    rows_examined_avg: if count_star > 0 {
                        uint_value(row, "sum_rows_examined").unwrap_or(0) / count_star
                    } else {
                        0
                    },
                    rows_sent_avg: uint_value(row, "sum_rows_sent").map(|v| {
                        if count_star > 0 {
                            v / count_star
                        } else {
                            0
                        }
                    }).unwrap_or(0),
                    rows_affected_avg: uint_value(row, "sum_rows_affected").map(|v| {
                        if count_star > 0 {
                            v / count_star
                        } else {
                            0
                        }
                    }).unwrap_or(0),
                    created_tmp_tables: uint_value(row, "sum_created_tmp_tables").unwrap_or(0),
                    created_tmp_disk_tables: uint_value(row,
                        "sum_created_tmp_disk_tables").unwrap_or(0),
                    sort_rows: uint_value(row, "sum_sort_rows").unwrap_or(0),
                    first_seen: str_value(row, "first_seen"),
                    last_seen: str_value(row, "last_seen"),
                });
            }
        }
    }
    None
}

// ── 场景 ───────────────────────────────────────────────────

pub struct PerfDiagnosticsScenario {
    descriptor: ScenarioDescriptor,
}

impl PerfDiagnosticsScenario {
    pub fn new() -> Self {
        Self {
            descriptor: ScenarioDescriptor {
                id: "perf-diagnostics".into(),
                title: "性能诊断（慢 SQL → 方案）".into(),
                description:
                    "对一条 SQL 采集 EXPLAIN/索引/统计证据，生成可审批执行的索引或 ANALYZE 动作；SQL 改写仅文本建议。"
                        .into(),
                engines: vec!["mysql".into(), "mariadb".into()],
                status: ScenarioStatus::Enabled,
                risk_ceiling: RiskClass::L1,
                allowed_actions: vec!["create_index".into(), "analyze_table".into()],
            },
        }
    }
}

impl OpsScenario for PerfDiagnosticsScenario {
    fn descriptor(&self) -> &ScenarioDescriptor {
        &self.descriptor
    }

    fn collect_evidence<'a>(
        &'a self,
        ctx: &'a OpsContext,
        input: &'a serde_json::Value,
    ) -> BoxFuture<'a, Result<EvidencePack, OpsError>> {
        Box::pin(async move {
            let input: PerfDiagnosticsInput = serde_json::from_value(input.clone())?;
            collect_evidence(ctx, input.normalized()).await
        })
    }

    fn build_plan(
        &self,
        identity: &PlanIdentity,
        evidence: &EvidencePack,
        input: &serde_json::Value,
        policy: &crate::ops::policy::OpsPolicy,
    ) -> Result<OpsPlan, OpsError> {
        let input: PerfDiagnosticsInput = serde_json::from_value(input.clone())?;
        build_plan(identity, evidence, input.normalized(), policy)
    }
}

// ── 证据采集 ───────────────────────────────────────────────

pub async fn collect_evidence(
    ctx: &OpsContext,
    input: PerfDiagnosticsInput,
) -> Result<EvidencePack, OpsError> {
    if !ctx.is_mysql_family() {
        return Err(OpsError::Validation(
            "首期性能诊断仅支持 MySQL/MariaDB".into(),
        ));
    }
    if ctx.db_name.trim().is_empty() {
        return Err(OpsError::Validation("当前连接缺少数据库名".into()));
    }
    validate_identifier(&ctx.db_name)
        .map_err(|e| OpsError::Validation(format!("数据库名非法: {e}")))?;

    let mut notes = Vec::new();
    let mut degraded_reasons = Vec::new();
    if let Some(note) = input.notes.clone() {
        notes.push(note);
    }

    let window = load_perf_window(ctx, &ctx.db_name).await;
    notes.push(window.note.clone());

    let (target_sql, sql_source, digest_info) =
        resolve_target_sql(ctx, &input, &mut notes, &mut degraded_reasons).await?;

    let statement = parse_single_readonly_query(&target_sql)?;
    let aliases = build_alias_map(&statement);

    let rich = explain_rich(ctx, "target", &target_sql).await?;
    let mut explain = rich.samples;
    let facts = rich.facts;
    notes.extend(rich.notes);
    if explain.is_empty() {
        return Err(OpsError::Validation(
            "EXPLAIN 未返回可分析的表行；请确认 SQL 是否包含表访问".into(),
        ));
    }

    let mut sort_or_temp: Vec<String> = Vec::new();
    if facts.using_filesort {
        sort_or_temp.push("FORMAT=JSON: using_filesort=true".into());
    }
    if facts.using_temporary_table {
        sort_or_temp.push("FORMAT=JSON: using_temporary_table=true".into());
    }
    for sample in &explain {
        let extra = sample.extra.as_deref().unwrap_or("");
        if extra.to_ascii_lowercase().contains("using filesort")
            && !sort_or_temp.iter().any(|s| s.contains("filesort"))
        {
            sort_or_temp.push(format!("{}: Extra 含 Using filesort", sample.label));
        }
        if extra.to_ascii_lowercase().contains("using temporary")
            && !sort_or_temp.iter().any(|s| s.contains("temporary"))
        {
            sort_or_temp.push(format!("{}: Extra 含 Using temporary", sample.label));
        }
    }
    if let Some(cost) = facts.query_cost {
        if let Some(first) = explain.first_mut() {
            first.estimated_cost = Some(cost);
        }
    }

    let table_names = collect_table_names(&explain, &aliases);
    let mut tables = Vec::new();
    let mut freshness_notes = Vec::new();
    for table in &table_names {
        if validate_identifier(table).is_err() {
            notes.push(format!("跳过非法表名 {table}"));
            continue;
        }
        match crate::ops::index_advisor::load_snapshot_public(ctx, table).await {
            Ok(snapshot) => {
                if snapshot.columns.is_empty() {
                    notes.push(format!("表 {table} 不存在或没有可见列"));
                    continue;
                }
                let stats = crate::ops::index_advisor::load_table_stats_public(ctx, table).await?;
                let freshness = load_freshness(ctx, table, &stats).await;
                freshness_notes.extend(
                    freshness
                        .notes
                        .iter()
                        .map(|n| format!("{table}: {n}")),
                );
                tables.push(PerfTableEvidence {
                    table: table.clone(),
                    snapshot,
                    stats,
                    freshness,
                });
            }
            Err(err) => notes.push(format!("读取表 {table} 结构失败: {err}")),
        }
    }
    if tables.is_empty() {
        return Err(OpsError::Validation(
            "EXPLAIN 涉及的表均无法读取结构，不能给出可靠方案".into(),
        ));
    }
    notes.extend(freshness_notes);

    let primary = pick_primary_table(&tables)
        .ok_or_else(|| OpsError::Validation("无法确定主表".into()))?;
    let (primary_table_name, primary_snapshot, primary_stats) = {
        let table = &tables[primary];
        (table.table.clone(), table.snapshot.clone(), table.stats.clone())
    };

    let implicit_casts = detect_implicit_casts(&statement, &tables);
    let estimated_scan_rows = estimated_scan_rows(&explain, facts.rows_examined_per_scan);
    let row_estimate = compute_row_estimate(&tables, primary, &explain);
    notes.push(row_estimate.reason.clone());

    let mut actual_latency_ms = None;
    let mut explain_analyze_raw = None;
    if input.explain_analyze {
        if matches!(ctx.db_client.pool.db_type(), crate::config::DbType::MariaDB) {
            notes.push("MariaDB 不支持 EXPLAIN ANALYZE；已降级为普通 EXPLAIN + FORMAT=JSON".into());
        } else {
            let timeout_ms = input.analyze_timeout_ms();
            let run = explain_analyze_with_timeout(ctx, &target_sql, timeout_ms).await;
            let outcome = match &run.result {
                AnalyzeResult::Success { ms, raw } => {
                    actual_latency_ms = Some(*ms);
                    explain_analyze_raw = Some(truncate(raw, 2000));
                    if let Some(first) = explain.first_mut() {
                        first.actual_time_ms = Some(*ms);
                    }
                    notes.push(
                        "EXPLAIN ANALYZE 会真实执行该只读查询；已按用户勾选采集实际耗时"
                            .into(),
                    );
                    "executed".to_string()
                }
                AnalyzeResult::Empty => {
                    notes.push("EXPLAIN ANALYZE 无返回内容".into());
                    "empty".to_string()
                }
                AnalyzeResult::Timeout(err) => {
                    degraded_reasons.push(format!(
                        "explain_analyze_timeout: 超过会话 max_execution_time={timeout_ms}ms（{err}）"
                    ));
                    notes.push(format!(
                        "EXPLAIN ANALYZE 超时（max_execution_time={timeout_ms}ms），已按降级处理"
                    ));
                    "timeout".to_string()
                }
                AnalyzeResult::Failed(err) => {
                    notes.push(format!("EXPLAIN ANALYZE 失败（可能不支持或权限不足）: {err}"));
                    "failed".to_string()
                }
                AnalyzeResult::NotConfigured(err) => {
                    notes.push(format!(
                        "无法设置 max_execution_time={timeout_ms}ms（{err}），为避免无超时执行，已跳过 EXPLAIN ANALYZE"
                    ));
                    degraded_reasons.push("explain_analyze_timeout_not_configured".into());
                    "skipped".to_string()
                }
            };
            append_analyze_audit(
                ctx,
                &target_sql,
                timeout_ms,
                run.duration_ms,
                &outcome,
                actual_latency_ms,
                match &run.result {
                    AnalyzeResult::Timeout(err) | AnalyzeResult::Failed(err) => Some(err.clone()),
                    AnalyzeResult::NotConfigured(err) => Some(err.clone()),
                    _ => None,
                },
            )?;
        }
    }

    let perf = PerfEvidence {
        target_sql: target_sql.clone(),
        sql_source,
        digest: digest_info,
        window: Some(window),
        row_estimate: Some(row_estimate),
        explain: explain.clone(),
        tables,
        estimated_scan_rows,
        implicit_casts,
        sort_or_temp,
        actual_latency_ms,
        explain_analyze_raw,
        notes: notes.clone(),
        degraded_reasons: degraded_reasons.clone(),
    };

    Ok(EvidencePack {
        db_name: ctx.db_name.clone(),
        table: primary_table_name,
        snapshot: primary_snapshot,
        stats: primary_stats,
        explain,
        index_usage: vec![],
        workload_sql: vec![target_sql],
        collected_at: crate::ops::types::now(),
        notes,
        perf: Some(perf),
        extra: None,
    })
}

async fn resolve_target_sql(
    ctx: &OpsContext,
    input: &PerfDiagnosticsInput,
    notes: &mut Vec<String>,
    degraded: &mut Vec<String>,
) -> Result<(String, String, Option<DigestInfo>), OpsError> {
    let mut digest_info = None;
    if let Some(digest) = &input.digest {
        digest_info = load_digest_info(ctx, digest).await;
        if digest_info.is_none() {
            degraded.push(format!(
                "digest_metrics_unavailable: 未在 digest 表中找到 {digest} 的指标"
            ));
        }
    }

    if let Some(sql) = &input.sql {
        ensure_single_readonly_query(sql)?;
        return Ok((sql.clone(), "user".into(), digest_info));
    }
    if let Some(sql) = &input.sample_sql {
        ensure_single_readonly_query(sql)?;
        return Ok((sql.clone(), "digest_sample".into(), digest_info));
    }
    if let Some(digest) = &input.digest {
        if let Some(sample) = lookup_sample_sql(ctx, digest, &ctx.db_name).await {
            ensure_single_readonly_query(&sample)?;
            notes.push("已从 performance_schema history 回查 digest 对应的 SQL_TEXT".into());
            return Ok((sample, "digest_sample".into(), digest_info));
        }
        return Err(OpsError::Validation(format!(
            "digest {digest} 只找到带 ? 占位的 digest_text，没有可执行 SQL 样本；请粘贴实际 SQL 后再诊断（digest_text 不能直接 EXPLAIN）"
        )));
    }
    Err(OpsError::Validation(
        "请提供一条只读 SQL，或从慢查询摘要选择一条并粘贴实际 SQL".into(),
    ))
}

fn ensure_single_readonly_query(sql: &str) -> Result<(), OpsError> {
    if sql.len() > MAX_SQL_LEN {
        return Err(OpsError::Validation(format!(
            "SQL 超过 {MAX_SQL_LEN} 字符"
        )));
    }
    let statements = Parser::parse_sql(&MySqlDialect {}, sql)
        .map_err(|e| OpsError::Validation(format!("SQL 解析失败: {e}")))?;
    if statements.len() != 1 {
        return Err(OpsError::Validation(
            "性能诊断只接受单条 SQL".into(),
        ));
    }
    if !matches!(statements[0], Statement::Query(_)) {
        return Err(OpsError::Validation(
            "性能诊断只接受只读 SELECT 查询".into(),
        ));
    }
    if !is_read_only_statement(&statements[0]) {
        return Err(OpsError::Validation("性能诊断只接受只读查询".into()));
    }
    Ok(())
}

fn parse_single_readonly_query(sql: &str) -> Result<Statement, OpsError> {
    ensure_single_readonly_query(sql)?;
    Parser::parse_sql(&MySqlDialect {}, sql)
        .map_err(|e| OpsError::Validation(format!("SQL 解析失败: {e}")))?
        .into_iter()
        .next()
        .ok_or_else(|| OpsError::Validation("SQL 为空".into()))
}

// ── EXPLAIN 采集与解析 ─────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub(crate) struct ExplainJsonFacts {
    pub query_cost: Option<f64>,
    pub using_filesort: bool,
    pub using_temporary_table: bool,
    pub rows_examined_per_scan: Option<i64>,
    pub table_names: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ExplainRich {
    pub samples: Vec<ExplainSample>,
    pub facts: ExplainJsonFacts,
    pub notes: Vec<String>,
}

async fn explain_plain_rows(
    ctx: &OpsContext,
    label: &str,
    sql: &str,
) -> Result<Vec<ExplainSample>, OpsError> {
    let rows = fetch_rows(ctx, &format!("EXPLAIN {sql}")).await?;
    let mut out = Vec::new();
    for (idx, row) in rows.iter().enumerate() {
        let table_name = str_value(row, "table");
        if table_name
            .as_deref()
            .map(|t| t.starts_with('<'))
            .unwrap_or(false)
        {
            continue;
        }
        out.push(ExplainSample {
            label: format!("{label}-{}", idx + 1),
            sql: sql.to_string(),
            access_type: str_value(row, "type"),
            key: str_value(row, "key"),
            possible_keys: str_value(row, "possible_keys"),
            rows_examined: int_value(row, "rows"),
            filtered: float_value(row, "filtered"),
            extra: str_value(row, "extra"),
            table_name,
            estimated_cost: None,
            actual_time_ms: None,
        });
    }
    Ok(out)
}

pub(crate) async fn explain_rich(
    ctx: &OpsContext,
    label: &str,
    sql: &str,
) -> Result<ExplainRich, OpsError> {
    let mut samples = explain_plain_rows(ctx, label, sql).await?;
    let mut notes = Vec::new();
    let mut facts = ExplainJsonFacts::default();
    match fetch_rows(ctx, &format!("EXPLAIN FORMAT=JSON {sql}")).await {
        Ok(rows) => {
            if let Some(raw) = rows
                .first()
                .and_then(|row| row.get("explain").cloned().or_else(|| row.values().next().cloned()))
            {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) {
                    facts = parse_explain_json(&value);
                } else {
                    notes.push("EXPLAIN FORMAT=JSON 返回内容不是合法 JSON，仅使用传统 EXPLAIN".into());
                }
            } else {
                notes.push("EXPLAIN FORMAT=JSON 无返回内容，仅使用传统 EXPLAIN".into());
            }
        }
        Err(err) => notes.push(format!(
            "EXPLAIN FORMAT=JSON 不可用（{err}），仅使用传统 EXPLAIN；成本对比将缺失"
        )),
    }
    if let Some(cost) = facts.query_cost {
        if let Some(first) = samples.first_mut() {
            first.estimated_cost = Some(cost);
        }
    }
    Ok(ExplainRich {
        samples,
        facts,
        notes,
    })
}

fn parse_explain_json(value: &serde_json::Value) -> ExplainJsonFacts {
    let mut facts = ExplainJsonFacts::default();
    visit_json(value, &mut facts);
    facts
}

fn visit_json(value: &serde_json::Value, facts: &mut ExplainJsonFacts) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                match key.as_str() {
                    "query_cost" => {
                        if let Some(cost) = json_f64(value) {
                            facts.query_cost =
                                Some(facts.query_cost.map_or(cost, |existing| existing.max(cost)));
                        }
                    }
                    "using_filesort" => {
                        if value.as_bool() == Some(true) {
                            facts.using_filesort = true;
                        }
                    }
                    "using_temporary_table" => {
                        if value.as_bool() == Some(true) {
                            facts.using_temporary_table = true;
                        }
                    }
                    "rows_examined_per_scan" => {
                        if let Some(rows) = value.as_i64() {
                            facts.rows_examined_per_scan = Some(
                                facts.rows_examined_per_scan.map_or(rows, |r| r.max(rows)),
                            );
                        }
                    }
                    "table_name" => {
                        if let Some(name) = value.as_str() {
                            let name = name.to_string();
                            if !facts.table_names.iter().any(|t| t.eq_ignore_ascii_case(&name)) {
                                facts.table_names.push(name);
                            }
                        }
                    }
                    _ => {}
                }
                visit_json(value, facts);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                visit_json(item, facts);
            }
        }
        _ => {}
    }
}

fn json_f64(value: &serde_json::Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
}

#[derive(Debug, Clone)]
enum AnalyzeResult {
    Success { ms: f64, raw: String },
    Empty,
    Timeout(String),
    Failed(String),
    NotConfigured(String),
}

#[derive(Debug, Clone)]
struct AnalyzeRun {
    result: AnalyzeResult,
    duration_ms: u64,
}

async fn explain_analyze_with_timeout(
    ctx: &OpsContext,
    sql: &str,
    timeout_ms: u64,
) -> AnalyzeRun {
    let pool = match ctx.db_client.mysql_pool() {
        Ok(pool) => pool,
        Err(err) => {
            return AnalyzeRun {
                result: AnalyzeResult::NotConfigured(err.to_string()),
                duration_ms: 0,
            }
        }
    };
    let mut conn = match pool.acquire().await {
        Ok(conn) => conn,
        Err(err) => {
            return AnalyzeRun {
                result: AnalyzeResult::NotConfigured(format!("获取连接失败: {err}")),
                duration_ms: 0,
            }
        }
    };
    // 必须在同一连接上设置会话超时，否则 pool 会把 SET 与 EXPLAIN 分到不同连接。
    let set_sql = format!("SET SESSION max_execution_time = {timeout_ms}");
    if let Err(err) = sqlx::query(&set_sql).execute(&mut *conn).await {
        return AnalyzeRun {
            result: AnalyzeResult::NotConfigured(err.to_string()),
            duration_ms: 0,
        };
    }
    let started = std::time::Instant::now();
    let result = sqlx::query(&format!("EXPLAIN ANALYZE {sql}"))
        .fetch_all(&mut *conn)
        .await;
    let duration_ms = started.elapsed().as_millis() as u64;
    let _ = sqlx::query("SET SESSION max_execution_time = 0")
        .execute(&mut *conn)
        .await;

    match result {
        Ok(rows) => {
            let maps = convert_mysql_rows(&rows);
            let raw = maps
                .iter()
                .filter_map(|row| row.get("explain").cloned())
                .collect::<Vec<_>>()
                .join("\n");
            if raw.trim().is_empty() {
                AnalyzeRun {
                    result: AnalyzeResult::Empty,
                    duration_ms,
                }
            } else if duration_ms >= timeout_ms {
                // MySQL 的 max_execution_time 触发后 EXPLAIN ANALYZE 仍可能返回被截断的输出，
                // 不一定报 3024，因此按实际墙钟时长判定超时并降级。
                AnalyzeRun {
                    result: AnalyzeResult::Timeout(format!(
                        "执行耗时 {duration_ms}ms 达到会话 max_execution_time={timeout_ms}ms，输出可能被截断"
                    )),
                    duration_ms,
                }
            } else if let Some(ms) = parse_actual_time_ms(&raw) {
                AnalyzeRun {
                    result: AnalyzeResult::Success { ms, raw },
                    duration_ms,
                }
            } else {
                AnalyzeRun {
                    result: AnalyzeResult::Failed("无法从 EXPLAIN ANALYZE 输出解析 actual time".into()),
                    duration_ms,
                }
            }
        }
        Err(err) => {
            let text = err.to_string();
            let result = if is_analyze_timeout(&text) {
                AnalyzeResult::Timeout(text)
            } else {
                AnalyzeResult::Failed(text)
            };
            AnalyzeRun {
                result,
                duration_ms,
            }
        }
    }
}

fn is_analyze_timeout(err: &str) -> bool {
    let lower = err.to_ascii_lowercase();
    lower.contains("maximum execution time")
        || lower.contains("max_execution_time")
        || lower.contains("3024")
}

fn append_analyze_audit(
    ctx: &OpsContext,
    target_sql: &str,
    timeout_ms: u64,
    duration_ms: u64,
    outcome: &str,
    actual_latency_ms: Option<f64>,
    error: Option<String>,
) -> Result<(), OpsError> {
    let Some(audit) = &ctx.audit else {
        return Ok(());
    };
    let record = analyze_audit_record(
        &ctx.db_id,
        &ctx.db_name,
        ctx.session_id.as_deref(),
        target_sql,
        timeout_ms,
        duration_ms,
        outcome,
        actual_latency_ms,
        error,
    );
    audit.append(&record)
}

#[allow(clippy::too_many_arguments)]
fn analyze_audit_record(
    db_id: &str,
    db_name: &str,
    session_id: Option<&str>,
    target_sql: &str,
    timeout_ms: u64,
    duration_ms: u64,
    outcome: &str,
    actual_latency_ms: Option<f64>,
    error: Option<String>,
) -> AuditRecord {
    let mut record = AuditRecord::new("perf.explain_analyze", outcome);
    record.db_id = Some(db_id.to_string());
    record.db_name = Some(db_name.to_string());
    record.action_id = Some("perf.explain_analyze".into());
    record.detail = serde_json::json!({
        "session_id": session_id,
        "target_sql_summary": truncate(target_sql, 300),
        "timeout_ms": timeout_ms,
        "duration_ms": duration_ms,
        "result": outcome,
        "actual_latency_ms": actual_latency_ms,
        "error": error,
    });
    record
}

fn parse_actual_time_ms(text: &str) -> Option<f64> {
    let mut best: Option<f64> = None;
    let mut rest = text;
    while let Some(pos) = rest.find("actual time=") {
        let after = &rest[pos + "actual time=".len()..];
        let segment = after.split_whitespace().next().unwrap_or("");
        if let Some((_, right)) = segment.split_once("..") {
            if let Ok(value) = right.trim_end_matches(',').parse::<f64>() {
                best = Some(best.map_or(value, |existing| existing.max(value)));
            }
        }
        rest = &after[1..];
    }
    best
}

/// 复测统一口径：传统 EXPLAIN + FORMAT=JSON 成本；可选 EXPLAIN ANALYZE 实际耗时。
/// 返回样本与降级说明。EXPLAIN ANALYZE 是真实执行，同样受会话超时约束并写入审计。
pub async fn explain_for_verification(
    ctx: &OpsContext,
    sqls: &[String],
    analyze: bool,
) -> Result<(Vec<ExplainSample>, Vec<String>), OpsError> {
    let mut out = Vec::new();
    let mut notes = Vec::new();
    for (idx, sql) in sqls.iter().enumerate() {
        let label = format!("verify-{}", idx + 1);
        let Ok(rich) = explain_rich(ctx, &label, sql).await else {
            continue;
        };
        let mut samples = rich.samples;
        if analyze {
            if matches!(
                ctx.db_client.pool.db_type(),
                crate::config::DbType::MariaDB
            ) {
                notes.push("MariaDB 不支持 EXPLAIN ANALYZE；复测仅使用 EXPLAIN".into());
            } else {
                let timeout_ms = DEFAULT_ANALYZE_TIMEOUT_MS;
                let run = explain_analyze_with_timeout(ctx, sql, timeout_ms).await;
                let mut actual_ms = None;
                let outcome = match &run.result {
                    AnalyzeResult::Success { ms, .. } => {
                        actual_ms = Some(*ms);
                        if let Some(first) = samples.first_mut() {
                            first.actual_time_ms = Some(*ms);
                        }
                        "executed".to_string()
                    }
                    AnalyzeResult::Empty => "empty".to_string(),
                    AnalyzeResult::Timeout(err) => {
                        notes.push(format!(
                            "复测 EXPLAIN ANALYZE 超时（max_execution_time={timeout_ms}ms，{err}），已按降级处理"
                        ));
                        "timeout".to_string()
                    }
                    AnalyzeResult::Failed(err) => {
                        notes.push(format!(
                            "复测 EXPLAIN ANALYZE 失败（{err}），已按降级处理"
                        ));
                        "failed".to_string()
                    }
                    AnalyzeResult::NotConfigured(err) => {
                        notes.push(format!(
                            "无法设置 max_execution_time={timeout_ms}ms（{err}），复测已跳过 EXPLAIN ANALYZE"
                        ));
                        "skipped".to_string()
                    }
                };
                append_analyze_audit(
                    ctx,
                    sql,
                    timeout_ms,
                    run.duration_ms,
                    &outcome,
                    actual_ms,
                    match &run.result {
                        AnalyzeResult::Timeout(err)
                        | AnalyzeResult::Failed(err)
                        | AnalyzeResult::NotConfigured(err) => Some(err.clone()),
                        _ => None,
                    },
                )?;
            }
        }
        out.extend(samples);
    }
    Ok((out, notes))
}

// ── 表与统计信息 ───────────────────────────────────────────

fn collect_table_names(explain: &[ExplainSample], aliases: &HashMap<String, String>) -> Vec<String> {
    let mut names = Vec::new();
    for sample in explain {
        let Some(name) = sample.table_name.as_deref() else {
            continue;
        };
        if name.starts_with('<') || name.starts_with("derived") {
            continue;
        }
        let real = aliases
            .get(&name.to_ascii_lowercase())
            .cloned()
            .unwrap_or_else(|| name.to_string());
        if !names.iter().any(|n: &String| n.eq_ignore_ascii_case(&real)) {
            names.push(real);
        }
    }
    names
}

async fn load_freshness(ctx: &OpsContext, table: &str, stats: &TableStats) -> StatsFreshness {
    let database = escape_sql_string(&ctx.db_name);
    let tbl = escape_sql_string(table);
    let innodb_last_update = match fetch_rows(
        ctx,
        &format!(
            "SELECT CAST(last_update AS CHAR) AS last_update FROM mysql.innodb_table_stats \
             WHERE database_name = '{database}' AND table_name = '{tbl}' LIMIT 1"
        ),
    )
    .await
    {
        Ok(rows) => rows.first().and_then(|r| str_value(r, "last_update")),
        Err(_) => None,
    };

    let mut notes = Vec::new();
    let now = chrono::Utc::now().naive_utc();
    let mut status = "unknown".to_string();
    let mut stale_days = None;

    if let Some(update) = stats.update_time.as_deref() {
        if let Some(dt) = parse_mysql_datetime(update) {
            let days = (now - dt).num_days().max(0);
            stale_days = Some(days);
            status = if days > STATS_STALE_DAYS {
                "stale".into()
            } else {
                "fresh".into()
            };
        } else {
            notes.push(format!("UPDATE_TIME 无法解析: {update}"));
        }
    } else if let Some(update) = innodb_last_update.as_deref() {
        if let Some(dt) = parse_mysql_datetime(update) {
            let days = (now - dt).num_days().max(0);
            stale_days = Some(days);
            status = if days > STATS_STALE_DAYS {
                "stale".into()
            } else {
                "fresh".into()
            };
            notes.push("information_schema.UPDATE_TIME 为空，使用 innodb_table_stats.last_update".into());
        }
    } else {
        notes.push("UPDATE_TIME 与 innodb_table_stats.last_update 均不可用（InnoDB 常见或权限不足）".into());
    }
    if stats.table_rows.is_none() {
        notes.push("TABLE_ROWS 不可用".into());
    }
    notes.push(format!(
        "新鲜度判定规则：updated 距现在超过 {STATS_STALE_DAYS} 天为 stale；无法取得时间记为 unknown"
    ));

    StatsFreshness {
        update_time: stats.update_time.clone(),
        innodb_last_update,
        stale_days,
        status,
        notes,
    }
}

fn parse_mysql_datetime(s: &str) -> Option<chrono::NaiveDateTime> {
    let trimmed = s.trim();
    for fmt in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M:%S%.f"] {
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(trimmed, fmt) {
            return Some(dt);
        }
    }
    None
}

fn pick_primary_table(tables: &[PerfTableEvidence]) -> Option<usize> {
    if tables.is_empty() {
        return None;
    }
    Some(0)
}

fn estimated_scan_rows(explain: &[ExplainSample], json_rows: Option<i64>) -> Option<i64> {
    let summed: i64 = explain
        .iter()
        .filter_map(|s| s.rows_examined)
        .sum();
    if summed > 0 {
        Some(summed)
    } else {
        json_rows
    }
}

/// 行数估算可信度。优先 TABLE_ROWS，缺失/为 0/与 data_length 明显不一致时降级到 EXPLAIN rows；
/// 两者都不可用时标记 untrusted，方案侧不会自动生成索引动作。
fn compute_row_estimate(
    tables: &[PerfTableEvidence],
    primary: usize,
    explain: &[ExplainSample],
) -> RowEstimateEvidence {
    let table = &tables[primary];
    let table_rows = table.stats.table_rows.filter(|rows| *rows > 0);
    let trusted_table_rows = table_rows.filter(|_| table.stats.data_length != Some(0));
    if let Some(rows) = trusted_table_rows {
        return RowEstimateEvidence {
            value: rows as i64,
            source: "information_schema.TABLES.TABLE_ROWS".into(),
            trusted: true,
            reason: format!(
                "使用 TABLE_ROWS={rows}（TABLE_ROWS>0 且 data_length 未显示为空）"
            ),
        };
    }

    let explain_rows = explain.iter().filter_map(|s| s.rows_examined).max().unwrap_or(0);
    if explain_rows > 0 {
        let reason = if table.stats.table_rows.is_none() {
            format!("TABLE_ROWS 不可用，降级使用 EXPLAIN rows={explain_rows}")
        } else if table.stats.table_rows == Some(0) {
            format!("TABLE_ROWS=0，降级使用 EXPLAIN rows={explain_rows}")
        } else {
            format!(
                "TABLE_ROWS={} 与 data_length={:?} 不一致，降级使用 EXPLAIN rows={explain_rows}",
                table.stats.table_rows.unwrap_or(0),
                table.stats.data_length
            )
        };
        return RowEstimateEvidence {
            value: explain_rows,
            source: "EXPLAIN.rows".into(),
            trusted: true,
            reason,
        };
    }

    RowEstimateEvidence {
        value: 0,
        source: "none".into(),
        trusted: false,
        reason: "TABLE_ROWS 与 EXPLAIN rows 均不可用或为 0；行数不可知".into(),
    }
}

// ── SQL 结构解析：别名与隐式转换 ───────────────────────────

fn object_name_last(name: &ObjectName) -> String {
    name.0
        .last()
        .and_then(|part| match part {
            ObjectNamePart::Identifier(ident) => Some(ident.value.clone()),
            _ => None,
        })
        .unwrap_or_else(|| name.to_string())
}

fn build_alias_map(statement: &Statement) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Statement::Query(query) = statement else {
        return map;
    };
    collect_alias_map(&query.body, &mut map);
    map
}

fn collect_alias_map(body: &SetExpr, map: &mut HashMap<String, String>) {
    match body {
        SetExpr::Select(select) => {
            for table in &select.from {
                if let TableFactor::Table { name, alias, .. } = &table.relation {
                    if let Some(alias) = alias {
                        map.insert(alias.name.value.to_ascii_lowercase(), object_name_last(name));
                    }
                }
            }
        }
        SetExpr::Query(query) => collect_alias_map(&query.body, map),
        SetExpr::SetOperation { left, right, .. } => {
            collect_alias_map(left, map);
            collect_alias_map(right, map);
        }
        _ => {}
    }
}

fn detect_implicit_casts(
    statement: &Statement,
    tables: &[PerfTableEvidence],
) -> Vec<ImplicitCastFinding> {
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    if let Statement::Query(query) = statement {
        walk_query_conditions(query, tables, &mut out, &mut seen);
    }
    out
}

fn walk_query_conditions(
    query: &Query,
    tables: &[PerfTableEvidence],
    out: &mut Vec<ImplicitCastFinding>,
    seen: &mut HashSet<String>,
) {
    walk_set_expr(&query.body, tables, out, seen);
}

fn walk_set_expr(
    body: &SetExpr,
    tables: &[PerfTableEvidence],
    out: &mut Vec<ImplicitCastFinding>,
    seen: &mut HashSet<String>,
) {
    match body {
        SetExpr::Select(select) => {
            if let Some(selection) = &select.selection {
                walk_expr(selection, tables, out, seen);
            }
            if let Some(having) = &select.having {
                walk_expr(having, tables, out, seen);
            }
            for table in &select.from {
                for join in &table.joins {
                    match &join.join_operator {
                        JoinOperator::Inner(constraint)
                        | JoinOperator::Left(constraint)
                        | JoinOperator::LeftOuter(constraint)
                        | JoinOperator::Right(constraint)
                        | JoinOperator::RightOuter(constraint)
                        | JoinOperator::FullOuter(constraint)
                        | JoinOperator::CrossJoin(constraint) => {
                            if let JoinConstraint::On(expr) = constraint {
                                walk_expr(expr, tables, out, seen);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        SetExpr::Query(query) => walk_set_expr(&query.body, tables, out, seen),
        SetExpr::SetOperation { left, right, .. } => {
            walk_set_expr(left, tables, out, seen);
            walk_set_expr(right, tables, out, seen);
        }
        _ => {}
    }
}

fn walk_expr(
    expr: &Expr,
    tables: &[PerfTableEvidence],
    out: &mut Vec<ImplicitCastFinding>,
    seen: &mut HashSet<String>,
) {
    match expr {
        Expr::BinaryOp { left, op, right } => {
            if is_comparison_op(op) {
                record_comparison(left, op, right, tables, out, seen);
                record_comparison(right, op, left, tables, out, seen);
            }
            walk_expr(left, tables, out, seen);
            walk_expr(right, tables, out, seen);
        }
        Expr::Nested(inner) | Expr::UnaryOp { expr: inner, .. } => {
            walk_expr(inner, tables, out, seen);
        }
        Expr::InList { expr, list, .. } => {
            for item in list {
                record_comparison(expr, &BinaryOperator::Eq, item, tables, out, seen);
            }
            walk_expr(expr, tables, out, seen);
        }
        Expr::Between { expr, low, high, .. } => {
            record_comparison(expr, &BinaryOperator::GtEq, low, tables, out, seen);
            record_comparison(expr, &BinaryOperator::LtEq, high, tables, out, seen);
        }
        _ => {}
    }
}

fn is_comparison_op(op: &BinaryOperator) -> bool {
    matches!(
        op,
        BinaryOperator::Eq
            | BinaryOperator::NotEq
            | BinaryOperator::Gt
            | BinaryOperator::Lt
            | BinaryOperator::GtEq
            | BinaryOperator::LtEq
    )
}

fn record_comparison(
    column_expr: &Expr,
    op: &BinaryOperator,
    literal_expr: &Expr,
    tables: &[PerfTableEvidence],
    out: &mut Vec<ImplicitCastFinding>,
    seen: &mut HashSet<String>,
) {
    let Some((_, column_name)) = expr_column(column_expr) else {
        return;
    };
    let Some((kind, literal)) = expr_literal(literal_expr) else {
        return;
    };
    let Some((table, column)) = resolve_column(column_expr, &column_name, tables) else {
        return;
    };
    let column_type = column.column_type.to_ascii_lowercase();
    let (index_risk, detail) = if is_character_type(&column_type) && kind == "number" {
        (
            true,
            format!(
                "字符/文本列 {} 与数值字面量 {} 比较：MySQL 会把列值转成数值，索引可能失效",
                column.name, literal
            ),
        )
    } else if is_numeric_type(&column_type) && kind == "string" {
        (
            false,
            format!(
                "数值列 {} 与字符串字面量 '{}' 比较：MySQL 会把字面量转成数值，一般不影响索引；仅作证据记录",
                column.name, literal
            ),
        )
    } else {
        return;
    };

    let key = format!(
        "{}|{}|{}|{}",
        table.table.to_ascii_lowercase(),
        column.name.to_ascii_lowercase(),
        op,
        literal
    );
    if !seen.insert(key) {
        return;
    }
    out.push(ImplicitCastFinding {
        table: table.table.clone(),
        column: column.name.clone(),
        column_type: column.column_type.clone(),
        operator: op.to_string(),
        literal_kind: kind.into(),
        literal,
        index_risk,
        detail,
    });
}

fn expr_column(expr: &Expr) -> Option<(Option<String>, String)> {
    match expr {
        Expr::Identifier(ident) => Some((None, ident.value.clone())),
        Expr::CompoundIdentifier(idents) if idents.len() >= 2 => {
            let table = idents[idents.len() - 2].value.clone();
            let column = idents[idents.len() - 1].value.clone();
            Some((Some(table), column))
        }
        Expr::Nested(inner) => expr_column(inner),
        _ => None,
    }
}

fn expr_literal(expr: &Expr) -> Option<(&'static str, String)> {
    match expr {
        Expr::Value(value) => match &value.value {
            Value::SingleQuotedString(s)
            | Value::DoubleQuotedString(s)
            | Value::TripleSingleQuotedString(s) => Some(("string", s.clone())),
            Value::Number(n, _) => Some(("number", n.to_string())),
            _ => None,
        },
        Expr::Nested(inner) => expr_literal(inner),
        _ => None,
    }
}

fn resolve_column<'a>(
    expr: &Expr,
    column_name: &str,
    tables: &'a [PerfTableEvidence],
) -> Option<(&'a PerfTableEvidence, &'a ColumnSnapshot)> {
    let (qualifier, _) = expr_column(expr)?;
    let candidates: Vec<&PerfTableEvidence> = if let Some(qualifier) = qualifier {
        tables
            .iter()
            .filter(|t| t.table.eq_ignore_ascii_case(&qualifier))
            .collect()
    } else {
        tables.iter().collect()
    };
    let mut matches = candidates
        .iter()
        .copied()
        .filter_map(|t| t.snapshot.find_column(column_name).map(|c| (t, c)));
    let first = matches.next()?;
    if matches.next().is_some() {
        // 列名在多张表里出现且没有限定表名，无法可靠判定属于哪张表。
        return None;
    }
    Some(first)
}

fn is_character_type(column_type: &str) -> bool {
    let lower = column_type.to_ascii_lowercase();
    [
        "char",
        "varchar",
        "tinytext",
        "text",
        "mediumtext",
        "longtext",
        "enum",
        "set",
        "binary",
        "varbinary",
    ]
    .iter()
    .any(|prefix| lower.starts_with(prefix))
}

fn is_numeric_type(column_type: &str) -> bool {
    let lower = column_type.to_ascii_lowercase();
    let base = lower.split_whitespace().next().unwrap_or("");
    let base = base.split('(').next().unwrap_or(base);
    matches!(
        base,
        "tinyint"
            | "smallint"
            | "mediumint"
            | "int"
            | "integer"
            | "bigint"
            | "decimal"
            | "numeric"
            | "float"
            | "double"
            | "real"
            | "bit"
            | "bool"
            | "boolean"
    )
}

// ── 方案生成 ───────────────────────────────────────────────

pub fn build_plan(
    identity: &PlanIdentity,
    evidence: &EvidencePack,
    input: PerfDiagnosticsInput,
    policy: &crate::ops::policy::OpsPolicy,
) -> Result<OpsPlan, OpsError> {
    let perf = evidence
        .perf
        .as_ref()
        .ok_or_else(|| OpsError::Validation("性能诊断缺少 perf 证据".into()))?;
    let snapshot = &evidence.snapshot;
    if snapshot.columns.is_empty() {
        return Err(OpsError::Validation("主表结构为空，无法生成方案".into()));
    }

    let mut findings = Vec::new();
    let mut risks = Vec::new();
    let mut checks = Vec::new();
    let mut actions = Vec::new();

    findings.push(Finding {
        id: "perf-target".into(),
        title: "慢 SQL 目标".into(),
        detail: format!(
            "来源 {}；SQL 长度 {} 字符；涉及表 {}",
            perf.sql_source,
            perf.target_sql.chars().count(),
            perf.tables
                .iter()
                .map(|t| t.table.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        severity: Severity::Info,
        evidence_refs: vec!["sql:target".into()],
        confidence: 1.0,
    });

    if let Some(window) = &perf.window {
        findings.push(Finding {
            id: "stats-window".into(),
            title: "统计窗口".into(),
            detail: window.note.clone(),
            severity: Severity::Info,
            evidence_refs: vec![
                "ps:global_status.Uptime".into(),
                "ps:events_statements_summary_by_digest".into(),
            ],
            confidence: if window.source == "unknown" { 0.5 } else { 0.9 },
        });
    }

    if let Some(digest) = &perf.digest {
        let cumulative = perf
            .window
            .as_ref()
            .map(|w| w.cumulative)
            .unwrap_or(true);
        let metric_scope = if cumulative { "累计" } else { "当前" };
        findings.push(Finding {
            id: "digest-metrics".into(),
            title: format!("performance_schema digest {}", truncate(&digest.digest, 12)),
            detail: format!(
                "{metric_scope}指标：执行 {} 次 | 总 {:.1}ms / 平均 {:.2}ms | 扫描总和 {} / 平均 {} | 临时表 {}（磁盘 {}）| 排序行 {}",
                digest.count_star,
                digest.total_latency_ms,
                digest.avg_latency_ms,
                digest.rows_examined_sum,
                digest.rows_examined_avg,
                digest.created_tmp_tables,
                digest.created_tmp_disk_tables,
                digest.sort_rows
            ),
            severity: Severity::Info,
            evidence_refs: vec!["ps:events_statements_summary_by_digest".into()],
            confidence: 1.0,
        });
    }

    for sample in &perf.explain {
        let access = sample.access_type.as_deref().unwrap_or("");
        let key = sample.key.as_deref().unwrap_or("");
        if sample.needs_index() {
            findings.push(Finding {
                id: format!("explain-{}", sample.label),
                title: format!(
                    "{} access={} key={}",
                    sample.table_name.as_deref().unwrap_or("(table)"),
                    if access.is_empty() { "-" } else { access },
                    if key.is_empty() { "-" } else { key }
                ),
                detail: sample.summary(),
                severity: Severity::Warn,
                evidence_refs: vec![format!("explain:{}", sample.label)],
                confidence: 0.8,
            });
        }
    }

    if !perf.sort_or_temp.is_empty() {
        findings.push(Finding {
            id: "sort-temp".into(),
            title: "存在排序 / 临时表证据".into(),
            detail: perf.sort_or_temp.join("; "),
            severity: Severity::Warn,
            evidence_refs: vec!["explain:sort-temp".into()],
            confidence: 0.85,
        });
    }

    for (idx, cast) in perf.implicit_casts.iter().enumerate() {
        findings.push(Finding {
            id: format!("implicit-cast-{}", idx + 1),
            title: format!(
                "疑似隐式类型转换: {}.{}",
                cast.table, cast.column
            ),
            detail: cast.detail.clone(),
            severity: if cast.index_risk {
                Severity::Warn
            } else {
                Severity::Info
            },
            evidence_refs: vec![format!("schema:{}.{}", cast.table, cast.column)],
            confidence: if cast.index_risk { 0.8 } else { 0.9 },
        });
    }

    findings.push(Finding {
        id: "stats-freshness".into(),
        title: format!("统计信息新鲜度: {}", evidence.perf.as_ref().unwrap_or(perf).tables[0].freshness.status),
        detail: perf
            .tables
            .iter()
            .map(|t| {
                format!(
                    "{}: status={} update_time={} innodb_last_update={} stale_days={}",
                    t.table,
                    t.freshness.status,
                    t.freshness.update_time.as_deref().unwrap_or("-"),
                    t.freshness.innodb_last_update.as_deref().unwrap_or("-"),
                    t.freshness
                        .stale_days
                        .map(|d| d.to_string())
                        .unwrap_or_else(|| "-".into())
                )
            })
            .collect::<Vec<_>>()
            .join("; "),
        severity: if perf.tables.iter().any(|t| t.freshness.status == "stale") {
            Severity::Warn
        } else {
            Severity::Info
        },
        evidence_refs: vec![
            "stats:information_schema.TABLES".into(),
            "stats:mysql.innodb_table_stats".into(),
        ],
        confidence: 0.9,
    });

    if perf.tables.len() > 1 {
        findings.push(Finding {
            id: "multi-table".into(),
            title: "多表查询".into(),
            detail: format!(
                "涉及 {} 张表；首期执行器只支持单表动作，本计划只为主表 {} 生成可执行动作，其余表仅保留证据与文本建议",
                perf.tables.len(),
                snapshot.table_name
            ),
            severity: Severity::Info,
            evidence_refs: vec!["explain:target".into()],
            confidence: 1.0,
        });
    }

    // ANALYZE TABLE：统计缺失、未知或过期时生成。
    let analyze_needed = perf
        .tables
        .iter()
        .find(|t| t.table.eq_ignore_ascii_case(&snapshot.table_name))
        .map(|t| t.freshness.status != "fresh" || t.stats.table_rows.is_none())
        .unwrap_or(true);
    if analyze_needed {
        checks.push(ValidationCheck {
            id: "analyze-reason".into(),
            title: "ANALYZE TABLE 依据".into(),
            status: ValidationStatus::Pass,
            detail: format!(
                "{} 的统计新鲜度为 {}，TABLE_ROWS={}；ANALYZE 可刷新优化器统计",
                snapshot.table_name,
                perf
                    .tables
                    .iter()
                    .find(|t| t.table.eq_ignore_ascii_case(&snapshot.table_name))
                    .map(|t| t.freshness.status.as_str())
                    .unwrap_or("unknown"),
                perf
                    .tables
                    .iter()
                    .find(|t| t.table.eq_ignore_ascii_case(&snapshot.table_name))
                    .and_then(|t| t.stats.table_rows)
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "-".into())
            ),
        });
        actions.push(ActionSpec::AnalyzeTable {
            table: snapshot.table_name.clone(),
        });
        findings.push(Finding {
            id: "analyze-candidate".into(),
            title: format!("建议执行 ANALYZE TABLE {}", snapshot.table_name),
            detail: "统计信息 unknown/stale 或 TABLE_ROWS 不可用；先刷新统计再复测执行计划".into(),
            severity: Severity::Info,
            evidence_refs: vec!["stats:information_schema.TABLES".into()],
            confidence: 0.8,
        });
    }

    // 索引候选：只处理主表，复用确定性列提取。
    let columns = extract_candidate_columns(&perf.target_sql, snapshot);
    let row_gate_trusted = perf
        .row_estimate
        .as_ref()
        .map(|r| r.trusted)
        .unwrap_or(false);
    let row_gate_value = perf.row_estimate.as_ref().map(|r| r.value).unwrap_or(0);
    let row_gate_source = perf
        .row_estimate
        .as_ref()
        .map(|r| r.source.as_str())
        .unwrap_or("none");
    let row_gate_reason = perf
        .row_estimate
        .as_ref()
        .map(|r| r.reason.clone())
        .unwrap_or_else(|| "缺少行数估算证据".into());
    let mut gated_suggestions = Vec::new();
    if !columns.is_empty() {
        if let Some(existing) = snapshot.indexes.iter().find(|i| i.covers_prefix(&columns)) {
            findings.push(Finding {
                id: "candidate-covered".into(),
                title: format!("候选列 {} 已被现有索引覆盖", columns.join(",")),
                detail: format!(
                    "现有索引 {} ({}) 的前缀已覆盖，不再重复创建",
                    existing.name,
                    existing.column_names().join(",")
                ),
                severity: Severity::Info,
                evidence_refs: vec!["schema:information_schema.STATISTICS".into()],
                confidence: 1.0,
            });
        } else if snapshot
            .columns
            .iter()
            .filter(|c| columns.iter().any(|x| x.eq_ignore_ascii_case(&c.name)))
            .count()
            == columns.len()
        {
            if !row_gate_trusted || row_gate_value < INDEX_MIN_TRUSTED_ROWS {
                let (title, detail) = if !row_gate_trusted {
                    (
                        "行数不可知，未自动生成索引动作".to_string(),
                        format!(
                            "候选列 {} 可能受益于索引，但行数不可知（估算来源 {}，值 {}）；{}。请先刷新统计或人工确认表规模后再评估建索引",
                            columns.join(", "),
                            row_gate_source,
                            row_gate_value,
                            row_gate_reason
                        ),
                    )
                } else {
                    (
                        "表规模过小，未自动生成索引动作".to_string(),
                        format!(
                            "候选列 {} 的估算行数为 {}（来源 {}，阈值 {}），索引收益不确定，降级为文本建议",
                            columns.join(", "),
                            row_gate_value,
                            row_gate_source,
                            INDEX_MIN_TRUSTED_ROWS
                        ),
                    )
                };
                checks.push(ValidationCheck {
                    id: "index-row-gate".into(),
                    title: "索引动作行数门槛".into(),
                    status: ValidationStatus::Warn,
                    detail: detail.clone(),
                });
                findings.push(Finding {
                    id: "candidate-row-gate".into(),
                    title: title.clone(),
                    detail: detail.clone(),
                    severity: Severity::Warn,
                    evidence_refs: vec!["stats:row_estimate".into()],
                    confidence: 0.6,
                });
                gated_suggestions.push(RewriteSuggestion {
                    id: "rewrite-row-gate".into(),
                    title,
                    suggestion: format!(
                        "人工确认表规模与过滤条件后，再决定是否为 {} 建索引；当前计划未生成可执行的 CreateIndex 动作",
                        columns.join(", ")
                    ),
                    rationale: detail,
                    expected_gain: "避免在行数不可知或规模过小的表上引入无收益的索引成本".into(),
                    evidence_refs: vec!["stats:row_estimate".into()],
                });
            } else {
                let existing_names: HashSet<String> = snapshot
                    .indexes
                    .iter()
                    .map(|i| i.name.to_ascii_lowercase())
                    .collect();
                let name = make_index_name(&snapshot.table_name, &columns, &existing_names);
                let action = ActionSpec::CreateIndex {
                    table: snapshot.table_name.clone(),
                    name: name.clone(),
                    columns: columns.clone(),
                    unique: false,
                    using: Some("BTREE".into()),
                };
                match crate::ops::ddl_compiler::compile_action(&action, snapshot) {
                    Ok(compiled) => {
                        checks.push(ValidationCheck {
                            id: format!("compiled-{name}"),
                            title: format!("候选索引 {name} 通过编译校验"),
                            status: ValidationStatus::Pass,
                            detail: compiled.sql,
                        });
                        findings.push(Finding {
                            id: format!("candidate-{name}"),
                            title: format!("建议新增索引 {name}"),
                            detail: format!(
                                "候选列：{}；依据 EXPLAIN 全表扫描/无可用索引；行数估算 {} 行（来源 {}，可信）",
                                columns.join(", "),
                                row_gate_value,
                                row_gate_source
                            ),
                            severity: Severity::Info,
                            evidence_refs: vec!["explain:target".into(), "stats:row_estimate".into()],
                            confidence: 0.7,
                        });
                        actions.push(action);
                    }
                    Err(err) => checks.push(ValidationCheck {
                        id: format!("compile-failed-{name}"),
                        title: format!("候选索引 {name} 未通过编译校验"),
                        status: ValidationStatus::Fail,
                        detail: err.to_string(),
                    }),
                }
            }
        }
    } else {
        findings.push(Finding {
            id: "no-index-candidate".into(),
            title: "未生成索引候选".into(),
            detail: "SQL 中未提取到可索引的过滤/排序列，或候选列不存在于主表".into(),
            severity: Severity::Warn,
            evidence_refs: vec!["sql:target".into()],
            confidence: 0.8,
        });
    }
    if actions.iter().filter(|a| matches!(a, ActionSpec::CreateIndex { .. })).count() >= MAX_INDEX_ACTIONS {
        findings.push(Finding {
            id: "candidate-limit".into(),
            title: "候选索引达到上限".into(),
            detail: format!("首期最多生成 {MAX_INDEX_ACTIONS} 个索引候选"),
            severity: Severity::Info,
            evidence_refs: vec![],
            confidence: 0.6,
        });
    }

    let mut rewrite_suggestions = gated_suggestions;
    rewrite_suggestions.extend(build_rewrite_suggestions(perf, snapshot));

    checks.push(ValidationCheck {
        id: "action-whitelist".into(),
        title: "动作白名单".into(),
        status: ValidationStatus::Pass,
        detail: "计划只包含 create_index / analyze_table；SQL 改写建议不是动作".into(),
    });
    checks.push(ValidationCheck {
        id: "rewrite-not-executable".into(),
        title: "SQL 改写不可执行".into(),
        status: ValidationStatus::Pass,
        detail: format!(
            "{} 条改写建议只作为文本输出，不会进入 ActionSpec 或执行链路",
            rewrite_suggestions.len()
        ),
    });
    checks.push(ValidationCheck {
        id: "schema-exists".into(),
        title: "主表与列存在性".into(),
        status: ValidationStatus::Pass,
        detail: format!(
            "{} 列在 {}.{} 中可见",
            snapshot.columns.len(),
            snapshot.db_name,
            snapshot.table_name
        ),
    });

    let effective_rows = perf
        .row_estimate
        .as_ref()
        .filter(|r| r.trusted && r.value > 0)
        .map(|r| r.value as u64)
        .or(evidence.stats.table_rows);
    if let Some(rows) = effective_rows {
        risks.push(RiskFinding {
            id: "ddl-lock".into(),
            title: "DDL / 统计刷新风险".into(),
            detail: "在线建索引仍需短暂元数据锁；ANALYZE TABLE 会重新统计并占用资源。".into(),
            severity: Severity::Warn,
            blocking: false,
        });
        risks.push(RiskFinding {
            id: "index-cost".into(),
            title: "存储与写入成本".into(),
            detail: format!(
                "表约 {rows} 行；新增索引会增加存储并影响写入，执行前会做影响评估"
            ),
            severity: Severity::Info,
            blocking: false,
        });
        if rows > policy.max_affected_rows && !policy.allow_large_table {
            risks.push(RiskFinding {
                id: "large-table".into(),
                title: "表规模超过策略上限".into(),
                detail: format!(
                    "估算 {rows} 行 > max_affected_rows {}；当前策略禁止落地",
                    policy.max_affected_rows
                ),
                severity: Severity::Critical,
                blocking: true,
            });
        }
    }

    let rollback_actions: Vec<ActionSpec> = actions
        .iter()
        .filter_map(|action| match action {
            ActionSpec::CreateIndex {
                table,
                name,
                columns,
                ..
            } => Some(ActionSpec::DropIndex {
                table: table.clone(),
                name: name.clone(),
                expected_definition: Some(format!("INDEX ({})", columns.join(","))),
            }),
            _ => None,
        })
        .collect();
    let rollback_summary = if rollback_actions.is_empty() {
        "本计划仅包含 ANALYZE TABLE（不改变表结构），无需回滚".to_string()
    } else {
        "回滚将删除本次新增索引；ANALYZE TABLE 无需回滚，也不会触碰已有索引".to_string()
    };

    let diagnosis = if actions.is_empty() {
        format!(
            "未生成可自动执行的结构化动作；已输出 {} 条文本改写建议供人工评估。",
            rewrite_suggestions.len()
        )
    } else {
        format!(
            "检测到慢 SQL 的索引/统计缺口，建议执行 {} 个动作（{} 个索引候选、{} 个 ANALYZE）；另有 {} 条 SQL 改写建议仅文本。",
            actions.len(),
            actions
                .iter()
                .filter(|a| matches!(a, ActionSpec::CreateIndex { .. }))
                .count(),
            actions
                .iter()
                .filter(|a| matches!(a, ActionSpec::AnalyzeTable { .. }))
                .count(),
            rewrite_suggestions.len()
        )
    };

    let explain_findings = perf
        .explain
        .iter()
        .map(ExplainSample::summary)
        .chain(perf.sort_or_temp.iter().cloned())
        .chain(
            perf.implicit_casts
                .iter()
                .filter(|c| c.index_risk)
                .map(|c| format!("疑似隐式转换: {}.{} {}", c.table, c.column, c.literal)),
        )
        .collect();

    let plan = OpsPlan {
        plan_id: uuid::Uuid::new_v4().to_string(),
        scenario_id: "perf-diagnostics".into(),
        db_id: identity.db_id.clone(),
        db_name: identity.db_name.clone(),
        db_fingerprint: identity.db_fingerprint.clone(),
        evidence_hash: evidence.evidence_hash(),
        diagnosis,
        findings,
        verification_sql: vec![perf.target_sql.clone()],
        actions: actions.clone(),
        risks,
        validation_checks: checks,
        rollback: RollbackPlan {
            summary: rollback_summary,
            actions: rollback_actions,
            automatic: true,
        },
        code_advice: vec![
            "SQL 改写建议仅供人工评估，不能作为可执行动作；本计划只执行结构化索引/ANALYZE 动作"
                .into(),
            "执行后请用同一 EXPLAIN 口径复测；未改善时如实记录，不要只保留最好的一次".into(),
        ],
        rewrite_suggestions,
        verification_explain_analyze: input.explain_analyze,
        confidence: if actions.is_empty() {
            0.5
        } else {
            (0.55 + actions.len() as f32 * 0.08).min(0.9)
        },
        requires_approval: !actions.is_empty(),
        created_at: crate::ops::types::now(),
        plan_hash: String::new(),
        status: PlanStatus::PendingApproval,
        evidence_summary: Some(EvidenceSummary {
            table: snapshot.table_name.clone(),
            column_count: snapshot.columns.len(),
            index_count: snapshot.indexes.len(),
            row_estimate: effective_rows,
            explain_findings,
        }),
        execution: None,
    };
    Ok(plan.seal())
}

fn build_rewrite_suggestions(
    perf: &PerfEvidence,
    snapshot: &TableSnapshot,
) -> Vec<RewriteSuggestion> {
    let mut out = Vec::new();

    for (idx, cast) in perf.implicit_casts.iter().enumerate() {
        if !cast.index_risk {
            continue;
        }
        out.push(RewriteSuggestion {
            id: format!("rewrite-implicit-cast-{}", idx + 1),
            title: format!("消除 {}.{} 的隐式类型转换", cast.table, cast.column),
            suggestion: format!(
                "把 {} 条件中的数值字面量改为与列类型匹配的字符串字面量（加引号），或显式转换列值并接受索引失效的代价",
                cast.column
            ),
            rationale: format!(
                "列类型为 {}，当前 {} {} {}；字符列与数值比较会让 MySQL 把列值转成数值，索引无法按原值范围命中",
                cast.column_type, cast.column, cast.operator, cast.literal
            ),
            expected_gain: "让该列索引恢复可用，减少全表扫描；具体收益需执行后复测".into(),
            evidence_refs: vec![format!("schema:{}.{}", cast.table, cast.column)],
        });
    }

    if !perf.sort_or_temp.is_empty() {
        out.push(RewriteSuggestion {
            id: "rewrite-sort-temp".into(),
            title: "减少排序 / 临时表".into(),
            suggestion:
                "让 ORDER BY / GROUP BY 的列与索引前缀对齐（等值过滤列在前、排序列在后），或把聚合改为预聚合查询"
                    .into(),
            rationale: format!("EXPLAIN 证据: {}", perf.sort_or_temp.join("; ")),
            expected_gain: "避免 filesort 或磁盘临时表，降低排序与临时存储开销".into(),
            evidence_refs: vec!["explain:sort-temp".into()],
        });
    }

    if let Some(rows) = perf.estimated_scan_rows {
        if rows > MAX_SCAN_ROWS_ALERT {
            let all_scan = perf
                .explain
                .iter()
                .any(|s| s.access_type.as_deref() == Some("ALL"));
            if all_scan {
                out.push(RewriteSuggestion {
                    id: "rewrite-scan-rows".into(),
                    title: "降低扫描行数".into(),
                    suggestion:
                        "检查 WHERE 是否遗漏业务上必须的等值过滤条件，或增加更高选择性的条件/分页，避免一次扫描过多行"
                            .into(),
                    rationale: format!(
                        "EXPLAIN 预估扫描约 {rows} 行且存在 access=ALL 的访问路径；过滤性不足会让索引收益有限"
                    ),
                    expected_gain: "缩小扫描范围，降低 IO 与响应时间".into(),
                    evidence_refs: vec!["explain:target".into()],
                });
            }
        }
    }

    if perf.tables.iter().any(|t| t.freshness.status != "fresh") {
        out.push(RewriteSuggestion {
            id: "rewrite-refresh-stats".into(),
            title: "先刷新统计再评估".into(),
            suggestion: "先执行 ANALYZE TABLE（本计划已作为结构化动作给出），再重新读取 EXPLAIN 与 digest 指标".into(),
            rationale: "统计信息 unknown/stale 时，优化器的行数估算可能偏离实际，索引收益判断不可靠".into(),
            expected_gain: "让优化器基于最新统计选择更优执行计划".into(),
            evidence_refs: vec!["stats:information_schema.TABLES".into()],
        });
    }

    let lower = perf.target_sql.to_ascii_lowercase();
    if lower.contains("select *") || lower.contains("select\n*") {
        out.push(RewriteSuggestion {
            id: "rewrite-select-star".into(),
            title: "减少 SELECT * 的列投影".into(),
            suggestion: "只选择实际需要的列，避免不必要的回表与网络传输；必要时用覆盖索引".into(),
            rationale: format!(
                "目标 SQL 使用 SELECT *；表 {} 共 {} 列，全列投影会放大 IO",
                snapshot.table_name,
                snapshot.columns.len()
            ),
            expected_gain: "降低回表/传输成本，可能让覆盖索引生效".into(),
            evidence_refs: vec!["sql:target".into()],
        });
    }

    out
}

fn make_index_name(table: &str, columns: &[String], existing: &HashSet<String>) -> String {
    let mut base = format!("idx_{}_{}", table, columns.join("_")).to_ascii_lowercase();
    base = base
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .collect();
    if base.len() > 60 {
        base.truncate(60);
    }
    let mut candidate = base.clone();
    let mut suffix = 2;
    while existing.contains(&candidate.to_ascii_lowercase()) {
        candidate = format!("{base}_{suffix}");
        suffix += 1;
    }
    candidate
}

// ── 单测 ───────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(table: &str, columns: &[(&str, &str)]) -> TableSnapshot {
        TableSnapshot {
            db_name: "app".into(),
            table_name: table.into(),
            columns: columns
                .iter()
                .map(|(name, ty)| ColumnSnapshot {
                    name: (*name).into(),
                    column_type: (*ty).into(),
                    nullable: true,
                    ..Default::default()
                })
                .collect(),
            indexes: vec![],
            create_table_sql: None,
        }
    }

    fn perf_evidence(target_sql: &str, snapshot: TableSnapshot) -> EvidencePack {
        let table = PerfTableEvidence {
            table: snapshot.table_name.clone(),
            snapshot: snapshot.clone(),
            stats: TableStats {
                table_rows: Some(200_000),
                update_time: Some("2026-11-01 10:00:00".into()),
                ..Default::default()
            },
            freshness: StatsFreshness {
                status: "fresh".into(),
                ..Default::default()
            },
        };
        let sample = ExplainSample {
            label: "target-1".into(),
            sql: target_sql.into(),
            access_type: Some("ALL".into()),
            key: None,
            possible_keys: None,
            rows_examined: Some(199_000),
            filtered: Some(1.0),
            extra: Some("Using where; Using filesort".into()),
            table_name: Some(snapshot.table_name.clone()),
            ..Default::default()
        };
        EvidencePack {
            db_name: "app".into(),
            table: snapshot.table_name.clone(),
            snapshot: snapshot.clone(),
            stats: table.stats.clone(),
            explain: vec![sample],
            workload_sql: vec![target_sql.into()],
            notes: vec![],
            perf: Some(PerfEvidence {
                target_sql: target_sql.into(),
                sql_source: "user".into(),
                explain: vec![ExplainSample {
                    label: "target-1".into(),
                    sql: target_sql.into(),
                    access_type: Some("ALL".into()),
                    key: None,
                    possible_keys: None,
                    rows_examined: Some(199_000),
                    filtered: Some(1.0),
                    extra: Some("Using where; Using filesort".into()),
                    table_name: Some(snapshot.table_name.clone()),
                    ..Default::default()
                }],
                tables: vec![table],
                estimated_scan_rows: Some(199_000),
                sort_or_temp: vec!["target-1: Extra 含 Using filesort".into()],
                window: Some(PerfWindowInfo {
                    window_start: Some("2026-11-01 00:00:00".into()),
                    window_hours: Some(168.0),
                    source: "server_uptime".into(),
                    cumulative: true,
                    note: "统计窗口：P_S 已运行 168.0 小时（起点 2026-11-01 00:00:00）；延迟与扫描数为累计值，不是最近该时长的增量。".into(),
                }),
                row_estimate: Some(RowEstimateEvidence {
                    value: 200_000,
                    source: "information_schema.TABLES.TABLE_ROWS".into(),
                    trusted: true,
                    reason: "使用 TABLE_ROWS=200000（TABLE_ROWS>0 且 data_length 未显示为空）".into(),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn identity() -> PlanIdentity {
        PlanIdentity {
            db_id: "db1".into(),
            db_name: "app".into(),
            db_fingerprint: "fp".into(),
            is_read_only: false,
        }
    }

    #[test]
    fn parse_explain_json_extracts_cost_filesort_temp_and_rows() {
        let value: serde_json::Value = serde_json::from_str(
            r#"{
                "query_block": {
                    "cost_info": {"query_cost": "842.35"},
                    "table": {
                        "table_name": "orders",
                        "access_type": "ALL",
                        "rows_examined_per_scan": 200000,
                        "using_filesort": true,
                        "using_temporary_table": true
                    }
                }
            }"#,
        )
        .unwrap();
        let facts = parse_explain_json(&value);
        assert_eq!(facts.query_cost, Some(842.35));
        assert!(facts.using_filesort);
        assert!(facts.using_temporary_table);
        assert_eq!(facts.rows_examined_per_scan, Some(200_000));
        assert_eq!(facts.table_names, vec!["orders".to_string()]);
    }

    #[test]
    fn parse_explain_analyze_actual_time() {
        let text = "-> Filter: (orders.status = 'paid')  (cost=100 rows=10) (actual time=1.234..5.678 rows=10 loops=1)\n-> Table scan on orders (actual time=0.123..4.987 rows=200 loops=1)";
        assert_eq!(parse_actual_time_ms(text), Some(5.678));
    }

    #[test]
    fn implicit_cast_rule_only_flags_index_risky_direction() {
        let statement = parse_single_readonly_query(
            "SELECT * FROM orders WHERE status = 1 AND user_id = '42' AND created_at >= '2026-01-01'",
        )
        .unwrap();
        let tables = vec![PerfTableEvidence {
            table: "orders".into(),
            snapshot: snapshot(
                "orders",
                &[
                    ("id", "bigint"),
                    ("status", "varchar(32)"),
                    ("user_id", "bigint"),
                    ("created_at", "datetime"),
                ],
            ),
            stats: TableStats::default(),
            freshness: StatsFreshness::default(),
        }];
        let casts = detect_implicit_casts(&statement, &tables);
        let risky: Vec<_> = casts.iter().filter(|c| c.index_risk).collect();
        assert_eq!(risky.len(), 1, "casts={casts:?}");
        assert_eq!(risky[0].column, "status");
        assert_eq!(risky[0].literal, "1");
        assert!(casts.iter().any(|c| c.column == "user_id" && !c.index_risk));
        assert!(!casts.iter().any(|c| c.column == "created_at"));
    }

    #[test]
    fn build_plan_generates_actions_and_text_only_rewrites() {
        let mut evidence = perf_evidence(
            "SELECT * FROM orders WHERE status = 1 ORDER BY created_at",
            snapshot(
                "orders",
                &[
                    ("id", "bigint"),
                    ("status", "varchar(32)"),
                    ("created_at", "datetime"),
                ],
            ),
        );
        if let Some(perf) = evidence.perf.as_mut() {
            perf.tables[0].freshness.status = "stale".into();
        }
        let plan = build_plan(
            &identity(),
            &evidence,
            PerfDiagnosticsInput::default().normalized(),
            &crate::ops::policy::OpsPolicy::default(),
        )
        .unwrap();

        assert_eq!(plan.scenario_id, "perf-diagnostics");
        assert!(plan
            .actions
            .iter()
            .all(|a| matches!(
                a,
                ActionSpec::CreateIndex { .. } | ActionSpec::AnalyzeTable { .. }
            )));
        assert!(plan.actions.iter().any(|a| matches!(a, ActionSpec::CreateIndex { .. })));
        assert!(plan.actions.iter().any(|a| matches!(a, ActionSpec::AnalyzeTable { .. })));
        assert!(!plan.rewrite_suggestions.is_empty());
        assert!(plan
            .validation_checks
            .iter()
            .any(|c| c.id == "rewrite-not-executable" && c.status == ValidationStatus::Pass));

        // 改写建议不可执行：不进入 actions，也不产生执行 SQL。
        for suggestion in &plan.rewrite_suggestions {
            let blob = format!(
                "{} {} {}",
                suggestion.title, suggestion.suggestion, suggestion.rationale
            );
            assert!(!blob.trim_start().to_ascii_lowercase().starts_with("select "));
            assert!(!blob.contains("ActionSpec::"));
            assert!(!plan
                .actions
                .iter()
                .any(|a| a.action_id().contains(&suggestion.id)));
        }
        assert!(plan
            .actions
            .iter()
            .all(|a| !matches!(a, ActionSpec::DropIndex { .. })));
        assert_eq!(plan.plan_hash, plan.clone().compute_hash());
    }

    #[test]
    fn stale_stats_adds_analyze_action() {
        let mut evidence = perf_evidence(
            "SELECT * FROM orders WHERE user_id = 42",
            snapshot("orders", &[("id", "bigint"), ("user_id", "bigint")]),
        );
        if let Some(perf) = evidence.perf.as_mut() {
            perf.tables[0].freshness.status = "stale".into();
        }
        let plan = build_plan(
            &identity(),
            &evidence,
            PerfDiagnosticsInput::default().normalized(),
            &crate::ops::policy::OpsPolicy::default(),
        )
        .unwrap();
        assert!(plan
            .actions
            .iter()
            .any(|a| matches!(a, ActionSpec::AnalyzeTable { .. })));
        assert!(plan
            .findings
            .iter()
            .any(|f| f.id == "stats-freshness"));
    }

    #[test]
    fn slow_query_candidate_summary_and_conversion() {
        let mut row = RowMap::new();
        row.insert("digest".into(), "a".repeat(64));
        row.insert("digest_text".into(), "SELECT * FROM orders WHERE user_id = ?".into());
        row.insert("schema_name".into(), "app".into());
        row.insert("count_star".into(), "10".into());
        row.insert("sum_timer_wait".into(), "5000000000".into());
        row.insert("avg_timer_wait".into(), "500000000".into());
        row.insert("sum_rows_examined".into(), "2000000".into());
        row.insert("sum_rows_sent".into(), "100".into());
        row.insert("sum_created_tmp_tables".into(), "2".into());
        row.insert("sum_sort_rows".into(), "300".into());
        row.insert("last_seen".into(), "2026-11-08 10:00:00".into());

        let candidate = SlowQueryCandidate::from_row(&row).unwrap();
        assert_eq!(candidate.count_star, 10);
        assert!((candidate.total_latency_ms - 5.0).abs() < 1e-9);
        assert!((candidate.avg_latency_ms - 0.5).abs() < 1e-9);
        assert_eq!(candidate.rows_examined_avg, 200_000);
        assert_eq!(candidate.rows_sent_avg, 10);
        assert!(candidate.summary.contains("执行 10 次"));
        assert!(candidate.summary.contains("扫描 200000/次"));
    }

    #[test]
    fn slow_query_sort_order_is_whitelisted() {
        assert_eq!(slow_query_order("total_latency"), "SUM_TIMER_WAIT DESC");
        assert_eq!(slow_query_order("avg_latency"), "AVG_TIMER_WAIT DESC");
        assert_eq!(slow_query_order("rows_examined"), "SUM_ROWS_EXAMINED DESC");
        assert_eq!(slow_query_order("'; DROP TABLE x"), "SUM_TIMER_WAIT DESC");
    }

    #[test]
    fn alias_map_resolves_explain_alias_to_real_table() {
        let statement = parse_single_readonly_query(
            "SELECT * FROM orders o WHERE o.user_id = 42",
        )
        .unwrap();
        let aliases = build_alias_map(&statement);
        assert_eq!(aliases.get("o"), Some(&"orders".to_string()));
        let names = collect_table_names(
            &[ExplainSample {
                table_name: Some("o".into()),
                ..Default::default()
            }],
            &aliases,
        );
        assert_eq!(names, vec!["orders".to_string()]);
    }

    #[test]
    fn stats_window_is_explicit_in_plan_findings() {
        let evidence = perf_evidence(
            "SELECT * FROM orders WHERE status = 1",
            snapshot("orders", &[("id", "bigint"), ("status", "varchar(32)")]),
        );
        let plan = build_plan(
            &identity(),
            &evidence,
            PerfDiagnosticsInput::default().normalized(),
            &crate::ops::policy::OpsPolicy::default(),
        )
        .unwrap();
        let window = plan
            .findings
            .iter()
            .find(|f| f.id == "stats-window")
            .expect("缺少统计窗口 finding");
        assert!(window.detail.contains("统计窗口"));
        assert!(window.detail.contains("已运行 168.0 小时"));
        assert!(window.detail.contains("累计值"));
    }

    #[test]
    fn untrusted_row_estimate_degrades_to_text_suggestion() {
        let mut evidence = perf_evidence(
            "SELECT * FROM orders WHERE status = 1",
            snapshot("orders", &[("id", "bigint"), ("status", "varchar(32)")]),
        );
        if let Some(perf) = evidence.perf.as_mut() {
            perf.row_estimate = Some(RowEstimateEvidence {
                value: 0,
                source: "none".into(),
                trusted: false,
                reason: "TABLE_ROWS 与 EXPLAIN rows 均不可用或为 0；行数不可知".into(),
            });
        }
        let plan = build_plan(
            &identity(),
            &evidence,
            PerfDiagnosticsInput::default().normalized(),
            &crate::ops::policy::OpsPolicy::default(),
        )
        .unwrap();
        assert!(
            !plan
                .actions
                .iter()
                .any(|a| matches!(a, ActionSpec::CreateIndex { .. })),
            "行数不可知时不应生成索引动作"
        );
        let suggestion = plan
            .rewrite_suggestions
            .iter()
            .find(|s| s.id == "rewrite-row-gate")
            .expect("缺少降级文本建议");
        assert!(suggestion.title.contains("行数不可知"));
        assert!(suggestion.title.contains("未自动生成索引动作"));
        assert!(suggestion.rationale.contains("行数不可知"));
        assert!(suggestion.rationale.contains("none"));
        assert!(plan.findings.iter().any(|f| f.id == "candidate-row-gate"));
    }

    #[test]
    fn small_trusted_table_skips_index_action() {
        let mut evidence = perf_evidence(
            "SELECT * FROM orders WHERE status = 1",
            snapshot("orders", &[("id", "bigint"), ("status", "varchar(32)")]),
        );
        if let Some(perf) = evidence.perf.as_mut() {
            perf.row_estimate = Some(RowEstimateEvidence {
                value: 200,
                source: "EXPLAIN.rows".into(),
                trusted: true,
                reason: "TABLE_ROWS 不可用，降级使用 EXPLAIN rows=200".into(),
            });
        }
        let plan = build_plan(
            &identity(),
            &evidence,
            PerfDiagnosticsInput::default().normalized(),
            &crate::ops::policy::OpsPolicy::default(),
        )
        .unwrap();
        assert!(!plan
            .actions
            .iter()
            .any(|a| matches!(a, ActionSpec::CreateIndex { .. })));
        let suggestion = plan
            .rewrite_suggestions
            .iter()
            .find(|s| s.id == "rewrite-row-gate")
            .expect("缺少降级文本建议");
        assert!(suggestion.title.contains("表规模过小"));
        assert!(suggestion.rationale.contains("200"));
    }

    #[test]
    fn compute_row_estimate_prefers_table_rows_then_explain_then_untrusted() {
        let mut table = PerfTableEvidence {
            table: "orders".into(),
            snapshot: snapshot("orders", &[("id", "bigint")]),
            stats: TableStats {
                table_rows: Some(500_000),
                data_length: Some(1_000_000),
                ..Default::default()
            },
            freshness: StatsFreshness::default(),
        };
        let explain = vec![ExplainSample {
            rows_examined: Some(120_000),
            ..Default::default()
        }];

        let trusted = compute_row_estimate(std::slice::from_ref(&table), 0, &explain);
        assert_eq!(trusted.value, 500_000);
        assert_eq!(trusted.source, "information_schema.TABLES.TABLE_ROWS");
        assert!(trusted.trusted);

        table.stats.table_rows = Some(0);
        let zero = compute_row_estimate(std::slice::from_ref(&table), 0, &explain);
        assert_eq!(zero.value, 120_000);
        assert_eq!(zero.source, "EXPLAIN.rows");
        assert!(zero.trusted);
        assert!(zero.reason.contains("TABLE_ROWS=0"));

        table.stats.table_rows = None;
        let none = compute_row_estimate(std::slice::from_ref(&table), 0, &[]);
        assert_eq!(none.value, 0);
        assert_eq!(none.source, "none");
        assert!(!none.trusted);
        assert!(none.reason.contains("行数不可知"));
    }

    #[test]
    fn analyze_audit_record_contains_sql_session_duration_and_result() {
        let record = analyze_audit_record(
            "db1",
            "app",
            Some("sess-1"),
            "SELECT * FROM orders WHERE status = 1",
            5000,
            42,
            "timeout",
            None,
            Some("maximum execution time exceeded".into()),
        );
        assert_eq!(record.event, "perf.explain_analyze");
        assert_eq!(record.outcome, "timeout");
        assert_eq!(record.db_id.as_deref(), Some("db1"));
        assert_eq!(record.db_name.as_deref(), Some("app"));
        let detail = &record.detail;
        assert_eq!(detail["session_id"], "sess-1");
        assert_eq!(detail["timeout_ms"], 5000);
        assert_eq!(detail["duration_ms"], 42);
        assert_eq!(detail["result"], "timeout");
        assert!(detail["target_sql_summary"]
            .as_str()
            .unwrap()
            .contains("SELECT * FROM orders"));
        assert_eq!(detail["error"], "maximum execution time exceeded");
    }

    #[test]
    fn analyze_timeout_error_is_recognized() {
        assert!(is_analyze_timeout(
            "Error 3024: Query execution was interrupted, maximum execution time exceeded"
        ));
        assert!(is_analyze_timeout("maximum execution time of 5000 ms exceeded"));
        assert!(!is_analyze_timeout("Access denied for user"));
    }
}
