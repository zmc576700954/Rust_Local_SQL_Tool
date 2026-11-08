import type { DbCapabilityLevel, DbType } from '../types'
import { tr } from '../i18n'

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

export function dbTypeFromUrl(url?: string | null): DbType | null {
  const lower = String(url || '').trim().toLowerCase()
  if (!lower) return null
  if (lower.startsWith('mariadb://')) return 'mariadb'
  if (lower.startsWith('mysql://')) return 'mysql'
  if (lower.startsWith('postgres://') || lower.startsWith('postgresql://')) return 'postgresql'
  if (lower.startsWith('sqlite:')) return 'sqlite'
  if (lower.startsWith('sqlserver://') || lower.startsWith('mssql://')) return 'sqlserver'
  if (lower.startsWith('mongodb://') || lower.startsWith('mongodb+srv://')) return 'mongodb'
  if (lower.startsWith('redis://') || lower.startsWith('rediss://')) return 'redis'
  if (lower.startsWith('oracle://')) return 'oracle'
  return null
}

// Mirrors core_lib::db::capability::capability_level — keep them in sync.
export function dbCapabilityLevel(dbType?: DbType | null): DbCapabilityLevel {
  if (dbType === 'mysql' || dbType === 'mariadb') return 'a'
  if (dbType === 'postgresql' || dbType === 'sqlite') return 'b'
  if (dbType === 'mongodb') return 'c'
  if (dbType === 'sqlserver' || dbType === 'oracle' || dbType === 'redis') return 'd'
  return 'a'
}

export function dbCapabilities(dbType?: DbType | null): DbCapabilities {
  const level = dbCapabilityLevel(dbType)
  if (dbType === 'mysql' || dbType === 'mariadb') {
    return {
      level,
      supports_sql: true,
      supports_schema_introspection: true,
      supports_import_export: true,
      supports_struct_sync: true,
      supports_data_sync: true,
    }
  }
  // PostgreSQL/SQLite: connect + non-transactional CRUD only.
  // SQL Server/Oracle/MongoDB/Redis: no working connection path.
  return {
    level,
    supports_sql: false,
    supports_schema_introspection: false,
    supports_import_export: false,
    supports_struct_sync: false,
    supports_data_sync: false,
  }
}

/** Short badge shown next to a database type, e.g. 实验性 / 未实现. */
export function dbCapabilityShortTag(dbType?: DbType | null): string | null {
  if (dbType === 'mysql' || dbType === 'mariadb') return null
  if (dbType === 'postgresql' || dbType === 'sqlite') return tr('实验性', 'Experimental')
  return tr('未实现', 'Not implemented')
}

/**
 * Human-readable warning for the real state of a database type.
 * Returns null for MySQL/MariaDB (the only fully wired engine).
 */
export function dbCapabilityNotice(dbType?: DbType | null): string | null {
  if (dbType === 'mysql' || dbType === 'mariadb') return null
  if (dbType === 'postgresql' || dbType === 'sqlite') {
    return tr(
      '实验性支持：当前仅可连接并使用基础 CRUD（数据表格编辑）；SQL 执行、结构浏览、DDL、导入导出与同步尚未接通。',
      'Experimental: connection and basic CRUD (data grid editing) only; SQL execution, schema browsing, DDL, import/export and sync are not wired yet.'
    )
  }
  if (dbType === 'sqlserver' || dbType === 'oracle') {
    return tr(
      '未实现：当前版本无法建立连接（后端返回 Unsupported）。',
      'Not implemented: this version cannot open a connection (backend returns Unsupported).'
    )
  }
  return tr(
    '未实现：当前版本仅支持 MySQL / MariaDB 全链路。',
    'Not implemented: only MySQL / MariaDB has a full pipeline in this version.'
  )
}
