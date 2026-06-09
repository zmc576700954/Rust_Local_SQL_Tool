//! AiService — 统一 AI 调用入口
//!
//! 将 web-server 和 src-tauri 的 AI 业务逻辑统一下沉：
//! - 模式标准化 (normalize_ai_mode)
//! - 查询上下文构建 (build_mode_scoped_query)
//! - 规则快路径 (try_rule_fast_path)
//! - SchemaBriefing 注入
//! - API Key 预检
//! - 规则命中计数更新
//!
//! 三条调用路径：
//! - chat()       → 非流式 agent loop (run_agent)
//! - chat_stream() → 流式 agent loop (run_agent_streaming)
//! - chat_raw()    → 单次 LLM 调用 (chat_completion_raw)

use tokio_util::sync::CancellationToken;

use crate::ai::agent::{
    AgentStream, TaskType,
    run_agent, run_agent_streaming, chat_completion_raw, generate_rule_template,
    try_rule_fast_path, validate_ai_profile,
};
use crate::ai::events::{AgentEvent, AgentResult, AiHealthReport};
use crate::ai::schema_briefing::SchemaBriefing;
use crate::ai::provider_utils;
use crate::db::DbClient;
use crate::knowledge_base::KnowledgeBase;
use crate::service::context::ServiceContext;
use crate::service::error::ServiceError;

// ── 请求类型 ──────────────────────────────────────────────

/// 统一 AI 请求参数 — 三条路径共用
pub struct AiChatRequest {
    pub query: String,
    pub mode: Option<String>,
    pub current_sql: Option<String>,
    pub chat_history: Option<Vec<serde_json::Value>>,
    pub extra_guidance: Option<String>,
}

/// AI 调用结果 — 非流式路径返回
pub struct AiChatResult {
    pub agent_result: AgentResult,
    /// 标准化后的 mode (如 "generate", "explain" 等)
    pub normalized_mode: Option<String>,
}

// ── 模式标准化 ────────────────────────────────────────────

fn normalize_ai_mode(mode: Option<&str>) -> Option<&str> {
    match mode.map(str::trim).filter(|v| !v.is_empty()) {
        Some("generate") | Some("generate_sql") => Some("generate"),
        Some("optimize") | Some("optimize_sql") => Some("optimize"),
        Some("explain") | Some("explain_sql") => Some("explain"),
        Some("fix") | Some("fix_sql") => Some("fix"),
        _ => None,
    }
}

fn build_mode_scoped_query(query: &str, mode: Option<&str>, current_sql: Option<&str>) -> String {
    let query = query.trim();
    let current_sql = current_sql.map(str::trim).filter(|sql| !sql.is_empty());

    match mode {
        Some("optimize") => {
            if let Some(sql) = current_sql {
                format!(
                    "Task mode: optimize_sql\nUser request:\n{}\n\nCurrent SQL:\n{}\n\nRequirements:\n\
                     - Operate only on Current SQL.\n\
                     - Preserve business intent and result semantics.\n\
                     - Return the improved SQL in the sql field and summarize the changes in explanation.\n\
                     - Do not answer with unrelated SQL or generic advice only.",
                    query, sql
                )
            } else {
                format!(
                    "Task mode: optimize_sql\nUser request:\n{}\n\nRequirements:\n\
                     - Return optimized SQL in the sql field.\n\
                     - Preserve business intent and result semantics.",
                    query
                )
            }
        }
        Some("explain") => {
            if let Some(sql) = current_sql {
                format!(
                    "Task mode: explain_sql\nUser request:\n{}\n\nCurrent SQL:\n{}\n\nRequirements:\n\
                     - Explain only this SQL.\n\
                     - Do not generate unrelated replacement SQL.\n\
                     - You may keep the sql field empty if no rewrite is needed.",
                    query, sql
                )
            } else {
                format!(
                    "Task mode: explain_sql\nUser request:\n{}\n\nRequirements:\n\
                     - Focus on explanation.\n\
                     - Keep sql empty when there is no concrete SQL to rewrite.",
                    query
                )
            }
        }
        _ => query.to_string(),
    }
}

// ── AiService ──────────────────────────────────────────────

pub struct AiService;

impl AiService {
    // ── 非流式 Agent 调用 ─────────────────────────────────

    /// 非流式 AI 查询 — 完整 agent loop (run_agent)
    ///
    /// 流程：模式标准化 → 查询上下文 → 规则快路径 → API Key 预检
    ///       → SchemaBriefing → run_agent → 规则命中计数
    pub async fn chat(ctx: &ServiceContext, req: AiChatRequest) -> Result<AiChatResult, ServiceError> {
        let config = ctx.get_config().await;
        let db_client = ctx.db_state().get_active_client().await;
        let rule_store = ctx.ai_state().get_rule_store().await;
        let policy = ctx.ai_state().get_policy().await;
        let knowledge_base = ctx.ai_state().get_knowledge_base().await;
        let cached_schema = Self::get_schema(ctx).await;

        let url = config.get_active_db_url().unwrap_or_default();
        let db_name = DbClient::extract_db_name(&url).unwrap_or_default();

        let normalized_mode = normalize_ai_mode(req.mode.as_deref());
        let scoped_query = build_mode_scoped_query(&req.query, normalized_mode, req.current_sql.as_deref());

        // 规则快路径
        if let Some(result) = try_rule_fast_path(&scoped_query, &rule_store, &policy) {
            Self::increment_rule_hit_count(ctx, &result.matched_rule_id).await;
            return Ok(AiChatResult {
                agent_result: result,
                normalized_mode: normalized_mode.map(|s| s.to_string()),
            });
        }

        // API Key 预检
        validate_ai_profile(&config).map_err(ServiceError::from_agent_error)?;

        // SchemaBriefing 注入
        let schema_briefing_str = Self::build_schema_briefing(&cached_schema, &scoped_query, &knowledge_base);

        let extra_guidance = req.extra_guidance.as_deref().or_else(|| Some(
            "Prefer concise SQL. First resolve entities, filters, time range, grouping, ordering, and output columns from the user's request."
        ));

        let task_type = TaskType::from_mode(normalized_mode);

        let result = run_agent(
            &config,
            db_client.as_ref(),
            &db_name,
            &scoped_query,
            &rule_store,
            &knowledge_base,
            &policy,
            req.chat_history.as_deref(),
            schema_briefing_str.as_deref(),
            extra_guidance,
            &task_type,
        )
        .await
        .map_err(ServiceError::from_agent_error)?;

        // 规则命中计数（agent 内部也可能匹配到规则）
        Self::increment_rule_hit_count(ctx, &result.matched_rule_id).await;

        Ok(AiChatResult {
            agent_result: result,
            normalized_mode: normalized_mode.map(|s| s.to_string()),
        })
    }

    // ── 流式 Agent 调用 ───────────────────────────────────

    /// 流式 AI 查询 — SSE-friendly agent loop (run_agent_streaming)
    ///
    /// 流程与非流式相同，但返回 AgentStream 供 handler 映射为 SSE events。
    pub async fn chat_stream(
        ctx: &ServiceContext,
        req: AiChatRequest,
        cancel_token: CancellationToken,
    ) -> Result<AgentStream, ServiceError> {
        let config = ctx.get_config().await;
        let db_client = ctx.db_state().get_active_client().await;
        let rule_store = ctx.ai_state().get_rule_store().await;
        let policy = ctx.ai_state().get_policy().await;
        let knowledge_base = ctx.ai_state().get_knowledge_base().await;
        let cached_schema = Self::get_schema(ctx).await;

        let url = config.get_active_db_url().unwrap_or_default();
        let db_name = DbClient::extract_db_name(&url).unwrap_or_default();

        let normalized_mode = normalize_ai_mode(req.mode.as_deref());
        let scoped_query = build_mode_scoped_query(&req.query, normalized_mode, req.current_sql.as_deref());

        // 规则快路径（流式：返回单条 final_sql event）
        if let Some(result) = try_rule_fast_path(&scoped_query, &rule_store, &policy) {
            Self::increment_rule_hit_count(ctx, &result.matched_rule_id).await;
            let sql = result.sql.clone();
            let task_type = result.task_type.clone();
            let events: Vec<AgentEvent> = vec![
                AgentEvent::FinalSql { sql, task_type },
                AgentEvent::Done,
            ];
            return Ok(Box::pin(futures_util::stream::iter(events)));
        }

        // API Key 预检
        validate_ai_profile(&config).map_err(ServiceError::from_agent_error)?;

        let schema_briefing_str = Self::build_schema_briefing(&cached_schema, &scoped_query, &knowledge_base);

        let extra_guidance = req.extra_guidance.as_deref().or_else(|| Some(
            "Prefer concise SQL. First resolve entities, filters, time range, grouping, ordering, and output columns from the user's request."
        ));

        let task_type = TaskType::from_mode(normalized_mode);

        let stream = run_agent_streaming(
            &config,
            db_client.as_ref(),
            &db_name,
            &scoped_query,
            &rule_store,
            &knowledge_base,
            &policy,
            req.chat_history.as_deref(),
            schema_briefing_str.as_deref(),
            extra_guidance,
            cancel_token,
            &task_type,
        )
        .await
        .map_err(ServiceError::from_agent_error)?;

        Ok(stream)
    }

    // ── 单次 LLM 调用 ─────────────────────────────────────

    /// 单次 LLM 调用 — 无工具、无 agent loop (chat_completion_raw)
    pub async fn chat_raw(
        ctx: &ServiceContext,
        system_prompt: &str,
        user_message: &str,
    ) -> Result<String, ServiceError> {
        let config = ctx.get_config().await;
        chat_completion_raw(&config, system_prompt, user_message)
            .await
            .map_err(ServiceError::from_agent_error)
    }

    // ── 规则模板生成 ───────────────────────────────────────

    /// 生成 Handlebars 规则模板 (generate_rule_template)
    pub async fn generate_rule_template_service(
        ctx: &ServiceContext,
        prompt: &str,
        sql: &str,
    ) -> Result<String, ServiceError> {
        let config = ctx.get_config().await;
        generate_rule_template(&config, prompt, sql)
            .await
            .map_err(ServiceError::from_agent_error)
    }

    // ── 健康检查 ───────────────────────────────────────────

    /// AI 服务健康检查
    pub async fn health_check(ctx: &ServiceContext) -> Result<AiHealthReport, ServiceError> {
        let config = ctx.get_config().await;
        provider_utils::health_check(&config)
            .await
            .map_err(ServiceError::from_agent_error)
    }

    // ── 辅助方法 ───────────────────────────────────────────

    /// 获取缓存的 SchemaResponse
    async fn get_schema(ctx: &ServiceContext) -> Option<crate::schema::SchemaResponse> {
        // 优先从 virtual_schema 取
        if let Some(vs) = ctx.ai_state().get_virtual_schema().await {
            return Some(vs);
        }
        // 其次从 schema_cache 取（当前活跃 DB）
        let active_db_id = ctx.active_db_id().await;
        if let Some(db_id) = active_db_id {
            if let Some(entry) = ctx.schema_state().get_schema(&db_id).await {
                return Some(entry.schema);
            }
        }
        None
    }

    /// 构建 SchemaBriefing 摘要文本
    fn build_schema_briefing(
        schema: &Option<crate::schema::SchemaResponse>,
        scoped_query: &str,
        knowledge_base: &KnowledgeBase,
    ) -> Option<String> {
        schema.as_ref().map(|s| {
            let briefing = SchemaBriefing::build(s, scoped_query, &knowledge_base.items);
            briefing.summary_text
        })
    }

    /// 异步更新规则命中计数（spawned task）
    async fn increment_rule_hit_count(ctx: &ServiceContext, matched_rule_id: &Option<String>) {
        if let Some(rule_id) = matched_rule_id {
            let rule_store_arc = ctx.ai_state().rule_store_arc();
            let rule_id = rule_id.clone();
            tokio::spawn(async move {
                let store_clone = {
                    let mut store = rule_store_arc.write().await;
                    if store.increment_hit_count(&rule_id) {
                        Some(store.clone())
                    } else {
                        None
                    }
                };
                if let Some(store) = store_clone {
                    if let Err(e) = store.save().await {
                        tracing::error!("Failed to save rule hit count: {}", e);
                    }
                }
            });
        }
    }
}