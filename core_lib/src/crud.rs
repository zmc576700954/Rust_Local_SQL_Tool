use crate::db::client::{DbPool, PlaceholderStyle};
use crate::db::DbError;
use crate::sql_util;
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

// ---------------------------------------------------------------------------
// SQL-building helpers (shared across all pool types)
// ---------------------------------------------------------------------------

/// Build an INSERT SQL string with the given placeholder style and quoting function.
fn build_insert_sql(
    table: &str,
    columns: &[&String],
    style: PlaceholderStyle,
    quote: impl Fn(&str) -> String,
) -> String {
    let cols = columns
        .iter()
        .map(|k| quote(k))
        .collect::<Vec<_>>()
        .join(", ");
    let placeholders: Vec<String> = match style {
        PlaceholderStyle::QuestionMark => {
            (0..columns.len()).map(|_| "?".to_string()).collect()
        }
        PlaceholderStyle::DollarNumber => {
            (1..=columns.len()).map(|i| format!("${}", i)).collect()
        }
        PlaceholderStyle::AtP => {
            (1..=columns.len()).map(|i| format!("@p{}", i)).collect()
        }
        PlaceholderStyle::ColonNumber => {
            (1..=columns.len()).map(|i| format!(":{}", i)).collect()
        }
    };
    format!(
        "INSERT INTO {} ({}) VALUES ({})",
        quote(table),
        cols,
        placeholders.join(", ")
    )
}

/// Build an UPDATE SQL string. SET columns are numbered first, then WHERE columns.
fn build_update_sql(
    table: &str,
    columns: &[&String],
    condition_keys: Vec<&String>,
    style: PlaceholderStyle,
    quote: impl Fn(&str) -> String,
) -> String {
    let set_clause = match style {
        PlaceholderStyle::QuestionMark => columns
            .iter()
            .map(|k| format!("{} = ?", quote(k)))
            .collect::<Vec<_>>()
            .join(", "),
        PlaceholderStyle::DollarNumber => columns
            .iter()
            .enumerate()
            .map(|(i, k)| format!("{} = ${}", quote(k), i + 1))
            .collect::<Vec<_>>()
            .join(", "),
        PlaceholderStyle::AtP => columns
            .iter()
            .enumerate()
            .map(|(i, k)| format!("{} = @p{}", quote(k), i + 1))
            .collect::<Vec<_>>()
            .join(", "),
        PlaceholderStyle::ColonNumber => columns
            .iter()
            .enumerate()
            .map(|(i, k)| format!("{} = :{}", quote(k), i + 1))
            .collect::<Vec<_>>()
            .join(", "),
    };

    let n = columns.len();
    let mut where_clauses = Vec::new();
    for (idx, k) in condition_keys.iter().enumerate() {
        let placeholder = match style {
            PlaceholderStyle::QuestionMark => "?".to_string(),
            PlaceholderStyle::DollarNumber => format!("${}", n + idx + 1),
            PlaceholderStyle::AtP => format!("@p{}", n + idx + 1),
            PlaceholderStyle::ColonNumber => format!(":{}", n + idx + 1),
        };
        where_clauses.push(format!("{} = {}", quote(k), placeholder));
    }

    format!(
        "UPDATE {} SET {} WHERE {}",
        quote(table),
        set_clause,
        where_clauses.join(" AND ")
    )
}

/// Build a DELETE SQL string. WHERE columns are numbered first.
fn build_delete_sql(
    table: &str,
    condition_keys: Vec<&String>,
    style: PlaceholderStyle,
    quote: impl Fn(&str) -> String,
) -> String {
    let mut where_clauses = Vec::new();
    for (idx, k) in condition_keys.iter().enumerate() {
        let placeholder = match style {
            PlaceholderStyle::QuestionMark => "?".to_string(),
            PlaceholderStyle::DollarNumber => format!("${}", idx + 1),
            PlaceholderStyle::AtP => format!("@p{}", idx + 1),
            PlaceholderStyle::ColonNumber => format!(":{}", idx + 1),
        };
        where_clauses.push(format!("{} = {}", quote(k), placeholder));
    }

    format!(
        "DELETE FROM {} WHERE {}",
        quote(table),
        where_clauses.join(" AND ")
    )
}

/// Build WHERE clause for NULL-check conditions (no placeholder needed).
fn build_null_where_clauses(
    condition: &serde_json::Map<String, serde_json::Value>,
    quote: impl Fn(&str) -> String,
) -> (Vec<String>, Vec<&String>) {
    let mut null_clauses = Vec::new();
    let mut non_null_keys = Vec::new();
    for (k, val) in condition.iter() {
        if val.is_null() {
            null_clauses.push(format!("{} IS NULL", quote(k)));
        } else {
            non_null_keys.push(k);
        }
    }
    (null_clauses, non_null_keys)
}

// ---------------------------------------------------------------------------
// Macro to bind a serde_json::Value to a sqlx query (reduces per-arm duplication)
// ---------------------------------------------------------------------------

macro_rules! bind_value {
    ($query:expr, $val:expr) => {{
        let val = $val;
        if let Some(s) = val.as_str() {
            $query = $query.bind(s);
        } else if let Some(n) = val.as_i64() {
            $query = $query.bind(n);
        } else if let Some(f) = val.as_f64() {
            $query = $query.bind(f);
        } else if let Some(b) = val.as_bool() {
            $query = $query.bind(b);
        } else if val.is_null() {
            $query = $query.bind(None::<String>);
        } else {
            $query = $query.bind(val.to_string());
        }
    }};
}

// ---------------------------------------------------------------------------
// CrudManager implementation
// ---------------------------------------------------------------------------

impl CrudManager {
    /// Generates and executes an INSERT statement
    pub async fn insert(pool: &DbPool, req: &CrudRequest) -> Result<u64, DbError> {
        let obj = req
            .data
            .as_object()
            .ok_or_else(|| DbError::MissingData("data is not an object".into()))?;

        let columns: Vec<&String> = obj.keys().collect();
        let style = pool.placeholder_style();
        let sql = build_insert_sql(&req.table_name, &columns, style, |s| pool.quote_ident(s));

        match pool {
            DbPool::MySQL(p) => {
                let mut query = sqlx::query(&sql);
                for col in &columns {
                    let val = obj
                        .get(*col)
                        .ok_or_else(|| DbError::MissingData(format!("missing column: {}", col)))?;
                    bind_value!(query, val);
                }
                let result = query.execute(p).await?;
                Ok(result.rows_affected())
            }
            DbPool::Postgres(p) => {
                let mut query = sqlx::query(&sql);
                for col in &columns {
                    let val = obj
                        .get(*col)
                        .ok_or_else(|| DbError::MissingData(format!("missing column: {}", col)))?;
                    bind_value!(query, val);
                }
                let result = query.execute(p).await?;
                Ok(result.rows_affected())
            }
            DbPool::SQLite(p) => {
                let mut query = sqlx::query(&sql);
                for col in &columns {
                    let val = obj
                        .get(*col)
                        .ok_or_else(|| DbError::MissingData(format!("missing column: {}", col)))?;
                    bind_value!(query, val);
                }
                let result = query.execute(p).await?;
                Ok(result.rows_affected())
            }
        }
    }

    /// Generates and executes an UPDATE statement
    pub async fn update(pool: &DbPool, req: &CrudRequest) -> Result<u64, DbError> {
        let obj = req
            .data
            .as_object()
            .ok_or_else(|| DbError::MissingData("data is not an object".into()))?;
        let condition = req
            .condition
            .as_ref()
            .ok_or_else(|| DbError::MissingData("missing condition".into()))?;
        if condition.is_empty() {
            return Err(DbError::MissingData(
                "condition must not be empty — refusing UPDATE without WHERE".into(),
            ));
        }

        let columns: Vec<&String> = obj.keys().collect();
        let style = pool.placeholder_style();
        let quote = |s: &str| pool.quote_ident(s);

        // Separate NULL conditions (use IS NULL, no placeholder) from non-NULL
        let (null_clauses, non_null_condition_keys) = build_null_where_clauses(condition, &quote);

        let mut sql = build_update_sql(&req.table_name, &columns, non_null_condition_keys.clone(), style, &quote);
        if !null_clauses.is_empty() {
            sql.push_str(" AND ");
            sql.push_str(&null_clauses.join(" AND "));
        }

        match pool {
            DbPool::MySQL(p) => {
                let mut query = sqlx::query(&sql);
                for col in &columns {
                    let val = obj
                        .get(*col)
                        .ok_or_else(|| DbError::MissingData(format!("missing column: {}", col)))?;
                    bind_value!(query, val);
                }
                for k in &non_null_condition_keys {
                    let val = condition
                        .get(*k)
                        .ok_or_else(|| DbError::MissingData(format!("missing condition key: {}", k)))?;
                    bind_value!(query, val);
                }
                let result = query.execute(p).await?;
                Ok(result.rows_affected())
            }
            DbPool::Postgres(p) => {
                let mut query = sqlx::query(&sql);
                for col in &columns {
                    let val = obj
                        .get(*col)
                        .ok_or_else(|| DbError::MissingData(format!("missing column: {}", col)))?;
                    bind_value!(query, val);
                }
                for k in &non_null_condition_keys {
                    let val = condition
                        .get(*k)
                        .ok_or_else(|| DbError::MissingData(format!("missing condition key: {}", k)))?;
                    bind_value!(query, val);
                }
                let result = query.execute(p).await?;
                Ok(result.rows_affected())
            }
            DbPool::SQLite(p) => {
                let mut query = sqlx::query(&sql);
                for col in &columns {
                    let val = obj
                        .get(*col)
                        .ok_or_else(|| DbError::MissingData(format!("missing column: {}", col)))?;
                    bind_value!(query, val);
                }
                for k in &non_null_condition_keys {
                    let val = condition
                        .get(*k)
                        .ok_or_else(|| DbError::MissingData(format!("missing condition key: {}", k)))?;
                    bind_value!(query, val);
                }
                let result = query.execute(p).await?;
                Ok(result.rows_affected())
            }
        }
    }

    /// Generates and executes a DELETE statement
    pub async fn delete(
        pool: &DbPool,
        table_name: &str,
        condition: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<u64, DbError> {
        if condition.is_empty() {
            return Err(DbError::MissingData(
                "condition must not be empty — refusing DELETE without WHERE".into(),
            ));
        }

        let style = pool.placeholder_style();
        let quote = |s: &str| pool.quote_ident(s);

        // Separate NULL conditions (use IS NULL, no placeholder) from non-NULL
        let (null_clauses, non_null_condition_keys) = build_null_where_clauses(condition, &quote);

        let mut sql = build_delete_sql(table_name, non_null_condition_keys.clone(), style, &quote);
        if !null_clauses.is_empty() {
            sql.push_str(" AND ");
            sql.push_str(&null_clauses.join(" AND "));
        }

        match pool {
            DbPool::MySQL(p) => {
                let mut query = sqlx::query(&sql);
                for k in &non_null_condition_keys {
                    let val = condition
                        .get(*k)
                        .ok_or_else(|| DbError::MissingData(format!("missing condition key: {}", k)))?;
                    bind_value!(query, val);
                }
                let result = query.execute(p).await?;
                Ok(result.rows_affected())
            }
            DbPool::Postgres(p) => {
                let mut query = sqlx::query(&sql);
                for k in &non_null_condition_keys {
                    let val = condition
                        .get(*k)
                        .ok_or_else(|| DbError::MissingData(format!("missing condition key: {}", k)))?;
                    bind_value!(query, val);
                }
                let result = query.execute(p).await?;
                Ok(result.rows_affected())
            }
            DbPool::SQLite(p) => {
                let mut query = sqlx::query(&sql);
                for k in &non_null_condition_keys {
                    let val = condition
                        .get(*k)
                        .ok_or_else(|| DbError::MissingData(format!("missing condition key: {}", k)))?;
                    bind_value!(query, val);
                }
                let result = query.execute(p).await?;
                Ok(result.rows_affected())
            }
        }
    }

    /// MySQL-only: generates and executes an INSERT using a raw MySQL executor.
    /// Used for transaction sessions that hold a `PoolConnection<MySql>`.
    pub async fn insert_mysql<'e, E>(executor: E, req: &CrudRequest) -> Result<u64, DbError>
    where
        E: sqlx::Executor<'e, Database = sqlx::MySql>,
    {
        let obj = req
            .data
            .as_object()
            .ok_or_else(|| DbError::MissingData("data is not an object".into()))?;

        let columns: Vec<&String> = obj.keys().collect();
        let placeholders: Vec<String> = (0..columns.len()).map(|_| "?".to_string()).collect();

        let sql = format!(
            "INSERT INTO {} ({}) VALUES ({})",
            sql_util::quote_ident_mysql(&req.table_name),
            columns
                .iter()
                .map(|k| sql_util::quote_ident_mysql(k))
                .collect::<Vec<_>>()
                .join(", "),
            placeholders.join(", ")
        );

        let mut query = sqlx::query(&sql);
        for col in &columns {
            let val = obj
                .get(*col)
                .ok_or_else(|| DbError::MissingData(format!("missing column: {}", col)))?;
            bind_value!(query, val);
        }

        let result = query.execute(executor).await?;
        Ok(result.rows_affected())
    }

    /// MySQL-only: generates and executes an UPDATE using a raw MySQL executor.
    /// Used for transaction sessions that hold a `PoolConnection<MySql>`.
    pub async fn update_mysql<'e, E>(executor: E, req: &CrudRequest) -> Result<u64, DbError>
    where
        E: sqlx::Executor<'e, Database = sqlx::MySql>,
    {
        let obj = req
            .data
            .as_object()
            .ok_or_else(|| DbError::MissingData("data is not an object".into()))?;
        let condition = req
            .condition
            .as_ref()
            .ok_or_else(|| DbError::MissingData("missing condition".into()))?;
        if condition.is_empty() {
            return Err(DbError::MissingData(
                "condition must not be empty — refusing UPDATE without WHERE".into(),
            ));
        }

        let columns: Vec<&String> = obj.keys().collect();

        let set_clause = columns
            .iter()
            .map(|k| format!("{} = ?", sql_util::quote_ident_mysql(k)))
            .collect::<Vec<_>>()
            .join(", ");

        let mut where_clauses = Vec::new();
        let mut non_null_values: Vec<&serde_json::Value> = Vec::new();
        for (k, val) in condition.iter() {
            if val.is_null() {
                where_clauses.push(format!("{} IS NULL", sql_util::quote_ident_mysql(k)));
            } else {
                where_clauses.push(format!("{} = ?", sql_util::quote_ident_mysql(k)));
                non_null_values.push(val);
            }
        }

        let sql = format!(
            "UPDATE {} SET {} WHERE {}",
            sql_util::quote_ident_mysql(&req.table_name),
            set_clause,
            where_clauses.join(" AND ")
        );

        let mut query = sqlx::query(&sql);
        for col in &columns {
            let val = obj
                .get(*col)
                .ok_or_else(|| DbError::MissingData(format!("missing column: {}", col)))?;
            bind_value!(query, val);
        }
        for val in non_null_values {
            bind_value!(query, val);
        }

        let result = query.execute(executor).await?;
        Ok(result.rows_affected())
    }

    /// MySQL-only: generates and executes a DELETE using a raw MySQL executor.
    /// Used for transaction sessions that hold a `PoolConnection<MySql>`.
    pub async fn delete_mysql<'e, E>(
        executor: E,
        table_name: &str,
        condition: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<u64, DbError>
    where
        E: sqlx::Executor<'e, Database = sqlx::MySql>,
    {
        if condition.is_empty() {
            return Err(DbError::MissingData(
                "condition must not be empty — refusing DELETE without WHERE".into(),
            ));
        }

        let mut where_clauses = Vec::new();
        let mut non_null_values: Vec<&serde_json::Value> = Vec::new();
        for (k, val) in condition.iter() {
            if val.is_null() {
                where_clauses.push(format!("{} IS NULL", sql_util::quote_ident_mysql(k)));
            } else {
                where_clauses.push(format!("{} = ?", sql_util::quote_ident_mysql(k)));
                non_null_values.push(val);
            }
        }

        let sql = format!(
            "DELETE FROM {} WHERE {}",
            sql_util::quote_ident_mysql(table_name),
            where_clauses.join(" AND ")
        );

        let mut query = sqlx::query(&sql);
        for val in non_null_values {
            bind_value!(query, val);
        }

        let result = query.execute(executor).await?;
        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_insert_sql_mysql_style() {
        let name = "name".to_string();
        let age = "age".to_string();
        let cols = vec![&name, &age];
        let sql = build_insert_sql(
            "users",
            &cols,
            PlaceholderStyle::QuestionMark,
            sql_util::quote_ident_mysql,
        );
        assert_eq!(sql, "INSERT INTO `users` (`name`, `age`) VALUES (?, ?)");
    }

    #[test]
    fn build_insert_sql_postgres_style() {
        let name = "name".to_string();
        let age = "age".to_string();
        let cols = vec![&name, &age];
        let sql = build_insert_sql(
            "users",
            &cols,
            PlaceholderStyle::DollarNumber,
            sql_util::quote_ident_pg,
        );
        assert_eq!(sql, "INSERT INTO \"users\" (\"name\", \"age\") VALUES ($1, $2)");
    }

    #[test]
    fn build_update_sql_mysql_style() {
        let name = "name".to_string();
        let age = "age".to_string();
        let id = "id".to_string();
        let cols = vec![&name, &age];
        let cond = vec![&id];
        let sql = build_update_sql(
            "users",
            &cols,
            cond,
            PlaceholderStyle::QuestionMark,
            sql_util::quote_ident_mysql,
        );
        assert_eq!(sql, "UPDATE `users` SET `name` = ?, `age` = ? WHERE `id` = ?");
    }

    #[test]
    fn build_update_sql_postgres_style() {
        let name = "name".to_string();
        let age = "age".to_string();
        let id = "id".to_string();
        let cols = vec![&name, &age];
        let cond = vec![&id];
        let sql = build_update_sql(
            "users",
            &cols,
            cond,
            PlaceholderStyle::DollarNumber,
            sql_util::quote_ident_pg,
        );
        assert_eq!(
            sql,
            "UPDATE \"users\" SET \"name\" = $1, \"age\" = $2 WHERE \"id\" = $3"
        );
    }

    #[test]
    fn build_delete_sql_mysql_style() {
        let id = "id".to_string();
        let cond = vec![&id];
        let sql = build_delete_sql(
            "users",
            cond,
            PlaceholderStyle::QuestionMark,
            sql_util::quote_ident_mysql,
        );
        assert_eq!(sql, "DELETE FROM `users` WHERE `id` = ?");
    }

    #[test]
    fn build_delete_sql_postgres_style() {
        let id = "id".to_string();
        let org_id = "org_id".to_string();
        let cond = vec![&id, &org_id];
        let sql = build_delete_sql(
            "users",
            cond,
            PlaceholderStyle::DollarNumber,
            sql_util::quote_ident_pg,
        );
        assert_eq!(sql, "DELETE FROM \"users\" WHERE \"id\" = $1 AND \"org_id\" = $2");
    }

    #[test]
    fn null_where_clauses() {
        let mut condition = serde_json::Map::new();
        condition.insert("id".to_string(), serde_json::json!(42));
        condition.insert("deleted_at".to_string(), serde_json::Value::Null);

        let (null_clauses, non_null_keys) =
            build_null_where_clauses(&condition, sql_util::quote_ident_mysql);

        assert_eq!(null_clauses, vec!["`deleted_at` IS NULL"]);
        let expected_key = "id".to_string();
        assert_eq!(non_null_keys, vec![&expected_key]);
    }
}
