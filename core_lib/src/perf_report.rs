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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerfSample {
    pub operation: String,
    pub iteration: u32,
    pub duration_ms: u128,
    pub rows: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerfBudget {
    pub operation: String,
    pub target_p50_ms: Option<u128>,
    pub target_p95_ms: Option<u128>,
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerfProbeSummary {
    pub operation: String,
    pub sample_count: usize,
    pub min_ms: u128,
    pub max_ms: u128,
    pub avg_ms: u128,
    pub p50_ms: u128,
    pub p95_ms: u128,
    pub rows: Option<u64>,
    pub budget: Option<PerfBudget>,
    pub samples: Vec<PerfSample>,
}

fn percentile_nearest_rank(sorted: &[u128], percentile: usize) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = ((sorted.len() * percentile).div_ceil(100)).saturating_sub(1);
    sorted[rank.min(sorted.len() - 1)]
}

pub fn summarize_perf_samples(
    operation: &str,
    samples: Vec<PerfSample>,
    budget: Option<PerfBudget>,
) -> PerfProbeSummary {
    if samples.is_empty() {
        return PerfProbeSummary {
            operation: operation.to_string(),
            sample_count: 0,
            min_ms: 0,
            max_ms: 0,
            avg_ms: 0,
            p50_ms: 0,
            p95_ms: 0,
            rows: None,
            budget,
            samples,
        };
    }

    let mut durations: Vec<u128> = samples.iter().map(|sample| sample.duration_ms).collect();
    durations.sort_unstable();
    let total: u128 = durations.iter().copied().sum();

    PerfProbeSummary {
        operation: operation.to_string(),
        sample_count: samples.len(),
        min_ms: *durations.first().unwrap_or(&0),
        max_ms: *durations.last().unwrap_or(&0),
        avg_ms: total / (durations.len() as u128),
        p50_ms: percentile_nearest_rank(&durations, 50),
        p95_ms: percentile_nearest_rank(&durations, 95),
        rows: samples.iter().filter_map(|sample| sample.rows).max(),
        budget,
        samples,
    }
}

#[cfg(test)]
mod tests {
    use super::{summarize_perf_samples, PerfBudget, PerfSample};

    #[test]
    fn summarize_perf_samples_calculates_percentiles() {
        let summary = summarize_perf_samples(
            "query_select_small",
            vec![
                PerfSample {
                    operation: "query_select_small".to_string(),
                    iteration: 1,
                    duration_ms: 10,
                    rows: Some(1),
                },
                PerfSample {
                    operation: "query_select_small".to_string(),
                    iteration: 2,
                    duration_ms: 30,
                    rows: Some(1),
                },
                PerfSample {
                    operation: "query_select_small".to_string(),
                    iteration: 3,
                    duration_ms: 20,
                    rows: Some(1),
                },
            ],
            Some(PerfBudget {
                operation: "query_select_small".to_string(),
                target_p50_ms: Some(80),
                target_p95_ms: Some(150),
                source: Some("test".to_string()),
            }),
        );

        assert_eq!(summary.sample_count, 3);
        assert_eq!(summary.min_ms, 10);
        assert_eq!(summary.max_ms, 30);
        assert_eq!(summary.avg_ms, 20);
        assert_eq!(summary.p50_ms, 20);
        assert_eq!(summary.p95_ms, 30);
        assert_eq!(summary.rows, Some(1));
        assert!(summary.budget.is_some());
    }
}
