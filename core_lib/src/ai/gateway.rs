use crate::config::{AiConnectionMode, AiModel, AiProvider, AppConfig, ResolvedAiProfile};
use crate::timeout_policy::TimeoutPolicy;
use reqwest::{Client, Error as ReqwestError, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::RwLock;
use tokio::time::{sleep, Instant};

type TierParams = (
    Option<f32>,
    u32,
    Option<String>,
    Option<String>,
    Option<u32>,
    Option<String>,
    Duration,
);

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

#[derive(Debug, Clone)]
struct TokenState {
    failures: u32,
    cooldown_until: Option<Instant>,
}

#[derive(Clone)]
pub struct AiGateway {
    config: Arc<AppConfig>,
    client: Client,
    current_token_idx: Arc<AtomicUsize>,
    token_state: Arc<RwLock<HashMap<String, TokenState>>>,
}

impl AiGateway {
    pub fn new(config: AppConfig) -> Self {
        let policy = TimeoutPolicy::default();
        let client = Client::builder()
            .connect_timeout(policy.external_http_connect)
            .timeout(policy.external_http_default)
            .build()
            .unwrap_or_else(|_| Client::new());

        Self {
            config: Arc::new(config),
            client,
            current_token_idx: Arc::new(AtomicUsize::new(0)),
            token_state: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn update_config(&mut self, config: AppConfig) {
        self.config = Arc::new(config);
        self.current_token_idx.store(0, Ordering::Relaxed);
        self.token_state = Arc::new(RwLock::new(HashMap::new()));
    }

    pub async fn chat_completion(&self, messages: Vec<ChatMessage>) -> Result<String, AiError> {
        self.chat_completion_internal(messages, false, None).await
    }

    pub async fn chat_completion_json(
        &self,
        messages: Vec<ChatMessage>,
    ) -> Result<String, AiError> {
        self.chat_completion_internal(messages, true, None).await
    }

    pub async fn fetch_provider_models(
        &self,
        provider: AiProvider,
        api_key: String,
        base_url: Option<String>,
    ) -> Result<Vec<String>, AiError> {
        let chat_url = base_url.unwrap_or_else(|| self.resolve_default_endpoint(&provider));
        let models_url = if provider == AiProvider::Anthropic {
            chat_url.replace("/messages", "/models")
        } else {
            chat_url.replace("/chat/completions", "/models")
        };

        let request_builder = self
            .client
            .get(&models_url)
            .timeout(Duration::from_secs(15));

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
                    let body: Value = response.json().await?;
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
                    Err(AiError::ApiError(format!(
                        "Failed to fetch models: Status {}, body: {}",
                        status, body_text
                    )))
                }
            }
            Err(e) => Err(AiError::Network(e)),
        }
    }

    pub async fn health_check(&self) -> Result<AiHealthReport, AiError> {
        let profile = self.config.resolve_ai_profile();
        let (model_id, model) = self.config.resolve_active_model();
        let tier = if model.as_ref().map(|m| m.supports_tier).unwrap_or(true) {
            self.config.active_tier.clone()
        } else {
            "balanced".to_string()
        };

        let endpoint = self.resolve_endpoint(&profile);

        let messages = vec![
            ChatMessage {
                role: "system".to_string(),
                content: "You are a health check probe. Reply with a short single sentence."
                    .to_string(),
            },
            ChatMessage {
                role: "user".to_string(),
                content: "ping".to_string(),
            },
        ];

        let start = Instant::now();
        let res = self
            .chat_completion_once(messages, false, tier.clone(), Some(16))
            .await?;
        let latency_ms = start.elapsed().as_millis();

        Ok(AiHealthReport {
            ok: true,
            active_ai_profile_id: self.config.active_ai_profile_id.clone(),
            provider: profile.provider,
            mode: profile.mode,
            endpoint,
            model_id,
            tier,
            latency_ms: Some(latency_ms),
            result_preview: Some(res.chars().take(200).collect()),
        })
    }

    fn resolve_default_endpoint(&self, provider: &AiProvider) -> String {
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

    fn resolve_endpoint(&self, profile: &ResolvedAiProfile) -> String {
        let default_url = self.resolve_default_endpoint(&profile.provider);
        match profile.mode {
            AiConnectionMode::Direct => default_url,
            AiConnectionMode::Relay | AiConnectionMode::LocalRelay | AiConnectionMode::Pool => {
                profile.relay_url.clone().unwrap_or(default_url)
            }
        }
    }

    fn tier_max_tokens(tier: &str) -> u32 {
        match tier {
            "fast" => 512,
            "balanced" => 2048,
            "high" => 4096,
            "ultra" => 8192,
            _ => 2048,
        }
    }

    fn tier_temperature(tier: &str) -> f32 {
        match tier {
            "fast" => 0.0,
            "balanced" => 0.1,
            "high" => 0.2,
            "ultra" => 0.3,
            _ => 0.1,
        }
    }

    fn tier_request_timeout(tier: &str) -> Duration {
        TimeoutPolicy::default().ai_request_timeout_for_tier(tier)
    }

    fn is_retryable(err: &AiError) -> bool {
        match err {
            AiError::RateLimited(_) | AiError::ServerError(_) => true,
            AiError::Network(e) => e.is_timeout() || e.is_connect(),
            _ => false,
        }
    }

    async fn choose_pool_token(&self, profile: &ResolvedAiProfile) -> Result<String, AiError> {
        if profile.pool.tokens.is_empty() {
            return Err(AiError::NoTokens);
        }

        let start =
            self.current_token_idx.fetch_add(1, Ordering::Relaxed) % profile.pool.tokens.len();

        let now = Instant::now();

        let states = self.token_state.read().await;
        for offset in 0..profile.pool.tokens.len() {
            let idx = (start + offset) % profile.pool.tokens.len();
            let token = &profile.pool.tokens[idx];
            match states.get(token).and_then(|s| s.cooldown_until) {
                Some(until) if until > now => continue,
                _ => return Ok(token.clone()),
            }
        }

        Err(AiError::NoTokens)
    }

    async fn mark_pool_failure(&self, profile: &ResolvedAiProfile, token: &str) {
        if profile.pool.tokens.is_empty() {
            return;
        }

        let mut states = self.token_state.write().await;
        let entry = states.entry(token.to_string()).or_insert(TokenState {
            failures: 0,
            cooldown_until: None,
        });
        entry.failures = entry.failures.saturating_add(1);

        if entry.failures >= profile.pool.max_failures.max(1) {
            entry.failures = 0;
            entry.cooldown_until =
                Some(Instant::now() + Duration::from_secs(profile.pool.cooldown_secs));
        }
    }

    async fn chat_completion_internal(
        &self,
        messages: Vec<ChatMessage>,
        json_mode: bool,
        max_tokens_override: Option<u32>,
    ) -> Result<String, AiError> {
        let profile = self.config.resolve_ai_profile();
        let (model_id, model) = self.config.resolve_active_model();
        let tier = if model.as_ref().map(|m| m.supports_tier).unwrap_or(true) {
            self.config.active_tier.clone()
        } else {
            "balanced".to_string()
        };

        let max_attempts = 3usize;
        let mut last_err: Option<AiError> = None;

        for attempt in 0..max_attempts {
            let res = self
                .chat_completion_attempt(
                    &profile,
                    &model_id,
                    tier.clone(),
                    messages.clone(),
                    json_mode,
                    max_tokens_override,
                    false,
                )
                .await;

            match res {
                Ok(text) => return Ok(text),
                Err(e) => {
                    let retryable = Self::is_retryable(&e);
                    if retryable && attempt + 1 < max_attempts {
                        let backoff = Duration::from_millis(
                            200u64
                                .saturating_mul(2u64.saturating_pow(attempt as u32))
                                .min(2000),
                        );
                        sleep(backoff).await;
                        last_err = Some(e);
                        continue;
                    }

                    return Err(e);
                }
            }
        }

        Err(last_err.unwrap_or_else(|| AiError::ApiError("Failed after retries".into())))
    }

    async fn chat_completion_once(
        &self,
        messages: Vec<ChatMessage>,
        json_mode: bool,
        tier: String,
        max_tokens_override: Option<u32>,
    ) -> Result<String, AiError> {
        let profile = self.config.resolve_ai_profile();
        let (model_id, _) = self.config.resolve_active_model();
        self.chat_completion_attempt(
            &profile,
            &model_id,
            tier,
            messages,
            json_mode,
            max_tokens_override,
            false,
        )
        .await
    }

    fn resolve_tier_params(
        &self,
        provider: &AiProvider,
        model: Option<&AiModel>,
        tier_id: &str,
        strip_optional: bool,
    ) -> TierParams {
        let default_timeout = Self::tier_request_timeout(tier_id);

        if let Some(m) = model {
            if let Some(custom_tiers) = &m.custom_tiers {
                if let Some(t) = custom_tiers.iter().find(|t| t.id == tier_id) {
                    if strip_optional {
                        return (
                            t.temperature.or_else(|| {
                                if matches!(provider, AiProvider::Moonshot) {
                                    None
                                } else {
                                    Some(Self::tier_temperature(tier_id))
                                }
                            }),
                            t.max_tokens
                                .unwrap_or_else(|| Self::tier_max_tokens(tier_id)),
                            None,
                            None,
                            None,
                            None,
                            default_timeout,
                        );
                    }
                    return (
                        t.temperature.or_else(|| {
                            if matches!(provider, AiProvider::Moonshot) {
                                None
                            } else {
                                Some(Self::tier_temperature(tier_id))
                            }
                        }),
                        t.max_tokens
                            .unwrap_or_else(|| Self::tier_max_tokens(tier_id)),
                        t.reasoning_effort.clone(),
                        t.thinking_type.clone(),
                        t.thinking_budget_tokens,
                        t.thinking_display.clone(),
                        default_timeout,
                    );
                }
            }
        }

        // Fallback to defaults
        (
            if matches!(provider, AiProvider::Moonshot) {
                None
            } else {
                Some(Self::tier_temperature(tier_id))
            },
            Self::tier_max_tokens(tier_id),
            None,
            None,
            None,
            None,
            default_timeout,
        )
    }

    #[allow(clippy::too_many_arguments)]
    async fn chat_completion_attempt(
        &self,
        profile: &ResolvedAiProfile,
        model_id: &str,
        tier: String,
        messages: Vec<ChatMessage>,
        json_mode: bool,
        max_tokens_override: Option<u32>,
        strip_optional: bool,
    ) -> Result<String, AiError> {
        if !strip_optional {
            let model = self.config.ai_models.iter().find(|m| m.id == model_id);
            let (
                _,
                _,
                reasoning_effort,
                thinking_type,
                thinking_budget_tokens,
                thinking_display,
                _,
            ) = self.resolve_tier_params(&profile.provider, model, &tier, false);
            let has_optional = reasoning_effort.is_some()
                || thinking_type.is_some()
                || thinking_budget_tokens.is_some()
                || thinking_display.is_some();

            let first = self
                .chat_completion_attempt_once(
                    profile,
                    model_id,
                    tier.clone(),
                    messages.clone(),
                    json_mode,
                    max_tokens_override,
                    false,
                )
                .await;

            match first {
                Ok(v) => return Ok(v),
                Err(e) => {
                    let is_bad_request =
                        matches!(&e, AiError::ApiError(msg) if msg.contains("Status: 400"));
                    if is_bad_request && (has_optional || json_mode) {
                        return self
                            .chat_completion_attempt_once(
                                profile,
                                model_id,
                                tier,
                                messages,
                                false,
                                max_tokens_override,
                                true,
                            )
                            .await;
                    }
                    return Err(e);
                }
            }
        }

        self.chat_completion_attempt_once(
            profile,
            model_id,
            tier,
            messages,
            json_mode,
            max_tokens_override,
            true,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn chat_completion_attempt_once(
        &self,
        profile: &ResolvedAiProfile,
        model_id: &str,
        tier: String,
        messages: Vec<ChatMessage>,
        json_mode: bool,
        max_tokens_override: Option<u32>,
        strip_optional: bool,
    ) -> Result<String, AiError> {
        let token = match profile.mode {
            AiConnectionMode::Pool => self.choose_pool_token(profile).await?,
            _ => profile.api_key.clone().unwrap_or_default(),
        };

        let model = self.config.ai_models.iter().find(|m| m.id == model_id);
        let (
            temperature,
            default_max_tokens,
            reasoning_effort,
            thinking_type,
            thinking_budget_tokens,
            thinking_display,
            request_timeout,
        ) = self.resolve_tier_params(&profile.provider, model, &tier, strip_optional);
        let max_tokens = max_tokens_override.unwrap_or(default_max_tokens);

        let url = self.resolve_endpoint(profile);

        let request_builder = self.client.post(&url).timeout(request_timeout);

        let resp = match profile.provider {
            AiProvider::Anthropic => {
                let system_msg = messages
                    .iter()
                    .find(|m| m.role == "system")
                    .map(|m| m.content.clone())
                    .unwrap_or_default();
                let filtered_messages: Vec<ChatMessage> = messages
                    .iter()
                    .filter(|m| m.role != "system")
                    .cloned()
                    .collect();

                let mut payload = json!({
                    "model": model_id,
                    "messages": filtered_messages,
                    "system": system_msg,
                    "max_tokens": max_tokens
                });

                if let Some(temp) = temperature {
                    if let Some(obj) = payload.as_object_mut() {
                        obj.insert("temperature".to_string(), json!(temp));
                    }
                }

                if let Some(tt) = thinking_type.clone() {
                    let mut thinking_obj = serde_json::Map::new();
                    thinking_obj.insert("type".to_string(), json!(tt));
                    if tt == "enabled" {
                        if let Some(budget) = thinking_budget_tokens {
                            thinking_obj.insert("budget_tokens".to_string(), json!(budget));
                        }
                    }
                    if tt == "adaptive" {
                        if let Some(effort) = reasoning_effort.clone() {
                            thinking_obj.insert("effort".to_string(), json!(effort));
                        }
                    }
                    if let Some(display) = thinking_display.clone() {
                        thinking_obj.insert("display".to_string(), json!(display));
                    }
                    if let Some(obj) = payload.as_object_mut() {
                        obj.insert("thinking".to_string(), Value::Object(thinking_obj));
                    }
                }

                request_builder
                    .header("x-api-key", token.clone())
                    .header("anthropic-version", "2023-06-01")
                    .header("content-type", "application/json")
                    .json(&payload)
                    .send()
                    .await
            }
            AiProvider::Openai if url.contains("/responses") => {
                let mut payload = json!({
                    "model": model_id,
                    "input": messages.clone(),
                    "max_output_tokens": max_tokens
                });

                if let Some(temp) = temperature {
                    if let Some(obj) = payload.as_object_mut() {
                        obj.insert("temperature".to_string(), json!(temp));
                    }
                }

                if let Some(effort) = reasoning_effort.clone() {
                    if let Some(obj) = payload.as_object_mut() {
                        obj.insert("reasoning".to_string(), json!({ "effort": effort }));
                    }
                }

                request_builder
                    .header("Authorization", format!("Bearer {}", token))
                    .json(&payload)
                    .send()
                    .await
            }
            _ => {
                let mut payload = json!({
                    "model": model_id,
                    "messages": messages.clone()
                });

                if let Some(temp) = temperature {
                    if let Some(obj) = payload.as_object_mut() {
                        obj.insert("temperature".to_string(), json!(temp));
                    }
                }

                if matches!(profile.provider, AiProvider::Moonshot) {
                    if let Some(obj) = payload.as_object_mut() {
                        obj.insert("max_completion_tokens".to_string(), json!(max_tokens));
                    }
                } else if let Some(obj) = payload.as_object_mut() {
                    obj.insert("max_tokens".to_string(), json!(max_tokens));
                }

                if let Some(tt) = thinking_type.clone() {
                    if let Some(obj) = payload.as_object_mut() {
                        obj.insert("thinking".to_string(), json!({ "type": tt }));
                    }
                }

                if json_mode {
                    if let Some(obj) = payload.as_object_mut() {
                        obj.insert(
                            "response_format".to_string(),
                            json!({ "type": "json_object" }),
                        );
                    }
                }

                request_builder
                    .header("Authorization", format!("Bearer {}", token))
                    .json(&payload)
                    .send()
                    .await
            }
        };

        match resp {
            Ok(response) => {
                let status = response.status();
                if status.is_success() {
                    let body: Value = response.json().await?;
                    let content = match profile.provider {
                        AiProvider::Anthropic => body
                            .get("content")
                            .and_then(|c| c.as_array())
                            .and_then(|arr| {
                                arr.iter().find_map(|b| {
                                    if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                                        b.get("text")
                                            .and_then(|t| t.as_str())
                                            .map(|s| s.to_string())
                                    } else {
                                        None
                                    }
                                })
                            }),
                        AiProvider::Openai if url.contains("/responses") => body
                            .get("output_text")
                            .and_then(|t| t.as_str())
                            .map(|s| s.to_string()),
                        _ => body["choices"][0]["message"]["content"]
                            .as_str()
                            .map(|s| s.to_string()),
                    };

                    if let Some(content) = content {
                        return Ok(content);
                    }

                    if let Some(error) = body.get("error") {
                        return Err(AiError::ApiError(error.to_string()));
                    }

                    Err(AiError::ApiError("Invalid response format".into()))
                } else {
                    let body_text = response.text().await.unwrap_or_default();
                    match status {
                        StatusCode::UNAUTHORIZED => Err(AiError::Auth(body_text)),
                        StatusCode::FORBIDDEN => Err(AiError::Forbidden(body_text)),
                        StatusCode::NOT_FOUND => Err(AiError::ModelNotFound(body_text)),
                        StatusCode::TOO_MANY_REQUESTS => {
                            if matches!(profile.mode, AiConnectionMode::Pool) {
                                self.mark_pool_failure(profile, &token).await;
                            }
                            Err(AiError::RateLimited(body_text))
                        }
                        s if s.is_server_error() => {
                            if matches!(profile.mode, AiConnectionMode::Pool) {
                                self.mark_pool_failure(profile, &token).await;
                            }
                            Err(AiError::ServerError(format!(
                                "Status: {}, body: {}",
                                status, body_text
                            )))
                        }
                        _ => Err(AiError::ApiError(format!(
                            "Status: {}, body: {}",
                            status, body_text
                        ))),
                    }
                }
            }
            Err(e) => {
                if matches!(profile.mode, AiConnectionMode::Pool)
                    && (e.is_timeout() || e.is_connect())
                {
                    self.mark_pool_failure(profile, &token).await;
                }
                Err(AiError::Network(e))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{extract::State, http::StatusCode as AxumStatusCode, routing::post, Json, Router};
    use std::net::SocketAddr;
    use tokio::net::TcpListener;

    #[derive(Clone, Default)]
    struct CaptureState {
        bodies: Arc<RwLock<Vec<Value>>>,
    }

    async fn start_capture_server<F>(responder: F) -> (String, CaptureState)
    where
        F: Fn(Value) -> (AxumStatusCode, Value) + Send + Sync + 'static,
    {
        let state = CaptureState::default();
        let responder = Arc::new(responder);

        let make_handler = || {
            let responder = responder.clone();
            move |State(s): State<CaptureState>, Json(body): Json<Value>| {
                let responder = responder.clone();
                async move {
                    {
                        let mut w = s.bodies.write().await;
                        w.push(body.clone());
                    }
                    let (status, resp) = responder(body);
                    (status, Json(resp))
                }
            }
        };

        let app = Router::new()
            .route("/", post(make_handler()))
            .route("/*path", post(make_handler()))
            .with_state(state.clone());

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        (format!("http://{}/", addr), state)
    }

    fn make_config(
        provider: AiProvider,
        endpoint: String,
        model: AiModel,
        active_tier: &str,
    ) -> AppConfig {
        let profile = crate::config::AiProfile {
            id: "p".to_string(),
            name: "p".to_string(),
            provider,
            mode: AiConnectionMode::Relay,
            api_key: Some("test-key".to_string()),
            relay_url: Some(endpoint),
            pool: crate::config::AiPoolConfig::default(),
        };

        AppConfig {
            ai_profiles: vec![profile],
            active_ai_profile_id: Some("p".to_string()),
            ai_models: vec![model],
            active_model_id: Some("m".to_string()),
            active_tier: active_tier.to_string(),
            ..AppConfig::default()
        }
    }

    #[tokio::test]
    async fn openai_responses_injects_reasoning_effort() {
        let (endpoint, capture) =
            start_capture_server(|_body| (AxumStatusCode::OK, json!({ "output_text": "ok" })))
                .await;

        let model = AiModel {
            id: "m".to_string(),
            provider: AiProvider::Openai,
            display_name: "m".to_string(),
            supports_tier: true,
            max_context: 0,
            custom_tiers: Some(vec![crate::config::AiModelTier {
                id: "xhigh".to_string(),
                display_name: "xhigh".to_string(),
                temperature: None,
                max_tokens: Some(999),
                reasoning_effort: Some("xhigh".to_string()),
                thinking_type: None,
                thinking_budget_tokens: None,
                thinking_display: None,
            }]),
        };

        let cfg = make_config(
            AiProvider::Openai,
            endpoint.trim_end_matches('/').to_string() + "/responses",
            model,
            "xhigh",
        );
        let gateway = AiGateway::new(cfg);

        let _ = gateway
            .chat_completion_once(
                vec![ChatMessage {
                    role: "user".to_string(),
                    content: "hi".to_string(),
                }],
                false,
                "xhigh".to_string(),
                Some(123),
            )
            .await
            .unwrap();

        let bodies = capture.bodies.read().await;
        assert_eq!(bodies.len(), 1);
        let body = &bodies[0];
        assert_eq!(
            body.get("max_output_tokens").and_then(|v| v.as_u64()),
            Some(123)
        );
        assert_eq!(
            body.get("reasoning")
                .and_then(|v| v.get("effort"))
                .and_then(|v| v.as_str()),
            Some("xhigh")
        );
        assert!(body.get("messages").is_none());
        assert!(body.get("input").is_some());
    }

    #[tokio::test]
    async fn deepseek_injects_thinking_and_max_tokens() {
        let (endpoint, capture) = start_capture_server(|_body| {
            (
                AxumStatusCode::OK,
                json!({ "choices": [ { "message": { "content": "ok" } } ] }),
            )
        })
        .await;

        let model = AiModel {
            id: "m".to_string(),
            provider: AiProvider::Deepseek,
            display_name: "m".to_string(),
            supports_tier: true,
            max_context: 0,
            custom_tiers: Some(vec![crate::config::AiModelTier {
                id: "thinking.enabled".to_string(),
                display_name: "thinking.enabled".to_string(),
                temperature: Some(0.2),
                max_tokens: Some(456),
                reasoning_effort: None,
                thinking_type: Some("enabled".to_string()),
                thinking_budget_tokens: None,
                thinking_display: None,
            }]),
        };

        let cfg = make_config(AiProvider::Deepseek, endpoint, model, "thinking.enabled");
        let gateway = AiGateway::new(cfg);

        let _ = gateway
            .chat_completion_once(
                vec![ChatMessage {
                    role: "user".to_string(),
                    content: "hi".to_string(),
                }],
                false,
                "thinking.enabled".to_string(),
                None,
            )
            .await
            .unwrap();

        let bodies = capture.bodies.read().await;
        assert_eq!(bodies.len(), 1);
        let body = &bodies[0];
        assert_eq!(body.get("max_tokens").and_then(|v| v.as_u64()), Some(456));
        assert_eq!(
            body.get("thinking")
                .and_then(|v| v.get("type"))
                .and_then(|v| v.as_str()),
            Some("enabled")
        );
    }

    #[tokio::test]
    async fn moonshot_uses_max_completion_tokens_and_omits_temperature_by_default() {
        let (endpoint, capture) = start_capture_server(|_body| {
            (
                AxumStatusCode::OK,
                json!({ "choices": [ { "message": { "content": "ok" } } ] }),
            )
        })
        .await;

        let model = AiModel {
            id: "m".to_string(),
            provider: AiProvider::Moonshot,
            display_name: "m".to_string(),
            supports_tier: true,
            max_context: 0,
            custom_tiers: Some(vec![crate::config::AiModelTier {
                id: "thinking.disabled".to_string(),
                display_name: "thinking.disabled".to_string(),
                temperature: None,
                max_tokens: Some(321),
                reasoning_effort: None,
                thinking_type: Some("disabled".to_string()),
                thinking_budget_tokens: None,
                thinking_display: None,
            }]),
        };

        let cfg = make_config(AiProvider::Moonshot, endpoint, model, "thinking.disabled");
        let gateway = AiGateway::new(cfg);

        let _ = gateway
            .chat_completion_once(
                vec![ChatMessage {
                    role: "user".to_string(),
                    content: "hi".to_string(),
                }],
                false,
                "thinking.disabled".to_string(),
                None,
            )
            .await
            .unwrap();

        let bodies = capture.bodies.read().await;
        assert_eq!(bodies.len(), 1);
        let body = &bodies[0];
        assert_eq!(
            body.get("max_completion_tokens").and_then(|v| v.as_u64()),
            Some(321)
        );
        assert!(body.get("temperature").is_none());
        assert_eq!(
            body.get("thinking")
                .and_then(|v| v.get("type"))
                .and_then(|v| v.as_str()),
            Some("disabled")
        );
    }

    #[tokio::test]
    async fn bad_request_triggers_strip_optional_retry_once() {
        let (endpoint, capture) = start_capture_server(|body| {
            if body.get("thinking").is_some() || body.get("reasoning").is_some() {
                (
                    AxumStatusCode::BAD_REQUEST,
                    json!({ "error": { "message": "unknown field" } }),
                )
            } else {
                (
                    AxumStatusCode::OK,
                    json!({ "choices": [ { "message": { "content": "ok" } } ] }),
                )
            }
        })
        .await;

        let model = AiModel {
            id: "m".to_string(),
            provider: AiProvider::Deepseek,
            display_name: "m".to_string(),
            supports_tier: true,
            max_context: 0,
            custom_tiers: Some(vec![crate::config::AiModelTier {
                id: "thinking.enabled".to_string(),
                display_name: "thinking.enabled".to_string(),
                temperature: Some(0.2),
                max_tokens: Some(16),
                reasoning_effort: None,
                thinking_type: Some("enabled".to_string()),
                thinking_budget_tokens: None,
                thinking_display: None,
            }]),
        };

        let cfg = make_config(AiProvider::Deepseek, endpoint, model, "thinking.enabled");
        let gateway = AiGateway::new(cfg);

        let _ = gateway
            .chat_completion_once(
                vec![ChatMessage {
                    role: "user".to_string(),
                    content: "hi".to_string(),
                }],
                false,
                "thinking.enabled".to_string(),
                Some(8),
            )
            .await
            .unwrap();

        let bodies = capture.bodies.read().await;
        assert_eq!(bodies.len(), 2);
        assert!(bodies[0].get("thinking").is_some());
        assert!(bodies[1].get("thinking").is_none());
    }

    #[tokio::test]
    async fn bad_request_on_response_format_retries_without_json_mode() {
        let (endpoint, capture) = start_capture_server(|body| {
            if body.get("response_format").is_some() {
                (
                    AxumStatusCode::BAD_REQUEST,
                    json!({ "error": { "message": "invalid parameter: response_format" } }),
                )
            } else {
                (
                    AxumStatusCode::OK,
                    json!({ "choices": [ { "message": { "content": "{\"ok\":true}" } } ] }),
                )
            }
        })
        .await;

        let model = AiModel {
            id: "m".to_string(),
            provider: AiProvider::Deepseek,
            display_name: "m".to_string(),
            supports_tier: true,
            max_context: 0,
            custom_tiers: None,
        };

        let cfg = make_config(AiProvider::Deepseek, endpoint, model, "balanced");
        let gateway = AiGateway::new(cfg);

        let _ = gateway
            .chat_completion_json(vec![ChatMessage {
                role: "user".to_string(),
                content: "hi".to_string(),
            }])
            .await
            .unwrap();

        let bodies = capture.bodies.read().await;
        assert_eq!(bodies.len(), 2);
        assert!(bodies[0].get("response_format").is_some());
        assert!(bodies[1].get("response_format").is_none());
    }
}
