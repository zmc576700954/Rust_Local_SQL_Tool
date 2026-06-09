//! MySQL 行编码 / JSON 值绑定工具
//!
//! 从 web-server/src/mysql_codec.rs 迁移至 core_lib，
//! 使 service 层可直接使用，无需依赖 web-server。

use sqlx::{mysql::MySqlRow, Column, Row, TypeInfo};

/// MySQL 列到 JSON 的解码策略
#[derive(Clone, Copy, Debug)]
pub enum MySqlJsonDecodeStrategy {
    I64,
    F64,
    DateTime,
    Date,
    Time,
    String,
    Bytes,
    Unknown,
}

/// 行编码器：从首行推断各列名 / 序号 / 解码策略，后续行复用
#[derive(Clone)]
pub struct MySqlRowJsonEncoder {
    pub columns: Vec<(String, usize, MySqlJsonDecodeStrategy)>,
}

impl MySqlRowJsonEncoder {
    pub fn from_row(row: &MySqlRow) -> Self {
        let columns = row
            .columns()
            .iter()
            .map(|col| {
                (
                    col.name().to_string(),
                    col.ordinal(),
                    mysql_json_decode_strategy(col.type_info().name()),
                )
            })
            .collect();
        Self { columns }
    }

    pub fn column_names(&self) -> Vec<String> {
        self.columns.iter().map(|(name, _, _)| name.clone()).collect()
    }
}

/// 根据 MySQL 类型名选择 JSON 解码策略
pub fn mysql_json_decode_strategy(type_name: &str) -> MySqlJsonDecodeStrategy {
    match type_name.to_ascii_lowercase().as_str() {
        "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint" | "year" => {
            MySqlJsonDecodeStrategy::I64
        }
        "float" | "double" | "decimal" | "numeric" | "real" => MySqlJsonDecodeStrategy::F64,
        "datetime" | "timestamp" => MySqlJsonDecodeStrategy::DateTime,
        "date" => MySqlJsonDecodeStrategy::Date,
        "time" => MySqlJsonDecodeStrategy::Time,
        "char" | "varchar" | "tinytext" | "text" | "mediumtext" | "longtext" | "enum" | "set"
        | "json" => MySqlJsonDecodeStrategy::String,
        "binary" | "varbinary" | "tinyblob" | "blob" | "mediumblob" | "longblob" | "bit" => {
            MySqlJsonDecodeStrategy::Bytes
        }
        _ => MySqlJsonDecodeStrategy::Unknown,
    }
}

/// 逐类型尝试解码（fallback 策略）
pub fn fallback_mysql_json_value(row: &MySqlRow, ordinal: usize) -> serde_json::Value {
    if let Ok(val) = row.try_get::<Option<i64>, _>(ordinal) {
        serde_json::json!(val)
    } else if let Ok(val) = row.try_get::<Option<f64>, _>(ordinal) {
        serde_json::json!(val)
    } else if let Ok(val) = row.try_get::<Option<bool>, _>(ordinal) {
        serde_json::json!(val)
    } else if let Ok(val) = row.try_get::<Option<chrono::NaiveDateTime>, _>(ordinal) {
        serde_json::json!(val.map(|dt| dt.to_string()))
    } else if let Ok(val) = row.try_get::<Option<chrono::NaiveDate>, _>(ordinal) {
        serde_json::json!(val.map(|d| d.to_string()))
    } else if let Ok(val) = row.try_get::<Option<chrono::NaiveTime>, _>(ordinal) {
        serde_json::json!(val.map(|t| t.to_string()))
    } else if let Ok(val) = row.try_get::<Option<String>, _>(ordinal) {
        serde_json::json!(val)
    } else if let Ok(val) = row.try_get::<Option<Vec<u8>>, _>(ordinal) {
        serde_json::json!(val.map(|bytes| String::from_utf8_lossy(&bytes).into_owned()))
    } else {
        serde_json::Value::Null
    }
}

/// 按指定策略解码单列值
pub fn mysql_json_value_by_strategy(
    row: &MySqlRow,
    ordinal: usize,
    strategy: MySqlJsonDecodeStrategy,
) -> serde_json::Value {
    let encoded = match strategy {
        MySqlJsonDecodeStrategy::I64 => row
            .try_get::<Option<i64>, _>(ordinal)
            .ok()
            .map(|val| serde_json::json!(val)),
        MySqlJsonDecodeStrategy::F64 => row
            .try_get::<Option<f64>, _>(ordinal)
            .ok()
            .map(|val| serde_json::json!(val)),
        MySqlJsonDecodeStrategy::DateTime => row
            .try_get::<Option<chrono::NaiveDateTime>, _>(ordinal)
            .ok()
            .map(|val| serde_json::json!(val.map(|dt| dt.to_string()))),
        MySqlJsonDecodeStrategy::Date => row
            .try_get::<Option<chrono::NaiveDate>, _>(ordinal)
            .ok()
            .map(|val| serde_json::json!(val.map(|d| d.to_string()))),
        MySqlJsonDecodeStrategy::Time => row
            .try_get::<Option<chrono::NaiveTime>, _>(ordinal)
            .ok()
            .map(|val| serde_json::json!(val.map(|t| t.to_string()))),
        MySqlJsonDecodeStrategy::String => row
            .try_get::<Option<String>, _>(ordinal)
            .ok()
            .map(|val| serde_json::json!(val)),
        MySqlJsonDecodeStrategy::Bytes => {
            row.try_get::<Option<Vec<u8>>, _>(ordinal).ok().map(|val| {
                serde_json::json!(val.map(|bytes| String::from_utf8_lossy(&bytes).into_owned()))
            })
        }
        MySqlJsonDecodeStrategy::Unknown => None,
    };

    encoded.unwrap_or_else(|| fallback_mysql_json_value(row, ordinal))
}

/// 将一整行编码为 JSON Object
pub fn encode_mysql_row(row: &MySqlRow, encoder: &MySqlRowJsonEncoder) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for (col_name, ordinal, strategy) in &encoder.columns {
        map.insert(
            col_name.clone(),
            mysql_json_value_by_strategy(row, *ordinal, *strategy),
        );
    }
    serde_json::Value::Object(map)
}

/// 将 JSON 绑定到 sqlx query — 处理 Null / Bool / Number / String / fallback
pub fn bind_json_value_to_query<'q>(
    query: sqlx::query::Query<'q, sqlx::MySql, sqlx::mysql::MySqlArguments>,
    val: &serde_json::Value,
) -> sqlx::query::Query<'q, sqlx::MySql, sqlx::mysql::MySqlArguments> {
    match val {
        serde_json::Value::Null => query.bind(None::<String>),
        serde_json::Value::Bool(b) => query.bind(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                query.bind(i)
            } else if let Some(f) = n.as_f64() {
                query.bind(f)
            } else {
                query.bind(n.to_string())
            }
        }
        serde_json::Value::String(s) => query.bind(s.clone()),
        _ => query.bind(val.to_string()),
    }
}