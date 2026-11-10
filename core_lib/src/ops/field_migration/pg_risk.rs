//! PostgreSQL 特有的字段治理风险规则。
//!
//! 在通用规则之后追加，只负责 PG 独有/语义不同的破坏性场景：
//! - 非普通表（relkind 非 r/p）
//! - 分区表（通用规则已有，这里不重复）
//! - 非确定性排序规则
//! - 目标 collation 变更导致表重写
//! - 类型变更导致表重写 + `USING` 预检结果
//! - `SET NOT NULL` 需要全表校验
//!
//! 重写类风险一律 `Block` 且 `blocks_write=true`，S1 只出脚本，不生成可执行计划。

use super::pg_evidence::pg_type_changed;
use super::risk::Builder;
use super::types::{
    FieldGovernanceAnalysis, FieldGovernanceInput, RiskLevel,
};

pub(crate) fn evaluate(
    builder: &mut Builder,
    analysis: &FieldGovernanceAnalysis,
    input: &FieldGovernanceInput,
) {
    if let Some(kind) = analysis.environment.table_kind.as_deref() {
        if !matches!(kind, "r" | "p") {
            builder.push(
                "E02_PG_UNSUPPORTED_RELKIND",
                RiskLevel::Block,
                "PG 对象不是普通表/分区表",
                format!(
                    "relkind={kind}；视图/物化视图/外部表不支持字段治理的建表与回填路径。"
                ),
                vec!["E02".into()],
                true,
                true,
            );
        }
    }

    if analysis.environment.nondeterministic_collation {
        let analyzed: Vec<String> = analysis
            .columns
            .iter()
            .filter(|column| column.collation.is_some())
            .map(|column| column.column.clone())
            .collect();
        let detail = if analyzed.is_empty() {
            "表内存在 collisdeterministic=false 的列 collation（本次分析列未直接命中）；\
             非确定性 collation 下等值/唯一语义不稳定，S1 不支持。"
                .to_string()
        } else {
            format!(
                "已分析列 {analyzed:?} 命中非确定性 collation；非确定性 collation 下等值/唯一语义不稳定，S1 不支持。"
            )
        };
        builder.push(
            "E13_PG_NONDETERMINISTIC_COLLATION",
            RiskLevel::Block,
            "存在非确定性排序规则",
            detail,
            vec!["E13".into()],
            true,
            true,
        );
    }

    let target_collation = input
        .target
        .as_ref()
        .and_then(|target| target.collation.as_deref());
    if let Some(target) = target_collation {
        let mismatched: Vec<String> = analysis
            .columns
            .iter()
            .filter(|column| column.is_text_like())
            .filter(|column| {
                column
                    .collation
                    .as_deref()
                    .map(|source| !source.eq_ignore_ascii_case(target))
                    .unwrap_or(false)
            })
            .map(|column| {
                format!(
                    "{} ({} -> {})",
                    column.column,
                    column.collation.as_deref().unwrap_or("-"),
                    target
                )
            })
            .collect();
        if !mismatched.is_empty() {
            builder.push(
                "E13_PG_COLLATION_REWRITE",
                RiskLevel::Block,
                "目标排序规则变更会触发表重写",
                format!(
                    "columns={mismatched:?}；PG 的 ALTER COLUMN TYPE ... COLLATE 会重写表，S1 只生成脚本。"
                ),
                vec!["E13".into()],
                true,
                true,
            );
        }
    }

    for column in &analysis.columns {
        let Some(target_type) = target_type_for_column(input, &column.column) else {
            continue;
        };
        if !pg_type_changed(&column.column_type, &target_type) {
            continue;
        }
        builder.push(
            "E01_PG_TYPE_REWRITE",
            RiskLevel::Block,
            "PG 类型变更会触发表重写",
            format!(
                "column={} source_type={} target_type={}；S1 只生成 ALTER COLUMN TYPE ... USING 脚本，不生成可执行计划。",
                column.column, column.column_type, target_type
            ),
            vec!["E01".into()],
            true,
            true,
        );
        match column.pg_cast_invalid_rows {
            Some(0) => {}
            Some(invalid) => builder.push(
                "E01_PG_USING_INVALID",
                RiskLevel::Block,
                "USING 转换预检发现不可转换行",
                format!(
                    "column={} target_type={} invalid_rows={}；pg_input_is_valid 预检失败，直接转换会报错或丢数据。",
                    column.column, target_type, invalid
                ),
                vec!["E01".into()],
                true,
                true,
            ),
            None => builder.push(
                "E01_PG_USING_UNVERIFIED",
                RiskLevel::Block,
                "USING 转换未预检",
                format!(
                    "column={} target_type={}；PG<16 无 pg_input_is_valid 或预检失败，转换结果不可判定。",
                    column.column, target_type
                ),
                vec!["E01".into()],
                true,
                true,
            ),
        }
    }

    for column in &analysis.columns {
        if !target_requires_not_null(input, &column.column) {
            continue;
        }
        if column.nullable || column.null_rows > 0 {
            builder.push(
                "E08_PG_SET_NOT_NULL_SCAN",
                RiskLevel::High,
                "SET NOT NULL 需要全表校验",
                format!(
                    "column={} nullable={} null_rows={}；PG 的 SET NOT NULL 会全表校验，脚本采用 CHECK NOT VALID → VALIDATE → SET NOT NULL。",
                    column.column, column.nullable, column.null_rows
                ),
                vec!["E08".into()],
                false,
                true,
            );
        }
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

fn target_requires_not_null(input: &FieldGovernanceInput, column: &str) -> bool {
    let Some(target) = input.target.as_ref() else {
        return false;
    };
    target.mappings.iter().any(|mapping| {
        let applies = mapping
            .source_key
            .as_deref()
            .map(|key| key.eq_ignore_ascii_case(column))
            .unwrap_or(true);
        applies && mapping.required == Some(true)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::field_migration::types::{
        sample_column, ColumnMapping, DependencyEvidence, EnvironmentEvidence,
        FieldGovernanceAnalysis, TargetSpec,
    };

    fn analysis() -> FieldGovernanceAnalysis {
        FieldGovernanceAnalysis {
            db_name: "app".into(),
            table: "orders".into(),
            column: Some("payload".into()),
            analyzed_columns: vec!["payload".into()],
            profile_mode: "full".into(),
            columns: vec![sample_column("payload", "jsonb")],
            dependencies: DependencyEvidence {
                single_column_pk: Some("id".into()),
                ..Default::default()
            },
            environment: EnvironmentEvidence {
                engine_family: "postgresql".into(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn input_with_type(target_type: &str) -> FieldGovernanceInput {
        FieldGovernanceInput {
            table: "orders".into(),
            column: Some("payload".into()),
            target: Some(TargetSpec {
                mappings: vec![ColumnMapping {
                    source_key: None,
                    target_column: "payload".into(),
                    target_type: Some(target_type.into()),
                    required: None,
                }],
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn codes(analysis: &FieldGovernanceAnalysis, input: &FieldGovernanceInput) -> Vec<String> {
        let mut builder = Builder::default();
        evaluate(&mut builder, analysis, input);
        builder.risks.into_iter().map(|risk| risk.code).collect()
    }

    #[test]
    fn blocks_non_table_relkind() {
        let mut analysis = analysis();
        analysis.environment.table_kind = Some("v".into());
        assert!(codes(&analysis, &input_with_type("text"))
            .contains(&"E02_PG_UNSUPPORTED_RELKIND".to_string()));
    }

    #[test]
    fn blocks_nondeterministic_collation() {
        let mut analysis = analysis();
        analysis.environment.nondeterministic_collation = true;
        assert!(codes(&analysis, &input_with_type("text"))
            .contains(&"E13_PG_NONDETERMINISTIC_COLLATION".to_string()));
    }

    #[test]
    fn type_change_blocks_and_uses_probe_result() {
        let mut analysis = analysis();
        analysis.columns[0].column_type = "jsonb".into();
        analysis.columns[0].pg_cast_invalid_rows = Some(0);
        let input = input_with_type("text");
        let found = codes(&analysis, &input);
        assert!(found.contains(&"E01_PG_TYPE_REWRITE".to_string()));
        assert!(!found.contains(&"E01_PG_USING_INVALID".to_string()));
        assert!(!found.contains(&"E01_PG_USING_UNVERIFIED".to_string()));

        analysis.columns[0].pg_cast_invalid_rows = Some(3);
        let found = codes(&analysis, &input);
        assert!(found.contains(&"E01_PG_USING_INVALID".to_string()));

        analysis.columns[0].pg_cast_invalid_rows = None;
        let found = codes(&analysis, &input);
        assert!(found.contains(&"E01_PG_USING_UNVERIFIED".to_string()));
    }

    #[test]
    fn set_not_null_requires_full_scan_confirmation() {
        let mut analysis = analysis();
        analysis.columns[0].nullable = true;
        let mut input = input_with_type("jsonb");
        input.target.as_mut().unwrap().mappings[0].required = Some(true);
        let codes = codes(&analysis, &input);
        assert!(codes.contains(&"E08_PG_SET_NOT_NULL_SCAN".to_string()));
    }
}
