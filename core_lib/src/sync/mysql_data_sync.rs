//! MySQL/MariaDB 分阶段数据同步引擎。
//!
//! 与 PostgreSQL 引擎共用 `pipeline` 的阶段顺序、选项校验、作业指纹、报告模型
//! 与 `checkpoint` 断点文件，只是把 SQL/DDL 换成 MySQL 方言：
//! Plan → 建基础结构 → 按主键 keyset 分批灌数据 → 建索引/外键 → AUTO_INCREMENT
//! 对齐 → 行数与分块哈希校验。
//!
//! 可靠性语义（MySQL DDL 会隐式提交）：
//! - 数据批次逐批提交，失败批次回滚，已提交批次与断点保留，可续跑；
//! - 建表/索引/外键/`ALTER TABLE ... AUTO_INCREMENT` 无法回滚，结果里通过
//!   `non_transactional_tables` 与 warnings 如实体现；
//! - 因此 MySQL/MariaDB 不支持 `single_transaction`（无法保证 job 级原子性），
//!   显式拒绝而不是给出错误的“整 job 可回滚”承诺。

use crate::config::DbType;
use crate::db::DbClient;
use crate::error::AppError;
use crate::sync::checkpoint::{Checkpoint, CheckpointStore, TableCheckpoint};
use crate::sync::mysql_engine::SyncMode;
use crate::sync::mysql_plan::{
    add_foreign_key_sql, batch_select_sql, create_index_sql, delete_after_pk_sql, delete_by_pk_sql,
    delete_all_sql, full_select_sql, strip_secondary_indexes_and_fks, target_pk_select_sql,
    upsert_dialect, upsert_sql, validate_plan, MySqlColumnSpec, MySqlForeignKeySpec,
    MySqlIndexSpec, MySqlTablePlan, UpsertDialect,
};
use crate::sync::pg_data_sync::{
    PgDataSyncOptions, PgDataSyncProgress, PgDataSyncReport, PgTableReport,
};
use crate::sync::pipeline;
use sha2::{Digest, Sha256};
use sqlx::mysql::{MySqlArguments, MySqlRow};
use sqlx::query::Query;
use sqlx::{MySql, MySqlConnection, MySqlPool, Row};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone)]
enum Bind {
    Text(Option<String>),
    Int(i64),
}

fn bind_all<'q>(
    mut q: Query<'q, MySql, MySqlArguments>,
    binds: &[Bind],
) -> Query<'q, MySql, MySqlArguments> {
    for b in binds {
        q = match b {
            Bind::Text(v) => q.bind(v.clone()),
            Bind::Int(v) => q.bind(*v),
        };
    }
    q
}

async fn execute_sql(
    conn: &mut MySqlConnection,
    sql: &str,
    binds: &[Bind],
) -> Result<u64, AppError> {
    Ok(bind_all(sqlx::query(sql), binds)
        .execute(&mut *conn)
        .await?
        .rows_affected())
}

async fn fetch_all(
    conn: &mut MySqlConnection,
    sql: &str,
    binds: &[Bind],
) -> Result<Vec<MySqlRow>, AppError> {
    Ok(bind_all(sqlx::query(sql), binds)
        .fetch_all(&mut *conn)
        .await?)
}

/// MySQL 8 的 `START TRANSACTION` 不在 prepared statement 协议支持列表里（错误 1295）。
/// `Execute` 对 `&str` 的实现在 sqlx 里不带 statement（statement() == None），
/// 会走文本协议，因此这里用 `Executor::execute` 而不是 `sqlx::query`。
async fn begin(conn: &mut MySqlConnection) -> Result<(), AppError> {
    sqlx::Executor::execute(&mut *conn, "START TRANSACTION").await?;
    Ok(())
}

async fn commit(conn: &mut MySqlConnection) -> Result<(), AppError> {
    sqlx::Executor::execute(&mut *conn, "COMMIT").await?;
    Ok(())
}

async fn rollback(conn: &mut MySqlConnection) {
    let _ = sqlx::Executor::execute(&mut *conn, "ROLLBACK").await;
}

fn table_mut<'a>(tables: &'a mut [PgTableReport], name: &str) -> &'a mut PgTableReport {
    tables
        .iter_mut()
        .find(|t| t.table_name == name)
        .expect("table report must exist")
}

fn q(name: &str) -> String {
    crate::sql_util::quote_ident_mysql(name)
}

pub struct MySqlStagedDataSyncEngine;

impl MySqlStagedDataSyncEngine {
    pub async fn run(
        source: DbClient,
        target: DbClient,
        mut options: PgDataSyncOptions,
        progress: Box<dyn Fn(PgDataSyncProgress) + Send + Sync>,
    ) -> Result<PgDataSyncReport, AppError> {
        if !matches!(source.db_type, DbType::MySQL | DbType::MariaDB) {
            return Err(AppError::BadRequest(format!(
                "MySQL 数据同步要求源库为 MySQL/MariaDB，当前为 {:?}",
                source.db_type
            )));
        }
        if !matches!(target.db_type, DbType::MySQL | DbType::MariaDB) {
            return Err(AppError::BadRequest(format!(
                "MySQL 数据同步要求目标库为 MySQL/MariaDB，当前为 {:?}",
                target.db_type
            )));
        }
        pipeline::validate_options(&options)?;
        if options.single_transaction {
            return Err(AppError::BadRequest(
                "MySQL/MariaDB 的 CREATE TABLE / CREATE INDEX / ADD FOREIGN KEY / ALTER TABLE AUTO_INCREMENT 都会隐式提交，无法保证 job 级原子性；请使用默认的逐批提交 + checkpoint 续跑"
                    .to_string(),
            ));
        }
        options.batch_size = options.batch_size.max(1);

        let src_pool: &MySqlPool = source.pool.mysql()?;
        let tgt_pool: &MySqlPool = target.pool.mysql()?;
        let mut src = src_pool.acquire().await?;
        let mut tgt = tgt_pool.acquire().await?;

        // 统一字符集与时区：TIMESTAMP 的文本往返必须在同一时区下进行。
        for conn in [&mut src, &mut tgt] {
            sqlx::query("SET NAMES utf8mb4").execute(&mut **conn).await?;
            sqlx::query("SET time_zone = '+00:00'")
                .execute(&mut **conn)
                .await?;
        }

        let target_version: String = sqlx::query_scalar("SELECT VERSION()")
            .fetch_one(&mut *tgt)
            .await?;
        let dialect = upsert_dialect(&target.db_type, &target_version);

        let src_db = current_database(&mut src).await?;
        let tgt_db = current_database(&mut tgt).await?;
        let plans = build_plans(&mut src, &mut tgt, &options, &src_db, &tgt_db).await?;
        for plan in &plans {
            for col in &plan.columns {
                if col.is_unsupported_spatial() {
                    return Err(AppError::BadRequest(format!(
                        "表 {} 的列 {} 是空间类型 {}，当前 MySQL 数据同步未实现 WKB 往返，拒绝以避免静默丢数据",
                        plan.display_name, col.name, col.column_type
                    )));
                }
            }
            validate_plan(plan, options.allow_no_pk)?;
            if !plan.target_exists && !options.create_missing_tables {
                return Err(AppError::BadRequest(format!(
                    "目标库缺少表 {}，且 create_missing_tables=false；请先做结构同步或显式允许建表",
                    plan.display_name
                )));
            }
        }

        let table_names: Vec<String> = plans.iter().map(|p| p.display_name.clone()).collect();
        let edges: Vec<(String, String)> = plans
            .iter()
            .flat_map(|p| {
                p.foreign_keys
                    .iter()
                    .filter(|fk| {
                        fk.referenced_schema.is_empty() || fk.referenced_schema == src_db
                    })
                    .map(move |fk| (p.display_name.clone(), fk.referenced_table.clone()))
            })
            .collect();
        let ordered_names = pipeline::order_tables_or_reject(
            &table_names,
            &edges,
            options.defer_constraints,
            "如确认可显式设置 defer_constraints=true，MySQL 下会改为在数据之后创建外键，不使用会话级 SET FOREIGN_KEY_CHECKS=0（该开关会跳过对已有数据的校验，风险更高）",
        )?;
        let by_name: BTreeMap<String, MySqlTablePlan> = plans
            .into_iter()
            .map(|p| (p.display_name.clone(), p))
            .collect();
        let ordered: Vec<MySqlTablePlan> = ordered_names
            .iter()
            .filter_map(|name| by_name.get(name).cloned())
            .collect();
        let all_have_pk = ordered.iter().all(|p| !p.pk_columns.is_empty());

        let mode_label = match options.mode {
            SyncMode::Mirror => "mirror",
            SyncMode::UpsertOnly => "upsert_only",
        };
        let key = pipeline::compute_job_key_for_tables(
            &format!("mysql:{:?}:{}", source.db_type, src_db),
            &format!("mysql:{:?}:{}", target.db_type, tgt_db),
            mode_label,
            options.batch_size,
            &ordered.iter().map(|p| p.job_key_table()).collect::<Vec<_>>(),
        );

        let store = CheckpointStore::new(&options.checkpoint_dir);
        let mut checkpoint = if options.resume {
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
                    source_rows: Some(p.source_count),
                    target_rows: None,
                    checksum_equal: None,
                    verification: "unverified".to_string(),
                    sequences_aligned: vec![],
                    error: None,
                    notes: p.warnings.clone(),
                })
                .collect(),
            total_rows: 0,
            atomic: false,
            rolled_back: false,
            rollback_scope: "batch".to_string(),
            resumable: false,
            verification_status: "unverified".to_string(),
            non_transactional_tables: vec![],
            deferred_constraints: options.defer_constraints,
            checkpoint_file: None,
            warnings: vec![
                "MySQL/MariaDB 的 DDL（建表/索引/外键/AUTO_INCREMENT 对齐）会隐式提交，这些阶段不可回滚；只有数据批次可回滚。已提交批次与断点会保留。"
                    .to_string(),
                "MySQL 的 ON DUPLICATE KEY UPDATE 在任意唯一键冲突时都会触发，不仅限于主键；校验阶段会做行数与分块哈希比对。"
                    .to_string(),
            ],
            error: None,
            started_at: chrono::Utc::now().timestamp(),
            finished_at: 0,
        };
        if matches!(target.db_type, DbType::MariaDB) {
            report.warnings.push(
                "目标为 MariaDB：JSON 实际是 LONGTEXT，不执行 MySQL 的 CAST(? AS JSON)，按原文本写入。"
                    .to_string(),
            );
        }
        if options.defer_constraints {
            report.warnings.push(
                "defer_constraints=true：MySQL 下通过“数据灌完后再建外键”实现，未使用 SET FOREIGN_KEY_CHECKS=0；外键创建时会校验已有数据，失败会如实报错。"
                    .to_string(),
            );
        }
        if options.resume {
            report.warnings.push(
                "resume=true：会对已存在的目标表补齐缺失的索引/外键（仅补建，不删除已有结构）。"
                    .to_string(),
            );
        }
        if options.mode == SyncMode::Mirror {
            report.warnings.push(
                "mirror 已显式确认：会删除目标库中源库不存在的主键行；删除按批次与源行集合比对。"
                    .to_string(),
            );
        }

        progress(pipeline::stage_progress(
            pipeline::STAGE_PLAN,
            format!("Plan 完成：{} 张表", ordered.len()),
            ordered.len(),
            0,
        ));

        let mut ddl_tables: BTreeSet<String> = BTreeSet::new();
        let result = execute_pipeline(
            &mut src,
            &mut tgt,
            &ordered,
            &options,
            &store,
            &mut checkpoint,
            &mut report,
            dialect,
            &source.db_type,
            &target.db_type,
            &mut ddl_tables,
            &progress,
        )
        .await;

        if let Err(e) = result {
            let message = e.to_string();
            for t in &mut report.tables {
                if t.status == "pending" {
                    t.status = "skipped".to_string();
                }
                if t.status == "running" {
                    t.status = "failed".to_string();
                    if t.error.is_none() {
                        t.error = Some(message.clone());
                    }
                }
                if t.verification == "unverified" && t.status == "skipped" {
                    t.verification = "skipped".to_string();
                }
            }
            report.error = Some(message);
        }

        report.non_transactional_tables = ddl_tables.into_iter().collect();
        pipeline::finalize_report(&mut report, &options, &store, &key, all_have_pk);
        Ok(report)
    }
}

async fn current_database(conn: &mut MySqlConnection) -> Result<String, AppError> {
    let db: Option<String> = sqlx::query_scalar("SELECT DATABASE()")
        .fetch_one(&mut *conn)
        .await?;
    db.ok_or_else(|| {
        AppError::BadRequest(
            "连接未选择 database（URL 需要包含数据库名），无法执行 MySQL 数据同步".to_string(),
        )
    })
}

// ── Plan ───────────────────────────────────────────────

async fn build_plans(
    src: &mut MySqlConnection,
    tgt: &mut MySqlConnection,
    options: &PgDataSyncOptions,
    src_db: &str,
    tgt_db: &str,
) -> Result<Vec<MySqlTablePlan>, AppError> {
    let table_names: Vec<String> = match &options.tables {
        Some(tables) => {
            let mut out = Vec::new();
            for raw in tables {
                let trimmed = raw.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let name = match trimmed.split_once('.') {
                    Some((schema, name)) => {
                        if schema != src_db {
                            return Err(AppError::BadRequest(format!(
                                "表 {trimmed} 指定了源库 schema {schema}，但当前连接的是 {src_db}；MySQL 数据同步当前只支持当前 database 下的表"
                            )));
                        }
                        name.to_string()
                    }
                    None => trimmed.to_string(),
                };
                if !out.contains(&name) {
                    out.push(name);
                }
            }
            out
        }
        None => sqlx::query_scalar::<_, String>(
            "SELECT CAST(TABLE_NAME AS CHAR) AS TABLE_NAME FROM information_schema.TABLES WHERE TABLE_SCHEMA = ? AND TABLE_TYPE = 'BASE TABLE' ORDER BY TABLE_NAME",
        )
        .bind(src_db)
        .fetch_all(&mut *src)
        .await?,
    };

    if table_names.is_empty() {
        return Err(AppError::BadRequest(
            "源库当前 database 下没有可同步的普通表".to_string(),
        ));
    }

    let mut plans = Vec::with_capacity(table_names.len());
    for name in table_names {
        plans.push(build_table_plan(src, tgt, &name, src_db, tgt_db).await?);
    }
    plans.sort_by(|a, b| a.display_name.cmp(&b.display_name));
    Ok(plans)
}

async fn build_table_plan(
    src: &mut MySqlConnection,
    tgt: &mut MySqlConnection,
    table: &str,
    src_db: &str,
    tgt_db: &str,
) -> Result<MySqlTablePlan, AppError> {
    let columns = load_columns(src, src_db, table).await?;
    if columns.is_empty() {
        return Err(AppError::BadRequest(format!(
            "源表 {src_db}.{table} 不存在或没有列"
        )));
    }
    let pk_columns = load_primary_key(src, src_db, table).await?;
    let mut foreign_keys = load_foreign_keys(src, src_db, table).await?;
    // 同一 database 内的外键在目标端也用当前 database 的非限定名，
    // 这样源/目标 database 名称不同也能正确重建。
    for fk in &mut foreign_keys {
        if fk.referenced_schema == src_db {
            fk.referenced_schema.clear();
        }
    }
    let indexes = load_indexes(src, src_db, table).await?;
    let source_count: i64 =
        sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {}", q(table)))
            .fetch_one(&mut *src)
            .await?;

    let target_exists: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM information_schema.TABLES WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND TABLE_TYPE = 'BASE TABLE'",
    )
    .bind(tgt_db)
    .bind(table)
    .fetch_one(&mut *tgt)
    .await?;
    let target_exists = target_exists > 0;
    let (target_columns, target_column_types, target_auto_increment_columns) =
        load_target_columns(tgt, tgt_db, table).await?;
    let target_pk_columns = if target_exists {
        load_primary_key(tgt, tgt_db, table).await?
    } else {
        vec![]
    };

    let row = sqlx::query(&format!("SHOW CREATE TABLE {}", q(table)))
        .fetch_one(&mut *src)
        .await?;
    let ddl: String = row.try_get(1)?;
    let create_table_ddl = strip_secondary_indexes_and_fks(&ddl)?;

    let mut warnings = Vec::new();
    for col in &columns {
        if col.is_generated() {
            warnings.push(format!(
                "生成列 {} 不写入，由目标库按相同表达式计算",
                col.name
            ));
        }
        if let Some((_, target_type)) = target_column_types
            .iter()
            .find(|(name, _)| name == &col.name)
        {
            if target_type.to_ascii_lowercase() != col.column_type.to_ascii_lowercase() {
                warnings.push(format!(
                    "目标列 {} 类型 {} 与源 {} 不一致；将按源文本写入，校验阶段会暴露精度/时区差异",
                    col.name, target_type, col.column_type
                ));
            }
        }
        if col.is_auto_increment()
            && target_exists
            && !target_auto_increment_columns.contains(&col.name)
        {
            warnings.push(format!(
                "目标列 {} 不是 AUTO_INCREMENT，无法执行序列对齐；后续插入可能撞主键",
                col.name
            ));
        }
    }
    for index in &indexes {
        if index.columns.is_empty() {
            warnings.push(format!(
                "索引 {} 使用了当前无法重建的表达式，已跳过；数据校验不受影响",
                index.name
            ));
        }
    }

    Ok(MySqlTablePlan {
        display_name: table.to_string(),
        name: table.to_string(),
        columns,
        pk_columns,
        target_exists,
        target_columns,
        target_column_types,
        target_pk_columns,
        target_auto_increment_columns,
        source_count: source_count.max(0) as u64,
        foreign_keys,
        indexes,
        create_table_ddl,
        warnings,
    })
}

async fn load_columns(
    conn: &mut MySqlConnection,
    db: &str,
    table: &str,
) -> Result<Vec<MySqlColumnSpec>, AppError> {
    // information_schema 的字符列是 utf8mb3，sqlx 会按 BLOB 解码；统一 CAST AS CHAR
    // 拿回 utf8 字符串，避免 “String 与 BLOB 不兼容” 的解码错误。
    let rows = sqlx::query(
        "SELECT CAST(COLUMN_NAME AS CHAR) AS COLUMN_NAME, CAST(COLUMN_TYPE AS CHAR) AS COLUMN_TYPE, \
         CAST(DATA_TYPE AS CHAR) AS DATA_TYPE, CAST(IS_NULLABLE AS CHAR) AS IS_NULLABLE, \
         CAST(COLUMN_DEFAULT AS CHAR) AS COLUMN_DEFAULT, CAST(EXTRA AS CHAR) AS EXTRA, \
         CAST(GENERATION_EXPRESSION AS CHAR) AS GENERATION_EXPRESSION, \
         CAST(CHARACTER_SET_NAME AS CHAR) AS CHARACTER_SET_NAME, \
         CAST(COLLATION_NAME AS CHAR) AS COLLATION_NAME, \
         CAST(COLUMN_COMMENT AS CHAR) AS COLUMN_COMMENT \
         FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY ORDINAL_POSITION",
    )
    .bind(db)
    .bind(table)
    .fetch_all(&mut *conn)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let extra: String = row.try_get("EXTRA")?;
        let generation_expression: Option<String> = row.try_get("GENERATION_EXPRESSION").ok();
        out.push(MySqlColumnSpec {
            name: row.try_get("COLUMN_NAME")?,
            column_type: row.try_get("COLUMN_TYPE")?,
            data_type: row.try_get::<String, _>("DATA_TYPE")?.to_ascii_lowercase(),
            nullable: row.try_get::<String, _>("IS_NULLABLE")? == "YES",
            default: row
                .try_get::<Option<String>, _>("COLUMN_DEFAULT")
                .ok()
                .flatten(),
            extra,
            generation_expression,
            character_set: row
                .try_get::<Option<String>, _>("CHARACTER_SET_NAME")
                .ok()
                .flatten(),
            collation: row
                .try_get::<Option<String>, _>("COLLATION_NAME")
                .ok()
                .flatten(),
            comment: row.try_get::<String, _>("COLUMN_COMMENT").unwrap_or_default(),
        });
    }
    Ok(out)
}

async fn load_primary_key(
    conn: &mut MySqlConnection,
    db: &str,
    table: &str,
) -> Result<Vec<String>, AppError> {
    Ok(sqlx::query_scalar::<_, String>(
        "SELECT CAST(COLUMN_NAME AS CHAR) AS COLUMN_NAME FROM information_schema.KEY_COLUMN_USAGE \
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND CONSTRAINT_NAME = 'PRIMARY' ORDER BY ORDINAL_POSITION",
    )
    .bind(db)
    .bind(table)
    .fetch_all(&mut *conn)
    .await?)
}

async fn load_target_columns(
    conn: &mut MySqlConnection,
    db: &str,
    table: &str,
) -> Result<(Vec<String>, Vec<(String, String)>, Vec<String>), AppError> {
    let rows = sqlx::query(
        "SELECT CAST(COLUMN_NAME AS CHAR) AS COLUMN_NAME, CAST(COLUMN_TYPE AS CHAR) AS COLUMN_TYPE, \
         CAST(EXTRA AS CHAR) AS EXTRA FROM information_schema.COLUMNS \
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY ORDINAL_POSITION",
    )
    .bind(db)
    .bind(table)
    .fetch_all(&mut *conn)
    .await?;
    let mut names = Vec::with_capacity(rows.len());
    let mut types = Vec::with_capacity(rows.len());
    let mut auto_inc = Vec::new();
    for row in rows {
        let name: String = row.try_get("COLUMN_NAME")?;
        let type_text: String = row.try_get("COLUMN_TYPE")?;
        let extra: String = row.try_get("EXTRA")?;
        if extra.to_ascii_lowercase().contains("auto_increment") {
            auto_inc.push(name.clone());
        }
        names.push(name.clone());
        types.push((name, type_text));
    }
    Ok((names, types, auto_inc))
}

async fn load_foreign_keys(
    conn: &mut MySqlConnection,
    db: &str,
    table: &str,
) -> Result<Vec<MySqlForeignKeySpec>, AppError> {
    let rows = sqlx::query(
        "SELECT CAST(k.CONSTRAINT_NAME AS CHAR) AS CONSTRAINT_NAME, \
                CAST(k.COLUMN_NAME AS CHAR) AS COLUMN_NAME, \
                CAST(k.REFERENCED_TABLE_SCHEMA AS CHAR) AS REFERENCED_TABLE_SCHEMA, \
                CAST(k.REFERENCED_TABLE_NAME AS CHAR) AS REFERENCED_TABLE_NAME, \
                CAST(k.REFERENCED_COLUMN_NAME AS CHAR) AS REFERENCED_COLUMN_NAME, \
                CAST(r.DELETE_RULE AS CHAR) AS DELETE_RULE, \
                CAST(r.UPDATE_RULE AS CHAR) AS UPDATE_RULE \
         FROM information_schema.KEY_COLUMN_USAGE k \
         JOIN information_schema.REFERENTIAL_CONSTRAINTS r \
           ON r.CONSTRAINT_SCHEMA = k.CONSTRAINT_SCHEMA AND r.CONSTRAINT_NAME = k.CONSTRAINT_NAME \
         WHERE k.TABLE_SCHEMA = ? AND k.TABLE_NAME = ? AND k.REFERENCED_TABLE_NAME IS NOT NULL \
         ORDER BY k.CONSTRAINT_NAME, k.ORDINAL_POSITION",
    )
    .bind(db)
    .bind(table)
    .fetch_all(&mut *conn)
    .await?;
    let mut map: BTreeMap<String, MySqlForeignKeySpec> = BTreeMap::new();
    for row in rows {
        let name: String = row.try_get("CONSTRAINT_NAME")?;
        let entry = map.entry(name.clone()).or_insert_with(|| MySqlForeignKeySpec {
            name,
            columns: vec![],
            referenced_schema: row
                .try_get::<Option<String>, _>("REFERENCED_TABLE_SCHEMA")
                .ok()
                .flatten()
                .unwrap_or_default(),
            referenced_table: row
                .try_get::<Option<String>, _>("REFERENCED_TABLE_NAME")
                .ok()
                .flatten()
                .unwrap_or_default(),
            referenced_columns: vec![],
            on_delete: row.try_get("DELETE_RULE").unwrap_or_default(),
            on_update: row.try_get("UPDATE_RULE").unwrap_or_default(),
        });
        entry.columns.push(row.try_get("COLUMN_NAME")?);
        entry
            .referenced_columns
            .push(row.try_get("REFERENCED_COLUMN_NAME")?);
    }
    Ok(map.into_values().collect())
}

async fn load_indexes(
    conn: &mut MySqlConnection,
    db: &str,
    table: &str,
) -> Result<Vec<MySqlIndexSpec>, AppError> {
    let rows = sqlx::query(
        "SELECT CAST(INDEX_NAME AS CHAR) AS INDEX_NAME, NON_UNIQUE, \
         CAST(COLUMN_NAME AS CHAR) AS COLUMN_NAME, SUB_PART, CAST(INDEX_TYPE AS CHAR) AS INDEX_TYPE \
         FROM information_schema.STATISTICS \
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND INDEX_NAME <> 'PRIMARY' \
         ORDER BY INDEX_NAME, SEQ_IN_INDEX",
    )
    .bind(db)
    .bind(table)
    .fetch_all(&mut *conn)
    .await?;
    let mut map: BTreeMap<String, MySqlIndexSpec> = BTreeMap::new();
    for row in rows {
        let name: String = row.try_get("INDEX_NAME")?;
        let non_unique: i64 = row.try_get("NON_UNIQUE")?;
        let index_type: String = row.try_get("INDEX_TYPE")?;
        let entry = map.entry(name.clone()).or_insert_with(|| MySqlIndexSpec {
            name,
            unique: non_unique == 0,
            index_type,
            columns: vec![],
        });
        if let Ok(Some(column)) = row.try_get::<Option<String>, _>("COLUMN_NAME") {
            let prefix: Option<u32> = row
                .try_get::<Option<u64>, _>("SUB_PART")
                .ok()
                .flatten()
                .and_then(|v| u32::try_from(v).ok());
            entry.columns.push((column, prefix, String::new()));
        }
    }
    Ok(map.into_values().collect())
}

// ── 阶段编排 ───────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn execute_pipeline(
    src: &mut MySqlConnection,
    tgt: &mut MySqlConnection,
    plans: &[MySqlTablePlan],
    opts: &PgDataSyncOptions,
    store: &CheckpointStore,
    checkpoint: &mut Checkpoint,
    report: &mut PgDataSyncReport,
    dialect: UpsertDialect,
    source_type: &DbType,
    target_type: &DbType,
    ddl_tables: &mut BTreeSet<String>,
    progress: &(dyn Fn(PgDataSyncProgress) + Send + Sync),
) -> Result<(), AppError> {
    create_structure(tgt, plans, opts, store, checkpoint, report, ddl_tables, progress).await?;

    for (idx, plan) in plans.iter().enumerate() {
        let table = table_mut(&mut report.tables, &plan.display_name);
        if let Err(e) = load_table(
            src,
            tgt,
            plan,
            opts,
            checkpoint,
            store,
            table,
            idx,
            plans.len(),
            dialect,
            source_type,
            target_type,
            progress,
        )
        .await
        {
            table.status = "failed".to_string();
            table.error = Some(e.to_string());
            table.verification = "unverified".to_string();
            report.rolled_back = true;
            report.warnings.push(format!(
                "表 {} 数据阶段失败：失败批次已回滚；此前已提交批次与断点保留（若失败发生在提交后/断点写入时，已提交批次仍保留）",
                plan.display_name
            ));
            return Err(e);
        }
    }

    create_indexes_and_fks(tgt, plans, opts, checkpoint, report, ddl_tables, progress).await?;
    align_sequences(tgt, plans, checkpoint, report, ddl_tables, progress).await?;
    verify_all(
        src,
        tgt,
        plans,
        opts.batch_size,
        source_type,
        target_type,
        report,
        progress,
    )
    .await;

    if report.error.is_none() {
        let failed = report
            .tables
            .iter()
            .any(|t| t.verification == "failed");
        if failed {
            report.error = Some(
                "行数或分块哈希校验未通过，请查看逐表 verification 字段".to_string(),
            );
        }
    }
    Ok(())
}

async fn create_structure(
    tgt: &mut MySqlConnection,
    plans: &[MySqlTablePlan],
    opts: &PgDataSyncOptions,
    store: &CheckpointStore,
    checkpoint: &mut Checkpoint,
    report: &mut PgDataSyncReport,
    ddl_tables: &mut BTreeSet<String>,
    progress: &(dyn Fn(PgDataSyncProgress) + Send + Sync),
) -> Result<(), AppError> {
    progress(pipeline::stage_progress(
        pipeline::STAGE_STRUCTURE,
        "正在创建缺失的基础结构",
        plans.len(),
        0,
    ));
    for (idx, plan) in plans.iter().enumerate() {
        if plan.target_exists {
            continue;
        }
        if !opts.create_missing_tables {
            return Err(AppError::BadRequest(format!(
                "目标库缺少表 {}，且 create_missing_tables=false",
                plan.display_name
            )));
        }
        execute_sql(tgt, &plan.create_table_ddl, &[]).await?;
        ddl_tables.insert(plan.display_name.clone());
        checkpoint.created_tables.insert(plan.display_name.clone());
        store.save(checkpoint)?;
        let table = table_mut(&mut report.tables, &plan.display_name);
        table.notes.push(format!(
            "已创建目标表 {}（MySQL DDL 隐式提交，此变更不可回滚）",
            plan.display_name
        ));
        progress(pipeline::stage_progress(
            pipeline::STAGE_STRUCTURE,
            format!("已创建表 {}", plan.display_name),
            idx + 1,
            0,
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn load_table(
    src: &mut MySqlConnection,
    tgt: &mut MySqlConnection,
    plan: &MySqlTablePlan,
    opts: &PgDataSyncOptions,
    checkpoint: &mut Checkpoint,
    store: &CheckpointStore,
    table: &mut PgTableReport,
    table_index: usize,
    table_total: usize,
    dialect: UpsertDialect,
    source_type: &DbType,
    target_type: &DbType,
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
            stage: pipeline::STAGE_LOAD.to_string(),
            table: Some(plan.display_name.clone()),
            table_index,
            table_total,
            rows_copied: entry.rows_copied,
            total_rows: Some(plan.source_count),
            message: "断点显示该表已完成，跳过数据写入".to_string(),
        });
        return Ok(());
    }

    table.status = "running".to_string();
    progress(PgDataSyncProgress {
        stage: pipeline::STAGE_LOAD.to_string(),
        table: Some(plan.display_name.clone()),
        table_index,
        table_total,
        rows_copied: entry.rows_copied,
        total_rows: Some(plan.source_count),
        message: "开始同步表数据".to_string(),
    });

    let (batch_size, clamped) = pipeline::clamp_batch_size(
        opts.batch_size,
        plan.writable_columns().len(),
        60_000,
    );
    if clamped {
        table.notes.push(format!(
            "列数较多，批次从 {} 收缩为 {}（避免超过 MySQL 占位符上限）",
            opts.batch_size, batch_size
        ));
    }

    if plan.pk_columns.is_empty() {
        load_no_pk(
            src,
            tgt,
            plan,
            opts,
            checkpoint,
            store,
            table,
            batch_size,
            dialect,
            source_type,
            target_type,
            table_index,
            table_total,
            progress,
        )
        .await?;
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
        let sql = batch_select_sql(plan, has_cursor, source_type);
        let mut binds = Vec::new();
        if let Some(cur) = &cursor {
            if cur.len() != plan.pk_columns.len() {
                return Err(AppError::InternalError(format!(
                    "表 {} 的断点主键列数不一致（断点 {}，当前 {}）",
                    plan.display_name,
                    cur.len(),
                    plan.pk_columns.len()
                )));
            }
            for value in cur {
                binds.push(Bind::Text(Some(value.clone())));
            }
        }
        binds.push(Bind::Int(batch_size as i64));
        let rows = fetch_all(src, &sql, &binds).await?;
        if rows.is_empty() {
            break;
        }

        let parsed = parse_rows(&rows, plan.writable_columns().len(), &pk_idx)?;
        let upper = parsed
            .1
            .last()
            .cloned()
            .ok_or_else(|| AppError::InternalError("读取到空批次".to_string()))?;
        let source_pks = parsed.1.clone();

        begin(tgt).await?;
        let result: Result<(), AppError> = async {
            let insert_sql = upsert_sql(plan, parsed.0.len(), target_type, dialect);
            let mut insert_binds = Vec::with_capacity(parsed.0.len() * plan.writable_columns().len());
            for row in &parsed.0 {
                for value in row {
                    insert_binds.push(Bind::Text(value.clone()));
                }
            }
            execute_sql(tgt, &insert_sql, &insert_binds).await?;
            if opts.mode == SyncMode::Mirror {
                mirror_delete_range(
                    tgt,
                    plan,
                    target_type,
                    cursor.as_deref(),
                    Some(&upper),
                    &source_pks,
                )
                .await?;
            }
            Ok(())
        }
        .await;
        match &result {
            Ok(()) => commit(tgt).await?,
            Err(_) => rollback(tgt).await,
        }
        result?;

        rows_copied += parsed.0.len() as u64;
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
        store.save(checkpoint)?;
        progress(PgDataSyncProgress {
            stage: pipeline::STAGE_LOAD.to_string(),
            table: Some(plan.display_name.clone()),
            table_index,
            table_total,
            rows_copied,
            total_rows: Some(plan.source_count),
            message: format!(
                "表 {} 已提交 {} 行（本批 {}）",
                plan.display_name,
                rows_copied,
                parsed.0.len()
            ),
        });
        if parsed.0.len() < batch_size {
            break;
        }
    }

    if opts.mode == SyncMode::Mirror {
        begin(tgt).await?;
        let result = async {
            if let Some(cur) = &cursor {
                let sql = delete_after_pk_sql(plan, target_type);
                let binds = cur
                    .iter()
                    .map(|v| Bind::Text(Some(v.clone())))
                    .collect::<Vec<_>>();
                execute_sql(tgt, &sql, &binds).await?;
            } else {
                execute_sql(tgt, &delete_all_sql(plan), &[]).await?;
            }
            Ok::<(), AppError>(())
        }
        .await;
        match &result {
            Ok(()) => commit(tgt).await?,
            Err(_) => rollback(tgt).await,
        }
        result?;
    }

    table.status = "completed".to_string();
    table.rows_copied = rows_copied;
    table.batches = batches;
    checkpoint.tables.insert(
        plan.display_name.clone(),
        TableCheckpoint {
            last_pk: cursor.unwrap_or_default(),
            rows_copied,
            completed: true,
        },
    );
    store.save(checkpoint)?;
    progress(PgDataSyncProgress {
        stage: pipeline::STAGE_LOAD.to_string(),
        table: Some(plan.display_name.clone()),
        table_index,
        table_total,
        rows_copied,
        total_rows: Some(plan.source_count),
        message: "表数据同步完成".to_string(),
    });
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn load_no_pk(
    src: &mut MySqlConnection,
    tgt: &mut MySqlConnection,
    plan: &MySqlTablePlan,
    opts: &PgDataSyncOptions,
    checkpoint: &mut Checkpoint,
    store: &CheckpointStore,
    table: &mut PgTableReport,
    batch_size: usize,
    dialect: UpsertDialect,
    _source_type: &DbType,
    target_type: &DbType,
    table_index: usize,
    table_total: usize,
    progress: &(dyn Fn(PgDataSyncProgress) + Send + Sync),
) -> Result<(), AppError> {
    let rows = fetch_all(src, &full_select_sql(plan), &[]).await?;
    let parsed = parse_rows(&rows, plan.writable_columns().len(), &[])?;
    begin(tgt).await?;
    let result: Result<(), AppError> = async {
        if opts.mode == SyncMode::Mirror {
            execute_sql(tgt, &delete_all_sql(plan), &[]).await?;
        }
        for chunk in parsed.0.chunks(batch_size.max(1)) {
            let sql = upsert_sql(plan, chunk.len(), target_type, dialect);
            let mut binds = Vec::with_capacity(chunk.len() * plan.writable_columns().len());
            for row in chunk {
                for value in row {
                    binds.push(Bind::Text(value.clone()));
                }
            }
            execute_sql(tgt, &sql, &binds).await?;
        }
        Ok(())
    }
    .await;
    match &result {
        Ok(()) => commit(tgt).await?,
        Err(_) => rollback(tgt).await,
    }
    result?;

    table.rows_copied = parsed.0.len() as u64;
    table.batches = parsed.0.len().div_ceil(batch_size.max(1));
    table.status = "completed".to_string();
    table.notes.push(
        "无主键表按整表复制；upsert 模式下重复执行可能产生重复行，已标记为不可续跑".to_string(),
    );
    checkpoint.tables.insert(
        plan.display_name.clone(),
        TableCheckpoint {
            last_pk: vec![],
            rows_copied: table.rows_copied,
            completed: true,
        },
    );
    store.save(checkpoint)?;
    progress(PgDataSyncProgress {
        stage: pipeline::STAGE_LOAD.to_string(),
        table: Some(plan.display_name.clone()),
        table_index,
        table_total,
        rows_copied: table.rows_copied,
        total_rows: Some(plan.source_count),
        message: "无主键表整表复制完成".to_string(),
    });
    Ok(())
}

/// 返回 (每行值, 每行主键, 最后一行主键)。
fn parse_rows(
    rows: &[MySqlRow],
    columns: usize,
    pk_idx: &[usize],
) -> Result<(Vec<Vec<Option<String>>>, Vec<Vec<String>>, Option<Vec<String>>), AppError> {
    let mut values = Vec::with_capacity(rows.len());
    let mut pks = Vec::with_capacity(rows.len());
    for row in rows {
        let mut row_values = Vec::with_capacity(columns);
        for i in 0..columns {
            row_values.push(row.try_get::<Option<String>, _>(i)?);
        }
        let mut pk = Vec::with_capacity(pk_idx.len());
        for &i in pk_idx {
            let value = row_values
                .get(i)
                .and_then(|v| v.clone())
                .ok_or_else(|| AppError::InternalError("主键列读到 NULL".to_string()))?;
            pk.push(value);
        }
        values.push(row_values);
        pks.push(pk);
    }
    let upper = pks.last().cloned();
    Ok((values, pks, upper))
}

async fn mirror_delete_range(
    tgt: &mut MySqlConnection,
    plan: &MySqlTablePlan,
    target_type: &DbType,
    lower: Option<&[String]>,
    upper: Option<&[String]>,
    source_pks: &[Vec<String>],
) -> Result<(), AppError> {
    let sql = target_pk_select_sql(plan, target_type, lower.is_some(), upper.is_some());
    let mut binds = Vec::new();
    if let Some(lower) = lower {
        for value in lower {
            binds.push(Bind::Text(Some(value.clone())));
        }
    }
    if let Some(upper) = upper {
        for value in upper {
            binds.push(Bind::Text(Some(value.clone())));
        }
    }
    let rows = fetch_all(tgt, &sql, &binds).await?;
    let keep: BTreeSet<Vec<String>> = source_pks.iter().cloned().collect();
    for row in rows {
        let mut pk = Vec::with_capacity(plan.pk_columns.len());
        for i in 0..plan.pk_columns.len() {
            let value = row
                .try_get::<Option<String>, _>(i)?
                .ok_or_else(|| AppError::InternalError("mirror 主键读到 NULL".to_string()))?;
            pk.push(value);
        }
        if !keep.contains(&pk) {
            let binds = pk
                .iter()
                .map(|v| Bind::Text(Some(v.clone())))
                .collect::<Vec<_>>();
            execute_sql(tgt, &delete_by_pk_sql(plan, target_type), &binds).await?;
        }
    }
    Ok(())
}

async fn create_indexes_and_fks(
    tgt: &mut MySqlConnection,
    plans: &[MySqlTablePlan],
    opts: &PgDataSyncOptions,
    checkpoint: &Checkpoint,
    report: &mut PgDataSyncReport,
    ddl_tables: &mut BTreeSet<String>,
    progress: &(dyn Fn(PgDataSyncProgress) + Send + Sync),
) -> Result<(), AppError> {
    progress(pipeline::stage_progress(
        pipeline::STAGE_INDEXES,
        "正在创建索引与外键",
        plans.len(),
        0,
    ));
    for (idx, plan) in plans.iter().enumerate() {
        if !should_manage_structure(plan, opts, checkpoint) {
            continue;
        }
        for index in &plan.indexes {
            if index.columns.is_empty() {
                continue;
            }
            let exists: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM information_schema.STATISTICS \
                 WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = ? AND INDEX_NAME = ?",
            )
            .bind(&plan.name)
            .bind(&index.name)
            .fetch_one(&mut *tgt)
            .await?;
            if exists > 0 {
                continue;
            }
            execute_sql(tgt, &create_index_sql(plan, index), &[]).await?;
            ddl_tables.insert(plan.display_name.clone());
            let table = table_mut(&mut report.tables, &plan.display_name);
            table.notes.push(format!(
                "已创建索引 {}（DDL 隐式提交，不可回滚）",
                index.name
            ));
        }
        progress(pipeline::stage_progress(
            pipeline::STAGE_INDEXES,
            format!("表 {} 索引处理完成", plan.display_name),
            idx + 1,
            0,
        ));
    }

    for plan in plans {
        if !should_manage_structure(plan, opts, checkpoint) {
            continue;
        }
        for fk in &plan.foreign_keys {
            let exists: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM information_schema.TABLE_CONSTRAINTS \
                 WHERE CONSTRAINT_SCHEMA = DATABASE() AND TABLE_NAME = ? AND CONSTRAINT_NAME = ? AND CONSTRAINT_TYPE = 'FOREIGN KEY'",
            )
            .bind(&plan.name)
            .bind(&fk.name)
            .fetch_one(&mut *tgt)
            .await?;
            if exists > 0 {
                continue;
            }
            execute_sql(tgt, &add_foreign_key_sql(plan, fk), &[]).await?;
            ddl_tables.insert(plan.display_name.clone());
            let table = table_mut(&mut report.tables, &plan.display_name);
            table.notes.push(format!(
                "已创建外键 {}（DDL 隐式提交，不可回滚；创建时校验已有数据）",
                fk.name
            ));
        }
    }
    Ok(())
}

fn should_manage_structure(
    plan: &MySqlTablePlan,
    opts: &PgDataSyncOptions,
    checkpoint: &Checkpoint,
) -> bool {
    !plan.target_exists
        || checkpoint.created_tables.contains(&plan.display_name)
        || opts.resume
}

async fn align_sequences(
    tgt: &mut MySqlConnection,
    plans: &[MySqlTablePlan],
    checkpoint: &Checkpoint,
    report: &mut PgDataSyncReport,
    ddl_tables: &mut BTreeSet<String>,
    progress: &(dyn Fn(PgDataSyncProgress) + Send + Sync),
) -> Result<(), AppError> {
    progress(pipeline::stage_progress(
        pipeline::STAGE_SEQUENCES,
        "正在对齐 AUTO_INCREMENT",
        plans.len(),
        0,
    ));
    for (idx, plan) in plans.iter().enumerate() {
        for col in plan.auto_increment_columns() {
            if plan.target_exists
                && !plan.target_auto_increment_columns.contains(&col)
                && !checkpoint.created_tables.contains(&plan.display_name)
            {
                let table = table_mut(&mut report.tables, &plan.display_name);
                table.notes.push(format!(
                    "目标列 {col} 不是 AUTO_INCREMENT，跳过序列对齐；后续插入可能撞主键"
                ));
                continue;
            }
            let max_sql = format!(
                "SELECT CAST(MAX({}) AS CHAR) FROM {}",
                q(&col),
                plan.qualified()
            );
            let max: Option<String> = sqlx::query_scalar(&max_sql)
                .fetch_one(&mut *tgt)
                .await?;
            let next = match max {
                Some(text) if !text.is_empty() => {
                    let value: u128 = text.parse().map_err(|_| {
                        AppError::InternalError(format!(
                            "表 {} 的 AUTO_INCREMENT 列 {} 最大值无法解析：{}",
                            plan.display_name, col, text
                        ))
                    })?;
                    value.checked_add(1).ok_or_else(|| {
                        AppError::InternalError(format!(
                            "表 {} 的 AUTO_INCREMENT 列 {} 已达到 u128 上限",
                            plan.display_name, col
                        ))
                    })?
                }
                _ => 1,
            };
            if next > u64::MAX as u128 {
                return Err(AppError::InternalError(format!(
                    "表 {} 的 AUTO_INCREMENT 列 {} 下一次值 {} 超过 MySQL 上限",
                    plan.display_name, col, next
                )));
            }
            let sql = format!(
                "ALTER TABLE {} AUTO_INCREMENT = {}",
                plan.qualified(),
                next.max(1)
            );
            execute_sql(tgt, &sql, &[]).await?;
            ddl_tables.insert(plan.display_name.clone());
            let table = table_mut(&mut report.tables, &plan.display_name);
            table.sequences_aligned.push(col.clone());
            table.notes.push(format!(
                "已对齐 {}.{} AUTO_INCREMENT = {}（DDL 隐式提交）",
                plan.display_name,
                col,
                next.max(1)
            ));
        }
        progress(pipeline::stage_progress(
            pipeline::STAGE_SEQUENCES,
            format!("表 {} 序列对齐完成", plan.display_name),
            idx + 1,
            0,
        ));
    }
    Ok(())
}

async fn verify_all(
    src: &mut MySqlConnection,
    tgt: &mut MySqlConnection,
    plans: &[MySqlTablePlan],
    batch_size: usize,
    source_type: &DbType,
    target_type: &DbType,
    report: &mut PgDataSyncReport,
    progress: &(dyn Fn(PgDataSyncProgress) + Send + Sync),
) {
    progress(pipeline::stage_progress(
        pipeline::STAGE_VERIFY,
        "正在做行数与分块哈希校验",
        plans.len(),
        0,
    ));
    for (idx, plan) in plans.iter().enumerate() {
        let table = table_mut(&mut report.tables, &plan.display_name);
        if table.status == "failed" {
            table.verification = "unverified".to_string();
            table.notes.push("数据阶段失败，未做完整校验".to_string());
            continue;
        }
        if plan.pk_columns.is_empty() {
            let src_count = count_rows(src, plan).await;
            let tgt_count = count_rows(tgt, plan).await;
            match (src_count, tgt_count) {
                (Ok(s), Ok(t)) => {
                    table.source_rows = Some(s);
                    table.target_rows = Some(t);
                    table.checksum_equal = None;
                    if s == t {
                        table.verification = "downgraded_unverified".to_string();
                        table.notes.push(
                            "无主键：只做了行数比对，未做分块哈希校验（downgraded_unverified）"
                                .to_string(),
                        );
                    } else {
                        table.verification = "failed".to_string();
                        table.notes.push(format!(
                            "无主键：行数不一致（源 {s}，目标 {t}）"
                        ));
                    }
                }
                (Err(e), _) | (_, Err(e)) => {
                    table.verification = "unverified".to_string();
                    table.notes.push(format!("行数校验失败：{e}"));
                }
            }
            progress(pipeline::stage_progress(
                pipeline::STAGE_VERIFY,
                format!("表 {} 校验完成", plan.display_name),
                idx + 1,
                0,
            ));
            continue;
        }

        let src_digest = digest_side(src, plan, batch_size, source_type).await;
        let tgt_digest = digest_side(tgt, plan, batch_size, target_type).await;
        match (src_digest, tgt_digest) {
            (Ok((s_count, s_hash)), Ok((t_count, t_hash))) => {
                table.source_rows = Some(s_count);
                table.target_rows = Some(t_count);
                table.checksum_equal = Some(s_hash == t_hash);
                if s_count == t_count && s_hash == t_hash {
                    table.verification = "verified".to_string();
                } else {
                    table.verification = "failed".to_string();
                    table.notes.push(format!(
                        "行数/分块哈希不一致（源 {s_count} 行 {}，目标 {t_count} 行 {}）",
                        short_hash(&s_hash),
                        short_hash(&t_hash)
                    ));
                }
            }
            (Err(e), _) | (_, Err(e)) => {
                table.verification = "unverified".to_string();
                table.notes.push(format!("分块哈希校验失败：{e}"));
            }
        }
        progress(pipeline::stage_progress(
            pipeline::STAGE_VERIFY,
            format!("表 {} 校验完成", plan.display_name),
            idx + 1,
            0,
        ));
    }
}

fn short_hash(hash: &str) -> &str {
    &hash[..hash.len().min(12)]
}

async fn count_rows(conn: &mut MySqlConnection, plan: &MySqlTablePlan) -> Result<u64, AppError> {
    let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {}", plan.qualified()))
        .fetch_one(&mut *conn)
        .await?;
    Ok(count.max(0) as u64)
}

async fn digest_side(
    conn: &mut MySqlConnection,
    plan: &MySqlTablePlan,
    batch_size: usize,
    flavor: &DbType,
) -> Result<(u64, String), AppError> {
    let columns = plan.writable_columns().len();
    let (batch_size, _) = pipeline::clamp_batch_size(batch_size, columns, 60_000);
    let pk_idx = plan.pk_indexes_in_writable()?;
    let mut hasher = Sha256::new();
    let mut cursor: Option<Vec<String>> = None;
    let mut total: u64 = 0;
    loop {
        let sql = batch_select_sql(plan, cursor.is_some(), flavor);
        let mut binds = Vec::new();
        if let Some(cur) = &cursor {
            for value in cur {
                binds.push(Bind::Text(Some(value.clone())));
            }
        }
        binds.push(Bind::Int(batch_size as i64));
        let rows = fetch_all(conn, &sql, &binds).await?;
        if rows.is_empty() {
            break;
        }
        for row in &rows {
            for i in 0..columns {
                match row.try_get::<Option<String>, _>(i)? {
                    None => hasher.update(b"\x00"),
                    Some(value) => {
                        hasher.update(b"\x01");
                        hasher.update((value.len() as u64).to_le_bytes());
                        hasher.update(value.as_bytes());
                    }
                }
            }
            hasher.update(b"\x02");
        }
        total += rows.len() as u64;
        let mut next = Vec::with_capacity(pk_idx.len());
        for &i in &pk_idx {
            let value = rows
                .last()
                .unwrap()
                .try_get::<Option<String>, _>(i)?
                .ok_or_else(|| AppError::InternalError("校验主键读到 NULL".to_string()))?;
            next.push(value);
        }
        cursor = Some(next);
        if rows.len() < batch_size {
            break;
        }
    }
    Ok((total, format!("{:x}", hasher.finalize())))
}
