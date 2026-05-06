import { useEffect, useMemo, useState } from 'react'
import { api } from '../api'
import { useToast } from './Toast'
import { parseError } from '../utils'

type AuditRow = {
  ts?: number
  action?: string
  job_id?: string
  operator?: string | null
  passed?: boolean
  elapsed_ms?: number
  [key: string]: any
}

function tsLabel(ts?: number) {
  if (!ts) return ''
  try {
    return new Date(ts * 1000).toISOString()
  } catch {
    return String(ts)
  }
}

export function GoLiveAuditTab({ isActive }: { isActive: boolean }) {
  const { toast } = useToast()
  const [loading, setLoading] = useState(false)
  const [rows, setRows] = useState<AuditRow[]>([])
  const [selected, setSelected] = useState<AuditRow | null>(null)

  const load = async () => {
    setLoading(true)
    try {
      const data = await api.goLiveAudit(200)
      setRows(Array.isArray(data) ? data : [])
    } catch (e: any) {
      const err = parseError(e)
      toast('加载门禁审计失败：' + (err.message || String(e)), 'error')
    } finally {
      setLoading(false)
    }
  }

  useEffect(() => {
    if (!isActive) return
    load()
  }, [isActive])

  const sorted = useMemo(() => {
    const list = Array.isArray(rows) ? rows : []
    return [...list].sort((a, b) => Number(b?.ts || 0) - Number(a?.ts || 0))
  }, [rows])

  const selectedJson = useMemo(() => {
    if (!selected) return ''
    try {
      return JSON.stringify(selected, null, 2)
    } catch {
      return String(selected)
    }
  }, [selected])

  return (
    <div className="flex h-full bg-dark-bg">
      <div className="w-[420px] border-r border-dark-border flex flex-col">
        <div className="h-10 border-b border-dark-border flex items-center justify-between px-4 bg-dark-panel shrink-0">
          <div className="text-sm font-medium text-gray-200">门禁审计</div>
          <button
            onClick={load}
            disabled={loading}
            className="px-2 py-1 rounded text-xs font-medium bg-[#21262d] hover:bg-[#30363d] text-gray-100 border border-[#30363d] disabled:opacity-50"
          >
            刷新
          </button>
        </div>

        <div className="flex-1 overflow-auto">
          {sorted.length === 0 && (
            <div className="p-4 text-sm text-gray-500">暂无审计记录</div>
          )}
          {sorted.map((r, idx) => {
            const key = `${String(r?.ts || '')}:${String(r?.job_id || '')}:${idx}`
            const active = selected === r
            const passed = typeof r?.passed === 'boolean' ? r.passed : null
            return (
              <button
                key={key}
                onClick={() => setSelected(r)}
                className={`w-full text-left px-4 py-3 border-b border-dark-border hover:bg-[#161b22] transition-colors ${active ? 'bg-[#0a0c10]' : ''}`}
              >
                <div className="flex items-center justify-between gap-3">
                  <div className="text-sm text-gray-200 truncate">{String(r?.action || '-') || '-'}</div>
                  {passed != null && (
                    <div className={`text-xs font-mono ${passed ? 'text-green-300' : 'text-red-300'}`}>{passed ? 'PASS' : 'FAIL'}</div>
                  )}
                </div>
                <div className="mt-1 text-xs text-gray-500 truncate">
                  {tsLabel(r?.ts)} · job_id={String(r?.job_id || '') || '-'}
                </div>
                <div className="mt-1 text-xs text-gray-500 truncate">
                  operator={String(r?.operator || '') || '-'} · elapsed_ms={String(r?.elapsed_ms ?? '') || '-'}
                </div>
              </button>
            )
          })}
        </div>
      </div>

      <div className="flex-1 flex flex-col overflow-hidden">
        <div className="h-10 border-b border-dark-border flex items-center px-4 bg-dark-panel shrink-0">
          <div className="text-sm font-medium text-gray-200">详情</div>
        </div>
        <div className="flex-1 overflow-auto p-4">
          {!selected && (
            <div className="text-sm text-gray-500">从左侧选择一条审计记录查看详情</div>
          )}
          {selected && (
            <textarea
              readOnly
              value={selectedJson}
              className="w-full h-full min-h-[400px] bg-[#0d1117] border border-[#30363d] rounded p-3 font-mono text-xs text-gray-200 outline-none"
            />
          )}
        </div>
      </div>
    </div>
  )
}

