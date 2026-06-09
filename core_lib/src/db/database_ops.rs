//! DatabaseOps — 统一数据库操作 trait
//!
//! 消除 service 层对 DbClient 具体实现（mysql_pool()）的直接依赖，
//! 使 WorkbenchService / SchemaService / CrudService 可以通过 trait 操作，
//! 不关心底层是 MySQL、PostgreSQL 还是 SQLite。
//!
//! 设计原则：
//! - trait 方法覆盖 service 层真正需要的操作（ping、查询、kill、quote）
//! - DbClient 作为默认实现，保持向后兼容
//! - MySQL 特有方法（kill_query、connection_id）标记为 optional

use crate::config::DbType;
use crate::db::client::{DbClient, DbError, DbPool, PlaceholderStyle};
use crate::db::protocol::BoxFuture;

use sqlx::{Column, Row};
use std::sync::Arc;

// ── DatabaseOps trait ──────────────────────────────────────

/// 统一数据库操作 trait — service 层通过此 trait 访问数据库
///
/// 覆盖核心操作：ping、查询执行、kill_query（optional）、quote_ident。
/// 不包含 pool 创建/连接管理（由 DbClient::new 处理）。
pub trait DatabaseOps: Send + Sync + Clone + 'static {
    /// 返回数据库类型
    fn db_type(&self) -> DbType;

    /// Ping 数据库验证连接存活
    fn ping(&self) -> BoxFuture<'_, Result<(), DbError>>;

    /// 执行一条 SQL 并返回 (columns, rows_as_json_text, affected_rows)
    ///
    /// columns: 列名列表
    /// rows_as_json_text: 每行是一个 serde_json::Value（由引擎适配器编码）
    /// affected_rows: INSERT/UPDATE/DELETE 的影响行数
    fn execute_query(&self, sql: &str) -> BoxFuture<'_, Result<QueryResult, DbError>>;

    /// 执行一条只返回 affected_rows 的 SQL（INSERT/UPDATE/DELETE）
    fn execute_command(&self, sql: &str) -> BoxFuture<'_, Result<u64, DbError>>;

    /// 返回 placeholder 样式（? vs $1）
    fn placeholder_style(&self) -> PlaceholderStyle;

    /// 引用标识符
    fn quote_ident(&self, s: &str) -> String;

    /// Kill 查询（MySQL-only，其他引擎返回 Ok(())）
    fn kill_query(&self, connection_id: u64) -> BoxFuture<'_, Result<(), DbError>>;

    /// 关闭连接池
    fn close(&self) -> BoxFuture<'_, ()>;

    /// 获取 Arc<Self> — 用于 spawned task 持有引用
    fn into_arc(self) -> Arc<Self>;
}

// ── QueryResult ────────────────────────────────────────────

/// 通用查询结果 — 所有引擎共享
#[derive(Debug, Clone)]
pub struct QueryResult {
    pub columns: Vec<String>,
    /// 每行是一组 serde_json::Value，列顺序与 columns 对应
    pub rows: Vec<Vec<serde_json::Value>>,
    pub affected_rows: u64,
}

// ── MySQL row → JSON helper ─────────────────────────────────

/// 将 MySQL row 的各列转为 serde_json::Value
/// 使用列索引号而非列名（避免 sqlx ColumnIndex trait bound 问题）
fn mysql_row_to_json(row: &sqlx::mysql::MySqlRow, column_count: usize) -> Vec<serde_json::Value> {
    use sqlx::Row;
    (0..column_count)
        .map(|i| {
            row.try_get::<serde_json::Value, usize>(i)
                .unwrap_or(serde_json::Value::Null)
        })
        .collect()
}

/// 将 PostgreSQL row 的各列转为 serde_json::Value
fn pg_row_to_json(row: &sqlx::postgres::PgRow, column_count: usize) -> Vec<serde_json::Value> {
    use sqlx::Row;
    (0..column_count)
        .map(|i| {
            row.try_get::<serde_json::Value, usize>(i)
                .unwrap_or(serde_json::Value::Null)
        })
        .collect()
}

/// 将 SQLite row 的各列转为 serde_json::Value
fn sqlite_row_to_json(row: &sqlx::sqlite::SqliteRow, column_count: usize) -> Vec<serde_json::Value> {
    use sqlx::Row;
    (0..column_count)
        .map(|i| {
            row.try_get::<serde_json::Value, usize>(i)
                .unwrap_or(serde_json::Value::Null)
        })
        .collect()
}

// ── DbClient 实现 DatabaseOps ──────────────────────────────

impl DatabaseOps for DbClient {
    fn db_type(&self) -> DbType {
        self.db_type.clone()
    }

    fn ping(&self) -> BoxFuture<'_, Result<(), DbError>> {
        Box::pin(async move { self.pool.ping().await })
    }

    fn execute_query(&self, sql: &str) -> BoxFuture<'_, Result<QueryResult, DbError>> {
        let sql = sql.to_string();
        Box::pin(async move {
            match &self.pool {
                DbPool::MySQL(pool) => {
                    let rows = sqlx::query(&sql).fetch_all(pool).await?;
                    let columns: Vec<String> = rows
                        .first()
                        .map(|r| {
                            r.columns()
                                .iter()
                                .map(|c| c.name().to_string())
                                .collect()
                        })
                        .unwrap_or_default();
                    let column_count = columns.len();
                    let json_rows = rows.iter().map(|r| mysql_row_to_json(r, column_count)).collect();
                    Ok(QueryResult {
                        columns,
                        rows: json_rows,
                        affected_rows: 0,
                    })
                }
                DbPool::Postgres(pool) => {
                    let rows = sqlx::query(&sql).fetch_all(pool).await?;
                    let columns: Vec<String> = rows
                        .first()
                        .map(|r| {
                            r.columns()
                                .iter()
                                .map(|c| c.name().to_string())
                                .collect()
                        })
                        .unwrap_or_default();
                    let column_count = columns.len();
                    let json_rows = rows.iter().map(|r| pg_row_to_json(r, column_count)).collect();
                    Ok(QueryResult {
                        columns,
                        rows: json_rows,
                        affected_rows: 0,
                    })
                }
                DbPool::SQLite(pool) => {
                    let rows = sqlx::query(&sql).fetch_all(pool).await?;
                    let columns: Vec<String> = rows
                        .first()
                        .map(|r| {
                            r.columns()
                                .iter()
                                .map(|c| c.name().to_string())
                                .collect()
                        })
                        .unwrap_or_default();
                    let column_count = columns.len();
                    let json_rows = rows.iter().map(|r| sqlite_row_to_json(r, column_count)).collect();
                    Ok(QueryResult {
                        columns,
                        rows: json_rows,
                        affected_rows: 0,
                    })
                }
            }
        })
    }

    fn execute_command(&self, sql: &str) -> BoxFuture<'_, Result<u64, DbError>> {
        let sql = sql.to_string();
        Box::pin(async move {
            match &self.pool {
                DbPool::MySQL(pool) => {
                    let result = sqlx::query(&sql).execute(pool).await?;
                    Ok(result.rows_affected())
                }
                DbPool::Postgres(pool) => {
                    let result = sqlx::query(&sql).execute(pool).await?;
                    Ok(result.rows_affected())
                }
                DbPool::SQLite(pool) => {
                    let result = sqlx::query(&sql).execute(pool).await?;
                    Ok(result.rows_affected())
                }
            }
        })
    }

    fn placeholder_style(&self) -> PlaceholderStyle {
        self.pool.placeholder_style()
    }

    fn quote_ident(&self, s: &str) -> String {
        self.pool.quote_ident(s)
    }

    fn kill_query(&self, connection_id: u64) -> BoxFuture<'_, Result<(), DbError>> {
        // MySQL: KILL QUERY; 其他引擎: no-op (返回 Ok)
        Box::pin(async move {
            match &self.pool {
                DbPool::MySQL(pool) => {
                    let sql = format!("KILL QUERY {}", connection_id);
                    sqlx::query(&sql).execute(pool).await?;
                    Ok(())
                }
                _ => Ok(()), // PostgreSQL/SQLite 不支持 kill query
            }
        })
    }

    fn close(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move { self.pool.close().await })
    }

    fn into_arc(self) -> Arc<Self> {
        Arc::new(self)
    }
}

// ── 辅助：获取 MySQL-only pool（向后兼容） ────────────────

/// 获取 MySQL pool 的便捷方法 — 仅在 MySQL 连接时使用
/// 保留向后兼容，service 层迁移后应逐步消除此调用
impl DbClient {
    /// 向后兼容：获取 MySQL pool
    /// 新代码应使用 DatabaseOps trait 方法代替
    pub fn as_mysql_pool(&self) -> Result<&sqlx::MySqlPool, DbError> {
        self.pool.mysql()
    }
}