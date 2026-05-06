use sqlx::{mysql::MySqlPoolOptions, MySqlPool};
use std::time::Duration;
use thiserror::Error;
use crate::timeout_policy::TimeoutPolicy;

#[derive(Debug, Error)]
pub enum DbError {
    #[error("Database error: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("Connection timeout")]
    Timeout,
    #[error("Connection string is missing")]
    MissingUrl,
    #[error("Data is missing: {0}")]
    MissingData(String),
}

#[derive(Debug, Clone)]
pub struct DbClient {
    pub pool: MySqlPool,
}

impl DbClient {
    /// Creates a new database connection pool
    pub async fn new(url: &str) -> Result<Self, DbError> {
        use sqlx::mysql::MySqlConnectOptions;
        use std::str::FromStr;

        let policy = TimeoutPolicy::default();
        let options = MySqlConnectOptions::from_str(url)?;

        let pool_future = MySqlPoolOptions::new()
            .max_connections(5)
            .acquire_timeout(Duration::from_secs(5))
            .connect_with(options);

        let pool = tokio::time::timeout(policy.db_connect, pool_future)
            .await
            .map_err(|_| DbError::Timeout)??;
        Ok(Self { pool })
    }

    /// Extract the database name from the connection URL
    pub fn extract_db_name(url: &str) -> Option<String> {
        // Simple extraction logic for "mysql://user:pass@host:port/dbname"
        url.split('/')
            .next_back()
            .map(|s| s.split('?').next().unwrap_or(s).to_string())
    }
}
