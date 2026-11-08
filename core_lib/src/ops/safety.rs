//! 安全基线：L4 危险 SQL 识别。
//!
//! Agent 链路只能走 `ActionSpec -> ddl_compiler`，本模块作为纵深防御，
//! 在执行前对最终 SQL 做一次硬拦截：DROP COLUMN/TABLE、TRUNCATE、类型变更、
//! GRANT/REVOKE、SET GLOBAL 一律不允许自动执行。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockedSqlKind {
    DropColumn,
    DropTable,
    DropDatabase,
    Truncate,
    ColumnTypeChange,
    GrantOrRevoke,
    SetGlobal,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockedSql {
    pub kind: BlockedSqlKind,
    pub message: String,
}

impl BlockedSqlKind {
    pub fn code(&self) -> &'static str {
        match self {
            Self::DropColumn => "drop_column",
            Self::DropTable => "drop_table",
            Self::DropDatabase => "drop_database",
            Self::Truncate => "truncate",
            Self::ColumnTypeChange => "column_type_change",
            Self::GrantOrRevoke => "grant_revoke",
            Self::SetGlobal => "set_global",
            Self::Other => "other",
        }
    }
}

/// 去掉注释并压缩空白，便于做关键字判定。
fn normalize_sql(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let bytes = sql.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // 行注释 -- 和 #
        if bytes[i] == b'-' && i + 1 < bytes.len() && bytes[i + 1] == b'-' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if bytes[i] == b'#' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // 块注释 /* ... */
        if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            if i + 1 < bytes.len() {
                i += 2;
            }
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string()
}

fn tokens(sql: &str) -> Vec<String> {
    sql.to_ascii_uppercase()
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '@' || c == '$'))
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn find_seq(tokens: &[String], seq: &[&str]) -> Option<usize> {
    if seq.is_empty() || tokens.len() < seq.len() {
        return None;
    }
    (0..=tokens.len() - seq.len())
        .find(|&i| seq.iter().enumerate().all(|(j, s)| tokens[i + j] == *s))
}

/// 识别禁止自动执行的 SQL。返回 None 表示允许进入编译/执行链路。
pub fn classify_blocked_sql(sql: &str) -> Option<BlockedSql> {
    let normalized = normalize_sql(sql);
    if normalized.is_empty() {
        return None;
    }
    let tokens = tokens(&normalized);
    if tokens.is_empty() {
        return None;
    }

    let make = |kind: BlockedSqlKind, message: &str| {
        Some(BlockedSql {
            kind,
            message: message.to_string(),
        })
    };

    // DCL / 全局变量
    if matches!(tokens[0].as_str(), "GRANT" | "REVOKE") {
        return make(
            BlockedSqlKind::GrantOrRevoke,
            "GRANT/REVOKE 禁止自动执行",
        );
    }
    if find_seq(&tokens, &["SET", "GLOBAL"]).is_some()
        || find_seq(&tokens, &["SET", "@@GLOBAL"]).is_some()
    {
        return make(BlockedSqlKind::SetGlobal, "SET GLOBAL 禁止自动执行");
    }

    // 整表 / 整库毁灭性操作
    if tokens[0] == "TRUNCATE" {
        return make(BlockedSqlKind::Truncate, "TRUNCATE 禁止自动执行");
    }
    if find_seq(&tokens, &["DROP", "TABLE"]).is_some() {
        return make(BlockedSqlKind::DropTable, "DROP TABLE 禁止自动执行");
    }
    if find_seq(&tokens, &["DROP", "DATABASE"]).is_some()
        || find_seq(&tokens, &["DROP", "SCHEMA"]).is_some()
    {
        return make(BlockedSqlKind::DropDatabase, "DROP DATABASE/SCHEMA 禁止自动执行");
    }

    // ALTER TABLE 内部的列级危险操作
    if tokens[0] == "ALTER" && tokens.get(1).map(|t| t.as_str()) == Some("TABLE") {
        if let Some(pos) = find_seq(&tokens, &["MODIFY"]) {
            // MODIFY 只可能作为列变更修饰；MODIFY COLUMN 显式也会命中
            let _ = pos;
            return make(
                BlockedSqlKind::ColumnTypeChange,
                "MODIFY COLUMN 类型变更禁止自动执行",
            );
        }
        if find_seq(&tokens, &["CHANGE"]).is_some() {
            return make(
                BlockedSqlKind::ColumnTypeChange,
                "CHANGE COLUMN 类型变更禁止自动执行",
            );
        }
        if find_seq(&tokens, &["ALTER", "COLUMN"]).is_some() {
            return make(
                BlockedSqlKind::ColumnTypeChange,
                "ALTER COLUMN 类型变更禁止自动执行",
            );
        }
        if find_seq(&tokens, &["RENAME", "COLUMN"]).is_some() {
            return make(
                BlockedSqlKind::ColumnTypeChange,
                "RENAME COLUMN 禁止自动执行",
            );
        }

        // DROP 不一定是 DROP INDEX：只有紧跟 INDEX 才放行。
        if let Some(pos) = tokens.iter().position(|t| t == "DROP") {
            let next = tokens.get(pos + 1).map(|s| s.as_str()).unwrap_or("");
            let allowed = matches!(
                next,
                "INDEX" | "KEY" | "FOREIGN" | "PRIMARY" | "CHECK" | "CONSTRAINT" | "PARTITION"
            );
            if !allowed {
                return make(BlockedSqlKind::DropColumn, "DROP COLUMN 禁止自动执行");
            }
        }
    }

    None
}

/// 断言 SQL 允许进入执行链路。
pub fn assert_sql_allowed(sql: &str) -> Result<(), String> {
    match classify_blocked_sql(sql) {
        Some(blocked) => Err(format!(
            "blocked_sql:{}: {}",
            blocked.kind.code(),
            blocked.message
        )),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn l4_statements_are_blocked() {
        let cases = [
            ("ALTER TABLE t DROP COLUMN c", BlockedSqlKind::DropColumn),
            ("ALTER TABLE t DROP c", BlockedSqlKind::DropColumn),
            ("DROP TABLE t", BlockedSqlKind::DropTable),
            ("DROP DATABASE app", BlockedSqlKind::DropDatabase),
            ("TRUNCATE TABLE t", BlockedSqlKind::Truncate),
            (
                "ALTER TABLE t MODIFY COLUMN c BIGINT",
                BlockedSqlKind::ColumnTypeChange,
            ),
            (
                "ALTER TABLE t CHANGE COLUMN c c2 VARCHAR(10)",
                BlockedSqlKind::ColumnTypeChange,
            ),
            (
                "ALTER TABLE t ALTER COLUMN c TYPE BIGINT",
                BlockedSqlKind::ColumnTypeChange,
            ),
            ("GRANT ALL ON app.* TO u", BlockedSqlKind::GrantOrRevoke),
            ("REVOKE SELECT ON app.* FROM u", BlockedSqlKind::GrantOrRevoke),
            ("SET GLOBAL max_connections = 100", BlockedSqlKind::SetGlobal),
            ("SET @@GLOBAL.max_connections = 100", BlockedSqlKind::SetGlobal),
        ];
        for (sql, expected) in cases {
            let blocked = classify_blocked_sql(sql).expect("should be blocked");
            assert_eq!(blocked.kind, expected, "sql: {sql}");
        }
    }

    #[test]
    fn allowed_index_ddl_is_not_blocked() {
        for sql in [
            "ALTER TABLE `app`.`t` ADD INDEX `idx_t_c` (`c`) ALGORITHM=INPLACE, LOCK=NONE",
            "ALTER TABLE `app`.`t` ADD UNIQUE INDEX `idx_t_c` (`c`)",
            "ALTER TABLE `app`.`t` DROP INDEX `idx_t_c`",
            "ANALYZE TABLE `app`.`t`",
        ] {
            assert!(classify_blocked_sql(sql).is_none(), "sql: {sql}");
        }
    }

    #[test]
    fn comments_do_not_bypass_blocked_check() {
        assert!(classify_blocked_sql("/* keep */ DROP TABLE t").is_some());
        assert!(classify_blocked_sql("-- x\nTRUNCATE TABLE t").is_some());
    }
}
