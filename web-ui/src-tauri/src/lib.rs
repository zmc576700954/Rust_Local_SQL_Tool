use core_lib::{
    config::{AppConfig, DbConnection, DbType},
    db::DbClient,
    perf_report::{summarize_perf_samples, PerfBudget, PerfProbeSummary, PerfSample},
    schema::{SchemaExtractor, SchemaResponse, TableWithDetails},
    sql_history::{SqlHistory, SqlHistoryStore},
    timeout_policy::TimeoutPolicy,
};
use serde::{Deserialize, Serialize};
use sqlx::{mysql::MySqlRow, Column, Row, TypeInfo};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, RwLock};

const DB_CLIENT_CACHE_TTL: Duration = Duration::from_secs(600);
const DESKTOP_HTTP_FALLBACK_PREFIX: &str = "DESKTOP_HTTP_FALLBACK:";
const PERF_PROBE_MAX_ITERATIONS: u32 = 30;
const QUERY_PREVIEW_CHUNK_SIZE: u32 = 200;
const QUERY_PREVIEW_ROW_CAP: u32 = 1000;

#[derive(Debug, Clone)]
struct CachedDbClient {
    client: DbClient,
    url: String,
    expires_at: Instant,
}

struct DesktopState {
    db_client_cache: Arc<RwLock<HashMap<String, CachedDbClient>>>,
    active_queries: Arc<RwLock<HashMap<String, ActiveQueryHandle>>>,
    transaction_sessions: Arc<RwLock<HashMap<String, SharedTransactionSession>>>,
}

impl Default for DesktopState {
    fn default() -> Self {
        Self {
            db_client_cache: Arc::new(RwLock::new(HashMap::new())),
            active_queries: Arc::new(RwLock::new(HashMap::new())),
            transaction_sessions: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

struct ResolvedConnection {
    cache_key: String,
    url: String,
}

#[derive(Debug, Deserialize)]
struct WorkbenchRunRequest {
    sql: String,
    force: Option<bool>,
    db_id: Option<String>,
    chunk_offset: Option<u32>,
    chunk_size: Option<u32>,
    cancel_token: Option<String>,
    transaction_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct WorkbenchRunResponse {
    columns: Vec<String>,
    rows: Vec<serde_json::Value>,
    row_count: usize,
    affected_rows: u64,
    execution_time_ms: u64,
    has_more: bool,
    next_offset: Option<u32>,
    chunk_offset: u32,
    chunk_size: Option<u32>,
    preview_cap: Option<u32>,
    truncated: bool,
}

#[derive(Debug, Deserialize)]
struct WorkbenchCancelRequest {
    cancel_token: String,
    db_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct WorkbenchCancelResponse {
    canceled: bool,
}

#[derive(Debug, Deserialize)]
struct WorkbenchTransactionRequest {
    action: String,
    transaction_id: String,
    db_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct WorkbenchTransactionResponse {
    action: String,
    transaction_id: String,
    state: String,
    execution_time_ms: u64,
}

#[derive(Clone)]
struct ActiveQueryHandle {
    db_client: DbClient,
    connection_id: u64,
    canceled: Arc<AtomicBool>,
}

struct ActiveQuerySession {
    token: String,
    connection_id: u64,
    canceled: Arc<AtomicBool>,
    owned_conn: Option<sqlx::pool::PoolConnection<sqlx::MySql>>,
    transaction_session: Option<SharedTransactionSession>,
}

struct TransactionSession {
    connection_id: u64,
    db_id: Option<String>,
    conn: sqlx::pool::PoolConnection<sqlx::MySql>,
}

type SharedTransactionSession = Arc<Mutex<TransactionSession>>;

#[derive(Clone, Copy)]
enum MySqlJsonDecodeStrategy {
    I64,
    F64,
    DateTime,
    Date,
    Time,
    String,
    Bytes,
    Unknown,
}

#[derive(Clone)]
struct MySqlRowJsonEncoder {
    columns: Vec<(String, usize, MySqlJsonDecodeStrategy)>,
}

impl MySqlRowJsonEncoder {
    fn from_row(row: &MySqlRow) -> Self {
        let columns = row
            .columns()
            .iter()
            .map(|col| {
                (
                    col.name().to_string(),
                    col.ordinal(),
                    mysql_json_decode_strategy(col.type_info().name()),
                )
            })
            .collect();
        Self { columns }
    }

    fn column_names(&self) -> Vec<String> {
        self.columns.iter().map(|(name, _, _)| name.clone()).collect()
    }
}

fn mysql_json_decode_strategy(type_name: &str) -> MySqlJsonDecodeStrategy {
    match type_name.to_ascii_lowercase().as_str() {
        "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint" | "year" => {
            MySqlJsonDecodeStrategy::I64
        }
        "float" | "double" | "decimal" | "numeric" | "real" => MySqlJsonDecodeStrategy::F64,
        "datetime" | "timestamp" => MySqlJsonDecodeStrategy::DateTime,
        "date" => MySqlJsonDecodeStrategy::Date,
        "time" => MySqlJsonDecodeStrategy::Time,
        "char" | "varchar" | "tinytext" | "text" | "mediumtext" | "longtext" | "enum"
        | "set" | "json" => MySqlJsonDecodeStrategy::String,
        "binary" | "varbinary" | "tinyblob" | "blob" | "mediumblob" | "longblob" | "bit" => {
            MySqlJsonDecodeStrategy::Bytes
        }
        _ => MySqlJsonDecodeStrategy::Unknown,
    }
}

fn fallback_mysql_json_value(row: &MySqlRow, ordinal: usize) -> serde_json::Value {
    if let Ok(val) = row.try_get::<Option<i64>, _>(ordinal) {
        serde_json::json!(val)
    } else if let Ok(val) = row.try_get::<Option<f64>, _>(ordinal) {
        serde_json::json!(val)
    } else if let Ok(val) = row.try_get::<Option<bool>, _>(ordinal) {
        serde_json::json!(val)
    } else if let Ok(val) = row.try_get::<Option<chrono::NaiveDateTime>, _>(ordinal) {
        serde_json::json!(val.map(|dt| dt.to_string()))
    } else if let Ok(val) = row.try_get::<Option<chrono::NaiveDate>, _>(ordinal) {
        serde_json::json!(val.map(|d| d.to_string()))
    } else if let Ok(val) = row.try_get::<Option<chrono::NaiveTime>, _>(ordinal) {
        serde_json::json!(val.map(|t| t.to_string()))
    } else if let Ok(val) = row.try_get::<Option<String>, _>(ordinal) {
        serde_json::json!(val)
    } else if let Ok(val) = row.try_get::<Option<Vec<u8>>, _>(ordinal) {
        serde_json::json!(val.map(|bytes| String::from_utf8_lossy(&bytes).into_owned()))
    } else {
        serde_json::Value::Null
    }
}

fn mysql_json_value_by_strategy(
    row: &MySqlRow,
    ordinal: usize,
    strategy: MySqlJsonDecodeStrategy,
) -> serde_json::Value {
    let encoded = match strategy {
        MySqlJsonDecodeStrategy::I64 => row
            .try_get::<Option<i64>, _>(ordinal)
            .ok()
            .map(|val| serde_json::json!(val)),
        MySqlJsonDecodeStrategy::F64 => row
            .try_get::<Option<f64>, _>(ordinal)
            .ok()
            .map(|val| serde_json::json!(val)),
        MySqlJsonDecodeStrategy::DateTime => row
            .try_get::<Option<chrono::NaiveDateTime>, _>(ordinal)
            .ok()
            .map(|val| serde_json::json!(val.map(|dt| dt.to_string()))),
        MySqlJsonDecodeStrategy::Date => row
            .try_get::<Option<chrono::NaiveDate>, _>(ordinal)
            .ok()
            .map(|val| serde_json::json!(val.map(|d| d.to_string()))),
        MySqlJsonDecodeStrategy::Time => row
            .try_get::<Option<chrono::NaiveTime>, _>(ordinal)
            .ok()
            .map(|val| serde_json::json!(val.map(|t| t.to_string()))),
        MySqlJsonDecodeStrategy::String => row
            .try_get::<Option<String>, _>(ordinal)
            .ok()
            .map(|val| serde_json::json!(val)),
        MySqlJsonDecodeStrategy::Bytes => row
            .try_get::<Option<Vec<u8>>, _>(ordinal)
            .ok()
            .map(|val| serde_json::json!(val.map(|bytes| String::from_utf8_lossy(&bytes).into_owned()))),
        MySqlJsonDecodeStrategy::Unknown => None,
    };

    encoded.unwrap_or_else(|| fallback_mysql_json_value(row, ordinal))
}

fn encode_mysql_row(row: &MySqlRow, encoder: &MySqlRowJsonEncoder) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for (col_name, ordinal, strategy) in &encoder.columns {
        map.insert(
            col_name.clone(),
            mysql_json_value_by_strategy(row, *ordinal, *strategy),
        );
    }
    serde_json::Value::Object(map)
}

#[derive(Debug, Deserialize)]
struct TablePageRequest {
    table_name: String,
    page: Option<u32>,
    page_size: Option<u32>,
    filters: Option<String>,
    orders: Option<String>,
    db_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct TablePageResponse {
    data: Vec<serde_json::Value>,
    total: Option<i64>,
    total_status: String,
    has_more: bool,
}

#[derive(Debug, Deserialize)]
struct TableSchemaRequest {
    table_name: String,
    db_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PerfProbeRequest {
    operation: Option<String>,
    db_id: Option<String>,
    sql: Option<String>,
    table_name: Option<String>,
    iterations: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct FilterCondition {
    column: String,
    operator: String,
    value: String,
}

#[derive(Debug, Deserialize)]
struct OrderCondition {
    column: String,
    desc: bool,
}

fn fallback_required(reason: &str) -> String {
    format!("{DESKTOP_HTTP_FALLBACK_PREFIX}{reason}")
}

fn quote_mysql_ident(raw: &str) -> Result<String, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err("Invalid identifier".to_string());
    }
    if s.len() > 512 {
        return Err("Identifier too long".to_string());
    }
    Ok(format!("`{}`", s.replace('`', "``")))
}

fn strip_leading_sql_comments(sql: &str) -> String {
    let mut clean_sql = sql.trim().to_string();
    loop {
        if clean_sql.starts_with("--") {
            if let Some(idx) = clean_sql.find('\n') {
                clean_sql = clean_sql[idx + 1..].trim().to_string();
            } else {
                return String::new();
            }
        } else if clean_sql.starts_with("/*") {
            if let Some(idx) = clean_sql.find("*/") {
                clean_sql = clean_sql[idx + 2..].trim().to_string();
            } else {
                return String::new();
            }
        } else {
            return clean_sql;
        }
    }
}

fn is_desktop_direct_supported(conn: &DbConnection) -> bool {
    let db_type = conn
        .db_type
        .clone()
        .unwrap_or_else(|| DbType::from_url(&conn.url));
    matches!(db_type, DbType::MySQL | DbType::MariaDB) && conn.ssh.is_none() && conn.ssl.is_none()
}

async fn resolve_connection(db_id: Option<&str>) -> Result<ResolvedConnection, String> {
    let config = AppConfig::load()
        .await
        .map_err(|e| format!("Failed to load local config: {e}"))?
        .normalize();

    if let Some(id) = db_id.or(config.active_db_id.as_deref()) {
        if let Some(conn) = config.db_connections.iter().find(|conn| conn.id == id) {
            if !is_desktop_direct_supported(conn) {
                return Err(fallback_required("unsupported_connection"));
            }
            return Ok(ResolvedConnection {
                cache_key: id.to_string(),
                url: conn.url.clone(),
            });
        }
        if db_id.is_some() {
            return Err("Database connection not found".to_string());
        }
    }

    let url = config
        .get_active_db_url()
        .ok_or_else(|| "Database not connected".to_string())?;

    if !matches!(DbType::from_url(&url), DbType::MySQL | DbType::MariaDB) {
        return Err(fallback_required("unsupported_connection"));
    }

    Ok(ResolvedConnection {
        cache_key: config
            .active_db_id
            .clone()
            .unwrap_or_else(|| "__active__".to_string()),
        url,
    })
}

async fn get_db_client(
    state: &DesktopState,
    db_id: Option<&str>,
) -> Result<(DbClient, String), String> {
    let resolved = resolve_connection(db_id).await?;

    {
        let cache = state.db_client_cache.read().await;
        if let Some(cached) = cache.get(&resolved.cache_key) {
            if cached.url == resolved.url && cached.expires_at > Instant::now() {
                return Ok((cached.client.clone(), resolved.url));
            }
        }
    }

    let client = DbClient::new(&resolved.url)
        .await
        .map_err(|e| format!("Failed to connect database: {e}"))?;

    let cached = CachedDbClient {
        client: client.clone(),
        url: resolved.url.clone(),
        expires_at: Instant::now() + DB_CLIENT_CACHE_TTL,
    };

    let mut cache = state.db_client_cache.write().await;
    cache.insert(resolved.cache_key, cached);

    Ok((client, resolved.url))
}

fn normalize_perf_probe_sql(raw: Option<&str>) -> Result<String, String> {
    let sql = raw.unwrap_or("SELECT 1 AS perf_probe").trim().to_string();
    if sql.is_empty() {
        return Err("Perf probe SQL cannot be empty".to_string());
    }

    let clean_sql = strip_leading_sql_comments(&sql);
    let upper_sql = clean_sql.to_uppercase();
    let is_read_only = upper_sql.starts_with("SELECT")
        || upper_sql.starts_with("SHOW")
        || upper_sql.starts_with("DESCRIBE")
        || upper_sql.starts_with("EXPLAIN");
    if !is_read_only {
        return Err("Perf probe only supports read-only SQL".to_string());
    }

    Ok(sql.trim_end_matches(';').to_string())
}

fn build_perf_probe_explain_sql(raw: Option<&str>) -> Result<String, String> {
    let sql = normalize_perf_probe_sql(raw)?;
    let clean_sql = strip_leading_sql_comments(&sql);
    if clean_sql.to_uppercase().starts_with("EXPLAIN") {
        return Ok(sql);
    }
    Ok(format!("EXPLAIN {sql}"))
}

fn normalize_perf_probe_iterations(raw: Option<u32>) -> u32 {
    raw.unwrap_or(5).clamp(1, PERF_PROBE_MAX_ITERATIONS)
}

fn normalize_perf_probe_table_name(raw: Option<&str>) -> Result<String, String> {
    let table_name = raw.unwrap_or("").trim();
    if table_name.is_empty() {
        return Err("Perf probe table_name is required".to_string());
    }
    Ok(table_name.to_string())
}

async fn get_fresh_db_client(db_id: Option<&str>) -> Result<(DbClient, String), String> {
    let resolved = resolve_connection(db_id).await?;
    let client = DbClient::new(&resolved.url)
        .await
        .map_err(|e| format!("Failed to connect database: {e}"))?;
    Ok((client, resolved.url))
}

fn default_perf_probe_budget(operation: &str) -> Option<PerfBudget> {
    match operation {
        "connect_warm" => Some(PerfBudget {
            operation: operation.to_string(),
            target_p50_ms: Some(50),
            target_p95_ms: Some(120),
            source: Some("phase1_desktop_warm_connect_target".to_string()),
        }),
        "query_select_small" => Some(PerfBudget {
            operation: operation.to_string(),
            target_p50_ms: Some(50),
            target_p95_ms: Some(120),
            source: Some("phase1_desktop_query_target".to_string()),
        }),
        "catalog_first_paint" => Some(PerfBudget {
            operation: operation.to_string(),
            target_p50_ms: Some(300),
            target_p95_ms: Some(700),
            source: Some("phase1_desktop_catalog_target".to_string()),
        }),
        "table_first_page" => Some(PerfBudget {
            operation: operation.to_string(),
            target_p50_ms: Some(80),
            target_p95_ms: Some(150),
            source: Some("phase1_desktop_table_first_page_target".to_string()),
        }),
        _ => None,
    }
}

async fn run_connect_warm_probe(
    state: &DesktopState,
    db_id: Option<&str>,
    iterations: u32,
) -> Result<PerfProbeSummary, String> {
    let (warm_client, _) = get_db_client(state, db_id).await?;
    warm_client
        .ping()
        .await
        .map_err(|e| format!("connect_warm warmup ping failed: {e}"))?;

    let mut samples = Vec::with_capacity(iterations as usize);
    for iteration in 0..iterations {
        let started_at = Instant::now();
        let (db_client, _) = get_db_client(state, db_id).await?;
        db_client
            .ping()
            .await
            .map_err(|e| format!("connect_warm ping failed: {e}"))?;
        samples.push(PerfSample {
            operation: "connect_warm".to_string(),
            iteration: iteration + 1,
            duration_ms: started_at.elapsed().as_millis(),
            rows: None,
        });
    }

    Ok(summarize_perf_samples(
        "connect_warm",
        samples,
        default_perf_probe_budget("connect_warm"),
    ))
}

async fn run_connect_cold_probe(
    db_id: Option<&str>,
    iterations: u32,
) -> Result<PerfProbeSummary, String> {
    let mut samples = Vec::with_capacity(iterations as usize);
    for iteration in 0..iterations {
        let started_at = Instant::now();
        let (db_client, _) = get_fresh_db_client(db_id).await?;
        db_client
            .ping()
            .await
            .map_err(|e| format!("connect_cold ping failed: {e}"))?;
        let duration_ms = started_at.elapsed().as_millis();
        db_client.pool.close().await;
        samples.push(PerfSample {
            operation: "connect_cold".to_string(),
            iteration: iteration + 1,
            duration_ms,
            rows: None,
        });
    }

    Ok(summarize_perf_samples(
        "connect_cold",
        samples,
        default_perf_probe_budget("connect_cold"),
    ))
}

async fn run_query_select_small_probe(
    state: &DesktopState,
    db_id: Option<&str>,
    sql: Option<&str>,
    iterations: u32,
) -> Result<PerfProbeSummary, String> {
    let sql = normalize_perf_probe_sql(sql)?;
    let (warm_client, _) = get_db_client(state, db_id).await?;
    sqlx::query(&sql)
        .fetch_all(&warm_client.pool)
        .await
        .map_err(|e| format!("query_select_small warmup failed: {e}"))?;

    let mut samples = Vec::with_capacity(iterations as usize);
    for iteration in 0..iterations {
        let (db_client, _) = get_db_client(state, db_id).await?;
        let started_at = Instant::now();
        let rows = sqlx::query(&sql)
            .fetch_all(&db_client.pool)
            .await
            .map_err(|e| format!("query_select_small failed: {e}"))?;
        samples.push(PerfSample {
            operation: "query_select_small".to_string(),
            iteration: iteration + 1,
            duration_ms: started_at.elapsed().as_millis(),
            rows: Some(rows.len() as u64),
        });
    }

    Ok(summarize_perf_samples(
        "query_select_small",
        samples,
        default_perf_probe_budget("query_select_small"),
    ))
}

async fn run_query_write_small_probe(
    state: &DesktopState,
    db_id: Option<&str>,
    iterations: u32,
) -> Result<PerfProbeSummary, String> {
    let (db_client, _) = get_db_client(state, db_id).await?;
    let mut conn = db_client.pool.acquire().await.map_err(|e| e.to_string())?;
    let temp_table = "__perf_probe_write_small";
    let drop_sql = format!("DROP TEMPORARY TABLE IF EXISTS {temp_table}");
    let create_sql = format!(
        "CREATE TEMPORARY TABLE {temp_table} (id BIGINT PRIMARY KEY AUTO_INCREMENT, marker VARCHAR(64) NOT NULL)"
    );
    let insert_sql = format!("INSERT INTO {temp_table} (marker) VALUES (?)");

    sqlx::query(&drop_sql)
        .execute(&mut *conn)
        .await
        .map_err(|e| format!("query_write_small temp table cleanup failed: {e}"))?;
    sqlx::query(&create_sql)
        .execute(&mut *conn)
        .await
        .map_err(|e| format!("query_write_small temp table create failed: {e}"))?;
    sqlx::query(&insert_sql)
        .bind("warmup")
        .execute(&mut *conn)
        .await
        .map_err(|e| format!("query_write_small warmup failed: {e}"))?;

    let mut samples = Vec::with_capacity(iterations as usize);
    for iteration in 0..iterations {
        let started_at = Instant::now();
        let result = sqlx::query(&insert_sql)
            .bind(format!("probe-{}", iteration + 1))
            .execute(&mut *conn)
            .await
            .map_err(|e| format!("query_write_small failed: {e}"))?;
        samples.push(PerfSample {
            operation: "query_write_small".to_string(),
            iteration: iteration + 1,
            duration_ms: started_at.elapsed().as_millis(),
            rows: Some(result.rows_affected()),
        });
    }

    let _ = sqlx::query(&drop_sql).execute(&mut *conn).await;

    Ok(summarize_perf_samples(
        "query_write_small",
        samples,
        default_perf_probe_budget("query_write_small"),
    ))
}

async fn run_explain_plan_probe(
    state: &DesktopState,
    db_id: Option<&str>,
    sql: Option<&str>,
    iterations: u32,
) -> Result<PerfProbeSummary, String> {
    let explain_sql = build_perf_probe_explain_sql(sql)?;
    let (warm_client, _) = get_db_client(state, db_id).await?;
    sqlx::query(&explain_sql)
        .fetch_all(&warm_client.pool)
        .await
        .map_err(|e| format!("explain_plan warmup failed: {e}"))?;

    let mut samples = Vec::with_capacity(iterations as usize);
    for iteration in 0..iterations {
        let (db_client, _) = get_db_client(state, db_id).await?;
        let started_at = Instant::now();
        let rows = sqlx::query(&explain_sql)
            .fetch_all(&db_client.pool)
            .await
            .map_err(|e| format!("explain_plan failed: {e}"))?;
        samples.push(PerfSample {
            operation: "explain_plan".to_string(),
            iteration: iteration + 1,
            duration_ms: started_at.elapsed().as_millis(),
            rows: Some(rows.len() as u64),
        });
    }

    Ok(summarize_perf_samples(
        "explain_plan",
        samples,
        default_perf_probe_budget("explain_plan"),
    ))
}

async fn fetch_schema_for_perf(
    db_client: &DbClient,
    db_name: &str,
) -> Result<SchemaResponse, String> {
    let tables = SchemaExtractor::get_tables(db_client, db_name)
        .await
        .map_err(|e| e.to_string())?;
    let columns_map = SchemaExtractor::get_columns_map(db_client, db_name)
        .await
        .map_err(|e| e.to_string())?;
    let indexes_map = SchemaExtractor::get_indexes_map(db_client, db_name)
        .await
        .map_err(|e| e.to_string())?;
    let foreign_keys_map = SchemaExtractor::get_foreign_keys_map(db_client, db_name)
        .await
        .map_err(|e| e.to_string())?;
    let views = SchemaExtractor::get_views(db_client, db_name)
        .await
        .map_err(|e| e.to_string())?;

    let mut result_tables = Vec::with_capacity(tables.len());
    for table in tables {
        let table_name = table.table_name;
        result_tables.push(TableWithDetails {
            columns: columns_map.get(&table_name).cloned().unwrap_or_default(),
            indexes: indexes_map.get(&table_name).cloned().unwrap_or_default(),
            foreign_keys: foreign_keys_map
                .get(&table_name)
                .cloned()
                .unwrap_or_default(),
            table_name,
        });
    }

    Ok(SchemaResponse {
        db_name: db_name.to_string(),
        tables: result_tables,
        views,
    })
}

async fn run_catalog_first_paint_probe(
    state: &DesktopState,
    db_id: Option<&str>,
    iterations: u32,
) -> Result<PerfProbeSummary, String> {
    let mut samples = Vec::with_capacity(iterations as usize);
    for iteration in 0..iterations {
        let (db_client, url) = get_db_client(state, db_id).await?;
        let db_name = DbClient::extract_db_name(&url)
            .ok_or_else(|| "Unable to determine database name from connection URL".to_string())?;
        let started_at = Instant::now();
        let schema = fetch_schema_for_perf(&db_client, &db_name).await?;
        samples.push(PerfSample {
            operation: "catalog_first_paint".to_string(),
            iteration: iteration + 1,
            duration_ms: started_at.elapsed().as_millis(),
            rows: Some((schema.tables.len() + schema.views.len()) as u64),
        });
    }

    Ok(summarize_perf_samples(
        "catalog_first_paint",
        samples,
        default_perf_probe_budget("catalog_first_paint"),
    ))
}

async fn run_table_first_page_probe(
    state: &DesktopState,
    db_id: Option<&str>,
    table_name: Option<&str>,
    iterations: u32,
) -> Result<PerfProbeSummary, String> {
    let table_name = normalize_perf_probe_table_name(table_name)?;
    let table_ident = quote_mysql_ident(&table_name)?;
    let data_sql = format!("SELECT * FROM {table_ident} LIMIT 101 OFFSET 0");
    let policy = TimeoutPolicy::default();
    let mut samples = Vec::with_capacity(iterations as usize);

    for iteration in 0..iterations {
        let (db_client, url) = get_db_client(state, db_id).await?;
        let db_name = DbClient::extract_db_name(&url)
            .ok_or_else(|| "Unable to determine database name from connection URL".to_string())?;
        let started_at = Instant::now();

        let _table_schema = tokio::try_join!(
            SchemaExtractor::get_columns(&db_client, &db_name, &table_name),
            SchemaExtractor::get_indexes(&db_client, &db_name, &table_name),
            SchemaExtractor::get_foreign_keys(&db_client, &db_name, &table_name),
        )
        .map_err(|e| e.to_string())?;

        let result_rows = tokio::time::timeout(policy.db_query, sqlx::query(&data_sql).fetch_all(&db_client.pool))
            .await
            .map_err(|_| "Query timed out after 30 seconds. Please optimize SQL or add indexes.".to_string())?
            .map_err(|e| e.to_string())?;

        let mut row_encoder = None;
        let data: Vec<serde_json::Value> = result_rows
            .into_iter()
            .take(100)
            .map(|row| {
                if row_encoder.is_none() {
                    row_encoder = Some(MySqlRowJsonEncoder::from_row(&row));
                }
                encode_mysql_row(
                    &row,
                    row_encoder
                        .as_ref()
                        .expect("row encoder should be initialized"),
                )
            })
            .collect();

        samples.push(PerfSample {
            operation: "table_first_page".to_string(),
            iteration: iteration + 1,
            duration_ms: started_at.elapsed().as_millis(),
            rows: Some(data.len() as u64),
        });
    }

    Ok(summarize_perf_samples(
        "table_first_page",
        samples,
        default_perf_probe_budget("table_first_page"),
    ))
}

async fn run_cancel_latency_probe(
    state: &DesktopState,
    db_id: Option<&str>,
    iterations: u32,
) -> Result<PerfProbeSummary, String> {
    let mut samples = Vec::with_capacity(iterations as usize);

    for iteration in 0..iterations {
        let (db_client, _) = get_db_client(state, db_id).await?;
        let mut conn = db_client.pool.acquire().await.map_err(|e| e.to_string())?;
        let connection_id = DbClient::connection_id_for_session(&mut conn)
            .await
            .map_err(|e| e.to_string())?;
        let canceled = Arc::new(AtomicBool::new(false));
        let cancel_token = format!(
            "perf_probe_cancel_{}_{}",
            iteration + 1,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_millis())
                .unwrap_or(0)
        );
        register_active_query(
            state,
            cancel_token.clone(),
            ActiveQueryHandle {
                db_client: db_client.clone(),
                connection_id,
                canceled,
            },
        )
        .await;

        let query_task = tokio::spawn(async move {
            sqlx::query("SELECT SLEEP(2) AS perf_probe_cancel")
                .fetch_all(&mut *conn)
                .await
        });

        tokio::time::sleep(Duration::from_millis(100)).await;
        let started_at = Instant::now();
        let canceled_ok = cancel_active_query(state, &cancel_token).await?;
        let join_result = tokio::time::timeout(Duration::from_secs(5), query_task)
            .await
            .map_err(|_| "cancel_latency probe join timed out".to_string())?;
        unregister_active_query(state, &cancel_token).await;
        join_result
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;

        if !canceled_ok {
            return Err("cancel_latency probe could not cancel active query".to_string());
        }

        samples.push(PerfSample {
            operation: "cancel_latency".to_string(),
            iteration: iteration + 1,
            duration_ms: started_at.elapsed().as_millis(),
            rows: None,
        });
    }

    Ok(summarize_perf_samples(
        "cancel_latency",
        samples,
        default_perf_probe_budget("cancel_latency"),
    ))
}

async fn register_active_query(state: &DesktopState, token: String, handle: ActiveQueryHandle) {
    state.active_queries.write().await.insert(token, handle);
}

async fn unregister_active_query(state: &DesktopState, token: &str) {
    state.active_queries.write().await.remove(token);
}

async fn cancel_active_query(state: &DesktopState, cancel_token: &str) -> Result<bool, String> {
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
                Err(e.to_string())
            }
        }
    }
}

async fn resolve_transaction_db_id(db_id: Option<&str>) -> Result<Option<String>, String> {
    if let Some(value) = db_id.map(str::trim).filter(|value| !value.is_empty()) {
        return Ok(Some(value.to_string()));
    }
    let config = AppConfig::load()
        .await
        .map_err(|e| format!("Failed to load local config: {e}"))?
        .normalize();
    Ok(config.active_db_id)
}

async fn get_or_open_transaction_session(
    state: &DesktopState,
    db_id: Option<&str>,
    transaction_id: &str,
) -> Result<SharedTransactionSession, String> {
    if let Some(existing) = state
        .transaction_sessions
        .read()
        .await
        .get(transaction_id)
        .cloned()
    {
        let expected_db_id = resolve_transaction_db_id(db_id).await?;
        let session_db_id = existing.lock().await.db_id.clone();
        if session_db_id != expected_db_id {
            return Err("Transaction session is bound to a different database connection".to_string());
        }
        return Ok(existing);
    }

    let resolved_db_id = resolve_transaction_db_id(db_id).await?;
    let (db_client, _) = get_db_client(state, db_id).await?;
    let mut conn = db_client
        .pool
        .acquire()
        .await
        .map_err(|e| format!("Failed to acquire connection: {e}"))?;
    let connection_id = DbClient::connection_id_for_session(&mut conn)
        .await
        .map_err(|e| format!("Failed to resolve connection id: {e}"))?;
    sqlx::query("START TRANSACTION")
        .execute(&mut *conn)
        .await
        .map_err(|e| format!("Failed to start transaction: {e}"))?;

    let session = Arc::new(Mutex::new(TransactionSession {
        connection_id,
        db_id: resolved_db_id,
        conn,
    }));
    state
        .transaction_sessions
        .write()
        .await
        .insert(transaction_id.to_string(), session.clone());
    Ok(session)
}

async fn append_sql_history(sql: String, status: &str, execution_time_ms: u64) {
    let Ok(mut store) = SqlHistoryStore::load().await else {
        return;
    };
    store.add_history(SqlHistory {
        id: String::new(),
        sql,
        status: status.to_string(),
        execution_time_ms,
        executed_at: 0,
        db_id: None,
        row_count: None,
        affected_rows: None,
        statement_kind: None,
    });
    let _ = store.save().await;
}

#[tauri::command]
async fn workbench_run(
    state: tauri::State<'_, DesktopState>,
    request: WorkbenchRunRequest,
) -> Result<WorkbenchRunResponse, String> {
    let _ = request.force;
    let clean_sql = strip_leading_sql_comments(&request.sql);
    let upper_sql = clean_sql.to_uppercase();
    let is_select = upper_sql.starts_with("SELECT")
        || upper_sql.starts_with("SHOW")
        || upper_sql.starts_with("DESCRIBE")
        || upper_sql.starts_with("EXPLAIN");

    let transaction_id = request
        .transaction_id
        .as_deref()
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_string);
    if transaction_id.is_none() && !is_select {
        return Err(fallback_required("write_or_ddl"));
    }

    let (db_client, _) = get_db_client(&state, request.db_id.as_deref()).await?;
    let mut sql = request.sql.trim().trim_end_matches(';').to_string();
    let chunk_offset = request.chunk_offset.unwrap_or(0);
    let mut chunk_size = None;
    let mut preview_cap = None;
    let mut has_more = false;
    let mut next_offset = None;
    let mut truncated = false;
    let is_chunked_preview = is_select && upper_sql.starts_with("SELECT") && !upper_sql.contains("LIMIT");
    if is_chunked_preview {
        let requested_chunk_size = request
            .chunk_size
            .unwrap_or(QUERY_PREVIEW_CHUNK_SIZE)
            .clamp(1, QUERY_PREVIEW_CHUNK_SIZE);
        let remaining = QUERY_PREVIEW_ROW_CAP.saturating_sub(chunk_offset);
        let effective_chunk_size = requested_chunk_size.min(remaining.max(1));
        sql.push_str(&format!(
            " LIMIT {} OFFSET {}",
            effective_chunk_size + 1,
            chunk_offset
        ));
        chunk_size = Some(requested_chunk_size);
        preview_cap = Some(QUERY_PREVIEW_ROW_CAP);
    } else if is_select && !upper_sql.contains("LIMIT") {
        sql.push_str(" LIMIT 1000");
    }

    let transaction_session = if let Some(id) = transaction_id.as_deref() {
        Some(get_or_open_transaction_session(&state, request.db_id.as_deref(), id).await?)
    } else {
        None
    };
    let cancel_token = request
        .cancel_token
        .as_deref()
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_string);
    let mut active_query = if let Some(token) = cancel_token {
        let canceled = Arc::new(AtomicBool::new(false));
        if let Some(transaction_session) = transaction_session.clone() {
            let connection_id = transaction_session.lock().await.connection_id;
            register_active_query(
                &state,
                token.clone(),
                ActiveQueryHandle {
                    db_client: db_client.clone(),
                    connection_id,
                    canceled: canceled.clone(),
                },
            )
            .await;
            Some(ActiveQuerySession {
                token,
                connection_id,
                canceled,
                owned_conn: None,
                transaction_session: Some(transaction_session),
            })
        } else {
            let mut conn = db_client
                .pool
                .acquire()
                .await
                .map_err(|e| format!("Failed to acquire connection: {e}"))?;
            let connection_id = DbClient::connection_id_for_session(&mut conn)
                .await
                .map_err(|e| format!("Failed to resolve connection id: {e}"))?;
            register_active_query(
                &state,
                token.clone(),
                ActiveQueryHandle {
                    db_client: db_client.clone(),
                    connection_id,
                    canceled: canceled.clone(),
                },
            )
            .await;
            Some(ActiveQuerySession {
                token,
                connection_id,
                canceled,
                owned_conn: Some(conn),
                transaction_session: None,
            })
        }
    } else {
        None
    };

    let start_time = Instant::now();
    let policy = TimeoutPolicy::default();
    let mut rows = Vec::new();
    let mut columns = Vec::new();
    let mut affected_rows = 0;
    let mut status = "success".to_string();
    let mut err_msg = None;

    if is_select {
        let result = match tokio::time::timeout(policy.db_query, async {
            if let Some(active_query) = active_query.as_mut() {
                if let Some(transaction_session) = active_query.transaction_session.as_ref() {
                    let mut session = transaction_session.lock().await;
                    sqlx::query(&sql).fetch_all(&mut *session.conn).await
                } else if let Some(conn) = active_query.owned_conn.as_mut() {
                    sqlx::query(&sql).fetch_all(&mut **conn).await
                } else {
                    sqlx::query(&sql).fetch_all(&db_client.pool).await
                }
            } else if let Some(transaction_session) = transaction_session.as_ref() {
                let mut session = transaction_session.lock().await;
                sqlx::query(&sql).fetch_all(&mut *session.conn).await
            } else {
                sqlx::query(&sql).fetch_all(&db_client.pool).await
            }
        })
        .await
        {
            Ok(result) => result.map_err(|e| e.to_string()),
            Err(_) => {
                if let Some(active_query) = active_query.as_ref() {
                    let _ = db_client.kill_query(active_query.connection_id).await;
                    unregister_active_query(&state, &active_query.token).await;
                }
                return Err("Query timed out after 30 seconds. Please optimize SQL or add indexes.".to_string());
            }
        };

        match result {
            Ok(result_rows) => {
                let mut row_encoder = None;
                let chunk_limit = if is_chunked_preview {
                    chunk_size.unwrap_or(QUERY_PREVIEW_CHUNK_SIZE)
                } else {
                    result_rows.len() as u32
                };
                let fetched_len = result_rows.len() as u32;
                if is_chunked_preview {
                    has_more = fetched_len > chunk_limit
                        && chunk_offset.saturating_add(chunk_limit) < QUERY_PREVIEW_ROW_CAP;
                    next_offset = has_more.then_some(chunk_offset.saturating_add(chunk_limit));
                    truncated = fetched_len > chunk_limit;
                }
                rows = result_rows
                    .into_iter()
                    .take(chunk_limit as usize)
                    .map(|row| {
                        if row_encoder.is_none() {
                            let encoder = MySqlRowJsonEncoder::from_row(&row);
                            columns = encoder.column_names();
                            row_encoder = Some(encoder);
                        }
                        encode_mysql_row(
                            &row,
                            row_encoder
                                .as_ref()
                                .expect("row encoder should be initialized"),
                        )
                    })
                    .collect();
            }
            Err(error) => {
                let was_canceled = active_query
                    .as_ref()
                    .map(|query| query.canceled.load(Ordering::SeqCst))
                    .unwrap_or(false);
                status = if was_canceled {
                    "canceled".to_string()
                } else {
                    "error".to_string()
                };
                err_msg = Some(if was_canceled {
                    "Query canceled".to_string()
                } else {
                    error
                });
            }
        }
    } else {
        let result = match tokio::time::timeout(policy.db_query, async {
            if let Some(active_query) = active_query.as_mut() {
                if let Some(transaction_session) = active_query.transaction_session.as_ref() {
                    let mut session = transaction_session.lock().await;
                    sqlx::query(&sql).execute(&mut *session.conn).await
                } else if let Some(conn) = active_query.owned_conn.as_mut() {
                    sqlx::query(&sql).execute(&mut **conn).await
                } else {
                    sqlx::query(&sql).execute(&db_client.pool).await
                }
            } else if let Some(transaction_session) = transaction_session.as_ref() {
                let mut session = transaction_session.lock().await;
                sqlx::query(&sql).execute(&mut *session.conn).await
            } else {
                sqlx::query(&sql).execute(&db_client.pool).await
            }
        })
        .await
        {
            Ok(result) => result.map_err(|e| e.to_string()),
            Err(_) => {
                if let Some(active_query) = active_query.as_ref() {
                    let _ = db_client.kill_query(active_query.connection_id).await;
                    unregister_active_query(&state, &active_query.token).await;
                }
                return Err("Query timed out after 30 seconds. Please optimize SQL or add indexes.".to_string());
            }
        };

        match result {
            Ok(result) => {
                affected_rows = result.rows_affected();
            }
            Err(error) => {
                let was_canceled = active_query
                    .as_ref()
                    .map(|query| query.canceled.load(Ordering::SeqCst))
                    .unwrap_or(false);
                status = if was_canceled {
                    "canceled".to_string()
                } else {
                    "error".to_string()
                };
                err_msg = Some(if was_canceled {
                    "Query canceled".to_string()
                } else {
                    error
                });
            }
        }
    }

    if let Some(active_query) = active_query.as_ref() {
        unregister_active_query(&state, &active_query.token).await;
    }
    let was_canceled = active_query
        .as_ref()
        .map(|query| query.canceled.load(Ordering::SeqCst))
        .unwrap_or(false);

    let elapsed = start_time.elapsed().as_millis() as u64;

    if let Some(error) = err_msg {
        if was_canceled {
            append_sql_history(sql, "canceled", elapsed).await;
            Err("ERR_CANCELED: Query canceled".to_string())
        } else {
            append_sql_history(sql, "error", elapsed).await;
            Err(error)
        }
    } else {
        append_sql_history(sql, &status, elapsed).await;
        Ok(WorkbenchRunResponse {
            columns,
            row_count: rows.len(),
            rows,
            affected_rows,
            execution_time_ms: elapsed,
            has_more,
            next_offset,
            chunk_offset,
            chunk_size,
            preview_cap,
            truncated,
        })
    }
}

#[tauri::command]
async fn workbench_cancel(
    state: tauri::State<'_, DesktopState>,
    request: WorkbenchCancelRequest,
) -> Result<WorkbenchCancelResponse, String> {
    let _ = request.db_id;
    let cancel_token = request.cancel_token.trim();
    if cancel_token.is_empty() {
        return Ok(WorkbenchCancelResponse { canceled: false });
    }

    let canceled = cancel_active_query(&state, cancel_token).await?;
    Ok(WorkbenchCancelResponse { canceled })
}

#[tauri::command]
async fn workbench_transaction(
    state: tauri::State<'_, DesktopState>,
    request: WorkbenchTransactionRequest,
) -> Result<WorkbenchTransactionResponse, String> {
    let transaction_id = request.transaction_id.trim();
    if transaction_id.is_empty() {
        return Err("transaction_id is required".to_string());
    }

    let action = request.action.trim().to_lowercase();
    if action != "commit" && action != "rollback" {
        return Err("transaction action must be commit or rollback".to_string());
    }

    let session = state
        .transaction_sessions
        .read()
        .await
        .get(transaction_id)
        .cloned()
        .ok_or_else(|| "transaction session not found".to_string())?;
    let expected_db_id = resolve_transaction_db_id(request.db_id.as_deref()).await?;
    {
        let guard = session.lock().await;
        if guard.db_id != expected_db_id {
            return Err("Transaction session is bound to a different database connection".to_string());
        }
    }

    let started_at = Instant::now();
    {
        let mut guard = session.lock().await;
        let sql = if action == "commit" { "COMMIT" } else { "ROLLBACK" };
        sqlx::query(sql)
            .execute(&mut *guard.conn)
            .await
            .map_err(|e| format!("Failed to {action} transaction: {e}"))?;
    }
    state.transaction_sessions.write().await.remove(transaction_id);

    Ok(WorkbenchTransactionResponse {
        action,
        transaction_id: transaction_id.to_string(),
        state: "idle".to_string(),
        execution_time_ms: started_at.elapsed().as_millis() as u64,
    })
}

#[tauri::command]
async fn table_page(
    state: tauri::State<'_, DesktopState>,
    request: TablePageRequest,
) -> Result<TablePageResponse, String> {
    let (db_client, _) = get_db_client(&state, request.db_id.as_deref()).await?;

    let page = request.page.unwrap_or(1);
    let page_size = request.page_size.unwrap_or(100);
    let offset = (page - 1) * page_size;

    let mut where_clause = String::new();
    let mut bindings = Vec::new();

    if let Some(filters_str) = &request.filters {
        if let Ok(filters) = serde_json::from_str::<Vec<FilterCondition>>(filters_str) {
            let mut conditions = Vec::new();
            for filter in filters {
                let op = match filter.operator.as_str() {
                    "equals" => "=",
                    "contains" => "LIKE",
                    "greater_than" => ">",
                    "less_than" => "<",
                    _ => "=",
                };
                let col = quote_mysql_ident(&filter.column)?;
                conditions.push(format!("{col} {op} ?"));
                if filter.operator == "contains" {
                    bindings.push(format!("%{}%", filter.value));
                } else {
                    bindings.push(filter.value);
                }
            }
            if !conditions.is_empty() {
                where_clause = format!("WHERE {}", conditions.join(" AND "));
            }
        }
    }

    let mut order_clause = String::new();
    if let Some(orders_str) = &request.orders {
        if let Ok(orders) = serde_json::from_str::<Vec<OrderCondition>>(orders_str) {
            let mut clauses = Vec::new();
            for order in orders {
                let dir = if order.desc { "DESC" } else { "ASC" };
                let col = quote_mysql_ident(&order.column)?;
                clauses.push(format!("{col} {dir}"));
            }
            if !clauses.is_empty() {
                order_clause = format!("ORDER BY {}", clauses.join(", "));
            }
        }
    }

    let table_ident = quote_mysql_ident(&request.table_name)?;
    let data_sql = format!(
        "SELECT * FROM {table_ident} {where_clause} {order_clause} LIMIT {} OFFSET {}",
        page_size + 1,
        offset
    );
    let mut query = sqlx::query(&data_sql);
    for binding in &bindings {
        query = query.bind(binding);
    }

    let policy = TimeoutPolicy::default();
    let result_rows = tokio::time::timeout(policy.db_query, query.fetch_all(&db_client.pool))
        .await
        .map_err(|_| "Query timed out after 30 seconds. Please optimize SQL or add indexes.".to_string())?
        .map_err(|e| e.to_string())?;

    let has_more = result_rows.len() as u32 > page_size;
    let mut row_encoder = None;
    let data = result_rows
        .into_iter()
        .take(page_size as usize)
        .map(|row| {
            if row_encoder.is_none() {
                row_encoder = Some(MySqlRowJsonEncoder::from_row(&row));
            }
            encode_mysql_row(
                &row,
                row_encoder
                    .as_ref()
                    .expect("row encoder should be initialized"),
            )
        })
        .collect();

    Ok(TablePageResponse {
        data,
        total: None,
        total_status: "calculating".to_string(),
        has_more,
    })
}

#[tauri::command]
async fn table_schema(
    state: tauri::State<'_, DesktopState>,
    request: TableSchemaRequest,
) -> Result<TableWithDetails, String> {
    let (db_client, url) = get_db_client(&state, request.db_id.as_deref()).await?;
    let db_name = DbClient::extract_db_name(&url)
        .ok_or_else(|| "Unable to determine database name from connection URL".to_string())?;

    let (columns, indexes, foreign_keys) = tokio::try_join!(
        SchemaExtractor::get_columns(&db_client, &db_name, &request.table_name),
        SchemaExtractor::get_indexes(&db_client, &db_name, &request.table_name),
        SchemaExtractor::get_foreign_keys(&db_client, &db_name, &request.table_name),
    )
    .map_err(|e| e.to_string())?;

    Ok(TableWithDetails {
        table_name: request.table_name,
        columns,
        indexes,
        foreign_keys,
    })
}

#[tauri::command]
async fn perf_probe(
    state: tauri::State<'_, DesktopState>,
    request: PerfProbeRequest,
) -> Result<PerfProbeSummary, String> {
    let operation = request
        .operation
        .clone()
        .unwrap_or_else(|| "connect_warm".to_string())
        .trim()
        .to_lowercase();
    let iterations = normalize_perf_probe_iterations(request.iterations);

    match operation.as_str() {
        "connect_cold" => run_connect_cold_probe(request.db_id.as_deref(), iterations).await,
        "connect_warm" => run_connect_warm_probe(&state, request.db_id.as_deref(), iterations).await,
        "query_select_small" => {
            run_query_select_small_probe(&state, request.db_id.as_deref(), request.sql.as_deref(), iterations)
                .await
        }
        "query_write_small" => {
            run_query_write_small_probe(&state, request.db_id.as_deref(), iterations).await
        }
        "explain_plan" => {
            run_explain_plan_probe(&state, request.db_id.as_deref(), request.sql.as_deref(), iterations)
                .await
        }
        "catalog_first_paint" => {
            run_catalog_first_paint_probe(&state, request.db_id.as_deref(), iterations).await
        }
        "table_first_page" => {
            run_table_first_page_probe(
                &state,
                request.db_id.as_deref(),
                request.table_name.as_deref(),
                iterations,
            )
            .await
        }
        "cancel_latency" => run_cancel_latency_probe(&state, request.db_id.as_deref(), iterations).await,
        _ => Err(format!(
            "Unsupported perf probe operation: {}",
            operation
        )),
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(DesktopState::default())
        .invoke_handler(tauri::generate_handler![
            workbench_run,
            workbench_cancel,
            workbench_transaction,
            table_page,
            table_schema,
            perf_probe
        ])
        .setup(|app| {
            if cfg!(debug_assertions) {
                app.handle().plugin(
                    tauri_plugin_log::Builder::default()
                        .level(log::LevelFilter::Info)
                        .build(),
                )?;
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
