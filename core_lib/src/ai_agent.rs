use crate::ai::gateway::{AiGateway, ChatMessage};
use crate::ai::prompting::{build_sql_generation_system_prompt, build_user_request_message};
use crate::schema::SchemaResponse;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum DbDialect {
    MySQL,
    PostgreSQL,
    Redis,
}

impl DbDialect {
    pub fn from_url(url: &str) -> Self {
        if url.starts_with("postgres://") || url.starts_with("postgresql://") {
            DbDialect::PostgreSQL
        } else if url.starts_with("redis://") {
            DbDialect::Redis
        } else {
            DbDialect::MySQL
        }
    }
}

pub struct AgentConfig {
    pub dialect: DbDialect,
    pub system_prompt: String,
}

impl AgentConfig {
    pub fn new(dialect: DbDialect) -> Self {
        let system_prompt = match dialect {
            DbDialect::MySQL => {
                "You are an expert MySQL AI Assistant. 
Always use backticks (`) for table and column names to avoid reserved keyword conflicts.
Ensure generated SQL is valid for MySQL 8.0+.
Return ONLY valid JSON with 'sql' and 'explanation' fields when asked for SQL, or just provide explanation when asked to explain.".to_string()
            }
            DbDialect::PostgreSQL => {
                "You are an expert PostgreSQL AI Assistant. 
Always use double quotes (\") for table and column names if they contain uppercase letters or are reserved words.
Ensure generated SQL is valid for PostgreSQL 14+.
Return ONLY valid JSON with 'sql' and 'explanation' fields when asked for SQL, or just provide explanation when asked to explain.".to_string()
            }
            DbDialect::Redis => {
                "You are an expert Redis AI Assistant. 
Provide valid Redis CLI commands.
Return ONLY valid JSON with 'command' and 'explanation' fields when asked for queries.".to_string()
            }
        };

        Self {
            dialect,
            system_prompt,
        }
    }

    pub fn format_schema_context(&self, schema: Option<&SchemaResponse>) -> String {
        if let Some(schema) = schema {
            if self.dialect == DbDialect::Redis {
                return "Schema context is not applicable for Redis.".to_string();
            }

            let mut context = format!("Current Database: {}\n\nTables:\n", schema.db_name);
            for table in &schema.tables {
                context.push_str(&format!("- Table: {}\n", table.table_name));
                context.push_str("  Columns:\n");
                for col in &table.columns {
                    context.push_str(&format!(
                        "    - {}: {} ({}){}\n",
                        col.column_name,
                        col.data_type,
                        col.column_type,
                        if col.is_nullable == "YES" {
                            " NULL"
                        } else {
                            " NOT NULL"
                        }
                    ));
                }
                if !table.indexes.is_empty() {
                    context.push_str("  Indexes:\n");
                    for idx in &table.indexes {
                        context
                            .push_str(&format!("    - {}: {}\n", idx.index_name, idx.column_name));
                    }
                }
                if !table.foreign_keys.is_empty() {
                    context.push_str("  Foreign Keys:\n");
                    for fk in &table.foreign_keys {
                        context.push_str(&format!(
                            "    - {} -> {}({})\n",
                            fk.column_name, fk.referenced_table_name, fk.referenced_column_name
                        ));
                    }
                }
                context.push('\n');
            }
            context
        } else {
            "No schema context provided.".to_string()
        }
    }
}

pub struct AiRouter {
    pub gateway: AiGateway,
}

impl AiRouter {
    pub fn new(gateway: AiGateway) -> Self {
        Self { gateway }
    }

    pub async fn dispatch_query(
        &self,
        dialect: DbDialect,
        query: &str,
        schema: Option<&SchemaResponse>,
        knowledge: &[crate::knowledge_base::Knowledge],
        chat_history: Option<Vec<serde_json::Value>>,
    ) -> Result<String, crate::ai::gateway::AiError> {
        let config = AgentConfig::new(dialect);
        let dialect_name = match config.dialect {
            DbDialect::MySQL => "MySQL",
            DbDialect::PostgreSQL => "PostgreSQL",
            DbDialect::Redis => "Redis",
        };
        let schema_for_prompt = if matches!(config.dialect, DbDialect::Redis) {
            None
        } else {
            schema
        };
        let system_message = build_sql_generation_system_prompt(
            dialect_name,
            query,
            schema_for_prompt,
            knowledge,
            chat_history.as_deref(),
            None,
            None,
            Some("Prefer concise SQL. First resolve entities, filters, time range, grouping, ordering, and output columns from the user's request. When the user asks to explain or optimize SQL, keep the returned sql field aligned with the user's current statement or the improved statement."),
        );
        let user_message = build_user_request_message(query);

        let messages = vec![
            ChatMessage {
                role: "system".to_string(),
                content: system_message,
            },
            ChatMessage {
                role: "user".to_string(),
                content: user_message,
            },
        ];

        self.gateway.chat_completion_json(messages).await
    }

    pub async fn explain_error(
        &self,
        dialect: DbDialect,
        error_msg: &str,
        failed_query: &str,
        schema: Option<&SchemaResponse>,
    ) -> Result<String, crate::ai::gateway::AiError> {
        let config = AgentConfig::new(dialect);
        let dialect_name = match config.dialect {
            DbDialect::MySQL => "MySQL",
            DbDialect::PostgreSQL => "PostgreSQL",
            DbDialect::Redis => "Redis",
        };
        let system_message = build_sql_generation_system_prompt(
            dialect_name,
            failed_query,
            schema,
            &[],
            None,
            Some(failed_query),
            Some(error_msg),
            Some("Intent is fix_sql. Preserve the business intent of the failed query, change only the minimal syntax, alias, join, or schema references needed to fix it, explain the failure briefly, and return a corrected query when possible. If a safe correction is impossible, keep sql empty and explain why."),
        );
        let user_message = build_user_request_message(&format!(
            "The following query failed and needs a fix.\nSQL:\n{}\n\nDatabase error:\n{}",
            failed_query, error_msg
        ));

        let messages = vec![
            ChatMessage {
                role: "system".to_string(),
                content: system_message,
            },
            ChatMessage {
                role: "user".to_string(),
                content: user_message,
            },
        ];

        self.gateway.chat_completion_json(messages).await
    }
}
