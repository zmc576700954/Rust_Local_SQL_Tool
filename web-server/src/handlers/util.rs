use core_lib::config::{DbConnection, DbType};
use core_lib::db::client::DbClient;
use core_lib::error::AppError;
use sqlx::Column;
use sqlx::Row;

/// 运维中心 / 性能探针 / 同步表等功能当前实现依赖 MySQL 的
/// `information_schema`、`performance_schema` 与 MySQL 方言。
///
/// 非 MySQL 连接在这里返回带引擎名的明确提示，而不是让 `mysql_pool()?`
/// 冒出一个底层驱动错误。
pub fn require_mysql_client(db_client: &DbClient, feature: &str) -> Result<(), AppError> {
    match &db_client.db_type {
        DbType::MySQL | DbType::MariaDB => Ok(()),
        other => Err(AppError::BadRequest(format!(
            "{} 当前仅支持 MySQL / MariaDB（依赖 MySQL 的 information_schema 与 performance_schema 元数据）；当前连接为 {:?}，请切换到 MySQL 连接后再使用该功能。",
            feature, other
        ))),
    }
}

/// 连接配置层面的同类守卫：用于尚未打开连接池、只解析出 URL 的入口。
pub fn require_mysql_connection(conn: &DbConnection, feature: &str) -> Result<(), AppError> {
    let db_type = conn
        .db_type
        .clone()
        .or_else(|| DbType::from_url(&conn.url))
        .unwrap_or(DbType::MySQL);
    match db_type {
        DbType::MySQL | DbType::MariaDB => Ok(()),
        other => Err(AppError::BadRequest(format!(
            "{} 当前仅支持 MySQL / MariaDB（依赖 MySQL 的 information_schema 与 performance_schema 元数据）；当前连接为 {:?}，请切换到 MySQL 连接后再使用该功能。",
            feature, other
        ))),
    }
}

/// Convert a single MySQL row to a JSON map, handling common column types.
pub fn row_to_json(row: &sqlx::mysql::MySqlRow) -> serde_json::Map<String, serde_json::Value> {
    let mut map = serde_json::Map::new();
    for col in row.columns() {
        let col_name = col.name().to_string();
        if let Ok(val) = row.try_get::<Option<i64>, _>(col.ordinal()) {
            map.insert(col_name, serde_json::json!(val));
        } else if let Ok(val) = row.try_get::<Option<f64>, _>(col.ordinal()) {
            map.insert(col_name, serde_json::json!(val));
        } else if let Ok(val) = row.try_get::<Option<bool>, _>(col.ordinal()) {
            map.insert(col_name, serde_json::json!(val));
        } else if let Ok(val) = row.try_get::<Option<chrono::NaiveDateTime>, _>(col.ordinal()) {
            map.insert(col_name, serde_json::json!(val.map(|dt| dt.to_string())));
        } else if let Ok(val) = row.try_get::<Option<chrono::NaiveDate>, _>(col.ordinal()) {
            map.insert(col_name, serde_json::json!(val.map(|d| d.to_string())));
        } else if let Ok(val) = row.try_get::<Option<chrono::NaiveTime>, _>(col.ordinal()) {
            map.insert(col_name, serde_json::json!(val.map(|t| t.to_string())));
        } else if let Ok(val) = row.try_get::<Option<String>, _>(col.ordinal()) {
            map.insert(col_name, serde_json::json!(val));
        } else {
            let val: Option<Vec<u8>> = row.try_get(col.ordinal()).unwrap_or(None);
            if let Some(bytes) = val {
                let s = String::from_utf8_lossy(&bytes).into_owned();
                map.insert(col_name, serde_json::json!(s));
            } else {
                map.insert(col_name, serde_json::Value::Null);
            }
        }
    }
    map
}
