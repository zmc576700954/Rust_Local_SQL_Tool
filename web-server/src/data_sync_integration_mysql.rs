//! MySQL / MariaDB 分阶段数据同步真库集成测试。
//!
//! 运行方式：
//! ```bash
//! DATA_SYNC_MYSQL_URL='mysql://root:password@127.0.0.1:33306/e2e' \
//! DATA_SYNC_MARIADB_URL='mysql://root:password@127.0.0.1:33307/e2e' \
//!   cargo test -p web-server --bin web-server data_sync_integration_mysql -- --test-threads=1 --nocapture
//! ```
//!
//! 覆盖：类型往返（decimal/datetime/timestamp/blob/varbinary/json/enum/set/bit）、
//! AUTO_INCREMENT 对齐（含空表）、幂等、断点续跑、mirror 确认、无主键拒绝、
//! 外键环与 defer_constraints、DDL 隐式提交的可靠性字段、跨 MySQL/MariaDB 方言。

use core_lib::config::{DbType, PoolConfig};
use core_lib::db::DbClient;
use core_lib::sync::{
    MySqlStagedDataSyncEngine, PgDataSyncOptions, PgDataSyncReport, SyncMode,
};
use sqlx::mysql::MySqlPoolOptions;
use sqlx::{Executor, MySqlPool};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const MYSQL_ENV: &str = "DATA_SYNC_MYSQL_URL";
const MARIADB_ENV: &str = "DATA_SYNC_MARIADB_URL";

#[derive(Clone, Copy)]
enum Flavor {
    Mysql,
    MariaDb,
}

impl Flavor {
    fn env(self) -> &'static str {
        match self {
            Flavor::Mysql => MYSQL_ENV,
            Flavor::MariaDb => MARIADB_ENV,
        }
    }

    fn db_type(self) -> DbType {
        match self {
            Flavor::Mysql => DbType::MySQL,
            Flavor::MariaDb => DbType::MariaDB,
        }
    }

    fn prefix(self) -> &'static str {
        match self {
            Flavor::Mysql => "my",
            Flavor::MariaDb => "ma",
        }
    }

    fn url(self) -> Option<String> {
        std::env::var(self.env()).ok().filter(|v| !v.trim().is_empty())
    }
}

struct Server {
    admin: MySqlPool,
    base_url: String,
}

impl Server {
    async fn open(flavor: Flavor) -> Option<Self> {
        let url = flavor.url()?;
        let admin = MySqlPoolOptions::new()
            .max_connections(4)
            .connect(&url)
            .await
            .unwrap_or_else(|e| panic!("连接 {} 失败（{}）：{e}", flavor.env(), url));
        Some(Self {
            admin,
            base_url: url,
        })
    }

    async fn create_db(&self, prefix: &str) -> String {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = format!("e2e_ds_{prefix}_{millis}_{}_{seq}", std::process::id());
        exec_pool(
            &self.admin,
            &format!(
                "CREATE DATABASE `{name}` DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_general_ci"
            ),
        )
        .await;
        name
    }

    async fn client(&self, flavor: Flavor, db: &str) -> DbClient {
        let url = replace_db(&self.base_url, db);
        DbClient::new(&url, &PoolConfig::default(), &flavor.db_type())
            .await
            .unwrap_or_else(|e| panic!("连接测试库 {db} 失败：{e}"))
    }

    async fn drop_db(&self, db: &str) {
        exec_pool(&self.admin, &format!("DROP DATABASE IF EXISTS `{db}`")).await;
    }
}

struct Pair {
    src_server: Server,
    tgt_server: Server,
    src_db: String,
    tgt_db: String,
    src: DbClient,
    tgt: DbClient,
}

impl Pair {
    async fn open(src_flavor: Flavor, tgt_flavor: Flavor) -> Option<Self> {
        let src_server = Server::open(src_flavor).await?;
        let tgt_server = Server::open(tgt_flavor).await?;
        let src_db = src_server.create_db(&format!("{}src", src_flavor.prefix())).await;
        // 源/目标各用独立 database；即使同 flavor 也分别建库，避免源数据污染目标。
        let tgt_db = tgt_server.create_db(&format!("{}tgt", tgt_flavor.prefix())).await;
        let src = src_server.client(src_flavor, &src_db).await;
        let tgt = tgt_server.client(tgt_flavor, &tgt_db).await;
        Some(Self {
            src_server,
            tgt_server,
            src_db,
            tgt_db,
            src,
            tgt,
        })
    }

    async fn cleanup(self) {
        self.src.pool.close().await;
        self.tgt.pool.close().await;
        self.src_server.drop_db(&self.src_db).await;
        self.tgt_server.drop_db(&self.tgt_db).await;
    }
}

fn replace_db(url: &str, db: &str) -> String {
    let (no_query, query) = match url.split_once('?') {
        Some((a, b)) => (a, Some(b)),
        None => (url, None),
    };
    let idx = no_query.rfind('/').unwrap_or(no_query.len());
    let base = &no_query[..idx];
    match query {
        Some(q) => format!("{base}/{db}?{q}"),
        None => format!("{base}/{db}"),
    }
}

fn options(tables: &[&str], batch: usize, dir: &Path) -> PgDataSyncOptions {
    PgDataSyncOptions {
        tables: Some(tables.iter().map(|t| t.to_string()).collect()),
        mode: SyncMode::UpsertOnly,
        batch_size: batch,
        create_missing_tables: true,
        mirror_confirmed: false,
        single_transaction: false,
        defer_constraints: false,
        allow_no_pk: false,
        resume: false,
        checkpoint_dir: dir.to_path_buf(),
        job_label: "integration".to_string(),
    }
}

fn checkpoint_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "data-sync-mysql-{label}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

async fn exec_pool(pool: &MySqlPool, sql: &str) {
    if let Err(err) = Executor::execute(pool, sqlx::raw_sql(sql)).await {
        panic!("执行 SQL 失败：{err}\nSQL: {sql}");
    }
}

async fn exec(client: &DbClient, sql: &str) {
    let pool = client.pool.mysql().expect("mysql pool");
    exec_pool(pool, sql).await;
}

async fn scalar_string(client: &DbClient, sql: &str) -> String {
    let pool = client.pool.mysql().expect("mysql pool");
    sqlx::query_scalar::<_, Option<String>>(sql)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|e| panic!("查询失败：{sql}\n{e}"))
        .unwrap_or_default()
}

async fn scalar_i64(client: &DbClient, sql: &str) -> i64 {
    let pool = client.pool.mysql().expect("mysql pool");
    sqlx::query_scalar::<_, i64>(sql)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|e| panic!("查询失败：{sql}\n{e}"))
}

async fn run(
    pair: &Pair,
    options: PgDataSyncOptions,
) -> Result<PgDataSyncReport, core_lib::error::AppError> {
    MySqlStagedDataSyncEngine::run(pair.src.clone(), pair.tgt.clone(), options, Box::new(|_| {})).await
}

fn assert_verified(report: &PgDataSyncReport) {
    assert!(
        report.error.is_none(),
        "同步失败：{:?}；warnings={:?}",
        report.error,
        report.warnings
    );
    assert_eq!(report.verification_status, "verified");
    for table in &report.tables {
        assert_eq!(table.status, "completed", "表 {} 未完成", table.table_name);
        assert_eq!(
            table.verification, "verified",
            "表 {} 校验未通过：{:?}",
            table.table_name, table.notes
        );
    }
}

async fn create_source_schema(pair: &Pair) {
    exec(
        &pair.src,
        "CREATE TABLE users (
            id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
            name VARCHAR(50) NOT NULL,
            balance DECIMAL(20,6) NULL,
            created_at DATETIME(6) NULL,
            updated_at TIMESTAMP(6) NULL,
            payload JSON NULL,
            avatar BLOB NULL,
            code VARBINARY(16) NULL,
            mood ENUM('sad','ok','happy') NULL,
            tags SET('a','b','c') NULL,
            flags BIT(8) NULL,
            big BIGINT UNSIGNED NULL,
            note TEXT NULL,
            generated_len INT GENERATED ALWAYS AS (CHAR_LENGTH(name)) STORED,
            PRIMARY KEY (id),
            UNIQUE KEY uq_users_name (name),
            KEY idx_users_created (created_at)
        ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4",
    )
    .await;
    exec(
        &pair.src,
        "CREATE TABLE orders (
            id INT NOT NULL AUTO_INCREMENT,
            user_id BIGINT UNSIGNED NOT NULL,
            amount DECIMAL(10,2) NOT NULL,
            PRIMARY KEY (id),
            KEY idx_orders_user (user_id),
            CONSTRAINT fk_orders_user FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
        ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4",
    )
    .await;
    exec(
        &pair.src,
        "CREATE TABLE empty_seq (
            id INT NOT NULL AUTO_INCREMENT,
            v VARCHAR(10) NULL,
            PRIMARY KEY (id)
        ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4",
    )
    .await;

    exec(
        &pair.src,
        "INSERT INTO users
            (id, name, balance, created_at, updated_at, payload, avatar, code,
             mood, tags, flags, big, note)
         VALUES
            (1, 'alice', 1234567890.123456, '2024-01-02 03:04:05.123456',
             '2024-01-02 03:04:05.654321', '{\"x\": 1, \"y\": [true, null]}',
             UNHEX('00FF10'), UNHEX('DEADBEEF'), 'happy', 'a,c', b'10101010',
             18446744073709551615, 'hello 世界'),
            (2, 'bob', -0.000001, '2024-02-03 04:05:06.000001',
             NULL, NULL, NULL, NULL, 'sad', NULL, b'00000001', 0, NULL)",
    )
    .await;
    exec(
        &pair.src,
        "INSERT INTO orders (id, user_id, amount) VALUES
            (1, 1, 10.25), (2, 2, 0.01), (3, 1, 99999999.99)",
    )
    .await;
}

async fn assert_types(pair: &Pair) {
    assert_eq!(
        scalar_i64(&pair.tgt, "SELECT COUNT(*) FROM users").await,
        2
    );
    assert_eq!(
        scalar_i64(&pair.tgt, "SELECT COUNT(*) FROM orders").await,
        3
    );
    assert_eq!(
        scalar_string(&pair.tgt, "SELECT CAST(balance AS CHAR) FROM users WHERE id = 1").await,
        "1234567890.123456"
    );
    assert_eq!(
        scalar_string(&pair.tgt, "SELECT CAST(balance AS CHAR) FROM users WHERE id = 2").await,
        "-0.000001"
    );
    assert_eq!(
        scalar_string(&pair.tgt, "SELECT HEX(avatar) FROM users WHERE id = 1").await,
        "00FF10"
    );
    assert_eq!(
        scalar_string(&pair.tgt, "SELECT HEX(code) FROM users WHERE id = 1").await,
        "DEADBEEF"
    );
    assert_eq!(
        scalar_string(&pair.tgt, "SELECT mood FROM users WHERE id = 1").await,
        "happy"
    );
    assert_eq!(
        scalar_string(&pair.tgt, "SELECT CAST(tags AS CHAR) FROM users WHERE id = 1").await,
        "a,c"
    );
    assert_eq!(
        scalar_string(&pair.tgt, "SELECT CAST(flags + 0 AS CHAR) FROM users WHERE id = 1").await,
        "170"
    );
    assert_eq!(
        scalar_string(&pair.tgt, "SELECT CAST(big AS CHAR) FROM users WHERE id = 1").await,
        "18446744073709551615"
    );
    assert_eq!(
        scalar_string(
            &pair.tgt,
            "SELECT DATE_FORMAT(created_at, '%Y-%m-%d %H:%i:%s.%f') FROM users WHERE id = 1"
        )
        .await,
        "2024-01-02 03:04:05.123456"
    );
    assert_eq!(
        scalar_string(
            &pair.tgt,
            "SELECT DATE_FORMAT(updated_at, '%Y-%m-%d %H:%i:%s.%f') FROM users WHERE id = 1"
        )
        .await,
        "2024-01-02 03:04:05.654321"
    );
    assert_eq!(
        scalar_string(
            &pair.tgt,
            "SELECT CAST(JSON_UNQUOTE(JSON_EXTRACT(payload, '$.x')) AS CHAR) FROM users WHERE id = 1"
        )
        .await,
        "1"
    );
    assert_eq!(
        scalar_i64(&pair.tgt, "SELECT generated_len FROM users WHERE id = 1").await,
        5
    );
    assert_eq!(
        scalar_i64(
            &pair.tgt,
            "SELECT COUNT(*) FROM information_schema.STATISTICS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'users' AND INDEX_NAME = 'idx_users_created'"
        )
        .await,
        1
    );
    assert_eq!(
        scalar_i64(
            &pair.tgt,
            "SELECT COUNT(*) FROM information_schema.TABLE_CONSTRAINTS WHERE CONSTRAINT_SCHEMA = DATABASE() AND TABLE_NAME = 'orders' AND CONSTRAINT_NAME = 'fk_orders_user' AND CONSTRAINT_TYPE = 'FOREIGN KEY'"
        )
        .await,
        1
    );
}

async fn assert_sequence_alignment(pair: &Pair) {
    exec(
        &pair.tgt,
        "INSERT INTO users (name, balance) VALUES ('carol', 1.000000)",
    )
    .await;
    let carol_id = scalar_i64(
        &pair.tgt,
        "SELECT CAST(id AS SIGNED) FROM users WHERE name = 'carol'",
    )
    .await;
    assert_eq!(carol_id, 3, "AUTO_INCREMENT 未对齐到 max(id)+1");
    exec(&pair.tgt, "DELETE FROM users WHERE name = 'carol'").await;

    exec(&pair.tgt, "INSERT INTO empty_seq (v) VALUES ('first')").await;
    let first_id = scalar_i64(&pair.tgt, "SELECT id FROM empty_seq WHERE v = 'first'").await;
    assert_eq!(first_id, 1, "空表 AUTO_INCREMENT 应从 1 开始");
    exec(&pair.tgt, "DELETE FROM empty_seq").await;
}

async fn run_types_and_idempotency(flavor: Flavor) {
    let Some(pair) = Pair::open(flavor, flavor).await else {
        return;
    };
    create_source_schema(&pair).await;

    let dir = checkpoint_dir("types");
    let report = run(&pair, options(&["users", "orders", "empty_seq"], 1, &dir))
        .await
        .expect("types sync");
    assert_verified(&report);
    assert!(!report.atomic, "MySQL DDL 隐式提交，不能声称 job 级原子");
    assert_eq!(report.rollback_scope, "batch");
    assert!(report.resumable);
    assert!(
        report.warnings.iter().any(|w| w.contains("隐式提交")),
        "结果必须说明 DDL 隐式提交"
    );
    assert!(
        report
            .non_transactional_tables
            .iter()
            .any(|t| t == "users")
    );
    assert_types(&pair).await;
    assert_sequence_alignment(&pair).await;

    // 重复执行必须幂等：同一份数据再跑一次，行数不能增加。
    let dir2 = checkpoint_dir("types-idempotent");
    let report2 = run(&pair, options(&["users", "orders", "empty_seq"], 2, &dir2))
        .await
        .expect("idempotent sync");
    assert_verified(&report2);
    assert_eq!(scalar_i64(&pair.tgt, "SELECT COUNT(*) FROM users").await, 2);
    assert_eq!(scalar_i64(&pair.tgt, "SELECT COUNT(*) FROM orders").await, 3);
    assert_eq!(
        scalar_i64(&pair.tgt, "SELECT COUNT(*) FROM empty_seq").await,
        0
    );
    assert_sequence_alignment(&pair).await;

    pair.cleanup().await;
}

async fn run_resume(flavor: Flavor) {
    let Some(pair) = Pair::open(flavor, flavor).await else {
        return;
    };
    exec(
        &pair.src,
        "CREATE TABLE resume_users (id INT NOT NULL, v VARCHAR(10) NULL, PRIMARY KEY (id)) ENGINE=InnoDB",
    )
    .await;
    exec(
        &pair.src,
        "INSERT INTO resume_users (id, v) VALUES (1,'a'),(2,'b'),(3,'c'),(4,'d'),(5,'e')",
    )
    .await;
    exec(
        &pair.tgt,
        "CREATE TABLE resume_users (id INT NOT NULL, v VARCHAR(10) NULL, PRIMARY KEY (id)) ENGINE=InnoDB",
    )
    .await;
    // 第 4 行插入时触发失败：前 3 个批次已提交，第 4 个批次回滚。
    exec(
        &pair.tgt,
        "CREATE TRIGGER resume_boom BEFORE INSERT ON resume_users FOR EACH ROW
         BEGIN
           IF NEW.id >= 4 THEN
             SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'interrupt after 3 rows';
           END IF;
         END",
    )
    .await;

    let dir = checkpoint_dir("resume");
    let mut opts = options(&["resume_users"], 1, &dir);
    opts.create_missing_tables = false;
    let first = run(&pair, opts.clone()).await.expect("first run report");
    assert!(first.error.is_some(), "第一次应因触发器失败");
    assert_eq!(first.tables[0].status, "failed");
    assert_eq!(first.tables[0].rows_copied, 3);
    assert_eq!(
        scalar_i64(&pair.tgt, "SELECT COUNT(*) FROM resume_users").await,
        3
    );
    assert!(first.rolled_back, "失败批次应回滚");
    assert!(first.resumable);

    exec(&pair.tgt, "DROP TRIGGER resume_boom").await;
    let mut resume = opts.clone();
    resume.resume = true;
    let second = run(&pair, resume).await.expect("resume run");
    assert_verified(&second);
    assert_eq!(
        scalar_i64(&pair.tgt, "SELECT COUNT(*) FROM resume_users").await,
        5
    );
    assert_eq!(
        scalar_i64(
            &pair.tgt,
            "SELECT COUNT(DISTINCT id) FROM resume_users"
        )
        .await,
        5
    );

    let missing = checkpoint_dir("resume-missing");
    let mut err_opts = options(&["resume_users"], 1, &missing);
    err_opts.resume = true;
    let err = run(&pair, err_opts).await.expect_err("无断点续跑必须报错");
    assert!(
        err.to_string().contains("未找到可续跑的断点"),
        "错误信息不对：{err}"
    );

    pair.cleanup().await;
}

async fn run_mirror_and_rejections(flavor: Flavor) {
    let Some(pair) = Pair::open(flavor, flavor).await else {
        return;
    };
    exec(
        &pair.src,
        "CREATE TABLE mirror_items (id INT NOT NULL, v VARCHAR(10) NULL, PRIMARY KEY (id)) ENGINE=InnoDB",
    )
    .await;
    exec(
        &pair.src,
        "INSERT INTO mirror_items (id, v) VALUES (1,'new1'),(2,'new2'),(3,'new3')",
    )
    .await;
    exec(
        &pair.tgt,
        "CREATE TABLE mirror_items (id INT NOT NULL, v VARCHAR(10) NULL, PRIMARY KEY (id)) ENGINE=InnoDB",
    )
    .await;
    exec(
        &pair.tgt,
        "INSERT INTO mirror_items (id, v) VALUES (1,'old1'),(3,'old3'),(4,'extra')",
    )
    .await;

    let dir = checkpoint_dir("mirror");
    let mut mirror = options(&["mirror_items"], 2, &dir);
    mirror.create_missing_tables = false;
    mirror.mode = SyncMode::Mirror;
    let err = run(&pair, mirror.clone())
        .await
        .expect_err("mirror 未确认必须拒绝");
    assert!(err.to_string().contains("mirror_confirmed"), "{err}");

    mirror.mirror_confirmed = true;
    let report = run(&pair, mirror).await.expect("mirror sync");
    assert_verified(&report);
    assert_eq!(
        scalar_string(
            &pair.tgt,
            "SELECT GROUP_CONCAT(CONCAT(id, ':', v) ORDER BY id) FROM mirror_items"
        )
        .await,
        "1:new1,2:new2,3:new3"
    );

    // 无主键默认拒绝；显式 allow_no_pk 后只做行数校验。
    exec(
        &pair.src,
        "CREATE TABLE no_pk_items (v VARCHAR(10) NULL) ENGINE=InnoDB",
    )
    .await;
    exec(&pair.src, "INSERT INTO no_pk_items (v) VALUES ('x'),('y')").await;
    let dir = checkpoint_dir("no-pk");
    let mut no_pk = options(&["no_pk_items"], 10, &dir);
    no_pk.create_missing_tables = true;
    let err = run(&pair, no_pk.clone())
        .await
        .expect_err("无主键默认必须拒绝");
    assert!(err.to_string().contains("没有主键"), "{err}");
    no_pk.allow_no_pk = true;
    let report = run(&pair, no_pk).await.expect("allow_no_pk sync");
    assert!(report.error.is_none(), "{:?}", report.error);
    assert_eq!(report.tables[0].verification, "downgraded_unverified");
    assert_eq!(report.verification_status, "downgraded_unverified");
    assert!(!report.resumable);
    assert_eq!(
        scalar_i64(&pair.tgt, "SELECT COUNT(*) FROM no_pk_items").await,
        2
    );

    // 外键环：默认拒绝，defer_constraints=true 时改为数据后建外键。
    // 先建表并灌入互相引用的行，再加外键，避免源库自身写入就被外键挡住。
    exec(
        &pair.src,
        "CREATE TABLE cyc_a (id INT NOT NULL, b_id INT NULL, PRIMARY KEY (id), KEY idx_a_b (b_id)) ENGINE=InnoDB",
    )
    .await;
    exec(
        &pair.src,
        "CREATE TABLE cyc_b (id INT NOT NULL, a_id INT NULL, PRIMARY KEY (id), KEY idx_b_a (a_id)) ENGINE=InnoDB",
    )
    .await;
    exec(&pair.src, "INSERT INTO cyc_a (id, b_id) VALUES (1, 1)").await;
    exec(&pair.src, "INSERT INTO cyc_b (id, a_id) VALUES (1, 1)").await;
    exec(
        &pair.src,
        "ALTER TABLE cyc_a ADD CONSTRAINT fk_cyc_a_b FOREIGN KEY (b_id) REFERENCES cyc_b(id)",
    )
    .await;
    exec(
        &pair.src,
        "ALTER TABLE cyc_b ADD CONSTRAINT fk_cyc_b_a FOREIGN KEY (a_id) REFERENCES cyc_a(id)",
    )
    .await;
    let dir = checkpoint_dir("cycle");
    let mut cycle = options(&["cyc_a", "cyc_b"], 2, &dir);
    let err = run(&pair, cycle.clone())
        .await
        .expect_err("外键环默认必须拒绝");
    assert!(err.to_string().contains("外键循环依赖"), "{err}");
    cycle.defer_constraints = true;
    let report = run(&pair, cycle).await.expect("defer constraints sync");
    assert_verified(&report);
    assert!(report.deferred_constraints);
    assert_eq!(
        scalar_i64(
            &pair.tgt,
            "SELECT COUNT(*) FROM information_schema.TABLE_CONSTRAINTS WHERE CONSTRAINT_SCHEMA = DATABASE() AND CONSTRAINT_TYPE = 'FOREIGN KEY' AND TABLE_NAME IN ('cyc_a','cyc_b')"
        )
        .await,
        2
    );

    // MySQL DDL 隐式提交，single_transaction 必须显式拒绝。
    let dir = checkpoint_dir("single");
    let mut single = options(&["mirror_items"], 2, &dir);
    single.create_missing_tables = false;
    single.single_transaction = true;
    let err = run(&pair, single)
        .await
        .expect_err("single_transaction 必须拒绝");
    assert!(err.to_string().contains("隐式提交"), "{err}");

    // 缺表且禁止建表必须拒绝。
    exec(
        &pair.src,
        "CREATE TABLE missing_target (id INT NOT NULL, PRIMARY KEY (id)) ENGINE=InnoDB",
    )
    .await;
    let dir = checkpoint_dir("missing");
    let mut missing = options(&["missing_target"], 2, &dir);
    missing.create_missing_tables = false;
    let err = run(&pair, missing)
        .await
        .expect_err("缺表且禁止建表必须拒绝");
    assert!(
        err.to_string().contains("create_missing_tables=false"),
        "{err}"
    );

    pair.cleanup().await;
}

async fn run_cross_flavor() {
    let Some(src_server) = Server::open(Flavor::Mysql).await else {
        return;
    };
    let Some(tgt_server) = Server::open(Flavor::MariaDb).await else {
        return;
    };
    let src_db = src_server.create_db("xmy").await;
    let tgt_db = tgt_server.create_db("xma").await;
    let src = src_server.client(Flavor::Mysql, &src_db).await;
    let tgt = tgt_server.client(Flavor::MariaDb, &tgt_db).await;
    let pair = Pair {
        src_server,
        tgt_server,
        src_db: src_db.clone(),
        tgt_db: tgt_db.clone(),
        src,
        tgt,
    };

    exec(
        &pair.src,
        "CREATE TABLE cross_items (id INT NOT NULL, v VARCHAR(20) NULL, PRIMARY KEY (id)) ENGINE=InnoDB",
    )
    .await;
    exec(
        &pair.src,
        "INSERT INTO cross_items (id, v) VALUES (1,'mysql-new'),(2,'two')",
    )
    .await;
    exec(
        &pair.tgt,
        "CREATE TABLE cross_items (id INT NOT NULL, v VARCHAR(20) NULL, PRIMARY KEY (id)) ENGINE=InnoDB",
    )
    .await;
    exec(
        &pair.tgt,
        "INSERT INTO cross_items (id, v) VALUES (1,'mariadb-old')",
    )
    .await;
    let dir = checkpoint_dir("cross-my-ma");
    let mut opts = options(&["cross_items"], 1, &dir);
    opts.create_missing_tables = false;
    let report = run(&pair, opts).await.expect("MySQL -> MariaDB");
    assert_verified(&report);
    assert_eq!(
        scalar_string(&pair.tgt, "SELECT v FROM cross_items WHERE id = 1").await,
        "mysql-new",
        "MariaDB 目标必须用 VALUES(col) 方言完成 upsert"
    );
    assert_eq!(
        scalar_i64(&pair.tgt, "SELECT COUNT(*) FROM cross_items").await,
        2
    );

    pair.cleanup().await;
}

mod mysql_tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn types_sequence_and_idempotent() {
        run_types_and_idempotency(Flavor::Mysql).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resume_after_interruption() {
        run_resume(Flavor::Mysql).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mirror_no_pk_cycle_and_plan_rejections() {
        run_mirror_and_rejections(Flavor::Mysql).await;
    }
}

mod mariadb_tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn types_sequence_and_idempotent() {
        run_types_and_idempotency(Flavor::MariaDb).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resume_after_interruption() {
        run_resume(Flavor::MariaDb).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mirror_no_pk_cycle_and_plan_rejections() {
        run_mirror_and_rejections(Flavor::MariaDb).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cross_flavor_upsert_uses_target_dialect() {
    run_cross_flavor().await;
}
