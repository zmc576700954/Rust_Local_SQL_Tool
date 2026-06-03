/** AI gateway types — errors, messages, and health reports. */

use crate::config::{AiConnectionMode, AiProvider};
use reqwest::Error as ReqwestError;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AiError {
    #[error("Network error: {0}")]
    Network(#[from] ReqwestError),
    #[error("No tokens available in pool")]
    NoTokens,
    #[error("AI auth failed: {0}")]
    Auth(String),
    #[error("AI forbidden: {0}")]
    Forbidden(String),
    #[error("AI model not found: {0}")]
    ModelNotFound(String),
    #[error("AI rate limited: {0}")]
    RateLimited(String),
    #[error("AI server error: {0}")]
    ServerError(String),
    #[error("API returned an error: {0}")]
    ApiError(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiHealthReport {
    pub ok: bool,
    pub active_ai_profile_id: Option<String>,
    pub provider: AiProvider,
    pub mode: AiConnectionMode,
    pub endpoint: String,
    pub model_id: String,
    pub tier: String,
    pub latency_ms: Option<u128>,
    pub result_preview: Option<String>,
}
