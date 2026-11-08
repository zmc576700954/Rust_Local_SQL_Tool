//! 字段治理 V1-V4 校验器真实规模验收（opt-in）。
//!
//! 目的：在 10 万行真实 MySQL 上验证 S1 生成的 V1-V4 校验 SQL
//! 能可靠跑通、能定位差异主键，并观察耗时与分批表现。
//!
//! 运行：
//! ```bash
//! FIELD_MIGRATION_MYSQL_URL='mysql://root:password@127.0.0.1:13309/e2e' \
//!   cargo test -p core_lib --test field_migration_verify_mysql -- --nocapture
//! ```
//! 未设置环境变量时自动跳过。

use std::sync::Arc;
use std::time::{Duration, Instant};

use sqlx::mysql::MySqlPool;
use sqlx::{MySqlConnection, Row};

use core_lib::db::client::DbClient;
use core_lib::ops::field_migration::diff::{fetch_page, DiffPageRequest};
use core_lib::ops::field_migration::types::{
    ColumnMapping, EmptyStringPolicy, FieldGovernanceAnalysis, FieldGovernanceInput, TargetSpec,
};
use core_lib::ops::policy::OpsPolicy;
use core_lib::ops::registry::{OpsContext, ScenarioRegistry};
use core_lib::sql::util::escape_sql_string;

const DB_ID: &str = "field-migration-verify-e2e";
const SOURCE_TABLE: &str = "fm_big";
const PAYLOAD_TABLE: &str = "fm_big_payload_ext";
const TEXT_TABLE: &str = "fm_big_text_ext";
const TOTAL_ROWS: i64 = 100_000;
const BATCH_SIZE: i64 = 10_000;

async fn exec(conn: &mut MySqlConnection, sql: &str) {
    if let Err(err) = sqlx::query(sql).execute(&mut *conn).await {
        panic!("执行 SQL 失败: {err}\nSQL: {sql}");
    }
}

async fn query_pks(conn: &mut MySqlConnection, sql: &str) -> Vec<i64> {
    let rows = sqlx::query(sql)
        .fetch_all(&mut *conn)
        .await
        .unwrap_or_else(|err| panic!("查询失败: {err}\nSQL: {sql}"));
    rows.iter()
        .map(|row| row.try_get::<i64, _>(0usize).expect("主键列不是 i64"))
        .collect()
}

fn payload_for(id: i64) -> Option<String> {
    let value = match id {
        1..=60_000 => Some(format!(
            r#"{{"a":"v{id}","count":1,"meta":{{"k":"v"}},"s":"plain{id}"}}"#
        )),
        60_001..=70_000 => Some(format!(r#"{{"a":"v{id}"}}"#)),
        70_001..=80_000 => Some(format!(
            r#"{{"a":"v{id}","count":1,"meta":{{"k":"v"}},"s":"plain{id}","extra":"x"}}"#
        )),
        80_001..=85_000 => None,
        85_001..=90_000 => Some("{}".to_string()),
        _ => Some(format!(
            r#"{{"a":"v{id}","count":1,"meta":{{"k":"v"}},"s":"plain{id}"}}"#
        )),
    };
    value
}

fn raw_text_for(id: i64) -> Option<String> {
    let value = match id {
        1..=60_000 => Some(r#"{"a":"v"}"#.to_string()),
        60_001..=62_000 => Some("{broken json".to_string()),
        62_001..=64_000 => None,
        64_001..=65_000 => Some(String::new()),
        65_001..=67_000 => Some("x".repeat(600)),
        67_001..=68_500 => Some("quotes \" backslash \\ emoji 😀 中文".to_string()),
        68_501..=70_000 => Some("   padded   ".to_string()),
        _ => Some(format!("plain raw {id}")),
    };
    value
}

async fn seed(conn: &mut MySqlConnection, schema: &str) {
    exec(conn, &format!("DROP TABLE IF EXISTS {schema}.{SOURCE_TABLE}")).await;
    exec(conn, &format!("DROP TABLE IF EXISTS {schema}.{PAYLOAD_TABLE}")).await;
    exec(conn, &format!("DROP TABLE IF EXISTS {schema}.{TEXT_TABLE}")).await;
    exec(
        conn,
        &format!(
            "CREATE TABLE {schema}.{SOURCE_TABLE} (\
               id BIGINT NOT NULL, \
               payload JSON NULL, \
               raw_text VARCHAR(1500) NULL, \
               PRIMARY KEY (id)\
             ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4"
        ),
    )
    .await;

    let mut rows = Vec::with_capacity(TOTAL_ROWS as usize);
    for id in 1..=TOTAL_ROWS {
        let payload = payload_for(id)
            .map(|value| format!("'{}'", escape_sql_string(&value)))
            .unwrap_or_else(|| "NULL".to_string());
        let raw = raw_text_for(id)
            .map(|value| format!("'{}'", escape_sql_string(&value)))
            .unwrap_or_else(|| "NULL".to_string());
        rows.push(format!("({id}, {payload}, {raw})"));
    }
    for (index, batch) in rows.chunks(500).enumerate() {
        let sql = format!(
            "INSERT INTO {schema}.{SOURCE_TABLE} (id, payload, raw_text) VALUES {}",
            batch.join(", ")
        );
        if index % 20 == 0 {
            println!("  insert batch {}/{}", index + 1, rows.len().div_ceil(500));
        }
        exec(conn, &sql).await;
    }
    exec(conn, &format!("ANALYZE TABLE {schema}.{SOURCE_TABLE}")).await;
}

async fn analyze(ctx: &OpsContext, input: &FieldGovernanceInput) -> FieldGovernanceAnalysis {
    let registry = ScenarioRegistry::builtin();
    let scenario = registry.find("field-governance").expect("场景未注册");
    let input_value = serde_json::to_value(input).expect("序列化输入失败");
    let evidence = scenario
        .collect_evidence(ctx, &input_value)
        .await
        .expect("采集证据失败");
    serde_json::from_value(evidence.extra.expect("缺少扩展证据")).expect("解析字段治理结果失败")
}

fn script<'a>(analysis: &'a FieldGovernanceAnalysis, kind: &str) -> &'a str {
    &analysis
        .scripts
        .iter()
        .find(|script| script.kind == kind)
        .unwrap_or_else(|| panic!("缺少脚本: {kind}"))
        .sql
}

#[derive(Default)]
struct VerifySections {
    v1: String,
    v2_count: String,
    v2_page: String,
    v3_count: String,
    v3_page: String,
    v4_count: String,
    v4_page: String,
}

/// 从生成的 verify 脚本中切出 V1、V2/V3/V4 的 count 与 page 语句。
fn split_verify(sql: &str) -> VerifySections {
    let markers: [(&str, fn(&mut VerifySections) -> &mut String); 7] = [
        ("-- V2 count", |s| &mut s.v2_count),
        ("-- V3 count", |s| &mut s.v3_count),
        ("-- V4 count", |s| &mut s.v4_count),
        ("-- V2 page", |s| &mut s.v2_page),
        ("-- V3 page", |s| &mut s.v3_page),
        ("-- V4 page", |s| &mut s.v4_page),
        ("-- V1", |s| &mut s.v1),
    ];
    let mut sections = VerifySections::default();
    let mut current: Option<usize> = None;
    for line in sql.lines() {
        let trimmed = line.trim_start();
        if let Some(index) = markers
            .iter()
            .position(|(marker, _)| trimmed.starts_with(marker))
        {
            current = Some(index);
            continue;
        }
        if trimmed.starts_with("--") {
            continue;
        }
        if let Some(index) = current {
            let buffer = (markers[index].1)(&mut sections);
            buffer.push_str(line);
            buffer.push('\n');
        }
    }
    let finish = |body: String| {
        let body = body.trim().trim_end_matches(';').trim().to_string();
        format!("{body};")
    };
    VerifySections {
        v1: finish(sections.v1),
        v2_count: finish(sections.v2_count),
        v2_page: finish(sections.v2_page),
        v3_count: finish(sections.v3_count),
        v3_page: finish(sections.v3_page),
        v4_count: finish(sections.v4_count),
        v4_page: finish(sections.v4_page),
    }
}

fn next_range(last: i64, end: i64) -> Option<(i64, i64)> {
    if last >= end {
        None
    } else {
        Some((last + 1, (last + BATCH_SIZE).min(end)))
    }
}

async fn backfill(
    conn: &mut MySqlConnection,
    template: &str,
    label: &str,
) -> Vec<(i64, i64, Duration)> {
    let mut timings = Vec::new();
    let mut last = 0i64;
    loop {
        let Some((start, end)) = next_range(last, TOTAL_ROWS) else {
            break;
        };
        let sql = template
            .replace(":last_pk", &last.to_string())
            .replace(":end_pk", &end.to_string())
            .replace(":batch_size", &BATCH_SIZE.to_string());
        let started = Instant::now();
        exec(conn, &sql).await;
        let elapsed = started.elapsed();
        println!("  {label} 批次 [{start}, {end}] 耗时 {:?}", elapsed);
        timings.push((start, end, elapsed));
        last = end;
    }
    timings
}

async fn count_rows(conn: &mut MySqlConnection, sql: &str) -> i64 {
    let row = sqlx::query(sql)
        .fetch_one(&mut *conn)
        .await
        .unwrap_or_else(|err| panic!("COUNT 失败: {err}\nSQL: {sql}"));
    row.try_get::<i64, _>(0usize).expect("COUNT 不是 i64")
}

/// 按主键区间逐页取差异主键；返回总数、全部主键、每页条数与耗时。
async fn run_pages(
    conn: &mut MySqlConnection,
    count_sql: &str,
    page_sql: &str,
    page_size: i64,
) -> (i64, Vec<i64>, Vec<usize>, Duration) {
    let started = Instant::now();
    let total = count_rows(conn, count_sql).await;
    let mut last = 0i64;
    let mut all = Vec::new();
    let mut pages = Vec::new();
    loop {
        let sql = page_sql
            .replace(":last_pk", &last.to_string())
            .replace(":page_size", &page_size.to_string());
        let pks = query_pks(conn, &sql).await;
        if pks.is_empty() {
            break;
        }
        pages.push(pks.len());
        last = *pks.last().expect("分页主键为空");
        all.extend(pks);
        if all.len() as i64 >= total {
            break;
        }
    }
    let elapsed = started.elapsed();
    (total, all, pages, elapsed)
}

fn assert_clean(pks: &[i64], label: &str) {
    assert!(
        pks.is_empty(),
        "{label} 干净数据出现差异: {:?}",
        &pks[..pks.len().min(20)]
    );
}

fn assert_only(pks: &[i64], expected: i64, label: &str) {
    assert_eq!(
        pks,
        vec![expected],
        "{label} 期望只定位主键 {expected}，实际 {:?}",
        pks
    );
}

#[tokio::test]
async fn field_governance_verify_v1_v4_at_100k_scale() {
    let Ok(url) = std::env::var("FIELD_MIGRATION_MYSQL_URL") else {
        eprintln!("跳过 V1-V4 真实规模验收：未设置 FIELD_MIGRATION_MYSQL_URL");
        return;
    };
    let schema = DbClient::extract_db_name(&url).unwrap_or_else(|| "e2e".to_string());
    let pool = MySqlPool::connect(&url).await.expect("连接 MySQL 失败");
    let mut conn = pool.acquire().await.expect("获取连接失败");
    let conn: &mut MySqlConnection = &mut conn;

    let started = Instant::now();
    seed(conn, &schema).await;
    println!("数据构造完成，耗时 {:?}", started.elapsed());

    let distribution: Vec<(String, i64)> = sqlx::query_as::<_, (String, i64)>(
        "SELECT 'payload=valid' AS k, COUNT(*) FROM fm_big WHERE JSON_VALID(payload) = 1 AND JSON_LENGTH(payload) > 0 AND id BETWEEN 1 AND 60000 \
         UNION ALL SELECT 'payload=missing_key', COUNT(*) FROM fm_big WHERE id BETWEEN 60001 AND 70000 \
         UNION ALL SELECT 'payload=extra_key', COUNT(*) FROM fm_big WHERE id BETWEEN 70001 AND 80000 \
         UNION ALL SELECT 'payload=NULL', COUNT(*) FROM fm_big WHERE payload IS NULL \
         UNION ALL SELECT 'payload=empty_obj', COUNT(*) FROM fm_big WHERE JSON_LENGTH(payload) = 0 AND payload IS NOT NULL \
         UNION ALL SELECT 'raw=JSON_VALID=0（含普通文本）', COUNT(*) FROM fm_big WHERE JSON_VALID(raw_text) = 0 AND raw_text IS NOT NULL AND raw_text <> '' \
         UNION ALL SELECT 'raw=NULL', COUNT(*) FROM fm_big WHERE raw_text IS NULL \
         UNION ALL SELECT 'raw=empty', COUNT(*) FROM fm_big WHERE raw_text = '' \
         UNION ALL SELECT 'raw=long600', COUNT(*) FROM fm_big WHERE CHAR_LENGTH(raw_text) = 600 \
         UNION ALL SELECT 'raw=padded', COUNT(*) FROM fm_big WHERE raw_text LIKE ' %' \
         UNION ALL SELECT 'total', COUNT(*) FROM fm_big",
    )
    .fetch_all(&mut *conn)
    .await
    .expect("统计分布失败");
    println!("数据分布:");
    for (label, count) in &distribution {
        println!("  {label}: {count}");
    }

    let policy = Arc::new(OpsPolicy {
        field_profile_max_rows: 10_000_000,
        field_profile_max_bytes: 20 * 1024 * 1024 * 1024,
        ..Default::default()
    });
    let client = DbClient::new_default(&url).await.expect("创建 DbClient 失败");
    let ctx = OpsContext::new(client, DB_ID.into(), schema.clone(), false).with_policy(policy);

    // ── 生成并执行 S1 脚本：payload（JSON key 映射）────────────
    let payload_analysis = analyze(
        &ctx,
        &FieldGovernanceInput {
            table: SOURCE_TABLE.into(),
            column: Some("payload".into()),
            profile_mode: Some("full".into()),
            target: Some(TargetSpec {
                table: Some(PAYLOAD_TABLE.into()),
                mappings: vec![
                    ColumnMapping {
                        source_key: Some("a".into()),
                        target_column: "a".into(),
                        target_type: Some("VARCHAR(512)".into()),
                        required: None,
                    },
                    ColumnMapping {
                        source_key: Some("count".into()),
                        target_column: "count".into(),
                        target_type: Some("DECIMAL(20,6)".into()),
                        required: None,
                    },
                    ColumnMapping {
                        source_key: Some("meta".into()),
                        target_column: "meta".into(),
                        target_type: Some("JSON".into()),
                        required: None,
                    },
                    ColumnMapping {
                        source_key: Some("s".into()),
                        target_column: "s".into(),
                        target_type: Some("VARCHAR(1024)".into()),
                        required: None,
                    },
                ],
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await;
    println!(
        "payload 分析：{} 行，{} 条风险，{} 个脚本",
        payload_analysis.row_count,
        payload_analysis.risks.len(),
        payload_analysis.scripts.len()
    );
    let payload_create = script(&payload_analysis, "create_table");
    let payload_backfill_tpl = script(&payload_analysis, "backfill");
    let payload_verify = split_verify(script(&payload_analysis, "verify"));
    exec(conn, payload_create).await;
    let payload_backfill_timings = backfill(conn, payload_backfill_tpl, "payload").await;
    println!(
        "payload 回填总耗时 {:?}（{} 批）",
        payload_backfill_timings
            .iter()
            .fold(Duration::ZERO, |acc, (_, _, d)| acc + *d),
        payload_backfill_timings.len()
    );

    // ── 生成并执行 S1 脚本：raw_text（整列拷贝）──────────────
    let text_analysis = analyze(
        &ctx,
        &FieldGovernanceInput {
            table: SOURCE_TABLE.into(),
            column: Some("raw_text".into()),
            profile_mode: Some("full".into()),
            target: Some(TargetSpec {
                table: Some(TEXT_TABLE.into()),
                mappings: vec![ColumnMapping {
                    source_key: None,
                    target_column: "raw_text".into(),
                    target_type: Some("VARCHAR(1024)".into()),
                    required: None,
                }],
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await;
    let text_create = script(&text_analysis, "create_table");
    let text_backfill_tpl = script(&text_analysis, "backfill");
    let text_verify = split_verify(script(&text_analysis, "verify"));
    exec(conn, text_create).await;
    let text_backfill_timings = backfill(conn, text_backfill_tpl, "raw_text").await;
    println!(
        "raw_text 回填总耗时 {:?}（{} 批）",
        text_backfill_timings
            .iter()
            .fold(Duration::ZERO, |acc, (_, _, d)| acc + *d),
        text_backfill_timings.len()
    );

    // ── 干净数据：V1-V4（分页）────────────────────────────
    println!("\n=== 干净数据 V1-V4（分页）===");
    let (source_rows, target_rows) = sqlx::query_as::<_, (i64, i64)>(&payload_verify.v1)
        .fetch_one(&mut *conn)
        .await
        .expect("V1 失败");
    println!("V1 payload: source={source_rows} target={target_rows}");
    assert_eq!((source_rows, target_rows), (TOTAL_ROWS, TOTAL_ROWS));

    let (source_rows, target_rows) = sqlx::query_as::<_, (i64, i64)>(&text_verify.v1)
        .fetch_one(&mut *conn)
        .await
        .expect("V1 失败");
    println!("V1 raw_text: source={source_rows} target={target_rows}");
    assert_eq!((source_rows, target_rows), (TOTAL_ROWS, TOTAL_ROWS));

    // 空串 1000 行、SQL NULL 2000 行必须在目标表各自保留。
    let source_empty = count_rows(conn, &format!(
        "SELECT COUNT(*) FROM {schema}.{SOURCE_TABLE} WHERE raw_text = ''"
    ))
    .await;
    let source_null = count_rows(conn, &format!(
        "SELECT COUNT(*) FROM {schema}.{SOURCE_TABLE} WHERE raw_text IS NULL"
    ))
    .await;
    let target_empty = count_rows(conn, &format!(
        "SELECT COUNT(*) FROM {schema}.{TEXT_TABLE} WHERE raw_text = ''"
    ))
    .await;
    let target_null = count_rows(conn, &format!(
        "SELECT COUNT(*) FROM {schema}.{TEXT_TABLE} WHERE raw_text IS NULL"
    ))
    .await;
    println!(
        "空串/NULL 保留：source empty={source_empty} null={source_null}；target empty={target_empty} null={target_null}"
    );
    assert_eq!((source_empty, source_null), (1000, 2000));
    assert_eq!((target_empty, target_null), (1000, 2000));

    assert!(
        !script(&text_analysis, "verify").contains("LIMIT 100"),
        "verify 脚本不应再用固定 LIMIT 100 截断"
    );
    assert!(script(&text_analysis, "verify").contains(":page_size"));

    let (v2_p_total, v2_p, v2_p_pages, d2_p) =
        run_pages(conn, &payload_verify.v2_count, &payload_verify.v2_page, 100).await;
    let (v3_p_total, v3_p, v3_p_pages, d3_p) =
        run_pages(conn, &payload_verify.v3_count, &payload_verify.v3_page, 100).await;
    let (v4_p_total, v4_p, v4_p_pages, d4_p) =
        run_pages(conn, &payload_verify.v4_count, &payload_verify.v4_page, 100).await;
    println!(
        "V2 payload total={v2_p_total} pages={v2_p_pages:?} pks={v2_p:?} {:?}",
        d2_p
    );
    println!(
        "V3 payload total={v3_p_total} pages={v3_p_pages:?} pks={v3_p:?} {:?}",
        d3_p
    );
    println!(
        "V4 payload total={v4_p_total} pages={v4_p_pages:?} pks={v4_p:?} {:?}",
        d4_p
    );

    let (v2_t_total, v2_t, _, d2_t) =
        run_pages(conn, &text_verify.v2_count, &text_verify.v2_page, 100).await;
    let (v3_t_total, v3_t, _, d3_t) =
        run_pages(conn, &text_verify.v3_count, &text_verify.v3_page, 100).await;
    let (v4_t_total, v4_t, v4_t_pages, d4_t) =
        run_pages(conn, &text_verify.v4_count, &text_verify.v4_page, 100).await;
    println!(
        "V4 raw_text total={v4_t_total} pages={v4_t_pages:?} pks={v4_t:?} {:?}",
        d4_t
    );
    println!(
        "V2 raw_text total={v2_t_total} {:?} / V3 total={v3_t_total} {:?}",
        d2_t, d3_t
    );

    assert_clean(&v2_p, "V2 payload");
    assert_clean(&v3_p, "V3 payload");
    assert_clean(&v4_p, "V4 payload");
    assert_clean(&v2_t, "V2 raw_text");
    assert_clean(&v3_t, "V3 raw_text");
    assert_clean(&v4_t, "V4 raw_text");
    assert_eq!((v2_p_total, v3_p_total, v4_p_total), (0, 0, 0));
    assert_eq!((v2_t_total, v3_t_total, v4_t_total), (0, 0, 0));

    // 只读分页 API 的底层实现：干净数据返回总数 0、无主键。
    let clean_page = fetch_page(
        &ctx,
        &DiffPageRequest {
            table: SOURCE_TABLE.into(),
            column: "raw_text".into(),
            target_table: TEXT_TABLE.into(),
            pk_column: Some("id".into()),
            target_columns: text_analysis.target_columns.clone(),
            empty_string_policy: EmptyStringPolicy::Preserve,
            phase: "v4".into(),
            last_pk: None,
            page_size: Some(100),
        },
    )
    .await
    .expect("分页 API 失败");
    println!(
        "分页 API 干净数据：phase={} total={} pks={:?} has_more={}",
        clean_page.phase, clean_page.total, clean_page.pks, clean_page.has_more
    );
    assert_eq!(clean_page.total, 0);
    assert!(clean_page.pks.is_empty());
    assert!(!clean_page.has_more);

    // ── 故障注入 1：改一行值（V4 定位主键）────────────────
    exec(
        conn,
        &format!("UPDATE {schema}.{PAYLOAD_TABLE} SET a = 'CORRUPT' WHERE id = 12345"),
    )
    .await;
    let (total, pks, pages, elapsed) = run_pages(
        conn,
        &payload_verify.v4_count,
        &payload_verify.v4_page,
        100,
    )
    .await;
    println!(
        "故障1 改值：V4 payload total={total} pages={pages:?} -> {:?} {:?}",
        pks, elapsed
    );
    assert_eq!(total, 1);
    assert_only(&pks, 12345, "故障1 改值");
    exec(
        conn,
        &format!("UPDATE {schema}.{PAYLOAD_TABLE} SET a = 'v12345' WHERE id = 12345"),
    )
    .await;

    // ── 故障注入 2：删一行（V2 定位主键）──────────────────
    exec(
        conn,
        &format!("DELETE FROM {schema}.{PAYLOAD_TABLE} WHERE id = 23456"),
    )
    .await;
    let (total, pks, pages, elapsed) = run_pages(
        conn,
        &payload_verify.v2_count,
        &payload_verify.v2_page,
        100,
    )
    .await;
    let (source_rows, target_rows) = sqlx::query_as::<_, (i64, i64)>(&payload_verify.v1)
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    println!(
        "故障2 删行：V2 payload total={total} pages={pages:?} -> {:?} {:?}；V1 source={source_rows} target={target_rows}",
        pks, elapsed
    );
    assert_eq!(total, 1);
    assert_only(&pks, 23456, "故障2 删行");
    assert_eq!(target_rows, TOTAL_ROWS - 1);
    let restore = payload_backfill_tpl
        .replace(":last_pk", "20000")
        .replace(":end_pk", "30000")
        .replace(":batch_size", "10000");
    exec(conn, &restore).await;

    // ── 故障注入 3：多插一行（V3 定位主键）────────────────
    exec(
        conn,
        &format!("INSERT INTO {schema}.{PAYLOAD_TABLE} (id, a) VALUES (100001, 'extra')"),
    )
    .await;
    let (total, pks, pages, elapsed) = run_pages(
        conn,
        &payload_verify.v3_count,
        &payload_verify.v3_page,
        100,
    )
    .await;
    println!(
        "故障3 多插：V3 payload total={total} pages={pages:?} -> {:?} {:?}",
        pks, elapsed
    );
    assert_eq!(total, 1);
    assert_only(&pks, 100001, "故障3 多插");
    exec(
        conn,
        &format!("DELETE FROM {schema}.{PAYLOAD_TABLE} WHERE id = 100001"),
    )
    .await;

    // ── 故障注入 4：JSON key 顺序打乱（应一致）────────────
    exec(
        conn,
        &format!(
            "UPDATE {schema}.{SOURCE_TABLE} SET payload = JSON_OBJECT(\
               's', 'plain34567', 'meta', JSON_OBJECT('k', 'v'), 'count', 1, 'a', 'v34567'\
             ) WHERE id = 34567"
        ),
    )
    .await;
    let (total, pks, pages, elapsed) = run_pages(
        conn,
        &payload_verify.v4_count,
        &payload_verify.v4_page,
        100,
    )
    .await;
    println!(
        "故障4 key 顺序打乱：V4 payload total={total} pages={pages:?} -> {:?} {:?}",
        pks, elapsed
    );
    assert!(
        !pks.contains(&34567),
        "key 顺序不应产生差异，实际 {:?}",
        pks
    );

    // ── 故障注入 5：数值 1 -> 1.0（按 4.2 应一致）─────────
    exec(
        conn,
        &format!(
            "UPDATE {schema}.{SOURCE_TABLE} SET payload = JSON_SET(payload, '$.count', 1.0) WHERE id = 45678"
        ),
    )
    .await;
    let (json_type, token) = sqlx::query_as::<_, (String, String)>(
        "SELECT CONVERT(JSON_TYPE(JSON_EXTRACT(payload, '$.count')) USING utf8mb4), \
                CONVERT(JSON_UNQUOTE(JSON_EXTRACT(payload, '$.count')) USING utf8mb4) \
         FROM fm_big WHERE id = 45678",
    )
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    let (stored,): (String,) = sqlx::query_as::<_, (String,)>(
        "SELECT CONVERT(CAST(JSON_EXTRACT(payload, '$.count') AS CHAR) USING utf8mb4) \
         FROM fm_big WHERE id = 45678",
    )
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    println!(
        "故障5 数值 1->1.0：JSON_TYPE={json_type} token={token} json_char={stored}"
    );
    let (target_value,): (String,) = sqlx::query_as::<_, (String,)>(
        "SELECT CONVERT(CAST(count AS CHAR) USING utf8mb4) FROM fm_big_payload_ext WHERE id = 45678",
    )
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    println!("目标列 count={target_value}");
    let (total, pks, pages, elapsed) = run_pages(
        conn,
        &payload_verify.v4_count,
        &payload_verify.v4_page,
        100,
    )
    .await;
    println!(
        "故障5 后 V4 payload total={total} pages={pages:?} -> {:?} {:?}",
        pks, elapsed
    );
    assert!(
        !pks.contains(&45678),
        "1 与 1.0 经十进制规范化应一致，实际 {:?}",
        pks
    );

    // ── 故障注入 6：trim 前后不一致（应定位主键）───────────
    exec(
        conn,
        &format!(
            "INSERT INTO {schema}.{SOURCE_TABLE} (id, payload, raw_text) \
             VALUES (100002, NULL, '  trim-me  ')"
        ),
    )
    .await;
    exec(
        conn,
        &format!(
            "INSERT INTO {schema}.{TEXT_TABLE} (id, raw_text) VALUES (100002, 'trim-me')"
        ),
    )
    .await;
    let (total, pks, pages, elapsed) = run_pages(
        conn,
        &text_verify.v4_count,
        &text_verify.v4_page,
        100,
    )
    .await;
    println!(
        "故障6 trim 不一致：V4 raw_text total={total} pages={pages:?} -> {:?} {:?}",
        pks, elapsed
    );
    assert_eq!(total, 1);
    assert_only(&pks, 100002, "故障6 trim");
    exec(
        conn,
        &format!("DELETE FROM {schema}.{SOURCE_TABLE} WHERE id = 100002"),
    )
    .await;
    exec(
        conn,
        &format!("DELETE FROM {schema}.{TEXT_TABLE} WHERE id = 100002"),
    )
    .await;

    // ── 故障注入 7：目标把空串写成 NULL（必须定位主键）─────
    exec(
        conn,
        &format!("UPDATE {schema}.{TEXT_TABLE} SET raw_text = NULL WHERE id = 64001"),
    )
    .await;
    let (total, pks, pages, elapsed) = run_pages(
        conn,
        &text_verify.v4_count,
        &text_verify.v4_page,
        100,
    )
    .await;
    println!(
        "故障7 空串->NULL：V4 raw_text total={total} pages={pages:?} pks={:?} {:?}",
        pks, elapsed
    );
    assert_eq!(total, 1);
    assert_only(&pks, 64001, "故障7 空串->NULL");
    exec(
        conn,
        &format!("UPDATE {schema}.{TEXT_TABLE} SET raw_text = '' WHERE id = 64001"),
    )
    .await;
    let (total_after, pks_after, _, _) = run_pages(
        conn,
        &text_verify.v4_count,
        &text_verify.v4_page,
        100,
    )
    .await;
    println!(
        "故障7 恢复后 V4 raw_text total={total_after} pks={pks_after:?}"
    );
    assert_eq!(total_after, 0);

    // ── 200 条差异：验证分页完整覆盖 ─────────────────────
    println!("\n=== 200 条差异分页完整性 ===");
    let mut inserts = Vec::new();
    for id in 100_003..=100_202 {
        inserts.push(format!("({id}, 'extra-{id}')"));
    }
    exec(
        conn,
        &format!(
            "INSERT INTO {schema}.{TEXT_TABLE} (id, raw_text) VALUES {}",
            inserts.join(", ")
        ),
    )
    .await;
    let (total, all_pks, pages, elapsed) = run_pages(
        conn,
        &text_verify.v3_count,
        &text_verify.v3_page,
        100,
    )
    .await;
    println!(
        "200 差异 V3 total={total} pages={pages:?} collected={} {:?}",
        all_pks.len(),
        elapsed
    );
    assert_eq!(total, 200);
    assert_eq!(pages, vec![100, 100], "应恰好翻两页，每页 100 条");
    assert_eq!(
        all_pks,
        (100_003..=100_202).collect::<Vec<i64>>(),
        "分页必须完整覆盖全部 200 个差异主键"
    );

    // 只读分页 API：总数 + 当前页 + 主键区间游标。
    let first_page = fetch_page(
        &ctx,
        &DiffPageRequest {
            table: SOURCE_TABLE.into(),
            column: "raw_text".into(),
            target_table: TEXT_TABLE.into(),
            pk_column: Some("id".into()),
            target_columns: text_analysis.target_columns.clone(),
            empty_string_policy: EmptyStringPolicy::Preserve,
            phase: "v3".into(),
            last_pk: None,
            page_size: Some(100),
        },
    )
    .await
    .expect("分页 API 第一页失败");
    println!(
        "分页 API V3 第一页：total={} count={} next={:?} has_more={}",
        first_page.total,
        first_page.pks.len(),
        first_page.next_last_pk,
        first_page.has_more
    );
    assert_eq!(first_page.total, 200);
    assert_eq!(first_page.pks.len(), 100);
    assert!(first_page.has_more);
    let second_page = fetch_page(
        &ctx,
        &DiffPageRequest {
            table: SOURCE_TABLE.into(),
            column: "raw_text".into(),
            target_table: TEXT_TABLE.into(),
            pk_column: Some("id".into()),
            target_columns: text_analysis.target_columns.clone(),
            empty_string_policy: EmptyStringPolicy::Preserve,
            phase: "v3".into(),
            last_pk: first_page.next_last_pk.clone(),
            page_size: Some(100),
        },
    )
    .await
    .expect("分页 API 第二页失败");
    let mut page_pks = first_page.pks.clone();
    page_pks.extend(second_page.pks.clone());
    println!(
        "分页 API V3 第二页：total={} count={} last={:?} collected={}",
        second_page.total,
        second_page.pks.len(),
        second_page.next_last_pk,
        page_pks.len()
    );
    assert_eq!(second_page.total, 200);
    assert_eq!(second_page.pks.len(), 100);
    assert!(!second_page.has_more);
    let expected_pks: Vec<String> = (100_003..=100_202).map(|id| id.to_string()).collect();
    assert_eq!(page_pks, expected_pks);
    exec(
        conn,
        &format!("DELETE FROM {schema}.{TEXT_TABLE} WHERE id BETWEEN 100003 AND 100202"),
    )
    .await;

    // 清理
    exec(conn, &format!("DROP TABLE IF EXISTS {schema}.{SOURCE_TABLE}")).await;
    exec(conn, &format!("DROP TABLE IF EXISTS {schema}.{PAYLOAD_TABLE}")).await;
    exec(conn, &format!("DROP TABLE IF EXISTS {schema}.{TEXT_TABLE}")).await;
}
