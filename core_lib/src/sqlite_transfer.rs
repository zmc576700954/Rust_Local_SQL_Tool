//! SQLite 导入/导出的核心辅助。
//!
//! SQLite 是单写者引擎：导入在一个 `BEGIN IMMEDIATE` 事务里完成，任何一行
//! 失败都整批回滚（`rollback_scope = "job"`）。并发导入不会依赖
//! `SQLITE_BUSY` 碰运气，而是在短暂等待后显式拒绝并给出可读错误。

use crate::db::DbError;
use serde_json::Value;
use sqlx::SqlitePool;
use std::time::Duration;

const IMPORT_BATCH_SIZE: usize = 200;
const WRITE_LOCK_WAIT_MS: u64 = 1_000;

#[derive(Debug, Clone, Default)]
pub struct SqliteImportOutcome {
    pub total: u64,
    pub inserted: u64,
    pub failed_row: Option<u64>,
    pub error: Option<String>,
    pub rolled_back: bool,
    /// SQLite 导入始终是整批原子事务。
    pub atomic: bool,
    pub rollback_scope: String,
}

/// 在单个事务中写入所有行。成功返回 `inserted == total`；
/// 任一行失败则回滚整批，返回 `inserted == 0` 与失败行号。
pub async fn import_rows_atomic(
    pool: &SqlitePool,
    table: &str,
    columns: &[String],
    rows: &[Vec<Value>],
) -> Result<SqliteImportOutcome, DbError> {
    let total = rows.len() as u64;
    let mut outcome = SqliteImportOutcome {
        total,
        atomic: true,
        rollback_scope: "job".to_string(),
        ..Default::default()
    };
    if rows.is_empty() {
        return Ok(outcome);
    }
    if columns.is_empty() {
        return Err(DbError::Unsupported(
            "SQLite 导入缺少目标列".to_string(),
        ));
    }

    let table_ident = quote_ident(table);
    let col_list = columns
        .iter()
        .map(|c| quote_ident(c))
        .collect::<Vec<_>>()
        .join(", ");

    let mut conn = pool.acquire().await?;
    // 单写者策略：短暂排队，超时后显式拒绝，而不是无限等待。
    let _ = sqlx::query(&format!("PRAGMA busy_timeout = {}", WRITE_LOCK_WAIT_MS))
        .execute(&mut *conn)
        .await;
    if let Err(e) = sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await {
        return Err(busy_or(e));
    }

    let mut inserted = 0u64;
    for (batch_start, batch) in rows.chunks(IMPORT_BATCH_SIZE).enumerate() {
        let placeholders = batch
            .iter()
            .map(|row| {
                format!(
                    "({})",
                    std::iter::repeat("?")
                        .take(row.len())
                        .collect::<Vec<_>>()
                        .join(",")
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "INSERT INTO {} ({}) VALUES {}",
            table_ident, col_list, placeholders
        );
        let mut query = sqlx::query(&sql);
        for row in batch {
            if row.len() != columns.len() {
                let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
                outcome.rolled_back = true;
                outcome.error = Some(format!(
                    "第 {} 行列数 {} 与目标列数 {} 不一致",
                    batch_start * IMPORT_BATCH_SIZE + 1,
                    row.len(),
                    columns.len()
                ));
                outcome.failed_row = Some((batch_start * IMPORT_BATCH_SIZE + 1) as u64);
                return Ok(outcome);
            }
            for value in row {
                query = bind_value(query, value);
            }
        }
        match query.execute(&mut *conn).await {
            Ok(result) => inserted += result.rows_affected(),
            Err(e) => {
                let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
                outcome.rolled_back = true;
                outcome.error = Some(e.to_string());
                outcome.failed_row = Some((batch_start * IMPORT_BATCH_SIZE + 1) as u64);
                outcome.inserted = 0;
                return Ok(outcome);
            }
        }
    }

    if let Err(e) = sqlx::query("COMMIT").execute(&mut *conn).await {
        let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
        outcome.rolled_back = true;
        outcome.error = Some(e.to_string());
        outcome.inserted = 0;
        return Ok(outcome);
    }
    outcome.inserted = inserted;
    Ok(outcome)
}

fn bind_value<'q>(
    query: sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>>,
    value: &'q Value,
) -> sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>> {
    match value {
        Value::Null => query.bind(Option::<String>::None),
        Value::Bool(b) => query.bind(if *b { 1i64 } else { 0i64 }),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                query.bind(i)
            } else if let Some(u) = n.as_u64() {
                if u <= i64::MAX as u64 {
                    query.bind(u as i64)
                } else {
                    query.bind(u as f64)
                }
            } else {
                query.bind(n.as_f64().unwrap_or(0.0))
            }
        }
        Value::String(s) => query.bind(s.clone()),
        other => query.bind(other.to_string()),
    }
}

fn busy_or(error: sqlx::Error) -> DbError {
    let text = error.to_string();
    let lower = text.to_ascii_lowercase();
    if lower.contains("locked") || lower.contains("busy") {
        DbError::Unsupported(format!(
            "SQLite 单写者：已有导入/写事务占用数据库（等待 {}ms 后仍被拒绝），本次导入未执行，请稍后重试",
            WRITE_LOCK_WAIT_MS
        ))
    } else {
        DbError::from(error)
    }
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// 等待一个已有写事务释放锁的时间上限，供文档/测试引用。
pub fn write_lock_wait() -> Duration {
    Duration::from_millis(WRITE_LOCK_WAIT_MS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::{SystemTime, UNIX_EPOCH};

    async fn temp_pool(name: &str) -> (SqlitePool, String) {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "sqlite_transfer_{}_{}_{}.db",
            name,
            std::process::id(),
            stamp
        ));
        let url = format!("sqlite://{}?mode=rwc", path.to_string_lossy());
        let pool = sqlx::SqlitePool::connect(&url).await.unwrap();
        (pool, path.to_string_lossy().to_string())
    }

    fn row(id: i64, v: &str) -> Vec<Value> {
        vec![json!(id), json!(v)]
    }

    #[tokio::test]
    async fn rolls_back_entire_job_on_duplicate_key() {
        let (pool, path) = temp_pool("rollback").await;
        sqlx::query("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT NOT NULL)")
            .execute(&pool)
            .await
            .unwrap();

        let outcome = import_rows_atomic(
            &pool,
            "t",
            &["id".to_string(), "v".to_string()],
            &[row(1, "a"), row(2, "b"), row(2, "dup")],
        )
        .await
        .unwrap();
        assert!(outcome.atomic);
        assert_eq!(outcome.rollback_scope, "job");
        assert!(outcome.rolled_back);
        assert_eq!(outcome.inserted, 0);
        assert!(outcome.error.is_some());
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM t")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0, "失败后库里不能残留任何行");
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn commits_all_rows_on_success() {
        let (pool, path) = temp_pool("success").await;
        sqlx::query("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
            .execute(&pool)
            .await
            .unwrap();
        let rows: Vec<Vec<Value>> = (1..=5).map(|i| row(i, &format!("v{}", i))).collect();
        let outcome =
            import_rows_atomic(&pool, "t", &["id".to_string(), "v".to_string()], &rows)
                .await
                .unwrap();
        assert!(!outcome.rolled_back);
        assert_eq!(outcome.inserted, 5);
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM t")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 5);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn rejects_concurrent_writer_explicitly() {
        let (pool, path) = temp_pool("busy").await;
        sqlx::query("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
            .execute(&pool)
            .await
            .unwrap();

        let mut blocker = pool.acquire().await.unwrap();
        sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut *blocker)
            .await
            .unwrap();

        let err = import_rows_atomic(
            &pool,
            "t",
            &["id".to_string(), "v".to_string()],
            &[row(1, "a")],
        )
        .await
        .err()
        .expect("must reject while another writer holds the lock");
        let message = err.to_string();
        assert!(
            message.contains("SQLite 单写者"),
            "unexpected error: {}",
            message
        );

        sqlx::query("ROLLBACK").execute(&mut *blocker).await.unwrap();
        let _ = std::fs::remove_file(path);
    }
}
