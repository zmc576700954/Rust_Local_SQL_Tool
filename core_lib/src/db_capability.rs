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
        DbType::MySQL | DbType::MariaDB | DbType::PostgreSQL | DbType::SQLite => DbCapabilityLevel::A,
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
