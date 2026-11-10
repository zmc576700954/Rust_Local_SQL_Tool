//! PostgreSQL 数据同步真实库集成测试（opt-in）。
//!
//! 运行方式：
//! ```bash
//! DATA_SYNC_PG_URL='postgres://postgres:password@127.0.0.1:55432/e2e' \
//!   cargo test -p web-server --bin web-server data_sync_integration_pg -- --nocapture
//! ```
//!
//! 覆盖：PG 特有类型（数组/enum/jsonb/numeric(p,s)/timestamptz/bytea/bool/生成列/
//! identity）、复合主键、外键拓扑与环、序列对齐（含空表）、分批续跑、重复执行幂等、
//! mirror 显式确认与删除、单事务回滚、Plan 阶段拒绝路径。

use core_lib::db::DbClient;
use core_lib::error::AppError;
use core_lib::sync::{PgDataSyncEngine, PgDataSyncOptions, PgDataSyncReport, SyncMode};
use sqlx::{Executor, PgPool};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

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

fn unique_db(prefix: &str) -> String {
    format!("{prefix}_{}", unique_suffix())
}

fn checkpoint_dir(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("pg-data-sync-{tag}-{}", unique_suffix()))
}

fn pg_url() -> Option<String> {
    match std::env::var("DATA_SYNC_PG_URL") {
        Ok(url) if !url.trim().is_empty() => Some(url),
        _ => {
            eprintln!("跳过 PostgreSQL 数据同步集成测试：未设置 DATA_SYNC_PG_URL");
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

async fn create_database(pool: &PgPool, db: &str) {
    exec(pool, &format!("DROP DATABASE IF EXISTS \"{db}\" WITH (FORCE)")).await;
    exec(pool, &format!("CREATE DATABASE \"{db}\"")).await;
}

async fn cleanup_database(pool: &PgPool, db: &str) {
    let sql = format!("DROP DATABASE IF EXISTS \"{db}\" WITH (FORCE)");
    if let Err(err) = Executor::execute(pool, sqlx::raw_sql(&sql)).await {
        panic!("清理失败: 数据库 \"{db}\": {err}\nSQL: {sql}");
    }
}

struct Fixture {
    admin_url: String,
    source_db: String,
    target_db: String,
    source_client: DbClient,
    target_client: DbClient,
    source_pool: PgPool,
    target_pool: PgPool,
}

async fn fixture(tag: &str) -> Option<Fixture> {
    let url = pg_url()?;
    let source_db = unique_db(&format!("it_dsync_src_{tag}"));
    let target_db = unique_db(&format!("it_dsync_tgt_{tag}"));
    let admin_pool = PgPool::connect(&url)
        .await
        .expect("连接 PostgreSQL 失败，请确认 compose 服务已就绪");
    create_database(&admin_pool, &source_db).await;
    create_database(&admin_pool, &target_db).await;
    let source_url = with_database(&url, &source_db);
    let target_url = with_database(&url, &target_db);
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
    Some(Fixture {
        admin_url: url,
        source_db,
        target_db,
        source_client,
        target_client,
        source_pool,
        target_pool,
    })
}

async fn drop_fixture(f: &Fixture) {
    let admin_pool = PgPool::connect(&f.admin_url)
        .await
        .expect("连接管理库失败");
    cleanup_database(&admin_pool, &f.source_db).await;
    cleanup_database(&admin_pool, &f.target_db).await;
}

fn table_report<'a>(report: &'a PgDataSyncReport, name: &str) -> &'a core_lib::sync::PgTableReport {
    report
        .tables
        .iter()
        .find(|t| t.table_name == name)
        .unwrap_or_else(|| panic!("报告缺少表 {name}: {report:?}"))
}

fn options(batch_size: usize, dir: PathBuf) -> PgDataSyncOptions {
    PgDataSyncOptions {
        batch_size,
        checkpoint_dir: dir,
        job_label: "integration".to_string(),
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_data_sync_end_to_end_types_sequence_and_idempotent() {
    let Some(f) = fixture("types").await else {
        return;
    };
    exec(
        &f.source_pool,
        r#"
        CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy');
        CREATE TABLE users (
            id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
            name text NOT NULL,
            tags text[],
            moods mood[],
            mood mood,
            profile jsonb,
            balance numeric(12,2),
            created_at timestamptz,
            raw bytea,
            active boolean,
            name_lower text GENERATED ALWAYS AS (lower(name)) STORED
        );
        CREATE TABLE orders (
            user_id bigint NOT NULL REFERENCES users(id),
            order_no integer NOT NULL,
            amount numeric(12,2) NOT NULL,
            notes text,
            PRIMARY KEY (user_id, order_no)
        );
        CREATE INDEX orders_amount_idx ON orders (amount);
        CREATE TABLE empty_seq (
            id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
            note text
        );
        INSERT INTO users (name, tags, moods, mood, profile, balance, created_at, raw, active)
        VALUES
            ('Alice', ARRAY['a','b'], ARRAY['happy','ok']::mood[], 'happy',
             '{"x":1,"y":[true,null]}'::jsonb, 1234567890.12,
             '2024-01-02 03:04:05+08', '\x00ff10'::bytea, true),
            ('Bob', ARRAY[]::text[], ARRAY[]::mood[], 'sad',
             '{"k":"v"}'::jsonb, 0.01,
             '2023-12-31 23:59:59.123456-05', '\xdeadbeef'::bytea, false);
        INSERT INTO orders VALUES (1, 10, 9.99, 'first'), (2, 20, 100.00, NULL);
        "#,
    )
    .await;

    let dir = checkpoint_dir("types");
    let report = PgDataSyncEngine::run(
        f.source_client.clone(),
        f.target_client.clone(),
        options(1, dir.clone()),
        Box::new(|_| {}),
    )
    .await
    .expect("数据同步应成功");
    assert!(report.error.is_none(), "同步报错: {report:?}");
    assert_eq!(report.verification_status, "verified", "{report:?}");
    assert!(report.resumable, "有主键且逐批提交应可续跑");
    assert_eq!(report.rollback_scope, "batch");
    assert!(!report.atomic);

    let users = table_report(&report, "public.users");
    assert_eq!(users.status, "completed");
    assert_eq!(users.source_rows, Some(2));
    assert_eq!(users.target_rows, Some(2));
    assert_eq!(users.checksum_equal, Some(true));
    assert!(users.sequences_aligned.contains(&"id".to_string()));

    let orders = table_report(&report, "public.orders");
    assert_eq!(orders.source_rows, Some(2));
    assert_eq!(orders.target_rows, Some(2));
    assert_eq!(orders.checksum_equal, Some(true));
    assert_eq!(orders.pk_columns, vec!["user_id", "order_no"]);

    // 类型逐项核对
    let tags: Vec<String> =
        sqlx::query_scalar("SELECT tags FROM users WHERE name = 'Alice'")
            .fetch_one(&f.target_pool)
            .await
            .expect("读取 text[]");
    assert_eq!(tags, vec!["a".to_string(), "b".to_string()]);

    let moods: Vec<String> =
        sqlx::query_scalar("SELECT moods::text[] FROM users WHERE name = 'Alice'")
            .fetch_one(&f.target_pool)
            .await
            .expect("读取 enum[]");
    assert_eq!(moods, vec!["happy".to_string(), "ok".to_string()]);

    let mood: String = sqlx::query_scalar("SELECT mood::text FROM users WHERE name = 'Alice'")
        .fetch_one(&f.target_pool)
        .await
        .expect("读取 enum");
    assert_eq!(mood, "happy");

    let profile: String =
        sqlx::query_scalar("SELECT profile::text FROM users WHERE name = 'Alice'")
            .fetch_one(&f.target_pool)
            .await
            .expect("读取 jsonb");
    assert_eq!(profile, "{\"x\": 1, \"y\": [true, null]}");

    let balance: String =
        sqlx::query_scalar("SELECT balance::text FROM users WHERE name = 'Alice'")
            .fetch_one(&f.target_pool)
            .await
            .expect("读取 numeric");
    assert_eq!(balance, "1234567890.12");

    let ts_equal: bool = sqlx::query_scalar(
        "SELECT created_at = '2024-01-02 03:04:05+08'::timestamptz \
         FROM users WHERE name = 'Alice'",
    )
    .fetch_one(&f.target_pool)
    .await
    .expect("读取 timestamptz");
    assert!(ts_equal, "timestamptz 必须保持同一时刻");

    let raw: Vec<u8> =
        sqlx::query_scalar("SELECT raw FROM users WHERE name = 'Alice'")
            .fetch_one(&f.target_pool)
            .await
            .expect("读取 bytea");
    assert_eq!(raw, vec![0x00, 0xff, 0x10]);

    let active: bool =
        sqlx::query_scalar("SELECT active FROM users WHERE name = 'Alice'")
            .fetch_one(&f.target_pool)
            .await
            .expect("读取 bool");
    assert!(active);

    let lower: String =
        sqlx::query_scalar("SELECT name_lower FROM users WHERE name = 'Alice'")
            .fetch_one(&f.target_pool)
            .await
            .expect("读取生成列");
    assert_eq!(lower, "alice", "生成列应由目标库自行计算");

    // 索引与外键在数据之后创建
    let index_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM pg_indexes \
         WHERE schemaname = 'public' AND tablename = 'orders' AND indexname = 'orders_amount_idx')",
    )
    .fetch_one(&f.target_pool)
    .await
    .expect("查询索引");
    assert!(index_exists, "非主键索引应被创建");
    let fk_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM pg_constraint con \
         JOIN pg_class c ON c.oid = con.conrelid \
         WHERE c.relname = 'orders' AND con.contype = 'f')",
    )
    .fetch_one(&f.target_pool)
    .await
    .expect("查询外键");
    assert!(fk_exists, "外键应在数据后创建");

    // 序列对齐：identity ALWAYS 列直接插入新行不撞主键
    let new_id: i64 = sqlx::query_scalar("INSERT INTO users (name) VALUES ('Carol') RETURNING id")
        .fetch_one(&f.target_pool)
        .await
        .expect("序列未对齐导致插入失败");
    assert_eq!(new_id, 3);

    // 空表也要 setval(...,1,false)，插入从 1 开始
    let empty_id: i64 =
        sqlx::query_scalar("INSERT INTO empty_seq (note) VALUES ('x') RETURNING id")
            .fetch_one(&f.target_pool)
            .await
            .expect("空表序列未对齐导致插入失败");
    assert_eq!(empty_id, 1);

    // 重复执行幂等：先清掉上面探测插入的行，再跑一次
    exec(
        &f.target_pool,
        "DELETE FROM users WHERE name = 'Carol'; DELETE FROM empty_seq;",
    )
    .await;
    let again = PgDataSyncEngine::run(
        f.source_client.clone(),
        f.target_client.clone(),
        options(1, dir.clone()),
        Box::new(|_| {}),
    )
    .await
    .expect("重复执行应成功");
    assert!(again.error.is_none(), "{again:?}");
    assert_eq!(again.verification_status, "verified", "{again:?}");
    let user_count: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
        .fetch_one(&f.target_pool)
        .await
        .unwrap();
    assert_eq!(user_count, 2, "重复执行不应产生重复行");
    let order_count: i64 = sqlx::query_scalar("SELECT count(*) FROM orders")
        .fetch_one(&f.target_pool)
        .await
        .unwrap();
    assert_eq!(order_count, 2);

    let _ = std::fs::remove_dir_all(&dir);
    drop_fixture(&f).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_data_sync_resume_after_interruption() {
    let Some(f) = fixture("resume").await else {
        return;
    };
    exec(
        &f.source_pool,
        r#"
        CREATE TABLE resume_users (id integer PRIMARY KEY, v text NOT NULL);
        INSERT INTO resume_users VALUES (1,'a'),(2,'b'),(3,'c'),(4,'d'),(5,'e');
        "#,
    )
    .await;
    exec(
        &f.target_pool,
        r#"
        CREATE TABLE resume_users (id integer PRIMARY KEY, v text NOT NULL);
        CREATE FUNCTION resume_boom() RETURNS trigger AS $$
        BEGIN
            IF (SELECT count(*) FROM resume_users) >= 3 THEN
                RAISE EXCEPTION 'interrupt after 3 rows';
            END IF;
            RETURN NEW;
        END $$ LANGUAGE plpgsql;
        CREATE TRIGGER resume_boom BEFORE INSERT ON resume_users
        FOR EACH ROW EXECUTE FUNCTION resume_boom();
        "#,
    )
    .await;

    let dir = checkpoint_dir("resume");
    let mut opts = options(1, dir.clone());
    opts.tables = Some(vec!["resume_users".to_string()]);

    let first = PgDataSyncEngine::run(f.source_client.clone(), f.target_client.clone(), opts.clone(), Box::new(|_| {}))
        .await
        .expect("引擎应返回失败报告而不是 panic");
    assert!(
        first.error.as_deref().unwrap_or_default().contains("interrupt after 3 rows"),
        "{first:?}"
    );
    let t = table_report(&first, "public.resume_users");
    assert_eq!(t.status, "failed");
    assert_eq!(t.rows_copied, 3, "失败前应已提交 3 个批次");
    assert_eq!(first.verification_status, "unverified");
    let committed: i64 = sqlx::query_scalar("SELECT count(*) FROM resume_users")
        .fetch_one(&f.target_pool)
        .await
        .unwrap();
    assert_eq!(committed, 3);

    exec(
        &f.target_pool,
        "DROP TRIGGER resume_boom ON resume_users; DROP FUNCTION resume_boom();",
    )
    .await;

    opts.resume = true;
    let resumed = PgDataSyncEngine::run(f.source_client.clone(), f.target_client.clone(), opts.clone(), Box::new(|_| {}))
        .await
        .expect("续跑应成功");
    assert!(resumed.error.is_none(), "{resumed:?}");
    assert_eq!(resumed.verification_status, "verified", "{resumed:?}");
    let t = table_report(&resumed, "public.resume_users");
    assert_eq!(t.rows_copied, 5, "续跑应累计到 5 行");
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM resume_users")
        .fetch_one(&f.target_pool)
        .await
        .unwrap();
    let distinct: i64 = sqlx::query_scalar("SELECT count(DISTINCT id) FROM resume_users")
        .fetch_one(&f.target_pool)
        .await
        .unwrap();
    assert_eq!(total, 5);
    assert_eq!(distinct, 5, "续跑不应产生重复主键");

    // 没有断点时的 resume 必须显式报错，不能悄悄从头再跑
    let mut missing = options(1, checkpoint_dir("resume-missing"));
    missing.tables = Some(vec!["resume_users".to_string()]);
    missing.resume = true;
    let err = PgDataSyncEngine::run(f.source_client.clone(), f.target_client.clone(), missing, Box::new(|_| {}))
        .await
        .expect_err("缺少断点时必须拒绝续跑");
    assert!(matches!(err, AppError::BadRequest(_)), "{err:?}");
    assert!(err.to_string().contains("未找到可续跑的断点"), "{err}");

    let _ = std::fs::remove_dir_all(&dir);
    drop_fixture(&f).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_data_sync_mirror_requires_confirmation_and_deletes() {
    let Some(f) = fixture("mirror").await else {
        return;
    };
    exec(
        &f.source_pool,
        r#"
        CREATE TABLE mirror_items (id integer PRIMARY KEY, v text NOT NULL);
        INSERT INTO mirror_items VALUES (1,'one'),(2,'two'),(3,'three');
        "#,
    )
    .await;
    exec(
        &f.target_pool,
        r#"
        CREATE TABLE mirror_items (id integer PRIMARY KEY, v text NOT NULL);
        INSERT INTO mirror_items VALUES (1,'old'),(3,'three'),(4,'extra');
        "#,
    )
    .await;
    let dir = checkpoint_dir("mirror");
    let mut opts = options(2, dir.clone());
    opts.tables = Some(vec!["mirror_items".to_string()]);
    opts.mode = SyncMode::Mirror;

    let err = PgDataSyncEngine::run(f.source_client.clone(), f.target_client.clone(), opts.clone(), Box::new(|_| {}))
        .await
        .expect_err("mirror 未确认时必须拒绝");
    assert!(err.to_string().contains("mirror"), "{err}");

    opts.mirror_confirmed = true;
    let report = PgDataSyncEngine::run(f.source_client.clone(), f.target_client.clone(), opts, Box::new(|_| {}))
        .await
        .expect("mirror 同步应成功");
    assert!(report.error.is_none(), "{report:?}");
    assert_eq!(report.verification_status, "verified", "{report:?}");

    let rows: Vec<(i32, String)> =
        sqlx::query_as("SELECT id, v FROM mirror_items ORDER BY id")
            .fetch_all(&f.target_pool)
            .await
            .expect("读取 mirror 结果");
    assert_eq!(
        rows,
        vec![
            (1, "one".to_string()),
            (2, "two".to_string()),
            (3, "three".to_string())
        ],
        "mirror 应删除目标多出的 id=4 并更新 id=1"
    );

    let _ = std::fs::remove_dir_all(&dir);
    drop_fixture(&f).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_data_sync_plan_rejections_and_single_transaction() {
    let Some(f) = fixture("reject").await else {
        return;
    };
    exec(
        &f.source_pool,
        r#"
        CREATE TABLE no_pk_table (a integer, b text);
        INSERT INTO no_pk_table VALUES (1,'x');
        CREATE TYPE pair AS (x integer, y integer);
        CREATE TABLE comp_col (id integer PRIMARY KEY, p pair);
        INSERT INTO comp_col VALUES (1, ROW(1,2));
        CREATE TABLE cyc_a (id integer PRIMARY KEY, b_id integer);
        CREATE TABLE cyc_b (id integer PRIMARY KEY, a_id integer);
        INSERT INTO cyc_a VALUES (1, 1);
        INSERT INTO cyc_b VALUES (1, 1);
        ALTER TABLE cyc_a ADD CONSTRAINT cyc_a_b_fk FOREIGN KEY (b_id) REFERENCES cyc_b(id);
        ALTER TABLE cyc_b ADD CONSTRAINT cyc_b_a_fk FOREIGN KEY (a_id) REFERENCES cyc_a(id);
        CREATE TABLE atomic_items (id integer PRIMARY KEY, v integer);
        INSERT INTO atomic_items VALUES (1,1),(2,2),(3,50);
        "#,
    )
    .await;
    exec(
        &f.target_pool,
        r#"
        CREATE TABLE atomic_items (
            id integer PRIMARY KEY,
            v integer CHECK (v < 10)
        );
        "#,
    )
    .await;

    // 1) 无主键默认拒绝
    let mut opts = options(10, checkpoint_dir("reject-nopk"));
    opts.tables = Some(vec!["no_pk_table".to_string()]);
    let err = PgDataSyncEngine::run(f.source_client.clone(), f.target_client.clone(), opts, Box::new(|_| {}))
        .await
        .expect_err("无主键必须拒绝");
    assert!(err.to_string().contains("没有主键"), "{err}");

    // 2) 复合类型 Plan 阶段拒绝
    let mut opts = options(10, checkpoint_dir("reject-comp"));
    opts.tables = Some(vec!["comp_col".to_string()]);
    let err = PgDataSyncEngine::run(f.source_client.clone(), f.target_client.clone(), opts, Box::new(|_| {}))
        .await
        .expect_err("复合类型必须拒绝");
    assert!(err.to_string().contains("复合/伪类型"), "{err}");

    // 3) 外键环默认拒绝并列出环上的表
    let mut opts = options(10, checkpoint_dir("reject-cycle"));
    opts.tables = Some(vec!["cyc_a".to_string(), "cyc_b".to_string()]);
    let err = PgDataSyncEngine::run(f.source_client.clone(), f.target_client.clone(), opts, Box::new(|_| {}))
        .await
        .expect_err("外键环默认必须拒绝");
    assert!(err.to_string().contains("外键循环依赖"), "{err}");
    assert!(err.to_string().contains("cyc_a"), "{err}");
    assert!(err.to_string().contains("cyc_b"), "{err}");

    // 4) defer_constraints 显式开启后可同步环（当前测试用户为超级用户）
    let mut opts = options(10, checkpoint_dir("defer-cycle"));
    opts.tables = Some(vec!["cyc_a".to_string(), "cyc_b".to_string()]);
    opts.defer_constraints = true;
    let report = PgDataSyncEngine::run(f.source_client.clone(), f.target_client.clone(), opts, Box::new(|_| {}))
        .await
        .expect("defer 模式应成功");
    assert!(report.error.is_none(), "{report:?}");
    assert!(report.deferred_constraints);
    assert_eq!(report.verification_status, "verified", "{report:?}");
    let cyc_count: i64 = sqlx::query_scalar(
        "SELECT (SELECT count(*) FROM cyc_a) + (SELECT count(*) FROM cyc_b)",
    )
    .fetch_one(&f.target_pool)
    .await
    .unwrap();
    assert_eq!(cyc_count, 2);

    // 5) create_missing_tables=false 且目标缺表 -> 拒绝
    let mut opts = options(10, checkpoint_dir("no-create"));
    opts.tables = Some(vec!["orders_missing".to_string()]);
    exec(
        &f.source_pool,
        "CREATE TABLE orders_missing (id integer PRIMARY KEY);",
    )
    .await;
    opts.create_missing_tables = false;
    let err = PgDataSyncEngine::run(f.source_client.clone(), f.target_client.clone(), opts, Box::new(|_| {}))
        .await
        .expect_err("目标缺表且禁止建表必须拒绝");
    assert!(err.to_string().contains("create_missing_tables=false"), "{err}");

    // 6) single_transaction 与 resume 互斥
    let mut opts = options(10, checkpoint_dir("mutex"));
    opts.single_transaction = true;
    opts.resume = true;
    let err = PgDataSyncEngine::run(f.source_client.clone(), f.target_client.clone(), opts, Box::new(|_| {}))
        .await
        .expect_err("单事务与续跑互斥");
    assert!(err.to_string().contains("互斥"), "{err}");

    // 7) 单事务失败 -> 整作业回滚
    let dir = checkpoint_dir("single-tx");
    let mut opts = options(1, dir.clone());
    opts.tables = Some(vec!["atomic_items".to_string()]);
    opts.single_transaction = true;
    let failed = PgDataSyncEngine::run(f.source_client.clone(), f.target_client.clone(), opts.clone(), Box::new(|_| {}))
        .await
        .expect("引擎应返回失败报告");
    assert!(failed.rolled_back, "单事务失败必须回滚: {failed:?}");
    assert_eq!(failed.rollback_scope, "job");
    assert!(failed.atomic);
    assert!(!failed.resumable, "单事务作业不可续跑");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM atomic_items")
        .fetch_one(&f.target_pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "回滚后目标表应为空");

    // 8) 去掉 CHECK 后单事务成功
    exec(
        &f.target_pool,
        "ALTER TABLE atomic_items DROP CONSTRAINT atomic_items_v_check;",
    )
    .await;
    let ok = PgDataSyncEngine::run(f.source_client.clone(), f.target_client.clone(), opts, Box::new(|_| {}))
        .await
        .expect("单事务同步应成功");
    assert!(ok.error.is_none(), "{ok:?}");
    assert!(ok.atomic);
    assert!(!ok.rolled_back);
    assert_eq!(ok.rollback_scope, "job");
    assert_eq!(ok.verification_status, "verified", "{ok:?}");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM atomic_items")
        .fetch_one(&f.target_pool)
        .await
        .unwrap();
    assert_eq!(count, 3);

    let _ = std::fs::remove_dir_all(&dir);
    drop_fixture(&f).await;
}
