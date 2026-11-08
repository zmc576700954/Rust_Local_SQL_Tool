//! MySQL 原始行读取与标量转换。
//!
//! `DatabaseOps::execute_query` 的 MySQL 适配器会用 `serde_json::Value` 解码每一列，
//! 普通 VARCHAR/BIGINT 会解码失败并静默变成 `Null`。ops 的证据采集统一走这里。

use sqlx::{Column, Row};
use std::collections::HashMap;

use crate::ops::error::OpsError;
use crate::ops::registry::OpsContext;

pub(crate) type RowMap = HashMap<String, String>;

pub(crate) async fn fetch_rows(ctx: &OpsContext, sql: &str) -> Result<Vec<RowMap>, OpsError> {
    let pool = ctx.db_client.mysql_pool()?;
    let rows = sqlx::query(sql).fetch_all(pool).await?;
    Ok(convert_mysql_rows(&rows))
}

/// 把 sqlx MySQL 行转换为小写键的字符串映射；供需要固定连接（如 SET SESSION）的调用方复用。
pub(crate) fn convert_mysql_rows(rows: &[sqlx::mysql::MySqlRow]) -> Vec<RowMap> {
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let count = row.columns().len();
        let mut map = HashMap::with_capacity(count);
        for index in 0..count {
            let key = row.column(index).name().to_ascii_lowercase();
            if let Some(value) = mysql_cell_to_text(row, index) {
                map.insert(key, value);
            }
        }
        out.push(map);
    }
    out
}

fn mysql_cell_to_text(row: &sqlx::mysql::MySqlRow, index: usize) -> Option<String> {
    match row.try_get::<Option<String>, _>(index) {
        Ok(Some(value)) => return Some(value),
        Ok(None) => return None,
        Err(_) => {}
    }
    // information_schema 的 COLUMN_TYPE / SHOW CREATE TABLE 等列是 TEXT 族，
    // 某些驱动元数据带 BINARY 标志，String 解码会被拒；按字节兜底。
    if let Ok(Some(value)) = row.try_get::<Option<&[u8]>, _>(index) {
        return Some(String::from_utf8_lossy(value).into_owned());
    }
    if let Ok(Some(value)) = row.try_get::<Option<i64>, _>(index) {
        return Some(value.to_string());
    }
    if let Ok(Some(value)) = row.try_get::<Option<u64>, _>(index) {
        return Some(value.to_string());
    }
    if let Ok(Some(value)) = row.try_get::<Option<f64>, _>(index) {
        return Some(value.to_string());
    }
    None
}

pub(crate) fn str_value(row: &RowMap, key: &str) -> Option<String> {
    row.get(key)
        .map(|v| v.trim())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

pub(crate) fn int_value(row: &RowMap, key: &str) -> Option<i64> {
    row.get(key).and_then(|v| v.trim().parse::<i64>().ok())
}

pub(crate) fn uint_value(row: &RowMap, key: &str) -> Option<u64> {
    row.get(key).and_then(|v| v.trim().parse::<u64>().ok())
}

pub(crate) fn float_value(row: &RowMap, key: &str) -> Option<f64> {
    row.get(key).and_then(|v| v.trim().parse::<f64>().ok())
}

pub(crate) fn bool_yes(row: &RowMap, key: &str) -> Option<bool> {
    row.get(key)
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "y" | "yes" | "1" | "true"))
}
