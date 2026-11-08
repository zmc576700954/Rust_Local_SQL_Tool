//! Performance diagnostics handlers — probes, suites, and benchmarks.

use axum::{
    extract::{Path, Query, State},
    Json,
};
use core_lib::{
    config::DbConnection,
    db::{open_db_client, DbClient},
    error::AppError,
    perf_report::{PerfProbeSummary, PerfBudget, PerfSample, summarize_perf_samples},
};
use serde::{Deserialize, Serialize};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use crate::state::*;
use crate::mysql_codec::*;
use crate::resolve_db_client_for_request;
use crate::handlers::go_live::{LimitQuery, read_jsonl_recent, safe_ident_suffix};
use crate::handlers::util::require_mysql_connection;
use crate::{
    PERF_SUITE_ARCHIVE_DEFAULT_LIMIT, PERF_PROBE_MAX_ITERATIONS,
    QUERY_PREVIEW_CHUNK_SIZE, QUERY_PREVIEW_ROW_CAP,
    clear_metadata_caches, get_cached_table_schema,
    is_read_only_connection, get_or_open_transaction_session,
    ExecuteRequest, ExecuteResponse,
};
use core_lib::sql_history::SqlHistory;
use tokio::io::AsyncWriteExt;

#[derive(Debug, Deserialize)]
pub(crate) struct PerfProbeRequest {
    operation: Option<String>,
    db_id: Option<String>,
    sql: Option<String>,
    table_name: Option<String>,
    iterations: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PerfSuiteHistoryRecord {
    id: String,
    recorded_at: String,
    connection_id: Option<String>,
    connection_name: Option<String>,
    operation: String,
    iterations: u32,
    sql: Option<String>,
    table_name: Option<String>,
    result: PerfProbeSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PerfSuiteArchiveRecord {
    id: String,
    recorded_at: String,
    connection_id: Option<String>,
    connection_name: Option<String>,
    label: Option<String>,
    build_version: Option<String>,
    branch_name: Option<String>,
    environment: Option<String>,
    notes: Option<String>,
    iterations: u32,
    sql: Option<String>,
    table_name: Option<String>,
    status: String,
    failed_operation: Option<String>,
    error: Option<String>,
    results: Vec<PerfSuiteHistoryRecord>,
    #[serde(default)]
    archive_path: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct PerfSuiteBaselinePinRequest {
    suite_id: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct PerfSuiteDiffListQuery {
    limit: Option<usize>,
    current_suite_id: Option<String>,
    baseline_suite_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PerfSuiteDiffArchiveRecord {
    id: String,
    recorded_at: String,
    current_suite_id: String,
    baseline_suite_id: String,
    current_suite_label: Option<String>,
    baseline_suite_label: Option<String>,
    gate_status: Option<String>,
    baseline_scope: Option<String>,
    current_suite: serde_json::Value,
    baseline_suite: serde_json::Value,
    gate: serde_json::Value,
    summary: serde_json::Value,
    rows: Vec<serde_json::Value>,
    #[serde(default)]
    archive_path: Option<String>,
}

pub fn normalize_perf_probe_sql(raw: Option<&str>) -> Result<String, AppError> {
    let sql = raw.unwrap_or("SELECT 1 AS perf_probe").trim().to_string();
    if sql.is_empty() {
        return Err(AppError::BadRequest(
            "Perf probe SQL cannot be empty".to_string(),
        ));
    }

    let clean_sql = strip_leading_perf_probe_sql_comments(&sql);
    let upper_sql = clean_sql.to_uppercase();
    let is_read_only = upper_sql.starts_with("SELECT")
        || upper_sql.starts_with("SHOW")
        || upper_sql.starts_with("DESCRIBE")
        || upper_sql.starts_with("EXPLAIN");
    if !is_read_only {
        return Err(AppError::BadRequest(
            "Perf probe only supports read-only SQL".to_string(),
        ));
    }

    Ok(sql.trim_end_matches(';').to_string())
}

pub fn strip_leading_perf_probe_sql_comments(sql: &str) -> String {
    let mut clean_sql = sql.trim().to_string();
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
            return clean_sql;
        }
    }
}

pub fn build_perf_probe_explain_sql(raw: Option<&str>) -> Result<String, AppError> {
    let sql = normalize_perf_probe_sql(raw)?;
    let clean_sql = strip_leading_perf_probe_sql_comments(&sql);
    if clean_sql.to_uppercase().starts_with("EXPLAIN") {
        return Ok(sql);
    }
    Ok(format!("EXPLAIN {sql}"))
}

pub fn normalize_perf_probe_iterations(raw: Option<u32>) -> u32 {
    raw.unwrap_or(5).clamp(1, PERF_PROBE_MAX_ITERATIONS)
}

pub fn normalize_perf_probe_table_name(raw: Option<&str>) -> Result<String, AppError> {
    let table_name = raw.unwrap_or("").trim();
    if table_name.is_empty() {
        return Err(AppError::BadRequest(
            "Perf probe table_name is required".to_string(),
        ));
    }
    Ok(table_name.to_string())
}

fn default_perf_probe_budget(operation: &str) -> Option<PerfBudget> {
    match operation {
        "connect_warm" => Some(PerfBudget {
            operation: operation.to_string(),
            target_p50_ms: Some(50),
            target_p95_ms: Some(120),
            source: Some("phase1_local_warm_connect_target".to_string()),
        }),
        "query_select_small" => Some(PerfBudget {
            operation: operation.to_string(),
            target_p50_ms: Some(80),
            target_p95_ms: Some(150),
            source: Some("phase1_web_query_target".to_string()),
        }),
        "catalog_first_paint" => Some(PerfBudget {
            operation: operation.to_string(),
            target_p50_ms: Some(400),
            target_p95_ms: Some(700),
            source: Some("phase1_web_catalog_target".to_string()),
        }),
        "table_first_page" => Some(PerfBudget {
            operation: operation.to_string(),
            target_p50_ms: Some(120),
            target_p95_ms: Some(200),
            source: Some("phase1_web_table_first_page_target".to_string()),
        }),
        _ => None,
    }
}

pub async fn resolve_perf_probe_connection(
    state: &AppState,
    db_id: Option<&str>,
) -> Result<DbConnection, AppError> {
    let config = state.config.read().await.clone();
    if let Some(id) = db_id {
        return config
            .db_connections
            .iter()
            .find(|conn| conn.id == id)
            .cloned()
            .ok_or_else(|| AppError::BadRequest(format!("Database connection {id} not found")));
    }

    config
        .get_active_db_connection()
        .ok_or_else(|| AppError::BadRequest("Database not connected".to_string()))
}

pub async fn open_fresh_perf_probe_client(
    state: &AppState,
    db_id: Option<&str>,
) -> Result<DbClient, AppError> {
    let conn = resolve_perf_probe_connection(state, db_id).await?;
    let pool_config = state.config.read().await.pool_config.clone();
    open_db_client(&conn, &pool_config)
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))
}

pub async fn run_connect_warm_probe(
    state: &AppState,
    db_id: Option<&str>,
    iterations: u32,
) -> Result<PerfProbeSummary, AppError> {
    let (warm_client, _) = resolve_db_client_for_request(state, db_id).await?;
    tokio::time::timeout(state.timeouts.db_query, warm_client.ping())
        .await
        .map_err(|_| AppError::Timeout("connect_warm warmup ping timed out".to_string()))?
        .map_err(|e| AppError::BadRequest(e.to_string()))?;

    let mut samples = Vec::with_capacity(iterations as usize);
    for iteration in 0..iterations {
        let started_at = Instant::now();
        let (db_client, _) = resolve_db_client_for_request(state, db_id).await?;
        tokio::time::timeout(state.timeouts.db_query, db_client.ping())
            .await
            .map_err(|_| AppError::Timeout("connect_warm ping timed out".to_string()))?
            .map_err(|e| AppError::BadRequest(e.to_string()))?;
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

pub async fn run_connect_cold_probe(
    state: &AppState,
    db_id: Option<&str>,
    iterations: u32,
) -> Result<PerfProbeSummary, AppError> {
    let mut samples = Vec::with_capacity(iterations as usize);
    for iteration in 0..iterations {
        let started_at = Instant::now();
        let db_client = open_fresh_perf_probe_client(state, db_id).await?;
        tokio::time::timeout(state.timeouts.db_query, db_client.ping())
            .await
            .map_err(|_| AppError::Timeout("connect_cold ping timed out".to_string()))?
            .map_err(|e| AppError::BadRequest(e.to_string()))?;
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

pub async fn run_query_select_small_probe(
    state: &AppState,
    db_id: Option<&str>,
    sql: Option<&str>,
    iterations: u32,
) -> Result<PerfProbeSummary, AppError> {
    let sql = normalize_perf_probe_sql(sql)?;
    let (warm_client, _) = resolve_db_client_for_request(state, db_id).await?;
    tokio::time::timeout(
        state.timeouts.db_query,
        sqlx::query(&sql).fetch_all(warm_client.mysql_pool()?),
    )
    .await
    .map_err(|_| AppError::Timeout("query_select_small warmup timed out".to_string()))?
    .map_err(|e| AppError::BadRequest(e.to_string()))?;

    let mut samples = Vec::with_capacity(iterations as usize);
    for iteration in 0..iterations {
        let (db_client, _) = resolve_db_client_for_request(state, db_id).await?;
        let started_at = Instant::now();
        let rows = tokio::time::timeout(
            state.timeouts.db_query,
            sqlx::query(&sql).fetch_all(db_client.mysql_pool()?),
        )
        .await
        .map_err(|_| AppError::Timeout("query_select_small timed out".to_string()))?
        .map_err(|e| AppError::BadRequest(e.to_string()))?;

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

pub async fn run_query_write_small_probe(
    state: &AppState,
    db_id: Option<&str>,
    iterations: u32,
) -> Result<PerfProbeSummary, AppError> {
    let (db_client, _) = resolve_db_client_for_request(state, db_id).await?;
    let mut conn = db_client
        .mysql_pool()?
        .acquire()
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    let temp_table = "__perf_probe_write_small";
    let drop_sql = format!("DROP TEMPORARY TABLE IF EXISTS {temp_table}");
    let create_sql = format!(
        "CREATE TEMPORARY TABLE {temp_table} (id BIGINT PRIMARY KEY AUTO_INCREMENT, marker VARCHAR(64) NOT NULL)"
    );
    let insert_sql = format!("INSERT INTO {temp_table} (marker) VALUES (?)");

    tokio::time::timeout(
        state.timeouts.db_query,
        sqlx::query(&drop_sql).execute(&mut *conn),
    )
    .await
    .map_err(|_| AppError::Timeout("query_write_small drop temp table timed out".to_string()))?
    .map_err(|e| AppError::InternalError(e.to_string()))?;
    tokio::time::timeout(
        state.timeouts.db_query,
        sqlx::query(&create_sql).execute(&mut *conn),
    )
    .await
    .map_err(|_| AppError::Timeout("query_write_small create temp table timed out".to_string()))?
    .map_err(|e| AppError::InternalError(e.to_string()))?;
    tokio::time::timeout(
        state.timeouts.db_query,
        sqlx::query(&insert_sql).bind("warmup").execute(&mut *conn),
    )
    .await
    .map_err(|_| AppError::Timeout("query_write_small warmup timed out".to_string()))?
    .map_err(|e| AppError::InternalError(e.to_string()))?;

    let mut samples = Vec::with_capacity(iterations as usize);
    for iteration in 0..iterations {
        let started_at = Instant::now();
        let result = tokio::time::timeout(
            state.timeouts.db_query,
            sqlx::query(&insert_sql)
                .bind(format!("probe-{}", iteration + 1))
                .execute(&mut *conn),
        )
        .await
        .map_err(|_| AppError::Timeout("query_write_small timed out".to_string()))?
        .map_err(|e| AppError::InternalError(e.to_string()))?;

        samples.push(PerfSample {
            operation: "query_write_small".to_string(),
            iteration: iteration + 1,
            duration_ms: started_at.elapsed().as_millis(),
            rows: Some(result.rows_affected()),
        });
    }

    let _ = tokio::time::timeout(
        state.timeouts.db_query,
        sqlx::query(&drop_sql).execute(&mut *conn),
    )
    .await;

    Ok(summarize_perf_samples(
        "query_write_small",
        samples,
        default_perf_probe_budget("query_write_small"),
    ))
}

pub async fn run_explain_plan_probe(
    state: &AppState,
    db_id: Option<&str>,
    sql: Option<&str>,
    iterations: u32,
) -> Result<PerfProbeSummary, AppError> {
    let explain_sql = build_perf_probe_explain_sql(sql)?;
    let (warm_client, _) = resolve_db_client_for_request(state, db_id).await?;
    tokio::time::timeout(
        state.timeouts.db_query,
        sqlx::query(&explain_sql).fetch_all(warm_client.mysql_pool()?),
    )
    .await
    .map_err(|_| AppError::Timeout("explain_plan warmup timed out".to_string()))?
    .map_err(|e| AppError::BadRequest(e.to_string()))?;

    let mut samples = Vec::with_capacity(iterations as usize);
    for iteration in 0..iterations {
        let (db_client, _) = resolve_db_client_for_request(state, db_id).await?;
        let started_at = Instant::now();
        let rows = tokio::time::timeout(
            state.timeouts.db_query,
            sqlx::query(&explain_sql).fetch_all(db_client.mysql_pool()?),
        )
        .await
        .map_err(|_| AppError::Timeout("explain_plan timed out".to_string()))?
        .map_err(|e| AppError::BadRequest(e.to_string()))?;

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

pub async fn run_catalog_first_paint_probe(
    state: &AppState,
    db_id: Option<&str>,
    iterations: u32,
) -> Result<PerfProbeSummary, AppError> {
    let mut samples = Vec::with_capacity(iterations as usize);
    for iteration in 0..iterations {
        clear_metadata_caches(state).await;
        let (_db_client, _db_name) = resolve_db_client_for_request(state, db_id).await?;
        let started_at = Instant::now();
        let ctx = crate::bridge::bridge_service_context(state);
        let schema = core_lib::service::schema::SchemaService::get_schema(&ctx, db_id)
            .await
            .map_err(|e| e.to_app_error())?;
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

pub async fn run_table_first_page_probe(
    state: &AppState,
    db_id: Option<&str>,
    table_name: Option<&str>,
    iterations: u32,
) -> Result<PerfProbeSummary, AppError> {
    let table_name = normalize_perf_probe_table_name(table_name)?;
    let table_ident = quote_mysql_ident(&table_name)?;
    let data_sql = format!("SELECT * FROM {} LIMIT 101 OFFSET 0", table_ident);
    let mut samples = Vec::with_capacity(iterations as usize);

    for iteration in 0..iterations {
        clear_metadata_caches(state).await;
        let (db_client, db_name) = resolve_db_client_for_request(state, db_id).await?;
        let started_at = Instant::now();
        let _table_schema =
            get_cached_table_schema(state, db_id, &db_client, &db_name, &table_name).await?;
        let result_rows = tokio::time::timeout(
            state.timeouts.db_query,
            sqlx::query(&data_sql).fetch_all(db_client.mysql_pool()?),
        )
        .await
        .map_err(|_| {
            AppError::Timeout(
                "table_first_page probe timed out after 30 seconds".to_string(),
            )
        })?
        .map_err(|e| AppError::InternalError(e.to_string()))?;

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

pub async fn run_cancel_latency_probe(
    state: &AppState,
    db_id: Option<&str>,
    iterations: u32,
) -> Result<PerfProbeSummary, AppError> {
    let mut samples = Vec::with_capacity(iterations as usize);

    for iteration in 0..iterations {
        let (db_client, _) = resolve_db_client_for_request(state, db_id).await?;
        let mut conn = db_client
            .mysql_pool()?
            .acquire()
            .await
            .map_err(|e| AppError::InternalError(e.to_string()))?;
        let connection_id = DbClient::connection_id_for_session(&mut conn)
            .await
            .map_err(|e| AppError::InternalError(e.to_string()))?;
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
        let join_result = tokio::time::timeout(state.timeouts.db_query, query_task)
            .await
            .map_err(|_| AppError::Timeout("cancel_latency probe join timed out".to_string()))?;
        unregister_active_query(state, &cancel_token).await;
        join_result
            .map_err(|e| AppError::InternalError(e.to_string()))?
            .map_err(|e| AppError::InternalError(e.to_string()))?;

        if !canceled_ok {
            return Err(AppError::BadRequest(
                "cancel_latency probe could not cancel active query".to_string(),
            ));
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

pub async fn diagnostics_perf_probe(
    State(state): State<AppState>,
    Json(req): Json<PerfProbeRequest>,
) -> Result<Json<PerfProbeSummary>, AppError> {
    let operation = req
        .operation
        .clone()
        .unwrap_or_else(|| "connect_warm".to_string())
        .trim()
        .to_lowercase();
    let iterations = normalize_perf_probe_iterations(req.iterations);

    // 先做与引擎无关的参数校验，保证错误原因最具体（read-only / table_name），
    // 再判断引擎是否支持性能探针。
    match operation.as_str() {
        "query_select_small" | "explain_plan" => {
            normalize_perf_probe_sql(req.sql.as_deref())?;
        }
        "table_first_page" => {
            normalize_perf_probe_table_name(req.table_name.as_deref())?;
        }
        _ => {}
    }

    // 性能探针依赖 MySQL 的临时表语法、information_schema 与 performance_schema。
    // 非 MySQL 连接给出明确提示，避免底层驱动错误直接冒到界面。
    let conn = resolve_perf_probe_connection(&state, req.db_id.as_deref()).await?;
    require_mysql_connection(&conn, "性能探针")?;

    let summary = match operation.as_str() {
        "connect_cold" => {
            run_connect_cold_probe(&state, req.db_id.as_deref(), iterations).await?
        }
        "connect_warm" => {
            run_connect_warm_probe(&state, req.db_id.as_deref(), iterations).await?
        }
        "query_select_small" => {
            run_query_select_small_probe(&state, req.db_id.as_deref(), req.sql.as_deref(), iterations)
                .await?
        }
        "query_write_small" => {
            run_query_write_small_probe(&state, req.db_id.as_deref(), iterations).await?
        }
        "explain_plan" => {
            run_explain_plan_probe(&state, req.db_id.as_deref(), req.sql.as_deref(), iterations)
                .await?
        }
        "catalog_first_paint" => {
            run_catalog_first_paint_probe(&state, req.db_id.as_deref(), iterations).await?
        }
        "table_first_page" => {
            run_table_first_page_probe(
                &state,
                req.db_id.as_deref(),
                req.table_name.as_deref(),
                iterations,
            )
            .await?
        }
        "cancel_latency" => {
            run_cancel_latency_probe(&state, req.db_id.as_deref(), iterations).await?
        }
        _ => {
            return Err(AppError::BadRequest(format!(
                "Unsupported perf probe operation: {}",
                operation
            )));
        }
    };

    Ok(Json(summary))
}

pub fn perf_suite_archive_dir(limits: &RuntimeLimits) -> std::path::PathBuf {
    let mut path = std::path::PathBuf::from(&limits.temp_dir);
    path.push("diagnostics");
    path.push("perf-suites");
    path
}

pub fn perf_suite_index_path(limits: &RuntimeLimits) -> std::path::PathBuf {
    perf_suite_archive_dir(limits).join("index.jsonl")
}

pub fn perf_suite_baseline_path(limits: &RuntimeLimits) -> std::path::PathBuf {
    perf_suite_archive_dir(limits).join("baseline.json")
}

pub fn perf_suite_diff_archive_dir(limits: &RuntimeLimits) -> std::path::PathBuf {
    perf_suite_archive_dir(limits).join("diffs")
}

pub fn perf_suite_diff_index_path(limits: &RuntimeLimits) -> std::path::PathBuf {
    perf_suite_diff_archive_dir(limits).join("index.jsonl")
}

pub async fn read_jsonl_all(path: &str) -> Result<Vec<serde_json::Value>, AppError> {
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
    Ok(rows)
}

pub async fn find_perf_suite_archive_record(
    limits: &RuntimeLimits,
    suite_id: &str,
) -> Result<Option<PerfSuiteArchiveRecord>, AppError> {
    let path = perf_suite_index_path(limits);
    let rows = read_jsonl_all(&path.to_string_lossy()).await?;
    for row in rows.into_iter().rev() {
        let Some(id) = row.get("id").and_then(|value| value.as_str()) else {
            continue;
        };
        if id != suite_id {
            continue;
        }
        let indexed_report = match serde_json::from_value::<PerfSuiteArchiveRecord>(row) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if let Some(archive_path) = indexed_report.archive_path.clone() {
            match tokio::fs::read_to_string(&archive_path).await {
                Ok(content) => {
                    if let Ok(report) = serde_json::from_str::<PerfSuiteArchiveRecord>(&content) {
                        return Ok(Some(report));
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(AppError::InternalError(e.to_string())),
            }
        }
        return Ok(Some(indexed_report));
    }
    Ok(None)
}

pub async fn diagnostics_perf_suite_list(
    State(state): State<AppState>,
    Query(q): Query<LimitQuery>,
) -> Result<Json<Vec<PerfSuiteArchiveRecord>>, AppError> {
    let limit = q
        .limit
        .unwrap_or(PERF_SUITE_ARCHIVE_DEFAULT_LIMIT)
        .clamp(1, 200);
    let path = perf_suite_index_path(&state.limits);
    let rows = read_jsonl_recent(&path.to_string_lossy(), limit).await?;
    let mut reports = Vec::with_capacity(rows.len());
    for row in rows {
        if let Ok(report) = serde_json::from_value::<PerfSuiteArchiveRecord>(row) {
            reports.push(report);
        }
    }
    Ok(Json(reports))
}

pub async fn diagnostics_perf_suite_detail(
    State(state): State<AppState>,
    Path(suite_id): Path<String>,
) -> Result<Json<PerfSuiteArchiveRecord>, AppError> {
    let report = find_perf_suite_archive_record(&state.limits, &suite_id)
        .await?
        .ok_or_else(|| AppError::NotFound("perf suite not found".to_string()))?;
    Ok(Json(report))
}

pub async fn diagnostics_perf_suite_save(
    State(state): State<AppState>,
    Json(mut report): Json<PerfSuiteArchiveRecord>,
) -> Result<Json<PerfSuiteArchiveRecord>, AppError> {
    if report.id.trim().is_empty() {
        return Err(AppError::BadRequest(
            "Perf suite report id is required".to_string(),
        ));
    }
    if report.recorded_at.trim().is_empty() {
        return Err(AppError::BadRequest(
            "Perf suite recorded_at is required".to_string(),
        ));
    }

    let archive_dir = perf_suite_archive_dir(&state.limits);
    let file_stem = {
        let value = safe_ident_suffix(&report.id);
        if value.is_empty() {
            "suite".to_string()
        } else {
            value
        }
    };
    let file_name = format!(
        "{}-{}.json",
        chrono::Utc::now().format("%Y%m%dT%H%M%S"),
        file_stem
    );
    let report_path = archive_dir.join(file_name);
    report.archive_path = Some(report_path.to_string_lossy().to_string());

    let pretty_bytes = serde_json::to_vec_pretty(&report)
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    let jsonl_line = serde_json::to_string(&report)
        .map_err(|e| AppError::InternalError(e.to_string()))?;

    ensure_temp_quota(
        &state.limits,
        pretty_bytes.len() as u64 + jsonl_line.len() as u64 + 1,
    )
    .await?;
    tokio::fs::create_dir_all(&archive_dir)
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    tokio::fs::write(&report_path, pretty_bytes)
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;

    let index_path = perf_suite_index_path(&state.limits);
    let mut index = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&index_path)
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    index
        .write_all(jsonl_line.as_bytes())
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    index
        .write_all(b"\n")
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;

    Ok(Json(report))
}

pub async fn diagnostics_perf_suite_baseline_get(
    State(state): State<AppState>,
) -> Result<Json<Option<PerfSuiteArchiveRecord>>, AppError> {
    let path = perf_suite_baseline_path(&state.limits);
    let content = match tokio::fs::read_to_string(&path).await {
        Ok(v) => v,
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                return Ok(Json(None));
            }
            return Err(AppError::InternalError(e.to_string()));
        }
    };
    let report = serde_json::from_str::<PerfSuiteArchiveRecord>(&content)
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    Ok(Json(Some(report)))
}

pub async fn diagnostics_perf_suite_baseline_pin(
    State(state): State<AppState>,
    Json(req): Json<PerfSuiteBaselinePinRequest>,
) -> Result<Json<PerfSuiteArchiveRecord>, AppError> {
    let suite_id = req.suite_id.trim();
    if suite_id.is_empty() {
        return Err(AppError::BadRequest(
            "Perf suite baseline suite_id is required".to_string(),
        ));
    }

    let report = find_perf_suite_archive_record(&state.limits, suite_id)
        .await?
        .ok_or_else(|| AppError::NotFound("perf suite not found".to_string()))?;
    let baseline_path = perf_suite_baseline_path(&state.limits);
    let bytes = serde_json::to_vec_pretty(&report)
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    ensure_temp_quota(&state.limits, bytes.len() as u64).await?;
    tokio::fs::create_dir_all(perf_suite_archive_dir(&state.limits))
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    tokio::fs::write(baseline_path, bytes)
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    Ok(Json(report))
}

pub async fn diagnostics_perf_suite_diff_list(
    State(state): State<AppState>,
    Query(q): Query<PerfSuiteDiffListQuery>,
) -> Result<Json<Vec<PerfSuiteDiffArchiveRecord>>, AppError> {
    let limit = q.limit.unwrap_or(PERF_SUITE_ARCHIVE_DEFAULT_LIMIT).clamp(1, 200);
    let path = perf_suite_diff_index_path(&state.limits);
    let rows = read_jsonl_recent(&path.to_string_lossy(), limit).await?;
    let mut reports = Vec::with_capacity(rows.len());
    for row in rows {
        let Ok(report) = serde_json::from_value::<PerfSuiteDiffArchiveRecord>(row) else {
            continue;
        };
        if let Some(current_suite_id) = q.current_suite_id.as_deref() {
            if report.current_suite_id != current_suite_id {
                continue;
            }
        }
        if let Some(baseline_suite_id) = q.baseline_suite_id.as_deref() {
            if report.baseline_suite_id != baseline_suite_id {
                continue;
            }
        }
        reports.push(report);
    }
    Ok(Json(reports))
}

pub async fn diagnostics_perf_suite_diff_save(
    State(state): State<AppState>,
    Json(mut report): Json<PerfSuiteDiffArchiveRecord>,
) -> Result<Json<PerfSuiteDiffArchiveRecord>, AppError> {
    if report.id.trim().is_empty() {
        return Err(AppError::BadRequest(
            "Perf suite diff report id is required".to_string(),
        ));
    }
    if report.recorded_at.trim().is_empty() {
        return Err(AppError::BadRequest(
            "Perf suite diff recorded_at is required".to_string(),
        ));
    }
    if report.current_suite_id.trim().is_empty() || report.baseline_suite_id.trim().is_empty() {
        return Err(AppError::BadRequest(
            "Perf suite diff current/baseline suite id is required".to_string(),
        ));
    }

    let archive_dir = perf_suite_diff_archive_dir(&state.limits);
    let file_stem = {
        let value = safe_ident_suffix(&report.id);
        if value.is_empty() {
            "diff".to_string()
        } else {
            value
        }
    };
    let file_name = format!(
        "{}-{}.json",
        chrono::Utc::now().format("%Y%m%dT%H%M%S"),
        file_stem
    );
    let report_path = archive_dir.join(file_name);
    report.archive_path = Some(report_path.to_string_lossy().to_string());

    let pretty_bytes = serde_json::to_vec_pretty(&report)
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    let jsonl_line = serde_json::to_string(&report)
        .map_err(|e| AppError::InternalError(e.to_string()))?;

    ensure_temp_quota(
        &state.limits,
        pretty_bytes.len() as u64 + jsonl_line.len() as u64 + 1,
    )
    .await?;
    tokio::fs::create_dir_all(&archive_dir)
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    tokio::fs::write(&report_path, pretty_bytes)
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;

    let index_path = perf_suite_diff_index_path(&state.limits);
    let mut index = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&index_path)
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    index
        .write_all(jsonl_line.as_bytes())
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    index
        .write_all(b"\n")
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;

    Ok(Json(report))
}

pub async fn execute_sql(
    State(state): State<AppState>,
    Json(req): Json<ExecuteRequest>,
) -> Result<Json<ExecuteResponse>, AppError> {
    use core_lib::service::workbench::{ExecuteParams, WorkbenchService};

    let ctx = crate::bridge::bridge_service_context(&state);
    let result = WorkbenchService::execute_sql(
        &ctx,
        ExecuteParams {
            sql: req.sql,
            force: req.force,
            db_id: req.db_id,
            chunk_offset: req.chunk_offset,
            chunk_size: req.chunk_size,
            cancel_token: req.cancel_token,
            transaction_id: req.transaction_id,
        },
    )
    .await
    .map_err(|e| e.to_app_error())?;

    Ok(Json(ExecuteResponse {
        columns: result.columns,
        rows: result.rows,
        row_count: result.row_count,
        affected_rows: result.affected_rows,
        execution_time_ms: result.execution_time_ms,
        has_more: result.has_more,
        next_offset: result.next_offset,
        chunk_offset: result.chunk_offset,
        chunk_size: result.chunk_size,
        preview_cap: result.preview_cap,
        truncated: result.truncated,
        transaction_state: result.transaction_state,
    }))
}
