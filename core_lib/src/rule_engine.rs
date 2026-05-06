use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use thiserror::Error;
use tokio::fs;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum RuleError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON parsing error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Cannot determine home directory")]
    NoHomeDir,
    #[error("Rule not found: {0}")]
    NotFound(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum RuleType {
    /// Exact or high similarity match with parameters to be replaced.
    Template,
    /// Exact match with no parameters. Just a direct mapping.
    Module,
    /// Used as context to guide AI for partial matches.
    Suggestion,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    #[serde(default = "default_id")]
    pub id: String,
    pub rule_type: RuleType,
    /// The natural language description that triggers this rule.
    pub prompt_pattern: String,
    /// The SQL statement, potentially containing {{param}} placeholders.
    pub sql_template: String,
    /// How many times this rule has been successfully matched/used.
    pub hit_count: u32,
    /// When the rule was created or last updated (timestamp).
    pub updated_at: i64,
}

fn default_id() -> String {
    Uuid::new_v4().to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RuleStore {
    pub rules: Vec<Rule>,
}

impl RuleStore {
    /// Returns the path to the rules configuration file
    pub fn store_path() -> Result<PathBuf, RuleError> {
        let home = dirs::home_dir().ok_or(RuleError::NoHomeDir)?;
        let dir = home.join(".local-ai-sql");
        Ok(dir.join("rules.json"))
    }

    /// Loads the rules from the local file
    pub async fn load() -> Result<Self, RuleError> {
        let path = Self::store_path()?;
        if !path.exists() {
            return Ok(RuleStore::default());
        }

        let content = fs::read_to_string(&path).await?;
        let store: RuleStore = serde_json::from_str(&content)?;
        Ok(store)
    }

    /// Saves the rules to the local file using an atomic write
    pub async fn save(&self) -> Result<(), RuleError> {
        let path = Self::store_path()?;
        if let Some(parent) = path.parent() {
            if !parent.exists() {
                fs::create_dir_all(parent).await?;
            }
        }

        let content = serde_json::to_string_pretty(self)?;

        // Atomic write: write to a temporary file first, then rename
        let tmp_path = path.with_extension("json.tmp");
        fs::write(&tmp_path, content).await?;

        // Ensure data is synced to disk (optional but safer)
        if let Ok(file) = fs::File::open(&tmp_path).await {
            let _ = file.sync_all().await;
        }

        // Atomic replace
        fs::rename(&tmp_path, &path).await?;
        Ok(())
    }

    /// Adds a new rule
    pub fn add_rule(&mut self, mut rule: Rule) {
        if rule.id.is_empty() {
            rule.id = Uuid::new_v4().to_string();
        }
        self.rules.push(rule);
    }

    /// Increments the hit count for a specific rule
    pub fn increment_hit_count(&mut self, id: &str) -> bool {
        if let Some(rule) = self.rules.iter_mut().find(|r| r.id == id) {
            rule.hit_count += 1;
            true
        } else {
            false
        }
    }

    /// Updates an existing rule
    pub fn update_rule(&mut self, updated_rule: Rule) -> Result<(), RuleError> {
        if let Some(idx) = self.rules.iter().position(|r| r.id == updated_rule.id) {
            self.rules[idx] = updated_rule;
            Ok(())
        } else {
            Err(RuleError::NotFound(updated_rule.id))
        }
    }

    /// Deletes a rule by ID
    pub fn delete_rule(&mut self, id: &str) -> Result<(), RuleError> {
        let len_before = self.rules.len();
        self.rules.retain(|r| r.id != id);
        if self.rules.len() < len_before {
            Ok(())
        } else {
            Err(RuleError::NotFound(id.to_string()))
        }
    }
}
