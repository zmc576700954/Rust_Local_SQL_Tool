//! E03-E06：索引/唯一约束/外键/生成列/视图/触发器/例程依赖扫描。
//!
//! 只读 information_schema。任何一项读取失败都会记录 gap，由风险引擎按
//! 「不可判定 = 阻断写步骤」处理。

use std::collections::HashSet;

use crate::ops::mysql_rows::str_value;
use crate::ops::registry::OpsContext;
use crate::ops::types::TableSnapshot;
use crate::sql::util::escape_sql_string;

use super::read_only::fetch_read_only;
use super::types::{
    DependencyEvidence, DependencyObject, ForeignKeyReference, GeneratedColumnReference,
    IndexReference,
};

pub async fn load_dependencies(
    ctx: &OpsContext,
    table: &str,
    columns: &[String],
    snapshot: &TableSnapshot,
) -> DependencyEvidence {
    let db = escape_sql_string(&ctx.db_name);
    let tbl = escape_sql_string(table);

    let mut evidence = DependencyEvidence::default();
    evidence.scan_complete = true;

    // E03：索引与唯一约束（来自既有 snapshot，避免重复查询）。
    let mut primary_key_columns = Vec::new();
    for index in &snapshot.indexes {
        let cols = index.column_names();
        let primary = index.name.eq_ignore_ascii_case("PRIMARY");
        if primary {
            primary_key_columns = cols.clone();
        }
        evidence.indexes.push(IndexReference {
            name: index.name.clone(),
            unique: index.unique,
            primary,
            index_type: index.index_type.clone(),
            columns: cols,
        });
    }
    evidence.primary_key_columns = primary_key_columns.clone();
    evidence.single_column_pk = if primary_key_columns.len() == 1 {
        Some(primary_key_columns[0].clone())
    } else {
        None
    };

    // E04：作为子表的外键。
    let child_sql = format!(
        "SELECT CONSTRAINT_NAME, TABLE_NAME, COLUMN_NAME, REFERENCED_TABLE_NAME, REFERENCED_COLUMN_NAME \
         FROM information_schema.KEY_COLUMN_USAGE \
         WHERE TABLE_SCHEMA = '{db}' AND TABLE_NAME = '{tbl}' AND REFERENCED_TABLE_NAME IS NOT NULL"
    );
    match fetch_read_only(ctx, &child_sql).await {
        Ok(rows) => {
            for row in rows {
                let Some(constraint_name) = str_value(&row, "constraint_name") else {
                    continue;
                };
                let Some(column) = str_value(&row, "column_name") else {
                    continue;
                };
                evidence.foreign_keys.push(ForeignKeyReference {
                    constraint_name,
                    direction: "child".into(),
                    table: table.to_string(),
                    column,
                    referenced_table: str_value(&row, "referenced_table_name"),
                    referenced_column: str_value(&row, "referenced_column_name"),
                });
            }
        }
        Err(err) => {
            evidence.scan_complete = false;
            evidence
                .gaps
                .push(format!("E04 子表外键读取失败: {err}"));
        }
    }

    // E04：作为父表被其他表引用。
    let parent_sql = format!(
        "SELECT CONSTRAINT_NAME, TABLE_NAME, COLUMN_NAME, REFERENCED_TABLE_NAME, REFERENCED_COLUMN_NAME \
         FROM information_schema.KEY_COLUMN_USAGE \
         WHERE REFERENCED_TABLE_SCHEMA = '{db}' AND REFERENCED_TABLE_NAME = '{tbl}'"
    );
    match fetch_read_only(ctx, &parent_sql).await {
        Ok(rows) => {
            for row in rows {
                let Some(constraint_name) = str_value(&row, "constraint_name") else {
                    continue;
                };
                let Some(column) = str_value(&row, "referenced_column_name") else {
                    continue;
                };
                evidence.foreign_keys.push(ForeignKeyReference {
                    constraint_name,
                    direction: "parent".into(),
                    table: str_value(&row, "table_name").unwrap_or_default(),
                    column,
                    referenced_table: Some(table.to_string()),
                    referenced_column: str_value(&row, "referenced_column_name"),
                });
            }
        }
        Err(err) => {
            evidence.scan_complete = false;
            evidence
                .gaps
                .push(format!("E04 父表外键读取失败: {err}"));
        }
    }

    // E05：生成列（从 COLUMNS 元数据读取）。
    let generated_sql = format!(
        "SELECT COLUMN_NAME, GENERATION_EXPRESSION, EXTRA \
         FROM information_schema.COLUMNS \
         WHERE TABLE_SCHEMA = '{db}' AND TABLE_NAME = '{tbl}' \
           AND GENERATION_EXPRESSION IS NOT NULL AND GENERATION_EXPRESSION <> ''"
    );
    match fetch_read_only(ctx, &generated_sql).await {
        Ok(rows) => {
            for row in rows {
                let Some(name) = str_value(&row, "column_name") else {
                    continue;
                };
                let expression = str_value(&row, "generation_expression").unwrap_or_default();
                evidence.generated_columns.push(GeneratedColumnReference {
                    name,
                    expression,
                    extra: str_value(&row, "extra").unwrap_or_default(),
                });
            }
        }
        Err(err) => {
            evidence.scan_complete = false;
            evidence
                .gaps
                .push(format!("E05 生成列读取失败: {err}"));
        }
    }

    // E06：视图定义。
    let views_sql = format!(
        "SELECT TABLE_NAME, VIEW_DEFINITION FROM information_schema.VIEWS WHERE TABLE_SCHEMA = '{db}'"
    );
    match fetch_read_only(ctx, &views_sql).await {
        Ok(rows) => {
            for row in rows {
                let Some(name) = str_value(&row, "table_name") else {
                    continue;
                };
                let definition = str_value(&row, "view_definition").unwrap_or_default();
                let matched = matched_columns(&definition, columns);
                if !matched.is_empty() {
                    evidence.objects.push(DependencyObject {
                        kind: "view".into(),
                        name,
                        matched_columns: matched,
                    });
                }
            }
        }
        Err(err) => {
            evidence.scan_complete = false;
            evidence.gaps.push(format!("E06 视图读取失败: {err}"));
        }
    }

    // E06：触发器（同时记录源表上的任意触发器）。
    let triggers_sql = format!(
        "SELECT TRIGGER_NAME, EVENT_OBJECT_TABLE, ACTION_STATEMENT \
         FROM information_schema.TRIGGERS WHERE EVENT_OBJECT_SCHEMA = '{db}'"
    );
    match fetch_read_only(ctx, &triggers_sql).await {
        Ok(rows) => {
            let mut seen = HashSet::new();
            for row in rows {
                let Some(name) = str_value(&row, "trigger_name") else {
                    continue;
                };
                let object_table = str_value(&row, "event_object_table").unwrap_or_default();
                let statement = str_value(&row, "action_statement").unwrap_or_default();
                if object_table.eq_ignore_ascii_case(table) {
                    evidence.source_triggers.push(name.clone());
                }
                let matched = matched_columns(&statement, columns);
                if !matched.is_empty() && seen.insert(name.clone()) {
                    evidence.objects.push(DependencyObject {
                        kind: "trigger".into(),
                        name,
                        matched_columns: matched,
                    });
                }
            }
            evidence.source_triggers.sort();
            evidence.source_triggers.dedup();
        }
        Err(err) => {
            evidence.scan_complete = false;
            evidence
                .gaps
                .push(format!("E06 触发器读取失败: {err}"));
        }
    }

    // E06：存储过程/函数。
    let routines_sql = format!(
        "SELECT ROUTINE_NAME, ROUTINE_DEFINITION FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA = '{db}'"
    );
    match fetch_read_only(ctx, &routines_sql).await {
        Ok(rows) => {
            for row in rows {
                let Some(name) = str_value(&row, "routine_name") else {
                    continue;
                };
                let definition = str_value(&row, "routine_definition").unwrap_or_default();
                let matched = matched_columns(&definition, columns);
                if !matched.is_empty() {
                    evidence.objects.push(DependencyObject {
                        kind: "routine".into(),
                        name,
                        matched_columns: matched,
                    });
                }
            }
        }
        Err(err) => {
            evidence.scan_complete = false;
            evidence
                .gaps
                .push(format!("E06 例程读取失败: {err}"));
        }
    }

    evidence
}

/// 在 SQL/表达式文本中按标识符边界匹配列名。
pub fn matched_columns(text: &str, columns: &[String]) -> Vec<String> {
    let mut matched = Vec::new();
    for column in columns {
        if contains_identifier(text, column) && !matched.contains(column) {
            matched.push(column.clone());
        }
    }
    matched
}

pub fn contains_identifier(text: &str, identifier: &str) -> bool {
    if identifier.trim().is_empty() {
        return false;
    }
    let target = identifier.to_ascii_lowercase();
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '$'))
        .any(|token| token.to_ascii_lowercase() == target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifier_match_respects_boundaries() {
        assert!(contains_identifier("SELECT JSON_EXTRACT(payload, '$.a')", "payload"));
        assert!(!contains_identifier("SELECT payload_extra FROM t", "payload"));
        assert!(!contains_identifier("SELECT t.identity FROM t", "id"));
        assert!(contains_identifier("SELECT t.`id` FROM t", "id"));
    }

    #[test]
    fn matched_columns_deduplicates() {
        let columns = vec!["payload".to_string(), "id".to_string()];
        let matched = matched_columns("SELECT payload FROM t WHERE id = 1", &columns);
        assert_eq!(matched, vec!["payload", "id"]);
    }
}
