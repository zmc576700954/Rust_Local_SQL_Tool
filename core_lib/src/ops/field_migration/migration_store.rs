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

/// 主键比较方式。整数主键直接写十进制字面量；文本主键按 MySQL 排序规则比较。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PkKind {
    Integer,
    Text,
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
}

impl Default for BackfillConfig {
    fn default() -> Self {
        Self {
            batch_size: 1000,
            batch_interval_ms: 0,
            max_rows_per_second: 0,
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
    #[serde(default)]
    pub pk_kind: Option<PkKind>,
    pub target_columns: Vec<TargetColumn>,
    pub empty_string_policy: EmptyStringPolicy,
    /// L2 建表脚本；仅在目标表不存在时执行一次（CREATE TABLE IF NOT EXISTS）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create_table_sql: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchVerify {
    pub source_rows: u64,
    pub target_rows: u64,
    pub v2: u64,
    pub v3: u64,
    pub v4: u64,
}

impl BatchVerify {
    pub fn passed(&self) -> bool {
        self.source_rows == self.target_rows
            && self.v2 == 0
            && self.v3 == 0
            && self.v4 == 0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    /// 已完成并校验通过的批次数。
    pub completed_batches: u64,
    /// 已完成的批边界主键；下一批严格大于该主键。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_pk: Option<String>,
    /// 累计成功写入/校验行数。
    pub written_rows: u64,
    pub verified: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify: Option<BatchVerify>,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchRecord {
    /// 1 基批次号，界面直接展示。
    pub batch_no: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_pk: Option<String>,
    pub end_pk: String,
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
    /// 当前批次第一个源主键（用于中断后确认从哪个主键续起）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_first_pk: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_pk: Option<String>,
    /// 当前正在执行 SQL 的连接 ID；中止时可 KILL QUERY。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_id: Option<u64>,
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
        matches!(self.status, MigrationStatus::Pending | MigrationStatus::Running)
    }
}

const RECENT_BATCH_LIMIT: usize = 80;

#[derive(Debug, Default, Serialize, Deserialize)]
struct MigrationFile {
    jobs: HashMap<String, MigrationJob>,
}

#[derive(Debug)]
pub struct MigrationStore {
    jobs: HashMap<String, MigrationJob>,
    path: Option<PathBuf>,
}

impl MigrationStore {
    pub fn new(path: Option<PathBuf>) -> Self {
        let mut store = Self {
            jobs: HashMap::new(),
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
            spec: BackfillSpec {
                source_table: "orders".into(),
                source_column: "payload".into(),
                target_table: "orders_ext".into(),
                pk_column: "id".into(),
                pk_kind: None,
                target_columns: Vec::new(),
                empty_string_policy: EmptyStringPolicy::Preserve,
                create_table_sql: None,
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

    #[test]
    fn recent_batch_records_are_capped() {
        let mut j = job("j1");
        for index in 0..(RECENT_BATCH_LIMIT + 10) {
            MigrationStore::push_batch_record(
                &mut j,
                BatchRecord {
                    batch_no: index as u64,
                    last_pk: None,
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
}
