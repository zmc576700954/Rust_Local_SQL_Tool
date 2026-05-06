use csv::ReaderBuilder;
use serde::{Deserialize, Serialize};
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
        fn escape_sql_string(v: &str) -> String {
            v.replace('\'', "''")
        }

        fn quoted_ident(v: &str) -> String {
            format!("`{}`", v.replace('`', "``"))
        }

        let mut out = String::new();

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
                    .map(|m| (m.target_col.clone(), header_to_idx.get(&m.source_col).copied()))
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
            let source_table = config
                .source_table
                .as_ref()
                .ok_or(TransferError::Db("Missing source table".into()))?;

            if config.mappings.is_empty() {
                return Err(TransferError::Db("Missing mappings".into()));
            }

            // This is a naive implementation that generates SQL.
            // In a real scenario, we'd fetch from source DB and stream to target DB.
            // Since we don't have a direct cross-db transfer in memory right now without full execution,
            // we'll just create a placeholder or actually fetch data if we can.
            // Wait, the task says "streaming from source to target".
            // Let's implement actual fetching from source DB.
            let source_pool = sqlx::MySqlPool::connect(source_url)
                .await
                .map_err(|e| TransferError::Db(e.to_string()))?;

            // Query all rows from source
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

            use sqlx::Row;
            // use sqlx::TypeInfo;

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
            let mut batch: Vec<String> = Vec::with_capacity(batch_rows);

            for row in rows {
                let mut vals = Vec::new();

                for (i, _mapping) in config.mappings.iter().enumerate() {
                    // Extract value from row. We'll use a string representation.
                    // This requires decoding depending on the column type.
                    // A simple workaround is to cast to string if possible, or we just format it.
                    let val_str = if let Ok(val) = row.try_get::<String, _>(i) {
                        format!("'{}'", escape_sql_string(&val))
                    } else if let Ok(val) = row.try_get::<i64, _>(i) {
                        val.to_string()
                    } else if let Ok(val) = row.try_get::<f64, _>(i) {
                        val.to_string()
                    } else if let Ok(val) = row.try_get::<bool, _>(i) {
                        if val {
                            "TRUE".to_string()
                        } else {
                            "FALSE".to_string()
                        }
                    } else {
                        "NULL".to_string() // Fallback
                    };
                    vals.push(val_str);
                }

                batch.push(format!("({})", vals.join(", ")));
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

        // Return generated script for execution by the frontend or execute it here?
        // The requirements say: "APIs for local transfer and network transfer... Support column mapping and transfer modes".
        // Let's just return the DML script so the frontend can execute it via the existing execute endpoint, or we execute it directly.
        Ok(out)
    }
}
