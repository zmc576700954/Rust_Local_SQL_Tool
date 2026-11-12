//! PostgreSQL `ALTER COLUMN TYPE` 执行器（S4）。
//!
//! 约束：
//! - 白名单（PG 保证不触发表重写的组合）自动执行，其余一律 `Rewrite`；
//! - `Rewrite` 必须显式 `allow_rewrite=true`（且沿用 `production_confirmed` 门）；
//! - 非确定性转换（`timestamp`↔`timestamptz` 等，依赖 session TimeZone）即使开闸也 BLOCK；
//! - 单语句自动提交，不进事务块；失败由 PG 原子回滚列本身，续跑 = 重跑语句；
//! - 目标类型经本地解析 + `::regtype` 双重验证，SQL 由解析产物重构，不拼原始串。

use serde_json::json;
use sqlx::postgres::PgDatabaseError;

use crate::ops::error::OpsError;
use crate::ops::pg_rows::convert_pg_rows;
use crate::ops::policy::OpsPolicy;
use crate::ops::registry::OpsContext;
use crate::ops::types::{hash_json, now, AuditRecord};
use crate::sql::util::quote_ident_pg;

use super::migration_store::{
    BlockingDetail, MigrationHub, MigrationStatus, TypeChangeClass, TypeChangeExecution,
    TypeChangeJob, TypeChangePhase, TypeChangeVerify, CONTROL_ABORT, CONTROL_PAUSE,
};
use super::pg_backfill::exec_ddl_raw;
use super::pg_evidence::normalize_pg_type;
use super::pg_verify::qualified;

const ENGINE: &str = "postgresql";

#[derive(Debug, Clone)]
pub struct TypeChangeRequest {
    pub schema: Option<String>,
    pub table: String,
    pub column: String,
    pub target_type: String,
    pub allow_rewrite: bool,
    pub production_confirmed: bool,
}

// ── B 组：类型解析与分类（纯函数） ────────────────────────

/// 归一化 + 剥 typmod 后的类型。`typmod` 只覆盖 `name` / `name(n)` / `name(p,s)`
/// 三种形态；`timestamp` 的 typmod 出现在词中间（`timestamp(3) with time zone`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedType {
    pub base: String,
    /// `Some((n, None))` = 单参 typmod；`Some((p, Some(s)))` = 双参。
    pub typmod: Option<(i32, Option<i32>)>,
}

pub(crate) fn parse_type(raw: &str) -> Option<ParsedType> {
    let normalized = normalize_pg_type(raw);
    // `normalize_pg_type` 只处理整串别名；带 typmod 的别名在此展开。
    let normalized = expand_alias_with_typmod(&normalized);
    let lowered = normalized.to_ascii_lowercase();
    let (typmod, base) = if let Some(open) = lowered.find('(') {
        let close = lowered[open..].find(')')? + open;
        let inner = &normalized[open + 1..close];
        let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
        let typmod = match parts.as_slice() {
            [one] => Some((one.parse::<i32>().ok()?, None)),
            [p, s] => Some((p.parse::<i32>().ok()?, Some(s.parse::<i32>().ok()?))),
            _ => return None,
        };
        let base = format!("{}{}", normalized[..open].trim(), &normalized[close + 1..])
            .trim()
            .to_string();
        (typmod, base)
    } else {
        (None, normalized.trim().to_string())
    };
    if base.is_empty() {
        return None;
    }
    // numeric 单参 typmod 即 numeric(p,0)，与 format_type 输出对齐。
    let typmod = if base == "numeric" {
        typmod.map(|(p, s)| (p, Some(s.unwrap_or(0))))
    } else {
        typmod
    };
    Some(ParsedType { base, typmod })
}

/// 带参数的别名展开：`decimal(p,s)`/`dec` → `numeric`、`timestamptz(p)` →
/// `timestamp(p) with time zone`、`timestamp(p)` → `timestamp(p) without time zone`、
/// `char(n)` → `character(n)`。
fn expand_alias_with_typmod(raw: &str) -> String {
    let trimmed = raw.trim();
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("decimal") {
        return format!("numeric{}", &trimmed["decimal".len()..]);
    }
    if lower.starts_with("dec") {
        return format!("numeric{}", &trimmed["dec".len()..]);
    }
    if lower.starts_with("timestamptz") {
        return format!(
            "timestamp{} with time zone",
            &trimmed["timestamptz".len()..]
        );
    }
    if lower.starts_with("timestamp") && !lower.contains("time zone") {
        return format!("{trimmed} without time zone");
    }
    if lower.starts_with("char(") {
        return format!("character{}", &trimmed["char".len()..]);
    }
    trimmed.to_string()
}

/// 白名单外一律 `Rewrite`（宁严勿宽）；解析失败也按 `Rewrite`，交给开闸门。
pub fn classify_type_change(source: &str, target: &str) -> TypeChangeClass {
    use TypeChangeClass::*;
    let (Some(src), Some(tgt)) = (parse_type(source), parse_type(target)) else {
        return Rewrite;
    };
    const VARCHAR: &str = "character varying";
    match (src.base.as_str(), tgt.base.as_str()) {
        (VARCHAR, VARCHAR) => {
            // m ≥ n 或 m 不限：元数据级放宽。
            return match (src.typmod, tgt.typmod) {
                (Some((_n, None)), None) => MetadataOnly,
                (Some((n, None)), Some((m, None))) if m >= n => MetadataOnly,
                _ => Rewrite,
            };
        }
        (VARCHAR, "text") => return MetadataOnly,
        ("text", VARCHAR) if tgt.typmod.is_none() => return MetadataOnly,
        ("numeric", "numeric") => {
            return match (src.typmod, tgt.typmod) {
                // 不限精度 numeric 是任意 (p,s) 的放宽。
                (_, None) => MetadataOnly,
                (None, Some(_)) => Rewrite,
                (Some((p, Some(s))), Some((p2, Some(s2)))) if s2 >= s && p2 - s2 >= p - s => {
                    MetadataOnly
                }
                _ => Rewrite,
            };
        }
        ("timestamp with time zone", "timestamp with time zone")
        | ("timestamp without time zone", "timestamp without time zone") => {
            // 无 typmod 视为默认精度 6（PG timestamp 精度上限）。
            let p = src.typmod.map(|(p, _)| p).unwrap_or(6);
            let p2 = tgt.typmod.map(|(p, _)| p).unwrap_or(6);
            return if p2 >= p { MetadataOnly } else { Rewrite };
        }
        _ => {}
    }
    Rewrite
}

/// 依赖 session `TimeZone` 的转换：即使开闸也 BLOCK（S2「不确定即拒绝」原则）。
pub fn is_nondeterministic_cast(source: &str, target: &str) -> bool {
    let (Some(src), Some(tgt)) = (parse_type(source), parse_type(target)) else {
        return false;
    };
    let pairs = [
        ("timestamp without time zone", "timestamp with time zone"),
        ("timestamp with time zone", "timestamp without time zone"),
        ("date", "timestamp with time zone"),
    ];
    pairs
        .iter()
        .any(|(from, to)| src.base == *from && tgt.base == *to)
}

/// 由解析产物重构目标类型串（base 已另行经 `::regtype` 验证；typmod 只含数字）。
pub fn render_target(target: &str) -> Option<String> {
    let parsed = parse_type(target)?;
    let rendered = match parsed.typmod {
        None => parsed.base,
        Some((p, None)) => {
            // timestamp 的 typmod 在词中间：timestamp(3) with time zone。
            if parsed.base == "timestamp with time zone"
                || parsed.base == "timestamp without time zone"
            {
                let suffix = parsed.base.trim_start_matches("timestamp").to_string();
                format!("timestamp({p}){suffix}")
            } else {
                format!("{}({p})", parsed.base)
            }
        }
        Some((p, Some(s))) => format!("{}({p},{s})", parsed.base),
    };
    Some(rendered)
}

/// 由解析产物重构源类型串，用于漂移比对（与 render_target 同一语义）。
pub fn canonical_type(raw: &str) -> Option<String> {
    render_target(raw)
}

// ── C 组：执行器 ─────────────────────────────────────────

pub fn spec_hash(schema: &str, table: &str, column: &str, source: &str, target: &str) -> String {
    hash_json(&json!({
        "schema": schema,
        "table": table,
        "column": column,
        "source_type": source,
        "target_type": target,
    }))
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
    req: TypeChangeRequest,
) -> Result<TypeChangeJob, OpsError> {
    if req.table.trim().is_empty() {
        return Err(OpsError::Validation("改类型任务需要指定表名".into()));
    }
    if req.column.trim().is_empty() {
        return Err(OpsError::Validation("改类型任务需要指定列名".into()));
    }
    if req.target_type.trim().is_empty() {
        return Err(OpsError::Validation("改类型任务需要指定目标类型".into()));
    }
    // create 不连库（决策表 #8），但目标类型必须能本地解析，挡住纯垃圾输入。
    if parse_type(&req.target_type).is_none() {
        return Err(OpsError::Validation(format!(
            "目标类型无法解析: {}",
            req.target_type
        )));
    }
    let schema = req
        .schema
        .clone()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let at = now();
    let job = TypeChangeJob {
        job_id: format!("tc-{}", uuid::Uuid::new_v4()),
        plan_id: "manual".into(),
        db_id: ctx.db_id.clone(),
        db_name: ctx.db_name.clone(),
        db_fingerprint: ctx.db_fingerprint.clone(),
        schema,
        table: req.table.trim().to_string(),
        column: req.column.trim().to_string(),
        // 首跑 preflight 记录真实列类型后再算指纹。
        source_type: String::new(),
        target_type: req.target_type.trim().to_string(),
        spec_hash: String::new(),
        engine: String::new(),
        status: MigrationStatus::Pending,
        phase: TypeChangePhase::Preflight,
        classification: TypeChangeClass::Rewrite,
        allow_rewrite: req.allow_rewrite,
        production_confirmed: req.production_confirmed,
        execution: TypeChangeExecution::default(),
        verify: TypeChangeVerify::default(),
        run_count: 0,
        blocking: None,
        notes: Vec::new(),
        created_at: at,
        updated_at: at,
        started_at: 0,
        finished_at: None,
        last_error: None,
    };
    hub.insert_typechange(job.clone()).await?;
    Ok(job)
}

/// `ALTER TABLE ... ALTER COLUMN ... TYPE ...`；列/表加引号，目标串来自
/// `render_target` 的解析产物（base 另经 `::regtype` 验证，typmod 只含数字）。
pub fn build_alter_sql(schema: &str, table: &str, column: &str, target: &str) -> String {
    let quoted = quote_ident_pg(column);
    format!(
        "ALTER TABLE {} ALTER COLUMN {} TYPE {target} USING {quoted}::{target}",
        qualified(Some(schema), table),
        quoted
    )
}

pub async fn run_job(
    job_id: &str,
    ctx: &OpsContext,
    hub: &MigrationHub,
    policy: &OpsPolicy,
) -> Result<(), OpsError> {
    let job = hub.get_typechange(job_id).await?;
    if job.status.is_finished() {
        return Ok(());
    }
    if !ctx.is_postgres() {
        return block_typechange(
            ctx,
            hub,
            job_id,
            "改类型执行器只支持 PostgreSQL",
            "preflight",
        )
        .await;
    }
    if ctx.db_name != job.db_name {
        return block_typechange(
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
        return block_typechange(ctx, hub, job_id, &err.to_string(), "policy").await;
    }
    if job.engine.is_empty() {
        let _ = hub
            .update_typechange(job_id, |job| job.engine = ENGINE.to_string())
            .await?;
    } else if job.engine != ENGINE {
        return block_typechange(
            ctx,
            hub,
            job_id,
            &format!("任务引擎为 {}，禁止用 PostgreSQL 续跑", job.engine),
            "preflight:engine_mismatch",
        )
        .await;
    }

    let job = hub
        .update_typechange(job_id, |job| {
            job.status = MigrationStatus::Running;
            job.phase = TypeChangePhase::Preflight;
            job.run_count += 1;
            if job.started_at == 0 {
                job.started_at = now();
            }
            job.finished_at = None;
            job.last_error = None;
            job.blocking = None;
        })
        .await?;
    let _ = append_typechange_audit(
        ctx,
        "field_migration.typechange.started",
        "ok",
        &job,
        json!({
            "job_id": job_id,
            "engine": ENGINE,
            "run_count": job.run_count,
            "table": job.table,
            "column": job.column,
            "target_type": job.target_type,
        }),
    );

    // ── Preflight ──────────────────────────────────────
    let schema = sanitize_schema(job.schema.as_deref());
    let qualified = qualified(Some(&schema), &job.table);

    // 1) 表存在且是普通表（分区表/视图/外表不在 S4 范围）。
    let relkind = table_relkind(ctx, &schema, &job.table).await?;
    let Some(relkind) = relkind else {
        return block_typechange(
            ctx,
            hub,
            job_id,
            &format!("表 {qualified} 不存在"),
            "preflight:missing_table",
        )
        .await;
    };
    if relkind != "r" {
        let reason = match relkind.as_str() {
            "p" => "分区表改类型不在 S4 支持范围（S5 候选）",
            "v" | "m" => "视图不支持改类型",
            _ => "仅支持普通表（relkind='r'）",
        };
        return block_typechange(ctx, hub, job_id, reason, "preflight:unsupported_relation").await;
    }

    // 2) 目标类型本地解析 + 库内 ::regtype 验证。
    let Some(rendered_target) = render_target(&job.target_type) else {
        return block_typechange(
            ctx,
            hub,
            job_id,
            &format!("目标类型无法解析: {}", job.target_type),
            "preflight:bad_target_type",
        )
        .await;
    };
    if let Some(parsed) = parse_type(&job.target_type) {
        if !regtype_exists(ctx, &parsed.base).await? {
            return block_typechange(
                ctx,
                hub,
                job_id,
                &format!("目标类型在库内不存在: {}", parsed.base),
                "preflight:bad_target_type",
            )
            .await;
        }
    }

    // 3) 列存在 + live 类型；记录/比对 source_type（漂移与自愈判定）。
    let Some(col_info) = column_info(ctx, &schema, &job.table, &job.column).await? else {
        return block_typechange(
            ctx,
            hub,
            job_id,
            &format!("表 {qualified} 缺少列 {}", job.column),
            "preflight:missing_column",
        )
        .await;
    };
    let live_type = col_info.coltype.clone();

    // 3a) 列已是目标类型：no-op（首跑直接完成；续跑视为外部已完成，跳 Verifying）。
    if let (Some(live), Some(target)) =
        (canonical_type(&live_type), canonical_type(&job.target_type))
    {
        if live == target {
            let job = hub
                .update_typechange(job_id, |job| {
                    job.status = MigrationStatus::Completed;
                    job.phase = TypeChangePhase::Done;
                    job.finished_at = Some(now());
                    job.last_error = None;
                    job.verify.live_type = Some(live_type.clone());
                    job.notes.push("列已是目标类型，无需执行（no-op）".into());
                })
                .await?;
            let _ = append_typechange_audit(
                ctx,
                "field_migration.typechange.completed",
                "ok",
                &job,
                json!({ "job_id": job_id, "noop": true, "live_type": live_type }),
            );
            return Ok(());
        }
    }

    // 3b) 首跑记录 source_type 与指纹；续跑校验漂移与指纹。
    let mut job = job;
    if job.source_type.is_empty() {
        let fingerprint = spec_hash(
            &schema,
            &job.table,
            &job.column,
            &live_type,
            &job.target_type,
        );
        job = hub
            .update_typechange(job_id, |job| {
                job.source_type = live_type.clone();
                job.spec_hash = fingerprint.clone();
            })
            .await?;
        let _ = append_typechange_audit(
            ctx,
            "field_migration.typechange.spec_recorded",
            "ok",
            &job,
            json!({
                "job_id": job_id,
                "source_type": live_type,
                "spec_hash": fingerprint,
            }),
        );
    } else {
        let expected = spec_hash(
            &schema,
            &job.table,
            &job.column,
            &job.source_type,
            &job.target_type,
        );
        if job.spec_hash != expected {
            return block_typechange(
                ctx,
                hub,
                job_id,
                "任务规格与指纹不一致（表/列/源类型/目标类型被篡改），拒绝续跑",
                "preflight:spec_mismatch",
            )
            .await;
        }
        if live_type != job.source_type {
            return block_typechange(
                ctx,
                hub,
                job_id,
                &format!(
                    "列类型漂移：任务绑定 {}，当前 {}；如需变更请新建任务",
                    job.source_type, live_type
                ),
                "preflight:schema_drift",
            )
            .await;
        }
    }

    // 4) 分类 + 开闸门。
    let classification = classify_type_change(&job.source_type, &job.target_type);
    hub.update_typechange(job_id, |job| job.classification = classification)
        .await?;
    if classification == TypeChangeClass::Rewrite && !job.allow_rewrite {
        let size = relation_size(ctx, &schema, &job.table).await.unwrap_or(0);
        let rows = estimated_rows(ctx, &schema, &job.table).await.unwrap_or(0);
        return block_typechange(
            ctx,
            hub,
            job_id,
            &format!(
                "目标类型 {}{} 触发表重写（ACCESS EXCLUSIVE 锁至完成），需要显式开闸：\
                 allow_rewrite=true 且 production_confirmed。\
                 预估 rows≈{rows}，size={size} bytes；请安排维护窗口或继续使用 S1 脚本。",
                job.target_type, ""
            ),
            "preflight:rewrite_requires_gate",
        )
        .await;
    }

    // 5) 非确定性转换硬阻断（即使开闸）。
    if is_nondeterministic_cast(&job.source_type, &job.target_type) {
        return block_typechange(
            ctx,
            hub,
            job_id,
            "转换依赖会话时区（TimeZone），结果不确定：拒绝执行；\
             S5 提供显式 AT TIME ZONE 方案",
            "preflight:non_deterministic_cast",
        )
        .await;
    }

    // 6) 依赖：视图/规则。
    let views = dependent_views(ctx, &schema, &job.table).await?;
    if !views.is_empty() {
        return block_typechange(
            ctx,
            hub,
            job_id,
            &format!(
                "列被视图依赖（PG 拒绝修改被视图引用的列类型）: {}",
                views.join(", ")
            ),
            "preflight:dependent_views",
        )
        .await;
    }
    // 6b) 依赖：外键（任一方向）。
    let fks = foreign_key_constraints(ctx, &schema, &job.table, col_info.attnum).await?;
    if !fks.is_empty() {
        return block_typechange(
            ctx,
            hub,
            job_id,
            &format!(
                "列参与外键（任一方向），S4 一律阻断（S5 候选）: {}",
                fks.join(", ")
            ),
            "preflight:foreign_keys",
        )
        .await;
    }

    // 7) 属主校验（超级用户通过 pg_has_role 恒真）。
    if !is_table_owner(ctx, &schema, &job.table).await? {
        return block_typechange(
            ctx,
            hub,
            job_id,
            "当前用户不是表属主（或无其角色 USAGE），无法执行 ALTER TYPE",
            "preflight:not_owner",
        )
        .await;
    }

    // 8) 前置快照：行数/索引/默认值（供 verify 比对）。
    let rows_before = table_rows(ctx, &schema, &job.table).await?;
    let index_before = index_health(ctx, &schema, &job.table).await?;
    hub.update_typechange(job_id, |job| {
        job.verify.rows_before = rows_before;
        job.verify.index_count = index_before.total;
        job.verify.default_before = col_info.default_expr.clone();
    })
    .await?;

    // ── Applying ───────────────────────────────────────
    if !wait_control(hub, job_id).await? {
        return abort_typechange(ctx, hub, job_id).await;
    }
    hub.update_typechange(job_id, |job| job.phase = TypeChangePhase::Applying)
        .await?;
    let sql = build_alter_sql(&schema, &job.table, &job.column, &rendered_target);
    let result = exec_ddl_raw(ctx, policy, &sql).await;

    if let Err(err) = result {
        let message = err.to_string();
        let (code, detail) = database_error_parts(&err);
        return match classify_apply_failure(&code, &message) {
            ApplyFailure::LockTimeout => {
                block_typechange(
                    ctx,
                    hub,
                    job_id,
                    &format!("获取锁超时（lock_timeout），可续跑: {message}"),
                    "apply:lock_timeout",
                )
                .await
            }
            ApplyFailure::Conversion => {
                let extra = if detail.is_empty() {
                    String::new()
                } else {
                    format!("（PG DETAIL: {detail}）")
                };
                block_typechange(
                    ctx,
                    hub,
                    job_id,
                    &format!("类型转换失败，存在无法转换的值{extra}: {message}"),
                    "apply:conversion_failed",
                )
                .await
            }
            ApplyFailure::NoCast => {
                block_typechange(
                    ctx,
                    hub,
                    job_id,
                    &format!("无可行转换路径: {message}"),
                    "apply:no_cast",
                )
                .await
            }
            ApplyFailure::DependentObjects => {
                block_typechange(
                    ctx,
                    hub,
                    job_id,
                    &format!("存在依赖对象（约束/索引等）阻碍变更: {message}"),
                    "apply:dependent_objects",
                )
                .await
            }
            ApplyFailure::Privilege => {
                block_typechange(
                    ctx,
                    hub,
                    job_id,
                    &format!("权限不足: {message}"),
                    "apply:insufficient_privilege",
                )
                .await
            }
            ApplyFailure::Other => {
                // 连接中断/未知错误：DDL 原子回滚列本身，续跑 = 重跑语句。
                let job = hub
                    .update_typechange(job_id, |job| {
                        job.status = MigrationStatus::Interrupted;
                        job.finished_at = Some(now());
                        job.last_error = Some(format!("ALTER TYPE 失败: {message}"));
                        job.notes.push("失败：可重新执行（续跑 = 重跑语句）".into());
                    })
                    .await?;
                let _ = append_typechange_audit(
                    ctx,
                    "field_migration.typechange.interrupted",
                    "interrupted",
                    &job,
                    json!({ "job_id": job_id, "error": message }),
                );
                Ok(())
            }
        };
    }

    // ── Verifying ──────────────────────────────────────
    if !wait_control(hub, job_id).await? {
        return abort_typechange(ctx, hub, job_id).await;
    }
    hub.update_typechange(job_id, |job| job.phase = TypeChangePhase::Verifying)
        .await?;
    let live_after = column_info(ctx, &schema, &job.table, &job.column)
        .await?
        .map(|info| info.coltype)
        .unwrap_or_default();
    let canonical_after = canonical_type(&live_after);
    let rows_after = table_rows(ctx, &schema, &job.table).await?;
    let index_after = index_health(ctx, &schema, &job.table).await?;
    let default_after = column_info(ctx, &schema, &job.table, &job.column)
        .await?
        .and_then(|info| info.default_expr);

    let type_ok = canonical_after
        .zip(canonical_type(&job.target_type))
        .map(|(after, target)| after == target)
        .unwrap_or(false);
    let rows_ok = rows_after == rows_before;
    let indexes_ok = index_after.total == index_before.total && index_after.invalid == 0;

    hub.update_typechange(job_id, |job| {
        job.verify.live_type = Some(live_after.clone());
        job.verify.rows_after = rows_after;
        job.verify.indexes_valid = indexes_ok;
        job.verify.default_after = default_after;
    })
    .await?;
    if !(type_ok && rows_ok && indexes_ok) {
        let mut problems = Vec::new();
        if !type_ok {
            problems.push(format!("类型不符: live={live_after}"));
        }
        if !rows_ok {
            problems.push(format!("行数不符: {} -> {rows_after}", rows_before));
        }
        if !indexes_ok {
            problems.push(format!(
                "索引健康不符: {}(invalid={}) -> total={}, invalid={}",
                index_before.total, index_before.invalid, index_after.total, index_after.invalid
            ));
        }
        return block_typechange(
            ctx,
            hub,
            job_id,
            &format!("执行后校验未通过: {}", problems.join("; ")),
            "verify:mismatch",
        )
        .await;
    }

    let job = hub
        .update_typechange(job_id, |job| {
            job.status = MigrationStatus::Completed;
            job.phase = TypeChangePhase::Done;
            job.finished_at = Some(now());
            job.last_error = None;
            job.notes.push(format!(
                "verify: ok, {} -> {rendered_target}, rows={rows_after}",
                job.source_type
            ));
        })
        .await?;
    let _ = append_typechange_audit(
        ctx,
        "field_migration.typechange.completed",
        "ok",
        &job,
        json!({
            "job_id": job_id,
            "table": job.table,
            "column": job.column,
            "source_type": job.source_type,
            "target_type": job.target_type,
            "classification": job.classification.as_str(),
            "rows": rows_after,
        }),
    );
    Ok(())
}

// ── 错误映射（纯函数，供单测） ──────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyFailure {
    LockTimeout,
    Conversion,
    NoCast,
    DependentObjects,
    Privilege,
    Other,
}

/// SQLSTATE 优先，message 兜底（连接中断等拿不到 code 时走 Other → Interrupted）。
pub fn classify_apply_failure(code: &str, message: &str) -> ApplyFailure {
    match code {
        "55P03" => return ApplyFailure::LockTimeout,
        "22P02" | "22001" | "22003" | "22007" | "22008" => return ApplyFailure::Conversion,
        "42804" | "42846" => return ApplyFailure::NoCast,
        "2BP01" => return ApplyFailure::DependentObjects,
        "42501" => return ApplyFailure::Privilege,
        _ => {}
    }
    let lowered = message.to_ascii_lowercase();
    if lowered.contains("lock timeout") || lowered.contains("lock_not_available") {
        ApplyFailure::LockTimeout
    } else if lowered.contains("out of range")
        || lowered.contains("invalid input syntax")
        || lowered.contains("value too long")
        || lowered.contains("date/time field overflow")
        || lowered.contains("cannot be cast")
        || lowered.contains("conversion failed")
    {
        ApplyFailure::Conversion
    } else {
        ApplyFailure::Other
    }
}

fn database_error_parts(err: &sqlx::Error) -> (String, String) {
    if let Some(db_err) = err.as_database_error() {
        let code = db_err.code().map(|c| c.to_string()).unwrap_or_default();
        let detail = db_err
            .downcast_ref::<PgDatabaseError>()
            .detail()
            .unwrap_or_default()
            .to_string();
        (code, detail)
    } else {
        (String::new(), String::new())
    }
}

// ── 基础设施 ─────────────────────────────────────────────

struct ColumnInfo {
    coltype: String,
    attnum: i16,
    default_expr: Option<String>,
}

async fn table_relkind(
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

async fn regtype_exists(ctx: &OpsContext, base: &str) -> Result<bool, OpsError> {
    let result: Result<i32, _> = sqlx::query_scalar("SELECT $1::regtype::oid::int4")
        .bind(base)
        .fetch_one(ctx.db_client.pool.pg()?)
        .await;
    match result {
        Ok(_) => Ok(true),
        Err(sqlx::Error::Database(_)) => Ok(false),
        Err(err) => Err(err.into()),
    }
}

async fn column_info(
    ctx: &OpsContext,
    schema: &str,
    table: &str,
    column: &str,
) -> Result<Option<ColumnInfo>, OpsError> {
    let qualified = qualified(Some(schema), table);
    let rows = sqlx::query(
        "SELECT format_type(a.atttypid, a.atttypmod)::text AS coltype, \
                a.attnum::int2::text AS attnum, \
                pg_get_expr(d.adbin, d.adrelid) AS default_expr \
         FROM pg_attribute a \
         LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
         WHERE a.attrelid = to_regclass($1) AND a.attname = $2 AND NOT a.attisdropped",
    )
    .bind(&qualified)
    .bind(column)
    .fetch_all(ctx.db_client.pool.pg()?)
    .await?;
    let rows = convert_pg_rows(&rows);
    Ok(rows.first().map(|row| ColumnInfo {
        coltype: row.get("coltype").cloned().unwrap_or_default(),
        attnum: row
            .get("attnum")
            .and_then(|v| v.trim().parse::<i16>().ok())
            .unwrap_or(0),
        default_expr: row.get("default_expr").cloned(),
    }))
}

async fn dependent_views(
    ctx: &OpsContext,
    schema: &str,
    table: &str,
) -> Result<Vec<String>, OpsError> {
    let qualified = qualified(Some(schema), table);
    let rows = sqlx::query(
        "SELECT DISTINCT v.relname::text AS name \
         FROM pg_depend d \
         JOIN pg_rewrite r ON r.oid = d.objid \
         JOIN pg_class v ON v.oid = r.ev_class \
         WHERE d.refobjid = to_regclass($1) AND d.deptype = 'n' \
           AND v.relkind = 'v' AND v.oid <> to_regclass($1)",
    )
    .bind(&qualified)
    .fetch_all(ctx.db_client.pool.pg()?)
    .await?;
    let rows = convert_pg_rows(&rows);
    Ok(rows
        .iter()
        .filter_map(|row| row.get("name").cloned())
        .collect())
}

async fn foreign_key_constraints(
    ctx: &OpsContext,
    schema: &str,
    table: &str,
    attnum: i16,
) -> Result<Vec<String>, OpsError> {
    let qualified = qualified(Some(schema), table);
    let rows = sqlx::query(
        "SELECT con.conname::text AS name \
         FROM pg_constraint con \
         WHERE con.contype = 'f' \
           AND ((con.conrelid = to_regclass($1) AND $2 = ANY (con.conkey)) \
             OR (con.confrelid = to_regclass($1) AND $2 = ANY (con.confkey)))",
    )
    .bind(&qualified)
    .bind(attnum)
    .fetch_all(ctx.db_client.pool.pg()?)
    .await?;
    let rows = convert_pg_rows(&rows);
    Ok(rows
        .iter()
        .filter_map(|row| row.get("name").cloned())
        .collect())
}

async fn is_table_owner(ctx: &OpsContext, schema: &str, table: &str) -> Result<bool, OpsError> {
    let rows = sqlx::query(
        "SELECT pg_has_role(current_user, c.relowner, 'USAGE')::text AS ok \
         FROM pg_class c WHERE c.oid = to_regclass($1)",
    )
    .bind(&qualified(Some(schema), table))
    .fetch_one(ctx.db_client.pool.pg()?)
    .await?;
    let rows = convert_pg_rows(&[rows]);
    Ok(rows
        .first()
        .and_then(|row| row.get("ok").cloned())
        .map(|v| v == "true")
        .unwrap_or(false))
}

async fn table_rows(ctx: &OpsContext, schema: &str, table: &str) -> Result<u64, OpsError> {
    let qualified = qualified(Some(schema), table);
    let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {qualified}"))
        .fetch_one(ctx.db_client.pool.pg()?)
        .await?;
    Ok(count.max(0) as u64)
}

async fn estimated_rows(ctx: &OpsContext, schema: &str, table: &str) -> Result<u64, OpsError> {
    let rows = sqlx::query(
        "SELECT c.reltuples::bigint::text AS rows \
         FROM pg_class c WHERE c.oid = to_regclass($1)",
    )
    .bind(&qualified(Some(schema), table))
    .fetch_one(ctx.db_client.pool.pg()?)
    .await?;
    let rows = convert_pg_rows(&[rows]);
    Ok(rows
        .first()
        .and_then(|row| row.get("rows").cloned())
        .and_then(|v| v.trim().parse::<i64>().ok())
        .map(|v| v.max(0) as u64)
        .unwrap_or(0))
}

async fn relation_size(ctx: &OpsContext, schema: &str, table: &str) -> Result<u64, OpsError> {
    let size: i64 = sqlx::query_scalar("SELECT pg_relation_size(to_regclass($1))")
        .bind(&qualified(Some(schema), table))
        .fetch_one(ctx.db_client.pool.pg()?)
        .await?;
    Ok(size.max(0) as u64)
}

#[derive(Debug, Clone, Copy)]
struct IndexHealth {
    total: u64,
    invalid: u64,
}

async fn index_health(
    ctx: &OpsContext,
    schema: &str,
    table: &str,
) -> Result<IndexHealth, OpsError> {
    let rows = sqlx::query(
        "SELECT COUNT(*)::text AS total, \
                COUNT(*) FILTER (WHERE NOT i.indisvalid)::text AS invalid \
         FROM pg_index i WHERE i.indrelid = to_regclass($1)",
    )
    .bind(&qualified(Some(schema), table))
    .fetch_one(ctx.db_client.pool.pg()?)
    .await?;
    let rows = convert_pg_rows(&[rows]);
    let row = rows.first().cloned().unwrap_or_default();
    let parse = |value: Option<&String>| {
        value
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(0)
    };
    Ok(IndexHealth {
        total: parse(row.get("total")),
        invalid: parse(row.get("invalid")),
    })
}

async fn wait_control(hub: &MigrationHub, job_id: &str) -> Result<bool, OpsError> {
    loop {
        match hub.control_value(job_id).await {
            CONTROL_ABORT => return Ok(false),
            CONTROL_PAUSE => {
                let job = hub.get_typechange(job_id).await?;
                if matches!(
                    job.status,
                    MigrationStatus::Running | MigrationStatus::Pending
                ) {
                    hub.update_typechange(job_id, |job| job.status = MigrationStatus::Paused)
                        .await?;
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            _ => {
                let job = hub.get_typechange(job_id).await?;
                if job.status == MigrationStatus::Paused {
                    hub.update_typechange(job_id, |job| {
                        job.status = MigrationStatus::Running;
                    })
                    .await?;
                }
                return Ok(true);
            }
        }
    }
}

async fn abort_typechange(
    ctx: &OpsContext,
    hub: &MigrationHub,
    job_id: &str,
) -> Result<(), OpsError> {
    let job = hub
        .update_typechange(job_id, |job| {
            job.status = MigrationStatus::Aborted;
            job.finished_at = Some(now());
        })
        .await?;
    let _ = append_typechange_audit(
        ctx,
        "field_migration.typechange.aborted",
        "aborted",
        &job,
        json!({ "job_id": job_id }),
    );
    Ok(())
}

async fn block_typechange(
    ctx: &OpsContext,
    hub: &MigrationHub,
    job_id: &str,
    message: &str,
    phase: &str,
) -> Result<(), OpsError> {
    let job = hub
        .update_typechange(job_id, |job| {
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
    let _ = append_typechange_audit(
        ctx,
        "field_migration.typechange.blocked",
        "blocked",
        &job,
        json!({ "job_id": job_id, "phase": phase, "message": message }),
    );
    Ok(())
}

fn append_typechange_audit(
    ctx: &OpsContext,
    event: &str,
    outcome: &str,
    job: &TypeChangeJob,
    detail: serde_json::Value,
) -> Result<(), OpsError> {
    let Some(audit) = ctx.audit.as_ref() else {
        return Ok(());
    };
    let mut record = AuditRecord::new(event, outcome);
    record.plan_id = Some(job.plan_id.clone());
    record.db_id = Some(job.db_id.clone());
    record.db_name = Some(job.db_name.clone());
    record.action_id = Some(format!("field_migration.typechange:{}", job.job_id));
    record.actor = Some("local-user".into());
    record.detail = detail;
    audit.append(&record)
}

#[cfg(test)]
mod tests {
    use super::*;

    use TypeChangeClass::*;

    #[test]
    fn varchar_widening_is_metadata_only() {
        assert_eq!(
            classify_type_change("character varying(50)", "character varying(200)"),
            MetadataOnly
        );
        // m == n 也算放宽（无变化，run 阶段会先 no-op）。
        assert_eq!(
            classify_type_change("character varying(50)", "character varying(50)"),
            MetadataOnly
        );
        // 不限长度。
        assert_eq!(
            classify_type_change("character varying(50)", "character varying"),
            MetadataOnly
        );
        // 别名归一。
        assert_eq!(classify_type_change("varchar(50)", "text"), MetadataOnly);
        assert_eq!(
            classify_type_change("varchar(50)", "varchar(200)"),
            MetadataOnly
        );
        // 缩窄必须开闸。
        assert_eq!(
            classify_type_change("character varying(200)", "character varying(50)"),
            Rewrite
        );
        // text -> varchar(n) 需要校验长度，按 Rewrite。
        assert_eq!(
            classify_type_change("text", "character varying(10)"),
            Rewrite
        );
        // text -> varchar(不限) 二进制兼容。
        assert_eq!(
            classify_type_change("text", "character varying"),
            MetadataOnly
        );
    }

    #[test]
    fn numeric_typmod_relax_is_metadata_only() {
        // s 放宽 + 整数位不缩。
        assert_eq!(
            classify_type_change("numeric(10,2)", "numeric(14,4)"),
            MetadataOnly
        );
        // 整数位缩了：10-2=8 > 12-6=6。
        assert_eq!(
            classify_type_change("numeric(10,2)", "numeric(12,6)"),
            Rewrite
        );
        // scale 缩了。
        assert_eq!(
            classify_type_change("numeric(10,6)", "numeric(14,4)"),
            Rewrite
        );
        // 不限精度是放宽。
        assert_eq!(
            classify_type_change("numeric(10,2)", "numeric"),
            MetadataOnly
        );
        // 不限 -> 限是缩窄。
        assert_eq!(classify_type_change("numeric", "numeric(10,2)"), Rewrite);
        // 单参 numeric(p) 即 numeric(p,0)。
        assert_eq!(
            classify_type_change("numeric(10)", "numeric(12,2)"),
            MetadataOnly
        );
        // 别名归一。
        assert_eq!(
            classify_type_change("decimal(10,2)", "numeric(14,4)"),
            MetadataOnly
        );
    }

    #[test]
    fn timestamp_precision_relax_is_metadata_only() {
        assert_eq!(
            classify_type_change(
                "timestamp(3) without time zone",
                "timestamp(6) without time zone"
            ),
            MetadataOnly
        );
        // 无 typmod 视为默认精度 6。
        assert_eq!(
            classify_type_change("timestamp(3)", "timestamp"),
            MetadataOnly
        );
        // 精度缩窄。
        assert_eq!(
            classify_type_change("timestamp(6)", "timestamp(3)"),
            Rewrite
        );
        // 别名带 typmod。
        assert_eq!(
            classify_type_change("timestamp(3) with time zone", "timestamptz(6)"),
            MetadataOnly
        );
        // 时区属性变化不在白名单（另有非确定性硬阻断）。
        assert_eq!(
            classify_type_change("timestamp(3) with time zone", "timestamp(3)"),
            Rewrite
        );
    }

    #[test]
    fn non_whitelisted_and_unparseable_types_fall_back_to_rewrite() {
        assert_eq!(classify_type_change("integer", "bigint"), Rewrite);
        assert_eq!(classify_type_change("jsonb", "text"), Rewrite);
        assert_eq!(
            classify_type_change("integer", "character varying(10)"),
            Rewrite
        );
        // 解析失败按 Rewrite（宁严勿宽，交给开闸门）。
        assert_eq!(classify_type_change("weird(", "text"), Rewrite);
        assert_eq!(classify_type_change("text", "1abc(3)"), Rewrite);
    }

    #[test]
    fn nondeterministic_casts_are_recognized() {
        assert!(is_nondeterministic_cast(
            "timestamp without time zone",
            "timestamp with time zone"
        ));
        assert!(is_nondeterministic_cast(
            "timestamptz",
            "timestamp without time zone"
        ));
        assert!(is_nondeterministic_cast("date", "timestamp with time zone"));
        // date -> timestamp(无时区) 不依赖会话时区。
        assert!(!is_nondeterministic_cast(
            "date",
            "timestamp without time zone"
        ));
        assert!(!is_nondeterministic_cast("integer", "bigint"));
    }

    #[test]
    fn render_target_reconstructs_canonical_forms() {
        assert_eq!(
            render_target("varchar(200)").as_deref(),
            Some("character varying(200)")
        );
        assert_eq!(render_target("text").as_deref(), Some("text"));
        assert_eq!(
            render_target("numeric(14,4)").as_deref(),
            Some("numeric(14,4)")
        );
        // timestamp 的 typmod 必须放回词中间。
        assert_eq!(
            render_target("timestamptz(6)").as_deref(),
            Some("timestamp(6) with time zone")
        );
        assert_eq!(
            render_target("timestamp(3)").as_deref(),
            Some("timestamp(3) without time zone")
        );
        assert_eq!(render_target("int8").as_deref(), Some("bigint"));
        assert_eq!(render_target("garbage("), None);
    }

    #[test]
    fn alter_sql_quotes_identifiers_and_binds_typmod() {
        let sql = build_alter_sql("public", "orders", "payload", "character varying(200)");
        assert_eq!(
            sql,
            "ALTER TABLE \"public\".\"orders\" ALTER COLUMN \"payload\" \
             TYPE character varying(200) USING \"payload\"::character varying(200)"
        );
        // 表/列名带引号字符不注入。
        let sql = build_alter_sql("s", "t; DROP TABLE x; --", "c", "text");
        assert!(sql.contains("\"t; DROP TABLE x; --\""));
        assert!(!sql.contains("; DROP TABLE x; --;"));
    }

    #[test]
    fn apply_failures_map_by_sqlstate_then_message() {
        assert_eq!(
            classify_apply_failure("55P03", "anything"),
            ApplyFailure::LockTimeout
        );
        assert_eq!(
            classify_apply_failure("22P02", "invalid input syntax"),
            ApplyFailure::Conversion
        );
        assert_eq!(
            classify_apply_failure("22001", "value too long"),
            ApplyFailure::Conversion
        );
        assert_eq!(
            classify_apply_failure("42846", "cannot be cast"),
            ApplyFailure::NoCast
        );
        assert_eq!(
            classify_apply_failure("2BP01", "dependent objects"),
            ApplyFailure::DependentObjects
        );
        assert_eq!(
            classify_apply_failure("42501", "permission denied"),
            ApplyFailure::Privilege
        );
        // 连接中断等拿不到 code。
        assert_eq!(
            classify_apply_failure("", "connection closed"),
            ApplyFailure::Other
        );
        // message 兜底。
        assert_eq!(
            classify_apply_failure("", "canceling statement due to lock timeout"),
            ApplyFailure::LockTimeout
        );
        assert_eq!(
            classify_apply_failure("", "value too long for type"),
            ApplyFailure::Conversion
        );
    }

    #[test]
    fn spec_hash_binds_all_four_identifiers() {
        let base = spec_hash("public", "orders", "payload", "text", "varchar(200)");
        assert_ne!(
            base,
            spec_hash("public", "orders", "other", "text", "varchar(200)")
        );
        assert_ne!(
            base,
            spec_hash("public", "orders", "payload", "varchar(50)", "varchar(200)")
        );
        assert_ne!(
            base,
            spec_hash("public", "orders", "payload", "text", "varchar(100)")
        );
        assert_ne!(
            base,
            spec_hash("other", "orders", "payload", "text", "varchar(200)")
        );
    }
}
