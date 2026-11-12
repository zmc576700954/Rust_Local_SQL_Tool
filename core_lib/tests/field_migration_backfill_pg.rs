//! 字段回填 S2 的 PostgreSQL 真库验收（opt-in）。
//!
//! 运行方式：
//! ```bash
//! FIELD_MIGRATION_PG_URL='postgres://postgres:password@127.0.0.1:55432/postgres' \
//!   cargo test -p core_lib --test field_migration_backfill_pg -- --test-threads=1 --no-fail-fast
//! ```
//! 未设置环境变量时打印原因并返回，不影响常规单测。
//!
//! 覆盖：
//! 1) 10 万行分批提交、`pg_terminate_backend` 中断后 checkpoint 续跑；
//! 2) 批中失败只回滚当前批（rollback_scope=batch），修复约束后续跑完成；
//! 3) 老 checkpoint（engine 缺省 mysql）拒绝被 PG 复用；
//! 4) 目标缺唯一索引 BLOCK、仅 UNIQUE 索引可满足 ON CONFLICT；
//! 5) `INSERT ... SELECT ... ORDER BY ... LIMIT` 每批取最小主键；
//! 6) text（COLLATE "C"）/numeric/uuid 主键的类型化绑定分页；
//! 7) 限速与 `SET LOCAL` 超时映射；
//! 8) single_transaction 整任务回滚且不可续跑。
//!
//! 说明：E14_TARGET_EXISTS 为 Block 级风险，目标表预创建的用例（CHECK 失败、
//! 缺唯一索引、仅 UNIQUE、单事务回滚）直接构造 MigrationJob 到达执行器预检；
//! 干净路径（建表 + 回填 + 校验）仍走 analyze → prepare → new_job 完整链路。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use sqlx::postgres::PgPool;
use sqlx::Executor;

use core_lib::db::client::DbClient;
use core_lib::ops::field_migration::backfill;
use core_lib::ops::field_migration::migration_store::{
    BackfillConfig, BackfillExecution, BackfillSpec, Checkpoint, MigrationHub, MigrationJob,
    MigrationPhase, MigrationStatus, PkKind, CONTROL_ABORT, CONTROL_PAUSE,
};
use core_lib::ops::field_migration::pg_backfill;
use core_lib::ops::field_migration::schema::TargetColumn;
use core_lib::ops::field_migration::types::{
    ColumnMapping, EmptyStringPolicy, FieldGovernanceAnalysis, FieldGovernanceInput, TargetSpec,
};
use core_lib::ops::policy::OpsPolicy;
use core_lib::ops::registry::{OpsContext, ScenarioRegistry};

const DB_ID: &str = "field-migration-pg-s2-e2e";

static DB_SEQ: AtomicU64 = AtomicU64::new(0);

/// 运行内唯一的库名后缀：进程号 + 自增序号 + 纳秒时间，避免并行用例互相干扰。
fn unique_suffix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "{}_{}_{}",
        std::process::id(),
        DB_SEQ.fetch_add(1, Ordering::Relaxed),
        nanos
    )
}

fn unique_db(prefix: &str) -> String {
    format!("{prefix}_{}", unique_suffix())
}

/// 换掉 URL 里的库名，保留账号、主机、端口与查询参数。
fn with_database(url: &str, db: &str) -> String {
    match url.rsplit_once('/') {
        Some((base, tail)) => {
            let query = tail.find('?').map(|i| &tail[i..]).unwrap_or("");
            format!("{base}/{db}{query}")
        }
        None => format!("{url}/{db}"),
    }
}

async fn admin_exec(pool: &PgPool, sql: &str) {
    if let Err(err) = pool.execute(sql).await {
        panic!("执行管理 SQL 失败: {err}\nSQL: {sql}");
    }
}

async fn create_database(pool: &PgPool, db: &str) {
    admin_exec(
        pool,
        &format!("DROP DATABASE IF EXISTS \"{db}\" WITH (FORCE)"),
    )
    .await;
    admin_exec(pool, &format!("CREATE DATABASE \"{db}\"")).await;
}

/// 只删除本用例创建的库；失败必须显式报错，不能静默吞掉。
async fn cleanup_database(pool: &PgPool, db: &str) {
    let terminate = format!(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '{db}' AND pid <> pg_backend_pid()"
    );
    if let Err(err) = pool.execute(terminate.as_str()).await {
        eprintln!("终止连接失败: {err}");
    }
    let sql = format!("DROP DATABASE IF EXISTS \"{db}\" WITH (FORCE)");
    if let Err(err) = pool.execute(sql.as_str()).await {
        eprintln!("清理失败: 数据库 \"{db}\": {err}\nSQL: {sql}");
        panic!("清理失败: 数据库 \"{db}\": {err}");
    }
}

async fn isolated_database(base_url: &str, prefix: &str) -> (String, PgPool) {
    let admin_pool = PgPool::connect(base_url)
        .await
        .expect("连接 PostgreSQL 管理库失败，请确认 phase3-pg 已就绪");
    let db = unique_db(prefix);
    create_database(&admin_pool, &db).await;
    (db, admin_pool)
}

async fn exec(pool: &PgPool, sql: &str) {
    if let Err(err) = pool.execute(sql).await {
        panic!("执行 SQL 失败: {err}\nSQL: {sql}");
    }
}

async fn count(pool: &PgPool, sql: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(sql)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|err| panic!("查询失败: {err}\nSQL: {sql}"))
}

fn test_policy() -> Arc<OpsPolicy> {
    Arc::new(OpsPolicy {
        field_profile_max_rows: 10_000_000,
        field_profile_max_bytes: 20 * 1024 * 1024 * 1024,
        ..Default::default()
    })
}

async fn make_ctx(url: &str, db_name: &str, policy: &Arc<OpsPolicy>) -> OpsContext {
    let client = DbClient::new_default(url)
        .await
        .expect("创建 DbClient 失败");
    OpsContext::new(client, DB_ID.into(), db_name.to_string(), false).with_policy(policy.clone())
}

/// jsonb 源表：`score` 取主键值，便于用 CHECK 精确控制失败批次。
async fn seed_jsonb(pool: &PgPool, source: &str, rows: i64) {
    exec(
        pool,
        &format!("CREATE TABLE {source} (id bigint PRIMARY KEY, payload jsonb)"),
    )
    .await;
    exec(
        pool,
        &format!(
            "INSERT INTO {source} \
             SELECT g, jsonb_build_object('theme', 'dark', 'score', g, 'note', 'n-' || g) \
             FROM generate_series(1, {rows}) AS g"
        ),
    )
    .await;
    exec(pool, &format!("ANALYZE {source}")).await;
}

fn input_for(source: &str, target: &str) -> FieldGovernanceInput {
    FieldGovernanceInput {
        table: source.into(),
        column: Some("payload".into()),
        target: Some(TargetSpec {
            table: Some(target.into()),
            pk_column: Some("id".into()),
            collation: None,
            residual_json: Some(false),
            mappings: vec![
                ColumnMapping {
                    source_key: Some("theme".into()),
                    target_column: "theme".into(),
                    target_type: Some("text".into()),
                    required: Some(false),
                },
                ColumnMapping {
                    source_key: Some("score".into()),
                    target_column: "score".into(),
                    target_type: Some("bigint".into()),
                    required: Some(false),
                },
                ColumnMapping {
                    source_key: Some("note".into()),
                    target_column: "note".into(),
                    target_type: Some("text".into()),
                    required: Some(false),
                },
            ],
        }),
        declarations: Vec::new(),
        profile_mode: Some("full".into()),
        sample_per_mille: None,
        empty_string_policy: Some(EmptyStringPolicy::Preserve),
        observation_started_at: None,
        notes: None,
    }
}

/// 走与 web-server 相同的链路：分析 → 计划 → prepare → job。目标表由任务自建。
async fn build_job(
    ctx: &OpsContext,
    policy: &OpsPolicy,
    source: &str,
    target: &str,
    config: BackfillConfig,
) -> MigrationJob {
    let registry = ScenarioRegistry::builtin();
    let scenario = registry.find("field-governance").expect("场景未注册");
    let input = input_for(source, target);
    let input_value = serde_json::to_value(&input).expect("序列化输入失败");
    let evidence = scenario
        .collect_evidence(ctx, &input_value)
        .await
        .expect("采集证据失败");
    let analysis: FieldGovernanceAnalysis =
        serde_json::from_value(evidence.extra.clone().expect("缺少字段治理扩展证据"))
            .expect("解析字段治理证据失败");
    let blocking: Vec<String> = analysis
        .risks
        .iter()
        .filter(|risk| risk.level == core_lib::ops::field_migration::types::RiskLevel::Block)
        .map(|risk| format!("{}: {}", risk.code, risk.title))
        .collect();
    if !blocking.is_empty() || !analysis.gaps.is_empty() {
        panic!(
            "seed 数据不应出现阻断/缺口: blocking={blocking:?} gaps={:?}",
            analysis.gaps
        );
    }
    let plan = scenario
        .build_plan(&ctx.identity(), &evidence, &input_value, policy)
        .expect("生成计划失败");
    let prepared = backfill::prepare(&plan, &analysis, policy, config).expect("prepare 失败");
    assert!(prepared.execution.is_some(), "PG 任务必须带 execution 语义");
    backfill::new_job(&plan, &prepared, false)
}

/// 直接构造任务（目标表预创建场景）。E14 目标已存在是 Block 级风险，
/// 这些用例本来就到不了 prepare；手工构造等价于 prepare 产出的 spec。
fn manual_job(db_name: &str, source: &str, target: &str, config: BackfillConfig) -> MigrationJob {
    manual_job_with_pk(db_name, source, target, Vec::new(), config)
}

/// 复合主键场景的手工任务：spec.pk_columns 显式给出（目标表预建，E14 无法走 prepare）。
fn manual_job_with_pk(
    db_name: &str,
    source: &str,
    target: &str,
    pk_columns: Vec<String>,
    config: BackfillConfig,
) -> MigrationJob {
    let single = config.single_transaction;
    let pk_column = pk_columns.first().cloned().unwrap_or_else(|| "id".into());
    MigrationJob {
        job_id: format!("pg-s2-{}", unique_suffix()),
        plan_id: "pg-s2-manual".into(),
        plan_hash: "manual".into(),
        migration_hash: "manual".into(),
        approval_id: None,
        db_id: DB_ID.into(),
        db_name: db_name.into(),
        db_fingerprint: "manual".into(),
        spec: BackfillSpec {
            source_table: source.into(),
            source_column: "payload".into(),
            target_table: target.into(),
            pk_column,
            pk_columns,
            pk_kind: None,
            pk_kinds: Vec::new(),
            target_columns: vec![
                TargetColumn {
                    name: "theme".into(),
                    source_key: Some("theme".into()),
                    sql_type: "text".into(),
                    nullable: true,
                    required: false,
                    note: None,
                },
                TargetColumn {
                    name: "score".into(),
                    source_key: Some("score".into()),
                    sql_type: "bigint".into(),
                    nullable: true,
                    required: false,
                    note: None,
                },
                TargetColumn {
                    name: "note".into(),
                    source_key: Some("note".into()),
                    sql_type: "text".into(),
                    nullable: true,
                    required: false,
                    note: None,
                },
            ],
            empty_string_policy: EmptyStringPolicy::Preserve,
            create_table_sql: None,
            schema: Some("public".into()),
            source_data_type: "jsonb".into(),
        },
        config,
        engine: String::new(),
        execution: Some(BackfillExecution {
            atomic: single,
            rollback_scope: if single { "job".into() } else { "batch".into() },
            resumable: !single,
            validation_mode: "snapshot".into(),
        }),
        status: MigrationStatus::Pending,
        phase: MigrationPhase::Pending,
        production_confirmed: false,
        checkpoint: None,
        progress: Default::default(),
        blocking: None,
        recent_batches: Vec::new(),
        source_schema_hash: String::new(),
        target_schema_hash: String::new(),
        target_created_by_job: false,
        expected_rows: 0,
        created_at: 0,
        updated_at: 0,
        started_at: 0,
        finished_at: None,
        last_error: None,
        run_count: 0,
    }
}

async fn run_to_end(
    hub: &MigrationHub,
    ctx: &OpsContext,
    policy: &OpsPolicy,
    job_id: &str,
) -> MigrationJob {
    backfill::run_job(job_id.to_string(), ctx.clone(), hub.clone(), policy.clone()).await;
    hub.get(job_id).await.expect("任务丢失")
}

async fn wait_until<F>(
    hub: &MigrationHub,
    job_id: &str,
    mut predicate: F,
    timeout: Duration,
) -> MigrationJob
where
    F: FnMut(&MigrationJob) -> bool,
{
    let deadline = Instant::now() + timeout;
    loop {
        let job = hub.get(job_id).await.expect("任务丢失");
        if predicate(&job) {
            return job;
        }
        if Instant::now() > deadline {
            panic!("等待超时，当前任务: status={:?}", job.status);
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn backfill_batches_commit_and_resume() {
    let Ok(base_url) = std::env::var("FIELD_MIGRATION_PG_URL") else {
        eprintln!("跳过 S2 PG 真库验证：未设置 FIELD_MIGRATION_PG_URL");
        return;
    };
    let (db_name, admin_pool) = isolated_database(&base_url, "it_s2pg_full").await;
    let url = with_database(&base_url, &db_name);
    let pool = PgPool::connect(&url).await.expect("连接 PG 失败");
    let source = "fm_s2pg_full_source";
    let target = "fm_s2pg_full_target";
    seed_jsonb(&pool, source, 100_000).await;

    let policy = test_policy();
    let ctx = make_ctx(&url, &db_name, &policy).await;
    let job = build_job(
        &ctx,
        policy.as_ref(),
        source,
        target,
        BackfillConfig {
            batch_size: 10_000,
            batch_interval_ms: 0,
            max_rows_per_second: 0,
            single_transaction: false,
        },
    )
    .await;
    let job_id = job.job_id.clone();
    let hub = MigrationHub::for_test();
    hub.insert(job).await.unwrap();

    let handle = tokio::spawn(backfill::run_job(
        job_id.clone(),
        ctx.clone(),
        hub.clone(),
        policy.as_ref().clone(),
    ));
    let before_kill = wait_until(
        &hub,
        &job_id,
        |job| {
            job.checkpoint
                .as_ref()
                .map(|checkpoint| checkpoint.completed_batches >= 1)
                .unwrap_or(false)
        },
        Duration::from_secs(180),
    )
    .await;
    println!(
        "中断前: batches={} checkpoint_pk={:?}",
        before_kill.checkpoint.as_ref().unwrap().completed_batches,
        before_kill.checkpoint.as_ref().unwrap().last_pk
    );
    // 捕获正在执行的批次并终止其后端，模拟进程/连接崩溃。
    let mut terminated = false;
    let deadline = Instant::now() + Duration::from_secs(180);
    while Instant::now() < deadline {
        let job = hub.get(&job_id).await.unwrap();
        if !matches!(
            job.status,
            MigrationStatus::Running | MigrationStatus::Pending
        ) {
            break;
        }
        if let Some(pid) = job.progress.connection_id {
            let killed: bool = sqlx::query_scalar("SELECT pg_terminate_backend($1)")
                .bind(pid as i32)
                .fetch_one(&pool)
                .await
                .expect("终止后端失败");
            println!("pg_terminate_backend({pid}) = {killed}");
            terminated = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert!(terminated, "未能捕获执行中的批次");
    handle.await.expect("任务线程异常");

    let interrupted = hub.get(&job_id).await.unwrap();
    println!(
        "中断后: status={:?} last_error={:?} checkpoint_pk={:?}",
        interrupted.status,
        interrupted.last_error,
        interrupted
            .checkpoint
            .as_ref()
            .and_then(|c| c.last_pk.clone())
    );
    assert_eq!(interrupted.status, MigrationStatus::Interrupted);
    let checkpoint = interrupted
        .checkpoint
        .clone()
        .expect("中断后缺少 checkpoint");
    let target_rows = count(&pool, &format!("SELECT COUNT(*) FROM {target}")).await;
    println!(
        "中断时目标行数={target_rows} checkpoint.written={}",
        checkpoint.written_rows
    );
    assert_eq!(target_rows % 10_000, 0, "目标行数必须整除批大小");
    assert_eq!(target_rows as u64, checkpoint.written_rows);
    assert_eq!(checkpoint.engine, "postgresql");
    assert_eq!(checkpoint.pk_kind, Some(PkKind::Integer));
    assert!(checkpoint.target_fingerprint.is_some());

    // 换一个新连接续跑。
    let resume_ctx = make_ctx(&url, &db_name, &policy).await;
    backfill::run_job(
        job_id.clone(),
        resume_ctx,
        hub.clone(),
        policy.as_ref().clone(),
    )
    .await;
    let resumed = hub.get(&job_id).await.unwrap();
    println!(
        "续跑完成: status={:?} blocking={:?} err={:?} resume_after_pk={:?} batches={}",
        resumed.status,
        resumed.blocking.as_ref().map(|b| b.phase.clone()),
        resumed.last_error,
        resumed.progress.resume_after_pk,
        resumed
            .checkpoint
            .as_ref()
            .map(|c| c.completed_batches)
            .unwrap_or(0),
    );
    assert_eq!(resumed.status, MigrationStatus::Completed);
    assert_eq!(resumed.progress.resume_after_pk, checkpoint.last_pk);
    assert_eq!(resumed.progress.success_rows, 100_000);

    let target_rows = count(&pool, &format!("SELECT COUNT(*) FROM {target}")).await;
    assert_eq!(target_rows, 100_000);
    let v2 = count(
        &pool,
        &format!(
            "SELECT COUNT(*) FROM {source} s LEFT JOIN {target} t ON t.id = s.id WHERE t.id IS NULL"
        ),
    )
    .await;
    let v3 = count(
        &pool,
        &format!(
            "SELECT COUNT(*) FROM {target} t LEFT JOIN {source} s ON s.id = t.id WHERE s.id IS NULL"
        ),
    )
    .await;
    assert_eq!([v2, v3], [0, 0], "续跑后不允许缺失/多余主键");
    let verify = resumed
        .checkpoint
        .as_ref()
        .and_then(|c| c.verify.clone())
        .expect("完成后缺少校验记录");
    assert_eq!(verify.validation_mode, "snapshot");
    assert!(verify.passed());

    drop(ctx);
    pool.close().await;
    cleanup_database(&admin_pool, &db_name).await;
}

#[tokio::test]
async fn batch_failure_rolls_back_only_current_batch() {
    let Ok(base_url) = std::env::var("FIELD_MIGRATION_PG_URL") else {
        eprintln!("跳过 S2 PG 真库验证：未设置 FIELD_MIGRATION_PG_URL");
        return;
    };
    let (db_name, admin_pool) = isolated_database(&base_url, "it_s2pg_fail").await;
    let url = with_database(&base_url, &db_name);
    let pool = PgPool::connect(&url).await.expect("连接 PG 失败");
    let source = "fm_s2pg_fail_source";
    let target = "fm_s2pg_fail_target";
    seed_jsonb(&pool, source, 30).await;
    // 目标 CHECK 让第 3 批（score 21..=30）失败。
    exec(
        &pool,
        &format!(
            "CREATE TABLE {target} (id bigint PRIMARY KEY, theme text, score bigint, note text, \
             CONSTRAINT fm_chk_score CHECK (score <= 20))"
        ),
    )
    .await;

    let policy = test_policy();
    let ctx = make_ctx(&url, &db_name, &policy).await;
    let job = manual_job(
        &db_name,
        source,
        target,
        BackfillConfig {
            batch_size: 10,
            batch_interval_ms: 0,
            max_rows_per_second: 0,
            single_transaction: false,
        },
    );
    let job_id = job.job_id.clone();
    let hub = MigrationHub::for_test();
    hub.insert(job).await.unwrap();

    let job = run_to_end(&hub, &ctx, policy.as_ref(), &job_id).await;
    println!(
        "批失败后: status={:?} last_error={:?}",
        job.status, job.last_error
    );
    assert_eq!(job.status, MigrationStatus::Interrupted);
    let last_error = job.last_error.clone().expect("缺少失败信息");
    assert!(
        last_error.contains("批次 3"),
        "错误需含批次号: {last_error}"
    );
    assert!(
        last_error.contains("(20, 30]"),
        "错误需含主键区间: {last_error}"
    );
    assert!(
        last_error.contains("已提交 20 行"),
        "错误需含已完成行数: {last_error}"
    );
    assert!(last_error.contains("rollback_scope=batch"));
    let target_rows = count(&pool, &format!("SELECT COUNT(*) FROM {target}")).await;
    assert_eq!(target_rows, 20, "只有前两批允许提交");
    let checkpoint = job.checkpoint.clone().expect("缺少 checkpoint");
    assert_eq!(checkpoint.completed_batches, 2);
    assert_eq!(checkpoint.last_pk.as_deref(), Some("20"));

    // 修复约束（CHECK 不在结构哈希内，不会触发目标漂移阻断）后续跑完成。
    exec(
        &pool,
        &format!("ALTER TABLE {target} DROP CONSTRAINT fm_chk_score"),
    )
    .await;
    let job = run_to_end(&hub, &ctx, policy.as_ref(), &job_id).await;
    assert_eq!(job.status, MigrationStatus::Completed);
    let target_rows = count(&pool, &format!("SELECT COUNT(*) FROM {target}")).await;
    assert_eq!(target_rows, 30);
    let mismatched = count(
        &pool,
        &format!(
            "SELECT COUNT(*) FROM {source} s JOIN {target} t ON t.id = s.id \
             WHERE t.score IS DISTINCT FROM CAST(s.payload ->> 'score' AS bigint)"
        ),
    )
    .await;
    assert_eq!(mismatched, 0);

    drop(ctx);
    pool.close().await;
    cleanup_database(&admin_pool, &db_name).await;
}

#[tokio::test]
async fn engine_mismatch_refuses_resume() {
    let Ok(base_url) = std::env::var("FIELD_MIGRATION_PG_URL") else {
        eprintln!("跳过 S2 PG 真库验证：未设置 FIELD_MIGRATION_PG_URL");
        return;
    };
    let (db_name, admin_pool) = isolated_database(&base_url, "it_s2pg_eng").await;
    let url = with_database(&base_url, &db_name);
    let pool = PgPool::connect(&url).await.expect("连接 PG 失败");
    let source = "fm_s2pg_eng_source";
    let target = "fm_s2pg_eng_target";
    seed_jsonb(&pool, source, 30).await;

    let policy = test_policy();
    let ctx = make_ctx(&url, &db_name, &policy).await;
    let job = manual_job(
        &db_name,
        source,
        target,
        BackfillConfig {
            batch_size: 10,
            batch_interval_ms: 0,
            max_rows_per_second: 0,
            single_transaction: false,
        },
    );
    let job_id = job.job_id.clone();
    let hub = MigrationHub::for_test();
    hub.insert(job).await.unwrap();

    // 老 checkpoint 文件没有 engine 字段，反序列化缺省为 mysql。
    let legacy: Checkpoint = serde_json::from_value(serde_json::json!({
        "completed_batches": 1,
        "last_pk": "5",
        "written_rows": 5,
        "verified": true,
        "updated_at": 0
    }))
    .expect("旧 checkpoint 反序列化失败");
    assert_eq!(legacy.engine, "mysql", "旧文件 engine 必须缺省为 mysql");
    hub.update(&job_id, |job| job.checkpoint = Some(legacy))
        .await
        .unwrap();

    let job = run_to_end(&hub, &ctx, policy.as_ref(), &job_id).await;
    println!(
        "引擎不一致: status={:?} blocking={:?}",
        job.status,
        job.blocking.as_ref().map(|b| b.phase.clone())
    );
    assert_eq!(job.status, MigrationStatus::Blocked);
    assert_eq!(
        job.blocking.as_ref().map(|b| b.phase.as_str()),
        Some("preflight:engine_mismatch")
    );
    let target_exists: bool = sqlx::query_scalar(&format!(
        "SELECT to_regclass('public.{target}') IS NOT NULL"
    ))
    .fetch_one(&pool)
    .await
    .expect("查询目标表失败");
    assert!(!target_exists, "预检阻断前不得创建/写入目标表");

    drop(ctx);
    pool.close().await;
    cleanup_database(&admin_pool, &db_name).await;
}

#[tokio::test]
async fn missing_unique_index_blocks_before_write() {
    let Ok(base_url) = std::env::var("FIELD_MIGRATION_PG_URL") else {
        eprintln!("跳过 S2 PG 真库验证：未设置 FIELD_MIGRATION_PG_URL");
        return;
    };
    let (db_name, admin_pool) = isolated_database(&base_url, "it_s2pg_nouk").await;
    let url = with_database(&base_url, &db_name);
    let pool = PgPool::connect(&url).await.expect("连接 PG 失败");
    let source = "fm_s2pg_nouk_source";
    let target = "fm_s2pg_nouk_target";
    seed_jsonb(&pool, source, 30).await;
    // 目标表无主键/唯一索引，ON CONFLICT (id) 无法工作。
    exec(
        &pool,
        &format!("CREATE TABLE {target} (id bigint, theme text, score bigint, note text)"),
    )
    .await;

    let policy = test_policy();
    let ctx = make_ctx(&url, &db_name, &policy).await;
    let job = manual_job(
        &db_name,
        source,
        target,
        BackfillConfig {
            batch_size: 10,
            batch_interval_ms: 0,
            max_rows_per_second: 0,
            single_transaction: false,
        },
    );
    let job_id = job.job_id.clone();
    let hub = MigrationHub::for_test();
    hub.insert(job).await.unwrap();

    let job = run_to_end(&hub, &ctx, policy.as_ref(), &job_id).await;
    println!(
        "缺唯一索引: status={:?} blocking={:?}",
        job.status,
        job.blocking.as_ref().map(|b| b.phase.clone())
    );
    assert_eq!(job.status, MigrationStatus::Blocked);
    assert_eq!(
        job.blocking.as_ref().map(|b| b.phase.as_str()),
        Some("preflight:on_conflict_target")
    );
    let target_rows = count(&pool, &format!("SELECT COUNT(*) FROM {target}")).await;
    assert_eq!(target_rows, 0, "阻断前不得写入目标表");

    drop(ctx);
    pool.close().await;
    cleanup_database(&admin_pool, &db_name).await;
}

#[tokio::test]
async fn unique_index_suffices_for_on_conflict() {
    let Ok(base_url) = std::env::var("FIELD_MIGRATION_PG_URL") else {
        eprintln!("跳过 S2 PG 真库验证：未设置 FIELD_MIGRATION_PG_URL");
        return;
    };
    let (db_name, admin_pool) = isolated_database(&base_url, "it_s2pg_uk").await;
    let url = with_database(&base_url, &db_name);
    let pool = PgPool::connect(&url).await.expect("连接 PG 失败");
    let source = "fm_s2pg_uk_source";
    let target = "fm_s2pg_uk_target";
    seed_jsonb(&pool, source, 1_000).await;
    // 只有 UNIQUE 索引（没有主键）也要能满足 ON CONFLICT。
    exec(
        &pool,
        &format!(
            "CREATE TABLE {target} (id bigint, theme text, score bigint, note text, \
             CONSTRAINT uk_{target}_id UNIQUE (id))"
        ),
    )
    .await;

    let policy = test_policy();
    let ctx = make_ctx(&url, &db_name, &policy).await;
    let job = manual_job(
        &db_name,
        source,
        target,
        BackfillConfig {
            batch_size: 500,
            batch_interval_ms: 0,
            max_rows_per_second: 0,
            single_transaction: false,
        },
    );
    let job_id = job.job_id.clone();
    let hub = MigrationHub::for_test();
    hub.insert(job).await.unwrap();

    let job = run_to_end(&hub, &ctx, policy.as_ref(), &job_id).await;
    println!(
        "仅 UNIQUE 索引: status={:?} last_error={:?}",
        job.status, job.last_error
    );
    assert_eq!(job.status, MigrationStatus::Completed);
    let target_rows = count(&pool, &format!("SELECT COUNT(*) FROM {target}")).await;
    assert_eq!(target_rows, 1_000);
    let verify = job
        .checkpoint
        .as_ref()
        .and_then(|c| c.verify.clone())
        .expect("完成后缺少校验记录");
    assert!(verify.passed());
    assert_eq!(verify.validation_mode, "snapshot");

    drop(ctx);
    pool.close().await;
    cleanup_database(&admin_pool, &db_name).await;
}

#[tokio::test]
async fn insert_select_order_by_limit_takes_lowest_keys() {
    let Ok(base_url) = std::env::var("FIELD_MIGRATION_PG_URL") else {
        eprintln!("跳过 S2 PG 真库验证：未设置 FIELD_MIGRATION_PG_URL");
        return;
    };
    let (db_name, admin_pool) = isolated_database(&base_url, "it_s2pg_ord").await;
    let url = with_database(&base_url, &db_name);
    let pool = PgPool::connect(&url).await.expect("连接 PG 失败");
    let source = "fm_s2pg_ord_source";
    let target = "fm_s2pg_ord_target";
    // 乱序物理插入，验证每批 ORDER BY ... LIMIT 取最小主键。
    exec(
        &pool,
        &format!("CREATE TABLE {source} (id bigint PRIMARY KEY, payload jsonb)"),
    )
    .await;
    exec(
        &pool,
        &format!(
            "INSERT INTO {source} \
             SELECT g, jsonb_build_object('theme', 'dark', 'score', g, 'note', 'n-' || g) \
             FROM (SELECT 31 - g AS g FROM generate_series(1, 30) g) reversed \
             ORDER BY (g * 7) % 30"
        ),
    )
    .await;
    exec(&pool, &format!("ANALYZE {source}")).await;

    let policy = test_policy();
    let ctx = make_ctx(&url, &db_name, &policy).await;
    let job = build_job(
        &ctx,
        policy.as_ref(),
        source,
        target,
        BackfillConfig {
            batch_size: 10,
            batch_interval_ms: 200,
            max_rows_per_second: 0,
            single_transaction: false,
        },
    )
    .await;
    let job_id = job.job_id.clone();
    let hub = MigrationHub::for_test();
    hub.insert(job).await.unwrap();

    let handle = tokio::spawn(backfill::run_job(
        job_id.clone(),
        ctx.clone(),
        hub.clone(),
        policy.as_ref().clone(),
    ));
    wait_until(
        &hub,
        &job_id,
        |job| {
            job.checkpoint
                .as_ref()
                .map(|checkpoint| checkpoint.completed_batches >= 1)
                .unwrap_or(false)
        },
        Duration::from_secs(60),
    )
    .await;
    hub.set_control(&job_id, CONTROL_PAUSE).await;
    let paused = wait_until(
        &hub,
        &job_id,
        |job| job.status == MigrationStatus::Paused,
        Duration::from_secs(60),
    )
    .await;
    let committed = paused
        .checkpoint
        .as_ref()
        .map(|c| c.completed_batches)
        .unwrap_or(0);
    assert!(committed >= 1, "暂停时至少已完成一批");
    let expected_rows = (committed * 10) as i64;
    let target_rows = count(&pool, &format!("SELECT COUNT(*) FROM {target}")).await;
    let distinct_rows = count(&pool, &format!("SELECT COUNT(DISTINCT id) FROM {target}")).await;
    let min_id = count(&pool, &format!("SELECT MIN(id) FROM {target}")).await;
    let max_id = count(&pool, &format!("SELECT MAX(id) FROM {target}")).await;
    println!(
        "暂停时: batches={committed} rows={target_rows} distinct={distinct_rows} min={min_id} max={max_id}"
    );
    assert_eq!(target_rows, expected_rows);
    assert_eq!(distinct_rows, expected_rows, "不允许重复主键");
    assert_eq!(min_id, 1);
    assert_eq!(
        max_id, expected_rows,
        "每批必须取最小主键：前 {committed} 批应恰好覆盖 1..={expected_rows}"
    );

    hub.set_control(&job_id, CONTROL_ABORT).await;
    handle.await.expect("任务线程异常");
    let aborted = hub.get(&job_id).await.unwrap();
    assert_eq!(aborted.status, MigrationStatus::Aborted);

    drop(ctx);
    pool.close().await;
    cleanup_database(&admin_pool, &db_name).await;
}

#[tokio::test]
async fn text_pk_uses_c_collation() {
    let Ok(base_url) = std::env::var("FIELD_MIGRATION_PG_URL") else {
        eprintln!("跳过 S2 PG 真库验证：未设置 FIELD_MIGRATION_PG_URL");
        return;
    };
    let (db_name, admin_pool) = isolated_database(&base_url, "it_s2pg_text").await;
    let url = with_database(&base_url, &db_name);
    let pool = PgPool::connect(&url).await.expect("连接 PG 失败");
    let source = "fm_s2pg_text_source";
    let target = "fm_s2pg_text_target";
    exec(
        &pool,
        &format!("CREATE TABLE {source} (id text PRIMARY KEY, payload jsonb)"),
    )
    .await;
    // 混合大小写：默认排序规则与 COLLATE "C" 的相对顺序不同。
    let ids = [
        "a1", "A2", "B10", "b3", "aa", "Ab", "z9", "Z1", "m5", "M5", "a0", "A0",
    ];
    for (index, id) in ids.iter().enumerate() {
        exec(
            &pool,
            &format!(
                "INSERT INTO {source} VALUES ('{id}', jsonb_build_object('theme', 'dark', 'score', {index}, 'note', 'n-{id}'))"
            ),
        )
        .await;
    }
    exec(&pool, &format!("ANALYZE {source}")).await;

    let policy = test_policy();
    let ctx = make_ctx(&url, &db_name, &policy).await;
    let job = build_job(
        &ctx,
        policy.as_ref(),
        source,
        target,
        BackfillConfig {
            batch_size: 5,
            batch_interval_ms: 0,
            max_rows_per_second: 0,
            single_transaction: false,
        },
    )
    .await;
    let job_id = job.job_id.clone();
    let hub = MigrationHub::for_test();
    hub.insert(job).await.unwrap();

    let job = run_to_end(&hub, &ctx, policy.as_ref(), &job_id).await;
    println!(
        "文本主键: status={:?} last_error={:?}",
        job.status, job.last_error
    );
    assert_eq!(job.status, MigrationStatus::Completed);
    assert_eq!(job.spec.pk_kind, Some(PkKind::Text));
    let target_rows = count(&pool, &format!("SELECT COUNT(*) FROM {target}")).await;
    let distinct_rows = count(&pool, &format!("SELECT COUNT(DISTINCT id) FROM {target}")).await;
    assert_eq!(target_rows as usize, ids.len());
    assert_eq!(distinct_rows as usize, ids.len(), "不允许重复/遗漏主键");

    // SQL 结构断言：文本主键比较/排序统一 COLLATE "C"，参数化绑定。
    let sql = pg_backfill::build_batch_sql(
        &job.spec,
        "\"public\".\"fm_s2pg_text_source\"",
        "\"public\".\"fm_s2pg_text_target\"",
        true,
    )
    .expect("生成批次 SQL 失败");
    println!("批次 SQL: {sql}");
    assert!(
        sql.contains("COLLATE \"C\""),
        "文本主键必须 COLLATE \"C\": {sql}"
    );
    assert!(sql.contains("$1::text"), "主键必须参数化绑定: {sql}");
    assert!(
        sql.contains("ON CONFLICT (\"id\") DO UPDATE"),
        "必须带 ON CONFLICT: {sql}"
    );

    drop(ctx);
    pool.close().await;
    cleanup_database(&admin_pool, &db_name).await;
}

#[tokio::test]
async fn numeric_pk_pagination() {
    let Ok(base_url) = std::env::var("FIELD_MIGRATION_PG_URL") else {
        eprintln!("跳过 S2 PG 真库验证：未设置 FIELD_MIGRATION_PG_URL");
        return;
    };
    let (db_name, admin_pool) = isolated_database(&base_url, "it_s2pg_num").await;
    let url = with_database(&base_url, &db_name);
    let pool = PgPool::connect(&url).await.expect("连接 PG 失败");
    let source = "fm_s2pg_num_source";
    let target = "fm_s2pg_num_target";
    exec(
        &pool,
        &format!("CREATE TABLE {source} (id numeric(20,4) PRIMARY KEY, payload jsonb)"),
    )
    .await;
    exec(
        &pool,
        &format!(
            "INSERT INTO {source} \
             SELECT (g * 1.25)::numeric(20,4), jsonb_build_object('theme', 'dark', 'score', g, 'note', 'n-' || g) \
             FROM generate_series(1, 25) AS g"
        ),
    )
    .await;
    exec(&pool, &format!("ANALYZE {source}")).await;

    let policy = test_policy();
    let ctx = make_ctx(&url, &db_name, &policy).await;
    let job = build_job(
        &ctx,
        policy.as_ref(),
        source,
        target,
        BackfillConfig {
            batch_size: 10,
            batch_interval_ms: 0,
            max_rows_per_second: 0,
            single_transaction: false,
        },
    )
    .await;
    let job_id = job.job_id.clone();
    let hub = MigrationHub::for_test();
    hub.insert(job).await.unwrap();

    let job = run_to_end(&hub, &ctx, policy.as_ref(), &job_id).await;
    println!(
        "numeric 主键: status={:?} last_error={:?}",
        job.status, job.last_error
    );
    assert_eq!(job.status, MigrationStatus::Completed);
    assert_eq!(job.spec.pk_kind, Some(PkKind::Numeric));
    let checkpoint = job.checkpoint.clone().expect("缺少 checkpoint");
    assert_eq!(checkpoint.pk_kind, Some(PkKind::Numeric));
    let target_rows = count(&pool, &format!("SELECT COUNT(*) FROM {target}")).await;
    assert_eq!(target_rows, 25);

    drop(ctx);
    pool.close().await;
    cleanup_database(&admin_pool, &db_name).await;
}

#[tokio::test]
async fn uuid_pk_pagination() {
    let Ok(base_url) = std::env::var("FIELD_MIGRATION_PG_URL") else {
        eprintln!("跳过 S2 PG 真库验证：未设置 FIELD_MIGRATION_PG_URL");
        return;
    };
    let (db_name, admin_pool) = isolated_database(&base_url, "it_s2pg_uuid").await;
    let url = with_database(&base_url, &db_name);
    let pool = PgPool::connect(&url).await.expect("连接 PG 失败");
    let source = "fm_s2pg_uuid_source";
    let target = "fm_s2pg_uuid_target";
    exec(
        &pool,
        &format!("CREATE TABLE {source} (id uuid PRIMARY KEY, payload jsonb)"),
    )
    .await;
    exec(
        &pool,
        &format!(
            "INSERT INTO {source} \
             SELECT gen_random_uuid(), jsonb_build_object('theme', 'dark', 'score', g, 'note', 'n-' || g) \
             FROM generate_series(1, 25) AS g"
        ),
    )
    .await;
    exec(&pool, &format!("ANALYZE {source}")).await;

    let policy = test_policy();
    let ctx = make_ctx(&url, &db_name, &policy).await;
    let job = build_job(
        &ctx,
        policy.as_ref(),
        source,
        target,
        BackfillConfig {
            batch_size: 10,
            batch_interval_ms: 0,
            max_rows_per_second: 0,
            single_transaction: false,
        },
    )
    .await;
    let job_id = job.job_id.clone();
    let hub = MigrationHub::for_test();
    hub.insert(job).await.unwrap();

    let job = run_to_end(&hub, &ctx, policy.as_ref(), &job_id).await;
    println!(
        "uuid 主键: status={:?} last_error={:?}",
        job.status, job.last_error
    );
    assert_eq!(job.status, MigrationStatus::Completed);
    assert_eq!(job.spec.pk_kind, Some(PkKind::Uuid));
    let checkpoint = job.checkpoint.clone().expect("缺少 checkpoint");
    assert_eq!(checkpoint.pk_kind, Some(PkKind::Uuid));
    let last_pk = checkpoint.last_pk.clone().expect("缺少边界主键");
    assert_eq!(last_pk.len(), 36, "uuid 文本应为规范形式: {last_pk}");
    assert_eq!(
        last_pk,
        last_pk.to_ascii_lowercase(),
        "uuid 应为小写: {last_pk}"
    );
    let target_rows = count(&pool, &format!("SELECT COUNT(*) FROM {target}")).await;
    assert_eq!(target_rows, 25);

    drop(ctx);
    pool.close().await;
    cleanup_database(&admin_pool, &db_name).await;
}

#[tokio::test]
async fn rate_limit_and_timeout_mapping() {
    // SET LOCAL 映射是纯函数断言，不依赖数据库。
    let mut policy = OpsPolicy::default();
    policy.lock_wait_timeout_secs = 7;
    let statements = pg_backfill::pg_timeout_statements(&policy);
    assert_eq!(
        statements,
        vec![
            "SET LOCAL lock_timeout = '7s'".to_string(),
            "SET LOCAL statement_timeout = '0'".to_string(),
            "SET LOCAL idle_in_transaction_session_timeout = '60s'".to_string(),
        ],
        "SET LOCAL 超时映射必须固定为三条且取自 policy"
    );

    let Ok(base_url) = std::env::var("FIELD_MIGRATION_PG_URL") else {
        eprintln!("跳过限速真库验证：未设置 FIELD_MIGRATION_PG_URL");
        return;
    };
    let (db_name, admin_pool) = isolated_database(&base_url, "it_s2pg_rate").await;
    let url = with_database(&base_url, &db_name);
    let pool = PgPool::connect(&url).await.expect("连接 PG 失败");
    let source = "fm_s2pg_rate_source";
    let target = "fm_s2pg_rate_target";
    seed_jsonb(&pool, source, 4_000).await;

    let policy = test_policy();
    let ctx = make_ctx(&url, &db_name, &policy).await;
    let job = build_job(
        &ctx,
        policy.as_ref(),
        source,
        target,
        BackfillConfig {
            batch_size: 1_000,
            batch_interval_ms: 0,
            max_rows_per_second: 2_000,
            single_transaction: false,
        },
    )
    .await;
    let job_id = job.job_id.clone();
    let hub = MigrationHub::for_test();
    hub.insert(job).await.unwrap();

    let started = Instant::now();
    let job = run_to_end(&hub, &ctx, policy.as_ref(), &job_id).await;
    let elapsed = started.elapsed();
    println!(
        "限速回填: status={:?} elapsed={}ms",
        job.status,
        elapsed.as_millis()
    );
    assert_eq!(job.status, MigrationStatus::Completed);
    // 4000 行 / 2000 行每秒 ≈ 2s 下限；留实现与调度余量。
    assert!(
        elapsed >= Duration::from_millis(1_500),
        "限速未生效：{}ms 完成 4000 行",
        elapsed.as_millis()
    );

    drop(ctx);
    pool.close().await;
    cleanup_database(&admin_pool, &db_name).await;
}

#[tokio::test]
async fn single_transaction_failure_rolls_back_job() {
    let Ok(base_url) = std::env::var("FIELD_MIGRATION_PG_URL") else {
        eprintln!("跳过 S2 PG 真库验证：未设置 FIELD_MIGRATION_PG_URL");
        return;
    };
    let (db_name, admin_pool) = isolated_database(&base_url, "it_s2pg_single").await;
    let url = with_database(&base_url, &db_name);
    let pool = PgPool::connect(&url).await.expect("连接 PG 失败");
    let source = "fm_s2pg_single_source";
    let target = "fm_s2pg_single_target";
    seed_jsonb(&pool, source, 30).await;
    exec(
        &pool,
        &format!(
            "CREATE TABLE {target} (id bigint PRIMARY KEY, theme text, score bigint, note text, \
             CONSTRAINT fm_chk_score CHECK (score <= 20))"
        ),
    )
    .await;

    let policy = test_policy();
    let ctx = make_ctx(&url, &db_name, &policy).await;
    let job = manual_job(
        &db_name,
        source,
        target,
        BackfillConfig {
            batch_size: 10,
            batch_interval_ms: 0,
            max_rows_per_second: 0,
            single_transaction: true,
        },
    );
    let job_id = job.job_id.clone();
    let hub = MigrationHub::for_test();
    hub.insert(job).await.unwrap();

    let job = run_to_end(&hub, &ctx, policy.as_ref(), &job_id).await;
    println!(
        "单事务失败后: status={:?} last_error={:?}",
        job.status, job.last_error
    );
    assert_eq!(job.status, MigrationStatus::Interrupted);
    let last_error = job.last_error.clone().expect("缺少失败信息");
    assert!(
        last_error.contains("rollback_scope=job"),
        "错误需含回滚范围: {last_error}"
    );
    assert!(
        last_error.contains("批次 3"),
        "错误需含批次号: {last_error}"
    );
    let target_rows = count(&pool, &format!("SELECT COUNT(*) FROM {target}")).await;
    assert_eq!(target_rows, 0, "单事务失败必须整体回滚");
    assert!(job.checkpoint.is_none(), "单事务失败不得留下 checkpoint");
    let execution = job.execution.clone().expect("缺少 execution 语义");
    assert!(execution.atomic);
    assert_eq!(execution.rollback_scope, "job");
    assert!(!execution.resumable);
    assert_eq!(execution.validation_mode, "snapshot");

    // 修复约束后也不允许续跑：单事务模式失败后必须重建任务。
    exec(
        &pool,
        &format!("ALTER TABLE {target} DROP CONSTRAINT fm_chk_score"),
    )
    .await;
    let job = run_to_end(&hub, &ctx, policy.as_ref(), &job_id).await;
    println!(
        "单事务续跑: status={:?} blocking={:?}",
        job.status,
        job.blocking.as_ref().map(|b| b.phase.clone())
    );
    assert_eq!(job.status, MigrationStatus::Blocked);
    assert_eq!(
        job.blocking.as_ref().map(|b| b.phase.as_str()),
        Some("preflight:not_resumable")
    );
    let target_rows = count(&pool, &format!("SELECT COUNT(*) FROM {target}")).await;
    assert_eq!(target_rows, 0, "拒绝续跑后不得写入目标表");

    drop(ctx);
    pool.close().await;
    cleanup_database(&admin_pool, &db_name).await;
}

// ── S5：复合主键 / 分区源 / 部分唯一索引 ─────────────────────────

/// 复合主键端到端：S1 脚本建多列主键目标表，执行器走 tuple keyset 分批。
/// 依据 specs/pg-field-governance-s5/spec.md §5（行比较 keyset + 复合 ON CONFLICT）。
#[tokio::test]
async fn composite_pk_backfill() {
    let Ok(base_url) = std::env::var("FIELD_MIGRATION_PG_URL") else {
        eprintln!("跳过 S5 PG 真库验证：未设置 FIELD_MIGRATION_PG_URL");
        return;
    };
    let (db_name, admin_pool) = isolated_database(&base_url, "it_s5_cpk").await;
    let url = with_database(&base_url, &db_name);
    let pool = PgPool::connect(&url).await.expect("连接 PG 失败");
    let source = "fm_s5_cpk_source";
    let target = "fm_s5_cpk_target";
    exec(
        &pool,
        &format!(
            "CREATE TABLE {source} (tenant_id bigint NOT NULL, seq integer NOT NULL, \
             payload jsonb, PRIMARY KEY (tenant_id, seq))"
        ),
    )
    .await;
    // 故意让 tenant_id 与 seq 交错，验证 keyset 按 (tenant_id, seq) 字典序而不是 seq 单列。
    exec(
        &pool,
        &format!(
            "INSERT INTO {source} \
             SELECT (g % 7) + 1, g, \
                    jsonb_build_object('theme', 'dark', 'score', g, 'note', 'n-' || g) \
             FROM generate_series(1, 5000) AS g"
        ),
    )
    .await;
    exec(&pool, &format!("ANALYZE {source}")).await;

    let policy = test_policy();
    let ctx = make_ctx(&url, &db_name, &policy).await;
    let job = build_job(
        &ctx,
        policy.as_ref(),
        source,
        target,
        BackfillConfig {
            batch_size: 700,
            batch_interval_ms: 0,
            max_rows_per_second: 0,
            single_transaction: false,
        },
    )
    .await;
    assert_eq!(
        job.spec.pk_columns,
        vec!["tenant_id".to_string(), "seq".to_string()],
        "复合主键必须写入 spec.pk_columns"
    );
    assert_eq!(job.spec.pk_column, "tenant_id", "首列作为展示用主键名");
    let create_sql = job
        .spec
        .create_table_sql
        .as_deref()
        .expect("PG 任务必须带建表脚本");
    assert!(
        create_sql.contains("PRIMARY KEY (\"tenant_id\", \"seq\")"),
        "建表脚本必须是多列主键: {create_sql}"
    );

    let job_id = job.job_id.clone();
    let hub = MigrationHub::for_test();
    hub.insert(job).await.unwrap();

    let job = run_to_end(&hub, &ctx, policy.as_ref(), &job_id).await;
    println!(
        "复合主键回填: status={:?} last_error={:?} batches={}",
        job.status,
        job.last_error,
        job.checkpoint
            .as_ref()
            .map(|c| c.completed_batches)
            .unwrap_or(0)
    );
    assert_eq!(job.status, MigrationStatus::Completed);
    let target_rows = count(&pool, &format!("SELECT COUNT(*) FROM {target}")).await;
    assert_eq!(target_rows, 5_000, "目标表行数必须与源表一致");
    let checkpoint = job.checkpoint.as_ref().expect("完成后必须有 checkpoint");
    let last_pks = checkpoint
        .last_pks
        .as_ref()
        .expect("复合主键必须记 last_pks");
    assert_eq!(last_pks.len(), 2, "last_pks 必须等长于主键列");
    assert_eq!(checkpoint.pk_kinds.len(), 2, "checkpoint 必须记两列类型");
    let verify = checkpoint.verify.clone().expect("完成后缺少校验记录");
    assert!(verify.passed(), "V1–V4 必须全部通过: {verify:?}");

    drop(ctx);
    pool.close().await;
    cleanup_database(&admin_pool, &db_name).await;
}

/// 复合主键含文本列：行比较内文本列统一 `COLLATE "C"`，与 ORDER BY 一致。
/// 依据 specs/pg-field-governance-s5/spec.md §5.2。
#[tokio::test]
async fn composite_pk_text_collate() {
    let Ok(base_url) = std::env::var("FIELD_MIGRATION_PG_URL") else {
        eprintln!("跳过 S5 PG 真库验证：未设置 FIELD_MIGRATION_PG_URL");
        return;
    };
    let (db_name, admin_pool) = isolated_database(&base_url, "it_s5_cpkt").await;
    let url = with_database(&base_url, &db_name);
    let pool = PgPool::connect(&url).await.expect("连接 PG 失败");
    let source = "fm_s5_cpkt_source";
    let target = "fm_s5_cpkt_target";
    exec(
        &pool,
        &format!(
            "CREATE TABLE {source} (region text COLLATE \"C\" NOT NULL, seq integer NOT NULL, \
             payload jsonb, PRIMARY KEY (region, seq))"
        ),
    )
    .await;
    exec(
        &pool,
        &format!(
            "INSERT INTO {source} \
             SELECT 'r-' || lpad(((g % 5) + 1)::text, 2, '0'), g, \
                    jsonb_build_object('theme', 'light', 'score', g, 'note', 'n-' || g) \
             FROM generate_series(1, 2000) AS g"
        ),
    )
    .await;
    exec(&pool, &format!("ANALYZE {source}")).await;

    let policy = test_policy();
    let ctx = make_ctx(&url, &db_name, &policy).await;
    let job = build_job(
        &ctx,
        policy.as_ref(),
        source,
        target,
        BackfillConfig {
            batch_size: 300,
            batch_interval_ms: 0,
            max_rows_per_second: 0,
            single_transaction: false,
        },
    )
    .await;
    let job_id = job.job_id.clone();
    let hub = MigrationHub::for_test();
    hub.insert(job).await.unwrap();

    let job = run_to_end(&hub, &ctx, policy.as_ref(), &job_id).await;
    println!(
        "复合文本主键回填: status={:?} last_error={:?}",
        job.status, job.last_error
    );
    assert_eq!(job.status, MigrationStatus::Completed);
    let target_rows = count(&pool, &format!("SELECT COUNT(*) FROM {target}")).await;
    assert_eq!(target_rows, 2_000);
    let verify = job
        .checkpoint
        .as_ref()
        .and_then(|c| c.verify.clone())
        .expect("完成后缺少校验记录");
    assert!(
        verify.passed(),
        "文本列必须按 COLLATE \"C\" 比较: {verify:?}"
    );

    drop(ctx);
    pool.close().await;
    cleanup_database(&admin_pool, &db_name).await;
}

/// 源表为分区表：S5 起经父表 keyset 透明分页，风险由 Block 降为 High。
/// 依据 specs/pg-field-governance-s5/spec.md §7.1。
#[tokio::test]
async fn partitioned_source_backfill() {
    let Ok(base_url) = std::env::var("FIELD_MIGRATION_PG_URL") else {
        eprintln!("跳过 S5 PG 真库验证：未设置 FIELD_MIGRATION_PG_URL");
        return;
    };
    let (db_name, admin_pool) = isolated_database(&base_url, "it_s5_psrc").await;
    let url = with_database(&base_url, &db_name);
    let pool = PgPool::connect(&url).await.expect("连接 PG 失败");
    let source = "fm_s5_psrc_source";
    let target = "fm_s5_psrc_target";
    exec(
        &pool,
        &format!(
            "CREATE TABLE {source} (id bigint NOT NULL, payload jsonb, PRIMARY KEY (id)) \
             PARTITION BY RANGE (id)"
        ),
    )
    .await;
    exec(
        &pool,
        &format!("CREATE TABLE {source}_p1 PARTITION OF {source} FOR VALUES FROM (0) TO (2000)"),
    )
    .await;
    exec(
        &pool,
        &format!("CREATE TABLE {source}_p2 PARTITION OF {source} FOR VALUES FROM (2000) TO (9000)"),
    )
    .await;
    exec(
        &pool,
        &format!(
            "INSERT INTO {source} \
             SELECT g, jsonb_build_object('theme', 'dark', 'score', g, 'note', 'n-' || g) \
             FROM generate_series(1, 4000) AS g"
        ),
    )
    .await;
    exec(&pool, &format!("ANALYZE {source}")).await;

    let policy = test_policy();
    let ctx = make_ctx(&url, &db_name, &policy).await;
    let job = build_job(
        &ctx,
        policy.as_ref(),
        source,
        target,
        BackfillConfig {
            batch_size: 500,
            batch_interval_ms: 0,
            max_rows_per_second: 0,
            single_transaction: false,
        },
    )
    .await;
    let job_id = job.job_id.clone();
    let hub = MigrationHub::for_test();
    hub.insert(job).await.unwrap();

    let job = run_to_end(&hub, &ctx, policy.as_ref(), &job_id).await;
    println!(
        "分区源回填: status={:?} last_error={:?}",
        job.status, job.last_error
    );
    assert_eq!(job.status, MigrationStatus::Completed);
    let target_rows = count(&pool, &format!("SELECT COUNT(*) FROM {target}")).await;
    assert_eq!(target_rows, 4_000, "分区源必须被完整覆盖");
    let verify = job
        .checkpoint
        .as_ref()
        .and_then(|c| c.verify.clone())
        .expect("完成后缺少校验记录");
    assert!(verify.passed(), "分区源校验必须通过: {verify:?}");

    drop(ctx);
    pool.close().await;
    cleanup_database(&admin_pool, &db_name).await;
}

/// 复合主键中断续跑：tuple checkpoint 必须让后续批次严格接上，不重不漏。
/// 中断用 CHECK 约束确定性制造（第 3 批失败 → Interrupted 可续跑），
/// 不依赖「抢在任务完成前 kill 连接」的时序，避免并发测试下 flaky。
/// 依据 specs/pg-field-governance-s5/spec.md §5.3。
#[tokio::test]
async fn composite_pk_resume() {
    let Ok(base_url) = std::env::var("FIELD_MIGRATION_PG_URL") else {
        eprintln!("跳过 S5 PG 真库验证：未设置 FIELD_MIGRATION_PG_URL");
        return;
    };
    let (db_name, admin_pool) = isolated_database(&base_url, "it_s5_cpkr").await;
    let url = with_database(&base_url, &db_name);
    let pool = PgPool::connect(&url).await.expect("连接 PG 失败");
    let source = "fm_s5_cpkr_source";
    let target = "fm_s5_cpkr_target";
    exec(
        &pool,
        &format!(
            "CREATE TABLE {source} (tenant_id bigint NOT NULL, seq integer NOT NULL, \
             payload jsonb, PRIMARY KEY (tenant_id, seq))"
        ),
    )
    .await;
    exec(
        &pool,
        &format!(
            "INSERT INTO {source} \
             SELECT ((g - 1) / 1000) + 1, g, \
                    jsonb_build_object('theme', 'dark', 'score', g, 'note', 'n-' || g) \
             FROM generate_series(1, 2000) AS g"
        ),
    )
    .await;
    // 目标表预建：复合主键（ON CONFLICT 目标）+ 一条会在第 3 批（seq 601–900）触发的 CHECK。
    exec(
        &pool,
        &format!(
            "CREATE TABLE {target} (tenant_id bigint NOT NULL, seq integer NOT NULL, \
             theme text, score bigint, note text, PRIMARY KEY (tenant_id, seq), \
             CONSTRAINT fm_chk_seq CHECK (seq < 900))"
        ),
    )
    .await;

    let policy = test_policy();
    let ctx = make_ctx(&url, &db_name, &policy).await;
    let job = manual_job_with_pk(
        &db_name,
        source,
        target,
        vec!["tenant_id".into(), "seq".into()],
        BackfillConfig {
            batch_size: 300,
            batch_interval_ms: 0,
            max_rows_per_second: 0,
            single_transaction: false,
        },
    );
    let job_id = job.job_id.clone();
    let hub = MigrationHub::for_test();
    hub.insert(job).await.unwrap();

    let failed = run_to_end(&hub, &ctx, policy.as_ref(), &job_id).await;
    println!(
        "复合主键中断: status={:?} last_error={:?}",
        failed.status, failed.last_error
    );
    assert_eq!(failed.status, MigrationStatus::Interrupted);
    let checkpoint = failed.checkpoint.clone().expect("中断后缺少 checkpoint");
    let last_pks = checkpoint
        .last_pks
        .clone()
        .expect("复合主键中断后必须记 last_pks");
    assert_eq!(last_pks.len(), 2, "last_pks 必须两列");
    // 租户分段与 seq 单调对齐：排序后第 2 批结束于 (tenant_id=1, seq=600)。
    assert_eq!(last_pks, vec!["1".to_string(), "600".to_string()]);
    assert_eq!(checkpoint.pk_kinds.len(), 2, "checkpoint.pk_kinds 必须两列");
    let written_before = count(&pool, &format!("SELECT COUNT(*) FROM {target}")).await;
    assert_eq!(written_before, 600, "失败批必须整批回滚");
    assert_eq!(written_before as u64, checkpoint.written_rows);

    // 删除约束后换新连接续跑。
    exec(
        &pool,
        &format!("ALTER TABLE {target} DROP CONSTRAINT fm_chk_seq"),
    )
    .await;
    let resume_ctx = make_ctx(&url, &db_name, &policy).await;
    let job = run_to_end(&hub, &resume_ctx, policy.as_ref(), &job_id).await;
    println!(
        "复合主键续跑: status={:?} last_error={:?} batches={}",
        job.status,
        job.last_error,
        job.checkpoint
            .as_ref()
            .map(|c| c.completed_batches)
            .unwrap_or(0)
    );
    assert_eq!(job.status, MigrationStatus::Completed);
    let target_rows = count(&pool, &format!("SELECT COUNT(*) FROM {target}")).await;
    assert_eq!(target_rows, 2_000, "续跑必须不重不漏");
    let verify = job
        .checkpoint
        .as_ref()
        .and_then(|c| c.verify.clone())
        .expect("完成后缺少校验记录");
    assert!(verify.passed(), "续跑后 V1–V4 必须通过: {verify:?}");

    drop(ctx);
    drop(resume_ctx);
    pool.close().await;
    cleanup_database(&admin_pool, &db_name).await;
}

/// 部分唯一索引作为 ON CONFLICT 目标：谓词必须原样进入 `ON CONFLICT ... WHERE`。
/// 依据 specs/pg-field-governance-s5/spec.md §6。
#[tokio::test]
async fn partial_unique_index_conflict_target() {
    let Ok(base_url) = std::env::var("FIELD_MIGRATION_PG_URL") else {
        eprintln!("跳过 S5 PG 真库验证：未设置 FIELD_MIGRATION_PG_URL");
        return;
    };
    let (db_name, admin_pool) = isolated_database(&base_url, "it_s5_pidx").await;
    let url = with_database(&base_url, &db_name);
    let pool = PgPool::connect(&url).await.expect("连接 PG 失败");
    let source = "fm_s5_pidx_source";
    let target = "fm_s5_pidx_target";
    seed_jsonb(&pool, source, 1_000).await;
    // 目标表没有主键，只有部分唯一索引（谓词 id > 0 覆盖全部数据）。
    exec(
        &pool,
        &format!("CREATE TABLE {target} (id bigint, theme text, score bigint, note text)"),
    )
    .await;
    exec(
        &pool,
        &format!("CREATE UNIQUE INDEX uk_{target}_id ON {target} (id) WHERE id > 0"),
    )
    .await;

    let policy = test_policy();
    let ctx = make_ctx(&url, &db_name, &policy).await;
    let job = manual_job(
        &db_name,
        source,
        target,
        BackfillConfig {
            batch_size: 300,
            batch_interval_ms: 0,
            max_rows_per_second: 0,
            single_transaction: false,
        },
    );
    let job_id = job.job_id.clone();
    let hub = MigrationHub::for_test();
    hub.insert(job).await.unwrap();

    let job = run_to_end(&hub, &ctx, policy.as_ref(), &job_id).await;
    println!(
        "部分唯一索引: status={:?} last_error={:?}",
        job.status, job.last_error
    );
    assert_eq!(
        job.status,
        MigrationStatus::Completed,
        "部分唯一索引必须能作为冲突目标: {:?}",
        job.last_error
    );
    let target_rows = count(&pool, &format!("SELECT COUNT(*) FROM {target}")).await;
    assert_eq!(target_rows, 1_000);
    let verify = job
        .checkpoint
        .as_ref()
        .and_then(|c| c.verify.clone())
        .expect("完成后缺少校验记录");
    assert!(verify.passed(), "部分索引路径仍须通过 V1–V4: {verify:?}");
    // 若没有把部分索引的谓词带进 `ON CONFLICT ... WHERE`，PG 会报 42P10
    // （no unique or exclusion constraint matching），任务根本到不了 Completed。

    drop(ctx);
    pool.close().await;
    cleanup_database(&admin_pool, &db_name).await;
}

/// 目标表为分区表：PG 全版本不支持分区表 `ON CONFLICT DO UPDATE`，必须 BLOCK。
/// 依据 specs/pg-field-governance-s5/spec.md §7.1（已知缺口，维持手工路径）。
#[tokio::test]
async fn partitioned_target_blocked() {
    let Ok(base_url) = std::env::var("FIELD_MIGRATION_PG_URL") else {
        eprintln!("跳过 S5 PG 真库验证：未设置 FIELD_MIGRATION_PG_URL");
        return;
    };
    let (db_name, admin_pool) = isolated_database(&base_url, "it_s5_ptgt").await;
    let url = with_database(&base_url, &db_name);
    let pool = PgPool::connect(&url).await.expect("连接 PG 失败");
    let source = "fm_s5_ptgt_source";
    let target = "fm_s5_ptgt_target";
    seed_jsonb(&pool, source, 500).await;
    exec(
        &pool,
        &format!(
            "CREATE TABLE {target} (id bigint NOT NULL, theme text, score bigint, note text, \
             PRIMARY KEY (id)) PARTITION BY RANGE (id)"
        ),
    )
    .await;
    exec(
        &pool,
        &format!("CREATE TABLE {target}_p1 PARTITION OF {target} FOR VALUES FROM (0) TO (1000)"),
    )
    .await;

    let policy = test_policy();
    let ctx = make_ctx(&url, &db_name, &policy).await;
    let job = manual_job(
        &db_name,
        source,
        target,
        BackfillConfig {
            batch_size: 100,
            batch_interval_ms: 0,
            max_rows_per_second: 0,
            single_transaction: false,
        },
    );
    let job_id = job.job_id.clone();
    let hub = MigrationHub::for_test();
    hub.insert(job).await.unwrap();

    let job = run_to_end(&hub, &ctx, policy.as_ref(), &job_id).await;
    println!(
        "分区目标: status={:?} blocking={:?}",
        job.status,
        job.blocking.as_ref().map(|b| b.phase.clone())
    );
    assert_eq!(job.status, MigrationStatus::Blocked);
    assert_eq!(
        job.blocking.as_ref().map(|b| b.phase.as_str()),
        Some("preflight:partitioned_target")
    );
    let target_rows = count(&pool, &format!("SELECT COUNT(*) FROM {target}")).await;
    assert_eq!(target_rows, 0, "阻断后不得写入分区目标表");

    drop(ctx);
    pool.close().await;
    cleanup_database(&admin_pool, &db_name).await;
}
