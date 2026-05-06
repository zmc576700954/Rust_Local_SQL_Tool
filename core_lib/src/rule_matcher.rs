use crate::rule_engine::{Rule, RuleStore};
use strsim::jaro_winkler;

#[derive(Debug)]
pub enum MatchResult {
    /// High confidence match (> 0.85). Direct SQL output.
    DirectMatch {
        rule: Rule,
        confidence: f64,
        // If it's a template, this would be the populated SQL. For now, we return the raw template.
        sql: String,
    },
    /// Medium confidence match (0.6 ~ 0.85). Suggestion to guide AI.
    SuggestionMatch { rule: Rule, confidence: f64 },
    /// No significant match.
    None,
}

pub struct SemanticMatcher;

impl SemanticMatcher {
    const HIGH_THRESHOLD: f64 = 0.85;
    const SUGGEST_THRESHOLD: f64 = 0.60;

    /// Finds the best matching rule for a given natural language query.
    pub fn find_best_match(query: &str, store: &RuleStore) -> MatchResult {
        Self::find_best_match_with_thresholds(
            query,
            store,
            Self::HIGH_THRESHOLD,
            Self::SUGGEST_THRESHOLD,
        )
    }

    pub fn find_best_match_with_thresholds(
        query: &str,
        store: &RuleStore,
        high_threshold: f64,
        suggest_threshold: f64,
    ) -> MatchResult {
        if store.rules.is_empty() {
            return MatchResult::None;
        }

        let mut best_rule: Option<&Rule> = None;
        let mut highest_score = 0.0;

        for rule in &store.rules {
            // Calculate similarity using Jaro-Winkler distance
            // It gives a score between 0.0 (no match) and 1.0 (exact match)
            let score = jaro_winkler(query, &rule.prompt_pattern);

            if score > highest_score {
                highest_score = score;
                best_rule = Some(rule);
            }
        }

        if let Some(rule) = best_rule {
            if highest_score >= high_threshold {
                MatchResult::DirectMatch {
                    rule: rule.clone(),
                    confidence: highest_score,
                    sql: rule.sql_template.clone(),
                }
            } else if highest_score >= suggest_threshold {
                MatchResult::SuggestionMatch {
                    rule: rule.clone(),
                    confidence: highest_score,
                }
            } else {
                MatchResult::None
            }
        } else {
            MatchResult::None
        }
    }
}
