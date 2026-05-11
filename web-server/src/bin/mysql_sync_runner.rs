use core_lib::db::DbClient;
use core_lib::mysql_sync::{MySqlDataSyncEngine, SyncMode};
use reqwest::Client;
use serde_json::{json, Value};
use std::time::Instant;

fn env_bool(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .map(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes" | "y" | "on"))
        .unwrap_or(false)
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn env_string(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

fn tier_rows(tier: &str) -> Value {
    match tier {
        "1m" => {
            json!({"users":1_000_000,"orders":1_000_000,"events":1_000_000,"kv_hotspot":1_000_000,"files":100_000})
        }
        "10m" => {
            json!({"users":10_000_000,"orders":10_000_000,"events":10_000_000,"kv_hotspot":10_000_000,"files":1_000_000})
        }
        "100m" => {
            json!({"users":100_000_000,"orders":100_000_000,"events":100_000_000,"kv_hotspot":100_000_000,"files":10_000_000})
        }
        _ => {
            json!({"users":1_000_000,"orders":1_000_000,"events":1_000_000,"kv_hotspot":1_000_000,"files":100_000})
        }
    }
}

fn parse_mode(s: &str) -> SyncMode {
    match s.to_lowercase().as_str() {
        "mirror" => SyncMode::Mirror,
        "upsert_only" | "upsert-only" | "upsert" => SyncMode::UpsertOnly,
        other => panic!("Invalid MODE: {}", other),
    }
}

async fn poll_job(
    http: &Client,
    base: &str,
    job_id: &str,
) -> Result<Value, Box<dyn std::error::Error>> {
    let url = format!(
        "{}/backend/tools/data-sync/jobs/{}",
        base.trim_end_matches('/'),
        job_id
    );
    loop {
        let resp = http.get(&url).send().await?;
        let status = resp.status();
        let v: Value = resp.json().await?;
        if !status.is_success() {
            return Err(format!("job status error: {}", v).into());
        }
        let s = v
            .get("status")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_lowercase();
        if s == "completed" {
            return Ok(v);
        }
        if s == "error" {
            return Err(format!("job failed: {}", v).into());
        }
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn http_sync_table(
    http: &Client,
    base: &str,
    source_db_id: &str,
    target_db_id: &str,
    table: &str,
    primary_key: &str,
    mode: &str,
    chunk_size: usize,
    max_rows: usize,
) -> Result<Value, Box<dyn std::error::Error>> {
    let t0 = Instant::now();
    let compare_url = format!(
        "{}/backend/tools/data-sync/compare",
        base.trim_end_matches('/')
    );
    let compare_resp = http
        .post(&compare_url)
        .json(&json!({
            "source_db_id": source_db_id,
            "target_db_id": target_db_id,
            "table_name": table,
            "primary_key": primary_key,
            "mode": mode,
            "chunk_size": chunk_size
        }))
        .send()
        .await?;
    let compare_resp: Value = compare_resp.json().await?;
    let job_id = compare_resp
        .get("job_id")
        .and_then(|x| x.as_str())
        .ok_or("missing job_id")?
        .to_string();
    let compare_job = poll_job(http, base, &job_id).await?;
    let compare_ms = t0.elapsed().as_millis();

    let t1 = Instant::now();
    let preview_url = format!(
        "{}/backend/tools/data-sync/preview",
        base.trim_end_matches('/')
    );
    let actions: Vec<&str> = if mode == "upsert_only" {
        vec!["insert", "update"]
    } else {
        vec!["insert", "update", "delete"]
    };
    let preview_resp = http
        .post(&preview_url)
        .json(&json!({
            "job_id": job_id,
            "max_rows": max_rows,
            "actions": actions
        }))
        .send()
        .await?;
    let _preview_resp: Value = preview_resp.json().await?;
    let preview_job = poll_job(http, base, &job_id).await?;
    let preview_ms = t1.elapsed().as_millis();

    let t2 = Instant::now();
    let deploy_url = format!(
        "{}/backend/tools/data-sync/deploy",
        base.trim_end_matches('/')
    );
    let deploy_resp = http
        .post(&deploy_url)
        .json(&json!({ "job_id": job_id }))
        .send()
        .await?;
    let _deploy_resp: Value = deploy_resp.json().await?;
    let deploy_job = poll_job(http, base, &job_id).await?;
    let deploy_ms = t2.elapsed().as_millis();

    Ok(json!({
        "mode": mode,
        "table": table,
        "primary_key": primary_key,
        "job_id": job_id,
        "compare_ms": compare_ms,
        "preview_ms": preview_ms,
        "deploy_ms": deploy_ms,
        "compare": compare_job.get("compare").cloned(),
        "preview": preview_job.get("preview").cloned(),
        "deploy": deploy_job.get("deploy").cloned(),
    }))
}

async fn engine_sync_table(
    source: &DbClient,
    target: &DbClient,
    table: &str,
    primary_key: &str,
    mode: SyncMode,
    chunk_size: usize,
    max_rows: usize,
) -> Result<Value, Box<dyn std::error::Error>> {
    let mode_label = format!("{:?}", mode).to_lowercase();
    let t0 = Instant::now();
    let compare =
        MySqlDataSyncEngine::compare(source, target, table, primary_key, chunk_size).await?;
    let compare_ms = t0.elapsed().as_millis();

    let t1 = Instant::now();
    let preview =
        MySqlDataSyncEngine::preview(source, target, &compare, mode.clone(), max_rows, None)
            .await?;
    let preview_ms = t1.elapsed().as_millis();

    let t2 = Instant::now();
    let affected = MySqlDataSyncEngine::deploy(target, &preview.statements, |_c, _t| {}).await?;
    let deploy_ms = t2.elapsed().as_millis();

    Ok(json!({
        "mode": mode_label,
        "table": table,
        "primary_key": primary_key,
        "compare_ms": compare_ms,
        "preview_ms": preview_ms,
        "deploy_ms": deploy_ms,
        "compare": { "chunks": compare.chunks.len(), "different_chunks": compare.different_chunks },
        "preview": { "diff": preview.diff, "truncated": preview.truncated, "statements": preview.statements.len() },
        "deploy": { "affected_rows": affected }
    }))
}

async fn engine_verify_mirror_zero_diff(
    source: &DbClient,
    target: &DbClient,
    table: &str,
    primary_key: &str,
    chunk_size: usize,
) -> Result<Value, Box<dyn std::error::Error>> {
    let t0 = Instant::now();
    let compare =
        MySqlDataSyncEngine::compare(source, target, table, primary_key, chunk_size).await?;
    Ok(json!({
        "table": table,
        "different_chunks": compare.different_chunks,
        "chunks": compare.chunks.len(),
        "verify_ms": t0.elapsed().as_millis()
    }))
}

fn runner_usage() -> &'static str {
    "mysql_sync_runner env:\n\
  SOURCE_DB_URL / TARGET_DB_URL (required)\n\
  BASE_URL=http://127.0.0.1:3000 (http path)\n\
  SOURCE_DB_ID=source  TARGET_DB_ID=target (ids in app config)\n\
  TIER=1m|10m|100m  CHUNK_SIZE=1000  MAX_ROWS=20000\n\
  LOADGEN=1 RESET=1 DIVERGE=1 SEED=1 BATCH=1000\n\
\n\
Example:\n\
  LOADGEN=1 RESET=1 DIVERGE=1 TIER=1m \\\n\
  SOURCE_DB_URL='mysql://...' TARGET_DB_URL='mysql://...' \\\n\
  BASE_URL='http://127.0.0.1:3000' SOURCE_DB_ID='dev' TARGET_DB_ID='prod' \\\n\
  cargo run -p web-server --bin mysql_sync_runner\n"
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if env_bool("HELP") {
        eprintln!("{}", runner_usage());
        return Ok(());
    }
    let source_url = std::env::var("SOURCE_DB_URL")
        .or_else(|_| std::env::var("SOURCE_URL"))
        .expect("Missing SOURCE_DB_URL");
    let target_url = std::env::var("TARGET_DB_URL")
        .or_else(|_| std::env::var("TARGET_URL"))
        .expect("Missing TARGET_DB_URL");

    let base = env_string("BASE_URL", "http://127.0.0.1:3000");
    let source_db_id = env_string("SOURCE_DB_ID", "source");
    let target_db_id = env_string("TARGET_DB_ID", "target");

    let tier = env_string("TIER", "1m");
    let chunk_size = env_usize("CHUNK_SIZE", 1000);
    let max_rows = env_usize("MAX_ROWS", 20000);
    let seed = env_u64("SEED", 1);
    let batch = env_u64("BATCH", 1000);

    let do_loadgen = env_bool("LOADGEN");
    let reset = env_bool("RESET");
    let diverge = env_bool("DIVERGE");

    let tables: Vec<(&str, &str)> = vec![
        ("users", "id"),
        ("orders", "id"),
        ("events", "id"),
        ("kv_hotspot", "id"),
        ("files", "id"),
    ];

    let t0 = Instant::now();
    if do_loadgen {
        let mut cmd = std::process::Command::new("cargo");
        cmd.arg("run")
            .arg("-p")
            .arg("web-server")
            .arg("--bin")
            .arg("mysql_sync_loadgen")
            .arg("--")
            .arg(format!("--tier={}", tier));
        cmd.env("SOURCE_DB_URL", &source_url)
            .env("TARGET_DB_URL", &target_url)
            .env("SEED", seed.to_string())
            .env("BATCH", batch.to_string());
        if reset {
            cmd.env("RESET", "1");
        }
        if diverge {
            cmd.env("DIVERGE", "1");
        }
        let st = cmd.status()?;
        if !st.success() {
            return Err("loadgen failed".into());
        }
    }

    let http = Client::new();
    let source = DbClient::new(&source_url).await?;
    let target = DbClient::new(&target_url).await?;

    let mut report_http = Vec::new();
    let mut report_engine = Vec::new();
    let mut verify = Vec::new();

    for mode in ["mirror", "upsert_only"] {
        for (table, pk) in &tables {
            let http_res = http_sync_table(
                &http,
                &base,
                &source_db_id,
                &target_db_id,
                table,
                pk,
                mode,
                chunk_size,
                max_rows,
            )
            .await?;
            report_http.push(http_res);

            let engine_res = engine_sync_table(
                &source,
                &target,
                table,
                pk,
                parse_mode(mode),
                chunk_size,
                max_rows,
            )
            .await?;
            report_engine.push(engine_res);

            if mode == "mirror" {
                let v =
                    engine_verify_mirror_zero_diff(&source, &target, table, pk, chunk_size).await?;
                verify.push(v);
            }
        }
    }

    let out = json!({
        "tier": tier,
        "rows": tier_rows(&tier),
        "chunk_size": chunk_size,
        "max_rows": max_rows,
        "loadgen": { "enabled": do_loadgen, "reset": reset, "diverge": diverge, "seed": seed, "batch": batch },
        "http": report_http,
        "engine": report_engine,
        "verify": verify,
        "elapsed_ms": t0.elapsed().as_millis()
    });

    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}
