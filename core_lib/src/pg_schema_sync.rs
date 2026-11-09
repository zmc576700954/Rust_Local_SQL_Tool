//! PostgreSQL 结构对比与 DDL 生成。
//!
//! 这里刻意不复用 MySQL 的 `DdlEngine`：PostgreSQL 的 schema 归属、索引方向、
//! 生成列、外键约束与 MySQL 语义不同，硬塞进 MySQL 形状会丢失信息。
//! 输出仍然映射到共享的 `SchemaDiff` 供前端展示，但 DDL 由本模块独立生成。

use crate::schema::{ColumnInfo, IndexInfo, SchemaResponse, TableWithDetails};
use crate::schema_ext::ForeignKeyInfo;
use crate::sql_util::split_pg_table_ref;
use crate::tools::{ColumnDiff, FkDiff, IndexDiff, SchemaDiff, TableDiff};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// 破坏性差异类别。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DestructiveChange {
    pub table_name: String,
    /// drop_table / drop_column / narrow_type / drop_index / modify_primary_key /
    /// rewrite_generated_column
    pub kind: String,
    pub detail: String,
}

#[derive(Debug, Clone, Default)]
pub struct PgDdlPlan {
    pub statements: Vec<String>,
    pub destructive: Vec<DestructiveChange>,
    /// 当前无法自动处理的差异（例如表达式索引），会在界面上单独提示。
    pub unsupported: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct PgSyncResult {
    pub diff: SchemaDiff,
    pub destructive: Vec<DestructiveChange>,
    pub unsupported: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct PgRequiresConfirmation {
    pub destructive: Vec<DestructiveChange>,
}

pub struct PgSchemaSyncEngine;

impl PgSchemaSyncEngine {
    /// 只读对比：返回供界面展示的差异，以及破坏性/暂不支持的条目。
    pub fn compare(source: &SchemaResponse, target: &SchemaResponse) -> PgSyncResult {
        let comparisons = compute_comparisons(source, target);
        let diff = to_schema_diff(&comparisons);
        let plan = build_plan(&comparisons, None);
        PgSyncResult {
            diff,
            destructive: plan.destructive,
            unsupported: plan.unsupported,
        }
    }

    /// 为选中的表生成 DDL。
    ///
    /// 存在破坏性差异且 `confirm_destructive == false` 时返回
    /// `PgRequiresConfirmation`，调用方必须拒绝请求，且不能执行任何 DDL。
    pub fn generate_ddl(
        source: &SchemaResponse,
        target: &SchemaResponse,
        selected_tables: &[String],
        confirm_destructive: bool,
    ) -> Result<PgDdlPlan, PgRequiresConfirmation> {
        let comparisons = compute_comparisons(source, target);
        let plan = build_plan(&comparisons, Some(selected_tables));
        if !confirm_destructive && !plan.destructive.is_empty() {
            return Err(PgRequiresConfirmation {
                destructive: plan.destructive,
            });
        }
        Ok(plan)
    }
}

// ── 内部结构 ────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
struct IndexColumn {
    name: String,
    sort: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PgIndex {
    name: String,
    unique: bool,
    primary: bool,
    has_expression: bool,
    columns: Vec<IndexColumn>,
}

impl PgIndex {
    fn column_list(&self) -> String {
        self.columns
            .iter()
            .map(|c| format!("{} {}", quote_ident(&c.name), c.sort))
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn plain_column_list(&self) -> String {
        self.columns
            .iter()
            .map(|c| quote_ident(&c.name))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PgForeignKey {
    name: String,
    columns: Vec<String>,
    referenced_table: String,
    referenced_columns: Vec<String>,
    update_rule: String,
    delete_rule: String,
}

#[derive(Debug, Clone, Default)]
struct TableComparison {
    table_name: String,
    status: &'static str,
    columns_added: Vec<ColumnInfo>,
    columns_removed: Vec<ColumnInfo>,
    columns_modified: Vec<(ColumnInfo, ColumnInfo)>, // (old, new)
    indexes_added: Vec<PgIndex>,
    indexes_removed: Vec<PgIndex>,
    indexes_modified: Vec<(PgIndex, PgIndex)>, // (old, new)
    fks_added: Vec<PgForeignKey>,
    fks_removed: Vec<PgForeignKey>,
    fks_modified: Vec<(PgForeignKey, PgForeignKey)>,
}

// ── 差异计算 ────────────────────────────────────────────

fn compute_comparisons(source: &SchemaResponse, target: &SchemaResponse) -> Vec<TableComparison> {
    let mut comparisons = Vec::new();
    let mut seen = BTreeSet::new();

    for src in &source.tables {
        seen.insert(src.table_name.clone());
        match target
            .tables
            .iter()
            .find(|t| t.table_name == src.table_name)
        {
            Some(tgt) => comparisons.push(compare_table(src, tgt)),
            None => {
                let mut cmp = TableComparison {
                    table_name: src.table_name.clone(),
                    status: "added",
                    columns_added: src.columns.clone(),
                    indexes_added: group_indexes(src),
                    fks_added: group_fks(src),
                    ..Default::default()
                };
                cmp.status = "added";
                comparisons.push(cmp);
            }
        }
    }

    for tgt in &target.tables {
        if seen.contains(&tgt.table_name) {
            continue;
        }
        let mut cmp = TableComparison {
            table_name: tgt.table_name.clone(),
            status: "removed",
            columns_removed: tgt.columns.clone(),
            indexes_removed: group_indexes(tgt),
            fks_removed: group_fks(tgt),
            ..Default::default()
        };
        cmp.status = "removed";
        comparisons.push(cmp);
    }
    comparisons
}

fn compare_table(src: &TableWithDetails, tgt: &TableWithDetails) -> TableComparison {
    let mut cmp = TableComparison {
        table_name: src.table_name.clone(),
        status: "unchanged",
        ..Default::default()
    };

    for src_col in &src.columns {
        match tgt
            .columns
            .iter()
            .find(|c| c.column_name == src_col.column_name)
        {
            Some(tgt_col) => {
                if !columns_equal(tgt_col, src_col) {
                    cmp.columns_modified.push((tgt_col.clone(), src_col.clone()));
                }
            }
            None => cmp.columns_added.push(src_col.clone()),
        }
    }
    for tgt_col in &tgt.columns {
        if !src
            .columns
            .iter()
            .any(|c| c.column_name == tgt_col.column_name)
        {
            cmp.columns_removed.push(tgt_col.clone());
        }
    }

    let src_indexes = group_indexes(src);
    let tgt_indexes = group_indexes(tgt);
    for src_idx in &src_indexes {
        match tgt_indexes.iter().find(|i| i.name == src_idx.name) {
            Some(tgt_idx) => {
                if tgt_idx != src_idx {
                    cmp.indexes_modified.push((tgt_idx.clone(), src_idx.clone()));
                }
            }
            None => cmp.indexes_added.push(src_idx.clone()),
        }
    }
    for tgt_idx in &tgt_indexes {
        if !src_indexes.iter().any(|i| i.name == tgt_idx.name) {
            cmp.indexes_removed.push(tgt_idx.clone());
        }
    }

    let src_fks = group_fks(src);
    let tgt_fks = group_fks(tgt);
    for src_fk in &src_fks {
        match tgt_fks.iter().find(|f| f.name == src_fk.name) {
            Some(tgt_fk) => {
                if tgt_fk != src_fk {
                    cmp.fks_modified.push((tgt_fk.clone(), src_fk.clone()));
                }
            }
            None => cmp.fks_added.push(src_fk.clone()),
        }
    }
    for tgt_fk in &tgt_fks {
        if !src_fks.iter().any(|f| f.name == tgt_fk.name) {
            cmp.fks_removed.push(tgt_fk.clone());
        }
    }

    if !cmp.columns_added.is_empty()
        || !cmp.columns_removed.is_empty()
        || !cmp.columns_modified.is_empty()
        || !cmp.indexes_added.is_empty()
        || !cmp.indexes_removed.is_empty()
        || !cmp.indexes_modified.is_empty()
        || !cmp.fks_added.is_empty()
        || !cmp.fks_removed.is_empty()
        || !cmp.fks_modified.is_empty()
    {
        cmp.status = "modified";
    }
    cmp
}

fn group_indexes(table: &TableWithDetails) -> Vec<PgIndex> {
    let mut grouped: BTreeMap<String, Vec<&IndexInfo>> = BTreeMap::new();
    for idx in &table.indexes {
        grouped
            .entry(idx.index_name.clone())
            .or_default()
            .push(idx);
    }
    grouped
        .into_iter()
        .map(|(name, mut rows)| {
            rows.sort_by_key(|r| r.seq_in_index);
            let first = rows[0];
            PgIndex {
                name,
                unique: !first.non_unique,
                primary: rows.iter().any(|r| r.is_primary),
                has_expression: rows
                    .iter()
                    .any(|r| r.is_expression || r.column_name.is_empty()),
                columns: rows
                    .iter()
                    .map(|r| IndexColumn {
                        name: r.column_name.clone(),
                        sort: if r.sort_order.eq_ignore_ascii_case("DESC") {
                            "DESC".to_string()
                        } else {
                            "ASC".to_string()
                        },
                    })
                    .collect(),
            }
        })
        .collect()
}

fn group_fks(table: &TableWithDetails) -> Vec<PgForeignKey> {
    let mut grouped: BTreeMap<String, Vec<&ForeignKeyInfo>> = BTreeMap::new();
    for fk in &table.foreign_keys {
        grouped.entry(fk.constraint_name.clone()).or_default().push(fk);
    }
    grouped
        .into_iter()
        .map(|(name, mut rows)| {
            rows.sort_by_key(|r| r.ordinal_position);
            let first = rows[0];
            PgForeignKey {
                name,
                columns: rows.iter().map(|r| r.column_name.clone()).collect(),
                referenced_table: first.referenced_table_name.clone(),
                referenced_columns: rows
                    .iter()
                    .map(|r| r.referenced_column_name.clone())
                    .collect(),
                update_rule: first.update_rule.clone(),
                delete_rule: first.delete_rule.clone(),
            }
        })
        .collect()
}

fn columns_equal(a: &ColumnInfo, b: &ColumnInfo) -> bool {
    a.column_type == b.column_type
        && a.is_nullable == b.is_nullable
        && effective_default(a) == effective_default(b)
        && normalized_generation(a) == normalized_generation(b)
}

fn normalized_generation(col: &ColumnInfo) -> Option<String> {
    col.generation_expression
        .as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn is_auto_increment(col: &ColumnInfo) -> bool {
    let extra = col.extra.to_lowercase();
    extra.contains("identity")
        || extra.contains("auto_increment")
        || col
            .column_default
            .as_deref()
            .map(|d| d.starts_with("nextval("))
            .unwrap_or(false)
}

/// 自增/identity 与 serial(nextval) 在结构对比中视为等价，避免无意义差异。
fn effective_default(col: &ColumnInfo) -> Option<String> {
    if normalized_generation(col).is_some() || is_auto_increment(col) {
        return None;
    }
    col.column_default
        .as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

// ── SchemaDiff 映射（供前端展示） ───────────────────────

fn to_schema_diff(comparisons: &[TableComparison]) -> SchemaDiff {
    let tables = comparisons
        .iter()
        .map(|cmp| {
            let mut diff = TableDiff {
                table_name: cmp.table_name.clone(),
                status: cmp.status.to_string(),
                columns_added: cmp.columns_added.clone(),
                columns_removed: cmp.columns_removed.clone(),
                columns_modified: cmp
                    .columns_modified
                    .iter()
                    .map(|(old, new)| ColumnDiff {
                        old: old.clone(),
                        new: new.clone(),
                    })
                    .collect(),
                indexes_added: expand_indexes(&cmp.indexes_added),
                indexes_removed: expand_indexes(&cmp.indexes_removed),
                indexes_modified: expand_modified_indexes(&cmp.indexes_modified),
                fks_added: expand_fks(&cmp.fks_added),
                fks_removed: expand_fks(&cmp.fks_removed),
                fks_modified: cmp
                    .fks_modified
                    .iter()
                    .filter_map(|(old, new)| {
                        Some(FkDiff {
                            old: expand_fk(old).into_iter().next()?,
                            new: expand_fk(new).into_iter().next()?,
                        })
                    })
                    .collect(),
            };
            if diff.status == "modified"
                && diff.columns_added.is_empty()
                && diff.columns_removed.is_empty()
                && diff.columns_modified.is_empty()
                && diff.indexes_added.is_empty()
                && diff.indexes_removed.is_empty()
                && diff.indexes_modified.is_empty()
                && diff.fks_added.is_empty()
                && diff.fks_removed.is_empty()
                && diff.fks_modified.is_empty()
            {
                diff.status = "unchanged".to_string();
            }
            diff
        })
        .collect();
    SchemaDiff { tables }
}

fn expand_indexes(indexes: &[PgIndex]) -> Vec<IndexInfo> {
    indexes.iter().flat_map(expand_index).collect()
}

fn expand_index(idx: &PgIndex) -> Vec<IndexInfo> {
    idx.columns
        .iter()
        .enumerate()
        .map(|(i, col)| IndexInfo {
            index_name: idx.name.clone(),
            column_name: col.name.clone(),
            non_unique: !idx.unique,
            index_type: "btree".to_string(),
            seq_in_index: (i + 1) as u32,
            sort_order: col.sort.clone(),
            is_primary: idx.primary,
            is_expression: idx.has_expression,
        })
        .collect()
}

fn expand_modified_indexes(pairs: &[(PgIndex, PgIndex)]) -> Vec<IndexDiff> {
    pairs
        .iter()
        .flat_map(|(old, new)| {
            let old_rows = expand_index(old);
            let new_rows = expand_index(new);
            let count = old_rows.len().max(new_rows.len()).max(1);
            let mut result = Vec::new();
            for i in 0..count {
                let old_row = old_rows
                    .get(i)
                    .cloned()
                    .or_else(|| old_rows.first().cloned());
                let new_row = new_rows
                    .get(i)
                    .cloned()
                    .or_else(|| new_rows.first().cloned());
                if let (Some(old_row), Some(new_row)) = (old_row, new_row) {
                    result.push(IndexDiff {
                        old: old_row,
                        new: new_row,
                    });
                }
            }
            result
        })
        .collect()
}

fn expand_fks(fks: &[PgForeignKey]) -> Vec<ForeignKeyInfo> {
    fks.iter().flat_map(expand_fk).collect()
}

fn expand_fk(fk: &PgForeignKey) -> Vec<ForeignKeyInfo> {
    fk.columns
        .iter()
        .enumerate()
        .map(|(i, col)| ForeignKeyInfo {
            constraint_name: fk.name.clone(),
            column_name: col.clone(),
            referenced_table_name: fk.referenced_table.clone(),
            referenced_column_name: fk
                .referenced_columns
                .get(i)
                .cloned()
                .unwrap_or_default(),
            update_rule: fk.update_rule.clone(),
            delete_rule: fk.delete_rule.clone(),
            ordinal_position: (i + 1) as u32,
        })
        .collect()
}

// ── DDL 生成 ────────────────────────────────────────────

fn build_plan(
    comparisons: &[TableComparison],
    selected: Option<&[String]>,
) -> PgDdlPlan {
    let selected_set: Option<BTreeSet<&str>> = selected
        .map(|tables| tables.iter().map(|s| s.as_str()).collect());
    let is_selected = |table: &str| match &selected_set {
        Some(set) => set.contains(table),
        None => true,
    };

    let mut plan = PgDdlPlan::default();

    // 1) 删除表（破坏性）
    for cmp in comparisons {
        if cmp.status != "removed" || !is_selected(&cmp.table_name) {
            continue;
        }
        plan.destructive.push(DestructiveChange {
            table_name: cmp.table_name.clone(),
            kind: "drop_table".to_string(),
            detail: format!("目标库存在源库没有的表 {}", cmp.table_name),
        });
        plan.statements
            .push(format!("DROP TABLE IF EXISTS {};", quote_display(&cmp.table_name)));
    }

    // 2) 删除外键（修改或删除）
    for cmp in comparisons {
        if cmp.status == "removed" || !is_selected(&cmp.table_name) {
            continue;
        }
        for fk in &cmp.fks_removed {
            plan.statements.push(drop_fk_sql(&cmp.table_name, fk));
        }
        for (old, _) in &cmp.fks_modified {
            plan.statements.push(drop_fk_sql(&cmp.table_name, old));
        }
    }

    // 3) 删除索引 / 主键（修改或删除）
    for cmp in comparisons {
        if cmp.status == "removed" || !is_selected(&cmp.table_name) {
            continue;
        }
        for idx in &cmp.indexes_removed {
            add_drop_index(&mut plan, &cmp.table_name, idx);
        }
        for (old, new) in &cmp.indexes_modified {
            if old.has_expression || new.has_expression {
                plan.unsupported.push(format!(
                    "表达式索引 {}.{} 定义发生变化，当前不支持自动重建",
                    cmp.table_name, old.name
                ));
                continue;
            }
            add_drop_index(&mut plan, &cmp.table_name, old);
        }
    }

    // 4) 创建新增表
    for cmp in comparisons {
        if cmp.status != "added" || !is_selected(&cmp.table_name) {
            continue;
        }
        let source_table = comparisons_table_placeholder(cmp);
        let pk = cmp
            .indexes_added
            .iter()
            .find(|i| i.primary)
            .cloned();
        let mut parts: Vec<String> = source_table
            .iter()
            .map(column_definition)
            .collect();
        if let Some(pk) = &pk {
            parts.push(format!("PRIMARY KEY ({})", pk.plain_column_list()));
        }
        if !parts.is_empty() {
            plan.statements.push(format!(
                "CREATE TABLE {} (\n  {}\n);",
                quote_display(&cmp.table_name),
                parts.join(",\n  ")
            ));
        }
    }

    // 5) 列变更（仅已存在的表）
    //
    // 顺序很重要：PostgreSQL 不允许修改「被生成列引用」的列类型，因此必须
    // 先做删列/改列，再添加普通列，最后添加生成列。
    for cmp in comparisons {
        if cmp.status != "modified" || !is_selected(&cmp.table_name) {
            continue;
        }
        for col in &cmp.columns_removed {
            plan.destructive.push(DestructiveChange {
                table_name: cmp.table_name.clone(),
                kind: "drop_column".to_string(),
                detail: format!("删除列 {}（目标库有、源库没有）", col.column_name),
            });
            plan.statements.push(format!(
                "ALTER TABLE {} DROP COLUMN {};",
                quote_display(&cmp.table_name),
                quote_ident(&col.column_name)
            ));
        }
        for (old, new) in &cmp.columns_modified {
            add_column_changes(&mut plan, &cmp.table_name, old, new);
        }
        for col in cmp
            .columns_added
            .iter()
            .filter(|c| normalized_generation(c).is_none())
        {
            plan.statements.push(format!(
                "ALTER TABLE {} ADD COLUMN {};",
                quote_display(&cmp.table_name),
                column_definition(col)
            ));
        }
        for col in cmp
            .columns_added
            .iter()
            .filter(|c| normalized_generation(c).is_some())
        {
            plan.statements.push(format!(
                "ALTER TABLE {} ADD COLUMN {};",
                quote_display(&cmp.table_name),
                column_definition(col)
            ));
        }
    }

    // 6) 主键与索引创建（修改表 + 新增表）
    for cmp in comparisons {
        if !is_selected(&cmp.table_name) || cmp.status == "removed" {
            continue;
        }
        for idx in &cmp.indexes_added {
            if cmp.status == "added" && idx.primary {
                // 新增表的主键已在 CREATE TABLE 中内联。
                continue;
            }
            add_create_index(&mut plan, &cmp.table_name, idx);
        }
        for (_, new) in &cmp.indexes_modified {
            if new.has_expression {
                continue; // 已在上面记为 unsupported
            }
            add_create_index(&mut plan, &cmp.table_name, new);
        }
    }

    // 7) 外键创建
    for cmp in comparisons {
        if !is_selected(&cmp.table_name) || cmp.status == "removed" {
            continue;
        }
        for fk in &cmp.fks_added {
            plan.statements.push(add_fk_sql(&cmp.table_name, fk));
        }
        for (_, new) in &cmp.fks_modified {
            plan.statements.push(add_fk_sql(&cmp.table_name, new));
        }
    }

    plan
}

fn add_drop_index(plan: &mut PgDdlPlan, table: &str, idx: &PgIndex) {
    if idx.has_expression && !idx.primary {
        plan.unsupported.push(format!(
            "表达式索引 {}.{} 的删除未自动生成，请手工确认",
            table, idx.name
        ));
        return;
    }
    if idx.primary {
        plan.destructive.push(DestructiveChange {
            table_name: table.to_string(),
            kind: "modify_primary_key".to_string(),
            detail: format!("主键约束 {} 将被删除并重建", idx.name),
        });
        plan.statements.push(format!(
            "ALTER TABLE {} DROP CONSTRAINT IF EXISTS {};",
            quote_display(table),
            quote_ident(&idx.name)
        ));
    } else {
        plan.destructive.push(DestructiveChange {
            table_name: table.to_string(),
            kind: "drop_index".to_string(),
            detail: format!("删除索引 {}", idx.name),
        });
        // 唯一约束会以索引形式出现在 pg_index 中；先尝试按约束删除，
        // 普通索引下该语句是 no-op，再按索引删除。
        plan.statements.push(format!(
            "ALTER TABLE {} DROP CONSTRAINT IF EXISTS {};",
            quote_display(table),
            quote_ident(&idx.name)
        ));
        plan.statements.push(format!(
            "DROP INDEX IF EXISTS {};",
            quote_index(table, &idx.name)
        ));
    }
}

fn add_create_index(plan: &mut PgDdlPlan, table: &str, idx: &PgIndex) {
    if idx.has_expression {
        plan.unsupported.push(format!(
            "表达式索引 {}.{} 暂不支持自动创建",
            table, idx.name
        ));
        return;
    }
    if idx.primary {
        plan.statements.push(format!(
            "ALTER TABLE {} ADD PRIMARY KEY ({});",
            quote_display(table),
            idx.plain_column_list()
        ));
        return;
    }
    let unique = if idx.unique { "UNIQUE " } else { "" };
    plan.statements.push(format!(
        "CREATE {}INDEX {} ON {} ({});",
        unique,
        quote_index(table, &idx.name),
        quote_display(table),
        idx.column_list()
    ));
}

fn add_column_changes(
    plan: &mut PgDdlPlan,
    table: &str,
    old: &ColumnInfo,
    new: &ColumnInfo,
) {
    // 生成列表达式无法原地修改，只能 DROP + ADD（破坏性）。
    if normalized_generation(old) != normalized_generation(new) {
        plan.destructive.push(DestructiveChange {
            table_name: table.to_string(),
            kind: "rewrite_generated_column".to_string(),
            detail: format!("生成列 {} 的表达式变化，需要删除并重建该列", new.column_name),
        });
        plan.statements.push(format!(
            "ALTER TABLE {} DROP COLUMN {};",
            quote_display(table),
            quote_ident(&new.column_name)
        ));
        plan.statements.push(format!(
            "ALTER TABLE {} ADD COLUMN {};",
            quote_display(table),
            column_definition(new)
        ));
        return;
    }

    let mut actions: Vec<String> = Vec::new();

    if old.column_type != new.column_type {
        if type_change_is_destructive(&old.column_type, &new.column_type) {
            plan.destructive.push(DestructiveChange {
                table_name: table.to_string(),
                kind: "narrow_type".to_string(),
                detail: format!(
                    "列 {} 类型从 {} 收窄为 {}",
                    new.column_name, old.column_type, new.column_type
                ),
            });
        }
        actions.push(format!(
            "ALTER COLUMN {} TYPE {} USING {}::{}",
            quote_ident(&new.column_name),
            new.column_type,
            quote_ident(&new.column_name),
            new.column_type
        ));
    }

    if old.is_nullable != new.is_nullable {
        actions.push(format!(
            "ALTER COLUMN {} {}",
            quote_ident(&new.column_name),
            if new.is_nullable == "NO" {
                "SET NOT NULL"
            } else {
                "DROP NOT NULL"
            }
        ));
    }

    let old_auto = is_auto_increment(old);
    let new_auto = is_auto_increment(new);
    if new_auto && !old_auto {
        actions.push(format!(
            "ALTER COLUMN {} ADD GENERATED BY DEFAULT AS IDENTITY",
            quote_ident(&new.column_name)
        ));
    } else if old_auto && !new_auto {
        actions.push(format!(
            "ALTER COLUMN {} DROP IDENTITY IF EXISTS",
            quote_ident(&new.column_name)
        ));
        match effective_default(new) {
            Some(def) => actions.push(format!(
                "ALTER COLUMN {} SET DEFAULT {}",
                quote_ident(&new.column_name),
                def
            )),
            None => actions.push(format!(
                "ALTER COLUMN {} DROP DEFAULT",
                quote_ident(&new.column_name)
            )),
        }
    } else if effective_default(old) != effective_default(new) && !new_auto {
        match effective_default(new) {
            Some(def) => actions.push(format!(
                "ALTER COLUMN {} SET DEFAULT {}",
                quote_ident(&new.column_name),
                def
            )),
            None => actions.push(format!(
                "ALTER COLUMN {} DROP DEFAULT",
                quote_ident(&new.column_name)
            )),
        }
    }

    if !actions.is_empty() {
        plan.statements.push(format!(
            "ALTER TABLE {} {};",
            quote_display(table),
            actions.join(", ")
        ));
    }
}

fn drop_fk_sql(table: &str, fk: &PgForeignKey) -> String {
    format!(
        "ALTER TABLE {} DROP CONSTRAINT IF EXISTS {};",
        quote_display(table),
        quote_ident(&fk.name)
    )
}

fn add_fk_sql(table: &str, fk: &PgForeignKey) -> String {
    let cols = fk
        .columns
        .iter()
        .map(|c| quote_ident(c))
        .collect::<Vec<_>>()
        .join(", ");
    let ref_cols = fk
        .referenced_columns
        .iter()
        .map(|c| quote_ident(c))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "ALTER TABLE {} ADD CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({}) ON UPDATE {} ON DELETE {};",
        quote_display(table),
        quote_ident(&fk.name),
        cols,
        quote_display(&fk.referenced_table),
        ref_cols,
        normalize_action(&fk.update_rule),
        normalize_action(&fk.delete_rule)
    )
}

fn normalize_action(action: &str) -> String {
    let upper = action.trim().to_uppercase();
    if upper.is_empty() {
        "NO ACTION".to_string()
    } else {
        upper
    }
}

fn column_definition(col: &ColumnInfo) -> String {
    let mut sql = format!(
        "{} {}",
        quote_ident(&col.column_name),
        col.column_type
    );
    if let Some(expr) = normalized_generation(col) {
        sql.push_str(&format!(" GENERATED ALWAYS AS ({}) STORED", expr));
        if col.is_nullable == "NO" {
            sql.push_str(" NOT NULL");
        }
        return sql;
    }
    if is_auto_increment(col) {
        sql.push_str(" GENERATED BY DEFAULT AS IDENTITY");
    } else if let Some(def) = effective_default(col) {
        sql.push_str(&format!(" DEFAULT {}", def));
    }
    if col.is_nullable == "NO" {
        sql.push_str(" NOT NULL");
    }
    sql
}

fn quote_ident(name: &str) -> String {
    crate::sql_util::quote_ident_pg(name)
}

fn quote_display(name: &str) -> String {
    match split_pg_table_ref(name) {
        (Some(schema), table) => format!("{}.{}", quote_ident(schema), quote_ident(table)),
        (None, table) => quote_ident(table),
    }
}

fn quote_index(table: &str, index: &str) -> String {
    match split_pg_table_ref(table) {
        (Some(schema), _) => format!("{}.{}", quote_ident(schema), quote_ident(index)),
        (None, _) => quote_ident(index),
    }
}

/// 新增表的列来自 `TableComparison.columns_added`。
fn comparisons_table_placeholder(cmp: &TableComparison) -> Vec<ColumnInfo> {
    cmp.columns_added.clone()
}

// ── 类型放宽矩阵 ────────────────────────────────────────

/// 返回 true 表示该类型变更可能收窄/丢失数据，必须要求确认。
fn type_change_is_destructive(old: &str, new: &str) -> bool {
    if old.trim().eq_ignore_ascii_case(new.trim()) {
        return false;
    }
    let (old_base, old_params) = parse_type(old);
    let (new_base, new_params) = parse_type(new);

    match (old_base.as_str(), new_base.as_str()) {
        ("smallint", "integer") | ("smallint", "bigint") | ("integer", "bigint") => false,
        ("smallint", "numeric") | ("integer", "numeric") | ("bigint", "numeric") => {
            match &new_params {
                None => false,
                Some(params) => {
                    let digits = match old_base.as_str() {
                        "smallint" => 5,
                        "integer" => 10,
                        _ => 19,
                    };
                    numeric_integer_digits(params) < digits
                }
            }
        }
        ("character varying", "text") | ("character", "text") => false,
        ("character varying", "character varying")
        | ("character", "character")
        | ("character", "character varying")
        | ("character varying", "character") => {
            !char_length_widened(old_params.as_ref(), new_params.as_ref())
        }
        ("numeric", "numeric") => !numeric_widened(old_params.as_ref(), new_params.as_ref()),
        ("timestamp without time zone", "timestamp with time zone")
        | ("timestamp with time zone", "timestamp without time zone")
        | ("time without time zone", "time with time zone")
        | ("time with time zone", "time without time zone") => true,
        ("timestamp without time zone", "timestamp without time zone")
        | ("timestamp with time zone", "timestamp with time zone")
        | ("time without time zone", "time without time zone")
        | ("time with time zone", "time with time zone") => {
            let old_precision = old_params
                .as_ref()
                .and_then(|p| p.first().copied())
                .unwrap_or(6);
            let new_precision = new_params
                .as_ref()
                .and_then(|p| p.first().copied())
                .unwrap_or(6);
            new_precision < old_precision
        }
        _ => true,
    }
}

fn parse_type(raw: &str) -> (String, Option<Vec<i64>>) {
    let lower = raw.trim().to_lowercase();
    let mut base = lower.clone();
    let mut params = None;
    if let (Some(open), Some(close)) = (lower.find('('), lower.find(')')) {
        if open < close {
            let head = lower[..open].trim();
            let tail = lower[close + 1..].trim();
            base = if tail.is_empty() {
                head.to_string()
            } else {
                format!("{} {}", head, tail)
            };
            params = Some(
                lower[open + 1..close]
                    .split(',')
                    .filter_map(|p| p.trim().parse::<i64>().ok())
                    .collect(),
            );
        }
    }
    let base = match base.as_str() {
        "int" | "int4" | "serial" => "integer".to_string(),
        "int8" | "bigserial" => "bigint".to_string(),
        "int2" | "smallserial" => "smallint".to_string(),
        "varchar" => "character varying".to_string(),
        "char" | "bpchar" => "character".to_string(),
        "decimal" => "numeric".to_string(),
        "bool" => "boolean".to_string(),
        "float8" => "double precision".to_string(),
        "float4" => "real".to_string(),
        other => other.to_string(),
    };
    (base, params)
}

fn numeric_integer_digits(params: &[i64]) -> i64 {
    let precision = params.first().copied().unwrap_or(0);
    let scale = params.get(1).copied().unwrap_or(0);
    precision - scale
}

fn numeric_widened(old: Option<&Vec<i64>>, new: Option<&Vec<i64>>) -> bool {
    match (old, new) {
        (_, None) => true,      // 新类型无精度限制，视为放宽
        (None, Some(_)) => false, // 旧类型无限制 → 新类型有限制，收窄
        (Some(old), Some(new)) => {
            let old_p = *old.first().unwrap_or(&0);
            let old_s = *old.get(1).unwrap_or(&0);
            let new_p = *new.first().unwrap_or(&0);
            let new_s = *new.get(1).unwrap_or(&0);
            new_s >= old_s && (new_p - new_s) >= (old_p - old_s)
        }
    }
}

fn char_length_widened(old: Option<&Vec<i64>>, new: Option<&Vec<i64>>) -> bool {
    match (old, new) {
        (_, None) => true,       // 新类型无长度限制，视为放宽
        (None, Some(_)) => false, // 无限制 → 有限制，收窄
        (Some(old), Some(new)) => {
            new.first().copied().unwrap_or(0) >= old.first().copied().unwrap_or(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::ViewInfo;

    fn column(
        name: &str,
        column_type: &str,
        nullable: &str,
        default: Option<&str>,
    ) -> ColumnInfo {
        ColumnInfo {
            column_name: name.to_string(),
            data_type: column_type.split('(').next().unwrap_or(column_type).to_string(),
            column_type: column_type.to_string(),
            is_nullable: nullable.to_string(),
            column_comment: None,
            column_key: String::new(),
            column_default: default.map(|s| s.to_string()),
            extra: String::new(),
            type_warning: None,
            generation_expression: None,
        }
    }

    fn index(
        name: &str,
        columns: &[(&str, &str)],
        unique: bool,
        primary: bool,
    ) -> Vec<IndexInfo> {
        columns
            .iter()
            .enumerate()
            .map(|(i, (col, sort))| IndexInfo {
                index_name: name.to_string(),
                column_name: col.to_string(),
                non_unique: !unique,
                index_type: "btree".to_string(),
                seq_in_index: (i + 1) as u32,
                sort_order: sort.to_string(),
                is_primary: primary,
                is_expression: false,
            })
            .collect()
    }

    fn fk(
        name: &str,
        cols: &[(&str, &str)],
        ref_table: &str,
        on_update: &str,
        on_delete: &str,
    ) -> Vec<ForeignKeyInfo> {
        cols.iter()
            .enumerate()
            .map(|(i, (col, ref_col))| ForeignKeyInfo {
                constraint_name: name.to_string(),
                column_name: col.to_string(),
                referenced_table_name: ref_table.to_string(),
                referenced_column_name: ref_col.to_string(),
                update_rule: on_update.to_string(),
                delete_rule: on_delete.to_string(),
                ordinal_position: (i + 1) as u32,
            })
            .collect()
    }

    fn table(
        name: &str,
        columns: Vec<ColumnInfo>,
        indexes: Vec<IndexInfo>,
        fks: Vec<ForeignKeyInfo>,
    ) -> TableWithDetails {
        TableWithDetails {
            table_name: name.to_string(),
            columns,
            indexes,
            foreign_keys: fks,
        }
    }

    fn schema(tables: Vec<TableWithDetails>) -> SchemaResponse {
        SchemaResponse {
            db_name: "demo".to_string(),
            tables,
            views: Vec::<ViewInfo>::new(),
        }
    }

    fn modified(src: &TableWithDetails, tgt: &TableWithDetails) -> TableComparison {
        compare_table(src, tgt)
    }

    #[test]
    fn detects_added_removed_and_unchanged_tables() {
        let src = schema(vec![
            table("users", vec![column("id", "integer", "NO", None)], vec![], vec![]),
            table("orders", vec![column("id", "integer", "NO", None)], vec![], vec![]),
        ]);
        let tgt = schema(vec![
            table("users", vec![column("id", "integer", "NO", None)], vec![], vec![]),
            table("legacy", vec![column("id", "integer", "NO", None)], vec![], vec![]),
        ]);

        let result = PgSchemaSyncEngine::compare(&src, &tgt);
        let statuses: BTreeMap<_, _> = result
            .diff
            .tables
            .iter()
            .map(|t| (t.table_name.as_str(), t.status.as_str()))
            .collect();
        assert_eq!(statuses.get("users"), Some(&"unchanged"));
        assert_eq!(statuses.get("orders"), Some(&"added"));
        assert_eq!(statuses.get("legacy"), Some(&"removed"));
    }

    #[test]
    fn detects_column_add_drop_type_nullable_and_default_changes() {
        let tgt = table(
            "users",
            vec![
                column("id", "integer", "NO", None),
                column("legacy_col", "text", "YES", None),
                column("name", "character varying(20)", "NO", Some("'x'::text")),
            ],
            vec![],
            vec![],
        );
        let src = table(
            "users",
            vec![
                column("id", "integer", "NO", None),
                column("name", "character varying(50)", "YES", Some("'y'::text")),
                column("email", "text", "YES", None),
            ],
            vec![],
            vec![],
        );

        let cmp = modified(&src, &tgt);
        assert_eq!(cmp.status, "modified");
        assert_eq!(cmp.columns_added.len(), 1);
        assert_eq!(cmp.columns_added[0].column_name, "email");
        assert_eq!(cmp.columns_removed.len(), 1);
        assert_eq!(cmp.columns_removed[0].column_name, "legacy_col");
        assert_eq!(cmp.columns_modified.len(), 1);
        assert_eq!(cmp.columns_modified[0].0.column_type, "character varying(20)");
        assert_eq!(cmp.columns_modified[0].1.column_type, "character varying(50)");
        assert_eq!(cmp.columns_modified[0].0.is_nullable, "NO");
        assert_eq!(cmp.columns_modified[0].1.is_nullable, "YES");
    }

    #[test]
    fn generated_column_expression_change_is_destructive_rewrite() {
        let tgt_col = column("full_name", "text", "YES", None);
        let src_col = ColumnInfo {
            generation_expression: Some("(first_name || last_name)".to_string()),
            ..column("full_name", "text", "YES", None)
        };
        let tgt = table("people", vec![tgt_col], vec![], vec![]);
        let src = table("people", vec![src_col], vec![], vec![]);
        let comparisons = vec![compare_table(&src, &tgt)];
        let plan = build_plan(&comparisons, None);

        assert!(plan
            .destructive
            .iter()
            .any(|d| d.kind == "rewrite_generated_column"));
        assert!(plan
            .statements
            .iter()
            .any(|s| s.contains("DROP COLUMN \"full_name\"")));
        assert!(plan
            .statements
            .iter()
            .any(|s| s.contains("GENERATED ALWAYS AS")));
    }

    #[test]
    fn type_widening_is_safe_but_narrowing_is_destructive() {
        assert!(!type_change_is_destructive("integer", "bigint"));
        assert!(!type_change_is_destructive("smallint", "integer"));
        assert!(!type_change_is_destructive(
            "character varying(20)",
            "character varying(50)"
        ));
        assert!(!type_change_is_destructive("character varying(20)", "text"));
        assert!(!type_change_is_destructive("numeric(10,2)", "numeric(12,2)"));
        assert!(!type_change_is_destructive("numeric(10,2)", "numeric(11,3)"));
        assert!(!type_change_is_destructive("integer", "numeric"));
        assert!(!type_change_is_destructive("timestamp(3) without time zone", "timestamp(6) without time zone"));

        assert!(type_change_is_destructive("bigint", "integer"));
        assert!(type_change_is_destructive(
            "character varying(50)",
            "character varying(20)"
        ));
        assert!(type_change_is_destructive("text", "character varying(20)"));
        assert!(type_change_is_destructive("numeric(12,2)", "numeric(10,2)"));
        assert!(type_change_is_destructive("numeric(10,2)", "numeric(10,4)"));
        assert!(type_change_is_destructive("integer", "numeric(8,2)"));
        assert!(type_change_is_destructive("timestamp without time zone", "timestamp(0) without time zone"));
        assert!(type_change_is_destructive("text", "integer"));
        assert!(type_change_is_destructive("jsonb", "json"));
    }

    #[test]
    fn detects_index_column_order_and_sort_direction_changes() {
        let tgt = table(
            "events",
            vec![
                column("a", "integer", "NO", None),
                column("b", "integer", "NO", None),
            ],
            index("idx_ab", &[("a", "ASC"), ("b", "ASC")], false, false),
            vec![],
        );
        let src = table(
            "events",
            vec![
                column("a", "integer", "NO", None),
                column("b", "integer", "NO", None),
            ],
            index("idx_ab", &[("b", "DESC"), ("a", "ASC")], false, false),
            vec![],
        );

        let cmp = modified(&src, &tgt);
        assert_eq!(cmp.indexes_modified.len(), 1);
        let (old, new) = &cmp.indexes_modified[0];
        assert_eq!(old.columns[0].name, "a");
        assert_eq!(new.columns[0].name, "b");
        assert_eq!(new.columns[0].sort, "DESC");

        let comparisons = vec![cmp];
        let plan = build_plan(&comparisons, None);
        assert!(plan.destructive.iter().any(|d| d.kind == "drop_index"));
        assert!(plan
            .statements
            .iter()
            .any(|s| s.contains("CREATE INDEX \"idx_ab\"")));
        assert!(plan
            .statements
            .iter()
            .any(|s| s.contains("\"b\" DESC")));
    }

    #[test]
    fn detects_unique_and_primary_key_changes() {
        let tgt = table(
            "users",
            vec![
                column("id", "integer", "NO", None),
                column("tenant", "integer", "NO", None),
            ],
            index("users_pkey", &[("id", "ASC")], true, true),
            vec![],
        );
        let src = table(
            "users",
            vec![
                column("id", "integer", "NO", None),
                column("tenant", "integer", "NO", None),
            ],
            index("users_pkey", &[("id", "ASC"), ("tenant", "ASC")], true, true),
            vec![],
        );

        let cmp = modified(&src, &tgt);
        assert_eq!(cmp.indexes_modified.len(), 1);
        let plan = build_plan(&vec![cmp], None);
        assert!(plan
            .destructive
            .iter()
            .any(|d| d.kind == "modify_primary_key"));
        assert!(plan
            .statements
            .iter()
            .any(|s| s.contains("ADD PRIMARY KEY (\"id\", \"tenant\")")));
    }

    #[test]
    fn detects_fk_add_remove_and_action_change() {
        let tgt = table(
            "orders",
            vec![column("user_id", "integer", "NO", None)],
            vec![],
            fk("fk_user", &[("user_id", "id")], "users", "NO ACTION", "NO ACTION"),
        );
        let src = table(
            "orders",
            vec![column("user_id", "integer", "NO", None)],
            vec![],
            fk("fk_user", &[("user_id", "id")], "users", "CASCADE", "SET NULL"),
        );

        let cmp = modified(&src, &tgt);
        assert_eq!(cmp.fks_modified.len(), 1);
        let plan = build_plan(&vec![cmp], None);
        assert!(plan
            .statements
            .iter()
            .any(|s| s.contains("ON UPDATE CASCADE ON DELETE SET NULL")));
    }

    #[test]
    fn cross_schema_same_table_name_is_not_treated_as_equal() {
        let src = schema(vec![
            table(
                "public.users",
                vec![column("id", "integer", "NO", None)],
                vec![],
                vec![],
            ),
            table(
                "app.users",
                vec![
                    column("id", "integer", "NO", None),
                    column("tenant", "text", "YES", None),
                ],
                vec![],
                vec![],
            ),
        ]);
        let tgt = schema(vec![table(
            "public.users",
            vec![column("id", "integer", "NO", None)],
            vec![],
            vec![],
        )]);

        let result = PgSchemaSyncEngine::compare(&src, &tgt);
        assert_eq!(result.diff.tables.len(), 2);
        let app = result
            .diff
            .tables
            .iter()
            .find(|t| t.table_name == "app.users")
            .expect("应保留其他 schema 的同名表");
        assert_eq!(app.status, "added");
    }

    #[test]
    fn identical_schemas_produce_no_diff_and_no_ddl() {
        let s = schema(vec![table(
            "users",
            vec![column("id", "integer", "NO", None)],
            index("users_pkey", &[("id", "ASC")], true, true),
            vec![],
        )]);
        let result = PgSchemaSyncEngine::compare(&s, &s);
        assert!(result
            .diff
            .tables
            .iter()
            .all(|t| t.status == "unchanged"));
        assert!(result.destructive.is_empty());

        let plan = PgSchemaSyncEngine::generate_ddl(&s, &s, &["users".to_string()], false)
            .expect("无差异时不需要确认");
        assert!(plan.statements.is_empty());
    }

    #[test]
    fn destructive_change_requires_confirmation_and_produces_no_plan_without_it() {
        let src = schema(vec![table(
            "users",
            vec![column("id", "integer", "NO", None)],
            vec![],
            vec![],
        )]);
        let tgt = schema(vec![table(
            "users",
            vec![
                column("id", "integer", "NO", None),
                column("legacy", "text", "YES", None),
            ],
            vec![],
            vec![],
        )]);

        let blocked = PgSchemaSyncEngine::generate_ddl(&src, &tgt, &["users".to_string()], false)
            .expect_err("删除列必须要求确认");
        assert!(blocked
            .destructive
            .iter()
            .any(|d| d.kind == "drop_column"));

        let plan = PgSchemaSyncEngine::generate_ddl(&src, &tgt, &["users".to_string()], true)
            .expect("确认后应生成 DDL");
        assert!(plan
            .statements
            .iter()
            .any(|s| s.contains("DROP COLUMN \"legacy\"")));
    }

    #[test]
    fn identity_and_serial_are_normalized_as_equal() {
        let tgt_col = ColumnInfo {
            extra: "identity".to_string(),
            ..column("id", "integer", "NO", None)
        };
        let src_col = ColumnInfo {
            extra: "auto_increment".to_string(),
            ..column("id", "integer", "NO", Some("nextval('users_id_seq'::regclass)"))
        };
        assert!(columns_equal(&tgt_col, &src_col));
    }

    #[test]
    fn only_selected_tables_are_planned() {
        let src = schema(vec![
            table("a", vec![column("id", "integer", "NO", None)], vec![], vec![]),
            table("b", vec![column("id", "integer", "NO", None)], vec![], vec![]),
        ]);
        let tgt = schema(vec![]);
        let plan = PgSchemaSyncEngine::generate_ddl(
            &src,
            &tgt,
            &["a".to_string()],
            false,
        )
        .expect("新增表不是破坏性变更");
        assert!(plan.statements.iter().all(|s| s.contains("\"a\"")));
        assert!(!plan.statements.iter().any(|s| s.contains("\"b\"")));
    }

    #[test]
    fn expression_indexes_are_reported_as_unsupported_not_silently_ignored() {
        let mut idx = index("idx_lower_name", &[("", "ASC")], false, false);
        idx[0].is_expression = true;
        idx[0].column_name = String::new();
        let src = table(
            "users",
            vec![column("name", "text", "YES", None)],
            idx,
            vec![],
        );
        let tgt = table("users", vec![column("name", "text", "YES", None)], vec![], vec![]);

        let cmp = modified(&src, &tgt);
        let plan = build_plan(&vec![cmp], None);
        assert!(plan
            .unsupported
            .iter()
            .any(|u| u.contains("表达式索引")));
    }
}
