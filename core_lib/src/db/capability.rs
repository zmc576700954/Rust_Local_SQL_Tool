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

pub fn capability_level(db_type: &DbType) -> DbCapabilityLevel {
    match db_type {
        DbType::MySQL | DbType::MariaDB | DbType::PostgreSQL | DbType::SQLite => {
            DbCapabilityLevel::A
        }
        DbType::SQLServer | DbType::Oracle => DbCapabilityLevel::B,
        DbType::MongoDB => DbCapabilityLevel::C,
        DbType::Redis => DbCapabilityLevel::D,
    }
}

pub fn capabilities(db_type: &DbType) -> DbCapabilities {
    let level = capability_level(db_type);
    match db_type {
        DbType::MySQL | DbType::MariaDB | DbType::PostgreSQL | DbType::SQLite => DbCapabilities {
            level,
            supports_sql: true,
            supports_schema_introspection: true,
            supports_import_export: true,
            supports_struct_sync: true,
            supports_data_sync: true,
        },
        DbType::SQLServer | DbType::Oracle => DbCapabilities {
            level,
            supports_sql: true,
            supports_schema_introspection: false,
            supports_import_export: false,
            supports_struct_sync: false,
            supports_data_sync: false,
        },
        DbType::MongoDB => DbCapabilities {
            level,
            supports_sql: false,
            supports_schema_introspection: false,
            supports_import_export: false,
            supports_struct_sync: false,
            supports_data_sync: false,
        },
        DbType::Redis => DbCapabilities {
            level,
            supports_sql: false,
            supports_schema_introspection: false,
            supports_import_export: false,
            supports_struct_sync: false,
            supports_data_sync: false,
        },
    }
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
            DbType::PostgreSQL | DbType::SQLite => DbCapabilities {
                level,
                supports_sql: true,
                supports_schema_introspection: true,
                supports_import_export: true,
                supports_struct_sync: true,
                supports_data_sync: true,
            },
            DbType::SQLServer | DbType::Oracle => DbCapabilities {
                level,
                supports_sql: true,
                supports_schema_introspection: true,
                supports_import_export: true,
                supports_struct_sync: false,
                supports_data_sync: false,
            },
            DbType::MongoDB => DbCapabilities {
                level,
                supports_sql: false,
                supports_schema_introspection: false,
                supports_import_export: false,
                supports_struct_sync: false,
                supports_data_sync: false,
            },
            DbType::Redis => DbCapabilities {
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
        assert!(caps.supports_sql);
        assert!(caps.supports_data_sync);
    }

    #[test]
    fn runtime_capabilities_pg_has_sync() {
        let caps = DbCapabilities::runtime_capabilities(&DbType::PostgreSQL);
        assert!(caps.supports_sql);
        assert!(caps.supports_data_sync);
    }

    #[test]
    fn runtime_capabilities_mssql_no_sync() {
        let caps = DbCapabilities::runtime_capabilities(&DbType::SQLServer);
        assert!(caps.supports_sql);
        assert!(!caps.supports_data_sync);
        assert!(!caps.supports_struct_sync);
    }

    #[test]
    fn check_capability_success() {
        let caps = DbCapabilities::runtime_capabilities(&DbType::MySQL);
        assert!(caps.check_capability("data_sync").is_ok());
    }

    #[test]
    fn check_capability_failure() {
        let caps = DbCapabilities::runtime_capabilities(&DbType::Redis);
        assert!(caps.check_capability("data_sync").is_err());
    }
}
