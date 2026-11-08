//! 只读查询门：字段治理的所有读取必须先通过只读语句校验。
//!
//! 复用 `core_lib::sql::util::is_read_only_statement` 做 AST 级判断；
//! 解析失败或非只读语句直接返回 `Validation`，绝不落到执行层。

use sqlparser::dialect::MySqlDialect;
use sqlparser::parser::Parser;

use crate::ops::error::OpsError;
use crate::ops::mysql_rows::{fetch_rows, RowMap};
use crate::ops::registry::OpsContext;
use crate::sql::util::is_read_only_statement;

/// 校验并执行只读 SQL。任何非 `SELECT` / `EXPLAIN` / `SHOW` 语句都会被拒绝。
pub(crate) async fn fetch_read_only(ctx: &OpsContext, sql: &str) -> Result<Vec<RowMap>, OpsError> {
    let statements = Parser::parse_sql(&MySqlDialect {}, sql)
        .map_err(|err| OpsError::Validation(format!("字段治理只读校验失败：{err}")))?;
    if statements.len() != 1 || !is_read_only_statement(&statements[0]) {
        return Err(OpsError::Validation(
            "字段治理只允许只读查询，已拒绝非只读 SQL".to_string(),
        ));
    }
    fetch_rows(ctx, sql).await
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
}
