use core_lib::perf_report::PerformanceReport;
use serde_json::Value;
use std::fs;

fn env_string(name: &str, default: Option<&str>) -> Option<String> {
    std::env::var(name)
        .ok()
        .or_else(|| default.map(|d| d.to_string()))
}

fn env_u128(name: &str) -> Option<u128> {
    std::env::var(name).ok().and_then(|s| s.parse().ok())
}

fn env_f64(name: &str) -> Option<f64> {
    std::env::var(name).ok().and_then(|s| s.parse().ok())
}

fn env_bool(name: &str, default: bool) -> bool {
    std::env::var(name)
        .ok()
        .map(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes" | "y" | "on"))
        .unwrap_or(default)
}

fn arg_value(args: &[String], key: &str) -> Option<String> {
    let key_eq = format!("{}=", key);
    for a in args {
        if a.starts_with(&key_eq) {
            return Some(a[key_eq.len()..].to_string());
        }
    }
    None
}

fn usage() -> &'static str {
    "perf_gate_ci env/args:\n\
  REPORT_PATH=./perf-report.json  (or --report=...)\n\
  MAX_DURATION_MS=...  MIN_ROWS_PER_S=...  MIN_BYTES_PER_S=...\n\
  MAX_DURATION_MS_<KIND_UPPER>=...  MIN_ROWS_PER_S_<KIND_UPPER>=...  MIN_BYTES_PER_S_<KIND_UPPER>=...\n\
  FAIL_ON_MISSING=1|0 (default true)\n\
\n\
Exit codes:\n\
  0: pass\n\
  2: threshold failed\n\
  3: report read/parse error\n"
}

fn kind_key(kind: &str) -> String {
    kind.replace(['-', ' '], "_").to_uppercase()
}

fn threshold_u128(base: &str, kind: &str) -> Option<u128> {
    let kind_env = format!("{}_{}", base, kind_key(kind));
    env_u128(&kind_env).or_else(|| env_u128(base))
}

fn threshold_f64(base: &str, kind: &str) -> Option<f64> {
    let kind_env = format!("{}_{}", base, kind_key(kind));
    env_f64(&kind_env).or_else(|| env_f64(base))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if env_bool("HELP", false) || args.iter().any(|a| a == "--help") {
        eprintln!("{}", usage());
        return;
    }

    let report_path = arg_value(&args, "--report")
        .or_else(|| env_string("REPORT_PATH", None))
        .unwrap_or_else(|| "./perf-report.json".to_string());
    let fail_on_missing = env_bool("FAIL_ON_MISSING", true);

    let content = match fs::read_to_string(&report_path) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("failed to read report {}: {}", report_path, e);
            std::process::exit(3);
        }
    };

    let report: PerformanceReport = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("failed to parse report {}: {}", report_path, e);
            std::process::exit(3);
        }
    };

    let mut failures: Vec<Value> = Vec::new();
    for c in &report.cases {
        if let Some(max_ms) = threshold_u128("MAX_DURATION_MS", &c.kind) {
            if max_ms > 0 && c.metrics.duration_ms > max_ms {
                failures.push(serde_json::json!({
                    "id": c.id,
                    "kind": c.kind,
                    "metric": "duration_ms",
                    "actual": c.metrics.duration_ms,
                    "threshold": max_ms
                }));
            }
        } else if fail_on_missing {
            failures.push(serde_json::json!({
                "id": c.id,
                "kind": c.kind,
                "metric": "max_duration_ms_missing"
            }));
        }

        if let Some(min_rps) = threshold_f64("MIN_ROWS_PER_S", &c.kind) {
            let v = c.metrics.throughput_rows_per_s.unwrap_or(0.0);
            if min_rps > 0.0 && v < min_rps {
                failures.push(serde_json::json!({
                    "id": c.id,
                    "kind": c.kind,
                    "metric": "throughput_rows_per_s",
                    "actual": v,
                    "threshold": min_rps
                }));
            }
        }

        if let Some(min_bps) = threshold_f64("MIN_BYTES_PER_S", &c.kind) {
            let v = c.metrics.throughput_bytes_per_s.unwrap_or(0.0);
            if min_bps > 0.0 && v < min_bps {
                failures.push(serde_json::json!({
                    "id": c.id,
                    "kind": c.kind,
                    "metric": "throughput_bytes_per_s",
                    "actual": v,
                    "threshold": min_bps
                }));
            }
        }
    }

    if failures.is_empty() {
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({"ok": true, "cases": report.cases.len()})).unwrap_or_default());
        return;
    }

    eprintln!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "ok": false,
            "failures": failures,
            "report_path": report_path
        }))
        .unwrap_or_default()
    );
    std::process::exit(2);
}

