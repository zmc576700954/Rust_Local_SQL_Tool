use crate::db::DbClient;
use crate::error::AppError;
use crate::timeout_policy::TimeoutPolicy;
use crc32fast::Hasher;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{Column, Row};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SyncMode {
    Mirror,
    UpsertOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PkRange {
    pub start: Option<String>,
    pub start_inclusive: bool,
    pub end: Option<String>,
    pub end_inclusive: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkChecksum {
    pub range: PkRange,
    pub source_count: usize,
    pub target_count: usize,
    pub source_crc32: u32,
    pub target_crc32: u32,
    pub equal: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompareResult {
    pub table_name: String,
    pub primary_key: String,
    pub chunk_size: usize,
    pub chunks: Vec<ChunkChecksum>,
    pub different_chunks: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RowDiff {
    pub table_name: String,
    pub primary_key: String,
    pub mode: SyncMode,
    pub insert_count: usize,
    pub update_count: usize,
    pub delete_count: usize,
    pub inserts: Vec<Value>,
    pub updates: Vec<(Value, Value)>,
    pub deletes: Vec<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreviewResult {
    pub diff: RowDiff,
    pub statements: Vec<String>,
    pub sql: String,
    pub truncated: bool,
}

pub struct MySqlDataSyncEngine;

impl MySqlDataSyncEngine {
    pub async fn compare(
        source: &DbClient,
        target: &DbClient,
        table_name: &str,
        primary_key: &str,
        chunk_size: usize,
    ) -> Result<CompareResult, AppError> {
        let chunk_size = chunk_size.max(1);
        let (source_min, source_max) = fetch_min_max_pk(source, table_name, primary_key).await?;
        let (target_min, target_max) = fetch_min_max_pk(target, table_name, primary_key).await?;

        let mut ranges = Vec::new();

        if source_min.is_none() && target_min.is_none() {
            return Ok(CompareResult {
                table_name: table_name.to_string(),
                primary_key: primary_key.to_string(),
                chunk_size,
                chunks: vec![],
                different_chunks: 0,
            });
        }

        if let (Some(t_min), Some(s_min)) = (target_min.clone(), source_min.clone()) {
            if compare_pk_str(&t_min, &s_min) == std::cmp::Ordering::Less {
                ranges.push(PkRange {
                    start: None,
                    start_inclusive: true,
                    end: Some(s_min),
                    end_inclusive: false,
                });
            }
        }

        if let Some(_s_min) = source_min.clone() {
            let mut last_pk: Option<String> = None;
            loop {
                let pk_list = fetch_pk_list_after(
                    source,
                    table_name,
                    primary_key,
                    last_pk.clone(),
                    chunk_size,
                )
                .await?;
                if pk_list.is_empty() {
                    break;
                }
                let start = pk_list.first().cloned();
                let end = pk_list.last().cloned();
                if let (Some(start), Some(end)) = (start, end.clone()) {
                    ranges.push(PkRange {
                        start: Some(start),
                        start_inclusive: true,
                        end: Some(end.clone()),
                        end_inclusive: true,
                    });
                    last_pk = Some(end);
                } else {
                    break;
                }
            }
        } else {
            ranges.push(PkRange {
                start: None,
                start_inclusive: true,
                end: None,
                end_inclusive: true,
            });
        }

        if let (Some(t_max), Some(s_max)) = (target_max.clone(), source_max.clone()) {
            if compare_pk_str(&t_max, &s_max) == std::cmp::Ordering::Greater {
                ranges.push(PkRange {
                    start: Some(s_max),
                    start_inclusive: false,
                    end: None,
                    end_inclusive: true,
                });
            }
        }

        let mut chunks = Vec::new();
        let mut different_chunks = 0usize;

        for range in ranges {
            let source_rows = fetch_rows_in_range(source, table_name, primary_key, &range).await?;
            let target_rows = fetch_rows_in_range(target, table_name, primary_key, &range).await?;

            let source_crc32 = checksum_rows(primary_key, &source_rows);
            let target_crc32 = checksum_rows(primary_key, &target_rows);
            let equal = source_crc32 == target_crc32 && source_rows.len() == target_rows.len();
            if !equal {
                different_chunks += 1;
            }

            chunks.push(ChunkChecksum {
                range,
                source_count: source_rows.len(),
                target_count: target_rows.len(),
                source_crc32,
                target_crc32,
                equal,
            });
        }

        Ok(CompareResult {
            table_name: table_name.to_string(),
            primary_key: primary_key.to_string(),
            chunk_size,
            chunks,
            different_chunks,
        })
    }

    pub async fn preview(
        source: &DbClient,
        target: &DbClient,
        compare: &CompareResult,
        mode: SyncMode,
        max_rows: usize,
        actions: Option<Vec<String>>,
    ) -> Result<PreviewResult, AppError> {
        let max_rows = max_rows.max(1);
        let mut inserts = Vec::new();
        let mut updates = Vec::new();
        let mut deletes = Vec::new();
        let mut truncated = false;

        let mut allow_insert = true;
        let mut allow_update = true;
        let mut allow_delete = mode == SyncMode::Mirror;
        if let Some(actions) = actions {
            let actions: std::collections::HashSet<String> =
                actions.into_iter().map(|s| s.to_lowercase()).collect();
            allow_insert = actions.contains("insert");
            allow_update = actions.contains("update");
            allow_delete = allow_delete && actions.contains("delete");
        }

        for chunk in &compare.chunks {
            if chunk.equal {
                continue;
            }

            let source_rows = fetch_rows_in_range(
                source,
                &compare.table_name,
                &compare.primary_key,
                &chunk.range,
            )
            .await?;
            let target_rows = fetch_rows_in_range(
                target,
                &compare.table_name,
                &compare.primary_key,
                &chunk.range,
            )
            .await?;

            let mut source_map: HashMap<String, Value> = HashMap::new();
            let mut target_map: HashMap<String, Value> = HashMap::new();

            for row in source_rows {
                if let Some(pk) = extract_pk_to_string(&row, &compare.primary_key) {
                    source_map.insert(pk, row);
                }
            }
            for row in target_rows {
                if let Some(pk) = extract_pk_to_string(&row, &compare.primary_key) {
                    target_map.insert(pk, row);
                }
            }

            for (pk, src_row) in &source_map {
                if inserts.len() + updates.len() + deletes.len() >= max_rows {
                    truncated = true;
                    break;
                }
                if let Some(tgt_row) = target_map.get(pk) {
                    if src_row != tgt_row {
                        updates.push((tgt_row.clone(), src_row.clone()));
                    }
                } else {
                    inserts.push(src_row.clone());
                }
            }
            if truncated {
                break;
            }

            if mode == SyncMode::Mirror {
                for (pk, tgt_row) in &target_map {
                    if deletes.len() + inserts.len() + updates.len() >= max_rows {
                        truncated = true;
                        break;
                    }
                    if !source_map.contains_key(pk) {
                        deletes.push(tgt_row.clone());
                    }
                }
            }
            if truncated {
                break;
            }
        }

        if !allow_insert {
            inserts.clear();
        }
        if !allow_update {
            updates.clear();
        }
        if !allow_delete {
            deletes.clear();
        }

        let diff = RowDiff {
            table_name: compare.table_name.clone(),
            primary_key: compare.primary_key.clone(),
            mode: mode.clone(),
            insert_count: inserts.len(),
            update_count: updates.len(),
            delete_count: deletes.len(),
            inserts,
            updates,
            deletes,
        };

        let statements = generate_statements(&diff);
        let sql = if statements.is_empty() {
            "-- No changes detected".to_string()
        } else {
            statements.join("\n")
        };

        Ok(PreviewResult {
            diff,
            statements,
            sql,
            truncated,
        })
    }

    pub async fn deploy(
        target: &DbClient,
        statements: &[String],
        progress: impl Fn(usize, usize) + Send + Sync,
    ) -> Result<u64, AppError> {
        let policy = TimeoutPolicy::default();
        let mut tx = tokio::time::timeout(policy.db_query, target.pool.begin())
            .await
            .map_err(|_| AppError::Timeout("开启事务超时".to_string()))?
            .map_err(|e| AppError::InternalError(e.to_string()))?;

        let total = statements.len();
        let mut affected = 0u64;

        for (idx, stmt) in statements.iter().enumerate() {
            let stmt = stmt.trim();
            if stmt.is_empty() || stmt.starts_with("--") {
                progress(idx + 1, total);
                continue;
            }
            let res = tokio::time::timeout(policy.db_query, sqlx::query(stmt).execute(&mut *tx))
                .await
                .map_err(|_| AppError::Timeout(format!("执行SQL超时: {}", stmt)))?
                .map_err(|e| AppError::InternalError(e.to_string()))?;
            affected += res.rows_affected();
            progress(idx + 1, total);
        }

        tokio::time::timeout(policy.db_query, tx.commit())
            .await
            .map_err(|_| AppError::Timeout("提交事务超时".to_string()))?
            .map_err(|e| AppError::InternalError(e.to_string()))?;

        Ok(affected)
    }
}

fn compare_pk_str(a: &str, b: &str) -> std::cmp::Ordering {
    match (a.parse::<f64>(), b.parse::<f64>()) {
        (Ok(aa), Ok(bb)) => aa.partial_cmp(&bb).unwrap_or(a.cmp(b)),
        _ => a.cmp(b),
    }
}

async fn fetch_min_max_pk(
    db: &DbClient,
    table_name: &str,
    primary_key: &str,
) -> Result<(Option<String>, Option<String>), AppError> {
    let policy = TimeoutPolicy::default();
    let sql = format!(
        "SELECT MIN(`{pk}`) AS min_pk, MAX(`{pk}`) AS max_pk FROM `{table}`",
        pk = primary_key,
        table = table_name
    );
    let row = tokio::time::timeout(policy.db_query, sqlx::query(&sql).fetch_one(&db.pool))
        .await
        .map_err(|_| AppError::Timeout("获取PK范围超时".to_string()))?
        .map_err(|e| AppError::InternalError(e.to_string()))?;

    let min_pk = value_to_string(&row, 0);
    let max_pk = value_to_string(&row, 1);
    Ok((min_pk, max_pk))
}

async fn fetch_pk_list_after(
    db: &DbClient,
    table_name: &str,
    primary_key: &str,
    last_pk: Option<String>,
    limit: usize,
) -> Result<Vec<String>, AppError> {
    let policy = TimeoutPolicy::default();
    let mut sql = format!(
        "SELECT `{pk}` FROM `{table}`",
        pk = primary_key,
        table = table_name
    );
    if last_pk.is_some() {
        sql.push_str(&format!(" WHERE `{}` > ?", primary_key));
    }
    sql.push_str(&format!(" ORDER BY `{}` LIMIT {}", primary_key, limit));

    let mut q = sqlx::query(&sql);
    if let Some(v) = last_pk {
        q = q.bind(v);
    }

    let rows = tokio::time::timeout(policy.db_query, q.fetch_all(&db.pool))
        .await
        .map_err(|_| AppError::Timeout("拉取PK分块超时".to_string()))?
        .map_err(|e| AppError::InternalError(e.to_string()))?;

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        if let Some(v) = value_to_string(&row, 0) {
            out.push(v);
        }
    }
    Ok(out)
}

async fn fetch_rows_in_range(
    db: &DbClient,
    table_name: &str,
    primary_key: &str,
    range: &PkRange,
) -> Result<Vec<Value>, AppError> {
    let policy = TimeoutPolicy::default();
    let mut sql = format!("SELECT * FROM `{}`", table_name);
    let mut has_where = false;

    if let Some(_start) = &range.start {
        sql.push_str(&format!(
            " WHERE `{}` {} ?",
            primary_key,
            if range.start_inclusive { ">=" } else { ">" }
        ));
        has_where = true;
        if let Some(_end) = &range.end {
            sql.push_str(&format!(
                " AND `{}` {} ?",
                primary_key,
                if range.end_inclusive { "<=" } else { "<" }
            ));
        }
    } else if let Some(_end) = &range.end {
        sql.push_str(&format!(
            " WHERE `{}` {} ?",
            primary_key,
            if range.end_inclusive { "<=" } else { "<" }
        ));
        has_where = true;
    }

    if !has_where && range.end.is_some() {
        sql.push_str(&format!(
            " WHERE `{}` {} ?",
            primary_key,
            if range.end_inclusive { "<=" } else { "<" }
        ));
    }

    sql.push_str(&format!(" ORDER BY `{}`", primary_key));

    let mut q = sqlx::query(&sql);
    if let Some(start) = &range.start {
        q = q.bind(start.clone());
        if let Some(end) = &range.end {
            q = q.bind(end.clone());
        }
    } else if let Some(end) = &range.end {
        q = q.bind(end.clone());
    }

    let rows = tokio::time::timeout(policy.db_query_long, q.fetch_all(&db.pool))
        .await
        .map_err(|_| AppError::Timeout("拉取数据分块超时".to_string()))?
        .map_err(|e| AppError::InternalError(e.to_string()))?;

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        out.push(row_to_json(&row));
    }
    Ok(out)
}

fn extract_pk_to_string(row: &Value, primary_key: &str) -> Option<String> {
    let v = row.get(primary_key)?;
    Some(match v {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        Value::Array(a) => serde_json::to_string(a).unwrap_or_default(),
        Value::Object(o) => serde_json::to_string(o).unwrap_or_default(),
    })
}

fn checksum_rows(primary_key: &str, rows: &[Value]) -> u32 {
    let mut hasher = Hasher::new();
    for row in rows {
        if let Some(pk) = extract_pk_to_string(row, primary_key) {
            hasher.update(pk.as_bytes());
            hasher.update(&[0u8]);
        }
        if let Ok(bytes) = serde_json::to_vec(row) {
            hasher.update(&bytes);
        }
        hasher.update(&[0xffu8]);
    }
    hasher.finalize()
}

fn row_to_json(row: &sqlx::mysql::MySqlRow) -> Value {
    let mut map = serde_json::Map::new();
    for col in row.columns() {
        let col_name = col.name().to_string();

        if let Ok(val) = row.try_get::<Option<i64>, _>(col.ordinal()) {
            map.insert(col_name, serde_json::json!(val));
        } else if let Ok(val) = row.try_get::<Option<f64>, _>(col.ordinal()) {
            map.insert(col_name, serde_json::json!(val));
        } else if let Ok(val) = row.try_get::<Option<bool>, _>(col.ordinal()) {
            map.insert(col_name, serde_json::json!(val));
        } else if let Ok(val) = row.try_get::<Option<chrono::NaiveDateTime>, _>(col.ordinal()) {
            map.insert(col_name, serde_json::json!(val.map(|dt| dt.to_string())));
        } else if let Ok(val) = row.try_get::<Option<chrono::NaiveDate>, _>(col.ordinal()) {
            map.insert(col_name, serde_json::json!(val.map(|d| d.to_string())));
        } else if let Ok(val) = row.try_get::<Option<chrono::NaiveTime>, _>(col.ordinal()) {
            map.insert(col_name, serde_json::json!(val.map(|t| t.to_string())));
        } else if let Ok(val) = row.try_get::<Option<String>, _>(col.ordinal()) {
            map.insert(col_name, serde_json::json!(val));
        } else {
            let val: Option<Vec<u8>> = row.try_get(col.ordinal()).unwrap_or(None);
            if let Some(bytes) = val {
                let s = String::from_utf8_lossy(&bytes).into_owned();
                map.insert(col_name, serde_json::json!(s));
            } else {
                map.insert(col_name, Value::Null);
            }
        }
    }
    Value::Object(map)
}

fn value_to_string(row: &sqlx::mysql::MySqlRow, ordinal: usize) -> Option<String> {
    if let Ok(v) = row.try_get::<Option<String>, _>(ordinal) {
        return v;
    }
    if let Ok(v) = row.try_get::<Option<i64>, _>(ordinal) {
        return v.map(|x| x.to_string());
    }
    if let Ok(v) = row.try_get::<Option<f64>, _>(ordinal) {
        return v.map(|x| x.to_string());
    }
    if let Ok(v) = row.try_get::<Option<bool>, _>(ordinal) {
        return v.map(|x| x.to_string());
    }
    if let Ok(v) = row.try_get::<Option<Vec<u8>>, _>(ordinal) {
        return v.map(|x| String::from_utf8_lossy(&x).into_owned());
    }
    None
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

fn generate_statements(diff: &RowDiff) -> Vec<String> {
    let mut stmts = Vec::new();

    if diff.mode == SyncMode::Mirror && !diff.deletes.is_empty() {
        for row in &diff.deletes {
            if let Some(pk_val) = row.get(&diff.primary_key) {
                let pk = format_value(pk_val);
                stmts.push(format!(
                    "DELETE FROM `{}` WHERE `{}` = {};",
                    diff.table_name, diff.primary_key, pk
                ));
            }
        }
    }

    let mut upserts: Vec<Value> = Vec::new();
    upserts.extend(diff.inserts.iter().cloned());
    upserts.extend(diff.updates.iter().map(|(_, new)| new.clone()));

    for row in upserts {
        if let Some(obj) = row.as_object() {
            let mut cols = Vec::new();
            let mut vals = Vec::new();
            let mut updates = Vec::new();
            for (k, v) in obj {
                cols.push(format!("`{}`", k));
                vals.push(format_value(v));
                if k != &diff.primary_key {
                    updates.push(format!("`{}` = new.`{}`", k, k));
                }
            }
            if updates.is_empty() {
                stmts.push(format!(
                    "INSERT IGNORE INTO `{}` ({}) VALUES ({});",
                    diff.table_name,
                    cols.join(", "),
                    vals.join(", ")
                ));
            } else {
                stmts.push(format!(
                    "INSERT INTO `{}` ({}) VALUES ({}) AS new ON DUPLICATE KEY UPDATE {};",
                    diff.table_name,
                    cols.join(", "),
                    vals.join(", "),
                    updates.join(", ")
                ));
            }
        }
    }

    stmts
}
