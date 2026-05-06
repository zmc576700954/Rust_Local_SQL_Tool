use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use thiserror::Error;
use tokio::fs;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum KnowledgeError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON parsing error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Cannot determine home directory")]
    NoHomeDir,
    #[error("Knowledge not found: {0}")]
    NotFound(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum KnowledgeType {
    /// Table structures and comments
    Ddl,
    /// Business rules and nomenclature
    Documentation,
    /// High quality golden SQL examples
    Sql,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Knowledge {
    #[serde(default = "default_id")]
    pub id: String,
    pub knowledge_type: KnowledgeType,
    /// Which database connection this belongs to. If empty or none, it's global.
    pub db_connection_id: Option<String>,
    /// The title or summary of this knowledge
    pub title: String,
    /// The actual content (DDL string, documentation text, or SQL query)
    pub content: String,
    /// For SQL type, this could be the natural language question it answers
    pub description: Option<String>,
    /// When the knowledge was created or last updated (timestamp).
    pub updated_at: i64,
    /// Whether this is a high-quality example meant to be fed to AI.
    #[serde(default)]
    pub is_golden: bool,
}

fn default_id() -> String {
    Uuid::new_v4().to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct KnowledgeBase {
    pub items: Vec<Knowledge>,
}

impl KnowledgeBase {
    /// Returns the path to the knowledge base configuration file
    pub fn store_path() -> Result<PathBuf, KnowledgeError> {
        let home = dirs::home_dir().ok_or(KnowledgeError::NoHomeDir)?;
        let dir = home.join(".local-ai-sql");
        Ok(dir.join("knowledge_base.json"))
    }

    /// Loads the knowledge base from the local file
    pub async fn load() -> Result<Self, KnowledgeError> {
        let path = Self::store_path()?;
        if !path.exists() {
            return Ok(KnowledgeBase::default());
        }

        let content = fs::read_to_string(&path).await?;
        let store: KnowledgeBase = serde_json::from_str(&content)?;
        Ok(store)
    }

    /// Saves the knowledge base to the local file using an atomic write
    pub async fn save(&self) -> Result<(), KnowledgeError> {
        let path = Self::store_path()?;
        if let Some(parent) = path.parent() {
            if !parent.exists() {
                fs::create_dir_all(parent).await?;
            }
        }

        let content = serde_json::to_string_pretty(self)?;

        let tmp_path = path.with_extension("json.tmp");
        fs::write(&tmp_path, content).await?;

        if let Ok(file) = fs::File::open(&tmp_path).await {
            let _ = file.sync_all().await;
        }

        fs::rename(&tmp_path, &path).await?;
        Ok(())
    }

    /// Adds a new knowledge item
    pub fn add_item(&mut self, mut item: Knowledge) {
        if item.id.is_empty() {
            item.id = Uuid::new_v4().to_string();
        }
        item.updated_at = chrono::Utc::now().timestamp();
        self.items.push(item);
    }

    /// Updates an existing knowledge item
    pub fn update_item(&mut self, mut updated_item: Knowledge) -> Result<(), KnowledgeError> {
        if let Some(idx) = self.items.iter().position(|i| i.id == updated_item.id) {
            updated_item.updated_at = chrono::Utc::now().timestamp();
            self.items[idx] = updated_item;
            Ok(())
        } else {
            Err(KnowledgeError::NotFound(updated_item.id))
        }
    }

    /// Deletes a knowledge item by ID
    pub fn delete_item(&mut self, id: &str) -> Result<(), KnowledgeError> {
        let len_before = self.items.len();
        self.items.retain(|i| i.id != id);
        if self.items.len() < len_before {
            Ok(())
        } else {
            Err(KnowledgeError::NotFound(id.to_string()))
        }
    }

    /// Retrieve knowledge relevant to the query
    /// A simple keyword matching algorithm
    pub fn retrieve(
        &self,
        db_connection_id: Option<&str>,
        query: &str,
        limit: usize,
    ) -> Vec<Knowledge> {
        let query_lower = query.to_lowercase();
        // Extract words (simple splitting by whitespace)
        let keywords: Vec<&str> = query_lower.split_whitespace().collect();

        let mut scored_items: Vec<(&Knowledge, usize)> = self
            .items
            .iter()
            .filter(|i| {
                // Filter by connection ID if provided
                let conn_match = if let Some(conn_id) = db_connection_id {
                    i.db_connection_id.as_deref() == Some(conn_id) || i.db_connection_id.is_none()
                } else {
                    i.db_connection_id.is_none()
                };

                // For SQL type, only retrieve if it's golden
                let is_valid_sql = i.knowledge_type != KnowledgeType::Sql || i.is_golden;

                conn_match && is_valid_sql
            })
            .map(|item| {
                let mut score = 0;
                let title_lower = item.title.to_lowercase();
                let content_lower = item.content.to_lowercase();
                let desc_lower = item.description.as_deref().unwrap_or("").to_lowercase();

                for kw in &keywords {
                    if title_lower.contains(kw) {
                        score += 3;
                    }
                    if desc_lower.contains(kw) {
                        score += 2;
                    }
                    if content_lower.contains(kw) {
                        score += 1;
                    }
                }

                // If it's a DDL or Documentation, we might want to include it anyway if it's small,
                // but let's stick to keyword scoring. We give a small base score to Documentation
                // so it can be included if there's no better match.
                if score == 0 && item.knowledge_type == KnowledgeType::Documentation {
                    score = 1; // Base score to ensure some context if nothing matches
                }

                (item, score)
            })
            .filter(|(_, score)| *score > 0)
            .collect();

        // Sort by score descending
        scored_items.sort_by(|a, b| b.1.cmp(&a.1));

        scored_items
            .into_iter()
            .take(limit)
            .map(|(i, _)| i.clone())
            .collect()
    }
}
