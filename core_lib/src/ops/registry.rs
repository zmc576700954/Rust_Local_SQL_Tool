//! 运维场景注册表与统一上下文。

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::db::client::DbClient;
use crate::db::database_ops::DatabaseOps;
use crate::db::protocol::BoxFuture;
use crate::ops::audit::AuditLog;
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
    /// 审计日志；性能诊断的 EXPLAIN ANALYZE 真实执行会写入这里。
    pub audit: Option<Arc<AuditLog>>,
    /// 调用方会话标识，写入审计。
    pub session_id: Option<String>,
    /// 本次请求实际生效的运维策略。字段治理等需要在证据采集阶段
    /// 使用策略阈值（如 full 剖面上限）；其他场景可忽略。
    pub policy: Option<Arc<OpsPolicy>>,
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
            audit: None,
            session_id: None,
            policy: None,
        }
    }

    pub fn with_audit(mut self, audit: Option<Arc<AuditLog>>, session_id: Option<String>) -> Self {
        self.audit = audit;
        self.session_id = session_id;
        self
    }

    pub fn with_policy(mut self, policy: Arc<OpsPolicy>) -> Self {
        self.policy = Some(policy);
        self
    }

    pub fn is_mysql_family(&self) -> bool {
        matches!(
            self.db_client.db_type(),
            crate::config::DbType::MySQL | crate::config::DbType::MariaDB
        )
    }

    pub fn is_postgres(&self) -> bool {
        matches!(self.db_client.db_type(), crate::config::DbType::PostgreSQL)
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
        let perf_diagnostics = super::perf_diagnostics::PerfDiagnosticsScenario::new();
        let field_governance = super::field_migration::FieldGovernanceScenario::new();
        let descriptors = vec![
            index_advisor.descriptor().clone(),
            perf_diagnostics.descriptor().clone(),
            field_governance.descriptor().clone(),
            // SQLite 仍为实验性；PostgreSQL 已并入「单表索引优化」的只读分析。
            ScenarioDescriptor {
                id: "sqlite-index-advisor".into(),
                title: "SQLite 索引优化（实验性）".into(),
                description: "SQLite 仅保留只读实验入口；当前不生成可执行动作。".into(),
                engines: vec!["sqlite".into()],
                status: ScenarioStatus::ExperimentalReadOnly,
                risk_ceiling: RiskClass::L0,
                allowed_actions: vec![],
            },
            ScenarioDescriptor {
                id: "periodic-inspection".into(),
                title: "定期巡检（阶段 1）".into(),
                description: "规则型只读检查 + 应用内调度；入口在运维中心巡检页。".into(),
                engines: vec!["mysql".into(), "mariadb".into(), "postgresql".into()],
                status: ScenarioStatus::Enabled,
                risk_ceiling: RiskClass::L0,
                allowed_actions: vec![],
            },
        ];
        Self {
            scenarios: vec![
                Box::new(index_advisor),
                Box::new(perf_diagnostics),
                Box::new(field_governance),
            ],
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
