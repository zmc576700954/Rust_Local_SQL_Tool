//! 真实 PostgreSQL 巡检验证（opt-in）。
//!
//! 运行方式：
//! ```bash
//! INSPECTION_PG_URL='postgres://postgres:password@127.0.0.1:55432/postgres' \
//!   cargo test -p core_lib --test inspection_pg -- --nocapture --test-threads=1
//! ```
//! 未设置环境变量时自动跳过，不影响常规单测。
//!
//! 每个用例创建独立 schema 与独立角色，结束后清理；不触碰既有业务表。

use sqlx::postgres::PgPool;
use sqlx::Row;

use core_lib::db::client::DbClient;
use core_lib::ops::inspection::presets::all_check_ids;
use core_lib::ops::inspection::runner::run_inspection;
use core_lib::ops::inspection::types::{
    CheckResult, CheckStatus, InspectionThresholds, CHECK_CHARSET_COLLATION, CHECK_INACTIVE_ACCOUNT,
    CHECK_MISSING_PRIMARY_KEY, CHECK_ORPHAN_FOREIGN_KEY, CHECK_OVER_PRIVILEGED_ACCOUNT,
    CHECK_PASSWORDLESS_ACCOUNT, CHECK_REDUNDANT_INDEX, CHECK_TABLE_CAPACITY, CHECK_UNUSED_INDEX,
};
use core_lib::ops::OpsContext;
use std::sync::atomic::{AtomicU64, Ordering};

static SEQ: AtomicU64 = AtomicU64::new(0);

fn base_url() -> Option<String> {
    std::env::var("INSPECTION_PG_URL").ok().filter(|url| !url.trim().is_empty())
}

fn unique_suffix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "{}_{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed) + nanos as u64 % 1_000_000
    )
}

fn database_name(url: &str) -> String {
    let tail = url.rsplit('/').next().unwrap_or("");
    tail.split('?').next().unwrap_or("").to_string()
}

/// 给连接串追加 `options=-csearch_path=<schema>`，让巡检的 current_schema() 落在用例 schema。
fn with_search_path(url: &str, schema: &str) -> String {
    let separator = if url.contains('?') { '&' } else { '?' };
    format!("{url}{separator}options=-csearch_path%3D{schema}")
}

fn admin_url(url: &str) -> String {
    // 管理连接必须落在被测库本身：schema 建在哪个库，巡检的 current_schema() 才能命中。
    url.to_string()
}

struct Fixture {
    schema: String,
    role_createdb: String,
    role_passwordless: String,
    admin: PgPool,
    client_url: String,
    db_name: String,
}

impl Fixture {
    async fn setup(url: &str) -> Self {
        let schema = format!("insp_{}", unique_suffix());
        let role_createdb = format!("insp_dba_{}", unique_suffix());
        let role_passwordless = format!("insp_nopw_{}", unique_suffix());
        let admin = PgPool::connect(&admin_url(url))
            .await
            .expect("连接 PostgreSQL 管理库失败，请确认容器已就绪");

        let mut conn = admin.acquire().await.expect("获取管理连接失败");

        let statements = vec![
            format!("DROP SCHEMA IF EXISTS {schema} CASCADE"),
            format!("CREATE SCHEMA {schema}"),
            format!("SET search_path TO {schema}"),
            "CREATE TABLE parent(id int PRIMARY KEY)".to_string(),
            "CREATE TABLE child(id int PRIMARY KEY, parent_id int)".to_string(),
            "ALTER TABLE child ADD CONSTRAINT fk_child_parent FOREIGN KEY (parent_id) REFERENCES parent(id)"
                .to_string(),
            "CREATE TABLE orphan_child(id int PRIMARY KEY, parent_id int)".to_string(),
            "INSERT INTO orphan_child VALUES (1, 999)".to_string(),
            "ALTER TABLE orphan_child ADD CONSTRAINT fk_orphan_child FOREIGN KEY (parent_id) REFERENCES parent(id) NOT VALID"
                .to_string(),
            "CREATE TABLE redundant_demo(id int PRIMARY KEY, a int, b text, c int, cname text)"
                .to_string(),
            "CREATE INDEX idx_a ON redundant_demo(a)".to_string(),
            "CREATE INDEX idx_ab ON redundant_demo(a, b)".to_string(),
            "CREATE INDEX idx_lower ON redundant_demo(lower(b))".to_string(),
            "CREATE INDEX idx_lower_dup ON redundant_demo((lower(b)))".to_string(),
            "CREATE INDEX idx_upper ON redundant_demo(upper(b))".to_string(),
            "CREATE INDEX idx_partial ON redundant_demo(a) WHERE c > 0".to_string(),
            "CREATE INDEX idx_unused ON redundant_demo(c)".to_string(),
            "CREATE TABLE no_pk(x int)".to_string(),
        ];
        for sql in statements {
            if let Err(err) = sqlx::query(&sql).execute(&mut *conn).await {
                panic!("执行管理 SQL 失败: {err}\nSQL: {sql}");
            }
        }

        // 选一个与库默认不同的 collation，确保列级差异可判定。
        let default_collation: String = sqlx::query("SELECT datcollate FROM pg_database WHERE datname = current_database()")
            .fetch_one(&mut *conn)
            .await
            .expect("读取 datcollate 失败")
            .get(0);
        let collation = if default_collation.eq_ignore_ascii_case("C") {
            "POSIX"
        } else {
            "C"
        };
        let sql = format!("ALTER TABLE redundant_demo ALTER COLUMN cname TYPE text COLLATE \"{collation}\"");
        if let Err(err) = sqlx::query(&sql).execute(&mut *conn).await {
            panic!("设置列 collation 失败: {err}\nSQL: {sql}");
        }

        for sql in [
            format!("DROP ROLE IF EXISTS {role_createdb}"),
            format!("DROP ROLE IF EXISTS {role_passwordless}"),
            format!("CREATE ROLE {role_createdb} LOGIN PASSWORD 'insp_pw' CREATEDB"),
            format!("CREATE ROLE {role_passwordless} LOGIN"),
        ] {
            if let Err(err) = sqlx::query(&sql).execute(&mut *conn).await {
                panic!("执行角色 SQL 失败: {err}\nSQL: {sql}");
            }
        }
        drop(conn);

        Fixture {
            client_url: with_search_path(url, &schema),
            db_name: database_name(url),
            schema,
            role_createdb,
            role_passwordless,
            admin,
        }
    }

    async fn cleanup(self) {
        let mut conn = self.admin.acquire().await.expect("获取清理连接失败");
        for sql in [
            format!("DROP SCHEMA IF EXISTS {} CASCADE", self.schema),
            format!("DROP ROLE IF EXISTS {}", self.role_createdb),
            format!("DROP ROLE IF EXISTS {}", self.role_passwordless),
        ] {
            if let Err(err) = sqlx::query(&sql).execute(&mut *conn).await {
                eprintln!("清理失败: {err}\nSQL: {sql}");
                panic!("清理失败: {err}\nSQL: {sql}");
            }
        }
    }
}

fn check<'a>(checks: &'a [CheckResult], id: &str) -> &'a CheckResult {
    checks
        .iter()
        .find(|item| item.check_id == id)
        .unwrap_or_else(|| panic!("缺少检查项 {id}"))
}

async fn run_fixture(fixture: &Fixture, check_ids: &[String]) -> core_lib::ops::inspection::types::InspectionSnapshot {
    let client = DbClient::new_default(&fixture.client_url)
        .await
        .expect("创建 PG 连接池失败");
    let ctx = OpsContext::new(
        client,
        "inspection-pg-e2e".into(),
        fixture.db_name.clone(),
        true,
    );
    let thresholds = InspectionThresholds {
        unused_index_min_uptime_hours: 0,
        unused_index_min_snapshots: 1,
        ..InspectionThresholds::default()
    };
    run_inspection(&ctx, check_ids, &thresholds, "manual", &[], &[])
        .await
        .expect("PG 巡检执行失败")
}

#[tokio::test]
async fn inspection_pg_covers_nine_checks() {
    let Some(url) = base_url() else {
        eprintln!("INSPECTION_PG_URL 未设置，跳过 PG 巡检验证");
        return;
    };
    let fixture = Fixture::setup(&url).await;
    let check_ids = all_check_ids();
    let snapshot = run_fixture(&fixture, &check_ids).await;

    assert_eq!(snapshot.checks.len(), check_ids.len());
    assert!(
        snapshot.summary().skipped_count >= 1,
        "inactive_account 必须是显式 skipped"
    );

    // 字段治理不在本轮 A；这里仅确认巡检结构完整。

    // 1) 冗余索引：前缀覆盖 + 表达式重复；不同表达式、不同谓词不误报。
    let redundant = check(&snapshot.checks, CHECK_REDUNDANT_INDEX);
    assert_ne!(redundant.status, CheckStatus::Skipped, "{:?}", redundant.skipped_reason);
    let redundant_keys: Vec<&str> = redundant.evidence.iter().map(|item| item.key.as_str()).collect();
    assert!(
        redundant_keys.contains(&"redundant_demo.idx_a"),
        "idx_a 应被判为 idx_ab 的前缀冗余，实际: {redundant_keys:?}"
    );
    assert!(
        redundant_keys.contains(&"redundant_demo.idx_lower_dup"),
        "同表达式索引应被判重复，实际: {redundant_keys:?}"
    );
    assert!(
        !redundant_keys.contains(&"redundant_demo.idx_upper"),
        "不同表达式不能判冗余: {redundant_keys:?}"
    );
    assert!(
        !redundant_keys.contains(&"redundant_demo.idx_partial"),
        "不同谓词不能判冗余: {redundant_keys:?}"
    );

    // 2) 未使用索引：真实 pg_stat_user_indexes 数据源。
    let unused = check(&snapshot.checks, CHECK_UNUSED_INDEX);
    assert_ne!(unused.status, CheckStatus::Skipped, "{:?}", unused.skipped_reason);
    let unused_keys: Vec<&str> = unused.evidence.iter().map(|item| item.key.as_str()).collect();
    assert!(
        unused_keys.contains(&"redundant_demo.idx_unused"),
        "idx_unused 应出现在未使用索引证据中，实际: {unused_keys:?}"
    );

    // 3) 主键缺失。
    let missing_pk = check(&snapshot.checks, CHECK_MISSING_PRIMARY_KEY);
    let pk_keys: Vec<&str> = missing_pk.evidence.iter().map(|item| item.key.as_str()).collect();
    assert!(pk_keys.contains(&"table:no_pk"), "no_pk 应报缺少主键: {pk_keys:?}");
    assert!(
        !pk_keys.contains(&"table:redundant_demo"),
        "有主键的表不能误报: {pk_keys:?}"
    );

    // 4) 容量：执行成功，但 DATA_FREE 碎片必须显式标注不适用。
    let capacity = check(&snapshot.checks, CHECK_TABLE_CAPACITY);
    assert_ne!(capacity.status, CheckStatus::Skipped);
    assert!(
        capacity.not_applicable.iter().any(|note| note.contains("DATA_FREE")),
        "容量检查必须说明 PG 无 DATA_FREE: {:?}",
        capacity.not_applicable
    );

    // 5) 字符集/排序规则：列级差异可判定，表级显式不适用。
    let charset = check(&snapshot.checks, CHECK_CHARSET_COLLATION);
    let charset_keys: Vec<&str> = charset.evidence.iter().map(|item| item.key.as_str()).collect();
    assert!(
        charset_keys.contains(&"column:redundant_demo.cname"),
        "cname 的列级 collation 差异应被发现: {charset_keys:?}"
    );
    assert!(
        charset.not_applicable.iter().any(|note| note.contains("表级")),
        "必须说明 PG 没有表级字符集/排序规则: {:?}",
        charset.not_applicable
    );

    // 6) 孤儿外键：NOT VALID 外键本身必须报告，且不执行扫描。
    let orphan = check(&snapshot.checks, CHECK_ORPHAN_FOREIGN_KEY);
    let orphan_keys: Vec<&str> = orphan.evidence.iter().map(|item| item.key.as_str()).collect();
    assert!(
        orphan_keys.contains(&"constraint:orphan_child.fk_orphan_child"),
        "NOT VALID 外键必须报告: {orphan_keys:?}"
    );
    let not_valid = orphan
        .evidence
        .iter()
        .find(|item| item.key == "constraint:orphan_child.fk_orphan_child")
        .unwrap();
    assert_eq!(not_valid.fields["validated"], false);
    assert!(not_valid.message.contains("NOT VALID"));

    // 7) 高权限账号：PG 语义（CREATEDB）。
    let over_priv = check(&snapshot.checks, CHECK_OVER_PRIVILEGED_ACCOUNT);
    let over_keys: Vec<&str> = over_priv.evidence.iter().map(|item| item.key.as_str()).collect();
    assert!(
        over_keys.contains(&fixture.role_createdb.as_str()),
        "CREATEDB 角色应被标记: {over_keys:?}"
    );

    // 8) 无密码账号：pg_authid 可读时应命中真实角色。
    let passwordless = check(&snapshot.checks, CHECK_PASSWORDLESS_ACCOUNT);
    assert_ne!(passwordless.status, CheckStatus::Skipped, "{:?}", passwordless.skipped_reason);
    let pw_keys: Vec<&str> = passwordless.evidence.iter().map(|item| item.key.as_str()).collect();
    assert!(
        pw_keys.contains(&fixture.role_passwordless.as_str()),
        "无密码登录角色应被标记: {pw_keys:?}"
    );

    // 9) inactive_account：PG 不适用，必须显式 skipped 且有可读原因。
    let inactive = check(&snapshot.checks, CHECK_INACTIVE_ACCOUNT);
    assert_eq!(inactive.status, CheckStatus::Skipped);
    let reason = inactive.skipped_reason.as_deref().unwrap_or("");
    assert!(
        reason.contains("没有") || reason.contains("无"),
        "skipped 原因必须可读: {reason}"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn inspection_pg_redundant_index_respects_expression_and_predicate() {
    let Some(url) = base_url() else {
        eprintln!("INSPECTION_PG_URL 未设置，跳过 PG 巡检验证");
        return;
    };
    let fixture = Fixture::setup(&url).await;
    // 补充：同列不同谓词的两个部分索引不能互相判冗余。
    {
        let mut conn = fixture.admin.acquire().await.expect("获取连接失败");
        for sql in [
            format!("SET search_path TO {}", fixture.schema),
            "CREATE INDEX idx_partial_same_a ON redundant_demo(a) WHERE c > 10".to_string(),
        ] {
            sqlx::query(&sql)
                .execute(&mut *conn)
                .await
                .unwrap_or_else(|err| panic!("准备部分索引失败: {err}\nSQL: {sql}"));
        }
    }
    let snapshot = run_fixture(&fixture, &[CHECK_REDUNDANT_INDEX.to_string()]).await;
    let redundant = check(&snapshot.checks, CHECK_REDUNDANT_INDEX);
    let keys: Vec<&str> = redundant.evidence.iter().map(|item| item.key.as_str()).collect();
    assert!(
        !keys.contains(&"redundant_demo.idx_partial_same_a"),
        "同列但谓词不同的部分索引不能判冗余: {keys:?}"
    );
    assert!(
        !keys.contains(&"redundant_demo.idx_partial"),
        "同列但谓词不同的部分索引不能判冗余: {keys:?}"
    );
    fixture.cleanup().await;
}

#[tokio::test]
async fn inspection_pg_skips_unsupported_accounts_without_false_negative() {
    let Some(url) = base_url() else {
        eprintln!("INSPECTION_PG_URL 未设置，跳过 PG 巡检验证");
        return;
    };
    let fixture = Fixture::setup(&url).await;
    let snapshot = run_fixture(&fixture, &all_check_ids()).await;

    // 所有 skipped 项都必须带可读原因，且不能出现“0 个对象/0 项问题”的假阴性。
    for item in snapshot.checks.iter().filter(|item| item.status == CheckStatus::Skipped) {
        let reason = item.skipped_reason.as_deref().unwrap_or("");
        assert!(
            reason.len() >= 8,
            "skipped 检查 {} 缺少可读原因",
            item.check_id
        );
    }
    for item in snapshot.checks.iter().filter(|item| item.status != CheckStatus::Skipped) {
        assert!(
            item.checked_objects > 0 || !item.not_applicable.is_empty(),
            "检查 {} 既没有检查对象也没有不适用说明，等于静默空结果",
            item.check_id
        );
    }
    fixture.cleanup().await;
}
