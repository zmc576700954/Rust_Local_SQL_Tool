use crate::config::DbType;
use crate::db::{DbClient, DbError};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexInfo {
    pub index_name: String,
    pub column_name: String,
    pub non_unique: bool,
    pub index_type: String,
    /// 该列在索引中的序号（从 1 开始），用于还原复合索引的列序。
    #[serde(default)]
    pub seq_in_index: u32,
    /// 排序方向："ASC" 或 "DESC"。
    #[serde(default = "default_sort_order")]
    pub sort_order: String,
    /// 是否为主键索引。
    #[serde(default)]
    pub is_primary: bool,
    /// 是否为表达式索引（PostgreSQL 中 indkey 的 attnum 为 0）。
    #[serde(default)]
    pub is_expression: bool,
    /// 是否为部分索引（SQLite `PRAGMA index_list.partial=1`）。
    #[serde(default)]
    pub is_partial: bool,
    /// 部分索引谓词；SQLite 无法从 PRAGMA 还原时保留为 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub predicate: Option<String>,
    /// 索引原始 DDL（SQLite 取自 `sqlite_master.sql`）。表达式/部分索引依赖它
    /// 才能如实报告 unsupported，而不是静默丢弃。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggerInfo {
    pub trigger_name: String,
    pub table_name: String,
    /// `sqlite_master.sql` 中的原始 `CREATE TRIGGER` 文本。
    pub sql: String,
}

fn default_sort_order() -> String {
    "ASC".to_string()
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
    /// 该列在外键中的序号（从 1 开始），用于还原复合外键的列序。
    #[serde(default)]
    pub ordinal_position: u32,
}

use crate::schema::extractor::pg_display_name;
use crate::schema::SchemaExtractor;

/// MySQL STATISTICS.COLLATION：'A' 升序、'D' 降序、NULL（HASH/FULLTEXT 等）按升序处理。
fn mysql_sort_order(collation: Option<&str>) -> String {
    match collation {
        Some("D") => "DESC".to_string(),
        _ => "ASC".to_string(),
    }
}

impl SchemaExtractor {
    /// Fetches all views in a specific database schema
    pub async fn get_views(client: &DbClient, db_name: &str) -> Result<Vec<ViewInfo>, DbError> {
        match &client.db_type {
            DbType::MySQL | DbType::MariaDB => {
                let query = r#"
                    SELECT TABLE_NAME as table_name, VIEW_DEFINITION as view_definition 
                    FROM information_schema.VIEWS 
                    WHERE TABLE_SCHEMA = ?
                "#;
                let views = sqlx::query(query)
                    .bind(db_name)
                    .fetch_all(client.mysql_pool()?)
                    .await?;

                let mut result = Vec::new();
                for row in views {
                    result.push(ViewInfo {
                        table_name: crate::sql_util::mysql_text(&row, "table_name")
                            .unwrap_or_default(),
                        view_definition: crate::sql_util::mysql_text(&row, "view_definition")
                            .unwrap_or_default(),
                    });
                }
                Ok(result)
            }
            DbType::PostgreSQL => {
                let query = r#"
                    SELECT table_schema, table_name, view_definition
                    FROM information_schema.views
                    WHERE table_schema NOT IN ('pg_catalog', 'information_schema')
                    ORDER BY table_schema, table_name
                "#;
                let rows = sqlx::query(query).fetch_all(client.pool.pg()?).await?;
                let mut result = Vec::new();
                use sqlx::Row;
                for row in rows {
                    let schema: String = row.try_get("table_schema")?;
                    let table: String = row.try_get("table_name")?;
                    result.push(ViewInfo {
                        table_name: pg_display_name(&schema, &table),
                        view_definition: row.try_get("view_definition").unwrap_or_default(),
                    });
                }
                Ok(result)
            }
            DbType::SQLite => crate::schema::sqlite::list_views(client).await,
            other => Err(DbError::Unsupported(other.display_name().into())),
        }
    }

    /// Fetches all indexes for a specific table
    pub async fn get_indexes(
        client: &DbClient,
        db_name: &str,
        table_name: &str,
    ) -> Result<Vec<IndexInfo>, DbError> {
        match &client.db_type {
            DbType::MySQL | DbType::MariaDB => {
                let query = r#"
                    SELECT 
                        INDEX_NAME as index_name, 
                        COLUMN_NAME as column_name, 
                        NON_UNIQUE as non_unique, 
                        INDEX_TYPE as index_type,
                        SEQ_IN_INDEX as seq_in_index,
                        COLLATION as collation
                    FROM information_schema.STATISTICS 
                    WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?
                    ORDER BY INDEX_NAME, SEQ_IN_INDEX
                "#;
                let indexes = sqlx::query(query)
                    .bind(db_name)
                    .bind(table_name)
                    .fetch_all(client.mysql_pool()?)
                    .await?;

                let mut result = Vec::new();
                use sqlx::Row;
                for row in indexes {
                    let index_name = crate::sql_util::mysql_text(&row, "index_name")
                        .unwrap_or_default();
                    let collation = crate::sql_util::mysql_text(&row, "collation");
                    result.push(IndexInfo {
                        is_primary: index_name == "PRIMARY",
                        sort_order: mysql_sort_order(collation.as_deref()),
                        seq_in_index: row
                            .try_get::<u64, _>("seq_in_index")
                            .unwrap_or(0)
                            .min(u32::MAX as u64) as u32,
                        column_name: crate::sql_util::mysql_text(&row, "column_name")
                            .unwrap_or_default(),
                        non_unique: row.try_get::<i64, _>("non_unique").unwrap_or(1) != 0,
                        index_type: crate::sql_util::mysql_text(&row, "index_type")
                            .unwrap_or_default(),
                        is_expression: false,
                        is_partial: false,
                        predicate: None,
                        definition: None,
                        index_name,
                    });
                }
                Ok(result)
            }
            DbType::PostgreSQL => {
                let (schema, table) = crate::sql_util::split_pg_table_ref(table_name);
                let rows = Self::pg_indexes(client, schema.unwrap_or("public"), Some(table)).await?;
                Ok(rows.into_iter().map(|(_, idx)| idx).collect())
            }
            DbType::SQLite => crate::schema::sqlite::list_indexes(client, table_name).await,
            other => Err(DbError::Unsupported(other.display_name().into())),
        }
    }

    /// Fetches all indexes for all tables in a specific database schema
    pub async fn get_indexes_map(
        client: &DbClient,
        db_name: &str,
    ) -> Result<HashMap<String, Vec<IndexInfo>>, DbError> {
        match &client.db_type {
            DbType::MySQL | DbType::MariaDB => {
                let query = r#"
                    SELECT 
                        TABLE_NAME as table_name,
                        INDEX_NAME as index_name, 
                        COLUMN_NAME as column_name, 
                        NON_UNIQUE as non_unique, 
                        INDEX_TYPE as index_type,
                        SEQ_IN_INDEX as seq_in_index,
                        COLLATION as collation
                    FROM information_schema.STATISTICS 
                    WHERE TABLE_SCHEMA = ?
                    ORDER BY TABLE_NAME, INDEX_NAME, SEQ_IN_INDEX
                "#;
                let rows = sqlx::query(query)
                    .bind(db_name)
                    .fetch_all(client.mysql_pool()?)
                    .await?;

                let mut result: HashMap<String, Vec<IndexInfo>> = HashMap::new();
                use sqlx::Row;
                for row in rows {
                    let table_name =
                        crate::sql_util::mysql_text(&row, "table_name").unwrap_or_default();
                    let index_name =
                        crate::sql_util::mysql_text(&row, "index_name").unwrap_or_default();
                    let collation = crate::sql_util::mysql_text(&row, "collation");
                    result.entry(table_name).or_default().push(IndexInfo {
                        is_primary: index_name == "PRIMARY",
                        sort_order: mysql_sort_order(collation.as_deref()),
                        seq_in_index: row
                            .try_get::<u64, _>("seq_in_index")
                            .unwrap_or(0)
                            .min(u32::MAX as u64) as u32,
                        column_name: crate::sql_util::mysql_text(&row, "column_name")
                            .unwrap_or_default(),
                        non_unique: row.try_get::<i64, _>("non_unique").unwrap_or(1) != 0,
                        index_type: crate::sql_util::mysql_text(&row, "index_type")
                            .unwrap_or_default(),
                        is_expression: false,
                        is_partial: false,
                        predicate: None,
                        definition: None,
                        index_name,
                    });
                }
                Ok(result)
            }
            DbType::PostgreSQL => {
                let rows = Self::pg_indexes(client, "%", None).await?;
                let mut result: HashMap<String, Vec<IndexInfo>> = HashMap::new();
                for (table, idx) in rows {
                    result.entry(table).or_default().push(idx);
                }
                Ok(result)
            }
            DbType::SQLite => crate::schema::sqlite::list_indexes_map(client).await,
            other => Err(DbError::Unsupported(other.display_name().into())),
        }
    }

    /// Fetches all foreign keys for a specific table
    pub async fn get_foreign_keys(
        client: &DbClient,
        db_name: &str,
        table_name: &str,
    ) -> Result<Vec<ForeignKeyInfo>, DbError> {
        match &client.db_type {
            DbType::MySQL | DbType::MariaDB => {
                let query = r#"
                    SELECT 
                        kcu.CONSTRAINT_NAME as constraint_name,
                        kcu.COLUMN_NAME as column_name,
                        kcu.REFERENCED_TABLE_NAME as referenced_table_name,
                        kcu.REFERENCED_COLUMN_NAME as referenced_column_name,
                        rc.UPDATE_RULE as update_rule,
                        rc.DELETE_RULE as delete_rule,
                        kcu.ORDINAL_POSITION as ordinal_position
                    FROM information_schema.KEY_COLUMN_USAGE kcu
                    JOIN information_schema.REFERENTIAL_CONSTRAINTS rc 
                      ON kcu.CONSTRAINT_NAME = rc.CONSTRAINT_NAME 
                      AND kcu.CONSTRAINT_SCHEMA = rc.CONSTRAINT_SCHEMA
                    WHERE kcu.TABLE_SCHEMA = ? 
                      AND kcu.TABLE_NAME = ?
                      AND kcu.REFERENCED_TABLE_NAME IS NOT NULL
                    ORDER BY kcu.CONSTRAINT_NAME, kcu.ORDINAL_POSITION
                "#;
                let fks = sqlx::query(query)
                    .bind(db_name)
                    .bind(table_name)
                    .fetch_all(client.mysql_pool()?)
                    .await?;

                let mut result = Vec::new();
                use sqlx::Row;
                for row in fks {
                    result.push(ForeignKeyInfo {
                        constraint_name: crate::sql_util::mysql_text(&row, "constraint_name")
                            .unwrap_or_default(),
                        column_name: crate::sql_util::mysql_text(&row, "column_name")
                            .unwrap_or_default(),
                        referenced_table_name: crate::sql_util::mysql_text(
                            &row,
                            "referenced_table_name",
                        )
                        .unwrap_or_default(),
                        referenced_column_name: crate::sql_util::mysql_text(
                            &row,
                            "referenced_column_name",
                        )
                        .unwrap_or_default(),
                        update_rule: crate::sql_util::mysql_text(&row, "update_rule")
                            .unwrap_or_default(),
                        delete_rule: crate::sql_util::mysql_text(&row, "delete_rule")
                            .unwrap_or_default(),
                        ordinal_position: row
                            .try_get::<u64, _>("ordinal_position")
                            .unwrap_or(0)
                            .min(u32::MAX as u64) as u32,
                    });
                }
                Ok(result)
            }
            DbType::PostgreSQL => {
                let (schema, table) = crate::sql_util::split_pg_table_ref(table_name);
                let rows = Self::pg_foreign_keys(client, schema.unwrap_or("public"), Some(table)).await?;
                Ok(rows.into_iter().map(|(_, fk)| fk).collect())
            }
            DbType::SQLite => crate::schema::sqlite::list_foreign_keys(client, table_name).await,
            other => Err(DbError::Unsupported(other.display_name().into())),
        }
    }

    /// Fetches all foreign keys for all tables in a specific database schema
    pub async fn get_foreign_keys_map(
        client: &DbClient,
        db_name: &str,
    ) -> Result<HashMap<String, Vec<ForeignKeyInfo>>, DbError> {
        match &client.db_type {
            DbType::MySQL | DbType::MariaDB => {
                let query = r#"
                    SELECT 
                        kcu.TABLE_NAME as table_name,
                        kcu.CONSTRAINT_NAME as constraint_name,
                        kcu.COLUMN_NAME as column_name,
                        kcu.REFERENCED_TABLE_NAME as referenced_table_name,
                        kcu.REFERENCED_COLUMN_NAME as referenced_column_name,
                        rc.UPDATE_RULE as update_rule,
                        rc.DELETE_RULE as delete_rule,
                        kcu.ORDINAL_POSITION as ordinal_position
                    FROM information_schema.KEY_COLUMN_USAGE kcu
                    JOIN information_schema.REFERENTIAL_CONSTRAINTS rc 
                      ON kcu.CONSTRAINT_NAME = rc.CONSTRAINT_NAME 
                      AND kcu.CONSTRAINT_SCHEMA = rc.CONSTRAINT_SCHEMA
                    WHERE kcu.TABLE_SCHEMA = ? 
                      AND kcu.REFERENCED_TABLE_NAME IS NOT NULL
                    ORDER BY kcu.TABLE_NAME, kcu.CONSTRAINT_NAME, kcu.ORDINAL_POSITION
                "#;
                let rows = sqlx::query(query)
                    .bind(db_name)
                    .fetch_all(client.mysql_pool()?)
                    .await?;

                let mut result: HashMap<String, Vec<ForeignKeyInfo>> = HashMap::new();
                use sqlx::Row;
                for row in rows {
                    let table_name =
                        crate::sql_util::mysql_text(&row, "table_name").unwrap_or_default();
                    result.entry(table_name).or_default().push(ForeignKeyInfo {
                        constraint_name: crate::sql_util::mysql_text(&row, "constraint_name")
                            .unwrap_or_default(),
                        column_name: crate::sql_util::mysql_text(&row, "column_name")
                            .unwrap_or_default(),
                        referenced_table_name: crate::sql_util::mysql_text(
                            &row,
                            "referenced_table_name",
                        )
                        .unwrap_or_default(),
                        referenced_column_name: crate::sql_util::mysql_text(
                            &row,
                            "referenced_column_name",
                        )
                        .unwrap_or_default(),
                        update_rule: crate::sql_util::mysql_text(&row, "update_rule")
                            .unwrap_or_default(),
                        delete_rule: crate::sql_util::mysql_text(&row, "delete_rule")
                            .unwrap_or_default(),
                        ordinal_position: row
                            .try_get::<u64, _>("ordinal_position")
                            .unwrap_or(0)
                            .min(u32::MAX as u64) as u32,
                    });
                }
                Ok(result)
            }
            DbType::PostgreSQL => {
                let rows = Self::pg_foreign_keys(client, "%", None).await?;
                let mut result: HashMap<String, Vec<ForeignKeyInfo>> = HashMap::new();
                for (table, fk) in rows {
                    result.entry(table).or_default().push(fk);
                }
                Ok(result)
            }
            DbType::SQLite => crate::schema::sqlite::list_foreign_keys_map(client).await,
            other => Err(DbError::Unsupported(other.display_name().into())),
        }
    }

    /// PostgreSQL 索引元数据。返回 (展示表名, 索引信息)。
    async fn pg_indexes(
        client: &DbClient,
        schema: &str,
        table: Option<&str>,
    ) -> Result<Vec<(String, IndexInfo)>, DbError> {
        use sqlx::Row;
        let sql = r#"
            SELECT n.nspname AS table_schema,
                   t.relname AS table_name,
                   i.relname AS index_name,
                   COALESCE(a.attname, '') AS column_name,
                   NOT idx.indisunique AS non_unique,
                   am.amname AS index_type,
                   k.ord::int4 AS seq_in_index,
                   CASE WHEN (idx.indoption[(k.ord - 1)::int] & 1) = 1 THEN 'DESC' ELSE 'ASC' END AS sort_order,
                   idx.indisprimary AS is_primary,
                   (k.attnum = 0) AS is_expression
            FROM pg_index idx
            JOIN pg_class t ON t.oid = idx.indrelid
            JOIN pg_namespace n ON n.oid = t.relnamespace
            JOIN pg_class i ON i.oid = idx.indexrelid
            JOIN pg_am am ON am.oid = i.relam
            LEFT JOIN LATERAL unnest(idx.indkey) WITH ORDINALITY AS k(attnum, ord) ON true
            LEFT JOIN pg_attribute a ON a.attrelid = t.oid AND a.attnum = k.attnum
            WHERE n.nspname NOT IN ('pg_catalog', 'information_schema')
              AND ($1 = '%' OR n.nspname = $1)
              AND ($2 = '' OR t.relname = $2)
            ORDER BY n.nspname, t.relname, i.relname, k.ord
        "#;
        let rows = sqlx::query(sql)
            .bind(schema)
            .bind(table.unwrap_or(""))
            .fetch_all(client.pool.pg()?)
            .await?;

        let mut result = Vec::new();
        for row in rows {
            let schema_name: String = row.try_get("table_schema")?;
            let table_name: String = row.try_get("table_name")?;
            result.push((
                pg_display_name(&schema_name, &table_name),
                IndexInfo {
                    index_name: row.try_get("index_name")?,
                    column_name: row.try_get("column_name")?,
                    non_unique: row.try_get("non_unique")?,
                    index_type: row.try_get("index_type")?,
                    seq_in_index: row.try_get::<i32, _>("seq_in_index").unwrap_or(0).max(0) as u32,
                    sort_order: row.try_get("sort_order")?,
                    is_primary: row.try_get("is_primary")?,
                    is_expression: row.try_get("is_expression")?,
                    is_partial: false,
                    predicate: None,
                    definition: None,
                },
            ));
        }
        Ok(result)
    }

    /// PostgreSQL 外键元数据。返回 (展示表名, 外键信息)。
    async fn pg_foreign_keys(
        client: &DbClient,
        schema: &str,
        table: Option<&str>,
    ) -> Result<Vec<(String, ForeignKeyInfo)>, DbError> {
        use sqlx::Row;
        let sql = r#"
            SELECT n.nspname AS table_schema,
                   cl.relname AS table_name,
                   con.conname AS constraint_name,
                   a.attname AS column_name,
                   CASE WHEN fn.nspname = 'public' THEN fcl.relname
                        ELSE fn.nspname || '.' || fcl.relname END AS referenced_table_name,
                   fa.attname AS referenced_column_name,
                   ck.ord::int4 AS ordinal_position,
                   CASE con.confupdtype
                     WHEN 'a' THEN 'NO ACTION'
                     WHEN 'r' THEN 'RESTRICT'
                     WHEN 'c' THEN 'CASCADE'
                     WHEN 'n' THEN 'SET NULL'
                     WHEN 'd' THEN 'SET DEFAULT'
                     ELSE 'NO ACTION'
                   END AS update_rule,
                   CASE con.confdeltype
                     WHEN 'a' THEN 'NO ACTION'
                     WHEN 'r' THEN 'RESTRICT'
                     WHEN 'c' THEN 'CASCADE'
                     WHEN 'n' THEN 'SET NULL'
                     WHEN 'd' THEN 'SET DEFAULT'
                     ELSE 'NO ACTION'
                   END AS delete_rule
            FROM pg_constraint con
            JOIN pg_class cl ON cl.oid = con.conrelid
            JOIN pg_namespace n ON n.oid = cl.relnamespace
            JOIN pg_class fcl ON fcl.oid = con.confrelid
            JOIN pg_namespace fn ON fn.oid = fcl.relnamespace
            JOIN LATERAL unnest(con.conkey) WITH ORDINALITY AS ck(attnum, ord) ON true
            JOIN pg_attribute a ON a.attrelid = con.conrelid AND a.attnum = ck.attnum
            JOIN LATERAL unnest(con.confkey) WITH ORDINALITY AS fk(attnum, ord) ON fk.ord = ck.ord
            JOIN pg_attribute fa ON fa.attrelid = con.confrelid AND fa.attnum = fk.attnum
            WHERE con.contype = 'f'
              AND n.nspname NOT IN ('pg_catalog', 'information_schema')
              AND ($1 = '%' OR n.nspname = $1)
              AND ($2 = '' OR cl.relname = $2)
            ORDER BY n.nspname, cl.relname, con.conname, ck.ord
        "#;
        let rows = sqlx::query(sql)
            .bind(schema)
            .bind(table.unwrap_or(""))
            .fetch_all(client.pool.pg()?)
            .await?;

        let mut result = Vec::new();
        for row in rows {
            let schema_name: String = row.try_get("table_schema")?;
            let table_name: String = row.try_get("table_name")?;
            result.push((
                pg_display_name(&schema_name, &table_name),
                ForeignKeyInfo {
                    constraint_name: row.try_get("constraint_name")?,
                    column_name: row.try_get("column_name")?,
                    referenced_table_name: row.try_get("referenced_table_name")?,
                    referenced_column_name: row.try_get("referenced_column_name")?,
                    update_rule: row.try_get("update_rule")?,
                    delete_rule: row.try_get("delete_rule")?,
                    ordinal_position: row
                        .try_get::<i32, _>("ordinal_position")
                        .unwrap_or(0)
                        .max(0) as u32,
                },
            ));
        }
        Ok(result)
    }
}
