//! MySQL / MariaDB 结构同步回归测试（opt-in）。
//!
//! ```bash
//! IMPORT_SQL_MYSQL_URL='mysql://root:password@127.0.0.1:33306/e2e' \
//!   cargo test -p web-server --bin web-server schema_sync_integration_mysql -- --nocapture
//! ```
//!
//! 目的：确认扩展列/索引元数据（seq_in_index / sort_order / is_primary /
//! generation_expression）没有改变 MySQL 结构同步的差异计算与 DDL 输出。

use core_lib::db::DbClient;
use core_lib::schema::{SchemaExtractor, SchemaResponse, TableWithDetails};
use core_lib::tools::SyncEngine;
use sqlx::{Executor, MySqlPool};
use std::sync::atomic::{AtomicU64, Ordering};

static DB_SEQ: AtomicU64 = AtomicU64::new(0);

/// 运行内唯一的库名后缀：进程号 + 自增序号 + 纳秒时间，避免并行用例互相干扰。
fn unique_suffix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "{}_{}_{}",
        std::process::id(),
        DB_SEQ.fetch_add(1, Ordering::Relaxed),
        nanos
    )
}

fn unique_db(prefix: &str) -> String {
    format!("{prefix}_{}", unique_suffix())
}

fn mysql_url() -> Option<String> {
    match std::env::var("IMPORT_SQL_MYSQL_URL") {
        Ok(url) if !url.trim().is_empty() => Some(url),
        _ => {
            eprintln!("跳过 MySQL 结构同步回归测试：未设置 IMPORT_SQL_MYSQL_URL");
            None
        }
    }
}

fn with_database(url: &str, db: &str) -> String {
    match url.rsplit_once('/') {
        Some((base, tail)) => {
            let query = tail.find('?').map(|i| &tail[i..]).unwrap_or("");
            format!("{base}/{db}{query}")
        }
        None => format!("{url}/{db}"),
    }
}

async fn exec(pool: &MySqlPool, sql: &str) {
    if let Err(err) = Executor::execute(pool, sqlx::raw_sql(sql)).await {
        panic!("执行 SQL 失败: {err}\nSQL: {sql}");
    }
}

/// 创建本用例独占的库；同名库若存在（极端情况下的残留）先整体清掉。
async fn create_database(pool: &MySqlPool, db: &str) {
    exec(pool, &format!("DROP DATABASE IF EXISTS `{db}`")).await;
    exec(pool, &format!("CREATE DATABASE IF NOT EXISTS `{db}`")).await;
}

/// 只删除本用例创建的库；失败必须显式报错，不能静默吞掉。
async fn cleanup_database(pool: &MySqlPool, db: &str) {
    let sql = format!("DROP DATABASE IF EXISTS `{db}`");
    if let Err(err) = Executor::execute(pool, sqlx::raw_sql(&sql)).await {
        eprintln!("清理失败: 数据库 `{db}`: {err}\nSQL: {sql}");
        panic!("清理失败: 数据库 `{db}`: {err}");
    }
}

async fn schema_of(client: &DbClient, db_name: &str) -> SchemaResponse {
    let tables = SchemaExtractor::get_tables(client, db_name)
        .await
        .expect("读取表列表失败");
    let columns = SchemaExtractor::get_columns_map(client, db_name)
        .await
        .expect("读取列失败");
    let indexes = SchemaExtractor::get_indexes_map(client, db_name)
        .await
        .expect("读取索引失败");
    let foreign_keys = SchemaExtractor::get_foreign_keys_map(client, db_name)
        .await
        .expect("读取外键失败");
    let views = SchemaExtractor::get_views(client, db_name)
        .await
        .expect("读取视图失败");

    let tables = tables
        .into_iter()
        .map(|table| TableWithDetails {
            columns: columns.get(&table.table_name).cloned().unwrap_or_default(),
            indexes: indexes.get(&table.table_name).cloned().unwrap_or_default(),
            foreign_keys: foreign_keys
                .get(&table.table_name)
                .cloned()
                .unwrap_or_default(),
            table_name: table.table_name,
            without_rowid: false,
            triggers: vec![],
        })
        .collect();
    SchemaResponse {
        db_name: db_name.to_string(),
        tables,
        views,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn mysql_schema_sync_diff_still_works_with_extended_metadata() {
    let Some(url) = mysql_url() else { return };
    let source_db = unique_db("it_sync_src");
    let target_db = unique_db("it_sync_tgt");

    let admin = DbClient::new_default(&url)
        .await
        .expect("创建管理连接失败，请确认 MySQL/MariaDB 已就绪");
    let admin_pool = MySqlPool::connect(&url)
        .await
        .expect("连接 MySQL/MariaDB 失败，请确认 compose 服务已就绪");
    for db in [&source_db, &target_db] {
        create_database(&admin_pool, db).await;
    }

    let source_url = with_database(&url, &source_db);
    let target_url = with_database(&url, &target_db);
    let source_client = DbClient::new_default(&source_url)
        .await
        .expect("创建源库连接失败");
    let target_client = DbClient::new_default(&target_url)
        .await
        .expect("创建目标库连接失败");
    let source_pool = MySqlPool::connect(&source_url)
        .await
        .expect("连接源库失败");
    let target_pool = MySqlPool::connect(&target_url)
        .await
        .expect("连接目标库失败");

    exec(
        &source_pool,
        r#"CREATE TABLE users (
            id int NOT NULL AUTO_INCREMENT,
            name varchar(50) DEFAULT 'new',
            email varchar(100),
            PRIMARY KEY (id),
            KEY idx_name (name),
            UNIQUE KEY uk_email (email)
        ) ENGINE=InnoDB"#,
    )
    .await;
    exec(
        &target_pool,
        r#"CREATE TABLE users (
            id int NOT NULL AUTO_INCREMENT,
            name varchar(20) NOT NULL DEFAULT 'old',
            legacy varchar(20),
            PRIMARY KEY (id),
            KEY idx_name (id, name)
        ) ENGINE=InnoDB"#,
    )
    .await;

    let source = schema_of(&source_client, &source_db).await;
    let target = schema_of(&target_client, &target_db).await;

    // 扩展元数据必须被 MySQL 正确填充。
    let pk = source.tables[0]
        .indexes
        .iter()
        .find(|i| i.index_name == "PRIMARY")
        .expect("应识别主键索引");
    assert!(pk.is_primary);
    assert_eq!(pk.seq_in_index, 1);
    assert_eq!(pk.sort_order, "ASC");

    let (diff, _) = SyncEngine::schema_sync(&source, &target);
    let users = diff
        .tables
        .iter()
        .find(|t| t.table_name == "users")
        .expect("users diff");
    assert_eq!(users.status, "modified");
    assert!(users
        .columns_added
        .iter()
        .any(|c| c.column_name == "email"));
    assert!(users
        .columns_removed
        .iter()
        .any(|c| c.column_name == "legacy"));
    assert!(users
        .columns_modified
        .iter()
        .any(|c| c.old.column_name == "name"));
    assert!(
        !users.indexes_modified.is_empty() || !users.indexes_added.is_empty(),
        "复合索引列序/唯一索引差异应被检测到"
    );

    // 相同结构不应产生任何差异（扩展字段不能引入幽灵差异）。
    let (same, _) = SyncEngine::schema_sync(&source, &source);
    assert!(
        same.tables.iter().all(|t| t.status == "unchanged"),
        "相同结构不应产生差异: {:?}",
        same.tables
            .iter()
            .map(|t| format!("{}:{}", t.table_name, t.status))
            .collect::<Vec<_>>()
    );

    let ddl = core_lib::sync::SchemaSyncEngine::generate_ddl_for_selection(
        &source,
        &target,
        &["users".to_string()],
    );
    assert!(ddl.contains("users"), "应为 users 生成 DDL，实际: {ddl}");

    drop(target_client);
    drop(target_pool);
    drop(source_client);
    drop(source_pool);
    for db in [&source_db, &target_db] {
        cleanup_database(&admin_pool, db).await;
    }
    drop(admin);
    drop(admin_pool);
}
