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
    BlockingDetail, FkCounterpartSpec, FkPlanEntry, MigrationHub, MigrationStatus, TypeChangeClass,
    TypeChangeExecution, TypeChangeJob, TypeChangePhase, TypeChangeVerify, CONTROL_ABORT,
    CONTROL_PAUSE,
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
    /// freeform USING 表达式（S5）；提供即按 Rewrite 开闸并纳入指纹。
    pub using_expr: Option<String>,
    /// FK 拆装编排的对端列（S5）；空 = 无 FK 编排。
    pub fk_counterparts: Vec<FkCounterpartSpec>,
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

pub fn spec_hash(
    schema: &str,
    table: &str,
    column: &str,
    source: &str,
    target: &str,
    using_expr: Option<&str>,
    fk_plan: &[FkPlanEntry],
) -> String {
    hash_json(&json!({
        "schema": schema,
        "table": table,
        "column": column,
        "source_type": source,
        "target_type": target,
        "using_expr": using_expr,
        "fk_plan": fk_plan,
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
    // freeform USING 的本地门：长度与单语句约束（解析干跑在 preflight）。
    let using_expr = req
        .using_expr
        .as_ref()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    if let Some(expr) = using_expr.as_ref() {
        if expr.len() > 2000 {
            return Err(OpsError::Validation(
                "USING 表达式过长（上限 2000 字符）".into(),
            ));
        }
        if expr.contains(';') {
            return Err(OpsError::Validation(
                "USING 表达式必须为单条表达式（不允许分号）".into(),
            ));
        }
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
        using_expr,
        fk_counterparts: req.fk_counterparts.clone(),
        spec_hash: String::new(),
        engine: String::new(),
        status: MigrationStatus::Pending,
        phase: TypeChangePhase::Preflight,
        classification: TypeChangeClass::Rewrite,
        allow_rewrite: req.allow_rewrite,
        production_confirmed: req.production_confirmed,
        execution: TypeChangeExecution::default(),
        verify: TypeChangeVerify::default(),
        fk_plan: Vec::new(),
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
    build_alter_sql_with_using(schema, table, column, target, None)
}

/// 自定义 `USING` 版本：表达式由调用方保证已过 `preflight:bad_using_expr` 干跑。
pub fn build_alter_sql_with_using(
    schema: &str,
    table: &str,
    column: &str,
    target: &str,
    using_expr: Option<&str>,
) -> String {
    let quoted = quote_ident_pg(column);
    let using = match using_expr {
        Some(expr) => expr.to_string(),
        None => format!("{quoted}::{target}"),
    };
    format!(
        "ALTER TABLE {} ALTER COLUMN {} TYPE {target} USING {using}",
        qualified(Some(schema), table),
        quoted
    )
}

/// USING 解析干跑（门 1）：只做解析/计划，不执行、不评估数据。
async fn dry_run_using(
    ctx: &OpsContext,
    schema: &str,
    table: &str,
    expr: &str,
) -> Result<(), OpsError> {
    let sql = format!(
        "SELECT ({expr}) FROM {} LIMIT 0",
        qualified(Some(schema), table)
    );
    let pool = ctx.db_client.pool.pg()?;
    sqlx::query(&sql).fetch_all(pool).await?;
    Ok(())
}

/// 分区叶子数量（递归 `pg_inherits`）；非分区表返回 0。
async fn partition_count(ctx: &OpsContext, schema: &str, table: &str) -> Result<u64, OpsError> {
    let rows = sqlx::query(
        "WITH RECURSIVE tree AS ( \
             SELECT c.oid, c.relkind FROM pg_class c WHERE c.oid = to_regclass($1) \
             UNION ALL \
             SELECT child.oid, child.relkind FROM pg_inherits i \
             JOIN pg_class child ON child.oid = i.inhrelid \
             JOIN tree parent ON parent.oid = i.inhparent \
         ) \
         SELECT COUNT(*)::text AS count FROM tree WHERE relkind = 'r'",
    )
    .bind(qualified(Some(schema), table))
    .fetch_one(ctx.db_client.pool.pg()?)
    .await?;
    let rows = convert_pg_rows(&[rows]);
    Ok(rows
        .first()
        .and_then(|row| row.get("count"))
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(0))
}

/// 列是否属于分区键（`pg_partitioned_table.partattrs`）。
async fn is_partition_key(
    ctx: &OpsContext,
    schema: &str,
    table: &str,
    column: &str,
) -> Result<bool, OpsError> {
    let rows = sqlx::query(
        "SELECT COUNT(*)::text AS count \
         FROM pg_partitioned_table pt \
         JOIN generate_series(0, pt.partnatts - 1) AS k(ord) ON true \
         JOIN pg_attribute a ON a.attrelid = pt.partrelid AND a.attnum = pt.partattrs[k.ord] \
         WHERE pt.partrelid = to_regclass($1) AND a.attname = $2",
    )
    .bind(qualified(Some(schema), table))
    .bind(column)
    .fetch_one(ctx.db_client.pool.pg()?)
    .await?;
    let rows = convert_pg_rows(&[rows]);
    Ok(rows
        .first()
        .and_then(|row| row.get("count"))
        .and_then(|value| value.trim().parse::<i64>().ok())
        .unwrap_or(0)
        > 0)
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
    let qualified_table = qualified(Some(&schema), &job.table);

    // 1) 表存在且是普通表或分区父表（S5：分区父表单语句级联全部分区）。
    let relkind = table_relkind(ctx, &schema, &job.table).await?;
    let Some(relkind) = relkind else {
        return block_typechange(
            ctx,
            hub,
            job_id,
            &format!("表 {qualified_table} 不存在"),
            "preflight:missing_table",
        )
        .await;
    };
    if relkind != "r" && relkind != "p" {
        let reason = match relkind.as_str() {
            "v" | "m" => "视图不支持改类型",
            _ => "仅支持普通表或分区父表（relkind='r'/'p'）",
        };
        return block_typechange(ctx, hub, job_id, reason, "preflight:unsupported_relation").await;
    }
    if relkind == "p" {
        // PG 硬限制：分区键列的类型不可修改（会破坏分区路由与分区边界表达式）。
        // 实测报错 `cannot alter column ... because it is part of the partition key`，
        // 必须在写之前拦下，否则任务跑到 Applying 才失败。
        if is_partition_key(ctx, &schema, &job.table, &job.column).await? {
            return block_typechange(
                ctx,
                hub,
                job_id,
                &format!(
                    "列 {} 是分区键：PostgreSQL 不允许修改分区键列的类型；\
                     需新建表 + 全量搬迁 + 切换分区方案",
                    job.column
                ),
                "preflight:partition_key",
            )
            .await;
        }
        let partitions = partition_count(ctx, &schema, &job.table).await?;
        let job = hub
            .update_typechange(job_id, |job| {
                job.notes.push(format!(
                    "分区父表：ALTER TYPE 单语句级联 {partitions} 个分区，\
                     锁范围 = 父表 + 全部分区 ACCESS EXCLUSIVE，重写成本按全部分区总量计"
                ));
            })
            .await?;
        let _ = &job;
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
            &format!("表 {qualified_table} 缺少列 {}", job.column),
            "preflight:missing_column",
        )
        .await;
    };
    let live_type = col_info.coltype.clone();

    let live_is_target = canonical_type(&live_type)
        .zip(canonical_type(&job.target_type))
        .map(|(live, target)| live == target)
        .unwrap_or(false);
    let fk_pending = job.fk_plan.iter().any(|entry| !entry.validated);

    // 3a) 列已是目标类型且无未完成 FK 编排：no-op 完成。
    //     （若 FK 编排中断在 ALTER 之后，则不能 no-op，必须继续重建 FK。）
    if live_is_target && !fk_pending {
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

    // 3b) freeform USING 解析干跑（S5 门 1）：只解析/计划，不执行、不评估数据。
    if let Some(expr) = job.using_expr.clone() {
        if let Err(err) = dry_run_using(ctx, &schema, &job.table, &expr).await {
            return block_typechange(
                ctx,
                hub,
                job_id,
                &format!("USING 表达式无法解析或引用无效（{err}）：{expr}"),
                "preflight:bad_using_expr",
            )
            .await;
        }
        if !job.fk_plan.is_empty() {
            return block_typechange(
                ctx,
                hub,
                job_id,
                "FK 编排与自定义 USING 不能同时使用：对端列无法安全复用同一表达式，\
                 请先做类型变更、再单独编排 FK",
                "preflight:fk_with_custom_using",
            )
            .await;
        }
    }

    // 3c) FK 拆装计划：首跑枚举并校验对端声明；续跑复用已存计划。
    let mut job = job;
    let plan = match plan_fk_or_block(
        ctx,
        &schema,
        &job.table,
        &job.column,
        &job.source_type,
        &live_type,
        &job.target_type,
        &job.fk_plan,
        &job.fk_counterparts,
        col_info.attnum,
    )
    .await?
    {
        FkPlanOutcome::Ready(plan) => plan,
        FkPlanOutcome::Blocked(message, phase) => {
            return block_typechange(ctx, hub, job_id, &message, &phase).await
        }
    };
    if plan != job.fk_plan {
        job = hub
            .update_typechange(job_id, |job| job.fk_plan = plan.clone())
            .await?;
    }

    // 3d) 首跑记录 source_type 与指纹（含 using_expr / fk_plan）；续跑校验。
    if job.source_type.is_empty() {
        let fingerprint = spec_hash(
            &schema,
            &job.table,
            &job.column,
            &live_type,
            &job.target_type,
            job.using_expr.as_deref(),
            &job.fk_plan,
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
                "using_expr": job.using_expr,
                "fk_constraints": job.fk_plan.iter().map(|entry| entry.constraint.clone()).collect::<Vec<_>>(),
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
            job.using_expr.as_deref(),
            &job.fk_plan,
        );
        if job.spec_hash != expected {
            return block_typechange(
                ctx,
                hub,
                job_id,
                "任务规格与指纹不一致（表/列/源类型/目标类型/USING/FK 计划被篡改），拒绝续跑",
                "preflight:spec_mismatch",
            )
            .await;
        }
        // 中断在 ALTER 之后：live 已等于目标且 FK 未完成，允许继续重建 FK。
        let interrupted_after_alter = live_is_target && fk_pending;
        if live_type != job.source_type && !interrupted_after_alter {
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

    // 4) 分类 + 开闸门。自定义 USING 无法静态判定免重写，一律按 Rewrite 处理。
    let classification = if job.using_expr.is_some() {
        TypeChangeClass::Rewrite
    } else {
        classify_type_change(&job.source_type, &job.target_type)
    };
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

    // 5) 非确定性转换硬阻断（提供自定义 USING 时由用户显式承担时区语义）。
    if job.using_expr.is_none() && is_nondeterministic_cast(&job.source_type, &job.target_type) {
        return block_typechange(
            ctx,
            hub,
            job_id,
            "转换依赖会话时区（TimeZone），结果不确定：拒绝执行；\
             请提供显式 USING（如 col AT TIME ZONE 'UTC'）并开闸",
            "preflight:non_deterministic_cast",
        )
        .await;
    }

    // 6) 依赖：视图/规则（外键由 3c 的拆装计划处理）。
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
    if job.fk_plan.is_empty() {
        // 单列路径（S4 行为）：一条 ALTER，可选 freeform USING。
        let sql = build_alter_sql_with_using(
            &schema,
            &job.table,
            &job.column,
            &rendered_target,
            job.using_expr.as_deref(),
        );
        if let Err(err) = exec_ddl_raw(ctx, policy, &sql).await {
            return handle_apply_failure(ctx, hub, job_id, "alter_column", err).await;
        }
    } else {
        // S5 FK 编排：DROP FK → ALTER 主列 → ALTER 对端列 → ADD NOT VALID → VALIDATE。
        for entry in job.fk_plan.clone() {
            let sql = format!(
                "ALTER TABLE {} DROP CONSTRAINT IF EXISTS {}",
                qualified(Some(&entry.referencing_schema), &entry.referencing_table),
                quote_ident_pg(&entry.constraint)
            );
            if let Err(err) = exec_ddl_raw(ctx, policy, &sql).await {
                return handle_apply_failure(
                    ctx,
                    hub,
                    job_id,
                    &format!("fk_drop:{}", entry.constraint),
                    err,
                )
                .await;
            }
            hub.update_typechange(job_id, |job| {
                if let Some(item) = job
                    .fk_plan
                    .iter_mut()
                    .find(|item| item.constraint == entry.constraint)
                {
                    item.dropped = true;
                }
            })
            .await?;
        }

        if !live_is_target {
            let sql = build_alter_sql(&schema, &job.table, &job.column, &rendered_target);
            if let Err(err) = exec_ddl_raw(ctx, policy, &sql).await {
                return handle_apply_failure(ctx, hub, job_id, "alter_column", err).await;
            }
        }

        for entry in job.fk_plan.clone() {
            // 主列是引用侧还是被引用侧决定对端列位置。
            let (cp_schema, cp_table, cp_column) =
                if entry.referenced_table == job.table && entry.referenced_column == job.column {
                    (
                        entry.referencing_schema.clone(),
                        entry.referencing_table.clone(),
                        entry.referencing_column.clone(),
                    )
                } else {
                    (
                        entry.referenced_schema.clone(),
                        entry.referenced_table.clone(),
                        entry.referenced_column.clone(),
                    )
                };
            let cp_live = column_info(ctx, &cp_schema, &cp_table, &cp_column)
                .await?
                .map(|info| info.coltype)
                .unwrap_or_default();
            if canonical_type(&cp_live) != canonical_type(&rendered_target) {
                let sql = build_alter_sql(&cp_schema, &cp_table, &cp_column, &rendered_target);
                if let Err(err) = exec_ddl_raw(ctx, policy, &sql).await {
                    return handle_apply_failure(
                        ctx,
                        hub,
                        job_id,
                        &format!("alter_counterpart:{cp_table}.{cp_column}"),
                        err,
                    )
                    .await;
                }
            }
            hub.update_typechange(job_id, |job| {
                if let Some(item) = job
                    .fk_plan
                    .iter_mut()
                    .find(|item| item.constraint == entry.constraint)
                {
                    item.altered = true;
                }
            })
            .await?;
        }

        for entry in job.fk_plan.clone() {
            if entry.recreated {
                continue;
            }
            let sql = format!(
                "ALTER TABLE {} ADD CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({}) NOT VALID",
                qualified(Some(&entry.referencing_schema), &entry.referencing_table),
                quote_ident_pg(&entry.constraint),
                quote_ident_pg(&entry.referencing_column),
                qualified(Some(&entry.referenced_schema), &entry.referenced_table),
                quote_ident_pg(&entry.referenced_column)
            );
            if let Err(err) = exec_ddl_raw(ctx, policy, &sql).await {
                return handle_apply_failure(
                    ctx,
                    hub,
                    job_id,
                    &format!("fk_readd:{}", entry.constraint),
                    err,
                )
                .await;
            }
            hub.update_typechange(job_id, |job| {
                if let Some(item) = job
                    .fk_plan
                    .iter_mut()
                    .find(|item| item.constraint == entry.constraint)
                {
                    item.recreated = true;
                }
            })
            .await?;
        }

        for entry in job.fk_plan.clone() {
            if entry.validated {
                continue;
            }
            let sql = format!(
                "ALTER TABLE {} VALIDATE CONSTRAINT {}",
                qualified(Some(&entry.referencing_schema), &entry.referencing_table),
                quote_ident_pg(&entry.constraint)
            );
            if let Err(err) = exec_ddl_raw(ctx, policy, &sql).await {
                return handle_apply_failure(
                    ctx,
                    hub,
                    job_id,
                    &format!("fk_validate:{}", entry.constraint),
                    err,
                )
                .await;
            }
            hub.update_typechange(job_id, |job| {
                if let Some(item) = job
                    .fk_plan
                    .iter_mut()
                    .find(|item| item.constraint == entry.constraint)
                {
                    item.validated = true;
                }
            })
            .await?;
        }
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
    // S5：FK 编排结果（约束存在 + convalidated + 对端同型）。
    let fk_problems =
        fk_plan_problems(ctx, &job.fk_plan, &job.table, &job.column, &job.target_type).await?;
    let fk_ok = fk_problems.is_empty();

    hub.update_typechange(job_id, |job| {
        job.verify.live_type = Some(live_after.clone());
        job.verify.rows_after = rows_after;
        job.verify.indexes_valid = indexes_ok;
        job.verify.default_after = default_after;
    })
    .await?;
    if !(type_ok && rows_ok && indexes_ok && fk_ok) {
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
        problems.extend(fk_problems);
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

struct FkDetail {
    constraint: String,
    referencing_schema: String,
    referencing_table: String,
    referencing_column: String,
    referenced_schema: String,
    referenced_table: String,
    referenced_column: String,
    key_count: i64,
    /// 本表是该 FK 的被引用侧（true）还是引用侧（false）。
    our_side_referenced: bool,
}

async fn foreign_key_details(
    ctx: &OpsContext,
    schema: &str,
    table: &str,
    attnum: i16,
) -> Result<Vec<FkDetail>, OpsError> {
    let qualified = qualified(Some(schema), table);
    let rows = sqlx::query(
        "SELECT con.conname::text AS name, \
                rn.nspname::text AS referencing_schema, rc.relname::text AS referencing_table, \
                ra.attname::text AS referencing_column, \
                fn.nspname::text AS referenced_schema, fc.relname::text AS referenced_table, \
                fa.attname::text AS referenced_column, \
                COALESCE(array_length(con.conkey, 1), 0)::text AS key_count, \
                (con.confrelid = to_regclass($1))::text AS our_side_referenced \
         FROM pg_constraint con \
         JOIN pg_class rc ON rc.oid = con.conrelid \
         JOIN pg_namespace rn ON rn.oid = rc.relnamespace \
         JOIN pg_class fc ON fc.oid = con.confrelid \
         JOIN pg_namespace fn ON fn.oid = fc.relnamespace \
         JOIN pg_attribute ra ON ra.attrelid = con.conrelid AND ra.attnum = con.conkey[1] \
         JOIN pg_attribute fa ON fa.attrelid = con.confrelid AND fa.attnum = con.confkey[1] \
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
        .map(|row| FkDetail {
            constraint: row.get("name").cloned().unwrap_or_default(),
            referencing_schema: row.get("referencing_schema").cloned().unwrap_or_default(),
            referencing_table: row.get("referencing_table").cloned().unwrap_or_default(),
            referencing_column: row.get("referencing_column").cloned().unwrap_or_default(),
            referenced_schema: row.get("referenced_schema").cloned().unwrap_or_default(),
            referenced_table: row.get("referenced_table").cloned().unwrap_or_default(),
            referenced_column: row.get("referenced_column").cloned().unwrap_or_default(),
            key_count: row
                .get("key_count")
                .and_then(|value| value.trim().parse::<i64>().ok())
                .unwrap_or(0),
            our_side_referenced: matches!(
                row.get("our_side_referenced").map(String::as_str),
                Some("true") | Some("t")
            ),
        })
        .collect())
}

pub(crate) enum FkPlanOutcome {
    Ready(Vec<FkPlanEntry>),
    Blocked(String, String),
}

/// 首跑：枚举 FK 并校验对端声明/类型/属主；续跑：复用已存计划（幂等步骤自愈）。
#[allow(clippy::too_many_arguments)]
async fn plan_fk_or_block(
    ctx: &OpsContext,
    schema: &str,
    table: &str,
    column: &str,
    source_type: &str,
    live_type: &str,
    target_type: &str,
    existing: &[FkPlanEntry],
    counterparts: &[FkCounterpartSpec],
    attnum: i16,
) -> Result<FkPlanOutcome, OpsError> {
    if !existing.is_empty() {
        // 续跑：主列可能已被改到目标类型，对端允许 source 或 target。
        for entry in existing {
            let (cp_schema, cp_table, cp_column) = counterpart_of(entry, table, column);
            let cp_live = column_info(ctx, &cp_schema, &cp_table, &cp_column)
                .await?
                .map(|info| info.coltype)
                .unwrap_or_default();
            let cp_canonical = canonical_type(&cp_live);
            let source_canonical = canonical_type(source_type);
            let target_canonical = canonical_type(target_type);
            if cp_canonical != source_canonical && cp_canonical != target_canonical {
                return Ok(FkPlanOutcome::Blocked(
                    format!(
                        "FK {} 对端列 {cp_table}.{cp_column} 类型漂移：{}（期望 {} 或 {}）",
                        entry.constraint, cp_live, source_type, target_type
                    ),
                    "preflight:fk_type_mismatch".into(),
                ));
            }
        }
        return Ok(FkPlanOutcome::Ready(existing.to_vec()));
    }

    let details = foreign_key_details(ctx, schema, table, attnum).await?;
    if details.is_empty() {
        return Ok(FkPlanOutcome::Ready(Vec::new()));
    }
    if let Some(composite) = details.iter().find(|detail| detail.key_count > 1) {
        return Ok(FkPlanOutcome::Blocked(
            format!(
                "外键 {} 是复合外键（{} 列）：S5 仅编排单列 FK",
                composite.constraint, composite.key_count
            ),
            "preflight:composite_fk".into(),
        ));
    }

    let mut plan = Vec::new();
    let mut missing = Vec::new();
    for detail in details {
        let (cp_schema, cp_table, cp_column) = if detail.our_side_referenced {
            (
                detail.referencing_schema.clone(),
                detail.referencing_table.clone(),
                detail.referencing_column.clone(),
            )
        } else {
            (
                detail.referenced_schema.clone(),
                detail.referenced_table.clone(),
                detail.referenced_column.clone(),
            )
        };
        let declared = counterparts.iter().any(|spec| {
            spec.table.eq_ignore_ascii_case(&cp_table)
                && spec.column.eq_ignore_ascii_case(&cp_column)
                && spec
                    .schema
                    .as_deref()
                    .map(|value| value.eq_ignore_ascii_case(&cp_schema))
                    .unwrap_or(true)
        });
        if !declared {
            missing.push(format!(
                "{} -> {cp_schema}.{cp_table}.{cp_column}",
                detail.constraint
            ));
            continue;
        }
        let cp_live = column_info(ctx, &cp_schema, &cp_table, &cp_column)
            .await?
            .map(|info| info.coltype)
            .unwrap_or_default();
        if canonical_type(&cp_live) != canonical_type(live_type) {
            return Ok(FkPlanOutcome::Blocked(
                format!(
                    "FK {} 对端列 {cp_table}.{cp_column} 类型 {cp_live} 与主列 {live_type} 不一致：\
                     双端必须同型才能重建 FK",
                    detail.constraint
                ),
                "preflight:fk_type_mismatch".into(),
            ));
        }
        if !is_table_owner(ctx, &cp_schema, &cp_table).await? {
            return Ok(FkPlanOutcome::Blocked(
                format!("当前用户不是对端表 {cp_table} 的属主，无法 ALTER 对端列"),
                "preflight:not_owner".into(),
            ));
        }
        plan.push(FkPlanEntry {
            constraint: detail.constraint,
            referencing_schema: detail.referencing_schema,
            referencing_table: detail.referencing_table,
            referencing_column: detail.referencing_column,
            referenced_schema: detail.referenced_schema,
            referenced_table: detail.referenced_table,
            referenced_column: detail.referenced_column,
            target_type: target_type.to_string(),
            dropped: false,
            altered: false,
            recreated: false,
            validated: false,
        });
    }
    if !missing.is_empty() {
        return Ok(FkPlanOutcome::Blocked(
            format!(
                "列参与外键但未声明对端列（fk_counterparts），禁止无声明拆装：{}",
                missing.join("; ")
            ),
            "preflight:fk_counterparts_missing".into(),
        ));
    }
    Ok(FkPlanOutcome::Ready(plan))
}

fn counterpart_of(entry: &FkPlanEntry, table: &str, column: &str) -> (String, String, String) {
    if entry.referenced_table == table && entry.referenced_column == column {
        (
            entry.referencing_schema.clone(),
            entry.referencing_table.clone(),
            entry.referencing_column.clone(),
        )
    } else {
        (
            entry.referenced_schema.clone(),
            entry.referenced_table.clone(),
            entry.referenced_column.clone(),
        )
    }
}

/// 校验 FK 编排结果：约束存在、`convalidated=true`、对端列类型等于目标。
async fn fk_plan_problems(
    ctx: &OpsContext,
    plan: &[FkPlanEntry],
    table: &str,
    column: &str,
    target_type: &str,
) -> Result<Vec<String>, OpsError> {
    let mut problems = Vec::new();
    for entry in plan {
        let validated: Option<String> = sqlx::query_scalar(
            "SELECT con.convalidated::text FROM pg_constraint con \
             WHERE con.conname = $1 AND con.conrelid = to_regclass($2) AND con.contype = 'f'",
        )
        .bind(&entry.constraint)
        .bind(qualified(
            Some(&entry.referencing_schema),
            &entry.referencing_table,
        ))
        .fetch_optional(ctx.db_client.pool.pg()?)
        .await?;
        match validated.as_deref() {
            Some("true") | Some("t") => {}
            Some(_) => problems.push(format!("FK {} 未完成 VALIDATE", entry.constraint)),
            None => problems.push(format!("FK {} 不存在（重建失败）", entry.constraint)),
        }
        let (cp_schema, cp_table, cp_column) = counterpart_of(entry, table, column);
        let cp_live = column_info(ctx, &cp_schema, &cp_table, &cp_column)
            .await?
            .map(|info| info.coltype)
            .unwrap_or_default();
        if canonical_type(&cp_live) != canonical_type(target_type) {
            problems.push(format!(
                "对端列 {cp_table}.{cp_column} 类型为 {cp_live}，期望 {target_type}"
            ));
        }
    }
    Ok(problems)
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
        "WITH RECURSIVE tree AS ( \
             SELECT c.oid FROM pg_class c WHERE c.oid = to_regclass($1) \
             UNION ALL \
             SELECT i.inhrelid FROM pg_inherits i JOIN tree parent ON parent.oid = i.inhparent \
         ) \
         SELECT COUNT(*)::text AS total, \
                COUNT(*) FILTER (WHERE NOT ix.indisvalid)::text AS invalid \
         FROM pg_index ix JOIN tree t ON ix.indrelid = t.oid",
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

/// Applying 失败映射：SQLSTATE 优先；`step` 标注失败步骤（FK 编排五步之一）。
async fn handle_apply_failure(
    ctx: &OpsContext,
    hub: &MigrationHub,
    job_id: &str,
    step: &str,
    err: sqlx::Error,
) -> Result<(), OpsError> {
    let message = err.to_string();
    let (code, detail) = database_error_parts(&err);
    let prefix = format!("[{step}] ");
    match classify_apply_failure(&code, &message) {
        ApplyFailure::LockTimeout => {
            block_typechange(
                ctx,
                hub,
                job_id,
                &format!("{prefix}获取锁超时（lock_timeout），可续跑: {message}"),
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
                &format!("{prefix}类型转换失败，存在无法转换的值{extra}: {message}"),
                "apply:conversion_failed",
            )
            .await
        }
        ApplyFailure::NoCast => {
            block_typechange(
                ctx,
                hub,
                job_id,
                &format!("{prefix}无可行转换路径: {message}"),
                "apply:no_cast",
            )
            .await
        }
        ApplyFailure::DependentObjects => {
            block_typechange(
                ctx,
                hub,
                job_id,
                &format!("{prefix}存在依赖对象（约束/索引等）阻碍变更: {message}"),
                "apply:dependent_objects",
            )
            .await
        }
        ApplyFailure::Privilege => {
            block_typechange(
                ctx,
                hub,
                job_id,
                &format!("{prefix}权限不足: {message}"),
                "apply:insufficient_privilege",
            )
            .await
        }
        ApplyFailure::Other => {
            // 连接中断/未知错误：DDL 原子回滚当前语句，续跑由进度位自愈。
            let job = hub
                .update_typechange(job_id, |job| {
                    job.status = MigrationStatus::Interrupted;
                    job.finished_at = Some(now());
                    job.last_error = Some(format!("{prefix}执行失败: {message}"));
                    job.notes
                        .push("失败：可重新执行（按 fk_plan 进度位续跑）".into());
                })
                .await?;
            let _ = append_typechange_audit(
                ctx,
                "field_migration.typechange.interrupted",
                "interrupted",
                &job,
                json!({ "job_id": job_id, "step": step, "error": message }),
            );
            Ok(())
        }
    }
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

    /// S5：`spec_hash` 由四要素扩为六要素（加 `using_expr` 与 `fk_plan`）。
    /// 依据 specs/pg-field-governance-s5/spec.md §8「门 3 指纹绑定」——
    /// 篡改 USING 表达式或对端声明必须让指纹失配，否则续跑会按新语义改生产列。
    #[test]
    fn spec_hash_binds_all_six_identifiers() {
        let base = spec_hash(
            "public",
            "orders",
            "payload",
            "text",
            "varchar(200)",
            None,
            &[],
        );
        assert_ne!(
            base,
            spec_hash(
                "public",
                "orders",
                "other",
                "text",
                "varchar(200)",
                None,
                &[]
            )
        );
        assert_ne!(
            base,
            spec_hash(
                "public",
                "orders",
                "payload",
                "varchar(50)",
                "varchar(200)",
                None,
                &[]
            )
        );
        assert_ne!(
            base,
            spec_hash(
                "public",
                "orders",
                "payload",
                "text",
                "varchar(100)",
                None,
                &[]
            )
        );
        assert_ne!(
            base,
            spec_hash(
                "other",
                "orders",
                "payload",
                "text",
                "varchar(200)",
                None,
                &[]
            )
        );
        assert_ne!(
            base,
            spec_hash(
                "public",
                "orders",
                "payload",
                "text",
                "varchar(200)",
                Some("payload::text"),
                &[]
            )
        );
        let fk = FkPlanEntry {
            constraint: "children_parent_id_fkey".into(),
            referencing_schema: "public".into(),
            referencing_table: "children".into(),
            referencing_column: "parent_id".into(),
            referenced_schema: "public".into(),
            referenced_table: "orders".into(),
            referenced_column: "payload".into(),
            target_type: "varchar(200)".into(),
            dropped: false,
            altered: false,
            recreated: false,
            validated: false,
        };
        assert_ne!(
            base,
            spec_hash(
                "public",
                "orders",
                "payload",
                "text",
                "varchar(200)",
                None,
                std::slice::from_ref(&fk)
            )
        );
    }
}
