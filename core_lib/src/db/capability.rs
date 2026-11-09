use crate::config::{DbCapabilityLevel, DbType};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbCapabilities {
    pub level: DbCapabilityLevel,
    pub supports_sql: bool,
    pub supports_schema_introspection: bool,
    pub supports_import_export: bool,
    pub supports_struct_sync: bool,
    pub supports_data_sync: bool,
}

/// Capability level reflects what is actually wired in the current codebase:
/// - A: full pipeline (connect / test / SQL / schema / DDL / import-export / sync)
/// - B: partial pipeline (connect / test / SQL / schema / DDL / import-export,
///      but no struct/data sync and ops-center scenarios remain MySQL-only)
/// - C/D: declared but not connectable / not implemented
pub fn capability_level(db_type: &DbType) -> DbCapabilityLevel {
    match db_type {
        DbType::MySQL | DbType::MariaDB => DbCapabilityLevel::A,
        // PostgreSQL: connection, SQL execution, transactions, schema browsing,
        // DDL execution, import/export and structure sync are wired. Data sync
        // and the ops-center scenarios still depend on MySQL metadata and stay
        // unsupported.
        // SQLite: connection + SQL execution only; schema browsing and DDL are
        // deliberately not exposed yet.
        DbType::PostgreSQL | DbType::SQLite => DbCapabilityLevel::B,
        DbType::MongoDB => DbCapabilityLevel::C,
        DbType::SQLServer | DbType::Oracle | DbType::Redis => DbCapabilityLevel::D,
    }
}

pub fn capabilities(db_type: &DbType) -> DbCapabilities {
    DbCapabilities::runtime_capabilities(db_type)
}

impl DbCapabilities {
    pub fn runtime_capabilities(db_type: &DbType) -> Self {
        let level = capability_level(db_type);
        match db_type {
            DbType::MySQL | DbType::MariaDB => DbCapabilities {
                level,
                supports_sql: true,
                supports_schema_introspection: true,
                supports_import_export: true,
                supports_struct_sync: true,
                supports_data_sync: true,
            },
            // PostgreSQL: SQL execution, schema introspection, import/export and
            // structure sync are wired. Data sync still relies on MySQL-shaped
            // compare/deploy SQL and stays unsupported.
            DbType::PostgreSQL => DbCapabilities {
                level,
                supports_sql: true,
                supports_schema_introspection: true,
                supports_import_export: true,
                supports_struct_sync: true,
                supports_data_sync: false,
            },
            // SQLite: connect + SQL execution only. Schema browsing, DDL,
            // import/export and sync are explicitly not advertised.
            DbType::SQLite => DbCapabilities {
                level,
                supports_sql: true,
                supports_schema_introspection: false,
                supports_import_export: false,
                supports_struct_sync: false,
                supports_data_sync: false,
            },
            // SQL Server / Oracle / MongoDB / Redis have no working connection
            // path (DbClient::new returns Unsupported), so no capability is claimed.
            DbType::SQLServer
            | DbType::Oracle
            | DbType::MongoDB
            | DbType::Redis => DbCapabilities {
                level,
                supports_sql: false,
                supports_schema_introspection: false,
                supports_import_export: false,
                supports_struct_sync: false,
                supports_data_sync: false,
            },
        }
    }

    pub fn check_capability(&self, name: &str) -> Result<(), String> {
        let supported = match name {
            "sql" => self.supports_sql,
            "schema_introspection" => self.supports_schema_introspection,
            "import_export" => self.supports_import_export,
            "struct_sync" => self.supports_struct_sync,
            "data_sync" => self.supports_data_sync,
            _ => return Err(format!("Unknown capability: {}", name)),
        };
        if supported {
            Ok(())
        } else {
            Err(format!(
                "Capability '{}' is not supported for this database type",
                name
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DbType;

    #[test]
    fn runtime_capabilities_mysql_full() {
        let caps = DbCapabilities::runtime_capabilities(&DbType::MySQL);
        assert_eq!(caps.level, DbCapabilityLevel::A);
        assert!(caps.supports_sql);
        assert!(caps.supports_schema_introspection);
        assert!(caps.supports_import_export);
        assert!(caps.supports_struct_sync);
        assert!(caps.supports_data_sync);
    }

    #[test]
    fn runtime_capabilities_pg_wires_structure_sync_but_not_data_sync() {
        let caps = DbCapabilities::runtime_capabilities(&DbType::PostgreSQL);
        assert_eq!(caps.level, DbCapabilityLevel::B);
        assert!(caps.supports_sql);
        assert!(caps.supports_schema_introspection);
        assert!(caps.supports_import_export);
        assert!(caps.supports_struct_sync);
        assert!(!caps.supports_data_sync);
    }

    #[test]
    fn runtime_capabilities_sqlite_only_query_execution() {
        let caps = DbCapabilities::runtime_capabilities(&DbType::SQLite);
        assert_eq!(caps.level, DbCapabilityLevel::B);
        assert!(caps.supports_sql);
        assert!(!caps.supports_schema_introspection);
        assert!(!caps.supports_import_export);
        assert!(!caps.supports_struct_sync);
        assert!(!caps.supports_data_sync);
    }

    #[test]
    fn runtime_capabilities_mssql_not_connectable() {
        let caps = DbCapabilities::runtime_capabilities(&DbType::SQLServer);
        assert_eq!(caps.level, DbCapabilityLevel::D);
        assert!(!caps.supports_sql);
        assert!(!caps.supports_schema_introspection);
        assert!(!caps.supports_import_export);
        assert!(!caps.supports_data_sync);
        assert!(!caps.supports_struct_sync);
    }

    #[test]
    fn static_and_runtime_capabilities_agree() {
        for db_type in [
            DbType::MySQL,
            DbType::MariaDB,
            DbType::PostgreSQL,
            DbType::SQLite,
            DbType::SQLServer,
            DbType::Oracle,
            DbType::MongoDB,
            DbType::Redis,
        ] {
            assert_eq!(capabilities(&db_type), DbCapabilities::runtime_capabilities(&db_type));
        }
    }

    #[test]
    fn check_capability_success() {
        let caps = DbCapabilities::runtime_capabilities(&DbType::MySQL);
        assert!(caps.check_capability("data_sync").is_ok());
    }

    #[test]
    fn check_capability_failure_for_experimental_and_unsupported() {
        let pg_caps = DbCapabilities::runtime_capabilities(&DbType::PostgreSQL);
        // PostgreSQL 已接通查询、结构内省、导入导出与结构同步，但数据同步仍未支持
        assert!(pg_caps.check_capability("sql").is_ok());
        assert!(pg_caps.check_capability("schema_introspection").is_ok());
        assert!(pg_caps.check_capability("import_export").is_ok());
        assert!(pg_caps.check_capability("struct_sync").is_ok());
        assert!(pg_caps.check_capability("data_sync").is_err());

        // SQLite 仅接通连接与查询执行
        let sqlite_caps = DbCapabilities::runtime_capabilities(&DbType::SQLite);
        assert!(sqlite_caps.check_capability("sql").is_ok());
        assert!(sqlite_caps.check_capability("schema_introspection").is_err());
        assert!(sqlite_caps.check_capability("import_export").is_err());

        let mssql_caps = DbCapabilities::runtime_capabilities(&DbType::SQLServer);
        assert!(mssql_caps.check_capability("sql").is_err());

        let redis_caps = DbCapabilities::runtime_capabilities(&DbType::Redis);
        assert!(redis_caps.check_capability("data_sync").is_err());
    }
}
