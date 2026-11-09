#![recursion_limit = "256"]

mod ai_handlers;
mod bridge;
mod handlers;
#[cfg(test)]
mod import_sql_integration_mysql;
mod inspection_handlers;
mod inspection_scheduler;
mod mysql_codec;
mod ops_handlers;
mod routes;
mod service_handlers;
mod ssh;
mod state;

use state::*;
use ssh::*;
use mysql_codec::*;
use handlers::*;

use axum::{
    extract::{Query, State},
    http::{HeaderMap, Request, StatusCode},
    middleware::Next,
    response::Response,
    routing::{get, post},
    Json, Router,
};
use core_lib::error::{with_locale, AppError};
use axum::extract::Multipart;
use core_lib::transfer::{TransferConfig, TransferEngine};
use sqlx::Row;
use std::io::Write;

// ----------------- Transfer Handlers -----------------

#[derive(serde::Serialize)]
struct UploadResponse {
    columns: Vec<String>,
    preview_data: Vec<Vec<String>>,
    source_path: String,
}

fn normalize_locale(headers: &HeaderMap) -> String {
    if let Some(v) = headers.get("x-locale").and_then(|v| v.to_str().ok()) {
        let v = v.trim().to_lowercase();
        if v.starts_with("zh") {
            return "zh".to_string();
        }
        if v.starts_with("en") {
            return "en".to_string();
        }
    }
    if let Some(v) = headers
        .get(axum::http::header::ACCEPT_LANGUAGE)
        .and_then(|v| v.to_str().ok())
    {
        let v = v.trim().to_lowercase();
        if v.starts_with("zh") {
            return "zh".to_string();
        }
        if v.starts_with("en") {
            return "en".to_string();
        }
    }
    "en".to_string()
}

async fn set_request_locale(req: Request<axum::body::Body>, next: Next) -> Response {
    let locale = normalize_locale(req.headers());
    with_locale(locale, async move { next.run(req).await }).await
}

async fn transfer_upload(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<UploadResponse>, AppError> {
    let limits = state.limits.clone();
    let mut file_data = None;
    let mut file_name_opt = None;
    let mut delimiter = b',';
    let mut _encoding = "utf-8".to_string();

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::BadRequest(e.to_string()))?
    {
        if let Some(name) = field.name() {
            let name_str = name.to_string();
            if name_str == "file" {
                file_name_opt = Some(field.file_name().unwrap_or("upload.csv").to_string());
                let data = field
                    .bytes()
                    .await
                    .map_err(|e| AppError::BadRequest(e.to_string()))?;
                if data.len() as u64 > limits.max_file_bytes {
                    return Err(AppError::PayloadTooLarge(format!(
                        "upload too large: bytes={}B, max={}B",
                        data.len(),
                        limits.max_file_bytes
                    )));
                }
                file_data = Some(data);
            } else if name_str == "delimiter" {
                let val = field
                    .text()
                    .await
                    .map_err(|e| AppError::BadRequest(e.to_string()))?;
                if !val.is_empty() {
                    delimiter = val.as_bytes()[0];
                }
            } else if name_str == "encoding" {
                _encoding = field
                    .text()
                    .await
                    .map_err(|e| AppError::BadRequest(e.to_string()))?;
            }
        }
    }

    if let (Some(data), Some(file_name)) = (file_data, file_name_opt) {
        ensure_temp_quota(&limits, data.len() as u64).await?;
        let temp_path = format!(
            "{}/{}",
            limits.temp_dir.trim_end_matches('/'),
            uuid::Uuid::new_v4()
        );
        let mut f = std::fs::File::create(&temp_path)
            .map_err(|e| AppError::InternalError(e.to_string()))?;

        // If SQL file, just return it as a single column or special format?
        // Actually, if it's SQL, we could just return the raw SQL in DML and skip mapping.
        // But for now let's just write the data.
        f.write_all(&data)
            .map_err(|e| AppError::InternalError(e.to_string()))?;

        if file_name.ends_with(".sql") {
            // For SQL files, we can just return a single column and the content as preview
            // Or we can just read the file content and return as DML directly later.
            // Let's return a dummy mapping for SQL.
            let content = String::from_utf8_lossy(&data).to_string();
            return Ok(Json(UploadResponse {
                columns: vec!["sql_content".to_string()],
                preview_data: vec![vec![content.chars().take(100).collect::<String>() + "..."]],
                source_path: temp_path,
            }));
        }

        // Parse CSV/TXT
        let result = TransferEngine::parse_local_file(&temp_path, delimiter, true)
            .map_err(|e| AppError::BadRequest(e.to_string()))?;

        return Ok(Json(UploadResponse {
            columns: result.columns,
            preview_data: result.preview_data,
            source_path: temp_path,
        }));
    }

    Err(AppError::BadRequest("No file uploaded".to_string()))
}

async fn transfer_execute(
    State(state): State<AppState>,
    Json(mut config): Json<TransferConfig>,
) -> Result<Json<serde_json::Value>, AppError> {
    if config.source_type == "network_db" {
        if let Some(ref db_id) = config.source_db_id {
            let app_config = state.config.read().await.clone();
            if let Some(conn) = app_config.db_connections.iter().find(|c| &c.id == db_id) {
                config.source_url = Some(conn.url.clone());
            } else {
                return Err(AppError::BadRequest(
                    "Source DB connection not found".into(),
                ));
            }
        }
    }

    if let Some(ref target_db_id) = config.target_db_id {
        let app_config = state.config.read().await.clone();
        if let Some(conn) = app_config
            .db_connections
            .iter()
            .find(|c| &c.id == target_db_id)
        {
            config.target_url = conn.url.clone();
        } else {
            return Err(AppError::BadRequest(
                "Target DB connection not found".into(),
            ));
        }
    }

    let report = TransferEngine::execute_transfer_with_report(&config)
        .await
        .map_err(|e| AppError::BadRequest(e.to_string()))?;
    let compared = report.insert_count + report.update_count + report.unchanged_count;
    let changed = report.insert_count + report.update_count;
    if report.compare_based && compared >= 200 && changed.saturating_mul(100) / compared >= 85 {
        return Err(AppError::BadRequest(GAP_TOO_LARGE_MSG.to_string()));
    }

    if config.source_type == "local_file" {
        if let Some(p) = config.source_path.as_ref() {
            // Validate source_path is within the temp directory and has a UUID filename
            let temp_base = state.limits.temp_dir.trim_end_matches('/');
            let canonical = std::path::Path::new(p)
                .canonicalize()
                .unwrap_or_default();
            let canonical_str = canonical.to_string_lossy();
            if !canonical_str.starts_with(temp_base) {
                return Err(AppError::BadRequest(
                    "source_path must be within the temp directory".into(),
                ));
            }
            if let Some(filename) = canonical.file_name().and_then(|f| f.to_str()) {
                if uuid::Uuid::parse_str(filename).is_err() {
                    return Err(AppError::BadRequest(
                        "source_path filename must be a valid UUID".into(),
                    ));
                }
            } else {
                return Err(AppError::BadRequest(
                    "source_path has an invalid filename".into(),
                ));
            }
            let _ = tokio::fs::remove_file(&canonical).await;
        }
    }
    let dml = report.dml;
    let insert_count = report.insert_count;
    let update_count = report.update_count;
    let unchanged_count = report.unchanged_count;
    let compare_based = report.compare_based;
    Ok(Json(serde_json::json!({
        "dml": dml,
        "insert_count": insert_count,
        "update_count": update_count,
        "unchanged_count": unchanged_count,
        "compare_based": compare_based
    })))
}

// ----------------- Rule Management Handlers -----------------

async fn get_rules(State(state): State<AppState>) -> Result<Json<Vec<Rule>>, AppError> {
    let store = state.rule_store.read().await;
    Ok(Json(store.rules.clone()))
}

#[derive(Deserialize)]
struct ImportRequest {
    table_name: String,
    data: Vec<std::collections::HashMap<String, serde_json::Value>>,
    mapping: std::collections::HashMap<String, String>, // db_column -> source_field
    #[serde(default)]
    skip_errors: bool,
    /// Target connection; when omitted the currently active connection is used.
    db_id: Option<String>,
}

#[derive(Serialize)]
struct ImportResponse {
    success: bool,
    completed: bool,
    inserted: usize,
    errors: usize,
    total: usize,
    failed_row: Option<usize>,
    remaining: usize,
    failure_policy: &'static str,
    error_details: Vec<String>,
    message: String,
}

async fn import_data(
    State(state): State<AppState>,
    Json(req): Json<ImportRequest>,
) -> Result<Json<ImportResponse>, AppError> {
    let db_client = if let Some(db_id) = req.db_id.as_deref() {
        get_temp_db_client(&state, db_id).await?.0
    } else {
        state
            .db_client
            .read()
            .await
            .clone()
            .ok_or_else(|| AppError::BadRequest("Database not connected".to_string()))?
    };

    let table_name = req.table_name;
    let total = req.data.len();
    let mut inserted = 0;
    let mut errors = 0;
    let mut error_details = Vec::new();
    let mut failed_positions: Vec<usize> = Vec::new();
    let mut stopped = false;

    // Filter mapping to only include mapped columns
    let mapped_cols: Vec<(String, String)> = req
        .mapping
        .into_iter()
        .filter(|(_, src)| !src.is_empty())
        .collect();

    if mapped_cols.is_empty() {
        return Err(AppError::BadRequest("No columns mapped".to_string()));
    }

    let db_type = db_client.db_type.clone();
    let mut db_col_names: Vec<String> = Vec::with_capacity(mapped_cols.len());
    for (db, _) in &mapped_cols {
        db_col_names.push(quote_ident_for(&db_type, db)?);
    }
    let col_list = db_col_names.join(", ");
    let table_ident = core_lib::sql_util::quote_table_ref_checked(&table_name, &db_type)
        .map_err(AppError::BadRequest)?;

    let batch_size: usize = 500;
    let mut batch_values: Vec<String> = Vec::with_capacity(batch_size);
    let mut batch_params: Vec<serde_json::Value> =
        Vec::with_capacity(batch_size * mapped_cols.len());

    'rows: for (i, row) in req.data.iter().enumerate() {
        let values: Vec<serde_json::Value> = mapped_cols
            .iter()
            .map(|(_, src_field)| {
                row.get(src_field)
                    .cloned()
                    .unwrap_or(serde_json::Value::Null)
            })
            .collect();

        // 占位符按引擎生成：MySQL 用 `?` 并绑定；PostgreSQL / SQLite 生成转义后的字面量。
        if matches!(db_type, core_lib::config::DbType::MySQL | core_lib::config::DbType::MariaDB) {
            batch_values.push(format!(
                "({})",
                vec!["?"; mapped_cols.len()].join(", ")
            ));
            batch_params.extend(values);
        } else {
            batch_values.push(format!(
                "({})",
                values
                    .iter()
                    .map(core_lib::sql_util::format_sql_value)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }

        let is_last = i + 1 == req.data.len();
        if batch_values.len() >= batch_size || is_last {
            let batch_sql = format!(
                "INSERT INTO {} ({}) VALUES {}",
                table_ident,
                col_list,
                batch_values.join(", ")
            );

            match execute_import_sql(&db_client, &batch_sql, &batch_params, &db_type).await {
                Ok(_) => {
                    inserted += batch_values.len();
                }
                Err(_batch_err) => {
                    // 批语句失败无法定位具体行：回退为逐行执行，找到失败行后按策略处理。
                    let batch_start = i + 1 - batch_values.len();
                    for (j, chunk) in batch_values.iter().enumerate() {
                        let row_sql = format!(
                            "INSERT INTO {} ({}) VALUES {}",
                            table_ident, col_list, chunk
                        );
                        let row_values = if matches!(
                            db_type,
                            core_lib::config::DbType::MySQL | core_lib::config::DbType::MariaDB
                        ) {
                            let row_start = j * mapped_cols.len();
                            batch_params[row_start..row_start + mapped_cols.len()].to_vec()
                        } else {
                            Vec::new()
                        };
                        match execute_import_sql(&db_client, &row_sql, &row_values, &db_type).await {
                            Ok(_) => inserted += 1,
                            Err(row_err) => {
                                errors += 1;
                                let row_no = batch_start + j + 1;
                                failed_positions.push(row_no);
                                error_details.push(format!("Row {}: {}", row_no, row_err));
                                if !req.skip_errors {
                                    stopped = true;
                                    break 'rows;
                                }
                            }
                        }
                    }
                }
            }

            batch_values.clear();
            batch_params.clear();
        }
    }

    let processed = inserted + errors;
    let success = errors == 0;
    let failed_row = failed_positions.first().copied();
    let remaining = if stopped {
        total.saturating_sub(processed)
    } else {
        0
    };
    let message = if success {
        format!("已成功写入 {} 行。", inserted)
    } else if req.skip_errors {
        format!(
            "已写入 {} 行，失败 {} 行（行号：{}），已跳过失败行继续执行。",
            inserted,
            errors,
            failed_positions
                .iter()
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )
    } else {
        format!(
            "已成功写入 {} 行，第 {} 行失败，剩余 {} 行未写入（遇错即停）。错误：{}",
            inserted,
            failed_row.unwrap_or(0),
            remaining,
            error_details.first().cloned().unwrap_or_default()
        )
    };

    Ok(Json(ImportResponse {
        success,
        completed: !stopped,
        inserted,
        errors,
        total,
        failed_row,
        remaining,
        failure_policy: if req.skip_errors { "continue" } else { "stop" },
        error_details,
        message,
    }))
}

/// 按引擎引用列名 / 表名。
pub(crate) fn quote_ident_for(
    db_type: &core_lib::config::DbType,
    s: &str,
) -> Result<String, AppError> {
    match db_type {
        core_lib::config::DbType::MySQL | core_lib::config::DbType::MariaDB => {
            core_lib::sql::util::quote_ident_mysql_checked(s).map_err(AppError::BadRequest)
        }
        _ => core_lib::sql_util::quote_ident_pg_checked(s).map_err(AppError::BadRequest),
    }
}

/// 执行导入 SQL：MySQL 绑定参数，PostgreSQL / SQLite 使用已内联的字面量。
async fn execute_import_sql(
    db_client: &core_lib::db::DbClient,
    sql: &str,
    values: &[serde_json::Value],
    db_type: &core_lib::config::DbType,
) -> Result<u64, String> {
    match db_type {
        core_lib::config::DbType::MySQL | core_lib::config::DbType::MariaDB => {
            let mut query = sqlx::query(sql);
            for val in values {
                query = bind_json_value_to_query(query, val);
            }
            query
                .execute(db_client.pool.mysql().map_err(|e| e.to_string())?)
                .await
                .map(|r| r.rows_affected())
                .map_err(|e| e.to_string())
        }
        _ => db_client
            .pool
            .execute_affected(sql)
            .await
            .map_err(|e| e.to_string()),
    }
}

#[derive(Deserialize)]
struct SaveRuleRequest {
    prompt: String,
    sql: String,
}

async fn save_rule(
    State(state): State<AppState>,
    Json(req): Json<SaveRuleRequest>,
) -> Result<Json<Rule>, AppError> {
    // Call AI to extract templates
    let config = state.config.read().await.clone();
    let sql_template = match core_lib::ai::agent::generate_rule_template(&config, &req.prompt, &req.sql).await {
        Ok(res) => res,
        Err(e) => {
            tracing::warn!("AI template extraction failed, falling back to raw SQL: {:?}", e);
            req.sql.clone()
        }
    };

    let new_rule = Rule {
        id: uuid::Uuid::new_v4().to_string(),
        rule_type: if sql_template.contains("{{") {
            RuleType::Template
        } else {
            RuleType::Module
        },
        prompt_pattern: req.prompt,
        sql_template,
        hit_count: 0,
        updated_at: chrono::Utc::now().timestamp(),
    };

    let store_clone = {
        let mut store = state.rule_store.write().await;
        store.add_rule(new_rule.clone());
        store.clone()
    };
    store_clone
        .save()
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;

    // Evolve policy
    let store_clone = state.policy.clone();
    tokio::spawn(async move {
        match PolicyStore::evolve_policy("save_rule").await {
            Ok(new_policy) => {
                let mut policy_write = store_clone.write().await;
                *policy_write = new_policy;
                tracing::info!("Policy evolved after save_rule");
            }
            Err(e) => {
                tracing::error!("Failed to evolve policy: {:?}", e);
            }
        }
    });

    Ok(Json(new_rule))
}

#[derive(Deserialize)]
struct DeleteRuleRequest {
    id: String,
}

async fn delete_rule(
    State(state): State<AppState>,
    Json(req): Json<DeleteRuleRequest>,
) -> Result<StatusCode, AppError> {
    let store_clone = {
        let mut store = state.rule_store.write().await;
        store
            .delete_rule(&req.id)
            .map_err(|e| AppError::BadRequest(e.to_string()))?;
        store.clone()
    };
    store_clone
        .save()
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    Ok(StatusCode::OK)
}

#[derive(Deserialize)]
struct NavicatParseRequest {
    xml_content: String,
}

#[derive(Serialize)]
struct NavicatParseResponse {
    connections: Vec<NavicatConnection>,
}

async fn parse_navicat(
    Json(req): Json<NavicatParseRequest>,
) -> Result<Json<NavicatParseResponse>, AppError> {
    let connections = NavicatParser::parse_ncx(&req.xml_content)
        .map_err(|e| AppError::BadRequest(e.to_string()))?;

    Ok(Json(NavicatParseResponse { connections }))
}

async fn db_test(Json(req): Json<DbTestRequest>) -> Result<Json<DbTestResponse>, AppError> {
    // 按 URL scheme 识别引擎；仅阻止尚未实现连接的引擎。
    let requested_type = req
        .db_url
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .and_then(core_lib::config::DbType::from_url);
    if let Some(db_type) = requested_type.as_ref() {
        if !matches!(
            db_type,
            core_lib::config::DbType::MySQL
                | core_lib::config::DbType::MariaDB
                | core_lib::config::DbType::PostgreSQL
                | core_lib::config::DbType::SQLite
        ) {
            return Ok(Json(db_test_failed(
                "validation",
                "DB_TEST_UNSUPPORTED_TYPE",
                "连接测试暂不支持该数据库类型。",
                Some("当前支持 MySQL / MariaDB / PostgreSQL / SQLite。"),
                Some(format!("db_type={}", db_type.display_name())),
            )));
        }
    }

    let policy = TimeoutPolicy::default();
    let mut db_url_for_connect = req.db_url.clone();
    let mut host_override: Option<String> = None;
    let mut port_override: Option<u16> = None;
    let _ssh_tunnel = if req.ssh_enabled.unwrap_or(false) {
        let ssh_host = req
            .ssh_host
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.to_string());
        let ssh_username = req
            .ssh_username
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.to_string());
        let ssh_password = req.ssh_password.clone().filter(|s| !s.is_empty());
        if ssh_host.is_none() || ssh_username.is_none() || ssh_password.is_none() {
            return Ok(Json(db_test_failed(
                "validation",
                "DB_TEST_SSH_MISSING_FIELDS",
                "SSH 参数不完整，请检查 SSH Host、用户名、密码。",
                Some("启用 SSH 时，Host/Username/Password 均为必填。"),
                None,
            )));
        }
        let ssh_port = req.ssh_port.unwrap_or(22);
        let (remote_host, remote_port) =
            if let Some(db_url) = req.db_url.as_deref().filter(|s| !s.trim().is_empty()) {
                match extract_target_host_port_from_url(db_url) {
                    Ok(v) => v,
                    Err(e) => return Ok(Json(classify_ssh_setup_error(&e))),
                }
            } else {
                let host = req
                    .host
                    .as_deref()
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_default()
                    .to_string();
                let port = req.port.unwrap_or(3306);
                if host.is_empty() {
                    return Ok(Json(db_test_failed(
                        "validation",
                        "DB_TEST_MISSING_FIELDS",
                        "连接参数不完整，host 和 username 为必填项。",
                        Some("请填写主机地址和用户名后重试。"),
                        None,
                    )));
                }
                (host, port)
            };

        let tunnel_cfg = SshTunnelConfig {
            ssh_host: ssh_host.unwrap_or_default(),
            ssh_port,
            ssh_username: ssh_username.unwrap_or_default(),
            ssh_password: ssh_password.unwrap_or_default(),
            remote_host,
            remote_port,
        };
        let tunnel = match start_ssh_tunnel(tunnel_cfg) {
            Ok(v) => v,
            Err(e) => return Ok(Json(classify_ssh_setup_error(&e))),
        };
        if let Some(db_url) = req.db_url.as_deref().filter(|s| !s.trim().is_empty()) {
            db_url_for_connect = match rewrite_db_url_with_local_tunnel(db_url, tunnel.local_port) {
                Ok(v) => Some(v),
                Err(e) => return Ok(Json(classify_ssh_setup_error(&e))),
            };
        } else {
            host_override = Some("127.0.0.1".to_string());
            port_override = Some(tunnel.local_port);
        }
        Some(tunnel)
    } else {
        None
    };

    // 非 MySQL 引擎走独立的 sqlx 连接测试；MySQL 保持原有路径。
    let detected_type = requested_type.unwrap_or(core_lib::config::DbType::MySQL);
    if matches!(
        detected_type,
        core_lib::config::DbType::PostgreSQL | core_lib::config::DbType::SQLite
    ) {
        return Ok(Json(
            db_test_sqlx_engine(
                detected_type,
                db_url_for_connect.as_deref(),
                req.host.as_deref(),
                req.username.as_deref(),
                req.password.as_deref(),
                host_override,
                port_override,
                req.ssl_mode.as_deref(),
                req.probe_capabilities.unwrap_or(false),
                policy,
            )
            .await,
        ));
    }

    use sqlx::mysql::MySqlConnectOptions;
    use sqlx::mysql::MySqlSslMode;
    use sqlx::Row;
    use std::str::FromStr;

    let mut options = if let Some(db_url) = db_url_for_connect
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        match MySqlConnectOptions::from_str(db_url) {
            Ok(opts) => opts,
            Err(e) => {
                return Ok(Json(db_test_failed(
                    "validation",
                    "DB_TEST_INVALID_URL",
                    "连接地址格式错误，请检查 db_url。",
                    Some("示例：mysql://user:password@host:3306/dbname"),
                    Some(e.to_string()),
                )));
            }
        }
    } else {
        let host = req.host.as_deref().filter(|s| !s.trim().is_empty());
        let username = req.username.as_deref().filter(|s| !s.trim().is_empty());
        if host.is_none() || username.is_none() {
            return Ok(Json(db_test_failed(
                "validation",
                "DB_TEST_MISSING_FIELDS",
                "连接参数不完整，host 和 username 为必填项。",
                Some("请填写主机地址和用户名后重试。"),
                None,
            )));
        }
        let host = host_override.unwrap_or_else(|| host.unwrap_or_default().to_string());
        let username = username.unwrap_or_default();
        let port = port_override.unwrap_or(req.port.unwrap_or(3306));

        let mut opts = MySqlConnectOptions::new()
            .host(&host)
            .port(port)
            .username(username)
            .database("mysql");
        if let Some(password) = req.password.as_deref() {
            if !password.is_empty() {
                opts = opts.password(password);
            }
        }
        opts
    };

    if let Some(mode) = req.ssl_mode.as_deref() {
        options = match mode.to_lowercase().as_str() {
            "disabled" => options.ssl_mode(MySqlSslMode::Disabled),
            "required" => options.ssl_mode(MySqlSslMode::Required),
            "verify_ca" => options.ssl_mode(MySqlSslMode::VerifyCa),
            "verify_identity" => options.ssl_mode(MySqlSslMode::VerifyIdentity),
            _ => options,
        };
    }

    let pool_future = sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(1)
        .connect_with(options);

    let pool = match tokio::time::timeout(policy.db_connect, pool_future).await {
        Ok(pool) => pool,
        Err(_) => {
            return Ok(Json(db_test_failed(
                "timeout",
                "DB_TEST_CONNECT_TIMEOUT",
                "连接数据库超时（已超过 10 秒），请检查网络、IP 或防火墙配置是否正确。",
                Some("若是云数据库，请确认白名单及安全组规则已放行。"),
                None,
            )));
        }
    };
    let pool = match pool {
        Ok(pool) => pool,
        Err(e) => return Ok(Json(classify_db_test_connect_error(&e.to_string()))),
    };

    let ping_future = sqlx::query("SELECT 1").execute(&pool);
    match tokio::time::timeout(policy.db_query, ping_future).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            return Ok(Json(db_test_failed(
                "query",
                "DB_TEST_PING_FAILED",
                "Connection established, but ping failed.",
                Some("Check proxy restrictions or query permissions, then retry."),
                Some(e.to_string()),
            )));
        }
        Err(_) => {
            return Ok(Json(db_test_failed(
                "timeout",
                "DB_TEST_PING_TIMEOUT",
                "Connection established, but ping timed out.",
                Some("Check database load or network jitter, then retry."),
                None,
            )));
        }
    }

    let probe_capabilities = req.probe_capabilities.unwrap_or(false);
    if !probe_capabilities {
        return Ok(Json(db_test_response(
            true,
            vec![],
            db_test_diagnostic(
                "success",
                "success",
                "DB_TEST_OK",
                "Connection successful.",
                None,
                None,
            ),
            "handshake",
            false,
            None,
            None,
        )));
    }

    let server_version = match tokio::time::timeout(
        policy.db_query,
        sqlx::query("SELECT VERSION()").fetch_one(&pool),
    )
    .await
    {
        Ok(Ok(row)) => row.try_get::<String, _>(0).ok(),
        _ => None,
    };

    let rows_future = sqlx::query("SHOW DATABASES").fetch_all(&pool);
    let rows = match tokio::time::timeout(policy.db_query, rows_future).await {
        Ok(rows) => rows,
        Err(_) => {
            return Ok(Json(db_test_response(
                true,
                vec![],
                db_test_diagnostic(
                    "warning",
                    "query",
                    "DB_TEST_CAPABILITY_PROBE_FAILED",
                    "Connection successful, but capability probe timed out while listing databases.",
                    Some("Check instance load or retry capability probing later."),
                    None,
                ),
                "handshake",
                true,
                Some(false),
                server_version,
            )));
        }
    };
    let rows = match rows {
        Ok(rows) => rows,
        Err(e) => {
            return Ok(Json(db_test_response(
                true,
                vec![],
                db_test_diagnostic(
                    "warning",
                    "query",
                    "DB_TEST_CAPABILITY_PROBE_FAILED",
                    "Connection successful, but failed to list databases.",
                    Some("Check SHOW DATABASES permission or metadata query restrictions."),
                    Some(e.to_string()),
                ),
                "handshake",
                true,
                Some(false),
                server_version,
            )));
        }
    };

    let mut databases = Vec::new();
    for row in rows {
        let name: String = row.try_get(0).unwrap_or_default();
        if !name.is_empty() {
            databases.push(name);
        }
    }

    Ok(Json(db_test_response(
        true,
        databases,
        db_test_diagnostic(
            "success",
            "success",
            "DB_TEST_OK",
            "Connection successful. Capability probe completed.",
            None,
            None,
        ),
        "capabilities",
        true,
        Some(true),
        server_version,
    )))
}
/// PostgreSQL / SQLite 的真实连接测试。
///
/// 与 MySQL 路径共享同样的诊断码（超时 / 握手失败 / ping 失败 / 能力探测失败），
/// 但数据库列表与版本查询按引擎选择。
async fn db_test_sqlx_engine(
    db_type: core_lib::config::DbType,
    db_url: Option<&str>,
    host: Option<&str>,
    username: Option<&str>,
    password: Option<&str>,
    host_override: Option<String>,
    port_override: Option<u16>,
    ssl_mode: Option<&str>,
    probe_capabilities: bool,
    policy: TimeoutPolicy,
) -> DbTestResponse {
    use core_lib::config::DbType;
    use core_lib::db::connect::DbClientConnectOptions;

    let url = match db_url.filter(|s| !s.trim().is_empty()) {
        Some(url) => url.to_string(),
        None => {
            if db_type == DbType::SQLite {
                return db_test_failed(
                    "validation",
                    "DB_TEST_MISSING_URL",
                    "SQLite 连接需要提供 sqlite:// 文件路径。",
                    Some("示例：sqlite:///Users/me/data/app.db"),
                    None,
                );
            }
            let host = host_override
                .or_else(|| host.map(|s| s.to_string()))
                .unwrap_or_default();
            let user = username.unwrap_or_default();
            if host.trim().is_empty() || user.trim().is_empty() {
                return db_test_failed(
                    "validation",
                    "DB_TEST_MISSING_FIELDS",
                    "连接参数不完整，host 和 username 为必填项。",
                    Some("请填写主机地址和用户名后重试。"),
                    None,
                );
            }
            let port = port_override.unwrap_or(5432);
            match password.filter(|p| !p.is_empty()) {
                Some(p) => format!("postgres://{}:{}@{}:{}/postgres", user, p, host, port),
                None => format!("postgres://{}@{}:{}/postgres", user, host, port),
            }
        }
    };

    let mut pool_config = core_lib::config::PoolConfig::default();
    pool_config.max_connections = 1;
    pool_config.min_connections = 0;

    let connect = core_lib::db::DbClient::new_with_options(
        &url,
        &pool_config,
        &db_type,
        DbClientConnectOptions {
            ssl_mode: ssl_mode.map(|s| s.to_string()),
        },
        None,
    );
    let client = match tokio::time::timeout(policy.db_connect, connect).await {
        Err(_) => {
            return db_test_failed(
                "timeout",
                "DB_TEST_CONNECT_TIMEOUT",
                "连接数据库超时（已超过 10 秒），请检查网络、IP 或防火墙配置是否正确。",
                Some("若是云数据库，请确认白名单及安全组规则已放行。"),
                None,
            );
        }
        Ok(Ok(client)) => client,
        Ok(Err(e)) => {
            use core_lib::db::DbError;
            let msg = e.to_string();
            if let DbError::Unsupported(_) = e {
                return db_test_failed(
                    "validation",
                    "DB_TEST_INVALID_URL",
                    "连接地址格式错误，请检查 db_url。",
                    Some("PostgreSQL 示例：postgres://user:password@host:5432/dbname"),
                    Some(msg),
                );
            }
            return classify_db_test_connect_error(&msg);
        }
    };

    match tokio::time::timeout(policy.db_query, client.pool.ping()).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            return db_test_failed(
                "query",
                "DB_TEST_PING_FAILED",
                "Connection established, but ping failed.",
                Some("Check proxy restrictions or query permissions, then retry."),
                Some(e.to_string()),
            );
        }
        Err(_) => {
            return db_test_failed(
                "timeout",
                "DB_TEST_PING_TIMEOUT",
                "Connection established, but ping timed out.",
                Some("Check database load or network jitter, then retry."),
                None,
            );
        }
    }

    if !probe_capabilities {
        let resp = db_test_response(
            true,
            vec![],
            db_test_diagnostic(
                "success",
                "success",
                "DB_TEST_OK",
                "Connection successful.",
                None,
                None,
            ),
            "handshake",
            false,
            None,
            None,
        );
        client.pool.close().await;
        return resp;
    }

    let (version_sql, databases_sql) = match db_type {
        DbType::PostgreSQL => (
            "SELECT version()",
            Some("SELECT datname FROM pg_database WHERE datistemplate = false ORDER BY datname"),
        ),
        _ => ("SELECT sqlite_version()", None),
    };

    let server_version = match tokio::time::timeout(
        policy.db_query,
        client.pool.fetch_all_result(version_sql),
    )
    .await
    {
        Ok(Ok(result)) => result
            .rows
            .first()
            .and_then(|r| r.first())
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        _ => None,
    };

    let mut databases = Vec::new();
    if let Some(sql) = databases_sql {
        match tokio::time::timeout(policy.db_query, client.pool.fetch_all_result(sql)).await {
            Ok(Ok(result)) => {
                for row in &result.rows {
                    if let Some(name) = row.first().and_then(|v| v.as_str()) {
                        if !name.is_empty() {
                            databases.push(name.to_string());
                        }
                    }
                }
            }
            other => {
                let detail = match other {
                    Ok(Err(e)) => Some(e.to_string()),
                    _ => None,
                };
                let resp = db_test_response(
                    true,
                    vec![],
                    db_test_diagnostic(
                        "warning",
                        "query",
                        "DB_TEST_CAPABILITY_PROBE_FAILED",
                        "Connection successful, but failed to list databases.",
                        Some("Check metadata query restrictions or role privileges."),
                        detail,
                    ),
                    "handshake",
                    true,
                    Some(false),
                    server_version,
                );
                client.pool.close().await;
                return resp;
            }
        }
    }

    let resp = db_test_response(
        true,
        databases,
        db_test_diagnostic(
            "success",
            "success",
            "DB_TEST_OK",
            "Connection successful. Capability probe completed.",
            None,
            None,
        ),
        "capabilities",
        true,
        Some(true),
        server_version,
    );
    client.pool.close().await;
    resp
}


use core_lib::{
    ai::{
        agent::AgentError,
        policy_store::{Policy, PolicyStore},
    },
    config::AppConfig,
    crud::{CrudManager, CrudRequest},
    db::{connection_fingerprint, open_db_client, DbClient},
    knowledge_base::KnowledgeBase,
    navicat::{NavicatConnection, NavicatParser},
    offline_parser::OfflineParser,
    rule_engine::{Rule, RuleStore, RuleType},
    schema::{SchemaExtractor, SchemaResponse, TableWithDetails},
    sql_history::{SqlHistory, SqlHistoryStore},
    tools::{DdlEngine, MockDataGenerator, SyncEngine},
    timeout_policy::TimeoutPolicy,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, RwLock, Semaphore};
use tower_http::{
    cors::CorsLayer,
    services::{ServeDir, ServeFile},
};

const SCHEMA_CACHE_TTL: Duration = Duration::from_secs(30);
const TABLE_SCHEMA_CACHE_TTL: Duration = Duration::from_secs(300);
pub(crate) const DB_CLIENT_CACHE_TTL: Duration = Duration::from_secs(600);
pub(crate) const PERF_PROBE_MAX_ITERATIONS: u32 = 30;
pub(crate) const PERF_SUITE_ARCHIVE_DEFAULT_LIMIT: usize = 10;

pub(crate) async fn get_or_open_transaction_session(
    state: &AppState,
    db_id: Option<&str>,
    transaction_id: &str,
    create_if_not_found: bool,
) -> Result<SharedTransactionSession, AppError> {
    if let Some(existing) = state
        .transaction_sessions
        .read()
        .await
        .get(transaction_id)
        .cloned()
    {
        let expected_db_id = resolve_transaction_db_id(state, db_id).await;
        let mut session = existing.lock().await;
        if session.db_id != expected_db_id {
            return Err(AppError::BadRequest(
                "Transaction session is bound to a different database connection".to_string(),
            ));
        }
        session.last_accessed = std::time::Instant::now();
        drop(session);
        return Ok(existing);
    }

    if !create_if_not_found {
        return Err(AppError::NotFound("transaction session not found".to_string()));
    }

    let resolved_db_id = resolve_transaction_db_id(state, db_id).await;
    let (db_client, _) = resolve_db_client_for_request(state, db_id).await?;
    let mut conn = db_client
        .pool
        .acquire_engine()
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    let connection_id = conn
        .backend_id()
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    let begin_sql = conn.begin_statement();
    tokio::time::timeout(state.timeouts.db_query, conn.execute_command(begin_sql))
        .await
        .map_err(|_| AppError::Timeout("Starting transaction timed out".to_string()))?
        .map_err(|e| AppError::InternalError(e.to_string()))?;

    let session = Arc::new(Mutex::new(TransactionSession {
        connection_id,
        db_id: resolved_db_id,
        conn,
        last_accessed: std::time::Instant::now(),
    }));

    state
        .transaction_sessions
        .write()
        .await
        .insert(transaction_id.to_string(), session.clone());
    Ok(session)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();
    tracing::info!("Starting Local AI SQL Assistant Web Server...");

    let config = AppConfig::load().await.unwrap_or_default().normalize();
    let timeouts = TimeoutPolicy::default();
    let limits = RuntimeLimits::default();
    let job_semaphore = Arc::new(Semaphore::new(limits.max_job_concurrency));

    // Initialize DB Client if configured (including SSH tunnel / TLS)
    let mut db_client = None;
    if let Some(conn) = config.get_active_db_connection() {
        match open_db_client(&conn, &config.pool_config).await {
            Ok(client) => {
                tracing::info!("Connected to database");
                db_client = Some(client);
            }
            Err(e) => tracing::error!("Failed to connect to database: {}", e),
        }
    }

    let rule_store = RuleStore::load().await.unwrap_or_default();
    let policy = PolicyStore::load_effective()
        .await
        .unwrap_or_else(|_| Policy::default());
    let sql_history = SqlHistoryStore::load().await.unwrap_or_default();
    let knowledge_base = KnowledgeBase::load().await.unwrap_or_default();
    let ops = Arc::new(core_lib::ops::OpsRuntime::init_default());

    let state = AppState {
        config: Arc::new(RwLock::new(config)),
        db_client: Arc::new(RwLock::new(db_client)),
        db_client_cache: Arc::new(RwLock::new(HashMap::new())),
        virtual_schema: Arc::new(RwLock::new(None)),
        schema_cache: Arc::new(RwLock::new(HashMap::new())),
        table_schema_cache: Arc::new(RwLock::new(HashMap::new())),
        rule_store: Arc::new(RwLock::new(rule_store)),
        policy: Arc::new(RwLock::new(policy)),
        sql_history: Arc::new(RwLock::new(sql_history)),
        knowledge_base: Arc::new(RwLock::new(knowledge_base)),
        sync_jobs: Arc::new(RwLock::new(HashMap::new())),
        perf_sync_jobs: Arc::new(RwLock::new(HashMap::new())),
        active_queries: Arc::new(RwLock::new(HashMap::new())),
        transaction_sessions: Arc::new(RwLock::new(HashMap::new())),
        tool_jobs: Arc::new(RwLock::new(HashMap::new())),
        tool_job_handles: Arc::new(RwLock::new(HashMap::new())),
        timeouts,
        limits: limits.clone(),
        job_semaphore,
        export_semaphore: Arc::new(Semaphore::new(1)),
        ops,
    };

    // 进程内调度：跟随本进程运行，进程退出即停止。
    inspection_scheduler::spawn(state.clone());

    let api = Router::new()
        .route("/config", get(get_config).post(update_config))
        .route("/db/test", post(db_test))
        .route("/diagnostics/perf/probe", post(diagnostics_perf_probe))
        .route(
            "/diagnostics/perf/suites",
            get(diagnostics_perf_suite_list).post(diagnostics_perf_suite_save),
        )
        .route(
            "/diagnostics/perf/suites/baseline",
            get(diagnostics_perf_suite_baseline_get).post(diagnostics_perf_suite_baseline_pin),
        )
        .route(
            "/diagnostics/perf/suite-diffs",
            get(diagnostics_perf_suite_diff_list).post(diagnostics_perf_suite_diff_save),
        )
        .route(
            "/diagnostics/perf/suites/:suite_id",
            get(diagnostics_perf_suite_detail),
        )
        .route("/schema", get(get_schema))
        .route("/schema/parse", post(parse_schema))
        .route("/chat", post(ai_handlers::chat_to_sql))
        .route("/chat/stream", post(ai_handlers::chat_to_sql_stream))
        .route("/execute", post(execute_sql))
        .route("/execute/transaction", post(execute_transaction))
        .route("/execute/cancel", post(execute_cancel))
        .route("/policy", get(get_policy))
        .route("/policy/reset", post(reset_policy))
        .route("/policy/snapshot", post(snapshot_policy))
        .route("/policy/rollback", post(rollback_policy))
        .route("/crud/insert", post(crud_insert))
        .route("/crud/update", post(crud_update))
        .route("/crud/delete", post(crud_delete))
        .route("/navicat/parse", post(parse_navicat))
        .route("/rules", get(get_rules))
        .route("/rules/save", post(save_rule))
        .route("/rules/delete", post(delete_rule))
        .route("/table/data", get(get_table_data))
        .route("/table/schema", get(get_table_schema))
        .route("/table/ddl/preview", post(preview_ddl))
        .route("/table/ddl", post(execute_ddl))
        .route("/tools/mock-data", post(generate_mock_data))
        .route("/tools/export", post(export_data))
        .route("/tools/import", post(import_data))
        .route("/tools/jobs/export/start", post(export_job_start))
        .route("/tools/jobs/import/start", post(import_job_start))
        .route("/tools/jobs/import-sql/start", post(import_sql_job_start))
        .route("/tools/jobs/go-live/start", post(go_live_job_start))
        .route("/tools/go-live/reports", get(go_live_reports_list))
        .route("/tools/go-live/audit", get(go_live_audit_list))
        .route(
            "/ops/session-token",
            get(ops_handlers::ops_session_token),
        )
        .route("/ops/scenarios", get(ops_handlers::ops_scenarios))
        .route(
            "/ops/policy",
            get(ops_handlers::ops_get_policy).post(ops_handlers::ops_update_policy),
        )
        .route("/ops/analyze", post(ops_handlers::ops_analyze))
        .route(
            "/ops/field-governance/diff-page",
            post(ops_handlers::ops_field_diff_page),
        )
        .route(
            "/ops/field-governance/backfill",
            get(ops_handlers::ops_field_backfill_list)
                .post(ops_handlers::ops_field_backfill_start),
        )
        .route(
            "/ops/field-governance/backfill/approve",
            post(ops_handlers::ops_field_backfill_approve),
        )
        .route(
            "/ops/field-governance/backfill/:job_id",
            get(ops_handlers::ops_field_backfill_status),
        )
        .route(
            "/ops/field-governance/backfill/:job_id/pause",
            post(ops_handlers::ops_field_backfill_pause),
        )
        .route(
            "/ops/field-governance/backfill/:job_id/resume",
            post(ops_handlers::ops_field_backfill_resume),
        )
        .route(
            "/ops/field-governance/backfill/:job_id/abort",
            post(ops_handlers::ops_field_backfill_abort),
        )
        .route(
            "/ops/slow-queries",
            get(ops_handlers::ops_slow_queries),
        )
        .route(
            "/ops/analyze/stream",
            post(ops_handlers::ops_analyze_stream),
        )
        .route("/ops/plans", get(ops_handlers::ops_list_plans))
        .route("/ops/plans/:plan_id", get(ops_handlers::ops_get_plan))
        .route(
            "/ops/plans/:plan_id/approve",
            post(ops_handlers::ops_approve),
        )
        .route(
            "/ops/plans/:plan_id/execute",
            post(ops_handlers::ops_execute),
        )
        .route(
            "/ops/plans/:plan_id/rollback",
            post(ops_handlers::ops_rollback),
        )
        .route("/ops/audit", get(ops_handlers::ops_audit))
        .route("/ops/approvals", get(ops_handlers::ops_approvals))
        .route(
            "/ops/inspection/presets",
            get(inspection_handlers::ops_inspection_presets),
        )
        .route(
            "/ops/inspection/schedule",
            get(inspection_handlers::ops_inspection_get_schedule)
                .post(inspection_handlers::ops_inspection_save_schedule),
        )
        .route(
            "/ops/inspection/run",
            post(inspection_handlers::ops_inspection_run),
        )
        .route(
            "/ops/inspection/latest",
            get(inspection_handlers::ops_inspection_latest),
        )
        .route(
            "/ops/inspection/history",
            get(inspection_handlers::ops_inspection_history),
        )
        .route(
            "/ops/inspection/alerts",
            get(inspection_handlers::ops_inspection_alerts),
        )
        .route(
            "/ops/inspection/alerts/:alert_id/ack",
            post(inspection_handlers::ops_inspection_ack_alert),
        )
        .route("/tools/jobs/:job_id", get(tool_job_status))
        .route("/tools/jobs/:job_id/cancel", post(tool_job_cancel))
        .route(
            "/tools/jobs/:job_id/artifacts/:artifact",
            get(tool_job_artifact_download),
        )
        .route("/tools/schema-sync/diff", post(sync_schema_diff))
        .route("/tools/schema-sync/ddl", post(sync_schema_ddl))
        .route("/tools/data-sync/diff", post(sync_data_diff))
        .route("/tools/data-sync/dml", post(sync_data_dml))
        .route("/tools/data-sync/compare", post(mysql_sync_compare))
        .route("/tools/data-sync/preview", post(mysql_sync_preview))
        .route("/tools/data-sync/deploy", post(mysql_sync_deploy))
        .route("/tools/data-sync/jobs/:job_id", get(mysql_sync_job_status))
        .route("/tools/perf-sync/start", post(perf_sync_start))
        .route("/tools/perf-sync/check", post(perf_sync_check))
        .route("/tools/perf-sync/jobs/:job_id", get(perf_sync_job_status))
        .route("/tools/data-transfer/upload", post(transfer_upload))
        .route("/tools/data-transfer/execute", post(transfer_execute))
        .route("/sql/history", get(get_history).post(clear_history))
        .route("/sql/explain", post(explain_sql))
        .route("/sql/session-info", get(session_info))
        .route("/api/ai/models", get(ai_handlers::ai_models))
        .route(
            "/api/ai/provider/models",
            post(ai_handlers::fetch_provider_models),
        )
        .route("/api/ai/health", get(ai_handlers::ai_health))
        .route("/api/ai/query", post(ai_handlers::ai_query))
        .route(
            "/api/ai/explain_error",
            post(ai_handlers::ai_explain_error),
        )
        .route("/api/ai/knowledge", get(ai_handlers::get_knowledge))
        .route("/api/ai/knowledge", post(ai_handlers::add_knowledge))
        .route(
            "/api/ai/knowledge",
            axum::routing::put(ai_handlers::update_knowledge),
        )
        .route(
            "/api/ai/knowledge/delete",
            post(ai_handlers::delete_knowledge),
        )
        .layer(axum::extract::DefaultBodyLimit::max(
            (limits.max_file_bytes.min(usize::MAX as u64)) as usize,
        ))
        .layer(axum::middleware::from_fn(set_request_locale));

    let dist_dir = std::env::var("WEB_UI_DIST_DIR").unwrap_or_else(|_| "web-ui/dist".to_string());
    let index_path = std::path::Path::new(&dist_dir).join("index.html");
    let static_service = ServeDir::new(dist_dir).not_found_service(ServeFile::new(index_path));

    let mut cors_origins: Vec<axum::http::HeaderValue> = [
        "http://localhost",
        "http://127.0.0.1",
        "http://localhost:5173",
        "http://localhost:3000",
        "http://127.0.0.1:5173",
        "http://127.0.0.1:3000",
        "tauri://localhost",
        "http://tauri.localhost",
        "https://tauri.localhost",
    ]
    .iter()
    .map(|s| s.parse::<axum::http::HeaderValue>().unwrap())
    .collect();
    if let Ok(extra) = std::env::var("CORS_EXTRA_ORIGINS") {
        for origin in extra.split(',') {
            let trimmed = origin.trim();
            if !trimmed.is_empty() {
                if let Ok(val) = trimmed.parse::<axum::http::HeaderValue>() {
                    cors_origins.push(val);
                }
            }
        }
    }

    let app = Router::new()
        .nest("/backend", api)
        .fallback_service(static_service)
        .layer(
            CorsLayer::new()
                .allow_origin(cors_origins)
                .allow_methods([
                    axum::http::Method::GET,
                    axum::http::Method::POST,
                    axum::http::Method::PUT,
                    axum::http::Method::DELETE,
                    axum::http::Method::OPTIONS,
                ])
                .allow_headers([
                    axum::http::header::CONTENT_TYPE,
                    axum::http::header::ACCEPT,
                    axum::http::header::ACCEPT_LANGUAGE,
                    axum::http::header::AUTHORIZATION,
                    axum::http::HeaderName::from_static("x-locale"),
                    axum::http::HeaderName::from_static("x-silent-error"),
                ]),
        )
        .with_state(state.clone());

    let state_for_cleanup = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            interval.tick().await;
            // Use a single write lock for both check and removal to avoid TOCTOU race
            let mut sessions = state_for_cleanup.transaction_sessions.write().await;
            let expired: Vec<String> = sessions
                .iter()
                .filter_map(|(id, session_arc)| {
                    // We can't await inside filter_map, but since this runs every 60s
                    // and sessions are short-lived, we check via try_lock
                    if let Ok(session) = session_arc.try_lock() {
                        if session.last_accessed.elapsed() > std::time::Duration::from_secs(600) {
                            return Some(id.clone());
                        }
                    }
                    None
                })
                .collect();

            for id in expired {
                if let Some(session_arc) = sessions.remove(&id) {
                    let mut session = session_arc.lock().await;
                    let _ = session.conn.execute_command("ROLLBACK").await;
                    tracing::info!("Cleaned up idle transaction session: {}", id);
                }
            }
        }
    });

    let bind_addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:3000".to_string());
    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    let actual_addr = listener.local_addr()?;
    tracing::info!("Server listening on http://{}", actual_addr);

    // Desktop sidecar mode: announce the resolved address so the Tauri shell can
    // wait for readiness instead of guessing/polling a fixed port. Using port 0
    // above lets the OS pick a free port, avoiding collisions between instances.
    if std::env::var("SIDECAR_READY_STDOUT").is_ok() {
        println!("SIDECAR_READY http://{}", actual_addr);
    }

    // Desktop sidecar mode: the Tauri shell owns our stdin. If the shell exits
    // (including crash / force quit) the pipe closes and we see EOF, which lets
    // the sidecar shut down and release DB connections instead of lingering.
    if std::env::var("EXIT_ON_STDIN_EOF").is_ok() {
        use tokio::io::AsyncReadExt;
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let mut stdin = tokio::io::stdin();
            let mut buf = [0u8; 64];
            loop {
                match stdin.read(&mut buf).await {
                    Ok(0) | Err(_) => {
                        tracing::info!("stdin closed, shutting down sidecar");
                        let _ = shutdown_tx.send(());
                        break;
                    }
                    Ok(_) => {}
                }
            }
        });
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await?;
    } else {
        axum::serve(listener, app).await?;
    }

    Ok(())
}

fn map_agent_error(e: AgentError) -> AppError {
    match e {
        AgentError::MissingApiKey => AppError::AiAuth("Missing API key. Please configure your AI token.".to_string()),
        AgentError::NoTokens => AppError::BadRequest("No tokens available in pool".to_string()),
        AgentError::Auth(msg) => {
            let body = serde_json::json!({
                "error": "ai_auth_failed",
                "message": "AI 鉴权失败，请在引导页里更新 AI Token / Relay 配置后重试。",
                "detail": msg,
            }).to_string();
            AppError::AiAuth(body)
        }
        AgentError::Forbidden(msg) => AppError::AiForbidden(msg),
        AgentError::ModelNotFound(msg) => AppError::AiModelNotFound(msg),
        AgentError::RateLimited(msg) => AppError::AiRateLimited(msg),
        AgentError::ServerError(msg) => AppError::ExternalServiceUnavailable(msg),
        AgentError::Network(msg) => {
            let lower = msg.to_lowercase();
            if lower.contains("timeout") {
                AppError::AiAgentTimeout(msg)
            } else if lower.contains("proxy") || lower.contains("tunnel") {
                AppError::AiProxy(msg)
            } else if lower.contains("connection") || lower.contains("connect") {
                AppError::ExternalServiceUnavailable(msg)
            } else {
                AppError::AiAgentTimeout(msg)
            }
        }
        AgentError::Agent(msg) => AppError::InternalError(msg),
    }
}

// ----------------- CRUD API Handlers -----------------

#[derive(Deserialize)]
pub(crate) struct CrudMutationRequest {
    table_name: String,
    data: serde_json::Value,
    condition: Option<serde_json::Map<String, serde_json::Value>>,
    db_id: Option<String>,
    transaction_id: Option<String>,
}

async fn crud_insert(
    State(state): State<AppState>,
    Json(req): Json<CrudMutationRequest>,
) -> Result<Json<ExecuteResponse>, AppError> {
    let is_read_only = is_read_only_connection(&state, req.db_id.as_deref()).await;
    if is_read_only {
        return Err(AppError::Forbidden(
            "当前连接为只读模式，禁止执行非查询操作！".to_string(),
        ));
    }

    let (db_client, _) = resolve_db_client_for_request(&state, req.db_id.as_deref()).await?;

    let transaction_id = req
        .transaction_id
        .as_deref()
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_string);
    let transaction_session = if let Some(id) = transaction_id.as_deref() {
        Some(get_or_open_transaction_session(&state, req.db_id.as_deref(), id, false).await?)
    } else {
        None
    };

    let crud_req = CrudRequest {
        table_name: req.table_name,
        data: req.data,
        condition: req.condition,
    };

    let (affected_rows, transaction_state) = if let Some(session) = transaction_session {
        let mut guard = session.lock().await;
        guard.last_accessed = std::time::Instant::now();
        let affected = CrudManager::insert_engine(&mut guard.conn, &crud_req)
            .await
            .map_err(|e| AppError::InternalError(e.to_string()))?;
        (affected, Some("active".to_string()))
    } else {
        let affected = CrudManager::insert(&db_client.pool, &crud_req)
            .await
            .map_err(|e| AppError::InternalError(e.to_string()))?;
        (affected, None)
    };

    Ok(Json(ExecuteResponse {
        columns: vec![],
        row_count: 0,
        rows: vec![],
        affected_rows,
        execution_time_ms: 0,
        has_more: false,
        next_offset: None,
        chunk_offset: 0,
        chunk_size: None,
        preview_cap: None,
        truncated: false,
        transaction_state,
    }))
}

async fn crud_update(
    State(state): State<AppState>,
    Json(req): Json<CrudMutationRequest>,
) -> Result<Json<ExecuteResponse>, AppError> {
    let is_read_only = is_read_only_connection(&state, req.db_id.as_deref()).await;
    if is_read_only {
        return Err(AppError::Forbidden(
            "当前连接为只读模式，禁止执行非查询操作！".to_string(),
        ));
    }

    let (db_client, _) = resolve_db_client_for_request(&state, req.db_id.as_deref()).await?;

    let transaction_id = req
        .transaction_id
        .as_deref()
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_string);
    let transaction_session = if let Some(id) = transaction_id.as_deref() {
        Some(get_or_open_transaction_session(&state, req.db_id.as_deref(), id, false).await?)
    } else {
        None
    };

    let crud_req = CrudRequest {
        table_name: req.table_name,
        data: req.data,
        condition: req.condition,
    };

    let (affected_rows, transaction_state) = if let Some(session) = transaction_session {
        let mut guard = session.lock().await;
        guard.last_accessed = std::time::Instant::now();
        let affected = CrudManager::update_engine(&mut guard.conn, &crud_req)
            .await
            .map_err(|e| AppError::InternalError(e.to_string()))?;
        (affected, Some("active".to_string()))
    } else {
        let affected = CrudManager::update(&db_client.pool, &crud_req)
            .await
            .map_err(|e| AppError::InternalError(e.to_string()))?;
        (affected, None)
    };

    Ok(Json(ExecuteResponse {
        columns: vec![],
        row_count: 0,
        rows: vec![],
        affected_rows,
        execution_time_ms: 0,
        has_more: false,
        next_offset: None,
        chunk_offset: 0,
        chunk_size: None,
        preview_cap: None,
        truncated: false,
        transaction_state,
    }))
}

#[derive(Deserialize)]
pub(crate) struct DeleteRequest {
    table_name: String,
    condition: serde_json::Map<String, serde_json::Value>,
    db_id: Option<String>,
    transaction_id: Option<String>,
}

async fn crud_delete(
    State(state): State<AppState>,
    Json(req): Json<DeleteRequest>,
) -> Result<Json<ExecuteResponse>, AppError> {
    let is_read_only = is_read_only_connection(&state, req.db_id.as_deref()).await;
    if is_read_only {
        return Err(AppError::Forbidden(
            "当前连接为只读模式，禁止执行非查询操作！".to_string(),
        ));
    }

    let (db_client, _) = resolve_db_client_for_request(&state, req.db_id.as_deref()).await?;

    let transaction_id = req
        .transaction_id
        .as_deref()
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_string);
    let transaction_session = if let Some(id) = transaction_id.as_deref() {
        Some(get_or_open_transaction_session(&state, req.db_id.as_deref(), id, false).await?)
    } else {
        None
    };

    let (affected_rows, transaction_state) = if let Some(session) = transaction_session {
        let mut guard = session.lock().await;
        guard.last_accessed = std::time::Instant::now();
        let affected = CrudManager::delete_engine(&mut guard.conn, &req.table_name, &req.condition)
            .await
            .map_err(|e| AppError::InternalError(e.to_string()))?;
        (affected, Some("active".to_string()))
    } else {
        let affected = CrudManager::delete(&db_client.pool, &req.table_name, &req.condition)
            .await
            .map_err(|e| AppError::InternalError(e.to_string()))?;
        (affected, None)
    };

    Ok(Json(ExecuteResponse {
        columns: vec![],
        row_count: 0,
        rows: vec![],
        affected_rows,
        execution_time_ms: 0,
        has_more: false,
        next_offset: None,
        chunk_offset: 0,
        chunk_size: None,
        preview_cap: None,
        truncated: false,
        transaction_state,
    }))
}

fn config_for_client(raw: &AppConfig) -> serde_json::Value {
    let api_key_set = raw.api_key.as_ref().is_some_and(|s| !s.is_empty());
    let token_pool_set = !raw.token_pool.is_empty();
    let mut profile_flags: HashMap<String, (bool, bool)> = HashMap::new();
    for p in &raw.ai_profiles {
        let p_api_key_set = p.api_key.as_ref().is_some_and(|s| !s.is_empty());
        let p_token_pool_set = !p.pool.tokens.is_empty();
        profile_flags.insert(p.id.clone(), (p_api_key_set, p_token_pool_set));
    }

    let redacted = raw.redacted_for_client();
    let mut v = serde_json::to_value(redacted).unwrap_or_else(|_| serde_json::json!({}));
    if let Some(obj) = v.as_object_mut() {
        obj.insert(
            "api_key_set".to_string(),
            serde_json::Value::Bool(api_key_set),
        );
        obj.insert(
            "token_pool_set".to_string(),
            serde_json::Value::Bool(token_pool_set),
        );
        if let Some(arr) = obj.get_mut("ai_profiles").and_then(|x| x.as_array_mut()) {
            for item in arr {
                if let Some(pobj) = item.as_object_mut() {
                    let id = pobj
                        .get("id")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    if let Some((k, t)) = profile_flags.get(&id).copied() {
                        pobj.insert("api_key_set".to_string(), serde_json::Value::Bool(k));
                        pobj.insert("token_pool_set".to_string(), serde_json::Value::Bool(t));
                    }
                }
            }
        }
    }
    v
}

async fn get_config(State(state): State<AppState>) -> Json<serde_json::Value> {
    let config = state.config.read().await.clone();
    Json(config_for_client(&config))
}

/// Resolves the connection referenced by an explicit `active_db_id`.
fn active_connection_for_update(config: &AppConfig) -> Option<core_lib::config::DbConnection> {
    let active_id = config.active_db_id.as_ref()?;
    config
        .db_connections
        .iter()
        .find(|c| &c.id == active_id)
        .cloned()
}

fn active_connection_fingerprint(config: &AppConfig) -> Option<String> {
    active_connection_for_update(config).map(|c| connection_fingerprint(&c))
}

/// Whether a config save must rebuild the active pool.
///
/// - Explicit connection selections: only a target/transport change reconnects.
/// - Pure legacy configs (no `db_connections`, only `db_url`): a `db_url` change
///   reconnects.
/// - Disconnecting / deleting the last connection (`active_db_id` cleared) is a
///   change and must close the old pool instead of falling back to `db_url`.
fn save_requires_reconnect(prev: &AppConfig, new: &AppConfig) -> bool {
    let prev_had_active = prev.active_db_id.is_some();
    let new_has_active = new.active_db_id.is_some();
    if prev_had_active || new_has_active {
        active_connection_fingerprint(prev) != active_connection_fingerprint(new)
    } else {
        prev.db_url != new.db_url
    }
}

/// The connection to dial for a save that requires a reconnect.
fn connection_to_dial_for_save(prev: &AppConfig, new: &AppConfig) -> Option<core_lib::config::DbConnection> {
    if new.active_db_id.is_some() {
        return active_connection_for_update(new);
    }
    // Only pure legacy configs may fall back to db_url. When the previous config
    // had an active connection, a cleared active_db_id means "disconnect".
    if prev.active_db_id.is_none() {
        return new.get_active_db_connection();
    }
    None
}

async fn update_config(
    State(state): State<AppState>,
    Json(new_config): Json<AppConfig>,
) -> Result<Json<serde_json::Value>, AppError> {
    let prev_config = state.config.read().await.clone();
    let mut new_config = new_config.normalize();
    new_config.merge_secrets_from(&prev_config);

    // Only reconnect when the active connection's target or transport changed.
    let connection_changed = save_requires_reconnect(&prev_config, &new_config);

    // Build the replacement pool BEFORE mutating persisted or in-memory state.
    // If dialing the new database fails, the old config and pool stay untouched,
    // so the UI can never display the new database while still talking to the old.
    let new_client = if connection_changed {
        match connection_to_dial_for_save(&prev_config, &new_config) {
            Some(conn) => Some(
                open_db_client(&conn, &new_config.pool_config)
                    .await
                    .map_err(|e| AppError::BadRequest(format!("DB connection failed: {}", e)))?,
            ),
            None => None,
        }
    } else {
        None
    };

    // Persist only after the new connection (if any) succeeded.
    new_config
        .save()
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;

    // Swap the pool atomically when reconnecting (replace keeps Some(...) at all
    // times, so there is no "db_client is None" gap), then close the old pool.
    if connection_changed {
        let old_client = {
            let mut db_write = state.db_client.write().await;
            std::mem::replace(&mut *db_write, new_client)
        };
        if let Some(old) = old_client {
            old.pool.close().await;
        }
    }

    // Update in-memory config after the pool is ready.
    {
        let mut config_write = state.config.write().await;
        *config_write = new_config.clone();
    }
    state.db_client_cache.write().await.clear();
    clear_metadata_caches(&state).await;

    // AI config is now read fresh from AppState.config on each request.

    Ok(Json(config_for_client(&new_config)))
}

#[cfg(test)]
mod config_redaction_tests {
    use super::*;
    use axum::{routing::get, Router};
    use core_lib::config::{AiConnectionMode, AiProfile, AiProvider};
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn get_config_does_not_leak_secrets() {
        let cfg = AppConfig {
            api_key: Some("secret-key".to_string()),
            token_pool: vec!["secret-token".to_string()],
            db_url: Some("mysql://u:secret-pass@127.0.0.1:3306/db".to_string()),
            ai_profiles: vec![AiProfile {
                id: "p".to_string(),
                name: "p".to_string(),
                provider: AiProvider::Openai,
                mode: AiConnectionMode::Direct,
                api_key: Some("secret-profile-key".to_string()),
                relay_url: None,
                pool: core_lib::config::AiPoolConfig {
                    tokens: vec!["secret-profile-token".to_string()],
                    ..core_lib::config::AiPoolConfig::default()
                },
            }],
            active_ai_profile_id: Some("p".to_string()),
            ..AppConfig::default()
        };

        let state = test_state_with_config(cfg);
        let app = Router::new()
            .route("/backend/config", get(get_config))
            .with_state(state);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let base = format!("http://{}", addr);
        let body: serde_json::Value = reqwest::get(format!("{}/backend/config", base))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        assert!(body.get("api_key").unwrap().is_null());
        assert_eq!(
            body.get("db_url").and_then(|v| v.as_str()).unwrap(),
            "mysql://u:******@127.0.0.1:3306/db"
        );
        assert_eq!(
            body.get("token_pool")
                .and_then(|v| v.as_array())
                .unwrap()
                .len(),
            0
        );
        let profiles = body.get("ai_profiles").and_then(|v| v.as_array()).unwrap();
        assert!(profiles[0].get("api_key").unwrap().is_null());
        assert_eq!(
            profiles[0]
                .get("pool")
                .and_then(|p| p.get("tokens"))
                .and_then(|t| t.as_array())
                .unwrap()
                .len(),
            0
        );
        let as_str = body.to_string();
        assert!(!as_str.contains("secret-key"));
        assert!(!as_str.contains("secret-token"));
        assert!(!as_str.contains("secret-pass"));
        assert!(!as_str.contains("secret-profile-key"));
        assert!(!as_str.contains("secret-profile-token"));
    }
}

fn schema_cache_key(db_id: Option<&str>, db_name: &str) -> String {
    match db_id {
        Some(id) => format!("{}::{}", id, db_name),
        None => format!("active::{}", db_name),
    }
}

fn table_schema_cache_key(db_id: Option<&str>, db_name: &str, table_name: &str) -> String {
    format!("{}::{}", schema_cache_key(db_id, db_name), table_name)
}

/// 把 DbError 转成 AppError：`Unsupported`（如 SQLite 结构内省未支持）会变成
/// 400 BadRequest，而不再是 500 Internal，避免把"功能未支持"误报为服务端故障。
fn db_error_to_app(e: core_lib::db::DbError) -> AppError {
    core_lib::service::error::ServiceError::from(e).to_app_error()
}

pub(crate) async fn get_cached_table_schema(
    state: &AppState,
    db_id: Option<&str>,
    db_client: &DbClient,
    db_name: &str,
    table_name: &str,
) -> Result<TableWithDetails, AppError> {
    let key = table_schema_cache_key(db_id, db_name, table_name);
    // Fast path: read lock allows concurrent cache hits
    {
        let cache = state.table_schema_cache.read().await;
        if let Some(entry) = cache.get(&key) {
            if entry.expires_at > Instant::now() {
                return Ok(entry.table.clone());
            }
        }
    }

    // Acquire write lock, then double-check (another thread may have refreshed)
    let mut cache = state.table_schema_cache.write().await;
    if let Some(entry) = cache.get(&key) {
        if entry.expires_at > Instant::now() {
            return Ok(entry.table.clone());
        }
    }

    let columns = SchemaExtractor::get_columns(db_client, db_name, table_name)
        .await
        .map_err(db_error_to_app)?;
    let indexes = SchemaExtractor::get_indexes(db_client, db_name, table_name)
        .await
        .map_err(db_error_to_app)?;
    let foreign_keys = SchemaExtractor::get_foreign_keys(db_client, db_name, table_name)
        .await
        .map_err(db_error_to_app)?;
    let table = TableWithDetails {
        table_name: table_name.to_string(),
        columns,
        indexes,
        foreign_keys,
    };

    let now = Instant::now();
    cache.retain(|_, v| v.expires_at > now);
    cache.insert(
        key,
        CachedTableSchemaEntry {
            table: table.clone(),
            expires_at: now + TABLE_SCHEMA_CACHE_TTL,
        },
    );
    Ok(table)
}

pub(crate) async fn clear_metadata_caches(state: &AppState) {
    state.schema_cache.write().await.clear();
    state.table_schema_cache.write().await.clear();
}

async fn get_schema_internal(state: &AppState) -> Option<SchemaResponse> {
    let ctx = crate::bridge::bridge_service_context(state);
    core_lib::service::schema::SchemaService::get_schema(&ctx, None)
        .await
        .ok()
}

async fn get_schema_for_db_id(state: &AppState, db_id: &str) -> Result<SchemaResponse, AppError> {
    let ctx = crate::bridge::bridge_service_context(state);
    core_lib::service::schema::SchemaService::get_schema(&ctx, Some(db_id))
        .await
        .map_err(|e| e.to_app_error())
}

pub(crate) async fn resolve_db_client_for_request(
    state: &AppState,
    db_id: Option<&str>,
) -> Result<(DbClient, String), AppError> {
    if let Some(id) = db_id {
        return get_temp_db_client(state, id).await;
    }
    let db_client = state
        .db_client
        .read()
        .await
        .clone()
        .ok_or_else(|| AppError::BadRequest("Database not connected".to_string()))?;
    let url = state
        .config
        .read()
        .await
        .get_active_db_url()
        .unwrap_or_default();
    let db_name = DbClient::extract_db_name(&url).unwrap_or_default();
    Ok((db_client, db_name))
}

pub(crate) async fn is_read_only_connection(state: &AppState, db_id: Option<&str>) -> bool {
    let config = state.config.read().await;
    if let Some(id) = db_id {
        return config
            .db_connections
            .iter()
            .find(|c| c.id == id)
            .map(|c| c.is_read_only)
            .unwrap_or(false);
    }
    if let Some(active_id) = &config.active_db_id {
        return config
            .db_connections
            .iter()
            .find(|c| &c.id == active_id)
            .map(|c| c.is_read_only)
            .unwrap_or(false);
    }
    false
}

#[derive(Deserialize)]
struct DbContextQuery {
    db_id: Option<String>,
}

async fn get_schema(
    State(state): State<AppState>,
    axum::extract::Query(query): axum::extract::Query<DbContextQuery>,
) -> Result<Json<SchemaResponse>, AppError> {
    if let Some(db_id) = query.db_id.as_deref() {
        return Ok(Json(get_schema_for_db_id(&state, db_id).await?));
    }
    if let Some(schema) = get_schema_internal(&state).await {
        Ok(Json(schema))
    } else {
        Err(AppError::BadRequest(
            "Database not connected and no virtual schema loaded".to_string(),
        ))
    }
}

#[derive(Deserialize)]
struct ParseSchemaRequest {
    sql_content: String,
}

async fn parse_schema(
    State(state): State<AppState>,
    Json(req): Json<ParseSchemaRequest>,
) -> Result<Json<SchemaResponse>, AppError> {
    let schema = OfflineParser::parse_sql(&req.sql_content).map_err(AppError::BadRequest)?;

    let mut virtual_schema_write = state.virtual_schema.write().await;
    *virtual_schema_write = Some(schema.clone());

    Ok(Json(schema))
}

async fn get_policy(State(state): State<AppState>) -> Result<Json<Policy>, AppError> {
    let policy = state.policy.read().await;
    Ok(Json(policy.clone()))
}

async fn reset_policy(State(state): State<AppState>) -> Result<StatusCode, AppError> {
    PolicyStore::reset_override()
        .await
        .map_err(|e| AppError::InternalError(format!("{:?}", e)))?;
    let effective = PolicyStore::load_effective()
        .await
        .unwrap_or_else(|_| Policy::default());
    let mut policy = state.policy.write().await;
    *policy = effective;
    Ok(StatusCode::OK)
}

#[derive(Serialize)]
struct SnapshotPolicyResponse {
    name: String,
}

async fn snapshot_policy(
    State(state): State<AppState>,
) -> Result<Json<SnapshotPolicyResponse>, AppError> {
    let name = PolicyStore::create_snapshot()
        .await
        .map_err(|e| AppError::InternalError(format!("{:?}", e)))?;
    let effective = PolicyStore::load_effective()
        .await
        .unwrap_or_else(|_| Policy::default());
    let mut policy = state.policy.write().await;
    *policy = effective;
    Ok(Json(SnapshotPolicyResponse { name }))
}

#[derive(Deserialize)]
struct RollbackPolicyRequest {
    name: String,
}

async fn rollback_policy(
    State(state): State<AppState>,
    Json(req): Json<RollbackPolicyRequest>,
) -> Result<StatusCode, AppError> {
    PolicyStore::rollback_snapshot(&req.name)
        .await
        .map_err(|e| AppError::BadRequest(format!("{:?}", e)))?;
    let effective = PolicyStore::load_effective()
        .await
        .unwrap_or_else(|_| Policy::default());
    let mut policy = state.policy.write().await;
    *policy = effective;
    Ok(StatusCode::OK)
}

pub(crate) const QUERY_PREVIEW_CHUNK_SIZE: u32 = 200;
pub(crate) const QUERY_PREVIEW_ROW_CAP: u32 = 1000;

#[derive(Deserialize)]
pub(crate) struct ExecuteRequest {
    sql: String,
    force: Option<bool>,
    db_id: Option<String>,
    chunk_offset: Option<u32>,
    chunk_size: Option<u32>,
    cancel_token: Option<String>,
    transaction_id: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct ExecuteResponse {
    columns: Vec<String>,
    rows: Vec<serde_json::Value>,
    row_count: usize,
    affected_rows: u64,
    execution_time_ms: u64,
    has_more: bool,
    next_offset: Option<u32>,
    chunk_offset: u32,
    chunk_size: Option<u32>,
    preview_cap: Option<u32>,
    truncated: bool,
    transaction_state: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct ExecuteCancelRequest {
    cancel_token: String,
}

#[derive(Serialize)]
struct ExecuteCancelResponse {
    canceled: bool,
}

#[derive(Deserialize)]
pub(crate) struct ExecuteTransactionRequest {
    action: String,
    transaction_id: String,
    db_id: Option<String>,
}

#[derive(Serialize)]
struct ExecuteTransactionResponse {
    action: String,
    transaction_id: String,
    state: String,
    execution_time_ms: u64,
}

async fn execute_cancel(
    State(state): State<AppState>,
    Json(req): Json<ExecuteCancelRequest>,
) -> Result<Json<ExecuteCancelResponse>, AppError> {
    let cancel_token = req.cancel_token.trim();
    if cancel_token.is_empty() {
        return Ok(Json(ExecuteCancelResponse { canceled: false }));
    }

    let canceled = cancel_active_query(&state, cancel_token).await?;
    Ok(Json(ExecuteCancelResponse { canceled }))
}

async fn execute_transaction(
    State(state): State<AppState>,
    Json(req): Json<ExecuteTransactionRequest>,
) -> Result<Json<ExecuteTransactionResponse>, AppError> {
    let transaction_id = req.transaction_id.trim();
    if transaction_id.is_empty() {
        return Err(AppError::BadRequest(
            "transaction_id is required".to_string(),
        ));
    }

    let action = req.action.trim().to_lowercase();
    if action == "begin" {
        // Explicitly start a new transaction session
        let _ = get_or_open_transaction_session(&state, req.db_id.as_deref(), transaction_id, true).await?;
        return Ok(Json(ExecuteTransactionResponse {
            action: "begin".to_string(),
            transaction_id: transaction_id.to_string(),
            state: "active".to_string(),
            execution_time_ms: 0,
        }));
    }

    if action != "commit" && action != "rollback" {
        return Err(AppError::BadRequest(
            "transaction action must be begin, commit or rollback".to_string(),
        ));
    }

    let session = state
        .transaction_sessions
        .read()
        .await
        .get(transaction_id)
        .cloned()
        .ok_or_else(|| AppError::NotFound("transaction session not found".to_string()))?;
    let expected_db_id = resolve_transaction_db_id(&state, req.db_id.as_deref()).await;
    {
        let guard = session.lock().await;
        if guard.db_id != expected_db_id {
            return Err(AppError::BadRequest(
                "Transaction session is bound to a different database connection".to_string(),
            ));
        }
    }

    let started_at = Instant::now();
    {
        let mut guard = session.lock().await;
        let sql = if action == "commit" {
            "COMMIT"
        } else {
            "ROLLBACK"
        };
        tokio::time::timeout(state.timeouts.db_query, guard.conn.execute_command(sql))
            .await
            .map_err(|_| AppError::Timeout(format!("{action} timed out")))?
            .map_err(db_error_to_app)?;
    }

    state
        .transaction_sessions
        .write()
        .await
        .remove(transaction_id);
    clear_metadata_caches(&state).await;

    Ok(Json(ExecuteTransactionResponse {
        action,
        transaction_id: transaction_id.to_string(),
        state: "idle".to_string(),
        execution_time_ms: started_at.elapsed().as_millis() as u64,
    }))
}

#[derive(Deserialize)]
struct GetTableDataRequest {
    table_name: String,
    page: Option<u32>,
    page_size: Option<u32>,
    filters: Option<String>,
    orders: Option<String>,
    db_id: Option<String>,
}

#[derive(Serialize)]
struct GetTableDataResponse {
    data: Vec<serde_json::Value>,
    total: Option<i64>,
    total_status: String,
    has_more: bool,
}

#[derive(Deserialize, Debug)]
struct FilterCondition {
    column: String,
    operator: String,
    value: String,
}

#[derive(Deserialize, Debug)]
struct OrderCondition {
    column: String,
    desc: bool,
}

async fn get_table_data(
    State(state): State<AppState>,
    axum::extract::Query(req): axum::extract::Query<GetTableDataRequest>,
) -> Result<Json<GetTableDataResponse>, AppError> {
    use core_lib::service::schema::{GetTableDataParams, SchemaService};

    let ctx = crate::bridge::bridge_service_context(&state);
    let result = SchemaService::get_table_data(
        &ctx,
        GetTableDataParams {
            db_id: req.db_id,
            table_name: req.table_name,
            page: req.page,
            page_size: req.page_size,
            filters: req.filters,
            orders: req.orders,
        },
    )
    .await
    .map_err(|e| e.to_app_error())?;

    Ok(Json(GetTableDataResponse {
        data: result.data,
        total: result.total,
        total_status: result.total_status,
        has_more: result.has_more,
    }))
}

#[derive(Deserialize)]
struct GetTableSchemaRequest {
    table_name: String,
    db_id: Option<String>,
}

async fn get_table_schema(
    State(state): State<AppState>,
    axum::extract::Query(req): axum::extract::Query<GetTableSchemaRequest>,
) -> Result<Json<TableWithDetails>, AppError> {
    let (db_client, db_name) = resolve_db_client_for_request(&state, req.db_id.as_deref()).await?;
    let table = get_cached_table_schema(
        &state,
        req.db_id.as_deref(),
        &db_client,
        &db_name,
        &req.table_name,
    )
    .await?;
    Ok(Json(table))
}

#[derive(Deserialize)]
struct ExecuteDdlRequest {
    sql: String,
    db_id: Option<String>,
}

#[derive(Deserialize)]
struct PreviewDdlRequest {
    old_table: Option<TableWithDetails>,
    new_table: TableWithDetails,
    db_id: Option<String>,
}

#[derive(Serialize)]
struct PreviewDdlResponse {
    sql: String,
}

fn is_ddl_statement(stmt: &sqlparser::ast::Statement) -> bool {
    use sqlparser::ast::Statement;
    matches!(
        stmt,
        Statement::CreateTable(..)
            | Statement::CreateView { .. }
            | Statement::CreateIndex { .. }
            | Statement::CreateSchema { .. }
            | Statement::CreateFunction(..)
            | Statement::CreateTrigger(..)
            | Statement::CreateSequence { .. }
            | Statement::CreateType { .. }
            | Statement::AlterTable { .. }
            | Statement::AlterIndex { .. }
            | Statement::AlterView { .. }
            | Statement::Drop { .. }
            | Statement::Truncate { .. }
            | Statement::RenameTable(..)
    )
}

async fn preview_ddl(
    State(state): State<AppState>,
    Json(req): Json<PreviewDdlRequest>,
) -> Result<Json<PreviewDdlResponse>, AppError> {
    let (db_client, _) = resolve_db_client_for_request(&state, req.db_id.as_deref()).await?;
    match db_client.db_type {
        core_lib::config::DbType::MySQL | core_lib::config::DbType::MariaDB => {}
        core_lib::config::DbType::PostgreSQL => {
            return Err(AppError::BadRequest(
                "结构对比式 DDL 预览当前仅支持 MySQL。PostgreSQL 已支持连接、查询、结构浏览与已编写 DDL 的执行，请直接编写 DDL 后通过 DDL 执行接口提交。"
                    .to_string(),
            ));
        }
        _ => {
            return Err(AppError::BadRequest(
                "SQLite 结构变更（DDL）当前版本未支持。SQLite 仅支持连接与查询执行。"
                    .to_string(),
            ));
        }
    }

    let sql = DdlEngine::generate_preview(req.old_table.as_ref(), &req.new_table);
    Ok(Json(PreviewDdlResponse { sql }))
}

async fn execute_ddl(
    State(state): State<AppState>,
    Json(req): Json<ExecuteDdlRequest>,
) -> Result<Json<ExecuteResponse>, AppError> {
    let is_read_only = is_read_only_connection(&state, req.db_id.as_deref()).await;
    if is_read_only {
        return Err(AppError::Forbidden(
            "当前连接为只读模式，禁止执行非查询操作！".to_string(),
        ));
    }

    // Validate SQL is a DDL statement (not arbitrary SQL)
    let dialect = sqlparser::dialect::GenericDialect {};
    match sqlparser::parser::Parser::parse_sql(&dialect, req.sql.trim()) {
        Ok(stmts) => {
            if stmts.len() != 1 {
                return Err(AppError::BadRequest(
                    "DDL endpoint only supports a single statement".to_string(),
                ));
            }
            if !is_ddl_statement(&stmts[0]) {
                return Err(AppError::BadRequest(
                    "DDL endpoint only supports DDL statements (CREATE/ALTER/DROP/TRUNCATE/RENAME TABLE, VIEW, INDEX, SCHEMA, FUNCTION, TRIGGER, SEQUENCE, TYPE)"
                        .to_string(),
                ));
            }
        }
        Err(e) => {
            return Err(AppError::BadRequest(format!(
                "SQL parsing failed ({}), only valid DDL statements are allowed",
                e
            )));
        }
    }

    let (db_client, _) = resolve_db_client_for_request(&state, req.db_id.as_deref()).await?;

    // SQLite 的结构变更当前明确未支持；PostgreSQL 与 MySQL 允许执行已编写好的 DDL。
    if matches!(db_client.db_type, core_lib::config::DbType::SQLite) {
        return Err(AppError::BadRequest(
            "SQLite 结构变更（DDL）当前版本未支持。SQLite 仅支持连接与查询执行，请改用 MySQL 或 PostgreSQL。"
                .to_string(),
        ));
    }

    let affected_rows = match tokio::time::timeout(
        state.timeouts.db_query,
        db_client.pool.execute_affected(&req.sql),
    )
    .await
    {
        Ok(Ok(n)) => n,
        Ok(Err(e)) => return Err(db_error_to_app(e)),
        Err(_) => {
            return Err(AppError::Timeout(
                "DDL execution timed out after 30 seconds.".to_string(),
            ))
        }
    };
    clear_metadata_caches(&state).await;

    Ok(Json(ExecuteResponse {
        columns: vec![],
        row_count: 0,
        rows: vec![],
        affected_rows,
        execution_time_ms: 0,
        has_more: false,
        next_offset: None,
        chunk_offset: 0,
        chunk_size: None,
        preview_cap: None,
        truncated: false,
        transaction_state: None,
    }))
}

// ----------------- Tools API Handlers -----------------

#[derive(Deserialize)]
struct MockDataRequest {
    table_name: String,
    row_count: u32,
    rules: Option<std::collections::HashMap<String, String>>,
    /// Target connection; when omitted the currently active connection is used.
    db_id: Option<String>,
}

#[derive(Serialize)]
struct MockDataResponse {
    sql: String,
}

async fn generate_mock_data(
    State(state): State<AppState>,
    Json(req): Json<MockDataRequest>,
) -> Result<Json<MockDataResponse>, AppError> {
    let (db_client, db_name) = if let Some(db_id) = req.db_id.as_deref() {
        get_temp_db_client(&state, db_id).await?
    } else {
        let client = state
            .db_client
            .read()
            .await
            .clone()
            .ok_or_else(|| AppError::BadRequest("Database not connected".to_string()))?;
        let url = state
            .config
            .read()
            .await
            .get_active_db_url()
            .unwrap_or_default();
        let name = DbClient::extract_db_name(&url).unwrap_or_default();
        (client, name)
    };

    let columns = SchemaExtractor::get_columns(&db_client, &db_name, &req.table_name)
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;

    let foreign_keys = SchemaExtractor::get_foreign_keys(&db_client, &db_name, &req.table_name)
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;

    let table = TableWithDetails {
        table_name: req.table_name.clone(),
        columns,
        indexes: vec![],
        foreign_keys,
    };

    let config = state.config.read().await.clone();

    let sql = MockDataGenerator::generate(&config, &db_client, &table, req.row_count, req.rules)
        .await
        .map_err(AppError::InternalError)?;

    Ok(Json(MockDataResponse { sql }))
}

use axum::body::Body;

#[derive(Deserialize)]
struct ExportRequest {
    table_name: String,
    export_type: String, // "csv", "sql", "json"
    /// Target connection; when omitted the currently active connection is used.
    db_id: Option<String>,
}

/// 同步导出的流式文件：响应体读完后自动删除临时文件。
struct DeleteOnDropFile {
    file: tokio::fs::File,
    path: String,
}

impl tokio::io::AsyncRead for DeleteOnDropFile {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.file).poll_read(cx, buf)
    }
}

impl Drop for DeleteOnDropFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

async fn export_data(
    State(state): State<AppState>,
    Json(req): Json<ExportRequest>,
) -> Result<Response, AppError> {
    let db_client = if let Some(db_id) = req.db_id.as_deref() {
        get_temp_db_client(&state, db_id).await?.0
    } else {
        state
            .db_client
            .read()
            .await
            .clone()
            .ok_or_else(|| AppError::BadRequest("Database not connected".to_string()))?
    };

    let table_name = req.table_name.clone();
    let export_type = req.export_type.to_lowercase();
    if !matches!(export_type.as_str(), "csv" | "json" | "sql") {
        return Err(AppError::BadRequest(format!(
            "unsupported export_type: {}",
            export_type
        )));
    }

    // 导出一律先落临时文件：数据库侧边读边写，内存不随行数增长；
    // 同时可以在发送响应体之前返回明确的状态码与错误文案。
    let _permit = crate::handlers::util::acquire_export_slot(&state)?;
    let estimate = crate::handlers::util::estimate_export_size(&db_client, &table_name).await;
    crate::handlers::util::ensure_export_estimate(estimate.as_ref(), state.limits.max_file_bytes)?;

    let temp_dir = state.limits.temp_dir.trim_end_matches('/').to_string();
    ensure_temp_quota(&state.limits, 0).await?;
    let job_id = format!("sync-{}", uuid::Uuid::new_v4());
    let data_path = format!("{}/{}.{}", temp_dir, job_id, export_type);
    let job_req = ExportJobStartRequest {
        table_name: table_name.clone(),
        export_type: export_type.clone(),
        where_clause: None,
        primary_key: None,
        pk_start: None,
        pk_end: None,
        window_limit: None,
        window_offset: None,
        db_id: None,
    };

    let write_result = handlers::go_live::run_export_job(
        &db_client,
        &state,
        &job_id,
        &job_req,
        &data_path,
        state.limits.max_file_bytes,
        state.limits.max_export_memory_bytes,
    )
    .await;
    if let Err(e) = write_result {
        // 失败时不保留半成品文件。
        let _ = tokio::fs::remove_file(&data_path).await;
        return Err(e);
    }

    let file = tokio::fs::File::open(&data_path)
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    let body = Body::from_stream(tokio_util::io::ReaderStream::new(DeleteOnDropFile {
        file,
        path: data_path,
    }));

    let content_type = match export_type.as_str() {
        "csv" => "text/csv",
        "json" => "application/json",
        "sql" => "application/sql",
        _ => "text/plain",
    };

    let filename = format!("{}.{}", table_name, export_type);

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
pub(crate) struct ExportJobStartRequest {
    table_name: String,
    export_type: String,
    where_clause: Option<String>,
    primary_key: Option<String>,
    pk_start: Option<String>,
    pk_end: Option<String>,
    window_limit: Option<u64>,
    window_offset: Option<u64>,
    /// Target connection; when omitted the currently active connection is used.
    db_id: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct ImportJobStartRequest {
    table_name: String,
    data: Vec<std::collections::HashMap<String, serde_json::Value>>,
    mapping: std::collections::HashMap<String, String>,
    /// 失败策略：默认遇错即停；true 表示跳过失败行继续。
    #[serde(default)]
    skip_errors: bool,
    /// Target connection; when omitted the currently active connection is used.
    db_id: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct ImportSqlJobStartRequest {
    sql: String,
    force: Option<bool>,
    db_id: Option<String>,
    /// 失败策略：默认遇错即停；true 表示跳过失败语句继续。
    #[serde(default)]
    skip_errors: bool,
}

#[derive(Serialize)]
pub(crate) struct ToolJobStartResponse {
    job_id: String,
}

pub(crate) const GAP_TOO_LARGE_MSG: &str = "当前对比数据库差距过大，不符合结构/数据同步规范/数据传输规范";

fn schema_gap_too_large(diff: &core_lib::tools::SchemaDiff) -> bool {
    let total = diff.tables.len();
    if total == 0 {
        return false;
    }
    let changed = diff
        .tables
        .iter()
        .filter(|t| t.status != "unchanged")
        .count();
    changed >= 120 || (total >= 20 && changed.saturating_mul(100) / total >= 85)
}

fn data_gap_too_large(
    diff: &core_lib::sync::DataDiff,
    source_rows: usize,
    target_rows: usize,
) -> bool {
    let changed = diff.insert_count + diff.update_count + diff.delete_count;
    let compared = source_rows.max(target_rows);
    changed >= 50_000 || (compared >= 500 && changed.saturating_mul(100) / compared >= 85)
}

#[derive(Deserialize)]
struct SyncSchemaDiffRequest {
    source_db_id: String,
    target_db_id: String,
}

#[derive(Serialize)]
struct SyncSchemaDiffResponse {
    diff: core_lib::tools::SchemaDiff,
}

async fn sync_schema_diff(
    State(state): State<AppState>,
    Json(req): Json<SyncSchemaDiffRequest>,
) -> Result<Json<SyncSchemaDiffResponse>, AppError> {
    let (source_client, _source_db_name) = get_temp_db_client(&state, &req.source_db_id).await?;
    let (target_client, _target_db_name) = get_temp_db_client(&state, &req.target_db_id).await?;
    crate::handlers::util::require_mysql_client(&source_client, "结构同步（源库）")?;
    crate::handlers::util::require_mysql_client(&target_client, "结构同步（目标库）")?;

    let ctx = crate::bridge::bridge_service_context(&state);
    let source = core_lib::service::schema::SchemaService::get_schema(&ctx, Some(&req.source_db_id))
        .await
        .map_err(|e| e.to_app_error())?;
    let target = core_lib::service::schema::SchemaService::get_schema(&ctx, Some(&req.target_db_id))
        .await
        .map_err(|e| e.to_app_error())?;

    let (diff, _) = SyncEngine::schema_sync(&source, &target);
    if schema_gap_too_large(&diff) {
        return Err(AppError::BadRequest(GAP_TOO_LARGE_MSG.to_string()));
    }
    Ok(Json(SyncSchemaDiffResponse { diff }))
}

#[derive(Deserialize)]
struct SyncSchemaDdlRequest {
    source_db_id: String,
    target_db_id: String,
    selected_tables: Vec<String>,
}

#[derive(Serialize)]
struct SyncSchemaDdlResponse {
    ddl_statements: String,
}

async fn sync_schema_ddl(
    State(state): State<AppState>,
    Json(req): Json<SyncSchemaDdlRequest>,
) -> Result<Json<SyncSchemaDdlResponse>, AppError> {
    let (source_client, _source_db_name) = get_temp_db_client(&state, &req.source_db_id).await?;
    let (target_client, _target_db_name) = get_temp_db_client(&state, &req.target_db_id).await?;
    crate::handlers::util::require_mysql_client(&source_client, "结构同步（源库）")?;
    crate::handlers::util::require_mysql_client(&target_client, "结构同步（目标库）")?;

    let ctx = crate::bridge::bridge_service_context(&state);
    let source = core_lib::service::schema::SchemaService::get_schema(&ctx, Some(&req.source_db_id))
        .await
        .map_err(|e| e.to_app_error())?;
    let target = core_lib::service::schema::SchemaService::get_schema(&ctx, Some(&req.target_db_id))
        .await
        .map_err(|e| e.to_app_error())?;
    let (diff, _) = SyncEngine::schema_sync(&source, &target);
    if schema_gap_too_large(&diff) {
        return Err(AppError::BadRequest(GAP_TOO_LARGE_MSG.to_string()));
    }

    let ddl = core_lib::sync::SchemaSyncEngine::generate_ddl_for_selection(
        &source,
        &target,
        &req.selected_tables,
    );
    Ok(Json(SyncSchemaDdlResponse {
        ddl_statements: ddl,
    }))
}

async fn fetch_all_table_data(
    db_client: &DbClient,
    table_name: &str,
) -> Result<Vec<serde_json::Value>, AppError> {
    let safe_table = quote_mysql_ident(table_name)?;
    let data_sql = format!("SELECT * FROM {} LIMIT 50000", safe_table);
    let result_rows = sqlx::query(&data_sql)
        .fetch_all(db_client.mysql_pool()?)
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;

    let mut rows = Vec::new();
    for row in result_rows {
        let map = row_to_json(&row);
        rows.push(serde_json::Value::Object(map));
    }
    Ok(rows)
}

#[derive(Deserialize)]
struct SyncDataDiffRequest {
    table_name: String,
    source_db_id: String,
    target_db_id: String,
    primary_key: String,
}

#[derive(Serialize)]
struct SyncDataDiffResponse {
    diff: core_lib::sync::DataDiff,
}

async fn sync_data_diff(
    State(state): State<AppState>,
    Json(req): Json<SyncDataDiffRequest>,
) -> Result<Json<SyncDataDiffResponse>, AppError> {
    let (source_client, _) = get_temp_db_client(&state, &req.source_db_id).await?;
    let (target_client, _) = get_temp_db_client(&state, &req.target_db_id).await?;

    let source_data = fetch_all_table_data(&source_client, &req.table_name).await?;
    let target_data = fetch_all_table_data(&target_client, &req.table_name).await?;

    let diff = core_lib::sync::DataSyncEngine::compute_data_diff(
        &req.table_name,
        &source_data,
        &target_data,
        &req.primary_key,
    );
    if data_gap_too_large(&diff, source_data.len(), target_data.len()) {
        return Err(AppError::BadRequest(GAP_TOO_LARGE_MSG.to_string()));
    }
    Ok(Json(SyncDataDiffResponse { diff }))
}

#[derive(Deserialize)]
struct SyncDataDmlRequest {
    diffs: Vec<core_lib::sync::DataDiff>,
    selections: std::collections::HashMap<String, Vec<String>>,
    primary_key: String,
}

#[derive(Serialize)]
struct SyncDataDmlResponse {
    dml_statements: String,
}

async fn sync_data_dml(
    Json(req): Json<SyncDataDmlRequest>,
) -> Result<Json<SyncDataDmlResponse>, AppError> {
    let dml = core_lib::sync::DataSyncEngine::generate_dml_for_selection(
        &req.diffs,
        &req.selections,
        &req.primary_key,
    );
    Ok(Json(SyncDataDmlResponse {
        dml_statements: dml,
    }))
}

// ----------------- SQL History API Handlers -----------------

async fn get_history(State(state): State<AppState>) -> Result<Json<Vec<SqlHistory>>, AppError> {
    match SqlHistoryStore::load().await {
        Ok(store) => {
            let history = store.data.history.clone();
            let mut state_store = state.sql_history.write().await;
            *state_store = store;
            Ok(Json(history))
        }
        Err(_) => {
            let store = state.sql_history.read().await;
            Ok(Json(store.data.history.clone()))
        }
    }
}

async fn clear_history(State(state): State<AppState>) -> Result<StatusCode, AppError> {
    let store_clone = {
        let mut store = state.sql_history.write().await;
        store.clear_history();
        store.clone()
    };
    store_clone
        .save()
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    Ok(StatusCode::OK)
}

#[derive(Deserialize)]
struct ExplainSqlRequest {
    sql: String,
    #[serde(default)]
    db_id: Option<String>,
}

#[derive(Serialize)]
struct ExplainSqlResponse {
    rows: Vec<serde_json::Value>,
}

#[derive(Serialize)]
struct SessionInfoEntryResponse {
    key: String,
    value: Option<String>,
}

#[derive(Serialize)]
struct SessionInfoResponse {
    db_id: Option<String>,
    db_name: String,
    connection_name: Option<String>,
    read_only: bool,
    fetched_at: i64,
    summary: Vec<SessionInfoEntryResponse>,
    session_variables: Vec<SessionInfoEntryResponse>,
    global_variables: Vec<SessionInfoEntryResponse>,
}

fn session_info_entry(key: &str, value: Option<String>) -> SessionInfoEntryResponse {
    SessionInfoEntryResponse {
        key: key.to_string(),
        value,
    }
}

fn build_show_variables_query(scope: &str, variable_names: &[&str]) -> String {
    let names = variable_names
        .iter()
        .map(|name| format!("'{}'", name))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "SHOW {} VARIABLES WHERE Variable_name IN ({})",
        scope, names
    )
}

async fn fetch_mysql_variable_map(
    pool: &sqlx::MySqlPool,
    scope: &str,
    variable_names: &[&str],
    policy: &TimeoutPolicy,
) -> Result<HashMap<String, String>, AppError> {
    let query = build_show_variables_query(scope, variable_names);
    let rows = tokio::time::timeout(policy.db_query, sqlx::query(&query).fetch_all(pool))
        .await
        .map_err(|_| {
            AppError::InternalError(format!(
                "{} variable query timed out",
                scope.to_lowercase()
            ))
        })?
        .map_err(|e| AppError::InternalError(e.to_string()))?;

    let mut values = HashMap::new();
    for row in rows {
        let key = row
            .try_get::<String, _>("Variable_name")
            .map_err(|e| AppError::InternalError(e.to_string()))?;
        let value = row
            .try_get::<String, _>("Value")
            .map_err(|e| AppError::InternalError(e.to_string()))?;
        values.insert(key, value);
    }
    Ok(values)
}

fn pick_mysql_variable(values: &HashMap<String, String>, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| values.get(*key).cloned())
        .filter(|value| !value.trim().is_empty())
}

async fn explain_sql(
    State(state): State<AppState>,
    Json(req): Json<ExplainSqlRequest>,
) -> Result<Json<ExplainSqlResponse>, AppError> {
    let (db_client, _) = resolve_db_client_for_request(&state, req.db_id.as_deref()).await?;

    // Validate SQL is a single read-only statement before prepending EXPLAIN
    let dialect = sqlparser::dialect::GenericDialect {};
    let statements = sqlparser::parser::Parser::parse_sql(&dialect, req.sql.trim())
        .map_err(|e| AppError::BadRequest(format!("SQL parse error: {}", e)))?;
    if statements.len() != 1 {
        return Err(AppError::BadRequest("EXPLAIN only supports a single statement".to_string()));
    }
    if !core_lib::sql::util::is_read_only_statement(&statements[0]) {
        return Err(AppError::BadRequest("EXPLAIN only supports read-only queries (SELECT, SHOW, DESCRIBE)".to_string()));
    }
    // EXPLAIN 各引擎语法与输出格式不同：
    // - MySQL:         EXPLAIN <sql>，输出列结构化的执行计划表
    // - PostgreSQL:    EXPLAIN <sql>，输出单列 "QUERY PLAN" 文本树
    // - SQLite:        EXPLAIN QUERY PLAN <sql>，输出 id/parent/notused/detail 四列
    let db_type = db_client.db_type.clone();
    let explain_sql = match db_type {
        core_lib::config::DbType::SQLite => format!("EXPLAIN QUERY PLAN {}", req.sql),
        _ => format!("EXPLAIN {}", req.sql),
    };

    let result = tokio::time::timeout(
        state.timeouts.db_query,
        db_client.pool.fetch_all_result(&explain_sql),
    )
    .await
    .map_err(|_| AppError::Timeout("EXPLAIN timed out after 30 seconds.".to_string()))?
    .map_err(|e| {
        AppError::BadRequest(format!(
            "{} 无法执行该 SQL 的 EXPLAIN：{}",
            db_client.db_type.display_name(),
            e
        ))
    })?;

    let rows = result.to_objects();
    Ok(Json(ExplainSqlResponse { rows }))
}

async fn session_info(
    State(state): State<AppState>,
    Query(query): Query<DbContextQuery>,
) -> Result<Json<SessionInfoResponse>, AppError> {
    let config = state.config.read().await.clone();
    let effective_db_id = query.db_id.clone().or_else(|| config.active_db_id.clone());
    let connection = effective_db_id.as_deref().and_then(|db_id| {
        config
            .db_connections
            .iter()
            .find(|item| item.id == db_id)
            .cloned()
    });
    let (db_client, db_name) = resolve_db_client_for_request(&state, effective_db_id.as_deref()).await?;
    crate::handlers::util::require_mysql_client(&db_client, "会话信息（SHOW VARIABLES）")?;
    let policy = state.timeouts.clone();

    let summary_row = tokio::time::timeout(
        policy.db_query,
        sqlx::query(
            "SELECT \
                CAST(CONNECTION_ID() AS CHAR) AS connection_id, \
                NULLIF(DATABASE(), '') AS current_database, \
                CURRENT_USER() AS current_user, \
                USER() AS session_user, \
                VERSION() AS server_version, \
                DATE_FORMAT(NOW(), '%Y-%m-%d %H:%i:%s') AS server_time",
        )
        .fetch_one(db_client.mysql_pool()?),
    )
    .await
    .map_err(|_| AppError::InternalError("Session info query timed out".to_string()))?
    .map_err(|e| AppError::InternalError(e.to_string()))?;

    let session_map = fetch_mysql_variable_map(
        db_client.mysql_pool()?,
        "SESSION",
        &[
            "autocommit",
            "transaction_isolation",
            "tx_isolation",
            "sql_mode",
            "time_zone",
            "character_set_connection",
            "collation_connection",
        ],
        &policy,
    )
    .await?;

    let global_map = fetch_mysql_variable_map(
        db_client.mysql_pool()?,
        "GLOBAL",
        &[
            "version_comment",
            "hostname",
            "port",
            "character_set_server",
            "collation_server",
            "max_connections",
            "max_allowed_packet",
            "wait_timeout",
            "interactive_timeout",
            "read_only",
        ],
        &policy,
    )
    .await?;

    let current_database = summary_row
        .try_get::<Option<String>, _>("current_database")
        .map_err(|e| AppError::InternalError(e.to_string()))?
        .or_else(|| (!db_name.trim().is_empty()).then_some(db_name.clone()));

    let summary = vec![
        session_info_entry(
            "connection_id",
            summary_row
                .try_get::<Option<String>, _>("connection_id")
                .map_err(|e| AppError::InternalError(e.to_string()))?,
        ),
        session_info_entry("current_database", current_database),
        session_info_entry(
            "current_user",
            summary_row
                .try_get::<Option<String>, _>("current_user")
                .map_err(|e| AppError::InternalError(e.to_string()))?,
        ),
        session_info_entry(
            "session_user",
            summary_row
                .try_get::<Option<String>, _>("session_user")
                .map_err(|e| AppError::InternalError(e.to_string()))?,
        ),
        session_info_entry(
            "server_version",
            summary_row
                .try_get::<Option<String>, _>("server_version")
                .map_err(|e| AppError::InternalError(e.to_string()))?,
        ),
        session_info_entry(
            "server_time",
            summary_row
                .try_get::<Option<String>, _>("server_time")
                .map_err(|e| AppError::InternalError(e.to_string()))?,
        ),
    ];

    let session_variables = vec![
        session_info_entry(
            "autocommit",
            pick_mysql_variable(&session_map, &["autocommit"]),
        ),
        session_info_entry(
            "transaction_isolation",
            pick_mysql_variable(&session_map, &["transaction_isolation", "tx_isolation"]),
        ),
        session_info_entry("sql_mode", pick_mysql_variable(&session_map, &["sql_mode"])),
        session_info_entry("time_zone", pick_mysql_variable(&session_map, &["time_zone"])),
        session_info_entry(
            "character_set_connection",
            pick_mysql_variable(&session_map, &["character_set_connection"]),
        ),
        session_info_entry(
            "collation_connection",
            pick_mysql_variable(&session_map, &["collation_connection"]),
        ),
    ];

    let global_variables = vec![
        session_info_entry(
            "version_comment",
            pick_mysql_variable(&global_map, &["version_comment"]),
        ),
        session_info_entry("hostname", pick_mysql_variable(&global_map, &["hostname"])),
        session_info_entry("port", pick_mysql_variable(&global_map, &["port"])),
        session_info_entry(
            "character_set_server",
            pick_mysql_variable(&global_map, &["character_set_server"]),
        ),
        session_info_entry(
            "collation_server",
            pick_mysql_variable(&global_map, &["collation_server"]),
        ),
        session_info_entry(
            "max_connections",
            pick_mysql_variable(&global_map, &["max_connections"]),
        ),
        session_info_entry(
            "max_allowed_packet",
            pick_mysql_variable(&global_map, &["max_allowed_packet"]),
        ),
        session_info_entry(
            "wait_timeout",
            pick_mysql_variable(&global_map, &["wait_timeout"]),
        ),
        session_info_entry(
            "interactive_timeout",
            pick_mysql_variable(&global_map, &["interactive_timeout"]),
        ),
        session_info_entry("read_only", pick_mysql_variable(&global_map, &["read_only"])),
    ];

    Ok(Json(SessionInfoResponse {
        db_id: effective_db_id,
        db_name,
        connection_name: connection
            .as_ref()
            .map(|item| {
                if item.name.trim().is_empty() {
                    item.id.clone()
                } else {
                    item.name.clone()
                }
            }),
        read_only: connection
            .as_ref()
            .map(|item| item.is_read_only)
            .unwrap_or(false),
        fetched_at: chrono::Utc::now().timestamp_millis(),
        summary,
        session_variables,
        global_variables,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        http::Request,
        response::IntoResponse,
        routing::{get, post},
        Router,
    };
    use std::time::Duration;
    use tokio::sync::Semaphore;
    use tower::ServiceExt;

    fn test_state() -> AppState {
        let config = AppConfig::default();
        let limits = RuntimeLimits {
            temp_dir: format!("/tmp/local-ai-sql-test-{}", uuid::Uuid::new_v4()),
            ..Default::default()
        };

        AppState {
            config: Arc::new(RwLock::new(config)),
            db_client: Arc::new(RwLock::new(None)),
            db_client_cache: Arc::new(RwLock::new(HashMap::new())),
            virtual_schema: Arc::new(RwLock::new(None)),
            schema_cache: Arc::new(RwLock::new(HashMap::new())),
            table_schema_cache: Arc::new(RwLock::new(HashMap::new())),
            rule_store: Arc::new(RwLock::new(RuleStore::default())),
            policy: Arc::new(RwLock::new(Policy::default())),
            sql_history: Arc::new(RwLock::new(SqlHistoryStore::default())),
            knowledge_base: Arc::new(RwLock::new(KnowledgeBase::default())),
            sync_jobs: Arc::new(RwLock::new(HashMap::new())),
            perf_sync_jobs: Arc::new(RwLock::new(HashMap::new())),
            active_queries: Arc::new(RwLock::new(HashMap::new())),
            transaction_sessions: Arc::new(RwLock::new(HashMap::new())),
            tool_jobs: Arc::new(RwLock::new(HashMap::new())),
            tool_job_handles: Arc::new(RwLock::new(HashMap::new())),
            timeouts: TimeoutPolicy::default(),
            limits: limits.clone(),
            job_semaphore: Arc::new(Semaphore::new(limits.max_job_concurrency)),
            export_semaphore: Arc::new(Semaphore::new(1)),
            ops: Arc::new(core_lib::ops::OpsRuntime::for_test()),
        }
    }

    fn test_app(state: AppState) -> Router {
        let api = Router::new()
            .route("/config", get(get_config))
            .route(
                "/ops/inspection/presets",
                get(inspection_handlers::ops_inspection_presets),
            )
            .route(
                "/ops/inspection/schedule",
                get(inspection_handlers::ops_inspection_get_schedule),
            )
            .route("/diagnostics/perf/probe", post(diagnostics_perf_probe))
            .route(
                "/diagnostics/perf/suites",
                get(diagnostics_perf_suite_list).post(diagnostics_perf_suite_save),
            )
            .route(
                "/diagnostics/perf/suites/baseline",
                get(diagnostics_perf_suite_baseline_get).post(diagnostics_perf_suite_baseline_pin),
            )
            .route(
                "/diagnostics/perf/suite-diffs",
                get(diagnostics_perf_suite_diff_list).post(diagnostics_perf_suite_diff_save),
            )
            .route(
                "/diagnostics/perf/suites/:suite_id",
                get(diagnostics_perf_suite_detail),
            )
            .route("/execute", post(execute_sql))
            .route("/execute/transaction", post(execute_transaction))
            .route("/execute/cancel", post(execute_cancel))
            .route("/table/ddl", post(execute_ddl))
            .route("/sql/session-info", get(session_info))
            .route("/tools/schema-sync/diff", post(sync_schema_diff))
            .route("/tools/data-transfer/execute", post(transfer_execute))
            .route("/tools/data-sync/compare", post(mysql_sync_compare))
            .route("/tools/data-sync/preview", post(mysql_sync_preview))
            .route("/tools/data-sync/deploy", post(mysql_sync_deploy))
            .route("/tools/data-sync/jobs/:job_id", get(mysql_sync_job_status))
            .route("/tools/perf-sync/start", post(perf_sync_start))
            .route("/tools/perf-sync/check", post(perf_sync_check))
            .route("/tools/perf-sync/jobs/:job_id", get(perf_sync_job_status))
            .route("/tools/jobs/go-live/start", post(go_live_job_start))
            .route("/tools/go-live/reports", get(go_live_reports_list))
            .route("/tools/go-live/audit", get(go_live_audit_list))
            .route("/ops/session-token", get(ops_handlers::ops_session_token))
            .route("/ops/analyze", post(ops_handlers::ops_analyze))
            .route(
                "/ops/field-governance/diff-page",
                post(ops_handlers::ops_field_diff_page),
            )
            .route(
                "/ops/field-governance/backfill",
                get(ops_handlers::ops_field_backfill_list)
                    .post(ops_handlers::ops_field_backfill_start),
            )
            .route(
                "/ops/field-governance/backfill/approve",
                post(ops_handlers::ops_field_backfill_approve),
            )
            .route(
                "/ops/field-governance/backfill/:job_id",
                get(ops_handlers::ops_field_backfill_status),
            )
            .route(
                "/ops/field-governance/backfill/:job_id/pause",
                post(ops_handlers::ops_field_backfill_pause),
            )
            .route(
                "/ops/field-governance/backfill/:job_id/resume",
                post(ops_handlers::ops_field_backfill_resume),
            )
            .route(
                "/ops/field-governance/backfill/:job_id/abort",
                post(ops_handlers::ops_field_backfill_abort),
            )
            .route(
                "/ops/plans/:plan_id/approve",
                post(ops_handlers::ops_approve),
            )
            .route(
                "/ops/plans/:plan_id/execute",
                post(ops_handlers::ops_execute),
            )
            .route("/ops/plans/:plan_id/rollback", post(ops_handlers::ops_rollback))
            .route("/ops/policy", post(ops_handlers::ops_update_policy))
            .route("/tools/jobs/:job_id", get(tool_job_status))
            .route("/tools/jobs/:job_id/cancel", post(tool_job_cancel))
            .route(
                "/tools/jobs/:job_id/artifacts/:artifact",
                get(tool_job_artifact_download),
            );

        Router::new().nest("/backend", api).with_state(state)
    }

    #[tokio::test]
    async fn config_endpoint_returns_json() {
        let app = test_app(test_state());

        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/backend/config")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(v.get("db_connections").is_some());
        assert!(v.get("ai_profiles").is_some());
    }

    #[tokio::test]
    async fn ops_inspection_meta_routes_are_wired() {
        let app = test_app(test_state());
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/backend/ops/inspection/presets")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["presets"].as_array().unwrap().len(), 4);
        assert_eq!(v["checks"].as_array().unwrap().len(), 9);

        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/backend/ops/inspection/schedule")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["schedule"]["enabled"], false);
        assert!(v.get("scope_note").is_some());
        assert_eq!(v["notification_channels"][0], "in_app");
    }

    #[tokio::test]
    async fn ops_write_endpoints_require_local_token() {
        let app = test_app(test_state());
        for (uri, body) in [
            ("/backend/ops/analyze", r#"{"table":"orders","workload_sql":[]}"#),
            (
                "/backend/ops/plans/nope/approve",
                r#"{"plan_hash":"h","action_ids":["create_index:idx"]}"#,
            ),
            (
                "/backend/ops/plans/nope/execute",
                r#"{"action_ids":["create_index:idx"],"approval_token":"t"}"#,
            ),
            (
                "/backend/ops/plans/nope/rollback",
                r#"{"reason":"test"}"#,
            ),
            (
                "/backend/ops/policy",
                r#"{"environment":"prod","allow_apply":true,"allow_prod_apply":false,"approval_ttl_secs":300,"max_affected_rows":10000000,"allow_large_table":false,"ddl_timeout_secs":600,"lock_wait_timeout_secs":5}"#,
            ),
        ] {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(uri)
                        .header("content-type", "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::FORBIDDEN, "uri={uri}");
        }
    }

    #[tokio::test]
    async fn ops_session_token_is_available_for_frontend_handshake() {
        let app = test_app(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/backend/ops/session-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v.get("header").and_then(|x| x.as_str()), Some("x-ops-token"));
        assert_eq!(v.get("token").and_then(|x| x.as_str()), Some("test-ops-token"));
    }

    #[tokio::test]
    async fn perf_probe_rejects_non_read_only_sql() {
        let app = test_app(test_state());

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/backend/diagnostics/perf/probe")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"operation":"query_select_small","sql":"DELETE FROM users"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            v.get("code").and_then(|x| x.as_str()),
            Some("ERR_BAD_REQUEST")
        );
        assert!(v
            .get("details")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .contains("read-only"));
    }

    #[tokio::test]
    async fn perf_probe_returns_error_when_db_not_connected() {
        let app = test_app(test_state());

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/backend/diagnostics/perf/probe")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"operation":"connect_warm","iterations":2}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            v.get("code").and_then(|x| x.as_str()),
            Some("ERR_BAD_REQUEST")
        );
        assert!(v
            .get("details")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .contains("Database not connected"));
    }

    #[tokio::test]
    async fn perf_probe_table_first_page_requires_table_name() {
        let app = test_app(test_state());

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/backend/diagnostics/perf/probe")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"operation":"table_first_page"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            v.get("code").and_then(|x| x.as_str()),
            Some("ERR_BAD_REQUEST")
        );
        assert!(v
            .get("details")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .contains("table_name"));
    }

    #[tokio::test]
    async fn perf_suite_archive_list_returns_empty_when_no_reports_exist() {
        let app = test_app(test_state());

        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/backend/diagnostics/perf/suites?limit=10")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v, serde_json::json!([]));
    }

    #[tokio::test]
    async fn perf_suite_archive_save_and_list_round_trip() {
        let app = test_app(test_state());
        let payload = r#"{
            "id": "suite-test-1",
            "recorded_at": "2026-05-08T10:00:00Z",
            "connection_id": "db-local",
            "connection_name": "Local MySQL",
            "label": "before optimization",
            "build_version": "v0.9.3",
            "branch_name": "codex/perf",
            "environment": "desktop-tauri",
            "notes": "baseline run",
            "iterations": 5,
            "sql": "SELECT 1 AS perf_probe",
            "table_name": "users",
            "status": "success",
            "failed_operation": null,
            "error": null,
            "results": [
                {
                    "id": "entry-1",
                    "recorded_at": "2026-05-08T10:00:00Z",
                    "connection_id": "db-local",
                    "connection_name": "Local MySQL",
                    "operation": "connect_warm",
                    "iterations": 5,
                    "sql": null,
                    "table_name": null,
                    "result": {
                        "operation": "connect_warm",
                        "sample_count": 5,
                        "min_ms": 10,
                        "max_ms": 25,
                        "avg_ms": 16,
                        "p50_ms": 15,
                        "p95_ms": 25,
                        "rows": null,
                        "budget": {
                            "operation": "connect_warm",
                            "target_p50_ms": 50,
                            "target_p95_ms": 120,
                            "source": "test"
                        },
                        "samples": [
                            { "operation": "connect_warm", "iteration": 1, "duration_ms": 15, "rows": null }
                        ]
                    }
                }
            ]
        }"#;

        let save_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/backend/diagnostics/perf/suites")
                    .header("content-type", "application/json")
                    .body(Body::from(payload))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(save_resp.status(), StatusCode::OK);
        let save_body = to_bytes(save_resp.into_body(), usize::MAX).await.unwrap();
        let saved: serde_json::Value = serde_json::from_slice(&save_body).unwrap();
        let archive_path = saved
            .get("archive_path")
            .and_then(|x| x.as_str())
            .unwrap_or("");
        assert!(archive_path.contains("perf-suites"));
        assert!(std::path::Path::new(archive_path).exists());

        let list_resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/backend/diagnostics/perf/suites?limit=10")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(list_resp.status(), StatusCode::OK);
        let list_body = to_bytes(list_resp.into_body(), usize::MAX).await.unwrap();
        let list: serde_json::Value = serde_json::from_slice(&list_body).unwrap();
        assert_eq!(list.as_array().map(|items| items.len()), Some(1));
        assert_eq!(
            list.get(0)
                .and_then(|item| item.get("label"))
                .and_then(|x| x.as_str()),
            Some("before optimization")
        );
        assert_eq!(
            list.get(0)
                .and_then(|item| item.get("environment"))
                .and_then(|x| x.as_str()),
            Some("desktop-tauri")
        );
    }

    #[tokio::test]
    async fn perf_suite_archive_detail_and_baseline_round_trip() {
        let app = test_app(test_state());
        let payload = r#"{
            "id": "suite-test-detail",
            "recorded_at": "2026-05-08T10:10:00Z",
            "connection_id": "db-local",
            "connection_name": "Local MySQL",
            "label": "after optimization",
            "build_version": "v0.9.4",
            "branch_name": "codex/perf-detail",
            "environment": "web-local",
            "notes": "candidate baseline",
            "iterations": 5,
            "sql": "SELECT 1 AS perf_probe",
            "table_name": "orders",
            "status": "success",
            "failed_operation": null,
            "error": null,
            "results": []
        }"#;

        let save_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/backend/diagnostics/perf/suites")
                    .header("content-type", "application/json")
                    .body(Body::from(payload))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(save_resp.status(), StatusCode::OK);

        let detail_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/backend/diagnostics/perf/suites/suite-test-detail")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(detail_resp.status(), StatusCode::OK);
        let detail_body = to_bytes(detail_resp.into_body(), usize::MAX).await.unwrap();
        let detail: serde_json::Value = serde_json::from_slice(&detail_body).unwrap();
        assert_eq!(
            detail.get("label").and_then(|x| x.as_str()),
            Some("after optimization")
        );

        let baseline_empty_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/backend/diagnostics/perf/suites/baseline")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(baseline_empty_resp.status(), StatusCode::OK);
        let baseline_empty_body =
            to_bytes(baseline_empty_resp.into_body(), usize::MAX).await.unwrap();
        let baseline_empty: serde_json::Value =
            serde_json::from_slice(&baseline_empty_body).unwrap();
        assert!(baseline_empty.is_null());

        let pin_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/backend/diagnostics/perf/suites/baseline")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"suite_id":"suite-test-detail"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(pin_resp.status(), StatusCode::OK);

        let baseline_resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/backend/diagnostics/perf/suites/baseline")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(baseline_resp.status(), StatusCode::OK);
        let baseline_body = to_bytes(baseline_resp.into_body(), usize::MAX).await.unwrap();
        let baseline: serde_json::Value = serde_json::from_slice(&baseline_body).unwrap();
        assert_eq!(
            baseline.get("id").and_then(|value| value.as_str()),
            Some("suite-test-detail")
        );
        assert_eq!(
            baseline.get("label").and_then(|value| value.as_str()),
            Some("after optimization")
        );
    }

    #[tokio::test]
    async fn perf_suite_diff_archive_save_and_filtered_list_round_trip() {
        let app = test_app(test_state());
        let payload = r#"{
            "id": "suite-diff-test-1",
            "recorded_at": "2026-05-08T11:00:00Z",
            "current_suite_id": "suite-current",
            "baseline_suite_id": "suite-baseline",
            "current_suite_label": "after optimization",
            "baseline_suite_label": "before optimization",
            "gate_status": "pass",
            "baseline_scope": "pinned",
            "current_suite": { "id": "suite-current", "label": "after optimization" },
            "baseline_suite": { "id": "suite-baseline", "label": "before optimization" },
            "gate": { "status": "pass", "message": "ok" },
            "summary": { "fasterCount": 4, "slowerCount": 0, "comparableCount": 4 },
            "rows": [
                { "operation": "connect_warm", "p50": { "value": "-5 ms" } }
            ]
        }"#;

        let save_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/backend/diagnostics/perf/suite-diffs")
                    .header("content-type", "application/json")
                    .body(Body::from(payload))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(save_resp.status(), StatusCode::OK);
        let save_body = to_bytes(save_resp.into_body(), usize::MAX).await.unwrap();
        let saved: serde_json::Value = serde_json::from_slice(&save_body).unwrap();
        let archive_path = saved
            .get("archive_path")
            .and_then(|x| x.as_str())
            .unwrap_or("");
        assert!(archive_path.contains("perf-suites"));
        assert!(archive_path.contains("diffs"));
        assert!(std::path::Path::new(archive_path).exists());

        let list_resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/backend/diagnostics/perf/suite-diffs?limit=10&current_suite_id=suite-current&baseline_suite_id=suite-baseline")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(list_resp.status(), StatusCode::OK);
        let list_body = to_bytes(list_resp.into_body(), usize::MAX).await.unwrap();
        let list: serde_json::Value = serde_json::from_slice(&list_body).unwrap();
        assert_eq!(list.as_array().map(|items| items.len()), Some(1));
        assert_eq!(
            list.get(0)
                .and_then(|item| item.get("gate_status"))
                .and_then(|x| x.as_str()),
            Some("pass")
        );
    }

    #[tokio::test]
    async fn execute_sql_returns_error_when_db_not_connected() {
        let app = test_app(test_state());

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/backend/execute")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"sql":"SELECT 1"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            v.get("code").and_then(|x| x.as_str()),
            Some("ERR_BAD_REQUEST")
        );
        assert!(v.get("type").is_some());
        assert!(v
            .get("details")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .contains("Database not connected"));
    }

    #[tokio::test]
    async fn session_info_returns_error_when_db_not_connected() {
        let app = test_app(test_state());

        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/backend/sql/session-info")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            v.get("code").and_then(|x| x.as_str()),
            Some("ERR_BAD_REQUEST")
        );
        assert!(v
            .get("details")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .contains("Database not connected"));
    }

    #[tokio::test]
    async fn timeout_error_includes_code_and_type() {
        let resp = AppError::Timeout("x".to_string()).into_response();
        assert_eq!(resp.status(), StatusCode::GATEWAY_TIMEOUT);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v.get("code").and_then(|x| x.as_str()), Some("ERR_TIMEOUT"));
        assert_eq!(v.get("type").and_then(|x| x.as_str()), Some("timeout"));
    }

    #[tokio::test]
    async fn tool_job_cancel_sets_canceled_status() {
        let state = test_state();
        let job_id = "job-test";
        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        {
            let mut jobs = state.tool_jobs.write().await;
            jobs.insert(
                job_id.to_string(),
                ToolJob {
                    job_id: job_id.to_string(),
                    kind: ToolJobKind::Export,
                    status: ToolJobStatus::Running,
                    progress: ToolJobProgress {
                        current: 0,
                        total: None,
                        message: None,
                    },
                    created_at: 0,
                    updated_at: 0,
                    elapsed_ms: None,
                    artifacts: None,
                    result: None,
                    error: None,
                },
            );
            let mut handles = state.tool_job_handles.write().await;
            handles.insert(job_id.to_string(), handle.abort_handle());
        }

        let app = test_app(state.clone());
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/backend/tools/jobs/{}/cancel", job_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(format!("/backend/tools/jobs/{}", job_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v.get("status").and_then(|x| x.as_str()), Some("canceled"));
    }

    #[tokio::test]
    async fn transfer_execute_returns_error_when_source_db_missing() {
        let app = test_app(test_state());

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/backend/tools/data-transfer/execute")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"source_type":"network_db","source_db_id":"missing","source_path":null,"source_url":null,"source_table":null,"target_url":"mysql://root@127.0.0.1:3306/test","target_table":"t","mode":"Append","mappings":[]}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let s = String::from_utf8_lossy(&body);
        assert!(s.contains("Source DB connection not found"));
    }

    #[tokio::test]
    async fn go_live_job_start_creates_report_artifact_on_failure() {
        let app = test_app(test_state());

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/backend/tools/jobs/go-live/start")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let job_id = v
            .get("job_id")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        assert!(!job_id.is_empty());

        let mut job: Option<serde_json::Value> = None;
        for _ in 0..200 {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("GET")
                        .uri(format!("/backend/tools/jobs/{}", job_id))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_eq!(resp.status(), StatusCode::OK);
            let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let status = v.get("status").and_then(|x| x.as_str()).unwrap_or("");
            if status == "completed" || status == "error" || status == "canceled" {
                job = Some(v);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let job = job.expect("job not finished");
        assert_eq!(job.get("kind").and_then(|x| x.as_str()), Some("go_live"));
        assert_eq!(job.get("status").and_then(|x| x.as_str()), Some("error"));
        let data_path = job
            .get("artifacts")
            .and_then(|a| a.get("data_path"))
            .and_then(|x| x.as_str())
            .unwrap_or("");
        assert!(data_path.ends_with(".json"));

        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(format!("/backend/tools/jobs/{}/artifacts/data", job_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let report: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let steps = report
            .get("steps")
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default();
        assert_eq!(steps.len(), 5);
    }

    #[tokio::test]
    async fn go_live_reports_list_and_read_only_skip_work() {
        let cfg = AppConfig {
            db_connections: vec![core_lib::config::DbConnection {
                id: "ro".to_string(),
                name: "ro".to_string(),
                url: "mysql://root@127.0.0.1:1/test".to_string(),
                group_name: None,
                color: None,
                is_favorite: false,
                ssh: None,
                ssl: None,
                db_type: Some(core_lib::config::DbType::MySQL),
                capability_level: None,
                schema: None,
                is_read_only: true,
            }],
            active_db_id: Some("ro".to_string()),
            ..Default::default()
        }
        .normalize();

        let mut state = test_state_with_config(cfg);
        state.limits.temp_dir = format!("/tmp/local-ai-sql-test-{}", uuid::Uuid::new_v4());

        let app = test_app(state);
        let payload = serde_json::json!({
            "steps": ["config", "sql_smoke"],
            "connection_ids": ["ro"],
            "operator": "tester"
        });

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/backend/tools/jobs/go-live/start")
                    .header("content-type", "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let job_id = v
            .get("job_id")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        assert!(!job_id.is_empty());

        let mut job: Option<serde_json::Value> = None;
        for _ in 0..300 {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("GET")
                        .uri(format!("/backend/tools/jobs/{}", job_id))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_eq!(resp.status(), StatusCode::OK);
            let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let status = v.get("status").and_then(|x| x.as_str()).unwrap_or("");
            if status == "completed" || status == "error" || status == "canceled" {
                job = Some(v);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let job = job.expect("job not finished");
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(format!("/backend/tools/jobs/{}/artifacts/data", job_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let report: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let steps = report
            .get("steps")
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default();

        let ro_sql_smoke = steps.iter().find(|s| {
            s.get("name").and_then(|x| x.as_str()) == Some("sql_smoke")
                && s.get("connection_id").and_then(|x| x.as_str()) == Some("ro")
        });
        let ro_sql_smoke = ro_sql_smoke.expect("missing sql_smoke step");
        assert_eq!(
            ro_sql_smoke.get("status").and_then(|x| x.as_str()),
            Some("skip")
        );
        assert_eq!(
            ro_sql_smoke
                .get("details")
                .and_then(|d| d.get("reason"))
                .and_then(|x| x.as_str()),
            Some("read_only")
        );

        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/backend/tools/go-live/reports?limit=10")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let arr = v.as_array().cloned().unwrap_or_default();
        assert!(!arr.is_empty());
        assert!(arr[0].get("job_id").is_some());
        assert!(arr[0].get("report_path").is_some());

        let _ = job;
    }

    #[tokio::test]
    async fn schema_sync_diff_returns_error_when_db_id_missing() {
        let app = test_app(test_state());

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/backend/tools/schema-sync/diff")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"source_db_id":"a","target_db_id":"b"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let s = String::from_utf8_lossy(&body);
        assert!(s.contains("Database connection a not found"));
    }

    #[tokio::test]
    async fn mysql_sync_compare_returns_error_when_db_id_missing() {
        let app = test_app(test_state());

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/backend/tools/data-sync/compare")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"source_db_id":"a","target_db_id":"b","table_name":"t","primary_key":"id","mode":"mirror","chunk_size":1000}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let s = String::from_utf8_lossy(&body);
        assert!(s.contains("Database connection a not found"));
    }

    #[tokio::test]
    async fn mysql_sync_job_status_returns_404_when_missing() {
        let app = test_app(test_state());

        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/backend/tools/data-sync/jobs/not-exist")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn perf_sync_start_returns_error_when_db_id_missing() {
        let app = test_app(test_state());

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/backend/tools/perf-sync/start")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"source_db_id":"a","target_db_id":"b"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let s = String::from_utf8_lossy(&body);
        assert!(s.contains("Database connection a not found"));
    }

    #[tokio::test]
    async fn perf_sync_check_returns_error_when_db_id_missing() {
        let app = test_app(test_state());

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/backend/tools/perf-sync/check")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"source_db_id":"a","target_db_id":"b"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let s = String::from_utf8_lossy(&body);
        assert!(s.contains("Database connection a not found"));
    }

    #[tokio::test]
    async fn execute_ddl_rejects_non_ddl_sql() {
        let app = test_app(test_state());

        // SELECT should be rejected by DDL endpoint
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/backend/table/ddl")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"sql":"SELECT * FROM users"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let s = String::from_utf8_lossy(&body);
        assert!(s.contains("DDL statements"));
    }

    #[tokio::test]
    async fn execute_ddl_rejects_insert_statements() {
        let app = test_app(test_state());

        // INSERT should be rejected by DDL endpoint
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/backend/table/ddl")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"sql":"INSERT INTO users (name) VALUES ('hack')"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let s = String::from_utf8_lossy(&body);
        assert!(s.contains("DDL statements"));
    }

    #[tokio::test]
    async fn execute_ddl_rejects_malformed_sql() {
        let app = test_app(test_state());

        // Malformed SQL that sqlparser cannot parse should also be rejected
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/backend/table/ddl")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"sql":"NOT VALID SQL ;;"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let s = String::from_utf8_lossy(&body);
        assert!(s.contains("SQL parsing failed"));
    }

    #[test]
    fn export_job_request_accepts_db_id() {
        let req: ExportJobStartRequest =
            serde_json::from_str(r#"{"table_name":"t","export_type":"csv","db_id":"db-2"}"#)
                .unwrap();
        assert_eq!(req.db_id.as_deref(), Some("db-2"));
    }

    #[test]
    fn export_job_request_db_id_is_optional() {
        let req: ExportJobStartRequest =
            serde_json::from_str(r#"{"table_name":"t","export_type":"csv"}"#).unwrap();
        assert!(req.db_id.is_none());
    }

    #[test]
    fn import_job_request_accepts_db_id_and_defaults_to_none() {
        let req: ImportJobStartRequest = serde_json::from_str(
            r#"{"table_name":"t","data":[],"mapping":{},"skip_errors":false,"db_id":"db-2"}"#,
        )
        .unwrap();
        assert_eq!(req.db_id.as_deref(), Some("db-2"));

        let req: ImportJobStartRequest = serde_json::from_str(
            r#"{"table_name":"t","data":[],"mapping":{},"skip_errors":false}"#,
        )
        .unwrap();
        assert!(req.db_id.is_none());
    }

    #[test]
    fn import_requests_default_to_stop_on_error() {
        // 缺省 skip_errors 时应为遇错即停
        let req: ImportJobStartRequest =
            serde_json::from_str(r#"{"table_name":"t","data":[],"mapping":{}}"#).unwrap();
        assert!(!req.skip_errors);

        let req: ImportSqlJobStartRequest =
            serde_json::from_str(r#"{"sql":"SELECT 1"}"#).unwrap();
        assert!(!req.skip_errors);

        let req: ImportSqlJobStartRequest =
            serde_json::from_str(r#"{"sql":"SELECT 1","skip_errors":true}"#).unwrap();
        assert!(req.skip_errors);
    }

    #[test]
    fn mock_data_request_accepts_db_id_and_defaults_to_none() {
        let req: MockDataRequest =
            serde_json::from_str(r#"{"table_name":"t","row_count":5,"db_id":"db-2"}"#).unwrap();
        assert_eq!(req.db_id.as_deref(), Some("db-2"));

        let req: MockDataRequest =
            serde_json::from_str(r#"{"table_name":"t","row_count":5}"#).unwrap();
        assert!(req.db_id.is_none());
    }

    fn lifecycle_conn(id: &str, url: &str) -> core_lib::config::DbConnection {
        core_lib::config::DbConnection {
            id: id.to_string(),
            name: id.to_string(),
            url: url.to_string(),
            group_name: None,
            color: None,
            is_favorite: false,
            ssh: None,
            ssl: None,
            db_type: core_lib::config::DbType::from_url(url),
            capability_level: None,
            schema: None,
            is_read_only: false,
        }
    }

    #[test]
    fn update_config_keeps_pool_when_only_non_connection_settings_change() {
        let mut prev = AppConfig::default();
        prev.active_db_id = Some("db-1".to_string());
        prev.db_connections = vec![lifecycle_conn("db-1", "mysql://u:p@h/app")];
        let mut next = prev.clone();
        next.ai_profiles = vec![];

        assert!(!save_requires_reconnect(&prev, &next));
    }

    #[test]
    fn update_config_reconnects_when_url_changes() {
        let mut prev = AppConfig::default();
        prev.active_db_id = Some("db-1".to_string());
        prev.db_connections = vec![lifecycle_conn("db-1", "mysql://u:p@h/app")];

        let mut next = prev.clone();
        next.db_connections = vec![lifecycle_conn("db-1", "mysql://u:p@h/other")];

        assert!(save_requires_reconnect(&prev, &next));
        let dial = connection_to_dial_for_save(&prev, &next).unwrap();
        assert_eq!(dial.url, "mysql://u:p@h/other");
    }

    #[test]
    fn update_config_reconnects_when_transport_changes() {
        let mut prev = AppConfig::default();
        prev.active_db_id = Some("db-1".to_string());
        prev.db_connections = vec![lifecycle_conn("db-1", "mysql://u:p@h/app")];

        let mut next = prev.clone();
        let mut conn = lifecycle_conn("db-1", "mysql://u:p@h/app");
        conn.ssl = Some(serde_json::json!({"enabled": true, "mode": "required"}));
        next.db_connections = vec![conn];

        assert!(save_requires_reconnect(&prev, &next));
    }

    #[test]
    fn update_config_disconnect_closes_pool_without_legacy_fallback() {
        let mut prev = AppConfig::default();
        prev.active_db_id = Some("db-1".to_string());
        prev.db_connections = vec![lifecycle_conn("db-1", "mysql://u:p@h/app")];
        prev.db_url = Some("mysql://u:p@h/app".to_string());

        let mut next = prev.clone();
        next.active_db_id = None;

        assert!(save_requires_reconnect(&prev, &next));
        assert!(connection_to_dial_for_save(&prev, &next).is_none());
        assert!(active_connection_for_update(&next).is_none());
    }

    #[test]
    fn update_config_reconnects_for_legacy_db_url_change() {
        let mut prev = AppConfig::default();
        prev.db_url = Some("mysql://u:p@h/app".to_string());

        let mut next = prev.clone();
        next.db_url = Some("mysql://u:p@h/other".to_string());

        assert!(save_requires_reconnect(&prev, &next));
        let dial = connection_to_dial_for_save(&prev, &next).unwrap();
        assert_eq!(dial.url, "mysql://u:p@h/other");
    }

    #[test]
    fn update_config_treats_empty_active_id_as_disconnected_even_with_legacy_url() {
        let config = AppConfig {
            active_db_id: None,
            db_url: Some("mysql://u:p@h/app".to_string()),
            db_connections: vec![lifecycle_conn("db-1", "mysql://u:p@h/app")],
            ..Default::default()
        };

        assert!(active_connection_for_update(&config).is_none());
        assert!(active_connection_fingerprint(&config).is_none());
    }
}
