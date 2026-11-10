//! 索引表达式/谓词的规范化。
//!
//! 结构对比不能直接比较原始 SQL：不同写法（多余括号、空白、关键字大小写）
//! 会表达同一个索引；但也不能宽松到把不同表达式判成相等。这里的规则是
//! **保守规范化**：只消除引擎语义上确定等价的差异，其余一律保留，交给
//! 对比逻辑报 diff。
//!
//! - 单引号字符串字面量逐字保留（大小写敏感）；
//! - PostgreSQL 双引号标识符逐字保留（大小写敏感）；SQLite/MySQL 的标识符
//!   大小写不敏感，统一去引号并小写；
//! - 普通 token 去掉空白并小写（关键字/未加引号标识符大小写不敏感）；
//! - PostgreSQL 的美元引用字符串逐字保留；
//! - 最外层成对的括号重复剥离（`(lower(a))` 与 `lower(a)` 等价）。

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlDialect {
    Postgres,
    Sqlite,
    Mysql,
}

/// 规范化索引列表达式或 `WHERE` 谓词。
pub fn normalize_index_expression(dialect: SqlDialect, input: &str) -> String {
    let stripped = strip_outer_parens(&collapse(dialect, input));
    stripped
}

fn collapse(dialect: SqlDialect, input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\'' => {
                // 单引号字符串：逐字保留，'' 为转义。
                out.push(c);
                i += 1;
                while i < chars.len() {
                    out.push(chars[i]);
                    if chars[i] == '\'' {
                        if chars.get(i + 1) == Some(&'\'') {
                            out.push('\'');
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            '"' | '`' | '[' if dialect != SqlDialect::Postgres || c == '"' => {
                let close = if c == '[' { ']' } else { c };
                let mut content = String::new();
                i += 1;
                while i < chars.len() {
                    if chars[i] == close {
                        if chars.get(i + 1) == Some(&close) && close != ']' {
                            content.push(close);
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    content.push(chars[i]);
                    i += 1;
                }
                if dialect == SqlDialect::Postgres {
                    // PG 双引号标识符大小写敏感，保留引号与内容。
                    out.push('"');
                    out.push_str(&content);
                    out.push('"');
                } else {
                    out.push_str(&content.to_lowercase());
                }
            }
            '$' if dialect == SqlDialect::Postgres => {
                // 美元引用：$tag$...$tag$。无法识别时按普通字符处理。
                if let Some((tag, end)) = dollar_quote(&chars, i) {
                    for ch in &chars[i..end] {
                        out.push(*ch);
                    }
                    let _ = tag;
                    i = end;
                } else {
                    out.push('$');
                    i += 1;
                }
            }
            c if c.is_whitespace() => {
                i += 1;
            }
            c => {
                out.push(c.to_ascii_lowercase());
                i += 1;
            }
        }
    }
    out
}

/// 解析从 `start` 开始的美元引用，返回 (tag, 结束位置)。结束位置为闭引号之后。
fn dollar_quote(chars: &[char], start: usize) -> Option<(String, usize)> {
    debug_assert_eq!(chars[start], '$');
    let mut i = start + 1;
    let mut tag = String::new();
    while i < chars.len() && chars[i] != '$' {
        if !(chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
            return None;
        }
        tag.push(chars[i]);
        i += 1;
    }
    if i >= chars.len() {
        return None;
    }
    let open_len = tag.chars().count() + 2;
    let close: Vec<char> = format!("${}$", tag).chars().collect();
    let mut j = start + open_len;
    while j + close.len() <= chars.len() {
        if chars[j..j + close.len()] == close[..] {
            return Some((tag, j + close.len()));
        }
        j += 1;
    }
    None
}

/// 重复剥离最外层成对括号。仅当首尾括号确实互相匹配时才剥离。
fn strip_outer_parens(input: &str) -> String {
    let mut current = input;
    loop {
        let bytes: Vec<char> = current.chars().collect();
        if bytes.first() != Some(&'(') || bytes.last() != Some(&')') {
            return current.to_string();
        }
        let mut depth = 0i32;
        let mut encloses_all = true;
        let mut quote: Option<char> = None;
        for (idx, c) in bytes.iter().enumerate() {
            if let Some(q) = quote {
                if *c == q {
                    quote = None;
                }
                continue;
            }
            match c {
                '\'' | '"' | '`' => quote = Some(*c),
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 && idx != bytes.len() - 1 {
                        encloses_all = false;
                        break;
                    }
                    if depth < 0 {
                        return current.to_string();
                    }
                }
                _ => {}
            }
        }
        if !encloses_all || depth != 0 {
            return current.to_string();
        }
        let start = current.char_indices().next().map(|(i, _)| i).unwrap_or(0);
        let end = current
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
        current = &current[start + 1..end];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_normalization_ignores_case_whitespace_and_outer_parens() {
        assert_eq!(
            normalize_index_expression(SqlDialect::Sqlite, " LOWER( Name ) "),
            normalize_index_expression(SqlDialect::Sqlite, "lower(name)")
        );
        assert_eq!(
            normalize_index_expression(SqlDialect::Sqlite, "((a + b))"),
            "a+b"
        );
    }

    #[test]
    fn string_literals_are_case_sensitive() {
        assert_ne!(
            normalize_index_expression(SqlDialect::Sqlite, "a = 'X'"),
            normalize_index_expression(SqlDialect::Sqlite, "a = 'x'")
        );
    }

    #[test]
    fn postgres_quoted_identifiers_are_case_sensitive() {
        assert_ne!(
            normalize_index_expression(SqlDialect::Postgres, "\"Name\""),
            normalize_index_expression(SqlDialect::Postgres, "\"name\"")
        );
        assert_eq!(
            normalize_index_expression(SqlDialect::Postgres, "LOWER(\"Name\")"),
            normalize_index_expression(SqlDialect::Postgres, "lower(\"Name\")")
        );
    }

    #[test]
    fn postgres_dollar_quotes_are_preserved() {
        assert_eq!(
            normalize_index_expression(SqlDialect::Postgres, "$tag$A B$tag$"),
            "$tag$A B$tag$"
        );
    }

    #[test]
    fn different_expressions_stay_different() {
        assert_ne!(
            normalize_index_expression(SqlDialect::Sqlite, "lower(a)"),
            normalize_index_expression(SqlDialect::Sqlite, "upper(a)")
        );
        assert_ne!(
            normalize_index_expression(SqlDialect::Postgres, "a + b"),
            normalize_index_expression(SqlDialect::Postgres, "a - b")
        );
    }

    #[test]
    fn mysql_backtick_identifiers_are_case_insensitive() {
        assert_eq!(
            normalize_index_expression(SqlDialect::Mysql, "`Name` + 1"),
            normalize_index_expression(SqlDialect::Mysql, "name + 1")
        );
    }
}
