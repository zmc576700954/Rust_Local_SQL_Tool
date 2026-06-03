use core_lib::{
    ai::{
        planner::Planner,
        policy_store::Policy,
    },
    config::AppConfig,
    db::DbClient,
    knowledge_base::KnowledgeBase,
    mysql_sync::{CompareResult, PreviewResult, SyncMode},
    rule_engine::RuleStore,
    schema::{SchemaResponse, TableWithDetails},
    sql_history::SqlHistoryStore,
    timeout_policy::TimeoutPolicy,
};
#[cfg(test)]
use core_lib::ai::gateway::AiGateway;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, RwLock, Semaphore};

use crate::AppError;

// ----------------- Cache Types -----------------

#[derive(Debug, Clone)]
pub struct CachedDbClient {
    pub client: DbClient,
    pub db_name: String,
    pub url: String,
    pub expires_at: Instant,
}

#[derive(Debug, Clone)]
pub struct CachedSchemaEntry {
    pub schema: SchemaResponse,
    pub expires_at: Instant,
}

#[derive(Debug, Clone)]
pub struct CachedTableSchemaEntry {
    pub table: TableWithDetails,
    pub expires_at: Instant,
}

// ----------------- Active Query Types -----------------

#[derive(Clone)]
pub struct ActiveQueryHandle {
    pub db_client: DbClient,
    pub connection_id: u64,
    pub canceled: Arc<AtomicBool>,
}

pub struct ActiveQuerySession {
    pub token: String,
    pub connection_id: u64,
    pub canceled: Arc<AtomicBool>,
    pub owned_conn: Option<sqlx::pool::PoolConnection<sqlx::MySql>>,
    pub transaction_session: Option<SharedTransactionSession>,
}

// ----------------- Transaction Types -----------------

pub struct TransactionSession {
    pub connection_id: u64,
    pub db_id: Option<String>,
    pub conn: sqlx::pool::PoolConnection<sqlx::MySql>,
    pub last_accessed: std::time::Instant,
}

pub type SharedTransactionSession = Arc<Mutex<TransactionSession>>;

// ----------------- RuntimeLimits -----------------

#[derive(Debug, Clone)]
pub struct RuntimeLimits {
    pub temp_dir: String,
    pub temp_quota_bytes: u64,
    pub max_file_bytes: u64,
    pub max_job_concurrency: usize,
}

impl Default for RuntimeLimits {
    fn default() -> Self {
        let temp_dir = std::env::var("LOCAL_AI_SQL_TEMP_DIR")
            .ok()
            .unwrap_or_else(|| "/tmp/local-ai-sql".to_string());
        let temp_quota_bytes = std::env::var("LOCAL_AI_SQL_TEMP_QUOTA_BYTES")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(2 * 1024 * 1024 * 1024);
        let max_file_bytes = std::env::var("LOCAL_AI_SQL_MAX_FILE_BYTES")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(500 * 1024 * 1024);
        let max_job_concurrency = std::env::var("LOCAL_AI_SQL_MAX_JOB_CONCURRENCY")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(4);
        Self {
            temp_dir,
            temp_quota_bytes,
            max_file_bytes,
            max_job_concurrency: max_job_concurrency.max(1),
        }
    }
}

// ----------------- Dir Size Helpers -----------------

pub fn dir_size_bytes_sync(path: &std::path::Path) -> u64 {
    let mut total = 0u64;
    let Ok(rd) = std::fs::read_dir(path) else {
        return 0;
    };
    for entry in rd.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_file() {
            total = total.saturating_add(meta.len());
        } else if meta.is_dir() {
            total = total.saturating_add(dir_size_bytes_sync(&entry.path()));
        }
    }
    total
}

pub async fn dir_size_bytes(path: &std::path::Path) -> Result<u64, AppError> {
    let p = path.to_path_buf();
    tokio::task::spawn_blocking(move || dir_size_bytes_sync(&p))
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))
}

pub async fn ensure_temp_quota(limits: &RuntimeLimits, additional_bytes: u64) -> Result<(), AppError> {
    let dir = std::path::Path::new(&limits.temp_dir);
    if !dir.exists() {
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|e| AppError::InternalError(e.to_string()))?;
    }
    let used = dir_size_bytes(dir).await?;
    if used.saturating_add(additional_bytes) > limits.temp_quota_bytes {
        return Err(AppError::ResourceLimit(format!(
            "temp quota exceeded: used={}B, additional={}B, quota={}B, dir={}",
            used, additional_bytes, limits.temp_quota_bytes, limits.temp_dir
        )));
    }
    Ok(())
}

// ----------------- MySQL Sync Job Types -----------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MySqlSyncStage {
    Compare,
    Preview,
    Deploy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MySqlSyncJobStatus {
    Pending,
    Running,
    Completed,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MySqlSyncProgress {
    pub current: u64,
    pub total: u64,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployResult {
    pub affected_rows: u64,
    pub statements: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MySqlSyncJob {
    pub job_id: String,
    pub stage: MySqlSyncStage,
    pub status: MySqlSyncJobStatus,
    pub progress: MySqlSyncProgress,
    pub source_db_id: String,
    pub target_db_id: String,
    pub table_name: String,
    pub primary_key: String,
    pub mode: SyncMode,
    pub chunk_size: usize,
    pub created_at: i64,
    pub updated_at: i64,
    pub compare_ms: Option<u128>,
    pub preview_ms: Option<u128>,
    pub deploy_ms: Option<u128>,
    pub compare: Option<CompareResult>,
    pub preview: Option<PreviewResult>,
    pub deploy: Option<DeployResult>,
    pub error: Option<String>,
}

// ----------------- Perf Sync Job Types -----------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PerfSyncStage {
    Prepare,
    DetectBaseline,
    InjectMirror,
    Mirror,
    VerifyMirror,
    InjectUpsertOnly,
    UpsertOnly,
    VerifyUpsertOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PerfSyncJobStatus {
    Pending,
    Running,
    Completed,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PerfSyncProgress {
    pub current: u64,
    pub total: u64,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerfSyncTableSpec {
    pub table_name: String,
    pub primary_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerfSyncLoadgenRequest {
    pub tier: Option<String>,
    pub fill: Option<bool>,
    pub reset: Option<bool>,
    pub inject: Option<bool>,
    pub seed: Option<u64>,
    pub batch: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerfSyncStartRequest {
    pub source_db_id: String,
    pub target_db_id: String,
    pub tier: Option<String>,
    pub tables: Option<Vec<PerfSyncTableSpec>>,
    pub chunk_size: Option<usize>,
    pub max_rows: Option<usize>,
    pub loadgen: Option<PerfSyncLoadgenRequest>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerfSyncTableCount {
    pub source: u64,
    pub target: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerfSyncTableSyncReport {
    pub table_name: String,
    pub primary_key: String,
    pub compare_ms: u128,
    pub preview_ms: u128,
    pub deploy_ms: u128,
    pub compare_chunks: usize,
    pub different_chunks: usize,
    pub insert_count: usize,
    pub update_count: usize,
    pub delete_count: usize,
    pub statements: usize,
    pub truncated: bool,
    pub affected_rows: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerfSyncTableVerifyReport {
    pub table_name: String,
    pub different_chunks: usize,
    pub chunks: usize,
    pub verify_ms: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerfSyncModeReport {
    pub mode: SyncMode,
    pub injected_counts: HashMap<String, PerfSyncTableCount>,
    pub tables: Vec<PerfSyncTableSyncReport>,
    pub verify: Vec<PerfSyncTableVerifyReport>,
    pub passed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerfSyncReport {
    pub baseline_counts: HashMap<String, PerfSyncTableCount>,
    pub loadgen: Option<core_lib::loadgen::LoadgenReport>,
    pub mirror: PerfSyncModeReport,
    pub upsert_only: PerfSyncModeReport,
    pub stage_ms: HashMap<String, u128>,
    pub elapsed_ms: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerfSyncJob {
    pub job_id: String,
    pub stage: PerfSyncStage,
    pub status: PerfSyncJobStatus,
    pub progress: PerfSyncProgress,
    pub request: PerfSyncStartRequest,
    pub created_at: i64,
    pub updated_at: i64,
    pub report: Option<PerfSyncReport>,
    pub error: Option<String>,
}

// ----------------- Tool Job Types -----------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolJobKind {
    Export,
    Import,
    ImportSql,
    GoLive,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolJobStatus {
    Pending,
    Running,
    Completed,
    Error,
    Canceled,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ToolJobProgress {
    pub current: u64,
    pub total: Option<u64>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolJobArtifacts {
    pub data_path: Option<String>,
    pub manifest_path: Option<String>,
    pub file_name: Option<String>,
    pub content_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolJob {
    pub job_id: String,
    pub kind: ToolJobKind,
    pub status: ToolJobStatus,
    pub progress: ToolJobProgress,
    pub created_at: i64,
    pub updated_at: i64,
    pub elapsed_ms: Option<u128>,
    pub artifacts: Option<ToolJobArtifacts>,
    pub result: Option<serde_json::Value>,
    pub error: Option<String>,
}

// ----------------- AppState -----------------

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<RwLock<AppConfig>>,
    pub db_client: Arc<RwLock<Option<DbClient>>>,
    pub db_client_cache: Arc<RwLock<HashMap<String, CachedDbClient>>>,
    pub planner: Arc<RwLock<Planner>>,
    pub virtual_schema: Arc<RwLock<Option<SchemaResponse>>>,
    pub schema_cache: Arc<RwLock<HashMap<String, CachedSchemaEntry>>>,
    pub table_schema_cache: Arc<RwLock<HashMap<String, CachedTableSchemaEntry>>>,
    pub rule_store: Arc<RwLock<RuleStore>>,
    pub policy: Arc<RwLock<Policy>>,
    pub sql_history: Arc<RwLock<SqlHistoryStore>>,
    pub knowledge_base: Arc<RwLock<KnowledgeBase>>,
    pub sync_jobs: Arc<RwLock<HashMap<String, MySqlSyncJob>>>,
    pub perf_sync_jobs: Arc<RwLock<HashMap<String, PerfSyncJob>>>,
    pub active_queries: Arc<RwLock<HashMap<String, ActiveQueryHandle>>>,
    pub transaction_sessions: Arc<RwLock<HashMap<String, SharedTransactionSession>>>,
    pub tool_jobs: Arc<RwLock<HashMap<String, ToolJob>>>,
    pub tool_job_handles: Arc<RwLock<HashMap<String, tokio::task::AbortHandle>>>,
    pub timeouts: TimeoutPolicy,
    pub limits: RuntimeLimits,
    pub job_semaphore: Arc<Semaphore>,
}

// ----------------- Active Query Helpers -----------------

pub async fn register_active_query(state: &AppState, token: String, handle: ActiveQueryHandle) {
    state.active_queries.write().await.insert(token, handle);
}

pub async fn unregister_active_query(state: &AppState, token: &str) {
    state.active_queries.write().await.remove(token);
}

pub async fn cancel_active_query(state: &AppState, cancel_token: &str) -> Result<bool, AppError> {
    let handle = state.active_queries.read().await.get(cancel_token).cloned();
    let Some(handle) = handle else {
        return Ok(false);
    };

    match handle.db_client.kill_query(handle.connection_id).await {
        Ok(_) => {
            handle.canceled.store(true, Ordering::SeqCst);
            Ok(true)
        }
        Err(e) => {
            let message = e.to_string().to_lowercase();
            if message.contains("unknown thread id") {
                Ok(false)
            } else {
                Err(AppError::InternalError(e.to_string()))
            }
        }
    }
}

pub async fn resolve_transaction_db_id(
    state: &AppState,
    db_id: Option<&str>,
) -> Option<String> {
    if let Some(value) = db_id.map(str::trim).filter(|value| !value.is_empty()) {
        return Some(value.to_string());
    }
    state.config.read().await.active_db_id.clone()
}

// ----------------- Test Helper -----------------

#[cfg(test)]
pub fn test_state_with_config(config: AppConfig) -> AppState {
    let gateway = AiGateway::new(config.clone());
    let planner = Planner::new(gateway);
    AppState {
        config: Arc::new(RwLock::new(config)),
        db_client: Arc::new(RwLock::new(None)),
        db_client_cache: Arc::new(RwLock::new(HashMap::new())),
        planner: Arc::new(RwLock::new(planner)),
        virtual_schema: Arc::new(RwLock::new(None)),
        schema_cache: Arc::new(RwLock::new(HashMap::new())),
        table_schema_cache: Arc::new(RwLock::new(HashMap::new())),
        rule_store: Arc::new(RwLock::new(RuleStore::default())),
        policy: Arc::new(RwLock::new(Policy::default())),
        sql_history: Arc::new(RwLock::new(SqlHistoryStore::default())),
        knowledge_base: Arc::new(RwLock::new(KnowledgeBase::default())),
        sync_jobs: Arc::new(RwLock::new(HashMap::new())),
        perf_sync_jobs: Arc::new(RwLock::new(HashMap::new())),
        active_queries: Arc::new(RwLock::new(HashMap::new())),
        transaction_sessions: Arc::new(RwLock::new(HashMap::new())),
        tool_jobs: Arc::new(RwLock::new(HashMap::new())),
        tool_job_handles: Arc::new(RwLock::new(HashMap::new())),
        timeouts: TimeoutPolicy::default(),
        limits: RuntimeLimits::default(),
        job_semaphore: Arc::new(Semaphore::new(1)),
    }
}
