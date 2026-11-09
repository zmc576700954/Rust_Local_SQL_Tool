//! 定期巡检的公共数据模型：检查结果、快照、diff、告警、计划与阈值。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ops::types::Severity;

/// 检查项 ID（首期 9 项，全部为只读检查）。
pub const CHECK_REDUNDANT_INDEX: &str = "redundant_index";
pub const CHECK_UNUSED_INDEX: &str = "unused_index";
pub const CHECK_MISSING_PRIMARY_KEY: &str = "missing_primary_key";
pub const CHECK_TABLE_CAPACITY: &str = "table_capacity";
pub const CHECK_CHARSET_COLLATION: &str = "charset_collation_mismatch";
pub const CHECK_ORPHAN_FOREIGN_KEY: &str = "orphan_foreign_key";
pub const CHECK_OVER_PRIVILEGED_ACCOUNT: &str = "over_privileged_account";
pub const CHECK_PASSWORDLESS_ACCOUNT: &str = "passwordless_account";
pub const CHECK_INACTIVE_ACCOUNT: &str = "inactive_account";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Ok,
    Warning,
    Critical,
    Skipped,
}

impl CheckStatus {
    pub fn rank(self) -> u8 {
        match self {
            CheckStatus::Ok => 0,
            CheckStatus::Skipped => 1,
            CheckStatus::Warning => 2,
            CheckStatus::Critical => 3,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            CheckStatus::Ok => "正常",
            CheckStatus::Warning => "警告",
            CheckStatus::Critical => "严重",
            CheckStatus::Skipped => "已跳过",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotStatus {
    Ok,
    Warning,
    Critical,
}

impl SnapshotStatus {
    pub fn from_severity(severity: Severity) -> Self {
        match severity {
            Severity::Info => SnapshotStatus::Ok,
            Severity::Warn => SnapshotStatus::Warning,
            Severity::Critical => SnapshotStatus::Critical,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvidenceItem {
    /// 检查项内部的稳定指纹键，用于跨快照 diff 与告警去重。
    pub key: String,
    pub severity: Severity,
    pub message: String,
    /// 结构化证据，字段名与检查规则一一对应。
    pub fields: Value,
}

impl EvidenceItem {
    pub fn new(key: impl Into<String>, severity: Severity, message: String, fields: Value) -> Self {
        Self {
            key: key.into(),
            severity,
            message,
            fields,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckResult {
    pub check_id: String,
    pub title: String,
    pub status: CheckStatus,
    /// 该检查的判定规则原文，UI 直接展示，避免“视情况而定”。
    pub rule: String,
    pub summary: String,
    pub evidence: Vec<EvidenceItem>,
    /// 本次实际检查的对象数量（表/索引/账号），便于确认不是静默漏检。
    pub checked_objects: usize,
    pub duration_ms: u64,
    pub skipped_reason: Option<String>,
}

impl CheckResult {
    pub fn skipped(check_id: &str, title: &str, rule: &str, reason: impl Into<String>) -> Self {
        Self {
            check_id: check_id.to_string(),
            title: title.to_string(),
            status: CheckStatus::Skipped,
            rule: rule.to_string(),
            summary: "未执行或无法判定".into(),
            evidence: Vec::new(),
            checked_objects: 0,
            duration_ms: 0,
            skipped_reason: Some(reason.into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotSummary {
    pub snapshot_id: String,
    pub db_id: String,
    pub db_name: String,
    pub db_fingerprint: String,
    pub started_at: i64,
    pub finished_at: i64,
    pub trigger: String,
    pub status: SnapshotStatus,
    pub check_ids: Vec<String>,
    pub evidence_count: usize,
    pub critical_count: usize,
    pub warning_count: usize,
    pub info_count: usize,
    pub skipped_count: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InspectionSnapshot {
    pub snapshot_id: String,
    pub db_id: String,
    pub db_name: String,
    pub db_fingerprint: String,
    pub started_at: i64,
    pub finished_at: i64,
    pub trigger: String,
    pub status: SnapshotStatus,
    pub check_ids: Vec<String>,
    pub checks: Vec<CheckResult>,
}

impl InspectionSnapshot {
    pub fn summary(&self) -> SnapshotSummary {
        let mut critical_count = 0;
        let mut warning_count = 0;
        let mut info_count = 0;
        let mut skipped_count = 0;
        let mut evidence_count = 0;
        for check in &self.checks {
            if check.status == CheckStatus::Skipped {
                skipped_count += 1;
            }
            for item in &check.evidence {
                evidence_count += 1;
                match item.severity {
                    Severity::Critical => critical_count += 1,
                    Severity::Warn => warning_count += 1,
                    Severity::Info => info_count += 1,
                }
            }
        }
        SnapshotSummary {
            snapshot_id: self.snapshot_id.clone(),
            db_id: self.db_id.clone(),
            db_name: self.db_name.clone(),
            db_fingerprint: self.db_fingerprint.clone(),
            started_at: self.started_at,
            finished_at: self.finished_at,
            trigger: self.trigger.clone(),
            status: self.status,
            check_ids: self.check_ids.clone(),
            evidence_count,
            critical_count,
            warning_count,
            info_count,
            skipped_count,
        }
    }

    /// 所有 evidence 的 `(check_id, key)` 集合，用于告警存活判断。
    pub fn evidence_fingerprints(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for check in &self.checks {
            for item in &check.evidence {
                out.push((check.check_id.clone(), item.key.clone()));
            }
        }
        out
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiffEvidence {
    pub check_id: String,
    pub evidence_key: String,
    pub severity: Severity,
    pub message: String,
    pub fields: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChangedEvidence {
    pub check_id: String,
    pub evidence_key: String,
    pub before: DiffEvidence,
    pub after: DiffEvidence,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckStatusChange {
    pub check_id: String,
    pub before: CheckStatus,
    pub after: CheckStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotDiff {
    pub previous_snapshot_id: Option<String>,
    pub current_snapshot_id: String,
    pub added: Vec<DiffEvidence>,
    pub resolved: Vec<DiffEvidence>,
    pub changed: Vec<ChangedEvidence>,
    pub check_status_changes: Vec<CheckStatusChange>,
}

impl SnapshotDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.resolved.is_empty()
            && self.changed.is_empty()
            && self.check_status_changes.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertState {
    Active,
    Resolved,
}

impl AlertState {
    pub fn label(self) -> &'static str {
        match self {
            AlertState::Active => "活跃",
            AlertState::Resolved => "已恢复",
        }
    }
}

/// 单个通道的投递状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryStatus {
    /// 尚未尝试。
    Pending,
    /// 已尝试失败，正在退避重试。
    Retrying,
    Success,
    Failed,
    /// 因限流 / 配置缺失等原因主动跳过。
    Skipped,
}

impl DeliveryStatus {
    pub fn label(self) -> &'static str {
        match self {
            DeliveryStatus::Pending => "待发送",
            DeliveryStatus::Retrying => "重试中",
            DeliveryStatus::Success => "成功",
            DeliveryStatus::Failed => "失败",
            DeliveryStatus::Skipped => "已跳过",
        }
    }
}

/// 某条告警在某个通道上的投递记录。失败必须可见，不能只写日志。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelDelivery {
    pub channel: String,
    pub status: DeliveryStatus,
    pub attempts: u32,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub last_attempt_at: Option<i64>,
    #[serde(default)]
    pub delivered_at: Option<i64>,
    /// 每次尝试的时间戳，用于核对退避间隔。
    #[serde(default)]
    pub attempt_times: Vec<i64>,
}

impl ChannelDelivery {
    pub fn pending(channel: impl Into<String>) -> Self {
        Self {
            channel: channel.into(),
            status: DeliveryStatus::Pending,
            attempts: 0,
            last_error: None,
            last_attempt_at: None,
            delivered_at: None,
            attempt_times: Vec::new(),
        }
    }

    pub fn skipped(channel: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            channel: channel.into(),
            status: DeliveryStatus::Skipped,
            attempts: 0,
            last_error: Some(reason.into()),
            last_attempt_at: None,
            delivered_at: None,
            attempt_times: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlertRecord {
    pub alert_id: String,
    pub db_id: String,
    pub db_name: String,
    pub db_fingerprint: String,
    pub check_id: String,
    pub evidence_key: String,
    /// db_fingerprint + check_id + evidence_key 的稳定指纹。
    pub fingerprint: String,
    pub severity: Severity,
    pub title: String,
    pub message: String,
    pub first_seen_at: i64,
    pub last_seen_at: i64,
    pub occurrences: u32,
    pub state: AlertState,
    pub acknowledged: bool,
    pub resolved_at: Option<i64>,
    pub snapshot_id: String,
    /// 每个通道最近一次投递的状态与错误。
    #[serde(default)]
    pub deliveries: Vec<ChannelDelivery>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleMode {
    Interval,
    Cron,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InspectionSchedule {
    pub enabled: bool,
    pub mode: ScheduleMode,
    /// 间隔模式的最小间隔为 1 分钟。
    pub interval_minutes: u64,
    pub cron_expr: String,
    pub preset_id: String,
    /// 由预设解析出的实际检查项集合；保存时固化，便于审计。
    pub check_ids: Vec<String>,
    pub db_id: String,
    pub db_name: String,
    pub updated_at: i64,
    pub last_run_at: Option<i64>,
    pub next_run_at: Option<i64>,
    pub last_error: Option<String>,
}

impl Default for InspectionSchedule {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: ScheduleMode::Interval,
            interval_minutes: 1440,
            cron_expr: "0 3 * * *".into(),
            preset_id: "quick_health".into(),
            check_ids: Vec::new(),
            db_id: String::new(),
            db_name: String::new(),
            updated_at: 0,
            last_run_at: None,
            next_run_at: None,
            last_error: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InspectionThresholds {
    pub large_table_bytes: u64,
    pub fragmentation_min_bytes: u64,
    pub fragmentation_ratio_percent: u64,
    /// 未使用索引的统计窗口；窗口不足时降级为 info。
    pub unused_index_min_uptime_hours: u64,
    /// 未使用索引升级为 warning 所需的连续零使用快照次数。
    #[serde(default = "default_unused_index_min_snapshots")]
    pub unused_index_min_snapshots: u64,
    /// 长期未登录账号所需的最小实例连续运行天数。
    pub inactive_account_min_uptime_days: u64,
    /// 孤儿外键计数上限，超过即标记 truncated。
    pub orphan_count_cap: u64,
}

pub fn default_unused_index_min_snapshots() -> u64 {
    3
}

impl Default for InspectionThresholds {
    fn default() -> Self {
        Self {
            large_table_bytes: 2 * 1024 * 1024 * 1024,
            fragmentation_min_bytes: 100 * 1024 * 1024,
            fragmentation_ratio_percent: 20,
            unused_index_min_uptime_hours: 24,
            unused_index_min_snapshots: default_unused_index_min_snapshots(),
            inactive_account_min_uptime_days: 7,
            orphan_count_cap: 100,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InspectionPreset {
    pub id: String,
    pub title: String,
    pub description: String,
    pub check_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InspectionRunResult {
    pub snapshot: InspectionSnapshot,
    pub diff: SnapshotDiff,
    /// 本次新增或状态变化的告警（含恢复）。
    pub alerts: Vec<AlertRecord>,
    /// 需要推送的事件：新建、严重度升级、恢复。未变化的活跃 finding 不产生事件（去重）。
    #[serde(default)]
    pub events: Vec<crate::ops::inspection::alert::AlertNotification>,
    /// 运行边界说明：应用关闭后巡检与通知都会停止。
    #[serde(default)]
    pub scope_note: String,
}

pub fn now_ts() -> i64 {
    chrono::Local::now().timestamp()
}

pub fn severity_rank(severity: Severity) -> u8 {
    match severity {
        Severity::Info => 0,
        Severity::Warn => 1,
        Severity::Critical => 2,
    }
}

pub fn max_severity(a: Severity, b: Severity) -> Severity {
    if severity_rank(a) >= severity_rank(b) {
        a
    } else {
        b
    }
}

pub fn status_from_evidence(evidence: &[EvidenceItem]) -> CheckStatus {
    let mut status = CheckStatus::Ok;
    for item in evidence {
        status = match item.severity {
            Severity::Info => status,
            Severity::Warn if status.rank() < CheckStatus::Warning.rank() => CheckStatus::Warning,
            Severity::Warn => status,
            Severity::Critical => CheckStatus::Critical,
        };
    }
    status
}

pub fn fingerprint(parts: &[&str]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update(b"\x1f");
    }
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_from_evidence_prefers_critical() {
        assert_eq!(status_from_evidence(&[]), CheckStatus::Ok);
        assert_eq!(
            status_from_evidence(&[EvidenceItem::new(
                "a",
                Severity::Info,
                "info".into(),
                Value::Null
            )]),
            CheckStatus::Ok
        );
        assert_eq!(
            status_from_evidence(&[EvidenceItem::new(
                "a",
                Severity::Warn,
                "warn".into(),
                Value::Null
            )]),
            CheckStatus::Warning
        );
        assert_eq!(
            status_from_evidence(&[
                EvidenceItem::new("a", Severity::Warn, "warn".into(), Value::Null),
                EvidenceItem::new("b", Severity::Critical, "crit".into(), Value::Null),
            ]),
            CheckStatus::Critical
        );
    }

    #[test]
    fn fingerprint_is_stable_and_separator_scoped() {
        let a = fingerprint(&["db", "check", "key"]);
        let b = fingerprint(&["db", "check", "key"]);
        assert_eq!(a, b);
        assert_ne!(a, fingerprint(&["db", "check", "key2"]));
        assert_ne!(fingerprint(&["a", "bc"]), fingerprint(&["ab", "c"]));
    }

    #[test]
    fn snapshot_summary_counts_by_severity() {
        let snapshot = InspectionSnapshot {
            snapshot_id: "s1".into(),
            db_id: "db".into(),
            db_name: "app".into(),
            db_fingerprint: "fp".into(),
            started_at: 1,
            finished_at: 2,
            trigger: "manual".into(),
            status: SnapshotStatus::Critical,
            check_ids: vec!["redundant_index".into()],
            checks: vec![
                CheckResult {
                    check_id: "redundant_index".into(),
                    title: "冗余索引".into(),
                    status: CheckStatus::Critical,
                    rule: "r".into(),
                    summary: "s".into(),
                    evidence: vec![
                        EvidenceItem::new("a", Severity::Warn, "w".into(), Value::Null),
                        EvidenceItem::new("b", Severity::Critical, "c".into(), Value::Null),
                        EvidenceItem::new("c", Severity::Info, "i".into(), Value::Null),
                    ],
                    checked_objects: 3,
                    duration_ms: 5,
                    skipped_reason: None,
                },
                CheckResult::skipped("unused_index", "未使用索引", "r", "权限不足"),
            ],
        };
        let summary = snapshot.summary();
        assert_eq!(summary.warning_count, 1);
        assert_eq!(summary.critical_count, 1);
        assert_eq!(summary.info_count, 1);
        assert_eq!(summary.evidence_count, 3);
        assert_eq!(summary.skipped_count, 1);
    }
}
