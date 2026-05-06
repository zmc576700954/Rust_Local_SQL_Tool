use std::time::Duration;

#[derive(Debug, Clone)]
pub struct TimeoutPolicy {
    pub db_connect: Duration,
    pub db_query: Duration,
    pub db_query_long: Duration,
    pub external_http_connect: Duration,
    pub external_http_default: Duration,
    pub ai_tier_fast: Duration,
    pub ai_tier_balanced: Duration,
    pub ai_tier_high: Duration,
    pub ai_tier_ultra: Duration,
    pub job_poll_request: Duration,
}

fn env_u64(key: &str) -> Option<u64> {
    std::env::var(key).ok().and_then(|v| v.parse::<u64>().ok())
}

fn ms(key: &str, default_ms: u64) -> Duration {
    Duration::from_millis(env_u64(key).unwrap_or(default_ms))
}

impl Default for TimeoutPolicy {
    fn default() -> Self {
        Self {
            db_connect: ms("LOCAL_AI_SQL_DB_CONNECT_TIMEOUT_MS", 10_000),
            db_query: ms("LOCAL_AI_SQL_DB_QUERY_TIMEOUT_MS", 60_000),
            db_query_long: ms("LOCAL_AI_SQL_DB_QUERY_LONG_TIMEOUT_MS", 120_000),
            external_http_connect: ms("LOCAL_AI_SQL_HTTP_CONNECT_TIMEOUT_MS", 10_000),
            external_http_default: ms("LOCAL_AI_SQL_HTTP_TIMEOUT_MS", 90_000),
            ai_tier_fast: ms("LOCAL_AI_SQL_AI_TIER_FAST_TIMEOUT_MS", 30_000),
            ai_tier_balanced: ms("LOCAL_AI_SQL_AI_TIER_BALANCED_TIMEOUT_MS", 60_000),
            ai_tier_high: ms("LOCAL_AI_SQL_AI_TIER_HIGH_TIMEOUT_MS", 90_000),
            ai_tier_ultra: ms("LOCAL_AI_SQL_AI_TIER_ULTRA_TIMEOUT_MS", 120_000),
            job_poll_request: ms("LOCAL_AI_SQL_JOB_POLL_TIMEOUT_MS", 10_000),
        }
    }
}

impl TimeoutPolicy {
    pub fn ai_request_timeout_for_tier(&self, tier: &str) -> Duration {
        match tier {
            "fast" => self.ai_tier_fast,
            "balanced" => self.ai_tier_balanced,
            "high" => self.ai_tier_high,
            "ultra" => self.ai_tier_ultra,
            _ => self.ai_tier_balanced,
        }
    }
}

