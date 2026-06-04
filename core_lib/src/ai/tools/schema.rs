use crate::db::DbClient;
use crate::schema::SchemaExtractor;
use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, thiserror::Error)]
#[error("Schema query error: {0}")]
pub struct SchemaToolError(pub String);

#[derive(Deserialize)]
pub struct QuerySchemaArgs {
    pub table_name: Option<String>,
    #[serde(default = "default_true")]
    pub include_columns: bool,
    #[serde(default)]
    pub include_indexes: bool,
    #[serde(default)]
    pub include_foreign_keys: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Serialize)]
pub struct QuerySchemaOutput {
    pub db_name: String,
    pub tables: Vec<serde_json::Value>,
}

#[derive(Clone)]
pub struct QuerySchemaTool {
    db_client: DbClient,
    db_name: String,
}

impl QuerySchemaTool {
    pub fn new(db_client: DbClient, db_name: String) -> Self {
        Self { db_client, db_name }
    }
}

impl Tool for QuerySchemaTool {
    const NAME: &'static str = "query_schema";

    type Error = SchemaToolError;
    type Args = QuerySchemaArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "query_schema".to_string(),
            description: "Query the database schema. Use this to discover tables, columns, indexes, \
                and foreign keys. Call without table_name to list all tables. Call with table_name \
                to get detailed schema for a specific table."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "table_name": {
                        "type": "string",
                        "description": "Optional. Specific table name to query. Omit to list all tables."
                    },
                    "include_columns": {
                        "type": "boolean",
                        "description": "Include column details (default: true).",
                        "default": true
                    },
                    "include_indexes": {
                        "type": "boolean",
                        "description": "Include index details (default: false).",
                        "default": false
                    },
                    "include_foreign_keys": {
                        "type": "boolean",
                        "description": "Include foreign key details (default: false).",
                        "default": false
                    }
                }
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let tables = SchemaExtractor::get_tables(&self.db_client, &self.db_name)
            .await
            .map_err(|e| SchemaToolError(e.to_string()))?;

        if let Some(ref table_name) = args.table_name {
            // Specific table query
            let table_info = tables
                .iter()
                .find(|t| t.table_name.eq_ignore_ascii_case(table_name));
            let Some(info) = table_info else {
                return Ok(format!(
                    "Table '{}' not found. Available tables: {}",
                    table_name,
                    tables
                        .iter()
                        .map(|t| t.table_name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            };

            let mut result = json!({
                "table_name": info.table_name,
                "comment": info.table_comment,
            });

            if args.include_columns {
                let columns = SchemaExtractor::get_columns(
                    &self.db_client,
                    &self.db_name,
                    &info.table_name,
                )
                .await
                .map_err(|e| SchemaToolError(e.to_string()))?;

                result["columns"] = json!(columns
                    .iter()
                    .map(|c| {
                        json!({
                            "name": c.column_name,
                            "type": c.column_type,
                            "nullable": c.is_nullable == "YES",
                            "key": c.column_key,
                            "comment": c.column_comment,
                        })
                    })
                    .collect::<Vec<_>>());
            }

            serde_json::to_string_pretty(&result)
                .map_err(|e| SchemaToolError(e.to_string()))
        } else {
            // List all tables
            let table_names: Vec<&str> = tables.iter().map(|t| t.table_name.as_str()).collect();
            let result = json!({
                "db_name": self.db_name,
                "table_count": table_names.len(),
                "tables": table_names,
            });
            serde_json::to_string_pretty(&result)
                .map_err(|e| SchemaToolError(e.to_string()))
        }
    }
}
