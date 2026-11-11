//! PostgreSQL 回填执行器（S2）。
//!
//! 与 MySQL 路径隔离：
//! - 主键 keyset 分页，`last_pk` 按类型绑定，不拼字面量；
//! - 文本主键分页/比较统一 `COLLATE "C"`；
//! - 单批 `INSERT ... SELECT ... ORDER BY ... LIMIT ... ON CONFLICT DO UPDATE`；
//! - 默认每批一事务，失败只回滚当前批；`single_transaction` 才整任务原子；
//! - 写前预检目标表的 `ON CONFLICT` 唯一索引/主键；
//! - checkpoint 绑定 engine / pk_kind / target_fingerprint，跨引擎续跑拒绝；
//! - 不执行改类型、`SET NOT NULL`、改 collation。

use std::collections::HashMap;
use std::time::Instant;

use serde_json::json;
use sqlx::pool::PoolConnection;
use sqlx::{Acquire, PgConnection};

use crate::ops::error::OpsError;
use crate::ops::policy::OpsPolicy;
use crate::ops::registry::OpsContext;
use crate::ops::types::{now, ColumnSnapshot, IndexColumn, IndexDefinition, TableSnapshot};
use crate::sql::util::{escape_sql_string, quote_ident_pg};

use super::backfill::{
    append_audit, block_message, block_on_verify, execute_create, validate_create_statement,
    wait_for_control, wait_rate_limit,
};
use super::migration_store::{
    BackfillSpec, BatchRecord, BatchVerify, Checkpoint, DiffDetail, MigrationHub, MigrationPhase,
    MigrationStatus, MigrationStore, PkKind,
};
use super::pg_scripts::value_expr;
use super::pg_verify::{
    self, bind_value, cast_clause, fetch_on_pool, order_expr, push_range, PgBind,
    VALIDATION_MODE_SNAPSHOT,
};
use super::types::ColumnProfile;
use crate::ops::pg_rows::{convert_pg_rows, fetch_rows};

const ENGINE: &str = "postgresql";
const DIFF_PAGE_SIZE: u32 = 100;

// ── 预检 ───────────────────────────────────────────────────

pub(crate) async fn run_inner(
    job_id: &str,
    ctx: &OpsContext,
    hub: &MigrationHub,
    policy: &OpsPolicy,
) -> Result<(), OpsError> {
    let mut job = hub.get(job_id).await?;
    if job.status.is_finished() {
        return Ok(());
    }
    if !ctx.is_postgres() {
        return block_message(ctx, hub, job_id, "回填只支持 PostgreSQL", "preflight").await;
    }
    if ctx.db_name != job.db_name {
        return block_message(
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
        return block_message(ctx, hub, job_id, &err.to_string(), "policy").await;
    }

    // 引擎绑定与续跑拒绝。
    if job.engine.is_empty() {
        job = hub
            .update(job_id, |job| job.engine = ENGINE.to_string())
            .await?;
    } else if job.engine != ENGINE {
        return block_message(
            ctx,
            hub,
            job_id,
            &format!("任务引擎为 {}，禁止用 PostgreSQL 续跑", job.engine),
            "preflight:engine_mismatch",
        )
        .await;
    }
    if let Some(checkpoint) = job.checkpoint.as_ref() {
        if checkpoint.engine != ENGINE {
            return block_message(
                ctx,
                hub,
                job_id,
                &format!(
                    "checkpoint 引擎为 {}，禁止复用到 PostgreSQL",
                    checkpoint.engine
                ),
                "preflight:engine_mismatch",
            )
            .await;
        }
    }
    if let Some(execution) = job.execution.as_ref() {
        if !execution.resumable && job.run_count > 0 {
            return block_message(
                ctx,
                hub,
                job_id,
                "该任务为单事务模式，失败后不可续跑，请重建任务",
                "preflight:not_resumable",
            )
            .await;
        }
    }

    let mut spec = job.spec.clone();
    let schema = spec
        .schema
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "public".to_string());
    let source = pg_verify::qualified(Some(&schema), &spec.source_table);
    let destination = pg_verify::qualified(Some(&schema), &spec.target_table);
    let resume_after = job
        .checkpoint
        .as_ref()
        .and_then(|checkpoint| checkpoint.last_pk.clone());

    job = hub
        .update(job_id, |job| {
            job.status = MigrationStatus::Running;
            job.phase = MigrationPhase::Preflight;
            job.run_count += 1;
            if job.started_at == 0 {
                job.started_at = now();
            }
            job.finished_at = None;
            job.last_error = None;
            job.blocking = None;
            job.progress.resume_after_pk = resume_after.clone();
        })
        .await?;
    let _ = append_audit(
        ctx,
        "field_migration.backfill.run.started",
        "ok",
        &job,
        json!({
            "job_id": job_id,
            "engine": ENGINE,
            "run_count": job.run_count,
            "resume_after_pk": job.progress.resume_after_pk,
            "batch_size": job.config.batch_size,
            "single_transaction": job.config.single_transaction,
        }),
    );

    // ── 源表快照 / 主键类型 ────────────────────────────────
    let source_snapshot = load_snapshot(ctx, &schema, &spec.source_table).await?;
    if source_snapshot.columns.is_empty() {
        return block_message(ctx, hub, job_id, "源表不存在或没有列", "preflight").await;
    }
    let source_hash = source_snapshot.snapshot_hash();
    if job.source_schema_hash.is_empty() {
        job = hub
            .update(job_id, |job| job.source_schema_hash = source_hash.clone())
            .await?;
    } else if job.source_schema_hash != source_hash {
        return block_message(
            ctx,
            hub,
            job_id,
            "源表结构已变化，禁止在旧 checkpoint 上继续",
            "preflight:schema_drift",
        )
        .await;
    }
    let pk_column = source_snapshot
        .find_column(&spec.pk_column)
        .ok_or_else(|| OpsError::Validation(format!("源表缺少主键列 {}", spec.pk_column)))?;
    let pk_kind = pk_kind_from_column_type(&pk_column.column_type);
    let pk_kind = match pk_kind {
        Ok(kind) => kind,
        Err(err) => {
            return block_message(ctx, hub, job_id, &err.to_string(), "preflight:pk_type").await
        }
    };
    if let Some(checkpoint) = job.checkpoint.as_ref() {
        if let Some(previous) = checkpoint.pk_kind {
            if previous != pk_kind {
                return block_message(
                    ctx,
                    hub,
                    job_id,
                    &format!("checkpoint 主键类型为 {previous:?}，当前为 {pk_kind:?}，拒绝续跑"),
                    "preflight:pk_kind_mismatch",
                )
                .await;
            }
        }
    }
    job = hub
        .update(job_id, |job| job.spec.pk_kind = Some(pk_kind))
        .await?;
    spec.pk_kind = Some(pk_kind);
    if pk_kind == PkKind::Text
        && !pk_collation_deterministic(ctx, &schema, &spec.source_table, &spec.pk_column).await?
    {
        return block_message(
            ctx,
            hub,
            job_id,
            "主键使用非确定性排序规则，禁止按 COLLATE \"C\" 分页",
            "preflight:pk_collation",
        )
        .await;
    }

    // ── 目标表（只允许 CREATE） ───────────────────────────
    let mut target_snapshot = load_snapshot(ctx, &schema, &spec.target_table).await?;
    if target_snapshot.columns.is_empty() {
        let create_sql = spec
            .create_table_sql
            .clone()
            .ok_or_else(|| OpsError::Validation("目标表不存在且没有建表脚本，无法继续".into()))?;
        validate_create_statement(&create_sql)?;
        execute_create(ctx, &create_sql).await?;
        job = hub
            .update(job_id, |job| job.target_created_by_job = true)
            .await?;
        let _ = append_audit(
            ctx,
            "field_migration.backfill.target_created",
            "ok",
            &job,
            json!({ "job_id": job_id, "target_table": spec.target_table }),
        );
        target_snapshot = load_snapshot(ctx, &schema, &spec.target_table).await?;
        if target_snapshot.columns.is_empty() {
            return block_message(ctx, hub, job_id, "目标表创建后仍不可见", "preflight").await;
        }
    }
    for column in &spec.target_columns {
        if target_snapshot.find_column(&column.name).is_none() {
            return block_message(
                ctx,
                hub,
                job_id,
                &format!("目标表缺少列 {}", column.name),
                "preflight",
            )
            .await;
        }
    }
    let target_hash = target_snapshot.snapshot_hash();
    if job.target_schema_hash.is_empty() {
        job = hub
            .update(job_id, |job| job.target_schema_hash = target_hash.clone())
            .await?;
    } else if job.target_schema_hash != target_hash {
        return block_message(
            ctx,
            hub,
            job_id,
            "目标表结构已变化，禁止继续写入",
            "preflight:target_schema_drift",
        )
        .await;
    }
    if let Some(checkpoint) = job.checkpoint.as_ref() {
        if let Some(fingerprint) = checkpoint.target_fingerprint.as_deref() {
            if fingerprint != target_hash {
                return block_message(
                    ctx,
                    hub,
                    job_id,
                    "目标表指纹与 checkpoint 不一致，禁止继续写入",
                    "preflight:target_schema_drift",
                )
                .await;
            }
        }
    }

    // ON CONFLICT 目标索引预检。
    if !has_on_conflict_target(ctx, &schema, &spec.target_table, &spec.pk_column).await? {
        return block_message(
            ctx,
            hub,
            job_id,
            &format!(
                "目标表 {} 没有可用于 ON CONFLICT ({}) 的单列主键或唯一索引",
                spec.target_table, spec.pk_column
            ),
            "preflight:on_conflict_target",
        )
        .await;
    }

    // 目标表已存在且非空、又不是本任务创建的，拒绝覆盖。
    if job.checkpoint.is_none() && !job.target_created_by_job {
        let target_rows = count_rows(ctx, &spec, &destination, None, None).await?;
        if target_rows > 0 {
            return block_message(
                ctx,
                hub,
                job_id,
                &format!("目标表已存在且包含 {target_rows} 行，但本任务没有 checkpoint，拒绝覆盖"),
                "preflight:target_not_empty",
            )
            .await;
        }
    }

    // ── 源行数基线 ────────────────────────────────────────
    let expected = count_rows(ctx, &spec, &source, None, None).await?;
    if job.expected_rows == 0 {
        hub.update(job_id, |job| {
            job.expected_rows = expected;
            job.progress.expected_rows = expected;
            job.progress.total_batches = expected.div_ceil(job.config.batch_size);
            job.progress.remaining_rows = expected;
        })
        .await?;
    } else if job.expected_rows != expected {
        return block_message(
            ctx,
            hub,
            job_id,
            &format!(
                "源表行数漂移：基线与 {}，当前 {}，禁止续跑",
                job.expected_rows, expected
            ),
            "preflight:source_drift",
        )
        .await;
    }
    job = hub
        .update(job_id, |job| {
            job.status = MigrationStatus::Running;
            job.phase = MigrationPhase::Backfilling;
            job.progress.expected_rows = job.expected_rows;
            job.progress.total_batches = job.expected_rows.div_ceil(job.config.batch_size);
            job.progress.success_rows = job
                .checkpoint
                .as_ref()
                .map(|checkpoint| checkpoint.written_rows)
                .unwrap_or(0);
            job.progress.resume_skipped_batches = job
                .checkpoint
                .as_ref()
                .map(|checkpoint| checkpoint.completed_batches)
                .unwrap_or(0);
            job.progress.remaining_rows =
                job.expected_rows.saturating_sub(job.progress.success_rows);
        })
        .await?;

    let completed_batches = job
        .checkpoint
        .as_ref()
        .map(|checkpoint| checkpoint.completed_batches)
        .unwrap_or(0);
    let checkpoint_pk = job
        .checkpoint
        .as_ref()
        .and_then(|checkpoint| checkpoint.last_pk.clone());
    if completed_batches > 0 && checkpoint_pk.is_none() {
        return block_message(
            ctx,
            hub,
            job_id,
            "checkpoint 缺少边界主键，拒绝从头静默重放",
            "preflight:checkpoint_invalid",
        )
        .await;
    }

    // 续跑只读抽检最后一批。
    if completed_batches > 0 {
        if let Some(record) = job
            .recent_batches
            .iter()
            .find(|record| record.batch_no == completed_batches)
            .cloned()
        {
            let spot = pg_verify::verify_range(
                ctx,
                &spec,
                record.last_pk.as_deref(),
                Some(&record.end_pk),
            )
            .await?;
            if !spot.passed() {
                let diffs = collect_range_diffs(
                    ctx,
                    &spec,
                    record.last_pk.as_deref(),
                    &record.end_pk,
                    &spot,
                    false,
                )
                .await?;
                block_on_verify(
                    ctx,
                    hub,
                    job_id,
                    record.batch_no,
                    record.last_pk.clone(),
                    Some(record.end_pk.clone()),
                    "resume_spot_check",
                    spot,
                    diffs,
                )
                .await?;
                return Ok(());
            }
        }
    }

    let success_rows = job
        .checkpoint
        .as_ref()
        .map(|checkpoint| checkpoint.written_rows)
        .unwrap_or(0);
    let mut batch_index: u64 = completed_batches;
    let mut last_pk: Option<String> = checkpoint_pk;
    let batch_size = job.config.batch_size;

    if job.config.single_transaction {
        let pool = ctx.db_client.pool.pg()?;
        let mut conn = pool.acquire().await?;
        sqlx::query("BEGIN ISOLATION LEVEL REPEATABLE READ")
            .execute(&mut *conn)
            .await?;
        for statement in pg_timeout_statements(policy) {
            sqlx::query(&statement).execute(&mut *conn).await?;
        }
        let result = run_loop(
            ctx,
            hub,
            job_id,
            &spec,
            &source,
            &destination,
            policy,
            batch_size,
            success_rows,
            &mut batch_index,
            &mut last_pk,
            Some(&mut conn),
        )
        .await;
        match result {
            Ok(final_rows) => {
                let current = hub.get(job_id).await?;
                if !current.is_active_status() {
                    let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
                    return Ok(());
                }
                sqlx::query("COMMIT").execute(&mut *conn).await?;
                let checkpoint_rows = last_pk.clone();
                job = hub
                    .update(job_id, |job| {
                        let checkpoint = job
                            .checkpoint
                            .get_or_insert_with(|| empty_checkpoint(&target_hash, pk_kind));
                        checkpoint.completed_batches = batch_index;
                        checkpoint.last_pk = checkpoint_rows.clone();
                        checkpoint.written_rows = final_rows;
                        checkpoint.engine = ENGINE.to_string();
                        checkpoint.pk_kind = Some(pk_kind);
                        checkpoint.target_fingerprint = Some(target_hash.clone());
                        checkpoint.updated_at = now();
                    })
                    .await?;
                let _ = append_audit(
                    ctx,
                    "field_migration.backfill.single_transaction_committed",
                    "ok",
                    &job,
                    json!({
                        "job_id": job_id,
                        "batch_no": batch_index,
                        "written_rows": job.checkpoint.as_ref().map(|c| c.written_rows),
                    }),
                );
            }
            Err(err) => {
                let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
                return Err(err);
            }
        }
    } else {
        run_loop(
            ctx,
            hub,
            job_id,
            &spec,
            &source,
            &destination,
            policy,
            batch_size,
            success_rows,
            &mut batch_index,
            &mut last_pk,
            None,
        )
        .await?;
        let current = hub.get(job_id).await?;
        if !current.is_active_status() {
            return Ok(());
        }
    }

    // ── 最终全表校验 ──────────────────────────────────────
    job = hub
        .update(job_id, |job| job.phase = MigrationPhase::Verifying)
        .await?;
    let final_verify = pg_verify::verify_full(ctx, &spec).await?;
    if !final_verify.passed() {
        let diffs = collect_full_diffs(ctx, &spec, &final_verify).await?;
        let located = locate_first_failed_batch(ctx, &spec, job.config.batch_size).await?;
        match located {
            Some((batch_no, start_pk, batch_end_pk, verify, batch_diffs)) => {
                block_on_verify(
                    ctx,
                    hub,
                    job_id,
                    batch_no,
                    start_pk,
                    batch_end_pk,
                    "final_recheck",
                    verify,
                    batch_diffs,
                )
                .await?;
            }
            None => {
                block_on_verify(
                    ctx,
                    hub,
                    job_id,
                    job.progress.total_batches.max(batch_index),
                    last_pk.clone(),
                    None,
                    "final",
                    final_verify,
                    diffs,
                )
                .await?;
            }
        }
        return Ok(());
    }

    let job = hub
        .update(job_id, |job| {
            job.status = MigrationStatus::Completed;
            job.phase = MigrationPhase::Completed;
            job.finished_at = Some(now());
            job.progress.current_batch = job.progress.total_batches;
            job.progress.success_rows = job.expected_rows;
            job.progress.processed_rows = job.expected_rows;
            job.progress.failed_rows = 0;
            job.progress.remaining_rows = 0;
            job.progress.connection_id = None;
            let checkpoint = job
                .checkpoint
                .get_or_insert_with(|| empty_checkpoint(&target_hash, pk_kind));
            checkpoint.completed_batches = job.progress.total_batches;
            checkpoint.written_rows = job.expected_rows;
            checkpoint.verified = true;
            checkpoint.updated_at = now();
            checkpoint.engine = ENGINE.to_string();
            checkpoint.pk_kind = Some(pk_kind);
            checkpoint.target_fingerprint = Some(target_hash.clone());
            if checkpoint.last_pk.is_none() {
                checkpoint.last_pk = last_pk.clone();
            }
        })
        .await?;
    let _ = append_audit(
        ctx,
        "field_migration.backfill.completed",
        "ok",
        &job,
        json!({
            "job_id": job_id,
            "expected_rows": job.expected_rows,
            "success_rows": job.progress.success_rows,
            "total_batches": job.progress.total_batches,
            "final_verify": final_verify,
            "validation_mode": VALIDATION_MODE_SNAPSHOT,
        }),
    );
    Ok(())
}

fn empty_checkpoint(target_hash: &str, pk_kind: PkKind) -> Checkpoint {
    Checkpoint {
        completed_batches: 0,
        last_pk: None,
        written_rows: 0,
        verified: false,
        verify: None,
        engine: ENGINE.to_string(),
        pk_kind: Some(pk_kind),
        target_fingerprint: Some(target_hash.to_string()),
        updated_at: now(),
    }
}

// ── 批次循环 ───────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn run_loop(
    ctx: &OpsContext,
    hub: &MigrationHub,
    job_id: &str,
    spec: &BackfillSpec,
    source: &str,
    destination: &str,
    policy: &OpsPolicy,
    batch_size: u64,
    mut success_rows: u64,
    batch_index: &mut u64,
    last_pk: &mut Option<String>,
    mut single_conn: Option<&mut PoolConnection<sqlx::Postgres>>,
) -> Result<u64, OpsError> {
    let kind = spec
        .pk_kind
        .ok_or_else(|| OpsError::Validation("缺少主键类型，无法回填".into()))?;
    let job = hub.get(job_id).await?;
    let single = single_conn.is_some();
    let target_fingerprint = job.target_schema_hash.clone();
    let mut first_write_at: Option<Instant> = None;
    let mut written_for_rate: u64 = 0;

    loop {
        if !wait_for_control(hub, job_id).await? {
            let job = hub
                .update(job_id, |job| {
                    job.status = MigrationStatus::Aborted;
                    job.phase = MigrationPhase::Aborted;
                    job.finished_at = Some(now());
                })
                .await?;
            let _ = append_audit(
                ctx,
                "field_migration.backfill.aborted",
                "aborted",
                &job,
                json!({ "job_id": job_id, "batch_no": *batch_index + 1 }),
            );
            return Ok(success_rows);
        }

        let pks = fetch_pk_page(ctx, spec, source, last_pk.as_deref(), batch_size).await?;
        if pks.is_empty() {
            break;
        }
        let batch_no = *batch_index + 1;
        let first_pk = pks.first().cloned().unwrap_or_default();
        let end_pk = pks.last().cloned().unwrap_or_default();
        let source_rows = pks.len() as u64;
        let started = Instant::now();

        hub.update(job_id, |job| {
            job.progress.current_batch = batch_no;
            job.progress.current_first_pk = Some(first_pk.clone());
            job.progress.last_pk = last_pk.clone();
        })
        .await?;

        let pre = if let Some(conn) = single_conn.as_deref_mut() {
            pg_verify::run_verify(conn, spec, last_pk.as_deref(), Some(&end_pk)).await?
        } else {
            pg_verify::verify_range(ctx, spec, last_pk.as_deref(), Some(&end_pk)).await?
        };
        if pre.v3 > 0 || pre.v4 > 0 {
            let diffs =
                collect_range_diffs(ctx, spec, last_pk.as_deref(), &end_pk, &pre, true).await?;
            block_on_verify(
                ctx,
                hub,
                job_id,
                batch_no,
                last_pk.clone(),
                Some(end_pk.clone()),
                "precheck",
                pre,
                diffs,
            )
            .await?;
            return Ok(success_rows);
        }

        let already_complete =
            pre.source_rows == pre.target_rows && pre.v2 == 0 && pre.v3 == 0 && pre.v4 == 0;
        let mut written_rows = 0u64;
        if !already_complete {
            if let Some(conn) = single_conn.as_deref_mut() {
                execute_batch_on_conn(
                    hub,
                    job_id,
                    spec,
                    source,
                    destination,
                    last_pk.as_deref(),
                    &end_pk,
                    source_rows,
                    batch_no,
                    success_rows,
                    conn,
                )
                .await?;
            } else {
                execute_batch_in_tx(
                    ctx,
                    hub,
                    job_id,
                    spec,
                    source,
                    destination,
                    last_pk.as_deref(),
                    &end_pk,
                    source_rows,
                    batch_no,
                    success_rows,
                    policy,
                )
                .await?;
            }
            written_rows = source_rows;
            written_for_rate += source_rows;
            wait_rate_limit(&job.config, &mut first_write_at, written_for_rate).await;
        }

        let verify = if let Some(conn) = single_conn.as_deref_mut() {
            pg_verify::run_verify(conn, spec, last_pk.as_deref(), Some(&end_pk)).await?
        } else {
            pg_verify::verify_range(ctx, spec, last_pk.as_deref(), Some(&end_pk)).await?
        };
        if !verify.passed() {
            let diffs =
                collect_range_diffs(ctx, spec, last_pk.as_deref(), &end_pk, &verify, false).await?;
            block_on_verify(
                ctx,
                hub,
                job_id,
                batch_no,
                last_pk.clone(),
                Some(end_pk.clone()),
                "postcheck",
                verify,
                diffs,
            )
            .await?;
            return Ok(success_rows);
        }

        success_rows += source_rows;
        let duration_ms = started.elapsed().as_millis() as u64;

        append_audit(
            ctx,
            "field_migration.backfill.batch",
            "ok",
            &job,
            json!({
                "job_id": job_id,
                "batch_no": batch_no,
                "range": { "last_pk": last_pk, "end_pk": end_pk },
                "written_rows": written_rows,
                "source_rows": source_rows,
                "verify": &verify,
                "verified": true,
                "duration_ms": duration_ms,
            }),
        )?;

        // 单事务模式在 COMMIT 前不推进 checkpoint；失败由整体回滚兜底。
        if !single {
            let end_for_checkpoint = end_pk.clone();
            let verify_for_checkpoint = verify.clone();
            let last_for_record = last_pk.clone();
            hub.update(job_id, |job| {
                job.checkpoint = Some(Checkpoint {
                    completed_batches: batch_no,
                    last_pk: Some(end_for_checkpoint.clone()),
                    written_rows: success_rows,
                    verified: true,
                    verify: Some(verify_for_checkpoint.clone()),
                    engine: ENGINE.to_string(),
                    pk_kind: Some(kind),
                    target_fingerprint: Some(target_fingerprint.clone()),
                    updated_at: now(),
                });
                job.phase = MigrationPhase::Backfilling;
                job.progress.success_rows = success_rows;
                job.progress.processed_rows = success_rows;
                job.progress.remaining_rows = job.expected_rows.saturating_sub(success_rows);
                job.progress.current_batch = batch_no;
                job.progress.last_pk = Some(end_for_checkpoint.clone());
                job.progress.connection_id = None;
                MigrationStore::push_batch_record(
                    job,
                    BatchRecord {
                        batch_no,
                        last_pk: last_for_record,
                        end_pk: end_for_checkpoint,
                        source_rows,
                        written_rows,
                        verified: true,
                        verify: verify_for_checkpoint,
                        duration_ms,
                        at: now(),
                    },
                );
            })
            .await?;
        } else {
            hub.update(job_id, |job| {
                job.progress.success_rows = success_rows;
                job.progress.processed_rows = success_rows;
                job.progress.remaining_rows = job.expected_rows.saturating_sub(success_rows);
                job.progress.current_batch = batch_no;
                job.progress.last_pk = Some(end_pk.clone());
                job.progress.connection_id = None;
            })
            .await?;
        }

        *batch_index += 1;
        *last_pk = Some(end_pk);
    }

    Ok(success_rows)
}

// ── 单批执行 ───────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn execute_batch_in_tx(
    ctx: &OpsContext,
    hub: &MigrationHub,
    job_id: &str,
    spec: &BackfillSpec,
    source: &str,
    destination: &str,
    last_pk: Option<&str>,
    end_pk: &str,
    rows: u64,
    batch_no: u64,
    committed_rows: u64,
    policy: &OpsPolicy,
) -> Result<(), OpsError> {
    let pool = ctx.db_client.pool.pg()?;
    let mut conn = pool.acquire().await?;
    set_connection_id(hub, job_id, &mut conn).await?;
    let result = execute_in_transaction(
        &mut conn,
        spec,
        source,
        destination,
        last_pk,
        end_pk,
        rows,
        policy,
    )
    .await;
    hub.update(job_id, |job| job.progress.connection_id = None)
        .await?;
    match result {
        Ok(()) => Ok(()),
        Err(err) => Err(OpsError::Validation(format!(
            "批次 {batch_no} 失败（rollback_scope=batch）：范围 ({}, {}]，已提交 {committed_rows} 行；{err}",
            last_pk.unwrap_or("<start>"),
            end_pk
        ))),
    }
}

async fn execute_in_transaction(
    conn: &mut PgConnection,
    spec: &BackfillSpec,
    source: &str,
    destination: &str,
    last_pk: Option<&str>,
    end_pk: &str,
    rows: u64,
    policy: &OpsPolicy,
) -> Result<(), OpsError> {
    let mut tx = conn.begin().await?;
    for statement in pg_timeout_statements(policy) {
        sqlx::query(&statement).execute(&mut *tx).await?;
    }
    match run_insert(&mut tx, spec, source, destination, last_pk, end_pk, rows).await {
        Ok(()) => {
            tx.commit().await?;
            Ok(())
        }
        Err(err) => {
            let _ = tx.rollback().await;
            Err(err)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute_batch_on_conn(
    hub: &MigrationHub,
    job_id: &str,
    spec: &BackfillSpec,
    source: &str,
    destination: &str,
    last_pk: Option<&str>,
    end_pk: &str,
    rows: u64,
    batch_no: u64,
    committed_rows: u64,
    conn: &mut PgConnection,
) -> Result<(), OpsError> {
    set_connection_id(hub, job_id, conn).await?;
    run_insert(conn, spec, source, destination, last_pk, end_pk, rows)
        .await
        .map_err(|err| {
            OpsError::Validation(format!(
                "批次 {batch_no} 失败（rollback_scope=job）：范围 ({}, {}]，本事务回滚，之前 {committed_rows} 行未提交；{err}",
                last_pk.unwrap_or("<start>"),
                end_pk
            ))
        })
}

async fn set_connection_id(
    hub: &MigrationHub,
    job_id: &str,
    conn: &mut PgConnection,
) -> Result<(), OpsError> {
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *conn)
        .await?;
    hub.update(job_id, |job| job.progress.connection_id = Some(pid as u64))
        .await?;
    Ok(())
}

async fn run_insert(
    conn: &mut PgConnection,
    spec: &BackfillSpec,
    source: &str,
    destination: &str,
    last_pk: Option<&str>,
    end_pk: &str,
    rows: u64,
) -> Result<(), OpsError> {
    let kind = spec
        .pk_kind
        .ok_or_else(|| OpsError::Validation("缺少主键类型，无法回填".into()))?;
    let (sql, expected) = batch_sql_parts(spec, source, destination, last_pk.is_some())?;
    let mut binds = Vec::new();
    if let Some(value) = last_pk {
        binds.push(bind_value(kind, value)?);
    }
    binds.push(bind_value(kind, end_pk)?);
    binds.push(PgBind::Int(rows.max(1) as i64));
    if binds.len() != expected {
        return Err(OpsError::Validation("回填绑定参数数量不一致".into()));
    }
    pg_verify::bind_query(&sql, &binds)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// 供测试断言 SQL 结构；绑定数量与 `last_pk` 是否存在一致。
pub fn build_batch_sql(
    spec: &BackfillSpec,
    source: &str,
    destination: &str,
    has_last: bool,
) -> Result<String, OpsError> {
    batch_sql_parts(spec, source, destination, has_last).map(|(sql, _)| sql)
}

fn batch_sql_parts(
    spec: &BackfillSpec,
    source: &str,
    destination: &str,
    has_last: bool,
) -> Result<(String, usize), OpsError> {
    let kind = spec
        .pk_kind
        .ok_or_else(|| OpsError::Validation("缺少主键类型，无法生成回填语句".into()))?;
    if spec.target_columns.is_empty() {
        return Err(OpsError::Validation("缺少目标列，无法生成回填语句".into()));
    }
    let pk = quote_ident_pg(&spec.pk_column);
    let profile = ColumnProfile {
        data_type: spec.source_data_type.clone(),
        column_type: spec.source_data_type.clone(),
        ..Default::default()
    };
    let source_column = quote_ident_pg(&spec.source_column);
    let mut columns = vec![pk.clone()];
    let mut values = vec![pk.clone()];
    for column in &spec.target_columns {
        columns.push(quote_ident_pg(&column.name));
        values.push(value_expr(
            &source_column,
            &profile,
            column,
            spec.empty_string_policy,
        ));
    }
    let updates = spec
        .target_columns
        .iter()
        .map(|column| {
            let name = quote_ident_pg(&column.name);
            format!("{name} = EXCLUDED.{name}")
        })
        .collect::<Vec<_>>()
        .join(", ");

    let mut sql = format!(
        "INSERT INTO {destination} ({}) SELECT {} FROM {source} WHERE TRUE",
        columns.join(", "),
        values.join(", ")
    );
    let mut bind_count = 0usize;
    if has_last {
        bind_count += 1;
        sql.push_str(&format!(" AND {pk} > {}", cast_clause(kind, bind_count)));
    }
    bind_count += 1;
    sql.push_str(&format!(" AND {pk} <= {}", cast_clause(kind, bind_count)));
    sql.push_str(&format!(" ORDER BY {}", order_expr(&pk, kind)));
    bind_count += 1;
    sql.push_str(&format!(" LIMIT ${bind_count}"));
    sql.push_str(&format!(" ON CONFLICT ({pk}) DO UPDATE SET {updates}"));
    Ok((sql, bind_count))
}

// ── 读取 ───────────────────────────────────────────────────

async fn count_rows(
    ctx: &OpsContext,
    spec: &BackfillSpec,
    table: &str,
    last_pk: Option<&str>,
    end_pk: Option<&str>,
) -> Result<u64, OpsError> {
    let kind = spec
        .pk_kind
        .ok_or_else(|| OpsError::Validation("缺少主键类型，无法计数".into()))?;
    let pk = quote_ident_pg(&spec.pk_column);
    let mut binds = Vec::new();
    let mut sql = format!("SELECT COUNT(*) AS count FROM {table} WHERE TRUE");
    push_range(&mut sql, &pk, kind, last_pk, end_pk, &mut binds)?;
    let pool = ctx.db_client.pool.pg()?;
    let rows = fetch_on_pool(pool, &sql, &binds).await?;
    Ok(pg_verify::count_of(&rows, "count"))
}

async fn fetch_pk_page(
    ctx: &OpsContext,
    spec: &BackfillSpec,
    source: &str,
    last_pk: Option<&str>,
    batch_size: u64,
) -> Result<Vec<String>, OpsError> {
    let kind = spec
        .pk_kind
        .ok_or_else(|| OpsError::Validation("缺少主键类型，无法分页".into()))?;
    let pk = quote_ident_pg(&spec.pk_column);
    let mut binds = Vec::new();
    let mut sql = format!("SELECT {pk} AS pk_value FROM {source} WHERE TRUE");
    push_range(&mut sql, &pk, kind, last_pk, None, &mut binds)?;
    sql.push_str(&format!(" ORDER BY {}", order_expr(&pk, kind)));
    binds.push(PgBind::Int(batch_size.max(1) as i64));
    sql.push_str(&format!(" LIMIT ${}", binds.len()));
    let pool = ctx.db_client.pool.pg()?;
    let rows = fetch_on_pool(pool, &sql, &binds).await?;
    Ok(rows
        .iter()
        .filter_map(|row| row.get("pk_value").cloned())
        .collect())
}

// ── 差异定位 ───────────────────────────────────────────────

async fn locate_first_failed_batch(
    ctx: &OpsContext,
    spec: &BackfillSpec,
    batch_size: u64,
) -> Result<
    Option<(
        u64,
        Option<String>,
        Option<String>,
        BatchVerify,
        Vec<DiffDetail>,
    )>,
    OpsError,
> {
    let mut last_pk: Option<String> = None;
    let mut batch_no = 0u64;
    loop {
        let pks = fetch_pk_page_with_limit(ctx, spec, last_pk.as_deref(), batch_size).await?;
        if pks.is_empty() {
            return Ok(None);
        }
        batch_no += 1;
        let end_pk = pks.last().cloned().unwrap_or_default();
        let verify = pg_verify::verify_range(ctx, spec, last_pk.as_deref(), Some(&end_pk)).await?;
        if !verify.passed() {
            let diffs =
                collect_range_diffs(ctx, spec, last_pk.as_deref(), &end_pk, &verify, false).await?;
            return Ok(Some((
                batch_no,
                last_pk.clone(),
                Some(end_pk),
                verify,
                diffs,
            )));
        }
        last_pk = Some(end_pk);
    }
}

async fn fetch_pk_page_with_limit(
    ctx: &OpsContext,
    spec: &BackfillSpec,
    last_pk: Option<&str>,
    limit: u64,
) -> Result<Vec<String>, OpsError> {
    let kind = spec
        .pk_kind
        .ok_or_else(|| OpsError::Validation("缺少主键类型，无法分页".into()))?;
    let pk = quote_ident_pg(&spec.pk_column);
    let source = pg_verify::qualified(spec.schema.as_deref(), &spec.source_table);
    let mut binds = Vec::new();
    let mut sql = format!("SELECT {pk} AS pk_value FROM {source} WHERE TRUE");
    push_range(&mut sql, &pk, kind, last_pk, None, &mut binds)?;
    sql.push_str(&format!(" ORDER BY {}", order_expr(&pk, kind)));
    binds.push(PgBind::Int(limit as i64));
    sql.push_str(&format!(" LIMIT ${}", binds.len()));
    let pool = ctx.db_client.pool.pg()?;
    let rows = fetch_on_pool(pool, &sql, &binds).await?;
    Ok(rows
        .iter()
        .filter_map(|row| row.get("pk_value").cloned())
        .collect())
}

async fn collect_range_diffs(
    ctx: &OpsContext,
    spec: &BackfillSpec,
    last_pk: Option<&str>,
    end_pk: &str,
    verify: &BatchVerify,
    precheck: bool,
) -> Result<Vec<DiffDetail>, OpsError> {
    let mut diffs = Vec::new();
    if verify.source_rows != verify.target_rows {
        diffs.push(DiffDetail {
            phase: "v1".into(),
            total: verify.source_rows.abs_diff(verify.target_rows),
            pks: Vec::new(),
            has_more: false,
        });
    }
    for phase in ["v2", "v3", "v4"] {
        let count = match phase {
            "v2" => verify.v2,
            "v3" => verify.v3,
            _ => verify.v4,
        };
        if precheck && phase == "v2" {
            continue;
        }
        if count > 0 {
            let pks = pg_verify::diff_page(ctx, spec, phase, last_pk, Some(end_pk), DIFF_PAGE_SIZE)
                .await?;
            diffs.push(DiffDetail {
                phase: phase.into(),
                total: count,
                has_more: count > DIFF_PAGE_SIZE as u64,
                pks,
            });
        }
    }
    Ok(diffs)
}

async fn collect_full_diffs(
    ctx: &OpsContext,
    spec: &BackfillSpec,
    verify: &BatchVerify,
) -> Result<Vec<DiffDetail>, OpsError> {
    let mut diffs = Vec::new();
    if verify.source_rows != verify.target_rows {
        diffs.push(DiffDetail {
            phase: "v1".into(),
            total: verify.source_rows.abs_diff(verify.target_rows),
            pks: Vec::new(),
            has_more: false,
        });
    }
    for phase in ["v2", "v3", "v4"] {
        let count = match phase {
            "v2" => verify.v2,
            "v3" => verify.v3,
            _ => verify.v4,
        };
        if count > 0 {
            let pks = pg_verify::diff_page(ctx, spec, phase, None, None, DIFF_PAGE_SIZE).await?;
            diffs.push(DiffDetail {
                phase: phase.into(),
                total: count,
                has_more: count > DIFF_PAGE_SIZE as u64,
                pks,
            });
        }
    }
    Ok(diffs)
}

// ── 快照/类型/索引查询 ─────────────────────────────────────

fn pk_kind_from_column_type(column_type: &str) -> Result<PkKind, OpsError> {
    let lower = column_type.trim().to_ascii_lowercase();
    if lower.starts_with("bigint") || lower.starts_with("integer") || lower.starts_with("smallint")
    {
        Ok(PkKind::Integer)
    } else if lower.starts_with("numeric") || lower.starts_with("decimal") {
        Ok(PkKind::Numeric)
    } else if lower.starts_with("uuid") {
        Ok(PkKind::Uuid)
    } else if lower.starts_with("character varying")
        || lower.starts_with("character(")
        || lower == "text"
        || lower == "bpchar"
        || lower == "name"
    {
        Ok(PkKind::Text)
    } else {
        Err(OpsError::Validation(format!(
            "主键类型 {column_type} 不在支持范围（整型/numeric/text/uuid）"
        )))
    }
}

async fn pk_collation_deterministic(
    ctx: &OpsContext,
    schema: &str,
    table: &str,
    column: &str,
) -> Result<bool, OpsError> {
    let pool = ctx.db_client.pool.pg()?;
    let rows = sqlx::query(
        "SELECT co.collisdeterministic::text AS deterministic \
         FROM pg_attribute a \
         JOIN pg_class c ON c.oid = a.attrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         JOIN pg_collation co ON co.oid = a.attcollation \
         WHERE n.nspname = $1 AND c.relname = $2 AND a.attname = $3",
    )
    .bind(schema)
    .bind(table)
    .bind(column)
    .fetch_all(pool)
    .await?;
    let rows = convert_pg_rows(&rows);
    Ok(rows
        .first()
        .and_then(|row| row.get("deterministic"))
        .map(|value| value != "false" && value != "f")
        .unwrap_or(true))
}

/// 目标表必须存在单列主键或唯一索引，供 `ON CONFLICT (pk)` 使用。
pub async fn has_on_conflict_target(
    ctx: &OpsContext,
    schema: &str,
    table: &str,
    column: &str,
) -> Result<bool, OpsError> {
    let qualified = pg_verify::qualified(Some(schema), table);
    let pool = ctx.db_client.pool.pg()?;
    let rows = sqlx::query(
        "SELECT COUNT(*) AS count \
         FROM pg_index i \
         JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = i.indkey[0] \
         WHERE i.indrelid = to_regclass($1) \
           AND i.indisunique AND i.indnkeyatts = 1 \
           AND i.indpred IS NULL AND i.indexprs IS NULL \
           AND a.attname = $2",
    )
    .bind(&qualified)
    .bind(column)
    .fetch_all(pool)
    .await?;
    let rows = convert_pg_rows(&rows);
    Ok(pg_verify::count_of(&rows, "count") > 0)
}

/// 事务内超时映射；S2 固定三条 `SET LOCAL`。
pub fn pg_timeout_statements(policy: &OpsPolicy) -> Vec<String> {
    vec![
        format!(
            "SET LOCAL lock_timeout = '{}s'",
            policy.lock_wait_timeout_secs.max(1)
        ),
        "SET LOCAL statement_timeout = '0'".to_string(),
        "SET LOCAL idle_in_transaction_session_timeout = '60s'".to_string(),
    ]
}

async fn load_snapshot(
    ctx: &OpsContext,
    schema: &str,
    table: &str,
) -> Result<TableSnapshot, OpsError> {
    let schema_lit = escape_sql_string(schema);
    let table_lit = escape_sql_string(table);
    let columns_sql = format!(
        "SELECT a.attname::text AS column_name, \
                format_type(a.atttypid, a.atttypmod)::text AS column_type, \
                CASE WHEN a.attnotnull THEN 'NO' ELSE 'YES' END AS is_nullable, \
                CASE WHEN EXISTS (SELECT 1 FROM pg_index i \
                                  WHERE i.indrelid = c.oid AND i.indisprimary \
                                    AND a.attnum = ANY (i.indkey::int2[])) \
                     THEN 'PRI' ELSE '' END AS column_key, \
                '' AS extra, pg_encoding_to_char(d.encoding) AS character_set, \
                COALESCE(co.collname, '') AS collation \
         FROM pg_attribute a \
         JOIN pg_class c ON c.oid = a.attrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         CROSS JOIN pg_database d \
         LEFT JOIN pg_collation co ON co.oid = a.attcollation \
         WHERE n.nspname = '{schema_lit}' AND c.relname = '{table_lit}' \
           AND a.attnum > 0 AND NOT a.attisdropped \
         ORDER BY a.attnum"
    );
    let rows = fetch_rows(ctx, &columns_sql).await?;
    let columns = rows
        .iter()
        .map(|row| ColumnSnapshot {
            name: row.get("column_name").cloned().unwrap_or_default(),
            column_type: row.get("column_type").cloned().unwrap_or_default(),
            nullable: row
                .get("is_nullable")
                .map(|value| value.eq_ignore_ascii_case("YES"))
                .unwrap_or(true),
            column_key: row
                .get("column_key")
                .filter(|value| !value.is_empty())
                .cloned(),
            extra: row.get("extra").filter(|value| !value.is_empty()).cloned(),
            character_set: row.get("character_set").cloned(),
            collation: row
                .get("collation")
                .filter(|value| !value.is_empty())
                .cloned(),
        })
        .filter(|column| !column.name.is_empty())
        .collect();

    let indexes_sql = format!(
        "SELECT i.relname::text AS index_name, ix.indisunique::text AS is_unique, \
                k.ord::text AS seq_in_index, \
                pg_get_indexdef(ix.indexrelid, k.ord::int, true)::text AS column_name, \
                am.amname::text AS index_type \
         FROM pg_class c \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         JOIN pg_index ix ON ix.indrelid = c.oid \
         JOIN pg_class i ON i.oid = ix.indexrelid \
         JOIN pg_am am ON am.oid = i.relam \
         JOIN generate_series(1, ix.indnkeyatts) AS k(ord) ON true \
         WHERE n.nspname = '{schema_lit}' AND c.relname = '{table_lit}' \
         ORDER BY i.relname, k.ord"
    );
    let index_rows = fetch_rows(ctx, &indexes_sql).await?;
    let mut grouped: HashMap<String, IndexDefinition> = HashMap::new();
    for row in &index_rows {
        let Some(name) = row.get("index_name").cloned() else {
            continue;
        };
        let entry = grouped
            .entry(name.clone())
            .or_insert_with(|| IndexDefinition {
                name: name.clone(),
                unique: false,
                index_type: row
                    .get("index_type")
                    .cloned()
                    .unwrap_or_else(|| "btree".into())
                    .to_ascii_uppercase(),
                columns: Vec::new(),
            });
        entry.unique = matches!(
            row.get("is_unique").map(String::as_str),
            Some("true") | Some("t")
        );
        entry.columns.push(IndexColumn {
            name: row.get("column_name").cloned().unwrap_or_default(),
            seq: row
                .get("seq_in_index")
                .and_then(|value| value.parse::<i64>().ok())
                .unwrap_or(0),
            cardinality: None,
        });
    }
    let mut indexes: Vec<IndexDefinition> = grouped.into_values().collect();
    indexes.sort_by(|a, b| a.name.cmp(&b.name));
    for index in &mut indexes {
        index.columns.sort_by_key(|column| column.seq);
    }

    Ok(TableSnapshot {
        db_name: ctx.db_name.clone(),
        table_name: table.to_string(),
        columns,
        indexes,
        create_table_sql: None,
    })
}
