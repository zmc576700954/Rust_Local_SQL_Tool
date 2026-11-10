//! SQLite 结构同步。
//!
//! SQLite 没有 MySQL 那样的列级 `ALTER`：改类型/改可空/改默认值/主键变更
//! 都必须整表重建。重建脚本由本模块生成，并整体在一个事务内执行：
//!
//! ```text
//! PRAGMA foreign_keys = OFF;      -- 连接级开关，不能在事务内切换
//! PRAGMA legacy_alter_table = ON; -- RENAME 时不改写其他表的 REFERENCES
//! BEGIN;
//!   CREATE TABLE "__sqlite_sync_new_x" (...);
//!   INSERT INTO "__sqlite_sync_new_x" (...) SELECT ... FROM "x";
//!   DROP TABLE "x";
//!   ALTER TABLE "__sqlite_sync_new_x" RENAME TO "x";
//!   CREATE INDEX ... / CREATE TRIGGER ... / 视图
//!   PRAGMA foreign_key_check;      -- 校验最终结构，有孤儿行则回滚
//! COMMIT;
//! PRAGMA legacy_alter_table = OFF;
//! PRAGMA foreign_keys = ON;
//! ```
//!
//! SQLite 的 DDL 是事务性的，任何一步失败（含外键校验失败）都会回滚到
//! 重建前的旧表；PRAGMA 的设置在成功/失败路径都由执行器恢复为进入前的值。
//!
//! 表达式索引与部分索引按 `sqlite_master.sql` 还原出的表达式文本/谓词
//! 生成 `CREATE INDEX ... ((expr)) WHERE pred`；只有元数据不足以安全重建
//! 时才进入 `unsupported`（区分「未支持」与「无法安全转换」）并跳过该表。

use crate::db::DbError;
use crate::pg_schema_sync::DestructiveChange;
use crate::schema::{
    normalize_index_expression, ColumnInfo, IndexInfo, SchemaResponse, SqlDialect, SyncIssue,
    TableWithDetails,
};
use crate::tools::{IndexDiff, SchemaDiff, SyncEngine};
use sqlx::{Row, SqlitePool};
use std::collections::BTreeMap;

pub const REBUILD_MARKER: &str = "-- sqlite-rebuild-plan";

/// 脚本内声明需要做外键校验的表（JSON 数组，包含被重建/新建的表）。
const CHECK_MARKER_PREFIX: &str = "-- sqlite-rebuild-check: ";
const BODY_BEGIN_MARKER: &str = "-- sqlite-rebuild-body-begin";
const BODY_END_MARKER: &str = "-- sqlite-rebuild-body-end";
/// 错误里最多展开的外键违规条数，其余只给总数。
const FK_VIOLATION_DETAIL_LIMIT: usize = 10;

#[derive(Debug, Clone, Default)]
pub struct SqliteDdlPlan {
    /// 事务内语句（不含 PRAGMA/BEGIN/COMMIT）。
    pub statements: Vec<String>,
    pub destructive: Vec<DestructiveChange>,
    pub unsupported: Vec<SyncIssue>,
    /// COMMIT 前做外键校验的表（重建/新建的表）。
    pub check_tables: Vec<String>,
}

impl SqliteDdlPlan {
    pub fn script(&self) -> String {
        let mut out = String::new();
        out.push_str(REBUILD_MARKER);
        out.push('\n');
        out.push_str(CHECK_MARKER_PREFIX);
        out.push_str(
            &serde_json::to_string(&self.check_tables).unwrap_or_else(|_| "[]".to_string()),
        );
        out.push('\n');
        out.push_str("PRAGMA foreign_keys = OFF;\n");
        out.push_str("PRAGMA legacy_alter_table = ON;\n");
        out.push_str("BEGIN;\n");
        out.push_str(BODY_BEGIN_MARKER);
        out.push('\n');
        for stmt in &self.statements {
            out.push_str(stmt.trim().trim_end_matches(';'));
            out.push_str(";\n");
        }
        out.push_str(BODY_END_MARKER);
        out.push('\n');
        out.push_str("COMMIT;\n");
        out.push_str("PRAGMA legacy_alter_table = OFF;\n");
        out.push_str("PRAGMA foreign_keys = ON;\n");
        out
    }
}

#[derive(Debug, Clone)]
pub struct SqliteSyncResult {
    pub diff: SchemaDiff,
    pub destructive: Vec<DestructiveChange>,
    pub unsupported: Vec<SyncIssue>,
}

#[derive(Debug, Clone)]
pub struct SqliteRequiresConfirmation {
    pub destructive: Vec<DestructiveChange>,
}

#[derive(Debug)]
pub enum SqliteSyncError {
    RequiresConfirmation(Vec<DestructiveChange>),
    Unsupported(String),
}

impl std::fmt::Display for SqliteSyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RequiresConfirmation(items) => write!(
                f,
                "destructive changes require confirmation: {}",
                items
                    .iter()
                    .map(|d| format!("{}.{}", d.table_name, d.kind))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::Unsupported(msg) => write!(f, "{}", msg),
        }
    }
}

impl std::error::Error for SqliteSyncError {}

pub struct SqliteSchemaSyncEngine;

impl SqliteSchemaSyncEngine {
    pub fn compare(source: &SchemaResponse, target: &SchemaResponse) -> SqliteSyncResult {
        let (mut diff, _) = SyncEngine::schema_sync(source, target);
        let mut destructive = Vec::new();
        let mut unsupported = Vec::new();

        for table_diff in &mut diff.tables {
            if table_diff.status == "removed" {
                destructive.push(DestructiveChange {
                    table_name: table_diff.table_name.clone(),
                    kind: "drop_table".to_string(),
                    detail: format!("删除目标表 {}", table_diff.table_name),
                });
                continue;
            }
            let Some(src) = source
                .tables
                .iter()
                .find(|t| t.table_name == table_diff.table_name)
            else {
                continue;
            };
            let tgt = target
                .tables
                .iter()
                .find(|t| t.table_name == table_diff.table_name);

            // 索引按名称分组对比。SyncEngine 的逐行比较无法表达"表达式列"
            // 与"WHERE 谓词"，这里用规范化后的文本重新判定，覆盖其索引结果。
            if let Some(tgt) = tgt {
                let src_groups = grouped_indexes(src);
                let tgt_groups = grouped_indexes(tgt);
                let mut added = Vec::new();
                let mut removed = Vec::new();
                let mut modified = Vec::new();
                for (name, src_rows) in &src_groups {
                    match tgt_groups.get(name) {
                        Some(tgt_rows) if !index_rows_equal(src_rows, tgt_rows) => {
                            modified.extend(index_row_diffs(tgt_rows, src_rows));
                        }
                        Some(_) => {}
                        None => added.extend(src_rows.clone()),
                    }
                }
                for (name, tgt_rows) in &tgt_groups {
                    if !src_groups.contains_key(name) {
                        removed.extend(tgt_rows.clone());
                    }
                }
                table_diff.indexes_added = added;
                table_diff.indexes_removed = removed;
                table_diff.indexes_modified = modified;
            } else {
                table_diff.indexes_added = group_index_rows(src);
            }

            if let Some(tgt) = tgt {
                if tgt.without_rowid != src.without_rowid {
                    table_diff.status = "modified".to_string();
                }
                if !same_triggers(&src.triggers, &tgt.triggers) {
                    table_diff.status = "modified".to_string();
                }
                let changed = !table_diff.columns_added.is_empty()
                    || !table_diff.columns_removed.is_empty()
                    || !table_diff.columns_modified.is_empty()
                    || !table_diff.indexes_added.is_empty()
                    || !table_diff.indexes_removed.is_empty()
                    || !table_diff.indexes_modified.is_empty()
                    || !table_diff.fks_added.is_empty()
                    || !table_diff.fks_removed.is_empty()
                    || !table_diff.fks_modified.is_empty()
                    || tgt.without_rowid != src.without_rowid
                    || !same_triggers(&src.triggers, &tgt.triggers);
                table_diff.status = if changed {
                    "modified".to_string()
                } else {
                    "unchanged".to_string()
                };
            }

            if !table_diff.columns_removed.is_empty() {
                destructive.push(DestructiveChange {
                    table_name: table_diff.table_name.clone(),
                    kind: "drop_column".to_string(),
                    detail: format!(
                        "删除列 {}",
                        table_diff
                            .columns_removed
                            .iter()
                            .map(|c| c.column_name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                });
            }
            for change in &table_diff.columns_modified {
                if change.old.column_type != change.new.column_type {
                    destructive.push(DestructiveChange {
                        table_name: table_diff.table_name.clone(),
                        kind: "narrow_type".to_string(),
                        detail: format!(
                            "列 {} 类型 {} -> {}",
                            change.new.column_name, change.old.column_type, change.new.column_type
                        ),
                    });
                }
                if change.old.generation_expression != change.new.generation_expression
                    || change.old.extra != change.new.extra
                {
                    destructive.push(DestructiveChange {
                        table_name: table_diff.table_name.clone(),
                        kind: "rewrite_generated_column".to_string(),
                        detail: format!("列 {} 的生成列定义发生变化", change.new.column_name),
                    });
                }
            }
            if let Some(tgt) = tgt {
                let old_pk = primary_key_columns(tgt);
                let new_pk = primary_key_columns(src);
                if old_pk != new_pk {
                    table_diff.status = "modified".to_string();
                    destructive.push(DestructiveChange {
                        table_name: table_diff.table_name.clone(),
                        kind: "modify_primary_key".to_string(),
                        detail: format!(
                            "主键 {:?} -> {:?}",
                            old_pk.iter().map(|(n, _)| n).collect::<Vec<_>>(),
                            new_pk.iter().map(|(n, _)| n).collect::<Vec<_>>()
                        ),
                    });
                }
            }

            // 只有表确实需要变更时才报告无法安全同步的索引；
            // 完全一致的表不应因为存在无法解析的历史索引而反复告警。
            if table_diff.status != "unchanged" {
                let mut groups: Vec<Vec<IndexInfo>> =
                    grouped_indexes(src).into_values().collect();
                if let Some(tgt) = tgt {
                    groups.extend(grouped_indexes(tgt).into_values());
                }
                for group in &groups {
                    if let Some(reason) = index_rows_blocker(group) {
                        unsupported.push(SyncIssue::unsafe_conversion(
                            Some(&table_diff.table_name),
                            group.first().map(|r| r.index_name.as_str()),
                            reason,
                        ));
                    }
                }
            }
        }

        destructive.sort_by(|a, b| {
            a.table_name
                .cmp(&b.table_name)
                .then(a.kind.cmp(&b.kind))
                .then(a.detail.cmp(&b.detail))
        });
        destructive.dedup_by(|a, b| {
            a.table_name == b.table_name && a.kind == b.kind && a.detail == b.detail
        });
        unsupported.sort();
        unsupported.dedup();

        SqliteSyncResult {
            diff,
            destructive,
            unsupported,
        }
    }

    pub fn generate_ddl(
        source: &SchemaResponse,
        target: &SchemaResponse,
        selected_tables: Option<&[String]>,
        confirm_destructive: bool,
    ) -> Result<SqliteDdlPlan, SqliteSyncError> {
        let result = Self::compare(source, target);
        if !confirm_destructive && !result.destructive.is_empty() {
            return Err(SqliteSyncError::RequiresConfirmation(
                result.destructive.clone(),
            ));
        }

        let selected = |name: &str| {
            selected_tables
                .map(|list| list.is_empty() || list.iter().any(|t| t == name))
                .unwrap_or(true)
        };
        let mut plan = SqliteDdlPlan {
            destructive: result.destructive.clone(),
            unsupported: result.unsupported.clone(),
            ..Default::default()
        };
        for table_diff in &result.diff.tables {
            if !selected(&table_diff.table_name) {
                continue;
            }
            if table_diff.status == "removed" {
                plan.statements.push(format!(
                    "DROP TABLE {}",
                    quote_ident(&table_diff.table_name)
                ));
                continue;
            }
            let Some(src) = source
                .tables
                .iter()
                .find(|t| t.table_name == table_diff.table_name)
            else {
                continue;
            };
            let tgt = target
                .tables
                .iter()
                .find(|t| t.table_name == table_diff.table_name);
            match table_diff.status.as_str() {
                "added" => {
                    if table_has_blocked_index(src) {
                        continue;
                    }
                    plan.statements
                        .push(create_table_sql(src, &src.table_name));
                    push_index_statements(&mut plan, src);
                    push_trigger_statements(&mut plan, src, &[]);
                    plan.check_tables.push(src.table_name.clone());
                }
                "modified" => {
                    if table_has_blocked_index(src) {
                        continue;
                    }
                    let Some(tgt) = tgt else {
                        continue;
                    };
                    // 仅索引/触发器不同时不需要重建表：直接 DROP/CREATE INDEX，
                    // 避免整表拷贝。列、外键、WITHOUT ROWID 变化才走重建。
                    let structure_changed = !table_diff.columns_added.is_empty()
                        || !table_diff.columns_removed.is_empty()
                        || !table_diff.columns_modified.is_empty()
                        || !table_diff.fks_added.is_empty()
                        || !table_diff.fks_removed.is_empty()
                        || !table_diff.fks_modified.is_empty()
                        || primary_key_columns(src) != primary_key_columns(tgt)
                        || src.without_rowid != tgt.without_rowid;
                    if !structure_changed {
                        push_index_diff_statements(&mut plan, src, tgt);
                        if !same_triggers(&src.triggers, &tgt.triggers) {
                            push_trigger_statements(&mut plan, src, &tgt.triggers);
                        }
                        continue;
                    }
                    let mut table_statements = Vec::new();
                    if let Err(reason) = rebuild_table_sql(src, tgt, &mut table_statements) {
                        plan.unsupported.push(SyncIssue::unsafe_conversion(
                            Some(&src.table_name),
                            None,
                            reason,
                        ));
                        continue;
                    }
                    plan.statements.extend(table_statements);
                    push_index_statements(&mut plan, src);
                    // DROP TABLE 会一并删除触发器；重建后必须无条件恢复，
                    // 不能因为源/目标定义相同而跳过。
                    push_trigger_statements(&mut plan, src, &[]);
                    plan.check_tables.push(src.table_name.clone());
                }
                _ => {
                    // 表本身没变：索引/触发器差异可以原地同步，不需要重建。
                    if let Some(tgt) = tgt {
                        push_index_diff_statements(&mut plan, src, tgt);
                        if !same_triggers(&src.triggers, &tgt.triggers) {
                            push_trigger_statements(&mut plan, src, &tgt.triggers);
                        }
                    }
                }
            }
        }

        // 触发器/视图的删除放在重建之后、创建之前更安全；这里按表逐个
        // DROP IF EXISTS + CREATE 已能覆盖触发器；视图最后单独同步。
        push_view_statements(&mut plan, source, target);

        if plan.statements.is_empty() && plan.unsupported.is_empty() {
            return Err(SqliteSyncError::Unsupported(
                "没有可执行的 SQLite 结构差异".to_string(),
            ));
        }
        Ok(plan)
    }
}

fn grouped_indexes(table: &TableWithDetails) -> BTreeMap<String, Vec<IndexInfo>> {
    let mut map: BTreeMap<String, Vec<IndexInfo>> = BTreeMap::new();
    for idx in &table.indexes {
        map.entry(idx.index_name.clone())
            .or_default()
            .push(idx.clone());
    }
    for rows in map.values_mut() {
        rows.sort_by_key(|r| r.seq_in_index);
    }
    map
}

fn group_index_rows(table: &TableWithDetails) -> Vec<IndexInfo> {
    grouped_indexes(table).into_values().flatten().collect()
}

fn normalized_predicate(predicate: &Option<String>) -> Option<String> {
    predicate
        .as_ref()
        .map(|p| normalize_index_expression(SqlDialect::Sqlite, p))
}

fn normalized_expression(expression: &Option<String>) -> Option<String> {
    expression
        .as_ref()
        .map(|e| normalize_index_expression(SqlDialect::Sqlite, e))
}

/// 同名的多列索引必须整体比较：列顺序、表达式文本、排序方向、谓词任一不同
/// 都是差异。只比较第一行或只比较列名会把表达式索引误判为相同。
fn index_rows_equal(a: &[IndexInfo], b: &[IndexInfo]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b.iter()).all(|(x, y)| {
        x.non_unique == y.non_unique
            && x.sort_order.eq_ignore_ascii_case(&y.sort_order)
            && normalized_predicate(&x.predicate) == normalized_predicate(&y.predicate)
            && match (&x.expression, &y.expression) {
                (Some(ex), Some(ey)) => normalized_expression(&Some(ex.clone()))
                    == normalized_expression(&Some(ey.clone())),
                (None, None) => x.column_name.eq_ignore_ascii_case(&y.column_name),
                _ => false,
            }
    })
}

fn index_row_diffs(tgt_rows: &[IndexInfo], src_rows: &[IndexInfo]) -> Vec<IndexDiff> {
    src_rows
        .iter()
        .enumerate()
        .map(|(i, new)| IndexDiff {
            old: tgt_rows
                .get(i)
                .or_else(|| tgt_rows.first())
                .cloned()
                .unwrap_or_else(|| new.clone()),
            new: new.clone(),
        })
        .collect()
}

fn index_rows_blocker(rows: &[IndexInfo]) -> Option<String> {
    rows.iter().find_map(|r| r.conversion_blocker.clone())
}

fn table_has_blocked_index(table: &TableWithDetails) -> bool {
    grouped_indexes(table)
        .values()
        .any(|rows| index_rows_blocker(rows).is_some())
}

fn is_managed_index(rows: &[IndexInfo]) -> bool {
    rows.first()
        .map(|r| r.is_primary || r.index_name.starts_with("sqlite_autoindex_"))
        .unwrap_or(true)
}

fn index_create_sql(table_name: &str, rows: &[IndexInfo]) -> Option<String> {
    if is_managed_index(rows) || index_rows_blocker(rows).is_some() {
        return None;
    }
    let first = rows.first()?;
    let parts: Vec<String> = rows
        .iter()
        .map(|r| {
            let base = match &r.expression {
                Some(expr) => format!("({})", expr.trim()),
                None => quote_ident(&r.column_name),
            };
            let sort = if r.sort_order.eq_ignore_ascii_case("DESC") {
                "DESC"
            } else {
                "ASC"
            };
            format!("{} {}", base, sort)
        })
        .collect();
    let predicate = rows
        .iter()
        .find_map(|r| r.predicate.clone())
        .map(|p| format!(" WHERE {}", p.trim()))
        .unwrap_or_default();
    Some(format!(
        "CREATE {}INDEX {} ON {} ({}){}",
        if first.non_unique { "" } else { "UNIQUE " },
        quote_ident(&first.index_name),
        quote_ident(table_name),
        parts.join(", "),
        predicate
    ))
}

fn push_index_diff_statements(
    plan: &mut SqliteDdlPlan,
    src: &TableWithDetails,
    tgt: &TableWithDetails,
) {
    let src_groups = grouped_indexes(src);
    let tgt_groups = grouped_indexes(tgt);
    for (name, rows) in &src_groups {
        match tgt_groups.get(name) {
            Some(old_rows) if !index_rows_equal(rows, old_rows) => {
                if index_rows_blocker(rows).is_some() || index_rows_blocker(old_rows).is_some() {
                    continue;
                }
                if !is_managed_index(old_rows) {
                    plan.statements.push(format!("DROP INDEX IF EXISTS {}", quote_ident(name)));
                }
                if let Some(sql) = index_create_sql(&src.table_name, rows) {
                    plan.statements.push(sql);
                }
            }
            Some(_) => {}
            None => {
                if let Some(sql) = index_create_sql(&src.table_name, rows) {
                    plan.statements.push(sql);
                }
            }
        }
    }
    for (name, rows) in &tgt_groups {
        if !src_groups.contains_key(name) && !is_managed_index(rows) {
            plan.statements.push(format!("DROP INDEX IF EXISTS {}", quote_ident(name)));
        }
    }
}

fn rebuild_table_sql(
    src: &TableWithDetails,
    tgt: &TableWithDetails,
    out: &mut Vec<String>,
) -> Result<(), String> {
    let new_name = format!("__sqlite_sync_new_{}", src.table_name);
    out.push(create_table_sql(src, &new_name));

    let shared: Vec<&ColumnInfo> = src
        .columns
        .iter()
        .filter(|c| c.generation_expression.is_none())
        .filter(|c| {
            tgt.columns
                .iter()
                .any(|t| t.column_name == c.column_name && t.generation_expression.is_none())
        })
        .collect();
    if shared.is_empty() && !tgt.columns.iter().all(|c| c.generation_expression.is_some()) {
        return Err(format!(
            "{}.{}: 没有可映射的列，拒绝重建以避免静默丢数据",
            src.table_name, new_name
        ));
    }
    if !shared.is_empty() {
        let cols = shared
            .iter()
            .map(|c| quote_ident(&c.column_name))
            .collect::<Vec<_>>()
            .join(", ");
        out.push(format!(
            "INSERT INTO {} ({}) SELECT {} FROM {}",
            quote_ident(&new_name),
            cols,
            cols,
            quote_ident(&src.table_name)
        ));
    }
    out.push(format!("DROP TABLE {}", quote_ident(&src.table_name)));
    out.push(format!(
        "ALTER TABLE {} RENAME TO {}",
        quote_ident(&new_name),
        quote_ident(&src.table_name)
    ));
    Ok(())
}

fn create_table_sql(table: &TableWithDetails, name: &str) -> String {
    let pk = primary_key_columns(table);
    let rowid_alias = pk.len() == 1
        && !table.without_rowid
        && table
            .columns
            .iter()
            .find(|c| Some(c.column_name.as_str()) == Some(pk[0].0.as_str()))
            .map(|c| c.column_type.eq_ignore_ascii_case("INTEGER"))
            .unwrap_or(false);

    let mut defs: Vec<String> = Vec::new();
    for col in &table.columns {
        let mut def = format!("{} {}", quote_ident(&col.column_name), col.column_type);
        if col.is_nullable == "NO" {
            def.push_str(" NOT NULL");
        }
        if let Some(expr) = &col.generation_expression {
            def.push_str(&format!(" GENERATED ALWAYS AS ({})", expr));
            if col.extra.to_ascii_uppercase().contains("STORED") {
                def.push_str(" STORED");
            } else {
                def.push_str(" VIRTUAL");
            }
        } else {
            if let Some(default) = &col.column_default {
                def.push_str(&format!(" DEFAULT {}", default));
            }
            if rowid_alias && Some(col.column_name.as_str()) == Some(pk[0].0.as_str()) {
                def.push_str(" PRIMARY KEY");
                if col.extra.to_ascii_uppercase().contains("AUTOINCREMENT") {
                    def.push_str(" AUTOINCREMENT");
                }
            }
        }
        defs.push(def);
    }

    if !rowid_alias && !pk.is_empty() {
        defs.push(format!(
            "PRIMARY KEY ({})",
            pk.iter()
                .map(|(name, _)| quote_ident(name))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    let mut uniques: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for idx in table
        .indexes
        .iter()
        .filter(|i| !i.is_primary && !i.non_unique && i.index_name.starts_with("sqlite_autoindex_"))
    {
        if seen.insert(idx.index_name.clone()) {
            let cols: Vec<&IndexInfo> = table
                .indexes
                .iter()
                .filter(|i| i.index_name == idx.index_name)
                .collect();
            let mut cols = cols;
            cols.sort_by_key(|i| i.seq_in_index);
            uniques.push(format!(
                "UNIQUE ({})",
                cols.iter()
                    .map(|c| quote_ident(&c.column_name))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    defs.extend(uniques);

    let mut fk_groups: std::collections::BTreeMap<String, Vec<&crate::schema_ext::ForeignKeyInfo>> =
        std::collections::BTreeMap::new();
    for fk in &table.foreign_keys {
        fk_groups
            .entry(fk.constraint_name.clone())
            .or_default()
            .push(fk);
    }
    for (_, mut fks) in fk_groups {
        fks.sort_by_key(|f| f.ordinal_position);
        let cols = fks
            .iter()
            .map(|f| quote_ident(&f.column_name))
            .collect::<Vec<_>>()
            .join(", ");
        let ref_table = fks
            .first()
            .map(|f| f.referenced_table_name.clone())
            .unwrap_or_default();
        let ref_cols = fks
            .iter()
            .map(|f| f.referenced_column_name.as_str())
            .filter(|c| !c.is_empty())
            .map(quote_ident)
            .collect::<Vec<_>>()
            .join(", ");
        let mut clause = format!(
            "FOREIGN KEY ({}) REFERENCES {}{}",
            cols,
            quote_ident(&ref_table),
            if ref_cols.is_empty() {
                String::new()
            } else {
                format!(" ({})", ref_cols)
            }
        );
        if let Some(rule) = fks.first().map(|f| f.update_rule.as_str()) {
            if is_action_rule(rule) {
                clause.push_str(&format!(" ON UPDATE {}", rule));
            }
        }
        if let Some(rule) = fks.first().map(|f| f.delete_rule.as_str()) {
            if is_action_rule(rule) {
                clause.push_str(&format!(" ON DELETE {}", rule));
            }
        }
        defs.push(clause);
    }

    let mut sql = format!(
        "CREATE TABLE {} (\n  {}\n)",
        quote_ident(name),
        defs.join(",\n  ")
    );
    if table.without_rowid {
        sql.push_str(" WITHOUT ROWID");
    }
    sql
}

fn is_action_rule(rule: &str) -> bool {
    !rule.trim().is_empty() && !rule.eq_ignore_ascii_case("NO ACTION")
}

fn primary_key_columns(table: &TableWithDetails) -> Vec<(String, u32)> {
    let mut cols: Vec<(String, u32)> = table
        .columns
        .iter()
        .filter_map(|c| {
            c.primary_key_ordinal
                .map(|ord| (c.column_name.clone(), ord))
        })
        .collect();
    cols.sort_by_key(|(_, ord)| *ord);
    cols
}

fn push_index_statements(plan: &mut SqliteDdlPlan, table: &TableWithDetails) {
    for rows in grouped_indexes(table).values() {
        if let Some(sql) = index_create_sql(&table.table_name, rows) {
            plan.statements.push(sql);
        }
    }
}

fn push_trigger_statements(
    plan: &mut SqliteDdlPlan,
    table: &TableWithDetails,
    target_triggers: &[crate::schema_ext::TriggerInfo],
) {
    for tgt in target_triggers {
        if !table
            .triggers
            .iter()
            .any(|t| t.trigger_name == tgt.trigger_name)
        {
            plan.statements.push(format!(
                "DROP TRIGGER IF EXISTS {}",
                quote_ident(&tgt.trigger_name)
            ));
        }
    }
    for src in &table.triggers {
        let same = target_triggers
            .iter()
            .any(|t| t.trigger_name == src.trigger_name && t.sql == src.sql);
        if same {
            continue;
        }
        plan.statements.push(format!(
            "DROP TRIGGER IF EXISTS {}",
            quote_ident(&src.trigger_name)
        ));
        plan.statements.push(src.sql.clone());
    }
}

fn push_view_statements(
    plan: &mut SqliteDdlPlan,
    source: &SchemaResponse,
    target: &SchemaResponse,
) {
    for tgt in &target.views {
        if !source.views.iter().any(|v| v.table_name == tgt.table_name) {
            plan.statements
                .push(format!("DROP VIEW IF EXISTS {}", quote_ident(&tgt.table_name)));
        }
    }
    for src in &source.views {
        let same = target
            .views
            .iter()
            .any(|v| v.table_name == src.table_name && v.view_definition == src.view_definition);
        if same {
            continue;
        }
        if src.view_definition.trim().is_empty() {
            plan.unsupported.push(SyncIssue::unsafe_conversion(
                Some(&src.table_name),
                None,
                "视图缺少原始定义，无法同步",
            ));
            continue;
        }
        plan.statements
            .push(format!("DROP VIEW IF EXISTS {}", quote_ident(&src.table_name)));
        plan.statements.push(src.view_definition.clone());
    }
}

fn same_triggers(
    a: &[crate::schema_ext::TriggerInfo],
    b: &[crate::schema_ext::TriggerInfo],
) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().all(|x| {
        b.iter()
            .any(|y| y.trigger_name == x.trigger_name && y.sql == x.sql)
    })
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// 执行结构同步脚本。
///
/// `PRAGMA foreign_keys` 是连接级开关，事务内设置无效：在 `BEGIN` 之前关闭，
/// 无论成功、校验失败还是执行出错，最后都恢复为进入连接前的值，避免把连接
/// 留在 `foreign_keys=OFF` 导致后续写入失去约束。
///
/// 事务内、`COMMIT` 之前会先恢复索引/触发器/视图，再对重建后的表做
/// `PRAGMA foreign_key_check`；发现孤儿行则整笔重建回滚并报出明细。
pub fn execute_script<'a>(
    pool: &'a SqlitePool,
    script: &'a str,
) -> impl std::future::Future<Output = Result<(), DbError>> + Send + 'a {
    async move {
        if !script.trim_start().starts_with(REBUILD_MARKER) {
            return Err(DbError::Unsupported(
                "SQLite 结构同步脚本缺少 sqlite-rebuild-plan 标记".to_string(),
            ));
        }
        let check_tables = parse_check_tables(script)?;
        let body = extract_rebuild_body(script)?;

        // PRAGMA 是连接级状态，必须在同一条连接上读取并恢复，
        // 否则可能读到池里另一条连接的默认值。
        let mut conn = pool.acquire().await?;
        let original_fk = pragma_int(&mut conn, "foreign_keys").await?;
        let original_legacy = pragma_int(&mut conn, "legacy_alter_table").await?;

        use sqlx::Executor;
        // 事务外切换连接级 PRAGMA；事务内设置 foreign_keys 不会生效。
        let setup = Executor::execute(
            &mut *conn,
            sqlx::raw_sql("PRAGMA foreign_keys = OFF;\nPRAGMA legacy_alter_table = ON;"),
        )
        .await;

        let result = match setup {
            Err(err) => Err(DbError::from(err)),
            Ok(_) => run_rebuild_transaction(&mut conn, body, check_tables.as_deref()).await,
        };

        if result.is_err() {
            // 事务可能停在重建中途；回滚后原表仍在。
            let _ = Executor::execute(&mut *conn, sqlx::raw_sql("ROLLBACK")).await;
        }
        // 任何路径都恢复 PRAGMA，包括校验失败/建表失败/COMMIT 失败。
        let legacy_sql = format!("PRAGMA legacy_alter_table = {}", original_legacy);
        let _ = Executor::execute(&mut *conn, sqlx::raw_sql(&legacy_sql)).await;
        let fk_sql = format!("PRAGMA foreign_keys = {}", original_fk);
        let _ = Executor::execute(&mut *conn, sqlx::raw_sql(&fk_sql)).await;
        result
    }
}

async fn run_rebuild_transaction(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
    body: &str,
    check_tables: Option<&[String]>,
) -> Result<(), DbError> {
    use sqlx::Executor;
    Executor::execute(&mut **conn, sqlx::raw_sql("BEGIN")).await?;
    Executor::execute(&mut **conn, sqlx::raw_sql(body)).await?;

    // 索引/触发器/视图已在 body 中恢复，这里校验的是最终结构。
    let violations = collect_fk_violations(conn, check_tables).await?;
    if !violations.is_empty() {
        return Err(fk_violation_error(&violations));
    }

    Executor::execute(&mut **conn, sqlx::raw_sql("COMMIT")).await?;
    Ok(())
}

pub async fn execute_plan(pool: &SqlitePool, plan: &SqliteDdlPlan) -> Result<(), DbError> {
    if plan.statements.is_empty() {
        return Ok(());
    }
    execute_script(pool, &plan.script()).await
}

async fn pragma_int(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
    name: &str,
) -> Result<i64, DbError> {
    let row = sqlx::query(&format!("PRAGMA {}", name))
        .fetch_one(&mut **conn)
        .await?;
    Ok(row.try_get::<i64, _>(0).unwrap_or(0))
}

/// 读取脚本声明的校验表清单；旧脚本没有该行时返回 `None`，
/// 退回全库校验，避免漏检。
fn parse_check_tables(script: &str) -> Result<Option<Vec<String>>, DbError> {
    let line = script
        .lines()
        .find(|line| line.trim_start().starts_with(CHECK_MARKER_PREFIX));
    let Some(line) = line else {
        return Ok(None);
    };
    let payload = line.trim_start()[CHECK_MARKER_PREFIX.len()..].trim();
    serde_json::from_str::<Vec<String>>(payload).map(Some).map_err(|err| {
        DbError::Unsupported(format!(
            "SQLite 重建脚本的校验表清单无法解析（{err}）：{payload}"
        ))
    })
}

/// 取出 `BEGIN` 与 `COMMIT` 之间的重建语句；PRAGMA/事务边界由执行器控制。
fn extract_rebuild_body(script: &str) -> Result<&str, DbError> {
    let begin = script.find(BODY_BEGIN_MARKER).ok_or_else(|| {
        DbError::Unsupported("SQLite 重建脚本缺少 body-begin 标记".to_string())
    })? + BODY_BEGIN_MARKER.len();
    let end = script[begin..]
        .find(BODY_END_MARKER)
        .map(|offset| begin + offset)
        .ok_or_else(|| DbError::Unsupported("SQLite 重建脚本缺少 body-end 标记".to_string()))?;
    Ok(&script[begin..end])
}

#[derive(Debug, Clone)]
struct FkViolation {
    child_table: String,
    rowid: Option<i64>,
    parent_table: String,
    fk_id: i64,
}

/// 在事务内执行 `PRAGMA foreign_key_check`。
///
/// 选择全库扫描而不是逐表 `foreign_key_check(<table>)`：重建父表主键时，
/// 受影响的是其他子表，逐表检查只覆盖“自己作为子表”的情况。全库扫描
/// 后再按本次重建/新建的表过滤，既能覆盖父子两侧，又不会因无关表的
/// 历史孤儿数据阻塞同步。成本是遍历所有带外键的表，桌面单机库可接受。
async fn collect_fk_violations(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
    check_tables: Option<&[String]>,
) -> Result<Vec<FkViolation>, DbError> {
    if matches!(check_tables, Some(tables) if tables.is_empty()) {
        return Ok(Vec::new());
    }
    let rows = sqlx::query("PRAGMA foreign_key_check")
        .fetch_all(&mut **conn)
        .await?;
    let mut violations = Vec::new();
    for row in rows {
        let child_table: String = row.try_get(0).unwrap_or_default();
        let rowid: Option<i64> = row.try_get(1).ok().flatten();
        let parent_table: String = row.try_get(2).unwrap_or_default();
        let fk_id: i64 = row.try_get(3).unwrap_or(0);
        let relevant = match check_tables {
            Some(tables) => tables.iter().any(|table| {
                table.eq_ignore_ascii_case(&child_table)
                    || table.eq_ignore_ascii_case(&parent_table)
            }),
            None => true,
        };
        if relevant {
            violations.push(FkViolation {
                child_table,
                rowid,
                parent_table,
                fk_id,
            });
        }
    }
    Ok(violations)
}

fn fk_violation_error(violations: &[FkViolation]) -> DbError {
    let mut message = format!(
        "SQLite 外键校验失败，已回滚表重建：共 {} 条违规。",
        violations.len()
    );
    for (index, violation) in violations
        .iter()
        .take(FK_VIOLATION_DETAIL_LIMIT)
        .enumerate()
    {
        let rowid = match violation.rowid {
            Some(rowid) => format!("rowid={rowid}"),
            None => "rowid=NULL（WITHOUT ROWID 表无法从 foreign_key_check 定位单行）".to_string(),
        };
        message.push_str(&format!(
            "\n{}. 子表 \"{}\"，父表 \"{}\"，{}，fk#={}",
            index + 1,
            violation.child_table,
            violation.parent_table,
            rowid,
            violation.fk_id
        ));
    }
    if violations.len() > FK_VIOLATION_DETAIL_LIMIT {
        message.push_str(&format!(
            "\n... 其余 {} 条未展开",
            violations.len() - FK_VIOLATION_DETAIL_LIMIT
        ));
    }
    DbError::Unsupported(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DbType, PoolConfig};
    use crate::db::DbClient;
    use std::time::{SystemTime, UNIX_EPOCH};

    async fn temp_client(name: &str) -> (DbClient, String) {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "sqlite_sync_{}_{}_{}.db",
            name,
            std::process::id(),
            stamp
        ));
        let url = format!("sqlite://{}?mode=rwc", path.to_string_lossy());
        let client = DbClient::new(&url, &PoolConfig::default(), &DbType::SQLite)
            .await
            .expect("open sqlite");
        (client, path.to_string_lossy().to_string())
    }

    async fn exec(client: &DbClient, sql: &str) {
        sqlx::raw_sql(sql)
            .execute(client.pool.sqlite().unwrap())
            .await
            .expect(sql);
    }

    async fn schema(client: &DbClient) -> SchemaResponse {
        let tables = crate::schema::SchemaExtractor::get_tables(client, "main")
            .await
            .unwrap();
        let mut result = Vec::new();
        for t in tables {
            let columns = crate::schema::SchemaExtractor::get_columns(client, "main", &t.table_name)
                .await
                .unwrap();
            let indexes = crate::schema::SchemaExtractor::get_indexes(client, "main", &t.table_name)
                .await
                .unwrap();
            let foreign_keys =
                crate::schema::SchemaExtractor::get_foreign_keys(client, "main", &t.table_name)
                    .await
                    .unwrap();
            let triggers =
                crate::schema::SchemaExtractor::get_triggers(client, "main", &t.table_name)
                    .await
                    .unwrap();
            result.push(TableWithDetails {
                table_name: t.table_name,
                columns,
                indexes,
                foreign_keys,
                without_rowid: t.without_rowid,
                triggers,
            });
        }
        SchemaResponse {
            db_name: "main".to_string(),
            tables: result,
            views: crate::schema::SchemaExtractor::get_views(client, "main")
                .await
                .unwrap(),
        }
    }

    #[tokio::test]
    async fn introspects_columns_indexes_fks_triggers_and_without_rowid() {
        let (client, path) = temp_client("introspect").await;
        exec(
            &client,
            "CREATE TABLE parent (id INTEGER PRIMARY KEY, code TEXT NOT NULL UNIQUE)",
        )
        .await;
        exec(
            &client,
            "CREATE TABLE child (
                id INTEGER NOT NULL,
                parent_id INTEGER REFERENCES parent(id) ON DELETE CASCADE ON UPDATE SET NULL,
                name TEXT DEFAULT 'x',
                qty NUMERIC(10,2),
                payload BLOB,
                note TEXT,
                gen TEXT GENERATED ALWAYS AS (upper(name)) STORED,
                PRIMARY KEY (id, parent_id)
             ) WITHOUT ROWID",
        )
        .await;
        exec(
            &client,
            "CREATE INDEX idx_child_name ON child(name DESC) WHERE name IS NOT NULL",
        )
        .await;
        exec(
            &client,
            "CREATE INDEX idx_child_expr ON child(lower(name))",
        )
        .await;
        exec(
            &client,
            "CREATE TRIGGER trg_child AFTER INSERT ON child BEGIN UPDATE child SET note='t' WHERE id=NEW.id; END",
        )
        .await;
        exec(&client, "CREATE VIEW v_child AS SELECT id, name FROM child").await;

        let s = schema(&client).await;
        let child = s.tables.iter().find(|t| t.table_name == "child").unwrap();
        assert!(child.without_rowid);
        let name = child
            .columns
            .iter()
            .find(|c| c.column_name == "name")
            .unwrap();
        assert_eq!(name.data_type, "TEXT");
        assert_eq!(name.column_default.as_deref(), Some("'x'"));
        let qty = child.columns.iter().find(|c| c.column_name == "qty").unwrap();
        assert_eq!(qty.data_type, "NUMERIC");
        assert_eq!(qty.column_type, "NUMERIC(10,2)");
        let gen = child.columns.iter().find(|c| c.column_name == "gen").unwrap();
        assert_eq!(gen.generation_expression.as_deref(), Some("upper(name)"));
        assert_eq!(gen.extra, "STORED GENERATED");
        let pk: Vec<_> = child
            .columns
            .iter()
            .filter_map(|c| c.primary_key_ordinal.map(|o| (c.column_name.clone(), o)))
            .collect();
        assert_eq!(pk, vec![("id".to_string(), 1), ("parent_id".to_string(), 2)]);
        assert!(child
            .indexes
            .iter()
            .any(|i| i.index_name == "idx_child_name" && i.is_partial));
        assert!(child
            .indexes
            .iter()
            .any(|i| i.index_name == "idx_child_expr" && i.is_expression));
        assert_eq!(child.foreign_keys.len(), 1);
        assert_eq!(child.foreign_keys[0].delete_rule, "CASCADE");
        assert_eq!(child.foreign_keys[0].update_rule, "SET NULL");
        assert_eq!(child.triggers.len(), 1);
        assert!(child.triggers[0].sql.contains("CREATE TRIGGER"));
        assert!(s.views.iter().any(|v| v.table_name == "v_child"));

        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn add_column_rebuild_preserves_data_indexes_triggers_and_view() {
        let (client, path) = temp_client("rebuild").await;
        exec(
            &client,
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT NOT NULL)",
        )
        .await;
        exec(&client, "INSERT INTO t (a) VALUES ('one'),('two')").await;
        exec(&client, "CREATE INDEX idx_a ON t(a)").await;
        exec(
            &client,
            "CREATE TRIGGER trg_t AFTER INSERT ON t BEGIN UPDATE t SET a=NEW.a; END",
        )
        .await;
        exec(&client, "CREATE VIEW v_t AS SELECT id,a FROM t").await;

        let target = schema(&client).await;
        let mut source = target.clone();
        source
            .tables
            .iter_mut()
            .find(|t| t.table_name == "t")
            .unwrap()
            .columns
            .push(ColumnInfo {
                column_name: "b".to_string(),
                data_type: "TEXT".to_string(),
                column_type: "TEXT".to_string(),
                is_nullable: "YES".to_string(),
                column_comment: None,
                column_key: String::new(),
                column_default: Some("'d'".to_string()),
                extra: String::new(),
                type_warning: None,
                generation_expression: None,
                primary_key_ordinal: None,
            });

        let plan = SqliteSchemaSyncEngine::generate_ddl(&source, &target, None, false).unwrap();
        assert!(plan.destructive.is_empty());
        execute_plan(client.pool.sqlite().unwrap(), &plan)
            .await
            .unwrap();

        let after = schema(&client).await;
        let t = after.tables.iter().find(|t| t.table_name == "t").unwrap();
        assert!(t.columns.iter().any(|c| c.column_name == "b"));
        assert_eq!(
            t.indexes.iter().filter(|i| i.index_name == "idx_a").count(),
            1
        );
        assert_eq!(t.triggers.len(), 1);
        assert!(after.views.iter().any(|v| v.table_name == "v_t"));
        let rows = sqlx::query("SELECT id, a, b FROM t ORDER BY id")
            .fetch_all(client.pool.sqlite().unwrap())
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        let b: Option<String> = sqlx::Row::try_get(&rows[0], "b").unwrap();
        assert_eq!(b.as_deref(), Some("d"));

        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn drop_column_requires_confirmation() {
        let (client, path) = temp_client("confirm").await;
        exec(
            &client,
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b TEXT)",
        )
        .await;
        let target = schema(&client).await;
        let mut source = target.clone();
        source
            .tables
            .iter_mut()
            .find(|t| t.table_name == "t")
            .unwrap()
            .columns
            .retain(|c| c.column_name != "b");

        let err = SqliteSchemaSyncEngine::generate_ddl(&source, &target, None, false)
            .err()
            .expect("must require confirmation");
        match err {
            SqliteSyncError::RequiresConfirmation(items) => {
                assert!(items.iter().any(|i| i.kind == "drop_column"));
            }
            other => panic!("unexpected: {:?}", other),
        }
        let plan = SqliteSchemaSyncEngine::generate_ddl(&source, &target, None, true).unwrap();
        assert!(plan.statements.iter().any(|s| s.contains("CREATE TABLE")));
        assert!(plan.statements.iter().any(|s| s.contains("DROP TABLE")));
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn nullable_default_type_and_primary_key_changes_use_rebuild() {
        let (client, path) = temp_client("column_changes").await;
        exec(
            &client,
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT NOT NULL DEFAULT 'x')",
        )
        .await;
        exec(&client, "INSERT INTO t (a) VALUES ('v')").await;
        let target = schema(&client).await;
        let mut source = target.clone();
        let t = source.tables.iter_mut().find(|t| t.table_name == "t").unwrap();
        let a = t.columns.iter_mut().find(|c| c.column_name == "a").unwrap();
        a.is_nullable = "YES".to_string();
        a.column_default = Some("'y'".to_string());
        t.columns
            .iter_mut()
            .find(|c| c.column_name == "id")
            .unwrap()
            .column_type = "BIGINT".to_string();

        let err = SqliteSchemaSyncEngine::generate_ddl(&source, &target, None, false)
            .err()
            .expect("type change must require confirmation");
        match err {
            SqliteSyncError::RequiresConfirmation(items) => {
                assert!(items.iter().any(|i| i.kind == "narrow_type"));
            }
            other => panic!("unexpected: {:?}", other),
        }
        let plan = SqliteSchemaSyncEngine::generate_ddl(&source, &target, None, true).unwrap();
        execute_plan(client.pool.sqlite().unwrap(), &plan)
            .await
            .unwrap();

        let after = schema(&client).await;
        let t = after.tables.iter().find(|t| t.table_name == "t").unwrap();
        let a = t.columns.iter().find(|c| c.column_name == "a").unwrap();
        assert_eq!(a.is_nullable, "YES");
        assert_eq!(a.column_default.as_deref(), Some("'y'"));
        assert_eq!(
            t.columns
                .iter()
                .find(|c| c.column_name == "id")
                .unwrap()
                .column_type,
            "BIGINT"
        );
        let rows = sqlx::query("SELECT a FROM t")
            .fetch_all(client.pool.sqlite().unwrap())
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);

        // 主键变更：把主键从 id 改为 a。
        let mut source2 = after.clone();
        let t2 = source2
            .tables
            .iter_mut()
            .find(|t| t.table_name == "t")
            .unwrap();
        for col in &mut t2.columns {
            col.primary_key_ordinal = if col.column_name == "a" {
                Some(1)
            } else {
                None
            };
        }
        let err = SqliteSchemaSyncEngine::generate_ddl(&source2, &after, None, false)
            .err()
            .expect("primary key change must require confirmation");
        match err {
            SqliteSyncError::RequiresConfirmation(items) => {
                assert!(items.iter().any(|i| i.kind == "modify_primary_key"));
            }
            other => panic!("unexpected: {:?}", other),
        }
        let plan = SqliteSchemaSyncEngine::generate_ddl(&source2, &after, None, true).unwrap();
        execute_plan(client.pool.sqlite().unwrap(), &plan)
            .await
            .unwrap();
        let t = &schema(&client).await.tables[0];
        let pk: Vec<_> = t
            .columns
            .iter()
            .filter_map(|c| c.primary_key_ordinal.map(|o| (c.column_name.clone(), o)))
            .collect();
        assert_eq!(pk, vec![("a".to_string(), 1)]);
        let _ = std::fs::remove_file(path);
    }

    fn find_index<'a>(
        schema: &'a SchemaResponse,
        table: &str,
        index: &str,
    ) -> Vec<&'a IndexInfo> {
        let mut rows: Vec<&IndexInfo> = schema
            .tables
            .iter()
            .find(|t| t.table_name == table)
            .map(|t| {
                t.indexes
                    .iter()
                    .filter(|i| i.index_name == index)
                    .collect()
            })
            .unwrap_or_default();
        rows.sort_by_key(|r| r.seq_in_index);
        rows
    }

    #[tokio::test]
    async fn expression_and_partial_index_diffs_are_detected() {
        let (src, src_path) = temp_client("expr_diff_src").await;
        exec(
            &src,
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b TEXT)",
        )
        .await;
        exec(&src, "CREATE INDEX idx_expr ON t (lower(a))").await;
        exec(
            &src,
            "CREATE INDEX idx_partial ON t (a) WHERE b IS NOT NULL",
        )
        .await;

        // 完全等价（仅大小写/空白/多余括号不同）不应产生 diff。
        let (equal, equal_path) = temp_client("expr_diff_equal").await;
        exec(
            &equal,
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b TEXT)",
        )
        .await;
        exec(&equal, "CREATE INDEX idx_expr ON t ( LOWER( a ) )").await;
        exec(
            &equal,
            "CREATE INDEX idx_partial ON t (a) WHERE (b IS NOT NULL)",
        )
        .await;

        // 仅谓词不同。
        let (pred, pred_path) = temp_client("expr_diff_pred").await;
        exec(
            &pred,
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b TEXT)",
        )
        .await;
        exec(&pred, "CREATE INDEX idx_expr ON t (lower(a))").await;
        exec(
            &pred,
            "CREATE INDEX idx_partial ON t (a) WHERE b IS NOT NULL AND a <> ''",
        )
        .await;

        // 仅表达式不同。
        let (expr, expr_path) = temp_client("expr_diff_expr").await;
        exec(
            &expr,
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b TEXT)",
        )
        .await;
        exec(&expr, "CREATE INDEX idx_expr ON t (upper(a))").await;
        exec(
            &expr,
            "CREATE INDEX idx_partial ON t (a) WHERE b IS NOT NULL",
        )
        .await;

        // 表达式与谓词都不同。
        let (both, both_path) = temp_client("expr_diff_both").await;
        exec(
            &both,
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b TEXT)",
        )
        .await;
        exec(&both, "CREATE INDEX idx_expr ON t (upper(a))").await;
        exec(
            &both,
            "CREATE INDEX idx_partial ON t (a) WHERE a <> ''",
        )
        .await;

        let source = schema(&src).await;
        for (client, expect_diff) in [
            (&equal, false),
            (&pred, true),
            (&expr, true),
            (&both, true),
        ] {
            let target = schema(client).await;
            let result = SqliteSchemaSyncEngine::compare(&source, &target);
            let table = result
                .diff
                .tables
                .iter()
                .find(|t| t.table_name == "t")
                .expect("table t");
            assert_eq!(
                table.status,
                if expect_diff { "modified" } else { "unchanged" },
                "status mismatch for expect_diff={expect_diff}"
            );
            assert_eq!(
                !table.indexes_modified.is_empty(),
                expect_diff,
                "index diff mismatch for expect_diff={expect_diff}"
            );
            assert!(
                result.unsupported.is_empty(),
                "表达式/部分索引应可真同步，不应报 unsupported: {:?}",
                result.unsupported
            );
        }

        for path in [
            src_path,
            equal_path,
            pred_path,
            expr_path,
            both_path,
        ] {
            let _ = std::fs::remove_file(path);
        }
    }

    #[tokio::test]
    async fn expression_and_partial_indexes_rebuild_and_recover_shape() {
        let (src, src_path) = temp_client("expr_rebuild_src").await;
        exec(
            &src,
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b TEXT)",
        )
        .await;
        exec(&src, "CREATE UNIQUE INDEX idx_expr ON t (lower(a) DESC, b)").await;
        exec(
            &src,
            "CREATE INDEX idx_partial ON t (lower(b) ASC) WHERE a IS NOT NULL",
        )
        .await;

        let (tgt, tgt_path) = temp_client("expr_rebuild_tgt").await;
        exec(
            &tgt,
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b TEXT, legacy TEXT)",
        )
        .await;
        exec(&tgt, "INSERT INTO t (id, a, b, legacy) VALUES (1, 'A', 'B', 'x')").await;
        exec(&tgt, "CREATE INDEX idx_expr ON t (b)").await;

        let source = schema(&src).await;
        let target = schema(&tgt).await;
        let plan = SqliteSchemaSyncEngine::generate_ddl(&source, &target, None, true)
            .expect("plan");
        assert!(plan.unsupported.is_empty(), "{:?}", plan.unsupported);
        assert!(
            plan.statements
                .iter()
                .any(|s| s.contains("__sqlite_sync_new_t")),
            "应走表重建以移除 legacy 列: {:?}",
            plan.statements
        );
        execute_plan(tgt.pool.sqlite().unwrap(), &plan)
            .await
            .expect("execute plan");

        let after = schema(&tgt).await;
        let columns: Vec<_> = after.tables[0]
            .columns
            .iter()
            .map(|c| c.column_name.as_str())
            .collect();
        assert_eq!(columns, vec!["id", "a", "b"]);

        for index in ["idx_expr", "idx_partial"] {
            let src_rows: Vec<IndexInfo> =
                find_index(&source, "t", index).into_iter().cloned().collect();
            let tgt_rows: Vec<IndexInfo> =
                find_index(&after, "t", index).into_iter().cloned().collect();
            assert!(index_rows_equal(&src_rows, &tgt_rows), "{index} 形态不一致");
        }
        let expr = find_index(&after, "t", "idx_expr");
        assert!(expr[0].expression.as_deref().unwrap_or("").contains("lower"));
        assert_eq!(expr[0].sort_order, "DESC");

        let partial = find_index(&after, "t", "idx_partial");
        assert!(partial[0]
            .predicate
            .as_deref()
            .unwrap_or("")
            .to_ascii_lowercase()
            .contains("a is not null"));

        let sql: String = sqlx::query_scalar(
            "SELECT sql FROM sqlite_master WHERE type='index' AND name='idx_partial'",
        )
        .fetch_one(tgt.pool.sqlite().unwrap())
        .await
        .unwrap();
        assert!(sql.to_ascii_uppercase().contains("WHERE"));

        let violations: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM pragma_foreign_key_check")
                .fetch_one(tgt.pool.sqlite().unwrap())
                .await
                .unwrap();
        assert_eq!(violations, 0);

        let _ = std::fs::remove_file(src_path);
        let _ = std::fs::remove_file(tgt_path);
    }

    #[tokio::test]
    async fn without_rowid_and_trigger_recovery() {
        let (client, path) = temp_client("without_rowid").await;
        exec(
            &client,
            "CREATE TABLE t (id TEXT PRIMARY KEY, a TEXT) WITHOUT ROWID",
        )
        .await;
        exec(&client, "INSERT INTO t VALUES ('k','v')").await;
        exec(
            &client,
            "CREATE TRIGGER trg_t AFTER UPDATE ON t BEGIN SELECT 1; END",
        )
        .await;
        let source = schema(&client).await;
        let mut target = source.clone();
        target.tables[0]
            .columns
            .iter_mut()
            .find(|c| c.column_name == "a")
            .unwrap()
            .column_default = Some("'v'".to_string());

        let plan = SqliteSchemaSyncEngine::generate_ddl(&source, &target, None, true).unwrap();
        execute_plan(client.pool.sqlite().unwrap(), &plan)
            .await
            .unwrap();
        let after = schema(&client).await;
        let t = &after.tables[0];
        assert!(t.without_rowid);
        assert_eq!(t.triggers.len(), 1);
        assert_eq!(
            sqlx::query("SELECT id FROM t")
                .fetch_all(client.pool.sqlite().unwrap())
                .await
                .unwrap()
                .len(),
            1
        );
        let _ = std::fs::remove_file(path);
    }

    /// 父表 p + 子表 t（t.pid -> p.id）；with_orphan 时插入 pid=99 的孤儿行。
    async fn setup_fk_fixture(client: &DbClient, with_orphan: bool) {
        let extra_row = if with_orphan { ", (2, 99)" } else { ", (2, 1)" };
        let sql = format!(
            "PRAGMA foreign_keys = OFF;
             CREATE TABLE p (id INTEGER PRIMARY KEY, label TEXT);
             CREATE TABLE t (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES p(id));
             INSERT INTO p VALUES (1, 'ok');
             INSERT INTO t VALUES (1, 1){extra_row};"
        );
        // 单条连接执行整段，确保 PRAGMA 与插入落在同一连接上。
        sqlx::raw_sql(&sql)
            .execute(client.pool.sqlite().unwrap())
            .await
            .expect("setup fk fixture");
    }

    /// 给源结构加一列 note，触发 t 的表重建。
    fn add_note_column(source: &mut SchemaResponse) {
        let t = source
            .tables
            .iter_mut()
            .find(|t| t.table_name == "t")
            .unwrap();
        let mut note = t
            .columns
            .iter()
            .find(|c| c.column_name == "pid")
            .unwrap()
            .clone();
        note.column_name = "note".to_string();
        note.data_type = "TEXT".to_string();
        note.column_type = "TEXT".to_string();
        note.is_nullable = "YES".to_string();
        note.column_comment = None;
        note.column_key = String::new();
        note.column_default = None;
        note.extra = String::new();
        note.type_warning = None;
        note.generation_expression = None;
        note.primary_key_ordinal = None;
        t.columns.push(note);
    }

    /// 单连接池，便于验证连接级 PRAGMA 的恢复；进入时 foreign_keys=1。
    async fn single_conn_pool(path: &str) -> SqlitePool {
        let url = format!("sqlite://{}?mode=rwc", path);
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .expect("open single-connection sqlite pool");
        let mut conn = pool.acquire().await.expect("acquire sqlite connection");
        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(&mut *conn)
            .await
            .expect("enable foreign_keys for test connection");
        pool
    }

    #[tokio::test]
    async fn fk_check_rejects_orphans_rolls_back_and_restores_pragmas() {
        let (client, path) = temp_client("fk_orphan").await;
        setup_fk_fixture(&client, true).await;

        let target = schema(&client).await;
        let mut source = target.clone();
        add_note_column(&mut source);
        let plan = SqliteSchemaSyncEngine::generate_ddl(&source, &target, None, true).unwrap();
        assert!(
            plan.check_tables.iter().any(|t| t == "t"),
            "重建表必须进入外键校验清单: {:?}",
            plan.check_tables
        );

        let pool = single_conn_pool(&path).await;
        let err = execute_script(&pool, &plan.script())
            .await
            .expect_err("孤儿行必须让表重建失败");
        let message = err.to_string();
        assert!(message.contains("外键校验失败"), "{message}");
        assert!(message.contains("子表 \"t\""), "{message}");
        assert!(message.contains("父表 \"p\""), "{message}");
        assert!(message.contains("rowid=2"), "{message}");

        // 回滚后原表结构不变：仍是 id/pid 两列，没有 note。
        let columns = sqlx::query("PRAGMA table_info(t)")
            .fetch_all(&pool)
            .await
            .unwrap();
        let names: Vec<String> = columns
            .iter()
            .map(|row| row.try_get::<String, _>("name").unwrap())
            .collect();
        assert_eq!(names, vec!["id".to_string(), "pid".to_string()]);

        // 原数据不变，孤儿行仍在（没有半成品新表替换原表）。
        let rows = sqlx::query("SELECT id, pid FROM t ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].try_get::<i64, _>("pid").unwrap(), 99);

        // 临时新表已随事务回滚，没有残留。
        let leftovers: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE name LIKE '__sqlite_sync_new_t%'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(leftovers, 0);

        // 同一连接上的 foreign_keys 已恢复为进入前的 1。
        let mut conn = pool.acquire().await.unwrap();
        let fk: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        assert_eq!(fk, 1);

        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn fk_check_passes_when_rebuild_has_no_orphans() {
        let (client, path) = temp_client("fk_clean").await;
        setup_fk_fixture(&client, false).await;

        let target = schema(&client).await;
        let mut source = target.clone();
        add_note_column(&mut source);
        let plan = SqliteSchemaSyncEngine::generate_ddl(&source, &target, None, true).unwrap();

        let pool = single_conn_pool(&path).await;
        execute_script(&pool, &plan.script())
            .await
            .expect("无孤儿行时重建应成功");

        // 新列已生效，数据完整。
        let columns = sqlx::query("PRAGMA table_info(t)")
            .fetch_all(&pool)
            .await
            .unwrap();
        let names: Vec<String> = columns
            .iter()
            .map(|row| row.try_get::<String, _>("name").unwrap())
            .collect();
        assert!(names.contains(&"note".to_string()), "{names:?}");
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM t")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 2);

        // 事务结束后全库外键校验为空。
        let violations = sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert!(
            violations.is_empty(),
            "重建成功后不应有外键违规，实际 {} 条",
            violations.len()
        );

        // PRAGMA 同样恢复为进入前的 1。
        let mut conn = pool.acquire().await.unwrap();
        let fk: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        assert_eq!(fk, 1);

        let _ = std::fs::remove_file(path);
    }
}
