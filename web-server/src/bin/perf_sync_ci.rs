use reqwest::Client;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Write;
use std::time::Duration;

use core_lib::perf_report::{PerformanceCase, PerformanceMetrics, PerformanceReport, PerformanceStage};

fn env_string(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn env_bool(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .map(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes" | "y" | "on"))
        .unwrap_or(false)
}

async fn poll_job(http: &Client, base: &str, job_id: &str) -> Result<Value, Box<dyn std::error::Error>> {
    let url = format!("{}/backend/tools/perf-sync/jobs/{}", base.trim_end_matches('/'), job_id);
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
        tokio::time::sleep(Duration::from_millis(1200)).await;
    }
}

fn write_report(path: &str, payload: &Value) -> Result<(), Box<dyn std::error::Error>> {
    let mut f = std::fs::File::create(path)?;
    f.write_all(serde_json::to_string_pretty(payload)?.as_bytes())?;
    Ok(())
}

fn write_report_any<T: Serialize>(path: &str, payload: &T) -> Result<(), Box<dyn std::error::Error>> {
    let mut f = std::fs::File::create(path)?;
    f.write_all(serde_json::to_string_pretty(payload)?.as_bytes())?;
    Ok(())
}

fn build_cases(done: &Value) -> Vec<PerformanceCase> {
    let mut cases = Vec::new();
    let Some(report) = done.get("report") else {
        return cases;
    };

    let mut verify_lookup: HashMap<(String, String), Value> = HashMap::new();
    for mode_key in ["mirror", "upsert_only"] {
        let verify = report
            .get(mode_key)
            .and_then(|v| v.get("verify"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        for v in verify {
            let table = v.get("table_name").and_then(|x| x.as_str()).unwrap_or("").to_string();
            if table.is_empty() {
                continue;
            }
            verify_lookup.insert((mode_key.to_string(), table), v);
        }
    }

    for mode_key in ["mirror", "upsert_only"] {
        let tables = report
            .get(mode_key)
            .and_then(|v| v.get("tables"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        for t in tables {
            let table_name = t.get("table_name").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let primary_key = t.get("primary_key").and_then(|x| x.as_str()).unwrap_or("").to_string();
            if table_name.is_empty() {
                continue;
            }

            let compare_ms = t.get("compare_ms").and_then(|x| x.as_u64()).unwrap_or(0) as u128;
            let preview_ms = t.get("preview_ms").and_then(|x| x.as_u64()).unwrap_or(0) as u128;
            let deploy_ms = t.get("deploy_ms").and_then(|x| x.as_u64()).unwrap_or(0) as u128;
            let verify_ms = verify_lookup
                .get(&(mode_key.to_string(), table_name.clone()))
                .and_then(|v| v.get("verify_ms"))
                .and_then(|x| x.as_u64())
                .unwrap_or(0) as u128;

            let affected_rows = t.get("affected_rows").and_then(|x| x.as_u64());
            let duration_ms = compare_ms + preview_ms + deploy_ms + verify_ms;

            let mut labels = HashMap::new();
            labels.insert("mode".to_string(), mode_key.to_string());
            labels.insert("table".to_string(), table_name.clone());
            if !primary_key.is_empty() {
                labels.insert("primary_key".to_string(), primary_key);
            }

            let stages = vec![
                PerformanceStage {
                    name: "compare".to_string(),
                    metrics: PerformanceMetrics::new(compare_ms, None, None),
                },
                PerformanceStage {
                    name: "preview".to_string(),
                    metrics: PerformanceMetrics::new(preview_ms, None, None),
                },
                PerformanceStage {
                    name: "deploy".to_string(),
                    metrics: PerformanceMetrics::new(deploy_ms, affected_rows, None),
                },
                PerformanceStage {
                    name: "verify".to_string(),
                    metrics: PerformanceMetrics::new(verify_ms, None, None),
                },
            ];

            cases.push(PerformanceCase {
                id: format!("sync_{}_{}", mode_key, table_name),
                kind: "sync".to_string(),
                labels,
                metrics: PerformanceMetrics::new(duration_ms, affected_rows, None),
                stages,
                extra: Some(t.clone()),
            });
        }
    }

    cases
}

fn usage() -> &'static str {
    "perf_sync_ci env:\n\
  BASE_URL=http://127.0.0.1:3000\n\
  SOURCE_DB_ID=source  TARGET_DB_ID=target\n\
  TIER=1m|10m|100m  CHUNK_SIZE=1000  MAX_ROWS=20000\n\
  AUTO_FILL=1 (default true)  RESET=0  INJECT=1\n\
  SEED=1  BATCH=1000\n\
  REPORT_PATH=./perf-sync-report.json\n\
  FAIL_ON_VERIFY=1 (default true)\n\
\n\
Exit codes:\n\
  0: success (verify passed if enabled)\n\
  2: verify failed\n\
  3: request/runtime error\n"
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if env_bool("HELP") {
        eprintln!("{}", usage());
        return Ok(());
    }

    let base = env_string("BASE_URL", "http://127.0.0.1:3000");
    let source_db_id = env_string("SOURCE_DB_ID", "source");
    let target_db_id = env_string("TARGET_DB_ID", "target");
    let tier = env_string("TIER", "1m");
    let chunk_size = env_usize("CHUNK_SIZE", 1000);
    let max_rows = env_usize("MAX_ROWS", 20000);

    let auto_fill = std::env::var("AUTO_FILL")
        .ok()
        .map(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes" | "y" | "on"))
        .unwrap_or(true);
    let reset = env_bool("RESET");
    let inject = std::env::var("INJECT")
        .ok()
        .map(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes" | "y" | "on"))
        .unwrap_or(true);
    let seed = env_u64("SEED", 1);
    let batch = env_u64("BATCH", 1000);

    let report_path = env_string("REPORT_PATH", "./perf-sync-report.json");
    let fail_on_verify = std::env::var("FAIL_ON_VERIFY")
        .ok()
        .map(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes" | "y" | "on"))
        .unwrap_or(true);

    let http = Client::new();

    let check_url = format!("{}/backend/tools/perf-sync/check", base.trim_end_matches('/'));
    let check_resp = http
        .post(&check_url)
        .json(&json!({
            "source_db_id": source_db_id,
            "target_db_id": target_db_id,
            "tier": tier
        }))
        .send()
        .await?;
    let check_status = check_resp.status();
    let check: Value = check_resp.json().await?;
    if !check_status.is_success() {
        eprintln!("check failed: {}", check);
        std::process::exit(3);
    }

    let insufficient = check
        .get("insufficient")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let fill = auto_fill && !insufficient.is_empty();
    if !insufficient.is_empty() && !auto_fill {
        eprintln!("insufficient data and AUTO_FILL disabled: {}", check);
        write_report(&report_path, &json!({"check": check, "error": "insufficient_data"}))?;
        std::process::exit(3);
    }

    let start_url = format!("{}/backend/tools/perf-sync/start", base.trim_end_matches('/'));
    let start_resp = http
        .post(&start_url)
        .json(&json!({
            "source_db_id": source_db_id,
            "target_db_id": target_db_id,
            "tier": tier,
            "chunk_size": chunk_size,
            "max_rows": max_rows,
            "loadgen": fill.then(|| json!({
                "tier": tier,
                "fill": true,
                "reset": reset,
                "inject": inject,
                "seed": seed,
                "batch": batch
            }))
        }))
        .send()
        .await?;
    let start_status = start_resp.status();
    let start: Value = start_resp.json().await?;
    if !start_status.is_success() {
        eprintln!("start failed: {}", start);
        write_report(&report_path, &json!({"check": check, "start": start}))?;
        std::process::exit(3);
    }

    let job_id = start
        .get("job_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if job_id.is_empty() {
        eprintln!("missing job_id: {}", start);
        write_report(&report_path, &json!({"check": check, "start": start}))?;
        std::process::exit(3);
    }

    let done = match poll_job(&http, &base, job_id).await {
        Ok(v) => v,
        Err(e) => {
            eprintln!("job poll failed: {}", e);
            write_report(&report_path, &json!({"check": check, "start": start, "error": e.to_string()}))?;
            std::process::exit(3);
        }
    };

    let cases = build_cases(&done);
    let mut report = PerformanceReport::new(cases);
    report.meta = Some(json!({
        "check": check,
        "start": start,
        "done": done
    }));
    write_report_any(&report_path, &report)?;
    println!("{}", serde_json::to_string_pretty(&report)?);

    if fail_on_verify {
        let report_obj = done.get("report").or_else(|| done.get("result"));
        let passed = report_obj
            .and_then(|r| r.get("mirror"))
            .and_then(|m| m.get("passed"))
            .and_then(|p| p.as_bool())
            .unwrap_or(false);
        if !passed {
            eprintln!("verify failed (mirror.passed=false)");
            std::process::exit(2);
        }
    }

    Ok(())
}
