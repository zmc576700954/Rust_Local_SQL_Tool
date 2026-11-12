//! PostgreSQL `CREATE INDEX CONCURRENTLY` 执行器（S3）。
//!
//! 约束：
//! - 单语句自动提交，**不进任何事务块**（PG 会直接报错）；
//! - 同名 INVALID 索引（上次失败残留）先 `DROP INDEX CONCURRENTLY` 清理再重建；
//! - 同名有效索引视为 no-op；
//! - 失败时不留 INVALID 索引；unique 违例单独标记为 `build:unique_violation`；
//! - 进度轮询是 best-effort，查不到不影响主流程。

use std::time::Duration;

use serde_json::json;
use sqlx::postgres::PgPool;

use crate::ops::error::OpsError;
use crate::ops::pg_rows::convert_pg_rows;
use crate::ops::policy::OpsPolicy;
use crate::ops::registry::OpsContext;
use crate::ops::types::{hash_json, now, AuditRecord};
use crate::sql::util::quote_ident_pg;

use super::migration_store::{
    BlockingDetail, IndexJob, IndexPhase, IndexProgress, IndexSpec, MigrationHub, MigrationStatus,
    CONTROL_ABORT, CONTROL_PAUSE,
};
use super::pg_backfill::load_snapshot;
use super::pg_verify::qualified;

const ENGINE: &str = "postgresql";
const NAME_MAX_BYTES: usize = 63;
const PROGRESS_POLL: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct IndexRequest {
    pub schema: Option<String>,
    pub table: String,
    pub index_name: String,
    pub columns: Vec<String>,
    pub unique: bool,
    pub production_confirmed: bool,
}

pub fn spec_hash(spec: &IndexSpec) -> String {
    hash_json(spec)
}

fn sanitize_schema(schema: Option<&str>) -> String {
    schema
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("public")
        .to_string()
}

/// 生成 `CREATE INDEX [UNIQUE] CONCURRENTLY`；列名逐项加引号。
pub fn build_create_index_sql(schema: &str, spec: &IndexSpec) -> String {
    let columns = spec
        .columns
        .iter()
        .map(|column| quote_ident_pg(column))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "CREATE {}INDEX CONCURRENTLY {} ON {} ({})",
        if spec.unique { "UNIQUE " } else { "" },
        quote_ident_pg(&spec.index_name),
        qualified(Some(schema), &spec.table),
        columns
    )
}

pub async fn new_job(
    ctx: &OpsContext,
    hub: &MigrationHub,
    req: IndexRequest,
) -> Result<IndexJob, OpsError> {
    if req.table.trim().is_empty() {
        return Err(OpsError::Validation("索引任务需要指定表名".into()));
    }
    if req.index_name.trim().is_empty() {
        return Err(OpsError::Validation("索引任务需要指定索引名".into()));
    }
    if req.columns.is_empty() {
        return Err(OpsError::Validation("索引任务需要至少一个列".into()));
    }
    let schema = req
        .schema
        .clone()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let spec = IndexSpec {
        schema: schema.clone(),
        table: req.table.trim().to_string(),
        index_name: req.index_name.trim().to_string(),
        columns: req
            .columns
            .iter()
            .map(|column| column.trim().to_string())
            .filter(|column| !column.is_empty())
            .collect(),
        unique: req.unique,
    };
    if spec.columns.is_empty() {
        return Err(OpsError::Validation("索引任务需要至少一个列".into()));
    }
    let at = now();
    let job = IndexJob {
        job_id: format!("ix-{}", uuid::Uuid::new_v4()),
        plan_id: "manual".into(),
        db_id: ctx.db_id.clone(),
        db_name: ctx.db_name.clone(),
        db_fingerprint: ctx.db_fingerprint.clone(),
        spec_hash: spec_hash(&spec),
        spec,
        engine: String::new(),
        status: MigrationStatus::Pending,
        phase: IndexPhase::Preflight,
        progress: None,
        production_confirmed: req.production_confirmed,
        run_count: 0,
        blocking: None,
        notes: Vec::new(),
        created_at: at,
        updated_at: at,
        started_at: 0,
        finished_at: None,
        last_error: None,
    };
    hub.insert_index(job.clone()).await?;
    Ok(job)
}

pub async fn run_job(
    job_id: &str,
    ctx: &OpsContext,
    hub: &MigrationHub,
    policy: &OpsPolicy,
) -> Result<(), OpsError> {
    let mut job = hub.get_index(job_id).await?;
    if job.status.is_finished() {
        return Ok(());
    }
    if !ctx.is_postgres() {
        return block_index(ctx, hub, job_id, "并发建索引只支持 PostgreSQL", "preflight").await;
    }
    if ctx.db_name != job.db_name {
        return block_index(
            ctx,
            hub,
            job_id,
            &format!(
                "数据库名变化：任务绑定 {}，当前连接 {}",
                job.db_name, ctx.db_name
            ),
            "preflight",
        )
        .await;
    }
    if let Err(err) = policy.authorize_backfill(ctx.is_read_only, job.production_confirmed) {
        return block_index(ctx, hub, job_id, &err.to_string(), "policy").await;
    }
    if job.engine.is_empty() {
        let _ = hub
            .update_index(job_id, |job| job.engine = ENGINE.to_string())
            .await?;
    } else if job.engine != ENGINE {
        return block_index(
            ctx,
            hub,
            job_id,
            &format!("任务引擎为 {}，禁止用 PostgreSQL 续跑", job.engine),
            "preflight:engine_mismatch",
        )
        .await;
    }

    let expected_hash = spec_hash(&job.spec);
    if job.spec_hash.is_empty() {
        let _ = hub
            .update_index(job_id, |job| job.spec_hash = expected_hash.clone())
            .await?;
    } else if job.spec_hash != expected_hash {
        return block_index(
            ctx,
            hub,
            job_id,
            "索引规格与任务指纹不一致，拒绝续跑",
            "preflight:spec_mismatch",
        )
        .await;
    }

    // 名称长度在 preflight 判决，不静默截断。
    if job.spec.index_name.len() > NAME_MAX_BYTES {
        return block_index(
            ctx,
            hub,
            job_id,
            &format!(
                "索引名 {} 超过 {NAME_MAX_BYTES} 字节，PG 会截断，请改名",
                job.spec.index_name
            ),
            "preflight:name_too_long",
        )
        .await;
    }

    job = hub
        .update_index(job_id, |job| {
            job.status = MigrationStatus::Running;
            job.phase = IndexPhase::Preflight;
            job.run_count += 1;
            if job.started_at == 0 {
                job.started_at = now();
            }
            job.finished_at = None;
            job.last_error = None;
            job.blocking = None;
        })
        .await?;
    let _ = append_index_audit(
        ctx,
        "field_migration.index.run.started",
        "ok",
        &job,
        json!({
            "job_id": job_id,
            "engine": ENGINE,
            "run_count": job.run_count,
            "index": job.spec.index_name,
            "table": job.spec.table,
            "unique": job.spec.unique,
        }),
    );

    let schema = sanitize_schema(job.spec.schema.as_deref());
    let snapshot = load_snapshot(ctx, &schema, &job.spec.table).await?;
    if snapshot.columns.is_empty() {
        return block_index(ctx, hub, job_id, "表不存在或没有列", "preflight").await;
    }
    for column in &job.spec.columns {
        if snapshot.find_column(column).is_none() {
            return block_index(
                ctx,
                hub,
                job_id,
                &format!("表 {} 缺少列 {column}", job.spec.table),
                "preflight",
            )
            .await;
        }
    }
    if is_partitioned(ctx, &schema, &job.spec.table).await? {
        return block_index(
            ctx,
            hub,
            job_id,
            "PG 不支持对分区表执行 CREATE INDEX CONCURRENTLY",
            "preflight:partitioned",
        )
        .await;
    }

    // 同名索引状态：valid → no-op；invalid → 清理后重建；约束 → 冲突。
    match index_state(ctx, &schema, &job.spec.index_name).await? {
        Some(state) if state.is_constraint => {
            return block_index(
                ctx,
                hub,
                job_id,
                "同名对象是约束而非普通索引，请更换索引名",
                "preflight:name_conflict",
            )
            .await
        }
        Some(state) if state.valid && state.ready => {
            let job = hub
                .update_index(job_id, |job| {
                    job.status = MigrationStatus::Completed;
                    job.phase = IndexPhase::Done;
                    job.finished_at = Some(now());
                    job.notes.push("同名索引已有效，无需重建（no-op）".into());
                })
                .await?;
            let _ = append_index_audit(
                ctx,
                "field_migration.index.completed",
                "ok",
                &job,
                json!({ "job_id": job_id, "noop": true }),
            );
            return Ok(());
        }
        Some(_) => {
            // 上次失败残留的 INVALID 索引：官方推荐先 DROP CONCURRENTLY 再重建。
            exec_index_ddl(
                ctx,
                policy,
                &format!(
                    "DROP INDEX CONCURRENTLY IF EXISTS {}",
                    qualified(Some(&schema), &job.spec.index_name)
                ),
            )
            .await?;
            let _ = hub
                .update_index(job_id, |job| {
                    job.notes.push("已清理残留 INVALID 索引".into())
                })
                .await?;
        }
        None => {}
    }

    if !wait_control(hub, job_id).await? {
        return abort_index(ctx, hub, job_id).await;
    }
    hub.update_index(job_id, |job| job.phase = IndexPhase::Building)
        .await?;
    let sql = build_create_index_sql(&schema, &job.spec);
    let pool: &PgPool = ctx.db_client.pool.pg()?;
    let poller = spawn_progress_poller(
        hub,
        job_id,
        pool.clone(),
        schema.clone(),
        job.spec.index_name.clone(),
    );
    let result = exec_index_ddl(ctx, policy, &sql).await;
    poller.abort();

    if let Err(err) = result {
        let message = err.to_string();
        let cleaned = cleanup_invalid(ctx, &schema, &job.spec.index_name, policy)
            .await
            .unwrap_or(false);
        // PG 的唯一索引失败文案是 `could not create unique index ...`，
        // DETAIL 里的 `duplicate key value` 不一定进入 message，两种都认。
        if message.contains("duplicate key value")
            || message.contains("could not create unique index")
            || message.contains("23505")
        {
            return block_index(
                ctx,
                hub,
                job_id,
                &format!("唯一索引构建失败（存在重复值）: {message}"),
                "build:unique_violation",
            )
            .await;
        }
        // 非唯一违例：可重跑（preflight 会清理残留）。
        let job = hub
            .update_index(job_id, |job| {
                job.status = MigrationStatus::Interrupted;
                job.finished_at = Some(now());
                job.last_error = Some(format!(
                    "CREATE INDEX CONCURRENTLY 失败: {message}; 残留索引已清理: {cleaned}"
                ));
                job.notes.push("失败：可重新执行".into());
            })
            .await?;
        let _ = append_index_audit(
            ctx,
            "field_migration.index.interrupted",
            "interrupted",
            &job,
            json!({ "job_id": job_id, "error": message, "cleaned": cleaned }),
        );
        return Ok(());
    }

    hub.update_index(job_id, |job| job.phase = IndexPhase::Verifying)
        .await?;
    let state = index_state(ctx, &schema, &job.spec.index_name)
        .await?
        .ok_or_else(|| {
            OpsError::Validation(format!("索引 {} 执行后不可见", job.spec.index_name))
        })?;
    if !(state.valid && state.ready) {
        return block_index(
            ctx,
            hub,
            job_id,
            "索引构建结束但 indisvalid/indisready 为假",
            "verify",
        )
        .await;
    }
    let size = index_size(ctx, &schema, &job.spec.index_name).await?;
    let job = hub
        .update_index(job_id, |job| {
            job.status = MigrationStatus::Completed;
            job.phase = IndexPhase::Done;
            job.finished_at = Some(now());
            job.last_error = None;
            job.notes.push(format!("verify: ok, size={size} bytes"));
        })
        .await?;
    let _ = append_index_audit(
        ctx,
        "field_migration.index.completed",
        "ok",
        &job,
        json!({
            "job_id": job_id,
            "index": job.spec.index_name,
            "table": job.spec.table,
            "unique": job.spec.unique,
            "size_bytes": size,
        }),
    );
    Ok(())
}

// ── 基础设施 ───────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
struct IndexState {
    valid: bool,
    ready: bool,
    is_constraint: bool,
}

async fn index_state(
    ctx: &OpsContext,
    schema: &str,
    name: &str,
) -> Result<Option<IndexState>, OpsError> {
    let rows = sqlx::query(
        "SELECT i.indisvalid::text AS valid, i.indisready::text AS ready, \
                CASE WHEN con.oid IS NULL THEN 'NO' ELSE 'YES' END AS is_constraint \
         FROM pg_class c \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         LEFT JOIN pg_index i ON i.indexrelid = c.oid \
         LEFT JOIN pg_constraint con ON con.conindid = c.oid \
         WHERE n.nspname = $1 AND c.relname = $2",
    )
    .bind(schema)
    .bind(name)
    .fetch_all(ctx.db_client.pool.pg()?)
    .await?;
    let rows = convert_pg_rows(&rows);
    Ok(rows.first().map(|row| IndexState {
        valid: row
            .get("valid")
            .map(|value| value == "true")
            .unwrap_or(false),
        ready: row
            .get("ready")
            .map(|value| value == "true")
            .unwrap_or(false),
        is_constraint: row
            .get("is_constraint")
            .map(|value| value == "YES")
            .unwrap_or(false),
    }))
}

async fn is_partitioned(ctx: &OpsContext, schema: &str, table: &str) -> Result<bool, OpsError> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pg_partitioned_table pt \
         JOIN pg_class c ON c.oid = pt.partrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = $1 AND c.relname = $2",
    )
    .bind(schema)
    .bind(table)
    .fetch_one(ctx.db_client.pool.pg()?)
    .await?;
    Ok(count > 0)
}

async fn index_size(ctx: &OpsContext, schema: &str, name: &str) -> Result<u64, OpsError> {
    let qualified = qualified(Some(schema), name);
    let size: i64 = sqlx::query_scalar("SELECT pg_relation_size(to_regclass($1))")
        .bind(&qualified)
        .fetch_one(ctx.db_client.pool.pg()?)
        .await?;
    Ok(size.max(0) as u64)
}

/// 单语句自动提交（不进事务块）；S4 起收敛到共享实现，行为不变。
async fn exec_index_ddl(ctx: &OpsContext, policy: &OpsPolicy, sql: &str) -> Result<(), OpsError> {
    super::pg_backfill::exec_ddl_auto_commit(ctx, policy, sql).await
}

/// 清理失败残留的 INVALID 索引；返回是否确实清理了一个。
async fn cleanup_invalid(
    ctx: &OpsContext,
    schema: &str,
    name: &str,
    policy: &OpsPolicy,
) -> Result<bool, OpsError> {
    match index_state(ctx, schema, name).await? {
        Some(state) if !state.valid || !state.ready => {
            exec_index_ddl(
                ctx,
                policy,
                &format!(
                    "DROP INDEX CONCURRENTLY IF EXISTS {}",
                    qualified(Some(schema), name)
                ),
            )
            .await?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// best-effort 进度轮询；任何查询失败都静默跳过。
fn spawn_progress_poller(
    hub: &MigrationHub,
    job_id: &str,
    pool: PgPool,
    schema: String,
    name: String,
) -> tokio::task::JoinHandle<()> {
    let hub = hub.clone();
    let job_id = job_id.to_string();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(PROGRESS_POLL).await;
            let qualified = qualified(Some(&schema), &name);
            let rows = sqlx::query(
                "SELECT phase::text AS phase, blocks_done::text AS blocks_done, \
                        blocks_total::text AS blocks_total, tuples_done::text AS tuples_done, \
                        tuples_total::text AS tuples_total \
                 FROM pg_stat_progress_create_index WHERE index_relid = to_regclass($1)",
            )
            .bind(&qualified)
            .fetch_all(&pool)
            .await;
            let Ok(rows) = rows else { continue };
            let rows = convert_pg_rows(&rows);
            let Some(row) = rows.first() else { continue };
            let progress = IndexProgress {
                phase: row.get("phase").cloned().unwrap_or_default(),
                blocks_done: parse_u64(row.get("blocks_done")),
                blocks_total: parse_u64(row.get("blocks_total")),
                tuples_done: parse_u64(row.get("tuples_done")),
                tuples_total: parse_u64(row.get("tuples_total")),
                updated_at: now(),
            };
            let _ = hub
                .update_index(&job_id, |job| job.progress = Some(progress.clone()))
                .await;
        }
    })
}

fn parse_u64(value: Option<&String>) -> u64 {
    value
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

async fn wait_control(hub: &MigrationHub, job_id: &str) -> Result<bool, OpsError> {
    loop {
        match hub.control_value(job_id).await {
            CONTROL_ABORT => return Ok(false),
            CONTROL_PAUSE => {
                let job = hub.get_index(job_id).await?;
                if matches!(
                    job.status,
                    MigrationStatus::Running | MigrationStatus::Pending
                ) {
                    hub.update_index(job_id, |job| job.status = MigrationStatus::Paused)
                        .await?;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            _ => {
                let job = hub.get_index(job_id).await?;
                if job.status == MigrationStatus::Paused {
                    hub.update_index(job_id, |job| {
                        job.status = MigrationStatus::Running;
                        job.phase = IndexPhase::Building;
                    })
                    .await?;
                }
                return Ok(true);
            }
        }
    }
}

async fn abort_index(ctx: &OpsContext, hub: &MigrationHub, job_id: &str) -> Result<(), OpsError> {
    let job = hub
        .update_index(job_id, |job| {
            job.status = MigrationStatus::Aborted;
            job.finished_at = Some(now());
        })
        .await?;
    let _ = append_index_audit(
        ctx,
        "field_migration.index.aborted",
        "aborted",
        &job,
        json!({ "job_id": job_id }),
    );
    Ok(())
}

async fn block_index(
    ctx: &OpsContext,
    hub: &MigrationHub,
    job_id: &str,
    message: &str,
    phase: &str,
) -> Result<(), OpsError> {
    let job = hub
        .update_index(job_id, |job| {
            job.status = MigrationStatus::Blocked;
            job.finished_at = Some(now());
            job.last_error = Some(message.to_string());
            job.blocking = Some(BlockingDetail {
                batch_no: job.run_count.max(1),
                last_pk: None,
                end_pk: None,
                phase: phase.to_string(),
                total_diffs: 0,
                pks: Vec::new(),
                has_more: false,
                differences: Vec::new(),
                verify: None,
                message: message.to_string(),
            });
        })
        .await?;
    let _ = append_index_audit(
        ctx,
        "field_migration.index.blocked",
        "blocked",
        &job,
        json!({ "job_id": job_id, "phase": phase, "message": message }),
    );
    Ok(())
}

fn append_index_audit(
    ctx: &OpsContext,
    event: &str,
    outcome: &str,
    job: &IndexJob,
    detail: serde_json::Value,
) -> Result<(), OpsError> {
    let Some(audit) = ctx.audit.as_ref() else {
        return Ok(());
    };
    let mut record = AuditRecord::new(event, outcome);
    record.plan_id = Some(job.plan_id.clone());
    record.db_id = Some(job.db_id.clone());
    record.db_name = Some(job.db_name.clone());
    record.action_id = Some(format!("field_migration.index:{}", job.job_id));
    record.actor = Some("local-user".into());
    record.detail = detail;
    audit.append(&record)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> IndexSpec {
        IndexSpec {
            schema: Some("public".into()),
            table: "orders".into(),
            index_name: "idx_orders_payload".into(),
            columns: vec!["payload".into(), "created_at".into()],
            unique: true,
        }
    }

    #[test]
    fn create_sql_uses_concurrently_and_quoted_columns() {
        let sql = build_create_index_sql("public", &spec());
        assert!(sql.starts_with("CREATE UNIQUE INDEX CONCURRENTLY"));
        assert!(sql.contains("\"public\".\"orders\""));
        assert!(sql.contains("(\"payload\", \"created_at\")"));
        assert!(!sql.to_ascii_uppercase().contains("BEGIN"));
    }

    #[test]
    fn spec_hash_binds_columns_and_unique() {
        let base = spec_hash(&spec());
        let mut other = spec();
        other.unique = false;
        assert_ne!(base, spec_hash(&other));
        let mut reordered = spec();
        reordered.columns.reverse();
        assert_ne!(base, spec_hash(&reordered));
    }

    #[test]
    fn name_limit_is_bytes() {
        assert!(("a".repeat(63)).len() <= NAME_MAX_BYTES);
        assert!(("指".repeat(22)).len() > NAME_MAX_BYTES);
    }
}
