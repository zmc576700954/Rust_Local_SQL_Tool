use core_lib::perf_report::{
    PerformanceCase, PerformanceMetrics, PerformanceReport, PerformanceStage,
};
use reqwest::multipart::{Form, Part};
use reqwest::Client;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Duration;

type AnyError = Box<dyn std::error::Error + Send + Sync>;

fn env_string(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn env_bool(name: &str, default: bool) -> bool {
    std::env::var(name)
        .ok()
        .map(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes" | "y" | "on"))
        .unwrap_or(default)
}

fn percentile(sorted: &[u128], p: f64) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    let p = p.clamp(0.0, 1.0);
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn build_csv(rows: usize, cols: usize) -> Vec<u8> {
    let cols = cols.max(1);
    let mut out = String::new();
    for c in 0..cols {
        if c > 0 {
            out.push(',');
        }
        out.push_str(&format!("c{}", c + 1));
    }
    out.push('\n');
    for r in 0..rows {
        for c in 0..cols {
            if c > 0 {
                out.push(',');
            }
            out.push_str(&format!("v{}_{}", r + 1, c + 1));
        }
        out.push('\n');
    }
    out.into_bytes()
}

async fn configure_db(http: &Client, base: &str, db_url: &str) -> Result<(), AnyError> {
    let url = format!("{}/backend/config", base.trim_end_matches('/'));
    let resp = http
        .post(&url)
        .json(&json!({
            "db_connections": [{
                "id": "e2e",
                "name": "e2e",
                "url": db_url
            }],
            "active_db_id": "e2e"
        }))
        .send()
        .await?;
    let status = resp.status();
    let v: Value = resp.json().await.unwrap_or_else(|_| json!({}));
    if !status.is_success() {
        return Err(format!("config update failed: {}", v).into());
    }
    Ok(())
}

async fn execute_sql(http: &Client, base: &str, sql: &str) -> Result<(), AnyError> {
    let url = format!("{}/backend/sql/execute", base.trim_end_matches('/'));
    let resp = http
        .post(&url)
        .json(&json!({ "sql": sql, "force": true }))
        .send()
        .await?;
    let status = resp.status();
    let v: Value = resp.json().await.unwrap_or_else(|_| json!({}));
    if !status.is_success() {
        return Err(format!("execute sql failed: {}", v).into());
    }
    Ok(())
}

async fn upload_csv(
    http: &Client,
    base: &str,
    csv_bytes: Vec<u8>,
) -> Result<(String, u128), AnyError> {
    let url = format!(
        "{}/backend/tools/data-transfer/upload",
        base.trim_end_matches('/')
    );
    let t0 = std::time::Instant::now();
    let part = Part::bytes(csv_bytes).file_name("load.csv");
    let form = Form::new()
        .part("file", part)
        .text("delimiter", ",")
        .text("encoding", "utf-8");
    let resp = http.post(&url).multipart(form).send().await?;
    let status = resp.status();
    let v: Value = resp.json().await?;
    if !status.is_success() {
        return Err(format!("upload failed: {}", v).into());
    }
    let path = v
        .get("source_path")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    if path.is_empty() {
        return Err(format!("upload missing source_path: {}", v).into());
    }
    Ok((path, t0.elapsed().as_millis()))
}

async fn transfer_execute(
    http: &Client,
    base: &str,
    source_path: String,
    target_table: &str,
    cols: usize,
) -> Result<(String, u128), AnyError> {
    let url = format!(
        "{}/backend/tools/data-transfer/execute",
        base.trim_end_matches('/')
    );
    let mappings: Vec<Value> = (0..cols.max(1))
        .map(|i| {
            let c = format!("c{}", i + 1);
            json!({ "source_col": c, "target_col": c })
        })
        .collect();
    let payload = json!({
        "source_type": "local_file",
        "source_path": source_path,
        "source_url": null,
        "source_db_id": null,
        "source_table": null,
        "target_url": "",
        "target_table": target_table,
        "mode": "Replace",
        "mappings": mappings
    });
    let t0 = std::time::Instant::now();
    let resp = http.post(&url).json(&payload).send().await?;
    let status = resp.status();
    let v: Value = resp.json().await?;
    if !status.is_success() {
        return Err(format!("transfer_execute failed: {}", v).into());
    }
    let dml = v
        .get("dml")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    if dml.is_empty() {
        return Err(format!("transfer_execute missing dml: {}", v).into());
    }
    Ok((dml, t0.elapsed().as_millis()))
}

async fn import_sql_start(
    http: &Client,
    base: &str,
    sql: String,
) -> Result<(String, u128), AnyError> {
    let url = format!(
        "{}/backend/tools/jobs/import-sql/start",
        base.trim_end_matches('/')
    );
    let t0 = std::time::Instant::now();
    let resp = http
        .post(&url)
        .json(&json!({ "sql": sql, "force": true }))
        .send()
        .await?;
    let status = resp.status();
    let v: Value = resp.json().await?;
    if !status.is_success() {
        return Err(format!("import_sql_start failed: {}", v).into());
    }
    let job_id = v
        .get("job_id")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    if job_id.is_empty() {
        return Err(format!("import_sql_start missing job_id: {}", v).into());
    }
    Ok((job_id, t0.elapsed().as_millis()))
}

async fn poll_job(http: &Client, base: &str, job_id: &str) -> Result<(Value, u128), AnyError> {
    let url = format!(
        "{}/backend/tools/jobs/{}",
        base.trim_end_matches('/'),
        job_id
    );
    let t0 = std::time::Instant::now();
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
        if s == "completed" || s == "error" || s == "canceled" {
            return Ok((v, t0.elapsed().as_millis()));
        }
        tokio::time::sleep(Duration::from_millis(800)).await;
    }
}

fn usage() -> &'static str {
    "perf_data_transfer_ci env:\n\
  BASE_URL=http://127.0.0.1:3000\n\
  DB_URL=mysql://root:password@127.0.0.1:3306/e2e\n\
  TARGET_TABLE=load_transfer\n\
  CSV_ROWS=2000  CSV_COLS=6\n\
  CONCURRENCY=4  ITERATIONS=8\n\
  RUN_IMPORT=1 (default true)\n\
  REPORT_PATH=./perf-data-transfer-report.json\n\
  FAIL_ON_ERROR=1 (default true)\n"
}

#[tokio::main]
async fn main() -> Result<(), AnyError> {
    if env_bool("HELP", false) {
        eprintln!("{}", usage());
        return Ok(());
    }

    let base = env_string("BASE_URL", "http://127.0.0.1:3000");
    let db_url = env_string("DB_URL", "mysql://root:password@127.0.0.1:3306/e2e");
    let target_table = env_string("TARGET_TABLE", "load_transfer");
    let csv_rows = env_usize("CSV_ROWS", 2000);
    let csv_cols = env_usize("CSV_COLS", 6);
    let concurrency = env_usize("CONCURRENCY", 4).max(1);
    let iterations = env_usize("ITERATIONS", 8).max(1);
    let run_import = env_bool("RUN_IMPORT", true);
    let report_path = env_string("REPORT_PATH", "./perf-data-transfer-report.json");
    let fail_on_error = env_bool("FAIL_ON_ERROR", true);

    let http = Client::new();
    if run_import {
        configure_db(&http, &base, &db_url).await?;
    }

    if run_import {
        let mut ddl = String::new();
        ddl.push_str("CREATE TABLE IF NOT EXISTS `");
        ddl.push_str(&target_table.replace('`', "``"));
        ddl.push_str("` (");
        for i in 0..csv_cols.max(1) {
            if i > 0 {
                ddl.push(',');
            }
            ddl.push_str(&format!("`c{}` VARCHAR(255) NOT NULL", i + 1));
        }
        ddl.push_str(") ENGINE=InnoDB;");
        execute_sql(&http, &base, &ddl).await?;
    }

    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(concurrency));
    let mut handles = Vec::new();

    for _ in 0..iterations {
        let sem = sem.clone();
        let http = http.clone();
        let base = base.clone();
        let target_table = target_table.clone();
        let permit = sem.acquire_owned().await?;
        handles.push(tokio::spawn(async move {
            let _permit = permit;
            let csv = build_csv(csv_rows, csv_cols);
            let bytes = csv.len() as u64;
            let (source_path, upload_ms) = upload_csv(&http, &base, csv).await?;
            let (dml, execute_ms) =
                transfer_execute(&http, &base, source_path, &target_table, csv_cols).await?;
            let (start_ms, poll_ms, done) = if run_import {
                let (job_id, start_ms) = import_sql_start(&http, &base, dml).await?;
                let (done, poll_ms) = poll_job(&http, &base, &job_id).await?;
                (start_ms, poll_ms, done)
            } else {
                (0, 0, json!({ "status": "skipped" }))
            };
            Ok::<(u128, u128, u128, u128, u64, Value), AnyError>((
                upload_ms, execute_ms, start_ms, poll_ms, bytes, done,
            ))
        }));
    }

    let mut upload_lat = Vec::<u128>::new();
    let mut execute_lat = Vec::<u128>::new();
    let mut start_lat = Vec::<u128>::new();
    let mut poll_lat = Vec::<u128>::new();
    let mut total_lat = Vec::<u128>::new();
    let mut errors = Vec::<String>::new();
    let mut bytes_total: u64 = 0;

    for h in handles {
        match h.await {
            Ok(Ok((u, e, s, p, bytes, done))) => {
                if run_import {
                    let status = done
                        .get("status")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_lowercase();
                    if status != "completed" {
                        errors.push(format!("job not completed: {}", done));
                        continue;
                    }
                }
                upload_lat.push(u);
                execute_lat.push(e);
                start_lat.push(s);
                poll_lat.push(p);
                total_lat.push(u + e + s + p);
                bytes_total = bytes_total.saturating_add(bytes);
            }
            Ok(Err(e)) => errors.push(e.to_string()),
            Err(e) => errors.push(e.to_string()),
        }
    }

    upload_lat.sort_unstable();
    execute_lat.sort_unstable();
    start_lat.sort_unstable();
    poll_lat.sort_unstable();
    total_lat.sort_unstable();

    let total_rows = (csv_rows as u64).saturating_mul((iterations - errors.len()) as u64);
    let cases = vec![PerformanceCase {
        id: "data_transfer_csv_import_sql".to_string(),
        kind: "data_transfer".to_string(),
        labels: HashMap::from([
            ("target_table".to_string(), target_table.clone()),
            ("csv_rows".to_string(), csv_rows.to_string()),
            ("csv_cols".to_string(), csv_cols.to_string()),
            ("concurrency".to_string(), concurrency.to_string()),
            ("iterations".to_string(), iterations.to_string()),
            ("run_import".to_string(), run_import.to_string()),
        ]),
        metrics: PerformanceMetrics::new(
            total_lat.iter().copied().sum(),
            Some(total_rows),
            Some(bytes_total),
        ),
        stages: vec![
            PerformanceStage {
                name: "upload".to_string(),
                metrics: PerformanceMetrics::new(upload_lat.iter().copied().sum(), None, None),
            },
            PerformanceStage {
                name: "transfer_execute".to_string(),
                metrics: PerformanceMetrics::new(execute_lat.iter().copied().sum(), None, None),
            },
            PerformanceStage {
                name: "import_sql_start".to_string(),
                metrics: PerformanceMetrics::new(start_lat.iter().copied().sum(), None, None),
            },
            PerformanceStage {
                name: "import_sql_poll".to_string(),
                metrics: PerformanceMetrics::new(poll_lat.iter().copied().sum(), None, None),
            },
        ],
        extra: Some(json!({
            "ok": errors.is_empty(),
            "errors": errors,
            "latency_ms": {
                "upload_p50": percentile(&upload_lat, 0.50),
                "upload_p95": percentile(&upload_lat, 0.95),
                "execute_p50": percentile(&execute_lat, 0.50),
                "execute_p95": percentile(&execute_lat, 0.95),
                "poll_p50": percentile(&poll_lat, 0.50),
                "poll_p95": percentile(&poll_lat, 0.95),
                "total_p50": percentile(&total_lat, 0.50),
                "total_p95": percentile(&total_lat, 0.95)
            },
            "successes": (iterations - errors.len()),
            "bytes_total": bytes_total,
            "rows_total": total_rows
        })),
    }];

    let report = PerformanceReport::new(cases);
    std::fs::write(&report_path, serde_json::to_vec_pretty(&report)?)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    eprintln!("REPORT_PATH={}", report_path);

    if fail_on_error && !errors.is_empty() {
        std::process::exit(2);
    }

    Ok(())
}
