use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StructuredSqlIntent {
    pub sql: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    pub explanation: Option<String>,
}

/// Pipeline extractor for SQL intent.
/// Level 1: Try direct JSON parsing
/// Level 2: Try JSON parsing after extracting markdown code block
/// Level 3: Fallback to raw SQL extraction
pub fn extract_sql_intent(response_text: &str) -> StructuredSqlIntent {
    let text = response_text.trim();

    if text.is_empty() {
        return StructuredSqlIntent {
            sql: String::new(),
            command: None,
            explanation: Some(
                "AI 返回为空或被截断，无法解析 SQL。建议：检查 API Key/代理/限流，或降低 tier/max_tokens 后重试。"
                    .to_string(),
            ),
        };
    }

    // Level 1: Direct JSON parsing
    if let Some(intent) = parse_intent_json(text) {
        return intent;
    }

    // Level 2: Extract JSON from markdown
    let json_str = extract_code_block(text, "json");
    if let Some(intent) = parse_intent_json(&json_str) {
        return intent;
    }

    // Level 3: Fallback raw SQL extraction
    let sql_str = extract_code_block(text, "sql");
    StructuredSqlIntent {
        sql: sql_str,
        command: None,
        explanation: Some(diagnose_fallback(text)),
    }
}

fn parse_intent_json(text: &str) -> Option<StructuredSqlIntent> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let sql = v.get("sql").and_then(|x| x.as_str()).unwrap_or_default();
    let command = v.get("command").and_then(|x| x.as_str()).map(|s| s.to_string());
    let explanation = v
        .get("explanation")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());

    let mut sql_final = sql.to_string();
    if sql_final.trim().is_empty() {
        if let Some(cmd) = &command {
            sql_final = cmd.clone();
        }
    }

    if sql_final.trim().is_empty() && explanation.is_none() && command.is_none() {
        return None;
    }

    Some(StructuredSqlIntent {
        sql: sql_final,
        command,
        explanation,
    })
}

fn diagnose_fallback(raw: &str) -> String {
    let preview = raw.chars().take(160).collect::<String>();
    if raw.contains("```json") && !raw.contains("```") {
        format!(
            "AI 返回 JSON 可能被截断，已尝试回退提取 SQL。返回预览：{}。建议：降低 max_tokens 或重试。",
            preview
        )
    } else {
        format!(
            "AI 返回非 JSON，已尝试回退提取 SQL。返回预览：{}。建议：切换 provider/model 或关闭 JSON 模式重试。",
            preview
        )
    }
}

pub fn extract_code_block(text: &str, lang_tag: &str) -> String {
    let start_tag = format!("```{}", lang_tag);

    // 1. Try to find specific language block like ```sql
    if let Some(start_idx) = text.find(&start_tag) {
        let content_after = &text[start_idx + start_tag.len()..];
        if let Some(end_idx) = content_after.find("```") {
            return content_after[..end_idx].trim().to_string();
        } else {
            // Unclosed code block, return the rest
            return content_after.trim().to_string();
        }
    }

    // 2. Try to find generic code block ```
    if let Some(start_idx) = text.find("```") {
        let content_after = &text[start_idx + 3..];
        if let Some(end_idx) = content_after.find("```") {
            return content_after[..end_idx].trim().to_string();
        } else {
            // Unclosed code block, return the rest
            return content_after.trim().to_string();
        }
    }

    // 3. Fallback: if no markdown block is found, assume the whole text is the code,
    // but try to strip common conversational prefixes
    let mut cleaned = text.trim();
    if let Some(idx) = cleaned.find(":\n") {
        // e.g. "Here is the SQL:\nSELECT * FROM users;"
        if idx < 100 {
            // Only strip if it's a short prefix
            cleaned = cleaned[idx + 2..].trim();
        }
    }

    cleaned.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_clean_sql() {
        let text = "SELECT * FROM users;";
        assert_eq!(extract_code_block(text, "sql"), "SELECT * FROM users;");
    }

    #[test]
    fn test_extract_markdown_sql() {
        let text = "```sql\nSELECT * FROM users;\n```";
        assert_eq!(extract_code_block(text, "sql"), "SELECT * FROM users;");
    }

    #[test]
    fn test_extract_conversational_sql() {
        let text = "Here is your query:\n```sql\nSELECT * FROM orders WHERE id = 1;\n```\nHope this helps!";
        assert_eq!(
            extract_code_block(text, "sql"),
            "SELECT * FROM orders WHERE id = 1;"
        );
    }

    #[test]
    fn test_extract_generic_markdown() {
        let text = "Sure!\n```\nSELECT * FROM users;\n```\nDone.";
        assert_eq!(extract_code_block(text, "sql"), "SELECT * FROM users;");
    }

    #[test]
    fn test_extract_json_with_garbage() {
        let text = "I extracted the parameters for you:\n```json\n{\"id\": 1}\n```\nLet me know if you need more.";
        assert_eq!(extract_code_block(text, "json"), "{\"id\": 1}");
    }

    #[test]
    fn test_extract_no_markdown_with_prefix() {
        let text = "The SQL is:\nSELECT * FROM table;";
        assert_eq!(extract_code_block(text, "sql"), "SELECT * FROM table;");
    }

    #[test]
    fn extract_sql_intent_handles_empty() {
        let intent = extract_sql_intent("   ");
        assert!(intent.sql.is_empty());
        assert!(intent.explanation.unwrap_or_default().contains("为空"));
    }

    #[test]
    fn extract_sql_intent_handles_non_json_and_provides_diagnostic() {
        let intent = extract_sql_intent("Here is the query:\n```sql\nSELECT 1;\n```");
        assert_eq!(intent.sql, "SELECT 1;");
        assert!(intent.explanation.unwrap_or_default().contains("回退"));
    }

    #[test]
    fn extract_sql_intent_accepts_command_field() {
        let intent = extract_sql_intent("{\"command\":\"GET k\",\"explanation\":\"x\"}");
        assert_eq!(intent.sql, "GET k");
        assert_eq!(intent.explanation.as_deref(), Some("x"));
    }
}
