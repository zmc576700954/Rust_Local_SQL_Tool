//! 字段回填 S2 的 PG V1–V4 校验真库验收（opt-in）。
//!
//! 运行方式：
//! ```bash
//! FIELD_MIGRATION_PG_URL='postgres://postgres:password@127.0.0.1:55432/postgres' \
//!   cargo test -p core_lib --test field_migration_verify_pg -- --test-threads=1 --no-fail-fast
//! ```
//! 未设置环境变量时打印原因并返回，不影响常规单测。
//!
//! 覆盖（对应方案第 4 节规范化口径）：
//! numeric 直接比较（1 / 1.0 / 1e3 相等、NaN 自等）、bytea 走 encode(hex)、
//! timestamptz 归一化 UTC、json 先 ::jsonb、NULL 与 '' 不合并、uuid 文本规范化、
//! 空表、REPEATABLE READ READ ONLY 快照一致性、差异分页。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use sqlx::postgres::PgPool;
use sqlx::Executor;

use core_lib::db::client::DbClient;
use core_lib::ops::field_migration::migration_store::{BackfillSpec, PkKind};
use core_lib::ops::field_migration::pg_verify::{self, PgSnapshot};
use core_lib::ops::field_migration::schema::TargetColumn;
use core_lib::ops::field_migration::types::EmptyStringPolicy;
use core_lib::ops::policy::OpsPolicy;
use core_lib::ops::registry::OpsContext;

const DB_ID: &str = "field-migration-pg-verify-e2e";

static DB_SEQ: AtomicU64 = AtomicU64::new(0);

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

fn with_database(url: &str, db: &str) -> String {
    match url.rsplit_once('/') {
        Some((base, tail)) => {
            let query = tail.find('?').map(|i| &tail[i..]).unwrap_or("");
            format!("{base}/{db}{query}")
        }
        None => format!("{url}/{db}"),
    }
}

async fn admin_exec(pool: &PgPool, sql: &str) {
    if let Err(err) = pool.execute(sql).await {
        panic!("执行管理 SQL 失败: {err}\nSQL: {sql}");
    }
}

async fn cleanup_database(pool: &PgPool, db: &str) {
    let terminate = format!(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '{db}' AND pid <> pg_backend_pid()"
    );
    if let Err(err) = pool.execute(terminate.as_str()).await {
        eprintln!("终止连接失败: {err}");
    }
    let sql = format!("DROP DATABASE IF EXISTS \"{db}\" WITH (FORCE)");
    if let Err(err) = pool.execute(sql.as_str()).await {
        panic!("清理失败: 数据库 \"{db}\": {err}\nSQL: {sql}");
    }
}

async fn isolated_database(base_url: &str, prefix: &str) -> (String, PgPool) {
    let admin_pool = PgPool::connect(base_url)
        .await
        .expect("连接 PostgreSQL 管理库失败，请确认 phase3-pg 已就绪");
    let db = format!("{prefix}_{}", unique_suffix());
    admin_exec(
        &admin_pool,
        &format!("DROP DATABASE IF EXISTS \"{db}\" WITH (FORCE)"),
    )
    .await;
    admin_exec(&admin_pool, &format!("CREATE DATABASE \"{db}\"")).await;
    (db, admin_pool)
}

async fn exec(pool: &PgPool, sql: &str) {
    if let Err(err) = pool.execute(sql).await {
        panic!("执行 SQL 失败: {err}\nSQL: {sql}");
    }
}

async fn count(pool: &PgPool, sql: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(sql)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|err| panic!("查询失败: {err}\nSQL: {sql}"))
}

async fn make_ctx(url: &str, db_name: &str) -> OpsContext {
    let client = DbClient::new_default(url)
        .await
        .expect("创建 DbClient 失败");
    OpsContext::new(client, DB_ID.into(), db_name.to_string(), false)
        .with_policy(Arc::new(OpsPolicy::default()))
}

/// 直接构造校验规格：源列 v（source_data_type）→ 目标列 v（sql_type），bigint 主键。
fn spec_for(source: &str, target: &str, source_type: &str, target_sql_type: &str) -> BackfillSpec {
    BackfillSpec {
        source_table: source.into(),
        source_column: "v".into(),
        target_table: target.into(),
        pk_column: "id".into(),
        pk_columns: Vec::new(),
        pk_kind: Some(PkKind::Integer),
        pk_kinds: Vec::new(),
        target_columns: vec![TargetColumn {
            name: "v".into(),
            source_key: None,
            sql_type: target_sql_type.into(),
            nullable: true,
            required: false,
            note: None,
        }],
        empty_string_policy: EmptyStringPolicy::Preserve,
        create_table_sql: None,
        schema: Some("public".into()),
        source_data_type: source_type.into(),
    }
}

/// 建一对源/目标表并灌数据；`src_type`/`tgt_type` 是 PG 列类型。
async fn setup_pair(
    pool: &PgPool,
    source: &str,
    target: &str,
    src_type: &str,
    tgt_type: &str,
    src_rows: &str,
    tgt_rows: &str,
) {
    exec(
        pool,
        &format!("CREATE TABLE {source} (id bigint PRIMARY KEY, v {src_type})"),
    )
    .await;
    exec(
        pool,
        &format!("CREATE TABLE {target} (id bigint PRIMARY KEY, v {tgt_type})"),
    )
    .await;
    if !src_rows.is_empty() {
        exec(pool, &format!("INSERT INTO {source} VALUES {src_rows}")).await;
    }
    if !tgt_rows.is_empty() {
        exec(pool, &format!("INSERT INTO {target} VALUES {tgt_rows}")).await;
    }
}

macro_rules! pg_env {
    () => {
        match std::env::var("FIELD_MIGRATION_PG_URL") {
            Ok(url) => url,
            Err(_) => {
                eprintln!("跳过 S2 PG 校验真库验证：未设置 FIELD_MIGRATION_PG_URL");
                return;
            }
        }
    };
}

#[tokio::test]
async fn numeric_normalization() {
    let base_url = pg_env!();
    let (db_name, admin_pool) = isolated_database(&base_url, "it_s2pg_vnum").await;
    let url = with_database(&base_url, &db_name);
    let pool = PgPool::connect(&url).await.expect("连接 PG 失败");
    setup_pair(
        &pool,
        "v_src",
        "v_tgt",
        "numeric",
        "numeric",
        "(1, 1), (2, 1.0), (3, 1e3), (4, 'NaN'), (5, 1000)",
        "(1, 1.0), (2, 1), (3, 1000), (4, 'NaN'), (5, 999)",
    )
    .await;

    let ctx = make_ctx(&url, &db_name).await;
    let spec = spec_for("v_src", "v_tgt", "numeric", "numeric");
    let verify = pg_verify::verify_full(&ctx, &spec).await.expect("校验失败");
    println!("numeric: {verify:?}");
    assert_eq!(verify.source_rows, 5);
    assert_eq!(verify.target_rows, 5);
    assert_eq!(verify.v2, 0);
    assert_eq!(verify.v3, 0);
    // 1/1.0/1e3 按数值相等、NaN 自等；只有 1000 vs 999 判差异。
    assert_eq!(verify.v4, 1, "numeric 比较不得经过 f64，NaN 必须自等");
    assert_eq!(verify.validation_mode, "snapshot");

    pool.close().await;
    cleanup_database(&admin_pool, &db_name).await;
}

#[tokio::test]
async fn bytea_hex_compare() {
    let base_url = pg_env!();
    let (db_name, admin_pool) = isolated_database(&base_url, "it_s2pg_vbytea").await;
    let url = with_database(&base_url, &db_name);
    let pool = PgPool::connect(&url).await.expect("连接 PG 失败");
    setup_pair(
        &pool,
        "v_src",
        "v_tgt",
        "bytea",
        "bytea",
        "(1, '\\xdead'), (2, '\\xbeef'), (3, '\\x00ff')",
        "(1, '\\xdead'), (2, '\\xdeaf'), (3, '\\x00ff')",
    )
    .await;

    let ctx = make_ctx(&url, &db_name).await;
    let spec = spec_for("v_src", "v_tgt", "bytea", "bytea");
    let verify = pg_verify::verify_full(&ctx, &spec).await.expect("校验失败");
    println!("bytea: {verify:?}");
    assert_eq!(verify.v2, 0);
    assert_eq!(verify.v3, 0);
    assert_eq!(verify.v4, 1, "不同字节必须判差异，相同字节不判");

    pool.close().await;
    cleanup_database(&admin_pool, &db_name).await;
}

#[tokio::test]
async fn timestamptz_cross_timezone() {
    let base_url = pg_env!();
    let (db_name, admin_pool) = isolated_database(&base_url, "it_s2pg_vts").await;
    let url = with_database(&base_url, &db_name);
    let pool = PgPool::connect(&url).await.expect("连接 PG 失败");
    setup_pair(
        &pool,
        "v_src",
        "v_tgt",
        "timestamptz",
        "timestamptz",
        "(1, '2024-01-01 00:00:00+08'), (2, '2024-01-01 00:00:00+08'), (3, '2024-06-01 12:00:00+00')",
        "(1, '2023-12-31 16:00:00+00'), (2, '2024-01-01 01:00:00+00'), (3, '2024-06-01 12:00:00+00')",
    )
    .await;

    let ctx = make_ctx(&url, &db_name).await;
    let spec = spec_for("v_src", "v_tgt", "timestamptz", "timestamp with time zone");
    let verify = pg_verify::verify_full(&ctx, &spec).await.expect("校验失败");
    println!("timestamptz: {verify:?}");
    assert_eq!(verify.v2, 0);
    assert_eq!(verify.v3, 0);
    // id=1 是同一时刻的不同时区写法（无差异）；id=2 差一小时（差异）。
    assert_eq!(verify.v4, 1, "同一时刻不同时区必须判等，不同时刻必须判差异");

    pool.close().await;
    cleanup_database(&admin_pool, &db_name).await;
}

#[tokio::test]
async fn json_vs_jsonb() {
    let base_url = pg_env!();
    let (db_name, admin_pool) = isolated_database(&base_url, "it_s2pg_vjson").await;
    let url = with_database(&base_url, &db_name);
    let pool = PgPool::connect(&url).await.expect("连接 PG 失败");
    setup_pair(
        &pool,
        "v_src",
        "v_tgt",
        "json",
        "jsonb",
        r#"(1, '{"a":1,"b":2}'), (2, '{"x":1.0}'), (3, '{"k":"v"}'), (4, '{"arr":[1,2,3]}')"#,
        r#"(1, '{"b":2,"a":1}'), (2, '{"x":1}'), (3, '{"k":"w"}'), (4, '{"arr":[1,2,3]}')"#,
    )
    .await;

    let ctx = make_ctx(&url, &db_name).await;
    let spec = spec_for("v_src", "v_tgt", "json", "jsonb");
    let verify = pg_verify::verify_full(&ctx, &spec).await.expect("校验失败");
    println!("json/jsonb: {verify:?}");
    assert_eq!(verify.v2, 0);
    assert_eq!(verify.v3, 0);
    // 键序与数字格式差异被 jsonb 归一化；只有 id=3 是真差异。
    assert_eq!(verify.v4, 1, "json 必须先 ::jsonb 归一化键序与数字表示");

    pool.close().await;
    cleanup_database(&admin_pool, &db_name).await;
}

#[tokio::test]
async fn null_vs_empty_string() {
    let base_url = pg_env!();
    let (db_name, admin_pool) = isolated_database(&base_url, "it_s2pg_vnull").await;
    let url = with_database(&base_url, &db_name);
    let pool = PgPool::connect(&url).await.expect("连接 PG 失败");
    setup_pair(
        &pool,
        "v_src",
        "v_tgt",
        "text",
        "text",
        "(1, NULL), (2, ''), (3, NULL), (4, 'x')",
        "(1, ''), (2, ''), (3, NULL), (4, 'x')",
    )
    .await;

    let ctx = make_ctx(&url, &db_name).await;
    let spec = spec_for("v_src", "v_tgt", "text", "text");
    let verify = pg_verify::verify_full(&ctx, &spec).await.expect("校验失败");
    println!("null/empty: {verify:?}");
    assert_eq!(verify.v2, 0);
    assert_eq!(verify.v3, 0);
    assert_eq!(verify.v4, 1, "NULL 与 '' 不得合并；NULL 与 NULL 不算差异");

    pool.close().await;
    cleanup_database(&admin_pool, &db_name).await;
}

#[tokio::test]
async fn uuid_compare() {
    let base_url = pg_env!();
    let (db_name, admin_pool) = isolated_database(&base_url, "it_s2pg_vuuid").await;
    let url = with_database(&base_url, &db_name);
    let pool = PgPool::connect(&url).await.expect("连接 PG 失败");
    // 源是 uuid 文本，目标是 uuid；id=3 源为大写形式，规范化后应判等。
    setup_pair(
        &pool,
        "v_src",
        "v_tgt",
        "text",
        "uuid",
        "(1, '7d3f2b10-4c5e-4a1b-9c8d-2e6f0a1b2c3d'), \
         (2, '8e4f3c21-5d6f-4b2c-ad9e-3f7a1b2c3d4e'), \
         (3, 'A1B2C3D4-E5F6-4A7B-8C9D-0E1F2A3B4C5D')",
        "(1, '7d3f2b10-4c5e-4a1b-9c8d-2e6f0a1b2c3d'), \
         (2, '9f5a4d32-6e7a-4c8d-beaf-4a8b2c3d4e5f'), \
         (3, 'a1b2c3d4-e5f6-4a7b-8c9d-0e1f2a3b4c5d')",
    )
    .await;

    let ctx = make_ctx(&url, &db_name).await;
    let spec = spec_for("v_src", "v_tgt", "text", "uuid");
    let verify = pg_verify::verify_full(&ctx, &spec).await.expect("校验失败");
    println!("uuid: {verify:?}");
    assert_eq!(verify.v2, 0);
    assert_eq!(verify.v3, 0);
    assert_eq!(verify.v4, 1, "大小写不同的同一 uuid 判等，不同 uuid 判差异");

    pool.close().await;
    cleanup_database(&admin_pool, &db_name).await;
}

#[tokio::test]
async fn empty_tables() {
    let base_url = pg_env!();
    let (db_name, admin_pool) = isolated_database(&base_url, "it_s2pg_vempty").await;
    let url = with_database(&base_url, &db_name);
    let pool = PgPool::connect(&url).await.expect("连接 PG 失败");
    setup_pair(&pool, "v_src", "v_tgt", "text", "text", "", "").await;

    let ctx = make_ctx(&url, &db_name).await;
    let spec = spec_for("v_src", "v_tgt", "text", "text");
    let verify = pg_verify::verify_full(&ctx, &spec).await.expect("校验失败");
    println!("empty: {verify:?}");
    assert_eq!(verify.source_rows, 0);
    assert_eq!(verify.target_rows, 0);
    assert_eq!([verify.v2, verify.v3, verify.v4], [0, 0, 0]);
    assert!(verify.passed(), "空表必须通过校验");
    assert_eq!(verify.validation_mode, "snapshot");

    pool.close().await;
    cleanup_database(&admin_pool, &db_name).await;
}

#[tokio::test]
async fn snapshot_consistency() {
    let base_url = pg_env!();
    let (db_name, admin_pool) = isolated_database(&base_url, "it_s2pg_vsnap").await;
    let url = with_database(&base_url, &db_name);
    let pool = PgPool::connect(&url).await.expect("连接 PG 失败");
    setup_pair(
        &pool,
        "v_src",
        "v_tgt",
        "text",
        "text",
        "(1, 'a'), (2, 'b')",
        "(1, 'a'), (2, 'b')",
    )
    .await;

    // 只读快照内两次计数一致；快照期间外部写入不可见。
    let mut snapshot = PgSnapshot::begin(&pool).await.expect("开启快照失败");
    let (isolation, read_only) = snapshot.settings().await.expect("读取快照设置失败");
    println!("快照设置: isolation={isolation} read_only={read_only}");
    assert_eq!(isolation, "repeatable read");
    assert!(read_only, "校验快照必须是 READ ONLY");
    let before = snapshot
        .count("SELECT COUNT(*) AS count FROM v_src", &[])
        .await
        .expect("快照内计数失败");
    exec(
        &pool,
        "INSERT INTO v_src VALUES (10, 'x'), (11, 'y'), (12, 'z')",
    )
    .await;
    let after = snapshot
        .count("SELECT COUNT(*) AS count FROM v_src", &[])
        .await
        .expect("快照内计数失败");
    assert_eq!(before, after, "REPEATABLE READ 快照内两次计数必须一致");
    snapshot.commit().await.expect("提交快照失败");
    let external = count(&pool, "SELECT COUNT(*) FROM v_src").await;
    assert_eq!(external, before as i64 + 3, "提交后必须看到外部写入");

    // verify_range 内部走快照并回写 validation_mode。
    let ctx = make_ctx(&url, &db_name).await;
    let spec = spec_for("v_src", "v_tgt", "text", "text");
    let verify = pg_verify::verify_range(&ctx, &spec, None, Some(&["2".to_string()]))
        .await
        .expect("范围校验失败");
    assert_eq!(verify.validation_mode, "snapshot");
    assert!(verify.passed(), "范围 (.., 2] 内两侧一致");

    pool.close().await;
    cleanup_database(&admin_pool, &db_name).await;
}

#[tokio::test]
async fn diff_pages() {
    let base_url = pg_env!();
    let (db_name, admin_pool) = isolated_database(&base_url, "it_s2pg_vdiff").await;
    let url = with_database(&base_url, &db_name);
    let pool = PgPool::connect(&url).await.expect("连接 PG 失败");
    // 源 1..=10；目标 4..=13（id 5/6/7 值不一致）。
    let src_rows = (1..=10)
        .map(|i| format!("({i}, 'x')"))
        .collect::<Vec<_>>()
        .join(", ");
    let tgt_rows = (4..=13)
        .map(|i| {
            let value = if (5..=7).contains(&i) { "y" } else { "x" };
            format!("({i}, '{value}')")
        })
        .collect::<Vec<_>>()
        .join(", ");
    setup_pair(
        &pool, "v_src", "v_tgt", "text", "text", &src_rows, &tgt_rows,
    )
    .await;

    let ctx = make_ctx(&url, &db_name).await;
    let spec = spec_for("v_src", "v_tgt", "text", "text");
    let verify = pg_verify::verify_full(&ctx, &spec).await.expect("校验失败");
    println!("diff: {verify:?}");
    assert_eq!(verify.v2, 3, "源有目标无: 1,2,3");
    assert_eq!(verify.v3, 3, "目标有源无: 11,12,13");
    assert_eq!(verify.v4, 3, "值不一致: 5,6,7");

    for (phase, expected) in [
        ("v2", vec!["1", "2", "3"]),
        ("v3", vec!["11", "12", "13"]),
        ("v4", vec!["5", "6", "7"]),
    ] {
        let total = pg_verify::diff_total(&ctx, &spec, phase, None, None)
            .await
            .expect("差异计数失败");
        assert_eq!(total, 3, "{phase} 总数不符");

        // 以 page=2 走 keyset 分页：总数正确、无重复、顺序递增。
        // S5 起 keyset 边界统一为键向量（单列即长度为 1 的向量），断言语义不变。
        let mut collected: Vec<String> = Vec::new();
        let mut last: Option<Vec<String>> = None;
        loop {
            let page = pg_verify::diff_page(&ctx, &spec, phase, last.as_deref(), None, 2)
                .await
                .expect("差异分页失败");
            if page.is_empty() {
                break;
            }
            if let Some(previous) = &last {
                assert!(
                    page.first().unwrap() > &previous[0],
                    "{phase} 分页必须严格递增: {page:?} after {previous:?}"
                );
            }
            last = page.last().map(|value| vec![value.clone()]);
            collected.extend(page);
        }
        let mut dedup = collected.clone();
        dedup.dedup();
        assert_eq!(dedup.len(), collected.len(), "{phase} 分页不允许重复");
        assert_eq!(collected, expected, "{phase} 差异主键不符");
    }

    pool.close().await;
    cleanup_database(&admin_pool, &db_name).await;
}
