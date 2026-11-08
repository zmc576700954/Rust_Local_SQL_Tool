//! 运维（ops）底座：场景注册、统一计划模型、结构化动作、审批、审计与执行。
//!
//! 阶段 0 只落地 MySQL 单表索引优化闭环（仅新增索引）。设计约束：
//! - Agent 只产出 `ActionSpec`，SQL 一律由服务端 `ddl_compiler` 生成并校验；
//! - 审批令牌绑定计划哈希、一次性消费；
//! - 默认只读 + dry-run，L4 危险操作硬拦截；
//! - 新链路不复用 `DdlEngine` / `execute_ddl`。

pub mod ai_planner;
pub mod approval;
pub mod audit;
pub mod ddl_compiler;
pub mod error;
pub mod executor;
pub mod index_advisor;
pub mod local_token;
pub mod plan_store;
pub mod policy;
pub mod registry;
pub mod runtime;
pub mod safety;
pub mod types;

pub use error::OpsError;
pub use policy::OpsPolicy;
pub use registry::{OpsContext, OpsScenario, PlanIdentity, ScenarioDescriptor, ScenarioRegistry};
pub use runtime::OpsRuntime;
pub use types::{
    ActionSpec, ApprovalRecord, AuditRecord, EvidencePack, ExecutionReport, OpsPlan, PlanStatus,
    RiskClass,
};
