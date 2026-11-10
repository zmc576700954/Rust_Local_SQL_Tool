//! PostgreSQL 原始行读取与标量转换。
//!
//! 与 `mysql_rows` 对应：巡检/性能诊断的证据采集统一拿「列名小写 → 文本」的行映射，
//! 避免各检查项各自处理 sqlx 类型解码。所有查询都应显式 `::text` 转换，
//! 这里再对 JSON 值做一次兜底。

use std::collections::HashMap;

use sqlx::{Column, Row};

use crate::ops::error::OpsError;
use crate::ops::mysql_rows::RowMap;
use crate::ops::registry::OpsContext;
use crate::sql_util::{pg_cell_to_value, UNMAPPED_VALUE_KEY};

pub(crate) async fn fetch_rows(ctx: &OpsContext, sql: &str) -> Result<Vec<RowMap>, OpsError> {
    let pool = ctx.db_client.pool.pg()?;
    let rows = sqlx::query(sql).fetch_all(pool).await?;
    Ok(convert_pg_rows(&rows))
}

pub(crate) fn convert_pg_rows(rows: &[sqlx::postgres::PgRow]) -> Vec<RowMap> {
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let count = row.columns().len();
        let mut map = HashMap::with_capacity(count);
        for index in 0..count {
            let key = row.column(index).name().to_ascii_lowercase();
            if let Some(value) = json_to_text(&pg_cell_to_value(row, index)) {
                map.insert(key, value);
            }
        }
        out.push(map);
    }
    out
}

fn json_to_text(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Null => None,
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Bool(flag) => Some(if *flag { "true" } else { "false" }.to_string()),
        serde_json::Value::Number(number) => Some(number.to_string()),
        serde_json::Value::Array(_) => Some(value.to_string()),
        serde_json::Value::Object(map) => map
            .get(UNMAPPED_VALUE_KEY)
            .and_then(|inner| inner.as_str())
            .map(|text| text.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_to_text_follows_row_map_conventions() {
        assert_eq!(json_to_text(&serde_json::Value::Null), None);
        assert_eq!(json_to_text(&serde_json::json!("keep")), Some("keep".into()));
        assert_eq!(json_to_text(&serde_json::json!(true)), Some("true".into()));
        assert_eq!(json_to_text(&serde_json::json!(42)), Some("42".into()));
        let unmapped = serde_json::json!({ UNMAPPED_VALUE_KEY: "(1,abc)" });
        assert_eq!(json_to_text(&unmapped), Some("(1,abc)".into()));
    }
}
