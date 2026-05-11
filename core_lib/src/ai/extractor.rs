use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StructuredSqlIntent {
    pub sql: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    pub explanation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sql_empty_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_information: Vec<String>,
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
                "AI response was empty; no SQL could be extracted. Check API key, relay, rate limits, or tier/max_tokens and retry."
                    .to_string(),
            ),
            task_type: None,
            sql_empty_reason: None,
            missing_information: Vec::new(),
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
    let accepted_sql = if looks_like_sql_or_command(&sql_str) {
        sql_str
    } else {
        String::new()
    };
    StructuredSqlIntent {
        sql: accepted_sql.clone(),
        command: None,
        explanation: Some(diagnose_fallback(text, !accepted_sql.is_empty())),
        task_type: None,
        sql_empty_reason: None,
        missing_information: Vec::new(),
    }
}

fn parse_intent_json(text: &str) -> Option<StructuredSqlIntent> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let sql = v.get("sql").and_then(|x| x.as_str()).unwrap_or_default();
    let command = v
        .get("command")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());
    let explanation = v
        .get("explanation")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());
    let task_type = v
        .get("task_type")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());
    let sql_empty_reason = v
        .get("sql_empty_reason")
        .and_then(|x| x.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.to_string());
    let missing_information = v
        .get("missing_information")
        .and_then(|x| x.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(|s| s.to_string()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

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
        task_type,
        sql_empty_reason,
        missing_information,
    })
}

fn looks_like_sql_or_command(candidate: &str) -> bool {
    let token = candidate
        .trim_start()
        .trim_start_matches('(')
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .to_ascii_uppercase();

    matches!(
        token.as_str(),
        "SELECT"
            | "WITH"
            | "INSERT"
            | "UPDATE"
            | "DELETE"
            | "SHOW"
            | "DESCRIBE"
            | "DESC"
            | "EXPLAIN"
            | "CREATE"
            | "ALTER"
            | "DROP"
            | "TRUNCATE"
            | "REPLACE"
            | "CALL"
            | "USE"
            | "SET"
            | "GRANT"
            | "REVOKE"
            | "BEGIN"
            | "COMMIT"
            | "ROLLBACK"
            | "GET"
            | "SETEX"
            | "DEL"
            | "KEYS"
            | "SCAN"
            | "HGET"
            | "HGETALL"
            | "HSET"
            | "LRANGE"
            | "SMEMBERS"
            | "ZRANGE"
    )
}

fn diagnose_fallback(raw: &str, accepted_sql: bool) -> String {
    let preview = raw.chars().take(160).collect::<String>();
    if !accepted_sql {
        format!(
            "AI returned non-JSON content that did not resemble SQL or a supported command. Response preview: {}. Suggestion: tighten the prompt or retry with JSON mode.",
            preview
        )
    } else if raw.contains("```json") && !raw.contains("```") {
        format!(
            "AI returned a truncated JSON block; falling back to SQL extraction. Response preview: {}. Suggestion: reduce max_tokens or retry.",
            preview
        )
    } else {
        format!(
            "AI returned non-JSON content; falling back to SQL extraction. Response preview: {}. Suggestion: switch provider/model or retry without JSON mode.",
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
        assert!(intent.explanation.unwrap_or_default().contains("empty"));
        assert_eq!(intent.task_type, None);
        assert_eq!(intent.sql_empty_reason, None);
        assert!(intent.missing_information.is_empty());
    }

    #[test]
    fn extract_sql_intent_handles_non_json_and_provides_diagnostic() {
        let intent = extract_sql_intent("Here is the query:\n```sql\nSELECT 1;\n```");
        assert_eq!(intent.sql, "SELECT 1;");
        assert!(intent.explanation.unwrap_or_default().contains("falling back"));
    }

    #[test]
    fn extract_sql_intent_rejects_non_sql_chatter_fallback() {
        let intent = extract_sql_intent("Here is a detailed explanation of the query and why it works.");
        assert!(intent.sql.is_empty());
        assert!(intent
            .explanation
            .unwrap_or_default()
            .contains("did not resemble SQL"));
    }

    #[test]
    fn extract_sql_intent_accepts_command_field() {
        let intent = extract_sql_intent("{\"command\":\"GET k\",\"explanation\":\"x\"}");
        assert_eq!(intent.sql, "GET k");
        assert_eq!(intent.explanation.as_deref(), Some("x"));
    }

    #[test]
    fn extract_sql_intent_ignores_additional_prompt_fields() {
        let intent = extract_sql_intent(
            "{\"task_type\":\"generate_sql\",\"sql\":\"SELECT 1;\",\"explanation\":\"ok\",\"sql_empty_reason\":\"\",\"missing_information\":[],\"grounding_evidence\":[\"dual\"]}",
        );
        assert_eq!(intent.sql, "SELECT 1;");
        assert_eq!(intent.explanation.as_deref(), Some("ok"));
        assert_eq!(intent.task_type.as_deref(), Some("generate_sql"));
        assert_eq!(intent.sql_empty_reason, None);
        assert!(intent.missing_information.is_empty());
    }

    #[test]
    fn extract_sql_intent_parses_empty_sql_metadata() {
        let intent = extract_sql_intent(
            "{\"task_type\":\"ask_clarification\",\"sql\":\"\",\"explanation\":\"Need date range\",\"sql_empty_reason\":\"missing_filter\",\"missing_information\":[\"date_range\",\"metric\"]}",
        );
        assert!(intent.sql.is_empty());
        assert_eq!(intent.explanation.as_deref(), Some("Need date range"));
        assert_eq!(intent.task_type.as_deref(), Some("ask_clarification"));
        assert_eq!(intent.sql_empty_reason.as_deref(), Some("missing_filter"));
        assert_eq!(intent.missing_information, vec!["date_range", "metric"]);
    }
}
