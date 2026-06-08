import { api } from './api'
import type { ChatMessage } from './api'
import type { ToastType } from './components/Toast'
import type { AgentStep } from './components/AgentProcessPanel'
import type { QueryErrorInsight } from './types'
import { formatErr, parseError } from './utils'
import type { AppError } from './utils'

type ToastFn = (message: string, type?: ToastType) => void

type QueryAiHistoryItem = ChatMessage

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
  agentSteps?: AgentStep[]
  abortController?: AbortController | null
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

export async function runGenerateSqlStream(params: {
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

  // Create an AbortController for cancellation support
  const abortController = new AbortController()

  const steps: AgentStep[] = []
  updateActiveTabState({
    isGenerating: true,
    errorObj: null,
    lastExplanation: null,
    agentSteps: [],
    abortController,
  })

  try {
    const chatHistory = Array.isArray(activeTabState.chatHistory) ? activeTabState.chatHistory : []
    const historyToPass = chatHistory.slice(-5).filter(msg => msg && Object.keys(msg).length > 0)
    const body = await api.chatToSqlStream(q, historyToPass, undefined, undefined, abortController.signal)
    if (!body) throw new Error('No response body')

    const reader = body.getReader()
    const decoder = new TextDecoder()
    let buffer = ''
    let finalSql = ''
    let finalExplanation = ''

    const flushStep = (eventType: string, data: Record<string, unknown>) => {
      const step: AgentStep = { type: eventType as AgentStep['type'], ...data } as AgentStep
      steps.push(step)
      updateActiveTabState({ agentSteps: [...steps] })
    }

    while (true) {
      const { done, value } = await reader.read()
      if (done) break

      buffer += decoder.decode(value, { stream: true })

      // SSE events are delimited by double newlines.
      // Split on \n\n to get complete event blocks, handling payloads that contain \n.
      const parts = buffer.split('\n\n')
      buffer = parts.pop() || '' // Keep incomplete trailing block

      for (const block of parts) {
        if (!block.trim()) continue
        let eventType = 'message'
        let dataStr = ''

        for (const line of block.split('\n')) {
          if (line.startsWith('event: ')) {
            eventType = line.slice(7).trim()
          } else if (line.startsWith('data: ')) {
            dataStr = line.slice(6)
          }
        }

        if (!dataStr) continue
        try {
          const data = JSON.parse(dataStr)

          switch (eventType) {
            case 'thinking':
              flushStep('thinking', { text: data.text })
              break
            case 'tool_call':
              flushStep('tool_call', { tool: data.tool, args: data.args, callId: data.call_id })
              break
            case 'tool_result':
              flushStep('tool_result', { tool: data.tool, result: data.result, callId: data.call_id })
              break
            case 'sql_draft':
              flushStep('sql_draft', { sql: data.sql })
              break
            case 'final_sql':
              finalSql = data.sql || ''
              flushStep('final_sql', { sql: data.sql, taskType: data.task_type })
              break
            case 'explanation':
              finalExplanation = data.text || ''
              flushStep('explanation', { text: data.text })
              break
            case 'error':
              flushStep('error', { message: data.message })
              break
            case 'token_usage':
              flushStep('token_usage', { promptTokens: data.prompt_tokens, completionTokens: data.completion_tokens, totalTokens: data.total_tokens })
              break
            case 'done':
              // Stream complete
              break
            default:
              // Fallback: try to extract from data fields
              if (data.sql && !finalSql) finalSql = data.sql
              if (data.text && !finalExplanation) finalExplanation = data.text
              if (data.message) {
                flushStep('error', { message: data.message })
              }
              break
          }
        } catch { /* ignore parse errors in stream */ }
      }
    }

    const newHistory = [...chatHistory, { role: 'user', content: q }, { role: 'assistant', content: finalSql || '' }]
    updateActiveTabState({
      sql: finalSql || activeTabState.query,
      lastExplanation: finalExplanation || null,
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
    updateActiveTabState({ isGenerating: false, abortController: null })
  }
}

/** Cancel an in-progress Agent stream by aborting the fetch request */
export function cancelAgentStream(abortController: AbortController | null) {
  if (abortController && !abortController.signal.aborted) {
    abortController.abort()
  }
}
