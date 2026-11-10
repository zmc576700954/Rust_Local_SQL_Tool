//! 巡检编排：按检查项顺序采集，聚合成一份快照。

use uuid::Uuid;

use crate::ops::error::OpsError;
use crate::ops::inspection::checks::collect_check;
use crate::ops::inspection::redact::scrub_snapshot;
use crate::ops::inspection::types::{
    now_ts, CheckStatus, InspectionSnapshot, InspectionThresholds, SnapshotStatus,
};
use crate::ops::registry::OpsContext;
use crate::ops::types::Severity;

pub async fn run_inspection(
    ctx: &OpsContext,
    check_ids: &[String],
    thresholds: &InspectionThresholds,
    trigger: &str,
    history: &[InspectionSnapshot],
    active_unused_alert_keys: &[String],
) -> Result<InspectionSnapshot, OpsError> {
    if !ctx.is_mysql_family() && !ctx.is_postgres() {
        return Err(OpsError::Validation(
            "首期巡检只支持 MySQL/MariaDB/PostgreSQL；SQLite 仍为实验性".into(),
        ));
    }
    if ctx.db_name.trim().is_empty() {
        return Err(OpsError::Validation("巡检目标数据库名为空".into()));
    }
    if check_ids.is_empty() {
        return Err(OpsError::Validation("巡检检查项为空".into()));
    }

    let started_at = now_ts();
    let mut checks = Vec::with_capacity(check_ids.len());
    for check_id in check_ids {
        checks.push(
            collect_check(
                ctx,
                check_id,
                thresholds,
                history,
                started_at,
                active_unused_alert_keys,
            )
            .await,
        );
    }

    let status = aggregate_status(&checks);
    let mut snapshot = InspectionSnapshot {
        snapshot_id: Uuid::new_v4().to_string(),
        db_id: ctx.db_id.clone(),
        db_name: ctx.db_name.clone(),
        db_fingerprint: ctx.db_fingerprint.clone(),
        started_at,
        finished_at: now_ts(),
        trigger: trigger.to_string(),
        status,
        check_ids: check_ids.to_vec(),
        checks,
    };
    // 源头防线：检查文案可能夹带驱动返回的连接串/口令，落库前统一删除。
    scrub_snapshot(&mut snapshot, &[]);
    Ok(snapshot)
}

fn aggregate_status(checks: &[crate::ops::inspection::types::CheckResult]) -> SnapshotStatus {
    let mut status = SnapshotStatus::Ok;
    for check in checks {
        match check.status {
            CheckStatus::Critical => return SnapshotStatus::Critical,
            CheckStatus::Warning if status == SnapshotStatus::Ok => {
                status = SnapshotStatus::Warning
            }
            _ => {}
        }
    }
    status
}

pub fn status_from_max_severity(severities: impl IntoIterator<Item = Severity>) -> SnapshotStatus {
    let mut status = SnapshotStatus::Ok;
    for severity in severities {
        match severity {
            Severity::Critical => return SnapshotStatus::Critical,
            Severity::Warn => status = SnapshotStatus::Warning,
            Severity::Info => {}
        }
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::inspection::types::CheckResult;

    fn check(id: &str, status: CheckStatus) -> CheckResult {
        CheckResult {
            check_id: id.into(),
            title: id.into(),
            status,
            rule: "r".into(),
            summary: "s".into(),
            evidence: Vec::new(),
            checked_objects: 0,
            duration_ms: 1,
            skipped_reason: None,
            not_applicable: Vec::new(),
        }
    }

    #[test]
    fn aggregate_status_prefers_critical() {
        assert_eq!(aggregate_status(&[]), SnapshotStatus::Ok);
        assert_eq!(
            aggregate_status(&[check("a", CheckStatus::Skipped), check("b", CheckStatus::Ok)]),
            SnapshotStatus::Ok
        );
        assert_eq!(
            aggregate_status(&[check("a", CheckStatus::Warning)]),
            SnapshotStatus::Warning
        );
        assert_eq!(
            aggregate_status(&[
                check("a", CheckStatus::Warning),
                check("b", CheckStatus::Critical)
            ]),
            SnapshotStatus::Critical
        );
    }
}
