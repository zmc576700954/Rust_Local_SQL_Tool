//! PostgreSQL 方言的字段治理脚本生成。
//!
//! 所有脚本 `executable=false`，本模块不连接数据库执行任何语句。
//! 覆盖：建表、分批回填（`ON CONFLICT`/`EXCLUDED`）、V1–V4 校验、
//! 回滚、下线，以及 PG 特有的重写类脚本（改类型/`SET NOT NULL`/改 collation）。

use crate::ops::safety::classify_blocked_sql;
use crate::ops::types::TableSnapshot;
use crate::sql::util::{escape_sql_string, quote_ident_pg};

use super::pg_evidence::pg_type_changed;
use super::schema::TargetColumn;
use super::scripts::dry_run_report;
use super::types::{
    ColumnProfile, DryRunReport, EmptyStringPolicy, FieldGovernanceAnalysis, FieldGovernanceInput,
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

    let schema = analysis.environment.schema.clone();
    let source = qualified(schema.as_deref(), &analysis.table);
    let destination = qualified(schema.as_deref(), &analysis.target_table);
    let pk_name = input
        .target
        .as_ref()
        .and_then(|target| target.pk_column.clone())
        .or_else(|| analysis.dependencies.single_column_pk.clone())
        .unwrap_or_else(|| "id".into());
    let pk_type = snapshot
        .find_column(&pk_name)
        .map(|column| column.column_type.clone())
        .unwrap_or_else(|| "bigint".into());
    let pk = quote_ident_pg(&pk_name);
    let source_column = quote_ident_pg(&profile.column);

    let target_columns = pg_target_columns(input, &profile);
    analysis.empty_string_policy = input.empty_string_policy();
    analysis.target_columns = target_columns.clone();

    let create_sql = create_table_sql_pg(
        schema.as_deref(),
        &analysis.target_table,
        &pk_name,
        &pk_type,
        &target_columns,
        input,
    );
    let backfill_sql = build_backfill_sql_pg(
        &source,
        &destination,
        &pk,
        &source_column,
        &profile,
        &target_columns,
        analysis.empty_string_policy,
    );
    let verify_sql = build_verify_sql_pg(
        &source,
        &destination,
        &pk,
        &source_column,
        &profile,
        &target_columns,
        analysis.empty_string_policy,
    );
    let rollback_delete_sql = format!(
        "DELETE FROM {destination} WHERE {pk} >= :min_pk;"
    );
    let rollback_drop_sql = format!("DROP TABLE IF EXISTS {destination};");
    let retire_sql = format!("ALTER TABLE {source} DROP COLUMN {source_column};");

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
                "PostgreSQL 方言：无 ENGINE / DEFAULT CHARSET；类型由证据推断。".into(),
                "只生成，不执行；执行前确认目标表不存在或使用 IF NOT EXISTS。".into(),
                "目标 collation 以列级 COLLATE 表达。".into(),
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
                "按主键 keyset 分批；每批独立事务，失败只重跑该批。".into(),
                "ON CONFLICT (pk) DO UPDATE + EXCLUDED 保证幂等。".into(),
                "必须替换 :last_pk / :end_pk / :batch_size 占位符。".into(),
                format!(
                    "空串策略={}（{}）；SQL NULL 与空串语义不同。",
                    analysis.empty_string_policy.as_str(),
                    analysis.empty_string_policy.label()
                ),
                if analysis.dependencies.single_column_pk.is_none() {
                    "源表没有单列主键：分片不安全，禁止自动回填。".into()
                } else {
                    "回填期间需评估写入并发；S2 才提供执行器。".into()
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
                "V1 行数、V2 缺失主键、V3 多余主键、V4 规范化逐行比对。".into(),
                "numeric 用 numeric 比较；timestamptz 两侧 AT TIME ZONE 'UTC'；bytea 用 encode(...,'hex')。".into(),
                "只生成 SQL，不连接数据库执行。".into(),
            ],
        },
        ManualScript {
            id: "rollback-delete".into(),
            title: "回滚：删除回填数据".into(),
            kind: "rollback".into(),
            phase: "rollback".into(),
            sql: rollback_delete_sql,
            executable: false,
            irreversible: false,
            notes: vec!["按主键下界删除；需人工确认范围。".into()],
        },
        ManualScript {
            id: "rollback-drop".into(),
            title: "回滚：删除目标表".into(),
            kind: "rollback".into(),
            phase: "rollback".into(),
            sql: rollback_drop_sql,
            executable: false,
            irreversible: true,
            notes: vec!["不可逆；必须先确认目标表只包含本次回填数据。".into()],
        },
        ManualScript {
            id: "retire-column".into(),
            title: "下线：删除源列".into(),
            kind: "retire".into(),
            phase: "retire".into(),
            sql: retire_sql,
            executable: false,
            irreversible: true,
            notes: vec![
                "不可逆；只在观察期结束、依赖清零后人工执行。".into(),
                "PG 的 DROP COLUMN 会重写元数据但保留物理空间，需 VACUUM FULL 才能回收。".into(),
            ],
        },
    ];

    push_rewrite_scripts(
        &mut scripts,
        input,
        &profile,
        schema.as_deref(),
        &analysis.table,
        &source_column,
    );

    analysis.scripts = scripts;
    analysis.dry_run = dry_run_report(&analysis.scripts);
}

fn qualified(schema: Option<&str>, table: &str) -> String {
    match schema {
        Some(schema) => format!("{}.{}", quote_ident_pg(schema), quote_ident_pg(table)),
        None => quote_ident_pg(table),
    }
}

fn create_table_sql_pg(
    schema: Option<&str>,
    target_table: &str,
    pk_name: &str,
    pk_type: &str,
    columns: &[TargetColumn],
    input: &FieldGovernanceInput,
) -> String {
    let destination = qualified(schema, target_table);
    let collation = input
        .target
        .as_ref()
        .and_then(|target| target.collation.as_deref());
    let mut lines = Vec::new();
    lines.push(format!("  {} {} NOT NULL", quote_ident_pg(pk_name), pk_type));
    for column in columns {
        let pg_type = mysql_to_pg_type(&column.sql_type);
        let nullability = if column.required { "NOT NULL" } else { "NULL" };
        let collate = if collation.is_some() && is_text_pg_type(&pg_type) {
            format!(" COLLATE {}", quote_ident_pg(collation.unwrap()))
        } else {
            String::new()
        };
        lines.push(format!(
            "  {} {}{} {}",
            quote_ident_pg(&column.name),
            pg_type,
            collate,
            nullability
        ));
    }
    lines.push(format!("  PRIMARY KEY ({})", quote_ident_pg(pk_name)));
    format!(
        "CREATE TABLE IF NOT EXISTS {destination} (\n{}\n);",
        lines.join(",\n")
    )
}

fn push_rewrite_scripts(
    scripts: &mut Vec<ManualScript>,
    input: &FieldGovernanceInput,
    profile: &ColumnProfile,
    schema: Option<&str>,
    table: &str,
    source_column: &str,
) {
    let source = qualified(schema, table);
    let mapping_type = input.target.as_ref().and_then(|target| {
        target
            .mappings
            .iter()
            .find(|mapping| {
                mapping
                    .source_key
                    .as_deref()
                    .map(|key| key.eq_ignore_ascii_case(&profile.column))
                    .unwrap_or(true)
            })
            .and_then(|mapping| mapping.target_type.clone())
    });

    if let Some(target_type) = mapping_type {
        if pg_type_changed(&profile.column_type, &target_type) {
            scripts.push(ManualScript {
                id: "alter-type".into(),
                title: "重写类：修改列类型（仅脚本）".into(),
                kind: "alter_table".into(),
                phase: "L2.5_alter_type".into(),
                sql: format!(
                    "ALTER TABLE {source} ALTER COLUMN {source_column} TYPE {} USING {source_column}::{};",
                    target_type, target_type
                ),
                executable: false,
                irreversible: false,
                notes: vec![
                    "PG 改类型可能触发表重写，必须维护窗口 + 备份。".into(),
                    "USING 转换失败会直接报错；已由 E01 预检记录不可转换行数。".into(),
                    "S1 只出脚本，不生成可执行计划。".into(),
                ],
            });
        }
    }

    let required = input.target.as_ref().map(|target| {
        target.mappings.iter().any(|mapping| {
            let applies = mapping
                .source_key
                .as_deref()
                .map(|key| key.eq_ignore_ascii_case(&profile.column))
                .unwrap_or(true);
            applies && mapping.required == Some(true)
        })
    });
    if required == Some(true) {
        let constraint = quote_ident_pg(&format!("fm_not_null_{}", profile.column));
        scripts.push(ManualScript {
            id: "set-not-null".into(),
            title: "重写类：SET NOT NULL 安全路径（仅脚本）".into(),
            kind: "alter_table".into(),
            phase: "L2.5_set_not_null".into(),
            sql: format!(
                "ALTER TABLE {source} ADD CONSTRAINT {constraint} CHECK ({source_column} IS NOT NULL) NOT VALID;\n\
                 ALTER TABLE {source} VALIDATE CONSTRAINT {constraint};\n\
                 ALTER TABLE {source} ALTER COLUMN {source_column} SET NOT NULL;\n\
                 ALTER TABLE {source} DROP CONSTRAINT {constraint};"
            ),
            executable: false,
            irreversible: false,
            notes: vec![
                "SET NOT NULL 会全表校验；使用 CHECK NOT VALID → VALIDATE → SET NOT NULL 降低锁范围。".into(),
                "VALIDATE 仍需扫描全表，必须维护窗口。".into(),
            ],
        });
    }

    if let (Some(target), Some(source_collation)) = (
        input.target.as_ref().and_then(|target| target.collation.as_deref()),
        profile.collation.as_deref(),
    ) {
        if !source_collation.eq_ignore_ascii_case(target) && profile.is_text_like() {
            scripts.push(ManualScript {
                id: "alter-collation".into(),
                title: "重写类：修改排序规则（仅脚本）".into(),
                kind: "alter_table".into(),
                phase: "L2.5_alter_collation".into(),
                sql: format!(
                    "ALTER TABLE {source} ALTER COLUMN {source_column} TYPE {} COLLATE {} USING {source_column}::{};",
                    profile.column_type, quote_ident_pg(target), profile.column_type
                ),
                executable: false,
                irreversible: false,
                notes: vec![
                    "PG 改 collation 会重写表与所有索引，必须维护窗口。".into(),
                    "非确定性 collation 已被 E13_PG_NONDETERMINISTIC_COLLATION 阻断。".into(),
                ],
            });
        }
    }
}

fn is_text_pg_type(pg_type: &str) -> bool {
    let lower = pg_type.to_ascii_lowercase();
    lower.contains("char")
        || lower.contains("text")
        || lower.starts_with("json")
}

/// MySQL 风格类型 → PG 类型；已经是 PG 类型名时原样返回。
pub fn mysql_to_pg_type(sql_type: &str) -> String {
    let trimmed = sql_type.trim();
    let upper = trimmed.to_ascii_uppercase();
    if upper.contains("WITH TIME ZONE") || upper.contains("WITHOUT TIME ZONE") {
        return trimmed.to_string();
    }
    let base = upper
        .split(|ch: char| ch == '(' || ch.is_whitespace())
        .next()
        .unwrap_or("");
    let args = trimmed
        .find('(')
        .and_then(|open| trimmed.rfind(')').map(|close| trimmed[open + 1..close].to_string()));
    match base {
        "VARCHAR" | "CHARACTER" | "CHAR" => {
            let len = args.unwrap_or_else(|| "64".into());
            if base == "VARCHAR" || upper.contains("VARYING") {
                format!("character varying({len})")
            } else {
                format!("character({len})")
            }
        }
        "TINYTEXT" | "TEXT" | "MEDIUMTEXT" | "LONGTEXT" => "text".into(),
        "TINYINT" => {
            if args.as_deref() == Some("1") {
                "boolean".into()
            } else {
                "smallint".into()
            }
        }
        "SMALLINT" => "smallint".into(),
        "MEDIUMINT" | "INT" | "INTEGER" => "integer".into(),
        "BIGINT" => "bigint".into(),
        "DECIMAL" | "NUMERIC" => match args {
            Some(args) if !args.trim().is_empty() => format!("numeric({args})"),
            _ => "numeric".into(),
        },
        "FLOAT" => "real".into(),
        "DOUBLE" => "double precision".into(),
        "DATE" => "date".into(),
        "DATETIME" | "TIMESTAMP" => "timestamp without time zone".into(),
        "TIMESTAMPTZ" => "timestamp with time zone".into(),
        "TIME" => "time without time zone".into(),
        "YEAR" => "smallint".into(),
        "JSON" | "JSONB" => "jsonb".into(),
        "BYTEA" | "BLOB" | "TINYBLOB" | "MEDIUMBLOB" | "LONGBLOB" => "bytea".into(),
        "BOOL" | "BOOLEAN" => "boolean".into(),
        "VARYING" => format!(
            "character varying({})",
            args.unwrap_or_else(|| "64".into())
        ),
        _ => trimmed.to_string(),
    }
}

fn pg_type_for_profile(profile: &ColumnProfile) -> String {
    match profile.data_type.as_str() {
        "jsonb" => "jsonb".into(),
        "json" => "json".into(),
        "text" => "text".into(),
        "varchar" => format!(
            "character varying({})",
            profile
                .char_max_length
                .unwrap_or(255)
                .max(profile.max_char_length)
                .min(65535)
        ),
        "bpchar" | "char" => format!(
            "character({})",
            profile.char_max_length.unwrap_or(64)
        ),
        "int2" => "smallint".into(),
        "int4" => "integer".into(),
        "int8" => "bigint".into(),
        "numeric" => format!(
            "numeric({},{})",
            profile.numeric_precision.unwrap_or(20),
            profile.numeric_scale.unwrap_or(6)
        ),
        "float4" => "real".into(),
        "float8" => "double precision".into(),
        "bool" => "boolean".into(),
        "bytea" => "bytea".into(),
        "date" => "date".into(),
        "timestamp" => "timestamp without time zone".into(),
        "timestamptz" => "timestamp with time zone".into(),
        "time" => "time without time zone".into(),
        "timetz" => "time with time zone".into(),
        _ => {
            if profile.column_type.trim().is_empty() {
                "text".into()
            } else {
                profile.column_type.clone()
            }
        }
    }
}

fn pg_target_columns(input: &FieldGovernanceInput, profile: &ColumnProfile) -> Vec<TargetColumn> {
    let target = input.target.clone().unwrap_or_default();
    if !target.mappings.is_empty() {
        return target
            .mappings
            .iter()
            .map(|mapping| {
                let inferred = mapping
                    .source_key
                    .as_deref()
                    .and_then(|key| {
                        profile
                            .json_key_types
                            .iter()
                            .find(|item| item.key.eq_ignore_ascii_case(key))
                            .map(|item| pg_infer_json_type(&item.types))
                    })
                    .unwrap_or_else(|| pg_type_for_profile(profile));
                TargetColumn {
                    name: mapping.target_column.clone(),
                    source_key: mapping.source_key.clone(),
                    sql_type: mapping.target_type.clone().unwrap_or(inferred),
                    nullable: !mapping.required.unwrap_or(false),
                    required: mapping.required.unwrap_or(false),
                    note: Some("来自用户列映射".into()),
                }
            })
            .collect();
    }

    if !profile.json_key_types.is_empty() {
        let required: std::collections::HashSet<String> = input
            .declarations
            .iter()
            .flat_map(|declaration| declaration.required_keys.iter().cloned())
            .collect();
        let mut used = std::collections::HashSet::new();
        return profile
            .json_key_types
            .iter()
            .map(|key| {
                let mut name = super::schema::sanitize_identifier(&key.key);
                while !used.insert(name.clone()) {
                    name.push('_');
                }
                TargetColumn {
                    name,
                    source_key: Some(key.key.clone()),
                    sql_type: pg_infer_json_type(&key.types),
                    nullable: !required.contains(&key.key),
                    required: required.contains(&key.key),
                    note: Some("按 JSON key 类型分布推断（草案）".into()),
                }
            })
            .collect();
    }

    vec![TargetColumn {
        name: "value".into(),
        source_key: None,
        sql_type: pg_type_for_profile(profile),
        nullable: profile.nullable,
        required: false,
        note: Some("源列为标量，默认映射到 value".into()),
    }]
}

fn pg_infer_json_type(types: &std::collections::BTreeMap<String, u64>) -> String {
    let non_null: Vec<&str> = types
        .keys()
        .map(String::as_str)
        .filter(|t| *t != "NULL")
        .collect();
    if non_null.len() != 1 {
        return "jsonb".into();
    }
    match non_null[0] {
        "STRING" => "text".into(),
        "INTEGER" => "bigint".into(),
        "DOUBLE" => "numeric".into(),
        "BOOLEAN" => "boolean".into(),
        _ => "jsonb".into(),
    }
}

fn json_key_expr(source_column: &str, key: &str, as_text: bool) -> String {
    let key_literal = escape_sql_string(key);
    if as_text {
        format!("CAST({source_column} AS jsonb) ->> '{key_literal}'")
    } else {
        format!("CAST({source_column} AS jsonb) -> '{key_literal}'")
    }
}

fn json_guard(source_column: &str, key: &str) -> String {
    let key_literal = escape_sql_string(key);
    format!(
        "{source_column} IS NOT NULL AND jsonb_typeof(CAST({source_column} AS jsonb)) = 'object' \
         AND (CAST({source_column} AS jsonb) -> '{key_literal}') IS NOT NULL"
    )
}

fn cast_value(value: String, pg_type: &str, policy: EmptyStringPolicy) -> String {
    let lower = pg_type.to_ascii_lowercase();
    if lower.starts_with("json") {
        return value;
    }
    let value = if policy == EmptyStringPolicy::ToNull && is_text_pg_type(&lower) {
        format!("NULLIF({value}, '')")
    } else {
        value
    };
    if lower.contains("numeric")
        || lower.starts_with("bigint")
        || lower.starts_with("integer")
        || lower.starts_with("smallint")
    {
        format!("CAST({value} AS {pg_type})")
    } else if lower.contains("timestamp") || lower.starts_with("date") {
        format!("CAST({value} AS {pg_type})")
    } else if lower.starts_with("boolean") || lower == "bool" {
        format!("CAST({value} AS boolean)")
    } else if lower.starts_with("bytea") {
        format!("convert_to({value}, 'UTF8')")
    } else {
        value
    }
}

fn value_expr(
    source_column: &str,
    profile: &ColumnProfile,
    column: &TargetColumn,
    policy: EmptyStringPolicy,
) -> String {
    let pg_type = mysql_to_pg_type(&column.sql_type);
    let lower = pg_type.to_ascii_lowercase();
    match column.source_key.as_deref() {
        Some(key) => {
            let value = cast_value(
                json_key_expr(source_column, key, !lower.starts_with("json")),
                &pg_type,
                policy,
            );
            format!(
                "CASE WHEN {} THEN {value} ELSE NULL END",
                json_guard(source_column, key)
            )
        }
        None => {
            let value = if lower.starts_with("json") || profile.is_json_type() {
                source_column.to_string()
            } else if lower.starts_with("bytea") {
                if profile.data_type == "bytea" {
                    source_column.to_string()
                } else {
                    format!("convert_to(CAST({source_column} AS text), 'UTF8')")
                }
            } else if lower.starts_with("text") || lower.contains("char") {
                format!("CAST({source_column} AS text)")
            } else {
                cast_value(source_column.to_string(), &pg_type, policy)
            };
            value
        }
    }
}

fn build_backfill_sql_pg(
    source: &str,
    destination: &str,
    pk: &str,
    source_column: &str,
    profile: &ColumnProfile,
    columns: &[TargetColumn],
    policy: EmptyStringPolicy,
) -> String {
    let mut targets = vec![pk.to_string()];
    let mut values = vec![pk.to_string()];
    for column in columns {
        targets.push(quote_ident_pg(&column.name));
        values.push(value_expr(source_column, profile, column, policy));
    }
    let updates = columns
        .iter()
        .map(|column| {
            format!(
                "{} = EXCLUDED.{}",
                quote_ident_pg(&column.name),
                quote_ident_pg(&column.name)
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "INSERT INTO {destination} ({})\nSELECT {}\nFROM {source}\nWHERE {pk} > :last_pk AND {pk} <= :end_pk\nORDER BY {pk}\nLIMIT :batch_size\nON CONFLICT ({pk}) DO UPDATE SET {updates};",
        targets.join(", "),
        values.join(",\n       ")
    )
}

fn normalized_source_expr(
    source_column: &str,
    profile: &ColumnProfile,
    column: &TargetColumn,
    policy: EmptyStringPolicy,
) -> (String, String) {
    // 返回 (源表达式, 比较方式)
    let pg_type = mysql_to_pg_type(&column.sql_type);
    let lower = pg_type.to_ascii_lowercase();
    let raw = value_expr(source_column, profile, column, policy);
    if lower.starts_with("json") {
        ("s.src".into(), "json".into())
    } else if lower.contains("numeric")
        || lower.starts_with("bigint")
        || lower.starts_with("integer")
        || lower.starts_with("smallint")
    {
        (format!("CAST({raw} AS numeric)"), "numeric".into())
    } else if lower.contains("timestamp") {
        (format!("CAST({raw} AS {pg_type})"), "timestamptz".into())
    } else if lower.starts_with("bytea") {
        let hex = if column.source_key.is_none() && profile.data_type == "bytea" {
            format!("encode({source_column}, 'hex')")
        } else {
            format!("encode(convert_to({raw}, 'UTF8'), 'hex')")
        };
        (hex, "bytea".into())
    } else {
        (raw, "text".into())
    }
}

fn build_verify_sql_pg(
    source: &str,
    destination: &str,
    pk: &str,
    source_column: &str,
    profile: &ColumnProfile,
    columns: &[TargetColumn],
    policy: EmptyStringPolicy,
) -> String {
    let mismatch_conditions = columns
        .iter()
        .map(|column| {
            let target = format!("t.{}", quote_ident_pg(&column.name));
            let (expr, kind) = normalized_source_expr(source_column, profile, column, policy);
            match kind.as_str() {
                "numeric" | "text" => format!("{expr} IS DISTINCT FROM {target}"),
                "timestamptz" => format!(
                    "{expr} AT TIME ZONE 'UTC' IS DISTINCT FROM {target} AT TIME ZONE 'UTC'"
                ),
                "bytea" => format!("{expr} IS DISTINCT FROM encode({target}, 'hex')"),
                "json" => {
                    let value = column
                        .source_key
                        .as_deref()
                        .map(|key| json_key_expr(source_column, key, false))
                        .unwrap_or_else(|| source_column.to_string());
                    format!("{value} IS DISTINCT FROM {target}")
                }
                _ => format!("{expr} IS DISTINCT FROM {target}"),
            }
        })
        .collect::<Vec<_>>()
        .join(" OR ");
    let projected = columns
        .iter()
        .map(|column| quote_ident_pg(&column.name))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "-- V1 行数\n\
         SELECT 'V1_row_count' AS check_name, \
                (SELECT COUNT(*)::text FROM {source}) AS source_rows, \
                (SELECT COUNT(*)::text FROM {destination}) AS target_rows\n\
         UNION ALL\n\
         -- V2 缺失主键\n\
         SELECT 'V2_missing_pk', COUNT(*)::text, '' \
         FROM {source} s \
         WHERE NOT EXISTS (SELECT 1 FROM {destination} t WHERE t.{pk} = s.{pk})\n\
         UNION ALL\n\
         -- V3 多余主键\n\
         SELECT 'V3_extra_pk', COUNT(*)::text, '' \
         FROM {destination} t \
         WHERE NOT EXISTS (SELECT 1 FROM {source} s WHERE s.{pk} = t.{pk})\n\
         UNION ALL\n\
         -- V4 规范化逐行比对（{projected}）\n\
         SELECT 'V4_value_mismatch', COUNT(*)::text, '' \
         FROM {source} s JOIN {destination} t ON t.{pk} = s.{pk} \
         WHERE {mismatch_conditions};"
    )
}

/// 供测试与外部检查：确认生成的脚本全部不可执行。
pub fn assert_not_executable(scripts: &[ManualScript]) -> Result<(), String> {
    for script in scripts {
        if script.executable {
            return Err(format!("脚本 {} 不应可执行", script.id));
        }
        let blocked = classify_blocked_sql(&script.sql);
        if matches!(script.kind.as_str(), "retire") && blocked.is_none() {
            return Err(format!("下线脚本 {} 未被安全基线拦截", script.id));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mysql_types_convert_to_pg_and_keep_pg_types() {
        assert_eq!(mysql_to_pg_type("VARCHAR(512)"), "character varying(512)");
        assert_eq!(mysql_to_pg_type("TINYINT(1)"), "boolean");
        assert_eq!(mysql_to_pg_type("DECIMAL(20,6)"), "numeric(20,6)");
        assert_eq!(mysql_to_pg_type("JSON"), "jsonb");
        assert_eq!(
            mysql_to_pg_type("timestamp with time zone"),
            "timestamp with time zone"
        );
        assert_eq!(mysql_to_pg_type("jsonb"), "jsonb");
        assert_eq!(mysql_to_pg_type("bytea"), "bytea");
    }

    #[test]
    fn json_key_expr_uses_pg_operators() {
        assert_eq!(json_key_expr("payload", "theme", false), "CAST(payload AS jsonb) -> 'theme'");
        assert_eq!(json_key_expr("payload", "theme", true), "CAST(payload AS jsonb) ->> 'theme'");
        assert!(json_guard("payload", "theme").contains("jsonb_typeof"));
    }

    #[test]
    fn bytea_and_timestamptz_expressions_are_normalized() {
        let profile = ColumnProfile {
            column: "created_at".into(),
            data_type: "timestamptz".into(),
            ..Default::default()
        };
        let column = TargetColumn {
            name: "created_at".into(),
            source_key: None,
            sql_type: "timestamp with time zone".into(),
            nullable: true,
            required: false,
            note: None,
        };
        let (expr, kind) =
            normalized_source_expr("created_at", &profile, &column, EmptyStringPolicy::Preserve);
        assert_eq!(kind, "timestamptz");
        assert!(expr.contains("timestamp with time zone"));
    }
}
