//! 告警通知渠道抽象。
//!
//! 首期只实现「站内通知」：告警本身由 `InspectionStore` 持久化，运维中心直接读取。
//! 邮件 / Webhook 仅保留实现位，不读取任何配置，也不会真正发送。

use serde::{Deserialize, Serialize};

use crate::db::protocol::BoxFuture;
use crate::ops::inspection::types::AlertRecord;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertEventKind {
    Raised,
    Resolved,
}

#[derive(Debug, Clone)]
pub struct AlertNotification {
    pub kind: AlertEventKind,
    pub alert: AlertRecord,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelError {
    NotConfigured(String),
    Send(String),
}

impl std::fmt::Display for ChannelError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChannelError::NotConfigured(message) => write!(formatter, "未配置: {message}"),
            ChannelError::Send(message) => write!(formatter, "发送失败: {message}"),
        }
    }
}

impl std::error::Error for ChannelError {}

pub trait NotificationChannel: Send + Sync {
    fn id(&self) -> &'static str;
    fn enabled(&self) -> bool;
    fn send<'a>(
        &'a self,
        notification: &'a AlertNotification,
    ) -> BoxFuture<'a, Result<(), ChannelError>>;
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
    ) -> BoxFuture<'a, Result<(), ChannelError>> {
        Box::pin(async move { Ok(()) })
    }
}

/// 邮件通道占位：不读取配置、不发送。接口保留以便后续单独接入。
pub struct EmailChannel;

impl NotificationChannel for EmailChannel {
    fn id(&self) -> &'static str {
        "email"
    }

    fn enabled(&self) -> bool {
        false
    }

    fn send<'a>(
        &'a self,
        _notification: &'a AlertNotification,
    ) -> BoxFuture<'a, Result<(), ChannelError>> {
        Box::pin(async move {
            Err(ChannelError::NotConfigured(
                "邮件通知未实现且未配置".into(),
            ))
        })
    }
}

/// Webhook 通道占位：不读取配置、不发送。
pub struct WebhookChannel;

impl NotificationChannel for WebhookChannel {
    fn id(&self) -> &'static str {
        "webhook"
    }

    fn enabled(&self) -> bool {
        false
    }

    fn send<'a>(
        &'a self,
        _notification: &'a AlertNotification,
    ) -> BoxFuture<'a, Result<(), ChannelError>> {
        Box::pin(async move {
            Err(ChannelError::NotConfigured(
                "Webhook 通知未实现且未配置".into(),
            ))
        })
    }
}

pub struct NotificationDispatcher {
    channels: Vec<Box<dyn NotificationChannel>>,
}

impl NotificationDispatcher {
    /// 首期只注册站内通道；邮件/Webhook 不注册、不配置。
    pub fn in_app_only() -> Self {
        Self {
            channels: vec![Box::new(InAppChannel)],
        }
    }

    pub fn channel_ids(&self) -> Vec<&'static str> {
        self.channels.iter().map(|channel| channel.id()).collect()
    }

    /// 返回每个通道的错误；站内通道不会失败，占位通道不在默认列表中。
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
        }
    }

    #[tokio::test]
    async fn in_app_channel_is_enabled_and_succeeds() {
        let channel = InAppChannel;
        assert!(channel.enabled());
        let notification = AlertNotification {
            kind: AlertEventKind::Raised,
            alert: alert(),
        };
        assert!(channel.send(&notification).await.is_ok());
        let dispatcher = NotificationDispatcher::in_app_only();
        assert_eq!(dispatcher.channel_ids(), vec!["in_app"]);
        assert!(dispatcher.dispatch(&notification).await.is_empty());
    }

    #[tokio::test]
    async fn placeholder_channels_are_unconfigured() {
        let notification = AlertNotification {
            kind: AlertEventKind::Raised,
            alert: alert(),
        };
        let channels: Vec<&dyn NotificationChannel> = vec![&EmailChannel, &WebhookChannel];
        for channel in channels {
            assert!(!channel.enabled());
            match channel.send(&notification).await {
                Err(ChannelError::NotConfigured(_)) => {}
                other => panic!("expected NotConfigured, got {other:?}"),
            }
        }
    }
}
