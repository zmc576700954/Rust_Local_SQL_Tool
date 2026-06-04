use crate::ai::policy_store::Policy;
use crate::rule_engine::RuleStore;
use crate::rule_matcher::{MatchResult, SemanticMatcher};
use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::Deserialize;
use serde_json::json;

#[derive(Debug, thiserror::Error)]
#[error("Rules query error: {0}")]
pub struct RulesToolError(pub String);

#[derive(Deserialize)]
pub struct QueryRulesArgs {
    pub query: String,
}

#[derive(Clone)]
pub struct QueryRulesTool {
    rule_store: RuleStore,
    policy: Policy,
}

impl QueryRulesTool {
    pub fn new(rule_store: RuleStore, policy: Policy) -> Self {
        Self { rule_store, policy }
    }
}

impl Tool for QueryRulesTool {
    const NAME: &'static str = "query_rules";

    type Error = RulesToolError;
    type Args = QueryRulesArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "query_rules".to_string(),
            description: "Search the rule engine for matching SQL rules. Returns rules that \
                semantically match the user's query, including their prompt patterns, SQL templates, \
                and confidence scores. Use this to find proven SQL patterns before generating new SQL."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The natural language query to search for matching rules."
                    }
                },
                "required": ["query"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let match_result = SemanticMatcher::find_best_match_with_thresholds(
            &args.query,
            &self.rule_store,
            self.policy.rule_direct_threshold,
            self.policy.rule_suggest_threshold,
        );

        let result = match match_result {
            MatchResult::DirectMatch {
                rule,
                confidence,
                sql,
            } => {
                json!({
                    "match_type": "direct",
                    "confidence": confidence,
                    "rule": {
                        "id": rule.id,
                        "prompt_pattern": rule.prompt_pattern,
                        "sql_template": rule.sql_template,
                        "rule_type": format!("{:?}", rule.rule_type),
                        "hit_count": rule.hit_count,
                    },
                    "resolved_sql": sql,
                })
            }
            MatchResult::SuggestionMatch { rule, confidence } => {
                json!({
                    "match_type": "suggestion",
                    "confidence": confidence,
                    "rule": {
                        "id": rule.id,
                        "prompt_pattern": rule.prompt_pattern,
                        "sql_template": rule.sql_template,
                        "rule_type": format!("{:?}", rule.rule_type),
                        "hit_count": rule.hit_count,
                    },
                    "note": "This is a partial match. Use it as a reference, not a direct template."
                })
            }
            MatchResult::None => {
                json!({
                    "match_type": "none",
                    "note": "No matching rules found. Generate SQL from scratch using schema context."
                })
            }
        };

        serde_json::to_string_pretty(&result)
            .map_err(|e| RulesToolError(e.to_string()))
    }
}
