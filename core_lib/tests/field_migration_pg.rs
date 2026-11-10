//! 字段治理 S1 的 PostgreSQL 真库验证（opt-in）。
//!
//! 运行方式：
//! ```bash
//! FIELD_MIGRATION_PG_URL='postgres://postgres:password@127.0.0.1:55432/postgres' \
//!   cargo test -p core_lib --test field_migration_pg -- --test-threads=1 --no-fail-fast
//! ```
//! 未设置环境变量时打印原因并返回，不影响常规单测。
//!
//! 覆盖：E01-E14 PG 取数、只读门拒绝写语句、numeric/bytea/timestamptz 类型格式化、
//! 分区表/复合主键/非确定性排序规则 BLOCK、pg_authid 权限降级不出假阴性。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use sqlx::postgres::PgPool;
use sqlx::Executor;

use core_lib::config::DbType;
use core_lib::db::client::DbClient;
use core_lib::ops::field_migration::read_only::validate_read_only_sql;
use core_lib::ops::field_migration::types::{
    ColumnMapping, FieldGovernanceAnalysis, FieldGovernanceInput, RiskLevel,
    SemanticDeclarationInput, TargetSpec,
};
use core_lib::ops::policy::OpsPolicy;
use core_lib::ops::registry::{OpsContext, ScenarioRegistry};

const DB_ID: &str = "field-migration-pg-e2e";
const TABLE: &str = "fm_orders";

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

fn with_database(url: &str, db: &str) -> String {
    match url.rsplit_once('/') {
        Some((base, tail)) => {
            let query = tail.find('?').map(|i| &tail[i..]).unwrap_or("");
            format!("{base}/{db}{query}")
        }
        None => format!("{url}/{db}"),
    }
}

fn with_credentials(url: &str, user: &str, password: &str) -> String {
    if let Some((scheme, rest)) = url.split_once("://") {
        let host = rest.split_once('@').map(|(_, host)| host).unwrap_or(rest);
        format!("{scheme}://{user}:{password}@{host}")
    } else {
        url.to_string()
    }
}

async fn admin_exec(pool: &PgPool, sql: &str) {
    if let Err(err) = pool.execute(sql).await {
        panic!("执行管理 SQL 失败: {err}\nSQL: {sql}");
    }
}

async fn create_database(pool: &PgPool, db: &str) {
    admin_exec(pool, &format!("DROP DATABASE IF EXISTS \"{db}\" WITH (FORCE)")).await;
    admin_exec(pool, &format!("CREATE DATABASE \"{db}\"")).await;
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
        eprintln!("清理失败: 数据库 \"{db}\": {err}\nSQL: {sql}");
        panic!("清理失败: 数据库 \"{db}\": {err}");
    }
}

async fn isolated_database(base_url: &str, prefix: &str) -> (String, PgPool) {
    let admin_pool = PgPool::connect(base_url)
        .await
        .expect("连接 PostgreSQL 管理库失败，请确认 phase3-pg 已就绪");
    let db = unique_db(prefix);
    create_database(&admin_pool, &db).await;
    (db, admin_pool)
}

async fn exec(pool: &PgPool, sql: &str) {
    if let Err(err) = pool.execute(sql).await {
        panic!("执行 SQL 失败: {err}\nSQL: {sql}");
    }
}

async fn setup(pool: &PgPool) {
    exec(pool, "CREATE TABLE fm_users (id bigint PRIMARY KEY NOT NULL)").await;
    exec(
        pool,
        "CREATE TABLE fm_orders (\
           id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, \
           user_id bigint NOT NULL, \
           payload jsonb NULL, \
           payload_text text NULL, \
           score numeric(12,4) NULL, \
           blob_col bytea NULL, \
           created_at timestamptz NULL, \
           payload_theme text GENERATED ALWAYS AS (payload ->> 'theme') STORED, \
           CONSTRAINT uk_payload_text UNIQUE (payload_text)\
         )",
    )
    .await;
    exec(pool, "CREATE INDEX idx_user ON fm_orders (user_id)").await;
    exec(
        pool,
        "ALTER TABLE fm_orders ADD CONSTRAINT fk_user \
         FOREIGN KEY (user_id) REFERENCES fm_users (id)",
    )
    .await;
    exec(pool, "INSERT INTO fm_users (id) SELECT generate_series(1, 6)").await;
    exec(
        pool,
        "INSERT INTO fm_orders (user_id, payload, payload_text, score, blob_col, created_at) VALUES \
         (1, '{\"theme\":\"dark\",\"age\":30}', '{\"theme\":\"dark\",\"age\":30}', 12.3400, '\\xdeadbeef', '2024-01-01 00:00:00+00'), \
         (2, '{\"theme\":\"light\",\"age\":\"unknown\"}', '{\"theme\":\"light\"}', NULL, '\\x00', '2024-02-01 00:00:00+00'), \
         (3, '{\"theme\":null}', '{broken json', 3.1400, NULL, NULL), \
         (4, NULL, '', NULL, NULL, NULL), \
         (5, '{}', '   padded value   ', 0.0000, '\\x01', '2024-03-01 00:00:00+08'), \
         (6, NULL, NULL, NULL, NULL, NULL)",
    )
    .await;
    exec(
        pool,
        "UPDATE fm_orders SET payload_text = repeat('x', 60) WHERE id = 2",
    )
    .await;
    exec(pool, "CREATE VIEW fm_orders_v AS SELECT id, payload FROM fm_orders").await;
    exec(pool, "ANALYZE fm_orders").await;

    exec(
        pool,
        "CREATE TABLE fm_partitioned (id bigint, payload jsonb, created_at date NOT NULL) \
         PARTITION BY RANGE (created_at)",
    )
    .await;
    exec(
        pool,
        "CREATE TABLE fm_partitioned_2024 PARTITION OF fm_partitioned \
         FOR VALUES FROM ('2024-01-01') TO ('2025-01-01')",
    )
    .await;
    exec(
        pool,
        "CREATE TABLE fm_composite (a bigint NOT NULL, b bigint NOT NULL, payload jsonb, PRIMARY KEY (a, b))",
    )
    .await;
    exec(
        pool,
        "CREATE COLLATION fm_nd (provider = icu, locale = 'und-u-ks-level2', deterministic = false)",
    )
    .await;
    exec(
        pool,
        "CREATE TABLE fm_collated (id bigint PRIMARY KEY, payload jsonb, label text COLLATE fm_nd)",
    )
    .await;
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

fn risk_level(analysis: &FieldGovernanceAnalysis, prefix: &str) -> Option<RiskLevel> {
    analysis
        .risks
        .iter()
        .find(|risk| risk.code.starts_with(prefix))
        .map(|risk| risk.level)
}

fn assert_risk(analysis: &FieldGovernanceAnalysis, prefix: &str, level: RiskLevel) {
    let found = risk_level(analysis, prefix);
    assert_eq!(
        found,
        Some(level),
        "缺少预期风险 {prefix}（实际风险: {:?}）",
        analysis
            .risks
            .iter()
            .map(|risk| (&risk.code, risk.level))
            .collect::<Vec<_>>()
    );
}

fn not_applicable_items(item: &serde_json::Value) -> Vec<String> {
    item["details"]["not_applicable"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|entry| entry["item"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn evidence_item(analysis: &FieldGovernanceAnalysis, id: &str) -> serde_json::Value {
    // 通过序列化拿 details，避免测试依赖内部结构。
    let value = serde_json::to_value(analysis).expect("序列化分析失败");
    let items = value["evidence"].as_array().expect("evidence 不是数组");
    items
        .iter()
        .find(|item| item["id"] == id)
        .unwrap_or_else(|| panic!("缺少证据项 {id}"))
        .clone()
}

fn print_evidence(analysis: &FieldGovernanceAnalysis) {
    println!("\n=== 真实采集：{}.{} ===", analysis.db_name, analysis.table);
    for item in &analysis.evidence {
        println!(
            "{} {} status={} mode={} :: {}",
            item.id, item.title, item.status, item.mode, item.summary
        );
    }
    for column in &analysis.columns {
        println!(
            "列 {} type={} nullable={} mode={} rows={} null={} empty={} max_chars={} invalid_json={} input_check={:?} keys={:?}",
            column.column,
            column.column_type,
            column.nullable,
            column.profile_mode,
            column.rows_profiled,
            column.null_rows,
            column.empty_rows,
            column.max_char_length,
            column.json_invalid_rows,
            column.pg_input_check,
            column.json_keys,
        );
    }
    println!("依赖: indexes={:?}", analysis.dependencies.indexes);
    println!(
        "生成列: {:?} / 外键: {:?} / 对象: {:?}",
        analysis.dependencies.generated_columns,
        analysis.dependencies.foreign_keys,
        analysis.dependencies.objects
    );
    println!(
        "环境: engine_family={} version_num={:?} kind={:?} partitioned={} nondeterministic={} pg_authid_readable={:?} no_password_roles={:?}",
        analysis.environment.engine_family,
        analysis.environment.server_version_num,
        analysis.environment.table_kind,
        analysis.environment.partitioned,
        analysis.environment.nondeterministic_collation,
        analysis.environment.pg_authid_readable,
        analysis.environment.no_password_roles,
    );
    for risk in &analysis.risks {
        println!(
            "  [{}] {} blocks_write={} :: {}",
            risk.code,
            risk.level.as_str(),
            risk.blocks_write,
            risk.detail
        );
    }
}

#[tokio::test]
async fn field_governance_real_pg_evidence_risk_and_scripts() {
    let Ok(base_url) = std::env::var("FIELD_MIGRATION_PG_URL") else {
        eprintln!("跳过字段治理真实 PG 验证：未设置 FIELD_MIGRATION_PG_URL");
        return;
    };
    let (db_name, admin_pool) = isolated_database(&base_url, "it_fm_pg").await;
    let url = with_database(&base_url, &db_name);
    let pool = PgPool::connect(&url).await.expect("连接 PostgreSQL 失败");
    setup(&pool).await;

    let policy = Arc::new(OpsPolicy {
        field_profile_max_rows: 10_000_000,
        field_profile_max_bytes: 20 * 1024 * 1024 * 1024,
        ..Default::default()
    });
    let client = DbClient::new_default(&url).await.expect("创建 DbClient 失败");
    let ctx = OpsContext::new(client, DB_ID.into(), db_name.clone(), false)
        .with_policy(policy.clone());

    // ── 1) jsonb 列：顶层类型 / 类型漂移 / 生成列 / 声明 ──────────
    let json_analysis = analyze(
        &ctx,
        &FieldGovernanceInput {
            table: TABLE.into(),
            column: Some("payload".into()),
            profile_mode: Some("full".into()),
            declarations: vec![SemanticDeclarationInput {
                purpose: Some("只存用户偏好".into()),
                expected_keys: vec!["theme".into()],
                required_keys: vec!["theme".into()],
                ..Default::default()
            }],
            ..Default::default()
        },
    )
    .await;
    print_evidence(&json_analysis);

    assert_eq!(json_analysis.profile_mode, "full");
    assert_eq!(json_analysis.row_count, 6);
    assert_eq!(json_analysis.environment.engine_family, "postgresql");
    assert_eq!(json_analysis.columns[0].json_invalid_rows, 0);
    assert_eq!(json_analysis.columns[0].json_object_rows, 4);
    assert_eq!(json_analysis.columns[0].null_rows, 2);
    assert_eq!(json_analysis.columns[0].pg_input_check.as_deref(), Some("not_applicable"));
    assert_risk(&json_analysis, "E08_NULL_PRESENT", RiskLevel::High);
    assert_risk(&json_analysis, "E05_GENERATED_COLUMN", RiskLevel::Block);
    assert_risk(&json_analysis, "E11_TYPE_DRIFT@payload:age", RiskLevel::Block);
    assert_risk(&json_analysis, "SEM_DECL_UNEXPECTED_KEY", RiskLevel::Block);
    assert_risk(&json_analysis, "SEM_DECL_REQUIRED_KEY_MISSING", RiskLevel::Block);
    assert!(!json_analysis.scripts.is_empty(), "jsonb 列应生成脚本包");
    for script in &json_analysis.scripts {
        assert!(!script.executable, "脚本绝不能标记为可执行");
    }
    assert!(
        json_analysis
            .scripts
            .iter()
            .any(|script| script.sql.contains("ON CONFLICT") && script.sql.contains("EXCLUDED")),
        "回填脚本必须使用 PG UPSERT 方言"
    );
    assert!(
        json_analysis
            .scripts
            .iter()
            .any(|script| script.sql.contains("jsonb_typeof")),
        "JSON 回填/校验必须使用 jsonb_typeof"
    );
    assert!(
        json_analysis
            .scripts
            .iter()
            .any(|script| script.sql.contains("->>")),
        "JSON 标量必须使用 ->>"
    );
    let retire = json_analysis
        .scripts
        .iter()
        .find(|script| script.kind == "retire")
        .expect("缺少下线脚本");
    assert!(retire.irreversible);
    assert!(retire.sql.contains("DROP COLUMN"));

    // E02/E07/E14 的「PG 不适用」必须显式写明，不能用近似指标顶替。
    let e02 = evidence_item(&json_analysis, "E02");
    let e02_na = not_applicable_items(&e02);
    assert!(e02_na.iter().any(|item| item == "TABLE_COLLATION"));
    assert!(e02_na.iter().any(|item| item == "ENGINE"));
    let e07 = evidence_item(&json_analysis, "E07");
    assert!(not_applicable_items(&e07).iter().any(|item| item == "UPDATE_TIME"));
    let e14 = evidence_item(&json_analysis, "E14");
    assert!(not_applicable_items(&e14).iter().any(|item| item == "INNODB_TRX"));
    assert_eq!(e14["details"]["pg_authid_readable"], true);
    assert!(e14["details"]["no_password_roles"].is_number());

    // ── 2) text 列：非法 JSON / 空串 / 长度 / 唯一索引 / 类型预检 ──
    let text_analysis = analyze(
        &ctx,
        &FieldGovernanceInput {
            table: TABLE.into(),
            column: Some("payload_text".into()),
            profile_mode: Some("full".into()),
            target: Some(TargetSpec {
                table: Some("fm_orders_payload_text_ext".into()),
                mappings: vec![ColumnMapping {
                    source_key: None,
                    target_column: "payload_text".into(),
                    target_type: Some("character varying(20)".into()),
                    required: Some(true),
                }],
                ..Default::default()
            }),
            declarations: vec![SemanticDeclarationInput {
                target_nullable: Some(false),
                ..Default::default()
            }],
            ..Default::default()
        },
    )
    .await;
    print_evidence(&text_analysis);

    assert_eq!(text_analysis.columns[0].json_invalid_rows, 3);
    assert_eq!(text_analysis.columns[0].empty_rows, 1);
    assert_eq!(text_analysis.columns[0].null_rows, 1);
    assert!(text_analysis.columns[0].max_char_length > 20);
    assert_eq!(text_analysis.columns[0].untrimmed_rows, 1);
    let version = text_analysis.environment.server_version_num.unwrap_or(0);
    if version >= 160_000 {
        assert_eq!(
            text_analysis.columns[0].pg_input_check.as_deref(),
            Some("pg_input_is_valid"),
            "PG16+ 必须使用服务端 pg_input_is_valid"
        );
    } else {
        assert_eq!(
            text_analysis.columns[0].pg_input_check.as_deref(),
            Some("client_sample"),
            "PG<16 必须降级为客户端采样"
        );
        assert!(
            text_analysis.gaps.iter().any(|gap| gap.evidence_id == "E09"),
            "PG<16 必须记录 E09 gap"
        );
    }
    assert_risk(&text_analysis, "E09_INVALID_JSON", RiskLevel::Block);
    assert_risk(&text_analysis, "E08_EMPTY_STRING", RiskLevel::High);
    assert_risk(&text_analysis, "E01_TARGET_LENGTH_TOO_SMALL", RiskLevel::Block);
    assert_risk(&text_analysis, "E03_PRIMARY_OR_UNIQUE@payload_text", RiskLevel::Block);
    assert_risk(&text_analysis, "E13_TRIM_DIRTY", RiskLevel::High);
    assert_risk(&text_analysis, "SEM_DECL_NULL_CONFLICT", RiskLevel::Block);
    assert_risk(&text_analysis, "E01_PG_TYPE_REWRITE", RiskLevel::Block);
    // 依据：pg_input_is_valid 会按目标类型的 typmod 校验，character varying(20) 下
    // 第 1 行（24 字符的合法 JSON）与第 2 行（60 字符）共 2 行不可转换，必须报 BLOCK。
    assert_eq!(text_analysis.columns[0].pg_cast_invalid_rows, Some(2));
    assert_risk(&text_analysis, "E01_PG_USING_INVALID", RiskLevel::Block);
    // 依据：PG 无列级字符集，E13 只看数据库编码 + 列 collation，不能拿 MySQL 的
    // charset 缺失规则误报 BLOCK。
    assert!(
        !text_analysis
            .risks
            .iter()
            .any(|risk| risk.code.starts_with("E13_BINARY_CHARSET")),
        "PG 不应触发 MySQL 的列级字符集规则"
    );
    assert_eq!(text_analysis.risk_summary.risk_floor, "block");
    assert!(
        !text_analysis.risk_summary.undetermined_write_block,
        "full 模式且 PG16+ 不应标记不可判定"
    );

    // ── 3) sampled 模式：PROFILE_NOT_FULL ───────────────────────
    let sampled = analyze(
        &ctx,
        &FieldGovernanceInput {
            table: TABLE.into(),
            column: Some("payload".into()),
            profile_mode: Some("sampled".into()),
            sample_per_mille: Some(500),
            ..Default::default()
        },
    )
    .await;
    println!(
        "\n=== sampled 模式 === mode={} rows={} undetermined={}",
        sampled.profile_mode, sampled.row_count, sampled.risk_summary.undetermined_write_block
    );
    assert_eq!(sampled.profile_mode, "sampled");
    assert!(sampled.risk_summary.undetermined_write_block);
    assert_risk(&sampled, "PROFILE_NOT_FULL", RiskLevel::Block);

    // ── 4) 标量类型：numeric / bytea / timestamptz 的方言脚本 ────
    let numeric_analysis = analyze(
        &ctx,
        &FieldGovernanceInput {
            table: TABLE.into(),
            column: Some("score".into()),
            profile_mode: Some("full".into()),
            ..Default::default()
        },
    )
    .await;
    print_evidence(&numeric_analysis);
    assert_eq!(numeric_analysis.columns[0].data_type, "numeric");
    assert!(!numeric_analysis
        .gaps
        .iter()
        .any(|gap| matches!(gap.evidence_id.as_str(), "E08" | "E12")));
    assert!(numeric_analysis
        .scripts
        .iter()
        .any(|script| script.sql.to_ascii_lowercase().contains("numeric")));

    let timestamptz_analysis = analyze(
        &ctx,
        &FieldGovernanceInput {
            table: TABLE.into(),
            column: Some("created_at".into()),
            profile_mode: Some("full".into()),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(timestamptz_analysis.columns[0].data_type, "timestamptz");
    assert!(!timestamptz_analysis
        .gaps
        .iter()
        .any(|gap| gap.evidence_id == "E08"));
    assert!(
        timestamptz_analysis
            .scripts
            .iter()
            .any(|script| script.sql.contains("AT TIME ZONE 'UTC'")),
        "timestamptz 校验必须做时区归一化"
    );

    let bytea_from_json = analyze(
        &ctx,
        &FieldGovernanceInput {
            table: TABLE.into(),
            column: Some("payload".into()),
            profile_mode: Some("full".into()),
            target: Some(TargetSpec {
                table: Some("fm_orders_theme_bytes".into()),
                mappings: vec![ColumnMapping {
                    source_key: Some("theme".into()),
                    target_column: "theme_bytes".into(),
                    target_type: Some("bytea".into()),
                    required: None,
                }],
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await;
    assert!(
        bytea_from_json
            .scripts
            .iter()
            .any(|script| script.sql.contains("encode(convert_to(")),
        "bytea 规范化必须使用 encode(convert_to(...), 'hex')"
    );

    // ── 5) 整表盘点：无脚本 + 外键阻断 ──────────────────────────
    let inventory = analyze(
        &ctx,
        &FieldGovernanceInput {
            table: TABLE.into(),
            profile_mode: Some("full".into()),
            ..Default::default()
        },
    )
    .await;
    println!("\n=== 整表盘点 === analyzed={:?}", inventory.analyzed_columns);
    assert_eq!(inventory.analyzed_columns.len(), 8);
    assert!(inventory.scripts.is_empty(), "整表盘点不应生成脚本");
    assert_risk(&inventory, "E04_FOREIGN_KEY@user_id", RiskLevel::Block);

    // ── 6) 只读门：拒绝写语句 ──────────────────────────────────
    for sql in [
        "SELECT COUNT(*) AS n FROM fm_orders",
        "SELECT jsonb_typeof(payload) AS t FROM fm_orders",
    ] {
        validate_read_only_sql(DbType::PostgreSQL, sql)
            .unwrap_or_else(|err| panic!("只读 SQL 应被接受: {sql}: {err}"));
    }
    for sql in [
        "INSERT INTO fm_orders_ext (id) SELECT id FROM fm_orders ON CONFLICT (id) DO UPDATE SET id = EXCLUDED.id",
        "UPDATE fm_orders SET payload = '{}'::jsonb",
        "DELETE FROM fm_orders WHERE id = 1",
        "ALTER TABLE fm_orders ALTER COLUMN payload TYPE text USING payload::text",
        "CREATE TABLE fm_orders_ext (id bigint)",
        "TRUNCATE TABLE fm_orders",
    ] {
        assert!(
            validate_read_only_sql(DbType::PostgreSQL, sql).is_err(),
            "写语句必须被拒绝: {sql}"
        );
    }

    // ── 7) BLOCK：分区表 / 复合主键 / 非确定性排序规则 ───────────
    let partitioned = analyze(
        &ctx,
        &FieldGovernanceInput {
            table: "fm_partitioned".into(),
            column: Some("payload".into()),
            profile_mode: Some("full".into()),
            ..Default::default()
        },
    )
    .await;
    print_evidence(&partitioned);
    assert!(partitioned.environment.partitioned);
    assert_risk(&partitioned, "E02_PARTITIONED", RiskLevel::Block);

    let composite = analyze(
        &ctx,
        &FieldGovernanceInput {
            table: "fm_composite".into(),
            column: Some("payload".into()),
            profile_mode: Some("full".into()),
            ..Default::default()
        },
    )
    .await;
    assert_risk(&composite, "E14_NO_SINGLE_PK", RiskLevel::Block);

    let collated = analyze(
        &ctx,
        &FieldGovernanceInput {
            table: "fm_collated".into(),
            column: Some("payload".into()),
            profile_mode: Some("full".into()),
            ..Default::default()
        },
    )
    .await;
    print_evidence(&collated);
    assert!(collated.environment.nondeterministic_collation);
    assert_risk(&collated, "E13_PG_NONDETERMINISTIC_COLLATION", RiskLevel::Block);

    // ── 8) build_plan：无写动作 ────────────────────────────────
    let registry = ScenarioRegistry::builtin();
    let scenario = registry.find("field-governance").expect("场景未注册");
    let input_value = serde_json::to_value(FieldGovernanceInput {
        table: TABLE.into(),
        column: Some("payload".into()),
        profile_mode: Some("full".into()),
        ..Default::default()
    })
    .unwrap();
    let evidence = scenario
        .collect_evidence(&ctx, &input_value)
        .await
        .expect("采集证据失败");
    let identity = core_lib::ops::registry::PlanIdentity {
        db_id: DB_ID.into(),
        db_name: db_name.clone(),
        db_fingerprint: ctx.db_fingerprint.clone(),
        is_read_only: false,
    };
    let plan = scenario
        .build_plan(&identity, &evidence, &input_value, &policy)
        .expect("生成计划失败");
    assert!(plan.actions.is_empty(), "字段治理不得产生任何写动作");
    assert!(!plan.requires_approval);

    // ── 9) pg_authid 权限降级：非超级用户不得返回 0 假阴性 ───────
    let role = format!("fm_reader_{}", unique_suffix().replace('.', "_"));
    admin_exec(
        &admin_pool,
        &format!("CREATE ROLE \"{role}\" LOGIN PASSWORD 'reader_pw' NOSUPERUSER"),
    )
    .await;
    admin_exec(&admin_pool, &format!("GRANT CONNECT ON DATABASE \"{db_name}\" TO \"{role}\"")).await;
    exec(&pool, &format!("GRANT USAGE ON SCHEMA public TO \"{role}\"")).await;
    exec(&pool, &format!("GRANT SELECT ON ALL TABLES IN SCHEMA public TO \"{role}\"")).await;
    let reader_url = with_credentials(&base_url, &role, "reader_pw");
    let reader_url = with_database(&reader_url, &db_name);
    let reader_client = DbClient::new_default(&reader_url)
        .await
        .expect("创建只读角色 DbClient 失败");
    let reader_ctx = OpsContext::new(reader_client, DB_ID.into(), db_name.clone(), true)
        .with_policy(policy.clone());
    let reader_analysis = analyze(
        &reader_ctx,
        &FieldGovernanceInput {
            table: TABLE.into(),
            column: Some("payload".into()),
            profile_mode: Some("full".into()),
            ..Default::default()
        },
    )
    .await;
    print_evidence(&reader_analysis);
    assert_eq!(reader_analysis.environment.pg_authid_readable, Some(false));
    assert!(
        reader_analysis.environment.no_password_roles.is_none(),
        "pg_authid 不可读时不得返回 0"
    );
    assert!(
        reader_analysis
            .gaps
            .iter()
            .any(|gap| gap.evidence_id == "E14" && gap.reason.contains("pg_authid")),
        "pg_authid 不可读必须记录可读原因"
    );
    let env_json = serde_json::to_value(&reader_analysis.environment).unwrap();
    assert!(
        env_json.get("no_password_roles").is_none(),
        "不可读时 no_password_roles 必须缺省，不能序列化成 0"
    );
    drop(reader_ctx);

    drop(ctx);
    pool.close().await;
    cleanup_database(&admin_pool, &db_name).await;
    admin_exec(&admin_pool, &format!("DROP ROLE IF EXISTS \"{role}\"")).await;
    admin_pool.close().await;
}
