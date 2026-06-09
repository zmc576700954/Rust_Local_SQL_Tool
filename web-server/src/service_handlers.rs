//! service_handlers.rs — 调用 core_lib service 层的薄壳 handler
//!
//! 每个 handler 仅做三件事：
//! 1. 从 axum 提取请求参数
//! 2. 构造 ServiceContext + service 参数 → 调用 service 方法
//! 3. 将 ServiceError → AppError，返回 HTTP 响应

#![allow(dead_code)]

use axum::{
    extract::{Query, State},
    Json,
};
use serde::Deserialize;

use core_lib::service::{
    AiService, ConfigService, CrudService, SchemaService, WorkbenchService,
    config::UpdateConfigParams,
    crud::{CrudDeleteParams, CrudMutationParams, CrudResult},
    schema::{GetTableDataParams, GetTableDataResult},
    workbench::{ExecuteCancelParams, ExecuteCancelResult, ExecuteParams, ExecuteResult, ExecuteTransactionParams, ExecuteTransactionResult},
    ai::{AiChatRequest, AiChatResult},
};

use crate::bridge::bridge_service_context;
use crate::state::AppState;

use core_lib::config::AppConfig;
use core_lib::error::AppError;
use core_lib::schema::SchemaResponse;

// ── 配置 ────────────────────────────────────────────────

pub async fn get_config(State(state): State<AppState>) -> Json<serde_json::Value> {
    let ctx = bridge_service_context(&state);
    Json(ConfigService::get_config(&ctx).await)
}

pub async fn update_config(
    State(state): State<AppState>,
    Json(new_config): Json<AppConfig>,
) -> Result<Json<serde_json::Value>, AppError> {
    let ctx = bridge_service_context(&state);
    let result = ConfigService::update_config(&ctx, UpdateConfigParams { new_config })
        .await
        .map_err(|e| e.to_app_error())?;
    Ok(Json(result))
}

// ── Schema ──────────────────────────────────────────────

#[derive(Deserialize)]
pub(crate) struct DbContextQuery {
    db_id: Option<String>,
}

pub async fn get_schema(
    State(state): State<AppState>,
    Query(query): Query<DbContextQuery>,
) -> Result<Json<SchemaResponse>, AppError> {
    let ctx = bridge_service_context(&state);
    let result = SchemaService::get_schema(&ctx, query.db_id.as_deref())
        .await
        .map_err(|e| e.to_app_error())?;
    Ok(Json(result))
}

#[derive(Deserialize)]
pub(crate) struct ParseSchemaRequest {
    sql_content: String,
}

pub async fn parse_schema(
    State(state): State<AppState>,
    Json(req): Json<ParseSchemaRequest>,
) -> Result<Json<SchemaResponse>, AppError> {
    let ctx = bridge_service_context(&state);
    let result = SchemaService::parse_virtual_schema(&ctx, &req.sql_content)
        .await
        .map_err(|e| e.to_app_error())?;
    Ok(Json(result))
}

// ── SQL 执行 ────────────────────────────────────────────

pub async fn execute_sql(
    State(state): State<AppState>,
    Json(req): Json<crate::ExecuteRequest>,
) -> Result<Json<ExecuteResult>, AppError> {
    let ctx = bridge_service_context(&state);
    let params = ExecuteParams {
        sql: req.sql,
        force: req.force,
        db_id: req.db_id,
        chunk_offset: req.chunk_offset,
        chunk_size: req.chunk_size,
        cancel_token: req.cancel_token,
        transaction_id: req.transaction_id,
    };
    let result = WorkbenchService::execute_sql(&ctx, params)
        .await
        .map_err(|e| e.to_app_error())?;
    Ok(Json(result))
}

pub async fn execute_cancel(
    State(state): State<AppState>,
    Json(req): Json<crate::ExecuteCancelRequest>,
) -> Result<Json<ExecuteCancelResult>, AppError> {
    let ctx = bridge_service_context(&state);
    let params = ExecuteCancelParams {
        cancel_token: req.cancel_token,
    };
    let result = WorkbenchService::execute_cancel(&ctx, params)
        .await
        .map_err(|e| e.to_app_error())?;
    Ok(Json(result))
}

pub async fn execute_transaction(
    State(state): State<AppState>,
    Json(req): Json<crate::ExecuteTransactionRequest>,
) -> Result<Json<ExecuteTransactionResult>, AppError> {
    let ctx = bridge_service_context(&state);
    let params = ExecuteTransactionParams {
        action: req.action,
        transaction_id: req.transaction_id,
        db_id: req.db_id,
    };
    let result = WorkbenchService::execute_transaction(&ctx, params)
        .await
        .map_err(|e| e.to_app_error())?;
    Ok(Json(result))
}

// ── CRUD ────────────────────────────────────────────────

pub async fn crud_insert(
    State(state): State<AppState>,
    Json(req): Json<crate::CrudMutationRequest>,
) -> Result<Json<CrudResult>, AppError> {
    let ctx = bridge_service_context(&state);
    let params = CrudMutationParams {
        table_name: req.table_name,
        data: req.data,
        condition: req.condition,
        db_id: req.db_id,
        transaction_id: req.transaction_id,
    };
    let result = CrudService::insert(&ctx, params)
        .await
        .map_err(|e| e.to_app_error())?;
    Ok(Json(result))
}

pub async fn crud_update(
    State(state): State<AppState>,
    Json(req): Json<crate::CrudMutationRequest>,
) -> Result<Json<CrudResult>, AppError> {
    let ctx = bridge_service_context(&state);
    let params = CrudMutationParams {
        table_name: req.table_name,
        data: req.data,
        condition: req.condition,
        db_id: req.db_id,
        transaction_id: req.transaction_id,
    };
    let result = CrudService::update(&ctx, params)
        .await
        .map_err(|e| e.to_app_error())?;
    Ok(Json(result))
}

pub async fn crud_delete(
    State(state): State<AppState>,
    Json(req): Json<crate::DeleteRequest>,
) -> Result<Json<CrudResult>, AppError> {
    let ctx = bridge_service_context(&state);
    let params = CrudDeleteParams {
        table_name: req.table_name,
        condition: req.condition,
        db_id: req.db_id,
        transaction_id: req.transaction_id,
    };
    let result = CrudService::delete(&ctx, params)
        .await
        .map_err(|e| e.to_app_error())?;
    Ok(Json(result))
}

// ── Table ───────────────────────────────────────────────

#[derive(Deserialize)]
pub(crate) struct GetTableDataQuery {
    table_name: String,
    page: Option<u32>,
    page_size: Option<u32>,
    filters: Option<String>,
    orders: Option<String>,
    db_id: Option<String>,
}

pub async fn get_table_data(
    State(state): State<AppState>,
    Query(req): Query<GetTableDataQuery>,
) -> Result<Json<GetTableDataResult>, AppError> {
    let ctx = bridge_service_context(&state);
    let params = GetTableDataParams {
        table_name: req.table_name,
        page: req.page,
        page_size: req.page_size,
        filters: req.filters,
        orders: req.orders,
        db_id: req.db_id,
    };
    let result = SchemaService::get_table_data(&ctx, params)
        .await
        .map_err(|e| e.to_app_error())?;
    Ok(Json(result))
}

#[derive(Deserialize)]
pub(crate) struct GetTableSchemaQuery {
    table_name: String,
    db_id: Option<String>,
}

pub async fn get_table_schema(
    State(state): State<AppState>,
    Query(req): Query<GetTableSchemaQuery>,
) -> Result<Json<core_lib::schema::TableWithDetails>, AppError> {
    let ctx = bridge_service_context(&state);
    let result = SchemaService::get_table_schema(&ctx, req.db_id.as_deref(), &req.table_name)
        .await
        .map_err(|e| e.to_app_error())?;
    Ok(Json(result))
}

// ── AI ────────────────────────────────────────────────────

/// 非流式 AI 查询 — 通过 AiService 调用
pub async fn ai_query(
    State(state): State<AppState>,
    Json(req): Json<crate::ai_handlers::AiQueryRequest>,
) -> Result<Json<crate::ai_handlers::AiQueryResponse>, AppError> {
    let ctx = bridge_service_context(&state);
    // 在传给 AiChatRequest 之前先 clone current_sql（explain 模式可能还需要原始值）
    let current_sql_for_fallback = req.current_sql.clone();
    let chat_req = AiChatRequest {
        query: req.query,
        mode: req.mode,
        current_sql: req.current_sql,
        chat_history: req.chat_history,
        extra_guidance: None,
    };
    let result = AiService::chat(&ctx, chat_req)
        .await
        .map_err(|e| e.to_app_error())?;

    let AiChatResult { agent_result, normalized_mode } = result;

    // 日志元数据
    crate::ai_handlers::log_ai_intent_metadata(
        "/api/ai/query",
        &agent_result.sql,
        agent_result.task_type.as_deref(),
        agent_result.sql_empty_reason.as_deref(),
        &agent_result.missing_information,
    );

    // explain 模式特殊处理
    let explanation_only = normalized_mode.as_deref() == Some("explain")
        && agent_result
            .explanation
            .as_deref()
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false);
    if agent_result.sql.trim().is_empty() && !explanation_only {
        return Err(AppError::ParseError(
            agent_result
                .explanation
                .unwrap_or_else(|| "AI 返回无法解析为 SQL。".to_string()),
        ));
    }

    let response_sql = if explanation_only && agent_result.sql.trim().is_empty() {
        current_sql_for_fallback.unwrap_or_default()
    } else {
        agent_result.sql
    };

    Ok(Json(crate::ai_handlers::AiQueryResponse {
        sql: response_sql,
        explanation: agent_result.explanation,
        task_type: agent_result.task_type,
        sql_empty_reason: agent_result.sql_empty_reason,
        missing_information: agent_result.missing_information,
        grounding_evidence: agent_result.grounding_evidence,
        assumptions: agent_result.assumptions,
        referenced_tables: agent_result.referenced_tables,
        risk_level: agent_result.risk_level,
        needs_confirmation: agent_result.needs_confirmation,
    }))
}

/// 非流式 chat — 通过 AiService 调用
pub async fn chat_to_sql(
    State(state): State<AppState>,
    Json(req): Json<crate::ai_handlers::ChatRequest>,
) -> Result<Json<crate::ai_handlers::ChatResponse>, AppError> {
    let ctx = bridge_service_context(&state);
    let chat_req = AiChatRequest {
        query: req.query,
        mode: req.mode,
        current_sql: req.current_sql,
        chat_history: req.chat_history,
        extra_guidance: None,
    };
    let result = AiService::chat(&ctx, chat_req)
        .await
        .map_err(|e| e.to_app_error())?;

    let AiChatResult { agent_result, normalized_mode: _ } = result;

    crate::ai_handlers::log_ai_intent_metadata(
        "/chat",
        &agent_result.sql,
        agent_result.task_type.as_deref(),
        agent_result.sql_empty_reason.as_deref(),
        &agent_result.missing_information,
    );

    Ok(Json(crate::ai_handlers::ChatResponse {
        sql: agent_result.sql,
        explanation: agent_result.explanation,
        task_type: agent_result.task_type,
        sql_empty_reason: agent_result.sql_empty_reason,
        missing_information: agent_result.missing_information,
        grounding_evidence: agent_result.grounding_evidence,
        assumptions: agent_result.assumptions,
        referenced_tables: agent_result.referenced_tables,
        risk_level: agent_result.risk_level,
        needs_confirmation: agent_result.needs_confirmation,
    }))
}

/// 流式 chat — 通过 AiService 调用，返回 SSE
pub async fn chat_to_sql_stream(
    State(state): State<AppState>,
    Json(req): Json<crate::ai_handlers::ChatRequest>,
) -> Result<axum::response::sse::Sse<std::pin::Pin<Box<dyn futures::stream::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>> + Send>>>, AppError> {
    use axum::response::sse::{Event, Sse};
    use futures::StreamExt;

    let ctx = bridge_service_context(&state);
    let chat_req = AiChatRequest {
        query: req.query,
        mode: req.mode,
        current_sql: req.current_sql,
        chat_history: req.chat_history,
        extra_guidance: None,
    };
    let cancel_token = tokio_util::sync::CancellationToken::new();

    let agent_stream = AiService::chat_stream(&ctx, chat_req, cancel_token)
        .await
        .map_err(|e| e.to_app_error())?;

    let sse_stream = agent_stream.map(|event| {
        let event_type = event.event_type().to_string();
        let json_str = serde_json::to_string(&event.data_json()).unwrap_or_default();
        Ok(Event::default().event(event_type).data(json_str))
    });

    Ok(Sse::new(Box::pin(sse_stream)))
}