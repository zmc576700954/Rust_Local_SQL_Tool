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
  if (dbType === 'mysql' || dbType === 'mariadb' || dbType === 'postgresql') return 'a'
  if (dbType === 'sqlite') return 'b'
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
  if (dbType === 'postgresql') {
    // PG: SQL execution, schema browsing, DDL, import/export, structure sync,
    // staged data sync and the ops-center read-only scenarios (inspection,
    // performance diagnostics, index analysis) are wired. No ops writes.
    return {
      level,
      supports_sql: true,
      supports_schema_introspection: true,
      supports_import_export: true,
      supports_struct_sync: true,
      supports_data_sync: true,
    }
  }
  if (dbType === 'sqlite') {
    // SQLite: schema introspection, DDL (in-transaction table rebuild),
    // import/export and structure sync are wired. Data sync is not: there is no
    // staged checkpoint/resume pipeline and SQLite is single-writer.
    return {
      level,
      supports_sql: true,
      supports_schema_introspection: true,
      supports_import_export: true,
      supports_struct_sync: true,
      supports_data_sync: false,
    }
  }
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
  if (dbType === 'mysql' || dbType === 'mariadb' || dbType === 'postgresql') return null
  if (dbType === 'sqlite') return tr('部分支持', 'Partial')
  return tr('未实现', 'Not implemented')
}

/**
 * Human-readable warning for the real state of a database type.
 * Returns null for MySQL/MariaDB (no caveats); PostgreSQL is level A with
 * read-only ops caveats, so it keeps an explanatory notice.
 */
export function dbCapabilityNotice(dbType?: DbType | null): string | null {
  if (dbType === 'mysql' || dbType === 'mariadb') return null
  if (dbType === 'postgresql') {
    return tr(
      'PostgreSQL（Level A）：已支持连接、SQL 执行（含事务）、结构浏览、DDL、导入导出、结构同步（预览 + 确认后执行，破坏性变更需二次确认）、数据同步（分阶段流水线，mirror 会删除目标库多余数据，必须显式确认），以及运维中心的只读场景（定期巡检、性能诊断、索引优化只读分析）。运维中心不提供 apply，索引优化不生成可执行 DDL；level A 不含 PG 字段治理（写路径仍是 MySQL 方言）。pg_stat_statements 未预加载、track_counts=off 等实例配置缺失时按项降级并给出原因。',
      'PostgreSQL (Level A): connection, SQL execution (with transactions), schema browsing, DDL, import/export, structure sync (preview + confirm; destructive changes require a second confirmation), data sync (staged pipeline; mirror deletes extra target rows and requires explicit confirmation), and the ops-center read-only scenarios (periodic inspection, performance diagnostics, read-only index analysis) are supported. Ops Center provides no apply and the index advisor emits no executable DDL; level A excludes PG field governance (its write path is MySQL-specific). Missing instance configuration (pg_stat_statements not preloaded, track_counts=off, etc.) degrades per item with a readable reason.'
    )
  }
  if (dbType === 'sqlite') {
    return tr(
      'SQLite：已支持连接、SQL 执行、结构浏览、DDL（事务内表重建）、导入导出与结构同步（破坏性变更需二次确认）。数据同步尚未支持：没有分阶段 checkpoint/续跑流水线，且 SQLite 为单写者引擎。',
      'SQLite: connection, SQL execution, schema browsing, DDL (in-transaction table rebuild), import/export and structure sync (destructive changes require a second confirmation) are supported. Data sync is not supported: there is no staged checkpoint/resume pipeline and SQLite is single-writer.'
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
