use crate::knowledge_base::KnowledgeBase;
use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::Deserialize;
use serde_json::json;

#[derive(Debug, thiserror::Error)]
#[error("Knowledge query error: {0}")]
pub struct KnowledgeToolError(pub String);

#[derive(Deserialize)]
pub struct QueryKnowledgeArgs {
    pub query: String,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_limit() -> usize {
    5
}

#[derive(Clone)]
pub struct QueryKnowledgeTool {
    knowledge_base: KnowledgeBase,
    db_connection_id: Option<String>,
}

impl QueryKnowledgeTool {
    pub fn new(knowledge_base: KnowledgeBase, db_connection_id: Option<String>) -> Self {
        Self {
            knowledge_base,
            db_connection_id,
        }
    }
}

impl Tool for QueryKnowledgeTool {
    const NAME: &'static str = "query_knowledge";

    type Error = KnowledgeToolError;
    type Args = QueryKnowledgeArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "query_knowledge".to_string(),
            description: "Search the knowledge base for business context, field descriptions, \
                data definitions, and example queries. Use this to understand business terminology \
                and data semantics before writing SQL."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search keywords to find relevant knowledge entries."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of results to return (default: 5).",
                        "default": 5
                    }
                },
                "required": ["query"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let items = self.knowledge_base.retrieve(
            self.db_connection_id.as_deref(),
            &args.query,
            args.limit,
        );

        if items.is_empty() {
            return Ok("No matching knowledge entries found.".to_string());
        }

        let results: Vec<serde_json::Value> = items
            .iter()
            .map(|item| {
                json!({
                    "type": format!("{:?}", item.knowledge_type),
                    "title": item.title,
                    "content": item.content,
                    "description": item.description,
                })
            })
            .collect();

        let output = json!({
            "count": results.len(),
            "entries": results,
        });

        serde_json::to_string_pretty(&output)
            .map_err(|e| KnowledgeToolError(e.to_string()))
    }
}
