//! 服务端 DDL 编译器：`ActionSpec` -> 可执行 SQL。
//!
//! 这是新链路唯一的 SQL 生成入口。任何动作都必须通过校验后才会产出 SQL；
//! Agent 不接触 SQL 文本。首期仅支持 MySQL/MariaDB 的「新增索引」与必有的
//! 「删除索引」回滚动作。

use sqlparser::dialect::MySqlDialect;
use sqlparser::parser::Parser;

use crate::ops::error::OpsError;
use crate::ops::safety;
use crate::ops::types::{
    ActionSpec, CompiledAction, RiskClass, TableSnapshot,
};
use crate::sql::util::{quote_ident_mysql, validate_identifier};

const MAX_INDEX_COLUMNS: usize = 16;

pub fn compile_actions(
    actions: &[ActionSpec],
    snapshot: &TableSnapshot,
) -> Result<Vec<CompiledAction>, OpsError> {
    actions
        .iter()
        .map(|action| compile_action(action, snapshot))
        .collect()
}

pub fn compile_action(
    action: &ActionSpec,
    snapshot: &TableSnapshot,
) -> Result<CompiledAction, OpsError> {
    match action {
        ActionSpec::CreateIndex {
            table,
            name,
            columns,
            unique,
            using,
        } => compile_create_index(snapshot, table, name, columns, *unique, using.as_deref()),
        ActionSpec::DropIndex {
            table,
            name,
            expected_definition,
        } => compile_drop_index(snapshot, table, name, expected_definition.as_deref()),
        ActionSpec::AnalyzeTable { table } => compile_analyze(snapshot, table),
    }
}

fn ensure_same_table(snapshot: &TableSnapshot, table: &str) -> Result<(), OpsError> {
    if !snapshot.table_name.eq_ignore_ascii_case(table) {
        return Err(OpsError::Validation(format!(
            "动作表名 {} 与证据表 {} 不一致",
            table, snapshot.table_name
        )));
    }
    if snapshot.db_name.trim().is_empty() {
        return Err(OpsError::Validation("证据缺少数据库名".into()));
    }
    Ok(())
}

fn ensure_safe_ident(kind: &str, value: &str) -> Result<(), OpsError> {
    validate_identifier(value)
        .map_err(|e| OpsError::Validation(format!("{kind} '{}' 非法: {e}", value)))
}

fn ensure_supported_column_type(column_type: &str, column: &str) -> Result<(), OpsError> {
    let lower = column_type.to_ascii_lowercase();
    let unsupported = [
        "text", "tinytext", "mediumtext", "longtext", "blob", "tinyblob", "mediumblob",
        "longblob", "json", "geometry", "point", "linestring", "polygon", "multipoint",
        "multilinestring", "multipolygon", "geometrycollection",
    ];
    if unsupported.iter().any(|t| lower.contains(t)) {
        return Err(OpsError::Validation(format!(
            "列 {} 类型 {} 暂不支持直接建索引（MVP 不做前缀索引）",
            column, column_type
        )));
    }
    Ok(())
}

/// 用 sqlparser 对「不含在线算法子句」的核心 DDL 做结构校验，
/// 避免手工拼接时出现语法或语义漂移。
fn parse_single_alter(core_sql: &str) -> Result<(), OpsError> {
    let statements = Parser::parse_sql(&MySqlDialect {}, core_sql)
        .map_err(|e| OpsError::Validation(format!("DDL 语法校验失败: {e}; sql={core_sql}")))?;
    if statements.len() != 1 {
        return Err(OpsError::Validation(format!(
            "DDL 必须且只能是一条语句，实际 {} 条",
            statements.len()
        )));
    }
    match &statements[0] {
        sqlparser::ast::Statement::AlterTable { .. } => Ok(()),
        _ => Err(OpsError::Validation(
            "DDL 必须是 ALTER TABLE 语句".to_string(),
        )),
    }
}

fn compile_create_index(
    snapshot: &TableSnapshot,
    table: &str,
    name: &str,
    columns: &[String],
    unique: bool,
    using: Option<&str>,
) -> Result<CompiledAction, OpsError> {
    ensure_same_table(snapshot, table)?;
    ensure_safe_ident("索引名", name)?;
    if columns.is_empty() {
        return Err(OpsError::Validation("索引列不能为空".into()));
    }
    if columns.len() > MAX_INDEX_COLUMNS {
        return Err(OpsError::Validation(format!(
            "索引列数 {} 超过上限 {}",
            columns.len(),
            MAX_INDEX_COLUMNS
        )));
    }
    let mut seen = std::collections::HashSet::new();
    for column in columns {
        ensure_safe_ident("列名", column)?;
        if !seen.insert(column.to_ascii_lowercase()) {
            return Err(OpsError::Validation(format!("索引列重复: {column}")));
        }
        let snap = snapshot.find_column(column).ok_or_else(|| {
            OpsError::Validation(format!("列 {} 不在表 {} 中", column, snapshot.table_name))
        })?;
        ensure_supported_column_type(&snap.column_type, column)?;
    }

    if let Some(existing) = snapshot.find_index(name) {
        return Err(OpsError::Validation(format!(
            "索引名 {} 已存在（现有定义: {}）",
            name,
            existing.column_names().join(",")
        )));
    }
    if let Some(existing) = snapshot
        .indexes
        .iter()
        .find(|index| index.covers_prefix(columns))
    {
        return Err(OpsError::Validation(format!(
            "已有索引 {} ({}) 可覆盖候选前缀，无需重复创建",
            existing.name,
            existing.column_names().join(",")
        )));
    }

    if unique {
        return Err(OpsError::Validation(
            "MVP 仅支持普通索引；唯一索引需要重复值扫描证据，暂不开放".into(),
        ));
    }

    let using = using.unwrap_or("BTREE").to_ascii_uppercase();
    if using != "BTREE" {
        return Err(OpsError::Validation(format!(
            "MVP 仅支持 BTREE 索引，收到 {using}"
        )));
    }

    let db = quote_ident_mysql(&snapshot.db_name);
    let table_q = quote_ident_mysql(&snapshot.table_name);
    let name_q = quote_ident_mysql(name);
    let cols = columns
        .iter()
        .map(|c| quote_ident_mysql(c))
        .collect::<Vec<_>>()
        .join(", ");
    let unique_kw = if unique { "UNIQUE " } else { "" };

    let core_sql = format!(
        "ALTER TABLE {db}.{table_q} ADD {unique_kw}INDEX {name_q} ({cols})"
    );
    parse_single_alter(&core_sql)?;

    // ALGORITHM/LOCK 是独立的 alter_option，必须用逗号与 ADD INDEX 分隔。
    let exec_sql = format!("{core_sql}, ALGORITHM=INPLACE, LOCK=NONE");
    safety::assert_sql_allowed(&exec_sql)
        .map_err(|e| OpsError::Validation(format!("编译产物被安全基线拦截: {e}")))?;

    let action = ActionSpec::CreateIndex {
        table: table.to_string(),
        name: name.to_string(),
        columns: columns.to_vec(),
        unique,
        using: Some(using),
    };
    Ok(CompiledAction {
        action_id: action.action_id(),
        action,
        risk_class: RiskClass::L1,
        sql: exec_sql,
        rollback_sql: Some(format!(
            "ALTER TABLE {db}.{table_q} DROP INDEX {name_q}"
        )),
        compensation: Some(ActionSpec::DropIndex {
            table: table.to_string(),
            name: name.to_string(),
            expected_definition: Some(format!("INDEX ({cols})")),
        }),
    })
}

fn compile_drop_index(
    snapshot: &TableSnapshot,
    table: &str,
    name: &str,
    expected_definition: Option<&str>,
) -> Result<CompiledAction, OpsError> {
    ensure_same_table(snapshot, table)?;
    ensure_safe_ident("索引名", name)?;

    // 回滚动作发生在建索引之后，旧快照里必然不存在该索引；
    // 只要带了 expected_definition（由计划生成）即可放行，执行时再按实时 schema 校验。
    if let Some(existing) = snapshot.find_index(name) {
        if let Some(expected) = expected_definition {
            let cols = existing.column_names().join(",");
            let normalized = format!("INDEX ({})", cols);
            if expected.trim() != normalized {
                return Err(OpsError::Validation(format!(
                    "索引 {} 定义已变化：期望 {}，实际 {}",
                    name, expected, normalized
                )));
            }
        }
    } else if expected_definition.is_none() {
        return Err(OpsError::Validation(format!(
            "索引 {} 不存在，且未提供 expected_definition",
            name
        )));
    }

    let db = quote_ident_mysql(&snapshot.db_name);
    let table_q = quote_ident_mysql(&snapshot.table_name);
    let name_q = quote_ident_mysql(name);
    let sql = format!("ALTER TABLE {db}.{table_q} DROP INDEX {name_q}");
    parse_single_alter(&sql)?;
    safety::assert_sql_allowed(&sql)
        .map_err(|e| OpsError::Validation(format!("编译产物被安全基线拦截: {e}")))?;

    Ok(CompiledAction {
        action_id: ActionSpec::DropIndex {
            table: table.to_string(),
            name: name.to_string(),
            expected_definition: expected_definition.map(str::to_string),
        }
        .action_id(),
        action: ActionSpec::DropIndex {
            table: table.to_string(),
            name: name.to_string(),
            expected_definition: expected_definition.map(str::to_string),
        },
        risk_class: RiskClass::L2,
        sql,
        rollback_sql: None,
        compensation: None,
    })
}

fn compile_analyze(snapshot: &TableSnapshot, table: &str) -> Result<CompiledAction, OpsError> {
    ensure_same_table(snapshot, table)?;
    let db = quote_ident_mysql(&snapshot.db_name);
    let table_q = quote_ident_mysql(&snapshot.table_name);
    let sql = format!("ANALYZE TABLE {db}.{table_q}");
    safety::assert_sql_allowed(&sql)
        .map_err(|e| OpsError::Validation(format!("编译产物被安全基线拦截: {e}")))?;
    Ok(CompiledAction {
        action_id: ActionSpec::AnalyzeTable {
            table: table.to_string(),
        }
        .action_id(),
        action: ActionSpec::AnalyzeTable {
            table: table.to_string(),
        },
        risk_class: RiskClass::L0,
        sql,
        rollback_sql: None,
        compensation: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::types::{
        ColumnSnapshot, IndexColumn, IndexDefinition,
    };

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
                ColumnSnapshot {
                    name: "payload".into(),
                    column_type: "json".into(),
                    nullable: true,
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
    fn compiles_single_and_composite_index() {
        let single = ActionSpec::CreateIndex {
            table: "orders".into(),
            name: "idx_orders_user".into(),
            columns: vec!["user_id".into()],
            unique: false,
            using: None,
        };
        let compiled = compile_action(&single, &snapshot()).unwrap();
        assert_eq!(
            compiled.sql,
            "ALTER TABLE `app`.`orders` ADD INDEX `idx_orders_user` (`user_id`), ALGORITHM=INPLACE, LOCK=NONE"
        );
        assert_eq!(compiled.risk_class, RiskClass::L1);
        assert!(compiled.rollback_sql.unwrap().contains("DROP INDEX `idx_orders_user`"));

        let composite = ActionSpec::CreateIndex {
            table: "orders".into(),
            name: "idx_orders_user_status_created".into(),
            columns: vec!["user_id".into(), "status".into(), "created_at".into()],
            unique: false,
            using: Some("btree".into()),
        };
        let compiled = compile_action(&composite, &snapshot()).unwrap();
        assert!(compiled
            .sql
            .contains("(`user_id`, `status`, `created_at`), ALGORITHM=INPLACE, LOCK=NONE"));
    }

    #[test]
    fn rejects_json_text_and_unknown_columns() {
        let json = ActionSpec::CreateIndex {
            table: "orders".into(),
            name: "idx_orders_payload".into(),
            columns: vec!["payload".into()],
            unique: false,
            using: None,
        };
        let err = compile_action(&json, &snapshot()).unwrap_err().to_string();
        assert!(err.contains("暂不支持直接建索引"));

        let unknown = ActionSpec::CreateIndex {
            table: "orders".into(),
            name: "idx_orders_missing".into(),
            columns: vec!["missing".into()],
            unique: false,
            using: None,
        };
        let err = compile_action(&unknown, &snapshot()).unwrap_err().to_string();
        assert!(err.contains("不在表"));
    }

    #[test]
    fn rejects_duplicate_name_and_covered_prefix() {
        let snapshot = TableSnapshot {
            indexes: vec![IndexDefinition {
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
            }],
            ..snapshot()
        };
        let dup = ActionSpec::CreateIndex {
            table: "orders".into(),
            name: "idx_orders_user_status".into(),
            columns: vec!["user_id".into()],
            unique: false,
            using: None,
        };
        assert!(compile_action(&dup, &snapshot).is_err());

        let covered = ActionSpec::CreateIndex {
            table: "orders".into(),
            name: "idx_orders_user".into(),
            columns: vec!["user_id".into()],
            unique: false,
            using: None,
        };
        let err = compile_action(&covered, &snapshot).unwrap_err().to_string();
        assert!(err.contains("可覆盖候选前缀"));
    }

    #[test]
    fn never_emits_l4_sql() {
        let actions = vec![
            ActionSpec::CreateIndex {
                table: "orders".into(),
                name: "idx_orders_status".into(),
                columns: vec!["status".into()],
                unique: false,
                using: None,
            },
            ActionSpec::DropIndex {
                table: "orders".into(),
                name: "idx_orders_status".into(),
                expected_definition: Some("INDEX (`status`)".into()),
            },
        ];
        for action in actions {
            let compiled =
                compile_action(&action, &TableSnapshot { indexes: vec![], ..snapshot() }).unwrap();
            assert!(
                safety::classify_blocked_sql(&compiled.sql).is_none(),
                "sql={}",
                compiled.sql
            );
            assert!(!compiled.sql.to_ascii_uppercase().contains("DROP COLUMN"));
            assert!(!compiled.sql.to_ascii_uppercase().contains("MODIFY"));
        }
    }
}
