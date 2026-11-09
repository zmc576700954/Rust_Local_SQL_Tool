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

/// 带长度/空值校验的 PostgreSQL 标识符引用
pub fn quote_ident_pg_checked(s: &str) -> Result<String, String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err("Invalid identifier".into());
    }
    if trimmed.len() > 512 {
        return Err("Identifier too long".into());
    }
    Ok(quote_ident_pg(trimmed))
}

/// 根据 DbType 选择标识符引用方式
pub fn quote_ident(s: &str, db_type: crate::config::DbType) -> String {
    match db_type {
        crate::config::DbType::MySQL | crate::config::DbType::MariaDB => quote_ident_mysql(s),
        _ => quote_ident_pg(s),
    }
}

/// 拆分 PostgreSQL 的 schema 限定名（`schema.table`）。
///
/// 未带 schema 时返回 `(None, table)`；带 schema 时返回 `(Some(schema), table)`。
pub fn split_pg_table_ref(name: &str) -> (Option<&str>, &str) {
    match name.split_once('.') {
        Some((schema, table)) if !schema.is_empty() && !table.is_empty() => {
            (Some(schema), table)
        }
        _ => (None, name),
    }
}

/// 按引擎引用表名：MySQL 反引号；PostgreSQL 支持 `schema.table` 两级；SQLite 双引号。
pub fn quote_table_ref_checked(
    name: &str,
    db_type: &crate::config::DbType,
) -> Result<String, String> {
    use crate::config::DbType;
    match db_type {
        DbType::MySQL | DbType::MariaDB => quote_ident_mysql_checked(name),
        DbType::PostgreSQL => {
            let (schema, table) = split_pg_table_ref(name);
            let table_quoted = quote_ident_pg_checked(table)?;
            match schema {
                Some(schema) => Ok(format!(
                    "{}.{}",
                    quote_ident_pg_checked(schema)?,
                    table_quoted
                )),
                None => Ok(table_quoted),
            }
        }
        _ => quote_ident_pg_checked(name),
    }
}

/// 去掉 SQL 文本里的注释，保留字符串字面量内容。
///
/// 只处理 `--` 行注释、`#` 行注释与 `/* ... */` 块注释；字符串内的注释标记
/// 不会被移除（脚本拆分与关键字判断依赖这一点）。
pub fn strip_sql_comments(sql: &str) -> String {
    let mut stripped = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;
    while let Some(ch) = chars.next() {
        if in_single {
            stripped.push(ch);
            if ch == '\'' {
                if chars.peek() == Some(&'\'') {
                    stripped.push(chars.next().unwrap());
                } else {
                    in_single = false;
                }
            }
            continue;
        }
        if in_double {
            stripped.push(ch);
            if ch == '"' {
                if chars.peek() == Some(&'"') {
                    stripped.push(chars.next().unwrap());
                } else {
                    in_double = false;
                }
            }
            continue;
        }
        match ch {
            '\'' => {
                in_single = true;
                stripped.push(ch);
            }
            '"' => {
                in_double = true;
                stripped.push(ch);
            }
            '-' if chars.peek() == Some(&'-') => {
                chars.next();
                chars.by_ref().take_while(|c| *c != '\n').for_each(drop);
                stripped.push(' ');
            }
            '#' => {
                chars.by_ref().take_while(|c| *c != '\n').for_each(drop);
                stripped.push(' ');
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut prev = '\0';
                for c in chars.by_ref() {
                    if prev == '*' && c == '/' {
                        break;
                    }
                    prev = c;
                }
                stripped.push(' ');
            }
            _ => stripped.push(ch),
        }
    }
    stripped
}

/// 语句是否以事务控制关键字开头（BEGIN / START TRANSACTION / COMMIT / ROLLBACK / SAVEPOINT / RELEASE）。
///
/// 脚本导入在 PG/SQLite 上会自带一层事务；脚本里出现显式事务控制会提前提交，
/// 使「失败整体回滚」的承诺失效，所以必须提前识别并拒绝。
pub fn is_transaction_control_statement(sql: &str) -> bool {
    let stripped = strip_sql_comments(sql);
    let first = stripped
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_matches(|c: char| c == '(' || c == ';')
        .to_ascii_uppercase();
    matches!(
        first.as_str(),
        "BEGIN" | "START" | "COMMIT" | "ROLLBACK" | "SAVEPOINT" | "RELEASE" | "END"
    )
}

/// 取语句开头的若干个 token（已去注释、转大写），遇到 `(` 或含引号的字面量即停止。
///
/// 只用于识别语句「头部形态」，因此不需要完整解析：字符串字面量里的关键字
/// （如 `INSERT ... VALUES ('DROP TABLE')`）不会出现在头部 token 里。
fn leading_tokens(sql: &str) -> Vec<String> {
    let stripped = strip_sql_comments(sql);
    let mut tokens = Vec::new();
    for raw in stripped.split_whitespace() {
        let token = raw
            .trim_matches(|c: char| c == '(' || c == ';')
            .to_ascii_uppercase();
        if raw.starts_with('(') || token.is_empty() {
            break;
        }
        if token.contains('\'') || token.contains('`') || token.contains('"') {
            break;
        }
        tokens.push(token);
        if tokens.len() >= 8 {
            break;
        }
    }
    tokens
}

/// MySQL/MariaDB 语句是否会触发隐式提交。
///
/// 判定刻意保守：只要语句类别存在隐式提交可能（DDL、LOCK/UNLOCK TABLES、
/// `SET autocommit`、事务控制、管理类命令、带 COMMIT 的存储过程调用等），
/// 就返回 true，让脚本导入走非原子逐条路径。宁可少包一次事务，也不把可能
/// 隐式提交的脚本包进事务后谎报「已回滚」。
pub fn mysql_statement_causes_implicit_commit(sql: &str) -> bool {
    let tokens = leading_tokens(sql);
    let Some(first) = tokens.first().map(String::as_str) else {
        return false;
    };
    match first {
        // DDL：CREATE/ALTER/DROP/RENAME/TRUNCATE 一律视为隐式提交
        // （CREATE TEMPORARY TABLE 等例外也按不安全处理）。
        "CREATE" | "ALTER" | "DROP" | "RENAME" | "TRUNCATE" => true,
        // 锁表与事务控制。
        "LOCK" | "UNLOCK" | "BEGIN" | "START" | "COMMIT" | "ROLLBACK" | "SAVEPOINT"
        | "RELEASE" | "XA" => true,
        // 账号与权限。
        "GRANT" | "REVOKE" => true,
        // 管理类命令。
        "ANALYZE" | "OPTIMIZE" | "REPAIR" | "CHECK" | "FLUSH" | "RESET" | "CACHE" | "INSTALL"
        | "UNINSTALL" | "LOAD" | "CHANGE" | "PURGE" | "SHUTDOWN" | "RESTART" | "KILL" => true,
        // 存储过程内部可能自带 COMMIT。
        "CALL" => true,
        // SET autocommit / SET PASSWORD 会隐式提交；其余 SET 视为安全。
        "SET" => tokens.iter().any(|t| {
            let ident = t.split('=').next().unwrap_or("").trim_end_matches(';');
            let bare = ident
                .trim_start_matches('@')
                .trim_start_matches("GLOBAL")
                .trim_start_matches("SESSION")
                .trim_start_matches("LOCAL");
            bare == "AUTOCOMMIT" || bare == "PASSWORD"
        }),
        _ => false,
    }
}

/// 返回脚本中第一条会触发 MySQL 隐式提交的语句 (1-based 序号, 语句预览)。
///
/// `None` 表示整个脚本都是可事务化的 DML，可以安全地用 `START TRANSACTION`
/// 包住并在失败时整体回滚。
pub fn mysql_script_implicit_commit_statement(statements: &[String]) -> Option<(usize, String)> {
    statements
        .iter()
        .enumerate()
        .find(|(_, sql)| mysql_statement_causes_implicit_commit(sql))
        .map(|(idx, sql)| {
            let preview: String = sql.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(120).collect();
            (idx + 1, preview)
        })
}

/// 该语句是否属于「不能在 PostgreSQL 事务块中执行」的类别。
///
/// 返回类别标签（用于错误信息），`None` 表示可在事务内执行。
pub fn pg_non_transactional_statement_kind(sql: &str) -> Option<&'static str> {
    let tokens = leading_tokens(sql);
    let first = tokens.first()?.as_str();
    let second = tokens.get(1).map(String::as_str).unwrap_or("");
    let prefix_has = |kw: &str| tokens.iter().any(|t| t == kw);

    match first {
        "VACUUM" => Some("VACUUM"),
        "CREATE" | "DROP" => {
            let is_create = first == "CREATE";
            // `CREATE UNIQUE INDEX` 允许 UNIQUE 出现在对象类型之前。
            let mut rest = &tokens[1..];
            if rest.first().map(String::as_str) == Some("UNIQUE") {
                rest = &rest[1..];
            }
            match rest.first().map(String::as_str).unwrap_or("") {
                "DATABASE" => Some(if is_create { "CREATE DATABASE" } else { "DROP DATABASE" }),
                "TABLESPACE" => {
                    Some(if is_create { "CREATE TABLESPACE" } else { "DROP TABLESPACE" })
                }
                "SUBSCRIPTION" => {
                    Some(if is_create { "CREATE SUBSCRIPTION" } else { "DROP SUBSCRIPTION" })
                }
                "INDEX" if prefix_has("CONCURRENTLY") => Some(if is_create {
                    "CREATE INDEX CONCURRENTLY"
                } else {
                    "DROP INDEX CONCURRENTLY"
                }),
                _ => None,
            }
        }
        "REINDEX" if prefix_has("CONCURRENTLY") => Some("REINDEX CONCURRENTLY"),
        "ALTER" => match second {
            "SYSTEM" => Some("ALTER SYSTEM"),
            "DATABASE" if prefix_has("TABLESPACE") => Some("ALTER DATABASE ... SET TABLESPACE"),
            _ => None,
        },
        _ => None,
    }
}

/// 把 SQL 脚本拆成独立语句。
///
/// 按顶层分号拆分，字符串（单引号/双引号/反引号）、行注释、块注释与
/// PostgreSQL 美元引号（`$$...$$`、`$tag$...$tag$`）里的分号不参与拆分。
/// 纯空白或纯注释片段会被丢弃。
pub fn split_sql_statements(sql: &str) -> Vec<String> {
    #[derive(Clone, PartialEq)]
    enum State {
        Normal,
        LineComment,
        BlockComment,
        SingleQuote,
        DoubleQuote,
        Backtick,
        DollarQuote(String),
    }

    // 尝试识别 `$tag$` 形式的美元引号开头，返回 (tag, 开标记长度)。
    fn dollar_open(chars: &[char], start: usize) -> Option<(String, usize)> {
        let mut j = start + 1;
        while j < chars.len() {
            let ch = chars[j];
            if ch == '$' {
                return Some((chars[start + 1..j].iter().collect(), j - start + 1));
            }
            if !(ch.is_ascii_alphanumeric() || ch == '_') {
                return None;
            }
            j += 1;
        }
        None
    }

    let chars: Vec<char> = sql.chars().collect();
    let mut statements = Vec::new();
    let mut current = String::new();
    let mut has_code = false;
    let mut state = State::Normal;
    let mut i = 0usize;

    while i < chars.len() {
        let c = chars[i];
        match state.clone() {
            State::Normal => match c {
                '-' if chars.get(i + 1) == Some(&'-') => {
                    current.push('-');
                    current.push('-');
                    i += 2;
                    state = State::LineComment;
                }
                '#' => {
                    current.push(c);
                    i += 1;
                    state = State::LineComment;
                }
                '/' if chars.get(i + 1) == Some(&'*') => {
                    current.push('/');
                    current.push('*');
                    i += 2;
                    state = State::BlockComment;
                }
                '\'' => {
                    current.push(c);
                    has_code = true;
                    i += 1;
                    state = State::SingleQuote;
                }
                '"' => {
                    current.push(c);
                    has_code = true;
                    i += 1;
                    state = State::DoubleQuote;
                }
                '`' => {
                    current.push(c);
                    has_code = true;
                    i += 1;
                    state = State::Backtick;
                }
                '$' => match dollar_open(&chars, i) {
                    Some((tag, len)) => {
                        current.extend(&chars[i..i + len]);
                        has_code = true;
                        i += len;
                        state = State::DollarQuote(tag);
                    }
                    None => {
                        current.push(c);
                        has_code = true;
                        i += 1;
                    }
                },
                ';' => {
                    if has_code {
                        let stmt = current.trim();
                        if !stmt.is_empty() {
                            statements.push(stmt.to_string());
                        }
                    }
                    current.clear();
                    has_code = false;
                    i += 1;
                }
                _ => {
                    if !c.is_whitespace() {
                        has_code = true;
                    }
                    current.push(c);
                    i += 1;
                }
            },
            State::LineComment => {
                current.push(c);
                i += 1;
                if c == '\n' {
                    state = State::Normal;
                }
            }
            State::BlockComment => {
                current.push(c);
                i += 1;
                if c == '*' && chars.get(i) == Some(&'/') {
                    current.push('/');
                    i += 1;
                    state = State::Normal;
                }
            }
            State::SingleQuote => {
                current.push(c);
                i += 1;
                if c == '\'' {
                    if chars.get(i) == Some(&'\'') {
                        current.push('\'');
                        i += 1;
                    } else {
                        state = State::Normal;
                    }
                }
            }
            State::DoubleQuote => {
                current.push(c);
                i += 1;
                if c == '"' {
                    if chars.get(i) == Some(&'"') {
                        current.push('"');
                        i += 1;
                    } else {
                        state = State::Normal;
                    }
                }
            }
            State::Backtick => {
                current.push(c);
                i += 1;
                if c == '`' {
                    if chars.get(i) == Some(&'`') {
                        current.push('`');
                        i += 1;
                    } else {
                        state = State::Normal;
                    }
                }
            }
            State::DollarQuote(tag) => {
                if c == '$' {
                    let tag_len = tag.chars().count();
                    let closes = tag.chars().enumerate().all(|(k, tc)| {
                        chars.get(i + 1 + k) == Some(&tc)
                    }) && chars.get(i + 1 + tag_len) == Some(&'$');
                    if closes {
                        let len = tag_len + 2;
                        current.extend(&chars[i..i + len]);
                        i += len;
                        state = State::Normal;
                        continue;
                    }
                }
                current.push(c);
                i += 1;
            }
        }
    }

    if has_code {
        let stmt = current.trim();
        if !stmt.is_empty() {
            statements.push(stmt.to_string());
        }
    }
    statements
}

/// MySQL 表名引用：支持 `db.table` 两级，分别加反引号。
pub fn quote_table_ref_mysql(name: &str) -> String {
    match name.split_once('.') {
        Some((schema, table)) if !schema.is_empty() && !table.is_empty() => {
            format!("{}.{}", quote_ident_mysql(schema), quote_ident_mysql(table))
        }
        _ => quote_ident_mysql(name),
    }
}

/// 按引擎选择列引用的哨兵占位符（`?` 或 `$n`）。
/// 序号从 1 开始；MySQL/SQLite 忽略序号。
pub fn placeholder_for(
    style: crate::db::client::PlaceholderStyle,
    index: usize,
) -> String {
    use crate::db::client::PlaceholderStyle;
    match style {
        PlaceholderStyle::QuestionMark => "?".to_string(),
        PlaceholderStyle::DollarNumber => format!("${}", index),
        PlaceholderStyle::AtP => format!("@p{}", index),
        PlaceholderStyle::ColonNumber => format!(":{}", index),
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

/// MySQL `information_schema` 的字符串列在部分 8.0 版本会被驱动标记为 binary，
/// 直接按 `String` 解码会报 `VARCHAR`/`VARBINARY` 不匹配。这里先按字符串解码，
/// 失败时退回按字节解码再转文本。
pub fn mysql_text(row: &sqlx::mysql::MySqlRow, name: &str) -> Option<String> {
    use sqlx::Row;
    if let Ok(v) = row.try_get::<Option<String>, _>(name) {
        return v;
    }
    if let Ok(v) = row.try_get::<Option<Vec<u8>>, _>(name) {
        return v.map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
    }
    None
}

/// 把 JSON 数组转成 PostgreSQL 数组字面量（用于导入到 array 类型列）。
///
/// 只处理标量元素（字符串/数字/布尔/NULL）；嵌套数组或对象返回 `None`，
/// 由调用方退回 JSON 文本（如 jsonb 列里的数组）。
pub fn pg_array_literal(value: &Value) -> Option<String> {
    let arr = value.as_array()?;
    if arr.is_empty() {
        // 空数组必须带类型才能使用，`'{}'` 是唯一不需要元素类型的写法。
        return Some("'{}'".to_string());
    }
    let mut parts = Vec::with_capacity(arr.len());
    for item in arr {
        match item {
            Value::Null => parts.push("NULL".to_string()),
            Value::Bool(b) => parts.push(if *b { "TRUE" } else { "FALSE" }.to_string()),
            Value::Number(n) => parts.push(n.to_string()),
            Value::String(s) => parts.push(format!("'{}'", escape_sql_string(s))),
            Value::Array(_) | Value::Object(_) => return None,
        }
    }
    Some(format!("ARRAY[{}]", parts.join(", ")))
}

/// 「类型未映射」标记的键名。
///
/// 真 NULL 用 JSON `null` 表示；某个非空值无法被字段级映射时，返回
/// `{ "$unmapped_type": <PG 类型名>, "$value": <原始文本或 null>, "$encoding": "text"|"binary" }`，
/// 让前端能把「数据是空」和「能力没覆盖」区分开。
pub const UNMAPPED_TYPE_KEY: &str = "$unmapped_type";
pub const UNMAPPED_VALUE_KEY: &str = "$value";
pub const UNMAPPED_ENCODING_KEY: &str = "$encoding";

/// 这些 PostgreSQL 基础类型有专门的解码/映射（sqlx Decode 或本模块的文本化处理）。
pub fn pg_type_has_field_mapping(type_name: &str) -> bool {
    let type_name = type_name.to_ascii_lowercase();
    matches!(
        type_name.as_str(),
        "bool"
            | "int2"
            | "int4"
            | "int8"
            | "float4"
            | "float8"
            | "numeric"
            | "text"
            | "varchar"
            | "bpchar"
            | "char"
            | "name"
            | "citext"
            | "json"
            | "jsonb"
            | "uuid"
            | "timestamptz"
            | "timestamp"
            | "date"
            | "time"
            | "timetz"
            | "bytea"
    )
}

/// 这些 PostgreSQL 类型没有对应的 Rust 解码器，但服务端文本表示本身就是
/// 稳定、可读的，直接按文本回读即可（枚举与 range 由 `PgTypeKind` 判定）。
pub fn pg_text_type_supported(type_name: &str) -> bool {
    // 内置类型的 `PgTypeInfo::name()` 返回大写（INET/INTERVAL），自定义类型保持
    // 定义时的大小写，所以这里统一转小写比较。
    let type_name = type_name.to_ascii_lowercase();
    matches!(
        type_name.as_str(),
        "inet"
            | "cidr"
            | "macaddr"
            | "macaddr8"
            | "interval"
            | "money"
            | "tsvector"
            | "tsquery"
            | "xml"
            | "bit"
            | "varbit"
            | "bit varying"
            | "point"
            | "line"
            | "lseg"
            | "box"
            | "path"
            | "polygon"
            | "circle"
            | "oid"
            | "regclass"
            | "regtype"
            | "pg_lsn"
            | "txid_snapshot"
    )
}

/// 自定义类型（OID >= 16384）的元信息，来自 `pg_type` 目录查询。
///
/// 简单查询协议下 sqlx 不会解析用户自定义类型的 OID，直接调用 `name()`
/// 会触发 `unreachable!`。这里预先查好名称与类别，避免依赖内部解析状态。
#[derive(Debug, Clone, Default)]
pub struct PgTypeMeta {
    /// `format_type(oid, NULL)` 的结果，如 `mood`、`mood[]`、`color_pair`
    pub type_name: String,
    /// pg_type.typtype：b 基础 / c 复合 / d 域 / e 枚举 / r range / m 多值 range
    pub type_kind: String,
    /// 数组元素类型的 typtype，非数组为空
    pub elem_type_kind: String,
}

impl PgTypeMeta {
    fn text_readable(&self) -> bool {
        matches!(self.type_kind.as_str(), "e" | "r" | "m" | "d")
            || matches!(self.elem_type_kind.as_str(), "e" | "r" | "m" | "d")
    }
}

/// 该列的 `PgTypeInfo` 是否是 sqlx「未解析」的类型声明。
///
/// 简单查询协议（`raw_sql`）不会去 catalog 解析非内置 OID，`PgTypeInfo` 停在
/// `DeclareWithOid`：此时 `name()` 恒为 `"?"`，而 `kind()` 会直接 `unreachable!`，
/// 所有基于数组/类别的解码兼容性检查（`array_compatible` → `kind()`）都会 panic。
/// 这类列必须跳过逐类型 try_get，直接走目录兜底。
pub fn pg_type_is_unresolved(type_info: &sqlx::postgres::PgTypeInfo) -> bool {
    use sqlx::TypeInfo;
    type_info.oid().is_none() || type_info.name() == "?"
}

/// 该 PostgreSQL 类型是否"按文本即可读回"（枚举、range、domain 以及上面列出的文本类型）。
///
/// 数组按元素类型递归判断：枚举数组/range 数组的文本表示（`{a,b}`）同样可读。
fn pg_type_is_text_readable(type_info: &sqlx::postgres::PgTypeInfo) -> bool {
    use sqlx::postgres::PgTypeKind;
    use sqlx::TypeInfo;
    match type_info.kind() {
        PgTypeKind::Enum(_) | PgTypeKind::Range(_) | PgTypeKind::Domain(_) => true,
        PgTypeKind::Array(element) => pg_type_is_text_readable(element),
        _ => pg_text_type_supported(type_info.name()),
    }
}

/// 所有字段级解码都失败后的兜底：
/// - 真 NULL 仍返回 `Value::Null`；
/// - 可文本化的类型（枚举/range/inet/interval 等）返回原始文本；
/// - 其余类型返回结构化「类型未映射」标记，绝不静默变成空值。
fn pg_cell_fallback(
    row: &sqlx::postgres::PgRow,
    idx: usize,
    catalog: &std::collections::HashMap<u32, PgTypeMeta>,
) -> Value {
    use sqlx::{Row, TypeInfo, ValueRef};

    let raw = match row.try_get_raw(idx) {
        Ok(raw) => raw,
        // 列序号越界等异常：此时无法判断是否为 NULL，按未映射标记返回。
        Err(_) => return pg_unmapped_value("<unknown>", None, "unknown"),
    };
    if raw.is_null() {
        return Value::Null;
    }

    // 简单查询协议下结果固定是文本格式；若走了二进制（例如带参数的查询），
    // 未映射类型没有解码器，二进制字节无法安全转文本，只能标注编码方式。
    let (text, encoding) = match raw.format() {
        sqlx::postgres::PgValueFormat::Text => {
            (raw.as_str().ok().map(|s| s.to_string()), "text")
        }
        sqlx::postgres::PgValueFormat::Binary => (None, "binary"),
    };

    let type_info = raw.type_info().into_owned();
    let oid = type_info.oid().map(|o| o.0).unwrap_or(0);

    // 已解析的内置类型：name()/kind() 可安全调用，直接按类型判断。
    if !pg_type_is_unresolved(&type_info) {
        let type_name = type_info.name().to_string();
        if pg_type_is_text_readable(&type_info) {
            if let Some(text) = text {
                return Value::String(text);
            }
        }
        return pg_unmapped_value(&type_name, text.as_deref(), encoding);
    }

    // 未解析的类型：优先用目录查询结果判断，缺失时退化为 OID 标记。
    match catalog.get(&oid) {
        Some(meta) if meta.text_readable() => match text {
            Some(text) => Value::String(text),
            None => pg_unmapped_value(&meta.type_name, None, encoding),
        },
        // 扩展类型（如 citext）在目录里是基础类型，但文本表示可直接回读。
        Some(meta)
            if pg_text_type_supported(&meta.type_name)
                || pg_type_has_field_mapping(&meta.type_name) =>
        {
            match text {
                Some(text) => Value::String(text),
                None => pg_unmapped_value(&meta.type_name, None, encoding),
            }
        }
        Some(meta) => pg_unmapped_value(&meta.type_name, text.as_deref(), encoding),
        None => pg_unmapped_value(&format!("oid:{}", oid), text.as_deref(), encoding),
    }
}

/// NUMERIC 文本能否用 f64 精确表达。
///
/// 比较方式是「数值相等」而不是字面相等：`1.50` 与 `1.5` 视为同一数值，
/// 只有 f64 丢精度（如 `1234567890123456789.123456789`）时才判为不相等。
fn decimal_text_matches_f64(text: &str, value: f64) -> bool {
    let printed = format!("{}", value);
    normalize_decimal_text(&printed) == normalize_decimal_text(text)
}

/// 去掉小数部分末尾无意义的 0 与单独的 `.`，便于数值文本比较。
fn normalize_decimal_text(s: &str) -> String {
    let s = s.trim();
    if !s.contains('.') && !s.contains('e') && !s.contains('E') {
        return s.trim_start_matches('+').to_string();
    }
    if s.contains('e') || s.contains('E') {
        // 科学计数法不在此做规范化，交给调用方按字符串处理。
        return s.to_string();
    }
    let trimmed = s.trim_end_matches('0');
    let trimmed = trimmed.trim_end_matches('.');
    trimmed.trim_start_matches('+').to_string()
}

fn pg_unmapped_value(type_name: &str, text: Option<&str>, encoding: &str) -> Value {
    let mut map = serde_json::Map::new();
    map.insert(
        UNMAPPED_TYPE_KEY.to_string(),
        Value::String(type_name.to_string()),
    );
    map.insert(
        UNMAPPED_VALUE_KEY.to_string(),
        text.map(|t| Value::String(t.to_string()))
            .unwrap_or(Value::Null),
    );
    map.insert(
        UNMAPPED_ENCODING_KEY.to_string(),
        Value::String(encoding.to_string()),
    );
    Value::Object(map)
}

/// 从 PgRow 的指定列提取为 serde_json::Value
///
/// PostgreSQL 的类型体系比 MySQL 宽：这里按 sqlx 的解码能力显式覆盖
/// 布尔、各宽度整数、浮点、时间/日期、JSON/JSONB、数组与文本。
pub fn pg_cell_to_value(row: &sqlx::postgres::PgRow, idx: usize) -> Value {
    pg_cell_to_value_with_catalog(row, idx, &std::collections::HashMap::new())
}

/// 带自定义类型目录的版本：枚举/复合类型等用户定义类型需要目录来判断类别与展示名。
pub fn pg_cell_to_value_with_catalog(
    row: &sqlx::postgres::PgRow,
    idx: usize,
    catalog: &std::collections::HashMap<u32, PgTypeMeta>,
) -> Value {
    use sqlx::{Row, ValueRef};
    // 未解析的自定义类型必须先绕过下面的逐类型 try_get：数组兼容性检查会调用
    // `PgTypeInfo::kind()`，对 `DeclareWithOid` 是 `unreachable!` panic。
    if let Ok(raw) = row.try_get_raw(idx) {
        if pg_type_is_unresolved(&raw.type_info()) {
            return pg_cell_fallback(row, idx, catalog);
        }
    }
    if let Ok(v) = row.try_get::<Option<bool>, _>(idx) {
        return serde_json::json!(v);
    }
    // JSON / JSONB：保留原始 JSON 结构，而不是字符串化
    if let Ok(v) = row.try_get::<Option<serde_json::Value>, _>(idx) {
        return v.unwrap_or(Value::Null);
    }
    // 数组类型（text[] / int[] / bigint[] / float8[] / bool[]）
    if let Ok(v) = row.try_get::<Option<Vec<String>>, _>(idx) {
        return serde_json::json!(v);
    }
    if let Ok(v) = row.try_get::<Option<Vec<i32>>, _>(idx) {
        return serde_json::json!(v);
    }
    if let Ok(v) = row.try_get::<Option<Vec<i64>>, _>(idx) {
        return serde_json::json!(v);
    }
    if let Ok(v) = row.try_get::<Option<Vec<f64>>, _>(idx) {
        return serde_json::json!(v);
    }
    if let Ok(v) = row.try_get::<Option<Vec<bool>>, _>(idx) {
        return serde_json::json!(v);
    }
    // NUMERIC / DECIMAL：sqlx 不把 NUMERIC 解码为 f64，需要 BigDecimal。
    // f64 能精确还原时输出 JSON number（与 MySQL DECIMAL 行为一致），
    // 超出 f64 精度时退回字符串，避免高精度数值在导出/往返中丢位。
    if let Ok(v) = row.try_get::<Option<sqlx::types::BigDecimal>, _>(idx) {
        return match v {
            Some(v) => {
                let text = v.to_string();
                match text.parse::<f64>() {
                    Ok(f) if f.is_finite() && decimal_text_matches_f64(&text, f) => {
                        serde_json::json!(f)
                    }
                    _ => serde_json::json!(text),
                }
            }
            None => Value::Null,
        };
    }
    // uuid：sqlx 需要 uuid feature 才能解码，输出统一为字符串
    if let Ok(v) = row.try_get::<Option<sqlx::types::uuid::Uuid>, _>(idx) {
        return serde_json::json!(v.map(|u| u.to_string()));
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
    // timestamptz / timetz 解码为带时区类型
    if let Ok(v) = row.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>(idx) {
        return serde_json::json!(v.map(|dt| dt.to_rfc3339()));
    }
    if let Ok(v) = row.try_get::<Option<chrono::DateTime<chrono::FixedOffset>>, _>(idx) {
        return serde_json::json!(v.map(|dt| dt.to_rfc3339()));
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
            // bytea：输出 PostgreSQL 的十六进制文本形式，避免 UTF-8 lossy 产生乱码。
            let hex: String = bytes.iter().map(|b| format!("{:02x}", b)).collect();
            return serde_json::json!(format!("\\x{}", hex));
        }
    }
    pg_cell_fallback(row, idx, catalog)
}

/// 从 PgRow 提取单列为 Option<String>（逐类型 try-get）
pub fn pg_cell_to_string(row: &sqlx::postgres::PgRow, ordinal: usize) -> Option<String> {
    use sqlx::{Row, ValueRef};
    // 未解析的自定义类型跳过逐类型 try_get，避免 `kind()` panic。
    if let Ok(raw) = row.try_get_raw(ordinal) {
        if pg_type_is_unresolved(&raw.type_info()) {
            return match pg_cell_fallback(row, ordinal, &std::collections::HashMap::new()) {
                Value::Null => None,
                Value::String(text) => Some(text),
                other => Some(other.to_string()),
            };
        }
    }
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
    if let Ok(v) = row.try_get::<Option<sqlx::types::BigDecimal>, _>(ordinal) {
        return v.map(|x| x.to_string());
    }
    if let Ok(v) = row.try_get::<Option<sqlx::types::uuid::Uuid>, _>(ordinal) {
        return v.map(|u| u.to_string());
    }
    if let Ok(v) = row.try_get::<Option<bool>, _>(ordinal) {
        return v.map(|x| x.to_string());
    }
    if let Ok(v) = row.try_get::<Option<Vec<u8>>, _>(ordinal) {
        return v.map(|x| {
            let hex: String = x.iter().map(|b| format!("{:02x}", b)).collect();
            format!("\\x{}", hex)
        });
    }
    match pg_cell_fallback(row, ordinal, &std::collections::HashMap::new()) {
        Value::Null => None,
        Value::String(text) => Some(text),
        other => Some(other.to_string()),
    }
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
    fn pg_text_readable_covers_enum_range_and_inet_family() {
        // 枚举/range 由 PgTypeKind 判定，文本类型由白名单判定
        for name in ["inet", "cidr", "interval", "money", "tsvector", "point"] {
            assert!(pg_text_type_supported(name), "{name} 应按文本回读");
        }
        assert!(!pg_text_type_supported("mycomp"));
        assert!(!pg_text_type_supported("mood"));
    }

    #[test]
    fn pg_field_mapping_covers_core_scalar_types() {
        for name in [
            "bool", "int2", "int4", "int8", "float4", "float8", "numeric", "text", "varchar",
            "json", "jsonb", "uuid", "timestamptz", "date", "time", "bytea",
        ] {
            assert!(pg_type_has_field_mapping(name), "{name} 应有字段级映射");
        }
        assert!(!pg_type_has_field_mapping("mycomp"));
        assert!(!pg_type_has_field_mapping("_mood"));
    }

    #[test]
    fn unmapped_marker_is_structured_and_not_null() {
        let marker = pg_unmapped_value("mycomp", Some("(1,abc)"), "text");
        let obj = marker.as_object().expect("未映射标记应为对象");
        assert_eq!(obj.get(UNMAPPED_TYPE_KEY).and_then(|v| v.as_str()), Some("mycomp"));
        assert_eq!(obj.get(UNMAPPED_VALUE_KEY).and_then(|v| v.as_str()), Some("(1,abc)"));
        assert_eq!(obj.get(UNMAPPED_ENCODING_KEY).and_then(|v| v.as_str()), Some("text"));
        assert!(!marker.is_null(), "未映射标记不能是 JSON null");
    }

    #[test]
    fn unmapped_marker_keeps_binary_encoding_when_no_text() {
        let marker = pg_unmapped_value("mycomp", None, "binary");
        let obj = marker.as_object().expect("未映射标记应为对象");
        assert_eq!(obj.get(UNMAPPED_ENCODING_KEY).and_then(|v| v.as_str()), Some("binary"));
        assert!(obj.get(UNMAPPED_VALUE_KEY).map(|v| v.is_null()).unwrap_or(false));
    }

    #[test]
    fn pg_type_is_unresolved_detects_declare_with_oid() {
        use sqlx::postgres::types::Oid;
        use sqlx::Type;
        // 简单查询协议下的自定义类型：只带 OID，name() 为 "?"，kind() 会 panic
        let custom = sqlx::postgres::PgTypeInfo::with_oid(Oid(16_524));
        assert!(pg_type_is_unresolved(&custom));
        // 内置类型已解析
        let int4 = <i32 as Type<sqlx::Postgres>>::type_info();
        assert!(!pg_type_is_unresolved(&int4));
        let text = <String as Type<sqlx::Postgres>>::type_info();
        assert!(!pg_type_is_unresolved(&text));
    }

    #[test]
    fn pg_array_literal_handles_scalars_and_empty() {
        assert_eq!(
            pg_array_literal(&serde_json::json!(["a", "值😀", 1, true])).unwrap(),
            "ARRAY['a', '值😀', 1, TRUE]"
        );
        assert_eq!(pg_array_literal(&serde_json::json!([])).unwrap(), "'{}'");
        assert_eq!(pg_array_literal(&serde_json::json!([{"k": 1}])), None);
    }

    #[test]
    fn decimal_precision_detects_f64_loss() {
        // f64 能精确表达（1.50 与 1.5 视为同一数值）
        assert!(decimal_text_matches_f64("1.50", 1.5));
        assert!(decimal_text_matches_f64("123.45", 123.45));
        assert!(decimal_text_matches_f64("100", 100.0));
        // 超出 f64 精度：必须退回字符串
        let precise = "1234567890123456789.123456789012345678";
        let f = precise.parse::<f64>().unwrap();
        assert!(!decimal_text_matches_f64(precise, f));
    }

    #[test]
    fn split_sql_statements_ignores_semicolons_in_strings_and_comments() {
        let script = "SELECT 1; SELECT ';' AS s; -- comment ;\n/* block ; */ SELECT 2;";
        let stmts = split_sql_statements(script);
        assert_eq!(stmts.len(), 3, "实际拆分：{stmts:?}");
        assert!(stmts[1].contains("';'"));
        assert!(stmts[2].contains("SELECT 2"));
    }

    #[test]
    fn split_sql_statements_handles_dollar_quotes_and_escaped_quotes() {
        let script = "CREATE FUNCTION f() RETURNS void AS $$ BEGIN PERFORM 1; END $$ LANGUAGE plpgsql;\
                      SELECT 'it''s;ok'; SELECT `a;b` FROM t;";
        let stmts = split_sql_statements(script);
        assert_eq!(stmts.len(), 3, "实际拆分：{stmts:?}");
        assert!(stmts[0].contains("PERFORM 1; END"));
        assert!(stmts[1].contains("it''s;ok"));
        assert!(stmts[2].contains("a;b"));
    }

    #[test]
    fn split_sql_statements_drops_empty_and_comment_only_fragments() {
        let stmts = split_sql_statements(" ; -- only comment\n /* another */ ;  ");
        assert!(stmts.is_empty(), "实际拆分：{stmts:?}");

        let stmts = split_sql_statements("SELECT 1; ; ; SELECT 2");
        assert_eq!(stmts, vec!["SELECT 1".to_string(), "SELECT 2".to_string()]);
    }

    #[test]
    fn transaction_control_statements_are_detected() {
        for sql in [
            "BEGIN",
            "START TRANSACTION",
            "COMMIT",
            "ROLLBACK",
            "SAVEPOINT sp1",
            "RELEASE SAVEPOINT sp1",
            "-- leading comment\nCOMMIT;",
            "/* x */ END",
        ] {
            assert!(
                is_transaction_control_statement(sql),
                "应识别为事务控制：{sql}"
            );
        }
        for sql in [
            "SELECT 'BEGIN'",
            "CREATE TABLE t (id int)",
            "INSERT INTO t VALUES (1)",
            "UPDATE t SET a = 'COMMIT'",
        ] {
            assert!(
                !is_transaction_control_statement(sql),
                "不应识别为事务控制：{sql}"
            );
        }
    }

    #[test]
    fn strip_sql_comments_keeps_string_literals() {
        let stripped = strip_sql_comments("SELECT '--not-comment', '#not-comment' -- tail\n/* block */");
        assert!(stripped.contains("'--not-comment'"));
        assert!(stripped.contains("'#not-comment'"));
        assert!(!stripped.contains("tail"));
        assert!(!stripped.contains("block"));
        assert!(stripped.contains("SELECT"));
    }

    #[test]
    fn mysql_implicit_commit_detection_is_conservative() {
        for sql in [
            "CREATE TABLE t (id int)",
            "ALTER TABLE t ADD COLUMN c int",
            "DROP TABLE t",
            "RENAME TABLE a TO b",
            "TRUNCATE TABLE t",
            "LOCK TABLES t WRITE",
            "UNLOCK TABLES",
            "SET autocommit = 0",
            "SET SESSION autocommit=1",
            "SET @@autocommit = 1",
            "SET PASSWORD FOR 'u'@'%' = 'x'",
            "START TRANSACTION",
            "COMMIT",
            "GRANT SELECT ON *.* TO 'u'@'%'",
            "ANALYZE TABLE t",
            "FLUSH TABLES",
            "CALL some_proc()",
            "CREATE TEMPORARY TABLE tmp (id int)",
        ] {
            assert!(
                mysql_statement_causes_implicit_commit(sql),
                "应判定为隐式提交：{sql}"
            );
        }
    }

    #[test]
    fn mysql_implicit_commit_detection_ignores_comments_and_literals() {
        // 注释里出现 DDL 关键字不应误判
        for sql in [
            "-- DROP TABLE users\nINSERT INTO t VALUES (1)",
            "/* CREATE TABLE x */ INSERT INTO t VALUES (1)",
            "# ALTER TABLE t",
            "INSERT INTO audit (note) VALUES ('DROP TABLE users')",
            "UPDATE t SET note = 'CREATE INDEX CONCURRENTLY x' WHERE id = 1",
        ] {
            assert!(
                !mysql_statement_causes_implicit_commit(sql),
                "不应判定为隐式提交：{sql}"
            );
        }
        // 纯 DML 脚本可以包事务
        let script: Vec<String> = ["INSERT INTO t VALUES (1)", "UPDATE t SET a = 1", "DELETE FROM t WHERE id = 2"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(mysql_script_implicit_commit_statement(&script).is_none());
    }

    #[test]
    fn mysql_script_reports_first_implicit_commit_statement() {
        let script: Vec<String> = [
            "INSERT INTO t VALUES (1)",
            "UPDATE t SET a = 2",
            "CREATE TABLE x (id int)",
            "DROP TABLE y",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let (index, preview) = mysql_script_implicit_commit_statement(&script).unwrap();
        assert_eq!(index, 3);
        assert!(preview.starts_with("CREATE TABLE x"));
    }

    #[test]
    fn pg_non_transactional_statements_are_detected() {
        for (sql, kind) in [
            ("CREATE INDEX CONCURRENTLY idx ON t (c)", "CREATE INDEX CONCURRENTLY"),
            ("CREATE UNIQUE INDEX CONCURRENTLY idx ON t (c)", "CREATE INDEX CONCURRENTLY"),
            ("DROP INDEX CONCURRENTLY idx", "DROP INDEX CONCURRENTLY"),
            ("REINDEX INDEX CONCURRENTLY idx", "REINDEX CONCURRENTLY"),
            ("REINDEX TABLE CONCURRENTLY t", "REINDEX CONCURRENTLY"),
            ("VACUUM t", "VACUUM"),
            ("VACUUM (FULL) t", "VACUUM"),
            ("CREATE DATABASE d", "CREATE DATABASE"),
            ("DROP DATABASE d", "DROP DATABASE"),
            ("CREATE TABLESPACE ts LOCATION '/x'", "CREATE TABLESPACE"),
            ("ALTER SYSTEM SET work_mem = '1GB'", "ALTER SYSTEM"),
        ] {
            assert_eq!(
                pg_non_transactional_statement_kind(sql),
                Some(kind),
                "未识别：{sql}"
            );
        }
    }

    #[test]
    fn pg_non_transactional_detection_ignores_comments_and_literals() {
        for sql in [
            "INSERT INTO t VALUES ('CREATE INDEX CONCURRENTLY idx ON t')",
            "SELECT 'VACUUM'",
            "-- CREATE INDEX CONCURRENTLY idx ON t\nSELECT 1",
            "/* ALTER SYSTEM SET x = 1 */ SELECT 1",
            "CREATE INDEX idx ON t (c)",
            "CREATE TABLE t (id int)",
            "REINDEX INDEX idx",
        ] {
            assert_eq!(
                pg_non_transactional_statement_kind(sql),
                None,
                "误判：{sql}"
            );
        }
    }

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

    #[test]
    fn placeholder_for_matches_engine_style() {
        use crate::db::client::PlaceholderStyle;
        assert_eq!(placeholder_for(PlaceholderStyle::QuestionMark, 1), "?");
        assert_eq!(placeholder_for(PlaceholderStyle::QuestionMark, 3), "?");
        assert_eq!(placeholder_for(PlaceholderStyle::DollarNumber, 1), "$1");
        assert_eq!(placeholder_for(PlaceholderStyle::DollarNumber, 4), "$4");
    }

    #[test]
    fn quote_table_ref_handles_schema_qualified_names() {
        use crate::config::DbType;
        assert_eq!(
            quote_table_ref_checked("users", &DbType::PostgreSQL).unwrap(),
            "\"users\""
        );
        assert_eq!(
            quote_table_ref_checked("public.users", &DbType::PostgreSQL).unwrap(),
            "\"public\".\"users\""
        );
        assert_eq!(
            quote_table_ref_checked("app.orders", &DbType::PostgreSQL).unwrap(),
            "\"app\".\"orders\""
        );
        assert_eq!(quote_table_ref_mysql("shop.users"), "`shop`.`users`");
        assert_eq!(quote_table_ref_mysql("users"), "`users`");
        assert_eq!(
            split_pg_table_ref("app.orders"),
            (Some("app"), "orders")
        );
        assert_eq!(split_pg_table_ref("orders"), (None, "orders"));
    }
}
