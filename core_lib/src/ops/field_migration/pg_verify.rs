//! PG 版 V1–V4 校验。
//!
//! 口径（方案第 4 节）：
//! - 同库校验放进 `BEGIN` + `SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY` 快照；
//! - `numeric` 直接比较、不走 f64；`bytea` 走 `encode(...,'hex')`；
//! - `timestamptz` 归一化到 UTC；`json` 先 `::jsonb`；
//! - 全部用 `IS DISTINCT FROM`，`NULL` 与空串不合并；
//! - 主键分页与差异分页统一类型化绑定，文本主键统一 `COLLATE "C"`。

use std::str::FromStr;

use sqlx::postgres::{PgArguments, PgPool};
use sqlx::types::{BigDecimal, Uuid};
use sqlx::{PgConnection, Postgres, Transaction};

use crate::ops::error::OpsError;
use crate::ops::mysql_rows::RowMap;
use crate::ops::pg_rows::convert_pg_rows;
use crate::ops::registry::OpsContext;
use crate::sql::util::quote_ident_pg;

use super::migration_store::{BackfillSpec, BatchVerify, PkKind};
use super::pg_scripts::{json_key_expr, normalized_source_expr};
use super::types::ColumnProfile;

pub const VALIDATION_MODE_SNAPSHOT: &str = "snapshot";

/// 类型化绑定值；避免把主键拼进 SQL 字面量。
#[derive(Debug, Clone)]
pub enum PgBind {
    Int(i64),
    Numeric(BigDecimal),
    Text(String),
    Uuid(Uuid),
}

pub(crate) fn bind_value(kind: PkKind, value: &str) -> Result<PgBind, OpsError> {
    match kind {
        PkKind::Integer => value
            .parse::<i64>()
            .map(PgBind::Int)
            .map_err(|err| OpsError::Validation(format!("主键 {value} 不是合法整数: {err}"))),
        PkKind::Numeric => BigDecimal::from_str(value)
            .map(PgBind::Numeric)
            .map_err(|err| OpsError::Validation(format!("主键 {value} 不是合法 numeric: {err}"))),
        PkKind::Text => Ok(PgBind::Text(value.to_string())),
        PkKind::Uuid => Uuid::parse_str(value)
            .map(PgBind::Uuid)
            .map_err(|err| OpsError::Validation(format!("主键 {value} 不是合法 uuid: {err}"))),
    }
}

pub(crate) fn cast_clause(kind: PkKind, index: usize) -> String {
    match kind {
        PkKind::Integer => format!("${index}::bigint"),
        PkKind::Numeric => format!("${index}::numeric"),
        PkKind::Text => format!("(${index}::text COLLATE \"C\")"),
        PkKind::Uuid => format!("${index}::uuid"),
    }
}

/// 文本主键排序与比较统一 `COLLATE "C"`。
pub(crate) fn order_expr(key: &str, kind: PkKind) -> String {
    match kind {
        PkKind::Text => format!("{key} COLLATE \"C\""),
        _ => key.to_string(),
    }
}

pub(crate) fn push_range(
    sql: &mut String,
    key: &str,
    kind: PkKind,
    last: Option<&str>,
    end: Option<&str>,
    binds: &mut Vec<PgBind>,
) -> Result<(), OpsError> {
    if let Some(value) = last {
        binds.push(bind_value(kind, value)?);
        let clause = cast_clause(kind, binds.len());
        sql.push_str(&format!(" AND {key} > {clause}"));
    }
    if let Some(value) = end {
        binds.push(bind_value(kind, value)?);
        let clause = cast_clause(kind, binds.len());
        sql.push_str(&format!(" AND {key} <= {clause}"));
    }
    Ok(())
}

pub(crate) fn qualified(schema: Option<&str>, table: &str) -> String {
    match schema {
        Some(schema) if !schema.trim().is_empty() => {
            format!("{}.{}", quote_ident_pg(schema), quote_ident_pg(table))
        }
        _ => format!("{}.{}", quote_ident_pg("public"), quote_ident_pg(table)),
    }
}

pub(crate) fn bind_query<'q>(
    sql: &'q str,
    binds: &[PgBind],
) -> sqlx::query::Query<'q, Postgres, PgArguments> {
    let mut query = sqlx::query::<Postgres>(sql);
    for bind in binds {
        query = match bind {
            PgBind::Int(value) => query.bind(*value),
            PgBind::Numeric(value) => query.bind(value.clone()),
            PgBind::Text(value) => query.bind(value.clone()),
            PgBind::Uuid(value) => query.bind(*value),
        };
    }
    query
}

async fn fetch_rows(
    conn: &mut PgConnection,
    sql: &str,
    binds: &[PgBind],
) -> Result<Vec<RowMap>, OpsError> {
    let rows = bind_query(sql, binds).fetch_all(&mut *conn).await?;
    Ok(convert_pg_rows(&rows))
}

pub(crate) async fn fetch_on_pool(
    pool: &PgPool,
    sql: &str,
    binds: &[PgBind],
) -> Result<Vec<RowMap>, OpsError> {
    let rows = bind_query(sql, binds).fetch_all(pool).await?;
    Ok(convert_pg_rows(&rows))
}

pub(crate) fn count_of(rows: &[RowMap], key: &str) -> u64 {
    rows.first()
        .and_then(|row| row.get(key))
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

/// 只读快照；Drop 时 sqlx 自动回滚，所有成功路径显式 commit/rollback。
pub struct PgSnapshot<'a> {
    tx: Transaction<'a, Postgres>,
}

impl<'a> PgSnapshot<'a> {
    pub async fn begin(pool: &'a PgPool) -> Result<Self, OpsError> {
        let mut tx = pool.begin().await?;
        // BEGIN 后、任何查询前设置事务特性，等价于
        // BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY。
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await?;
        Ok(Self { tx })
    }

    pub async fn settings(&mut self) -> Result<(String, bool), OpsError> {
        let isolation: String = sqlx::query_scalar("SHOW transaction_isolation")
            .fetch_one(&mut *self.tx)
            .await?;
        let read_only: bool =
            sqlx::query_scalar("SELECT current_setting('transaction_read_only')::boolean")
                .fetch_one(&mut *self.tx)
                .await?;
        Ok((isolation, read_only))
    }

    pub async fn count(&mut self, sql: &str, binds: &[PgBind]) -> Result<u64, OpsError> {
        let rows = fetch_rows(&mut self.tx, sql, binds).await?;
        Ok(count_of(&rows, "count"))
    }

    pub(crate) async fn verify(
        &mut self,
        spec: &BackfillSpec,
        last_pk: Option<&str>,
        end_pk: Option<&str>,
    ) -> Result<BatchVerify, OpsError> {
        run_verify(&mut self.tx, spec, last_pk, end_pk).await
    }

    pub async fn rows(&mut self, sql: &str, binds: &[PgBind]) -> Result<Vec<RowMap>, OpsError> {
        fetch_rows(&mut self.tx, sql, binds).await
    }

    pub async fn commit(self) -> Result<(), OpsError> {
        self.tx.commit().await?;
        Ok(())
    }

    pub async fn rollback(self) -> Result<(), OpsError> {
        self.tx.rollback().await?;
        Ok(())
    }
}

pub async fn snapshot_settings(ctx: &OpsContext) -> Result<(String, bool), OpsError> {
    let pool = ctx.db_client.pool.pg()?;
    let mut snapshot = PgSnapshot::begin(pool).await?;
    let settings = snapshot.settings().await?;
    snapshot.rollback().await?;
    Ok(settings)
}

pub fn pk_kind_from_pg_type(data_type: &str) -> Option<PkKind> {
    let lower = data_type.trim().to_ascii_lowercase();
    if lower == "smallint"
        || lower == "integer"
        || lower == "bigint"
        || lower == "int2"
        || lower == "int4"
        || lower == "int8"
    {
        Some(PkKind::Integer)
    } else if lower == "numeric" || lower == "decimal" {
        Some(PkKind::Numeric)
    } else if lower == "uuid" {
        Some(PkKind::Uuid)
    } else if lower == "text"
        || lower == "character varying"
        || lower == "character"
        || lower == "varchar"
        || lower == "bpchar"
        || lower == "name"
    {
        Some(PkKind::Text)
    } else {
        None
    }
}

pub async fn detect_pk_kind(
    ctx: &OpsContext,
    schema: Option<&str>,
    table: &str,
    column: &str,
) -> Result<PkKind, OpsError> {
    let pool = ctx.db_client.pool.pg()?;
    let rows = sqlx::query(
        "SELECT data_type FROM information_schema.columns \
         WHERE table_schema = COALESCE($1, current_schema()) \
           AND table_name = $2 AND column_name = $3",
    )
    .bind(schema)
    .bind(table)
    .bind(column)
    .fetch_all(pool)
    .await?;
    let rows = convert_pg_rows(&rows);
    let data_type = rows
        .first()
        .and_then(|row| row.get("data_type"))
        .map(|value| value.trim().to_string())
        .ok_or_else(|| {
            OpsError::Validation(format!(
                "无法确定主键 {table}.{column} 的类型（列不存在或不可见）"
            ))
        })?;
    pk_kind_from_pg_type(&data_type).ok_or_else(|| {
        OpsError::Validation(format!(
            "主键 {table}.{column} 类型 {data_type} 不支持 keyset 分批"
        ))
    })
}

fn spec_kind(spec: &BackfillSpec) -> Result<PkKind, OpsError> {
    spec.pk_kind
        .ok_or_else(|| OpsError::Validation("缺少主键类型，无法校验".into()))
}

fn source_profile(spec: &BackfillSpec) -> ColumnProfile {
    ColumnProfile {
        data_type: spec.source_data_type.clone(),
        column_type: spec.source_data_type.clone(),
        ..Default::default()
    }
}

fn source_ref(spec: &BackfillSpec) -> String {
    qualified(spec.schema.as_deref(), &spec.source_table)
}

fn target_ref(spec: &BackfillSpec) -> String {
    qualified(spec.schema.as_deref(), &spec.target_table)
}

fn source_pk(spec: &BackfillSpec) -> String {
    format!("s.{}", quote_ident_pg(&spec.pk_column))
}

fn target_pk(spec: &BackfillSpec) -> String {
    format!("t.{}", quote_ident_pg(&spec.pk_column))
}

fn mismatch_condition(spec: &BackfillSpec) -> Result<String, OpsError> {
    if spec.target_columns.is_empty() {
        return Err(OpsError::Validation("缺少目标列，无法生成 V4 比对".into()));
    }
    let profile = source_profile(spec);
    let source_column = format!("s.{}", quote_ident_pg(&spec.source_column));
    let conditions = spec
        .target_columns
        .iter()
        .map(|column| {
            let target = format!("t.{}", quote_ident_pg(&column.name));
            let (expr, kind) =
                normalized_source_expr(&source_column, &profile, column, spec.empty_string_policy);
            match kind.as_str() {
                "numeric" => format!("{expr} IS DISTINCT FROM CAST({target} AS numeric)"),
                "uuid" => format!("{expr} IS DISTINCT FROM {target}"),
                "timestamptz" => format!(
                    "{expr} AT TIME ZONE 'UTC' IS DISTINCT FROM {target} AT TIME ZONE 'UTC'"
                ),
                "bytea" => format!("{expr} IS DISTINCT FROM encode({target}, 'hex')"),
                "json" => {
                    let value = column
                        .source_key
                        .as_deref()
                        .map(|key| json_key_expr(&source_column, key, false))
                        .unwrap_or_else(|| source_column.clone());
                    format!("CAST({value} AS jsonb) IS DISTINCT FROM CAST({target} AS jsonb)")
                }
                _ => format!("{expr} IS DISTINCT FROM CAST({target} AS text)"),
            }
        })
        .collect::<Vec<_>>();
    Ok(conditions.join(" OR "))
}

/// 在给定连接上执行 V1–V4；调用方负责事务/快照边界。
pub(crate) async fn run_verify(
    conn: &mut PgConnection,
    spec: &BackfillSpec,
    last_pk: Option<&str>,
    end_pk: Option<&str>,
) -> Result<BatchVerify, OpsError> {
    let kind = spec_kind(spec)?;
    let source = source_ref(spec);
    let destination = target_ref(spec);
    let pk_s = source_pk(spec);
    let pk_t = target_pk(spec);

    // V1：源/目标行数（同一范围）。
    let mut binds = Vec::new();
    let mut sql = format!("SELECT (SELECT COUNT(*) FROM {source} s WHERE TRUE");
    push_range(&mut sql, &pk_s, kind, last_pk, end_pk, &mut binds)?;
    sql.push_str(&format!(
        ") AS source_rows, (SELECT COUNT(*) FROM {destination} t WHERE TRUE"
    ));
    push_range(&mut sql, &pk_t, kind, last_pk, end_pk, &mut binds)?;
    sql.push_str(") AS target_rows");
    let rows = fetch_rows(conn, &sql, &binds).await?;
    let source_rows = count_of(&rows, "source_rows");
    let target_rows = count_of(&rows, "target_rows");

    // V2：源有目标无。
    let mut binds = Vec::new();
    let mut sql = format!(
        "SELECT COUNT(*) AS count FROM {source} s \
         LEFT JOIN {destination} t ON {pk_t} = {pk_s} \
         WHERE {pk_t} IS NULL"
    );
    push_range(&mut sql, &pk_s, kind, last_pk, end_pk, &mut binds)?;
    let rows = fetch_rows(conn, &sql, &binds).await?;
    let v2 = count_of(&rows, "count");

    // V3：目标有源无。
    let mut binds = Vec::new();
    let mut sql = format!(
        "SELECT COUNT(*) AS count FROM {destination} t \
         LEFT JOIN {source} s ON {pk_s} = {pk_t} \
         WHERE {pk_s} IS NULL"
    );
    push_range(&mut sql, &pk_t, kind, last_pk, end_pk, &mut binds)?;
    let rows = fetch_rows(conn, &sql, &binds).await?;
    let v3 = count_of(&rows, "count");

    // V4：两侧都有但值不一致。
    let mismatch = mismatch_condition(spec)?;
    let mut binds = Vec::new();
    let mut sql = format!(
        "SELECT COUNT(*) AS count FROM {source} s \
         JOIN {destination} t ON {pk_t} = {pk_s} \
         WHERE ({mismatch})"
    );
    push_range(&mut sql, &pk_s, kind, last_pk, end_pk, &mut binds)?;
    let rows = fetch_rows(conn, &sql, &binds).await?;
    let v4 = count_of(&rows, "count");

    Ok(BatchVerify {
        source_rows,
        target_rows,
        v2,
        v3,
        v4,
        validation_mode: VALIDATION_MODE_SNAPSHOT.to_string(),
    })
}

pub async fn verify_range(
    ctx: &OpsContext,
    spec: &BackfillSpec,
    last_pk: Option<&str>,
    end_pk: Option<&str>,
) -> Result<BatchVerify, OpsError> {
    let pool = ctx.db_client.pool.pg()?;
    let mut snapshot = PgSnapshot::begin(pool).await?;
    let verify = match snapshot.verify(spec, last_pk, end_pk).await {
        Ok(verify) => verify,
        Err(err) => {
            let _ = snapshot.rollback().await;
            return Err(err);
        }
    };
    snapshot.commit().await?;
    Ok(verify)
}

pub async fn verify_full(ctx: &OpsContext, spec: &BackfillSpec) -> Result<BatchVerify, OpsError> {
    verify_range(ctx, spec, None, None).await
}

fn diff_select(spec: &BackfillSpec, phase: &str) -> Result<(String, String, PkKind), OpsError> {
    let kind = spec_kind(spec)?;
    let source = source_ref(spec);
    let destination = target_ref(spec);
    let pk_s = source_pk(spec);
    let pk_t = target_pk(spec);
    let (sql, key) = match phase {
        "v2" => (
            format!(
                "FROM {source} s LEFT JOIN {destination} t ON {pk_t} = {pk_s} \
                 WHERE {pk_t} IS NULL"
            ),
            pk_s.clone(),
        ),
        "v3" => (
            format!(
                "FROM {destination} t LEFT JOIN {source} s ON {pk_s} = {pk_t} \
                 WHERE {pk_s} IS NULL"
            ),
            pk_t.clone(),
        ),
        "v4" => (
            format!(
                "FROM {source} s JOIN {destination} t ON {pk_t} = {pk_s} \
                 WHERE ({})",
                mismatch_condition(spec)?
            ),
            pk_s.clone(),
        ),
        other => {
            return Err(OpsError::Validation(format!(
                "未知差异阶段 {other}，仅支持 v2/v3/v4"
            )))
        }
    };
    Ok((sql, key, kind))
}

pub async fn diff_total(
    ctx: &OpsContext,
    spec: &BackfillSpec,
    phase: &str,
    last_pk: Option<&str>,
    end_pk: Option<&str>,
) -> Result<u64, OpsError> {
    let (from, key, kind) = diff_select(spec, phase)?;
    let mut binds = Vec::new();
    let mut sql = format!("SELECT COUNT(*) AS count {from}");
    push_range(&mut sql, &key, kind, last_pk, end_pk, &mut binds)?;
    let pool = ctx.db_client.pool.pg()?;
    let mut snapshot = PgSnapshot::begin(pool).await?;
    let rows = match snapshot.rows(&sql, &binds).await {
        Ok(rows) => rows,
        Err(err) => {
            let _ = snapshot.rollback().await;
            return Err(err);
        }
    };
    snapshot.commit().await?;
    Ok(count_of(&rows, "count"))
}

pub async fn diff_page(
    ctx: &OpsContext,
    spec: &BackfillSpec,
    phase: &str,
    last_pk: Option<&str>,
    end_pk: Option<&str>,
    limit: u32,
) -> Result<Vec<String>, OpsError> {
    let (from, key, kind) = diff_select(spec, phase)?;
    let mut binds = Vec::new();
    let mut sql = format!("SELECT {key} AS pk_value {from}");
    push_range(&mut sql, &key, kind, last_pk, end_pk, &mut binds)?;
    sql.push_str(&format!(" ORDER BY {}", order_expr(&key, kind)));
    binds.push(PgBind::Int(limit as i64));
    sql.push_str(&format!(" LIMIT ${}", binds.len()));

    let pool = ctx.db_client.pool.pg()?;
    let mut snapshot = PgSnapshot::begin(pool).await?;
    let rows = match snapshot.rows(&sql, &binds).await {
        Ok(rows) => rows,
        Err(err) => {
            let _ = snapshot.rollback().await;
            return Err(err);
        }
    };
    snapshot.commit().await?;
    Ok(rows
        .iter()
        .filter_map(|row| row.get("pk_value").cloned())
        .collect())
}
