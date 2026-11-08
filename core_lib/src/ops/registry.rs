//! 运维场景注册表与统一上下文。

use serde::{Deserialize, Serialize};

use crate::db::client::DbClient;
use crate::db::database_ops::DatabaseOps;
use crate::db::protocol::BoxFuture;
use crate::ops::error::OpsError;
use crate::ops::policy::OpsPolicy;
use crate::ops::types::{hash_json, EvidencePack, OpsPlan, RiskClass};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScenarioStatus {
    Enabled,
    ExperimentalReadOnly,
    Disabled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioDescriptor {
    pub id: String,
    pub title: String,
    pub description: String,
    pub engines: Vec<String>,
    pub status: ScenarioStatus,
    pub risk_ceiling: RiskClass,
    /// 该场景可自动执行的动作白名单。
    pub allowed_actions: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct OpsContext {
    pub db_client: DbClient,
    pub db_id: String,
    pub db_name: String,
    pub db_fingerprint: String,
    pub is_read_only: bool,
}

impl OpsContext {
    pub fn new(db_client: DbClient, db_id: String, db_name: String, is_read_only: bool) -> Self {
        let fingerprint = hash_json(&serde_json::json!({
            "db_type": format!("{:?}", db_client.db_type()),
            "db_id": db_id,
            "db_name": db_name,
        }));
        Self {
            db_client,
            db_id,
            db_name,
            db_fingerprint: fingerprint,
            is_read_only,
        }
    }

    pub fn is_mysql_family(&self) -> bool {
        matches!(
            self.db_client.db_type(),
            crate::config::DbType::MySQL | crate::config::DbType::MariaDB
        )
    }

    pub fn identity(&self) -> PlanIdentity {
        PlanIdentity {
            db_id: self.db_id.clone(),
            db_name: self.db_name.clone(),
            db_fingerprint: self.db_fingerprint.clone(),
            is_read_only: self.is_read_only,
        }
    }
}

/// 计划生成所需的数据库身份信息（不包含连接池，便于单测和纯函数化）。
#[derive(Debug, Clone)]
pub struct PlanIdentity {
    pub db_id: String,
    pub db_name: String,
    pub db_fingerprint: String,
    pub is_read_only: bool,
}

/// 场景实现。输入/输出都用统一计划模型；场景内部可以自由做证据采集。
pub trait OpsScenario: Send + Sync {
    fn descriptor(&self) -> &ScenarioDescriptor;

    fn collect_evidence<'a>(
        &'a self,
        ctx: &'a OpsContext,
        input: &'a serde_json::Value,
    ) -> BoxFuture<'a, Result<EvidencePack, OpsError>>;

    fn build_plan(
        &self,
        identity: &PlanIdentity,
        evidence: &EvidencePack,
        input: &serde_json::Value,
        policy: &OpsPolicy,
    ) -> Result<OpsPlan, OpsError>;
}

pub struct ScenarioRegistry {
    scenarios: Vec<Box<dyn OpsScenario>>,
    descriptors: Vec<ScenarioDescriptor>,
}

impl ScenarioRegistry {
    pub fn builtin() -> Self {
        let index_advisor = super::index_advisor::IndexAdvisorScenario::new();
        let descriptors = vec![
            index_advisor.descriptor().clone(),
            // 首期仅开放 MySQL。PG/SQLite 标为实验性，继续只读，不出计划。
            ScenarioDescriptor {
                id: "pg-index-advisor".into(),
                title: "PostgreSQL 索引优化（实验性）".into(),
                description: "首期仅支持 MySQL/MariaDB；PG 仅保留只读实验入口。".into(),
                engines: vec!["postgres".into()],
                status: ScenarioStatus::ExperimentalReadOnly,
                risk_ceiling: RiskClass::L0,
                allowed_actions: vec![],
            },
            ScenarioDescriptor {
                id: "sqlite-index-advisor".into(),
                title: "SQLite 索引优化（实验性）".into(),
                description: "首期仅支持 MySQL/MariaDB；SQLite 仅保留只读实验入口。".into(),
                engines: vec!["sqlite".into()],
                status: ScenarioStatus::ExperimentalReadOnly,
                risk_ceiling: RiskClass::L0,
                allowed_actions: vec![],
            },
            ScenarioDescriptor {
                id: "perf-diagnostics".into(),
                title: "性能诊断（阶段 1 规划）".into(),
                description: "阶段 0 不实现性能诊断。".into(),
                engines: vec![],
                status: ScenarioStatus::Disabled,
                risk_ceiling: RiskClass::L0,
                allowed_actions: vec![],
            },
            ScenarioDescriptor {
                id: "periodic-inspection".into(),
                title: "定期巡检（阶段 2 规划）".into(),
                description: "阶段 0 不实现定期巡检。".into(),
                engines: vec![],
                status: ScenarioStatus::Disabled,
                risk_ceiling: RiskClass::L0,
                allowed_actions: vec![],
            },
            ScenarioDescriptor {
                id: "field-governance".into(),
                title: "字段治理（后续阶段规划）".into(),
                description: "阶段 0 不实现字段治理 / JSON 拆表迁移。".into(),
                engines: vec![],
                status: ScenarioStatus::Disabled,
                risk_ceiling: RiskClass::L0,
                allowed_actions: vec![],
            },
        ];
        Self {
            scenarios: vec![Box::new(index_advisor)],
            descriptors,
        }
    }

    pub fn descriptors(&self) -> &[ScenarioDescriptor] {
        &self.descriptors
    }

    pub fn find(&self, id: &str) -> Option<&dyn OpsScenario> {
        self.scenarios
            .iter()
            .map(|s| s.as_ref())
            .find(|s| s.descriptor().id == id)
    }
}

impl Default for ScenarioRegistry {
    fn default() -> Self {
        Self::builtin()
    }
}
