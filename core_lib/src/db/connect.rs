use crate::config::{DbConnection, DbType, PoolConfig};
use crate::db::client::{DbClient, DbError};
use crate::db::ssh::{
    extract_target_host_port_from_url, rewrite_db_url_with_local_tunnel, start_ssh_tunnel,
    SshTunnelConfig,
};

/// Connection-level options that must be applied when the real pool is created.
#[derive(Debug, Clone, Default)]
pub struct DbClientConnectOptions {
    /// TLS mode for engines that support it (`disabled` / `preferred` /
    /// `required` / `verify_ca` / `verify_identity`).
    pub ssl_mode: Option<String>,
}

impl DbClientConnectOptions {
    pub fn from_connection(conn: &DbConnection) -> Self {
        Self {
            ssl_mode: ssl_mode_from_connection(conn),
        }
    }
}

fn ssl_mode_from_connection(conn: &DbConnection) -> Option<String> {
    let ssl = conn.ssl.as_ref()?;
    let enabled = ssl.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
    if !enabled {
        return None;
    }
    let mode = ssl
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("preferred")
        .trim()
        .to_ascii_lowercase();
    Some(if mode.is_empty() {
        "preferred".to_string()
    } else {
        mode
    })
}

fn json_string(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn ssh_fields(conn: &DbConnection) -> Result<Option<(String, u16, String, String)>, DbError> {
    let Some(ssh) = conn.ssh.as_ref() else {
        return Ok(None);
    };
    let enabled = ssh.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
    if !enabled {
        return Ok(None);
    }

    let host = json_string(ssh, "host")
        .ok_or_else(|| DbError::MissingData("SSH host is required when SSH is enabled".into()))?;
    let username = json_string(ssh, "username").ok_or_else(|| {
        DbError::MissingData("SSH username is required when SSH is enabled".into())
    })?;
    let password = json_string(ssh, "password")
        .ok_or_else(|| DbError::MissingData("SSH password is required when SSH is enabled".into()))?;
    let port = ssh
        .get("port")
        .and_then(|v| v.as_u64())
        .filter(|p| *p > 0 && *p <= u16::MAX as u64)
        .map(|p| p as u16)
        .unwrap_or(22);

    Ok(Some((host, port, username, password)))
}

/// Stable fingerprint of everything that affects how a connection is opened.
/// Used to decide whether a config save needs a reconnect and to invalidate the
/// temporary connection cache.
pub fn connection_fingerprint(conn: &DbConnection) -> String {
    let ssh = conn
        .ssh
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_default())
        .unwrap_or_default();
    let ssl = conn
        .ssl
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_default())
        .unwrap_or_default();
    format!(
        "{}|{}|{}|{}",
        conn.url,
        conn.db_type
            .as_ref()
            .map(|t| t.display_name())
            .unwrap_or_default(),
        ssh,
        ssl
    )
}

/// Opens a real `DbClient` from a stored connection definition, applying SSH
/// tunneling and TLS exactly like the connection will be used at runtime.
///
/// Supporting paths that cannot honor the requested transport will return an
/// explicit error instead of silently connecting in plaintext.
pub async fn open_db_client(
    conn: &DbConnection,
    pool_config: &PoolConfig,
) -> Result<DbClient, DbError> {
    let db_type = conn
        .db_type
        .clone()
        .or_else(|| DbType::from_url(&conn.url))
        .ok_or_else(|| {
            DbError::Unsupported(format!(
                "无法从连接地址识别数据库类型: {}",
                conn.url.chars().take(40).collect::<String>()
            ))
        })?;

    let options = DbClientConnectOptions::from_connection(conn);
    let mut url = conn.url.clone();
    let mut tunnel = None;

    if let Some((ssh_host, ssh_port, ssh_username, ssh_password)) = ssh_fields(conn)? {
        if matches!(db_type, DbType::SQLite) {
            return Err(DbError::Unsupported(
                "SQLite 是本地文件数据库，不支持 SSH 隧道".into(),
            ));
        }
        let (remote_host, remote_port) =
            extract_target_host_port_from_url(&url).map_err(DbError::Tunnel)?;
        let handle = start_ssh_tunnel(SshTunnelConfig {
            ssh_host,
            ssh_port,
            ssh_username,
            ssh_password,
            remote_host,
            remote_port,
        })
        .map_err(DbError::Tunnel)?;
        url = rewrite_db_url_with_local_tunnel(&url, handle.local_port).map_err(DbError::Tunnel)?;
        tunnel = Some(handle);
    }

    if options.ssl_mode.is_some() && matches!(db_type, DbType::SQLite) {
        return Err(DbError::Unsupported(
            "SQLite 是本地文件数据库，不支持 SSL".into(),
        ));
    }

    DbClient::new_with_options(&url, pool_config, &db_type, options, tunnel).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn conn(url: &str, ssh: Option<serde_json::Value>, ssl: Option<serde_json::Value>) -> DbConnection {
        DbConnection {
            id: "c1".into(),
            name: "test".into(),
            url: url.into(),
            group_name: None,
            color: None,
            is_favorite: false,
            ssh,
            ssl,
            db_type: DbType::from_url(url),
            capability_level: None,
            schema: None,
            is_read_only: false,
        }
    }

    #[test]
    fn ssl_mode_disabled_connection_is_ignored() {
        let c = conn("mysql://u:p@h/db", None, Some(json!({"enabled": false, "mode": "required"})));
        assert_eq!(DbClientConnectOptions::from_connection(&c).ssl_mode, None);
    }

    #[test]
    fn ssl_mode_enabled_uses_configured_mode() {
        let c = conn("mysql://u:p@h/db", None, Some(json!({"enabled": true, "mode": "verify_ca"})));
        assert_eq!(
            DbClientConnectOptions::from_connection(&c).ssl_mode.as_deref(),
            Some("verify_ca")
        );
    }

    #[test]
    fn ssl_mode_enabled_defaults_to_preferred() {
        let c = conn("mysql://u:p@h/db", None, Some(json!({"enabled": true})));
        assert_eq!(
            DbClientConnectOptions::from_connection(&c).ssl_mode.as_deref(),
            Some("preferred")
        );
    }

    #[test]
    fn ssh_disabled_is_ignored() {
        let c = conn("mysql://u:p@h/db", Some(json!({"enabled": false})), None);
        assert!(ssh_fields(&c).unwrap().is_none());
    }

    #[test]
    fn ssh_missing_fields_are_rejected() {
        let c = conn(
            "mysql://u:p@h/db",
            Some(json!({"enabled": true, "host": "bastion", "username": "ops"})),
            None,
        );
        let err = ssh_fields(&c).unwrap_err().to_string();
        assert!(err.contains("password"), "unexpected error: {err}");
    }

    #[test]
    fn fingerprint_changes_with_transport_config() {
        let base = conn("mysql://u:p@h/db", None, None);
        let with_ssl = conn(
            "mysql://u:p@h/db",
            None,
            Some(json!({"enabled": true, "mode": "required"})),
        );
        let with_ssh = conn(
            "mysql://u:p@h/db",
            Some(json!({"enabled": true, "host": "bastion", "username": "ops", "password": "pw"})),
            None,
        );
        assert_ne!(connection_fingerprint(&base), connection_fingerprint(&with_ssl));
        assert_ne!(connection_fingerprint(&base), connection_fingerprint(&with_ssh));
    }

    #[tokio::test]
    async fn sqlite_with_ssh_is_explicitly_rejected() {
        let c = conn(
            "sqlite:///tmp/test.db",
            Some(json!({"enabled": true, "host": "bastion", "username": "ops", "password": "pw"})),
            None,
        );
        let err = open_db_client(&c, &PoolConfig::default()).await.unwrap_err().to_string();
        assert!(err.contains("SSH"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn sqlite_with_ssl_is_explicitly_rejected() {
        let c = conn(
            "sqlite:///tmp/test.db",
            None,
            Some(json!({"enabled": true, "mode": "preferred"})),
        );
        let err = open_db_client(&c, &PoolConfig::default()).await.unwrap_err().to_string();
        assert!(err.contains("SSL"), "unexpected error: {err}");
    }
}
