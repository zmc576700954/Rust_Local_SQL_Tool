use crate::ai::prompting::select_relevant_table_selection;
use crate::knowledge_base::Knowledge;
use crate::schema::SchemaResponse;

/// A lightweight schema briefing injected into the Agent preamble,
/// giving the model a global view of the database before it starts
/// calling tools.
pub struct SchemaBriefing {
    /// Formatted text summary for preamble injection
    pub summary_text: String,
    /// Names of tables selected as relevant to the user request
    pub relevant_table_names: Vec<String>,
}

impl SchemaBriefing {
    /// Build a schema briefing from the full schema response and the user's request.
    ///
    /// Uses the lexical table selection algorithm from `prompting.rs` to identify
    /// the most relevant tables, then formats a concise text summary including
    /// table names, key columns, and FK relationships.
    pub fn build(
        schema: &SchemaResponse,
        user_request: &str,
        knowledge: &[Knowledge],
    ) -> Self {
        let selection = select_relevant_table_selection(
            schema,
            user_request,
            knowledge,
            None, // no chat history for briefing
        );

        let relevant_table_names: Vec<String> = selection
            .tables
            .iter()
            .map(|t| t.table_name.clone())
            .collect();

        let all_table_names: Vec<String> = schema
            .tables
            .iter()
            .map(|t| t.table_name.clone())
            .collect();

        let mut summary = format!(
            "Database: {} ({} tables)\n",
            schema.db_name,
            all_table_names.len()
        );

        // Relevant tables section — key columns only (max 8 per table)
        if !relevant_table_names.is_empty() {
            summary.push_str("\nRelevant tables (based on request analysis):\n");
            for table in &selection.tables {
                let comment = table
                    .columns
                    .first()
                    .and_then(|c| c.column_comment.as_deref())
                    .unwrap_or("");
                let table_comment_hint = if comment.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", comment)
                };

                // Show up to 8 key column names (PRI keys first, then others)
                let key_columns: Vec<String> = table
                    .columns
                    .iter()
                    .filter(|c| c.column_key == "PRI")
                    .map(|c| c.column_name.clone())
                    .collect();
                let other_columns: Vec<String> = table
                    .columns
                    .iter()
                    .filter(|c| c.column_key != "PRI")
                    .take(8 - key_columns.len())
                    .map(|c| c.column_name.clone())
                    .collect();
                let col_display = [key_columns, other_columns]
                    .concat()
                    .join(", ");

                summary.push_str(&format!(
                    "  - {} ({}){}\n",
                    table.table_name, col_display, table_comment_hint
                ));
            }
        }

        // FK relationships section — show relationships between relevant tables
        let relevant_set: std::collections::HashSet<&str> = relevant_table_names
            .iter()
            .map(|s| s.as_str())
            .collect();

        let mut fk_lines: Vec<String> = Vec::new();
        for table in &selection.tables {
            for fk in &table.foreign_keys {
                // Only show FK if both sides are in the relevant set or it's a cross-reference
                // to a well-known table
                if relevant_set.contains(fk.referenced_table_name.as_str())
                    || relevant_set.contains(table.table_name.as_str())
                {
                    fk_lines.push(format!(
                        "{}.{} → {}.{}",
                        table.table_name,
                        fk.column_name,
                        fk.referenced_table_name,
                        fk.referenced_column_name
                    ));
                }
            }
        }
        if !fk_lines.is_empty() {
            summary.push_str("\nRelationships: ");
            summary.push_str(&fk_lines.join(", "));
            summary.push('\n');
        }

        // Other tables section — list remaining tables for discovery
        let other_tables: Vec<&str> = all_table_names
            .iter()
            .filter(|name| !relevant_set.contains(name.as_str()))
            .take(30) // cap at 30 to avoid overwhelming the preamble
            .map(|s| s.as_str())
            .collect();
        if !other_tables.is_empty() {
            summary.push_str("\nOther tables: ");
            summary.push_str(&other_tables.join(", "));
            if all_table_names.len() > relevant_table_names.len() + other_tables.len() {
                summary.push_str(", ...");
            }
            summary.push('\n');
        }

        // Signal strength warning
        if let Some(ref warning) = selection.selection_warning {
            summary.push_str(&format!("\nNote: {}\n", warning));
        }

        Self {
            summary_text: summary,
            relevant_table_names,
        }
    }

    /// Build an empty briefing when no schema is available.
    pub fn empty() -> Self {
        Self {
            summary_text: "No schema context available. Use query_schema tool to discover tables.\n".to_string(),
            relevant_table_names: Vec::new(),
        }
    }
}