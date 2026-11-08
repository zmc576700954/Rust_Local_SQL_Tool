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
        hash_json(self)
    }
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
        format!(
            "{}: access={} key={} rows={} extra={}",
            self.label,
            self.access_type.as_deref().unwrap_or("-"),
            self.key.as_deref().unwrap_or("-"),
            self.rows_examined.unwrap_or(-1),
            self.extra.as_deref().unwrap_or("-")
        )
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IndexUsage {
    pub index_name: String,
    pub count_star: Option<u64>,
    pub count_read: Option<u64>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationReport {
    pub before: Vec<ExplainSample>,
    pub after: Vec<ExplainSample>,
    pub improved_queries: usize,
    pub total_queries: usize,
    pub summary: String,
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
