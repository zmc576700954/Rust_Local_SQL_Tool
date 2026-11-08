//! ops 运行时：场景注册表、计划/审批存储、策略、审计与本机令牌。

use std::path::{Path, PathBuf};
use tokio::sync::RwLock;

use crate::ops::approval::ApprovalStore;
use crate::ops::audit::AuditLog;
use crate::ops::local_token::LocalToken;
use crate::ops::plan_store::PlanStore;
use crate::ops::policy::{default_policy_path, OpsPolicy};
use crate::ops::registry::ScenarioRegistry;

pub struct OpsRuntime {
    pub registry: ScenarioRegistry,
    pub plans: RwLock<PlanStore>,
    pub approvals: RwLock<ApprovalStore>,
    pub policy: RwLock<OpsPolicy>,
    pub audit: AuditLog,
    pub local_token: LocalToken,
    pub data_dir: PathBuf,
    pub policy_path: PathBuf,
}

impl OpsRuntime {
    pub fn init_default() -> Self {
        let dir = default_data_dir();
        let _ = std::fs::create_dir_all(&dir);
        let policy_path = default_policy_path(&dir);
        let policy = OpsPolicy::load_or_default(&policy_path).with_env_overrides();
        Self {
            registry: ScenarioRegistry::builtin(),
            plans: RwLock::new(PlanStore::new(Some(dir.join("plans.json")))),
            approvals: RwLock::new(ApprovalStore::new(Some(dir.join("approvals.json")))),
            policy: RwLock::new(policy),
            audit: AuditLog::new(Some(dir.join("audit.jsonl"))),
            local_token: LocalToken::init(&dir),
            data_dir: dir,
            policy_path,
        }
    }

    pub fn new(data_dir: PathBuf, policy: OpsPolicy, token: LocalToken) -> Self {
        Self {
            registry: ScenarioRegistry::builtin(),
            plans: RwLock::new(PlanStore::new(Some(data_dir.join("plans.json")))),
            approvals: RwLock::new(ApprovalStore::new(Some(data_dir.join("approvals.json")))),
            policy: RwLock::new(policy),
            audit: AuditLog::new(Some(data_dir.join("audit.jsonl"))),
            local_token: token,
            policy_path: default_policy_path(&data_dir),
            data_dir,
        }
    }

    /// 单元测试用的纯内存运行时，不落盘。
    pub fn for_test() -> Self {
        Self {
            registry: ScenarioRegistry::builtin(),
            plans: RwLock::new(PlanStore::new(None)),
            approvals: RwLock::new(ApprovalStore::new(None)),
            policy: RwLock::new(OpsPolicy::default()),
            audit: AuditLog::new(None),
            local_token: LocalToken::for_test("test-ops-token"),
            data_dir: PathBuf::new(),
            policy_path: PathBuf::new(),
        }
    }

    pub async fn save_policy(&self, policy: &OpsPolicy) -> Result<(), crate::ops::error::OpsError> {
        if !self.policy_path.as_os_str().is_empty() {
            policy.save(&self.policy_path)?;
        }
        *self.policy.write().await = policy.clone();
        Ok(())
    }
}

pub fn default_data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("LOCAL_AI_SQL_OPS_DIR") {
        let dir = dir.trim();
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    dirs::home_dir()
        .map(|home| home.join(".local-ai-sql").join("ops"))
        .unwrap_or_else(|| Path::new("/tmp").join("local-ai-sql-ops"))
}
