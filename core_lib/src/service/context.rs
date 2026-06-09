//! ServiceContext — 替代原 AppState "上帝对象"
//!
//! 将 15 个 Arc<RwLock<...>> 字段按领域分组为 5 个子状态，
//! 对外只暴露方法而非裸字段，降低耦合度。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::{Mutex, RwLock, Semaphore};

use crate::config::AppConfig;
use crate::db::DbClient;
use crate::rule_engine::RuleStore;
use crate::schema::{SchemaResponse, TableWithDetails};
use crate::sql_history::SqlHistoryStore;
use crate::timeout_policy::TimeoutPolicy;
use crate::knowledge_base::KnowledgeBase;
use crate::ai::policy_store::Policy;

use crate::service::error::ServiceError;

// ── 缓存类型（从 web-server/src/state.rs 迁移） ───────────

#[derive(Debug, Clone)]
pub struct CachedDbClient {
    pub client: DbClient,
    pub db_name: String,
    pub url: String,
    pub expires_at: std::time::Instant,
}

#[derive(Debug, Clone)]
pub struct CachedSchemaEntry {
    pub schema: SchemaResponse,
    pub expires_at: std::time::Instant,
}

#[derive(Debug, Clone)]
pub struct CachedTableSchemaEntry {
    pub table: TableWithDetails,
    pub expires_at: std::time::Instant,
}

// ── 活跃查询类型 ──────────────────────────────────────────

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

// ── 事务类型 ──────────────────────────────────────────────

pub struct TransactionSession {
    pub connection_id: u64,
    pub db_id: Option<String>,
    pub conn: sqlx::pool::PoolConnection<sqlx::MySql>,
    pub last_accessed: std::time::Instant,
}

pub type SharedTransactionSession = Arc<Mutex<TransactionSession>>;

// ── 运行时限制 ──────────────────────────────────────────

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

// ── 子状态定义 ───────────────────────────────────────────

/// 数据库连接状态
pub struct DbState {
    active_client: Arc<RwLock<Option<DbClient>>>,
    client_cache: Arc<RwLock<HashMap<String, CachedDbClient>>>,
}

impl DbState {
    pub fn new() -> Self {
        Self {
            active_client: Arc::new(RwLock::new(None)),
            client_cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn get_active_client(&self) -> Option<DbClient> {
        self.active_client.read().await.clone()
    }

    pub async fn set_active_client(&self, client: DbClient) {
        *self.active_client.write().await = Some(client);
    }

    pub async fn clear_active_client(&self) -> Option<DbClient> {
        self.active_client.write().await.take()
    }

    pub async fn get_cached_client(&self, db_name: &str) -> Option<CachedDbClient> {
        self.client_cache.read().await.get(db_name).cloned()
    }

    pub async fn insert_cached_client(&self, db_name: String, client: CachedDbClient) {
        self.client_cache.write().await.insert(db_name, client);
    }

    pub async fn remove_cached_client(&self, db_name: &str) -> Option<CachedDbClient> {
        self.client_cache.write().await.remove(db_name)
    }

    pub async fn cached_client_names(&self) -> Vec<String> {
        self.client_cache.read().await.keys().cloned().collect()
    }

    /// 清除所有缓存的 DB 客户端
    pub async fn clear_cached_clients(&self) {
        self.client_cache.write().await.clear();
    }
}

/// AI 服务状态
pub struct AiState {
    rule_store: Arc<RwLock<RuleStore>>,
    policy: Arc<RwLock<Policy>>,
    knowledge_base: Arc<RwLock<KnowledgeBase>>,
    virtual_schema: Arc<RwLock<Option<SchemaResponse>>>,
}

impl AiState {
    pub fn new() -> Self {
        Self {
            rule_store: Arc::new(RwLock::new(RuleStore::default())),
            policy: Arc::new(RwLock::new(Policy::default())),
            knowledge_base: Arc::new(RwLock::new(KnowledgeBase::default())),
            virtual_schema: Arc::new(RwLock::new(None)),
        }
    }

    pub async fn get_rule_store(&self) -> RuleStore {
        self.rule_store.read().await.clone()
    }

    pub async fn set_rule_store(&self, store: RuleStore) {
        *self.rule_store.write().await = store;
    }

    pub async fn get_policy(&self) -> Policy {
        self.policy.read().await.clone()
    }

    pub async fn set_policy(&self, policy: Policy) {
        *self.policy.write().await = policy;
    }

    pub async fn get_knowledge_base(&self) -> KnowledgeBase {
        self.knowledge_base.read().await.clone()
    }

    pub async fn set_knowledge_base(&self, kb: KnowledgeBase) {
        *self.knowledge_base.write().await = kb;
    }

    pub async fn get_virtual_schema(&self) -> Option<SchemaResponse> {
        self.virtual_schema.read().await.clone()
    }

    pub async fn set_virtual_schema(&self, schema: SchemaResponse) {
        *self.virtual_schema.write().await = Some(schema);
    }

    /// 暴露 Arc 引用 — 供 AiService 增量更新规则命中计数（spawned task 需持有 Arc）
    pub fn rule_store_arc(&self) -> Arc<RwLock<RuleStore>> {
        self.rule_store.clone()
    }

    /// 暴露 Arc 引用 — 供 AiService 增量更新 knowledge base
    pub fn knowledge_base_arc(&self) -> Arc<RwLock<KnowledgeBase>> {
        self.knowledge_base.clone()
    }
}

/// 会话状态（事务 + 活跃查询）
pub struct SessionState {
    transaction_sessions: Arc<RwLock<HashMap<String, SharedTransactionSession>>>,
    active_queries: Arc<RwLock<HashMap<String, ActiveQueryHandle>>>,
}

impl SessionState {
    pub fn new() -> Self {
        Self {
            transaction_sessions: Arc::new(RwLock::new(HashMap::new())),
            active_queries: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn get_transaction(&self, session_id: &str) -> Option<SharedTransactionSession> {
        self.transaction_sessions.read().await.get(session_id).cloned()
    }

    pub async fn insert_transaction(&self, session_id: String, session: SharedTransactionSession) {
        self.transaction_sessions.write().await.insert(session_id, session);
    }

    pub async fn remove_transaction(&self, session_id: &str) -> Option<SharedTransactionSession> {
        self.transaction_sessions.write().await.remove(session_id)
    }

    pub async fn get_active_query(&self, token: &str) -> Option<ActiveQueryHandle> {
        self.active_queries.read().await.get(token).cloned()
    }

    pub async fn insert_active_query(&self, token: String, handle: ActiveQueryHandle) {
        self.active_queries.write().await.insert(token, handle);
    }

    pub async fn remove_active_query(&self, token: &str) {
        self.active_queries.write().await.remove(token);
    }

    pub async fn transaction_ids(&self) -> Vec<String> {
        self.transaction_sessions.read().await.keys().cloned().collect()
    }
}

/// Schema 缓存状态
pub struct SchemaState {
    schema_cache: Arc<RwLock<HashMap<String, CachedSchemaEntry>>>,
    table_schema_cache: Arc<RwLock<HashMap<String, CachedTableSchemaEntry>>>,
}

impl SchemaState {
    pub fn new() -> Self {
        Self {
            schema_cache: Arc::new(RwLock::new(HashMap::new())),
            table_schema_cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn get_schema(&self, db_id: &str) -> Option<CachedSchemaEntry> {
        self.schema_cache.read().await.get(db_id).cloned()
    }

    pub async fn insert_schema(&self, db_id: String, entry: CachedSchemaEntry) {
        self.schema_cache.write().await.insert(db_id, entry);
    }

    pub async fn get_table_schema(&self, table_name: &str) -> Option<CachedTableSchemaEntry> {
        self.table_schema_cache.read().await.get(table_name).cloned()
    }

    pub async fn insert_table_schema(&self, table_name: String, entry: CachedTableSchemaEntry) {
        self.table_schema_cache.write().await.insert(table_name, entry);
    }

    /// 清除所有缓存（用于 metadata cache invalidation）
    pub async fn clear_all(&self) {
        self.schema_cache.write().await.clear();
        self.table_schema_cache.write().await.clear();
    }

    /// 逐过期条目清理（read-lock → write-lock double-check）
    pub async fn retain_fresh(&self) {
        let now = std::time::Instant::now();
        self.schema_cache.write().await.retain(|_, v| v.expires_at > now);
        self.table_schema_cache.write().await.retain(|_, v| v.expires_at > now);
    }
}

// ── ServiceContext ────────────────────────────────────────

/// 统一服务上下文 — 替代原 AppState
///
/// 按领域分组内部状态，对外只暴露方法而非裸字段。
pub struct ServiceContext {
    config: Arc<RwLock<AppConfig>>,
    db_state: DbState,
    ai_state: AiState,
    session_state: SessionState,
    schema_state: SchemaState,
    timeouts: TimeoutPolicy,
    limits: RuntimeLimits,
    job_semaphore: Arc<Semaphore>,
    sql_history: Arc<RwLock<SqlHistoryStore>>,
}

impl ServiceContext {
    /// 从现有 AppState 创建 ServiceContext（迁移兼容）
    ///
    /// 此方法将 AppState 的所有字段转移给 ServiceContext，
    /// 之后 AppState 仅作为 ServiceContext 的薄壳。
    pub fn from_app_state(state: &crate::service::context::AppStateCompat) -> Self {
        Self {
            config: state.config.clone(),
            db_state: DbState {
                active_client: state.db_client.clone(),
                client_cache: state.db_client_cache.clone(),
            },
            ai_state: AiState {
                rule_store: state.rule_store.clone(),
                policy: state.policy.clone(),
                knowledge_base: state.knowledge_base.clone(),
                virtual_schema: state.virtual_schema.clone(),
            },
            session_state: SessionState {
                transaction_sessions: state.transaction_sessions.clone(),
                active_queries: state.active_queries.clone(),
            },
            schema_state: SchemaState {
                schema_cache: state.schema_cache.clone(),
                table_schema_cache: state.table_schema_cache.clone(),
            },
            timeouts: state.timeouts.clone(),
            limits: state.limits.clone(),
            job_semaphore: state.job_semaphore.clone(),
            sql_history: state.sql_history.clone(),
        }
    }

    // ── Config 访问 ──────────────────────────────────────

    pub async fn get_config(&self) -> AppConfig {
        self.config.read().await.clone()
    }

    pub async fn update_config(&self, new_config: AppConfig) {
        *self.config.write().await = new_config;
    }

    // ── 子状态引用 ──────────────────────────────────────

    pub fn db_state(&self) -> &DbState {
        &self.db_state
    }

    pub fn ai_state(&self) -> &AiState {
        &self.ai_state
    }

    pub fn session_state(&self) -> &SessionState {
        &self.session_state
    }

    pub fn schema_state(&self) -> &SchemaState {
        &self.schema_state
    }

    pub fn timeouts(&self) -> &TimeoutPolicy {
        &self.timeouts
    }

    pub fn limits(&self) -> &RuntimeLimits {
        &self.limits
    }

    pub fn job_semaphore(&self) -> &Arc<Semaphore> {
        &self.job_semaphore
    }

    pub async fn get_sql_history(&self) -> SqlHistoryStore {
        self.sql_history.read().await.clone()
    }

    /// 获取当前活跃的数据库 ID
    pub async fn active_db_id(&self) -> Option<String> {
        self.config.read().await.active_db_id.clone()
    }

    /// 更新 SQL 执行历史
    pub async fn update_sql_history(&self, store: SqlHistoryStore) {
        *self.sql_history.write().await = store;
    }
}

/// AppState 兼容结构 — 用于从现有 AppState 字段创建 ServiceContext
///
/// 这是一个临时结构，在迁移完成后将被删除。
/// 所有字段直接从原 AppState 的 Arc<RwLock<...>> 引用复制过来。
pub struct AppStateCompat {
    pub config: Arc<RwLock<AppConfig>>,
    pub db_client: Arc<RwLock<Option<DbClient>>>,
    pub db_client_cache: Arc<RwLock<HashMap<String, CachedDbClient>>>,
    pub virtual_schema: Arc<RwLock<Option<SchemaResponse>>>,
    pub schema_cache: Arc<RwLock<HashMap<String, CachedSchemaEntry>>>,
    pub table_schema_cache: Arc<RwLock<HashMap<String, CachedTableSchemaEntry>>>,
    pub rule_store: Arc<RwLock<RuleStore>>,
    pub policy: Arc<RwLock<Policy>>,
    pub sql_history: Arc<RwLock<SqlHistoryStore>>,
    pub knowledge_base: Arc<RwLock<KnowledgeBase>>,
    pub active_queries: Arc<RwLock<HashMap<String, ActiveQueryHandle>>>,
    pub transaction_sessions: Arc<RwLock<HashMap<String, SharedTransactionSession>>>,
    pub timeouts: TimeoutPolicy,
    pub limits: RuntimeLimits,
    pub job_semaphore: Arc<Semaphore>,
}

// ── 活跃查询辅助 ────────────────────────────────────────

pub async fn register_active_query(ctx: &ServiceContext, token: String, handle: ActiveQueryHandle) {
    ctx.session_state().insert_active_query(token, handle).await;
}

pub async fn unregister_active_query(ctx: &ServiceContext, token: &str) {
    ctx.session_state().remove_active_query(token).await;
}

pub async fn cancel_active_query(ctx: &ServiceContext, cancel_token: &str) -> Result<bool, ServiceError> {
    let handle = ctx.session_state().get_active_query(cancel_token).await;
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
                Err(ServiceError::InternalError(e.to_string()))
            }
        }
    }
}

pub async fn resolve_transaction_db_id(ctx: &ServiceContext, db_id: Option<&str>) -> Option<String> {
    if let Some(value) = db_id.map(str::trim).filter(|v| !v.is_empty()) {
        return Some(value.to_string());
    }
    ctx.active_db_id().await
}