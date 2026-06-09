// SQL 安全工具函数集中模块
// 统一 sync.rs / mysql_sync.rs / transfer.rs / loadgen.rs 中的重复实现

use serde_json::Value;
use sqlx::Column;

/// 验证标识符是否只含安全字符（字母、数字、下划线、$）
/// 长度限制 1..=64
pub fn validate_identifier(s: &str) -> Result<(), String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err("Identifier is empty".into());
    }
    if trimmed.len() > 64 {
        return Err(format!("Identifier too long ({} > 64)", trimmed.len()));
    }
    if !trimmed
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '$')
    {
        return Err(format!("Identifier contains illegal chars: {}", trimmed));
    }
    Ok(())
}

/// MySQL 标识符引用：反引号包裹，内部反引号转义为 ``
pub fn quote_ident_mysql(s: &str) -> String {
    format!("`{}`", s.replace('`', "``"))
}

/// MySQL 标识符引用（带长度/空值校验），用于对外暴露的 API 端点
pub fn quote_ident_mysql_checked(s: &str) -> Result<String, String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err("Invalid identifier".into());
    }
    if trimmed.len() > 512 {
        return Err("Identifier too long".into());
    }
    Ok(quote_ident_mysql(trimmed))
}

/// PostgreSQL / SQLite 标识符引用：双引号包裹，内部双引号转义为 ""
pub fn quote_ident_pg(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// 根据 DbType 选择标识符引用方式
pub fn quote_ident(s: &str, db_type: crate::config::DbType) -> String {
    match db_type {
        crate::config::DbType::MySQL | crate::config::DbType::MariaDB => quote_ident_mysql(s),
        _ => quote_ident_pg(s),
    }
}

/// SQL 字符串值转义（参数化查询的后备方案）
/// 单引号 ' 转义为 ''
pub fn escape_sql_string(s: &str) -> String {
    s.replace('\'', "''")
}

/// 统一的 JSON Value -> SQL 字面量格式化
/// 用于无法使用参数化绑定的场景（如 DDL 生成、数据同步预览 SQL）
pub fn format_sql_value(v: &Value) -> String {
    match v {
        Value::Null => "NULL".to_string(),
        Value::Bool(b) => {
            if *b {
                "TRUE".to_string()
            } else {
                "FALSE".to_string()
            }
        }
        Value::Number(n) => n.to_string(),
        Value::String(s) => format!("'{}'", escape_sql_string(s)),
        Value::Array(_) | Value::Object(_) => {
            format!("'{}'", escape_sql_string(&v.to_string()))
        }
    }
}

/// 从 MySqlRow 提取整行为 serde_json::Value（逐类型 try-get）
/// 统一 mysql_sync.rs 和 transfer.rs 中重复的 row_to_json / row_cell_to_value 逻辑
pub fn mysql_row_to_json(row: &sqlx::mysql::MySqlRow) -> Value {
    use sqlx::Row;
    let mut map = serde_json::Map::new();
    for col in row.columns() {
        let col_name = col.name().to_string();
        let val = mysql_cell_to_value(row, col.ordinal());
        map.insert(col_name, val);
    }
    Value::Object(map)
}

/// 从 MySqlRow 的指定列提取为 serde_json::Value
pub fn mysql_cell_to_value(row: &sqlx::mysql::MySqlRow, idx: usize) -> Value {
    use sqlx::Row;
    if let Ok(v) = row.try_get::<Option<i64>, _>(idx) {
        return serde_json::json!(v);
    }
    if let Ok(v) = row.try_get::<Option<f64>, _>(idx) {
        return serde_json::json!(v);
    }
    if let Ok(v) = row.try_get::<Option<bool>, _>(idx) {
        return serde_json::json!(v);
    }
    if let Ok(v) = row.try_get::<Option<chrono::NaiveDateTime>, _>(idx) {
        return serde_json::json!(v.map(|dt| dt.to_string()));
    }
    if let Ok(v) = row.try_get::<Option<chrono::NaiveDate>, _>(idx) {
        return serde_json::json!(v.map(|d| d.to_string()));
    }
    if let Ok(v) = row.try_get::<Option<chrono::NaiveTime>, _>(idx) {
        return serde_json::json!(v.map(|t| t.to_string()));
    }
    if let Ok(v) = row.try_get::<Option<String>, _>(idx) {
        return serde_json::json!(v);
    }
    if let Ok(v) = row.try_get::<Option<Vec<u8>>, _>(idx) {
        if let Some(bytes) = v {
            return serde_json::json!(String::from_utf8_lossy(&bytes).to_string());
        }
    }
    Value::Null
}

/// 从 MySqlRow 提取单列为 Option<String>（逐类型 try-get）
pub fn mysql_cell_to_string(row: &sqlx::mysql::MySqlRow, ordinal: usize) -> Option<String> {
    use sqlx::Row;
    if let Ok(v) = row.try_get::<Option<String>, _>(ordinal) {
        return v;
    }
    if let Ok(v) = row.try_get::<Option<i64>, _>(ordinal) {
        return v.map(|x| x.to_string());
    }
    if let Ok(v) = row.try_get::<Option<f64>, _>(ordinal) {
        return v.map(|x| x.to_string());
    }
    if let Ok(v) = row.try_get::<Option<bool>, _>(ordinal) {
        return v.map(|x| x.to_string());
    }
    if let Ok(v) = row.try_get::<Option<Vec<u8>>, _>(ordinal) {
        return v.map(|x| String::from_utf8_lossy(&x).into_owned());
    }
    None
}

/// 从 PgRow 提取整行为 serde_json::Value（逐类型 try-get）
pub fn pg_row_to_json(row: &sqlx::postgres::PgRow) -> Value {
    use sqlx::Row;
    let mut map = serde_json::Map::new();
    for col in row.columns() {
        let col_name = col.name().to_string();
        let val = pg_cell_to_value(row, col.ordinal());
        map.insert(col_name, val);
    }
    Value::Object(map)
}

/// 从 PgRow 的指定列提取为 serde_json::Value
pub fn pg_cell_to_value(row: &sqlx::postgres::PgRow, idx: usize) -> Value {
    use sqlx::Row;
    if let Ok(v) = row.try_get::<Option<bool>, _>(idx) {
        return serde_json::json!(v);
    }
    if let Ok(v) = row.try_get::<Option<i16>, _>(idx) {
        return serde_json::json!(v);
    }
    if let Ok(v) = row.try_get::<Option<i32>, _>(idx) {
        return serde_json::json!(v);
    }
    if let Ok(v) = row.try_get::<Option<i64>, _>(idx) {
        return serde_json::json!(v);
    }
    if let Ok(v) = row.try_get::<Option<f32>, _>(idx) {
        return serde_json::json!(v);
    }
    if let Ok(v) = row.try_get::<Option<f64>, _>(idx) {
        return serde_json::json!(v);
    }
    if let Ok(v) = row.try_get::<Option<chrono::NaiveDateTime>, _>(idx) {
        return serde_json::json!(v.map(|dt| dt.to_string()));
    }
    if let Ok(v) = row.try_get::<Option<chrono::NaiveDate>, _>(idx) {
        return serde_json::json!(v.map(|d| d.to_string()));
    }
    if let Ok(v) = row.try_get::<Option<chrono::NaiveTime>, _>(idx) {
        return serde_json::json!(v.map(|t| t.to_string()));
    }
    if let Ok(v) = row.try_get::<Option<String>, _>(idx) {
        return serde_json::json!(v);
    }
    if let Ok(v) = row.try_get::<Option<Vec<u8>>, _>(idx) {
        if let Some(bytes) = v {
            return serde_json::json!(String::from_utf8_lossy(&bytes).to_string());
        }
    }
    Value::Null
}

/// 从 PgRow 提取单列为 Option<String>（逐类型 try-get）
pub fn pg_cell_to_string(row: &sqlx::postgres::PgRow, ordinal: usize) -> Option<String> {
    use sqlx::Row;
    if let Ok(v) = row.try_get::<Option<String>, _>(ordinal) {
        return v;
    }
    if let Ok(v) = row.try_get::<Option<i16>, _>(ordinal) {
        return v.map(|x| x.to_string());
    }
    if let Ok(v) = row.try_get::<Option<i32>, _>(ordinal) {
        return v.map(|x| x.to_string());
    }
    if let Ok(v) = row.try_get::<Option<i64>, _>(ordinal) {
        return v.map(|x| x.to_string());
    }
    if let Ok(v) = row.try_get::<Option<f32>, _>(ordinal) {
        return v.map(|x| x.to_string());
    }
    if let Ok(v) = row.try_get::<Option<f64>, _>(ordinal) {
        return v.map(|x| x.to_string());
    }
    if let Ok(v) = row.try_get::<Option<bool>, _>(ordinal) {
        return v.map(|x| x.to_string());
    }
    if let Ok(v) = row.try_get::<Option<Vec<u8>>, _>(ordinal) {
        return v.map(|x| String::from_utf8_lossy(&x).into_owned());
    }
    None
}

/// 从 SqliteRow 提取整行为 serde_json::Value（逐类型 try-get）
pub fn sqlite_row_to_json(row: &sqlx::sqlite::SqliteRow) -> Value {
    use sqlx::Row;
    let mut map = serde_json::Map::new();
    for col in row.columns() {
        let col_name = col.name().to_string();
        let val = sqlite_cell_to_value(row, col.ordinal());
        map.insert(col_name, val);
    }
    Value::Object(map)
}

/// 从 SqliteRow 的指定列提取为 serde_json::Value
pub fn sqlite_cell_to_value(row: &sqlx::sqlite::SqliteRow, idx: usize) -> Value {
    use sqlx::Row;
    if let Ok(v) = row.try_get::<Option<i64>, _>(idx) {
        return serde_json::json!(v);
    }
    if let Ok(v) = row.try_get::<Option<f64>, _>(idx) {
        return serde_json::json!(v);
    }
    if let Ok(v) = row.try_get::<Option<bool>, _>(idx) {
        return serde_json::json!(v);
    }
    if let Ok(v) = row.try_get::<Option<String>, _>(idx) {
        return serde_json::json!(v);
    }
    if let Ok(v) = row.try_get::<Option<Vec<u8>>, _>(idx) {
        if let Some(bytes) = v {
            return serde_json::json!(String::from_utf8_lossy(&bytes).to_string());
        }
    }
    Value::Null
}

/// 从 SqliteRow 提取单列为 Option<String>（逐类型 try-get）
pub fn sqlite_cell_to_string(row: &sqlx::sqlite::SqliteRow, ordinal: usize) -> Option<String> {
    use sqlx::Row;
    if let Ok(v) = row.try_get::<Option<String>, _>(ordinal) {
        return v;
    }
    if let Ok(v) = row.try_get::<Option<i64>, _>(ordinal) {
        return v.map(|x| x.to_string());
    }
    if let Ok(v) = row.try_get::<Option<f64>, _>(ordinal) {
        return v.map(|x| x.to_string());
    }
    if let Ok(v) = row.try_get::<Option<bool>, _>(ordinal) {
        return v.map(|x| x.to_string());
    }
    if let Ok(v) = row.try_get::<Option<Vec<u8>>, _>(ordinal) {
        return v.map(|x| String::from_utf8_lossy(&x).into_owned());
    }
    None
}

/// Check if a SQL statement is a mutation (INSERT/UPDATE/DELETE/DROP/ALTER/TRUNCATE/CREATE/REPLACE).
/// Used for read-only mode enforcement.
///
/// Strips both single-line (`--`) and multi-line (`/* ... */)`) comments,
/// as well as WITH...AS CTEs, before checking the first keyword.
/// This prevents bypass via inline comments like `/**/INSERT` or `SELECT /*x*/INSERT`.
pub fn is_mutation_sql(sql: &str) -> bool {
    let upper = sql.trim().to_uppercase();

    // Strip multi-line comments /* ... */ (may span multiple lines or be inline)
    let mut stripped = String::with_capacity(upper.len());
    let mut in_ml_comment = false;
    let mut chars = upper.chars().peekable();
    while let Some(ch) = chars.next() {
        if in_ml_comment {
            if ch == '*' && chars.peek() == Some(&'/') {
                chars.next(); // consume '/'
                in_ml_comment = false;
                // Replace comment with a space so keywords don't merge
                stripped.push(' ');
            }
            // Skip everything inside comment
            continue;
        }
        if ch == '/' && chars.peek() == Some(&'*') {
            chars.next(); // consume '*'
            in_ml_comment = true;
            continue;
        }
        if ch == '-' && chars.peek() == Some(&'-') {
            // Single-line comment: skip until end of line
            chars.by_ref().take_while(|c| *c != '\n').for_each(drop);
            stripped.push(' ');
            continue;
        }
        stripped.push(ch);
    }

    // Now strip leading WITH ... AS CTEs
    // Walk through words: if we see "WITH" followed by an identifier then "AS",
    // skip the CTE preamble until we reach the actual statement keyword.
    let words: Vec<&str> = stripped.split_whitespace().collect();
    let mut idx = 0;

    // Skip optional WITH ... AS (...),  ... AS (...) preamble
    if !words.is_empty() && words[idx] == "WITH" {
        idx += 1;
        // CTEs repeat: name AS (subselect), name2 AS (subselect), ...
        // We just advance past all CTE names until we find a non-CTE keyword.
        // A CTE starts with an identifier followed by AS.
        while idx < words.len() {
            let w = words[idx];
            // If this word is a real statement keyword, we've exited the CTE preamble
            if matches!(
                w,
                "SELECT"
                    | "INSERT"
                    | "UPDATE"
                    | "DELETE"
                    | "DROP"
                    | "ALTER"
                    | "TRUNCATE"
                    | "CREATE"
                    | "REPLACE"
                    | "GRANT"
                    | "REVOKE"
                    | "SHOW"
                    | "DESCRIBE"
                    | "DESC"
                    | "EXPLAIN"
                    | "SET"
                    | "USE"
                    | "CALL"
            ) {
                break;
            }
            // Otherwise this is part of the CTE name / AS keyword — skip it
            idx += 1;
        }
    }

    let first_word = words.get(idx).copied().unwrap_or("");
    matches!(
        first_word,
        "INSERT"
            | "UPDATE"
            | "DELETE"
            | "DROP"
            | "ALTER"
            | "TRUNCATE"
            | "CREATE"
            | "REPLACE"
            | "GRANT"
            | "REVOKE"
    )
}

/// AST-level check: whether a parsed statement is read-only (SELECT, SHOW, DESCRIBE, EXPLAIN, etc.)
/// Shared by executor, execute_sql, and explain_sql handlers.
pub fn is_read_only_statement(stmt: &sqlparser::ast::Statement) -> bool {
    use sqlparser::ast::Statement;
    matches!(
        stmt,
        Statement::Query(_)
            | Statement::Explain { .. }
            | Statement::ExplainTable { .. }
            | Statement::ShowVariable { .. }
            | Statement::ShowCreate { .. }
            | Statement::ShowTables { .. }
            | Statement::ShowDatabases { .. }
            | Statement::ShowColumns { .. }
            | Statement::ShowStatus { .. }
            | Statement::ShowVariables { .. }
            | Statement::ShowCollation { .. }
            | Statement::ShowCharset(_)
            | Statement::ShowSchemas { .. }
            | Statement::ShowViews { .. }
            | Statement::ShowFunctions { .. }
            | Statement::ShowObjects(_)
    )
}

/// AST-level check: whether a parsed statement is dangerous (DDL, DCL, or DML mutations).
/// Used to gate execution behind confirmation prompts.
pub fn is_dangerous_statement(stmt: &sqlparser::ast::Statement) -> bool {
    use sqlparser::ast::Statement;
    matches!(
        stmt,
        Statement::Insert { .. }
            | Statement::Update { .. }
            | Statement::Delete { .. }
            | Statement::Drop { .. }
            | Statement::Truncate { .. }
            | Statement::AlterTable { .. }
            | Statement::CreateTable(..)
            | Statement::CreateView(..)
            | Statement::CreateIndex(..)
            | Statement::CreateSchema { .. }
            | Statement::CreateFunction(..)
            | Statement::CreateTrigger(..)
            | Statement::Grant(..)
            | Statement::Revoke(..)
            | Statement::RenameTable(..)
            | Statement::Call(..)
            | Statement::Set(..)
    )
}

/// Generate a cross-engine UPSERT SQL statement.
/// MySQL: INSERT INTO ... ON DUPLICATE KEY UPDATE ...
/// PostgreSQL: INSERT INTO ... ON CONFLICT (pk) DO UPDATE SET ...
pub fn generate_upsert_sql(
    table: &str,
    columns: &[String],
    primary_key: &str,
    db_type: &crate::config::DbType,
    rows: &[serde_json::Map<String, Value>],
) -> String {
    if rows.is_empty() {
        return String::new();
    }

    let qi = |s: &str| quote_ident(s, db_type.clone());
    let col_list = columns.iter().map(|c| qi(c)).collect::<Vec<_>>().join(", ");

    let mut values_clauses = Vec::new();
    for row in rows {
        let vals: Vec<String> = columns
            .iter()
            .map(|c| {
                row.get(c)
                    .map(format_sql_value)
                    .unwrap_or_else(|| "NULL".to_string())
            })
            .collect();
        values_clauses.push(format!("({})", vals.join(", ")));
    }

    let non_pk_cols: Vec<&String> = columns
        .iter()
        .filter(|c| !c.eq_ignore_ascii_case(primary_key))
        .collect();

    match db_type {
        crate::config::DbType::MySQL | crate::config::DbType::MariaDB => {
            let updates: Vec<String> = non_pk_cols
                .iter()
                .map(|c| format!("{col} = VALUES({col})", col = qi(c)))
                .collect();
            format!(
                "INSERT INTO {table} ({cols}) VALUES {vals} ON DUPLICATE KEY UPDATE {updates}",
                table = qi(table),
                cols = col_list,
                vals = values_clauses.join(", "),
                updates = updates.join(", ")
            )
        }
        crate::config::DbType::PostgreSQL => {
            let updates: Vec<String> = non_pk_cols
                .iter()
                .map(|c| format!("{col} = EXCLUDED.{col}", col = qi(c)))
                .collect();
            format!(
                "INSERT INTO {table} ({cols}) VALUES {vals} ON CONFLICT ({pk}) DO UPDATE SET {updates}",
                table = qi(table),
                cols = col_list,
                vals = values_clauses.join(", "),
                pk = qi(primary_key),
                updates = updates.join(", ")
            )
        }
        _ => {
            // SQLite: INSERT OR REPLACE
            format!(
                "INSERT OR REPLACE INTO {table} ({cols}) VALUES {vals}",
                table = qi(table),
                cols = col_list,
                vals = values_clauses.join(", "),
            )
        }
    }
}

/// Map a MySQL data type to its PostgreSQL equivalent.
/// Reference: pgloader type mapping rules.
pub fn mysql_to_pg_type(mysql_type: &str, column_type: &str, extra: &str) -> String {
    let upper = mysql_type.to_uppercase();
    match upper.as_str() {
        "TINYINT" if column_type.contains("(1)") => "BOOLEAN".into(),
        "INT" | "INTEGER" if extra.contains("auto_increment") => {
            if column_type.to_uppercase().contains("UNSIGNED") {
                "BIGSERIAL"
            } else {
                "SERIAL"
            }
            .into()
        }
        "BIGINT" if extra.contains("auto_increment") => "BIGSERIAL".into(),
        "TINYINT" | "SMALLINT" | "MEDIUMINT" if column_type.to_uppercase().contains("UNSIGNED") => {
            match upper.as_str() {
                "TINYINT" => "SMALLINT",
                "SMALLINT" => "INTEGER",
                _ => "INTEGER",
            }
            .into()
        }
        "DOUBLE" => "DOUBLE PRECISION".into(),
        "DATETIME" | "TIMESTAMP" => "TIMESTAMPTZ".into(),
        "YEAR" => "INTEGER".into(),
        "BLOB" | "TINYBLOB" | "MEDIUMBLOB" | "LONGBLOB" => "BYTEA".into(),
        "TEXT" | "TINYTEXT" | "MEDIUMTEXT" | "LONGTEXT" => "TEXT".into(),
        "JSON" => "JSONB".into(),
        _ => mysql_type.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_identifier_accepts_alphanumeric() {
        assert!(validate_identifier("users").is_ok());
        assert!(validate_identifier("my_table_123").is_ok());
        assert!(validate_identifier("_private").is_ok());
    }

    #[test]
    fn validate_identifier_rejects_empty() {
        assert!(validate_identifier("").is_err());
        assert!(validate_identifier("  ").is_err());
    }

    #[test]
    fn validate_identifier_rejects_special_chars() {
        assert!(validate_identifier("table;DROP").is_err());
        assert!(validate_identifier("table name").is_err());
        assert!(validate_identifier("table`name").is_err());
    }

    #[test]
    fn validate_identifier_rejects_too_long() {
        let long_name = "a".repeat(65);
        assert!(validate_identifier(&long_name).is_err());
    }

    #[test]
    fn quote_ident_mysql_escapes_backticks() {
        assert_eq!(quote_ident_mysql("users"), "`users`");
        assert_eq!(quote_ident_mysql("my`table"), "`my``table`");
    }

    #[test]
    fn quote_ident_pg_escapes_double_quotes() {
        assert_eq!(quote_ident_pg("users"), "\"users\"");
        assert_eq!(quote_ident_pg("my\"table"), "\"my\"\"table\"");
    }

    #[test]
    fn escape_sql_string_escapes_single_quotes() {
        assert_eq!(escape_sql_string("hello"), "hello");
        assert_eq!(escape_sql_string("it's"), "it''s");
        assert_eq!(escape_sql_string("a''b"), "a''''b");
    }

    #[test]
    fn format_sql_value_handles_types() {
        assert_eq!(format_sql_value(&Value::Null), "NULL");
        assert_eq!(format_sql_value(&Value::Bool(true)), "TRUE");
        assert_eq!(format_sql_value(&serde_json::json!(42)), "42");
        assert_eq!(format_sql_value(&serde_json::json!("hello")), "'hello'");
        assert_eq!(
            format_sql_value(&serde_json::json!("it's")),
            "'it''s'"
        );
        assert_eq!(
            format_sql_value(&serde_json::json!({"a": 1})),
            "'{\"a\":1}'"
        );
    }

    #[test]
    fn is_mutation_sql_detects_writes() {
        assert!(is_mutation_sql("INSERT INTO t VALUES (1)"));
        assert!(is_mutation_sql("UPDATE t SET a=1"));
        assert!(is_mutation_sql("DELETE FROM t"));
        assert!(is_mutation_sql("DROP TABLE t"));
        assert!(is_mutation_sql("ALTER TABLE t ADD c INT"));
        assert!(is_mutation_sql("TRUNCATE TABLE t"));
        assert!(is_mutation_sql("CREATE TABLE t (id INT)"));
        assert!(is_mutation_sql("REPLACE INTO t VALUES (1)"));
        assert!(is_mutation_sql("-- comment\nINSERT INTO t VALUES (1)"));
        assert!(is_mutation_sql("/* block comment */\nINSERT INTO t VALUES (1)"));
        assert!(is_mutation_sql("GRANT SELECT ON t TO user"));
        assert!(is_mutation_sql("REVOKE SELECT ON t FROM user"));
    }

    #[test]
    fn is_mutation_sql_allows_reads() {
        assert!(!is_mutation_sql("SELECT * FROM t"));
        assert!(!is_mutation_sql("SHOW TABLES"));
        assert!(!is_mutation_sql("DESCRIBE t"));
        assert!(!is_mutation_sql("EXPLAIN SELECT * FROM t"));
        assert!(!is_mutation_sql("-- read only\nSELECT 1"));
        assert!(!is_mutation_sql("WITH cte AS (SELECT 1) SELECT * FROM cte"));
    }

    #[test]
    fn is_mutation_sql_blocks_comment_bypass() {
        // Inline comment bypass: /**/INSERT should be detected as INSERT
        assert!(is_mutation_sql("/**/INSERT INTO t VALUES (1)"));
        // Multi-line comment before mutation keyword
        assert!(is_mutation_sql("/*\ncomment\n*/\nDROP TABLE t"));
        // Tab between comment and keyword
        assert!(is_mutation_sql("-- comment\t\nDELETE FROM t"));
        // WITH CTE followed by mutation
        assert!(is_mutation_sql("WITH cte AS (SELECT 1) INSERT INTO t SELECT * FROM cte"));
        // Comment with embedded whitespace bypassing
        assert!(is_mutation_sql("/*x*/DROP TABLE t"));
        // Nested-looking inline comment
        assert!(is_mutation_sql("/**/GRANT SELECT ON t TO user"));
    }

    #[test]
    fn generate_upsert_sql_mysql() {
        let cols = vec!["id".into(), "name".into(), "score".into()];
        let mut row = serde_json::Map::new();
        row.insert("id".into(), serde_json::json!(1));
        row.insert("name".into(), serde_json::json!("alice"));
        row.insert("score".into(), serde_json::json!(95));
        let sql = generate_upsert_sql("users", &cols, "id", &crate::config::DbType::MySQL, &[row]);
        assert!(sql.contains("ON DUPLICATE KEY UPDATE"));
        assert!(sql.contains("VALUES(`name`)"));
        assert!(!sql.contains("id = VALUES(`id`)")); // PK excluded from updates
    }

    #[test]
    fn generate_upsert_sql_pg() {
        let cols = vec!["id".into(), "name".into()];
        let mut row = serde_json::Map::new();
        row.insert("id".into(), serde_json::json!(1));
        row.insert("name".into(), serde_json::json!("bob"));
        let sql = generate_upsert_sql("users", &cols, "id", &crate::config::DbType::PostgreSQL, &[row]);
        assert!(sql.contains("ON CONFLICT"));
        assert!(sql.contains("EXCLUDED"));
    }

    #[test]
    fn mysql_to_pg_type_maps_correctly() {
        assert_eq!(mysql_to_pg_type("DOUBLE", "", ""), "DOUBLE PRECISION");
        assert_eq!(mysql_to_pg_type("DATETIME", "", ""), "TIMESTAMPTZ");
        assert_eq!(mysql_to_pg_type("BLOB", "", ""), "BYTEA");
        assert_eq!(mysql_to_pg_type("JSON", "", ""), "JSONB");
        assert_eq!(mysql_to_pg_type("VARCHAR", "", ""), "VARCHAR"); // unchanged
        assert_eq!(
            mysql_to_pg_type("INT", "INT UNSIGNED", "auto_increment"),
            "BIGSERIAL"
        );
        assert_eq!(
            mysql_to_pg_type("TINYINT", "TINYINT(1)", ""),
            "BOOLEAN"
        );
    }
}
