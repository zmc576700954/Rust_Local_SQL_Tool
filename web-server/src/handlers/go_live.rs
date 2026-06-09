//! GoLive handler — pre-deployment checks, job orchestration, and report generation.

use super::util::row_to_json;
use axum::{
    body::Body,
    extract::{Path, Query, State},
    response::Response,
    Json,
};
use core_lib::{
    config::{AppConfig, DbType},
    db::DbClient,
    error::AppError,
    tools::DataExporter,
};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;

use crate::state::*;
use crate::mysql_codec::*;
use crate::{
    ExportJobStartRequest, ImportJobStartRequest, ImportSqlJobStartRequest,
    ToolJobStartResponse, DB_CLIENT_CACHE_TTL, CachedDbClient,
};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct GoLiveThresholds {
    #[serde(default)]
    max_total_ms: Option<u64>,
    #[serde(default)]
    per_step_max_ms: HashMap<String, u64>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub(crate) struct GoLiveJobStartRequest {
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
pub(crate) struct GoLiveStepReport {
    name: String,
    connection_id: Option<String>,
    status: GoLiveStepStatus,
    duration_ms: u128,
    errors: Vec<String>,
    code: Option<String>,
    details: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct GoLiveReport {
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

pub async fn tool_job_status(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Result<Json<ToolJob>, AppError> {
    let job = { state.tool_jobs.read().await.get(&job_id).cloned() };
    job.map(Json)
        .ok_or_else(|| AppError::NotFound("job not found".to_string()))
}

pub async fn tool_job_cancel(
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

pub async fn tool_job_artifact_download(
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
pub(crate) struct LimitQuery {
    pub(crate) limit: Option<usize>,
}

pub async fn go_live_reports_list(
    State(state): State<AppState>,
    Query(q): Query<LimitQuery>,
) -> Result<Json<Vec<serde_json::Value>>, AppError> {
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let temp_dir = state.limits.temp_dir.trim_end_matches('/').to_string();
    let path = format!("{}/go-live-index.jsonl", temp_dir);
    Ok(Json(read_jsonl_recent(&path, limit).await?))
}

pub async fn go_live_audit_list(
    State(state): State<AppState>,
    Query(q): Query<LimitQuery>,
) -> Result<Json<Vec<serde_json::Value>>, AppError> {
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let temp_dir = state.limits.temp_dir.trim_end_matches('/').to_string();
    let path = format!("{}/go-live-audit.jsonl", temp_dir);
    Ok(Json(read_jsonl_recent(&path, limit).await?))
}

pub async fn read_jsonl_recent(path: &str, limit: usize) -> Result<Vec<serde_json::Value>, AppError> {
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

pub async fn export_job_start(
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
            let stats = run_export_job(
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

pub async fn import_job_start(
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

pub async fn import_sql_job_start(
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
    let db_client = if let Some(db_id) = req.db_id.clone() {
        get_temp_db_client(&state, &db_id).await?.0
    } else {
        state
            .db_client
            .read()
            .await
            .clone()
            .ok_or_else(|| AppError::BadRequest("Database not connected".to_string()))?
    };

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

pub fn mask_db_url(url: &str) -> String {
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
    format!(
        "{}{}@{}",
        &url[..scheme_end],
        masked_creds,
        &rest[at_idx + 1..]
    )
}

pub fn sanitize_config_for_report(config: &AppConfig) -> serde_json::Value {
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

pub fn ai_key_present(config: &AppConfig) -> bool {
    let p = config.resolve_ai_profile();
    match p.mode {
        core_lib::config::AiConnectionMode::Pool => {
            p.pool.tokens.iter().any(|t| !t.trim().is_empty())
        }
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

pub fn normalize_go_live_steps(raw: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let steps = if raw.is_empty() {
        default_go_live_steps()
    } else {
        raw
    };
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

pub fn go_live_step_is_write(step: &str) -> bool {
    matches!(step, "sql_smoke" | "export_import_smoke")
}

pub fn safe_ident_suffix(s: &str) -> String {
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

pub fn apply_go_live_thresholds(
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

pub async fn go_live_job_start(
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
        if t.is_empty() {
            None
        } else {
            Some(t)
        }
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
pub(crate) struct GoLiveConnSpec {
    id: String,
    url: String,
    db_type: DbType,
    is_read_only: bool,
}

pub fn resolve_go_live_connections(
    config: &AppConfig,
    ids: &[String],
) -> (Vec<GoLiveConnSpec>, Vec<String>) {
    let mut out: Vec<GoLiveConnSpec> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    let resolved_ids = if ids.is_empty() {
        if let Some(active_id) = &config.active_db_id {
            vec![active_id.clone()]
        } else {
            vec!["active".to_string()]
        }
    } else {
        ids.iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
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
            let db_type = conn
                .db_type
                .clone()
                .unwrap_or_else(|| DbType::from_url(&url).unwrap_or(DbType::MySQL));
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

pub async fn append_jsonl(
    path: &str,
    limits: &RuntimeLimits,
    value: &serde_json::Value,
) -> Result<(), AppError> {
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

pub async fn run_go_live_job(
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
    let thresholds = req
        .thresholds
        .clone()
        .filter(|t| t.max_total_ms.unwrap_or(0) > 0 || !t.per_step_max_ms.is_empty());

    let per_conn_steps: Vec<String> = requested_steps
        .iter()
        .filter(|s| {
            matches!(
                s.as_str(),
                "mysql_connect" | "sql_smoke" | "export_import_smoke"
            )
        })
        .cloned()
        .collect();

    let total_steps = (if requested_steps.iter().any(|s| s == "config") {
        1
    } else {
        0
    }) + (connections.len() * per_conn_steps.len())
        + (if requested_steps.iter().any(|s| s == "ai_smoke") {
            1
        } else {
            0
        });

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
        append_jsonl(
            &index_path,
            &limits,
            &serde_json::json!({
                "job_id": job_id,
                "created_at": report.created_at.clone(),
                "finished_at": report.finished_at.clone(),
                "passed": report.passed,
                "operator": report.operator.clone(),
                "connection_ids": report.connection_ids.clone(),
                "report_path": path
            }),
        )
        .await?;
        append_jsonl(
            &audit_path,
            &limits,
            &serde_json::json!({
                "ts": chrono::Utc::now().timestamp(),
                "action": "go_live_job_finished",
                "job_id": job_id,
                "operator": report.operator.clone(),
                "passed": report.passed,
                "elapsed_ms": report.elapsed_ms
            }),
        )
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

            let client_res: Result<DbClient, String> =
                if let Some(c) = clients.get(&conn_id).cloned() {
                    Ok(c)
                } else {
                    DbClient::new_default(&conn.url).await.map_err(|e| e.to_string())
                };

            let mut client_opt: Option<DbClient> = None;
            match client_res {
                Ok(c) => {
                    let r: Result<(i64,), sqlx::Error> =
                        sqlx::query_as("SELECT 1").fetch_one(c.mysql_pool()?).await;
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
                        match client.mysql_pool()?.acquire().await {
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
                                        let r = sqlx::query(
                                            "INSERT INTO go_live_tmp_smoke (v) VALUES (?)",
                                        )
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
                                                errors.push(format!(
                                                    "pagination rows != 10: {}",
                                                    v.len()
                                                ));
                                            } else if v[0].0 != 11 {
                                                errors.push(format!(
                                                    "pagination first id != 11: {}",
                                                    v[0].0
                                                ));
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
                        let suffix = format!("{}_{}", &id_short[..8], safe_ident_suffix(&conn_id));
                        let src_table = format!("go_live_smoke_items_{}", suffix);
                        let dst_table = format!("go_live_smoke_items_imported_{}", suffix);

                        let pool = client.mysql_pool()?.clone();
                        let safe_src = quote_mysql_ident(&src_table)?;
                        let safe_dst = quote_mysql_ident(&dst_table)?;
                        let drop_all = async {
                            let _ = sqlx::query(&format!("DROP TABLE IF EXISTS {}", safe_dst))
                                .execute(&pool)
                                .await;
                            let _ = sqlx::query(&format!("DROP TABLE IF EXISTS {}", safe_src))
                                .execute(&pool)
                                .await;
                        };

                        let limits = state.limits.clone();
                        let temp_dir = limits.temp_dir.trim_end_matches('/').to_string();
                        let export_path = format!(
                            "{}/go-live-export-{}-{}.json",
                            temp_dir,
                            job_id,
                            safe_ident_suffix(&conn_id)
                        );
                        let dummy_job_id = uuid::Uuid::new_v4().to_string();
                        let nop_state = AppState {
                            tool_jobs: Arc::new(RwLock::new(HashMap::new())),
                            tool_job_handles: Arc::new(RwLock::new(HashMap::new())),
                            ..state.clone()
                        };

                        let r: Result<(usize, usize, u64), AppError> = async {
                            sqlx::query(&format!("DROP TABLE IF EXISTS {}", safe_dst))
                                .execute(&pool)
                                .await
                                .map_err(|e| AppError::InternalError(e.to_string()))?;
                            sqlx::query(&format!("DROP TABLE IF EXISTS {}", safe_src))
                                .execute(&pool)
                                .await
                                .map_err(|e| AppError::InternalError(e.to_string()))?;

                            sqlx::query(&format!(
                                "CREATE TABLE {} (id BIGINT PRIMARY KEY, name VARCHAR(255) NOT NULL, score DOUBLE NOT NULL, created_at DATETIME NOT NULL)",
                                safe_src
                            ))
                            .execute(&pool)
                            .await
                            .map_err(|e| AppError::InternalError(e.to_string()))?;

                            sqlx::query(&format!(
                                "CREATE TABLE {} (id BIGINT PRIMARY KEY, name VARCHAR(255) NOT NULL, score DOUBLE NOT NULL, created_at DATETIME NOT NULL)",
                                safe_dst
                            ))
                            .execute(&pool)
                            .await
                            .map_err(|e| AppError::InternalError(e.to_string()))?;

                            let now = chrono::Utc::now().naive_utc();
                            for i in 1..=25i64 {
                                sqlx::query(&format!(
                                    "INSERT INTO {} (id, name, score, created_at) VALUES (?, ?, ?, ?)",
                                    safe_src
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

                            let (c,): (i64,) = sqlx::query_as(&format!("SELECT COUNT(*) FROM {}", safe_dst))
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
            let config = state.config.read().await.clone();
            match core_lib::ai::agent::generate_rule_template(&config, "go-live smoke", "SELECT 1;")
                .await
            {
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
    append_jsonl(
        &index_path,
        &limits,
        &serde_json::json!({
            "job_id": job_id,
            "created_at": report.created_at.clone(),
            "finished_at": report.finished_at.clone(),
            "passed": report.passed,
            "operator": report.operator.clone(),
            "connection_ids": report.connection_ids.clone(),
            "report_path": path
        }),
    )
    .await?;
    append_jsonl(
        &audit_path,
        &limits,
        &serde_json::json!({
            "ts": chrono::Utc::now().timestamp(),
            "action": "go_live_job_finished",
            "job_id": job_id,
            "operator": report.operator.clone(),
            "passed": report.passed,
            "elapsed_ms": report.elapsed_ms
        }),
    )
    .await?;

    Ok((report, path))
}

pub async fn write_go_live_report(
    state: &AppState,
    job_id: &str,
    report: &GoLiveReport,
) -> Result<String, AppError> {
    let limits = state.limits.clone();
    let temp_dir = limits.temp_dir.trim_end_matches('/').to_string();
    let report_path = format!("{}/go-live-report-{}.json", temp_dir, job_id);
    let bytes =
        serde_json::to_vec_pretty(report).map_err(|e| AppError::InternalError(e.to_string()))?;
    ensure_temp_quota(&limits, bytes.len() as u64).await?;
    tokio::fs::write(&report_path, bytes)
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    Ok(report_path)
}

pub(crate) struct ExportStats {
    sha256: String,
    line_count: u64,
    bytes: u64,
    row_count: u64,
}

pub async fn update_tool_job(state: &AppState, job_id: &str, f: impl FnOnce(&mut ToolJob)) {
    let mut jobs = state.tool_jobs.write().await;
    if let Some(job) = jobs.get_mut(job_id) {
        f(job);
        job.updated_at = chrono::Utc::now().timestamp();
    }
}

pub fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

pub fn sql_literal(s: &str) -> String {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        "NULL".to_string()
    } else if trimmed.parse::<i64>().is_ok() || trimmed.parse::<f64>().is_ok() {
        trimmed.to_string()
    } else {
        format!("'{}'", trimmed.replace('\'', "''"))
    }
}

pub async fn write_stats_chunk(
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

pub async fn fetch_table_columns(
    pool: &sqlx::MySqlPool,
    table_name: &str,
) -> Result<Vec<String>, AppError> {
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
        let name: String = r
            .try_get(0)
            .map_err(|e| AppError::InternalError(e.to_string()))?;
        cols.push(name);
    }
    Ok(cols)
}

pub async fn run_export_job(
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

    if !matches!(export_type.as_str(), "csv" | "txt" | "sql" | "xml" | "json") {
        return Err(AppError::BadRequest(format!(
            "Unsupported export format: {}",
            req.export_type
        )));
    }

    let headers = fetch_table_columns(db_client.mysql_pool()?, &req.table_name)
        .await
        .unwrap_or_default();

    let mut conditions = Vec::new();
    if let Some(w) = &req.where_clause {
        if !w.trim().is_empty() {
            conditions.push(format!("({})", w));
        }
    }
    if let Some(pk) = &req.primary_key {
        let safe_pk = quote_mysql_ident(pk)?;
        if let Some(s) = &req.pk_start {
            if !s.trim().is_empty() {
                conditions.push(format!("{} >= {}", safe_pk, sql_literal(s)));
            }
        }
        if let Some(s) = &req.pk_end {
            if !s.trim().is_empty() {
                conditions.push(format!("{} <= {}", safe_pk, sql_literal(s)));
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
        .map(|pk| {
            quote_mysql_ident(pk).map(|safe_pk| format!(" ORDER BY {}", safe_pk))
        })
        .transpose()?
        .unwrap_or_default();

    let mut limit_sql = String::new();
    if let Some(lim) = req.window_limit {
        limit_sql.push_str(&format!(" LIMIT {}", lim));
        if let Some(off) = req.window_offset {
            limit_sql.push_str(&format!(" OFFSET {}", off));
        }
    }

    let safe_table = quote_mysql_ident(&req.table_name)?;
    let data_sql = format!(
        "SELECT * FROM {}{}{}{}",
        safe_table, where_sql, order_sql, limit_sql
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

    let mut stream = sqlx::query(&data_sql).fetch(db_client.mysql_pool()?);
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

        let map = row_to_json(&row);

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
    let sha256 = hash
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<String>();

    Ok(ExportStats {
        sha256,
        line_count,
        bytes,
        row_count: processed,
    })
}

pub async fn run_import_job(
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
    let sql = format!(
        "INSERT INTO {} ({}) VALUES ({})",
        table_ident, col_list, placeholders
    );

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

        match query.execute(db_client.mysql_pool()?).await {
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

pub async fn run_import_sql_job(
    db_client: &DbClient,
    state: &AppState,
    job_id: &str,
    req: ImportSqlJobStartRequest,
) -> Result<serde_json::Value, AppError> {
    let is_read_only = {
        let config = state.config.read().await;
        let selected_db_id = req.db_id.clone().or_else(|| config.active_db_id.clone());
        if let Some(active_id) = &selected_db_id {
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
        .execute(db_client.mysql_pool()?)
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;

    Ok(serde_json::json!({
        "affected_rows": result.rows_affected()
    }))
}

pub async fn get_temp_db_client(state: &AppState, db_id: &str) -> Result<(DbClient, String), AppError> {
    let config = state.config.read().await.clone();
    let conn = config
        .db_connections
        .iter()
        .find(|c| c.id == db_id)
        .ok_or_else(|| AppError::BadRequest(format!("Database connection {} not found", db_id)))?;
    let db_name = DbClient::extract_db_name(&conn.url).unwrap_or_default();

    if config.active_db_id.as_deref() == Some(db_id) {
        if let Some(client) = state.db_client.read().await.clone() {
            return Ok((client, db_name));
        }
    }

    let now = Instant::now();
    if let Some(entry) = state.db_client_cache.read().await.get(db_id).cloned() {
        if entry.url == conn.url && entry.expires_at > now {
            return Ok((entry.client, entry.db_name));
        }
    }

    let client = DbClient::new_default(&conn.url)
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    let entry = CachedDbClient {
        client: client.clone(),
        db_name: db_name.clone(),
        url: conn.url.clone(),
        expires_at: now + DB_CLIENT_CACHE_TTL,
    };
    state
        .db_client_cache
        .write()
        .await
        .insert(db_id.to_string(), entry);
    Ok((client, db_name))
}

