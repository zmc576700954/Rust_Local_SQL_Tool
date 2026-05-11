import { useCallback, useEffect, useMemo, useState } from 'react'
import { Copy, Database, RefreshCw, Server, Shield } from 'lucide-react'
import { api } from '../api'
import type { WorkbenchSessionInfo } from '../types'
import { parseError } from '../utils'
import type { AppError } from '../utils'

interface SessionInfoPanelProps {
  dbId?: string | null
  dbLabel?: string | null
  dbType: string
  isActive: boolean
  transactionMode?: 'auto' | 'manual'
  transactionState?: 'idle' | 'active' | 'committing' | 'rolling_back'
}

function formatKeyLabel(key: string) {
  return key
    .split('_')
    .filter(Boolean)
    .map((part) => part.charAt(0).toUpperCase() + part.slice(1))
    .join(' ')
}

function emitToast(message: string, type: 'success' | 'error') {
  window.dispatchEvent(new CustomEvent('global-toast', { detail: { message, type } }))
}

function renderValue(value: string | null | undefined) {
  if (value === null || value === undefined || value === '') return '—'
  return value
}

export function SessionInfoPanel({
  dbId,
  dbLabel,
  dbType,
  isActive,
  transactionMode,
  transactionState,
}: SessionInfoPanelProps) {
  const [sessionInfo, setSessionInfo] = useState<WorkbenchSessionInfo | null>(null)
  const [isLoading, setIsLoading] = useState(false)
  const [error, setError] = useState<AppError | null>(null)

  const loadSessionInfo = useCallback(async () => {
    setIsLoading(true)
    setError(null)
    try {
      const result = await api.getSessionInfo(dbId || undefined)
      setSessionInfo(result)
    } catch (e: unknown) {
      setError(parseError(e))
    } finally {
      setIsLoading(false)
    }
  }, [dbId])

  useEffect(() => {
    if (!isActive) return
    void loadSessionInfo()
  }, [dbId, isActive, loadSessionInfo])

  const summaryItems = useMemo(() => {
    const remoteSummary = sessionInfo?.summary || []
    return [
      { key: 'db_type', value: dbType || null },
      { key: 'connection_name', value: dbLabel || sessionInfo?.connection_name || null },
      { key: 'read_only', value: sessionInfo ? (sessionInfo.read_only ? 'yes' : 'no') : null },
      { key: 'transaction_mode', value: transactionMode || 'auto' },
      { key: 'transaction_state', value: transactionState || 'idle' },
      ...remoteSummary,
    ]
  }, [dbLabel, dbType, sessionInfo, transactionMode, transactionState])

  const copyText = async (text: string, successMessage: string) => {
    try {
      await navigator.clipboard.writeText(text)
      emitToast(successMessage, 'success')
    } catch {
      emitToast('Copy failed', 'error')
    }
  }

  const handleCopyJson = () => {
    if (!sessionInfo) return
    void copyText(JSON.stringify(sessionInfo, null, 2), 'Session info JSON copied')
  }

  const handleCopySection = (section: 'summary' | 'session' | 'global') => {
    if (!sessionInfo) return
    const items =
      section === 'summary'
        ? summaryItems
        : section === 'session'
          ? sessionInfo.session_variables
          : sessionInfo.global_variables
    const content = items
      .map((item) => `${item.key}=${item.value ?? ''}`)
      .join('\n')
    if (!content.trim()) return
    void copyText(content, `${section} info copied`)
  }

  return (
    <div className="flex h-full flex-col bg-[#0a0c10] text-gray-200">
      <div className="flex items-center justify-between border-b border-dark-border bg-dark-panel px-5 py-3">
        <div className="min-w-0">
          <div className="flex items-center gap-2 text-sm font-semibold text-gray-100">
            <Server className="h-4 w-4 text-cyan-300" />
            <span>Session Info</span>
          </div>
          <div className="mt-1 flex flex-wrap items-center gap-2 text-xs text-gray-400">
            <span className="rounded border border-cyan-500/20 bg-cyan-500/10 px-2 py-0.5 text-cyan-200">
              {dbType || 'Database'}
            </span>
            {dbLabel && <span>{dbLabel}</span>}
            {dbId && <span className="opacity-70">db_id: {dbId}</span>}
          </div>
        </div>
        <div className="flex items-center gap-2">
          <button
            onClick={() => void loadSessionInfo()}
            disabled={isLoading}
            className="flex items-center gap-2 rounded border border-[#30363d] bg-[#161b22] px-3 py-1.5 text-xs text-gray-300 transition-colors hover:bg-[#21262d] hover:text-white disabled:cursor-wait disabled:opacity-50"
          >
            <RefreshCw className={`h-3.5 w-3.5 ${isLoading ? 'animate-spin' : ''}`} />
            Refresh
          </button>
          <button
            onClick={handleCopyJson}
            disabled={!sessionInfo}
            className="flex items-center gap-2 rounded border border-[#30363d] bg-[#161b22] px-3 py-1.5 text-xs text-gray-300 transition-colors hover:bg-[#21262d] hover:text-white disabled:opacity-50"
          >
            <Copy className="h-3.5 w-3.5" />
            Copy JSON
          </button>
        </div>
      </div>

      <div className="flex-1 overflow-auto p-5">
        {error && !sessionInfo && (
          <div className="rounded-lg border border-red-500/30 bg-red-500/10 p-4">
            <div className="text-sm font-semibold text-red-300">{error.title || 'Failed to load session info'}</div>
            <div className="mt-2 text-sm text-gray-300">{error.message}</div>
            {error.solution && (
              <div className="mt-2 text-sm text-gray-400">
                Solution: {error.solution}
              </div>
            )}
          </div>
        )}

        {!sessionInfo && isLoading && (
          <div className="flex h-full items-center justify-center text-sm text-gray-400">
            Loading session info...
          </div>
        )}

        {sessionInfo && (
          <div className="space-y-5">
            <section className="rounded-xl border border-[#30363d] bg-[#0d1117]">
              <div className="flex items-center justify-between border-b border-[#30363d] px-4 py-3">
                <div className="flex items-center gap-2 text-sm font-medium text-gray-100">
                  <Database className="h-4 w-4 text-blue-300" />
                  Summary
                </div>
                <button
                  onClick={() => handleCopySection('summary')}
                  className="text-xs text-gray-400 transition-colors hover:text-white"
                >
                  Copy
                </button>
              </div>
              <div className="grid gap-3 p-4 md:grid-cols-2 xl:grid-cols-3">
                {summaryItems.map((item) => (
                  <div key={item.key} className="rounded-lg border border-[#30363d] bg-[#161b22] p-3">
                    <div className="text-[11px] uppercase tracking-wide text-gray-500">
                      {formatKeyLabel(item.key)}
                    </div>
                    <div className="mt-1 break-all text-sm text-gray-200">
                      {renderValue(item.value)}
                    </div>
                  </div>
                ))}
              </div>
            </section>

            <div className="grid gap-5 xl:grid-cols-2">
              <section className="rounded-xl border border-[#30363d] bg-[#0d1117]">
                <div className="flex items-center justify-between border-b border-[#30363d] px-4 py-3">
                  <div className="flex items-center gap-2 text-sm font-medium text-gray-100">
                    <Server className="h-4 w-4 text-cyan-300" />
                    Session Variables
                  </div>
                  <button
                    onClick={() => handleCopySection('session')}
                    className="text-xs text-gray-400 transition-colors hover:text-white"
                  >
                    Copy
                  </button>
                </div>
                <div className="divide-y divide-[#21262d]">
                  {sessionInfo.session_variables.map((item) => (
                    <div key={item.key} className="flex items-start justify-between gap-3 px-4 py-3">
                      <div className="text-sm text-gray-400">{formatKeyLabel(item.key)}</div>
                      <div className="max-w-[60%] break-all text-right text-sm text-gray-200">
                        {renderValue(item.value)}
                      </div>
                    </div>
                  ))}
                </div>
              </section>

              <section className="rounded-xl border border-[#30363d] bg-[#0d1117]">
                <div className="flex items-center justify-between border-b border-[#30363d] px-4 py-3">
                  <div className="flex items-center gap-2 text-sm font-medium text-gray-100">
                    <Shield className="h-4 w-4 text-amber-300" />
                    Server Variables
                  </div>
                  <button
                    onClick={() => handleCopySection('global')}
                    className="text-xs text-gray-400 transition-colors hover:text-white"
                  >
                    Copy
                  </button>
                </div>
                <div className="divide-y divide-[#21262d]">
                  {sessionInfo.global_variables.map((item) => (
                    <div key={item.key} className="flex items-start justify-between gap-3 px-4 py-3">
                      <div className="text-sm text-gray-400">{formatKeyLabel(item.key)}</div>
                      <div className="max-w-[60%] break-all text-right text-sm text-gray-200">
                        {renderValue(item.value)}
                      </div>
                    </div>
                  ))}
                </div>
              </section>
            </div>
          </div>
        )}
      </div>
    </div>
  )
}
