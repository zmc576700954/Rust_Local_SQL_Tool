use crate::config::DbType;
use crate::db_protocol::*;
use crate::error::AppError;
use crate::sql_util;
use sqlx::{Column, Row};

#[derive(Debug, Clone)]
pub struct PgAdapter {
    pool: sqlx::PgPool,
}

impl PgAdapter {
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &sqlx::PgPool {
        &self.pool
    }
}

impl UnifiedQueryEngine for PgAdapter {
    fn db_type(&self) -> DbType {
        DbType::PostgreSQL
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
                    vals.push(sql_util::pg_cell_to_value(row, i));
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

impl UnifiedMetadataProvider for PgAdapter {
    fn db_type(&self) -> DbType {
        DbType::PostgreSQL
    }

    fn list_databases<'a>(&'a self) -> BoxFuture<'a, Result<Vec<String>, AppError>> {
        Box::pin(async move {
            let rows = sqlx::query("SELECT datname FROM pg_database WHERE datistemplate = false ORDER BY datname")
                .fetch_all(&self.pool)
                .await?;
            let mut dbs = Vec::with_capacity(rows.len());
            for row in rows {
                if let Ok(name) = row.try_get::<String, _>(0) {
                    dbs.push(name);
                }
            }
            Ok(dbs)
        })
    }

    fn list_tables<'a>(
        &'a self,
        database: &str,
    ) -> BoxFuture<'a, Result<Vec<UnifiedTableRef>, AppError>> {
        let db = database.to_string();
        Box::pin(async move {
            // PostgreSQL: query information_schema in the specified database
            // Note: cross-database queries aren't directly supported in PG,
            // so we query the current database's schema
            let rows = sqlx::query(
                "SELECT table_name FROM information_schema.tables
                 WHERE table_schema = 'public' AND table_catalog = $1
                 ORDER BY table_name",
            )
            .bind(&db)
            .fetch_all(&self.pool)
            .await?;

            let mut tables = Vec::with_capacity(rows.len());
            for row in rows {
                if let Ok(name) = row.try_get::<String, _>(0) {
                    tables.push(UnifiedTableRef {
                        database: Some(db.clone()),
                        schema: Some("public".to_string()),
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
        Box::pin(async move {
            let rows = sqlx::query(
                "SELECT column_name, data_type, is_nullable, column_default
                 FROM information_schema.columns
                 WHERE table_schema = 'public' AND table_name = $1
                 ORDER BY ordinal_position",
            )
            .bind(&table.name)
            .fetch_all(&self.pool)
            .await?;

            let mut columns = Vec::with_capacity(rows.len());
            for row in rows {
                columns.push(UnifiedColumn {
                    name: row.try_get::<String, _>("column_name").unwrap_or_default(),
                    data_type: row.try_get::<String, _>("data_type").unwrap_or_default(),
                    is_nullable: row
                        .try_get::<String, _>("is_nullable")
                        .unwrap_or_else(|_| "YES".to_string())
                        == "YES",
                    comment: None,
                });
            }

            Ok(UnifiedTableSchema { table, columns })
        })
    }
}

