use crate::knowledge_base::Knowledge;
use crate::schema::{SchemaResponse, TableWithDetails};
use serde_json::{json, Value};
use std::collections::HashSet;

const DEFAULT_SELECTED_TABLES: usize = 6;
const MAX_SELECTED_TABLES: usize = 8;
const MAX_COLUMNS_PER_TABLE: usize = 12;
const MAX_INDEXES_PER_TABLE: usize = 6;
const MAX_FOREIGN_KEYS_PER_TABLE: usize = 6;
const MAX_KNOWLEDGE_ITEMS: usize = 5;
const MAX_HISTORY_ITEMS: usize = 6;
const MAX_TEXT_CHARS: usize = 240;
const MAX_AVAILABLE_TABLES: usize = 128;
const MAX_AMBIGUOUS_TABLES: usize = 3;
const STRONG_TABLE_SIGNAL_SCORE: usize = 24;
const AMBIGUITY_SCORE_GAP: usize = 8;

struct TableSelection<'a> {
    tables: Vec<&'a TableWithDetails>,
    signal_strength: &'static str,
    selection_warning: Option<String>,
    ambiguous_candidates: Vec<String>,
}

pub fn build_sql_generation_system_prompt(
    dialect: &str,
    user_request: &str,
    schema: Option<&SchemaResponse>,
    knowledge: &[Knowledge],
    chat_history: Option<&[serde_json::Value]>,
    current_sql: Option<&str>,
    db_error: Option<&str>,
    extra_guidance: Option<&str>,
) -> String {
    let context = build_context_json(
        dialect,
        user_request,
        schema,
        knowledge,
        chat_history,
        current_sql,
        db_error,
    );

    let mut prompt = format!(
        "You are a careful {} SQL assistant.\n\n\
<workflow>\n\
1. Determine whether the user is asking to generate_sql, explain_sql, optimize_sql, or fix_sql.\n\
2. Before writing SQL, resolve the request into target entities, required output columns, filters, time window, grouping, ordering, and whether the task is read-only or mutating.\n\
3. Use only the schema, relationships, knowledge, history, current_sql, and db_error inside <context_json>.\n\
4. Ground every table, column, and join in <context_json>. Prefer joins backed by relationships or foreign_keys, and never invent join keys from naming similarity alone.\n\
5. Prefer the smallest correct set of tables, joins, filters, and selected columns.\n\
6. If information is missing or ambiguous, keep assumptions minimal, record them in assumptions, and keep sql empty when a safe grounded query cannot be produced.\n\
7. For explain_sql, optimize_sql, and fix_sql, preserve the business intent of current_sql unless the user request or db_error requires a targeted change.\n\
8. For UPDATE, DELETE, INSERT, DDL, TRUNCATE, DROP, and ALTER, mark risk_level as high and needs_confirmation as true.\n\
</workflow>\n\n\
<constraints>\n\
- Never invent tables, columns, indexes, or join keys that are absent from <context_json>.\n\
- Keep SQL executable and valid for {}.\n\
- Treat available_tables as discovery-only names. Use relevant_schema for column-level grounding and relationships for trusted join evidence.\n\
- When the request implies latest, recent, top, count, distinct, active, status, trend, or a time range, encode those semantics explicitly in SQL or explain what is missing.\n\
- When optimizing SQL, preserve result semantics first, then reduce scan/sort/join cost.\n\
- For explain_sql, sql may be empty or may echo current_sql when no rewrite is needed.\n\
- For generate_sql, optimize_sql, and fix_sql, sql must always be present. Use an empty string only when a safe grounded query cannot be produced.\n\
- explanation must always be present as 1 to 3 short sentences that explain what you did or what blocked generation.\n\
- If schema context is incomplete, generate the safest possible SQL or return an empty sql string, and explain what is missing.\n\
- referenced_tables must match the tables actually used in sql.\n\
- If sql is empty, referenced_tables must be [], sql_empty_reason must be non-empty, and missing_information must list the blockers.\n\
- grounding_evidence must cite the exact tables, columns, relationships, current_sql fragments, or db_error clues used for grounding.\n\
- task_type must match your resolved intent from the workflow.\n\
- Return JSON only. Do not use markdown fences or extra prose outside the JSON object.\n\
</constraints>\n\n",
        dialect, dialect
    );

    if let Some(guidance) = extra_guidance.filter(|item| !item.trim().is_empty()) {
        prompt.push_str("<extra_guidance>\n");
        prompt.push_str(guidance.trim());
        prompt.push_str("\n</extra_guidance>\n\n");
    }

    prompt.push_str(
        "<output_schema>\n\
{\n\
  \"task_type\": \"generate_sql|explain_sql|optimize_sql|fix_sql\",\n\
  \"sql\": \"string\",\n\
  \"explanation\": \"string\",\n\
  \"sql_empty_reason\": \"string // empty unless sql is empty; use missing_schema|ambiguous_request|insufficient_context|unsafe_mutation|db_error_unresolved\",\n\
  \"missing_information\": [\"string // only the missing schema or request details that block safe SQL generation\"],\n\
  \"grounding_evidence\": [\"string // exact table.column names, relationships, current_sql clues, or db_error clues used for grounding\"],\n\
  \"assumptions\": [\"string // include ambiguity, missing context, or conservative fallback notes\"],\n\
  \"referenced_tables\": [\"string // must match tables actually referenced in sql\"],\n\
  \"risk_level\": \"low|medium|high\",\n\
  \"needs_confirmation\": false\n\
}\n\
</output_schema>\n\n\
<context_json>\n",
    );
    prompt.push_str(&serde_json::to_string_pretty(&context).unwrap_or_else(|_| "{}".to_string()));
    prompt.push_str("\n</context_json>");
    prompt
}

pub fn build_user_request_message(user_request: &str) -> String {
    format!("<user_request>\n{}\n</user_request>", user_request.trim())
}

fn build_context_json(
    dialect: &str,
    user_request: &str,
    schema: Option<&SchemaResponse>,
    knowledge: &[Knowledge],
    chat_history: Option<&[serde_json::Value]>,
    current_sql: Option<&str>,
    db_error: Option<&str>,
) -> Value {
    let table_selection = schema
        .map(|schema| select_relevant_table_selection(schema, user_request, knowledge, chat_history));

    let relevant_tables = table_selection
        .as_ref()
        .map(|selection| selection.tables.clone())
        .unwrap_or_default();

    let relevant_table_names: HashSet<&str> = relevant_tables
        .iter()
        .map(|table| table.table_name.as_str())
        .collect();

    let relationships = relevant_tables
        .iter()
        .flat_map(|table| {
            table.foreign_keys.iter().filter_map(|fk| {
                if relevant_table_names.contains(fk.referenced_table_name.as_str()) {
                    Some(json!({
                        "from_table": table.table_name,
                        "from_column": fk.column_name,
                        "to_table": fk.referenced_table_name,
                        "to_column": fk.referenced_column_name,
                    }))
                } else {
                    None
                }
            })
        })
        .collect::<Vec<_>>();

    json!({
        "dialect": dialect,
        "db_name": schema.map(|item| item.db_name.clone()).unwrap_or_default(),
        "available_tables": schema.map(|item| item.tables.iter().take(MAX_AVAILABLE_TABLES).map(|table| table.table_name.clone()).collect::<Vec<_>>()).unwrap_or_default(),
        "available_table_count": schema.map(|item| item.tables.len()).unwrap_or_default(),
        "relevant_table_count": relevant_tables.len(),
        "table_signal_strength": table_selection.as_ref().map(|selection| selection.signal_strength).unwrap_or("unknown"),
        "selection_warning": table_selection.as_ref().and_then(|selection| selection.selection_warning.clone()),
        "ambiguous_table_candidates": table_selection.as_ref().map(|selection| selection.ambiguous_candidates.clone()).unwrap_or_default(),
        "schema_scope_note": "available_tables are discovery candidates only. relevant_schema contains the column-level schema details included in this prompt. relationships only lists foreign-key edges between relevant tables.",
        "table_selection_note": "relevant_schema tables were selected from lexical matches in the request, retrieved knowledge, and recent history, then expanded with foreign-key neighbors when available.",
        "relevant_schema": relevant_tables.iter().map(|table| summarize_table(table)).collect::<Vec<_>>(),
        "relationships": relationships,
        "retrieved_knowledge": knowledge.iter().take(MAX_KNOWLEDGE_ITEMS).map(|item| {
            json!({
                "type": format!("{:?}", item.knowledge_type),
                "title": trim_text(&item.title, 80),
                "description": item.description.as_ref().map(|value| trim_text(value, MAX_TEXT_CHARS)),
                "content": trim_text(&item.content, MAX_TEXT_CHARS),
                "is_golden": item.is_golden,
            })
        }).collect::<Vec<_>>(),
        "history_summary": chat_history.unwrap_or(&[]).iter().rev().take(MAX_HISTORY_ITEMS).collect::<Vec<_>>().into_iter().rev().filter_map(|msg| {
            let role = msg.get("role").and_then(|value| value.as_str())?;
            let content = msg.get("content").and_then(|value| value.as_str())?;
            Some(json!({
                "role": role,
                "content": trim_text(content, MAX_TEXT_CHARS),
            }))
        }).collect::<Vec<_>>(),
        "current_sql": current_sql.map(|value| trim_text(value, 1000)),
        "db_error": db_error.map(|value| trim_text(value, 600)),
    })
}

fn summarize_table(table: &TableWithDetails) -> Value {
    json!({
        "table_name": table.table_name,
        "columns": table.columns.iter().take(MAX_COLUMNS_PER_TABLE).map(|column| {
            json!({
                "name": column.column_name,
                "type": column.column_type,
                "nullable": column.is_nullable == "YES",
                "key": column.column_key,
                "comment": column.column_comment.as_ref().map(|value| trim_text(value, 80)),
            })
        }).collect::<Vec<_>>(),
        "indexes": table.indexes.iter().take(MAX_INDEXES_PER_TABLE).map(|index| {
            json!({
                "name": index.index_name,
                "column": index.column_name,
                "non_unique": index.non_unique,
            })
        }).collect::<Vec<_>>(),
        "foreign_keys": table.foreign_keys.iter().take(MAX_FOREIGN_KEYS_PER_TABLE).map(|fk| {
            json!({
                "column": fk.column_name,
                "referenced_table": fk.referenced_table_name,
                "referenced_column": fk.referenced_column_name,
            })
        }).collect::<Vec<_>>(),
    })
}

fn select_relevant_table_selection<'a>(
    schema: &'a SchemaResponse,
    user_request: &str,
    knowledge: &[Knowledge],
    chat_history: Option<&[serde_json::Value]>,
) -> TableSelection<'a> {
    let mut context_tokens = tokenize(user_request);
    for item in knowledge.iter().take(MAX_KNOWLEDGE_ITEMS) {
        context_tokens.extend(tokenize(&item.title));
        context_tokens.extend(tokenize(&item.content));
        if let Some(description) = &item.description {
            context_tokens.extend(tokenize(description));
        }
    }
    for msg in chat_history.unwrap_or(&[]).iter().rev().take(MAX_HISTORY_ITEMS) {
        if let Some(content) = msg.get("content").and_then(|value| value.as_str()) {
            context_tokens.extend(tokenize(content));
        }
    }

    let token_set = context_tokens.into_iter().collect::<HashSet<_>>();
    let mut scored = schema
        .tables
        .iter()
        .enumerate()
        .map(|(index, table)| {
            let mut score = 0usize;
            score += identifier_match_score(&table.table_name, &token_set, 100, 48, 0);
            for column in &table.columns {
                score += identifier_match_score(&column.column_name, &token_set, 12, 8, 4);
            }
            for fk in &table.foreign_keys {
                score += identifier_match_score(&fk.referenced_table_name, &token_set, 18, 10, 0);
                score += identifier_match_score(&fk.column_name, &token_set, 6, 4, 2);
            }
            (index, score, table)
        })
        .collect::<Vec<_>>();

    scored.sort_by(|(left_index, left_score, _), (right_index, right_score, _)| {
        right_score
            .cmp(left_score)
            .then_with(|| left_index.cmp(right_index))
    });

    let mut selected = Vec::new();
    let mut selected_names = HashSet::new();
    let top_score = scored.first().map(|(_, score, _)| *score).unwrap_or(0);
    let top_has_signal = top_score > 0;
    let seed_limit = if top_has_signal {
        MAX_SELECTED_TABLES
    } else {
        DEFAULT_SELECTED_TABLES.min(schema.tables.len())
    };

    let mut ambiguous_candidates = scored
        .iter()
        .filter(|(_, score, _)| *score > 0 && score.saturating_add(AMBIGUITY_SCORE_GAP) >= top_score)
        .take(MAX_AMBIGUOUS_TABLES)
        .map(|(_, _, table)| table.table_name.clone())
        .collect::<Vec<_>>();
    if ambiguous_candidates.len() <= 1 {
        ambiguous_candidates.clear();
    }

    for (_, _, table) in scored.iter().take(seed_limit) {
        if selected_names.insert(table.table_name.as_str()) {
            selected.push(*table);
        }
    }

    let mut related_names = Vec::new();
    for table in &selected {
        for fk in &table.foreign_keys {
            if selected_names.len() >= MAX_SELECTED_TABLES {
                break;
            }
            if !selected_names.contains(fk.referenced_table_name.as_str()) {
                related_names.push(fk.referenced_table_name.clone());
            }
        }
    }

    for table_name in related_names {
        if selected_names.len() >= MAX_SELECTED_TABLES {
            break;
        }
        if let Some(table) = schema.tables.iter().find(|item| item.table_name == table_name) {
            if selected_names.insert(table.table_name.as_str()) {
                selected.push(table);
            }
        }
    }

    let signal_strength = if top_score == 0 {
        "none"
    } else if top_score < STRONG_TABLE_SIGNAL_SCORE {
        "weak"
    } else {
        "strong"
    };

    let selection_warning = match signal_strength {
        "none" => Some(
            "No strong lexical table-grounding signal was found in the request, knowledge, or recent history. relevant_schema is fallback context only.".to_string(),
        ),
        "weak" => Some(
            "Table grounding is weak. Verify table names, join paths, and business filters conservatively before generating SQL.".to_string(),
        ),
        _ if !ambiguous_candidates.is_empty() => Some(format!(
            "Multiple tables have similarly strong lexical grounding signals: {}. Avoid overcommitting to one candidate without matching columns or relationships.",
            ambiguous_candidates.join(", ")
        )),
        _ => None,
    };

    TableSelection {
        tables: selected,
        signal_strength,
        selection_warning,
        ambiguous_candidates,
    }
}

fn trim_text(input: &str, max_chars: usize) -> String {
    let trimmed = input.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }
    let mut out = trimmed.chars().take(max_chars).collect::<String>();
    out.push_str("...");
    out
}

fn tokenize(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let parts = input
        .split(|ch: char| !(ch.is_alphanumeric() || ch == '_'))
        .filter(|part| !part.is_empty())
        .map(|part| part.to_lowercase())
        .collect::<Vec<_>>();

    for part in &parts {
        push_token_variant(&mut tokens, part);
        for segment in part.split('_').filter(|segment| !segment.is_empty()) {
            if segment != part {
                push_token_variant(&mut tokens, segment);
            }
        }
    }

    for window in parts.windows(2) {
        let combined = format!("{}_{}", window[0], window[1]);
        push_token_variant(&mut tokens, &combined);
    }

    tokens
}

fn identifier_match_score(
    identifier: &str,
    token_set: &HashSet<String>,
    exact_score: usize,
    alias_score: usize,
    split_score: usize,
) -> usize {
    let normalized = identifier.to_lowercase();
    if token_set.contains(&normalized) {
        return exact_score;
    }

    let alias_tokens = identifier_alias_tokens(&normalized);
    if alias_tokens.is_empty() {
        return 0;
    }

    let alias_hits = alias_tokens
        .iter()
        .filter(|token| token_set.contains(*token))
        .count();

    if normalized.contains('_') && alias_hits >= 2 {
        split_score.max(alias_score)
    } else if alias_hits > 0 {
        alias_score
    } else {
        0
    }
}

fn identifier_alias_tokens(identifier: &str) -> Vec<String> {
    let mut aliases = Vec::new();
    if let Some(singular) = singularize_token(identifier) {
        aliases.push(singular);
    }
    for segment in identifier.split('_').filter(|segment| !segment.is_empty()) {
        if segment.len() <= 1 {
            continue;
        }
        aliases.push(segment.to_string());
        if let Some(singular) = singularize_token(segment) {
            aliases.push(singular);
        }
        if let Some(stripped) = segment.strip_suffix("_id") {
            if !stripped.is_empty() {
                aliases.push(stripped.to_string());
            }
        }
    }
    aliases.sort();
    aliases.dedup();
    aliases
}

fn push_token_variant(tokens: &mut Vec<String>, token: &str) {
    if token.is_empty() {
        return;
    }
    tokens.push(token.to_string());
    if let Some(singular) = singularize_token(token) {
        tokens.push(singular);
    }
}

fn singularize_token(token: &str) -> Option<String> {
    if token.len() <= 3 {
        return None;
    }
    if let Some(stripped) = token.strip_suffix("ies") {
        return Some(format!("{}y", stripped));
    }
    if token.ends_with("ses") || token.ends_with("xes") {
        return token.strip_suffix("es").map(|value| value.to_string());
    }
    if token.ends_with('s') && !token.ends_with("ss") {
        return token.strip_suffix('s').map(|value| value.to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{ColumnInfo, SchemaResponse, TableWithDetails};

    fn sample_schema() -> SchemaResponse {
        SchemaResponse {
            db_name: "demo".to_string(),
            tables: vec![
                TableWithDetails {
                    table_name: "users".to_string(),
                    columns: vec![
                        ColumnInfo {
                            column_name: "id".to_string(),
                            data_type: "bigint".to_string(),
                            column_type: "bigint".to_string(),
                            is_nullable: "NO".to_string(),
                            column_comment: Some("primary key".to_string()),
                            column_key: "PRI".to_string(),
                            column_default: None,
                            extra: String::new(),
                        },
                        ColumnInfo {
                            column_name: "email".to_string(),
                            data_type: "varchar".to_string(),
                            column_type: "varchar(255)".to_string(),
                            is_nullable: "NO".to_string(),
                            column_comment: None,
                            column_key: String::new(),
                            column_default: None,
                            extra: String::new(),
                        },
                    ],
                    indexes: vec![],
                    foreign_keys: vec![],
                },
                TableWithDetails {
                    table_name: "orders".to_string(),
                    columns: vec![ColumnInfo {
                        column_name: "user_id".to_string(),
                        data_type: "bigint".to_string(),
                        column_type: "bigint".to_string(),
                        is_nullable: "NO".to_string(),
                        column_comment: None,
                        column_key: "MUL".to_string(),
                        column_default: None,
                        extra: String::new(),
                    }],
                    indexes: vec![],
                    foreign_keys: vec![],
                },
            ],
            views: vec![],
        }
    }

    #[test]
    fn selects_table_mentioned_in_request() {
        let schema = sample_schema();
        let selection = select_relevant_table_selection(&schema, "find users by email", &[], None);
        assert_eq!(selection.tables.first().map(|table| table.table_name.as_str()), Some("users"));
    }

    #[test]
    fn selects_plural_table_from_singular_request() {
        let schema = sample_schema();
        let selection = select_relevant_table_selection(&schema, "find user records", &[], None);
        assert_eq!(selection.tables.first().map(|table| table.table_name.as_str()), Some("users"));
    }

    #[test]
    fn table_selection_marks_weak_signal_for_unknown_request() {
        let schema = sample_schema();
        let selection = select_relevant_table_selection(&schema, "profitability dashboard", &[], None);
        assert_eq!(selection.signal_strength, "none");
        assert!(selection.selection_warning.unwrap_or_default().contains("fallback"));
    }

    #[test]
    fn table_selection_marks_ambiguity_for_close_candidates() {
        let schema = SchemaResponse {
            db_name: "demo".to_string(),
            tables: vec![
                TableWithDetails {
                    table_name: "user_profiles".to_string(),
                    columns: vec![],
                    indexes: vec![],
                    foreign_keys: vec![],
                },
                TableWithDetails {
                    table_name: "user_sessions".to_string(),
                    columns: vec![],
                    indexes: vec![],
                    foreign_keys: vec![],
                },
            ],
            views: vec![],
        };
        let selection = select_relevant_table_selection(&schema, "show user activity", &[], None);
        assert!(selection.ambiguous_candidates.len() >= 2);
    }

    #[test]
    fn system_prompt_contains_context_and_schema() {
        let schema = sample_schema();
        let prompt = build_sql_generation_system_prompt(
            "MySQL",
            "find users by email",
            Some(&schema),
            &[],
            None,
            None,
            None,
            None,
        );
        assert!(prompt.contains("<context_json>"));
        assert!(prompt.contains("\"relevant_schema\""));
        assert!(prompt.contains("\"schema_scope_note\""));
        assert!(prompt.contains("\"table_signal_strength\""));
        assert!(prompt.contains("\"task_type\""));
        assert!(prompt.contains("\"sql_empty_reason\""));
        assert!(prompt.contains("\"missing_information\""));
        assert!(prompt.contains("\"grounding_evidence\""));
        assert!(prompt.contains("\"users\""));
    }
}
