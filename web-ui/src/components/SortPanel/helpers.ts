import type { SortRule, SortRulePayload } from './types'

const KEYWORD_BLACKLIST = [
  '--', '/*', '*/', ';',
  'union', 'select', 'insert', 'update', 'delete', 'drop', 'truncate',
  'into', 'from', 'where', 'exec', 'benchmark', 'sleep',
  'load_file', 'outfile', 'information_schema',
]

const ALLOWED_CHAR = /^[A-Za-z0-9_.,()*+\-/'"`\s]+$/

let __rid = 0
function genId(): string {
  __rid += 1
  return `r${Date.now().toString(36)}${__rid.toString(36)}`
}

export function createRule(over: Partial<SortRule> = {}): SortRule {
  return {
    id: genId(),
    kind: 'column',
    desc: false,
    nulls: 'default',
    ...over,
  }
}

function quoteIdent(col: string): string {
  return `\`${col.replace(/`/g, '``')}\``
}

/** Serialize the user-facing rule list to the wire format the backend expects. */
export function toOrdersPayload(rules: SortRule[]): string {
  if (rules.length === 0) return ''
  const payload: SortRulePayload[] = rules
    .filter(r => (r.kind === 'column' ? !!r.column : !!r.expression))
    .map(r => ({
      kind: r.kind,
      column: r.column,
      expression: r.expression,
      desc: r.desc,
      nulls: r.nulls,
    }))
  return JSON.stringify(payload)
}

/** Parse a JSON payload (e.g. from sessionStorage) back to rule list. Returns [] on error. */
export function fromOrdersPayload(json: string | undefined | null): SortRule[] {
  if (!json) return []
  try {
    const arr = JSON.parse(json) as Array<Partial<SortRule>>
    if (!Array.isArray(arr)) return []
    const out: SortRule[] = []
    for (const item of arr) {
      const kind: SortRule['kind'] = item.kind === 'expression' ? 'expression' : 'column'
      if (kind === 'column' && !item.column) continue
      if (kind === 'expression' && !item.expression) continue
      out.push({
        id: genId(),
        kind,
        column: item.column,
        expression: item.expression,
        desc: !!item.desc,
        nulls: item.nulls === 'first' || item.nulls === 'last' ? item.nulls : 'default',
      })
    }
    return out
  } catch {
    return []
  }
}

/** Build a `ORDER BY ...` SQL fragment for client-side SQL rewriting (query results). */
export function buildSqlOrderBy(rules: SortRule[]): string {
  const clauses: string[] = []
  for (const r of rules) {
    let target: string
    if (r.kind === 'column') {
      if (!r.column) continue
      target = quoteIdent(r.column)
    } else {
      if (!r.expression) continue
      if (validateExpressionClient(r.expression) !== null) continue
      target = r.expression
    }
    const dir = r.desc ? 'DESC' : 'ASC'
    if (r.nulls === 'first') {
      clauses.push(`${target} IS NULL DESC, ${target} ${dir}`)
    } else if (r.nulls === 'last') {
      clauses.push(`${target} IS NULL ASC, ${target} ${dir}`)
    } else {
      clauses.push(`${target} ${dir}`)
    }
  }
  return clauses.length === 0 ? '' : `ORDER BY ${clauses.join(', ')}`
}

/** Client-side expression validation mirroring backend sanitize_sort_expression. Returns null on ok, error message otherwise. */
export function validateExpressionClient(expr: string): string | null {
  if (!expr) return '表达式不能为空'
  if (expr.length > 200) return '表达式长度超过 200 字符上限'
  if (!ALLOWED_CHAR.test(expr)) return '表达式含非法字符'
  let depth = 0
  for (const ch of expr) {
    if (ch === '(') depth += 1
    else if (ch === ')') {
      depth -= 1
      if (depth < 0) return '圆括号不匹配'
    }
  }
  if (depth !== 0) return '圆括号不匹配'
  const lower = expr.toLowerCase()
  for (const kw of KEYWORD_BLACKLIST) {
    if (lower.includes(kw)) return `含禁用关键字: ${kw}`
  }
  return null
}
