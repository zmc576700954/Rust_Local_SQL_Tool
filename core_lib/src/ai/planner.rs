use crate::ai::extractor::extract_code_block;
use crate::ai::gateway::{AiError, AiGateway, ChatMessage};
use crate::ai::policy_store::Policy;
use crate::ai::prompting::{build_sql_generation_system_prompt, build_user_request_message};
use crate::db::DbClient;
use crate::rule_engine::{RuleStore, RuleType};
use crate::rule_matcher::{MatchResult, SemanticMatcher};
use crate::schema::{SchemaExtractor, SchemaResponse, TableWithDetails};
use crate::template::{extract_placeholders, render_template};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntentResult {
    pub sql: String,
    pub explanation: Option<String>,
    pub task_type: Option<String>,
    pub sql_empty_reason: Option<String>,
    pub missing_information: Vec<String>,
    pub matched_rule_id: Option<String>,
}

#[derive(Clone)]
pub struct Planner {
    pub gateway: AiGateway,
}

impl Planner {
    pub fn new(gateway: AiGateway) -> Self {
        Self { gateway }
    }

    pub async fn generate_rule_template(&self, prompt: &str, sql: &str) -> Result<String, AiError> {
        let system_prompt = "You are an expert SQL analyst. The user will provide a Natural Language prompt and its corresponding SQL statement. \
        Your task is to identify dynamic parameters (like IDs, names, dates, amounts) in the SQL and replace them with Handlebars-style templates like {{id}}, {{status}}, etc. \
        Output ONLY the templated SQL, nothing else. If there are no obvious parameters, just return the exact original SQL.";

        let user_msg = format!("Prompt: {}\n\nSQL: {}", prompt, sql);

        let messages = vec![
            ChatMessage {
                role: "system".into(),
                content: system_prompt.to_string(),
            },
            ChatMessage {
                role: "user".into(),
                content: user_msg,
            },
        ];

        let response_text = self.gateway.chat_completion(messages).await?;

        let cleaned_sql = extract_code_block(&response_text, "sql");

        Ok(cleaned_sql)
    }

    async fn extract_template_params(
        &self,
        user_input: &str,
        rule_prompt: &str,
        sql_template: &str,
        params: &[String],
    ) -> Result<serde_json::Map<String, Value>, AiError> {
        let system_prompt = "You are a parameter extraction engine for SQL templates.\n\
You MUST output ONLY a valid JSON object, no markdown, no explanations, no extra text.\n\
Keys MUST match the given parameter list exactly.\n\
If a parameter cannot be inferred, set it to null.\n\
Do not generate SQL.";

        let user_msg = format!(
            "User Request:\n{}\n\n\
Matched Rule Prompt:\n{}\n\n\
SQL Template:\n{}\n\n\
Parameters (keys):\n{}\n\n\
Return JSON object only.",
            user_input,
            rule_prompt,
            sql_template,
            params.join(", ")
        );

        let messages = vec![
            ChatMessage {
                role: "system".into(),
                content: system_prompt.to_string(),
            },
            ChatMessage {
                role: "user".into(),
                content: user_msg,
            },
        ];

        let response_text = self.gateway.chat_completion(messages).await?;

        let cleaned = extract_code_block(&response_text, "json");

        let v: Value = serde_json::from_str(&cleaned)
            .map_err(|e| AiError::ApiError(format!("Failed to parse JSON from AI: {}", e)))?;

        let obj = v
            .as_object()
            .ok_or_else(|| AiError::ApiError("AI did not return a JSON object".into()))?;

        for p in params {
            if !obj.contains_key(p) {
                return Err(AiError::ApiError(format!("AI response missing key: {}", p)));
            }
        }

        Ok(obj.clone())
    }

    async fn try_rule_fast_path(
        &self,
        user_input: &str,
        store: &RuleStore,
        policy: &Policy,
    ) -> Option<IntentResult> {
        let match_result = SemanticMatcher::find_best_match_with_thresholds(
            user_input,
            store,
            policy.rule_direct_threshold,
            policy.rule_suggest_threshold,
        );

        match match_result {
            MatchResult::DirectMatch {
                rule, confidence, ..
            } => {
                tracing::info!(
                    "Rule Matched ({:?}, conf: {:.2}): {}",
                    rule.rule_type,
                    confidence,
                    rule.prompt_pattern
                );

                if rule.rule_type == RuleType::Template && rule.sql_template.contains("{{") {
                    let params = extract_placeholders(&rule.sql_template);
                    if params.is_empty() {
                        return Some(IntentResult {
                            sql: rule.sql_template,
                            explanation: Some(format!(
                                "Local Cache Hit (Rule: {})",
                                rule.prompt_pattern
                            )),
                            task_type: Some("generate_sql".to_string()),
                            sql_empty_reason: None,
                            missing_information: Vec::new(),
                            matched_rule_id: Some(rule.id.clone()),
                        });
                    }

                    let extracted = self
                        .extract_template_params(
                            user_input,
                            &rule.prompt_pattern,
                            &rule.sql_template,
                            &params,
                        )
                        .await;

                    match extracted {
                        Ok(obj) => {
                            let sql = render_template(&rule.sql_template, &obj);
                            return Some(IntentResult {
                                sql,
                                explanation: Some(format!(
                                    "Template Rule Filled (Rule: {})",
                                    rule.prompt_pattern
                                )),
                                task_type: Some("generate_sql".to_string()),
                                sql_empty_reason: None,
                                missing_information: Vec::new(),
                                matched_rule_id: Some(rule.id.clone()),
                            });
                        }
                        Err(e) => {
                            tracing::warn!(
                                "Template param extraction failed, fallback to raw template: {}",
                                e
                            );
                        }
                    }

                    return Some(IntentResult {
                        sql: rule.sql_template,
                        explanation: Some(format!(
                            "Template Rule Fallback (Rule: {})",
                            rule.prompt_pattern
                        )),
                        task_type: Some("generate_sql".to_string()),
                        sql_empty_reason: None,
                        missing_information: Vec::new(),
                        matched_rule_id: Some(rule.id.clone()),
                    });
                }

                Some(IntentResult {
                    sql: rule.sql_template,
                    explanation: Some(format!(
                        "Local Cache Hit (Rule: {})",
                        rule.prompt_pattern
                    )),
                    task_type: Some("generate_sql".to_string()),
                    sql_empty_reason: None,
                    missing_information: Vec::new(),
                    matched_rule_id: Some(rule.id.clone()),
                })
            }
            MatchResult::SuggestionMatch { .. } => None,
            MatchResult::None => None,
        }
    }

    pub async fn generate_sql_with_virtual_schema(
        &self,
        user_input: &str,
        virtual_schema: &SchemaResponse,
        store: &RuleStore,
        policy: &Policy,
        db_type: &str,
        chat_history: Option<&[Value]>,
    ) -> Result<IntentResult, AiError> {
        if let Some(r) = self.try_rule_fast_path(user_input, store, policy).await {
            return Ok(r);
        }

        let extra_guidance = suggestion_guidance(user_input, store, policy);
        let system_prompt = build_sql_generation_system_prompt(
            db_type,
            user_input,
            Some(virtual_schema),
            &[],
            chat_history,
            None,
            None,
            extra_guidance.as_deref(),
        );

        let messages = vec![
            ChatMessage {
                role: "system".into(),
                content: system_prompt,
            },
            ChatMessage {
                role: "user".into(),
                content: build_user_request_message(user_input),
            },
        ];

        let response_text = if policy.structured_output_enabled {
            self.gateway.chat_completion_json(messages).await?
        } else {
            self.gateway.chat_completion(messages).await?
        };
        let intent = crate::ai::extractor::extract_sql_intent(&response_text);

        Ok(IntentResult {
            sql: intent.sql,
            explanation: intent
                .explanation
                .or(Some("Generated using offline schema".to_string())),
            task_type: intent.task_type,
            sql_empty_reason: intent.sql_empty_reason,
            missing_information: intent.missing_information,
            matched_rule_id: None,
        })
    }

    pub async fn generate_sql_no_schema(
        &self,
        user_input: &str,
        store: &RuleStore,
        policy: &Policy,
        db_type: &str,
        chat_history: Option<&[Value]>,
    ) -> Result<IntentResult, AiError> {
        if let Some(r) = self.try_rule_fast_path(user_input, store, policy).await {
            return Ok(r);
        }

        let extra_guidance = suggestion_guidance(user_input, store, policy);
        let system_prompt = build_sql_generation_system_prompt(
            db_type,
            user_input,
            None,
            &[],
            chat_history,
            None,
            None,
            extra_guidance.as_deref(),
        );

        let messages = vec![
            ChatMessage {
                role: "system".into(),
                content: system_prompt,
            },
            ChatMessage {
                role: "user".into(),
                content: build_user_request_message(user_input),
            },
        ];

        let response_text = if policy.structured_output_enabled {
            self.gateway.chat_completion_json(messages).await?
        } else {
            self.gateway.chat_completion(messages).await?
        };

        let intent = crate::ai::extractor::extract_sql_intent(&response_text);

        Ok(IntentResult {
            sql: intent.sql,
            explanation: intent
                .explanation
                .or(Some("Generated without schema context".to_string())),
            task_type: intent.task_type,
            sql_empty_reason: intent.sql_empty_reason,
            missing_information: intent.missing_information,
            matched_rule_id: None,
        })
    }

    /// 1. Fetch schema context
    /// 2. Build prompt
    /// 3. Call AI Gateway
    /// 4. Extract SQL
    pub async fn generate_sql(
        &self,
        db_client: &DbClient,
        db_name: &str,
        user_input: &str,
        store: &RuleStore,
        policy: &Policy,
        db_type: &str,
        chat_history: Option<&[Value]>,
    ) -> Result<IntentResult, AiError> {
        if let Some(r) = self.try_rule_fast_path(user_input, store, policy).await {
            return Ok(r);
        }

        let extra_guidance = suggestion_guidance(user_input, store, policy);

        // Step 2: Extract schema context if needed
        let mut schema = SchemaResponse {
            db_name: db_name.to_string(),
            tables: Vec::new(),
            views: Vec::new(),
        };
        if let Ok(tables) = SchemaExtractor::get_tables(db_client, db_name).await {
            for table in tables {
                if let Ok(columns) =
                    SchemaExtractor::get_columns(db_client, db_name, &table.table_name).await
                {
                    schema.tables.push(TableWithDetails {
                        table_name: table.table_name,
                        columns,
                        indexes: Vec::new(),
                        foreign_keys: Vec::new(),
                    });
                }
            }
        }

        let system_prompt = build_sql_generation_system_prompt(
            db_type,
            user_input,
            Some(&schema),
            &[],
            chat_history,
            None,
            None,
            extra_guidance.as_deref(),
        );

        let messages = vec![
            ChatMessage {
                role: "system".into(),
                content: system_prompt,
            },
            ChatMessage {
                role: "user".into(),
                content: build_user_request_message(user_input),
            },
        ];

        // Step 4: Call AI Gateway
        let response_text = if policy.structured_output_enabled {
            self.gateway.chat_completion_json(messages).await?
        } else {
            self.gateway.chat_completion(messages).await?
        };

        // Step 5: Extract SQL Intent using the multi-level fault-tolerant pipeline
        let intent = crate::ai::extractor::extract_sql_intent(&response_text);

        Ok(IntentResult {
            sql: intent.sql,
            explanation: intent.explanation,
            task_type: intent.task_type,
            sql_empty_reason: intent.sql_empty_reason,
            missing_information: intent.missing_information,
            matched_rule_id: None,
        })
    }
}

fn suggestion_guidance(user_input: &str, store: &RuleStore, policy: &Policy) -> Option<String> {
    let match_result = SemanticMatcher::find_best_match_with_thresholds(
        user_input,
        store,
        policy.rule_direct_threshold,
        policy.rule_suggest_threshold,
    );

    if let MatchResult::SuggestionMatch { rule, confidence } = match_result {
        tracing::info!(
            "Rule Suggested ({:?}, conf: {:.2}): {}",
            rule.rule_type,
            confidence,
            rule.prompt_pattern
        );
        return Some(format!(
            "A similar successful rule exists. Use it as a semantic hint, not as a blind template.\nKeep the user's requested metric, filters, time window, grouping, ordering, and row granularity aligned with the current request.\nReuse tables and predicates from the reference only when they are grounded by the current schema context.\nKnown rule prompt: {}\nReference SQL:\n{}",
            rule.prompt_pattern, rule.sql_template
        ));
    }

    None
}
