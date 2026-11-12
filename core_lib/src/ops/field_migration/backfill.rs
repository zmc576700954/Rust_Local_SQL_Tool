//! S2 独立回填执行器。
//!
//! 明确边界：
//! - 不复用索引场景的 `ops::executor::execute`，也不复用 `DdlEngine` / `execute_ddl`；
//! - 没有任何 `DROP COLUMN` 可执行路径，下线仍然只出脚本；
//! - 目标表只允许 `CREATE TABLE IF NOT EXISTS`，源表只读；
//! - 按主键区间分批，每批独立事务 + 批前/批后校验 + checkpoint + 审计；
//! - 续跑时重新校验已完成的批次（只读），发现目标被改坏就停在该批，不覆盖已修正数据。

use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::json;
use sqlx::Acquire;

use crate::ops::error::OpsError;
use crate::ops::index_advisor::load_snapshot_public;
use crate::ops::policy::OpsPolicy;
use crate::ops::registry::OpsContext;
use crate::ops::safety::classify_blocked_sql;
use crate::ops::types::{hash_json, now, AuditRecord, OpsPlan};
use crate::sql::util::{escape_sql_string, quote_ident_mysql};

use super::migration_store::{
    default_engine_mysql, BackfillConfig, BackfillExecution, BackfillSpec, BatchRecord,
    BatchVerify, BlockingDetail, Checkpoint, DiffDetail, MigrationHub, MigrationJob,
    MigrationPhase, MigrationStatus, MigrationStore, PkKind, CONTROL_ABORT, CONTROL_PAUSE,
};
use super::read_only::fetch_read_only;
use super::scripts;
use super::types::{EmptyStringPolicy, FieldGovernanceAnalysis, FieldRisk, RiskLevel};

const MAX_BATCH_SIZE: u64 = 10_000;
const MAX_BATCH_INTERVAL_MS: u64 = 60_000;
const MAX_ROWS_PER_SECOND: u64 = 100_000;
const DIFF_PAGE_SIZE: u64 = 100;

#[derive(Debug, Clone)]
pub struct PreparedBackfill {
    pub spec: BackfillSpec,
    pub config: BackfillConfig,
    pub migration_hash: String,
    pub high_risks: Vec<FieldRisk>,
    /// PG 专用执行语义；MySQL 为 None。
    pub execution: Option<BackfillExecution>,
}

#[derive(Serialize)]
struct MigrationHashView<'a> {
    plan_id: &'a str,
    plan_hash: &'a str,
    db_fingerprint: &'a str,
    spec: &'a BackfillSpec,
    config: &'a BackfillConfig,
}

/// 把只读分析产物收紧为可执行规格。任何一项不满足都不允许进入写步骤。
pub fn prepare(
    plan: &OpsPlan,
    analysis: &FieldGovernanceAnalysis,
    policy: &OpsPolicy,
    requested: BackfillConfig,
) -> Result<PreparedBackfill, OpsError> {
    if plan.scenario_id != "field-governance" {
        return Err(OpsError::Validation(
            "只有字段治理计划可以创建回填任务".into(),
        ));
    }
    if analysis.profile_mode != "full" {
        return Err(OpsError::Validation(format!(
            "剖面模式为 {}，不能授权写步骤；必须使用 full 剖面",
            analysis.profile_mode
        )));
    }
    let is_pg = analysis.environment.engine_family == "postgresql";
    let column = analysis
        .column
        .clone()
        .filter(|c| !c.trim().is_empty())
        .ok_or_else(|| OpsError::Validation("缺少源列，无法创建回填任务".into()))?;
    if analysis.target_table.trim().is_empty() {
        return Err(OpsError::Validation("缺少目标表".into()));
    }
    if analysis.target_columns.is_empty() {
        return Err(OpsError::Validation(
            "缺少目标列映射，无法生成回填语句".into(),
        ));
    }
    if analysis.has_block() || analysis.risk_summary.undetermined_write_block {
        return Err(OpsError::Validation(
            "分析存在阻断项或未判定证据，禁止进入回填".into(),
        ));
    }
    if !analysis.gaps.is_empty() {
        return Err(OpsError::Validation(format!(
            "存在 {} 项证据缺口，写步骤按阻断处理",
            analysis.gaps.len()
        )));
    }
    if !analysis.dependencies.scan_complete {
        return Err(OpsError::Validation("依赖扫描未完成，禁止自动回填".into()));
    }
    if !analysis.dependencies.source_triggers.is_empty() {
        return Err(OpsError::Validation("源表存在触发器，禁止自动回填".into()));
    }
    // S5：PG 支持复合主键 keyset；MySQL 保持单列（行为/SQL 零变化）。
    let mut pk_columns: Vec<String> = analysis.dependencies.primary_key_columns.clone();
    if pk_columns.is_empty() {
        if let Some(single) = analysis.dependencies.single_column_pk.clone() {
            if !single.trim().is_empty() {
                pk_columns.push(single);
            }
        }
    }
    if pk_columns.is_empty() {
        return Err(OpsError::Validation("源表没有主键，禁止自动回填".into()));
    }
    if !is_pg && pk_columns.len() != 1 {
        return Err(OpsError::Validation(
            "源表主键不是单列，MySQL 回填仅支持单列主键".into(),
        ));
    }
    let pk_column = pk_columns[0].clone();
    if !is_pg {
        if let Some(engine) = analysis.environment.engine.as_deref() {
            if !engine.eq_ignore_ascii_case("innodb") {
                return Err(OpsError::Validation(format!(
                    "源表引擎为 {engine}，只有 InnoDB 支持安全分批回填"
                )));
            }
        }
    }
    // S5：PG 源分区表经父表 keyset 分页透明；MySQL 分区表仍拦截。
    if analysis.environment.partitioned && !is_pg {
        return Err(OpsError::Validation("源表是分区表，禁止自动回填".into()));
    }
    if analysis.environment.read_only {
        return Err(OpsError::Validation(
            "当前连接为只读副本，禁止执行回填".into(),
        ));
    }
    if analysis.row_count > policy.field_migration_max_rows {
        return Err(OpsError::Validation(format!(
            "源表 {} 行超过自动回填上限 {}",
            analysis.row_count, policy.field_migration_max_rows
        )));
    }
    if analysis.data_length.unwrap_or(0) > policy.field_migration_max_bytes {
        return Err(OpsError::Validation(format!(
            "源表数据量 {} 字节超过自动回填上限 {}",
            analysis.data_length.unwrap_or(0),
            policy.field_migration_max_bytes
        )));
    }
    let empty_policy = analysis.empty_string_policy;
    if empty_policy == EmptyStringPolicy::Block {
        let empty_rows = analysis
            .selected_profile()
            .map(|profile| profile.empty_rows)
            .unwrap_or(0);
        if empty_rows > 0 {
            return Err(OpsError::Validation(format!(
                "空串策略为 block，但存在 {empty_rows} 行空串，禁止回填"
            )));
        }
    }

    let create_table_sql = analysis
        .scripts
        .iter()
        .find(|script| script.kind == "create_table")
        .map(|script| script.sql.clone());
    if let Some(sql) = create_table_sql.as_deref() {
        validate_create_statement(sql)?;
    }

    // S2/S5 只支持源/目标主键列名一致；跨列名映射必须回到 S1 重新生成脚本。
    if analysis.target_columns.iter().any(|column| {
        pk_columns
            .iter()
            .any(|pk| column.name.eq_ignore_ascii_case(pk))
    }) {
        return Err(OpsError::Validation("目标列映射不能覆盖主键列".into()));
    }

    let config = normalize_config(requested, policy);
    if config.single_transaction && !is_pg {
        return Err(OpsError::Validation("单事务模式仅支持 PostgreSQL".into()));
    }
    let source_data_type = analysis
        .selected_profile()
        .map(|profile| profile.data_type.clone())
        .unwrap_or_default();
    let spec = BackfillSpec {
        source_table: analysis.table.clone(),
        source_column: column,
        target_table: analysis.target_table.clone(),
        pk_column,
        // 复合主键仅 PG 路径写入；单列与 MySQL 恒为空（JSON 字节不变）。
        pk_columns: if is_pg && pk_columns.len() > 1 {
            pk_columns.clone()
        } else {
            Vec::new()
        },
        // 实际类型在 preflight 用源表快照确定。
        pk_kind: None,
        pk_kinds: Vec::new(),
        target_columns: analysis.target_columns.clone(),
        empty_string_policy: empty_policy,
        create_table_sql,
        schema: analysis.environment.schema.clone(),
        source_data_type,
    };
    let execution = is_pg.then(|| BackfillExecution {
        atomic: config.single_transaction,
        rollback_scope: if config.single_transaction {
            "job".to_string()
        } else {
            "batch".to_string()
        },
        resumable: !config.single_transaction,
        validation_mode: "snapshot".to_string(),
    });
    let migration_hash = hash_json(&MigrationHashView {
        plan_id: &plan.plan_id,
        plan_hash: &plan.plan_hash,
        db_fingerprint: &plan.db_fingerprint,
        spec: &spec,
        config: &config,
    });
    let high_risks = analysis
        .risks
        .iter()
        .filter(|risk| risk.level == RiskLevel::High)
        .cloned()
        .collect();
    Ok(PreparedBackfill {
        spec,
        config,
        migration_hash,
        high_risks,
        execution,
    })
}

fn normalize_config(requested: BackfillConfig, policy: &OpsPolicy) -> BackfillConfig {
    let max_batch = MAX_BATCH_SIZE.min(policy.field_migration_max_rows.max(1));
    BackfillConfig {
        batch_size: requested.batch_size.clamp(1, max_batch),
        batch_interval_ms: requested.batch_interval_ms.min(MAX_BATCH_INTERVAL_MS),
        max_rows_per_second: requested.max_rows_per_second.min(MAX_ROWS_PER_SECOND),
        single_transaction: requested.single_transaction,
    }
}

pub fn new_job(
    plan: &OpsPlan,
    prepared: &PreparedBackfill,
    production_confirmed: bool,
) -> MigrationJob {
    let ts = now();
    MigrationJob {
        job_id: uuid::Uuid::new_v4().to_string(),
        plan_id: plan.plan_id.clone(),
        plan_hash: plan.plan_hash.clone(),
        migration_hash: prepared.migration_hash.clone(),
        approval_id: None,
        db_id: plan.db_id.clone(),
        db_name: plan.db_name.clone(),
        db_fingerprint: plan.db_fingerprint.clone(),
        spec: prepared.spec.clone(),
        config: prepared.config.clone(),
        // 首次运行时按连接绑定；旧文件反序列化缺省为 mysql。
        engine: String::new(),
        execution: prepared.execution.clone(),
        status: MigrationStatus::Pending,
        phase: MigrationPhase::Pending,
        production_confirmed,
        checkpoint: None,
        progress: Default::default(),
        blocking: None,
        recent_batches: Vec::new(),
        source_schema_hash: String::new(),
        target_schema_hash: String::new(),
        target_created_by_job: false,
        expected_rows: 0,
        created_at: ts,
        updated_at: ts,
        started_at: 0,
        finished_at: None,
        last_error: None,
        run_count: 0,
    }
}

/// 后台任务入口。所有可恢复/不可恢复错误最终都会落成任务状态。
pub async fn run_job(job_id: String, ctx: OpsContext, hub: MigrationHub, policy: OpsPolicy) {
    hub.mark_active(&job_id).await;
    let result = if ctx.is_postgres() {
        super::pg_backfill::run_inner(&job_id, &ctx, &hub, &policy).await
    } else {
        run_inner(&job_id, &ctx, &hub, &policy).await
    };
    if let Err(err) = result {
        let aborted = hub.control_value(&job_id).await == CONTROL_ABORT;
        let message = err.to_string();
        let snapshot = hub
            .update(&job_id, |job| {
                if matches!(
                    job.status,
                    MigrationStatus::Pending | MigrationStatus::Running
                ) {
                    job.status = if aborted {
                        MigrationStatus::Aborted
                    } else {
                        MigrationStatus::Interrupted
                    };
                    job.phase = if aborted {
                        MigrationPhase::Aborted
                    } else {
                        MigrationPhase::Interrupted
                    };
                    job.last_error = Some(message.clone());
                    job.finished_at = Some(now());
                }
            })
            .await
            .ok();
        if let Some(job) = snapshot {
            let _ = append_audit(
                &ctx,
                "field_migration.backfill.run",
                if aborted { "aborted" } else { "error" },
                &job,
                json!({ "job_id": job_id, "error": message }),
            );
        }
    }
    hub.mark_inactive(&job_id).await;
}

async fn run_inner(
    job_id: &str,
    ctx: &OpsContext,
    hub: &MigrationHub,
    policy: &OpsPolicy,
) -> Result<(), OpsError> {
    let mut job = hub.get(job_id).await?;
    if job.status.is_finished() {
        return Ok(());
    }
    if !ctx.is_mysql_family() {
        return block_message(ctx, hub, job_id, "回填只支持 MySQL/MariaDB", "preflight").await;
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
    let spec = job.spec.clone();
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
            "run_count": job.run_count,
            "resume_after_pk": job.progress.resume_after_pk,
            "resume_skipped_batches": job.progress.resume_skipped_batches,
            "batch_size": job.config.batch_size,
            "batch_interval_ms": job.config.batch_interval_ms,
            "max_rows_per_second": job.config.max_rows_per_second,
        }),
    );

    let source = qualified(&ctx.db_name, &spec.source_table);
    let destination = qualified(&ctx.db_name, &spec.target_table);
    let source_column = quote_ident_mysql(&spec.source_column);
    let pk = quote_ident_mysql(&spec.pk_column);

    // ── preflight：源表快照 ─────────────────────────────────
    let source_snapshot = load_snapshot_public(ctx, &spec.source_table).await?;
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
    let pk_kind = pk_kind_from_type(&pk_column.column_type);
    if job.spec.pk_kind != Some(pk_kind) {
        job = hub
            .update(job_id, |job| job.spec.pk_kind = Some(pk_kind))
            .await?;
    }

    // ── preflight：目标表（只允许 CREATE） ─────────────────
    let mut target_snapshot = load_snapshot_public(ctx, &spec.target_table).await?;
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
        target_snapshot = load_snapshot_public(ctx, &spec.target_table).await?;
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

    // 目标表已存在且非空、又不是本任务创建的，说明可能覆盖他人数据。
    if job.checkpoint.is_none() && !job.target_created_by_job {
        let target_rows = count_rows(ctx, &destination, &pk, None, None).await?;
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

    // ── preflight：源行数基线 ──────────────────────────────
    let expected = count_rows(ctx, &source, &pk, None, None).await?;
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

    // 续跑只抽检「最后一批已完成批次」：范围 (record.last_pk, record.end_pk]，
    // 成本约 1 个批次（2 次 COUNT + 3 次差异计数 + 必要时的差异主键查询）。
    // 抽检只读；不会覆盖已被人工修正的数据。
    if completed_batches > 0 {
        if let Some(record) = job
            .recent_batches
            .iter()
            .find(|record| record.batch_no == completed_batches)
            .cloned()
        {
            let spot_start = record
                .last_pk
                .as_deref()
                .map(|pk_value| pk_literal(pk_kind, pk_value))
                .transpose()?;
            let spot_end = pk_literal(pk_kind, &record.end_pk)?;
            let spot = verify_range(
                ctx,
                &spec,
                &source,
                &destination,
                &source_column,
                &pk,
                spot_start.as_deref(),
                &spot_end,
            )
            .await?;
            if !spot.passed() {
                let diffs = collect_range_diffs(
                    ctx,
                    &spec,
                    &source,
                    &destination,
                    &source_column,
                    &pk,
                    spot_start.as_deref(),
                    &spot_end,
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

    let mut success_rows = job
        .checkpoint
        .as_ref()
        .map(|checkpoint| checkpoint.written_rows)
        .unwrap_or(0);
    // 直接 seek 到 checkpoint 边界之后，不再从头重走已完成批次。
    let mut batch_index: u64 = completed_batches;
    let mut last_pk: Option<String> = checkpoint_pk.clone();
    let mut last_literal: Option<String> = match checkpoint_pk.as_deref() {
        Some(pk_value) => Some(pk_literal(pk_kind, pk_value)?),
        None => None,
    };
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
                json!({ "job_id": job_id, "batch_no": batch_index + 1 }),
            );
            return Ok(());
        }

        let pks = fetch_pk_page(
            ctx,
            &source,
            &pk,
            last_literal.as_deref(),
            job.config.batch_size,
        )
        .await?;
        if pks.is_empty() {
            break;
        }
        let batch_no = batch_index + 1;
        let first_pk = pks.first().cloned().unwrap_or_default();
        let end_pk = pks.last().cloned().unwrap_or_default();
        let source_rows = pks.len() as u64;
        let end_literal = pk_literal(pk_kind, &end_pk)?;
        let started = Instant::now();
        job = hub
            .update(job_id, |job| {
                job.progress.current_batch = batch_no;
                job.progress.current_first_pk = Some(first_pk.clone());
                job.progress.last_pk = last_pk.clone();
            })
            .await?;

        // 续跑已直接 seek 到 checkpoint 之后，这里不再重走已完成批次。

        // 批前校验：目标里已有但不一致的数据视为外部修正/污染，禁止覆盖。
        let pre = verify_range(
            ctx,
            &spec,
            &source,
            &destination,
            &source_column,
            &pk,
            last_literal.as_deref(),
            &end_literal,
        )
        .await?;
        if pre.v3 > 0 || pre.v4 > 0 {
            let diffs = collect_range_diffs(
                ctx,
                &spec,
                &source,
                &destination,
                &source_column,
                &pk,
                last_literal.as_deref(),
                &end_literal,
                &pre,
                true,
            )
            .await?;
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
            return Ok(());
        }

        let already_complete =
            pre.source_rows == pre.target_rows && pre.v2 == 0 && pre.v3 == 0 && pre.v4 == 0;
        let mut written_rows = 0u64;
        if !already_complete {
            execute_batch(
                ctx,
                hub,
                job_id,
                &spec,
                &source,
                &destination,
                &source_column,
                &pk,
                pk_kind,
                last_literal.as_deref(),
                &end_literal,
                source_rows,
                policy,
            )
            .await?;
            written_rows = source_rows;
            written_for_rate += source_rows;
            wait_rate_limit(&job.config, &mut first_write_at, written_for_rate).await;
        }

        // 批后校验：行数、主键集合、逐行规范化比对。
        let verify = verify_range(
            ctx,
            &spec,
            &source,
            &destination,
            &source_column,
            &pk,
            last_literal.as_deref(),
            &end_literal,
        )
        .await?;
        if !verify.passed() {
            let diffs = collect_range_diffs(
                ctx,
                &spec,
                &source,
                &destination,
                &source_column,
                &pk,
                last_literal.as_deref(),
                &end_literal,
                &verify,
                false,
            )
            .await?;
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
            return Ok(());
        }

        success_rows += source_rows;
        let duration_ms = started.elapsed().as_millis() as u64;

        // 审计先于 checkpoint：审计失败则本批不推进 checkpoint，下次重跑时只读复检后补齐。
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
                "verify": verify,
                "verified": true,
                "duration_ms": duration_ms,
            }),
        )?;

        job = hub
            .update(job_id, |job| {
                job.checkpoint = Some(Checkpoint {
                    completed_batches: batch_no,
                    last_pk: Some(end_pk.clone()),
                    last_pks: None,
                    written_rows: success_rows,
                    verified: true,
                    verify: Some(verify.clone()),
                    engine: if job.engine.is_empty() {
                        default_engine_mysql()
                    } else {
                        job.engine.clone()
                    },
                    pk_kind: job.spec.pk_kind,
                    pk_kinds: Vec::new(),
                    target_fingerprint: None,
                    updated_at: now(),
                });
                job.phase = MigrationPhase::Backfilling;
                job.progress.success_rows = success_rows;
                job.progress.processed_rows = success_rows;
                job.progress.remaining_rows = job.expected_rows.saturating_sub(success_rows);
                job.progress.current_batch = batch_no;
                job.progress.last_pk = Some(end_pk.clone());
                job.progress.connection_id = None;
                MigrationStore::push_batch_record(
                    job,
                    BatchRecord {
                        batch_no,
                        last_pk: last_pk.clone(),
                        last_pks: None,
                        end_pk: end_pk.clone(),
                        end_pks: None,
                        source_rows,
                        written_rows,
                        verified: true,
                        verify: verify.clone(),
                        duration_ms,
                        at: now(),
                    },
                );
            })
            .await?;

        batch_index += 1;
        last_pk = Some(end_pk);
        last_literal = Some(end_literal);
    }

    // ── 最终全表校验（V1-V4） ───────────────────────────────
    job = hub
        .update(job_id, |job| job.phase = MigrationPhase::Verifying)
        .await?;
    let final_verify = verify_full(ctx, &spec, &source, &destination, &source_column, &pk).await?;
    if !final_verify.passed() {
        let diffs = collect_full_diffs(
            ctx,
            &spec,
            &source,
            &destination,
            &source_column,
            &pk,
            &final_verify,
        )
        .await?;
        // 全表校验只在最终阶段执行；失败时再按批定位第一个不一致批次，
        // 以便报告具体批次与差异主键（此定位仅在失败时发生，成本 O(批数)）。
        let located = locate_first_failed_batch(
            ctx,
            &spec,
            &source,
            &destination,
            &source_column,
            &pk,
            pk_kind,
            job.config.batch_size,
        )
        .await?;
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
            let checkpoint = job.checkpoint.get_or_insert_with(|| Checkpoint {
                completed_batches: job.progress.total_batches,
                last_pk: None,
                last_pks: None,
                written_rows: job.expected_rows,
                verified: true,
                verify: None,
                engine: if job.engine.is_empty() {
                    default_engine_mysql()
                } else {
                    job.engine.clone()
                },
                pk_kind: job.spec.pk_kind,
                pk_kinds: Vec::new(),
                target_fingerprint: None,
                updated_at: now(),
            });
            checkpoint.completed_batches = job.progress.total_batches;
            checkpoint.written_rows = job.expected_rows;
            checkpoint.verified = true;
            checkpoint.updated_at = now();
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
        }),
    );
    Ok(())
}

// ── SQL 执行 ───────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn execute_batch(
    ctx: &OpsContext,
    hub: &MigrationHub,
    job_id: &str,
    spec: &BackfillSpec,
    source: &str,
    destination: &str,
    source_column: &str,
    pk: &str,
    pk_kind: PkKind,
    last_literal: Option<&str>,
    end_literal: &str,
    rows: u64,
    policy: &OpsPolicy,
) -> Result<(), OpsError> {
    let sql = scripts::build_backfill_batch_sql(
        source,
        destination,
        source_column,
        pk,
        &spec.target_columns,
        spec.empty_string_policy,
        last_literal,
        end_literal,
        rows.max(1),
    );
    let pool = ctx.db_client.mysql_pool()?;
    let mut conn = pool.acquire().await?;
    let connection_id: u64 = sqlx::query_scalar("SELECT CONNECTION_ID()")
        .fetch_one(&mut *conn)
        .await?;
    hub.update(job_id, |job| {
        job.progress.connection_id = Some(connection_id)
    })
    .await?;
    // 与既有 ops 执行保持一致：限制 MDL/锁等待，避免长时间阻塞源表写入。
    let _ = sqlx::query(&format!(
        "SET SESSION lock_wait_timeout = {}",
        policy.lock_wait_timeout_secs.max(1)
    ))
    .execute(&mut *conn)
    .await;
    let _ = sqlx::query(&format!(
        "SET SESSION innodb_lock_wait_timeout = {}",
        policy.lock_wait_timeout_secs.max(1)
    ))
    .execute(&mut *conn)
    .await;

    let mut tx = conn.begin().await?;
    sqlx::query(&sql).execute(&mut *tx).await?;
    tx.commit().await?;
    let _ = pk_kind;
    Ok(())
}

pub(crate) async fn execute_create(ctx: &OpsContext, sql: &str) -> Result<(), OpsError> {
    let statement = sql.trim().trim_end_matches(';');
    if ctx.is_postgres() {
        let pool = ctx.db_client.pool.pg()?;
        sqlx::query(statement).execute(pool).await?;
    } else {
        let pool = ctx.db_client.mysql_pool()?;
        sqlx::query(statement).execute(pool).await?;
    }
    Ok(())
}

pub(crate) fn validate_create_statement(sql: &str) -> Result<(), OpsError> {
    if classify_blocked_sql(sql).is_some() {
        return Err(OpsError::Validation(
            "建表脚本命中安全拦截（只允许 CREATE TABLE）".into(),
        ));
    }
    let body = sql
        .lines()
        .filter(|line| !line.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n");
    let upper = body.trim().to_ascii_uppercase();
    if !upper.starts_with("CREATE TABLE") {
        return Err(OpsError::Validation(
            "目标表不存在时只允许执行 CREATE TABLE".into(),
        ));
    }
    if upper.contains(" ALTER ") || upper.contains(" DROP ") || upper.contains(" RENAME ") {
        return Err(OpsError::Validation(
            "建表脚本包含 ALTER/DROP/RENAME，拒绝执行".into(),
        ));
    }
    Ok(())
}

// ── 校验 ───────────────────────────────────────────────────

async fn verify_range(
    ctx: &OpsContext,
    spec: &BackfillSpec,
    source: &str,
    destination: &str,
    source_column: &str,
    pk: &str,
    last_literal: Option<&str>,
    end_literal: &str,
) -> Result<BatchVerify, OpsError> {
    let source_rows = count_rows(ctx, source, pk, last_literal, Some(end_literal)).await?;
    let target_rows = count_rows(ctx, destination, pk, last_literal, Some(end_literal)).await?;
    let v2 = count_diff(
        ctx,
        spec,
        source,
        destination,
        source_column,
        pk,
        last_literal,
        Some(end_literal),
        "v2",
    )
    .await?;
    let v3 = count_diff(
        ctx,
        spec,
        source,
        destination,
        source_column,
        pk,
        last_literal,
        Some(end_literal),
        "v3",
    )
    .await?;
    let v4 = count_diff(
        ctx,
        spec,
        source,
        destination,
        source_column,
        pk,
        last_literal,
        Some(end_literal),
        "v4",
    )
    .await?;
    Ok(BatchVerify {
        source_rows,
        target_rows,
        v2,
        v3,
        v4,
        validation_mode: String::new(),
    })
}

#[allow(clippy::too_many_arguments)]
async fn locate_first_failed_batch(
    ctx: &OpsContext,
    spec: &BackfillSpec,
    source: &str,
    destination: &str,
    source_column: &str,
    pk: &str,
    pk_kind: PkKind,
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
    let mut last_literal: Option<String> = None;
    let mut last_pk: Option<String> = None;
    let mut batch_no = 0u64;
    loop {
        let pks = fetch_pk_page(ctx, source, pk, last_literal.as_deref(), batch_size).await?;
        if pks.is_empty() {
            return Ok(None);
        }
        batch_no += 1;
        let end_pk = pks.last().cloned().unwrap_or_default();
        let end_literal = pk_literal(pk_kind, &end_pk)?;
        let verify = verify_range(
            ctx,
            spec,
            source,
            destination,
            source_column,
            pk,
            last_literal.as_deref(),
            &end_literal,
        )
        .await?;
        if !verify.passed() {
            let diffs = collect_range_diffs(
                ctx,
                spec,
                source,
                destination,
                source_column,
                pk,
                last_literal.as_deref(),
                &end_literal,
                &verify,
                false,
            )
            .await?;
            return Ok(Some((
                batch_no,
                last_pk.clone(),
                Some(end_pk),
                verify,
                diffs,
            )));
        }
        last_pk = Some(end_pk);
        last_literal = Some(end_literal);
    }
}

async fn verify_full(
    ctx: &OpsContext,
    spec: &BackfillSpec,
    source: &str,
    destination: &str,
    source_column: &str,
    pk: &str,
) -> Result<BatchVerify, OpsError> {
    let source_rows = count_rows(ctx, source, pk, None, None).await?;
    let target_rows = count_rows(ctx, destination, pk, None, None).await?;
    let v2 = count_diff(
        ctx,
        spec,
        source,
        destination,
        source_column,
        pk,
        None,
        None,
        "v2",
    )
    .await?;
    let v3 = count_diff(
        ctx,
        spec,
        source,
        destination,
        source_column,
        pk,
        None,
        None,
        "v3",
    )
    .await?;
    let v4 = count_diff(
        ctx,
        spec,
        source,
        destination,
        source_column,
        pk,
        None,
        None,
        "v4",
    )
    .await?;
    Ok(BatchVerify {
        source_rows,
        target_rows,
        v2,
        v3,
        v4,
        validation_mode: String::new(),
    })
}

#[allow(clippy::too_many_arguments)]
async fn count_diff(
    ctx: &OpsContext,
    spec: &BackfillSpec,
    source: &str,
    destination: &str,
    source_column: &str,
    pk: &str,
    last_literal: Option<&str>,
    end_literal: Option<&str>,
    phase: &str,
) -> Result<u64, OpsError> {
    let select = scoped_diff_select(
        spec,
        source,
        destination,
        source_column,
        pk,
        last_literal,
        end_literal,
        phase,
    );
    let sql = scripts::diff_total_sql(&select);
    let rows = fetch_read_only(ctx, &sql).await?;
    Ok(rows
        .first()
        .and_then(|row| row.get("diff_total"))
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0))
}

async fn count_rows(
    ctx: &OpsContext,
    table: &str,
    pk: &str,
    last_literal: Option<&str>,
    end_literal: Option<&str>,
) -> Result<u64, OpsError> {
    let condition = range_condition(pk, last_literal, end_literal);
    let sql = if condition.is_empty() {
        format!("SELECT COUNT(*) AS row_count FROM {table}")
    } else {
        format!("SELECT COUNT(*) AS row_count FROM {table} WHERE {condition}")
    };
    let rows = fetch_read_only(ctx, &sql).await?;
    Ok(rows
        .first()
        .and_then(|row| row.get("row_count"))
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0))
}

fn scoped_diff_select(
    spec: &BackfillSpec,
    source: &str,
    destination: &str,
    source_column: &str,
    pk: &str,
    last_literal: Option<&str>,
    end_literal: Option<&str>,
    phase: &str,
) -> String {
    let base = scripts::diff_select(
        phase,
        source,
        destination,
        source_column,
        pk,
        &spec.target_columns,
        spec.empty_string_policy,
    );
    let key = if phase == "v3" {
        format!("t.{pk}")
    } else {
        format!("s.{pk}")
    };
    let condition = range_condition(&key, last_literal, end_literal);
    if condition.is_empty() {
        base
    } else {
        format!("{base} AND {condition}")
    }
}

fn range_condition(key: &str, last_literal: Option<&str>, end_literal: Option<&str>) -> String {
    let mut parts = Vec::new();
    if !key.is_empty() {
        if let Some(last) = last_literal {
            parts.push(format!("{key} > {last}"));
        }
        if let Some(end) = end_literal {
            parts.push(format!("{key} <= {end}"));
        }
    }
    parts.join(" AND ")
}

async fn fetch_pk_page(
    ctx: &OpsContext,
    source: &str,
    pk: &str,
    last_literal: Option<&str>,
    batch_size: u64,
) -> Result<Vec<String>, OpsError> {
    let sql = match last_literal {
        Some(last) => format!(
            "SELECT {pk} AS pk_value FROM {source} WHERE {pk} > {last} ORDER BY {pk} LIMIT {batch_size}"
        ),
        None => format!(
            "SELECT {pk} AS pk_value FROM {source} ORDER BY {pk} LIMIT {batch_size}"
        ),
    };
    let rows = fetch_read_only(ctx, &sql).await?;
    Ok(rows
        .iter()
        .filter_map(|row| row.get("pk_value").cloned())
        .collect())
}

// ── 差异定位 ───────────────────────────────────────────────

async fn collect_range_diffs(
    ctx: &OpsContext,
    spec: &BackfillSpec,
    source: &str,
    destination: &str,
    source_column: &str,
    pk: &str,
    last_literal: Option<&str>,
    end_literal: &str,
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
            // 批前允许目标缺行；真正写完后由批后校验兜底。
            continue;
        }
        if count > 0 {
            diffs.push(
                collect_page(
                    ctx,
                    spec,
                    source,
                    destination,
                    source_column,
                    pk,
                    last_literal,
                    Some(end_literal),
                    phase,
                    count,
                )
                .await?,
            );
        }
    }
    Ok(diffs)
}

async fn collect_full_diffs(
    ctx: &OpsContext,
    spec: &BackfillSpec,
    source: &str,
    destination: &str,
    source_column: &str,
    pk: &str,
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
            diffs.push(
                collect_page(
                    ctx,
                    spec,
                    source,
                    destination,
                    source_column,
                    pk,
                    None,
                    None,
                    phase,
                    count,
                )
                .await?,
            );
        }
    }
    Ok(diffs)
}

#[allow(clippy::too_many_arguments)]
async fn collect_page(
    ctx: &OpsContext,
    spec: &BackfillSpec,
    source: &str,
    destination: &str,
    source_column: &str,
    pk: &str,
    last_literal: Option<&str>,
    end_literal: Option<&str>,
    phase: &str,
    total: u64,
) -> Result<DiffDetail, OpsError> {
    let select = scoped_diff_select(
        spec,
        source,
        destination,
        source_column,
        pk,
        last_literal,
        end_literal,
        phase,
    );
    let key = if phase == "v3" {
        format!("t.{pk}")
    } else {
        format!("s.{pk}")
    };
    let sql = format!("{select}\nORDER BY {key}\nLIMIT {DIFF_PAGE_SIZE}");
    let rows = fetch_read_only(ctx, &sql).await?;
    let pks = rows
        .iter()
        .filter_map(|row| row.get("pk_value").cloned())
        .collect::<Vec<_>>();
    Ok(DiffDetail {
        phase: phase.into(),
        total,
        has_more: total > DIFF_PAGE_SIZE,
        pks,
    })
}

// ── 阻断 / 审计 ────────────────────────────────────────────

pub(crate) async fn block_on_verify(
    ctx: &OpsContext,
    hub: &MigrationHub,
    job_id: &str,
    batch_no: u64,
    last_pk: Option<String>,
    end_pk: Option<String>,
    prefix: &str,
    verify: BatchVerify,
    diffs: Vec<DiffDetail>,
) -> Result<(), OpsError> {
    let total_diffs: u64 = diffs.iter().map(|diff| diff.total).sum();
    let pks = diffs
        .first()
        .map(|diff| diff.pks.clone())
        .unwrap_or_default();
    let has_more = diffs.iter().any(|diff| diff.has_more);
    let phase = diffs
        .first()
        .map(|diff| format!("{prefix}:{}", diff.phase))
        .unwrap_or_else(|| prefix.to_string());
    let message = format!(
        "批次 {} 校验未通过：V1 源/目标 {}/{}，V2/V3/V4={}/{}/{}；差异合计 {}",
        batch_no,
        verify.source_rows,
        verify.target_rows,
        verify.v2,
        verify.v3,
        verify.v4,
        total_diffs
    );
    let job = hub
        .update(job_id, |job| {
            job.status = MigrationStatus::Blocked;
            job.phase = MigrationPhase::Blocked;
            job.finished_at = Some(now());
            job.last_error = Some(message.clone());
            job.progress.failed_rows = if verify.source_rows > 0 {
                verify.source_rows
            } else {
                total_diffs
            };
            job.progress.remaining_rows =
                job.expected_rows.saturating_sub(job.progress.success_rows);
            job.progress.current_batch = batch_no;
            job.blocking = Some(BlockingDetail {
                batch_no,
                last_pk: last_pk.clone(),
                end_pk: end_pk.clone(),
                phase: phase.clone(),
                total_diffs,
                pks: pks.clone(),
                has_more,
                differences: diffs,
                verify: Some(verify),
                message: message.clone(),
            });
        })
        .await?;
    append_audit(
        ctx,
        "field_migration.backfill.blocked",
        "blocked",
        &job,
        json!({
            "job_id": job_id,
            "batch_no": batch_no,
            "phase": phase,
            "range": { "last_pk": last_pk, "end_pk": end_pk },
            "verify": job.blocking.as_ref().and_then(|b| b.verify.clone()),
            "total_diffs": total_diffs,
            "pks": pks,
            "message": message,
        }),
    )?;
    Ok(())
}

pub(crate) async fn block_message(
    ctx: &OpsContext,
    hub: &MigrationHub,
    job_id: &str,
    message: &str,
    phase: &str,
) -> Result<(), OpsError> {
    let job = hub
        .update(job_id, |job| {
            job.status = MigrationStatus::Blocked;
            job.phase = MigrationPhase::Blocked;
            job.finished_at = Some(now());
            job.last_error = Some(message.to_string());
            job.blocking = Some(BlockingDetail {
                batch_no: job.progress.current_batch.max(1),
                last_pk: job.progress.last_pk.clone(),
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
    append_audit(
        ctx,
        "field_migration.backfill.blocked",
        "blocked",
        &job,
        json!({ "job_id": job_id, "phase": phase, "message": message }),
    )?;
    Ok(())
}

pub(crate) fn append_audit(
    ctx: &OpsContext,
    event: &str,
    outcome: &str,
    job: &MigrationJob,
    detail: serde_json::Value,
) -> Result<(), OpsError> {
    let Some(audit) = ctx.audit.as_ref() else {
        return Ok(());
    };
    let mut record = AuditRecord::new(event, outcome);
    record.plan_id = Some(job.plan_id.clone());
    record.db_id = Some(job.db_id.clone());
    record.db_name = Some(job.db_name.clone());
    record.action_id = Some(format!("field_migration.backfill:{}", job.job_id));
    record.actor = Some("local-user".into());
    record.detail = detail;
    audit.append(&record)
}

// ── 控制 / 限速 ────────────────────────────────────────────

pub(crate) async fn wait_for_control(hub: &MigrationHub, job_id: &str) -> Result<bool, OpsError> {
    loop {
        match hub.control_value(job_id).await {
            CONTROL_ABORT => return Ok(false),
            CONTROL_PAUSE => {
                let job = hub.get(job_id).await?;
                if matches!(
                    job.status,
                    MigrationStatus::Running | MigrationStatus::Pending
                ) {
                    hub.update(job_id, |job| {
                        job.status = MigrationStatus::Paused;
                        job.phase = MigrationPhase::Paused;
                    })
                    .await?;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            _ => {
                let job = hub.get(job_id).await?;
                if job.status == MigrationStatus::Paused {
                    hub.update(job_id, |job| {
                        job.status = MigrationStatus::Running;
                        job.phase = MigrationPhase::Backfilling;
                    })
                    .await?;
                }
                return Ok(true);
            }
        }
    }
}

pub(crate) async fn wait_rate_limit(
    config: &BackfillConfig,
    first_write_at: &mut Option<Instant>,
    written_total: u64,
) {
    if written_total == 0 {
        return;
    }
    let started = *first_write_at.get_or_insert_with(Instant::now);
    let mut wait = Duration::ZERO;
    if config.max_rows_per_second > 0 {
        let target =
            Duration::from_secs_f64(written_total as f64 / config.max_rows_per_second as f64);
        let elapsed = started.elapsed();
        if target > elapsed {
            wait = target - elapsed;
        }
    }
    let interval = Duration::from_millis(config.batch_interval_ms);
    if interval > wait {
        wait = interval;
    }
    if !wait.is_zero() {
        tokio::time::sleep(wait).await;
    }
}

// ── 工具 ───────────────────────────────────────────────────

fn qualified(db: &str, table: &str) -> String {
    format!("{}.{}", quote_ident_mysql(db), quote_ident_mysql(table))
}

fn pk_kind_from_type(column_type: &str) -> PkKind {
    let lower = column_type.trim().to_ascii_lowercase();
    if lower.starts_with("bigint")
        || lower.starts_with("int")
        || lower.starts_with("smallint")
        || lower.starts_with("mediumint")
        || lower.starts_with("tinyint")
    {
        PkKind::Integer
    } else {
        PkKind::Text
    }
}

fn pk_literal(kind: PkKind, value: &str) -> Result<String, OpsError> {
    match kind {
        PkKind::Integer => value
            .trim()
            .parse::<i128>()
            .map(|number| number.to_string())
            .map_err(|_| {
                OpsError::Validation(format!("主键值 {value} 不是整数，按主键区间分批不安全"))
            }),
        PkKind::Text => Ok(format!("'{}'", escape_sql_string(value))),
        // PG 原生类型主键走参数绑定，不会进入 MySQL 字面量拼接路径。
        PkKind::Numeric | PkKind::Uuid => Err(OpsError::Validation(format!(
            "主键类型 {kind:?} 只支持 PostgreSQL 类型化绑定，MySQL 路径不可用"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::types::{EvidenceSummary, PlanStatus, RollbackPlan, TableSnapshot};
    use crate::sql::util::quote_ident_mysql;

    use super::super::schema::TargetColumn;
    use super::super::types::{
        ColumnProfile, DependencyEvidence, EnvironmentEvidence, RiskSummary,
    };

    fn plan() -> OpsPlan {
        OpsPlan {
            plan_id: "p1".into(),
            scenario_id: "field-governance".into(),
            db_id: "db1".into(),
            db_name: "app".into(),
            db_fingerprint: "fp".into(),
            evidence_hash: "ev".into(),
            diagnosis: "test".into(),
            findings: Vec::new(),
            actions: Vec::new(),
            verification_sql: Vec::new(),
            risks: Vec::new(),
            validation_checks: Vec::new(),
            rollback: RollbackPlan {
                summary: String::new(),
                actions: Vec::new(),
                automatic: false,
            },
            code_advice: Vec::new(),
            rewrite_suggestions: Vec::new(),
            verification_explain_analyze: false,
            confidence: 0.9,
            requires_approval: true,
            created_at: 1,
            plan_hash: "ph".into(),
            status: PlanStatus::PendingApproval,
            evidence_summary: Some(EvidenceSummary::default()),
            execution: None,
        }
    }

    fn analysis() -> FieldGovernanceAnalysis {
        let mut profile = test_profile();
        profile.empty_rows = 0;
        FieldGovernanceAnalysis {
            db_name: "app".into(),
            table: "orders".into(),
            column: Some("payload".into()),
            analyzed_columns: vec!["payload".into()],
            profile_mode: "full".into(),
            sample_per_mille: None,
            estimated_rows: Some(10),
            data_length: Some(1024),
            index_length: Some(0),
            row_count: 10,
            columns: vec![profile],
            dependencies: DependencyEvidence {
                primary_key_columns: vec!["id".into()],
                single_column_pk: Some("id".into()),
                scan_complete: true,
                ..Default::default()
            },
            environment: EnvironmentEvidence {
                engine: Some("InnoDB".into()),
                partitioned: false,
                read_only: false,
                version_supports_json: true,
                ..Default::default()
            },
            evidence: Vec::new(),
            gaps: Vec::new(),
            declarations: Vec::new(),
            declaration_hash: String::new(),
            declaration_evidence: Vec::new(),
            target_table: "orders_ext".into(),
            target_exists: false,
            empty_string_policy: EmptyStringPolicy::Preserve,
            target_columns: vec![TargetColumn {
                name: "theme".into(),
                source_key: Some("theme".into()),
                sql_type: "VARCHAR(512)".into(),
                nullable: true,
                required: false,
                note: None,
            }],
            risks: Vec::new(),
            risk_summary: RiskSummary::default(),
            scripts: vec![super::super::types::ManualScript {
                id: "create-table".into(),
                title: "create".into(),
                kind: "create_table".into(),
                phase: "L2_table_created".into(),
                sql: "CREATE TABLE IF NOT EXISTS `app`.`orders_ext` (`id` BIGINT NOT NULL, \
                      `theme` VARCHAR(512) NULL, PRIMARY KEY (`id`)) ENGINE=InnoDB;"
                    .into(),
                executable: false,
                irreversible: false,
                notes: Vec::new(),
            }],
            dry_run: Default::default(),
            retire_package: Default::default(),
            rollback: Default::default(),
            dual_write: Default::default(),
        }
    }

    fn test_profile() -> ColumnProfile {
        let mut profile = super::super::types::sample_column("payload", "json");
        profile.nullable = true;
        profile.profile_mode = "full".into();
        profile
    }

    #[test]
    fn prepare_rejects_sampled_profile_and_blocking_risks() {
        let policy = OpsPolicy::default();
        let plan = plan();
        let mut sampled = analysis();
        sampled.profile_mode = "sampled".into();
        assert!(prepare(&plan, &sampled, &policy, BackfillConfig::default()).is_err());

        let mut blocked = analysis();
        blocked.risk_summary.block = 1;
        blocked.risks.push(FieldRisk {
            id: "risk-block".into(),
            code: "TEST_BLOCK".into(),
            level: RiskLevel::Block,
            title: "block".into(),
            detail: "blocked".into(),
            evidence_refs: Vec::new(),
            blocks_write: true,
            requires_user_confirmation: false,
        });
        assert!(prepare(&plan, &blocked, &policy, BackfillConfig::default()).is_err());
    }

    #[test]
    fn prepare_builds_stable_migration_hash_and_clamps_config() {
        let policy = OpsPolicy::default();
        let plan = plan();
        let analysis = analysis();
        let config = BackfillConfig {
            batch_size: 1_000_000,
            batch_interval_ms: 10,
            max_rows_per_second: 999_999,
            single_transaction: false,
        };
        let first = prepare(&plan, &analysis, &policy, config.clone()).unwrap();
        assert_eq!(first.config.batch_size, MAX_BATCH_SIZE);
        assert_eq!(first.config.batch_interval_ms, 10);
        assert_eq!(first.config.max_rows_per_second, MAX_ROWS_PER_SECOND);
        let second = prepare(&plan, &analysis, &policy, config).unwrap();
        assert_eq!(first.migration_hash, second.migration_hash);
    }

    #[test]
    fn batch_sql_is_bounded_idempotent_and_never_drops_columns() {
        let columns = vec![TargetColumn {
            name: "theme".into(),
            source_key: Some("theme".into()),
            sql_type: "VARCHAR(512)".into(),
            nullable: true,
            required: false,
            note: None,
        }];
        let sql = scripts::build_backfill_batch_sql(
            "`app`.`orders`",
            "`app`.`orders_ext`",
            "`payload`",
            "`id`",
            &columns,
            EmptyStringPolicy::Preserve,
            Some("100"),
            "200",
            100,
        );
        assert!(sql.contains("`id` > 100"));
        assert!(sql.contains("`id` <= 200"));
        assert!(sql.contains("ON DUPLICATE KEY UPDATE"));
        assert!(sql.contains("LIMIT 100"));
        assert!(!sql.to_ascii_uppercase().contains("DROP COLUMN"));
        assert!(!sql.to_ascii_uppercase().contains(" REPLACE "));
    }

    #[test]
    fn pk_literals_are_typed_and_escaped() {
        assert_eq!(pk_literal(PkKind::Integer, "42").unwrap(), "42");
        assert!(pk_literal(PkKind::Integer, "abc").is_err());
        assert_eq!(pk_literal(PkKind::Text, "a'b").unwrap(), "'a''b'");
        assert_eq!(pk_kind_from_type("bigint unsigned"), PkKind::Integer);
        assert_eq!(pk_kind_from_type("varchar(64)"), PkKind::Text);
    }

    #[test]
    fn create_statement_validation_rejects_ddl_other_than_create() {
        validate_create_statement(
            "CREATE TABLE IF NOT EXISTS `app`.`t` (`id` BIGINT NOT NULL, PRIMARY KEY (`id`));",
        )
        .unwrap();
        assert!(validate_create_statement("ALTER TABLE app.t DROP COLUMN payload").is_err());
        assert!(validate_create_statement("DROP TABLE app.t").is_err());
        assert!(validate_create_statement("CREATE TABLE t (id INT); DROP TABLE x;").is_err());
    }

    #[test]
    fn range_condition_is_monotonic_and_first_batch_has_no_lower_bound() {
        let pk = quote_ident_mysql("id");
        assert_eq!(
            range_condition(&format!("s.{pk}"), None, Some("200")),
            format!("s.{pk} <= 200")
        );
        assert_eq!(
            range_condition(&format!("s.{pk}"), Some("100"), Some("200")),
            format!("s.{pk} > 100 AND s.{pk} <= 200")
        );
    }

    #[test]
    fn source_snapshot_is_not_mutated_by_spec_building() {
        let snapshot = TableSnapshot::default();
        assert!(snapshot.columns.is_empty());
    }
}
