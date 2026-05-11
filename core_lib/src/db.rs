use crate::timeout_policy::TimeoutPolicy;
use sqlx::{mysql::MySqlPoolOptions, MySqlPool};
use std::time::Duration;
use thiserror::Error;

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
            .max_connections(10)
            .min_connections(1)
            .acquire_timeout(Duration::from_secs(3))
            .idle_timeout(Duration::from_secs(600))
            .max_lifetime(Duration::from_secs(1800))
            .test_before_acquire(false)
            .connect_with(options);

        let pool = tokio::time::timeout(policy.db_connect, pool_future)
            .await
            .map_err(|_| DbError::Timeout)??;
        Ok(Self { pool })
    }

    pub async fn ping(&self) -> Result<(), DbError> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }

    pub async fn connection_id_for_session(
        conn: &mut sqlx::pool::PoolConnection<sqlx::MySql>,
    ) -> Result<u64, DbError> {
        use sqlx::Row;

        let row = sqlx::query("SELECT CONNECTION_ID() AS connection_id")
            .fetch_one(&mut **conn)
            .await?;
        Ok(row.try_get::<u64, _>("connection_id")?)
    }

    pub async fn kill_query(&self, connection_id: u64) -> Result<(), DbError> {
        let sql = format!("KILL QUERY {}", connection_id);
        sqlx::query(&sql).execute(&self.pool).await?;
        Ok(())
    }

    /// Extract the database name from the connection URL
    pub fn extract_db_name(url: &str) -> Option<String> {
        // Simple extraction logic for "mysql://user:pass@host:port/dbname"
        url.split('/')
            .next_back()
            .map(|s| s.split('?').next().unwrap_or(s).to_string())
    }
}
