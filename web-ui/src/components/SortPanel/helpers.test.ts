import { describe, it, expect } from 'vitest'
import {
  createRule,
  toOrdersPayload,
  fromOrdersPayload,
  buildSqlOrderBy,
  validateExpressionClient,
} from './helpers'
import type { SortRule } from './types'

const rule = (over: Partial<SortRule>): SortRule => ({
  id: 'r1',
  kind: 'column',
  column: 'a',
  desc: false,
  nulls: 'default',
  ...over,
})

describe('createRule', () => {
  it('generates a unique id and sensible defaults', () => {
    const a = createRule({ column: 'id' })
    const b = createRule({ column: 'id' })
    expect(a.id).not.toBe(b.id)
    expect(a.kind).toBe('column')
    expect(a.nulls).toBe('default')
    expect(a.desc).toBe(false)
  })
})

describe('toOrdersPayload / fromOrdersPayload', () => {
  it('round-trips column rules with nulls', () => {
    const rules: SortRule[] = [
      rule({ column: 'a', desc: false, nulls: 'first' }),
      rule({ id: 'r2', column: 'b', desc: true, nulls: 'last' }),
    ]
    const json = toOrdersPayload(rules)
    const parsed = fromOrdersPayload(json)
    expect(parsed.length).toBe(2)
    expect(parsed[0]).toMatchObject({ kind: 'column', column: 'a', desc: false, nulls: 'first' })
    expect(parsed[1]).toMatchObject({ kind: 'column', column: 'b', desc: true, nulls: 'last' })
  })

  it('round-trips expression rules', () => {
    const rules: SortRule[] = [
      rule({ kind: 'expression', column: undefined, expression: 'LENGTH(name)', desc: true }),
    ]
    const parsed = fromOrdersPayload(toOrdersPayload(rules))
    expect(parsed[0]).toMatchObject({ kind: 'expression', expression: 'LENGTH(name)', desc: true })
  })

  it('returns empty array for invalid JSON', () => {
    expect(fromOrdersPayload('not json')).toEqual([])
    expect(fromOrdersPayload(undefined)).toEqual([])
    expect(fromOrdersPayload('')).toEqual([])
  })

  it('skips invalid rules (no column and no expression)', () => {
    const json = JSON.stringify([{ kind: 'column', desc: false, nulls: 'default' }])
    expect(fromOrdersPayload(json)).toEqual([])
  })

  it('emits empty string when rule list is empty', () => {
    expect(toOrdersPayload([])).toBe('')
  })
})

describe('buildSqlOrderBy', () => {
  it('quotes column with backticks', () => {
    expect(buildSqlOrderBy([rule({ column: 'id' })])).toBe('ORDER BY `id` ASC')
  })

  it('emits nulls first emulation for ASC', () => {
    expect(buildSqlOrderBy([rule({ column: 'c', nulls: 'first' })]))
      .toBe('ORDER BY `c` IS NULL DESC, `c` ASC')
  })

  it('emits nulls last emulation for DESC', () => {
    expect(buildSqlOrderBy([rule({ column: 'c', desc: true, nulls: 'last' })]))
      .toBe('ORDER BY `c` IS NULL ASC, `c` DESC')
  })

  it('emits expression body verbatim', () => {
    const rules = [rule({ kind: 'expression', column: undefined, expression: 'LENGTH(name)', desc: true })]
    expect(buildSqlOrderBy(rules)).toBe('ORDER BY LENGTH(name) DESC')
  })

  it('joins multiple rules with comma', () => {
    expect(buildSqlOrderBy([rule({ column: 'a' }), rule({ id: 'r2', column: 'b', desc: true })]))
      .toBe('ORDER BY `a` ASC, `b` DESC')
  })

  it('returns empty for empty list', () => {
    expect(buildSqlOrderBy([])).toBe('')
  })

  it('escapes embedded backticks in column name', () => {
    expect(buildSqlOrderBy([rule({ column: 'a`b' })])).toBe('ORDER BY `a``b` ASC')
  })
})

describe('validateExpressionClient', () => {
  it('accepts safe expressions', () => {
    expect(validateExpressionClient('LENGTH(name)')).toBeNull()
    expect(validateExpressionClient('CAST(price AS DECIMAL(10,2))')).toBeNull()
  })

  it('rejects injections', () => {
    expect(validateExpressionClient('name; DROP TABLE x')).not.toBeNull()
    expect(validateExpressionClient('UNION SELECT *')).not.toBeNull()
  })

  it('rejects empty', () => {
    expect(validateExpressionClient('')).not.toBeNull()
  })

  it('rejects unbalanced parens', () => {
    expect(validateExpressionClient('LENGTH(name')).not.toBeNull()
  })
})
