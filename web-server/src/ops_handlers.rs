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
use core_lib::ops::field_migration::backfill::{self, PreparedBackfill};
use core_lib::ops::field_migration::migration_store::{
    BackfillConfig, IndexJob, MigrationJob, MigrationPhase, MigrationStatus, NotNullJob,
    TypeChangeJob, CONTROL_ABORT, CONTROL_PAUSE, CONTROL_RUN,
};
use core_lib::ops::field_migration::types::FieldGovernanceAnalysis;
use core_lib::ops::field_migration::{pg_index, pg_notnull, pg_typechange};
use core_lib::ops::index_advisor::IndexAdvisorInput;
use core_lib::ops::local_token::HEADER as OPS_TOKEN_HEADER;
use core_lib::ops::perf_diagnostics::{self, SlowQueryScan};
use core_lib::ops::policy::OpsPolicy;
use core_lib::ops::types::{
    ActionSpec, AuditRecord, EvidencePack, ExecutionReport, OpsPlan, PlanStatus, PlanSummary,
};
use core_lib::ops::OpsContext;

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
    session_id: Option<String>,
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
    Ok(
        core_lib::ops::OpsContext::new(db_client, resolved_id, db_name, read_only).with_audit(
            Some(std::sync::Arc::new(state.ops.audit.clone())),
            session_id,
        ),
    )
}

/// 请求级会话标识：优先使用调用方提供的 x-request-id，否则生成一次请求 ID。
fn session_id(headers: &HeaderMap) -> String {
    headers
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("web-{}", uuid::Uuid::new_v4()))
}

async fn plan_ctx(state: &AppState, plan: &OpsPlan) -> Result<core_lib::ops::OpsContext, AppError> {
    let db_id = if plan.db_id.trim().is_empty() {
        None
    } else {
        Some(plan.db_id.as_str())
    };
    build_ctx(state, db_id, None).await
}

/// 场景级引擎守卫。
///
/// PostgreSQL 在本轮只开放只读场景（巡检、性能诊断、索引优化只读分析）；
/// 字段治理的写路径仍是 MySQL 方言，明确拒绝并给出原因，避免入口点进去才报错。
fn ensure_scenario_engine(
    ctx: &core_lib::ops::OpsContext,
    scenario_id: &str,
) -> Result<(), AppError> {
    scenario_engine_decision(ctx.is_postgres(), scenario_id).map_err(AppError::BadRequest)
}

fn scenario_engine_decision(is_postgres: bool, scenario_id: &str) -> Result<(), String> {
    if !is_postgres {
        return Ok(());
    }
    match scenario_id {
        "mysql-index-advisor" | "perf-diagnostics" | "periodic-inspection" => Ok(()),
        // S3 起 PG 字段治理（证据/风险/脚本 + 回填/DDL 执行器）已打通。
        "field-governance" => Ok(()),
        other => Err(format!(
            "PostgreSQL 暂不支持场景 {other}；当前开放：巡检、性能诊断、索引优化（只读分析）、字段治理。"
        )),
    }
}

/// PG 写动作不在本轮范围：执行/回滚入口显式拒绝，不依赖“计划恰好为空”。
fn ensure_write_engine(ctx: &core_lib::ops::OpsContext, operation: &str) -> Result<(), AppError> {
    write_engine_decision(ctx.is_postgres(), operation).map_err(AppError::BadRequest)
}

fn write_engine_decision(is_postgres: bool, operation: &str) -> Result<(), String> {
    if is_postgres {
        return Err(format!(
            "PostgreSQL 的 {operation} 不在本轮范围：PG 场景保持只读，不执行写动作。请切换到 MySQL/MariaDB 连接。"
        ));
    }
    Ok(())
}

pub(crate) fn audit_event(state: &AppState, event: &str, outcome: &str, detail: serde_json::Value) {
    let mut record = AuditRecord::new(event, outcome);
    record.actor = Some("local-user".into());
    // 审计文件同样不允许出现连接串/凭据形态。
    let mut detail = detail;
    core_lib::ops::inspection::scrub_json(&mut detail, &[]);
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

pub(crate) async fn ops_session_token(State(state): State<AppState>) -> Json<SessionTokenResponse> {
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
    if !matches!(
        environment.as_str(),
        "dev" | "staging" | "prod" | "production"
    ) {
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
    /// 索引分析场景必填；性能诊断场景不需要。
    #[serde(default)]
    pub table: Option<String>,
    #[serde(default)]
    pub workload_sql: Option<Vec<String>>,
    #[serde(default)]
    pub sql: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub use_llm: Option<bool>,
    /// 性能诊断：performance_schema digest（64 位 hex）。
    #[serde(default)]
    pub digest: Option<String>,
    /// 性能诊断：从 digest 关联到的可执行 SQL 样本。
    #[serde(default)]
    pub sample_sql: Option<String>,
    /// 性能诊断：是否真实执行 EXPLAIN ANALYZE。
    #[serde(default)]
    pub explain_analyze: Option<bool>,
    /// 性能诊断：EXPLAIN ANALYZE 会话超时，默认 5000ms。
    #[serde(default)]
    pub explain_analyze_timeout_ms: Option<u64>,
    /// 字段治理：源列；为空表示整表字段盘点。
    #[serde(default)]
    pub column: Option<String>,
    /// 字段治理：目标表/列映射/排序规则/residual JSON。
    #[serde(default)]
    pub target: Option<serde_json::Value>,
    /// 字段治理：用户语义描述数组（带 ID 的未验证声明）。
    #[serde(default)]
    pub declarations: Option<serde_json::Value>,
    /// 字段治理：auto / full / sampled。
    #[serde(default)]
    pub profile_mode: Option<String>,
    /// 字段治理：sampled 模式千分比。
    #[serde(default)]
    pub sample_per_mille: Option<u32>,
    /// 字段治理：空串策略 preserve / to_null / block；缺省 preserve。
    #[serde(default)]
    pub empty_string_policy: Option<core_lib::ops::field_migration::types::EmptyStringPolicy>,
    /// 字段治理 S3：观察期起始日期（YYYY-MM-DD），用于下线准备清单提示。
    #[serde(default)]
    pub observation_started_at: Option<String>,
}

impl OpsAnalyzeRequest {
    fn into_input(self, scenario_id: &str) -> Result<serde_json::Value, AppError> {
        if scenario_id == "perf-diagnostics" {
            return Ok(serde_json::json!({
                "sql": self.sql,
                "digest": self.digest,
                "sample_sql": self.sample_sql,
                "notes": self.notes,
                "explain_analyze": self.explain_analyze.unwrap_or(false),
                "explain_analyze_timeout_ms": self.explain_analyze_timeout_ms,
                "use_llm": self.use_llm.unwrap_or(false),
            }));
        }
        if scenario_id == "field-governance" {
            let table = self
                .table
                .filter(|t| !t.trim().is_empty())
                .ok_or_else(|| AppError::BadRequest("字段治理缺少 table".into()))?;
            return Ok(serde_json::json!({
                "table": table,
                "column": self.column,
                "target": self.target,
                "declarations": self.declarations,
                "profile_mode": self.profile_mode,
                "sample_per_mille": self.sample_per_mille,
                "empty_string_policy": self.empty_string_policy,
                "observation_started_at": self.observation_started_at,
                "notes": self.notes,
            }));
        }
        let table = self
            .table
            .filter(|t| !t.trim().is_empty())
            .ok_or_else(|| AppError::BadRequest("索引分析缺少 table".into()))?;
        let mut workload = self.workload_sql.unwrap_or_default();
        if let Some(sql) = self.sql {
            workload.insert(0, sql);
        }
        serde_json::to_value(IndexAdvisorInput {
            table,
            workload_sql: workload,
            notes: self.notes,
            use_llm: self.use_llm.unwrap_or(false),
        })
        .map_err(|e| AppError::BadRequest(format!("输入序列化失败: {e}")))
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
            "场景 {} 当前为 {:?}，尚未开放",
            descriptor.id, descriptor.status
        )));
    }
    let scenario = state
        .ops
        .registry
        .find(&scenario_id)
        .ok_or_else(|| AppError::NotFound(format!("场景 {scenario_id} 未注册")))?;

    let use_llm = req.use_llm.unwrap_or(false);
    let db_id = req.db_id.clone();
    // 先读策略，让需要在证据采集阶段使用阈值的场景（字段治理）拿到生效值。
    let policy = state.ops.policy.read().await.clone();
    let ctx = build_ctx(state, db_id.as_deref(), Some(session_id(headers)))
        .await?
        .with_policy(std::sync::Arc::new(policy.clone()));
    ensure_scenario_engine(&ctx, &scenario_id)?;
    let input_value = req.into_input(&scenario_id)?;

    let evidence = scenario.collect_evidence(&ctx, &input_value).await?;
    let mut plan = scenario.build_plan(&ctx.identity(), &evidence, &input_value, &policy)?;
    if use_llm {
        // 复用 ai/ 下已有模型调用栈；模型输出仍要过服务端编译校验。
        let config = state.config.read().await.clone();
        plan = core_lib::ops::ai_planner::enhance_plan(&config, &evidence, plan).await;
    }

    state.ops.plans.write().await.insert(plan.clone())?;
    if plan.scenario_id == "field-governance" {
        // S2 启动回填时需要在服务端取回完整分析证据；只保留字段治理场景，避免无界增长。
        state
            .ops
            .evidence
            .write()
            .await
            .insert(plan.plan_id.clone(), evidence.clone());
    }
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

#[derive(Deserialize)]
pub(crate) struct FieldDiffPageBody {
    #[serde(default)]
    pub db_id: Option<String>,
    #[serde(flatten)]
    pub request: core_lib::ops::field_migration::diff::DiffPageRequest,
}

/// 字段治理 V2/V3/V4 差异主键分页（只读；返回总数与当前页主键）。
pub(crate) async fn ops_field_diff_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<FieldDiffPageBody>,
) -> Result<Json<core_lib::ops::field_migration::diff::DiffPage>, AppError> {
    authorize_write(&state, &headers)?;
    let ctx = build_ctx(&state, body.db_id.as_deref(), Some(session_id(&headers))).await?;
    ensure_scenario_engine(&ctx, "field-governance")?;
    let page = core_lib::ops::field_migration::diff::fetch_page(&ctx, &body.request).await?;
    Ok(Json(page))
}

// ── S2 字段回填 ────────────────────────────────────────────

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct FieldBackfillOptions {
    #[serde(default)]
    pub batch_size: Option<u64>,
    #[serde(default)]
    pub batch_interval_ms: Option<u64>,
    #[serde(default)]
    pub max_rows_per_second: Option<u64>,
    /// 仅 PostgreSQL：整任务单事务，失败整体回滚且不可续跑。
    #[serde(default)]
    pub single_transaction: Option<bool>,
    #[serde(default)]
    pub production_confirmed: Option<bool>,
    #[serde(default)]
    pub confirmed_high_risks: Vec<String>,
}

impl FieldBackfillOptions {
    fn config(&self) -> BackfillConfig {
        let defaults = BackfillConfig::default();
        BackfillConfig {
            batch_size: self.batch_size.unwrap_or(defaults.batch_size),
            batch_interval_ms: self.batch_interval_ms.unwrap_or(defaults.batch_interval_ms),
            max_rows_per_second: self
                .max_rows_per_second
                .unwrap_or(defaults.max_rows_per_second),
            single_transaction: self
                .single_transaction
                .unwrap_or(defaults.single_transaction),
        }
    }

    fn production_confirmed(&self) -> bool {
        self.production_confirmed.unwrap_or(false)
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct FieldBackfillApproveBody {
    /// 正常启动时必须提供；按 job_id 续跑时可省略。
    #[serde(default)]
    pub plan_id: Option<String>,
    /// 仅续跑场景：直接按既有任务签发令牌，无需内存中的分析证据。
    #[serde(default)]
    pub job_id: Option<String>,
    #[serde(default)]
    pub plan_hash: Option<String>,
    #[serde(flatten)]
    pub options: FieldBackfillOptions,
}

#[derive(Debug, Deserialize)]
pub(crate) struct FieldBackfillStartBody {
    pub plan_id: String,
    pub approval_token: String,
    #[serde(default)]
    pub plan_hash: Option<String>,
    #[serde(flatten)]
    pub options: FieldBackfillOptions,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct FieldBackfillResumeBody {
    #[serde(default)]
    pub approval_token: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct FieldBackfillListQuery {
    #[serde(default)]
    pub plan_id: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize)]
pub(crate) struct FieldBackfillApproveResponse {
    pub approval_id: String,
    pub token: String,
    pub expires_at: i64,
    pub migration_hash: String,
    pub plan_id: String,
    pub plan_hash: String,
    pub config: BackfillConfig,
    pub high_risks: Vec<core_lib::ops::field_migration::types::FieldRisk>,
}

#[derive(Debug, Serialize)]
pub(crate) struct FieldBackfillListResponse {
    pub jobs: Vec<MigrationJob>,
}

async fn prepare_field_backfill(
    state: &AppState,
    headers: &HeaderMap,
    plan_id: &str,
    plan_hash: Option<&str>,
    options: &FieldBackfillOptions,
    require_confirmations: bool,
) -> Result<(OpsPlan, PreparedBackfill, OpsContext, OpsPolicy, bool), AppError> {
    let plan = state
        .ops
        .plans
        .read()
        .await
        .get(plan_id)
        .ok_or_else(|| AppError::BadRequest(format!("计划 {plan_id} 不存在或已过期")))?;
    if plan.scenario_id != "field-governance" {
        return Err(AppError::BadRequest(
            "该计划不是字段治理计划，不能启动回填".into(),
        ));
    }
    if let Some(expected_hash) = plan_hash {
        if expected_hash != plan.plan_hash {
            return Err(AppError::BadRequest(
                "plan_hash 与当前计划不一致，请重新分析".into(),
            ));
        }
    }
    let evidence = state
        .ops
        .evidence
        .read()
        .await
        .get(plan_id)
        .cloned()
        .ok_or_else(|| {
            AppError::BadRequest(
                "服务端未保留该计划的完整分析证据（可能重启过），请重新执行只读分析".into(),
            )
        })?;
    let analysis: FieldGovernanceAnalysis = evidence
        .extra
        .clone()
        .ok_or_else(|| AppError::BadRequest("计划缺少字段治理扩展证据".into()))
        .and_then(|value| {
            serde_json::from_value(value)
                .map_err(|err| AppError::BadRequest(format!("扩展证据解析失败: {err}")))
        })?;
    let db_id = if plan.db_id.trim().is_empty() {
        None
    } else {
        Some(plan.db_id.as_str())
    };
    let ctx = build_ctx(state, db_id, Some(session_id(headers))).await?;
    ensure_scenario_engine(&ctx, "field-governance")?;
    if ctx.db_name != plan.db_name {
        return Err(AppError::BadRequest(format!(
            "当前连接库 {} 与计划库 {} 不一致",
            ctx.db_name, plan.db_name
        )));
    }
    let policy = state.ops.policy.read().await.clone();
    let production_confirmed = options.production_confirmed();
    policy.authorize_backfill(ctx.is_read_only, production_confirmed)?;
    let prepared = backfill::prepare(&plan, &analysis, &policy, options.config())?;
    if require_confirmations {
        let missing = prepared
            .high_risks
            .iter()
            .filter(|risk| !options.confirmed_high_risks.iter().any(|id| id == &risk.id))
            .map(|risk| risk.id.clone())
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            return Err(AppError::BadRequest(format!(
                "以下 HIGH 风险未确认，禁止回填: {}",
                missing.join(", ")
            )));
        }
    }
    Ok((plan, prepared, ctx, policy, production_confirmed))
}

pub(crate) async fn ops_field_backfill_approve(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<FieldBackfillApproveBody>,
) -> Result<Json<FieldBackfillApproveResponse>, AppError> {
    authorize_write(&state, &headers)?;
    let session = session_id(&headers);
    if let Some(job_id) = body.job_id.as_deref().filter(|id| !id.trim().is_empty()) {
        // 续跑令牌按既有任务签发：不依赖进程内的分析证据，服务重启后仍可用；
        // 批参数直接取 job.config，续跑与首发不会出现参数分叉。
        let job = state.ops.migrations.get(job_id).await?;
        if !matches!(
            job.status,
            MigrationStatus::Interrupted | MigrationStatus::Paused | MigrationStatus::Blocked
        ) {
            return Err(AppError::BadRequest(format!(
                "任务 {} 当前状态为 {:?}，无需续跑令牌",
                job.job_id, job.status
            )));
        }
        let plan = state
            .ops
            .plans
            .read()
            .await
            .get(&job.plan_id)
            .ok_or_else(|| AppError::BadRequest(format!("计划 {} 不存在或已过期", job.plan_id)))?;
        if plan.plan_hash != job.plan_hash {
            return Err(AppError::BadRequest(
                "任务绑定的计划哈希已变化，禁止续跑".into(),
            ));
        }
        let policy = state.ops.policy.read().await.clone();
        policy.authorize_backfill(false, job.production_confirmed)?;
        let (token, record) = state.ops.approvals.write().await.issue_field(
            &plan,
            &job.migration_hash,
            "backfill",
            policy.approval_ttl_secs,
            Some(session.clone()),
        )?;
        audit_event(
            &state,
            "field_migration.backfill.approved",
            "ok",
            serde_json::json!({
                "approval_id": record.approval_id,
                "job_id": job.job_id,
                "plan_id": job.plan_id,
                "migration_hash": job.migration_hash,
                "resume": true,
                "batch_size": job.config.batch_size,
                "batch_interval_ms": job.config.batch_interval_ms,
                "max_rows_per_second": job.config.max_rows_per_second,
            }),
        );
        return Ok(Json(FieldBackfillApproveResponse {
            approval_id: record.approval_id,
            token,
            expires_at: record.expires_at,
            migration_hash: job.migration_hash,
            plan_id: job.plan_id,
            plan_hash: job.plan_hash,
            config: job.config,
            high_risks: Vec::new(),
        }));
    }
    let plan_id = body
        .plan_id
        .as_deref()
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| AppError::BadRequest("缺少 plan_id（或续跑时提供 job_id）".into()))?;
    let (plan, prepared, ctx, policy, production_confirmed) = prepare_field_backfill(
        &state,
        &headers,
        plan_id,
        body.plan_hash.as_deref(),
        &body.options,
        true,
    )
    .await?;
    let (token, record) = state.ops.approvals.write().await.issue_field(
        &plan,
        &prepared.migration_hash,
        "backfill",
        policy.approval_ttl_secs,
        Some(session.clone()),
    )?;
    audit_event(
        &state,
        "field_migration.backfill.approved",
        "ok",
        serde_json::json!({
            "approval_id": record.approval_id,
            "plan_id": plan.plan_id,
            "plan_hash": plan.plan_hash,
            "migration_hash": prepared.migration_hash,
            "db_id": ctx.db_id,
            "db_name": ctx.db_name,
            "production_confirmed": production_confirmed,
            "batch_size": prepared.config.batch_size,
            "batch_interval_ms": prepared.config.batch_interval_ms,
            "max_rows_per_second": prepared.config.max_rows_per_second,
            "high_risks": prepared.high_risks.iter().map(|r| r.id.clone()).collect::<Vec<_>>(),
        }),
    );
    Ok(Json(FieldBackfillApproveResponse {
        approval_id: record.approval_id,
        token,
        expires_at: record.expires_at,
        migration_hash: prepared.migration_hash,
        plan_id: plan.plan_id,
        plan_hash: plan.plan_hash,
        config: prepared.config,
        high_risks: prepared.high_risks,
    }))
}

pub(crate) async fn ops_field_backfill_start(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<FieldBackfillStartBody>,
) -> Result<Json<MigrationJob>, AppError> {
    authorize_write(&state, &headers)?;
    let (plan, prepared, ctx, policy, production_confirmed) = prepare_field_backfill(
        &state,
        &headers,
        &body.plan_id,
        body.plan_hash.as_deref(),
        &body.options,
        false,
    )
    .await?;
    let record = state.ops.approvals.write().await.consume_field(
        &body.approval_token,
        &plan,
        &prepared.migration_hash,
        "backfill",
    )?;

    let active = state
        .ops
        .migrations
        .list(Some(&plan.plan_id), 50)
        .await
        .into_iter()
        .find(|job| job.is_active_status());
    if let Some(job) = active {
        return Err(AppError::BadRequest(format!(
            "该计划已有运行中的回填任务 {}（状态 {}）",
            job.job_id,
            job.status.as_str()
        )));
    }

    let mut job = backfill::new_job(&plan, &prepared, production_confirmed);
    job.approval_id = Some(record.approval_id.clone());
    let job_id = job.job_id.clone();
    state.ops.migrations.insert(job.clone()).await?;
    let hub = state.ops.migrations.clone();
    hub.set_control(&job_id, CONTROL_RUN).await;
    hub.mark_active(&job_id).await;
    audit_event(
        &state,
        "field_migration.backfill.started",
        "ok",
        serde_json::json!({
            "job_id": job_id,
            "plan_id": plan.plan_id,
            "approval_id": record.approval_id,
            "migration_hash": prepared.migration_hash,
            "batch_size": prepared.config.batch_size,
        }),
    );
    tokio::spawn(async move {
        backfill::run_job(job_id, ctx, hub, policy).await;
    });
    Ok(Json(job))
}

pub(crate) async fn ops_field_backfill_status(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Result<Json<MigrationJob>, AppError> {
    let hub = state.ops.migrations.clone();
    let mut job = hub.get(&job_id).await?;
    if job.status == MigrationStatus::Running && !hub.is_active(&job_id).await {
        job = hub
            .update(&job_id, |job| {
                job.status = MigrationStatus::Interrupted;
                job.phase = MigrationPhase::Interrupted;
                job.last_error = Some("后台任务已停止，可从 checkpoint 续跑".into());
            })
            .await?;
    }
    Ok(Json(job))
}

pub(crate) async fn ops_field_backfill_list(
    State(state): State<AppState>,
    Query(query): Query<FieldBackfillListQuery>,
) -> Result<Json<FieldBackfillListResponse>, AppError> {
    let limit = query.limit.unwrap_or(20).clamp(1, 100);
    let jobs = state
        .ops
        .migrations
        .list(query.plan_id.as_deref(), limit)
        .await;
    Ok(Json(FieldBackfillListResponse { jobs }))
}

// ── S3：PG SET NOT NULL / CONCURRENTLY 索引 ──────────────

#[derive(Debug, Deserialize)]
pub(crate) struct FieldNotNullBody {
    #[serde(default)]
    pub db_id: Option<String>,
    #[serde(default)]
    pub schema: Option<String>,
    pub table: String,
    pub column: String,
    #[serde(default)]
    pub production_confirmed: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct FieldIndexBody {
    #[serde(default)]
    pub db_id: Option<String>,
    #[serde(default)]
    pub schema: Option<String>,
    pub table: String,
    pub index_name: String,
    pub columns: Vec<String>,
    #[serde(default)]
    pub unique: Option<bool>,
    #[serde(default)]
    pub production_confirmed: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct FieldDdlListQuery {
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize)]
pub(crate) struct FieldNotNullListResponse {
    pub jobs: Vec<NotNullJob>,
}

#[derive(Debug, Serialize)]
pub(crate) struct FieldIndexListResponse {
    pub jobs: Vec<IndexJob>,
}

pub(crate) async fn ops_field_notnull_create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<FieldNotNullBody>,
) -> Result<Json<NotNullJob>, AppError> {
    authorize_write(&state, &headers)?;
    let ctx = build_ctx(&state, body.db_id.as_deref(), Some(session_id(&headers))).await?;
    ensure_scenario_engine(&ctx, "field-governance")?;
    let policy = state.ops.policy.read().await.clone();
    let production_confirmed = body.production_confirmed.unwrap_or(false);
    policy.authorize_backfill(ctx.is_read_only, production_confirmed)?;
    let job = pg_notnull::new_job(
        &ctx,
        &state.ops.migrations,
        pg_notnull::NotNullRequest {
            schema: body.schema,
            table: body.table,
            column: body.column,
            production_confirmed,
        },
    )
    .await?;
    audit_event(
        &state,
        "field_migration.notnull.created",
        "ok",
        serde_json::json!({ "job_id": job.job_id, "table": job.table, "column": job.column }),
    );
    Ok(Json(job))
}

pub(crate) async fn ops_field_notnull_run(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(job_id): Path<String>,
) -> Result<Json<NotNullJob>, AppError> {
    authorize_write(&state, &headers)?;
    let hub = state.ops.migrations.clone();
    let job = hub.get_notnull(&job_id).await?;
    if hub.is_active(&job_id).await {
        return Err(AppError::BadRequest(format!("任务 {job_id} 正在运行")));
    }
    let ctx = build_ctx(&state, Some(&job.db_id), Some(session_id(&headers))).await?;
    ensure_scenario_engine(&ctx, "field-governance")?;
    let policy = state.ops.policy.read().await.clone();
    policy.authorize_backfill(ctx.is_read_only, job.production_confirmed)?;
    hub.set_control(&job_id, CONTROL_RUN).await;
    hub.mark_active(&job_id).await;
    let spawn_hub = hub.clone();
    let spawn_id = job_id.clone();
    tokio::spawn(async move {
        if let Err(err) = pg_notnull::run_job(&spawn_id, &ctx, &spawn_hub, &policy).await {
            let _ = spawn_hub
                .update_notnull(&spawn_id, |job| {
                    job.status = MigrationStatus::Interrupted;
                    job.last_error = Some(err.to_string());
                })
                .await;
        }
        spawn_hub.mark_inactive(&spawn_id).await;
    });
    Ok(Json(job))
}

pub(crate) async fn ops_field_notnull_status(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Result<Json<NotNullJob>, AppError> {
    let hub = state.ops.migrations.clone();
    let mut job = hub.get_notnull(&job_id).await?;
    if job.status == MigrationStatus::Running && !hub.is_active(&job_id).await {
        job = hub
            .update_notnull(&job_id, |job| {
                job.status = MigrationStatus::Interrupted;
                job.last_error = Some("后台任务已停止，可从当前阶段续跑".into());
            })
            .await?;
    }
    Ok(Json(job))
}

pub(crate) async fn ops_field_notnull_list(
    State(state): State<AppState>,
    Query(query): Query<FieldDdlListQuery>,
) -> Result<Json<FieldNotNullListResponse>, AppError> {
    let limit = query.limit.unwrap_or(20).clamp(1, 100);
    let jobs = state.ops.migrations.list_notnull(limit).await;
    Ok(Json(FieldNotNullListResponse { jobs }))
}

pub(crate) async fn ops_field_index_create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<FieldIndexBody>,
) -> Result<Json<IndexJob>, AppError> {
    authorize_write(&state, &headers)?;
    let ctx = build_ctx(&state, body.db_id.as_deref(), Some(session_id(&headers))).await?;
    ensure_scenario_engine(&ctx, "field-governance")?;
    let policy = state.ops.policy.read().await.clone();
    let production_confirmed = body.production_confirmed.unwrap_or(false);
    policy.authorize_backfill(ctx.is_read_only, production_confirmed)?;
    let job = pg_index::new_job(
        &ctx,
        &state.ops.migrations,
        pg_index::IndexRequest {
            schema: body.schema,
            table: body.table,
            index_name: body.index_name,
            columns: body.columns,
            unique: body.unique.unwrap_or(false),
            production_confirmed,
        },
    )
    .await?;
    audit_event(
        &state,
        "field_migration.index.created",
        "ok",
        serde_json::json!({ "job_id": job.job_id, "index": job.spec.index_name, "table": job.spec.table }),
    );
    Ok(Json(job))
}

pub(crate) async fn ops_field_index_run(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(job_id): Path<String>,
) -> Result<Json<IndexJob>, AppError> {
    authorize_write(&state, &headers)?;
    let hub = state.ops.migrations.clone();
    let job = hub.get_index(&job_id).await?;
    if hub.is_active(&job_id).await {
        return Err(AppError::BadRequest(format!("任务 {job_id} 正在运行")));
    }
    let ctx = build_ctx(&state, Some(&job.db_id), Some(session_id(&headers))).await?;
    ensure_scenario_engine(&ctx, "field-governance")?;
    let policy = state.ops.policy.read().await.clone();
    policy.authorize_backfill(ctx.is_read_only, job.production_confirmed)?;
    hub.set_control(&job_id, CONTROL_RUN).await;
    hub.mark_active(&job_id).await;
    let spawn_hub = hub.clone();
    let spawn_id = job_id.clone();
    tokio::spawn(async move {
        if let Err(err) = pg_index::run_job(&spawn_id, &ctx, &spawn_hub, &policy).await {
            let _ = spawn_hub
                .update_index(&spawn_id, |job| {
                    job.status = MigrationStatus::Interrupted;
                    job.last_error = Some(err.to_string());
                })
                .await;
        }
        spawn_hub.mark_inactive(&spawn_id).await;
    });
    Ok(Json(job))
}

pub(crate) async fn ops_field_index_status(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Result<Json<IndexJob>, AppError> {
    let hub = state.ops.migrations.clone();
    let mut job = hub.get_index(&job_id).await?;
    if job.status == MigrationStatus::Running && !hub.is_active(&job_id).await {
        job = hub
            .update_index(&job_id, |job| {
                job.status = MigrationStatus::Interrupted;
                job.last_error = Some("后台任务已停止，可重新执行".into());
            })
            .await?;
    }
    Ok(Json(job))
}

pub(crate) async fn ops_field_index_list(
    State(state): State<AppState>,
    Query(query): Query<FieldDdlListQuery>,
) -> Result<Json<FieldIndexListResponse>, AppError> {
    let limit = query.limit.unwrap_or(20).clamp(1, 100);
    let jobs = state.ops.migrations.list_index(limit).await;
    Ok(Json(FieldIndexListResponse { jobs }))
}

// ── S4：PG 改类型 ────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub(crate) struct FieldFkCounterpartBody {
    #[serde(default)]
    pub schema: Option<String>,
    pub table: String,
    pub column: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct FieldTypeChangeBody {
    #[serde(default)]
    pub db_id: Option<String>,
    #[serde(default)]
    pub schema: Option<String>,
    pub table: String,
    pub column: String,
    pub target_type: String,
    #[serde(default)]
    pub allow_rewrite: Option<bool>,
    #[serde(default)]
    pub production_confirmed: Option<bool>,
    /// S5：freeform USING（提供即按 Rewrite 开闸，解析干跑在 preflight）。
    #[serde(default)]
    pub using_expr: Option<String>,
    /// S5：FK 拆装编排的对端列。
    #[serde(default)]
    pub fk_counterparts: Vec<FieldFkCounterpartBody>,
}

#[derive(Debug, Serialize)]
pub(crate) struct FieldTypeChangeListResponse {
    pub jobs: Vec<TypeChangeJob>,
}

pub(crate) async fn ops_field_typechange_create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<FieldTypeChangeBody>,
) -> Result<Json<TypeChangeJob>, AppError> {
    authorize_write(&state, &headers)?;
    let ctx = build_ctx(&state, body.db_id.as_deref(), Some(session_id(&headers))).await?;
    ensure_scenario_engine(&ctx, "field-governance")?;
    let policy = state.ops.policy.read().await.clone();
    let production_confirmed = body.production_confirmed.unwrap_or(false);
    policy.authorize_backfill(ctx.is_read_only, production_confirmed)?;
    let job = pg_typechange::new_job(
        &ctx,
        &state.ops.migrations,
        pg_typechange::TypeChangeRequest {
            schema: body.schema,
            table: body.table,
            column: body.column,
            target_type: body.target_type,
            allow_rewrite: body.allow_rewrite.unwrap_or(false),
            production_confirmed,
            using_expr: body.using_expr,
            fk_counterparts: body
                .fk_counterparts
                .into_iter()
                .map(
                    |item| core_lib::ops::field_migration::migration_store::FkCounterpartSpec {
                        schema: item.schema,
                        table: item.table,
                        column: item.column,
                    },
                )
                .collect(),
        },
    )
    .await?;
    audit_event(
        &state,
        "field_migration.typechange.created",
        "ok",
        serde_json::json!({
            "job_id": job.job_id,
            "table": job.table,
            "column": job.column,
            "target_type": job.target_type,
            "allow_rewrite": job.allow_rewrite,
        }),
    );
    Ok(Json(job))
}

pub(crate) async fn ops_field_typechange_run(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(job_id): Path<String>,
) -> Result<Json<TypeChangeJob>, AppError> {
    authorize_write(&state, &headers)?;
    let hub = state.ops.migrations.clone();
    let job = hub.get_typechange(&job_id).await?;
    if hub.is_active(&job_id).await {
        return Err(AppError::BadRequest(format!("任务 {job_id} 正在运行")));
    }
    let ctx = build_ctx(&state, Some(&job.db_id), Some(session_id(&headers))).await?;
    ensure_scenario_engine(&ctx, "field-governance")?;
    let policy = state.ops.policy.read().await.clone();
    policy.authorize_backfill(ctx.is_read_only, job.production_confirmed)?;
    hub.set_control(&job_id, CONTROL_RUN).await;
    hub.mark_active(&job_id).await;
    let spawn_hub = hub.clone();
    let spawn_id = job_id.clone();
    tokio::spawn(async move {
        if let Err(err) = pg_typechange::run_job(&spawn_id, &ctx, &spawn_hub, &policy).await {
            let _ = spawn_hub
                .update_typechange(&spawn_id, |job| {
                    job.status = MigrationStatus::Interrupted;
                    job.last_error = Some(err.to_string());
                })
                .await;
        }
        spawn_hub.mark_inactive(&spawn_id).await;
    });
    Ok(Json(job))
}

pub(crate) async fn ops_field_typechange_status(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Result<Json<TypeChangeJob>, AppError> {
    let hub = state.ops.migrations.clone();
    let mut job = hub.get_typechange(&job_id).await?;
    if job.status == MigrationStatus::Running && !hub.is_active(&job_id).await {
        job = hub
            .update_typechange(&job_id, |job| {
                job.status = MigrationStatus::Interrupted;
                job.last_error = Some("后台任务已停止，可重新执行（续跑 = 重跑语句）".into());
            })
            .await?;
    }
    Ok(Json(job))
}

pub(crate) async fn ops_field_typechange_list(
    State(state): State<AppState>,
    Query(query): Query<FieldDdlListQuery>,
) -> Result<Json<FieldTypeChangeListResponse>, AppError> {
    let limit = query.limit.unwrap_or(20).clamp(1, 100);
    let jobs = state.ops.migrations.list_typechange(limit).await;
    Ok(Json(FieldTypeChangeListResponse { jobs }))
}

pub(crate) async fn ops_field_backfill_pause(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(job_id): Path<String>,
) -> Result<Json<MigrationJob>, AppError> {
    authorize_write(&state, &headers)?;
    let hub = state.ops.migrations.clone();
    let job = hub.get(&job_id).await?;
    if job.status.is_finished() {
        return Err(AppError::BadRequest(format!(
            "任务已结束（{}），不能暂停",
            job.status.as_str()
        )));
    }
    hub.set_control(&job_id, CONTROL_PAUSE).await;
    let job = hub
        .update(&job_id, |job| {
            if matches!(
                job.status,
                MigrationStatus::Pending | MigrationStatus::Running
            ) {
                job.status = MigrationStatus::Paused;
                job.phase = MigrationPhase::Paused;
            }
        })
        .await?;
    audit_event(
        &state,
        "field_migration.backfill.paused",
        "ok",
        serde_json::json!({ "job_id": job_id, "checkpoint": job.checkpoint }),
    );
    Ok(Json(job))
}

pub(crate) async fn ops_field_backfill_resume(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(job_id): Path<String>,
    Json(body): Json<FieldBackfillResumeBody>,
) -> Result<Json<MigrationJob>, AppError> {
    authorize_write(&state, &headers)?;
    let hub = state.ops.migrations.clone();
    let job = hub.get(&job_id).await?;
    if hub.is_active(&job_id).await {
        return Err(AppError::BadRequest("任务仍在运行，无需继续".into()));
    }
    match job.status {
        MigrationStatus::Completed | MigrationStatus::Aborted => {
            return Err(AppError::BadRequest(format!(
                "任务已结束（{}），不能继续",
                job.status.as_str()
            )));
        }
        MigrationStatus::Interrupted | MigrationStatus::Blocked => {
            let token = body.approval_token.as_deref().unwrap_or("").trim();
            if token.is_empty() {
                return Err(AppError::BadRequest(
                    "中断/阻断后的续跑需要新的审批令牌".into(),
                ));
            }
            state.ops.approvals.write().await.consume_field_binding(
                token,
                &job.plan_id,
                &job.plan_hash,
                &job.db_fingerprint,
                &job.migration_hash,
                "backfill",
            )?;
        }
        _ => {}
    }
    let db_id = if job.db_id.trim().is_empty() {
        None
    } else {
        Some(job.db_id.as_str())
    };
    let ctx = build_ctx(&state, db_id, Some(session_id(&headers))).await?;
    ensure_scenario_engine(&ctx, "field-governance")?;
    let policy = state.ops.policy.read().await.clone();
    policy.authorize_backfill(ctx.is_read_only, job.production_confirmed)?;
    hub.set_control(&job_id, CONTROL_RUN).await;
    hub.mark_active(&job_id).await;
    let job = hub
        .update(&job_id, |job| {
            job.status = MigrationStatus::Running;
            job.phase = MigrationPhase::Backfilling;
            if job.started_at == 0 {
                job.started_at = core_lib::ops::types::now();
            }
            job.finished_at = None;
            job.last_error = None;
            job.blocking = None;
            job.progress.resume_after_pk = job
                .checkpoint
                .as_ref()
                .and_then(|checkpoint| checkpoint.last_pk.clone());
        })
        .await?;
    audit_event(
        &state,
        "field_migration.backfill.resumed",
        "ok",
        serde_json::json!({
            "job_id": job_id,
            "checkpoint": job.checkpoint,
            "resume_after_pk": job.progress.resume_after_pk,
        }),
    );
    tokio::spawn(async move {
        backfill::run_job(job_id, ctx, hub, policy).await;
    });
    Ok(Json(job))
}

pub(crate) async fn ops_field_backfill_abort(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(job_id): Path<String>,
) -> Result<Json<MigrationJob>, AppError> {
    authorize_write(&state, &headers)?;
    let hub = state.ops.migrations.clone();
    let job = hub.get(&job_id).await?;
    if job.status.is_finished() {
        return Ok(Json(job));
    }
    hub.set_control(&job_id, CONTROL_ABORT).await;
    // 若正在执行批 SQL，立即 KILL QUERY；执行器会把它收敛为 aborted。
    if let Some(connection_id) = job.progress.connection_id {
        let db_id = if job.db_id.trim().is_empty() {
            None
        } else {
            Some(job.db_id.as_str())
        };
        if let Ok(ctx) = build_ctx(&state, db_id, Some(session_id(&headers))).await {
            let _ = ctx.db_client.kill_query(connection_id).await;
        }
    }
    let job = if hub.is_active(&job_id).await {
        job
    } else {
        hub.update(&job_id, |job| {
            job.status = MigrationStatus::Aborted;
            job.phase = MigrationPhase::Aborted;
            job.finished_at = Some(core_lib::ops::types::now());
        })
        .await?
    };
    audit_event(
        &state,
        "field_migration.backfill.abort_requested",
        "ok",
        serde_json::json!({ "job_id": job_id, "connection_id": job.progress.connection_id }),
    );
    Ok(Json(job))
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
            Err(err) => vec![Event::default().event("error").data(
                serde_json::to_string(&serde_json::json!({
                    "message": err.to_string()
                }))
                .unwrap_or_default(),
            )],
        };
        futures::stream::iter(events.into_iter().map(Ok))
    })
    .flatten();
    Ok(Sse::new(Box::pin(stream)))
}

#[derive(Deserialize)]
pub(crate) struct SlowQueryListQuery {
    #[serde(default)]
    pub db_id: Option<String>,
    /// total_latency / avg_latency / rows_examined
    #[serde(default)]
    pub sort: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

pub(crate) async fn ops_slow_queries(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<SlowQueryListQuery>,
) -> Result<Json<SlowQueryScan>, AppError> {
    authorize_write(&state, &headers)?;
    let ctx = build_ctx(&state, query.db_id.as_deref(), Some(session_id(&headers))).await?;
    let sort = query.sort.unwrap_or_else(|| "total_latency".to_string());
    Ok(Json(
        perf_diagnostics::list_slow_queries(&ctx, &sort, query.limit.unwrap_or(20)).await?,
    ))
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
    if ctx.is_postgres() {
        return Err(AppError::BadRequest(
            "PostgreSQL 只读方案不进入审批流程（不包含可执行动作）".into(),
        ));
    }
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
        approvals.issue(
            &plan,
            &req.action_ids,
            policy.approval_ttl_secs,
            Some("local-user".into()),
        )?
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
        plan.actions
            .iter()
            .map(|action| action.action_id())
            .collect()
    });
    if action_ids.is_empty() {
        return Err(AppError::BadRequest("计划没有可执行动作".into()));
    }

    let ctx = plan_ctx(&state, &plan).await?;
    ensure_write_engine(&ctx, "执行动作")?;
    let policy = state.ops.policy.read().await.clone();

    if dry_run {
        let report = executor::dry_run(&ctx, &plan, &action_ids, &policy, &state.ops.audit).await?;
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
    ensure_write_engine(&ctx, "回滚")?;
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

pub(crate) async fn ops_approvals(State(state): State<AppState>) -> Json<serde_json::Value> {
    let records = state.ops.approvals.read().await.list_recent(50);
    Json(serde_json::json!({ "approvals": records }))
}

// 明确不在 ops 链路里复用 DdlEngine / execute_ddl；编译入口只允许 ddl_compiler。
const _: fn(&ActionSpec) = |_| {};

#[cfg(test)]
mod tests {
    use super::{scenario_engine_decision, write_engine_decision};

    #[test]
    fn pg_allows_only_readonly_scenarios() {
        for scenario in [
            "mysql-index-advisor",
            "perf-diagnostics",
            "periodic-inspection",
        ] {
            assert!(
                scenario_engine_decision(true, scenario).is_ok(),
                "PG 应放行只读场景 {scenario}"
            );
        }
    }

    #[test]
    fn pg_allows_field_governance_after_s3() {
        // S3 spec §6：字段治理能力位增加 postgresql，PG 连接不再被场景门控拦截。
        assert!(scenario_engine_decision(true, "field-governance").is_ok());
    }

    #[test]
    fn pg_rejects_unknown_scenario_with_supported_list() {
        let err = scenario_engine_decision(true, "sqlite-index-advisor").unwrap_err();
        assert!(err.contains("当前开放"), "{err}");
    }

    #[test]
    fn mysql_family_is_not_gated() {
        for scenario in [
            "field-governance",
            "mysql-index-advisor",
            "periodic-inspection",
        ] {
            assert!(
                scenario_engine_decision(false, scenario).is_ok(),
                "MySQL/MariaDB 不应被 PG 守卫拦截: {scenario}"
            );
        }
    }

    #[test]
    fn pg_rejects_write_actions() {
        let err = write_engine_decision(true, "执行动作").unwrap_err();
        assert!(err.contains("保持只读"), "{err}");
        assert!(err.contains("不在本轮范围"), "{err}");
        assert!(write_engine_decision(false, "执行动作").is_ok());
    }
}
