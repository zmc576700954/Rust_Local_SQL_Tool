//! S3：下线准备、回滚评估、双写方案。
//!
//! 本模块只做确定性分析与脚本生成：
//! - 不连接数据库执行任何语句；
//! - 所有脚本 `executable=false`；
//! - `DROP COLUMN` 只作为 `manual_scripts` 展示，不存在任何可执行路径。

use chrono::NaiveDate;

use crate::ops::policy::OpsPolicy;
use crate::ops::types::TableSnapshot;
use crate::sql::util::{escape_sql_string, quote_ident_mysql};

use super::dependency::contains_identifier;
use super::schema::sanitize_identifier;
use super::scripts;
use super::types::{
    ColumnProfile, DualWriteMetric, DualWritePlan, EmptyStringPolicy, FieldGovernanceAnalysis,
    FieldGovernanceInput, ManualScript, RetireChecklistItem, RetirePackage, RollbackAssessment,
};

pub fn build(
    analysis: &mut FieldGovernanceAnalysis,
    input: &FieldGovernanceInput,
    policy: &OpsPolicy,
    snapshot: &TableSnapshot,
) {
    let rollback = assess_rollback(analysis, input);
    let package = build_retire_package(analysis, input, policy, &rollback);
    let dual_write = build_dual_write_plan(analysis);

    let (before, after) = build_scripts(analysis, policy, &rollback, snapshot, &package);

    if let Some(position) = analysis
        .scripts
        .iter()
        .position(|script| script.kind == "retire")
    {
        for (offset, script) in before.into_iter().enumerate() {
            analysis.scripts.insert(position + offset, script);
        }
    } else {
        analysis.scripts.extend(before);
    }
    analysis.scripts.extend(after);

    analysis.rollback = rollback;
    analysis.retire_package = package;
    analysis.dual_write = dual_write;
    scripts::refresh_dry_run(analysis);
}

// ── 观察期 ─────────────────────────────────────────────────

struct ObservationState {
    elapsed_days: Option<u64>,
    satisfied: bool,
    status: &'static str,
    auto_verified: bool,
    note: String,
}

fn observation_state(started_at: Option<&str>, required_days: u64) -> ObservationState {
    let Some(raw) = started_at.map(str::trim).filter(|value| !value.is_empty()) else {
        return ObservationState {
            elapsed_days: None,
            satisfied: false,
            status: "unknown",
            auto_verified: false,
            note: "未提供观察期起始日期（observation_started_at）；无法自动判定，必须人工确认。"
                .into(),
        };
    };
    match NaiveDate::parse_from_str(raw, "%Y-%m-%d") {
        Ok(date) => {
            let elapsed = (chrono::Utc::now().date_naive() - date)
                .num_days()
                .max(0) as u64;
            if elapsed >= required_days {
                ObservationState {
                    elapsed_days: Some(elapsed),
                    satisfied: true,
                    status: "pass",
                    auto_verified: true,
                    note: format!("观察期已满足：已 {elapsed} 天 ≥ 要求 {required_days} 天。"),
                }
            } else {
                ObservationState {
                    elapsed_days: Some(elapsed),
                    satisfied: false,
                    status: "warn",
                    auto_verified: true,
                    note: format!(
                        "观察期未满：已 {elapsed} 天，要求 {required_days} 天。仅提示，不代替用户决定。"
                    ),
                }
            }
        }
        Err(_) => ObservationState {
            elapsed_days: None,
            satisfied: false,
            status: "unknown",
            auto_verified: false,
            note: format!(
                "观察期起始日期无法解析（{raw}），期望 YYYY-MM-DD；必须人工确认。"
            ),
        },
    }
}

// ── 下线清单 ───────────────────────────────────────────────

fn build_retire_package(
    analysis: &FieldGovernanceAnalysis,
    input: &FieldGovernanceInput,
    policy: &OpsPolicy,
    rollback: &RollbackAssessment,
) -> RetirePackage {
    let Some(column) = analysis.column.clone() else {
        return RetirePackage {
            generated: false,
            table: analysis.table.clone(),
            target_table: analysis.target_table.clone(),
            observation_required_days: policy.field_observation_days,
            observation_note: "整表盘点模式不生成下线包；请指定源列后再次分析。".into(),
            snapshot_retention_days: policy.field_snapshot_retention_days,
            rollback_window_hours: policy.field_rollback_window_hours,
            note: "整表盘点模式不生成下线包；请指定源列后再次分析。".into(),
            ..Default::default()
        };
    };

    let observation = observation_state(
        input.observation_started_at.as_deref(),
        policy.field_observation_days,
    );
    let snapshot_table = format!(
        "__mig_snapshot_{}_{}",
        sanitize_identifier(&analysis.table),
        sanitize_identifier(&column)
    );

    let mut checklist = Vec::new();

    // 依赖扫描：全部自动核实。
    push_item(
        &mut checklist,
        "dependency:scan",
        "dependency",
        "依赖扫描是否完整",
        if analysis.dependencies.scan_complete {
            "索引/外键/生成列/视图/触发器/例程均已成功读取。".to_string()
        } else {
            "至少一项依赖读取失败；依赖未知，不得下线。".to_string()
        },
        true,
        if analysis.dependencies.scan_complete {
            "pass"
        } else {
            "fail"
        },
        !analysis.dependencies.scan_complete,
        vec!["E06".into()],
    );

    let mut index_hits = Vec::new();
    for index in &analysis.dependencies.indexes {
        if index
            .columns
            .iter()
            .any(|name| name.eq_ignore_ascii_case(&column))
        {
            index_hits.push(format!(
                "{}（{}，{}）",
                index.name,
                if index.primary {
                    "PRIMARY"
                } else if index.unique {
                    "UNIQUE"
                } else {
                    "普通索引"
                },
                index.columns.join(",")
            ));
        }
    }
    push_dependency_item(
        &mut checklist,
        "dependency:indexes",
        "索引引用",
        &index_hits,
        "命中的索引必须先用人工脚本删除或改写；UNIQUE/PRIMARY 不能直接下线。",
        "E03",
    );

    let mut fk_hits = Vec::new();
    for fk in &analysis.dependencies.foreign_keys {
        if fk.column.eq_ignore_ascii_case(&column) {
            fk_hits.push(format!(
                "{}（{}：{}.{} → {}.{}）",
                fk.constraint_name,
                fk.direction,
                fk.table,
                fk.column,
                fk.referenced_table.as_deref().unwrap_or("-"),
                fk.referenced_column.as_deref().unwrap_or("-")
            ));
        }
    }
    push_dependency_item(
        &mut checklist,
        "dependency:foreign_keys",
        "外键引用",
        &fk_hits,
        "必须先人工删除或改写外键约束；工具不执行任何 ADD/DROP FOREIGN KEY。",
        "E04",
    );

    let mut generated_hits = Vec::new();
    for generated in &analysis.dependencies.generated_columns {
        if contains_identifier(&generated.expression, &column) {
            generated_hits.push(format!(
                "{}（{}）",
                generated.name, generated.expression
            ));
        }
    }
    push_dependency_item(
        &mut checklist,
        "dependency:generated_columns",
        "生成列引用",
        &generated_hits,
        "生成列表达式依赖该列，必须先人工改写表达式并重建生成列。",
        "E05",
    );

    for kind in ["view", "trigger", "routine"] {
        let hits: Vec<String> = analysis
            .dependencies
            .objects
            .iter()
            .filter(|object| object.kind == kind)
            .filter(|object| {
                object
                    .matched_columns
                    .iter()
                    .any(|name| name.eq_ignore_ascii_case(&column))
            })
            .map(|object| format!("{}（列：{}）", object.name, object.matched_columns.join(",")))
            .collect();
        let (id, title, evidence) = match kind {
            "view" => ("dependency:views", "视图引用", "E06"),
            "trigger" => ("dependency:triggers", "触发器引用", "E06"),
            _ => ("dependency:routines", "存储过程/函数引用", "E06"),
        };
        push_dependency_item(
            &mut checklist,
            id,
            title,
            &hits,
            "依赖对象必须先人工修改/删除，并与应用发布对齐；工具只生成脚本。",
            evidence,
        );
    }

    push_item(
        &mut checklist,
        "dependency:source_triggers",
        "dependency",
        "源表触发器",
        if analysis.dependencies.source_triggers.is_empty() {
            "源表未发现触发器。".to_string()
        } else {
            format!(
                "源表存在触发器：{}；触发器副作用不可控，下线前必须人工确认。",
                analysis.dependencies.source_triggers.join(", ")
            )
        },
        true,
        if analysis.dependencies.source_triggers.is_empty() {
            "pass"
        } else {
            "fail"
        },
        !analysis.dependencies.source_triggers.is_empty(),
        vec!["E06".into()],
    );

    push_item(
        &mut checklist,
        "gate:block_risks",
        "gate",
        "BLOCK 风险已清理",
        if analysis.risk_summary.block == 0 {
            "当前分析没有 BLOCK 风险。".to_string()
        } else {
            format!(
                "仍有 {} 项 BLOCK 风险；即使校验通过也不得进入下线准备。",
                analysis.risk_summary.block
            )
        },
        true,
        if analysis.risk_summary.block == 0 {
            "pass"
        } else {
            "fail"
        },
        analysis.risk_summary.block > 0,
        vec!["RISK".into()],
    );

    // 观察期：只提示，不代替用户决定。
    push_item(
        &mut checklist,
        "observation:window",
        "observation",
        "观察期是否满足",
        observation.note.clone(),
        observation.auto_verified,
        observation.status,
        false,
        vec!["OBSERVATION".into()],
    );

    // 必须人工确认的闸门。
    push_manual(
        &mut checklist,
        "gate:final_verify",
        "最终全量校验",
        "源列 → 目标表 → 快照三向校验必须全部 mismatch=0；校验失败不得下线。",
        true,
    );
    push_manual(
        &mut checklist,
        "gate:read_switch",
        "读路径已切换",
        "应用所有读路径已切到目标表；旧列不再被读取。库内无法自动验证。",
        true,
    );
    push_manual(
        &mut checklist,
        "gate:write_stop",
        "旧列已停止写入",
        "所有写路径已停止写旧列（或已开启双写）；库内无法自动验证。",
        true,
    );
    push_manual(
        &mut checklist,
        "backup:snapshot",
        "旧列快照已备份并校验",
        &format!(
            "按主键把旧列完整复制到 {}，并做行数/逐行校验；快照是删列后的唯一恢复源。",
            snapshot_table
        ),
        true,
    );
    push_manual(
        &mut checklist,
        "backup:retention",
        "快照保留期已确认",
        &format!(
            "快照至少保留 {} 天；保留期内不得删除。到期清理必须人工确认。",
            policy.field_snapshot_retention_days
        ),
        true,
    );
    push_manual(
        &mut checklist,
        "gate:rollback_ready",
        "回滚预案已确认",
        &format!(
            "回滚评估结论={}（{}）。{}",
            rollback.conclusion,
            rollback.shape,
            if rollback.irreversible {
                "该迁移判定为不可回滚；必须书面确认可接受的数据损失。"
            } else {
                "回滚脚本仅为草案，必须在预发环境演练后才能用于生产。"
            }
        ),
        rollback.irreversible,
    );
    push_manual(
        &mut checklist,
        "gate:operators",
        "执行人 / 复核人 / 停机窗口",
        "下线是 L4 人工操作：必须记录执行人、复核人、维护窗口与人工回执。",
        true,
    );

    let blockers: Vec<String> = checklist
        .iter()
        .filter(|item| item.blocks_retire && item.status != "pass")
        .map(|item| item.title.clone())
        .collect();
    let auto_verified_count = checklist
        .iter()
        .filter(|item| item.auto_verified)
        .count();
    let manual_confirmation_count = checklist.len().saturating_sub(auto_verified_count);

    RetirePackage {
        generated: true,
        table: analysis.table.clone(),
        column: Some(column),
        target_table: analysis.target_table.clone(),
        snapshot_table: Some(snapshot_table),
        observation_required_days: policy.field_observation_days,
        observation_elapsed_days: observation.elapsed_days,
        observation_satisfied: observation.satisfied,
        observation_note: observation.note,
        snapshot_retention_days: policy.field_snapshot_retention_days,
        rollback_window_hours: policy.field_rollback_window_hours,
        blockers,
        auto_verified_count,
        manual_confirmation_count,
        checklist,
        execution_order: vec![
            "1. 完成 S2 回填与最终全表校验：V1-V4 全 0、无差异主键。".into(),
            "2. 应用完成双写/切读，旧列停止写入（外部发布，工具只记录回执）。".into(),
            "3. 执行快照脚本：snapshot-create → snapshot-copy（分批）→ snapshot-verify。".into(),
            "4. 记录快照行数、校验结果、plan_id 到审计。".into(),
            "5. 再次执行最终全表校验，确认快照之后旧列没有漂移。".into(),
            "6. 人工复核本清单全部「必须人工确认」项。".into(),
            "7. 维护窗口内执行 retire-column（DROP COLUMN），记录执行人/复核人/回执。".into(),
            "8. 保留快照不少于建议天数；到期清理必须人工确认。".into(),
        ],
        recommended_window:
            "业务低峰维护窗口；避开长事务与批量导入，先确认没有 MDL 等待。DROP COLUMN 可能触发表重建，需预留磁盘与回滚时间。"
                .into(),
        irreversible: true,
        note: "本下线包只生成清单与脚本，不执行任何写操作；DROP COLUMN 没有可执行入口。"
            .into(),
    }
}

fn push_dependency_item(
    checklist: &mut Vec<RetireChecklistItem>,
    id: &str,
    title: &str,
    hits: &[String],
    guidance: &str,
    evidence_ref: &str,
) {
    push_item(
        checklist,
        id,
        "dependency",
        title,
        if hits.is_empty() {
            format!("未命中。{guidance}")
        } else {
            format!("命中 {} 项：{}。{guidance}", hits.len(), hits.join("；"))
        },
        true,
        if hits.is_empty() { "pass" } else { "fail" },
        !hits.is_empty(),
        vec![evidence_ref.into()],
    );
}

#[allow(clippy::too_many_arguments)]
fn push_item(
    checklist: &mut Vec<RetireChecklistItem>,
    id: &str,
    category: &str,
    title: &str,
    detail: impl Into<String>,
    auto_verified: bool,
    status: &str,
    blocks_retire: bool,
    evidence_refs: Vec<String>,
) {
    checklist.push(RetireChecklistItem {
        id: id.into(),
        category: category.into(),
        title: title.into(),
        detail: detail.into(),
        auto_verified,
        status: status.into(),
        blocks_retire,
        evidence_refs,
    });
}

fn push_manual(
    checklist: &mut Vec<RetireChecklistItem>,
    id: &str,
    title: &str,
    detail: &str,
    blocks_retire: bool,
) {
    push_item(
        checklist,
        id,
        "gate",
        title,
        detail,
        false,
        "manual",
        blocks_retire,
        Vec::new(),
    );
}

// ── 回滚评估 ───────────────────────────────────────────────

fn assess_rollback(
    analysis: &FieldGovernanceAnalysis,
    input: &FieldGovernanceInput,
) -> RollbackAssessment {
    let Some(profile) = analysis.selected_profile().cloned() else {
        return RollbackAssessment {
            shape: "none".into(),
            conclusion: "not_applicable".into(),
            summary: "未指定源列，无法评估回滚。".into(),
            irreversible: false,
            note: "整表盘点模式不做回滚评估。".into(),
            ..Default::default()
        };
    };
    let Some(pk) = analysis.dependencies.single_column_pk.clone() else {
        return RollbackAssessment {
            shape: "none".into(),
            conclusion: "irreversible".into(),
            summary: "源表没有单列主键，无法按主键 1:1 反写。".into(),
            reasons: vec!["无单列主键：反向写回无法可靠对齐行。".into()],
            irreversible: true,
            note: "仅输出评估，不生成反向脚本。".into(),
            ..Default::default()
        };
    };

    let shape = classify_shape(analysis, &profile);
    let mut reasons = Vec::new();
    let mut lossy_aspects = Vec::new();
    let mut non_restorable = Vec::new();

    if input.empty_string_policy() == EmptyStringPolicy::ToNull {
        lossy_aspects.push(
            "空串策略 to_null：目标表把空串写成 NULL，反写时 NULL 无法区分原始 NULL 与空串。"
                .into(),
        );
        non_restorable.push("空串与 NULL 的区分".into());
    }

    if shape == "json_1_to_n" {
        reasons.push(
            "源列包含 JSON 顶层数组（1:N）：目标表按元素拆行，无法从多行无损还原数组边界、重复元素与原始词元。"
                .into(),
        );
        reasons.push(
            "1:N 反向写回缺少合并规则（顺序、重复、最大长度），因此判定不可回滚。".into(),
        );
        non_restorable.extend([
            "数组边界".into(),
            "元素顺序".into(),
            "重复元素".into(),
            "折叠前的原始词元".into(),
        ]);
    }

    if shape == "json_split_1_to_1" {
        let mut mapped_keys = Vec::new();
        for column in &analysis.target_columns {
            if let Some(key) = column.source_key.as_deref() {
                mapped_keys.push(key.to_ascii_lowercase());
            }
        }
        let residual = input
            .target
            .as_ref()
            .map(|target| target.residual_json())
            .unwrap_or(false);
        if !residual {
            let unmapped: Vec<String> = profile
                .json_keys
                .iter()
                .filter(|key| !mapped_keys.contains(&key.to_ascii_lowercase()))
                .cloned()
                .collect();
            if !unmapped.is_empty() {
                lossy_aspects.push(format!(
                    "未映射 JSON key（{}）不会写入目标表，反写会丢失这些字段。",
                    unmapped.join(",")
                ));
            }
        }
        for column in &analysis.target_columns {
            let Some(key) = column.source_key.as_deref() else {
                continue;
            };
            let Some(key_types) = profile
                .json_key_types
                .iter()
                .find(|item| item.key.eq_ignore_ascii_case(key))
            else {
                continue;
            };
            let non_numeric: Vec<String> = key_types
                .types
                .keys()
                .filter(|kind| {
                    !matches!(kind.as_str(), "INTEGER" | "DOUBLE" | "NULL")
                })
                .cloned()
                .collect();
            if is_numeric_sql_type(&column.sql_type) && !non_numeric.is_empty() {
                lossy_aspects.push(format!(
                    "key {} 的源类型包含 {:?}，写入数值列 {} 后无法还原原始类型/文本。",
                    key, non_numeric, column.sql_type
                ));
            }
        }
    }

    if shape == "scalar_1_to_1" {
        if let Some(target) = analysis.target_columns.first() {
            let source_family = type_family(&profile.data_type);
            let target_family = type_family(&target.sql_type);
            if source_family != target_family {
                lossy_aspects.push(format!(
                    "类型从 {} 变为 {}，反向转换不可证明无损。",
                    profile.column_type, target.sql_type
                ));
            }
            if let Some(length) = parse_varchar_length(&target.sql_type) {
                if profile.is_text_like() && profile.max_char_length > length {
                    lossy_aspects.push(format!(
                        "目标列长度 {} 小于观测最大长度 {}，反写前数据可能已被截断。",
                        length, profile.max_char_length
                    ));
                }
            }
        }
    }

    let irreversible = shape == "json_1_to_n"
        || input.empty_string_policy() == EmptyStringPolicy::ToNull
        || !lossy_aspects.is_empty();
    if shape == "json_split_1_to_1" {
        non_restorable.extend([
            "JSON key 顺序".into(),
            "数值词元（如 1 与 1.0）".into(),
            "重复 key".into(),
            "空白与转义形式".into(),
        ]);
        if !irreversible {
            lossy_aspects.push(
                "JSON 对象的 key 顺序、数字词元（1 vs 1.0）与重复 key 在拆列后不再保留；反写只能重建语义等价的 JSON，不能还原字节级原文。"
                    .into(),
            );
        }
    }

    let (conclusion, summary) = if irreversible {
        let mut all_reasons = reasons.clone();
        all_reasons.extend(lossy_aspects.clone());
        (
            "irreversible",
            format!("不可回滚：{}", all_reasons.join(" ")),
        )
    } else if shape == "json_split_1_to_1" {
        (
            "rebuildable_semantic_equivalent",
            "可重建（语义等价）：目标表可以重建语义等价的 JSON，但无法一键还原源列字节。不可还原项：key 顺序、数值词元（如 1 与 1.0）、重复 key、空白与转义形式。反向脚本仅为草案。"
                .to_string(),
        )
    } else {
        (
            "reversible",
            "可回滚：目标表可按主键 1:1 反写回源列；前提是源列仍存在、旧列已停写，且反写前做一次逐行校验。"
                .to_string(),
        )
    };

    let mut prereqs = vec![
        "源列仍然存在（未执行 DROP COLUMN）。".to_string(),
        "旧列或目标表的写入已冻结，避免反写过程中出现新差异。".to_string(),
        "反写必须绑定审批与审计，并先做只读逐行比对。".to_string(),
        "下线前快照已生成并校验；反写失败时以快照为准。".to_string(),
    ];
    if shape == "json_split_1_to_1" {
        prereqs.push("反向 JSON 的 key 集合、空串策略与源端完全一致。".into());
    }
    if analysis.target_table.trim().is_empty() {
        prereqs.push("缺少目标表名，无法生成反向脚本。".into());
    }

    let reverse_sql = if irreversible {
        None
    } else {
        reverse_sql(analysis, input, &profile, pk.as_str(), shape)
    };

    let note = format!(
        "该结论只基于当前证据与映射，不替代人工评审。目标表 {}；1:1/1:N 判定依据 shape={shape}。下线前快照是最终恢复源。",
        analysis.target_table
    );

    RollbackAssessment {
        shape: shape.into(),
        conclusion: conclusion.into(),
        summary,
        reasons,
        prerequisites: prereqs,
        lossy_aspects,
        non_restorable,
        irreversible,
        reverse_sql,
        note,
    }
}

fn classify_shape(analysis: &FieldGovernanceAnalysis, profile: &ColumnProfile) -> &'static str {
    if profile.json_array_rows > 0 {
        return "json_1_to_n";
    }
    if profile.is_json_type()
        && (!profile.json_key_types.is_empty()
            || analysis
                .target_columns
                .iter()
                .any(|column| column.source_key.is_some()))
    {
        return "json_split_1_to_1";
    }
    "scalar_1_to_1"
}

fn reverse_sql(
    analysis: &FieldGovernanceAnalysis,
    input: &FieldGovernanceInput,
    profile: &ColumnProfile,
    pk: &str,
    shape: &str,
) -> Option<String> {
    let source = format!(
        "{}.{}",
        quote_ident_mysql(&analysis.db_name),
        quote_ident_mysql(&analysis.table)
    );
    let destination = format!(
        "{}.{}",
        quote_ident_mysql(&analysis.db_name),
        quote_ident_mysql(&analysis.target_table)
    );
    let column = quote_ident_mysql(profile.column.as_str());
    let pk_ident = quote_ident_mysql(pk);
    if shape == "scalar_1_to_1" {
        let target_column = analysis
            .target_columns
            .first()
            .map(|target| quote_ident_mysql(&target.name))
            .unwrap_or_else(|| column.clone());
        return Some(format!(
            "-- 回滚草案（L3 人工执行，非自动）：目标表 1:1 反写回源列\n\
             -- 前提：源列仍存在、旧列已停写、已做逐行校验。\n\
             UPDATE {source} s JOIN {destination} t ON t.{pk_ident} = s.{pk_ident}\n\
             SET s.{column} = t.{target_column};"
        ));
    }
    if shape == "json_split_1_to_1" {
        let mut pairs = Vec::new();
        for target in &analysis.target_columns {
            let Some(key) = target.source_key.as_deref() else {
                continue;
            };
            let key = escape_sql_string(key);
            pairs.push(format!(
                "'{key}', t.{}",
                quote_ident_mysql(&target.name)
            ));
        }
        if pairs.is_empty() {
            return None;
        }
        let residual = input
            .target
            .as_ref()
            .map(|target| target.residual_json())
            .unwrap_or(false);
        let object = format!("JSON_OBJECT({})", pairs.join(", "));
        let value = if residual {
            format!(
                "JSON_MERGE_PATCH({object}, COALESCE(t.`residual_json`, JSON_OBJECT()))"
            )
        } else {
            object
        };
        return Some(format!(
            "-- 回滚草案（L3 人工执行，非自动）：JSON 拆列反写，仅当所有 key 均无损\n\
             -- 注意：key 顺序、数字词元与重复 key 不保证字节级还原。\n\
             UPDATE {source} s JOIN {destination} t ON t.{pk_ident} = s.{pk_ident}\n\
             SET s.{column} = {value};"
        ));
    }
    None
}

fn is_numeric_sql_type(sql_type: &str) -> bool {
    let upper = sql_type.trim().to_ascii_uppercase();
    [
        "BIGINT", "INT", "SMALLINT", "MEDIUMINT", "TINYINT", "DECIMAL", "NUMERIC", "DOUBLE",
        "FLOAT", "REAL",
    ]
    .iter()
    .any(|prefix| upper.starts_with(prefix))
}

fn type_family(sql_type: &str) -> &'static str {
    let upper = sql_type.trim().to_ascii_uppercase();
    if upper.starts_with("VARCHAR")
        || upper.starts_with("CHAR")
        || upper.ends_with("TEXT")
        || upper.starts_with("TEXT")
    {
        "text"
    } else if upper == "JSON" {
        "json"
    } else if matches!(
        upper.as_str(),
        "TINYINT" | "SMALLINT" | "MEDIUMINT" | "INT" | "INTEGER" | "BIGINT"
    ) {
        "integer"
    } else if upper.starts_with("DECIMAL") || upper.starts_with("NUMERIC") {
        "decimal"
    } else if matches!(upper.as_str(), "FLOAT" | "DOUBLE" | "REAL") {
        "float"
    } else if upper.starts_with("DATE") || upper.starts_with("TIME") {
        "datetime"
    } else {
        "other"
    }
}

fn parse_varchar_length(sql_type: &str) -> Option<u64> {
    let upper = sql_type.to_ascii_uppercase();
    if !(upper.starts_with("VARCHAR") || upper.starts_with("CHAR")) {
        return None;
    }
    let open = upper.find('(')?;
    let close = upper[open + 1..].find(')')?;
    upper[open + 1..open + 1 + close].trim().parse().ok()
}

// ── 双写方案 ───────────────────────────────────────────────

fn build_dual_write_plan(analysis: &FieldGovernanceAnalysis) -> DualWritePlan {
    let target = if analysis.target_table.trim().is_empty() {
        "目标表".to_string()
    } else {
        analysis.target_table.clone()
    };
    DualWritePlan {
        mode: "proposal_only".into(),
        steps: vec![
            "1. 应用改造：所有写路径在同一事务/同一请求内同时写旧列与目标表；旧列仍是唯一事实源。工具不改用户代码，也不编排发布。".into(),
            format!("2. 历史回填：用 S2 执行器把源列回填到 {target}，直到 V1-V4 全 0。"),
            "3. 双写对账：按固定周期跑 V1-V4（行数、主键集合、逐行哈希、差异主键），差异必须为 0。".into(),
            "4. 进入观察期：观察期内持续对账，并统计双写失败/延迟；未满观察期只提示，不代替用户决定。".into(),
            "5. 切读：应用读路径切到目标表；旧列保留双写，但不再读取。".into(),
            "6. 停写旧列：观察期无差异后再单独发布「停写旧列」，与切读分成两个窗口。".into(),
            "7. 快照 + 最终校验 + 人工 DROP COLUMN：仍按下线包执行，工具不自动删列。".into(),
        ],
        switch_point: vec![
            "切读点：双写已上线、历史回填完成且至少一次全量 V1-V4=0，再发布读路径切换。".into(),
            "停写点：切读后观察期满足且双写对账持续为 0，再发布旧列停写；不得与切读同批发布。".into(),
            "下线点：停写后生成旧列快照、完成三向校验，最后人工执行 DROP COLUMN。".into(),
        ],
        metrics: vec![
            DualWriteMetric {
                name: "dual_write_mismatch_total".into(),
                target: "0".into(),
                alert_threshold: "任意非 0 即告警并暂停切读/停写".into(),
                rationale: "对账发现差异说明双写或回填遗漏。".into(),
            },
            DualWriteMetric {
                name: "dual_write_error_total".into(),
                target: "0".into(),
                alert_threshold: "5 分钟内 > 0 即告警".into(),
                rationale: "目标表写入失败会导致长期不一致。".into(),
            },
            DualWriteMetric {
                name: "dual_write_lag_seconds".into(),
                target: "≤ 1s".into(),
                alert_threshold: "持续 > 60s 即回退".into(),
                rationale: "双写应在业务请求内完成，异步化会引入窗口。".into(),
            },
            DualWriteMetric {
                name: "source_target_row_drift".into(),
                target: "0".into(),
                alert_threshold: "任意非 0 即暂停".into(),
                rationale: "行数漂移通常意味着漏写/重复写。".into(),
            },
            DualWriteMetric {
                name: "observation_days".into(),
                target: "≥ 7".into(),
                alert_threshold: "未满观察期不得停写/下线".into(),
                rationale: "观察期用于覆盖周期性业务与补偿任务。".into(),
            },
            DualWriteMetric {
                name: "write_latency_p99_ms".into(),
                target: "≤ 基线 + 10%".into(),
                alert_threshold: "连续 10 分钟超阈值即回退".into(),
                rationale: "双写会放大写延迟，必须先确认容量。".into(),
            },
        ],
        rollback_conditions: vec![
            "双写对账出现任意非 0 差异，且 15 分钟内无法定位。".into(),
            "目标表写入错误率持续升高或写延迟超过基线 10%。".into(),
            "行数漂移、主键集合缺失/多出，或观察期不满足。".into(),
            "应用版本回滚，或切换读后出现业务错误。".into(),
            "回退方式：先把读路径切回旧列，再停止目标表写入；已写入目标表的数据保留，不做自动删除。".into(),
        ],
        note: "只出方案，不编排应用切换、不改用户代码、不执行任何写操作；所有切换均为外部人工发布。"
            .into(),
    }
}

// ── 下线/快照脚本 ───────────────────────────────────────────

fn build_scripts(
    analysis: &FieldGovernanceAnalysis,
    policy: &OpsPolicy,
    rollback: &RollbackAssessment,
    snapshot: &TableSnapshot,
    package: &RetirePackage,
) -> (Vec<ManualScript>, Vec<ManualScript>) {
    let Some(column) = analysis.column.clone() else {
        return (Vec::new(), Vec::new());
    };
    let Some(pk) = analysis.dependencies.single_column_pk.clone() else {
        return (Vec::new(), Vec::new());
    };
    let Some(snapshot_table) = package.snapshot_table.clone() else {
        return (Vec::new(), Vec::new());
    };

    let pk_type = snapshot
        .find_column(&pk)
        .map(|item| item.column_type.clone())
        .unwrap_or_else(|| "BIGINT".into());
    let column_type = analysis
        .selected_profile()
        .map(|profile| profile.column_type.clone())
        .unwrap_or_else(|| "TEXT".into());
    let source = format!(
        "{}.{}",
        quote_ident_mysql(&analysis.db_name),
        quote_ident_mysql(&analysis.table)
    );
    let source_column = quote_ident_mysql(&column);
    let pk_ident = quote_ident_mysql(&pk);
    let snapshot_ref = format!(
        "{}.{}",
        quote_ident_mysql(&analysis.db_name),
        quote_ident_mysql(&snapshot_table)
    );

    let before = vec![
        ManualScript {
            id: "snapshot-create".into(),
            title: "下线前：创建旧列快照表".into(),
            kind: "snapshot".into(),
            phase: "L3_snapshot".into(),
            sql: format!(
                "-- 下线前强制快照（只生成，不执行）\n\
                 -- 快照表只含主键与旧列，不复制二级索引；执行时建议名称追加 yyyymmddhhmm。\n\
                 CREATE TABLE IF NOT EXISTS {snapshot_ref} (\n\
                   {pk_ident} {pk_type} NOT NULL,\n\
                   {source_column} {column_type} NULL,\n\
                   PRIMARY KEY ({pk_ident})\n\
                 ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;"
            ),
            executable: false,
            irreversible: false,
            notes: vec![
                "快照表是删列后的唯一恢复源，必须先校验再允许下线。".into(),
                "快照表不参与业务读写；到期清理必须人工确认。".into(),
            ],
        },
        ManualScript {
            id: "snapshot-copy".into(),
            title: "下线前：分批复制旧列到快照表".into(),
            kind: "snapshot".into(),
            phase: "L3_snapshot".into(),
            sql: format!(
                "-- 分批复制（草案；本工具不执行）\n\
                 -- :last_pk / :end_pk / :batch_size 由人工替换；每批独立事务。\n\
                 INSERT INTO {snapshot_ref} ({pk_ident}, {source_column})\n\
                 SELECT {pk_ident}, {source_column}\n\
                 FROM {source}\n\
                 WHERE {pk_ident} > :last_pk AND {pk_ident} <= :end_pk\n\
                 ORDER BY {pk_ident}\n\
                 LIMIT :batch_size\n\
                 ON DUPLICATE KEY UPDATE {source_column} = VALUES({source_column});"
            ),
            executable: false,
            irreversible: false,
            notes: vec![
                "建议复用 S2 的分批参数与限速，避免长事务。".into(),
                "复制完成后必须执行 snapshot-verify，mismatch=0 才能继续。".into(),
            ],
        },
        ManualScript {
            id: "snapshot-verify".into(),
            title: "下线前：校验快照与旧列一致".into(),
            kind: "snapshot".into(),
            phase: "L3_snapshot".into(),
            sql: format!(
                "-- 只读校验：行数 + 严格逐行比较（SQL NULL 与空串不合并）\n\
                 SELECT (SELECT COUNT(*) FROM {source}) AS source_rows,\n\
                        (SELECT COUNT(*) FROM {snapshot_ref}) AS snapshot_rows,\n\
                        (SELECT COUNT(*) FROM {source} s LEFT JOIN {snapshot_ref} snap\n\
                         ON snap.{pk_ident} = s.{pk_ident}\n\
                         WHERE snap.{pk_ident} IS NULL) AS missing_rows,\n\
                        (SELECT COUNT(*) FROM {source} s JOIN {snapshot_ref} snap\n\
                         ON snap.{pk_ident} = s.{pk_ident}\n\
                         WHERE NOT ((s.{source_column} IS NULL AND snap.{source_column} IS NULL)\n\
                              OR (s.{source_column} IS NOT NULL AND snap.{source_column} IS NOT NULL\n\
                                  AND CAST(s.{source_column} AS BINARY) = CAST(snap.{source_column} AS BINARY)))\n\
                         ) AS mismatch_rows;"
            ),
            executable: false,
            irreversible: false,
            notes: vec![
                "missing_rows=0 且 mismatch_rows=0 才能标记 snapshot_taken。".into(),
                "校验结果必须写入审计（含 row_count 与 plan_id）。".into(),
            ],
        },
    ];

    let mut after = Vec::new();
    if !rollback.irreversible {
        if let Some(reverse_sql) = rollback.reverse_sql.clone() {
            after.push(ManualScript {
                id: "rollback-reverse".into(),
                title: "回滚草案：目标表反写回源列（有条件）".into(),
                kind: "rollback".into(),
                phase: "rollback".into(),
                sql: reverse_sql,
                executable: false,
                irreversible: false,
                notes: vec![
                    format!("回滚评估结论={}（{}）。", rollback.conclusion, rollback.shape),
                    "仅在源列仍存在、旧列已停写且逐行校验通过后使用。".into(),
                    "反写会覆盖源列；必须审批、审计并在预发演练。".into(),
                ],
            });
        }
    }
    after.push(ManualScript {
        id: "rollback-restore-snapshot".into(),
        title: "回滚：从旧列快照恢复源列".into(),
        kind: "rollback".into(),
        phase: "rollback".into(),
        sql: format!(
            "-- 回滚：用快照恢复旧列（只生成，不执行）\n\
             -- 前提：旧列仍存在；快照完整且校验通过；快照后的新写入需要另行评估。\n\
             UPDATE {source} s JOIN {snapshot_ref} snap ON snap.{pk_ident} = s.{pk_ident}\n\
             SET s.{source_column} = snap.{source_column};"
        ),
        executable: false,
        irreversible: false,
        notes: vec![
            format!(
                "快照保留建议不少于 {} 天；超过窗口的回滚可能丢失快照后的写入。",
                policy.field_snapshot_retention_days
            ),
        ],
    });
    after.push(ManualScript {
        id: "snapshot-drop".into(),
        title: "回滚：删除旧列快照表（不可逆）".into(),
        kind: "rollback".into(),
        phase: "rollback".into(),
        sql: format!("DROP TABLE IF EXISTS {snapshot_ref};"),
        executable: false,
        irreversible: true,
        notes: vec![
            "删除备份后无恢复源；必须在保留期结束且人工确认后才能执行。".into(),
            "工具不自动清理快照表。".into(),
        ],
    });

    (before, after)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::field_migration::schema::TargetColumn;
    use crate::ops::field_migration::types::{
        DependencyEvidence, DependencyObject, GeneratedColumnReference, IndexReference,
        KeyTypeDistribution, RiskSummary,
    };

    fn profile(column: &str, data_type: &str, column_type: &str) -> ColumnProfile {
        ColumnProfile {
            column: column.into(),
            data_type: data_type.into(),
            column_type: column_type.into(),
            nullable: true,
            max_char_length: 10,
            ..Default::default()
        }
    }

    fn analysis_for(profile: ColumnProfile) -> FieldGovernanceAnalysis {
        FieldGovernanceAnalysis {
            db_name: "e2e".into(),
            table: "orders".into(),
            column: Some(profile.column.clone()),
            columns: vec![profile],
            target_table: "orders_ext".into(),
            target_exists: false,
            risk_summary: RiskSummary::default(),
            dependencies: DependencyEvidence {
                primary_key_columns: vec!["id".into()],
                single_column_pk: Some("id".into()),
                scan_complete: true,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn input() -> FieldGovernanceInput {
        FieldGovernanceInput {
            table: "orders".into(),
            column: Some("owner_id".into()),
            ..Default::default()
        }
    }

    fn snapshot() -> TableSnapshot {
        TableSnapshot {
            db_name: "e2e".into(),
            table_name: "orders".into(),
            ..Default::default()
        }
    }

    fn find_item<'a>(package: &'a RetirePackage, id: &str) -> &'a RetireChecklistItem {
        package
            .checklist
            .iter()
            .find(|item| item.id == id)
            .unwrap_or_else(|| panic!("缺少清单项 {id}: {:?}", package.checklist))
    }

    #[test]
    fn dependency_checklist_hits_index_fk_generated_view_trigger_routine() {
        let mut analysis = analysis_for(profile("owner_id", "varchar", "varchar(64)"));
        analysis.target_columns = vec![TargetColumn {
            name: "owner_id".into(),
            source_key: None,
            sql_type: "varchar(64)".into(),
            nullable: true,
            required: false,
            note: None,
        }];
        analysis.dependencies = DependencyEvidence {
            primary_key_columns: vec!["id".into()],
            single_column_pk: Some("id".into()),
            indexes: vec![IndexReference {
                name: "idx_owner".into(),
                unique: false,
                primary: false,
                index_type: "BTREE".into(),
                columns: vec!["owner_id".into()],
            }],
            foreign_keys: vec![crate::ops::field_migration::types::ForeignKeyReference {
                constraint_name: "fk_owner".into(),
                direction: "child".into(),
                table: "orders".into(),
                column: "owner_id".into(),
                referenced_table: Some("users".into()),
                referenced_column: Some("id".into()),
            }],
            generated_columns: vec![GeneratedColumnReference {
                name: "owner_copy".into(),
                expression: "(`owner_id` + 1)".into(),
                extra: "STORED GENERATED".into(),
            }],
            objects: vec![
                DependencyObject {
                    kind: "view".into(),
                    name: "v_orders".into(),
                    matched_columns: vec!["owner_id".into()],
                },
                DependencyObject {
                    kind: "trigger".into(),
                    name: "trg_orders".into(),
                    matched_columns: vec!["owner_id".into()],
                },
                DependencyObject {
                    kind: "routine".into(),
                    name: "fn_owner".into(),
                    matched_columns: vec!["owner_id".into()],
                },
            ],
            source_triggers: vec!["trg_orders".into()],
            scan_complete: true,
            gaps: Vec::new(),
        };
        let policy = OpsPolicy::default();
        let input = input();
        scripts::generate(&snapshot(), &mut analysis, &input);
        build(&mut analysis, &input, &policy, &snapshot());

        let package = &analysis.retire_package;
        assert!(package.generated);
        for id in [
            "dependency:indexes",
            "dependency:foreign_keys",
            "dependency:generated_columns",
            "dependency:views",
            "dependency:triggers",
            "dependency:routines",
            "dependency:source_triggers",
        ] {
            assert_eq!(
                find_item(package, id).status,
                "fail",
                "依赖项 {id} 应命中并阻断: {}",
                find_item(package, id).detail
            );
            assert!(find_item(package, id).auto_verified, "{id} 应为自动核实");
        }
        assert!(package.blockers.iter().any(|item| item == "索引引用"));
        assert!(package.auto_verified_count >= 7);
        assert!(package.manual_confirmation_count >= 5);
        assert!(
            package
                .checklist
                .iter()
                .filter(|item| !item.auto_verified)
                .all(|item| matches!(item.status.as_str(), "manual" | "unknown"))
        );

        // 所有脚本只展示，绝不带执行入口；DROP COLUMN 只出现在不可执行脚本里。
        assert!(!analysis.scripts.is_empty());
        for script in &analysis.scripts {
            assert!(!script.executable, "脚本 {} 不能标记为可执行", script.id);
        }
        let retire = analysis
            .scripts
            .iter()
            .find(|script| script.kind == "retire")
            .expect("缺少 retire 脚本");
        assert!(retire.sql.contains("DROP COLUMN"));
        assert!(retire.irreversible);
        assert!(analysis.scripts.iter().any(|script| script.id == "snapshot-create"));
        assert!(analysis.scripts.iter().any(|script| script.id == "snapshot-copy"));
        assert!(analysis.scripts.iter().any(|script| script.id == "snapshot-verify"));
        assert!(!analysis.dry_run.executed_against_database);
        assert!(
            analysis
                .dry_run
                .checks
                .iter()
                .any(|check| check.script_id == "snapshot-copy"),
            "dry-run 应覆盖 S3 追加的快照脚本"
        );
    }

    #[test]
    fn observation_period_is_only_a_warning() {
        let today = chrono::Utc::now().date_naive().format("%Y-%m-%d").to_string();
        let mut analysis = analysis_for(profile("owner_id", "varchar", "varchar(64)"));
        let mut observation_input = input();
        observation_input.observation_started_at = Some(today);
        build(&mut analysis, &observation_input, &OpsPolicy::default(), &snapshot());

        let package = &analysis.retire_package;
        assert!(!package.observation_satisfied);
        assert_eq!(package.observation_elapsed_days, Some(0));
        assert!(package.observation_note.contains("观察期未满"));
        assert!(package.observation_note.contains("仅提示"));
        let item = find_item(package, "observation:window");
        assert_eq!(item.status, "warn");
        assert!(item.auto_verified);
        assert!(
            !item.blocks_retire,
            "观察期只提示，不代替用户决定，也不应阻断下线清单"
        );

        let mut missing = analysis_for(profile("owner_id", "varchar", "varchar(64)"));
        build(&mut missing, &input(), &OpsPolicy::default(), &snapshot());
        assert!(missing
            .retire_package
            .observation_note
            .contains("必须人工确认"));
        assert_eq!(find_item(&missing.retire_package, "observation:window").status, "unknown");
    }

    #[test]
    fn rollback_1_to_1_is_reversible_and_1_to_n_is_not() {
        // 1:1 标量：同族文本、长度足够 → 可回滚。
        let mut scalar = analysis_for(profile("owner_id", "varchar", "varchar(64)"));
        scalar.target_columns = vec![TargetColumn {
            name: "owner_id".into(),
            source_key: None,
            sql_type: "varchar(64)".into(),
            nullable: true,
            required: false,
            note: None,
        }];
        build(&mut scalar, &input(), &OpsPolicy::default(), &snapshot());
        assert_eq!(scalar.rollback.shape, "scalar_1_to_1");
        assert_eq!(scalar.rollback.conclusion, "reversible");
        assert!(!scalar.rollback.irreversible);
        assert!(scalar.rollback.summary.contains("可回滚"));
        let sql = scalar.rollback.reverse_sql.as_deref().unwrap_or_default();
        assert!(sql.contains("UPDATE"));
        assert!(sql.contains("SET s.`owner_id` = t.`owner_id`"));

        // 1:N：JSON 顶层数组 → 明确不可回滚，且不生成反向脚本。
        let mut json = analysis_for(profile("tags", "json", "json"));
        json.column = Some("tags".into());
        json.columns[0].json_keys = vec!["a".into(), "b".into()];
        json.columns[0].json_array_rows = 3;
        build(&mut json, &input(), &OpsPolicy::default(), &snapshot());
        assert_eq!(json.rollback.shape, "json_1_to_n");
        assert_eq!(json.rollback.conclusion, "irreversible");
        assert!(json.rollback.irreversible);
        assert!(json.rollback.summary.contains("不可回滚"));
        assert!(json.rollback.summary.contains("1:N"));
        assert!(json.rollback.reverse_sql.is_none());
    }

    #[test]
    fn rollback_json_split_is_rebuildable_semantically_and_to_null_is_irreversible() {
        let mut split = analysis_for(profile("payload", "json", "json"));
        split.column = Some("payload".into());
        split.columns[0].json_keys = vec!["theme".into(), "age".into()];
        split.columns[0].json_key_types = vec![
            KeyTypeDistribution {
                key: "theme".into(),
                types: [("STRING".to_string(), 10u64)].into_iter().collect(),
            },
            KeyTypeDistribution {
                key: "age".into(),
                types: [("INTEGER".to_string(), 10u64)].into_iter().collect(),
            },
        ];
        split.target_columns = vec![
            TargetColumn {
                name: "theme".into(),
                source_key: Some("theme".into()),
                sql_type: "varchar(64)".into(),
                nullable: true,
                required: false,
                note: None,
            },
            TargetColumn {
                name: "age".into(),
                source_key: Some("age".into()),
                sql_type: "bigint".into(),
                nullable: true,
                required: false,
                note: None,
            },
        ];
        build(&mut split, &input(), &OpsPolicy::default(), &snapshot());
        assert_eq!(split.rollback.shape, "json_split_1_to_1");
        assert_eq!(split.rollback.conclusion, "rebuildable_semantic_equivalent");
        assert!(!split.rollback.irreversible);
        assert!(split.rollback.summary.contains("可重建（语义等价）"));
        assert!(split.rollback.summary.contains("无法一键还原"));
        for item in ["JSON key 顺序", "数值词元（如 1 与 1.0）", "重复 key"] {
            assert!(
                split.rollback.non_restorable.iter().any(|value| value == item),
                "non_restorable 缺少 {item}: {:?}",
                split.rollback.non_restorable
            );
        }
        let sql = split.rollback.reverse_sql.as_deref().unwrap_or_default();
        assert!(sql.contains("JSON_OBJECT"));
        assert!(sql.contains("'theme'"));

        // 空串 → NULL 折叠：任何形态都不可回滚。
        let mut to_null = analysis_for(profile("owner_id", "varchar", "varchar(64)"));
        to_null.target_columns = vec![TargetColumn {
            name: "owner_id".into(),
            source_key: None,
            sql_type: "varchar(64)".into(),
            nullable: true,
            required: false,
            note: None,
        }];
        let mut input = input();
        input.empty_string_policy = Some(EmptyStringPolicy::ToNull);
        build(&mut to_null, &input, &OpsPolicy::default(), &snapshot());
        assert_eq!(to_null.rollback.conclusion, "irreversible");
        assert!(to_null.rollback.irreversible);
        assert!(to_null.rollback.summary.contains("空串"));
        assert!(to_null.rollback.reverse_sql.is_none());
    }

    #[test]
    fn retire_package_requires_a_source_column() {
        let mut analysis = FieldGovernanceAnalysis {
            db_name: "e2e".into(),
            table: "orders".into(),
            ..Default::default()
        };
        build(&mut analysis, &input(), &OpsPolicy::default(), &snapshot());
        assert!(!analysis.retire_package.generated);
        assert!(analysis.retire_package.note.contains("指定源列"));
        assert_eq!(analysis.scripts.len(), 0);
        assert_eq!(analysis.rollback.shape, "none");
    }
}
