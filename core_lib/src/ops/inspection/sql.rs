//! 巡检专用只读 SQL 守卫。
//!
//! 巡检不会复用允许 apply 的执行通道，也不接受外部 SQL 输入。
//! 即便如此，所有真正发往数据库的语句仍需经过 AST + 关键字双重只读校验，
//! 确保巡检链路在代码层面无法执行任何写操作。
//!
//! 解析方言按连接引擎选择：MySQL/MariaDB 用 `MySqlDialect`，PostgreSQL 用
//! `PostgreSqlDialect`；否则 PG 的 `::text` / `generate_series` 等合法只读语句
//! 会被 MySQL 方言误判。

use sqlparser::dialect::{MySqlDialect, PostgreSqlDialect};
use sqlparser::parser::Parser;

use crate::config::DbType;
use crate::ops::error::OpsError;
use crate::ops::mysql_rows::{self, RowMap};
use crate::ops::pg_rows;
use crate::ops::registry::OpsContext;
use crate::sql::util::is_read_only_statement;

const FORBIDDEN_PATTERNS: &[&str] = &[
    "into outfile",
    "into dumpfile",
    "for update",
    "for share",
    "for no key update",
    "for key share",
    "lock in share mode",
    "procedure analyse",
    "user_lock",
];

pub fn ensure_read_only_sql(sql: &str) -> Result<(), OpsError> {
    ensure_read_only_sql_for(&DbType::MySQL, sql)
}

pub fn ensure_read_only_sql_for(db_type: &DbType, sql: &str) -> Result<(), OpsError> {
    let trimmed = sql.trim();
    if trimmed.is_empty() {
        return Err(OpsError::Validation("巡检 SQL 为空".into()));
    }
    let statements = match db_type {
        DbType::PostgreSQL => Parser::parse_sql(&PostgreSqlDialect {}, trimmed),
        DbType::MySQL | DbType::MariaDB => Parser::parse_sql(&MySqlDialect {}, trimmed),
        other => {
            return Err(OpsError::Validation(format!(
                "巡检不支持该引擎的 SQL 解析: {}",
                other.display_name()
            )))
        }
    }
    .map_err(|err| OpsError::Validation(format!("巡检 SQL 无法解析，拒绝执行: {err}")))?;
    if statements.len() != 1 {
        return Err(OpsError::Validation(format!(
            "巡检只允许单条语句，实际解析出 {} 条",
            statements.len()
        )));
    }
    if !is_read_only_statement(&statements[0]) {
        return Err(OpsError::Validation(
            "巡检链路只允许只读语句（SELECT/SHOW/DESCRIBE/EXPLAIN）".into(),
        ));
    }
    let lowered = trimmed.to_ascii_lowercase();
    if let Some(pattern) = FORBIDDEN_PATTERNS
        .iter()
        .find(|pattern| lowered.contains(*pattern))
    {
        return Err(OpsError::Validation(format!(
            "巡检 SQL 包含禁止的副作用子句: {pattern}"
        )));
    }
    Ok(())
}

pub(crate) async fn fetch_read_only(
    ctx: &OpsContext,
    sql: &str,
) -> Result<Vec<RowMap>, OpsError> {
    let db_type = ctx.db_client.db_type.clone();
    ensure_read_only_sql_for(&db_type, sql)?;
    match db_type {
        DbType::PostgreSQL => pg_rows::fetch_rows(ctx, sql).await,
        DbType::MySQL | DbType::MariaDB => mysql_rows::fetch_rows(ctx, sql).await,
        other => Err(OpsError::Validation(format!(
            "巡检不支持该引擎: {}",
            other.display_name()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_information_schema_selects() {
        let sql = "SELECT TABLE_NAME FROM information_schema.TABLES WHERE TABLE_SCHEMA = 'app'";
        assert!(ensure_read_only_sql(sql).is_ok());
    }

    #[test]
    fn accepts_postgres_catalog_selects_with_casts_and_series() {
        let sql = "SELECT c.relname::text AS table_name, pg_get_indexdef(ix.indexrelid, k.ord::int, true) AS def \
                   FROM pg_class c JOIN generate_series(1, 3) AS k(ord) ON true";
        assert!(ensure_read_only_sql_for(&DbType::PostgreSQL, sql).is_ok());
    }

    #[test]
    fn rejects_mutations_and_ddl() {
        for sql in [
            "INSERT INTO t VALUES (1)",
            "UPDATE t SET a = 1",
            "DELETE FROM t",
            "DROP TABLE t",
            "ALTER TABLE t ADD COLUMN c INT",
            "CREATE TABLE t (id INT)",
            "TRUNCATE TABLE t",
            "GRANT SUPER ON *.* TO u@'%'",
            "SET GLOBAL general_log = ON",
        ] {
            assert!(
                ensure_read_only_sql(sql).is_err(),
                "must reject write statement: {sql}"
            );
        }
    }

    #[test]
    fn rejects_multiple_statements() {
        assert!(ensure_read_only_sql("SELECT 1; DROP TABLE t").is_err());
    }

    #[test]
    fn rejects_side_effecting_select_clauses() {
        assert!(ensure_read_only_sql("SELECT * FROM t FOR UPDATE").is_err());
        assert!(ensure_read_only_sql("SELECT * FROM t INTO OUTFILE '/tmp/a'").is_err());
        assert!(ensure_read_only_sql("SELECT * FROM t PROCEDURE ANALYSE()").is_err());
        assert!(ensure_read_only_sql_for(
            &DbType::PostgreSQL,
            "SELECT * FROM t FOR SHARE"
        )
        .is_err());
    }

    #[test]
    fn rejects_empty_sql() {
        assert!(ensure_read_only_sql("   ").is_err());
    }
}
