use crate::config::DbType;
use crate::db_protocol::*;
use crate::error::AppError;
use sqlx::{Column, Row};

#[derive(Debug, Clone)]
pub struct SqliteAdapter {
    pool: sqlx::SqlitePool,
}

impl SqliteAdapter {
    pub fn new(pool: sqlx::SqlitePool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &sqlx::SqlitePool {
        &self.pool
    }
}

impl UnifiedQueryEngine for SqliteAdapter {
    fn db_type(&self) -> DbType {
        DbType::SQLite
    }

    fn execute<'a>(
        &'a self,
        req: UnifiedQueryRequest,
    ) -> BoxFuture<'a, Result<UnifiedQueryResult, AppError>> {
        Box::pin(async move {
            let rows = sqlx::query(&req.statement)
                .fetch_all(&self.pool)
                .await?;

            if rows.is_empty() {
                return Ok(UnifiedQueryResult {
                    columns: vec![],
                    rows: vec![],
                    affected_rows: None,
                });
            }

            let columns: Vec<String> = rows[0]
                .columns()
                .iter()
                .map(|c| c.name().to_string())
                .collect();

            let mut result_rows = Vec::with_capacity(rows.len());
            for row in &rows {
                let mut vals = Vec::with_capacity(columns.len());
                for i in 0..columns.len() {
                    vals.push(crate::sql_util::sqlite_cell_to_value(row, i));
                }
                result_rows.push(vals);
            }

            Ok(UnifiedQueryResult {
                columns,
                rows: result_rows,
                affected_rows: None,
            })
        })
    }
}

impl UnifiedMetadataProvider for SqliteAdapter {
    fn db_type(&self) -> DbType {
        DbType::SQLite
    }

    fn list_databases<'a>(&'a self) -> BoxFuture<'a, Result<Vec<String>, AppError>> {
        Box::pin(async move {
            // SQLite has no multi-database concept; return the current database name
            let row = sqlx::query("PRAGMA database_list")
                .fetch_one(&self.pool)
                .await?;
            let name = row.try_get::<String, _>("file").unwrap_or_else(|_| ":memory:".to_string());
            Ok(vec![name])
        })
    }

    fn list_tables<'a>(
        &'a self,
        _database: &str,
    ) -> BoxFuture<'a, Result<Vec<UnifiedTableRef>, AppError>> {
        Box::pin(async move {
            let rows = sqlx::query(
                "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
            )
            .fetch_all(&self.pool)
            .await?;

            let mut tables = Vec::with_capacity(rows.len());
            for row in rows {
                if let Ok(name) = row.try_get::<String, _>(0) {
                    tables.push(UnifiedTableRef {
                        database: None,
                        schema: None,
                        name,
                    });
                }
            }
            Ok(tables)
        })
    }

    fn get_table_schema<'a>(
        &'a self,
        table: UnifiedTableRef,
    ) -> BoxFuture<'a, Result<UnifiedTableSchema, AppError>> {
        let table_name = table.name.clone();
        Box::pin(async move {
            let pragma_sql = format!("PRAGMA table_info({})", crate::sql_util::quote_ident_pg(&table_name));
            let rows = sqlx::query(&pragma_sql)
                .fetch_all(&self.pool)
                .await?;

            let mut columns = Vec::with_capacity(rows.len());
            for row in rows {
                columns.push(UnifiedColumn {
                    name: row.try_get::<String, _>("name").unwrap_or_default(),
                    data_type: row.try_get::<String, _>("type").unwrap_or_default(),
                    is_nullable: row.try_get::<bool, _>("notnull").map(|v| !v).unwrap_or(true),
                    comment: None,
                });
            }

            Ok(UnifiedTableSchema { table, columns })
        })
    }
}

