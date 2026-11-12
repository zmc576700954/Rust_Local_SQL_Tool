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
    self, bind_value, cast_clause, fetch_on_pool, PgBind, VALIDATION_MODE_SNAPSHOT,
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
    // 复合主键（S5）：`pk_columns` 非空时逐列确定类型；单列沿用 `pk_column`。
    let pk_columns = pg_verify::pk_columns_of(&spec);
    let mut pk_kinds = Vec::with_capacity(pk_columns.len());
    for column in &pk_columns {
        let Some(snapshot_column) = source_snapshot.find_column(column) else {
            return block_message(
                ctx,
                hub,
                job_id,
                &format!("源表缺少主键列 {column}"),
                "preflight",
            )
            .await;
        };
        match pk_kind_from_column_type(&snapshot_column.column_type) {
            Ok(kind) => pk_kinds.push(kind),
            Err(err) => {
                return block_message(ctx, hub, job_id, &err.to_string(), "preflight:pk_type").await
            }
        }
    }
    let pk_kind = pk_kinds
        .first()
        .copied()
        .ok_or_else(|| OpsError::Validation("缺少主键列，无法回填".into()))?;
    if let Some(checkpoint) = job.checkpoint.as_ref() {
        let previous = if checkpoint.pk_kinds.is_empty() {
            checkpoint.pk_kind.map(|kind| vec![kind])
        } else {
            Some(checkpoint.pk_kinds.clone())
        };
        if let Some(previous) = previous {
            if previous != pk_kinds {
                return block_message(
                    ctx,
                    hub,
                    job_id,
                    &format!("checkpoint 主键类型为 {previous:?}，当前为 {pk_kinds:?}，拒绝续跑"),
                    "preflight:pk_kind_mismatch",
                )
                .await;
            }
        }
    }
    job = hub
        .update(job_id, |job| {
            job.spec.pk_kind = Some(pk_kind);
            if pk_kinds.len() > 1 {
                job.spec.pk_kinds = pk_kinds.clone();
            }
        })
        .await?;
    spec.pk_kind = Some(pk_kind);
    if pk_kinds.len() > 1 {
        spec.pk_kinds = pk_kinds.clone();
    }
    for (column, kind) in pk_columns.iter().zip(&pk_kinds) {
        if *kind == PkKind::Text
            && !pk_collation_deterministic(ctx, &schema, &spec.source_table, column).await?
        {
            return block_message(
                ctx,
                hub,
                job_id,
                &format!("主键列 {column} 使用非确定性排序规则，禁止按 COLLATE \"C\" 分页"),
                "preflight:pk_collation",
            )
            .await;
        }
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

    // 分区表（S5）：源分区表经父表 keyset 分页透明；目标分区表 PG 全版本不支持
    // `ON CONFLICT DO UPDATE`，维持阻断并给手工路径。
    let target_relkind = relation_kind(ctx, &schema, &spec.target_table).await?;
    if target_relkind.as_deref() == Some("p") {
        return block_message(
            ctx,
            hub,
            job_id,
            &format!(
                "目标表 {} 是分区表：PostgreSQL 不支持分区表 ON CONFLICT DO UPDATE，\
                 无法复用分批回填；请按分区逐个子表执行 INSERT ... SELECT ... ON CONFLICT，\
                 或改用 S1 脚本",
                spec.target_table
            ),
            "preflight:partitioned_target",
        )
        .await;
    }

    // ON CONFLICT 目标索引预检（S5：接受部分唯一索引，表达式索引仍不支持）。
    let Some(conflict_target) =
        on_conflict_target(ctx, &schema, &spec.target_table, &pk_columns).await?
    else {
        return block_message(
            ctx,
            hub,
            job_id,
            &format!(
                "目标表 {} 没有可用于 ON CONFLICT ({}) 的唯一索引（表达式唯一索引不支持）",
                spec.target_table,
                pk_columns.join(", ")
            ),
            "preflight:on_conflict_target",
        )
        .await;
    };
    if let Some(predicate) = conflict_target.predicate.clone() {
        // 部分唯一索引的残余风险写审计（MigrationJob 没有 notes 字段）。
        let _ = append_audit(
            ctx,
            "field_migration.backfill.on_conflict_partial",
            "warn",
            &job,
            json!({
                "job_id": job_id,
                "target_table": spec.target_table,
                "predicate": predicate,
                "note": "ON CONFLICT 使用部分唯一索引：谓词外的行不参与冲突处理，重复主键由批后 V1/V2 校验拦截",
            }),
        );
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
    // 复合主键优先读 `last_pks`；旧 checkpoint（单列）回退到 `last_pk`。
    let checkpoint_key: Option<Vec<String>> = job.checkpoint.as_ref().and_then(|checkpoint| {
        checkpoint
            .last_pks
            .clone()
            .or_else(|| checkpoint.last_pk.clone().map(|value| vec![value]))
    });
    if completed_batches > 0 && checkpoint_key.is_none() {
        return block_message(
            ctx,
            hub,
            job_id,
            "checkpoint 缺少边界主键，拒绝从头静默重放",
            "preflight:checkpoint_invalid",
        )
        .await;
    }
    if let Some(key) = checkpoint_key.as_ref() {
        if key.len() != pk_columns.len() {
            return block_message(
                ctx,
                hub,
                job_id,
                &format!(
                    "checkpoint 边界主键有 {} 列，任务主键有 {} 列，拒绝续跑",
                    key.len(),
                    pk_columns.len()
                ),
                "preflight:checkpoint_invalid",
            )
            .await;
        }
    }

    // 续跑只读抽检最后一批。
    if completed_batches > 0 {
        if let Some(record) = job
            .recent_batches
            .iter()
            .find(|record| record.batch_no == completed_batches)
            .cloned()
        {
            let record_last: Option<Vec<String>> = record
                .last_pks
                .clone()
                .or_else(|| record.last_pk.clone().map(|value| vec![value]));
            let record_end: Vec<String> = record
                .end_pks
                .clone()
                .unwrap_or_else(|| vec![record.end_pk.clone()]);
            let spot =
                pg_verify::verify_range(ctx, &spec, record_last.as_deref(), Some(&record_end))
                    .await?;
            if !spot.passed() {
                let diffs = collect_range_diffs(
                    ctx,
                    &spec,
                    record_last.as_deref(),
                    Some(&record_end),
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
    let mut last_key: Option<Vec<String>> = checkpoint_key;
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
            &pk_columns,
            &conflict_target,
            &source,
            &destination,
            policy,
            batch_size,
            success_rows,
            &mut batch_index,
            &mut last_key,
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
                let checkpoint_key = last_key.clone();
                let checkpoint_display = checkpoint_key
                    .as_ref()
                    .map(|values| pg_verify::format_key(&pk_columns, values));
                job = hub
                    .update(job_id, |job| {
                        let checkpoint = job
                            .checkpoint
                            .get_or_insert_with(|| empty_checkpoint(&target_hash, pk_kind));
                        checkpoint.completed_batches = batch_index;
                        checkpoint.last_pk = checkpoint_display.clone();
                        checkpoint.last_pks = if pk_columns.len() > 1 {
                            checkpoint_key.clone()
                        } else {
                            None
                        };
                        checkpoint.written_rows = final_rows;
                        checkpoint.engine = ENGINE.to_string();
                        checkpoint.pk_kind = Some(pk_kind);
                        if pk_columns.len() > 1 {
                            checkpoint.pk_kinds = pk_kinds.clone();
                        }
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
            &pk_columns,
            &conflict_target,
            &source,
            &destination,
            policy,
            batch_size,
            success_rows,
            &mut batch_index,
            &mut last_key,
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
    let last_display = last_key
        .as_ref()
        .map(|values| pg_verify::format_key(&pk_columns, values));
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
                    last_display.clone(),
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
            if pk_columns.len() > 1 {
                checkpoint.pk_kinds = pk_kinds.clone();
                if checkpoint.last_pks.is_none() {
                    checkpoint.last_pks = last_key.clone();
                }
            }
            checkpoint.target_fingerprint = Some(target_hash.clone());
            if checkpoint.last_pk.is_none() {
                checkpoint.last_pk = last_display.clone();
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
        last_pks: None,
        written_rows: 0,
        verified: false,
        verify: None,
        engine: ENGINE.to_string(),
        pk_kind: Some(pk_kind),
        pk_kinds: Vec::new(),
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
    pk_columns: &[String],
    conflict: &ConflictTarget,
    source: &str,
    destination: &str,
    policy: &OpsPolicy,
    batch_size: u64,
    mut success_rows: u64,
    batch_index: &mut u64,
    last_key: &mut Option<Vec<String>>,
    mut single_conn: Option<&mut PoolConnection<sqlx::Postgres>>,
) -> Result<u64, OpsError> {
    let kinds = pg_verify::pk_kinds_of(spec)?;
    let kind = kinds
        .first()
        .copied()
        .ok_or_else(|| OpsError::Validation("缺少主键类型，无法回填".into()))?;
    let composite = pk_columns.len() > 1;
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

        let keys = fetch_pk_page(ctx, spec, source, last_key.as_deref(), batch_size).await?;
        if keys.is_empty() {
            break;
        }
        let batch_no = *batch_index + 1;
        let first_key = keys.first().cloned().unwrap_or_default();
        let end_key = keys.last().cloned().unwrap_or_default();
        let first_display = pg_verify::format_key(pk_columns, &first_key);
        let end_display = pg_verify::format_key(pk_columns, &end_key);
        let last_display = last_key
            .as_ref()
            .map(|values| pg_verify::format_key(pk_columns, values));
        let source_rows = keys.len() as u64;
        let started = Instant::now();

        hub.update(job_id, |job| {
            job.progress.current_batch = batch_no;
            job.progress.current_first_pk = Some(first_display.clone());
            job.progress.last_pk = last_display.clone();
        })
        .await?;

        let pre = if let Some(conn) = single_conn.as_deref_mut() {
            pg_verify::run_verify(conn, spec, last_key.as_deref(), Some(&end_key)).await?
        } else {
            pg_verify::verify_range(ctx, spec, last_key.as_deref(), Some(&end_key)).await?
        };
        if pre.v3 > 0 || pre.v4 > 0 {
            let diffs =
                collect_range_diffs(ctx, spec, last_key.as_deref(), Some(&end_key), &pre, true)
                    .await?;
            block_on_verify(
                ctx,
                hub,
                job_id,
                batch_no,
                last_display.clone(),
                Some(end_display.clone()),
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
                    conflict,
                    source,
                    destination,
                    last_key.as_deref(),
                    &end_key,
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
                    conflict,
                    source,
                    destination,
                    last_key.as_deref(),
                    &end_key,
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
            pg_verify::run_verify(conn, spec, last_key.as_deref(), Some(&end_key)).await?
        } else {
            pg_verify::verify_range(ctx, spec, last_key.as_deref(), Some(&end_key)).await?
        };
        if !verify.passed() {
            let diffs = collect_range_diffs(
                ctx,
                spec,
                last_key.as_deref(),
                Some(&end_key),
                &verify,
                false,
            )
            .await?;
            block_on_verify(
                ctx,
                hub,
                job_id,
                batch_no,
                last_display.clone(),
                Some(end_display.clone()),
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
                "range": { "last_pk": last_display, "end_pk": end_display },
                "written_rows": written_rows,
                "source_rows": source_rows,
                "verify": &verify,
                "verified": true,
                "duration_ms": duration_ms,
            }),
        )?;

        // 单事务模式在 COMMIT 前不推进 checkpoint；失败由整体回滚兜底。
        if !single {
            let end_for_checkpoint = end_display.clone();
            let end_keys_for_checkpoint = composite.then(|| end_key.clone());
            let kinds_for_checkpoint = composite.then(|| kinds.clone());
            let verify_for_checkpoint = verify.clone();
            let last_for_record = last_display.clone();
            let last_keys_for_record = if composite { last_key.clone() } else { None };
            hub.update(job_id, |job| {
                job.checkpoint = Some(Checkpoint {
                    completed_batches: batch_no,
                    last_pk: Some(end_for_checkpoint.clone()),
                    last_pks: end_keys_for_checkpoint.clone(),
                    written_rows: success_rows,
                    verified: true,
                    verify: Some(verify_for_checkpoint.clone()),
                    engine: ENGINE.to_string(),
                    pk_kind: Some(kind),
                    pk_kinds: kinds_for_checkpoint.clone().unwrap_or_default(),
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
                        last_pks: last_keys_for_record,
                        end_pk: end_for_checkpoint,
                        end_pks: end_keys_for_checkpoint,
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
                job.progress.last_pk = Some(end_display.clone());
                job.progress.connection_id = None;
            })
            .await?;
        }

        *batch_index += 1;
        *last_key = Some(end_key);
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
    conflict: &ConflictTarget,
    source: &str,
    destination: &str,
    last_key: Option<&[String]>,
    end_key: &[String],
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
        conflict,
        source,
        destination,
        last_key,
        end_key,
        rows,
        policy,
    )
    .await;
    hub.update(job_id, |job| job.progress.connection_id = None)
        .await?;
    let columns = pg_verify::pk_columns_of(spec);
    let range_start = last_key
        .map(|values| pg_verify::format_key(&columns, values))
        .unwrap_or_else(|| "<start>".to_string());
    let range_end = pg_verify::format_key(&columns, end_key);
    match result {
        Ok(()) => Ok(()),
        Err(err) => Err(OpsError::Validation(format!(
            "批次 {batch_no} 失败（rollback_scope=batch）：范围 ({range_start}, {range_end}]，已提交 {committed_rows} 行；{err}"
        ))),
    }
}

async fn execute_in_transaction(
    conn: &mut PgConnection,
    spec: &BackfillSpec,
    conflict: &ConflictTarget,
    source: &str,
    destination: &str,
    last_key: Option<&[String]>,
    end_key: &[String],
    rows: u64,
    policy: &OpsPolicy,
) -> Result<(), OpsError> {
    let mut tx = conn.begin().await?;
    for statement in pg_timeout_statements(policy) {
        sqlx::query(&statement).execute(&mut *tx).await?;
    }
    match run_insert(
        &mut tx,
        spec,
        conflict,
        source,
        destination,
        last_key,
        end_key,
        rows,
    )
    .await
    {
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
    conflict: &ConflictTarget,
    source: &str,
    destination: &str,
    last_key: Option<&[String]>,
    end_key: &[String],
    rows: u64,
    batch_no: u64,
    committed_rows: u64,
    conn: &mut PgConnection,
) -> Result<(), OpsError> {
    set_connection_id(hub, job_id, conn).await?;
    run_insert(
        conn, spec, conflict, source, destination, last_key, end_key, rows,
    )
    .await
    .map_err(|err| {
        let columns = pg_verify::pk_columns_of(spec);
        let range_start = last_key
            .map(|values| pg_verify::format_key(&columns, values))
            .unwrap_or_else(|| "<start>".to_string());
        let range_end = pg_verify::format_key(&columns, end_key);
        OpsError::Validation(format!(
            "批次 {batch_no} 失败（rollback_scope=job）：范围 ({range_start}, {range_end}]，本事务回滚，之前 {committed_rows} 行未提交；{err}"
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

#[allow(clippy::too_many_arguments)]
async fn run_insert(
    conn: &mut PgConnection,
    spec: &BackfillSpec,
    conflict: &ConflictTarget,
    source: &str,
    destination: &str,
    last_key: Option<&[String]>,
    end_key: &[String],
    rows: u64,
) -> Result<(), OpsError> {
    let kinds = pg_verify::pk_kinds_of(spec)?;
    let (sql, expected) = batch_sql_parts(spec, conflict, source, destination, last_key.is_some())?;
    let mut binds = Vec::new();
    if let Some(values) = last_key {
        for (kind, value) in kinds.iter().zip(values) {
            binds.push(bind_value(*kind, value)?);
        }
    }
    for (kind, value) in kinds.iter().zip(end_key) {
        binds.push(bind_value(*kind, value)?);
    }
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
    let conflict = ConflictTarget {
        columns: pg_verify::pk_columns_of(spec),
        predicate: None,
    };
    batch_sql_parts(spec, &conflict, source, destination, has_last).map(|(sql, _)| sql)
}

fn tuple_placeholders(kinds: &[PkKind], bind_count: &mut usize) -> String {
    kinds
        .iter()
        .map(|kind| {
            *bind_count += 1;
            cast_clause(*kind, *bind_count)
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn batch_sql_parts(
    spec: &BackfillSpec,
    conflict: &ConflictTarget,
    source: &str,
    destination: &str,
    has_last: bool,
) -> Result<(String, usize), OpsError> {
    let kinds = pg_verify::pk_kinds_of(spec)?;
    if spec.target_columns.is_empty() {
        return Err(OpsError::Validation("缺少目标列，无法生成回填语句".into()));
    }
    let pk_columns = pg_verify::pk_columns_of(spec);
    let pk_quoted = pk_columns
        .iter()
        .map(|column| quote_ident_pg(column))
        .collect::<Vec<_>>();
    let profile = ColumnProfile {
        data_type: spec.source_data_type.clone(),
        column_type: spec.source_data_type.clone(),
        ..Default::default()
    };
    let source_column = quote_ident_pg(&spec.source_column);
    let mut columns = pk_quoted.clone();
    let mut values = pk_quoted.clone();
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
    let single = pk_quoted.len() == 1;
    if has_last {
        if single {
            bind_count += 1;
            sql.push_str(&format!(
                " AND {} > {}",
                pk_quoted[0],
                cast_clause(kinds[0], bind_count)
            ));
        } else {
            let tuple = tuple_placeholders(&kinds, &mut bind_count);
            sql.push_str(&format!(
                " AND {} > ({tuple})",
                pg_verify::row_expr(&pk_quoted, &kinds)
            ));
        }
    }
    if single {
        bind_count += 1;
        sql.push_str(&format!(
            " AND {} <= {}",
            pk_quoted[0],
            cast_clause(kinds[0], bind_count)
        ));
    } else {
        let tuple = tuple_placeholders(&kinds, &mut bind_count);
        sql.push_str(&format!(
            " AND {} <= ({tuple})",
            pg_verify::row_expr(&pk_quoted, &kinds)
        ));
    }
    sql.push_str(&format!(
        " ORDER BY {}",
        pg_verify::order_key(&pk_quoted, &kinds)
    ));
    bind_count += 1;
    sql.push_str(&format!(" LIMIT ${bind_count}"));
    let conflict_columns = conflict
        .columns
        .iter()
        .map(|column| quote_ident_pg(column))
        .collect::<Vec<_>>()
        .join(", ");
    match conflict.predicate.as_deref() {
        Some(predicate) => sql.push_str(&format!(
            " ON CONFLICT ({conflict_columns}) WHERE {predicate} DO UPDATE SET {updates}"
        )),
        None => sql.push_str(&format!(
            " ON CONFLICT ({conflict_columns}) DO UPDATE SET {updates}"
        )),
    }
    Ok((sql, bind_count))
}

// ── 读取 ───────────────────────────────────────────────────

async fn count_rows(
    ctx: &OpsContext,
    spec: &BackfillSpec,
    table: &str,
    last_key: Option<&[String]>,
    end_key: Option<&[String]>,
) -> Result<u64, OpsError> {
    let columns = pg_verify::pk_columns_of(spec);
    let kinds = pg_verify::pk_kinds_of(spec)?;
    let refs: Vec<String> = columns
        .iter()
        .map(|column| quote_ident_pg(column))
        .collect();
    let mut binds = Vec::new();
    let mut sql = format!("SELECT COUNT(*) AS count FROM {table} WHERE TRUE");
    pg_verify::push_key_range(&mut sql, &refs, &kinds, last_key, end_key, &mut binds)?;
    let pool = ctx.db_client.pool.pg()?;
    let rows = fetch_on_pool(pool, &sql, &binds).await?;
    Ok(pg_verify::count_of(&rows, "count"))
}

/// keyset 分页取主键 tuple；返回按主键列顺序排列的值。
async fn fetch_key_page(
    ctx: &OpsContext,
    spec: &BackfillSpec,
    table: &str,
    last_key: Option<&[String]>,
    limit: u64,
) -> Result<Vec<Vec<String>>, OpsError> {
    let columns = pg_verify::pk_columns_of(spec);
    let kinds = pg_verify::pk_kinds_of(spec)?;
    let refs: Vec<String> = columns
        .iter()
        .map(|column| quote_ident_pg(column))
        .collect();
    let mut binds = Vec::new();
    let mut sql = format!("SELECT {} FROM {table} WHERE TRUE", refs.join(", "));
    pg_verify::push_key_range(&mut sql, &refs, &kinds, last_key, None, &mut binds)?;
    sql.push_str(&format!(
        " ORDER BY {}",
        pg_verify::order_key(&refs, &kinds)
    ));
    binds.push(PgBind::Int(limit.max(1) as i64));
    sql.push_str(&format!(" LIMIT ${}", binds.len()));
    let pool = ctx.db_client.pool.pg()?;
    let rows = fetch_on_pool(pool, &sql, &binds).await?;
    Ok(rows
        .iter()
        .map(|row| {
            columns
                .iter()
                .map(|column| row.get(column).cloned().unwrap_or_default())
                .collect()
        })
        .collect())
}

async fn fetch_pk_page(
    ctx: &OpsContext,
    spec: &BackfillSpec,
    source: &str,
    last_key: Option<&[String]>,
    batch_size: u64,
) -> Result<Vec<Vec<String>>, OpsError> {
    fetch_key_page(ctx, spec, source, last_key, batch_size).await
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
    let columns = pg_verify::pk_columns_of(spec);
    let mut last_key: Option<Vec<String>> = None;
    let mut batch_no = 0u64;
    loop {
        let keys = fetch_pk_page_with_limit(ctx, spec, last_key.as_deref(), batch_size).await?;
        if keys.is_empty() {
            return Ok(None);
        }
        batch_no += 1;
        let end_key = keys.last().cloned().unwrap_or_default();
        let verify =
            pg_verify::verify_range(ctx, spec, last_key.as_deref(), Some(&end_key)).await?;
        if !verify.passed() {
            let diffs = collect_range_diffs(
                ctx,
                spec,
                last_key.as_deref(),
                Some(&end_key),
                &verify,
                false,
            )
            .await?;
            let last_display = last_key
                .as_ref()
                .map(|values| pg_verify::format_key(&columns, values));
            return Ok(Some((
                batch_no,
                last_display,
                Some(pg_verify::format_key(&columns, &end_key)),
                verify,
                diffs,
            )));
        }
        last_key = Some(end_key);
    }
}

async fn fetch_pk_page_with_limit(
    ctx: &OpsContext,
    spec: &BackfillSpec,
    last_key: Option<&[String]>,
    limit: u64,
) -> Result<Vec<Vec<String>>, OpsError> {
    let source = pg_verify::qualified(spec.schema.as_deref(), &spec.source_table);
    fetch_key_page(ctx, spec, &source, last_key, limit).await
}

async fn collect_range_diffs(
    ctx: &OpsContext,
    spec: &BackfillSpec,
    last_key: Option<&[String]>,
    end_key: Option<&[String]>,
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
            let pks =
                pg_verify::diff_page(ctx, spec, phase, last_key, end_key, DIFF_PAGE_SIZE).await?;
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

pub(crate) fn pk_kind_from_column_type(column_type: &str) -> Result<PkKind, OpsError> {
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

pub(crate) async fn pk_collation_deterministic(
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

/// `ON CONFLICT` 推断目标：键列 + 可选部分索引谓词。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictTarget {
    pub columns: Vec<String>,
    pub predicate: Option<String>,
}

/// 目标表必须存在唯一索引供 `ON CONFLICT (cols)` 使用。
///
/// S5：键列集合等于主键列（顺序无关，PG 推断不要求顺序）；表达式索引不参与推断；
/// 非部分索引优先，只有部分索引时取其谓词作为推断条件。
pub async fn on_conflict_target(
    ctx: &OpsContext,
    schema: &str,
    table: &str,
    columns: &[String],
) -> Result<Option<ConflictTarget>, OpsError> {
    let qualified = pg_verify::qualified(Some(schema), table);
    let pool = ctx.db_client.pool.pg()?;
    let rows = sqlx::query(
        "SELECT ic.relname::text AS index_name, \
                (ix.indpred IS NULL)::text AS full_index, \
                pg_get_expr(ix.indpred, ix.indrelid)::text AS predicate, \
                a.attname::text AS column_name, \
                k.ord::text AS ord \
         FROM pg_index ix \
         JOIN pg_class ic ON ic.oid = ix.indexrelid \
         JOIN generate_series(0, ix.indnkeyatts - 1) AS k(ord) ON true \
         JOIN pg_attribute a ON a.attrelid = ix.indrelid AND a.attnum = ix.indkey[k.ord] \
         WHERE ix.indrelid = to_regclass($1) \
           AND ix.indisunique AND ix.indexprs IS NULL AND ix.indnkeyatts = $2 \
         ORDER BY ic.relname, k.ord",
    )
    .bind(&qualified)
    .bind(columns.len() as i16)
    .fetch_all(pool)
    .await?;
    let rows = convert_pg_rows(&rows);
    let mut candidates: Vec<(String, bool, Option<String>, Vec<String>)> = Vec::new();
    for row in &rows {
        let Some(name) = row.get("index_name").cloned() else {
            continue;
        };
        let full = matches!(
            row.get("full_index").map(String::as_str),
            Some("true") | Some("t")
        );
        let predicate = row
            .get("predicate")
            .filter(|value| !value.trim().is_empty())
            .cloned();
        let column = row.get("column_name").cloned().unwrap_or_default();
        match candidates.iter_mut().find(|(index, ..)| *index == name) {
            Some((_, _, _, cols)) => cols.push(column),
            None => candidates.push((name, full, predicate, vec![column])),
        }
    }
    let wanted: std::collections::BTreeSet<&str> = columns.iter().map(String::as_str).collect();
    let mut partial: Option<ConflictTarget> = None;
    for (_, full, predicate, cols) in candidates {
        let actual: std::collections::BTreeSet<&str> = cols.iter().map(String::as_str).collect();
        if actual != wanted {
            continue;
        }
        let target = ConflictTarget {
            columns: columns.to_vec(),
            predicate: if full { None } else { predicate },
        };
        if target.predicate.is_none() {
            return Ok(Some(target));
        }
        if partial.is_none() {
            partial = Some(target);
        }
    }
    Ok(partial)
}

/// 目标表 relkind（'r' 普通表 / 'p' 分区表 / 'v' 视图 ...）。
async fn relation_kind(
    ctx: &OpsContext,
    schema: &str,
    table: &str,
) -> Result<Option<String>, OpsError> {
    let rows = sqlx::query(
        "SELECT c.relkind::text AS relkind \
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = $1 AND c.relname = $2",
    )
    .bind(schema)
    .bind(table)
    .fetch_all(ctx.db_client.pool.pg()?)
    .await?;
    let rows = convert_pg_rows(&rows);
    Ok(rows.first().and_then(|row| row.get("relkind").cloned()))
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

/// DDL 单语句自动提交（不进事务块）；session 级锁超时、语句不限时，执行后 RESET。
/// S3 起为 notnull/index/typechange 三个执行器共用的唯一实现；
/// `exec_ddl_raw` 保留原始 sqlx 错误（供 typechange 读 SQLSTATE/DETAIL）。
pub async fn exec_ddl_raw(
    ctx: &OpsContext,
    policy: &OpsPolicy,
    sql: &str,
) -> Result<(), sqlx::Error> {
    let pool = ctx.db_client.pool.pg().map_err(|err| {
        sqlx::Error::Configuration(Box::new(std::io::Error::other(err.to_string())))
    })?;
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

/// DDL 单语句自动提交（不进事务块）；S4 起收敛到共享实现，行为不变。
pub async fn exec_ddl_auto_commit(
    ctx: &OpsContext,
    policy: &OpsPolicy,
    sql: &str,
) -> Result<(), OpsError> {
    exec_ddl_raw(ctx, policy, sql).await.map_err(OpsError::from)
}

pub(crate) async fn load_snapshot(
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
           AND d.datname = current_database() \
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::field_migration::migration_store::PkKind;
    use crate::ops::field_migration::schema::TargetColumn;
    use crate::ops::field_migration::types::EmptyStringPolicy;

    fn spec_for(pk_columns: &[&str], kinds: Vec<PkKind>, pk_kind: Option<PkKind>) -> BackfillSpec {
        let columns: Vec<String> = pk_columns.iter().map(|name| name.to_string()).collect();
        BackfillSpec {
            source_table: "src".into(),
            source_column: "payload".into(),
            target_table: "dst".into(),
            pk_column: columns.first().cloned().unwrap_or_else(|| "id".to_string()),
            // 约定：单列主键时 pk_columns 为空（存了才是复合）。
            pk_columns: if columns.len() > 1 {
                columns
            } else {
                Vec::new()
            },
            pk_kind,
            pk_kinds: kinds,
            target_columns: vec![TargetColumn {
                name: "theme".into(),
                source_key: Some("theme".into()),
                sql_type: "text".into(),
                nullable: true,
                required: false,
                note: None,
            }],
            empty_string_policy: EmptyStringPolicy::Preserve,
            create_table_sql: None,
            schema: Some("public".into()),
            source_data_type: "jsonb".into(),
        }
    }

    /// S2 形态必须逐字保持：单列主键不带行构造器、不带 COLLATE（整型）。
    #[test]
    fn single_pk_batch_sql_keeps_s2_shape() {
        let spec = spec_for(&["id"], Vec::new(), Some(PkKind::Integer));
        let sql =
            build_batch_sql(&spec, "public.src", "public.dst", false).expect("生成批 SQL 失败");
        assert!(sql.contains("AND \"id\" <= $1::bigint"), "{sql}");
        assert!(sql.contains("ORDER BY \"id\""), "{sql}");
        assert!(sql.contains("LIMIT $2"), "{sql}");
        assert!(sql.contains("ON CONFLICT (\"id\") DO UPDATE SET"), "{sql}");
        assert!(!sql.contains("(\"id\") >"), "单列不得使用行构造器: {sql}");
    }

    /// S5：复合主键改用行比较，绑定顺序为 [last.., end.., limit]。
    #[test]
    fn composite_pk_batch_sql_uses_row_comparison() {
        let spec = spec_for(
            &["tenant_id", "seq"],
            vec![PkKind::Integer, PkKind::Integer],
            Some(PkKind::Integer),
        );
        let sql =
            build_batch_sql(&spec, "public.src", "public.dst", true).expect("生成批 SQL 失败");
        assert!(
            sql.contains("AND (\"tenant_id\", \"seq\") > ($1::bigint, $2::bigint)"),
            "{sql}"
        );
        assert!(
            sql.contains("AND (\"tenant_id\", \"seq\") <= ($3::bigint, $4::bigint)"),
            "{sql}"
        );
        assert!(sql.contains("ORDER BY \"tenant_id\", \"seq\""), "{sql}");
        assert!(sql.contains("LIMIT $5"), "{sql}");
        assert!(
            sql.contains("ON CONFLICT (\"tenant_id\", \"seq\") DO UPDATE SET"),
            "{sql}"
        );
    }

    /// 文本列的复合键：行构造器内与 ORDER BY 都必须带 `COLLATE "C"`。
    #[test]
    fn composite_text_pk_collates_in_row_and_order() {
        let spec = spec_for(
            &["region", "seq"],
            vec![PkKind::Text, PkKind::Integer],
            Some(PkKind::Text),
        );
        let sql =
            build_batch_sql(&spec, "public.src", "public.dst", false).expect("生成批 SQL 失败");
        assert!(
            sql.contains(
                "(\"region\" COLLATE \"C\", \"seq\") <= (($1::text COLLATE \"C\"), $2::bigint)"
            ),
            "{sql}"
        );
        assert!(
            sql.contains("ORDER BY \"region\" COLLATE \"C\", \"seq\""),
            "{sql}"
        );
    }
}
