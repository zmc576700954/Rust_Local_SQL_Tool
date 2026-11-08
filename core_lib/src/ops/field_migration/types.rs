//! 字段治理（S1 只读切片）的数据结构。
//!
//! 本模块只承载证据/风险/脚本/声明的可序列化模型；不包含任何写库路径。
//! `FieldGovernanceAnalysis` 会整体序列化进 `EvidencePack.extra`，因此它必须
//! 只包含确定性内容（时间戳放在 `EvidencePack.collected_at`）。

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use crate::ops::types::hash_json;

use super::schema::TargetColumn;

// ── 空串策略 ───────────────────────────────────────────────

/// 文档 4.2(1)：SQL NULL / 空串 / JSON null 三种语义不可互换。
/// 默认 `preserve`，任何转换都必须是显式选择，不允许静默抹平。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmptyStringPolicy {
    /// 保留空串（默认）；回填与校验都按原文比较。
    #[default]
    Preserve,
    /// 显式把空串转成 NULL；高风险，需要用户确认。
    ToNull,
    /// 命中空串即阻断，禁止进入写步骤。
    Block,
}

impl EmptyStringPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Preserve => "preserve",
            Self::ToNull => "to_null",
            Self::Block => "block",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Preserve => "保留空串",
            Self::ToNull => "空串转 NULL",
            Self::Block => "命中空串即阻断",
        }
    }
}

// ── 输入 ───────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FieldGovernanceInput {
    /// 源表；为空由 handler 校验拒绝。
    pub table: String,
    /// 源列；为空表示整表字段盘点。
    #[serde(default)]
    pub column: Option<String>,
    /// 目标表与列映射；缺省时按 JSON key 推断草案。
    #[serde(default)]
    pub target: Option<TargetSpec>,
    /// 用户语义描述。每一项都会被标记为 `source=user, verified=false`。
    #[serde(default)]
    pub declarations: Vec<SemanticDeclarationInput>,
    /// auto / full / sampled。
    #[serde(default)]
    pub profile_mode: Option<String>,
    /// sampled 模式的确定性抽样比例（千分比），受策略上限约束。
    #[serde(default)]
    pub sample_per_mille: Option<u32>,
    /// 空串处理策略；缺省为 preserve（保留空串）。
    #[serde(default)]
    pub empty_string_policy: Option<EmptyStringPolicy>,
    /// S3 下线准备：观察期起始日期（YYYY-MM-DD）。
    /// 不传时无法自动判定观察期，清单会标记为「必须人工确认」。
    #[serde(default)]
    pub observation_started_at: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
}

impl FieldGovernanceInput {
    pub fn normalized(mut self) -> Self {
        self.table = self.table.trim().to_string();
        self.column = self
            .column
            .map(|c| c.trim().to_string())
            .filter(|c| !c.is_empty());
        self.profile_mode = self
            .profile_mode
            .map(|m| m.trim().to_ascii_lowercase())
            .filter(|m| !m.is_empty());
        self.sample_per_mille = self.sample_per_mille.map(|v| v.clamp(1, 1000));
        self.observation_started_at = self
            .observation_started_at
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        self.notes = self
            .notes
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty());
        if let Some(target) = self.target.take() {
            self.target = Some(target.normalized());
        }
        self
    }

    pub fn empty_string_policy(&self) -> EmptyStringPolicy {
        self.empty_string_policy.unwrap_or_default()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TargetSpec {
    #[serde(default)]
    pub table: Option<String>,
    /// 目标主键列名；默认沿用源表单列主键名。
    #[serde(default)]
    pub pk_column: Option<String>,
    /// 目标表排序规则；用于 E13 比较语义检查。
    #[serde(default)]
    pub collation: Option<String>,
    /// 无法映射的 JSON key 是否允许落到 residual JSON 列。
    #[serde(default)]
    pub residual_json: Option<bool>,
    #[serde(default)]
    pub mappings: Vec<ColumnMapping>,
}

impl TargetSpec {
    pub fn normalized(mut self) -> Self {
        self.table = self
            .table
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty());
        self.pk_column = self
            .pk_column
            .map(|c| c.trim().to_string())
            .filter(|c| !c.is_empty());
        self.collation = self
            .collation
            .map(|c| c.trim().to_string())
            .filter(|c| !c.is_empty());
        self.mappings = self
            .mappings
            .into_iter()
            .filter_map(|m| {
                let target_column = m.target_column.trim().to_string();
                if target_column.is_empty() {
                    return None;
                }
                Some(ColumnMapping {
                    source_key: m
                        .source_key
                        .map(|k| k.trim().to_string())
                        .filter(|k| !k.is_empty()),
                    target_column,
                    target_type: m
                        .target_type
                        .map(|t| t.trim().to_string())
                        .filter(|t| !t.is_empty()),
                    required: m.required,
                })
            })
            .collect();
        self
    }

    pub fn residual_json(&self) -> bool {
        self.residual_json.unwrap_or(false)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ColumnMapping {
    /// JSON key；为空表示源列整体映射到目标列（标量场景）。
    #[serde(default)]
    pub source_key: Option<String>,
    pub target_column: String,
    #[serde(default)]
    pub target_type: Option<String>,
    #[serde(default)]
    pub required: Option<bool>,
}

/// 用户语义描述输入。字段都可选，避免强制用户填完整契约。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SemanticDeclarationInput {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub column: Option<String>,
    #[serde(default)]
    pub purpose: Option<String>,
    /// object / array / scalar
    #[serde(default)]
    pub expected_top_level: Option<String>,
    #[serde(default)]
    pub expected_keys: Vec<String>,
    #[serde(default)]
    pub required_keys: Vec<String>,
    #[serde(default)]
    pub value_domain: HashMap<String, String>,
    #[serde(default)]
    pub target_nullable: Option<bool>,
    /// null / empty / invalid
    #[serde(default)]
    pub empty_string_means: Option<String>,
    #[serde(default)]
    pub unique_keys: Vec<String>,
    #[serde(default)]
    pub uniqueness_expected: Option<bool>,
    #[serde(default)]
    pub consumers: Vec<String>,
    #[serde(default)]
    pub write_owner: Option<String>,
    /// stop_write / dual_write
    #[serde(default)]
    pub cutover_mode: Option<String>,
    #[serde(default)]
    pub cutover_hours: Option<u64>,
    #[serde(default)]
    pub retention_days: Option<u64>,
}

// ── 声明（归一化后） ────────────────────────────────────────

/// 归一化后的声明：始终 `source=user`、`verified=false`。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Declaration {
    pub id: String,
    pub source: String,
    pub verified: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_top_level: Option<String>,
    #[serde(default)]
    pub expected_keys: Vec<String>,
    #[serde(default)]
    pub required_keys: Vec<String>,
    #[serde(default)]
    pub value_domain: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_nullable: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub empty_string_means: Option<String>,
    #[serde(default)]
    pub unique_keys: Vec<String>,
    #[serde(default)]
    pub uniqueness_expected: bool,
    #[serde(default)]
    pub consumers: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cutover_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cutover_hours: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention_days: Option<u64>,
}

impl Declaration {
    pub fn normalize(input: SemanticDeclarationInput, index: usize) -> Self {
        let id = input
            .id
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| format!("decl-{}", index + 1));
        let mut value_domain = BTreeMap::new();
        for (key, value) in input.value_domain {
            let key = key.trim().to_string();
            if !key.is_empty() {
                value_domain.insert(key, value.trim().to_string());
            }
        }
        Self {
            id,
            source: "user".into(),
            verified: false,
            column: input
                .column
                .map(|c| c.trim().to_string())
                .filter(|c| !c.is_empty()),
            purpose: input
                .purpose
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty()),
            expected_top_level: input
                .expected_top_level
                .map(|t| t.trim().to_ascii_lowercase())
                .filter(|t| !t.is_empty()),
            expected_keys: clean_keys(input.expected_keys),
            required_keys: clean_keys(input.required_keys),
            value_domain,
            target_nullable: input.target_nullable,
            empty_string_means: input
                .empty_string_means
                .map(|m| m.trim().to_ascii_lowercase())
                .filter(|m| matches!(m.as_str(), "null" | "empty" | "invalid")),
            unique_keys: clean_keys(input.unique_keys),
            uniqueness_expected: input.uniqueness_expected.unwrap_or(false),
            consumers: input
                .consumers
                .into_iter()
                .map(|c| c.trim().to_string())
                .filter(|c| !c.is_empty())
                .collect(),
            write_owner: input
                .write_owner
                .map(|w| w.trim().to_string())
                .filter(|w| !w.is_empty()),
            cutover_mode: input
                .cutover_mode
                .map(|m| m.trim().to_ascii_lowercase())
                .filter(|m| matches!(m.as_str(), "stop_write" | "dual_write")),
            cutover_hours: input.cutover_hours,
            retention_days: input.retention_days,
        }
    }
}

fn clean_keys(keys: Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    for key in keys {
        let key = key.trim().to_string();
        if !key.is_empty() && !out.contains(&key) {
            out.push(key);
        }
    }
    out
}

pub fn normalize_declarations(inputs: Vec<SemanticDeclarationInput>) -> Vec<Declaration> {
    inputs
        .into_iter()
        .enumerate()
        .map(|(index, input)| Declaration::normalize(input, index))
        .collect()
}

pub fn declaration_hash(declarations: &[Declaration]) -> String {
    hash_json(&declarations.to_vec())
}

// ── 证据 ───────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceItem {
    pub id: String,
    pub title: String,
    /// collected / partial / missing
    pub status: String,
    /// structural / full / sampled / none
    pub mode: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sql: Vec<String>,
    #[serde(default)]
    pub details: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceGap {
    pub evidence_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KeyPresence {
    pub key: String,
    pub rows: u64,
    pub ratio: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KeyTypeDistribution {
    pub key: String,
    /// SQL JSON 类型（STRING/INTEGER/DOUBLE/BOOLEAN/NULL/OBJECT/ARRAY）→ 行数。
    pub types: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ColumnProfile {
    pub column: String,
    pub data_type: String,
    pub column_type: String,
    pub nullable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub charset: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub char_max_length: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub numeric_precision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub numeric_scale: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column_default: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation_expression: Option<String>,
    /// full / sampled / none
    pub profile_mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_per_mille: Option<u32>,
    /// 剖面覆盖的行数（sampled 时为样本行数）。
    pub rows_profiled: u64,
    pub null_rows: u64,
    pub empty_rows: u64,
    pub max_char_length: u64,
    pub max_byte_length: u64,
    pub avg_char_length: f64,
    pub untrimmed_rows: u64,
    pub control_char_rows: u64,
    pub distinct_rows: u64,
    pub distinct_binary_rows: u64,
    pub json_valid_rows: u64,
    pub json_invalid_rows: u64,
    pub json_object_rows: u64,
    pub json_array_rows: u64,
    pub json_scalar_rows: u64,
    pub json_other_rows: u64,
    #[serde(default)]
    pub json_keys: Vec<String>,
    #[serde(default)]
    pub json_key_presence: Vec<KeyPresence>,
    #[serde(default)]
    pub json_key_types: Vec<KeyTypeDistribution>,
    pub json_profiled: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

impl ColumnProfile {
    pub fn is_text_like(&self) -> bool {
        matches!(
            self.data_type.as_str(),
            "json"
                | "char"
                | "varchar"
                | "tinytext"
                | "text"
                | "mediumtext"
                | "longtext"
        )
    }

    pub fn is_json_type(&self) -> bool {
        self.data_type.eq_ignore_ascii_case("json")
    }

    pub fn json_key_presence(&self, key: &str) -> Option<&KeyPresence> {
        self.json_key_presence
            .iter()
            .find(|p| p.key.eq_ignore_ascii_case(key))
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IndexReference {
    pub name: String,
    pub unique: bool,
    pub primary: bool,
    pub index_type: String,
    pub columns: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ForeignKeyReference {
    pub constraint_name: String,
    /// child / parent
    pub direction: String,
    pub table: String,
    pub column: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub referenced_table: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub referenced_column: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GeneratedColumnReference {
    pub name: String,
    pub expression: String,
    pub extra: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DependencyObject {
    /// view / trigger / routine
    pub kind: String,
    pub name: String,
    #[serde(default)]
    pub matched_columns: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DependencyEvidence {
    #[serde(default)]
    pub primary_key_columns: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub single_column_pk: Option<String>,
    #[serde(default)]
    pub indexes: Vec<IndexReference>,
    #[serde(default)]
    pub foreign_keys: Vec<ForeignKeyReference>,
    #[serde(default)]
    pub generated_columns: Vec<GeneratedColumnReference>,
    #[serde(default)]
    pub objects: Vec<DependencyObject>,
    /// 源表上的任意触发器（不区分是否匹配目标列）。
    #[serde(default)]
    pub source_triggers: Vec<String>,
    /// 所有依赖查询都成功完成时才为 true。
    pub scan_complete: bool,
    #[serde(default)]
    pub gaps: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EnvironmentEvidence {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_comment: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table_collation: Option<String>,
    pub partitioned: bool,
    pub read_only: bool,
    #[serde(default)]
    pub long_transactions: u64,
    #[serde(default)]
    pub version_supports_json: bool,
}

// ── 声明验证证据 ───────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeclarationEvidence {
    pub declaration_id: String,
    /// uniqueness / required_key / shape
    pub kind: String,
    #[serde(default)]
    pub key_set: Vec<String>,
    pub rows: u64,
    #[serde(default)]
    pub detail: String,
}

// ── 风险 ───────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    Info,
    High,
    Block,
}

impl RiskLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::High => "high",
            Self::Block => "block",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldRisk {
    pub id: String,
    /// 稳定规则码，UI/测试只断言 code，不断言文案。
    pub code: String,
    pub level: RiskLevel,
    pub title: String,
    pub detail: String,
    #[serde(default)]
    pub evidence_refs: Vec<String>,
    /// 是否阻断进入任何写步骤（S1 没有写步骤，但必须显式标注）。
    pub blocks_write: bool,
    /// 需要用户显式确认（HIGH 项；S2 起才参与状态机门禁）。
    pub requires_user_confirmation: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RiskSummary {
    pub block: usize,
    pub high: usize,
    pub info: usize,
    /// 任一证据缺口或抽样/未采集导致写步骤不可判定。
    pub undetermined_write_block: bool,
    /// 规则引擎计算出的风险下限（block > high > info）。
    pub risk_floor: String,
}

// ── 脚本 / dry-run ─────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManualScript {
    pub id: String,
    pub title: String,
    /// create_table / backfill / verify / rollback / retire
    pub kind: String,
    /// analyzed / table_created / backfill / verify / retire
    pub phase: String,
    pub sql: String,
    /// S1 固定为 false：脚本只生成，不执行。
    pub executable: bool,
    /// 执行后是否不可回滚（必须提前声明）。
    pub irreversible: bool,
    #[serde(default)]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DryRunCheck {
    pub script_id: String,
    pub check: String,
    pub status: String,
    pub detail: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DryRunReport {
    /// 只做只读解析与 SQL 展示；不连接执行。
    pub executed_against_database: bool,
    pub passed: bool,
    pub checks: Vec<DryRunCheck>,
    pub note: String,
}

// ── S3：下线准备 / 回滚评估 / 双写方案（只输出清单与脚本）──

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RetireChecklistItem {
    pub id: String,
    /// dependency / observation / backup / gate / timing
    pub category: String,
    pub title: String,
    pub detail: String,
    /// true = 已自动核实；false = 必须人工确认。
    pub auto_verified: bool,
    /// pass / fail / warn / unknown / manual
    pub status: String,
    /// 该项未满足时是否应阻断下线（仅作为清单结论，工具不执行任何动作）。
    pub blocks_retire: bool,
    #[serde(default)]
    pub evidence_refs: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RetirePackage {
    /// 指定源列后才生成；整表盘点模式为 false。
    pub generated: bool,
    pub table: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<String>,
    pub target_table: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_table: Option<String>,
    pub observation_required_days: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation_elapsed_days: Option<u64>,
    pub observation_satisfied: bool,
    pub observation_note: String,
    pub snapshot_retention_days: u64,
    pub rollback_window_hours: u64,
    #[serde(default)]
    pub blockers: Vec<String>,
    pub auto_verified_count: usize,
    pub manual_confirmation_count: usize,
    #[serde(default)]
    pub checklist: Vec<RetireChecklistItem>,
    #[serde(default)]
    pub execution_order: Vec<String>,
    pub recommended_window: String,
    pub irreversible: bool,
    pub note: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RollbackAssessment {
    /// scalar_1_to_1 / json_split_1_to_1 / json_1_to_n / none
    pub shape: String,
    /// reversible / rebuildable_semantic_equivalent / irreversible / not_applicable
    pub conclusion: String,
    pub summary: String,
    #[serde(default)]
    pub reasons: Vec<String>,
    #[serde(default)]
    pub prerequisites: Vec<String>,
    #[serde(default)]
    pub lossy_aspects: Vec<String>,
    /// 明确不可还原的内容。`rebuildable_semantic_equivalent` 时必须非空，
    /// 避免用户把「语义等价重建」理解成一键还原。
    #[serde(default)]
    pub non_restorable: Vec<String>,
    pub irreversible: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reverse_sql: Option<String>,
    pub note: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DualWriteMetric {
    pub name: String,
    pub target: String,
    pub alert_threshold: String,
    pub rationale: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DualWritePlan {
    /// 固定 proposal_only：只出方案，不编排应用切换、不改用户代码。
    pub mode: String,
    #[serde(default)]
    pub steps: Vec<String>,
    #[serde(default)]
    pub switch_point: Vec<String>,
    #[serde(default)]
    pub metrics: Vec<DualWriteMetric>,
    #[serde(default)]
    pub rollback_conditions: Vec<String>,
    pub note: String,
}

// ── 汇总 ───────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FieldGovernanceAnalysis {
    pub db_name: String,
    pub table: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<String>,
    pub analyzed_columns: Vec<String>,
    /// full / sampled / none
    pub profile_mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_per_mille: Option<u32>,
    pub estimated_rows: Option<u64>,
    pub data_length: Option<u64>,
    pub index_length: Option<u64>,
    /// 实际 COUNT(*)（full 模式）或估算值（sampled/none）。
    pub row_count: u64,
    pub columns: Vec<ColumnProfile>,
    pub dependencies: DependencyEvidence,
    pub environment: EnvironmentEvidence,
    pub evidence: Vec<EvidenceItem>,
    pub gaps: Vec<EvidenceGap>,
    #[serde(default)]
    pub declarations: Vec<Declaration>,
    pub declaration_hash: String,
    #[serde(default)]
    pub declaration_evidence: Vec<DeclarationEvidence>,
    pub target_table: String,
    pub target_exists: bool,
    /// 本次方案采用的空串策略（默认 preserve）。
    #[serde(default)]
    pub empty_string_policy: EmptyStringPolicy,
    /// 脚本实际使用的目标列（含类型），供只读分页校验复用。
    #[serde(default)]
    pub target_columns: Vec<TargetColumn>,
    pub risks: Vec<FieldRisk>,
    pub risk_summary: RiskSummary,
    #[serde(default)]
    pub scripts: Vec<ManualScript>,
    pub dry_run: DryRunReport,
    /// S3：下线准备清单（只提示，不执行）。
    #[serde(default)]
    pub retire_package: RetirePackage,
    /// S3：从目标表反写回源列的可回滚性评估。
    #[serde(default)]
    pub rollback: RollbackAssessment,
    /// S3：应用层双写方案（只出方案）。
    #[serde(default)]
    pub dual_write: DualWritePlan,
}

impl FieldGovernanceAnalysis {
    pub fn selected_profile(&self) -> Option<&ColumnProfile> {
        let column = self.column.as_deref()?;
        self.columns
            .iter()
            .find(|c| c.column.eq_ignore_ascii_case(column))
    }

    pub fn has_block(&self) -> bool {
        self.risks
            .iter()
            .any(|risk| risk.level == RiskLevel::Block)
    }
}

// ── 单测基座 ───────────────────────────────────────────────

#[cfg(test)]
pub fn sample_column(column: &str, data_type: &str) -> ColumnProfile {
    ColumnProfile {
        column: column.into(),
        data_type: data_type.into(),
        column_type: format!("{data_type}(64)"),
        nullable: true,
        profile_mode: "full".into(),
        rows_profiled: 100,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declarations_are_always_unverified_user_source() {
        let declarations = normalize_declarations(vec![SemanticDeclarationInput {
            purpose: Some("只存用户偏好".into()),
            required_keys: vec!["theme".into(), "theme".into()],
            ..Default::default()
        }]);
        assert_eq!(declarations.len(), 1);
        assert_eq!(declarations[0].id, "decl-1");
        assert_eq!(declarations[0].source, "user");
        assert!(!declarations[0].verified);
        assert_eq!(declarations[0].required_keys, vec!["theme"]);
    }

    #[test]
    fn declaration_hash_is_stable_and_changes_with_content() {
        let a = normalize_declarations(vec![SemanticDeclarationInput {
            purpose: Some("只存用户偏好".into()),
            ..Default::default()
        }]);
        let b = normalize_declarations(vec![SemanticDeclarationInput {
            purpose: Some("只存用户偏好".into()),
            ..Default::default()
        }]);
        assert_eq!(declaration_hash(&a), declaration_hash(&b));

        let c = normalize_declarations(vec![SemanticDeclarationInput {
            purpose: Some("允许订单字段".into()),
            ..Default::default()
        }]);
        assert_ne!(declaration_hash(&a), declaration_hash(&c));
    }

    #[test]
    fn risk_level_orders_block_above_high_above_info() {
        assert!(RiskLevel::Block > RiskLevel::High);
        assert!(RiskLevel::High > RiskLevel::Info);
    }
}
