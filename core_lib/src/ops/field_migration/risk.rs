//! 三档风险判定（确定性规则）。
//!
//! 规则：BLOCK > HIGH > INFO；不可判定（证据缺口/抽样/扫描不完整）= 写步骤 BLOCK。
//! 用户语义描述只作为「未验证约束」追加风险，绝不删除或下调已有风险。

use std::collections::HashSet;

use crate::ops::policy::OpsPolicy;

use super::dependency::contains_identifier;
use super::types::{
    ColumnProfile, Declaration, FieldGovernanceAnalysis, FieldGovernanceInput, FieldRisk,
    RiskLevel, RiskSummary, TargetSpec,
};

pub fn evaluate(
    analysis: &mut FieldGovernanceAnalysis,
    input: &FieldGovernanceInput,
    policy: &OpsPolicy,
) {
    let mut builder = Builder::default();

    // ── 表级结构/体量/环境 ────────────────────────────────
    let engine = analysis
        .environment
        .engine
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !engine.is_empty() && !engine.contains("innodb") {
        builder.push(
            "E02_NOT_INNODB",
            RiskLevel::Block,
            "源表不是 InnoDB",
            format!("engine={engine}；无事务/主键语义，不能安全分批回滚。"),
            vec!["E02".into()],
            true,
            true,
        );
    }
    if analysis.environment.partitioned {
        builder.push(
            "E02_PARTITIONED",
            RiskLevel::Block,
            "源表是分区表",
            "分区表的 DDL、分块读取与下线行为不可控，S1 不支持。".into(),
            vec!["E02".into()],
            true,
            true,
        );
    }

    if analysis.row_count > policy.field_migration_max_rows
        || analysis.data_length.unwrap_or(0) > policy.field_migration_max_bytes
    {
        builder.push(
            "E07_TOO_LARGE_AUTO",
            RiskLevel::Block,
            "体量超过自动迁移上限",
            format!(
                "row_count={} data_length={:?}；超过阈值（{} 行 / {} 字节），只允许脚本或专用工具。",
                analysis.row_count, analysis.data_length, policy.field_migration_max_rows,
                policy.field_migration_max_bytes
            ),
            vec!["E07".into()],
            true,
            true,
        );
    } else if analysis.row_count > 200_000
        || analysis.data_length.unwrap_or(0) > 512 * 1024 * 1024
    {
        builder.push(
            "E07_LARGE_HIGH",
            RiskLevel::High,
            "体量较大",
            format!(
                "row_count={} data_length={:?}；回填耗时长，需要维护窗口与限速。",
                analysis.row_count, analysis.data_length
            ),
            vec!["E07".into()],
            false,
            true,
        );
    }

    if analysis.environment.read_only {
        builder.push(
            "E14_READ_ONLY",
            RiskLevel::Block,
            "连接或实例为只读",
            "只读实例无法执行回填；S1 可以继续输出报告，但写步骤必须阻断。".into(),
            vec!["E14".into()],
            true,
            true,
        );
    }
    if !analysis.environment.version_supports_json
        && analysis
            .columns
            .iter()
            .any(|column| column.is_text_like())
    {
        builder.push(
            "E14_VERSION_NO_JSON",
            RiskLevel::Block,
            "MySQL 版本不支持原生 JSON",
            format!(
                "version={}；低于 5.7.8，无法安全使用 JSON_VALID/JSON_KEYS。",
                analysis.environment.version.as_deref().unwrap_or("-")
            ),
            vec!["E14".into()],
            true,
            true,
        );
    }
    if analysis.environment.long_transactions > 0 {
        builder.push(
            "E14_LONG_TRANSACTION",
            RiskLevel::High,
            "存在长事务",
            format!(
                "{} 个事务超过 30s；DDL 需要 MDL，回填需避开。",
                analysis.environment.long_transactions
            ),
            vec!["E14".into()],
            false,
            true,
        );
    }
    if analysis.dependencies.single_column_pk.is_none() {
        builder.push(
            "E14_NO_SINGLE_PK",
            RiskLevel::Block,
            "无单列主键",
            format!(
                "主键列={:?}；无法安全分片与幂等回填，只能人工导出/导入。",
                analysis.dependencies.primary_key_columns
            ),
            vec!["E14".into()],
            true,
            true,
        );
    }
    if analysis.target_exists {
        builder.push(
            "E14_TARGET_EXISTS",
            RiskLevel::Block,
            "目标表已存在",
            format!(
                "target={}；S1 无法证明它是本计划的产物，禁止进入写步骤。",
                analysis.target_table
            ),
            vec!["E14".into()],
            true,
            true,
        );
    }
    if !analysis.dependencies.scan_complete {
        builder.push(
            "E06_DEPENDENCY_UNVERIFIED",
            RiskLevel::Block,
            "依赖扫描不完整",
            "视图/触发器/例程/外键至少一项读取失败；依赖未知，写步骤按阻断。".into(),
            vec!["E06".into()],
            true,
            true,
        );
    }
    if analysis.profile_mode != "full" {
        builder.push(
            "PROFILE_NOT_FULL",
            RiskLevel::Block,
            "数据剖面不是 full 模式",
            format!(
                "profile_mode={}；抽样只能回答「有没有发现问题」，不能证明全部数据安全，不能授权任何写步骤。",
                analysis.profile_mode
            ),
            vec!["E07".into()],
            true,
            true,
        );
    }
    if !analysis.gaps.is_empty() {
        builder.push(
            "EVIDENCE_GAP",
            RiskLevel::Block,
            "存在未采集/未验证证据",
            format!("{} 项证据缺口，按「不可判定 = 阻断」处理。", analysis.gaps.len()),
            analysis
                .gaps
                .iter()
                .map(|gap| gap.evidence_id.clone())
                .collect(),
            true,
            true,
        );
    }

    // ── 列级规则 ────────────────────────────────────────────
    let columns: Vec<ColumnProfile> = analysis.columns.clone();
    for column in &columns {
        evaluate_column(&mut builder, analysis, input, column, policy);
    }

    // ── 空串策略（显式，不允许静默转换）─────────────────────
    evaluate_empty_string_policy(&mut builder, analysis, input);

    // ── 语义声明只增约束 ────────────────────────────────────
    let declarations = analysis.declarations.clone();
    for declaration in &declarations {
        evaluate_declaration(&mut builder, analysis, input, declaration);
    }

    builder
        .risks
        .sort_by(|a, b| {
            b.level
                .cmp(&a.level)
                .then_with(|| a.code.cmp(&b.code))
                .then_with(|| a.id.cmp(&b.id))
        });
    let block = builder
        .risks
        .iter()
        .filter(|risk| risk.level == RiskLevel::Block)
        .count();
    let high = builder
        .risks
        .iter()
        .filter(|risk| risk.level == RiskLevel::High)
        .count();
    let info = builder
        .risks
        .iter()
        .filter(|risk| risk.level == RiskLevel::Info)
        .count();
    let risk_floor = if block > 0 {
        "block"
    } else if high > 0 {
        "high"
    } else {
        "info"
    };
    analysis.risks = builder.risks;
    analysis.risk_summary = RiskSummary {
        block,
        high,
        info,
        undetermined_write_block: !analysis.gaps.is_empty()
            || analysis.profile_mode != "full"
            || !analysis.dependencies.scan_complete,
        risk_floor: risk_floor.into(),
    };
}

fn evaluate_empty_string_policy(
    builder: &mut Builder,
    analysis: &FieldGovernanceAnalysis,
    input: &FieldGovernanceInput,
) {
    let policy = input.empty_string_policy();
    let empty_rows: u64 = analysis.columns.iter().map(|column| column.empty_rows).sum();
    let (level, title, detail, blocks_write, requires_confirmation) = match policy {
        super::types::EmptyStringPolicy::Preserve => (
            RiskLevel::Info,
            "空串策略：保留空串",
            format!(
                "策略=preserve；观测空串 {} 行。回填与校验均按原文保留，不使用 NULLIF 抹平。",
                empty_rows
            ),
            false,
            false,
        ),
        super::types::EmptyStringPolicy::ToNull if empty_rows > 0 => (
            RiskLevel::High,
            "空串策略：转为 NULL",
            format!(
                "策略=to_null；{} 行空串会被显式写成 NULL（违反文档 4.2(1) 默认语义），需要用户确认。",
                empty_rows
            ),
            false,
            true,
        ),
        super::types::EmptyStringPolicy::ToNull => (
            RiskLevel::Info,
            "空串策略：转为 NULL（当前无命中）",
            "策略=to_null；当前数据未观测到空串，但脚本仍会按该策略转换。".into(),
            false,
            false,
        ),
        super::types::EmptyStringPolicy::Block if empty_rows > 0 => (
            RiskLevel::Block,
            "空串策略：命中即阻断",
            format!(
                "策略=block；观测空串 {} 行，禁止进入任何写步骤，必须先人工处理空串语义。",
                empty_rows
            ),
            true,
            true,
        ),
        super::types::EmptyStringPolicy::Block => (
            RiskLevel::Info,
            "空串策略：命中即阻断（当前无命中）",
            "策略=block；当前未观测到空串，脚本保持原文但命中即阻断。".into(),
            false,
            false,
        ),
    };
    builder.push(
        "E08_EMPTY_STRING_POLICY",
        level,
        title,
        detail,
        vec!["E08".into()],
        blocks_write,
        requires_confirmation,
    );
}

fn evaluate_column(
    builder: &mut Builder,
    analysis: &FieldGovernanceAnalysis,
    input: &FieldGovernanceInput,
    column: &ColumnProfile,
    policy: &OpsPolicy,
) {
    let suffix = format!("@{}", column.column);

    // E01：类型无法安全解析。
    if matches!(
        column.data_type.as_str(),
        "blob" | "tinyblob" | "mediumblob" | "longblob" | "binary" | "varbinary" | "geometry"
            | "point" | "linestring" | "polygon" | "multipoint" | "multilinestring"
            | "multipolygon" | "geometrycollection"
    ) {
        builder.push(
            &format!("E01_UNPARSEABLE_TYPE{suffix}"),
            RiskLevel::Block,
            "列类型无法安全解析",
            format!(
                "column={} data_type={}；不能做 JSON/字符串语义推断。",
                column.column, column.data_type
            ),
            vec!["E01".into()],
            true,
            true,
        );
    }

    // E01：目标列长度容量。
    if let Some(target_length) = target_length_for(column, input) {
        if column.max_char_length > target_length {
            builder.push(
                &format!("E01_TARGET_LENGTH_TOO_SMALL{suffix}"),
                RiskLevel::Block,
                "目标列长度不足",
                format!(
                    "column={} 观测最大长度 {} > 目标长度 {}，必然截断。",
                    column.column, column.max_char_length, target_length
                ),
                vec!["E01".into(), "E12".into()],
                true,
                true,
            );
        } else if column.max_char_length >= target_length.saturating_mul(9) / 10 {
            builder.push(
                &format!("E01_TARGET_LENGTH_TIGHT{suffix}"),
                RiskLevel::High,
                "目标列长度余量不足",
                format!(
                    "column={} 观测最大长度 {} 已接近目标长度 {}（≥90%）。",
                    column.column, column.max_char_length, target_length
                ),
                vec!["E01".into(), "E12".into()],
                false,
                true,
            );
        }
    }

    // E03：索引/唯一约束。
    let in_primary = analysis
        .dependencies
        .primary_key_columns
        .iter()
        .any(|name| name.eq_ignore_ascii_case(&column.column));
    let unique_indexes: Vec<String> = analysis
        .dependencies
        .indexes
        .iter()
        .filter(|index| !index.primary && index.unique)
        .filter(|index| index.columns.iter().any(|c| c.eq_ignore_ascii_case(&column.column)))
        .map(|index| index.name.clone())
        .collect();
    if in_primary || !unique_indexes.is_empty() {
        builder.push(
            &format!("E03_PRIMARY_OR_UNIQUE{suffix}"),
            RiskLevel::Block,
            "列被主键/唯一约束引用",
            format!(
                "column={} primary={} unique_indexes={:?}；拆出会改变唯一性语义。",
                column.column, in_primary, unique_indexes
            ),
            vec!["E03".into()],
            true,
            true,
        );
    }
    let secondary_indexes: Vec<String> = analysis
        .dependencies
        .indexes
        .iter()
        .filter(|index| !index.unique)
        .filter(|index| index.columns.iter().any(|c| c.eq_ignore_ascii_case(&column.column)))
        .map(|index| index.name.clone())
        .collect();
    if !secondary_indexes.is_empty() {
        builder.push(
            &format!("E03_SECONDARY_INDEX{suffix}"),
            RiskLevel::High,
            "列在普通二级索引中",
            format!(
                "column={} indexes={:?}；下线后查询可能退化，需确认索引处理方案。",
                column.column, secondary_indexes
            ),
            vec!["E03".into()],
            false,
            true,
        );
    }

    // E04：外键。
    let fk: Vec<String> = analysis
        .dependencies
        .foreign_keys
        .iter()
        .filter(|fk| fk.column.eq_ignore_ascii_case(&column.column))
        .map(|fk| {
            format!(
                "{}:{}->{}.{}",
                fk.direction,
                fk.constraint_name,
                fk.referenced_table.as_deref().unwrap_or("-"),
                fk.referenced_column.as_deref().unwrap_or("-")
            )
        })
        .collect();
    if !fk.is_empty() {
        builder.push(
            &format!("E04_FOREIGN_KEY{suffix}"),
            RiskLevel::Block,
            "列参与外键",
            format!("column={} fk={:?}；约束/孤儿/级联行为不可自动保证。", column.column, fk),
            vec!["E04".into()],
            true,
            true,
        );
    }

    // E05：生成列。
    let generated: Vec<String> = analysis
        .dependencies
        .generated_columns
        .iter()
        .filter(|item| contains_identifier(&item.expression, &column.column))
        .map(|item| item.name.clone())
        .collect();
    if !generated.is_empty() {
        builder.push(
            &format!("E05_GENERATED_COLUMN{suffix}"),
            RiskLevel::Block,
            "存在生成列依赖该列",
            format!("column={} generated_columns={:?}；表达式必须人工改写。", column.column, generated),
            vec!["E05".into()],
            true,
            true,
        );
    }

    // E06：视图/触发器/例程依赖；源表触发器。
    let objects: Vec<String> = analysis
        .dependencies
        .objects
        .iter()
        .filter(|object| {
            object
                .matched_columns
                .iter()
                .any(|c| c.eq_ignore_ascii_case(&column.column))
        })
        .map(|object| format!("{}:{}", object.kind, object.name))
        .collect();
    if !objects.is_empty() {
        builder.push(
            &format!("E06_OBJECT_DEPENDENCY{suffix}"),
            RiskLevel::Block,
            "视图/触发器/例程文本引用该列",
            format!("column={} objects={:?}；需人工修改依赖对象。", column.column, objects),
            vec!["E06".into()],
            true,
            true,
        );
    }

    // E08：NULL / 空串。
    if column.null_rows > 0 {
        builder.push(
            &format!("E08_NULL_PRESENT{suffix}"),
            RiskLevel::High,
            "存在 NULL 行",
            format!("column={} null_rows={}；需确认 NULL 语义（SQL NULL vs JSON null）。", column.column, column.null_rows),
            vec!["E08".into()],
            false,
            true,
        );
    }
    if column.null_rows > 0 && target_requires_not_null(input, &column.column) {
        builder.push(
            &format!("E08_NULL_TO_NOT_NULL{suffix}"),
            RiskLevel::Block,
            "源列有 NULL 但目标列 NOT NULL",
            format!("column={} null_rows={}；未声明填充规则，无法决定替代值。", column.column, column.null_rows),
            vec!["E08".into()],
            true,
            true,
        );
    }
    if column.empty_rows > 0 {
        builder.push(
            &format!("E08_EMPTY_STRING{suffix}"),
            RiskLevel::High,
            "存在空串行",
            format!("column={} empty_rows={}；空串与 JSON null/{{}} 语义不同，必须选策略。", column.column, column.empty_rows),
            vec!["E08".into()],
            false,
            true,
        );
        let ratio = column.empty_rows as f64 / column.rows_profiled.max(1) as f64;
        if ratio > 0.2 {
            builder.push(
                &format!("E08_EMPTY_STRING_OVER_20PCT{suffix}"),
                RiskLevel::Block,
                "空串占比过高",
                format!("column={} empty_ratio={:.1}% > 20%；数据语义过度模糊。", column.column, ratio * 100.0),
                vec!["E08".into()],
                true,
                true,
            );
        }
    }

    // E09：非法 JSON。
    if column.json_invalid_rows > 0 {
        builder.push(
            &format!("E09_INVALID_JSON{suffix}"),
            RiskLevel::Block,
            "存在非法 JSON",
            format!(
                "column={} invalid_rows={}；默认不允许猜测修复，只能人工处理或隔离。",
                column.column, column.json_invalid_rows
            ),
            vec!["E09".into()],
            true,
            true,
        );
    }

    // E10：顶层数组 / key 数量。
    if column.json_array_rows > 0 {
        builder.push(
            &format!("E10_ARRAY_NOT_SUPPORTED{suffix}"),
            RiskLevel::Block,
            "JSON 顶层包含数组",
            format!(
                "column={} array_rows={}；1:N 拆表需要显式人工映射，S1 不支持自动建表。",
                column.column, column.json_array_rows
            ),
            vec!["E10".into()],
            true,
            true,
        );
    }
    if column.json_keys.len() > policy.field_max_json_keys {
        builder.push(
            &format!("E10_TOO_MANY_KEYS{suffix}"),
            RiskLevel::Block,
            "JSON key 数量过多",
            format!(
                "column={} keys={} > {}；结构推断不稳定，需要显式映射。",
                column.column,
                column.json_keys.len(),
                policy.field_max_json_keys
            ),
            vec!["E10".into()],
            true,
            true,
        );
    }

    // E11：key 类型漂移。
    for key in &column.json_key_types {
        let non_null: HashSet<&str> = key
            .types
            .keys()
            .map(String::as_str)
            .filter(|t| *t != "NULL")
            .collect();
        if non_null.len() > 1 {
            let typed_target = has_typed_target(input, &key.key);
            let residual = input
                .target
                .as_ref()
                .map(TargetSpec::residual_json)
                .unwrap_or(false);
            let level = if typed_target || !residual {
                RiskLevel::Block
            } else {
                RiskLevel::High
            };
            builder.push(
                &format!("E11_TYPE_DRIFT{suffix}:{}", key.key),
                level,
                "JSON key 类型漂移",
                format!(
                    "column={} key={} types={:?}；不能静默强转。",
                    column.column, key.key, key.types
                ),
                vec!["E11".into()],
                level == RiskLevel::Block,
                true,
            );
        }
    }

    // E12/E13：脏数据与排序规则。
    if column.control_char_rows > 0 {
        builder.push(
            &format!("E12_CONTROL_CHARS{suffix}"),
            RiskLevel::High,
            "存在控制字符",
            format!("column={} control_rows={}；下游文本/索引/日志可能异常。", column.column, column.control_char_rows),
            vec!["E12".into()],
            false,
            true,
        );
    }
    if column.is_text_like()
        && !column.is_json_type()
        && column
            .charset
            .as_deref()
            .map(|c| c.eq_ignore_ascii_case("binary"))
            .unwrap_or(true)
    {
        builder.push(
            &format!("E13_BINARY_CHARSET{suffix}"),
            RiskLevel::Block,
            "文本列字符集缺失或为 binary",
            format!("column={} charset={:?}；文本语义不可判定。", column.column, column.charset),
            vec!["E13".into()],
            true,
            true,
        );
    }
    if column.distinct_binary_rows > column.distinct_rows {
        let target_unique = input
            .declarations
            .iter()
            .any(|declaration| declaration.uniqueness_expected.unwrap_or(false));
        let level = if target_unique {
            RiskLevel::Block
        } else {
            RiskLevel::High
        };
        builder.push(
            &format!("E13_CASE_FOLD_COLLISION{suffix}"),
            level,
            "排序规则折叠导致大小写冲突",
            format!(
                "column={} distinct={} binary_distinct={}；看似不同、实际相等。",
                column.column, column.distinct_rows, column.distinct_binary_rows
            ),
            vec!["E13".into()],
            level == RiskLevel::Block,
            true,
        );
    }
    if column.rows_profiled > 0
        && column.untrimmed_rows as f64 / column.rows_profiled as f64 > 0.05
    {
        builder.push(
            &format!("E13_TRIM_DIRTY{suffix}"),
            RiskLevel::High,
            "前后空白脏数据超过 5%",
            format!(
                "column={} untrimmed_rows={} / {}。",
                column.column, column.untrimmed_rows, column.rows_profiled
            ),
            vec!["E13".into()],
            false,
            true,
        );
    }
    if let (Some(source), Some(target)) = (
        column.collation.as_deref(),
        input.target.as_ref().and_then(|t| t.collation.as_deref()),
    ) {
        if !source.eq_ignore_ascii_case(target) {
            builder.push(
                &format!("E13_COLLATION_MISMATCH{suffix}"),
                RiskLevel::High,
                "目标排序规则与源列不同",
                format!("column={} source_collation={} target_collation={}。", column.column, source, target),
                vec!["E13".into()],
                false,
                true,
            );
        }
    }
}

fn evaluate_declaration(
    builder: &mut Builder,
    analysis: &FieldGovernanceAnalysis,
    input: &FieldGovernanceInput,
    declaration: &Declaration,
) {
    let matched_column = declaration
        .column
        .as_deref()
        .map(|c| {
            analysis
                .columns
                .iter()
                .find(|column| column.column.eq_ignore_ascii_case(c))
        })
        .unwrap_or_else(|| analysis.selected_profile());
    let Some(column) = matched_column else {
        return;
    };

    if let (Some(subject), Some(selected)) = (declaration.column.as_deref(), analysis.column.as_deref())
    {
        if !subject.eq_ignore_ascii_case(selected) {
            builder.push(
                "SEM_DECL_SUBJECT_MISMATCH",
                RiskLevel::High,
                "声明对象与证据不一致",
                format!("declaration={} subject={subject} analyzed_column={selected}", declaration.id),
                vec!["SEM-DECL".into()],
                false,
                true,
            );
        }
    }

    builder.push(
        "SEM_DECL_UNVERIFIED",
        RiskLevel::Info,
        "语义声明未验证",
        format!(
            "declaration={} source=user verified=false；声明只作为约束参与判断，不能抵消数据/结构事实。",
            declaration.id
        ),
        vec!["SEM-DECL".into()],
        false,
        false,
    );

    if !declaration.consumers.is_empty() {
        builder.push(
            "SEM_DECL_CONSUMERS_UNVERIFIED",
            RiskLevel::High,
            "声明中的消费者无法在库内验证",
            format!(
                "declaration={} consumers={:?}；必须人工确认读路径已切换。",
                declaration.id, declaration.consumers
            ),
            vec!["SEM-DECL".into()],
            false,
            true,
        );
    }

    if !declaration.value_domain.is_empty() {
        builder.push(
            "SEM_DECL_DOMAIN_UNVERIFIED",
            RiskLevel::High,
            "声明值域无法在库内验证",
            format!(
                "declaration={} value_domain={:?}；需人工确认或补数据校验规则。",
                declaration.id, declaration.value_domain
            ),
            vec!["SEM-DECL".into()],
            false,
            true,
        );
    }

    if declaration.target_nullable == Some(false) && column.null_rows > 0 {
        builder.push(
            "SEM_DECL_NULL_CONFLICT",
            RiskLevel::Block,
            "声明 NOT NULL 与数据冲突",
            format!(
                "declaration={} column={} null_rows={}；声明不能覆盖数据事实。",
                declaration.id, column.column, column.null_rows
            ),
            vec!["SEM-DECL".into(), "E08".into()],
            true,
            true,
        );
    }
    if declaration.target_nullable == Some(true) && column.null_rows == 0 {
        builder.push(
            "SEM_DECL_NULLABLE_UNUSED",
            RiskLevel::Info,
            "声明可空但数据中没有 NULL",
            format!("declaration={} column={}。", declaration.id, column.column),
            vec!["SEM-DECL".into()],
            false,
            false,
        );
    }

    if !declaration.required_keys.is_empty() {
        let missing: Vec<String> = declaration
            .required_keys
            .iter()
            .filter(|key| {
                column
                    .json_key_presence(key)
                    .map(|presence| presence.rows < column.json_object_rows.max(1))
                    .unwrap_or(true)
            })
            .cloned()
            .collect();
        if !missing.is_empty() {
            builder.push(
                "SEM_DECL_REQUIRED_KEY_MISSING",
                RiskLevel::Block,
                "声明必需 key 缺失",
                format!(
                    "declaration={} required_keys={:?} missing_or_partial={:?}；声明与数据冲突。",
                    declaration.id, declaration.required_keys, missing
                ),
                vec!["SEM-DECL".into(), "E10".into()],
                true,
                true,
            );
        }
    }

    if !declaration.expected_keys.is_empty() {
        let declared: HashSet<String> = declaration
            .expected_keys
            .iter()
            .map(|key| key.to_ascii_lowercase())
            .collect();
        let unexpected: Vec<String> = column
            .json_keys
            .iter()
            .filter(|key| !declared.contains(&key.to_ascii_lowercase()))
            .cloned()
            .collect();
        if !unexpected.is_empty() {
            let residual_allowed = input
                .target
                .as_ref()
                .map(TargetSpec::residual_json)
                .unwrap_or(false);
            let level = if residual_allowed {
                RiskLevel::High
            } else {
                RiskLevel::Block
            };
            builder.push(
                "SEM_DECL_UNEXPECTED_KEY",
                level,
                "出现契约之外的新 key",
                format!(
                    "declaration={} expected={:?} unexpected={:?}；未声明 residual 存储时数据会丢失。",
                    declaration.id, declaration.expected_keys, unexpected
                ),
                vec!["SEM-DECL".into(), "E10".into()],
                level == RiskLevel::Block,
                true,
            );
        }
    }

    if declaration.uniqueness_expected {
        let evidence = analysis
            .declaration_evidence
            .iter()
            .find(|item| item.declaration_id == declaration.id && item.kind == "uniqueness");
        match evidence {
            Some(item) if item.rows > 0 => builder.push(
                "SEM_DECL_UNIQUENESS_CONFLICT",
                RiskLevel::Block,
                "声明唯一但与数据冲突",
                format!(
                    "declaration={} unique_keys={:?} duplicate_keys={}；{}",
                    declaration.id, declaration.unique_keys, item.rows, item.detail
                ),
                vec!["SEM-DECL".into()],
                true,
                true,
            ),
            Some(_) => builder.push(
                "SEM_DECL_UNIQUENESS_OK",
                RiskLevel::Info,
                "声明唯一性验证通过",
                format!("declaration={} unique_keys={:?}。", declaration.id, declaration.unique_keys),
                vec!["SEM-DECL".into()],
                false,
                false,
            ),
            None => builder.push(
                "SEM_DECL_UNIQUENESS_UNVERIFIED",
                RiskLevel::Block,
                "声明唯一性未验证",
                format!("declaration={} unique_keys={:?}；缺少验证证据。", declaration.id, declaration.unique_keys),
                vec!["SEM-DECL".into()],
                true,
                true,
            ),
        }
    }

    if let Some(means) = declaration.empty_string_means.as_deref() {
        if means == "invalid" && column.empty_rows > 0 {
            builder.push(
                "SEM_DECL_EMPTY_INVALID_CONFLICT",
                RiskLevel::High,
                "声明空串非法但数据中存在空串",
                format!("declaration={} empty_rows={}。", declaration.id, column.empty_rows),
                vec!["SEM-DECL".into(), "E08".into()],
                false,
                true,
            );
        } else {
            builder.push(
                "SEM_DECL_EMPTY_POLICY",
                RiskLevel::Info,
                "已声明空串语义",
                format!("declaration={} empty_string_means={means}。", declaration.id),
                vec!["SEM-DECL".into()],
                false,
                false,
            );
        }
    }

    if declaration.cutover_mode.as_deref() == Some("dual_write") {
        builder.push(
            "SEM_DECL_DUAL_WRITE",
            RiskLevel::High,
            "声明采用双写",
            "双写一致性无法在库内验证，必须人工实现并观察一段时间。".into(),
            vec!["SEM-DECL".into()],
            false,
            true,
        );
    }
    if declaration.retention_days.map(|d| d < 30).unwrap_or(false) {
        builder.push(
            "SEM_DECL_RETENTION_TOO_SHORT",
            RiskLevel::High,
            "兼容期/保留期过短",
            format!(
                "declaration={} retention_days={}；建议至少保留 30 天。",
                declaration.id,
                declaration.retention_days.unwrap_or(0)
            ),
            vec!["SEM-DECL".into()],
            false,
            true,
        );
    }
}

fn target_length_for(column: &ColumnProfile, input: &FieldGovernanceInput) -> Option<u64> {
    let mappings = input.target.as_ref()?.mappings.as_slice();
    for mapping in mappings {
        let relevant = mapping
            .source_key
            .as_deref()
            .map(|key| key.eq_ignore_ascii_case(&column.column))
            .unwrap_or_else(|| mapping.target_column.eq_ignore_ascii_case(&column.column));
        if !relevant {
            continue;
        }
        if let Some(sql_type) = mapping.target_type.as_deref() {
            if let Some(length) = parse_type_length(sql_type) {
                return Some(length);
            }
        }
    }
    None
}

fn target_requires_not_null(input: &FieldGovernanceInput, _column: &str) -> bool {
    if input
        .declarations
        .iter()
        .any(|declaration| declaration.target_nullable == Some(false))
    {
        return true;
    }
    if input
        .declarations
        .iter()
        .any(|declaration| !declaration.required_keys.is_empty())
    {
        return true;
    }
    // JSON key 映射的任意必填列，在源列为 NULL 时都会落空；
    // 因此只要存在必填目标列，就按目标 NOT NULL 处理。
    input
        .target
        .as_ref()
        .map(|target| {
            target.mappings.iter().any(|mapping| {
                mapping.required == Some(true)
                    || mapping
                        .target_type
                        .as_deref()
                        .map(|t| t.to_ascii_uppercase().contains("NOT NULL"))
                        .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

fn has_typed_target(input: &FieldGovernanceInput, key: &str) -> bool {
    input
        .target
        .as_ref()
        .map(|target| {
            target.mappings.iter().any(|mapping| {
                mapping
                    .source_key
                    .as_deref()
                    .map(|source_key| source_key.eq_ignore_ascii_case(key))
                    .unwrap_or(false)
                    && mapping.target_type.is_some()
            })
        })
        .unwrap_or(false)
}

fn parse_type_length(sql_type: &str) -> Option<u64> {
    let upper = sql_type.to_ascii_uppercase();
    if !(upper.starts_with("VARCHAR") || upper.starts_with("CHAR")) {
        return None;
    }
    let open = upper.find('(')?;
    let close = upper[open + 1..].find(')')?;
    upper[open + 1..open + 1 + close].trim().parse().ok()
}

#[derive(Default)]
struct Builder {
    risks: Vec<FieldRisk>,
    counter: usize,
}

impl Builder {
    #[allow(clippy::too_many_arguments)]
    fn push(
        &mut self,
        code: &str,
        level: RiskLevel,
        title: &str,
        detail: String,
        evidence_refs: Vec<String>,
        blocks_write: bool,
        requires_user_confirmation: bool,
    ) {
        self.counter += 1;
        self.risks.push(FieldRisk {
            id: format!("risk-{}-{code}", self.counter),
            code: code.into(),
            level,
            title: title.into(),
            detail,
            evidence_refs,
            blocks_write,
            requires_user_confirmation,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::field_migration::types::{
        sample_column, ColumnMapping, DeclarationEvidence, DependencyEvidence,
        EnvironmentEvidence, FieldGovernanceAnalysis, IndexReference, SemanticDeclarationInput,
        TargetSpec, normalize_declarations,
    };
    use crate::ops::policy::OpsPolicy;

    fn base_analysis() -> FieldGovernanceAnalysis {
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
            columns: vec![sample_column("payload", "json")],
            dependencies: DependencyEvidence {
                primary_key_columns: vec!["id".into()],
                single_column_pk: Some("id".into()),
                scan_complete: true,
                ..Default::default()
            },
            environment: EnvironmentEvidence {
                engine: Some("InnoDB".into()),
                version: Some("8.0.36".into()),
                version_supports_json: true,
                ..Default::default()
            },
            evidence: Vec::new(),
            gaps: Vec::new(),
            declarations: Vec::new(),
            declaration_hash: String::new(),
            declaration_evidence: Vec::new(),
            target_table: "orders_payload_ext".into(),
            target_exists: false,
            empty_string_policy: crate::ops::field_migration::types::EmptyStringPolicy::Preserve,
            target_columns: Vec::new(),
            risks: Vec::new(),
            risk_summary: RiskSummary::default(),
            scripts: Vec::new(),
            dry_run: Default::default(),
            retire_package: Default::default(),
            rollback: Default::default(),
            dual_write: Default::default(),
        }
    }

    fn evaluate_with(
        analysis: &mut FieldGovernanceAnalysis,
        declarations: Vec<SemanticDeclarationInput>,
    ) -> (Vec<FieldRisk>, RiskSummary) {
        let input = FieldGovernanceInput {
            table: "orders".into(),
            column: Some("payload".into()),
            declarations,
            ..Default::default()
        };
        analysis.declarations = normalize_declarations(input.declarations.clone());
        evaluate(analysis, &input, &OpsPolicy::default());
        (analysis.risks.clone(), analysis.risk_summary.clone())
    }

    fn has_code(risks: &[FieldRisk], code: &str) -> bool {
        risks.iter().any(|risk| risk.code == code)
    }

    #[test]
    fn clean_full_profile_has_no_block() {
        let mut analysis = base_analysis();
        let (risks, summary) = evaluate_with(&mut analysis, vec![]);
        assert!(!has_code(&risks, "E09_INVALID_JSON@payload"));
        assert_eq!(summary.block, 0);
        assert_eq!(summary.risk_floor, "info");
    }

    #[test]
    fn invalid_json_blocks() {
        let mut analysis = base_analysis();
        analysis.columns[0].json_invalid_rows = 3;
        let (risks, summary) = evaluate_with(&mut analysis, vec![]);
        assert!(has_code(&risks, "E09_INVALID_JSON@payload"));
        assert_eq!(summary.block, 1);
    }

    #[test]
    fn unique_index_blocks_and_secondary_index_is_high() {
        let mut analysis = base_analysis();
        analysis.dependencies.indexes.push(IndexReference {
            name: "uk_payload".into(),
            unique: true,
            primary: false,
            index_type: "BTREE".into(),
            columns: vec!["payload".into()],
        });
        let (risks, summary) = evaluate_with(&mut analysis, vec![]);
        assert!(has_code(&risks, "E03_PRIMARY_OR_UNIQUE@payload"));
        assert_eq!(summary.block, 1);

        let mut analysis = base_analysis();
        analysis.dependencies.indexes.push(IndexReference {
            name: "idx_payload".into(),
            unique: false,
            primary: false,
            index_type: "BTREE".into(),
            columns: vec!["payload".into()],
        });
        let (risks, summary) = evaluate_with(&mut analysis, vec![]);
        assert!(has_code(&risks, "E03_SECONDARY_INDEX@payload"));
        assert_eq!(summary.block, 0);
        assert_eq!(summary.high, 1);
    }

    #[test]
    fn empty_string_policy_is_explicit_in_risk_report() {
        let mut analysis = base_analysis();
        analysis.columns[0].empty_rows = 1000;
        let input = FieldGovernanceInput {
            table: "orders".into(),
            column: Some("payload".into()),
            empty_string_policy: Some(super::super::types::EmptyStringPolicy::Preserve),
            ..Default::default()
        };
        evaluate(&mut analysis, &input, &OpsPolicy::default());
        let risk = analysis
            .risks
            .iter()
            .find(|risk| risk.code == "E08_EMPTY_STRING_POLICY")
            .expect("缺少空串策略风险项");
        assert_eq!(risk.level, RiskLevel::Info);
        assert!(risk.detail.contains("preserve"));
        assert!(!risk.blocks_write);
    }

    #[test]
    fn empty_string_policy_to_null_is_high_and_requires_confirmation() {
        let mut analysis = base_analysis();
        analysis.columns[0].empty_rows = 1000;
        let input = FieldGovernanceInput {
            table: "orders".into(),
            column: Some("payload".into()),
            empty_string_policy: Some(super::super::types::EmptyStringPolicy::ToNull),
            ..Default::default()
        };
        evaluate(&mut analysis, &input, &OpsPolicy::default());
        let risk = analysis
            .risks
            .iter()
            .find(|risk| risk.code == "E08_EMPTY_STRING_POLICY")
            .expect("缺少空串策略风险项");
        assert_eq!(risk.level, RiskLevel::High);
        assert!(risk.requires_user_confirmation);
    }

    #[test]
    fn empty_string_policy_block_only_blocks_when_hit() {
        let mut analysis = base_analysis();
        analysis.columns[0].empty_rows = 1000;
        let input = FieldGovernanceInput {
            table: "orders".into(),
            column: Some("payload".into()),
            empty_string_policy: Some(super::super::types::EmptyStringPolicy::Block),
            ..Default::default()
        };
        evaluate(&mut analysis, &input, &OpsPolicy::default());
        let risk = analysis
            .risks
            .iter()
            .find(|risk| risk.code == "E08_EMPTY_STRING_POLICY")
            .expect("缺少空串策略风险项");
        assert_eq!(risk.level, RiskLevel::Block);
        assert!(risk.blocks_write);

        let mut clean = base_analysis();
        clean.columns[0].empty_rows = 0;
        let input = FieldGovernanceInput {
            table: "orders".into(),
            column: Some("payload".into()),
            empty_string_policy: Some(super::super::types::EmptyStringPolicy::Block),
            ..Default::default()
        };
        evaluate(&mut clean, &input, &OpsPolicy::default());
        let risk = clean
            .risks
            .iter()
            .find(|risk| risk.code == "E08_EMPTY_STRING_POLICY")
            .expect("缺少空串策略风险项");
        assert_eq!(risk.level, RiskLevel::Info);
    }

    #[test]
    fn evidence_gap_and_sampled_mode_block_writes() {
        let mut analysis = base_analysis();
        analysis.gaps.push(super::super::types::EvidenceGap {
            evidence_id: "E06".into(),
            reason: "permission denied".into(),
        });
        let (risks, summary) = evaluate_with(&mut analysis, vec![]);
        assert!(has_code(&risks, "EVIDENCE_GAP"));
        assert!(summary.undetermined_write_block);

        let mut analysis = base_analysis();
        analysis.profile_mode = "sampled".into();
        let (risks, summary) = evaluate_with(&mut analysis, vec![]);
        assert!(has_code(&risks, "PROFILE_NOT_FULL"));
        assert!(summary.undetermined_write_block);
    }

    #[test]
    fn no_single_pk_blocks() {
        let mut analysis = base_analysis();
        analysis.dependencies.single_column_pk = None;
        analysis.dependencies.primary_key_columns = vec!["a".into(), "b".into()];
        let (risks, _) = evaluate_with(&mut analysis, vec![]);
        assert!(has_code(&risks, "E14_NO_SINGLE_PK"));
    }

    #[test]
    fn array_json_blocks() {
        let mut analysis = base_analysis();
        analysis.columns[0].json_array_rows = 4;
        let (risks, _) = evaluate_with(&mut analysis, vec![]);
        assert!(has_code(&risks, "E10_ARRAY_NOT_SUPPORTED@payload"));
    }

    #[test]
    fn null_to_not_null_requires_declaration_conflict_blocks() {
        let mut analysis = base_analysis();
        analysis.columns[0].null_rows = 2;
        let input = FieldGovernanceInput {
            table: "orders".into(),
            column: Some("payload".into()),
            target: Some(TargetSpec {
                mappings: vec![ColumnMapping {
                    source_key: Some("theme".into()),
                    target_column: "theme".into(),
                    target_type: Some("VARCHAR(64) NOT NULL".into()),
                    required: Some(true),
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        analysis.declarations = Vec::new();
        evaluate(&mut analysis, &input, &OpsPolicy::default());
        assert!(analysis
            .risks
            .iter()
            .any(|risk| risk.code == "E08_NULL_TO_NOT_NULL@payload"));
    }

    #[test]
    fn declaration_cannot_downgrade_existing_block() {
        let mut analysis = base_analysis();
        analysis.dependencies.indexes.push(IndexReference {
            name: "uk_payload".into(),
            unique: true,
            primary: false,
            index_type: "BTREE".into(),
            columns: vec!["payload".into()],
        });
        let declarations = vec![SemanticDeclarationInput {
            purpose: Some("这个字段以后只存用户偏好".into()),
            expected_keys: vec!["theme".into()],
            ..Default::default()
        }];
        let (risks, summary) = evaluate_with(&mut analysis, declarations);
        assert!(has_code(&risks, "E03_PRIMARY_OR_UNIQUE@payload"));
        assert!(summary.block >= 1);
    }

    #[test]
    fn declaration_only_adds_constraints() {
        let mut baseline = base_analysis();
        let (baseline_risks, _) = evaluate_with(&mut baseline, vec![]);

        let mut with_decl = base_analysis();
        with_decl.columns[0].null_rows = 2;
        let declarations = vec![SemanticDeclarationInput {
            purpose: Some("只存用户偏好".into()),
            expected_keys: vec!["theme".into()],
            required_keys: vec!["theme".into()],
            consumers: vec!["order-service".into()],
            value_domain: [("theme".to_string(), "enum(light,dark)".to_string())]
                .into_iter()
                .collect(),
            target_nullable: Some(false),
            ..Default::default()
        }];
        let (with_risks, summary) = evaluate_with(&mut with_decl, declarations);
        assert!(with_risks.len() > baseline_risks.len());
        assert!(has_code(&with_risks, "SEM_DECL_UNVERIFIED"));
        assert!(has_code(&with_risks, "SEM_DECL_CONSUMERS_UNVERIFIED"));
        assert!(has_code(&with_risks, "SEM_DECL_REQUIRED_KEY_MISSING"));
        assert!(has_code(&with_risks, "SEM_DECL_NULL_CONFLICT"));
        assert!(summary.block >= 2);
    }

    #[test]
    fn declaration_required_key_missing_blocks_even_without_target() {
        let mut analysis = base_analysis();
        analysis.columns[0].json_keys = vec!["theme".into()];
        analysis.columns[0].json_object_rows = 5;
        analysis.columns[0].json_key_presence = vec![super::super::types::KeyPresence {
            key: "theme".into(),
            rows: 3,
            ratio: 0.6,
        }];
        let declarations = vec![SemanticDeclarationInput {
            required_keys: vec!["theme".into()],
            ..Default::default()
        }];
        let (risks, _) = evaluate_with(&mut analysis, declarations);
        assert!(has_code(&risks, "SEM_DECL_REQUIRED_KEY_MISSING"));
    }

    #[test]
    fn declaration_uniqueness_conflict_blocks() {
        let mut analysis = base_analysis();
        analysis.declaration_evidence = vec![DeclarationEvidence {
            declaration_id: "decl-1".into(),
            kind: "uniqueness".into(),
            key_set: vec!["theme".into()],
            rows: 2,
            detail: "duplicate".into(),
        }];
        let declarations = vec![SemanticDeclarationInput {
            id: Some("decl-1".into()),
            unique_keys: vec!["theme".into()],
            uniqueness_expected: Some(true),
            ..Default::default()
        }];
        let (risks, _) = evaluate_with(&mut analysis, declarations);
        assert!(has_code(&risks, "SEM_DECL_UNIQUENESS_CONFLICT"));
    }

    #[test]
    fn target_length_too_small_blocks() {
        let mut analysis = base_analysis();
        analysis.columns[0].data_type = "varchar".into();
        analysis.columns[0].max_char_length = 200;
        let input = FieldGovernanceInput {
            table: "orders".into(),
            column: Some("payload".into()),
            target: Some(TargetSpec {
                mappings: vec![ColumnMapping {
                    source_key: Some("payload".into()),
                    target_column: "payload".into(),
                    target_type: Some("VARCHAR(64)".into()),
                    required: None,
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        evaluate(&mut analysis, &input, &OpsPolicy::default());
        assert!(analysis
            .risks
            .iter()
            .any(|risk| risk.code == "E01_TARGET_LENGTH_TOO_SMALL@payload"));
    }
}
