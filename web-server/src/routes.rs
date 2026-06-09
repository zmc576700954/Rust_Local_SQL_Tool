//! routes.rs — 统一路由注册
//!
//! 将原来散落在 main.rs 里的 71 条路由集中到此处，
//! handler 仅做 HTTP 参数提取 → service 调用 → 响应转换。

#![allow(dead_code)]

use axum::{
    routing::{get, post},
    Router,
};

use crate::state::AppState;

// handler 模块引用
use crate::handlers::*;
use crate::ai_handlers;
use crate::service_handlers;

/// 构建 /backend 下的所有 API 路由
pub fn api_routes(state: AppState) -> Router<AppState> {
    Router::new()
        // ── Config ────────────────────────────────────
        .route("/config", get(service_handlers::get_config).post(service_handlers::update_config))

        // ── DB ────────────────────────────────────────
        .route("/db/test", post(crate::db_test))

        // ── Schema ────────────────────────────────────
        .route("/schema", get(service_handlers::get_schema))
        .route("/schema/parse", post(service_handlers::parse_schema))

        // ── Execute ───────────────────────────────────
        .route("/execute", post(service_handlers::execute_sql))
        .route("/execute/transaction", post(service_handlers::execute_transaction))
        .route("/execute/cancel", post(service_handlers::execute_cancel))

        // ── CRUD ──────────────────────────────────────
        .route("/crud/insert", post(service_handlers::crud_insert))
        .route("/crud/update", post(service_handlers::crud_update))
        .route("/crud/delete", post(service_handlers::crud_delete))

        // ── Table ─────────────────────────────────────
        .route("/table/data", get(service_handlers::get_table_data))
        .route("/table/schema", get(service_handlers::get_table_schema))
        .route("/table/ddl/preview", post(crate::preview_ddl))
        .route("/table/ddl", post(crate::execute_ddl))

        // ── AI ────────────────────────────────────────
        .route("/chat", post(service_handlers::chat_to_sql))
        .route("/chat/stream", post(service_handlers::chat_to_sql_stream))
        .route("/api/ai/models", get(ai_handlers::ai_models))
        .route("/api/ai/provider/models", post(ai_handlers::fetch_provider_models))
        .route("/api/ai/health", get(ai_handlers::ai_health))
        .route("/api/ai/query", post(service_handlers::ai_query))
        .route("/api/ai/explain_error", post(ai_handlers::ai_explain_error))
        .route("/api/ai/knowledge", get(ai_handlers::get_knowledge).post(ai_handlers::add_knowledge).put(ai_handlers::update_knowledge))
        .route("/api/ai/knowledge/delete", post(ai_handlers::delete_knowledge))

        // ── Policy ────────────────────────────────────
        .route("/policy", get(crate::get_policy))
        .route("/policy/reset", post(crate::reset_policy))
        .route("/policy/snapshot", post(crate::snapshot_policy))
        .route("/policy/rollback", post(crate::rollback_policy))

        // ── Rules ─────────────────────────────────────
        .route("/rules", get(crate::get_rules))
        .route("/rules/save", post(crate::save_rule))
        .route("/rules/delete", post(crate::delete_rule))

        // ── Diagnostics / Perf ────────────────────────
        .route("/diagnostics/perf/probe", post(diagnostics_perf_probe))
        .route("/diagnostics/perf/suites", get(diagnostics_perf_suite_list).post(diagnostics_perf_suite_save))
        .route("/diagnostics/perf/suites/baseline", get(diagnostics_perf_suite_baseline_get).post(diagnostics_perf_suite_baseline_pin))
        .route("/diagnostics/perf/suite-diffs", get(diagnostics_perf_suite_diff_list).post(diagnostics_perf_suite_diff_save))
        .route("/diagnostics/perf/suites/:suite_id", get(diagnostics_perf_suite_detail))

        // ── Tools / Jobs ──────────────────────────────
        .route("/tools/mock-data", post(crate::generate_mock_data))
        .route("/tools/export", post(crate::export_data))
        .route("/tools/import", post(crate::import_data))
        .route("/tools/jobs/export/start", post(export_job_start))
        .route("/tools/jobs/import/start", post(import_job_start))
        .route("/tools/jobs/import-sql/start", post(import_sql_job_start))
        .route("/tools/jobs/go-live/start", post(go_live_job_start))
        .route("/tools/go-live/reports", get(go_live_reports_list))
        .route("/tools/go-live/audit", get(go_live_audit_list))
        .route("/tools/jobs/:job_id", get(tool_job_status))
        .route("/tools/jobs/:job_id/cancel", post(tool_job_cancel))
        .route("/tools/jobs/:job_id/artifacts/:artifact", get(tool_job_artifact_download))

        // ── Sync ──────────────────────────────────────
        .route("/tools/schema-sync/diff", post(crate::sync_schema_diff))
        .route("/tools/schema-sync/ddl", post(crate::sync_schema_ddl))
        .route("/tools/data-sync/diff", post(crate::sync_data_diff))
        .route("/tools/data-sync/dml", post(crate::sync_data_dml))
        .route("/tools/data-sync/compare", post(mysql_sync_compare))
        .route("/tools/data-sync/preview", post(mysql_sync_preview))
        .route("/tools/data-sync/deploy", post(mysql_sync_deploy))
        .route("/tools/data-sync/jobs/:job_id", get(mysql_sync_job_status))
        .route("/tools/perf-sync/start", post(perf_sync_start))
        .route("/tools/perf-sync/check", post(perf_sync_check))
        .route("/tools/perf-sync/jobs/:job_id", get(perf_sync_job_status))

        // ── Transfer ──────────────────────────────────
        .route("/tools/data-transfer/upload", post(crate::transfer_upload))
        .route("/tools/data-transfer/execute", post(crate::transfer_execute))

        // ── Navicat ───────────────────────────────────
        .route("/navicat/parse", post(crate::parse_navicat))

        // ── SQL History ───────────────────────────────
        .route("/sql/history", get(crate::get_history).post(crate::clear_history))
        .route("/sql/explain", post(crate::explain_sql))
        .route("/sql/session-info", get(crate::session_info))

        // ── 中间件 ────────────────────────────────────
        .layer(axum::extract::DefaultBodyLimit::max(
            state.limits.max_file_bytes as usize,
        ))
}