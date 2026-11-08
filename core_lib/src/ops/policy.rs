//! ops 执行策略：默认只读 + dry-run，生产环境必须显式开启才允许落地。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::ops::error::OpsError;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpsPolicy {
    /// dev / staging / prod
    pub environment: String,
    /// 是否允许真正执行写动作；默认 false（只读 + dry-run）。
    pub allow_apply: bool,
    /// 生产环境是否允许落地；默认 false。
    pub allow_prod_apply: bool,
    /// 审批令牌有效期（秒）。
    pub approval_ttl_secs: u64,
    /// 单表估算行数超过该值时拒绝执行，除非 allow_large_table。
    pub max_affected_rows: u64,
    /// 允许大表执行（仍会做影响评估并写审计）。
    pub allow_large_table: bool,
    /// 单条 DDL 的执行超时（秒）。
    pub ddl_timeout_secs: u64,
    /// MySQL 在线 DDL 的 lock_wait_timeout（秒）。
    pub lock_wait_timeout_secs: u64,
}

impl Default for OpsPolicy {
    fn default() -> Self {
        Self {
            environment: "dev".into(),
            allow_apply: false,
            allow_prod_apply: false,
            approval_ttl_secs: 300,
            max_affected_rows: 10_000_000,
            allow_large_table: false,
            ddl_timeout_secs: 600,
            lock_wait_timeout_secs: 5,
        }
    }
}

impl OpsPolicy {
    pub fn load_or_default(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(raw) => serde_json::from_str::<Self>(&raw).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), OpsError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let raw = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, raw)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// 从环境变量做受控覆盖，方便本机端到端验证。
    pub fn with_env_overrides(mut self) -> Self {
        if let Ok(value) = std::env::var("LOCAL_AI_SQL_OPS_ENV") {
            if !value.trim().is_empty() {
                self.environment = value.trim().to_lowercase();
            }
        }
        if env_flag("LOCAL_AI_SQL_OPS_ALLOW_APPLY") {
            self.allow_apply = true;
        }
        if env_flag("LOCAL_AI_SQL_OPS_ALLOW_PROD_APPLY") {
            self.allow_prod_apply = true;
        }
        self
    }

    pub fn is_prod(&self) -> bool {
        self.environment.eq_ignore_ascii_case("prod")
            || self.environment.eq_ignore_ascii_case("production")
    }

    /// 执行前的策略闸门。`dry_run` 永远允许；真实执行必须显式开启。
    pub fn authorize_apply(&self, dry_run: bool, read_only_connection: bool) -> Result<(), OpsError> {
        if dry_run {
            return Ok(());
        }
        if read_only_connection {
            return Err(OpsError::Policy(
                "当前连接为只读模式，禁止执行索引变更".into(),
            ));
        }
        if !self.allow_apply {
            return Err(OpsError::Policy(
                "一键执行未开启（默认只读 + dry-run）。请显式开启 allow_apply 后再试。".into(),
            ));
        }
        if self.is_prod() && !self.allow_prod_apply {
            return Err(OpsError::Policy(
                "生产环境默认禁止一键执行。需要显式开启 allow_prod_apply。".into(),
            ));
        }
        Ok(())
    }
}

fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

pub fn default_policy_path(dir: &Path) -> PathBuf {
    dir.join("ops-policy.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_is_read_only_and_dry_run_friendly() {
        let policy = OpsPolicy::default();
        assert!(!policy.allow_apply);
        assert!(!policy.allow_prod_apply);
        assert!(policy.authorize_apply(true, false).is_ok());
        assert!(policy.authorize_apply(false, false).is_err());
        assert!(policy.authorize_apply(false, true).is_err());
    }

    #[test]
    fn prod_requires_explicit_flag() {
        let mut policy = OpsPolicy {
            allow_apply: true,
            environment: "prod".into(),
            ..Default::default()
        };
        assert!(policy.authorize_apply(false, false).is_err());
        policy.allow_prod_apply = true;
        assert!(policy.authorize_apply(false, false).is_ok());
    }
}
