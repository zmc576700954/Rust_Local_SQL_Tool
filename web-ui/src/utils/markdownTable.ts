export type MarkdownTableRow = Record<string, unknown>

export type MarkdownTableAsyncOptions = {
  /** Number of rows worth of text to render before yielding to the main thread. */
  rowsPerChunk?: number
  /** Override the yield strategy, mainly for tests. */
  yieldToMainThread?: () => Promise<void>
}

const NULL_CELL = 'NULL'
const EMPTY_STRING_CELL = '""'
const DEFAULT_ROWS_PER_CHUNK = 1000

function escapeMarkdownCell(text: string): string {
  return text
    .replace(/\\/g, '\\\\')
    .replace(/\r\n|\r|\n/g, '<br>')
    .replace(/\|/g, '\\|')
}

function stringifyCellValue(value: unknown): string {
  if (value === null || value === undefined) return NULL_CELL
  if (value === '') return EMPTY_STRING_CELL
  if (typeof value === 'bigint') return value.toString()
  if (value instanceof Date) return value.toISOString()
  if (typeof value === 'object') {
    const json = JSON.stringify(value)
    return json === undefined ? String(value) : json
  }
  return String(value)
}

/** Render a single value as a Markdown table cell. */
export function formatMarkdownCell(value: unknown): string {
  return escapeMarkdownCell(stringifyCellValue(value))
}

function buildTablePrefix(columns: readonly string[]): string[] {
  if (columns.length === 0) return []
  const header = `| ${columns.map((column) => escapeMarkdownCell(String(column))).join(' | ')} |`
  const divider = `| ${columns.map(() => '---').join(' | ')} |`
  return [header, divider]
}

function buildTableRow(columns: readonly string[], row: MarkdownTableRow): string {
  return `| ${columns.map((column) => formatMarkdownCell(row[column])).join(' | ')} |`
}

/** Build a Markdown table synchronously from the given columns and rows. */
export function buildMarkdownTable(
  columns: readonly string[],
  rows: readonly MarkdownTableRow[],
): string {
  if (columns.length === 0) return ''
  const lines = buildTablePrefix(columns)
  for (const row of rows) {
    lines.push(buildTableRow(columns, row))
  }
  return lines.join('\n')
}

function defaultYieldToMainThread(): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, 0))
}

/**
 * Build a Markdown table without blocking the main thread for large results.
 * Rows are rendered in chunks and the builder yields between chunks, so tens of
 * thousands of rows can be copied without freezing the UI.
 */
export async function buildMarkdownTableAsync(
  columns: readonly string[],
  rows: readonly MarkdownTableRow[],
  options: MarkdownTableAsyncOptions = {},
): Promise<string> {
  if (columns.length === 0) return ''
  const rowsPerChunk = Math.max(1, Math.floor(options.rowsPerChunk ?? DEFAULT_ROWS_PER_CHUNK))
  const yieldToMainThread = options.yieldToMainThread ?? defaultYieldToMainThread
  const lines = buildTablePrefix(columns)

  for (let index = 0; index < rows.length; index += 1) {
    lines.push(buildTableRow(columns, rows[index]))
    const renderedRows = index + 1
    if (renderedRows % rowsPerChunk === 0 && renderedRows < rows.length) {
      await yieldToMainThread()
    }
  }

  return lines.join('\n')
}
