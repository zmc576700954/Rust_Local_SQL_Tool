//! dry-run 脚本包：建表 DDL、回填模板、校验 SQL、回滚脚本、下线脚本。
//!
//! 所有脚本 `executable=false`，本模块不连接数据库执行任何语句。

use crate::ops::safety::classify_blocked_sql;
use crate::ops::types::TableSnapshot;
use crate::sql::util::{escape_sql_string, quote_ident_mysql};

use super::evidence::json_path;
use super::schema::{build_target_columns, create_table_sql, TargetColumn};
use super::types::{
    DryRunCheck, DryRunReport, EmptyStringPolicy, FieldGovernanceAnalysis, FieldGovernanceInput,
    ManualScript,
};

pub fn generate(
    snapshot: &TableSnapshot,
    analysis: &mut FieldGovernanceAnalysis,
    input: &FieldGovernanceInput,
) {
    let Some(column_name) = analysis.column.clone() else {
        analysis.scripts = Vec::new();
        analysis.dry_run = DryRunReport {
            executed_against_database: false,
            passed: true,
            checks: Vec::new(),
            note: "整表字段盘点不生成迁移脚本；请指定源列后再次分析。".into(),
        };
        return;
    };
    let Some(profile) = analysis
        .columns
        .iter()
        .find(|column| column.column.eq_ignore_ascii_case(&column_name))
        .cloned()
    else {
        return;
    };

    let target = input.target.clone().unwrap_or_default();
    let target_table = analysis.target_table.clone();
    let pk_name = target
        .pk_column
        .clone()
        .or_else(|| analysis.dependencies.single_column_pk.clone())
        .unwrap_or_else(|| "id".into());
    let pk_type = snapshot
        .find_column(&pk_name)
        .map(|column| column.column_type.clone())
        .unwrap_or_else(|| "BIGINT".into());
    let target_columns = build_target_columns(input, &profile);
    analysis.empty_string_policy = input.empty_string_policy();
    analysis.target_columns = target_columns.clone();
    let source = format!(
        "{}.{}",
        quote_ident_mysql(&analysis.db_name),
        quote_ident_mysql(&analysis.table)
    );
    let destination = format!(
        "{}.{}",
        quote_ident_mysql(&analysis.db_name),
        quote_ident_mysql(&target_table)
    );
    let source_column = quote_ident_mysql(&profile.column);
    let pk = quote_ident_mysql(&pk_name);

    let create_sql = create_table_sql(
        &analysis.db_name,
        &target,
        &target_table,
        &pk_name,
        &pk_type,
        &target_columns,
    );
    let backfill_sql = build_backfill_sql(
        &source,
        &destination,
        &source_column,
        &pk,
        &target_columns,
        analysis.empty_string_policy,
    );
    let verify_sql = build_verify_sql(
        &source,
        &destination,
        &source_column,
        &pk,
        &target_columns,
        analysis.empty_string_policy,
    );
    let rollback_sql = build_rollback_sql(&destination, &pk);
    let retire_sql = build_retire_sql(&source, &source_column, &profile.column);

    let mut scripts = vec![
        ManualScript {
            id: "create-table".into(),
            title: "建表 DDL（阶段 L2）".into(),
            kind: "create_table".into(),
            phase: "L2_table_created".into(),
            sql: create_sql,
            executable: false,
            irreversible: false,
            notes: vec![
                "只生成，不执行。".into(),
                "列类型/长度由证据推断，必须人工确认容量与排序规则。".into(),
                "执行前确认目标表不存在。".into(),
            ],
        },
        ManualScript {
            id: "backfill".into(),
            title: "分批回填 SQL 模板（阶段 L3）".into(),
            kind: "backfill".into(),
            phase: "L3_backfill".into(),
            sql: backfill_sql,
            executable: false,
            irreversible: false,
            notes: vec![
                "按主键区间分批；每批独立事务，失败只重跑该批。".into(),
                "ON DUPLICATE KEY UPDATE 保证幂等。".into(),
                "必须替换 :last_pk / :end_pk / :batch_size 占位符。".into(),
                format!(
                    "空串策略={}（{}）；SQL NULL 与空串语义不同，脚本不得静默转换。",
                    analysis.empty_string_policy.as_str(),
                    analysis.empty_string_policy.label()
                ),
                if analysis.dependencies.single_column_pk.is_none() {
                    "源表没有单列主键：分片不安全，禁止自动回填。".into()
                } else {
                    "回填期间需评估写入并发；建议停写或双写。".into()
                },
            ],
        },
        ManualScript {
            id: "verify".into(),
            title: "校验 SQL（阶段 verify）".into(),
            kind: "verify".into(),
            phase: "verify".into(),
            sql: verify_sql,
            executable: false,
            irreversible: false,
            notes: vec![
                "行数 + 主键集合 + 规范化解码逐行比对。".into(),
                "SQL NULL / 空串 / JSON null 严格区分，不做合并归一。".into(),
                format!(
                    "空串策略={}（{}）；页大小由 :page_size 指定，不再固定截断。",
                    analysis.empty_string_policy.as_str(),
                    analysis.empty_string_policy.label()
                ),
                "差异主键按主键区间分页；:last_pk 传上一页最后一个主键。".into(),
                "校验失败必须阻断后续下线步骤。".into(),
            ],
        },
        ManualScript {
            id: "rollback-delete".into(),
            title: "回滚：清空目标表数据（可重新回填）".into(),
            kind: "rollback".into(),
            phase: "rollback".into(),
            sql: rollback_sql,
            executable: false,
            irreversible: false,
            notes: vec![
                "仅在源列仍存在时可重新回填恢复。".into(),
                "若目标表还有其他写入方，禁止执行本脚本。".into(),
            ],
        },
        ManualScript {
            id: "rollback-drop".into(),
            title: "回滚：删除目标表（不可逆）".into(),
            kind: "rollback".into(),
            phase: "rollback".into(),
            sql: format!("DROP TABLE IF EXISTS {destination};"),
            executable: false,
            irreversible: true,
            notes: vec![
                "会删除目标表全部数据，无法撤销。".into(),
                "只在确认目标表由本计划创建且无其他写入方时使用。".into(),
            ],
        },
        ManualScript {
            id: "retire-column".into(),
            title: "下线旧列（阶段 L4，不可逆）".into(),
            kind: "retire".into(),
            phase: "L4_retire".into(),
            sql: retire_sql,
            executable: false,
            irreversible: true,
            notes: vec![
                "DROP COLUMN 永远不自动执行，只能人工确认后执行。".into(),
                "执行前必须：最终校验 mismatch=0、旧列快照已备份、应用读路径已切换。".into(),
                "执行后旧列数据只能从备份恢复。".into(),
            ],
        },
    ];
    scripts.sort_by(|a, b| a.phase.cmp(&b.phase).then_with(|| a.id.cmp(&b.id)));
    analysis.scripts = scripts;
    analysis.dry_run = dry_run_report(&analysis.scripts);
}

fn build_backfill_sql(
    source: &str,
    destination: &str,
    source_column: &str,
    pk: &str,
    columns: &[TargetColumn],
    empty_string_policy: EmptyStringPolicy,
) -> String {
    let mut select_exprs = vec![pk.to_string()];
    let mut insert_columns = vec![pk.to_string()];
    let mut update_exprs = Vec::new();
    for column in columns {
        insert_columns.push(quote_ident_mysql(&column.name));
        select_exprs.push(backfill_expr(source_column, column, empty_string_policy));
        update_exprs.push(format!(
            "{} = VALUES({})",
            quote_ident_mysql(&column.name),
            quote_ident_mysql(&column.name)
        ));
    }
    let policy_comment = match empty_string_policy {
        EmptyStringPolicy::Preserve => {
            "-- 空串策略 preserve：保留空串，不使用 NULLIF 抹平；空串与 NULL 是不同语义。\n"
        }
        EmptyStringPolicy::ToNull => {
            "-- 空串策略 to_null：已显式选择把空串转成 NULL（高风险，需用户确认）。\n"
        }
        EmptyStringPolicy::Block => {
            "-- 空串策略 block：命中空串即阻断；存在空串时禁止执行本脚本。\n"
        }
    };
    format!(
        "-- 阶段 L3：分批回填（草案；本工具不执行）\n\
         {policy_comment}\
         -- 每批一个事务；:last_pk / :end_pk / :batch_size 由人工替换。\n\
         INSERT INTO {destination} ({})\n\
         SELECT {}\n\
         FROM {source}\n\
         WHERE {pk} > :last_pk AND {pk} <= :end_pk\n\
         ORDER BY {pk}\n\
         LIMIT :batch_size\n\
         ON DUPLICATE KEY UPDATE {};",
        insert_columns.join(", "),
        select_exprs.join(",\n       "),
        update_exprs.join(", ")
    )
}

/// 判断目标列是否为数值类型；数值列在 V4 中使用十进制词元比较。
fn is_numeric_sql_type(sql_type: &str) -> bool {
    let upper = sql_type.trim().to_ascii_uppercase();
    [
        "BIGINT", "INT", "SMALLINT", "MEDIUMINT", "TINYINT", "DECIMAL", "NUMERIC", "DOUBLE",
        "FLOAT", "REAL",
    ]
    .iter()
    .any(|prefix| upper.starts_with(prefix))
}

fn is_text_sql_type(sql_type: &str) -> bool {
    let upper = sql_type.trim().to_ascii_uppercase();
    upper.starts_with("VARCHAR")
        || upper.starts_with("CHAR")
        || upper.ends_with("TEXT")
        || upper.starts_with("TEXT")
}

/// `to_null` 是唯一允许把空串变成 NULL 的路径；preserve/block 都保持原文。
fn apply_empty_string_policy(expr: String, policy: EmptyStringPolicy) -> String {
    match policy {
        EmptyStringPolicy::ToNull => format!("NULLIF({expr}, '')"),
        EmptyStringPolicy::Preserve | EmptyStringPolicy::Block => expr,
    }
}

fn backfill_expr(
    source_column: &str,
    column: &TargetColumn,
    empty_string_policy: EmptyStringPolicy,
) -> String {
    let Some(key) = column.source_key.as_deref() else {
        let raw = source_column.to_string();
        return if is_text_sql_type(&column.sql_type) {
            apply_empty_string_policy(raw, empty_string_policy)
        } else {
            raw
        };
    };
    let path = escape_sql_string(&json_path(key));
    let base = format!("JSON_EXTRACT({source_column}, '{path}')");
    let unquoted = format!("JSON_UNQUOTE({base})");
    let upper = column.sql_type.to_ascii_uppercase();
    let value = if is_text_sql_type(&column.sql_type) {
        apply_empty_string_policy(unquoted, empty_string_policy)
    } else if upper.starts_with("BIGINT")
        || upper.starts_with("INT")
        || upper.starts_with("SMALLINT")
        || upper.starts_with("MEDIUMINT")
    {
        format!("CAST({unquoted} AS SIGNED)")
    } else if upper.starts_with("DECIMAL") || upper.starts_with("NUMERIC") {
        format!("CAST({unquoted} AS DECIMAL(20, 6))")
    } else if upper.starts_with("TINYINT") || upper.contains("BOOLEAN") {
        format!(
            "CASE WHEN JSON_TYPE({base}) = 'BOOLEAN' THEN IF({base} = TRUE, 1, 0) \
             WHEN {base} IS NULL THEN NULL ELSE CAST({unquoted} AS SIGNED) END"
        )
    } else if upper == "JSON" {
        base.clone()
    } else {
        unquoted
    };
    format!(
        "CASE WHEN {source_column} IS NOT NULL AND JSON_VALID({source_column}) = 1 \
         AND JSON_CONTAINS_PATH({source_column}, 'one', '{path}') \
         THEN {value} ELSE NULL END"
    )
}

fn mismatch_condition(
    source_column: &str,
    columns: &[TargetColumn],
    empty_string_policy: EmptyStringPolicy,
) -> String {
    let mut comparisons = Vec::new();
    for column in columns {
        let target_column = quote_ident_mysql(&column.name);
        if let Some(key) = column.source_key.as_deref() {
            let path = escape_sql_string(&json_path(key));
            let present = format!(
                "COALESCE(JSON_CONTAINS_PATH(s.{source_column}, 'one', '{path}'), 0)"
            );
            let source_token = format!("JSON_UNQUOTE(JSON_EXTRACT(s.{source_column}, '{path}'))");
            let source_type = format!("JSON_TYPE(JSON_EXTRACT(s.{source_column}, '{path}'))");
            if is_numeric_sql_type(&column.sql_type) {
                // 文档 4.2(3)：数值按十进制词元比较，禁止先转 f64；
                // 1 与 1.0 经 DECIMAL 规范化后应判为一致。
                comparisons.push(format!(
                    "({present} = 0 AND t.{target_column} IS NOT NULL)\n\
                     OR ({present} = 1\n\
                         AND CASE\n\
                           WHEN {source_type} IN ('INTEGER', 'DOUBLE', 'BOOLEAN') THEN\n\
                             t.{target_column} IS NULL\n\
                             OR CAST(t.{target_column} AS DECIMAL(65,30))\n\
                                <> CAST({source_token} AS DECIMAL(65,30))\n\
                           WHEN {source_type} = 'NULL' THEN t.{target_column} IS NOT NULL\n\
                           ELSE COALESCE(NULLIF({source_token}, ''), '<NULL>')\n\
                                <> COALESCE(CAST(t.{target_column} AS CHAR), '<NULL>')\n\
                         END)"
                ));
            } else {
                // 文档 4.2(1)：SQL NULL 与空串严格区分。
                // 旧写法用 COALESCE(NULLIF(token,''), '<NULL>') 会把两者抹平，
                // 导致“目标把空串写成 NULL”无法被发现。
                let strict = match empty_string_policy {
                    EmptyStringPolicy::ToNull => format!(
                        "({present} = 0 AND t.{target_column} IS NOT NULL)\n\
                         OR ({present} = 1 AND (\n\
                             (({source_token} IS NULL OR {source_token} = '') AND t.{target_column} IS NOT NULL)\n\
                             OR ({source_token} IS NOT NULL AND {source_token} <> '' AND\n\
                                 (t.{target_column} IS NULL\n\
                                  OR CAST(t.{target_column} AS BINARY) <> CAST({source_token} AS BINARY))\n\
                             )\n\
                           ))"
                    ),
                    EmptyStringPolicy::Preserve | EmptyStringPolicy::Block => format!(
                        "({present} = 0 AND t.{target_column} IS NOT NULL)\n\
                         OR ({present} = 1 AND (\n\
                             ({source_token} IS NULL AND t.{target_column} IS NOT NULL)\n\
                             OR ({source_token} IS NOT NULL AND\n\
                                 (t.{target_column} IS NULL\n\
                                  OR CAST(t.{target_column} AS BINARY) <> CAST({source_token} AS BINARY))\n\
                             )\n\
                           ))"
                    ),
                };
                comparisons.push(strict);
            }
        } else if is_text_sql_type(&column.sql_type) {
            // 标量列：NULL-safe 且按二进制原文比较，空串 ≠ NULL。
            let strict = match empty_string_policy {
                EmptyStringPolicy::ToNull => format!(
                    "((s.{source_column} IS NULL OR s.{source_column} = '') AND t.{target_column} IS NOT NULL)\n\
                     OR (s.{source_column} IS NOT NULL AND s.{source_column} <> '' AND\n\
                         (t.{target_column} IS NULL\n\
                          OR CAST(s.{source_column} AS BINARY) <> CAST(t.{target_column} AS BINARY)))"
                ),
                EmptyStringPolicy::Preserve | EmptyStringPolicy::Block => format!(
                    "NOT ((s.{source_column} IS NULL AND t.{target_column} IS NULL)\n\
                          OR (s.{source_column} IS NOT NULL AND t.{target_column} IS NOT NULL\n\
                              AND CAST(s.{source_column} AS BINARY) = CAST(t.{target_column} AS BINARY)))"
                ),
            };
            comparisons.push(strict);
        } else {
            comparisons.push(format!(
                "COALESCE(CAST(s.{source_column} AS CHAR), '<NULL>') \
                 <> COALESCE(CAST(t.{target_column} AS CHAR), '<NULL>')"
            ));
        }
    }
    if comparisons.is_empty() {
        "FALSE".to_string()
    } else {
        comparisons.join("\n       OR ")
    }
}

/// V2/V3/V4 的差异主键选择语句；调用方可再追加主键区间与页大小。
pub(crate) fn diff_select(
    phase: &str,
    source: &str,
    destination: &str,
    source_column: &str,
    pk: &str,
    columns: &[TargetColumn],
    empty_string_policy: EmptyStringPolicy,
) -> String {
    match phase {
        "v2" => format!(
            "SELECT s.{pk} AS pk_value FROM {source} s LEFT JOIN {destination} t ON t.{pk} = s.{pk} \
             WHERE t.{pk} IS NULL"
        ),
        "v3" => format!(
            "SELECT t.{pk} AS pk_value FROM {destination} t LEFT JOIN {source} s ON s.{pk} = t.{pk} \
             WHERE s.{pk} IS NULL"
        ),
        "v4" => {
            let mismatch = mismatch_condition(source_column, columns, empty_string_policy);
            format!(
                "SELECT s.{pk} AS pk_value FROM {source} s JOIN {destination} t ON t.{pk} = s.{pk} \
                 WHERE {mismatch}"
            )
        }
        _ => format!("SELECT s.{pk} AS pk_value FROM {source} s WHERE 0"),
    }
}

/// 差异总数（不截断）。
pub(crate) fn diff_total_sql(select: &str) -> String {
    format!("SELECT COUNT(*) AS diff_total FROM (\n{select}\n) AS diff")
}

/// 按主键区间分页；`last_pk` 传上一页最后一个主键（首次为空），`page_size` 为页大小。
pub(crate) fn diff_page_sql(
    select: &str,
    key_expr: &str,
    last_pk: Option<&str>,
    page_size: &str,
) -> String {
    match last_pk {
        Some(last) => format!(
            "{select}\n  AND {key_expr} > {last}\nORDER BY {key_expr}\nLIMIT {page_size}"
        ),
        None => format!("{select}\nORDER BY {key_expr}\nLIMIT {page_size}"),
    }
}

fn build_verify_sql(
    source: &str,
    destination: &str,
    source_column: &str,
    pk: &str,
    columns: &[TargetColumn],
    empty_string_policy: EmptyStringPolicy,
) -> String {
    let v2 = diff_select(
        "v2",
        source,
        destination,
        source_column,
        pk,
        columns,
        empty_string_policy,
    );
    let v3 = diff_select(
        "v3",
        source,
        destination,
        source_column,
        pk,
        columns,
        empty_string_policy,
    );
    let v4 = diff_select(
        "v4",
        source,
        destination,
        source_column,
        pk,
        columns,
        empty_string_policy,
    );
    let policy_note = format!(
        "空串策略={}（{}）；SQL NULL / 空串 / JSON null 严格区分。",
        empty_string_policy.as_str(),
        empty_string_policy.label()
    );
    let s_key = format!("s.{pk}");
    let t_key = format!("t.{pk}");
    format!(
        "-- 阶段 verify：只读校验，不写库。\n\
         -- {policy_note}\n\
         -- V1 行数（源表与目标表；目标表可能只含已回填部分）\n\
         SELECT (SELECT COUNT(*) FROM {source}) AS source_rows, \
         (SELECT COUNT(*) FROM {destination}) AS target_rows;\n\n\
         -- V2 count（源表有、目标表缺失的差异总数；不截断）\n\
         {};\n\n\
         -- V2 page（差异主键分页；:last_pk 传上一页最后一个主键，首次从最小主键开始）\n\
         {};\n\n\
         -- V3 count（目标表多余主键的差异总数；不截断）\n\
         {};\n\n\
         -- V3 page（差异主键分页；:last_pk 同上）\n\
         {};\n\n\
         -- V4 count（规范化逐行比对的差异总数；不截断）\n\
         {};\n\n\
         -- V4 page（差异主键分页；:last_pk 同上）\n\
         {};",
        diff_total_sql(&v2),
        diff_page_sql(&v2, &s_key, Some(":last_pk"), ":page_size"),
        diff_total_sql(&v3),
        diff_page_sql(&v3, &t_key, Some(":last_pk"), ":page_size"),
        diff_total_sql(&v4),
        diff_page_sql(&v4, &s_key, Some(":last_pk"), ":page_size"),
    )
}

fn build_rollback_sql(destination: &str, pk: &str) -> String {
    format!(
        "-- 回滚：清空目标表数据（仅在源列仍存在、可重新回填时使用）\n\
         -- 若目标表存在其他写入方，禁止执行。\n\
         DELETE FROM {destination} WHERE {pk} >= :min_pk AND {pk} <= :max_pk;"
    )
}

fn build_retire_sql(source: &str, source_column: &str, raw_column: &str) -> String {
    format!(
        "-- 阶段 L4：下线旧列（不可逆，永不自动执行）\n\
         -- 前置检查（人工执行并确认）：\n\
         --   1) 『校验 SQL』最终 mismatch=0；\n\
         --   2) 旧列已按主键完整备份到影子表，并校验行数与 checksum；\n\
         --   3) 所有读路径已切换到目标表，写路径已停止写入旧列；\n\
         --   4) 兼容期结束且无回滚需求。\n\
         -- 目标列：{}\n\
         ALTER TABLE {source} DROP COLUMN {source_column};",
        raw_column
    )
}

fn dry_run_report(scripts: &[ManualScript]) -> DryRunReport {
    let mut checks = Vec::new();
    for script in scripts {
        checks.push(DryRunCheck {
            script_id: script.id.clone(),
            check: "script_is_display_only".into(),
            status: if script.executable { "fail" } else { "pass" }.into(),
            detail: "S1 所有脚本 executable=false".into(),
        });
        checks.push(DryRunCheck {
            script_id: script.id.clone(),
            check: "phase_declared".into(),
            status: if script.phase.trim().is_empty() { "fail" } else { "pass" }.into(),
            detail: format!("phase={}", script.phase),
        });
        let blocked = classify_blocked_sql(&script.sql);
        let (check, status, detail) = match script.kind.as_str() {
            "retire" => (
                "safety_baseline_blocks_retire",
                if blocked.is_some() { "pass" } else { "fail" },
                "DROP COLUMN 必须被安全基线拦截".to_string(),
            ),
            "rollback" if script.irreversible => (
                "safety_baseline_blocks_drop_table",
                if blocked.is_some() { "pass" } else { "fail" },
                "DROP TABLE 必须被安全基线拦截".to_string(),
            ),
            _ => (
                "safety_baseline_allows_manual_preview",
                if blocked.is_none() { "pass" } else { "fail" },
                "建表/回填/校验不应触发硬拦截；仍需人工执行".to_string(),
            ),
        };
        checks.push(DryRunCheck {
            script_id: script.id.clone(),
            check: check.into(),
            status: status.into(),
            detail,
        });
        if script.kind == "backfill" {
            let upper = script.sql.to_ascii_uppercase();
            checks.push(DryRunCheck {
                script_id: script.id.clone(),
                check: "backfill_is_idempotent".into(),
                status: if upper.contains("ON DUPLICATE KEY UPDATE")
                    && !upper.contains(" REPLACE ")
                {
                    "pass"
                } else {
                    "fail"
                }
                .into(),
                detail: "使用 ON DUPLICATE KEY UPDATE，禁止 REPLACE".into(),
            });
        }
        if script.kind == "retire" {
            checks.push(DryRunCheck {
                script_id: script.id.clone(),
                check: "retire_contains_drop_column".into(),
                status: if script.sql.to_ascii_uppercase().contains("DROP COLUMN") {
                    "pass"
                } else {
                    "fail"
                }
                .into(),
                detail: "下线脚本必须明确写出 DROP COLUMN".into(),
            });
        }
    }
    let passed = checks.iter().all(|check| check.status == "pass");
    DryRunReport {
        executed_against_database: false,
        passed,
        checks,
        note: "dry-run 只做 SQL 结构与安全基线解析，不连接数据库执行。".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::field_migration::types::{
        sample_column, ColumnMapping, DependencyEvidence, FieldGovernanceAnalysis, RiskSummary,
        TargetSpec,
    };
    use crate::ops::types::{ColumnSnapshot, TableSnapshot};

    fn snapshot() -> TableSnapshot {
        TableSnapshot {
            db_name: "app".into(),
            table_name: "orders".into(),
            columns: vec![ColumnSnapshot {
                name: "id".into(),
                column_type: "bigint".into(),
                nullable: false,
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn analysis() -> FieldGovernanceAnalysis {
        let mut column = sample_column("payload", "json");
        column.json_key_types = vec![
            super::super::types::KeyTypeDistribution {
                key: "theme".into(),
                types: [("STRING".to_string(), 10)].into_iter().collect(),
            },
            super::super::types::KeyTypeDistribution {
                key: "age".into(),
                types: [("INTEGER".to_string(), 10)].into_iter().collect(),
            },
        ];
        column.json_keys = vec!["theme".into(), "age".into()];
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
            columns: vec![column],
            dependencies: DependencyEvidence {
                primary_key_columns: vec!["id".into()],
                single_column_pk: Some("id".into()),
                scan_complete: true,
                ..Default::default()
            },
            environment: Default::default(),
            evidence: Vec::new(),
            gaps: Vec::new(),
            declarations: Vec::new(),
            declaration_hash: String::new(),
            declaration_evidence: Vec::new(),
            target_table: "orders_payload_ext".into(),
            target_exists: false,
            empty_string_policy: FieldGovernanceInput::default().empty_string_policy(),
            target_columns: Vec::new(),
            risks: Vec::new(),
            risk_summary: RiskSummary::default(),
            scripts: Vec::new(),
            dry_run: Default::default(),
        }
    }

    fn input() -> FieldGovernanceInput {
        FieldGovernanceInput {
            table: "orders".into(),
            column: Some("payload".into()),
            ..Default::default()
        }
    }

    #[test]
    fn generates_all_required_scripts_without_execution() {
        let mut analysis = analysis();
        generate(&snapshot(), &mut analysis, &input());
        let kinds: Vec<&str> = analysis.scripts.iter().map(|s| s.kind.as_str()).collect();
        assert!(kinds.contains(&"create_table"));
        assert!(kinds.contains(&"backfill"));
        assert!(kinds.contains(&"verify"));
        assert!(kinds.contains(&"rollback"));
        assert!(kinds.contains(&"retire"));
        assert!(analysis.scripts.iter().all(|script| !script.executable));
        assert!(analysis.dry_run.passed, "{:?}", analysis.dry_run);
        assert!(!analysis.dry_run.executed_against_database);
    }

    #[test]
    fn retire_and_drop_table_are_marked_irreversible() {
        let mut analysis = analysis();
        generate(&snapshot(), &mut analysis, &input());
        let retire = analysis
            .scripts
            .iter()
            .find(|script| script.kind == "retire")
            .unwrap();
        assert!(retire.irreversible);
        assert!(retire.sql.contains("DROP COLUMN"));
        assert_eq!(retire.phase, "L4_retire");

        let drop = analysis
            .scripts
            .iter()
            .find(|script| script.id == "rollback-drop")
            .unwrap();
        assert!(drop.irreversible);
        assert!(drop.sql.contains("DROP TABLE"));
    }

    #[test]
    fn backfill_is_batched_and_idempotent() {
        let mut analysis = analysis();
        generate(&snapshot(), &mut analysis, &input());
        let backfill = analysis
            .scripts
            .iter()
            .find(|script| script.kind == "backfill")
            .unwrap();
        assert!(backfill.sql.contains(":last_pk"));
        assert!(backfill.sql.contains(":end_pk"));
        assert!(backfill.sql.contains("ON DUPLICATE KEY UPDATE"));
        assert!(!backfill.sql.to_ascii_uppercase().contains(" REPLACE "));
        assert!(backfill.sql.contains("CASE WHEN"));
    }

    #[test]
    fn verify_sql_covers_rows_pk_and_normalized_values() {
        let mut analysis = analysis();
        generate(&snapshot(), &mut analysis, &input());
        let verify = analysis
            .scripts
            .iter()
            .find(|script| script.kind == "verify")
            .unwrap();
        assert!(verify.sql.contains("source_rows"));
        assert!(verify.sql.contains("LEFT JOIN"));
        assert!(verify.sql.contains("JSON_CONTAINS_PATH"));
        assert!(verify.sql.contains("<NULL>"));
    }

    #[test]
    fn verify_sql_normalizes_numeric_tokens() {
        let mut analysis = analysis();
        let mut inp = input();
        inp.target = Some(TargetSpec {
            mappings: vec![
                ColumnMapping {
                    source_key: Some("age".into()),
                    target_column: "age".into(),
                    target_type: Some("BIGINT".into()),
                    required: None,
                },
                ColumnMapping {
                    source_key: Some("theme".into()),
                    target_column: "theme".into(),
                    target_type: Some("VARCHAR(512)".into()),
                    required: None,
                },
            ],
            ..Default::default()
        });
        generate(&snapshot(), &mut analysis, &inp);
        let verify = analysis
            .scripts
            .iter()
            .find(|script| script.kind == "verify")
            .unwrap();
        // 数值列按 DECIMAL(65,30) 规范化，1 与 1.0 判定一致；字符串列仍按原文比较。
        assert!(verify.sql.contains("CAST(t.`age` AS DECIMAL(65,30))"));
        assert!(verify
            .sql
            .contains("CAST(JSON_UNQUOTE(JSON_EXTRACT(s.`payload`, '$.\"age\"')) AS DECIMAL(65,30))"));
        // 源列为 SQL NULL 时 JSON_CONTAINS_PATH 返回 NULL，必须归一为 0，
        // 否则“源 NULL + 目标非 NULL”会被漏判。
        assert!(verify.sql.contains("COALESCE(JSON_CONTAINS_PATH(s.`payload`"));
        assert!(!verify.sql.contains("CAST(t.`theme` AS DECIMAL"));
    }

    #[test]
    fn backfill_preserves_empty_string_by_default() {
        let mut analysis = analysis();
        generate(&snapshot(), &mut analysis, &input());
        let backfill = analysis
            .scripts
            .iter()
            .find(|script| script.kind == "backfill")
            .unwrap();
        assert!(backfill.sql.contains("空串策略 preserve"));
        assert!(!backfill.sql.contains("NULLIF(JSON_UNQUOTE"));
        assert!(!backfill.sql.contains("NULLIF(`payload`"));
        assert!(backfill.sql.contains("JSON_UNQUOTE(JSON_EXTRACT(`payload`"));
        assert_eq!(analysis.empty_string_policy, EmptyStringPolicy::Preserve);
        assert!(!analysis.target_columns.is_empty());
    }

    #[test]
    fn backfill_to_null_is_explicit_and_never_silent() {
        let mut analysis = analysis();
        let mut inp = input();
        inp.empty_string_policy = Some(EmptyStringPolicy::ToNull);
        generate(&snapshot(), &mut analysis, &inp);
        let backfill = analysis
            .scripts
            .iter()
            .find(|script| script.kind == "backfill")
            .unwrap();
        assert!(backfill.sql.contains("空串策略 to_null"));
        assert!(backfill.sql.contains("NULLIF(JSON_UNQUOTE(JSON_EXTRACT(`payload`"));
        assert_eq!(analysis.empty_string_policy, EmptyStringPolicy::ToNull);
    }

    #[test]
    fn backfill_block_keeps_source_but_marks_script_blocked() {
        let mut analysis = analysis();
        let mut inp = input();
        inp.empty_string_policy = Some(EmptyStringPolicy::Block);
        generate(&snapshot(), &mut analysis, &inp);
        let backfill = analysis
            .scripts
            .iter()
            .find(|script| script.kind == "backfill")
            .unwrap();
        assert!(backfill.sql.contains("空串策略 block"));
        assert!(!backfill.sql.contains("NULLIF(JSON_UNQUOTE"));
        assert!(!backfill.sql.contains("NULLIF(`payload`"));
    }

    #[test]
    fn verify_sql_paginates_without_fixed_limit_and_keeps_empty_vs_null() {
        let mut analysis = analysis();
        generate(&snapshot(), &mut analysis, &input());
        let verify = analysis
            .scripts
            .iter()
            .find(|script| script.kind == "verify")
            .unwrap();
        assert!(!verify.sql.contains("LIMIT 100;"));
        assert!(verify.sql.contains("-- V2 count"));
        assert!(verify.sql.contains("-- V2 page"));
        assert!(verify.sql.contains("-- V3 count"));
        assert!(verify.sql.contains("-- V4 count"));
        assert!(verify.sql.contains("COUNT(*) AS diff_total"));
        assert!(verify.sql.contains(":last_pk"));
        assert!(verify.sql.contains(":page_size"));
        // 字符串列不再用 COALESCE(NULLIF(...), '<NULL>') 抹平空串与 NULL。
        assert!(!verify
            .sql
            .contains("COALESCE(NULLIF(JSON_UNQUOTE(JSON_EXTRACT(s.`payload`, '$.\"theme\"'))"));
    }

    #[test]
    fn inventory_mode_generates_no_scripts() {
        let mut analysis = analysis();
        analysis.column = None;
        generate(&snapshot(), &mut analysis, &input());
        assert!(analysis.scripts.is_empty());
        assert!(analysis
            .dry_run
            .note
            .contains("整表字段盘点不生成迁移脚本"));
    }
}
