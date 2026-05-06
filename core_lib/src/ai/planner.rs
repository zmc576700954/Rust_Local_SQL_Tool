use crate::ai::extractor::extract_code_block;
use crate::ai::gateway::{AiError, AiGateway, ChatMessage};
use crate::ai::policy_store::Policy;
use crate::db::DbClient;
use crate::rule_engine::{RuleStore, RuleType};
use crate::rule_matcher::{MatchResult, SemanticMatcher};
use crate::schema::{SchemaExtractor, SchemaResponse};
use crate::template::{extract_placeholders, render_template};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntentResult {
    pub sql: String,
    pub explanation: Option<String>,
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
                                "💡 0 Token Local Cache Hit (Rule: {})",
                                rule.prompt_pattern
                            )),
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
                                    "💡 Template Rule Filled (Rule: {})",
                                    rule.prompt_pattern
                                )),
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
                            "💡 Template Rule Fallback (Rule: {})",
                            rule.prompt_pattern
                        )),
                        matched_rule_id: Some(rule.id.clone()),
                    });
                }

                Some(IntentResult {
                    sql: rule.sql_template,
                    explanation: Some(format!(
                        "💡 0 Token Local Cache Hit (Rule: {})",
                        rule.prompt_pattern
                    )),
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
    ) -> Result<IntentResult, AiError> {
        if let Some(r) = self.try_rule_fast_path(user_input, store, policy).await {
            return Ok(r);
        }

        let mut system_prompt = format!(
            "You are a local AI SQL assistant. You must generate valid {} SQL statements based on the user's natural language request.\n\n\
            Available Schema (Offline Mode):\n\
            Database: {}\n",
            db_type,
            virtual_schema.db_name
        );

        for t in &virtual_schema.tables {
            system_prompt.push_str(&format!("- Table: {}\n", t.table_name));
            for c in &t.columns {
                system_prompt.push_str(&format!(
                    "  - {}: {} ({})\n",
                    c.column_name, c.data_type, c.is_nullable
                ));
            }
        }

        system_prompt.push_str(
            &format!("\nImportant Rules:\n\
            1. You MUST output your response ONLY as a JSON object with two keys: 'sql' containing the valid {} statement, and 'explanation' containing a brief 1-sentence reasoning in Chinese.\n\
            2. Do not wrap it in markdown blocks, just return the raw JSON.\n\
            3. Use standard {} syntax.", db_type, db_type)
        );

        let messages = vec![
            ChatMessage {
                role: "system".into(),
                content: system_prompt,
            },
            ChatMessage {
                role: "user".into(),
                content: user_input.to_string(),
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
                .or(Some("💡 Generated using offline schema".to_string())),
            matched_rule_id: None,
        })
    }

    pub async fn generate_sql_no_schema(
        &self,
        user_input: &str,
        store: &RuleStore,
        policy: &Policy,
        db_type: &str,
    ) -> Result<IntentResult, AiError> {
        if let Some(r) = self.try_rule_fast_path(user_input, store, policy).await {
            return Ok(r);
        }

        let system_prompt = format!("You are a local AI SQL assistant. You must generate valid {} SQL statements based on the user's natural language request.\n\
\n\
Important Rules:\n\
1. You MUST output your response ONLY as a JSON object with two keys: 'sql' containing the valid {} statement, and 'explanation' containing a brief 1-sentence reasoning in Chinese.\n\
2. Do not wrap it in markdown blocks, just return the raw JSON.\n\
3. Use standard {} syntax.", db_type, db_type, db_type);

        let messages = vec![
            ChatMessage {
                role: "system".into(),
                content: system_prompt,
            },
            ChatMessage {
                role: "user".into(),
                content: user_input.to_string(),
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
                .or(Some("💡 Generated without schema context".to_string())),
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
    ) -> Result<IntentResult, AiError> {
        if let Some(r) = self.try_rule_fast_path(user_input, store, policy).await {
            return Ok(r);
        }

        let match_result = SemanticMatcher::find_best_match_with_thresholds(
            user_input,
            store,
            policy.rule_direct_threshold,
            policy.rule_suggest_threshold,
        );

        let mut system_prompt = if let MatchResult::SuggestionMatch { rule, confidence } =
            match_result
        {
            tracing::info!(
                "Rule Suggested ({:?}, conf: {:.2}): {}",
                rule.rule_type,
                confidence,
                rule.prompt_pattern
            );
            format!(
                "You are a local AI SQL assistant. You must generate valid {} SQL statements based on the user's natural language request.\n\
                \n\
                💡 HINT: The user's request is very similar to a known rule: \"{}\".\n\
                Here is the base SQL for that rule. Please use it as a starting point and modify it to match the user's new conditions:\n\
                ```sql\n\
                {}\n\
                ```\n\n",
                db_type, rule.prompt_pattern, rule.sql_template
            )
        } else {
            format!("You are a local AI SQL assistant. You must generate valid {} SQL statements based on the user's natural language request.\n\n", db_type)
        };

        // Step 2: Extract schema context if needed
        let mut schema_context = String::new();
        if let Ok(tables) = SchemaExtractor::get_tables(db_client, db_name).await {
            for table in tables {
                schema_context.push_str(&format!("Table: {}\n", table.table_name));
                if let Ok(columns) =
                    SchemaExtractor::get_columns(db_client, db_name, &table.table_name).await
                {
                    for col in columns {
                        schema_context.push_str(&format!(
                            "  - {} ({}): {}\n",
                            col.column_name,
                            col.data_type,
                            col.column_comment.unwrap_or_default()
                        ));
                    }
                }
            }
        }

        // Step 3: Build system prompt
        system_prompt.push_str(&format!(
            "Available Schema:\n\
            {}\n\
            \n\
            Important Rules:\n\
            1. You MUST output your response ONLY as a JSON object with two keys: 'sql' containing the valid {} statement, and 'explanation' containing a brief 1-sentence reasoning in Chinese.\n\
            2. Do not wrap it in markdown blocks, just return the raw JSON.\n\
            3. If the user asks for data modification (INSERT/UPDATE/DELETE), ensure it's safe.\n\
            4. Use standard {} syntax.",
            schema_context, db_type, db_type
        ));

        let messages = vec![
            ChatMessage {
                role: "system".into(),
                content: system_prompt,
            },
            ChatMessage {
                role: "user".into(),
                content: user_input.into(),
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
            matched_rule_id: None,
        })
    }
}
