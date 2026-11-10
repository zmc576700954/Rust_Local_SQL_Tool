//! SQLite 结构内省。
//!
//! 数据源优先级：`PRAGMA table_xinfo|index_list|index_xinfo|foreign_key_list`
//! 提供权威的列/索引/外键信息；`sqlite_master.sql` 只用于补齐 PRAGMA
//! 无法表达的信息（`WITHOUT ROWID`、生成列表达式、索引/触发器原始 DDL）。
//!
//! 表达式索引与部分索引在 `IndexInfo` 里无法完整表达：这里保留
//! `is_expression/is_partial` 与原始 SQL，交由结构同步显式报告 unsupported，
//! 不静默当成普通索引。

use crate::db::{DbClient, DbError};
use crate::schema_ext::{ForeignKeyInfo, IndexInfo, TriggerInfo, ViewInfo};
use crate::schema::extractor::ColumnInfo;
use sqlx::Row;
use std::collections::HashMap;

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// SQLite 类型亲和性规则（https://sqlite.org/datatype3.html §3.1）。
pub fn type_affinity(declared: &str) -> &'static str {
    let upper = declared.to_ascii_uppercase();
    if upper.contains("INT") {
        "INTEGER"
    } else if upper.contains("CHAR") || upper.contains("CLOB") || upper.contains("TEXT") {
        "TEXT"
    } else if upper.contains("BLOB") || upper.trim().is_empty() {
        "BLOB"
    } else if upper.contains("REAL") || upper.contains("FLOA") || upper.contains("DOUB") {
        "REAL"
    } else {
        "NUMERIC"
    }
}

pub async fn list_tables(client: &DbClient) -> Result<Vec<(String, String, bool)>, DbError> {
    let rows = sqlx::query(
        "SELECT name, COALESCE(sql, '') AS sql FROM sqlite_master \
         WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
    )
    .fetch_all(client.pool.sqlite()?)
    .await?;
    let mut result = Vec::new();
    for row in rows {
        let name: String = row.try_get("name")?;
        let sql: String = row.try_get("sql")?;
        let without_rowid = sql_has_keyword(&sql, "WITHOUT ROWID");
        result.push((name, sql, without_rowid));
    }
    Ok(result)
}

pub async fn list_columns(client: &DbClient, table: &str) -> Result<Vec<ColumnInfo>, DbError> {
    let table_sql = table_create_sql(client, table).await?;
    let generated = table_sql
        .as_deref()
        .map(generated_expressions)
        .unwrap_or_default();
    let without_rowid = table_sql
        .as_deref()
        .map(|s| sql_has_keyword(s, "WITHOUT ROWID"))
        .unwrap_or(false);

    let pragma = format!("PRAGMA table_xinfo({})", quote_ident(table));
    let rows = sqlx::query(&pragma).fetch_all(client.pool.sqlite()?).await?;
    let pk_count = rows
        .iter()
        .filter(|r| r.try_get::<i64, _>("pk").unwrap_or(0) > 0)
        .count();
    let mut result = Vec::with_capacity(rows.len());
    for row in rows {
        let name: String = row.try_get("name")?;
        let declared: String = row
            .try_get::<Option<String>, _>("type")
            .unwrap_or(None)
            .unwrap_or_default();
        let notnull: i64 = row.try_get("notnull").unwrap_or(0);
        let default: Option<String> = row.try_get("dflt_value").unwrap_or(None);
        let pk: i64 = row.try_get("pk").unwrap_or(0);
        let hidden: i64 = row.try_get("hidden").unwrap_or(0);
        // hidden: 0 普通列，1 虚拟表隐藏列，2 生成列(VIRTUAL)，3 生成列(STORED)。
        if hidden == 1 {
            continue;
        }
        let affinity = type_affinity(&declared);
        let is_generated = hidden >= 2;
        let generated_expr = if is_generated {
            generated.get(&name).cloned()
        } else {
            None
        };
        let (extra, type_warning) = match hidden {
            2 => ("VIRTUAL GENERATED".to_string(), None),
            3 => ("STORED GENERATED".to_string(), None),
            _ => (String::new(), None),
        };
        let type_warning = if hidden >= 2 && generated_expr.is_none() {
            Some("生成列表达式无法从 sqlite_master.sql 还原".to_string())
        } else {
            type_warning
        };
        let is_rowid_alias = pk == 1
            && pk_count == 1
            && !without_rowid
            && declared.trim().eq_ignore_ascii_case("INTEGER");
        let extra = if is_rowid_alias {
            let mut e = "auto_increment".to_string();
            if table_sql
                .as_deref()
                .map(|s| s.to_ascii_uppercase().contains("AUTOINCREMENT"))
                .unwrap_or(false)
            {
                e.push_str(" AUTOINCREMENT");
            }
            e
        } else {
            extra
        };
        result.push(ColumnInfo {
            column_name: name,
            data_type: affinity.to_string(),
            column_type: if declared.trim().is_empty() {
                affinity.to_string()
            } else {
                declared.trim().to_string()
            },
            is_nullable: if notnull != 0 { "NO" } else { "YES" }.to_string(),
            column_comment: None,
            column_key: if pk > 0 { "PRI" } else { "" }.to_string(),
            column_default: if generated_expr.is_some() { None } else { default },
            extra,
            type_warning,
            generation_expression: generated_expr,
            primary_key_ordinal: if pk > 0 { Some(pk as u32) } else { None },
        });
    }
    Ok(result)
}

pub async fn list_columns_map(
    client: &DbClient,
) -> Result<HashMap<String, Vec<ColumnInfo>>, DbError> {
    let mut result = HashMap::new();
    for (table, _, _) in list_tables(client).await? {
        let cols = list_columns(client, &table).await?;
        result.insert(table, cols);
    }
    Ok(result)
}

pub async fn list_indexes(client: &DbClient, table: &str) -> Result<Vec<IndexInfo>, DbError> {
    let definitions = index_definitions(client, table).await?;
    let pragma = format!("PRAGMA index_list({})", quote_ident(table));
    let indexes = sqlx::query(&pragma).fetch_all(client.pool.sqlite()?).await?;
    let mut result = Vec::new();
    for idx in indexes {
        let index_name: String = idx.try_get("name")?;
        let unique: i64 = idx.try_get("unique").unwrap_or(0);
        let origin: String = idx.try_get("origin").unwrap_or_default();
        let partial: i64 = idx.try_get("partial").unwrap_or(0);
        let definition = definitions.get(&index_name).cloned();
        let xinfo = format!("PRAGMA index_xinfo({})", quote_ident(&index_name));
        let cols = sqlx::query(&xinfo)
            .fetch_all(client.pool.sqlite()?)
            .await?;
        for col in cols {
            let key: i64 = col.try_get("key").unwrap_or(0);
            if key == 0 {
                continue;
            }
            let seqno: i64 = col.try_get("seqno").unwrap_or(0);
            let cid: i64 = col.try_get("cid").unwrap_or(0);
            let name: Option<String> = col.try_get("name").unwrap_or(None);
            let desc: i64 = col.try_get("desc").unwrap_or(0);
            let is_expression = cid < 0 || name.as_deref().unwrap_or("").is_empty();
            result.push(IndexInfo {
                index_name: index_name.clone(),
                column_name: name.unwrap_or_default(),
                non_unique: unique == 0,
                index_type: "BTREE".to_string(),
                seq_in_index: (seqno + 1) as u32,
                sort_order: if desc != 0 { "DESC" } else { "ASC" }.to_string(),
                is_primary: origin == "pk",
                is_expression,
                is_partial: partial != 0,
                predicate: None,
                definition: definition.clone(),
            });
        }
    }
    Ok(result)
}

pub async fn list_indexes_map(
    client: &DbClient,
) -> Result<HashMap<String, Vec<IndexInfo>>, DbError> {
    let mut result = HashMap::new();
    for (table, _, _) in list_tables(client).await? {
        let idx = list_indexes(client, &table).await?;
        result.insert(table, idx);
    }
    Ok(result)
}

pub async fn list_foreign_keys(
    client: &DbClient,
    table: &str,
) -> Result<Vec<ForeignKeyInfo>, DbError> {
    let pragma = format!("PRAGMA foreign_key_list({})", quote_ident(table));
    let rows = sqlx::query(&pragma).fetch_all(client.pool.sqlite()?).await?;
    let mut result = Vec::new();
    for row in rows {
        let id: i64 = row.try_get("id").unwrap_or(0);
        let seq: i64 = row.try_get("seq").unwrap_or(0);
        let ref_table: String = row.try_get("table").unwrap_or_default();
        let from: String = row.try_get("from").unwrap_or_default();
        let to: Option<String> = row.try_get("to").unwrap_or(None);
        let update: String = row.try_get("on_update").unwrap_or_default();
        let delete: String = row.try_get("on_delete").unwrap_or_default();
        result.push(ForeignKeyInfo {
            // SQLite 不保存命名外键约束；用稳定可读的合成名。
            constraint_name: format!("fk_{}_{}", table, id),
            column_name: from,
            referenced_table_name: ref_table,
            referenced_column_name: to.unwrap_or_default(),
            update_rule: update,
            delete_rule: delete,
            ordinal_position: (seq + 1) as u32,
        });
    }
    Ok(result)
}

pub async fn list_foreign_keys_map(
    client: &DbClient,
) -> Result<HashMap<String, Vec<ForeignKeyInfo>>, DbError> {
    let mut result = HashMap::new();
    for (table, _, _) in list_tables(client).await? {
        let fks = list_foreign_keys(client, &table).await?;
        result.insert(table, fks);
    }
    Ok(result)
}

pub async fn list_views(client: &DbClient) -> Result<Vec<ViewInfo>, DbError> {
    let rows = sqlx::query(
        "SELECT name, COALESCE(sql, '') AS sql FROM sqlite_master \
         WHERE type = 'view' ORDER BY name",
    )
    .fetch_all(client.pool.sqlite()?)
    .await?;
    let mut result = Vec::new();
    for row in rows {
        result.push(ViewInfo {
            table_name: row.try_get("name")?,
            view_definition: row.try_get("sql")?,
        });
    }
    Ok(result)
}

pub async fn list_triggers(
    client: &DbClient,
    table: &str,
) -> Result<Vec<TriggerInfo>, DbError> {
    let rows = sqlx::query(
        "SELECT name, tbl_name, COALESCE(sql, '') AS sql FROM sqlite_master \
         WHERE type = 'trigger' AND tbl_name = ? ORDER BY name",
    )
    .bind(table)
    .fetch_all(client.pool.sqlite()?)
    .await?;
    let mut result = Vec::new();
    for row in rows {
        result.push(TriggerInfo {
            trigger_name: row.try_get("name")?,
            table_name: row.try_get("tbl_name")?,
            sql: row.try_get("sql")?,
        });
    }
    Ok(result)
}

pub async fn list_triggers_map(
    client: &DbClient,
) -> Result<HashMap<String, Vec<TriggerInfo>>, DbError> {
    let mut result = HashMap::new();
    for (table, _, _) in list_tables(client).await? {
        let triggers = list_triggers(client, &table).await?;
        result.insert(table, triggers);
    }
    Ok(result)
}

pub async fn table_without_rowid(client: &DbClient, table: &str) -> Result<bool, DbError> {
    Ok(table_create_sql(client, table)
        .await?
        .as_deref()
        .map(|s| sql_has_keyword(s, "WITHOUT ROWID"))
        .unwrap_or(false))
}

/// 导出时使用：返回 `SELECT *` 对应的列名与顺序（排除虚拟表隐藏列）。
pub async fn export_columns(
    pool: &sqlx::SqlitePool,
    table: &str,
) -> Result<Vec<String>, DbError> {
    let pragma = format!("PRAGMA table_xinfo({})", quote_ident(table));
    let rows = sqlx::query(&pragma).fetch_all(pool).await?;
    let mut result = Vec::new();
    for row in rows {
        let hidden: i64 = row.try_get("hidden").unwrap_or(0);
        if hidden == 1 {
            continue;
        }
        result.push(row.try_get("name")?);
    }
    Ok(result)
}

async fn table_create_sql(client: &DbClient, table: &str) -> Result<Option<String>, DbError> {
    let row = sqlx::query("SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?")
        .bind(table)
        .fetch_optional(client.pool.sqlite()?)
        .await?;
    Ok(row.and_then(|r| r.try_get::<Option<String>, _>("sql").unwrap_or(None)))
}

async fn index_definitions(
    client: &DbClient,
    table: &str,
) -> Result<HashMap<String, String>, DbError> {
    let rows = sqlx::query(
        "SELECT name, COALESCE(sql, '') AS sql FROM sqlite_master \
         WHERE type = 'index' AND tbl_name = ?",
    )
    .bind(table)
    .fetch_all(client.pool.sqlite()?)
    .await?;
    let mut result = HashMap::new();
    for row in rows {
        let sql: String = row.try_get("sql")?;
        if !sql.trim().is_empty() {
            result.insert(row.try_get("name")?, sql);
        }
    }
    Ok(result)
}

/// 判断 SQL 文本是否包含关键字；先去掉引号内内容，避免列名误判。
fn sql_has_keyword(sql: &str, keyword: &str) -> bool {
    strip_quoted(sql)
        .to_ascii_uppercase()
        .split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_'))
        .collect::<Vec<_>>()
        .windows(keyword.split_whitespace().count())
        .any(|w| w.join(" ") == keyword.to_ascii_uppercase())
}

fn strip_quoted(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\'' || c == '"' || c == '`' {
            let quote = c;
            out.push(' ');
            while let Some(n) = chars.next() {
                if n == quote {
                    if chars.peek() == Some(&quote) {
                        chars.next();
                        continue;
                    }
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// 从 `CREATE TABLE` 文本里提取生成列表达式。PRAGMA 只给 hidden 标志，
/// 表达式只能从原始 SQL 补齐；解析失败时返回空表，由调用方标记 type_warning。
fn generated_expressions(create_sql: &str) -> HashMap<String, String> {
    let mut result = HashMap::new();
    let Some(open) = create_sql.find('(') else {
        return result;
    };
    let body = &create_sql[open + 1..];
    for def in split_top_level(body) {
        let def = def.trim();
        let upper = def.to_ascii_uppercase();
        if upper.starts_with("PRIMARY ")
            || upper.starts_with("UNIQUE")
            || upper.starts_with("CHECK")
            || upper.starts_with("FOREIGN")
            || upper.starts_with("CONSTRAINT")
        {
            continue;
        }
        let Some(name) = first_identifier(def) else {
            continue;
        };
        if let Some(expr) = generated_expression_of(def) {
            result.insert(name, expr);
        }
    }
    result
}

fn first_identifier(def: &str) -> Option<String> {
    let def = def.trim_start();
    if def.is_empty() {
        return None;
    }
    let first = def.chars().next()?;
    if first == '"' || first == '`' || first == '[' {
        let close = match first {
            '[' => ']',
            c => c,
        };
        let rest = &def[1..];
        let end = rest.find(close)?;
        return Some(rest[..end].to_string());
    }
    let end = def
        .find(|c: char| c.is_whitespace() || c == '(' || c == ',')
        .unwrap_or(def.len());
    Some(def[..end].to_string())
}

fn generated_expression_of(def: &str) -> Option<String> {
    let upper = def.to_ascii_uppercase();
    let idx = find_keyword(&upper, "AS")?;
    let after = def[idx + 2..].trim_start();
    if !after.starts_with('(') {
        return None;
    }
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    for (i, c) in after.char_indices() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '\'' | '"' | '`' => quote = Some(c),
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(after[1..i].trim().to_string());
                }
            }
            _ => {}
        }
    }
    None
}

fn find_keyword(upper: &str, keyword: &str) -> Option<usize> {
    let bytes = upper.as_bytes();
    let needle = keyword.as_bytes();
    let mut i = 0;
    while i + needle.len() <= bytes.len() {
        if &bytes[i..i + needle.len()] == needle {
            let before_ok = i == 0 || !is_ident_char(bytes[i - 1] as char);
            let after_ok = i + needle.len() == bytes.len()
                || !is_ident_char(bytes[i + needle.len()] as char);
            if before_ok && after_ok {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn split_top_level(body: &str) -> Vec<&str> {
    let mut result = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    let mut quote: Option<char> = None;
    for (i, c) in body.char_indices() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '\'' | '"' | '`' => quote = Some(c),
            '(' => depth += 1,
            ')' => {
                if depth == 0 {
                    result.push(&body[start..i]);
                    return result;
                }
                depth -= 1;
            }
            ',' if depth == 0 => {
                result.push(&body[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    result.push(&body[start..]);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn affinity_matches_sqlite_rules() {
        assert_eq!(type_affinity("INTEGER"), "INTEGER");
        assert_eq!(type_affinity("BIGINT"), "INTEGER");
        assert_eq!(type_affinity("VARCHAR(20)"), "TEXT");
        assert_eq!(type_affinity("CLOB"), "TEXT");
        assert_eq!(type_affinity(""), "BLOB");
        assert_eq!(type_affinity("BLOB"), "BLOB");
        assert_eq!(type_affinity("DOUBLE"), "REAL");
        assert_eq!(type_affinity("FLOAT"), "REAL");
        assert_eq!(type_affinity("DECIMAL(10,2)"), "NUMERIC");
        assert_eq!(type_affinity("BOOLEAN"), "NUMERIC");
    }

    #[test]
    fn extracts_generated_expression() {
        let sql = r#"CREATE TABLE t (
            id INTEGER PRIMARY KEY,
            a TEXT,
            b TEXT GENERATED ALWAYS AS (upper(a)) STORED,
            c INTEGER AS (id + 1) VIRTUAL,
            CONSTRAINT u UNIQUE (a)
        ) WITHOUT ROWID"#;
        let map = generated_expressions(sql);
        assert_eq!(map.get("b").map(String::as_str), Some("upper(a)"));
        assert_eq!(map.get("c").map(String::as_str), Some("id + 1"));
        assert!(!map.contains_key("id"));
        assert!(sql_has_keyword(sql, "WITHOUT ROWID"));
    }

    #[test]
    fn ignores_keywords_inside_quotes() {
        assert!(!sql_has_keyword(
            r#"CREATE TABLE t (a TEXT DEFAULT 'WITHOUT ROWID')"#,
            "WITHOUT ROWID"
        ));
        assert!(sql_has_keyword(
            r#"CREATE TABLE t (a TEXT) WITHOUT ROWID"#,
            "WITHOUT ROWID"
        ));
    }
}
