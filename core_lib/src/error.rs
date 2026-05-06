use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("Database connection error: {0}")]
    DbConnectionError(String),
    #[error("SQL syntax error: {0}")]
    SqlSyntaxError(String),
    #[error("AI agent timeout or error: {0}")]
    AiAgentTimeout(String),
    #[error("AI rate limited: {0}")]
    AiRateLimited(String),
    #[error("AI authentication failed: {0}")]
    AiAuth(String),
    #[error("AI forbidden: {0}")]
    AiForbidden(String),
    #[error("AI model not found: {0}")]
    AiModelNotFound(String),
    #[error("AI proxy error: {0}")]
    AiProxy(String),
    #[error("External service unavailable: {0}")]
    ExternalServiceUnavailable(String),
    #[error("Not found: {0}")]
    NotFound(String),
    #[error("Unauthorized: {0}")]
    Unauthorized(String),
    #[error("Bad request: {0}")]
    BadRequest(String),
    #[error("Payload too large: {0}")]
    PayloadTooLarge(String),
    #[error("Resource limit exceeded: {0}")]
    ResourceLimit(String),
    #[error("Too many requests: {0}")]
    TooManyRequests(String),
    #[error("Internal server error: {0}")]
    InternalError(String),
    #[error("Parse error: {0}")]
    ParseError(String),
    #[error("Forbidden: {0}")]
    Forbidden(String),
    #[error("Timeout: {0}")]
    Timeout(String),
    #[error("Canceled: {0}")]
    Canceled(String),
}

#[derive(Serialize)]
pub struct ErrorResponse {
    pub success: bool,
    pub code: String,
    #[serde(rename = "type")]
    pub r#type: String,
    pub message: String,
    pub details: String,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, code, ty, message, details) = match &self {
            AppError::DbConnectionError(msg) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "ERR_DB_CONNECTION",
                "db",
                "Database connection failed",
                msg.clone(),
            ),
            AppError::SqlSyntaxError(msg) => (
                StatusCode::BAD_REQUEST,
                "ERR_SQL_SYNTAX",
                "validation",
                "SQL syntax is invalid",
                msg.clone(),
            ),
            AppError::AiAgentTimeout(msg) => (
                StatusCode::GATEWAY_TIMEOUT,
                "ERR_AI_TIMEOUT",
                "timeout",
                "AI agent timeout or error",
                msg.clone(),
            ),
            AppError::AiRateLimited(msg) => (
                StatusCode::TOO_MANY_REQUESTS,
                "ERR_AI_RATE_LIMITED",
                "rate_limit",
                "AI rate limited",
                msg.clone(),
            ),
            AppError::AiAuth(msg) => (
                StatusCode::UNAUTHORIZED,
                "ERR_AI_AUTH",
                "auth",
                "AI authentication failed",
                msg.clone(),
            ),
            AppError::AiForbidden(msg) => (
                StatusCode::FORBIDDEN,
                "ERR_AI_FORBIDDEN",
                "auth",
                "AI access forbidden",
                msg.clone(),
            ),
            AppError::AiModelNotFound(msg) => (
                StatusCode::NOT_FOUND,
                "ERR_AI_MODEL_NOT_FOUND",
                "not_found",
                "AI model not found",
                msg.clone(),
            ),
            AppError::AiProxy(msg) => (
                StatusCode::BAD_GATEWAY,
                "ERR_AI_PROXY",
                "proxy",
                "AI proxy error",
                msg.clone(),
            ),
            AppError::ExternalServiceUnavailable(msg) => (
                StatusCode::BAD_GATEWAY,
                "ERR_EXTERNAL_UNAVAILABLE",
                "network",
                "External service unavailable",
                msg.clone(),
            ),
            AppError::NotFound(msg) => (
                StatusCode::NOT_FOUND,
                "ERR_NOT_FOUND",
                "not_found",
                "Resource not found",
                msg.clone(),
            ),
            AppError::Unauthorized(msg) => (
                StatusCode::UNAUTHORIZED,
                "ERR_UNAUTHORIZED",
                "auth",
                "Unauthorized access",
                msg.clone(),
            ),
            AppError::BadRequest(msg) => (
                StatusCode::BAD_REQUEST,
                "ERR_BAD_REQUEST",
                "validation",
                "Bad request parameters",
                msg.clone(),
            ),
            AppError::PayloadTooLarge(msg) => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "ERR_PAYLOAD_TOO_LARGE",
                "resource_limit",
                "Payload too large",
                msg.clone(),
            ),
            AppError::ResourceLimit(msg) => (
                StatusCode::INSUFFICIENT_STORAGE,
                "ERR_RESOURCE_LIMIT",
                "resource_limit",
                "Resource limit exceeded",
                msg.clone(),
            ),
            AppError::TooManyRequests(msg) => (
                StatusCode::TOO_MANY_REQUESTS,
                "ERR_CONCURRENCY_LIMIT",
                "resource_limit",
                "Too many requests",
                msg.clone(),
            ),
            AppError::ParseError(msg) => (
                StatusCode::BAD_REQUEST,
                "ERR_PARSE",
                "validation",
                "Failed to parse data",
                msg.clone(),
            ),
            AppError::Forbidden(msg) => (
                StatusCode::FORBIDDEN,
                "ERR_FORBIDDEN",
                "auth",
                "Access forbidden",
                msg.clone(),
            ),
            AppError::Timeout(msg) => (
                StatusCode::GATEWAY_TIMEOUT,
                "ERR_TIMEOUT",
                "timeout",
                "Request timeout",
                msg.clone(),
            ),
            AppError::Canceled(msg) => (
                StatusCode::CONFLICT,
                "ERR_CANCELED",
                "canceled",
                "Request canceled",
                msg.clone(),
            ),
            AppError::InternalError(msg) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "ERR_INTERNAL",
                "internal",
                "Internal server error",
                msg.clone(),
            ),
        };

        let message = redact_sensitive(message);
        let details = redact_sensitive(&details);

        let body = Json(ErrorResponse {
            success: false,
            code: code.to_string(),
            r#type: ty.to_string(),
            message,
            details,
        });

        (status, body).into_response()
    }
}

// Convert from other error types if convenient
impl From<sqlx::Error> for AppError {
    fn from(err: sqlx::Error) -> Self {
        AppError::InternalError(err.to_string())
    }
}

impl From<reqwest::Error> for AppError {
    fn from(err: reqwest::Error) -> Self {
        if err.is_timeout() {
            return AppError::Timeout(err.to_string());
        }
        if err.is_connect() {
            return AppError::ExternalServiceUnavailable(err.to_string());
        }
        AppError::InternalError(err.to_string())
    }
}

fn redact_sensitive(input: &str) -> String {
    let mut out = input.to_string();

    let mut search_start = 0usize;
    while let Some(rel_idx) = out[search_start..].find("Bearer ") {
        let idx = search_start + rel_idx;
        let start = idx + "Bearer ".len();
        let end = out[start..]
            .find(|c: char| {
                c.is_whitespace() || matches!(c, '"' | '\'' | ',' | ')' | ';')
            })
            .map(|i| start + i)
            .unwrap_or(out.len());
        out.replace_range(start..end, "******");
        search_start = start + "******".len();
    }

    out = redact_kv_like(&out, "api_key");
    out = redact_kv_like(&out, "apiKey");
    out = redact_kv_like(&out, "password");
    out = redact_url_passwords(&out);

    out
}

fn redact_kv_like(input: &str, key: &str) -> String {
    let mut out = input.to_string();
    let mut offset = 0usize;
    while let Some(pos) = out[offset..].find(key) {
        let key_pos = offset + pos;
        let after_key = key_pos + key.len();
        let sep_pos = out[after_key..]
            .find(|c: char| [':', '='].contains(&c))
            .map(|i| after_key + i);
        let Some(sep_pos) = sep_pos else {
            offset = after_key;
            continue;
        };
        let mut val_start = sep_pos + 1;
        while val_start < out.len() && out.as_bytes()[val_start].is_ascii_whitespace() {
            val_start += 1;
        }
        if val_start >= out.len() {
            break;
        }
        if out.as_bytes()[val_start] == b'"' {
            val_start += 1;
            let val_end = out[val_start..]
                .find('"')
                .map(|i| val_start + i)
                .unwrap_or(out.len());
            out.replace_range(val_start..val_end, "******");
            offset = val_end;
            continue;
        }
        let val_end = out[val_start..]
            .find(|c: char| c.is_whitespace() || c == ',' || c == '&' || c == ')' || c == ';' || c == '"')
            .map(|i| val_start + i)
            .unwrap_or(out.len());
        out.replace_range(val_start..val_end, "******");
        offset = val_end;
    }
    out
}

fn redact_url_passwords(input: &str) -> String {
    let mut out = input.to_string();
    let mut offset = 0usize;
    while let Some(pos) = out[offset..].find("://") {
        let scheme_sep = offset + pos + 3;
        let Some(at_rel) = out[scheme_sep..].find('@') else {
            offset = scheme_sep;
            continue;
        };
        let at_idx = scheme_sep + at_rel;
        let userinfo = &out[scheme_sep..at_idx];
        let Some(colon_rel) = userinfo.find(':') else {
            offset = at_idx + 1;
            continue;
        };
        let pass_start = scheme_sep + colon_rel + 1;
        out.replace_range(pass_start..at_idx, "******");
        offset = pass_start + "******".len() + 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;
    use serde_json::Value;

    #[tokio::test]
    async fn error_response_redacts_tokens_and_passwords() {
        let err = AppError::InternalError(
            "Authorization: Bearer sk-abc123 mysql://u:p@127.0.0.1/db api_key=kk password=\"pp\""
                .to_string(),
        );
        let resp = err.into_response();
        let body = resp.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        let details = v.get("details").and_then(|d| d.as_str()).unwrap();
        assert!(!details.contains("sk-abc123"));
        assert!(!details.contains("mysql://u:p@"));
        assert!(!details.contains("api_key=kk"));
        assert!(!details.contains("password=\"pp\""));
        assert!(details.contains("Bearer ******"));
        assert!(details.contains("mysql://u:******@"));
    }
}
