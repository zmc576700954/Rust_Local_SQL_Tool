import { describe, expect, it } from 'vitest'
import {
  dbCapabilities,
  dbCapabilityLevel,
  dbCapabilityNotice,
  dbCapabilityShortTag,
  dbTypeFromUrl,
} from './dbCapabilities'

describe('dbCapabilities', () => {
  it('marks only MySQL/MariaDB as level A with the full pipeline', () => {
    for (const dbType of ['mysql', 'mariadb'] as const) {
      expect(dbCapabilityLevel(dbType)).toBe('a')
      expect(dbCapabilities(dbType)).toEqual({
        level: 'a',
        supports_sql: true,
        supports_schema_introspection: true,
        supports_import_export: true,
        supports_struct_sync: true,
        supports_data_sync: true,
      })
      expect(dbCapabilityNotice(dbType)).toBeNull()
      expect(dbCapabilityShortTag(dbType)).toBeNull()
    }
  })

  it('marks PostgreSQL as partial with both structure and data sync wired', () => {
    expect(dbCapabilityLevel('postgresql')).toBe('b')
    expect(dbCapabilities('postgresql')).toEqual({
      level: 'b',
      supports_sql: true,
      supports_schema_introspection: true,
      supports_import_export: true,
      supports_struct_sync: true,
      supports_data_sync: true,
    })
    expect(dbCapabilityNotice('postgresql')).not.toBeNull()
    expect(dbCapabilityShortTag('postgresql')).toMatch(/部分支持|Partial/)
  })

  it('marks SQLite as partial: schema, transfer and structure sync wired, data sync not', () => {
    expect(dbCapabilityLevel('sqlite')).toBe('b')
    expect(dbCapabilities('sqlite')).toEqual({
      level: 'b',
      supports_sql: true,
      supports_schema_introspection: true,
      supports_import_export: true,
      supports_struct_sync: true,
      // Data sync stays false: no staged checkpoint/resume pipeline exists and
      // SQLite is single-writer.
      supports_data_sync: false,
    })
    expect(dbCapabilityNotice('sqlite')).not.toBeNull()
    expect(dbCapabilityShortTag('sqlite')).toMatch(/部分支持|Partial/)
  })

  it('marks SQL Server and Oracle as not connectable', () => {
    for (const dbType of ['sqlserver', 'oracle'] as const) {
      expect(dbCapabilityLevel(dbType)).toBe('d')
      const caps = dbCapabilities(dbType)
      expect(caps.supports_sql).toBe(false)
      expect(caps.supports_schema_introspection).toBe(false)
      expect(caps.supports_import_export).toBe(false)
      expect(caps.supports_struct_sync).toBe(false)
      expect(caps.supports_data_sync).toBe(false)
      expect(dbCapabilityShortTag(dbType)).toMatch(/未实现|Not implemented/)
    }
  })

  it('derives database type from connection URLs', () => {
    expect(dbTypeFromUrl('mysql://localhost:3306/app')).toBe('mysql')
    expect(dbTypeFromUrl('mariadb://localhost:3306/app')).toBe('mariadb')
    expect(dbTypeFromUrl('postgres://localhost/app')).toBe('postgresql')
    expect(dbTypeFromUrl('postgresql://localhost/app')).toBe('postgresql')
    expect(dbTypeFromUrl('sqlite:///tmp/app.db')).toBe('sqlite')
    expect(dbTypeFromUrl('sqlserver://localhost/app')).toBe('sqlserver')
    expect(dbTypeFromUrl('mongodb://localhost/app')).toBe('mongodb')
    expect(dbTypeFromUrl('redis://localhost:6379')).toBe('redis')
    expect(dbTypeFromUrl('oracle://localhost/app')).toBe('oracle')
    expect(dbTypeFromUrl('')).toBeNull()
    expect(dbTypeFromUrl(null)).toBeNull()
  })
})
