export function splitSqlStatements(sql: string): string[] {
  const statements: string[] = []
  let current = ''
  let inSingle = false
  let inDouble = false
  let inBacktick = false
  let inLineComment = false
  let inHashComment = false
  let inBlockComment = false

  for (let index = 0; index < sql.length; index += 1) {
    const char = sql[index]
    const next = sql[index + 1]
    const prev = sql[index - 1]

    if (inLineComment) {
      current += char
      if (char === '\n') {
        inLineComment = false
      }
      continue
    }

    if (inHashComment) {
      current += char
      if (char === '\n') {
        inHashComment = false
      }
      continue
    }

    if (inBlockComment) {
      current += char
      if (prev === '*' && char === '/') {
        inBlockComment = false
      }
      continue
    }

    if (!inSingle && !inDouble && !inBacktick) {
      if (char === '-' && next === '-') {
        inLineComment = true
        current += char
        continue
      }
      if (char === '#') {
        inHashComment = true
        current += char
        continue
      }
      if (char === '/' && next === '*') {
        inBlockComment = true
        current += char
        continue
      }
    }

    if (char === '\'' && !inDouble && !inBacktick && prev !== '\\') {
      inSingle = !inSingle
      current += char
      continue
    }

    if (char === '"' && !inSingle && !inBacktick && prev !== '\\') {
      inDouble = !inDouble
      current += char
      continue
    }

    if (char === '`' && !inSingle && !inDouble) {
      inBacktick = !inBacktick
      current += char
      continue
    }

    if (char === ';' && !inSingle && !inDouble && !inBacktick) {
      const trimmed = current.trim()
      if (trimmed) {
        statements.push(trimmed)
      }
      current = ''
      continue
    }

    current += char
  }

  const trailing = current.trim()
  if (trailing) {
    statements.push(trailing)
  }

  return statements
}

export function getStatementKind(sql: string): string {
  return sql.trim().split(/\s+/)[0]?.toUpperCase() || 'STATEMENT'
}

export function getStatementLabel(sql: string, index: number): string {
  return `${getStatementKind(sql)} ${index + 1}`
}

export function isPotentiallyDangerousSql(sql: string): boolean {
  const upperSql = sql.toUpperCase()
  return upperSql.includes('UPDATE ')
    || upperSql.includes('DELETE ')
    || upperSql.includes('DROP ')
    || upperSql.includes('TRUNCATE ')
    || upperSql.includes('ALTER ')
}
