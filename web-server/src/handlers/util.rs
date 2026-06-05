use sqlx::Column;
use sqlx::Row;

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
