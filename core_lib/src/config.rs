use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use thiserror::Error;
use tokio::fs;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON parsing error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Cannot determine home directory")]
    NoHomeDir,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum AiConnectionMode {
    #[default]
    Direct,
    Relay,
    #[serde(rename = "local_relay")]
    LocalRelay,
    Pool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum AiProvider {
    #[default]
    Openai,
    Deepseek,
    #[serde(rename = "moonshot")]
    Moonshot,
    #[serde(rename = "zhipu")]
    Zhipu,
    Anthropic,
    Custom,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AiTier {
    Fast,
    #[default]
    Balanced,
    High,
    Ultra,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PoolRotationStrategy {
    #[default]
    RoundRobin,
}

fn default_pool_cooldown_secs() -> u64 {
    60
}

fn default_pool_max_failures() -> u32 {
    3
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AiPoolConfig {
    #[serde(default)]
    pub tokens: Vec<String>,
    #[serde(default)]
    pub rotation: PoolRotationStrategy,
    #[serde(default = "default_pool_max_failures")]
    pub max_failures: u32,
    #[serde(default = "default_pool_cooldown_secs")]
    pub cooldown_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiProfile {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub provider: AiProvider,
    #[serde(default)]
    pub mode: AiConnectionMode,
    pub api_key: Option<String>,
    pub relay_url: Option<String>,
    #[serde(default)]
    pub pool: AiPoolConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiModelTier {
    pub id: String,
    pub display_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_budget_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_display: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiModel {
    pub id: String,
    #[serde(default)]
    pub provider: AiProvider,
    pub display_name: String,
    #[serde(default)]
    pub supports_tier: bool,
    #[serde(default)]
    pub max_context: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_tiers: Option<Vec<AiModelTier>>,
}

#[derive(Debug, Clone)]
pub struct ResolvedAiProfile {
    pub provider: AiProvider,
    pub mode: AiConnectionMode,
    pub api_key: Option<String>,
    pub relay_url: Option<String>,
    pub pool: AiPoolConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum DbType {
    #[serde(rename = "mysql")]
    #[default]
    MySQL,
    #[serde(rename = "mariadb")]
    MariaDB,
    #[serde(rename = "postgresql")]
    PostgreSQL,
    #[serde(rename = "sqlite")]
    SQLite,
    #[serde(rename = "sqlserver")]
    SQLServer,
    #[serde(rename = "mongodb")]
    MongoDB,
    #[serde(rename = "redis")]
    Redis,
    #[serde(rename = "oracle")]
    Oracle,
}

impl DbType {
    pub fn from_url(url: &str) -> Self {
        let u = url.to_lowercase();
        if u.starts_with("mariadb://") {
            Self::MariaDB
        } else if u.starts_with("postgres://") || u.starts_with("postgresql://") {
            Self::PostgreSQL
        } else if u.starts_with("sqlite://") {
            Self::SQLite
        } else if u.starts_with("sqlserver://") || u.starts_with("mssql://") {
            Self::SQLServer
        } else if u.starts_with("mongodb://") || u.starts_with("mongodb+srv://") {
            Self::MongoDB
        } else if u.starts_with("redis://") || u.starts_with("rediss://") {
            Self::Redis
        } else if u.starts_with("oracle://") {
            Self::Oracle
        } else {
            Self::MySQL
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            Self::MySQL => "MySQL",
            Self::MariaDB => "MariaDB",
            Self::PostgreSQL => "PostgreSQL",
            Self::SQLite => "SQLite",
            Self::SQLServer => "SQLServer",
            Self::MongoDB => "MongoDB",
            Self::Redis => "Redis",
            Self::Oracle => "Oracle",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum DbCapabilityLevel {
    #[serde(rename = "a")]
    #[default]
    A,
    #[serde(rename = "b")]
    B,
    #[serde(rename = "c")]
    C,
    #[serde(rename = "d")]
    D,
}

impl DbCapabilityLevel {
    pub fn display_name(&self) -> &'static str {
        match self {
            Self::A => "Level A",
            Self::B => "Level B",
            Self::C => "Level C",
            Self::D => "Level D",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type")]
pub enum DbConnectionSchema {
    #[serde(rename = "mysql")]
    MySQL { url: String },
    #[serde(rename = "mariadb")]
    MariaDB { url: String },
    #[serde(rename = "postgresql")]
    PostgreSQL { url: String },
    #[serde(rename = "sqlite")]
    SQLite { url: String },
    #[serde(rename = "sqlserver")]
    SQLServer { url: String },
    #[serde(rename = "mongodb")]
    MongoDB { url: String },
    #[serde(rename = "redis")]
    Redis { url: String },
    #[serde(rename = "oracle")]
    Oracle { url: String },
}

impl DbConnectionSchema {
    pub fn from_db_type(db_type: &DbType, url: &str) -> Self {
        match db_type {
            DbType::MySQL => Self::MySQL {
                url: url.to_string(),
            },
            DbType::MariaDB => Self::MariaDB {
                url: url.to_string(),
            },
            DbType::PostgreSQL => Self::PostgreSQL {
                url: url.to_string(),
            },
            DbType::SQLite => Self::SQLite {
                url: url.to_string(),
            },
            DbType::SQLServer => Self::SQLServer {
                url: url.to_string(),
            },
            DbType::MongoDB => Self::MongoDB {
                url: url.to_string(),
            },
            DbType::Redis => Self::Redis {
                url: url.to_string(),
            },
            DbType::Oracle => Self::Oracle {
                url: url.to_string(),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbConnection {
    pub id: String,
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub group_name: Option<String>,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub is_favorite: bool,
    #[serde(default)]
    pub ssh: Option<serde_json::Value>,
    #[serde(default)]
    pub ssl: Option<serde_json::Value>,
    #[serde(default)]
    pub db_type: Option<DbType>,
    #[serde(default)]
    pub capability_level: Option<DbCapabilityLevel>,
    #[serde(default)]
    pub schema: Option<DbConnectionSchema>,
    #[serde(default)]
    pub is_read_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// MySQL connection string, e.g. "mysql://user:pass@127.0.0.1:3306/db"
    /// Kept for backward compatibility
    pub db_url: Option<String>,

    #[serde(default)]
    pub db_connections: Vec<DbConnection>,

    pub active_db_id: Option<String>,

    /// AI Provider: openai, deepseek, anthropic, custom
    #[serde(default)]
    pub ai_provider: AiProvider,

    /// AI Mode: direct, relay, pool
    #[serde(default)]
    pub ai_mode: AiConnectionMode,

    /// Used for Direct and Relay mode
    pub api_key: Option<String>,

    /// Custom relay URL for Relay mode
    pub relay_url: Option<String>,

    /// Array of tokens for Pool mode
    #[serde(default)]
    pub token_pool: Vec<String>,

    /// Model name to use
    #[serde(default)]
    pub model_name: String,

    #[serde(default)]
    pub ai_profiles: Vec<AiProfile>,

    pub active_ai_profile_id: Option<String>,

    #[serde(default)]
    pub ai_models: Vec<AiModel>,

    pub active_model_id: Option<String>,

    #[serde(default = "default_active_tier")]
    pub active_tier: String,
}

fn default_active_tier() -> String {
    "balanced".to_string()
}

impl Default for AppConfig {
    fn default() -> Self {
        let default_profile = AiProfile {
            id: "default".to_string(),
            name: "Default".to_string(),
            provider: AiProvider::Openai,
            mode: AiConnectionMode::Direct,
            api_key: None,
            relay_url: None,
            pool: AiPoolConfig::default(),
        };

        let ai_models = vec![
            AiModel {
                id: "gpt-4o-mini".to_string(),
                provider: AiProvider::Openai,
                display_name: "GPT-4o mini".to_string(),
                supports_tier: true,
                max_context: 128_000,
                custom_tiers: None,
            },
            AiModel {
                id: "gpt-4o".to_string(),
                provider: AiProvider::Openai,
                display_name: "GPT-4o".to_string(),
                supports_tier: true,
                max_context: 128_000,
                custom_tiers: None,
            },
            AiModel {
                id: "deepseek-chat".to_string(),
                provider: AiProvider::Deepseek,
                display_name: "DeepSeek Chat".to_string(),
                supports_tier: true,
                max_context: 64_000,
                custom_tiers: None,
            },
            AiModel {
                id: "deepseek-reasoner".to_string(),
                provider: AiProvider::Deepseek,
                display_name: "DeepSeek Reasoner".to_string(),
                supports_tier: true,
                max_context: 64_000,
                custom_tiers: None,
            },
            AiModel {
                id: "claude-3-5-sonnet-20240620".to_string(),
                provider: AiProvider::Anthropic,
                display_name: "Claude 3.5 Sonnet".to_string(),
                supports_tier: true,
                max_context: 200_000,
                custom_tiers: None,
            },
        ];

        Self {
            db_url: None,
            db_connections: vec![],
            active_db_id: None,
            ai_provider: AiProvider::Openai,
            ai_mode: AiConnectionMode::Direct,
            api_key: None,
            relay_url: None,
            token_pool: vec![],
            model_name: "gpt-4o-mini".to_string(),
            ai_profiles: vec![default_profile],
            active_ai_profile_id: Some("default".to_string()),
            ai_models,
            active_model_id: Some("gpt-4o-mini".to_string()),
            active_tier: "balanced".to_string(),
        }
    }
}

impl AppConfig {
    /// Returns the active DB URL
    pub fn get_active_db_url(&self) -> Option<String> {
        if let Some(active_id) = &self.active_db_id {
            if let Some(conn) = self.db_connections.iter().find(|c| &c.id == active_id) {
                return Some(conn.url.clone());
            }
        }
        self.db_url.clone()
    }

    /// Returns the active DB type based on the URL scheme
    pub fn get_active_db_type(&self) -> String {
        self.get_active_db_type_enum().display_name().to_string()
    }

    pub fn get_active_db_type_enum(&self) -> DbType {
        if let Some(active_id) = &self.active_db_id {
            if let Some(conn) = self.db_connections.iter().find(|c| &c.id == active_id) {
                if let Some(t) = &conn.db_type {
                    return t.clone();
                }
                return DbType::from_url(&conn.url);
            }
        }
        let url = self.get_active_db_url().unwrap_or_default();
        DbType::from_url(&url)
    }

    pub fn resolve_ai_profile(&self) -> ResolvedAiProfile {
        if let Some(active_id) = &self.active_ai_profile_id {
            if let Some(p) = self.ai_profiles.iter().find(|p| &p.id == active_id) {
                return ResolvedAiProfile {
                    provider: p.provider.clone(),
                    mode: p.mode.clone(),
                    api_key: p.api_key.clone(),
                    relay_url: p.relay_url.clone(),
                    pool: p.pool.clone(),
                };
            }
        }

        ResolvedAiProfile {
            provider: self.ai_provider.clone(),
            mode: self.ai_mode.clone(),
            api_key: self.api_key.clone(),
            relay_url: self.relay_url.clone(),
            pool: AiPoolConfig {
                tokens: self.token_pool.clone(),
                ..AiPoolConfig::default()
            },
        }
    }

    pub fn resolve_active_model(&self) -> (String, Option<AiModel>) {
        if let Some(active_id) = &self.active_model_id {
            if let Some(m) = self.ai_models.iter().find(|m| &m.id == active_id) {
                return (m.id.clone(), Some(m.clone()));
            }
        }

        (self.model_name.clone(), None)
    }

    fn ensure_defaults(&mut self) {
        if self.ai_profiles.is_empty() {
            self.ai_profiles.push(AiProfile {
                id: "default".to_string(),
                name: "Default".to_string(),
                provider: self.ai_provider.clone(),
                mode: self.ai_mode.clone(),
                api_key: self.api_key.clone(),
                relay_url: self.relay_url.clone(),
                pool: AiPoolConfig {
                    tokens: self.token_pool.clone(),
                    ..AiPoolConfig::default()
                },
            });
        }
        if self.active_ai_profile_id.is_none() {
            if let Some(first) = self.ai_profiles.first() {
                self.active_ai_profile_id = Some(first.id.clone());
            }
        }

        if self.ai_models.is_empty() {
            self.ai_models = AppConfig::default().ai_models;
        }
        if self.active_model_id.is_none() {
            if self.ai_models.iter().any(|m| m.id == self.model_name) {
                self.active_model_id = Some(self.model_name.clone());
            } else if let Some(first) = self.ai_models.first() {
                self.active_model_id = Some(first.id.clone());
            }
        }

        for c in &mut self.db_connections {
            let t = c
                .db_type
                .clone()
                .unwrap_or_else(|| DbType::from_url(&c.url));
            c.db_type = Some(t.clone());
            if c.capability_level.is_none() {
                c.capability_level = Some(crate::db_capability::capability_level(&t));
            }
            if c.schema.is_none() {
                c.schema = Some(DbConnectionSchema::from_db_type(&t, &c.url));
            }
        }
    }

    pub fn normalize(mut self) -> Self {
        self.ensure_defaults();
        self
    }

    pub fn redacted_for_client(&self) -> Self {
        let mut cfg = self.clone();

        cfg.api_key = None;
        cfg.token_pool = vec![];

        for p in &mut cfg.ai_profiles {
            p.api_key = None;
            p.pool.tokens = vec![];
        }

        cfg.db_url = cfg.db_url.as_ref().map(|u| redact_url_password(u));
        for c in &mut cfg.db_connections {
            c.url = redact_url_password(&c.url);
        }

        cfg
    }

    pub fn merge_secrets_from(&mut self, prev: &AppConfig) {
        self.api_key = merge_secret_opt(self.api_key.clone(), prev.api_key.clone());

        if self.token_pool.is_empty() && !prev.token_pool.is_empty() {
            self.token_pool = prev.token_pool.clone();
        }

        if let (Some(new_url), Some(old_url)) = (self.db_url.clone(), prev.db_url.clone()) {
            self.db_url = Some(restore_url_password(&new_url, &old_url));
        }

        if !self.db_connections.is_empty() && !prev.db_connections.is_empty() {
            for c in &mut self.db_connections {
                if let Some(old) = prev.db_connections.iter().find(|p| p.id == c.id) {
                    c.url = restore_url_password(&c.url, &old.url);
                }
            }
        }

        if !self.ai_profiles.is_empty() && !prev.ai_profiles.is_empty() {
            for p in &mut self.ai_profiles {
                if let Some(old) = prev.ai_profiles.iter().find(|op| op.id == p.id) {
                    p.api_key = merge_secret_opt(p.api_key.clone(), old.api_key.clone());
                    if p.pool.tokens.is_empty() && !old.pool.tokens.is_empty() {
                        p.pool.tokens = old.pool.tokens.clone();
                    }
                }
            }
        }
    }

    /// Returns the path to the configuration file
    pub fn config_path() -> Result<PathBuf, ConfigError> {
        let home = dirs::home_dir().ok_or(ConfigError::NoHomeDir)?;
        let dir = home.join(".local-ai-sql");
        Ok(dir.join("config.json"))
    }

    /// Loads the configuration from the local file
    pub async fn load() -> Result<Self, ConfigError> {
        let path = Self::config_path()?;
        if !path.exists() {
            // Return default config if file doesn't exist
            return Ok(AppConfig::default());
        }

        let content = fs::read_to_string(&path).await?;
        let mut config: AppConfig = serde_json::from_str(&content)?;
        config.ensure_defaults();
        Ok(config)
    }

    /// Saves the configuration to the local file
    pub async fn save(&self) -> Result<(), ConfigError> {
        let path = Self::config_path()?;
        if let Some(parent) = path.parent() {
            if !parent.exists() {
                fs::create_dir_all(parent).await?;
            }
        }

        let content = serde_json::to_string_pretty(self)?;
        fs::write(&path, content).await?;
        Ok(())
    }
}

fn merge_secret_opt(new: Option<String>, old: Option<String>) -> Option<String> {
    match new {
        Some(v) if !v.trim().is_empty() && v.trim() != "******" => Some(v),
        _ => old,
    }
}

fn redact_url_password(url: &str) -> String {
    let (prefix, rest) = match url.split_once("://") {
        Some(v) => v,
        None => return url.to_string(),
    };

    let at_idx = match rest.find('@') {
        Some(v) => v,
        None => return url.to_string(),
    };

    let userinfo = &rest[..at_idx];
    let after = &rest[at_idx..];
    let Some((user, _pass)) = userinfo.split_once(':') else {
        return url.to_string();
    };

    format!("{}://{}:{}{}", prefix, user, "******", after)
}

fn restore_url_password(new_url: &str, old_url: &str) -> String {
    if !new_url.contains(":******@") {
        return new_url.to_string();
    }

    let (new_prefix, new_rest) = match new_url.split_once("://") {
        Some(v) => v,
        None => return new_url.to_string(),
    };
    let (old_prefix, old_rest) = match old_url.split_once("://") {
        Some(v) => v,
        None => return new_url.to_string(),
    };
    if new_prefix != old_prefix {
        return new_url.to_string();
    }

    let new_at = match new_rest.find('@') {
        Some(v) => v,
        None => return new_url.to_string(),
    };
    let old_at = match old_rest.find('@') {
        Some(v) => v,
        None => return new_url.to_string(),
    };

    let new_userinfo = &new_rest[..new_at];
    let old_userinfo = &old_rest[..old_at];

    let Some((new_user, new_pass)) = new_userinfo.split_once(':') else {
        return new_url.to_string();
    };
    let Some((old_user, old_pass)) = old_userinfo.split_once(':') else {
        return new_url.to_string();
    };

    if new_user != old_user || new_pass != "******" {
        return new_url.to_string();
    }

    format!(
        "{}://{}:{}@{}",
        new_prefix,
        old_user,
        old_pass,
        &new_rest[new_at + 1..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacted_for_client_strips_secrets_and_masks_passwords() {
        let cfg = AppConfig {
            api_key: Some("k".to_string()),
            token_pool: vec!["t1".to_string()],
            db_url: Some("mysql://u:p@127.0.0.1:3306/db".to_string()),
            db_connections: vec![DbConnection {
                id: "db1".to_string(),
                name: "db1".to_string(),
                url: "postgres://u2:p2@127.0.0.1:5432/db".to_string(),
                group_name: None,
                color: None,
                is_favorite: false,
                ssh: None,
                ssl: None,
                db_type: None,
                capability_level: None,
                schema: None,
                is_read_only: false,
            }],
            ai_profiles: vec![AiProfile {
                id: "p".to_string(),
                name: "p".to_string(),
                provider: AiProvider::Openai,
                mode: AiConnectionMode::Direct,
                api_key: Some("k2".to_string()),
                relay_url: None,
                pool: AiPoolConfig {
                    tokens: vec!["t2".to_string()],
                    ..AiPoolConfig::default()
                },
            }],
            active_ai_profile_id: Some("p".to_string()),
            ..AppConfig::default()
        };

        let redacted = cfg.redacted_for_client();
        assert!(redacted.api_key.is_none());
        assert!(redacted.token_pool.is_empty());
        assert_eq!(
            redacted.db_url.as_deref(),
            Some("mysql://u:******@127.0.0.1:3306/db")
        );
        assert_eq!(
            redacted.db_connections[0].url.as_str(),
            "postgres://u2:******@127.0.0.1:5432/db"
        );
        assert!(redacted.ai_profiles[0].api_key.is_none());
        assert!(redacted.ai_profiles[0].pool.tokens.is_empty());
    }

    #[test]
    fn merge_secrets_from_restores_masked_password_and_missing_keys() {
        let prev = AppConfig {
            api_key: Some("prev-key".to_string()),
            db_url: Some("mysql://u:prevpass@127.0.0.1:3306/db".to_string()),
            ai_profiles: vec![AiProfile {
                id: "p".to_string(),
                name: "p".to_string(),
                provider: AiProvider::Openai,
                mode: AiConnectionMode::Direct,
                api_key: Some("prev-profile-key".to_string()),
                relay_url: None,
                pool: AiPoolConfig {
                    tokens: vec!["prev-token".to_string()],
                    ..AiPoolConfig::default()
                },
            }],
            ..AppConfig::default()
        };

        let mut incoming = prev.redacted_for_client();
        incoming.api_key = None;
        incoming.db_url = Some("mysql://u:******@127.0.0.1:3306/db".to_string());
        incoming.ai_profiles[0].api_key = None;
        incoming.ai_profiles[0].pool.tokens = vec![];

        incoming.merge_secrets_from(&prev);

        assert_eq!(incoming.api_key.as_deref(), Some("prev-key"));
        assert_eq!(
            incoming.db_url.as_deref(),
            Some("mysql://u:prevpass@127.0.0.1:3306/db")
        );
        assert_eq!(
            incoming.ai_profiles[0].api_key.as_deref(),
            Some("prev-profile-key")
        );
        assert_eq!(incoming.ai_profiles[0].pool.tokens, vec!["prev-token"]);
    }
}
