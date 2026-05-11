import { describe, expect, it } from 'vitest'
import { getStatementKind, splitSqlStatements } from './sqlStatements'

describe('splitSqlStatements', () => {
  it('splits multiple statements by semicolon', () => {
    expect(splitSqlStatements('SELECT 1; SELECT 2;')).toEqual(['SELECT 1', 'SELECT 2'])
  })

  it('does not split semicolons inside strings or comments', () => {
    const sql = `
      SELECT 'a;b' AS val;
      -- keep;comment
      SELECT 2;
      /* block;comment */
      SELECT "x;y";
    `
    expect(splitSqlStatements(sql)).toEqual([
      "SELECT 'a;b' AS val",
      '-- keep;comment\n      SELECT 2',
      '/* block;comment */\n      SELECT "x;y"',
    ])
  })
})

describe('getStatementKind', () => {
  it('returns the first SQL keyword in uppercase', () => {
    expect(getStatementKind('  select * from users')).toBe('SELECT')
  })
})
