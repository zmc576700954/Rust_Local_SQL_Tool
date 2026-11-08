//! E01-E14 证据采集（只读）。
//!
//! 所有 SQL 都是 SELECT / information_schema 查询；本模块没有任何写库语句。
//! 采集失败不会中断分析，而是写入 `EvidenceGap`，由风险引擎按阻断处理。

use std::collections::BTreeMap;

use serde_json::{json, Value};

use crate::ops::error::OpsError;
use crate::ops::index_advisor::{load_snapshot_public, load_table_stats_public};
use crate::ops::mysql_rows::{float_value, int_value, str_value, uint_value};
use crate::ops::policy::OpsPolicy;
use crate::ops::registry::OpsContext;
use crate::ops::types::{TableSnapshot, TableStats};
use crate::sql::util::{escape_sql_string, quote_ident_mysql, validate_identifier};

use super::dependency::load_dependencies;
use super::read_only::fetch_read_only;
use super::types::*;

#[derive(Debug, Clone)]
pub struct Collected {
    pub snapshot: TableSnapshot,
    pub stats: TableStats,
    pub analysis: FieldGovernanceAnalysis,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileMode {
    Full,
    Sampled,
    None,
}

impl ProfileMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Sampled => "sampled",
            Self::None => "none",
        }
    }
}

/// auto 模式的确定性选择：只有估算体量在 full 上限内才做 full。
pub fn decide_profile_mode(
    requested: Option<&str>,
    estimated_rows: Option<u64>,
    data_length: Option<u64>,
    policy: &OpsPolicy,
) -> ProfileMode {
    match requested {
        Some("full") => {
            if estimated_rows.unwrap_or(0) > policy.field_profile_max_rows
                || data_length.unwrap_or(0) > policy.field_profile_max_bytes
            {
                ProfileMode::Sampled
            } else {
                ProfileMode::Full
            }
        }
        Some("sampled") => ProfileMode::Sampled,
        _ => {
            if estimated_rows.unwrap_or(0) > policy.field_profile_max_rows
                || data_length.unwrap_or(0) > policy.field_profile_max_bytes
            {
                ProfileMode::Sampled
            } else {
                ProfileMode::Full
            }
        }
    }
}

pub async fn collect(
    ctx: &OpsContext,
    input: &FieldGovernanceInput,
    policy: &OpsPolicy,
) -> Result<Collected, OpsError> {
    if !ctx.is_mysql_family() {
        return Err(OpsError::Validation(
            "字段治理 S1 仅支持 MySQL / MariaDB".into(),
        ));
    }
    validate_identifier(&input.table)
        .map_err(|e| OpsError::Validation(format!("源表名非法: {e}")))?;

    let snapshot = load_snapshot_public(ctx, &input.table).await?;
    let stats = load_table_stats_public(ctx, &input.table).await?;

    let db_columns = load_db_columns(ctx, &input.table).await?;
    let analyzed: Vec<DbColumn> = match input.column.as_deref() {
        Some(column) => {
            let found = db_columns
                .iter()
                .find(|c| c.name.eq_ignore_ascii_case(column))
                .cloned();
            match found {
                Some(col) => vec![col],
                None => {
                    return Err(OpsError::Validation(format!(
                        "源表 {} 中不存在列 {column}",
                        input.table
                    )))
                }
            }
        }
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
    validate_identifier(&target_table)
        .map_err(|e| OpsError::Validation(format!("目标表名非法: {e}")))?;

    let mut gaps: Vec<EvidenceGap> = Vec::new();
    let environment = load_environment(ctx, &input.table, &stats, &mut gaps).await;
    let target_exists = target_exists(ctx, &target_table).await;
    let dependencies =
        load_dependencies(ctx, &input.table, &analyzed_columns, &snapshot).await;
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

    let source_sql = build_source_sql(
        ctx,
        &input.table,
        dependencies.single_column_pk.as_deref(),
        mode,
        sample_per_mille.unwrap_or(0),
        stats.table_rows,
    );

    let row_count = if mode == ProfileMode::Full {
        match fetch_read_only(
            ctx,
            &format!(
                "SELECT COUNT(*) AS row_count FROM {}",
                source_sql
            ),
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
                    reason: format!("COUNT(*) 失败，回退到 TABLE_ROWS 估算: {err}"),
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
        let (profile, sql, profile_gaps) = profile_column(
            ctx,
            col,
            &source_sql,
            mode,
            sample_per_mille,
            policy,
        )
        .await;
        data_sql.extend(sql);
        gaps.extend(profile_gaps);
        profiles.push(profile);
    }

    // 声明唯一性验证：只采集用户显式声明的 key 集合。
    let declarations = normalize_declarations(input.declarations.clone());
    let declaration_evidence = collect_declaration_evidence(
        ctx,
        &declarations,
        &profiles,
        &source_sql,
        mode,
        &mut gaps,
    )
    .await;

    let evidence = build_evidence_items(
        &db_columns,
        &profiles,
        &dependencies,
        &environment,
        &stats,
        row_count,
        mode,
        sample_per_mille,
        &data_sql,
        &gaps,
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
        risks: Vec::new(),
        risk_summary: RiskSummary::default(),
        scripts: Vec::new(),
        dry_run: DryRunReport::default(),
    };

    Ok(Collected {
        snapshot,
        stats,
        analysis,
    })
}

async fn load_db_columns(ctx: &OpsContext, table: &str) -> Result<Vec<DbColumn>, OpsError> {
    let db = escape_sql_string(&ctx.db_name);
    let tbl = escape_sql_string(table);
    let sql = format!(
        "SELECT COLUMN_NAME, DATA_TYPE, COLUMN_TYPE, IS_NULLABLE, COLUMN_KEY, EXTRA, \
         CHARACTER_SET_NAME, COLLATION_NAME, CHARACTER_MAXIMUM_LENGTH, NUMERIC_PRECISION, \
         NUMERIC_SCALE, COLUMN_DEFAULT, GENERATION_EXPRESSION \
         FROM information_schema.COLUMNS \
         WHERE TABLE_SCHEMA = '{db}' AND TABLE_NAME = '{tbl}' ORDER BY ORDINAL_POSITION"
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
                column_key: str_value(&row, "column_key"),
                extra: str_value(&row, "extra"),
                charset: str_value(&row, "character_set_name"),
                collation: str_value(&row, "collation_name"),
                char_max_length: uint_value(&row, "character_maximum_length"),
                numeric_precision: uint_value(&row, "numeric_precision"),
                numeric_scale: uint_value(&row, "numeric_scale"),
                column_default: str_value(&row, "column_default"),
                generation_expression: str_value(&row, "generation_expression"),
            })
        })
        .collect())
}

async fn load_environment(
    ctx: &OpsContext,
    table: &str,
    stats: &TableStats,
    gaps: &mut Vec<EvidenceGap>,
) -> EnvironmentEvidence {
    let mut env = EnvironmentEvidence {
        engine: stats.engine.clone(),
        table_collation: stats.collation.clone(),
        read_only: ctx.is_read_only,
        ..Default::default()
    };

    match fetch_read_only(
        ctx,
        "SELECT VERSION() AS version, @@version_comment AS version_comment, @@read_only AS read_only",
    )
    .await
    {
        Ok(rows) => {
            if let Some(row) = rows.first() {
                env.version = str_value(row, "version");
                env.version_comment = str_value(row, "version_comment");
                if int_value(row, "read_only").unwrap_or(0) != 0 {
                    env.read_only = true;
                }
            }
            env.version_supports_json = env
                .version
                .as_deref()
                .map(version_supports_json)
                .unwrap_or(true);
        }
        Err(err) => gaps.push(EvidenceGap {
            evidence_id: "E14".into(),
            reason: format!("数据库版本/只读状态读取失败: {err}"),
        }),
    }

    let trx_sql = "SELECT COUNT(*) AS n FROM information_schema.INNODB_TRX \
         WHERE trx_started < (NOW() - INTERVAL 30 SECOND)";
    match fetch_read_only(ctx, trx_sql).await {
        Ok(rows) => {
            env.long_transactions = rows
                .first()
                .and_then(|row| uint_value(row, "n"))
                .unwrap_or(0);
        }
        Err(err) => gaps.push(EvidenceGap {
            evidence_id: "E14".into(),
            reason: format!("长事务读取失败: {err}"),
        }),
    }

    let db = escape_sql_string(&ctx.db_name);
    let tbl = escape_sql_string(table);
    let partition_sql = format!(
        "SELECT COUNT(*) AS n FROM information_schema.PARTITIONS \
         WHERE TABLE_SCHEMA = '{db}' AND TABLE_NAME = '{tbl}' AND PARTITION_NAME IS NOT NULL"
    );
    match fetch_read_only(ctx, &partition_sql).await {
        Ok(rows) => {
            env.partitioned = rows
                .first()
                .and_then(|row| uint_value(row, "n"))
                .unwrap_or(0)
                > 0;
        }
        Err(err) => gaps.push(EvidenceGap {
            evidence_id: "E02".into(),
            reason: format!("分区信息读取失败: {err}"),
        }),
    }

    env
}

async fn target_exists(ctx: &OpsContext, target_table: &str) -> bool {
    let db = escape_sql_string(&ctx.db_name);
    let tbl = escape_sql_string(target_table);
    let sql = format!(
        "SELECT COUNT(*) AS n FROM information_schema.TABLES \
         WHERE TABLE_SCHEMA = '{db}' AND TABLE_NAME = '{tbl}'"
    );
    fetch_read_only(ctx, &sql)
        .await
        .ok()
        .and_then(|rows| rows.first().and_then(|row| uint_value(row, "n")))
        .map(|n| n > 0)
        .unwrap_or(false)
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

fn build_source_sql(
    ctx: &OpsContext,
    table: &str,
    pk: Option<&str>,
    mode: ProfileMode,
    sample_per_mille: u32,
    estimated_rows: Option<u64>,
) -> String {
    let qualified = format!(
        "{}.{}",
        quote_ident_mysql(&ctx.db_name),
        quote_ident_mysql(table)
    );
    match mode {
        ProfileMode::Full => qualified,
        ProfileMode::Sampled => match pk {
            Some(pk) => format!(
                "{} WHERE CRC32(CAST({} AS CHAR)) % 1000 < {}",
                qualified,
                quote_ident_mysql(pk),
                sample_per_mille
            ),
            None => {
                let est = estimated_rows.unwrap_or(0);
                let sample_rows = ((est as f64 * sample_per_mille as f64 / 1000.0).ceil() as u64)
                    .max(1000)
                    .min(100_000);
                format!("(SELECT * FROM {} LIMIT {}) AS _sample", qualified, sample_rows)
            }
        },
        ProfileMode::None => qualified,
    }
}

async fn profile_column(
    ctx: &OpsContext,
    col: &DbColumn,
    source_sql: &str,
    mode: ProfileMode,
    sample_per_mille: Option<u32>,
    policy: &OpsPolicy,
) -> (ColumnProfile, Vec<String>, Vec<EvidenceGap>) {
    let mut gaps = Vec::new();
    let mut sql_used = Vec::new();
    let c = quote_ident_mysql(&col.name);
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
         CAST(SUM(CASE WHEN {c} IS NULL THEN 1 ELSE 0 END) AS UNSIGNED) AS null_rows, \
         CAST(SUM(CASE WHEN {c} IS NOT NULL AND CAST({c} AS CHAR) = '' THEN 1 ELSE 0 END) AS UNSIGNED) AS empty_rows, \
         MAX(CHAR_LENGTH(CAST({c} AS CHAR))) AS max_chars, \
         MAX(OCTET_LENGTH(CAST({c} AS CHAR))) AS max_bytes, \
         AVG(CHAR_LENGTH(CAST({c} AS CHAR))) AS avg_chars, \
         CAST(SUM(CASE WHEN {c} IS NOT NULL AND CAST({c} AS CHAR) <> TRIM(CAST({c} AS CHAR)) THEN 1 ELSE 0 END) AS UNSIGNED) AS untrimmed_rows, \
         COUNT(DISTINCT CAST({c} AS CHAR)) AS distinct_rows, \
         COUNT(DISTINCT CAST({c} AS BINARY)) AS distinct_binary_rows \
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
                profile.distinct_rows = uint_value(row, "distinct_rows").unwrap_or(0);
                profile.distinct_binary_rows = uint_value(row, "distinct_binary_rows").unwrap_or(0);
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

    let control_sql = format!(
        "SELECT CAST(SUM(CASE WHEN {c} IS NOT NULL AND CAST({c} AS CHAR) REGEXP '[[:cntrl:]]' THEN 1 ELSE 0 END) AS UNSIGNED) AS control_rows \
         FROM {source_sql}"
    );
    sql_used.push(control_sql.clone());
    match fetch_read_only(ctx, &control_sql).await {
        Ok(rows) => {
            profile.control_char_rows = rows
                .first()
                .and_then(|row| uint_value(row, "control_rows"))
                .unwrap_or(0);
        }
        Err(err) => gaps.push(EvidenceGap {
            evidence_id: "E13".into(),
            reason: format!("列 {} 控制字符检查失败: {err}", col.name),
        }),
    }

    if !profile.is_text_like() {
        profile.notes.push("非文本/JSON 列：跳过 JSON 结构剖面".into());
        return (profile, sql_used, gaps);
    }

    let json_valid_sql = format!(
        "SELECT COUNT(*) AS rows_profiled, \
         CAST(SUM(CASE WHEN {c} IS NOT NULL AND JSON_VALID({c}) = 1 THEN 1 ELSE 0 END) AS UNSIGNED) AS valid_rows, \
         CAST(SUM(CASE WHEN {c} IS NOT NULL AND CAST({c} AS CHAR) <> '' AND JSON_VALID({c}) = 0 THEN 1 ELSE 0 END) AS UNSIGNED) AS invalid_rows \
         FROM {source_sql}"
    );
    sql_used.push(json_valid_sql.clone());
    match fetch_read_only(ctx, &json_valid_sql).await {
        Ok(rows) => {
            if let Some(row) = rows.first() {
                profile.json_valid_rows = uint_value(row, "valid_rows").unwrap_or(0);
                profile.json_invalid_rows = uint_value(row, "invalid_rows").unwrap_or(0);
            }
        }
        Err(err) => {
            gaps.push(EvidenceGap {
                evidence_id: "E09".into(),
                reason: format!("列 {} JSON_VALID 统计失败: {err}", col.name),
            });
            return (profile, sql_used, gaps);
        }
    }

    let top_type_sql = format!(
        "SELECT JSON_TYPE({c}) AS jt, COUNT(*) AS n FROM {source_sql} \
         WHERE {c} IS NOT NULL AND JSON_VALID({c}) = 1 GROUP BY JSON_TYPE({c})"
    );
    sql_used.push(top_type_sql.clone());
    match fetch_read_only(ctx, &top_type_sql).await {
        Ok(rows) => {
            for row in rows {
                let Some(t) = str_value(&row, "jt") else {
                    continue;
                };
                let n = uint_value(&row, "n").unwrap_or(0);
                match t.to_ascii_uppercase().as_str() {
                    "OBJECT" => profile.json_object_rows += n,
                    "ARRAY" => profile.json_array_rows += n,
                    "STRING" | "INTEGER" | "DOUBLE" | "BOOLEAN" | "NULL" => {
                        profile.json_scalar_rows += n
                    }
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

    if profile.json_object_rows == 0 {
        return (profile, sql_used, gaps);
    }

    let shape_count_sql = format!(
        "SELECT COUNT(DISTINCT CAST(JSON_KEYS({c}) AS CHAR)) AS shape_count FROM {source_sql} \
         WHERE {c} IS NOT NULL AND JSON_VALID({c}) = 1 AND JSON_TYPE({c}) = 'OBJECT'"
    );
    sql_used.push(shape_count_sql.clone());
    let shape_count = match fetch_read_only(ctx, &shape_count_sql).await {
        Ok(rows) => rows
            .first()
            .and_then(|row| uint_value(row, "shape_count"))
            .unwrap_or(0),
        Err(err) => {
            gaps.push(EvidenceGap {
                evidence_id: "E10".into(),
                reason: format!("列 {} key 形状数量统计失败: {err}", col.name),
            });
            return (profile, sql_used, gaps);
        }
    };
    if shape_count > 200 {
        profile.notes.push(format!(
            "key 形状数量 {shape_count} 超过统计上限 200，key 分布不完整"
        ));
    }

    let shapes_sql = format!(
        "SELECT CAST(JSON_KEYS({c}) AS CHAR) AS k, COUNT(*) AS n FROM {source_sql} \
         WHERE {c} IS NOT NULL AND JSON_VALID({c}) = 1 AND JSON_TYPE({c}) = 'OBJECT' \
         GROUP BY CAST(JSON_KEYS({c}) AS CHAR) ORDER BY n DESC LIMIT 200"
    );
    sql_used.push(shapes_sql.clone());
    let mut presence: BTreeMap<String, u64> = BTreeMap::new();
    match fetch_read_only(ctx, &shapes_sql).await {
        Ok(rows) => {
            for row in rows {
                let Some(raw) = str_value(&row, "k") else {
                    continue;
                };
                let n = uint_value(&row, "n").unwrap_or(0);
                for key in parse_json_keys(&raw) {
                    *presence.entry(key).or_insert(0) += n;
                }
            }
        }
        Err(err) => {
            gaps.push(EvidenceGap {
                evidence_id: "E10".into(),
                reason: format!("列 {} key 集合统计失败: {err}", col.name),
            });
        }
    }

    let object_rows = profile.json_object_rows.max(1);
    profile.json_keys = presence.keys().cloned().collect();
    for (key, rows) in &presence {
        profile.json_key_presence.push(KeyPresence {
            key: key.clone(),
            rows: *rows,
            ratio: *rows as f64 / object_rows as f64,
        });
    }

    // E11：key 类型漂移。只对前 field_max_json_keys 个 key 做类型统计，避免 N 次全表扫描。
    let max_key_type_scan = policy.field_max_json_keys.min(20);
    for key in profile.json_keys.iter().take(max_key_type_scan) {
        let path = json_path(key);
        let path_sql = escape_sql_string(&path);
        let type_sql = format!(
            "SELECT JSON_TYPE(JSON_EXTRACT({c}, '{path_sql}')) AS t, COUNT(*) AS n FROM {source_sql} \
             WHERE {c} IS NOT NULL AND JSON_VALID({c}) = 1 AND JSON_CONTAINS_PATH({c}, 'one', '{path_sql}') \
             GROUP BY JSON_TYPE(JSON_EXTRACT({c}, '{path_sql}'))"
        );
        sql_used.push(type_sql.clone());
        match fetch_read_only(ctx, &type_sql).await {
            Ok(rows) => {
                let mut types = BTreeMap::new();
                for row in rows {
                    if let Some(t) = str_value(&row, "t") {
                        types.insert(t.to_ascii_uppercase(), uint_value(&row, "n").unwrap_or(0));
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

async fn collect_declaration_evidence(
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
    let c = quote_ident_mysql(&profile.column);
    for declaration in declarations {
        if declaration.unique_keys.is_empty() || !declaration.uniqueness_expected {
            continue;
        }
        let mut exprs = Vec::new();
        for key in &declaration.unique_keys {
            let path = escape_sql_string(&json_path(key));
            exprs.push(format!(
                "JSON_UNQUOTE(JSON_EXTRACT({c}, '{path}'))"
            ));
        }
        let group_expr = exprs.join(", ");
        let sql = format!(
            "SELECT COUNT(*) AS duplicate_keys, CAST(COALESCE(SUM(cnt), 0) AS UNSIGNED) AS duplicate_rows FROM (\
               SELECT {group_expr}, COUNT(*) AS cnt FROM {source_sql} \
               WHERE {c} IS NOT NULL AND JSON_VALID({c}) = 1 \
               GROUP BY {group_expr} HAVING COUNT(*) > 1\
             ) d"
        );
        match fetch_read_only(ctx, &sql).await {
            Ok(rows) => {
                let row = rows.first();
                let duplicate_keys = row.and_then(|r| uint_value(r, "duplicate_keys")).unwrap_or(0);
                let duplicate_rows = row.and_then(|r| uint_value(r, "duplicate_rows")).unwrap_or(0);
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

fn build_evidence_items(
    db_columns: &[DbColumn],
    profiles: &[ColumnProfile],
    dependencies: &DependencyEvidence,
    environment: &EnvironmentEvidence,
    stats: &TableStats,
    row_count: u64,
    mode: ProfileMode,
    sample_per_mille: Option<u32>,
    data_sql: &[String],
    gaps: &[EvidenceGap],
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
                "charset": profile.charset,
                "collation": profile.collation,
                "generation_expression": profile.generation_expression,
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
            })
        })
        .collect();

    vec![
        EvidenceItem {
            id: "E01".into(),
            title: "列定义".into(),
            status: status("E01"),
            mode: "structural".into(),
            summary: format!("{} 个列定义已读取", db_columns.len()),
            sql: Vec::new(),
            details: json!(columns_json),
        },
        EvidenceItem {
            id: "E02".into(),
            title: "表定义".into(),
            status: status("E02"),
            mode: "structural".into(),
            summary: format!(
                "engine={} collation={} partitioned={}",
                environment.engine.as_deref().unwrap_or("-"),
                environment.table_collation.as_deref().unwrap_or("-"),
                environment.partitioned
            ),
            sql: Vec::new(),
            details: json!({
                "engine": environment.engine,
                "collation": environment.table_collation,
                "partitioned": environment.partitioned,
                "data_length": stats.data_length,
                "index_length": stats.index_length,
            }),
        },
        EvidenceItem {
            id: "E03".into(),
            title: "索引与唯一约束".into(),
            status: status("E03"),
            mode: "structural".into(),
            summary: format!("{} 个索引", dependencies.indexes.len()),
            sql: Vec::new(),
            details: json!(dependencies.indexes),
        },
        EvidenceItem {
            id: "E04".into(),
            title: "外键".into(),
            status: status("E04"),
            mode: "structural".into(),
            summary: format!("{} 个外键引用", dependencies.foreign_keys.len()),
            sql: Vec::new(),
            details: json!(dependencies.foreign_keys),
        },
        EvidenceItem {
            id: "E05".into(),
            title: "生成列依赖".into(),
            status: status("E05"),
            mode: "structural".into(),
            summary: format!("{} 个生成列", dependencies.generated_columns.len()),
            sql: Vec::new(),
            details: json!(dependencies.generated_columns),
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
            }),
        },
        EvidenceItem {
            id: "E07".into(),
            title: "表体量".into(),
            status: status("E07"),
            mode: mode_str.into(),
            summary: format!(
                "row_count={} table_rows={:?} data_length={:?}",
                row_count, stats.table_rows, stats.data_length
            ),
            sql: Vec::new(),
            details: json!({
                "row_count": row_count,
                "table_rows_estimate": stats.table_rows,
                "data_length": stats.data_length,
                "index_length": stats.index_length,
                "profile_mode": mode_str,
                "sample_per_mille": sample_per_mille,
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
            details: json!(data_detail),
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
            title: "字符集/排序规则/脏数据".into(),
            status: status("E13"),
            mode: mode_str.into(),
            summary: format!(
                "charset={:?} collation={:?}",
                profiles.first().and_then(|p| p.charset.as_ref()),
                profiles.first().and_then(|p| p.collation.as_ref())
            ),
            sql: Vec::new(),
            details: json!(data_detail),
        },
        EvidenceItem {
            id: "E14".into(),
            title: "写入活动/环境".into(),
            status: status("E14"),
            mode: "structural".into(),
            summary: format!(
                "version={} read_only={} long_trx={} supports_json={}",
                environment.version.as_deref().unwrap_or("-"),
                environment.read_only,
                environment.long_transactions,
                environment.version_supports_json
            ),
            sql: Vec::new(),
            details: json!({
                "version": environment.version,
                "version_comment": environment.version_comment,
                "read_only": environment.read_only,
                "long_transactions": environment.long_transactions,
                "version_supports_json": environment.version_supports_json,
                "single_column_pk": dependencies.single_column_pk,
                "primary_key_columns": dependencies.primary_key_columns,
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

fn parse_json_keys(raw: &str) -> Vec<String> {
    let value: Value = match serde_json::from_str(raw) {
        Ok(value) => value,
        Err(_) => return Vec::new(),
    };
    value
        .as_array()
        .map(|array| {
            array
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

pub fn json_path(key: &str) -> String {
    let escaped = key.replace('\\', "\\\\").replace('"', "\\\"");
    format!("$.\"{escaped}\"")
}

fn version_supports_json(version: &str) -> bool {
    let Some((major, minor, patch)) = parse_version(version) else {
        return true;
    };
    if major >= 8 {
        return true;
    }
    if major == 5 {
        return (minor, patch) >= (7, 8);
    }
    if major >= 10 {
        // MariaDB 10.2.7+ 提供 JSON_VALID/JSON_KEYS 等函数。
        return minor >= 2;
    }
    false
}

fn parse_version(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.split('.');
    let major = parts.next()?.trim().parse().ok()?;
    let minor = parts
        .next()
        .and_then(|v| v.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let patch = parts
        .next()
        .and_then(|v| v.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    Some((major, minor, patch))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::policy::OpsPolicy;

    #[test]
    fn auto_mode_switches_to_sampled_above_caps() {
        let policy = OpsPolicy::default();
        assert_eq!(
            decide_profile_mode(None, Some(100), Some(1024), &policy),
            ProfileMode::Full
        );
        assert_eq!(
            decide_profile_mode(
                None,
                Some(policy.field_profile_max_rows + 1),
                Some(1024),
                &policy
            ),
            ProfileMode::Sampled
        );
        assert_eq!(
            decide_profile_mode(
                None,
                Some(1),
                Some(policy.field_profile_max_bytes + 1),
                &policy
            ),
            ProfileMode::Sampled
        );
    }

    #[test]
    fn explicit_full_is_downgraded_above_caps() {
        let policy = OpsPolicy::default();
        assert_eq!(
            decide_profile_mode(Some("full"), Some(1), Some(1), &policy),
            ProfileMode::Full
        );
        assert_eq!(
            decide_profile_mode(
                Some("full"),
                Some(policy.field_profile_max_rows + 1),
                Some(1),
                &policy
            ),
            ProfileMode::Sampled
        );
    }

    #[test]
    fn version_json_support_boundaries() {
        assert!(!version_supports_json("5.7.7-log"));
        assert!(version_supports_json("5.7.8-log"));
        assert!(version_supports_json("8.0.36"));
        assert!(version_supports_json("10.6.12-MariaDB"));
    }

    #[test]
    fn json_path_escapes_quotes_and_backslashes() {
        assert_eq!(json_path("theme"), "$.\"theme\"");
        assert_eq!(json_path("a\"b"), "$.\"a\\\"b\"");
    }

    #[test]
    fn json_keys_parse_handles_null_and_arrays() {
        assert!(parse_json_keys("null").is_empty());
        assert_eq!(parse_json_keys("[\"a\",\"b\"]"), vec!["a", "b"]);
    }
}
