use crate::db::{DbClient, DbError};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrudRequest {
    pub table_name: String,
    // Using a generic JSON value to represent row data
    pub data: serde_json::Value,
    // Used for updates and deletes
    pub condition: Option<serde_json::Map<String, serde_json::Value>>,
}

pub struct CrudManager;

impl CrudManager {
    /// Generates and executes an INSERT statement
    pub async fn insert(client: &DbClient, req: &CrudRequest) -> Result<u64, DbError> {
        let obj = req
            .data
            .as_object()
            .ok_or_else(|| DbError::MissingData("data is not an object".into()))?;

        let columns: Vec<&String> = obj.keys().collect();
        let placeholders: Vec<String> = (0..columns.len()).map(|_| "?".to_string()).collect();

        let sql = format!(
            "INSERT INTO `{}` ({}) VALUES ({})",
            req.table_name,
            columns
                .iter()
                .map(|k| format!("`{}`", k))
                .collect::<Vec<_>>()
                .join(", "),
            placeholders.join(", ")
        );

        let mut query = sqlx::query(&sql);
        for col in &columns {
            let val = obj
                .get(*col)
                .ok_or_else(|| DbError::MissingData(format!("missing column: {}", col)))?;
            // Simple mapping for MVP
            if let Some(s) = val.as_str() {
                query = query.bind(s);
            } else if let Some(n) = val.as_i64() {
                query = query.bind(n);
            } else if let Some(f) = val.as_f64() {
                query = query.bind(f);
            } else if let Some(b) = val.as_bool() {
                query = query.bind(b);
            } else if val.is_null() {
                query = query.bind(None::<String>);
            } else {
                query = query.bind(val.to_string());
            }
        }

        let result = query.execute(&client.pool).await?;
        Ok(result.rows_affected())
    }

    /// Generates and executes an UPDATE statement
    pub async fn update(client: &DbClient, req: &CrudRequest) -> Result<u64, DbError> {
        let obj = req
            .data
            .as_object()
            .ok_or_else(|| DbError::MissingData("data is not an object".into()))?;
        let condition = req
            .condition
            .as_ref()
            .ok_or_else(|| DbError::MissingData("missing condition".into()))?;

        let columns: Vec<&String> = obj.keys().collect();

        let set_clause = columns
            .iter()
            .map(|k| format!("`{}` = ?", k))
            .collect::<Vec<_>>()
            .join(", ");

        let mut where_clauses = Vec::new();
        for (k, val) in condition.iter() {
            if val.is_null() {
                where_clauses.push(format!("`{}` IS NULL", k));
            } else {
                where_clauses.push(format!("`{}` = ?", k));
            }
        }

        let sql = format!(
            "UPDATE `{}` SET {} WHERE {}",
            req.table_name,
            set_clause,
            where_clauses.join(" AND ")
        );

        let mut query = sqlx::query(&sql);
        for col in &columns {
            let val = obj
                .get(*col)
                .ok_or_else(|| DbError::MissingData(format!("missing column: {}", col)))?;
            if let Some(s) = val.as_str() {
                query = query.bind(s);
            } else if let Some(n) = val.as_i64() {
                query = query.bind(n);
            } else if let Some(f) = val.as_f64() {
                query = query.bind(f);
            } else if let Some(b) = val.as_bool() {
                query = query.bind(b);
            } else if val.is_null() {
                query = query.bind(None::<String>);
            } else {
                query = query.bind(val.to_string());
            }
        }

        for val in condition.values() {
            if val.is_null() {
                continue;
            }
            if let Some(s) = val.as_str() {
                query = query.bind(s);
            } else if let Some(n) = val.as_i64() {
                query = query.bind(n);
            } else if let Some(f) = val.as_f64() {
                query = query.bind(f);
            } else if let Some(b) = val.as_bool() {
                query = query.bind(b);
            } else {
                query = query.bind(val.to_string());
            }
        }

        let result = query.execute(&client.pool).await?;
        Ok(result.rows_affected())
    }

    /// Generates and executes a DELETE statement
    pub async fn delete(
        client: &DbClient,
        table_name: &str,
        condition: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<u64, DbError> {
        let mut where_clauses = Vec::new();
        for (k, val) in condition.iter() {
            if val.is_null() {
                where_clauses.push(format!("`{}` IS NULL", k));
            } else {
                where_clauses.push(format!("`{}` = ?", k));
            }
        }

        let sql = format!(
            "DELETE FROM `{}` WHERE {}",
            table_name,
            where_clauses.join(" AND ")
        );

        let mut query = sqlx::query(&sql);
        for val in condition.values() {
            if val.is_null() {
                continue;
            }
            if let Some(s) = val.as_str() {
                query = query.bind(s);
            } else if let Some(n) = val.as_i64() {
                query = query.bind(n);
            } else if let Some(f) = val.as_f64() {
                query = query.bind(f);
            } else if let Some(b) = val.as_bool() {
                query = query.bind(b);
            } else {
                query = query.bind(val.to_string());
            }
        }

        let result = query.execute(&client.pool).await?;
        Ok(result.rows_affected())
    }
}
