use csv::ReaderBuilder;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::Row;
use std::fs::File;
use std::io::Read;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TransferError {
    #[error("IO Error: {0}")]
    Io(#[from] std::io::Error),
    #[error("CSV Error: {0}")]
    Csv(#[from] csv::Error),
    #[error("Unsupported file type")]
    UnsupportedFileType,
    #[error("Database Error: {0}")]
    Db(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TransferMode {
    Append,
    Replace, // Typically Truncate then Insert
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnMapping {
    pub source_col: String,
    pub target_col: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferConfig {
    pub source_type: String, // "local_file" or "network_db"
    pub source_path: Option<String>,
    pub source_url: Option<String>,
    pub source_db_id: Option<String>,
    pub source_table: Option<String>,

    pub target_db_id: Option<String>,
    pub target_url: String,
    pub target_table: String,

    pub mode: TransferMode,
    pub mappings: Vec<ColumnMapping>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FileParseResult {
    pub columns: Vec<String>,
    pub preview_data: Vec<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferExecuteReport {
    pub dml: String,
    pub insert_count: usize,
    pub update_count: usize,
    pub unchanged_count: usize,
    pub compare_based: bool,
}

pub struct TransferEngine;

impl TransferEngine {
    /// Parse a local file (CSV for now, can be extended for TXT/SQL)
    pub fn parse_local_file(
        path: &str,
        delimiter: u8,
        has_headers: bool,
    ) -> Result<FileParseResult, TransferError> {
        let file = File::open(path)?;
        let mut rdr = ReaderBuilder::new()
            .delimiter(delimiter)
            .has_headers(has_headers)
            .from_reader(file);

        let mut columns = Vec::new();
        if has_headers {
            if let Ok(headers) = rdr.headers() {
                for header in headers {
                    columns.push(header.to_string());
                }
            }
        } else {
            // Generate dummy column names
            if let Some(Ok(record)) = rdr.records().next() {
                for i in 0..record.len() {
                    columns.push(format!("col_{}", i + 1));
                }
            }
            // Reset reader if no headers to read the first row as data
            // But since we consumed it, we'd need to recreate or seek. Let's just recreate.
        }

        let file = File::open(path)?;
        let mut rdr = ReaderBuilder::new()
            .delimiter(delimiter)
            .has_headers(has_headers)
            .from_reader(file);

        let mut preview_data = Vec::new();
        for (i, result) in rdr.records().enumerate() {
            if i >= 5 {
                // preview up to 5 rows
                break;
            }
            if let Ok(record) = result {
                let mut row = Vec::new();
                for field in record.iter() {
                    row.push(field.to_string());
                }
                preview_data.push(row);
            }
        }

        Ok(FileParseResult {
            columns,
            preview_data,
        })
    }

    pub async fn execute_transfer(config: &TransferConfig) -> Result<String, TransferError> {
        let report = Self::execute_transfer_with_report(config).await?;
        Ok(report.dml)
    }

    pub async fn execute_transfer_with_report(
        config: &TransferConfig,
    ) -> Result<TransferExecuteReport, TransferError> {
        fn escape_sql_string(v: &str) -> String {
            v.replace('\'', "''")
        }

        fn quoted_ident(v: &str) -> String {
            format!("`{}`", v.replace('`', "``"))
        }

        fn value_to_sql(v: &Value) -> String {
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
                Value::String(s) => format!("'{}'", s.replace('\'', "''")),
                Value::Array(a) => format!(
                    "'{}'",
                    serde_json::to_string(a)
                        .unwrap_or_default()
                        .replace('\'', "''")
                ),
                Value::Object(o) => format!(
                    "'{}'",
                    serde_json::to_string(o)
                        .unwrap_or_default()
                        .replace('\'', "''")
                ),
            }
        }

        fn row_cell_to_value(row: &sqlx::mysql::MySqlRow, idx: usize) -> Value {
            if let Ok(v) = row.try_get::<Option<i64>, _>(idx) {
                return serde_json::json!(v);
            }
            if let Ok(v) = row.try_get::<Option<f64>, _>(idx) {
                return serde_json::json!(v);
            }
            if let Ok(v) = row.try_get::<Option<bool>, _>(idx) {
                return serde_json::json!(v);
            }
            if let Ok(v) = row.try_get::<Option<chrono::NaiveDateTime>, _>(idx) {
                return serde_json::json!(v.map(|x| x.to_string()));
            }
            if let Ok(v) = row.try_get::<Option<chrono::NaiveDate>, _>(idx) {
                return serde_json::json!(v.map(|x| x.to_string()));
            }
            if let Ok(v) = row.try_get::<Option<chrono::NaiveTime>, _>(idx) {
                return serde_json::json!(v.map(|x| x.to_string()));
            }
            if let Ok(v) = row.try_get::<Option<String>, _>(idx) {
                return serde_json::json!(v);
            }
            if let Ok(v) = row.try_get::<Option<Vec<u8>>, _>(idx) {
                if let Some(bytes) = v {
                    return serde_json::json!(String::from_utf8_lossy(&bytes).to_string());
                }
            }
            Value::Null
        }

        fn value_key(v: &Value) -> String {
            match v {
                Value::Null => "null".to_string(),
                Value::Bool(b) => b.to_string(),
                Value::Number(n) => n.to_string(),
                Value::String(s) => s.clone(),
                Value::Array(a) => serde_json::to_string(a).unwrap_or_default(),
                Value::Object(o) => serde_json::to_string(o).unwrap_or_default(),
            }
        }

        let mut out = String::new();
        let mut insert_count = 0usize;
        let mut update_count = 0usize;
        let mut unchanged_count = 0usize;
        let mut compare_based = false;

        if let TransferMode::Replace = config.mode {
            out.push_str("TRUNCATE TABLE ");
            out.push_str(&quoted_ident(&config.target_table));
            out.push_str(";\n");
        }

        if config.source_type == "local_file" {
            let path = config
                .source_path
                .as_ref()
                .ok_or(TransferError::Db("Missing source path".into()))?;
            let file = File::open(path)?;

            // Check if it's a SQL file
            let mut file_clone = File::open(path)?;
            let mut first_bytes = [0u8; 1024];
            let n = file_clone.read(&mut first_bytes).unwrap_or(0);
            let content_preview = String::from_utf8_lossy(&first_bytes[..n]);
            let is_sql = content_preview.to_lowercase().contains("insert into")
                || content_preview.to_lowercase().contains("update ")
                || content_preview.to_lowercase().contains("create table");

            if is_sql || path.ends_with(".sql") {
                let mut content = String::new();
                File::open(path)?.read_to_string(&mut content)?;
                out.push_str(&content);
                if !out.ends_with('\n') {
                    out.push('\n');
                }
            } else {
                if config.mappings.is_empty() {
                    return Err(TransferError::Db("Missing mappings".into()));
                }

                let mut rdr = ReaderBuilder::new()
                    .has_headers(true) // Assuming headers are true for now
                    .from_reader(file);

                let headers = rdr.headers()?.clone();
                let mut header_to_idx = std::collections::HashMap::new();
                for (i, h) in headers.iter().enumerate() {
                    header_to_idx.insert(h.to_string(), i);
                }

                let mapping_idx: Vec<(String, Option<usize>)> = config
                    .mappings
                    .iter()
                    .map(|m| {
                        (
                            m.target_col.clone(),
                            header_to_idx.get(&m.source_col).copied(),
                        )
                    })
                    .collect();

                let insert_cols: Vec<String> = mapping_idx
                    .iter()
                    .map(|(tgt, _)| quoted_ident(tgt))
                    .collect();

                let target_ident = quoted_ident(&config.target_table);
                let batch_rows: usize = std::env::var("LOCAL_AI_SQL_TRANSFER_INSERT_BATCH_ROWS")
                    .ok()
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or(500)
                    .max(1);

                let mut batch: Vec<String> = Vec::with_capacity(batch_rows);

                for result in rdr.records() {
                    let record = result?;
                    let mut vals: Vec<String> = Vec::with_capacity(mapping_idx.len());
                    for (_tgt, idx_opt) in &mapping_idx {
                        if let Some(idx) = idx_opt {
                            let v = record.get(*idx).unwrap_or_default();
                            vals.push(format!("'{}'", escape_sql_string(v)));
                        } else {
                            vals.push("NULL".to_string());
                        }
                    }
                    batch.push(format!("({})", vals.join(", ")));
                    insert_count += 1;

                    if batch.len() >= batch_rows {
                        out.push_str("INSERT INTO ");
                        out.push_str(&target_ident);
                        out.push_str(" (");
                        out.push_str(&insert_cols.join(", "));
                        out.push_str(") VALUES ");
                        out.push_str(&batch.join(", "));
                        out.push_str(";\n");
                        batch.clear();
                    }
                }

                if !batch.is_empty() {
                    out.push_str("INSERT INTO ");
                    out.push_str(&target_ident);
                    out.push_str(" (");
                    out.push_str(&insert_cols.join(", "));
                    out.push_str(") VALUES ");
                    out.push_str(&batch.join(", "));
                    out.push_str(";\n");
                }
            }
        } else if config.source_type == "network_db" {
            let source_url = config
                .source_url
                .as_ref()
                .ok_or(TransferError::Db("Missing source url".into()))?;
            let target_url = if !config.target_url.is_empty() {
                config.target_url.clone()
            } else {
                return Err(TransferError::Db("Missing target url".into()));
            };
            let source_table = config
                .source_table
                .as_ref()
                .ok_or(TransferError::Db("Missing source table".into()))?;

            if config.mappings.is_empty() {
                return Err(TransferError::Db("Missing mappings".into()));
            }

            let source_pool = sqlx::MySqlPool::connect(source_url)
                .await
                .map_err(|e| TransferError::Db(e.to_string()))?;
            let target_pool = sqlx::MySqlPool::connect(&target_url)
                .await
                .map_err(|e| TransferError::Db(e.to_string()))?;

            let source_cols: Vec<String> = config
                .mappings
                .iter()
                .map(|m| quoted_ident(&m.source_col))
                .collect();
            let query = format!("SELECT {} FROM `{}`", source_cols.join(", "), source_table);

            let rows = sqlx::query(&query)
                .fetch_all(&source_pool)
                .await
                .map_err(|e| TransferError::Db(e.to_string()))?;

            let insert_cols: Vec<String> = config
                .mappings
                .iter()
                .map(|m| quoted_ident(&m.target_col))
                .collect();

            let target_ident = quoted_ident(&config.target_table);
            let batch_rows: usize = std::env::var("LOCAL_AI_SQL_TRANSFER_INSERT_BATCH_ROWS")
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(500)
                .max(1);
            let pk_idx = config
                .mappings
                .iter()
                .position(|m| m.target_col.eq_ignore_ascii_case("id"));
            let target_cols: Vec<String> = config
                .mappings
                .iter()
                .map(|m| quoted_ident(&m.target_col))
                .collect();

            if let Some(pk_idx) = pk_idx {
                compare_based = true;
                let target_query = format!(
                    "SELECT {} FROM `{}`",
                    target_cols.join(", "),
                    config.target_table
                );
                let target_rows = sqlx::query(&target_query)
                    .fetch_all(&target_pool)
                    .await
                    .unwrap_or_default();

                let mut target_map: std::collections::HashMap<String, Vec<Value>> =
                    std::collections::HashMap::new();
                for row in target_rows {
                    let mut vals = Vec::with_capacity(config.mappings.len());
                    for i in 0..config.mappings.len() {
                        vals.push(row_cell_to_value(&row, i));
                    }
                    let pk = value_key(vals.get(pk_idx).unwrap_or(&Value::Null));
                    target_map.insert(pk, vals);
                }

                for row in rows {
                    let mut src_vals = Vec::with_capacity(config.mappings.len());
                    for i in 0..config.mappings.len() {
                        src_vals.push(row_cell_to_value(&row, i));
                    }
                    let pk = value_key(src_vals.get(pk_idx).unwrap_or(&Value::Null));
                    if let Some(tgt_vals) = target_map.get(&pk) {
                        if *tgt_vals == src_vals {
                            unchanged_count += 1;
                            continue;
                        }
                        update_count += 1;
                        let mut sets = Vec::new();
                        for (i, m) in config.mappings.iter().enumerate() {
                            if i == pk_idx {
                                continue;
                            }
                            sets.push(format!(
                                "{} = {}",
                                quoted_ident(&m.target_col),
                                value_to_sql(&src_vals[i])
                            ));
                        }
                        out.push_str("UPDATE ");
                        out.push_str(&target_ident);
                        out.push_str(" SET ");
                        out.push_str(&sets.join(", "));
                        out.push_str(" WHERE ");
                        out.push_str(&quoted_ident(&config.mappings[pk_idx].target_col));
                        out.push_str(" = ");
                        out.push_str(&value_to_sql(&src_vals[pk_idx]));
                        out.push_str(";\n");
                    } else {
                        insert_count += 1;
                        let vals = src_vals.iter().map(value_to_sql).collect::<Vec<_>>();
                        out.push_str("INSERT INTO ");
                        out.push_str(&target_ident);
                        out.push_str(" (");
                        out.push_str(&insert_cols.join(", "));
                        out.push_str(") VALUES (");
                        out.push_str(&vals.join(", "));
                        out.push_str(");\n");
                    }
                }
            } else {
                let mut batch: Vec<String> = Vec::with_capacity(batch_rows);
                for row in rows {
                    let mut vals = Vec::new();
                    for i in 0..config.mappings.len() {
                        let v = row_cell_to_value(&row, i);
                        vals.push(value_to_sql(&v));
                    }
                    batch.push(format!("({})", vals.join(", ")));
                    insert_count += 1;
                    if batch.len() >= batch_rows {
                        out.push_str("INSERT INTO ");
                        out.push_str(&target_ident);
                        out.push_str(" (");
                        out.push_str(&insert_cols.join(", "));
                        out.push_str(") VALUES ");
                        out.push_str(&batch.join(", "));
                        out.push_str(";\n");
                        batch.clear();
                    }
                }
                if !batch.is_empty() {
                    out.push_str("INSERT INTO ");
                    out.push_str(&target_ident);
                    out.push_str(" (");
                    out.push_str(&insert_cols.join(", "));
                    out.push_str(") VALUES ");
                    out.push_str(&batch.join(", "));
                    out.push_str(";\n");
                }
            }
        }

        Ok(TransferExecuteReport {
            dml: out,
            insert_count,
            update_count,
            unchanged_count,
            compare_based,
        })
    }
}
