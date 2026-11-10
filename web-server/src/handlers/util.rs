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

/// 导入/导出的引擎守卫：MySQL 与 PostgreSQL 已接通，SQLite 暂未支持。
pub fn require_file_transfer_client(db_client: &DbClient, feature: &str) -> Result<(), AppError> {
    match &db_client.db_type {
        DbType::MySQL | DbType::MariaDB | DbType::PostgreSQL => Ok(()),
        other => Err(AppError::BadRequest(format!(
            "{} 当前仅支持 MySQL / MariaDB / PostgreSQL；当前连接为 {:?}，请切换到已支持的连接后再使用该功能。",
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

/// 连接配置层面的能力守卫：按连接实际引擎查询能力位。
///
/// 与 `require_mysql_connection` 的区别：这里不再写死 MySQL，
/// 而是以 `DbCapabilities` 为准（例如 data_sync 在 PG 接通后为 true）。
pub fn require_connection_capability(
    conn: &DbConnection,
    capability: &str,
    feature: &str,
) -> Result<(), AppError> {
    let db_type = conn
        .db_type
        .clone()
        .or_else(|| DbType::from_url(&conn.url))
        .unwrap_or(DbType::MySQL);
    let caps = core_lib::db::capability::DbCapabilities::runtime_capabilities(&db_type);
    caps.check_capability(capability).map_err(|e| {
        AppError::BadRequest(format!(
            "{}：{}（当前连接为 {:?}），请切换到已支持该能力的连接后再使用",
            feature, e, db_type
        ))
    })
}

/// 导出前按目录统计估算行数与平均行宽（best effort）。
///
/// 只做轻量目录查询；统计信息缺失时才回退到一次有 3 秒上限的采样，
/// 超时则放弃估算，由流式导出和内存/文件上限兜底。
pub async fn estimate_export_size(
    db_client: &DbClient,
    table_name: &str,
) -> Option<core_lib::tools::ExportEstimate> {
    let (rows, avg_row_bytes) = match db_client.db_type {
        DbType::PostgreSQL => {
            let pool = db_client.pool.pg().ok()?;
            let table_ident = core_lib::sql_util::quote_table_ref_checked(
                table_name,
                &DbType::PostgreSQL,
            )
            .ok()?;
            let stats = sqlx::query(
                r#"
                SELECT c.reltuples::bigint AS est_rows,
                       COALESCE((
                           SELECT sum(s.avg_width)::bigint
                           FROM pg_stats s
                           WHERE s.schemaname = n.nspname AND s.tablename = c.relname
                       ), 0) AS avg_width
                FROM pg_class c
                JOIN pg_namespace n ON n.oid = c.relnamespace
                WHERE c.oid = to_regclass($1)
                "#,
            )
            .bind(&table_ident)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten()?;
            let est_rows: i64 = stats.try_get("est_rows").ok()?;
            let avg_width: i64 = stats.try_get("avg_width").unwrap_or(0);

            let rows = if est_rows >= 0 {
                est_rows as u64
            } else {
                let row = tokio::time::timeout(
                    std::time::Duration::from_secs(3),
                    sqlx::query(&format!("SELECT count(*)::bigint AS n FROM {}", table_ident))
                        .fetch_one(pool),
                )
                .await
                .ok()?
                .ok()?;
                row.try_get::<i64, _>("n").ok()? as u64
            };

            let avg = if avg_width > 0 {
                avg_width as u64
            } else {
                let row = tokio::time::timeout(
                    std::time::Duration::from_secs(3),
                    sqlx::query(&format!(
                        "SELECT COALESCE(avg(pg_column_size(t))::bigint, 0) AS w FROM (SELECT * FROM {} LIMIT 200) t",
                        table_ident
                    ))
                    .fetch_one(pool),
                )
                .await
                .ok()?
                .ok()?;
                row.try_get::<i64, _>("w").unwrap_or(0) as u64
            };
            (rows, avg)
        }
        DbType::MySQL | DbType::MariaDB => {
            let pool = db_client.pool.mysql().ok()?;
            let row = sqlx::query(
                "SELECT table_rows AS est_rows, avg_row_length AS avg_width \
                 FROM information_schema.tables \
                 WHERE table_schema = DATABASE() AND table_name = ?",
            )
            .bind(table_name)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten()?;
            let rows: i64 = row.try_get("est_rows").unwrap_or(0);
            let avg: i64 = row.try_get("avg_width").unwrap_or(0);
            (rows.max(0) as u64, avg.max(0) as u64)
        }
        _ => return None,
    };

    Some(core_lib::tools::ExportEstimate {
        rows,
        avg_row_bytes: avg_row_bytes.max(1),
    })
}

/// 明显超过安全阈值时在启动阶段直接拒绝，避免用户等待数分钟后才失败。
pub fn ensure_export_estimate(
    estimate: Option<&core_lib::tools::ExportEstimate>,
    max_bytes: u64,
) -> Result<(), AppError> {
    let Some(est) = estimate else {
        return Ok(());
    };
    if est.exceeds(max_bytes) {
        return Err(AppError::ExportTooLarge {
            estimated_rows: est.rows,
            avg_row_bytes: est.avg_row_bytes,
            estimated_bytes: est.estimated_bytes(),
            max_bytes,
        });
    }
    Ok(())
}

/// 导出并发闸门：全局同一时刻只允许一个导出任务，避免多个任务同时吃内存。
pub fn acquire_export_slot(
    state: &crate::state::AppState,
) -> Result<tokio::sync::OwnedSemaphorePermit, AppError> {
    state
        .export_semaphore
        .clone()
        .try_acquire_owned()
        .map_err(|_| {
            AppError::TooManyRequests(
                "导出并发上限为 1，已有导出任务正在执行，请稍后重试".to_string(),
            )
        })
}

/// 按列顺序把 PostgreSQL 行转换成 JSON 对象（用于流式导出）。
pub fn pg_row_to_json_with_catalog(
    row: &sqlx::postgres::PgRow,
    headers: &[String],
    catalog: &std::collections::HashMap<u32, core_lib::sql_util::PgTypeMeta>,
) -> serde_json::Map<String, serde_json::Value> {
    let mut map = serde_json::Map::with_capacity(headers.len());
    for (idx, name) in headers.iter().enumerate() {
        map.insert(
            name.clone(),
            core_lib::sql_util::pg_cell_to_value_with_catalog(row, idx, catalog),
        );
    }
    map
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
