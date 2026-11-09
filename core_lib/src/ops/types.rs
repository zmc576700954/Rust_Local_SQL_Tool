//! 运维底座统一类型：计划、结构化动作、证据、审批、执行与审计。
//!
//! 关键约束：Agent 只能产出 `ActionSpec`（结构化动作），不能产出可直接执行的
//! SQL 文本。SQL 一律由服务端 `ddl_compiler` 生成并校验。

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

fn now_ts() -> i64 {
    chrono::Utc::now().timestamp()
}

// ── 风险等级 ───────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskClass {
    /// 只读或纯诊断
    L0,
    /// 低风险在线变更（新增索引、ANALYZE）
    L1,
    /// 中风险（删除索引、普通 DML）
    L2,
    /// 高风险（需输入表名二次确认）
    L3,
    /// 禁止自动执行
    L4,
}

impl RiskClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::L0 => "L0",
            Self::L1 => "L1",
            Self::L2 => "L2",
            Self::L3 => "L3",
            Self::L4 => "L4",
        }
    }

    pub fn is_auto_executable(&self) -> bool {
        matches!(self, Self::L0 | Self::L1)
    }
}

// ── 结构化动作 ─────────────────────────────────────────────

/// 结构化动作。故意不提供 RawSql 变体，从类型层面阻断 Agent 直接下发 SQL。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ActionSpec {
    CreateIndex {
        table: String,
        name: String,
        columns: Vec<String>,
        #[serde(default)]
        unique: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        using: Option<String>,
    },
    DropIndex {
        table: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_definition: Option<String>,
    },
    AnalyzeTable {
        table: String,
    },
}

impl ActionSpec {
    /// 稳定的动作标识，用于审批绑定与执行结果关联。
    pub fn action_id(&self) -> String {
        match self {
            Self::CreateIndex { name, .. } => format!("create_index:{name}"),
            Self::DropIndex { name, .. } => format!("drop_index:{name}"),
            Self::AnalyzeTable { table } => format!("analyze_table:{table}"),
        }
    }

    pub fn table(&self) -> &str {
        match self {
            Self::CreateIndex { table, .. }
            | Self::DropIndex { table, .. }
            | Self::AnalyzeTable { table } => table,
        }
    }

    pub fn risk_class(&self) -> RiskClass {
        match self {
            Self::AnalyzeTable { .. } => RiskClass::L0,
            Self::CreateIndex { .. } => RiskClass::L1,
            Self::DropIndex { .. } => RiskClass::L2,
        }
    }
}

// ── 证据 ───────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ColumnSnapshot {
    pub name: String,
    pub column_type: String,
    pub nullable: bool,
    pub column_key: Option<String>,
    pub extra: Option<String>,
    pub character_set: Option<String>,
    pub collation: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IndexColumn {
    pub name: String,
    pub seq: i64,
    pub cardinality: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IndexDefinition {
    pub name: String,
    pub unique: bool,
    pub index_type: String,
    pub columns: Vec<IndexColumn>,
}

impl IndexDefinition {
    pub fn column_names(&self) -> Vec<String> {
        let mut cols = self.columns.clone();
        cols.sort_by_key(|c| c.seq);
        cols.into_iter().map(|c| c.name).collect()
    }

    /// existing index 是否已经覆盖 candidate 的前缀。
    pub fn covers_prefix(&self, candidate: &[String]) -> bool {
        let existing = self.column_names();
        if existing.is_empty() || candidate.is_empty() {
            return false;
        }
        let n = candidate.len().min(existing.len());
        existing[..n]
            .iter()
            .zip(candidate.iter())
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TableSnapshot {
    pub db_name: String,
    pub table_name: String,
    pub columns: Vec<ColumnSnapshot>,
    pub indexes: Vec<IndexDefinition>,
    pub create_table_sql: Option<String>,
}

impl TableSnapshot {
    pub fn find_column(&self, name: &str) -> Option<&ColumnSnapshot> {
        self.columns
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(name))
    }

    pub fn find_index(&self, name: &str) -> Option<&IndexDefinition> {
        self.indexes.iter().find(|i| i.name.eq_ignore_ascii_case(name))
    }

    pub fn snapshot_hash(&self) -> String {
        // 结构哈希只应对“结构”负责：
        // - MariaDB 的 SHOW CREATE TABLE 会带上当前自增值（AUTO_INCREMENT=n），写入数据后会变；
        // - 索引 CARDINALITY 是统计信息，随数据写入实时变化。
        // 两者都会让续跑误判结构漂移，这里统一剔除。
        let mut normalized = self.clone();
        if let Some(sql) = &normalized.create_table_sql {
            normalized.create_table_sql = Some(strip_auto_increment_counter(sql));
        }
        for index in &mut normalized.indexes {
            for column in &mut index.columns {
                column.cardinality = None;
            }
        }
        hash_json(&normalized)
    }
}

/// 去掉 SHOW CREATE TABLE 输出中的 `AUTO_INCREMENT=<数字>` 易变计数。
fn strip_auto_increment_counter(sql: &str) -> String {
    let lower = sql.to_ascii_lowercase();
    let needle = "auto_increment=";
    let mut out = String::with_capacity(sql.len());
    let mut cursor = 0;
    while let Some(offset) = lower[cursor..].find(needle) {
        let mut start = cursor + offset;
        let mut end = start + needle.len();
        while end < sql.len() && sql.as_bytes()[end].is_ascii_digit() {
            end += 1;
        }
        // 连同前导空格一起删掉，避免留下两个连续空格导致哈希仍不稳定。
        if start > 0 && sql.as_bytes()[start - 1] == b' ' {
            start -= 1;
        }
        out.push_str(&sql[cursor..start]);
        cursor = end;
    }
    out.push_str(&sql[cursor..]);
    out
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TableStats {
    pub engine: Option<String>,
    pub table_rows: Option<u64>,
    pub data_length: Option<u64>,
    pub index_length: Option<u64>,
    pub data_free: Option<u64>,
    pub auto_increment: Option<u64>,
    pub collation: Option<String>,
    /// information_schema.TABLES.UPDATE_TIME；InnoDB 可能为 NULL。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_time: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExplainSample {
    pub label: String,
    pub sql: String,
    pub access_type: Option<String>,
    pub key: Option<String>,
    pub possible_keys: Option<String>,
    pub rows_examined: Option<i64>,
    pub filtered: Option<f64>,
    pub extra: Option<String>,
    /// EXPLAIN 中的物理表名（多表查询时逐行记录）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table_name: Option<String>,
    /// EXPLAIN FORMAT=JSON 的 query_cost。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_cost: Option<f64>,
    /// EXPLAIN ANALYZE 的实际耗时（毫秒）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_time_ms: Option<f64>,
}

impl ExplainSample {
    /// 该查询是否值得新增索引（全表扫描 / 无可用索引 / 文件排序）。
    pub fn needs_index(&self) -> bool {
        let access = self.access_type.as_deref().unwrap_or("");
        let extra = self.extra.as_deref().unwrap_or("").to_ascii_lowercase();
        if self.key.as_deref().unwrap_or("").trim().is_empty() && self.possible_keys.is_none() {
            return true;
        }
        access.eq_ignore_ascii_case("ALL")
            || extra.contains("using filesort")
            || extra.contains("using temporary")
    }

    pub fn summary(&self) -> String {
        let mut base = format!(
            "{}: access={} key={} rows={} extra={}",
            self.label,
            self.access_type.as_deref().unwrap_or("-"),
            self.key.as_deref().unwrap_or("-"),
            self.rows_examined.unwrap_or(-1),
            self.extra.as_deref().unwrap_or("-")
        );
        if let Some(cost) = self.estimated_cost {
            base.push_str(&format!(" cost={cost:.2}"));
        }
        if let Some(ms) = self.actual_time_ms {
            base.push_str(&format!(" actual_ms={ms:.3}"));
        }
        base
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IndexUsage {
    pub index_name: String,
    pub count_star: Option<u64>,
    pub count_read: Option<u64>,
}

// ── 性能诊断证据 ───────────────────────────────────────────

/// 来自 performance_schema 的 digest 指标快照。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DigestInfo {
    pub digest: String,
    pub digest_text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_name: Option<String>,
    pub count_star: u64,
    /// 累计总延迟（毫秒）。
    pub total_latency_ms: f64,
    /// 平均延迟（毫秒）。
    pub avg_latency_ms: f64,
    pub rows_examined_sum: u64,
    pub rows_examined_avg: u64,
    pub rows_sent_avg: u64,
    pub rows_affected_avg: u64,
    pub created_tmp_tables: u64,
    pub created_tmp_disk_tables: u64,
    pub sort_rows: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_seen: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<String>,
}

/// 统计信息新鲜度评估。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StatsFreshness {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_time: Option<String>,
    /// mysql.innodb_table_stats.last_update（若无权限则为 None）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub innodb_last_update: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_days: Option<i64>,
    /// fresh / stale / unknown
    pub status: String,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PerfTableEvidence {
    pub table: String,
    pub snapshot: TableSnapshot,
    pub stats: TableStats,
    pub freshness: StatsFreshness,
}

/// 疑似隐式类型转换。只做确定性规则判定，结论需人工确认。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImplicitCastFinding {
    pub table: String,
    pub column: String,
    pub column_type: String,
    pub operator: String,
    /// string / number
    pub literal_kind: String,
    pub literal: String,
    /// 是否可能直接导致索引失效。
    pub index_risk: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PerfWindowInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_start: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_hours: Option<f64>,
    /// server_uptime / digest_first_seen / unknown
    pub source: String,
    /// 延迟与扫描数是否为累计值。
    pub cumulative: bool,
    pub note: String,
}

/// 索引动作依赖的行数估算。必须记录来源与是否可信。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RowEstimateEvidence {
    pub value: i64,
    /// information_schema.TABLES.TABLE_ROWS / explain.rows / none
    pub source: String,
    pub trusted: bool,
    pub reason: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PerfEvidence {
    pub target_sql: String,
    /// user / digest_sample
    pub sql_source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<DigestInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<PerfWindowInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row_estimate: Option<RowEstimateEvidence>,
    pub explain: Vec<ExplainSample>,
    pub tables: Vec<PerfTableEvidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_scan_rows: Option<i64>,
    pub implicit_casts: Vec<ImplicitCastFinding>,
    pub sort_or_temp: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_latency_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explain_analyze_raw: Option<String>,
    pub notes: Vec<String>,
    pub degraded_reasons: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EvidencePack {
    pub db_name: String,
    pub table: String,
    pub snapshot: TableSnapshot,
    pub stats: TableStats,
    pub explain: Vec<ExplainSample>,
    pub index_usage: Vec<IndexUsage>,
    pub workload_sql: Vec<String>,
    pub collected_at: i64,
    pub notes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub perf: Option<PerfEvidence>,
    /// 场景扩展证据（字段治理的 E01-E14）。只有该场景会填充；
    /// 结构固定为 `field_migration::FieldGovernanceAnalysis`，避免把本类型撑大。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra: Option<serde_json::Value>,
}

impl EvidencePack {
    pub fn evidence_hash(&self) -> String {
        hash_json(&EvidenceHashView {
            db_name: &self.db_name,
            table: &self.table,
            snapshot_hash: &self.snapshot.snapshot_hash(),
            stats: &self.stats,
            explain: &self.explain,
            index_usage: &self.index_usage,
            workload_sql: &self.workload_sql,
            perf: &self.perf,
            extra: &self.extra,
        })
    }
}

#[derive(Serialize)]
struct EvidenceHashView<'a> {
    db_name: &'a str,
    table: &'a str,
    snapshot_hash: &'a str,
    stats: &'a TableStats,
    explain: &'a [ExplainSample],
    index_usage: &'a [IndexUsage],
    workload_sql: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    perf: &'a Option<PerfEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    extra: &'a Option<serde_json::Value>,
}

// ── 计划模型 ───────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warn,
    Critical,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    pub id: String,
    pub title: String,
    pub detail: String,
    pub severity: Severity,
    #[serde(default)]
    pub evidence_refs: Vec<String>,
    pub confidence: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskFinding {
    pub id: String,
    pub title: String,
    pub detail: String,
    pub severity: Severity,
    pub blocking: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationStatus {
    Pass,
    Warn,
    Fail,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationCheck {
    pub id: String,
    pub title: String,
    pub status: ValidationStatus,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollbackPlan {
    pub summary: String,
    pub actions: Vec<ActionSpec>,
    pub automatic: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EvidenceSummary {
    pub table: String,
    pub column_count: usize,
    pub index_count: usize,
    pub row_estimate: Option<u64>,
    pub explain_findings: Vec<String>,
}

/// SQL 改写建议：纯文本产物，永不进入 ActionSpec。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RewriteSuggestion {
    pub id: String,
    pub title: String,
    pub suggestion: String,
    pub rationale: String,
    pub expected_gain: String,
    #[serde(default)]
    pub evidence_refs: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    PendingApproval,
    Approved,
    Executing,
    Executed,
    Failed,
    RolledBack,
    RollbackFailed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpsPlan {
    pub plan_id: String,
    pub scenario_id: String,
    pub db_id: String,
    pub db_name: String,
    pub db_fingerprint: String,
    pub evidence_hash: String,
    pub diagnosis: String,
    pub findings: Vec<Finding>,
    pub actions: Vec<ActionSpec>,
    /// 执行后用于复测的 SQL 样本（只读）。
    pub verification_sql: Vec<String>,
    pub risks: Vec<RiskFinding>,
    pub validation_checks: Vec<ValidationCheck>,
    pub rollback: RollbackPlan,
    pub code_advice: Vec<String>,
    /// 结构化 SQL 改写建议。只有文本，不是可执行动作。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rewrite_suggestions: Vec<RewriteSuggestion>,
    /// 复测时是否使用 EXPLAIN ANALYZE（仅性能诊断）。
    #[serde(default)]
    pub verification_explain_analyze: bool,
    pub confidence: f32,
    pub requires_approval: bool,
    pub created_at: i64,
    pub plan_hash: String,
    pub status: PlanStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_summary: Option<EvidenceSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<ExecutionReport>,
}

impl OpsPlan {
    /// 计划哈希只覆盖不可变的核心内容（数据库指纹、证据、动作、回滚），
    /// 不包含状态和执行结果，避免审批后被状态更新影响。
    pub fn compute_hash(&self) -> String {
        hash_json(&PlanHashView {
            plan_id: &self.plan_id,
            scenario_id: &self.scenario_id,
            db_id: &self.db_id,
            db_name: &self.db_name,
            db_fingerprint: &self.db_fingerprint,
            evidence_hash: &self.evidence_hash,
            actions: &self.actions,
            rollback_actions: &self.rollback.actions,
            verification_sql: &self.verification_sql,
        })
    }

    pub fn seal(mut self) -> Self {
        self.plan_hash = self.compute_hash();
        self
    }

    pub fn refresh_hash(&mut self) {
        self.plan_hash = self.compute_hash();
    }

    pub fn action_by_id(&self, action_id: &str) -> Option<&ActionSpec> {
        self.actions.iter().find(|a| a.action_id() == action_id)
    }

    pub fn summary(&self) -> PlanSummary {
        PlanSummary {
            plan_id: self.plan_id.clone(),
            scenario_id: self.scenario_id.clone(),
            db_name: self.db_name.clone(),
            table: self
                .evidence_summary
                .as_ref()
                .map(|s| s.table.clone())
                .unwrap_or_default(),
            action_count: self.actions.len(),
            status: self.status,
            created_at: self.created_at,
            plan_hash: self.plan_hash.clone(),
            diagnosis: self.diagnosis.clone(),
        }
    }
}

#[derive(Serialize)]
struct PlanHashView<'a> {
    plan_id: &'a str,
    scenario_id: &'a str,
    db_id: &'a str,
    db_name: &'a str,
    db_fingerprint: &'a str,
    evidence_hash: &'a str,
    actions: &'a [ActionSpec],
    rollback_actions: &'a [ActionSpec],
    verification_sql: &'a [String],
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanSummary {
    pub plan_id: String,
    pub scenario_id: String,
    pub db_name: String,
    pub table: String,
    pub action_count: usize,
    pub status: PlanStatus,
    pub created_at: i64,
    pub plan_hash: String,
    pub diagnosis: String,
}

// ── 编译 / 执行 ─────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompiledAction {
    pub action_id: String,
    pub action: ActionSpec,
    pub risk_class: RiskClass,
    pub sql: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollback_sql: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compensation: Option<ActionSpec>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImpactAssessment {
    pub estimated_rows: Option<u64>,
    pub estimated_index_bytes: Option<u64>,
    pub metadata_lock: String,
    pub write_amplification: String,
    pub blocking: bool,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionState {
    Pending,
    Running,
    Succeeded,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionExecution {
    pub action_id: String,
    pub sql: String,
    pub state: ActionState,
    pub started_at: i64,
    pub finished_at: i64,
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollback_sql: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QueryComparison {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_access: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_access: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_rows: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_rows: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_cost: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_cost: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_actual_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_actual_ms: Option<f64>,
    pub improved: bool,
    pub verdict: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationReport {
    pub before: Vec<ExplainSample>,
    pub after: Vec<ExplainSample>,
    pub improved_queries: usize,
    pub total_queries: usize,
    pub summary: String,
    #[serde(default)]
    pub comparisons: Vec<QueryComparison>,
    #[serde(default)]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionReport {
    pub plan_id: String,
    pub dry_run: bool,
    pub started_at: i64,
    pub finished_at: i64,
    pub actions: Vec<ActionExecution>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub impact: Option<ImpactAssessment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification: Option<VerificationReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub rollback_performed: bool,
    pub compiled_sql: Vec<String>,
}

// ── 审批 ───────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRecord {
    pub approval_id: String,
    pub plan_id: String,
    pub plan_hash: String,
    pub db_fingerprint: String,
    pub action_ids: Vec<String>,
    pub token_hash: String,
    pub issued_at: i64,
    pub expires_at: i64,
    pub consumed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumed_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumed_by: Option<String>,
}

// ── 审计 ───────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditRecord {
    pub id: String,
    pub timestamp: i64,
    pub event: String,
    pub outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub db_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub db_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    #[serde(default)]
    pub detail: serde_json::Value,
}

impl AuditRecord {
    pub fn new(event: &str, outcome: &str) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: now_ts(),
            event: event.to_string(),
            outcome: outcome.to_string(),
            plan_id: None,
            db_id: None,
            db_name: None,
            action_id: None,
            actor: None,
            detail: serde_json::Value::Null,
        }
    }
}

// ── 工具函数 ───────────────────────────────────────────────

pub fn hash_json<T: Serialize>(value: &T) -> String {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

pub fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    format!("{:x}", hasher.finalize())
}

pub fn now() -> i64 {
    now_ts()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(create_sql: &str) -> TableSnapshot {
        TableSnapshot {
            db_name: "e2e".into(),
            table_name: "t".into(),
            columns: Vec::new(),
            indexes: Vec::new(),
            create_table_sql: Some(create_sql.into()),
        }
    }

    #[test]
    fn snapshot_hash_ignores_index_cardinality() {
        let mut fresh = snapshot("CREATE TABLE `t` (`id` int, KEY `idx` (`id`)) ENGINE=InnoDB");
        fresh.indexes.push(IndexDefinition {
            name: "idx".into(),
            unique: false,
            index_type: "BTREE".into(),
            columns: vec![IndexColumn {
                name: "id".into(),
                seq: 1,
                cardinality: Some(0),
            }],
        });
        let mut grown = fresh.clone();
        grown.indexes[0].columns[0].cardinality = Some(100_000);
        assert_eq!(fresh.snapshot_hash(), grown.snapshot_hash());
    }

    #[test]
    fn snapshot_hash_ignores_auto_increment_counter() {
        let fresh = snapshot(
            "CREATE TABLE `t` (`id` int NOT NULL AUTO_INCREMENT, PRIMARY KEY (`id`)) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4",
        );
        let grown = snapshot(
            "CREATE TABLE `t` (`id` int NOT NULL AUTO_INCREMENT, PRIMARY KEY (`id`)) ENGINE=InnoDB AUTO_INCREMENT=42 DEFAULT CHARSET=utf8mb4",
        );
        assert_eq!(fresh.snapshot_hash(), grown.snapshot_hash());
    }

    #[test]
    fn snapshot_hash_still_detects_real_ddl_change() {
        let v1 = snapshot(
            "CREATE TABLE `t` (`id` int NOT NULL AUTO_INCREMENT, `v` int DEFAULT NULL, PRIMARY KEY (`id`)) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4",
        );
        let v2 = snapshot(
            "CREATE TABLE `t` (`id` int NOT NULL AUTO_INCREMENT, `v` bigint DEFAULT NULL, PRIMARY KEY (`id`)) ENGINE=InnoDB AUTO_INCREMENT=7 DEFAULT CHARSET=utf8mb4",
        );
        assert_ne!(v1.snapshot_hash(), v2.snapshot_hash());
    }
}
