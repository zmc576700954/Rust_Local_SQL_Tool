//! 计划校验与 interval / cron 下一次运行时间计算。
//!
//! 调度器跟随应用进程运行，不依赖系统级 crontab。cron 使用本地时区。

use std::collections::HashSet;

use chrono::{Datelike, Duration, Local, TimeZone, Timelike};

use crate::ops::inspection::presets::resolve_check_ids;
use crate::ops::inspection::types::{InspectionSchedule, ScheduleMode};

const MAX_CRON_HORIZON_DAYS: i64 = 366 * 5;

#[derive(Debug, Clone, PartialEq, Eq)]
struct CronField {
    values: HashSet<u32>,
    wildcard: bool,
}

impl CronField {
    fn matches(&self, value: u32) -> bool {
        self.values.contains(&value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CronSpec {
    minute: CronField,
    hour: CronField,
    day_of_month: CronField,
    month: CronField,
    day_of_week: CronField,
}

fn parse_field(
    expr: &str,
    min: u32,
    max: u32,
    normalize: fn(u32) -> u32,
) -> Result<CronField, String> {
    let expr = expr.trim();
    if expr.is_empty() {
        return Err("cron 字段为空".into());
    }
    let mut values = HashSet::new();
    let wildcard = expr == "*";
    for part in expr.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err(format!("cron 字段包含空项: {expr}"));
        }
        let (range_part, step) = match part.split_once('/') {
            Some((range, step)) => {
                let step = step
                    .parse::<u32>()
                    .map_err(|_| format!("cron 步长无效: {part}"))?;
                if step == 0 {
                    return Err(format!("cron 步长不能为 0: {part}"));
                }
                (range, Some(step))
            }
            None => (part, None),
        };
        let (start, end) = if range_part == "*" {
            (min, max)
        } else if let Some((start, end)) = range_part.split_once('-') {
            let start = start
                .parse::<u32>()
                .map_err(|_| format!("cron 起始值无效: {part}"))?;
            let end = end
                .parse::<u32>()
                .map_err(|_| format!("cron 结束值无效: {part}"))?;
            (start, end)
        } else {
            let value = range_part
                .parse::<u32>()
                .map_err(|_| format!("cron 值无效: {part}"))?;
            (value, value)
        };
        if start < min || end > max || start > end {
            return Err(format!(
                "cron 值超出范围 {min}-{max}: {part}"
            ));
        }
        let step = step.unwrap_or(1);
        let mut value = start;
        while value <= end {
            values.insert(normalize(value));
            value = match value.checked_add(step) {
                Some(next) => next,
                None => break,
            };
        }
    }
    if values.is_empty() {
        return Err(format!("cron 字段没有有效值: {expr}"));
    }
    Ok(CronField { values, wildcard })
}

fn parse_cron(expr: &str) -> Result<CronSpec, String> {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(format!(
            "cron 表达式必须是 5 段（分 时 日 月 周），实际 {} 段: {expr}",
            fields.len()
        ));
    }
    let normalize_weekday = |value: u32| if value == 7 { 0 } else { value };
    Ok(CronSpec {
        minute: parse_field(fields[0], 0, 59, |value| value)?,
        hour: parse_field(fields[1], 0, 23, |value| value)?,
        day_of_month: parse_field(fields[2], 1, 31, |value| value)?,
        month: parse_field(fields[3], 1, 12, |value| value)?,
        day_of_week: parse_field(fields[4], 0, 7, normalize_weekday)?,
    })
}

impl CronSpec {
    fn matches(&self, time: &chrono::DateTime<Local>) -> bool {
        if !self.minute.matches(time.minute())
            || !self.hour.matches(time.hour())
            || !self.month.matches(time.month())
        {
            return false;
        }
        let dom_match = self.day_of_month.matches(time.day());
        let dow_match = self.day_of_week.matches(time.weekday().num_days_from_sunday());
        match (self.day_of_month.wildcard, self.day_of_week.wildcard) {
            (true, true) => true,
            (false, true) => dom_match,
            (true, false) => dow_match,
            (false, false) => dom_match || dow_match,
        }
    }
}

pub fn validate_schedule(schedule: &InspectionSchedule) -> Result<(), String> {
    if schedule.db_id.trim().is_empty() {
        return Err("请先选择巡检目标数据库".into());
    }
    if resolve_check_ids(&schedule.preset_id, &schedule.check_ids).is_none() {
        return Err(format!(
            "未知预设或自定义检查项为空: {}",
            schedule.preset_id
        ));
    }
    match schedule.mode {
        ScheduleMode::Interval => {
            if !(1..=10_080).contains(&schedule.interval_minutes) {
                return Err("间隔必须在 1 到 10080 分钟（7 天）之间".into());
            }
        }
        ScheduleMode::Cron => {
            parse_cron(&schedule.cron_expr)?;
        }
    }
    Ok(())
}

pub fn compute_next_run(schedule: &InspectionSchedule, from_ts: i64) -> Result<i64, String> {
    match schedule.mode {
        ScheduleMode::Interval => {
            if !(1..=10_080).contains(&schedule.interval_minutes) {
                return Err("间隔必须在 1 到 10080 分钟（7 天）之间".into());
            }
            Ok(from_ts + schedule.interval_minutes as i64 * 60)
        }
        ScheduleMode::Cron => {
            let spec = parse_cron(&schedule.cron_expr)?;
            let start = Local
                .timestamp_opt(from_ts, 0)
                .single()
                .ok_or_else(|| "无法把当前时间转换为本地时间".to_string())?;
            let mut cursor = start
                .with_second(0)
                .and_then(|value| value.with_nanosecond(0))
                .ok_or_else(|| "无法对齐到分钟".to_string())?
                + Duration::minutes(1);
            let limit = MAX_CRON_HORIZON_DAYS * 24 * 60;
            for _ in 0..limit {
                if spec.matches(&cursor) {
                    return Ok(cursor.timestamp());
                }
                cursor += Duration::minutes(1);
            }
            Err(format!(
                "未来 {} 年内没有匹配 cron 的时间: {}",
                MAX_CRON_HORIZON_DAYS / 366,
                schedule.cron_expr
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schedule(mode: ScheduleMode) -> InspectionSchedule {
        InspectionSchedule {
            enabled: true,
            mode,
            interval_minutes: 60,
            cron_expr: "0 3 * * *".into(),
            preset_id: "quick_health".into(),
            check_ids: Vec::new(),
            db_id: "db-1".into(),
            db_name: "app".into(),
            updated_at: 0,
            last_run_at: None,
            next_run_at: None,
            last_error: None,
        }
    }

    fn at(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> i64 {
        Local
            .with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
            .expect("valid local time")
            .timestamp()
    }

    fn local(ts: i64) -> chrono::DateTime<Local> {
        Local.timestamp_opt(ts, 0).single().expect("local time")
    }

    #[test]
    fn interval_next_run_is_from_plus_interval() {
        let schedule = schedule(ScheduleMode::Interval);
        assert_eq!(
            compute_next_run(&schedule, at(2026, 1, 1, 9, 0)).unwrap(),
            at(2026, 1, 1, 10, 0)
        );
    }

    #[test]
    fn interval_validation_rejects_zero_and_too_large() {
        let mut schedule = schedule(ScheduleMode::Interval);
        schedule.interval_minutes = 0;
        assert!(validate_schedule(&schedule).is_err());
        schedule.interval_minutes = 10_081;
        assert!(validate_schedule(&schedule).is_err());
        schedule.interval_minutes = 1;
        assert!(validate_schedule(&schedule).is_ok());
    }

    #[test]
    fn cron_every_minute_is_next_minute() {
        let mut schedule = schedule(ScheduleMode::Cron);
        schedule.cron_expr = "* * * * *".into();
        let next = local(compute_next_run(&schedule, at(2026, 1, 1, 9, 30)).unwrap());
        assert_eq!(next.hour(), 9);
        assert_eq!(next.minute(), 31);
    }

    #[test]
    fn cron_daily_window_is_respected() {
        let mut schedule = schedule(ScheduleMode::Cron);
        schedule.cron_expr = "0 3 * * *".into();
        let next = local(compute_next_run(&schedule, at(2026, 1, 1, 2, 0)).unwrap());
        assert_eq!((next.hour(), next.minute()), (3, 0));
        assert_eq!(next.day(), 1);
        let next = local(compute_next_run(&schedule, at(2026, 1, 1, 4, 0)).unwrap());
        assert_eq!(next.day(), 2);
        assert_eq!(next.hour(), 3);
    }

    #[test]
    fn cron_step_field() {
        let mut schedule = schedule(ScheduleMode::Cron);
        schedule.cron_expr = "*/15 * * * *".into();
        let next = local(compute_next_run(&schedule, at(2026, 1, 1, 10, 7)).unwrap());
        assert_eq!((next.hour(), next.minute()), (10, 15));
    }

    #[test]
    fn cron_day_of_week_matches_monday() {
        let mut schedule = schedule(ScheduleMode::Cron);
        schedule.cron_expr = "30 2 * * 1".into();
        // 2026-01-01 是周四，下一次周一为 01-05。
        let next = local(compute_next_run(&schedule, at(2026, 1, 1, 0, 0)).unwrap());
        assert_eq!(next.day(), 5);
        assert_eq!(next.weekday().num_days_from_sunday(), 1);
        assert_eq!((next.hour(), next.minute()), (2, 30));
    }

    #[test]
    fn cron_validation_rejects_malformed_expressions() {
        let mut schedule = schedule(ScheduleMode::Cron);
        for expr in ["* * *", "* * * * * *", "60 * * * *", "* 24 * * *", "*/0 * * * *"] {
            schedule.cron_expr = expr.into();
            assert!(validate_schedule(&schedule).is_err(), "must reject: {expr}");
        }
    }

    #[test]
    fn schedule_requires_db_and_known_preset() {
        let mut schedule = schedule(ScheduleMode::Interval);
        schedule.db_id.clear();
        assert!(validate_schedule(&schedule).is_err());
        schedule.db_id = "db-1".into();
        schedule.preset_id = "nope".into();
        assert!(validate_schedule(&schedule).is_err());
    }

    #[test]
    fn custom_preset_uses_explicit_check_ids() {
        let mut schedule = schedule(ScheduleMode::Interval);
        schedule.preset_id = "custom".into();
        schedule.check_ids = vec!["orphan_foreign_key".into()];
        assert!(validate_schedule(&schedule).is_ok());
        schedule.check_ids.clear();
        assert!(validate_schedule(&schedule).is_err());
    }
}
