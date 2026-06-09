use crate::config::{AiConnectionMode, AiProvider};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum AgentEvent {
    #[serde(rename = "thinking")]
    Thinking { text: String },
    #[serde(rename = "tool_call")]
    ToolCall {
        tool: String,
        args: serde_json::Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
    },
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
    #[serde(rename = "token_usage")]
    TokenUsage {
        prompt_tokens: u64,
        completion_tokens: u64,
        total_tokens: u64,
    },
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
            Self::TokenUsage { .. } => "token_usage",
            Self::Done => "done",
        }
    }

    /// Serialize the event data payload (excluding the "type" discriminator).
    pub fn data_json(&self) -> serde_json::Value {
        match self {
            Self::Thinking { text } => json!({ "text": text }),
            Self::ToolCall { tool, args, call_id } => json!({ "tool": tool, "args": args, "call_id": call_id }),
            Self::ToolResult { tool, result, call_id } => json!({ "tool": tool, "result": result, "call_id": call_id }),
            Self::SqlDraft { sql } => json!({ "sql": sql }),
            Self::FinalSql { sql, task_type } => json!({ "sql": sql, "task_type": task_type }),
            Self::Explanation { text } => json!({ "text": text }),
            Self::Error { message } => json!({ "message": message }),
            Self::TokenUsage { prompt_tokens, completion_tokens, total_tokens } => json!({
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion_tokens,
                "total_tokens": total_tokens,
            }),
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
    // Grounding metadata — mirrors StructuredSqlIntent
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub grounding_evidence: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assumptions: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub referenced_tables: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk_level: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub needs_confirmation: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_rule_id: Option<String>,
    pub events: Vec<AgentEvent>,
}

/// AI provider health check report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiHealthReport {
    pub ok: bool,
    pub active_ai_profile_id: Option<String>,
    pub provider: AiProvider,
    pub mode: AiConnectionMode,
    pub endpoint: String,
    pub model_id: String,
    pub tier: String,
    pub latency_ms: Option<u128>,
    pub result_preview: Option<String>,
}
