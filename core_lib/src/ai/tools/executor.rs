use crate::db::DbClient;
use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::Deserialize;
use serde_json::json;
use sqlx::Column;
use sqlparser::ast::Statement;
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser;

#[derive(Debug, thiserror::Error)]
#[error("SQL execution error: {0}")]
pub struct ExecutorToolError(pub String);

#[derive(Deserialize)]
pub struct ExecuteSqlArgs {
    pub sql: String,
    #[serde(default = "default_limit")]
    pub limit: u32,
}

fn default_limit() -> u32 {
    20
}

#[derive(Clone)]
pub struct ExecuteSqlTool {
    db_client: DbClient,
}

impl ExecuteSqlTool {
    pub fn new(db_client: DbClient) -> Self {
        Self { db_client }
    }

    /// Validate that a SQL string is read-only and inject LIMIT at AST level.
    /// Returns the safe SQL with LIMIT applied, or an error if the SQL is unsafe.
    fn prepare_safe_sql(sql: &str, limit: u32) -> Result<String, ExecutorToolError> {
        let trimmed = sql.trim().trim_start_matches('(').trim_end_matches(')');
        if trimmed.is_empty() {
            return Err(ExecutorToolError("Empty SQL".to_string()));
        }

        let dialect = GenericDialect {};
        let mut statements = Parser::parse_sql(&dialect, trimmed)
            .map_err(|_| ExecutorToolError("Failed to parse SQL".to_string()))?;

        if statements.is_empty() {
            return Err(ExecutorToolError("No statements parsed".to_string()));
        }

        // Validate all statements are read-only (AST-level, not keyword matching)
        for stmt in &statements {
            let safe = matches!(
                stmt,
                Statement::Query(_)
                    | Statement::Explain { .. }
                    | Statement::ExplainTable { .. }
                    | Statement::ShowVariable { .. }
                    | Statement::ShowCreate { .. }
            );
            if !safe {
                return Err(ExecutorToolError(
                    "Only read-only queries (SELECT, SHOW, DESCRIBE, EXPLAIN, WITH) are allowed."
                        .to_string(),
                ));
            }
        }

        // Inject LIMIT into the first Query statement if missing (AST-level, immune to comments/subqueries)
        if let Some(Statement::Query(query)) = statements.first_mut() {
            if query.limit_clause.is_none() {
                query.limit_clause = Some(sqlparser::ast::LimitClause::LimitOffset {
                    limit: Some(sqlparser::ast::Expr::Value(
                        sqlparser::ast::Value::Number(limit.to_string(), false).into(),
                    )),
                    offset: None,
                    limit_by: vec![],
                });
            }
        }

        Ok(statements
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .join("; "))
    }
}

impl Tool for ExecuteSqlTool {
    const NAME: &'static str = "execute_sql";

    type Error = ExecutorToolError;
    type Args = ExecuteSqlArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "execute_sql".to_string(),
            description: "Execute a read-only SQL query to validate it works and preview results. \
                Only SELECT, SHOW, DESCRIBE, EXPLAIN, and WITH (CTE) queries are allowed. \
                Use this to verify generated SQL is correct before presenting it to the user."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "sql": {
                        "type": "string",
                        "description": "The SQL query to execute."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of rows to return (default: 20).",
                        "default": 20
                    }
                },
                "required": ["sql"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let limit = args.limit.max(1);
        let sql_with_limit = Self::prepare_safe_sql(&args.sql, limit)?;

        // Each DB pool type is separate due to sqlx's per-DB row types.
        // The logic is identical; extracted into a macro to stay DRY.
        macro_rules! exec_pool {
            ($pool:expr) => {{
                use sqlx::Row;
                let result = sqlx::query(&sql_with_limit)
                    .fetch_all($pool)
                    .await
                    .map_err(|e| ExecutorToolError(e.to_string()))?;

                if result.is_empty() {
                    return Ok("Query executed successfully. No rows returned.".to_string());
                }

                let columns: Vec<String> = result[0]
                    .columns()
                    .iter()
                    .map(|c| c.name().to_string())
                    .collect();

                let rows: Vec<serde_json::Value> = result
                    .iter()
                    .map(|row| {
                        let mut obj = serde_json::Map::new();
                        for (i, col) in columns.iter().enumerate() {
                            let val: serde_json::Value =
                                row.try_get::<serde_json::Value, _>(i)
                                    .unwrap_or(serde_json::Value::Null);
                            obj.insert(col.clone(), val);
                        }
                        serde_json::Value::Object(obj)
                    })
                    .collect();

                let output = json!({
                    "row_count": rows.len(),
                    "columns": columns,
                    "rows": rows,
                });
                serde_json::to_string_pretty(&output)
                    .map_err(|e| ExecutorToolError(e.to_string()))
            }};
        }

        match self.db_client.pool {
            crate::db::client::DbPool::MySQL(ref pool) => exec_pool!(pool),
            crate::db::client::DbPool::Postgres(ref pool) => exec_pool!(pool),
            crate::db::client::DbPool::SQLite(ref pool) => exec_pool!(pool),
        }
    }
}
