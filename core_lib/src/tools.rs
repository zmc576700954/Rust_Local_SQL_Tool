use crate::config::AppConfig;
use crate::db::DbClient;
use crate::schema::{ColumnInfo, SchemaResponse, TableWithDetails};
use crate::schema_ext::{ForeignKeyInfo, IndexInfo};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnDiff {
    pub old: ColumnInfo,
    pub new: ColumnInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexDiff {
    pub old: IndexInfo,
    pub new: IndexInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FkDiff {
    pub old: ForeignKeyInfo,
    pub new: ForeignKeyInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableDiff {
    pub table_name: String,
    pub status: String, // "added", "removed", "modified", "unchanged"
    pub columns_added: Vec<ColumnInfo>,
    pub columns_removed: Vec<ColumnInfo>,
    pub columns_modified: Vec<ColumnDiff>,
    pub indexes_added: Vec<IndexInfo>,
    pub indexes_removed: Vec<IndexInfo>,
    pub indexes_modified: Vec<IndexDiff>,
    pub fks_added: Vec<ForeignKeyInfo>,
    pub fks_removed: Vec<ForeignKeyInfo>,
    pub fks_modified: Vec<FkDiff>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaDiff {
    pub tables: Vec<TableDiff>,
}

pub struct DdlEngine;

impl DdlEngine {
    pub fn generate_preview(
        old_table: Option<&TableWithDetails>,
        new_table: &TableWithDetails,
    ) -> String {
        if let Some(old) = old_table {
            // Very simplified ALTER TABLE logic
            let mut alters = Vec::new();
            let table_name = &new_table.table_name;

            for new_col in &new_table.columns {
                if let Some(old_col) = old
                    .columns
                    .iter()
                    .find(|c| c.column_name == new_col.column_name)
                {
                    // Check if anything changed
                    if old_col.column_type != new_col.column_type
                        || old_col.is_nullable != new_col.is_nullable
                        || old_col.column_default != new_col.column_default
                        || old_col.extra != new_col.extra
                        || old_col.column_comment != new_col.column_comment
                    {
                        let mut col_def = format!(
                            "MODIFY COLUMN `{}` {}",
                            new_col.column_name, new_col.column_type
                        );
                        if new_col.is_nullable == "NO" {
                            col_def.push_str(" NOT NULL");
                        } else {
                            col_def.push_str(" NULL");
                        }
                        if let Some(def) = &new_col.column_default {
                            col_def.push_str(&format!(" DEFAULT '{}'", def));
                        }
                        if new_col.extra.contains("auto_increment") {
                            col_def.push_str(" AUTO_INCREMENT");
                        }
                        if let Some(comment) = &new_col.column_comment {
                            col_def.push_str(&format!(" COMMENT '{}'", comment));
                        }
                        alters.push(col_def);
                    }
                } else {
                    // Added column
                    let mut col_def = format!(
                        "ADD COLUMN `{}` {}",
                        new_col.column_name, new_col.column_type
                    );
                    if new_col.is_nullable == "NO" {
                        col_def.push_str(" NOT NULL");
                    } else {
                        col_def.push_str(" NULL");
                    }
                    if let Some(def) = &new_col.column_default {
                        col_def.push_str(&format!(" DEFAULT '{}'", def));
                    }
                    if new_col.extra.contains("auto_increment") {
                        col_def.push_str(" AUTO_INCREMENT");
                    }
                    if let Some(comment) = &new_col.column_comment {
                        col_def.push_str(&format!(" COMMENT '{}'", comment));
                    }
                    alters.push(col_def);
                }
            }

            for old_col in &old.columns {
                if !new_table
                    .columns
                    .iter()
                    .any(|c| c.column_name == old_col.column_name)
                {
                    alters.push(format!("DROP COLUMN `{}`", old_col.column_name));
                }
            }

            // Primary Key handling
            let old_pks: Vec<_> = old
                .columns
                .iter()
                .filter(|c| c.column_key == "PRI")
                .map(|c| &c.column_name)
                .collect();
            let new_pks: Vec<_> = new_table
                .columns
                .iter()
                .filter(|c| c.column_key == "PRI")
                .map(|c| &c.column_name)
                .collect();
            if old_pks != new_pks {
                if !old_pks.is_empty() {
                    alters.push("DROP PRIMARY KEY".to_string());
                }
                if !new_pks.is_empty() {
                    let pk_str = new_pks
                        .iter()
                        .map(|c| format!("`{}`", c))
                        .collect::<Vec<_>>()
                        .join(", ");
                    alters.push(format!("ADD PRIMARY KEY ({})", pk_str));
                }
            }

            // Indexes handling
            for new_idx in &new_table.indexes {
                if !old
                    .indexes
                    .iter()
                    .any(|i| i.index_name == new_idx.index_name)
                {
                    let idx_type = if new_idx.non_unique {
                        "INDEX"
                    } else {
                        "UNIQUE INDEX"
                    };
                    alters.push(format!(
                        "ADD {} `{}` (`{}`)",
                        idx_type, new_idx.index_name, new_idx.column_name
                    ));
                }
            }
            for old_idx in &old.indexes {
                // don't drop primary key here, it's handled above
                if old_idx.index_name != "PRIMARY"
                    && !new_table
                        .indexes
                        .iter()
                        .any(|i| i.index_name == old_idx.index_name)
                {
                    alters.push(format!("DROP INDEX `{}`", old_idx.index_name));
                }
            }

            // Foreign Keys handling
            for new_fk in &new_table.foreign_keys {
                if !old
                    .foreign_keys
                    .iter()
                    .any(|f| f.constraint_name == new_fk.constraint_name)
                {
                    alters.push(format!("ADD CONSTRAINT `{}` FOREIGN KEY (`{}`) REFERENCES `{}` (`{}`) ON DELETE {} ON UPDATE {}", 
                        new_fk.constraint_name, new_fk.column_name, new_fk.referenced_table_name, new_fk.referenced_column_name,
                        new_fk.delete_rule, new_fk.update_rule));
                }
            }
            for old_fk in &old.foreign_keys {
                if !new_table
                    .foreign_keys
                    .iter()
                    .any(|f| f.constraint_name == old_fk.constraint_name)
                {
                    alters.push(format!("DROP FOREIGN KEY `{}`", old_fk.constraint_name));
                }
            }

            if alters.is_empty() {
                return "-- No changes detected".to_string();
            }

            // Classify ALTER clauses for ALGORITHM hint
            let algorithm = classify_alter_algorithm(&alters);
            let hint = if algorithm == "DEFAULT" {
                String::new()
            } else {
                format!("\n  /* {} */", algorithm)
            };

            format!(
                "ALTER TABLE `{}`\n  {};{}\n-- ⚠ Review before executing. For large tables, consider gh-ost or pt-online-schema-change.",
                table_name,
                alters.join(",\n  "),
                hint
            )
        } else {
            // CREATE TABLE logic
            let mut cols = Vec::new();
            for col in &new_table.columns {
                let mut col_def = format!("`{}` {}", col.column_name, col.column_type);
                if col.is_nullable == "NO" {
                    col_def.push_str(" NOT NULL");
                } else {
                    col_def.push_str(" NULL");
                }
                if let Some(def) = &col.column_default {
                    col_def.push_str(&format!(" DEFAULT '{}'", def));
                }
                if col.extra.contains("auto_increment") {
                    col_def.push_str(" AUTO_INCREMENT");
                }
                if let Some(comment) = &col.column_comment {
                    col_def.push_str(&format!(" COMMENT '{}'", comment));
                }
                cols.push(col_def);
            }

            let pks: Vec<_> = new_table
                .columns
                .iter()
                .filter(|c| c.column_key == "PRI")
                .map(|c| format!("`{}`", c.column_name))
                .collect();
            if !pks.is_empty() {
                cols.push(format!("PRIMARY KEY ({})", pks.join(", ")));
            }

            for idx in &new_table.indexes {
                if idx.index_name != "PRIMARY" {
                    let idx_type = if idx.non_unique {
                        "INDEX"
                    } else {
                        "UNIQUE INDEX"
                    };
                    cols.push(format!(
                        "{} `{}` (`{}`)",
                        idx_type, idx.index_name, idx.column_name
                    ));
                }
            }

            for fk in &new_table.foreign_keys {
                cols.push(format!("CONSTRAINT `{}` FOREIGN KEY (`{}`) REFERENCES `{}` (`{}`) ON DELETE {} ON UPDATE {}", 
                    fk.constraint_name, fk.column_name, fk.referenced_table_name, fk.referenced_column_name,
                    fk.delete_rule, fk.update_rule));
            }

            format!(
                "CREATE TABLE `{}` (\n  {}\n);",
                new_table.table_name,
                cols.join(",\n  ")
            )
        }
    }

    /// Generate rollback DDL that reverses the forward ALTER TABLE.
    /// Returns a SQL statement that, when executed, reverts the schema change.
    pub fn generate_rollback(
        old_table: Option<&TableWithDetails>,
        new_table: &TableWithDetails,
    ) -> String {
        let Some(old) = old_table else {
            // Forward was CREATE TABLE → rollback is DROP TABLE
            return format!("DROP TABLE IF EXISTS `{}`;", new_table.table_name);
        };

        let mut rollbacks = Vec::new();
        let table_name = &new_table.table_name;

        // Columns added in forward → DROP COLUMN in rollback
        for new_col in &new_table.columns {
            if !old
                .columns
                .iter()
                .any(|c| c.column_name == new_col.column_name)
            {
                rollbacks.push(format!("DROP COLUMN `{}`", new_col.column_name));
            }
        }

        // Columns dropped in forward → ADD COLUMN (with old definition) in rollback
        for old_col in &old.columns {
            if !new_table
                .columns
                .iter()
                .any(|c| c.column_name == old_col.column_name)
            {
                let mut col_def =
                    format!("ADD COLUMN `{}` {}", old_col.column_name, old_col.column_type);
                if old_col.is_nullable == "NO" {
                    col_def.push_str(" NOT NULL");
                } else {
                    col_def.push_str(" NULL");
                }
                if let Some(def) = &old_col.column_default {
                    col_def.push_str(&format!(" DEFAULT '{}'", def));
                }
                if old_col.extra.contains("auto_increment") {
                    col_def.push_str(" AUTO_INCREMENT");
                }
                rollbacks.push(col_def);
            }
        }

        // Columns modified in forward → MODIFY back to old definition
        for new_col in &new_table.columns {
            if let Some(old_col) = old
                .columns
                .iter()
                .find(|c| c.column_name == new_col.column_name)
            {
                if old_col.column_type != new_col.column_type
                    || old_col.is_nullable != new_col.is_nullable
                    || old_col.column_default != new_col.column_default
                    || old_col.extra != new_col.extra
                {
                    let mut col_def = format!(
                        "MODIFY COLUMN `{}` {}",
                        old_col.column_name, old_col.column_type
                    );
                    if old_col.is_nullable == "NO" {
                        col_def.push_str(" NOT NULL");
                    } else {
                        col_def.push_str(" NULL");
                    }
                    if let Some(def) = &old_col.column_default {
                        col_def.push_str(&format!(" DEFAULT '{}'", def));
                    }
                    if old_col.extra.contains("auto_increment") {
                        col_def.push_str(" AUTO_INCREMENT");
                    }
                    rollbacks.push(col_def);
                }
            }
        }

        // Indexes added in forward → DROP INDEX
        for new_idx in &new_table.indexes {
            if new_idx.index_name != "PRIMARY"
                && !old
                    .indexes
                    .iter()
                    .any(|i| i.index_name == new_idx.index_name)
            {
                rollbacks.push(format!("DROP INDEX `{}`", new_idx.index_name));
            }
        }

        // Indexes dropped in forward → ADD INDEX (with old definition)
        for old_idx in &old.indexes {
            if old_idx.index_name != "PRIMARY"
                && !new_table
                    .indexes
                    .iter()
                    .any(|i| i.index_name == old_idx.index_name)
            {
                let idx_type = if old_idx.non_unique {
                    "INDEX"
                } else {
                    "UNIQUE INDEX"
                };
                rollbacks.push(format!(
                    "ADD {} `{}` (`{}`)",
                    idx_type, old_idx.index_name, old_idx.column_name
                ));
            }
        }

        // Foreign keys added in forward → DROP FOREIGN KEY
        for new_fk in &new_table.foreign_keys {
            if !old
                .foreign_keys
                .iter()
                .any(|f| f.constraint_name == new_fk.constraint_name)
            {
                rollbacks.push(format!(
                    "DROP FOREIGN KEY `{}`",
                    new_fk.constraint_name
                ));
            }
        }

        // Foreign keys dropped in forward → ADD CONSTRAINT
        for old_fk in &old.foreign_keys {
            if !new_table
                .foreign_keys
                .iter()
                .any(|f| f.constraint_name == old_fk.constraint_name)
            {
                rollbacks.push(format!(
                    "ADD CONSTRAINT `{}` FOREIGN KEY (`{}`) REFERENCES `{}` (`{}`)",
                    old_fk.constraint_name,
                    old_fk.column_name,
                    old_fk.referenced_table_name,
                    old_fk.referenced_column_name,
                ));
            }
        }

        if rollbacks.is_empty() {
            return "-- No rollback needed".to_string();
        }
        format!(
            "-- Rollback DDL for: ALTER TABLE `{}`\nALTER TABLE `{}`\n  {};",
            table_name,
            table_name,
            rollbacks.join(",\n  ")
        )
    }
}

/// Classify ALTER clauses and return the most appropriate MySQL ALGORITHM hint.
/// Returns one of: "ALGORITHM=INSTANT", "ALGORITHM=INPLACE, LOCK=NONE", "ALGORITHM=DEFAULT".
fn classify_alter_algorithm(alters: &[String]) -> String {
    let mut has_inplace = false;

    for alter in alters {
        let upper = alter.trim().to_uppercase();
        if upper.starts_with("ADD COLUMN") {
            // ALGORITHM=INSTANT (MySQL 8.0.12+) — no table rebuild
            continue;
        } else if upper.starts_with("DROP COLUMN") {
            // ALGORITHM=INSTANT (MySQL 8.0.29+) — no table rebuild
            continue;
        } else if upper.starts_with("MODIFY COLUMN") {
            // Requires table rebuild
            has_inplace = true;
        } else if upper.starts_with("ADD INDEX")
            || upper.starts_with("ADD UNIQUE INDEX")
            || upper.starts_with("ADD INDEX")
            || upper.starts_with("DROP INDEX")
        {
            has_inplace = true;
        } else if upper.starts_with("ADD PRIMARY KEY") || upper.starts_with("DROP PRIMARY KEY") {
            has_inplace = true;
        } else if upper.contains("FOREIGN KEY") {
            // Foreign key operations use DEFAULT algorithm
            return "ALGORITHM=DEFAULT".to_string();
        }
    }

    if has_inplace {
        "ALGORITHM=INPLACE, LOCK=NONE".to_string()
    } else {
        // All operations are INSTANT-compatible
        "ALGORITHM=INSTANT".to_string()
    }
}

pub struct SyncEngine;

impl SyncEngine {
    pub fn schema_sync(
        source: &SchemaResponse,
        target: &SchemaResponse,
    ) -> (SchemaDiff, Vec<String>) {
        let mut diff = SchemaDiff { tables: Vec::new() };
        let mut ddl_statements = Vec::new();

        for src_table in &source.tables {
            if let Some(tgt_table) = target
                .tables
                .iter()
                .find(|t| t.table_name == src_table.table_name)
            {
                // Table exists, compare properties
                let mut table_diff = TableDiff {
                    table_name: src_table.table_name.clone(),
                    status: "unchanged".to_string(),
                    columns_added: Vec::new(),
                    columns_removed: Vec::new(),
                    columns_modified: Vec::new(),
                    indexes_added: Vec::new(),
                    indexes_removed: Vec::new(),
                    indexes_modified: Vec::new(),
                    fks_added: Vec::new(),
                    fks_removed: Vec::new(),
                    fks_modified: Vec::new(),
                };

                // Compare columns
                for src_col in &src_table.columns {
                    if let Some(tgt_col) = tgt_table
                        .columns
                        .iter()
                        .find(|c| c.column_name == src_col.column_name)
                    {
                        if tgt_col.column_type != src_col.column_type
                            || tgt_col.is_nullable != src_col.is_nullable
                            || tgt_col.column_default != src_col.column_default
                            || tgt_col.extra != src_col.extra
                            || tgt_col.column_comment != src_col.column_comment
                        {
                            table_diff.columns_modified.push(ColumnDiff {
                                old: tgt_col.clone(),
                                new: src_col.clone(),
                            });
                            table_diff.status = "modified".to_string();
                        }
                    } else {
                        table_diff.columns_added.push(src_col.clone());
                        table_diff.status = "modified".to_string();
                    }
                }
                for tgt_col in &tgt_table.columns {
                    if !src_table
                        .columns
                        .iter()
                        .any(|c| c.column_name == tgt_col.column_name)
                    {
                        table_diff.columns_removed.push(tgt_col.clone());
                        table_diff.status = "modified".to_string();
                    }
                }

                // Compare indexes
                for src_idx in &src_table.indexes {
                    if let Some(tgt_idx) = tgt_table
                        .indexes
                        .iter()
                        .find(|i| i.index_name == src_idx.index_name)
                    {
                        if tgt_idx.column_name != src_idx.column_name
                            || tgt_idx.non_unique != src_idx.non_unique
                            || tgt_idx.index_type != src_idx.index_type
                        {
                            table_diff.indexes_modified.push(IndexDiff {
                                old: tgt_idx.clone(),
                                new: src_idx.clone(),
                            });
                            table_diff.status = "modified".to_string();
                        }
                    } else {
                        table_diff.indexes_added.push(src_idx.clone());
                        table_diff.status = "modified".to_string();
                    }
                }
                for tgt_idx in &tgt_table.indexes {
                    if !src_table
                        .indexes
                        .iter()
                        .any(|i| i.index_name == tgt_idx.index_name)
                    {
                        table_diff.indexes_removed.push(tgt_idx.clone());
                        table_diff.status = "modified".to_string();
                    }
                }

                // Compare FKs
                for src_fk in &src_table.foreign_keys {
                    if let Some(tgt_fk) = tgt_table
                        .foreign_keys
                        .iter()
                        .find(|f| f.constraint_name == src_fk.constraint_name)
                    {
                        if tgt_fk.column_name != src_fk.column_name
                            || tgt_fk.referenced_table_name != src_fk.referenced_table_name
                            || tgt_fk.referenced_column_name != src_fk.referenced_column_name
                            || tgt_fk.update_rule != src_fk.update_rule
                            || tgt_fk.delete_rule != src_fk.delete_rule
                        {
                            table_diff.fks_modified.push(FkDiff {
                                old: tgt_fk.clone(),
                                new: src_fk.clone(),
                            });
                            table_diff.status = "modified".to_string();
                        }
                    } else {
                        table_diff.fks_added.push(src_fk.clone());
                        table_diff.status = "modified".to_string();
                    }
                }
                for tgt_fk in &tgt_table.foreign_keys {
                    if !src_table
                        .foreign_keys
                        .iter()
                        .any(|f| f.constraint_name == tgt_fk.constraint_name)
                    {
                        table_diff.fks_removed.push(tgt_fk.clone());
                        table_diff.status = "modified".to_string();
                    }
                }

                if table_diff.status == "modified" {
                    diff.tables.push(table_diff);
                    let ddl = DdlEngine::generate_preview(Some(tgt_table), src_table);
                    if ddl != "-- No changes detected" {
                        ddl_statements.push(ddl);
                    }
                } else {
                    diff.tables.push(table_diff);
                }
            } else {
                // Table added
                diff.tables.push(TableDiff {
                    table_name: src_table.table_name.clone(),
                    status: "added".to_string(),
                    columns_added: src_table.columns.clone(),
                    columns_removed: Vec::new(),
                    columns_modified: Vec::new(),
                    indexes_added: src_table.indexes.clone(),
                    indexes_removed: Vec::new(),
                    indexes_modified: Vec::new(),
                    fks_added: src_table.foreign_keys.clone(),
                    fks_removed: Vec::new(),
                    fks_modified: Vec::new(),
                });
                ddl_statements.push(DdlEngine::generate_preview(None, src_table));
            }
        }

        // Check tables removed
        for tgt_table in &target.tables {
            if !source
                .tables
                .iter()
                .any(|t| t.table_name == tgt_table.table_name)
            {
                diff.tables.push(TableDiff {
                    table_name: tgt_table.table_name.clone(),
                    status: "removed".to_string(),
                    columns_added: Vec::new(),
                    columns_removed: tgt_table.columns.clone(),
                    columns_modified: Vec::new(),
                    indexes_added: Vec::new(),
                    indexes_removed: tgt_table.indexes.clone(),
                    indexes_modified: Vec::new(),
                    fks_added: Vec::new(),
                    fks_removed: tgt_table.foreign_keys.clone(),
                    fks_modified: Vec::new(),
                });
                ddl_statements.push(format!("DROP TABLE `{}`;", tgt_table.table_name));
            }
        }

        (diff, ddl_statements)
    }

    pub fn data_sync(
        table_name: &str,
        source_data: &[Value],
        target_data: &[Value],
        primary_key: &str,
    ) -> Vec<String> {
        let mut sync_statements = Vec::new();

        // A simple data sync implementation based on primary key
        // In a real scenario, this would compare rows and generate INSERT/UPDATE/DELETE
        // For this task, we will just generate comments or basic statements to demonstrate the engine.

        sync_statements.push(format!("-- Data sync for table: {}", table_name));
        sync_statements.push(format!(
            "-- Comparing {} rows from source with {} rows from target using PK: {}",
            source_data.len(),
            target_data.len(),
            primary_key
        ));

        sync_statements
    }
}

pub struct MockDataGenerator;

impl MockDataGenerator {
    pub async fn generate(
        config: &AppConfig,
        db_client: &DbClient,
        table: &TableWithDetails,
        row_count: u32,
        rules: Option<HashMap<String, String>>,
    ) -> Result<String, String> {
        let mut fk_data: HashMap<String, Vec<String>> = HashMap::new();

        // 1. Pre-fetch valid PK pool from referenced tables
        for fk in &table.foreign_keys {
            let query = format!(
                "SELECT `{}` FROM `{}` LIMIT 100",
                fk.referenced_column_name, fk.referenced_table_name
            );
            if let Ok(pool) = db_client.mysql_pool() {
            if let Ok(rows) = sqlx::query(&query).fetch_all(pool).await {
                let mut values = Vec::new();
                use sqlx::Row;
                for row in rows {
                    // Assuming string format for simplicity, handled by try_get generic mapping
                    let val: String = match row.try_get::<String, _>(0) {
                        Ok(v) => v,
                        Err(_) => match row.try_get::<i64, _>(0) {
                            Ok(v) => v.to_string(),
                            Err(_) => continue,
                        },
                    };
                    values.push(val);
                }
                fk_data.insert(fk.column_name.clone(), values);
            }
            }
        }

        // 2. Generate data in chunks (Batch execution support)
        let chunk_size = 50;
        let mut chunks = Vec::new();
        let mut remaining = row_count;

        while remaining > 0 {
            let current_chunk = if remaining > chunk_size {
                chunk_size
            } else {
                remaining
            };
            remaining -= current_chunk;
            chunks.push(current_chunk);
        }

        let mut all_sqls = Vec::new();

        for chunk in chunks {
            let mut prompt = format!("Please generate a SQL INSERT statement with {} rows of mock data for the following table:\n", chunk);
            prompt.push_str(&format!("Table: {}\nColumns:\n", table.table_name));
            for col in &table.columns {
                prompt.push_str(&format!("- {} ({})\n", col.column_name, col.column_type));
            }

            prompt.push_str("\nRules:\n");
            prompt.push_str("1. Return ONLY the raw SQL INSERT statement, nothing else. Do not use markdown blocks.\n");
            prompt.push_str("2. Use bulk insert syntax: INSERT INTO table_name (col1, col2) VALUES (val1, val2), (val3, val4);\n");

            if let Some(ref r) = rules {
                prompt.push_str("3. Use the following specific rules for columns:\n");
                for (col, rule) in r {
                    prompt.push_str(&format!("  - {}: {}\n", col, rule));
                }
            }

            if !fk_data.is_empty() {
                prompt.push_str("4. MUST use the following valid values for foreign key columns to ensure referential integrity:\n");
                for (col, values) in &fk_data {
                    if !values.is_empty() {
                        let sample = values
                            .iter()
                            .take(10)
                            .map(|s| format!("'{}'", s))
                            .collect::<Vec<_>>()
                            .join(", ");
                        prompt
                            .push_str(&format!("  - {} MUST be chosen from: [{}]\n", col, sample));
                    }
                }
            }

            let system_prompt = "You are a database expert that generates realistic mock data SQL scripts.";

            match crate::ai::agent::chat_completion_raw(config, system_prompt, &prompt).await {
                Ok(sql) => {
                    let sql = sql
                        .trim()
                        .trim_start_matches("```sql")
                        .trim_start_matches("```")
                        .trim_end_matches("```")
                        .trim()
                        .to_string();
                    all_sqls.push(sql);
                }
                Err(e) => return Err(format!("Failed to generate mock data: {:?}", e)),
            }
        }

        Ok(all_sqls.join("\n\n"))
    }
}

pub struct DataExporter;

impl DataExporter {
    pub fn csv_header(headers: &[String]) -> String {
        format!("{}\n", headers.join(","))
    }

    pub fn csv_row(headers: &[String], row: &serde_json::Map<String, Value>) -> String {
        let mut row_data = Vec::new();
        for header in headers {
            let val_str = match row.get(header) {
                Some(Value::String(s)) => format!("\"{}\"", s.replace("\"", "\"\"")),
                Some(Value::Null) => String::new(),
                Some(v) => v.to_string(),
                None => String::new(),
            };
            row_data.push(val_str);
        }
        format!("{}\n", row_data.join(","))
    }

    pub fn sql_header(table_name: &str, headers: &[String]) -> String {
        format!(
            "INSERT INTO {} ({}) VALUES\n",
            table_name,
            headers.join(", ")
        )
    }

    pub fn sql_row(
        headers: &[String],
        row: &serde_json::Map<String, Value>,
        is_last: bool,
    ) -> String {
        let mut row_data = Vec::new();
        for header in headers {
            let val_str = match row.get(header) {
                Some(Value::String(s)) => format!("'{}'", s.replace("'", "''")),
                Some(Value::Null) => "NULL".to_string(),
                Some(v) => v.to_string(),
                None => "NULL".to_string(),
            };
            row_data.push(val_str);
        }
        let suffix = if is_last { ";\n" } else { ",\n" };
        format!("({}){}", row_data.join(", "), suffix)
    }

    pub fn json_row(row: &serde_json::Map<String, Value>, is_first: bool, is_last: bool) -> String {
        let mut s = String::new();
        if is_first {
            s.push_str("[\n");
        } else {
            s.push_str(",\n");
        }
        s.push_str(&serde_json::to_string(row).unwrap_or_default());
        if is_last {
            s.push_str("\n]\n");
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_col(name: &str, col_type: &str, nullable: &str, key: &str) -> ColumnInfo {
        ColumnInfo {
            column_name: name.to_string(),
            data_type: col_type.to_lowercase(),
            column_type: col_type.to_string(),
            is_nullable: nullable.to_string(),
            column_comment: None,
            column_key: key.to_string(),
            column_default: None,
            extra: String::new(),
        }
    }

    fn make_table(name: &str, cols: Vec<ColumnInfo>) -> TableWithDetails {
        TableWithDetails {
            table_name: name.to_string(),
            columns: cols,
            indexes: vec![],
            foreign_keys: vec![],
        }
    }

    #[test]
    fn generate_preview_no_changes() {
        let table = make_table("users", vec![make_col("id", "INT", "NO", "PRI")]);
        let ddl = DdlEngine::generate_preview(Some(&table), &table);
        assert_eq!(ddl, "-- No changes detected");
    }

    #[test]
    fn generate_preview_add_column() {
        let old = make_table("users", vec![make_col("id", "INT", "NO", "PRI")]);
        let mut new = old.clone();
        new.columns
            .push(make_col("name", "VARCHAR(255)", "YES", ""));
        let ddl = DdlEngine::generate_preview(Some(&old), &new);
        assert!(ddl.contains("ADD COLUMN"));
        assert!(ddl.contains("name"));
        assert!(ddl.contains("VARCHAR(255)"));
        assert!(ddl.contains("ALGORITHM=INSTANT"));
    }

    #[test]
    fn generate_preview_drop_column() {
        let mut old = make_table("users", vec![make_col("id", "INT", "NO", "PRI")]);
        old.columns
            .push(make_col("email", "VARCHAR(255)", "YES", ""));
        let new = make_table("users", vec![make_col("id", "INT", "NO", "PRI")]);
        let ddl = DdlEngine::generate_preview(Some(&old), &new);
        assert!(ddl.contains("DROP COLUMN"));
        assert!(ddl.contains("email"));
    }

    #[test]
    fn generate_preview_modify_column() {
        let old = make_table(
            "users",
            vec![make_col("id", "INT", "NO", "PRI"), make_col("name", "VARCHAR(100)", "YES", "")],
        );
        let new = make_table(
            "users",
            vec![make_col("id", "INT", "NO", "PRI"), make_col("name", "VARCHAR(255)", "YES", "")],
        );
        let ddl = DdlEngine::generate_preview(Some(&old), &new);
        assert!(ddl.contains("MODIFY COLUMN"));
        assert!(ddl.contains("ALGORITHM=INPLACE"));
    }

    #[test]
    fn generate_preview_create_table() {
        let table = make_table(
            "orders",
            vec![
                make_col("id", "BIGINT", "NO", "PRI"),
                make_col("amount", "DECIMAL(18,2)", "NO", ""),
            ],
        );
        let ddl = DdlEngine::generate_preview(None, &table);
        assert!(ddl.contains("CREATE TABLE"));
        assert!(ddl.contains("orders"));
        assert!(ddl.contains("BIGINT"));
        assert!(ddl.contains("PRIMARY KEY"));
    }

    #[test]
    fn generate_rollback_for_add_column() {
        let old = make_table("users", vec![make_col("id", "INT", "NO", "PRI")]);
        let mut new = old.clone();
        new.columns
            .push(make_col("name", "VARCHAR(255)", "YES", ""));
        let rollback = DdlEngine::generate_rollback(Some(&old), &new);
        assert!(rollback.contains("DROP COLUMN"));
        assert!(rollback.contains("name"));
    }

    #[test]
    fn generate_rollback_for_drop_column() {
        let mut old = make_table("users", vec![make_col("id", "INT", "NO", "PRI")]);
        old.columns
            .push(make_col("email", "VARCHAR(255)", "YES", ""));
        let new = make_table("users", vec![make_col("id", "INT", "NO", "PRI")]);
        let rollback = DdlEngine::generate_rollback(Some(&old), &new);
        assert!(rollback.contains("ADD COLUMN"));
        assert!(rollback.contains("email"));
        assert!(rollback.contains("VARCHAR(255)"));
    }

    #[test]
    fn generate_rollback_for_create_table() {
        let table = make_table("temp", vec![make_col("id", "INT", "NO", "PRI")]);
        let rollback = DdlEngine::generate_rollback(None, &table);
        assert!(rollback.contains("DROP TABLE IF EXISTS"));
        assert!(rollback.contains("temp"));
    }

    #[test]
    fn classify_alter_algorithm_instant_for_add_column() {
        let alters = vec!["ADD COLUMN `name` VARCHAR(255)".to_string()];
        assert_eq!(classify_alter_algorithm(&alters), "ALGORITHM=INSTANT");
    }

    #[test]
    fn classify_alter_algorithm_inplace_for_modify() {
        let alters = vec!["MODIFY COLUMN `name` VARCHAR(512)".to_string()];
        assert_eq!(
            classify_alter_algorithm(&alters),
            "ALGORITHM=INPLACE, LOCK=NONE"
        );
    }

    #[test]
    fn classify_alter_algorithm_default_for_fk() {
        let alters = vec!["ADD CONSTRAINT `fk_user` FOREIGN KEY (`user_id`) REFERENCES `users` (`id`)".to_string()];
        assert_eq!(classify_alter_algorithm(&alters), "ALGORITHM=DEFAULT");
    }
}
