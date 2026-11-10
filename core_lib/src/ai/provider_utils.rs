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

    // ---------------------------------------------------------------------
    // 代理分裂覆盖（原 e2e-runner 的 `ai_proxy_failure_split_e2e`）
    //
    // “无代理时直连成功 / 有代理时请求走代理并失败”这两条只能在单测里验：
    // `health_check` 里的 SSRF 校验由 `resolve_endpoint(&profile, !cfg!(test))` 控制，
    // 独立二进制（e2e-runner 依赖 core_lib 时是非 test 构建）始终开启校验，而 mock AI
    // 服务只能监听回环地址，请求根本发不出去。这里用同一条
    // health_check -> resolve_endpoint -> chat_completion_raw 路径做等价覆盖。
    //
    // 生产构建的 SSRF 判定一行未改：本模块的 validate_ai_url 用例继续断言回环/私网/链路本地
    // 地址被拒绝，`ssrf_guard_still_rejects_internal_addresses` 显式守住这条底线
    // （测试模式只是让 health_check 跳过校验，并没有把校验函数本身放宽）。
    // ---------------------------------------------------------------------

    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};

    /// 代理相关环境变量是进程级状态，而 `cargo test` 默认多线程并发执行。
    /// 改写这些变量的用例必须先拿这把锁，保证彼此串行。
    fn proxy_env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 大小写两套都覆盖：reqwest/hyper-util 读环境变量时两者都会看，
    /// NO_PROXY 里如果恰好含 127.0.0.1 会让“走代理”这半边失去意义。
    const PROXY_ENV_KEYS: [&str; 8] = [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
        "no_proxy",
    ];

    /// 用例开始时清空代理环境变量，Drop 时按原值恢复，避免污染同进程的其它用例。
    struct ProxyEnvSnapshot(Vec<(&'static str, Option<std::ffi::OsString>)>);

    impl ProxyEnvSnapshot {
        fn clear() -> Self {
            let saved = PROXY_ENV_KEYS
                .iter()
                .map(|key| (*key, std::env::var_os(key)))
                .collect();
            for key in PROXY_ENV_KEYS {
                std::env::remove_var(key);
            }
            Self(saved)
        }
    }

    impl Drop for ProxyEnvSnapshot {
        fn drop(&mut self) {
            for (key, value) in &self.0 {
                match value {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    /// 回环地址上的测试服务：`hits` 用来证明请求到底打到了谁（直连目标 or 代理）。
    struct TestServer {
        addr: SocketAddr,
        hits: Arc<AtomicUsize>,
        handle: tokio::task::JoinHandle<()>,
    }

    impl TestServer {
        fn hits(&self) -> usize {
            self.hits.load(Ordering::SeqCst)
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.handle.abort();
        }
    }

    /// OpenAI 兼容的 mock。注意 rig 0.36 的 `providers::openai::Client` 默认走
    /// Responses API（`{base_url}/responses`），不是 `/chat/completions`；
    /// 因此 base_url 只写到 `/v1`，这里注册 `/v1/responses`。
    async fn start_mock_ai_server() -> TestServer {
        use axum::extract::State;

        async fn handler(state: State<Arc<AtomicUsize>>) -> axum::Json<serde_json::Value> {
            state.0.fetch_add(1, Ordering::SeqCst);
            axum::Json(serde_json::json!({
                "id": "resp-provider-utils-test",
                "object": "response",
                "created_at": 0,
                "status": "completed",
                "model": "gpt-4o-mini",
                "output": [{
                    "type": "message",
                    "id": "msg-provider-utils-test",
                    "role": "assistant",
                    "status": "completed",
                    "content": [{ "type": "output_text", "text": "pong" }]
                }]
            }))
        }

        let hits = Arc::new(AtomicUsize::new(0));
        let app = axum::Router::new()
            .route("/v1/responses", axum::routing::post(handler))
            .with_state(hits.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock ai server");
        let addr = listener.local_addr().expect("mock ai addr");
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        TestServer { addr, hits, handle }
    }

    /// 不做转发的坏代理：只要收到请求就回 502，用来证明请求确实走了代理。
    async fn start_bad_http_proxy() -> TestServer {
        let hits = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind bad proxy");
        let addr = listener.local_addr().expect("bad proxy addr");
        let counter = hits.clone();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let counter = counter.clone();
                tokio::spawn(async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    let mut buf = [0u8; 4096];
                    let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut buf).await;
                    let _ = tokio::io::AsyncWriteExt::write_all(
                        &mut socket,
                        b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n",
                    )
                    .await;
                });
            }
        });
        TestServer { addr, hits, handle }
    }

    /// Relay 模式下把 relay_url 指向本地 mock。注意 `AppConfig::default()` 的
    /// active profile id 就是 "default"，改 `ai_profiles[0]` 即等于改生效 profile。
    fn relay_config(addr: SocketAddr) -> AppConfig {
        let mut config = AppConfig::default();
        let profile = config
            .ai_profiles
            .first_mut()
            .expect("default AppConfig should contain one ai profile");
        profile.provider = AiProvider::Openai;
        profile.mode = AiConnectionMode::Relay;
        profile.api_key = Some("test-key".to_string());
        profile.relay_url = Some(format!("http://{}/v1", addr));
        config
    }

    #[tokio::test]
    async fn health_check_succeeds_directly_without_proxy() {
        let _env_guard = proxy_env_lock();
        let _env = ProxyEnvSnapshot::clear();

        let mock = start_mock_ai_server().await;
        let config = relay_config(mock.addr);

        let report = health_check(&config)
            .await
            .expect("health check should reach the mock endpoint directly when no proxy is set");
        assert_eq!(mock.hits(), 1, "mock ai server should receive exactly one request");
        assert!(report.ok);
        assert_eq!(report.endpoint, format!("http://{}/v1", mock.addr));
        assert_eq!(report.result_preview.as_deref(), Some("pong"));
    }

    #[tokio::test]
    async fn health_check_uses_proxy_when_http_proxy_set() {
        let _env_guard = proxy_env_lock();
        let _env = ProxyEnvSnapshot::clear();

        let mock = start_mock_ai_server().await;
        let proxy = start_bad_http_proxy().await;
        let config = relay_config(mock.addr);

        std::env::set_var("HTTP_PROXY", format!("http://{}", proxy.addr));

        let result = health_check(&config).await;
        assert!(
            result.is_err(),
            "health check must fail when the http proxy answers 502"
        );
        assert!(
            proxy.hits() >= 1,
            "request should be routed through the proxy, but the proxy saw no traffic"
        );
        assert_eq!(
            mock.hits(),
            0,
            "request must not bypass the proxy and reach the endpoint directly"
        );
    }

    /// 守住 SSRF 底线：测试模式只让 health_check 跳过校验，校验函数本身仍必须拒绝内网地址。
    #[test]
    fn ssrf_guard_still_rejects_internal_addresses() {
        for url in [
            "http://127.0.0.1/v1/chat/completions",
            "http://localhost/v1/chat/completions",
            "http://10.0.0.1/v1/chat/completions",
            "http://192.168.1.1/v1/chat/completions",
            "http://172.16.0.1/v1/chat/completions",
            "http://169.254.169.254/latest/meta-data",
            "ftp://example.com/v1",
        ] {
            let err = validate_ai_url(url)
                .err()
                .unwrap_or_else(|| panic!("internal address must stay blocked: {}", url));
            let msg = err.to_string();
            assert!(!msg.is_empty(), "rejection must carry a reason: {}", url);
        }
    }
}