//! PostgreSQL 巡检采集与判定。
//!
//! 与 MySQL 路径的目标一致：返回同样结构的 `CheckResult`，但数据源全部换成
//! PG 目录/统计视图。原则：
//!
//! - 有精确对应的用精确来源（`pg_index`、`pg_stat_user_indexes`、`pg_constraint` 等）；
//! - PG 确实没有的概念（DATA_FREE 碎片、表级字符集/排序规则、账号锁定、
//!   MySQL 插件/PROCESS/FILE/GRANT OPTION 语义）不找近似指标顶替，
//!   在 `not_applicable` 里写明原因，整项不适用的返回 `skipped` + 原因；
//! - 依赖实例配置/权限的（`track_counts=off`、统计窗口重置、`pg_authid`/`pg_hba_file_rules`
//!   需超级用户）显式降级，不静默返回空结果，也不返回“0 个问题”的假阴性。
//!
//! 巡检范围：`current_schema()`（通常是 `public`），与 MySQL 巡检“单 schema”语义对应。

use std::collections::{BTreeMap, HashSet};
use std::time::Instant;

use serde_json::json;

use crate::ops::inspection::checks::{
    detect_charset_mismatches, detect_missing_primary_keys, detect_table_capacity,
    detect_unused_indexes, finish, parse_orphan_count, skipped, ColumnCollationRow,
    ForeignKeyDef, IndexRecord, IndexUsageRow, TableCapacityRow, TableCollationRow,
    TablePrimaryKeyRow,
};
use crate::ops::inspection::presets::CheckDescriptor;
use crate::ops::inspection::sql::fetch_read_only;
use crate::ops::inspection::types::{
    CheckResult, EvidenceItem, InspectionSnapshot, InspectionThresholds, CHECK_CHARSET_COLLATION,
    CHECK_INACTIVE_ACCOUNT, CHECK_MISSING_PRIMARY_KEY, CHECK_ORPHAN_FOREIGN_KEY,
    CHECK_OVER_PRIVILEGED_ACCOUNT, CHECK_PASSWORDLESS_ACCOUNT, CHECK_REDUNDANT_INDEX,
    CHECK_TABLE_CAPACITY, CHECK_UNUSED_INDEX,
};
use crate::ops::mysql_rows::{str_value, uint_value, RowMap};
use crate::ops::registry::OpsContext;
use crate::ops::types::Severity;
use crate::schema::index_expr::{normalize_index_expression, SqlDialect};
use crate::sql::util::quote_ident_pg;

pub(crate) async fn collect_check(
    ctx: &OpsContext,
    check_id: &str,
    thresholds: &InspectionThresholds,
    history: &[InspectionSnapshot],
    current_at: i64,
    active_unused_alert_keys: &[String],
    descriptor: &CheckDescriptor,
    started: Instant,
) -> CheckResult {
    match check_id {
        CHECK_REDUNDANT_INDEX => {
            check_redundant_indexes(ctx, descriptor, started).await
        }
        CHECK_UNUSED_INDEX => {
            check_unused_indexes(
                ctx,
                thresholds,
                history,
                current_at,
                active_unused_alert_keys,
                descriptor,
                started,
            )
            .await
        }
        CHECK_MISSING_PRIMARY_KEY => check_missing_primary_keys(ctx, descriptor, started).await,
        CHECK_TABLE_CAPACITY => {
            check_table_capacity(ctx, thresholds, descriptor, started).await
        }
        CHECK_CHARSET_COLLATION => check_charset_collation(ctx, descriptor, started).await,
        CHECK_ORPHAN_FOREIGN_KEY => {
            check_orphan_foreign_keys(ctx, thresholds, descriptor, started).await
        }
        CHECK_OVER_PRIVILEGED_ACCOUNT => {
            check_over_privileged_accounts(ctx, descriptor, started).await
        }
        CHECK_PASSWORDLESS_ACCOUNT => {
            check_passwordless_accounts(ctx, descriptor, started).await
        }
        CHECK_INACTIVE_ACCOUNT => skipped(
            descriptor,
            "PostgreSQL 没有 MySQL performance_schema.accounts 的等价历史登录统计；\
             不从 pg_stat_activity（仅当前会话）推断，避免把空结果误报为“0 个不活跃账号”",
            started,
        ),
        other => CheckResult::skipped(
            other,
            other,
            "未知检查项",
            format!("未知检查项: {other}"),
        ),
    }
}

fn row_truthy(row: &RowMap, key: &str) -> bool {
    matches!(
        str_value(row, key).as_deref(),
        Some("true") | Some("t") | Some("1")
    )
}

// ---------------------------------------------------------------------------
// 索引：冗余
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct PgIndexKey {
    /// 规范化后的键表达式（表达式列经共享 normalizer；普通列用 pg_get_indexdef 文本）。
    normalized: String,
    /// 展示用原始文本。
    display: String,
    /// 普通列的真实列名；表达式列为 None（外键支撑判断只认普通列）。
    column_name: Option<String>,
    sort: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PgIndexDef {
    table: String,
    name: String,
    unique: bool,
    primary: bool,
    method: String,
    predicate: Option<String>,
    keys: Vec<PgIndexKey>,
}

impl PgIndexDef {
    fn is_btree(&self) -> bool {
        self.method.eq_ignore_ascii_case("btree")
    }

    fn display_columns(&self) -> Vec<String> {
        self.keys.iter().map(|key| key.display.clone()).collect()
    }

    fn same_key_prefix(&self, other: &Self, prefix_len: usize) -> bool {
        prefix_len <= self.keys.len()
            && prefix_len <= other.keys.len()
            && self.keys[..prefix_len]
                .iter()
                .zip(other.keys[..prefix_len].iter())
                .all(|(left, right)| {
                    left.normalized == right.normalized
                        && left.sort.eq_ignore_ascii_case(&right.sort)
                })
    }

    fn same_keys(&self, other: &Self) -> bool {
        self.keys.len() == other.keys.len() && self.same_key_prefix(other, self.keys.len())
    }

    fn supports_foreign_key(&self, fks: &[ForeignKeyDef]) -> bool {
        fks.iter()
            .filter(|fk| fk.child_table.eq_ignore_ascii_case(&self.table))
            .any(|fk| {
                fk.child_columns.len() <= self.keys.len()
                    && fk
                        .child_columns
                        .iter()
                        .zip(self.keys.iter())
                        .all(|(column, key)| {
                            key.column_name
                                .as_deref()
                                .map(|name| name == column)
                                .unwrap_or(false)
                        })
            })
    }
}

async fn load_index_defs(ctx: &OpsContext) -> Result<Vec<PgIndexDef>, String> {
    let sql = "SELECT c.relname::text AS table_name, \
                      i.relname::text AS index_name, \
                      am.amname::text AS index_method, \
                      ix.indisunique::text AS is_unique, \
                      ix.indisprimary::text AS is_primary, \
                      pg_get_expr(ix.indpred, ix.indrelid)::text AS predicate, \
                      k.ord::text AS col_seq, \
                      pg_get_indexdef(ix.indexrelid, k.ord::int, true)::text AS column_def, \
                      COALESCE(a.attname, '')::text AS column_name, \
                      CASE WHEN ix.indkey[k.ord - 1] = 0 THEN 'true' ELSE 'false' END AS is_expression, \
                      CASE WHEN (ix.indoption[k.ord - 1] & 1) = 1 THEN 'DESC' ELSE 'ASC' END AS sort_order \
               FROM pg_class c \
               JOIN pg_namespace n ON n.oid = c.relnamespace \
               JOIN pg_index ix ON ix.indrelid = c.oid \
               JOIN pg_class i ON i.oid = ix.indexrelid \
               JOIN pg_am am ON am.oid = i.relam \
               JOIN generate_series(1, ix.indnkeyatts) AS k(ord) ON true \
               LEFT JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum = ix.indkey[k.ord - 1] \
               WHERE n.nspname = current_schema() AND c.relkind IN ('r', 'p') \
               ORDER BY c.relname, i.relname, k.ord";
    let rows = fetch_read_only(ctx, sql)
        .await
        .map_err(|err| format!("读取 pg_index 失败: {err}"))?;
    Ok(parse_index_defs(&rows))
}

fn parse_index_defs(rows: &[RowMap]) -> Vec<PgIndexDef> {
    let mut grouped: BTreeMap<(String, String), (PgIndexDef, Vec<(usize, PgIndexKey)>)> =
        BTreeMap::new();
    for row in rows {
        let (Some(table), Some(name), Some(seq)) = (
            str_value(row, "table_name"),
            str_value(row, "index_name"),
            uint_value(row, "col_seq"),
        ) else {
            continue;
        };
        let definition = str_value(row, "column_def").unwrap_or_default();
        let is_expression = row_truthy(row, "is_expression");
        let key = PgIndexKey {
            normalized: normalize_index_expression(SqlDialect::Postgres, &definition),
            display: definition.trim().to_string(),
            column_name: if is_expression {
                None
            } else {
                str_value(row, "column_name").filter(|value| !value.is_empty())
            },
            sort: str_value(row, "sort_order").unwrap_or_else(|| "ASC".into()),
        };
        let entry = grouped
            .entry((table.clone(), name.clone()))
            .or_insert_with(|| {
                (
                    PgIndexDef {
                        table,
                        name,
                        unique: false,
                        primary: false,
                        method: String::new(),
                        predicate: None,
                        keys: Vec::new(),
                    },
                    Vec::new(),
                )
            });
        entry.0.unique = row_truthy(row, "is_unique");
        entry.0.primary = row_truthy(row, "is_primary");
        entry.0.method = str_value(row, "index_method").unwrap_or_default();
        entry.0.predicate = str_value(row, "predicate").map(|predicate| {
            normalize_index_expression(SqlDialect::Postgres, &predicate)
        });
        entry.1.push((seq as usize, key));
    }
    grouped
        .into_values()
        .map(|(mut def, mut keys)| {
            keys.sort_by_key(|(seq, _)| *seq);
            def.keys = keys.into_iter().map(|(_, key)| key).collect();
            def
        })
        .collect()
}

fn same_predicate(a: &PgIndexDef, b: &PgIndexDef) -> bool {
    a.predicate == b.predicate
}

fn detect_pg_redundant_indexes(
    defs: &[PgIndexDef],
    fks: &[ForeignKeyDef],
) -> (Vec<EvidenceItem>, usize) {
    let checked = defs
        .iter()
        .filter(|def| def.is_btree() && !def.primary)
        .count();
    let mut evidence: Vec<EvidenceItem> = Vec::new();
    let mut redundant: HashSet<(String, String)> = HashSet::new();

    let tables: HashSet<&str> = defs
        .iter()
        .filter(|def| def.is_btree())
        .map(|def| def.table.as_str())
        .collect();

    for table in tables {
        let mut table_indexes: Vec<&PgIndexDef> = defs
            .iter()
            .filter(|def| def.table == table && def.is_btree())
            .collect();
        table_indexes.sort_by(|a, b| {
            b.primary.cmp(&a.primary)
                .then_with(|| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase()))
        });

        for redundant_index in table_indexes.iter().filter(|def| !def.primary && !def.unique) {
            let mut covering_hit: Option<(&PgIndexDef, &'static str)> = None;
            for covering in table_indexes
                .iter()
                .filter(|def| def.name != redundant_index.name)
            {
                if !same_predicate(redundant_index, covering) {
                    continue;
                }
                let reason = if redundant_index.same_keys(covering) {
                    if covering.unique {
                        "duplicate"
                    } else if covering.name.to_ascii_lowercase()
                        < redundant_index.name.to_ascii_lowercase()
                    {
                        // 两个非唯一索引完全相同：只报告排序靠后的一个，避免重复证据。
                        "duplicate"
                    } else {
                        continue;
                    }
                } else if redundant_index.same_key_prefix(covering, redundant_index.keys.len())
                    && redundant_index.keys.len() < covering.keys.len()
                {
                    "covered_by_prefix"
                } else {
                    continue;
                };
                covering_hit = Some((covering, reason));
                break;
            }
            let Some((covering, reason)) = covering_hit else {
                continue;
            };
            if !redundant.insert((
                redundant_index.table.clone(),
                redundant_index.name.clone(),
            )) {
                continue;
            }
            let supports_fk = redundant_index.supports_foreign_key(fks);
            let mut fields = json!({
                "table": redundant_index.table,
                "redundant_index": redundant_index.name,
                "redundant_columns": redundant_index.display_columns(),
                "redundant_unique": redundant_index.unique,
                "covering_index": covering.name,
                "covering_columns": covering.display_columns(),
                "reason": reason,
                "index_type": "BTREE",
                "supports_foreign_key": supports_fk,
                "engine": "postgresql",
            });
            if let Some(predicate) = &redundant_index.predicate {
                fields["predicate"] = json!(predicate);
            }
            if let Some(predicate) = &covering.predicate {
                fields["covering_predicate"] = json!(predicate);
            }
            let predicate_note = redundant_index
                .predicate
                .as_ref()
                .map(|predicate| format!("（部分索引 WHERE {predicate}）"))
                .unwrap_or_default();
            evidence.push(EvidenceItem::new(
                format!("{}.{}", redundant_index.table, redundant_index.name),
                Severity::Warn,
                format!(
                    "索引 {}.{}{} 可被 {}.{} 覆盖（列 {}），原因：{}{}",
                    redundant_index.table,
                    redundant_index.name,
                    predicate_note,
                    covering.table,
                    covering.name,
                    redundant_index.display_columns().join(", "),
                    if reason == "duplicate" { "列与谓词完全相同" } else { "键列是更长索引的前缀" },
                    if supports_fk {
                        "；该索引当前支撑外键约束，删除前需确认"
                    } else {
                        ""
                    }
                ),
                fields,
            ));
        }
    }

    evidence.sort_by(|a, b| a.key.cmp(&b.key));
    (evidence, checked)
}

async fn check_redundant_indexes(
    ctx: &OpsContext,
    descriptor: &CheckDescriptor,
    started: Instant,
) -> CheckResult {
    let defs = match load_index_defs(ctx).await {
        Ok(defs) => defs,
        Err(err) => return skipped(descriptor, err, started),
    };
    let fks = match load_foreign_keys(ctx).await {
        Ok(fks) => fks.into_iter().map(|item| item.def).collect::<Vec<_>>(),
        Err(err) => return skipped(descriptor, err, started),
    };
    let (evidence, checked) = detect_pg_redundant_indexes(&defs, &fks);
    finish(descriptor, evidence, checked, started)
}

fn index_records_from_defs(defs: &[PgIndexDef]) -> Vec<IndexRecord> {
    let mut records = Vec::new();
    for def in defs {
        for (position, key) in def.keys.iter().enumerate() {
            records.push(IndexRecord {
                table: def.table.clone(),
                name: def.name.clone(),
                unique: def.unique,
                seq: position as u32 + 1,
                column: key
                    .column_name
                    .clone()
                    .unwrap_or_else(|| key.display.clone()),
                sub_part: None,
                index_type: if def.method.is_empty() {
                    "BTREE".into()
                } else {
                    def.method.to_ascii_uppercase()
                },
            });
        }
    }
    records
}

// ---------------------------------------------------------------------------
// 索引：未使用
// ---------------------------------------------------------------------------

async fn check_unused_indexes(
    ctx: &OpsContext,
    thresholds: &InspectionThresholds,
    history: &[InspectionSnapshot],
    current_at: i64,
    active_unused_alert_keys: &[String],
    descriptor: &CheckDescriptor,
    started: Instant,
) -> CheckResult {
    let track_counts = match fetch_read_only(ctx, "SELECT current_setting('track_counts') AS track_counts")
        .await
    {
        Ok(rows) => rows
            .first()
            .and_then(|row| str_value(row, "track_counts"))
            .unwrap_or_else(|| "off".into()),
        Err(err) => return skipped(descriptor, format!("读取 track_counts 失败: {err}"), started),
    };
    if !track_counts.eq_ignore_ascii_case("on") {
        return skipped(
            descriptor,
            "实例 track_counts=off，pg_stat_user_indexes 不累计索引扫描次数；\
             无法判定未使用索引，未返回 0 条结果冒充“全部索引都在使用”",
            started,
        );
    }

    let stats_sql = "SELECT GREATEST(EXTRACT(EPOCH FROM (now() - pg_postmaster_start_time())), 0)::bigint::text AS uptime_seconds, \
                            COALESCE((SELECT GREATEST(EXTRACT(EPOCH FROM (now() - stats_reset)), 0)::bigint \
                                      FROM pg_stat_database WHERE datname = current_database() AND stats_reset IS NOT NULL), \
                                     -1::bigint)::text AS stats_age_seconds, \
                            COALESCE((SELECT EXTRACT(EPOCH FROM stats_reset)::bigint \
                                      FROM pg_stat_database WHERE datname = current_database() AND stats_reset IS NOT NULL), \
                                     -1::bigint)::text AS stats_reset_epoch";
    let stats = match fetch_read_only(ctx, stats_sql).await {
        Ok(rows) => rows,
        Err(err) => {
            return skipped(
                descriptor,
                format!("读取统计窗口失败: {err}"),
                started,
            )
        }
    };
    let uptime_seconds = stats
        .first()
        .and_then(|row| uint_value(row, "uptime_seconds"))
        .unwrap_or(0);
    let stats_age_seconds = stats
        .first()
        .and_then(|row| str_value(row, "stats_age_seconds"))
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|value| *value >= 0)
        .map(|value| value as u64);
    let stats_reset_epoch = stats
        .first()
        .and_then(|row| uint_value(row, "stats_reset_epoch"));

    let mut effective_seconds = uptime_seconds;
    let stats_reset_newer = stats_age_seconds
        .map(|age| age.saturating_add(60) < uptime_seconds)
        .unwrap_or(false);
    if let (true, Some(age)) = (stats_reset_newer, stats_age_seconds) {
        // 统计被重置过：旧快照的计数与新窗口不可比，按重置时间截断历史。
        effective_seconds = age;
    }
    let effective_hours = effective_seconds / 3600;
    if stats_reset_newer && effective_hours < thresholds.unused_index_min_uptime_hours {
        return skipped(
            descriptor,
            format!(
                "索引使用统计已被重置（pg_stat_database.stats_reset），有效窗口仅 {} 小时，\
                 低于 {} 小时阈值；本轮不做未使用索引判定，避免误报",
                effective_hours, thresholds.unused_index_min_uptime_hours
            ),
            started,
        );
    }
    let uptime_hours = effective_hours;
    let filtered_history = if stats_reset_newer {
        stats_reset_epoch.map(|epoch| {
            history
                .iter()
                .filter(|snapshot| snapshot.finished_at >= epoch as i64)
                .cloned()
                .collect::<Vec<_>>()
        })
    } else {
        None
    };
    let effective_history = filtered_history.as_deref().unwrap_or(history);

    let sql = "SELECT s.relname::text AS table_name, s.indexrelname::text AS index_name, \
                      s.idx_scan::text AS count_star, s.idx_tup_read::text AS count_read \
               FROM pg_stat_user_indexes s \
               JOIN pg_index ix ON ix.indexrelid = s.indexrelid \
               WHERE s.schemaname = current_schema() AND NOT ix.indisprimary \
               ORDER BY s.relname, s.indexrelname";
    let rows = match fetch_read_only(ctx, sql).await {
        Ok(rows) => rows,
        Err(err) => return skipped(descriptor, format!("读取 pg_stat_user_indexes 失败: {err}"), started),
    };
    let usage: Vec<IndexUsageRow> = rows
        .iter()
        .filter_map(|row| {
            Some(IndexUsageRow {
                table: str_value(row, "table_name")?,
                index: str_value(row, "index_name")?,
                count_star: uint_value(row, "count_star").unwrap_or(0),
                count_read: uint_value(row, "count_read").unwrap_or(0),
                // PG 没有单索引写入次数字段，保留 0 并在 not_applicable 中说明。
                count_write: 0,
            })
        })
        .collect();

    let defs = match load_index_defs(ctx).await {
        Ok(defs) => defs,
        Err(err) => return skipped(descriptor, err, started),
    };
    let fks = match load_foreign_keys(ctx).await {
        Ok(fks) => fks.into_iter().map(|item| item.def).collect::<Vec<_>>(),
        Err(err) => return skipped(descriptor, err, started),
    };
    let records = index_records_from_defs(&defs);
    let (evidence, checked) = detect_unused_indexes(
        &usage,
        &fks,
        &records,
        uptime_hours,
        thresholds,
        effective_history,
        current_at,
        active_unused_alert_keys,
    );
    let mut result = finish(descriptor, evidence, checked, started);
    result.not_applicable.push(
        "PostgreSQL 不提供单索引写入次数（无 MySQL count_write 对应列），该字段固定为 0；\
         未使用判定只依据 idx_scan"
            .into(),
    );
    result
}

// ---------------------------------------------------------------------------
// 表：主键 / 容量 / 字符集排序规则
// ---------------------------------------------------------------------------

async fn check_missing_primary_keys(
    ctx: &OpsContext,
    descriptor: &CheckDescriptor,
    started: Instant,
) -> CheckResult {
    let sql = "SELECT c.relname::text AS table_name, \
                      CASE c.relkind WHEN 'p' THEN 'partitioned' ELSE 'heap' END AS engine, \
                      GREATEST(c.reltuples, 0)::bigint::text AS table_rows, \
                      CASE WHEN EXISTS (SELECT 1 FROM pg_index ix \
                                        WHERE ix.indrelid = c.oid AND ix.indisprimary) \
                           THEN '1' ELSE '0' END AS primary_index_count \
               FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
               WHERE n.nspname = current_schema() AND c.relkind IN ('r', 'p') \
               ORDER BY c.relname";
    let rows = match fetch_read_only(ctx, sql).await {
        Ok(rows) => rows,
        Err(err) => return skipped(descriptor, format!("读取 pg_class 失败: {err}"), started),
    };
    let tables: Vec<TablePrimaryKeyRow> = rows
        .iter()
        .filter_map(|row| {
            Some(TablePrimaryKeyRow {
                table: str_value(row, "table_name")?,
                engine: str_value(row, "engine").unwrap_or_else(|| "heap".into()),
                table_rows: uint_value(row, "table_rows").unwrap_or(0),
                primary_index_count: uint_value(row, "primary_index_count").unwrap_or(0),
            })
        })
        .collect();
    let (evidence, checked) = detect_missing_primary_keys(&tables);
    finish(descriptor, evidence, checked, started)
}

async fn check_table_capacity(
    ctx: &OpsContext,
    thresholds: &InspectionThresholds,
    descriptor: &CheckDescriptor,
    started: Instant,
) -> CheckResult {
    let sql = "SELECT c.relname::text AS table_name, 'heap' AS engine, \
                      GREATEST(c.reltuples, 0)::bigint::text AS table_rows, \
                      pg_table_size(c.oid)::text AS data_length, \
                      pg_indexes_size(c.oid)::text AS index_length \
               FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
               WHERE n.nspname = current_schema() AND c.relkind = 'r' \
               ORDER BY c.relname";
    let rows = match fetch_read_only(ctx, sql).await {
        Ok(rows) => rows,
        Err(err) => return skipped(descriptor, format!("读取表体积失败: {err}"), started),
    };
    let tables: Vec<TableCapacityRow> = rows
        .iter()
        .filter_map(|row| {
            Some(TableCapacityRow {
                table: str_value(row, "table_name")?,
                engine: str_value(row, "engine").unwrap_or_else(|| "heap".into()),
                table_rows: uint_value(row, "table_rows").unwrap_or(0),
                data_length: uint_value(row, "data_length").unwrap_or(0),
                index_length: uint_value(row, "index_length").unwrap_or(0),
                data_free: 0,
            })
        })
        .collect();
    let (evidence, checked) = detect_table_capacity(&tables, thresholds);
    let mut result = finish(descriptor, evidence, checked, started);
    result.not_applicable.push(
        "PostgreSQL 没有 DATA_FREE 对应概念，未做碎片率判定（行数来自 pg_class.reltuples 估算）"
            .into(),
    );
    result
}

async fn check_charset_collation(
    ctx: &OpsContext,
    descriptor: &CheckDescriptor,
    started: Instant,
) -> CheckResult {
    let db_sql = "SELECT pg_encoding_to_char(d.encoding) AS encoding, d.datcollate AS default_collation \
                  FROM pg_database d WHERE d.datname = current_database()";
    let db_rows = match fetch_read_only(ctx, db_sql).await {
        Ok(rows) => rows,
        Err(err) => return skipped(descriptor, format!("读取 pg_database 失败: {err}"), started),
    };
    let Some(db_row) = db_rows.first() else {
        return skipped(descriptor, "无法读取当前数据库的编码/排序规则", started);
    };
    let encoding = str_value(db_row, "encoding").unwrap_or_else(|| "UTF8".into());
    let default_collation = str_value(db_row, "default_collation").unwrap_or_default();

    let sql = "SELECT c.relname::text AS table_name, a.attname::text AS column_name, \
                      co.collname::text AS collation \
               FROM pg_attribute a \
               JOIN pg_class c ON c.oid = a.attrelid \
               JOIN pg_namespace n ON n.oid = c.relnamespace \
               JOIN pg_collation co ON co.oid = a.attcollation \
               WHERE n.nspname = current_schema() AND c.relkind IN ('r', 'p') \
                 AND a.attnum > 0 AND NOT a.attisdropped AND a.attcollation <> 0 \
               ORDER BY c.relname, a.attnum";
    let rows = match fetch_read_only(ctx, sql).await {
        Ok(rows) => rows,
        Err(err) => return skipped(descriptor, format!("读取列排序规则失败: {err}"), started),
    };
    let mut columns = Vec::new();
    let mut tables: BTreeMap<String, TableCollationRow> = BTreeMap::new();
    for row in &rows {
        let (Some(table), Some(column)) = (
            str_value(row, "table_name"),
            str_value(row, "column_name"),
        ) else {
            continue;
        };
        let collation = str_value(row, "collation").unwrap_or_default();
        // collname='default' 表示继承数据库默认排序规则，按库默认值参与比较。
        let actual = if collation.eq_ignore_ascii_case("default") {
            default_collation.clone()
        } else {
            collation
        };
        tables.entry(table.clone()).or_insert_with(|| TableCollationRow {
            table: table.clone(),
            collation: default_collation.clone(),
        });
        columns.push(ColumnCollationRow {
            table,
            column,
            charset: Some(encoding.clone()),
            collation: actual,
        });
    }
    let tables: Vec<TableCollationRow> = tables.into_values().collect();
    let (evidence, _) = detect_charset_mismatches(&default_collation, &tables, &columns);
    let mut result = finish(descriptor, evidence, columns.len(), started);
    result.not_applicable.push(
        "PostgreSQL 没有表级字符集/排序规则，表级一致性检查未执行；列级按 pg_collation 对比库默认 collation"
            .into(),
    );
    result
}

// ---------------------------------------------------------------------------
// 外键
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct PgForeignKey {
    def: ForeignKeyDef,
    validated: bool,
}

async fn load_foreign_keys(ctx: &OpsContext) -> Result<Vec<PgForeignKey>, String> {
    let sql = "SELECT con.conname::text AS constraint_name, child.relname::text AS table_name, \
                      parent.relname::text AS referenced_table_name, \
                      ca.attname::text AS column_name, pa.attname::text AS referenced_column_name, \
                      k.ord::text AS ordinal_position, con.convalidated::text AS convalidated \
               FROM pg_constraint con \
               JOIN pg_class child ON child.oid = con.conrelid \
               JOIN pg_class parent ON parent.oid = con.confrelid \
               JOIN pg_namespace n ON n.oid = child.relnamespace \
               JOIN generate_subscripts(con.conkey, 1) AS k(ord) ON true \
               JOIN pg_attribute ca ON ca.attrelid = con.conrelid AND ca.attnum = con.conkey[k.ord] \
               JOIN pg_attribute pa ON pa.attrelid = con.confrelid AND pa.attnum = con.confkey[k.ord] \
               WHERE con.contype = 'f' AND n.nspname = current_schema() \
               ORDER BY child.relname, con.conname, k.ord";
    let rows = fetch_read_only(ctx, sql)
        .await
        .map_err(|err| format!("读取 pg_constraint 失败: {err}"))?;
    Ok(parse_foreign_keys(&rows))
}

fn parse_foreign_keys(rows: &[RowMap]) -> Vec<PgForeignKey> {
    struct Builder {
        child_table: String,
        constraint_name: String,
        parent_table: String,
        validated: bool,
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
            validated: true,
            entries: Vec::new(),
        });
        entry.validated = row_truthy(row, "convalidated");
        entry.entries.push((seq, child_column, parent_column));
    }
    builders
        .into_values()
        .map(|mut builder| {
            builder.entries.sort_by_key(|(seq, _, _)| *seq);
            PgForeignKey {
                def: ForeignKeyDef {
                    child_table: builder.child_table,
                    constraint_name: builder.constraint_name,
                    child_columns: builder.entries.iter().map(|(_, c, _)| c.clone()).collect(),
                    parent_table: builder.parent_table,
                    parent_columns: builder.entries.iter().map(|(_, _, p)| p.clone()).collect(),
                },
                validated: builder.validated,
            }
        })
        .collect()
}

fn pg_orphan_count_sql(fk: &ForeignKeyDef, cap: u64) -> Result<String, String> {
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
                quote_ident_pg(child),
                quote_ident_pg(parent)
            )
        })
        .collect::<Vec<_>>()
        .join(" AND ");
    let not_null = fk
        .child_columns
        .iter()
        .map(|column| format!("c.{} IS NOT NULL", quote_ident_pg(column)))
        .collect::<Vec<_>>()
        .join(" AND ");
    let no_match = fk
        .parent_columns
        .iter()
        .map(|column| format!("p.{} IS NULL", quote_ident_pg(column)))
        .collect::<Vec<_>>()
        .join(" AND ");
    Ok(format!(
        "SELECT COUNT(*) AS orphan_count FROM (SELECT 1 FROM {} c LEFT JOIN {} p ON {} WHERE {} AND {} LIMIT {}) AS orphan_sample",
        quote_ident_pg(&fk.child_table),
        quote_ident_pg(&fk.parent_table),
        join,
        not_null,
        no_match,
        cap.max(1)
    ))
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
    for item in &fks {
        let fk = &item.def;
        if !item.validated {
            evidence.push(EvidenceItem::new(
                format!("constraint:{}.{}", fk.child_table, fk.constraint_name),
                Severity::Warn,
                format!(
                    "外键 {}.{} 未被验证（NOT VALID），本轮不执行孤儿行扫描；\
                     请先对约束执行 VALIDATE CONSTRAINT 再复检",
                    fk.child_table, fk.constraint_name
                ),
                json!({
                    "child_table": fk.child_table,
                    "constraint": fk.constraint_name,
                    "parent_table": fk.parent_table,
                    "validated": false,
                    "orphan_count": null,
                }),
            ));
            continue;
        }
        let sql = match pg_orphan_count_sql(fk, thresholds.orphan_count_cap) {
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
                    "validated": true,
                }),
            ));
        }
    }
    evidence.sort_by(|a, b| a.key.cmp(&b.key));
    finish(descriptor, evidence, fks.len(), started)
}

// ---------------------------------------------------------------------------
// 账号：高权限 / 无密码
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct PgRole {
    name: String,
    can_login: bool,
    superuser: bool,
    createrole: bool,
    createdb: bool,
    replication: bool,
    bypassrls: bool,
}

#[derive(Debug, Clone)]
struct PgMembership {
    granted_role: String,
    member: String,
    admin_option: bool,
}

async fn load_roles(ctx: &OpsContext) -> Result<Vec<PgRole>, String> {
    let sql = "SELECT r.rolname::text AS role_name, r.rolcanlogin::text AS rolcanlogin, \
                      r.rolsuper::text AS rolsuper, r.rolcreaterole::text AS rolcreaterole, \
                      r.rolcreatedb::text AS rolcreatedb, r.rolreplication::text AS rolreplication, \
                      r.rolbypassrls::text AS rolbypassrls \
               FROM pg_roles r ORDER BY r.rolname";
    let rows = fetch_read_only(ctx, sql)
        .await
        .map_err(|err| format!("读取 pg_roles 失败: {err}"))?;
    Ok(rows
        .iter()
        .filter_map(|row| {
            Some(PgRole {
                name: str_value(row, "role_name")?,
                can_login: row_truthy(row, "rolcanlogin"),
                superuser: row_truthy(row, "rolsuper"),
                createrole: row_truthy(row, "rolcreaterole"),
                createdb: row_truthy(row, "rolcreatedb"),
                replication: row_truthy(row, "rolreplication"),
                bypassrls: row_truthy(row, "rolbypassrls"),
            })
        })
        .collect())
}

async fn load_memberships(ctx: &OpsContext) -> Result<Vec<PgMembership>, String> {
    let sql = "SELECT granted.rolname::text AS granted_role, member.rolname::text AS member_role, \
                      m.admin_option::text AS admin_option \
               FROM pg_auth_members m \
               JOIN pg_roles granted ON granted.oid = m.roleid \
               JOIN pg_roles member ON member.oid = m.member \
               ORDER BY member.rolname, granted.rolname";
    let rows = fetch_read_only(ctx, sql)
        .await
        .map_err(|err| format!("读取 pg_auth_members 失败: {err}"))?;
    Ok(rows
        .iter()
        .filter_map(|row| {
            Some(PgMembership {
                granted_role: str_value(row, "granted_role")?,
                member: str_value(row, "member_role")?,
                admin_option: row_truthy(row, "admin_option"),
            })
        })
        .collect())
}

fn is_system_role(role: &PgRole) -> bool {
    !role.can_login
        || role.name.eq_ignore_ascii_case("postgres")
        || role.name.starts_with("pg_")
}

fn detect_pg_over_privileged(roles: &[PgRole], memberships: &[PgMembership]) -> Vec<EvidenceItem> {
    let super_roles: HashSet<&str> = roles
        .iter()
        .filter(|role| role.superuser)
        .map(|role| role.name.as_str())
        .collect();
    let mut evidence = Vec::new();
    for role in roles.iter().filter(|role| !is_system_role(role)) {
        let own: Vec<&PgMembership> = memberships
            .iter()
            .filter(|item| item.member == role.name)
            .collect();
        let inherits_super = own
            .iter()
            .any(|item| super_roles.contains(item.granted_role.as_str()));
        let mut privileges = Vec::new();
        if role.superuser || inherits_super {
            privileges.push("SUPERUSER");
        }
        if role.createrole {
            privileges.push("CREATEROLE");
        }
        if role.createdb {
            privileges.push("CREATEDB");
        }
        if role.replication {
            privileges.push("REPLICATION");
        }
        if role.bypassrls {
            privileges.push("BYPASSRLS");
        }
        if own.iter().any(|item| item.admin_option) {
            privileges.push("ADMIN OPTION");
        }
        if own
            .iter()
            .any(|item| item.granted_role == "pg_read_server_files")
        {
            privileges.push("pg_read_server_files");
        }
        if own
            .iter()
            .any(|item| item.granted_role == "pg_write_server_files")
        {
            privileges.push("pg_write_server_files");
        }
        if privileges.is_empty() {
            continue;
        }
        let critical = privileges.iter().any(|privilege| {
            matches!(
                *privilege,
                "SUPERUSER" | "CREATEROLE" | "BYPASSRLS" | "ADMIN OPTION"
            )
        });
        evidence.push(EvidenceItem::new(
            role.name.clone(),
            if critical {
                Severity::Critical
            } else {
                Severity::Warn
            },
            format!(
                "角色 {} 持有高权限：{}（可登录）",
                role.name,
                privileges.join(", ")
            ),
            json!({
                "role": role.name,
                "user": role.name,
                "privileges": privileges,
                "can_login": role.can_login,
                "rolsuper": role.superuser,
                "rolcreaterole": role.createrole,
                "rolcreatedb": role.createdb,
                "rolreplication": role.replication,
                "rolbypassrls": role.bypassrls,
            }),
        ));
    }
    evidence.sort_by(|a, b| a.key.cmp(&b.key));
    evidence
}

async fn check_over_privileged_accounts(
    ctx: &OpsContext,
    descriptor: &CheckDescriptor,
    started: Instant,
) -> CheckResult {
    let roles = match load_roles(ctx).await {
        Ok(roles) => roles,
        Err(err) => return skipped(descriptor, err, started),
    };
    let memberships = match load_memberships(ctx).await {
        Ok(items) => items,
        Err(err) => return skipped(descriptor, err, started),
    };
    let evidence = detect_pg_over_privileged(&roles, &memberships);
    let mut result = finish(descriptor, evidence, roles.len(), started);
    result.not_applicable.push(
        "PostgreSQL 没有 MySQL 的 PROCESS/FILE/GRANT OPTION 精确对应语义，插件维度也未映射；\
         角色也没有 Host=% 概念，未做来源主机收敛检查"
            .into(),
    );
    result
}

#[derive(Debug, Clone)]
struct HbaRule {
    line: u64,
    connect_type: String,
    databases: String,
    user_names: String,
    address: String,
    auth_method: String,
    error: Option<String>,
}

async fn load_hba_rules(ctx: &OpsContext) -> Result<Vec<HbaRule>, String> {
    let sql = "SELECT line_number::text AS line_number, type::text AS type, \
                      array_to_string(database, ',') AS databases, \
                      array_to_string(user_name, ',') AS user_names, \
                      COALESCE(address::text, '') AS address, \
                      auth_method::text AS auth_method, COALESCE(error, '') AS error \
               FROM pg_hba_file_rules ORDER BY line_number";
    let rows = fetch_read_only(ctx, sql).await.map_err(|err| {
        format!(
            "读取 pg_hba_file_rules 需要超级用户或 pg_read_server_files 权限，已跳过 hba 规则检查: {err}"
        )
    })?;
    Ok(rows
        .iter()
        .filter_map(|row| {
            Some(HbaRule {
                line: uint_value(row, "line_number")?,
                connect_type: str_value(row, "type").unwrap_or_default(),
                databases: str_value(row, "databases").unwrap_or_default(),
                user_names: str_value(row, "user_names").unwrap_or_default(),
                address: str_value(row, "address").unwrap_or_default(),
                auth_method: str_value(row, "auth_method").unwrap_or_default(),
                error: str_value(row, "error").filter(|value| !value.is_empty()),
            })
        })
        .collect())
}

fn detect_hba_trust(rules: &[HbaRule]) -> Vec<EvidenceItem> {
    rules
        .iter()
        .filter(|rule| rule.error.is_none() && rule.auth_method.eq_ignore_ascii_case("trust"))
        .map(|rule| {
            EvidenceItem::new(
                format!("hba:{}", rule.line),
                Severity::Warn,
                format!(
                    "pg_hba.conf 第 {} 行对 {} / 用户 {} / 来源 {} 使用 trust 认证（无需密码），\
                     建议改为 scram-sha-256 并限制来源",
                    rule.line,
                    if rule.databases.is_empty() { "-" } else { &rule.databases },
                    if rule.user_names.is_empty() { "-" } else { &rule.user_names },
                    if rule.address.is_empty() {
                        rule.connect_type.as_str()
                    } else {
                        &rule.address
                    }
                ),
                json!({
                    "line_number": rule.line,
                    "type": rule.connect_type,
                    "databases": rule.databases,
                    "user_names": rule.user_names,
                    "address": rule.address,
                    "auth_method": rule.auth_method,
                    "source": "pg_hba_file_rules",
                }),
            )
        })
        .collect()
}

async fn load_passwordless_roles(ctx: &OpsContext) -> Result<Vec<(String, bool)>, String> {
    let sql = "SELECT r.rolname::text AS role_name, (r.rolpassword IS NULL)::text AS no_password \
               FROM pg_authid r WHERE r.rolcanlogin ORDER BY r.rolname";
    let rows = fetch_read_only(ctx, sql).await.map_err(|err| {
        format!(
            "pg_authid 不可读（需要超级用户权限），已跳过无密码账号检查；不返回“0 个无密码账号”的假阴性: {err}"
        )
    })?;
    Ok(rows
        .iter()
        .filter_map(|row| {
            Some((
                str_value(row, "role_name")?,
                row_truthy(row, "no_password"),
            ))
        })
        .collect())
}

fn detect_pg_passwordless(roles: &[(String, bool)]) -> Vec<EvidenceItem> {
    roles
        .iter()
        .filter(|(name, no_password)| {
            *no_password
                && !name.eq_ignore_ascii_case("postgres")
                && !name.starts_with("pg_")
        })
        .map(|(name, _)| {
            EvidenceItem::new(
                name.clone(),
                Severity::Critical,
                format!("角色 {name} 可登录但未设置密码（pg_authid.rolpassword 为空）"),
                json!({
                    "role": name,
                    "user": name,
                    "password_source": "pg_authid.rolpassword",
                    "account_locked_supported": false,
                }),
            )
        })
        .collect()
}

async fn check_passwordless_accounts(
    ctx: &OpsContext,
    descriptor: &CheckDescriptor,
    started: Instant,
) -> CheckResult {
    let roles = match load_passwordless_roles(ctx).await {
        Ok(roles) => roles,
        Err(err) => {
            // 即使账号明细不可读，也要把 hba 的降级原因一并写清楚，
            // 避免只报一半依赖权限的子项。
            let mut reason = err;
            match load_hba_rules(ctx).await {
                Ok(_) => reason.push_str(
                    "；pg_hba.conf trust 规则可读，但缺少账号明细时本项整体跳过，\
                     不输出不完整的“无密码账号”结论",
                ),
                Err(hba_err) => reason.push_str(&format!("；{hba_err}")),
            }
            return skipped(descriptor, reason, started);
        }
    };
    let mut evidence = detect_pg_passwordless(&roles);
    let mut checked = roles.len();
    let mut not_applicable = vec![
        "PostgreSQL 没有 account_locked 字段，无法区分“已锁定但无密码”的账号；\
         也没有 MySQL plugin/auth_socket 概念，插件维度未检查"
            .to_string(),
    ];
    match load_hba_rules(ctx).await {
        Ok(rules) => {
            checked += rules.len();
            evidence.extend(detect_hba_trust(&rules));
        }
        Err(reason) => not_applicable.push(reason),
    }
    evidence.sort_by(|a, b| a.key.cmp(&b.key));
    let mut result = finish(descriptor, evidence, checked, started);
    result.not_applicable.extend(not_applicable);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(pairs: &[(&str, &str)]) -> RowMap {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    fn column_key(name: &str, sort: &str) -> PgIndexKey {
        PgIndexKey {
            normalized: name.to_string(),
            display: name.to_string(),
            column_name: Some(name.to_string()),
            sort: sort.to_string(),
        }
    }

    fn expression_key(expression: &str, sort: &str) -> PgIndexKey {
        PgIndexKey {
            normalized: normalize_index_expression(SqlDialect::Postgres, expression),
            display: expression.to_string(),
            column_name: None,
            sort: sort.to_string(),
        }
    }

    fn def(
        name: &str,
        unique: bool,
        primary: bool,
        method: &str,
        predicate: Option<&str>,
        keys: Vec<PgIndexKey>,
    ) -> PgIndexDef {
        PgIndexDef {
            table: "public.t".into(),
            name: name.into(),
            unique,
            primary,
            method: method.into(),
            predicate: predicate.map(|predicate| {
                normalize_index_expression(SqlDialect::Postgres, predicate)
            }),
            keys,
        }
    }

    #[test]
    fn parse_index_defs_keeps_expression_sort_and_predicate() {
        let rows = vec![
            row(&[
                ("table_name", "t"),
                ("index_name", "idx_expr"),
                ("index_method", "btree"),
                ("is_unique", "false"),
                ("is_primary", "false"),
                ("predicate", "(id > 10)"),
                ("col_seq", "1"),
                ("column_def", "id"),
                ("column_name", "id"),
                ("is_expression", "false"),
                ("sort_order", "ASC"),
            ]),
            row(&[
                ("table_name", "t"),
                ("index_name", "idx_expr"),
                ("index_method", "btree"),
                ("is_unique", "false"),
                ("is_primary", "false"),
                ("predicate", "(id > 10)"),
                ("col_seq", "2"),
                ("column_def", "lower(name)"),
                ("column_name", ""),
                ("is_expression", "true"),
                ("sort_order", "DESC"),
            ]),
        ];
        let defs = parse_index_defs(&rows);
        assert_eq!(defs.len(), 1);
        let def = &defs[0];
        assert_eq!(def.keys.len(), 2);
        assert_eq!(def.keys[0].display, "id");
        assert_eq!(def.keys[0].column_name.as_deref(), Some("id"));
        assert_eq!(def.keys[1].display, "lower(name)");
        assert_eq!(def.keys[1].column_name, None);
        assert_eq!(def.keys[1].sort, "DESC");
        assert_eq!(
            def.predicate.as_deref(),
            Some(normalize_index_expression(SqlDialect::Postgres, "id > 10").as_str())
        );
    }

    #[test]
    fn duplicate_columns_and_predicate_are_redundant() {
        let defs = vec![
            def("idx_full", false, false, "btree", None, vec![column_key("a", "ASC"), column_key("b", "ASC")]),
            def("idx_prefix", false, false, "btree", None, vec![column_key("a", "ASC")]),
        ];
        let (evidence, checked) = detect_pg_redundant_indexes(&defs, &[]);
        assert_eq!(checked, 2);
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].key, "public.t.idx_prefix");
        assert_eq!(evidence[0].fields["reason"], "covered_by_prefix");
        assert_eq!(evidence[0].fields["covering_index"], "idx_full");
    }

    #[test]
    fn different_predicate_is_not_redundant() {
        let defs = vec![
            def("idx_all", false, false, "btree", None, vec![column_key("a", "ASC")]),
            def("idx_partial", false, false, "btree", Some("a > 10"), vec![column_key("a", "ASC")]),
        ];
        let (evidence, _) = detect_pg_redundant_indexes(&defs, &[]);
        assert!(evidence.is_empty(), "部分索引与非部分索引谓词不同，不能判冗余");
    }

    #[test]
    fn different_expression_is_not_redundant() {
        let defs = vec![
            def("idx_lower", false, false, "btree", None, vec![expression_key("lower(name)", "ASC")]),
            def("idx_upper", false, false, "btree", None, vec![expression_key("upper(name)", "ASC")]),
        ];
        let (evidence, _) = detect_pg_redundant_indexes(&defs, &[]);
        assert!(evidence.is_empty(), "表达式不同必须报 diff，不能按列名蒙过去");
    }

    #[test]
    fn normalized_expression_equivalence_is_redundant() {
        let defs = vec![
            def("idx_a", false, false, "btree", None, vec![expression_key("lower(name)", "ASC")]),
            def("idx_b", false, false, "btree", None, vec![expression_key("( lower( name ) )", "ASC")]),
        ];
        let (evidence, _) = detect_pg_redundant_indexes(&defs, &[]);
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].fields["reason"], "duplicate");
    }

    #[test]
    fn non_btree_indexes_are_skipped() {
        let defs = vec![
            def("idx_a", false, false, "hash", None, vec![column_key("a", "ASC")]),
            def("idx_b", false, false, "hash", None, vec![column_key("a", "ASC")]),
        ];
        let (evidence, checked) = detect_pg_redundant_indexes(&defs, &[]);
        assert_eq!(checked, 0);
        assert!(evidence.is_empty());
    }

    #[test]
    fn unique_index_is_never_reported_as_redundant() {
        let defs = vec![
            def("idx_u", true, false, "btree", None, vec![column_key("a", "ASC")]),
            def("idx_plain", false, false, "btree", None, vec![column_key("a", "ASC")]),
        ];
        let (evidence, _) = detect_pg_redundant_indexes(&defs, &[]);
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].fields["redundant_index"], "idx_plain");
        assert_eq!(evidence[0].fields["redundant_unique"], false);
    }

    #[test]
    fn over_privileged_roles_follow_pg_semantics() {
        let roles = vec![
            PgRole {
                name: "app".into(),
                can_login: true,
                superuser: false,
                createrole: false,
                createdb: false,
                replication: false,
                bypassrls: false,
            },
            PgRole {
                name: "dba".into(),
                can_login: true,
                superuser: true,
                createrole: true,
                createdb: true,
                replication: true,
                bypassrls: true,
            },
            PgRole {
                name: "postgres".into(),
                can_login: true,
                superuser: true,
                createrole: true,
                createdb: true,
                replication: true,
                bypassrls: true,
            },
            PgRole {
                name: "nologin".into(),
                can_login: false,
                superuser: true,
                createrole: false,
                createdb: false,
                replication: false,
                bypassrls: false,
            },
        ];
        let memberships = vec![PgMembership {
            granted_role: "analyst".into(),
            member: "app".into(),
            admin_option: true,
        }];
        let evidence = detect_pg_over_privileged(&roles, &memberships);
        let keys: Vec<&str> = evidence.iter().map(|item| item.key.as_str()).collect();
        assert_eq!(keys, vec!["app", "dba"]);
        assert_eq!(evidence[0].severity, Severity::Critical);
        assert_eq!(evidence[0].fields["privileges"], json!(["ADMIN OPTION"]));
        assert_eq!(evidence[1].fields["privileges"][0], "SUPERUSER");
    }

    #[test]
    fn hba_trust_rules_are_flagged() {
        let rules = vec![
            HbaRule {
                line: 5,
                connect_type: "host".into(),
                databases: "all".into(),
                user_names: "all".into(),
                address: "0.0.0.0/0".into(),
                auth_method: "trust".into(),
                error: None,
            },
            HbaRule {
                line: 6,
                connect_type: "host".into(),
                databases: "all".into(),
                user_names: "all".into(),
                address: "0.0.0.0/0".into(),
                auth_method: "scram-sha-256".into(),
                error: None,
            },
        ];
        let evidence = detect_hba_trust(&rules);
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].key, "hba:5");
        assert_eq!(evidence[0].severity, Severity::Warn);
    }

    #[test]
    fn passwordless_roles_skip_builtin_accounts() {
        let roles = vec![
            ("app".to_string(), true),
            ("postgres".to_string(), true),
            ("pg_checkpoint".to_string(), true),
            ("svc".to_string(), false),
        ];
        let evidence = detect_pg_passwordless(&roles);
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].key, "app");
        assert_eq!(evidence[0].severity, Severity::Critical);
    }

    #[test]
    fn not_valid_foreign_keys_are_reported_without_scanning() {
        let rows = vec![
            row(&[
                ("table_name", "orders"),
                ("constraint_name", "fk_customer"),
                ("column_name", "customer_id"),
                ("referenced_table_name", "customers"),
                ("referenced_column_name", "id"),
                ("ordinal_position", "1"),
                ("convalidated", "false"),
            ]),
        ];
        let fks = parse_foreign_keys(&rows);
        assert_eq!(fks.len(), 1);
        assert!(!fks[0].validated);
        assert_eq!(fks[0].def.child_columns, vec!["customer_id".to_string()]);
    }

    #[test]
    fn orphan_sql_uses_postgres_quoting() {
        let fk = ForeignKeyDef {
            child_table: "orders".into(),
            constraint_name: "fk_customer".into(),
            child_columns: vec!["customer_id".into()],
            parent_table: "customers".into(),
            parent_columns: vec!["id".into()],
        };
        let sql = pg_orphan_count_sql(&fk, 10).unwrap();
        assert!(sql.contains("FROM \"orders\" c"));
        assert!(sql.contains("LEFT JOIN \"customers\" p"));
        assert!(sql.contains("c.\"customer_id\" = p.\"id\""));
        assert!(sql.contains("LIMIT 10"));
    }
}

