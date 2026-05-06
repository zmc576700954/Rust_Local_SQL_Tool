use serde_json::{Map, Value};

pub fn extract_placeholders(sql_template: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    let bytes = sql_template.as_bytes();

    while i + 1 < bytes.len() {
        if bytes[i] == b'{' && bytes[i + 1] == b'{' {
            if let Some(end) = find_close(sql_template, i + 2) {
                let raw = &sql_template[i + 2..end];
                let key = raw.trim();
                if !key.is_empty() && !out.iter().any(|k| k == key) {
                    out.push(key.to_string());
                }
                i = end + 2;
                continue;
            }
        }
        i += 1;
    }

    out
}

pub fn render_template(sql_template: &str, params: &Map<String, Value>) -> String {
    render_template_inner(sql_template, params).unwrap_or_else(|_| sql_template.to_string())
}

fn render_template_inner(
    sql_template: &str,
    params: &Map<String, Value>,
) -> Result<String, String> {
    let mut out = String::with_capacity(sql_template.len());
    let mut i = 0;
    let bytes = sql_template.as_bytes();

    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'{' && bytes[i + 1] == b'{' {
            let start = i;
            let end = find_close(sql_template, i + 2)
                .ok_or_else(|| "unclosed placeholder".to_string())?;
            let raw = &sql_template[i + 2..end];
            let key = raw.trim();
            let val = params
                .get(key)
                .ok_or_else(|| format!("missing param: {}", key))?;

            let quoted = is_surrounded_by_single_quotes(sql_template, start, end + 2);
            let rendered = render_value(val, quoted)?;
            out.push_str(&rendered);

            i = end + 2;
            continue;
        }

        out.push(bytes[i] as char);
        i += 1;
    }

    Ok(out)
}

fn find_close(s: &str, from: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = from;
    while i + 1 < bytes.len() {
        if bytes[i] == b'}' && bytes[i + 1] == b'}' {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn is_surrounded_by_single_quotes(
    s: &str,
    placeholder_start: usize,
    placeholder_end_exclusive: usize,
) -> bool {
    let before = prev_non_ws_byte(s, placeholder_start);
    let after = next_non_ws_byte(s, placeholder_end_exclusive);
    matches!(before, Some(b'\'')) && matches!(after, Some(b'\''))
}

fn prev_non_ws_byte(s: &str, from: usize) -> Option<u8> {
    let bytes = s.as_bytes();
    let mut i = from;
    while i > 0 {
        i -= 1;
        let b = bytes[i];
        if !b.is_ascii_whitespace() {
            return Some(b);
        }
    }
    None
}

fn next_non_ws_byte(s: &str, from: usize) -> Option<u8> {
    let bytes = s.as_bytes();
    let mut i = from;
    while i < bytes.len() {
        let b = bytes[i];
        if !b.is_ascii_whitespace() {
            return Some(b);
        }
        i += 1;
    }
    None
}

fn render_value(value: &Value, already_quoted: bool) -> Result<String, String> {
    match value {
        Value::Null => Ok("NULL".to_string()),
        Value::Bool(b) => Ok(if *b { "TRUE" } else { "FALSE" }.to_string()),
        Value::Number(n) => Ok(n.to_string()),
        Value::String(s) => {
            let escaped = escape_sql_string(s);
            if already_quoted {
                Ok(escaped)
            } else {
                Ok(format!("'{}'", escaped))
            }
        }
        _ => Err("unsupported json value type".to_string()),
    }
}

fn escape_sql_string(s: &str) -> String {
    s.replace('\'', "''")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_placeholders_trim_and_dedup() {
        let sql = "select * from t where a={{a}} and b={{ a }} and c={{c}} and a2={{a}}";
        let p = extract_placeholders(sql);
        assert_eq!(p, vec!["a".to_string(), "c".to_string()]);
    }

    #[test]
    fn renders_with_existing_quotes() {
        let sql = "select * from t where name='{{name}}'";
        let mut m = Map::new();
        m.insert("name".to_string(), json!("O'Reilly"));
        let rendered = render_template(sql, &m);
        assert_eq!(rendered, "select * from t where name='O''Reilly'");
    }

    #[test]
    fn renders_without_quotes_adds_quotes() {
        let sql = "select * from t where name={{name}}";
        let mut m = Map::new();
        m.insert("name".to_string(), json!("abc"));
        let rendered = render_template(sql, &m);
        assert_eq!(rendered, "select * from t where name='abc'");
    }

    #[test]
    fn renders_null_and_number() {
        let sql = "select * from t where a={{a}} and b={{b}}";
        let mut m = Map::new();
        m.insert("a".to_string(), Value::Null);
        m.insert("b".to_string(), json!(123));
        let rendered = render_template(sql, &m);
        assert_eq!(rendered, "select * from t where a=NULL and b=123");
    }
}
