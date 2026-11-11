//! PostgreSQL `SET NOT NULL` 执行器（S3）。
//!
//! 路径与 S1 脚本一致：`ADD CHECK ... NOT VALID` → `VALIDATE CONSTRAINT` →
//! `SET NOT NULL` → `DROP CONSTRAINT`，前置一段 keyset 分批的 NULL 扫描：
//! - 扫描只读、可续跑，发现 NULL 即阻断并给出计数与主键样本；
//! - DDL 阶段单语句自动提交（不开事务块），每阶段入口幂等；
//! - PG < 12 直接阻断：只有 12+ 能用已验证 CHECK 跳过 `SET NOT NULL` 的全表扫描；
//! - 中断残留的 NOT VALID 约束/已验证约束会在 preflight 探测并跳转到对应阶段。

use std::time::Instant;

use serde_json::json;
use sqlx::postgres::PgPool;

use crate::ops::error::OpsError;
use crate::ops::pg_rows::convert_pg_rows;
use crate::ops::policy::OpsPolicy;
use crate::ops::registry::OpsContext;
use crate::ops::types::{hash_json, now, AuditRecord};
use crate::sql::util::{escape_sql_string, quote_ident_pg};

use super::backfill::wait_rate_limit;
use super::migration_store::{
    BackfillConfig, BlockingDetail, MigrationHub, MigrationStatus, NotNullExecution, NotNullJob,
    NotNullPhase, NotNullScanProgress, PkKind, CONTROL_ABORT, CONTROL_PAUSE,
};
use super::pg_backfill::{
    load_snapshot, pg_timeout_statements, pk_collation_deterministic, pk_kind_from_column_type,
};
use super::pg_verify::{bind_query, bind_value, cast_clause, order_expr, qualified, PgBind};

const ENGINE: &str = "postgresql";
const SAMPLE_LIMIT: usize = 100;
/// PG 12 起 `SET NOT NULL` 可复用已验证 CHECK 跳过全表扫描。
const SET_NOT_NULL_MIN_VERSION: i32 = 120_000;

/// 新建任务入参。
#[derive(Debug, Clone)]
pub struct NotNullRequest {
    pub schema: Option<String>,
    pub table: String,
    pub column: String,
    #[allow(dead_code)]
    pub production_confirmed: bool,
}

/// 约束名与 S1 脚本一致。
pub fn constraint_name(column: &str) -> String {
    format!("fm_not_null_{column}")
}

/// 表/列/约束名指纹；续跑不一致即拒绝。
pub fn spec_hash(schema: &str, table: &str, column: &str) -> String {
    hash_json(&json!({
        "schema": schema,
        "table": table,
        "column": column,
        "constraint": constraint_name(column),
    }))
}

pub fn version_supports_set_not_null(server_version_num: i32) -> bool {
    server_version_num >= SET_NOT_NULL_MIN_VERSION
}

fn sanitize_schema(schema: Option<&str>) -> String {
    schema
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("public")
        .to_string()
}

pub async fn new_job(
    ctx: &OpsContext,
    hub: &MigrationHub,
    req: NotNullRequest,
) -> Result<NotNullJob, OpsError> {
    if req.table.trim().is_empty() {
        return Err(OpsError::Validation("SET NOT NULL 需要指定表名".into()));
    }
    if req.column.trim().is_empty() {
        return Err(OpsError::Validation("SET NOT NULL 需要指定列名".into()));
    }
    let schema = req
        .schema
        .clone()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let table = req.table.trim().to_string();
    let column = req.column.trim().to_string();
    let effective_schema = sanitize_schema(schema.as_deref());
    let fingerprint = spec_hash(&effective_schema, &table, &column);
    let at = now();
    let job = NotNullJob {
        job_id: format!("nn-{}", uuid::Uuid::new_v4()),
        plan_id: "manual".into(),
        db_id: ctx.db_id.clone(),
        db_name: ctx.db_name.clone(),
        db_fingerprint: ctx.db_fingerprint.clone(),
        schema,
        table,
        column: column.clone(),
        constraint_name: constraint_name(&column),
        spec_hash: fingerprint,
        engine: String::new(),
        status: MigrationStatus::Pending,
        phase: NotNullPhase::Preflight,
        scan: NotNullScanProgress::default(),
        config: BackfillConfig::default(),
        execution: NotNullExecution::default(),
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
    hub.insert_notnull(job.clone()).await?;
    Ok(job)
}

// ── 执行入口 ───────────────────────────────────────────────

pub async fn run_job(
    job_id: &str,
    ctx: &OpsContext,
    hub: &MigrationHub,
    policy: &OpsPolicy,
) -> Result<(), OpsError> {
    let mut job = hub.get_notnull(job_id).await?;
    if job.status.is_finished() {
        return Ok(());
    }
    if !ctx.is_postgres() {
        return block_notnull(
            ctx,
            hub,
            job_id,
            "SET NOT NULL 只支持 PostgreSQL",
            "preflight",
            0,
        )
        .await;
    }
    if ctx.db_name != job.db_name {
        return block_notnull(
            ctx,
            hub,
            job_id,
            &format!(
                "数据库名变化：任务绑定 {}，当前连接 {}",
                job.db_name, ctx.db_name
            ),
            "preflight",
            0,
        )
        .await;
    }
    if let Err(err) = policy.authorize_backfill(ctx.is_read_only, job.production_confirmed) {
        return block_notnull(ctx, hub, job_id, &err.to_string(), "policy", 0).await;
    }
    if job.engine.is_empty() {
        job = hub
            .update_notnull(job_id, |job| job.engine = ENGINE.to_string())
            .await?;
    } else if job.engine != ENGINE {
        return block_notnull(
            ctx,
            hub,
            job_id,
            &format!("任务引擎为 {}，禁止用 PostgreSQL 续跑", job.engine),
            "preflight:engine_mismatch",
            0,
        )
        .await;
    }

    let schema = sanitize_schema(job.schema.as_deref());
    let expected_hash = spec_hash(&schema, &job.table, &job.column);
    if job.spec_hash.is_empty() {
        let _ = hub
            .update_notnull(job_id, |job| job.spec_hash = expected_hash.clone())
            .await?;
    } else if job.spec_hash != expected_hash {
        return block_notnull(
            ctx,
            hub,
            job_id,
            "表/列/约束名与任务指纹不一致，拒绝续跑",
            "preflight:spec_mismatch",
            0,
        )
        .await;
    }

    job = hub
        .update_notnull(job_id, |job| {
            job.status = MigrationStatus::Running;
            job.phase = NotNullPhase::Preflight;
            job.run_count += 1;
            if job.started_at == 0 {
                job.started_at = now();
            }
            job.finished_at = None;
            job.last_error = None;
            job.blocking = None;
        })
        .await?;
    let _ = append_notnull_audit(
        ctx,
        "field_migration.notnull.run.started",
        "ok",
        &job,
        json!({
            "job_id": job_id,
            "engine": ENGINE,
            "run_count": job.run_count,
            "table": job.table,
            "column": job.column,
        }),
    );

    // ── preflight ─────────────────────────────────────────
    let snapshot = load_snapshot(ctx, &schema, &job.table).await?;
    if snapshot.columns.is_empty() {
        return block_notnull(ctx, hub, job_id, "表不存在或没有列", "preflight", 0).await;
    }
    let Some(column) = snapshot.find_column(&job.column).cloned() else {
        return block_notnull(
            ctx,
            hub,
            job_id,
            &format!("表 {} 缺少列 {}", job.table, job.column),
            "preflight",
            0,
        )
        .await;
    };
    if !column.nullable {
        // 已是 NOT NULL：no-op 完成，不做任何 DDL。
        let job = hub
            .update_notnull(job_id, |job| {
                job.status = MigrationStatus::Completed;
                job.phase = NotNullPhase::Done;
                job.finished_at = Some(now());
                job.notes.push("列已是 NOT NULL，无需执行（no-op）".into());
            })
            .await?;
        let _ = append_notnull_audit(
            ctx,
            "field_migration.notnull.completed",
            "ok",
            &job,
            json!({ "job_id": job_id, "noop": true }),
        );
        return Ok(());
    }
    if is_partitioned(ctx, &schema, &job.table).await? {
        return block_notnull(
            ctx,
            hub,
            job_id,
            "分区表不在支持范围（沿用 S2 边界）",
            "preflight:partitioned",
            0,
        )
        .await;
    }
    // 直接查 pg_index：快照里的 column_key 依赖 indkey 转换，PG 上不可靠。
    let pk_columns = primary_key_columns(ctx, &schema, &job.table).await?;
    if pk_columns.len() != 1 {
        return block_notnull(
            ctx,
            hub,
            job_id,
            "扫描分页需要单列主键（复合主键/无主键不在支持范围）",
            "preflight:pk",
            0,
        )
        .await;
    }
    let (pk_column, pk_type) = pk_columns[0].clone();
    let pk_kind = match pk_kind_from_column_type(&pk_type) {
        Ok(kind) => kind,
        Err(err) => {
            return block_notnull(ctx, hub, job_id, &err.to_string(), "preflight:pk_type", 0).await
        }
    };
    if pk_kind == PkKind::Text
        && !pk_collation_deterministic(ctx, &schema, &job.table, &pk_column).await?
    {
        return block_notnull(
            ctx,
            hub,
            job_id,
            "主键使用非确定性排序规则，禁止按 COLLATE \"C\" 分页",
            "preflight:pk_collation",
            0,
        )
        .await;
    }
    let version: i32 = sqlx::query_scalar("SELECT current_setting('server_version_num')::int")
        .fetch_one(ctx.db_client.pool.pg()?)
        .await?;
    if !version_supports_set_not_null(version) {
        return block_notnull(
            ctx,
            hub,
            job_id,
            &format!(
                "PostgreSQL {version} 过低：SET NOT NULL 需要 12+ 才能复用已验证 CHECK 跳过全表扫描"
            ),
            "preflight:pg_version",
            0,
        )
        .await;
    }

    // 残留约束决定起点：无 → Scan；NOT VALID → Validate；已验证 → SetNotNull。
    let mut phase = match constraint_state(ctx, &schema, &job.table, &job.constraint_name).await? {
        None => NotNullPhase::Scan,
        Some(false) => NotNullPhase::Validate,
        Some(true) => NotNullPhase::SetNotNull,
    };
    hub.update_notnull(job_id, |job| {
        job.phase = phase;
        job.scan.pk_kind = Some(pk_kind);
    })
    .await?;

    // ── 阶段机 ────────────────────────────────────────────
    loop {
        if !wait_control(hub, job_id, phase).await? {
            abort_job(ctx, hub, job_id).await?;
            return Ok(());
        }
        let next = match phase {
            NotNullPhase::Scan => {
                scan_phase(ctx, hub, job_id, &schema, &pk_column, pk_kind, policy).await?
            }
            NotNullPhase::AddCheck => add_check(ctx, hub, job_id, &schema, policy).await?,
            NotNullPhase::Validate => validate(ctx, hub, job_id, &schema, policy).await?,
            NotNullPhase::SetNotNull => set_not_null(ctx, hub, job_id, &schema, policy).await?,
            NotNullPhase::DropCheck => drop_check(ctx, hub, job_id, &schema, policy).await?,
            NotNullPhase::Verify => verify(ctx, hub, job_id, &schema).await?,
            NotNullPhase::Done | NotNullPhase::Preflight => None,
        };
        match next {
            Some(next) => {
                phase = next;
                hub.update_notnull(job_id, |job| job.phase = next).await?;
            }
            None => break,
        }
    }
    Ok(())
}

// ── 阶段实现 ───────────────────────────────────────────────

async fn scan_phase(
    ctx: &OpsContext,
    hub: &MigrationHub,
    job_id: &str,
    schema: &str,
    pk_column: &str,
    pk_kind: PkKind,
    policy: &OpsPolicy,
) -> Result<Option<NotNullPhase>, OpsError> {
    let mut job = hub.get_notnull(job_id).await?;
    let table = qualified(Some(schema), &job.table);
    let column = quote_ident_pg(&job.column);
    let key = quote_ident_pg(pk_column);
    let batch_size = job.config.batch_size.max(1);
    let config = job.config.clone();
    let mut last_pk = job.scan.last_pk.clone();
    let mut scanned_rows = job.scan.scanned_rows;
    let mut null_count = job.scan.null_count;
    let mut samples = job.scan.null_samples.clone();
    let mut first_at: Option<Instant> = None;

    loop {
        if !wait_control(hub, job_id, NotNullPhase::Scan).await? {
            return Ok(None);
        }
        // 先取整行页确定范围（进度与 NULL 密度无关），再取该范围内的 NULL 主键。
        let mut sql = format!("SELECT {key} AS pk_value FROM {table} WHERE TRUE");
        let mut binds: Vec<PgBind> = Vec::new();
        if let Some(value) = last_pk.as_deref() {
            binds.push(super::pg_verify::bind_value(pk_kind, value)?);
            sql.push_str(&format!(
                " AND {key} > {}",
                cast_clause(pk_kind, binds.len())
            ));
        }
        sql.push_str(&format!(" ORDER BY {}", order_expr(&key, pk_kind)));
        binds.push(PgBind::Int(batch_size as i64));
        sql.push_str(&format!(" LIMIT ${}", binds.len()));
        let page = fetch_rows_tx(ctx, &sql, &binds, policy).await?;
        let end_pk = page
            .iter()
            .filter_map(|row| row.get("pk_value").cloned())
            .last();
        let Some(end_pk) = end_pk else {
            break;
        };
        let page_len = page.len() as u64;

        let mut null_sql = format!("SELECT {key} AS pk_value FROM {table} WHERE {column} IS NULL");
        let mut null_binds: Vec<PgBind> = Vec::new();
        if let Some(value) = last_pk.as_deref() {
            null_binds.push(bind_value(pk_kind, value)?);
            null_sql.push_str(&format!(
                " AND {key} > {}",
                cast_clause(pk_kind, null_binds.len())
            ));
        }
        null_binds.push(bind_value(pk_kind, &end_pk)?);
        null_sql.push_str(&format!(
            " AND {key} <= {}",
            cast_clause(pk_kind, null_binds.len())
        ));
        null_sql.push_str(&format!(" ORDER BY {}", order_expr(&key, pk_kind)));
        null_binds.push(PgBind::Int(batch_size as i64));
        null_sql.push_str(&format!(" LIMIT ${}", null_binds.len()));
        let nulls = fetch_rows_tx(ctx, &null_sql, &null_binds, policy).await?;

        scanned_rows += page_len;
        null_count += nulls.len() as u64;
        for value in nulls.iter().filter_map(|row| row.get("pk_value")) {
            if samples.len() < SAMPLE_LIMIT {
                samples.push(value.clone());
            }
        }
        last_pk = Some(end_pk.clone());
        job = hub
            .update_notnull(job_id, |job| {
                job.scan.last_pk = Some(end_pk.clone());
                job.scan.scanned_rows = scanned_rows;
                job.scan.null_count = null_count;
                job.scan.null_samples = samples.clone();
            })
            .await?;
        wait_rate_limit(&config, &mut first_at, scanned_rows).await;
        if page_len < batch_size {
            break;
        }
    }

    if null_count > 0 {
        // 数据已变，续跑从头重扫：清空游标，保留样本供展示。
        hub.update_notnull(job_id, |job| {
            job.phase = NotNullPhase::Scan;
            job.scan.last_pk = None;
            job.scan.scanned_rows = 0;
            job.scan.null_count = 0;
            job.scan.null_samples = samples.clone();
        })
        .await?;
        block_notnull(
            ctx,
            hub,
            job_id,
            &format!(
                "列 {} 存在 {null_count} 行 NULL（已扫描 {scanned_rows} 行），先修复数据再重跑",
                job.column
            ),
            "scan:null_rows",
            null_count,
        )
        .await?;
        return Ok(None);
    }
    let _ = append_notnull_audit(
        ctx,
        "field_migration.notnull.scan.passed",
        "ok",
        &job,
        json!({ "job_id": job_id, "scanned_rows": scanned_rows, "null_count": 0 }),
    );
    Ok(Some(NotNullPhase::AddCheck))
}

async fn add_check(
    ctx: &OpsContext,
    hub: &MigrationHub,
    job_id: &str,
    schema: &str,
    policy: &OpsPolicy,
) -> Result<Option<NotNullPhase>, OpsError> {
    let job = hub.get_notnull(job_id).await?;
    if constraint_state(ctx, schema, &job.table, &job.constraint_name)
        .await?
        .is_some()
    {
        return Ok(Some(NotNullPhase::Validate));
    }
    let sql = format!(
        "ALTER TABLE {} ADD CONSTRAINT {} CHECK ({} IS NOT NULL) NOT VALID",
        qualified(Some(schema), &job.table),
        quote_ident_pg(&job.constraint_name),
        quote_ident_pg(&job.column)
    );
    if let Err(err) = exec_ddl(ctx, policy, &sql).await {
        block_notnull(
            ctx,
            hub,
            job_id,
            &format!("ADD CHECK NOT VALID 失败: {err}"),
            "add_check",
            0,
        )
        .await?;
        return Ok(None);
    }
    hub.update_notnull(job_id, |job| {
        job.phase = NotNullPhase::Validate;
        job.notes.push("add_check: ok".into());
    })
    .await?;
    Ok(Some(NotNullPhase::Validate))
}

async fn validate(
    ctx: &OpsContext,
    hub: &MigrationHub,
    job_id: &str,
    schema: &str,
    policy: &OpsPolicy,
) -> Result<Option<NotNullPhase>, OpsError> {
    let job = hub.get_notnull(job_id).await?;
    let sql = format!(
        "ALTER TABLE {} VALIDATE CONSTRAINT {}",
        qualified(Some(schema), &job.table),
        quote_ident_pg(&job.constraint_name)
    );
    if let Err(err) = exec_ddl(ctx, policy, &sql).await {
        // 扫描后又有 NULL 写入：保留 NOT VALID 约束，退回 Scan 重扫。
        hub.update_notnull(job_id, |job| {
            job.phase = NotNullPhase::Scan;
            job.scan.last_pk = None;
            job.scan.scanned_rows = 0;
            job.scan.null_count = 0;
        })
        .await?;
        block_notnull(
            ctx,
            hub,
            job_id,
            &format!("VALIDATE CONSTRAINT 失败（扫描后出现 NULL？）: {err}"),
            "validate:violations",
            0,
        )
        .await?;
        return Ok(None);
    }
    hub.update_notnull(job_id, |job| {
        job.phase = NotNullPhase::SetNotNull;
        job.notes.push("validate: ok".into());
    })
    .await?;
    Ok(Some(NotNullPhase::SetNotNull))
}

async fn set_not_null(
    ctx: &OpsContext,
    hub: &MigrationHub,
    job_id: &str,
    schema: &str,
    policy: &OpsPolicy,
) -> Result<Option<NotNullPhase>, OpsError> {
    let job = hub.get_notnull(job_id).await?;
    let sql = format!(
        "ALTER TABLE {} ALTER COLUMN {} SET NOT NULL",
        qualified(Some(schema), &job.table),
        quote_ident_pg(&job.column)
    );
    if let Err(err) = exec_ddl(ctx, policy, &sql).await {
        block_notnull(
            ctx,
            hub,
            job_id,
            &format!("SET NOT NULL 失败: {err}"),
            "set_not_null",
            0,
        )
        .await?;
        return Ok(None);
    }
    hub.update_notnull(job_id, |job| {
        job.phase = NotNullPhase::DropCheck;
        job.notes.push("set_not_null: ok".into());
    })
    .await?;
    Ok(Some(NotNullPhase::DropCheck))
}

async fn drop_check(
    ctx: &OpsContext,
    hub: &MigrationHub,
    job_id: &str,
    schema: &str,
    policy: &OpsPolicy,
) -> Result<Option<NotNullPhase>, OpsError> {
    let job = hub.get_notnull(job_id).await?;
    if constraint_state(ctx, schema, &job.table, &job.constraint_name)
        .await?
        .is_some()
    {
        let sql = format!(
            "ALTER TABLE {} DROP CONSTRAINT {}",
            qualified(Some(schema), &job.table),
            quote_ident_pg(&job.constraint_name)
        );
        if let Err(err) = exec_ddl(ctx, policy, &sql).await {
            block_notnull(
                ctx,
                hub,
                job_id,
                &format!("DROP CONSTRAINT 失败: {err}"),
                "drop_check",
                0,
            )
            .await?;
            return Ok(None);
        }
    }
    hub.update_notnull(job_id, |job| {
        job.phase = NotNullPhase::Verify;
        job.notes.push("drop_check: ok".into());
    })
    .await?;
    Ok(Some(NotNullPhase::Verify))
}

async fn verify(
    ctx: &OpsContext,
    hub: &MigrationHub,
    job_id: &str,
    schema: &str,
) -> Result<Option<NotNullPhase>, OpsError> {
    let job = hub.get_notnull(job_id).await?;
    let schema_lit = escape_sql_string(schema);
    let table_lit = escape_sql_string(&job.table);
    let column_lit = escape_sql_string(&job.column);
    let sql = format!(
        "SELECT CASE WHEN a.attnotnull THEN 'YES' ELSE 'NO' END AS not_null, \
                CASE WHEN EXISTS (SELECT 1 FROM pg_constraint con \
                                  WHERE con.conrelid = c.oid AND con.conname = '{}') \
                     THEN 'YES' ELSE 'NO' END AS constraint_present \
         FROM pg_attribute a \
         JOIN pg_class c ON c.oid = a.attrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = '{schema_lit}' AND c.relname = '{table_lit}' \
           AND a.attname = '{column_lit}' AND a.attnum > 0",
        escape_sql_string(&job.constraint_name)
    );
    let rows = sqlx::query(&sql)
        .fetch_all(ctx.db_client.pool.pg()?)
        .await?;
    let rows = convert_pg_rows(&rows);
    let not_null = rows
        .first()
        .and_then(|row| row.get("not_null"))
        .map(|value| value == "YES")
        .unwrap_or(false);
    let constraint_present = rows
        .first()
        .and_then(|row| row.get("constraint_present"))
        .map(|value| value == "YES")
        .unwrap_or(false);
    if !not_null || constraint_present {
        block_notnull(
            ctx,
            hub,
            job_id,
            &format!("最终校验失败：not_null={not_null} 残留约束={constraint_present}"),
            "verify",
            0,
        )
        .await?;
        return Ok(None);
    }
    let job = hub
        .update_notnull(job_id, |job| {
            job.status = MigrationStatus::Completed;
            job.phase = NotNullPhase::Done;
            job.finished_at = Some(now());
            job.last_error = None;
            job.notes.push("verify: ok".into());
        })
        .await?;
    let _ = append_notnull_audit(
        ctx,
        "field_migration.notnull.completed",
        "ok",
        &job,
        json!({
            "job_id": job_id,
            "table": job.table,
            "column": job.column,
            "scanned_rows": job.scan.scanned_rows,
            "execution": job.execution,
        }),
    );
    Ok(None)
}

// ── 基础设施 ───────────────────────────────────────────────

/// 只读批事务：`SET LOCAL` 三条 + 查询 + 提交；异常时 sqlx 自动回滚。
async fn fetch_rows_tx(
    ctx: &OpsContext,
    sql: &str,
    binds: &[PgBind],
    policy: &OpsPolicy,
) -> Result<Vec<crate::ops::mysql_rows::RowMap>, OpsError> {
    let pool: &PgPool = ctx.db_client.pool.pg()?;
    let mut tx = pool.begin().await?;
    for statement in pg_timeout_statements(policy) {
        sqlx::query(&statement).execute(&mut *tx).await?;
    }
    let rows = bind_query(sql, binds).fetch_all(&mut *tx).await?;
    let converted = convert_pg_rows(&rows);
    tx.commit().await?;
    Ok(converted)
}

/// DDL 单语句自动提交（不进事务块）；session 级锁超时，执行后 RESET。
async fn exec_ddl(ctx: &OpsContext, policy: &OpsPolicy, sql: &str) -> Result<(), OpsError> {
    let pool: &PgPool = ctx.db_client.pool.pg()?;
    let mut conn = pool.acquire().await?;
    sqlx::query(&format!(
        "SET lock_timeout = '{}s'",
        policy.lock_wait_timeout_secs.max(1)
    ))
    .execute(&mut *conn)
    .await?;
    sqlx::query("SET statement_timeout = '0'")
        .execute(&mut *conn)
        .await?;
    let result = sqlx::query(sql).execute(&mut *conn).await;
    let _ = sqlx::query("RESET lock_timeout").execute(&mut *conn).await;
    let _ = sqlx::query("RESET statement_timeout")
        .execute(&mut *conn)
        .await;
    result?;
    Ok(())
}

/// `Some(validated)`；`None` 表示约束不存在。
async fn constraint_state(
    ctx: &OpsContext,
    schema: &str,
    table: &str,
    name: &str,
) -> Result<Option<bool>, OpsError> {
    let rows = sqlx::query(
        "SELECT con.convalidated::text AS validated \
         FROM pg_constraint con \
         JOIN pg_class c ON c.oid = con.conrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = $1 AND c.relname = $2 AND con.conname = $3",
    )
    .bind(schema)
    .bind(table)
    .bind(name)
    .fetch_all(ctx.db_client.pool.pg()?)
    .await?;
    let rows = convert_pg_rows(&rows);
    Ok(rows
        .first()
        .and_then(|row| row.get("validated"))
        .map(|value| value == "true" || value == "t"))
}

/// 单列主键的列名与类型；复合主键返回多行，由调用方判为 BLOCK。
async fn primary_key_columns(
    ctx: &OpsContext,
    schema: &str,
    table: &str,
) -> Result<Vec<(String, String)>, OpsError> {
    let rows = sqlx::query(
        "SELECT a.attname::text AS name, \
                format_type(a.atttypid, a.atttypmod)::text AS coltype \
         FROM pg_index i \
         JOIN pg_class c ON c.oid = i.indrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = ANY (i.indkey) \
         WHERE n.nspname = $1 AND c.relname = $2 AND i.indisprimary",
    )
    .bind(schema)
    .bind(table)
    .fetch_all(ctx.db_client.pool.pg()?)
    .await?;
    let rows = convert_pg_rows(&rows);
    Ok(rows
        .iter()
        .map(|row| {
            (
                row.get("name").cloned().unwrap_or_default(),
                row.get("coltype").cloned().unwrap_or_default(),
            )
        })
        .collect())
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

async fn wait_control(
    hub: &MigrationHub,
    job_id: &str,
    phase: NotNullPhase,
) -> Result<bool, OpsError> {
    loop {
        match hub.control_value(job_id).await {
            CONTROL_ABORT => return Ok(false),
            CONTROL_PAUSE => {
                let job = hub.get_notnull(job_id).await?;
                if matches!(
                    job.status,
                    MigrationStatus::Running | MigrationStatus::Pending
                ) {
                    hub.update_notnull(job_id, |job| {
                        job.status = MigrationStatus::Paused;
                    })
                    .await?;
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            _ => {
                let job = hub.get_notnull(job_id).await?;
                if job.status == MigrationStatus::Paused {
                    hub.update_notnull(job_id, |job| {
                        job.status = MigrationStatus::Running;
                        job.phase = phase;
                    })
                    .await?;
                }
                return Ok(true);
            }
        }
    }
}

async fn abort_job(ctx: &OpsContext, hub: &MigrationHub, job_id: &str) -> Result<(), OpsError> {
    let job = hub
        .update_notnull(job_id, |job| {
            job.status = MigrationStatus::Aborted;
            job.finished_at = Some(now());
        })
        .await?;
    let _ = append_notnull_audit(
        ctx,
        "field_migration.notnull.aborted",
        "aborted",
        &job,
        json!({ "job_id": job_id, "phase": job.phase.as_str() }),
    );
    Ok(())
}

async fn block_notnull(
    ctx: &OpsContext,
    hub: &MigrationHub,
    job_id: &str,
    message: &str,
    phase: &str,
    total: u64,
) -> Result<(), OpsError> {
    let job = hub
        .update_notnull(job_id, |job| {
            job.status = MigrationStatus::Blocked;
            job.finished_at = Some(now());
            job.last_error = Some(message.to_string());
            job.blocking = Some(BlockingDetail {
                batch_no: job.run_count.max(1),
                last_pk: job.scan.last_pk.clone(),
                end_pk: None,
                phase: phase.to_string(),
                total_diffs: total,
                pks: job.scan.null_samples.clone(),
                has_more: false,
                differences: Vec::new(),
                verify: None,
                message: message.to_string(),
            });
        })
        .await?;
    let _ = append_notnull_audit(
        ctx,
        "field_migration.notnull.blocked",
        "blocked",
        &job,
        json!({
            "job_id": job_id,
            "phase": phase,
            "message": message,
            "total": total,
            "samples": job.blocking.as_ref().map(|b| b.pks.clone()),
        }),
    );
    Ok(())
}

fn append_notnull_audit(
    ctx: &OpsContext,
    event: &str,
    outcome: &str,
    job: &NotNullJob,
    detail: serde_json::Value,
) -> Result<(), OpsError> {
    let Some(audit) = ctx.audit.as_ref() else {
        return Ok(());
    };
    let mut record = AuditRecord::new(event, outcome);
    record.plan_id = Some(job.plan_id.clone());
    record.db_id = Some(job.db_id.clone());
    record.db_name = Some(job.db_name.clone());
    record.action_id = Some(format!("field_migration.notnull:{}", job.job_id));
    record.actor = Some("local-user".into());
    record.detail = detail;
    audit.append(&record)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_gate_matches_pg12_boundary() {
        assert!(!version_supports_set_not_null(119_999));
        assert!(version_supports_set_not_null(120_000));
        assert!(version_supports_set_not_null(170_000));
    }

    #[test]
    fn constraint_name_matches_s1_script() {
        assert_eq!(constraint_name("payload"), "fm_not_null_payload");
    }

    #[test]
    fn spec_hash_binds_schema_table_and_column() {
        let base = spec_hash("public", "orders", "payload");
        assert_eq!(base, spec_hash("public", "orders", "payload"));
        assert_ne!(base, spec_hash("other", "orders", "payload"));
        assert_ne!(base, spec_hash("public", "orders2", "payload"));
        assert_ne!(base, spec_hash("public", "orders", "payload2"));
    }
}
