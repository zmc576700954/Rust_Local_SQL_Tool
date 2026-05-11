use crate::db::{DbClient, DbError};
use crate::schema_ext::{IndexInfo, ViewInfo};
pub struct SchemaExtractor;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableInfo {
    pub table_name: String,
    pub table_comment: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnInfo {
    pub column_name: String,
    pub data_type: String,
    pub column_type: String,
    pub is_nullable: String,
    pub column_comment: Option<String>,
    pub column_key: String,
    pub column_default: Option<String>,
    pub extra: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaResponse {
    pub db_name: String,
    pub tables: Vec<TableWithDetails>,
    pub views: Vec<ViewInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableWithDetails {
    pub table_name: String,
    pub columns: Vec<ColumnInfo>,
    pub indexes: Vec<IndexInfo>,
    pub foreign_keys: Vec<crate::schema_ext::ForeignKeyInfo>,
}

impl SchemaExtractor {
    /// Fetches all tables in a specific database schema
    pub async fn get_tables(client: &DbClient, db_name: &str) -> Result<Vec<TableInfo>, DbError> {
        let query = r#"
            SELECT TABLE_NAME as table_name, TABLE_COMMENT as table_comment 
            FROM information_schema.TABLES 
            WHERE TABLE_SCHEMA = ?
        "#;

        let tables = sqlx::query(query)
            .bind(db_name)
            .fetch_all(&client.pool)
            .await?;

        // We do manual mapping because `sqlx::query_as` to struct needs #[derive(FromRow)]
        // which sometimes has issues with information_schema column casing.
        let mut result = Vec::new();
        use sqlx::Row;
        for row in tables {
            result.push(TableInfo {
                table_name: row.try_get::<String, _>("table_name")?,
                table_comment: row
                    .try_get::<Option<String>, _>("table_comment")
                    .unwrap_or_default(),
            });
        }

        Ok(result)
    }

    /// Fetches all columns for a specific table
    pub async fn get_columns(
        client: &DbClient,
        db_name: &str,
        table_name: &str,
    ) -> Result<Vec<ColumnInfo>, DbError> {
        let query = r#"
            SELECT 
                COLUMN_NAME as column_name, 
                DATA_TYPE as data_type, 
                COLUMN_TYPE as column_type,
                IS_NULLABLE as is_nullable, 
                COLUMN_COMMENT as column_comment, 
                COLUMN_KEY as column_key,
                COLUMN_DEFAULT as column_default,
                EXTRA as extra
            FROM information_schema.COLUMNS 
            WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? 
            ORDER BY ORDINAL_POSITION
        "#;

        let columns = sqlx::query(query)
            .bind(db_name)
            .bind(table_name)
            .fetch_all(&client.pool)
            .await?;

        let mut result = Vec::new();
        use sqlx::Row;
        for row in columns {
            result.push(ColumnInfo {
                column_name: row.try_get::<String, _>("column_name")?,
                data_type: row.try_get::<String, _>("data_type")?,
                column_type: row.try_get::<String, _>("column_type")?,
                is_nullable: row.try_get::<String, _>("is_nullable")?,
                column_comment: row
                    .try_get::<Option<String>, _>("column_comment")
                    .unwrap_or_default(),
                column_key: row.try_get::<String, _>("column_key")?,
                column_default: row
                    .try_get::<Option<String>, _>("column_default")
                    .unwrap_or_default(),
                extra: row.try_get::<String, _>("extra")?,
            });
        }

        Ok(result)
    }

    /// Fetches all columns for all tables in a specific database schema
    pub async fn get_columns_map(
        client: &DbClient,
        db_name: &str,
    ) -> Result<HashMap<String, Vec<ColumnInfo>>, DbError> {
        let query = r#"
            SELECT 
                TABLE_NAME as table_name,
                COLUMN_NAME as column_name, 
                DATA_TYPE as data_type, 
                COLUMN_TYPE as column_type,
                IS_NULLABLE as is_nullable, 
                COLUMN_COMMENT as column_comment, 
                COLUMN_KEY as column_key,
                COLUMN_DEFAULT as column_default,
                EXTRA as extra
            FROM information_schema.COLUMNS 
            WHERE TABLE_SCHEMA = ? 
            ORDER BY TABLE_NAME, ORDINAL_POSITION
        "#;

        let rows = sqlx::query(query)
            .bind(db_name)
            .fetch_all(&client.pool)
            .await?;

        let mut result: HashMap<String, Vec<ColumnInfo>> = HashMap::new();
        use sqlx::Row;
        for row in rows {
            let table_name = row.try_get::<String, _>("table_name")?;
            result.entry(table_name).or_default().push(ColumnInfo {
                column_name: row.try_get::<String, _>("column_name")?,
                data_type: row.try_get::<String, _>("data_type")?,
                column_type: row.try_get::<String, _>("column_type")?,
                is_nullable: row.try_get::<String, _>("is_nullable")?,
                column_comment: row
                    .try_get::<Option<String>, _>("column_comment")
                    .unwrap_or_default(),
                column_key: row.try_get::<String, _>("column_key")?,
                column_default: row
                    .try_get::<Option<String>, _>("column_default")
                    .unwrap_or_default(),
                extra: row.try_get::<String, _>("extra")?,
            });
        }

        Ok(result)
    }
}
