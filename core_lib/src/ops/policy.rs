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
    /// 字段治理：full 剖面允许的最大估算行数；超出自动降为 sampled。
    #[serde(default = "default_field_profile_max_rows")]
    pub field_profile_max_rows: u64,
    /// 字段治理：full 剖面允许的最大数据量（字节）。
    #[serde(default = "default_field_profile_max_bytes")]
    pub field_profile_max_bytes: u64,
    /// 字段治理：自动回填（S2 起）允许的最大行数；S1 仅用于风险分级。
    #[serde(default = "default_field_migration_max_rows")]
    pub field_migration_max_rows: u64,
    /// 字段治理：自动回填允许的最大数据量（字节）。
    #[serde(default = "default_field_migration_max_bytes")]
    pub field_migration_max_bytes: u64,
    /// 字段治理：sampled 模式的确定性抽样比例（千分比）。
    #[serde(default = "default_field_sample_per_mille")]
    pub field_sample_per_mille: u32,
    /// 字段治理：最多分析多少个 JSON key（超过则视为结构不稳定）。
    #[serde(default = "default_field_max_json_keys")]
    pub field_max_json_keys: usize,
    /// 字段治理 S2：是否允许分批回填。默认仅在非生产环境开启。
    #[serde(default = "default_field_allow_backfill")]
    pub field_allow_backfill: bool,
    /// 字段治理 S2：生产环境回填的独立开关；默认关闭。
    #[serde(default)]
    pub field_allow_prod_backfill: bool,
    /// 字段治理 S3：下线前要求的观察期天数（默认 7 天，仅用于清单提示）。
    #[serde(default = "default_field_observation_days")]
    pub field_observation_days: u64,
    /// 字段治理 S3：旧列快照最低保留天数（默认 30 天）。
    #[serde(default = "default_field_snapshot_retention_days")]
    pub field_snapshot_retention_days: u64,
    /// 字段治理 S3：切换读之后允许回切的窗口（小时，默认 72）。
    #[serde(default = "default_field_rollback_window_hours")]
    pub field_rollback_window_hours: u64,
}

fn default_field_profile_max_rows() -> u64 {
    5_000_000
}

fn default_field_profile_max_bytes() -> u64 {
    10 * 1024 * 1024 * 1024
}

fn default_field_migration_max_rows() -> u64 {
    2_000_000
}

fn default_field_migration_max_bytes() -> u64 {
    5 * 1024 * 1024 * 1024
}

fn default_field_sample_per_mille() -> u32 {
    100
}

fn default_field_max_json_keys() -> usize {
    100
}

fn default_field_allow_backfill() -> bool {
    true
}

fn default_field_observation_days() -> u64 {
    7
}

fn default_field_snapshot_retention_days() -> u64 {
    30
}

fn default_field_rollback_window_hours() -> u64 {
    72
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
            field_profile_max_rows: default_field_profile_max_rows(),
            field_profile_max_bytes: default_field_profile_max_bytes(),
            field_migration_max_rows: default_field_migration_max_rows(),
            field_migration_max_bytes: default_field_migration_max_bytes(),
            field_sample_per_mille: default_field_sample_per_mille(),
            field_max_json_keys: default_field_max_json_keys(),
            field_allow_backfill: default_field_allow_backfill(),
            field_allow_prod_backfill: false,
            field_observation_days: default_field_observation_days(),
            field_snapshot_retention_days: default_field_snapshot_retention_days(),
            field_rollback_window_hours: default_field_rollback_window_hours(),
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
        if env_flag("LOCAL_AI_SQL_OPS_ALLOW_BACKFILL") {
            self.field_allow_backfill = true;
        }
        if env_flag("LOCAL_AI_SQL_OPS_ALLOW_PROD_BACKFILL") {
            self.field_allow_prod_backfill = true;
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

    /// 字段回填闸门：与索引一键执行相互独立。
    /// 默认仅非生产环境可用；生产必须同时开启 `field_allow_prod_backfill`
    /// 并在请求中显式确认生产环境。
    pub fn authorize_backfill(
        &self,
        read_only_connection: bool,
        production_confirmed: bool,
    ) -> Result<(), OpsError> {
        if read_only_connection {
            return Err(OpsError::Policy(
                "当前连接为只读模式，禁止执行字段回填".into(),
            ));
        }
        if !self.field_allow_backfill {
            return Err(OpsError::Policy(
                "字段回填未开启（field_allow_backfill=false）".into(),
            ));
        }
        if self.is_prod() {
            if !self.field_allow_prod_backfill {
                return Err(OpsError::Policy(
                    "生产环境默认禁止字段回填。需要显式开启 field_allow_prod_backfill。".into(),
                ));
            }
            if !production_confirmed {
                return Err(OpsError::Policy(
                    "生产环境回填需要在请求中显式确认 production_confirmed=true".into(),
                ));
            }
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

    #[test]
    fn backfill_is_enabled_by_default_only_outside_prod() {
        let policy = OpsPolicy::default();
        assert!(policy.authorize_backfill(false, false).is_ok());
        assert!(policy.authorize_backfill(true, false).is_err());

        let mut prod = OpsPolicy {
            environment: "prod".into(),
            ..Default::default()
        };
        assert!(prod.authorize_backfill(false, false).is_err());
        prod.field_allow_prod_backfill = true;
        // 生产开关打开后仍要求请求侧显式确认。
        assert!(prod.authorize_backfill(false, false).is_err());
        assert!(prod.authorize_backfill(false, true).is_ok());

        let mut disabled = OpsPolicy::default();
        disabled.field_allow_backfill = false;
        assert!(disabled.authorize_backfill(false, false).is_err());
    }
}
