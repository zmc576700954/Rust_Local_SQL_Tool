#![recursion_limit = "256"]

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, Request, StatusCode},
    middleware::Next,
    response::Response,
    routing::{get, post},
    Json, Router,
};
use core_lib::error::{with_locale, AppError};

use axum::extract::Multipart;
use core_lib::transfer::{TransferConfig, TransferEngine};
use sqlx::{mysql::MySqlRow, Column, Row, TypeInfo};
use std::io::Write;

mod ai_handlers;

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
            let _ = tokio::fs::remove_file(p).await;
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
    skip_errors: bool,
}

#[derive(Serialize)]
struct ImportResponse {
    inserted: usize,
    errors: usize,
    error_details: Vec<String>,
}

async fn import_data(
    State(state): State<AppState>,
    Json(req): Json<ImportRequest>,
) -> Result<Json<ImportResponse>, AppError> {
    let db_client = state
        .db_client
        .read()
        .await
        .clone()
        .ok_or_else(|| AppError::BadRequest("Database not connected".to_string()))?;

    let table_name = req.table_name;
    let mut inserted = 0;
    let mut errors = 0;
    let mut error_details = Vec::new();

    // Filter mapping to only include mapped columns
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

    // Process in batches or row by row. For simplicity and error handling per row, we can do row by row or small batches.
    // If skip_errors is true, we must be able to isolate errors. Row by row is safer for skip_errors.
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
                // If source field is missing in data, bind null
                query = query.bind(None::<String>);
            }
        }

        match query.execute(&db_client.pool).await {
            Ok(_) => {
                inserted += 1;
            }
            Err(e) => {
                errors += 1;
                let err_msg = format!("Row {}: {}", i + 1, e);
                error_details.push(err_msg.clone());
                if !req.skip_errors {
                    return Err(AppError::BadRequest(err_msg));
                }
            }
        }
    }

    Ok(Json(ImportResponse {
        inserted,
        errors,
        error_details,
    }))
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
    let planner = state.planner.read().await.clone();
    let sql_template = match planner.generate_rule_template(&req.prompt, &req.sql).await {
        Ok(res) => res,
        Err(_e) => req.sql.clone(), // Fallback to raw sql if AI extraction fails
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

#[derive(Deserialize)]
struct DbTestRequest {
    host: Option<String>,
    port: Option<u16>,
    username: Option<String>,
    password: Option<String>,
    db_url: Option<String>,
    ssl_mode: Option<String>,
    ssh_enabled: Option<bool>,
    ssh_host: Option<String>,
    ssh_port: Option<u16>,
    ssh_username: Option<String>,
    ssh_password: Option<String>,
    probe_capabilities: Option<bool>,
}

#[derive(Serialize)]
struct DbTestDiagnostic {
    status: String,
    category: String,
    code: String,
    message: String,
    hint: Option<String>,
    detail: Option<String>,
}

#[derive(Serialize)]
struct DbTestResponse {
    success: bool,
    databases: Vec<String>,
    diagnostic: DbTestDiagnostic,
    stage: String,
    capabilities_probed: bool,
    capabilities_ok: Option<bool>,
    server_version: Option<String>,
}

fn db_test_response(
    success: bool,
    databases: Vec<String>,
    diagnostic: DbTestDiagnostic,
    stage: &str,
    capabilities_probed: bool,
    capabilities_ok: Option<bool>,
    server_version: Option<String>,
) -> DbTestResponse {
    DbTestResponse {
        success,
        databases,
        diagnostic,
        stage: stage.to_string(),
        capabilities_probed,
        capabilities_ok,
        server_version,
    }
}

fn db_test_diagnostic(
    status: &str,
    category: &str,
    code: &str,
    message: &str,
    hint: Option<&str>,
    detail: Option<String>,
) -> DbTestDiagnostic {
    DbTestDiagnostic {
        status: status.to_string(),
        category: category.to_string(),
        code: code.to_string(),
        message: message.to_string(),
        hint: hint.map(|v| v.to_string()),
        detail,
    }
}

fn db_test_failed(
    category: &str,
    code: &str,
    message: &str,
    hint: Option<&str>,
    detail: Option<String>,
) -> DbTestResponse {
    db_test_response(
        false,
        vec![],
        db_test_diagnostic("error", category, code, message, hint, detail),
        "handshake",
        false,
        None,
        None,
    )
}

fn classify_db_test_connect_error(msg: &str) -> DbTestResponse {
    let lower = msg.to_lowercase();
    if lower.contains("access denied")
        || lower.contains("authentication failed")
        || lower.contains("using password")
    {
        return db_test_failed(
            "auth",
            "DB_TEST_AUTH_FAILED",
            "数据库账号或密码错误，请检查后重试。",
            Some("请核对用户名、密码及账号来源主机权限。"),
            Some(msg.to_string()),
        );
    }
    if lower.contains("ssl")
        || lower.contains("tls")
        || lower.contains("certificate")
        || lower.contains("handshake")
        || lower.contains("verify")
    {
        return db_test_failed(
            "ssl",
            "DB_TEST_SSL_FAILED",
            "SSL 连接失败，请检查 SSL 模式与证书配置。",
            Some("可先切换为 preferred/disabled 验证是否为证书问题。"),
            Some(msg.to_string()),
        );
    }
    if lower.contains("connection refused")
        || lower.contains("can't connect")
        || lower.contains("could not connect")
        || lower.contains("unknown host")
        || lower.contains("no route to host")
        || lower.contains("timed out")
    {
        return db_test_failed(
            "network",
            "DB_TEST_NETWORK_FAILED",
            "无法连接到数据库服务器，请检查地址/端口/网络后重试。",
            Some("请确认数据库服务已启动、防火墙放行、IP 白名单可访问。"),
            Some(msg.to_string()),
        );
    }
    db_test_failed(
        "unknown",
        "DB_TEST_CONNECT_FAILED",
        "数据库连接失败，请检查连接参数后重试。",
        None,
        Some(msg.to_string()),
    )
}

#[derive(Clone)]
struct SshTunnelConfig {
    ssh_host: String,
    ssh_port: u16,
    ssh_username: String,
    ssh_password: String,
    remote_host: String,
    remote_port: u16,
}

struct SshTunnelHandle {
    local_port: u16,
    stop_tx: std::sync::mpsc::Sender<()>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Drop for SshTunnelHandle {
    fn drop(&mut self) {
        let _ = self.stop_tx.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn classify_ssh_setup_error(msg: &str) -> DbTestResponse {
    let lower = msg.to_lowercase();
    if lower.contains("init timeout") || lower.contains("timed out") {
        return db_test_failed(
            "ssh",
            "DB_TEST_SSH_INIT_TIMEOUT",
            "SSH 隧道初始化超时，请检查 SSH 网络连通性。",
            Some("请检查 SSH 地址、端口、防火墙及网络质量后重试。"),
            Some(msg.to_string()),
        );
    }
    if lower.contains("handshake failed") {
        return db_test_failed(
            "ssh",
            "DB_TEST_SSH_HANDSHAKE_FAILED",
            "SSH 握手失败，请检查 SSH 服务端协议与安全配置。",
            Some("请确认服务端允许当前认证方式，并检查 SSH 服务状态。"),
            Some(msg.to_string()),
        );
    }
    if lower.contains("host key") || lower.contains("fingerprint") || lower.contains("known hosts")
    {
        return db_test_failed(
            "ssh",
            "DB_TEST_SSH_HOSTKEY_FAILED",
            "SSH 主机密钥校验失败，请确认目标主机身份。",
            Some("请核对 SSH 主机指纹，避免连接到错误主机。"),
            Some(msg.to_string()),
        );
    }
    if lower.contains("auth")
        || lower.contains("password")
        || lower.contains("userauth")
        || lower.contains("permission denied")
    {
        return db_test_failed(
            "ssh",
            "DB_TEST_SSH_AUTH_FAILED",
            "SSH 认证失败，请检查 SSH 用户名或密码。",
            Some("请确认 SSH 账号可登录，并校验密码是否正确。"),
            Some(msg.to_string()),
        );
    }
    if lower.contains("timeout")
        || lower.contains("refused")
        || lower.contains("unreachable")
        || lower.contains("could not resolve")
    {
        return db_test_failed(
            "ssh",
            "DB_TEST_SSH_CONNECT_FAILED",
            "SSH 连接失败，请检查 SSH 地址、端口及网络连通性。",
            Some("请确认 SSH 服务已启动，且安全组/防火墙放行对应端口。"),
            Some(msg.to_string()),
        );
    }
    if lower.contains("open ssh channel failed")
        || lower.contains("channel")
        || lower.contains("direct-tcpip")
    {
        return db_test_failed(
            "ssh",
            "DB_TEST_SSH_CHANNEL_FAILED",
            "SSH 隧道通道创建失败，请检查目标数据库地址与端口。",
            Some("请确认 SSH 服务器可访问目标数据库主机和端口。"),
            Some(msg.to_string()),
        );
    }
    db_test_failed(
        "ssh",
        "DB_TEST_SSH_TUNNEL_FAILED",
        "SSH 隧道建立失败，请检查 SSH 配置后重试。",
        None,
        Some(msg.to_string()),
    )
}

fn extract_target_host_port_from_url(db_url: &str) -> Result<(String, u16), String> {
    let parsed = url::Url::parse(db_url).map_err(|e| format!("invalid db_url: {e}"))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| "db_url missing host".to_string())?
        .to_string();
    let port = parsed.port().unwrap_or(3306);
    Ok((host, port))
}

fn rewrite_db_url_with_local_tunnel(db_url: &str, local_port: u16) -> Result<String, String> {
    let mut parsed = url::Url::parse(db_url).map_err(|e| format!("invalid db_url: {e}"))?;
    parsed
        .set_host(Some("127.0.0.1"))
        .map_err(|_| "failed to set tunnel host".to_string())?;
    parsed
        .set_port(Some(local_port))
        .map_err(|_| "failed to set tunnel port".to_string())?;
    Ok(parsed.to_string())
}

fn write_all_to_stream_nonblocking(
    stream: &mut std::net::TcpStream,
    data: &[u8],
) -> Result<(), String> {
    let mut written = 0usize;
    while written < data.len() {
        match std::io::Write::write(stream, &data[written..]) {
            Ok(0) => return Err("local stream closed".to_string()),
            Ok(n) => written += n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            Err(e) => return Err(format!("write local stream failed: {e}")),
        }
    }
    Ok(())
}

fn write_all_to_channel_nonblocking(
    channel: &mut ssh2::Channel,
    data: &[u8],
) -> Result<(), String> {
    let mut written = 0usize;
    while written < data.len() {
        match std::io::Write::write(channel, &data[written..]) {
            Ok(0) => return Err("ssh channel closed".to_string()),
            Ok(n) => written += n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            Err(e) => return Err(format!("write ssh channel failed: {e}")),
        }
    }
    Ok(())
}

fn proxy_one_connection(
    session: &mut ssh2::Session,
    mut local_stream: std::net::TcpStream,
    remote_host: &str,
    remote_port: u16,
    stop_rx: &std::sync::mpsc::Receiver<()>,
) -> Result<(), String> {
    session.set_blocking(false);
    let mut channel = session
        .channel_direct_tcpip(remote_host, remote_port, None)
        .map_err(|e| format!("open ssh channel failed: {e}"))?;
    local_stream
        .set_nonblocking(true)
        .map_err(|e| format!("set local nonblocking failed: {e}"))?;

    let mut uplink_buf = [0u8; 16 * 1024];
    let mut downlink_buf = [0u8; 16 * 1024];

    loop {
        if stop_rx.try_recv().is_ok() {
            break;
        }
        let mut progressed = false;

        match std::io::Read::read(&mut local_stream, &mut uplink_buf) {
            Ok(0) => break,
            Ok(n) => {
                write_all_to_channel_nonblocking(&mut channel, &uplink_buf[..n])?;
                progressed = true;
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(format!("read local stream failed: {e}")),
        }

        match std::io::Read::read(&mut channel, &mut downlink_buf) {
            Ok(0) => break,
            Ok(n) => {
                write_all_to_stream_nonblocking(&mut local_stream, &downlink_buf[..n])?;
                progressed = true;
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(format!("read ssh channel failed: {e}")),
        }

        if !progressed {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    let _ = channel.close();
    let _ = channel.wait_close();
    Ok(())
}

fn start_ssh_tunnel(cfg: SshTunnelConfig) -> Result<SshTunnelHandle, String> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
        .map_err(|e| format!("bind local tunnel failed: {e}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("set listener nonblocking failed: {e}"))?;
    let local_port = listener
        .local_addr()
        .map_err(|e| format!("read local tunnel addr failed: {e}"))?
        .port();

    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();

    let worker = std::thread::spawn(move || {
        let ssh_tcp = match std::net::TcpStream::connect((cfg.ssh_host.as_str(), cfg.ssh_port)) {
            Ok(v) => v,
            Err(e) => {
                let _ = ready_tx.send(Err(format!("ssh tcp connect failed: {e}")));
                return;
            }
        };
        let _ = ssh_tcp.set_read_timeout(Some(std::time::Duration::from_secs(10)));
        let _ = ssh_tcp.set_write_timeout(Some(std::time::Duration::from_secs(10)));

        let mut session = match ssh2::Session::new() {
            Ok(v) => v,
            Err(e) => {
                let _ = ready_tx.send(Err(format!("create ssh session failed: {e}")));
                return;
            }
        };
        session.set_tcp_stream(ssh_tcp);
        if let Err(e) = session.handshake() {
            let _ = ready_tx.send(Err(format!("ssh handshake failed: {e}")));
            return;
        }
        if let Err(e) = session.userauth_password(&cfg.ssh_username, &cfg.ssh_password) {
            let _ = ready_tx.send(Err(format!("ssh auth failed: {e}")));
            return;
        }
        if !session.authenticated() {
            let _ = ready_tx.send(Err("ssh auth failed: unauthenticated".to_string()));
            return;
        }
        let _ = ready_tx.send(Ok(()));

        loop {
            if stop_rx.try_recv().is_ok() {
                break;
            }
            match listener.accept() {
                Ok((local_stream, _)) => {
                    let _ = proxy_one_connection(
                        &mut session,
                        local_stream,
                        &cfg.remote_host,
                        cfg.remote_port,
                        &stop_rx,
                    );
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    });

    let ready = ready_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .map_err(|_| "ssh tunnel init timeout".to_string())?;
    ready?;

    Ok(SshTunnelHandle {
        local_port,
        stop_tx,
        worker: Some(worker),
    })
}

async fn db_test(Json(req): Json<DbTestRequest>) -> Result<Json<DbTestResponse>, AppError> {
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
use core_lib::timeout_policy::TimeoutPolicy;
use core_lib::{
    ai::{
        gateway::{AiError, AiGateway},
        planner::Planner,
        policy_store::{Policy, PolicyStore},
    },
    config::{AppConfig, DbType},
    crud::{CrudManager, CrudRequest},
    db::DbClient,
    knowledge_base::KnowledgeBase,
    mysql_sync::{CompareResult, MySqlDataSyncEngine, PreviewResult, SyncMode},
    navicat::{NavicatConnection, NavicatParser},
    offline_parser::OfflineParser,
    perf_report::{summarize_perf_samples, PerfBudget, PerfProbeSummary, PerfSample},
    rule_engine::{Rule, RuleStore, RuleType},
    schema::{SchemaExtractor, SchemaResponse, TableWithDetails},
    sql_history::{SqlHistory, SqlHistoryStore},
    tools::{DataExporter, DdlEngine, MockDataGenerator, SyncEngine},
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};
use tokio::{
    io::AsyncWriteExt,
    sync::{Mutex, RwLock, Semaphore},
};
use tower_http::{
    cors::CorsLayer,
    services::{ServeDir, ServeFile},
};

const SCHEMA_CACHE_TTL: Duration = Duration::from_secs(30);
const TABLE_SCHEMA_CACHE_TTL: Duration = Duration::from_secs(300);
const DB_CLIENT_CACHE_TTL: Duration = Duration::from_secs(600);
const PERF_PROBE_MAX_ITERATIONS: u32 = 30;
const PERF_SUITE_ARCHIVE_DEFAULT_LIMIT: usize = 10;

#[derive(Debug, Clone)]
struct CachedDbClient {
    client: DbClient,
    db_name: String,
    url: String,
    expires_at: Instant,
}

#[derive(Debug, Clone)]
struct CachedSchemaEntry {
    schema: SchemaResponse,
    expires_at: Instant,
}

#[derive(Debug, Clone)]
struct CachedTableSchemaEntry {
    table: TableWithDetails,
    expires_at: Instant,
}

#[derive(Clone)]
struct ActiveQueryHandle {
    db_client: DbClient,
    connection_id: u64,
    canceled: Arc<AtomicBool>,
}

struct ActiveQuerySession {
    token: String,
    connection_id: u64,
    canceled: Arc<AtomicBool>,
    owned_conn: Option<sqlx::pool::PoolConnection<sqlx::MySql>>,
    transaction_session: Option<SharedTransactionSession>,
}

struct TransactionSession {
    connection_id: u64,
    db_id: Option<String>,
    conn: sqlx::pool::PoolConnection<sqlx::MySql>,
}

type SharedTransactionSession = Arc<Mutex<TransactionSession>>;

#[derive(Debug, Clone)]
struct RuntimeLimits {
    temp_dir: String,
    temp_quota_bytes: u64,
    max_file_bytes: u64,
    max_job_concurrency: usize,
}

impl Default for RuntimeLimits {
    fn default() -> Self {
        let temp_dir = std::env::var("LOCAL_AI_SQL_TEMP_DIR")
            .ok()
            .unwrap_or_else(|| "/tmp/local-ai-sql".to_string());
        let temp_quota_bytes = std::env::var("LOCAL_AI_SQL_TEMP_QUOTA_BYTES")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(2 * 1024 * 1024 * 1024);
        let max_file_bytes = std::env::var("LOCAL_AI_SQL_MAX_FILE_BYTES")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(500 * 1024 * 1024);
        let max_job_concurrency = std::env::var("LOCAL_AI_SQL_MAX_JOB_CONCURRENCY")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(4);
        Self {
            temp_dir,
            temp_quota_bytes,
            max_file_bytes,
            max_job_concurrency: max_job_concurrency.max(1),
        }
    }
}

fn dir_size_bytes_sync(path: &std::path::Path) -> u64 {
    let mut total = 0u64;
    let Ok(rd) = std::fs::read_dir(path) else {
        return 0;
    };
    for entry in rd.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_file() {
            total = total.saturating_add(meta.len());
        } else if meta.is_dir() {
            total = total.saturating_add(dir_size_bytes_sync(&entry.path()));
        }
    }
    total
}

#[cfg(test)]
fn test_state_with_config(config: AppConfig) -> AppState {
    let gateway = AiGateway::new(config.clone());
    let planner = Planner::new(gateway);
    AppState {
        config: Arc::new(RwLock::new(config)),
        db_client: Arc::new(RwLock::new(None)),
        db_client_cache: Arc::new(RwLock::new(HashMap::new())),
        planner: Arc::new(RwLock::new(planner)),
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
        limits: RuntimeLimits::default(),
        job_semaphore: Arc::new(Semaphore::new(1)),
    }
}

async fn dir_size_bytes(path: &std::path::Path) -> Result<u64, AppError> {
    let p = path.to_path_buf();
    tokio::task::spawn_blocking(move || dir_size_bytes_sync(&p))
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))
}

async fn ensure_temp_quota(limits: &RuntimeLimits, additional_bytes: u64) -> Result<(), AppError> {
    let dir = std::path::Path::new(&limits.temp_dir);
    if !dir.exists() {
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|e| AppError::InternalError(e.to_string()))?;
    }
    let used = dir_size_bytes(dir).await?;
    if used.saturating_add(additional_bytes) > limits.temp_quota_bytes {
        return Err(AppError::ResourceLimit(format!(
            "temp quota exceeded: used={}B, additional={}B, quota={}B, dir={}",
            used, additional_bytes, limits.temp_quota_bytes, limits.temp_dir
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MySqlSyncStage {
    Compare,
    Preview,
    Deploy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MySqlSyncJobStatus {
    Pending,
    Running,
    Completed,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct MySqlSyncProgress {
    current: u64,
    total: u64,
    message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DeployResult {
    affected_rows: u64,
    statements: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MySqlSyncJob {
    job_id: String,
    stage: MySqlSyncStage,
    status: MySqlSyncJobStatus,
    progress: MySqlSyncProgress,
    source_db_id: String,
    target_db_id: String,
    table_name: String,
    primary_key: String,
    mode: SyncMode,
    chunk_size: usize,
    created_at: i64,
    updated_at: i64,
    compare_ms: Option<u128>,
    preview_ms: Option<u128>,
    deploy_ms: Option<u128>,
    compare: Option<CompareResult>,
    preview: Option<PreviewResult>,
    deploy: Option<DeployResult>,
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PerfSyncStage {
    Prepare,
    DetectBaseline,
    InjectMirror,
    Mirror,
    VerifyMirror,
    InjectUpsertOnly,
    UpsertOnly,
    VerifyUpsertOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PerfSyncJobStatus {
    Pending,
    Running,
    Completed,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct PerfSyncProgress {
    current: u64,
    total: u64,
    message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PerfSyncTableSpec {
    table_name: String,
    primary_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PerfSyncLoadgenRequest {
    tier: Option<String>,
    fill: Option<bool>,
    reset: Option<bool>,
    inject: Option<bool>,
    seed: Option<u64>,
    batch: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PerfSyncStartRequest {
    source_db_id: String,
    target_db_id: String,
    tier: Option<String>,
    tables: Option<Vec<PerfSyncTableSpec>>,
    chunk_size: Option<usize>,
    max_rows: Option<usize>,
    loadgen: Option<PerfSyncLoadgenRequest>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PerfSyncTableCount {
    source: u64,
    target: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PerfSyncTableSyncReport {
    table_name: String,
    primary_key: String,
    compare_ms: u128,
    preview_ms: u128,
    deploy_ms: u128,
    compare_chunks: usize,
    different_chunks: usize,
    insert_count: usize,
    update_count: usize,
    delete_count: usize,
    statements: usize,
    truncated: bool,
    affected_rows: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PerfSyncTableVerifyReport {
    table_name: String,
    different_chunks: usize,
    chunks: usize,
    verify_ms: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PerfSyncModeReport {
    mode: SyncMode,
    injected_counts: HashMap<String, PerfSyncTableCount>,
    tables: Vec<PerfSyncTableSyncReport>,
    verify: Vec<PerfSyncTableVerifyReport>,
    passed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PerfSyncReport {
    baseline_counts: HashMap<String, PerfSyncTableCount>,
    loadgen: Option<core_lib::loadgen::LoadgenReport>,
    mirror: PerfSyncModeReport,
    upsert_only: PerfSyncModeReport,
    stage_ms: HashMap<String, u128>,
    elapsed_ms: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PerfSyncJob {
    job_id: String,
    stage: PerfSyncStage,
    status: PerfSyncJobStatus,
    progress: PerfSyncProgress,
    request: PerfSyncStartRequest,
    created_at: i64,
    updated_at: i64,
    report: Option<PerfSyncReport>,
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ToolJobKind {
    Export,
    Import,
    ImportSql,
    GoLive,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ToolJobStatus {
    Pending,
    Running,
    Completed,
    Error,
    Canceled,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ToolJobProgress {
    current: u64,
    total: Option<u64>,
    message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ToolJobArtifacts {
    data_path: Option<String>,
    manifest_path: Option<String>,
    file_name: Option<String>,
    content_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ToolJob {
    job_id: String,
    kind: ToolJobKind,
    status: ToolJobStatus,
    progress: ToolJobProgress,
    created_at: i64,
    updated_at: i64,
    elapsed_ms: Option<u128>,
    artifacts: Option<ToolJobArtifacts>,
    result: Option<serde_json::Value>,
    error: Option<String>,
}

#[derive(Clone)]
struct AppState {
    config: Arc<RwLock<AppConfig>>,
    db_client: Arc<RwLock<Option<DbClient>>>,
    db_client_cache: Arc<RwLock<HashMap<String, CachedDbClient>>>,
    planner: Arc<RwLock<Planner>>,
    virtual_schema: Arc<RwLock<Option<SchemaResponse>>>,
    schema_cache: Arc<RwLock<HashMap<String, CachedSchemaEntry>>>,
    table_schema_cache: Arc<RwLock<HashMap<String, CachedTableSchemaEntry>>>,
    rule_store: Arc<RwLock<RuleStore>>,
    policy: Arc<RwLock<Policy>>,
    sql_history: Arc<RwLock<SqlHistoryStore>>,
    knowledge_base: Arc<RwLock<KnowledgeBase>>,
    sync_jobs: Arc<RwLock<HashMap<String, MySqlSyncJob>>>,
    perf_sync_jobs: Arc<RwLock<HashMap<String, PerfSyncJob>>>,
    active_queries: Arc<RwLock<HashMap<String, ActiveQueryHandle>>>,
    transaction_sessions: Arc<RwLock<HashMap<String, SharedTransactionSession>>>,
    tool_jobs: Arc<RwLock<HashMap<String, ToolJob>>>,
    tool_job_handles: Arc<RwLock<HashMap<String, tokio::task::AbortHandle>>>,
    timeouts: TimeoutPolicy,
    limits: RuntimeLimits,
    job_semaphore: Arc<Semaphore>,
}

async fn register_active_query(state: &AppState, token: String, handle: ActiveQueryHandle) {
    state.active_queries.write().await.insert(token, handle);
}

async fn unregister_active_query(state: &AppState, token: &str) {
    state.active_queries.write().await.remove(token);
}

async fn cancel_active_query(state: &AppState, cancel_token: &str) -> Result<bool, AppError> {
    let handle = state.active_queries.read().await.get(cancel_token).cloned();
    let Some(handle) = handle else {
        return Ok(false);
    };

    match handle.db_client.kill_query(handle.connection_id).await {
        Ok(_) => {
            handle.canceled.store(true, Ordering::SeqCst);
            Ok(true)
        }
        Err(e) => {
            let message = e.to_string().to_lowercase();
            if message.contains("unknown thread id") {
                Ok(false)
            } else {
                Err(AppError::InternalError(e.to_string()))
            }
        }
    }
}

async fn resolve_transaction_db_id(
    state: &AppState,
    db_id: Option<&str>,
) -> Option<String> {
    if let Some(value) = db_id.map(str::trim).filter(|value| !value.is_empty()) {
        return Some(value.to_string());
    }
    state.config.read().await.active_db_id.clone()
}

async fn get_or_open_transaction_session(
    state: &AppState,
    db_id: Option<&str>,
    transaction_id: &str,
) -> Result<SharedTransactionSession, AppError> {
    if let Some(existing) = state
        .transaction_sessions
        .read()
        .await
        .get(transaction_id)
        .cloned()
    {
        let expected_db_id = resolve_transaction_db_id(state, db_id).await;
        let session_db_id = existing.lock().await.db_id.clone();
        if session_db_id != expected_db_id {
            return Err(AppError::BadRequest(
                "Transaction session is bound to a different database connection".to_string(),
            ));
        }
        return Ok(existing);
    }

    let resolved_db_id = resolve_transaction_db_id(state, db_id).await;
    let (db_client, _) = resolve_db_client_for_request(state, db_id).await?;
    let mut conn = db_client
        .pool
        .acquire()
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    let connection_id = DbClient::connection_id_for_session(&mut conn)
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    tokio::time::timeout(
        state.timeouts.db_query,
        sqlx::query("START TRANSACTION").execute(&mut *conn),
    )
    .await
    .map_err(|_| AppError::Timeout("Starting transaction timed out".to_string()))?
    .map_err(|e| AppError::InternalError(e.to_string()))?;

    let session = Arc::new(Mutex::new(TransactionSession {
        connection_id,
        db_id: resolved_db_id,
        conn,
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

    // Initialize DB Client if configured
    let mut db_client = None;
    if let Some(ref url) = config.get_active_db_url() {
        match DbClient::new(url).await {
            Ok(client) => {
                tracing::info!("Connected to database");
                db_client = Some(client);
            }
            Err(e) => tracing::error!("Failed to connect to database: {}", e),
        }
    }

    let gateway = AiGateway::new(config.clone());
    let planner = Planner::new(gateway);
    let rule_store = RuleStore::load().await.unwrap_or_default();
    let policy = PolicyStore::load_effective()
        .await
        .unwrap_or_else(|_| Policy::default());
    let sql_history = SqlHistoryStore::load().await.unwrap_or_default();
    let knowledge_base = KnowledgeBase::load().await.unwrap_or_default();

    let state = AppState {
        config: Arc::new(RwLock::new(config)),
        db_client: Arc::new(RwLock::new(db_client)),
        db_client_cache: Arc::new(RwLock::new(HashMap::new())),
        planner: Arc::new(RwLock::new(planner)),
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
    };

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
        .route("/tools/mysql-sync/compare", post(mysql_sync_compare))
        .route("/tools/mysql-sync/preview", post(mysql_sync_preview))
        .route("/tools/mysql-sync/deploy", post(mysql_sync_deploy))
        .route("/tools/mysql-sync/jobs/:job_id", get(mysql_sync_job_status))
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

    let app = Router::new()
        .nest("/backend", api)
        .fallback_service(static_service)
        .layer(CorsLayer::permissive())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await?;
    tracing::info!("Server listening on http://0.0.0.0:3000");
    axum::serve(listener, app).await?;

    Ok(())
}

fn map_ai_error(e: AiError) -> AppError {
    match e {
        AiError::Auth(msg) => AppError::AiAuth(msg),
        AiError::Forbidden(msg) => AppError::AiForbidden(msg),
        AiError::ModelNotFound(msg) => AppError::AiModelNotFound(msg),
        AiError::NoTokens => AppError::BadRequest("No tokens available in pool".to_string()),
        AiError::RateLimited(msg) => AppError::AiRateLimited(msg),
        AiError::ServerError(msg) => AppError::ExternalServiceUnavailable(msg),
        AiError::Network(e) => {
            if e.is_timeout() {
                AppError::AiAgentTimeout(e.to_string())
            } else if e.to_string().to_lowercase().contains("proxy")
                || e.to_string().to_lowercase().contains("tunnel")
            {
                AppError::AiProxy(e.to_string())
            } else if e.is_connect() {
                AppError::ExternalServiceUnavailable(e.to_string())
            } else {
                AppError::AiAgentTimeout(e.to_string())
            }
        }
        AiError::ApiError(msg) => AppError::InternalError(msg),
    }
}

// ----------------- CRUD API Handlers -----------------

#[derive(Deserialize)]
struct CrudMutationRequest {
    table_name: String,
    data: serde_json::Value,
    condition: Option<serde_json::Map<String, serde_json::Value>>,
    db_id: Option<String>,
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

    let crud_req = CrudRequest {
        table_name: req.table_name,
        data: req.data,
        condition: req.condition,
    };

    let affected_rows = CrudManager::insert(&db_client, &crud_req)
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;

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

    let crud_req = CrudRequest {
        table_name: req.table_name,
        data: req.data,
        condition: req.condition,
    };

    let affected_rows = CrudManager::update(&db_client, &crud_req)
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;

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
    }))
}

#[derive(Deserialize)]
struct DeleteRequest {
    table_name: String,
    condition: serde_json::Map<String, serde_json::Value>,
    db_id: Option<String>,
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

    let affected_rows = CrudManager::delete(&db_client, &req.table_name, &req.condition)
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;

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

async fn update_config(
    State(state): State<AppState>,
    Json(new_config): Json<AppConfig>,
) -> Result<Json<serde_json::Value>, AppError> {
    let prev_config = state.config.read().await.clone();
    let mut new_config = new_config.normalize();
    new_config.merge_secrets_from(&prev_config);
    // Save to file
    new_config
        .save()
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;

    // Update in-memory state
    {
        let mut config_write = state.config.write().await;
        *config_write = new_config.clone();
    }
    state.db_client_cache.write().await.clear();
    clear_metadata_caches(&state).await;

    // Re-init DB if url changed
    if let Some(ref url) = new_config.get_active_db_url() {
        match DbClient::new(url).await {
            Ok(client) => {
                if let Some(old) = state.db_client.write().await.take() {
                    old.pool.close().await;
                }
                let mut db_write = state.db_client.write().await;
                *db_write = Some(client);
            }
            Err(e) => return Err(AppError::BadRequest(format!("DB connection failed: {}", e))),
        }
    }

    // Update Planner's gateway config
    // Actually we need mutability on Planner.
    // For now, recreate Planner and Gateway
    let gateway = AiGateway::new(new_config.clone());
    let mut planner_write = state.planner.write().await;
    *planner_write = Planner::new(gateway);

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

async fn fetch_schema_for_db(db_client: &DbClient, db_name: &str) -> Option<SchemaResponse> {
    let tables = SchemaExtractor::get_tables(db_client, db_name).await.ok()?;
    let columns_map = SchemaExtractor::get_columns_map(db_client, db_name)
        .await
        .unwrap_or_default();
    let indexes_map = SchemaExtractor::get_indexes_map(db_client, db_name)
        .await
        .unwrap_or_default();
    let foreign_keys_map = SchemaExtractor::get_foreign_keys_map(db_client, db_name)
        .await
        .unwrap_or_default();

    let mut result_tables = Vec::with_capacity(tables.len());
    for t in tables {
        let table_name = t.table_name;
        let columns = columns_map.get(&table_name).cloned().unwrap_or_default();
        let indexes = indexes_map.get(&table_name).cloned().unwrap_or_default();
        let foreign_keys = foreign_keys_map
            .get(&table_name)
            .cloned()
            .unwrap_or_default();

        result_tables.push(TableWithDetails {
            table_name,
            columns,
            indexes,
            foreign_keys,
        });
    }

    let views = SchemaExtractor::get_views(db_client, db_name)
        .await
        .unwrap_or_default();

    Some(SchemaResponse {
        db_name: db_name.to_string(),
        tables: result_tables,
        views,
    })
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

async fn get_cached_schema(
    state: &AppState,
    db_id: Option<&str>,
    db_client: &DbClient,
    db_name: &str,
) -> Option<SchemaResponse> {
    let key = schema_cache_key(db_id, db_name);
    if let Some(entry) = state.schema_cache.read().await.get(&key).cloned() {
        if entry.expires_at > Instant::now() {
            return Some(entry.schema);
        }
    }

    let schema = fetch_schema_for_db(db_client, db_name).await?;
    state.schema_cache.write().await.insert(
        key,
        CachedSchemaEntry {
            schema: schema.clone(),
            expires_at: Instant::now() + SCHEMA_CACHE_TTL,
        },
    );
    Some(schema)
}

async fn get_cached_table_schema(
    state: &AppState,
    db_id: Option<&str>,
    db_client: &DbClient,
    db_name: &str,
    table_name: &str,
) -> Result<TableWithDetails, AppError> {
    let key = table_schema_cache_key(db_id, db_name, table_name);
    if let Some(entry) = state.table_schema_cache.read().await.get(&key).cloned() {
        if entry.expires_at > Instant::now() {
            return Ok(entry.table);
        }
    }

    let columns = SchemaExtractor::get_columns(db_client, db_name, table_name)
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    let indexes = SchemaExtractor::get_indexes(db_client, db_name, table_name)
        .await
        .unwrap_or_default();
    let foreign_keys = SchemaExtractor::get_foreign_keys(db_client, db_name, table_name)
        .await
        .unwrap_or_default();
    let table = TableWithDetails {
        table_name: table_name.to_string(),
        columns,
        indexes,
        foreign_keys,
    };

    state.table_schema_cache.write().await.insert(
        key,
        CachedTableSchemaEntry {
            table: table.clone(),
            expires_at: Instant::now() + TABLE_SCHEMA_CACHE_TTL,
        },
    );
    Ok(table)
}

async fn clear_metadata_caches(state: &AppState) {
    state.schema_cache.write().await.clear();
    state.table_schema_cache.write().await.clear();
}

async fn get_schema_internal(state: &AppState) -> Option<SchemaResponse> {
    if let Some(vs) = state.virtual_schema.read().await.clone() {
        return Some(vs);
    }

    let db_client = state.db_client.read().await.clone()?;
    let url = state
        .config
        .read()
        .await
        .get_active_db_url()
        .unwrap_or_default();
    let db_name = DbClient::extract_db_name(&url).unwrap_or_default();

    get_cached_schema(state, None, &db_client, &db_name).await
}

async fn get_schema_for_db_id(state: &AppState, db_id: &str) -> Result<SchemaResponse, AppError> {
    let (db_client, db_name) = get_temp_db_client(state, db_id).await?;
    get_cached_schema(state, Some(db_id), &db_client, &db_name)
        .await
        .ok_or_else(|| AppError::InternalError("Failed to fetch schema".to_string()))
}

async fn resolve_db_client_for_request(
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

async fn is_read_only_connection(state: &AppState, db_id: Option<&str>) -> bool {
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

const QUERY_PREVIEW_CHUNK_SIZE: u32 = 200;
const QUERY_PREVIEW_ROW_CAP: u32 = 1000;

#[derive(Deserialize)]
struct ExecuteRequest {
    sql: String,
    force: Option<bool>,
    db_id: Option<String>,
    chunk_offset: Option<u32>,
    chunk_size: Option<u32>,
    cancel_token: Option<String>,
    transaction_id: Option<String>,
}

#[derive(Serialize)]
struct ExecuteResponse {
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
struct ExecuteCancelRequest {
    cancel_token: String,
}

#[derive(Serialize)]
struct ExecuteCancelResponse {
    canceled: bool,
}

#[derive(Deserialize)]
struct ExecuteTransactionRequest {
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
    if action != "commit" && action != "rollback" {
        return Err(AppError::BadRequest(
            "transaction action must be commit or rollback".to_string(),
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
        tokio::time::timeout(state.timeouts.db_query, sqlx::query(sql).execute(&mut *guard.conn))
            .await
            .map_err(|_| AppError::Timeout(format!("{action} timed out")))?
            .map_err(|e| AppError::InternalError(e.to_string()))?;
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

#[derive(Clone, Copy)]
enum MySqlJsonDecodeStrategy {
    I64,
    F64,
    DateTime,
    Date,
    Time,
    String,
    Bytes,
    Unknown,
}

#[derive(Clone)]
struct MySqlRowJsonEncoder {
    columns: Vec<(String, usize, MySqlJsonDecodeStrategy)>,
}

impl MySqlRowJsonEncoder {
    fn from_row(row: &MySqlRow) -> Self {
        let columns = row
            .columns()
            .iter()
            .map(|col| {
                (
                    col.name().to_string(),
                    col.ordinal(),
                    mysql_json_decode_strategy(col.type_info().name()),
                )
            })
            .collect();
        Self { columns }
    }

    fn column_names(&self) -> Vec<String> {
        self.columns
            .iter()
            .map(|(name, _, _)| name.clone())
            .collect()
    }
}

fn mysql_json_decode_strategy(type_name: &str) -> MySqlJsonDecodeStrategy {
    match type_name.to_ascii_lowercase().as_str() {
        "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint" | "year" => {
            MySqlJsonDecodeStrategy::I64
        }
        "float" | "double" | "decimal" | "numeric" | "real" => MySqlJsonDecodeStrategy::F64,
        "datetime" | "timestamp" => MySqlJsonDecodeStrategy::DateTime,
        "date" => MySqlJsonDecodeStrategy::Date,
        "time" => MySqlJsonDecodeStrategy::Time,
        "char" | "varchar" | "tinytext" | "text" | "mediumtext" | "longtext" | "enum" | "set"
        | "json" => MySqlJsonDecodeStrategy::String,
        "binary" | "varbinary" | "tinyblob" | "blob" | "mediumblob" | "longblob" | "bit" => {
            MySqlJsonDecodeStrategy::Bytes
        }
        _ => MySqlJsonDecodeStrategy::Unknown,
    }
}

fn fallback_mysql_json_value(row: &MySqlRow, ordinal: usize) -> serde_json::Value {
    if let Ok(val) = row.try_get::<Option<i64>, _>(ordinal) {
        serde_json::json!(val)
    } else if let Ok(val) = row.try_get::<Option<f64>, _>(ordinal) {
        serde_json::json!(val)
    } else if let Ok(val) = row.try_get::<Option<bool>, _>(ordinal) {
        serde_json::json!(val)
    } else if let Ok(val) = row.try_get::<Option<chrono::NaiveDateTime>, _>(ordinal) {
        serde_json::json!(val.map(|dt| dt.to_string()))
    } else if let Ok(val) = row.try_get::<Option<chrono::NaiveDate>, _>(ordinal) {
        serde_json::json!(val.map(|d| d.to_string()))
    } else if let Ok(val) = row.try_get::<Option<chrono::NaiveTime>, _>(ordinal) {
        serde_json::json!(val.map(|t| t.to_string()))
    } else if let Ok(val) = row.try_get::<Option<String>, _>(ordinal) {
        serde_json::json!(val)
    } else if let Ok(val) = row.try_get::<Option<Vec<u8>>, _>(ordinal) {
        serde_json::json!(val.map(|bytes| String::from_utf8_lossy(&bytes).into_owned()))
    } else {
        serde_json::Value::Null
    }
}

fn mysql_json_value_by_strategy(
    row: &MySqlRow,
    ordinal: usize,
    strategy: MySqlJsonDecodeStrategy,
) -> serde_json::Value {
    let encoded = match strategy {
        MySqlJsonDecodeStrategy::I64 => row
            .try_get::<Option<i64>, _>(ordinal)
            .ok()
            .map(|val| serde_json::json!(val)),
        MySqlJsonDecodeStrategy::F64 => row
            .try_get::<Option<f64>, _>(ordinal)
            .ok()
            .map(|val| serde_json::json!(val)),
        MySqlJsonDecodeStrategy::DateTime => row
            .try_get::<Option<chrono::NaiveDateTime>, _>(ordinal)
            .ok()
            .map(|val| serde_json::json!(val.map(|dt| dt.to_string()))),
        MySqlJsonDecodeStrategy::Date => row
            .try_get::<Option<chrono::NaiveDate>, _>(ordinal)
            .ok()
            .map(|val| serde_json::json!(val.map(|d| d.to_string()))),
        MySqlJsonDecodeStrategy::Time => row
            .try_get::<Option<chrono::NaiveTime>, _>(ordinal)
            .ok()
            .map(|val| serde_json::json!(val.map(|t| t.to_string()))),
        MySqlJsonDecodeStrategy::String => row
            .try_get::<Option<String>, _>(ordinal)
            .ok()
            .map(|val| serde_json::json!(val)),
        MySqlJsonDecodeStrategy::Bytes => {
            row.try_get::<Option<Vec<u8>>, _>(ordinal).ok().map(|val| {
                serde_json::json!(val.map(|bytes| String::from_utf8_lossy(&bytes).into_owned()))
            })
        }
        MySqlJsonDecodeStrategy::Unknown => None,
    };

    encoded.unwrap_or_else(|| fallback_mysql_json_value(row, ordinal))
}

fn encode_mysql_row(row: &MySqlRow, encoder: &MySqlRowJsonEncoder) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for (col_name, ordinal, strategy) in &encoder.columns {
        map.insert(
            col_name.clone(),
            mysql_json_value_by_strategy(row, *ordinal, *strategy),
        );
    }
    serde_json::Value::Object(map)
}

fn quote_mysql_ident(raw: &str) -> Result<String, AppError> {
    let s = raw.trim();
    if s.is_empty() {
        return Err(AppError::BadRequest("Invalid identifier".to_string()));
    }
    if s.len() > 512 {
        return Err(AppError::BadRequest("Identifier too long".to_string()));
    }
    Ok(format!("`{}`", s.replace('`', "``")))
}

#[derive(Debug, Deserialize)]
struct PerfProbeRequest {
    operation: Option<String>,
    db_id: Option<String>,
    sql: Option<String>,
    table_name: Option<String>,
    iterations: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PerfSuiteHistoryRecord {
    id: String,
    recorded_at: String,
    connection_id: Option<String>,
    connection_name: Option<String>,
    operation: String,
    iterations: u32,
    sql: Option<String>,
    table_name: Option<String>,
    result: PerfProbeSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PerfSuiteArchiveRecord {
    id: String,
    recorded_at: String,
    connection_id: Option<String>,
    connection_name: Option<String>,
    label: Option<String>,
    build_version: Option<String>,
    branch_name: Option<String>,
    environment: Option<String>,
    notes: Option<String>,
    iterations: u32,
    sql: Option<String>,
    table_name: Option<String>,
    status: String,
    failed_operation: Option<String>,
    error: Option<String>,
    results: Vec<PerfSuiteHistoryRecord>,
    #[serde(default)]
    archive_path: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PerfSuiteBaselinePinRequest {
    suite_id: String,
}

#[derive(Debug, Deserialize)]
struct PerfSuiteDiffListQuery {
    limit: Option<usize>,
    current_suite_id: Option<String>,
    baseline_suite_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PerfSuiteDiffArchiveRecord {
    id: String,
    recorded_at: String,
    current_suite_id: String,
    baseline_suite_id: String,
    current_suite_label: Option<String>,
    baseline_suite_label: Option<String>,
    gate_status: Option<String>,
    baseline_scope: Option<String>,
    current_suite: serde_json::Value,
    baseline_suite: serde_json::Value,
    gate: serde_json::Value,
    summary: serde_json::Value,
    rows: Vec<serde_json::Value>,
    #[serde(default)]
    archive_path: Option<String>,
}

fn normalize_perf_probe_sql(raw: Option<&str>) -> Result<String, AppError> {
    let sql = raw.unwrap_or("SELECT 1 AS perf_probe").trim().to_string();
    if sql.is_empty() {
        return Err(AppError::BadRequest(
            "Perf probe SQL cannot be empty".to_string(),
        ));
    }

    let clean_sql = strip_leading_perf_probe_sql_comments(&sql);
    let upper_sql = clean_sql.to_uppercase();
    let is_read_only = upper_sql.starts_with("SELECT")
        || upper_sql.starts_with("SHOW")
        || upper_sql.starts_with("DESCRIBE")
        || upper_sql.starts_with("EXPLAIN");
    if !is_read_only {
        return Err(AppError::BadRequest(
            "Perf probe only supports read-only SQL".to_string(),
        ));
    }

    Ok(sql.trim_end_matches(';').to_string())
}

fn strip_leading_perf_probe_sql_comments(sql: &str) -> String {
    let mut clean_sql = sql.trim().to_string();
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
            return clean_sql;
        }
    }
}

fn build_perf_probe_explain_sql(raw: Option<&str>) -> Result<String, AppError> {
    let sql = normalize_perf_probe_sql(raw)?;
    let clean_sql = strip_leading_perf_probe_sql_comments(&sql);
    if clean_sql.to_uppercase().starts_with("EXPLAIN") {
        return Ok(sql);
    }
    Ok(format!("EXPLAIN {sql}"))
}

fn normalize_perf_probe_iterations(raw: Option<u32>) -> u32 {
    raw.unwrap_or(5).clamp(1, PERF_PROBE_MAX_ITERATIONS)
}

fn normalize_perf_probe_table_name(raw: Option<&str>) -> Result<String, AppError> {
    let table_name = raw.unwrap_or("").trim();
    if table_name.is_empty() {
        return Err(AppError::BadRequest(
            "Perf probe table_name is required".to_string(),
        ));
    }
    Ok(table_name.to_string())
}

fn default_perf_probe_budget(operation: &str) -> Option<PerfBudget> {
    match operation {
        "connect_warm" => Some(PerfBudget {
            operation: operation.to_string(),
            target_p50_ms: Some(50),
            target_p95_ms: Some(120),
            source: Some("phase1_local_warm_connect_target".to_string()),
        }),
        "query_select_small" => Some(PerfBudget {
            operation: operation.to_string(),
            target_p50_ms: Some(80),
            target_p95_ms: Some(150),
            source: Some("phase1_web_query_target".to_string()),
        }),
        "catalog_first_paint" => Some(PerfBudget {
            operation: operation.to_string(),
            target_p50_ms: Some(400),
            target_p95_ms: Some(700),
            source: Some("phase1_web_catalog_target".to_string()),
        }),
        "table_first_page" => Some(PerfBudget {
            operation: operation.to_string(),
            target_p50_ms: Some(120),
            target_p95_ms: Some(200),
            source: Some("phase1_web_table_first_page_target".to_string()),
        }),
        _ => None,
    }
}

async fn resolve_perf_probe_connection_url(
    state: &AppState,
    db_id: Option<&str>,
) -> Result<String, AppError> {
    let config = state.config.read().await.clone();
    if let Some(id) = db_id {
        let conn = config
            .db_connections
            .iter()
            .find(|conn| conn.id == id)
            .ok_or_else(|| AppError::BadRequest(format!("Database connection {id} not found")))?;
        return Ok(conn.url.clone());
    }

    config
        .get_active_db_url()
        .ok_or_else(|| AppError::BadRequest("Database not connected".to_string()))
}

async fn open_fresh_perf_probe_client(
    state: &AppState,
    db_id: Option<&str>,
) -> Result<DbClient, AppError> {
    let url = resolve_perf_probe_connection_url(state, db_id).await?;
    DbClient::new(&url).await.map_err(|e| AppError::InternalError(e.to_string()))
}

async fn run_connect_warm_probe(
    state: &AppState,
    db_id: Option<&str>,
    iterations: u32,
) -> Result<PerfProbeSummary, AppError> {
    let (warm_client, _) = resolve_db_client_for_request(state, db_id).await?;
    tokio::time::timeout(state.timeouts.db_query, warm_client.ping())
        .await
        .map_err(|_| AppError::Timeout("connect_warm warmup ping timed out".to_string()))?
        .map_err(|e| AppError::BadRequest(e.to_string()))?;

    let mut samples = Vec::with_capacity(iterations as usize);
    for iteration in 0..iterations {
        let started_at = Instant::now();
        let (db_client, _) = resolve_db_client_for_request(state, db_id).await?;
        tokio::time::timeout(state.timeouts.db_query, db_client.ping())
            .await
            .map_err(|_| AppError::Timeout("connect_warm ping timed out".to_string()))?
            .map_err(|e| AppError::BadRequest(e.to_string()))?;
        samples.push(PerfSample {
            operation: "connect_warm".to_string(),
            iteration: iteration + 1,
            duration_ms: started_at.elapsed().as_millis(),
            rows: None,
        });
    }

    Ok(summarize_perf_samples(
        "connect_warm",
        samples,
        default_perf_probe_budget("connect_warm"),
    ))
}

async fn run_connect_cold_probe(
    state: &AppState,
    db_id: Option<&str>,
    iterations: u32,
) -> Result<PerfProbeSummary, AppError> {
    let mut samples = Vec::with_capacity(iterations as usize);
    for iteration in 0..iterations {
        let started_at = Instant::now();
        let db_client = open_fresh_perf_probe_client(state, db_id).await?;
        tokio::time::timeout(state.timeouts.db_query, db_client.ping())
            .await
            .map_err(|_| AppError::Timeout("connect_cold ping timed out".to_string()))?
            .map_err(|e| AppError::BadRequest(e.to_string()))?;
        let duration_ms = started_at.elapsed().as_millis();
        db_client.pool.close().await;
        samples.push(PerfSample {
            operation: "connect_cold".to_string(),
            iteration: iteration + 1,
            duration_ms,
            rows: None,
        });
    }

    Ok(summarize_perf_samples(
        "connect_cold",
        samples,
        default_perf_probe_budget("connect_cold"),
    ))
}

async fn run_query_select_small_probe(
    state: &AppState,
    db_id: Option<&str>,
    sql: Option<&str>,
    iterations: u32,
) -> Result<PerfProbeSummary, AppError> {
    let sql = normalize_perf_probe_sql(sql)?;
    let (warm_client, _) = resolve_db_client_for_request(state, db_id).await?;
    tokio::time::timeout(
        state.timeouts.db_query,
        sqlx::query(&sql).fetch_all(&warm_client.pool),
    )
    .await
    .map_err(|_| AppError::Timeout("query_select_small warmup timed out".to_string()))?
    .map_err(|e| AppError::BadRequest(e.to_string()))?;

    let mut samples = Vec::with_capacity(iterations as usize);
    for iteration in 0..iterations {
        let (db_client, _) = resolve_db_client_for_request(state, db_id).await?;
        let started_at = Instant::now();
        let rows = tokio::time::timeout(
            state.timeouts.db_query,
            sqlx::query(&sql).fetch_all(&db_client.pool),
        )
        .await
        .map_err(|_| AppError::Timeout("query_select_small timed out".to_string()))?
        .map_err(|e| AppError::BadRequest(e.to_string()))?;

        samples.push(PerfSample {
            operation: "query_select_small".to_string(),
            iteration: iteration + 1,
            duration_ms: started_at.elapsed().as_millis(),
            rows: Some(rows.len() as u64),
        });
    }

    Ok(summarize_perf_samples(
        "query_select_small",
        samples,
        default_perf_probe_budget("query_select_small"),
    ))
}

async fn run_query_write_small_probe(
    state: &AppState,
    db_id: Option<&str>,
    iterations: u32,
) -> Result<PerfProbeSummary, AppError> {
    let (db_client, _) = resolve_db_client_for_request(state, db_id).await?;
    let mut conn = db_client
        .pool
        .acquire()
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    let temp_table = "__perf_probe_write_small";
    let drop_sql = format!("DROP TEMPORARY TABLE IF EXISTS {temp_table}");
    let create_sql = format!(
        "CREATE TEMPORARY TABLE {temp_table} (id BIGINT PRIMARY KEY AUTO_INCREMENT, marker VARCHAR(64) NOT NULL)"
    );
    let insert_sql = format!("INSERT INTO {temp_table} (marker) VALUES (?)");

    tokio::time::timeout(
        state.timeouts.db_query,
        sqlx::query(&drop_sql).execute(&mut *conn),
    )
    .await
    .map_err(|_| AppError::Timeout("query_write_small drop temp table timed out".to_string()))?
    .map_err(|e| AppError::InternalError(e.to_string()))?;
    tokio::time::timeout(
        state.timeouts.db_query,
        sqlx::query(&create_sql).execute(&mut *conn),
    )
    .await
    .map_err(|_| AppError::Timeout("query_write_small create temp table timed out".to_string()))?
    .map_err(|e| AppError::InternalError(e.to_string()))?;
    tokio::time::timeout(
        state.timeouts.db_query,
        sqlx::query(&insert_sql).bind("warmup").execute(&mut *conn),
    )
    .await
    .map_err(|_| AppError::Timeout("query_write_small warmup timed out".to_string()))?
    .map_err(|e| AppError::InternalError(e.to_string()))?;

    let mut samples = Vec::with_capacity(iterations as usize);
    for iteration in 0..iterations {
        let started_at = Instant::now();
        let result = tokio::time::timeout(
            state.timeouts.db_query,
            sqlx::query(&insert_sql)
                .bind(format!("probe-{}", iteration + 1))
                .execute(&mut *conn),
        )
        .await
        .map_err(|_| AppError::Timeout("query_write_small timed out".to_string()))?
        .map_err(|e| AppError::InternalError(e.to_string()))?;

        samples.push(PerfSample {
            operation: "query_write_small".to_string(),
            iteration: iteration + 1,
            duration_ms: started_at.elapsed().as_millis(),
            rows: Some(result.rows_affected()),
        });
    }

    let _ = tokio::time::timeout(
        state.timeouts.db_query,
        sqlx::query(&drop_sql).execute(&mut *conn),
    )
    .await;

    Ok(summarize_perf_samples(
        "query_write_small",
        samples,
        default_perf_probe_budget("query_write_small"),
    ))
}

async fn run_explain_plan_probe(
    state: &AppState,
    db_id: Option<&str>,
    sql: Option<&str>,
    iterations: u32,
) -> Result<PerfProbeSummary, AppError> {
    let explain_sql = build_perf_probe_explain_sql(sql)?;
    let (warm_client, _) = resolve_db_client_for_request(state, db_id).await?;
    tokio::time::timeout(
        state.timeouts.db_query,
        sqlx::query(&explain_sql).fetch_all(&warm_client.pool),
    )
    .await
    .map_err(|_| AppError::Timeout("explain_plan warmup timed out".to_string()))?
    .map_err(|e| AppError::BadRequest(e.to_string()))?;

    let mut samples = Vec::with_capacity(iterations as usize);
    for iteration in 0..iterations {
        let (db_client, _) = resolve_db_client_for_request(state, db_id).await?;
        let started_at = Instant::now();
        let rows = tokio::time::timeout(
            state.timeouts.db_query,
            sqlx::query(&explain_sql).fetch_all(&db_client.pool),
        )
        .await
        .map_err(|_| AppError::Timeout("explain_plan timed out".to_string()))?
        .map_err(|e| AppError::BadRequest(e.to_string()))?;

        samples.push(PerfSample {
            operation: "explain_plan".to_string(),
            iteration: iteration + 1,
            duration_ms: started_at.elapsed().as_millis(),
            rows: Some(rows.len() as u64),
        });
    }

    Ok(summarize_perf_samples(
        "explain_plan",
        samples,
        default_perf_probe_budget("explain_plan"),
    ))
}

async fn run_catalog_first_paint_probe(
    state: &AppState,
    db_id: Option<&str>,
    iterations: u32,
) -> Result<PerfProbeSummary, AppError> {
    let mut samples = Vec::with_capacity(iterations as usize);
    for iteration in 0..iterations {
        clear_metadata_caches(state).await;
        let (db_client, db_name) = resolve_db_client_for_request(state, db_id).await?;
        let started_at = Instant::now();
        let schema = get_cached_schema(state, db_id, &db_client, &db_name)
            .await
            .ok_or_else(|| AppError::InternalError("Failed to fetch schema".to_string()))?;
        samples.push(PerfSample {
            operation: "catalog_first_paint".to_string(),
            iteration: iteration + 1,
            duration_ms: started_at.elapsed().as_millis(),
            rows: Some((schema.tables.len() + schema.views.len()) as u64),
        });
    }

    Ok(summarize_perf_samples(
        "catalog_first_paint",
        samples,
        default_perf_probe_budget("catalog_first_paint"),
    ))
}

async fn run_table_first_page_probe(
    state: &AppState,
    db_id: Option<&str>,
    table_name: Option<&str>,
    iterations: u32,
) -> Result<PerfProbeSummary, AppError> {
    let table_name = normalize_perf_probe_table_name(table_name)?;
    let table_ident = quote_mysql_ident(&table_name)?;
    let data_sql = format!("SELECT * FROM {} LIMIT 101 OFFSET 0", table_ident);
    let mut samples = Vec::with_capacity(iterations as usize);

    for iteration in 0..iterations {
        clear_metadata_caches(state).await;
        let (db_client, db_name) = resolve_db_client_for_request(state, db_id).await?;
        let started_at = Instant::now();
        let _table_schema =
            get_cached_table_schema(state, db_id, &db_client, &db_name, &table_name).await?;
        let result_rows = tokio::time::timeout(
            state.timeouts.db_query,
            sqlx::query(&data_sql).fetch_all(&db_client.pool),
        )
        .await
        .map_err(|_| {
            AppError::Timeout(
                "table_first_page probe timed out after 30 seconds".to_string(),
            )
        })?
        .map_err(|e| AppError::InternalError(e.to_string()))?;

        let mut row_encoder = None;
        let data: Vec<serde_json::Value> = result_rows
            .into_iter()
            .take(100)
            .map(|row| {
                if row_encoder.is_none() {
                    row_encoder = Some(MySqlRowJsonEncoder::from_row(&row));
                }
                encode_mysql_row(
                    &row,
                    row_encoder
                        .as_ref()
                        .expect("row encoder should be initialized"),
                )
            })
            .collect();

        samples.push(PerfSample {
            operation: "table_first_page".to_string(),
            iteration: iteration + 1,
            duration_ms: started_at.elapsed().as_millis(),
            rows: Some(data.len() as u64),
        });
    }

    Ok(summarize_perf_samples(
        "table_first_page",
        samples,
        default_perf_probe_budget("table_first_page"),
    ))
}

async fn run_cancel_latency_probe(
    state: &AppState,
    db_id: Option<&str>,
    iterations: u32,
) -> Result<PerfProbeSummary, AppError> {
    let mut samples = Vec::with_capacity(iterations as usize);

    for iteration in 0..iterations {
        let (db_client, _) = resolve_db_client_for_request(state, db_id).await?;
        let mut conn = db_client
            .pool
            .acquire()
            .await
            .map_err(|e| AppError::InternalError(e.to_string()))?;
        let connection_id = DbClient::connection_id_for_session(&mut conn)
            .await
            .map_err(|e| AppError::InternalError(e.to_string()))?;
        let canceled = Arc::new(AtomicBool::new(false));
        let cancel_token = format!(
            "perf_probe_cancel_{}_{}",
            iteration + 1,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_millis())
                .unwrap_or(0)
        );
        register_active_query(
            state,
            cancel_token.clone(),
            ActiveQueryHandle {
                db_client: db_client.clone(),
                connection_id,
                canceled,
            },
        )
        .await;

        let query_task = tokio::spawn(async move {
            sqlx::query("SELECT SLEEP(2) AS perf_probe_cancel")
                .fetch_all(&mut *conn)
                .await
        });

        tokio::time::sleep(Duration::from_millis(100)).await;
        let started_at = Instant::now();
        let canceled_ok = cancel_active_query(state, &cancel_token).await?;
        let join_result = tokio::time::timeout(state.timeouts.db_query, query_task)
            .await
            .map_err(|_| AppError::Timeout("cancel_latency probe join timed out".to_string()))?;
        unregister_active_query(state, &cancel_token).await;
        join_result
            .map_err(|e| AppError::InternalError(e.to_string()))?
            .map_err(|e| AppError::InternalError(e.to_string()))?;

        if !canceled_ok {
            return Err(AppError::BadRequest(
                "cancel_latency probe could not cancel active query".to_string(),
            ));
        }

        samples.push(PerfSample {
            operation: "cancel_latency".to_string(),
            iteration: iteration + 1,
            duration_ms: started_at.elapsed().as_millis(),
            rows: None,
        });
    }

    Ok(summarize_perf_samples(
        "cancel_latency",
        samples,
        default_perf_probe_budget("cancel_latency"),
    ))
}

async fn diagnostics_perf_probe(
    State(state): State<AppState>,
    Json(req): Json<PerfProbeRequest>,
) -> Result<Json<PerfProbeSummary>, AppError> {
    let operation = req
        .operation
        .clone()
        .unwrap_or_else(|| "connect_warm".to_string())
        .trim()
        .to_lowercase();
    let iterations = normalize_perf_probe_iterations(req.iterations);

    let summary = match operation.as_str() {
        "connect_cold" => {
            run_connect_cold_probe(&state, req.db_id.as_deref(), iterations).await?
        }
        "connect_warm" => {
            run_connect_warm_probe(&state, req.db_id.as_deref(), iterations).await?
        }
        "query_select_small" => {
            run_query_select_small_probe(&state, req.db_id.as_deref(), req.sql.as_deref(), iterations)
                .await?
        }
        "query_write_small" => {
            run_query_write_small_probe(&state, req.db_id.as_deref(), iterations).await?
        }
        "explain_plan" => {
            run_explain_plan_probe(&state, req.db_id.as_deref(), req.sql.as_deref(), iterations)
                .await?
        }
        "catalog_first_paint" => {
            run_catalog_first_paint_probe(&state, req.db_id.as_deref(), iterations).await?
        }
        "table_first_page" => {
            run_table_first_page_probe(
                &state,
                req.db_id.as_deref(),
                req.table_name.as_deref(),
                iterations,
            )
            .await?
        }
        "cancel_latency" => {
            run_cancel_latency_probe(&state, req.db_id.as_deref(), iterations).await?
        }
        _ => {
            return Err(AppError::BadRequest(format!(
                "Unsupported perf probe operation: {}",
                operation
            )));
        }
    };

    Ok(Json(summary))
}

fn perf_suite_archive_dir(limits: &RuntimeLimits) -> std::path::PathBuf {
    let mut path = std::path::PathBuf::from(&limits.temp_dir);
    path.push("diagnostics");
    path.push("perf-suites");
    path
}

fn perf_suite_index_path(limits: &RuntimeLimits) -> std::path::PathBuf {
    perf_suite_archive_dir(limits).join("index.jsonl")
}

fn perf_suite_baseline_path(limits: &RuntimeLimits) -> std::path::PathBuf {
    perf_suite_archive_dir(limits).join("baseline.json")
}

fn perf_suite_diff_archive_dir(limits: &RuntimeLimits) -> std::path::PathBuf {
    perf_suite_archive_dir(limits).join("diffs")
}

fn perf_suite_diff_index_path(limits: &RuntimeLimits) -> std::path::PathBuf {
    perf_suite_diff_archive_dir(limits).join("index.jsonl")
}

async fn read_jsonl_all(path: &str) -> Result<Vec<serde_json::Value>, AppError> {
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
    Ok(rows)
}

async fn find_perf_suite_archive_record(
    limits: &RuntimeLimits,
    suite_id: &str,
) -> Result<Option<PerfSuiteArchiveRecord>, AppError> {
    let path = perf_suite_index_path(limits);
    let rows = read_jsonl_all(&path.to_string_lossy()).await?;
    for row in rows.into_iter().rev() {
        let Some(id) = row.get("id").and_then(|value| value.as_str()) else {
            continue;
        };
        if id != suite_id {
            continue;
        }
        let indexed_report = match serde_json::from_value::<PerfSuiteArchiveRecord>(row) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if let Some(archive_path) = indexed_report.archive_path.clone() {
            match tokio::fs::read_to_string(&archive_path).await {
                Ok(content) => {
                    if let Ok(report) = serde_json::from_str::<PerfSuiteArchiveRecord>(&content) {
                        return Ok(Some(report));
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(AppError::InternalError(e.to_string())),
            }
        }
        return Ok(Some(indexed_report));
    }
    Ok(None)
}

async fn diagnostics_perf_suite_list(
    State(state): State<AppState>,
    Query(q): Query<LimitQuery>,
) -> Result<Json<Vec<PerfSuiteArchiveRecord>>, AppError> {
    let limit = q
        .limit
        .unwrap_or(PERF_SUITE_ARCHIVE_DEFAULT_LIMIT)
        .clamp(1, 200);
    let path = perf_suite_index_path(&state.limits);
    let rows = read_jsonl_recent(&path.to_string_lossy(), limit).await?;
    let mut reports = Vec::with_capacity(rows.len());
    for row in rows {
        if let Ok(report) = serde_json::from_value::<PerfSuiteArchiveRecord>(row) {
            reports.push(report);
        }
    }
    Ok(Json(reports))
}

async fn diagnostics_perf_suite_detail(
    State(state): State<AppState>,
    Path(suite_id): Path<String>,
) -> Result<Json<PerfSuiteArchiveRecord>, AppError> {
    let report = find_perf_suite_archive_record(&state.limits, &suite_id)
        .await?
        .ok_or_else(|| AppError::NotFound("perf suite not found".to_string()))?;
    Ok(Json(report))
}

async fn diagnostics_perf_suite_save(
    State(state): State<AppState>,
    Json(mut report): Json<PerfSuiteArchiveRecord>,
) -> Result<Json<PerfSuiteArchiveRecord>, AppError> {
    if report.id.trim().is_empty() {
        return Err(AppError::BadRequest(
            "Perf suite report id is required".to_string(),
        ));
    }
    if report.recorded_at.trim().is_empty() {
        return Err(AppError::BadRequest(
            "Perf suite recorded_at is required".to_string(),
        ));
    }

    let archive_dir = perf_suite_archive_dir(&state.limits);
    let file_stem = {
        let value = safe_ident_suffix(&report.id);
        if value.is_empty() {
            "suite".to_string()
        } else {
            value
        }
    };
    let file_name = format!(
        "{}-{}.json",
        chrono::Utc::now().format("%Y%m%dT%H%M%S"),
        file_stem
    );
    let report_path = archive_dir.join(file_name);
    report.archive_path = Some(report_path.to_string_lossy().to_string());

    let pretty_bytes = serde_json::to_vec_pretty(&report)
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    let jsonl_line = serde_json::to_string(&report)
        .map_err(|e| AppError::InternalError(e.to_string()))?;

    ensure_temp_quota(
        &state.limits,
        pretty_bytes.len() as u64 + jsonl_line.len() as u64 + 1,
    )
    .await?;
    tokio::fs::create_dir_all(&archive_dir)
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    tokio::fs::write(&report_path, pretty_bytes)
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;

    let index_path = perf_suite_index_path(&state.limits);
    let mut index = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&index_path)
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    index
        .write_all(jsonl_line.as_bytes())
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    index
        .write_all(b"\n")
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;

    Ok(Json(report))
}

async fn diagnostics_perf_suite_baseline_get(
    State(state): State<AppState>,
) -> Result<Json<Option<PerfSuiteArchiveRecord>>, AppError> {
    let path = perf_suite_baseline_path(&state.limits);
    let content = match tokio::fs::read_to_string(&path).await {
        Ok(v) => v,
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                return Ok(Json(None));
            }
            return Err(AppError::InternalError(e.to_string()));
        }
    };
    let report = serde_json::from_str::<PerfSuiteArchiveRecord>(&content)
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    Ok(Json(Some(report)))
}

async fn diagnostics_perf_suite_baseline_pin(
    State(state): State<AppState>,
    Json(req): Json<PerfSuiteBaselinePinRequest>,
) -> Result<Json<PerfSuiteArchiveRecord>, AppError> {
    let suite_id = req.suite_id.trim();
    if suite_id.is_empty() {
        return Err(AppError::BadRequest(
            "Perf suite baseline suite_id is required".to_string(),
        ));
    }

    let report = find_perf_suite_archive_record(&state.limits, suite_id)
        .await?
        .ok_or_else(|| AppError::NotFound("perf suite not found".to_string()))?;
    let baseline_path = perf_suite_baseline_path(&state.limits);
    let bytes = serde_json::to_vec_pretty(&report)
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    ensure_temp_quota(&state.limits, bytes.len() as u64).await?;
    tokio::fs::create_dir_all(perf_suite_archive_dir(&state.limits))
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    tokio::fs::write(baseline_path, bytes)
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    Ok(Json(report))
}

async fn diagnostics_perf_suite_diff_list(
    State(state): State<AppState>,
    Query(q): Query<PerfSuiteDiffListQuery>,
) -> Result<Json<Vec<PerfSuiteDiffArchiveRecord>>, AppError> {
    let limit = q.limit.unwrap_or(PERF_SUITE_ARCHIVE_DEFAULT_LIMIT).clamp(1, 200);
    let path = perf_suite_diff_index_path(&state.limits);
    let rows = read_jsonl_recent(&path.to_string_lossy(), limit).await?;
    let mut reports = Vec::with_capacity(rows.len());
    for row in rows {
        let Ok(report) = serde_json::from_value::<PerfSuiteDiffArchiveRecord>(row) else {
            continue;
        };
        if let Some(current_suite_id) = q.current_suite_id.as_deref() {
            if report.current_suite_id != current_suite_id {
                continue;
            }
        }
        if let Some(baseline_suite_id) = q.baseline_suite_id.as_deref() {
            if report.baseline_suite_id != baseline_suite_id {
                continue;
            }
        }
        reports.push(report);
    }
    Ok(Json(reports))
}

async fn diagnostics_perf_suite_diff_save(
    State(state): State<AppState>,
    Json(mut report): Json<PerfSuiteDiffArchiveRecord>,
) -> Result<Json<PerfSuiteDiffArchiveRecord>, AppError> {
    if report.id.trim().is_empty() {
        return Err(AppError::BadRequest(
            "Perf suite diff report id is required".to_string(),
        ));
    }
    if report.recorded_at.trim().is_empty() {
        return Err(AppError::BadRequest(
            "Perf suite diff recorded_at is required".to_string(),
        ));
    }
    if report.current_suite_id.trim().is_empty() || report.baseline_suite_id.trim().is_empty() {
        return Err(AppError::BadRequest(
            "Perf suite diff current/baseline suite id is required".to_string(),
        ));
    }

    let archive_dir = perf_suite_diff_archive_dir(&state.limits);
    let file_stem = {
        let value = safe_ident_suffix(&report.id);
        if value.is_empty() {
            "diff".to_string()
        } else {
            value
        }
    };
    let file_name = format!(
        "{}-{}.json",
        chrono::Utc::now().format("%Y%m%dT%H%M%S"),
        file_stem
    );
    let report_path = archive_dir.join(file_name);
    report.archive_path = Some(report_path.to_string_lossy().to_string());

    let pretty_bytes = serde_json::to_vec_pretty(&report)
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    let jsonl_line = serde_json::to_string(&report)
        .map_err(|e| AppError::InternalError(e.to_string()))?;

    ensure_temp_quota(
        &state.limits,
        pretty_bytes.len() as u64 + jsonl_line.len() as u64 + 1,
    )
    .await?;
    tokio::fs::create_dir_all(&archive_dir)
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    tokio::fs::write(&report_path, pretty_bytes)
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;

    let index_path = perf_suite_diff_index_path(&state.limits);
    let mut index = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&index_path)
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    index
        .write_all(jsonl_line.as_bytes())
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    index
        .write_all(b"\n")
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;

    Ok(Json(report))
}

async fn execute_sql(
    State(state): State<AppState>,
    Json(mut req): Json<ExecuteRequest>,
) -> Result<Json<ExecuteResponse>, AppError> {
    let (db_client, _) = resolve_db_client_for_request(&state, req.db_id.as_deref()).await?;
    let is_read_only = is_read_only_connection(&state, req.db_id.as_deref()).await;

    use std::time::Instant;

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
    let statement_kind = clean_sql
        .split_whitespace()
        .next()
        .map(|part| part.to_uppercase());

    let is_select = upper_sql.starts_with("SELECT")
        || upper_sql.starts_with("SHOW")
        || upper_sql.starts_with("DESCRIBE")
        || upper_sql.starts_with("EXPLAIN");

    if is_read_only && !is_select {
        return Err(AppError::Forbidden(
            "当前连接为只读模式，禁止执行非查询操作！".to_string(),
        ));
    }

    // Safety check for dangerous operations
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

    let mut rows = Vec::new();
    let mut columns = Vec::new();
    let mut affected_rows = 0;
    let mut has_more = false;
    let mut next_offset = None;
    let chunk_offset = req.chunk_offset.unwrap_or(0);
    let mut chunk_size = None;
    let mut preview_cap = None;
    let mut truncated = false;
    let is_chunked_preview =
        is_select && upper_sql.starts_with("SELECT") && !upper_sql.contains("LIMIT");

    if is_chunked_preview {
        let requested_chunk_size = req
            .chunk_size
            .unwrap_or(QUERY_PREVIEW_CHUNK_SIZE)
            .clamp(1, QUERY_PREVIEW_CHUNK_SIZE);
        let remaining = QUERY_PREVIEW_ROW_CAP.saturating_sub(chunk_offset);
        let effective_chunk_size = requested_chunk_size.min(remaining.max(1));
        req.sql = req.sql.trim().trim_end_matches(';').to_string();
        req.sql.push_str(&format!(
            " LIMIT {} OFFSET {}",
            effective_chunk_size + 1,
            chunk_offset
        ));
        chunk_size = Some(requested_chunk_size);
        preview_cap = Some(QUERY_PREVIEW_ROW_CAP);
    } else if is_select && !upper_sql.contains("LIMIT") {
        req.sql = req.sql.trim().trim_end_matches(';').to_string();
        req.sql.push_str(" LIMIT 1000");
    }

    let transaction_id = req
        .transaction_id
        .as_deref()
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_string);
    let transaction_session = if let Some(id) = transaction_id.as_deref() {
        Some(get_or_open_transaction_session(&state, req.db_id.as_deref(), id).await?)
    } else {
        None
    };
    let cancel_token = req
        .cancel_token
        .as_deref()
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_string);
    let mut active_query = if let Some(token) = cancel_token {
        let canceled = Arc::new(AtomicBool::new(false));
        if let Some(transaction_session) = transaction_session.clone() {
            let connection_id = transaction_session.lock().await.connection_id;
            register_active_query(
                &state,
                token.clone(),
                ActiveQueryHandle {
                    db_client: db_client.clone(),
                    connection_id,
                    canceled: canceled.clone(),
                },
            )
            .await;
            Some(ActiveQuerySession {
                token,
                connection_id,
                canceled,
                owned_conn: None,
                transaction_session: Some(transaction_session),
            })
        } else {
            let mut conn = db_client
                .pool
                .acquire()
                .await
                .map_err(|e| AppError::InternalError(e.to_string()))?;
            let connection_id = DbClient::connection_id_for_session(&mut conn)
                .await
                .map_err(|e| AppError::InternalError(e.to_string()))?;
            register_active_query(
                &state,
                token.clone(),
                ActiveQueryHandle {
                    db_client: db_client.clone(),
                    connection_id,
                    canceled: canceled.clone(),
                },
            )
            .await;
            Some(ActiveQuerySession {
                token,
                connection_id,
                canceled,
                owned_conn: Some(conn),
                transaction_session: None,
            })
        }
    } else {
        None
    };

    let start_time = Instant::now();
    let execution_result = if is_select {
        match tokio::time::timeout(state.timeouts.db_query, async {
            if let Some(active_query) = active_query.as_mut() {
                if let Some(transaction_session) = active_query.transaction_session.as_ref() {
                    let mut session = transaction_session.lock().await;
                    sqlx::query(&req.sql).fetch_all(&mut *session.conn).await
                } else if let Some(conn) = active_query.owned_conn.as_mut() {
                    sqlx::query(&req.sql).fetch_all(&mut **conn).await
                } else {
                    sqlx::query(&req.sql).fetch_all(&db_client.pool).await
                }
            } else if let Some(transaction_session) = transaction_session.as_ref() {
                let mut session = transaction_session.lock().await;
                sqlx::query(&req.sql).fetch_all(&mut *session.conn).await
            } else {
                sqlx::query(&req.sql).fetch_all(&db_client.pool).await
            }
        })
        .await
        {
            Ok(res) => res,
            Err(_) => {
                if let Some(active_query) = active_query.as_ref() {
                    let _ = db_client.kill_query(active_query.connection_id).await;
                    unregister_active_query(&state, &active_query.token).await;
                }
                return Err(AppError::Timeout(
                    "查询执行超时（已超过 30 秒），已被系统安全阻断，请优化 SQL 或添加索引。"
                        .to_string(),
                ));
            }
        }
    } else {
        // Just for type matching we do a dummy empty result, the real logic is below
        Ok(vec![])
    };

    let mut status = "success".to_string();
    let mut err_msg = None;

    if is_select {
        match execution_result {
            Ok(result_rows) => {
                let chunk_limit = if is_chunked_preview {
                    chunk_size.unwrap_or(QUERY_PREVIEW_CHUNK_SIZE)
                } else {
                    result_rows.len() as u32
                };
                let fetched_len = result_rows.len() as u32;
                if is_chunked_preview {
                    has_more = fetched_len > chunk_limit
                        && chunk_offset.saturating_add(chunk_limit) < QUERY_PREVIEW_ROW_CAP;
                    next_offset = has_more.then_some(chunk_offset.saturating_add(chunk_limit));
                    truncated = fetched_len > chunk_limit;
                }

                let mut row_encoder = None;
                for row in result_rows.into_iter().take(chunk_limit as usize) {
                    if row_encoder.is_none() {
                        let encoder = MySqlRowJsonEncoder::from_row(&row);
                        columns = encoder.column_names();
                        row_encoder = Some(encoder);
                    }
                    rows.push(encode_mysql_row(
                        &row,
                        row_encoder
                            .as_ref()
                            .expect("row encoder should be initialized"),
                    ));
                }
            }
            Err(e) => {
                let query_was_canceled = active_query
                    .as_ref()
                    .map(|query| query.canceled.load(Ordering::SeqCst))
                    .unwrap_or(false);
                status = if query_was_canceled {
                    "canceled".to_string()
                } else {
                    "error".to_string()
                };
                err_msg = Some(if query_was_canceled {
                    "Query canceled".to_string()
                } else {
                    e.to_string()
                });
            }
        }
    } else {
        match tokio::time::timeout(state.timeouts.db_query, async {
            if let Some(active_query) = active_query.as_mut() {
                if let Some(transaction_session) = active_query.transaction_session.as_ref() {
                    let mut session = transaction_session.lock().await;
                    sqlx::query(&req.sql).execute(&mut *session.conn).await
                } else if let Some(conn) = active_query.owned_conn.as_mut() {
                    sqlx::query(&req.sql).execute(&mut **conn).await
                } else {
                    sqlx::query(&req.sql).execute(&db_client.pool).await
                }
            } else if let Some(transaction_session) = transaction_session.as_ref() {
                let mut session = transaction_session.lock().await;
                sqlx::query(&req.sql).execute(&mut *session.conn).await
            } else {
                sqlx::query(&req.sql).execute(&db_client.pool).await
            }
        })
        .await
        {
            Ok(Ok(result)) => {
                affected_rows = result.rows_affected();
            }
            Ok(Err(e)) => {
                let query_was_canceled = active_query
                    .as_ref()
                    .map(|query| query.canceled.load(Ordering::SeqCst))
                    .unwrap_or(false);
                status = if query_was_canceled {
                    "canceled".to_string()
                } else {
                    "error".to_string()
                };
                err_msg = Some(if query_was_canceled {
                    "Query canceled".to_string()
                } else {
                    e.to_string()
                });
            }
            Err(_) => {
                if let Some(active_query) = active_query.as_ref() {
                    let _ = db_client.kill_query(active_query.connection_id).await;
                    unregister_active_query(&state, &active_query.token).await;
                }
                return Err(AppError::Timeout(
                    "查询执行超时（已超过 30 秒），已被系统安全阻断，请优化 SQL 或添加索引。"
                        .to_string(),
                ));
            }
        }
    }

    if let Some(active_query) = active_query.as_ref() {
        unregister_active_query(&state, &active_query.token).await;
    }
    let was_canceled = active_query
        .as_ref()
        .map(|query| query.canceled.load(Ordering::SeqCst))
        .unwrap_or(false);

    let elapsed = start_time.elapsed().as_millis() as u64;
    let history_row_count = if err_msg.is_none() && is_select {
        Some(rows.len() as u64)
    } else {
        None
    };
    let history_affected_rows = if err_msg.is_none() && !is_select {
        Some(affected_rows)
    } else {
        None
    };

    // Record history
    {
        let store_clone = {
            let mut store = state.sql_history.write().await;
            store.add_history(SqlHistory {
                id: "".to_string(), // will be generated
                sql: req.sql.clone(),
                status,
                execution_time_ms: elapsed,
                executed_at: 0, // will be generated
                db_id: req.db_id.clone(),
                row_count: history_row_count,
                affected_rows: history_affected_rows,
                statement_kind: statement_kind.clone(),
            });
            store.clone()
        };
        let _ = store_clone.save().await; // ignore save errors for history
    }

    if let Some(e) = err_msg {
        if was_canceled {
            return Err(AppError::Canceled(e));
        }
        return Err(AppError::InternalError(e));
    }

    if !is_select {
        clear_metadata_caches(&state).await;
    }

    let transaction_state = if let Some(id) = transaction_id.as_deref() {
        if state.transaction_sessions.read().await.contains_key(id) {
            Some("active".to_string())
        } else {
            Some("idle".to_string())
        }
    } else {
        None
    };

    Ok(Json(ExecuteResponse {
        columns,
        row_count: rows.len(),
        rows,
        affected_rows,
        execution_time_ms: elapsed,
        has_more,
        next_offset,
        chunk_offset,
        chunk_size,
        preview_cap,
        truncated,
        transaction_state,
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
    let (db_client, _) = resolve_db_client_for_request(&state, req.db_id.as_deref()).await?;

    let page = req.page.unwrap_or(1);
    let page_size = req.page_size.unwrap_or(100);
    let offset = (page - 1) * page_size;

    let mut where_clause = String::new();
    let mut bindings = Vec::new();

    if let Some(filters_str) = &req.filters {
        if let Ok(filters) = serde_json::from_str::<Vec<FilterCondition>>(filters_str) {
            let mut conditions = Vec::new();
            for f in filters {
                let col = quote_mysql_ident(&f.column)?;
                match f.operator.as_str() {
                    "equals" => {
                        conditions.push(format!("{} = ?", col));
                        bindings.push(f.value.clone());
                    }
                    "not_equals" => {
                        conditions.push(format!("{} <> ?", col));
                        bindings.push(f.value.clone());
                    }
                    "contains" => {
                        conditions.push(format!("{} LIKE ?", col));
                        bindings.push(format!("%{}%", f.value));
                    }
                    "starts_with" => {
                        conditions.push(format!("{} LIKE ?", col));
                        bindings.push(format!("{}%", f.value));
                    }
                    "ends_with" => {
                        conditions.push(format!("{} LIKE ?", col));
                        bindings.push(format!("%{}", f.value));
                    }
                    "greater_than" => {
                        conditions.push(format!("{} > ?", col));
                        bindings.push(f.value.clone());
                    }
                    "less_than" => {
                        conditions.push(format!("{} < ?", col));
                        bindings.push(f.value.clone());
                    }
                    "between" => {
                        let parts: Vec<String> = f
                            .value
                            .split(',')
                            .map(|part| part.trim().to_string())
                            .filter(|part| !part.is_empty())
                            .collect();
                        if parts.len() >= 2 {
                            conditions.push(format!("{} BETWEEN ? AND ?", col));
                            bindings.push(parts[0].clone());
                            bindings.push(parts[1].clone());
                        }
                    }
                    "in" => {
                        let parts: Vec<String> = f
                            .value
                            .split(',')
                            .map(|part| part.trim().to_string())
                            .filter(|part| !part.is_empty())
                            .collect();
                        if !parts.is_empty() {
                            let placeholders = std::iter::repeat("?")
                                .take(parts.len())
                                .collect::<Vec<_>>()
                                .join(", ");
                            conditions.push(format!("{} IN ({})", col, placeholders));
                            bindings.extend(parts);
                        }
                    }
                    "not_in" => {
                        let parts: Vec<String> = f
                            .value
                            .split(',')
                            .map(|part| part.trim().to_string())
                            .filter(|part| !part.is_empty())
                            .collect();
                        if !parts.is_empty() {
                            let placeholders = std::iter::repeat("?")
                                .take(parts.len())
                                .collect::<Vec<_>>()
                                .join(", ");
                            conditions.push(format!("{} NOT IN ({})", col, placeholders));
                            bindings.extend(parts);
                        }
                    }
                    "is_null" => {
                        conditions.push(format!("{} IS NULL", col));
                    }
                    "is_not_null" => {
                        conditions.push(format!("{} IS NOT NULL", col));
                    }
                    _ => {
                        conditions.push(format!("{} = ?", col));
                        bindings.push(f.value.clone());
                    }
                }
            }
            if !conditions.is_empty() {
                where_clause = format!("WHERE {}", conditions.join(" AND "));
            }
        }
    }

    let mut order_clause = String::new();
    if let Some(orders_str) = &req.orders {
        if let Ok(orders) = serde_json::from_str::<Vec<OrderCondition>>(orders_str) {
            let mut o_clauses = Vec::new();
            for o in orders {
                let dir = if o.desc { "DESC" } else { "ASC" };
                let col = quote_mysql_ident(&o.column)?;
                o_clauses.push(format!("{} {}", col, dir));
            }
            if !o_clauses.is_empty() {
                order_clause = format!("ORDER BY {}", o_clauses.join(", "));
            }
        }
    }

    let table_ident = quote_mysql_ident(&req.table_name)?;
    let data_sql = format!(
        "SELECT * FROM {} {} {} LIMIT {} OFFSET {}",
        table_ident,
        where_clause,
        order_clause,
        page_size + 1,
        offset
    );
    let mut data_query = sqlx::query(&data_sql);
    for b in &bindings {
        data_query = data_query.bind(b);
    }

    let mut rows = Vec::new();
    let result_rows = match tokio::time::timeout(
        state.timeouts.db_query,
        data_query.fetch_all(&db_client.pool),
    )
    .await
    {
        Ok(Ok(res)) => res,
        Ok(Err(e)) => return Err(AppError::InternalError(e.to_string())),
        Err(_) => {
            return Err(AppError::Timeout(
                "Query timed out after 30 seconds. Please optimize SQL or add indexes.".to_string(),
            ))
        }
    };

    let has_more = result_rows.len() as u32 > page_size;
    let mut row_encoder = None;
    for row in result_rows.into_iter().take(page_size as usize) {
        if row_encoder.is_none() {
            row_encoder = Some(MySqlRowJsonEncoder::from_row(&row));
        }
        rows.push(encode_mysql_row(
            &row,
            row_encoder
                .as_ref()
                .expect("row encoder should be initialized"),
        ));
    }

    Ok(Json(GetTableDataResponse {
        data: rows,
        total: None,
        total_status: "calculating".to_string(),
        has_more,
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
}

#[derive(Serialize)]
struct PreviewDdlResponse {
    sql: String,
}

async fn preview_ddl(
    Json(req): Json<PreviewDdlRequest>,
) -> Result<Json<PreviewDdlResponse>, AppError> {
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

    let (db_client, _) = resolve_db_client_for_request(&state, req.db_id.as_deref()).await?;

    let result = sqlx::query(&req.sql)
        .execute(&db_client.pool)
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    clear_metadata_caches(&state).await;

    Ok(Json(ExecuteResponse {
        columns: vec![],
        row_count: 0,
        rows: vec![],
        affected_rows: result.rows_affected(),
        execution_time_ms: 0,
        has_more: false,
        next_offset: None,
        chunk_offset: 0,
        chunk_size: None,
        preview_cap: None,
        truncated: false,
    }))
}

// ----------------- Tools API Handlers -----------------

#[derive(Deserialize)]
struct MockDataRequest {
    table_name: String,
    row_count: u32,
    rules: Option<std::collections::HashMap<String, String>>,
}

#[derive(Serialize)]
struct MockDataResponse {
    sql: String,
}

async fn generate_mock_data(
    State(state): State<AppState>,
    Json(req): Json<MockDataRequest>,
) -> Result<Json<MockDataResponse>, AppError> {
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

    let planner = state.planner.read().await.clone();
    let gateway = planner.gateway;

    let sql = MockDataGenerator::generate(&gateway, &db_client, &table, req.row_count, req.rules)
        .await
        .map_err(AppError::InternalError)?;

    Ok(Json(MockDataResponse { sql }))
}

use axum::body::Body;
use futures::StreamExt;

#[derive(Deserialize)]
struct ExportRequest {
    table_name: String,
    export_type: String, // "csv", "sql", "json"
}

async fn export_data(
    State(state): State<AppState>,
    Json(req): Json<ExportRequest>,
) -> Result<Response, AppError> {
    let db_client = state
        .db_client
        .read()
        .await
        .clone()
        .ok_or_else(|| AppError::BadRequest("Database not connected".to_string()))?;

    let table_name = req.table_name.clone();
    let export_type = req.export_type.clone();
    let data_sql = format!("SELECT * FROM {}", table_name);

    let (tx, rx) =
        tokio::sync::mpsc::channel::<Result<axum::body::Bytes, std::convert::Infallible>>(100);

    let spawn_table_name = table_name.clone();
    let spawn_export_type = export_type.clone();
    tokio::spawn(async move {
        use sqlx::Column;
        use sqlx::Row;

        let mut stream = sqlx::query(&data_sql).fetch(&db_client.pool);
        let mut headers_sent = false;
        let mut headers = Vec::new();
        let mut is_first_json = true;
        let mut previous_row: Option<serde_json::Map<String, serde_json::Value>> = None;

        while let Some(row_result) = stream.next().await {
            if let Ok(row) = row_result {
                if !headers_sent {
                    for col in row.columns() {
                        headers.push(col.name().to_string());
                    }
                    if spawn_export_type == "csv" {
                        let _ = tx
                            .send(Ok(axum::body::Bytes::from(DataExporter::csv_header(
                                &headers,
                            ))))
                            .await;
                    } else if spawn_export_type == "sql" {
                        let _ = tx
                            .send(Ok(axum::body::Bytes::from(DataExporter::sql_header(
                                &spawn_table_name,
                                &headers,
                            ))))
                            .await;
                    }
                    headers_sent = true;
                }

                let mut map = serde_json::Map::new();
                for col in row.columns() {
                    let col_name = col.name().to_string();
                    if let Ok(val) = row.try_get::<Option<i64>, _>(col.ordinal()) {
                        map.insert(col_name, serde_json::json!(val));
                    } else if let Ok(val) = row.try_get::<Option<f64>, _>(col.ordinal()) {
                        map.insert(col_name, serde_json::json!(val));
                    } else if let Ok(val) = row.try_get::<Option<bool>, _>(col.ordinal()) {
                        map.insert(col_name, serde_json::json!(val));
                    } else if let Ok(val) =
                        row.try_get::<Option<chrono::NaiveDateTime>, _>(col.ordinal())
                    {
                        map.insert(col_name, serde_json::json!(val.map(|dt| dt.to_string())));
                    } else if let Ok(val) =
                        row.try_get::<Option<chrono::NaiveDate>, _>(col.ordinal())
                    {
                        map.insert(col_name, serde_json::json!(val.map(|d| d.to_string())));
                    } else if let Ok(val) =
                        row.try_get::<Option<chrono::NaiveTime>, _>(col.ordinal())
                    {
                        map.insert(col_name, serde_json::json!(val.map(|t| t.to_string())));
                    } else if let Ok(val) = row.try_get::<Option<String>, _>(col.ordinal()) {
                        map.insert(col_name, serde_json::json!(val));
                    } else {
                        let val: Option<Vec<u8>> = row.try_get(col.ordinal()).unwrap_or(None);
                        if let Some(bytes) = val {
                            let s = String::from_utf8_lossy(&bytes).into_owned();
                            map.insert(col_name, serde_json::json!(s));
                        } else {
                            map.insert(col_name, serde_json::Value::Null);
                        }
                    }
                }

                if spawn_export_type == "csv" {
                    let _ = tx
                        .send(Ok(axum::body::Bytes::from(DataExporter::csv_row(
                            &headers, &map,
                        ))))
                        .await;
                } else if spawn_export_type == "sql" {
                    if let Some(prev) = previous_row.take() {
                        let _ = tx
                            .send(Ok(axum::body::Bytes::from(DataExporter::sql_row(
                                &headers, &prev, false,
                            ))))
                            .await;
                    }
                    previous_row = Some(map);
                } else if spawn_export_type == "json" {
                    if let Some(prev) = previous_row.take() {
                        let _ = tx
                            .send(Ok(axum::body::Bytes::from(DataExporter::json_row(
                                &prev,
                                is_first_json,
                                false,
                            ))))
                            .await;
                        is_first_json = false;
                    }
                    previous_row = Some(map);
                }
            }
        }

        // Flush last row
        if spawn_export_type == "sql" {
            if let Some(prev) = previous_row {
                let _ = tx
                    .send(Ok(axum::body::Bytes::from(DataExporter::sql_row(
                        &headers, &prev, true,
                    ))))
                    .await;
            } else if !headers_sent {
                // No rows, empty file
            }
        } else if spawn_export_type == "json" {
            if let Some(prev) = previous_row {
                let _ = tx
                    .send(Ok(axum::body::Bytes::from(DataExporter::json_row(
                        &prev,
                        is_first_json,
                        true,
                    ))))
                    .await;
            } else {
                let _ = tx.send(Ok(axum::body::Bytes::from("[]\n"))).await;
            }
        }
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    let body = Body::from_stream(stream);

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
struct ExportJobStartRequest {
    table_name: String,
    export_type: String,
    where_clause: Option<String>,
    primary_key: Option<String>,
    pk_start: Option<String>,
    pk_end: Option<String>,
    window_limit: Option<u64>,
    window_offset: Option<u64>,
}

#[derive(Deserialize)]
struct ImportJobStartRequest {
    table_name: String,
    data: Vec<std::collections::HashMap<String, serde_json::Value>>,
    mapping: std::collections::HashMap<String, String>,
    skip_errors: bool,
}

#[derive(Deserialize)]
struct ImportSqlJobStartRequest {
    sql: String,
    force: Option<bool>,
    db_id: Option<String>,
}

#[derive(Serialize)]
struct ToolJobStartResponse {
    job_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct GoLiveThresholds {
    #[serde(default)]
    max_total_ms: Option<u64>,
    #[serde(default)]
    per_step_max_ms: HashMap<String, u64>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct GoLiveJobStartRequest {
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
struct GoLiveStepReport {
    name: String,
    connection_id: Option<String>,
    status: GoLiveStepStatus,
    duration_ms: u128,
    errors: Vec<String>,
    code: Option<String>,
    details: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GoLiveReport {
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

async fn tool_job_status(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Result<Json<ToolJob>, AppError> {
    let job = { state.tool_jobs.read().await.get(&job_id).cloned() };
    job.map(Json)
        .ok_or_else(|| AppError::NotFound("job not found".to_string()))
}

async fn tool_job_cancel(
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

async fn tool_job_artifact_download(
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
struct LimitQuery {
    limit: Option<usize>,
}

async fn go_live_reports_list(
    State(state): State<AppState>,
    Query(q): Query<LimitQuery>,
) -> Result<Json<Vec<serde_json::Value>>, AppError> {
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let temp_dir = state.limits.temp_dir.trim_end_matches('/').to_string();
    let path = format!("{}/go-live-index.jsonl", temp_dir);
    Ok(Json(read_jsonl_recent(&path, limit).await?))
}

async fn go_live_audit_list(
    State(state): State<AppState>,
    Query(q): Query<LimitQuery>,
) -> Result<Json<Vec<serde_json::Value>>, AppError> {
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let temp_dir = state.limits.temp_dir.trim_end_matches('/').to_string();
    let path = format!("{}/go-live-audit.jsonl", temp_dir);
    Ok(Json(read_jsonl_recent(&path, limit).await?))
}

async fn read_jsonl_recent(path: &str, limit: usize) -> Result<Vec<serde_json::Value>, AppError> {
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

async fn export_job_start(
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

async fn import_job_start(
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

async fn import_sql_job_start(
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

fn mask_db_url(url: &str) -> String {
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

fn sanitize_config_for_report(config: &AppConfig) -> serde_json::Value {
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

fn ai_key_present(config: &AppConfig) -> bool {
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

fn normalize_go_live_steps(raw: Vec<String>) -> Vec<String> {
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

fn go_live_step_is_write(step: &str) -> bool {
    matches!(step, "sql_smoke" | "export_import_smoke")
}

fn safe_ident_suffix(s: &str) -> String {
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

fn apply_go_live_thresholds(
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

async fn go_live_job_start(
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
struct GoLiveConnSpec {
    id: String,
    url: String,
    db_type: DbType,
    is_read_only: bool,
}

fn resolve_go_live_connections(
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
                .unwrap_or_else(|| DbType::from_url(&url));
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

async fn append_jsonl(
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

async fn run_go_live_job(
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
                    DbClient::new(&conn.url).await.map_err(|e| e.to_string())
                };

            let mut client_opt: Option<DbClient> = None;
            match client_res {
                Ok(c) => {
                    let r: Result<(i64,), sqlx::Error> =
                        sqlx::query_as("SELECT 1").fetch_one(&c.pool).await;
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
                        match client.pool.acquire().await {
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

                        let pool = client.pool.clone();
                        let drop_all = async {
                            let _ = sqlx::query(&format!("DROP TABLE IF EXISTS `{}`", dst_table))
                                .execute(&pool)
                                .await;
                            let _ = sqlx::query(&format!("DROP TABLE IF EXISTS `{}`", src_table))
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
                            sqlx::query(&format!("DROP TABLE IF EXISTS `{}`", dst_table))
                                .execute(&pool)
                                .await
                                .map_err(|e| AppError::InternalError(e.to_string()))?;
                            sqlx::query(&format!("DROP TABLE IF EXISTS `{}`", src_table))
                                .execute(&pool)
                                .await
                                .map_err(|e| AppError::InternalError(e.to_string()))?;

                            sqlx::query(&format!(
                                "CREATE TABLE `{}` (id BIGINT PRIMARY KEY, name VARCHAR(255) NOT NULL, score DOUBLE NOT NULL, created_at DATETIME NOT NULL)",
                                src_table
                            ))
                            .execute(&pool)
                            .await
                            .map_err(|e| AppError::InternalError(e.to_string()))?;

                            sqlx::query(&format!(
                                "CREATE TABLE `{}` (id BIGINT PRIMARY KEY, name VARCHAR(255) NOT NULL, score DOUBLE NOT NULL, created_at DATETIME NOT NULL)",
                                dst_table
                            ))
                            .execute(&pool)
                            .await
                            .map_err(|e| AppError::InternalError(e.to_string()))?;

                            let now = chrono::Utc::now().naive_utc();
                            for i in 1..=25i64 {
                                sqlx::query(&format!(
                                    "INSERT INTO `{}` (id, name, score, created_at) VALUES (?, ?, ?, ?)",
                                    src_table
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

                            let (c,): (i64,) = sqlx::query_as(&format!("SELECT COUNT(*) FROM `{}`", dst_table))
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
            let planner = state.planner.read().await.clone();
            match planner
                .generate_rule_template("go-live smoke", "SELECT 1;")
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

async fn write_go_live_report(
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

struct ExportStats {
    sha256: String,
    line_count: u64,
    bytes: u64,
    row_count: u64,
}

async fn update_tool_job(state: &AppState, job_id: &str, f: impl FnOnce(&mut ToolJob)) {
    let mut jobs = state.tool_jobs.write().await;
    if let Some(job) = jobs.get_mut(job_id) {
        f(job);
        job.updated_at = chrono::Utc::now().timestamp();
    }
}

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn sql_literal(s: &str) -> String {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        "NULL".to_string()
    } else if trimmed.parse::<i64>().is_ok() || trimmed.parse::<f64>().is_ok() {
        trimmed.to_string()
    } else {
        format!("'{}'", trimmed.replace('\'', "''"))
    }
}

async fn write_stats_chunk(
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

async fn fetch_table_columns(
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

async fn run_export_job(
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

    let headers = fetch_table_columns(&db_client.pool, &req.table_name)
        .await
        .unwrap_or_default();

    let mut conditions = Vec::new();
    if let Some(w) = &req.where_clause {
        if !w.trim().is_empty() {
            conditions.push(format!("({})", w));
        }
    }
    if let Some(pk) = &req.primary_key {
        if let Some(s) = &req.pk_start {
            if !s.trim().is_empty() {
                conditions.push(format!("`{}` >= {}", pk, sql_literal(s)));
            }
        }
        if let Some(s) = &req.pk_end {
            if !s.trim().is_empty() {
                conditions.push(format!("`{}` <= {}", pk, sql_literal(s)));
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
        .map(|pk| format!(" ORDER BY `{}`", pk))
        .unwrap_or_default();

    let mut limit_sql = String::new();
    if let Some(lim) = req.window_limit {
        limit_sql.push_str(&format!(" LIMIT {}", lim));
        if let Some(off) = req.window_offset {
            limit_sql.push_str(&format!(" OFFSET {}", off));
        }
    }

    let data_sql = format!(
        "SELECT * FROM `{}`{}{}{}",
        req.table_name, where_sql, order_sql, limit_sql
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

    let mut stream = sqlx::query(&data_sql).fetch(&db_client.pool);
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

        let mut map = serde_json::Map::new();
        for col in row.columns() {
            let col_name = col.name().to_string();
            if let Ok(val) = row.try_get::<Option<i64>, _>(col.ordinal()) {
                map.insert(col_name, serde_json::json!(val));
            } else if let Ok(val) = row.try_get::<Option<f64>, _>(col.ordinal()) {
                map.insert(col_name, serde_json::json!(val));
            } else if let Ok(val) = row.try_get::<Option<bool>, _>(col.ordinal()) {
                map.insert(col_name, serde_json::json!(val));
            } else if let Ok(val) = row.try_get::<Option<chrono::NaiveDateTime>, _>(col.ordinal()) {
                map.insert(col_name, serde_json::json!(val.map(|dt| dt.to_string())));
            } else if let Ok(val) = row.try_get::<Option<chrono::NaiveDate>, _>(col.ordinal()) {
                map.insert(col_name, serde_json::json!(val.map(|d| d.to_string())));
            } else if let Ok(val) = row.try_get::<Option<chrono::NaiveTime>, _>(col.ordinal()) {
                map.insert(col_name, serde_json::json!(val.map(|t| t.to_string())));
            } else if let Ok(val) = row.try_get::<Option<String>, _>(col.ordinal()) {
                map.insert(col_name, serde_json::json!(val));
            } else {
                let val: Option<Vec<u8>> = row.try_get(col.ordinal()).unwrap_or(None);
                if let Some(bytes) = val {
                    let s = String::from_utf8_lossy(&bytes).into_owned();
                    map.insert(col_name, serde_json::json!(s));
                } else {
                    map.insert(col_name, serde_json::Value::Null);
                }
            }
        }

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

async fn run_import_job(
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

        match query.execute(&db_client.pool).await {
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

async fn run_import_sql_job(
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
        .execute(&db_client.pool)
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;

    Ok(serde_json::json!({
        "affected_rows": result.rows_affected()
    }))
}

async fn get_temp_db_client(state: &AppState, db_id: &str) -> Result<(DbClient, String), AppError> {
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

    let client = DbClient::new(&conn.url)
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

const GAP_TOO_LARGE_MSG: &str = "当前对比数据库差距过大，不符合结构/数据同步规范/数据传输规范";

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
    let (source_client, source_db_name) = get_temp_db_client(&state, &req.source_db_id).await?;
    let (target_client, target_db_name) = get_temp_db_client(&state, &req.target_db_id).await?;

    let source = fetch_schema_for_db(&source_client, &source_db_name)
        .await
        .ok_or_else(|| AppError::InternalError("Failed to fetch source schema".to_string()))?;
    let target = fetch_schema_for_db(&target_client, &target_db_name)
        .await
        .ok_or_else(|| AppError::InternalError("Failed to fetch target schema".to_string()))?;

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
    let (source_client, source_db_name) = get_temp_db_client(&state, &req.source_db_id).await?;
    let (target_client, target_db_name) = get_temp_db_client(&state, &req.target_db_id).await?;

    let source = fetch_schema_for_db(&source_client, &source_db_name)
        .await
        .ok_or_else(|| AppError::InternalError("Failed to fetch source schema".to_string()))?;
    let target = fetch_schema_for_db(&target_client, &target_db_name)
        .await
        .ok_or_else(|| AppError::InternalError("Failed to fetch target schema".to_string()))?;
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
    use sqlx::Column;
    use sqlx::Row;

    let data_sql = format!("SELECT * FROM `{}` LIMIT 50000", table_name);
    let result_rows = sqlx::query(&data_sql)
        .fetch_all(&db_client.pool)
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;

    let mut rows = Vec::new();
    for row in result_rows {
        let mut map = serde_json::Map::new();
        for col in row.columns() {
            let col_name = col.name().to_string();
            if let Ok(val) = row.try_get::<Option<i64>, _>(col.ordinal()) {
                map.insert(col_name, serde_json::json!(val));
            } else if let Ok(val) = row.try_get::<Option<f64>, _>(col.ordinal()) {
                map.insert(col_name, serde_json::json!(val));
            } else if let Ok(val) = row.try_get::<Option<bool>, _>(col.ordinal()) {
                map.insert(col_name, serde_json::json!(val));
            } else if let Ok(val) = row.try_get::<Option<chrono::NaiveDateTime>, _>(col.ordinal()) {
                map.insert(col_name, serde_json::json!(val.map(|dt| dt.to_string())));
            } else if let Ok(val) = row.try_get::<Option<chrono::NaiveDate>, _>(col.ordinal()) {
                map.insert(col_name, serde_json::json!(val.map(|d| d.to_string())));
            } else if let Ok(val) = row.try_get::<Option<chrono::NaiveTime>, _>(col.ordinal()) {
                map.insert(col_name, serde_json::json!(val.map(|t| t.to_string())));
            } else if let Ok(val) = row.try_get::<Option<String>, _>(col.ordinal()) {
                map.insert(col_name, serde_json::json!(val));
            } else {
                let val: Option<Vec<u8>> = row.try_get(col.ordinal()).unwrap_or(None);
                if let Some(bytes) = val {
                    let s = String::from_utf8_lossy(&bytes).into_owned();
                    map.insert(col_name, serde_json::json!(s));
                } else {
                    map.insert(col_name, serde_json::Value::Null);
                }
            }
        }
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

#[derive(Deserialize)]
struct MySqlSyncCompareRequest {
    source_db_id: String,
    target_db_id: String,
    table_name: String,
    primary_key: String,
    mode: SyncMode,
    chunk_size: Option<usize>,
}

#[derive(Deserialize)]
struct MySqlSyncPreviewRequest {
    job_id: String,
    max_rows: Option<usize>,
    actions: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct MySqlSyncDeployRequest {
    job_id: String,
}

#[derive(Serialize)]
struct MySqlSyncJobStartResponse {
    job_id: String,
}

async fn mysql_sync_compare(
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

async fn mysql_sync_preview(
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

async fn mysql_sync_deploy(
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

async fn mysql_sync_job_status(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Result<Json<MySqlSyncJob>, AppError> {
    let job = { state.sync_jobs.read().await.get(&job_id).cloned() };
    job.map(Json)
        .ok_or_else(|| AppError::NotFound("job not found".to_string()))
}

async fn update_mysql_sync_job(state: &AppState, job_id: &str, f: impl FnOnce(&mut MySqlSyncJob)) {
    let mut jobs = state.sync_jobs.write().await;
    if let Some(job) = jobs.get_mut(job_id) {
        f(job);
    }
}

#[derive(Serialize)]
struct PerfSyncJobStartResponse {
    job_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PerfSyncCheckRequest {
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
struct PerfSyncCheckResponse {
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

async fn perf_sync_check(
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

async fn fetch_table_count(db: &DbClient, table: &str) -> Result<u64, AppError> {
    let policy = TimeoutPolicy::default();
    let sql = format!("SELECT COUNT(*) FROM `{}`", table);
    let fut = sqlx::query_scalar::<_, i64>(&sql).fetch_one(&db.pool);
    let v = tokio::time::timeout(policy.db_query, fut)
        .await
        .map_err(|_| AppError::Timeout(format!("统计表 {} 行数超时", table)))?
        .map_err(|e| AppError::InternalError(e.to_string()))?;
    Ok(v.max(0) as u64)
}

async fn fetch_counts(
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

async fn run_sync_table(
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

async fn verify_table(
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

async fn perf_sync_start(
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

async fn run_perf_sync_job(
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

async fn perf_sync_job_status(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Result<Json<PerfSyncJob>, AppError> {
    let job = { state.perf_sync_jobs.read().await.get(&job_id).cloned() };
    job.map(Json)
        .ok_or_else(|| AppError::NotFound("job not found".to_string()))
}

async fn update_perf_sync_job(state: &AppState, job_id: &str, f: impl FnOnce(&mut PerfSyncJob)) {
    let mut jobs = state.perf_sync_jobs.write().await;
    if let Some(job) = jobs.get_mut(job_id) {
        f(job);
    }
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
    let db_client = state
        .db_client
        .read()
        .await
        .clone()
        .ok_or_else(|| AppError::BadRequest("Database not connected".to_string()))?;

    let explain_sql = format!("EXPLAIN {}", req.sql);
    use sqlx::Column;
    use sqlx::Row;

    let result_rows = sqlx::query(&explain_sql)
        .fetch_all(&db_client.pool)
        .await
        .map_err(|e| AppError::InternalError(e.to_string()))?;

    let mut rows = Vec::new();
    for row in result_rows {
        let mut map = serde_json::Map::new();
        for col in row.columns() {
            let col_name = col.name().to_string();

            if let Ok(val) = row.try_get::<Option<i64>, _>(col.ordinal()) {
                map.insert(col_name, serde_json::json!(val));
            } else if let Ok(val) = row.try_get::<Option<f64>, _>(col.ordinal()) {
                map.insert(col_name, serde_json::json!(val));
            } else if let Ok(val) = row.try_get::<Option<String>, _>(col.ordinal()) {
                map.insert(col_name, serde_json::json!(val));
            } else {
                let val: Option<Vec<u8>> = row.try_get(col.ordinal()).unwrap_or(None);
                if let Some(bytes) = val {
                    let s = String::from_utf8_lossy(&bytes).into_owned();
                    map.insert(col_name, serde_json::json!(s));
                } else {
                    map.insert(col_name, serde_json::Value::Null);
                }
            }
        }
        rows.push(serde_json::Value::Object(map));
    }

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
        .fetch_one(&db_client.pool),
    )
    .await
    .map_err(|_| AppError::InternalError("Session info query timed out".to_string()))?
    .map_err(|e| AppError::InternalError(e.to_string()))?;

    let session_map = fetch_mysql_variable_map(
        &db_client.pool,
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
        &db_client.pool,
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
        let gateway = AiGateway::new(config.clone());
        let planner = Planner::new(gateway);
        let limits = RuntimeLimits {
            temp_dir: format!("/tmp/local-ai-sql-test-{}", uuid::Uuid::new_v4()),
            ..Default::default()
        };

        AppState {
            config: Arc::new(RwLock::new(config)),
            db_client: Arc::new(RwLock::new(None)),
            db_client_cache: Arc::new(RwLock::new(HashMap::new())),
            planner: Arc::new(RwLock::new(planner)),
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
        }
    }

    fn test_app(state: AppState) -> Router {
        let api = Router::new()
            .route("/config", get(get_config))
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
            .route("/sql/session-info", get(session_info))
            .route("/tools/schema-sync/diff", post(sync_schema_diff))
            .route("/tools/data-transfer/execute", post(transfer_execute))
            .route("/tools/mysql-sync/compare", post(mysql_sync_compare))
            .route("/tools/mysql-sync/preview", post(mysql_sync_preview))
            .route("/tools/mysql-sync/deploy", post(mysql_sync_deploy))
            .route("/tools/mysql-sync/jobs/:job_id", get(mysql_sync_job_status))
            .route("/tools/perf-sync/start", post(perf_sync_start))
            .route("/tools/perf-sync/check", post(perf_sync_check))
            .route("/tools/perf-sync/jobs/:job_id", get(perf_sync_job_status))
            .route("/tools/jobs/go-live/start", post(go_live_job_start))
            .route("/tools/go-live/reports", get(go_live_reports_list))
            .route("/tools/go-live/audit", get(go_live_audit_list))
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
                db_type: Some(DbType::MySQL),
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
                    .uri("/backend/tools/mysql-sync/compare")
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
                    .uri("/backend/tools/mysql-sync/jobs/not-exist")
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
}
