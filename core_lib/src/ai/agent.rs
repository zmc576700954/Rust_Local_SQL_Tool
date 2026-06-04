use crate::ai::events::{AgentEvent, AgentResult};
use crate::ai::policy_store::Policy;
use crate::ai::tools::executor::ExecuteSqlTool;
use crate::ai::tools::knowledge::QueryKnowledgeTool;
use crate::ai::tools::rules::QueryRulesTool;
use crate::ai::tools::schema::QuerySchemaTool;
use crate::config::{AiConnectionMode, AiProvider, AppConfig, ResolvedAiProfile};
use crate::db::DbClient;
use crate::knowledge_base::KnowledgeBase;
use crate::rule_engine::RuleStore;
use futures_util::StreamExt;
use rig::agent::MultiTurnStreamItem;
use rig::client::CompletionClient;
use rig::completion::Prompt;
use rig::streaming::{StreamedUserContent, StreamingChat};
use rig::tool::ToolDyn;
use std::pin::Pin;

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("Missing API key")]
    MissingApiKey,
    #[error("AI auth failed: {0}")]
    Auth(String),
    #[error("Agent error: {0}")]
    Agent(String),
}

impl AgentError {
    /// Classify a raw rig-core error message into the appropriate variant.
    fn from_rig_message(msg: String) -> Self {
        let lower = msg.to_lowercase();
        if lower.contains("401") || lower.contains("unauthorized") || lower.contains("invalid api key") {
            AgentError::Auth(msg)
        } else if lower.contains("403") || lower.contains("forbidden") {
            AgentError::Auth(msg)
        } else {
            AgentError::Agent(msg)
        }
    }
}

type AgentStream = Pin<Box<dyn futures_util::Stream<Item = AgentEvent> + Send>>;

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

fn build_preamble(dialect: &str, extra_guidance: Option<&str>) -> String {
    let mut preamble = format!(
        "You are a careful {} SQL assistant with access to tools.\n\n\
        You can use the following tools:\n\
        - query_schema: Discover table structures (columns, types, indexes, foreign keys)\n\
        - execute_sql: Execute read-only SQL to validate correctness and preview results\n\
        - query_rules: Search for proven SQL rule patterns\n\
        - query_knowledge: Search business context and field descriptions\n\n\
        <workflow>\n\
        1. Understand the user's request and determine the task type (generate_sql, explain_sql, optimize_sql, fix_sql).\n\
        2. Use query_schema to discover relevant tables and columns if needed.\n\
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

    if let Some(guidance) = extra_guidance.filter(|s| !s.trim().is_empty()) {
        preamble.push_str("\n\n<extra_guidance>\n");
        preamble.push_str(guidance.trim());
        preamble.push_str("\n</extra_guidance>");
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
            events: vec![AgentEvent::FinalSql {
                sql: rule.sql_template,
                task_type: Some("generate_sql".to_string()),
            }],
        }),
        _ => None,
    }
}

fn map_stream_item(
    item: Result<
        MultiTurnStreamItem<impl rig::completion::GetTokenUsage>,
        impl std::fmt::Display,
    >,
) -> Option<AgentEvent> {
    match item {
        Ok(MultiTurnStreamItem::FinalResponse(response)) => {
            let text = response.response();
            let intent = crate::ai::extractor::extract_sql_intent(text);
            if !intent.sql.is_empty() {
                Some(AgentEvent::FinalSql {
                    sql: intent.sql,
                    task_type: intent.task_type,
                })
            } else if let Some(ref explanation) = intent.explanation {
                Some(AgentEvent::Explanation {
                    text: explanation.clone(),
                })
            } else if !text.is_empty() {
                Some(AgentEvent::Thinking {
                    text: text.to_string(),
                })
            } else {
                None
            }
        }
        Ok(MultiTurnStreamItem::StreamAssistantItem(content)) => {
            use rig::streaming::StreamedAssistantContent;
            match content {
                StreamedAssistantContent::Text(text) if !text.text.is_empty() => {
                    Some(AgentEvent::Thinking { text: text.text })
                }
                StreamedAssistantContent::ToolCall { tool_call, .. } => {
                    Some(AgentEvent::ToolCall {
                        tool: tool_call.function.name,
                        args: tool_call.function.arguments,
                    })
                }
                _ => None,
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
            Some(AgentEvent::ToolResult {
                tool: tool_result.id.clone(),
                result: content_text,
                call_id: tool_result.call_id,
            })
        }
        Ok(_) => None,
        Err(e) => Some(AgentEvent::Error {
            message: e.to_string(),
        }),
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
    extra_guidance: Option<&str>,
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
    let preamble = build_preamble(&dialect, extra_guidance);
    let max_turns = policy.agent_max_turns as usize;
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
    _chat_history: Option<&[serde_json::Value]>,
    extra_guidance: Option<&str>,
) -> Result<AgentResult, AgentError> {
    let (profile, model_id, tools, preamble, max_turns) = prepare_agent_context(
        config, db_client, db_name, rule_store, knowledge_base, policy, extra_guidance,
    )?;

    // Per-agent timeout: 120s per turn × max_turns
    let timeout_secs = (max_turns as u64).saturating_mul(120);
    let response_text: String = match profile.provider {
        AiProvider::Anthropic => {
            let agent =
                build_anthropic_agent(&profile, &model_id, &preamble, tools, max_turns)?;
            tokio::time::timeout(
                std::time::Duration::from_secs(timeout_secs),
                agent.prompt(user_input),
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
                agent.prompt(user_input),
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
    extra_guidance: Option<&str>,
) -> Result<AgentStream, AgentError> {
    let (profile, model_id, tools, preamble, max_turns) = prepare_agent_context(
        config, db_client, db_name, rule_store, knowledge_base, policy, extra_guidance,
    )?;
    let history = build_history(chat_history);

    let stream: AgentStream = match profile.provider {
        AiProvider::Anthropic => {
            let agent =
                build_anthropic_agent(&profile, &model_id, &preamble, tools, max_turns)?;
            let s = agent.stream_chat(user_input, &history).await;
            Box::pin(s.filter_map(|item| async { map_stream_item(item) }))
        }
        _ => {
            let agent =
                build_openai_agent(&profile, &model_id, &preamble, tools, max_turns)?;
            let s = agent.stream_chat(user_input, &history).await;
            Box::pin(s.filter_map(|item| async { map_stream_item(item) }))
        }
    };

    Ok(stream)
}
