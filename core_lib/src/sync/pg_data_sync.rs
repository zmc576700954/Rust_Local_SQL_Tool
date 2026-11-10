//! PostgreSQL 分阶段数据同步引擎。
//!
//! 流水线：Plan（元数据/主键/外键拓扑）→ 建基础结构（enum/表）→ 按主键 keyset
//! 分批灌数据（先 commit 再写 checkpoint）→ 建索引/外键 → 序列对齐 → 行数与
//! 分块哈希校验。
//!
//! 可靠性字段按实际语义填写：
//! - 默认逐批提交：`atomic=false`、`rollback_scope="batch"`、可续跑；
//! - `single_transaction=true`：`atomic=true`、`rollback_scope="job"`、不可续跑。

use crate::config::DbType;
use crate::error::AppError;
use crate::sql_util::{quote_ident_pg, split_pg_table_ref};
use crate::sync::checkpoint::{Checkpoint, CheckpointStore, TableCheckpoint};
use crate::sync::mysql_engine::SyncMode;
use crate::sync::pg_plan::{
    add_foreign_key_sql, batch_select_sql, compute_job_key, create_enum_sql, create_index_if_not_exists,
    create_table_sql, delete_after_pk_sql, delete_all_sql, delete_by_pk_sql, full_select_sql,
    qualified_table, range_pk_select_sql, sequence_setval_sql, topological_order, upsert_sql,
    validate_plan, PgColumnSpec, PgEnumSpec, PgForeignKeySpec, PgTablePlan,
};
use crate::sync::pg_types::enum_type_refs;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::postgres::{PgArguments, PgRow};
use sqlx::query::Query;
use sqlx::{PgConnection, PgPool, Postgres, Row};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct PgDataSyncOptions {
    /// None 表示同步默认 schema 下的全部普通表。
    pub tables: Option<Vec<String>>,
    pub mode: SyncMode,
    pub batch_size: usize,
    pub create_missing_tables: bool,
    /// mirror 模式会删除目标库数据，必须显式确认。
    pub mirror_confirmed: bool,
    pub single_transaction: bool,
    pub defer_constraints: bool,
    pub allow_no_pk: bool,
    pub resume: bool,
    pub checkpoint_dir: PathBuf,
    /// 源/目标连接标识，参与断点指纹。
    pub job_label: String,
}

impl Default for PgDataSyncOptions {
    fn default() -> Self {
        Self {
            tables: None,
            mode: SyncMode::UpsertOnly,
            batch_size: 1000,
            create_missing_tables: true,
            mirror_confirmed: false,
            single_transaction: false,
            defer_constraints: false,
            allow_no_pk: false,
            resume: false,
            checkpoint_dir: PathBuf::from("data-sync"),
            job_label: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PgDataSyncProgress {
    pub stage: String,
    pub table: Option<String>,
    pub table_index: usize,
    pub table_total: usize,
    pub rows_copied: u64,
    pub total_rows: Option<u64>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PgTableReport {
    pub table_name: String,
    /// pending / running / completed / failed / skipped
    pub status: String,
    pub rows_copied: u64,
    pub batches: usize,
    pub pk_columns: Vec<String>,
    pub source_rows: Option<u64>,
    pub target_rows: Option<u64>,
    pub checksum_equal: Option<bool>,
    /// verified / count_only / failed / unverified / skipped
    pub verification: String,
    pub sequences_aligned: Vec<String>,
    pub error: Option<String>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PgDataSyncReport {
    pub job_key: String,
    pub mode: SyncMode,
    pub tables: Vec<PgTableReport>,
    pub total_rows: u64,
    pub atomic: bool,
    pub rolled_back: bool,
    pub rollback_scope: String,
    pub resumable: bool,
    pub verification_status: String,
    pub non_transactional_tables: Vec<String>,
    pub deferred_constraints: bool,
    pub checkpoint_file: Option<String>,
    pub warnings: Vec<String>,
    pub error: Option<String>,
    pub started_at: i64,
    pub finished_at: i64,
}

#[derive(Debug, Clone)]
struct SourceRow {
    values: Vec<Option<String>>,
    pk: Vec<String>,
}

#[derive(Debug, Clone)]
enum Bind {
    Text(Option<String>),
    Int(i64),
}

fn bind_all<'q>(
    mut q: Query<'q, Postgres, PgArguments>,
    binds: &[Bind],
) -> Query<'q, Postgres, PgArguments> {
    for b in binds {
        q = match b {
            Bind::Text(v) => q.bind(v.clone()),
            Bind::Int(v) => q.bind(*v),
        };
    }
    q
}

async fn execute_sql(
    conn: &mut PgConnection,
    sql: &str,
    binds: &[Bind],
) -> Result<u64, AppError> {
    let q = bind_all(sqlx::query(sql), binds);
    Ok(q.execute(&mut *conn).await?.rows_affected())
}

async fn fetch_all(
    conn: &mut PgConnection,
    sql: &str,
    binds: &[Bind],
) -> Result<Vec<PgRow>, AppError> {
    let q = bind_all(sqlx::query(sql), binds);
    Ok(q.fetch_all(&mut *conn).await?)
}

async fn begin(conn: &mut PgConnection, defer_constraints: bool) -> Result<(), AppError> {
    sqlx::query("BEGIN").execute(&mut *conn).await?;
    if defer_constraints {
        sqlx::query("SET LOCAL session_replication_role = replica")
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

async fn commit(conn: &mut PgConnection) -> Result<(), AppError> {
    sqlx::query("COMMIT").execute(&mut *conn).await?;
    Ok(())
}

async fn rollback(conn: &mut PgConnection) {
    let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
}

fn table_mut<'a>(tables: &'a mut [PgTableReport], name: &str) -> &'a mut PgTableReport {
    tables
        .iter_mut()
        .find(|t| t.table_name == name)
        .expect("table report must exist")
}

pub struct PgDataSyncEngine;

impl PgDataSyncEngine {
    pub async fn run(
        source: &crate::db::DbClient,
        target: &crate::db::DbClient,
        mut options: PgDataSyncOptions,
        progress: impl Fn(PgDataSyncProgress) + Send + Sync,
    ) -> Result<PgDataSyncReport, AppError> {
        if !matches!(source.db_type, DbType::PostgreSQL) {
            return Err(AppError::BadRequest(format!(
                "PG 数据同步要求源库为 PostgreSQL，当前为 {:?}",
                source.db_type
            )));
        }
        if !matches!(target.db_type, DbType::PostgreSQL) {
            return Err(AppError::BadRequest(format!(
                "PG 数据同步要求目标库为 PostgreSQL，当前为 {:?}",
                target.db_type
            )));
        }
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
        options.batch_size = options.batch_size.max(1);

        let src_pool: &PgPool = source.pool.pg()?;
        let tgt_pool: &PgPool = target.pool.pg()?;
        let mut src = src_pool.acquire().await?;
        let mut tgt = tgt_pool.acquire().await?;

        // 统一的文本 I/O 上下文，保证 timestamptz/date 的 ::text 在两端一致。
        for conn in [&mut src, &mut tgt] {
            sqlx::query("SET TimeZone='UTC'").execute(&mut **conn).await?;
            sqlx::query("SET DateStyle='ISO, MDY'")
                .execute(&mut **conn)
                .await?;
        }

        let plans = build_plans(&mut src, &mut tgt, &options).await?;
        for plan in &plans {
            validate_plan(plan, options.allow_no_pk)?;
            if !plan.target_exists && !options.create_missing_tables {
                return Err(AppError::BadRequest(format!(
                    "目标库缺少表 {}，且 create_missing_tables=false；请先做结构同步或显式允许建表",
                    plan.display_name
                )));
            }
        }

        if options.defer_constraints {
            ensure_superuser(&mut tgt).await?;
        }

        let table_names: Vec<String> = plans.iter().map(|p| p.display_name.clone()).collect();
        let edges: Vec<(String, String)> = plans
            .iter()
            .flat_map(|p| {
                p.foreign_keys
                    .iter()
                    .map(move |fk| (p.display_name.clone(), fk.referenced_table.clone()))
            })
            .collect();
        let ordered_names = match topological_order(&table_names, &edges) {
            Ok(order) => order,
            Err(cycle) => {
                if options.defer_constraints {
                    let mut sorted = table_names.clone();
                    sorted.sort();
                    sorted
                } else {
                    return Err(AppError::BadRequest(format!(
                        "检测到外键循环依赖：{}。默认拒绝同步；如确认可在目标端临时关闭外键触发，请显式设置 defer_constraints=true（需要目标库超级用户权限）",
                        cycle.join(", ")
                    )));
                }
            }
        };
        let by_name: BTreeMap<String, PgTablePlan> = plans
            .into_iter()
            .map(|p| (p.display_name.clone(), p))
            .collect();
        let ordered: Vec<PgTablePlan> = ordered_names
            .iter()
            .filter_map(|name| by_name.get(name).cloned())
            .collect();

        let src_db: String = sqlx::query_scalar("SELECT current_database()")
            .fetch_one(&mut *src)
            .await?;
        let tgt_db: String = sqlx::query_scalar("SELECT current_database()")
            .fetch_one(&mut *tgt)
            .await?;
        let mode_label = match options.mode {
            SyncMode::Mirror => "mirror",
            SyncMode::UpsertOnly => "upsert_only",
        };
        let key = compute_job_key(
            &format!("{}:{}", options.job_label, src_db),
            &format!("{}:{}", options.job_label, tgt_db),
            mode_label,
            options.batch_size,
            &ordered,
        );

        let store = CheckpointStore::new(&options.checkpoint_dir);
        let mut checkpoint = if options.single_transaction {
            Checkpoint::new(key.clone())
        } else if options.resume {
            store.load(&key)?.ok_or_else(|| {
                AppError::BadRequest(format!(
                    "未找到可续跑的断点 {}（job_key={}），请先执行一次同步或关闭 resume",
                    store.path_for(&key).display(),
                    key
                ))
            })?
        } else {
            Checkpoint::new(key.clone())
        };

        let now = chrono::Utc::now().timestamp();
        let mut report = PgDataSyncReport {
            job_key: key.clone(),
            mode: options.mode.clone(),
            tables: ordered
                .iter()
                .map(|p| PgTableReport {
                    table_name: p.display_name.clone(),
                    status: "pending".to_string(),
                    rows_copied: 0,
                    batches: 0,
                    pk_columns: p.pk_columns.clone(),
                    source_rows: None,
                    target_rows: None,
                    checksum_equal: None,
                    verification: "unverified".to_string(),
                    sequences_aligned: vec![],
                    error: None,
                    notes: p.warnings.clone(),
                })
                .collect(),
            total_rows: 0,
            atomic: options.single_transaction,
            rolled_back: false,
            rollback_scope: if options.single_transaction {
                "job".to_string()
            } else {
                "batch".to_string()
            },
            resumable: false,
            verification_status: "unverified".to_string(),
            non_transactional_tables: vec![],
            deferred_constraints: options.defer_constraints,
            checkpoint_file: None,
            warnings: vec![],
            error: None,
            started_at: now,
            finished_at: now,
        };

        let pipeline = execute_pipeline(
            &mut src,
            &mut tgt,
            &ordered,
            &options,
            &store,
            &mut checkpoint,
            &mut report,
            &progress,
        )
        .await;

        if let Err(e) = pipeline {
            if options.single_transaction {
                // 单事务模式下任何阶段失败都回滚整个作业；重复 ROLLBACK 无副作用。
                rollback(&mut tgt).await;
                report.rolled_back = true;
            }
            if report.error.is_none() {
                report.error = Some(e.to_string());
            }
            for t in &mut report.tables {
                if t.status == "pending" {
                    t.status = "skipped".to_string();
                }
            }
            if report.verification_status == "unverified" {
                for t in &mut report.tables {
                    if t.verification == "unverified" && t.status == "skipped" {
                        t.verification = "skipped".to_string();
                    }
                }
            }
        }

        report.total_rows = report.tables.iter().map(|t| t.rows_copied).sum();
        report.finished_at = chrono::Utc::now().timestamp();
        report.resumable =
            !options.single_transaction && ordered.iter().all(|p| !p.pk_columns.is_empty());
        if !options.single_transaction {
            report.checkpoint_file = Some(store.path_for(&key).display().to_string());
        }
        Ok(report)
    }
}

async fn ensure_superuser(conn: &mut PgConnection) -> Result<(), AppError> {
    let is_super: bool =
        sqlx::query_scalar("SELECT rolsuper FROM pg_roles WHERE rolname = current_user")
            .fetch_one(&mut *conn)
            .await?;
    if !is_super {
        return Err(AppError::BadRequest(
            "defer_constraints 需要目标库超级用户权限（用于 session_replication_role=replica），当前用户无权限；不会静默降级"
                .to_string(),
        ));
    }
    Ok(())
}

async fn execute_pipeline(
    src: &mut PgConnection,
    tgt: &mut PgConnection,
    plans: &[PgTablePlan],
    opts: &PgDataSyncOptions,
    store: &CheckpointStore,
    checkpoint: &mut Checkpoint,
    report: &mut PgDataSyncReport,
    progress: &(dyn Fn(PgDataSyncProgress) + Send + Sync),
) -> Result<(), AppError> {
    let single = opts.single_transaction;
    if single {
        begin(tgt, opts.defer_constraints).await?;
    }

    progress(PgDataSyncProgress {
        stage: "structure".to_string(),
        table: None,
        table_index: 0,
        table_total: plans.len(),
        rows_copied: 0,
        total_rows: None,
        message: "正在创建缺失的 enum/表结构".to_string(),
    });
    create_structure(tgt, plans, single, opts.defer_constraints, &mut report.warnings).await?;

    let table_total = plans.len();
    for (idx, plan) in plans.iter().enumerate() {
        {
            let t = table_mut(&mut report.tables, &plan.display_name);
            t.status = "running".to_string();
        }
        let res = load_table(
            src,
            tgt,
            plan,
            opts,
            checkpoint,
            store,
            table_mut(&mut report.tables, &plan.display_name),
            !single,
            idx,
            table_total,
            progress,
        )
        .await;
        if let Err(e) = res {
            {
                let t = table_mut(&mut report.tables, &plan.display_name);
                t.status = "failed".to_string();
                t.error = Some(e.to_string());
            }
            if single {
                rollback(tgt).await;
                report.rolled_back = true;
            }
            report.error = Some(format!("表 {} 同步失败：{e}", plan.display_name));
            return Err(e);
        }
    }

    progress(PgDataSyncProgress {
        stage: "indexes".to_string(),
        table: None,
        table_index: table_total,
        table_total,
        rows_copied: report.tables.iter().map(|t| t.rows_copied).sum(),
        total_rows: None,
        message: "正在创建索引与外键".to_string(),
    });
    if let Err(e) = create_indexes_and_fks(tgt, plans, single, opts.defer_constraints).await {
        if single {
            rollback(tgt).await;
            report.rolled_back = true;
        }
        report.error = Some(format!("创建索引/外键失败：{e}"));
        return Err(e);
    }

    progress(PgDataSyncProgress {
        stage: "sequences".to_string(),
        table: None,
        table_index: table_total,
        table_total,
        rows_copied: report.tables.iter().map(|t| t.rows_copied).sum(),
        total_rows: None,
        message: "正在对齐序列".to_string(),
    });
    if let Err(e) = align_sequences(tgt, plans, single, opts.defer_constraints, &mut report.tables).await
    {
        if single {
            rollback(tgt).await;
            report.rolled_back = true;
        }
        report.error = Some(format!("序列对齐失败：{e}"));
        return Err(e);
    }

    progress(PgDataSyncProgress {
        stage: "verify".to_string(),
        table: None,
        table_index: table_total,
        table_total,
        rows_copied: report.tables.iter().map(|t| t.rows_copied).sum(),
        total_rows: None,
        message: "正在做行数与分块哈希校验".to_string(),
    });
    verify_all(src, tgt, plans, opts.batch_size, &mut report.tables).await;
    report.verification_status = overall_verification_status(&report.tables);

    if single {
        if report.verification_status == "failed" {
            rollback(tgt).await;
            report.rolled_back = true;
            report.error =
                Some("单事务模式下校验失败，已回滚整个作业".to_string());
            return Err(AppError::InternalError(
                "单事务模式下校验失败，已回滚整个作业".to_string(),
            ));
        }
        commit(tgt).await?;
    }

    // 非单事务模式下，校验失败不回滚已提交批次，但结果会如实标记。
    if report.verification_status == "failed" && report.error.is_none() {
        report.error = Some("行数或分块哈希校验未通过，请查看逐表 verification 字段".to_string());
    }
    Ok(())
}

// ── Plan 阶段 ──────────────────────────────────────────

async fn build_plans(
    src: &mut PgConnection,
    tgt: &mut PgConnection,
    opts: &PgDataSyncOptions,
) -> Result<Vec<PgTablePlan>, AppError> {
    let default_schema: String = sqlx::query_scalar("SELECT COALESCE(current_schema(), 'public')")
        .fetch_one(&mut *src)
        .await?;

    let table_refs: Vec<(String, String)> = match &opts.tables {
        Some(list) => list
            .iter()
            .filter(|t| !t.trim().is_empty())
            .map(|t| {
                let (schema, name) = split_pg_table_ref(t.trim());
                (
                    schema.unwrap_or(default_schema.as_str()).to_string(),
                    name.to_string(),
                )
            })
            .collect(),
        None => {
            let names: Vec<String> = sqlx::query_scalar(
                "SELECT c.relname FROM pg_class c \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = $1 AND c.relkind = 'r' AND NOT c.relispartition \
                 ORDER BY c.relname",
            )
            .bind(&default_schema)
            .fetch_all(&mut *src)
            .await?;
            names
                .into_iter()
                .map(|n| (default_schema.clone(), n))
                .collect()
        }
    };

    if table_refs.is_empty() {
        return Err(AppError::BadRequest(
            "没有可同步的表：请检查表选择或源库默认 schema".to_string(),
        ));
    }

    let mut plans = Vec::with_capacity(table_refs.len());
    let mut seen = BTreeSet::new();
    for (schema, name) in table_refs {
        let display = format!("{schema}.{name}");
        if !seen.insert(display.clone()) {
            continue;
        }
        let qualified = qualified_table(&schema, &name);
        if !relation_exists(src, &qualified).await? {
            return Err(AppError::BadRequest(format!(
                "源库不存在表 {display}"
            )));
        }
        let columns = introspect_columns(src, &schema, &name).await?;
        if columns.is_empty() {
            return Err(AppError::BadRequest(format!(
                "源表 {display} 没有可读列"
            )));
        }
        let source_pk_columns = pk_columns(src, &qualified).await?;
        let target_exists = relation_exists(tgt, &qualified).await?;
        let target_columns = if target_exists {
            target_columns(tgt, &schema, &name).await?
        } else {
            vec![]
        };
        let target_pk_columns = if target_exists {
            pk_columns(tgt, &qualified).await?
        } else {
            vec![]
        };
        let target_identity_always = if target_exists {
            has_always_identity(tgt, &schema, &name).await?
        } else {
            columns.iter().any(|c| c.identity == "a")
        };
        let source_count: i64 = sqlx::query_scalar(&format!(
            "SELECT count(*)::bigint FROM {qualified}"
        ))
        .fetch_one(&mut *src)
        .await?;
        let foreign_keys = foreign_keys(src, &schema, &name).await?;
        let index_defs = index_defs(src, &schema, &name).await?;

        let mut enum_keys = BTreeSet::new();
        for col in &columns {
            for key in enum_type_refs(col) {
                enum_keys.insert(key);
            }
        }
        let mut enum_types = Vec::with_capacity(enum_keys.len());
        for (es, en) in enum_keys {
            let labels = enum_labels(src, &es, &en).await?.ok_or_else(|| {
                AppError::BadRequest(format!(
                    "无法读取源库枚举类型 {}.{} 的标签",
                    es, en
                ))
            })?;
            enum_types.push(PgEnumSpec {
                schema: es,
                name: en,
                labels,
            });
        }

        let mut warnings = Vec::new();
        if !target_exists {
            for col in &columns {
                if col.identity.is_empty()
                    && col
                        .default_expr
                        .as_deref()
                        .map(|d| d.trim_start().starts_with("nextval("))
                        .unwrap_or(false)
                {
                    warnings.push(format!(
                        "表 {display} 列 {} 的 serial 默认值在目标新建表上以 identity 承接",
                        col.name
                    ));
                }
            }
        }

        plans.push(PgTablePlan {
            display_name: display,
            schema,
            name,
            columns,
            pk_columns: source_pk_columns,
            target_exists,
            target_columns,
            target_pk_columns,
            target_identity_always,
            source_count: source_count.max(0) as u64,
            foreign_keys,
            index_defs,
            enum_types,
            warnings,
        });
    }

    Ok(plans)
}

async fn relation_exists(conn: &mut PgConnection, qualified: &str) -> Result<bool, AppError> {
    let exists: bool = sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
        .bind(qualified)
        .fetch_one(&mut *conn)
        .await?;
    Ok(exists)
}

async fn introspect_columns(
    conn: &mut PgConnection,
    schema: &str,
    table: &str,
) -> Result<Vec<PgColumnSpec>, AppError> {
    let rows = sqlx::query(
        "SELECT a.attname AS column_name, \
                format_type(a.atttypid, a.atttypmod) AS formatted_type, \
                t.typname AS type_name, tn.nspname AS type_schema, t.typtype::text AS type_kind, \
                et.typname AS elem_type_name, etn.nspname AS elem_type_schema, et.typtype::text AS elem_type_kind, \
                a.attnotnull AS not_null, a.attidentity::text AS identity, a.attgenerated::text AS generated, \
                CASE WHEN a.attgenerated = '' THEN pg_get_expr(ad.adbin, ad.adrelid) END AS default_expr, \
                CASE WHEN a.attgenerated <> '' THEN pg_get_expr(ad.adbin, ad.adrelid) END AS generation_expr \
         FROM pg_attribute a \
         JOIN pg_class c ON c.oid = a.attrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         JOIN pg_type t ON t.oid = a.atttypid \
         JOIN pg_namespace tn ON tn.oid = t.typnamespace \
         LEFT JOIN pg_type et ON et.oid = t.typelem AND t.typcategory = 'A' \
         LEFT JOIN pg_namespace etn ON etn.oid = et.typnamespace \
         LEFT JOIN pg_attrdef ad ON ad.adrelid = a.attrelid AND ad.adnum = a.attnum \
         WHERE c.relname = $1 AND n.nspname = $2 AND a.attnum > 0 AND NOT a.attisdropped \
         ORDER BY a.attnum",
    )
    .bind(table)
    .bind(schema)
    .fetch_all(&mut *conn)
    .await?;

    rows.into_iter()
        .map(|row| {
            Ok(PgColumnSpec {
                name: row.try_get("column_name")?,
                formatted_type: row.try_get("formatted_type")?,
                type_name: row.try_get("type_name")?,
                type_schema: row.try_get("type_schema")?,
                type_kind: row.try_get("type_kind")?,
                elem_type_name: row.try_get::<Option<String>, _>("elem_type_name")?,
                elem_type_schema: row.try_get::<Option<String>, _>("elem_type_schema")?,
                elem_type_kind: row.try_get::<Option<String>, _>("elem_type_kind")?,
                not_null: row.try_get("not_null")?,
                identity: row.try_get::<Option<String>, _>("identity")?.unwrap_or_default(),
                generated: row
                    .try_get::<Option<String>, _>("generated")?
                    .unwrap_or_default(),
                default_expr: row.try_get::<Option<String>, _>("default_expr")?,
                generation_expr: row.try_get::<Option<String>, _>("generation_expr")?,
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()
        .map_err(AppError::from)
}

async fn pk_columns(conn: &mut PgConnection, qualified: &str) -> Result<Vec<String>, AppError> {
    let names: Vec<String> = sqlx::query_scalar(
        "SELECT a.attname \
         FROM pg_index i \
         CROSS JOIN LATERAL unnest(string_to_array(i.indkey::text, ' ')::int2[]) WITH ORDINALITY AS k(attnum, ord) \
         JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k.attnum \
         WHERE i.indrelid = to_regclass($1) AND i.indisprimary \
         ORDER BY k.ord",
    )
    .bind(qualified)
    .fetch_all(&mut *conn)
    .await?;
    Ok(names)
}

async fn target_columns(
    conn: &mut PgConnection,
    schema: &str,
    table: &str,
) -> Result<Vec<String>, AppError> {
    let names: Vec<String> = sqlx::query_scalar(
        "SELECT column_name::text FROM information_schema.columns \
         WHERE table_schema = $1 AND table_name = $2 ORDER BY ordinal_position",
    )
    .bind(schema)
    .bind(table)
    .fetch_all(&mut *conn)
    .await?;
    Ok(names)
}

async fn has_always_identity(
    conn: &mut PgConnection,
    schema: &str,
    table: &str,
) -> Result<bool, AppError> {
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS( \
           SELECT 1 FROM pg_attribute a \
           JOIN pg_class c ON c.oid = a.attrelid \
           JOIN pg_namespace n ON n.oid = c.relnamespace \
           WHERE n.nspname = $1 AND c.relname = $2 AND a.attnum > 0 \
             AND NOT a.attisdropped AND a.attidentity = 'a')",
    )
    .bind(schema)
    .bind(table)
    .fetch_one(&mut *conn)
    .await?;
    Ok(exists)
}

async fn foreign_keys(
    conn: &mut PgConnection,
    schema: &str,
    table: &str,
) -> Result<Vec<PgForeignKeySpec>, AppError> {
    let rows = sqlx::query(
        "SELECT con.conname AS name, pg_get_constraintdef(con.oid) AS definition, \
                rn.nspname AS ref_schema, rc.relname AS ref_table \
         FROM pg_constraint con \
         JOIN pg_class c ON c.oid = con.conrelid \
         JOIN pg_namespace cn ON cn.oid = c.relnamespace \
         JOIN pg_class rc ON rc.oid = con.confrelid \
         JOIN pg_namespace rn ON rn.oid = rc.relnamespace \
         WHERE con.contype = 'f' AND cn.nspname = $1 AND c.relname = $2 \
         ORDER BY con.conname",
    )
    .bind(schema)
    .bind(table)
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter()
        .map(|row| {
            let ref_schema: String = row.try_get("ref_schema")?;
            let ref_table: String = row.try_get("ref_table")?;
            Ok(PgForeignKeySpec {
                name: row.try_get("name")?,
                definition: row.try_get("definition")?,
                referenced_table: format!("{ref_schema}.{ref_table}"),
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()
        .map_err(AppError::from)
}

async fn index_defs(
    conn: &mut PgConnection,
    schema: &str,
    table: &str,
) -> Result<Vec<String>, AppError> {
    let pk_indexes: Vec<String> = sqlx::query_scalar(
        "SELECT ic.relname FROM pg_index x \
         JOIN pg_class ic ON ic.oid = x.indexrelid \
         JOIN pg_class c ON c.oid = x.indrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = $1 AND c.relname = $2 AND x.indisprimary",
    )
    .bind(schema)
    .bind(table)
    .fetch_all(&mut *conn)
    .await?;
    let rows = sqlx::query(
        "SELECT indexname, indexdef FROM pg_indexes \
         WHERE schemaname = $1 AND tablename = $2 ORDER BY indexname",
    )
    .bind(schema)
    .bind(table)
    .fetch_all(&mut *conn)
    .await?;
    let mut defs = Vec::new();
    for row in rows {
        let name: String = row.try_get("indexname")?;
        let def: String = row.try_get("indexdef")?;
        if !pk_indexes.contains(&name) {
            defs.push(def);
        }
    }
    Ok(defs)
}

async fn enum_labels(
    conn: &mut PgConnection,
    schema: &str,
    name: &str,
) -> Result<Option<Vec<String>>, AppError> {
    let type_ref = format!("{}.{}", quote_ident_pg(schema), quote_ident_pg(name));
    let row = sqlx::query(
        "SELECT array_agg(e.enumlabel::text ORDER BY e.enumsortorder) AS labels \
         FROM pg_enum e WHERE e.enumtypid = to_regtype($1)",
    )
    .bind(&type_ref)
    .fetch_one(&mut *conn)
    .await?;
    let labels: Option<Vec<String>> = row.try_get("labels")?;
    Ok(labels)
}

// ── 结构 / 索引 / 序列 ─────────────────────────────────

async fn create_structure(
    conn: &mut PgConnection,
    plans: &[PgTablePlan],
    single: bool,
    defer: bool,
    warnings: &mut Vec<String>,
) -> Result<(), AppError> {
    if !single {
        begin(conn, defer).await?;
    }
    let result: Result<(), AppError> = async {
        let mut seen = BTreeSet::new();
        for plan in plans {
            for e in &plan.enum_types {
                let key = e.key();
                if !seen.insert(key.clone()) {
                    continue;
                }
                let type_ref = format!(
                    "{}.{}",
                    quote_ident_pg(&e.schema),
                    quote_ident_pg(&e.name)
                );
                let exists: bool = sqlx::query_scalar("SELECT to_regtype($1) IS NOT NULL")
                    .bind(&type_ref)
                    .fetch_one(&mut *conn)
                    .await?;
                if !exists {
                    execute_sql(conn, &create_enum_sql(e), &[]).await?;
                } else {
                    let target_labels = enum_labels(conn, &e.schema, &e.name)
                        .await?
                        .unwrap_or_default();
                    if target_labels != e.labels {
                        return Err(AppError::BadRequest(format!(
                            "目标库枚举类型 {} 的标签与源库不一致（源 {:?} / 目标 {:?}），拒绝同步",
                            key, e.labels, target_labels
                        )));
                    }
                }
            }
        }
        for plan in plans {
            if !plan.target_exists {
                execute_sql(conn, &create_table_sql(plan), &[]).await?;
                warnings.extend(plan.warnings.iter().cloned());
            }
        }
        Ok(())
    }
    .await;
    if !single {
        match &result {
            Ok(()) => commit(conn).await?,
            Err(_) => rollback(conn).await,
        }
    }
    result
}

async fn create_indexes_and_fks(
    conn: &mut PgConnection,
    plans: &[PgTablePlan],
    single: bool,
    defer: bool,
) -> Result<(), AppError> {
    if !single {
        begin(conn, defer).await?;
    }
    let result: Result<(), AppError> = async {
        for plan in plans {
            if plan.target_exists {
                continue;
            }
            for def in &plan.index_defs {
                execute_sql(conn, &create_index_if_not_exists(def), &[]).await?;
            }
            for fk in &plan.foreign_keys {
                execute_sql(conn, &add_foreign_key_sql(plan, fk), &[]).await?;
            }
        }
        Ok(())
    }
    .await;
    if !single {
        match &result {
            Ok(()) => commit(conn).await?,
            Err(_) => rollback(conn).await,
        }
    }
    result
}

async fn align_sequences(
    conn: &mut PgConnection,
    plans: &[PgTablePlan],
    single: bool,
    defer: bool,
    tables: &mut [PgTableReport],
) -> Result<(), AppError> {
    if !single {
        begin(conn, defer).await?;
    }
    let result: Result<(), AppError> = async {
        for plan in plans {
            let mut aligned = Vec::new();
            for col in plan.writable_columns() {
                let seq: Option<String> =
                    sqlx::query_scalar("SELECT pg_get_serial_sequence($1, $2)")
                        .bind(plan.qualified())
                        .bind(&col.name)
                        .fetch_one(&mut *conn)
                        .await?;
                if seq.is_some() {
                    execute_sql(
                        conn,
                        &sequence_setval_sql(plan, &col.name),
                        &[
                            Bind::Text(Some(plan.qualified())),
                            Bind::Text(Some(col.name.clone())),
                        ],
                    )
                    .await?;
                    aligned.push(col.name.clone());
                }
            }
            table_mut(tables, &plan.display_name).sequences_aligned = aligned;
        }
        Ok(())
    }
    .await;
    if !single {
        match &result {
            Ok(()) => commit(conn).await?,
            Err(_) => rollback(conn).await,
        }
    }
    result
}

// ── 数据加载 ───────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn load_table(
    src: &mut PgConnection,
    tgt: &mut PgConnection,
    plan: &PgTablePlan,
    opts: &PgDataSyncOptions,
    checkpoint: &mut Checkpoint,
    store: &CheckpointStore,
    table: &mut PgTableReport,
    save_checkpoint: bool,
    table_index: usize,
    table_total: usize,
    progress: &(dyn Fn(PgDataSyncProgress) + Send + Sync),
) -> Result<(), AppError> {
    let entry = checkpoint
        .tables
        .get(&plan.display_name)
        .cloned()
        .unwrap_or_default();
    if entry.completed {
        table.status = "skipped".to_string();
        table.verification = "skipped".to_string();
        table.rows_copied = entry.rows_copied;
        progress(PgDataSyncProgress {
            stage: "load".to_string(),
            table: Some(plan.display_name.clone()),
            table_index,
            table_total,
            rows_copied: entry.rows_copied,
            total_rows: Some(plan.source_count),
            message: "断点显示该表已完成，跳过".to_string(),
        });
        return Ok(());
    }

    let writable = plan.writable_columns();
    let ncols = writable.len();
    if ncols == 0 {
        return Err(AppError::BadRequest(format!(
            "表 {} 没有可写入列（全部为生成列）",
            plan.display_name
        )));
    }
    let effective_batch = opts
        .batch_size
        .min((60_000 / ncols.max(1)).max(1))
        .max(1);
    if effective_batch < opts.batch_size {
        table.notes.push(format!(
            "批次大小受 PostgreSQL 参数上限限制，从 {} 降为 {}",
            opts.batch_size, effective_batch
        ));
    }

    // 无主键：整表读取；mirror 用 delete+insert 保证重复执行结果一致。
    if plan.pk_columns.is_empty() {
        let sql = full_select_sql(plan);
        let rows = fetch_all(src, &sql, &[]).await?;
        let parsed = parse_source_rows(&rows, ncols, &[])?;
        if !opts.single_transaction {
            begin(tgt, opts.defer_constraints).await?;
        }
        let result: Result<(), AppError> = async {
            if opts.mode == SyncMode::Mirror {
                execute_sql(tgt, &delete_all_sql(plan), &[]).await?;
            }
            for chunk in parsed.chunks(effective_batch) {
                let sql = upsert_sql(plan, chunk.len());
                let binds = chunk
                    .iter()
                    .flat_map(|r| r.values.iter().cloned().map(Bind::Text))
                    .collect::<Vec<_>>();
                execute_sql(tgt, &sql, &binds).await?;
            }
            Ok(())
        }
        .await;
        if !opts.single_transaction {
            match &result {
                Ok(()) => commit(tgt).await?,
                Err(_) => rollback(tgt).await,
            }
        }
        result?;
        table.rows_copied = parsed.len() as u64;
        table.batches = 1;
        table.status = "completed".to_string();
        table.notes.push(
            "无主键表按整表复制；upsert 模式下重复执行可能产生重复行，已标记为不可续跑"
                .to_string(),
        );
        checkpoint.tables.insert(
            plan.display_name.clone(),
            TableCheckpoint {
                last_pk: vec![],
                rows_copied: table.rows_copied,
                completed: true,
            },
        );
        if save_checkpoint {
            store.save(checkpoint)?;
        }
        progress(PgDataSyncProgress {
            stage: "load".to_string(),
            table: Some(plan.display_name.clone()),
            table_index,
            table_total,
            rows_copied: table.rows_copied,
            total_rows: Some(plan.source_count),
            message: "无主键表整表复制完成".to_string(),
        });
        return Ok(());
    }

    let pk_idx = plan.pk_indexes_in_writable()?;
    let mut cursor: Option<Vec<String>> = if entry.last_pk.is_empty() {
        None
    } else {
        Some(entry.last_pk.clone())
    };
    let mut rows_copied = entry.rows_copied;
    let mut batches = 0usize;

    loop {
        let has_cursor = cursor.is_some();
        let limit_idx = if has_cursor { pk_idx.len() + 1 } else { 1 };
        let sql = batch_select_sql(plan, has_cursor, limit_idx);
        let mut binds = Vec::new();
        if let Some(c) = &cursor {
            for v in c {
                binds.push(Bind::Text(Some(v.clone())));
            }
        }
        binds.push(Bind::Int(effective_batch as i64));
        let rows = fetch_all(src, &sql, &binds).await?;
        if rows.is_empty() {
            break;
        }
        let parsed = parse_source_rows(&rows, ncols, &pk_idx)?;
        let upper = parsed
            .last()
            .map(|r| r.pk.clone())
            .ok_or_else(|| AppError::InternalError("批次解析为空".to_string()))?;
        let lower = cursor.clone();

        if !opts.single_transaction {
            begin(tgt, opts.defer_constraints).await?;
        }
        let result: Result<(), AppError> = async {
            let sql = upsert_sql(plan, parsed.len());
            let binds = parsed
                .iter()
                .flat_map(|r| r.values.iter().cloned().map(Bind::Text))
                .collect::<Vec<_>>();
            execute_sql(tgt, &sql, &binds).await?;
            if opts.mode == SyncMode::Mirror {
                mirror_delete_range(tgt, plan, lower.as_deref(), &upper, &parsed).await?;
            }
            Ok(())
        }
        .await;
        if !opts.single_transaction {
            match &result {
                Ok(()) => commit(tgt).await?,
                Err(_) => rollback(tgt).await,
            }
        }
        result?;

        rows_copied += parsed.len() as u64;
        batches += 1;
        cursor = Some(upper.clone());
        table.rows_copied = rows_copied;
        table.batches = batches;
        checkpoint.tables.insert(
            plan.display_name.clone(),
            TableCheckpoint {
                last_pk: upper,
                rows_copied,
                completed: false,
            },
        );
        if save_checkpoint {
            // 先 commit（上面已完成）再写断点：中断时最多重放一个批次，upsert 幂等。
            store.save(checkpoint)?;
        }
        progress(PgDataSyncProgress {
            stage: "load".to_string(),
            table: Some(plan.display_name.clone()),
            table_index,
            table_total,
            rows_copied,
            total_rows: Some(plan.source_count),
            message: format!("已复制 {rows_copied}/{} 行", plan.source_count),
        });
    }

    // mirror 尾部：删除源表末尾之后的多余目标行；源表为空时清空目标表。
    if opts.mode == SyncMode::Mirror {
        if !opts.single_transaction {
            begin(tgt, opts.defer_constraints).await?;
        }
        let result: Result<(), AppError> = async {
            match &cursor {
                Some(last) => {
                    let binds = last
                        .iter()
                        .map(|v| Bind::Text(Some(v.clone())))
                        .collect::<Vec<_>>();
                    execute_sql(tgt, &delete_after_pk_sql(plan), &binds).await?;
                }
                None => {
                    execute_sql(tgt, &delete_all_sql(plan), &[]).await?;
                }
            }
            Ok(())
        }
        .await;
        if !opts.single_transaction {
            match &result {
                Ok(()) => commit(tgt).await?,
                Err(_) => rollback(tgt).await,
            }
        }
        result?;
    }

    table.status = "completed".to_string();
    checkpoint.tables.insert(
        plan.display_name.clone(),
        TableCheckpoint {
            last_pk: cursor.unwrap_or_default(),
            rows_copied,
            completed: true,
        },
    );
    if save_checkpoint {
        store.save(checkpoint)?;
    }
    progress(PgDataSyncProgress {
        stage: "load".to_string(),
        table: Some(plan.display_name.clone()),
        table_index,
        table_total,
        rows_copied,
        total_rows: Some(plan.source_count),
        message: "表同步完成".to_string(),
    });
    Ok(())
}

fn parse_source_rows(
    rows: &[PgRow],
    ncols: usize,
    pk_idx: &[usize],
) -> Result<Vec<SourceRow>, AppError> {
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let mut values = Vec::with_capacity(ncols);
        for i in 0..ncols {
            values.push(row.try_get::<Option<String>, _>(i)?);
        }
        let mut pk = Vec::with_capacity(pk_idx.len());
        for &i in pk_idx {
            match &values[i] {
                Some(v) => pk.push(v.clone()),
                None => {
                    return Err(AppError::InternalError(
                        "主键列出现 NULL，无法按主键分批".to_string(),
                    ))
                }
            }
        }
        out.push(SourceRow { values, pk });
    }
    Ok(out)
}

async fn mirror_delete_range(
    conn: &mut PgConnection,
    plan: &PgTablePlan,
    lower: Option<&[String]>,
    upper: &[String],
    source_rows: &[SourceRow],
) -> Result<(), AppError> {
    let sql = range_pk_select_sql(plan, lower.is_some(), true);
    let mut binds = Vec::new();
    if let Some(lower) = lower {
        for v in lower {
            binds.push(Bind::Text(Some(v.clone())));
        }
    }
    for v in upper {
        binds.push(Bind::Text(Some(v.clone())));
    }
    let rows = fetch_all(conn, &sql, &binds).await?;

    let source_set: BTreeSet<Vec<String>> = source_rows.iter().map(|r| r.pk.clone()).collect();
    let pk_len = plan.pk_columns.len();
    for row in rows {
        let mut pk = Vec::with_capacity(pk_len);
        for i in 0..pk_len {
            pk.push(row.try_get::<String, _>(i)?);
        }
        if !source_set.contains(&pk) {
            let binds = pk
                .iter()
                .map(|v| Bind::Text(Some(v.clone())))
                .collect::<Vec<_>>();
            execute_sql(conn, &delete_by_pk_sql(plan), &binds).await?;
        }
    }
    Ok(())
}

// ── 校验 ───────────────────────────────────────────────

async fn verify_all(
    src: &mut PgConnection,
    tgt: &mut PgConnection,
    plans: &[PgTablePlan],
    chunk_size: usize,
    tables: &mut [PgTableReport],
) {
    for plan in plans {
        let status = table_mut(tables, &plan.display_name).status.clone();
        if status == "failed" {
            let table = table_mut(tables, &plan.display_name);
            table.verification = "unverified".to_string();
            continue;
        }
        let result = verify_one(src, tgt, plan, chunk_size).await;
        let table = table_mut(tables, &plan.display_name);
        match result {
            Ok((source_count, target_count, source_hash, target_hash)) => {
                table.source_rows = Some(source_count);
                table.target_rows = Some(target_count);
                if plan.pk_columns.is_empty() {
                    table.checksum_equal = None;
                    table.verification = if source_count == target_count {
                        "count_only".to_string()
                    } else {
                        "failed".to_string()
                    };
                } else {
                    let equal = source_count == target_count
                        && source_hash.is_some()
                        && source_hash == target_hash;
                    table.checksum_equal = Some(equal);
                    table.verification = if equal {
                        "verified".to_string()
                    } else {
                        "failed".to_string()
                    };
                }
            }
            Err(e) => {
                table.verification = "unverified".to_string();
                table.notes.push(format!("校验执行失败，未验证：{e}"));
            }
        }
    }
}

async fn verify_one(
    src: &mut PgConnection,
    tgt: &mut PgConnection,
    plan: &PgTablePlan,
    chunk_size: usize,
) -> Result<(u64, u64, Option<String>, Option<String>), AppError> {
    let (source_count, source_hash) = compute_digest(src, plan, chunk_size).await?;
    let (target_count, target_hash) = compute_digest(tgt, plan, chunk_size).await?;
    Ok((source_count, target_count, source_hash, target_hash))
}

async fn compute_digest(
    conn: &mut PgConnection,
    plan: &PgTablePlan,
    chunk_size: usize,
) -> Result<(u64, Option<String>), AppError> {
    if plan.pk_columns.is_empty() {
        let count: i64 =
            sqlx::query_scalar(&format!("SELECT count(*)::bigint FROM {}", plan.qualified()))
                .fetch_one(&mut *conn)
                .await?;
        return Ok((count.max(0) as u64, None));
    }
    let order = plan
        .pk_columns
        .iter()
        .map(|c| quote_ident_pg(c))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "WITH ordered AS ( \
           SELECT md5(t::text) AS rh, row_number() OVER (ORDER BY {order}) AS rn \
           FROM {} t \
         ) \
         SELECT ((rn - 1) / $1)::bigint AS chunk, count(*)::bigint AS c, \
                md5(string_agg(rh, '' ORDER BY rn)) AS h \
         FROM ordered GROUP BY ((rn - 1) / $1) ORDER BY chunk",
        plan.qualified()
    );
    let rows = fetch_all(conn, &sql, &[Bind::Int(chunk_size.max(1) as i64)]).await?;
    let mut total: i64 = 0;
    let mut accumulated = String::new();
    for row in rows {
        let count: i64 = row.try_get("c")?;
        let hash: Option<String> = row.try_get("h")?;
        total += count;
        if let Some(h) = hash {
            accumulated.push_str(&h);
        }
    }
    let mut hasher = Sha256::new();
    hasher.update(accumulated.as_bytes());
    Ok((total.max(0) as u64, Some(format!("{:x}", hasher.finalize()))))
}

pub fn overall_verification_status(tables: &[PgTableReport]) -> String {
    let mut any_failed = false;
    let mut any_unverified = false;
    let mut any_count_only = false;
    let mut any_verified = false;
    for t in tables {
        match t.verification.as_str() {
            "failed" => any_failed = true,
            "unverified" => any_unverified = true,
            "count_only" => any_count_only = true,
            "verified" => any_verified = true,
            _ => {}
        }
    }
    if any_failed {
        "failed".to_string()
    } else if any_unverified {
        "unverified".to_string()
    } else if any_count_only {
        "count_only".to_string()
    } else if any_verified {
        "verified".to_string()
    } else {
        "unverified".to_string()
    }
}
