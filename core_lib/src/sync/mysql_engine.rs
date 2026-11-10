use crate::config::DbType;
use crate::db::DbPool;
use crate::db::DbClient;
use crate::error::AppError;
use crate::sql_util;
use crate::timeout_policy::TimeoutPolicy;
use crc32fast::Hasher;
use serde::{Deserialize, Serialize};
use serde_json::Value;
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

/// Backward-compatible type alias.
pub type MySqlDataSyncEngine = RowDataSyncEngine;

/// Row-level data sync engine supporting MySQL, PostgreSQL, and SQLite.
pub struct RowDataSyncEngine;

impl RowDataSyncEngine {
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

        let statements = generate_statements(&diff, &target.db_type);
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
        let total = statements.len();
        let mut affected = 0u64;

        match &target.pool {
            DbPool::MySQL(p) => {
                let mut tx = tokio::time::timeout(policy.db_query, p.begin())
                    .await
                    .map_err(|_| AppError::Timeout("开启事务超时".to_string()))?
                    .map_err(|e| AppError::InternalError(e.to_string()))?;

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
            }
            DbPool::Postgres(p) => {
                let mut tx = tokio::time::timeout(policy.db_query, p.begin())
                    .await
                    .map_err(|_| AppError::Timeout("开启事务超时".to_string()))?
                    .map_err(|e| AppError::InternalError(e.to_string()))?;

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
            }
            DbPool::SQLite(p) => {
                let mut tx = tokio::time::timeout(policy.db_query, p.begin())
                    .await
                    .map_err(|_| AppError::Timeout("开启事务超时".to_string()))?
                    .map_err(|e| AppError::InternalError(e.to_string()))?;

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
            }
        }

        Ok(affected)
    }
}

fn compare_pk_str(a: &str, b: &str) -> std::cmp::Ordering {
    // Try integer comparison first (i128 handles up to 2^127, no precision loss)
    match (a.parse::<i128>(), b.parse::<i128>()) {
        (Ok(aa), Ok(bb)) => return aa.cmp(&bb),
        _ => {}
    }
    // Fall back to float for decimal PKs
    match (a.parse::<f64>(), b.parse::<f64>()) {
        (Ok(aa), Ok(bb)) => aa.partial_cmp(&bb).unwrap_or(a.cmp(b)),
        _ => a.cmp(b),
    }
}

/// Build a parameterized SQL condition fragment for PK range filtering.
/// Returns (sql_fragment, bind_values) where bind_values are the values to bind in order.
#[allow(unused_assignments)]
fn build_pk_range_conditions(
    primary_key: &str,
    db_type: &DbType,
    range: &PkRange,
) -> (String, Vec<String>) {
    let pk_ident = sql_util::quote_ident(primary_key, db_type.clone());
    let mut sql = String::new();
    let mut bind_values: Vec<String> = Vec::new();
    let mut param_idx = 1usize; // for PostgreSQL $N placeholders

    if let Some(_start) = &range.start {
        let placeholder = match db_type {
            DbType::PostgreSQL => {
                let p = format!("${}", param_idx);
                param_idx += 1;
                p
            }
            _ => "?".to_string(),
        };
        sql.push_str(&format!(
            " WHERE {pk} {op} {ph}",
            pk = pk_ident,
            op = if range.start_inclusive { ">=" } else { ">" },
            ph = placeholder
        ));
        if let Some(_end) = &range.end {
            let placeholder = match db_type {
                DbType::PostgreSQL => {
                    let p = format!("${}", param_idx);
                    param_idx += 1;
                    p
                }
                _ => "?".to_string(),
            };
            sql.push_str(&format!(
                " AND {pk} {op} {ph}",
                pk = pk_ident,
                op = if range.end_inclusive { "<=" } else { "<" },
                ph = placeholder
            ));
        }
    } else if let Some(_end) = &range.end {
        let placeholder = match db_type {
            DbType::PostgreSQL => {
                let p = format!("${}", param_idx);
                param_idx += 1;
                p
            }
            _ => "?".to_string(),
        };
        sql.push_str(&format!(
            " WHERE {pk} {op} {ph}",
            pk = pk_ident,
            op = if range.end_inclusive { "<=" } else { "<" },
            ph = placeholder
        ));
    }

    // Collect bind values in the same order as placeholders appear
    if let Some(start) = &range.start {
        bind_values.push(start.clone());
        if let Some(end) = &range.end {
            bind_values.push(end.clone());
        }
    } else if let Some(end) = &range.end {
        bind_values.push(end.clone());
    }

    (sql, bind_values)
}

async fn fetch_min_max_pk(
    db: &DbClient,
    table_name: &str,
    primary_key: &str,
) -> Result<(Option<String>, Option<String>), AppError> {
    let policy = TimeoutPolicy::default();
    let pk_ident = sql_util::quote_ident(primary_key, db.db_type.clone());
    let tbl_ident = sql_util::quote_ident(table_name, db.db_type.clone());
    let sql = format!(
        "SELECT MIN({pk}) AS min_pk, MAX({pk}) AS max_pk FROM {table}",
        pk = pk_ident,
        table = tbl_ident
    );

    match &db.pool {
        DbPool::MySQL(p) => {
            let row = tokio::time::timeout(policy.db_query, sqlx::query(&sql).fetch_one(p))
                .await
                .map_err(|_| AppError::Timeout("获取PK范围超时".to_string()))?
                .map_err(|e| AppError::InternalError(e.to_string()))?;
            let min_pk = sql_util::mysql_cell_to_string(&row, 0);
            let max_pk = sql_util::mysql_cell_to_string(&row, 1);
            Ok((min_pk, max_pk))
        }
        DbPool::Postgres(p) => {
            let row = tokio::time::timeout(policy.db_query, sqlx::query(&sql).fetch_one(p))
                .await
                .map_err(|_| AppError::Timeout("获取PK范围超时".to_string()))?
                .map_err(|e| AppError::InternalError(e.to_string()))?;
            let min_pk = sql_util::pg_cell_to_string(&row, 0);
            let max_pk = sql_util::pg_cell_to_string(&row, 1);
            Ok((min_pk, max_pk))
        }
        DbPool::SQLite(p) => {
            let row = tokio::time::timeout(policy.db_query, sqlx::query(&sql).fetch_one(p))
                .await
                .map_err(|_| AppError::Timeout("获取PK范围超时".to_string()))?
                .map_err(|e| AppError::InternalError(e.to_string()))?;
            let min_pk = sql_util::sqlite_cell_to_string(&row, 0);
            let max_pk = sql_util::sqlite_cell_to_string(&row, 1);
            Ok((min_pk, max_pk))
        }
    }
}

async fn fetch_pk_list_after(
    db: &DbClient,
    table_name: &str,
    primary_key: &str,
    last_pk: Option<String>,
    limit: usize,
) -> Result<Vec<String>, AppError> {
    let policy = TimeoutPolicy::default();
    let pk_ident = sql_util::quote_ident(primary_key, db.db_type.clone());
    let tbl_ident = sql_util::quote_ident(table_name, db.db_type.clone());

    let mut sql = format!("SELECT {pk} FROM {table}", pk = pk_ident, table = tbl_ident);
    let mut bind_values: Vec<String> = Vec::new();

    if last_pk.is_some() {
        let placeholder = match db.db_type {
            DbType::PostgreSQL => "$1".to_string(),
            _ => "?".to_string(),
        };
        sql.push_str(&format!(" WHERE {pk} > {ph}", pk = pk_ident, ph = placeholder));
        if let Some(ref v) = last_pk {
            bind_values.push(v.clone());
        }
    }
    sql.push_str(&format!(" ORDER BY {pk} LIMIT {limit}", pk = pk_ident, limit = limit));

    match &db.pool {
        DbPool::MySQL(p) => {
            let mut q = sqlx::query(&sql);
            for v in &bind_values {
                q = q.bind(v.clone());
            }
            let rows = tokio::time::timeout(policy.db_query, q.fetch_all(p))
                .await
                .map_err(|_| AppError::Timeout("拉取PK分块超时".to_string()))?
                .map_err(|e| AppError::InternalError(e.to_string()))?;
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                if let Some(v) = sql_util::mysql_cell_to_string(&row, 0) {
                    out.push(v);
                }
            }
            Ok(out)
        }
        DbPool::Postgres(p) => {
            let mut q = sqlx::query(&sql);
            for v in &bind_values {
                q = q.bind(v.clone());
            }
            let rows = tokio::time::timeout(policy.db_query, q.fetch_all(p))
                .await
                .map_err(|_| AppError::Timeout("拉取PK分块超时".to_string()))?
                .map_err(|e| AppError::InternalError(e.to_string()))?;
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                if let Some(v) = sql_util::pg_cell_to_string(&row, 0) {
                    out.push(v);
                }
            }
            Ok(out)
        }
        DbPool::SQLite(p) => {
            let mut q = sqlx::query(&sql);
            for v in &bind_values {
                q = q.bind(v.clone());
            }
            let rows = tokio::time::timeout(policy.db_query, q.fetch_all(p))
                .await
                .map_err(|_| AppError::Timeout("拉取PK分块超时".to_string()))?
                .map_err(|e| AppError::InternalError(e.to_string()))?;
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                if let Some(v) = sql_util::sqlite_cell_to_string(&row, 0) {
                    out.push(v);
                }
            }
            Ok(out)
        }
    }
}

async fn fetch_rows_in_range(
    db: &DbClient,
    table_name: &str,
    primary_key: &str,
    range: &PkRange,
) -> Result<Vec<Value>, AppError> {
    let policy = TimeoutPolicy::default();
    let pk_ident = sql_util::quote_ident(primary_key, db.db_type.clone());
    let tbl_ident = sql_util::quote_ident(table_name, db.db_type.clone());
    let mut sql = format!("SELECT * FROM {table}", table = tbl_ident);

    let (conditions, bind_values) = build_pk_range_conditions(primary_key, &db.db_type, range);
    sql.push_str(&conditions);
    sql.push_str(&format!(" ORDER BY {pk}", pk = pk_ident));

    // Safety: enforce max rows per chunk to prevent OOM on large tables
    let max_rows = max_rows_per_chunk();
    sql.push_str(&format!(" LIMIT {}", max_rows));

    match &db.pool {
        DbPool::MySQL(p) => {
            let mut q = sqlx::query(&sql);
            for v in &bind_values {
                q = q.bind(v.clone());
            }
            let rows = tokio::time::timeout(policy.db_query_long, q.fetch_all(p))
                .await
                .map_err(|_| AppError::Timeout("拉取数据分块超时".to_string()))?
                .map_err(|e| AppError::InternalError(e.to_string()))?;
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                out.push(sql_util::mysql_row_to_json(&row));
            }
            Ok(out)
        }
        DbPool::Postgres(p) => {
            let mut q = sqlx::query(&sql);
            for v in &bind_values {
                q = q.bind(v.clone());
            }
            let rows = tokio::time::timeout(policy.db_query_long, q.fetch_all(p))
                .await
                .map_err(|_| AppError::Timeout("拉取数据分块超时".to_string()))?
                .map_err(|e| AppError::InternalError(e.to_string()))?;
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                out.push(sql_util::pg_row_to_json(&row));
            }
            Ok(out)
        }
        DbPool::SQLite(p) => {
            let mut q = sqlx::query(&sql);
            for v in &bind_values {
                q = q.bind(v.clone());
            }
            let rows = tokio::time::timeout(policy.db_query_long, q.fetch_all(p))
                .await
                .map_err(|_| AppError::Timeout("拉取数据分块超时".to_string()))?
                .map_err(|e| AppError::InternalError(e.to_string()))?;
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                out.push(sql_util::sqlite_row_to_json(&row));
            }
            Ok(out)
        }
    }
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

/// 解析同步主键定义。单列主键返回单元素；复合主键按逗号拆分为多列，
/// 供 SQLite 的 ON CONFLICT(...) 冲突目标使用。
fn split_primary_key(primary_key: &str) -> Vec<String> {
    primary_key
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn generate_statements(diff: &RowDiff, db_type: &DbType) -> Vec<String> {
    let qi = |s: &str| sql_util::quote_ident(s, db_type.clone());
    let mut stmts = Vec::new();

    if diff.mode == SyncMode::Mirror && !diff.deletes.is_empty() {
        for row in &diff.deletes {
            if let Some(pk_val) = row.get(&diff.primary_key) {
                let pk = sql_util::format_sql_value(pk_val);
                stmts.push(format!(
                    "DELETE FROM {} WHERE {} = {};",
                    qi(&diff.table_name),
                    qi(&diff.primary_key),
                    pk
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
            let mut non_pk_updates = Vec::new();
            for (k, v) in obj {
                cols.push(qi(k));
                vals.push(sql_util::format_sql_value(v));
                if k != &diff.primary_key {
                    non_pk_updates.push(k.clone());
                }
            }

            match db_type {
                DbType::MySQL | DbType::MariaDB => {
                    if non_pk_updates.is_empty() {
                        stmts.push(format!(
                            "INSERT IGNORE INTO {} ({}) VALUES ({});",
                            qi(&diff.table_name),
                            cols.join(", "),
                            vals.join(", ")
                        ));
                    } else {
                        // MariaDB 不支持 MySQL 8.0.19+ 的行别名语法（AS new），
                        // 统一退回 VALUES(col)；目标引擎类型由连接时的 VERSION()
                        // 纠偏结果决定，避免把 mysql:// 指向的 MariaDB 误判为 MySQL。
                        let is_mariadb = matches!(db_type, DbType::MariaDB);
                        let updates: Vec<String> = non_pk_updates
                            .iter()
                            .map(|c| {
                                if is_mariadb {
                                    format!("{} = VALUES({})", qi(c), qi(c))
                                } else {
                                    format!("{} = new.{}", qi(c), qi(c))
                                }
                            })
                            .collect();
                        if is_mariadb {
                            stmts.push(format!(
                                "INSERT INTO {} ({}) VALUES ({}) ON DUPLICATE KEY UPDATE {};",
                                qi(&diff.table_name),
                                cols.join(", "),
                                vals.join(", "),
                                updates.join(", ")
                            ));
                        } else {
                            stmts.push(format!(
                                "INSERT INTO {} ({}) VALUES ({}) AS new ON DUPLICATE KEY UPDATE {};",
                                qi(&diff.table_name),
                                cols.join(", "),
                                vals.join(", "),
                                updates.join(", ")
                            ));
                        }
                    }
                }
                DbType::PostgreSQL => {
                    if non_pk_updates.is_empty() {
                        stmts.push(format!(
                            "INSERT INTO {} ({}) VALUES ({}) ON CONFLICT DO NOTHING;",
                            qi(&diff.table_name),
                            cols.join(", "),
                            vals.join(", ")
                        ));
                    } else {
                        let updates: Vec<String> = non_pk_updates
                            .iter()
                            .map(|c| format!("{} = EXCLUDED.{}", qi(c), qi(c)))
                            .collect();
                        stmts.push(format!(
                            "INSERT INTO {} ({}) VALUES ({}) ON CONFLICT ({}) DO UPDATE SET {};",
                            qi(&diff.table_name),
                            cols.join(", "),
                            vals.join(", "),
                            qi(&diff.primary_key),
                            updates.join(", ")
                        ));
                    }
                }
                _ => {
                    // SQLite：不能用 INSERT OR REPLACE。SQLite 的 REPLACE 语义是
                    // 先 DELETE 冲突行再 INSERT，会触发外键 ON DELETE CASCADE 和
                    // delete 触发器，父表更新一行可能连带删掉子表数据。
                    // 改用 ON CONFLICT(pk) DO UPDATE，与 MySQL / PG 的 upsert 对齐。
                    let pk_cols = split_primary_key(&diff.primary_key);
                    let updates: Vec<String> = non_pk_updates
                        .iter()
                        .filter(|c| !pk_cols.iter().any(|pk| pk.eq_ignore_ascii_case(c)))
                        .map(|c| format!("{} = excluded.{}", qi(c), qi(c)))
                        .collect();
                    let conflict_target = pk_cols
                        .iter()
                        .map(|c| qi(c))
                        .collect::<Vec<_>>()
                        .join(", ");
                    if updates.is_empty() {
                        stmts.push(format!(
                            "INSERT INTO {} ({}) VALUES ({}) ON CONFLICT ({}) DO NOTHING;",
                            qi(&diff.table_name),
                            cols.join(", "),
                            vals.join(", "),
                            conflict_target
                        ));
                    } else {
                        stmts.push(format!(
                            "INSERT INTO {} ({}) VALUES ({}) ON CONFLICT ({}) DO UPDATE SET {};",
                            qi(&diff.table_name),
                            cols.join(", "),
                            vals.join(", "),
                            conflict_target,
                            updates.join(", ")
                        ));
                    }
                }
            }
        }
    }

    stmts
}

/// Maximum rows per chunk for fetch_rows_in_range.
/// Defaults to 100,000; override via LOCAL_AI_SQL_SYNC_MAX_ROWS_PER_CHUNK env var.
fn max_rows_per_chunk() -> usize {
    std::env::var("LOCAL_AI_SQL_SYNC_MAX_ROWS_PER_CHUNK")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(100_000)
        .max(1)
}

/// Stream rows in a PK range one at a time -- O(1) memory for large exports.
/// Calls `on_row` for each row; returns total count processed.
pub async fn fetch_rows_in_range_stream<F>(
    db: &DbClient,
    table_name: &str,
    primary_key: &str,
    range: &PkRange,
    on_row: F,
) -> Result<usize, AppError>
where
    F: FnMut(Value) + Send,
{
    use futures_util::TryStreamExt;

    let _policy = TimeoutPolicy::default();
    let pk_ident = sql_util::quote_ident(primary_key, db.db_type.clone());
    let tbl_ident = sql_util::quote_ident(table_name, db.db_type.clone());
    let mut sql = format!("SELECT * FROM {table}", table = tbl_ident);

    let (conditions, bind_values) = build_pk_range_conditions(primary_key, &db.db_type, range);
    sql.push_str(&conditions);
    sql.push_str(&format!(" ORDER BY {pk}", pk = pk_ident));
    sql.push_str(&format!(" LIMIT {}", max_rows_per_chunk()));

    match &db.pool {
        DbPool::MySQL(p) => {
            let mut q = sqlx::query(&sql);
            for v in &bind_values {
                q = q.bind(v.clone());
            }
            let mut stream = q.fetch(p);
            let mut count = 0usize;
            let mut on_row = on_row;
            while let Some(row) = stream
                .try_next()
                .await
                .map_err(|e| AppError::InternalError(e.to_string()))?
            {
                on_row(sql_util::mysql_row_to_json(&row));
                count += 1;
            }
            Ok(count)
        }
        DbPool::Postgres(p) => {
            let mut q = sqlx::query(&sql);
            for v in &bind_values {
                q = q.bind(v.clone());
            }
            let mut stream = q.fetch(p);
            let mut count = 0usize;
            let mut on_row = on_row;
            while let Some(row) = stream
                .try_next()
                .await
                .map_err(|e| AppError::InternalError(e.to_string()))?
            {
                on_row(sql_util::pg_row_to_json(&row));
                count += 1;
            }
            Ok(count)
        }
        DbPool::SQLite(p) => {
            let mut q = sqlx::query(&sql);
            for v in &bind_values {
                q = q.bind(v.clone());
            }
            let mut stream = q.fetch(p);
            let mut count = 0usize;
            let mut on_row = on_row;
            while let Some(row) = stream
                .try_next()
                .await
                .map_err(|e| AppError::InternalError(e.to_string()))?
            {
                on_row(sql_util::sqlite_row_to_json(&row));
                count += 1;
            }
            Ok(count)
        }
    }
}

/// Generate a NULL-safe row-level checksum expression for the given columns.
/// MySQL uses CRC32(CONCAT_WS(...)), PostgreSQL uses md5(concat_ws(...)).
pub fn row_checksum_expr(columns: &[String], db_type: &crate::config::DbType) -> String {
    let coalesced: Vec<String> = columns
        .iter()
        .map(|col| {
            format!(
                "COALESCE(CAST({} AS CHAR), '<NULL>')",
                sql_util::quote_ident(col, db_type.clone())
            )
        })
        .collect();
    match db_type {
        crate::config::DbType::MySQL | crate::config::DbType::MariaDB => {
            format!("CRC32(CONCAT_WS('#', {}))", coalesced.join(", "))
        }
        _ => format!("md5(concat_ws('#', {}))", coalesced.join(", ")),
    }
}

/// Adaptive chunk sizing for data sync operations.
/// Adjusts chunk size based on observed throughput to target a specific duration per chunk.
#[derive(Debug, Clone)]
pub struct AdaptiveChunker {
    /// Target duration per chunk in milliseconds
    pub target_duration_ms: u64,
    /// Current chunk size (rows)
    pub current_chunk_size: usize,
    /// Minimum chunk size
    pub min_chunk_size: usize,
    /// Maximum chunk size
    pub max_chunk_size: usize,
    /// Exponential moving average throughput (rows/sec)
    ema_throughput: f64,
    /// EMA smoothing factor
    alpha: f64,
}

impl AdaptiveChunker {
    pub fn new(target_duration_ms: u64) -> Self {
        Self {
            target_duration_ms,
            current_chunk_size: 1000,
            min_chunk_size: 100,
            max_chunk_size: 100_000,
            ema_throughput: 0.0,
            alpha: 0.3,
        }
    }

    /// Adjust chunk size based on observed performance.
    /// Call after each chunk completes with the actual row count and elapsed time.
    pub fn adjust(&mut self, actual_rows: usize, actual_duration_ms: u64) {
        let duration = (actual_duration_ms as f64).max(1.0);
        let throughput = (actual_rows as f64) * 1000.0 / duration;

        if self.ema_throughput == 0.0 {
            // First measurement — seed the EMA
            self.ema_throughput = throughput;
        } else {
            self.ema_throughput =
                self.alpha * throughput + (1.0 - self.alpha) * self.ema_throughput;
        }

        let ideal_rows =
            (self.ema_throughput * (self.target_duration_ms as f64) / 1000.0) as usize;
        self.current_chunk_size = ideal_rows.clamp(self.min_chunk_size, self.max_chunk_size);
    }

    /// Returns the current recommended chunk size.
    pub fn chunk_size(&self) -> usize {
        self.current_chunk_size
    }
}

impl Default for AdaptiveChunker {
    fn default() -> Self {
        Self::new(500)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adaptive_chunker_increases_on_fast_throughput() {
        let mut c = AdaptiveChunker::new(500);
        // Simulate: 1000 rows in 100ms = 10,000 rows/sec
        c.adjust(1000, 100);
        // Ideal: 10000 * 0.5 = 5000 rows
        assert_eq!(c.chunk_size(), 5000);
    }

    #[test]
    fn adaptive_chunker_decreases_on_slow_throughput() {
        let mut c = AdaptiveChunker::new(500);
        // First: fast
        c.adjust(1000, 100);
        // Then: slow — 100 rows in 5000ms = 20 rows/sec
        c.adjust(100, 5000);
        // EMA: 0.3 * 20 + 0.7 * 10000 = 7006 -> ideal = 3503
        // But after many slow iterations it converges down
        for _ in 0..10 {
            c.adjust(100, 5000);
        }
        // After many slow iterations, chunk_size converges toward min
        assert!(c.chunk_size() <= 150, "chunk_size {} should be near min 100", c.chunk_size());
    }

    #[test]
    fn row_checksum_expr_mysql_uses_crc32() {
        let cols = vec!["id".to_string(), "name".to_string()];
        let expr = row_checksum_expr(&cols, &crate::config::DbType::MySQL);
        assert!(expr.contains("CRC32"));
        assert!(expr.contains("CONCAT_WS"));
        assert!(expr.contains("COALESCE"));
    }

    #[test]
    fn row_checksum_expr_pg_uses_md5() {
        let cols = vec!["id".to_string(), "name".to_string()];
        let expr = row_checksum_expr(&cols, &crate::config::DbType::PostgreSQL);
        assert!(expr.contains("md5"));
        assert!(expr.contains("concat_ws"));
    }

    #[test]
    fn max_rows_per_chunk_default_is_100k() {
        // Without env var, default should be 100_000
        assert_eq!(max_rows_per_chunk(), 100_000);
    }

    #[test]
    fn generate_statements_mysql_upsert() {
        let diff = RowDiff {
            table_name: "users".to_string(),
            primary_key: "id".to_string(),
            mode: SyncMode::UpsertOnly,
            insert_count: 1,
            update_count: 0,
            delete_count: 0,
            inserts: vec![serde_json::json!({"id": 1, "name": "alice"})],
            updates: vec![],
            deletes: vec![],
        };
        let stmts = generate_statements(&diff, &DbType::MySQL);
        assert!(stmts[0].contains("ON DUPLICATE KEY UPDATE"));
        assert!(stmts[0].contains("`name` = new.`name`"));
    }

    #[test]
    fn generate_statements_mariadb_upsert_uses_values_function() {
        let diff = RowDiff {
            table_name: "users".to_string(),
            primary_key: "id".to_string(),
            mode: SyncMode::UpsertOnly,
            insert_count: 1,
            update_count: 0,
            delete_count: 0,
            inserts: vec![serde_json::json!({"id": 1, "name": "alice"})],
            updates: vec![],
            deletes: vec![],
        };
        let stmts = generate_statements(&diff, &DbType::MariaDB);
        assert!(stmts[0].contains("ON DUPLICATE KEY UPDATE"));
        assert!(stmts[0].contains("`name` = VALUES(`name`)"));
        assert!(
            !stmts[0].contains(" AS new "),
            "MariaDB 不支持行别名语法: {}",
            stmts[0]
        );
    }

    #[test]
    fn generate_statements_pg_upsert() {
        let diff = RowDiff {
            table_name: "users".to_string(),
            primary_key: "id".to_string(),
            mode: SyncMode::UpsertOnly,
            insert_count: 1,
            update_count: 0,
            delete_count: 0,
            inserts: vec![serde_json::json!({"id": 1, "name": "alice"})],
            updates: vec![],
            deletes: vec![],
        };
        let stmts = generate_statements(&diff, &DbType::PostgreSQL);
        assert!(stmts[0].contains("ON CONFLICT"));
        assert!(stmts[0].contains("EXCLUDED"));
    }

    #[test]
    fn generate_statements_sqlite_upsert() {
        let diff = RowDiff {
            table_name: "users".to_string(),
            primary_key: "id".to_string(),
            mode: SyncMode::UpsertOnly,
            insert_count: 1,
            update_count: 0,
            delete_count: 0,
            inserts: vec![serde_json::json!({"id": 1, "name": "alice"})],
            updates: vec![],
            deletes: vec![],
        };
        let stmts = generate_statements(&diff, &DbType::SQLite);
        // 断言变更为 ON CONFLICT：INSERT OR REPLACE 在 SQLite 上是先 DELETE 再
        // INSERT，会级联删除子表并触发 delete 触发器，不能用于行级同步。
        assert!(
            stmts[0].starts_with("INSERT INTO"),
            "SQLite 不得再生成 INSERT OR REPLACE: {}",
            stmts[0]
        );
        assert!(
            stmts[0].contains("ON CONFLICT (\"id\") DO UPDATE SET"),
            "SQLite 冲突目标应为主键: {}",
            stmts[0]
        );
        assert!(stmts[0].contains("\"name\" = excluded.\"name\""), "{}", stmts[0]);
        assert!(!stmts[0].contains("OR REPLACE"), "{}", stmts[0]);
    }

    #[test]
    fn generate_statements_sqlite_composite_pk_conflict_target() {
        let diff = RowDiff {
            table_name: "members".to_string(),
            primary_key: "tenant_id, id".to_string(),
            mode: SyncMode::UpsertOnly,
            insert_count: 1,
            update_count: 0,
            delete_count: 0,
            inserts: vec![serde_json::json!({"tenant_id": 7, "id": 1, "name": "alice"})],
            updates: vec![],
            deletes: vec![],
        };
        let stmts = generate_statements(&diff, &DbType::SQLite);
        assert!(
            stmts[0].contains("ON CONFLICT (\"tenant_id\", \"id\") DO UPDATE SET"),
            "复合主键冲突目标生成错误: {}",
            stmts[0]
        );
        // 冲突目标里的两列都不能出现在 UPDATE SET 中。
        assert!(
            !stmts[0].contains("\"tenant_id\" = excluded"),
            "主键列不应被更新: {}",
            stmts[0]
        );
        assert!(
            !stmts[0].contains("\"id\" = excluded"),
            "主键列不应被更新: {}",
            stmts[0]
        );
        assert!(stmts[0].contains("\"name\" = excluded.\"name\""), "{}", stmts[0]);
    }

    /// 真库用例：SQLite 父表 upsert 已存在行后，子表数据必须原样存在，
    /// 且父表的 delete 触发器不得被触发。
    #[tokio::test]
    async fn sqlite_upsert_keeps_children_and_skips_delete_trigger() {
        use std::time::{SystemTime, UNIX_EPOCH};

        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "mysql_engine_sqlite_upsert_{}_{}.db",
            std::process::id(),
            stamp
        ));
        let url = format!("sqlite://{}?mode=rwc", path.to_string_lossy());
        // 单连接：PRAGMA 是连接级的，必须保证外键与递归触发器在所有语句上生效。
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap();

        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(&pool)
            .await
            .unwrap();
        // REPLACE 冲突删除行时，SQLite 只在开启递归触发器时才触发 delete 触发器，
        // 这里显式打开，才能钉死“修复前会触发”的行为。
        sqlx::query("PRAGMA recursive_triggers = ON")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE parent (
                 id INTEGER PRIMARY KEY,
                 name TEXT NOT NULL
             )",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE child (
                 id INTEGER PRIMARY KEY,
                 parent_id INTEGER NOT NULL REFERENCES parent(id) ON DELETE CASCADE,
                 note TEXT NOT NULL
             )",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE parent_delete_log (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 parent_id INTEGER NOT NULL
             )",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TRIGGER parent_after_delete AFTER DELETE ON parent BEGIN
                 INSERT INTO parent_delete_log(parent_id) VALUES (OLD.id);
             END",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO parent (id, name) VALUES (1, 'old')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO child (id, parent_id, note) VALUES (10, 1, 'keep')")
            .execute(&pool)
            .await
            .unwrap();

        let diff = RowDiff {
            table_name: "parent".to_string(),
            primary_key: "id".to_string(),
            mode: SyncMode::UpsertOnly,
            insert_count: 0,
            update_count: 1,
            delete_count: 0,
            inserts: vec![],
            updates: vec![
                (
                    serde_json::json!({"id": 1, "name": "old"}),
                    serde_json::json!({"id": 1, "name": "new"}),
                ),
            ],
            deletes: vec![],
        };
        let statements = generate_statements(&diff, &DbType::SQLite);
        assert_eq!(statements.len(), 1);
        for stmt in &statements {
            sqlx::query(stmt).execute(&pool).await.unwrap();
        }

        let name: String = sqlx::query_scalar("SELECT name FROM parent WHERE id = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(name, "new", "父表应被更新为新值");
        let child_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM child WHERE parent_id = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(child_count, 1, "upsert 不得级联删除子表数据");
        let child_note: String = sqlx::query_scalar("SELECT note FROM child WHERE id = 10")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(child_note, "keep", "子表数据必须原样存在");
        let delete_log: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM parent_delete_log")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(delete_log, 0, "upsert 不得触发父表 delete 触发器");

        // 说明修复前的行为：同样的行如果走 INSERT OR REPLACE，会先 DELETE，
        // 外键 ON DELETE CASCADE 删掉子表，并且 delete 触发器被触发。
        // 这段只用于固化“旧语义有害”的事实，不作为生产路径。
        sqlx::query("INSERT OR REPLACE INTO parent (id, name) VALUES (1, 'legacy')")
            .execute(&pool)
            .await
            .unwrap();
        let child_after_replace: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM child WHERE parent_id = 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            child_after_replace, 0,
            "INSERT OR REPLACE 应级联删除子表（修复前的坏行为）"
        );
        let delete_log_after_replace: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM parent_delete_log")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            delete_log_after_replace, 1,
            "INSERT OR REPLACE 应触发 delete 触发器（修复前的坏行为）"
        );

        drop(pool);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn generate_statements_mirror_delete() {
        let diff = RowDiff {
            table_name: "users".to_string(),
            primary_key: "id".to_string(),
            mode: SyncMode::Mirror,
            insert_count: 0,
            update_count: 0,
            delete_count: 1,
            inserts: vec![],
            updates: vec![],
            deletes: vec![serde_json::json!({"id": 42})],
        };
        let stmts = generate_statements(&diff, &DbType::PostgreSQL);
        assert!(stmts[0].contains("DELETE FROM"));
        assert!(stmts[0].contains("\"id\" = 42"));
    }
}
