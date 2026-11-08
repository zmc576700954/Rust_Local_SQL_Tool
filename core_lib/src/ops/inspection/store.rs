//! 巡检快照 / 计划 / 告警的本地持久化。
//!
//! 单个 JSON 文件保存：计划、按数据库指纹分组的快照历史、告警记录。
//! 快照按数据库指纹保留最近 30 次；告警按事件指纹去重，恢复后保留历史。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::ops::error::OpsError;
use crate::ops::inspection::diff::compare_snapshots;
use crate::ops::inspection::presets::check_descriptor;
use crate::ops::inspection::scheduler::compute_next_run;
use crate::ops::inspection::types::{
    fingerprint, max_severity, AlertRecord, AlertState, CheckStatus, EvidenceItem,
    InspectionRunResult, InspectionSchedule, InspectionSnapshot, SnapshotSummary,
    CHECK_UNUSED_INDEX,
};
use crate::ops::types::Severity;

const MAX_SNAPSHOTS_PER_DB: usize = 30;
const MAX_ALERTS: usize = 500;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InspectionData {
    #[serde(default)]
    schedule: InspectionSchedule,
    #[serde(default)]
    snapshots: HashMap<String, Vec<InspectionSnapshot>>,
    #[serde(default)]
    alerts: Vec<AlertRecord>,
}

impl Default for InspectionData {
    fn default() -> Self {
        Self {
            schedule: InspectionSchedule::default(),
            snapshots: HashMap::new(),
            alerts: Vec::new(),
        }
    }
}

pub struct InspectionStore {
    file: Option<PathBuf>,
    data: InspectionData,
}

impl InspectionStore {
    pub fn new(file: Option<PathBuf>) -> Self {
        let data = file
            .as_ref()
            .and_then(|path| match std::fs::read_to_string(path) {
                Ok(content) => match serde_json::from_str::<InspectionData>(&content) {
                    Ok(data) => Some(data),
                    Err(err) => {
                        tracing::warn!(
                            path = %path.display(),
                            %err,
                            "巡检状态文件解析失败，使用空状态"
                        );
                        None
                    }
                },
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
                Err(err) => {
                    tracing::warn!(path = %path.display(), %err, "巡检状态文件读取失败");
                    None
                }
            })
            .unwrap_or_default();
        Self { file, data }
    }

    pub fn schedule(&self) -> &InspectionSchedule {
        &self.data.schedule
    }

    pub fn save_schedule(&mut self, schedule: InspectionSchedule) -> Result<(), OpsError> {
        self.data.schedule = schedule;
        self.persist()
    }

    pub fn record_error(&mut self, message: impl Into<String>) -> Result<(), OpsError> {
        self.data.schedule.last_error = Some(message.into());
        self.persist()
    }

    /// 调度执行失败：记录错误并把下一次运行时间推进一个周期，避免循环重试。
    pub fn record_failed_run(
        &mut self,
        message: impl Into<String>,
        now: i64,
    ) -> Result<(), OpsError> {
        self.data.schedule.last_error = Some(message.into());
        self.data.schedule.last_run_at = Some(now);
        self.data.schedule.next_run_at = if self.data.schedule.enabled {
            compute_next_run(&self.data.schedule, now).ok()
        } else {
            None
        };
        self.persist()
    }

    pub fn latest(&self, db_fingerprint: &str) -> Option<&InspectionSnapshot> {
        self.data
            .snapshots
            .get(db_fingerprint)
            .and_then(|snapshots| snapshots.last())
    }

    /// 按 db_id 查最近一次快照。用于数据库暂时不可连接时仍能查看历史。
    pub fn latest_for_db_id(&self, db_id: &str) -> Option<&InspectionSnapshot> {
        self.data
            .snapshots
            .values()
            .flat_map(|snapshots| snapshots.iter())
            .filter(|snapshot| snapshot.db_id == db_id)
            .max_by_key(|snapshot| snapshot.finished_at)
    }

    pub fn history_for_db_id(&self, db_id: &str, limit: usize) -> Vec<SnapshotSummary> {
        let mut snapshots: Vec<&InspectionSnapshot> = self
            .data
            .snapshots
            .values()
            .flat_map(|snapshots| snapshots.iter())
            .filter(|snapshot| snapshot.db_id == db_id)
            .collect();
        snapshots.sort_by(|a, b| b.finished_at.cmp(&a.finished_at));
        snapshots
            .into_iter()
            .take(limit.max(1))
            .map(InspectionSnapshot::summary)
            .collect()
    }

    pub fn alerts_for_db_id(&self, db_id: &str, limit: usize) -> Vec<AlertRecord> {
        let mut alerts: Vec<AlertRecord> = self
            .data
            .alerts
            .iter()
            .filter(|alert| alert.db_id == db_id)
            .cloned()
            .collect();
        alerts.sort_by(|a, b| b.last_seen_at.cmp(&a.last_seen_at));
        alerts.truncate(limit.max(1));
        alerts
    }

    pub fn unread_count_for_db_id(&self, db_id: &str) -> usize {
        self.data
            .alerts
            .iter()
            .filter(|alert| {
                alert.db_id == db_id
                    && alert.state == AlertState::Active
                    && !alert.acknowledged
            })
            .count()
    }

    pub fn previous(&self, db_fingerprint: &str) -> Option<&InspectionSnapshot> {
        self.data
            .snapshots
            .get(db_fingerprint)
            .and_then(|snapshots| snapshots.iter().rev().nth(1))
    }

    /// 按时间顺序返回最近 `limit` 份快照，供跨快照规则（如未使用索引窗口）判断。
    pub fn snapshots_for_fingerprint(
        &self,
        db_fingerprint: &str,
        limit: usize,
    ) -> Vec<InspectionSnapshot> {
        let Some(snapshots) = self.data.snapshots.get(db_fingerprint) else {
            return Vec::new();
        };
        let limit = limit.max(1);
        let start = snapshots.len().saturating_sub(limit);
        snapshots[start..].to_vec()
    }

    /// 当前活跃告警的 evidence_key 集合，用于让对应检查产出可判定的恢复证据。
    pub fn active_alert_evidence_keys(
        &self,
        db_fingerprint: &str,
        check_id: &str,
    ) -> Vec<String> {
        self.data
            .alerts
            .iter()
            .filter(|alert| {
                alert.db_fingerprint == db_fingerprint
                    && alert.check_id == check_id
                    && alert.state == AlertState::Active
            })
            .map(|alert| alert.evidence_key.clone())
            .collect()
    }

    pub fn history(&self, db_fingerprint: &str, limit: usize) -> Vec<SnapshotSummary> {
        self.data
            .snapshots
            .get(db_fingerprint)
            .map(|snapshots| {
                snapshots
                    .iter()
                    .rev()
                    .take(limit.max(1))
                    .map(InspectionSnapshot::summary)
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn snapshot_by_id(&self, snapshot_id: &str) -> Option<&InspectionSnapshot> {
        self.data
            .snapshots
            .values()
            .flat_map(|snapshots| snapshots.iter())
            .find(|snapshot| snapshot.snapshot_id == snapshot_id)
    }

    pub fn alerts(&self, db_fingerprint: Option<&str>, limit: usize) -> Vec<AlertRecord> {
        let mut alerts: Vec<AlertRecord> = self
            .data
            .alerts
            .iter()
            .filter(|alert| {
                db_fingerprint
                    .map(|fingerprint| alert.db_fingerprint == fingerprint)
                    .unwrap_or(true)
            })
            .cloned()
            .collect();
        alerts.sort_by(|a, b| b.last_seen_at.cmp(&a.last_seen_at));
        alerts.truncate(limit.max(1));
        alerts
    }

    pub fn unread_count(&self, db_fingerprint: Option<&str>) -> usize {
        self.data
            .alerts
            .iter()
            .filter(|alert| {
                alert.state == AlertState::Active
                    && !alert.acknowledged
                    && db_fingerprint
                        .map(|fingerprint| alert.db_fingerprint == fingerprint)
                        .unwrap_or(true)
            })
            .count()
    }

    pub fn acknowledge(&mut self, alert_id: &str) -> Result<AlertRecord, OpsError> {
        let alert = self
            .data
            .alerts
            .iter_mut()
            .find(|alert| alert.alert_id == alert_id)
            .ok_or_else(|| OpsError::NotFound(format!("告警不存在: {alert_id}")))?;
        alert.acknowledged = true;
        let cloned = alert.clone();
        self.persist()?;
        Ok(cloned)
    }

    /// 落快照、与上一份对比、更新告警，并推进计划的下一次运行时间。
    pub fn record_snapshot(
        &mut self,
        snapshot: InspectionSnapshot,
    ) -> Result<InspectionRunResult, OpsError> {
        let fingerprint = snapshot.db_fingerprint.clone();
        let previous = self.latest(&fingerprint).cloned();
        let diff = compare_snapshots(previous.as_ref(), &snapshot);
        let alerts = self.reconcile_alerts(&snapshot);

        let entry = self.data.snapshots.entry(fingerprint).or_default();
        entry.push(snapshot.clone());
        if entry.len() > MAX_SNAPSHOTS_PER_DB {
            let overflow = entry.len() - MAX_SNAPSHOTS_PER_DB;
            entry.drain(0..overflow);
        }

        self.data.schedule.last_run_at = Some(snapshot.finished_at);
        self.data.schedule.last_error = None;
        if self.data.schedule.enabled {
            self.data.schedule.next_run_at = compute_next_run(&self.data.schedule, snapshot.finished_at).ok();
        } else {
            self.data.schedule.next_run_at = None;
        }

        self.trim_alerts();
        self.persist()?;
        Ok(InspectionRunResult {
            diff,
            alerts,
            snapshot,
        })
    }

    fn reconcile_alerts(
        &mut self,
        snapshot: &InspectionSnapshot,
    ) -> Vec<AlertRecord> {
        let mut touched: BTreeMap<String, AlertRecord> = BTreeMap::new();
        let check_ids: Vec<&str> = snapshot
            .checks
            .iter()
            .map(|check| check.check_id.as_str())
            .collect();
        let current: HashMap<(String, String), &EvidenceItem> = snapshot
            .checks
            .iter()
            .flat_map(|check| {
                check
                    .evidence
                    .iter()
                    .map(move |item| ((check.check_id.clone(), item.key.clone()), item))
            })
            .collect();
        let skipped_checks: HashSet<&str> = snapshot
            .checks
            .iter()
            .filter(|check| check.status == CheckStatus::Skipped)
            .map(|check| check.check_id.as_str())
            .collect();

        // 1. 本轮纳入检查、但当前不再出现的活跃告警 → 恢复。
        for alert in self.data.alerts.iter_mut() {
            if alert.db_fingerprint != snapshot.db_fingerprint
                || alert.state != AlertState::Active
                || !check_ids.contains(&alert.check_id.as_str())
            {
                continue;
            }
            // 检查被跳过时无法判断对象是否恢复，保持告警原状。
            if skipped_checks.contains(alert.check_id.as_str()) {
                continue;
            }
            let current_item = current.get(&(alert.check_id.clone(), alert.evidence_key.clone()));
            let should_resolve = if alert.check_id == CHECK_UNUSED_INDEX {
                // 只有真正观测到 COUNT_STAR>0 才恢复；窗口重置、统计降级或索引缺失都不恢复，
                // 否则重启后告警会来回开关。
                current_item
                    .and_then(|item| item.fields.get("count_star"))
                    .and_then(|value| value.as_u64())
                    .map(|value| value > 0)
                    .unwrap_or(false)
            } else {
                current_item
                    .map(|item| item.severity == Severity::Info)
                    .unwrap_or(true)
            };
            if should_resolve {
                alert.state = AlertState::Resolved;
                alert.resolved_at = Some(snapshot.finished_at);
                alert.last_seen_at = snapshot.finished_at;
                alert.snapshot_id = snapshot.snapshot_id.clone();
                touched.insert(alert.alert_id.clone(), alert.clone());
            }
        }

        // 2. 本轮 warning/critical 证据 → 新建或更新活跃告警。
        for check in &snapshot.checks {
            for item in &check.evidence {
                if item.severity == Severity::Info {
                    continue;
                }
                let fingerprint = fingerprint(&[
                    &snapshot.db_fingerprint,
                    &check.check_id,
                    &item.key,
                ]);
                let existing_pos = self.data.alerts.iter().position(|alert| {
                    alert.fingerprint == fingerprint && alert.state == AlertState::Active
                });
                let title = check_descriptor(&check.check_id)
                    .map(|descriptor| descriptor.title.to_string())
                    .unwrap_or_else(|| check.check_id.clone());
                match existing_pos {
                    Some(pos) => {
                        let alert = &mut self.data.alerts[pos];
                        let changed = alert.severity != item.severity || alert.message != item.message;
                        alert.severity = max_severity(alert.severity, item.severity);
                        alert.message = item.message.clone();
                        alert.title = title;
                        alert.last_seen_at = snapshot.finished_at;
                        alert.occurrences = alert.occurrences.saturating_add(1);
                        alert.snapshot_id = snapshot.snapshot_id.clone();
                        if changed {
                            alert.acknowledged = false;
                        }
                        touched.insert(alert.alert_id.clone(), alert.clone());
                    }
                    None => {
                        let alert = AlertRecord {
                            alert_id: Uuid::new_v4().to_string(),
                            db_id: snapshot.db_id.clone(),
                            db_name: snapshot.db_name.clone(),
                            db_fingerprint: snapshot.db_fingerprint.clone(),
                            check_id: check.check_id.clone(),
                            evidence_key: item.key.clone(),
                            fingerprint,
                            severity: item.severity,
                            title,
                            message: item.message.clone(),
                            first_seen_at: snapshot.finished_at,
                            last_seen_at: snapshot.finished_at,
                            occurrences: 1,
                            state: AlertState::Active,
                            acknowledged: false,
                            resolved_at: None,
                            snapshot_id: snapshot.snapshot_id.clone(),
                        };
                        self.data.alerts.push(alert.clone());
                        touched.insert(alert.alert_id.clone(), alert);
                    }
                }
            }
        }

        touched.into_values().collect()
    }

    fn trim_alerts(&mut self) {
        if self.data.alerts.len() <= MAX_ALERTS {
            return;
        }
        self.data.alerts.sort_by(|a, b| {
            let a_active = a.state == AlertState::Active;
            let b_active = b.state == AlertState::Active;
            b_active
                .cmp(&a_active)
                .then_with(|| b.last_seen_at.cmp(&a.last_seen_at))
        });
        self.data.alerts.truncate(MAX_ALERTS);
    }

    fn persist(&self) -> Result<(), OpsError> {
        let Some(path) = self.file.as_ref() else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec_pretty(&self.data)?;
        let tmp = with_suffix(path, ".tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "inspections.json".into());
    file_name.push_str(suffix);
    path.with_file_name(file_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::inspection::types::{CheckResult, CheckStatus, SnapshotStatus};
    use serde_json::json;

    fn test_path() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("local-ai-sql-inspection-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir.join("inspections.json")
    }

    fn snapshot(id: &str, finished_at: i64, evidence: Vec<EvidenceItem>) -> InspectionSnapshot {
        InspectionSnapshot {
            snapshot_id: id.into(),
            db_id: "db-1".into(),
            db_name: "app".into(),
            db_fingerprint: "fp-1".into(),
            started_at: finished_at - 1,
            finished_at,
            trigger: "manual".into(),
            status: SnapshotStatus::Warning,
            check_ids: vec!["redundant_index".into()],
            checks: vec![CheckResult {
                check_id: "redundant_index".into(),
                title: "冗余索引".into(),
                status: if evidence
                    .iter()
                    .any(|item| item.severity == Severity::Critical)
                {
                    CheckStatus::Critical
                } else if evidence.is_empty() {
                    CheckStatus::Ok
                } else {
                    CheckStatus::Warning
                },
                rule: "r".into(),
                summary: "s".into(),
                evidence,
                checked_objects: 1,
                duration_ms: 1,
                skipped_reason: None,
            }],
        }
    }

    fn warn(key: &str, message: &str) -> EvidenceItem {
        EvidenceItem::new(key, Severity::Warn, message.into(), json!({"k": key}))
    }

    #[test]
    fn record_first_snapshot_creates_alerts_and_persists() {
        let path = test_path();
        let mut store = InspectionStore::new(Some(path.clone()));
        let result = store
            .record_snapshot(snapshot("s1", 100, vec![warn("t.i", "dup")]))
            .expect("record");
        assert_eq!(result.diff.added.len(), 1);
        assert_eq!(result.alerts.len(), 1);
        assert_eq!(result.alerts[0].occurrences, 1);
        assert_eq!(store.latest("fp-1").unwrap().snapshot_id, "s1");

        let reloaded = InspectionStore::new(Some(path));
        assert_eq!(reloaded.history("fp-1", 10).len(), 1);
        assert_eq!(reloaded.alerts(Some("fp-1"), 10).len(), 1);
        assert_eq!(reloaded.unread_count(Some("fp-1")), 1);
    }

    #[test]
    fn repeated_unchanged_finding_does_not_duplicate_alert() {
        let mut store = InspectionStore::new(None);
        store
            .record_snapshot(snapshot("s1", 100, vec![warn("t.i", "dup")]))
            .unwrap();
        let result = store
            .record_snapshot(snapshot("s2", 200, vec![warn("t.i", "dup")]))
            .unwrap();
        assert!(result.diff.added.is_empty());
        assert_eq!(store.alerts(Some("fp-1"), 10).len(), 1);
        let alert = &store.alerts(Some("fp-1"), 10)[0];
        assert_eq!(alert.occurrences, 2);
        assert_eq!(alert.last_seen_at, 200);
    }

    #[test]
    fn disappears_resolves_alert_and_keeps_history() {
        let mut store = InspectionStore::new(None);
        store
            .record_snapshot(snapshot("s1", 100, vec![warn("t.i", "dup")]))
            .unwrap();
        let result = store
            .record_snapshot(snapshot("s2", 200, vec![]))
            .unwrap();
        assert_eq!(result.diff.resolved.len(), 1);
        let alerts = store.alerts(Some("fp-1"), 10);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].state, AlertState::Resolved);
        assert_eq!(alerts[0].resolved_at, Some(200));
    }

    #[test]
    fn reappears_creates_new_alert_episode() {
        let mut store = InspectionStore::new(None);
        store
            .record_snapshot(snapshot("s1", 100, vec![warn("t.i", "dup")]))
            .unwrap();
        store
            .record_snapshot(snapshot("s2", 200, vec![]))
            .unwrap();
        store
            .record_snapshot(snapshot("s3", 300, vec![warn("t.i", "dup again")]))
            .unwrap();
        let alerts = store.alerts(Some("fp-1"), 10);
        assert_eq!(alerts.len(), 2);
        assert!(alerts.iter().any(|alert| alert.state == AlertState::Resolved));
        assert!(alerts.iter().any(|alert| alert.state == AlertState::Active));
    }

    #[test]
    fn info_evidence_does_not_alert() {
        let mut store = InspectionStore::new(None);
        let info = EvidenceItem::new("t.i", Severity::Info, "unused window short".into(), json!({}));
        let result = store.record_snapshot(snapshot("s1", 100, vec![info])).unwrap();
        assert_eq!(result.diff.added.len(), 1);
        assert!(result.alerts.is_empty());
        assert!(store.alerts(Some("fp-1"), 10).is_empty());
    }

    fn unused_snapshot(
        id: &str,
        finished_at: i64,
        status: CheckStatus,
        evidence: Vec<EvidenceItem>,
    ) -> InspectionSnapshot {
        InspectionSnapshot {
            snapshot_id: id.into(),
            db_id: "db-1".into(),
            db_name: "app".into(),
            db_fingerprint: "fp-1".into(),
            started_at: finished_at - 1,
            finished_at,
            trigger: "manual".into(),
            status: SnapshotStatus::Ok,
            check_ids: vec![CHECK_UNUSED_INDEX.into()],
            checks: vec![CheckResult {
                check_id: CHECK_UNUSED_INDEX.into(),
                title: "未使用索引".into(),
                status,
                rule: "r".into(),
                summary: "s".into(),
                evidence,
                checked_objects: 1,
                duration_ms: 1,
                skipped_reason: (status == CheckStatus::Skipped)
                    .then(|| "performance_schema 不可用".to_string()),
            }],
        }
    }

    fn unused_warn(key: &str) -> EvidenceItem {
        EvidenceItem::new(
            key,
            Severity::Warn,
            "连续 3 次为 0".into(),
            json!({"count_star": 0, "used": false}),
        )
    }

    fn unused_zero_info(key: &str, reset_reason: &str) -> EvidenceItem {
        EvidenceItem::new(
            key,
            Severity::Info,
            "窗口重置".into(),
            json!({
                "count_star": 0,
                "used": false,
                "window_reset": true,
                "window_reset_reason": reset_reason,
            }),
        )
    }

    fn unused_used_info(key: &str) -> EvidenceItem {
        EvidenceItem::new(
            key,
            Severity::Info,
            "COUNT_STAR>0".into(),
            json!({"count_star": 5, "used": true}),
        )
    }

    #[test]
    fn skipped_unused_check_does_not_resolve_alert() {
        let mut store = InspectionStore::new(None);
        store
            .record_snapshot(unused_snapshot(
                "s1",
                100,
                CheckStatus::Warning,
                vec![unused_warn("t.idx_a")],
            ))
            .unwrap();
        store
            .record_snapshot(unused_snapshot("s2", 200, CheckStatus::Skipped, vec![]))
            .unwrap();
        let alerts = store.alerts(Some("fp-1"), 10);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].state, AlertState::Active);
    }

    #[test]
    fn window_reset_info_does_not_resolve_unused_alert() {
        let mut store = InspectionStore::new(None);
        store
            .record_snapshot(unused_snapshot(
                "s1",
                100,
                CheckStatus::Warning,
                vec![unused_warn("t.idx_a")],
            ))
            .unwrap();
        for (index, reason) in ["instance_restart", "uptime_below_threshold"]
            .iter()
            .enumerate()
        {
            store
                .record_snapshot(unused_snapshot(
                    &format!("s{}", index + 2),
                    200 + index as i64,
                    CheckStatus::Ok,
                    vec![unused_zero_info("t.idx_a", reason)],
                ))
                .unwrap();
            let alerts = store.alerts(Some("fp-1"), 10);
            assert_eq!(alerts[0].state, AlertState::Active, "reason={reason}");
        }
    }

    #[test]
    fn used_index_resolves_unused_alert() {
        let mut store = InspectionStore::new(None);
        store
            .record_snapshot(unused_snapshot(
                "s1",
                100,
                CheckStatus::Warning,
                vec![unused_warn("t.idx_a")],
            ))
            .unwrap();
        let result = store
            .record_snapshot(unused_snapshot(
                "s2",
                200,
                CheckStatus::Ok,
                vec![unused_used_info("t.idx_a")],
            ))
            .unwrap();
        assert_eq!(result.diff.changed.len(), 1);
        let alerts = store.alerts(Some("fp-1"), 10);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].state, AlertState::Resolved);
        assert_eq!(alerts[0].resolved_at, Some(200));
    }

    #[test]
    fn acknowledge_marks_unread_count() {
        let mut store = InspectionStore::new(None);
        let result = store
            .record_snapshot(snapshot("s1", 100, vec![warn("t.i", "dup")]))
            .unwrap();
        assert_eq!(store.unread_count(Some("fp-1")), 1);
        let alert_id = result.alerts[0].alert_id.clone();
        let acked = store.acknowledge(&alert_id).unwrap();
        assert!(acked.acknowledged);
        assert_eq!(store.unread_count(Some("fp-1")), 0);
    }

    #[test]
    fn record_error_and_schedule_advance_next_run() {
        let mut store = InspectionStore::new(None);
        let mut schedule = InspectionSchedule {
            enabled: true,
            mode: crate::ops::inspection::types::ScheduleMode::Interval,
            interval_minutes: 60,
            db_id: "db-1".into(),
            db_name: "app".into(),
            ..InspectionSchedule::default()
        };
        schedule.last_error = Some("old".into());
        store.save_schedule(schedule).unwrap();
        store.record_error("connection lost").unwrap();
        assert_eq!(store.schedule().last_error.as_deref(), Some("connection lost"));
        store
            .record_snapshot(snapshot("s1", 100, vec![]))
            .unwrap();
        assert!(store.schedule().last_error.is_none());
        assert_eq!(store.schedule().last_run_at, Some(100));
        assert_eq!(store.schedule().next_run_at, Some(100 + 3600));
    }

    #[test]
    fn snapshot_history_is_newest_first_and_limited() {
        let mut store = InspectionStore::new(None);
        for index in 0..5 {
            store
                .record_snapshot(snapshot(&format!("s{index}"), index as i64, vec![]))
                .unwrap();
        }
        let history = store.history("fp-1", 3);
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].snapshot_id, "s4");
        assert_eq!(history[2].snapshot_id, "s2");
    }
}
