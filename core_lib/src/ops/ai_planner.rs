//! 可选的模型辅助规划。
//!
//! 复用 ai/ 下已有的 `chat_completion_raw`（rig 循环与 provider 策略）和
//! `extractor::extract_code_block` 的容错解析，不新建模型调用栈。
//! 模型仍只能产出结构化 `ActionSpec`；服务端会逐条校验并重新编译，模型输出
//! 永远不会变成可直接执行的 SQL。

use serde::Deserialize;

use crate::ai::agent::chat_completion_raw;
use crate::ai::extractor::extract_code_block;
use crate::config::AppConfig;
use crate::ops::ddl_compiler;
use crate::ops::types::{
    ActionSpec, EvidencePack, Finding, OpsPlan, PlanStatus, RiskFinding, RollbackPlan, Severity,
    ValidationCheck, ValidationStatus,
};

#[derive(Debug, Deserialize)]
struct LlmFinding {
    title: String,
    detail: String,
    #[serde(default)]
    severity: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LlmRisk {
    title: String,
    detail: String,
    #[serde(default)]
    severity: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LlmDraft {
    #[serde(default)]
    diagnosis: Option<String>,
    #[serde(default)]
    findings: Vec<LlmFinding>,
    #[serde(default)]
    actions: Vec<ActionSpec>,
    #[serde(default)]
    risks: Vec<LlmRisk>,
    #[serde(default)]
    code_advice: Vec<String>,
}

/// 用模型增强诊断与候选动作。失败时原样返回确定性计划，绝不阻塞闭环。
pub async fn enhance_plan(config: &AppConfig, evidence: &EvidencePack, base: OpsPlan) -> OpsPlan {
    let prompt = build_prompt(evidence, &base);
    let response = match chat_completion_raw(
        config,
        "你是一名保守的 MySQL 索引优化专家。只输出严格 JSON，不输出 SQL，不使用 markdown。\
         只能提出 create_index 结构化动作；禁止 drop/truncate/类型变更等危险操作。",
        &prompt,
    )
    .await
    {
        Ok(text) => text,
        Err(err) => {
            tracing::warn!("ops AI 规划失败，回退确定性计划: {}", err);
            return base;
        }
    };

    let Some(mut draft) = parse_draft(&response) else {
        tracing::warn!("ops AI 输出无法解析为 JSON，回退确定性计划");
        return base;
    };

    draft.actions.retain(|action| matches!(action, ActionSpec::CreateIndex { .. }));
    if draft.actions.is_empty() {
        return base;
    }

    let mut accepted = Vec::new();
    let mut rejected = Vec::new();
    for action in &draft.actions {
        match ddl_compiler::compile_action(action, &evidence.snapshot) {
            Ok(_) => accepted.push(action.clone()),
            Err(err) => rejected.push(format!("{}: {}", action.action_id(), err)),
        }
    }
    if accepted.is_empty() {
        tracing::warn!("ops AI 候选全部未通过校验: {:?}", rejected);
        return base;
    }
    accepted.truncate(3);

    let mut plan = base;
    if let Some(diagnosis) = draft.diagnosis {
        if !diagnosis.trim().is_empty() {
            plan.diagnosis = diagnosis.trim().to_string();
        }
    }
    if !draft.findings.is_empty() {
        plan.findings = draft
            .findings
            .into_iter()
            .enumerate()
            .map(|(idx, f)| Finding {
                id: format!("ai-finding-{idx}"),
                title: f.title,
                detail: f.detail,
                severity: severity(&f.severity),
                evidence_refs: vec!["ai:review".into()],
                confidence: 0.7,
            })
            .collect();
    }
    plan.actions = accepted.clone();
    for risk in draft.risks {
        plan.risks.push(RiskFinding {
            id: format!("ai-risk-{}", plan.risks.len()),
            title: risk.title,
            detail: risk.detail,
            severity: severity(&risk.severity),
            blocking: false,
        });
    }
    if !draft.code_advice.is_empty() {
        plan.code_advice = draft.code_advice;
    }
    plan.rollback = RollbackPlan {
        summary: "回滚将删除本次新增索引；不会触碰已有索引。".into(),
        actions: accepted
            .iter()
            .filter_map(|action| match action {
                ActionSpec::CreateIndex { table, name, columns, .. } => {
                    Some(ActionSpec::DropIndex {
                        table: table.clone(),
                        name: name.clone(),
                        expected_definition: Some(format!("INDEX ({})", columns.join(","))),
                    })
                }
                _ => None,
            })
            .collect(),
        automatic: true,
    };
    plan.validation_checks.push(ValidationCheck {
        id: "ai-actions-compiled".into(),
        title: "模型候选通过服务端编译校验".into(),
        status: ValidationStatus::Pass,
        detail: format!("{} 个动作通过 ddl_compiler", plan.actions.len()),
    });
    for rejected_item in rejected {
        plan.validation_checks.push(ValidationCheck {
            id: format!("ai-rejected-{}", plan.validation_checks.len()),
            title: "模型候选被拒绝".into(),
            status: ValidationStatus::Warn,
            detail: rejected_item,
        });
    }
    plan.status = PlanStatus::PendingApproval;
    plan.requires_approval = true;
    plan.execution = None;
    plan.refresh_hash();
    plan
}

fn parse_draft(text: &str) -> Option<LlmDraft> {
    if let Ok(draft) = serde_json::from_str::<LlmDraft>(text.trim()) {
        return Some(draft);
    }
    let block = extract_code_block(text, "json");
    if let Ok(draft) = serde_json::from_str::<LlmDraft>(block.trim()) {
        return Some(draft);
    }
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end <= start {
        return None;
    }
    serde_json::from_str::<LlmDraft>(&text[start..=end]).ok()
}

fn severity(value: &Option<String>) -> Severity {
    match value.as_deref().unwrap_or("info").to_ascii_lowercase().as_str() {
        "critical" | "high" => Severity::Critical,
        "warn" | "warning" | "medium" => Severity::Warn,
        _ => Severity::Info,
    }
}

fn build_prompt(evidence: &EvidencePack, base: &OpsPlan) -> String {
    let columns = evidence
        .snapshot
        .columns
        .iter()
        .map(|c| format!("{} {}", c.name, c.column_type))
        .collect::<Vec<_>>()
        .join(", ");
    let indexes = evidence
        .snapshot
        .indexes
        .iter()
        .map(|i| format!("{} ({})", i.name, i.column_names().join(",")))
        .collect::<Vec<_>>()
        .join("; ");
    let explain = evidence
        .explain
        .iter()
        .map(|e| e.summary())
        .collect::<Vec<_>>()
        .join("\n");
    let workload = evidence.workload_sql.join("\n");

    format!(
        "数据库: {}\n表: {}\n列: {}\n现有索引: {}\n表行数: {:?}\n\nEXPLAIN:\n{}\n\n工作负载 SQL:\n{}\n\n\
         确定性计划诊断: {}\n候选动作: {}\n\n\
         请输出 JSON: {{\"diagnosis\":\"...\",\"findings\":[{{\"title\":\"\",\"detail\":\"\",\"severity\":\"info|warn|critical\"}}],\
         \"actions\":[{{\"type\":\"create_index\",\"table\":\"{}\",\"name\":\"idx_...\",\"columns\":[\"...\"],\"unique\":false}}],\
         \"risks\":[{{\"title\":\"\",\"detail\":\"\",\"severity\":\"info|warn\"}}],\"code_advice\":[\"...\"]}}",
        evidence.db_name,
        evidence.table,
        columns,
        indexes,
        evidence.stats.table_rows,
        explain,
        workload,
        base.diagnosis,
        serde_json::to_string(&base.actions).unwrap_or_default(),
        evidence.table
    )
}
