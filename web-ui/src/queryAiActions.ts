import { api } from './api'
import type { ToastType } from './components/Toast'
import type { QueryErrorInsight } from './types'
import { formatErr, parseError } from './utils'
import type { AppError } from './utils'

type ToastFn = (message: string, type?: ToastType) => void

type QueryAiHistoryItem = Record<string, unknown>

type QueryAiTabState = {
  query: string
  chatHistory: QueryAiHistoryItem[]
  errorObj: AppError | null
}

type QueryAiTabPatch = {
  isGenerating?: boolean
  isExplainingError?: boolean
  errorObj?: AppError | null
  lastExplanation?: string | null
  lastErrorInsight?: QueryErrorInsight | null
  sql?: string
  lastQuery?: string
  query?: string
  chatHistory?: QueryAiHistoryItem[]
}

type UpdateQueryAiTabState = (patch: QueryAiTabPatch) => void

async function requestQueryErrorInsight(params: {
  currentSql: string
  errorObj: AppError
  statementLabel?: string | null
  statementKind?: string | null
}): Promise<QueryErrorInsight> {
  const { currentSql, errorObj, statementLabel, statementKind } = params
  const result = await api.aiExplainError(errorObj.message, currentSql)
  return {
    source_sql: currentSql,
    error_message: errorObj.message,
    explanation: result.explanation || 'AI did not return an explanation.',
    fixed_sql: typeof result.fixed_query === 'string' ? result.fixed_query : null,
    statement_label: statementLabel || null,
    statement_kind: statementKind || null,
    generated_at: Date.now(),
  }
}

export async function runAiOptimize(params: {
  currentSql: string
  updateActiveTabState: UpdateQueryAiTabState
  toast: ToastFn
}) {
  const { currentSql, updateActiveTabState, toast } = params
  if (!currentSql || !currentSql.trim() || currentSql.trim() === '-- Generated SQL will appear here') {
    return
  }

  updateActiveTabState({ isGenerating: true, errorObj: null, lastExplanation: null })
  try {
    const result = await api.aiQuery({
      query: 'Please optimize the current SQL query and explain the improvements.',
      mode: 'optimize',
      current_sql: currentSql,
    })
    if (result.sql && result.sql !== currentSql && !result.sql.includes('Please optimize')) {
      updateActiveTabState({ sql: result.sql })
    }
    if (result.explanation) {
      updateActiveTabState({ lastExplanation: result.explanation })
    }
    toast('AI optimize complete', 'success')
  } catch (e: unknown) {
    updateActiveTabState({ errorObj: parseError(e) })
  } finally {
    updateActiveTabState({ isGenerating: false })
  }
}

export async function runAiExplain(params: {
  currentSql: string
  updateActiveTabState: UpdateQueryAiTabState
  toast: ToastFn
}) {
  const { currentSql, updateActiveTabState, toast } = params
  if (!currentSql || !currentSql.trim() || currentSql.trim() === '-- Generated SQL will appear here') {
    return
  }

  updateActiveTabState({ isGenerating: true, errorObj: null, lastExplanation: null })
  try {
    const result = await api.aiQuery({
      query: 'Please explain the current SQL query in detail.',
      mode: 'explain',
      current_sql: currentSql,
    })
    if (result.explanation) {
      updateActiveTabState({ lastExplanation: result.explanation })
    }
    toast('AI explain complete', 'success')
  } catch (e: unknown) {
    updateActiveTabState({ errorObj: parseError(e) })
  } finally {
    updateActiveTabState({ isGenerating: false })
  }
}

export async function runFixWithAi(params: {
  currentSql: string
  errorObj: AppError | null
  statementLabel?: string | null
  statementKind?: string | null
  updateActiveTabState: UpdateQueryAiTabState
  toast: ToastFn
}) {
  const { currentSql, errorObj, statementLabel, statementKind, updateActiveTabState, toast } = params
  if (!errorObj?.message) return

  updateActiveTabState({ isExplainingError: true, lastErrorInsight: null })
  try {
    const insight = await requestQueryErrorInsight({
      currentSql,
      errorObj,
      statementLabel,
      statementKind,
    })
    updateActiveTabState({
      lastErrorInsight: insight,
      lastExplanation: insight.explanation,
      ...(insight.fixed_sql && insight.fixed_sql !== currentSql ? { sql: insight.fixed_sql } : {}),
    })
    toast(
      insight.fixed_sql && insight.fixed_sql !== currentSql
        ? 'AI prepared a SQL fix suggestion'
        : 'AI explained the query error',
      'success'
    )
  } catch (e: unknown) {
    toast(`AI fix failed: ${formatErr(e)}`, 'error')
  } finally {
    updateActiveTabState({ isExplainingError: false })
  }
}

export async function runExplainErrorWithAi(params: {
  currentSql: string
  errorObj: AppError | null
  statementLabel?: string | null
  statementKind?: string | null
  updateActiveTabState: UpdateQueryAiTabState
  toast: ToastFn
}) {
  const { currentSql, errorObj, statementLabel, statementKind, updateActiveTabState, toast } = params
  if (!errorObj?.message) return

  updateActiveTabState({ isExplainingError: true, lastErrorInsight: null })
  try {
    const insight = await requestQueryErrorInsight({
      currentSql,
      errorObj,
      statementLabel,
      statementKind,
    })
    updateActiveTabState({
      lastErrorInsight: insight,
      lastExplanation: insight.explanation,
    })
    toast('AI explained the query error', 'success')
  } catch (e: unknown) {
    toast(`AI error analysis failed: ${formatErr(e)}`, 'error')
  } finally {
    updateActiveTabState({ isExplainingError: false })
  }
}

export async function runGenerateSql(params: {
  overrideQuery?: string
  activeTabState: QueryAiTabState
  updateActiveTabState: UpdateQueryAiTabState
  setShowCommandPalette: (show: boolean) => void
  setShowOnboarding: (show: boolean) => void
}) {
  const {
    overrideQuery,
    activeTabState,
    updateActiveTabState,
    setShowCommandPalette,
    setShowOnboarding,
  } = params

  const q = overrideQuery || activeTabState.query
  if (!q.trim()) return

  updateActiveTabState({ isGenerating: true, errorObj: null, lastExplanation: null })

  try {
    const chatHistory = Array.isArray(activeTabState.chatHistory) ? activeTabState.chatHistory : []
    const historyToPass = chatHistory.slice(-5).filter(msg => msg && Object.keys(msg).length > 0)
    const result = await api.chatToSql(q, historyToPass)
    const newHistory = [...chatHistory, { role: 'user', content: q }, { role: 'assistant', content: result.sql }]

    updateActiveTabState({
      sql: result.sql,
      lastExplanation: result.explanation || null,
      lastQuery: q,
      query: '',
      chatHistory: newHistory,
    })
    setShowCommandPalette(false)
  } catch (e: unknown) {
    const err = parseError(e)
    updateActiveTabState({ errorObj: err })
    if (err.title.includes('Auth Error')) {
      setTimeout(() => setShowOnboarding(true), 1500)
    }
  } finally {
    updateActiveTabState({ isGenerating: false })
  }
}
