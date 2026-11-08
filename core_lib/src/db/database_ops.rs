//! DatabaseOps — 统一数据库操作 trait
//!
//! 消除 service 层对 DbClient 具体实现（mysql_pool()）的直接依赖，
//! 使 WorkbenchService / SchemaService / CrudService 可以通过 trait 操作，
//! 不关心底层是 MySQL、PostgreSQL 还是 SQLite。
//!
//! 本模块还提供 `EngineConnection`（引擎无关的单连接句柄），供事务会话和
//! 可取消查询使用。占位符与标识符引用由各引擎显式处理，不做字符串替换。

use crate::config::DbType;
use crate::db::client::{DbClient, DbError, DbPool, PlaceholderStyle};
use crate::db::protocol::BoxFuture;
use crate::sql_util;

use sqlx::{Column, Row};
use std::sync::Arc;

// ── DatabaseOps trait ──────────────────────────────────────

/// 统一数据库操作 trait — service 层通过此 trait 访问数据库
pub trait DatabaseOps: Send + Sync + Clone + 'static {
    /// 返回数据库类型
    fn db_type(&self) -> DbType;

    /// Ping 数据库验证连接存活
    fn ping(&self) -> BoxFuture<'_, Result<(), DbError>>;

    /// 执行一条 SQL 并返回 (columns, rows_as_json_text, affected_rows)
    fn execute_query(&self, sql: &str) -> BoxFuture<'_, Result<QueryResult, DbError>>;

    /// 执行一条只返回 affected_rows 的 SQL（INSERT/UPDATE/DELETE）
    fn execute_command(&self, sql: &str) -> BoxFuture<'_, Result<u64, DbError>>;

    /// 返回 placeholder 样式（? vs $1）
    fn placeholder_style(&self) -> PlaceholderStyle;

    /// 引用标识符
    fn quote_ident(&self, s: &str) -> String;

    /// Kill 查询（MySQL: KILL QUERY；PostgreSQL: pg_terminate_backend；SQLite: no-op）
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

impl QueryResult {
    /// 把行数据按 columns 组合成 JSON 对象数组。
    pub fn to_objects(&self) -> Vec<serde_json::Value> {
        self.rows
            .iter()
            .map(|row| {
                let mut map = serde_json::Map::with_capacity(self.columns.len());
                for (idx, name) in self.columns.iter().enumerate() {
                    let value = row.get(idx).cloned().unwrap_or(serde_json::Value::Null);
                    map.insert(name.clone(), value);
                }
                serde_json::Value::Object(map)
            })
            .collect()
    }
}

fn columns_of_mysql(rows: &[sqlx::mysql::MySqlRow]) -> Vec<String> {
    rows.first()
        .map(|r| r.columns().iter().map(|c| c.name().to_string()).collect())
        .unwrap_or_default()
}

fn columns_of_pg(rows: &[sqlx::postgres::PgRow]) -> Vec<String> {
    rows.first()
        .map(|r| r.columns().iter().map(|c| c.name().to_string()).collect())
        .unwrap_or_default()
}

fn columns_of_sqlite(rows: &[sqlx::sqlite::SqliteRow]) -> Vec<String> {
    rows.first()
        .map(|r| r.columns().iter().map(|c| c.name().to_string()).collect())
        .unwrap_or_default()
}

fn mysql_rows_to_result(rows: Vec<sqlx::mysql::MySqlRow>) -> QueryResult {
    let columns = columns_of_mysql(&rows);
    let count = columns.len();
    let row_values = rows
        .iter()
        .map(|r| {
            (0..count)
                .map(|i| sql_util::mysql_cell_to_value(r, i))
                .collect()
        })
        .collect();
    QueryResult {
        columns,
        rows: row_values,
        affected_rows: 0,
    }
}

fn pg_rows_to_result(rows: Vec<sqlx::postgres::PgRow>) -> QueryResult {
    let columns = columns_of_pg(&rows);
    let count = columns.len();
    let row_values = rows
        .iter()
        .map(|r| (0..count).map(|i| sql_util::pg_cell_to_value(r, i)).collect())
        .collect();
    QueryResult {
        columns,
        rows: row_values,
        affected_rows: 0,
    }
}

fn sqlite_rows_to_result(rows: Vec<sqlx::sqlite::SqliteRow>) -> QueryResult {
    let columns = columns_of_sqlite(&rows);
    let count = columns.len();
    let row_values = rows
        .iter()
        .map(|r| {
            (0..count)
                .map(|i| sql_util::sqlite_cell_to_value(r, i))
                .collect()
        })
        .collect();
    QueryResult {
        columns,
        rows: row_values,
        affected_rows: 0,
    }
}

// ── EngineConnection ──────────────────────────────────────

/// 引擎无关的单连接句柄。
///
/// 事务会话和可取消查询需要把执行固定在同一条连接上；该枚举替代原先
/// 直接绑定的 `PoolConnection<MySql>`。
#[derive(Debug)]
pub enum EngineConnection {
    MySQL(sqlx::pool::PoolConnection<sqlx::MySql>),
    Postgres(sqlx::pool::PoolConnection<sqlx::Postgres>),
    SQLite(sqlx::pool::PoolConnection<sqlx::Sqlite>),
}

impl EngineConnection {
    pub fn db_type(&self) -> DbType {
        match self {
            EngineConnection::MySQL(_) => DbType::MySQL,
            EngineConnection::Postgres(_) => DbType::PostgreSQL,
            EngineConnection::SQLite(_) => DbType::SQLite,
        }
    }

    pub fn placeholder_style(&self) -> PlaceholderStyle {
        match self {
            EngineConnection::MySQL(_) | EngineConnection::SQLite(_) => {
                PlaceholderStyle::QuestionMark
            }
            EngineConnection::Postgres(_) => PlaceholderStyle::DollarNumber,
        }
    }

    pub fn quote_ident(&self, s: &str) -> String {
        match self {
            EngineConnection::MySQL(_) => sql_util::quote_ident_mysql(s),
            EngineConnection::Postgres(_) | EngineConnection::SQLite(_) => {
                sql_util::quote_ident_pg(s)
            }
        }
    }

    /// 按引擎引用表名（支持 `schema.table` 形式）。
    pub fn quote_table_ref(&self, name: &str) -> String {
        match self {
            EngineConnection::MySQL(_) => sql_util::quote_table_ref_mysql(name),
            EngineConnection::Postgres(_) => {
                sql_util::quote_table_ref_checked(name, &DbType::PostgreSQL)
                    .unwrap_or_else(|_| sql_util::quote_ident_pg(name))
            }
            EngineConnection::SQLite(_) => sql_util::quote_ident_pg(name),
        }
    }

    /// 返回可用于取消查询的会话标识。
    /// MySQL: `CONNECTION_ID()`；PostgreSQL: `pg_backend_pid()`；SQLite: 0（无法取消）。
    pub async fn backend_id(&mut self) -> Result<u64, DbError> {
        match self {
            EngineConnection::MySQL(conn) => {
                let row = sqlx::query("SELECT CONNECTION_ID() AS connection_id")
                    .fetch_one(&mut **conn)
                    .await?;
                Ok(row.try_get::<u64, _>("connection_id")?)
            }
            EngineConnection::Postgres(conn) => {
                let row = sqlx::query("SELECT pg_backend_pid() AS connection_id")
                    .fetch_one(&mut **conn)
                    .await?;
                Ok(row.try_get::<i32, _>("connection_id")? as u64)
            }
            EngineConnection::SQLite(_) => Ok(0),
        }
    }

    /// 在当前连接上执行查询并返回统一结果。
    pub async fn fetch_all(&mut self, sql: &str) -> Result<QueryResult, DbError> {
        match self {
            EngineConnection::MySQL(conn) => {
                let rows = sqlx::query(sql).fetch_all(&mut **conn).await?;
                Ok(mysql_rows_to_result(rows))
            }
            EngineConnection::Postgres(conn) => {
                let rows = sqlx::query(sql).fetch_all(&mut **conn).await?;
                Ok(pg_rows_to_result(rows))
            }
            EngineConnection::SQLite(conn) => {
                let rows = sqlx::query(sql).fetch_all(&mut **conn).await?;
                Ok(sqlite_rows_to_result(rows))
            }
        }
    }

    /// 在当前连接上执行命令并返回影响行数。
    ///
    /// 事务控制语句（START TRANSACTION / COMMIT / ROLLBACK / BEGIN）在 MySQL
    /// 的 prepared statement 协议里会报 1295；这里统一用 `raw_sql` 直接下发文本。
    pub async fn execute_command(&mut self, sql: &str) -> Result<u64, DbError> {
        // 注意：必须通过 `Executor::execute` 调用，而不是
        // `sqlx::raw_sql(sql).execute(conn)`。后者的 async fn 返回类型在
        // 指定 `+ Send` 时会触发 "implementation of `Executor` is not general
        // enough"，导致上层 handler 的 future 非 Send；显式走 trait 方法
        // 得到 `BoxFuture`（其内部已声明 Send）即可绕开。
        use sqlx::Executor;
        match self {
            EngineConnection::MySQL(conn) => {
                let result = Executor::execute(&mut **conn, sqlx::raw_sql(sql)).await?;
                Ok(result.rows_affected())
            }
            EngineConnection::Postgres(conn) => {
                let result = Executor::execute(&mut **conn, sqlx::raw_sql(sql)).await?;
                Ok(result.rows_affected())
            }
            EngineConnection::SQLite(conn) => {
                let result = Executor::execute(&mut **conn, sqlx::raw_sql(sql)).await?;
                Ok(result.rows_affected())
            }
        }
    }

    /// 事务起始语句：SQLite 用 `BEGIN`，其余用 `START TRANSACTION`。
    pub fn begin_statement(&self) -> &'static str {
        match self {
            EngineConnection::SQLite(_) => "BEGIN",
            _ => "START TRANSACTION",
        }
    }
}

// ── DbPool 统一执行 ──────────────────────────────────────

impl DbPool {
    /// 从连接池获取一条引擎无关连接。
    pub async fn acquire_engine(&self) -> Result<EngineConnection, DbError> {
        Ok(match self {
            DbPool::MySQL(p) => EngineConnection::MySQL(p.acquire().await?),
            DbPool::Postgres(p) => EngineConnection::Postgres(p.acquire().await?),
            DbPool::SQLite(p) => EngineConnection::SQLite(p.acquire().await?),
        })
    }

    /// 在连接池上执行查询。
    pub async fn fetch_all_result(&self, sql: &str) -> Result<QueryResult, DbError> {
        match self {
            DbPool::MySQL(p) => {
                let rows = sqlx::query(sql).fetch_all(p).await?;
                Ok(mysql_rows_to_result(rows))
            }
            DbPool::Postgres(p) => {
                let rows = sqlx::query(sql).fetch_all(p).await?;
                Ok(pg_rows_to_result(rows))
            }
            DbPool::SQLite(p) => {
                let rows = sqlx::query(sql).fetch_all(p).await?;
                Ok(sqlite_rows_to_result(rows))
            }
        }
    }

    /// 在连接池上执行命令。
    pub async fn execute_affected(&self, sql: &str) -> Result<u64, DbError> {
        match self {
            DbPool::MySQL(p) => Ok(sqlx::query(sql).execute(p).await?.rows_affected()),
            DbPool::Postgres(p) => Ok(sqlx::query(sql).execute(p).await?.rows_affected()),
            DbPool::SQLite(p) => Ok(sqlx::query(sql).execute(p).await?.rows_affected()),
        }
    }

    /// 按引擎引用表名（支持 `schema.table` 形式）。
    pub fn quote_table_ref(&self, name: &str) -> String {
        match self {
            DbPool::MySQL(_) => crate::sql_util::quote_table_ref_mysql(name),
            DbPool::Postgres(_) => {
                crate::sql_util::quote_table_ref_checked(name, &DbType::PostgreSQL)
                    .unwrap_or_else(|_| crate::sql_util::quote_ident_pg(name))
            }
            DbPool::SQLite(_) => crate::sql_util::quote_ident_pg(name),
        }
    }

    /// 终止指定会话上的查询。
    pub async fn kill_backend(&self, backend_id: u64) -> Result<(), DbError> {
        match self {
            DbPool::MySQL(p) => {
                sqlx::query(&format!("KILL QUERY {}", backend_id))
                    .execute(p)
                    .await?;
                Ok(())
            }
            DbPool::Postgres(p) => {
                sqlx::query("SELECT pg_terminate_backend($1) AS killed")
                    .bind(backend_id as i32)
                    .execute(p)
                    .await?;
                Ok(())
            }
            DbPool::SQLite(_) => Ok(()),
        }
    }
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
        Box::pin(async move { self.pool.fetch_all_result(&sql).await })
    }

    fn execute_command(&self, sql: &str) -> BoxFuture<'_, Result<u64, DbError>> {
        let sql = sql.to_string();
        Box::pin(async move { self.pool.execute_affected(&sql).await })
    }

    fn placeholder_style(&self) -> PlaceholderStyle {
        self.pool.placeholder_style()
    }

    fn quote_ident(&self, s: &str) -> String {
        self.pool.quote_ident(s)
    }

    fn kill_query(&self, connection_id: u64) -> BoxFuture<'_, Result<(), DbError>> {
        Box::pin(async move { self.pool.kill_backend(connection_id).await })
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
