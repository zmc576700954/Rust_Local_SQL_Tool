use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum AgentEvent {
    #[serde(rename = "thinking")]
    Thinking { text: String },
    #[serde(rename = "tool_call")]
    ToolCall { tool: String, args: serde_json::Value },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool: String,
        result: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
    },
    #[serde(rename = "sql_draft")]
    SqlDraft { sql: String },
    #[serde(rename = "final_sql")]
    FinalSql { sql: String, task_type: Option<String> },
    #[serde(rename = "explanation")]
    Explanation { text: String },
    #[serde(rename = "error")]
    Error { message: String },
    #[serde(rename = "done")]
    Done,
}

impl AgentEvent {
    /// SSE event type name for this variant.
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::Thinking { .. } => "thinking",
            Self::ToolCall { .. } => "tool_call",
            Self::ToolResult { .. } => "tool_result",
            Self::SqlDraft { .. } => "sql_draft",
            Self::FinalSql { .. } => "final_sql",
            Self::Explanation { .. } => "explanation",
            Self::Error { .. } => "error",
            Self::Done => "done",
        }
    }

    /// Serialize the event data payload (excluding the "type" discriminator).
    pub fn data_json(&self) -> serde_json::Value {
        match self {
            Self::Thinking { text } => json!({ "text": text }),
            Self::ToolCall { tool, args } => json!({ "tool": tool, "args": args }),
            Self::ToolResult { tool, result, call_id } => json!({ "tool": tool, "result": result, "call_id": call_id }),
            Self::SqlDraft { sql } => json!({ "sql": sql }),
            Self::FinalSql { sql, task_type } => json!({ "sql": sql, "task_type": task_type }),
            Self::Explanation { text } => json!({ "text": text }),
            Self::Error { message } => json!({ "message": message }),
            Self::Done => json!({}),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentResult {
    pub sql: String,
    pub explanation: Option<String>,
    pub task_type: Option<String>,
    pub sql_empty_reason: Option<String>,
    pub missing_information: Vec<String>,
    pub events: Vec<AgentEvent>,
}
