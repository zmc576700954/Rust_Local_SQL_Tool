//! 字段治理 S1 真实 MySQL 验证（opt-in）。
//!
//! 运行方式：
//! ```bash
//! FIELD_MIGRATION_MYSQL_URL='mysql://root:password@127.0.0.1:13309/e2e' \
//!   cargo test -p core_lib --test field_migration_mysql -- --nocapture
//! ```
//! 未设置环境变量时自动跳过，不影响常规单测。
//!
//! 覆盖：正常 JSON / 非法 JSON / NULL / 空串 / key 集合不一致 / 超长字符串 /
//! 唯一索引与普通索引引用 / 生成列依赖 / 整表盘点无脚本。

use std::sync::Arc;

use sqlx::mysql::MySqlPool;

use core_lib::db::client::DbClient;
use core_lib::ops::field_migration::types::{
    FieldGovernanceAnalysis, FieldGovernanceInput, RiskLevel,
};
use core_lib::ops::policy::OpsPolicy;
use core_lib::ops::registry::{OpsContext, ScenarioRegistry};

const DB_ID: &str = "field-migration-e2e";
const TABLE: &str = "fm_orders";

async fn exec(conn: &mut sqlx::pool::PoolConnection<sqlx::MySql>, sql: &str) {
    if let Err(err) = sqlx::query(sql).execute(&mut **conn).await {
        panic!("执行 SQL 失败: {err}\nSQL: {sql}");
    }
}

async fn setup(conn: &mut sqlx::pool::PoolConnection<sqlx::MySql>) {
    exec(conn, &format!("DROP TABLE IF EXISTS {TABLE}")).await;
    exec(conn, "DROP TABLE IF EXISTS fm_users").await;
    exec(conn, "CREATE TABLE fm_users (id INT NOT NULL PRIMARY KEY) ENGINE=InnoDB").await;
    exec(
        conn,
        &format!(
            "CREATE TABLE {TABLE} (\
               id INT NOT NULL AUTO_INCREMENT, \
               user_id INT NOT NULL, \
               payload JSON NULL, \
               payload_text VARCHAR(512) NULL, \
               payload_theme VARCHAR(64) GENERATED ALWAYS AS \
                 (JSON_UNQUOTE(JSON_EXTRACT(payload, '$.theme'))) STORED, \
               PRIMARY KEY (id), \
               UNIQUE KEY uk_payload_text (payload_text), \
               KEY idx_user (user_id), \
               CONSTRAINT fk_user FOREIGN KEY (user_id) REFERENCES fm_users (id)\
             ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4"
        ),
    )
    .await;
    exec(conn, "INSERT INTO fm_users (id) VALUES (1), (2), (3), (4), (5), (6)").await;
    // 正常 JSON、类型漂移、key 集合不一致、NULL、空对象。
    exec(
        conn,
        &format!(
            "INSERT INTO {TABLE} (user_id, payload, payload_text) VALUES \
             (1, '{{\"theme\":\"dark\",\"age\":30}}', '{{\"theme\":\"dark\",\"age\":30}}'), \
             (2, '{{\"theme\":\"light\",\"age\":\"unknown\"}}', '{{\"theme\":\"light\"}}'), \
             (3, '{{\"theme\":null}}', '{{broken json'), \
             (4, NULL, ''), \
             (5, '{{}}', '   padded value   '), \
             (6, NULL, NULL)"
        ),
    )
    .await;
    // 超长字符串：max_char_length 会超过映射目标 VARCHAR(20)。
    exec(
        conn,
        &format!(
            "UPDATE {TABLE} SET payload_text = REPEAT('x', 60) WHERE id = 2"
        ),
    )
    .await;
    exec(conn, &format!("ANALYZE TABLE {TABLE}")).await;
}

async fn analyze(
    ctx: &OpsContext,
    input: &FieldGovernanceInput,
) -> FieldGovernanceAnalysis {
    let registry = ScenarioRegistry::builtin();
    let scenario = registry.find("field-governance").expect("场景未注册");
    let input_value = serde_json::to_value(input).expect("序列化输入失败");
    let evidence = scenario
        .collect_evidence(ctx, &input_value)
        .await
        .expect("采集证据失败");
    let analysis: FieldGovernanceAnalysis =
        serde_json::from_value(evidence.extra.expect("缺少字段治理扩展证据"))
            .expect("解析字段治理证据失败");
    analysis
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
            "列 {} type={} nullable={} mode={} rows={} null={} empty={} max_chars={} invalid_json={} keys={:?} key_presence={:?}",
            column.column,
            column.column_type,
            column.nullable,
            column.profile_mode,
            column.rows_profiled,
            column.null_rows,
            column.empty_rows,
            column.max_char_length,
            column.json_invalid_rows,
            column.json_keys,
            column
                .json_key_presence
                .iter()
                .map(|p| (&p.key, p.rows))
                .collect::<Vec<_>>(),
        );
    }
    println!("依赖: indexes={:?}", analysis.dependencies.indexes);
    println!(
        "生成列: {:?} / 外键: {:?}",
        analysis.dependencies.generated_columns, analysis.dependencies.foreign_keys
    );
    println!(
        "风险汇总: block={} high={} info={} floor={} undetermined={}",
        analysis.risk_summary.block,
        analysis.risk_summary.high,
        analysis.risk_summary.info,
        analysis.risk_summary.risk_floor,
        analysis.risk_summary.undetermined_write_block
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
async fn field_governance_real_mysql_json_and_text_profiles() {
    let Ok(url) = std::env::var("FIELD_MIGRATION_MYSQL_URL") else {
        eprintln!("跳过字段治理真实库验证：未设置 FIELD_MIGRATION_MYSQL_URL");
        return;
    };
    let db_name = DbClient::extract_db_name(&url).unwrap_or_else(|| "e2e".to_string());
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

    // ── 1) JSON 列：正常 JSON / key 不一致 / 类型漂移 / NULL ─────────
    let json_analysis = analyze(
        &ctx,
        &FieldGovernanceInput {
            table: TABLE.into(),
            column: Some("payload".into()),
            profile_mode: Some("full".into()),
            declarations: vec![core_lib::ops::field_migration::types::SemanticDeclarationInput {
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
    assert_eq!(json_analysis.columns[0].json_invalid_rows, 0);
    assert!(json_analysis.columns[0].json_valid_rows >= 4);
    assert_eq!(json_analysis.columns[0].json_object_rows, 4);
    assert_eq!(json_analysis.columns[0].null_rows, 2);
    assert_risk(&json_analysis, "E08_NULL_PRESENT", RiskLevel::High);
    assert_risk(&json_analysis, "E05_GENERATED_COLUMN", RiskLevel::Block);
    assert_risk(
        &json_analysis,
        "E11_TYPE_DRIFT@payload:age",
        RiskLevel::Block,
    );
    assert_risk(&json_analysis, "SEM_DECL_UNEXPECTED_KEY", RiskLevel::Block);
    assert_risk(
        &json_analysis,
        "SEM_DECL_REQUIRED_KEY_MISSING",
        RiskLevel::Block,
    );
    assert!(
        !json_analysis.scripts.is_empty(),
        "JSON 列应生成 dry-run 脚本包"
    );
    for script in &json_analysis.scripts {
        assert!(!script.executable, "脚本绝不能标记为可执行");
        assert!(!script.phase.is_empty());
    }
    let retire = json_analysis
        .scripts
        .iter()
        .find(|script| script.kind == "retire")
        .expect("缺少下线脚本");
    assert!(retire.irreversible, "DROP COLUMN 必须标记不可逆");
    assert!(retire.sql.contains("DROP COLUMN"));

    // ── 2) 文本列：非法 JSON / 空串 / 超长 / 唯一索引 ─────────────
    let text_analysis = analyze(
        &ctx,
        &FieldGovernanceInput {
            table: TABLE.into(),
            column: Some("payload_text".into()),
            profile_mode: Some("full".into()),
            target: Some(core_lib::ops::field_migration::types::TargetSpec {
                table: Some("fm_orders_payload_text_ext".into()),
                mappings: vec![core_lib::ops::field_migration::types::ColumnMapping {
                    source_key: None,
                    target_column: "payload_text".into(),
                    target_type: Some("VARCHAR(20)".into()),
                    required: Some(true),
                }],
                ..Default::default()
            }),
            declarations: vec![core_lib::ops::field_migration::types::SemanticDeclarationInput {
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
    assert_risk(&text_analysis, "E09_INVALID_JSON", RiskLevel::Block);
    assert_risk(&text_analysis, "E08_EMPTY_STRING", RiskLevel::High);
    assert_risk(
        &text_analysis,
        "E01_TARGET_LENGTH_TOO_SMALL",
        RiskLevel::Block,
    );
    assert_risk(
        &text_analysis,
        "E03_PRIMARY_OR_UNIQUE@payload_text",
        RiskLevel::Block,
    );
    assert_risk(&text_analysis, "E13_TRIM_DIRTY", RiskLevel::High);
    assert_risk(&text_analysis, "SEM_DECL_NULL_CONFLICT", RiskLevel::Block);
    assert_eq!(text_analysis.risk_summary.risk_floor, "block");
    assert!(
        !text_analysis.risk_summary.undetermined_write_block,
        "full 模式且无缺口时不应标记不可判定"
    );

    // ── 3) sampled 模式：不可判定 → 写步骤阻断 ─────────────────
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
        sampled.profile_mode,
        sampled.row_count,
        sampled.risk_summary.undetermined_write_block
    );
    assert_eq!(sampled.profile_mode, "sampled");
    assert!(sampled.risk_summary.undetermined_write_block);
    assert_risk(&sampled, "PROFILE_NOT_FULL", RiskLevel::Block);

    let create = text_analysis
        .scripts
        .iter()
        .find(|script| script.kind == "create_table")
        .expect("缺少建表脚本");
    println!("\n=== 脚本片段 ===");
    println!("[create_table / {} / irreversible={}]\n{}", create.phase, create.irreversible, create.sql);
    for script in text_analysis
        .scripts
        .iter()
        .filter(|script| script.kind == "backfill" || script.kind == "verify")
    {
        println!(
            "\n[{} / {} / irreversible={}]\n{}",
            script.kind, script.phase, script.irreversible, script.sql
        );
    }
    assert!(create.sql.contains("CREATE TABLE"));
    assert!(create.sql.contains("fm_orders_payload_text_ext"));

    // ── 3) 整表盘点：不生成脚本 ─────────────────────────────────
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
    assert_eq!(inventory.analyzed_columns.len(), 5);
    assert!(inventory.scripts.is_empty(), "整表盘点不应生成脚本");
    assert_risk(&inventory, "E04_FOREIGN_KEY@user_id", RiskLevel::Block);

    // ── 4) 通过 build_plan 确认没有写动作 ───────────────────────
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
    assert!(plan
        .validation_checks
        .iter()
        .any(|check| check.id == "field-governance:no-write-actions"));

    exec(&mut conn, &format!("DROP TABLE IF EXISTS {TABLE}")).await;
    exec(&mut conn, "DROP TABLE IF EXISTS fm_users").await;
}
