//! 定期巡检（阶段 1）：只读规则检查、应用内调度、快照 diff 与站内告警。

pub mod alert;
pub mod checks;
pub mod diff;
pub mod notify;
pub mod presets;
pub mod redact;
pub mod runner;
pub mod scheduler;
pub mod sql;
pub mod store;
pub mod types;

pub use alert::{
    classify_http_status, classify_smtp_code, deliver_with_retry, AlertEventKind, AlertNotification,
    ChannelError, EmailChannel, InAppChannel, NotificationChannel, NotificationDispatcher,
    RetryPolicy, SendOutcome, WebhookChannel,
};
pub use checks::collect_check;
pub use diff::compare_snapshots;
pub use notify::{
    build_content, EmailNotificationSettings, EmailSecurity, NotificationSecretStore,
    NotificationSecrets, NotificationSettings, WebhookNotificationSettings, SCOPE_NOTE,
};
pub use presets::{
    all_check_ids, check_descriptor, preset_by_id, presets, resolve_check_ids, CheckDescriptor,
    CHECK_DESCRIPTORS,
};
pub use redact::{
    contains_sensitive_shape, scrub_alert, scrub_check, scrub_diff, scrub_evidence, scrub_json,
    scrub_run_result, scrub_snapshot, scrub_text,
};
pub use runner::run_inspection;
pub use scheduler::{compute_next_run, validate_schedule};
pub use store::InspectionStore;
pub use types::*;
