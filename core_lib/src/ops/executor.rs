//! 索引变更执行器：dry-run、影响评估、在线 DDL、复测与补偿回滚。
//!
//! 新链路完全不使用 `DdlEngine` / `execute_ddl`。执行前必须：
//! 1) 审批令牌绑定计划哈希并一次性消费；2) 刷新 schema 重新编译校验；
//! 3) 先写审计再执行；4) 失败时对已成功的建索引动作做补偿。

use std::time::Duration;

use crate::ops::approval::ApprovalStore;
use crate::ops::audit::AuditLog;
use crate::ops::ddl_compiler;
use crate::ops::error::OpsError;
use crate::ops::index_advisor;
use crate::ops::policy::OpsPolicy;
use crate::ops::registry::OpsContext;
use crate::ops::types::{
    ActionExecution, ActionSpec, ActionState, AuditRecord, CompiledAction, ExecutionReport,
    ExplainSample, ImpactAssessment, OpsPlan, QueryComparison, TableSnapshot, TableStats,
    VerificationReport,
};

/// 执行前刷新 schema，防止计划审批后表结构已变化。
pub async fn refresh_snapshot(ctx: &OpsContext, table: &str) -> Result<TableSnapshot, OpsError> {
    index_advisor::load_snapshot_public(ctx, table).await
}

pub async fn refresh_stats(ctx: &OpsContext, table: &str) -> Result<TableStats, OpsError> {
    index_advisor::load_table_stats_public(ctx, table).await
}

/// dry-run：只编译、评估影响，不执行任何 DDL。
pub async fn dry_run(
    ctx: &OpsContext,
    plan: &OpsPlan,
    action_ids: &[String],
    policy: &OpsPolicy,
    audit: &AuditLog,
) -> Result<ExecutionReport, OpsError> {
    policy.authorize_apply(true, ctx.is_read_only)?;
    let actions = selected_actions(plan, action_ids)?;
    let snapshot = refresh_snapshot(ctx, action_table(&actions)?).await?;
    let compiled = ddl_compiler::compile_actions(&actions, &snapshot)?;
    let stats = refresh_stats(ctx, &snapshot.table_name).await?;
    let impact = assess_impact(&snapshot, &stats, &actions);

    let started_at = crate::ops::types::now();
    let actions_report = compiled
        .iter()
        .map(|c| ActionExecution {
            action_id: c.action_id.clone(),
            sql: c.sql.clone(),
            state: ActionState::Pending,
            started_at,
            finished_at: started_at,
            duration_ms: 0,
            error: None,
            rollback_sql: c.rollback_sql.clone(),
        })
        .collect::<Vec<_>>();

    let report = ExecutionReport {
        plan_id: plan.plan_id.clone(),
        dry_run: true,
        started_at,
        finished_at: crate::ops::types::now(),
        actions: actions_report,
        impact: Some(impact),
        verification: None,
        error: None,
        rollback_performed: false,
        compiled_sql: compiled.into_iter().map(|c| c.sql).collect(),
    };

    audit.append(&audit_record(
        "ops.dry_run",
        "ok",
        plan,
        None,
        serde_json::json!({
            "action_ids": action_ids,
            "compiled_sql": report.compiled_sql,
            "impact": report.impact,
        }),
    ))?;
    Ok(report)
}

/// 真实执行。调用方必须先把审批令牌交给这里，由这里一次性消费。
pub async fn execute(
    ctx: &OpsContext,
    plan: &OpsPlan,
    action_ids: &[String],
    approval: &mut ApprovalStore,
    approval_token: &str,
    policy: &OpsPolicy,
    audit: &AuditLog,
) -> Result<ExecutionReport, OpsError> {
    policy.authorize_apply(false, ctx.is_read_only)?;
    if !ctx.is_mysql_family() {
        return Err(OpsError::Validation(
            "首期执行链路仅支持 MySQL/MariaDB".into(),
        ));
    }
    if !plan.requires_approval {
        return Err(OpsError::Approval("该计划没有可执行动作".into()));
    }

    let actions = selected_actions(plan, action_ids)?;
    let table = action_table(&actions)?.to_string();
    let snapshot = refresh_snapshot(ctx, &table).await?;
    let compiled = ddl_compiler::compile_actions(&actions, &snapshot)?;
    let stats = refresh_stats(ctx, &table).await?;
    let impact = assess_impact(&snapshot, &stats, &actions);

    if let Some(rows) = stats.table_rows {
        if rows > policy.max_affected_rows && !policy.allow_large_table {
            return Err(OpsError::Policy(format!(
                "表 {} 估算 {} 行，超过 max_affected_rows {}；拒绝执行",
                table, rows, policy.max_affected_rows
            )));
        }
    }

    // 先消费令牌，保证同一张审批不能被执行两次。
    approval.consume(approval_token, plan, action_ids)?;

    // 执行前审计必须成功（fail closed）。
    audit.append(&audit_record(
        "ops.execute.start",
        "started",
        plan,
        None,
        serde_json::json!({
            "action_ids": action_ids,
            "compiled_sql": compiled.iter().map(|c| &c.sql).collect::<Vec<_>>(),
            "impact": impact,
            "db_fingerprint": plan.db_fingerprint,
            "plan_hash": plan.plan_hash,
        }),
    ))?;

    let before_verification = explain_for_plan(ctx, plan).await;
    let started_at = crate::ops::types::now();
    let mut action_reports = Vec::new();
    let mut failure: Option<String> = None;
    let mut rollback_performed = false;
    let mut timed_out_connection: Option<u64> = None;

    let pool = ctx.db_client.mysql_pool()?;
    let mut conn = pool.acquire().await?;
    let lock_wait = policy.lock_wait_timeout_secs.max(1);
    for stmt in [
        format!("SET SESSION lock_wait_timeout = {lock_wait}"),
        format!("SET SESSION innodb_lock_wait_timeout = {lock_wait}"),
    ] {
        let _ = sqlx::query(&stmt).execute(&mut *conn).await;
    }
    // 记录本连接 id：客户端超时后服务端 DDL 可能仍挂着，需要主动 KILL QUERY。
    let connection_id: u64 = sqlx::query_scalar("SELECT CONNECTION_ID()")
        .fetch_one(&mut *conn)
        .await
        .unwrap_or(0);

    for compiled_action in &compiled {
        let action_started = crate::ops::types::now();
        let result = tokio::time::timeout(
            Duration::from_secs(policy.ddl_timeout_secs.max(1)),
            sqlx::query(&compiled_action.sql).execute(&mut *conn),
        )
        .await;

        match result {
            Ok(Ok(_)) => {
                let finished = crate::ops::types::now();
                action_reports.push(ActionExecution {
                    action_id: compiled_action.action_id.clone(),
                    sql: compiled_action.sql.clone(),
                    state: ActionState::Succeeded,
                    started_at: action_started,
                    finished_at: finished,
                    duration_ms: ((finished - action_started).max(0) * 1000) as u64,
                    error: None,
                    rollback_sql: compiled_action.rollback_sql.clone(),
                });
            }
            Ok(Err(err)) => {
                let finished = crate::ops::types::now();
                let message = err.to_string();
                action_reports.push(ActionExecution {
                    action_id: compiled_action.action_id.clone(),
                    sql: compiled_action.sql.clone(),
                    state: ActionState::Failed,
                    started_at: action_started,
                    finished_at: finished,
                    duration_ms: ((finished - action_started).max(0) * 1000) as u64,
                    error: Some(message.clone()),
                    rollback_sql: compiled_action.rollback_sql.clone(),
                });
                failure = Some(message);
                break;
            }
            Err(_) => {
                let finished = crate::ops::types::now();
                let message = format!(
                    "DDL 执行超时（{}s）",
                    policy.ddl_timeout_secs.max(1)
                );
                action_reports.push(ActionExecution {
                    action_id: compiled_action.action_id.clone(),
                    sql: compiled_action.sql.clone(),
                    state: ActionState::Failed,
                    started_at: action_started,
                    finished_at: finished,
                    duration_ms: ((finished - action_started).max(0) * 1000) as u64,
                    error: Some(message.clone()),
                    rollback_sql: compiled_action.rollback_sql.clone(),
                });
                failure = Some(message);
                timed_out_connection = Some(connection_id);
                break;
            }
        }
    }

    // 失败后的补偿以“真实索引状态”为准：客户端超时不代表服务端 DDL 已取消，
    // 语句仍可能完成并留下索引。先 KILL QUERY 主动中止服务端语句，再关闭连接、
    // 重新读取 schema，只删除执行前不存在、执行后实际出现的索引。
    if failure.is_some() {
        if let Some(connection_id) = timed_out_connection {
            if connection_id != 0 {
                if let Ok(kill_pool) = ctx.db_client.mysql_pool() {
                    if let Ok(mut killer) = kill_pool.acquire().await {
                        let _ = sqlx::query(&format!("KILL QUERY {connection_id}"))
                            .execute(&mut *killer)
                            .await;
                    }
                }
            }
        }
        let _ = conn.close().await;
        match compensate_created_indexes(ctx, &table, &snapshot, &compiled, policy).await {
            Ok((attempted, compensation_errors)) => {
                rollback_performed = attempted && compensation_errors.is_empty();
                if !compensation_errors.is_empty() {
                    failure = Some(format!(
                        "{}；补偿失败: {}",
                        failure.unwrap_or_default(),
                        compensation_errors.join("; ")
                    ));
                }
            }
            Err(err) => {
                failure = Some(format!(
                    "{}；补偿状态未知: {}",
                    failure.unwrap_or_default(),
                    err
                ));
            }
        }
    }

    // 失败时不做 after-EXPLAIN：失败会触发补偿，schema 已回到执行前，
    // 此时再对比只会得到“0/N 未改善”，把“执行失败并已回滚”误读成“索引无效”。
    // 同时也避免卡在失败连接上残留的待处理 MDL。
    let verification = if failure.is_some() {
        None
    } else {
        let (before_explain, mut verify_notes) = match before_verification {
            Ok((samples, notes)) => (samples, notes),
            Err(err) => (Vec::new(), vec![format!("执行前 EXPLAIN 失败: {err}")]),
        };
        match explain_for_plan(ctx, plan).await {
            Ok((after_explain, after_notes)) => {
                verify_notes.extend(after_notes);
                Some(compare_explain(&before_explain, &after_explain, verify_notes))
            }
            Err(err) => {
                verify_notes.push(format!("执行后 EXPLAIN 失败: {err}"));
                Some(compare_explain(
                    &before_explain,
                    &Vec::new(),
                    verify_notes,
                ))
            }
        }
    };
    let finished_at = crate::ops::types::now();

    let report = ExecutionReport {
        plan_id: plan.plan_id.clone(),
        dry_run: false,
        started_at,
        finished_at,
        actions: action_reports,
        impact: Some(impact),
        verification,
        error: failure.clone(),
        rollback_performed,
        compiled_sql: compiled.iter().map(|c| c.sql.clone()).collect(),
    };

    let audit_result = audit.append(&audit_record(
        "ops.execute.result",
        if failure.is_some() { "failed" } else { "ok" },
        plan,
        None,
        serde_json::json!({
            "dry_run": false,
            "rollback_performed": rollback_performed,
            "error": failure,
            "actions": report.actions.iter().map(|a| serde_json::json!({
                "action_id": a.action_id,
                "state": a.state,
                "duration_ms": a.duration_ms,
                "error": a.error,
            })).collect::<Vec<_>>(),
            "verification": report.verification,
        }),
    ));
    if let Err(err) = audit_result {
        tracing::error!("执行结果审计写入失败: {}", err);
    }

    // DDL 失败也返回执行结果：report.error / rollback_performed 需要被持久化并呈现，
    // 调用方据 report.error 将计划标记为 Failed，而不是丢掉报告只返回裸错误。
    Ok(report)
}

/// 按真实 schema 状态补偿本次执行中“实际已建”的索引。
///
/// 返回 (是否执行过补偿, 补偿错误列表)。只处理执行前不存在、
/// 执行后实际出现的索引，避免误删既有索引；客户端超时后服务端仍可能
/// 完成 DDL，因此不能只信任逐条语句的返回值。
async fn compensate_created_indexes(
    ctx: &OpsContext,
    table: &str,
    pre_snapshot: &TableSnapshot,
    compiled: &[CompiledAction],
    policy: &OpsPolicy,
) -> Result<(bool, Vec<String>), OpsError> {
    let refreshed = refresh_snapshot(ctx, table).await?;
    let mut targets: Vec<CompiledAction> = Vec::new();
    for action in compiled {
        let Some(ActionSpec::DropIndex { name, .. }) = action.compensation.as_ref() else {
            continue;
        };
        if pre_snapshot.find_index(name).is_some() {
            continue;
        }
        let Some(definition) = refreshed.find_index(name) else {
            continue;
        };
        let spec = ActionSpec::DropIndex {
            table: table.to_string(),
            name: name.clone(),
            expected_definition: Some(format!(
                "INDEX ({})",
                definition.column_names().join(",")
            )),
        };
        targets.push(ddl_compiler::compile_action(&spec, &refreshed)?);
    }
    if targets.is_empty() {
        return Ok((false, Vec::new()));
    }

    let pool = ctx.db_client.mysql_pool()?;
    let mut conn = pool.acquire().await?;
    let lock_wait = policy.lock_wait_timeout_secs.max(1);
    for stmt in [
        format!("SET SESSION lock_wait_timeout = {lock_wait}"),
        format!("SET SESSION innodb_lock_wait_timeout = {lock_wait}"),
    ] {
        let _ = sqlx::query(&stmt).execute(&mut *conn).await;
    }

    // 清理 DROP 需要拿 DDL 的 MDL，等待上限由 lock_wait_timeout 决定，
    // 因此给足 lock_wait_timeout + 余量，避免清理被 ddl_timeout 提前掐断。
    let cleanup_timeout =
        Duration::from_secs(policy.lock_wait_timeout_secs.max(policy.ddl_timeout_secs).max(1) + 5);
    let mut errors = Vec::new();
    for target in targets {
        let result = tokio::time::timeout(
            cleanup_timeout,
            sqlx::query(&target.sql).execute(&mut *conn),
        )
        .await;
        match result {
            Ok(Ok(_)) => {}
            Ok(Err(err)) => errors.push(format!("{}: {}", target.action_id, err)),
            Err(_) => errors.push(format!("{}: 补偿执行超时", target.action_id)),
        }
    }
    Ok((true, errors))
}

/// 回滚：按计划中的 DROP INDEX 动作删除本次新增索引。
pub async fn rollback(
    ctx: &OpsContext,
    plan: &OpsPlan,
    reason: &str,
    policy: &OpsPolicy,
    audit: &AuditLog,
) -> Result<ExecutionReport, OpsError> {
    if reason.trim().is_empty() {
        return Err(OpsError::Approval("回滚必须填写原因".into()));
    }
    policy.authorize_apply(false, ctx.is_read_only)?;
    if plan.rollback.actions.is_empty() {
        return Err(OpsError::Validation("该计划没有可回滚动作".into()));
    }
    let table = plan
        .evidence_summary
        .as_ref()
        .map(|s| s.table.clone())
        .ok_or_else(|| OpsError::Validation("计划缺少表信息".into()))?;
    let snapshot = refresh_snapshot(ctx, &table).await?;

    audit.append(&audit_record(
        "ops.rollback.start",
        "started",
        plan,
        None,
        serde_json::json!({ "reason": reason }),
    ))?;

    let pool = ctx.db_client.mysql_pool()?;
    let mut conn = pool.acquire().await?;
    let lock_wait = policy.lock_wait_timeout_secs.max(1);
    let _ = sqlx::query(&format!("SET SESSION lock_wait_timeout = {lock_wait}"))
        .execute(&mut *conn)
        .await;

    let started_at = crate::ops::types::now();
    let mut reports = Vec::new();
    let mut compiled_sql = Vec::new();
    let mut failure = None;
    for action in &plan.rollback.actions {
        let ActionSpec::DropIndex { name, .. } = action else {
            continue;
        };
        // 已不存在则视为回滚成功（幂等）。
        if snapshot.find_index(name).is_none() {
            reports.push(ActionExecution {
                action_id: action.action_id(),
                sql: String::new(),
                state: ActionState::Skipped,
                started_at,
                finished_at: crate::ops::types::now(),
                duration_ms: 0,
                error: None,
                rollback_sql: None,
            });
            continue;
        }
        let compiled = match ddl_compiler::compile_action(action, &snapshot) {
            Ok(compiled) => compiled,
            Err(err) => {
                failure = Some(err.to_string());
                break;
            }
        };
        compiled_sql.push(compiled.sql.clone());
        let action_started = crate::ops::types::now();
        let result = tokio::time::timeout(
            Duration::from_secs(policy.ddl_timeout_secs.max(1)),
            sqlx::query(&compiled.sql).execute(&mut *conn),
        )
        .await;
        match result {
            Ok(Ok(_)) => reports.push(ActionExecution {
                action_id: compiled.action_id,
                sql: compiled.sql,
                state: ActionState::Succeeded,
                started_at: action_started,
                finished_at: crate::ops::types::now(),
                duration_ms: 0,
                error: None,
                rollback_sql: None,
            }),
            Ok(Err(err)) => {
                failure = Some(err.to_string());
                break;
            }
            Err(_) => {
                failure = Some(format!("回滚 DDL 超时（{}s）", policy.ddl_timeout_secs));
                break;
            }
        }
    }

    let report = ExecutionReport {
        plan_id: plan.plan_id.clone(),
        dry_run: false,
        started_at,
        finished_at: crate::ops::types::now(),
        actions: reports,
        impact: None,
        verification: None,
        error: failure.clone(),
        rollback_performed: false,
        compiled_sql,
    };

    let _ = audit.append(&audit_record(
        "ops.rollback.result",
        if failure.is_some() { "failed" } else { "ok" },
        plan,
        None,
        serde_json::json!({ "reason": reason, "error": failure }),
    ));

    if let Some(err) = failure {
        return Err(OpsError::Db(err));
    }
    Ok(report)
}

fn selected_actions<'a>(plan: &'a OpsPlan, action_ids: &[String]) -> Result<Vec<ActionSpec>, OpsError> {
    if action_ids.is_empty() {
        return Err(OpsError::Validation("请选择至少一个动作".into()));
    }
    let mut out = Vec::new();
    for id in action_ids {
        let action = plan
            .action_by_id(id)
            .ok_or_else(|| OpsError::Validation(format!("动作 {id} 不属于计划")))?;
        if !action.risk_class().is_auto_executable() {
            return Err(OpsError::Validation(format!(
                "动作 {id} 风险等级 {} 不允许自动执行",
                action.risk_class().as_str()
            )));
        }
        out.push(action.clone());
    }
    Ok(out)
}

fn action_table<'a>(actions: &'a [ActionSpec]) -> Result<&'a str, OpsError> {
    let first = actions
        .first()
        .ok_or_else(|| OpsError::Validation("动作集合为空".into()))?;
    let table = first.table();
    if actions.iter().any(|a| a.table() != table) {
        return Err(OpsError::Validation(
            "首期只支持单表计划，审批动作表名不一致".into(),
        ));
    }
    Ok(table)
}

fn assess_impact(
    snapshot: &TableSnapshot,
    stats: &TableStats,
    actions: &[ActionSpec],
) -> ImpactAssessment {
    let mut notes = Vec::new();
    let blocking = false;
    let estimated_rows = stats.table_rows;
    let mut estimated_index_bytes = None;
    if let Some(rows) = stats.table_rows {
        let mut per_row = 0usize;
        for action in actions {
            if let ActionSpec::CreateIndex { columns, .. } = action {
                per_row += columns.len() * 14 + 8;
            }
        }
        estimated_index_bytes = Some(rows.saturating_mul(per_row as u64));
        notes.push(format!("索引列数合计 {}，按每行约 {} 字节估算", actions.len(), per_row));
    }
    if snapshot.columns.iter().any(|c| c.column_type.to_ascii_lowercase().contains("varchar")) {
        notes.push("varchar 列较长时，实际索引体积可能高于估算".into());
    }
    notes.push("执行将使用 ALGORITHM=INPLACE, LOCK=NONE；如不支持会失败并自动补偿".into());

    if let (Some(rows), Some(data_length)) = (stats.table_rows, stats.data_length) {
        if data_length > 20 * 1024 * 1024 * 1024 {
            notes.push(format!(
                "表数据量约 {:.1} GB，大表 DDL 需要更长窗口",
                data_length as f64 / (1024.0 * 1024.0 * 1024.0)
            ));
        }
        let _ = rows;
    }

    ImpactAssessment {
        estimated_rows,
        estimated_index_bytes,
        metadata_lock: "ALTER TABLE 需要短暂元数据锁；存在长事务时可能等待".into(),
        write_amplification: format!(
            "新增 {} 个索引会增加写放大",
            actions
                .iter()
                .filter(|a| matches!(a, ActionSpec::CreateIndex { .. }))
                .count()
        ),
        blocking,
        notes,
    }
}

async fn explain_queries(ctx: &OpsContext, sqls: &[String]) -> Vec<ExplainSample> {
    let mut out = Vec::new();
    for (idx, sql) in sqls.iter().enumerate() {
        let label = format!("workload-{}", idx + 1);
        if let Ok(Some(sample)) = index_advisor::explain_sample_public(ctx, &label, sql).await {
            out.push(sample);
        }
    }
    out
}

async fn explain_for_plan(
    ctx: &OpsContext,
    plan: &OpsPlan,
) -> Result<(Vec<ExplainSample>, Vec<String>), OpsError> {
    if plan.scenario_id == "perf-diagnostics" {
        crate::ops::perf_diagnostics::explain_for_verification(
            ctx,
            &plan.verification_sql,
            plan.verification_explain_analyze,
        )
        .await
    } else {
        Ok((explain_queries(ctx, &plan.verification_sql).await, vec![]))
    }
}

fn compare_explain(
    before: &[ExplainSample],
    after: &[ExplainSample],
    notes: Vec<String>,
) -> VerificationReport {
    let total = before.len().max(after.len());
    let mut comparisons = Vec::new();
    let mut improved = 0usize;
    for index in 0..total {
        let b = before.get(index);
        let a = after.get(index);
        let label = b
            .or(a)
            .map(|s| s.label.clone())
            .unwrap_or_else(|| format!("verify-{}", index + 1));
        let before_access = b.and_then(|s| s.access_type.clone());
        let after_access = a.and_then(|s| s.access_type.clone());
        let before_key = b.and_then(|s| s.key.clone()).filter(|k| !k.is_empty());
        let after_key = a.and_then(|s| s.key.clone()).filter(|k| !k.is_empty());
        let before_rows = b.and_then(|s| s.rows_examined);
        let after_rows = a.and_then(|s| s.rows_examined);
        let before_cost = b.and_then(|s| s.estimated_cost);
        let after_cost = a.and_then(|s| s.estimated_cost);
        let before_actual = b.and_then(|s| s.actual_time_ms);
        let after_actual = a.and_then(|s| s.actual_time_ms);

        if b.is_none() || a.is_none() {
            comparisons.push(QueryComparison {
                label,
                before_access,
                after_access,
                before_key,
                after_key,
                before_rows,
                after_rows,
                before_cost,
                after_cost,
                before_actual_ms: before_actual,
                after_actual_ms: after_actual,
                improved: false,
                verdict: "缺少执行前或执行后的 EXPLAIN 证据，无法对比".into(),
            });
            continue;
        }

        let key_gained = before_key.is_none() && after_key.is_some();
        let key_lost = before_key.is_some() && after_key.is_none();
        let rows_dropped = matches!((before_rows, after_rows), (Some(b), Some(a)) if a < b);
        let cost_dropped = matches!((before_cost, after_cost), (Some(b), Some(a)) if a < b);
        let actual_dropped = matches!((before_actual, after_actual), (Some(b), Some(a)) if a < b * 0.95);
        let is_improved = key_gained || rows_dropped || cost_dropped || actual_dropped;
        if is_improved {
            improved += 1;
        }

        let verdict = if key_lost {
            "索引命中退化：执行后 key 从有变无".to_string()
        } else if is_improved {
            let mut parts = Vec::new();
            if key_gained {
                parts.push(format!(
                    "key {} -> {}",
                    before_key.as_deref().unwrap_or("-"),
                    after_key.as_deref().unwrap_or("-")
                ));
            }
            if rows_dropped {
                parts.push(format!(
                    "扫描行 {} -> {}",
                    before_rows.unwrap_or(0),
                    after_rows.unwrap_or(0)
                ));
            }
            if cost_dropped {
                parts.push(format!(
                    "成本 {:.2} -> {:.2}",
                    before_cost.unwrap_or(0.0),
                    after_cost.unwrap_or(0.0)
                ));
            }
            if actual_dropped {
                parts.push(format!(
                    "实际耗时 {:.3}ms -> {:.3}ms",
                    before_actual.unwrap_or(0.0),
                    after_actual.unwrap_or(0.0)
                ));
            }
            format!("改善：{}", parts.join("；"))
        } else {
            "未改善：访问方式、扫描行数、成本与实际耗时均无明显变化".to_string()
        };

        comparisons.push(QueryComparison {
            label,
            before_access,
            after_access,
            before_key,
            after_key,
            before_rows,
            after_rows,
            before_cost,
            after_cost,
            before_actual_ms: before_actual,
            after_actual_ms: after_actual,
            improved: is_improved,
            verdict,
        });
    }
    let summary = if total == 0 {
        "没有可用于复测的工作负载 SQL".to_string()
    } else {
        format!("{improved}/{total} 条查询的索引命中、扫描行数、成本或实际耗时得到改善")
    };
    VerificationReport {
        before: before.to_vec(),
        after: after.to_vec(),
        improved_queries: improved,
        total_queries: total,
        summary,
        comparisons,
        notes,
    }
}

fn audit_record(
    event: &str,
    outcome: &str,
    plan: &OpsPlan,
    action_id: Option<&str>,
    detail: serde_json::Value,
) -> AuditRecord {
    let mut record = AuditRecord::new(event, outcome);
    record.plan_id = Some(plan.plan_id.clone());
    record.db_id = Some(plan.db_id.clone());
    record.db_name = Some(plan.db_name.clone());
    record.action_id = action_id.map(str::to_string);
    record.actor = Some("local-user".into());
    record.detail = detail;
    record
}
