import { useEffect, useMemo, useState } from 'react'
import { Activity, Database, Download, Play, RefreshCw } from 'lucide-react'
import { api } from '../api'
import { parseError } from '../utils'
import type { ConfigData, DbConnection, SchemaResponse } from '../types'

type PerfProbeOperation =
  | 'connect_cold'
  | 'connect_warm'
  | 'query_select_small'
  | 'query_write_small'
  | 'explain_plan'
  | 'catalog_first_paint'
  | 'cancel_latency'
  | 'table_first_page'

type PerfBudget = {
  operation?: string
  target_p50_ms?: number | null
  target_p95_ms?: number | null
  source?: string | null
}

type PerfSample = {
  operation?: string
  iteration?: number
  duration_ms?: number
  rows?: number | null
}

type PerfProbeSummary = {
  operation?: string
  sample_count?: number
  min_ms?: number
  max_ms?: number
  avg_ms?: number
  p50_ms?: number
  p95_ms?: number
  rows?: number | null
  budget?: PerfBudget | null
  samples?: PerfSample[]
}

type PerfHistoryEntry = {
  id: string
  recorded_at: string
  connection_id?: string | null
  connection_name?: string | null
  operation: PerfProbeOperation
  iterations: number
  sql?: string | null
  table_name?: string | null
  result: PerfProbeSummary
}

type PerfSuiteReport = {
  id: string
  recorded_at: string
  connection_id?: string | null
  connection_name?: string | null
  label?: string | null
  build_version?: string | null
  branch_name?: string | null
  environment?: string | null
  notes?: string | null
  iterations: number
  sql?: string | null
  table_name?: string | null
  status: 'success' | 'failed'
  failed_operation?: PerfProbeOperation | null
  error?: string | null
  archive_path?: string | null
  results: PerfHistoryEntry[]
}

type PerfSuiteMetaDraft = {
  label: string
  buildVersion: string
  branchName: string
  environment: string
  notes: string
}

type PerfSuiteComparisonRow = {
  operation: PerfProbeOperation
  current: PerfProbeSummary
  baseline: PerfProbeSummary
  p50: ReturnType<typeof comparisonDelta>
  p95: ReturnType<typeof comparisonDelta>
  avg: ReturnType<typeof comparisonDelta>
}

type PerfGateSummary = {
  status: 'pass' | 'warn' | 'fail'
  budgetFailCount: number
  regressionCount: number
  comparedCount: number
  baselineScope: 'pinned' | 'adhoc' | 'none'
  message: string
}

type PerfSuiteDiffArchiveRecord = {
  id: string
  recorded_at: string
  current_suite_id: string
  baseline_suite_id: string
  current_suite_label?: string | null
  baseline_suite_label?: string | null
  gate_status?: string | null
  baseline_scope?: string | null
  archive_path?: string | null
}

type PerfSuiteDiffStatusFilter = 'all' | 'pass' | 'warn' | 'fail'

const PERF_HISTORY_STORAGE_KEY = 'perf-diagnostics-history-v1'
const PERF_SUITE_STORAGE_KEY = 'perf-diagnostics-suite-history-v1'
const PERF_SUITE_META_STORAGE_KEY = 'perf-diagnostics-suite-meta-v1'
const PERF_SUITE_PINNED_STORAGE_KEY = 'perf-diagnostics-suite-pinned-v1'
const PERF_HISTORY_LIMIT = 20
const PERF_SUITE_LIMIT = 10
const FULL_SUITE_OPERATIONS: PerfProbeOperation[] = [
  'connect_cold',
  'connect_warm',
  'query_select_small',
  'query_write_small',
  'explain_plan',
  'catalog_first_paint',
  'table_first_page',
  'cancel_latency',
]
const SUITE_LABEL_PRESETS = [
  { label: 'Before Opt', value: 'before optimization' },
  { label: 'After Opt', value: 'after optimization' },
  { label: 'RC', value: 'release candidate' },
  { label: 'Nightly', value: 'nightly' },
]
const SUITE_DIFF_STATUS_FILTERS: Array<{ value: PerfSuiteDiffStatusFilter; label: string }> = [
  { value: 'all', label: 'All' },
  { value: 'pass', label: 'PASS' },
  { value: 'warn', label: 'WARN' },
  { value: 'fail', label: 'FAIL' },
]

function suiteDiffStatusPriority(status?: string | null) {
  switch ((status || '').toLowerCase()) {
    case 'fail':
      return 0
    case 'warn':
      return 1
    case 'pass':
      return 2
    default:
      return 3
  }
}

async function copyTextToClipboard(text: string) {
  if (!text) return false
  try {
    if (typeof navigator !== 'undefined' && navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(text)
      return true
    }
  } catch {
    // fall through
  }
  return false
}

const OPERATION_OPTIONS: Array<{
  value: PerfProbeOperation
  label: string
  description: string
}> = [
  {
    value: 'connect_cold',
    label: 'connect_cold',
    description: 'Measure a fresh pool open + first ping without reusing the connection registry cache.',
  },
  {
    value: 'connect_warm',
    label: 'connect_warm',
    description: 'Measure warm connection fetch + ping through the current runtime path.',
  },
  {
    value: 'query_select_small',
    label: 'query_select_small',
    description: 'Measure a small read-only SQL query on a warm connection.',
  },
  {
    value: 'query_write_small',
    label: 'query_write_small',
    description: 'Measure a small safe write on a session-local temporary table without touching user tables.',
  },
  {
    value: 'explain_plan',
    label: 'explain_plan',
    description: 'Measure EXPLAIN plan generation for the current read-only SQL on a warm connection.',
  },
  {
    value: 'catalog_first_paint',
    label: 'catalog_first_paint',
    description: 'Measure first schema tree paint after metadata cache reset.',
  },
  {
    value: 'cancel_latency',
    label: 'cancel_latency',
    description: 'Measure end-to-end query cancel latency using a long-running probe query and real KILL QUERY handling.',
  },
  {
    value: 'table_first_page',
    label: 'table_first_page',
    description: 'Measure first-page metadata + row fetch + JSON serialization.',
  },
]

function metricValue(value?: number | null) {
  if (typeof value !== 'number' || Number.isNaN(value)) return '-'
  return `${value} ms`
}

function metricDelta(actual?: number | null, target?: number | null) {
  if (typeof actual !== 'number' || typeof target !== 'number' || Number.isNaN(actual) || Number.isNaN(target)) {
    return ''
  }
  const delta = actual - target
  const sign = delta > 0 ? '+' : ''
  return `${sign}${delta} ms vs budget`
}

function metricBudgetStatus(actual?: number | null, target?: number | null) {
  if (typeof actual !== 'number' || typeof target !== 'number' || Number.isNaN(actual) || Number.isNaN(target)) {
    return 'na' as const
  }
  return actual <= target ? ('pass' as const) : ('fail' as const)
}

function comparisonDelta(current?: number | null, baseline?: number | null) {
  if (
    typeof current !== 'number' ||
    typeof baseline !== 'number' ||
    Number.isNaN(current) ||
    Number.isNaN(baseline)
  ) {
    return {
      value: '-',
      status: 'na' as const,
      detail: '',
    }
  }
  const delta = current - baseline
  const sign = delta > 0 ? '+' : ''
  return {
    value: `${sign}${delta} ms`,
    status: delta <= 0 ? ('pass' as const) : ('fail' as const),
    detail: delta <= 0 ? 'Faster or equal to baseline' : 'Slower than baseline',
  }
}

function formatRecordedAt(value?: string | null) {
  if (!value) return '-'
  const date = new Date(value)
  if (Number.isNaN(date.getTime())) return value
  return date.toLocaleString()
}

function parsePerfHistoryEntry(item: unknown): PerfHistoryEntry | null {
  if (!item || typeof item !== 'object') return null
  const record = item as Record<string, unknown>
  if (typeof record.id !== 'string' || typeof record.operation !== 'string' || !record.result) {
    return null
  }
  return {
    id: record.id,
    recorded_at: typeof record.recorded_at === 'string' ? record.recorded_at : new Date().toISOString(),
    connection_id: typeof record.connection_id === 'string' ? record.connection_id : null,
    connection_name: typeof record.connection_name === 'string' ? record.connection_name : null,
    operation: record.operation as PerfProbeOperation,
    iterations: typeof record.iterations === 'number' ? record.iterations : 1,
    sql: typeof record.sql === 'string' ? record.sql : null,
    table_name: typeof record.table_name === 'string' ? record.table_name : null,
    result: record.result as PerfProbeSummary,
  }
}

function loadPerfHistory(): PerfHistoryEntry[] {
  try {
    const raw = window.localStorage.getItem(PERF_HISTORY_STORAGE_KEY)
    if (!raw) return []
    const parsed = JSON.parse(raw)
    if (!Array.isArray(parsed)) return []
    return parsed.map(parsePerfHistoryEntry).filter(Boolean).slice(0, PERF_HISTORY_LIMIT) as PerfHistoryEntry[]
  } catch {
    return []
  }
}

function savePerfHistory(entries: PerfHistoryEntry[]) {
  try {
    window.localStorage.setItem(PERF_HISTORY_STORAGE_KEY, JSON.stringify(entries.slice(0, PERF_HISTORY_LIMIT)))
  } catch {
    // ignore local persistence failures
  }
}

function normalizeOptionalString(value: unknown) {
  return typeof value === 'string' && value.trim() ? value : null
}

function parsePerfSuiteReport(item: unknown): PerfSuiteReport | null {
  if (!item || typeof item !== 'object') return null
  const record = item as Record<string, unknown>
  if (typeof record.id !== 'string' || !Array.isArray(record.results)) {
    return null
  }
  return {
    id: record.id,
    recorded_at: typeof record.recorded_at === 'string' ? record.recorded_at : new Date().toISOString(),
    connection_id: typeof record.connection_id === 'string' ? record.connection_id : null,
    connection_name: typeof record.connection_name === 'string' ? record.connection_name : null,
    label: normalizeOptionalString(record.label),
    build_version: normalizeOptionalString(record.build_version),
    branch_name: normalizeOptionalString(record.branch_name),
    environment: normalizeOptionalString(record.environment),
    notes: normalizeOptionalString(record.notes),
    iterations: typeof record.iterations === 'number' ? record.iterations : 1,
    sql: typeof record.sql === 'string' ? record.sql : null,
    table_name: typeof record.table_name === 'string' ? record.table_name : null,
    status: record.status === 'failed' ? 'failed' : 'success',
    failed_operation: typeof record.failed_operation === 'string'
      ? (record.failed_operation as PerfProbeOperation)
      : null,
    error: typeof record.error === 'string' ? record.error : null,
    archive_path: typeof record.archive_path === 'string' ? record.archive_path : null,
    results: record.results.map(parsePerfHistoryEntry).filter(Boolean) as PerfHistoryEntry[],
  }
}

function normalizePerfSuiteHistory(items: unknown): PerfSuiteReport[] {
  if (!Array.isArray(items)) return []
  return items.map(parsePerfSuiteReport).filter(Boolean).slice(0, PERF_SUITE_LIMIT) as PerfSuiteReport[]
}

function mergePerfSuiteHistory(primary: PerfSuiteReport[], secondary: PerfSuiteReport[]) {
  const merged: PerfSuiteReport[] = []
  for (const entry of [...primary, ...secondary]) {
    if (!merged.some((item) => item.id === entry.id)) {
      merged.push(entry)
    }
    if (merged.length >= PERF_SUITE_LIMIT) {
      break
    }
  }
  return merged
}

function loadPerfSuiteHistory(): PerfSuiteReport[] {
  try {
    const raw = window.localStorage.getItem(PERF_SUITE_STORAGE_KEY)
    if (!raw) return []
    const parsed = JSON.parse(raw)
    return normalizePerfSuiteHistory(parsed)
  } catch {
    return []
  }
}

function savePerfSuiteHistory(entries: PerfSuiteReport[]) {
  try {
    window.localStorage.setItem(PERF_SUITE_STORAGE_KEY, JSON.stringify(entries.slice(0, PERF_SUITE_LIMIT)))
  } catch {
    // ignore local persistence failures
  }
}

function loadPerfSuiteMetaDraft(): PerfSuiteMetaDraft {
  try {
    const raw = window.localStorage.getItem(PERF_SUITE_META_STORAGE_KEY)
    if (!raw) {
      return {
        label: '',
        buildVersion: '',
        branchName: '',
        environment: '',
        notes: '',
      }
    }
    const parsed = JSON.parse(raw) as Record<string, unknown>
    return {
      label: typeof parsed.label === 'string' ? parsed.label : '',
      buildVersion: typeof parsed.buildVersion === 'string' ? parsed.buildVersion : '',
      branchName: typeof parsed.branchName === 'string' ? parsed.branchName : '',
      environment: typeof parsed.environment === 'string' ? parsed.environment : '',
      notes: typeof parsed.notes === 'string' ? parsed.notes : '',
    }
  } catch {
    return {
      label: '',
      buildVersion: '',
      branchName: '',
      environment: '',
      notes: '',
    }
  }
}

function savePerfSuiteMetaDraft(draft: PerfSuiteMetaDraft) {
  try {
    window.localStorage.setItem(PERF_SUITE_META_STORAGE_KEY, JSON.stringify(draft))
  } catch {
    // ignore local persistence failures
  }
}

function loadPinnedPerfSuiteBaseline(): PerfSuiteReport | null {
  try {
    const raw = window.localStorage.getItem(PERF_SUITE_PINNED_STORAGE_KEY)
    if (!raw) return null
    return parsePerfSuiteReport(JSON.parse(raw))
  } catch {
    return null
  }
}

function savePinnedPerfSuiteBaseline(report: PerfSuiteReport | null) {
  try {
    if (!report) {
      window.localStorage.removeItem(PERF_SUITE_PINNED_STORAGE_KEY)
      return
    }
    window.localStorage.setItem(PERF_SUITE_PINNED_STORAGE_KEY, JSON.stringify(report))
  } catch {
    // ignore local persistence failures
  }
}

function parsePerfSuiteDiffArchiveRecord(item: unknown): PerfSuiteDiffArchiveRecord | null {
  if (!item || typeof item !== 'object') return null
  const record = item as Record<string, unknown>
  if (
    typeof record.id !== 'string' ||
    typeof record.recorded_at !== 'string' ||
    typeof record.current_suite_id !== 'string' ||
    typeof record.baseline_suite_id !== 'string'
  ) {
    return null
  }
  return {
    id: record.id,
    recorded_at: record.recorded_at,
    current_suite_id: record.current_suite_id,
    baseline_suite_id: record.baseline_suite_id,
    current_suite_label: normalizeOptionalString(record.current_suite_label),
    baseline_suite_label: normalizeOptionalString(record.baseline_suite_label),
    gate_status: normalizeOptionalString(record.gate_status),
    baseline_scope: normalizeOptionalString(record.baseline_scope),
    archive_path: normalizeOptionalString(record.archive_path),
  }
}

function buildSuiteBudgetSummary(report: PerfSuiteReport | null) {
  if (!report) return null
  const budgetedResults = report.results.filter(
    (entry) =>
      typeof entry.result.budget?.target_p50_ms === 'number' ||
      typeof entry.result.budget?.target_p95_ms === 'number'
  )
  const passCount = budgetedResults.filter((entry) => {
    const p50Ok =
      typeof entry.result.budget?.target_p50_ms !== 'number' ||
      (typeof entry.result.p50_ms === 'number' && entry.result.p50_ms <= entry.result.budget.target_p50_ms)
    const p95Ok =
      typeof entry.result.budget?.target_p95_ms !== 'number' ||
      (typeof entry.result.p95_ms === 'number' && entry.result.p95_ms <= entry.result.budget.target_p95_ms)
    return p50Ok && p95Ok
  }).length
  return {
    passCount,
    failCount: Math.max(0, budgetedResults.length - passCount),
    totalCount: budgetedResults.length,
  }
}

function buildProbePayload(
  probeOperation: PerfProbeOperation,
  selectedDbId: string,
  iterations: number,
  sql: string,
  tableName: string
) {
  const payload: Record<string, unknown> = {
    operation: probeOperation,
    db_id: selectedDbId || undefined,
    iterations,
  }
  if (probeOperation === 'query_select_small' || probeOperation === 'explain_plan') {
    payload.sql = sql
  }
  if (probeOperation === 'table_first_page') {
    payload.table_name = tableName
  }
  return payload
}

function createHistoryEntry(params: {
  operation: PerfProbeOperation
  selectedDbId: string
  selectedConnectionName?: string | null
  iterations: number
  sql: string
  tableName: string
  result: PerfProbeSummary
}): PerfHistoryEntry {
  const { operation, selectedDbId, selectedConnectionName, iterations, sql, tableName, result } = params
  return {
    id: `${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
    recorded_at: new Date().toISOString(),
    connection_id: selectedDbId || null,
    connection_name: selectedConnectionName || null,
    operation,
    iterations,
    sql: operation === 'query_select_small' || operation === 'explain_plan' ? sql : null,
    table_name: operation === 'table_first_page' ? tableName : null,
    result,
  }
}

async function syncPerfSuiteHistoryToArchive(nextHistory: PerfSuiteReport[]) {
  const normalized = nextHistory.slice(0, PERF_SUITE_LIMIT)
  try {
    const saved = await api.savePerfSuite(normalized[0])
    const remoteSaved = parsePerfSuiteReport(saved)
    const remoteHistory = normalizePerfSuiteHistory(await api.listPerfSuites(PERF_SUITE_LIMIT))
    const merged = mergePerfSuiteHistory(
      remoteSaved ? [remoteSaved, ...remoteHistory] : remoteHistory,
      normalized
    )
    savePerfSuiteHistory(merged)
    return {
      history: merged,
      mode: 'backend' as const,
    }
  } catch {
    savePerfSuiteHistory(normalized)
    return {
      history: normalized,
      mode: 'local' as const,
    }
  }
}

export function PerfDiagnosticsPanel({
  configData,
  schemaData,
  isActive,
}: {
  configData: ConfigData | null
  schemaData: SchemaResponse | null
  isActive: boolean
}) {
  const connections = useMemo(() => {
    const list = Array.isArray(configData?.db_connections) ? configData?.db_connections : []
    return list as DbConnection[]
  }, [configData])

  const activeDbId = String(configData?.active_db_id || connections[0]?.id || '')
  const [selectedDbId, setSelectedDbId] = useState(activeDbId)
  const [operation, setOperation] = useState<PerfProbeOperation>('connect_warm')
  const [iterations, setIterations] = useState(10)
  const [sql, setSql] = useState('SELECT 1 AS perf_probe')
  const [tableName, setTableName] = useState('')
  const [tables, setTables] = useState<string[]>([])
  const [isLoadingTables, setIsLoadingTables] = useState(false)
  const [isRunning, setIsRunning] = useState(false)
  const [error, setError] = useState('')
  const [historyRuns, setHistoryRuns] = useState<PerfHistoryEntry[]>([])
  const [suiteHistory, setSuiteHistory] = useState<PerfSuiteReport[]>([])
  const [suiteArchiveMode, setSuiteArchiveMode] = useState<'backend' | 'local'>('local')
  const [archivedSuiteDiff, setArchivedSuiteDiff] = useState<PerfSuiteDiffArchiveRecord | null>(null)
  const [suiteDiffHistory, setSuiteDiffHistory] = useState<PerfSuiteDiffArchiveRecord[]>([])
  const [suiteDiffStatusFilter, setSuiteDiffStatusFilter] = useState<PerfSuiteDiffStatusFilter>('all')
  const [suiteDiffPinnedOnly, setSuiteDiffPinnedOnly] = useState(false)
  const [suiteDiffCurrentBaselineOnly, setSuiteDiffCurrentBaselineOnly] = useState(false)
  const [suiteDiffSearchQuery, setSuiteDiffSearchQuery] = useState('')
  const [expandedSuiteDiffId, setExpandedSuiteDiffId] = useState('')
  const [isLoadingSuiteDiffHistory, setIsLoadingSuiteDiffHistory] = useState(false)
  const [isArchivingSuiteDiff, setIsArchivingSuiteDiff] = useState(false)
  const [pinnedSuiteBaseline, setPinnedSuiteBaseline] = useState<PerfSuiteReport | null>(null)
  const [isPinningSuiteBaseline, setIsPinningSuiteBaseline] = useState(false)
  const [suiteLabel, setSuiteLabel] = useState('')
  const [suiteBuildVersion, setSuiteBuildVersion] = useState('')
  const [suiteBranchName, setSuiteBranchName] = useState('')
  const [suiteEnvironment, setSuiteEnvironment] = useState('')
  const [suiteNotes, setSuiteNotes] = useState('')
  const [selectedHistoryId, setSelectedHistoryId] = useState('')
  const [compareBaselineId, setCompareBaselineId] = useState('')
  const [suiteReport, setSuiteReport] = useState<PerfSuiteReport | null>(null)
  const [compareSuiteId, setCompareSuiteId] = useState('')
  const [sameConnectionOnly, setSameConnectionOnly] = useState(true)
  const [sameBuildVersionOnly, setSameBuildVersionOnly] = useState(false)
  const [suiteProgress, setSuiteProgress] = useState<{
    current: number
    total: number
    operation: PerfProbeOperation
  } | null>(null)

  const selectedConnection = useMemo(
    () => connections.find((conn) => String(conn.id) === String(selectedDbId)) || null,
    [connections, selectedDbId]
  )

  const currentOperation = useMemo(
    () => OPERATION_OPTIONS.find((item) => item.value === operation) || OPERATION_OPTIONS[0],
    [operation]
  )

  const activeHistoryEntry = useMemo(
    () => historyRuns.find((entry) => entry.id === selectedHistoryId) || null,
    [historyRuns, selectedHistoryId]
  )

  const result = activeHistoryEntry?.result || null

  const budgetComparison = useMemo(() => {
    if (!result) return null
    return {
      p50Status: metricBudgetStatus(result.p50_ms, result.budget?.target_p50_ms),
      p95Status: metricBudgetStatus(result.p95_ms, result.budget?.target_p95_ms),
      p50Delta: metricDelta(result.p50_ms, result.budget?.target_p50_ms),
      p95Delta: metricDelta(result.p95_ms, result.budget?.target_p95_ms),
    }
  }, [result])

  const compareCandidates = useMemo(() => {
    if (!activeHistoryEntry) return []
    return historyRuns.filter(
      (entry) => entry.id !== activeHistoryEntry.id && entry.operation === activeHistoryEntry.operation
    )
  }, [activeHistoryEntry, historyRuns])

  const compareBaseline = useMemo(
    () => compareCandidates.find((entry) => entry.id === compareBaselineId) || null,
    [compareBaselineId, compareCandidates]
  )

  const needsSql = operation === 'query_select_small' || operation === 'explain_plan'
  const needsTable = operation === 'table_first_page'
  const sqlLabel = operation === 'explain_plan' ? 'Read-only SQL to EXPLAIN' : 'Read-only SQL'
  const canRunFullSuite = Boolean(selectedDbId || connections.length === 0) && Boolean(tableName.trim())

  const suiteBudgetSummary = useMemo(() => buildSuiteBudgetSummary(suiteReport), [suiteReport])

  const suiteCompareCandidates = useMemo(() => {
    if (!suiteReport) return []
    return suiteHistory.filter(
      (report) =>
        report.id !== suiteReport.id &&
        (!sameConnectionOnly ||
          !suiteReport.connection_id ||
          (!!report.connection_id && report.connection_id === suiteReport.connection_id)) &&
        (!sameBuildVersionOnly || report.build_version === suiteReport.build_version)
    )
  }, [sameBuildVersionOnly, sameConnectionOnly, suiteHistory, suiteReport])

  const compareSuiteReport = useMemo(
    () => suiteCompareCandidates.find((report) => report.id === compareSuiteId) || null,
    [compareSuiteId, suiteCompareCandidates]
  )
  const pinnedSuiteBaselineId = pinnedSuiteBaseline?.id || ''
  const isPinnedSuiteSelected = Boolean(suiteReport && pinnedSuiteBaselineId && suiteReport.id === pinnedSuiteBaselineId)

  const suiteComparisonRows = useMemo(() => {
    if (!suiteReport || !compareSuiteReport) return []
    return FULL_SUITE_OPERATIONS.map((probeOperation) => {
      const current = suiteReport.results.find((entry) => entry.operation === probeOperation)?.result
      const baseline = compareSuiteReport.results.find((entry) => entry.operation === probeOperation)?.result
      if (!current || !baseline) return null
      return {
        operation: probeOperation,
        current,
        baseline,
        p50: comparisonDelta(current.p50_ms, baseline.p50_ms),
        p95: comparisonDelta(current.p95_ms, baseline.p95_ms),
        avg: comparisonDelta(current.avg_ms, baseline.avg_ms),
      }
    }).filter(Boolean) as PerfSuiteComparisonRow[]
  }, [compareSuiteReport, suiteReport])

  const suiteComparisonSummary = useMemo(() => {
    const comparableRows = suiteComparisonRows.filter((row) => row.p50.status !== 'na')
    return {
      fasterCount: comparableRows.filter((row) => row.p50.status === 'pass').length,
      slowerCount: comparableRows.filter((row) => row.p50.status === 'fail').length,
      comparableCount: comparableRows.length,
    }
  }, [suiteComparisonRows])

  const suiteGateSummary = useMemo<PerfGateSummary | null>(() => {
    if (!suiteReport) return null
    const budgetFailCount = suiteBudgetSummary?.failCount || 0
    const regressionCount = suiteComparisonRows.filter(
      (row) => row.p50.status === 'fail' || row.p95.status === 'fail'
    ).length
    const baselineScope: PerfGateSummary['baselineScope'] = compareSuiteReport
      ? compareSuiteReport.id === pinnedSuiteBaselineId
        ? 'pinned'
        : 'adhoc'
      : 'none'

    if (suiteReport.status === 'failed') {
      return {
        status: 'fail',
        budgetFailCount,
        regressionCount,
        comparedCount: suiteComparisonRows.length,
        baselineScope,
        message: 'Suite execution failed before perf gate could pass.',
      }
    }

    if (budgetFailCount > 0) {
      return {
        status: 'fail',
        budgetFailCount,
        regressionCount,
        comparedCount: suiteComparisonRows.length,
        baselineScope,
        message: `${budgetFailCount} budgeted operation(s) are over the current target.`,
      }
    }

    if (baselineScope === 'pinned' && regressionCount > 0) {
      return {
        status: 'fail',
        budgetFailCount,
        regressionCount,
        comparedCount: suiteComparisonRows.length,
        baselineScope,
        message: `${regressionCount} operation(s) regressed against the locked baseline.`,
      }
    }

    if (pinnedSuiteBaselineId && baselineScope === 'none') {
      return {
        status: 'warn',
        budgetFailCount,
        regressionCount,
        comparedCount: suiteComparisonRows.length,
        baselineScope,
        message: 'A locked baseline exists but is not comparable under the current filters.',
      }
    }

    if (regressionCount > 0) {
      return {
        status: 'warn',
        budgetFailCount,
        regressionCount,
        comparedCount: suiteComparisonRows.length,
        baselineScope,
        message: `${regressionCount} operation(s) are slower than the selected comparison baseline.`,
      }
    }

    return {
      status: 'pass',
      budgetFailCount,
      regressionCount,
      comparedCount: suiteComparisonRows.length,
      baselineScope,
      message: baselineScope === 'pinned'
        ? 'All budgeted operations passed and no regression was found versus the locked baseline.'
        : 'All visible perf gate checks passed for this suite.',
    }
  }, [
    compareSuiteReport,
    pinnedSuiteBaselineId,
    suiteBudgetSummary,
    suiteComparisonRows,
    suiteReport,
  ])

  const suiteDiffPayload = useMemo(() => {
    if (!suiteReport || !compareSuiteReport || !suiteGateSummary) return null
    return {
      id: `diff-${suiteReport.id}-${compareSuiteReport.id}-${Date.now()}`,
      recorded_at: new Date().toISOString(),
      current_suite_id: suiteReport.id,
      baseline_suite_id: compareSuiteReport.id,
      current_suite_label: suiteReport.label || null,
      baseline_suite_label: compareSuiteReport.label || null,
      gate_status: suiteGateSummary.status,
      baseline_scope: suiteGateSummary.baselineScope,
      current_suite: suiteReport,
      baseline_suite: compareSuiteReport,
      gate: suiteGateSummary,
      summary: suiteComparisonSummary,
      rows: suiteComparisonRows,
    }
  }, [
    compareSuiteReport,
    suiteComparisonRows,
    suiteComparisonSummary,
    suiteGateSummary,
    suiteReport,
  ])

  const filteredSuiteDiffHistory = useMemo(() => {
    const query = suiteDiffSearchQuery.trim().toLowerCase()
    return [...suiteDiffHistory]
      .filter((item) => {
        if (suiteDiffPinnedOnly && item.baseline_scope !== 'pinned') {
          return false
        }
        if (
          suiteDiffCurrentBaselineOnly &&
          compareSuiteReport &&
          item.baseline_suite_id !== compareSuiteReport.id
        ) {
          return false
        }
        if (suiteDiffStatusFilter !== 'all' && (item.gate_status || '').toLowerCase() !== suiteDiffStatusFilter) {
          return false
        }
        if (query) {
          const haystack = [
            item.baseline_suite_label,
            item.current_suite_label,
            item.baseline_suite_id,
            item.current_suite_id,
            item.archive_path,
          ]
            .filter(Boolean)
            .join(' ')
            .toLowerCase()
          if (!haystack.includes(query)) {
            return false
          }
        }
        return true
      })
      .sort((a, b) => {
        const statusOrder = suiteDiffStatusPriority(a.gate_status) - suiteDiffStatusPriority(b.gate_status)
        if (statusOrder !== 0) return statusOrder
        return String(b.recorded_at).localeCompare(String(a.recorded_at))
      })
  }, [
    compareSuiteReport,
    suiteDiffCurrentBaselineOnly,
    suiteDiffHistory,
    suiteDiffPinnedOnly,
    suiteDiffSearchQuery,
    suiteDiffStatusFilter,
  ])

  const hasActiveSuiteDiffFilters =
    suiteDiffStatusFilter !== 'all' ||
    suiteDiffPinnedOnly ||
    suiteDiffCurrentBaselineOnly ||
    suiteDiffSearchQuery.trim().length > 0

  const activeSuiteDiffFilterTags = useMemo(() => {
    const tags: string[] = []
    if (suiteDiffStatusFilter !== 'all') {
      tags.push(`status:${suiteDiffStatusFilter.toUpperCase()}`)
    }
    if (suiteDiffPinnedOnly) {
      tags.push('pinned')
    }
    if (suiteDiffCurrentBaselineOnly && compareSuiteReport) {
      tags.push(`baseline:${compareSuiteReport.label || compareSuiteReport.id}`)
    }
    const query = suiteDiffSearchQuery.trim()
    if (query) {
      tags.push(`search:${query}`)
    }
    return tags
  }, [
    compareSuiteReport,
    suiteDiffCurrentBaselineOnly,
    suiteDiffPinnedOnly,
    suiteDiffSearchQuery,
    suiteDiffStatusFilter,
  ])

  useEffect(() => {
    let cancelled = false
    setHistoryRuns(loadPerfHistory())
    const localSuiteHistory = loadPerfSuiteHistory()
    const localPinnedBaseline = loadPinnedPerfSuiteBaseline()
    const nextSuiteMetaDraft = loadPerfSuiteMetaDraft()
    setPinnedSuiteBaseline(localPinnedBaseline)
    setSuiteHistory(mergePerfSuiteHistory(localPinnedBaseline ? [localPinnedBaseline] : [], localSuiteHistory))
    setSuiteLabel(nextSuiteMetaDraft.label)
    setSuiteBuildVersion(nextSuiteMetaDraft.buildVersion)
    setSuiteBranchName(nextSuiteMetaDraft.branchName)
    setSuiteEnvironment(nextSuiteMetaDraft.environment)
    setSuiteNotes(nextSuiteMetaDraft.notes)
    if (localPinnedBaseline || localSuiteHistory.length > 0) {
      setSuiteReport(localSuiteHistory[0] || localPinnedBaseline)
    }

    api.listPerfSuites(PERF_SUITE_LIMIT)
      .then((remoteItems) => {
        if (cancelled) return
        const remoteSuiteHistory = normalizePerfSuiteHistory(remoteItems)
        if (remoteSuiteHistory.length === 0 && !localPinnedBaseline) return
        const merged = mergePerfSuiteHistory(
          localPinnedBaseline ? [localPinnedBaseline, ...remoteSuiteHistory] : remoteSuiteHistory,
          localSuiteHistory
        )
        setSuiteArchiveMode('backend')
        setSuiteHistory(merged)
        savePerfSuiteHistory(merged)
        setSuiteReport((prev) => {
          if (!prev) return merged[0] || null
          return merged.find((item) => item.id === prev.id) || merged[0] || null
        })
      })
      .catch(() => {
        if (cancelled) return
        setSuiteArchiveMode('local')
      })

    api.getPerfSuiteBaseline()
      .then((remotePinned) => {
        if (cancelled) return
        const parsedPinned = parsePerfSuiteReport(remotePinned)
        if (!parsedPinned) return
        setPinnedSuiteBaseline(parsedPinned)
        savePinnedPerfSuiteBaseline(parsedPinned)
        setSuiteHistory((prev) => {
          const merged = mergePerfSuiteHistory([parsedPinned], prev)
          savePerfSuiteHistory(merged)
          return merged
        })
      })
      .catch(() => {
        if (cancelled) return
      })

    return () => {
      cancelled = true
    }
  }, [])

  useEffect(() => {
    savePerfSuiteMetaDraft({
      label: suiteLabel,
      buildVersion: suiteBuildVersion,
      branchName: suiteBranchName,
      environment: suiteEnvironment,
      notes: suiteNotes,
    })
  }, [suiteBranchName, suiteBuildVersion, suiteEnvironment, suiteLabel, suiteNotes])

  useEffect(() => {
    if (!selectedDbId && activeDbId) {
      setSelectedDbId(activeDbId)
    }
  }, [activeDbId, selectedDbId])

  useEffect(() => {
    if (!selectedHistoryId && historyRuns.length > 0) {
      setSelectedHistoryId(historyRuns[0].id)
      return
    }
    if (selectedHistoryId && !historyRuns.some((entry) => entry.id === selectedHistoryId)) {
      setSelectedHistoryId(historyRuns[0]?.id || '')
    }
  }, [historyRuns, selectedHistoryId])

  useEffect(() => {
    if (compareCandidates.length === 0) {
      if (compareBaselineId) {
        setCompareBaselineId('')
      }
      return
    }
    if (!compareCandidates.some((entry) => entry.id === compareBaselineId)) {
      setCompareBaselineId(compareCandidates[0].id)
    }
  }, [compareBaselineId, compareCandidates])

  useEffect(() => {
    if (!suiteReport) {
      if (suiteHistory.length > 0) {
        setSuiteReport(suiteHistory[0])
      }
      return
    }
    if (!suiteHistory.some((report) => report.id === suiteReport.id)) {
      setSuiteReport(suiteHistory[0] || null)
    }
  }, [suiteHistory, suiteReport])

  useEffect(() => {
    if (suiteCompareCandidates.length === 0) {
      if (compareSuiteId) {
        setCompareSuiteId('')
      }
      return
    }
    const preferredPinnedId =
      pinnedSuiteBaselineId &&
      suiteReport?.id !== pinnedSuiteBaselineId &&
      suiteCompareCandidates.some((report) => report.id === pinnedSuiteBaselineId)
        ? pinnedSuiteBaselineId
        : ''
    const preferredCompareId = preferredPinnedId || suiteCompareCandidates[0].id
    if (!compareSuiteId || !suiteCompareCandidates.some((report) => report.id === compareSuiteId)) {
      setCompareSuiteId(preferredCompareId)
    }
  }, [compareSuiteId, pinnedSuiteBaselineId, suiteCompareCandidates, suiteReport])

  useEffect(() => {
    if (suiteArchiveMode !== 'backend' || !suiteReport) {
      setSuiteDiffHistory([])
      setExpandedSuiteDiffId('')
      setIsLoadingSuiteDiffHistory(false)
      return
    }
    let cancelled = false
    setIsLoadingSuiteDiffHistory(true)
    api.listPerfSuiteDiffs(PERF_SUITE_LIMIT, suiteReport.id)
      .then((items) => {
        if (cancelled) return
        const nextHistory = Array.isArray(items)
          ? (items
              .map(parsePerfSuiteDiffArchiveRecord)
              .filter(Boolean)
              .slice(0, PERF_SUITE_LIMIT) as PerfSuiteDiffArchiveRecord[])
          : []
        setSuiteDiffHistory(nextHistory)
      })
      .catch(() => {
        if (cancelled) return
        setSuiteDiffHistory([])
      })
      .finally(() => {
        if (!cancelled) {
          setIsLoadingSuiteDiffHistory(false)
        }
      })
    return () => {
      cancelled = true
    }
  }, [suiteArchiveMode, suiteReport])

  useEffect(() => {
    if (suiteArchiveMode !== 'backend' || !suiteReport || !compareSuiteReport) {
      setArchivedSuiteDiff(null)
      return
    }
    let cancelled = false
    api.listPerfSuiteDiffs(1, suiteReport.id, compareSuiteReport.id)
      .then((items) => {
        if (cancelled) return
        const first = Array.isArray(items) ? parsePerfSuiteDiffArchiveRecord(items[0]) : null
        setArchivedSuiteDiff(first)
      })
      .catch(() => {
        if (cancelled) return
        setArchivedSuiteDiff(null)
      })
    return () => {
      cancelled = true
    }
  }, [compareSuiteReport, suiteArchiveMode, suiteReport])

  useEffect(() => {
    if (!compareSuiteReport && suiteDiffCurrentBaselineOnly) {
      setSuiteDiffCurrentBaselineOnly(false)
    }
  }, [compareSuiteReport, suiteDiffCurrentBaselineOnly])

  useEffect(() => {
    if (!isActive) return
    if (!selectedDbId) {
      setTables([])
      return
    }

    if (selectedDbId === activeDbId && schemaData?.tables) {
      setTables(schemaData.tables.map((table) => table.table_name))
      return
    }

    let cancelled = false
    setIsLoadingTables(true)
    api.getSchema(selectedDbId)
      .then((schema) => {
        if (cancelled) return
        const nextTables = Array.isArray(schema?.tables)
          ? schema.tables.map((table: any) => String(table?.table_name || '')).filter(Boolean)
          : []
        setTables(nextTables)
      })
      .catch(() => {
        if (cancelled) return
        setTables([])
      })
      .finally(() => {
        if (!cancelled) {
          setIsLoadingTables(false)
        }
      })

    return () => {
      cancelled = true
    }
  }, [activeDbId, isActive, schemaData, selectedDbId])

  useEffect(() => {
    if (!needsTable) return
    if (!tableName && tables.length > 0) {
      setTableName(tables[0])
    }
  }, [needsTable, tableName, tables])

  const handleRun = async () => {
    setIsRunning(true)
    setError('')
    setSuiteProgress(null)
    try {
      const probeResult = await api.perfProbe(buildProbePayload(operation, selectedDbId, iterations, sql, tableName))
      const historyEntry = createHistoryEntry({
        operation,
        selectedDbId,
        selectedConnectionName: selectedConnection?.name,
        iterations,
        sql,
        tableName,
        result: probeResult as PerfProbeSummary,
      })
      setHistoryRuns((prev) => {
        const nextHistory = [historyEntry, ...prev].slice(0, PERF_HISTORY_LIMIT)
        savePerfHistory(nextHistory)
        return nextHistory
      })
      setSelectedHistoryId(historyEntry.id)
      setCompareBaselineId('')
    } catch (e: unknown) {
      setError(parseError(e).message || 'Perf probe failed')
    } finally {
      setIsRunning(false)
    }
  }

  const handleRunFullSuite = async () => {
    setIsRunning(true)
    setError('')
    setSuiteReport(null)
    const suiteEntries: PerfHistoryEntry[] = []
    let failedOperation: PerfProbeOperation | null = null
    const suiteMeta = {
      label: suiteLabel.trim() || null,
      build_version: suiteBuildVersion.trim() || null,
      branch_name: suiteBranchName.trim() || null,
      environment: suiteEnvironment.trim() || null,
      notes: suiteNotes.trim() || null,
    }

    try {
      for (let index = 0; index < FULL_SUITE_OPERATIONS.length; index += 1) {
        const suiteOperation = FULL_SUITE_OPERATIONS[index]
        failedOperation = suiteOperation
        setSuiteProgress({
          current: index + 1,
          total: FULL_SUITE_OPERATIONS.length,
          operation: suiteOperation,
        })
        const probeResult = await api.perfProbe(
          buildProbePayload(suiteOperation, selectedDbId, iterations, sql, tableName)
        )
        suiteEntries.push(
          createHistoryEntry({
            operation: suiteOperation,
            selectedDbId,
            selectedConnectionName: selectedConnection?.name,
            iterations,
            sql,
            tableName,
            result: probeResult as PerfProbeSummary,
          })
        )
      }

      setHistoryRuns((prev) => {
        const nextHistory = [...suiteEntries].reverse().concat(prev).slice(0, PERF_HISTORY_LIMIT)
        savePerfHistory(nextHistory)
        return nextHistory
      })

      const latestEntry = suiteEntries[suiteEntries.length - 1]
      if (latestEntry) {
        setSelectedHistoryId(latestEntry.id)
      }

      const nextSuiteReport: PerfSuiteReport = {
        id: `suite-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
        recorded_at: new Date().toISOString(),
        connection_id: selectedDbId || null,
        connection_name: selectedConnection?.name || null,
        ...suiteMeta,
        iterations,
        sql,
        table_name: tableName || null,
        status: 'success',
        failed_operation: null,
        error: null,
        results: suiteEntries,
      }
      const archiveResult = await syncPerfSuiteHistoryToArchive([nextSuiteReport, ...suiteHistory])
      setSuiteArchiveMode(archiveResult.mode)
      setSuiteHistory(archiveResult.history)
      setSuiteReport(archiveResult.history.find((entry) => entry.id === nextSuiteReport.id) || nextSuiteReport)
    } catch (e: unknown) {
      const parsedError = parseError(e).message || 'Perf suite failed'
      setError(parsedError)
      if (suiteEntries.length > 0) {
        setHistoryRuns((prev) => {
          const nextHistory = [...suiteEntries].reverse().concat(prev).slice(0, PERF_HISTORY_LIMIT)
          savePerfHistory(nextHistory)
          return nextHistory
        })
        setSelectedHistoryId(suiteEntries[suiteEntries.length - 1].id)
      }
      const failedSuiteReport: PerfSuiteReport = {
        id: `suite-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
        recorded_at: new Date().toISOString(),
        connection_id: selectedDbId || null,
        connection_name: selectedConnection?.name || null,
        ...suiteMeta,
        iterations,
        sql,
        table_name: tableName || null,
        status: 'failed',
        failed_operation: failedOperation,
        error: parsedError,
        results: suiteEntries,
      }
      const archiveResult = await syncPerfSuiteHistoryToArchive([failedSuiteReport, ...suiteHistory])
      setSuiteArchiveMode(archiveResult.mode)
      setSuiteHistory(archiveResult.history)
      setSuiteReport(
        archiveResult.history.find((entry) => entry.id === failedSuiteReport.id) || failedSuiteReport
      )
    } finally {
      setSuiteProgress(null)
      setIsRunning(false)
    }
  }

  const handleSelectHistoryRun = (entry: PerfHistoryEntry) => {
    setSelectedHistoryId(entry.id)
    setSelectedDbId(entry.connection_id || '')
    setOperation(entry.operation)
    setIterations(entry.iterations)
    setSql(entry.sql || 'SELECT 1 AS perf_probe')
    setTableName(entry.table_name || '')
    setCompareBaselineId('')
    setError('')
  }

  const handlePinSuiteBaseline = async () => {
    if (!suiteReport || isPinningSuiteBaseline) return
    setIsPinningSuiteBaseline(true)
    try {
      let pinnedReport = suiteReport
      try {
        const remotePinned = await api.pinPerfSuiteBaseline(suiteReport.id)
        pinnedReport = parsePerfSuiteReport(remotePinned) || suiteReport
        setSuiteArchiveMode('backend')
      } catch {
        pinnedReport = suiteReport
      }
      setPinnedSuiteBaseline(pinnedReport)
      savePinnedPerfSuiteBaseline(pinnedReport)
      setSuiteHistory((prev) => {
        const merged = mergePerfSuiteHistory([pinnedReport], prev)
        savePerfSuiteHistory(merged)
        return merged
      })
    } catch (e: unknown) {
      setError(parseError(e).message || 'Failed to pin suite baseline')
    } finally {
      setIsPinningSuiteBaseline(false)
    }
  }

  const applySuiteReportSelection = (report: PerfSuiteReport) => {
    setSuiteReport(report)
    setSelectedDbId(report.connection_id || '')
    setIterations(report.iterations)
    setSql(report.sql || 'SELECT 1 AS perf_probe')
    setTableName(report.table_name || '')
    setSuiteLabel(report.label || '')
    setSuiteBuildVersion(report.build_version || '')
    setSuiteBranchName(report.branch_name || '')
    setSuiteEnvironment(report.environment || '')
    setSuiteNotes(report.notes || '')
    setCompareSuiteId('')
    setError('')
  }

  const handleSelectSuiteReport = async (report: PerfSuiteReport) => {
    let nextReport = report
    if (suiteArchiveMode === 'backend') {
      try {
        const remoteReport = await api.getPerfSuite(report.id)
        nextReport = parsePerfSuiteReport(remoteReport) || report
      } catch {
        nextReport = report
      }
    }
    applySuiteReportSelection(nextReport)
  }

  const handleExportJson = () => {
    if (!activeHistoryEntry || !result) return
    const payload = {
      ...activeHistoryEntry,
      exported_at: new Date().toISOString(),
      result,
    }
    const blob = new Blob([JSON.stringify(payload, null, 2)], { type: 'application/json' })
    const url = window.URL.createObjectURL(blob)
    const anchor = document.createElement('a')
    anchor.href = url
    anchor.download = `perf-diagnostics-${String(result.operation || operation).replace(/[^a-z0-9_-]+/gi, '-')}-${Date.now()}.json`
    document.body.appendChild(anchor)
    anchor.click()
    document.body.removeChild(anchor)
    window.URL.revokeObjectURL(url)
  }

  const handleExportSuiteJson = () => {
    if (!suiteReport) return
    const payload = {
      exported_at: new Date().toISOString(),
      suite: suiteReport,
    }
    const blob = new Blob([JSON.stringify(payload, null, 2)], { type: 'application/json' })
    const url = window.URL.createObjectURL(blob)
    const anchor = document.createElement('a')
    anchor.href = url
    anchor.download = `perf-suite-${suiteReport.status}-${Date.now()}.json`
    document.body.appendChild(anchor)
    anchor.click()
    document.body.removeChild(anchor)
    window.URL.revokeObjectURL(url)
  }

  const handleExportSuiteDiffJson = () => {
    if (!suiteDiffPayload) return
    const payload = {
      exported_at: new Date().toISOString(),
      ...suiteDiffPayload,
    }
    const blob = new Blob([JSON.stringify(payload, null, 2)], { type: 'application/json' })
    const url = window.URL.createObjectURL(blob)
    const anchor = document.createElement('a')
    anchor.href = url
    anchor.download = `perf-suite-diff-${suiteDiffPayload.current_suite_id}-${suiteDiffPayload.baseline_suite_id}.json`
    document.body.appendChild(anchor)
    anchor.click()
    document.body.removeChild(anchor)
    window.URL.revokeObjectURL(url)
  }

  const handleArchiveSuiteDiffReport = async () => {
    if (!suiteDiffPayload || isArchivingSuiteDiff) return
    setIsArchivingSuiteDiff(true)
    try {
      const saved = await api.savePerfSuiteDiff(suiteDiffPayload)
      const parsed = parsePerfSuiteDiffArchiveRecord(saved)
      setArchivedSuiteDiff(parsed)
      setExpandedSuiteDiffId(parsed?.id || '')
      setSuiteArchiveMode('backend')
    } catch (e: unknown) {
      setError(parseError(e).message || 'Failed to archive suite diff report')
    } finally {
      setIsArchivingSuiteDiff(false)
    }
  }

  const handleLoadArchivedSuiteDiff = async (record: PerfSuiteDiffArchiveRecord) => {
    setError('')
    setSameConnectionOnly(false)
    setSameBuildVersionOnly(false)

    let currentReport =
      suiteHistory.find((item) => item.id === record.current_suite_id) ||
      (suiteReport?.id === record.current_suite_id ? suiteReport : null)
    let baselineReport =
      suiteHistory.find((item) => item.id === record.baseline_suite_id) ||
      (suiteReport?.id === record.baseline_suite_id ? suiteReport : null) ||
      (compareSuiteReport?.id === record.baseline_suite_id ? compareSuiteReport : null)

    if (suiteArchiveMode === 'backend') {
      if (!currentReport) {
        try {
          currentReport = parsePerfSuiteReport(await api.getPerfSuite(record.current_suite_id))
        } catch {
          currentReport = null
        }
      }
      if (!baselineReport) {
        try {
          baselineReport = parsePerfSuiteReport(await api.getPerfSuite(record.baseline_suite_id))
        } catch {
          baselineReport = null
        }
      }
    }

    if (!currentReport || !baselineReport) {
      setError('Failed to restore archived diff suite pair.')
      return
    }

    const merged = mergePerfSuiteHistory([currentReport, baselineReport], suiteHistory)
    setSuiteHistory(merged)
    savePerfSuiteHistory(merged)
    applySuiteReportSelection(currentReport)
    setCompareSuiteId(baselineReport.id)
    setArchivedSuiteDiff(record)
    setExpandedSuiteDiffId(record.id)
    setSuiteArchiveMode('backend')
  }

  const handleResetSuiteDiffFilters = () => {
    setSuiteDiffStatusFilter('all')
    setSuiteDiffPinnedOnly(false)
    setSuiteDiffCurrentBaselineOnly(false)
    setSuiteDiffSearchQuery('')
  }

  const handleCopySuiteDiffField = async (value: string, label: string) => {
    const ok = await copyTextToClipboard(value)
    if (!ok) {
      setError(`Failed to copy ${label}.`)
    }
  }

  const handleExportSuiteDiffItemJson = (item: PerfSuiteDiffArchiveRecord) => {
    const payload = {
      exported_at: new Date().toISOString(),
      suite_id: suiteReport?.id || null,
      suite_label: suiteReport?.label || null,
      compare_baseline_id: compareSuiteReport?.id || null,
      item,
    }
    const blob = new Blob([JSON.stringify(payload, null, 2)], { type: 'application/json' })
    const url = window.URL.createObjectURL(blob)
    const anchor = document.createElement('a')
    anchor.href = url
    anchor.download = `perf-suite-diff-item-${item.id}.json`
    document.body.appendChild(anchor)
    anchor.click()
    document.body.removeChild(anchor)
    window.URL.revokeObjectURL(url)
  }

  const handleExportFilteredSuiteDiffHistoryJson = () => {
    if (filteredSuiteDiffHistory.length === 0) return
    const payload = {
      exported_at: new Date().toISOString(),
      suite_id: suiteReport?.id || null,
      suite_label: suiteReport?.label || null,
      compare_baseline_id: compareSuiteReport?.id || null,
      filters: {
        status: suiteDiffStatusFilter,
        pinned_only: suiteDiffPinnedOnly,
        current_baseline_only: suiteDiffCurrentBaselineOnly,
        search_query: suiteDiffSearchQuery.trim() || null,
      },
      items: filteredSuiteDiffHistory,
    }
    const blob = new Blob([JSON.stringify(payload, null, 2)], { type: 'application/json' })
    const url = window.URL.createObjectURL(blob)
    const anchor = document.createElement('a')
    anchor.href = url
    anchor.download = `perf-suite-diff-history-${suiteReport?.id || 'current'}.json`
    document.body.appendChild(anchor)
    anchor.click()
    document.body.removeChild(anchor)
    window.URL.revokeObjectURL(url)
  }

  return (
    <div className="h-full overflow-y-auto bg-[#0a0c10] p-6">
      <div className="max-w-6xl mx-auto flex flex-col gap-6">
        <div className="border border-[#30363d] bg-[#161b22] rounded-xl p-5">
          <div className="flex items-start justify-between gap-4">
            <div>
              <div className="flex items-center gap-2 text-lg font-bold text-gray-100">
                <Activity className="w-5 h-5 text-blue-400" />
                Perf Diagnostics
              </div>
              <div className="text-sm text-gray-400 mt-1">
                Run repeatable probes against the current desktop / web runtime path and inspect p50 / p95 latency directly.
              </div>
            </div>
            <div className="text-xs text-gray-500 border border-[#30363d] rounded-lg px-3 py-2 bg-[#0d1117]">
              Active connection: {selectedConnection?.name || selectedDbId || 'None'}
            </div>
          </div>
        </div>

        <div className="grid grid-cols-1 xl:grid-cols-[360px_1fr] gap-4">
          <div className="border border-[#30363d] bg-[#161b22] rounded-xl p-4 flex flex-col gap-4">
            <div className="text-sm font-semibold text-gray-100">Probe Setup</div>

            <label className="flex flex-col gap-1">
              <span className="text-xs text-gray-400">Connection</span>
              <select
                value={selectedDbId}
                onChange={(e) => {
                  setSelectedDbId(e.target.value)
                  setError('')
                }}
                className="bg-[#0d1117] border border-[#30363d] rounded-lg px-3 py-2 text-sm text-gray-100"
              >
                {connections.length === 0 && <option value="">No connection</option>}
                {connections.map((conn) => (
                  <option key={conn.id} value={conn.id}>
                    {conn.name || conn.id}
                  </option>
                ))}
              </select>
            </label>

            <label className="flex flex-col gap-1">
              <span className="text-xs text-gray-400">Operation</span>
              <select
                value={operation}
                onChange={(e) => {
                  setOperation(e.target.value as PerfProbeOperation)
                  setError('')
                }}
                className="bg-[#0d1117] border border-[#30363d] rounded-lg px-3 py-2 text-sm text-gray-100"
              >
                {OPERATION_OPTIONS.map((item) => (
                  <option key={item.value} value={item.value}>
                    {item.label}
                  </option>
                ))}
              </select>
            </label>

            <div className="text-xs text-gray-400 rounded-lg border border-[#30363d] bg-[#0d1117] px-3 py-2">
              {currentOperation.description}
            </div>

            <label className="flex flex-col gap-1">
              <span className="text-xs text-gray-400">Iterations</span>
              <input
                type="number"
                min={1}
                max={30}
                value={iterations}
                onChange={(e) => setIterations(Math.max(1, Math.min(30, Number(e.target.value || 1))))}
                className="bg-[#0d1117] border border-[#30363d] rounded-lg px-3 py-2 text-sm text-gray-100"
              />
            </label>

            <div className="rounded-lg border border-[#30363d] bg-[#0d1117] px-3 py-3 flex flex-col gap-3">
              <div className="text-xs font-medium text-gray-300">Full Suite Metadata</div>
              <label className="flex flex-col gap-1">
                <span className="text-[11px] text-gray-500">Run Label</span>
                <input
                  value={suiteLabel}
                  onChange={(e) => setSuiteLabel(e.target.value)}
                  placeholder="before-index-tuning / rc1 / nightly"
                  className="bg-[#161b22] border border-[#30363d] rounded-lg px-3 py-2 text-sm text-gray-100"
                />
              </label>
              <div className="flex flex-wrap gap-2">
                {SUITE_LABEL_PRESETS.map((preset) => (
                  <button
                    key={preset.value}
                    type="button"
                    onClick={() => setSuiteLabel(preset.value)}
                    className="px-2.5 py-1 rounded-md border border-[#30363d] bg-[#161b22] text-[11px] text-gray-300 hover:bg-[#21262d]"
                  >
                    {preset.label}
                  </button>
                ))}
              </div>
              <div className="grid grid-cols-1 sm:grid-cols-2 gap-2">
                <label className="flex flex-col gap-1">
                  <span className="text-[11px] text-gray-500">Build Version</span>
                  <input
                    value={suiteBuildVersion}
                    onChange={(e) => setSuiteBuildVersion(e.target.value)}
                    placeholder="v0.9.3 / 2026.05.08-rc1"
                    className="bg-[#161b22] border border-[#30363d] rounded-lg px-3 py-2 text-sm text-gray-100"
                  />
                </label>
                <label className="flex flex-col gap-1">
                  <span className="text-[11px] text-gray-500">Branch / Commit</span>
                  <input
                    value={suiteBranchName}
                    onChange={(e) => setSuiteBranchName(e.target.value)}
                    placeholder="codex/perf-pass / a1b2c3d"
                    className="bg-[#161b22] border border-[#30363d] rounded-lg px-3 py-2 text-sm text-gray-100"
                  />
                </label>
              </div>
              <label className="flex flex-col gap-1">
                <span className="text-[11px] text-gray-500">Environment</span>
                <input
                  value={suiteEnvironment}
                  onChange={(e) => setSuiteEnvironment(e.target.value)}
                  placeholder="desktop-tauri / web-local / mysql8-local / win11"
                  className="bg-[#161b22] border border-[#30363d] rounded-lg px-3 py-2 text-sm text-gray-100"
                />
              </label>
              <label className="flex flex-col gap-1">
                <span className="text-[11px] text-gray-500">Notes</span>
                <textarea
                  value={suiteNotes}
                  onChange={(e) => setSuiteNotes(e.target.value)}
                  rows={3}
                  placeholder="Capture what changed in this run, such as pool reuse, schema cache, or release candidate checks."
                  className="bg-[#161b22] border border-[#30363d] rounded-lg px-3 py-2 text-sm text-gray-100 resize-y"
                />
              </label>
              <div className="text-[11px] text-gray-500">
                Use labels plus notes to keep before/after optimization and RC/nightly suite comparisons searchable.
              </div>
            </div>

            {needsSql && (
              <label className="flex flex-col gap-1">
                <span className="text-xs text-gray-400">{sqlLabel}</span>
                <textarea
                  value={sql}
                  onChange={(e) => setSql(e.target.value)}
                  rows={5}
                  className="bg-[#0d1117] border border-[#30363d] rounded-lg px-3 py-2 text-sm text-gray-100 font-mono resize-y"
                />
              </label>
            )}

            {needsTable && (
              <label className="flex flex-col gap-1">
                <span className="text-xs text-gray-400">Table Name</span>
                <input
                  list="perf-diagnostics-table-list"
                  value={tableName}
                  onChange={(e) => setTableName(e.target.value)}
                  placeholder={isLoadingTables ? 'Loading tables...' : 'Enter table name'}
                  className="bg-[#0d1117] border border-[#30363d] rounded-lg px-3 py-2 text-sm text-gray-100"
                />
                <datalist id="perf-diagnostics-table-list">
                  {tables.map((table) => (
                    <option key={table} value={table} />
                  ))}
                </datalist>
                <div className="text-[11px] text-gray-500">
                  {isLoadingTables
                    ? 'Refreshing table list...'
                    : `${tables.length} tables available for quick selection.`}
                </div>
              </label>
            )}

            <div className="grid grid-cols-1 sm:grid-cols-2 gap-2">
              <button
                onClick={handleRun}
                disabled={isRunning || (!selectedDbId && connections.length > 0) || (needsTable && !tableName.trim())}
                className="flex items-center justify-center gap-2 px-4 py-2 rounded-lg bg-blue-500/15 border border-blue-500/30 text-blue-300 hover:bg-blue-500/25 disabled:opacity-60"
              >
                {isRunning ? <RefreshCw className="w-4 h-4 animate-spin" /> : <Play className="w-4 h-4" />}
                {isRunning ? 'Running...' : 'Run Probe'}
              </button>

              <button
                onClick={handleRunFullSuite}
                disabled={isRunning || !canRunFullSuite}
                className="flex items-center justify-center gap-2 px-4 py-2 rounded-lg bg-emerald-500/15 border border-emerald-500/30 text-emerald-300 hover:bg-emerald-500/25 disabled:opacity-60"
              >
                {isRunning ? <RefreshCw className="w-4 h-4 animate-spin" /> : <Play className="w-4 h-4" />}
                {isRunning ? 'Running Suite...' : 'Run Full Suite'}
              </button>
            </div>

            <div className="text-[11px] text-gray-500">
              Full suite uses the current SQL for read/explain probes and the current table for table-first-page.
            </div>

            {suiteProgress && (
              <div className="text-xs text-blue-300 rounded-lg border border-blue-500/30 bg-blue-500/10 px-3 py-2">
                Suite progress: {suiteProgress.current}/{suiteProgress.total} · {suiteProgress.operation}
              </div>
            )}

            {error && (
              <div className="text-sm text-red-300 rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2">
                {error}
              </div>
            )}

            <div className="pt-2 border-t border-[#30363d] flex flex-col gap-3">
              <div className="flex items-center justify-between gap-2">
                <div className="text-sm font-semibold text-gray-100">Full Suite History</div>
                <div className="text-[11px] text-gray-500">
                  {suiteArchiveMode === 'backend' ? 'Backend archive + local cache' : 'Local fallback'} · latest {suiteHistory.length}
                </div>
              </div>

              {suiteHistory.length === 0 ? (
                <div className="rounded-lg border border-dashed border-[#30363d] px-3 py-4 text-xs text-gray-500">
                  Run a full suite to keep complete baseline snapshots for version-to-version comparison.
                </div>
              ) : (
                <div className="max-h-[220px] overflow-auto pr-1 flex flex-col gap-2">
                  {suiteHistory.map((report) => {
                    const summary = buildSuiteBudgetSummary(report)
                    return (
                      <button
                        key={report.id}
                        type="button"
                        onClick={() => {
                          void handleSelectSuiteReport(report)
                        }}
                        className={`text-left rounded-xl border px-3 py-3 transition ${
                          report.id === suiteReport?.id
                            ? 'border-emerald-500/40 bg-emerald-500/10'
                            : 'border-[#30363d] bg-[#0d1117] hover:bg-[#21262d]'
                        }`}
                      >
                        <div className="flex items-start justify-between gap-3">
                          <div>
                            <div className="text-xs font-medium text-gray-100">
                              {report.label || (report.status === 'success' ? 'Suite OK' : 'Suite Failed')}
                            </div>
                            <div className="text-[11px] text-gray-500 mt-1">{formatRecordedAt(report.recorded_at)}</div>
                          </div>
                          <div className={report.status === 'success' ? 'text-[11px] text-green-300' : 'text-[11px] text-red-300'}>
                            {report.status}
                          </div>
                        </div>
                        {pinnedSuiteBaselineId === report.id && (
                          <div className="text-[11px] text-amber-300 mt-2">
                            pinned baseline
                          </div>
                        )}
                        <div className="text-[11px] text-gray-400 mt-2">
                          {report.connection_name || report.connection_id || 'Unknown connection'}
                        </div>
                        <div className="text-[11px] text-gray-500 mt-1">
                          {report.results.length} ops
                          {summary ? ` · pass ${summary.passCount}/${summary.totalCount}` : ''}
                        </div>
                        {(report.build_version || report.branch_name || report.environment) && (
                          <div className="text-[11px] text-gray-500 mt-1">
                            {report.build_version || '-'}
                            {report.branch_name ? ` · ${report.branch_name}` : ''}
                            {report.environment ? ` · ${report.environment}` : ''}
                          </div>
                        )}
                        {report.notes && <div className="text-[11px] text-gray-500 mt-1 truncate">{report.notes}</div>}
                      </button>
                    )
                  })}
                </div>
              )}

              <div className="flex items-center justify-between gap-2">
                <div className="text-sm font-semibold text-gray-100">Recent Runs</div>
                <div className="text-[11px] text-gray-500">Local only · latest {historyRuns.length}</div>
              </div>

              {historyRuns.length === 0 ? (
                <div className="rounded-lg border border-dashed border-[#30363d] px-3 py-4 text-xs text-gray-500">
                  Run a probe to start building local perf history for before/after comparisons.
                </div>
              ) : (
                <div className="max-h-[320px] overflow-auto pr-1 flex flex-col gap-2">
                  {historyRuns.map((entry) => (
                    <button
                      key={entry.id}
                      type="button"
                      onClick={() => handleSelectHistoryRun(entry)}
                      className={`text-left rounded-xl border px-3 py-3 transition ${
                        entry.id === selectedHistoryId
                          ? 'border-blue-500/40 bg-blue-500/10'
                          : 'border-[#30363d] bg-[#0d1117] hover:bg-[#21262d]'
                      }`}
                    >
                      <div className="flex items-start justify-between gap-3">
                        <div>
                          <div className="text-xs font-medium text-gray-100">{entry.operation}</div>
                          <div className="text-[11px] text-gray-500 mt-1">{formatRecordedAt(entry.recorded_at)}</div>
                        </div>
                        <div className="text-[11px] text-gray-400">
                          p50 {metricValue(entry.result.p50_ms)}
                        </div>
                      </div>
                      <div className="text-[11px] text-gray-400 mt-2">
                        {entry.connection_name || entry.connection_id || 'Unknown connection'}
                      </div>
                      <div className="text-[11px] text-gray-500 mt-1">
                        p95 {metricValue(entry.result.p95_ms)} · {entry.iterations} samples
                      </div>
                    </button>
                  ))}
                </div>
              )}
            </div>
          </div>

          <div className="border border-[#30363d] bg-[#161b22] rounded-xl p-4 flex flex-col gap-4">
            <div className="flex items-center justify-between gap-3">
              <div className="flex items-center gap-2 text-sm font-semibold text-gray-100">
                <Database className="w-4 h-4 text-blue-400" />
                Result Inspector
              </div>
              <div className="flex items-center gap-2">
                <button
                  onClick={() => {
                    void handleArchiveSuiteDiffReport()
                  }}
                  disabled={!suiteDiffPayload || isArchivingSuiteDiff}
                  className="flex items-center gap-2 px-3 py-1.5 rounded-lg border border-[#30363d] bg-[#0d1117] text-xs text-gray-200 hover:bg-[#21262d] disabled:opacity-50"
                >
                  <Download className="w-3.5 h-3.5" />
                  {isArchivingSuiteDiff ? 'Archiving Diff...' : 'Archive Diff'}
                </button>
                <button
                  onClick={handleExportSuiteDiffJson}
                  disabled={!suiteDiffPayload}
                  className="flex items-center gap-2 px-3 py-1.5 rounded-lg border border-[#30363d] bg-[#0d1117] text-xs text-gray-200 hover:bg-[#21262d] disabled:opacity-50"
                >
                  <Download className="w-3.5 h-3.5" />
                  Export Diff JSON
                </button>
                <button
                  onClick={handleExportSuiteJson}
                  disabled={!suiteReport}
                  className="flex items-center gap-2 px-3 py-1.5 rounded-lg border border-[#30363d] bg-[#0d1117] text-xs text-gray-200 hover:bg-[#21262d] disabled:opacity-50"
                >
                  <Download className="w-3.5 h-3.5" />
                  Export Suite JSON
                </button>
                <button
                  onClick={handleExportJson}
                  disabled={!result}
                  className="flex items-center gap-2 px-3 py-1.5 rounded-lg border border-[#30363d] bg-[#0d1117] text-xs text-gray-200 hover:bg-[#21262d] disabled:opacity-50"
                >
                  <Download className="w-3.5 h-3.5" />
                  Export JSON
                </button>
              </div>
            </div>

            {suiteReport && (
              <div className="rounded-xl border border-[#30363d] overflow-hidden">
                <div className="px-4 py-3 bg-[#0d1117] border-b border-[#30363d] text-sm font-semibold text-gray-100">
                  Selected Full Suite
                </div>
                <div className="p-4 bg-[#161b22] flex flex-col gap-4">
                  <div className="text-xs text-gray-400">
                    Status:{' '}
                    <span className={suiteReport.status === 'success' ? 'text-green-300' : 'text-red-300'}>
                      {suiteReport.status}
                    </span>
                    {suiteReport.label && (
                      <>
                        {' · '}
                        Label: <span className="text-gray-200">{suiteReport.label}</span>
                      </>
                    )}
                    {' · '}
                    Captured: <span className="text-gray-200">{formatRecordedAt(suiteReport.recorded_at)}</span>
                    {suiteReport.connection_name && (
                      <>
                        {' · '}
                        Connection: <span className="text-gray-200">{suiteReport.connection_name}</span>
                      </>
                    )}
                    {isPinnedSuiteSelected && (
                      <>
                        {' · '}
                        <span className="text-amber-300">Pinned Baseline</span>
                      </>
                    )}
                  </div>

                  <div className="grid grid-cols-1 lg:grid-cols-[1fr_320px] gap-3">
                    <div className="rounded-xl border border-[#30363d] bg-[#0d1117] px-4 py-3">
                      <div className="text-xs text-gray-400">
                        Selected suite: <span className="text-gray-200">{formatRecordedAt(suiteReport.recorded_at)}</span>
                      </div>
                      <div className="text-xs text-gray-500 mt-2 break-all">
                        {suiteReport.sql && (
                          <>
                            SQL: <span className="text-gray-300">{suiteReport.sql}</span>
                          </>
                        )}
                        {suiteReport.table_name && (
                          <>
                            {suiteReport.sql && ' · '}
                            Table: <span className="text-gray-300">{suiteReport.table_name}</span>
                          </>
                        )}
                        {(suiteReport.build_version || suiteReport.branch_name || suiteReport.environment) && (
                          <>
                            {(suiteReport.sql || suiteReport.table_name) && ' · '}
                            Build: <span className="text-gray-300">{suiteReport.build_version || '-'}</span>
                            {suiteReport.branch_name && (
                              <>
                                {' · '}
                                Branch: <span className="text-gray-300">{suiteReport.branch_name}</span>
                              </>
                            )}
                            {suiteReport.environment && (
                              <>
                                {' · '}
                                Environment: <span className="text-gray-300">{suiteReport.environment}</span>
                              </>
                            )}
                          </>
                        )}
                      </div>
                      {suiteReport.notes && (
                        <div className="text-xs text-gray-500 mt-2">
                          Notes: <span className="text-gray-300">{suiteReport.notes}</span>
                        </div>
                      )}
                    </div>

                    <div className="rounded-xl border border-[#30363d] bg-[#0d1117] px-4 py-3">
                      <div className="flex items-center justify-between gap-3 mb-2">
                        <div className="text-xs text-gray-400">Compare suite baseline</div>
                        <button
                          type="button"
                          onClick={() => {
                            void handlePinSuiteBaseline()
                          }}
                          disabled={!suiteReport || isPinningSuiteBaseline || isPinnedSuiteSelected}
                          className="px-2.5 py-1 rounded-md border border-amber-500/30 bg-amber-500/10 text-[11px] text-amber-200 hover:bg-amber-500/20 disabled:opacity-50"
                        >
                          {isPinnedSuiteSelected ? 'Pinned' : isPinningSuiteBaseline ? 'Pinning...' : 'Pin as Baseline'}
                        </button>
                      </div>
                      <select
                        value={compareSuiteId}
                        onChange={(e) => setCompareSuiteId(e.target.value)}
                        disabled={suiteCompareCandidates.length === 0}
                        className="w-full bg-[#161b22] border border-[#30363d] rounded-lg px-3 py-2 text-sm text-gray-100 disabled:opacity-50"
                      >
                        {suiteCompareCandidates.length === 0 && <option value="">No comparable suite</option>}
                        {suiteCompareCandidates.map((report) => (
                          <option key={report.id} value={report.id}>
                            {(report.label || formatRecordedAt(report.recorded_at))} · {report.status}
                            {report.build_version ? ` · ${report.build_version}` : ''}
                            {report.environment ? ` · ${report.environment}` : ''}
                          </option>
                        ))}
                      </select>
                      <div className="grid grid-cols-1 gap-2 mt-3">
                        <label className="flex items-center gap-2 text-[11px] text-gray-400">
                          <input
                            type="checkbox"
                            checked={sameConnectionOnly}
                            onChange={(e) => setSameConnectionOnly(e.target.checked)}
                            className="rounded border-[#30363d] bg-[#161b22]"
                          />
                          Same connection only
                        </label>
                        <label className="flex items-center gap-2 text-[11px] text-gray-400">
                          <input
                            type="checkbox"
                            checked={sameBuildVersionOnly}
                            onChange={(e) => setSameBuildVersionOnly(e.target.checked)}
                            className="rounded border-[#30363d] bg-[#161b22]"
                          />
                          Same build version only
                        </label>
                      </div>
                      <div className="text-[11px] text-gray-500 mt-3">
                        {suiteCompareCandidates.length} candidate(s) after filters.
                      </div>
                      {pinnedSuiteBaseline && (
                        <div className="text-[11px] text-gray-500 mt-2">
                          Locked baseline: {pinnedSuiteBaseline.label || formatRecordedAt(pinnedSuiteBaseline.recorded_at)}
                          {pinnedSuiteBaseline.build_version ? ` · ${pinnedSuiteBaseline.build_version}` : ''}
                        </div>
                      )}
                      {archivedSuiteDiff?.archive_path && (
                        <div className="text-[11px] text-gray-500 mt-2 break-all">
                          Archived diff: {formatRecordedAt(archivedSuiteDiff.recorded_at)}
                          {' · '}
                          {archivedSuiteDiff.archive_path}
                        </div>
                      )}
                      {suiteArchiveMode === 'backend' && (
                        <div className="mt-3">
                          <div className="text-[11px] text-gray-500 mb-2">
                            Archived diff history for this suite
                            {isLoadingSuiteDiffHistory
                              ? ' | loading...'
                              : ` | ${filteredSuiteDiffHistory.length}/${suiteDiffHistory.length} record(s)`}
                          </div>
                          <div className="flex flex-wrap gap-2 mb-2">
                            {SUITE_DIFF_STATUS_FILTERS.map((filter) => (
                              <button
                                key={filter.value}
                                type="button"
                                onClick={() => setSuiteDiffStatusFilter(filter.value)}
                                className={`px-2 py-1 rounded-md border text-[10px] transition ${
                                  suiteDiffStatusFilter === filter.value
                                    ? 'border-blue-500/40 bg-blue-500/10 text-blue-200'
                                    : 'border-[#30363d] bg-[#161b22] text-gray-400 hover:bg-[#21262d]'
                                }`}
                              >
                                {filter.label}
                              </button>
                            ))}
                            <label className="flex items-center gap-2 px-2 py-1 rounded-md border border-[#30363d] bg-[#161b22] text-[10px] text-gray-400">
                              <input
                                type="checkbox"
                                checked={suiteDiffPinnedOnly}
                                onChange={(e) => setSuiteDiffPinnedOnly(e.target.checked)}
                                className="rounded border-[#30363d] bg-[#161b22]"
                              />
                              Pinned only
                            </label>
                            <label className="flex items-center gap-2 px-2 py-1 rounded-md border border-[#30363d] bg-[#161b22] text-[10px] text-gray-400">
                              <input
                                type="checkbox"
                                checked={suiteDiffCurrentBaselineOnly}
                                onChange={(e) => setSuiteDiffCurrentBaselineOnly(e.target.checked)}
                                disabled={!compareSuiteReport}
                                className="rounded border-[#30363d] bg-[#161b22]"
                              />
                              Current baseline only
                            </label>
                            <button
                              type="button"
                              onClick={handleResetSuiteDiffFilters}
                              disabled={!hasActiveSuiteDiffFilters}
                              className="px-2 py-1 rounded-md border border-[#30363d] bg-[#161b22] text-[10px] text-gray-400 hover:bg-[#21262d] disabled:opacity-50"
                            >
                              Reset filters
                            </button>
                            <button
                              type="button"
                              onClick={handleExportFilteredSuiteDiffHistoryJson}
                              disabled={filteredSuiteDiffHistory.length === 0}
                              className="px-2 py-1 rounded-md border border-[#30363d] bg-[#161b22] text-[10px] text-gray-400 hover:bg-[#21262d] disabled:opacity-50"
                            >
                              Export filtered JSON
                            </button>
                          </div>
                          <input
                            value={suiteDiffSearchQuery}
                            onChange={(e) => setSuiteDiffSearchQuery(e.target.value)}
                            placeholder="Search baseline label / suite id / archive path"
                            className="w-full mb-2 bg-[#161b22] border border-[#30363d] rounded-lg px-3 py-2 text-[11px] text-gray-100"
                          />
                          {activeSuiteDiffFilterTags.length > 0 && (
                            <div className="flex flex-wrap gap-2 mb-2">
                              {activeSuiteDiffFilterTags.map((tag) => (
                                <div
                                  key={tag}
                                  className="px-2 py-1 rounded-md border border-[#30363d] bg-[#0d1117] text-[10px] text-gray-400"
                                >
                                  {tag}
                                </div>
                              ))}
                            </div>
                          )}
                          {filteredSuiteDiffHistory.length === 0 ? (
                            <div className="text-[11px] text-gray-500">
                              {suiteDiffHistory.length === 0
                                ? 'No archived diff report for this suite yet.'
                                : 'No archived diff matches the current filters.'}
                            </div>
                          ) : (
                            <div className="max-h-[140px] overflow-auto pr-1 flex flex-col gap-2">
                              {filteredSuiteDiffHistory.map((item) => (
                                <div
                                  key={item.id}
                                  className={`rounded-lg border px-2.5 py-2 transition ${
                                    item.id === archivedSuiteDiff?.id
                                      ? 'border-blue-500/40 bg-blue-500/10'
                                      : 'border-[#30363d] bg-[#161b22]'
                                  }`}
                                >
                                  <button
                                    type="button"
                                    onClick={() => {
                                      void handleLoadArchivedSuiteDiff(item)
                                    }}
                                    className="w-full text-left"
                                  >
                                    <div className="flex items-start justify-between gap-2">
                                      <div className="text-[11px] text-gray-200">
                                        {item.baseline_suite_label || item.baseline_suite_id}
                                      </div>
                                      <div
                                        className={`text-[10px] uppercase ${
                                          item.gate_status === 'pass'
                                            ? 'text-green-300'
                                            : item.gate_status === 'fail'
                                              ? 'text-red-300'
                                              : 'text-amber-300'
                                        }`}
                                      >
                                        {(item.gate_status || 'unknown').toUpperCase()}
                                      </div>
                                    </div>
                                    <div className="text-[10px] text-gray-500 mt-1">
                                      {formatRecordedAt(item.recorded_at)}
                                      {item.baseline_scope ? ` | ${item.baseline_scope}` : ''}
                                    </div>
                                  </button>
                                  <div className="mt-2 flex flex-wrap gap-2">
                                    <button
                                      type="button"
                                      onClick={() =>
                                        setExpandedSuiteDiffId((prev) => (prev === item.id ? '' : item.id))
                                      }
                                      className="px-2 py-1 rounded-md border border-[#30363d] bg-[#0d1117] text-[10px] text-gray-400 hover:bg-[#21262d]"
                                    >
                                      {expandedSuiteDiffId === item.id ? 'Hide details' : 'Details'}
                                    </button>
                                    <button
                                      type="button"
                                      onClick={() => {
                                        void handleCopySuiteDiffField(
                                          `${item.current_suite_id} -> ${item.baseline_suite_id}`,
                                          'suite ids'
                                        )
                                      }}
                                      className="px-2 py-1 rounded-md border border-[#30363d] bg-[#0d1117] text-[10px] text-gray-400 hover:bg-[#21262d]"
                                    >
                                      Copy IDs
                                    </button>
                                    <button
                                      type="button"
                                      onClick={() => {
                                        void handleCopySuiteDiffField(item.archive_path || '', 'archive path')
                                      }}
                                      disabled={!item.archive_path}
                                      className="px-2 py-1 rounded-md border border-[#30363d] bg-[#0d1117] text-[10px] text-gray-400 hover:bg-[#21262d] disabled:opacity-50"
                                    >
                                      Copy path
                                    </button>
                                    <button
                                      type="button"
                                      onClick={() => handleExportSuiteDiffItemJson(item)}
                                      className="px-2 py-1 rounded-md border border-[#30363d] bg-[#0d1117] text-[10px] text-gray-400 hover:bg-[#21262d]"
                                    >
                                      Export item
                                    </button>
                                  </div>
                                  {expandedSuiteDiffId === item.id && (
                                    <div className="mt-2 rounded-md border border-[#30363d] bg-[#0d1117] px-2.5 py-2 text-[10px] text-gray-400 break-all">
                                      <div>
                                        Current suite:
                                        <span className="text-gray-300">
                                          {' '}
                                          {item.current_suite_label || item.current_suite_id}
                                        </span>
                                      </div>
                                      <div className="mt-1">
                                        Current suite id:
                                        <span className="text-gray-300"> {item.current_suite_id}</span>
                                      </div>
                                      <div className="mt-1">
                                        Baseline suite:
                                        <span className="text-gray-300">
                                          {' '}
                                          {item.baseline_suite_label || item.baseline_suite_id}
                                        </span>
                                      </div>
                                      <div className="mt-1">
                                        Baseline suite id:
                                        <span className="text-gray-300"> {item.baseline_suite_id}</span>
                                      </div>
                                      <div className="mt-1">
                                        Gate:
                                        <span className="text-gray-300">
                                          {' '}
                                          {(item.gate_status || 'unknown').toUpperCase()}
                                        </span>
                                      </div>
                                      {item.archive_path && (
                                        <div className="mt-1">
                                          Archive path:
                                          <span className="text-gray-300"> {item.archive_path}</span>
                                        </div>
                                      )}
                                      <div className="mt-2 flex flex-wrap gap-2">
                                        <button
                                          type="button"
                                          onClick={() => {
                                            void handleCopySuiteDiffField(item.current_suite_id, 'current suite id')
                                          }}
                                          className="px-2 py-1 rounded-md border border-[#30363d] bg-[#161b22] text-[10px] text-gray-400 hover:bg-[#21262d]"
                                        >
                                          Copy current id
                                        </button>
                                        <button
                                          type="button"
                                          onClick={() => {
                                            void handleCopySuiteDiffField(item.baseline_suite_id, 'baseline suite id')
                                          }}
                                          className="px-2 py-1 rounded-md border border-[#30363d] bg-[#161b22] text-[10px] text-gray-400 hover:bg-[#21262d]"
                                        >
                                          Copy baseline id
                                        </button>
                                      </div>
                                    </div>
                                  )}
                                </div>
                              ))}
                            </div>
                          )}
                        </div>
                      )}
                    </div>
                  </div>

                  {suiteBudgetSummary && (
                    <div className="grid grid-cols-2 lg:grid-cols-4 gap-3">
                      <div className="rounded-xl border border-[#30363d] bg-[#0d1117] px-3 py-3">
                        <div className="text-[11px] uppercase tracking-wide text-gray-500">Operations</div>
                        <div className="text-sm font-semibold text-gray-100 mt-1">{suiteReport.results.length}</div>
                      </div>
                      <div className="rounded-xl border border-green-500/40 bg-green-500/10 px-3 py-3">
                        <div className="text-[11px] uppercase tracking-wide text-gray-500">Budget Pass</div>
                        <div className="text-sm font-semibold text-gray-100 mt-1">{suiteBudgetSummary.passCount}</div>
                      </div>
                      <div className="rounded-xl border border-red-500/40 bg-red-500/10 px-3 py-3">
                        <div className="text-[11px] uppercase tracking-wide text-gray-500">Budget Fail</div>
                        <div className="text-sm font-semibold text-gray-100 mt-1">{suiteBudgetSummary.failCount}</div>
                      </div>
                      <div className="rounded-xl border border-[#30363d] bg-[#0d1117] px-3 py-3">
                        <div className="text-[11px] uppercase tracking-wide text-gray-500">Budgeted Ops</div>
                        <div className="text-sm font-semibold text-gray-100 mt-1">{suiteBudgetSummary.totalCount}</div>
                      </div>
                    </div>
                  )}

                  {suiteGateSummary && (
                    <div className="grid grid-cols-1 lg:grid-cols-4 gap-3">
                      <div
                        className={`rounded-xl border px-3 py-3 ${
                          suiteGateSummary.status === 'pass'
                            ? 'border-green-500/40 bg-green-500/10'
                            : suiteGateSummary.status === 'fail'
                              ? 'border-red-500/40 bg-red-500/10'
                              : 'border-amber-500/40 bg-amber-500/10'
                        }`}
                      >
                        <div className="text-[11px] uppercase tracking-wide text-gray-500">Perf Gate</div>
                        <div className="text-sm font-semibold text-gray-100 mt-1">
                          {suiteGateSummary.status.toUpperCase()}
                        </div>
                        <div className="text-[11px] text-gray-400 mt-1">{suiteGateSummary.message}</div>
                      </div>
                      <div className="rounded-xl border border-[#30363d] bg-[#0d1117] px-3 py-3">
                        <div className="text-[11px] uppercase tracking-wide text-gray-500">Budget Fails</div>
                        <div className="text-sm font-semibold text-gray-100 mt-1">{suiteGateSummary.budgetFailCount}</div>
                      </div>
                      <div className="rounded-xl border border-[#30363d] bg-[#0d1117] px-3 py-3">
                        <div className="text-[11px] uppercase tracking-wide text-gray-500">Baseline Regressions</div>
                        <div className="text-sm font-semibold text-gray-100 mt-1">{suiteGateSummary.regressionCount}</div>
                      </div>
                      <div className="rounded-xl border border-[#30363d] bg-[#0d1117] px-3 py-3">
                        <div className="text-[11px] uppercase tracking-wide text-gray-500">Baseline Scope</div>
                        <div className="text-sm font-semibold text-gray-100 mt-1">
                          {suiteGateSummary.baselineScope === 'pinned'
                            ? 'Locked'
                            : suiteGateSummary.baselineScope === 'adhoc'
                              ? 'Ad hoc'
                              : 'None'}
                        </div>
                        <div className="text-[11px] text-gray-400 mt-1">
                          {suiteGateSummary.comparedCount} comparable op(s)
                        </div>
                      </div>
                    </div>
                  )}

                  {suiteReport.error && (
                    <div className="text-sm text-red-300 rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2">
                      {suiteReport.failed_operation
                        ? `Suite failed at ${suiteReport.failed_operation}: ${suiteReport.error}`
                        : suiteReport.error}
                    </div>
                  )}

                  {compareSuiteReport && (
                    <div className="rounded-xl border border-[#30363d] overflow-hidden">
                      <div className="px-4 py-3 bg-[#0d1117] border-b border-[#30363d] text-sm font-semibold text-gray-100">
                        Suite Comparison
                      </div>
                      <div className="p-4 bg-[#161b22] flex flex-col gap-4">
                        <div className="grid grid-cols-3 gap-3">
                          <div className="rounded-xl border border-green-500/40 bg-green-500/10 px-3 py-3">
                            <div className="text-[11px] uppercase tracking-wide text-gray-500">Faster p50</div>
                            <div className="text-sm font-semibold text-gray-100 mt-1">{suiteComparisonSummary.fasterCount}</div>
                          </div>
                          <div className="rounded-xl border border-red-500/40 bg-red-500/10 px-3 py-3">
                            <div className="text-[11px] uppercase tracking-wide text-gray-500">Slower p50</div>
                            <div className="text-sm font-semibold text-gray-100 mt-1">{suiteComparisonSummary.slowerCount}</div>
                          </div>
                          <div className="rounded-xl border border-[#30363d] bg-[#0d1117] px-3 py-3">
                            <div className="text-[11px] uppercase tracking-wide text-gray-500">Comparable Ops</div>
                            <div className="text-sm font-semibold text-gray-100 mt-1">{suiteComparisonSummary.comparableCount}</div>
                          </div>
                        </div>

                        <div className="rounded-xl border border-[#30363d] overflow-hidden">
                          <table className="w-full text-sm">
                            <thead className="bg-[#0d1117]">
                              <tr className="text-left text-gray-400">
                                <th className="px-4 py-2 font-medium">Operation</th>
                                <th className="px-4 py-2 font-medium">p50 Δ</th>
                                <th className="px-4 py-2 font-medium">p95 Δ</th>
                                <th className="px-4 py-2 font-medium">Avg Δ</th>
                              </tr>
                            </thead>
                            <tbody>
                              {suiteComparisonRows.map((row) => (
                                <tr key={row.operation} className="border-t border-[#30363d]">
                                  <td className="px-4 py-2 text-gray-100">{row.operation}</td>
                                  <td className={`px-4 py-2 ${row.p50.status === 'pass' ? 'text-green-300' : row.p50.status === 'fail' ? 'text-red-300' : 'text-gray-300'}`}>
                                    {row.p50.value}
                                  </td>
                                  <td className={`px-4 py-2 ${row.p95.status === 'pass' ? 'text-green-300' : row.p95.status === 'fail' ? 'text-red-300' : 'text-gray-300'}`}>
                                    {row.p95.value}
                                  </td>
                                  <td className={`px-4 py-2 ${row.avg.status === 'pass' ? 'text-green-300' : row.avg.status === 'fail' ? 'text-red-300' : 'text-gray-300'}`}>
                                    {row.avg.value}
                                  </td>
                                </tr>
                              ))}
                            </tbody>
                          </table>
                        </div>

                        <div className="text-xs text-gray-500">
                          Baseline: {formatRecordedAt(compareSuiteReport.recorded_at)}
                          {compareSuiteReport.label && (
                            <>
                              {' · '}
                              {compareSuiteReport.label}
                            </>
                          )}
                          {compareSuiteReport.connection_name && (
                            <>
                              {' · '}
                              {compareSuiteReport.connection_name}
                            </>
                          )}
                          {compareSuiteReport.build_version && (
                            <>
                              {' · '}
                              {compareSuiteReport.build_version}
                            </>
                          )}
                          {compareSuiteReport.environment && (
                            <>
                              {' · '}
                              {compareSuiteReport.environment}
                            </>
                          )}
                          {compareSuiteReport.branch_name && (
                            <>
                              {' · '}
                              {compareSuiteReport.branch_name}
                            </>
                          )}
                          {compareSuiteReport.notes && (
                            <>
                              {' · '}
                              {compareSuiteReport.notes}
                            </>
                          )}
                        </div>
                      </div>
                    </div>
                  )}

                  <div className="rounded-xl border border-[#30363d] overflow-hidden">
                    <table className="w-full text-sm">
                      <thead className="bg-[#0d1117]">
                        <tr className="text-left text-gray-400">
                          <th className="px-4 py-2 font-medium">Operation</th>
                          <th className="px-4 py-2 font-medium">p50</th>
                          <th className="px-4 py-2 font-medium">p95</th>
                          <th className="px-4 py-2 font-medium">Rows</th>
                        </tr>
                      </thead>
                      <tbody>
                        {suiteReport.results.map((entry) => (
                          <tr key={entry.id} className="border-t border-[#30363d]">
                            <td className="px-4 py-2 text-gray-100">{entry.operation}</td>
                            <td className="px-4 py-2 text-gray-300">{metricValue(entry.result.p50_ms)}</td>
                            <td className="px-4 py-2 text-gray-300">{metricValue(entry.result.p95_ms)}</td>
                            <td className="px-4 py-2 text-gray-300">
                              {entry.result.rows == null ? '-' : String(entry.result.rows)}
                            </td>
                          </tr>
                        ))}
                      </tbody>
                    </table>
                  </div>
                </div>
              </div>
            )}

            {!result && !isRunning && !suiteReport && (
              <div className="flex-1 flex items-center justify-center rounded-xl border border-dashed border-[#30363d] text-sm text-gray-500 min-h-[320px]">
                Run a probe to capture p50 / p95 and raw sample timings.
              </div>
            )}

            {result && (
              <>
                <div className="grid grid-cols-1 lg:grid-cols-[1fr_280px] gap-3">
                  <div className="rounded-xl border border-[#30363d] bg-[#0d1117] px-4 py-3">
                    <div className="text-xs text-gray-400">
                      Operation: <span className="text-gray-200">{result.operation || operation}</span>
                      {activeHistoryEntry?.connection_name && (
                        <>
                          {' • '}
                          Connection: <span className="text-gray-200">{activeHistoryEntry.connection_name}</span>
                        </>
                      )}
                      {activeHistoryEntry?.recorded_at && (
                        <>
                          {' • '}
                          Captured: <span className="text-gray-200">{formatRecordedAt(activeHistoryEntry.recorded_at)}</span>
                        </>
                      )}
                    </div>
                    {(activeHistoryEntry?.sql || activeHistoryEntry?.table_name || result.budget?.source) && (
                      <div className="text-xs text-gray-500 mt-2 break-all">
                        {activeHistoryEntry?.sql && (
                          <>
                            SQL: <span className="text-gray-300">{activeHistoryEntry.sql}</span>
                          </>
                        )}
                        {activeHistoryEntry?.table_name && (
                          <>
                            {activeHistoryEntry?.sql && ' • '}
                            Table: <span className="text-gray-300">{activeHistoryEntry.table_name}</span>
                          </>
                        )}
                        {result.budget?.source && (
                          <>
                            {(activeHistoryEntry?.sql || activeHistoryEntry?.table_name) && ' • '}
                            Budget source: <span className="text-gray-300">{result.budget.source}</span>
                          </>
                        )}
                      </div>
                    )}
                  </div>

                  <div className="rounded-xl border border-[#30363d] bg-[#0d1117] px-4 py-3">
                    <div className="text-xs text-gray-400 mb-2">Compare to baseline</div>
                    <select
                      value={compareBaselineId}
                      onChange={(e) => setCompareBaselineId(e.target.value)}
                      disabled={compareCandidates.length === 0}
                      className="w-full bg-[#161b22] border border-[#30363d] rounded-lg px-3 py-2 text-sm text-gray-100 disabled:opacity-50"
                    >
                      {compareCandidates.length === 0 && <option value="">No baseline for this operation</option>}
                      {compareCandidates.map((entry) => (
                        <option key={entry.id} value={entry.id}>
                          {formatRecordedAt(entry.recorded_at)} • p50 {metricValue(entry.result.p50_ms)}
                        </option>
                      ))}
                    </select>
                  </div>
                </div>

                <div className="grid grid-cols-2 lg:grid-cols-4 gap-3">
                  {[
                    { label: 'Samples', value: String(result.sample_count || 0), status: undefined, detail: '' },
                    {
                      label: 'p50',
                      value: metricValue(result.p50_ms),
                      status: budgetComparison?.p50Status,
                      detail: budgetComparison?.p50Delta,
                    },
                    {
                      label: 'p95',
                      value: metricValue(result.p95_ms),
                      status: budgetComparison?.p95Status,
                      detail: budgetComparison?.p95Delta,
                    },
                    { label: 'Average', value: metricValue(result.avg_ms), status: undefined, detail: '' },
                    { label: 'Min', value: metricValue(result.min_ms), status: undefined, detail: '' },
                    { label: 'Max', value: metricValue(result.max_ms), status: undefined, detail: '' },
                    { label: 'Rows', value: result.rows == null ? '-' : String(result.rows), status: undefined, detail: '' },
                    {
                      label: 'Budget',
                      value: result.budget?.target_p50_ms || result.budget?.target_p95_ms
                        ? `p50 ${metricValue(result.budget?.target_p50_ms)} / p95 ${metricValue(result.budget?.target_p95_ms)}`
                        : '-',
                      status: undefined,
                      detail: '',
                    },
                  ].map((item) => (
                    <div
                      key={item.label}
                      className={`rounded-xl border px-3 py-3 ${
                        item.status === 'pass'
                          ? 'border-green-500/40 bg-green-500/10'
                          : item.status === 'fail'
                            ? 'border-red-500/40 bg-red-500/10'
                            : 'border-[#30363d] bg-[#0d1117]'
                      }`}
                    >
                      <div className="text-[11px] uppercase tracking-wide text-gray-500">{item.label}</div>
                      <div className="text-sm font-semibold text-gray-100 mt-1">{item.value}</div>
                      {item.status === 'pass' && (
                        <div className="text-[11px] text-green-300 mt-1">
                          Within budget{item.detail ? ` - ${item.detail}` : ''}
                        </div>
                      )}
                      {item.status === 'fail' && (
                        <div className="text-[11px] text-red-300 mt-1">
                          Over budget{item.detail ? ` - ${item.detail}` : ''}
                        </div>
                      )}
                    </div>
                  ))}
                </div>

                {compareBaseline && (
                  <div className="rounded-xl border border-[#30363d] overflow-hidden">
                    <div className="px-4 py-3 bg-[#0d1117] border-b border-[#30363d] text-sm font-semibold text-gray-100">
                      Baseline Comparison
                    </div>
                    <div className="grid grid-cols-2 lg:grid-cols-4 gap-3 p-4 bg-[#161b22]">
                      {[
                        {
                          label: 'p50 Δ',
                          ...comparisonDelta(result.p50_ms, compareBaseline.result.p50_ms),
                        },
                        {
                          label: 'p95 Δ',
                          ...comparisonDelta(result.p95_ms, compareBaseline.result.p95_ms),
                        },
                        {
                          label: 'Avg Δ',
                          ...comparisonDelta(result.avg_ms, compareBaseline.result.avg_ms),
                        },
                        {
                          label: 'Max Δ',
                          ...comparisonDelta(result.max_ms, compareBaseline.result.max_ms),
                        },
                      ].map((item) => (
                        <div
                          key={item.label}
                          className={`rounded-xl border px-3 py-3 ${
                            item.status === 'pass'
                              ? 'border-green-500/40 bg-green-500/10'
                              : item.status === 'fail'
                                ? 'border-red-500/40 bg-red-500/10'
                                : 'border-[#30363d] bg-[#0d1117]'
                          }`}
                        >
                          <div className="text-[11px] uppercase tracking-wide text-gray-500">{item.label}</div>
                          <div className="text-sm font-semibold text-gray-100 mt-1">{item.value}</div>
                          {item.detail && <div className="text-[11px] text-gray-400 mt-1">{item.detail}</div>}
                        </div>
                      ))}
                    </div>
                    <div className="px-4 py-3 border-t border-[#30363d] bg-[#0d1117] text-xs text-gray-500">
                      Baseline: {formatRecordedAt(compareBaseline.recorded_at)}
                      {compareBaseline.connection_name && (
                        <>
                          {' • '}
                          {compareBaseline.connection_name}
                        </>
                      )}
                    </div>
                  </div>
                )}

                <div className="rounded-xl border border-[#30363d] overflow-hidden">
                  <div className="px-4 py-3 bg-[#0d1117] border-b border-[#30363d] text-sm font-semibold text-gray-100">
                    Raw Samples
                  </div>
                  <div className="max-h-[360px] overflow-auto">
                    <table className="w-full text-sm">
                      <thead className="bg-[#161b22] sticky top-0">
                        <tr className="text-left text-gray-400">
                          <th className="px-4 py-2 font-medium">#</th>
                          <th className="px-4 py-2 font-medium">Duration</th>
                          <th className="px-4 py-2 font-medium">Rows</th>
                        </tr>
                      </thead>
                      <tbody>
                        {(result.samples || []).map((sample) => (
                          <tr key={`${sample.iteration}-${sample.duration_ms}`} className="border-t border-[#30363d]">
                            <td className="px-4 py-2 text-gray-300">{sample.iteration || '-'}</td>
                            <td className="px-4 py-2 text-gray-100">{metricValue(sample.duration_ms)}</td>
                            <td className="px-4 py-2 text-gray-300">
                              {sample.rows == null ? '-' : String(sample.rows)}
                            </td>
                          </tr>
                        ))}
                      </tbody>
                    </table>
                  </div>
                </div>
              </>
            )}
          </div>
        </div>
      </div>
    </div>
  )
}
