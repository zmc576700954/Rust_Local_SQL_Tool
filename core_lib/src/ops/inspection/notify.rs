//! 巡检通知的配置、凭据存储与内容构造。
//!
//! 凭据（SMTP 密码、Webhook URL 与自定义头）不进入 inspections.json，也不进入配置文件，
//! 而是写到与巡检状态同目录的 `notification-secrets.json`，Unix 下权限 0600。
//! 发送内容在离开进程前统一脱敏，避免把连接串/口令带进邮件或 Webhook。

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::ops::error::OpsError;
use crate::ops::inspection::alert::{AlertEventKind, AlertNotification};
use crate::ops::types::Severity;

/// 应用关闭后巡检与通知都会停止；该说明同时出现在接口、邮件与 Webhook 中。
pub const SCOPE_NOTE: &str = "巡检调度器与通知发送都运行在应用进程内：关闭应用后巡检与通知都会停止，桌面通知仅在应用运行时有效，不会有人在后台持续监控。";

// ---------------------------------------------------------------------------
// 配置
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmailSecurity {
    /// 明文 SMTP（本地调试 / 内网中继）。
    Plain,
    StartTls,
    Ssl,
}

impl Default for EmailSecurity {
    fn default() -> Self {
        EmailSecurity::StartTls
    }
}

fn default_smtp_port() -> u16 {
    587
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmailNotificationSettings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub host: String,
    #[serde(default = "default_smtp_port")]
    pub port: u16,
    #[serde(default)]
    pub security: EmailSecurity,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub from: String,
    #[serde(default)]
    pub to: Vec<String>,
}

impl Default for EmailNotificationSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            host: String::new(),
            port: default_smtp_port(),
            security: EmailSecurity::default(),
            username: String::new(),
            from: String::new(),
            to: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WebhookNotificationSettings {
    #[serde(default)]
    pub enabled: bool,
    /// 自定义头名称；头值作为凭据保存在 0600 文件中。
    #[serde(default)]
    pub header_names: Vec<String>,
}

impl Default for WebhookNotificationSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            header_names: Vec::new(),
        }
    }
}

fn default_max_retries() -> u32 {
    2
}

fn default_backoff_seconds() -> u64 {
    5
}

fn default_max_per_window() -> usize {
    20
}

fn default_window_seconds() -> i64 {
    3600
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NotificationSettings {
    #[serde(default)]
    pub email: EmailNotificationSettings,
    #[serde(default)]
    pub webhook: WebhookNotificationSettings,
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    #[serde(default = "default_backoff_seconds")]
    pub backoff_seconds: u64,
    /// 每个时间窗内最多发送多少次（按事件 × 通道计数）。
    #[serde(default = "default_max_per_window")]
    pub max_per_window: usize,
    #[serde(default = "default_window_seconds")]
    pub window_seconds: i64,
}

impl Default for NotificationSettings {
    fn default() -> Self {
        Self {
            email: EmailNotificationSettings::default(),
            webhook: WebhookNotificationSettings::default(),
            max_retries: default_max_retries(),
            backoff_seconds: default_backoff_seconds(),
            max_per_window: default_max_per_window(),
            window_seconds: default_window_seconds(),
        }
    }
}

// ---------------------------------------------------------------------------
// 凭据（0600 文件）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NotificationSecrets {
    #[serde(default)]
    pub smtp_password: String,
    #[serde(default)]
    pub webhook_url: String,
    #[serde(default)]
    pub webhook_headers: BTreeMap<String, String>,
}

impl NotificationSecrets {
    /// 所有需要从文本里抹掉的敏感值。
    pub fn redaction_values(&self) -> Vec<String> {
        let mut values = Vec::new();
        if !self.smtp_password.is_empty() {
            values.push(self.smtp_password.clone());
        }
        if !self.webhook_url.is_empty() {
            values.push(self.webhook_url.clone());
        }
        for value in self.webhook_headers.values() {
            if !value.is_empty() {
                values.push(value.clone());
            }
        }
        values
    }

    pub fn masked_view(&self) -> Value {
        json!({
            "smtp_password_set": !self.smtp_password.is_empty(),
            "webhook_url_set": !self.webhook_url.is_empty(),
            "webhook_url_masked": mask_url(&self.webhook_url),
            "webhook_headers": self.webhook_headers.keys().cloned().collect::<Vec<_>>(),
        })
    }
}

/// 只展示可辨识的前缀，隐藏路径与查询串中的 token。
pub fn mask_url(url: &str) -> String {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let Some(scheme_end) = trimmed.find("://") else {
        return "***".into();
    };
    let prefix = &trimmed[..scheme_end + 3];
    let rest = &trimmed[scheme_end + 3..];
    let host_end = rest
        .find(['/', '?', '#'])
        .unwrap_or(rest.len());
    let host = &rest[..host_end];
    let host_without_user = host.rsplit('@').next().unwrap_or(host);
    format!("{prefix}{host_without_user}/***")
}

pub struct NotificationSecretStore {
    file: Option<PathBuf>,
    secrets: NotificationSecrets,
}

impl NotificationSecretStore {
    pub fn new(file: Option<PathBuf>) -> Self {
        let secrets = file
            .as_ref()
            .map(|path| Self::load(path))
            .unwrap_or_default();
        Self { file, secrets }
    }

    fn load(path: &Path) -> NotificationSecrets {
        let content = match std::fs::read_to_string(path) {
            Ok(content) => content,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return NotificationSecrets::default();
            }
            Err(err) => {
                tracing::warn!(path = %path.display(), %err, "通知凭据文件读取失败");
                return NotificationSecrets::default();
            }
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(metadata) = std::fs::metadata(path) {
                if metadata.permissions().mode() & 0o777 != 0o600 {
                    tracing::warn!(path = %path.display(), "通知凭据文件权限不是 0600，正在收紧");
                    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
                }
            }
        }
        serde_json::from_str(&content).unwrap_or_else(|err| {
            tracing::warn!(path = %path.display(), %err, "通知凭据文件解析失败，按空凭据处理");
            NotificationSecrets::default()
        })
    }

    pub fn secrets(&self) -> &NotificationSecrets {
        &self.secrets
    }

    pub fn save(&mut self, secrets: NotificationSecrets) -> Result<(), OpsError> {
        self.secrets = secrets;
        self.persist()
    }

    fn persist(&self) -> Result<(), OpsError> {
        let Some(path) = self.file.as_ref() else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec_pretty(&self.secrets)?;
        let mut options = std::fs::OpenOptions::new();
        options.create(true).truncate(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path)?;
        file.write_all(&json)?;
        file.flush()?;
        Ok(())
    }
}

/// inspections.json 同目录下的凭据文件；无文件时使用内存存储。
pub fn secrets_path_for(inspections_file: &Path) -> PathBuf {
    inspections_file.with_file_name("notification-secrets.json")
}

// ---------------------------------------------------------------------------
// 内容与脱敏
// ---------------------------------------------------------------------------

pub struct NotificationContent {
    pub subject: String,
    pub text: String,
    pub payload: Value,
}

pub fn event_label(kind: AlertEventKind) -> &'static str {
    match kind {
        AlertEventKind::Raised => "新建",
        AlertEventKind::Escalated => "严重度升级",
        AlertEventKind::Resolved => "已恢复",
    }
}

pub fn severity_label(severity: Severity) -> &'static str {
    match severity {
        Severity::Info => "提示",
        Severity::Warn => "警告",
        Severity::Critical => "严重",
    }
}

fn format_ts(ts: i64) -> String {
    chrono::DateTime::<chrono::Local>::from(
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(ts.max(0) as u64),
    )
    .format("%Y-%m-%d %H:%M:%S")
    .to_string()
}

/// 构造邮件与 Webhook 的内容；所有字段先脱敏再说。
pub fn build_content(
    notification: &AlertNotification,
    secrets: &NotificationSecrets,
) -> NotificationContent {
    let alert = &notification.alert;
    let object = sanitize_text(&alert.evidence_key, secrets);
    let summary = sanitize_text(&alert.message, secrets);
    let db_name = sanitize_text(&alert.db_name, secrets);
    let title = sanitize_text(&alert.title, secrets);
    let checked_at = format_ts(alert.last_seen_at);

    let subject = format!(
        "[巡检告警][{}] {} · {} · {}",
        severity_label(alert.severity),
        title,
        db_name,
        object
    );

    let text = format!(
        "事件: {}\n库名: {}\n对象: {}\n检查项: {} ({})\n严重度: {}\n证据摘要: {}\n首次发现: {}\n最近出现: {}\n\n说明: {}\n",
        event_label(notification.kind),
        db_name,
        object,
        title,
        alert.check_id,
        severity_label(alert.severity),
        summary,
        format_ts(alert.first_seen_at),
        checked_at,
        SCOPE_NOTE,
    );

    let payload = json!({
        "event": match notification.kind {
            AlertEventKind::Raised => "raised",
            AlertEventKind::Escalated => "escalated",
            AlertEventKind::Resolved => "resolved",
        },
        "event_label": event_label(notification.kind),
        "alert_id": alert.alert_id,
        "db_name": db_name,
        "object": object,
        "check_id": alert.check_id,
        "check_title": title,
        "severity": match alert.severity {
            Severity::Info => "info",
            Severity::Warn => "warn",
            Severity::Critical => "critical",
        },
        "severity_label": severity_label(alert.severity),
        "summary": summary,
        "first_seen_at": alert.first_seen_at,
        "first_seen_at_text": format_ts(alert.first_seen_at),
        "last_seen_at": alert.last_seen_at,
        "last_seen_at_text": checked_at,
        "state": alert.state,
        "scope_note": SCOPE_NOTE,
    });

    NotificationContent {
        subject,
        text,
        payload,
    }
}

/// 脱敏：抹掉已知凭据、URL userinfo 与 password=/token= 这类键值。
pub fn sanitize_text(input: &str, secrets: &NotificationSecrets) -> String {
    let mut output = input.to_string();
    for value in secrets.redaction_values() {
        if !value.is_empty() {
            output = output.replace(&value, "***");
        }
    }
    output = redact_url_userinfo(&output);
    redact_secret_assignments(&output)
}

fn redact_url_userinfo(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(scheme_pos) = rest.find("://") {
        let (before, after) = rest.split_at(scheme_pos + 3);
        output.push_str(before);
        // 在下一个空白或引号内寻找 userinfo@。
        let end = after
            .find(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == '>' || c == '<')
            .unwrap_or(after.len());
        let (url_part, tail) = after.split_at(end);
        let at = url_part.find('@');
        let slash = url_part.find('/').unwrap_or(url_part.len());
        match at {
            Some(at) if at < slash && url_part[..at].contains(':') => {
                output.push_str("***:***@");
                output.push_str(&url_part[at + 1..]);
            }
            _ => output.push_str(url_part),
        }
        rest = tail;
    }
    output.push_str(rest);
    output
}

fn redact_secret_assignments(input: &str) -> String {
    const KEYS: [&str; 6] = ["password", "passwd", "pwd", "token", "secret", "authorization"];
    let mut output = String::with_capacity(input.len());
    let chars: Vec<char> = input.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        let lower_rest: String = chars[index..]
            .iter()
            .take(24)
            .collect::<String>()
            .to_ascii_lowercase();
        let mut matched = None;
        for key in KEYS {
            if lower_rest.starts_with(key) {
                matched = Some(key.len());
                break;
            }
        }
        match matched {
            Some(key_len) => {
                let after_key = index + key_len;
                let mut cursor = after_key;
                while cursor < chars.len() && chars[cursor].is_whitespace() {
                    cursor += 1;
                }
                if cursor < chars.len() && (chars[cursor] == '=' || chars[cursor] == ':') {
                    let mut value_start = cursor + 1;
                    while value_start < chars.len() && chars[value_start].is_whitespace() {
                        value_start += 1;
                    }
                    let mut value_end = value_start;
                    while value_end < chars.len()
                        && !chars[value_end].is_whitespace()
                        && !matches!(chars[value_end], '&' | ';' | ',' | '"' | '\'')
                    {
                        value_end += 1;
                    }
                    output.extend(&chars[index..value_start]);
                    output.push_str("***");
                    index = value_end;
                } else {
                    output.push(chars[index]);
                    index += 1;
                }
            }
            None => {
                output.push(chars[index]);
                index += 1;
            }
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::inspection::types::{AlertRecord, AlertState};

    fn secrets() -> NotificationSecrets {
        NotificationSecrets {
            smtp_password: "smtp-super-secret".into(),
            webhook_url: "https://hooks.example.com/services/T000/B000/abcdef".into(),
            webhook_headers: BTreeMap::from([(
                "Authorization".to_string(),
                "Bearer webhook-token-123".to_string(),
            )]),
        }
    }

    fn alert(message: &str, evidence_key: &str) -> AlertRecord {
        AlertRecord {
            alert_id: "a1".into(),
            db_id: "db-1".into(),
            db_name: "app".into(),
            db_fingerprint: "fp".into(),
            check_id: "unused_index".into(),
            evidence_key: evidence_key.into(),
            fingerprint: "f".into(),
            severity: Severity::Critical,
            title: "未使用索引".into(),
            message: message.into(),
            first_seen_at: 1_700_000_000,
            last_seen_at: 1_700_000_100,
            occurrences: 2,
            state: AlertState::Active,
            acknowledged: false,
            resolved_at: None,
            snapshot_id: "s1".into(),
            deliveries: Vec::new(),
        }
    }

    #[test]
    fn content_contains_required_fields() {
        let notification = AlertNotification {
            kind: AlertEventKind::Raised,
            alert: alert("连续 3 次为 0", "t.idx_a"),
        };
        let content = build_content(&notification, &secrets());
        for expected in ["app", "t.idx_a", "unused_index", "严重", "连续 3 次为 0", "首次发现"] {
            assert!(content.text.contains(expected), "missing {expected}");
        }
        assert_eq!(content.payload["db_name"], "app");
        assert_eq!(content.payload["object"], "t.idx_a");
        assert_eq!(content.payload["severity"], "critical");
    }

    #[test]
    fn sanitize_removes_secrets_and_connection_strings() {
        let secrets = secrets();
        let raw = "连接 mysql://root:rootpass@127.0.0.1:3306/app 失败；password=plain123 token:abc456 Bearer webhook-token-123 smtp-super-secret";
        let clean = sanitize_text(raw, &secrets);
        for leaked in [
            "rootpass",
            "plain123",
            "abc456",
            "webhook-token-123",
            "smtp-super-secret",
        ] {
            assert!(!clean.contains(leaked), "leaked {leaked}: {clean}");
        }
        assert!(clean.contains("mysql://***:***@127.0.0.1:3306/app"));
    }

    #[test]
    fn mask_url_hides_path_and_userinfo() {
        assert_eq!(
            mask_url("https://user:pass@hooks.example.com/services/x?token=1"),
            "https://hooks.example.com/***"
        );
        assert_eq!(mask_url(""), "");
    }

    #[test]
    fn secret_store_writes_0600_and_round_trips() {
        let dir = std::env::temp_dir().join(format!(
            "inspection-notify-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("notification-secrets.json");
        let mut store = NotificationSecretStore::new(Some(path.clone()));
        store.save(secrets()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let reloaded = NotificationSecretStore::new(Some(path));
        assert_eq!(reloaded.secrets(), &secrets());
    }
}
