use crate::ai::tools::executor::ExecuteSqlTool;
use crate::ai::tools::knowledge::QueryKnowledgeTool;
use crate::ai::tools::rules::QueryRulesTool;
use crate::ai::tools::schema::QuerySchemaTool;
use crate::config::{AiConnectionMode, AiProvider, AppConfig, ResolvedAiProfile};
use crate::db::DbClient;
use crate::knowledge_base::KnowledgeBase;
use crate::rule_engine::RuleStore;
use tokio_util::sync::CancellationToken;
use futures_util::StreamExt;
use rig::agent::MultiTurnStreamItem;
use rig::client::CompletionClient;
use rig::completion::Chat;
use rig::streaming::{StreamedUserContent, StreamingChat};
use rig::tool::ToolDyn;
use std::pin::Pin;

use serde::{Deserialize, Serialize};

use std::sync::Arc;
use std::collections::HashMap;

// Re-export types needed by callers outside core_lib
pub use crate::ai::events::{AgentEvent, AgentResult, AiHealthReport};
use crate::ai::policy_store::Policy;

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("Missing API key")]
    MissingApiKey,
    #[error("No tokens available in pool")]
    NoTokens,
    #[error("AI auth failed: {0}")]
    Auth(String),
    #[error("AI forbidden: {0}")]
    Forbidden(String),
    #[error("AI model not found: {0}")]
    ModelNotFound(String),
    #[error("AI rate limited: {0}")]
    RateLimited(String),
    #[error("AI server error: {0}")]
    ServerError(String),
    #[error("Network error: {0}")]
    Network(String),
    #[error("Agent error: {0}")]
    Agent(String),
}

impl AgentError {
    /// Classify a raw rig-core error message into the appropriate variant.
    pub fn from_rig_message(msg: String) -> Self {
        let lower = msg.to_lowercase();
        if lower.contains("401") || lower.contains("unauthorized") || lower.contains("invalid api key") {
            AgentError::Auth(msg)
        } else if lower.contains("403") || lower.contains("forbidden") {
            AgentError::Forbidden(msg)
        } else if lower.contains("404") || lower.contains("model not found") {
            AgentError::ModelNotFound(msg)
        } else if lower.contains("429") || lower.contains("rate limit") || lower.contains("rate limited") {
            AgentError::RateLimited(msg)
        } else if lower.contains("500") || lower.contains("502") || lower.contains("503") || lower.contains("server error") {
            AgentError::ServerError(msg)
        } else if lower.contains("timeout") || lower.contains("connection") || lower.contains("network") {
            AgentError::Network(msg)
        } else {
            AgentError::Agent(msg)
        }
    }
}

pub type AgentStream = Pin<Box<dyn futures_util::Stream<Item = AgentEvent> + Send>>;

/// Task type classification for differentiated preamble and max_turns
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum TaskType {
    #[serde(rename = "generate_sql")]
    Generate,
    #[serde(rename = "explain_sql")]
    Explain,
    #[serde(rename = "optimize_sql")]
    Optimize,
    #[serde(rename = "fix_sql")]
    Fix,
    #[serde(rename = "general")]
    General,
}

impl TaskType {
    /// Convert a normalized mode string to TaskType
    pub fn from_mode(mode: Option<&str>) -> Self {
        match mode {
            Some("generate") => TaskType::Generate,
            Some("optimize") => TaskType::Optimize,
            Some("explain") => TaskType::Explain,
            Some("fix") => TaskType::Fix,
            _ => TaskType::General,
        }
    }
}

fn build_task_specific_guidance(task_type: &TaskType, dialect: &str) -> String {
    match task_type {
        TaskType::Generate => format!(
            "Focus on generating {} SQL from natural language. \
             Use query_schema to discover tables, then compose the query. \
             Always validate with execute_sql before returning.", dialect),
        TaskType::Explain =>
            "Focus on explaining the provided SQL. Break down each clause, \
             identify tables, joins, filters, and aggregations. \
             sql field should echo the input SQL. No execute_sql needed.".to_string(),
        TaskType::Optimize =>
            "Focus on optimizing the provided SQL. Analyze potential \
             performance issues (missing indexes, unnecessary joins, N+1 patterns). \
             Use execute_sql with EXPLAIN to compare before/after plans.".to_string(),
        TaskType::Fix =>
            "Focus on fixing the SQL error. Analyze the error message, \
             identify the root cause, and provide a minimal correction. \
             Always validate the fix with execute_sql.".to_string(),
        TaskType::General =>
            "Determine the task type from the user's request.".to_string(),
    }
}

fn resolve_max_turns(task_type: &TaskType, policy_max: u32) -> usize {
    let base = match task_type {
        TaskType::Explain => 2,      // explain doesn't need multi-turn tool calls
        TaskType::Generate => 5,     // generate needs exploration
        TaskType::Optimize => 6,     // optimize needs EXPLAIN comparison
        TaskType::Fix => 4,          // fix needs validation retry
        TaskType::General => 5,
    };
    base.min(policy_max as usize)
}

/// Pre-flight check: validate that the AI profile has an API key configured.
/// Call this before building tools or creating agents to fail fast.
pub fn validate_ai_profile(config: &AppConfig) -> Result<ResolvedAiProfile, AgentError> {
    let profile = config.resolve_ai_profile();
    if profile.api_key.as_deref().unwrap_or("").is_empty() {
        return Err(AgentError::MissingApiKey);
    }
    Ok(profile)
}

fn build_tools(
    db_client: Option<&DbClient>,
    db_name: &str,
    rule_store: &RuleStore,
    policy: &Policy,
    knowledge_base: &KnowledgeBase,
    db_connection_id: Option<&str>,
) -> Vec<Box<dyn ToolDyn>> {
    let mut tools: Vec<Box<dyn ToolDyn>> = Vec::new();

    if let Some(client) = db_client {
        tools.push(Box::new(QuerySchemaTool::new(client.clone(), db_name.to_string())));
        tools.push(Box::new(ExecuteSqlTool::new(client.clone())));
    }

    tools.push(Box::new(QueryRulesTool::new(rule_store.clone(), policy.clone())));
    tools.push(Box::new(QueryKnowledgeTool::new(
        knowledge_base.clone(),
        db_connection_id.map(|s| s.to_string()),
    )));

    tools
}

fn build_preamble(
    dialect: &str,
    schema_briefing: Option<&str>,
    extra_guidance: Option<&str>,
    task_type: &TaskType,
) -> String {
    let mut preamble = format!(
        "You are a careful {} SQL assistant with access to tools.\n\n\
        You can use the following tools:\n\
        - query_schema: Discover table structures (columns, types, indexes, foreign keys)\n\
        - execute_sql: Execute read-only SQL to validate correctness and preview results\n\
        - query_rules: Search for proven SQL rule patterns\n\
        - query_knowledge: Search business context and field descriptions\n\n\
        <workflow>\n\
        1. Understand the user's request and determine the task type (generate_sql, explain_sql, optimize_sql, fix_sql).\n\
        2. If the <schema_context> section lists relevant tables, use them as your starting point. \
           Call query_schema for deeper details only when needed.\n\
        3. Use query_rules to check for matching proven patterns.\n\
        4. Use query_knowledge to understand business terminology if needed.\n\
        5. Generate the SQL query.\n\
        6. Use execute_sql to validate the generated SQL works correctly.\n\
        7. If execute_sql returns an error, fix the SQL and try again.\n\
        8. Return your final answer as a JSON object.\n\
        </workflow>\n\n\
        <output_format>\n\
        Your final response MUST be a JSON object with these fields:\n\
        {{\n\
          \"task_type\": \"generate_sql|explain_sql|optimize_sql|fix_sql\",\n\
          \"sql\": \"the SQL query\",\n\
          \"explanation\": \"1-3 sentences explaining what you did\",\n\
          \"sql_empty_reason\": \"only if sql is empty\",\n\
          \"missing_information\": [\"list of missing info if any\"]\n\
        }}\n\
        Do not use markdown fences. Return ONLY the JSON object.\n\
        </output_format>",
        dialect
    );

    // Inject schema briefing — gives the agent a global view of the database
    if let Some(briefing) = schema_briefing.filter(|s| !s.trim().is_empty()) {
        preamble.push_str("\n\n<schema_context>\n");
        preamble.push_str(briefing.trim());
        preamble.push_str("\n</schema_context>");
    }

    if let Some(guidance) = extra_guidance.filter(|s| !s.trim().is_empty()) {
        preamble.push_str("\n\n<extra_guidance>\n");
        preamble.push_str(guidance.trim());
        preamble.push_str("\n</extra_guidance>");
    }

    // Inject task-type-specific guidance
    let task_guidance = build_task_specific_guidance(&task_type, &dialect);
    if !task_guidance.is_empty() {
        preamble.push_str("\n\n<task_guidance>\n");
        preamble.push_str(task_guidance.trim());
        preamble.push_str("\n</task_guidance>");
    }

    preamble
}

fn resolve_base_url(profile: &ResolvedAiProfile) -> Option<String> {
    match profile.mode {
        AiConnectionMode::Direct => None,
        AiConnectionMode::Relay | AiConnectionMode::LocalRelay | AiConnectionMode::Pool => {
            profile.relay_url.clone()
        }
    }
}

fn build_history(chat_history: Option<&[serde_json::Value]>) -> Vec<rig::message::Message> {
    chat_history
        .unwrap_or(&[])
        .iter()
        .filter_map(|msg| {
            let role = msg.get("role")?.as_str()?;
            let content = msg.get("content")?.as_str()?;
            match role {
                "user" => Some(rig::message::Message::user(content)),
                "assistant" => Some(rig::message::Message::assistant(content)),
                _ => None,
            }
        })
        .collect()
}

/// Run the rule fast-path: check rules before creating agent
pub fn try_rule_fast_path(
    user_input: &str,
    store: &RuleStore,
    policy: &Policy,
) -> Option<AgentResult> {
    let match_result = crate::rule_matcher::SemanticMatcher::find_best_match_with_thresholds(
        user_input,
        store,
        policy.rule_direct_threshold,
        policy.rule_suggest_threshold,
    );

    match match_result {
        crate::rule_matcher::MatchResult::DirectMatch { rule, .. } => Some(AgentResult {
            sql: rule.sql_template.clone(),
            explanation: Some(format!("Local Cache Hit (Rule: {})", rule.prompt_pattern)),
            task_type: Some("generate_sql".to_string()),
            sql_empty_reason: None,
            missing_information: Vec::new(),
            grounding_evidence: Vec::new(),
            assumptions: Vec::new(),
            referenced_tables: Vec::new(),
            risk_level: None,
            needs_confirmation: None,
            matched_rule_id: Some(rule.id.clone()),
            events: vec![AgentEvent::FinalSql {
                sql: rule.sql_template,
                task_type: Some("generate_sql".to_string()),
            }],
        }),
        _ => None,
    }
}

fn map_stream_items(
    item: Result<
        MultiTurnStreamItem<impl rig::completion::GetTokenUsage>,
        impl std::fmt::Display,
    >,
) -> Vec<AgentEvent> {
    match item {
        Ok(MultiTurnStreamItem::FinalResponse(response)) => {
            let mut events = Vec::new();

            // Extract token usage from FinalResponse
            let usage = response.usage();
            if usage.total_tokens > 0 {
                events.push(AgentEvent::TokenUsage {
                    prompt_tokens: usage.input_tokens,
                    completion_tokens: usage.output_tokens,
                    total_tokens: usage.total_tokens,
                });
            }

            let text = response.response();
            let intent = crate::ai::extractor::extract_sql_intent(text);
            if !intent.sql.is_empty() {
                events.push(AgentEvent::FinalSql {
                    sql: intent.sql,
                    task_type: intent.task_type,
                });
            } else if let Some(ref explanation) = intent.explanation {
                events.push(AgentEvent::Explanation {
                    text: explanation.clone(),
                });
            } else if !text.is_empty() {
                events.push(AgentEvent::Thinking {
                    text: text.to_string(),
                });
            }

            events
        }
        Ok(MultiTurnStreamItem::StreamAssistantItem(content)) => {
            use rig::streaming::StreamedAssistantContent;
            match content {
                StreamedAssistantContent::Text(text) if !text.text.is_empty() => {
                    vec![AgentEvent::Thinking { text: text.text }]
                }
                StreamedAssistantContent::ToolCall { tool_call, .. } => {
                    vec![AgentEvent::ToolCall {
                        tool: tool_call.function.name.clone(),
                        args: tool_call.function.arguments,
                        call_id: Some(tool_call.id.clone()),
                    }]
                }
                _ => vec![],
            }
        }
        Ok(MultiTurnStreamItem::StreamUserItem(StreamedUserContent::ToolResult {
            tool_result,
            ..
        })) => {
            // Extract text content from the tool result
            let content_text = match tool_result.content.first_ref() {
                rig::completion::message::ToolResultContent::Text(t) => t.text.clone(),
                other => format!("{:?}", other),
            };
            // The tool name is not directly available in ToolResult; use the id as identifier.
            // The frontend correlates tool_call → tool_result by stream order.
            vec![AgentEvent::ToolResult {
                tool: tool_result.id.clone(),
                result: content_text,
                call_id: tool_result.call_id,
            }]
        }
        Ok(_) => vec![],
        Err(e) => vec![AgentEvent::Error {
            message: e.to_string(),
        }],
    }
}

/// Build a rig OpenAI-compatible client and agent with the given config.
fn build_openai_agent(
    profile: &ResolvedAiProfile,
    model_id: &str,
    preamble: &str,
    tools: Vec<Box<dyn ToolDyn>>,
    max_turns: usize,
) -> Result<rig::agent::Agent<impl rig::completion::CompletionModel>, AgentError> {
    let api_key = profile.api_key.as_deref().ok_or(AgentError::MissingApiKey)?;
    let mut builder = rig::providers::openai::Client::builder().api_key(api_key);
    if let Some(url) = resolve_base_url(profile) {
        builder = builder.base_url(&url);
    }
    let client = builder.build().map_err(|e| AgentError::from_rig_message(e.to_string()))?;
    Ok(client
        .agent(model_id)
        .preamble(preamble)
        .max_tokens(4096)
        .default_max_turns(max_turns)
        .tools(tools)
        .build())
}

/// Build a rig Anthropic client and agent with the given config.
fn build_anthropic_agent(
    profile: &ResolvedAiProfile,
    model_id: &str,
    preamble: &str,
    tools: Vec<Box<dyn ToolDyn>>,
    max_turns: usize,
) -> Result<rig::agent::Agent<impl rig::completion::CompletionModel>, AgentError> {
    let api_key = profile.api_key.as_deref().ok_or(AgentError::MissingApiKey)?;
    let mut builder = rig::providers::anthropic::Client::builder().api_key(api_key);
    if let Some(url) = resolve_base_url(profile) {
        builder = builder.base_url(&url);
    }
    let client = builder.build().map_err(|e| AgentError::from_rig_message(e.to_string()))?;
    Ok(client
        .agent(model_id)
        .preamble(preamble)
        .max_tokens(4096)
        .default_max_turns(max_turns)
        .tools(tools)
        .build())
}

/// Common preparation: resolve profile, model, tools, preamble, and max_turns from config.
fn prepare_agent_context<'a>(
    config: &AppConfig,
    db_client: Option<&'a DbClient>,
    db_name: &str,
    rule_store: &'a RuleStore,
    knowledge_base: &'a KnowledgeBase,
    policy: &Policy,
    schema_briefing: Option<&str>,
    extra_guidance: Option<&str>,
    task_type: &TaskType,
) -> Result<(ResolvedAiProfile, String, Vec<Box<dyn ToolDyn>>, String, usize), AgentError> {
    let profile = config.resolve_ai_profile();
    let (model_id, _) = config.resolve_active_model();
    let dialect = config.get_active_db_type();
    let tools = build_tools(
        db_client,
        db_name,
        rule_store,
        policy,
        knowledge_base,
        config.active_db_id.as_deref(),
    );
    let preamble = build_preamble(&dialect, schema_briefing, extra_guidance, task_type);
    let max_turns = resolve_max_turns(task_type, policy.agent_max_turns);
    Ok((profile, model_id, tools, preamble, max_turns))
}

/// Non-streaming entry point
pub async fn run_agent(
    config: &AppConfig,
    db_client: Option<&DbClient>,
    db_name: &str,
    user_input: &str,
    rule_store: &RuleStore,
    knowledge_base: &KnowledgeBase,
    policy: &Policy,
    chat_history: Option<&[serde_json::Value]>,
    schema_briefing: Option<&str>,
    extra_guidance: Option<&str>,
    task_type: &TaskType,
) -> Result<AgentResult, AgentError> {
    let (profile, model_id, tools, preamble, max_turns) = prepare_agent_context(
        config, db_client, db_name, rule_store, knowledge_base, policy, schema_briefing, extra_guidance, task_type,
    )?;
    let history = build_history(chat_history);

    // Per-agent timeout: 120s per turn × max_turns
    let timeout_secs = (max_turns as u64).saturating_mul(120);
    let response_text: String = match profile.provider {
        AiProvider::Anthropic => {
            let agent =
                build_anthropic_agent(&profile, &model_id, &preamble, tools, max_turns)?;
            tokio::time::timeout(
                std::time::Duration::from_secs(timeout_secs),
                agent.chat(user_input, history.clone()),
            )
            .await
            .map_err(|_| AgentError::Agent("Agent timed out".to_string()))?
            .map_err(|e| AgentError::from_rig_message(e.to_string()))?
        }
        _ => {
            let agent =
                build_openai_agent(&profile, &model_id, &preamble, tools, max_turns)?;
            tokio::time::timeout(
                std::time::Duration::from_secs(timeout_secs),
                agent.chat(user_input, history.clone()),
            )
            .await
            .map_err(|_| AgentError::Agent("Agent timed out".to_string()))?
            .map_err(|e| AgentError::from_rig_message(e.to_string()))?
        }
    };

    let intent = crate::ai::extractor::extract_sql_intent(&response_text);
    let mut events = Vec::new();

    if !intent.sql.is_empty() {
        events.push(AgentEvent::FinalSql {
            sql: intent.sql.clone(),
            task_type: intent.task_type.clone(),
        });
    }
    if let Some(ref explanation) = intent.explanation {
        events.push(AgentEvent::Explanation {
            text: explanation.clone(),
        });
    }
    events.push(AgentEvent::Done);

    Ok(AgentResult {
        sql: intent.sql,
        explanation: intent.explanation,
        task_type: intent.task_type,
        sql_empty_reason: intent.sql_empty_reason,
        missing_information: intent.missing_information,
        grounding_evidence: intent.grounding_evidence,
        assumptions: intent.assumptions,
        referenced_tables: intent.referenced_tables,
        risk_level: intent.risk_level,
        needs_confirmation: intent.needs_confirmation,
        matched_rule_id: None,
        events,
    })
}

/// Main entry: run agent with streaming, yielding AgentEvents
pub async fn run_agent_streaming(
    config: &AppConfig,
    db_client: Option<&DbClient>,
    db_name: &str,
    user_input: &str,
    rule_store: &RuleStore,
    knowledge_base: &KnowledgeBase,
    policy: &Policy,
    chat_history: Option<&[serde_json::Value]>,
    schema_briefing: Option<&str>,
    extra_guidance: Option<&str>,
    cancel_token: CancellationToken,
    task_type: &TaskType,
) -> Result<AgentStream, AgentError> {
    let (profile, model_id, tools, preamble, max_turns) = prepare_agent_context(
        config, db_client, db_name, rule_store, knowledge_base, policy, schema_briefing, extra_guidance, task_type,
    )?;
    let history = build_history(chat_history);

    // Build call_id → tool_name mapping table for ToolResult event correlation
    let tool_name_map: Arc<std::sync::Mutex<HashMap<String, String>>> = Arc::new(std::sync::Mutex::new(HashMap::new()));
    let stream: AgentStream = match profile.provider {
        AiProvider::Anthropic => {
            let agent =
                build_anthropic_agent(&profile, &model_id, &preamble, tools, max_turns)?;
            let s = agent.stream_chat(user_input, &history).await;
            let ct = cancel_token.clone();
            let map_arc = tool_name_map.clone();
            Box::pin(s
                .then(move |item| {
                    let ct = ct.clone();
                    let map_arc = map_arc.clone();
                    async move {
                        if ct.is_cancelled() {
                            vec![AgentEvent::Error {
                                message: "Agent execution cancelled by user".to_string(),
                            }]
                        } else {
                            let events = map_stream_items(item);
                            // Phase 1: Record ToolCall call_id → tool_name mappings
                            for ev in events.iter() {
                                if let AgentEvent::ToolCall { tool, call_id: Some(ref id), .. } = ev {
                                    map_arc.lock().unwrap().insert(id.clone(), tool.clone());
                                }
                            }
                            // Phase 2: Resolve ToolResult tool names from the mapping
                            events.into_iter().map(|ev| {
                                if let AgentEvent::ToolResult { ref tool, ref call_id, ref result } = ev {
                                    let resolved = if let Some(ref id) = call_id {
                                        map_arc.lock().unwrap()
                                            .get(id).cloned().unwrap_or_else(|| tool.clone())
                                    } else {
                                        tool.clone()
                                    };
                                    AgentEvent::ToolResult {
                                        tool: resolved,
                                        result: result.clone(),
                                        call_id: call_id.clone(),
                                    }
                                } else {
                                    ev
                                }
                            }).collect::<Vec<_>>()
                        }
                    }
                })
                .flat_map(futures_util::stream::iter)
            )
        }
        _ => {
            let agent =
                build_openai_agent(&profile, &model_id, &preamble, tools, max_turns)?;
            let s = agent.stream_chat(user_input, &history).await;
            let ct = cancel_token.clone();
            let map_arc = tool_name_map.clone();
            Box::pin(s
                .then(move |item| {
                    let ct = ct.clone();
                    let map_arc = map_arc.clone();
                    async move {
                        if ct.is_cancelled() {
                            vec![AgentEvent::Error {
                                message: "Agent execution cancelled by user".to_string(),
                            }]
                        } else {
                            let events = map_stream_items(item);
                            // Phase 1: Record ToolCall call_id → tool_name mappings
                            for ev in events.iter() {
                                if let AgentEvent::ToolCall { tool, call_id: Some(ref id), .. } = ev {
                                    map_arc.lock().unwrap().insert(id.clone(), tool.clone());
                                }
                            }
                            // Phase 2: Resolve ToolResult tool names from the mapping
                            events.into_iter().map(|ev| {
                                if let AgentEvent::ToolResult { ref tool, ref call_id, ref result } = ev {
                                    let resolved = if let Some(ref id) = call_id {
                                        map_arc.lock().unwrap()
                                            .get(id).cloned().unwrap_or_else(|| tool.clone())
                                    } else {
                                        tool.clone()
                                    };
                                    AgentEvent::ToolResult {
                                        tool: resolved,
                                        result: result.clone(),
                                        call_id: call_id.clone(),
                                    }
                                } else {
                                    ev
                                }
                            }).collect::<Vec<_>>()
                        }
                    }
                })
                .flat_map(futures_util::stream::iter)
            )
        }
    };

    Ok(stream)
}

/// One-shot LLM call: generate a Handlebars-style rule template from prompt + SQL.
/// Used by the "save rule" endpoint and the go-live smoke test.
pub async fn generate_rule_template(
    config: &AppConfig,
    prompt: &str,
    sql: &str,
) -> Result<String, AgentError> {
    let _profile = config.resolve_ai_profile();
    let (_model_id, _) = config.resolve_active_model();

    let system_prompt = "You are an expert SQL analyst. The user will provide a Natural Language prompt and its corresponding SQL statement. \
    Your task is to identify dynamic parameters (like IDs, names, dates, amounts) in the SQL and replace them with Handlebars-style templates like {{id}}, {{status}}, etc. \
    Output ONLY the templated SQL, nothing else. If there are no obvious parameters, just return the exact original SQL.";

    let user_msg = format!("Prompt: {}\n\nSQL: {}", prompt, sql);

    let response_text = chat_completion_raw(config, system_prompt, &user_msg).await?;
    let cleaned = crate::ai::extractor::extract_code_block(&response_text, "sql");
    Ok(cleaned)
}

/// One-shot LLM call without tools: send system + user messages and return raw text.
/// Used for simple AI calls that don't need the agent loop (rule template, mock data, etc.)
pub async fn chat_completion_raw(
    config: &AppConfig,
    system_prompt: &str,
    user_message: &str,
) -> Result<String, AgentError> {
    let profile = config.resolve_ai_profile();
    let (model_id, _) = config.resolve_active_model();
    let api_key = profile.api_key.as_deref().ok_or(AgentError::MissingApiKey)?;

    let timeout = crate::timeout_policy::TimeoutPolicy::default().ai_request_timeout_for_tier(&config.active_tier);
    let timeout_secs = timeout.as_secs().max(30);
    let history: Vec<rig::message::Message> = vec![];

    match profile.provider {
        AiProvider::Anthropic => {
            let mut builder = rig::providers::anthropic::Client::builder().api_key(api_key);
            if let Some(url) = resolve_base_url(&profile) {
                builder = builder.base_url(&url);
            }
            let client = builder.build().map_err(|e| AgentError::from_rig_message(e.to_string()))?;
            let agent = client
                .agent(&model_id)
                .preamble(system_prompt)
                .max_tokens(4096)
                .build();

            tokio::time::timeout(
                std::time::Duration::from_secs(timeout_secs),
                agent.chat(user_message, history),
            )
            .await
            .map_err(|_| AgentError::Agent("Request timed out".to_string()))?
            .map_err(|e| AgentError::from_rig_message(e.to_string()))
        }
        _ => {
            let mut builder = rig::providers::openai::Client::builder().api_key(api_key);
            if let Some(url) = resolve_base_url(&profile) {
                builder = builder.base_url(&url);
            }
            let client = builder.build().map_err(|e| AgentError::from_rig_message(e.to_string()))?;
            let agent = client
                .agent(&model_id)
                .preamble(system_prompt)
                .max_tokens(4096)
                .build();

            tokio::time::timeout(
                std::time::Duration::from_secs(timeout_secs),
                agent.chat(user_message, history),
            )
            .await
            .map_err(|_| AgentError::Agent("Request timed out".to_string()))?
            .map_err(|e| AgentError::from_rig_message(e.to_string()))
        }
    }
}
