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
    let masked = mask_comments_and_literals(text);
    let target = identifier.to_ascii_lowercase();
    masked
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '$'))
        .any(|token| token.to_ascii_lowercase() == target)
}

/// 把 SQL 文本中的注释与字符串字面量替换为空格，保留可执行注释 `/*! ... */`。
///
/// 匹配策略：
/// - 标识符按 ASCII 字母/数字/`_`/`$` 边界切分，大小写不敏感；
/// - 反引号、schema 限定名（`db.tbl.col`）都按 token 命中；
/// - `--`、`#`、`/* ... */` 注释内的同名列不算引用，避免假阳性；
/// - 单引号/双引号字符串字面量内的同名列不算引用，避免假阳性；
/// - MySQL 可执行注释 `/*! ... */` 仍按代码处理，避免漏匹配。
/// - 动态 SQL（例如 `PREPARE stmt FROM 'SELECT ... owner_id ...'`）无法静态解析，
///   这里按字符串处理，属于已知的漏匹配边界（宁可少报，不制造假阳性）。
fn mask_comments_and_literals(text: &str) -> String {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum State {
        Normal,
        LineComment,
        BlockComment,
        ExecutableComment,
        SingleQuote,
        DoubleQuote,
    }

    let chars: Vec<char> = text.chars().collect();
    let mut output = String::with_capacity(text.len());
    let mut state = State::Normal;
    let mut index = 0;

    while index < chars.len() {
        let current = chars[index];
        let next = chars.get(index + 1).copied();
        match state {
            State::Normal => {
                if current == '-' && next == Some('-') {
                    output.push(' ');
                    output.push(' ');
                    index += 2;
                    state = State::LineComment;
                } else if current == '#' {
                    output.push(' ');
                    index += 1;
                    state = State::LineComment;
                } else if current == '/' && next == Some('*') {
                    if chars.get(index + 2) == Some(&'!') {
                        // `/*!` 是 MySQL 可执行注释：保留内部 SQL。
                        output.push_str("   ");
                        index += 3;
                        state = State::ExecutableComment;
                    } else {
                        output.push(' ');
                        output.push(' ');
                        index += 2;
                        state = State::BlockComment;
                    }
                } else if current == '\'' {
                    output.push(' ');
                    index += 1;
                    state = State::SingleQuote;
                } else if current == '"' {
                    output.push(' ');
                    index += 1;
                    state = State::DoubleQuote;
                } else {
                    output.push(current);
                    index += 1;
                }
            }
            State::LineComment => {
                if current == '\n' {
                    output.push('\n');
                    state = State::Normal;
                } else {
                    output.push(' ');
                }
                index += 1;
            }
            State::BlockComment => {
                if current == '*' && next == Some('/') {
                    output.push(' ');
                    output.push(' ');
                    index += 2;
                    state = State::Normal;
                } else {
                    output.push(if current == '\n' { '\n' } else { ' ' });
                    index += 1;
                }
            }
            State::ExecutableComment => {
                if current == '*' && next == Some('/') {
                    output.push(' ');
                    output.push(' ');
                    index += 2;
                    state = State::Normal;
                } else {
                    output.push(current);
                    index += 1;
                }
            }
            State::SingleQuote | State::DoubleQuote => {
                let quote = if state == State::SingleQuote {
                    '\''
                } else {
                    '"'
                };
                if current == '\\' {
                    // 反斜杠转义：连同下一个字符一起屏蔽。
                    output.push(' ');
                    if let Some(escaped) = next {
                        output.push(if escaped == '\n' { '\n' } else { ' ' });
                        index += 2;
                    } else {
                        index += 1;
                    }
                } else if current == quote {
                    if next == Some(quote) {
                        // SQL 双写引号转义。
                        output.push(' ');
                        output.push(' ');
                        index += 2;
                    } else {
                        output.push(' ');
                        index += 1;
                        state = State::Normal;
                    }
                } else {
                    output.push(if current == '\n' { '\n' } else { ' ' });
                    index += 1;
                }
            }
        }
    }
    output
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
    fn identifier_match_handles_backticks_case_and_schema_qualification() {
        assert!(contains_identifier("SELECT `Owner_ID` FROM t", "owner_id"));
        assert!(contains_identifier(
            "SELECT `app`.`orders`.`owner_id` FROM `app`.`orders`",
            "owner_id"
        ));
        assert!(contains_identifier("SELECT t.Owner_ID FROM t", "owner_id"));
        assert!(contains_identifier(
            "INSERT INTO t (owner_id) VALUES (NEW.owner_id)",
            "owner_id"
        ));
    }

    #[test]
    fn identifier_match_ignores_comments_and_string_literals() {
        assert!(!contains_identifier("SELECT 'owner_id' AS label FROM t", "owner_id"));
        assert!(!contains_identifier(
            "SELECT \"owner_id\" AS label FROM t",
            "owner_id"
        ));
        assert!(!contains_identifier("-- owner_id\nSELECT 1", "owner_id"));
        assert!(!contains_identifier("# owner_id\nSELECT 1", "owner_id"));
        assert!(!contains_identifier("/* owner_id */ SELECT 1", "owner_id"));
        assert!(!contains_identifier(
            "SELECT 'it''s not owner_id' FROM t",
            "owner_id"
        ));
        assert!(!contains_identifier(
            "SELECT 'owner_id\\'s label' FROM t",
            "owner_id"
        ));
        // 字面量之外仍有真实引用时仍应命中。
        assert!(contains_identifier(
            "SELECT 'owner_id' AS label, owner_id FROM t",
            "owner_id"
        ));
        // 可执行注释里的引用不能漏掉。
        assert!(contains_identifier("/*!40101 owner_id */ SELECT 1", "owner_id"));
    }

    #[test]
    fn matched_columns_deduplicates() {
        let columns = vec!["payload".to_string(), "id".to_string()];
        let matched = matched_columns("SELECT payload FROM t WHERE id = 1", &columns);
        assert_eq!(matched, vec!["payload", "id"]);
    }

    #[test]
    fn matched_columns_skips_literals_but_keeps_real_references() {
        let columns = vec!["owner_id".to_string()];
        assert!(matched_columns("SELECT 'owner_id' AS label FROM t", &columns).is_empty());
        assert_eq!(
            matched_columns("SELECT t.`owner_id` FROM t WHERE 'x' = 'x'", &columns),
            vec!["owner_id"]
        );
    }
}
