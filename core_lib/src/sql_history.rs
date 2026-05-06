use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use thiserror::Error;
use tokio::fs;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON parsing error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Cannot determine home directory")]
    NoHomeDir,
    #[error("Not found: {0}")]
    NotFound(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SqlHistory {
    #[serde(default = "default_id")]
    pub id: String,
    pub sql: String,
    pub status: String, // "success", "error"
    pub execution_time_ms: u64,
    pub executed_at: i64,
}

fn default_id() -> String {
    Uuid::new_v4().to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SqlHistoryStoreData {
    pub history: Vec<SqlHistory>,
}

#[derive(Debug, Clone, Default)]
pub struct SqlHistoryStore {
    pub data: SqlHistoryStoreData,
}

impl SqlHistoryStore {
    pub fn store_path() -> Result<PathBuf, StoreError> {
        let home = dirs::home_dir().ok_or(StoreError::NoHomeDir)?;
        let dir = home.join(".local-ai-sql");
        Ok(dir.join("sql_history.json"))
    }

    pub async fn load() -> Result<Self, StoreError> {
        let path = Self::store_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = fs::read_to_string(&path).await?;
        let data: SqlHistoryStoreData = serde_json::from_str(&content)?;
        Ok(Self { data })
    }

    pub async fn save(&self) -> Result<(), StoreError> {
        let path = Self::store_path()?;
        if let Some(parent) = path.parent() {
            if !parent.exists() {
                fs::create_dir_all(parent).await?;
            }
        }
        let content = serde_json::to_string_pretty(&self.data)?;
        let tmp_path = path.with_extension("json.tmp");
        fs::write(&tmp_path, content).await?;
        fs::rename(&tmp_path, &path).await?;
        Ok(())
    }

    pub fn add_history(&mut self, mut history: SqlHistory) {
        if history.id.is_empty() {
            history.id = Uuid::new_v4().to_string();
        }
        if history.executed_at == 0 {
            history.executed_at = Utc::now().timestamp();
        }
        self.data.history.push(history);
        // keep only the last 100 history items
        if self.data.history.len() > 100 {
            self.data.history.remove(0);
        }
    }

    pub fn clear_history(&mut self) {
        self.data.history.clear();
    }
}
