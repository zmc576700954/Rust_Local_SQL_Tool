//! AppState → ServiceContext 桥接
//!
//! 将 web-server 的 AppState 的 Arc<RwLock> 字段映射为 ServiceContext，
//! 使 handler 可以调用 service 层方法。
//! 由于 ServiceContext 持有 Arc 引用，与 AppState 共享同一底层数据，
//! 不存在额外同步开销。

#![allow(dead_code)]

use core_lib::service::context::{AppStateCompat, ServiceContext};
use crate::state::AppState;

/// 从 AppState 创建共享数据的 ServiceContext
///
/// 两者持有相同的 Arc<RwLock>，数据修改在任一侧均可见。
pub fn bridge_service_context(state: &AppState) -> ServiceContext {
    ServiceContext::from_app_state(&AppStateCompat {
        config: state.config.clone(),
        db_client: state.db_client.clone(),
        db_client_cache: state.db_client_cache.clone(),
        virtual_schema: state.virtual_schema.clone(),
        schema_cache: state.schema_cache.clone(),
        table_schema_cache: state.table_schema_cache.clone(),
        rule_store: state.rule_store.clone(),
        policy: state.policy.clone(),
        sql_history: state.sql_history.clone(),
        knowledge_base: state.knowledge_base.clone(),
        active_queries: state.active_queries.clone(),
        transaction_sessions: state.transaction_sessions.clone(),
        timeouts: state.timeouts.clone(),
        limits: state.limits.clone(),
        job_semaphore: state.job_semaphore.clone(),
    })
}