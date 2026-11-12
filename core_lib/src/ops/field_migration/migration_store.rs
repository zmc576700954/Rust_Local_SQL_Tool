//! S2 字段回填任务与 checkpoint 持久化。
//!
//! 本模块只负责状态、checkpoint、进度与控制信号；不包含任何 SQL 执行逻辑。
//! 任务文件采用原子替换写入（先写 `.tmp` 再 rename），每批校验通过后落盘。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::ops::error::OpsError;
use crate::ops::types::now;

use super::schema::TargetColumn;
use super::types::EmptyStringPolicy;

/// 主键比较方式。MySQL 只使用 `Integer` / `Text`；PG 另外支持 `Numeric` / `Uuid`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PkKind {
    Integer,
    Numeric,
    Text,
    Uuid,
}

/// 旧任务/checkpoint 都来自 MySQL；缺省按 mysql 处理，PG 续跑看到该值会拒绝。
pub fn default_engine_mysql() -> String {
    "mysql".to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrationStatus {
    Pending,
    Running,
    Paused,
    /// 进程/连接异常中断，可从 checkpoint 续跑。
    Interrupted,
    /// 校验或策略阻断；不继续后续批次。
    Blocked,
    Completed,
    Aborted,
}

impl MigrationStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Interrupted => "interrupted",
            Self::Blocked => "blocked",
            Self::Completed => "completed",
            Self::Aborted => "aborted",
        }
    }

    pub fn is_finished(self) -> bool {
        matches!(self, Self::Completed | Self::Aborted)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrationPhase {
    Pending,
    Preflight,
    Backfilling,
    Verifying,
    Completed,
    Paused,
    Interrupted,
    Blocked,
    Aborted,
}

/// 回填计划配置。执行时使用 normalized 后的值，并随任务落盘。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackfillConfig {
    /// 每批行数。
    pub batch_size: u64,
    /// 每批结束后的固定间隔（毫秒）。
    pub batch_interval_ms: u64,
    /// 可选的行/秒上限；0 表示不限制。
    pub max_rows_per_second: u64,
    /// 单事务模式：仅在 PG 生效；整任务一个事务，失败整体回滚且不可续跑。
    #[serde(default)]
    pub single_transaction: bool,
}

impl Default for BackfillConfig {
    fn default() -> Self {
        Self {
            batch_size: 1000,
            batch_interval_ms: 0,
            max_rows_per_second: 0,
            single_transaction: false,
        }
    }
}

/// 回填执行所需的不可变规格。只读分析产物在这里收紧为可执行参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackfillSpec {
    pub source_table: String,
    pub source_column: String,
    pub target_table: String,
    /// 源与目标主键列名必须一致；S2 暂不支持重命名主键。
    pub pk_column: String,
    /// 复合主键列（顺序即行比较顺序）；空 = 单列，沿用 `pk_column`。
    /// 仅 PG 任务写入，MySQL 任务恒为空（JSON 字节不变）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pk_columns: Vec<String>,
    #[serde(default)]
    pub pk_kind: Option<PkKind>,
    /// 与 `pk_columns` 等长的逐列比较方式；单列任务为空。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pk_kinds: Vec<PkKind>,
    pub target_columns: Vec<TargetColumn>,
    pub empty_string_policy: EmptyStringPolicy,
    /// L2 建表脚本；仅在目标表不存在时执行一次（CREATE TABLE IF NOT EXISTS）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create_table_sql: Option<String>,
    /// PG schema；MySQL 为 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    /// 源列 data_type（PG 表达式重建用）；MySQL 可为空。
    #[serde(default)]
    pub source_data_type: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchVerify {
    pub source_rows: u64,
    pub target_rows: u64,
    pub v2: u64,
    pub v3: u64,
    pub v4: u64,
    /// PG 同库校验写 "snapshot"；MySQL 保持空串。
    #[serde(default)]
    pub validation_mode: String,
}

impl BatchVerify {
    pub fn passed(&self) -> bool {
        self.source_rows == self.target_rows && self.v2 == 0 && self.v3 == 0 && self.v4 == 0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    /// 已完成并校验通过的批次数。
    pub completed_batches: u64,
    /// 已完成的批边界主键；下一批严格大于该主键。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_pk: Option<String>,
    /// 复合主键边界（与 `pk_columns` 同序）；单列任务为 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_pks: Option<Vec<String>>,
    /// 累计成功写入/校验行数。
    pub written_rows: u64,
    pub verified: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify: Option<BatchVerify>,
    /// 产出该 checkpoint 的引擎；旧文件缺省为 mysql。
    #[serde(default = "default_engine_mysql")]
    pub engine: String,
    /// 主键比较方式；续跑时不一致即拒绝。
    #[serde(default)]
    pub pk_kind: Option<PkKind>,
    /// 复合主键逐列比较方式；单列任务为空。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pk_kinds: Vec<PkKind>,
    /// 目标表结构指纹；续跑时用于确认目标未被替换。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_fingerprint: Option<String>,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchRecord {
    /// 1 基批次号，界面直接展示。
    pub batch_no: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_pk: Option<String>,
    /// 复合主键批首边界（与 `pk_columns` 同序）；单列任务为 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_pks: Option<Vec<String>>,
    pub end_pk: String,
    /// 复合主键批尾（与 `pk_columns` 同序）；单列任务为 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_pks: Option<Vec<String>>,
    pub source_rows: u64,
    pub written_rows: u64,
    pub verified: bool,
    pub verify: BatchVerify,
    pub duration_ms: u64,
    pub at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffDetail {
    /// v1 / v2 / v3 / v4
    pub phase: String,
    pub total: u64,
    #[serde(default)]
    pub pks: Vec<String>,
    pub has_more: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockingDetail {
    /// 1 基批次号；final 校验失败时等于已尝试的批次数。
    pub batch_no: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_pk: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_pk: Option<String>,
    /// precheck:v3 / precheck:v4 / postcheck:v1 / final:v3 ...
    pub phase: String,
    pub total_diffs: u64,
    #[serde(default)]
    pub pks: Vec<String>,
    pub has_more: bool,
    #[serde(default)]
    pub differences: Vec<DiffDetail>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify: Option<BatchVerify>,
    pub message: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MigrationProgress {
    pub expected_rows: u64,
    /// 已处理（校验/写入）的源行数。
    pub processed_rows: u64,
    pub success_rows: u64,
    pub failed_rows: u64,
    pub remaining_rows: u64,
    pub total_batches: u64,
    /// 当前/最后一批的 1 基批次号。
    pub current_batch: u64,
    /// 续跑时 checkpoint 的边界主键。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_after_pk: Option<String>,
    /// 本次续跑直接跳过的已完成批数（直接 seek，不再重走）。
    #[serde(default)]
    pub resume_skipped_batches: u64,
    /// 当前批次第一个源主键（用于中断后确认从哪个主键续起）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_first_pk: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_pk: Option<String>,
    /// 当前正在执行 SQL 的连接 ID；中止时可 KILL QUERY。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_id: Option<u64>,
}

/// PG 回填的执行语义；MySQL 任务为 None，不改变既有 JSON 结构。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackfillExecution {
    #[serde(default)]
    pub atomic: bool,
    #[serde(default = "default_rollback_scope")]
    pub rollback_scope: String,
    #[serde(default = "default_resumable")]
    pub resumable: bool,
    #[serde(default = "default_validation_mode")]
    pub validation_mode: String,
}

fn default_rollback_scope() -> String {
    "batch".to_string()
}

fn default_resumable() -> bool {
    true
}

fn default_validation_mode() -> String {
    "per_statement".to_string()
}

impl Default for BackfillExecution {
    fn default() -> Self {
        Self {
            atomic: false,
            rollback_scope: default_rollback_scope(),
            resumable: default_resumable(),
            validation_mode: default_validation_mode(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationJob {
    pub job_id: String,
    pub plan_id: String,
    pub plan_hash: String,
    /// 绑定计划 + 配置 + 规格的稳定哈希；审批令牌只认这个值。
    pub migration_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_id: Option<String>,
    pub db_id: String,
    pub db_name: String,
    pub db_fingerprint: String,
    pub spec: BackfillSpec,
    pub config: BackfillConfig,
    /// 执行引擎；新任务首次运行绑定，旧文件缺省 mysql。
    #[serde(default = "default_engine_mysql")]
    pub engine: String,
    /// 仅 PG 任务填写。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<BackfillExecution>,
    pub status: MigrationStatus,
    pub phase: MigrationPhase,
    /// 生产环境是否已显式确认（dev/staging 默认 false 即可）。
    pub production_confirmed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<Checkpoint>,
    #[serde(default)]
    pub progress: MigrationProgress,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocking: Option<BlockingDetail>,
    #[serde(default)]
    pub recent_batches: Vec<BatchRecord>,
    /// 首次运行绑定的源表结构哈希；续跑时不一致即阻断。
    #[serde(default)]
    pub source_schema_hash: String,
    /// 首次运行绑定的目标表结构哈希。
    #[serde(default)]
    pub target_schema_hash: String,
    /// 目标表是否由本任务执行 CREATE 创建。
    #[serde(default)]
    pub target_created_by_job: bool,
    /// 首次运行时的实际源行数；续跑漂移即阻断。
    #[serde(default)]
    pub expected_rows: u64,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(default)]
    pub started_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// 已执行的续跑次数（首次运行=1）。
    #[serde(default)]
    pub run_count: u64,
}

impl MigrationJob {
    pub fn is_active_status(&self) -> bool {
        matches!(
            self.status,
            MigrationStatus::Pending | MigrationStatus::Running
        )
    }
}

// ── S3：SET NOT NULL 任务 ──────────────────────────────────

/// `SET NOT NULL` 阶段机；每阶段入口幂等，续跑从当前阶段继续。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotNullPhase {
    Preflight,
    Scan,
    AddCheck,
    Validate,
    SetNotNull,
    DropCheck,
    Verify,
    Done,
}

impl NotNullPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Preflight => "preflight",
            Self::Scan => "scan",
            Self::AddCheck => "add_check",
            Self::Validate => "validate",
            Self::SetNotNull => "set_not_null",
            Self::DropCheck => "drop_check",
            Self::Verify => "verify",
            Self::Done => "done",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NotNullScanProgress {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_pk: Option<String>,
    #[serde(default)]
    pub scanned_rows: u64,
    #[serde(default)]
    pub null_count: u64,
    #[serde(default)]
    pub null_samples: Vec<String>,
    #[serde(default)]
    pub pk_kind: Option<PkKind>,
}

/// DDL 阶段不整体回滚（逐阶段幂等续跑），因此 rollback_scope 固定为 `phase`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotNullExecution {
    #[serde(default)]
    pub atomic: bool,
    #[serde(default = "default_phase_scope")]
    pub rollback_scope: String,
    #[serde(default = "default_resumable")]
    pub resumable: bool,
}

fn default_phase_scope() -> String {
    "phase".to_string()
}

impl Default for NotNullExecution {
    fn default() -> Self {
        Self {
            atomic: false,
            rollback_scope: default_phase_scope(),
            resumable: default_resumable(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotNullJob {
    pub job_id: String,
    pub plan_id: String,
    pub db_id: String,
    pub db_name: String,
    pub db_fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    pub table: String,
    pub column: String,
    /// 固定为 `fm_not_null_<column>`，与 S1 脚本同名。
    pub constraint_name: String,
    /// 表/列/约束名指纹；续跑不一致即拒绝。
    pub spec_hash: String,
    #[serde(default = "default_engine_mysql")]
    pub engine: String,
    pub status: MigrationStatus,
    pub phase: NotNullPhase,
    #[serde(default)]
    pub scan: NotNullScanProgress,
    /// 扫描批大小/间隔/限速；复用 S2 的 `BackfillConfig` 与限速逻辑。
    #[serde(default)]
    pub config: BackfillConfig,
    #[serde(default)]
    pub execution: NotNullExecution,
    #[serde(default)]
    pub production_confirmed: bool,
    #[serde(default)]
    pub run_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocking: Option<BlockingDetail>,
    #[serde(default)]
    pub notes: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(default)]
    pub started_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

// ── S3：CONCURRENTLY 索引任务 ──────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    pub table: String,
    pub index_name: String,
    /// 仅普通列（有序）；不支持表达式/部分索引/INCLUDE/opclass。
    pub columns: Vec<String>,
    #[serde(default)]
    pub unique: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexPhase {
    Preflight,
    Building,
    Verifying,
    Done,
}

impl IndexPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Preflight => "preflight",
            Self::Building => "building",
            Self::Verifying => "verifying",
            Self::Done => "done",
        }
    }
}

/// `pg_stat_progress_create_index` 的最近一次快照；轮询是 best-effort。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IndexProgress {
    #[serde(default)]
    pub phase: String,
    #[serde(default)]
    pub blocks_done: u64,
    #[serde(default)]
    pub blocks_total: u64,
    #[serde(default)]
    pub tuples_done: u64,
    #[serde(default)]
    pub tuples_total: u64,
    /// 分区表配方：叶子分区总数/已完成数，以及当前分区名。
    #[serde(default)]
    pub partitions_total: u64,
    #[serde(default)]
    pub partitions_done: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_partition: Option<String>,
    #[serde(default)]
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexJob {
    pub job_id: String,
    pub plan_id: String,
    pub db_id: String,
    pub db_name: String,
    pub db_fingerprint: String,
    pub spec: IndexSpec,
    pub spec_hash: String,
    #[serde(default = "default_engine_mysql")]
    pub engine: String,
    pub status: MigrationStatus,
    pub phase: IndexPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<IndexProgress>,
    #[serde(default)]
    pub production_confirmed: bool,
    #[serde(default)]
    pub run_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocking: Option<BlockingDetail>,
    #[serde(default)]
    pub notes: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(default)]
    pub started_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TypeChangePhase {
    Preflight,
    Applying,
    Verifying,
    Done,
}

impl TypeChangePhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Preflight => "preflight",
            Self::Applying => "applying",
            Self::Verifying => "verifying",
            Self::Done => "done",
        }
    }
}

/// 白名单（PG 保证不触发表重写的组合）之外一律 `Rewrite`，走开闸门。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TypeChangeClass {
    MetadataOnly,
    Rewrite,
}

impl TypeChangeClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MetadataOnly => "metadata_only",
            Self::Rewrite => "rewrite",
        }
    }
}

/// DDL 单语句失败由 PG 原子回滚列本身（无半成品），续跑 = 重跑语句。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeChangeExecution {
    #[serde(default)]
    pub atomic: bool,
    #[serde(default = "default_statement_scope")]
    pub rollback_scope: String,
    #[serde(default = "default_resumable")]
    pub resumable: bool,
}

fn default_statement_scope() -> String {
    "statement".to_string()
}

impl Default for TypeChangeExecution {
    fn default() -> Self {
        Self {
            atomic: false,
            rollback_scope: default_statement_scope(),
            resumable: default_resumable(),
        }
    }
}

/// 前置快照 + 执行后核对：类型、行数、索引健康、默认值表达式（只记录不判败）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TypeChangeVerify {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_type: Option<String>,
    #[serde(default)]
    pub rows_before: u64,
    #[serde(default)]
    pub rows_after: u64,
    /// 表上索引数量（前置快照）。
    #[serde(default)]
    pub index_count: u64,
    #[serde(default)]
    pub indexes_valid: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_after: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FkCounterpartSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    pub table: String,
    pub column: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FkPlanEntry {
    pub constraint: String,
    pub referencing_schema: String,
    pub referencing_table: String,
    pub referencing_column: String,
    pub referenced_schema: String,
    pub referenced_table: String,
    pub referenced_column: String,
    /// 与主列目标类型一致（双端同型是 FK 重建前提）。
    pub target_type: String,
    /// 编排进度位（幂等续跑）。
    #[serde(default)]
    pub dropped: bool,
    #[serde(default)]
    pub altered: bool,
    #[serde(default)]
    pub recreated: bool,
    #[serde(default)]
    pub validated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeChangeJob {
    pub job_id: String,
    pub plan_id: String,
    pub db_id: String,
    pub db_name: String,
    pub db_fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    pub table: String,
    pub column: String,
    /// 首跑记录的列类型（format_type）；续跑做漂移校验。
    pub source_type: String,
    /// 请求的目标类型（用户原始串，执行前经 ::regtype 验证）。
    pub target_type: String,
    /// freeform USING 表达式（S5）；进 spec_hash，续跑篡改即拒绝。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub using_expr: Option<String>,
    /// 请求声明的 FK 对端列（S5）；与 `fk_plan` 一起进指纹。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fk_counterparts: Vec<FkCounterpartSpec>,
    /// 表/列/源类型/目标类型指纹；续跑不一致即拒绝。
    pub spec_hash: String,
    #[serde(default = "default_engine_mysql")]
    pub engine: String,
    pub status: MigrationStatus,
    pub phase: TypeChangePhase,
    pub classification: TypeChangeClass,
    /// 重写路径的显式开闸；`MetadataOnly` 恒不需要。
    #[serde(default)]
    pub allow_rewrite: bool,
    #[serde(default)]
    pub production_confirmed: bool,
    #[serde(default)]
    pub execution: TypeChangeExecution,
    #[serde(default)]
    pub verify: TypeChangeVerify,
    /// FK 拆装编排清单（S5）；进 spec_hash。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fk_plan: Vec<FkPlanEntry>,
    #[serde(default)]
    pub run_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocking: Option<BlockingDetail>,
    #[serde(default)]
    pub notes: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(default)]
    pub started_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

const RECENT_BATCH_LIMIT: usize = 80;

#[derive(Debug, Default, Serialize, Deserialize)]
struct MigrationFile {
    jobs: HashMap<String, MigrationJob>,
    /// 空映射不序列化：无 PG 任务的存量 store 文件字节不变。
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    notnull_jobs: HashMap<String, NotNullJob>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    index_jobs: HashMap<String, IndexJob>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    typechange_jobs: HashMap<String, TypeChangeJob>,
}

#[derive(Debug)]
pub struct MigrationStore {
    jobs: HashMap<String, MigrationJob>,
    notnull_jobs: HashMap<String, NotNullJob>,
    index_jobs: HashMap<String, IndexJob>,
    typechange_jobs: HashMap<String, TypeChangeJob>,
    path: Option<PathBuf>,
}

impl MigrationStore {
    pub fn new(path: Option<PathBuf>) -> Self {
        let mut store = Self {
            jobs: HashMap::new(),
            notnull_jobs: HashMap::new(),
            index_jobs: HashMap::new(),
            typechange_jobs: HashMap::new(),
            path,
        };
        store.load();
        // 进程启动时，上一个进程留下的 running 任务视为中断，等待人工续跑。
        if let Err(err) = store.mark_running_as_interrupted() {
            eprintln!("字段回填任务恢复状态写入失败: {err}");
        }
        store
    }

    fn load(&mut self) {
        let Some(path) = &self.path else {
            return;
        };
        let Ok(raw) = std::fs::read_to_string(path) else {
            return;
        };
        if let Ok(file) = serde_json::from_str::<MigrationFile>(&raw) {
            self.jobs = file.jobs;
            self.notnull_jobs = file.notnull_jobs;
            self.index_jobs = file.index_jobs;
            self.typechange_jobs = file.typechange_jobs;
        }
    }

    fn save(&self) -> Result<(), OpsError> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = MigrationFile {
            jobs: self.jobs.clone(),
            notnull_jobs: self.notnull_jobs.clone(),
            index_jobs: self.index_jobs.clone(),
            typechange_jobs: self.typechange_jobs.clone(),
        };
        let raw = serde_json::to_string_pretty(&file)?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, raw)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    fn mark_running_as_interrupted(&mut self) -> Result<(), OpsError> {
        let mut changed = false;
        for job in self.jobs.values_mut() {
            if job.status == MigrationStatus::Running {
                job.status = MigrationStatus::Interrupted;
                job.phase = MigrationPhase::Interrupted;
                job.updated_at = now();
                job.last_error = Some("进程重启：任务从 checkpoint 续跑".into());
                changed = true;
            }
        }
        for job in self.notnull_jobs.values_mut() {
            if job.status == MigrationStatus::Running {
                job.status = MigrationStatus::Interrupted;
                job.updated_at = now();
                job.last_error = Some("进程重启：任务从当前阶段续跑".into());
                changed = true;
            }
        }
        for job in self.index_jobs.values_mut() {
            if job.status == MigrationStatus::Running {
                job.status = MigrationStatus::Interrupted;
                job.updated_at = now();
                job.last_error = Some("进程重启：任务可重新执行".into());
                changed = true;
            }
        }
        for job in self.typechange_jobs.values_mut() {
            if job.status == MigrationStatus::Running {
                job.status = MigrationStatus::Interrupted;
                job.updated_at = now();
                job.last_error = Some("进程重启：任务可重新执行（DDL 失败原子回滚列本身）".into());
                changed = true;
            }
        }
        if changed {
            self.save()?;
        }
        Ok(())
    }

    pub fn insert(&mut self, job: MigrationJob) -> Result<(), OpsError> {
        if self.jobs.contains_key(&job.job_id) {
            return Err(OpsError::Validation(format!(
                "回填任务 {} 已存在",
                job.job_id
            )));
        }
        self.jobs.insert(job.job_id.clone(), job);
        self.save()
    }

    pub fn get(&self, job_id: &str) -> Option<&MigrationJob> {
        self.jobs.get(job_id)
    }

    pub fn update<F>(&mut self, job_id: &str, f: F) -> Result<MigrationJob, OpsError>
    where
        F: FnOnce(&mut MigrationJob),
    {
        let job = self
            .jobs
            .get_mut(job_id)
            .ok_or_else(|| OpsError::NotFound(format!("回填任务 {job_id} 不存在")))?;
        f(job);
        job.updated_at = now();
        let snapshot = job.clone();
        self.save()?;
        Ok(snapshot)
    }

    pub fn list(&self, plan_id: Option<&str>, limit: usize) -> Vec<MigrationJob> {
        let mut jobs: Vec<_> = self
            .jobs
            .values()
            .filter(|job| plan_id.map(|id| job.plan_id == id).unwrap_or(true))
            .cloned()
            .collect();
        jobs.sort_by_key(|job| std::cmp::Reverse(job.created_at));
        jobs.truncate(limit);
        jobs
    }

    pub fn push_batch_record(job: &mut MigrationJob, record: BatchRecord) {
        job.recent_batches.push(record);
        let excess = job.recent_batches.len().saturating_sub(RECENT_BATCH_LIMIT);
        if excess > 0 {
            job.recent_batches.drain(0..excess);
        }
    }

    // ── S3 任务 ──────────────────────────────────────────

    pub fn insert_notnull(&mut self, job: NotNullJob) -> Result<(), OpsError> {
        if self.notnull_jobs.contains_key(&job.job_id) {
            return Err(OpsError::Validation(format!(
                "SET NOT NULL 任务 {} 已存在",
                job.job_id
            )));
        }
        self.notnull_jobs.insert(job.job_id.clone(), job);
        self.save()
    }

    pub fn notnull(&self, job_id: &str) -> Option<&NotNullJob> {
        self.notnull_jobs.get(job_id)
    }

    pub fn update_notnull<F>(&mut self, job_id: &str, f: F) -> Result<NotNullJob, OpsError>
    where
        F: FnOnce(&mut NotNullJob),
    {
        let job = self
            .notnull_jobs
            .get_mut(job_id)
            .ok_or_else(|| OpsError::NotFound(format!("SET NOT NULL 任务 {job_id} 不存在")))?;
        f(job);
        job.updated_at = now();
        let snapshot = job.clone();
        self.save()?;
        Ok(snapshot)
    }

    pub fn list_notnull(&self, limit: usize) -> Vec<NotNullJob> {
        let mut jobs: Vec<_> = self.notnull_jobs.values().cloned().collect();
        jobs.sort_by_key(|job| std::cmp::Reverse(job.created_at));
        jobs.truncate(limit);
        jobs
    }

    pub fn insert_index(&mut self, job: IndexJob) -> Result<(), OpsError> {
        if self.index_jobs.contains_key(&job.job_id) {
            return Err(OpsError::Validation(format!(
                "索引任务 {} 已存在",
                job.job_id
            )));
        }
        self.index_jobs.insert(job.job_id.clone(), job);
        self.save()
    }

    pub fn index(&self, job_id: &str) -> Option<&IndexJob> {
        self.index_jobs.get(job_id)
    }

    pub fn update_index<F>(&mut self, job_id: &str, f: F) -> Result<IndexJob, OpsError>
    where
        F: FnOnce(&mut IndexJob),
    {
        let job = self
            .index_jobs
            .get_mut(job_id)
            .ok_or_else(|| OpsError::NotFound(format!("索引任务 {job_id} 不存在")))?;
        f(job);
        job.updated_at = now();
        let snapshot = job.clone();
        self.save()?;
        Ok(snapshot)
    }

    pub fn list_index(&self, limit: usize) -> Vec<IndexJob> {
        let mut jobs: Vec<_> = self.index_jobs.values().cloned().collect();
        jobs.sort_by_key(|job| std::cmp::Reverse(job.created_at));
        jobs.truncate(limit);
        jobs
    }

    // ── S4：改类型任务 ──────────────────────────────────

    pub fn insert_typechange(&mut self, job: TypeChangeJob) -> Result<(), OpsError> {
        if self.typechange_jobs.contains_key(&job.job_id) {
            return Err(OpsError::Validation(format!(
                "改类型任务 {} 已存在",
                job.job_id
            )));
        }
        self.typechange_jobs.insert(job.job_id.clone(), job);
        self.save()
    }

    pub fn typechange(&self, job_id: &str) -> Option<&TypeChangeJob> {
        self.typechange_jobs.get(job_id)
    }

    pub fn update_typechange<F>(&mut self, job_id: &str, f: F) -> Result<TypeChangeJob, OpsError>
    where
        F: FnOnce(&mut TypeChangeJob),
    {
        let job = self
            .typechange_jobs
            .get_mut(job_id)
            .ok_or_else(|| OpsError::NotFound(format!("改类型任务 {job_id} 不存在")))?;
        f(job);
        job.updated_at = now();
        let snapshot = job.clone();
        self.save()?;
        Ok(snapshot)
    }

    pub fn list_typechange(&self, limit: usize) -> Vec<TypeChangeJob> {
        let mut jobs: Vec<_> = self.typechange_jobs.values().cloned().collect();
        jobs.sort_by_key(|job| std::cmp::Reverse(job.created_at));
        jobs.truncate(limit);
        jobs
    }
}

// ── 控制信号 ───────────────────────────────────────────────

pub const CONTROL_RUN: u8 = 0;
pub const CONTROL_PAUSE: u8 = 1;
pub const CONTROL_ABORT: u8 = 2;

/// 进程内共享的任务存储 + 控制信号。可 Clone，便于后台任务持有。
#[derive(Clone)]
pub struct MigrationHub {
    pub store: Arc<RwLock<MigrationStore>>,
    controls: Arc<RwLock<HashMap<String, Arc<AtomicU8>>>>,
    active: Arc<RwLock<HashSet<String>>>,
}

impl MigrationHub {
    pub fn new(path: Option<PathBuf>) -> Self {
        Self {
            store: Arc::new(RwLock::new(MigrationStore::new(path))),
            controls: Arc::new(RwLock::new(HashMap::new())),
            active: Arc::new(RwLock::new(HashSet::new())),
        }
    }

    pub fn for_test() -> Self {
        Self::new(None)
    }

    pub async fn insert(&self, job: MigrationJob) -> Result<(), OpsError> {
        self.store.write().await.insert(job)
    }

    pub async fn get(&self, job_id: &str) -> Result<MigrationJob, OpsError> {
        self.store
            .read()
            .await
            .get(job_id)
            .cloned()
            .ok_or_else(|| OpsError::NotFound(format!("回填任务 {job_id} 不存在")))
    }

    pub async fn update<F>(&self, job_id: &str, f: F) -> Result<MigrationJob, OpsError>
    where
        F: FnOnce(&mut MigrationJob),
    {
        self.store.write().await.update(job_id, f)
    }

    pub async fn list(&self, plan_id: Option<&str>, limit: usize) -> Vec<MigrationJob> {
        self.store.read().await.list(plan_id, limit)
    }

    pub async fn insert_notnull(&self, job: NotNullJob) -> Result<(), OpsError> {
        self.store.write().await.insert_notnull(job)
    }

    pub async fn get_notnull(&self, job_id: &str) -> Result<NotNullJob, OpsError> {
        self.store
            .read()
            .await
            .notnull(job_id)
            .cloned()
            .ok_or_else(|| OpsError::NotFound(format!("SET NOT NULL 任务 {job_id} 不存在")))
    }

    pub async fn update_notnull<F>(&self, job_id: &str, f: F) -> Result<NotNullJob, OpsError>
    where
        F: FnOnce(&mut NotNullJob),
    {
        self.store.write().await.update_notnull(job_id, f)
    }

    pub async fn list_notnull(&self, limit: usize) -> Vec<NotNullJob> {
        self.store.read().await.list_notnull(limit)
    }

    pub async fn insert_index(&self, job: IndexJob) -> Result<(), OpsError> {
        self.store.write().await.insert_index(job)
    }

    pub async fn get_index(&self, job_id: &str) -> Result<IndexJob, OpsError> {
        self.store
            .read()
            .await
            .index(job_id)
            .cloned()
            .ok_or_else(|| OpsError::NotFound(format!("索引任务 {job_id} 不存在")))
    }

    pub async fn update_index<F>(&self, job_id: &str, f: F) -> Result<IndexJob, OpsError>
    where
        F: FnOnce(&mut IndexJob),
    {
        self.store.write().await.update_index(job_id, f)
    }

    pub async fn list_index(&self, limit: usize) -> Vec<IndexJob> {
        self.store.read().await.list_index(limit)
    }

    pub async fn insert_typechange(&self, job: TypeChangeJob) -> Result<(), OpsError> {
        self.store.write().await.insert_typechange(job)
    }

    pub async fn get_typechange(&self, job_id: &str) -> Result<TypeChangeJob, OpsError> {
        self.store
            .read()
            .await
            .typechange(job_id)
            .cloned()
            .ok_or_else(|| OpsError::NotFound(format!("改类型任务 {job_id} 不存在")))
    }

    pub async fn update_typechange<F>(&self, job_id: &str, f: F) -> Result<TypeChangeJob, OpsError>
    where
        F: FnOnce(&mut TypeChangeJob),
    {
        self.store.write().await.update_typechange(job_id, f)
    }

    pub async fn list_typechange(&self, limit: usize) -> Vec<TypeChangeJob> {
        self.store.read().await.list_typechange(limit)
    }

    /// 取得（必要时创建）任务控制信号。
    pub async fn control(&self, job_id: &str) -> Arc<AtomicU8> {
        if let Some(control) = self.controls.read().await.get(job_id).cloned() {
            return control;
        }
        let mut controls = self.controls.write().await;
        controls
            .entry(job_id.to_string())
            .or_insert_with(|| Arc::new(AtomicU8::new(CONTROL_RUN)))
            .clone()
    }

    pub async fn control_value(&self, job_id: &str) -> u8 {
        self.control(job_id).await.load(Ordering::SeqCst)
    }

    pub async fn set_control(&self, job_id: &str, value: u8) {
        self.control(job_id).await.store(value, Ordering::SeqCst);
    }

    pub async fn mark_active(&self, job_id: &str) {
        self.active.write().await.insert(job_id.to_string());
    }

    pub async fn mark_inactive(&self, job_id: &str) {
        self.active.write().await.remove(job_id);
    }

    pub async fn is_active(&self, job_id: &str) -> bool {
        self.active.read().await.contains(job_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(id: &str) -> MigrationJob {
        MigrationJob {
            job_id: id.into(),
            plan_id: "p1".into(),
            plan_hash: "ph".into(),
            migration_hash: "mh".into(),
            approval_id: None,
            db_id: "db".into(),
            db_name: "app".into(),
            db_fingerprint: "fp".into(),
            engine: String::new(),
            execution: None,
            spec: BackfillSpec {
                source_table: "orders".into(),
                source_column: "payload".into(),
                target_table: "orders_ext".into(),
                pk_column: "id".into(),
                pk_kind: None,
                pk_columns: Vec::new(),
                pk_kinds: Vec::new(),
                target_columns: Vec::new(),
                empty_string_policy: EmptyStringPolicy::Preserve,
                create_table_sql: None,
                schema: None,
                source_data_type: String::new(),
            },
            config: BackfillConfig::default(),
            status: MigrationStatus::Running,
            phase: MigrationPhase::Backfilling,
            production_confirmed: false,
            checkpoint: None,
            progress: MigrationProgress::default(),
            blocking: None,
            recent_batches: Vec::new(),
            source_schema_hash: String::new(),
            target_schema_hash: String::new(),
            target_created_by_job: false,
            expected_rows: 0,
            created_at: now(),
            updated_at: now(),
            started_at: 0,
            finished_at: None,
            last_error: None,
            run_count: 0,
        }
    }

    #[test]
    fn batch_config_survives_restart_so_resume_uses_same_parameters() {
        let dir = std::env::temp_dir().join(format!("fm-store-cfg-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("jobs.json");
        let config = BackfillConfig {
            batch_size: 10_000,
            batch_interval_ms: 40,
            max_rows_per_second: 1_000,
            single_transaction: false,
        };
        {
            let mut store = MigrationStore::new(Some(path.clone()));
            let mut saved = job("j2");
            saved.config = config.clone();
            saved.progress.resume_skipped_batches = 2;
            saved.checkpoint = Some(Checkpoint {
                completed_batches: 2,
                last_pk: Some("20000".into()),
                written_rows: 20_000,
                verified: true,
                verify: None,
                engine: default_engine_mysql(),
                pk_kind: None,
                last_pks: None,
                pk_kinds: Vec::new(),
                target_fingerprint: None,
                updated_at: now(),
            });
            store.insert(saved).unwrap();
        }
        let store = MigrationStore::new(Some(path));
        let loaded = store.get("j2").unwrap();
        assert_eq!(loaded.config, config, "重启后批参数必须与首发一致");
        assert_eq!(loaded.progress.resume_skipped_batches, 2);
        assert_eq!(
            loaded.checkpoint.as_ref().and_then(|c| c.last_pk.clone()),
            Some("20000".into())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn checkpoint_round_trips_and_marks_running_interrupted() {
        let dir = std::env::temp_dir().join(format!("fm-store-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("jobs.json");
        {
            let mut store = MigrationStore::new(Some(path.clone()));
            store.insert(job("j1")).unwrap();
        }
        // 重新打开：running 必须变成 interrupted，checkpoint 不丢失。
        let store = MigrationStore::new(Some(path));
        let loaded = store.get("j1").unwrap();
        assert_eq!(loaded.status, MigrationStatus::Interrupted);
        assert_eq!(loaded.phase, MigrationPhase::Interrupted);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn hub_control_is_shared_and_last_write_wins() {
        let hub = MigrationHub::for_test();
        hub.insert(job("j1")).await.unwrap();
        assert_eq!(hub.control_value("j1").await, CONTROL_RUN);
        hub.set_control("j1", CONTROL_PAUSE).await;
        assert_eq!(hub.control_value("j1").await, CONTROL_PAUSE);
        hub.set_control("j1", CONTROL_ABORT).await;
        assert_eq!(hub.control_value("j1").await, CONTROL_ABORT);
        hub.mark_active("j1").await;
        assert!(hub.is_active("j1").await);
        hub.mark_inactive("j1").await;
        assert!(!hub.is_active("j1").await);
    }

    /// S5：单列任务序列化不得出现 `pk_columns` / `pk_kinds`，否则存量 store 字节会变。
    #[test]
    fn single_pk_spec_omits_s5_fields() {
        let spec = job("j1").spec;
        let raw = serde_json::to_string(&spec).unwrap();
        assert!(!raw.contains("pk_columns"), "{raw}");
        assert!(!raw.contains("pk_kinds"), "{raw}");
    }

    /// S5：复合主键 + USING + FK 计划必须能完整回环，且旧 JSON（缺字段）仍可解析。
    #[test]
    fn s5_fields_round_trip_and_legacy_json_parses() {
        let mut spec = job("j1").spec;
        spec.pk_columns = vec!["tenant_id".into(), "seq".into()];
        spec.pk_kinds = vec![PkKind::Integer, PkKind::Integer];
        let raw = serde_json::to_string(&spec).unwrap();
        let back: BackfillSpec = serde_json::from_str(&raw).unwrap();
        assert_eq!(back.pk_columns, spec.pk_columns);
        assert_eq!(back.pk_kinds, spec.pk_kinds);

        // 模拟 S4 及以前的 store：删掉 S5 新字段后仍必须能反序列化。
        let mut value = serde_json::to_value(&spec).unwrap();
        for key in ["pk_columns", "pk_kinds"] {
            value.as_object_mut().unwrap().remove(key);
        }
        let legacy: BackfillSpec = serde_json::from_value(value).unwrap();
        assert!(legacy.pk_columns.is_empty());
        assert!(legacy.pk_kinds.is_empty());

        let mut tc = typechange_job("t1");
        tc.using_expr = Some("payload::text".into());
        tc.fk_counterparts = vec![FkCounterpartSpec {
            schema: Some("public".into()),
            table: "children".into(),
            column: "parent_id".into(),
        }];
        tc.fk_plan = vec![FkPlanEntry {
            constraint: "children_parent_id_fkey".into(),
            referencing_schema: "public".into(),
            referencing_table: "children".into(),
            referencing_column: "parent_id".into(),
            referenced_schema: "public".into(),
            referenced_table: "orders".into(),
            referenced_column: "id".into(),
            target_type: "bigint".into(),
            dropped: true,
            altered: false,
            recreated: false,
            validated: false,
        }];
        let raw = serde_json::to_string(&tc).unwrap();
        let back: TypeChangeJob = serde_json::from_str(&raw).unwrap();
        assert_eq!(back.using_expr.as_deref(), Some("payload::text"));
        assert_eq!(back.fk_counterparts.len(), 1);
        assert_eq!(back.fk_plan.len(), 1);
        assert!(back.fk_plan[0].dropped);
        assert!(!back.fk_plan[0].altered);
    }

    #[test]
    fn recent_batch_records_are_capped() {
        let mut j = job("j1");
        for index in 0..(RECENT_BATCH_LIMIT + 10) {
            MigrationStore::push_batch_record(
                &mut j,
                BatchRecord {
                    batch_no: index as u64,
                    last_pk: None,
                    last_pks: None,
                    end_pks: None,
                    end_pk: index.to_string(),
                    source_rows: 1,
                    written_rows: 1,
                    verified: true,
                    verify: BatchVerify::default(),
                    duration_ms: 0,
                    at: 0,
                },
            );
        }
        assert_eq!(j.recent_batches.len(), RECENT_BATCH_LIMIT);
        assert_eq!(
            j.recent_batches.last().unwrap().batch_no,
            (RECENT_BATCH_LIMIT + 9) as u64
        );
    }

    fn notnull_job(id: &str) -> NotNullJob {
        NotNullJob {
            job_id: id.into(),
            plan_id: "p1".into(),
            db_id: "db".into(),
            db_name: "app".into(),
            db_fingerprint: "fp".into(),
            schema: Some("public".into()),
            table: "orders".into(),
            column: "payload".into(),
            constraint_name: "fm_not_null_payload".into(),
            spec_hash: "sh".into(),
            engine: String::new(),
            status: MigrationStatus::Running,
            phase: NotNullPhase::Scan,
            scan: NotNullScanProgress::default(),
            config: BackfillConfig::default(),
            execution: NotNullExecution::default(),
            production_confirmed: false,
            run_count: 1,
            blocking: None,
            notes: Vec::new(),
            created_at: now(),
            updated_at: now(),
            started_at: now(),
            finished_at: None,
            last_error: None,
        }
    }

    fn index_job(id: &str) -> IndexJob {
        IndexJob {
            job_id: id.into(),
            plan_id: "p1".into(),
            db_id: "db".into(),
            db_name: "app".into(),
            db_fingerprint: "fp".into(),
            spec: IndexSpec {
                schema: Some("public".into()),
                table: "orders".into(),
                index_name: "idx_orders_payload".into(),
                columns: vec!["payload".into()],
                unique: false,
            },
            spec_hash: "ih".into(),
            engine: String::new(),
            status: MigrationStatus::Running,
            phase: IndexPhase::Building,
            progress: None,
            production_confirmed: false,
            run_count: 1,
            blocking: None,
            notes: Vec::new(),
            created_at: now(),
            updated_at: now(),
            started_at: now(),
            finished_at: None,
            last_error: None,
        }
    }

    #[test]
    fn s3_jobs_round_trip_and_running_marks_interrupted() {
        let dir = std::env::temp_dir().join(format!("fm-store-s3-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("jobs.json");
        {
            let mut store = MigrationStore::new(Some(path.clone()));
            store.insert_notnull(notnull_job("n1")).unwrap();
            store.insert_index(index_job("i1")).unwrap();
        }
        let mut store = MigrationStore::new(Some(path));
        // 重启后 running 视为 interrupted，且两类任务都保留。
        assert_eq!(
            store.notnull("n1").unwrap().status,
            MigrationStatus::Interrupted
        );
        assert_eq!(store.notnull("n1").unwrap().phase, NotNullPhase::Scan);
        assert_eq!(
            store.index("i1").unwrap().status,
            MigrationStatus::Interrupted
        );
        assert_eq!(store.index("i1").unwrap().spec.columns, vec!["payload"]);
        store
            .update_index("i1", |job| job.notes.push("retry".into()))
            .unwrap();
        assert_eq!(store.index("i1").unwrap().notes, vec!["retry"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_store_file_without_s3_maps_loads() {
        let dir = std::env::temp_dir().join(format!("fm-store-legacy-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("jobs.json");
        // 老文件只有 jobs 字段，没有 notnull_jobs/index_jobs。
        std::fs::write(&path, r#"{"jobs":{}}"#).unwrap();
        let store = MigrationStore::new(Some(path.clone()));
        assert!(store.notnull("n1").is_none());
        assert!(store.index("i1").is_none());
        // 无 S3 任务时保存不得新增空映射（MySQL 存量文件字节不变）。
        let mut store = store;
        store.insert(job("j9")).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("notnull_jobs"));
        assert!(!raw.contains("index_jobs"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn typechange_job(id: &str) -> TypeChangeJob {
        TypeChangeJob {
            job_id: id.into(),
            plan_id: "p1".into(),
            db_id: "db".into(),
            db_name: "app".into(),
            db_fingerprint: "fp".into(),
            schema: Some("public".into()),
            table: "orders".into(),
            column: "payload".into(),
            source_type: "character varying(50)".into(),
            target_type: "text".into(),
            spec_hash: "th".into(),
            using_expr: None,
            fk_counterparts: Vec::new(),
            fk_plan: Vec::new(),
            engine: String::new(),
            status: MigrationStatus::Running,
            phase: TypeChangePhase::Applying,
            classification: TypeChangeClass::MetadataOnly,
            allow_rewrite: false,
            production_confirmed: false,
            execution: TypeChangeExecution::default(),
            verify: TypeChangeVerify::default(),
            run_count: 1,
            blocking: None,
            notes: Vec::new(),
            created_at: now(),
            updated_at: now(),
            started_at: now(),
            finished_at: None,
            last_error: None,
        }
    }

    #[test]
    fn s4_typechange_round_trips_and_running_marks_interrupted() {
        let dir = std::env::temp_dir().join(format!("fm-store-s4-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("jobs.json");
        {
            let mut store = MigrationStore::new(Some(path.clone()));
            store.insert_typechange(typechange_job("t1")).unwrap();
        }
        let mut store = MigrationStore::new(Some(path));
        // 重启后 running 视为 interrupted，job 字段完整保留。
        let loaded = store.typechange("t1").unwrap();
        assert_eq!(loaded.status, MigrationStatus::Interrupted);
        assert_eq!(loaded.phase, TypeChangePhase::Applying);
        assert_eq!(loaded.source_type, "character varying(50)");
        assert_eq!(loaded.classification, TypeChangeClass::MetadataOnly);
        store
            .update_typechange("t1", |job| {
                job.verify.rows_before = 10;
                job.notes.push("retry".into());
            })
            .unwrap();
        assert_eq!(store.typechange("t1").unwrap().verify.rows_before, 10);
        assert_eq!(store.typechange("t1").unwrap().notes, vec!["retry"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_store_file_without_s4_map_keeps_byte_compatible() {
        let dir = std::env::temp_dir().join(format!("fm-store-s4l-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("jobs.json");
        // S3 时期老文件：没有 typechange_jobs 字段。
        std::fs::write(&path, r#"{"jobs":{},"notnull_jobs":{},"index_jobs":{}}"#).unwrap();
        let mut store = MigrationStore::new(Some(path.clone()));
        assert!(store.typechange("t1").is_none());
        // 无 S4 任务时保存不得新增空映射（S3 存量文件字节不变）。
        store.insert(job("j9")).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("typechange_jobs"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn typechange_execution_defaults_are_statement_scoped_and_resumable() {
        let execution = TypeChangeExecution::default();
        assert!(!execution.atomic);
        assert_eq!(execution.rollback_scope, "statement");
        assert!(execution.resumable);
        // 老文件缺省反序列化后也是同一语义。
        let legacy: TypeChangeExecution = serde_json::from_str("{}").unwrap();
        assert_eq!(legacy, execution);
    }
}
