//! 字段治理 / 拆表迁移。
//!
//! S1：证据采集（E01-E14）→ 三档风险报告 → dry-run 脚本包。
//! S2：独立回填执行器（分批 / checkpoint / 限速 / 批校验 / 审计），
//! 不接入索引 `Executor`、不复用 `DdlEngine`，且没有任何 DROP COLUMN 执行路径。

pub mod backfill;
pub mod dependency;
pub mod diff;
pub mod evidence;
pub mod migration_store;
pub mod read_only;
pub mod risk;
pub mod schema;
pub mod scripts;
pub mod types;

use serde_json::Value;

use crate::ops::error::OpsError;
use crate::ops::policy::OpsPolicy;
use crate::ops::registry::{
    OpsContext, OpsScenario, PlanIdentity, ScenarioDescriptor, ScenarioStatus,
};
use crate::ops::types::{
    now, EvidencePack, EvidenceSummary, Finding, OpsPlan, PlanStatus, RiskFinding, Severity,
    ValidationCheck, ValidationStatus,
};

use evidence::Collected;
use types::FieldGovernanceAnalysis;

pub struct FieldGovernanceScenario {
    descriptor: ScenarioDescriptor,
}

impl FieldGovernanceScenario {
    pub fn new() -> Self {
        Self {
            descriptor: ScenarioDescriptor {
                id: "field-governance".into(),
                title: "字段治理（只读分析）".into(),
                description: "采集 E01-E14 证据、生成三档风险报告与 dry-run 脚本包；\
                    不建表、不写数据、不删列。"
                    .into(),
                engines: vec!["mysql".into(), "mariadb".into()],
                status: ScenarioStatus::Enabled,
                risk_ceiling: crate::ops::types::RiskClass::L0,
                allowed_actions: vec![],
            },
        }
    }
}

impl Default for FieldGovernanceScenario {
    fn default() -> Self {
        Self::new()
    }
}

impl OpsScenario for FieldGovernanceScenario {
    fn descriptor(&self) -> &ScenarioDescriptor {
        &self.descriptor
    }

    fn collect_evidence<'a>(
        &'a self,
        ctx: &'a OpsContext,
        input: &'a Value,
    ) -> crate::db::protocol::BoxFuture<'a, Result<EvidencePack, OpsError>> {
        Box::pin(async move {
            let input = parse_input(input)?;
            let policy = ctx
                .policy
                .as_ref()
                .map(|policy| policy.as_ref().clone())
                .unwrap_or_default();
            let collected = analyze(ctx, &input, &policy).await?;
            let analysis_value = serde_json::to_value(&collected.analysis)?;
            Ok(EvidencePack {
                db_name: ctx.db_name.clone(),
                table: input.table.clone(),
                snapshot: collected.snapshot,
                stats: collected.stats,
                explain: Vec::new(),
                index_usage: Vec::new(),
                workload_sql: Vec::new(),
                collected_at: now(),
                notes: vec![
                    "只读分析：未执行任何写库语句。".into(),
                    format!("profile_mode={}", collected.analysis.profile_mode),
                ],
                perf: None,
                extra: Some(analysis_value),
            })
        })
    }

    fn build_plan(
        &self,
        identity: &PlanIdentity,
        evidence: &EvidencePack,
        _input: &Value,
        _policy: &OpsPolicy,
    ) -> Result<OpsPlan, OpsError> {
        let analysis: FieldGovernanceAnalysis = evidence
            .extra
            .clone()
            .ok_or_else(|| OpsError::InvalidPlan("缺少字段治理扩展证据".into()))
            .and_then(|value| {
                serde_json::from_value(value)
                    .map_err(|err| OpsError::InvalidPlan(format!("扩展证据解析失败: {err}")))
            })?;

        let findings = analysis
            .evidence
            .iter()
            .map(|item| Finding {
                id: format!("evidence:{}", item.id),
                title: format!("{}（{}）", item.title, item.id),
                detail: item.summary.clone(),
                severity: if item.status == "collected" {
                    Severity::Info
                } else {
                    Severity::Warn
                },
                evidence_refs: vec![item.id.clone()],
                confidence: if item.status == "collected" { 1.0 } else { 0.5 },
            })
            .collect();

        let risks: Vec<RiskFinding> = analysis
            .risks
            .iter()
            .map(|risk| RiskFinding {
                id: risk.id.clone(),
                title: format!("[{}] {}", risk.level.as_str().to_ascii_uppercase(), risk.title),
                detail: format!(
                    "{}（规则 {}；写步骤阻断={}；需用户确认={}）",
                    risk.detail, risk.code, risk.blocks_write, risk.requires_user_confirmation
                ),
                severity: match risk.level {
                    types::RiskLevel::Block => Severity::Critical,
                    types::RiskLevel::High => Severity::Warn,
                    types::RiskLevel::Info => Severity::Info,
                },
                blocking: risk.level == types::RiskLevel::Block,
            })
            .collect();

        let mut validation_checks: Vec<ValidationCheck> = analysis
            .dry_run
            .checks
            .iter()
            .map(|check| ValidationCheck {
                id: format!("{}:{}", check.script_id, check.check),
                title: format!("dry-run {}", check.check),
                status: match check.status.as_str() {
                    "pass" => ValidationStatus::Pass,
                    "fail" => ValidationStatus::Fail,
                    _ => ValidationStatus::Warn,
                },
                detail: check.detail.clone(),
            })
            .collect();
        validation_checks.push(ValidationCheck {
            id: "field-governance:no-write-actions".into(),
            title: "本计划不包含任何写动作".into(),
            status: if analysis.scripts.iter().all(|script| !script.executable) {
                ValidationStatus::Pass
            } else {
                ValidationStatus::Fail
            },
            detail: "S1 只生成脚本；ActionSpec 不新增写动作，DROP COLUMN 永不自动执行。".into(),
        });

        let verification_sql: Vec<String> = analysis
            .scripts
            .iter()
            .filter(|script| script.kind == "verify")
            .map(|script| script.sql.clone())
            .collect();

        let irreversible: Vec<&str> = analysis
            .scripts
            .iter()
            .filter(|script| script.irreversible)
            .map(|script| script.title.as_str())
            .collect();
        let mut code_advice = vec![
            "本轮只读：未执行建表、回填、校验或删列。".to_string(),
            "所有脚本 executable=false，由用户人工复制执行。".to_string(),
            "DROP COLUMN 永远不自动执行；执行前必须完成快照、最终校验与读路径切换。".to_string(),
        ];
        if !irreversible.is_empty() {
            code_advice.push(format!(
                "不可逆步骤（界面必须显式提示）：{}",
                irreversible.join("；")
            ));
        }

        let diagnosis = format!(
            "表 {}.{}：阻断 {} / 高风险 {} / 提示 {}；profile_mode={}，仅生成 dry-run 脚本。",
            analysis.db_name,
            analysis.table,
            analysis.risk_summary.block,
            analysis.risk_summary.high,
            analysis.risk_summary.info,
            analysis.profile_mode
        );
        let confidence = if analysis.risk_summary.undetermined_write_block {
            0.4
        } else if analysis.profile_mode == "full" {
            0.95
        } else {
            0.7
        };

        let plan = OpsPlan {
            plan_id: uuid::Uuid::new_v4().to_string(),
            scenario_id: "field-governance".into(),
            db_id: identity.db_id.clone(),
            db_name: identity.db_name.clone(),
            db_fingerprint: identity.db_fingerprint.clone(),
            evidence_hash: evidence.evidence_hash(),
            diagnosis,
            findings,
            actions: Vec::new(),
            verification_sql,
            risks,
            validation_checks,
            rollback: crate::ops::types::RollbackPlan {
                summary: "S1 不执行写操作；回滚脚本只在 evidence.extra.scripts 中展示。\
                           DROP TABLE / DROP COLUMN 已标注不可逆。"
                    .into(),
                actions: Vec::new(),
                automatic: false,
            },
            code_advice,
            rewrite_suggestions: Vec::new(),
            verification_explain_analyze: false,
            confidence,
            requires_approval: false,
            created_at: now(),
            plan_hash: String::new(),
            status: PlanStatus::PendingApproval,
            evidence_summary: Some(EvidenceSummary {
                table: analysis.table.clone(),
                column_count: analysis.analyzed_columns.len(),
                index_count: analysis.dependencies.indexes.len(),
                row_estimate: Some(analysis.row_count),
                explain_findings: vec![format!(
                    "E01-E14 已采集 {} 项；证据缺口 {} 项",
                    analysis.evidence.len(),
                    analysis.gaps.len()
                )],
            }),
            execution: None,
        };
        Ok(plan.seal())
    }
}

/// 供测试/集成测试直接调用：采集 + 规则判定 + 脚本生成。
pub async fn analyze(
    ctx: &OpsContext,
    input: &types::FieldGovernanceInput,
    policy: &OpsPolicy,
) -> Result<Collected, OpsError> {
    let input = input.clone().normalized();
    let mut collected = evidence::collect(ctx, &input, policy).await?;
    risk::evaluate(&mut collected.analysis, &input, policy);
    scripts::generate(&collected.snapshot, &mut collected.analysis, &input);
    Ok(collected)
}

fn parse_input(value: &Value) -> Result<types::FieldGovernanceInput, OpsError> {
    let input: types::FieldGovernanceInput = serde_json::from_value(value.clone())
        .map_err(|err| OpsError::Validation(format!("字段治理输入解析失败: {err}")))?;
    let input = input.normalized();
    if input.table.is_empty() {
        return Err(OpsError::Validation("源表不能为空".into()));
    }
    Ok(input)
}

#[cfg(test)]
mod tests {
    use super::types::*;
    use super::*;
    use crate::ops::types::TableSnapshot;
    use serde_json::json;

    fn evidence_with(analysis: &FieldGovernanceAnalysis) -> EvidencePack {
        EvidencePack {
            db_name: "app".into(),
            table: "orders".into(),
            snapshot: TableSnapshot::default(),
            stats: Default::default(),
            explain: Vec::new(),
            index_usage: Vec::new(),
            workload_sql: Vec::new(),
            collected_at: now(),
            notes: Vec::new(),
            perf: None,
            extra: Some(serde_json::to_value(analysis).unwrap()),
        }
    }

    fn analysis() -> FieldGovernanceAnalysis {
        let mut column = sample_column("payload", "json");
        column.json_invalid_rows = 2;
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
            evidence: vec![EvidenceItem {
                id: "E01".into(),
                title: "列定义".into(),
                status: "collected".into(),
                mode: "structural".into(),
                summary: "ok".into(),
                sql: Vec::new(),
                details: json!({}),
            }],
            gaps: Vec::new(),
            declarations: Vec::new(),
            declaration_hash: String::new(),
            declaration_evidence: Vec::new(),
            target_table: "orders_payload_ext".into(),
            target_exists: false,
            empty_string_policy: EmptyStringPolicy::Preserve,
            target_columns: Vec::new(),
            risks: vec![FieldRisk {
                id: "risk-1".into(),
                code: "E09_INVALID_JSON".into(),
                level: RiskLevel::Block,
                title: "非法 JSON".into(),
                detail: "2 行".into(),
                evidence_refs: vec!["E09".into()],
                blocks_write: true,
                requires_user_confirmation: true,
            }],
            risk_summary: RiskSummary {
                block: 1,
                high: 0,
                info: 0,
                undetermined_write_block: false,
                risk_floor: "block".into(),
            },
            scripts: vec![ManualScript {
                id: "retire-column".into(),
                title: "下线".into(),
                kind: "retire".into(),
                phase: "L4_retire".into(),
                sql: "ALTER TABLE app.orders DROP COLUMN payload;".into(),
                executable: false,
                irreversible: true,
                notes: Vec::new(),
            }],
            dry_run: DryRunReport {
                executed_against_database: false,
                passed: true,
                checks: Vec::new(),
                note: "dry-run".into(),
            },
        }
    }

    #[test]
    fn plan_has_no_actions_and_marks_block_and_irreversible() {
        let scenario = FieldGovernanceScenario::new();
        let identity = PlanIdentity {
            db_id: "db1".into(),
            db_name: "app".into(),
            db_fingerprint: "fp".into(),
            is_read_only: false,
        };
        let plan = scenario
            .build_plan(
                &identity,
                &evidence_with(&analysis()),
                &json!({}),
                &OpsPolicy::default(),
            )
            .unwrap();
        assert!(plan.actions.is_empty());
        assert!(plan
            .risks
            .iter()
            .any(|risk| risk.blocking && risk.severity == Severity::Critical));
        assert!(plan
            .code_advice
            .iter()
            .any(|advice| advice.contains("不可逆步骤")));
        assert!(!plan.plan_hash.is_empty());
    }

    #[test]
    fn descriptor_is_enabled_read_only_without_actions() {
        let scenario = FieldGovernanceScenario::new();
        let descriptor = scenario.descriptor();
        assert_eq!(descriptor.id, "field-governance");
        assert_eq!(descriptor.status, ScenarioStatus::Enabled);
        assert_eq!(descriptor.risk_ceiling, crate::ops::types::RiskClass::L0);
        assert!(descriptor.allowed_actions.is_empty());
    }

    #[test]
    fn parse_input_requires_table() {
        assert!(parse_input(&json!({"table": ""})).is_err());
        let parsed = parse_input(&json!({"table": " orders ", "column": " payload "})).unwrap();
        assert_eq!(parsed.table, "orders");
        assert_eq!(parsed.column.as_deref(), Some("payload"));
    }
}

