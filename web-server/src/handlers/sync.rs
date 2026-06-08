//! Sync handlers — schema sync, data sync, MySQL sync, and perf sync.

use axum::{
    extract::{Path, State},
    Json,
};
use core_lib::{
    db::{capability::DbCapabilities, DbClient},
    error::AppError,
    mysql_sync::{CompareResult, MySqlDataSyncEngine, PreviewResult, SyncMode},
    timeout_policy::TimeoutPolicy,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::quote_mysql_ident;

use crate::state::*;
use crate::{resolve_db_client_for_request, get_temp_db_client, GAP_TOO_LARGE_MSG};

#[derive(Deserialize)]
pub(crate) struct MySqlSyncCompareRequest {
    source_db_id: String,
    target_db_id: String,
    table_name: String,
    primary_key: String,
    mode: SyncMode,
    chunk_size: Option<usize>,
}

#[derive(Deserialize)]
pub(crate) struct MySqlSyncPreviewRequest {
    job_id: String,
    max_rows: Option<usize>,
    actions: Option<Vec<String>>,
}

#[derive(Deserialize)]
pub(crate) struct MySqlSyncDeployRequest {
    job_id: String,
}

#[derive(Serialize)]
pub(crate) struct MySqlSyncJobStartResponse {
    job_id: String,
}

pub async fn mysql_sync_compare(
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
        let source_conn = config
            .db_connections
            .iter()
            .find(|c| c.id == req.source_db_id);
        let db_type = source_conn
            .and_then(|c| c.db_type.clone())
            .unwrap_or(core_lib::config::DbType::MySQL);
        let caps = DbCapabilities::runtime_capabilities(&db_type);
        if let Err(e) = caps.check_capability("data_sync") {
            return Err(AppError::BadRequest(e));
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
            let total_chunks = compare.chunks.len();
            if total_chunks >= 20
                && compare.different_chunks.saturating_mul(100) / total_chunks >= 85
            {
                return Err(AppError::BadRequest(GAP_TOO_LARGE_MSG.to_string()));
            }
            if compare.different_chunks >= 500 {
                return Err(AppError::BadRequest(GAP_TOO_LARGE_MSG.to_string()));
            }

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

pub async fn mysql_sync_preview(
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

    {
        let config = state.config.read().await;
        let source_conn = config
            .db_connections
            .iter()
            .find(|c| c.id == job.source_db_id);
        let db_type = source_conn
            .and_then(|c| c.db_type.clone())
            .unwrap_or(core_lib::config::DbType::MySQL);
        let caps = DbCapabilities::runtime_capabilities(&db_type);
        if let Err(e) = caps.check_capability("data_sync") {
            return Err(AppError::BadRequest(e));
        }
    }

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

pub async fn mysql_sync_deploy(
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

    {
        let config = state.config.read().await;
        let source_conn = config
            .db_connections
            .iter()
            .find(|c| c.id == job.source_db_id);
        let db_type = source_conn
            .and_then(|c| c.db_type.clone())
            .unwrap_or(core_lib::config::DbType::MySQL);
        let caps = DbCapabilities::runtime_capabilities(&db_type);
        if let Err(e) = caps.check_capability("data_sync") {
            return Err(AppError::BadRequest(e));
        }
    }

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

pub async fn mysql_sync_job_status(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Result<Json<MySqlSyncJob>, AppError> {
    let job = { state.sync_jobs.read().await.get(&job_id).cloned() };
    job.map(Json)
        .ok_or_else(|| AppError::NotFound("job not found".to_string()))
}

pub async fn update_mysql_sync_job(state: &AppState, job_id: &str, f: impl FnOnce(&mut MySqlSyncJob)) {
    let mut jobs = state.sync_jobs.write().await;
    if let Some(job) = jobs.get_mut(job_id) {
        f(job);
    }
}

#[derive(Serialize)]
pub(crate) struct PerfSyncJobStartResponse {
    job_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PerfSyncCheckRequest {
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
pub(crate) struct PerfSyncCheckResponse {
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

pub async fn perf_sync_check(
    State(state): State<AppState>,
    Json(req): Json<PerfSyncCheckRequest>,
) -> Result<Json<PerfSyncCheckResponse>, AppError> {
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

pub async fn fetch_table_count(db: &DbClient, table: &str) -> Result<u64, AppError> {
    let policy = TimeoutPolicy::default();
    let safe_table = quote_mysql_ident(table)?;
    let sql = format!("SELECT COUNT(*) FROM {}", safe_table);
    let fut = sqlx::query_scalar::<_, i64>(&sql).fetch_one(db.mysql_pool()?);
    let v = tokio::time::timeout(policy.db_query, fut)
        .await
        .map_err(|_| AppError::Timeout(format!("统计表 {} 行数超时", table)))?
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    Ok(v.max(0) as u64)
}

pub async fn fetch_counts(
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

pub async fn run_sync_table(
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
    let preview =
        MySqlDataSyncEngine::preview(source, target, &compare, mode, max_rows, None).await?;
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

pub async fn verify_table(
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

pub async fn perf_sync_start(
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

pub async fn run_perf_sync_job(
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
    stage_ms.insert(
        "detect_baseline".to_string(),
        t_baseline.elapsed().as_millis(),
    );

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
        stage_ms.insert(
            "inject_upsert_only".to_string(),
            t_inject.elapsed().as_millis(),
        );
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

pub async fn perf_sync_job_status(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Result<Json<PerfSyncJob>, AppError> {
    let job = { state.perf_sync_jobs.read().await.get(&job_id).cloned() };
    job.map(Json)
        .ok_or_else(|| AppError::NotFound("job not found".to_string()))
}

pub async fn update_perf_sync_job(state: &AppState, job_id: &str, f: impl FnOnce(&mut PerfSyncJob)) {
    let mut jobs = state.perf_sync_jobs.write().await;
    if let Some(job) = jobs.get_mut(job_id) {
        f(job);
    }
}
