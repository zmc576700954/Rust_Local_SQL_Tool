use crate::config::{DbType, PoolConfig};
use crate::sql_util;
use crate::timeout_policy::TimeoutPolicy;
use std::time::Duration;
use thiserror::Error;

/// Placeholder style used by different database engines for parameterized queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaceholderStyle {
    /// `?` placeholders — MySQL, SQLite
    QuestionMark,
    /// `$1, $2, ...` placeholders — PostgreSQL
    DollarNumber,
    /// `@p1, @p2, ...` placeholders — SQL Server (reserved for future use)
    AtP,
    /// `:1, :2, ...` placeholders — Oracle (reserved for future use)
    ColonNumber,
}

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
    #[error("Unsupported database type: {0}")]
    Unsupported(String),
    #[error("Security: {0}")]
    Security(String),
}

/// Validate that a database connection URL is safe to use.
///
/// For a local SQL tool, connections to localhost/private IPs are expected and allowed.
/// This validator blocks:
/// - Non-database URL schemes (file://, ftp://, gopher://, etc.)
/// - SQLite paths that traverse outside safe directories (via ../)
/// - SQLite paths pointing to sensitive system files (/etc/passwd, etc.)
pub fn validate_db_url(url: &str, db_type: &DbType) -> Result<(), DbError> {
    match db_type {
        DbType::MySQL | DbType::MariaDB => {
            if !url.starts_with("mysql://") && !url.starts_with("mariadb://") {
                return Err(DbError::Security(format!(
                    "MySQL/MariaDB connection must use mysql:// or mariadb:// scheme, got: {}",
                    url.chars().take(20).collect::<String>()
                )));
            }
        }
        DbType::PostgreSQL => {
            if !url.starts_with("postgres://") && !url.starts_with("postgresql://") {
                return Err(DbError::Security(format!(
                    "PostgreSQL connection must use postgres:// or postgresql:// scheme, got: {}",
                    url.chars().take(20).collect::<String>()
                )));
            }
        }
        DbType::SQLite => {
            // SQLite URLs: "sqlite://" or "sqlite:" followed by file path
            // Block path traversal (../) that could escape intended directory
            let path = if url.starts_with("sqlite://") {
                &url[9..]
            } else if url.starts_with("sqlite:") {
                &url[7..]
            } else {
                return Err(DbError::Security(format!(
                    "SQLite connection must use sqlite:// scheme, got: {}",
                    url.chars().take(20).collect::<String>()
                )));
            };

            // Block path traversal
            if path.contains("..") {
                return Err(DbError::Security(
                    "SQLite path traversal detected: '..' is not allowed in database file paths"
                        .into(),
                ));
            }

            // Block sensitive system paths
            let sensitive_prefixes = [
                "/etc/",
                "/proc/",
                "/sys/",
                "/dev/",
                "/boot/",
                "/root/",
                "C:\\Windows\\",
                "C:\\ProgramData\\",
            ];
            for prefix in &sensitive_prefixes {
                if path.starts_with(prefix) {
                    return Err(DbError::Security(format!(
                        "SQLite path points to sensitive system directory: {}",
                        prefix
                    )));
                }
            }
        }
        other => {
            // For Redis/MongoDB/Oracle: only block clearly dangerous schemes
            let lower = url.to_lowercase();
            if lower.starts_with("file://")
                || lower.starts_with("ftp://")
                || lower.starts_with("gopher://")
                || lower.starts_with("dict://")
                || lower.starts_with("ldap://")
            {
                return Err(DbError::Security(format!(
                    "Dangerous URL scheme blocked for {} connection",
                    other.display_name()
                )));
            }
        }
    }
    Ok(())
}

/// Multi-engine connection pool wrapping sqlx pool types.
#[derive(Debug, Clone)]
pub enum DbPool {
    MySQL(sqlx::MySqlPool),
    Postgres(sqlx::PgPool),
    SQLite(sqlx::SqlitePool),
}

impl DbPool {
    /// Returns a reference to the inner MySQL pool, or error if not MySQL.
    pub fn mysql(&self) -> Result<&sqlx::MySqlPool, DbError> {
        match self {
            DbPool::MySQL(p) => Ok(p),
            _ => Err(DbError::Unsupported(
                "Expected MySQL pool but got different engine".into(),
            )),
        }
    }

    /// Returns a reference to the inner PgPool, or error if not Postgres.
    pub fn pg(&self) -> Result<&sqlx::PgPool, DbError> {
        match self {
            DbPool::Postgres(p) => Ok(p),
            _ => Err(DbError::Unsupported(
                "Expected PostgreSQL pool but got different engine".into(),
            )),
        }
    }

    /// Returns a reference to the inner SqlitePool, or error if not SQLite.
    pub fn sqlite(&self) -> Result<&sqlx::SqlitePool, DbError> {
        match self {
            DbPool::SQLite(p) => Ok(p),
            _ => Err(DbError::Unsupported(
                "Expected SQLite pool but got different engine".into(),
            )),
        }
    }

    /// Ping the database to verify the connection is alive.
    pub async fn ping(&self) -> Result<(), DbError> {
        match self {
            DbPool::MySQL(p) => {
                sqlx::query("SELECT 1").execute(p).await?;
            }
            DbPool::Postgres(p) => {
                sqlx::query("SELECT 1").execute(p).await?;
            }
            DbPool::SQLite(p) => {
                sqlx::query("SELECT 1").execute(p).await?;
            }
        }
        Ok(())
    }

    /// Close all connections in the pool.
    pub async fn close(&self) {
        match self {
            DbPool::MySQL(p) => p.close().await,
            DbPool::Postgres(p) => p.close().await,
            DbPool::SQLite(p) => p.close().await,
        }
    }

    /// Returns the engine type.
    pub fn db_type(&self) -> DbType {
        match self {
            DbPool::MySQL(_) => DbType::MySQL,
            DbPool::Postgres(_) => DbType::PostgreSQL,
            DbPool::SQLite(_) => DbType::SQLite,
        }
    }

    /// Returns the placeholder style for this pool's database engine.
    pub fn placeholder_style(&self) -> PlaceholderStyle {
        match self {
            DbPool::MySQL(_) | DbPool::SQLite(_) => PlaceholderStyle::QuestionMark,
            DbPool::Postgres(_) => PlaceholderStyle::DollarNumber,
        }
    }

    /// Quotes an identifier using the appropriate quoting style for this pool's engine.
    pub fn quote_ident(&self, s: &str) -> String {
        match self {
            DbPool::MySQL(_) => sql_util::quote_ident_mysql(s),
            DbPool::Postgres(_) | DbPool::SQLite(_) => sql_util::quote_ident_pg(s),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DbClient {
    pub pool: DbPool,
    pub db_type: DbType,
}

impl DbClient {
    /// Creates a new database connection pool for the given engine type.
    /// Validates the URL for security before connecting.
    pub async fn new(
        url: &str,
        pool_config: &PoolConfig,
        db_type: &DbType,
    ) -> Result<Self, DbError> {
        validate_db_url(url, db_type)?;

        let policy = TimeoutPolicy::default();
        let pool = match db_type {
            DbType::MySQL | DbType::MariaDB => {
                use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions};
                use std::str::FromStr;

                let options = MySqlConnectOptions::from_str(url)?;
                let pool_future = MySqlPoolOptions::new()
                    .max_connections(pool_config.max_connections)
                    .min_connections(pool_config.min_connections)
                    .acquire_timeout(Duration::from_millis(pool_config.acquire_timeout_ms))
                    .idle_timeout(Duration::from_millis(pool_config.idle_timeout_ms))
                    .max_lifetime(Duration::from_millis(pool_config.max_lifetime_ms))
                    .test_before_acquire(pool_config.test_before_acquire)
                    .connect_with(options);

                let p = tokio::time::timeout(policy.db_connect, pool_future)
                    .await
                    .map_err(|_| DbError::Timeout)??;
                DbPool::MySQL(p)
            }
            DbType::PostgreSQL => {
                use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
                use std::str::FromStr;

                let options = PgConnectOptions::from_str(url)?;
                let pool_future = PgPoolOptions::new()
                    .max_connections(pool_config.max_connections)
                    .min_connections(pool_config.min_connections)
                    .acquire_timeout(Duration::from_millis(pool_config.acquire_timeout_ms))
                    .idle_timeout(Duration::from_millis(pool_config.idle_timeout_ms))
                    .max_lifetime(Duration::from_millis(pool_config.max_lifetime_ms))
                    .test_before_acquire(pool_config.test_before_acquire)
                    .connect_with(options);

                let p = tokio::time::timeout(policy.db_connect, pool_future)
                    .await
                    .map_err(|_| DbError::Timeout)??;
                DbPool::Postgres(p)
            }
            DbType::SQLite => {
                use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
                use std::str::FromStr;

                let options = SqliteConnectOptions::from_str(url)?;
                let pool_future = SqlitePoolOptions::new()
                    .max_connections(pool_config.max_connections)
                    .min_connections(pool_config.min_connections)
                    .acquire_timeout(Duration::from_millis(pool_config.acquire_timeout_ms))
                    .idle_timeout(Duration::from_millis(pool_config.idle_timeout_ms))
                    .max_lifetime(Duration::from_millis(pool_config.max_lifetime_ms))
                    .test_before_acquire(pool_config.test_before_acquire)
                    .connect_with(options);

                let p = tokio::time::timeout(policy.db_connect, pool_future)
                    .await
                    .map_err(|_| DbError::Timeout)??;
                DbPool::SQLite(p)
            }
            other => return Err(DbError::Unsupported(other.display_name().into())),
        };

        Ok(Self {
            pool,
            db_type: db_type.clone(),
        })
    }

    /// Creates a new connection pool with default configuration, auto-detecting engine from URL.
    pub async fn new_default(url: &str) -> Result<Self, DbError> {
        let db_type = DbType::from_url(url).unwrap_or(DbType::MySQL);
        Self::new(url, &PoolConfig::default(), &db_type).await
    }

    /// Backward-compat: returns the inner MySQL pool, or error if not MySQL.
    pub fn mysql_pool(&self) -> Result<&sqlx::MySqlPool, DbError> {
        self.pool.mysql()
    }

    pub async fn ping(&self) -> Result<(), DbError> {
        self.pool.ping().await
    }

    /// MySQL-only: get the connection ID for an active session.
    pub async fn connection_id_for_session(
        conn: &mut sqlx::pool::PoolConnection<sqlx::MySql>,
    ) -> Result<u64, DbError> {
        use sqlx::Row;

        let row = sqlx::query("SELECT CONNECTION_ID() AS connection_id")
            .fetch_one(&mut **conn)
            .await?;
        Ok(row.try_get::<u64, _>("connection_id")?)
    }

    /// MySQL-only: kill a running query on a specific connection.
    pub async fn kill_query(&self, connection_id: u64) -> Result<(), DbError> {
        let pool = self.pool.mysql()?;
        let sql = format!("KILL QUERY {}", connection_id);
        sqlx::query(&sql).execute(pool).await?;
        Ok(())
    }

    /// Extract the database name from the connection URL
    pub fn extract_db_name(url: &str) -> Option<String> {
        url.split('/')
            .next_back()
            .map(|s| s.split('?').next().unwrap_or(s).to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_db_name_mysql_url() {
        assert_eq!(
            DbClient::extract_db_name("mysql://user:pass@127.0.0.1:3306/mydb"),
            Some("mydb".to_string())
        );
    }

    #[test]
    fn extract_db_name_pg_url() {
        assert_eq!(
            DbClient::extract_db_name("postgres://user:pass@localhost:5432/app_db"),
            Some("app_db".to_string())
        );
    }

    #[test]
    fn extract_db_name_with_query_params() {
        assert_eq!(
            DbClient::extract_db_name("mysql://user:pass@host:3306/db?charset=utf8mb4"),
            Some("db".to_string())
        );
    }

    #[test]
    fn db_type_from_url() {
        assert_eq!(DbType::from_url("mysql://localhost/db"), Some(DbType::MySQL));
        assert_eq!(DbType::from_url("postgres://localhost/db"), Some(DbType::PostgreSQL));
        assert_eq!(DbType::from_url("postgresql://localhost/db"), Some(DbType::PostgreSQL));
        assert_eq!(DbType::from_url("sqlite:///tmp/test.db"), Some(DbType::SQLite));
        assert_eq!(DbType::from_url("mariadb://localhost/db"), Some(DbType::MariaDB));
        assert_eq!(DbType::from_url("redis://localhost"), Some(DbType::Redis));
        assert_eq!(DbType::from_url("mongodb://localhost"), Some(DbType::MongoDB));
        assert_eq!(DbType::from_url("unknown://localhost/db"), None);
    }

    #[test]
    fn db_type_display_name() {
        assert_eq!(DbType::MySQL.display_name(), "MySQL");
        assert_eq!(DbType::PostgreSQL.display_name(), "PostgreSQL");
        assert_eq!(DbType::SQLite.display_name(), "SQLite");
    }

    #[test]
    fn validate_db_url_mysql_rejects_bad_scheme() {
        assert!(validate_db_url("ftp://host/db", &DbType::MySQL).is_err());
        assert!(validate_db_url("file:///etc/passwd", &DbType::MySQL).is_err());
        assert!(validate_db_url("postgres://host/db", &DbType::MySQL).is_err());
    }

    #[test]
    fn validate_db_url_mysql_accepts_valid() {
        assert!(validate_db_url("mysql://user:pass@127.0.0.1:3306/db", &DbType::MySQL).is_ok());
        assert!(validate_db_url("mariadb://user:pass@host/db", &DbType::MariaDB).is_ok());
    }

    #[test]
    fn validate_db_url_pg_rejects_bad_scheme() {
        assert!(validate_db_url("mysql://host/db", &DbType::PostgreSQL).is_err());
        assert!(validate_db_url("ftp://host/db", &DbType::PostgreSQL).is_err());
    }

    #[test]
    fn validate_db_url_pg_accepts_valid() {
        assert!(validate_db_url("postgres://user:pass@localhost:5432/db", &DbType::PostgreSQL).is_ok());
        assert!(validate_db_url("postgresql://user:pass@host/db", &DbType::PostgreSQL).is_ok());
    }

    #[test]
    fn validate_db_url_sqlite_rejects_traversal() {
        assert!(validate_db_url("sqlite:///tmp/../etc/passwd", &DbType::SQLite).is_err());
        assert!(validate_db_url("sqlite:../secret.db", &DbType::SQLite).is_err());
    }

    #[test]
    fn validate_db_url_sqlite_rejects_sensitive_paths() {
        assert!(validate_db_url("sqlite:///etc/passwd", &DbType::SQLite).is_err());
        assert!(validate_db_url("sqlite:///proc/self/environ", &DbType::SQLite).is_err());
    }

    #[test]
    fn validate_db_url_sqlite_accepts_valid() {
        assert!(validate_db_url("sqlite:///tmp/test.db", &DbType::SQLite).is_ok());
        assert!(validate_db_url("sqlite:///home/user/data/my.db", &DbType::SQLite).is_ok());
    }

    #[test]
    fn validate_db_url_other_rejects_dangerous_schemes() {
        assert!(validate_db_url("file:///etc/passwd", &DbType::Redis).is_err());
        assert!(validate_db_url("gopher://host", &DbType::MongoDB).is_err());
        assert!(validate_db_url("dict://host", &DbType::Redis).is_err());
    }
}
