//! 首期 9 项只读巡检规则的采集与判定。
//!
//! 设计：SQL 采集与纯判定分离。纯判定函数只接收结构化行，便于逐条规则做单元测试；
//! 所有 SQL 统一经 `inspection::sql::fetch_read_only` 执行，巡检链路不接触写通道。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Instant;

use serde_json::json;

use crate::ops::inspection::presets::{check_descriptor, CheckDescriptor};
use crate::ops::inspection::sql::fetch_read_only;
use crate::ops::inspection::types::{
    status_from_evidence, CheckResult, CheckStatus, EvidenceItem, InspectionSnapshot,
    InspectionThresholds, CHECK_CHARSET_COLLATION, CHECK_INACTIVE_ACCOUNT,
    CHECK_MISSING_PRIMARY_KEY, CHECK_ORPHAN_FOREIGN_KEY, CHECK_OVER_PRIVILEGED_ACCOUNT,
    CHECK_PASSWORDLESS_ACCOUNT, CHECK_REDUNDANT_INDEX, CHECK_TABLE_CAPACITY, CHECK_UNUSED_INDEX,
};
use crate::db::database_ops::DatabaseOps;
use crate::ops::mysql_rows::{bool_yes, int_value, str_value, uint_value, RowMap};
use crate::ops::registry::OpsContext;
use crate::ops::types::Severity;
use crate::sql::util::{escape_sql_string, quote_ident_mysql};

// ---------------------------------------------------------------------------
// 结构化行
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexRecord {
    pub table: String,
    pub name: String,
    pub unique: bool,
    pub seq: u32,
    pub column: String,
    pub sub_part: Option<u64>,
    pub index_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKeyDef {
    pub child_table: String,
    pub constraint_name: String,
    pub child_columns: Vec<String>,
    pub parent_table: String,
    pub parent_columns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexUsageRow {
    pub table: String,
    pub index: String,
    pub count_star: u64,
    pub count_read: u64,
    pub count_write: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TablePrimaryKeyRow {
    pub table: String,
    pub engine: String,
    pub table_rows: u64,
    pub primary_index_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableCapacityRow {
    pub table: String,
    pub engine: String,
    pub table_rows: u64,
    pub data_length: u64,
    pub index_length: u64,
    pub data_free: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableCollationRow {
    pub table: String,
    pub collation: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnCollationRow {
    pub table: String,
    pub column: String,
    pub charset: Option<String>,
    pub collation: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountRow {
    pub user: String,
    pub host: String,
    pub super_priv: bool,
    pub grant_priv: bool,
    pub file_priv: bool,
    pub process_priv: bool,
    pub plugin: String,
    pub password: String,
    pub locked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountUsageRow {
    pub user: String,
    pub host: String,
    pub current_connections: u64,
    pub total_connections: u64,
}

// ---------------------------------------------------------------------------
// 行解析
// ---------------------------------------------------------------------------

fn parse_index_records(rows: &[RowMap]) -> Vec<IndexRecord> {
    let mut out = Vec::new();
    for row in rows {
        let (Some(table), Some(name), Some(column)) = (
            str_value(row, "table_name"),
            str_value(row, "index_name"),
            str_value(row, "column_name"),
        ) else {
            continue;
        };
        let unique = int_value(row, "non_unique").map(|value| value == 0).unwrap_or(false);
        let seq = uint_value(row, "seq_in_index").unwrap_or(0) as u32;
        let index_type = str_value(row, "index_type").unwrap_or_default();
        out.push(IndexRecord {
            table,
            name,
            unique,
            seq,
            column,
            sub_part: uint_value(row, "sub_part"),
            index_type,
        });
    }
    out
}

fn group_foreign_keys(rows: &[RowMap]) -> Vec<ForeignKeyDef> {
    #[derive(Default)]
    struct Builder {
        child_table: String,
        constraint_name: String,
        parent_table: String,
        entries: Vec<(u32, String, String)>,
    }
    let mut builders: BTreeMap<(String, String), Builder> = BTreeMap::new();
    for row in rows {
        let (Some(child), Some(constraint), Some(child_column), Some(parent), Some(parent_column)) = (
            str_value(row, "table_name"),
            str_value(row, "constraint_name"),
            str_value(row, "column_name"),
            str_value(row, "referenced_table_name"),
            str_value(row, "referenced_column_name"),
        ) else {
            continue;
        };
        let seq = uint_value(row, "ordinal_position").unwrap_or(0) as u32;
        let key = (child.clone(), constraint.clone());
        let entry = builders.entry(key).or_insert_with(|| Builder {
            child_table: child,
            constraint_name: constraint,
            parent_table: parent,
            entries: Vec::new(),
        });
        entry.entries.push((seq, child_column, parent_column));
    }
    builders
        .into_values()
        .map(|mut builder| {
            builder.entries.sort_by_key(|(seq, _, _)| *seq);
            ForeignKeyDef {
                child_table: builder.child_table,
                constraint_name: builder.constraint_name,
                child_columns: builder.entries.iter().map(|(_, c, _)| c.clone()).collect(),
                parent_table: builder.parent_table,
                parent_columns: builder.entries.iter().map(|(_, _, p)| p.clone()).collect(),
            }
        })
        .collect()
}

fn parse_account_rows(rows: &[RowMap]) -> Vec<AccountRow> {
    let mut out = Vec::new();
    for row in rows {
        let (Some(user), Some(host)) = (str_value(row, "user_name"), str_value(row, "host_name"))
        else {
            continue;
        };
        out.push(AccountRow {
            user,
            host,
            super_priv: bool_yes(row, "super_priv").unwrap_or(false),
            grant_priv: bool_yes(row, "grant_priv").unwrap_or(false),
            file_priv: bool_yes(row, "file_priv").unwrap_or(false),
            process_priv: bool_yes(row, "process_priv").unwrap_or(false),
            plugin: str_value(row, "plugin").unwrap_or_default(),
            password: row
                .get("password_value")
                .map(|value| value.trim().to_string())
                .unwrap_or_default(),
            locked: bool_yes(row, "account_locked").unwrap_or(false),
        });
    }
    out
}

// ---------------------------------------------------------------------------
// 纯判定：索引
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct IndexDef {
    table: String,
    name: String,
    columns: Vec<String>,
    unique: bool,
    index_type: String,
}

fn build_index_defs(records: &[IndexRecord]) -> Vec<IndexDef> {
    let mut grouped: BTreeMap<(String, String), (Vec<(u32, String)>, bool, String)> = BTreeMap::new();
    for record in records {
        let entry = grouped
            .entry((record.table.clone(), record.name.clone()))
            .or_insert_with(|| (Vec::new(), record.unique, record.index_type.clone()));
        entry.0.push((record.seq, record.column.clone()));
        entry.1 = record.unique;
        entry.2 = record.index_type.clone();
    }
    let mut out: Vec<IndexDef> = grouped
        .into_iter()
        .map(|((table, name), (mut columns, unique, index_type))| {
            columns.sort_by_key(|(seq, _)| *seq);
            IndexDef {
                table,
                name,
                columns: columns.into_iter().map(|(_, column)| column).collect(),
                unique,
                index_type,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        let a_primary = a.name.eq_ignore_ascii_case("PRIMARY");
        let b_primary = b.name.eq_ignore_ascii_case("PRIMARY");
        b_primary
            .cmp(&a_primary)
            .then_with(|| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase()))
    });
    out
}

fn is_btree(index: &IndexDef) -> bool {
    index.index_type.eq_ignore_ascii_case("BTREE")
}

fn same_columns(a: &[String], b: &[String]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b.iter())
            .all(|(left, right)| left.eq_ignore_ascii_case(right))
}

fn columns_prefix(shorter: &[String], longer: &[String]) -> bool {
    shorter.len() < longer.len()
        && shorter
            .iter()
            .zip(longer.iter())
            .all(|(left, right)| left.eq_ignore_ascii_case(right))
}

fn supports_foreign_key(index: &IndexDef, fks: &[ForeignKeyDef]) -> bool {
    fks.iter()
        .filter(|fk| fk.child_table.eq_ignore_ascii_case(&index.table))
        .any(|fk| {
            fk.child_columns.len() <= index.columns.len()
                && fk
                    .child_columns
                    .iter()
                    .zip(index.columns.iter())
                    .all(|(left, right)| left.eq_ignore_ascii_case(right))
        })
}

pub fn detect_redundant_indexes(
    records: &[IndexRecord],
    fks: &[ForeignKeyDef],
) -> (Vec<EvidenceItem>, usize) {
    let defs = build_index_defs(records);
    let checked = defs
        .iter()
        .filter(|index| is_btree(index) && !index.name.eq_ignore_ascii_case("PRIMARY"))
        .count();
    let mut redundant: BTreeMap<(String, String), (IndexDef, IndexDef, &'static str)> =
        BTreeMap::new();

    let tables: Vec<String> = defs
        .iter()
        .map(|index| index.table.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    for table in tables {
        let table_indexes: Vec<&IndexDef> = defs
            .iter()
            .filter(|index| index.table == table && is_btree(index))
            .collect();

        // 完全重复：列序列完全相同。
        for (i, first) in table_indexes.iter().enumerate() {
            for second in table_indexes.iter().skip(i + 1) {
                if !same_columns(&first.columns, &second.columns) {
                    continue;
                }
                let redundant_index = if first.unique == second.unique {
                    if first.name.eq_ignore_ascii_case("PRIMARY") {
                        second
                    } else {
                        // defs 已按 PRIMARY 优先、名称排序，后一个作为冗余方。
                        second
                    }
                } else if first.unique {
                    second
                } else {
                    first
                };
                let keeper = if redundant_index.name == first.name {
                    second
                } else {
                    first
                };
                redundant.insert(
                    (redundant_index.table.clone(), redundant_index.name.clone()),
                    ((*redundant_index).clone(), (*keeper).clone(), "duplicate"),
                );
            }
        }

        // 前缀覆盖：短索引非唯一，且被更长索引覆盖。
        for short in &table_indexes {
            if short.unique
                || short.name.eq_ignore_ascii_case("PRIMARY")
                || redundant.contains_key(&(short.table.clone(), short.name.clone()))
            {
                continue;
            }
            let mut best: Option<&IndexDef> = None;
            for long in &table_indexes {
                if !columns_prefix(&short.columns, &long.columns) {
                    continue;
                }
                let better = best
                    .map(|current| long.columns.len() < current.columns.len())
                    .unwrap_or(true);
                if better {
                    best = Some(long);
                }
            }
            if let Some(cover) = best {
                redundant.insert(
                    (short.table.clone(), short.name.clone()),
                    ((*short).clone(), (*cover).clone(), "prefix_covered"),
                );
            }
        }
    }

    let mut evidence = Vec::new();
    for (_, (redundant_index, covering, reason)) in redundant {
        let fk = supports_foreign_key(&redundant_index, fks);
        let reason_text = if reason == "duplicate" {
            "与保留索引列完全相同"
        } else {
            "列序列被更长索引前缀覆盖"
        };
        let message = format!(
            "索引 {}({}) 冗余：{}；可保留 {}({}){}",
            redundant_index.name,
            redundant_index.columns.join(", "),
            reason_text,
            covering.name,
            covering.columns.join(", "),
            if fk {
                "；该索引当前支撑外键约束，删除前需确认"
            } else {
                ""
            }
        );
        evidence.push(EvidenceItem::new(
            format!("{}.{}", redundant_index.table, redundant_index.name),
            Severity::Warn,
            message,
            json!({
                "table": redundant_index.table,
                "redundant_index": redundant_index.name,
                "redundant_columns": redundant_index.columns,
                "redundant_unique": redundant_index.unique,
                "covering_index": covering.name,
                "covering_columns": covering.columns,
                "reason": reason,
                "index_type": redundant_index.index_type,
                "supports_foreign_key": fk,
            }),
        ));
    }
    evidence.sort_by(|a, b| a.key.cmp(&b.key));
    (evidence, checked)
}

/// 未使用索引的连续快照判定窗口。
///
/// 计数只依赖快照历史：当前快照为 0 时，从最近一次快照往回累加，遇到以下情况立即断开：
/// 该快照没有执行此检查、该快照没有该索引的零使用证据（说明当时被使用或索引不存在）、
/// 历史 uptime 回退（实例重启）、历史 uptime 低于统计阈值。
#[derive(Debug, Clone, PartialEq)]
pub struct UnusedIndexWindow {
    pub consecutive_zero_snapshots: u64,
    pub required_snapshots: u64,
    pub window_start_at: Option<i64>,
    pub window_uptimes: Vec<WindowUptimePoint>,
    pub window_reset: bool,
    pub window_reset_reason: Option<String>,
    pub previous_uptime_hours: Option<u64>,
    pub window_span_hours: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WindowUptimePoint {
    pub at: i64,
    pub uptime_hours: u64,
}

fn unused_check(snapshot: &InspectionSnapshot) -> Option<&CheckResult> {
    snapshot
        .checks
        .iter()
        .find(|check| check.check_id == CHECK_UNUSED_INDEX)
}

/// 依据快照历史计算某个索引的连续零使用窗口。
pub fn evaluate_unused_index_window(
    key: &str,
    current_uptime_hours: u64,
    current_at: i64,
    history: &[InspectionSnapshot],
    thresholds: &InspectionThresholds,
) -> UnusedIndexWindow {
    let required = thresholds.unused_index_min_snapshots.max(1);
    let min_uptime = thresholds.unused_index_min_uptime_hours;

    // 上一次执行过该检查的快照，用于识别实例重启。
    let previous_check = history
        .iter()
        .rev()
        .find_map(|snapshot| {
            unused_check(snapshot)
                .filter(|check| check.status != CheckStatus::Skipped)
                .map(|check| (snapshot, check))
        });
    let previous_uptime_hours = previous_check.and_then(|(_, check)| {
        check
            .evidence
            .iter()
            .find_map(|item| item.fields.get("uptime_hours").and_then(|value| value.as_u64()))
    });

    let mut reset_reason = None;
    if let Some(previous) = previous_uptime_hours {
        if current_uptime_hours < previous {
            reset_reason = Some("instance_restart");
        }
    }
    if reset_reason.is_none() && current_uptime_hours < min_uptime {
        reset_reason = Some("uptime_below_threshold");
    }

    if let Some(reason) = reset_reason {
        return UnusedIndexWindow {
            consecutive_zero_snapshots: 0,
            required_snapshots: required,
            window_start_at: None,
            window_uptimes: vec![WindowUptimePoint {
                at: current_at,
                uptime_hours: current_uptime_hours,
            }],
            window_reset: true,
            window_reset_reason: Some(reason.to_string()),
            previous_uptime_hours,
            window_span_hours: 0.0,
        };
    }

    let mut count = 1u64;
    let mut window_start_at = current_at;
    let mut uptimes = vec![WindowUptimePoint {
        at: current_at,
        uptime_hours: current_uptime_hours,
    }];
    let mut newer_uptime = current_uptime_hours;

    for snapshot in history.iter().rev() {
        let Some(check) = unused_check(snapshot) else {
            break;
        };
        if check.status == CheckStatus::Skipped {
            break;
        }
        let Some(item) = check.evidence.iter().find(|item| item.key == key) else {
            break;
        };
        // 上一次快照本身已判定窗口失效：该次不计入，计数从当前快照重新开始。
        if item
            .fields
            .get("window_reset")
            .and_then(|value| value.as_bool())
            == Some(true)
        {
            break;
        }
        let zero = item
            .fields
            .get("count_star")
            .and_then(|value| value.as_u64())
            .unwrap_or(0)
            == 0;
        if !zero {
            break;
        }
        let Some(older_uptime) = item
            .fields
            .get("uptime_hours")
            .and_then(|value| value.as_u64())
        else {
            break;
        };
        if older_uptime > newer_uptime {
            // 两个历史快照之间实例重启过，不能跨重启累计。
            break;
        }
        if older_uptime < min_uptime {
            // 历史快照本身处在无效窗口，不能计入。
            break;
        }
        count += 1;
        window_start_at = snapshot.finished_at;
        uptimes.push(WindowUptimePoint {
            at: snapshot.finished_at,
            uptime_hours: older_uptime,
        });
        newer_uptime = older_uptime;
    }
    uptimes.reverse();
    let window_span_hours = if count > 1 {
        (current_at - window_start_at).max(0) as f64 / 3600.0
    } else {
        0.0
    };

    UnusedIndexWindow {
        consecutive_zero_snapshots: count,
        required_snapshots: required,
        window_start_at: Some(window_start_at),
        window_uptimes: uptimes,
        window_reset: false,
        window_reset_reason: None,
        previous_uptime_hours,
        window_span_hours,
    }
}

pub fn detect_unused_indexes(
    rows: &[IndexUsageRow],
    fks: &[ForeignKeyDef],
    index_records: &[IndexRecord],
    uptime_hours: u64,
    thresholds: &InspectionThresholds,
    history: &[InspectionSnapshot],
    current_at: i64,
    active_alert_keys: &[String],
) -> (Vec<EvidenceItem>, usize) {
    let defs = build_index_defs(index_records);
    let mut evidence = Vec::new();

    for row in rows.iter().filter(|row| row.count_star == 0) {
        let key = format!("{}.{}", row.table, row.index);
        let window = evaluate_unused_index_window(
            &key,
            uptime_hours,
            current_at,
            history,
            thresholds,
        );
        let fk = defs
            .iter()
            .find(|index| {
                index.table.eq_ignore_ascii_case(&row.table)
                    && index.name.eq_ignore_ascii_case(&row.index)
            })
            .map(|index| supports_foreign_key(index, fks))
            .unwrap_or(false);

        let severity = if window.window_reset {
            Severity::Info
        } else if window.consecutive_zero_snapshots >= window.required_snapshots {
            Severity::Warn
        } else {
            Severity::Info
        };

        let message = if window.window_reset {
            let reason = match window.window_reset_reason.as_deref() {
                Some("instance_restart") => format!(
                    "实例 uptime 为 {} 小时，低于上一次快照记录的 {} 小时，判定实例已重启",
                    uptime_hours,
                    window
                        .previous_uptime_hours
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "未知".into())
                ),
                Some("uptime_below_threshold") => format!(
                    "实例仅运行 {} 小时，低于 {} 小时统计阈值",
                    uptime_hours, thresholds.unused_index_min_uptime_hours
                ),
                _ => "判定窗口无效".into(),
            };
            format!(
                "索引 {}.{} 判定窗口已重置：{}；本次 COUNT_STAR=0 仅记提示，不计入连续次数{}",
                row.table,
                row.index,
                reason,
                if fk {
                    "；该索引当前支撑外键约束，删除前需确认"
                } else {
                    ""
                }
            )
        } else if severity == Severity::Warn {
            format!(
                "索引 {}.{} 连续 {} 次巡检（覆盖 {:.1} 小时）COUNT_STAR 均为 0（要求 {} 次，实例运行 {} 小时）{}",
                row.table,
                row.index,
                window.consecutive_zero_snapshots,
                window.window_span_hours,
                window.required_snapshots,
                uptime_hours,
                if fk {
                    "；该索引当前支撑外键约束，删除前需确认"
                } else {
                    ""
                }
            )
        } else {
            format!(
                "索引 {}.{} 连续 {}/{} 次巡检 COUNT_STAR 为 0（实例运行 {} 小时），未达告警门槛{}",
                row.table,
                row.index,
                window.consecutive_zero_snapshots,
                window.required_snapshots,
                uptime_hours,
                if fk {
                    "；该索引当前支撑外键约束，删除前需确认"
                } else {
                    ""
                }
            )
        };

        evidence.push(EvidenceItem::new(
            key,
            severity,
            message,
            json!({
                "table": row.table,
                "index": row.index,
                "count_star": row.count_star,
                "count_read": row.count_read,
                "count_write": row.count_write,
                "uptime_hours": uptime_hours,
                "min_uptime_hours": thresholds.unused_index_min_uptime_hours,
                "consecutive_zero_snapshots": window.consecutive_zero_snapshots,
                "required_snapshots": window.required_snapshots,
                "window_start_at": window.window_start_at,
                "window_uptimes": window.window_uptimes.iter().map(|point| json!({
                    "at": point.at,
                    "uptime_hours": point.uptime_hours,
                })).collect::<Vec<_>>(),
                "window_reset": window.window_reset,
                "window_reset_reason": window.window_reset_reason,
                "previous_uptime_hours": window.previous_uptime_hours,
                "window_span_hours": window.window_span_hours,
                "used": false,
                "supports_foreign_key": fk,
            }),
        ));
    }

    // 已有告警的索引一旦真正被使用，必须产生一条 count_star>0 的证据，
    // 让快照存储明确区分“被使用”与“重启/降级”，只有前者才 resolve 告警。
    for row in rows.iter().filter(|row| row.count_star > 0) {
        let key = format!("{}.{}", row.table, row.index);
        if !active_alert_keys.iter().any(|active| active == &key) {
            continue;
        }
        let fk = defs
            .iter()
            .find(|index| {
                index.table.eq_ignore_ascii_case(&row.table)
                    && index.name.eq_ignore_ascii_case(&row.index)
            })
            .map(|index| supports_foreign_key(index, fks))
            .unwrap_or(false);
        evidence.push(EvidenceItem::new(
            key,
            Severity::Info,
            format!(
                "索引 {}.{} 本次监测到 COUNT_STAR>0（读 {} / 写 {}），连续零使用计数已清零，用于恢复已有告警{}",
                row.table,
                row.index,
                row.count_read,
                row.count_write,
                if fk {
                    "；该索引当前支撑外键约束"
                } else {
                    ""
                }
            ),
            json!({
                "table": row.table,
                "index": row.index,
                "count_star": row.count_star,
                "count_read": row.count_read,
                "count_write": row.count_write,
                "uptime_hours": uptime_hours,
                "min_uptime_hours": thresholds.unused_index_min_uptime_hours,
                "consecutive_zero_snapshots": 0,
                "required_snapshots": thresholds.unused_index_min_snapshots.max(1),
                "window_start_at": null,
                "window_uptimes": [],
                "window_reset": false,
                "window_reset_reason": null,
                "previous_uptime_hours": null,
                "window_span_hours": 0.0,
                "used": true,
                "supports_foreign_key": fk,
            }),
        ));
    }

    evidence.sort_by(|a, b| a.key.cmp(&b.key));
    (evidence, rows.len())
}

// ---------------------------------------------------------------------------
// 纯判定：表
// ---------------------------------------------------------------------------

pub fn detect_missing_primary_keys(rows: &[TablePrimaryKeyRow]) -> (Vec<EvidenceItem>, usize) {
    let mut evidence = Vec::new();
    for row in rows.iter().filter(|row| row.primary_index_count == 0) {
        evidence.push(EvidenceItem::new(
            format!("table:{}", row.table),
            Severity::Warn,
            format!(
                "表 {}（引擎 {}，估算 {} 行）没有主键",
                row.table, row.engine, row.table_rows
            ),
            json!({
                "table": row.table,
                "engine": row.engine,
                "table_rows": row.table_rows,
                "table_rows_is_estimate": true,
            }),
        ));
    }
    evidence.sort_by(|a, b| a.key.cmp(&b.key));
    (evidence, rows.len())
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

pub fn detect_table_capacity(
    rows: &[TableCapacityRow],
    thresholds: &InspectionThresholds,
) -> (Vec<EvidenceItem>, usize) {
    let mut evidence = Vec::new();
    for row in rows {
        let total = row.data_length.saturating_add(row.index_length);
        if total >= thresholds.large_table_bytes {
            evidence.push(EvidenceItem::new(
                format!("large:{}", row.table),
                Severity::Warn,
                format!(
                    "表 {} 体积 {}（数据 {} + 索引 {}），达到超大表阈值 {}",
                    row.table,
                    human_bytes(total),
                    human_bytes(row.data_length),
                    human_bytes(row.index_length),
                    human_bytes(thresholds.large_table_bytes)
                ),
                json!({
                    "table": row.table,
                    "engine": row.engine,
                    "table_rows": row.table_rows,
                    "table_rows_is_estimate": true,
                    "data_length": row.data_length,
                    "index_length": row.index_length,
                    "total_bytes": total,
                    "threshold_bytes": thresholds.large_table_bytes,
                }),
            ));
        }
        if total > 0 && row.data_free >= thresholds.fragmentation_min_bytes {
            let ratio = row.data_free as f64 / total as f64 * 100.0;
            if ratio >= thresholds.fragmentation_ratio_percent as f64 {
                evidence.push(EvidenceItem::new(
                    format!("frag:{}", row.table),
                    Severity::Warn,
                    format!(
                        "表 {} 存在碎片：DATA_FREE {}，占总大小 {:.1}%（阈值 {}% / {}）",
                        row.table,
                        human_bytes(row.data_free),
                        ratio,
                        thresholds.fragmentation_ratio_percent,
                        human_bytes(thresholds.fragmentation_min_bytes)
                    ),
                    json!({
                        "table": row.table,
                        "engine": row.engine,
                        "data_free": row.data_free,
                        "total_bytes": total,
                        "data_free_ratio_percent": (ratio * 10.0).round() / 10.0,
                        "min_bytes": thresholds.fragmentation_min_bytes,
                        "min_ratio_percent": thresholds.fragmentation_ratio_percent,
                    }),
                ));
            }
        }
    }
    evidence.sort_by(|a, b| a.key.cmp(&b.key));
    (evidence, rows.len())
}

pub fn detect_charset_mismatches(
    default_collation: &str,
    tables: &[TableCollationRow],
    columns: &[ColumnCollationRow],
) -> (Vec<EvidenceItem>, usize) {
    let mut evidence = Vec::new();
    let table_collations: HashMap<&str, &str> = tables
        .iter()
        .map(|row| (row.table.as_str(), row.collation.as_str()))
        .collect();
    for row in tables {
        if !row.collation.eq_ignore_ascii_case(default_collation) {
            evidence.push(EvidenceItem::new(
                format!("table:{}", row.table),
                Severity::Warn,
                format!(
                    "表 {} 的 collation 为 {}，与库默认 {} 不一致",
                    row.table, row.collation, default_collation
                ),
                json!({
                    "level": "table",
                    "table": row.table,
                    "actual": row.collation,
                    "expected": default_collation,
                }),
            ));
        }
    }
    for row in columns {
        let Some(expected) = table_collations.get(row.table.as_str()) else {
            continue;
        };
        if !row.collation.eq_ignore_ascii_case(expected) {
            evidence.push(EvidenceItem::new(
                format!("column:{}.{}", row.table, row.column),
                Severity::Warn,
                format!(
                    "列 {}.{} 的 collation 为 {}，与表 collation {} 不一致",
                    row.table, row.column, row.collation, expected
                ),
                json!({
                    "level": "column",
                    "table": row.table,
                    "column": row.column,
                    "charset": row.charset,
                    "actual": row.collation,
                    "expected": *expected,
                }),
            ));
        }
    }
    evidence.sort_by(|a, b| a.key.cmp(&b.key));
    (evidence, tables.len() + columns.len())
}

// ---------------------------------------------------------------------------
// 纯判定：外键
// ---------------------------------------------------------------------------

pub fn orphan_count_sql(fk: &ForeignKeyDef, cap: u64) -> Result<String, String> {
    if fk.child_columns.len() != fk.parent_columns.len() || fk.child_columns.is_empty() {
        return Err("外键列数量为空或不匹配".into());
    }
    let join = fk
        .child_columns
        .iter()
        .zip(fk.parent_columns.iter())
        .map(|(child, parent)| {
            format!(
                "c.{} = p.{}",
                quote_ident_mysql(child),
                quote_ident_mysql(parent)
            )
        })
        .collect::<Vec<_>>()
        .join(" AND ");
    let not_null = fk
        .child_columns
        .iter()
        .map(|column| format!("c.{} IS NOT NULL", quote_ident_mysql(column)))
        .collect::<Vec<_>>()
        .join(" AND ");
    let no_match = fk
        .parent_columns
        .iter()
        .map(|column| format!("p.{} IS NULL", quote_ident_mysql(column)))
        .collect::<Vec<_>>()
        .join(" AND ");
    let capped = cap.max(1);
    Ok(format!(
        "SELECT COUNT(*) AS orphan_count FROM (SELECT 1 FROM {} c LEFT JOIN {} p ON {} WHERE {} AND {} LIMIT {}) AS orphan_sample",
        quote_ident_mysql(&fk.child_table),
        quote_ident_mysql(&fk.parent_table),
        join,
        not_null,
        no_match,
        capped
    ))
}

pub fn parse_orphan_count(rows: &[RowMap]) -> u64 {
    rows.first()
        .and_then(|row| uint_value(row, "orphan_count"))
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// 纯判定：账号
// ---------------------------------------------------------------------------

fn is_builtin_local_root(account: &AccountRow) -> bool {
    account.user.eq_ignore_ascii_case("root") && account.host.eq_ignore_ascii_case("localhost")
}

fn is_socket_plugin(plugin: &str) -> bool {
    let lowered = plugin.to_ascii_lowercase();
    lowered.contains("socket")
}

fn is_system_account(account: &AccountRow) -> bool {
    matches!(
        account.user.as_str(),
        "mysql.sys" | "mysql.session" | "mysql.infoschema"
    )
}

pub fn detect_over_privileged_accounts(accounts: &[AccountRow]) -> (Vec<EvidenceItem>, usize) {
    let mut evidence = Vec::new();
    for account in accounts {
        if is_builtin_local_root(account) || is_system_account(account) {
            continue;
        }
        let mut privileges = Vec::new();
        if account.super_priv {
            privileges.push("SUPER");
        }
        if account.grant_priv {
            privileges.push("GRANT OPTION");
        }
        if account.file_priv {
            privileges.push("FILE");
        }
        if account.process_priv {
            privileges.push("PROCESS");
        }
        let wildcard = account.host == "%";
        if privileges.is_empty() && !wildcard {
            continue;
        }
        let critical = account.super_priv || account.grant_priv;
        let message = if privileges.is_empty() {
            format!(
                "账号 {}@{} 允许任意主机登录（Host=%），建议收敛到具体来源",
                account.user, account.host
            )
        } else {
            format!(
                "账号 {}@{} 持有高权限：{}{}",
                account.user,
                account.host,
                privileges.join(", "),
                if wildcard { "；且允许任意主机登录" } else { "" }
            )
        };
        evidence.push(EvidenceItem::new(
            format!("{}@{}", account.user, account.host),
            if critical {
                Severity::Critical
            } else {
                Severity::Warn
            },
            message,
            json!({
                "user": account.user,
                "host": account.host,
                "privileges": privileges,
                "wildcard_host": wildcard,
                "super_priv": account.super_priv,
                "grant_priv": account.grant_priv,
                "file_priv": account.file_priv,
                "process_priv": account.process_priv,
            }),
        ));
    }
    evidence.sort_by(|a, b| a.key.cmp(&b.key));
    (evidence, accounts.len())
}

pub fn detect_passwordless_accounts(accounts: &[AccountRow]) -> (Vec<EvidenceItem>, usize) {
    let mut evidence = Vec::new();
    for account in accounts {
        if is_system_account(account)
            || is_socket_plugin(&account.plugin)
            || !account.password.trim().is_empty()
        {
            continue;
        }
        evidence.push(EvidenceItem::new(
            format!("{}@{}", account.user, account.host),
            if account.locked {
                Severity::Warn
            } else {
                Severity::Critical
            },
            format!(
                "账号 {}@{} 未设置密码（plugin={}，account_locked={}）",
                account.user,
                account.host,
                if account.plugin.is_empty() {
                    "unknown"
                } else {
                    &account.plugin
                },
                account.locked
            ),
            json!({
                "user": account.user,
                "host": account.host,
                "plugin": account.plugin,
                "account_locked": account.locked,
                "password_source": "authentication_string/Password",
            }),
        ));
    }
    evidence.sort_by(|a, b| a.key.cmp(&b.key));
    (evidence, accounts.len())
}

pub fn detect_inactive_accounts(
    accounts: &[AccountRow],
    usage: &[AccountUsageRow],
    uptime_days: u64,
    thresholds: &InspectionThresholds,
) -> Result<(Vec<EvidenceItem>, usize), String> {
    if uptime_days < thresholds.inactive_account_min_uptime_days {
        return Err(format!(
            "实例连续运行 {} 天，低于 {} 天阈值，无法判定长期未登录",
            uptime_days, thresholds.inactive_account_min_uptime_days
        ));
    }
    let usage_map: HashMap<(&str, &str), &AccountUsageRow> = usage
        .iter()
        .map(|row| ((row.user.as_str(), row.host.as_str()), row))
        .collect();
    let mut evidence = Vec::new();
    let mut checked = 0;
    for account in accounts {
        if is_system_account(account) || account.user.is_empty() || account.locked {
            continue;
        }
        checked += 1;
        let stats = usage_map.get(&(account.user.as_str(), account.host.as_str()));
        let total = stats.map(|row| row.total_connections).unwrap_or(0);
        let current = stats.map(|row| row.current_connections).unwrap_or(0);
        if total == 0 && current == 0 {
            evidence.push(EvidenceItem::new(
                format!("{}@{}", account.user, account.host),
                Severity::Warn,
                format!(
                    "账号 {}@{} 在实例连续运行 {} 天内累计连接数为 0",
                    account.user, account.host, uptime_days
                ),
                json!({
                    "user": account.user,
                    "host": account.host,
                    "total_connections": total,
                    "current_connections": current,
                    "uptime_days": uptime_days,
                    "min_uptime_days": thresholds.inactive_account_min_uptime_days,
                }),
            ));
        }
    }
    evidence.sort_by(|a, b| a.key.cmp(&b.key));
    Ok((evidence, checked))
}

// ---------------------------------------------------------------------------
// 采集
// ---------------------------------------------------------------------------

fn schema_literal(ctx: &OpsContext) -> String {
    format!("'{}'", escape_sql_string(&ctx.db_name))
}

fn skipped(
    descriptor: &CheckDescriptor,
    reason: impl Into<String>,
    started: Instant,
) -> CheckResult {
    let mut result = CheckResult::skipped(descriptor.id, descriptor.title, descriptor.rule, reason);
    result.duration_ms = started.elapsed().as_millis() as u64;
    result
}

fn finish(
    descriptor: &CheckDescriptor,
    evidence: Vec<EvidenceItem>,
    checked_objects: usize,
    started: Instant,
) -> CheckResult {
    let status = status_from_evidence(&evidence);
    let summary = if evidence.is_empty() {
        format!("检查 {} 个对象，未发现问题", checked_objects)
    } else {
        format!(
            "检查 {} 个对象，发现 {} 项问题",
            checked_objects,
            evidence.len()
        )
    };
    CheckResult {
        check_id: descriptor.id.into(),
        title: descriptor.title.into(),
        status,
        rule: descriptor.rule.into(),
        summary,
        evidence,
        checked_objects,
        duration_ms: started.elapsed().as_millis() as u64,
        skipped_reason: None,
    }
}

async fn load_index_records(ctx: &OpsContext) -> Result<Vec<IndexRecord>, String> {
    let schema = schema_literal(ctx);
    let sql = format!(
        "SELECT TABLE_NAME, INDEX_NAME, NON_UNIQUE, SEQ_IN_INDEX, COLUMN_NAME, SUB_PART, INDEX_TYPE \
         FROM information_schema.STATISTICS \
         WHERE TABLE_SCHEMA = {} \
         ORDER BY TABLE_NAME, INDEX_NAME, SEQ_IN_INDEX",
        schema
    );
    let rows = fetch_read_only(ctx, &sql)
        .await
        .map_err(|err| format!("读取 information_schema.STATISTICS 失败: {err}"))?;
    Ok(parse_index_records(&rows))
}

async fn load_foreign_keys(ctx: &OpsContext) -> Result<Vec<ForeignKeyDef>, String> {
    let schema = schema_literal(ctx);
    let sql = format!(
        "SELECT TABLE_NAME, CONSTRAINT_NAME, COLUMN_NAME, ORDINAL_POSITION, \
         REFERENCED_TABLE_NAME, REFERENCED_COLUMN_NAME \
         FROM information_schema.KEY_COLUMN_USAGE \
         WHERE TABLE_SCHEMA = {} AND REFERENCED_TABLE_NAME IS NOT NULL \
         ORDER BY TABLE_NAME, CONSTRAINT_NAME, ORDINAL_POSITION",
        schema
    );
    let rows = fetch_read_only(ctx, &sql)
        .await
        .map_err(|err| format!("读取 information_schema.KEY_COLUMN_USAGE 失败: {err}"))?;
    Ok(group_foreign_keys(&rows))
}

async fn load_uptime_hours(ctx: &OpsContext) -> Result<u64, String> {
    let rows = fetch_read_only(
        ctx,
        "SELECT VARIABLE_VALUE FROM performance_schema.global_status WHERE VARIABLE_NAME = 'Uptime'",
    )
    .await
    .map_err(|err| format!("读取 performance_schema.global_status 失败: {err}"))?;
    let seconds = rows
        .first()
        .and_then(|row| uint_value(row, "variable_value"))
        .unwrap_or(0);
    Ok(seconds / 3600)
}

async fn check_redundant_indexes(
    ctx: &OpsContext,
    thresholds: &InspectionThresholds,
    descriptor: &CheckDescriptor,
    started: Instant,
) -> CheckResult {
    let _ = thresholds;
    let records = match load_index_records(ctx).await {
        Ok(records) => records,
        Err(err) => return skipped(descriptor, err, started),
    };
    let fks = match load_foreign_keys(ctx).await {
        Ok(fks) => fks,
        Err(err) => return skipped(descriptor, err, started),
    };
    let (evidence, checked) = detect_redundant_indexes(&records, &fks);
    finish(descriptor, evidence, checked, started)
}

async fn check_unused_indexes(
    ctx: &OpsContext,
    thresholds: &InspectionThresholds,
    descriptor: &CheckDescriptor,
    started: Instant,
    history: &[InspectionSnapshot],
    current_at: i64,
    active_alert_keys: &[String],
) -> CheckResult {
    let schema = schema_literal(ctx);
    let sql = format!(
        "SELECT OBJECT_SCHEMA, OBJECT_NAME, INDEX_NAME, COUNT_STAR, COUNT_READ, COUNT_WRITE \
         FROM performance_schema.table_io_waits_summary_by_index_usage \
         WHERE OBJECT_SCHEMA = {} AND INDEX_NAME IS NOT NULL AND INDEX_NAME <> 'PRIMARY' \
         ORDER BY OBJECT_NAME, INDEX_NAME",
        schema
    );
    let rows = match fetch_read_only(ctx, &sql).await {
        Ok(rows) => rows,
        Err(err) => {
            return skipped(
                descriptor,
                format!("performance_schema.table_io_waits_summary_by_index_usage 不可用: {err}"),
                started,
            )
        }
    };
    let uptime_hours = match load_uptime_hours(ctx).await {
        Ok(hours) => hours,
        Err(err) => return skipped(descriptor, err, started),
    };
    let fks = match load_foreign_keys(ctx).await {
        Ok(fks) => fks,
        Err(err) => return skipped(descriptor, err, started),
    };
    let index_records = match load_index_records(ctx).await {
        Ok(records) => records,
        Err(err) => return skipped(descriptor, err, started),
    };
    let usage = rows
        .iter()
        .filter_map(|row| {
            Some(IndexUsageRow {
                table: str_value(row, "object_name")?,
                index: str_value(row, "index_name")?,
                count_star: uint_value(row, "count_star").unwrap_or(0),
                count_read: uint_value(row, "count_read").unwrap_or(0),
                count_write: uint_value(row, "count_write").unwrap_or(0),
            })
        })
        .collect::<Vec<_>>();
    let (evidence, checked) = detect_unused_indexes(
        &usage,
        &fks,
        &index_records,
        uptime_hours,
        thresholds,
        history,
        current_at,
        active_alert_keys,
    );
    finish(descriptor, evidence, checked, started)
}

async fn check_missing_primary_keys(
    ctx: &OpsContext,
    _thresholds: &InspectionThresholds,
    descriptor: &CheckDescriptor,
    started: Instant,
) -> CheckResult {
    let schema = schema_literal(ctx);
    let sql = format!(
        "SELECT t.TABLE_NAME, t.ENGINE, t.TABLE_ROWS, \
         (SELECT COUNT(*) FROM information_schema.STATISTICS s \
          WHERE s.TABLE_SCHEMA = t.TABLE_SCHEMA AND s.TABLE_NAME = t.TABLE_NAME AND s.INDEX_NAME = 'PRIMARY') AS primary_index_count \
         FROM information_schema.TABLES t \
         WHERE t.TABLE_SCHEMA = {} AND t.TABLE_TYPE = 'BASE TABLE' AND t.ENGINE IS NOT NULL \
         ORDER BY t.TABLE_NAME",
        schema
    );
    let rows = match fetch_read_only(ctx, &sql).await {
        Ok(rows) => rows,
        Err(err) => return skipped(descriptor, format!("读取 information_schema.TABLES 失败: {err}"), started),
    };
    let parsed = rows
        .iter()
        .filter_map(|row| {
            Some(TablePrimaryKeyRow {
                table: str_value(row, "table_name")?,
                engine: str_value(row, "engine").unwrap_or_default(),
                table_rows: uint_value(row, "table_rows").unwrap_or(0),
                primary_index_count: uint_value(row, "primary_index_count").unwrap_or(0),
            })
        })
        .collect::<Vec<_>>();
    let (evidence, checked) = detect_missing_primary_keys(&parsed);
    finish(descriptor, evidence, checked, started)
}

async fn check_table_capacity(
    ctx: &OpsContext,
    thresholds: &InspectionThresholds,
    descriptor: &CheckDescriptor,
    started: Instant,
) -> CheckResult {
    let schema = schema_literal(ctx);
    let sql = format!(
        "SELECT TABLE_NAME, ENGINE, TABLE_ROWS, DATA_LENGTH, INDEX_LENGTH, DATA_FREE \
         FROM information_schema.TABLES \
         WHERE TABLE_SCHEMA = {} AND TABLE_TYPE = 'BASE TABLE' AND ENGINE IS NOT NULL \
         ORDER BY TABLE_NAME",
        schema
    );
    let rows = match fetch_read_only(ctx, &sql).await {
        Ok(rows) => rows,
        Err(err) => return skipped(descriptor, format!("读取表容量失败: {err}"), started),
    };
    let parsed = rows
        .iter()
        .filter_map(|row| {
            Some(TableCapacityRow {
                table: str_value(row, "table_name")?,
                engine: str_value(row, "engine").unwrap_or_default(),
                table_rows: uint_value(row, "table_rows").unwrap_or(0),
                data_length: uint_value(row, "data_length").unwrap_or(0),
                index_length: uint_value(row, "index_length").unwrap_or(0),
                data_free: uint_value(row, "data_free").unwrap_or(0),
            })
        })
        .collect::<Vec<_>>();
    let (evidence, checked) = detect_table_capacity(&parsed, thresholds);
    finish(descriptor, evidence, checked, started)
}

async fn check_charset_collation(
    ctx: &OpsContext,
    _thresholds: &InspectionThresholds,
    descriptor: &CheckDescriptor,
    started: Instant,
) -> CheckResult {
    let schema = schema_literal(ctx);
    let schema_sql = format!(
        "SELECT DEFAULT_CHARACTER_SET_NAME, DEFAULT_COLLATION_NAME FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = {}",
        schema
    );
    let schema_rows = match fetch_read_only(ctx, &schema_sql).await {
        Ok(rows) => rows,
        Err(err) => return skipped(descriptor, format!("读取库默认字符集失败: {err}"), started),
    };
    let Some(default_collation) = schema_rows
        .first()
        .and_then(|row| str_value(row, "default_collation_name"))
    else {
        return skipped(descriptor, "未找到当前库的默认 collation", started);
    };
    let table_sql = format!(
        "SELECT TABLE_NAME, TABLE_COLLATION FROM information_schema.TABLES \
         WHERE TABLE_SCHEMA = {} AND TABLE_TYPE = 'BASE TABLE' AND TABLE_COLLATION IS NOT NULL \
         ORDER BY TABLE_NAME",
        schema
    );
    let table_rows = match fetch_read_only(ctx, &table_sql).await {
        Ok(rows) => rows,
        Err(err) => return skipped(descriptor, format!("读取表 collation 失败: {err}"), started),
    };
    let tables = table_rows
        .iter()
        .filter_map(|row| {
            Some(TableCollationRow {
                table: str_value(row, "table_name")?,
                collation: str_value(row, "table_collation")?,
            })
        })
        .collect::<Vec<_>>();
    let column_sql = format!(
        "SELECT TABLE_NAME, COLUMN_NAME, CHARACTER_SET_NAME, COLLATION_NAME \
         FROM information_schema.COLUMNS \
         WHERE TABLE_SCHEMA = {} AND COLLATION_NAME IS NOT NULL \
         ORDER BY TABLE_NAME, ORDINAL_POSITION",
        schema
    );
    let column_rows = match fetch_read_only(ctx, &column_sql).await {
        Ok(rows) => rows,
        Err(err) => return skipped(descriptor, format!("读取列 collation 失败: {err}"), started),
    };
    let columns = column_rows
        .iter()
        .filter_map(|row| {
            Some(ColumnCollationRow {
                table: str_value(row, "table_name")?,
                column: str_value(row, "column_name")?,
                charset: str_value(row, "character_set_name"),
                collation: str_value(row, "collation_name")?,
            })
        })
        .collect::<Vec<_>>();
    let (evidence, checked) = detect_charset_mismatches(&default_collation, &tables, &columns);
    finish(descriptor, evidence, checked, started)
}

async fn check_orphan_foreign_keys(
    ctx: &OpsContext,
    thresholds: &InspectionThresholds,
    descriptor: &CheckDescriptor,
    started: Instant,
) -> CheckResult {
    let fks = match load_foreign_keys(ctx).await {
        Ok(fks) => fks,
        Err(err) => return skipped(descriptor, err, started),
    };
    let mut evidence = Vec::new();
    for fk in &fks {
        let sql = match orphan_count_sql(fk, thresholds.orphan_count_cap) {
            Ok(sql) => sql,
            Err(err) => {
                evidence.push(EvidenceItem::new(
                    format!("error:{}.{}", fk.child_table, fk.constraint_name),
                    Severity::Warn,
                    format!("外键 {} 检查未执行: {err}", fk.constraint_name),
                    json!({
                        "constraint": fk.constraint_name,
                        "child_table": fk.child_table,
                        "error": err,
                    }),
                ));
                continue;
            }
        };
        let rows = match fetch_read_only(ctx, &sql).await {
            Ok(rows) => rows,
            Err(err) => {
                evidence.push(EvidenceItem::new(
                    format!("error:{}.{}", fk.child_table, fk.constraint_name),
                    Severity::Warn,
                    format!("外键 {} 检查失败: {err}", fk.constraint_name),
                    json!({
                        "constraint": fk.constraint_name,
                        "child_table": fk.child_table,
                        "parent_table": fk.parent_table,
                        "error": err.to_string(),
                    }),
                ));
                continue;
            }
        };
        let orphan_count = parse_orphan_count(&rows);
        if orphan_count > 0 {
            let truncated = orphan_count >= thresholds.orphan_count_cap;
            evidence.push(EvidenceItem::new(
                format!("constraint:{}.{}", fk.child_table, fk.constraint_name),
                Severity::Critical,
                format!(
                    "外键 {}.{} 存在孤儿行：子表 {} → 父表 {}，孤儿数 {}{}",
                    fk.child_table,
                    fk.constraint_name,
                    fk.child_table,
                    fk.parent_table,
                    orphan_count,
                    if truncated { "（已封顶）" } else { "" }
                ),
                json!({
                    "child_table": fk.child_table,
                    "constraint": fk.constraint_name,
                    "child_columns": fk.child_columns,
                    "parent_table": fk.parent_table,
                    "parent_columns": fk.parent_columns,
                    "orphan_count": orphan_count,
                    "orphan_count_capped": truncated,
                    "cap": thresholds.orphan_count_cap,
                }),
            ));
        }
    }
    evidence.sort_by(|a, b| a.key.cmp(&b.key));
    finish(descriptor, evidence, fks.len(), started)
}

async fn load_accounts(ctx: &OpsContext) -> Result<Vec<AccountRow>, String> {
    let is_mariadb = matches!(
        ctx.db_client.db_type(),
        crate::config::DbType::MariaDB
    );
    let sql = if is_mariadb {
        "SELECT `User` AS user_name, `Host` AS host_name, `Super_priv` AS super_priv, \
         `Grant_priv` AS grant_priv, `File_priv` AS file_priv, `Process_priv` AS process_priv, \
         `plugin` AS plugin, `Password` AS password_value, 'N' AS account_locked \
         FROM `mysql`.`user` ORDER BY user_name, host_name"
    } else {
        "SELECT `User` AS user_name, `Host` AS host_name, `Super_priv` AS super_priv, \
         `Grant_priv` AS grant_priv, `File_priv` AS file_priv, `Process_priv` AS process_priv, \
         `plugin` AS plugin, `authentication_string` AS password_value, `account_locked` AS account_locked \
         FROM `mysql`.`user` ORDER BY user_name, host_name"
    };
    let rows = fetch_read_only(ctx, sql)
        .await
        .map_err(|err| format!("读取 mysql.user 失败（可能缺少 SELECT 权限）: {err}"))?;
    Ok(parse_account_rows(&rows))
}

async fn check_over_privileged_accounts(
    ctx: &OpsContext,
    _thresholds: &InspectionThresholds,
    descriptor: &CheckDescriptor,
    started: Instant,
) -> CheckResult {
    let accounts = match load_accounts(ctx).await {
        Ok(accounts) => accounts,
        Err(err) => return skipped(descriptor, err, started),
    };
    let (evidence, checked) = detect_over_privileged_accounts(&accounts);
    finish(descriptor, evidence, checked, started)
}

async fn check_passwordless_accounts(
    ctx: &OpsContext,
    _thresholds: &InspectionThresholds,
    descriptor: &CheckDescriptor,
    started: Instant,
) -> CheckResult {
    let accounts = match load_accounts(ctx).await {
        Ok(accounts) => accounts,
        Err(err) => return skipped(descriptor, err, started),
    };
    let (evidence, checked) = detect_passwordless_accounts(&accounts);
    finish(descriptor, evidence, checked, started)
}

async fn check_inactive_accounts(
    ctx: &OpsContext,
    thresholds: &InspectionThresholds,
    descriptor: &CheckDescriptor,
    started: Instant,
) -> CheckResult {
    let accounts = match load_accounts(ctx).await {
        Ok(accounts) => accounts,
        Err(err) => return skipped(descriptor, err, started),
    };
    let usage_sql = "SELECT USER, HOST, CURRENT_CONNECTIONS, TOTAL_CONNECTIONS \
         FROM performance_schema.accounts ORDER BY USER, HOST";
    let usage_rows = match fetch_read_only(ctx, usage_sql).await {
        Ok(rows) => rows,
        Err(err) => {
            return skipped(
                descriptor,
                format!("performance_schema.accounts 不可用: {err}"),
                started,
            )
        }
    };
    let usage = usage_rows
        .iter()
        .filter_map(|row| {
            Some(AccountUsageRow {
                user: str_value(row, "user")?,
                host: str_value(row, "host")?,
                current_connections: uint_value(row, "current_connections").unwrap_or(0),
                total_connections: uint_value(row, "total_connections").unwrap_or(0),
            })
        })
        .collect::<Vec<_>>();
    let uptime_seconds = match fetch_read_only(
        ctx,
        "SELECT VARIABLE_VALUE FROM performance_schema.global_status WHERE VARIABLE_NAME = 'Uptime'",
    )
    .await
    {
        Ok(rows) => rows
            .first()
            .and_then(|row| uint_value(row, "variable_value"))
            .unwrap_or(0),
        Err(err) => {
            return skipped(
                descriptor,
                format!("读取实例 uptime 失败: {err}"),
                started,
            )
        }
    };
    let uptime_days = uptime_seconds / 86_400;
    match detect_inactive_accounts(&accounts, &usage, uptime_days, thresholds) {
        Ok((evidence, checked)) => finish(descriptor, evidence, checked, started),
        Err(reason) => skipped(descriptor, reason, started),
    }
}

pub async fn collect_check(
    ctx: &OpsContext,
    check_id: &str,
    thresholds: &InspectionThresholds,
    history: &[InspectionSnapshot],
    current_at: i64,
    active_unused_alert_keys: &[String],
) -> CheckResult {
    let started = Instant::now();
    let Some(descriptor) = check_descriptor(check_id) else {
        return CheckResult::skipped(
            check_id,
            check_id,
            "未知检查项",
            format!("未知检查项: {check_id}"),
        );
    };
    match check_id {
        CHECK_REDUNDANT_INDEX => {
            check_redundant_indexes(ctx, thresholds, descriptor, started).await
        }
        CHECK_UNUSED_INDEX => {
            check_unused_indexes(
                ctx,
                thresholds,
                descriptor,
                started,
                history,
                current_at,
                active_unused_alert_keys,
            )
            .await
        }
        CHECK_MISSING_PRIMARY_KEY => {
            check_missing_primary_keys(ctx, thresholds, descriptor, started).await
        }
        CHECK_TABLE_CAPACITY => check_table_capacity(ctx, thresholds, descriptor, started).await,
        CHECK_CHARSET_COLLATION => {
            check_charset_collation(ctx, thresholds, descriptor, started).await
        }
        CHECK_ORPHAN_FOREIGN_KEY => {
            check_orphan_foreign_keys(ctx, thresholds, descriptor, started).await
        }
        CHECK_OVER_PRIVILEGED_ACCOUNT => {
            check_over_privileged_accounts(ctx, thresholds, descriptor, started).await
        }
        CHECK_PASSWORDLESS_ACCOUNT => {
            check_passwordless_accounts(ctx, thresholds, descriptor, started).await
        }
        CHECK_INACTIVE_ACCOUNT => {
            check_inactive_accounts(ctx, thresholds, descriptor, started).await
        }
        other => CheckResult::skipped(other, other, "未知检查项", format!("未知检查项: {other}")),
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn index_set(table: &str, name: &str, columns: &[&str], unique: bool) -> Vec<IndexRecord> {
        columns
            .iter()
            .enumerate()
            .map(|(seq, column)| IndexRecord {
                table: table.into(),
                name: name.into(),
                unique,
                seq: seq as u32,
                column: (*column).into(),
                sub_part: None,
                index_type: "BTREE".into(),
            })
            .collect()
    }

    #[test]
    fn redundant_detects_exact_duplicate() {
        let mut records = index_set("orders", "idx_id", &["id"], false);
        records.extend(index_set("orders", "idx_id_dup", &["id"], false));
        let (evidence, checked) = detect_redundant_indexes(&records, &[]);
        assert!(checked >= 2);
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].fields["redundant_index"], "idx_id_dup");
        assert_eq!(evidence[0].fields["reason"], "duplicate");
    }

    #[test]
    fn redundant_prefers_primary_over_unique_duplicate() {
        let mut records = index_set("t", "PRIMARY", &["id"], true);
        records.extend(index_set("t", "uk_id", &["id"], true));
        let (evidence, _) = detect_redundant_indexes(&records, &[]);
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].fields["redundant_index"], "uk_id");
        assert_eq!(evidence[0].fields["covering_index"], "PRIMARY");
    }

    #[test]
    fn redundant_detects_prefix_coverage_but_protects_unique() {
        let mut records = index_set("t", "idx_a", &["a"], false);
        records.extend(index_set("t", "idx_a_b", &["a", "b"], false));
        let (evidence, _) = detect_redundant_indexes(&records, &[]);
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].fields["redundant_index"], "idx_a");
        assert_eq!(evidence[0].fields["covering_index"], "idx_a_b");
        assert_eq!(evidence[0].fields["reason"], "prefix_covered");
    }

    #[test]
    fn redundant_does_not_flag_unique_prefix() {
        let mut records = index_set("t", "uk_a", &["a"], true);
        records.extend(index_set("t", "idx_a_b", &["a", "b"], false));
        let (evidence, _) = detect_redundant_indexes(&records, &[]);
        assert!(evidence.is_empty());
    }

    #[test]
    fn redundant_ignores_fulltext_and_reports_fk() {
        let mut records = index_set("t", "idx_a", &["a"], false);
        records.extend(index_set("t", "idx_a_b", &["a", "b"], false));
        let mut fulltext = index_set("t", "ft_a", &["a"], false);
        for record in &mut fulltext {
            record.index_type = "FULLTEXT".into();
        }
        records.extend(fulltext);
        let fks = vec![ForeignKeyDef {
            child_table: "t".into(),
            constraint_name: "fk_child".into(),
            child_columns: vec!["a".into()],
            parent_table: "p".into(),
            parent_columns: vec!["id".into()],
        }];
        let (evidence, _) = detect_redundant_indexes(&records, &fks);
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].fields["supports_foreign_key"], true);
    }

    fn unused_snapshot(
        id: &str,
        finished_at: i64,
        uptime_hours: u64,
        entries: &[(&str, u64)],
    ) -> InspectionSnapshot {
        let evidence: Vec<EvidenceItem> = entries
            .iter()
            .map(|(key, count_star)| {
                EvidenceItem::new(
                    *key,
                    Severity::Info,
                    format!("{key} count_star={count_star}"),
                    json!({
                        "count_star": count_star,
                        "uptime_hours": uptime_hours,
                    }),
                )
            })
            .collect();
        InspectionSnapshot {
            snapshot_id: id.into(),
            db_id: "db".into(),
            db_name: "app".into(),
            db_fingerprint: "fp".into(),
            started_at: finished_at - 1,
            finished_at,
            trigger: "manual".into(),
            status: crate::ops::inspection::types::SnapshotStatus::Ok,
            check_ids: vec![CHECK_UNUSED_INDEX.into()],
            checks: vec![CheckResult {
                check_id: CHECK_UNUSED_INDEX.into(),
                title: "未使用索引".into(),
                status: CheckStatus::Ok,
                rule: "r".into(),
                summary: "s".into(),
                evidence,
                checked_objects: 1,
                duration_ms: 1,
                skipped_reason: None,
            }],
        }
    }

    fn unused_reset_snapshot(
        id: &str,
        finished_at: i64,
        uptime_hours: u64,
        key: &str,
    ) -> InspectionSnapshot {
        let mut snapshot = unused_snapshot(id, finished_at, uptime_hours, &[(key, 0)]);
        for item in &mut snapshot.checks[0].evidence {
            item.fields["window_reset"] = json!(true);
            item.fields["window_reset_reason"] = json!("instance_restart");
        }
        snapshot
    }

    fn zero_row(table: &str, index: &str) -> IndexUsageRow {
        IndexUsageRow {
            table: table.into(),
            index: index.into(),
            count_star: 0,
            count_read: 0,
            count_write: 0,
        }
    }

    #[test]
    fn unused_index_first_snapshot_is_info() {
        let rows = vec![zero_row("t", "idx_a")];
        let thresholds = InspectionThresholds::default();
        let (evidence, checked) = detect_unused_indexes(
            &rows,
            &[],
            &[],
            48,
            &thresholds,
            &[],
            100,
            &[],
        );
        assert_eq!(checked, 1);
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].severity, Severity::Info);
        assert_eq!(evidence[0].fields["consecutive_zero_snapshots"], 1);
        assert_eq!(evidence[0].fields["required_snapshots"], 3);
        assert_eq!(evidence[0].fields["window_reset"], false);
    }

    #[test]
    fn unused_index_escalates_after_n_zero_snapshots() {
        let history = vec![
            unused_snapshot("s1", 100, 47, &[("t.idx_a", 0)]),
            unused_snapshot("s2", 200, 48, &[("t.idx_a", 0)]),
        ];
        let rows = vec![zero_row("t", "idx_a")];
        let thresholds = InspectionThresholds::default();
        let (evidence, _) = detect_unused_indexes(
            &rows,
            &[],
            &[],
            49,
            &thresholds,
            &history,
            300,
            &[],
        );
        assert_eq!(evidence[0].severity, Severity::Warn);
        assert_eq!(evidence[0].fields["consecutive_zero_snapshots"], 3);
        assert_eq!(evidence[0].fields["required_snapshots"], 3);
        assert_eq!(evidence[0].fields["window_start_at"], 100);
        assert_eq!(evidence[0].fields["window_uptimes"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn unused_index_non_zero_observation_resets_count() {
        let history = vec![
            unused_snapshot("s1", 100, 47, &[("t.idx_a", 0)]),
            unused_snapshot("s2", 200, 48, &[("t.idx_a", 10)]),
        ];
        let rows = vec![zero_row("t", "idx_a")];
        let thresholds = InspectionThresholds::default();
        let (evidence, _) = detect_unused_indexes(
            &rows,
            &[],
            &[],
            49,
            &thresholds,
            &history,
            300,
            &[],
        );
        assert_eq!(evidence[0].severity, Severity::Info);
        assert_eq!(evidence[0].fields["consecutive_zero_snapshots"], 1);
    }

    #[test]
    fn unused_index_instance_restart_resets_window() {
        let history = vec![unused_snapshot("s1", 100, 48, &[("t.idx_a", 0)])];
        let rows = vec![zero_row("t", "idx_a")];
        let thresholds = InspectionThresholds::default();
        let (evidence, _) = detect_unused_indexes(
            &rows,
            &[],
            &[],
            10,
            &thresholds,
            &history,
            200,
            &[],
        );
        assert_eq!(evidence[0].severity, Severity::Info);
        assert_eq!(evidence[0].fields["window_reset"], true);
        assert_eq!(evidence[0].fields["window_reset_reason"], "instance_restart");
        assert_eq!(evidence[0].fields["consecutive_zero_snapshots"], 0);
        assert_eq!(evidence[0].fields["previous_uptime_hours"], 48);
    }

    #[test]
    fn unused_index_uptime_below_threshold_resets_window() {
        let rows = vec![zero_row("t", "idx_a")];
        let thresholds = InspectionThresholds::default();
        let (evidence, _) = detect_unused_indexes(
            &rows,
            &[],
            &[],
            2,
            &thresholds,
            &[],
            100,
            &[],
        );
        assert_eq!(evidence[0].severity, Severity::Info);
        assert_eq!(evidence[0].fields["window_reset"], true);
        assert_eq!(
            evidence[0].fields["window_reset_reason"],
            "uptime_below_threshold"
        );
        assert_eq!(evidence[0].fields["consecutive_zero_snapshots"], 0);
    }

    #[test]
    fn unused_index_reset_snapshot_breaks_chain() {
        let history = vec![
            unused_snapshot("s1", 100, 48, &[("t.idx_a", 0)]),
            unused_reset_snapshot("s2", 200, 0, "t.idx_a"),
        ];
        let rows = vec![zero_row("t", "idx_a")];
        let thresholds = InspectionThresholds::default();
        let (evidence, _) = detect_unused_indexes(
            &rows,
            &[],
            &[],
            48,
            &thresholds,
            &history,
            300,
            &[],
        );
        assert_eq!(evidence[0].severity, Severity::Info);
        assert_eq!(evidence[0].fields["consecutive_zero_snapshots"], 1);
        assert_eq!(evidence[0].fields["window_reset"], false);
    }

    #[test]
    fn unused_index_used_index_with_active_alert_emits_evidence() {
        let rows = vec![IndexUsageRow {
            table: "t".into(),
            index: "idx_a".into(),
            count_star: 10,
            count_read: 10,
            count_write: 0,
        }];
        let (evidence, _) = detect_unused_indexes(
            &rows,
            &[],
            &[],
            48,
            &InspectionThresholds::default(),
            &[],
            100,
            &["t.idx_a".to_string()],
        );
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].severity, Severity::Info);
        assert_eq!(evidence[0].fields["used"], true);
        assert_eq!(evidence[0].fields["count_star"], 10);
    }

    #[test]
    fn unused_index_ignores_used_indexes_without_alert() {
        let rows = vec![IndexUsageRow {
            table: "t".into(),
            index: "idx_a".into(),
            count_star: 10,
            count_read: 10,
            count_write: 0,
        }];
        let (evidence, _) = detect_unused_indexes(
            &rows,
            &[],
            &[],
            48,
            &InspectionThresholds::default(),
            &[],
            100,
            &[],
        );
        assert!(evidence.is_empty());
    }

    #[test]
    fn missing_primary_key_detected() {
        let rows = vec![
            TablePrimaryKeyRow {
                table: "no_pk".into(),
                engine: "InnoDB".into(),
                table_rows: 10,
                primary_index_count: 0,
            },
            TablePrimaryKeyRow {
                table: "ok".into(),
                engine: "InnoDB".into(),
                table_rows: 10,
                primary_index_count: 1,
            },
        ];
        let (evidence, checked) = detect_missing_primary_keys(&rows);
        assert_eq!(checked, 2);
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].key, "table:no_pk");
    }

    #[test]
    fn table_capacity_large_and_fragmentation() {
        let thresholds = InspectionThresholds {
            large_table_bytes: 1000,
            fragmentation_min_bytes: 100,
            fragmentation_ratio_percent: 20,
            ..InspectionThresholds::default()
        };
        let rows = vec![
            TableCapacityRow {
                table: "big".into(),
                engine: "InnoDB".into(),
                table_rows: 100,
                data_length: 900,
                index_length: 200,
                data_free: 300,
            },
            TableCapacityRow {
                table: "small".into(),
                engine: "InnoDB".into(),
                table_rows: 10,
                data_length: 100,
                index_length: 50,
                data_free: 0,
            },
        ];
        let (evidence, checked) = detect_table_capacity(&rows, &thresholds);
        assert_eq!(checked, 2);
        assert_eq!(evidence.len(), 2);
        assert!(evidence.iter().any(|item| item.key == "large:big"));
        assert!(evidence.iter().any(|item| item.key == "frag:big"));
    }

    #[test]
    fn table_capacity_respects_fragmentation_floor() {
        let thresholds = InspectionThresholds {
            fragmentation_min_bytes: 100,
            fragmentation_ratio_percent: 20,
            ..InspectionThresholds::default()
        };
        let rows = vec![TableCapacityRow {
            table: "t".into(),
            engine: "InnoDB".into(),
            table_rows: 1,
            data_length: 1000,
            index_length: 0,
            data_free: 50,
        }];
        let (evidence, _) = detect_table_capacity(&rows, &thresholds);
        assert!(evidence.is_empty());
    }

    #[test]
    fn charset_mismatch_table_and_column() {
        let tables = vec![
            TableCollationRow {
                table: "a".into(),
                collation: "utf8mb4_0900_ai_ci".into(),
            },
            TableCollationRow {
                table: "b".into(),
                collation: "utf8mb4_unicode_ci".into(),
            },
        ];
        let columns = vec![
            ColumnCollationRow {
                table: "a".into(),
                column: "name".into(),
                charset: Some("utf8mb4".into()),
                collation: "utf8mb4_general_ci".into(),
            },
            ColumnCollationRow {
                table: "a".into(),
                column: "ok".into(),
                charset: Some("utf8mb4".into()),
                collation: "utf8mb4_0900_ai_ci".into(),
            },
        ];
        let (evidence, checked) = detect_charset_mismatches("utf8mb4_0900_ai_ci", &tables, &columns);
        assert_eq!(checked, 4);
        assert_eq!(evidence.len(), 2);
        assert!(evidence.iter().any(|item| item.key == "table:b"));
        assert!(evidence.iter().any(|item| item.key == "column:a.name"));
    }

    #[test]
    fn orphan_sql_contains_composite_conditions_and_cap() {
        let fk = ForeignKeyDef {
            child_table: "child".into(),
            constraint_name: "fk".into(),
            child_columns: vec!["a".into(), "b".into()],
            parent_table: "parent".into(),
            parent_columns: vec!["x".into(), "y".into()],
        };
        let sql = orphan_count_sql(&fk, 100).unwrap();
        assert!(sql.contains("LEFT JOIN `parent` p"));
        assert!(sql.contains("c.`a` = p.`x` AND c.`b` = p.`y`"));
        assert!(sql.contains("c.`a` IS NOT NULL AND c.`b` IS NOT NULL"));
        assert!(sql.contains("p.`x` IS NULL AND p.`y` IS NULL"));
        assert!(sql.contains("LIMIT 100"));
    }

    #[test]
    fn over_privileged_account_rules() {
        let accounts = vec![
            AccountRow {
                user: "root".into(),
                host: "localhost".into(),
                super_priv: true,
                grant_priv: true,
                file_priv: true,
                process_priv: true,
                plugin: "caching_sha2_password".into(),
                password: "secret".into(),
                locked: false,
            },
            AccountRow {
                user: "app".into(),
                host: "%".into(),
                super_priv: true,
                grant_priv: false,
                file_priv: false,
                process_priv: false,
                plugin: "caching_sha2_password".into(),
                password: "secret".into(),
                locked: false,
            },
            AccountRow {
                user: "readonly".into(),
                host: "%".into(),
                super_priv: false,
                grant_priv: false,
                file_priv: false,
                process_priv: false,
                plugin: "caching_sha2_password".into(),
                password: "secret".into(),
                locked: false,
            },
            AccountRow {
                user: "mysql.session".into(),
                host: "localhost".into(),
                super_priv: true,
                grant_priv: false,
                file_priv: false,
                process_priv: false,
                plugin: "caching_sha2_password".into(),
                password: "secret".into(),
                locked: false,
            },
        ];
        let (evidence, checked) = detect_over_privileged_accounts(&accounts);
        assert_eq!(checked, 4);
        assert_eq!(evidence.len(), 2);
        assert!(evidence.iter().any(|item| item.key == "app@%"));
        assert!(evidence.iter().any(|item| item.key == "readonly@%"));
        assert!(evidence.iter().all(|item| item.key != "root@localhost"));
        assert!(evidence
            .iter()
            .all(|item| item.key != "mysql.session@localhost"));
    }

    #[test]
    fn passwordless_rules_skip_socket_and_flag_locked() {
        let accounts = vec![
            AccountRow {
                user: "nopass".into(),
                host: "%".into(),
                super_priv: false,
                grant_priv: false,
                file_priv: false,
                process_priv: false,
                plugin: "caching_sha2_password".into(),
                password: String::new(),
                locked: false,
            },
            AccountRow {
                user: "locked".into(),
                host: "localhost".into(),
                super_priv: false,
                grant_priv: false,
                file_priv: false,
                process_priv: false,
                plugin: "caching_sha2_password".into(),
                password: String::new(),
                locked: true,
            },
            AccountRow {
                user: "socket".into(),
                host: "localhost".into(),
                super_priv: false,
                grant_priv: false,
                file_priv: false,
                process_priv: false,
                plugin: "auth_socket".into(),
                password: String::new(),
                locked: false,
            },
            AccountRow {
                user: "mysql.session".into(),
                host: "localhost".into(),
                super_priv: false,
                grant_priv: false,
                file_priv: false,
                process_priv: false,
                plugin: "caching_sha2_password".into(),
                password: String::new(),
                locked: false,
            },
        ];
        let (evidence, checked) = detect_passwordless_accounts(&accounts);
        assert_eq!(checked, 4);
        assert_eq!(evidence.len(), 2);
        assert_eq!(
            evidence
                .iter()
                .find(|item| item.key == "nopass@%")
                .unwrap()
                .severity,
            Severity::Critical
        );
        assert_eq!(
            evidence
                .iter()
                .find(|item| item.key == "locked@localhost")
                .unwrap()
                .severity,
            Severity::Warn
        );
    }

    #[test]
    fn inactive_accounts_require_uptime_threshold() {
        let accounts = vec![AccountRow {
            user: "idle".into(),
            host: "%".into(),
            super_priv: false,
            grant_priv: false,
            file_priv: false,
            process_priv: false,
            plugin: "caching_sha2_password".into(),
            password: "secret".into(),
            locked: false,
        }];
        let thresholds = InspectionThresholds::default();
        assert!(detect_inactive_accounts(&accounts, &[], 1, &thresholds).is_err());
        let (evidence, checked) = detect_inactive_accounts(&accounts, &[], 10, &thresholds).unwrap();
        assert_eq!(checked, 1);
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].fields["total_connections"], 0);
    }

    #[test]
    fn inactive_accounts_skip_system_locked_and_active() {
        let mut accounts = Vec::new();
        for (user, host, locked) in [
            ("mysql.sys", "localhost", false),
            ("idle", "localhost", true),
            ("active", "%", false),
        ] {
            accounts.push(AccountRow {
                user: user.into(),
                host: host.into(),
                super_priv: false,
                grant_priv: false,
                file_priv: false,
                process_priv: false,
                plugin: "caching_sha2_password".into(),
                password: "secret".into(),
                locked,
            });
        }
        let usage = vec![AccountUsageRow {
            user: "active".into(),
            host: "%".into(),
            current_connections: 1,
            total_connections: 100,
        }];
        let (evidence, checked) =
            detect_inactive_accounts(&accounts, &usage, 10, &InspectionThresholds::default())
                .unwrap();
        assert_eq!(checked, 1);
        assert!(evidence.is_empty());
    }

    #[test]
    fn parse_accounts_handles_empty_password_and_locked() {
        let mut row = RowMap::new();
        row.insert("user_name".into(), "u".into());
        row.insert("host_name".into(), "%".into());
        row.insert("super_priv".into(), "N".into());
        row.insert("grant_priv".into(), "N".into());
        row.insert("file_priv".into(), "Y".into());
        row.insert("process_priv".into(), "N".into());
        row.insert("plugin".into(), "caching_sha2_password".into());
        row.insert("password_value".into(), "".into());
        row.insert("account_locked".into(), "Y".into());
        let accounts = parse_account_rows(&[row]);
        assert_eq!(accounts.len(), 1);
        assert!(accounts[0].file_priv);
        assert!(accounts[0].password.is_empty());
        assert!(accounts[0].locked);
    }

    #[test]
    fn index_grouping_orders_composite_columns() {
        let mut records = index_set("t", "idx", &["a", "b", "c"], false);
        records.reverse();
        let defs = build_index_defs(&records);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].columns, vec!["a", "b", "c"]);
    }
}
