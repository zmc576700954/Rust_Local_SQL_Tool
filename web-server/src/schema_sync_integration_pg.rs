//! PostgreSQL 结构同步真实库集成测试（opt-in）。
//!
//! 运行方式：
//! ```bash
//! IMPORT_SQL_PG_URL='postgres://postgres:password@127.0.0.1:55432/e2e' \
//!   cargo test -p web-server --bin web-server schema_sync_integration_pg -- --nocapture
//! ```
//!
//! 测试会在同一实例创建独立目标库，构造列/类型/可空/默认值/生成列/索引/外键/
//! 跨 schema 同名表差异，验证：
//! 1. 只读对比能列出差异并标记破坏性变更；
//! 2. 未确认破坏性变更时拒绝生成 DDL，且目标库结构确实未变；
//! 3. 确认后执行 DDL，重新对比差异归零；
//! 4. 再次对比无差异、不生成任何 DDL（幂等）。

use core_lib::db::DbClient;
use core_lib::pg_schema_sync::{DestructiveChange, PgSchemaSyncEngine};
use core_lib::schema::{SchemaExtractor, SchemaResponse, TableWithDetails};
use sqlx::{Executor, PgPool};

const SOURCE_DB: &str = "it_sync_source";
const TARGET_DB: &str = "it_sync_target";

fn pg_url() -> Option<String> {
    match std::env::var("IMPORT_SQL_PG_URL") {
        Ok(url) if !url.trim().is_empty() => Some(url),
        _ => {
            eprintln!("跳过 PostgreSQL 结构同步集成测试：未设置 IMPORT_SQL_PG_URL");
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

async fn exec(pool: &PgPool, sql: &str) {
    if let Err(err) = Executor::execute(pool, sqlx::raw_sql(sql)).await {
        panic!("执行 SQL 失败: {err}\nSQL: {sql}");
    }
}

async fn schema_of(client: &DbClient) -> SchemaResponse {
    // 绕过 service 缓存，直接通过 extractor 读取真实结构。
    let tables = SchemaExtractor::get_tables(client, "")
        .await
        .expect("读取表列表失败");
    let columns = SchemaExtractor::get_columns_map(client, "")
        .await
        .expect("读取列失败");
    let indexes = SchemaExtractor::get_indexes_map(client, "")
        .await
        .expect("读取索引失败");
    let foreign_keys = SchemaExtractor::get_foreign_keys_map(client, "")
        .await
        .expect("读取外键失败");
    let views = SchemaExtractor::get_views(client, "")
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
        })
        .collect();
    SchemaResponse {
        db_name: String::new(),
        tables,
        views,
    }
}

async fn scalar_bool(pool: &PgPool, sql: &str) -> bool {
    sqlx::query_scalar::<_, bool>(sql)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|err| panic!("查询布尔值失败: {err}\nSQL: {sql}"))
}

async fn column_type(pool: &PgPool, table: &str, column: &str) -> String {
    sqlx::query_scalar::<_, String>(
        "SELECT format_type(a.atttypid, a.atttypmod)
         FROM pg_attribute a
         JOIN pg_class c ON c.oid = a.attrelid
         JOIN pg_namespace n ON n.oid = c.relnamespace
         WHERE n.nspname = 'public' AND c.relname = $1 AND a.attname = $2",
    )
    .bind(table)
    .bind(column)
    .fetch_one(pool)
    .await
    .unwrap_or_else(|err| panic!("查询列类型失败: {err}\n{table}.{column}"))
}

fn assert_has_kind(destructive: &[DestructiveChange], kind: &str, table: &str) {
    assert!(
        destructive
            .iter()
            .any(|d| d.kind == kind && d.table_name == table),
        "破坏性清单应包含 {kind} @ {table}，实际: {:?}",
        destructive
            .iter()
            .map(|d| format!("{}:{}", d.table_name, d.kind))
            .collect::<Vec<_>>()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_schema_sync_diff_execute_and_idempotent() {
    let Some(url) = pg_url() else { return };

    let admin = DbClient::new_default(&url)
        .await
        .expect("创建管理连接失败，请确认 PostgreSQL 已就绪");
    let admin_pool = PgPool::connect(&url)
        .await
        .expect("连接 PostgreSQL 失败，请确认 compose 服务已就绪");
    exec(
        &admin_pool,
        &format!("DROP DATABASE IF EXISTS {TARGET_DB} WITH (FORCE)"),
    )
    .await;
    exec(
        &admin_pool,
        &format!("DROP DATABASE IF EXISTS {SOURCE_DB} WITH (FORCE)"),
    )
    .await;
    exec(&admin_pool, &format!("CREATE DATABASE {SOURCE_DB}")).await;
    exec(&admin_pool, &format!("CREATE DATABASE {TARGET_DB}")).await;

    let source_url = with_database(&url, SOURCE_DB);
    let target_url = with_database(&url, TARGET_DB);
    let source_client = DbClient::new_default(&source_url)
        .await
        .expect("创建源库连接失败");
    let target_client = DbClient::new_default(&target_url)
        .await
        .expect("创建目标库连接失败");
    let source_pool = PgPool::connect(&source_url)
        .await
        .expect("连接源库失败");
    let target_pool = PgPool::connect(&target_url)
        .await
        .expect("连接目标库失败");

    // ── 构造差异 ─────────────────────────────────────────
    let source_setup = r#"
        DROP TABLE IF EXISTS sync_orders CASCADE;
        DROP TABLE IF EXISTS sync_users CASCADE;
        DROP TABLE IF EXISTS sync_audit_log CASCADE;
        DROP SCHEMA IF EXISTS app CASCADE;
        CREATE SCHEMA app;
        CREATE TABLE app.sync_users (id integer PRIMARY KEY, label text, tenant text);
        CREATE TABLE sync_users (
            id integer PRIMARY KEY,
            name varchar(50) DEFAULT 'new',
            email text,
            score integer,
            display_name text GENERATED ALWAYS AS (name || ':' || email) STORED
        );
        CREATE INDEX idx_sync_users_name ON sync_users (name DESC);
        CREATE INDEX idx_sync_users_name_id ON sync_users (name ASC, id DESC);
        CREATE UNIQUE INDEX idx_sync_users_email ON sync_users (email);
        CREATE TABLE sync_orders (
            id integer PRIMARY KEY,
            user_id integer,
            app_user_id integer,
            CONSTRAINT fk_sync_orders_user FOREIGN KEY (user_id) REFERENCES sync_users(id)
                ON UPDATE CASCADE ON DELETE CASCADE,
            CONSTRAINT fk_sync_orders_app_user FOREIGN KEY (app_user_id)
                REFERENCES app.sync_users(id) ON DELETE SET NULL
        );
        CREATE TABLE sync_audit_log (id integer PRIMARY KEY, note text);
    "#;
    let target_setup = r#"
        DROP TABLE IF EXISTS sync_orders CASCADE;
        DROP TABLE IF EXISTS sync_users CASCADE;
        DROP TABLE IF EXISTS sync_audit_log CASCADE;
        DROP TABLE IF EXISTS sync_obsolete CASCADE;
        DROP SCHEMA IF EXISTS app CASCADE;
        CREATE SCHEMA app;
        CREATE TABLE sync_users (
            id integer PRIMARY KEY,
            name varchar(20) NOT NULL DEFAULT 'old',
            legacy text,
            score bigint
        );
        CREATE INDEX idx_sync_users_name ON sync_users (name ASC);
        CREATE INDEX idx_sync_users_name_id ON sync_users (id ASC, name ASC);
        CREATE INDEX idx_sync_users_legacy ON sync_users (legacy);
        CREATE TABLE sync_orders (
            id integer PRIMARY KEY,
            user_id integer,
            CONSTRAINT fk_sync_orders_user FOREIGN KEY (user_id) REFERENCES sync_users(id)
                ON UPDATE NO ACTION ON DELETE NO ACTION
        );
        CREATE TABLE sync_obsolete (id integer PRIMARY KEY);
        CREATE TABLE app.sync_users (id integer PRIMARY KEY, label text);
    "#;
    exec(&source_pool, source_setup).await;
    exec(&target_pool, target_setup).await;

    // ── 1. 只读对比 ───────────────────────────────────────
    let source = schema_of(&source_client).await;
    let target = schema_of(&target_client).await;
    let first = PgSchemaSyncEngine::compare(&source, &target);

    let status = |table: &str| {
        first
            .diff
            .tables
            .iter()
            .find(|t| t.table_name == table)
            .map(|t| t.status.as_str())
            .unwrap_or("<missing>")
    };
    assert_eq!(status("sync_users"), "modified");
    assert_eq!(status("sync_orders"), "modified");
    assert_eq!(status("sync_audit_log"), "added");
    assert_eq!(status("sync_obsolete"), "removed");
    assert_eq!(status("app.sync_users"), "modified");

    // 破坏性差异必须被标记。
    assert_has_kind(&first.destructive, "drop_column", "sync_users");
    assert_has_kind(&first.destructive, "narrow_type", "sync_users");
    assert_has_kind(&first.destructive, "drop_index", "sync_users");
    assert_has_kind(&first.destructive, "drop_table", "sync_obsolete");

    println!("PG 差异清单:");
    for t in &first.diff.tables {
        println!(
            "  {} -> {} | cols +{} -{} ~{} | idx +{} -{} ~{} | fk +{} -{} ~{}",
            t.table_name,
            t.status,
            t.columns_added.len(),
            t.columns_removed.len(),
            t.columns_modified.len(),
            t.indexes_added.len(),
            t.indexes_removed.len(),
            t.indexes_modified.len(),
            t.fks_added.len(),
            t.fks_removed.len(),
            t.fks_modified.len()
        );
    }
    println!(
        "破坏性差异: {:?}",
        first
            .destructive
            .iter()
            .map(|d| format!("{}:{}", d.table_name, d.kind))
            .collect::<Vec<_>>()
    );

    let sync_users = first
        .diff
        .tables
        .iter()
        .find(|t| t.table_name == "sync_users")
        .expect("sync_users diff");
    assert!(sync_users
        .columns_added
        .iter()
        .any(|c| c.column_name == "email"));
    assert!(sync_users
        .columns_added
        .iter()
        .any(|c| c.column_name == "display_name"));
    assert!(sync_users
        .columns_removed
        .iter()
        .any(|c| c.column_name == "legacy"));
    assert!(sync_users
        .columns_modified
        .iter()
        .any(|c| c.old.column_name == "name"
            && c.old.column_type == "character varying(20)"
            && c.new.column_type == "character varying(50)"));
    assert!(sync_users
        .columns_modified
        .iter()
        .any(|c| c.old.column_name == "score"
            && c.old.column_type == "bigint"
            && c.new.column_type == "integer"));

    // ── 2. 未确认破坏性变更：拒绝且库结构不变 ──────────────
    let blocked = PgSchemaSyncEngine::generate_ddl(
        &source,
        &target,
        &["sync_users".to_string(), "sync_obsolete".to_string()],
        false,
    )
    .expect_err("存在破坏性差异时必须要求确认");
    assert_has_kind(&blocked.destructive, "drop_table", "sync_obsolete");

    assert!(
        column_type(&target_pool, "sync_users", "legacy").await == "text",
        "未确认时不得执行删除列"
    );
    assert!(
        scalar_bool(
            &target_pool,
            "SELECT to_regclass('public.sync_obsolete') IS NOT NULL"
        )
        .await,
        "未确认时不得执行删除表"
    );
    assert_eq!(
        column_type(&target_pool, "sync_users", "score").await,
        "bigint",
        "未确认时不得执行类型收窄"
    );

    // ── 3. 确认后生成并执行 DDL ───────────────────────────
    let selected: Vec<String> = vec![
        "sync_users".to_string(),
        "sync_orders".to_string(),
        "sync_audit_log".to_string(),
        "sync_obsolete".to_string(),
        "app.sync_users".to_string(),
    ];
    let plan = PgSchemaSyncEngine::generate_ddl(&source, &target, &selected, true)
        .expect("确认后应生成 DDL");
    assert!(!plan.statements.is_empty(), "应生成可执行 DDL");
    println!("生成 DDL（{} 条）:", plan.statements.len());
    for statement in &plan.statements {
        println!("  {statement}");
    }
    assert!(
        plan.statements
            .iter()
            .any(|s| s.contains("REFERENCES \"app\".\"sync_users\"")),
        "跨 schema 外键必须正确限定 schema，实际: {:?}",
        plan.statements
    );

    // 与 /backend/table/ddl 一致的单语句校验：确保前端逐条执行不会被拦截。
    for statement in &plan.statements {
        let parsed = sqlparser::parser::Parser::parse_sql(
            &sqlparser::dialect::PostgreSqlDialect {},
            statement,
        )
        .unwrap_or_else(|err| panic!("DDL 解析失败: {err}\n{statement}"));
        assert_eq!(parsed.len(), 1, "DDL 必须是单条语句: {statement}");
    }

    for statement in &plan.statements {
        exec(&target_pool, statement).await;
    }

    // ── 4. 执行后重新对比：差异必须归零 ───────────────────
    let target_after = schema_of(&target_client).await;
    let after = PgSchemaSyncEngine::compare(&source, &target_after);
    let remaining: Vec<String> = after
        .diff
        .tables
        .iter()
        .filter(|t| t.status != "unchanged")
        .map(|t| {
            format!(
                "{}:{} cols+{} -{} ~{} idx+{} -{} ~{} fk+{} -{} ~{}",
                t.table_name,
                t.status,
                t.columns_added.len(),
                t.columns_removed.len(),
                t.columns_modified.len(),
                t.indexes_added.len(),
                t.indexes_removed.len(),
                t.indexes_modified.len(),
                t.fks_added.len(),
                t.fks_removed.len(),
                t.fks_modified.len()
            )
        })
        .collect();
    assert!(
        remaining.is_empty(),
        "执行 DDL 后差异必须归零，仍有差异: {remaining:?}"
    );
    println!("执行 DDL 后差异数: {}", remaining.len());
    assert!(
        after.destructive.is_empty(),
        "归零后不应再有破坏性差异: {:?}",
        after
            .destructive
            .iter()
            .map(|d| format!("{}:{}", d.table_name, d.kind))
            .collect::<Vec<_>>()
    );

    // ── 5. 幂等：再次生成 DDL 为空 ────────────────────────
    let idempotent = PgSchemaSyncEngine::generate_ddl(&source, &target_after, &selected, true)
        .expect("无差异时生成 DDL 不应要求确认");
    assert!(
        idempotent.statements.is_empty(),
        "幂等对比不应生成任何 DDL，实际: {:?}",
        idempotent.statements
    );
    println!("二次对比生成 DDL 数: {}", idempotent.statements.len());

    // 清理目标库。
    drop(target_client);
    drop(target_pool);
    drop(source_client);
    drop(source_pool);
    exec(
        &admin_pool,
        &format!("DROP DATABASE IF EXISTS {TARGET_DB} WITH (FORCE)"),
    )
    .await;
    exec(
        &admin_pool,
        &format!("DROP DATABASE IF EXISTS {SOURCE_DB} WITH (FORCE)"),
    )
    .await;
    drop(admin);
    drop(admin_pool);
}
