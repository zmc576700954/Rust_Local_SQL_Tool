use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerformanceReport {
    pub schema_version: String,
    pub generated_at: DateTime<Utc>,
    pub cases: Vec<PerformanceCase>,
    pub meta: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerformanceCase {
    pub id: String,
    pub kind: String,
    pub labels: HashMap<String, String>,
    pub metrics: PerformanceMetrics,
    pub stages: Vec<PerformanceStage>,
    pub extra: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerformanceStage {
    pub name: String,
    pub metrics: PerformanceMetrics,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerformanceMetrics {
    pub duration_ms: u128,
    pub rows: Option<u64>,
    pub bytes: Option<u64>,
    pub throughput_rows_per_s: Option<f64>,
    pub throughput_bytes_per_s: Option<f64>,
}

impl PerformanceMetrics {
    pub fn new(duration_ms: u128, rows: Option<u64>, bytes: Option<u64>) -> Self {
        let dur_ms = duration_ms.max(1);
        let throughput_rows_per_s = rows.map(|r| (r as f64) * 1000.0 / (dur_ms as f64));
        let throughput_bytes_per_s = bytes.map(|b| (b as f64) * 1000.0 / (dur_ms as f64));
        Self {
            duration_ms,
            rows,
            bytes,
            throughput_rows_per_s,
            throughput_bytes_per_s,
        }
    }
}

impl PerformanceReport {
    pub fn new(cases: Vec<PerformanceCase>) -> Self {
        Self {
            schema_version: "1".to_string(),
            generated_at: Utc::now(),
            cases,
            meta: None,
        }
    }
}

