//! 目标表 schema 草案推断与建表 DDL 生成。
//!
//! 只生成脚本，不执行 DDL。推断结果永远标注为 `draft`，必须人工确认。

use std::collections::BTreeMap;
use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::sql::util::{quote_ident_mysql, validate_identifier};

use super::types::{ColumnProfile, FieldGovernanceInput, TargetSpec};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetColumn {
    pub name: String,
    pub source_key: Option<String>,
    pub sql_type: String,
    pub nullable: bool,
    pub required: bool,
    pub note: Option<String>,
}

pub fn build_target_columns(
    input: &FieldGovernanceInput,
    profile: &ColumnProfile,
) -> Vec<TargetColumn> {
    let target = input.target.clone().unwrap_or_default();
    if !target.mappings.is_empty() {
        return target
            .mappings
            .iter()
            .map(|mapping| {
                let key = mapping
                    .source_key
                    .clone()
                    .or_else(|| Some(mapping.target_column.clone()));
                let inferred = key
                    .as_deref()
                    .and_then(|key| find_key_types(profile, key))
                    .map(infer_json_type)
                    .unwrap_or_else(|| "JSON".to_string());
                let sql_type = mapping.target_type.clone().unwrap_or(inferred);
                TargetColumn {
                    name: mapping.target_column.clone(),
                    source_key: mapping.source_key.clone(),
                    sql_type,
                    nullable: !mapping.required.unwrap_or(false),
                    required: mapping.required.unwrap_or(false),
                    note: Some("来自用户列映射".into()),
                }
            })
            .collect();
    }

    if !profile.json_key_types.is_empty() {
        let mut used: HashSet<String> = HashSet::new();
        let required = input
            .declarations
            .iter()
            .flat_map(|d| d.required_keys.iter().cloned())
            .collect::<HashSet<_>>();
        return profile
            .json_key_types
            .iter()
            .map(|key| {
                let mut name = sanitize_identifier(&key.key);
                while !used.insert(name.clone()) {
                    name.push('_');
                }
                TargetColumn {
                    name,
                    source_key: Some(key.key.clone()),
                    sql_type: infer_json_type(&key.types),
                    nullable: !required.contains(&key.key),
                    required: required.contains(&key.key),
                    note: Some("按 JSON key 类型分布推断（草案）".into()),
                }
            })
            .collect();
    }

    vec![TargetColumn {
        name: "value".into(),
        source_key: None,
        sql_type: infer_scalar_type(profile),
        nullable: profile.nullable,
        required: false,
        note: Some("源列为标量，默认映射到 value".into()),
    }]
}

fn find_key_types<'a>(
    profile: &'a ColumnProfile,
    key: &str,
) -> Option<&'a BTreeMap<String, u64>> {
    profile
        .json_key_types
        .iter()
        .find(|item| item.key.eq_ignore_ascii_case(key))
        .map(|item| &item.types)
}

pub fn infer_json_type(types: &BTreeMap<String, u64>) -> String {
    let non_null: HashSet<&str> = types
        .keys()
        .map(String::as_str)
        .filter(|t| *t != "NULL")
        .collect();
    if non_null.is_empty() {
        return "JSON".into();
    }
    if non_null.len() > 1 {
        // 类型漂移：默认落 residual JSON，避免静默强转。
        return "JSON".into();
    }
    match *non_null.iter().next().unwrap() {
        "STRING" => "VARCHAR(512)".into(),
        "INTEGER" => "BIGINT".into(),
        "DOUBLE" => "DECIMAL(20,6)".into(),
        "BOOLEAN" => "TINYINT(1)".into(),
        "OBJECT" | "ARRAY" => "JSON".into(),
        _ => "JSON".into(),
    }
}

pub fn infer_scalar_type(profile: &ColumnProfile) -> String {
    match profile.data_type.as_str() {
        "json" => "JSON".into(),
        "tinyint" => "TINYINT".into(),
        "smallint" => "SMALLINT".into(),
        "mediumint" => "MEDIUMINT".into(),
        "int" | "integer" => "INT".into(),
        "bigint" => "BIGINT".into(),
        "decimal" | "numeric" => format!(
            "DECIMAL({},{})",
            profile.numeric_precision.unwrap_or(20),
            profile.numeric_scale.unwrap_or(6)
        ),
        "float" => "FLOAT".into(),
        "double" | "real" => "DOUBLE".into(),
        "date" => "DATE".into(),
        "datetime" | "timestamp" => "DATETIME".into(),
        "time" => "TIME".into(),
        "year" => "YEAR".into(),
        "char" => format!("CHAR({})", profile.char_max_length.unwrap_or(64)),
        "varchar" => {
            let len = profile
                .char_max_length
                .unwrap_or(255)
                .max(profile.max_char_length)
                .min(65535);
            format!("VARCHAR({len})")
        }
        "tinytext" | "text" | "mediumtext" | "longtext" => "TEXT".into(),
        "bool" | "boolean" => "TINYINT(1)".into(),
        _ => "TEXT".into(),
    }
}

pub fn sanitize_identifier(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push('_');
        }
    }
    out = out.trim_matches('_').to_string();
    if out.is_empty() {
        out.push_str("field");
    }
    if out.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
        out.insert(0, 'f');
    }
    out
}

pub fn create_table_sql(
    db: &str,
    target: &TargetSpec,
    target_table: &str,
    pk_column: &str,
    pk_type: &str,
    columns: &[TargetColumn],
) -> String {
    let mut lines = Vec::new();
    lines.push(format!(
        "  {} {} NOT NULL",
        quote_ident_mysql(pk_column),
        pk_type
    ));
    for column in columns {
        lines.push(format!(
            "  {} {} {}",
            quote_ident_mysql(&column.name),
            column.sql_type,
            if column.nullable { "NULL" } else { "NOT NULL" }
        ));
    }
    if target.residual_json() {
        lines.push("  `residual_json` JSON NULL".into());
    }
    lines.push(format!("  PRIMARY KEY ({})", quote_ident_mysql(pk_column)));

    let collation = target
        .collation
        .as_deref()
        .filter(|c| {
            c.chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        })
        .map(|c| format!(" COLLATE={c}"))
        .unwrap_or_default();

    format!(
        "-- 阶段 L2：建表（草案，必须人工确认列类型/长度）\n\
         CREATE TABLE IF NOT EXISTS {}.{} (\n{}\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4{};",
        quote_ident_mysql(db),
        quote_ident_mysql(target_table),
        lines.join(",\n"),
        collation
    )
}

pub fn target_identifier_valid(name: &str) -> bool {
    validate_identifier(name).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn types(entries: &[(&str, u64)]) -> BTreeMap<String, u64> {
        entries
            .iter()
            .map(|(key, value)| (key.to_string(), *value))
            .collect()
    }

    #[test]
    fn infer_json_type_handles_each_primitive() {
        assert_eq!(infer_json_type(&types(&[("STRING", 10)])), "VARCHAR(512)");
        assert_eq!(infer_json_type(&types(&[("INTEGER", 10)])), "BIGINT");
        assert_eq!(infer_json_type(&types(&[("DOUBLE", 10)])), "DECIMAL(20,6)");
        assert_eq!(infer_json_type(&types(&[("BOOLEAN", 10)])), "TINYINT(1)");
        assert_eq!(infer_json_type(&types(&[("OBJECT", 10)])), "JSON");
    }

    #[test]
    fn infer_json_type_mixed_drift_falls_back_to_json() {
        assert_eq!(
            infer_json_type(&types(&[("STRING", 5), ("INTEGER", 3)])),
            "JSON"
        );
    }

    #[test]
    fn infer_json_type_null_only_falls_back_to_json() {
        assert_eq!(infer_json_type(&types(&[("NULL", 10)])), "JSON");
    }

    #[test]
    fn sanitize_identifier_is_stable_and_unique_friendly() {
        assert_eq!(sanitize_identifier("user.theme"), "user_theme");
        assert_eq!(sanitize_identifier("123key"), "f123key");
        assert_eq!(sanitize_identifier(""), "field");
    }

    #[test]
    fn create_table_sql_marks_primary_key_and_residual() {
        let target = TargetSpec {
            residual_json: Some(true),
            collation: Some("utf8mb4_bin".into()),
            ..Default::default()
        };
        let columns = vec![TargetColumn {
            name: "theme".into(),
            source_key: Some("theme".into()),
            sql_type: "VARCHAR(512)".into(),
            nullable: true,
            required: false,
            note: None,
        }];
        let sql = create_table_sql("app", &target, "orders_ext", "id", "BIGINT", &columns);
        assert!(sql.contains("CREATE TABLE IF NOT EXISTS `app`.`orders_ext`"));
        assert!(sql.contains("PRIMARY KEY (`id`)"));
        assert!(sql.contains("`residual_json` JSON NULL"));
        assert!(sql.contains("COLLATE=utf8mb4_bin"));
    }
}
