//! WorkbenchService — SQL 执行 / 取消 / 事务管理
//!
//! 从 web-server/src/handlers/perf.rs execute_sql 提取核心业务逻辑，
//! 从 web-server/src/main.rs 提取 execute_cancel / execute_transaction。
//! Service 层不含 axum / tauri 依赖，仅依赖 ServiceContext + ServiceError。

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::Mutex;

use crate::db::DbClient;
use crate::sql::util::{is_dangerous_statement, is_read_only_statement};
use crate::sql_history::SqlHistory;
use crate::service::context::{
    ActiveQueryHandle, ActiveQuerySession, ServiceContext, SharedTransactionSession,
    TransactionSession,
};
use crate::service::error::ServiceError;
use crate::service::row_codec::{encode_mysql_row, MySqlRowJsonEncoder};
use crate::service::schema::SchemaService;

// ── 常量 ────────────────────────────────────────────────

const QUERY_PREVIEW_CHUNK_SIZE: u32 = 200;
const QUERY_PREVIEW_ROW_CAP: u32 = 1000;

// ── 参数 / 响应类型 ─────────────────────────────────────

/// SQL 执行请求参数（handler / command 均使用此结构）
#[derive(Debug, Clone)]
pub struct ExecuteParams {
    pub sql: String,
    pub force: Option<bool>,
    pub db_id: Option<String>,
    pub chunk_offset: Option<u32>,
    pub chunk_size: Option<u32>,
    pub cancel_token: Option<String>,
    pub transaction_id: Option<String>,
}

/// SQL 执行响应（handler / command 均使用此结构）
#[derive(Debug, Clone, serde::Serialize)]
pub struct ExecuteResult {
    pub columns: Vec<String>,
    pub rows: Vec<serde_json::Value>,
    pub row_count: usize,
    pub affected_rows: u64,
    pub execution_time_ms: u64,
    pub has_more: bool,
    pub next_offset: Option<u32>,
    pub chunk_offset: u32,
    pub chunk_size: Option<u32>,
    pub preview_cap: Option<u32>,
    pub truncated: bool,
    pub transaction_state: Option<String>,
}

/// 取消查询请求
#[derive(Debug, Clone)]
pub struct ExecuteCancelParams {
    pub cancel_token: String,
}

/// 取消查询响应
#[derive(Debug, Clone, serde::Serialize)]
pub struct ExecuteCancelResult {
    pub canceled: bool,
}

/// 事务操作请求
#[derive(Debug, Clone)]
pub struct ExecuteTransactionParams {
    pub action: String,
    pub transaction_id: String,
    pub db_id: Option<String>,
}

/// 事务操作响应
#[derive(Debug, Clone, serde::Serialize)]
pub struct ExecuteTransactionResult {
    pub action: String,
    pub transaction_id: String,
    pub state: String,
    pub execution_time_ms: u64,
}

// ── SQL 预处理结果 ──────────────────────────────────────

struct SqlClassification {
    clean_sql: String,
    upper_sql: String,
    statement_kind: Option<String>,
    is_select: bool,
    is_dangerous: bool,
}

// ── WorkbenchService ────────────────────────────────────

pub struct WorkbenchService;

impl WorkbenchService {
    // ── 核心：SQL 执行 ──────────────────────────────────

    /// 执行 SQL — 从 execute_sql handler 提取的纯业务逻辑
    pub async fn execute_sql(
        ctx: &ServiceContext,
        params: ExecuteParams,
    ) -> Result<ExecuteResult, ServiceError> {
        let (db_client, _) = SchemaService::resolve_db_client(ctx, params.db_id.as_deref()).await?;
        let is_read_only = SchemaService::is_read_only_connection(ctx, params.db_id.as_deref()).await;

        // 1. SQL 预处理 + 分类
        let classification = Self::classify_sql(&params.sql);

        // 2. 只读守卫
        if is_read_only && !classification.is_select {
            return Err(ServiceError::Forbidden(
                "当前连接为只读模式，禁止执行非查询操作！".into(),
            ));
        }

        // 3. 危险 SQL 守卫
        if classification.is_dangerous && params.force != Some(true) {
            return Err(ServiceError::BadRequest(
                serde_json::json!({
                    "error": "DANGEROUS_SQL",
                    "message": "检测到高危操作，请确认后强制执行"
                })
                .to_string(),
            ));
        }

        // 4. Chunked Preview 设置
        let chunk_offset = params.chunk_offset.unwrap_or(0);
        let is_chunked_preview = classification.is_select
            && classification.upper_sql.starts_with("SELECT")
            && !classification.upper_sql.contains("LIMIT");

        let mut modified_sql = params.sql.clone();
        let mut chunk_size = None;
        let mut preview_cap = None;
        let mut has_more = false;
        let mut next_offset = None;
        let mut truncated = false;

        if is_chunked_preview {
            let requested_chunk_size = params
                .chunk_size
                .unwrap_or(QUERY_PREVIEW_CHUNK_SIZE)
                .clamp(1, QUERY_PREVIEW_CHUNK_SIZE);
            let remaining = QUERY_PREVIEW_ROW_CAP.saturating_sub(chunk_offset);
            let effective_chunk_size = requested_chunk_size.min(remaining.max(1));
            modified_sql = classification
                .clean_sql
                .trim()
                .trim_end_matches(';')
                .to_string();
            modified_sql.push_str(&format!(
                " LIMIT {} OFFSET {}",
                effective_chunk_size + 1,
                chunk_offset
            ));
            chunk_size = Some(requested_chunk_size);
            preview_cap = Some(QUERY_PREVIEW_ROW_CAP);
        } else if classification.is_select && !classification.upper_sql.contains("LIMIT") {
            modified_sql = classification
                .clean_sql
                .trim()
                .trim_end_matches(';')
                .to_string();
            modified_sql.push_str(" LIMIT 1000");
        }

        // 5. Transaction session 解析
        let transaction_id = params
            .transaction_id
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string);
        let transaction_session = if let Some(id) = transaction_id.as_deref() {
            Some(
                Self::get_or_open_transaction_session(
                    ctx,
                    params.db_id.as_deref(),
                    id,
                    false,
                )
                .await?,
            )
        } else {
            None
        };

        // 6. Active query 注册
        let cancel_token = params
            .cancel_token
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string);
        let mut active_query = if let Some(token) = cancel_token {
            Some(
                Self::setup_active_query_async(
                    ctx,
                    &db_client,
                    token,
                    transaction_session.clone(),
                )
                .await?,
            )
        } else {
            None
        };

        // 7. 执行
        let mysql_pool = db_client.mysql_pool().ok().cloned();
        let start_time = Instant::now();

        let mut rows = Vec::new();
        let mut columns = Vec::new();
        let mut affected_rows = 0u64;
        let mut status = "success".to_string();
        let mut err_msg = None;

        if classification.is_select {
            let execution_result = Self::execute_select(
                ctx,
                &modified_sql,
                active_query.as_mut(),
                transaction_session.as_ref(),
                &mysql_pool,
            )
            .await;

            match execution_result {
                Ok(result_rows) => {
                    let chunk_limit = if is_chunked_preview {
                        chunk_size.unwrap_or(QUERY_PREVIEW_CHUNK_SIZE)
                    } else {
                        result_rows.len() as u32
                    };
                    let fetched_len = result_rows.len() as u32;
                    if is_chunked_preview {
                        has_more = fetched_len > chunk_limit
                            && chunk_offset.saturating_add(chunk_limit) < QUERY_PREVIEW_ROW_CAP;
                        next_offset = has_more.then_some(chunk_offset.saturating_add(chunk_limit));
                        truncated = fetched_len > chunk_limit;
                    }
                    let mut row_encoder = None;
                    for row in result_rows.into_iter().take(chunk_limit as usize) {
                        if row_encoder.is_none() {
                            let encoder = MySqlRowJsonEncoder::from_row(&row);
                            columns = encoder.column_names();
                            row_encoder = Some(encoder);
                        }
                        rows.push(encode_mysql_row(
                            &row,
                            row_encoder.as_ref().expect("row encoder initialized"),
                        ));
                    }
                }
                Err(e) => {
                    // 超时情况下 kill 连接并 unregister
                    if matches!(e, ServiceError::Timeout(_)) {
                        if let Some(aq) = active_query.as_ref() {
                            let _ = db_client.kill_query(aq.connection_id).await;
                            crate::service::context::unregister_active_query(ctx, &aq.token).await;
                        }
                        return Err(e);
                    }
                    let query_was_canceled = active_query
                        .as_ref()
                        .map(|q| q.canceled.load(Ordering::SeqCst))
                        .unwrap_or(false);
                    status = if query_was_canceled { "canceled" } else { "error" }.to_string();
                    err_msg = Some(if query_was_canceled {
                        "Query canceled".into()
                    } else {
                        e.to_string()
                    });
                }
            }
        } else {
            let execution_result = Self::execute_non_select(
                ctx,
                &modified_sql,
                active_query.as_mut(),
                transaction_session.as_ref(),
                &mysql_pool,
            )
            .await;

            match execution_result {
                Ok(result) => {
                    affected_rows = result.rows_affected();
                }
                Err(e) => {
                    // 超时情况下 kill 连接并 unregister
                    if matches!(e, ServiceError::Timeout(_)) {
                        if let Some(aq) = active_query.as_ref() {
                            let _ = db_client.kill_query(aq.connection_id).await;
                            crate::service::context::unregister_active_query(ctx, &aq.token).await;
                        }
                        return Err(e);
                    }
                    let query_was_canceled = active_query
                        .as_ref()
                        .map(|q| q.canceled.load(Ordering::SeqCst))
                        .unwrap_or(false);
                    status = if query_was_canceled { "canceled" } else { "error" }.to_string();
                    err_msg = Some(if query_was_canceled {
                        "Query canceled".into()
                    } else {
                        e.to_string()
                    });
                }
            }
        }

        // 8. 清理 active query
        if let Some(active_query) = active_query.as_ref() {
            crate::service::context::unregister_active_query(ctx, &active_query.token).await;
        }
        let was_canceled = active_query
            .as_ref()
            .map(|q| q.canceled.load(Ordering::SeqCst))
            .unwrap_or(false);

        let elapsed = start_time.elapsed().as_millis() as u64;

        // 9. 记录历史
        Self::record_history(
            ctx,
            &modified_sql,
            &status,
            elapsed,
            params.db_id.as_deref(),
            classification.is_select,
            classification.statement_kind.as_deref(),
            if err_msg.is_none() && classification.is_select {
                Some(rows.len() as u64)
            } else {
                None
            },
            if err_msg.is_none() && !classification.is_select {
                Some(affected_rows)
            } else {
                None
            },
        )
        .await;

        // 10. 错误处理
        if let Some(e) = err_msg {
            if was_canceled {
                return Err(ServiceError::Timeout(e));
            }
            return Err(ServiceError::DbQuery(e));
        }

        // 11. Transaction 后处理
        Self::post_process_transaction(
            ctx,
            transaction_session.as_ref(),
            transaction_id.as_deref(),
            &classification.upper_sql,
            err_msg.is_none(),
        )
        .await?;

        // 12. 非 SELECT 清除 metadata 缓存
        if !classification.is_select {
            SchemaService::clear_metadata_caches(ctx).await;
        }

        // 13. Transaction state
        let transaction_state = if let Some(id) = transaction_id.as_deref() {
            if ctx.session_state().get_transaction(id).await.is_some() {
                Some("active".into())
            } else {
                Some("idle".into())
            }
        } else {
            None
        };

        Ok(ExecuteResult {
            columns,
            row_count: rows.len(),
            rows,
            affected_rows,
            execution_time_ms: elapsed,
            has_more,
            next_offset,
            chunk_offset,
            chunk_size,
            preview_cap,
            truncated,
            transaction_state,
        })
    }

    // ── 取消查询 ────────────────────────────────────────

    pub async fn execute_cancel(
        ctx: &ServiceContext,
        params: ExecuteCancelParams,
    ) -> Result<ExecuteCancelResult, ServiceError> {
        let cancel_token = params.cancel_token.trim();
        if cancel_token.is_empty() {
            return Ok(ExecuteCancelResult { canceled: false });
        }
        let canceled = crate::service::context::cancel_active_query(ctx, cancel_token).await?;
        Ok(ExecuteCancelResult { canceled })
    }

    // ── 事务操作 ────────────────────────────────────────

    pub async fn execute_transaction(
        ctx: &ServiceContext,
        params: ExecuteTransactionParams,
    ) -> Result<ExecuteTransactionResult, ServiceError> {
        let transaction_id = params.transaction_id.trim();
        if transaction_id.is_empty() {
            return Err(ServiceError::BadRequest("transaction_id is required".into()));
        }
        let action = params.action.trim().to_lowercase();

        if action == "begin" {
            let _ = Self::get_or_open_transaction_session(
                ctx,
                params.db_id.as_deref(),
                transaction_id,
                true,
            )
            .await?;
            return Ok(ExecuteTransactionResult {
                action: "begin".into(),
                transaction_id: transaction_id.to_string(),
                state: "active".into(),
                execution_time_ms: 0,
            });
        }

        if action != "commit" && action != "rollback" {
            return Err(ServiceError::BadRequest(
                "transaction action must be begin, commit or rollback".into(),
            ));
        }

        let session = ctx
            .session_state()
            .get_transaction(transaction_id)
            .await
            .ok_or_else(|| ServiceError::NotFound("transaction session not found".into()))?;
        let expected_db_id =
            crate::service::context::resolve_transaction_db_id(ctx, params.db_id.as_deref()).await;
        {
            let guard = session.lock().await;
            if guard.db_id != expected_db_id {
                return Err(ServiceError::BadRequest(
                    "Transaction session is bound to a different database connection".into(),
                ));
            }
        }

        let started_at = Instant::now();
        {
            let mut guard = session.lock().await;
            let sql = if action == "commit" { "COMMIT" } else { "ROLLBACK" };
            tokio::time::timeout(
                ctx.timeouts().db_query,
                sqlx::query(sql).execute(&mut *guard.conn),
            )
            .await
            .map_err(|_| ServiceError::Timeout(format!("{action} timed out")))?
            .map_err(|e| ServiceError::InternalError(e.to_string()))?;
        }

        ctx.session_state().remove_transaction(transaction_id).await;
        SchemaService::clear_metadata_caches(ctx).await;

        Ok(ExecuteTransactionResult {
            action,
            transaction_id: transaction_id.to_string(),
            state: "idle".into(),
            execution_time_ms: started_at.elapsed().as_millis() as u64,
        })
    }

    // ── 内部辅助 ──────────────────────────────────────────

    /// SQL 预处理 + AST 分类
    fn classify_sql(raw_sql: &str) -> SqlClassification {
        let mut clean_sql = raw_sql.trim().to_string();
        // 去除首部注释
        loop {
            if clean_sql.starts_with("--") {
                if let Some(idx) = clean_sql.find('\n') {
                    clean_sql = clean_sql[idx + 1..].trim().to_string();
                } else {
                    clean_sql = String::new();
                }
            } else if clean_sql.starts_with("/*") {
                if let Some(idx) = clean_sql.find("*/") {
                    clean_sql = clean_sql[idx + 2..].trim().to_string();
                } else {
                    clean_sql = String::new();
                }
            } else {
                break;
            }
        }

        let upper_sql = clean_sql.to_uppercase();
        let statement_kind = clean_sql
            .split_whitespace()
            .next()
            .map(|part| part.to_uppercase());

        let dialect = sqlparser::dialect::GenericDialect {};
        let parsed = sqlparser::parser::Parser::parse_sql(&dialect, clean_sql.trim());
        let (is_select, is_dangerous) = if let Ok(stmts) = parsed.as_ref() {
            if stmts.len() != 1 {
                (false, true)
            } else {
                let stmt = &stmts[0];
                (is_read_only_statement(stmt), is_dangerous_statement(stmt))
            }
        } else {
            let sel = upper_sql.starts_with("SELECT")
                || upper_sql.starts_with("SHOW")
                || upper_sql.starts_with("DESCRIBE")
                || upper_sql.starts_with("EXPLAIN");
            let dangerous = upper_sql.contains("INSERT ")
                || upper_sql.contains("UPDATE ")
                || upper_sql.contains("DELETE ")
                || upper_sql.contains("DROP ")
                || upper_sql.contains("TRUNCATE ")
                || upper_sql.contains("ALTER ");
            (sel, dangerous)
        };

        SqlClassification {
            clean_sql,
            upper_sql,
            statement_kind,
            is_select,
            is_dangerous,
        }
    }

    /// async 版本：设置 active query（含连接获取）
    async fn setup_active_query_async(
        ctx: &ServiceContext,
        db_client: &DbClient,
        token: String,
        transaction_session: Option<SharedTransactionSession>,
    ) -> Result<ActiveQuerySession, ServiceError> {
        let canceled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        if let Some(ts) = &transaction_session {
            let connection_id = ts.lock().await.connection_id;
            crate::service::context::register_active_query(
                ctx,
                token.clone(),
                ActiveQueryHandle {
                    db_client: db_client.clone(),
                    connection_id,
                    canceled: canceled.clone(),
                },
            )
            .await;
            Ok(ActiveQuerySession {
                token,
                connection_id,
                canceled,
                owned_conn: None,
                transaction_session: Some(ts.clone()),
            })
        } else {
            let mut conn = db_client
                .mysql_pool()?
                .acquire()
                .await
                .map_err(|e| ServiceError::DbConnection(e.to_string()))?;
            let connection_id = DbClient::connection_id_for_session(&mut conn)
                .await
                .map_err(|e| ServiceError::InternalError(e.to_string()))?;
            crate::service::context::register_active_query(
                ctx,
                token.clone(),
                ActiveQueryHandle {
                    db_client: db_client.clone(),
                    connection_id,
                    canceled: canceled.clone(),
                },
            )
            .await;
            Ok(ActiveQuerySession {
                token,
                connection_id,
                canceled,
                owned_conn: Some(conn),
                transaction_session: None,
            })
        }
    }

    /// 执行 SELECT 查询（含超时）
    async fn execute_select(
        ctx: &ServiceContext,
        sql: &str,
        active_query: Option<&mut ActiveQuerySession>,
        transaction_session: Option<&SharedTransactionSession>,
        mysql_pool: &Option<sqlx::MySqlPool>,
    ) -> Result<Vec<sqlx::mysql::MySqlRow>, ServiceError> {
        let result = tokio::time::timeout(ctx.timeouts().db_query, async {
            if let Some(aq) = active_query {
                if let Some(ts) = &aq.transaction_session {
                    let mut session = ts.lock().await;
                    sqlx::query(sql).fetch_all(&mut *session.conn).await
                } else if let Some(conn) = aq.owned_conn.as_mut() {
                    sqlx::query(sql).fetch_all(&mut **conn).await
                } else if let Some(pool) = mysql_pool {
                    sqlx::query(sql).fetch_all(pool).await
                } else {
                    Err(sqlx::Error::PoolClosed)
                }
            } else if let Some(ts) = transaction_session {
                let mut session = ts.lock().await;
                sqlx::query(sql).fetch_all(&mut *session.conn).await
            } else if let Some(pool) = mysql_pool {
                sqlx::query(sql).fetch_all(pool).await
            } else {
                Err(sqlx::Error::PoolClosed)
            }
        })
        .await;

        match result {
            Ok(inner) => inner.map_err(ServiceError::from),
            Err(_) => Err(ServiceError::Timeout(
                "查询执行超时（已超过 30 秒），已被系统安全阻断，请优化 SQL 或添加索引。".into(),
            )),
        }
    }

    /// 执行非 SELECT 查询（含超时）
    async fn execute_non_select(
        ctx: &ServiceContext,
        sql: &str,
        active_query: Option<&mut ActiveQuerySession>,
        transaction_session: Option<&SharedTransactionSession>,
        mysql_pool: &Option<sqlx::MySqlPool>,
    ) -> Result<sqlx::mysql::MySqlQueryResult, ServiceError> {
        let result = tokio::time::timeout(ctx.timeouts().db_query, async {
            if let Some(aq) = active_query {
                if let Some(ts) = &aq.transaction_session {
                    let mut session = ts.lock().await;
                    sqlx::query(sql).execute(&mut *session.conn).await
                } else if let Some(conn) = aq.owned_conn.as_mut() {
                    sqlx::query(sql).execute(&mut **conn).await
                } else if let Some(pool) = mysql_pool {
                    sqlx::query(sql).execute(pool).await
                } else {
                    Err(sqlx::Error::PoolClosed)
                }
            } else if let Some(ts) = transaction_session {
                let mut session = ts.lock().await;
                sqlx::query(sql).execute(&mut *session.conn).await
            } else if let Some(pool) = mysql_pool {
                sqlx::query(sql).execute(pool).await
            } else {
                Err(sqlx::Error::PoolClosed)
            }
        })
        .await;

        match result {
            Ok(inner) => inner.map_err(ServiceError::from),
            Err(_) => Err(ServiceError::Timeout(
                "查询执行超时（已超过 30 秒），已被系统安全阻断，请优化 SQL 或添加索引。".into(),
            )),
        }
    }

    /// 获取或打开事务 session（公开方法，供 CrudService 等调用）
    pub async fn get_or_open_transaction_session(
        ctx: &ServiceContext,
        db_id: Option<&str>,
        transaction_id: &str,
        create_if_not_found: bool,
    ) -> Result<SharedTransactionSession, ServiceError> {
        if let Some(existing) = ctx.session_state().get_transaction(transaction_id).await {
            let expected_db_id =
                crate::service::context::resolve_transaction_db_id(ctx, db_id).await;
            let mut session = existing.lock().await;
            if session.db_id != expected_db_id {
                return Err(ServiceError::BadRequest(
                    "Transaction session is bound to a different database connection".into(),
                ));
            }
            session.last_accessed = std::time::Instant::now();
            drop(session);
            return Ok(existing);
        }

        if !create_if_not_found {
            return Err(ServiceError::NotFound("transaction session not found".into()));
        }

        let resolved_db_id =
            crate::service::context::resolve_transaction_db_id(ctx, db_id).await;
        let (db_client, _) = SchemaService::resolve_db_client(ctx, db_id).await?;
        let mut conn = db_client
            .mysql_pool()?
            .acquire()
            .await
            .map_err(|e| ServiceError::DbConnection(e.to_string()))?;
        let connection_id = DbClient::connection_id_for_session(&mut conn)
            .await
            .map_err(|e| ServiceError::InternalError(e.to_string()))?;
        tokio::time::timeout(
            ctx.timeouts().db_query,
            sqlx::query("START TRANSACTION").execute(&mut *conn),
        )
        .await
        .map_err(|_| ServiceError::Timeout("Starting transaction timed out".into()))?
        .map_err(|e| ServiceError::InternalError(e.to_string()))?;

        let session = Arc::new(Mutex::new(TransactionSession {
            connection_id,
            db_id: resolved_db_id,
            conn,
            last_accessed: std::time::Instant::now(),
        }));

        ctx.session_state()
            .insert_transaction(transaction_id.to_string(), session.clone())
            .await;
        Ok(session)
    }

    /// 事务后处理（COMMIT/ROLLBACK 清理）
    async fn post_process_transaction(
        ctx: &ServiceContext,
        transaction_session: Option<&SharedTransactionSession>,
        transaction_id: Option<&str>,
        upper_sql: &str,
        success: bool,
    ) -> Result<(), ServiceError> {
        if let Some(session) = transaction_session {
            let mut session_lock = session.lock().await;
            session_lock.last_accessed = std::time::Instant::now();
            drop(session_lock);

            let is_tx_end = {
                let s = upper_sql.trim();
                s == "COMMIT" || s == "ROLLBACK" || s == "COMMIT;" || s == "ROLLBACK;"
            };
            if is_tx_end && success {
                if let Some(id) = transaction_id {
                    ctx.session_state().remove_transaction(id).await;
                }
            }
        }
        Ok(())
    }

    /// 记录 SQL 执行历史
    async fn record_history(
        ctx: &ServiceContext,
        sql: &str,
        status: &str,
        execution_time_ms: u64,
        db_id: Option<&str>,
        _is_select: bool,
        statement_kind: Option<&str>,
        row_count: Option<u64>,
        affected_rows: Option<u64>,
    ) {
        let mut store = ctx.get_sql_history().await;
        store.add_history(SqlHistory {
            id: "".to_string(),
            sql: sql.to_string(),
            status: status.to_string(),
            execution_time_ms,
            executed_at: 0,
            db_id: db_id.map(str::to_string),
            row_count,
            affected_rows,
            statement_kind: statement_kind.map(str::to_string),
        });
        // 先保存到磁盘，再更新回 ctx
        let _ = store.save().await;
        ctx.update_sql_history(store).await;
    }
}