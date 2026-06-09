//! CrudService — 行级增删改操作
//!
//! 从 web-server/src/main.rs crud_insert / crud_update / crud_delete 提取。
//! Service 层不含 axum / tauri 依赖。

use crate::crud::{CrudManager, CrudRequest};
use crate::service::context::ServiceContext;
use crate::service::error::ServiceError;
use crate::service::schema::SchemaService;
use crate::service::workbench::WorkbenchService;

// ── 参数类型 ─────────────────────────────────────────────

#[derive(Debug, Clone, serde::Deserialize)]
pub struct CrudMutationParams {
    pub table_name: String,
    pub data: serde_json::Value,
    pub condition: Option<serde_json::Map<String, serde_json::Value>>,
    pub db_id: Option<String>,
    pub transaction_id: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct CrudDeleteParams {
    pub table_name: String,
    pub condition: serde_json::Map<String, serde_json::Value>,
    pub db_id: Option<String>,
    pub transaction_id: Option<String>,
}

/// CRUD 操作统一响应
#[derive(Debug, Clone, serde::Serialize)]
pub struct CrudResult {
    pub affected_rows: u64,
    pub transaction_state: Option<String>,
}

// ── CrudService ─────────────────────────────────────────

pub struct CrudService;

impl CrudService {
    /// 插入行
    pub async fn insert(
        ctx: &ServiceContext,
        params: CrudMutationParams,
    ) -> Result<CrudResult, ServiceError> {
        Self::enforce_not_read_only(ctx, params.db_id.as_deref()).await?;

        let (db_client, _) = SchemaService::resolve_db_client(ctx, params.db_id.as_deref()).await?;
        let transaction_id = Self::parse_transaction_id(params.transaction_id.as_deref());
        let transaction_session = if let Some(id) = transaction_id.as_deref() {
            Some(
                WorkbenchService::get_or_open_transaction_session(ctx, params.db_id.as_deref(), id, false)
                    .await?,
            )
        } else {
            None
        };

        let crud_req = CrudRequest {
            table_name: params.table_name,
            data: params.data,
            condition: params.condition,
        };

        let (affected_rows, transaction_state) = if let Some(session) = transaction_session {
            let mut guard = session.lock().await;
            guard.last_accessed = std::time::Instant::now();
            let affected = CrudManager::insert_mysql(&mut *guard.conn, &crud_req)
                .await
                .map_err(|e| ServiceError::DbWrite(e.to_string()))?;
            (affected, Some("active".into()))
        } else {
            let affected = CrudManager::insert(&db_client.pool, &crud_req)
                .await
                .map_err(|e| ServiceError::DbWrite(e.to_string()))?;
            (affected, None)
        };

        Ok(CrudResult { affected_rows, transaction_state })
    }

    /// 更新行
    pub async fn update(
        ctx: &ServiceContext,
        params: CrudMutationParams,
    ) -> Result<CrudResult, ServiceError> {
        Self::enforce_not_read_only(ctx, params.db_id.as_deref()).await?;

        let (db_client, _) = SchemaService::resolve_db_client(ctx, params.db_id.as_deref()).await?;
        let transaction_id = Self::parse_transaction_id(params.transaction_id.as_deref());
        let transaction_session = if let Some(id) = transaction_id.as_deref() {
            Some(
                WorkbenchService::get_or_open_transaction_session(ctx, params.db_id.as_deref(), id, false)
                    .await?,
            )
        } else {
            None
        };

        let crud_req = CrudRequest {
            table_name: params.table_name,
            data: params.data,
            condition: params.condition,
        };

        let (affected_rows, transaction_state) = if let Some(session) = transaction_session {
            let mut guard = session.lock().await;
            guard.last_accessed = std::time::Instant::now();
            let affected = CrudManager::update_mysql(&mut *guard.conn, &crud_req)
                .await
                .map_err(|e| ServiceError::DbWrite(e.to_string()))?;
            (affected, Some("active".into()))
        } else {
            let affected = CrudManager::update(&db_client.pool, &crud_req)
                .await
                .map_err(|e| ServiceError::DbWrite(e.to_string()))?;
            (affected, None)
        };

        Ok(CrudResult { affected_rows, transaction_state })
    }

    /// 删除行
    pub async fn delete(
        ctx: &ServiceContext,
        params: CrudDeleteParams,
    ) -> Result<CrudResult, ServiceError> {
        Self::enforce_not_read_only(ctx, params.db_id.as_deref()).await?;

        let (db_client, _) = SchemaService::resolve_db_client(ctx, params.db_id.as_deref()).await?;
        let transaction_id = Self::parse_transaction_id(params.transaction_id.as_deref());
        let transaction_session = if let Some(id) = transaction_id.as_deref() {
            Some(
                WorkbenchService::get_or_open_transaction_session(ctx, params.db_id.as_deref(), id, false)
                    .await?,
            )
        } else {
            None
        };

        let (affected_rows, transaction_state) = if let Some(session) = transaction_session {
            let mut guard = session.lock().await;
            guard.last_accessed = std::time::Instant::now();
            let affected = CrudManager::delete_mysql(&mut *guard.conn, &params.table_name, &params.condition)
                .await
                .map_err(|e| ServiceError::DbWrite(e.to_string()))?;
            (affected, Some("active".into()))
        } else {
            let affected = CrudManager::delete(&db_client.pool, &params.table_name, &params.condition)
                .await
                .map_err(|e| ServiceError::DbWrite(e.to_string()))?;
            (affected, None)
        };

        Ok(CrudResult { affected_rows, transaction_state })
    }

    // ── 内部辅助 ──────────────────────────────────────────

    async fn enforce_not_read_only(ctx: &ServiceContext, db_id: Option<&str>) -> Result<(), ServiceError> {
        if SchemaService::is_read_only_connection(ctx, db_id).await {
            return Err(ServiceError::Forbidden(
                "当前连接为只读模式，禁止执行非查询操作！".into(),
            ));
        }
        Ok(())
    }

    fn parse_transaction_id(raw: Option<&str>) -> Option<String> {
        raw.map(str::trim).filter(|t| !t.is_empty()).map(str::to_string)
    }
}