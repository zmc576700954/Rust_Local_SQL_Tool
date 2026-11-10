//! E01-E14 的 PostgreSQL 只读证据采集。
//!
//! 与 MySQL 的 `evidence.rs` 并列：本模块只做 SELECT / information_schema /
//! pg_catalog 查询，所有语句都走 `read_only::fetch_read_only`。
//!
//! 明确不适用项（`UPDATE_TIME`、`INNODB_TRX`、表级字符集/表级排序规则、
//! `DATA_FREE`）只写可读原因，不用近似指标顶替。

use std::collections::{BTreeMap, HashMap};

use serde_json::{json, Value};

use crate::ops::error::OpsError;
use crate::ops::index_advisor::{load_snapshot_public, load_table_stats_public};
use crate::ops::mysql_rows::{float_value, str_value, uint_value};
use crate::ops::policy::OpsPolicy;
use crate::ops::registry::OpsContext;
use crate::ops::types::TableStats;
use crate::sql::util::quote_ident_pg;

use super::dependency::matched_columns;
use super::evidence::{decide_profile_mode, Collected, ProfileMode};
use super::read_only::fetch_read_only;
use super::types::*;

/// E09 的版本选路结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum E09Mode {
    /// PG16+：服务端 `pg_input_is_valid`。
    PgInputIsValid,
    /// PG<16：客户端采样，并记录 gap。
    ClientSample,
}

/// PG16（160000）为分界；低版本没有 `pg_input_is_valid`。
pub fn e09_mode(server_version_num: u64) -> E09Mode {
    if server_version_num >= 160_000 {
        E09Mode::PgInputIsValid
    } else {
        E09Mode::ClientSample
    }
}

#[derive(Debug, Clone)]
struct DbColumn {
    name: String,
    data_type: String,
    column_type: String,
    nullable: bool,
    column_key: Option<String>,
    extra: Option<String>,
    charset: Option<String>,
    collation: Option<String>,
    char_max_length: Option<u64>,
    numeric_precision: Option<u64>,
    numeric_scale: Option<u64>,
    column_default: Option<String>,
    generation_expression: Option<String>,
}

pub async fn collect(
    ctx: &OpsContext,
    input: &FieldGovernanceInput,
    policy: &OpsPolicy,
) -> Result<Collected, OpsError> {
    crate::sql::util::validate_identifier(&input.table)
        .map_err(|e| OpsError::Validation(format!("源表名非法: {e}")))?;

    let snapshot = load_snapshot_public(ctx, &input.table).await?;
    let stats = load_table_stats_public(ctx, &input.table).await?;
    let db_columns = load_columns_pg(ctx, &input.table).await?;

    let analyzed: Vec<DbColumn> = match input.column.as_deref() {
        Some(column) => match db_columns
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(column))
            .cloned()
        {
            Some(col) => vec![col],
            None => {
                return Err(OpsError::Validation(format!(
                    "源表 {} 中不存在列 {column}",
                    input.table
                )))
            }
        },
        None => db_columns.clone(),
    };
    let analyzed_columns: Vec<String> = analyzed.iter().map(|c| c.name.clone()).collect();

    let mode = decide_profile_mode(
        input.profile_mode.as_deref(),
        stats.table_rows,
        stats.data_length,
        policy,
    );
    let requested_sample = input
        .sample_per_mille
        .unwrap_or(policy.field_sample_per_mille)
        .clamp(1, 1000);
    let sample_per_mille = if mode == ProfileMode::Sampled {
        Some(requested_sample)
    } else {
        None
    };

    let target_table = resolve_target_table(input, &analyzed_columns);
    crate::sql::util::validate_identifier(&target_table)
        .map_err(|e| OpsError::Validation(format!("目标表名非法: {e}")))?;

    let mut gaps: Vec<EvidenceGap> = Vec::new();
    let (environment, db_encoding) =
        load_environment_pg(ctx, &input.table, &mut gaps).await;
    let target_exists = target_exists_pg(ctx, &target_table).await;

    let dependencies = load_dependencies_pg(ctx, &input.table, &analyzed_columns).await;
    for gap in &dependencies.gaps {
        gaps.push(EvidenceGap {
            evidence_id: gap
                .split_whitespace()
                .next()
                .unwrap_or("E06")
                .to_string(),
            reason: gap.clone(),
        });
    }

    let source_sql = build_source_sql_pg(
        &input.table,
        dependencies.single_column_pk.as_deref(),
        mode,
        sample_per_mille.unwrap_or(0),
        stats.table_rows,
        environment.schema.as_deref(),
    );

    let row_count = if mode == ProfileMode::Full {
        match fetch_read_only(
            ctx,
            &format!("SELECT COUNT(*) AS row_count FROM {source_sql}"),
        )
        .await
        {
            Ok(rows) => rows
                .first()
                .and_then(|row| uint_value(row, "row_count"))
                .unwrap_or(0),
            Err(err) => {
                gaps.push(EvidenceGap {
                    evidence_id: "E07".into(),
                    reason: format!("COUNT(*) 失败，回退到 reltuples 估算: {err}"),
                });
                stats.table_rows.unwrap_or(0)
            }
        }
    } else {
        stats.table_rows.unwrap_or(0)
    };

    let mut profiles = Vec::with_capacity(analyzed.len());
    let mut data_sql = Vec::new();
    for col in &analyzed {
        let (profile, sql, profile_gaps) = profile_column_pg(
            ctx,
            col,
            &source_sql,
            mode,
            sample_per_mille,
            policy,
            &environment,
        )
        .await;
        data_sql.extend(sql);
        gaps.extend(profile_gaps);
        profiles.push(profile);
    }

    probe_casts_pg(
        ctx,
        input,
        &analyzed,
        &source_sql,
        &environment,
        &mut profiles,
        &mut gaps,
    )
    .await;

    let declarations = normalize_declarations(input.declarations.clone());
    let declaration_evidence = declaration_evidence_pg(
        ctx,
        &declarations,
        &profiles,
        &source_sql,
        mode,
        &mut gaps,
    )
    .await;

    let evidence = build_evidence_pg(
        &profiles,
        &dependencies,
        &environment,
        &stats,
        row_count,
        mode,
        sample_per_mille,
        &data_sql,
        &gaps,
        db_encoding.as_deref(),
    );

    let analysis = FieldGovernanceAnalysis {
        db_name: ctx.db_name.clone(),
        table: input.table.clone(),
        column: input.column.clone(),
        analyzed_columns,
        profile_mode: mode.as_str().into(),
        sample_per_mille,
        estimated_rows: stats.table_rows,
        data_length: stats.data_length,
        index_length: stats.index_length,
        row_count,
        columns: profiles,
        dependencies,
        environment,
        evidence,
        gaps,
        declarations: declarations.clone(),
        declaration_hash: declaration_hash(&declarations),
        declaration_evidence,
        target_table,
        target_exists,
        empty_string_policy: input.empty_string_policy(),
        target_columns: Vec::new(),
        risks: Vec::new(),
        risk_summary: RiskSummary::default(),
        scripts: Vec::new(),
        dry_run: DryRunReport::default(),
        retire_package: Default::default(),
        rollback: Default::default(),
        dual_write: Default::default(),
    };

    Ok(Collected {
        snapshot,
        stats,
        analysis,
    })
}

fn resolve_target_table(input: &FieldGovernanceInput, columns: &[String]) -> String {
    if let Some(table) = input
        .target
        .as_ref()
        .and_then(|target| target.table.as_ref())
    {
        return table.clone();
    }
    match input.column.as_deref() {
        Some(column) => format!("{}_{column}_ext", input.table),
        None => {
            let hint = columns.first().map(String::as_str).unwrap_or("fields");
            format!("{}_{hint}_ext", input.table)
        }
    }
}

fn qualified_table(schema: Option<&str>, table: &str) -> String {
    match schema {
        Some(schema) => format!(
            "{}.{}",
            quote_ident_pg(schema),
            quote_ident_pg(table)
        ),
        None => quote_ident_pg(table),
    }
}

async fn load_columns_pg(ctx: &OpsContext, table: &str) -> Result<Vec<DbColumn>, OpsError> {
    let tbl = crate::sql::util::escape_sql_string(table);
    let sql = format!(
        "SELECT c.column_name::text AS column_name, \
                c.udt_name::text AS data_type, \
                format_type(a.atttypid, a.atttypmod)::text AS column_type, \
                c.is_nullable::text AS is_nullable, \
                CASE WHEN EXISTS (\
                    SELECT 1 FROM pg_index i \
                    WHERE i.indrelid = cl.oid AND i.indisprimary \
                      AND a.attnum = ANY(i.indkey::int2[])\
                ) THEN 'PRI' ELSE '' END AS column_key, \
                CASE WHEN a.attidentity <> '' THEN 'IDENTITY' \
                     WHEN a.attgenerated = 's' THEN 'GENERATED STORED' \
                     WHEN a.attgenerated = 'v' THEN 'GENERATED VIRTUAL' \
                     ELSE '' END AS extra, \
                c.character_set_name::text AS character_set_name, \
                c.collation_name::text AS collation_name, \
                c.character_maximum_length::text AS character_maximum_length, \
                c.numeric_precision::text AS numeric_precision, \
                c.numeric_scale::text AS numeric_scale, \
                c.column_default::text AS column_default, \
                c.generation_expression::text AS generation_expression \
         FROM information_schema.columns c \
         JOIN pg_namespace n ON n.nspname = c.table_schema \
         JOIN pg_class cl ON cl.relname = c.table_name AND cl.relnamespace = n.oid \
         JOIN pg_attribute a ON a.attrelid = cl.oid AND a.attname = c.column_name \
         WHERE c.table_schema = current_schema() AND c.table_name = '{tbl}' \
         ORDER BY c.ordinal_position"
    );
    let rows = fetch_read_only(ctx, &sql).await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let name = str_value(&row, "column_name")?;
            Some(DbColumn {
                name,
                data_type: str_value(&row, "data_type")
                    .unwrap_or_default()
                    .to_ascii_lowercase(),
                column_type: str_value(&row, "column_type").unwrap_or_default(),
                nullable: str_value(&row, "is_nullable")
                    .map(|v| v.eq_ignore_ascii_case("YES"))
                    .unwrap_or(true),
                column_key: str_value(&row, "column_key").filter(|v| !v.is_empty()),
                extra: str_value(&row, "extra").filter(|v| !v.is_empty()),
                charset: str_value(&row, "character_set_name").filter(|v| !v.is_empty()),
                collation: str_value(&row, "collation_name").filter(|v| !v.is_empty()),
                char_max_length: uint_value(&row, "character_maximum_length"),
                numeric_precision: uint_value(&row, "numeric_precision"),
                numeric_scale: uint_value(&row, "numeric_scale"),
                column_default: str_value(&row, "column_default"),
                generation_expression: str_value(&row, "generation_expression")
                    .filter(|v| !v.is_empty()),
            })
        })
        .collect())
}

async fn load_environment_pg(
    ctx: &OpsContext,
    table: &str,
    gaps: &mut Vec<EvidenceGap>,
) -> (EnvironmentEvidence, Option<String>) {
    let mut env = EnvironmentEvidence {
        read_only: ctx.is_read_only,
        engine_family: "postgresql".into(),
        version_supports_json: true,
        ..Default::default()
    };
    let mut db_encoding = None;

    match fetch_read_only(
        ctx,
        "SELECT version()::text AS version, \
                current_setting('server_version_num')::text AS server_version_num, \
                COALESCE(current_schema(), 'public')::text AS schema_name, \
                pg_is_in_recovery()::text AS in_recovery, \
                current_setting('default_transaction_read_only')::text AS default_ro, \
                COALESCE((SELECT rolsuper FROM pg_roles WHERE rolname = current_user), false)::text AS is_superuser, \
                current_setting('server_encoding')::text AS server_encoding",
    )
    .await
    {
        Ok(rows) => {
            if let Some(row) = rows.first() {
                env.version = str_value(row, "version");
                env.server_version_num = uint_value(row, "server_version_num");
                env.schema = str_value(row, "schema_name").filter(|v| !v.is_empty());
                env.table_collation = None;
                env.engine = None;
                let in_recovery = str_value(row, "in_recovery")
                    .map(|v| v.eq_ignore_ascii_case("true"))
                    .unwrap_or(false);
                let default_ro = str_value(row, "default_ro")
                    .map(|v| v.eq_ignore_ascii_case("on"))
                    .unwrap_or(false);
                env.read_only = ctx.is_read_only || in_recovery || default_ro;
                db_encoding = str_value(row, "server_encoding").filter(|v| !v.is_empty());
                let is_superuser = str_value(row, "is_superuser")
                    .map(|v| v.eq_ignore_ascii_case("true"))
                    .unwrap_or(false);
                load_privilege_evidence_pg(ctx, is_superuser, &mut env, gaps).await;
            }
        }
        Err(err) => gaps.push(EvidenceGap {
            evidence_id: "E14".into(),
            reason: format!("PG 版本/环境读取失败: {err}"),
        }),
    }

    let tbl = crate::sql::util::escape_sql_string(table);
    let table_sql = format!(
        "SELECT c.relkind::text AS table_kind, \
                EXISTS (SELECT 1 FROM pg_partitioned_table pt WHERE pt.partrelid = c.oid)::text AS is_partitioned, \
                EXISTS (SELECT 1 FROM pg_inherits i WHERE i.inhrelid = c.oid)::text AS is_partition_child, \
                EXISTS (\
                    SELECT 1 FROM pg_attribute a \
                    JOIN pg_collation co ON co.oid = a.attcollation \
                    WHERE a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped \
                      AND NOT co.collisdeterministic\
                )::text AS nondeterministic \
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = current_schema() AND c.relname = '{tbl}'"
    );
    match fetch_read_only(ctx, &table_sql).await {
        Ok(rows) => {
            if let Some(row) = rows.first() {
                env.table_kind = str_value(row, "table_kind");
                let partitioned = str_value(row, "is_partitioned")
                    .map(|v| v.eq_ignore_ascii_case("true"))
                    .unwrap_or(false);
                let partition_child = str_value(row, "is_partition_child")
                    .map(|v| v.eq_ignore_ascii_case("true"))
                    .unwrap_or(false);
                env.partitioned = partitioned || partition_child;
                env.nondeterministic_collation = str_value(row, "nondeterministic")
                    .map(|v| v.eq_ignore_ascii_case("true"))
                    .unwrap_or(false);
            }
        }
        Err(err) => gaps.push(EvidenceGap {
            evidence_id: "E02".into(),
            reason: format!("PG 表定义读取失败: {err}"),
        }),
    }

    match fetch_read_only(
        ctx,
        "SELECT COUNT(*)::text AS n FROM pg_stat_activity \
         WHERE state <> 'idle' AND xact_start IS NOT NULL \
           AND xact_start < (now() - interval '30 seconds') \
           AND pid <> pg_backend_pid()",
    )
    .await
    {
        Ok(rows) => {
            env.long_transactions = rows
                .first()
                .and_then(|row| uint_value(row, "n"))
                .unwrap_or(0);
        }
        Err(err) => gaps.push(EvidenceGap {
            evidence_id: "E14".into(),
            reason: format!("pg_stat_activity 长事务读取失败: {err}"),
        }),
    }

    (env, db_encoding)
}

async fn load_privilege_evidence_pg(
    ctx: &OpsContext,
    is_superuser: bool,
    env: &mut EnvironmentEvidence,
    gaps: &mut Vec<EvidenceGap>,
) {
    if !is_superuser {
        env.pg_authid_readable = Some(false);
        env.no_password_roles = None;
        gaps.push(EvidenceGap {
            evidence_id: "E14".into(),
            reason: "pg_authid 仅超级用户可读：无法统计无密码账号，未返回 0 以避免假阴性"
                .into(),
        });
        return;
    }
    match fetch_read_only(
        ctx,
        "SELECT COUNT(*)::text AS n FROM pg_authid WHERE rolpassword IS NULL",
    )
    .await
    {
        Ok(rows) => {
            env.pg_authid_readable = Some(true);
            env.no_password_roles = rows
                .first()
                .and_then(|row| uint_value(row, "n"));
        }
        Err(err) => {
            env.pg_authid_readable = Some(false);
            env.no_password_roles = None;
            gaps.push(EvidenceGap {
                evidence_id: "E14".into(),
                reason: format!(
                    "pg_authid 读取失败（超级用户仍无权限？）：未返回 0，避免假阴性: {err}"
                ),
            });
        }
    }
}

async fn target_exists_pg(ctx: &OpsContext, target_table: &str) -> bool {
    let tbl = crate::sql::util::escape_sql_string(target_table);
    let sql = format!(
        "SELECT COUNT(*)::text AS n FROM pg_class c \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = current_schema() AND c.relname = '{tbl}'"
    );
    fetch_read_only(ctx, &sql)
        .await
        .ok()
        .and_then(|rows| rows.first().and_then(|row| uint_value(row, "n")))
        .map(|n| n > 0)
        .unwrap_or(false)
}

fn build_source_sql_pg(
    table: &str,
    pk: Option<&str>,
    mode: ProfileMode,
    sample_per_mille: u32,
    _estimated_rows: Option<u64>,
    schema: Option<&str>,
) -> String {
    let qualified = qualified_table(schema, table);
    match mode {
        ProfileMode::Full => format!("(SELECT * FROM {qualified}) AS _fm_src"),
        ProfileMode::Sampled => {
            let key = pk
                .map(|pk| format!("CAST({} AS text)", quote_ident_pg(pk)))
                .unwrap_or_else(|| "ctid::text".into());
            format!(
                "(SELECT * FROM {qualified} \
                 WHERE mod(abs(hashtext({key})::bigint), 1000) < {sample_per_mille}) AS _fm_src"
            )
        }
        ProfileMode::None => format!("(SELECT * FROM {qualified} LIMIT 0) AS _fm_src"),
    }
}

fn validity_expr(c: &str, is_json_type: bool, env: &EnvironmentEvidence) -> Option<String> {
    if is_json_type {
        return Some("TRUE".into());
    }
    let version = env.server_version_num.unwrap_or(0);
    if e09_mode(version) == E09Mode::PgInputIsValid {
        Some(format!(
            "pg_input_is_valid(CAST({c} AS text), 'jsonb')"
        ))
    } else {
        None
    }
}

#[allow(clippy::too_many_arguments)]
async fn profile_column_pg(
    ctx: &OpsContext,
    col: &DbColumn,
    source_sql: &str,
    mode: ProfileMode,
    sample_per_mille: Option<u32>,
    policy: &OpsPolicy,
    env: &EnvironmentEvidence,
) -> (ColumnProfile, Vec<String>, Vec<EvidenceGap>) {
    let mut gaps = Vec::new();
    let mut sql_used = Vec::new();
    let c = quote_ident_pg(&col.name);
    let mut profile = ColumnProfile {
        column: col.name.clone(),
        data_type: col.data_type.clone(),
        column_type: col.column_type.clone(),
        nullable: col.nullable,
        column_key: col.column_key.clone(),
        extra: col.extra.clone(),
        charset: col.charset.clone(),
        collation: col.collation.clone(),
        char_max_length: col.char_max_length,
        numeric_precision: col.numeric_precision,
        numeric_scale: col.numeric_scale,
        column_default: col.column_default.clone(),
        generation_expression: col.generation_expression.clone(),
        profile_mode: mode.as_str().into(),
        sample_per_mille,
        ..Default::default()
    };

    if mode == ProfileMode::None {
        gaps.push(EvidenceGap {
            evidence_id: "E08".into(),
            reason: format!("列 {} 未采集数据剖面", col.name),
        });
        return (profile, sql_used, gaps);
    }

    let agg_sql = format!(
        "SELECT COUNT(*) AS rows_profiled, \
         COUNT(*) FILTER (WHERE {c} IS NULL) AS null_rows, \
         COUNT(*) FILTER (WHERE {c} IS NOT NULL AND CAST({c} AS text) = '') AS empty_rows, \
         MAX(length(CAST({c} AS text))) AS max_chars, \
         MAX(octet_length(CAST({c} AS text))) AS max_bytes, \
         AVG(length(CAST({c} AS text))) AS avg_chars, \
         COUNT(*) FILTER (WHERE {c} IS NOT NULL AND CAST({c} AS text) <> btrim(CAST({c} AS text))) AS untrimmed_rows, \
         COUNT(*) FILTER (WHERE {c} IS NOT NULL AND CAST({c} AS text) ~ '[[:cntrl:]]') AS control_rows, \
         COUNT(DISTINCT CAST({c} AS text)) AS distinct_rows, \
         COUNT(DISTINCT CAST({c} AS text) COLLATE \"C\") AS distinct_binary_rows \
         FROM {source_sql}"
    );
    sql_used.push(agg_sql.clone());
    match fetch_read_only(ctx, &agg_sql).await {
        Ok(rows) => {
            if let Some(row) = rows.first() {
                profile.rows_profiled = uint_value(row, "rows_profiled").unwrap_or(0);
                profile.null_rows = uint_value(row, "null_rows").unwrap_or(0);
                profile.empty_rows = uint_value(row, "empty_rows").unwrap_or(0);
                profile.max_char_length = uint_value(row, "max_chars").unwrap_or(0);
                profile.max_byte_length = uint_value(row, "max_bytes").unwrap_or(0);
                profile.avg_char_length = float_value(row, "avg_chars").unwrap_or(0.0);
                profile.untrimmed_rows = uint_value(row, "untrimmed_rows").unwrap_or(0);
                profile.control_char_rows = uint_value(row, "control_rows").unwrap_or(0);
                profile.distinct_rows = uint_value(row, "distinct_rows").unwrap_or(0);
                profile.distinct_binary_rows =
                    uint_value(row, "distinct_binary_rows").unwrap_or(0);
            }
        }
        Err(err) => {
            profile.profile_mode = "none".into();
            gaps.push(EvidenceGap {
                evidence_id: "E08".into(),
                reason: format!("列 {} 空值/长度聚合失败: {err}", col.name),
            });
            return (profile, sql_used, gaps);
        }
    }

    if profile.data_type == "char" || profile.data_type == "bpchar" {
        profile.notes.push(
            "PG char(n) 按空格填充；空串与定长比较语义需人工确认，不能用 MySQL 的 CHAR 语义直接套用"
                .into(),
        );
    }

    if !profile.is_text_like() {
        profile
            .notes
            .push("非文本/JSON 列：跳过 JSON 结构剖面".into());
        return (profile, sql_used, gaps);
    }

    let json_type = profile.is_json_type();
    let validity = validity_expr(&c, json_type, env);

    if json_type {
        profile.json_valid_rows = profile
            .rows_profiled
            .saturating_sub(profile.null_rows);
        profile.json_invalid_rows = 0;
        profile.pg_input_check = Some("not_applicable".into());
        profile
            .notes
            .push("json/jsonb 列本身已校验，无需 pg_input_is_valid".into());
    } else if let Some(valid) = &validity {
        let json_valid_sql = format!(
            "SELECT COUNT(*) AS rows_profiled, \
             COUNT(*) FILTER (WHERE {c} IS NOT NULL AND {valid}) AS valid_rows, \
             COUNT(*) FILTER (WHERE {c} IS NOT NULL AND btrim(CAST({c} AS text)) <> '' AND NOT {valid}) AS invalid_rows \
             FROM {source_sql}"
        );
        sql_used.push(json_valid_sql.clone());
        match fetch_read_only(ctx, &json_valid_sql).await {
            Ok(rows) => {
                if let Some(row) = rows.first() {
                    profile.json_valid_rows = uint_value(row, "valid_rows").unwrap_or(0);
                    profile.json_invalid_rows = uint_value(row, "invalid_rows").unwrap_or(0);
                }
                profile.pg_input_check = Some("pg_input_is_valid".into());
            }
            Err(err) => {
                gaps.push(EvidenceGap {
                    evidence_id: "E09".into(),
                    reason: format!("列 {} pg_input_is_valid 统计失败: {err}", col.name),
                });
                return (profile, sql_used, gaps);
            }
        }
    } else {
        // PG<16：客户端采样，不返回全表近似值。
        let sample_limit = policy.field_profile_max_rows.min(1000).max(1);
        let sample_sql = format!(
            "SELECT CAST({c} AS text) AS v FROM {source_sql} \
             WHERE {c} IS NOT NULL AND btrim(CAST({c} AS text)) <> '' LIMIT {sample_limit}"
        );
        sql_used.push(sample_sql.clone());
        match fetch_read_only(ctx, &sample_sql).await {
            Ok(rows) => {
                let mut valid = 0u64;
                let mut invalid = 0u64;
                for row in &rows {
                    if let Some(v) = str_value(row, "v") {
                        if serde_json::from_str::<Value>(&v).is_ok() {
                            valid += 1;
                        } else {
                            invalid += 1;
                        }
                    }
                }
                profile.json_valid_rows = valid;
                profile.json_invalid_rows = invalid;
                profile.pg_input_check = Some("client_sample".into());
                profile.notes.push(format!(
                    "PG<16 无 pg_input_is_valid：E09 仅客户端采样 {} 行，非全量统计",
                    rows.len()
                ));
                gaps.push(EvidenceGap {
                    evidence_id: "E09".into(),
                    reason: format!(
                        "PG<16 无法服务端校验 JSON：列 {} 仅客户端采样 {} 行，写步骤不可判定",
                        col.name,
                        rows.len()
                    ),
                });
            }
            Err(err) => {
                gaps.push(EvidenceGap {
                    evidence_id: "E09".into(),
                    reason: format!("列 {} 客户端 JSON 采样失败: {err}", col.name),
                });
                return (profile, sql_used, gaps);
            }
        }
        profile
            .notes
            .push("PG<16：E10 JSON 结构剖面未采集（无服务端安全转换函数）".into());
        gaps.push(EvidenceGap {
            evidence_id: "E10".into(),
            reason: format!(
                "PG<16 无法安全地把文本转 jsonb：列 {} 的顶层类型/key 分布未采集",
                col.name
            ),
        });
        return (profile, sql_used, gaps);
    }

    let guard = match &validity {
        Some(valid) if valid != "TRUE" => format!(" AND {valid}"),
        _ => String::new(),
    };

    let top_type_sql = format!(
        "SELECT CASE WHEN jsonb_typeof(v) = 'object' THEN 'OBJECT' \
                     WHEN jsonb_typeof(v) = 'array' THEN 'ARRAY' \
                     WHEN jsonb_typeof(v) = 'null' THEN 'NULL' \
                     ELSE 'SCALAR' END AS top, COUNT(*) AS n \
         FROM (SELECT CAST({c} AS jsonb) AS v FROM {source_sql} \
               WHERE {c} IS NOT NULL{guard}) s \
         GROUP BY 1"
    );
    sql_used.push(top_type_sql.clone());
    match fetch_read_only(ctx, &top_type_sql).await {
        Ok(rows) => {
            for row in rows {
                let Some(t) = str_value(&row, "top") else {
                    continue;
                };
                let n = uint_value(&row, "n").unwrap_or(0);
                match t.as_str() {
                    "OBJECT" => profile.json_object_rows += n,
                    "ARRAY" => profile.json_array_rows += n,
                    "SCALAR" | "NULL" => profile.json_scalar_rows += n,
                    _ => profile.json_other_rows += n,
                }
            }
            profile.json_profiled = true;
        }
        Err(err) => gaps.push(EvidenceGap {
            evidence_id: "E10".into(),
            reason: format!("列 {} JSON 顶层类型统计失败: {err}", col.name),
        }),
    }

    if json_type {
        profile.notes.push(
            "jsonb 会归一化 key 顺序/重复 key/数字表示；顶层类型与 key 统计以归一化后的 jsonb 为准"
                .into(),
        );
    }

    if profile.json_object_rows == 0 {
        return (profile, sql_used, gaps);
    }

    let presence_sql = format!(
        "SELECT k::text AS key, COUNT(*) AS n \
         FROM {source_sql}, LATERAL jsonb_object_keys(CAST({c} AS jsonb)) AS k \
         WHERE {c} IS NOT NULL AND jsonb_typeof(CAST({c} AS jsonb)) = 'object'{guard} \
         GROUP BY k ORDER BY n DESC LIMIT 200"
    );
    sql_used.push(presence_sql.clone());
    let object_rows = profile.json_object_rows.max(1);
    match fetch_read_only(ctx, &presence_sql).await {
        Ok(rows) => {
            for row in rows {
                let Some(key) = str_value(&row, "key") else {
                    continue;
                };
                let n = uint_value(&row, "n").unwrap_or(0);
                profile.json_keys.push(key.clone());
                profile.json_key_presence.push(KeyPresence {
                    key,
                    rows: n,
                    ratio: n as f64 / object_rows as f64,
                });
            }
        }
        Err(err) => gaps.push(EvidenceGap {
            evidence_id: "E10".into(),
            reason: format!("列 {} key 分布统计失败: {err}", col.name),
        }),
    }
    if profile.json_keys.len() >= 200 {
        profile
            .notes
            .push("key 数量达到统计上限 200，key 分布可能不完整".into());
    }

    let shape_sql = format!(
        "SELECT COUNT(DISTINCT shape) AS shape_count FROM (\
           SELECT (SELECT string_agg(k2, ',' ORDER BY k2) \
                   FROM jsonb_object_keys(CAST({c} AS jsonb)) AS k2) AS shape \
           FROM {source_sql} \
           WHERE {c} IS NOT NULL AND jsonb_typeof(CAST({c} AS jsonb)) = 'object'{guard}\
         ) s"
    );
    sql_used.push(shape_sql.clone());
    match fetch_read_only(ctx, &shape_sql).await {
        Ok(rows) => {
            let shape_count = rows
                .first()
                .and_then(|row| uint_value(row, "shape_count"))
                .unwrap_or(0);
            if shape_count > 200 {
                profile.notes.push(format!(
                    "key 形状数量 {shape_count} 超过统计上限 200，key 分布不完整"
                ));
            }
        }
        Err(err) => gaps.push(EvidenceGap {
            evidence_id: "E10".into(),
            reason: format!("列 {} key 形状数量统计失败: {err}", col.name),
        }),
    }

    let max_key_type_scan = policy.field_max_json_keys.min(20);
    for key in profile.json_keys.iter().take(max_key_type_scan) {
        let key_sql = crate::sql::util::escape_sql_string(key);
        let type_sql = format!(
            "SELECT jsonb_typeof(CAST({c} AS jsonb) -> '{key_sql}') AS t, COUNT(*) AS n \
             FROM {source_sql} \
             WHERE {c} IS NOT NULL AND jsonb_typeof(CAST({c} AS jsonb)) = 'object' \
               AND (CAST({c} AS jsonb) -> '{key_sql}') IS NOT NULL{guard} \
             GROUP BY 1"
        );
        sql_used.push(type_sql.clone());
        match fetch_read_only(ctx, &type_sql).await {
            Ok(rows) => {
                let mut types = BTreeMap::new();
                for row in rows {
                    if let Some(t) = str_value(&row, "t") {
                        types.insert(pg_json_type_label(&t), uint_value(&row, "n").unwrap_or(0));
                    }
                }
                profile.json_key_types.push(KeyTypeDistribution {
                    key: key.clone(),
                    types,
                });
            }
            Err(err) => gaps.push(EvidenceGap {
                evidence_id: "E11".into(),
                reason: format!("列 {} key {key} 类型统计失败: {err}", col.name),
            }),
        }
    }
    if profile.json_keys.len() > max_key_type_scan {
        profile.notes.push(format!(
            "仅对前 {max_key_type_scan} 个 key 做了类型漂移统计，其余 key 未验证"
        ));
    }

    (profile, sql_used, gaps)
}

fn pg_json_type_label(raw: &str) -> String {
    match raw.to_ascii_lowercase().as_str() {
        "string" => "STRING".into(),
        "number" => "DOUBLE".into(),
        "boolean" => "BOOLEAN".into(),
        "null" => "NULL".into(),
        "object" => "OBJECT".into(),
        "array" => "ARRAY".into(),
        other => other.to_ascii_uppercase(),
    }
}

fn target_type_for_column(input: &FieldGovernanceInput, column: &str) -> Option<String> {
    let target = input.target.as_ref()?;
    for mapping in &target.mappings {
        let applies = mapping
            .source_key
            .as_deref()
            .map(|key| key.eq_ignore_ascii_case(column))
            .unwrap_or(true);
        if applies {
            return mapping.target_type.clone();
        }
    }
    None
}

fn safe_pg_type_name(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > 120 {
        return None;
    }
    if !trimmed
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | ' ' | '(' | ')' | ','))
    {
        return None;
    }
    Some(trimmed.to_string())
}

#[allow(clippy::too_many_arguments)]
async fn probe_casts_pg(
    ctx: &OpsContext,
    input: &FieldGovernanceInput,
    columns: &[DbColumn],
    source_sql: &str,
    env: &EnvironmentEvidence,
    profiles: &mut [ColumnProfile],
    gaps: &mut Vec<EvidenceGap>,
) {
    let version = env.server_version_num.unwrap_or(0);
    for col in columns {
        let Some(target_type) = target_type_for_column(input, &col.name) else {
            continue;
        };
        if !pg_type_changed(&col.column_type, &target_type) {
            continue;
        }
        let Some(profile) = profiles
            .iter_mut()
            .find(|profile| profile.column.eq_ignore_ascii_case(&col.name))
        else {
            continue;
        };
        if e09_mode(version) == E09Mode::ClientSample {
            gaps.push(EvidenceGap {
                evidence_id: "E01".into(),
                reason: format!(
                    "PG<16 无 pg_input_is_valid：列 {} 的 USING 转换未预检",
                    col.name
                ),
            });
            continue;
        }
        let Some(target_type) = safe_pg_type_name(&target_type) else {
            gaps.push(EvidenceGap {
                evidence_id: "E01".into(),
                reason: format!("目标类型名含非法字符，拒绝预检: {target_type}"),
            });
            continue;
        };
        let escaped = crate::sql::util::escape_sql_string(&target_type);
        let c = quote_ident_pg(&col.name);
        let sql = format!(
            "SELECT COUNT(*) AS invalid_rows FROM {source_sql} \
             WHERE {c} IS NOT NULL AND NOT pg_input_is_valid(CAST({c} AS text), '{escaped}')"
        );
        match fetch_read_only(ctx, &sql).await {
            Ok(rows) => {
                profile.pg_cast_invalid_rows = rows
                    .first()
                    .and_then(|row| uint_value(row, "invalid_rows"));
            }
            Err(err) => gaps.push(EvidenceGap {
                evidence_id: "E01".into(),
                reason: format!("列 {} USING 预检失败: {err}", col.name),
            }),
        }
    }
}

/// PG 类型名规范化：仅用于判断「是否发生类型变更」，不用于生成 DDL。
pub fn normalize_pg_type(raw: &str) -> String {
    let lowered = raw.trim().to_ascii_lowercase();
    let lowered = lowered
        .strip_prefix("pg_catalog.")
        .unwrap_or(&lowered)
        .to_string();
    let base = match lowered.as_str() {
        "int4" => "integer".to_string(),
        "int8" => "bigint".to_string(),
        "int2" => "smallint".to_string(),
        "float8" => "double precision".to_string(),
        "float4" => "real".to_string(),
        "bool" => "boolean".to_string(),
        "decimal" => "numeric".to_string(),
        "timestamptz" => "timestamp with time zone".to_string(),
        "timestamp" => "timestamp without time zone".to_string(),
        other => other.to_string(),
    };
    if base.starts_with("varchar") {
        return base.replacen("varchar", "character varying", 1);
    }
    if base.starts_with("bpchar") {
        return base.replacen("bpchar", "character", 1);
    }
    base
}

pub fn pg_type_changed(source_type: &str, target_type: &str) -> bool {
    normalize_pg_type(source_type) != normalize_pg_type(target_type)
}

async fn declaration_evidence_pg(
    ctx: &OpsContext,
    declarations: &[Declaration],
    profiles: &[ColumnProfile],
    source_sql: &str,
    mode: ProfileMode,
    gaps: &mut Vec<EvidenceGap>,
) -> Vec<DeclarationEvidence> {
    let mut evidence = Vec::new();
    let Some(profile) = profiles.first() else {
        return evidence;
    };
    let c = quote_ident_pg(&profile.column);
    for declaration in declarations {
        if declaration.unique_keys.is_empty() || !declaration.uniqueness_expected {
            continue;
        }
        let exprs: Vec<String> = declaration
            .unique_keys
            .iter()
            .map(|key| {
                let key_sql = crate::sql::util::escape_sql_string(key);
                format!("CAST({c} AS jsonb) ->> '{key_sql}'")
            })
            .collect();
        let group_expr = exprs.join(", ");
        let sql = format!(
            "SELECT COUNT(*) AS duplicate_keys, COALESCE(SUM(cnt), 0) AS duplicate_rows FROM (\
               SELECT {group_expr}, COUNT(*) AS cnt FROM {source_sql} \
               WHERE {c} IS NOT NULL AND jsonb_typeof(CAST({c} AS jsonb)) = 'object' \
               GROUP BY {group_expr} HAVING COUNT(*) > 1\
             ) d"
        );
        match fetch_read_only(ctx, &sql).await {
            Ok(rows) => {
                let row = rows.first();
                let duplicate_keys = row
                    .and_then(|r| uint_value(r, "duplicate_keys"))
                    .unwrap_or(0);
                let duplicate_rows = row
                    .and_then(|r| uint_value(r, "duplicate_rows"))
                    .unwrap_or(0);
                evidence.push(DeclarationEvidence {
                    declaration_id: declaration.id.clone(),
                    kind: "uniqueness".into(),
                    key_set: declaration.unique_keys.clone(),
                    rows: duplicate_keys,
                    detail: if mode == ProfileMode::Full {
                        format!("重复 key 组合 {duplicate_keys} 个，涉及 {duplicate_rows} 行")
                    } else {
                        format!(
                            "抽样模式下重复 key 组合 {duplicate_keys} 个，涉及 {duplicate_rows} 行（不可作为写授权）"
                        )
                    },
                });
            }
            Err(err) => gaps.push(EvidenceGap {
                evidence_id: "SEM-DECL".into(),
                reason: format!("声明 {} 唯一性验证失败: {err}", declaration.id),
            }),
        }
    }
    evidence
}

/// PG 依赖扫描：E03-E06。
pub async fn load_dependencies_pg(
    ctx: &OpsContext,
    table: &str,
    columns: &[String],
) -> DependencyEvidence {
    let tbl = crate::sql::util::escape_sql_string(table);
    let mut evidence = DependencyEvidence::default();
    evidence.scan_complete = true;
    evidence.notes.push(
        "E06 例程依赖仅按 SQL 文本匹配（下界）：动态 SQL/EXECUTE 无法静态识别".into(),
    );

    let index_sql = format!(
        "SELECT i.relname::text AS index_name, \
                CASE WHEN ix.indisunique THEN 'true' ELSE 'false' END AS is_unique, \
                CASE WHEN ix.indisprimary THEN 'true' ELSE 'false' END AS is_primary, \
                k.ord::text AS seq_in_index, \
                pg_get_indexdef(ix.indexrelid, k.ord::int, true)::text AS column_name, \
                am.amname::text AS index_type \
         FROM pg_index ix \
         JOIN pg_class i ON i.oid = ix.indexrelid \
         JOIN pg_class t ON t.oid = ix.indrelid \
         JOIN pg_namespace n ON n.oid = t.relnamespace \
         JOIN pg_am am ON am.oid = i.relam \
         JOIN generate_series(1, ix.indnkeyatts) AS k(ord) ON true \
         WHERE n.nspname = current_schema() AND t.relname = '{tbl}' \
         ORDER BY i.relname, k.ord"
    );
    match fetch_read_only(ctx, &index_sql).await {
        Ok(rows) => {
            let mut map: HashMap<String, IndexReference> = HashMap::new();
            let mut primary_columns = Vec::new();
            for row in rows {
                let Some(name) = str_value(&row, "index_name") else {
                    continue;
                };
                let unique = str_value(&row, "is_unique")
                    .map(|v| v.eq_ignore_ascii_case("true"))
                    .unwrap_or(false);
                let primary = str_value(&row, "is_primary")
                    .map(|v| v.eq_ignore_ascii_case("true"))
                    .unwrap_or(false);
                let entry = map.entry(name.clone()).or_insert_with(|| IndexReference {
                    name: name.clone(),
                    unique,
                    primary,
                    index_type: str_value(&row, "index_type").unwrap_or_default(),
                    columns: Vec::new(),
                });
                if let Some(column) = str_value(&row, "column_name") {
                    entry.columns.push(column.clone());
                    if primary {
                        primary_columns.push(column);
                    }
                }
            }
            let mut indexes: Vec<IndexReference> = map.into_values().collect();
            indexes.sort_by(|a, b| a.name.cmp(&b.name));
            evidence.indexes = indexes;
            evidence.primary_key_columns = primary_columns;
            evidence.single_column_pk = if evidence.primary_key_columns.len() == 1 {
                Some(evidence.primary_key_columns[0].clone())
            } else {
                None
            };
        }
        Err(err) => {
            evidence.scan_complete = false;
            evidence.gaps.push(format!("E03 索引读取失败: {err}"));
        }
    }

    let child_fk_sql = format!(
        "SELECT con.conname::text AS constraint_name, a.attname::text AS column_name, \
                rc.relname::text AS referenced_table, ra.attname::text AS referenced_column \
         FROM pg_constraint con \
         JOIN pg_class c ON c.oid = con.conrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         JOIN LATERAL generate_subscripts(con.conkey, 1) AS k(pos) ON true \
         JOIN pg_attribute a ON a.attrelid = con.conrelid AND a.attnum = con.conkey[k.pos] \
         JOIN pg_class rc ON rc.oid = con.confrelid \
         JOIN pg_attribute ra ON ra.attrelid = con.confrelid AND ra.attnum = con.confkey[k.pos] \
         WHERE con.contype = 'f' AND n.nspname = current_schema() AND c.relname = '{tbl}'"
    );
    match fetch_read_only(ctx, &child_fk_sql).await {
        Ok(rows) => {
            for row in rows {
                let Some(constraint_name) = str_value(&row, "constraint_name") else {
                    continue;
                };
                let Some(column) = str_value(&row, "column_name") else {
                    continue;
                };
                evidence.foreign_keys.push(ForeignKeyReference {
                    constraint_name,
                    direction: "child".into(),
                    table: table.to_string(),
                    column,
                    referenced_table: str_value(&row, "referenced_table"),
                    referenced_column: str_value(&row, "referenced_column"),
                });
            }
        }
        Err(err) => {
            evidence.scan_complete = false;
            evidence.gaps.push(format!("E04 子表外键读取失败: {err}"));
        }
    }

    let parent_fk_sql = format!(
        "SELECT con.conname::text AS constraint_name, cc.relname::text AS table_name, \
                a.attname::text AS referenced_column, ca.attname::text AS column_name \
         FROM pg_constraint con \
         JOIN pg_class c ON c.oid = con.confrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         JOIN pg_class cc ON cc.oid = con.conrelid \
         JOIN LATERAL generate_subscripts(con.conkey, 1) AS k(pos) ON true \
         JOIN pg_attribute ca ON ca.attrelid = con.conrelid AND ca.attnum = con.conkey[k.pos] \
         JOIN pg_attribute a ON a.attrelid = con.confrelid AND a.attnum = con.confkey[k.pos] \
         WHERE con.contype = 'f' AND n.nspname = current_schema() AND c.relname = '{tbl}'"
    );
    match fetch_read_only(ctx, &parent_fk_sql).await {
        Ok(rows) => {
            for row in rows {
                let Some(constraint_name) = str_value(&row, "constraint_name") else {
                    continue;
                };
                let Some(referenced_column) = str_value(&row, "referenced_column") else {
                    continue;
                };
                evidence.foreign_keys.push(ForeignKeyReference {
                    constraint_name,
                    direction: "parent".into(),
                    table: str_value(&row, "table_name").unwrap_or_default(),
                    column: referenced_column.clone(),
                    referenced_table: Some(table.to_string()),
                    referenced_column: Some(referenced_column),
                });
            }
        }
        Err(err) => {
            evidence.scan_complete = false;
            evidence.gaps.push(format!("E04 父表外键读取失败: {err}"));
        }
    }

    let generated_sql = format!(
        "SELECT a2.attname::text AS name, \
                COALESCE(pg_get_expr(ad.adbin, ad.adrelid), '')::text AS expression, \
                CASE a2.attgenerated WHEN 's' THEN 'GENERATED STORED' \
                     WHEN 'v' THEN 'GENERATED VIRTUAL' ELSE a2.attgenerated END AS extra \
         FROM pg_depend d \
         JOIN pg_attrdef ad ON ad.oid = d.objid \
         JOIN pg_attribute a2 ON a2.attrelid = ad.adrelid AND a2.attnum = ad.adnum \
         JOIN pg_class c ON c.oid = d.refobjid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         JOIN pg_attribute src ON src.attrelid = d.refobjid AND src.attnum = d.refobjsubid \
         WHERE n.nspname = current_schema() AND c.relname = '{tbl}' AND a2.attgenerated <> ''"
    );
    match fetch_read_only(ctx, &generated_sql).await {
        Ok(rows) => {
            let mut seen = std::collections::HashSet::new();
            for row in rows {
                let Some(name) = str_value(&row, "name") else {
                    continue;
                };
                if !seen.insert(name.clone()) {
                    continue;
                }
                evidence.generated_columns.push(GeneratedColumnReference {
                    name,
                    expression: str_value(&row, "expression").unwrap_or_default(),
                    extra: str_value(&row, "extra").unwrap_or_default(),
                });
            }
        }
        Err(err) => {
            evidence.scan_complete = false;
            evidence.gaps.push(format!("E05 生成列读取失败: {err}"));
        }
    }

    let views_sql = format!(
        "SELECT DISTINCT v.relname::text AS name, \
                CASE WHEN d.refobjsubid = 0 THEN '*' ELSE a.attname END::text AS column_name \
         FROM pg_depend d \
         JOIN pg_rewrite r ON r.oid = d.objid \
         JOIN pg_class v ON v.oid = r.ev_class \
         JOIN pg_class c ON c.oid = d.refobjid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         LEFT JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum = d.refobjsubid \
         WHERE d.classid = 'pg_rewrite'::regclass \
           AND d.refclassid = 'pg_class'::regclass \
           AND n.nspname = current_schema() AND c.relname = '{tbl}' \
           AND v.relkind IN ('v', 'm') AND v.oid <> c.oid"
    );
    match fetch_read_only(ctx, &views_sql).await {
        Ok(rows) => {
            let mut grouped: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for row in rows {
                let Some(name) = str_value(&row, "name") else {
                    continue;
                };
                let matched = match str_value(&row, "column_name").as_deref() {
                    Some("*") | None => columns.to_vec(),
                    Some(column) => vec![column.to_string()],
                };
                grouped.entry(name).or_default().extend(matched);
            }
            for (name, mut matched) in grouped {
                matched.sort();
                matched.dedup();
                evidence.objects.push(DependencyObject {
                    kind: "view".into(),
                    name,
                    matched_columns: matched,
                });
            }
        }
        Err(err) => {
            evidence.scan_complete = false;
            evidence.gaps.push(format!("E06 视图依赖读取失败: {err}"));
        }
    }

    let triggers_sql = format!(
        "SELECT t.tgname::text AS trigger_name, pg_get_triggerdef(t.oid)::text AS definition \
         FROM pg_trigger t \
         JOIN pg_class c ON c.oid = t.tgrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = current_schema() AND c.relname = '{tbl}' AND NOT t.tgisinternal"
    );
    match fetch_read_only(ctx, &triggers_sql).await {
        Ok(rows) => {
            for row in rows {
                let Some(name) = str_value(&row, "trigger_name") else {
                    continue;
                };
                let definition = str_value(&row, "definition").unwrap_or_default();
                evidence.source_triggers.push(name.clone());
                let matched = matched_columns(&definition, columns);
                if !matched.is_empty() {
                    evidence.objects.push(DependencyObject {
                        kind: "trigger".into(),
                        name,
                        matched_columns: matched,
                    });
                }
            }
            evidence.source_triggers.sort();
            evidence.source_triggers.dedup();
        }
        Err(err) => {
            evidence.scan_complete = false;
            evidence.gaps.push(format!("E06 触发器读取失败: {err}"));
        }
    }

    match fetch_read_only(
        ctx,
        "SELECT p.proname::text AS routine_name, pg_get_functiondef(p.oid)::text AS definition \
         FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
         WHERE n.nspname NOT IN ('pg_catalog', 'information_schema') \
           AND p.prokind IN ('f', 'p') LIMIT 1000",
    )
    .await
    {
        Ok(rows) => {
            for row in rows {
                let Some(name) = str_value(&row, "routine_name") else {
                    continue;
                };
                let definition = str_value(&row, "definition").unwrap_or_default();
                let matched = matched_columns(&definition, columns);
                if !matched.is_empty() {
                    evidence.objects.push(DependencyObject {
                        kind: "routine".into(),
                        name,
                        matched_columns: matched,
                    });
                }
            }
        }
        Err(err) => {
            evidence.scan_complete = false;
            evidence.gaps.push(format!("E06 例程读取失败: {err}"));
        }
    }

    evidence
}

#[allow(clippy::too_many_arguments)]
fn build_evidence_pg(
    profiles: &[ColumnProfile],
    dependencies: &DependencyEvidence,
    environment: &EnvironmentEvidence,
    stats: &TableStats,
    row_count: u64,
    mode: ProfileMode,
    sample_per_mille: Option<u32>,
    data_sql: &[String],
    gaps: &[EvidenceGap],
    db_encoding: Option<&str>,
) -> Vec<EvidenceItem> {
    let status = |id: &str| {
        if gaps.iter().any(|gap| gap.evidence_id == id) {
            "partial".to_string()
        } else {
            "collected".to_string()
        }
    };
    let mode_str = mode.as_str();
    let columns_json: Vec<Value> = profiles
        .iter()
        .map(|profile| {
            json!({
                "column": profile.column,
                "data_type": profile.data_type,
                "column_type": profile.column_type,
                "nullable": profile.nullable,
                "collation": profile.collation,
                "generation_expression": profile.generation_expression,
                "numeric_precision": profile.numeric_precision,
                "numeric_scale": profile.numeric_scale,
            })
        })
        .collect();
    let data_detail: Vec<Value> = profiles
        .iter()
        .map(|profile| {
            json!({
                "column": profile.column,
                "rows_profiled": profile.rows_profiled,
                "null_rows": profile.null_rows,
                "empty_rows": profile.empty_rows,
                "json_valid_rows": profile.json_valid_rows,
                "json_invalid_rows": profile.json_invalid_rows,
                "json_object_rows": profile.json_object_rows,
                "json_array_rows": profile.json_array_rows,
                "json_keys": profile.json_keys,
                "json_key_presence": profile.json_key_presence,
                "json_key_types": profile.json_key_types,
                "max_char_length": profile.max_char_length,
                "max_byte_length": profile.max_byte_length,
                "avg_char_length": profile.avg_char_length,
                "untrimmed_rows": profile.untrimmed_rows,
                "control_char_rows": profile.control_char_rows,
                "distinct_rows": profile.distinct_rows,
                "distinct_binary_rows": profile.distinct_binary_rows,
                "pg_input_check": profile.pg_input_check,
                "pg_cast_invalid_rows": profile.pg_cast_invalid_rows,
                "notes": profile.notes,
            })
        })
        .collect();

    vec![
        EvidenceItem {
            id: "E01".into(),
            title: "列定义".into(),
            status: status("E01"),
            mode: "structural".into(),
            summary: format!("{} 个列定义已读取（pg_catalog）", profiles.len()),
            sql: Vec::new(),
            details: json!({
                "columns": columns_json,
                "not_applicable": [{
                    "item": "COLUMN_TYPE / CHARACTER_SET_NAME",
                    "reason": "PG 无 MySQL 的 COLUMN_TYPE 与列级字符集概念；使用 format_type + 数据库编码"
                }],
            }),
        },
        EvidenceItem {
            id: "E02".into(),
            title: "表定义".into(),
            status: status("E02"),
            mode: "structural".into(),
            summary: format!(
                "table_kind={} partitioned={} collation=N/A",
                environment.table_kind.as_deref().unwrap_or("-"),
                environment.partitioned
            ),
            sql: Vec::new(),
            details: json!({
                "table_kind": environment.table_kind,
                "partitioned": environment.partitioned,
                "data_length": stats.data_length,
                "index_length": stats.index_length,
                "not_applicable": [
                    { "item": "ENGINE", "reason": "PG 无存储引擎概念（统一 heap/access method）" },
                    { "item": "ROW_FORMAT", "reason": "PG 无 MySQL row_format 概念" },
                    { "item": "TABLE_COLLATION", "reason": "PG 排序规则是列级/数据库级，不存在表级排序规则" }
                ],
            }),
        },
        EvidenceItem {
            id: "E03".into(),
            title: "索引与唯一约束".into(),
            status: status("E03"),
            mode: "structural".into(),
            summary: format!("{} 个索引（pg_index）", dependencies.indexes.len()),
            sql: Vec::new(),
            details: json!(dependencies.indexes),
        },
        EvidenceItem {
            id: "E04".into(),
            title: "外键".into(),
            status: status("E04"),
            mode: "structural".into(),
            summary: format!("{} 个外键引用（pg_constraint）", dependencies.foreign_keys.len()),
            sql: Vec::new(),
            details: json!(dependencies.foreign_keys),
        },
        EvidenceItem {
            id: "E05".into(),
            title: "生成列依赖".into(),
            status: status("E05"),
            mode: "structural".into(),
            summary: format!(
                "{} 个生成列（pg_attrdef + pg_depend）",
                dependencies.generated_columns.len()
            ),
            sql: Vec::new(),
            details: json!({
                "generated_columns": dependencies.generated_columns,
                "notes": dependencies.notes,
            }),
        },
        EvidenceItem {
            id: "E06".into(),
            title: "视图/触发器/例程依赖".into(),
            status: if dependencies.scan_complete {
                "collected".to_string()
            } else {
                "partial".to_string()
            },
            mode: "structural".into(),
            summary: format!(
                "依赖对象 {} 个，源表触发器 {} 个，扫描完整={}",
                dependencies.objects.len(),
                dependencies.source_triggers.len(),
                dependencies.scan_complete
            ),
            sql: Vec::new(),
            details: json!({
                "objects": dependencies.objects,
                "source_triggers": dependencies.source_triggers,
                "scan_complete": dependencies.scan_complete,
                "gaps": dependencies.gaps,
                "notes": dependencies.notes,
            }),
        },
        EvidenceItem {
            id: "E07".into(),
            title: "表体量".into(),
            status: status("E07"),
            mode: mode_str.into(),
            summary: format!(
                "row_count={} reltuples={:?} data_length={:?}",
                row_count, stats.table_rows, stats.data_length
            ),
            sql: Vec::new(),
            details: json!({
                "row_count": row_count,
                "reltuples_estimate": stats.table_rows,
                "data_length": stats.data_length,
                "index_length": stats.index_length,
                "profile_mode": mode_str,
                "sample_per_mille": sample_per_mille,
                "not_applicable": [
                    { "item": "UPDATE_TIME", "reason": "PG 无表级更新时间元数据；UPDATE_TIME 不适用" },
                    { "item": "DATA_FREE", "reason": "PG 无 InnoDB DATA_FREE 碎片字段；不适用" }
                ],
            }),
        },
        EvidenceItem {
            id: "E08".into(),
            title: "NULL / 空串比例".into(),
            status: status("E08"),
            mode: mode_str.into(),
            summary: format!("覆盖 {} 列的数据剖面", profiles.len()),
            sql: data_sql.to_vec(),
            details: json!(data_detail),
        },
        EvidenceItem {
            id: "E09".into(),
            title: "JSON 解析成功率".into(),
            status: status("E09"),
            mode: mode_str.into(),
            summary: json_invalid_summary(profiles),
            sql: Vec::new(),
            details: json!({
                "columns": data_detail,
                "server_version_num": environment.server_version_num,
            }),
        },
        EvidenceItem {
            id: "E10".into(),
            title: "JSON 顶层类型与 key 分布".into(),
            status: status("E10"),
            mode: mode_str.into(),
            summary: json_key_summary(profiles),
            sql: Vec::new(),
            details: json!(data_detail),
        },
        EvidenceItem {
            id: "E11".into(),
            title: "JSON key 类型漂移".into(),
            status: status("E11"),
            mode: mode_str.into(),
            summary: json_type_drift_summary(profiles),
            sql: Vec::new(),
            details: json!(data_detail),
        },
        EvidenceItem {
            id: "E12".into(),
            title: "字符串长度分布".into(),
            status: status("E12"),
            mode: mode_str.into(),
            summary: format!(
                "最大字符长度 {}，最大字节长度 {}",
                profiles.iter().map(|p| p.max_char_length).max().unwrap_or(0),
                profiles.iter().map(|p| p.max_byte_length).max().unwrap_or(0),
            ),
            sql: Vec::new(),
            details: json!(data_detail),
        },
        EvidenceItem {
            id: "E13".into(),
            title: "编码/排序规则/脏数据".into(),
            status: status("E13"),
            mode: mode_str.into(),
            summary: format!(
                "db_encoding={:?} column_collation={:?} nondeterministic={}",
                db_encoding,
                profiles.first().and_then(|p| p.collation.as_ref()),
                environment.nondeterministic_collation
            ),
            sql: Vec::new(),
            details: json!({
                "db_encoding": db_encoding,
                "columns": data_detail,
                "nondeterministic_collation": environment.nondeterministic_collation,
                "not_applicable": [
                    { "item": "TABLE_CHARSET", "reason": "PG 无表级字符集；编码是数据库级，列级仅 collation" },
                    { "item": "TABLE_COLLATION", "reason": "PG 不存在表级排序规则；只有列级 collation 与数据库默认" }
                ],
            }),
        },
        EvidenceItem {
            id: "E14".into(),
            title: "写入活动/环境".into(),
            status: status("E14"),
            mode: "structural".into(),
            summary: format!(
                "version={} read_only={} long_trx={} pg_authid_readable={:?}",
                environment.version.as_deref().unwrap_or("-"),
                environment.read_only,
                environment.long_transactions,
                environment.pg_authid_readable
            ),
            sql: Vec::new(),
            details: json!({
                "version": environment.version,
                "server_version_num": environment.server_version_num,
                "read_only": environment.read_only,
                "long_transactions": environment.long_transactions,
                "long_transactions_source": "pg_stat_activity（语义与 INNODB_TRX 不同）",
                "pg_authid_readable": environment.pg_authid_readable,
                "no_password_roles": environment.no_password_roles,
                "single_column_pk": dependencies.single_column_pk,
                "primary_key_columns": dependencies.primary_key_columns,
                "not_applicable": [
                    { "item": "INNODB_TRX", "reason": "PG 无 InnoDB 事务表；长事务改用 pg_stat_activity 且语义不同" },
                    { "item": "PROCESSLIST", "reason": "PG 对应 pg_stat_activity，字段语义不同" }
                ],
            }),
        },
    ]
}

fn json_invalid_summary(profiles: &[ColumnProfile]) -> String {
    let invalid: u64 = profiles.iter().map(|p| p.json_invalid_rows).sum();
    let valid: u64 = profiles.iter().map(|p| p.json_valid_rows).sum();
    format!("合法 JSON 行 {valid}，非法 JSON 行 {invalid}")
}

fn json_key_summary(profiles: &[ColumnProfile]) -> String {
    let keys: usize = profiles.iter().map(|p| p.json_keys.len()).sum();
    let arrays: u64 = profiles.iter().map(|p| p.json_array_rows).sum();
    format!("key 总数 {keys}，顶层数组行 {arrays}")
}

fn json_type_drift_summary(profiles: &[ColumnProfile]) -> String {
    let mut drift = 0;
    for profile in profiles {
        for key in &profile.json_key_types {
            let non_null = key.types.keys().filter(|t| t.as_str() != "NULL").count();
            if non_null > 1 {
                drift += 1;
            }
        }
    }
    format!("{drift} 个 key 存在类型漂移")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn e09_selects_pg16_boundary() {
        assert_eq!(e09_mode(150_000), E09Mode::ClientSample);
        assert_eq!(e09_mode(159_999), E09Mode::ClientSample);
        assert_eq!(e09_mode(160_000), E09Mode::PgInputIsValid);
        assert_eq!(e09_mode(170_000), E09Mode::PgInputIsValid);
    }

    #[test]
    fn pg_type_normalization_handles_aliases_and_modifiers() {
        assert_eq!(normalize_pg_type("int8"), "bigint");
        assert_eq!(normalize_pg_type("varchar(64)"), "character varying(64)");
        assert_eq!(
            normalize_pg_type("character varying(64)"),
            "character varying(64)"
        );
        assert!(!pg_type_changed("varchar(64)", "character varying(64)"));
        assert!(pg_type_changed("int8", "int4"));
        assert!(pg_type_changed("numeric(10,2)", "numeric(12,4)"));
    }

    #[test]
    fn pg_type_name_guard_rejects_injection() {
        assert!(safe_pg_type_name("character varying(64)").is_some());
        assert!(safe_pg_type_name("numeric(10, 2)").is_some());
        assert!(safe_pg_type_name("text; DROP TABLE x").is_none());
        assert!(safe_pg_type_name("text'").is_none());
    }
}
