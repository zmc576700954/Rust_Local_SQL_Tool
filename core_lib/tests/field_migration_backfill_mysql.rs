//! 字段回填 S2 真实 MySQL 验收（opt-in）。
//!
//! 运行：
//! ```bash
//! FIELD_MIGRATION_MYSQL_URL='mysql://root:root@127.0.0.1:3306/s2_e2e' \
//!   cargo test -p core_lib --test field_migration_backfill_mysql -- --nocapture --test-threads=1
//! ```
//! 未设置环境变量时自动跳过。
//!
//! 覆盖：
//! 1) 10 万行完整回填后 V1-V4 = 0；
//! 2) 中途 KILL CONNECTION 强制中断后从 checkpoint 续跑，结果一致且无重复行；
//! 3) 已完成批次目标数据被改坏后重新执行，停在该批并给出差异主键；
//! 4) 行/秒限速生效 + 暂停/继续/中止。

use std::sync::Arc;
use std::time::{Duration, Instant};

use sqlx::mysql::MySqlPool;
use sqlx::MySqlConnection;

use core_lib::db::client::DbClient;
use core_lib::ops::audit::AuditLog;
use core_lib::ops::field_migration::backfill;
use core_lib::ops::field_migration::migration_store::{
    BackfillConfig, MigrationHub, MigrationJob, MigrationStatus, CONTROL_ABORT, CONTROL_PAUSE,
    CONTROL_RUN,
};
use core_lib::ops::field_migration::types::{
    ColumnMapping, EmptyStringPolicy, FieldGovernanceAnalysis, FieldGovernanceInput, TargetSpec,
};
use core_lib::ops::policy::OpsPolicy;
use core_lib::ops::registry::{OpsContext, ScenarioRegistry};
use core_lib::sql::util::escape_sql_string;

const DB_ID: &str = "field-migration-s2-e2e";

fn db_name(url: &str) -> String {
    DbClient::extract_db_name(url).unwrap_or_else(|| "s2_e2e".to_string())
}

async fn exec(conn: &mut MySqlConnection, sql: &str) {
    if let Err(err) = sqlx::query(sql).execute(&mut *conn).await {
        panic!("执行 SQL 失败: {err}\nSQL: {sql}");
    }
}

async fn seed(conn: &mut MySqlConnection, source: &str, rows: i64) {
    exec(conn, &format!("DROP TABLE IF EXISTS {source}")).await;
    exec(
        conn,
        &format!(
            "CREATE TABLE {source} (\
               id BIGINT NOT NULL, \
               payload JSON NULL, \
               PRIMARY KEY (id)\
             ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4"
        ),
    )
    .await;
    let mut values = Vec::with_capacity(500);
    for id in 1..=rows {
        let payload = format!(
            r#"{{"theme":"dark","score":7,"note":"n-{id}"}}"#
        );
        values.push(format!("({id}, '{}')", escape_sql_string(&payload)));
        if values.len() == 500 {
            let sql = format!(
                "INSERT INTO {source} (id, payload) VALUES {}",
                values.join(", ")
            );
            exec(conn, &sql).await;
            values.clear();
        }
    }
    if !values.is_empty() {
        let sql = format!(
            "INSERT INTO {source} (id, payload) VALUES {}",
            values.join(", ")
        );
        exec(conn, &sql).await;
    }
    exec(conn, &format!("ANALYZE TABLE {source}")).await;
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
                    target_type: Some("VARCHAR(64)".into()),
                    required: Some(false),
                },
                ColumnMapping {
                    source_key: Some("score".into()),
                    target_column: "score".into(),
                    target_type: Some("INT".into()),
                    required: Some(false),
                },
                ColumnMapping {
                    source_key: Some("note".into()),
                    target_column: "note".into(),
                    target_type: Some("VARCHAR(128)".into()),
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

/// 走与 web-server 相同的链路：分析 → 计划 → prepare → job。
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
    let analysis: FieldGovernanceAnalysis = serde_json::from_value(
        evidence
            .extra
            .clone()
            .expect("缺少字段治理扩展证据"),
    )
    .expect("解析字段治理证据失败");
    if analysis.risks.iter().any(|risk| {
        risk.level == core_lib::ops::field_migration::types::RiskLevel::Block
    }) {
        panic!(
            "seed 数据不应出现阻断风险: {:?}",
            analysis
                .risks
                .iter()
                .map(|risk| (&risk.code, risk.level))
                .collect::<Vec<_>>()
        );
    }
    if let Some(gap) = analysis.gaps.first() {
        panic!("seed 数据不应出现证据缺口: {:?}", gap);
    }
    let plan = scenario
        .build_plan(&ctx.identity(), &evidence, &input_value, policy)
        .expect("生成计划失败");
    let prepared = backfill::prepare(&plan, &analysis, policy, config).expect("prepare 失败");
    backfill::new_job(&plan, &prepared, false)
}

async fn run_to_end(hub: &MigrationHub, ctx: &OpsContext, policy: &OpsPolicy, job_id: &str) -> MigrationJob {
    backfill::run_job(job_id.to_string(), ctx.clone(), hub.clone(), policy.clone()).await;
    hub.get(job_id).await.expect("任务丢失")
}

async fn wait_until<F>(hub: &MigrationHub, job_id: &str, mut predicate: F, timeout: Duration) -> MigrationJob
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
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn count(pool: &MySqlPool, sql: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(sql)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|err| panic!("查询失败: {err}\nSQL: {sql}"))
}

async fn verify_v(pool: &MySqlPool, source: &str, target: &str) -> [i64; 4] {
    let v1_src = count(pool, &format!("SELECT COUNT(*) FROM {source}")).await;
    let v1_tgt = count(pool, &format!("SELECT COUNT(*) FROM {target}")).await;
    let v2 = count(
        pool,
        &format!(
            "SELECT COUNT(*) FROM {source} s LEFT JOIN {target} t ON t.id = s.id WHERE t.id IS NULL"
        ),
    )
    .await;
    let v3 = count(
        pool,
        &format!(
            "SELECT COUNT(*) FROM {target} t LEFT JOIN {source} s ON s.id = t.id WHERE s.id IS NULL"
        ),
    )
    .await;
    let v4 = count(
        pool,
        &format!(
            "SELECT COUNT(*) FROM {source} s JOIN {target} t ON t.id = s.id \
             WHERE NOT (t.theme <=> JSON_UNQUOTE(JSON_EXTRACT(s.payload, '$.theme'))) \
                OR NOT (t.score <=> CAST(JSON_UNQUOTE(JSON_EXTRACT(s.payload, '$.score')) AS SIGNED)) \
                OR NOT (t.note <=> JSON_UNQUOTE(JSON_EXTRACT(s.payload, '$.note')))"
        ),
    )
    .await;
    println!("V1: source={v1_src} target={v1_tgt} · V2={v2} V3={v3} V4={v4}");
    assert_eq!(v1_src, v1_tgt, "V1 行数不一致");
    [v1_src, v2, v3, v4]
}

#[tokio::test]
async fn backfill_100k_full_roundtrip_v1_v4_zero() {
    let Ok(url) = std::env::var("FIELD_MIGRATION_MYSQL_URL") else {
        eprintln!("跳过 S2 真实库验证：未设置 FIELD_MIGRATION_MYSQL_URL");
        return;
    };
    let source = "fm_s2_full_source";
    let target = "fm_s2_full_target";
    let pool = MySqlPool::connect(&url).await.expect("连接 MySQL 失败");
    let mut conn = pool.acquire().await.expect("获取连接失败");
    let _ = sqlx::query(&format!("DROP TABLE IF EXISTS {target}"))
        .execute(&mut *conn)
        .await;
    seed(&mut conn, source, 100_000).await;
    drop(conn);

    let policy = Arc::new(OpsPolicy::default());
    let client = DbClient::new_default(&url).await.expect("创建 DbClient 失败");
    let audit_path = std::env::temp_dir().join(format!(
        "fm-s2-audit-{}-{}.jsonl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let audit = Arc::new(AuditLog::new(Some(audit_path)));
    let ctx = OpsContext::new(client, DB_ID.into(), db_name(&url), false)
        .with_policy(policy.clone())
        .with_audit(Some(audit.clone()), Some("s2-e2e".into()));

    let job = build_job(
        &ctx,
        policy.as_ref(),
        source,
        target,
        BackfillConfig {
            batch_size: 10_000,
            batch_interval_ms: 0,
            max_rows_per_second: 0,
        },
    )
    .await;
    let job_id = job.job_id.clone();
    let hub = MigrationHub::for_test();
    hub.insert(job).await.unwrap();

    let started = Instant::now();
    let job = run_to_end(&hub, &ctx, policy.as_ref(), &job_id).await;
    println!(
        "回填完成: status={:?} elapsed={}ms expected={} batches={} written={} last_pk={:?}",
        job.status,
        started.elapsed().as_millis(),
        job.expected_rows,
        job.checkpoint.as_ref().map(|c| c.completed_batches).unwrap_or(0),
        job.checkpoint.as_ref().map(|c| c.written_rows).unwrap_or(0),
        job.checkpoint.as_ref().and_then(|c| c.last_pk.clone()),
    );
    assert_eq!(job.status, MigrationStatus::Completed);
    assert_eq!(job.expected_rows, 100_000);
    assert_eq!(job.progress.success_rows, 100_000);

    let [rows, v2, v3, v4] = verify_v(&pool, source, target).await;
    assert_eq!(rows, 100_000);
    assert_eq!([v2, v3, v4], [0, 0, 0]);

    let batch_audits = audit
        .recent(2000)
        .expect("读取审计失败")
        .into_iter()
        .filter(|record| record.event == "field_migration.backfill.batch")
        .count();
    println!("逐批审计条数: {batch_audits}");
    assert_eq!(batch_audits, 10);
}

#[tokio::test]
async fn backfill_interrupt_then_resume_from_checkpoint() {
    let Ok(url) = std::env::var("FIELD_MIGRATION_MYSQL_URL") else {
        return;
    };
    let source = "fm_s2_resume_source";
    let target = "fm_s2_resume_target";
    let pool = MySqlPool::connect(&url).await.expect("连接 MySQL 失败");
    let mut conn = pool.acquire().await.expect("获取连接失败");
    let _ = sqlx::query(&format!("DROP TABLE IF EXISTS {target}"))
        .execute(&mut *conn)
        .await;
    seed(&mut conn, source, 100_000).await;
    drop(conn);

    let policy = Arc::new(OpsPolicy::default());
    let client = DbClient::new_default(&url).await.expect("创建 DbClient 失败");
    let ctx = OpsContext::new(client, DB_ID.into(), db_name(&url), false)
        .with_policy(policy.clone());
    let job = build_job(
        &ctx,
        policy.as_ref(),
        source,
        target,
        BackfillConfig {
            batch_size: 10_000,
            batch_interval_ms: 40,
            max_rows_per_second: 0,
        },
    )
    .await;
    assert_eq!(job.config.batch_size, 10_000);
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
                .map(|checkpoint| checkpoint.completed_batches >= 2)
                .unwrap_or(false)
        },
        Duration::from_secs(60),
    )
    .await;
    println!(
        "中断前: batches={} checkpoint_pk={:?} connection_id={:?}（关闭连接池模拟断连）",
        before_kill.checkpoint.as_ref().unwrap().completed_batches,
        before_kill.checkpoint.as_ref().unwrap().last_pk,
        before_kill.progress.connection_id
    );
    ctx.db_client.mysql_pool().expect("task pool").close().await;
    let _ = handle.await.expect("任务线程异常");
    let interrupted = hub.get(&job_id).await.unwrap();
    println!(
        "中断后: status={:?} last_error={:?} checkpoint_pk={:?}",
        interrupted.status,
        interrupted.last_error,
        interrupted.checkpoint.as_ref().and_then(|c| c.last_pk.clone())
    );
    assert_eq!(interrupted.status, MigrationStatus::Interrupted);
    assert_eq!(interrupted.progress.total_batches, 10, "首发批参数被改变");
    assert_eq!(interrupted.config.batch_size, 10_000, "首发批参数被改变");
    let checkpoint_before = interrupted.checkpoint.clone().expect("中断后缺少 checkpoint");

    let resume_client = DbClient::new_default(&url).await.expect("重建 DbClient 失败");
    let resume_ctx = OpsContext::new(resume_client, DB_ID.into(), db_name(&url), false)
        .with_policy(policy.clone());
    backfill::run_job(
        job_id.clone(),
        resume_ctx,
        hub.clone(),
        policy.as_ref().clone(),
    )
    .await;
    let resumed = hub.get(&job_id).await.unwrap();
    println!(
        "续跑完成: status={:?} resume_after_pk={:?} batches={} written={}",
        resumed.status,
        resumed.progress.resume_after_pk,
        resumed.checkpoint.as_ref().map(|c| c.completed_batches).unwrap_or(0),
        resumed.checkpoint.as_ref().map(|c| c.written_rows).unwrap_or(0),
    );
    assert_eq!(resumed.status, MigrationStatus::Completed);
    assert_eq!(resumed.progress.resume_after_pk, checkpoint_before.last_pk);
    assert_eq!(resumed.progress.resume_skipped_batches, checkpoint_before.completed_batches);
    assert_eq!(resumed.progress.total_batches, 10, "续跑批参数与首发不一致");
    assert_eq!(resumed.config.batch_size, 10_000, "续跑批参数与首发不一致");
    assert_eq!(resumed.expected_rows, 100_000);
    println!(
        "续跑参数一致性: batch_size={} total_batches={} resume_skipped={}",
        resumed.config.batch_size, resumed.progress.total_batches, resumed.progress.resume_skipped_batches
    );

    let [rows, v2, v3, v4] = verify_v(&pool, source, target).await;
    assert_eq!(rows, 100_000);
    assert_eq!([v2, v3, v4], [0, 0, 0]);
    let duplicates = count(
        &pool,
        &format!("SELECT COUNT(*) - COUNT(DISTINCT id) FROM {target}"),
    )
    .await;
    println!("重复主键行数: {duplicates}");
    assert_eq!(duplicates, 0);
    assert!(resumed.run_count >= 2);
}

#[tokio::test]
async fn backfill_blocks_at_corrupted_completed_batch_and_reports_pks() {
    let Ok(url) = std::env::var("FIELD_MIGRATION_MYSQL_URL") else {
        return;
    };
    let source = "fm_s2_block_source";
    let target = "fm_s2_block_target";
    let pool = MySqlPool::connect(&url).await.expect("连接 MySQL 失败");
    let mut conn = pool.acquire().await.expect("获取连接失败");
    let _ = sqlx::query(&format!("DROP TABLE IF EXISTS {target}"))
        .execute(&mut *conn)
        .await;
    seed(&mut conn, source, 20_000).await;
    drop(conn);

    let policy = Arc::new(OpsPolicy::default());
    let client = DbClient::new_default(&url).await.expect("创建 DbClient 失败");
    let ctx = OpsContext::new(client, DB_ID.into(), db_name(&url), false)
        .with_policy(policy.clone());
    let job = build_job(
        &ctx,
        policy.as_ref(),
        source,
        target,
        BackfillConfig {
            batch_size: 1_000,
            batch_interval_ms: 40,
            max_rows_per_second: 0,
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
                .map(|checkpoint| checkpoint.completed_batches >= 3)
                .unwrap_or(false)
        },
        Duration::from_secs(60),
    )
    .await;
    println!(
        "改坏前中断: batches={} checkpoint_pk={:?}（关闭连接池）",
        before_kill.checkpoint.as_ref().unwrap().completed_batches,
        before_kill.checkpoint.as_ref().unwrap().last_pk
    );
    ctx.db_client.mysql_pool().expect("task pool").close().await;
    let _ = handle.await.expect("任务线程异常");
    let interrupted = hub.get(&job_id).await.unwrap();
    assert_eq!(interrupted.status, MigrationStatus::Interrupted);

    // 把第 1 批内已完成的目标行改坏。
    sqlx::query(&format!(
        "UPDATE {target} SET note = 'corrupted-by-test' WHERE id = 1"
    ))
    .execute(&pool)
    .await
    .expect("改坏目标数据失败");

    let resume_client = DbClient::new_default(&url).await.expect("重建 DbClient 失败");
    let resume_ctx = OpsContext::new(resume_client, DB_ID.into(), db_name(&url), false)
        .with_policy(policy.clone());
    backfill::run_job(
        job_id.clone(),
        resume_ctx,
        hub.clone(),
        policy.as_ref().clone(),
    )
    .await;
    let blocked = hub.get(&job_id).await.unwrap();
    let blocking = blocked.blocking.clone().expect("缺少阻断详情");
    println!(
        "阻断: status={:?} phase={} batch={} range={:?}->{:?} total_diffs={} pks={:?} message={}",
        blocked.status,
        blocking.phase,
        blocking.batch_no,
        blocking.last_pk,
        blocking.end_pk,
        blocking.total_diffs,
        blocking.pks,
        blocking.message
    );
    assert_eq!(blocked.status, MigrationStatus::Blocked);
    assert!(blocking.phase.starts_with("final_recheck"), "phase={}", blocking.phase);
    assert_eq!(blocking.batch_no, 1);
    assert!(blocking.total_diffs >= 1);
    assert!(blocking.pks.iter().any(|pk| pk == "1"), "差异主键未定位到 1: {:?}", blocking.pks);
    let target_rows = count(&pool, &format!("SELECT COUNT(*) FROM {target}")).await;
    println!(
        "阻断后目标行数: {target_rows}，checkpoint_batches={}",
        blocked.checkpoint.as_ref().map(|c| c.completed_batches).unwrap_or(0)
    );
    assert_eq!(target_rows, 20_000);
    assert_eq!(blocked.checkpoint.as_ref().map(|c| c.completed_batches), Some(20));
}

#[tokio::test]
async fn backfill_replays_committed_but_uncheckpointed_batch_idempotently() {
    let Ok(url) = std::env::var("FIELD_MIGRATION_MYSQL_URL") else {
        return;
    };
    let source = "fm_s2_window_source";
    let target = "fm_s2_window_target";
    let pool = MySqlPool::connect(&url).await.expect("连接 MySQL 失败");
    let mut conn = pool.acquire().await.expect("获取连接失败");
    let _ = sqlx::query(&format!("DROP TABLE IF EXISTS {target}"))
        .execute(&mut *conn)
        .await;
    seed(&mut conn, source, 5_000).await;
    drop(conn);

    let policy = Arc::new(OpsPolicy::default());
    let client = DbClient::new_default(&url).await.expect("创建 DbClient 失败");
    let ctx = OpsContext::new(client, DB_ID.into(), db_name(&url), false)
        .with_policy(policy.clone());
    let job = build_job(
        &ctx,
        policy.as_ref(),
        source,
        target,
        BackfillConfig {
            batch_size: 1_000,
            batch_interval_ms: 40,
            max_rows_per_second: 0,
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
    let _ = wait_until(
        &hub,
        &job_id,
        |job| {
            job.checkpoint
                .as_ref()
                .map(|checkpoint| checkpoint.completed_batches >= 2)
                .unwrap_or(false)
        },
        Duration::from_secs(30),
    )
    .await;
    ctx.db_client.mysql_pool().expect("task pool").close().await;
    let _ = handle.await.expect("任务线程异常");
    let interrupted = hub.get(&job_id).await.unwrap();
    assert_eq!(interrupted.status, MigrationStatus::Interrupted);
    let checkpoint_pk = interrupted.checkpoint.as_ref().and_then(|c| c.last_pk.clone()).unwrap();
    println!(
        "窗口模拟前: checkpoint_batches={} checkpoint_pk={:?}",
        interrupted.checkpoint.as_ref().unwrap().completed_batches, checkpoint_pk
    );

    // 模拟「第 3 批 commit 成功但 checkpoint 未落盘」：把第 3 批数据提前写入目标。
    sqlx::query(&format!(
        "INSERT INTO {target} (id, theme, score, note) \
         SELECT id, \
                JSON_UNQUOTE(JSON_EXTRACT(payload, '$.theme')), \
                CAST(JSON_UNQUOTE(JSON_EXTRACT(payload, '$.score')) AS SIGNED), \
                JSON_UNQUOTE(JSON_EXTRACT(payload, '$.note')) \
         FROM {source} WHERE id > 2000 AND id <= 3000 \
         ON DUPLICATE KEY UPDATE theme = VALUES(theme), score = VALUES(score), note = VALUES(note)"
    ))
    .execute(&pool)
    .await
    .expect("模拟 commit 成功失败");
    let target_rows = count(&pool, &format!("SELECT COUNT(*) FROM {target}")).await;
    println!("窗口模拟后: 目标已有 {target_rows} 行（checkpoint 仍为 2 批）");
    assert_eq!(target_rows, 3_000);

    let resume_client = DbClient::new_default(&url).await.expect("重建 DbClient 失败");
    let resume_ctx = OpsContext::new(resume_client, DB_ID.into(), db_name(&url), false)
        .with_policy(policy.clone());
    backfill::run_job(
        job_id.clone(),
        resume_ctx,
        hub.clone(),
        policy.as_ref().clone(),
    )
    .await;
    let resumed = hub.get(&job_id).await.unwrap();
    println!(
        "窗口续跑完成: status={:?} checkpoint_batches={} written={}",
        resumed.status,
        resumed.checkpoint.as_ref().map(|c| c.completed_batches).unwrap_or(0),
        resumed.checkpoint.as_ref().map(|c| c.written_rows).unwrap_or(0)
    );
    assert_eq!(resumed.status, MigrationStatus::Completed);
    assert_eq!(resumed.checkpoint.as_ref().map(|c| c.completed_batches), Some(5));
    assert_eq!(resumed.checkpoint.as_ref().map(|c| c.written_rows), Some(5_000));
    let batch3 = resumed
        .recent_batches
        .iter()
        .find(|record| record.batch_no == 3)
        .expect("缺少第 3 批记录");
    assert_eq!(batch3.written_rows, 0, "已 commit 的边界批次不应重复写入");
    let [rows, v2, v3, v4] = verify_v(&pool, source, target).await;
    assert_eq!(rows, 5_000);
    assert_eq!([v2, v3, v4], [0, 0, 0]);
    let duplicates = count(
        &pool,
        &format!("SELECT COUNT(*) - COUNT(DISTINCT id) FROM {target}"),
    )
    .await;
    assert_eq!(duplicates, 0);
    println!("窗口续跑: 无重复行，V1-V4 全 0");
}

#[tokio::test]
async fn backfill_rate_limit_pause_and_abort() {
    let Ok(url) = std::env::var("FIELD_MIGRATION_MYSQL_URL") else {
        return;
    };
    let source = "fm_s2_rate_source";
    let pool = MySqlPool::connect(&url).await.expect("连接 MySQL 失败");
    let mut conn = pool.acquire().await.expect("获取连接失败");
    for table in ["fm_s2_rate_fast", "fm_s2_rate_slow", "fm_s2_rate_abort"] {
        let _ = sqlx::query(&format!("DROP TABLE IF EXISTS {table}"))
            .execute(&mut *conn)
            .await;
    }
    seed(&mut conn, source, 6_000).await;
    drop(conn);

    let policy = Arc::new(OpsPolicy::default());
    let client = DbClient::new_default(&url).await.expect("创建 DbClient 失败");
    let ctx = OpsContext::new(client, DB_ID.into(), db_name(&url), false)
        .with_policy(policy.clone());

    // 不限速基线。
    let fast_job = build_job(
        &ctx,
        policy.as_ref(),
        source,
        "fm_s2_rate_fast",
        BackfillConfig {
            batch_size: 1_000,
            batch_interval_ms: 0,
            max_rows_per_second: 0,
        },
    )
    .await;
    let fast_id = fast_job.job_id.clone();
    let hub = MigrationHub::for_test();
    hub.insert(fast_job).await.unwrap();
    let fast_started = Instant::now();
    let fast = run_to_end(&hub, &ctx, policy.as_ref(), &fast_id).await;
    let fast_ms = fast_started.elapsed().as_millis();
    assert_eq!(fast.status, MigrationStatus::Completed);
    println!("不限速: {}ms", fast_ms);

    // 限速 1000 行/秒：6000 行至少需要约 6 秒。
    let slow_job = build_job(
        &ctx,
        policy.as_ref(),
        source,
        "fm_s2_rate_slow",
        BackfillConfig {
            batch_size: 1_000,
            batch_interval_ms: 0,
            max_rows_per_second: 1_000,
        },
    )
    .await;
    let slow_id = slow_job.job_id.clone();
    let slow_hub = MigrationHub::for_test();
    slow_hub.insert(slow_job).await.unwrap();
    let slow_wall = Instant::now();
    let handle = tokio::spawn(backfill::run_job(
        slow_id.clone(),
        ctx.clone(),
        slow_hub.clone(),
        policy.as_ref().clone(),
    ));
    let _ = wait_until(
        &slow_hub,
        &slow_id,
        |job| {
            job.checkpoint
                .as_ref()
                .map(|checkpoint| checkpoint.completed_batches >= 2)
                .unwrap_or(false)
        },
        Duration::from_secs(30),
    )
    .await;
    slow_hub.set_control(&slow_id, CONTROL_PAUSE).await;
    let paused = wait_until(
        &slow_hub,
        &slow_id,
        |job| job.status == MigrationStatus::Paused,
        Duration::from_secs(10),
    )
    .await;
    println!(
        "暂停: status={:?} checkpoint_batches={}",
        paused.status,
        paused.checkpoint.as_ref().map(|c| c.completed_batches).unwrap_or(0)
    );
    let paused_checkpoint = paused.checkpoint.clone().unwrap();
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    let still_paused = slow_hub.get(&slow_id).await.unwrap();
    assert_eq!(still_paused.status, MigrationStatus::Paused);
    assert_eq!(
        still_paused.checkpoint.as_ref().map(|c| c.completed_batches),
        Some(paused_checkpoint.completed_batches),
        "暂停期间 checkpoint 不应推进"
    );
    println!(
        "暂停稳定: checkpoint_batches={}（1s 内未推进）",
        paused_checkpoint.completed_batches
    );
    slow_hub.set_control(&slow_id, CONTROL_RUN).await;
    let _ = handle.await.expect("任务线程异常");
    let slow = slow_hub.get(&slow_id).await.unwrap();
    println!(
        "限速完成: status={:?} 总墙钟={}ms（6000 行 @1000rps，含暂停约 1s）",
        slow.status,
        slow_wall.elapsed().as_millis()
    );
    assert_eq!(slow.status, MigrationStatus::Completed);
    assert!(
        slow_wall.elapsed() >= Duration::from_secs(5),
        "限速未生效：6000 行 @1000rps 耗时仅 {:?}",
        slow_wall.elapsed()
    );
    let [rows, v2, v3, v4] = verify_v(&pool, source, "fm_s2_rate_slow").await;
    assert_eq!(rows, 6_000);
    assert_eq!([v2, v3, v4], [0, 0, 0]);

    // 中止：写入若干批后中止，状态必须为 Aborted。
    let abort_job = build_job(
        &ctx,
        policy.as_ref(),
        source,
        "fm_s2_rate_abort",
        BackfillConfig {
            batch_size: 1_000,
            batch_interval_ms: 100,
            max_rows_per_second: 0,
        },
    )
    .await;
    let abort_id = abort_job.job_id.clone();
    let abort_hub = MigrationHub::for_test();
    abort_hub.insert(abort_job).await.unwrap();
    let handle = tokio::spawn(backfill::run_job(
        abort_id.clone(),
        ctx.clone(),
        abort_hub.clone(),
        policy.as_ref().clone(),
    ));
    wait_until(
        &abort_hub,
        &abort_id,
        |job| {
            job.checkpoint
                .as_ref()
                .map(|checkpoint| checkpoint.completed_batches >= 1)
                .unwrap_or(false)
        },
        Duration::from_secs(30),
    )
    .await;
    abort_hub.set_control(&abort_id, CONTROL_ABORT).await;
    let _ = handle.await.expect("任务线程异常");
    let aborted = abort_hub.get(&abort_id).await.unwrap();
    println!(
        "中止: status={:?} batches={}",
        aborted.status,
        aborted.checkpoint.as_ref().map(|c| c.completed_batches).unwrap_or(0)
    );
    assert_eq!(aborted.status, MigrationStatus::Aborted);
}
