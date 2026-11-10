//! 只读查询门：字段治理的所有读取必须先通过只读语句校验。
//!
//! 复用 `core_lib::sql::util::is_read_only_statement` 做 AST 级判断；
//! 解析失败或非只读语句直接返回 `Validation`，绝不落到执行层。
//!
//! 按引擎分派方言与取行器：
//! - MySQL / MariaDB：`MySqlDialect` + `mysql_rows::fetch_rows`
//! - PostgreSQL：`PostgreSqlDialect` + `pg_rows::fetch_rows`（numeric/bytea/timestamptz
//!   由 `pg_cell_to_value` 按类型格式化，不统一 `try_get::<String>`）

use sqlparser::dialect::{MySqlDialect, PostgreSqlDialect};
use sqlparser::parser::Parser;

use crate::config::DbType;
use crate::ops::error::OpsError;
use crate::ops::mysql_rows::{fetch_rows, RowMap};
use crate::ops::registry::OpsContext;
use crate::sql::util::is_read_only_statement;

/// 按引擎校验 SQL 是否为单条只读语句。
///
/// 公开用于集成测试直接验证只读门；不暴露任何执行能力。
pub fn validate_read_only_sql(db_type: DbType, sql: &str) -> Result<(), OpsError> {
    let statements = match db_type {
        DbType::MySQL | DbType::MariaDB => Parser::parse_sql(&MySqlDialect {}, sql),
        DbType::PostgreSQL => Parser::parse_sql(&PostgreSqlDialect {}, sql),
        // 其余引擎不在字段治理 S1 范围内；解析交给 MySQL 方言后由只读校验拒绝。
        _ => Parser::parse_sql(&MySqlDialect {}, sql),
    }
    .map_err(|err| OpsError::Validation(format!("字段治理只读校验失败：{err}")))?;
    if statements.len() != 1 || !is_read_only_statement(&statements[0]) {
        return Err(OpsError::Validation(
            "字段治理只允许只读查询，已拒绝非只读 SQL".to_string(),
        ));
    }
    Ok(())
}

/// 校验并执行只读 SQL。任何非 `SELECT` / `EXPLAIN` / `SHOW` 语句都会被拒绝。
pub(crate) async fn fetch_read_only(ctx: &OpsContext, sql: &str) -> Result<Vec<RowMap>, OpsError> {
    let db_type = ctx.db_client.db_type.clone();
    validate_read_only_sql(db_type, sql)?;
    if ctx.is_postgres() {
        crate::ops::pg_rows::fetch_rows(ctx, sql).await
    } else {
        fetch_rows(ctx, sql).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_select_and_rejects_writes() {
        let dialect = MySqlDialect {};
        let select = Parser::parse_sql(
            &dialect,
            "SELECT COUNT(*) AS rows_profiled FROM app.orders",
        )
        .unwrap();
        assert!(is_read_only_statement(&select[0]));

        for sql in [
            "INSERT INTO app.orders (id) VALUES (1)",
            "UPDATE app.orders SET id = 1",
            "DELETE FROM app.orders WHERE id = 1",
            "ALTER TABLE app.orders DROP COLUMN payload",
            "CREATE TABLE app.orders_ext (id INT)",
            "TRUNCATE TABLE app.orders",
        ] {
            let parsed = Parser::parse_sql(&dialect, sql).unwrap();
            assert!(
                !is_read_only_statement(&parsed[0]),
                "写语句必须被拒绝: {sql}"
            );
        }
    }

    #[test]
    fn pg_dialect_accepts_select_and_rejects_writes() {
        for sql in [
            "SELECT COUNT(*) AS row_count FROM public.orders",
            "SELECT jsonb_typeof(payload) AS t, COUNT(*) AS n FROM public.orders GROUP BY 1",
            "SELECT CAST(created_at AS text) FROM public.orders WHERE id > 0",
        ] {
            validate_read_only_sql(DbType::PostgreSQL, sql).unwrap_or_else(|err| {
                panic!("PG 只读语句必须被接受: {sql}: {err}");
            });
        }
        for sql in [
            "INSERT INTO public.orders (id) VALUES (1)",
            "INSERT INTO public.orders_ext (id) SELECT id FROM public.orders ON CONFLICT (id) DO UPDATE SET id = EXCLUDED.id",
            "UPDATE public.orders SET id = 1",
            "DELETE FROM public.orders WHERE id = 1",
            "ALTER TABLE public.orders ALTER COLUMN payload TYPE jsonb USING payload::jsonb",
            "CREATE TABLE public.orders_ext (id bigint)",
            "TRUNCATE TABLE public.orders",
        ] {
            assert!(
                validate_read_only_sql(DbType::PostgreSQL, sql).is_err(),
                "PG 写语句必须被拒绝: {sql}"
            );
        }
    }
}
