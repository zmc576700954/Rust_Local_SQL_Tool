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
        use crate::config::DbType;
        use sqlx::Row;

        match &client.db_type {
            DbType::MySQL | DbType::MariaDB => {
                let query = r#"
                    SELECT TABLE_NAME as table_name, TABLE_COMMENT as table_comment 
                    FROM information_schema.TABLES 
                    WHERE TABLE_SCHEMA = ?
                "#;
                let tables = sqlx::query(query)
                    .bind(db_name)
                    .fetch_all(client.mysql_pool()?)
                    .await?;

                let mut result = Vec::new();
                for row in tables {
                    result.push(TableInfo {
                        table_name: crate::sql_util::mysql_text(&row, "table_name")
                            .unwrap_or_default(),
                        table_comment: crate::sql_util::mysql_text(&row, "table_comment"),
                    });
                }
                Ok(result)
            }
            DbType::PostgreSQL => {
                // PostgreSQL 的 schema 概念：列出除系统 schema 外的所有用户 schema。
                // public 下的表用裸表名；其他 schema 用 `schema.table` 作为展示名。
                let query = r#"
                    SELECT n.nspname AS table_schema,
                           c.relname AS table_name,
                           obj_description(c.oid, 'pg_class') AS table_comment
                    FROM pg_class c
                    JOIN pg_namespace n ON n.oid = c.relnamespace
                    WHERE c.relkind = 'r'
                      AND n.nspname NOT IN ('pg_catalog', 'information_schema')
                    ORDER BY n.nspname, c.relname
                "#;
                let tables = sqlx::query(query).fetch_all(client.pool.pg()?).await?;
                let mut result = Vec::new();
                for row in tables {
                    let schema: String = row.try_get("table_schema")?;
                    let table: String = row.try_get("table_name")?;
                    result.push(TableInfo {
                        table_name: pg_display_name(&schema, &table),
                        table_comment: row.try_get("table_comment").unwrap_or_default(),
                    });
                }
                Ok(result)
            }
            DbType::SQLite => Err(DbError::Unsupported(
                "SQLite 结构内省当前版本未支持（仅支持连接与查询执行）".into(),
            )),
            other => Err(DbError::Unsupported(other.display_name().into())),
        }
    }

    /// Fetches all columns for a specific table
    pub async fn get_columns(
        client: &DbClient,
        db_name: &str,
        table_name: &str,
    ) -> Result<Vec<ColumnInfo>, DbError> {
        use crate::config::DbType;

        match &client.db_type {
            DbType::MySQL | DbType::MariaDB => {
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
                    .fetch_all(client.mysql_pool()?)
                    .await?;

                let mut result = Vec::new();
                for row in columns {
                    result.push(ColumnInfo {
                        column_name: crate::sql_util::mysql_text(&row, "column_name")
                            .unwrap_or_default(),
                        data_type: crate::sql_util::mysql_text(&row, "data_type")
                            .unwrap_or_default(),
                        column_type: crate::sql_util::mysql_text(&row, "column_type")
                            .unwrap_or_default(),
                        is_nullable: crate::sql_util::mysql_text(&row, "is_nullable")
                            .unwrap_or_default(),
                        column_comment: crate::sql_util::mysql_text(&row, "column_comment"),
                        column_key: crate::sql_util::mysql_text(&row, "column_key")
                            .unwrap_or_default(),
                        column_default: crate::sql_util::mysql_text(&row, "column_default"),
                        extra: crate::sql_util::mysql_text(&row, "extra").unwrap_or_default(),
                    });
                }
                Ok(result)
            }
            DbType::PostgreSQL => {
                let (schema, table) = crate::sql_util::split_pg_table_ref(table_name);
                let schema = schema.unwrap_or("public");
                Self::pg_columns(client, schema, Some(table))
                    .await
                    .map(|rows| rows.into_iter().map(|r| r.column).collect())
            }
            DbType::SQLite => Err(DbError::Unsupported(
                "SQLite 结构内省当前版本未支持（仅支持连接与查询执行）".into(),
            )),
            other => Err(DbError::Unsupported(other.display_name().into())),
        }
    }

    /// Fetches all columns for all tables in a specific database schema
    pub async fn get_columns_map(
        client: &DbClient,
        db_name: &str,
    ) -> Result<HashMap<String, Vec<ColumnInfo>>, DbError> {
        use crate::config::DbType;
        use sqlx::Row;

        match &client.db_type {
            DbType::MySQL | DbType::MariaDB => {
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
                    .fetch_all(client.mysql_pool()?)
                    .await?;

                let mut result: HashMap<String, Vec<ColumnInfo>> = HashMap::new();
                for row in rows {
                    let table_name = crate::sql_util::mysql_text(&row, "table_name")
                        .unwrap_or_default();
                    result.entry(table_name).or_default().push(ColumnInfo {
                        column_name: crate::sql_util::mysql_text(&row, "column_name")
                            .unwrap_or_default(),
                        data_type: crate::sql_util::mysql_text(&row, "data_type")
                            .unwrap_or_default(),
                        column_type: crate::sql_util::mysql_text(&row, "column_type")
                            .unwrap_or_default(),
                        is_nullable: crate::sql_util::mysql_text(&row, "is_nullable")
                            .unwrap_or_default(),
                        column_comment: crate::sql_util::mysql_text(&row, "column_comment"),
                        column_key: crate::sql_util::mysql_text(&row, "column_key")
                            .unwrap_or_default(),
                        column_default: crate::sql_util::mysql_text(&row, "column_default"),
                        extra: crate::sql_util::mysql_text(&row, "extra").unwrap_or_default(),
                    });
                }
                Ok(result)
            }
            DbType::PostgreSQL => {
                // 跨所有用户 schema 拉取列，并按展示名（public 裸名 / 其他 schema.table）分组。
                let rows = Self::pg_columns(client, "%", None).await?;
                let mut result: HashMap<String, Vec<ColumnInfo>> = HashMap::new();
                for col in rows {
                    result
                        .entry(col.table_name)
                        .or_default()
                        .push(col.column);
                }
                Ok(result)
            }
            DbType::SQLite => Err(DbError::Unsupported(
                "SQLite 结构内省当前版本未支持（仅支持连接与查询执行）".into(),
            )),
            other => Err(DbError::Unsupported(other.display_name().into())),
        }
    }

    /// PostgreSQL 列元数据（带 schema 过滤；schema = "%" 表示全部用户 schema）。
    async fn pg_columns(
        client: &DbClient,
        schema: &str,
        table: Option<&str>,
    ) -> Result<Vec<PgColumnRow>, DbError> {
        use sqlx::Row;

        // PG 没有 TABLE_COMMENT/COLUMN_KEY 这类 MySQL 元数据；类型用 format_type 保留
        // 精度/长度/数组元素类型，主键与唯一键从 information_schema 约束反推。
        let sql = r#"
            SELECT n.nspname AS table_schema,
                   c.relname AS table_name,
                   a.attname AS column_name,
                   t.typname AS data_type,
                   format_type(a.atttypid, a.atttypmod) AS column_type,
                   CASE WHEN a.attnotnull THEN 'NO' ELSE 'YES' END AS is_nullable,
                   col_description(a.attrelid, a.attnum) AS column_comment,
                   pg_get_expr(d.adbin, d.adrelid) AS column_default,
                   CASE WHEN a.attidentity IN ('a','d') THEN 'identity' ELSE '' END AS extra
            FROM pg_attribute a
            JOIN pg_class c ON c.oid = a.attrelid
            JOIN pg_namespace n ON n.oid = c.relnamespace
            JOIN pg_type t ON t.oid = a.atttypid
            LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum
            WHERE n.nspname NOT IN ('pg_catalog', 'information_schema')
              AND c.relkind IN ('r', 'p')
              AND a.attnum > 0
              AND NOT a.attisdropped
              AND ($1 = '%' OR n.nspname = $1)
              AND ($2 = '' OR c.relname = $2)
            ORDER BY n.nspname, c.relname, a.attnum
        "#;
        let table_filter = table.unwrap_or("");
        let rows = sqlx::query(sql)
            .bind(schema)
            .bind(table_filter)
            .fetch_all(client.pool.pg()?)
            .await?;

        let keys = Self::pg_key_map(client, schema, table).await?;

        let mut result = Vec::new();
        for row in rows {
            let schema_name: String = row.try_get("table_schema")?;
            let table_name: String = row.try_get("table_name")?;
            let column_name: String = row.try_get("column_name")?;
            let mut extra: String = row.try_get("extra").unwrap_or_default();
            let column_default: Option<String> = row.try_get("column_default").unwrap_or_default();
            if extra.is_empty() {
                if let Some(def) = column_default.as_deref() {
                    if def.starts_with("nextval(") {
                        extra = "auto_increment".to_string();
                    }
                }
            }
            let key = keys
                .get(&(pg_display_name(&schema_name, &table_name), column_name.clone()))
                .cloned()
                .unwrap_or_default();
            result.push(PgColumnRow {
                table_name: pg_display_name(&schema_name, &table_name),
                column: ColumnInfo {
                    column_name,
                    data_type: row.try_get("data_type")?,
                    column_type: row.try_get("column_type")?,
                    is_nullable: row.try_get("is_nullable")?,
                    column_comment: row.try_get("column_comment").unwrap_or_default(),
                    column_key: key,
                    column_default,
                    extra,
                },
            });
        }
        Ok(result)
    }

    /// PostgreSQL 主键/唯一键映射：键为 (展示表名, 列名)，值为 PRI / UNI。
    async fn pg_key_map(
        client: &DbClient,
        schema: &str,
        table: Option<&str>,
    ) -> Result<HashMap<(String, String), String>, DbError> {
        use sqlx::Row;
        let sql = r#"
            SELECT tc.table_schema,
                   tc.table_name,
                   kcu.column_name,
                   MAX(CASE WHEN tc.constraint_type = 'PRIMARY KEY' THEN 2 ELSE 1 END) AS key_rank
            FROM information_schema.table_constraints tc
            JOIN information_schema.key_column_usage kcu
              ON kcu.constraint_name = tc.constraint_name
             AND kcu.table_schema = tc.table_schema
             AND kcu.table_name = tc.table_name
            WHERE tc.constraint_type IN ('PRIMARY KEY', 'UNIQUE')
              AND tc.table_schema NOT IN ('pg_catalog', 'information_schema')
              AND ($1 = '%' OR tc.table_schema = $1)
              AND ($2 = '' OR tc.table_name = $2)
            GROUP BY tc.table_schema, tc.table_name, kcu.column_name
        "#;
        let rows = sqlx::query(sql)
            .bind(schema)
            .bind(table.unwrap_or(""))
            .fetch_all(client.pool.pg()?)
            .await?;

        let mut map = HashMap::new();
        for row in rows {
            let schema_name: String = row.try_get("table_schema")?;
            let table_name: String = row.try_get("table_name")?;
            let column_name: String = row.try_get("column_name")?;
            let rank: i32 = row.try_get("key_rank")?;
            map.insert(
                (pg_display_name(&schema_name, &table_name), column_name),
                if rank >= 2 { "PRI" } else { "UNI" }.to_string(),
            );
        }
        Ok(map)
    }
}

/// PG 列元数据行（带展示表名，用于分组）。
struct PgColumnRow {
    table_name: String,
    column: ColumnInfo,
}

/// PostgreSQL 展示表名：public schema 用裸名，其他 schema 用 `schema.table`。
pub fn pg_display_name(schema: &str, table: &str) -> String {
    if schema == "public" {
        table.to_string()
    } else {
        format!("{}.{}", schema, table)
    }
}

#[cfg(test)]
mod tests {
    use super::pg_display_name;

    #[test]
    fn pg_display_name_keeps_non_public_schema_qualified() {
        assert_eq!(pg_display_name("public", "users"), "users");
        assert_eq!(pg_display_name("app", "orders"), "app.orders");
    }
}
