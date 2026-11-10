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
//!   CREATE INDEX ... / CREATE TRIGGER ...
//! COMMIT;
//! PRAGMA legacy_alter_table = OFF;
//! PRAGMA foreign_keys = ON;
//! ```
//!
//! SQLite 的 DDL 是事务性的，任何一步失败都会回滚到重建前的旧表；
//! 脚本末尾的 PRAGMA 恢复在失败路径由执行器兜底。
//!
//! 表达式索引/部分索引无法在 `IndexInfo` 里完整表达：生成计划时进入
//! `unsupported` 并跳过该表的重建，避免生成一张结构被削弱的表。

use crate::db::DbError;
use crate::pg_schema_sync::DestructiveChange;
use crate::schema::{ColumnInfo, IndexInfo, SchemaResponse, TableWithDetails};
use crate::tools::{SchemaDiff, SyncEngine};
use sqlx::{Row, SqlitePool};

pub const REBUILD_MARKER: &str = "-- sqlite-rebuild-plan";

#[derive(Debug, Clone, Default)]
pub struct SqliteDdlPlan {
    /// 事务内语句（不含 PRAGMA/BEGIN/COMMIT）。
    pub statements: Vec<String>,
    pub destructive: Vec<DestructiveChange>,
    pub unsupported: Vec<String>,
}

impl SqliteDdlPlan {
    pub fn script(&self) -> String {
        let mut out = String::new();
        out.push_str(REBUILD_MARKER);
        out.push('\n');
        out.push_str("PRAGMA foreign_keys = OFF;\n");
        out.push_str("PRAGMA legacy_alter_table = ON;\n");
        out.push_str("BEGIN;\n");
        for stmt in &self.statements {
            out.push_str(stmt.trim().trim_end_matches(';'));
            out.push_str(";\n");
        }
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
    pub unsupported: Vec<String>,
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

            if let Some(tgt) = tgt {
                if tgt.without_rowid != src.without_rowid {
                    table_diff.status = "modified".to_string();
                }
                if !same_triggers(&src.triggers, &tgt.triggers) {
                    table_diff.status = "modified".to_string();
                }
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

            // 只有表确实需要变更时才报告无法自动同步的索引；
            // 完全一致的表不应因为存在表达式索引而反复告警。
            if table_diff.status != "unchanged" {
                for idx in src
                    .indexes
                    .iter()
                    .chain(tgt.map(|t| t.indexes.as_slice()).unwrap_or(&[]))
                {
                    if idx.is_expression || idx.is_partial {
                        let reason = if idx.is_partial {
                            "部分索引"
                        } else {
                            "表达式索引"
                        };
                        unsupported.push(format!(
                            "{}.{}: {}无法自动同步，原始 SQL: {}",
                            table_diff.table_name,
                            idx.index_name,
                            reason,
                            idx.definition.as_deref().unwrap_or("<not available>")
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
                    let reasons = unsupported_index_reasons(src);
                    if !reasons.is_empty() {
                        plan.unsupported.extend(reasons);
                        continue;
                    }
                    plan.statements
                        .push(create_table_sql(src, &src.table_name));
                    push_index_statements(&mut plan, src);
                    push_trigger_statements(&mut plan, src, &[]);
                }
                "modified" => {
                    let reasons = unsupported_index_reasons(src);
                    if !reasons.is_empty() {
                        plan.unsupported.extend(reasons);
                        continue;
                    }
                    let Some(tgt) = tgt else {
                        continue;
                    };
                    let mut table_statements = Vec::new();
                    if let Err(reason) = rebuild_table_sql(src, tgt, &mut table_statements) {
                        plan.unsupported.push(reason);
                        continue;
                    }
                    plan.statements.extend(table_statements);
                    push_index_statements(&mut plan, src);
                    // DROP TABLE 会一并删除触发器；重建后必须无条件恢复，
                    // 不能因为源/目标定义相同而跳过。
                    push_trigger_statements(&mut plan, src, &[]);
                }
                _ => {
                    // 表本身没变，但触发器可能不同。
                    if let Some(tgt) = tgt {
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

fn unsupported_index_reasons(table: &TableWithDetails) -> Vec<String> {
    table
        .indexes
        .iter()
        .filter(|idx| {
            idx.is_expression
                || idx.is_partial
                || (!idx.is_primary
                    && !idx.index_name.starts_with("sqlite_autoindex_")
                    && idx.definition.is_none())
        })
        .map(|idx| {
            let reason = if idx.is_partial {
                "部分索引"
            } else if idx.is_expression {
                "表达式索引"
            } else {
                "索引缺少原始 SQL"
            };
            format!(
                "{}.{}: {}无法在表重建中自动恢复，原始 SQL: {}",
                table.table_name,
                idx.index_name,
                reason,
                idx.definition.as_deref().unwrap_or("<not available>")
            )
        })
        .collect()
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
    for idx in &table.indexes {
        if idx.is_primary || idx.index_name.starts_with("sqlite_autoindex_") {
            continue;
        }
        if idx.is_expression || idx.is_partial {
            continue; // 已在 unsupported 中报告
        }
        let Some(definition) = &idx.definition else {
            plan.unsupported.push(format!(
                "{}.{}: 缺少索引原始 SQL，无法在重建后恢复",
                table.table_name, idx.index_name
            ));
            continue;
        };
        plan.statements.push(definition.clone());
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
            plan.unsupported.push(format!(
                "视图 {} 缺少原始定义，无法同步",
                src.table_name
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
/// `PRAGMA foreign_keys` 是连接级开关，事务内设置无效：脚本在 `BEGIN`
/// 之前关闭、`COMMIT` 之后恢复；这里额外兜底记录原值，失败时也恢复，
/// 避免把连接留在 `foreign_keys=OFF` 的状态。
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
        // PRAGMA 是连接级状态，必须在同一条连接上读取并恢复，
        // 否则可能读到池里另一条连接的默认值。
        let mut conn = pool.acquire().await?;
        let original_fk = pragma_int(&mut conn, "foreign_keys").await?;
        let original_legacy = pragma_int(&mut conn, "legacy_alter_table").await?;

        use sqlx::Executor;
        let result = Executor::execute(&mut *conn, sqlx::raw_sql(script)).await;
        if result.is_err() {
            // 脚本可能停在事务中间；回滚后再恢复 PRAGMA。
            let _ = Executor::execute(&mut *conn, sqlx::raw_sql("ROLLBACK")).await;
        }
        let legacy_sql = format!("PRAGMA legacy_alter_table = {}", original_legacy);
        let _ = Executor::execute(&mut *conn, sqlx::raw_sql(&legacy_sql)).await;
        let fk_sql = format!("PRAGMA foreign_keys = {}", original_fk);
        let _ = Executor::execute(&mut *conn, sqlx::raw_sql(&fk_sql)).await;
        result.map(|_| ()).map_err(DbError::from)
    }
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

    #[tokio::test]
    async fn expression_and_partial_indexes_are_reported_unsupported() {
        let (client, path) = temp_client("unsupported").await;
        exec(&client, "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT)").await;
        exec(&client, "CREATE INDEX idx_expr ON t(lower(a))").await;
        exec(
            &client,
            "CREATE INDEX idx_partial ON t(a) WHERE a IS NOT NULL",
        )
        .await;
        let source = schema(&client).await;
        let mut target = source.clone();
        target.tables.clear();

        let plan = SqliteSchemaSyncEngine::generate_ddl(&source, &target, None, true).unwrap();
        assert!(plan.unsupported.iter().any(|u| u.contains("表达式索引")));
        assert!(plan.unsupported.iter().any(|u| u.contains("部分索引")));
        assert!(plan.unsupported.iter().any(|u| u.contains("idx_expr")));
        assert!(!plan
            .statements
            .iter()
            .any(|s| s.contains("CREATE TABLE") && s.contains("\"t\"")));
        let _ = std::fs::remove_file(path);
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
}
