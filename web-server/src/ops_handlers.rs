//! ops 路由处理器：阶段 0 的 MySQL 索引优化闭环。
//!
//! 所有写接口都要求本机共享令牌 `x-ops-token`；审批令牌与计划哈希绑定并一次性消费。

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::sse::{Event, Sse};
use axum::Json;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;

use core_lib::error::AppError;
use core_lib::ops::approval::ApprovalStore;
use core_lib::ops::ddl_compiler;
use core_lib::ops::executor;
use core_lib::ops::index_advisor::IndexAdvisorInput;
use core_lib::ops::local_token::HEADER as OPS_TOKEN_HEADER;
use core_lib::ops::policy::OpsPolicy;
use core_lib::ops::types::{
    ActionSpec, AuditRecord, EvidencePack, ExecutionReport, OpsPlan, PlanStatus, PlanSummary,
};

use crate::is_read_only_connection;
use crate::resolve_db_client_for_request;
use crate::state::AppState;

// ── 通用 ────────────────────────────────────────────────────

pub(crate) fn authorize_write(state: &AppState, headers: &HeaderMap) -> Result<(), AppError> {
    let provided = headers
        .get(OPS_TOKEN_HEADER)
        .and_then(|value| value.to_str().ok());
    if state.ops.local_token.matches(provided) {
        Ok(())
    } else {
        Err(AppError::Forbidden(
            "缺少或错误的本机 ops 令牌（x-ops-token）".into(),
        ))
    }
}

pub(crate) async fn build_ctx(
    state: &AppState,
    db_id: Option<&str>,
) -> Result<core_lib::ops::OpsContext, AppError> {
    let (db_client, db_name) = resolve_db_client_for_request(state, db_id).await?;
    let resolved_id = match db_id {
        Some(id) => id.to_string(),
        None => state
            .config
            .read()
            .await
            .active_db_id
            .clone()
            .unwrap_or_default(),
    };
    let read_only = is_read_only_connection(state, db_id).await;
    Ok(core_lib::ops::OpsContext::new(
        db_client, resolved_id, db_name, read_only,
    ))
}

async fn plan_ctx(state: &AppState, plan: &OpsPlan) -> Result<core_lib::ops::OpsContext, AppError> {
    let db_id = if plan.db_id.trim().is_empty() {
        None
    } else {
        Some(plan.db_id.as_str())
    };
    build_ctx(state, db_id).await
}

pub(crate) fn audit_event(state: &AppState, event: &str, outcome: &str, detail: serde_json::Value) {
    let mut record = AuditRecord::new(event, outcome);
    record.actor = Some("local-user".into());
    record.detail = detail;
    if let Err(err) = state.ops.audit.append(&record) {
        tracing::warn!("ops 审计写入失败: {}", err);
    }
}

// ── 元信息 ──────────────────────────────────────────────────

#[derive(Serialize)]
pub(crate) struct SessionTokenResponse {
    token: String,
    header: String,
}

pub(crate) async fn ops_session_token(
    State(state): State<AppState>,
) -> Json<SessionTokenResponse> {
    Json(SessionTokenResponse {
        token: state.ops.local_token.token().to_string(),
        header: OPS_TOKEN_HEADER.to_string(),
    })
}

pub(crate) async fn ops_scenarios(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "scenarios": state.ops.registry.descriptors(),
    }))
}

pub(crate) async fn ops_get_policy(State(state): State<AppState>) -> Json<serde_json::Value> {
    let policy = state.ops.policy.read().await.clone();
    Json(serde_json::json!({
        "policy": policy,
        "read_only_by_default": !policy.allow_apply,
        "prod_locked": policy.is_prod() && !policy.allow_prod_apply,
    }))
}

pub(crate) async fn ops_update_policy(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(policy): Json<OpsPolicy>,
) -> Result<Json<serde_json::Value>, AppError> {
    authorize_write(&state, &headers)?;
    let environment = policy.environment.trim().to_ascii_lowercase();
    if !matches!(environment.as_str(), "dev" | "staging" | "prod" | "production") {
        return Err(AppError::BadRequest(format!(
            "未知环境 {}，仅支持 dev/staging/prod",
            policy.environment
        )));
    }
    state.ops.save_policy(&policy).await?;
    audit_event(
        &state,
        "ops.policy.updated",
        "ok",
        serde_json::json!({ "policy": policy }),
    );
    Ok(Json(serde_json::json!({ "policy": policy })))
}

// ── 分析 / 计划 ─────────────────────────────────────────────

#[derive(Deserialize)]
pub(crate) struct OpsAnalyzeRequest {
    #[serde(default)]
    pub scenario_id: Option<String>,
    #[serde(default)]
    pub db_id: Option<String>,
    pub table: String,
    #[serde(default)]
    pub workload_sql: Option<Vec<String>>,
    #[serde(default)]
    pub sql: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub use_llm: Option<bool>,
}

impl OpsAnalyzeRequest {
    fn into_input(self) -> IndexAdvisorInput {
        let mut workload = self.workload_sql.unwrap_or_default();
        if let Some(sql) = self.sql {
            workload.insert(0, sql);
        }
        IndexAdvisorInput {
            table: self.table,
            workload_sql: workload,
            notes: self.notes,
            use_llm: self.use_llm.unwrap_or(false),
        }
    }
}

#[derive(Serialize)]
pub(crate) struct OpsAnalyzeResponse {
    plan: OpsPlan,
    evidence: EvidencePack,
}

async fn run_analyze(
    state: &AppState,
    headers: &HeaderMap,
    req: OpsAnalyzeRequest,
) -> Result<OpsAnalyzeResponse, AppError> {
    authorize_write(state, headers)?;
    let scenario_id = req
        .scenario_id
        .clone()
        .unwrap_or_else(|| "mysql-index-advisor".to_string());
    let descriptor = state
        .ops
        .registry
        .descriptors()
        .iter()
        .find(|d| d.id == scenario_id)
        .ok_or_else(|| AppError::NotFound(format!("场景 {scenario_id} 不存在")))?;
    if !matches!(
        descriptor.status,
        core_lib::ops::registry::ScenarioStatus::Enabled
    ) {
        return Err(AppError::BadRequest(format!(
            "场景 {} 当前为 {:?}，阶段 0 仅开放 MySQL 索引优化",
            descriptor.id, descriptor.status
        )));
    }
    let scenario = state
        .ops
        .registry
        .find(&scenario_id)
        .ok_or_else(|| AppError::NotFound(format!("场景 {scenario_id} 未注册")))?;

    let ctx = build_ctx(state, req.db_id.as_deref()).await?;
    let input = req.into_input();
    let input_value = serde_json::to_value(&input)
        .map_err(|e| AppError::BadRequest(format!("输入序列化失败: {e}")))?;

    let evidence = scenario.collect_evidence(&ctx, &input_value).await?;
    let policy = state.ops.policy.read().await.clone();
    let mut plan = scenario.build_plan(&ctx.identity(), &evidence, &input_value, &policy)?;
    if input.use_llm {
        // 复用 ai/ 下已有模型调用栈；模型输出仍要过服务端编译校验。
        let config = state.config.read().await.clone();
        plan = core_lib::ops::ai_planner::enhance_plan(&config, &evidence, plan).await;
    }

    state.ops.plans.write().await.insert(plan.clone())?;
    audit_event(
        state,
        "ops.plan.created",
        "ok",
        serde_json::json!({
            "plan_id": plan.plan_id,
            "plan_hash": plan.plan_hash,
            "scenario_id": plan.scenario_id,
            "table": evidence.table,
            "action_count": plan.actions.len(),
            "evidence_hash": evidence.evidence_hash(),
        }),
    );

    Ok(OpsAnalyzeResponse { plan, evidence })
}

pub(crate) async fn ops_analyze(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<OpsAnalyzeRequest>,
) -> Result<Json<OpsAnalyzeResponse>, AppError> {
    Ok(Json(run_analyze(&state, &headers, req).await?))
}

pub(crate) async fn ops_analyze_stream(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<OpsAnalyzeRequest>,
) -> Result<
    Sse<std::pin::Pin<Box<dyn futures::Stream<Item = Result<Event, Infallible>> + Send>>>,
    AppError,
> {
    let result = run_analyze(&state, &headers, req).await;
    let stream = futures::stream::once(async move {
        let events = match result {
            Ok(response) => vec![
                Event::default()
                    .event("evidence")
                    .data(serde_json::to_string(&response.evidence).unwrap_or_default()),
                Event::default()
                    .event("plan")
                    .data(serde_json::to_string(&response.plan).unwrap_or_default()),
                Event::default().event("done").data("{}"),
            ],
            Err(err) => vec![Event::default()
                .event("error")
                .data(serde_json::to_string(&serde_json::json!({
                    "message": err.to_string()
                }))
                .unwrap_or_default())],
        };
        futures::stream::iter(events.into_iter().map(Ok))
    })
    .flatten();
    Ok(Sse::new(Box::pin(stream)))
}

#[derive(Deserialize)]
pub(crate) struct PlanListQuery {
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Serialize)]
pub(crate) struct PlanListResponse {
    plans: Vec<PlanSummary>,
}

pub(crate) async fn ops_list_plans(
    State(state): State<AppState>,
    Query(query): Query<PlanListQuery>,
) -> Json<PlanListResponse> {
    let limit = query.limit.unwrap_or(20).min(100);
    Json(PlanListResponse {
        plans: state.ops.plans.read().await.list(limit),
    })
}

pub(crate) async fn ops_get_plan(
    State(state): State<AppState>,
    Path(plan_id): Path<String>,
) -> Result<Json<OpsPlan>, AppError> {
    let plan = state
        .ops
        .plans
        .read()
        .await
        .get(&plan_id)
        .ok_or_else(|| AppError::NotFound(format!("计划 {plan_id} 不存在")))?;
    Ok(Json(plan))
}

// ── 审批 ────────────────────────────────────────────────────

#[derive(Deserialize)]
pub(crate) struct OpsApproveRequest {
    pub plan_hash: String,
    #[serde(default)]
    pub action_ids: Vec<String>,
}

#[derive(Serialize)]
pub(crate) struct OpsApproveResponse {
    approval_id: String,
    approval_token: String,
    expires_at: i64,
    plan_hash: String,
    action_ids: Vec<String>,
}

pub(crate) async fn ops_approve(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(plan_id): Path<String>,
    Json(req): Json<OpsApproveRequest>,
) -> Result<Json<OpsApproveResponse>, AppError> {
    authorize_write(&state, &headers)?;
    let plan = state
        .ops
        .plans
        .read()
        .await
        .get(&plan_id)
        .ok_or_else(|| AppError::NotFound(format!("计划 {plan_id} 不存在")))?;

    if req.plan_hash.trim() != plan.plan_hash {
        return Err(AppError::Forbidden(
            "计划哈希不匹配：计划内容可能已变化，请重新分析".into(),
        ));
    }
    if !matches!(plan.status, PlanStatus::PendingApproval) {
        return Err(AppError::BadRequest(format!(
            "计划当前状态 {:?}，不能重复审批",
            plan.status
        )));
    }
    if req.action_ids.is_empty() {
        return Err(AppError::BadRequest("请至少选择一个动作".into()));
    }

    // 审批前做一次实时 schema 编译校验，防止审批后结构漂移。
    let ctx = plan_ctx(&state, &plan).await?;
    let table = plan
        .evidence_summary
        .as_ref()
        .map(|s| s.table.clone())
        .ok_or_else(|| AppError::BadRequest("计划缺少表信息".into()))?;
    let snapshot = executor::refresh_snapshot(&ctx, &table).await?;
    let actions = req
        .action_ids
        .iter()
        .map(|id| {
            plan.action_by_id(id)
                .cloned()
                .ok_or_else(|| AppError::BadRequest(format!("动作 {id} 不属于计划")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    ddl_compiler::compile_actions(&actions, &snapshot)?;

    let policy = state.ops.policy.read().await.clone();
    let (token, record) = {
        let mut approvals = state.ops.approvals.write().await;
        approvals.issue(&plan, &req.action_ids, policy.approval_ttl_secs, Some("local-user".into()))?
    };

    {
        let mut plans = state.ops.plans.write().await;
        let mut updated = plans
            .get(&plan_id)
            .ok_or_else(|| AppError::NotFound(format!("计划 {plan_id} 不存在")))?;
        updated.status = PlanStatus::Approved;
        plans.update(updated)?;
    }

    audit_event(
        &state,
        "ops.plan.approved",
        "ok",
        serde_json::json!({
            "plan_id": plan.plan_id,
            "plan_hash": plan.plan_hash,
            "approval_id": record.approval_id,
            "action_ids": req.action_ids,
            "expires_at": record.expires_at,
        }),
    );

    Ok(Json(OpsApproveResponse {
        approval_id: record.approval_id,
        approval_token: token,
        expires_at: record.expires_at,
        plan_hash: plan.plan_hash,
        action_ids: req.action_ids,
    }))
}

// ── 执行 / 回滚 ─────────────────────────────────────────────

#[derive(Deserialize)]
pub(crate) struct OpsExecuteRequest {
    #[serde(default)]
    pub action_ids: Option<Vec<String>>,
    #[serde(default)]
    pub approval_token: Option<String>,
    #[serde(default)]
    pub dry_run: Option<bool>,
}

#[derive(Serialize)]
pub(crate) struct OpsExecuteResponse {
    plan: OpsPlan,
    report: ExecutionReport,
}

pub(crate) async fn ops_execute(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(plan_id): Path<String>,
    Json(req): Json<OpsExecuteRequest>,
) -> Result<Json<OpsExecuteResponse>, AppError> {
    authorize_write(&state, &headers)?;
    let plan = state
        .ops
        .plans
        .read()
        .await
        .get(&plan_id)
        .ok_or_else(|| AppError::NotFound(format!("计划 {plan_id} 不存在")))?;
    let dry_run = req.dry_run.unwrap_or(false);
    let action_ids = req.action_ids.unwrap_or_else(|| {
        plan.actions.iter().map(|action| action.action_id()).collect()
    });
    if action_ids.is_empty() {
        return Err(AppError::BadRequest("计划没有可执行动作".into()));
    }

    let ctx = plan_ctx(&state, &plan).await?;
    let policy = state.ops.policy.read().await.clone();

    if dry_run {
        let report =
            executor::dry_run(&ctx, &plan, &action_ids, &policy, &state.ops.audit).await?;
        return Ok(Json(OpsExecuteResponse { plan, report }));
    }

    if !matches!(plan.status, PlanStatus::Approved) {
        return Err(AppError::Forbidden(format!(
            "计划状态 {:?} 未批准，禁止执行",
            plan.status
        )));
    }
    let token = req
        .approval_token
        .as_deref()
        .ok_or_else(|| AppError::Forbidden("缺少审批令牌 approval_token".into()))?;

    let execution = {
        let mut approvals: tokio::sync::RwLockWriteGuard<'_, ApprovalStore> =
            state.ops.approvals.write().await;
        executor::execute(
            &ctx,
            &plan,
            &action_ids,
            &mut approvals,
            token,
            &policy,
            &state.ops.audit,
        )
        .await
    };

    match execution {
        Ok(report) => {
            let failed = report.error.is_some();
            let updated_plan = {
                let mut plans = state.ops.plans.write().await;
                let mut updated = plans
                    .get(&plan_id)
                    .ok_or_else(|| AppError::NotFound(format!("计划 {plan_id} 不存在")))?;
                updated.status = if failed {
                    PlanStatus::Failed
                } else {
                    PlanStatus::Executed
                };
                updated.execution = Some(report.clone());
                plans.update(updated.clone())?;
                updated
            };
            if failed {
                audit_event(
                    &state,
                    "ops.execute.failed",
                    "failed",
                    serde_json::json!({
                        "plan_id": plan_id,
                        "error": report.error,
                        "rollback_performed": report.rollback_performed,
                    }),
                );
            }
            Ok(Json(OpsExecuteResponse {
                plan: updated_plan,
                report,
            }))
        }
        Err(err) => {
            if matches!(err, core_lib::ops::OpsError::Db(_)) {
                let mut plans = state.ops.plans.write().await;
                if let Some(mut updated) = plans.get(&plan_id) {
                    updated.status = PlanStatus::Failed;
                    let _ = plans.update(updated);
                }
                audit_event(
                    &state,
                    "ops.execute.failed",
                    "failed",
                    serde_json::json!({ "plan_id": plan_id, "error": err.to_string() }),
                );
            }
            Err(err.into())
        }
    }
}

#[derive(Deserialize)]
pub(crate) struct OpsRollbackRequest {
    pub reason: String,
}

pub(crate) async fn ops_rollback(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(plan_id): Path<String>,
    Json(req): Json<OpsRollbackRequest>,
) -> Result<Json<OpsExecuteResponse>, AppError> {
    authorize_write(&state, &headers)?;
    let plan = state
        .ops
        .plans
        .read()
        .await
        .get(&plan_id)
        .ok_or_else(|| AppError::NotFound(format!("计划 {plan_id} 不存在")))?;
    if !matches!(plan.status, PlanStatus::Executed | PlanStatus::Failed) {
        return Err(AppError::BadRequest(format!(
            "计划状态 {:?} 不允许回滚",
            plan.status
        )));
    }
    let ctx = plan_ctx(&state, &plan).await?;
    let policy = state.ops.policy.read().await.clone();
    let report = executor::rollback(&ctx, &plan, &req.reason, &policy, &state.ops.audit).await?;

    let updated_plan = {
        let mut plans = state.ops.plans.write().await;
        let mut updated = plans
            .get(&plan_id)
            .ok_or_else(|| AppError::NotFound(format!("计划 {plan_id} 不存在")))?;
        updated.status = PlanStatus::RolledBack;
        updated.execution = Some(report.clone());
        plans.update(updated.clone())?;
        updated
    };

    audit_event(
        &state,
        "ops.plan.rolled_back",
        "ok",
        serde_json::json!({ "plan_id": plan_id, "reason": req.reason }),
    );
    Ok(Json(OpsExecuteResponse {
        plan: updated_plan,
        report,
    }))
}

// ── 审计 / 审批记录 ─────────────────────────────────────────

#[derive(Deserialize)]
pub(crate) struct AuditQuery {
    #[serde(default)]
    pub limit: Option<usize>,
}

pub(crate) async fn ops_audit(
    State(state): State<AppState>,
    Query(query): Query<AuditQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let limit = query.limit.unwrap_or(100).min(500);
    let records = state.ops.audit.recent(limit)?;
    Ok(Json(serde_json::json!({ "records": records })))
}

pub(crate) async fn ops_approvals(
    State(state): State<AppState>,
) -> Json<serde_json::Value> {
    let records = state.ops.approvals.read().await.list_recent(50);
    Json(serde_json::json!({ "approvals": records }))
}

// 明确不在 ops 链路里复用 DdlEngine / execute_ddl；编译入口只允许 ddl_compiler。
const _: fn(&ActionSpec) = |_| {};
