//! 告警通知渠道：站内、邮件（SMTP）与通用 Webhook。
//!
//! 站内通道只确认告警已由 `InspectionStore` 持久化；邮件与 Webhook 会真正建立连接发送。
//! 失败按协议分类：HTTP 4xx / SMTP 5xx 视为永久错误（不重试），
//! HTTP 5xx / SMTP 4xx 与连接失败视为临时错误（退避重试）。

use std::time::Duration;

use base64::Engine;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::db::protocol::BoxFuture;
use crate::ops::inspection::notify::{
    build_content, EmailNotificationSettings, EmailSecurity, NotificationSecrets,
    WebhookNotificationSettings,
};
use crate::ops::inspection::types::{
    now_ts, AlertRecord, ChannelDelivery, DeliveryStatus,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertEventKind {
    Raised,
    /// 同一条 finding 严重度升级时才再次推送。
    Escalated,
    Resolved,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlertNotification {
    pub kind: AlertEventKind,
    pub alert: AlertRecord,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelError {
    NotConfigured(String),
    /// 永久错误：凭据、地址、4xx 等，不重试。
    Permanent(String),
    /// 临时错误：5xx、连接失败、超时等，可退避重试。
    Transient(String),
}

impl ChannelError {
    pub fn is_retryable(&self) -> bool {
        matches!(self, ChannelError::Transient(_))
    }

    fn transient(message: impl Into<String>) -> Self {
        ChannelError::Transient(message.into())
    }
}

impl std::fmt::Display for ChannelError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChannelError::NotConfigured(message) => write!(formatter, "未配置: {message}"),
            ChannelError::Permanent(message) => write!(formatter, "永久失败: {message}"),
            ChannelError::Transient(message) => write!(formatter, "临时失败: {message}"),
        }
    }
}

impl std::error::Error for ChannelError {}

/// 发送结果：`redacted` 表示发送前清洗是否移除了连接串/凭据。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SendOutcome {
    pub redacted: bool,
}

pub trait NotificationChannel: Send + Sync {
    fn id(&self) -> &'static str;
    fn enabled(&self) -> bool;
    fn send<'a>(
        &'a self,
        notification: &'a AlertNotification,
    ) -> BoxFuture<'a, Result<SendOutcome, ChannelError>>;
}

/// 站内通知：告警已写入本地存储，这里只确认通道可用。
pub struct InAppChannel;

impl NotificationChannel for InAppChannel {
    fn id(&self) -> &'static str {
        "in_app"
    }

    fn enabled(&self) -> bool {
        true
    }

    fn send<'a>(
        &'a self,
        _notification: &'a AlertNotification,
    ) -> BoxFuture<'a, Result<SendOutcome, ChannelError>> {
        Box::pin(async move { Ok(SendOutcome::default()) })
    }
}

// ---------------------------------------------------------------------------
// 重试
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    pub max_retries: u32,
    pub backoff_seconds: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 2,
            backoff_seconds: 5,
        }
    }
}

/// 发送一次并对临时错误退避重试；返回完整的投递记录（含每次尝试的时间戳）。
pub async fn deliver_with_retry(
    channel: &dyn NotificationChannel,
    notification: &AlertNotification,
    policy: RetryPolicy,
) -> ChannelDelivery {
    let mut delivery = ChannelDelivery::pending(channel.id());
    loop {
        let attempt_at = now_ts();
        delivery.attempts += 1;
        delivery.last_attempt_at = Some(attempt_at);
        delivery.attempt_times.push(attempt_at);
        match channel.send(notification).await {
            Ok(outcome) => {
                delivery.status = DeliveryStatus::Success;
                delivery.delivered_at = Some(attempt_at);
                delivery.last_error = None;
                delivery.redacted = outcome.redacted;
                return delivery;
            }
            Err(err) => {
                // 驱动/HTTP 客户端错误可能回显 URL 或 host:port，投递记录同样不能原样保存。
                let (message, _) = crate::ops::inspection::redact::scrub_text(&err.to_string(), &[]);
                delivery.last_error = Some(message);
                if err.is_retryable() && delivery.attempts <= policy.max_retries {
                    delivery.status = DeliveryStatus::Retrying;
                    let sleep = policy
                        .backoff_seconds
                        .saturating_mul(delivery.attempts as u64);
                    if sleep > 0 {
                        tokio::time::sleep(Duration::from_secs(sleep)).await;
                    }
                    continue;
                }
                delivery.status = DeliveryStatus::Failed;
                return delivery;
            }
        }
    }
}

/// HTTP 状态码分类：4xx 永久，5xx 临时。
pub fn classify_http_status(status: u16) -> Result<(), ChannelError> {
    match status {
        200..=299 => Ok(()),
        400..=499 => Err(ChannelError::Permanent(format!("HTTP {status}"))),
        500..=599 => Err(ChannelError::Transient(format!("HTTP {status}"))),
        other => Err(ChannelError::Permanent(format!("HTTP {other}"))),
    }
}

/// SMTP 应答分类：4xx 临时，5xx 永久。
pub fn classify_smtp_code(code: u16, message: &str) -> Result<(), ChannelError> {
    match code {
        200..=299 => Ok(()),
        400..=499 => Err(ChannelError::Transient(format!("SMTP {code}: {message}"))),
        500..=599 => Err(ChannelError::Permanent(format!("SMTP {code}: {message}"))),
        other => Err(ChannelError::Permanent(format!("SMTP {other}: {message}"))),
    }
}

/// 非预期应答统一转成错误，避免对 2xx 调用 `unwrap_err`。
fn smtp_failure(code: u16, message: &str) -> ChannelError {
    match classify_smtp_code(code, message) {
        Err(err) => err,
        Ok(()) => ChannelError::Permanent(format!("SMTP {code}: {message}")),
    }
}

// ---------------------------------------------------------------------------
// 邮件：最小 SMTP 客户端（Plain / STARTTLS / SSL + AUTH PLAIN/LOGIN）
// ---------------------------------------------------------------------------

pub struct EmailChannel {
    settings: EmailNotificationSettings,
    secrets: NotificationSecrets,
}

impl EmailChannel {
    pub fn new(settings: EmailNotificationSettings, secrets: NotificationSecrets) -> Self {
        Self { settings, secrets }
    }
}

impl NotificationChannel for EmailChannel {
    fn id(&self) -> &'static str {
        "email"
    }

    fn enabled(&self) -> bool {
        self.settings.enabled
    }

    fn send<'a>(
        &'a self,
        notification: &'a AlertNotification,
    ) -> BoxFuture<'a, Result<SendOutcome, ChannelError>> {
        Box::pin(async move { send_email(&self.settings, &self.secrets, notification).await })
    }
}

async fn send_email(
    settings: &EmailNotificationSettings,
    secrets: &NotificationSecrets,
    notification: &AlertNotification,
) -> Result<SendOutcome, ChannelError> {
    if settings.host.trim().is_empty() {
        return Err(ChannelError::NotConfigured("SMTP 主机为空".into()));
    }
    if settings.to.is_empty() {
        return Err(ChannelError::NotConfigured("收件人为空".into()));
    }
    let from = if settings.from.trim().is_empty() {
        settings.username.trim().to_string()
    } else {
        settings.from.trim().to_string()
    };
    if from.is_empty() {
        return Err(ChannelError::NotConfigured("发件人为空".into()));
    }

    let content = build_content(notification, secrets);
    let message = build_mime_message(settings, &from, &content.subject, &content.text);

    let tcp = TcpStream::connect(format!("{}:{}", settings.host.trim(), settings.port))
        .await
        .map_err(|err| ChannelError::transient(format!("连接 SMTP 失败: {err}")))?;
    let conn = match settings.security {
        EmailSecurity::Ssl => Conn::Tls(Box::new(
            tls_connect(tcp, settings.host.trim(), &settings.ca_cert_path).await?,
        )),
        _ => Conn::Tcp(tcp),
    };
    let mut session = SmtpSession {
        conn: Some(conn),
        pending: Vec::new(),
    };

    let (code, text) = session.read_response().await?;
    classify_smtp_code(code, &text)?;
    if code != 220 {
        return Err(ChannelError::Permanent(format!("SMTP 欢迎语异常: {text}")));
    }
    session.ehlo().await?;

    if matches!(settings.security, EmailSecurity::StartTls) {
        let (code, text) = session.command("STARTTLS").await?;
        if code != 220 {
            return Err(smtp_failure(code, &text));
        }
        let conn = session
            .conn
            .take()
            .ok_or_else(|| ChannelError::Permanent("SMTP 连接状态异常".into()))?;
        let tcp = match conn {
            Conn::Tcp(stream) => stream,
            Conn::Tls(_) => {
                return Err(ChannelError::Permanent("SMTP 连接已加密".into()));
            }
        };
        let tls = tls_connect(tcp, settings.host.trim(), &settings.ca_cert_path).await?;
        session.conn = Some(Conn::Tls(Box::new(tls)));
        session.pending.clear();
        session.ehlo().await?;
    }

    if !settings.username.trim().is_empty() {
        session
            .authenticate(
                settings.username.trim(),
                secrets.smtp_password.as_str(),
            )
            .await?;
    }

    let (code, text) = session
        .command(&format!("MAIL FROM:<{from}>"))
        .await?;
    if code != 250 {
        return Err(smtp_failure(code, &text));
    }
    for recipient in &settings.to {
        let (code, text) = session
            .command(&format!("RCPT TO:<{}>", recipient.trim()))
            .await?;
        if code != 250 && code != 251 {
            return Err(smtp_failure(code, &text));
        }
    }
    let (code, text) = session.command("DATA").await?;
    if code != 354 {
        return Err(smtp_failure(code, &text));
    }
    session.write_raw(&dot_stuff(&message)).await?;
    let (code, text) = session.read_response().await?;
    if code != 250 {
        return Err(smtp_failure(code, &text));
    }
    let _ = session.command("QUIT").await;
    Ok(SendOutcome {
        redacted: content.redacted,
    })
}

async fn tls_connect(
    stream: TcpStream,
    host: &str,
    ca_cert_path: &str,
) -> Result<tokio_native_tls::TlsStream<TcpStream>, ChannelError> {
    let mut builder = native_tls::TlsConnector::builder();
    if !ca_cert_path.trim().is_empty() {
        let pem = std::fs::read(ca_cert_path.trim()).map_err(|err| {
            ChannelError::Permanent(format!("读取 CA 证书失败（{ca_cert_path}）: {err}"))
        })?;
        let certificate = native_tls::Certificate::from_pem(&pem).map_err(|err| {
            ChannelError::Permanent(format!("解析 CA 证书失败（{ca_cert_path}）: {err}"))
        })?;
        builder.add_root_certificate(certificate);
    }
    let connector = builder
        .build()
        .map_err(|err| ChannelError::transient(format!("初始化 TLS 失败: {err}")))?;
    let connector = tokio_native_tls::TlsConnector::from(connector);
    connector
        .connect(host, stream)
        .await
        .map_err(|err| ChannelError::transient(format!("TLS 握手失败: {err}")))
}

fn build_mime_message(
    settings: &EmailNotificationSettings,
    from: &str,
    subject: &str,
    text: &str,
) -> String {
    let recipients = settings
        .to
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join(", ");
    let encoded_subject = format!(
        "=?UTF-8?B?{}?=",
        base64_encode(subject.as_bytes())
    );
    let body = base64_wrap(&base64_encode(text.as_bytes()), 76);
    let mut message = String::new();
    message.push_str(&format!("From: {from}\r\n"));
    message.push_str(&format!("To: {recipients}\r\n"));
    message.push_str(&format!("Subject: {encoded_subject}\r\n"));
    message.push_str(&format!(
        "Date: {}\r\n",
        chrono::Local::now().to_rfc2822()
    ));
    message.push_str("MIME-Version: 1.0\r\n");
    message.push_str("Content-Type: text/plain; charset=UTF-8\r\n");
    message.push_str("Content-Transfer-Encoding: base64\r\n");
    message.push_str("\r\n");
    message.push_str(&body);
    message
}

fn base64_encode(input: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(input)
}

fn base64_wrap(encoded: &str, width: usize) -> String {
    let mut out = String::new();
    for chunk in encoded.as_bytes().chunks(width.max(1)) {
        out.push_str(&String::from_utf8_lossy(chunk));
        out.push_str("\r\n");
    }
    out
}

fn dot_stuff(message: &str) -> String {
    let mut out = String::new();
    for line in message.split("\r\n") {
        if line.starts_with('.') {
            out.push('.');
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    out.push_str(".\r\n");
    out
}

enum Conn {
    Tcp(TcpStream),
    Tls(Box<tokio_native_tls::TlsStream<TcpStream>>),
}

impl Conn {
    async fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Conn::Tcp(stream) => stream.read(buffer).await,
            Conn::Tls(stream) => stream.read(buffer).await,
        }
    }

    async fn write_all(&mut self, buffer: &[u8]) -> std::io::Result<()> {
        match self {
            Conn::Tcp(stream) => stream.write_all(buffer).await,
            Conn::Tls(stream) => stream.write_all(buffer).await,
        }
    }
}

struct SmtpSession {
    conn: Option<Conn>,
    pending: Vec<u8>,
}

impl SmtpSession {
    fn connection_error() -> ChannelError {
        ChannelError::Permanent("SMTP 连接已关闭".into())
    }

    async fn read_line(&mut self) -> Result<String, ChannelError> {
        loop {
            if let Some(position) = self.pending.iter().position(|byte| *byte == b'\n') {
                let line: Vec<u8> = self.pending.drain(..=position).collect();
                return Ok(String::from_utf8_lossy(&line)
                    .trim_end_matches(['\r', '\n'])
                    .to_string());
            }
            let mut chunk = [0u8; 512];
            let conn = self.conn.as_mut().ok_or_else(Self::connection_error)?;
            let read = conn
                .read(&mut chunk)
                .await
                .map_err(|err| ChannelError::transient(format!("读取 SMTP 响应失败: {err}")))?;
            if read == 0 {
                return Err(ChannelError::transient("SMTP 连接被对端关闭"));
            }
            self.pending.extend_from_slice(&chunk[..read]);
        }
    }

    async fn read_response(&mut self) -> Result<(u16, String), ChannelError> {
        let first = self.read_line().await?;
        let code = first
            .get(..3)
            .and_then(|value| value.parse::<u16>().ok())
            .ok_or_else(|| ChannelError::transient(format!("SMTP 响应无法解析: {first}")))?;
        let mut combined = first.clone();
        if first.as_bytes().get(3) == Some(&b'-') {
            loop {
                let line = self.read_line().await?;
                let continued = line.as_bytes().get(3) == Some(&b'-');
                combined.push_str(" | ");
                combined.push_str(&line);
                if !continued {
                    break;
                }
            }
        }
        Ok((code, combined))
    }

    async fn write_raw(&mut self, data: &str) -> Result<(), ChannelError> {
        let conn = self.conn.as_mut().ok_or_else(Self::connection_error)?;
        conn.write_all(data.as_bytes())
            .await
            .map_err(|err| ChannelError::transient(format!("写入 SMTP 失败: {err}")))
    }

    async fn write_line(&mut self, line: &str) -> Result<(), ChannelError> {
        self.write_raw(&format!("{line}\r\n")).await
    }

    async fn command(&mut self, line: &str) -> Result<(u16, String), ChannelError> {
        self.write_line(line).await?;
        self.read_response().await
    }

    async fn ehlo(&mut self) -> Result<(), ChannelError> {
        let (code, text) = self.command("EHLO localhost").await?;
        if code == 250 {
            Ok(())
        } else {
            Err(smtp_failure(code, &text))
        }
    }

    async fn authenticate(
        &mut self,
        username: &str,
        password: &str,
    ) -> Result<(), ChannelError> {
        let plain = base64_encode(format!("\0{username}\0{password}").as_bytes());
        let (code, text) = self.command(&format!("AUTH PLAIN {plain}")).await?;
        if code == 235 {
            return Ok(());
        }
        // 只有“机制不支持”才尝试 AUTH LOGIN；535 等凭据错误直接按永久失败处理，避免重复认证。
        if !matches!(code, 500 | 502 | 504) {
            return Err(smtp_failure(code, &text));
        }
        let (code, text) = self.command("AUTH LOGIN").await?;
        if code != 334 {
            return Err(smtp_failure(code, &text));
        }
        let (code, text) = self
            .command(&base64_encode(username.as_bytes()))
            .await?;
        if code != 334 {
            return Err(smtp_failure(code, &text));
        }
        let (code, text) = self
            .command(&base64_encode(password.as_bytes()))
            .await?;
        if code == 235 {
            Ok(())
        } else {
            Err(smtp_failure(code, &text))
        }
    }
}

// ---------------------------------------------------------------------------
// Webhook
// ---------------------------------------------------------------------------

pub struct WebhookChannel {
    settings: WebhookNotificationSettings,
    secrets: NotificationSecrets,
}

impl WebhookChannel {
    pub fn new(settings: WebhookNotificationSettings, secrets: NotificationSecrets) -> Self {
        Self { settings, secrets }
    }
}

impl NotificationChannel for WebhookChannel {
    fn id(&self) -> &'static str {
        "webhook"
    }

    fn enabled(&self) -> bool {
        self.settings.enabled
    }

    fn send<'a>(
        &'a self,
        notification: &'a AlertNotification,
    ) -> BoxFuture<'a, Result<SendOutcome, ChannelError>> {
        Box::pin(async move { send_webhook(&self.settings, &self.secrets, notification).await })
    }
}

async fn send_webhook(
    settings: &WebhookNotificationSettings,
    secrets: &NotificationSecrets,
    notification: &AlertNotification,
) -> Result<SendOutcome, ChannelError> {
    let url = secrets.webhook_url.trim();
    if url.is_empty() {
        return Err(ChannelError::NotConfigured("Webhook URL 为空".into()));
    }
    let content = build_content(notification, secrets);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|err| ChannelError::Permanent(format!("初始化 HTTP 客户端失败: {err}")))?;
    let mut request = client
        .post(url)
        .header("content-type", "application/json")
        .json(&content.payload);
    for name in &settings.header_names {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some(value) = secrets.webhook_headers.get(trimmed) else {
            continue;
        };
        let header_name = reqwest::header::HeaderName::from_bytes(trimmed.as_bytes())
            .map_err(|err| ChannelError::Permanent(format!("自定义头名称非法: {err}")))?;
        let header_value = reqwest::header::HeaderValue::from_str(value)
            .map_err(|err| ChannelError::Permanent(format!("自定义头值非法: {err}")))?;
        request = request.header(header_name, header_value);
    }
    let response = request
        .send()
        .await
        .map_err(|err| ChannelError::transient(format!("Webhook 请求失败: {err}")))?;
    let status = response.status().as_u16();
    if (200..300).contains(&status) {
        return Ok(SendOutcome {
            redacted: content.redacted,
        });
    }
    let body = response
        .text()
        .await
        .unwrap_or_default()
        .chars()
        .take(200)
        .collect::<String>();
    classify_http_status(status)
        .map(|_| SendOutcome {
            redacted: content.redacted,
        })
        .map_err(|err| match err {
            ChannelError::Transient(message) => {
                ChannelError::Transient(format!("{message}: {body}"))
            }
            ChannelError::Permanent(message) => {
                ChannelError::Permanent(format!("{message}: {body}"))
            }
            other => other,
        })
}

// ---------------------------------------------------------------------------
// 调度器
// ---------------------------------------------------------------------------

pub struct NotificationDispatcher {
    channels: Vec<Box<dyn NotificationChannel>>,
}

impl NotificationDispatcher {
    /// 只注册站内通道。
    pub fn in_app_only() -> Self {
        Self {
            channels: vec![Box::new(InAppChannel)],
        }
    }

    /// 按配置注册通道：站内始终存在，邮件/Webhook 仅在启用时加入。
    pub fn from_settings(
        email: &EmailNotificationSettings,
        webhook: &WebhookNotificationSettings,
        secrets: &NotificationSecrets,
    ) -> Self {
        let mut channels: Vec<Box<dyn NotificationChannel>> = vec![Box::new(InAppChannel)];
        if email.enabled {
            channels.push(Box::new(EmailChannel::new(
                email.clone(),
                secrets.clone(),
            )));
        }
        if webhook.enabled {
            channels.push(Box::new(WebhookChannel::new(
                webhook.clone(),
                secrets.clone(),
            )));
        }
        Self { channels }
    }

    pub fn channel_ids(&self) -> Vec<&'static str> {
        self.channels.iter().map(|channel| channel.id()).collect()
    }

    pub fn channels(&self) -> &[Box<dyn NotificationChannel>] {
        &self.channels
    }

    /// 返回每个通道的错误；站内通道不会失败。
    pub async fn dispatch(&self, notification: &AlertNotification) -> Vec<(String, ChannelError)> {
        let mut errors = Vec::new();
        for channel in &self.channels {
            if channel.enabled() {
                if let Err(err) = channel.send(notification).await {
                    errors.push((channel.id().to_string(), err));
                }
            }
        }
        errors
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::inspection::types::{AlertState, AlertRecord};
    use crate::ops::types::Severity;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};

    fn alert() -> AlertRecord {
        AlertRecord {
            alert_id: "a1".into(),
            db_id: "db".into(),
            db_name: "app".into(),
            db_fingerprint: "fp".into(),
            check_id: "redundant_index".into(),
            evidence_key: "t.i".into(),
            fingerprint: "f".into(),
            severity: Severity::Warn,
            title: "冗余索引".into(),
            message: "m".into(),
            first_seen_at: 1,
            last_seen_at: 1,
            occurrences: 1,
            state: AlertState::Active,
            acknowledged: false,
            resolved_at: None,
            snapshot_id: "s1".into(),
            deliveries: Vec::new(),
        }
    }

    fn notification() -> AlertNotification {
        AlertNotification {
            kind: AlertEventKind::Raised,
            alert: alert(),
        }
    }

    #[tokio::test]
    async fn in_app_channel_is_enabled_and_succeeds() {
        let channel = InAppChannel;
        assert!(channel.enabled());
        assert!(channel.send(&notification()).await.is_ok());
        let dispatcher = NotificationDispatcher::in_app_only();
        assert_eq!(dispatcher.channel_ids(), vec!["in_app"]);
        assert!(dispatcher.dispatch(&notification()).await.is_empty());
    }

    #[test]
    fn http_status_classification_maps_4xx_permanent_and_5xx_transient() {
        assert!(matches!(
            classify_http_status(400),
            Err(ChannelError::Permanent(_))
        ));
        assert!(matches!(
            classify_http_status(401),
            Err(ChannelError::Permanent(_))
        ));
        assert!(matches!(
            classify_http_status(500),
            Err(ChannelError::Transient(_))
        ));
        assert!(matches!(
            classify_http_status(503),
            Err(ChannelError::Transient(_))
        ));
        assert!(classify_http_status(204).is_ok());
    }

    #[test]
    fn smtp_status_classification_maps_5xx_permanent_and_4xx_transient() {
        assert!(matches!(
            classify_smtp_code(535, "auth failed"),
            Err(ChannelError::Permanent(_))
        ));
        assert!(matches!(
            classify_smtp_code(550, "bad address"),
            Err(ChannelError::Permanent(_))
        ));
        assert!(matches!(
            classify_smtp_code(451, "try later"),
            Err(ChannelError::Transient(_))
        ));
        assert!(classify_smtp_code(250, "ok").is_ok());
    }

    struct ScriptedChannel {
        errors: Mutex<Vec<ChannelError>>,
        calls: Arc<AtomicU32>,
    }

    impl NotificationChannel for ScriptedChannel {
        fn id(&self) -> &'static str {
            "scripted"
        }
        fn enabled(&self) -> bool {
            true
        }
        fn send<'a>(
            &'a self,
            _notification: &'a AlertNotification,
        ) -> BoxFuture<'a, Result<SendOutcome, ChannelError>> {
            let next = {
                let mut errors = self.errors.lock().unwrap();
                if errors.is_empty() {
                    None
                } else {
                    Some(errors.remove(0))
                }
            };
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                match next {
                    Some(err) => Err(err),
                    None => Ok(SendOutcome::default()),
                }
            })
        }
    }

    fn scripted(errors: Vec<ChannelError>) -> (ScriptedChannel, Arc<AtomicU32>) {
        let calls = Arc::new(AtomicU32::new(0));
        (
            ScriptedChannel {
                errors: Mutex::new(errors),
                calls: calls.clone(),
            },
            calls,
        )
    }

    #[tokio::test]
    async fn permanent_error_is_not_retried() {
        let (channel, calls) = scripted(vec![ChannelError::Permanent("HTTP 401".into())]);
        let delivery = deliver_with_retry(
            &channel,
            &notification(),
            RetryPolicy {
                max_retries: 3,
                backoff_seconds: 0,
            },
        )
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(delivery.status, DeliveryStatus::Failed);
        assert_eq!(delivery.attempts, 1);
    }

    #[tokio::test]
    async fn transient_error_retries_up_to_max_then_fails() {
        let (channel, calls) = scripted(vec![
            ChannelError::Transient("HTTP 500".into()),
            ChannelError::Transient("HTTP 500".into()),
            ChannelError::Transient("HTTP 500".into()),
        ]);
        let delivery = deliver_with_retry(
            &channel,
            &notification(),
            RetryPolicy {
                max_retries: 2,
                backoff_seconds: 0,
            },
        )
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(delivery.attempts, 3);
        assert_eq!(delivery.status, DeliveryStatus::Failed);
    }

    #[tokio::test]
    async fn transient_then_success_marks_success() {
        let (channel, calls) = scripted(vec![ChannelError::Transient("timeout".into())]);
        let delivery = deliver_with_retry(
            &channel,
            &notification(),
            RetryPolicy {
                max_retries: 2,
                backoff_seconds: 0,
            },
        )
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(delivery.status, DeliveryStatus::Success);
        assert_eq!(delivery.attempts, 2);
    }

    struct RedactedChannel;

    impl NotificationChannel for RedactedChannel {
        fn id(&self) -> &'static str {
            "email"
        }

        fn enabled(&self) -> bool {
            true
        }

        fn send<'a>(
            &'a self,
            _notification: &'a AlertNotification,
        ) -> BoxFuture<'a, Result<SendOutcome, ChannelError>> {
            Box::pin(async move { Ok(SendOutcome { redacted: true }) })
        }
    }

    #[tokio::test]
    async fn delivery_records_redaction_flag() {
        let delivery = deliver_with_retry(
            &RedactedChannel,
            &notification(),
            RetryPolicy {
                max_retries: 0,
                backoff_seconds: 0,
            },
        )
        .await;
        assert_eq!(delivery.status, DeliveryStatus::Success);
        assert!(delivery.redacted, "delivery must carry the redacted flag");
    }
}
