use axum::{
    extract::{Query, State},
    http::StatusCode,
    Json,
};
use core_lib::{
    ai::gateway::{AiError, AiHealthReport},
    ai_agent::{AiRouter, DbDialect},
    config::{AiModel, AiProvider},
    db::DbClient,
    error::AppError,
    knowledge_base::Knowledge,
};
use serde::{Deserialize, Serialize};

use crate::{get_schema_internal, map_ai_error, AppState};

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
    let gateway = state.planner.read().await.gateway.clone();
    let models = gateway
        .fetch_provider_models(req.provider, req.api_key, req.base_url)
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
    let gateway = core_lib::ai::gateway::AiGateway::new(config);
    let report = gateway.health_check().await.map_err(map_ai_error)?;
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
}

fn log_ai_intent_metadata(
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
    let url = config.get_active_db_url().unwrap_or_default();
    let dialect = DbDialect::from_url(&url);
    let db_conn_id = config.active_db_id.clone();
    let normalized_mode = normalize_ai_mode(mode.as_deref());
    let scoped_query = build_mode_scoped_query(&query, normalized_mode, current_sql.as_deref());

    let schema = get_schema_internal(&state).await;

    let knowledge = {
        let kb = state.knowledge_base.read().await;
        kb.retrieve(db_conn_id.as_deref(), &query, 5)
    };

    let gateway = core_lib::ai::gateway::AiGateway::new(config);
    let router = AiRouter::new(gateway);

    match router
        .dispatch_query(
            dialect,
            &scoped_query,
            schema.as_ref(),
            &knowledge,
            chat_history,
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
            let mut intent = core_lib::ai::extractor::extract_sql_intent(&result_str);
            if intent.task_type.is_none() {
                intent.task_type = normalized_mode.map(|value| match value {
                    "optimize" => "optimize_sql".to_string(),
                    "explain" => "explain_sql".to_string(),
                    _ => "generate_sql".to_string(),
                });
            }
            log_ai_intent_metadata(
                "/api/ai/query",
                &intent.sql,
                intent.task_type.as_deref(),
                intent.sql_empty_reason.as_deref(),
                &intent.missing_information,
            );
            let explanation_only = normalized_mode == Some("explain")
                && intent
                    .explanation
                    .as_deref()
                    .map(|value| !value.trim().is_empty())
                    .unwrap_or(false);
            if intent.sql.trim().is_empty() && !explanation_only {
                return Err(AppError::ParseError(
                    intent
                        .explanation
                        .unwrap_or_else(|| "AI 返回无法解析为 SQL。".to_string()),
                ));
            }
            let response_sql = if explanation_only && intent.sql.trim().is_empty() {
                current_sql.unwrap_or_default()
            } else {
                intent.sql
            };
            Ok(Json(AiQueryResponse {
                sql: response_sql,
                explanation: intent.explanation,
                task_type: intent.task_type,
                sql_empty_reason: intent.sql_empty_reason,
                missing_information: intent.missing_information,
            }))
        }
        Err(e) => Err(map_ai_error(e)),
    }
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

pub async fn ai_explain_error(
    State(state): State<AppState>,
    Json(req): Json<AiExplainErrorRequest>,
) -> Result<Json<AiExplainErrorResponse>, AppError> {
    let config = state.config.read().await.clone();
    let url = config.get_active_db_url().unwrap_or_default();
    let dialect = DbDialect::from_url(&url);

    let schema = get_schema_internal(&state).await;

    let gateway = core_lib::ai::gateway::AiGateway::new(config);
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
                let fixed_query = val["fixed_query"]
                    .as_str()
                    .or_else(|| val["sql"].as_str())
                    .map(|s| s.to_string());
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
    let planner = state.planner.read().await.clone();
    let db_client = state.db_client.read().await.clone();
    let cached_schema = get_schema_internal(&state).await;
    let policy = state.policy.read().await.clone();
    let rule_store = state.rule_store.read().await.clone();

    let db_type = state.config.read().await.get_active_db_type();
    let normalized_mode = normalize_ai_mode(mode.as_deref());
    let scoped_query = build_mode_scoped_query(&query, normalized_mode, current_sql.as_deref());

    let intent_res = if let Some(schema) = cached_schema.as_ref() {
        planner
            .generate_sql_with_virtual_schema(
                &scoped_query,
                schema,
                &rule_store,
                &policy,
                &db_type,
                chat_history.as_deref(),
            )
            .await
    } else if let Some(db_client) = db_client {
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
                &scoped_query,
                &rule_store,
                &policy,
                &db_type,
                chat_history.as_deref(),
            )
            .await
    } else {
        planner
            .generate_sql_no_schema(
                &scoped_query,
                &rule_store,
                &policy,
                &db_type,
                chat_history.as_deref(),
            )
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

    log_ai_intent_metadata(
        "/chat",
        &intent.sql,
        intent.task_type.as_deref(),
        intent.sql_empty_reason.as_deref(),
        &intent.missing_information,
    );

    if let Some(rule_id) = intent.matched_rule_id.clone() {
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
        task_type: intent.task_type,
        sql_empty_reason: intent.sql_empty_reason,
        missing_information: intent.missing_information,
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
