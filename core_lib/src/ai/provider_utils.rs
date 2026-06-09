//! Provider-level utilities that don't fit inside the rig-core agent framework.
//!
//! - `health_check`: pings the AI endpoint with a minimal request to verify connectivity
//! - `fetch_provider_models`: lists models from a provider's API (not supported by rig)
//! - `validate_ai_url`: SSRF protection for relay/custom URLs

use crate::ai::agent::AgentError;
use crate::ai::events::AiHealthReport;
use crate::config::{AiConnectionMode, AiProvider, AppConfig, ResolvedAiProfile};
use crate::timeout_policy::TimeoutPolicy;
use reqwest::Client;
use serde_json::Value;
use std::time::Duration;

/// Validate that an AI endpoint URL is safe to connect to.
/// Blocks SSRF attacks by rejecting private/internal network addresses
/// and non-HTTP schemes.
pub fn validate_ai_url(url: &str) -> Result<(), AgentError> {
    let parsed = url::Url::parse(url).map_err(|e| {
        AgentError::Agent(format!("Invalid URL: {}", e))
    })?;

    match parsed.scheme() {
        "http" | "https" => {}
        _ => {
            return Err(AgentError::Agent(format!(
                "Unsupported URL scheme: {} (only http/https allowed)",
                parsed.scheme()
            )))
        }
    }

    if let Some(host) = parsed.host_str() {
        if host == "localhost" {
            return Err(AgentError::Agent(
                "SSRF protection: requests to localhost are blocked".into(),
            ));
        }

        if let Ok(ip) = host.parse::<std::net::IpAddr>() {
            match ip {
                std::net::IpAddr::V4(v4) => {
                    if v4.is_private() || v4.is_loopback() || v4.is_link_local() {
                        return Err(AgentError::Agent(
                            "SSRF protection: requests to private/internal network addresses are blocked"
                                .into(),
                        ));
                    }
                }
                std::net::IpAddr::V6(v6) => {
                    if v6.is_loopback() || v6.is_unicast_link_local() {
                        return Err(AgentError::Agent(
                            "SSRF protection: requests to private/internal IPv6 addresses are blocked"
                                .into(),
                        ));
                    }
                }
            }
        }
    }

    Ok(())
}

/// Resolve the default endpoint URL for a given provider.
fn resolve_default_endpoint(provider: &AiProvider) -> String {
    match provider {
        AiProvider::Openai => "https://api.openai.com/v1/chat/completions",
        AiProvider::Deepseek => "https://api.deepseek.com/chat/completions",
        AiProvider::Moonshot => "https://api.moonshot.ai/v1/chat/completions",
        AiProvider::Zhipu => "https://open.bigmodel.cn/api/paas/v4/chat/completions",
        AiProvider::Anthropic => "https://api.anthropic.com/v1/messages",
        AiProvider::Custom => "https://api.openai.com/v1/chat/completions",
    }
    .to_string()
}

/// Resolve the active endpoint URL from the profile config.
fn resolve_endpoint(profile: &ResolvedAiProfile, ssrf_check: bool) -> Result<String, AgentError> {
    let default_url = resolve_default_endpoint(&profile.provider);
    let url = match profile.mode {
        AiConnectionMode::Direct => default_url,
        AiConnectionMode::Relay | AiConnectionMode::LocalRelay | AiConnectionMode::Pool => {
            profile.relay_url.clone().unwrap_or(default_url)
        }
    };
    if ssrf_check {
        validate_ai_url(&url)?;
    }
    Ok(url)
}

/// Health check: sends a minimal "ping" request to the active AI endpoint
/// and returns a report with latency measurement.
pub async fn health_check(config: &AppConfig) -> Result<AiHealthReport, AgentError> {
    let profile = config.resolve_ai_profile();
    let _api_key = profile.api_key.as_deref().ok_or(AgentError::MissingApiKey)?;
    let (model_id, model) = config.resolve_active_model();
    let tier = if model.as_ref().map(|m| m.supports_tier).unwrap_or(true) {
        config.active_tier.clone()
    } else {
        "balanced".to_string()
    };

    let endpoint = resolve_endpoint(&profile, !cfg!(test))?;

    // Use chat_completion_raw for the ping — simple preamble, no tools
    let start = std::time::Instant::now();

    let preamble = "You are a health check probe. Reply with a short single sentence.";
    let response = crate::ai::agent::chat_completion_raw(config, preamble, "ping").await?;
    let latency_ms = start.elapsed().as_millis();

    Ok(AiHealthReport {
        ok: true,
        active_ai_profile_id: config.active_ai_profile_id.clone(),
        provider: profile.provider,
        mode: profile.mode,
        endpoint,
        model_id,
        tier,
        latency_ms: Some(latency_ms),
        result_preview: Some(response.chars().take(200).collect()),
    })
}

/// Fetch the list of available models from a provider's API.
/// This uses direct HTTP calls since rig-core doesn't provide a models-listing API.
pub async fn fetch_provider_models(
    config: &AppConfig,
    provider: AiProvider,
    api_key: String,
    base_url: Option<String>,
) -> Result<Vec<String>, AgentError> {
    if api_key.trim().is_empty() {
        return Err(AgentError::Auth("API key is not configured".into()));
    }

    let _profile = config.resolve_ai_profile();
    let chat_url = base_url.unwrap_or_else(|| resolve_default_endpoint(&provider));
    let models_url = if provider == AiProvider::Anthropic {
        chat_url.replace("/messages", "/models")
    } else {
        chat_url.replace("/chat/completions", "/models")
    };

    if !cfg!(test) {
        validate_ai_url(&models_url)?;
    }

    let policy = TimeoutPolicy::default();
    let client = Client::builder()
        .connect_timeout(policy.external_http_connect)
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap_or_else(|_| Client::new());

    let request_builder = client.get(&models_url).timeout(Duration::from_secs(15));

    let resp = match provider {
        AiProvider::Anthropic => {
            request_builder
                .header("x-api-key", api_key)
                .header("anthropic-version", "2023-06-01")
                .send()
                .await
        }
        _ => {
            request_builder
                .header("Authorization", format!("Bearer {}", api_key))
                .send()
                .await
        }
    };

    match resp {
        Ok(response) => {
            let status = response.status();
            if status.is_success() {
                let body: Value = response.json().await.map_err(|e| AgentError::Network(e.to_string()))?;
                let mut models = Vec::new();
                if let Some(data) = body.get("data").and_then(|d| d.as_array()) {
                    for item in data {
                        if let Some(id) = item.get("id").and_then(|i| i.as_str()) {
                            models.push(id.to_string());
                        }
                    }
                } else if let Some(models_arr) = body.get("models").and_then(|d| d.as_array()) {
                    for item in models_arr {
                        if let Some(id) = item.get("id").and_then(|i| i.as_str()) {
                            models.push(id.to_string());
                        }
                    }
                }
                Ok(models)
            } else {
                let body_text = response.text().await.unwrap_or_default();
                let msg = format!("Failed to fetch models: Status {}, body: {}", status, body_text);
                match status.as_u16() {
                    401 => Err(AgentError::Auth(msg)),
                    403 => Err(AgentError::Forbidden(msg)),
                    404 => Err(AgentError::ModelNotFound(msg)),
                    429 => Err(AgentError::RateLimited(msg)),
                    s if s >= 500 => Err(AgentError::ServerError(msg)),
                    _ => Err(AgentError::Agent(msg)),
                }
            }
        }
        Err(e) => {
            if e.is_timeout() {
                Err(AgentError::Network(format!("Timeout: {}", e)))
            } else if e.is_connect() {
                Err(AgentError::Network(format!("Connection failed: {}", e)))
            } else {
                Err(AgentError::Network(e.to_string()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_ai_url_rejects_localhost() {
        assert!(validate_ai_url("http://127.0.0.1/v1/chat/completions").is_err());
        assert!(validate_ai_url("http://localhost/v1/chat/completions").is_err());
    }

    #[test]
    fn validate_ai_url_rejects_private_ip() {
        assert!(validate_ai_url("http://10.0.0.1/v1/chat/completions").is_err());
        assert!(validate_ai_url("http://192.168.1.1/v1/chat/completions").is_err());
        assert!(validate_ai_url("http://172.16.0.1/v1/chat/completions").is_err());
    }

    #[test]
    fn validate_ai_url_rejects_non_http_scheme() {
        assert!(validate_ai_url("ftp://example.com/v1").is_err());
        assert!(validate_ai_url("file:///etc/passwd").is_err());
    }

    #[test]
    fn validate_ai_url_allows_public_https() {
        assert!(validate_ai_url("https://api.openai.com/v1/chat/completions").is_ok());
        assert!(validate_ai_url("https://api.anthropic.com/v1/messages").is_ok());
    }
}