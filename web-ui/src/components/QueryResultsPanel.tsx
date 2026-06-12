import { Copy, Database, Play, Sparkles, Table, Trash2 } from 'lucide-react'
import { DataCharts } from './DataCharts'
import { SimpleDataTable } from './SimpleDataTable'
import { Skeleton } from './Skeleton'
import { Button } from './ui'
import type { QueryErrorInsight, QueryExecutionResult, QueryResultCompareReport } from '../types'
import type { AppError } from '../utils'
import { tr } from '../i18n'

type QueryResultsPanelState = {
  sql: string
  executeResult: QueryExecutionResult | null
  executeResults?: QueryExecutionResult[]
  activeResultIndex?: number
  isExecuting: boolean
  isExplainingError?: boolean
  isLoadingMoreResults?: boolean
  transactionMode?: 'auto' | 'manual'
  transactionState?: 'idle' | 'active' | 'committing' | 'rolling_back'
  errorObj: AppError | null
  lastErrorInsight?: QueryErrorInsight | null
  compareBaselineResult?: QueryExecutionResult | null
  compareBaselineCapturedAt?: number | null
  resultsView: 'table' | 'chart'
}

function formatResultStatus(status?: QueryExecutionResult['status']) {
  if (status === 'error') return 'Error'
  if (status === 'canceled') return 'Canceled'
  return 'Success'
}

function getResultStatusTone(status?: QueryExecutionResult['status']) {
  if (status === 'error') return 'text-red-300 border-red-500/30 bg-red-500/10'
  if (status === 'canceled') return 'text-amber-300 border-amber-500/30 bg-amber-500/10'
  return 'text-emerald-300 border-emerald-500/30 bg-emerald-500/10'
}

function getFooterDotTone(status?: QueryExecutionResult['status'], hasResult?: boolean) {
  if (!hasResult) return 'bg-gray-500/50'
  if (status === 'error') return 'bg-red-500/50'
  if (status === 'canceled') return 'bg-amber-500/50'
  return 'bg-green-500/50'
}

export function QueryResultsPanel({
  tabState,
  resultsPanelHeight,
  isResizingResults,
  onStartResize,
  onSetResultsView,
  onSelectResult,
  onLoadMoreResults,
  onClearResults,
  onExplainErrorWithAi,
  onFixWithAi,
  onApplySuggestedSql,
  compareReport,
  onSetCompareBaseline,
  onClearCompareBaseline,
  onCopyCompareJson,
  onDownloadCompareJson,
  onDownloadCsv,
  onDownloadSql,
  resolveActiveModelLabel,
  resolveActiveTier,
  aiMode,
  queryChunkSize,
  onOpenSortPanel,
}: {
  tabState: QueryResultsPanelState
  resultsPanelHeight: number
  isResizingResults: boolean
  onStartResize: () => void
  onSetResultsView: (view: 'table' | 'chart') => void
  onSelectResult: (index: number) => void
  onLoadMoreResults: () => void
  onClearResults: () => void
  onExplainErrorWithAi: () => void
  onFixWithAi: () => void
  onApplySuggestedSql: () => void
  onOpenSortPanel?: (sql: string) => void
  compareReport?: QueryResultCompareReport | null
  onSetCompareBaseline: () => void
  onClearCompareBaseline: () => void
  onCopyCompareJson: () => void
  onDownloadCompareJson: () => void
  onDownloadCsv: () => void
  onDownloadSql: () => void
  resolveActiveModelLabel: string
  resolveActiveTier: string
  aiMode?: string
  queryChunkSize: number
}) {
  const resultSets = Array.isArray(tabState.executeResults) && tabState.executeResults.length > 0
    ? tabState.executeResults
    : (tabState.executeResult ? [tabState.executeResult] : [])
  const activeResultIndex = resultSets.length === 0
    ? 0
    : Math.min(tabState.activeResultIndex ?? 0, resultSets.length - 1)
  const activeResult = resultSets[activeResultIndex] || tabState.executeResult || null
  const rows = activeResult?.rows ?? []
  const visibleRowCount = rows.length
  const reportedRowCount = activeResult?.row_count ?? visibleRowCount
  const previewCap = activeResult?.preview_cap ?? null
  const chunkSize = activeResult?.chunk_size || queryChunkSize
  const chunkOffset = activeResult?.chunk_offset ?? 0
  const chunkEnd = visibleRowCount > 0 ? chunkOffset + visibleRowCount : 0
  const hasRows = visibleRowCount > 0
  const hasPreviewCap = typeof previewCap === 'number' && previewCap > 0
  const isPreviewTruncated = Boolean(activeResult?.truncated)
  const hasActiveResult = Boolean(activeResult)
  const transactionCanLoadMore =
    tabState.transactionMode !== 'manual' || tabState.transactionState === 'active'
  const activeError = activeResult?.error ?? null
  const panelError = activeError || (resultSets.length === 0 ? tabState.errorObj : null)
  const failedSql = (activeResult?.source_sql || tabState.sql || '').trim()
  const matchingErrorInsight = (
    tabState.lastErrorInsight
    && panelError
    && tabState.lastErrorInsight.error_message === panelError.message
    && tabState.lastErrorInsight.source_sql.trim() === failedSql
  ) ? tabState.lastErrorInsight : null
  const suggestedSql = matchingErrorInsight?.fixed_sql?.trim() || ''
  const hasSuggestedSql = Boolean(suggestedSql)
  const suggestedSqlAlreadyApplied = Boolean(suggestedSql && suggestedSql === tabState.sql.trim())
  const hasSavedBaseline = Boolean(tabState.compareBaselineResult)
  const compareSummary = compareReport?.summary || null
  const currentResultCanBeBaseline = Boolean(
    activeResult
    && activeResult.status === 'success'
    && !activeResult.error
    && (rows.length > 0 || (activeResult.columns?.length ?? 0) > 0)
  )
  const baselineStatementLabel = tabState.compareBaselineResult?.statement_label || 'Saved baseline'
  const currentStatementLabel = activeResult?.statement_label || 'Current result'
  const baselineCapturedLabel = tabState.compareBaselineCapturedAt
    ? new Date(tabState.compareBaselineCapturedAt).toLocaleTimeString()
    : null

  const emitToast = (message: string, type: 'success' | 'error') => {
    window.dispatchEvent(new CustomEvent('global-toast', { detail: { message, type } }))
  }

  const stringifyLoadedRows = () =>
    JSON.stringify(rows, (_key, value) => typeof value === 'bigint' ? value.toString() : value, 2)

  const handleCopyJson = async () => {
    if (!hasRows) return
    try {
      await navigator.clipboard.writeText(stringifyLoadedRows())
      emitToast('Loaded result JSON copied to clipboard', 'success')
    } catch {
      emitToast('Failed to copy loaded result JSON', 'error')
    }
  }

  const handleDownloadJson = () => {
    if (!hasRows) return
    const blob = new Blob([stringifyLoadedRows()], { type: 'application/json;charset=utf-8;' })
    const url = URL.createObjectURL(blob)
    const link = document.createElement('a')
    link.href = url
    link.setAttribute('download', `query_result_${new Date().toISOString().replace(/[:.]/g, '-')}.json`)
    document.body.appendChild(link)
    link.click()
    document.body.removeChild(link)
    URL.revokeObjectURL(url)
  }

  const handleCopyText = async (text: string, successMessage: string, failureMessage: string) => {
    if (!text.trim()) return
    try {
      await navigator.clipboard.writeText(text)
      emitToast(successMessage, 'success')
    } catch {
      emitToast(failureMessage, 'error')
    }
  }

  return (
    <>
      <div
        className={`h-2 shrink-0 cursor-row-resize bg-dark-bg border-y border-dark-border flex items-center justify-center ${isResizingResults ? 'bg-dark-panel' : ''}`}
        onMouseDown={onStartResize}
        title="Drag to resize results panel"
      >
        <div className={`h-1 w-12 rounded-full transition-colors ${isResizingResults ? 'bg-blue-500/70' : 'bg-gray-600/70'}`} />
      </div>

      <div className="bg-dark-bg flex flex-col relative shrink-0 min-h-0" style={{ height: `${resultsPanelHeight}px` }}>
        <div className="border-b border-dark-border bg-dark-panel px-4 py-2 flex flex-col gap-2">
          <div className="flex items-start justify-between gap-3">
            <div className="flex min-w-0 flex-1 flex-wrap items-center gap-3">
              <span className="text-xs font-bold tracking-wider text-gray-400 uppercase">Results</span>
              {resultSets.length > 1 && (
                <span className="text-[11px] text-gray-400">
                  Statement {activeResultIndex + 1}/{resultSets.length}
                </span>
              )}
              {activeResult?.statement_label && (
                <span className={`text-[11px] px-2 py-0.5 rounded border ${getResultStatusTone(activeResult.status)}`}>
                  {activeResult.statement_label} - {formatResultStatus(activeResult.status)}
                </span>
              )}
              {hasPreviewCap && (
                <span className="text-[11px] text-amber-400/80">
                  Preview {reportedRowCount}/{previewCap}
                </span>
              )}
              {hasRows && (
                <span className="text-[11px] text-gray-400">
                  Loaded {visibleRowCount} rows
                </span>
              )}
              {activeResult?.has_more && (
                <span className="text-[11px] text-blue-300/80">
                  More rows available
                </span>
              )}
              {isPreviewTruncated && (
                <span className="text-[11px] text-rose-300/80">
                  Truncated at preview cap
                </span>
              )}
              {hasRows && (
                <div className="flex items-center bg-dark-surface rounded overflow-hidden border border-dark-border">
                  <button
                    onClick={() => onSetResultsView('table')}
                    className={`px-2 py-0.5 text-xs transition-colors ${tabState.resultsView === 'table' ? 'bg-blue-500/20 text-blue-400' : 'text-gray-400 hover:bg-dark-border'}`}
                  >
                    Table
                  </button>
                  <button
                    onClick={() => onSetResultsView('chart')}
                    className={`px-2 py-0.5 text-xs transition-colors ${tabState.resultsView === 'chart' ? 'bg-blue-500/20 text-blue-400' : 'text-gray-400 hover:bg-dark-border'}`}
                  >
                    Chart
                  </button>
                </div>
              )}
            </div>
            {hasActiveResult && (
              <div className="flex flex-wrap items-center justify-end gap-2">
                {activeResult?.has_more && !panelError && transactionCanLoadMore && (
                  <Button
                    variant="toolbar"
                    size="sm"
                    onClick={onLoadMoreResults}
                    disabled={Boolean(tabState.isLoadingMoreResults)}
                    className="text-blue-300 disabled:cursor-wait"
                  >
                    {tabState.isLoadingMoreResults ? 'Loading...' : `Load More (${activeResult.chunk_size || queryChunkSize})`}
                  </Button>
                )}
                {activeResult?.status === 'success' && tabState.sql && onOpenSortPanel && (() => {
                  const trimmed = tabState.sql.trim().replace(/;+$/, '')
                  const first = trimmed.split(/\s+/)[0]?.toUpperCase() || ''
                  const isSelect = first === 'SELECT' || first === 'WITH'
                  return (
                    <Button
                      variant="toolbar"
                      size="sm"
                      type="button"
                      disabled={!isSelect}
                      onClick={() => onOpenSortPanel(trimmed)}
                      title={isSelect
                        ? tr('打开排序面板', 'Open sort panel')
                        : tr('仅 SELECT 结果支持排序', 'Only SELECT results support sorting')}
                      className="text-purple-300 disabled:opacity-40 disabled:hover:bg-dark-surface"
                    >
                      {tr('排序', 'Sort')}
                    </Button>
                  )
                })()}
                <Button
                  variant="toolbar"
                  size="sm"
                  onClick={onClearResults}
                  className="text-red-400 hover:text-red-300"
                >
                  <Trash2 className="w-3 h-3" />
                  Clear Results
                </Button>
                {currentResultCanBeBaseline && (
                  <button
                    onClick={onSetCompareBaseline}
                    className="text-xs text-violet-300 hover:text-white bg-violet-500/10 hover:bg-violet-500/20 px-2 py-0.5 rounded border border-violet-500/30 transition-colors"
                  >
                    Set as Baseline
                  </button>
                )}
                {hasSavedBaseline && (
                  <button
                    onClick={onClearCompareBaseline}
                    className="text-xs text-violet-200 hover:text-white bg-dark-surface hover:bg-dark-border px-2 py-0.5 rounded border border-violet-500/20 transition-colors"
                  >
                    Clear Baseline
                  </button>
                )}
                {hasRows && (
                  <>
                    <Button
                      variant="toolbar"
                      size="sm"
                      onClick={handleCopyJson}
                      className="gap-1"
                    >
                      <Copy className="w-3 h-3" />
                      Copy JSON
                    </Button>
                    <Button
                      variant="toolbar"
                      size="sm"
                      onClick={handleDownloadJson}
                    >
                      Download JSON
                    </Button>
                    <Button
                      variant="toolbar"
                      size="sm"
                      onClick={onDownloadCsv}
                    >
                      Download CSV
                    </Button>
                    <Button
                      variant="toolbar"
                      size="sm"
                      onClick={onDownloadSql}
                    >
                      Download SQL
                    </Button>
                  </>
                )}
              </div>
            )}
          </div>

          {resultSets.length > 1 && (
            <div className="flex items-center gap-2 overflow-x-auto pb-0.5">
              {resultSets.map((result, index) => {
                const isActive = index === activeResultIndex
                const resultStatus = formatResultStatus(result.status)
                const secondaryText = result.error
                  ? resultStatus
                  : result.rows?.length
                    ? `${result.rows.length} rows`
                    : `${result.affected_rows ?? result.row_count ?? 0} affected`
                return (
                  <button
                    key={`${result.statement_label || result.statement_kind || 'statement'}-${index}`}
                    onClick={() => onSelectResult(index)}
                    className={`flex min-w-[140px] items-center justify-between gap-3 rounded border px-3 py-1.5 text-left text-xs transition-colors ${isActive ? 'border-blue-500/50 bg-blue-500/10 text-white' : 'border-dark-border bg-dark-panel text-gray-300 hover:bg-dark-surface'}`}
                  >
                    <div className="min-w-0">
                      <div className="truncate font-medium">{result.statement_label || `Statement ${index + 1}`}</div>
                      <div className="truncate text-[11px] text-gray-400">{result.statement_kind || 'STATEMENT'}</div>
                    </div>
                    <span className={`shrink-0 rounded border px-1.5 py-0.5 text-[11px] ${getResultStatusTone(result.status)}`}>
                      {secondaryText}
                    </span>
                  </button>
                )
              })}
            </div>
          )}

          {hasSavedBaseline && (
            <div className="rounded border border-violet-500/20 bg-violet-500/5 px-3 py-3">
              <div className="flex flex-wrap items-start justify-between gap-3">
                <div className="min-w-0">
                  <div className="text-xs font-semibold uppercase tracking-wide text-violet-300">
                    Result Compare Baseline
                  </div>
                  <div className="mt-1 text-xs text-gray-300">
                    <span className="font-medium text-gray-200">{baselineStatementLabel}</span>
                    {baselineCapturedLabel ? ` saved at ${baselineCapturedLabel}` : ''}
                  </div>
                  <div className="mt-1 text-[11px] text-gray-400">
                    Current target: {currentStatementLabel}
                  </div>
                </div>
                <div className="flex flex-wrap items-center gap-2">
                  <button
                    onClick={onClearCompareBaseline}
                    className="text-xs text-violet-200 hover:text-white bg-dark-surface hover:bg-dark-border px-2 py-1 rounded border border-violet-500/20 transition-colors"
                  >
                    Clear Baseline
                  </button>
                  <Button
                    variant="toolbar"
                    size="md"
                    onClick={onCopyCompareJson}
                    disabled={!compareSummary}
                  >
                    Copy Compare
                  </Button>
                  <Button
                    variant="toolbar"
                    size="md"
                    onClick={onDownloadCompareJson}
                    disabled={!compareSummary}
                  >
                    Download Compare JSON
                  </Button>
                </div>
              </div>

              {compareSummary ? (
                <div className="mt-3 grid gap-2 sm:grid-cols-2 xl:grid-cols-5">
                  {[
                    ['Baseline', compareSummary.baseline_row_count],
                    ['Current', compareSummary.current_row_count],
                    ['Unchanged', compareSummary.unchanged_count],
                    ['Added', compareSummary.added_count],
                    ['Removed', compareSummary.removed_count],
                  ].map(([label, value]) => (
                    <div key={label} className="rounded border border-dark-border bg-dark-bg px-3 py-2">
                      <div className="text-[11px] uppercase tracking-wide text-gray-500">{label}</div>
                      <div className="mt-1 text-sm font-semibold text-gray-200">{value}</div>
                    </div>
                  ))}
                </div>
              ) : (
                <div className="mt-3 text-xs text-amber-300/80">
                  Run or select a successful row-based result to compare against the saved baseline.
                </div>
              )}
            </div>
          )}
        </div>
        <div className="flex-1 overflow-auto bg-grid-pattern relative">
          {tabState.isExecuting && (
            <div className="absolute inset-0 z-30 bg-dark-canvas/80 backdrop-blur-sm p-4 flex flex-col gap-3">
              <div className="flex items-center gap-4 mb-4">
                <Skeleton className="h-6 w-24" />
                <Skeleton className="h-6 w-32" />
                <Skeleton className="h-6 w-20" />
              </div>
              {Array.from({ length: 5 }).map((_, i) => (
                <Skeleton key={i} className="h-8 w-full" />
              ))}
              <div className="absolute inset-0 flex items-center justify-center pointer-events-none">
                <div className="flex flex-col items-center gap-3 text-blue-400 font-medium animate-pulse">
                  <Database className="w-8 h-8" />
                  <span>Running query...</span>
                </div>
              </div>
            </div>
          )}
          {panelError && (
            <div className="absolute inset-0 p-4 bg-red-950/20 backdrop-blur-sm z-20 flex flex-col items-center justify-center">
              <div className="bg-dark-panel border border-red-500/30 rounded-lg p-5 max-w-2xl w-full shadow-2xl">
                {activeResult?.statement_label && (
                  <div className="text-[11px] uppercase tracking-wide text-gray-500 mb-2">
                    {activeResult.statement_label}
                  </div>
                )}
                <div className="flex items-center gap-2 text-red-400 font-semibold mb-2">
                  <span className="w-2 h-2 rounded-full bg-red-500"></span>
                  {panelError.title}
                </div>
                <div className="text-gray-300 text-sm mb-4 p-3 bg-red-500/10 rounded border border-red-500/20 font-mono overflow-auto">
                  {panelError.message}
                </div>
                {panelError.solution && (
                  <div className="text-sm">
                    <span className="text-blue-400 font-medium">Solution: </span>
                    <span className="text-gray-400">{panelError.solution}</span>
                  </div>
                )}
                {failedSql && (
                  <div className="mt-4 text-sm">
                    <div className="mb-1 text-gray-500 uppercase tracking-wide text-[11px]">Failed SQL</div>
                    <pre className="max-h-40 overflow-auto rounded border border-dark-border bg-dark-bg p-3 text-xs text-gray-300 whitespace-pre-wrap break-words">
                      {failedSql}
                    </pre>
                    <div className="mt-2 flex flex-wrap gap-2">
                      <Button
                        variant="toolbar"
                        size="md"
                        onClick={() => void handleCopyText(failedSql, 'Failed SQL copied', 'Failed to copy failed SQL')}
                      >
                        Copy SQL
                      </Button>
                      <button
                        onClick={onExplainErrorWithAi}
                        disabled={Boolean(tabState.isExplainingError)}
                        className="text-xs text-blue-300 hover:text-white bg-blue-500/10 hover:bg-blue-500/20 px-2 py-1 rounded border border-blue-500/30 transition-colors disabled:opacity-50 disabled:cursor-wait"
                      >
                        {tabState.isExplainingError ? 'Analyzing...' : 'Explain Error'}
                      </button>
                    </div>
                  </div>
                )}
                {matchingErrorInsight && (
                  <div className="mt-4 rounded border border-blue-500/20 bg-blue-500/5 p-4">
                    <div className="flex items-center justify-between gap-3 mb-2">
                      <div className="text-sm font-medium text-blue-300">AI Error Analysis</div>
                      <span className="text-[11px] text-gray-500">
                        {matchingErrorInsight.statement_label || matchingErrorInsight.statement_kind || 'Current statement'}
                      </span>
                    </div>
                    <div className="text-sm text-gray-300 whitespace-pre-wrap break-words">
                      {matchingErrorInsight.explanation}
                    </div>
                    {hasSuggestedSql && (
                      <div className="mt-4">
                        <div className="mb-1 text-gray-500 uppercase tracking-wide text-[11px]">Suggested SQL</div>
                        <pre className="max-h-40 overflow-auto rounded border border-dark-border bg-dark-bg p-3 text-xs text-gray-300 whitespace-pre-wrap break-words">
                          {suggestedSql}
                        </pre>
                        <div className="mt-2 flex flex-wrap gap-2">
                          <button
                            onClick={onApplySuggestedSql}
                            disabled={suggestedSqlAlreadyApplied}
                            className="text-xs text-emerald-300 hover:text-white bg-emerald-500/10 hover:bg-emerald-500/20 px-2 py-1 rounded border border-emerald-500/30 transition-colors disabled:opacity-50 disabled:cursor-not-allowed"
                          >
                            {suggestedSqlAlreadyApplied ? 'Already Applied' : 'Apply Suggested SQL'}
                          </button>
                          <Button
                            variant="toolbar"
                            size="md"
                            onClick={() => void handleCopyText(suggestedSql, 'Suggested SQL copied', 'Failed to copy suggested SQL')}
                          >
                            Copy Suggestion
                          </Button>
                        </div>
                      </div>
                    )}
                  </div>
                )}
                <div className="mt-4 flex justify-end">
                  <button
                    onClick={onFixWithAi}
                    disabled={Boolean(tabState.isExplainingError)}
                    className="flex items-center gap-2 px-4 py-2 bg-blue-600 hover:bg-blue-500 text-white rounded text-sm font-medium transition-colors shadow-lg shadow-blue-500/20 disabled:opacity-50 disabled:cursor-wait"
                  >
                    <Sparkles className="w-4 h-4" />
                    {tabState.isExplainingError ? 'Analyzing...' : 'Fix with AI'}
                  </button>
                </div>
              </div>
            </div>
          )}

          {!hasActiveResult && !panelError && (
            <div className="absolute inset-0 flex flex-col items-center justify-center text-gray-500 pointer-events-none p-6">
              <div className="w-16 h-16 bg-dark-panel rounded-2xl border border-dark-border flex items-center justify-center mb-4 shadow-lg">
                <Play className="w-8 h-8 text-gray-600" />
              </div>
              <h3 className="text-lg font-medium text-gray-400 mb-2">Awaiting Execution</h3>
              <p className="text-sm text-center max-w-md">
                Use <kbd className="bg-dark-surface px-1.5 py-0.5 rounded text-gray-300 mx-1">Cmd/Ctrl + K</kbd> to generate SQL.<br />
                Review it, then click <span className="text-green-500">Run</span> to display the results here.
              </p>
            </div>
          )}

          {activeResult && !panelError ? (
            <div className="w-full h-full">
              {hasRows ? (
                tabState.resultsView === 'table' ? (
                  <SimpleDataTable data={rows} />
                ) : (
                  <DataCharts data={rows} />
                )
              ) : (
                <div className="flex flex-col items-center justify-center h-full text-gray-500 gap-2">
                  <span className={`font-medium ${activeResult.status === 'canceled' ? 'text-amber-400/80' : 'text-green-500/80'}`}>
                    {activeResult.status === 'canceled' ? 'Statement canceled' : 'Query executed successfully'}
                  </span>
                  <span>{activeResult.affected_rows ?? activeResult.row_count ?? 0} rows affected.</span>
                </div>
              )}
            </div>
          ) : !panelError ? (
            <div className="flex flex-col items-center justify-center h-full text-gray-500 gap-2">
              <Table className="w-8 h-8 opacity-20" />
              <span>No results to display. Click Run to execute query.</span>
            </div>
          ) : null}
        </div>
        <div className="h-7 border-t border-dark-border bg-dark-panel px-4 flex items-center text-xs text-gray-500 justify-between">
          <div className="flex items-center gap-4 min-w-0 overflow-hidden">
            <span className="flex items-center gap-1.5 min-w-0 truncate">
              <div className={`w-2 h-2 rounded-full ${getFooterDotTone(activeResult?.status, hasActiveResult)}`}></div>
              {hasRows ? `${visibleRowCount} loaded` : (activeResult?.row_count ?? activeResult?.affected_rows ?? 0)} rows
              {resultSets.length > 1 ? ` | Statement ${activeResultIndex + 1}/${resultSets.length}` : ''}
              {hasRows && reportedRowCount !== visibleRowCount ? ` | Showing ${visibleRowCount}/${reportedRowCount}` : ''}
              {hasRows ? ` | Chunk ${chunkOffset + 1}-${chunkEnd}` : ''}
              {activeResult?.has_more ? ` | Next chunk ${chunkSize}` : ''}
              {hasPreviewCap ? ` | Preview cap ${previewCap}` : ''}
              {isPreviewTruncated ? ' | Truncated' : ''}
              {activeResult?.execution_time_ms !== undefined && ` | Executed in ${activeResult.execution_time_ms} ms`}
            </span>
            <span className="opacity-50">|</span>
            <span>{resolveActiveModelLabel}{resolveActiveTier ? ` - ${resolveActiveTier}` : ''}</span>
          </div>
          <div className="flex items-center gap-2 text-dark-accent/80">
            <Sparkles className="w-3 h-3" />
            <span className="capitalize">{String(aiMode || 'Direct')} Mode</span>
          </div>
        </div>
      </div>
    </>
  )
}
