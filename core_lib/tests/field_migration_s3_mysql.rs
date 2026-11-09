//! 字段治理 S3 真实 MySQL 验证（opt-in）。
//!
//! 运行方式：
//! ```bash
//! FIELD_MIGRATION_MYSQL_URL='mysql://root:password@127.0.0.1:3306/e2e' \
//!   cargo test -p core_lib --test field_migration_s3_mysql -- --nocapture
//! ```
//! 未设置环境变量时自动跳过，不影响常规单测。
//!
//! 覆盖：同一列同时被索引/外键/生成列/视图引用时依赖扫描全部命中；
//! 1:1 与 1:N 两种迁移的回滚结论不同；观察期未满时的提示文案；
//! 下线包里所有脚本均不可执行、DROP COLUMN 没有可执行路径。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use sqlx::mysql::MySqlPool;

use core_lib::db::client::DbClient;
use core_lib::ops::field_migration::types::{
    FieldGovernanceAnalysis, FieldGovernanceInput, TargetSpec,
};
use core_lib::ops::policy::OpsPolicy;
use core_lib::ops::registry::{OpsContext, ScenarioRegistry};

const DB_ID: &str = "field-migration-s3-e2e";

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

/// 换掉 URL 里的库名，保留账号、主机、端口与查询参数。
fn with_database(url: &str, db: &str) -> String {
    match url.rsplit_once('/') {
        Some((base, tail)) => {
            let query = tail.find('?').map(|i| &tail[i..]).unwrap_or("");
            format!("{base}/{db}{query}")
        }
        None => format!("{url}/{db}"),
    }
}

async fn admin_exec(pool: &MySqlPool, sql: &str) {
    if let Err(err) = sqlx::query(sql).execute(pool).await {
        panic!("执行管理 SQL 失败: {err}\nSQL: {sql}");
    }
}

/// 创建本用例独占的库；同名库若存在（极端情况下的残留）先整体清掉。
async fn create_database(pool: &MySqlPool, db: &str) {
    admin_exec(pool, &format!("DROP DATABASE IF EXISTS `{db}`")).await;
    admin_exec(pool, &format!("CREATE DATABASE IF NOT EXISTS `{db}`")).await;
}

/// 只删除本用例创建的库；失败必须显式报错，不能静默吞掉。
async fn cleanup_database(pool: &MySqlPool, db: &str) {
    let sql = format!("DROP DATABASE IF EXISTS `{db}`");
    if let Err(err) = sqlx::query(&sql).execute(pool).await {
        eprintln!("清理失败: 数据库 `{db}`: {err}\nSQL: {sql}");
        panic!("清理失败: 数据库 `{db}`: {err}");
    }
}

/// 连接管理库并创建本用例独占的库，返回库名与管理连接。
async fn isolated_database(base_url: &str, prefix: &str) -> (String, MySqlPool) {
    let admin_pool = MySqlPool::connect(base_url)
        .await
        .expect("连接 MySQL 管理库失败，请确认 compose 服务已就绪");
    let db = unique_db(prefix);
    create_database(&admin_pool, &db).await;
    (db, admin_pool)
}

async fn close_pool(pool: MySqlPool) {
    // sqlx 该版本 close() 不返回错误，调用即完成关闭。
    pool.close().await;
}

async fn exec(conn: &mut sqlx::pool::PoolConnection<sqlx::MySql>, sql: &str) {
    // 触发器/存储过程等 DDL 不支持 prepared statement 协议，统一走文本协议。
    if let Err(err) = sqlx::raw_sql(sql).execute(&mut **conn).await {
        panic!("执行 SQL 失败: {err}\nSQL: {sql}");
    }
}

async fn setup(conn: &mut sqlx::pool::PoolConnection<sqlx::MySql>) {
    exec(conn, "DROP TRIGGER IF EXISTS trg_s3_orders").await;
    exec(conn, "DROP PROCEDURE IF EXISTS sp_s3_owner_count").await;
    exec(conn, "DROP FUNCTION IF EXISTS fn_s3_owner_count").await;
    exec(conn, "DROP PROCEDURE IF EXISTS sp_s3_noise").await;
    exec(conn, "DROP VIEW IF EXISTS v_s3_noise").await;
    exec(conn, "DROP VIEW IF EXISTS v_s3_orders").await;
    exec(conn, "DROP TABLE IF EXISTS fm_s3_orders").await;
    exec(conn, "DROP TABLE IF EXISTS fm_s3_owner_ext").await;
    exec(conn, "DROP TABLE IF EXISTS fm_s3_users").await;
    exec(
        conn,
        "CREATE TABLE fm_s3_users (id INT NOT NULL PRIMARY KEY) ENGINE=InnoDB",
    )
    .await;
    exec(
        conn,
        "CREATE TABLE fm_s3_orders (\
           id INT NOT NULL AUTO_INCREMENT, \
           owner_id INT NOT NULL, \
           tags JSON NULL, \
           owner_plus_one INT GENERATED ALWAYS AS (owner_id + 1) STORED, \
           PRIMARY KEY (id), \
           KEY idx_s3_owner (owner_id), \
           CONSTRAINT fk_s3_owner FOREIGN KEY (owner_id) REFERENCES fm_s3_users (id)\
         ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4",
    )
    .await;
    exec(
        conn,
        "CREATE VIEW v_s3_orders AS SELECT id, owner_id FROM fm_s3_orders",
    )
    .await;
    // 触发器与存储过程/函数都真实引用 owner_id。
    exec(
        conn,
        "CREATE TRIGGER trg_s3_orders BEFORE INSERT ON fm_s3_orders \
         FOR EACH ROW SET NEW.owner_id = NEW.owner_id",
    )
    .await;
    exec(
        conn,
        "CREATE PROCEDURE sp_s3_owner_count(IN p_owner INT) \
         SELECT COUNT(*) FROM fm_s3_orders WHERE owner_id = p_owner",
    )
    .await;
    exec(
        conn,
        "CREATE FUNCTION fn_s3_owner_count(p INT) RETURNS INT \
         DETERMINISTIC READS SQL DATA \
         RETURN (SELECT COUNT(*) FROM fm_s3_orders WHERE owner_id = p)",
    )
    .await;
    // 噪声：列名只出现在字符串字面量里，不能被当成依赖。
    exec(
        conn,
        "CREATE VIEW v_s3_noise AS SELECT 'owner_id' AS label",
    )
    .await;
    exec(
        conn,
        "CREATE PROCEDURE sp_s3_noise() SELECT 'owner_id' AS label",
    )
    .await;
    exec(
        conn,
        "INSERT INTO fm_s3_users (id) VALUES (1), (2), (3)",
    )
    .await;
    exec(
        conn,
        "INSERT INTO fm_s3_orders (owner_id, tags) VALUES \
         (1, '[\"a\",\"b\"]'), (2, '[\"c\"]'), (3, NULL)",
    )
    .await;
    exec(conn, "ANALYZE TABLE fm_s3_orders").await;
}

async fn analyze(ctx: &OpsContext, input: &FieldGovernanceInput) -> FieldGovernanceAnalysis {
    let registry = ScenarioRegistry::builtin();
    let scenario = registry.find("field-governance").expect("场景未注册");
    let input_value = serde_json::to_value(input).expect("序列化输入失败");
    let evidence = scenario
        .collect_evidence(ctx, &input_value)
        .await
        .expect("采集证据失败");
    serde_json::from_value(evidence.extra.expect("缺少字段治理扩展证据"))
        .expect("解析字段治理证据失败")
}

fn item<'a>(
    analysis: &'a FieldGovernanceAnalysis,
    id: &str,
) -> &'a core_lib::ops::field_migration::types::RetireChecklistItem {
    analysis
        .retire_package
        .checklist
        .iter()
        .find(|item| item.id == id)
        .unwrap_or_else(|| {
            panic!(
                "缺少清单项 {id}；实际: {:?}",
                analysis.retire_package.checklist
            )
        })
}

#[tokio::test]
async fn s3_dependency_scan_hits_all_and_rollback_differs_for_1_to_1_and_1_to_n() {
    let Ok(base_url) = std::env::var("FIELD_MIGRATION_MYSQL_URL") else {
        eprintln!("跳过字段治理 S3 真实库验证：未设置 FIELD_MIGRATION_MYSQL_URL");
        return;
    };
    let (db_name, admin_pool) = isolated_database(&base_url, "it_s3").await;
    let url = with_database(&base_url, &db_name);
    let pool = MySqlPool::connect(&url).await.expect("连接 MySQL 失败");
    let mut conn = pool.acquire().await.expect("获取 MySQL 连接失败");
    setup(&mut conn).await;

    let policy = Arc::new(OpsPolicy {
        field_profile_max_rows: 10_000_000,
        field_profile_max_bytes: 20 * 1024 * 1024 * 1024,
        ..Default::default()
    });
    let client = DbClient::new_default(&url).await.expect("创建 DbClient 失败");
    let ctx = OpsContext::new(client, DB_ID.into(), db_name.clone(), false)
        .with_policy(policy.clone());

    let today = chrono::Utc::now().date_naive().format("%Y-%m-%d").to_string();

    // ── 1) 同一列同时被索引/外键/生成列/视图引用 ─────────────
    let scalar = analyze(
        &ctx,
        &FieldGovernanceInput {
            table: "fm_s3_orders".into(),
            column: Some("owner_id".into()),
            profile_mode: Some("full".into()),
            target: Some(TargetSpec {
                table: Some("fm_s3_owner_ext".into()),
                mappings: vec![core_lib::ops::field_migration::types::ColumnMapping {
                    source_key: None,
                    target_column: "owner_id".into(),
                    target_type: Some("INT".into()),
                    required: Some(true),
                }],
                ..Default::default()
            }),
            observation_started_at: Some(today.clone()),
            ..Default::default()
        },
    )
    .await;

    println!("\n=== S3 依赖扫描（owner_id）===");
    println!("indexes={:?}", scalar.dependencies.indexes);
    println!("foreign_keys={:?}", scalar.dependencies.foreign_keys);
    println!(
        "generated_columns={:?}",
        scalar.dependencies.generated_columns
    );
    println!("objects={:?}", scalar.dependencies.objects);
    println!("retire checklist:");
    for entry in &scalar.retire_package.checklist {
        println!(
            "  [{}] auto={} status={} :: {}",
            entry.id, entry.auto_verified, entry.status, entry.detail
        );
    }

    assert!(
        scalar
            .dependencies
            .indexes
            .iter()
            .any(|index| index.columns.iter().any(|column| column == "owner_id")),
        "索引依赖必须命中 owner_id"
    );
    assert!(
        scalar
            .dependencies
            .foreign_keys
            .iter()
            .any(|fk| fk.column == "owner_id"),
        "外键依赖必须命中 owner_id"
    );
    assert!(
        scalar
            .dependencies
            .generated_columns
            .iter()
            .any(|generated| generated.expression.contains("owner_id")),
        "生成列依赖必须命中 owner_id"
    );
    assert!(
        scalar
            .dependencies
            .objects
            .iter()
            .any(|object| object.kind == "view"
                && object.name.contains("v_s3_orders")
                && object.matched_columns.iter().any(|column| column == "owner_id")),
        "视图依赖必须命中 owner_id"
    );
    assert!(
        scalar
            .dependencies
            .objects
            .iter()
            .any(|object| object.kind == "trigger"
                && object.name.eq_ignore_ascii_case("trg_s3_orders")
                && object.matched_columns.iter().any(|column| column == "owner_id")),
        "TRIGGERS 依赖必须命中 owner_id"
    );
    assert!(
        scalar
            .dependencies
            .objects
            .iter()
            .any(|object| object.kind == "routine"
                && object.name.eq_ignore_ascii_case("sp_s3_owner_count")
                && object.matched_columns.iter().any(|column| column == "owner_id")),
        "ROUTINES 中的存储过程必须命中 owner_id"
    );
    assert!(
        scalar
            .dependencies
            .objects
            .iter()
            .any(|object| object.kind == "routine"
                && object.name.eq_ignore_ascii_case("fn_s3_owner_count")
                && object.matched_columns.iter().any(|column| column == "owner_id")),
        "ROUTINES 中的函数必须命中 owner_id"
    );
    assert!(
        scalar
            .dependencies
            .source_triggers
            .iter()
            .any(|name| name.eq_ignore_ascii_case("trg_s3_orders")),
        "源表触发器必须被记录"
    );
    for id in [
        "dependency:indexes",
        "dependency:foreign_keys",
        "dependency:generated_columns",
        "dependency:views",
        "dependency:triggers",
        "dependency:routines",
        "dependency:source_triggers",
    ] {
        assert_eq!(item(&scalar, id).status, "fail", "{id} 应命中并阻断下线");
        assert!(item(&scalar, id).auto_verified, "{id} 应标记为自动核实");
    }

    // 字符串字面量中的同名列不能造成假阳性。
    assert!(
        !scalar
            .dependencies
            .objects
            .iter()
            .any(|object| object.name.eq_ignore_ascii_case("v_s3_noise")),
        "视图字符串字面量中的 owner_id 不应命中"
    );
    assert!(
        !scalar
            .dependencies
            .objects
            .iter()
            .any(|object| object.name.eq_ignore_ascii_case("sp_s3_noise")),
        "存储过程字符串字面量中的 owner_id 不应命中"
    );
    assert!(scalar.retire_package.generated);
    assert!(!scalar.retire_package.observation_satisfied);
    assert!(scalar.retire_package.observation_note.contains("观察期未满"));
    assert!(scalar.retire_package.observation_note.contains("仅提示"));

    // 所有脚本都只展示，不能有可执行路径；DROP COLUMN 只出现在不可执行脚本里。
    assert!(!scalar.scripts.is_empty());
    for script in &scalar.scripts {
        assert!(!script.executable, "脚本 {} 不能可执行", script.id);
    }
    let retire = scalar
        .scripts
        .iter()
        .find(|script| script.kind == "retire")
        .expect("缺少下线脚本");
    assert!(retire.sql.contains("DROP COLUMN"));
    assert!(retire.irreversible);
    for id in ["snapshot-create", "snapshot-copy", "snapshot-verify"] {
        assert!(
            scalar.scripts.iter().any(|script| script.id == id),
            "缺少快照脚本 {id}"
        );
    }
    assert!(!scalar.dry_run.executed_against_database);
    assert!(scalar
        .dry_run
        .checks
        .iter()
        .any(|check| check.script_id == "snapshot-copy"));

    // 1:1 标量：结论为可回滚。
    println!(
        "\n=== 1:1 回滚评估 === shape={} conclusion={} summary={}",
        scalar.rollback.shape, scalar.rollback.conclusion, scalar.rollback.summary
    );
    assert_eq!(scalar.rollback.shape, "scalar_1_to_1");
    assert_eq!(scalar.rollback.conclusion, "reversible");
    assert!(!scalar.rollback.irreversible);
    assert!(scalar.rollback.summary.contains("可回滚"));
    assert!(scalar
        .rollback
        .reverse_sql
        .as_deref()
        .unwrap_or_default()
        .contains("UPDATE"));

    // ── 2) 1:N：JSON 顶层数组，结论为不可回滚 ───────────────
    let one_to_n = analyze(
        &ctx,
        &FieldGovernanceInput {
            table: "fm_s3_orders".into(),
            column: Some("tags".into()),
            profile_mode: Some("full".into()),
            ..Default::default()
        },
    )
    .await;
    println!(
        "\n=== 1:N 回滚评估 === shape={} conclusion={} summary={}",
        one_to_n.rollback.shape, one_to_n.rollback.conclusion, one_to_n.rollback.summary
    );
    assert_eq!(one_to_n.rollback.shape, "json_1_to_n");
    assert_eq!(one_to_n.rollback.conclusion, "irreversible");
    assert!(one_to_n.rollback.irreversible);
    assert!(one_to_n.rollback.summary.contains("不可回滚"));
    assert!(one_to_n.rollback.summary.contains("1:N"));
    assert!(one_to_n.rollback.reverse_sql.is_none());
    assert_ne!(
        scalar.rollback.conclusion, one_to_n.rollback.conclusion,
        "1:1 与 1:N 的回滚结论必须不同"
    );

    drop(conn);
    drop(ctx);
    close_pool(pool).await;
    cleanup_database(&admin_pool, &db_name).await;
}
