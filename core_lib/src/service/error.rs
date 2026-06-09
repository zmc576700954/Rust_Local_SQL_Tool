//! ServiceError — 统一错误类型（不依赖 axum 或 tauri）
//!
//! web-server 将 ServiceError 映射为 AppError（axum IntoResponse）
//! src-tauri 将 ServiceError 映射为 String（Tauri command 返回值）

use std::fmt;

/// Desktop 回退标记前缀
pub const DESKTOP_HTTP_FALLBACK_PREFIX: &str = "DESKTOP_HTTP_FALLBACK:";

/// 检查字符串是否为 Desktop HTTP 回退信号
pub fn is_desktop_fallback(err_str: &str) -> bool {
    err_str.starts_with(DESKTOP_HTTP_FALLBACK_PREFIX)
}

/// 提取 Desktop 回退的原因
pub fn desktop_fallback_reason(err_str: &str) -> Option<String> {
    err_str.strip_prefix(DESKTOP_HTTP_FALLBACK_PREFIX)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// 包装 Desktop 回退错误
pub fn wrap_desktop_fallback(reason: String) -> String {
    format!("{DESKTOP_HTTP_FALLBACK_PREFIX}{reason}")
}

/// 统一 Service 层错误类型
///
/// 不依赖 axum 或 tauri，保持 pure。
/// web-server 和 src-tauri 各自实现映射。
#[derive(Debug)]
pub enum ServiceError {
    BadRequest(String),
    NotFound(String),
    Forbidden(String),
    InternalError(String),
    Timeout(String),
    AiAuth(String),
    AiForbidden(String),
    AiModelNotFound(String),
    AiRateLimited(String),
    AiAgentTimeout(String),
    AiProxy(String),
    ExternalServiceUnavailable(String),
    ResourceLimit(String),
    PayloadTooLarge(String),
    ParseError(String),
    /// 桌面端不支持此功能，需回退到 HTTP
    DesktopFallbackRequired(String),
    ConfigInvalid(String),
    DbConnection(String),
    DbQuery(String),
    DbWrite(String),
    Transaction(String),
    Sync(String),
    Cache(String),
}

impl fmt::Display for ServiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadRequest(msg) => write!(f, "BadRequest: {msg}"),
            Self::NotFound(msg) => write!(f, "NotFound: {msg}"),
            Self::Forbidden(msg) => write!(f, "Forbidden: {msg}"),
            Self::InternalError(msg) => write!(f, "InternalError: {msg}"),
            Self::Timeout(msg) => write!(f, "Timeout: {msg}"),
            Self::AiAuth(msg) => write!(f, "AiAuth: {msg}"),
            Self::AiForbidden(msg) => write!(f, "AiForbidden: {msg}"),
            Self::AiModelNotFound(msg) => write!(f, "AiModelNotFound: {msg}"),
            Self::AiRateLimited(msg) => write!(f, "AiRateLimited: {msg}"),
            Self::AiAgentTimeout(msg) => write!(f, "AiAgentTimeout: {msg}"),
            Self::AiProxy(msg) => write!(f, "AiProxy: {msg}"),
            Self::ExternalServiceUnavailable(msg) => write!(f, "ExternalServiceUnavailable: {msg}"),
            Self::ResourceLimit(msg) => write!(f, "ResourceLimit: {msg}"),
            Self::PayloadTooLarge(msg) => write!(f, "PayloadTooLarge: {msg}"),
            Self::ParseError(msg) => write!(f, "ParseError: {msg}"),
            Self::DesktopFallbackRequired(msg) => write!(f, "{DESKTOP_HTTP_FALLBACK_PREFIX}{msg}"),
            Self::ConfigInvalid(msg) => write!(f, "ConfigInvalid: {msg}"),
            Self::DbConnection(msg) => write!(f, "DbConnection: {msg}"),
            Self::DbQuery(msg) => write!(f, "DbQuery: {msg}"),
            Self::DbWrite(msg) => write!(f, "DbWrite: {msg}"),
            Self::Transaction(msg) => write!(f, "Transaction: {msg}"),
            Self::Sync(msg) => write!(f, "Sync: {msg}"),
            Self::Cache(msg) => write!(f, "Cache: {msg}"),
        }
    }
}

impl std::error::Error for ServiceError {}

impl ServiceError {
    /// HTTP 状态码映射（web-server 使用）
    pub fn http_status(&self) -> u16 {
        match self {
            Self::BadRequest(_) => 400,
            Self::NotFound(_) => 404,
            Self::Forbidden(_) => 403,
            Self::InternalError(_) => 500,
            Self::Timeout(_) => 504,
            Self::AiAuth(_) => 401,
            Self::AiForbidden(_) => 403,
            Self::AiModelNotFound(_) => 404,
            Self::AiRateLimited(_) => 429,
            Self::AiAgentTimeout(_) => 504,
            Self::AiProxy(_) => 502,
            Self::ExternalServiceUnavailable(_) => 503,
            Self::ResourceLimit(_) => 429,
            Self::PayloadTooLarge(_) => 413,
            Self::ParseError(_) => 400,
            Self::DesktopFallbackRequired(_) => 501,
            Self::ConfigInvalid(_) => 400,
            Self::DbConnection(_) => 503,
            Self::DbQuery(_) => 500,
            Self::DbWrite(_) => 500,
            Self::Transaction(_) => 409,
            Self::Sync(_) => 500,
            Self::Cache(_) => 500,
        }
    }

    /// 判断是否需要重试
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Timeout(_) | Self::AiRateLimited(_) | Self::ExternalServiceUnavailable(_) | Self::AiAgentTimeout(_)
        )
    }

    /// 判断是否为 DesktopFallbackRequired
    pub fn is_desktop_fallback(&self) -> bool {
        matches!(self, Self::DesktopFallbackRequired(_))
    }

    /// 映射到 AppError（web-server 使用）
    ///
    /// 此方法仅在 web-server 的 handler 中使用。
    /// DesktopFallbackRequired 在 web-server 中不应出现。
    pub fn to_app_error(&self) -> crate::error::AppError {
        match self {
            Self::DesktopFallbackRequired(_) => {
                // web-server 本身就是 HTTP 服务，不需要回退
                crate::error::AppError::InternalError("DesktopFallbackRequired should not reach web-server".into())
            }
            Self::BadRequest(msg) => crate::error::AppError::BadRequest(msg.clone()),
            Self::NotFound(msg) => crate::error::AppError::NotFound(msg.clone()),
            Self::Forbidden(msg) => crate::error::AppError::Forbidden(msg.clone()),
            Self::InternalError(msg) => crate::error::AppError::InternalError(msg.clone()),
            Self::Timeout(msg) => crate::error::AppError::Timeout(msg.clone()),
            Self::AiAuth(msg) => crate::error::AppError::AiAuth(msg.clone()),
            Self::AiForbidden(msg) => crate::error::AppError::AiForbidden(msg.clone()),
            Self::AiModelNotFound(msg) => crate::error::AppError::AiModelNotFound(msg.clone()),
            Self::AiRateLimited(msg) => crate::error::AppError::AiRateLimited(msg.clone()),
            Self::AiAgentTimeout(msg) => crate::error::AppError::AiAgentTimeout(msg.clone()),
            Self::AiProxy(msg) => crate::error::AppError::AiProxy(msg.clone()),
            Self::ExternalServiceUnavailable(msg) => crate::error::AppError::ExternalServiceUnavailable(msg.clone()),
            Self::ResourceLimit(msg) => crate::error::AppError::ResourceLimit(msg.clone()),
            Self::PayloadTooLarge(msg) => crate::error::AppError::PayloadTooLarge(msg.clone()),
            Self::ParseError(msg) => crate::error::AppError::ParseError(msg.clone()),
            Self::ConfigInvalid(msg) => crate::error::AppError::BadRequest(msg.clone()),
            Self::DbConnection(msg) => crate::error::AppError::DbConnectionError(msg.clone()),
            Self::DbQuery(msg) => crate::error::AppError::DbQueryError { message: msg.clone(), code: None },
            Self::DbWrite(msg) => crate::error::AppError::DbQueryError { message: msg.clone(), code: None },
            Self::Transaction(msg) => crate::error::AppError::BadRequest(msg.clone()),
            Self::Sync(msg) => crate::error::AppError::InternalError(msg.clone()),
            Self::Cache(msg) => crate::error::AppError::InternalError(msg.clone()),
        }
    }

    /// 映射为 Tauri command 返回的错误字符串
    pub fn to_tauri_error(&self) -> String {
        match self {
            Self::DesktopFallbackRequired(reason) => wrap_desktop_fallback(reason.clone()),
            other => other.to_string(),
        }
    }
}

// ── From sqlx::Error ──────────────────────────────────

impl From<sqlx::Error> for ServiceError {
    fn from(err: sqlx::Error) -> Self {
        match &err {
            sqlx::Error::Database(db_err) => {
                let msg = db_err.message().to_string();
                Self::DbQuery(msg)
            }
            sqlx::Error::PoolTimedOut => Self::Timeout("DB pool timed out".into()),
            sqlx::Error::RowNotFound => Self::NotFound("Row not found".into()),
            sqlx::Error::Io(_) | sqlx::Error::PoolClosed => Self::DbConnection(err.to_string()),
            _ => Self::InternalError(err.to_string()),
        }
    }
}

impl From<crate::db::DbError> for ServiceError {
    fn from(err: crate::db::DbError) -> Self {
        match err {
            crate::db::DbError::Sqlx(e) => Self::from(e),
            crate::db::DbError::Timeout => Self::Timeout("Database timeout".into()),
            crate::db::DbError::MissingUrl => Self::BadRequest("Connection string missing".into()),
            crate::db::DbError::MissingData(msg) => Self::BadRequest(msg),
            crate::db::DbError::Unsupported(msg) => Self::BadRequest(msg),
            crate::db::DbError::Security(msg) => Self::BadRequest(msg),
        }
    }
}

impl From<serde_json::Error> for ServiceError {
    fn from(err: serde_json::Error) -> Self {
        Self::ParseError(err.to_string())
    }
}

impl From<std::io::Error> for ServiceError {
    fn from(err: std::io::Error) -> Self {
        Self::InternalError(err.to_string())
    }
}

// ── From AgentError ────────────────────────────────────

impl ServiceError {
    /// 从 AgentError 映射为 ServiceError
    ///
    /// 保留 AI 错误的语义分类，供 web-server 映射为 AppError、src-tauri 映射为 String。
    pub fn from_agent_error(e: crate::ai::agent::AgentError) -> Self {
        match e {
            crate::ai::agent::AgentError::MissingApiKey => Self::AiAuth("Missing API key. Please configure your AI token.".to_string()),
            crate::ai::agent::AgentError::NoTokens => Self::BadRequest("No tokens available in pool".to_string()),
            crate::ai::agent::AgentError::Auth(msg) => Self::AiAuth(msg),
            crate::ai::agent::AgentError::Forbidden(msg) => Self::AiForbidden(msg),
            crate::ai::agent::AgentError::ModelNotFound(msg) => Self::AiModelNotFound(msg),
            crate::ai::agent::AgentError::RateLimited(msg) => Self::AiRateLimited(msg),
            crate::ai::agent::AgentError::ServerError(msg) => Self::ExternalServiceUnavailable(msg),
            crate::ai::agent::AgentError::Network(msg) => {
                let lower = msg.to_lowercase();
                if lower.contains("timeout") {
                    Self::AiAgentTimeout(msg)
                } else if lower.contains("proxy") || lower.contains("tunnel") {
                    Self::AiProxy(msg)
                } else if lower.contains("connection") || lower.contains("connect") {
                    Self::ExternalServiceUnavailable(msg)
                } else {
                    Self::InternalError(msg)
                }
            }
            crate::ai::agent::AgentError::Agent(msg) => {
                let lower = msg.to_lowercase();
                if lower.contains("timeout") {
                    Self::AiAgentTimeout(msg)
                } else {
                    Self::InternalError(msg)
                }
            }
        }
    }
}

impl From<crate::ai::agent::AgentError> for ServiceError {
    fn from(e: crate::ai::agent::AgentError) -> Self {
        Self::from_agent_error(e)
    }
}