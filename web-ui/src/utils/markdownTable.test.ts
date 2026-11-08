import { describe, expect, it, vi } from 'vitest'
import { buildMarkdownTable, buildMarkdownTableAsync, formatMarkdownCell } from './markdownTable'

describe('formatMarkdownCell', () => {
  it('escapes pipes inside values', () => {
    expect(formatMarkdownCell('left | right')).toBe('left \\| right')
  })

  it('escapes backslashes before pipes so the table structure stays valid', () => {
    expect(formatMarkdownCell('path\\to\\file')).toBe('path\\\\to\\\\file')
    expect(formatMarkdownCell('ends with \\')).toBe('ends with \\\\')
  })

  it('converts line breaks to <br>', () => {
    expect(formatMarkdownCell('line 1\nline 2\r\nline 3\rline 4')).toBe(
      'line 1<br>line 2<br>line 3<br>line 4',
    )
  })

  it('distinguishes NULL from an empty string', () => {
    expect(formatMarkdownCell(null)).toBe('NULL')
    expect(formatMarkdownCell(undefined)).toBe('NULL')
    expect(formatMarkdownCell('')).toBe('""')
  })

  it('keeps numbers, booleans, and dates unquoted', () => {
    expect(formatMarkdownCell(42)).toBe('42')
    expect(formatMarkdownCell(3.14)).toBe('3.14')
    expect(formatMarkdownCell(true)).toBe('true')
    expect(formatMarkdownCell(false)).toBe('false')
    expect(formatMarkdownCell(new Date('2026-01-02T03:04:05.000Z'))).toBe('2026-01-02T03:04:05.000Z')
    expect(formatMarkdownCell('2026-01-02')).toBe('2026-01-02')
  })

  it('renders objects as JSON', () => {
    expect(formatMarkdownCell({ a: 1 })).toBe('{"a":1}')
    expect(formatMarkdownCell('{"a":1}')).toBe('{"a":1}')
    expect(formatMarkdownCell({ a: 'x|y' })).toBe('{"a":"x\\|y"}')
  })
})

describe('buildMarkdownTable', () => {
  it('builds a header, divider, and one line per row', () => {
    const table = buildMarkdownTable(
      ['id', 'name'],
      [
        { id: 1, name: 'Alice' },
        { id: 2, name: 'Bob' },
      ],
    )

    expect(table).toBe(
      ['| id | name |', '| --- | --- |', '| 1 | Alice |', '| 2 | Bob |'].join('\n'),
    )
  })

  it('supports Chinese column names and escapes pipes in headers', () => {
    const table = buildMarkdownTable(
      ['用户ID', '姓名|别名'],
      [{ '用户ID': 1, '姓名|别名': '张三' }],
    )

    expect(table).toBe(
      ['| 用户ID | 姓名\\|别名 |', '| --- | --- |', '| 1 | 张三 |'].join('\n'),
    )
  })

  it('keeps NULL and empty strings distinct in rows', () => {
    const table = buildMarkdownTable(
      ['id', 'note', 'optional'],
      [
        { id: 1, note: '', optional: null },
        { id: 2, note: 'ok', optional: undefined },
      ],
    )

    expect(table).toBe(
      [
        '| id | note | optional |',
        '| --- | --- | --- |',
        '| 1 | "" | NULL |',
        '| 2 | ok | NULL |',
      ].join('\n'),
    )
  })

  it('converts multi-line values to <br> and escapes pipes', () => {
    const table = buildMarkdownTable(
      ['id', 'value'],
      [{ id: 1, value: 'a|b\nc' }],
    )

    expect(table).toBe(
      ['| id | value |', '| --- | --- |', '| 1 | a\\|b<br>c |'].join('\n'),
    )
  })

  it('returns an empty string when there are no columns', () => {
    expect(buildMarkdownTable([], [{ id: 1 }])).toBe('')
  })

  it('returns header and divider for an empty result set', () => {
    expect(buildMarkdownTable(['id'], [])).toBe(['| id |', '| --- |'].join('\n'))
  })
})

describe('buildMarkdownTableAsync', () => {
  it('produces the same table as the synchronous builder', async () => {
    const columns = ['id', '名称']
    const rows = [
      { id: 1, '名称': '甲' },
      { id: 2, '名称': '乙' },
      { id: 3, '名称': '丙' },
    ]

    await expect(buildMarkdownTableAsync(columns, rows)).resolves.toBe(
      buildMarkdownTable(columns, rows),
    )
  })

  it('yields between chunks instead of building everything in one task', async () => {
    const yieldToMainThread = vi.fn(async () => {})

    await buildMarkdownTableAsync(['id'], [{ id: 1 }, { id: 2 }, { id: 3 }], {
      rowsPerChunk: 1,
      yieldToMainThread,
    })

    expect(yieldToMainThread).toHaveBeenCalledTimes(2)
  })
})
