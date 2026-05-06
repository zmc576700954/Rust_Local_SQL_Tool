use crate::db::{DbClient, DbError};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexInfo {
    pub index_name: String,
    pub column_name: String,
    pub non_unique: bool,
    pub index_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViewInfo {
    pub table_name: String,
    pub view_definition: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForeignKeyInfo {
    pub constraint_name: String,
    pub column_name: String,
    pub referenced_table_name: String,
    pub referenced_column_name: String,
    pub update_rule: String,
    pub delete_rule: String,
}

use crate::schema::SchemaExtractor;

impl SchemaExtractor {
    /// Fetches all views in a specific database schema
    pub async fn get_views(client: &DbClient, db_name: &str) -> Result<Vec<ViewInfo>, DbError> {
        let query = r#"
            SELECT TABLE_NAME as table_name, VIEW_DEFINITION as view_definition 
            FROM information_schema.VIEWS 
            WHERE TABLE_SCHEMA = ?
        "#;

        let views = sqlx::query(query)
            .bind(db_name)
            .fetch_all(&client.pool)
            .await?;

        let mut result = Vec::new();
        use sqlx::Row;
        for row in views {
            result.push(ViewInfo {
                table_name: row.try_get::<String, _>("table_name")?,
                view_definition: row.try_get::<String, _>("view_definition")?,
            });
        }

        Ok(result)
    }

    /// Fetches all indexes for a specific table
    pub async fn get_indexes(
        client: &DbClient,
        db_name: &str,
        table_name: &str,
    ) -> Result<Vec<IndexInfo>, DbError> {
        let query = r#"
            SELECT 
                INDEX_NAME as index_name, 
                COLUMN_NAME as column_name, 
                NON_UNIQUE as non_unique, 
                INDEX_TYPE as index_type 
            FROM information_schema.STATISTICS 
            WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?
            ORDER BY SEQ_IN_INDEX
        "#;

        let indexes = sqlx::query(query)
            .bind(db_name)
            .bind(table_name)
            .fetch_all(&client.pool)
            .await?;

        let mut result = Vec::new();
        use sqlx::Row;
        for row in indexes {
            result.push(IndexInfo {
                index_name: row.try_get::<String, _>("index_name")?,
                column_name: row.try_get::<String, _>("column_name")?,
                // In MySQL, NON_UNIQUE is 1 for non-unique (true), 0 for unique (false)
                non_unique: row.try_get::<i64, _>("non_unique").unwrap_or(1) != 0,
                index_type: row.try_get::<String, _>("index_type")?,
            });
        }

        Ok(result)
    }

    /// Fetches all foreign keys for a specific table
    pub async fn get_foreign_keys(
        client: &DbClient,
        db_name: &str,
        table_name: &str,
    ) -> Result<Vec<ForeignKeyInfo>, DbError> {
        let query = r#"
            SELECT 
                kcu.CONSTRAINT_NAME as constraint_name,
                kcu.COLUMN_NAME as column_name,
                kcu.REFERENCED_TABLE_NAME as referenced_table_name,
                kcu.REFERENCED_COLUMN_NAME as referenced_column_name,
                rc.UPDATE_RULE as update_rule,
                rc.DELETE_RULE as delete_rule
            FROM information_schema.KEY_COLUMN_USAGE kcu
            JOIN information_schema.REFERENTIAL_CONSTRAINTS rc 
              ON kcu.CONSTRAINT_NAME = rc.CONSTRAINT_NAME 
              AND kcu.CONSTRAINT_SCHEMA = rc.CONSTRAINT_SCHEMA
            WHERE kcu.TABLE_SCHEMA = ? 
              AND kcu.TABLE_NAME = ?
              AND kcu.REFERENCED_TABLE_NAME IS NOT NULL
        "#;

        let fks = sqlx::query(query)
            .bind(db_name)
            .bind(table_name)
            .fetch_all(&client.pool)
            .await?;

        let mut result = Vec::new();
        use sqlx::Row;
        for row in fks {
            result.push(ForeignKeyInfo {
                constraint_name: row.try_get::<String, _>("constraint_name")?,
                column_name: row.try_get::<String, _>("column_name")?,
                referenced_table_name: row.try_get::<String, _>("referenced_table_name")?,
                referenced_column_name: row.try_get::<String, _>("referenced_column_name")?,
                update_rule: row.try_get::<String, _>("update_rule")?,
                delete_rule: row.try_get::<String, _>("delete_rule")?,
            });
        }

        Ok(result)
    }
}
