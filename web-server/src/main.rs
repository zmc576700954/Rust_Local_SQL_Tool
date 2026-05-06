use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use core_lib::error::AppError;

use axum::extract::Multipart;
use core_lib::transfer::{TransferConfig, TransferEngine};
use std::io::Write;

// ----------------- Transfer Handlers -----------------

#[derive(serde::Serialize)]
struct UploadResponse {
    columns: Vec<String>,
    preview_data: Vec<Vec<String>>,
    source_path: String,
}

async fn transfer_upload(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<UploadResponse>, AppError> {
    let limits = state.limits.clone();
    let mut file_data = None;
    let mut file_name_opt = None;
    let mut delimiter = b',';
    let mut _encoding = "utf-8".to_string();

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::BadRequest(e.to_string()))?
    {
        if let Some(name) = field.name() {
            let name_str = name.to_string();
            if name_str == "file" {
                file_name_opt = Some(field.file_name().unwrap_or("upload.csv").to_string());
                let data = field
                    .bytes()
                    .await
                    .map_err(|e| AppError::BadRequest(e.to_string()))?;
                if data.len() as u64 > limits.max_file_bytes {
                    return Err(AppError::PayloadTooLarge(format!(
                        "upload too large: bytes={}B, max={}B",
                        data.len(),
                        limits.max_file_bytes
                    )));
                }
                file_data = Some(data);
            } else if name_str == "delimiter" {
                let val = field
                    .text()
                    .await
                    .map_err(|e| AppError::BadRequest(e.to_string()))?;
                if !val.is_empty() {
                    delimiter = val.as_bytes()[0];
                }
            } else if name_str == "encoding" {
                _encoding = field
                    .text()
                    .await
                    .map_err(|e| AppError::BadRequest(e.to_string()))?;
            }
        }
    }

    if let (Some(data), Some(file_name)) = (file_data, file_name_opt) {
        ensure_temp_quota(&limits, data.len() as u64).await?;
        let temp_path = format!(
            "{}/{}",
            limits.temp_dir.trim_end_matches('/'),
            uuid::Uuid::new_v4()
        );
        let mut f = std::fs::File::create(&temp_path)
            .map_err(|e| AppError::InternalError(e.to_string()))?;

        // If SQL file, just return it as a single column or special format?
        // Actually, if it's SQL, we could just return the raw SQL in DML and skip mapping.
        // But for now let's just write the data.
        f.write_all(&data)
            .map_err(|e| AppError::InternalError(e.to_string()))?;

        if file_name.ends_with(".sql") {
            // For SQL files, we can just return a single column and the content as preview
            // Or we can just read the file content and return as DML directly later.
            // Let's return a dummy mapping for SQL.
            let content = String::from_utf8_lossy(&data).to_string();
            return Ok(Json(UploadResponse {
                columns: vec!["sql_content".to_string()],
                preview_data: vec![vec![content.chars().take(100).collect::<String>() + "..."]],
                source_path: temp_path,
            }));
        }

        // Parse CSV/TXT
        let result = TransferEngine::parse_local_file(&temp_path, delimiter, true)
            .map_err(|e| AppError::BadRequest(e.to_string()))?;

        return Ok(Json(UploadResponse {
            columns: result.columns,
            preview_data: result.preview_data,
            source_path: temp_path,
        }));
    }

    Err(AppError::BadRequest("No file uploaded".to_string()))
}

async fn transfer_execute(
    State(state): State<AppState>,
    Json(mut config): Json<TransferConfig>,
) -> Result<Json<serde_json::Value>, AppError> {
    if config.source_type == "network_db" {
        if let Some(ref db_id) = config.source_db_id {
            let app_config = state.config.read().await.clone();
            if let Some(conn) = app_config.db_connections.iter().find(|c| &c.id == db_id) {
                config.source_url = Some(conn.url.clone());
            } else {
                return Err(AppError::BadRequest(
                    "Source DB connection not found".into(),
                ));
            }
        }
    }

    let dml = TransferEngine::execute_transfer(&config)
        .await
        .map_err(|e| AppError::BadRequest(e.to_string()))?;
    if config.source_type == "local_file" {
        if let Some(p) = config.source_path.as_ref() {
            let _ = tokio::fs::remove_file(p).await;
        }
    }
    Ok(Json(serde_json::json!({ "dml": dml })))
}

// ----------------- Rule Management Handlers -----------------

async fn get_rules(State(state): State<AppState>) -> Result<Json<Vec<Rule>>, AppError> {
    let store = state.rule_store.read().await;
    Ok(Json(store.rules.clone()))
}

#[derive(Deserialize)]
struct ImportRequest {
    table_name: String,
    data: Vec<std::collections::HashMap<String, serde_json::Value>>,
    mapping: std::collections::HashMap<String, String>, // db_column -> source_field
    skip_errors: bool,
}

#[derive(Serialize)]
struct ImportResponse {
    inserted: usize,
    errors: usize,
    error_details: Vec<String>,
}

async fn import_data(
    State(state): State<AppState>,
    Json(req): Json<ImportRequest>,
) -> Result<Json<ImportResponse>, AppError> {
    let db_client = state
        .db_client
        .read()
        .await
        .clone()
        .ok_or_else(|| AppError::BadRequest("Database not connected".to_string()))?;

    let table_name = req.table_name;
    let mut inserted = 0;
    let mut errors = 0;
    let mut error_details = Vec::new();

    // Filter mapping to only include mapped columns
    let mapped_cols: Vec<(String, String)> = req
        .mapping
        .into_iter()
        .filter(|(_, src)| !src.is_empty())
        .collect();

    if mapped_cols.is_empty() {
        return Err(AppError::BadRequest("No columns mapped".to_string()));
    }

    let mut db_col_names: Vec<String> = Vec::with_capacity(mapped_cols.len());
    for (db, _) in &mapped_cols {
        db_col_names.push(quote_mysql_ident(db)?);
    }
    let col_list = db_col_names.join(", ");
    let placeholders = vec!["?"; mapped_cols.len()].join(", ");
    let table_ident = quote_mysql_ident(&table_name)?;
    let sql = format!(
        "INSERT INTO {} ({}) VALUES ({})",
        table_ident, col_list, placeholders
    );

    // Process in batches or row by row. For simplicity and error handling per row, we can do row by row or small batches.
    // If skip_errors is true, we must be able to isolate errors. Row by row is safer for skip_errors.
    for (i, row) in req.data.iter().enumerate() {
        let mut query = sqlx::query(&sql);

        for (_, src_field) in &mapped_cols {
            if let Some(val) = row.get(src_field) {
                match val {
                    serde_json::Value::Null => query = query.bind(None::<String>),
                    serde_json::Value::Bool(b) => query = query.bind(b),
                    serde_json::Value::Number(n) => {
                        if let Some(i) = n.as_i64() {
                            query = query.bind(i);
                        } else if let Some(f) = n.as_f64() {
                            query = query.bind(f);
                        } else {
                            query = query.bind(n.to_string());
                        }
                    }
                    serde_json::Value::String(s) => query = query.bind(s),
                    _ => query = query.bind(val.to_string()),
                }
            } else {
                // If source field is missing in data, bind null
                query = query.bind(None::<String>);
            }
        }

        match query.execute(&db_client.pool).await {
            Ok(_) => {
                inserted += 1;
            }
            Err(e) => {
                errors += 1;
                let err_msg = format!("Row {}: {}", i + 1, e);
                error_details.push(err_msg.clone());
                if !req.skip_errors {
                    return Err(AppError::BadRequest(err_msg));
                }
            }
        }
    }

    Ok(Json(ImportResponse {
        inserted,
        errors,
        error_details,
    }))
}

#[derive(Deserialize)]
struct SaveRuleRequest {
    prompt: String,
    sql: String,
}

async fn save_rule(
    State(state): State<AppState>,
    Json(req): Json<SaveRuleRequest>,
) -> Result<Json<Rule>, AppError> {
    // Call AI to extract templates
    let planner = state.planner.read().await.clone();
    let sql_template = match planner.generate_rule_template(&req.prompt, &req.sql).await {
        Ok(res) => res,
        Err(_e) => req.sql.clone(), // Fallback to raw sql if AI extraction fails
    };

    let new_rule = Rule {
        id: uuid::Uuid::new_v4().to_string(),
        rule_type: if sql_template.contains("{{") {
            RuleType::Template
        } else {
            RuleType::Module
        },
        prompt_pattern: req.prompt,
        sql_template,
        hit_count: 0,
        updated_at: chrono::Utc::now().timestamp(),
    };

    let store_clone = {
        let mut store = state.rule_store.write().await;
        store.add_rule(new_rule.clone());
        store.clone()
    };
    store_clone
        .save()
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;

    // Evolve policy
    let store_clone = state.policy.clone();
    tokio::spawn(async move {
        match PolicyStore::evolve_policy("save_rule").await {
            Ok(new_policy) => {
                let mut policy_write = store_clone.write().await;
                *policy_write = new_policy;
                tracing::info!("Policy evolved after save_rule");
            }
            Err(e) => {
                tracing::error!("Failed to evolve policy: {:?}", e);
            }
        }
    });

    Ok(Json(new_rule))
}

#[derive(Deserialize)]
struct DeleteRuleRequest {
    id: String,
}

async fn delete_rule(
    State(state): State<AppState>,
    Json(req): Json<DeleteRuleRequest>,
) -> Result<StatusCode, AppError> {
    let store_clone = {
        let mut store = state.rule_store.write().await;
        store
            .delete_rule(&req.id)
            .map_err(|e| AppError::BadRequest(e.to_string()))?;
        store.clone()
    };
    store_clone
        .save()
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    Ok(StatusCode::OK)
}

#[derive(Deserialize)]
struct NavicatParseRequest {
    xml_content: String,
}

#[derive(Serialize)]
struct NavicatParseResponse {
    connections: Vec<NavicatConnection>,
}

async fn parse_navicat(
    Json(req): Json<NavicatParseRequest>,
) -> Result<Json<NavicatParseResponse>, AppError> {
    let connections = NavicatParser::parse_ncx(&req.xml_content)
        .map_err(|e| AppError::BadRequest(e.to_string()))?;

    Ok(Json(NavicatParseResponse { connections }))
}

#[derive(Deserialize)]
struct DbTestRequest {
    host: String,
    port: Option<u16>,
    username: String,
    password: String,
}

#[derive(Serialize)]
struct DbTestResponse {
    databases: Vec<String>,
}

async fn db_test(Json(req): Json<DbTestRequest>) -> Result<Json<DbTestResponse>, AppError> {
    let policy = TimeoutPolicy::default();
    let port = req.port.unwrap_or(3306);

    use sqlx::mysql::MySqlConnectOptions;
    use sqlx::Row;

    let mut options = MySqlConnectOptions::new()
        .host(&req.host)
        .port(port)
        .username(&req.username)
        .database("mysql");

    if !req.password.is_empty() {
        options = options.password(&req.password);
    }

    let pool_future = sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(1)
        .connect_with(options);

    let pool = tokio::time::timeout(policy.db_connect, pool_future)
        .await
        .map_err(|_| {
            AppError::Timeout(
                "连接数据库超时（已超过 10 秒），请检查网络、IP 或防火墙配置是否正确。".to_string(),
            )
        })?
        .map_err(|e| {
            let msg = e.to_string();
            if msg.to_lowercase().contains("access denied") {
                let body = serde_json::json!({
                    "error": "db_auth_failed",
                    "message": "数据库账号或密码错误，请检查后重试。",
                    "detail": msg
                })
                .to_string();
                AppError::Unauthorized(body)
            } else {
                let body = serde_json::json!({
                    "error": "db_connect_failed",
                    "message": "无法连接到数据库服务器，请检查地址/端口/网络后重试。",
                    "detail": msg
                })
                .to_string();
                AppError::BadRequest(body)
            }
        })?;

    let rows_future = sqlx::query("SHOW DATABASES").fetch_all(&pool);
    let rows = tokio::time::timeout(policy.db_query, rows_future)
        .await
        .map_err(|_| {
            AppError::Timeout(
                "连接数据库超时（已超过 10 秒），请检查网络、IP 或防火墙配置是否正确。".to_string(),
            )
        })?
        .map_err(|e| AppError::InternalError(e.to_string()))?;

    let mut databases = Vec::new();
    for row in rows {
        let name: String = row.try_get(0).unwrap_or_default();
        if !name.is_empty() {
            databases.push(name);
        }
    }

    Ok(Json(DbTestResponse { databases }))
}
use core_lib::{
    ai::{
        gateway::{AiError, AiGateway},
        planner::Planner,
        policy_store::{Policy, PolicyStore},
    },
    ai_agent::{AiRouter, DbDialect},
    config::{AiModel, AppConfig, DbType},
    crud::{CrudManager, CrudRequest},
    db::DbClient,
    knowledge_base::{Knowledge, KnowledgeBase},
    mysql_sync::{CompareResult, MySqlDataSyncEngine, PreviewResult, SyncMode},
    navicat::{NavicatConnection, NavicatParser},
    offline_parser::OfflineParser,
    rule_engine::{Rule, RuleStore, RuleType},
    schema::{SchemaExtractor, SchemaResponse, TableWithDetails},
    sql_history::{SqlHistory, SqlHistoryStore},
    tools::{DataExporter, DdlEngine, MockDataGenerator, SyncEngine},
};
use core_lib::timeout_policy::TimeoutPolicy;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{RwLock, Semaphore};
use tower_http::{
    cors::CorsLayer,
    services::{ServeDir, ServeFile},
};

#[derive(Debug, Clone)]
struct RuntimeLimits {
    temp_dir: String,
    temp_quota_bytes: u64,
    max_file_bytes: u64,
    max_job_concurrency: usize,
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

fn dir_size_bytes_sync(path: &std::path::Path) -> u64 {
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

#[cfg(test)]
fn test_state_with_config(config: AppConfig) -> AppState {
    let gateway = AiGateway::new(config.clone());
    let planner = Planner::new(gateway);
    AppState {
        config: Arc::new(RwLock::new(config)),
        db_client: Arc::new(RwLock::new(None)),
        planner: Arc::new(RwLock::new(planner)),
        virtual_schema: Arc::new(RwLock::new(None)),
        rule_store: Arc::new(RwLock::new(RuleStore::default())),
        policy: Arc::new(RwLock::new(Policy::default())),
        sql_history: Arc::new(RwLock::new(SqlHistoryStore::default())),
        knowledge_base: Arc::new(RwLock::new(KnowledgeBase::default())),
        sync_jobs: Arc::new(RwLock::new(HashMap::new())),
        perf_sync_jobs: Arc::new(RwLock::new(HashMap::new())),
        tool_jobs: Arc::new(RwLock::new(HashMap::new())),
        tool_job_handles: Arc::new(RwLock::new(HashMap::new())),
        timeouts: TimeoutPolicy::default(),
        limits: RuntimeLimits::default(),
        job_semaphore: Arc::new(Semaphore::new(1)),
    }
}

async fn dir_size_bytes(path: &std::path::Path) -> Result<u64, AppError> {
    let p = path.to_path_buf();
    tokio::task::spawn_blocking(move || dir_size_bytes_sync(&p))
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))
}

async fn ensure_temp_quota(limits: &RuntimeLimits, additional_bytes: u64) -> Result<(), AppError> {
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MySqlSyncStage {
    Compare,
    Preview,
    Deploy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MySqlSyncJobStatus {
    Pending,
    Running,
    Completed,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct MySqlSyncProgress {
    current: u64,
    total: u64,
    message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DeployResult {
    affected_rows: u64,
    statements: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MySqlSyncJob {
    job_id: String,
    stage: MySqlSyncStage,
    status: MySqlSyncJobStatus,
    progress: MySqlSyncProgress,
    source_db_id: String,
    target_db_id: String,
    table_name: String,
    primary_key: String,
    mode: SyncMode,
    chunk_size: usize,
    created_at: i64,
    updated_at: i64,
    compare_ms: Option<u128>,
    preview_ms: Option<u128>,
    deploy_ms: Option<u128>,
    compare: Option<CompareResult>,
    preview: Option<PreviewResult>,
    deploy: Option<DeployResult>,
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PerfSyncStage {
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
enum PerfSyncJobStatus {
    Pending,
    Running,
    Completed,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct PerfSyncProgress {
    current: u64,
    total: u64,
    message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PerfSyncTableSpec {
    table_name: String,
    primary_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PerfSyncLoadgenRequest {
    tier: Option<String>,
    fill: Option<bool>,
    reset: Option<bool>,
    inject: Option<bool>,
    seed: Option<u64>,
    batch: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PerfSyncStartRequest {
    source_db_id: String,
    target_db_id: String,
    tier: Option<String>,
    tables: Option<Vec<PerfSyncTableSpec>>,
    chunk_size: Option<usize>,
    max_rows: Option<usize>,
    loadgen: Option<PerfSyncLoadgenRequest>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PerfSyncTableCount {
    source: u64,
    target: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PerfSyncTableSyncReport {
    table_name: String,
    primary_key: String,
    compare_ms: u128,
    preview_ms: u128,
    deploy_ms: u128,
    compare_chunks: usize,
    different_chunks: usize,
    insert_count: usize,
    update_count: usize,
    delete_count: usize,
    statements: usize,
    truncated: bool,
    affected_rows: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PerfSyncTableVerifyReport {
    table_name: String,
    different_chunks: usize,
    chunks: usize,
    verify_ms: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PerfSyncModeReport {
    mode: SyncMode,
    injected_counts: HashMap<String, PerfSyncTableCount>,
    tables: Vec<PerfSyncTableSyncReport>,
    verify: Vec<PerfSyncTableVerifyReport>,
    passed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PerfSyncReport {
    baseline_counts: HashMap<String, PerfSyncTableCount>,
    loadgen: Option<core_lib::loadgen::LoadgenReport>,
    mirror: PerfSyncModeReport,
    upsert_only: PerfSyncModeReport,
    stage_ms: HashMap<String, u128>,
    elapsed_ms: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PerfSyncJob {
    job_id: String,
    stage: PerfSyncStage,
    status: PerfSyncJobStatus,
    progress: PerfSyncProgress,
    request: PerfSyncStartRequest,
    created_at: i64,
    updated_at: i64,
    report: Option<PerfSyncReport>,
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ToolJobKind {
    Export,
    Import,
    ImportSql,
    GoLive,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ToolJobStatus {
    Pending,
    Running,
    Completed,
    Error,
    Canceled,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ToolJobProgress {
    current: u64,
    total: Option<u64>,
    message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ToolJobArtifacts {
    data_path: Option<String>,
    manifest_path: Option<String>,
    file_name: Option<String>,
    content_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ToolJob {
    job_id: String,
    kind: ToolJobKind,
    status: ToolJobStatus,
    progress: ToolJobProgress,
    created_at: i64,
    updated_at: i64,
    elapsed_ms: Option<u128>,
    artifacts: Option<ToolJobArtifacts>,
    result: Option<serde_json::Value>,
    error: Option<String>,
}

#[derive(Clone)]
struct AppState {
    config: Arc<RwLock<AppConfig>>,
    db_client: Arc<RwLock<Option<DbClient>>>,
    planner: Arc<RwLock<Planner>>,
    virtual_schema: Arc<RwLock<Option<SchemaResponse>>>,
    rule_store: Arc<RwLock<RuleStore>>,
    policy: Arc<RwLock<Policy>>,
    sql_history: Arc<RwLock<SqlHistoryStore>>,
    knowledge_base: Arc<RwLock<KnowledgeBase>>,
    sync_jobs: Arc<RwLock<HashMap<String, MySqlSyncJob>>>,
    perf_sync_jobs: Arc<RwLock<HashMap<String, PerfSyncJob>>>,
    tool_jobs: Arc<RwLock<HashMap<String, ToolJob>>>,
    tool_job_handles: Arc<RwLock<HashMap<String, tokio::task::AbortHandle>>>,
    timeouts: TimeoutPolicy,
    limits: RuntimeLimits,
    job_semaphore: Arc<Semaphore>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();
    tracing::info!("Starting Local AI SQL Assistant Web Server...");

    let config = AppConfig::load().await.unwrap_or_default().normalize();
    let timeouts = TimeoutPolicy::default();
    let limits = RuntimeLimits::default();
    let job_semaphore = Arc::new(Semaphore::new(limits.max_job_concurrency));

    // Initialize DB Client if configured
    let mut db_client = None;
    if let Some(ref url) = config.get_active_db_url() {
        match DbClient::new(url).await {
            Ok(client) => {
                tracing::info!("Connected to database");
                db_client = Some(client);
            }
            Err(e) => tracing::error!("Failed to connect to database: {}", e),
        }
    }

    let gateway = AiGateway::new(config.clone());
    let planner = Planner::new(gateway);
    let rule_store = RuleStore::load().await.unwrap_or_default();
    let policy = PolicyStore::load_effective()
        .await
        .unwrap_or_else(|_| Policy::default());
    let sql_history = SqlHistoryStore::load().await.unwrap_or_default();
    let knowledge_base = KnowledgeBase::load().await.unwrap_or_default();

    let state = AppState {
        config: Arc::new(RwLock::new(config)),
        db_client: Arc::new(RwLock::new(db_client)),
        planner: Arc::new(RwLock::new(planner)),
        virtual_schema: Arc::new(RwLock::new(None)),
        rule_store: Arc::new(RwLock::new(rule_store)),
        policy: Arc::new(RwLock::new(policy)),
        sql_history: Arc::new(RwLock::new(sql_history)),
        knowledge_base: Arc::new(RwLock::new(knowledge_base)),
        sync_jobs: Arc::new(RwLock::new(HashMap::new())),
        perf_sync_jobs: Arc::new(RwLock::new(HashMap::new())),
        tool_jobs: Arc::new(RwLock::new(HashMap::new())),
        tool_job_handles: Arc::new(RwLock::new(HashMap::new())),
        timeouts,
        limits: limits.clone(),
        job_semaphore,
    };

    let api = Router::new()
        .route("/config", get(get_config).post(update_config))
        .route("/db/test", post(db_test))
        .route("/schema", get(get_schema))
        .route("/schema/parse", post(parse_schema))
        .route("/chat", post(chat_to_sql))
        .route("/execute", post(execute_sql))
        .route("/policy", get(get_policy))
        .route("/policy/reset", post(reset_policy))
        .route("/policy/snapshot", post(snapshot_policy))
        .route("/policy/rollback", post(rollback_policy))
        .route("/crud/insert", post(crud_insert))
        .route("/crud/update", post(crud_update))
        .route("/crud/delete", post(crud_delete))
        .route("/navicat/parse", post(parse_navicat))
        .route("/rules", get(get_rules))
        .route("/rules/save", post(save_rule))
        .route("/rules/delete", post(delete_rule))
        .route("/table/data", get(get_table_data))
        .route("/table/schema", get(get_table_schema))
        .route("/table/ddl/preview", post(preview_ddl))
        .route("/table/ddl", post(execute_ddl))
        .route("/tools/mock-data", post(generate_mock_data))
        .route("/tools/export", post(export_data))
        .route("/tools/import", post(import_data))
        .route("/tools/jobs/export/start", post(export_job_start))
        .route("/tools/jobs/import/start", post(import_job_start))
        .route("/tools/jobs/import-sql/start", post(import_sql_job_start))
        .route("/tools/jobs/go-live/start", post(go_live_job_start))
        .route("/tools/go-live/reports", get(go_live_reports_list))
        .route("/tools/go-live/audit", get(go_live_audit_list))
        .route("/tools/jobs/:job_id", get(tool_job_status))
        .route("/tools/jobs/:job_id/cancel", post(tool_job_cancel))
        .route(
            "/tools/jobs/:job_id/artifacts/:artifact",
            get(tool_job_artifact_download),
        )
        .route("/tools/schema-sync/diff", post(sync_schema_diff))
        .route("/tools/schema-sync/ddl", post(sync_schema_ddl))
        .route("/tools/data-sync/diff", post(sync_data_diff))
        .route("/tools/data-sync/dml", post(sync_data_dml))
        .route("/tools/data-sync/compare", post(mysql_sync_compare))
        .route("/tools/data-sync/preview", post(mysql_sync_preview))
        .route("/tools/data-sync/deploy", post(mysql_sync_deploy))
        .route("/tools/data-sync/jobs/:job_id", get(mysql_sync_job_status))
        .route("/tools/mysql-sync/compare", post(mysql_sync_compare))
        .route("/tools/mysql-sync/preview", post(mysql_sync_preview))
        .route("/tools/mysql-sync/deploy", post(mysql_sync_deploy))
        .route("/tools/mysql-sync/jobs/:job_id", get(mysql_sync_job_status))
        .route("/tools/perf-sync/start", post(perf_sync_start))
        .route("/tools/perf-sync/check", post(perf_sync_check))
        .route("/tools/perf-sync/jobs/:job_id", get(perf_sync_job_status))
        .route("/tools/data-transfer/upload", post(transfer_upload))
        .route("/tools/data-transfer/execute", post(transfer_execute))
        .route("/sql/history", get(get_history).post(clear_history))
        .route("/sql/explain", post(explain_sql))
        .route("/api/ai/models", get(ai_models))
        .route("/api/ai/provider/models", post(fetch_provider_models))
        .route("/api/ai/health", get(ai_health))
        .route("/api/ai/query", post(ai_query))
        .route("/api/ai/explain_error", post(ai_explain_error))
        .route("/api/ai/knowledge", get(get_knowledge))
        .route("/api/ai/knowledge", post(add_knowledge))
        .route("/api/ai/knowledge", axum::routing::put(update_knowledge))
        .route("/api/ai/knowledge/delete", post(delete_knowledge))
        .layer(axum::extract::DefaultBodyLimit::max(
            (limits.max_file_bytes.min(usize::MAX as u64)) as usize,
        ));

    let dist_dir =
        std::env::var("WEB_UI_DIST_DIR").unwrap_or_else(|_| "web-ui/dist".to_string());
    let index_path = std::path::Path::new(&dist_dir).join("index.html");
    let static_service =
        ServeDir::new(dist_dir).not_found_service(ServeFile::new(index_path));

    let app = Router::new()
        .nest("/backend", api)
        .fallback_service(static_service)
        .layer(CorsLayer::permissive())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await?;
    tracing::info!("Server listening on http://0.0.0.0:3000");
    axum::serve(listener, app).await?;

    Ok(())
}

fn map_ai_error(e: AiError) -> AppError {
    match e {
        AiError::Auth(msg) => AppError::AiAuth(msg),
        AiError::Forbidden(msg) => AppError::AiForbidden(msg),
        AiError::ModelNotFound(msg) => AppError::AiModelNotFound(msg),
        AiError::NoTokens => AppError::BadRequest("No tokens available in pool".to_string()),
        AiError::RateLimited(msg) => AppError::AiRateLimited(msg),
        AiError::ServerError(msg) => AppError::ExternalServiceUnavailable(msg),
        AiError::Network(e) => {
            if e.is_timeout() {
                AppError::AiAgentTimeout(e.to_string())
            } else if e.to_string().to_lowercase().contains("proxy")
                || e.to_string().to_lowercase().contains("tunnel")
            {
                AppError::AiProxy(e.to_string())
            } else if e.is_connect() {
                AppError::ExternalServiceUnavailable(e.to_string())
            } else {
                AppError::AiAgentTimeout(e.to_string())
            }
        }
        AiError::ApiError(msg) => AppError::InternalError(msg),
    }
}

use core_lib::config::AiProvider;

#[derive(Deserialize)]
struct FetchModelsRequest {
    provider: AiProvider,
    api_key: String,
    base_url: Option<String>,
}

#[derive(Serialize)]
struct FetchModelsResponse {
    models: Vec<String>,
}

async fn fetch_provider_models(
    State(state): State<AppState>,
    Json(req): Json<FetchModelsRequest>,
) -> Result<Json<FetchModelsResponse>, AppError> {
    let gateway = state.planner.read().await.gateway.clone();
    let models = gateway
        .fetch_provider_models(req.provider, req.api_key, req.base_url)
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    Ok(Json(FetchModelsResponse { models }))
}

#[derive(Serialize)]
struct AiModelsResponse {
    models: Vec<AiModel>,
    active_model_id: Option<String>,
    active_tier: String,
}

async fn ai_models(State(state): State<AppState>) -> Result<Json<AiModelsResponse>, AppError> {
    let config = state.config.read().await.clone();
    Ok(Json(AiModelsResponse {
        models: config.ai_models,
        active_model_id: config.active_model_id,
        active_tier: config.active_tier,
    }))
}

async fn ai_health(
    State(state): State<AppState>,
) -> Result<Json<core_lib::ai::gateway::AiHealthReport>, AppError> {
    let config = state.config.read().await.clone();
    let gateway = AiGateway::new(config);
    let report = gateway.health_check().await.map_err(map_ai_error)?;
    Ok(Json(report))
}

#[derive(Deserialize)]
struct AiQueryRequest {
    query: String,
    chat_history: Option<Vec<serde_json::Value>>,
}

#[derive(Serialize)]
struct AiQueryResponse {
    sql: String,
    explanation: Option<String>,
}

async fn ai_query(
    State(state): State<AppState>,
    Json(req): Json<AiQueryRequest>,
) -> Result<Json<AiQueryResponse>, AppError> {
    let config = state.config.read().await.clone();
    let url = config.get_active_db_url().unwrap_or_default();
    let dialect = DbDialect::from_url(&url);
    let db_conn_id = config.active_db_id.clone();

    let schema = get_schema_internal(&state).await;

    let knowledge = {
        let kb = state.knowledge_base.read().await;
        kb.retrieve(db_conn_id.as_deref(), &req.query, 5) // get top 5 relevant items
    };

    let gateway = AiGateway::new(config);
    let router = AiRouter::new(gateway);

    match router
        .dispatch_query(
            dialect,
            &req.query,
            schema.as_ref(),
            &knowledge,
            req.chat_history,
        )
        .await
    {
        Ok(result_str) => {
            if result_str.trim().is_empty() {
                return Err(AppError::ParseError(
                    "AI 返回为空，无法解析 SQL。建议：检查 API Key/代理/限流，或降低 tier/max_tokens 后重试。"
                        .to_string(),
                ));
            }
            let intent = core_lib::ai::extractor::extract_sql_intent(&result_str);
            if intent.sql.trim().is_empty() {
                return Err(AppError::ParseError(
                    intent
                        .explanation
                        .unwrap_or_else(|| "AI 返回无法解析为 SQL。".to_string()),
                ));
            }
            Ok(Json(AiQueryResponse {
                sql: intent.sql,
                explanation: intent.explanation,
            }))
        }
        Err(e) => Err(map_ai_error(e)),
    }
}

#[derive(Deserialize)]
struct AiExplainErrorRequest {
    error_msg: String,
    failed_query: String,
}

#[derive(Serialize)]
struct AiExplainErrorResponse {
    explanation: String,
    fixed_query: Option<String>,
}

async fn ai_explain_error(
    State(state): State<AppState>,
    Json(req): Json<AiExplainErrorRequest>,
) -> Result<Json<AiExplainErrorResponse>, AppError> {
    let config = state.config.read().await.clone();
    let url = config.get_active_db_url().unwrap_or_default();
    let dialect = DbDialect::from_url(&url);

    let schema = get_schema_internal(&state).await;

    let gateway = AiGateway::new(config);
    let router = AiRouter::new(gateway);

    match router
        .explain_error(dialect, &req.error_msg, &req.failed_query, schema.as_ref())
        .await
    {
        Ok(result_str) => {
            if result_str.trim().is_empty() {
                return Ok(Json(AiExplainErrorResponse {
                    explanation:
                        "AI 返回为空，无法解析解释结果。建议：检查 API Key/代理/限流，或降低 tier/max_tokens 后重试。"
                            .to_string(),
                    fixed_query: None,
                }));
            }

            let cleaned = core_lib::ai::extractor::extract_code_block(result_str.trim(), "json");
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(cleaned.trim()) {
                let explanation = val["explanation"]
                    .as_str()
                    .unwrap_or(&result_str)
                    .to_string();
                let fixed_query = val["fixed_query"].as_str().map(|s| s.to_string());
                Ok(Json(AiExplainErrorResponse {
                    explanation,
                    fixed_query,
                }))
            } else {
                let preview = result_str.trim().chars().take(200).collect::<String>();
                Ok(Json(AiExplainErrorResponse {
                    explanation: format!(
                        "AI 返回非 JSON，无法解析解释结果。返回预览：{}。建议：切换 provider/model 或关闭 JSON 模式重试。",
                        preview
                    ),
                    fixed_query: None,
                }))
            }
        }
        Err(e) => Err(map_ai_error(e)),
    }
}

// ----------------- Knowledge Base API Handlers -----------------

#[derive(Deserialize)]
struct GetKnowledgeRequest {
    db_connection_id: Option<String>,
}

async fn get_knowledge(
    State(state): State<AppState>,
    axum::extract::Query(req): axum::extract::Query<GetKnowledgeRequest>,
) -> Result<Json<Vec<Knowledge>>, AppError> {
    let kb = state.knowledge_base.read().await;
    let mut items: Vec<Knowledge> = kb
        .items
        .iter()
        .filter(|i| {
            if let Some(ref conn_id) = req.db_connection_id {
                i.db_connection_id.as_deref() == Some(conn_id) || i.db_connection_id.is_none()
            } else {
                true
            }
        })
        .cloned()
        .collect();

    // Sort by updated_at descending
    items.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Ok(Json(items))
}

async fn add_knowledge(
    State(state): State<AppState>,
    Json(item): Json<Knowledge>,
) -> Result<Json<Knowledge>, AppError> {
    let mut item = item;
    let kb_clone = {
        let mut kb = state.knowledge_base.write().await;
        kb.add_item(item.clone());
        // update the ID in case it was generated
        item =
            kb.items.last().cloned().ok_or_else(|| {
                AppError::InternalError("Failed to add knowledge item".to_string())
            })?;
        kb.clone()
    };
    kb_clone
        .save()
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    Ok(Json(item))
}

async fn update_knowledge(
    State(state): State<AppState>,
    Json(item): Json<Knowledge>,
) -> Result<Json<Knowledge>, AppError> {
    let kb_clone = {
        let mut kb = state.knowledge_base.write().await;
        kb.update_item(item.clone())
            .map_err(|e| AppError::BadRequest(e.to_string()))?;
        kb.clone()
    };
    kb_clone
        .save()
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    Ok(Json(item))
}

#[derive(Deserialize)]
struct DeleteKnowledgeRequest {
    id: String,
}

async fn delete_knowledge(
    State(state): State<AppState>,
    Json(req): Json<DeleteKnowledgeRequest>,
) -> Result<StatusCode, AppError> {
    let kb_clone = {
        let mut kb = state.knowledge_base.write().await;
        kb.delete_item(&req.id)
            .map_err(|e| AppError::BadRequest(e.to_string()))?;
        kb.clone()
    };
    kb_clone
        .save()
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    Ok(StatusCode::OK)
}

// ----------------- CRUD API Handlers -----------------

async fn crud_insert(
    State(state): State<AppState>,
    Json(req): Json<CrudRequest>,
) -> Result<Json<ExecuteResponse>, AppError> {
    let is_read_only = {
        let config = state.config.read().await;
        if let Some(active_id) = &config.active_db_id {
            config
                .db_connections
                .iter()
                .find(|c| &c.id == active_id)
                .map(|c| c.is_read_only)
                .unwrap_or(false)
        } else {
            false
        }
    };
    if is_read_only {
        return Err(AppError::Forbidden(
            "当前连接为只读模式，禁止执行非查询操作！".to_string(),
        ));
    }

    let db_client = state
        .db_client
        .read()
        .await
        .clone()
        .ok_or_else(|| AppError::BadRequest("Database not connected".to_string()))?;

    let affected_rows = CrudManager::insert(&db_client, &req)
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;

    Ok(Json(ExecuteResponse {
        rows: vec![],
        affected_rows,
        execution_time_ms: 0,
    }))
}

async fn crud_update(
    State(state): State<AppState>,
    Json(req): Json<CrudRequest>,
) -> Result<Json<ExecuteResponse>, AppError> {
    let is_read_only = {
        let config = state.config.read().await;
        if let Some(active_id) = &config.active_db_id {
            config
                .db_connections
                .iter()
                .find(|c| &c.id == active_id)
                .map(|c| c.is_read_only)
                .unwrap_or(false)
        } else {
            false
        }
    };
    if is_read_only {
        return Err(AppError::Forbidden(
            "当前连接为只读模式，禁止执行非查询操作！".to_string(),
        ));
    }

    let db_client = state
        .db_client
        .read()
        .await
        .clone()
        .ok_or_else(|| AppError::BadRequest("Database not connected".to_string()))?;

    let affected_rows = CrudManager::update(&db_client, &req)
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;

    Ok(Json(ExecuteResponse {
        rows: vec![],
        affected_rows,
        execution_time_ms: 0,
    }))
}

#[derive(Deserialize)]
struct DeleteRequest {
    table_name: String,
    condition: serde_json::Map<String, serde_json::Value>,
}

async fn crud_delete(
    State(state): State<AppState>,
    Json(req): Json<DeleteRequest>,
) -> Result<Json<ExecuteResponse>, AppError> {
    let is_read_only = {
        let config = state.config.read().await;
        if let Some(active_id) = &config.active_db_id {
            config
                .db_connections
                .iter()
                .find(|c| &c.id == active_id)
                .map(|c| c.is_read_only)
                .unwrap_or(false)
        } else {
            false
        }
    };
    if is_read_only {
        return Err(AppError::Forbidden(
            "当前连接为只读模式，禁止执行非查询操作！".to_string(),
        ));
    }

    let db_client = state
        .db_client
        .read()
        .await
        .clone()
        .ok_or_else(|| AppError::BadRequest("Database not connected".to_string()))?;

    let affected_rows = CrudManager::delete(&db_client, &req.table_name, &req.condition)
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;

    Ok(Json(ExecuteResponse {
        rows: vec![],
        affected_rows,
        execution_time_ms: 0,
    }))
}

fn config_for_client(raw: &AppConfig) -> serde_json::Value {
    let api_key_set = raw.api_key.as_ref().is_some_and(|s| !s.is_empty());
    let token_pool_set = !raw.token_pool.is_empty();
    let mut profile_flags: HashMap<String, (bool, bool)> = HashMap::new();
    for p in &raw.ai_profiles {
        let p_api_key_set = p.api_key.as_ref().is_some_and(|s| !s.is_empty());
        let p_token_pool_set = !p.pool.tokens.is_empty();
        profile_flags.insert(p.id.clone(), (p_api_key_set, p_token_pool_set));
    }

    let redacted = raw.redacted_for_client();
    let mut v = serde_json::to_value(redacted).unwrap_or_else(|_| serde_json::json!({}));
    if let Some(obj) = v.as_object_mut() {
        obj.insert("api_key_set".to_string(), serde_json::Value::Bool(api_key_set));
        obj.insert(
            "token_pool_set".to_string(),
            serde_json::Value::Bool(token_pool_set),
        );
        if let Some(arr) = obj.get_mut("ai_profiles").and_then(|x| x.as_array_mut()) {
            for item in arr {
                if let Some(pobj) = item.as_object_mut() {
                    let id = pobj
                        .get("id")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    if let Some((k, t)) = profile_flags.get(&id).copied() {
                        pobj.insert("api_key_set".to_string(), serde_json::Value::Bool(k));
                        pobj.insert("token_pool_set".to_string(), serde_json::Value::Bool(t));
                    }
                }
            }
        }
    }
    v
}

async fn get_config(State(state): State<AppState>) -> Json<serde_json::Value> {
    let config = state.config.read().await.clone();
    Json(config_for_client(&config))
}

async fn update_config(
    State(state): State<AppState>,
    Json(new_config): Json<AppConfig>,
) -> Result<Json<serde_json::Value>, AppError> {
    let prev_config = state.config.read().await.clone();
    let mut new_config = new_config.normalize();
    new_config.merge_secrets_from(&prev_config);
    // Save to file
    new_config
        .save()
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;

    // Update in-memory state
    {
        let mut config_write = state.config.write().await;
        *config_write = new_config.clone();
    }

    // Re-init DB if url changed
    if let Some(ref url) = new_config.get_active_db_url() {
        match DbClient::new(url).await {
            Ok(client) => {
                if let Some(old) = state.db_client.write().await.take() {
                    old.pool.close().await;
                }
                let mut db_write = state.db_client.write().await;
                *db_write = Some(client);
            }
            Err(e) => return Err(AppError::BadRequest(format!("DB connection failed: {}", e))),
        }
    }

    // Update Planner's gateway config
    // Actually we need mutability on Planner.
    // For now, recreate Planner and Gateway
    let gateway = AiGateway::new(new_config.clone());
    let mut planner_write = state.planner.write().await;
    *planner_write = Planner::new(gateway);

    Ok(Json(config_for_client(&new_config)))
}

#[cfg(test)]
mod config_redaction_tests {
    use super::*;
    use axum::{routing::get, Router};
    use core_lib::config::{AiConnectionMode, AiProfile, AiProvider};
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn get_config_does_not_leak_secrets() {
        let cfg = AppConfig {
            api_key: Some("secret-key".to_string()),
            token_pool: vec!["secret-token".to_string()],
            db_url: Some("mysql://u:secret-pass@127.0.0.1:3306/db".to_string()),
            ai_profiles: vec![AiProfile {
                id: "p".to_string(),
                name: "p".to_string(),
                provider: AiProvider::Openai,
                mode: AiConnectionMode::Direct,
                api_key: Some("secret-profile-key".to_string()),
                relay_url: None,
                pool: core_lib::config::AiPoolConfig {
                    tokens: vec!["secret-profile-token".to_string()],
                    ..core_lib::config::AiPoolConfig::default()
                },
            }],
            active_ai_profile_id: Some("p".to_string()),
            ..AppConfig::default()
        };

        let state = test_state_with_config(cfg);
        let app = Router::new().route("/backend/config", get(get_config)).with_state(state);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let base = format!("http://{}", addr);
        let body: serde_json::Value = reqwest::get(format!("{}/backend/config", base))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        assert!(body.get("api_key").unwrap().is_null());
        assert_eq!(
            body.get("db_url").and_then(|v| v.as_str()).unwrap(),
            "mysql://u:******@127.0.0.1:3306/db"
        );
        assert_eq!(body.get("token_pool").and_then(|v| v.as_array()).unwrap().len(), 0);
        let profiles = body.get("ai_profiles").and_then(|v| v.as_array()).unwrap();
        assert!(profiles[0].get("api_key").unwrap().is_null());
        assert_eq!(
            profiles[0]
                .get("pool")
                .and_then(|p| p.get("tokens"))
                .and_then(|t| t.as_array())
                .unwrap()
                .len(),
            0
        );
        let as_str = body.to_string();
        assert!(!as_str.contains("secret-key"));
        assert!(!as_str.contains("secret-token"));
        assert!(!as_str.contains("secret-pass"));
        assert!(!as_str.contains("secret-profile-key"));
        assert!(!as_str.contains("secret-profile-token"));
    }
}

async fn fetch_schema_for_db(db_client: &DbClient, db_name: &str) -> Option<SchemaResponse> {
    let tables = SchemaExtractor::get_tables(db_client, db_name).await.ok()?;
    let mut result_tables = Vec::new();
    for t in tables {
        let columns = SchemaExtractor::get_columns(db_client, db_name, &t.table_name)
            .await
            .ok()?;
        let indexes = SchemaExtractor::get_indexes(db_client, db_name, &t.table_name)
            .await
            .unwrap_or_default();
        let foreign_keys = SchemaExtractor::get_foreign_keys(db_client, db_name, &t.table_name)
            .await
            .unwrap_or_default();

        result_tables.push(TableWithDetails {
            table_name: t.table_name,
            columns,
            indexes,
            foreign_keys,
        });
    }

    let views = SchemaExtractor::get_views(db_client, db_name)
        .await
        .unwrap_or_default();

    Some(SchemaResponse {
        db_name: db_name.to_string(),
        tables: result_tables,
        views,
    })
}

async fn get_schema_internal(state: &AppState) -> Option<SchemaResponse> {
    if let Some(vs) = state.virtual_schema.read().await.clone() {
        return Some(vs);
    }

    let db_client = state.db_client.read().await.clone()?;
    let url = state
        .config
        .read()
        .await
        .get_active_db_url()
        .unwrap_or_default();
    let db_name = DbClient::extract_db_name(&url).unwrap_or_default();

    fetch_schema_for_db(&db_client, &db_name).await
}

async fn get_schema(State(state): State<AppState>) -> Result<Json<SchemaResponse>, AppError> {
    if let Some(schema) = get_schema_internal(&state).await {
        Ok(Json(schema))
    } else {
        Err(AppError::BadRequest(
            "Database not connected and no virtual schema loaded".to_string(),
        ))
    }
}

#[derive(Deserialize)]
struct ParseSchemaRequest {
    sql_content: String,
}

async fn parse_schema(
    State(state): State<AppState>,
    Json(req): Json<ParseSchemaRequest>,
) -> Result<Json<SchemaResponse>, AppError> {
    let schema = OfflineParser::parse_sql(&req.sql_content).map_err(AppError::BadRequest)?;

    let mut virtual_schema_write = state.virtual_schema.write().await;
    *virtual_schema_write = Some(schema.clone());

    Ok(Json(schema))
}

#[derive(Deserialize)]
struct ChatRequest {
    query: String,
}

#[derive(Serialize)]
struct ChatResponse {
    sql: String,
    explanation: Option<String>,
}

async fn get_policy(State(state): State<AppState>) -> Result<Json<Policy>, AppError> {
    let policy = state.policy.read().await;
    Ok(Json(policy.clone()))
}

async fn reset_policy(State(state): State<AppState>) -> Result<StatusCode, AppError> {
    PolicyStore::reset_override()
        .await
        .map_err(|e| AppError::InternalError(format!("{:?}", e)))?;
    let effective = PolicyStore::load_effective()
        .await
        .unwrap_or_else(|_| Policy::default());
    let mut policy = state.policy.write().await;
    *policy = effective;
    Ok(StatusCode::OK)
}

#[derive(Serialize)]
struct SnapshotPolicyResponse {
    name: String,
}

async fn snapshot_policy(
    State(state): State<AppState>,
) -> Result<Json<SnapshotPolicyResponse>, AppError> {
    let name = PolicyStore::create_snapshot()
        .await
        .map_err(|e| AppError::InternalError(format!("{:?}", e)))?;
    let effective = PolicyStore::load_effective()
        .await
        .unwrap_or_else(|_| Policy::default());
    let mut policy = state.policy.write().await;
    *policy = effective;
    Ok(Json(SnapshotPolicyResponse { name }))
}

#[derive(Deserialize)]
struct RollbackPolicyRequest {
    name: String,
}

async fn rollback_policy(
    State(state): State<AppState>,
    Json(req): Json<RollbackPolicyRequest>,
) -> Result<StatusCode, AppError> {
    PolicyStore::rollback_snapshot(&req.name)
        .await
        .map_err(|e| AppError::BadRequest(format!("{:?}", e)))?;
    let effective = PolicyStore::load_effective()
        .await
        .unwrap_or_else(|_| Policy::default());
    let mut policy = state.policy.write().await;
    *policy = effective;
    Ok(StatusCode::OK)
}

async fn chat_to_sql(
    State(state): State<AppState>,
    Json(req): Json<ChatRequest>,
) -> Result<Json<ChatResponse>, AppError> {
    let planner = state.planner.read().await.clone();
    let db_client = state.db_client.read().await.clone();
    let virtual_schema = state.virtual_schema.read().await.clone();
    let policy = state.policy.read().await.clone();
    let rule_store = state.rule_store.read().await.clone();

    let db_type = state.config.read().await.get_active_db_type();

    let intent_res = if let Some(db_client) = db_client {
        let url = state
            .config
            .read()
            .await
            .get_active_db_url()
            .unwrap_or_default();
        let db_name = DbClient::extract_db_name(&url).unwrap_or_default();
        planner
            .generate_sql(
                &db_client,
                &db_name,
                &req.query,
                &rule_store,
                &policy,
                &db_type,
            )
            .await
    } else if let Some(vs) = virtual_schema {
        planner
            .generate_sql_with_virtual_schema(&req.query, &vs, &rule_store, &policy, &db_type)
            .await
    } else {
        planner
            .generate_sql_no_schema(&req.query, &rule_store, &policy, &db_type)
            .await
    };

    let intent = match intent_res {
        Ok(res) => res,
        Err(AiError::Auth(msg)) => {
            let body = serde_json::json!({
                "error": "ai_auth_failed",
                "message": "AI 鉴权失败，请在引导页里更新 AI Token / Relay 配置后重试。",
                "detail": msg,
            })
            .to_string();
            return Err(AppError::AiAuth(body));
        }
        Err(AiError::Forbidden(msg)) => {
            let body = serde_json::json!({
                "error": "ai_forbidden",
                "message": "AI 返回 403 Forbidden：当前 Key/账号无权限或被服务端拒绝。",
                "detail": msg,
            })
            .to_string();
            return Err(AppError::AiForbidden(body));
        }
        Err(AiError::ModelNotFound(msg)) => {
            let body = serde_json::json!({
                "error": "ai_model_not_found",
                "message": "AI 返回 404 Not Found：模型不存在或当前中转/Provider 不支持该模型。",
                "detail": msg,
            })
            .to_string();
            return Err(AppError::AiModelNotFound(body));
        }
        Err(e) => return Err(map_ai_error(e)),
    };

    // Background task to update hit count
    if let Some(rule_id) = intent.matched_rule_id {
        let store_clone = state.rule_store.clone();
        tokio::spawn(async move {
            let store_clone2 = {
                let mut store = store_clone.write().await;
                if store.increment_hit_count(&rule_id) {
                    Some(store.clone())
                } else {
                    None
                }
            };
            if let Some(store) = store_clone2 {
                if let Err(e) = store.save().await {
                    tracing::error!("Failed to save rule hit count: {}", e);
                }
            }
        });
    }

    Ok(Json(ChatResponse {
        sql: intent.sql,
        explanation: intent.explanation,
    }))
}

#[derive(Deserialize)]
struct ExecuteRequest {
    sql: String,
    force: Option<bool>,
}

#[derive(Serialize)]
struct ExecuteResponse {
    rows: Vec<serde_json::Value>,
    affected_rows: u64,
    execution_time_ms: u64,
}

fn quote_mysql_ident(raw: &str) -> Result<String, AppError> {
    let s = raw.trim();
    if s.is_empty() {
        return Err(AppError::BadRequest("Invalid identifier".to_string()));
    }
    if s.len() > 512 {
        return Err(AppError::BadRequest("Identifier too long".to_string()));
    }
    Ok(format!("`{}`", s.replace('`', "``")))
}

async fn execute_sql(
    State(state): State<AppState>,
    Json(mut req): Json<ExecuteRequest>,
) -> Result<Json<ExecuteResponse>, AppError> {
    let db_client = state
        .db_client
        .read()
        .await
        .clone()
        .ok_or_else(|| AppError::BadRequest("Database not connected".to_string()))?;

    let is_read_only = {
        let config = state.config.read().await;
        if let Some(active_id) = &config.active_db_id {
            config
                .db_connections
                .iter()
                .find(|c| &c.id == active_id)
                .map(|c| c.is_read_only)
                .unwrap_or(false)
        } else {
            false
        }
    };

    use sqlx::Column;
    use sqlx::Row;
    use std::time::Instant;

    let mut clean_sql = req.sql.trim().to_string();
    loop {
        if clean_sql.starts_with("--") {
            if let Some(idx) = clean_sql.find('\n') {
                clean_sql = clean_sql[idx + 1..].trim().to_string();
            } else {
                clean_sql = String::new();
            }
        } else if clean_sql.starts_with("/*") {
            if let Some(idx) = clean_sql.find("*/") {
                clean_sql = clean_sql[idx + 2..].trim().to_string();
            } else {
                clean_sql = String::new();
            }
        } else {
            break;
        }
    }

    let upper_sql = clean_sql.to_uppercase();

    let is_select = upper_sql.starts_with("SELECT")
        || upper_sql.starts_with("SHOW")
        || upper_sql.starts_with("DESCRIBE")
        || upper_sql.starts_with("EXPLAIN");

    if is_read_only && !is_select {
        return Err(AppError::Forbidden(
            "当前连接为只读模式，禁止执行非查询操作！".to_string(),
        ));
    }

    // Safety check for dangerous operations
    let is_dangerous = upper_sql.contains("UPDATE ")
        || upper_sql.contains("DELETE ")
        || upper_sql.contains("DROP ")
        || upper_sql.contains("TRUNCATE ")
        || upper_sql.contains("ALTER ");

    if is_dangerous && req.force != Some(true) {
        let body = serde_json::json!({
            "error": "DANGEROUS_SQL",
            "message": "检测到高危操作，请确认后强制执行"
        })
        .to_string();
        return Err(AppError::BadRequest(body));
    }

    let mut rows = Vec::new();
    let mut affected_rows = 0;

    let is_select = upper_sql.starts_with("SELECT")
        || upper_sql.starts_with("SHOW")
        || upper_sql.starts_with("DESCRIBE");

    if is_select && !upper_sql.contains("LIMIT") {
        req.sql = req.sql.trim().trim_end_matches(';').to_string();
        req.sql.push_str(" LIMIT 50000");
    }

    let start_time = Instant::now();
    let execution_result = if is_select {
        match tokio::time::timeout(
            state.timeouts.db_query,
            sqlx::query(&req.sql).fetch_all(&db_client.pool),
        )
        .await
        {
            Ok(res) => res,
            Err(_) => {
                return Err(AppError::Timeout(
                    "查询执行超时（已超过 30 秒），已被系统安全阻断，请优化 SQL 或添加索引。"
                        .to_string(),
                ))
            }
        }
    } else {
        // Just for type matching we do a dummy empty result, the real logic is below
        Ok(vec![])
    };

    let mut status = "success".to_string();
    let mut err_msg = None;

    if is_select {
        match execution_result {
            Ok(result_rows) => {
                for row in result_rows {
                    let mut map = serde_json::Map::new();
                    for col in row.columns() {
                        let col_name = col.name().to_string();

                        // Dynamic Row Mapping for high fidelity JSON representation
                        if let Ok(val) = row.try_get::<Option<i64>, _>(col.ordinal()) {
                            map.insert(col_name, serde_json::json!(val));
                        } else if let Ok(val) = row.try_get::<Option<f64>, _>(col.ordinal()) {
                            map.insert(col_name, serde_json::json!(val));
                        } else if let Ok(val) = row.try_get::<Option<bool>, _>(col.ordinal()) {
                            map.insert(col_name, serde_json::json!(val));
                        } else if let Ok(val) =
                            row.try_get::<Option<chrono::NaiveDateTime>, _>(col.ordinal())
                        {
                            map.insert(col_name, serde_json::json!(val.map(|dt| dt.to_string())));
                        } else if let Ok(val) =
                            row.try_get::<Option<chrono::NaiveDate>, _>(col.ordinal())
                        {
                            map.insert(col_name, serde_json::json!(val.map(|d| d.to_string())));
                        } else if let Ok(val) =
                            row.try_get::<Option<chrono::NaiveTime>, _>(col.ordinal())
                        {
                            map.insert(col_name, serde_json::json!(val.map(|t| t.to_string())));
                        } else if let Ok(val) = row.try_get::<Option<String>, _>(col.ordinal()) {
                            map.insert(col_name, serde_json::json!(val));
                        } else {
                            // Fallback for bytes or unsupported types
                            let val: Option<Vec<u8>> = row.try_get(col.ordinal()).unwrap_or(None);
                            if let Some(bytes) = val {
                                let s = String::from_utf8_lossy(&bytes).into_owned();
                                map.insert(col_name, serde_json::json!(s));
                            } else {
                                map.insert(col_name, serde_json::Value::Null);
                            }
                        }
                    }
                    rows.push(serde_json::Value::Object(map));
                }
            }
            Err(e) => {
                status = "error".to_string();
                err_msg = Some(e.to_string());
            }
        }
    } else {
        match tokio::time::timeout(
            state.timeouts.db_query,
            sqlx::query(&req.sql).execute(&db_client.pool),
        )
        .await
        {
            Ok(Ok(result)) => {
                affected_rows = result.rows_affected();
            }
            Ok(Err(e)) => {
                status = "error".to_string();
                err_msg = Some(e.to_string());
            }
            Err(_) => {
                return Err(AppError::Timeout(
                    "查询执行超时（已超过 30 秒），已被系统安全阻断，请优化 SQL 或添加索引。"
                        .to_string(),
                ))
            }
        }
    }

    let elapsed = start_time.elapsed().as_millis() as u64;

    // Record history
    {
        let store_clone = {
            let mut store = state.sql_history.write().await;
            store.add_history(SqlHistory {
                id: "".to_string(), // will be generated
                sql: req.sql.clone(),
                status,
                execution_time_ms: elapsed,
                executed_at: 0, // will be generated
            });
            store.clone()
        };
        let _ = store_clone.save().await; // ignore save errors for history
    }

    if let Some(e) = err_msg {
        return Err(AppError::InternalError(e));
    }

    Ok(Json(ExecuteResponse {
        rows,
        affected_rows,
        execution_time_ms: elapsed,
    }))
}

#[derive(Deserialize)]
struct GetTableDataRequest {
    table_name: String,
    page: Option<u32>,
    page_size: Option<u32>,
    filters: Option<String>,
    orders: Option<String>,
}

#[derive(Serialize)]
struct GetTableDataResponse {
    data: Vec<serde_json::Value>,
    total: i64,
}

#[derive(Deserialize, Debug)]
struct FilterCondition {
    column: String,
    operator: String,
    value: String,
}

#[derive(Deserialize, Debug)]
struct OrderCondition {
    column: String,
    desc: bool,
}

async fn get_table_data(
    State(state): State<AppState>,
    axum::extract::Query(req): axum::extract::Query<GetTableDataRequest>,
) -> Result<Json<GetTableDataResponse>, AppError> {
    let db_client = state
        .db_client
        .read()
        .await
        .clone()
        .ok_or_else(|| AppError::BadRequest("Database not connected".to_string()))?;

    let page = req.page.unwrap_or(1);
    let page_size = req.page_size.unwrap_or(50);
    let offset = (page - 1) * page_size;

    let mut where_clause = String::new();
    let mut bindings = Vec::new();

    if let Some(filters_str) = &req.filters {
        if let Ok(filters) = serde_json::from_str::<Vec<FilterCondition>>(filters_str) {
            let mut conditions = Vec::new();
            for f in filters {
                let op = match f.operator.as_str() {
                    "equals" => "=",
                    "contains" => "LIKE",
                    "greater_than" => ">",
                    "less_than" => "<",
                    _ => "=",
                };
                let col = quote_mysql_ident(&f.column)?;
                conditions.push(format!("{} {} ?", col, op));
                if f.operator == "contains" {
                    bindings.push(format!("%{}%", f.value));
                } else {
                    bindings.push(f.value.clone());
                }
            }
            if !conditions.is_empty() {
                where_clause = format!("WHERE {}", conditions.join(" AND "));
            }
        }
    }

    let mut order_clause = String::new();
    if let Some(orders_str) = &req.orders {
        if let Ok(orders) = serde_json::from_str::<Vec<OrderCondition>>(orders_str) {
            let mut o_clauses = Vec::new();
            for o in orders {
                let dir = if o.desc { "DESC" } else { "ASC" };
                let col = quote_mysql_ident(&o.column)?;
                o_clauses.push(format!("{} {}", col, dir));
            }
            if !o_clauses.is_empty() {
                order_clause = format!("ORDER BY {}", o_clauses.join(", "));
            }
        }
    }

    let table_ident = quote_mysql_ident(&req.table_name)?;
    let count_sql = format!("SELECT COUNT(*) FROM {} {}", table_ident, where_clause);
    let mut count_query = sqlx::query_scalar::<_, i64>(&count_sql);
    for b in &bindings {
        count_query = count_query.bind(b);
    }
    let total: i64 = match tokio::time::timeout(
        state.timeouts.db_query,
        count_query.fetch_one(&db_client.pool),
    )
    .await
    {
        Ok(Ok(val)) => val,
        Ok(Err(e)) => return Err(AppError::InternalError(e.to_string())),
        Err(_) => {
            return Err(AppError::Timeout(
                "查询执行超时（已超过 30 秒），已被系统安全阻断，请优化 SQL 或添加索引。"
                    .to_string(),
            ))
        }
    };

    let data_sql = format!(
        "SELECT * FROM {} {} {} LIMIT {} OFFSET {}",
        table_ident, where_clause, order_clause, page_size, offset
    );
    let mut data_query = sqlx::query(&data_sql);
    for b in &bindings {
        data_query = data_query.bind(b);
    }

    use sqlx::Column;
    use sqlx::Row;

    let mut rows = Vec::new();
    let result_rows = match tokio::time::timeout(
        state.timeouts.db_query,
        data_query.fetch_all(&db_client.pool),
    )
    .await
    {
        Ok(Ok(res)) => res,
        Ok(Err(e)) => return Err(AppError::InternalError(e.to_string())),
        Err(_) => {
            return Err(AppError::Timeout(
                "查询执行超时（已超过 30 秒），已被系统安全阻断，请优化 SQL 或添加索引。"
                    .to_string(),
            ))
        }
    };

    for row in result_rows {
        let mut map = serde_json::Map::new();
        for col in row.columns() {
            let col_name = col.name().to_string();
            if let Ok(val) = row.try_get::<Option<i64>, _>(col.ordinal()) {
                map.insert(col_name, serde_json::json!(val));
            } else if let Ok(val) = row.try_get::<Option<f64>, _>(col.ordinal()) {
                map.insert(col_name, serde_json::json!(val));
            } else if let Ok(val) = row.try_get::<Option<bool>, _>(col.ordinal()) {
                map.insert(col_name, serde_json::json!(val));
            } else if let Ok(val) = row.try_get::<Option<chrono::NaiveDateTime>, _>(col.ordinal()) {
                map.insert(col_name, serde_json::json!(val.map(|dt| dt.to_string())));
            } else if let Ok(val) = row.try_get::<Option<chrono::NaiveDate>, _>(col.ordinal()) {
                map.insert(col_name, serde_json::json!(val.map(|d| d.to_string())));
            } else if let Ok(val) = row.try_get::<Option<chrono::NaiveTime>, _>(col.ordinal()) {
                map.insert(col_name, serde_json::json!(val.map(|t| t.to_string())));
            } else if let Ok(val) = row.try_get::<Option<String>, _>(col.ordinal()) {
                map.insert(col_name, serde_json::json!(val));
            } else {
                let val: Option<Vec<u8>> = row.try_get(col.ordinal()).unwrap_or(None);
                if let Some(bytes) = val {
                    let s = String::from_utf8_lossy(&bytes).into_owned();
                    map.insert(col_name, serde_json::json!(s));
                } else {
                    map.insert(col_name, serde_json::Value::Null);
                }
            }
        }
        rows.push(serde_json::Value::Object(map));
    }

    Ok(Json(GetTableDataResponse { data: rows, total }))
}

#[derive(Deserialize)]
struct GetTableSchemaRequest {
    table_name: String,
}

async fn get_table_schema(
    State(state): State<AppState>,
    axum::extract::Query(req): axum::extract::Query<GetTableSchemaRequest>,
) -> Result<Json<TableWithDetails>, AppError> {
    let db_client = state
        .db_client
        .read()
        .await
        .clone()
        .ok_or_else(|| AppError::BadRequest("Database not connected".to_string()))?;
    let url = state
        .config
        .read()
        .await
        .get_active_db_url()
        .unwrap_or_default();
    let db_name = DbClient::extract_db_name(&url).unwrap_or_default();

    let columns = SchemaExtractor::get_columns(&db_client, &db_name, &req.table_name)
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;

    let indexes = SchemaExtractor::get_indexes(&db_client, &db_name, &req.table_name)
        .await
        .unwrap_or_default();

    let foreign_keys = SchemaExtractor::get_foreign_keys(&db_client, &db_name, &req.table_name)
        .await
        .unwrap_or_default();

    Ok(Json(TableWithDetails {
        table_name: req.table_name,
        columns,
        indexes,
        foreign_keys,
    }))
}

#[derive(Deserialize)]
struct ExecuteDdlRequest {
    sql: String,
}

#[derive(Deserialize)]
struct PreviewDdlRequest {
    old_table: Option<TableWithDetails>,
    new_table: TableWithDetails,
}

#[derive(Serialize)]
struct PreviewDdlResponse {
    sql: String,
}

async fn preview_ddl(
    Json(req): Json<PreviewDdlRequest>,
) -> Result<Json<PreviewDdlResponse>, AppError> {
    let sql = DdlEngine::generate_preview(req.old_table.as_ref(), &req.new_table);
    Ok(Json(PreviewDdlResponse { sql }))
}

async fn execute_ddl(
    State(state): State<AppState>,
    Json(req): Json<ExecuteDdlRequest>,
) -> Result<Json<ExecuteResponse>, AppError> {
    let is_read_only = {
        let config = state.config.read().await;
        if let Some(active_id) = &config.active_db_id {
            config
                .db_connections
                .iter()
                .find(|c| &c.id == active_id)
                .map(|c| c.is_read_only)
                .unwrap_or(false)
        } else {
            false
        }
    };
    if is_read_only {
        return Err(AppError::Forbidden(
            "当前连接为只读模式，禁止执行非查询操作！".to_string(),
        ));
    }

    let db_client = state
        .db_client
        .read()
        .await
        .clone()
        .ok_or_else(|| AppError::BadRequest("Database not connected".to_string()))?;

    let result = sqlx::query(&req.sql)
        .execute(&db_client.pool)
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;

    Ok(Json(ExecuteResponse {
        rows: vec![],
        affected_rows: result.rows_affected(),
        execution_time_ms: 0,
    }))
}

// ----------------- Tools API Handlers -----------------

#[derive(Deserialize)]
struct MockDataRequest {
    table_name: String,
    row_count: u32,
    rules: Option<std::collections::HashMap<String, String>>,
}

#[derive(Serialize)]
struct MockDataResponse {
    sql: String,
}

async fn generate_mock_data(
    State(state): State<AppState>,
    Json(req): Json<MockDataRequest>,
) -> Result<Json<MockDataResponse>, AppError> {
    let db_client = state
        .db_client
        .read()
        .await
        .clone()
        .ok_or_else(|| AppError::BadRequest("Database not connected".to_string()))?;
    let url = state
        .config
        .read()
        .await
        .get_active_db_url()
        .unwrap_or_default();
    let db_name = DbClient::extract_db_name(&url).unwrap_or_default();

    let columns = SchemaExtractor::get_columns(&db_client, &db_name, &req.table_name)
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;

    let foreign_keys = SchemaExtractor::get_foreign_keys(&db_client, &db_name, &req.table_name)
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;

    let table = TableWithDetails {
        table_name: req.table_name.clone(),
        columns,
        indexes: vec![],
        foreign_keys,
    };

    let planner = state.planner.read().await.clone();
    let gateway = planner.gateway;

    let sql = MockDataGenerator::generate(&gateway, &db_client, &table, req.row_count, req.rules)
        .await
        .map_err(AppError::InternalError)?;

    Ok(Json(MockDataResponse { sql }))
}

use axum::body::Body;
use axum::response::Response;
use futures::StreamExt;

#[derive(Deserialize)]
struct ExportRequest {
    table_name: String,
    export_type: String, // "csv", "sql", "json"
}

async fn export_data(
    State(state): State<AppState>,
    Json(req): Json<ExportRequest>,
) -> Result<Response, AppError> {
    let db_client = state
        .db_client
        .read()
        .await
        .clone()
        .ok_or_else(|| AppError::BadRequest("Database not connected".to_string()))?;

    let table_name = req.table_name.clone();
    let export_type = req.export_type.clone();
    let data_sql = format!("SELECT * FROM {}", table_name);

    let (tx, rx) =
        tokio::sync::mpsc::channel::<Result<axum::body::Bytes, std::convert::Infallible>>(100);

    let spawn_table_name = table_name.clone();
    let spawn_export_type = export_type.clone();
    tokio::spawn(async move {
        use sqlx::Column;
        use sqlx::Row;

        let mut stream = sqlx::query(&data_sql).fetch(&db_client.pool);
        let mut headers_sent = false;
        let mut headers = Vec::new();
        let mut is_first_json = true;
        let mut previous_row: Option<serde_json::Map<String, serde_json::Value>> = None;

        while let Some(row_result) = stream.next().await {
            if let Ok(row) = row_result {
                if !headers_sent {
                    for col in row.columns() {
                        headers.push(col.name().to_string());
                    }
                    if spawn_export_type == "csv" {
                        let _ = tx
                            .send(Ok(axum::body::Bytes::from(DataExporter::csv_header(
                                &headers,
                            ))))
                            .await;
                    } else if spawn_export_type == "sql" {
                        let _ = tx
                            .send(Ok(axum::body::Bytes::from(DataExporter::sql_header(
                                &spawn_table_name,
                                &headers,
                            ))))
                            .await;
                    }
                    headers_sent = true;
                }

                let mut map = serde_json::Map::new();
                for col in row.columns() {
                    let col_name = col.name().to_string();
                    if let Ok(val) = row.try_get::<Option<i64>, _>(col.ordinal()) {
                        map.insert(col_name, serde_json::json!(val));
                    } else if let Ok(val) = row.try_get::<Option<f64>, _>(col.ordinal()) {
                        map.insert(col_name, serde_json::json!(val));
                    } else if let Ok(val) = row.try_get::<Option<bool>, _>(col.ordinal()) {
                        map.insert(col_name, serde_json::json!(val));
                    } else if let Ok(val) =
                        row.try_get::<Option<chrono::NaiveDateTime>, _>(col.ordinal())
                    {
                        map.insert(col_name, serde_json::json!(val.map(|dt| dt.to_string())));
                    } else if let Ok(val) =
                        row.try_get::<Option<chrono::NaiveDate>, _>(col.ordinal())
                    {
                        map.insert(col_name, serde_json::json!(val.map(|d| d.to_string())));
                    } else if let Ok(val) =
                        row.try_get::<Option<chrono::NaiveTime>, _>(col.ordinal())
                    {
                        map.insert(col_name, serde_json::json!(val.map(|t| t.to_string())));
                    } else if let Ok(val) = row.try_get::<Option<String>, _>(col.ordinal()) {
                        map.insert(col_name, serde_json::json!(val));
                    } else {
                        let val: Option<Vec<u8>> = row.try_get(col.ordinal()).unwrap_or(None);
                        if let Some(bytes) = val {
                            let s = String::from_utf8_lossy(&bytes).into_owned();
                            map.insert(col_name, serde_json::json!(s));
                        } else {
                            map.insert(col_name, serde_json::Value::Null);
                        }
                    }
                }

                if spawn_export_type == "csv" {
                    let _ = tx
                        .send(Ok(axum::body::Bytes::from(DataExporter::csv_row(
                            &headers, &map,
                        ))))
                        .await;
                } else if spawn_export_type == "sql" {
                    if let Some(prev) = previous_row.take() {
                        let _ = tx
                            .send(Ok(axum::body::Bytes::from(DataExporter::sql_row(
                                &headers, &prev, false,
                            ))))
                            .await;
                    }
                    previous_row = Some(map);
                } else if spawn_export_type == "json" {
                    if let Some(prev) = previous_row.take() {
                        let _ = tx
                            .send(Ok(axum::body::Bytes::from(DataExporter::json_row(
                                &prev,
                                is_first_json,
                                false,
                            ))))
                            .await;
                        is_first_json = false;
                    }
                    previous_row = Some(map);
                }
            }
        }

        // Flush last row
        if spawn_export_type == "sql" {
            if let Some(prev) = previous_row {
                let _ = tx
                    .send(Ok(axum::body::Bytes::from(DataExporter::sql_row(
                        &headers, &prev, true,
                    ))))
                    .await;
            } else if !headers_sent {
                // No rows, empty file
            }
        } else if spawn_export_type == "json" {
            if let Some(prev) = previous_row {
                let _ = tx
                    .send(Ok(axum::body::Bytes::from(DataExporter::json_row(
                        &prev,
                        is_first_json,
                        true,
                    ))))
                    .await;
            } else {
                let _ = tx.send(Ok(axum::body::Bytes::from("[]\n"))).await;
            }
        }
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    let body = Body::from_stream(stream);

    let content_type = match export_type.as_str() {
        "csv" => "text/csv",
        "json" => "application/json",
        "sql" => "application/sql",
        _ => "text/plain",
    };

    let filename = format!("{}.{}", table_name, export_type);

    Response::builder()
        .header("Content-Type", content_type)
        .header(
            "Content-Disposition",
            format!("attachment; filename=\"{}\"", filename),
        )
        .body(body)
        .map_err(|e| AppError::InternalError(e.to_string()))
}

#[derive(Deserialize)]
struct ExportJobStartRequest {
    table_name: String,
    export_type: String,
    where_clause: Option<String>,
    primary_key: Option<String>,
    pk_start: Option<String>,
    pk_end: Option<String>,
    window_limit: Option<u64>,
    window_offset: Option<u64>,
}

#[derive(Deserialize)]
struct ImportJobStartRequest {
    table_name: String,
    data: Vec<std::collections::HashMap<String, serde_json::Value>>,
    mapping: std::collections::HashMap<String, String>,
    skip_errors: bool,
}

#[derive(Deserialize)]
struct ImportSqlJobStartRequest {
    sql: String,
    force: Option<bool>,
}

#[derive(Serialize)]
struct ToolJobStartResponse {
    job_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct GoLiveThresholds {
    #[serde(default)]
    max_total_ms: Option<u64>,
    #[serde(default)]
    per_step_max_ms: HashMap<String, u64>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct GoLiveJobStartRequest {
    #[serde(default)]
    steps: Vec<String>,
    thresholds: Option<GoLiveThresholds>,
    #[serde(default)]
    connection_ids: Vec<String>,
    operator: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum GoLiveStepStatus {
    Pass,
    Fail,
    Skip,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GoLiveStepReport {
    name: String,
    connection_id: Option<String>,
    status: GoLiveStepStatus,
    duration_ms: u128,
    errors: Vec<String>,
    code: Option<String>,
    details: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GoLiveReport {
    job_id: String,
    operator: Option<String>,
    connection_ids: Vec<String>,
    requested_steps: Vec<String>,
    thresholds: Option<GoLiveThresholds>,
    created_at: String,
    finished_at: String,
    elapsed_ms: u128,
    passed: bool,
    steps: Vec<GoLiveStepReport>,
}

async fn tool_job_status(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Result<Json<ToolJob>, AppError> {
    let job = { state.tool_jobs.read().await.get(&job_id).cloned() };
    job.map(Json)
        .ok_or_else(|| AppError::NotFound("job not found".to_string()))
}

async fn tool_job_cancel(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Result<Json<ToolJob>, AppError> {
    let handle = { state.tool_job_handles.read().await.get(&job_id).cloned() };
    if let Some(h) = handle {
        h.abort();
    }

    update_tool_job(&state, &job_id, |j| {
        if matches!(j.status, ToolJobStatus::Pending | ToolJobStatus::Running) {
            j.status = ToolJobStatus::Canceled;
        }
    })
    .await;

    let job = { state.tool_jobs.read().await.get(&job_id).cloned() };
    job.map(Json)
        .ok_or_else(|| AppError::NotFound("job not found".to_string()))
}

async fn tool_job_artifact_download(
    State(state): State<AppState>,
    Path((job_id, artifact)): Path<(String, String)>,
) -> Result<Response, AppError> {
    let job = { state.tool_jobs.read().await.get(&job_id).cloned() }
        .ok_or_else(|| AppError::NotFound("job not found".to_string()))?;

    let artifacts = job
        .artifacts
        .clone()
        .ok_or_else(|| AppError::NotFound("artifact not found".to_string()))?;

    let path = match artifact.as_str() {
        "data" => artifacts.data_path,
        "manifest" => artifacts.manifest_path,
        _ => None,
    }
    .ok_or_else(|| AppError::NotFound("artifact not found".to_string()))?;

    let filename = std::path::Path::new(&path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("artifact")
        .to_string();

    let file = tokio::fs::File::open(&path)
        .await
        .map_err(|e| AppError::NotFound(e.to_string()))?;
    let stream = tokio_util::io::ReaderStream::new(file);
    let body = Body::from_stream(stream);

    let content_type = if artifact == "manifest" {
        "application/json".to_string()
    } else {
        artifacts
            .content_type
            .clone()
            .unwrap_or_else(|| "application/octet-stream".to_string())
    };

    Response::builder()
        .header("Content-Type", content_type)
        .header(
            "Content-Disposition",
            format!("attachment; filename=\"{}\"", filename),
        )
        .body(body)
        .map_err(|e| AppError::InternalError(e.to_string()))
}

#[derive(Deserialize)]
struct LimitQuery {
    limit: Option<usize>,
}

async fn go_live_reports_list(
    State(state): State<AppState>,
    Query(q): Query<LimitQuery>,
) -> Result<Json<Vec<serde_json::Value>>, AppError> {
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let temp_dir = state.limits.temp_dir.trim_end_matches('/').to_string();
    let path = format!("{}/go-live-index.jsonl", temp_dir);
    Ok(Json(read_jsonl_recent(&path, limit).await?))
}

async fn go_live_audit_list(
    State(state): State<AppState>,
    Query(q): Query<LimitQuery>,
) -> Result<Json<Vec<serde_json::Value>>, AppError> {
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let temp_dir = state.limits.temp_dir.trim_end_matches('/').to_string();
    let path = format!("{}/go-live-audit.jsonl", temp_dir);
    Ok(Json(read_jsonl_recent(&path, limit).await?))
}

async fn read_jsonl_recent(path: &str, limit: usize) -> Result<Vec<serde_json::Value>, AppError> {
    let content = match tokio::fs::read_to_string(path).await {
        Ok(v) => v,
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                return Ok(Vec::new());
            }
            return Err(AppError::InternalError(e.to_string()));
        }
    };
    let mut rows: Vec<serde_json::Value> = Vec::new();
    for line in content.lines() {
        let s = line.trim();
        if s.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(s) {
            rows.push(v);
        }
    }
    rows.reverse();
    rows.truncate(limit);
    Ok(rows)
}

async fn export_job_start(
    State(state): State<AppState>,
    Json(req): Json<ExportJobStartRequest>,
) -> Result<Json<ToolJobStartResponse>, AppError> {
    let permit = state
        .job_semaphore
        .clone()
        .try_acquire_owned()
        .map_err(|_| {
            AppError::TooManyRequests(format!(
                "job concurrency limit exceeded: max={}",
                state.limits.max_job_concurrency
            ))
        })?;
    let limits = state.limits.clone();
    ensure_temp_quota(&limits, limits.max_file_bytes).await?;
    let db_client = state
        .db_client
        .read()
        .await
        .clone()
        .ok_or_else(|| AppError::BadRequest("Database not connected".to_string()))?;

    let job_id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().timestamp();

    let job = ToolJob {
        job_id: job_id.clone(),
        kind: ToolJobKind::Export,
        status: ToolJobStatus::Pending,
        progress: ToolJobProgress::default(),
        created_at: now,
        updated_at: now,
        elapsed_ms: None,
        artifacts: None,
        result: None,
        error: None,
    };

    {
        let mut jobs = state.tool_jobs.write().await;
        jobs.insert(job_id.clone(), job);
    }

    let state_clone = state.clone();
    let job_id_clone = job_id.clone();
    let req_clone = req;
    let handle = tokio::spawn(async move {
        let _permit = permit;
        let limits = state_clone.limits.clone();
        update_tool_job(&state_clone, &job_id_clone, |j| {
            j.status = ToolJobStatus::Running;
            j.progress.message = Some("export running".to_string());
        })
        .await;

        let t_job = std::time::Instant::now();
        let table_name = req_clone.table_name.clone();
        let export_type = req_clone.export_type.to_lowercase();
        let ext = export_type.clone();
        let base_name = format!("export_{}_{}", table_name, job_id_clone);
        let temp_dir = limits.temp_dir.trim_end_matches('/').to_string();
        let data_path = format!("{}/{}.{}", temp_dir, base_name, ext);
        let manifest_path = format!("{}/{}.manifest.json", temp_dir, base_name);

        let content_type = match export_type.as_str() {
            "csv" => "text/csv",
            "json" => "application/json",
            "sql" => "application/sql",
            "xml" => "application/xml",
            "txt" => "text/plain",
            "xls" => "application/vnd.ms-excel",
            "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            _ => "application/octet-stream",
        }
        .to_string();

        let encoding = match export_type.as_str() {
            "xls" | "xlsx" => "utf-8-bom",
            _ => "utf-8",
        }
        .to_string();

        let res: Result<serde_json::Value, AppError> = async {
            let stats =
                run_export_job(
                    &db_client,
                    &state_clone,
                    &job_id_clone,
                    &req_clone,
                    &data_path,
                    limits.max_file_bytes,
                )
                    .await?;
            let elapsed_ms = t_job.elapsed().as_millis();

            let generated_at = chrono::Utc::now().to_rfc3339();
            let manifest = serde_json::json!({
                "schema_version": "1",
                "generated_at": generated_at,
                "sha256": stats.sha256,
                "line_count": stats.line_count,
                "bytes": stats.bytes,
                "row_count": stats.row_count,
                "elapsed_ms": elapsed_ms,
                "table": table_name,
                "format": export_type,
                "mime": content_type.clone(),
                "encoding": encoding.clone(),
            });
            let s = serde_json::to_string_pretty(&manifest)
                .map_err(|e| AppError::InternalError(e.to_string()))?;
            ensure_temp_quota(&limits, s.len() as u64).await?;
            tokio::fs::write(&manifest_path, s)
                .await
                .map_err(|e| AppError::InternalError(e.to_string()))?;

            Ok(manifest)
        }
        .await;

        match res {
            Ok(manifest) => {
                let elapsed_ms = t_job.elapsed().as_millis();
                update_tool_job(&state_clone, &job_id_clone, |j| {
                    j.status = ToolJobStatus::Completed;
                    j.progress.message = Some("export completed".to_string());
                    j.elapsed_ms = Some(elapsed_ms);
                    j.artifacts = Some(ToolJobArtifacts {
                        data_path: Some(data_path),
                        manifest_path: Some(manifest_path),
                        file_name: Some(base_name),
                        content_type: Some(content_type),
                    });
                    j.result = Some(manifest);
                })
                .await;
            }
            Err(e) => {
                let elapsed_ms = t_job.elapsed().as_millis();
                update_tool_job(&state_clone, &job_id_clone, |j| {
                    j.status = ToolJobStatus::Error;
                    j.elapsed_ms = Some(elapsed_ms);
                    j.error = Some(e.to_string());
                    j.progress.message = Some("export failed".to_string());
                })
                .await;
            }
        }
    });

    {
        let mut handles = state.tool_job_handles.write().await;
        handles.insert(job_id.clone(), handle.abort_handle());
    }

    Ok(Json(ToolJobStartResponse { job_id }))
}

async fn import_job_start(
    State(state): State<AppState>,
    Json(req): Json<ImportJobStartRequest>,
) -> Result<Json<ToolJobStartResponse>, AppError> {
    let permit = state
        .job_semaphore
        .clone()
        .try_acquire_owned()
        .map_err(|_| {
            AppError::TooManyRequests(format!(
                "job concurrency limit exceeded: max={}",
                state.limits.max_job_concurrency
            ))
        })?;
    let db_client = state
        .db_client
        .read()
        .await
        .clone()
        .ok_or_else(|| AppError::BadRequest("Database not connected".to_string()))?;

    let job_id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().timestamp();
    let total = req.data.len() as u64;

    let job = ToolJob {
        job_id: job_id.clone(),
        kind: ToolJobKind::Import,
        status: ToolJobStatus::Pending,
        progress: ToolJobProgress {
            current: 0,
            total: Some(total),
            message: Some("import pending".to_string()),
        },
        created_at: now,
        updated_at: now,
        elapsed_ms: None,
        artifacts: None,
        result: None,
        error: None,
    };

    {
        let mut jobs = state.tool_jobs.write().await;
        jobs.insert(job_id.clone(), job);
    }

    let state_clone = state.clone();
    let job_id_clone = job_id.clone();
    let handle = tokio::spawn(async move {
        let _permit = permit;
        update_tool_job(&state_clone, &job_id_clone, |j| {
            j.status = ToolJobStatus::Running;
            j.progress.message = Some("import running".to_string());
        })
        .await;

        let t_job = std::time::Instant::now();
        let res = run_import_job(&db_client, &state_clone, &job_id_clone, req).await;
        match res {
            Ok(result) => {
                let elapsed_ms = t_job.elapsed().as_millis();
                let mut result = result;
                if let Some(obj) = result.as_object_mut() {
                    obj.insert("elapsed_ms".to_string(), serde_json::json!(elapsed_ms));
                }
                update_tool_job(&state_clone, &job_id_clone, |j| {
                    j.status = ToolJobStatus::Completed;
                    j.progress.message = Some("import completed".to_string());
                    j.elapsed_ms = Some(elapsed_ms);
                    j.result = Some(result);
                })
                .await;
            }
            Err(e) => {
                let elapsed_ms = t_job.elapsed().as_millis();
                update_tool_job(&state_clone, &job_id_clone, |j| {
                    j.status = ToolJobStatus::Error;
                    j.elapsed_ms = Some(elapsed_ms);
                    j.error = Some(e.to_string());
                    j.progress.message = Some("import failed".to_string());
                })
                .await;
            }
        }
    });

    {
        let mut handles = state.tool_job_handles.write().await;
        handles.insert(job_id.clone(), handle.abort_handle());
    }

    Ok(Json(ToolJobStartResponse { job_id }))
}

async fn import_sql_job_start(
    State(state): State<AppState>,
    Json(req): Json<ImportSqlJobStartRequest>,
) -> Result<Json<ToolJobStartResponse>, AppError> {
    let permit = state
        .job_semaphore
        .clone()
        .try_acquire_owned()
        .map_err(|_| {
            AppError::TooManyRequests(format!(
                "job concurrency limit exceeded: max={}",
                state.limits.max_job_concurrency
            ))
        })?;
    let db_client = state
        .db_client
        .read()
        .await
        .clone()
        .ok_or_else(|| AppError::BadRequest("Database not connected".to_string()))?;

    let job_id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().timestamp();

    let job = ToolJob {
        job_id: job_id.clone(),
        kind: ToolJobKind::ImportSql,
        status: ToolJobStatus::Pending,
        progress: ToolJobProgress {
            current: 0,
            total: None,
            message: Some("import sql pending".to_string()),
        },
        created_at: now,
        updated_at: now,
        elapsed_ms: None,
        artifacts: None,
        result: None,
        error: None,
    };

    {
        let mut jobs = state.tool_jobs.write().await;
        jobs.insert(job_id.clone(), job);
    }

    let state_clone = state.clone();
    let job_id_clone = job_id.clone();
    let handle = tokio::spawn(async move {
        let _permit = permit;
        update_tool_job(&state_clone, &job_id_clone, |j| {
            j.status = ToolJobStatus::Running;
            j.progress.message = Some("import sql running".to_string());
        })
        .await;

        let t_job = std::time::Instant::now();
        let res = run_import_sql_job(&db_client, &state_clone, &job_id_clone, req).await;
        match res {
            Ok(result) => {
                let elapsed_ms = t_job.elapsed().as_millis();
                let mut result = result;
                if let Some(obj) = result.as_object_mut() {
                    obj.insert("elapsed_ms".to_string(), serde_json::json!(elapsed_ms));
                }
                update_tool_job(&state_clone, &job_id_clone, |j| {
                    j.status = ToolJobStatus::Completed;
                    j.progress.message = Some("import sql completed".to_string());
                    j.elapsed_ms = Some(elapsed_ms);
                    j.result = Some(result);
                })
                .await;
            }
            Err(e) => {
                let elapsed_ms = t_job.elapsed().as_millis();
                update_tool_job(&state_clone, &job_id_clone, |j| {
                    j.status = ToolJobStatus::Error;
                    j.elapsed_ms = Some(elapsed_ms);
                    j.error = Some(e.to_string());
                    j.progress.message = Some("import sql failed".to_string());
                })
                .await;
            }
        }
    });

    {
        let mut handles = state.tool_job_handles.write().await;
        handles.insert(job_id.clone(), handle.abort_handle());
    }

    Ok(Json(ToolJobStartResponse { job_id }))
}

fn mask_db_url(url: &str) -> String {
    let Some(scheme_idx) = url.find("://") else {
        return "****".to_string();
    };
    let scheme_end = scheme_idx + 3;
    let rest = &url[scheme_end..];
    let Some(at_idx) = rest.find('@') else {
        return url.to_string();
    };
    let creds = &rest[..at_idx];
    if creds.is_empty() {
        return url.to_string();
    }
    let masked_creds = if let Some(colon_idx) = creds.find(':') {
        format!("{}:****", &creds[..colon_idx])
    } else {
        "****".to_string()
    };
    format!("{}{}@{}", &url[..scheme_end], masked_creds, &rest[at_idx + 1..])
}

fn sanitize_config_for_report(config: &AppConfig) -> serde_json::Value {
    let mut v = serde_json::to_value(config).unwrap_or(serde_json::Value::Null);
    let Some(obj) = v.as_object_mut() else {
        return v;
    };

    let mask_str = serde_json::Value::String("****".to_string());

    if let Some(db_url) = obj.get_mut("db_url") {
        if let Some(s) = db_url.as_str() {
            *db_url = serde_json::Value::String(mask_db_url(s));
        }
    }

    if let Some(api_key) = obj.get_mut("api_key") {
        if api_key != &serde_json::Value::Null {
            *api_key = mask_str.clone();
        }
    }

    if let Some(pool) = obj.get_mut("token_pool") {
        if let Some(arr) = pool.as_array_mut() {
            for v in arr.iter_mut() {
                if v != &serde_json::Value::Null {
                    *v = mask_str.clone();
                }
            }
        }
    }

    if let Some(profiles) = obj.get_mut("ai_profiles") {
        if let Some(arr) = profiles.as_array_mut() {
            for p in arr.iter_mut() {
                let Some(pobj) = p.as_object_mut() else {
                    continue;
                };
                if let Some(api_key) = pobj.get_mut("api_key") {
                    if api_key != &serde_json::Value::Null {
                        *api_key = mask_str.clone();
                    }
                }
                if let Some(pool) = pobj.get_mut("pool") {
                    let Some(pool_obj) = pool.as_object_mut() else {
                        continue;
                    };
                    if let Some(tokens) = pool_obj.get_mut("tokens") {
                        if let Some(tokens_arr) = tokens.as_array_mut() {
                            for t in tokens_arr.iter_mut() {
                                if t != &serde_json::Value::Null {
                                    *t = mask_str.clone();
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    if let Some(conns) = obj.get_mut("db_connections") {
        if let Some(arr) = conns.as_array_mut() {
            for c in arr.iter_mut() {
                let Some(cobj) = c.as_object_mut() else {
                    continue;
                };
                if let Some(url) = cobj.get_mut("url") {
                    if let Some(s) = url.as_str() {
                        *url = serde_json::Value::String(mask_db_url(s));
                    }
                }
                if let Some(schema) = cobj.get_mut("schema") {
                    let Some(sobj) = schema.as_object_mut() else {
                        continue;
                    };
                    if let Some(url) = sobj.get_mut("url") {
                        if let Some(s) = url.as_str() {
                            *url = serde_json::Value::String(mask_db_url(s));
                        }
                    }
                }
            }
        }
    }

    v
}

fn ai_key_present(config: &AppConfig) -> bool {
    let p = config.resolve_ai_profile();
    match p.mode {
        core_lib::config::AiConnectionMode::Pool => p.pool.tokens.iter().any(|t| !t.trim().is_empty()),
        _ => !p.api_key.as_deref().unwrap_or("").trim().is_empty(),
    }
}

fn default_go_live_steps() -> Vec<String> {
    vec![
        "config".to_string(),
        "mysql_connect".to_string(),
        "sql_smoke".to_string(),
        "export_import_smoke".to_string(),
        "ai_smoke".to_string(),
    ]
}

fn normalize_go_live_steps(raw: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let steps = if raw.is_empty() { default_go_live_steps() } else { raw };
    for s in steps {
        let k = s.trim().to_lowercase();
        if k.is_empty() {
            continue;
        }
        if !matches!(
            k.as_str(),
            "config" | "mysql_connect" | "sql_smoke" | "export_import_smoke" | "ai_smoke"
        ) {
            continue;
        }
        if !out.iter().any(|x| x == &k) {
            out.push(k);
        }
    }
    if out.is_empty() {
        default_go_live_steps()
    } else {
        out
    }
}

fn go_live_step_is_write(step: &str) -> bool {
    matches!(step, "sql_smoke" | "export_import_smoke")
}

fn safe_ident_suffix(s: &str) -> String {
    let mut out = String::new();
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push('_');
        }
    }
    while out.contains("__") {
        out = out.replace("__", "_");
    }
    out.trim_matches('_').chars().take(24).collect()
}

fn apply_go_live_thresholds(
    step: &mut GoLiveStepReport,
    cumulative_ms: u128,
    thresholds: &Option<GoLiveThresholds>,
) {
    if matches!(step.status, GoLiveStepStatus::Skip) {
        return;
    }
    let Some(t) = thresholds else {
        return;
    };

    let mut violations: Vec<String> = Vec::new();

    if let Some(max_total_ms) = t.max_total_ms {
        if max_total_ms > 0 && cumulative_ms > (max_total_ms as u128) {
            violations.push(format!(
                "max_total_ms exceeded: actual={}ms threshold={}ms",
                cumulative_ms, max_total_ms
            ));
        }
    }

    if let Some(max_ms) = t.per_step_max_ms.get(&step.name).copied() {
        if max_ms > 0 && step.duration_ms > (max_ms as u128) {
            violations.push(format!(
                "per_step_max_ms exceeded: step={} actual={}ms threshold={}ms",
                step.name, step.duration_ms, max_ms
            ));
        }
    }

    if violations.is_empty() {
        return;
    }

    step.status = GoLiveStepStatus::Fail;
    step.code = Some("ERR_PERF_GATE".to_string());
    step.errors.extend(violations);
}

async fn go_live_job_start(
    State(state): State<AppState>,
    req: Option<Json<GoLiveJobStartRequest>>,
) -> Result<Json<ToolJobStartResponse>, AppError> {
    let permit = state
        .job_semaphore
        .clone()
        .try_acquire_owned()
        .map_err(|_| {
            AppError::TooManyRequests(format!(
                "job concurrency limit exceeded: max={}",
                state.limits.max_job_concurrency
            ))
        })?;

    let limits = state.limits.clone();
    ensure_temp_quota(&limits, 256 * 1024).await?;

    let mut req = req.map(|Json(v)| v).unwrap_or_default();
    req.steps = normalize_go_live_steps(req.steps);
    req.operator = req.operator.and_then(|s| {
        let t = s.trim().to_string();
        if t.is_empty() { None } else { Some(t) }
    });

    let job_id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().timestamp();

    let job = ToolJob {
        job_id: job_id.clone(),
        kind: ToolJobKind::GoLive,
        status: ToolJobStatus::Pending,
        progress: ToolJobProgress {
            current: 0,
            total: Some(5),
            message: Some("go-live pending".to_string()),
        },
        created_at: now,
        updated_at: now,
        elapsed_ms: None,
        artifacts: None,
        result: None,
        error: None,
    };

    {
        let mut jobs = state.tool_jobs.write().await;
        jobs.insert(job_id.clone(), job);
    }

    let state_clone = state.clone();
    let job_id_clone = job_id.clone();
    let req_clone = req.clone();
    let handle = tokio::spawn(async move {
        let _permit = permit;
        update_tool_job(&state_clone, &job_id_clone, |j| {
            j.status = ToolJobStatus::Running;
            j.progress.message = Some("go-live running".to_string());
        })
        .await;

        let t_job = std::time::Instant::now();
        let res = run_go_live_job(&state_clone, &job_id_clone, req_clone).await;
        match res {
            Ok((report, report_path)) => {
                let elapsed_ms = t_job.elapsed().as_millis();
                let passed = report.passed;
                update_tool_job(&state_clone, &job_id_clone, |j| {
                    j.elapsed_ms = Some(elapsed_ms);
                    j.artifacts = Some(ToolJobArtifacts {
                        data_path: Some(report_path),
                        manifest_path: None,
                        file_name: Some(format!("go-live-report-{}", job_id_clone)),
                        content_type: Some("application/json".to_string()),
                    });
                    j.result = Some(serde_json::json!({
                        "passed": passed,
                        "steps": report.steps,
                        "elapsed_ms": elapsed_ms
                    }));
                    if passed {
                        j.status = ToolJobStatus::Completed;
                        j.progress.message = Some("go-live completed".to_string());
                    } else {
                        j.status = ToolJobStatus::Error;
                        j.progress.message = Some("go-live failed".to_string());
                        j.error = Some("go-live failed".to_string());
                    }
                })
                .await;
            }
            Err(e) => {
                let elapsed_ms = t_job.elapsed().as_millis();
                update_tool_job(&state_clone, &job_id_clone, |j| {
                    j.status = ToolJobStatus::Error;
                    j.elapsed_ms = Some(elapsed_ms);
                    j.error = Some(e.to_string());
                    j.progress.message = Some("go-live failed".to_string());
                })
                .await;
            }
        }
    });

    {
        let mut handles = state.tool_job_handles.write().await;
        handles.insert(job_id.clone(), handle.abort_handle());
    }

    Ok(Json(ToolJobStartResponse { job_id }))
}

#[derive(Debug, Clone)]
struct GoLiveConnSpec {
    id: String,
    url: String,
    db_type: DbType,
    is_read_only: bool,
}

fn resolve_go_live_connections(config: &AppConfig, ids: &[String]) -> (Vec<GoLiveConnSpec>, Vec<String>) {
    let mut out: Vec<GoLiveConnSpec> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    let resolved_ids = if ids.is_empty() {
        if let Some(active_id) = &config.active_db_id {
            vec![active_id.clone()]
        } else {
            vec!["active".to_string()]
        }
    } else {
        ids.iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
    };

    for id in resolved_ids {
        if id == "active" {
            let url = config.get_active_db_url().unwrap_or_default();
            let db_type = config.get_active_db_type_enum();
            if url.trim().is_empty() {
                errors.push("missing active db url".to_string());
            }
            out.push(GoLiveConnSpec {
                id: "active".to_string(),
                url,
                db_type,
                is_read_only: false,
            });
            continue;
        }

        if let Some(conn) = config.db_connections.iter().find(|c| c.id == id) {
            let url = conn.url.clone();
            let db_type = conn.db_type.clone().unwrap_or_else(|| DbType::from_url(&url));
            if url.trim().is_empty() {
                errors.push(format!("missing db url for connection_id={}", id));
            }
            out.push(GoLiveConnSpec {
                id: conn.id.clone(),
                url,
                db_type,
                is_read_only: conn.is_read_only,
            });
        } else {
            errors.push(format!("db connection not found: {}", id));
        }
    }

    if out.is_empty() {
        out.push(GoLiveConnSpec {
            id: "active".to_string(),
            url: config.get_active_db_url().unwrap_or_default(),
            db_type: config.get_active_db_type_enum(),
            is_read_only: false,
        });
    }

    (out, errors)
}

async fn append_jsonl(path: &str, limits: &RuntimeLimits, value: &serde_json::Value) -> Result<(), AppError> {
    use tokio::io::AsyncWriteExt;
    let line = serde_json::to_string(value).map_err(|e| AppError::InternalError(e.to_string()))?;
    ensure_temp_quota(limits, (line.len() + 1) as u64).await?;
    let mut f = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    f.write_all(line.as_bytes())
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    f.write_all(b"\n")
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    Ok(())
}

async fn run_go_live_job(
    state: &AppState,
    job_id: &str,
    req: GoLiveJobStartRequest,
) -> Result<(GoLiveReport, String), AppError> {
    let t0 = std::time::Instant::now();
    let created_at = chrono::Utc::now();
    let mut steps: Vec<GoLiveStepReport> = Vec::new();

    let config = state.config.read().await.clone();
    let config_sanitized = sanitize_config_for_report(&config);
    let has_ai_key = ai_key_present(&config);
    let (connections, conn_errors) = resolve_go_live_connections(&config, &req.connection_ids);
    let operator = req.operator.clone();
    let requested_steps = normalize_go_live_steps(req.steps.clone());
    let thresholds = req.thresholds.clone().filter(|t| {
        t.max_total_ms.unwrap_or(0) > 0 || !t.per_step_max_ms.is_empty()
    });

    let per_conn_steps: Vec<String> = requested_steps
        .iter()
        .filter(|s| matches!(s.as_str(), "mysql_connect" | "sql_smoke" | "export_import_smoke"))
        .cloned()
        .collect();

    let total_steps = (if requested_steps.iter().any(|s| s == "config") { 1 } else { 0 })
        + (connections.len() * per_conn_steps.len())
        + (if requested_steps.iter().any(|s| s == "ai_smoke") { 1 } else { 0 });

    update_tool_job(state, job_id, |j| {
        j.progress.current = 0;
        j.progress.total = Some(total_steps as u64);
        j.progress.message = Some("go-live running".to_string());
    })
    .await;

    let mut current: u64 = 0;
    let mut passed = true;

    let mut config_failed = false;

    if requested_steps.iter().any(|s| s == "config") {
        update_tool_job(state, job_id, |j| {
            j.progress.message = Some("config check".to_string());
        })
        .await;

        let t_step = std::time::Instant::now();
        let errors = conn_errors.clone();
        ensure_temp_quota(&state.limits, 1).await?;

        let mut step = GoLiveStepReport {
            name: "config".to_string(),
            connection_id: None,
            status: if errors.is_empty() {
                GoLiveStepStatus::Pass
            } else {
                GoLiveStepStatus::Fail
            },
            duration_ms: t_step.elapsed().as_millis(),
            errors,
            code: None,
            details: Some(serde_json::json!({
                "config": config_sanitized,
                "connection_ids": connections.iter().map(|c| c.id.clone()).collect::<Vec<_>>(),
                "ai_key_present": has_ai_key,
                "operator": operator.clone(),
                "requested_steps": requested_steps.clone(),
                "thresholds": thresholds.clone()
            })),
        };

        apply_go_live_thresholds(&mut step, t0.elapsed().as_millis(), &thresholds);

        passed = passed && matches!(step.status, GoLiveStepStatus::Pass);
        config_failed = !passed;
        steps.push(step);
        current += 1;
        update_tool_job(state, job_id, |j| {
            j.progress.current = current;
        })
        .await;
    }

    if config_failed {
        for conn in &connections {
            for s in &per_conn_steps {
                steps.push(GoLiveStepReport {
                    name: s.clone(),
                    connection_id: Some(conn.id.clone()),
                    status: GoLiveStepStatus::Skip,
                    duration_ms: 0,
                    errors: vec!["skipped due to previous failure".to_string()],
                    code: None,
                    details: None,
                });
                current += 1;
            }
        }
        if requested_steps.iter().any(|s| s == "ai_smoke") {
            steps.push(GoLiveStepReport {
                name: "ai_smoke".to_string(),
                connection_id: None,
                status: GoLiveStepStatus::Skip,
                duration_ms: 0,
                errors: vec!["skipped due to previous failure".to_string()],
                code: None,
                details: None,
            });
            current += 1;
        }

        update_tool_job(state, job_id, |j| {
            j.progress.current = current;
        })
        .await;

        let report = GoLiveReport {
            job_id: job_id.to_string(),
            operator,
            connection_ids: connections.iter().map(|c| c.id.clone()).collect(),
            requested_steps,
            thresholds,
            created_at: created_at.to_rfc3339(),
            finished_at: chrono::Utc::now().to_rfc3339(),
            elapsed_ms: t0.elapsed().as_millis(),
            passed: false,
            steps,
        };
        let path = write_go_live_report(state, job_id, &report).await?;

        let limits = state.limits.clone();
        let temp_dir = limits.temp_dir.trim_end_matches('/').to_string();
        let index_path = format!("{}/go-live-index.jsonl", temp_dir);
        let audit_path = format!("{}/go-live-audit.jsonl", temp_dir);
        append_jsonl(&index_path, &limits, &serde_json::json!({
            "job_id": job_id,
            "created_at": report.created_at.clone(),
            "finished_at": report.finished_at.clone(),
            "passed": report.passed,
            "operator": report.operator.clone(),
            "connection_ids": report.connection_ids.clone(),
            "report_path": path
        }))
        .await?;
        append_jsonl(&audit_path, &limits, &serde_json::json!({
            "ts": chrono::Utc::now().timestamp(),
            "action": "go_live_job_finished",
            "job_id": job_id,
            "operator": report.operator.clone(),
            "passed": report.passed,
            "elapsed_ms": report.elapsed_ms
        }))
        .await?;
        return Ok((report, path));
    }

    let mut clients: HashMap<String, DbClient> = HashMap::new();
    let mut conn_ok: HashMap<String, bool> = HashMap::new();
    for conn in &connections {
        conn_ok.insert(conn.id.clone(), true);
    }

    for conn in &connections {
        let conn_id = conn.id.clone();
        let mut ok = *conn_ok.get(&conn_id).unwrap_or(&true);

        for s in &per_conn_steps {
            if !(ok || (conn.is_read_only && go_live_step_is_write(s))) {
                steps.push(GoLiveStepReport {
                    name: s.clone(),
                    connection_id: Some(conn_id.clone()),
                    status: GoLiveStepStatus::Skip,
                    duration_ms: 0,
                    errors: vec!["skipped due to previous failure".to_string()],
                    code: None,
                    details: None,
                });
                current += 1;
                continue;
            }

            update_tool_job(state, job_id, |j| {
                j.progress.message = Some(format!("{} {}", conn_id, s));
            })
            .await;

            if !matches!(conn.db_type, DbType::MySQL | DbType::MariaDB) {
                steps.push(GoLiveStepReport {
                    name: s.clone(),
                    connection_id: Some(conn_id.clone()),
                    status: GoLiveStepStatus::Skip,
                    duration_ms: 0,
                    errors: vec![format!(
                        "unsupported db_type: {} (only mysql/mariadb)",
                        conn.db_type.display_name()
                    )],
                    code: None,
                    details: Some(serde_json::json!({
                        "db_type": conn.db_type.display_name(),
                        "reason": "unsupported"
                    })),
                });
                current += 1;
                continue;
            }

            if conn.is_read_only && go_live_step_is_write(s) {
                steps.push(GoLiveStepReport {
                    name: s.clone(),
                    connection_id: Some(conn_id.clone()),
                    status: GoLiveStepStatus::Skip,
                    duration_ms: 0,
                    errors: Vec::new(),
                    code: None,
                    details: Some(serde_json::json!({ "reason": "read_only" })),
                });
                current += 1;
                continue;
            }

            let t_step = std::time::Instant::now();
            let mut errors: Vec<String> = Vec::new();
            let mut details: Option<serde_json::Value> = None;

            let client_res: Result<DbClient, String> = if let Some(c) = clients.get(&conn_id).cloned() {
                Ok(c)
            } else {
                DbClient::new(&conn.url)
                    .await
                    .map_err(|e| e.to_string())
            };

            let mut client_opt: Option<DbClient> = None;
            match client_res {
                Ok(c) => {
                    let r: Result<(i64,), sqlx::Error> = sqlx::query_as("SELECT 1").fetch_one(&c.pool).await;
                    if let Err(e) = r {
                        errors.push(e.to_string());
                    } else {
                        client_opt = Some(c.clone());
                        clients.insert(conn_id.clone(), c);
                    }
                }
                Err(e) => errors.push(e),
            }

            if errors.is_empty() {
                if s == "mysql_connect" {
                    details = Some(serde_json::json!({ "db_type": conn.db_type.display_name() }));
                } else if s == "sql_smoke" {
                    if let Some(client) = &client_opt {
                        match client.pool.acquire().await {
                            Ok(mut sql_conn) => {
                                let r: Result<(i64,), sqlx::Error> =
                                    sqlx::query_as("SELECT 1").fetch_one(&mut *sql_conn).await;
                                if let Err(e) = r {
                                    errors.push(e.to_string());
                                }
                                if errors.is_empty() {
                                    let r = sqlx::query(
                                        "CREATE TEMPORARY TABLE go_live_tmp_smoke (id INT PRIMARY KEY AUTO_INCREMENT, v INT NOT NULL)",
                                    )
                                    .execute(&mut *sql_conn)
                                    .await;
                                    if let Err(e) = r {
                                        errors.push(e.to_string());
                                    }
                                }
                                if errors.is_empty() {
                                    for i in 1..=25i64 {
                                        let r = sqlx::query("INSERT INTO go_live_tmp_smoke (v) VALUES (?)")
                                            .bind(i)
                                            .execute(&mut *sql_conn)
                                            .await;
                                        if let Err(e) = r {
                                            errors.push(e.to_string());
                                            break;
                                        }
                                    }
                                }
                                if errors.is_empty() {
                                    let page: Result<Vec<(i64,)>, sqlx::Error> = sqlx::query_as(
                                        "SELECT id FROM go_live_tmp_smoke ORDER BY id LIMIT 10 OFFSET 10",
                                    )
                                    .fetch_all(&mut *sql_conn)
                                    .await;
                                    match page {
                                        Ok(v) => {
                                            if v.len() != 10 {
                                                errors.push(format!("pagination rows != 10: {}", v.len()));
                                            } else if v[0].0 != 11 {
                                                errors.push(format!("pagination first id != 11: {}", v[0].0));
                                            }
                                        }
                                        Err(e) => errors.push(e.to_string()),
                                    }
                                }
                            }
                            Err(e) => errors.push(e.to_string()),
                        }
                    } else {
                        errors.push("missing db client".to_string());
                    }
                } else if s == "export_import_smoke" {
                    if let Some(client) = &client_opt {
                        let id_short = job_id.replace('-', "");
                        let suffix = format!(
                            "{}_{}",
                            &id_short[..8],
                            safe_ident_suffix(&conn_id)
                        );
                        let src_table = format!("go_live_smoke_items_{}", suffix);
                        let dst_table = format!("go_live_smoke_items_imported_{}", suffix);

                        let pool = client.pool.clone();
                        let drop_all = async {
                            let _ = sqlx::query(&format!("DROP TABLE IF EXISTS `{}`", dst_table))
                                .execute(&pool)
                                .await;
                            let _ = sqlx::query(&format!("DROP TABLE IF EXISTS `{}`", src_table))
                                .execute(&pool)
                                .await;
                        };

                        let limits = state.limits.clone();
                        let temp_dir = limits.temp_dir.trim_end_matches('/').to_string();
                        let export_path = format!("{}/go-live-export-{}-{}.json", temp_dir, job_id, safe_ident_suffix(&conn_id));
                        let dummy_job_id = uuid::Uuid::new_v4().to_string();
                        let nop_state = AppState {
                            tool_jobs: Arc::new(RwLock::new(HashMap::new())),
                            tool_job_handles: Arc::new(RwLock::new(HashMap::new())),
                            ..state.clone()
                        };

                        let r: Result<(usize, usize, u64), AppError> = async {
                            sqlx::query(&format!("DROP TABLE IF EXISTS `{}`", dst_table))
                                .execute(&pool)
                                .await
                                .map_err(|e| AppError::InternalError(e.to_string()))?;
                            sqlx::query(&format!("DROP TABLE IF EXISTS `{}`", src_table))
                                .execute(&pool)
                                .await
                                .map_err(|e| AppError::InternalError(e.to_string()))?;

                            sqlx::query(&format!(
                                "CREATE TABLE `{}` (id BIGINT PRIMARY KEY, name VARCHAR(255) NOT NULL, score DOUBLE NOT NULL, created_at DATETIME NOT NULL)",
                                src_table
                            ))
                            .execute(&pool)
                            .await
                            .map_err(|e| AppError::InternalError(e.to_string()))?;

                            sqlx::query(&format!(
                                "CREATE TABLE `{}` (id BIGINT PRIMARY KEY, name VARCHAR(255) NOT NULL, score DOUBLE NOT NULL, created_at DATETIME NOT NULL)",
                                dst_table
                            ))
                            .execute(&pool)
                            .await
                            .map_err(|e| AppError::InternalError(e.to_string()))?;

                            let now = chrono::Utc::now().naive_utc();
                            for i in 1..=25i64 {
                                sqlx::query(&format!(
                                    "INSERT INTO `{}` (id, name, score, created_at) VALUES (?, ?, ?, ?)",
                                    src_table
                                ))
                                .bind(i)
                                .bind(format!("item-{}", i))
                                .bind(i as f64 * 1.5)
                                .bind(now)
                                .execute(&pool)
                                .await
                                .map_err(|e| AppError::InternalError(e.to_string()))?;
                            }

                            ensure_temp_quota(&limits, 256 * 1024).await?;

                            let export_req = ExportJobStartRequest {
                                table_name: src_table.clone(),
                                export_type: "json".to_string(),
                                where_clause: Some("name LIKE 'item-%'".to_string()),
                                primary_key: Some("id".to_string()),
                                pk_start: Some("5".to_string()),
                                pk_end: Some("20".to_string()),
                                window_limit: Some(7),
                                window_offset: Some(3),
                            };

                            let _stats = run_export_job(client, &nop_state, &dummy_job_id, &export_req, &export_path, limits.max_file_bytes).await?;
                            let data = tokio::fs::read(&export_path)
                                .await
                                .map_err(|e| AppError::InternalError(e.to_string()))?;
                            let rows: Vec<std::collections::HashMap<String, serde_json::Value>> = serde_json::from_slice(&data)
                                .map_err(|e| AppError::InternalError(e.to_string()))?;
                            if rows.is_empty() {
                                return Err(AppError::InternalError("exported rows empty".to_string()));
                            }

                            let mut mapping = std::collections::HashMap::new();
                            mapping.insert("id".to_string(), "id".to_string());
                            mapping.insert("name".to_string(), "name".to_string());
                            mapping.insert("score".to_string(), "score".to_string());
                            mapping.insert("created_at".to_string(), "created_at".to_string());

                            let import_req = ImportJobStartRequest {
                                table_name: dst_table.clone(),
                                data: rows.clone(),
                                mapping,
                                skip_errors: false,
                            };

                            let import_res = run_import_job(client, &nop_state, &dummy_job_id, import_req).await?;
                            let inserted = import_res.get("inserted").and_then(|v| v.as_u64()).unwrap_or(0);

                            let (c,): (i64,) = sqlx::query_as(&format!("SELECT COUNT(*) FROM `{}`", dst_table))
                                .fetch_one(&pool)
                                .await
                                .map_err(|e| AppError::InternalError(e.to_string()))?;
                            if c as usize != rows.len() {
                                return Err(AppError::InternalError(format!(
                                    "imported count mismatch: {} vs {}",
                                    c,
                                    rows.len()
                                )));
                            }

                            Ok((rows.len(), inserted as usize, c as u64))
                        }
                        .await;

                        drop_all.await;

                        match r {
                            Ok((exported_rows, inserted_rows, counted)) => {
                                details = Some(serde_json::json!({
                                    "src_table": src_table,
                                    "dst_table": dst_table,
                                    "export_path": export_path,
                                    "exported_rows": exported_rows,
                                    "import_inserted": inserted_rows,
                                    "counted_rows": counted
                                }));
                            }
                            Err(e) => errors.push(e.to_string()),
                        }
                    } else {
                        errors.push("missing db client".to_string());
                    }
                }
            }

            let mut step = GoLiveStepReport {
                name: s.clone(),
                connection_id: Some(conn_id.clone()),
                status: if errors.is_empty() {
                    GoLiveStepStatus::Pass
                } else {
                    GoLiveStepStatus::Fail
                },
                duration_ms: t_step.elapsed().as_millis(),
                errors,
                code: None,
                details,
            };

            apply_go_live_thresholds(&mut step, t0.elapsed().as_millis(), &thresholds);

            passed = passed && !matches!(step.status, GoLiveStepStatus::Fail);
            ok = ok && !matches!(step.status, GoLiveStepStatus::Fail);
            steps.push(step);
            current += 1;
        }

        conn_ok.insert(conn_id, ok);
        update_tool_job(state, job_id, |j| {
            j.progress.current = current;
        })
        .await;
    }

    if requested_steps.iter().any(|s| s == "ai_smoke") {
        update_tool_job(state, job_id, |j| {
            j.progress.message = Some("ai smoke".to_string());
        })
        .await;

        let t_step = std::time::Instant::now();
        let mut errors = Vec::new();
        let mut details = None;
        let ai_step_status = if !has_ai_key {
            details = Some(serde_json::json!({ "reason": "missing_key" }));
            GoLiveStepStatus::Skip
        } else {
            let planner = state.planner.read().await.clone();
            match planner.generate_rule_template("go-live smoke", "SELECT 1;").await {
                Ok(sql) => {
                    details = Some(serde_json::json!({ "response": sql }));
                    GoLiveStepStatus::Pass
                }
                Err(e) => {
                    errors.push(e.to_string());
                    GoLiveStepStatus::Fail
                }
            }
        };

        let mut ai_step = GoLiveStepReport {
            name: "ai_smoke".to_string(),
            connection_id: None,
            status: ai_step_status,
            duration_ms: t_step.elapsed().as_millis(),
            errors,
            code: None,
            details,
        };
        apply_go_live_thresholds(&mut ai_step, t0.elapsed().as_millis(), &thresholds);
        passed = passed && !matches!(ai_step.status, GoLiveStepStatus::Fail);
        steps.push(ai_step);
        current += 1;
    }

    update_tool_job(state, job_id, |j| {
        j.progress.current = current;
        j.progress.message = Some(if passed {
            "go-live completed".to_string()
        } else {
            "go-live failed".to_string()
        });
    })
    .await;

    let report = GoLiveReport {
        job_id: job_id.to_string(),
        operator: operator.clone(),
        connection_ids: connections.iter().map(|c| c.id.clone()).collect(),
        requested_steps: requested_steps.clone(),
        thresholds: thresholds.clone(),
        created_at: created_at.to_rfc3339(),
        finished_at: chrono::Utc::now().to_rfc3339(),
        elapsed_ms: t0.elapsed().as_millis(),
        passed,
        steps,
    };
    let path = write_go_live_report(state, job_id, &report).await?;

    let limits = state.limits.clone();
    let temp_dir = limits.temp_dir.trim_end_matches('/').to_string();
    let index_path = format!("{}/go-live-index.jsonl", temp_dir);
    let audit_path = format!("{}/go-live-audit.jsonl", temp_dir);
    append_jsonl(&index_path, &limits, &serde_json::json!({
        "job_id": job_id,
        "created_at": report.created_at.clone(),
        "finished_at": report.finished_at.clone(),
        "passed": report.passed,
        "operator": report.operator.clone(),
        "connection_ids": report.connection_ids.clone(),
        "report_path": path
    }))
    .await?;
    append_jsonl(&audit_path, &limits, &serde_json::json!({
        "ts": chrono::Utc::now().timestamp(),
        "action": "go_live_job_finished",
        "job_id": job_id,
        "operator": report.operator.clone(),
        "passed": report.passed,
        "elapsed_ms": report.elapsed_ms
    }))
    .await?;

    Ok((report, path))
}

async fn write_go_live_report(state: &AppState, job_id: &str, report: &GoLiveReport) -> Result<String, AppError> {
    let limits = state.limits.clone();
    let temp_dir = limits.temp_dir.trim_end_matches('/').to_string();
    let report_path = format!("{}/go-live-report-{}.json", temp_dir, job_id);
    let bytes = serde_json::to_vec_pretty(report).map_err(|e| AppError::InternalError(e.to_string()))?;
    ensure_temp_quota(&limits, bytes.len() as u64).await?;
    tokio::fs::write(&report_path, bytes)
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    Ok(report_path)
}

struct ExportStats {
    sha256: String,
    line_count: u64,
    bytes: u64,
    row_count: u64,
}

async fn update_tool_job(state: &AppState, job_id: &str, f: impl FnOnce(&mut ToolJob)) {
    let mut jobs = state.tool_jobs.write().await;
    if let Some(job) = jobs.get_mut(job_id) {
        f(job);
        job.updated_at = chrono::Utc::now().timestamp();
    }
}

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn sql_literal(s: &str) -> String {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        "NULL".to_string()
    } else if trimmed.parse::<i64>().is_ok() || trimmed.parse::<f64>().is_ok() {
        trimmed.to_string()
    } else {
        format!("'{}'", trimmed.replace('\'', "''"))
    }
}

async fn write_stats_chunk(
    writer: &mut tokio::io::BufWriter<tokio::fs::File>,
    bytes: &mut u64,
    line_count: &mut u64,
    hasher: &mut sha2::Sha256,
    max_bytes: u64,
    buf: &[u8],
) -> Result<(), AppError> {
    use sha2::Digest;
    use tokio::io::AsyncWriteExt;

    if bytes.saturating_add(buf.len() as u64) > max_bytes {
        return Err(AppError::ResourceLimit(format!(
            "file size exceeded: bytes={}B, max={}B",
            bytes.saturating_add(buf.len() as u64),
            max_bytes
        )));
    }
    writer
        .write_all(buf)
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    *bytes += buf.len() as u64;
    *line_count += buf.iter().filter(|b| **b == b'\n').count() as u64;
    hasher.update(buf);
    Ok(())
}

async fn fetch_table_columns(pool: &sqlx::MySqlPool, table_name: &str) -> Result<Vec<String>, AppError> {
    use sqlx::Row;
    let rows = sqlx::query(
        "SELECT COLUMN_NAME FROM INFORMATION_SCHEMA.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = ? ORDER BY ORDINAL_POSITION",
    )
    .bind(table_name)
    .fetch_all(pool)
    .await
    .map_err(|e| AppError::InternalError(e.to_string()))?;

    let mut cols = Vec::new();
    for r in rows {
        let name: String = r.try_get(0).map_err(|e| AppError::InternalError(e.to_string()))?;
        cols.push(name);
    }
    Ok(cols)
}

async fn run_export_job(
    db_client: &DbClient,
    state: &AppState,
    job_id: &str,
    req: &ExportJobStartRequest,
    data_path: &str,
    max_bytes: u64,
) -> Result<ExportStats, AppError> {
    use sha2::Digest;
    use sqlx::Column;
    use sqlx::Row;
    use tokio::io::AsyncWriteExt;

    let export_type_raw = req.export_type.to_lowercase();
    let is_excel_compat = matches!(export_type_raw.as_str(), "xls" | "xlsx");
    let export_type = if is_excel_compat {
        "txt".to_string()
    } else {
        export_type_raw
    };

    if !matches!(
        export_type.as_str(),
        "csv" | "txt" | "sql" | "xml" | "json"
    ) {
        return Err(AppError::BadRequest(format!(
            "Unsupported export format: {}",
            req.export_type
        )));
    }

    let headers = fetch_table_columns(&db_client.pool, &req.table_name)
        .await
        .unwrap_or_default();

    let mut conditions = Vec::new();
    if let Some(w) = &req.where_clause {
        if !w.trim().is_empty() {
            conditions.push(format!("({})", w));
        }
    }
    if let Some(pk) = &req.primary_key {
        if let Some(s) = &req.pk_start {
            if !s.trim().is_empty() {
                conditions.push(format!("`{}` >= {}", pk, sql_literal(s)));
            }
        }
        if let Some(s) = &req.pk_end {
            if !s.trim().is_empty() {
                conditions.push(format!("`{}` <= {}", pk, sql_literal(s)));
            }
        }
    }

    let where_sql = if conditions.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", conditions.join(" AND "))
    };

    let order_sql = req
        .primary_key
        .as_ref()
        .map(|pk| format!(" ORDER BY `{}`", pk))
        .unwrap_or_default();

    let mut limit_sql = String::new();
    if let Some(lim) = req.window_limit {
        limit_sql.push_str(&format!(" LIMIT {}", lim));
        if let Some(off) = req.window_offset {
            limit_sql.push_str(&format!(" OFFSET {}", off));
        }
    }

    let data_sql = format!(
        "SELECT * FROM `{}`{}{}{}",
        req.table_name, where_sql, order_sql, limit_sql
    );

    let file = tokio::fs::File::create(data_path)
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    let mut writer = tokio::io::BufWriter::new(file);

    let mut bytes: u64 = 0;
    let mut line_count: u64 = 0;
    let mut hasher = sha2::Sha256::new();

    if is_excel_compat {
        write_stats_chunk(
            &mut writer,
            &mut bytes,
            &mut line_count,
            &mut hasher,
            max_bytes,
            b"\xEF\xBB\xBF",
        )
        .await?;
    }

    if export_type == "csv" {
        let header = if headers.is_empty() {
            String::new()
        } else {
            DataExporter::csv_header(&headers)
        };
        if !header.is_empty() {
            write_stats_chunk(
                &mut writer,
                &mut bytes,
                &mut line_count,
                &mut hasher,
                max_bytes,
                header.as_bytes(),
            )
            .await?;
        }
    } else if export_type == "txt" {
        if !headers.is_empty() {
            let mut s = String::new();
            s.push_str(&headers.join("\t"));
            s.push('\n');
            write_stats_chunk(
                &mut writer,
                &mut bytes,
                &mut line_count,
                &mut hasher,
                max_bytes,
                s.as_bytes(),
            )
            .await?;
        }
    } else if export_type == "sql" {
        if !headers.is_empty() {
            let header = DataExporter::sql_header(&req.table_name, &headers);
            write_stats_chunk(
                &mut writer,
                &mut bytes,
                &mut line_count,
                &mut hasher,
                max_bytes,
                header.as_bytes(),
            )
            .await?;
        }
    } else if export_type == "xml" {
        let mut s = String::new();
        s.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
        s.push_str(&format!(
            "<export schema_version=\"1\" table=\"{}\">\n",
            escape_xml(&req.table_name)
        ));
        s.push_str("<columns>\n");
        for h in &headers {
            s.push_str(&format!("  <column name=\"{}\" />\n", escape_xml(h)));
        }
        s.push_str("</columns>\n<rows>\n");
        write_stats_chunk(
            &mut writer,
            &mut bytes,
            &mut line_count,
            &mut hasher,
            max_bytes,
            s.as_bytes(),
        )
        .await?;
    }

    let mut stream = sqlx::query(&data_sql).fetch(&db_client.pool);
    let mut processed: u64 = 0;
    let mut is_first_json = true;
    let mut previous_row: Option<serde_json::Map<String, serde_json::Value>> = None;
    let mut effective_headers = headers;

    while let Some(row_result) = stream.next().await {
        let row = row_result.map_err(|e| AppError::InternalError(e.to_string()))?;
        if effective_headers.is_empty() {
            for col in row.columns() {
                effective_headers.push(col.name().to_string());
            }
            if export_type == "csv" && !effective_headers.is_empty() {
                let header = DataExporter::csv_header(&effective_headers);
                write_stats_chunk(
                    &mut writer,
                    &mut bytes,
                    &mut line_count,
                    &mut hasher,
                    max_bytes,
                    header.as_bytes(),
                )
                .await?;
            } else if export_type == "txt" && !effective_headers.is_empty() {
                let mut s = String::new();
                s.push_str(&effective_headers.join("\t"));
                s.push('\n');
                write_stats_chunk(
                    &mut writer,
                    &mut bytes,
                    &mut line_count,
                    &mut hasher,
                    max_bytes,
                    s.as_bytes(),
                )
                .await?;
            } else if export_type == "sql" && !effective_headers.is_empty() {
                let header = DataExporter::sql_header(&req.table_name, &effective_headers);
                write_stats_chunk(
                    &mut writer,
                    &mut bytes,
                    &mut line_count,
                    &mut hasher,
                    max_bytes,
                    header.as_bytes(),
                )
                .await?;
            } else if export_type == "xml" {
                let mut s = String::new();
                s.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
                s.push_str(&format!(
                    "<export schema_version=\"1\" table=\"{}\">\n",
                    escape_xml(&req.table_name)
                ));
                s.push_str("<columns>\n");
                for h in &effective_headers {
                    s.push_str(&format!("  <column name=\"{}\" />\n", escape_xml(h)));
                }
                s.push_str("</columns>\n<rows>\n");
                write_stats_chunk(
                    &mut writer,
                    &mut bytes,
                    &mut line_count,
                    &mut hasher,
                    max_bytes,
                    s.as_bytes(),
                )
                .await?;
            }
        }

        let mut map = serde_json::Map::new();
        for col in row.columns() {
            let col_name = col.name().to_string();
            if let Ok(val) = row.try_get::<Option<i64>, _>(col.ordinal()) {
                map.insert(col_name, serde_json::json!(val));
            } else if let Ok(val) = row.try_get::<Option<f64>, _>(col.ordinal()) {
                map.insert(col_name, serde_json::json!(val));
            } else if let Ok(val) = row.try_get::<Option<bool>, _>(col.ordinal()) {
                map.insert(col_name, serde_json::json!(val));
            } else if let Ok(val) = row.try_get::<Option<chrono::NaiveDateTime>, _>(col.ordinal()) {
                map.insert(col_name, serde_json::json!(val.map(|dt| dt.to_string())));
            } else if let Ok(val) = row.try_get::<Option<chrono::NaiveDate>, _>(col.ordinal()) {
                map.insert(col_name, serde_json::json!(val.map(|d| d.to_string())));
            } else if let Ok(val) = row.try_get::<Option<chrono::NaiveTime>, _>(col.ordinal()) {
                map.insert(col_name, serde_json::json!(val.map(|t| t.to_string())));
            } else if let Ok(val) = row.try_get::<Option<String>, _>(col.ordinal()) {
                map.insert(col_name, serde_json::json!(val));
            } else {
                let val: Option<Vec<u8>> = row.try_get(col.ordinal()).unwrap_or(None);
                if let Some(bytes) = val {
                    let s = String::from_utf8_lossy(&bytes).into_owned();
                    map.insert(col_name, serde_json::json!(s));
                } else {
                    map.insert(col_name, serde_json::Value::Null);
                }
            }
        }

        if export_type == "csv" {
            let row_s = DataExporter::csv_row(&effective_headers, &map);
            write_stats_chunk(
                &mut writer,
                &mut bytes,
                &mut line_count,
                &mut hasher,
                max_bytes,
                row_s.as_bytes(),
            )
            .await?;
        } else if export_type == "txt" {
            let mut vals = Vec::new();
            for h in &effective_headers {
                let v = match map.get(h) {
                    Some(serde_json::Value::Null) | None => String::new(),
                    Some(serde_json::Value::String(s)) => s.replace(['\t', '\n'], " "),
                    Some(v) => v.to_string(),
                };
                vals.push(v);
            }
            let line = format!("{}\n", vals.join("\t"));
            write_stats_chunk(
                &mut writer,
                &mut bytes,
                &mut line_count,
                &mut hasher,
                max_bytes,
                line.as_bytes(),
            )
            .await?;
        } else if export_type == "sql" {
            if let Some(prev) = previous_row.take() {
                let s = DataExporter::sql_row(&effective_headers, &prev, false);
                write_stats_chunk(
                    &mut writer,
                    &mut bytes,
                    &mut line_count,
                    &mut hasher,
                    max_bytes,
                    s.as_bytes(),
                )
                .await?;
            }
            previous_row = Some(map);
        } else if export_type == "json" {
            if let Some(prev) = previous_row.take() {
                let s = DataExporter::json_row(&prev, is_first_json, false);
                write_stats_chunk(
                    &mut writer,
                    &mut bytes,
                    &mut line_count,
                    &mut hasher,
                    max_bytes,
                    s.as_bytes(),
                )
                .await?;
                is_first_json = false;
            }
            previous_row = Some(map);
        } else if export_type == "xml" {
            let mut s = String::new();
            s.push_str("  <row>\n");
            for h in &effective_headers {
                let val_s = match map.get(h) {
                    Some(serde_json::Value::Null) | None => String::new(),
                    Some(serde_json::Value::String(v)) => v.clone(),
                    Some(v) => v.to_string(),
                };
                s.push_str(&format!(
                    "    <col name=\"{}\">{}</col>\n",
                    escape_xml(h),
                    escape_xml(&val_s)
                ));
            }
            s.push_str("  </row>\n");
            write_stats_chunk(
                &mut writer,
                &mut bytes,
                &mut line_count,
                &mut hasher,
                max_bytes,
                s.as_bytes(),
            )
            .await?;
        }

        processed += 1;
        if processed.is_multiple_of(200) {
            update_tool_job(state, job_id, |j| {
                j.progress.current = processed;
            })
            .await;
        }
    }

    if export_type == "sql" {
        if let Some(prev) = previous_row {
            let s = DataExporter::sql_row(&effective_headers, &prev, true);
            write_stats_chunk(
                &mut writer,
                &mut bytes,
                &mut line_count,
                &mut hasher,
                max_bytes,
                s.as_bytes(),
            )
            .await?;
        }
    } else if export_type == "json" {
        if let Some(prev) = previous_row {
            let s = DataExporter::json_row(&prev, is_first_json, true);
            write_stats_chunk(
                &mut writer,
                &mut bytes,
                &mut line_count,
                &mut hasher,
                max_bytes,
                s.as_bytes(),
            )
            .await?;
        } else {
            write_stats_chunk(
                &mut writer,
                &mut bytes,
                &mut line_count,
                &mut hasher,
                max_bytes,
                b"[]\n",
            )
            .await?;
        }
    } else if export_type == "xml" {
        write_stats_chunk(
            &mut writer,
            &mut bytes,
            &mut line_count,
            &mut hasher,
            max_bytes,
            b"</rows>\n</export>\n",
        )
        .await?;
    }

    writer
        .flush()
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;

    update_tool_job(state, job_id, |j| {
        j.progress.current = processed;
    })
    .await;

    let hash = hasher.finalize().to_vec();
    let sha256 = hash.iter().map(|b| format!("{:02x}", b)).collect::<String>();

    Ok(ExportStats {
        sha256,
        line_count,
        bytes,
        row_count: processed,
    })
}

async fn run_import_job(
    db_client: &DbClient,
    state: &AppState,
    job_id: &str,
    req: ImportJobStartRequest,
) -> Result<serde_json::Value, AppError> {
    let table_name = req.table_name;

    let mapped_cols: Vec<(String, String)> = req
        .mapping
        .into_iter()
        .filter(|(_, src)| !src.is_empty())
        .collect();

    if mapped_cols.is_empty() {
        return Err(AppError::BadRequest("No columns mapped".to_string()));
    }

    let mut db_col_names: Vec<String> = Vec::with_capacity(mapped_cols.len());
    for (db, _) in &mapped_cols {
        db_col_names.push(quote_mysql_ident(db)?);
    }
    let col_list = db_col_names.join(", ");
    let placeholders = vec!["?"; mapped_cols.len()].join(", ");
    let table_ident = quote_mysql_ident(&table_name)?;
    let sql = format!("INSERT INTO {} ({}) VALUES ({})", table_ident, col_list, placeholders);

    let mut inserted: u64 = 0;
    let mut errors: u64 = 0;
    let mut error_details = Vec::new();

    for (i, row) in req.data.iter().enumerate() {
        let mut query = sqlx::query(&sql);
        for (_, src_field) in &mapped_cols {
            if let Some(val) = row.get(src_field) {
                match val {
                    serde_json::Value::Null => query = query.bind(None::<String>),
                    serde_json::Value::Bool(b) => query = query.bind(b),
                    serde_json::Value::Number(n) => {
                        if let Some(i) = n.as_i64() {
                            query = query.bind(i);
                        } else if let Some(f) = n.as_f64() {
                            query = query.bind(f);
                        } else {
                            query = query.bind(n.to_string());
                        }
                    }
                    serde_json::Value::String(s) => query = query.bind(s),
                    _ => query = query.bind(val.to_string()),
                }
            } else {
                query = query.bind(None::<String>);
            }
        }

        match query.execute(&db_client.pool).await {
            Ok(_) => inserted += 1,
            Err(e) => {
                errors += 1;
                error_details.push(format!("Row {}: {}", i + 1, e));
                if !req.skip_errors {
                    break;
                }
            }
        }

        if (i + 1) % 200 == 0 {
            update_tool_job(state, job_id, |j| {
                j.progress.current = (i + 1) as u64;
            })
            .await;
        }
    }

    update_tool_job(state, job_id, |j| {
        j.progress.current = req.data.len() as u64;
    })
    .await;

    Ok(serde_json::json!({
        "inserted": inserted,
        "errors": errors,
        "error_details": error_details,
    }))
}

async fn run_import_sql_job(
    db_client: &DbClient,
    state: &AppState,
    job_id: &str,
    req: ImportSqlJobStartRequest,
) -> Result<serde_json::Value, AppError> {
    let is_read_only = {
        let config = state.config.read().await;
        if let Some(active_id) = &config.active_db_id {
            config
                .db_connections
                .iter()
                .find(|c| &c.id == active_id)
                .map(|c| c.is_read_only)
                .unwrap_or(false)
        } else {
            false
        }
    };

    let mut clean_sql = req.sql.trim().to_string();
    loop {
        if clean_sql.starts_with("--") {
            if let Some(idx) = clean_sql.find('\n') {
                clean_sql = clean_sql[idx + 1..].trim().to_string();
            } else {
                clean_sql = String::new();
            }
        } else if clean_sql.starts_with("/*") {
            if let Some(idx) = clean_sql.find("*/") {
                clean_sql = clean_sql[idx + 2..].trim().to_string();
            } else {
                clean_sql = String::new();
            }
        } else {
            break;
        }
    }

    let upper_sql = clean_sql.to_uppercase();
    let is_select = upper_sql.starts_with("SELECT")
        || upper_sql.starts_with("SHOW")
        || upper_sql.starts_with("DESCRIBE")
        || upper_sql.starts_with("EXPLAIN");

    if is_read_only && !is_select {
        return Err(AppError::Forbidden(
            "当前连接为只读模式，禁止执行非查询操作！".to_string(),
        ));
    }

    let is_dangerous = upper_sql.contains("UPDATE ")
        || upper_sql.contains("DELETE ")
        || upper_sql.contains("DROP ")
        || upper_sql.contains("TRUNCATE ")
        || upper_sql.contains("ALTER ");

    if is_dangerous && req.force != Some(true) {
        let body = serde_json::json!({
            "error": "DANGEROUS_SQL",
            "message": "检测到高危操作，请确认后强制执行"
        })
        .to_string();
        return Err(AppError::BadRequest(body));
    }

    update_tool_job(state, job_id, |j| {
        j.progress.message = Some("executing".to_string());
    })
    .await;

    let result = sqlx::query(&req.sql)
        .execute(&db_client.pool)
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;

    Ok(serde_json::json!({
        "affected_rows": result.rows_affected()
    }))
}

async fn get_temp_db_client(state: &AppState, db_id: &str) -> Result<(DbClient, String), AppError> {
    let config = state.config.read().await.clone();
    let conn = config
        .db_connections
        .iter()
        .find(|c| c.id == db_id)
        .ok_or_else(|| AppError::BadRequest(format!("Database connection {} not found", db_id)))?;
    let client = DbClient::new(&conn.url)
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    let db_name = DbClient::extract_db_name(&conn.url).unwrap_or_default();
    Ok((client, db_name))
}

#[derive(Deserialize)]
struct SyncSchemaDiffRequest {
    source_db_id: String,
    target_db_id: String,
}

#[derive(Serialize)]
struct SyncSchemaDiffResponse {
    diff: core_lib::tools::SchemaDiff,
}

async fn sync_schema_diff(
    State(state): State<AppState>,
    Json(req): Json<SyncSchemaDiffRequest>,
) -> Result<Json<SyncSchemaDiffResponse>, AppError> {
    let (source_client, source_db_name) = get_temp_db_client(&state, &req.source_db_id).await?;
    let (target_client, target_db_name) = get_temp_db_client(&state, &req.target_db_id).await?;

    let source = fetch_schema_for_db(&source_client, &source_db_name)
        .await
        .ok_or_else(|| AppError::InternalError("Failed to fetch source schema".to_string()))?;
    let target = fetch_schema_for_db(&target_client, &target_db_name)
        .await
        .ok_or_else(|| AppError::InternalError("Failed to fetch target schema".to_string()))?;

    let (diff, _) = SyncEngine::schema_sync(&source, &target);
    Ok(Json(SyncSchemaDiffResponse { diff }))
}

#[derive(Deserialize)]
struct SyncSchemaDdlRequest {
    source_db_id: String,
    target_db_id: String,
    selected_tables: Vec<String>,
}

#[derive(Serialize)]
struct SyncSchemaDdlResponse {
    ddl_statements: String,
}

async fn sync_schema_ddl(
    State(state): State<AppState>,
    Json(req): Json<SyncSchemaDdlRequest>,
) -> Result<Json<SyncSchemaDdlResponse>, AppError> {
    let (source_client, source_db_name) = get_temp_db_client(&state, &req.source_db_id).await?;
    let (target_client, target_db_name) = get_temp_db_client(&state, &req.target_db_id).await?;

    let source = fetch_schema_for_db(&source_client, &source_db_name)
        .await
        .ok_or_else(|| AppError::InternalError("Failed to fetch source schema".to_string()))?;
    let target = fetch_schema_for_db(&target_client, &target_db_name)
        .await
        .ok_or_else(|| AppError::InternalError("Failed to fetch target schema".to_string()))?;

    let ddl = core_lib::sync::SchemaSyncEngine::generate_ddl_for_selection(
        &source,
        &target,
        &req.selected_tables,
    );
    Ok(Json(SyncSchemaDdlResponse {
        ddl_statements: ddl,
    }))
}

async fn fetch_all_table_data(
    db_client: &DbClient,
    table_name: &str,
) -> Result<Vec<serde_json::Value>, AppError> {
    use sqlx::Column;
    use sqlx::Row;

    let data_sql = format!("SELECT * FROM `{}` LIMIT 50000", table_name);
    let result_rows = sqlx::query(&data_sql)
        .fetch_all(&db_client.pool)
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;

    let mut rows = Vec::new();
    for row in result_rows {
        let mut map = serde_json::Map::new();
        for col in row.columns() {
            let col_name = col.name().to_string();
            if let Ok(val) = row.try_get::<Option<i64>, _>(col.ordinal()) {
                map.insert(col_name, serde_json::json!(val));
            } else if let Ok(val) = row.try_get::<Option<f64>, _>(col.ordinal()) {
                map.insert(col_name, serde_json::json!(val));
            } else if let Ok(val) = row.try_get::<Option<bool>, _>(col.ordinal()) {
                map.insert(col_name, serde_json::json!(val));
            } else if let Ok(val) = row.try_get::<Option<chrono::NaiveDateTime>, _>(col.ordinal()) {
                map.insert(col_name, serde_json::json!(val.map(|dt| dt.to_string())));
            } else if let Ok(val) = row.try_get::<Option<chrono::NaiveDate>, _>(col.ordinal()) {
                map.insert(col_name, serde_json::json!(val.map(|d| d.to_string())));
            } else if let Ok(val) = row.try_get::<Option<chrono::NaiveTime>, _>(col.ordinal()) {
                map.insert(col_name, serde_json::json!(val.map(|t| t.to_string())));
            } else if let Ok(val) = row.try_get::<Option<String>, _>(col.ordinal()) {
                map.insert(col_name, serde_json::json!(val));
            } else {
                let val: Option<Vec<u8>> = row.try_get(col.ordinal()).unwrap_or(None);
                if let Some(bytes) = val {
                    let s = String::from_utf8_lossy(&bytes).into_owned();
                    map.insert(col_name, serde_json::json!(s));
                } else {
                    map.insert(col_name, serde_json::Value::Null);
                }
            }
        }
        rows.push(serde_json::Value::Object(map));
    }
    Ok(rows)
}

#[derive(Deserialize)]
struct SyncDataDiffRequest {
    table_name: String,
    source_db_id: String,
    target_db_id: String,
    primary_key: String,
}

#[derive(Serialize)]
struct SyncDataDiffResponse {
    diff: core_lib::sync::DataDiff,
}

async fn sync_data_diff(
    State(state): State<AppState>,
    Json(req): Json<SyncDataDiffRequest>,
) -> Result<Json<SyncDataDiffResponse>, AppError> {
    let (source_client, _) = get_temp_db_client(&state, &req.source_db_id).await?;
    let (target_client, _) = get_temp_db_client(&state, &req.target_db_id).await?;

    let source_data = fetch_all_table_data(&source_client, &req.table_name).await?;
    let target_data = fetch_all_table_data(&target_client, &req.table_name).await?;

    let diff = core_lib::sync::DataSyncEngine::compute_data_diff(
        &req.table_name,
        &source_data,
        &target_data,
        &req.primary_key,
    );
    Ok(Json(SyncDataDiffResponse { diff }))
}

#[derive(Deserialize)]
struct SyncDataDmlRequest {
    diffs: Vec<core_lib::sync::DataDiff>,
    selections: std::collections::HashMap<String, Vec<String>>,
    primary_key: String,
}

#[derive(Serialize)]
struct SyncDataDmlResponse {
    dml_statements: String,
}

async fn sync_data_dml(
    Json(req): Json<SyncDataDmlRequest>,
) -> Result<Json<SyncDataDmlResponse>, AppError> {
    let dml = core_lib::sync::DataSyncEngine::generate_dml_for_selection(
        &req.diffs,
        &req.selections,
        &req.primary_key,
    );
    Ok(Json(SyncDataDmlResponse {
        dml_statements: dml,
    }))
}

#[derive(Deserialize)]
struct MySqlSyncCompareRequest {
    source_db_id: String,
    target_db_id: String,
    table_name: String,
    primary_key: String,
    mode: SyncMode,
    chunk_size: Option<usize>,
}

#[derive(Deserialize)]
struct MySqlSyncPreviewRequest {
    job_id: String,
    max_rows: Option<usize>,
    actions: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct MySqlSyncDeployRequest {
    job_id: String,
}

#[derive(Serialize)]
struct MySqlSyncJobStartResponse {
    job_id: String,
}

async fn mysql_sync_compare(
    State(state): State<AppState>,
    Json(req): Json<MySqlSyncCompareRequest>,
) -> Result<Json<MySqlSyncJobStartResponse>, AppError> {
    let permit = state
        .job_semaphore
        .clone()
        .try_acquire_owned()
        .map_err(|_| {
            AppError::TooManyRequests(format!(
                "job concurrency limit exceeded: max={}",
                state.limits.max_job_concurrency
            ))
        })?;
    {
        let config = state.config.read().await;
        if !config
            .db_connections
            .iter()
            .any(|c| c.id == req.source_db_id)
        {
            return Err(AppError::BadRequest(format!(
                "Database connection {} not found",
                req.source_db_id
            )));
        }
        if !config
            .db_connections
            .iter()
            .any(|c| c.id == req.target_db_id)
        {
            return Err(AppError::BadRequest(format!(
                "Database connection {} not found",
                req.target_db_id
            )));
        }
    }

    let job_id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().timestamp();
    let chunk_size = req.chunk_size.unwrap_or(1000).max(1);

    let job = MySqlSyncJob {
        job_id: job_id.clone(),
        stage: MySqlSyncStage::Compare,
        status: MySqlSyncJobStatus::Pending,
        progress: MySqlSyncProgress::default(),
        source_db_id: req.source_db_id.clone(),
        target_db_id: req.target_db_id.clone(),
        table_name: req.table_name.clone(),
        primary_key: req.primary_key.clone(),
        mode: req.mode.clone(),
        chunk_size,
        created_at: now,
        updated_at: now,
        compare_ms: None,
        preview_ms: None,
        deploy_ms: None,
        compare: None,
        preview: None,
        deploy: None,
        error: None,
    };

    {
        let mut jobs = state.sync_jobs.write().await;
        jobs.insert(job_id.clone(), job);
    }

    let state_clone = state.clone();
    let job_id_clone = job_id.clone();
    let source_db_id = req.source_db_id.clone();
    let target_db_id = req.target_db_id.clone();
    let table_name = req.table_name.clone();
    let primary_key = req.primary_key.clone();
    tokio::spawn(async move {
        let _permit = permit;
        update_mysql_sync_job(&state_clone, &job_id_clone, |j| {
            j.stage = MySqlSyncStage::Compare;
            j.status = MySqlSyncJobStatus::Running;
            j.progress = MySqlSyncProgress {
                current: 0,
                total: 0,
                message: Some("正在对比分块校验和".to_string()),
            };
            j.updated_at = chrono::Utc::now().timestamp();
            j.compare = None;
            j.preview = None;
            j.deploy = None;
            j.error = None;
        })
        .await;

        let t_stage = std::time::Instant::now();
        let res: Result<CompareResult, AppError> = async {
            let (source_client, _) = get_temp_db_client(&state_clone, &source_db_id).await?;
            let (target_client, _) = get_temp_db_client(&state_clone, &target_db_id).await?;

            let chunk_size = {
                state_clone
                    .sync_jobs
                    .read()
                    .await
                    .get(&job_id_clone)
                    .map(|j| j.chunk_size)
                    .unwrap_or(1000)
            };

            let compare = MySqlDataSyncEngine::compare(
                &source_client,
                &target_client,
                &table_name,
                &primary_key,
                chunk_size,
            )
            .await?;

            Ok(compare)
        }
        .await;
        let compare_ms = t_stage.elapsed().as_millis();

        match res {
            Ok(compare) => {
                update_mysql_sync_job(&state_clone, &job_id_clone, |j| {
                    j.status = MySqlSyncJobStatus::Completed;
                    j.compare_ms = Some(compare_ms);
                    j.progress = MySqlSyncProgress {
                        current: compare.chunks.len() as u64,
                        total: compare.chunks.len() as u64,
                        message: Some(format!("对比完成：{} 个分块不同", compare.different_chunks)),
                    };
                    j.updated_at = chrono::Utc::now().timestamp();
                    j.compare = Some(compare);
                })
                .await;
            }
            Err(e) => {
                update_mysql_sync_job(&state_clone, &job_id_clone, |j| {
                    j.status = MySqlSyncJobStatus::Error;
                    j.updated_at = chrono::Utc::now().timestamp();
                    j.compare_ms = Some(compare_ms);
                    j.error = Some(e.to_string());
                })
                .await;
            }
        }
    });

    Ok(Json(MySqlSyncJobStartResponse { job_id }))
}

async fn mysql_sync_preview(
    State(state): State<AppState>,
    Json(req): Json<MySqlSyncPreviewRequest>,
) -> Result<Json<MySqlSyncJobStartResponse>, AppError> {
    let permit = state
        .job_semaphore
        .clone()
        .try_acquire_owned()
        .map_err(|_| {
            AppError::TooManyRequests(format!(
                "job concurrency limit exceeded: max={}",
                state.limits.max_job_concurrency
            ))
        })?;
    let job = { state.sync_jobs.read().await.get(&req.job_id).cloned() };
    let job = job.ok_or_else(|| AppError::NotFound("job not found".to_string()))?;
    let compare = job
        .compare
        .clone()
        .ok_or_else(|| AppError::BadRequest("compare not completed".to_string()))?;

    let state_clone = state.clone();
    let job_id_clone = req.job_id.clone();
    let max_rows = req.max_rows.unwrap_or(2000).max(1);
    let actions = req.actions.clone();
    tokio::spawn(async move {
        let _permit = permit;
        update_mysql_sync_job(&state_clone, &job_id_clone, |j| {
            j.stage = MySqlSyncStage::Preview;
            j.status = MySqlSyncJobStatus::Running;
            j.progress = MySqlSyncProgress {
                current: 0,
                total: 0,
                message: Some("正在生成差异与预览SQL".to_string()),
            };
            j.updated_at = chrono::Utc::now().timestamp();
            j.preview = None;
            j.deploy = None;
            j.error = None;
        })
        .await;

        let t_stage = std::time::Instant::now();
        let res: Result<PreviewResult, AppError> = async {
            let (source_client, _) = get_temp_db_client(&state_clone, &job.source_db_id).await?;
            let (target_client, _) = get_temp_db_client(&state_clone, &job.target_db_id).await?;

            let preview = MySqlDataSyncEngine::preview(
                &source_client,
                &target_client,
                &compare,
                job.mode.clone(),
                max_rows,
                actions,
            )
            .await?;

            Ok(preview)
        }
        .await;
        let preview_ms = t_stage.elapsed().as_millis();

        match res {
            Ok(preview) => {
                update_mysql_sync_job(&state_clone, &job_id_clone, |j| {
                    j.status = MySqlSyncJobStatus::Completed;
                    j.updated_at = chrono::Utc::now().timestamp();
                    j.preview_ms = Some(preview_ms);
                    j.preview = Some(preview.clone());
                    j.progress = MySqlSyncProgress {
                        current: preview.diff.insert_count as u64
                            + preview.diff.update_count as u64
                            + preview.diff.delete_count as u64,
                        total: preview.diff.insert_count as u64
                            + preview.diff.update_count as u64
                            + preview.diff.delete_count as u64,
                        message: Some(if preview.truncated {
                            "预览已截断（命中最大行数限制）".to_string()
                        } else {
                            "预览生成完成".to_string()
                        }),
                    };
                })
                .await;
            }
            Err(e) => {
                update_mysql_sync_job(&state_clone, &job_id_clone, |j| {
                    j.status = MySqlSyncJobStatus::Error;
                    j.updated_at = chrono::Utc::now().timestamp();
                    j.preview_ms = Some(preview_ms);
                    j.error = Some(e.to_string());
                })
                .await;
            }
        }
    });

    Ok(Json(MySqlSyncJobStartResponse { job_id: req.job_id }))
}

async fn mysql_sync_deploy(
    State(state): State<AppState>,
    Json(req): Json<MySqlSyncDeployRequest>,
) -> Result<Json<MySqlSyncJobStartResponse>, AppError> {
    let permit = state
        .job_semaphore
        .clone()
        .try_acquire_owned()
        .map_err(|_| {
            AppError::TooManyRequests(format!(
                "job concurrency limit exceeded: max={}",
                state.limits.max_job_concurrency
            ))
        })?;
    let job = { state.sync_jobs.read().await.get(&req.job_id).cloned() };
    let job = job.ok_or_else(|| AppError::NotFound("job not found".to_string()))?;
    let preview = job
        .preview
        .clone()
        .ok_or_else(|| AppError::BadRequest("preview not completed".to_string()))?;

    let is_read_only = {
        let config = state.config.read().await;
        config
            .db_connections
            .iter()
            .find(|c| c.id == job.target_db_id)
            .map(|c| c.is_read_only)
            .unwrap_or(false)
    };
    if is_read_only {
        return Err(AppError::Forbidden(
            "当前目标连接为只读模式，禁止执行部署".to_string(),
        ));
    }

    let state_clone = state.clone();
    let job_id_clone = req.job_id.clone();
    tokio::spawn(async move {
        let _permit = permit;
        update_mysql_sync_job(&state_clone, &job_id_clone, |j| {
            j.stage = MySqlSyncStage::Deploy;
            j.status = MySqlSyncJobStatus::Running;
            j.progress = MySqlSyncProgress {
                current: 0,
                total: preview.statements.len() as u64,
                message: Some("正在部署变更到目标库".to_string()),
            };
            j.updated_at = chrono::Utc::now().timestamp();
            j.deploy = None;
            j.error = None;
        })
        .await;

        let t_stage = std::time::Instant::now();
        let res: Result<(u64, usize), AppError> = async {
            let (target_client, _) = get_temp_db_client(&state_clone, &job.target_db_id).await?;
            let total = preview.statements.len();
            let store = state_clone.sync_jobs.clone();
            let job_id = job_id_clone.clone();
            let affected = MySqlDataSyncEngine::deploy(
                &target_client,
                &preview.statements,
                move |cur, tot| {
                    let store = store.clone();
                    let job_id = job_id.clone();
                    tokio::spawn(async move {
                        let mut jobs = store.write().await;
                        if let Some(j) = jobs.get_mut(&job_id) {
                            j.progress.current = cur as u64;
                            j.progress.total = tot as u64;
                            j.progress.message = Some(format!("已执行 {}/{} 条语句", cur, tot));
                            j.updated_at = chrono::Utc::now().timestamp();
                        }
                    });
                },
            )
            .await?;

            Ok((affected, total))
        }
        .await;
        let deploy_ms = t_stage.elapsed().as_millis();

        match res {
            Ok((affected, total)) => {
                update_mysql_sync_job(&state_clone, &job_id_clone, |j| {
                    j.status = MySqlSyncJobStatus::Completed;
                    j.updated_at = chrono::Utc::now().timestamp();
                    j.deploy_ms = Some(deploy_ms);
                    j.deploy = Some(DeployResult {
                        affected_rows: affected,
                        statements: total,
                    });
                    j.progress = MySqlSyncProgress {
                        current: total as u64,
                        total: total as u64,
                        message: Some(format!("部署完成，影响行数 {}", affected)),
                    };
                })
                .await;
            }
            Err(e) => {
                update_mysql_sync_job(&state_clone, &job_id_clone, |j| {
                    j.status = MySqlSyncJobStatus::Error;
                    j.updated_at = chrono::Utc::now().timestamp();
                    j.deploy_ms = Some(deploy_ms);
                    j.error = Some(e.to_string());
                })
                .await;
            }
        }
    });

    Ok(Json(MySqlSyncJobStartResponse { job_id: req.job_id }))
}

async fn mysql_sync_job_status(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Result<Json<MySqlSyncJob>, AppError> {
    let job = { state.sync_jobs.read().await.get(&job_id).cloned() };
    job.map(Json)
        .ok_or_else(|| AppError::NotFound("job not found".to_string()))
}

async fn update_mysql_sync_job(state: &AppState, job_id: &str, f: impl FnOnce(&mut MySqlSyncJob)) {
    let mut jobs = state.sync_jobs.write().await;
    if let Some(job) = jobs.get_mut(job_id) {
        f(job);
    }
}

#[derive(Serialize)]
struct PerfSyncJobStartResponse {
    job_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PerfSyncCheckRequest {
    source_db_id: String,
    target_db_id: String,
    tier: Option<String>,
    tables: Option<Vec<PerfSyncTableSpec>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PerfSyncInsufficient {
    table_name: String,
    expected: u64,
    source: u64,
    target: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PerfSyncFillPlanItem {
    table_name: String,
    expected: u64,
    source_current: u64,
    target_current: u64,
    source_fill: u64,
    target_fill: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PerfSyncCheckResponse {
    tier: String,
    expected_rows: serde_json::Value,
    baseline_counts: HashMap<String, PerfSyncTableCount>,
    insufficient: Vec<PerfSyncInsufficient>,
    fill_plan: Vec<PerfSyncFillPlanItem>,
}

fn default_perf_tables() -> Vec<PerfSyncTableSpec> {
    vec![
        PerfSyncTableSpec {
            table_name: "users".to_string(),
            primary_key: "id".to_string(),
        },
        PerfSyncTableSpec {
            table_name: "orders".to_string(),
            primary_key: "id".to_string(),
        },
        PerfSyncTableSpec {
            table_name: "events".to_string(),
            primary_key: "id".to_string(),
        },
        PerfSyncTableSpec {
            table_name: "kv_hotspot".to_string(),
            primary_key: "id".to_string(),
        },
        PerfSyncTableSpec {
            table_name: "files".to_string(),
            primary_key: "id".to_string(),
        },
    ]
}

async fn perf_sync_check(
    State(state): State<AppState>,
    Json(req): Json<PerfSyncCheckRequest>,
) -> Result<Json<PerfSyncCheckResponse>, AppError> {
    {
        let config = state.config.read().await;
        if !config.db_connections.iter().any(|c| c.id == req.source_db_id) {
            return Err(AppError::BadRequest(format!(
                "Database connection {} not found",
                req.source_db_id
            )));
        }
        if !config.db_connections.iter().any(|c| c.id == req.target_db_id) {
            return Err(AppError::BadRequest(format!(
                "Database connection {} not found",
                req.target_db_id
            )));
        }
    }

    let tables = req.tables.unwrap_or_else(default_perf_tables);
    let tier_str = req.tier.clone().unwrap_or_else(|| "1m".to_string());
    let tier = core_lib::loadgen::LoadgenTier::parse(&tier_str)
        .unwrap_or(core_lib::loadgen::LoadgenTier::M1);

    let (source_client, _) = get_temp_db_client(&state, &req.source_db_id).await?;
    let (target_client, _) = get_temp_db_client(&state, &req.target_db_id).await?;
    core_lib::loadgen::LoadgenEngine::ensure_schema(&source_client, &target_client).await?;

    let baseline_counts = fetch_counts(&source_client, &target_client, &tables).await?;
    let expected_rows = tier.rows_map();

    let mut insufficient = Vec::new();
    let mut fill_plan = Vec::new();
    for t in &tables {
        let expected = expected_rows
            .get(&t.table_name)
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        if let Some(c) = baseline_counts.get(&t.table_name) {
            if c.source < expected || c.target < expected {
                insufficient.push(PerfSyncInsufficient {
                    table_name: t.table_name.clone(),
                    expected,
                    source: c.source,
                    target: c.target,
                });
            }
            let source_fill = expected.saturating_sub(c.source);
            let target_fill = expected.saturating_sub(c.target);
            if source_fill > 0 || target_fill > 0 {
                fill_plan.push(PerfSyncFillPlanItem {
                    table_name: t.table_name.clone(),
                    expected,
                    source_current: c.source,
                    target_current: c.target,
                    source_fill,
                    target_fill,
                });
            }
        }
    }

    Ok(Json(PerfSyncCheckResponse {
        tier: match tier {
            core_lib::loadgen::LoadgenTier::M1 => "1m".to_string(),
            core_lib::loadgen::LoadgenTier::M10 => "10m".to_string(),
            core_lib::loadgen::LoadgenTier::M100 => "100m".to_string(),
        },
        expected_rows,
        baseline_counts,
        insufficient,
        fill_plan,
    }))
}

async fn fetch_table_count(db: &DbClient, table: &str) -> Result<u64, AppError> {
    let policy = TimeoutPolicy::default();
    let sql = format!("SELECT COUNT(*) FROM `{}`", table);
    let fut = sqlx::query_scalar::<_, i64>(&sql).fetch_one(&db.pool);
    let v = tokio::time::timeout(policy.db_query, fut)
        .await
        .map_err(|_| AppError::Timeout(format!("统计表 {} 行数超时", table)))?
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    Ok(v.max(0) as u64)
}

async fn fetch_counts(
    source: &DbClient,
    target: &DbClient,
    tables: &[PerfSyncTableSpec],
) -> Result<HashMap<String, PerfSyncTableCount>, AppError> {
    let mut out = HashMap::new();
    for t in tables {
        let s = fetch_table_count(source, &t.table_name).await?;
        let tg = fetch_table_count(target, &t.table_name).await?;
        out.insert(
            t.table_name.clone(),
            PerfSyncTableCount {
                source: s,
                target: tg,
            },
        );
    }
    Ok(out)
}

async fn run_sync_table(
    source: &DbClient,
    target: &DbClient,
    table: &PerfSyncTableSpec,
    mode: SyncMode,
    chunk_size: usize,
    max_rows: usize,
) -> Result<PerfSyncTableSyncReport, AppError> {
    let t0 = std::time::Instant::now();
    let compare = MySqlDataSyncEngine::compare(
        source,
        target,
        &table.table_name,
        &table.primary_key,
        chunk_size,
    )
    .await?;
    let compare_ms = t0.elapsed().as_millis();

    let t1 = std::time::Instant::now();
    let preview = MySqlDataSyncEngine::preview(source, target, &compare, mode, max_rows, None).await?;
    let preview_ms = t1.elapsed().as_millis();

    let t2 = std::time::Instant::now();
    let affected = MySqlDataSyncEngine::deploy(target, &preview.statements, |_c, _t| {}).await?;
    let deploy_ms = t2.elapsed().as_millis();

    Ok(PerfSyncTableSyncReport {
        table_name: table.table_name.clone(),
        primary_key: table.primary_key.clone(),
        compare_ms,
        preview_ms,
        deploy_ms,
        compare_chunks: compare.chunks.len(),
        different_chunks: compare.different_chunks,
        insert_count: preview.diff.insert_count,
        update_count: preview.diff.update_count,
        delete_count: preview.diff.delete_count,
        statements: preview.statements.len(),
        truncated: preview.truncated,
        affected_rows: affected,
    })
}

async fn verify_table(
    source: &DbClient,
    target: &DbClient,
    table: &PerfSyncTableSpec,
    chunk_size: usize,
) -> Result<PerfSyncTableVerifyReport, AppError> {
    let t0 = std::time::Instant::now();
    let compare = MySqlDataSyncEngine::compare(
        source,
        target,
        &table.table_name,
        &table.primary_key,
        chunk_size,
    )
    .await?;
    Ok(PerfSyncTableVerifyReport {
        table_name: table.table_name.clone(),
        different_chunks: compare.different_chunks,
        chunks: compare.chunks.len(),
        verify_ms: t0.elapsed().as_millis(),
    })
}

async fn perf_sync_start(
    State(state): State<AppState>,
    Json(req): Json<PerfSyncStartRequest>,
) -> Result<Json<PerfSyncJobStartResponse>, AppError> {
    let permit = state
        .job_semaphore
        .clone()
        .try_acquire_owned()
        .map_err(|_| {
            AppError::TooManyRequests(format!(
                "job concurrency limit exceeded: max={}",
                state.limits.max_job_concurrency
            ))
        })?;
    {
        let config = state.config.read().await;
        if !config
            .db_connections
            .iter()
            .any(|c| c.id == req.source_db_id)
        {
            return Err(AppError::BadRequest(format!(
                "Database connection {} not found",
                req.source_db_id
            )));
        }
        if !config
            .db_connections
            .iter()
            .any(|c| c.id == req.target_db_id)
        {
            return Err(AppError::BadRequest(format!(
                "Database connection {} not found",
                req.target_db_id
            )));
        }
    }

    let job_id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().timestamp();

    let job = PerfSyncJob {
        job_id: job_id.clone(),
        stage: PerfSyncStage::Prepare,
        status: PerfSyncJobStatus::Pending,
        progress: PerfSyncProgress::default(),
        request: req.clone(),
        created_at: now,
        updated_at: now,
        report: None,
        error: None,
    };

    {
        let mut jobs = state.perf_sync_jobs.write().await;
        jobs.insert(job_id.clone(), job);
    }

    let state_clone = state.clone();
    let job_id_clone = job_id.clone();
    tokio::spawn(async move {
        let _permit = permit;
        let res: Result<PerfSyncReport, AppError> =
            run_perf_sync_job(&state_clone, &job_id_clone, req).await;
        match res {
            Ok(report) => {
                update_perf_sync_job(&state_clone, &job_id_clone, |j| {
                    j.status = PerfSyncJobStatus::Completed;
                    j.updated_at = chrono::Utc::now().timestamp();
                    j.report = Some(report);
                    j.progress = PerfSyncProgress {
                        current: j.progress.total,
                        total: j.progress.total,
                        message: Some("完成".to_string()),
                    };
                })
                .await;
            }
            Err(e) => {
                update_perf_sync_job(&state_clone, &job_id_clone, |j| {
                    j.status = PerfSyncJobStatus::Error;
                    j.updated_at = chrono::Utc::now().timestamp();
                    j.error = Some(e.to_string());
                })
                .await;
            }
        }
    });

    Ok(Json(PerfSyncJobStartResponse { job_id }))
}

async fn run_perf_sync_job(
    state: &AppState,
    job_id: &str,
    req: PerfSyncStartRequest,
) -> Result<PerfSyncReport, AppError> {
    update_perf_sync_job(state, job_id, |j| {
        j.status = PerfSyncJobStatus::Running;
        j.stage = PerfSyncStage::Prepare;
        j.progress = PerfSyncProgress {
            current: 0,
            total: 0,
            message: Some("准备中".to_string()),
        };
        j.updated_at = chrono::Utc::now().timestamp();
        j.report = None;
        j.error = None;
    })
    .await;

    let chunk_size = req.chunk_size.unwrap_or(1000).max(1);
    let max_rows = req.max_rows.unwrap_or(20000).max(1);
    let tables = req.tables.unwrap_or_else(default_perf_tables);

    let loadgen = req.loadgen.clone();
    let fill = loadgen.as_ref().and_then(|x| x.fill).unwrap_or(false);
    let reset = loadgen.as_ref().and_then(|x| x.reset).unwrap_or(false);
    let inject = loadgen.as_ref().and_then(|x| x.inject).unwrap_or(false);
    let seed = loadgen.as_ref().and_then(|x| x.seed).unwrap_or(1);
    let batch = loadgen.as_ref().and_then(|x| x.batch).unwrap_or(1000);
    let tier_str = req
        .tier
        .clone()
        .or_else(|| loadgen.as_ref().and_then(|x| x.tier.clone()))
        .unwrap_or_else(|| "1m".to_string());
    let tier = core_lib::loadgen::LoadgenTier::parse(&tier_str)
        .unwrap_or(core_lib::loadgen::LoadgenTier::M1);

    let t0 = std::time::Instant::now();
    let (source_client, _) = get_temp_db_client(state, &req.source_db_id).await?;
    let (target_client, _) = get_temp_db_client(state, &req.target_db_id).await?;

    let mut stage_ms: HashMap<String, u128> = HashMap::new();
    let mut loadgen_report: Option<core_lib::loadgen::LoadgenReport> = None;

    let t_prepare = std::time::Instant::now();
    if fill {
        update_perf_sync_job(state, job_id, |j| {
            j.stage = PerfSyncStage::Prepare;
            j.progress.message = Some("正在填充数据".to_string());
            j.updated_at = chrono::Utc::now().timestamp();
        })
        .await;

        let report = core_lib::loadgen::LoadgenEngine::run(
            &source_client,
            &target_client,
            core_lib::loadgen::LoadgenConfig {
                tier,
                reset,
                seed,
                batch,
                diverge: None,
            },
        )
        .await?;
        loadgen_report = Some(report);
    } else {
        core_lib::loadgen::LoadgenEngine::ensure_schema(&source_client, &target_client).await?;
    }
    stage_ms.insert("prepare".to_string(), t_prepare.elapsed().as_millis());

    update_perf_sync_job(state, job_id, |j| {
        j.stage = PerfSyncStage::DetectBaseline;
        j.progress.message = Some("正在检测数据量".to_string());
        j.updated_at = chrono::Utc::now().timestamp();
    })
    .await;

    let t_baseline = std::time::Instant::now();
    let baseline_counts = fetch_counts(&source_client, &target_client, &tables).await?;

    if !fill {
        let expected = tier.rows_map();
        let mut insufficient = Vec::new();
        for t in &tables {
            let exp = expected
                .get(&t.table_name)
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            if let Some(c) = baseline_counts.get(&t.table_name) {
                if c.source < exp || c.target < exp {
                    insufficient.push(format!(
                        "{}(expected {}, source {}, target {})",
                        t.table_name, exp, c.source, c.target
                    ));
                }
            }
        }
        if !insufficient.is_empty() {
            return Err(AppError::BadRequest(format!(
                "Insufficient data for tier {}: {}. Enable fill to auto-populate baseline dataset.",
                tier_str,
                insufficient.join(", ")
            )));
        }
    }
    stage_ms.insert("detect_baseline".to_string(), t_baseline.elapsed().as_millis());

    let mirror_injected_counts = if inject {
        update_perf_sync_job(state, job_id, |j| {
            j.stage = PerfSyncStage::InjectMirror;
            j.progress.message = Some("正在注入 mirror 差异".to_string());
            j.updated_at = chrono::Utc::now().timestamp();
        })
        .await;

        let t_inject = std::time::Instant::now();
        core_lib::loadgen::LoadgenEngine::diverge_target(
            &target_client,
            tier,
            core_lib::loadgen::DivergeProfile::Mirror,
        )
        .await?;
        let counts = fetch_counts(&source_client, &target_client, &tables).await?;
        stage_ms.insert("inject_mirror".to_string(), t_inject.elapsed().as_millis());
        counts
    } else {
        stage_ms.insert("inject_mirror".to_string(), 0);
        baseline_counts.clone()
    };

    update_perf_sync_job(state, job_id, |j| {
        j.stage = PerfSyncStage::Mirror;
        j.progress = PerfSyncProgress {
            current: 0,
            total: tables.len() as u64,
            message: Some("正在执行 mirror 同步".to_string()),
        };
        j.updated_at = chrono::Utc::now().timestamp();
    })
    .await;

    let t_mirror = std::time::Instant::now();
    let mut mirror_tables = Vec::new();
    for (idx, table) in tables.iter().enumerate() {
        let report = run_sync_table(
            &source_client,
            &target_client,
            table,
            SyncMode::Mirror,
            chunk_size,
            max_rows,
        )
        .await?;
        mirror_tables.push(report);
        update_perf_sync_job(state, job_id, |j| {
            j.progress.current = (idx + 1) as u64;
            j.progress.total = tables.len() as u64;
            j.progress.message = Some(format!("mirror 同步 {}/{}", idx + 1, tables.len()));
            j.updated_at = chrono::Utc::now().timestamp();
        })
        .await;
    }
    stage_ms.insert("mirror".to_string(), t_mirror.elapsed().as_millis());

    update_perf_sync_job(state, job_id, |j| {
        j.stage = PerfSyncStage::VerifyMirror;
        j.progress = PerfSyncProgress {
            current: 0,
            total: tables.len() as u64,
            message: Some("正在校验 mirror 结果".to_string()),
        };
        j.updated_at = chrono::Utc::now().timestamp();
    })
    .await;

    let t_verify_mirror = std::time::Instant::now();
    let mut mirror_verify = Vec::new();
    for (idx, table) in tables.iter().enumerate() {
        let v = verify_table(&source_client, &target_client, table, chunk_size).await?;
        mirror_verify.push(v);
        update_perf_sync_job(state, job_id, |j| {
            j.progress.current = (idx + 1) as u64;
            j.progress.total = tables.len() as u64;
            j.progress.message = Some(format!("mirror 校验 {}/{}", idx + 1, tables.len()));
            j.updated_at = chrono::Utc::now().timestamp();
        })
        .await;
    }
    stage_ms.insert(
        "verify_mirror".to_string(),
        t_verify_mirror.elapsed().as_millis(),
    );
    let mirror_passed = mirror_verify.iter().all(|v| v.different_chunks == 0);
    let mirror = PerfSyncModeReport {
        mode: SyncMode::Mirror,
        injected_counts: mirror_injected_counts,
        tables: mirror_tables,
        verify: mirror_verify,
        passed: mirror_passed,
    };

    let upsert_injected_counts = if inject {
        update_perf_sync_job(state, job_id, |j| {
            j.stage = PerfSyncStage::InjectUpsertOnly;
            j.progress.message = Some("正在注入 upsert_only 差异".to_string());
            j.updated_at = chrono::Utc::now().timestamp();
        })
        .await;

        let t_inject = std::time::Instant::now();
        core_lib::loadgen::LoadgenEngine::diverge_target(
            &target_client,
            tier,
            core_lib::loadgen::DivergeProfile::UpsertOnly,
        )
        .await?;
        let counts = fetch_counts(&source_client, &target_client, &tables).await?;
        stage_ms.insert("inject_upsert_only".to_string(), t_inject.elapsed().as_millis());
        counts
    } else {
        stage_ms.insert("inject_upsert_only".to_string(), 0);
        baseline_counts.clone()
    };

    update_perf_sync_job(state, job_id, |j| {
        j.stage = PerfSyncStage::UpsertOnly;
        j.progress = PerfSyncProgress {
            current: 0,
            total: tables.len() as u64,
            message: Some("正在执行 upsert_only 同步".to_string()),
        };
        j.updated_at = chrono::Utc::now().timestamp();
    })
    .await;

    let t_upsert = std::time::Instant::now();
    let mut upsert_tables = Vec::new();
    for (idx, table) in tables.iter().enumerate() {
        let report = run_sync_table(
            &source_client,
            &target_client,
            table,
            SyncMode::UpsertOnly,
            chunk_size,
            max_rows,
        )
        .await?;
        upsert_tables.push(report);
        update_perf_sync_job(state, job_id, |j| {
            j.progress.current = (idx + 1) as u64;
            j.progress.total = tables.len() as u64;
            j.progress.message = Some(format!("upsert_only 同步 {}/{}", idx + 1, tables.len()));
            j.updated_at = chrono::Utc::now().timestamp();
        })
        .await;
    }
    stage_ms.insert("upsert_only".to_string(), t_upsert.elapsed().as_millis());

    update_perf_sync_job(state, job_id, |j| {
        j.stage = PerfSyncStage::VerifyUpsertOnly;
        j.progress = PerfSyncProgress {
            current: 0,
            total: tables.len() as u64,
            message: Some("正在校验 upsert_only 结果".to_string()),
        };
        j.updated_at = chrono::Utc::now().timestamp();
    })
    .await;

    let t_verify_upsert = std::time::Instant::now();
    let mut upsert_verify = Vec::new();
    for (idx, table) in tables.iter().enumerate() {
        let v = verify_table(&source_client, &target_client, table, chunk_size).await?;
        upsert_verify.push(v);
        update_perf_sync_job(state, job_id, |j| {
            j.progress.current = (idx + 1) as u64;
            j.progress.total = tables.len() as u64;
            j.progress.message = Some(format!("upsert_only 校验 {}/{}", idx + 1, tables.len()));
            j.updated_at = chrono::Utc::now().timestamp();
        })
        .await;
    }
    stage_ms.insert(
        "verify_upsert_only".to_string(),
        t_verify_upsert.elapsed().as_millis(),
    );
    let upsert_passed = upsert_verify.iter().all(|v| v.different_chunks == 0);
    let upsert_only = PerfSyncModeReport {
        mode: SyncMode::UpsertOnly,
        injected_counts: upsert_injected_counts,
        tables: upsert_tables,
        verify: upsert_verify,
        passed: upsert_passed,
    };

    Ok(PerfSyncReport {
        baseline_counts,
        loadgen: loadgen_report,
        mirror,
        upsert_only,
        stage_ms,
        elapsed_ms: t0.elapsed().as_millis(),
    })
}

async fn perf_sync_job_status(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Result<Json<PerfSyncJob>, AppError> {
    let job = { state.perf_sync_jobs.read().await.get(&job_id).cloned() };
    job.map(Json)
        .ok_or_else(|| AppError::NotFound("job not found".to_string()))
}

async fn update_perf_sync_job(
    state: &AppState,
    job_id: &str,
    f: impl FnOnce(&mut PerfSyncJob),
) {
    let mut jobs = state.perf_sync_jobs.write().await;
    if let Some(job) = jobs.get_mut(job_id) {
        f(job);
    }
}
// ----------------- SQL History API Handlers -----------------

async fn get_history(State(state): State<AppState>) -> Result<Json<Vec<SqlHistory>>, AppError> {
    let store = state.sql_history.read().await;
    Ok(Json(store.data.history.clone()))
}

async fn clear_history(State(state): State<AppState>) -> Result<StatusCode, AppError> {
    let store_clone = {
        let mut store = state.sql_history.write().await;
        store.clear_history();
        store.clone()
    };
    store_clone
        .save()
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    Ok(StatusCode::OK)
}

#[derive(Deserialize)]
struct ExplainSqlRequest {
    sql: String,
}

#[derive(Serialize)]
struct ExplainSqlResponse {
    rows: Vec<serde_json::Value>,
}

async fn explain_sql(
    State(state): State<AppState>,
    Json(req): Json<ExplainSqlRequest>,
) -> Result<Json<ExplainSqlResponse>, AppError> {
    let db_client = state
        .db_client
        .read()
        .await
        .clone()
        .ok_or_else(|| AppError::BadRequest("Database not connected".to_string()))?;

    let explain_sql = format!("EXPLAIN {}", req.sql);
    use sqlx::Column;
    use sqlx::Row;

    let result_rows = sqlx::query(&explain_sql)
        .fetch_all(&db_client.pool)
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;

    let mut rows = Vec::new();
    for row in result_rows {
        let mut map = serde_json::Map::new();
        for col in row.columns() {
            let col_name = col.name().to_string();

            if let Ok(val) = row.try_get::<Option<i64>, _>(col.ordinal()) {
                map.insert(col_name, serde_json::json!(val));
            } else if let Ok(val) = row.try_get::<Option<f64>, _>(col.ordinal()) {
                map.insert(col_name, serde_json::json!(val));
            } else if let Ok(val) = row.try_get::<Option<String>, _>(col.ordinal()) {
                map.insert(col_name, serde_json::json!(val));
            } else {
                let val: Option<Vec<u8>> = row.try_get(col.ordinal()).unwrap_or(None);
                if let Some(bytes) = val {
                    let s = String::from_utf8_lossy(&bytes).into_owned();
                    map.insert(col_name, serde_json::json!(s));
                } else {
                    map.insert(col_name, serde_json::Value::Null);
                }
            }
        }
        rows.push(serde_json::Value::Object(map));
    }

    Ok(Json(ExplainSqlResponse { rows }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        http::Request,
        response::IntoResponse,
        routing::{get, post},
        Router,
    };
    use std::time::Duration;
    use tokio::sync::Semaphore;
    use tower::ServiceExt;

    fn test_state() -> AppState {
        let config = AppConfig::default();
        let gateway = AiGateway::new(config.clone());
        let planner = Planner::new(gateway);
        let limits = RuntimeLimits {
            temp_dir: format!("/tmp/local-ai-sql-test-{}", uuid::Uuid::new_v4()),
            ..Default::default()
        };

        AppState {
            config: Arc::new(RwLock::new(config)),
            db_client: Arc::new(RwLock::new(None)),
            planner: Arc::new(RwLock::new(planner)),
            virtual_schema: Arc::new(RwLock::new(None)),
            rule_store: Arc::new(RwLock::new(RuleStore::default())),
            policy: Arc::new(RwLock::new(Policy::default())),
            sql_history: Arc::new(RwLock::new(SqlHistoryStore::default())),
            knowledge_base: Arc::new(RwLock::new(KnowledgeBase::default())),
            sync_jobs: Arc::new(RwLock::new(HashMap::new())),
            perf_sync_jobs: Arc::new(RwLock::new(HashMap::new())),
            tool_jobs: Arc::new(RwLock::new(HashMap::new())),
            tool_job_handles: Arc::new(RwLock::new(HashMap::new())),
            timeouts: TimeoutPolicy::default(),
            limits: limits.clone(),
            job_semaphore: Arc::new(Semaphore::new(limits.max_job_concurrency)),
        }
    }

    fn test_app(state: AppState) -> Router {
        let api = Router::new()
            .route("/config", get(get_config))
            .route("/execute", post(execute_sql))
            .route("/tools/schema-sync/diff", post(sync_schema_diff))
            .route("/tools/data-transfer/execute", post(transfer_execute))
            .route("/tools/mysql-sync/compare", post(mysql_sync_compare))
            .route("/tools/mysql-sync/preview", post(mysql_sync_preview))
            .route("/tools/mysql-sync/deploy", post(mysql_sync_deploy))
            .route("/tools/mysql-sync/jobs/:job_id", get(mysql_sync_job_status))
            .route("/tools/perf-sync/start", post(perf_sync_start))
            .route("/tools/perf-sync/check", post(perf_sync_check))
            .route("/tools/perf-sync/jobs/:job_id", get(perf_sync_job_status))
            .route("/tools/jobs/go-live/start", post(go_live_job_start))
            .route("/tools/go-live/reports", get(go_live_reports_list))
            .route("/tools/go-live/audit", get(go_live_audit_list))
            .route("/tools/jobs/:job_id", get(tool_job_status))
            .route("/tools/jobs/:job_id/cancel", post(tool_job_cancel))
            .route(
                "/tools/jobs/:job_id/artifacts/:artifact",
                get(tool_job_artifact_download),
            );

        Router::new().nest("/backend", api).with_state(state)
    }

    #[tokio::test]
    async fn config_endpoint_returns_json() {
        let app = test_app(test_state());

        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/backend/config")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(v.get("db_connections").is_some());
        assert!(v.get("ai_profiles").is_some());
    }

    #[tokio::test]
    async fn execute_sql_returns_error_when_db_not_connected() {
        let app = test_app(test_state());

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/backend/execute")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"sql":"SELECT 1"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v.get("code").and_then(|x| x.as_str()), Some("ERR_BAD_REQUEST"));
        assert!(v.get("type").is_some());
        assert!(v.get("details").and_then(|x| x.as_str()).unwrap_or("").contains("Database not connected"));
    }

    #[tokio::test]
    async fn timeout_error_includes_code_and_type() {
        let resp = AppError::Timeout("x".to_string()).into_response();
        assert_eq!(resp.status(), StatusCode::GATEWAY_TIMEOUT);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v.get("code").and_then(|x| x.as_str()), Some("ERR_TIMEOUT"));
        assert_eq!(v.get("type").and_then(|x| x.as_str()), Some("timeout"));
    }

    #[tokio::test]
    async fn tool_job_cancel_sets_canceled_status() {
        let state = test_state();
        let job_id = "job-test";
        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        {
            let mut jobs = state.tool_jobs.write().await;
            jobs.insert(
                job_id.to_string(),
                ToolJob {
                    job_id: job_id.to_string(),
                    kind: ToolJobKind::Export,
                    status: ToolJobStatus::Running,
                    progress: ToolJobProgress {
                        current: 0,
                        total: None,
                        message: None,
                    },
                    created_at: 0,
                    updated_at: 0,
                    elapsed_ms: None,
                    artifacts: None,
                    result: None,
                    error: None,
                },
            );
            let mut handles = state.tool_job_handles.write().await;
            handles.insert(job_id.to_string(), handle.abort_handle());
        }

        let app = test_app(state.clone());
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/backend/tools/jobs/{}/cancel", job_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(format!("/backend/tools/jobs/{}", job_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v.get("status").and_then(|x| x.as_str()), Some("canceled"));
    }

    #[tokio::test]
    async fn transfer_execute_returns_error_when_source_db_missing() {
        let app = test_app(test_state());

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/backend/tools/data-transfer/execute")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"source_type":"network_db","source_db_id":"missing","source_path":null,"source_url":null,"source_table":null,"target_url":"mysql://root@127.0.0.1:3306/test","target_table":"t","mode":"Append","mappings":[]}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let s = String::from_utf8_lossy(&body);
        assert!(s.contains("Source DB connection not found"));
    }

    #[tokio::test]
    async fn go_live_job_start_creates_report_artifact_on_failure() {
        let app = test_app(test_state());

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/backend/tools/jobs/go-live/start")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let job_id = v.get("job_id").and_then(|x| x.as_str()).unwrap_or("").to_string();
        assert!(!job_id.is_empty());

        let mut job: Option<serde_json::Value> = None;
        for _ in 0..200 {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("GET")
                        .uri(format!("/backend/tools/jobs/{}", job_id))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_eq!(resp.status(), StatusCode::OK);
            let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let status = v.get("status").and_then(|x| x.as_str()).unwrap_or("");
            if status == "completed" || status == "error" || status == "canceled" {
                job = Some(v);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let job = job.expect("job not finished");
        assert_eq!(job.get("kind").and_then(|x| x.as_str()), Some("go_live"));
        assert_eq!(job.get("status").and_then(|x| x.as_str()), Some("error"));
        let data_path = job
            .get("artifacts")
            .and_then(|a| a.get("data_path"))
            .and_then(|x| x.as_str())
            .unwrap_or("");
        assert!(data_path.ends_with(".json"));

        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(format!("/backend/tools/jobs/{}/artifacts/data", job_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let report: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let steps = report.get("steps").and_then(|x| x.as_array()).cloned().unwrap_or_default();
        assert_eq!(steps.len(), 5);
    }

    #[tokio::test]
    async fn go_live_reports_list_and_read_only_skip_work() {
        let cfg = AppConfig {
            db_connections: vec![core_lib::config::DbConnection {
                id: "ro".to_string(),
                name: "ro".to_string(),
                url: "mysql://root@127.0.0.1:1/test".to_string(),
                db_type: Some(DbType::MySQL),
                capability_level: None,
                schema: None,
                is_read_only: true,
            }],
            active_db_id: Some("ro".to_string()),
            ..Default::default()
        }
        .normalize();

        let mut state = test_state_with_config(cfg);
        state.limits.temp_dir = format!("/tmp/local-ai-sql-test-{}", uuid::Uuid::new_v4());

        let app = test_app(state);
        let payload = serde_json::json!({
            "steps": ["config", "sql_smoke"],
            "connection_ids": ["ro"],
            "operator": "tester"
        });

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/backend/tools/jobs/go-live/start")
                    .header("content-type", "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let job_id = v.get("job_id").and_then(|x| x.as_str()).unwrap_or("").to_string();
        assert!(!job_id.is_empty());

        let mut job: Option<serde_json::Value> = None;
        for _ in 0..300 {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("GET")
                        .uri(format!("/backend/tools/jobs/{}", job_id))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_eq!(resp.status(), StatusCode::OK);
            let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let status = v.get("status").and_then(|x| x.as_str()).unwrap_or("");
            if status == "completed" || status == "error" || status == "canceled" {
                job = Some(v);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let job = job.expect("job not finished");
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(format!("/backend/tools/jobs/{}/artifacts/data", job_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let report: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let steps = report
            .get("steps")
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default();

        let ro_sql_smoke = steps.iter().find(|s| {
            s.get("name").and_then(|x| x.as_str()) == Some("sql_smoke")
                && s.get("connection_id").and_then(|x| x.as_str()) == Some("ro")
        });
        let ro_sql_smoke = ro_sql_smoke.expect("missing sql_smoke step");
        assert_eq!(
            ro_sql_smoke.get("status").and_then(|x| x.as_str()),
            Some("skip")
        );
        assert_eq!(
            ro_sql_smoke
                .get("details")
                .and_then(|d| d.get("reason"))
                .and_then(|x| x.as_str()),
            Some("read_only")
        );

        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/backend/tools/go-live/reports?limit=10")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let arr = v.as_array().cloned().unwrap_or_default();
        assert!(!arr.is_empty());
        assert!(arr[0].get("job_id").is_some());
        assert!(arr[0].get("report_path").is_some());

        let _ = job;
    }

    #[tokio::test]
    async fn schema_sync_diff_returns_error_when_db_id_missing() {
        let app = test_app(test_state());

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/backend/tools/schema-sync/diff")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"source_db_id":"a","target_db_id":"b"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let s = String::from_utf8_lossy(&body);
        assert!(s.contains("Database connection a not found"));
    }

    #[tokio::test]
    async fn mysql_sync_compare_returns_error_when_db_id_missing() {
        let app = test_app(test_state());

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/backend/tools/mysql-sync/compare")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"source_db_id":"a","target_db_id":"b","table_name":"t","primary_key":"id","mode":"mirror","chunk_size":1000}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let s = String::from_utf8_lossy(&body);
        assert!(s.contains("Database connection a not found"));
    }

    #[tokio::test]
    async fn mysql_sync_job_status_returns_404_when_missing() {
        let app = test_app(test_state());

        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/backend/tools/mysql-sync/jobs/not-exist")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn perf_sync_start_returns_error_when_db_id_missing() {
        let app = test_app(test_state());

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/backend/tools/perf-sync/start")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"source_db_id":"a","target_db_id":"b"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let s = String::from_utf8_lossy(&body);
        assert!(s.contains("Database connection a not found"));
    }

    #[tokio::test]
    async fn perf_sync_check_returns_error_when_db_id_missing() {
        let app = test_app(test_state());

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/backend/tools/perf-sync/check")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"source_db_id":"a","target_db_id":"b"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let s = String::from_utf8_lossy(&body);
        assert!(s.contains("Database connection a not found"));
    }
}
