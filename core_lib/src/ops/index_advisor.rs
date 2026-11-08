//! MySQL 单表索引优化场景：证据采集 + 确定性候选生成 + 统一计划。
//!
//! 首期只做「新增普通索引」。候选由 EXPLAIN + 现有索引 + 表结构确定性推导，
//! 结构化动作再交给 `ddl_compiler` 生成并校验 DDL；不依赖模型也能闭环。

use serde::{Deserialize, Serialize};
use sqlparser::dialect::MySqlDialect;
use sqlparser::parser::Parser;
use sqlx::{Column, Row};
use std::collections::{HashMap, HashSet};

use crate::ops::error::OpsError;
use crate::ops::registry::{
    OpsContext, OpsScenario, PlanIdentity, ScenarioDescriptor, ScenarioStatus,
};
use crate::ops::types::{
    ActionSpec, ColumnSnapshot, EvidencePack, EvidenceSummary, ExplainSample, Finding,
    IndexColumn, IndexDefinition, IndexUsage, OpsPlan, PlanStatus, RiskClass, RiskFinding,
    RollbackPlan, Severity, TableSnapshot, TableStats, ValidationCheck, ValidationStatus,
};
use crate::sql::util::{escape_sql_string, quote_ident_mysql, validate_identifier};

const MAX_WORKLOAD_SQL: usize = 10;
const MAX_INDEX_COLUMNS: usize = 3;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IndexAdvisorInput {
    pub table: String,
    #[serde(default)]
    pub workload_sql: Vec<String>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub use_llm: bool,
}

impl IndexAdvisorInput {
    fn normalized(mut self) -> Self {
        self.table = self.table.trim().to_string();
        self.workload_sql = self
            .workload_sql
            .into_iter()
            .map(|s| s.trim().trim_end_matches(';').trim().to_string())
            .filter(|s| !s.is_empty())
            .take(MAX_WORKLOAD_SQL)
            .collect();
        self.notes = self
            .notes
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        self
    }
}

pub struct IndexAdvisorScenario {
    descriptor: ScenarioDescriptor,
}

impl IndexAdvisorScenario {
    pub fn new() -> Self {
        Self {
            descriptor: ScenarioDescriptor {
                id: "mysql-index-advisor".into(),
                title: "MySQL 单表索引优化".into(),
                description:
                    "收集 schema/索引/EXPLAIN 证据，生成新增索引计划；审批后在线执行并复测，可回滚。"
                        .into(),
                engines: vec!["mysql".into(), "mariadb".into()],
                status: ScenarioStatus::Enabled,
                risk_ceiling: RiskClass::L1,
                allowed_actions: vec!["create_index".into()],
            },
        }
    }
}

impl OpsScenario for IndexAdvisorScenario {
    fn descriptor(&self) -> &ScenarioDescriptor {
        &self.descriptor
    }

    fn collect_evidence<'a>(
        &'a self,
        ctx: &'a OpsContext,
        input: &'a serde_json::Value,
    ) -> crate::db::protocol::BoxFuture<'a, Result<EvidencePack, OpsError>> {
        Box::pin(async move {
            let input: IndexAdvisorInput = serde_json::from_value(input.clone())?;
            collect_evidence(ctx, input.normalized()).await
        })
    }

    fn build_plan(
        &self,
        identity: &PlanIdentity,
        evidence: &EvidencePack,
        input: &serde_json::Value,
        policy: &crate::ops::policy::OpsPolicy,
    ) -> Result<OpsPlan, OpsError> {
        let input: IndexAdvisorInput = serde_json::from_value(input.clone())?;
        build_plan(identity, evidence, input.normalized(), policy)
    }
}

// ── 证据采集 ───────────────────────────────────────────────

pub async fn collect_evidence(
    ctx: &OpsContext,
    input: IndexAdvisorInput,
) -> Result<EvidencePack, OpsError> {
    if !ctx.is_mysql_family() {
        return Err(OpsError::Validation(
            "首期仅支持 MySQL/MariaDB；PG/SQLite 仍为只读实验状态".into(),
        ));
    }
    validate_identifier(&input.table)
        .map_err(|e| OpsError::Validation(format!("表名非法: {e}")))?;
    if ctx.db_name.trim().is_empty() {
        return Err(OpsError::Validation("当前连接缺少数据库名".into()));
    }
    validate_identifier(&ctx.db_name)
        .map_err(|e| OpsError::Validation(format!("数据库名非法: {e}")))?;

    let mut notes = Vec::new();
    if let Some(note) = input.notes.clone() {
        notes.push(note);
    }

    let snapshot = load_snapshot(ctx, &input.table).await?;
    if snapshot.columns.is_empty() {
        return Err(OpsError::NotFound(format!(
            "表 {}.{} 不存在或没有可见列",
            ctx.db_name, input.table
        )));
    }
    let stats = load_table_stats(ctx, &input.table).await?;

    let mut explain = Vec::new();
    for (idx, sql) in input.workload_sql.iter().enumerate() {
        ensure_explainable_query(sql)?;
        let label = format!("workload-{}", idx + 1);
        match explain_sql(ctx, &label, sql).await {
            Ok(Some(sample)) => explain.push(sample),
            Ok(None) => notes.push(format!("{label} 的 EXPLAIN 无返回行")),
            Err(err) => notes.push(format!("{label} 的 EXPLAIN 失败: {err}")),
        }
    }
    if input.workload_sql.is_empty() {
        notes.push("未提供 workload SQL，只能基于表结构给出诊断，不会生成索引候选".into());
    }

    let index_usage = match load_index_usage(ctx, &input.table).await {
        Ok(usage) => usage,
        Err(err) => {
            notes.push(format!("读取 performance_schema 索引使用统计失败（可忽略）: {err}"));
            vec![]
        }
    };

    Ok(EvidencePack {
        db_name: ctx.db_name.clone(),
        table: input.table.clone(),
        snapshot,
        stats,
        explain,
        index_usage,
        workload_sql: input.workload_sql,
        collected_at: crate::ops::types::now(),
        notes,
    })
}

fn ensure_explainable_query(sql: &str) -> Result<(), OpsError> {
    if sql.len() > 5000 {
        return Err(OpsError::Validation("workload SQL 超过 5000 字符".into()));
    }
    let statements = Parser::parse_sql(&MySqlDialect {}, sql)
        .map_err(|e| OpsError::Validation(format!("workload SQL 解析失败: {e}")))?;
    if statements.len() != 1 {
        return Err(OpsError::Validation(
            "workload 样本必须是单条只读查询".into(),
        ));
    }
    if !crate::sql::util::is_read_only_statement(&statements[0]) {
        return Err(OpsError::Validation(
            "workload 样本必须是只读查询（EXPLAIN 只接受 SELECT）".into(),
        ));
    }
    Ok(())
}

pub(crate) async fn load_snapshot_public(
    ctx: &OpsContext,
    table: &str,
) -> Result<TableSnapshot, OpsError> {
    load_snapshot(ctx, table).await
}

pub(crate) async fn load_table_stats_public(
    ctx: &OpsContext,
    table: &str,
) -> Result<TableStats, OpsError> {
    load_table_stats(ctx, table).await
}

pub(crate) async fn explain_sample_public(
    ctx: &OpsContext,
    label: &str,
    sql: &str,
) -> Result<Option<ExplainSample>, OpsError> {
    explain_sql(ctx, label, sql).await
}

async fn load_snapshot(ctx: &OpsContext, table: &str) -> Result<TableSnapshot, OpsError> {
    let db = escape_sql_string(&ctx.db_name);
    let tbl = escape_sql_string(table);

    let columns_sql = format!(
        "SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_KEY, EXTRA, CHARACTER_SET_NAME, COLLATION_NAME \
         FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = '{db}' AND TABLE_NAME = '{tbl}' ORDER BY ORDINAL_POSITION"
    );
    let rows = fetch_rows(ctx, &columns_sql).await?;
    let columns: Vec<ColumnSnapshot> = rows
        .iter()
        .map(|row| ColumnSnapshot {
            name: str_value(row, "column_name").unwrap_or_default(),
            column_type: str_value(row, "column_type").unwrap_or_default(),
            nullable: str_value(row, "is_nullable")
                .map(|v| v.eq_ignore_ascii_case("YES"))
                .unwrap_or(true),
            column_key: str_value(row, "column_key"),
            extra: str_value(row, "extra"),
            character_set: str_value(row, "character_set_name"),
            collation: str_value(row, "collation_name"),
        })
        .filter(|c| !c.name.is_empty())
        .collect();

    let indexes_sql = format!(
        "SELECT INDEX_NAME, NON_UNIQUE, SEQ_IN_INDEX, COLUMN_NAME, CARDINALITY, INDEX_TYPE \
         FROM information_schema.STATISTICS WHERE TABLE_SCHEMA = '{db}' AND TABLE_NAME = '{tbl}' \
         ORDER BY INDEX_NAME, SEQ_IN_INDEX"
    );
    let index_rows = fetch_rows(ctx, &indexes_sql).await?;
    let mut grouped: HashMap<String, IndexDefinition> = HashMap::new();
    for row in &index_rows {
        let Some(name) = str_value(row, "index_name") else {
            continue;
        };
        let entry = grouped.entry(name.clone()).or_insert_with(|| IndexDefinition {
            name: name.clone(),
            unique: false,
            index_type: str_value(row, "index_type").unwrap_or_else(|| "BTREE".into()),
            columns: vec![],
        });
        entry.unique = int_value(row, "non_unique")
            .map(|v| v == 0)
            .unwrap_or(false);
        entry.columns.push(IndexColumn {
            name: str_value(row, "column_name").unwrap_or_default(),
            seq: int_value(row, "seq_in_index").unwrap_or(0),
            cardinality: int_value(row, "cardinality"),
        });
    }
    let mut indexes: Vec<IndexDefinition> = grouped.into_values().collect();
    indexes.sort_by(|a, b| a.name.cmp(&b.name));
    for index in &mut indexes {
        index.columns.sort_by_key(|c| c.seq);
    }

    let show_sql = format!(
        "SHOW CREATE TABLE {}.{}",
        quote_ident_mysql(&ctx.db_name),
        quote_ident_mysql(table)
    );
    let create_table_sql = match fetch_rows(ctx, &show_sql).await {
        Ok(rows) => rows.first().and_then(|row| row.get("create table").cloned()),
        Err(_) => None,
    };

    Ok(TableSnapshot {
        db_name: ctx.db_name.clone(),
        table_name: table.to_string(),
        columns,
        indexes,
        create_table_sql,
    })
}

async fn load_table_stats(ctx: &OpsContext, table: &str) -> Result<TableStats, OpsError> {
    let db = escape_sql_string(&ctx.db_name);
    let tbl = escape_sql_string(table);
    let sql = format!(
        "SELECT ENGINE, TABLE_ROWS, DATA_LENGTH, INDEX_LENGTH, DATA_FREE, AUTO_INCREMENT, TABLE_COLLATION \
         FROM information_schema.TABLES WHERE TABLE_SCHEMA = '{db}' AND TABLE_NAME = '{tbl}'"
    );
    let rows = fetch_rows(ctx, &sql).await?;
    let Some(row) = rows.first() else {
        return Ok(TableStats::default());
    };
    Ok(TableStats {
        engine: str_value(row, "engine"),
        table_rows: uint_value(row, "table_rows"),
        data_length: uint_value(row, "data_length"),
        index_length: uint_value(row, "index_length"),
        data_free: uint_value(row, "data_free"),
        auto_increment: uint_value(row, "auto_increment"),
        collation: str_value(row, "table_collation"),
    })
}

async fn explain_sql(
    ctx: &OpsContext,
    label: &str,
    sql: &str,
) -> Result<Option<ExplainSample>, OpsError> {
    let explain = format!("EXPLAIN {sql}");
    let rows = fetch_rows(ctx, &explain).await?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    Ok(Some(ExplainSample {
        label: label.to_string(),
        sql: sql.to_string(),
        access_type: str_value(row, "type"),
        key: str_value(row, "key"),
        possible_keys: str_value(row, "possible_keys"),
        rows_examined: int_value(row, "rows"),
        filtered: float_value(row, "filtered"),
        extra: str_value(row, "extra"),
    }))
}

async fn load_index_usage(ctx: &OpsContext, table: &str) -> Result<Vec<IndexUsage>, OpsError> {
    let db = escape_sql_string(&ctx.db_name);
    let tbl = escape_sql_string(table);
    let sql = format!(
        "SELECT INDEX_NAME, COUNT_STAR, COUNT_READ FROM performance_schema.table_io_waits_summary_by_index_usage \
         WHERE OBJECT_SCHEMA = '{db}' AND OBJECT_NAME = '{tbl}'"
    );
    let rows = fetch_rows(ctx, &sql).await?;
    Ok(rows
        .iter()
        .filter_map(|row| {
            Some(IndexUsage {
                index_name: str_value(row, "index_name")?,
                count_star: uint_value(row, "count_star"),
                count_read: uint_value(row, "count_read"),
            })
        })
        .collect())
}

// ── 计划生成 ───────────────────────────────────────────────

pub fn build_plan(
    identity: &PlanIdentity,
    evidence: &EvidencePack,
    input: IndexAdvisorInput,
    policy: &crate::ops::policy::OpsPolicy,
) -> Result<OpsPlan, OpsError> {
    let _ = input;
    let snapshot = &evidence.snapshot;
    let mut findings = Vec::new();
    let mut risks = Vec::new();
    let mut checks = Vec::new();
    let mut actions = Vec::new();

    findings.push(Finding {
        id: "schema-snapshot".into(),
        title: format!(
            "表 {} 共 {} 列 / {} 个索引",
            snapshot.table_name,
            snapshot.columns.len(),
            snapshot.indexes.len()
        ),
        detail: snapshot
            .indexes
            .iter()
            .map(|i| format!("{} ({})", i.name, i.column_names().join(",")))
            .collect::<Vec<_>>()
            .join("; "),
        severity: Severity::Info,
        evidence_refs: vec!["schema:information_schema.COLUMNS".into(), "schema:information_schema.STATISTICS".into()],
        confidence: 1.0,
    });

    for sample in &evidence.explain {
        if sample.needs_index() {
            findings.push(Finding {
                id: format!("explain-{}", sample.label),
                title: format!("{} 存在索引缺口", sample.label),
                detail: sample.summary(),
                severity: Severity::Warn,
                evidence_refs: vec![format!("explain:{}", sample.label)],
                confidence: 0.75,
            });
        }
    }

    let existing_names: HashSet<String> = snapshot
        .indexes
        .iter()
        .map(|i| i.name.to_ascii_lowercase())
        .collect();
    let mut chosen: HashSet<String> = existing_names.clone();
    let mut candidate_keys: HashMap<String, Vec<String>> = HashMap::new();
    for sample in &evidence.explain {
        let cols = extract_candidate_columns(&sample.sql, snapshot);
        if cols.is_empty() {
            continue;
        }
        let key = cols.join(",").to_ascii_lowercase();
        candidate_keys.entry(key).or_insert(cols);
    }

    for (key, columns) in candidate_keys {
        if actions.len() >= 3 {
            findings.push(Finding {
                id: format!("candidate-limit-{key}"),
                title: "候选索引数量达到上限".into(),
                detail: "首期最多生成 3 个候选，其余候选未纳入本计划".into(),
                severity: Severity::Info,
                evidence_refs: vec![],
                confidence: 0.6,
            });
            break;
        }
        if let Some(existing) = snapshot
            .indexes
            .iter()
            .find(|index| index.covers_prefix(&columns))
        {
            findings.push(Finding {
                id: format!("covered-{key}"),
                title: format!("候选列 {} 已被现有索引覆盖", columns.join(",")),
                detail: format!(
                    "现有索引 {} ({}) 的前缀已覆盖该候选，不再重复创建",
                    existing.name,
                    existing.column_names().join(",")
                ),
                severity: Severity::Info,
                evidence_refs: vec!["schema:information_schema.STATISTICS".into()],
                confidence: 1.0,
            });
            continue;
        }

        let name = make_index_name(&snapshot.table_name, &columns, &chosen);
        chosen.insert(name.to_ascii_lowercase());
        let action = ActionSpec::CreateIndex {
            table: snapshot.table_name.clone(),
            name: name.clone(),
            columns: columns.clone(),
            unique: false,
            using: Some("BTREE".into()),
        };
        match crate::ops::ddl_compiler::compile_action(&action, snapshot) {
            Ok(compiled) => {
                checks.push(ValidationCheck {
                    id: format!("compiled-{name}"),
                    title: format!("候选索引 {name} 通过编译校验"),
                    status: ValidationStatus::Pass,
                    detail: compiled.sql,
                });
                findings.push(Finding {
                    id: format!("candidate-{name}"),
                    title: format!("建议新增索引 {name}"),
                    detail: format!(
                        "基于 EXPLAIN 与工作负载推导，候选列：{}",
                        columns.join(", ")
                    ),
                    severity: Severity::Info,
                    evidence_refs: vec!["explain:workload".into()],
                    confidence: 0.7,
                });
                actions.push(action);
            }
            Err(err) => {
                checks.push(ValidationCheck {
                    id: format!("compile-failed-{name}"),
                    title: format!("候选索引 {name} 未通过编译校验"),
                    status: ValidationStatus::Fail,
                    detail: err.to_string(),
                });
            }
        }
    }

    if actions.is_empty() {
        findings.push(Finding {
            id: "no-candidate".into(),
            title: "未生成可执行索引候选".into(),
            detail: "请提供真实慢查询/工作负载 SQL，或确认现有索引已覆盖查询条件".into(),
            severity: Severity::Warn,
            evidence_refs: vec![],
            confidence: 0.9,
        });
    }

    checks.push(ValidationCheck {
        id: "action-whitelist".into(),
        title: "动作白名单".into(),
        status: ValidationStatus::Pass,
        detail: "计划只包含 create_index（L1），不含 L4 动作或原始 SQL".into(),
    });
    checks.push(ValidationCheck {
        id: "schema-exists".into(),
        title: "表与列存在性".into(),
        status: ValidationStatus::Pass,
        detail: format!(
            "{} 列在 {}.{} 中可见",
            snapshot.columns.len(),
            snapshot.db_name,
            snapshot.table_name
        ),
    });

    if let Some(rows) = evidence.stats.table_rows {
        let notes = vec![
            "使用 ALGORITHM=INPLACE, LOCK=NONE，尽量在线建索引；失败会中止并尝试补偿".to_string(),
            "索引会增加写入放大与存储占用".to_string(),
        ];
        risks.push(RiskFinding {
            id: "ddl-lock".into(),
            title: "DDL 锁与元数据锁风险".into(),
            detail: "在线 DDL 仍可能受元数据锁影响；执行前需确认无长事务。".into(),
            severity: Severity::Warn,
            blocking: false,
        });
        risks.push(RiskFinding {
            id: "index-cost".into(),
            title: "存储与写入成本".into(),
            detail: format!(
                "表约 {} 行，索引会增加存储并影响写入；执行前会做影响评估。",
                rows
            ),
            severity: Severity::Info,
            blocking: false,
        });
        if rows > policy.max_affected_rows && !policy.allow_large_table {
            risks.push(RiskFinding {
                id: "large-table".into(),
                title: "表规模超过策略上限".into(),
                detail: format!(
                    "估算 {} 行 > max_affected_rows {}；当前策略禁止落地",
                    rows, policy.max_affected_rows
                ),
                severity: Severity::Critical,
                blocking: true,
            });
        }
        findings.push(Finding {
            id: "impact".into(),
            title: "影响评估".into(),
            detail: notes.join("; "),
            severity: Severity::Info,
            evidence_refs: vec!["stats:information_schema.TABLES".into()],
            confidence: 0.8,
        });
    }

    // 首期不生成 DROP INDEX 之外的任何回滚；回滚动作由服务端固定生成。
    let rollback_actions: Vec<ActionSpec> = actions
        .iter()
        .filter_map(|action| match action {
            ActionSpec::CreateIndex { table, name, columns, .. } => Some(ActionSpec::DropIndex {
                table: table.clone(),
                name: name.clone(),
                expected_definition: Some(format!("INDEX ({})", columns.join(","))),
            }),
            _ => None,
        })
        .collect();

    let db_id = identity.db_id.clone();
    let confidence = if actions.is_empty() {
        0.4
    } else {
        (0.6 + actions.len() as f32 * 0.1).min(0.9)
    };
    let diagnosis = if actions.is_empty() {
        "未发现明确的索引缺口，或缺少可用的工作负载 SQL。".to_string()
    } else {
        format!(
            "检测到 {} 个索引缺口，建议新增 {} 个普通索引。",
            evidence.explain.iter().filter(|e| e.needs_index()).count(),
            actions.len()
        )
    };

    let plan = OpsPlan {
        plan_id: uuid::Uuid::new_v4().to_string(),
        scenario_id: "mysql-index-advisor".into(),
        db_id,
        db_name: identity.db_name.clone(),
        db_fingerprint: identity.db_fingerprint.clone(),
        evidence_hash: evidence.evidence_hash(),
        diagnosis,
        findings,
        verification_sql: evidence.workload_sql.clone(),
        actions: actions.clone(),
        risks,
        validation_checks: checks,
        rollback: RollbackPlan {
            summary: "回滚将删除本次新增索引；不会触碰已有索引。".into(),
            actions: rollback_actions,
            automatic: true,
        },
        code_advice: vec![
            "确认查询条件顺序与索引列顺序一致；范围条件后的列通常不应继续放入索引".to_string(),
            "上线后观察 1-2 个业务周期，再决定是否保留该索引".to_string(),
        ],
        confidence,
        requires_approval: !actions.is_empty(),
        created_at: crate::ops::types::now(),
        plan_hash: String::new(),
        status: PlanStatus::PendingApproval,
        evidence_summary: Some(EvidenceSummary {
            table: snapshot.table_name.clone(),
            column_count: snapshot.columns.len(),
            index_count: snapshot.indexes.len(),
            row_estimate: evidence.stats.table_rows,
            explain_findings: evidence.explain.iter().map(ExplainSample::summary).collect(),
        }),
        execution: None,
    };
    Ok(plan.seal())
}

fn extract_candidate_columns(sql: &str, snapshot: &TableSnapshot) -> Vec<String> {
    let padded = format!(" {} ", sql.to_ascii_lowercase());
    let mut columns: Vec<String> = Vec::new();

    for (start, stops) in [
        (
            " where ",
            vec![" order by ", " group by ", " limit ", " having ", " union ", " ;"],
        ),
        (" order by ", vec![" limit ", " ;", " union "]),
        (" group by ", vec![" having ", " order by ", " limit ", " ;"]),
    ] {
        let Some(pos) = padded.find(start) else {
            continue;
        };
        let slice_start = pos + start.len();
        let mut slice_end = padded.len();
        for stop in stops {
            if let Some(rel) = padded[slice_start..].find(stop) {
                slice_end = slice_end.min(slice_start + rel);
            }
        }
        let slice = &padded[slice_start..slice_end];
        for token in slice.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '$')) {
            if token.is_empty() {
                continue;
            }
            if let Some(column) = snapshot
                .columns
                .iter()
                .find(|c| c.name.eq_ignore_ascii_case(token))
            {
                if !columns
                    .iter()
                    .any(|c| c.eq_ignore_ascii_case(&column.name))
                {
                    columns.push(column.name.clone());
                }
            }
        }
    }

    columns.truncate(MAX_INDEX_COLUMNS);
    columns
}

fn make_index_name(table: &str, columns: &[String], existing: &HashSet<String>) -> String {
    let mut base = format!("idx_{}_{}", table, columns.join("_")).to_ascii_lowercase();
    base = base
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .collect();
    if base.len() > 60 {
        base.truncate(60);
    }
    let mut candidate = base.clone();
    let mut suffix = 2;
    while existing.contains(&candidate.to_ascii_lowercase()) {
        candidate = format!("{}_{}", base, suffix);
        suffix += 1;
    }
    candidate
}

/// 直接读取 MySQL 行并统一转成字符串映射。
///
/// 不能走 `DatabaseOps::execute_query`：其 MySQL 适配器用
/// `try_get::<serde_json::Value>` 解码每一列，普通 VARCHAR/BIGINT 列会解码失败
/// 并静默变成 `Null`，导致证据采集拿不到 schema/EXPLAIN 字段。
async fn fetch_rows(
    ctx: &OpsContext,
    sql: &str,
) -> Result<Vec<HashMap<String, String>>, OpsError> {
    let pool = ctx.db_client.mysql_pool()?;
    let rows = sqlx::query(sql).fetch_all(pool).await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows.iter() {
        let count = row.columns().len();
        let mut map = HashMap::with_capacity(count);
        for index in 0..count {
            let key = row.column(index).name().to_ascii_lowercase();
            if let Some(value) = mysql_cell_to_text(row, index) {
                map.insert(key, value);
            }
        }
        out.push(map);
    }
    Ok(out)
}

fn mysql_cell_to_text(row: &sqlx::mysql::MySqlRow, index: usize) -> Option<String> {
    match row.try_get::<Option<String>, _>(index) {
        Ok(Some(value)) => return Some(value),
        Ok(None) => return None,
        Err(_) => {}
    }
    // information_schema 的 COLUMN_TYPE / SHOW CREATE TABLE 等列是 TEXT 族，
    // 某些驱动元数据带 BINARY 标志，String 解码会被拒；按字节兜底。
    if let Ok(Some(value)) = row.try_get::<Option<&[u8]>, _>(index) {
        return Some(String::from_utf8_lossy(value).into_owned());
    }
    if let Ok(Some(value)) = row.try_get::<Option<i64>, _>(index) {
        return Some(value.to_string());
    }
    if let Ok(Some(value)) = row.try_get::<Option<u64>, _>(index) {
        return Some(value.to_string());
    }
    if let Ok(Some(value)) = row.try_get::<Option<f64>, _>(index) {
        return Some(value.to_string());
    }
    None
}

fn str_value(row: &HashMap<String, String>, key: &str) -> Option<String> {
    row.get(key)
        .map(|v| v.trim())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn int_value(row: &HashMap<String, String>, key: &str) -> Option<i64> {
    row.get(key).and_then(|v| v.trim().parse::<i64>().ok())
}

fn uint_value(row: &HashMap<String, String>, key: &str) -> Option<u64> {
    row.get(key).and_then(|v| v.trim().parse::<u64>().ok())
}

fn float_value(row: &HashMap<String, String>, key: &str) -> Option<f64> {
    row.get(key).and_then(|v| v.trim().parse::<f64>().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> TableSnapshot {
        TableSnapshot {
            db_name: "app".into(),
            table_name: "orders".into(),
            columns: vec![
                ColumnSnapshot {
                    name: "user_id".into(),
                    column_type: "bigint".into(),
                    nullable: false,
                    ..Default::default()
                },
                ColumnSnapshot {
                    name: "status".into(),
                    column_type: "varchar(32)".into(),
                    nullable: false,
                    ..Default::default()
                },
                ColumnSnapshot {
                    name: "created_at".into(),
                    column_type: "datetime".into(),
                    nullable: false,
                    ..Default::default()
                },
            ],
            indexes: vec![IndexDefinition {
                name: "PRIMARY".into(),
                unique: true,
                index_type: "BTREE".into(),
                columns: vec![IndexColumn {
                    name: "id".into(),
                    seq: 1,
                    cardinality: None,
                }],
            }],
            create_table_sql: None,
        }
    }

    #[test]
    fn extracts_candidates_from_three_explain_samples() {
        let cases = [
            ("SELECT * FROM orders WHERE user_id = 1 AND status = 'paid' ORDER BY created_at DESC", vec!["user_id", "status", "created_at"]),
            ("SELECT * FROM orders WHERE status = 'paid' ORDER BY created_at", vec!["status", "created_at"]),
            ("SELECT * FROM orders WHERE created_at >= '2026-01-01' ORDER BY user_id", vec!["created_at", "user_id"]),
        ];
        for (sql, expected) in cases {
            let cols = extract_candidate_columns(sql, &snapshot());
            assert_eq!(cols, expected.iter().map(|s| s.to_string()).collect::<Vec<_>>(), "sql={sql}");
        }
    }

    #[test]
    fn plan_generation_produces_valid_candidates_for_three_samples() {
        let evidence = EvidencePack {
            db_name: "app".into(),
            table: "orders".into(),
            snapshot: snapshot(),
            stats: TableStats {
                table_rows: Some(1000),
                ..Default::default()
            },
            explain: vec![
                ExplainSample {
                    label: "workload-1".into(),
                    sql: "SELECT * FROM orders WHERE user_id = 1 AND status = 'paid' ORDER BY created_at DESC".into(),
                    access_type: Some("ALL".into()),
                    key: None,
                    possible_keys: None,
                    rows_examined: Some(1000),
                    filtered: None,
                    extra: Some("Using where; Using filesort".into()),
                },
                ExplainSample {
                    label: "workload-2".into(),
                    sql: "SELECT * FROM orders WHERE status = 'paid' ORDER BY created_at".into(),
                    access_type: Some("ALL".into()),
                    key: None,
                    possible_keys: None,
                    rows_examined: Some(1000),
                    filtered: None,
                    extra: Some("Using where".into()),
                },
                ExplainSample {
                    label: "workload-3".into(),
                    sql: "SELECT * FROM orders WHERE created_at >= '2026-01-01' ORDER BY user_id".into(),
                    access_type: Some("ALL".into()),
                    key: None,
                    possible_keys: None,
                    rows_examined: Some(1000),
                    filtered: None,
                    extra: Some("Using where".into()),
                },
            ],
            workload_sql: vec![
                "SELECT * FROM orders WHERE user_id = 1 AND status = 'paid' ORDER BY created_at DESC".into(),
                "SELECT * FROM orders WHERE status = 'paid' ORDER BY created_at".into(),
                "SELECT * FROM orders WHERE created_at >= '2026-01-01' ORDER BY user_id".into(),
            ],
            ..Default::default()
        };
        let identity = PlanIdentity {
            db_id: "db1".into(),
            db_name: "app".into(),
            db_fingerprint: "fp".into(),
            is_read_only: false,
        };
        let plan = build_plan(
            &identity,
            &evidence,
            IndexAdvisorInput {
                table: "orders".into(),
                ..Default::default()
            },
            &crate::ops::policy::OpsPolicy::default(),
        )
        .unwrap();
        assert!(!plan.actions.is_empty(), "应生成候选动作");
        assert!(plan.actions.len() <= 3);
        assert_eq!(plan.plan_hash, plan.clone().compute_hash());
        assert!(!plan.rollback.actions.is_empty());
        for action in &plan.actions {
            let compiled = crate::ops::ddl_compiler::compile_action(action, &evidence.snapshot)
                .expect("候选动作必须通过编译校验");
            assert!(compiled.sql.contains("ALGORITHM=INPLACE, LOCK=NONE"));
            assert!(!compiled.sql.contains("DROP COLUMN"));
        }
    }

    #[test]
    fn candidate_generation_skips_existing_prefix_and_respects_limit() {
        let mut snap = snapshot();
        snap.indexes.push(IndexDefinition {
            name: "idx_orders_user_status".into(),
            unique: false,
            index_type: "BTREE".into(),
            columns: vec![
                IndexColumn { name: "user_id".into(), seq: 1, cardinality: None },
                IndexColumn { name: "status".into(), seq: 2, cardinality: None },
            ],
        });
        let evidence = EvidencePack {
            db_name: "app".into(),
            table: "orders".into(),
            snapshot: snap,
            stats: TableStats {
                table_rows: Some(1000),
                ..Default::default()
            },
            explain: vec![ExplainSample {
                label: "workload-1".into(),
                sql: "SELECT * FROM orders WHERE user_id = 1 AND status = 'paid'".into(),
                access_type: Some("ALL".into()),
                key: None,
                possible_keys: None,
                rows_examined: Some(1000),
                filtered: None,
                extra: Some("Using where".into()),
            }],
            ..Default::default()
        };
        let identity = PlanIdentity {
            db_id: "db1".into(),
            db_name: "app".into(),
            db_fingerprint: "fp".into(),
            is_read_only: false,
        };
        let plan = build_plan(
            &identity,
            &evidence,
            IndexAdvisorInput {
                table: "orders".into(),
                workload_sql: vec!["SELECT * FROM orders WHERE user_id = 1 AND status = 'paid'".into()],
                ..Default::default()
            },
            &crate::ops::policy::OpsPolicy::default(),
        )
        .unwrap();
        assert!(plan.actions.is_empty());
        assert!(plan.findings.iter().any(|f| f.id.starts_with("covered")));
    }
}
