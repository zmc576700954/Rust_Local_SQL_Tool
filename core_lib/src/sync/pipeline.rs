//! 数据同步流水线的通用部分：阶段顺序、作业指纹、选项校验、报告收尾。
//!
//! PostgreSQL 与 MySQL/MariaDB 引擎共用这里的数据模型与语义，差异只保留在
//! 各引擎的 Plan/DDL/SQL 生成层。这样 checkpoint、报告字段与阶段顺序不会
//! 在两个引擎里各写一份、逐渐漂移。

use crate::error::AppError;
use crate::sync::checkpoint::CheckpointStore;
use crate::sync::mysql_engine::SyncMode;
use crate::sync::pg_data_sync::{
    PgDataSyncOptions, PgDataSyncProgress, PgDataSyncReport, PgTableReport,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

/// 流水线阶段名，与 web-server 的 job.stage 取值保持一致。
pub const STAGE_PLAN: &str = "plan";
pub const STAGE_STRUCTURE: &str = "structure";
pub const STAGE_LOAD: &str = "load";
pub const STAGE_INDEXES: &str = "indexes";
pub const STAGE_SEQUENCES: &str = "sequences";
pub const STAGE_VERIFY: &str = "verify";

/// 通用选项校验：mirror 确认、单事务与续跑互斥。
pub fn validate_options(options: &PgDataSyncOptions) -> Result<(), AppError> {
    if options.mode == SyncMode::Mirror && !options.mirror_confirmed {
        return Err(AppError::BadRequest(
            "mirror 模式会删除目标库中源库不存在的数据，必须在接口上显式确认 mirror_confirmed=true"
                .to_string(),
        ));
    }
    if options.single_transaction && options.resume {
        return Err(AppError::BadRequest(
            "single_transaction 与 resume 互斥：单事务作业不可续跑".to_string(),
        ));
    }
    Ok(())
}

/// 按参数上限收缩批次大小；返回 (实际批次, 是否被收缩)。
pub fn clamp_batch_size(requested: usize, columns: usize, max_placeholders: usize) -> (usize, bool) {
    let requested = requested.max(1);
    let limit = (max_placeholders / columns.max(1)).max(1);
    let effective = requested.min(limit);
    (effective, effective < requested)
}

/// 生成阶段进度事件。
pub fn stage_progress(
    stage: &str,
    message: impl Into<String>,
    table_total: usize,
    rows_copied: u64,
) -> PgDataSyncProgress {
    PgDataSyncProgress {
        stage: stage.to_string(),
        table: None,
        table_index: table_total,
        table_total,
        rows_copied,
        total_rows: None,
        message: message.into(),
    }
}

/// 作业指纹里的一张表。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobKeyTable {
    pub display_name: String,
    pub columns: Vec<JobKeyColumn>,
    pub pk_columns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobKeyColumn {
    pub name: String,
    pub type_text: String,
    pub identity: String,
    pub generated: String,
}

/// 源/目标/模式/批次/表结构指纹。表顺序不敏感，结构变化会改变指纹。
pub fn compute_job_key_for_tables(
    source_label: &str,
    target_label: &str,
    mode: &str,
    batch_size: usize,
    tables: &[JobKeyTable],
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(source_label.as_bytes());
    hasher.update(b"\x1f");
    hasher.update(target_label.as_bytes());
    hasher.update(b"\x1f");
    hasher.update(mode.as_bytes());
    hasher.update(b"\x1f");
    hasher.update(batch_size.to_string().as_bytes());
    hasher.update(b"\x1f");
    let mut sorted = tables.to_vec();
    sorted.sort_by(|a, b| a.display_name.cmp(&b.display_name));
    for table in sorted {
        hasher.update(table.display_name.as_bytes());
        hasher.update(b"\x1e");
        for col in &table.columns {
            hasher.update(col.name.as_bytes());
            hasher.update(b"\x1d");
            hasher.update(col.type_text.as_bytes());
            hasher.update(b"\x1d");
            hasher.update(col.identity.as_bytes());
            hasher.update(b"\x1d");
            hasher.update(col.generated.as_bytes());
            hasher.update(b"\x1e");
        }
        for pk in &table.pk_columns {
            hasher.update(b"\x1c");
            hasher.update(pk.as_bytes());
        }
        hasher.update(b"\x1f");
    }
    format!("{:x}", hasher.finalize())
}

/// 外键拓扑排序；有环且未显式 defer 时拒绝，并把环上的表列出来。
pub fn order_tables_or_reject(
    tables: &[String],
    edges: &[(String, String)],
    defer_constraints: bool,
    defer_hint: &str,
) -> Result<Vec<String>, AppError> {
    match topological_order(tables, edges) {
        Ok(order) => Ok(order),
        Err(cycle) => {
            if defer_constraints {
                let mut sorted = tables.to_vec();
                sorted.sort();
                Ok(sorted)
            } else {
                Err(AppError::BadRequest(format!(
                    "检测到外键循环依赖：{}。默认拒绝同步；{}",
                    cycle.join(", "),
                    defer_hint
                )))
            }
        }
    }
}

/// `edges` 为 `(子表, 被引用父表)`。父表必须先同步。
/// 存在环时返回环上（Kahn 剩余）的表名列表。
pub fn topological_order(
    tables: &[String],
    edges: &[(String, String)],
) -> Result<Vec<String>, Vec<String>> {
    let known: BTreeSet<&str> = tables.iter().map(|t| t.as_str()).collect();
    let mut indegree: BTreeMap<&str, usize> = tables.iter().map(|t| (t.as_str(), 0)).collect();
    let mut children: BTreeMap<&str, Vec<&str>> = BTreeMap::new();

    for (child, parent) in edges {
        if child == parent {
            continue;
        }
        let (Some(child), Some(parent)) = (
            known.get(child.as_str()).copied(),
            known.get(parent.as_str()).copied(),
        ) else {
            continue;
        };
        if !children
            .get(parent)
            .map(|v| v.contains(&child))
            .unwrap_or(false)
        {
            children.entry(parent).or_default().push(child);
            *indegree.get_mut(child).unwrap() += 1;
        }
    }

    let mut ready: BTreeSet<&str> = indegree
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(t, _)| *t)
        .collect();
    let mut order = Vec::with_capacity(tables.len());
    while let Some(next) = ready.iter().next().copied() {
        ready.remove(next);
        order.push(next.to_string());
        if let Some(kids) = children.get(next) {
            for kid in kids {
                let d = indegree.get_mut(kid).unwrap();
                *d -= 1;
                if *d == 0 {
                    ready.insert(kid);
                }
            }
        }
    }

    if order.len() == tables.len() {
        Ok(order)
    } else {
        let cycle = indegree
            .into_iter()
            .filter(|(_, d)| *d > 0)
            .map(|(t, _)| t.to_string())
            .collect::<Vec<_>>();
        Err(cycle)
    }
}

/// 汇总逐表校验结果；`downgraded_unverified` 表示只做了行数校验。
pub fn overall_verification_status(tables: &[PgTableReport]) -> String {
    let mut any_failed = false;
    let mut any_unverified = false;
    let mut any_downgraded = false;
    let mut any_count_only = false;
    let mut any_verified = false;
    for t in tables {
        match t.verification.as_str() {
            "failed" => any_failed = true,
            "unverified" => any_unverified = true,
            "downgraded_unverified" => any_downgraded = true,
            "count_only" => any_count_only = true,
            "verified" => any_verified = true,
            _ => {}
        }
    }
    if any_failed {
        "failed".to_string()
    } else if any_unverified {
        "unverified".to_string()
    } else if any_downgraded {
        "downgraded_unverified".to_string()
    } else if any_count_only {
        "count_only".to_string()
    } else if any_verified {
        "verified".to_string()
    } else {
        "unverified".to_string()
    }
}

/// 收尾报告：总行数、结束时间、可续跑与断点文件。
pub fn finalize_report(
    report: &mut PgDataSyncReport,
    options: &PgDataSyncOptions,
    store: &CheckpointStore,
    job_key: &str,
    all_tables_have_pk: bool,
) {
    report.total_rows = report.tables.iter().map(|t| t.rows_copied).sum();
    report.finished_at = chrono::Utc::now().timestamp();
    report.resumable = !options.single_transaction && all_tables_have_pk;
    report.verification_status = overall_verification_status(&report.tables);
    if !options.single_transaction {
        report.checkpoint_file = Some(store.path_for(job_key).display().to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, ty: &str) -> JobKeyColumn {
        JobKeyColumn {
            name: name.to_string(),
            type_text: ty.to_string(),
            identity: String::new(),
            generated: String::new(),
        }
    }

    #[test]
    fn job_key_is_order_insensitive_and_structure_sensitive() {
        let a = JobKeyTable {
            display_name: "users".to_string(),
            columns: vec![col("id", "bigint")],
            pk_columns: vec!["id".to_string()],
        };
        let b = JobKeyTable {
            display_name: "orders".to_string(),
            columns: vec![col("id", "bigint")],
            pk_columns: vec!["id".to_string()],
        };
        let k1 = compute_job_key_for_tables("s", "t", "upsert_only", 100, &[a.clone(), b.clone()]);
        let k2 = compute_job_key_for_tables("s", "t", "upsert_only", 100, &[b, a.clone()]);
        assert_eq!(k1, k2);
        let mut changed = a;
        changed.columns[0].type_text = "int".to_string();
        let k3 = compute_job_key_for_tables("s", "t", "upsert_only", 100, &[changed]);
        assert_ne!(k1, k3);
    }

    #[test]
    fn topology_puts_parent_before_child_and_reports_cycles() {
        let tables = vec!["orders".to_string(), "users".to_string()];
        let edges = vec![("orders".to_string(), "users".to_string())];
        let order = topological_order(&tables, &edges).expect("no cycle");
        assert!(order.iter().position(|t| t == "users") < order.iter().position(|t| t == "orders"));
        let cycle = topological_order(
            &["a".to_string(), "b".to_string()],
            &[("a".to_string(), "b".to_string()), ("b".to_string(), "a".to_string())],
        )
        .expect_err("cycle");
        assert_eq!(cycle, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn verification_marks_downgraded_without_failed() {
        let mut t = PgTableReport {
            table_name: "t".to_string(),
            status: "completed".to_string(),
            rows_copied: 1,
            batches: 1,
            pk_columns: vec![],
            source_rows: Some(1),
            target_rows: Some(1),
            checksum_equal: None,
            verification: "downgraded_unverified".to_string(),
            sequences_aligned: vec![],
            error: None,
            notes: vec![],
        };
        assert_eq!(overall_verification_status(&[t.clone()]), "downgraded_unverified");
        t.verification = "failed".to_string();
        assert_eq!(overall_verification_status(&[t]), "failed");
    }
}
