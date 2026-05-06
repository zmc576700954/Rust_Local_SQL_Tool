import type { DbCapabilityLevel, DbType } from '../types'

export function dbTypeDisplayName(dbType?: DbType | null) {
  if (dbType === 'mysql') return 'MySQL'
  if (dbType === 'mariadb') return 'MariaDB'
  if (dbType === 'postgresql') return 'PostgreSQL'
  if (dbType === 'sqlite') return 'SQLite'
  if (dbType === 'sqlserver') return 'SQLServer'
  if (dbType === 'mongodb') return 'MongoDB'
  if (dbType === 'redis') return 'Redis'
  if (dbType === 'oracle') return 'Oracle'
  return 'Unknown'
}

export function dbLevelDisplayName(level?: DbCapabilityLevel | null) {
  if (level === 'a') return 'Level A'
  if (level === 'b') return 'Level B'
  if (level === 'c') return 'Level C'
  if (level === 'd') return 'Level D'
  return 'Level -'
}

export type DbCapabilities = {
  level: DbCapabilityLevel
  supports_sql: boolean
  supports_schema_introspection: boolean
  supports_import_export: boolean
  supports_struct_sync: boolean
  supports_data_sync: boolean
}

export function dbCapabilityLevel(dbType?: DbType | null): DbCapabilityLevel {
  if (dbType === 'mysql' || dbType === 'mariadb' || dbType === 'postgresql' || dbType === 'sqlite') return 'a'
  if (dbType === 'sqlserver' || dbType === 'oracle') return 'b'
  if (dbType === 'mongodb') return 'c'
  if (dbType === 'redis') return 'd'
  return 'a'
}

export function dbCapabilities(dbType?: DbType | null): DbCapabilities {
  const level = dbCapabilityLevel(dbType)
  if (dbType === 'mysql' || dbType === 'mariadb' || dbType === 'postgresql' || dbType === 'sqlite') {
    return {
      level,
      supports_sql: true,
      supports_schema_introspection: true,
      supports_import_export: true,
      supports_struct_sync: true,
      supports_data_sync: true,
    }
  }
  if (dbType === 'sqlserver' || dbType === 'oracle') {
    return {
      level,
      supports_sql: true,
      supports_schema_introspection: false,
      supports_import_export: false,
      supports_struct_sync: false,
      supports_data_sync: false,
    }
  }
  return {
    level,
    supports_sql: false,
    supports_schema_introspection: false,
    supports_import_export: false,
    supports_struct_sync: false,
    supports_data_sync: false,
  }
}

