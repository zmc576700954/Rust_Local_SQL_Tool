use crate::schema::SchemaResponse;
use crate::tools::DdlEngine;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

pub struct SchemaSyncEngine;

impl SchemaSyncEngine {
    pub fn generate_ddl_for_selection(
        source: &SchemaResponse,
        target: &SchemaResponse,
        selected_tables: &[String],
    ) -> String {
        let (diff, _) = crate::tools::SyncEngine::schema_sync(source, target);
        let mut ddl_statements = Vec::new();

        for table_name in selected_tables {
            if let Some(table_diff) = diff.tables.iter().find(|t| &t.table_name == table_name) {
                if table_diff.status == "added" {
                    if let Some(src_table) =
                        source.tables.iter().find(|t| &t.table_name == table_name)
                    {
                        ddl_statements.push(DdlEngine::generate_preview(None, src_table));
                    }
                } else if table_diff.status == "removed" {
                    ddl_statements.push(format!("DROP TABLE `{}`;", table_name));
                } else if table_diff.status == "modified" {
                    if let Some(src_table) =
                        source.tables.iter().find(|t| &t.table_name == table_name)
                    {
                        if let Some(tgt_table) =
                            target.tables.iter().find(|t| &t.table_name == table_name)
                        {
                            let ddl = DdlEngine::generate_preview(Some(tgt_table), src_table);
                            if ddl != "-- No changes detected" {
                                ddl_statements.push(ddl);
                            }
                        }
                    }
                }
            }
        }

        ddl_statements.join("\n\n")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataDiff {
    pub table_name: String,
    pub insert_count: usize,
    pub update_count: usize,
    pub delete_count: usize,
    // Detailed rows can be included or skipped. We'll just include counts and the diffed rows for generating SQL.
    pub inserts: Vec<Value>,
    pub updates: Vec<(Value, Value)>, // (old, new)
    pub deletes: Vec<Value>,
}

pub struct DataSyncEngine;

impl DataSyncEngine {
    pub fn compute_data_diff(
        table_name: &str,
        source_data: &[Value],
        target_data: &[Value],
        primary_key: &str,
    ) -> DataDiff {
        let mut source_map: HashMap<String, &Value> = HashMap::new();
        let mut target_map: HashMap<String, &Value> = HashMap::new();

        for row in source_data {
            if let Some(pk_val) = row.get(primary_key) {
                source_map.insert(pk_val.to_string(), row);
            }
        }

        for row in target_data {
            if let Some(pk_val) = row.get(primary_key) {
                target_map.insert(pk_val.to_string(), row);
            }
        }

        let mut inserts = Vec::new();
        let mut updates = Vec::new();
        let mut deletes = Vec::new();

        for (pk, src_row) in &source_map {
            if let Some(tgt_row) = target_map.get(pk) {
                // Check for updates
                if src_row != tgt_row {
                    updates.push(((*tgt_row).clone(), (*src_row).clone()));
                }
            } else {
                // Present in source, not in target -> Insert
                inserts.push((*src_row).clone());
            }
        }

        for (pk, tgt_row) in &target_map {
            if !source_map.contains_key(pk) {
                // Present in target, not in source -> Delete
                deletes.push((*tgt_row).clone());
            }
        }

        DataDiff {
            table_name: table_name.to_string(),
            insert_count: inserts.len(),
            update_count: updates.len(),
            delete_count: deletes.len(),
            inserts,
            updates,
            deletes,
        }
    }

    pub fn generate_dml_for_selection(
        diffs: &[DataDiff],
        selections: &HashMap<String, Vec<String>>, // table_name -> ["insert", "update", "delete"]
        primary_key: &str,
    ) -> String {
        let mut dml_statements = Vec::new();

        for diff in diffs {
            if let Some(ops) = selections.get(&diff.table_name) {
                if ops.contains(&"delete".to_string()) && diff.delete_count > 0 {
                    for row in &diff.deletes {
                        if let Some(pk_val) = row.get(primary_key) {
                            let val_str = if let Some(s) = pk_val.as_str() {
                                format!("'{}'", s.replace("'", "''"))
                            } else {
                                pk_val.to_string()
                            };
                            dml_statements.push(format!(
                                "DELETE FROM `{}` WHERE `{}` = {};",
                                diff.table_name, primary_key, val_str
                            ));
                        }
                    }
                }

                if ops.contains(&"insert".to_string()) && diff.insert_count > 0 {
                    for row in &diff.inserts {
                        if let Some(obj) = row.as_object() {
                            let mut cols = Vec::new();
                            let mut vals = Vec::new();
                            for (k, v) in obj {
                                cols.push(format!("`{}`", k));
                                vals.push(Self::format_value(v));
                            }
                            dml_statements.push(format!(
                                "INSERT INTO `{}` ({}) VALUES ({});",
                                diff.table_name,
                                cols.join(", "),
                                vals.join(", ")
                            ));
                        }
                    }
                }

                if ops.contains(&"update".to_string()) && diff.update_count > 0 {
                    for (_old_row, new_row) in &diff.updates {
                        if let Some(obj) = new_row.as_object() {
                            let mut sets = Vec::new();
                            for (k, v) in obj {
                                sets.push(format!("`{}` = {}", k, Self::format_value(v)));
                            }
                            if let Some(pk_val) = obj.get(primary_key) {
                                let pk_str = Self::format_value(pk_val);
                                dml_statements.push(format!(
                                    "UPDATE `{}` SET {} WHERE `{}` = {};",
                                    diff.table_name,
                                    sets.join(", "),
                                    primary_key,
                                    pk_str
                                ));
                            }
                        }
                    }
                }
            }
        }

        if dml_statements.is_empty() {
            return "-- No changes selected".to_string();
        }

        dml_statements.join("\n")
    }

    fn format_value(v: &Value) -> String {
        match v {
            Value::Null => "NULL".to_string(),
            Value::Bool(b) => {
                if *b {
                    "TRUE".to_string()
                } else {
                    "FALSE".to_string()
                }
            }
            Value::Number(n) => n.to_string(),
            Value::String(s) => format!("'{}'", s.replace("'", "''")),
            Value::Array(a) => format!(
                "'{}'",
                serde_json::to_string(a)
                    .unwrap_or_default()
                    .replace("'", "''")
            ),
            Value::Object(o) => format!(
                "'{}'",
                serde_json::to_string(o)
                    .unwrap_or_default()
                    .replace("'", "''")
            ),
        }
    }
}
