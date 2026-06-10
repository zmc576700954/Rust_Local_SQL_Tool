//! Whitelist + blacklist validation for user-provided ORDER BY expressions.

const MAX_LEN: usize = 200;

const KEYWORD_BLACKLIST: &[&str] = &[
    "--", "/*", "*/", ";",
    "union", "select", "insert", "update", "delete", "drop", "truncate",
    "into", "from", "where", "exec", "benchmark", "sleep",
    "load_file", "outfile", "information_schema",
];

/// Allow only a safe subset of characters that ORDER BY expressions need:
/// identifiers, dots (qualified names), commas, parentheses, basic arithmetic,
/// quoted strings, backticked identifiers, and whitespace.
fn is_allowed_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric()
        || matches!(
            ch,
            '_' | '.' | ',' | '(' | ')' | ' ' | '*' | '+' | '-' | '/' | '\'' | '"' | '`' | '\t' | '\n'
        )
}

pub fn sanitize_sort_expression(expr: &str) -> Result<(), String> {
    if expr.is_empty() {
        return Err("排序表达式不能为空".into());
    }
    if expr.len() > MAX_LEN {
        return Err(format!("排序表达式长度超过 {} 字符上限", MAX_LEN));
    }
    for ch in expr.chars() {
        if !is_allowed_char(ch) {
            return Err(format!("排序表达式含非法字符: {:?}", ch));
        }
    }

    // Parenthesis balance.
    let mut depth: i32 = 0;
    for ch in expr.chars() {
        if ch == '(' { depth += 1; }
        if ch == ')' {
            depth -= 1;
            if depth < 0 {
                return Err("排序表达式圆括号不匹配".into());
            }
        }
    }
    if depth != 0 {
        return Err("排序表达式圆括号不匹配".into());
    }

    let lower = expr.to_ascii_lowercase();
    for kw in KEYWORD_BLACKLIST {
        if lower.contains(kw) {
            return Err(format!("排序表达式含禁用关键字: {}", kw));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::sanitize_sort_expression;

    #[test]
    fn accepts_simple_function_call() {
        assert!(sanitize_sort_expression("LENGTH(name)").is_ok());
    }

    #[test]
    fn accepts_cast_expression() {
        assert!(sanitize_sort_expression("CAST(price AS DECIMAL(10,2))").is_ok());
    }

    #[test]
    fn accepts_arithmetic() {
        assert!(sanitize_sort_expression("(price * 1.1) + tax").is_ok());
    }

    #[test]
    fn accepts_backticked_identifier() {
        assert!(sanitize_sort_expression("`col` * 2").is_ok());
    }

    #[test]
    fn accepts_coalesce() {
        assert!(sanitize_sort_expression("COALESCE(a, b)").is_ok());
    }

    #[test]
    fn accepts_collate_clause() {
        assert!(sanitize_sort_expression("name COLLATE utf8mb4_bin").is_ok());
    }

    #[test]
    fn rejects_statement_terminator() {
        assert!(sanitize_sort_expression("name; DROP TABLE x").is_err());
    }

    #[test]
    fn rejects_inline_subquery() {
        assert!(sanitize_sort_expression("(SELECT 1)").is_err());
    }

    #[test]
    fn rejects_or_injection_with_comment() {
        assert!(sanitize_sort_expression("a OR 1=1 --").is_err());
    }

    #[test]
    fn rejects_union() {
        assert!(sanitize_sort_expression("UNION SELECT *").is_err());
    }

    #[test]
    fn rejects_benchmark() {
        assert!(sanitize_sort_expression("BENCHMARK(1000000, MD5('x'))").is_err());
    }

    #[test]
    fn rejects_from_clause() {
        assert!(sanitize_sort_expression("name FROM users").is_err());
    }

    #[test]
    fn rejects_unbalanced_parentheses() {
        assert!(sanitize_sort_expression("LENGTH(name").is_err());
    }

    #[test]
    fn rejects_too_long() {
        let s = "a".repeat(201);
        assert!(sanitize_sort_expression(&s).is_err());
    }

    #[test]
    fn rejects_empty() {
        assert!(sanitize_sort_expression("").is_err());
    }

    #[test]
    fn rejects_semicolon_only() {
        assert!(sanitize_sort_expression(";").is_err());
    }

    #[test]
    fn rejects_information_schema() {
        assert!(sanitize_sort_expression("information_schema.tables").is_err());
    }
}
