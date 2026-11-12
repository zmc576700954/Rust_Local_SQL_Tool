//! 单表索引优化场景：证据采集 + 确定性候选生成 + 统一计划。
//!
//! MySQL/MariaDB：可生成 `create_index` 动作，经审批后执行。
//! PostgreSQL：只做只读分析（EXPLAIN + pg_stat_user_indexes + pg_index），
//! 不生成可执行 DDL、不提供 apply；计划里显式标注只读。

use serde::{Deserialize, Serialize};
use sqlparser::dialect::{MySqlDialect, PostgreSqlDialect};
use sqlparser::parser::Parser;
use std::collections::{HashMap, HashSet};

use crate::ops::error::OpsError;
use crate::ops::mysql_rows::{fetch_rows, float_value, int_value, str_value, uint_value};
use crate::ops::pg_rows;
use crate::ops::registry::{
    OpsContext, OpsScenario, PlanIdentity, ScenarioDescriptor, ScenarioStatus,
};
use crate::ops::types::{
    ActionSpec, ColumnSnapshot, EvidencePack, EvidenceSummary, ExplainSample, Finding, IndexColumn,
    IndexDefinition, IndexUsage, OpsPlan, PlanStatus, RiskClass, RiskFinding, RollbackPlan,
    Severity, TableSnapshot, TableStats, ValidationCheck, ValidationStatus,
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
                title: "单表索引优化".into(),
                description:
                    "MySQL/MariaDB：收集 schema/索引/EXPLAIN 证据，生成新增索引计划，审批后执行并复测。\
                     PostgreSQL：仅只读分析，不生成可执行 DDL、不提供 apply。"
                        .into(),
                engines: vec!["mysql".into(), "mariadb".into(), "postgresql".into()],
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
    if !ctx.is_mysql_family() && !ctx.is_postgres() {
        return Err(OpsError::Validation(
            "索引优化仅支持 MySQL/MariaDB（可执行）与 PostgreSQL（只读）；SQLite 仍为实验状态"
                .into(),
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
        ensure_explainable_query(ctx, sql)?;
        let label = format!("workload-{}", idx + 1);
        match explain_sql(ctx, &label, sql).await {
            Ok(Some(sample)) => explain.push(sample),
            Ok(None) => notes.push(format!("{label} 的 EXPLAIN 无返回行")),
            Err(err) => notes.push(format!("{label} 的 EXPLAIN 失败: {err}")),
        }
    }
    if input.workload_sql.is_empty() {
        if ctx.is_postgres() {
            notes.push(
                "未提供 workload SQL：PG 只读分析只能展示表结构/索引使用情况，不会推导候选索引"
                    .into(),
            );
        } else {
            notes.push("未提供 workload SQL，只能基于表结构给出诊断，不会生成索引候选".into());
        }
    }

    let index_usage = match load_index_usage(ctx, &input.table).await {
        Ok(usage) => usage,
        Err(err) => {
            notes.push(format!("读取索引使用统计失败（可忽略）: {err}"));
            vec![]
        }
    };

    let extra = if ctx.is_postgres() {
        Some(serde_json::json!({
            "engine": "postgresql",
            "read_only": true,
            "read_only_reason": "PG 索引优化只读分析：不生成可执行 DDL，不提供 apply",
        }))
    } else {
        None
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
        perf: None,
        extra,
    })
}

fn ensure_explainable_query(ctx: &OpsContext, sql: &str) -> Result<(), OpsError> {
    if sql.len() > 5000 {
        return Err(OpsError::Validation("workload SQL 超过 5000 字符".into()));
    }
    let statements = if ctx.is_postgres() {
        Parser::parse_sql(&PostgreSqlDialect {}, sql)
    } else {
        Parser::parse_sql(&MySqlDialect {}, sql)
    }
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
    if ctx.is_postgres() {
        return load_snapshot_pg(ctx, table).await;
    }
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
        let entry = grouped
            .entry(name.clone())
            .or_insert_with(|| IndexDefinition {
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
        Ok(rows) => rows
            .first()
            .and_then(|row| row.get("create table").cloned()),
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
    if ctx.is_postgres() {
        return load_table_stats_pg(ctx, table).await;
    }
    let db = escape_sql_string(&ctx.db_name);
    let tbl = escape_sql_string(table);
    let sql = format!(
        "SELECT ENGINE, TABLE_ROWS, DATA_LENGTH, INDEX_LENGTH, DATA_FREE, AUTO_INCREMENT, TABLE_COLLATION, UPDATE_TIME \
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
        update_time: str_value(row, "update_time"),
    })
}

async fn explain_sql(
    ctx: &OpsContext,
    label: &str,
    sql: &str,
) -> Result<Option<ExplainSample>, OpsError> {
    if ctx.is_postgres() {
        return explain_sql_pg(ctx, label, sql).await;
    }
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
        ..Default::default()
    }))
}

async fn load_index_usage(ctx: &OpsContext, table: &str) -> Result<Vec<IndexUsage>, OpsError> {
    if ctx.is_postgres() {
        return load_index_usage_pg(ctx, table).await;
    }
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

// ── PostgreSQL 只读数据源 ───────────────────────────────────

async fn load_snapshot_pg(ctx: &OpsContext, table: &str) -> Result<TableSnapshot, OpsError> {
    let tbl = escape_sql_string(table);
    let columns_sql = format!(
        "SELECT a.attname::text AS column_name, \
                format_type(a.atttypid, a.atttypmod)::text AS column_type, \
                CASE WHEN a.attnotnull THEN 'NO' ELSE 'YES' END AS is_nullable, \
                CASE WHEN EXISTS (SELECT 1 FROM pg_index i \
                                  JOIN generate_subscripts(i.indkey, 1) AS k(pos) ON true \
                                  WHERE i.indrelid = c.oid AND i.indisprimary AND i.indkey[k.pos] = a.attnum) \
                     THEN 'PRI' ELSE '' END AS column_key, \
                '' AS extra, pg_encoding_to_char(d.encoding) AS character_set, \
                COALESCE(co.collname, '') AS collation \
         FROM pg_attribute a \
         JOIN pg_class c ON c.oid = a.attrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         CROSS JOIN pg_database d \
         LEFT JOIN pg_collation co ON co.oid = a.attcollation \
         WHERE n.nspname = current_schema() AND c.relname = '{tbl}' \
           AND a.attnum > 0 AND NOT a.attisdropped \
           AND d.datname = current_database() \
         ORDER BY a.attnum"
    );
    let rows = pg_rows::fetch_rows(ctx, &columns_sql).await?;
    let columns: Vec<ColumnSnapshot> = rows
        .iter()
        .map(|row| ColumnSnapshot {
            name: str_value(row, "column_name").unwrap_or_default(),
            column_type: str_value(row, "column_type").unwrap_or_default(),
            nullable: str_value(row, "is_nullable")
                .map(|v| v.eq_ignore_ascii_case("YES"))
                .unwrap_or(true),
            column_key: str_value(row, "column_key").filter(|value| !value.is_empty()),
            extra: str_value(row, "extra").filter(|value| !value.is_empty()),
            character_set: str_value(row, "character_set"),
            collation: str_value(row, "collation").filter(|value| !value.is_empty()),
        })
        .filter(|c| !c.name.is_empty())
        .collect();

    let indexes_sql = format!(
        "SELECT i.relname::text AS index_name, ix.indisunique::text AS is_unique, \
                k.ord::text AS seq_in_index, \
                pg_get_indexdef(ix.indexrelid, k.ord::int, true)::text AS column_name, \
                am.amname::text AS index_type \
         FROM pg_class c \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         JOIN pg_index ix ON ix.indrelid = c.oid \
         JOIN pg_class i ON i.oid = ix.indexrelid \
         JOIN pg_am am ON am.oid = i.relam \
         JOIN generate_series(1, ix.indnkeyatts) AS k(ord) ON true \
         WHERE n.nspname = current_schema() AND c.relname = '{tbl}' \
         ORDER BY i.relname, k.ord"
    );
    let index_rows = pg_rows::fetch_rows(ctx, &indexes_sql).await?;
    let mut grouped: HashMap<String, IndexDefinition> = HashMap::new();
    for row in &index_rows {
        let Some(name) = str_value(row, "index_name") else {
            continue;
        };
        let entry = grouped
            .entry(name.clone())
            .or_insert_with(|| IndexDefinition {
                name: name.clone(),
                unique: false,
                index_type: str_value(row, "index_type")
                    .unwrap_or_else(|| "btree".into())
                    .to_ascii_uppercase(),
                columns: vec![],
            });
        entry.unique = matches!(
            str_value(row, "is_unique").as_deref(),
            Some("true") | Some("t")
        );
        entry.columns.push(IndexColumn {
            name: str_value(row, "column_name").unwrap_or_default(),
            seq: int_value(row, "seq_in_index").unwrap_or(0),
            // PG 目录不提供单索引基数，保持 None，不造假数据。
            cardinality: None,
        });
    }
    let mut indexes: Vec<IndexDefinition> = grouped.into_values().collect();
    indexes.sort_by(|a, b| a.name.cmp(&b.name));
    for index in &mut indexes {
        index.columns.sort_by_key(|c| c.seq);
    }

    Ok(TableSnapshot {
        db_name: ctx.db_name.clone(),
        table_name: table.to_string(),
        columns,
        indexes,
        // PG 没有 SHOW CREATE TABLE 等价物，保持 None 而不是拼接近似 DDL。
        create_table_sql: None,
    })
}

async fn load_table_stats_pg(ctx: &OpsContext, table: &str) -> Result<TableStats, OpsError> {
    let tbl = escape_sql_string(table);
    let sql = format!(
        "SELECT 'heap' AS engine, GREATEST(c.reltuples, 0)::bigint::text AS table_rows, \
                pg_table_size(c.oid)::text AS data_length, \
                pg_indexes_size(c.oid)::text AS index_length, \
                '0' AS data_free, '' AS auto_increment, d.datcollate AS table_collation, \
                COALESCE(to_char(s.last_autovacuum, 'YYYY-MM-DD HH24:MI:SS'), '') AS update_time \
         FROM pg_class c \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         CROSS JOIN pg_database d \
         LEFT JOIN pg_stat_user_tables s ON s.relid = c.oid \
         WHERE n.nspname = current_schema() AND c.relname = '{tbl}' \
           AND d.datname = current_database()"
    );
    let rows = pg_rows::fetch_rows(ctx, &sql).await?;
    let Some(row) = rows.first() else {
        return Ok(TableStats::default());
    };
    Ok(TableStats {
        engine: str_value(row, "engine"),
        table_rows: uint_value(row, "table_rows"),
        data_length: uint_value(row, "data_length"),
        index_length: uint_value(row, "index_length"),
        data_free: None,
        auto_increment: None,
        collation: str_value(row, "table_collation"),
        update_time: str_value(row, "update_time").filter(|value| !value.is_empty()),
    })
}

async fn explain_sql_pg(
    ctx: &OpsContext,
    label: &str,
    sql: &str,
) -> Result<Option<ExplainSample>, OpsError> {
    let rows = pg_rows::fetch_rows(ctx, &format!("EXPLAIN (FORMAT JSON) {sql}")).await?;
    let Some(raw) = rows.first().and_then(|row| row.values().next()).cloned() else {
        return Ok(None);
    };
    let root = crate::ops::pg_explain::parse_root(&raw)
        .map_err(|err| OpsError::Validation(format!("{label} 的 EXPLAIN 无法解析: {err}")))?;
    Ok(Some(crate::ops::pg_explain::sample_from_node(
        label, sql, &root,
    )))
}

async fn load_index_usage_pg(ctx: &OpsContext, table: &str) -> Result<Vec<IndexUsage>, OpsError> {
    let track_counts = pg_rows::fetch_rows(
        ctx,
        "SELECT current_setting('track_counts') AS track_counts",
    )
    .await?
    .first()
    .and_then(|row| str_value(row, "track_counts"))
    .unwrap_or_else(|| "off".into());
    if !track_counts.eq_ignore_ascii_case("on") {
        return Err(OpsError::Validation(
            "track_counts=off：pg_stat_user_indexes 不累计索引扫描，索引使用统计不可用".into(),
        ));
    }
    let tbl = escape_sql_string(table);
    let sql = format!(
        "SELECT s.indexrelname::text AS index_name, s.idx_scan::text AS count_star, \
                s.idx_tup_read::text AS count_read \
         FROM pg_stat_user_indexes s \
         WHERE s.schemaname = current_schema() AND s.relname = '{tbl}'"
    );
    let rows = pg_rows::fetch_rows(ctx, &sql).await?;
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
    if is_read_only_evidence(evidence) {
        return build_read_only_plan(identity, evidence);
    }
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
        evidence_refs: vec![
            "schema:information_schema.COLUMNS".into(),
            "schema:information_schema.STATISTICS".into(),
        ],
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
            ActionSpec::CreateIndex {
                table,
                name,
                columns,
                ..
            } => Some(ActionSpec::DropIndex {
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
        rewrite_suggestions: vec![],
        verification_explain_analyze: false,
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
            explain_findings: evidence
                .explain
                .iter()
                .map(ExplainSample::summary)
                .collect(),
        }),
        execution: None,
    };
    Ok(plan.seal())
}

pub(crate) fn is_read_only_evidence(evidence: &EvidencePack) -> bool {
    evidence
        .extra
        .as_ref()
        .and_then(|extra| extra.get("read_only"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

/// PostgreSQL 只读计划：只给诊断与建议，不产生任何可执行动作。
fn build_read_only_plan(
    identity: &PlanIdentity,
    evidence: &EvidencePack,
) -> Result<OpsPlan, OpsError> {
    let snapshot = &evidence.snapshot;
    let mut findings = Vec::new();
    let mut checks = Vec::new();

    findings.push(Finding {
        id: "read-only".into(),
        title: "PostgreSQL 只读分析".into(),
        detail: "本计划不生成可执行 DDL、不提供 apply；仅在界面展示索引缺口与使用情况。".into(),
        severity: Severity::Info,
        evidence_refs: vec!["engine:postgresql".into()],
        confidence: 1.0,
    });
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
            .map(|index| format!("{} ({})", index.name, index.column_names().join(",")))
            .collect::<Vec<_>>()
            .join("; "),
        severity: Severity::Info,
        evidence_refs: vec!["schema:pg_attribute".into(), "schema:pg_index".into()],
        confidence: 1.0,
    });

    for sample in &evidence.explain {
        if sample.needs_index() {
            findings.push(Finding {
                id: format!("explain-{}", sample.label),
                title: format!(
                    "{} 命中 {}（只读建议）",
                    sample.table_name.as_deref().unwrap_or("(table)"),
                    sample.access_type.as_deref().unwrap_or("-")
                ),
                detail: sample.summary(),
                severity: Severity::Warn,
                evidence_refs: vec![format!("explain:{}", sample.label)],
                confidence: 0.75,
            });
        }
    }

    for usage in &evidence.index_usage {
        if usage.count_star.unwrap_or(0) == 0 {
            findings.push(Finding {
                id: format!("unused-{}", usage.index_name),
                title: format!("索引 {} 在统计窗口内未被扫描", usage.index_name),
                detail: "仅作只读提示；删除索引属于写操作，不在本轮 PG 能力范围内。".into(),
                severity: Severity::Info,
                evidence_refs: vec!["stats:pg_stat_user_indexes.idx_scan".into()],
                confidence: 0.7,
            });
        }
    }

    checks.push(ValidationCheck {
        id: "read-only".into(),
        title: "只读校验".into(),
        status: ValidationStatus::Pass,
        detail: "PG 索引优化为只读分析：计划不包含任何 ActionSpec，不提供 apply".into(),
    });

    let plan = OpsPlan {
        plan_id: uuid::Uuid::new_v4().to_string(),
        scenario_id: "mysql-index-advisor".into(),
        db_id: identity.db_id.clone(),
        db_name: identity.db_name.clone(),
        db_fingerprint: identity.db_fingerprint.clone(),
        evidence_hash: evidence.evidence_hash(),
        diagnosis: "PostgreSQL 只读分析：展示 EXPLAIN 索引缺口与 pg_stat_user_indexes 使用情况；\
                    不生成可执行 DDL。"
            .into(),
        findings,
        verification_sql: evidence.workload_sql.clone(),
        actions: vec![],
        risks: vec![],
        validation_checks: checks,
        rollback: RollbackPlan {
            summary: "只读计划，没有需要回滚的动作。".into(),
            actions: vec![],
            automatic: false,
        },
        code_advice: vec![
            "PG 索引 DDL 请按团队变更流程在数据库中执行；本工具不代执行。".to_string(),
            "删除未使用索引前，请结合 pg_stat_user_indexes 的统计窗口与业务周期确认。".to_string(),
        ],
        rewrite_suggestions: vec![],
        verification_explain_analyze: false,
        confidence: 0.6,
        requires_approval: false,
        created_at: crate::ops::types::now(),
        plan_hash: String::new(),
        status: PlanStatus::PendingApproval,
        evidence_summary: Some(EvidenceSummary {
            table: snapshot.table_name.clone(),
            column_count: snapshot.columns.len(),
            index_count: snapshot.indexes.len(),
            row_estimate: evidence.stats.table_rows,
            explain_findings: evidence
                .explain
                .iter()
                .map(ExplainSample::summary)
                .collect(),
        }),
        execution: None,
    };
    Ok(plan.seal())
}

pub(crate) fn extract_candidate_columns(sql: &str, snapshot: &TableSnapshot) -> Vec<String> {
    let padded = format!(" {} ", sql.to_ascii_lowercase());
    let mut columns: Vec<String> = Vec::new();

    for (start, stops) in [
        (
            " where ",
            vec![
                " order by ",
                " group by ",
                " limit ",
                " having ",
                " union ",
                " ;",
            ],
        ),
        (" order by ", vec![" limit ", " ;", " union "]),
        (
            " group by ",
            vec![" having ", " order by ", " limit ", " ;"],
        ),
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
                if !columns.iter().any(|c| c.eq_ignore_ascii_case(&column.name)) {
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
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
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
            assert_eq!(
                cols,
                expected.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                "sql={sql}"
            );
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
                    ..Default::default()
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
                    ..Default::default()
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
                    ..Default::default()
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
                IndexColumn {
                    name: "user_id".into(),
                    seq: 1,
                    cardinality: None,
                },
                IndexColumn {
                    name: "status".into(),
                    seq: 2,
                    cardinality: None,
                },
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
                ..Default::default()
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
                workload_sql: vec![
                    "SELECT * FROM orders WHERE user_id = 1 AND status = 'paid'".into()
                ],
                ..Default::default()
            },
            &crate::ops::policy::OpsPolicy::default(),
        )
        .unwrap();
        assert!(plan.actions.is_empty());
        assert!(plan.findings.iter().any(|f| f.id.starts_with("covered")));
    }
}
