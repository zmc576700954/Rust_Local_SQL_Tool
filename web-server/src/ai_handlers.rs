use axum::{
    extract::{Query, State},
    http::StatusCode,
    Json,
};
use core_lib::{
    ai::agent::AgentError,
    ai::events::AiHealthReport,
    ai::provider_utils,
    config::{AiModel, AiProvider},
    db::DbClient,
    error::AppError,
    knowledge_base::Knowledge,
};
use serde::{Deserialize, Serialize};

use crate::{get_schema_internal, map_agent_error, AppState};

#[derive(Deserialize)]
pub struct FetchModelsRequest {
    pub provider: AiProvider,
    pub api_key: String,
    pub base_url: Option<String>,
}

#[derive(Serialize)]
pub struct FetchModelsResponse {
    pub models: Vec<String>,
}

pub async fn fetch_provider_models(
    State(state): State<AppState>,
    Json(req): Json<FetchModelsRequest>,
) -> Result<Json<FetchModelsResponse>, AppError> {
    let config = state.config.read().await.clone();
    let models = provider_utils::fetch_provider_models(&config, req.provider, req.api_key, req.base_url)
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    Ok(Json(FetchModelsResponse { models }))
}

#[derive(Serialize)]
pub struct AiModelsResponse {
    pub models: Vec<AiModel>,
    pub active_model_id: Option<String>,
    pub active_tier: String,
}

pub async fn ai_models(State(state): State<AppState>) -> Result<Json<AiModelsResponse>, AppError> {
    let config = state.config.read().await.clone();
    Ok(Json(AiModelsResponse {
        models: config.ai_models,
        active_model_id: config.active_model_id,
        active_tier: config.active_tier,
    }))
}

pub async fn ai_health(State(state): State<AppState>) -> Result<Json<AiHealthReport>, AppError> {
    let config = state.config.read().await.clone();
    let report = provider_utils::health_check(&config).await.map_err(map_agent_error)?;
    Ok(Json(report))
}

#[derive(Deserialize)]
pub struct AiQueryRequest {
    pub query: String,
    pub mode: Option<String>,
    pub current_sql: Option<String>,
    pub chat_history: Option<Vec<serde_json::Value>>,
}

#[derive(Serialize)]
pub struct AiQueryResponse {
    pub sql: String,
    pub explanation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sql_empty_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_information: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub grounding_evidence: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assumptions: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub referenced_tables: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub risk_level: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub needs_confirmation: Option<bool>,
}

#[derive(Deserialize)]
pub struct ChatRequest {
    pub query: String,
    pub mode: Option<String>,
    pub current_sql: Option<String>,
    pub chat_history: Option<Vec<serde_json::Value>>,
}

#[derive(Serialize)]
pub struct ChatResponse {
    pub sql: String,
    pub explanation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sql_empty_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_information: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub grounding_evidence: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assumptions: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub referenced_tables: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub risk_level: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub needs_confirmation: Option<bool>,
}

pub(crate) fn log_ai_intent_metadata(
    route: &str,
    sql: &str,
    task_type: Option<&str>,
    sql_empty_reason: Option<&str>,
    missing_information: &[String],
) {
    if sql.trim().is_empty()
        || task_type.is_some()
        || sql_empty_reason.is_some()
        || !missing_information.is_empty()
    {
        tracing::info!(
            route = route,
            has_sql = !sql.trim().is_empty(),
            task_type = ?task_type,
            sql_empty_reason = ?sql_empty_reason,
            missing_information = ?missing_information,
            "AI intent metadata"
        );
    }
}

fn normalize_ai_mode(mode: Option<&str>) -> Option<&str> {
    match mode.map(str::trim).filter(|value| !value.is_empty()) {
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
                    "Task mode: optimize_sql\nUser request:\n{}\n\nCurrent SQL:\n{}\n\nRequirements:\n- Operate only on Current SQL.\n- Preserve business intent and result semantics.\n- Return the improved SQL in the sql field and summarize the changes in explanation.\n- Do not answer with unrelated SQL or generic advice only.",
                    query, sql
                )
            } else {
                format!(
                    "Task mode: optimize_sql\nUser request:\n{}\n\nRequirements:\n- Return optimized SQL in the sql field.\n- Preserve business intent and result semantics.",
                    query
                )
            }
        }
        Some("explain") => {
            if let Some(sql) = current_sql {
                format!(
                    "Task mode: explain_sql\nUser request:\n{}\n\nCurrent SQL:\n{}\n\nRequirements:\n- Explain only this SQL.\n- Do not generate unrelated replacement SQL.\n- You may keep the sql field empty if no rewrite is needed.",
                    query, sql
                )
            } else {
                format!(
                    "Task mode: explain_sql\nUser request:\n{}\n\nRequirements:\n- Focus on explanation.\n- Keep sql empty when there is no concrete SQL to rewrite.",
                    query
                )
            }
        }
        _ => query.to_string(),
    }
}

/// Non-streaming AI query endpoint — routes through the new Agent layer.
pub async fn ai_query(
    State(state): State<AppState>,
    Json(req): Json<AiQueryRequest>,
) -> Result<Json<AiQueryResponse>, AppError> {
    let AiQueryRequest {
        query,
        mode,
        current_sql,
        chat_history,
    } = req;
    let config = state.config.read().await.clone();
    let db_client = state.db_client.read().await.clone();
    let rule_store = state.rule_store.read().await.clone();
    let policy = state.policy.read().await.clone();
    let knowledge_base = state.knowledge_base.read().await.clone();
    let cached_schema = get_schema_internal(&state).await;

    let url = config.get_active_db_url().unwrap_or_default();
    let db_name = DbClient::extract_db_name(&url).unwrap_or_default();
    let normalized_mode = normalize_ai_mode(mode.as_deref());
    let scoped_query = build_mode_scoped_query(&query, normalized_mode, current_sql.as_deref());

    // Rule fast-path: skip agent if direct match
    if let Some(result) = core_lib::ai::agent::try_rule_fast_path(&scoped_query, &rule_store, &policy) {
        log_ai_intent_metadata(
            "/api/ai/query",
            &result.sql,
            result.task_type.as_deref(),
            result.sql_empty_reason.as_deref(),
            &result.missing_information,
        );
        // Increment rule hit count
        if let Some(rule_id) = result.matched_rule_id.clone() {
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
        return Ok(Json(AiQueryResponse {
            sql: result.sql,
            explanation: result.explanation,
            task_type: result.task_type,
            sql_empty_reason: result.sql_empty_reason,
            missing_information: result.missing_information,
            grounding_evidence: result.grounding_evidence,
            assumptions: result.assumptions,
            referenced_tables: result.referenced_tables,
            risk_level: result.risk_level,
            needs_confirmation: result.needs_confirmation,
        }));
    }

    // Pre-flight: validate API key
    core_lib::ai::agent::validate_ai_profile(&config).map_err(|e| match e {
        AgentError::MissingApiKey => {
            AppError::AiAuth("Missing API key. Please configure your AI token.".to_string())
        }
        other => AppError::InternalError(other.to_string()),
    })?;

    let extra_guidance = "Prefer concise SQL. First resolve entities, filters, time range, grouping, ordering, and output columns from the user's request.";

    // Build schema briefing for preamble injection
    let schema_briefing_str = if let Some(schema) = cached_schema.as_ref() {
        let briefing = core_lib::ai::schema_briefing::SchemaBriefing::build(
            schema,
            &scoped_query,
            &knowledge_base.items,
        );
        Some(briefing.summary_text)
    } else {
        None
    };

    let task_type = core_lib::ai::agent::TaskType::from_mode(normalized_mode);

    let result = core_lib::ai::agent::run_agent(
        &config,
        db_client.as_ref(),
        &db_name,
        &scoped_query,
        &rule_store,
        &knowledge_base,
        &policy,
        chat_history.as_deref(),
        schema_briefing_str.as_deref(),
        Some(extra_guidance),
        &task_type,
    )
    .await
    .map_err(map_agent_error)?;

    log_ai_intent_metadata(
        "/api/ai/query",
        &result.sql,
        result.task_type.as_deref(),
        result.sql_empty_reason.as_deref(),
        &result.missing_information,
    );

    // Increment rule hit count if matched
    if let Some(rule_id) = result.matched_rule_id.clone() {
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

    let explanation_only = normalized_mode == Some("explain")
        && result
            .explanation
            .as_deref()
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false);
    if result.sql.trim().is_empty() && !explanation_only {
        return Err(AppError::ParseError(
            result
                .explanation
                .unwrap_or_else(|| "AI 返回无法解析为 SQL。".to_string()),
        ));
    }

    let response_sql = if explanation_only && result.sql.trim().is_empty() {
        current_sql.unwrap_or_default()
    } else {
        result.sql
    };

    Ok(Json(AiQueryResponse {
        sql: response_sql,
        explanation: result.explanation,
        task_type: result.task_type,
        sql_empty_reason: result.sql_empty_reason,
        missing_information: result.missing_information,
        grounding_evidence: result.grounding_evidence,
        assumptions: result.assumptions,
        referenced_tables: result.referenced_tables,
        risk_level: result.risk_level,
        needs_confirmation: result.needs_confirmation,
    }))
}

#[derive(Deserialize)]
pub struct AiExplainErrorRequest {
    pub error_msg: String,
    pub failed_query: String,
}

#[derive(Serialize)]
pub struct AiExplainErrorResponse {
    pub explanation: String,
    pub fixed_query: Option<String>,
}

/// Explain error endpoint — routes through the Agent layer with TaskType::Fix.
pub async fn ai_explain_error(
    State(state): State<AppState>,
    Json(req): Json<AiExplainErrorRequest>,
) -> Result<Json<AiExplainErrorResponse>, AppError> {
    let config = state.config.read().await.clone();
    let db_client = state.db_client.read().await.clone();
    let rule_store = state.rule_store.read().await.clone();
    let policy = state.policy.read().await.clone();
    let knowledge_base = state.knowledge_base.read().await.clone();
    let cached_schema = get_schema_internal(&state).await;

    let url = config.get_active_db_url().unwrap_or_default();
    let db_name = DbClient::extract_db_name(&url).unwrap_or_default();

    // Validate API key
    core_lib::ai::agent::validate_ai_profile(&config).map_err(|e| match e {
        AgentError::MissingApiKey => {
            AppError::AiAuth("Missing API key. Please configure your AI token.".to_string())
        }
        other => AppError::InternalError(other.to_string()),
    })?;

    let scoped_query = format!(
        "The following query failed and needs a fix.\nSQL:\n{}\n\nDatabase error:\n{}\n\n\
        Intent is fix_sql. Preserve the business intent of the failed query, \
        change only the minimal syntax, alias, join, or schema references needed to fix it, \
        explain the failure briefly, and return a corrected query when possible. \
        If a safe correction is impossible, keep sql empty and explain why.",
        req.failed_query, req.error_msg
    );

    let extra_guidance = "Focus on fixing the SQL error. Analyze the error message, \
        identify the root cause, and provide a minimal correction. \
        Always validate the fix with execute_sql.";

    let schema_briefing_str = if let Some(schema) = cached_schema.as_ref() {
        let briefing = core_lib::ai::schema_briefing::SchemaBriefing::build(
            schema,
            &scoped_query,
            &knowledge_base.items,
        );
        Some(briefing.summary_text)
    } else {
        None
    };

    let result = core_lib::ai::agent::run_agent(
        &config,
        db_client.as_ref(),
        &db_name,
        &scoped_query,
        &rule_store,
        &knowledge_base,
        &policy,
        None,
        schema_briefing_str.as_deref(),
        Some(extra_guidance),
        &core_lib::ai::agent::TaskType::Fix,
    )
    .await
    .map_err(map_agent_error)?;

    let explanation = result.explanation.unwrap_or_else(|| "AI returned no explanation.".to_string());
    let fixed_query = if result.sql.trim().is_empty() {
        None
    } else {
        Some(result.sql)
    };

    Ok(Json(AiExplainErrorResponse {
        explanation,
        fixed_query,
    }))
}

#[derive(Deserialize)]
pub struct GetKnowledgeRequest {
    pub db_connection_id: Option<String>,
}

pub async fn get_knowledge(
    State(state): State<AppState>,
    Query(req): Query<GetKnowledgeRequest>,
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

    items.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Ok(Json(items))
}

pub async fn add_knowledge(
    State(state): State<AppState>,
    Json(item): Json<Knowledge>,
) -> Result<Json<Knowledge>, AppError> {
    let mut item = item;
    let kb_clone = {
        let mut kb = state.knowledge_base.write().await;
        kb.add_item(item.clone());
        item = kb
            .items
            .last()
            .cloned()
            .ok_or_else(|| AppError::InternalError("Failed to add knowledge item".to_string()))?;
        kb.clone()
    };
    kb_clone
        .save()
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    Ok(Json(item))
}

/// Non-streaming chat-to-SQL endpoint — routes through the new Agent layer.
pub async fn chat_to_sql(
    State(state): State<AppState>,
    Json(req): Json<ChatRequest>,
) -> Result<Json<ChatResponse>, AppError> {
    let ChatRequest {
        query,
        mode,
        current_sql,
        chat_history,
    } = req;
    let config = state.config.read().await.clone();
    let db_client = state.db_client.read().await.clone();
    let rule_store = state.rule_store.read().await.clone();
    let policy = state.policy.read().await.clone();
    let knowledge_base = state.knowledge_base.read().await.clone();
    let cached_schema = get_schema_internal(&state).await;

    let url = config.get_active_db_url().unwrap_or_default();
    let db_name = DbClient::extract_db_name(&url).unwrap_or_default();
    let normalized_mode = normalize_ai_mode(mode.as_deref());
    let scoped_query = build_mode_scoped_query(&query, normalized_mode, current_sql.as_deref());

    // Rule fast-path: skip agent if direct match
    if let Some(result) = core_lib::ai::agent::try_rule_fast_path(&scoped_query, &rule_store, &policy) {
        log_ai_intent_metadata(
            "/chat",
            &result.sql,
            result.task_type.as_deref(),
            result.sql_empty_reason.as_deref(),
            &result.missing_information,
        );
        if let Some(rule_id) = result.matched_rule_id.clone() {
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
        return Ok(Json(ChatResponse {
            sql: result.sql,
            explanation: result.explanation,
            task_type: result.task_type,
            sql_empty_reason: result.sql_empty_reason,
            missing_information: result.missing_information,
            grounding_evidence: result.grounding_evidence,
            assumptions: result.assumptions,
            referenced_tables: result.referenced_tables,
            risk_level: result.risk_level,
            needs_confirmation: result.needs_confirmation,
        }));
    }

    // Pre-flight: validate API key
    core_lib::ai::agent::validate_ai_profile(&config).map_err(|e| match e {
        AgentError::MissingApiKey => {
            AppError::AiAuth("Missing API key. Please configure your AI token.".to_string())
        }
        other => AppError::InternalError(other.to_string()),
    })?;

    let extra_guidance = "Prefer concise SQL. First resolve entities, filters, time range, grouping, ordering, and output columns from the user's request.";

    let schema_briefing_str = if let Some(schema) = cached_schema.as_ref() {
        let briefing = core_lib::ai::schema_briefing::SchemaBriefing::build(
            schema,
            &scoped_query,
            &knowledge_base.items,
        );
        Some(briefing.summary_text)
    } else {
        None
    };

    let task_type = core_lib::ai::agent::TaskType::from_mode(normalized_mode);

    let result = core_lib::ai::agent::run_agent(
        &config,
        db_client.as_ref(),
        &db_name,
        &scoped_query,
        &rule_store,
        &knowledge_base,
        &policy,
        chat_history.as_deref(),
        schema_briefing_str.as_deref(),
        Some(extra_guidance),
        &task_type,
    )
    .await
    .map_err(map_agent_error)?;

    log_ai_intent_metadata(
        "/chat",
        &result.sql,
        result.task_type.as_deref(),
        result.sql_empty_reason.as_deref(),
        &result.missing_information,
    );

    // Increment rule hit count if matched
    if let Some(rule_id) = result.matched_rule_id.clone() {
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
        sql: result.sql,
        explanation: result.explanation,
        task_type: result.task_type,
        sql_empty_reason: result.sql_empty_reason,
        missing_information: result.missing_information,
        grounding_evidence: result.grounding_evidence,
        assumptions: result.assumptions,
        referenced_tables: result.referenced_tables,
        risk_level: result.risk_level,
        needs_confirmation: result.needs_confirmation,
    }))
}

pub async fn update_knowledge(
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
pub struct DeleteKnowledgeRequest {
    pub id: String,
}

pub async fn delete_knowledge(
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

// ── SSE Streaming Agent Handler ──────────────────────────────────────────

use axum::response::sse::{Event, Sse};
use futures::stream::Stream;
use futures::StreamExt;
use std::convert::Infallible;

pub async fn chat_to_sql_stream(
    State(state): State<AppState>,
    Json(req): Json<ChatRequest>,
) -> Result<Sse<std::pin::Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>>, AppError> {
    let ChatRequest {
        query,
        mode,
        current_sql,
        chat_history,
    } = req;

    let config = state.config.read().await.clone();
    let db_client = state.db_client.read().await.clone();
    let rule_store = state.rule_store.read().await.clone();
    let policy = state.policy.read().await.clone();
    let knowledge_base = state.knowledge_base.read().await.clone();
    let cached_schema = get_schema_internal(&state).await;

    let url = config.get_active_db_url().unwrap_or_default();
    let db_name = DbClient::extract_db_name(&url).unwrap_or_default();
    let normalized_mode = normalize_ai_mode(mode.as_deref());
    let scoped_query = build_mode_scoped_query(&query, normalized_mode, current_sql.as_deref());

    // Rule fast-path: skip agent if direct match
    if let Some(result) = core_lib::ai::agent::try_rule_fast_path(&scoped_query, &rule_store, &policy) {
        // Increment rule hit count
        if let Some(rule_id) = result.matched_rule_id.clone() {
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
        let stream = futures::stream::once(async move {
            let data = serde_json::to_string(&serde_json::json!({
                "type": "final_sql",
                "data": { "sql": result.sql, "task_type": result.task_type }
            })).unwrap_or_default();
            Ok(Event::default().data(data))
        });
        return Ok(Sse::new(Box::pin(stream)));
    }

    // Pre-flight: validate API key before creating agent (fail fast)
    core_lib::ai::agent::validate_ai_profile(&config).map_err(|e| match e {
        core_lib::ai::agent::AgentError::MissingApiKey => {
            AppError::AiAuth("Missing API key. Please configure your AI token.".to_string())
        }
        other => AppError::InternalError(other.to_string()),
    })?;

    let extra_guidance = "Prefer concise SQL. First resolve entities, filters, time range, grouping, ordering, and output columns from the user's request.";

    // Create a cancellation token so the stream can be aborted when the client disconnects
    let cancel_token = tokio_util::sync::CancellationToken::new();

    // Build schema briefing for preamble injection
    let schema_briefing_str = if let Some(schema) = cached_schema.as_ref() {
        let briefing = core_lib::ai::schema_briefing::SchemaBriefing::build(
            schema,
            &scoped_query,
            &knowledge_base.items,
        );
        Some(briefing.summary_text)
    } else {
        None
    };

    let task_type = core_lib::ai::agent::TaskType::from_mode(normalized_mode);

    let agent_stream = core_lib::ai::agent::run_agent_streaming(
        &config,
        db_client.as_ref(),
        &db_name,
        &scoped_query,
        &rule_store,
        &knowledge_base,
        &policy,
        chat_history.as_deref(),
        schema_briefing_str.as_deref(),
        Some(extra_guidance),
        cancel_token,
        &task_type,
    )
    .await
    .map_err(|e| match e {
        core_lib::ai::agent::AgentError::MissingApiKey => {
            AppError::AiAuth("Missing API key. Please configure your AI token.".to_string())
        }
        core_lib::ai::agent::AgentError::Auth(msg) => {
            let body = serde_json::json!({
                "error": "ai_auth_failed",
                "message": "AI 鉴权失败，请在引导页里更新 AI Token / Relay 配置后重试。",
                "detail": msg,
            })
            .to_string();
            AppError::AiAuth(body)
        }
        _ => AppError::InternalError(e.to_string()),
    })?;

    let sse_stream = agent_stream.map(|event| {
        let event_type = event.event_type().to_string();
        let json_str = serde_json::to_string(&event.data_json()).unwrap_or_default();
        Ok(Event::default().event(event_type).data(json_str))
    });

    Ok(Sse::new(Box::pin(sse_stream)))
}