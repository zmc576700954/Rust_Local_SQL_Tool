/// DB connection test diagnostics and SSH error classification.

use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
pub struct DbTestRequest {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub db_url: Option<String>,
    pub ssl_mode: Option<String>,
    pub ssh_enabled: Option<bool>,
    pub ssh_host: Option<String>,
    pub ssh_port: Option<u16>,
    pub ssh_username: Option<String>,
    pub ssh_password: Option<String>,
    pub probe_capabilities: Option<bool>,
}

#[derive(Serialize)]
pub struct DbTestDiagnostic {
    pub status: String,
    pub category: String,
    pub code: String,
    pub message: String,
    pub hint: Option<String>,
    pub detail: Option<String>,
}

#[derive(Serialize)]
pub struct DbTestResponse {
    pub success: bool,
    pub databases: Vec<String>,
    pub diagnostic: DbTestDiagnostic,
    pub stage: String,
    pub capabilities_probed: bool,
    pub capabilities_ok: Option<bool>,
    pub server_version: Option<String>,
}

pub fn db_test_response(
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

pub fn db_test_diagnostic(
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

pub fn db_test_failed(
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

pub fn classify_db_test_connect_error(msg: &str) -> DbTestResponse {
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

pub fn classify_ssh_setup_error(msg: &str) -> DbTestResponse {
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

pub fn extract_target_host_port_from_url(db_url: &str) -> Result<(String, u16), String> {
    let parsed = url::Url::parse(db_url).map_err(|e| format!("invalid db_url: {e}"))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| "db_url missing host".to_string())?
        .to_string();
    let port = parsed.port().unwrap_or(3306);
    Ok((host, port))
}

pub fn rewrite_db_url_with_local_tunnel(db_url: &str, local_port: u16) -> Result<String, String> {
    let mut parsed = url::Url::parse(db_url).map_err(|e| format!("invalid db_url: {e}"))?;
    parsed
        .set_host(Some("127.0.0.1"))
        .map_err(|_| "failed to set tunnel host".to_string())?;
    parsed
        .set_port(Some(local_port))
        .map_err(|_| "failed to set tunnel port".to_string())?;
    Ok(parsed.to_string())
}
