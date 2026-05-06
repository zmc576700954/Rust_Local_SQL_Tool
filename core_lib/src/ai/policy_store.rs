use serde::{Deserialize, Serialize};
use tokio::fs;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ExtractorPipelineLevel {
    JsonOnly,
    JsonThenMarkdownThenSql,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum DangerousSqlPolicy {
    Block,
    AllowWithForce,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Policy {
    pub rule_direct_threshold: f64,
    pub rule_suggest_threshold: f64,
    pub suggest_inject_max_rules: u32,
    pub llm_enabled_when_offline: bool,
    pub llm_temperature: f64,
    pub llm_max_tokens: u32,
    pub structured_output_enabled: bool,
    pub extractor_pipeline_level: ExtractorPipelineLevel,
    pub dangerous_sql_policy: DangerousSqlPolicy,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            rule_direct_threshold: 0.85,
            rule_suggest_threshold: 0.60,
            suggest_inject_max_rules: 1,
            llm_enabled_when_offline: true,
            llm_temperature: 0.1,
            llm_max_tokens: 4096,
            structured_output_enabled: true,
            extractor_pipeline_level: ExtractorPipelineLevel::JsonThenMarkdownThenSql,
            dangerous_sql_policy: DangerousSqlPolicy::AllowWithForce,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PolicyOverride {
    pub rule_direct_threshold: Option<f64>,
    pub rule_suggest_threshold: Option<f64>,
    pub suggest_inject_max_rules: Option<u32>,
    pub llm_enabled_when_offline: Option<bool>,
    pub llm_temperature: Option<f64>,
    pub llm_max_tokens: Option<u32>,
    pub structured_output_enabled: Option<bool>,
    pub extractor_pipeline_level: Option<ExtractorPipelineLevel>,
    pub dangerous_sql_policy: Option<DangerousSqlPolicy>,
}

impl PolicyOverride {
    pub fn apply_to(&self, base: &Policy) -> Policy {
        Policy {
            rule_direct_threshold: self
                .rule_direct_threshold
                .unwrap_or(base.rule_direct_threshold),
            rule_suggest_threshold: self
                .rule_suggest_threshold
                .unwrap_or(base.rule_suggest_threshold),
            suggest_inject_max_rules: self
                .suggest_inject_max_rules
                .unwrap_or(base.suggest_inject_max_rules),
            llm_enabled_when_offline: self
                .llm_enabled_when_offline
                .unwrap_or(base.llm_enabled_when_offline),
            llm_temperature: self.llm_temperature.unwrap_or(base.llm_temperature),
            llm_max_tokens: self.llm_max_tokens.unwrap_or(base.llm_max_tokens),
            structured_output_enabled: self
                .structured_output_enabled
                .unwrap_or(base.structured_output_enabled),
            extractor_pipeline_level: self
                .extractor_pipeline_level
                .clone()
                .unwrap_or_else(|| base.extractor_pipeline_level.clone()),
            dangerous_sql_policy: self
                .dangerous_sql_policy
                .clone()
                .unwrap_or_else(|| base.dangerous_sql_policy.clone()),
        }
    }
}

#[derive(Debug)]
pub enum PolicyError {
    NoHomeDir,
    Io(std::io::Error),
    Json(serde_json::Error),
}

impl From<std::io::Error> for PolicyError {
    fn from(value: std::io::Error) -> Self {
        PolicyError::Io(value)
    }
}

impl From<serde_json::Error> for PolicyError {
    fn from(value: serde_json::Error) -> Self {
        PolicyError::Json(value)
    }
}

pub struct PolicyStore;

impl PolicyStore {
    pub fn default_policy() -> Result<Policy, PolicyError> {
        let content = include_str!("../../../agents/specs/v1/policy.json");
        let p: Policy = serde_json::from_str(content)?;
        Ok(p)
    }

    pub fn override_path() -> Result<std::path::PathBuf, PolicyError> {
        let home = dirs::home_dir().ok_or(PolicyError::NoHomeDir)?;
        Ok(home.join(".local-ai-sql/agents/overrides/v1/policy.override.json"))
    }

    pub fn snapshots_dir() -> Result<std::path::PathBuf, PolicyError> {
        let home = dirs::home_dir().ok_or(PolicyError::NoHomeDir)?;
        Ok(home.join(".local-ai-sql/agents/snapshots/v1"))
    }

    pub async fn load_override() -> Result<PolicyOverride, PolicyError> {
        let path = Self::override_path()?;
        if !path.exists() {
            return Ok(PolicyOverride::default());
        }
        let content = fs::read_to_string(&path).await.unwrap_or_default();
        if content.trim().is_empty() {
            return Ok(PolicyOverride::default());
        }
        let p: PolicyOverride = serde_json::from_str(&content).unwrap_or_else(|e| {
            tracing::warn!(
                "Failed to parse policy override, falling back to default: {}",
                e
            );
            PolicyOverride::default()
        });
        Ok(p)
    }

    pub async fn save_override(override_policy: &PolicyOverride) -> Result<(), PolicyError> {
        let path = Self::override_path()?;
        if let Some(parent) = path.parent() {
            if !parent.exists() {
                fs::create_dir_all(parent).await?;
            }
        }
        let content = serde_json::to_string_pretty(override_policy)?;

        let tmp_path = path.with_extension("override.json.tmp");
        fs::write(&tmp_path, content).await?;

        // Ensure data is synced to disk (optional but safer)
        if let Ok(file) = fs::File::open(&tmp_path).await {
            let _ = file.sync_all().await;
        }

        // Atomic replace
        fs::rename(&tmp_path, &path).await?;

        Ok(())
    }

    pub async fn reset_override() -> Result<(), PolicyError> {
        let path = Self::override_path()?;
        if path.exists() {
            fs::remove_file(&path).await?;
        }
        Ok(())
    }

    /// Evolve policy based on a positive signal (e.g. Save Rule or High Hit Count).
    /// This adjusts the thresholds to rely more on rules over time.
    pub async fn evolve_policy(evolution_signal_type: &str) -> Result<Policy, PolicyError> {
        let mut overrides = Self::load_override().await?;
        let base = Self::default_policy()?;
        let mut current_effective = overrides.apply_to(&base);

        let mut changed = false;

        if evolution_signal_type == "save_rule" {
            // Decrease the direct threshold slightly so rules are easier to match directly
            let new_direct = (current_effective.rule_direct_threshold - 0.02).max(0.60);
            if (new_direct - current_effective.rule_direct_threshold).abs() > 0.001 {
                overrides.rule_direct_threshold = Some(new_direct);
                current_effective.rule_direct_threshold = new_direct;
                changed = true;
            }

            // Decrease the suggest threshold slightly so rules are easier to be suggested
            let new_suggest = (current_effective.rule_suggest_threshold - 0.02).max(0.40);
            if (new_suggest - current_effective.rule_suggest_threshold).abs() > 0.001 {
                overrides.rule_suggest_threshold = Some(new_suggest);
                current_effective.rule_suggest_threshold = new_suggest;
                changed = true;
            }
        }

        if changed {
            Self::save_override(&overrides).await?;
        }

        Ok(current_effective)
    }

    pub async fn load_effective() -> Result<Policy, PolicyError> {
        let base = Self::default_policy()?;
        let override_policy = Self::load_override().await?;
        Ok(override_policy.apply_to(&base))
    }

    pub async fn create_snapshot() -> Result<String, PolicyError> {
        let dir = Self::snapshots_dir()?;
        if !dir.exists() {
            fs::create_dir_all(&dir).await?;
        }
        let effective = Self::load_effective().await?;
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let name = format!("{}.json", ts);
        let path = dir.join(&name);
        let content = serde_json::to_string_pretty(&effective)?;
        fs::write(&path, content).await?;
        Ok(name)
    }

    pub async fn rollback_snapshot(name: &str) -> Result<(), PolicyError> {
        let dir = Self::snapshots_dir()?;
        let path = dir.join(name);
        let content = fs::read_to_string(&path).await?;
        let snapshot: Policy = serde_json::from_str(&content)?;

        let base = Self::default_policy()?;
        let override_policy = PolicyOverride {
            rule_direct_threshold: if snapshot.rule_direct_threshold != base.rule_direct_threshold {
                Some(snapshot.rule_direct_threshold)
            } else {
                None
            },
            rule_suggest_threshold: if snapshot.rule_suggest_threshold
                != base.rule_suggest_threshold
            {
                Some(snapshot.rule_suggest_threshold)
            } else {
                None
            },
            suggest_inject_max_rules: if snapshot.suggest_inject_max_rules
                != base.suggest_inject_max_rules
            {
                Some(snapshot.suggest_inject_max_rules)
            } else {
                None
            },
            llm_enabled_when_offline: if snapshot.llm_enabled_when_offline
                != base.llm_enabled_when_offline
            {
                Some(snapshot.llm_enabled_when_offline)
            } else {
                None
            },
            llm_temperature: if snapshot.llm_temperature != base.llm_temperature {
                Some(snapshot.llm_temperature)
            } else {
                None
            },
            llm_max_tokens: if snapshot.llm_max_tokens != base.llm_max_tokens {
                Some(snapshot.llm_max_tokens)
            } else {
                None
            },
            structured_output_enabled: if snapshot.structured_output_enabled
                != base.structured_output_enabled
            {
                Some(snapshot.structured_output_enabled)
            } else {
                None
            },
            extractor_pipeline_level: if snapshot.extractor_pipeline_level
                != base.extractor_pipeline_level
            {
                Some(snapshot.extractor_pipeline_level)
            } else {
                None
            },
            dangerous_sql_policy: if snapshot.dangerous_sql_policy != base.dangerous_sql_policy {
                Some(snapshot.dangerous_sql_policy)
            } else {
                None
            },
        };

        Self::save_override(&override_policy).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_override_works() {
        let base = Policy::default();
        let override_policy = PolicyOverride {
            rule_direct_threshold: Some(0.9),
            ..Default::default()
        };
        let effective = override_policy.apply_to(&base);
        assert_eq!(effective.rule_direct_threshold, 0.9);
        assert_eq!(
            effective.rule_suggest_threshold,
            base.rule_suggest_threshold
        );
    }
}
