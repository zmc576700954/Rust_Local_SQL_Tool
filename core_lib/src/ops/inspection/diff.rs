//! 快照 diff：对比同一数据库指纹的相邻两次巡检。

use std::collections::{BTreeMap, BTreeSet};

use crate::ops::inspection::types::{
    ChangedEvidence, CheckStatusChange, DiffEvidence, EvidenceItem, InspectionSnapshot,
    SnapshotDiff,
};

type EvidenceKey = (String, String);

fn to_diff(check_id: &str, item: &EvidenceItem) -> DiffEvidence {
    DiffEvidence {
        check_id: check_id.to_string(),
        evidence_key: item.key.clone(),
        severity: item.severity,
        message: item.message.clone(),
        fields: item.fields.clone(),
    }
}

fn evidence_equal(a: &DiffEvidence, b: &DiffEvidence) -> bool {
    a.severity == b.severity && a.message == b.message && a.fields == b.fields
}

fn collect(snapshot: &InspectionSnapshot) -> BTreeMap<EvidenceKey, DiffEvidence> {
    let mut out = BTreeMap::new();
    for check in &snapshot.checks {
        for item in &check.evidence {
            out.insert(
                (check.check_id.clone(), item.key.clone()),
                to_diff(&check.check_id, item),
            );
        }
    }
    out
}

pub fn compare_snapshots(
    previous: Option<&InspectionSnapshot>,
    current: &InspectionSnapshot,
) -> SnapshotDiff {
    let current_map = collect(current);
    let previous_map = previous.map(collect).unwrap_or_default();

    let mut added = Vec::new();
    let mut resolved = Vec::new();
    let mut changed = Vec::new();

    for (key, after) in &current_map {
        match previous_map.get(key) {
            None => added.push(after.clone()),
            Some(before) if !evidence_equal(before, after) => changed.push(ChangedEvidence {
                check_id: key.0.clone(),
                evidence_key: key.1.clone(),
                before: before.clone(),
                after: after.clone(),
            }),
            Some(_) => {}
        }
    }
    for (key, before) in &previous_map {
        if !current_map.contains_key(key) {
            resolved.push(before.clone());
        }
    }

    let mut check_status_changes = Vec::new();
    if let Some(prev) = previous {
        let current_status: BTreeMap<&str, _> = current
            .checks
            .iter()
            .map(|check| (check.check_id.as_str(), check.status))
            .collect();
        let mut keys: BTreeSet<&str> = BTreeSet::new();
        for check in &prev.checks {
            keys.insert(check.check_id.as_str());
        }
        for key in keys {
            if let (Some(before), Some(after)) = (
                prev.checks
                    .iter()
                    .find(|check| check.check_id == key)
                    .map(|check| check.status),
                current_status.get(key).copied(),
            ) {
                if before != after {
                    check_status_changes.push(CheckStatusChange {
                        check_id: key.to_string(),
                        before,
                        after,
                    });
                }
            }
        }
    }

    SnapshotDiff {
        previous_snapshot_id: previous.map(|snapshot| snapshot.snapshot_id.clone()),
        current_snapshot_id: current.snapshot_id.clone(),
        added,
        resolved,
        changed,
        check_status_changes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::inspection::types::{CheckResult, CheckStatus, SnapshotStatus};
    use crate::ops::types::Severity;
    use serde_json::json;

    fn snapshot(id: &str, checks: Vec<CheckResult>) -> InspectionSnapshot {
        InspectionSnapshot {
            snapshot_id: id.into(),
            db_id: "db-1".into(),
            db_name: "app".into(),
            db_fingerprint: "fp-1".into(),
            started_at: 1,
            finished_at: 2,
            trigger: "manual".into(),
            status: SnapshotStatus::Warning,
            check_ids: checks.iter().map(|check| check.check_id.clone()).collect(),
            checks,
        }
    }

    fn check(id: &str, status: CheckStatus, evidence: Vec<EvidenceItem>) -> CheckResult {
        CheckResult {
            check_id: id.into(),
            title: id.into(),
            status,
            rule: "rule".into(),
            summary: "summary".into(),
            evidence,
            checked_objects: 1,
            duration_ms: 1,
            skipped_reason: None,
        }
    }

    fn evidence(key: &str, severity: Severity, message: &str, value: serde_json::Value) -> EvidenceItem {
        EvidenceItem::new(key, severity, message.into(), value)
    }

    #[test]
    fn first_run_reports_all_as_added() {
        let current = snapshot(
            "s2",
            vec![check(
                "redundant_index",
                CheckStatus::Warning,
                vec![evidence("t.i", Severity::Warn, "dup", json!({"table": "t"}))],
            )],
        );
        let diff = compare_snapshots(None, &current);
        assert!(diff.previous_snapshot_id.is_none());
        assert_eq!(diff.added.len(), 1);
        assert!(diff.resolved.is_empty());
        assert!(diff.changed.is_empty());
    }

    #[test]
    fn detects_added_resolved_and_changed() {
        let previous = snapshot(
            "s1",
            vec![check(
                "redundant_index",
                CheckStatus::Warning,
                vec![
                    evidence("gone", Severity::Warn, "gone", json!({"n": 1})),
                    evidence("changed", Severity::Warn, "before", json!({"n": 1})),
                    evidence("same", Severity::Warn, "same", json!({"n": 1})),
                ],
            )],
        );
        let current = snapshot(
            "s2",
            vec![check(
                "redundant_index",
                CheckStatus::Warning,
                vec![
                    evidence("changed", Severity::Critical, "after", json!({"n": 2})),
                    evidence("same", Severity::Warn, "same", json!({"n": 1})),
                    evidence("new", Severity::Warn, "new", json!({"n": 3})),
                ],
            )],
        );
        let diff = compare_snapshots(Some(&previous), &current);
        assert_eq!(diff.added.len(), 1);
        assert_eq!(diff.added[0].evidence_key, "new");
        assert_eq!(diff.resolved.len(), 1);
        assert_eq!(diff.resolved[0].evidence_key, "gone");
        assert_eq!(diff.changed.len(), 1);
        assert_eq!(diff.changed[0].evidence_key, "changed");
        assert_eq!(diff.changed[0].before.message, "before");
        assert_eq!(diff.changed[0].after.message, "after");
    }

    #[test]
    fn detects_check_status_change() {
        let previous = snapshot(
            "s1",
            vec![check("unused_index", CheckStatus::Skipped, vec![])],
        );
        let current = snapshot(
            "s2",
            vec![check("unused_index", CheckStatus::Ok, vec![])],
        );
        let diff = compare_snapshots(Some(&previous), &current);
        assert_eq!(diff.check_status_changes.len(), 1);
        assert_eq!(diff.check_status_changes[0].before, CheckStatus::Skipped);
        assert_eq!(diff.check_status_changes[0].after, CheckStatus::Ok);
    }
}
