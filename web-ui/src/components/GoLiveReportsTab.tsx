import { useEffect, useMemo, useState } from 'react'
import { api } from '../api'
import { useToast } from './Toast'
import { parseError } from '../utils'

type StepStatus = 'pass' | 'fail' | 'skip' | 'unknown'

function statusNormalize(raw: unknown): StepStatus {
  const s = String(raw || '').toLowerCase()
  if (s === 'pass' || s === 'passed' || s === 'ok' || s === 'success' || s === 'succeeded') return 'pass'
  if (s === 'fail' || s === 'failed' || s === 'error') return 'fail'
  if (s === 'skip' || s === 'skipped') return 'skip'
  return 'unknown'
}

function statusClass(status: StepStatus): string {
  if (status === 'pass') return 'text-green-300'
  if (status === 'fail') return 'text-red-300'
  if (status === 'skip') return 'text-gray-400'
  return 'text-gray-400'
}

function downloadGoLiveReport(jobId: string) {
  const id = String(jobId || '').trim()
  if (!id) return
  const url = `/backend/tools/jobs/${encodeURIComponent(id)}/artifacts/data`
  const link = document.createElement('a')
  link.href = url
  document.body.appendChild(link)
  link.click()
  document.body.removeChild(link)
}

type GoLiveIndexRow = {
  job_id: string
  created_at?: string
  finished_at?: string
  passed?: boolean
  operator?: string | null
  connection_ids?: string[]
}

type GoLiveStepRow = {
  name: string
  connection_id?: string | null
  status: string
  duration_ms?: number
  errors?: string[]
  code?: string | null
  details?: any
}

type GoLiveReport = {
  job_id: string
  operator?: string | null
  connection_ids?: string[]
  requested_steps?: string[]
  thresholds?: any
  created_at?: string
  finished_at?: string
  elapsed_ms?: number
  passed?: boolean
  steps?: GoLiveStepRow[]
}

export function GoLiveReportsTab({ isActive }: { isActive: boolean }) {
  const { toast } = useToast()
  const [loadingList, setLoadingList] = useState(false)
  const [rows, setRows] = useState<GoLiveIndexRow[]>([])
  const [selectedJobId, setSelectedJobId] = useState<string>('')
  const [detail, setDetail] = useState<GoLiveReport | null>(null)
  const [loadingDetail, setLoadingDetail] = useState(false)

  const [compareA, setCompareA] = useState('')
  const [compareB, setCompareB] = useState('')
  const [compareLoading, setCompareLoading] = useState(false)
  const [compareData, setCompareData] = useState<{ a: GoLiveReport; b: GoLiveReport } | null>(null)

  const loadList = async () => {
    setLoadingList(true)
    try {
      const data = await api.goLiveReports(100)
      setRows(Array.isArray(data) ? data : [])
    } catch (e: any) {
      const err = parseError(e)
      toast('加载门禁报告失败：' + (err.message || String(e)), 'error')
    } finally {
      setLoadingList(false)
    }
  }

  const loadDetail = async (jobId: string) => {
    const id = String(jobId || '').trim()
    if (!id) return
    setLoadingDetail(true)
    try {
      const data = await api.toolJobArtifactData(id, 'data')
      if (data && typeof data === 'object' && !Array.isArray(data)) {
        setDetail(data as any)
      } else {
        setDetail(null)
        toast('报告内容不是有效 JSON', 'error')
      }
    } catch (e: any) {
      const err = parseError(e)
      toast('加载报告详情失败：' + (err.message || String(e)), 'error')
      setDetail(null)
    } finally {
      setLoadingDetail(false)
    }
  }

  useEffect(() => {
    if (!isActive) return
    loadList()
  }, [isActive])

  useEffect(() => {
    if (!isActive) return
    if (!selectedJobId) return
    loadDetail(selectedJobId)
  }, [isActive, selectedJobId])

  const detailSteps = useMemo(() => {
    const s = Array.isArray(detail?.steps) ? detail?.steps : []
    return s.map(x => ({
      key: `${String(x?.connection_id || '')}:${String(x?.name || '')}`,
      name: String(x?.name || ''),
      connection_id: x?.connection_id == null ? null : String(x.connection_id),
      status: statusNormalize(x?.status),
      duration_ms: typeof x?.duration_ms === 'number' ? x.duration_ms : Number(x?.duration_ms || 0),
      errors: Array.isArray(x?.errors) ? x.errors.map(String) : [],
      code: x?.code == null ? null : String(x.code),
    }))
  }, [detail])

  const compareRows = useMemo(() => {
    if (!compareData) return []
    const toKey = (x: any) => `${String(x?.connection_id || '')}:${String(x?.name || '')}`
    const aSteps = Array.isArray(compareData.a?.steps) ? compareData.a.steps : []
    const bSteps = Array.isArray(compareData.b?.steps) ? compareData.b.steps : []
    const mapA = new Map<string, any>()
    const mapB = new Map<string, any>()
    for (const s of aSteps) mapA.set(toKey(s), s)
    for (const s of bSteps) mapB.set(toKey(s), s)
    const keys = Array.from(new Set([...mapA.keys(), ...mapB.keys()])).sort()
    return keys.map(k => {
      const a = mapA.get(k)
      const b = mapB.get(k)
      const name = String(a?.name || b?.name || '')
      const conn = a?.connection_id ?? b?.connection_id ?? null
      const aStatus = statusNormalize(a?.status)
      const bStatus = statusNormalize(b?.status)
      const aMs = typeof a?.duration_ms === 'number' ? a.duration_ms : Number(a?.duration_ms || 0)
      const bMs = typeof b?.duration_ms === 'number' ? b.duration_ms : Number(b?.duration_ms || 0)
      return {
        key: k,
        name,
        connection_id: conn == null ? null : String(conn),
        aStatus,
        bStatus,
        aMs,
        bMs,
        delta: bMs - aMs,
      }
    })
  }, [compareData])

  const runCompare = async () => {
    const aId = compareA.trim()
    const bId = compareB.trim()
    if (!aId || !bId) return
    if (aId === bId) {
      toast('请选择两个不同的 job_id', 'error')
      return
    }
    setCompareLoading(true)
    try {
      const [a, b] = await Promise.all([
        api.toolJobArtifactData(aId, 'data'),
        api.toolJobArtifactData(bId, 'data'),
      ])
      if (!a || typeof a !== 'object' || Array.isArray(a) || !b || typeof b !== 'object' || Array.isArray(b)) {
        toast('对比报告内容不是有效 JSON', 'error')
        setCompareData(null)
        return
      }
      setCompareData({ a: a as any, b: b as any })
    } catch (e: any) {
      const err = parseError(e)
      toast('对比加载失败：' + (err.message || String(e)), 'error')
      setCompareData(null)
    } finally {
      setCompareLoading(false)
    }
  }

  const sortedRows = useMemo(() => {
    const list = Array.isArray(rows) ? rows : []
    return [...list].sort((a, b) => String(b?.created_at || '').localeCompare(String(a?.created_at || '')))
  }, [rows])

  return (
    <div className="flex h-full bg-dark-bg">
      <div className="w-[380px] border-r border-dark-border flex flex-col">
        <div className="h-10 border-b border-dark-border flex items-center justify-between px-4 bg-dark-panel shrink-0">
          <div className="text-sm font-medium text-gray-200">门禁报告</div>
          <button
            onClick={loadList}
            disabled={loadingList}
            className="px-2 py-1 rounded text-xs font-medium bg-[#21262d] hover:bg-[#30363d] text-gray-100 border border-[#30363d] disabled:opacity-50"
          >
            刷新
          </button>
        </div>

        <div className="flex-1 overflow-auto">
          {sortedRows.length === 0 && (
            <div className="p-4 text-sm text-gray-500">暂无报告</div>
          )}
          {sortedRows.map((r, idx) => {
            const id = String(r?.job_id || '').trim()
            const passed = !!r?.passed
            const active = id && id === selectedJobId
            return (
              <button
                key={id || `row-${idx}`}
                onClick={() => setSelectedJobId(id)}
                className={`w-full text-left px-4 py-3 border-b border-dark-border hover:bg-[#161b22] transition-colors ${active ? 'bg-[#0a0c10]' : ''}`}
              >
                <div className="flex items-center justify-between gap-3">
                  <div className="text-sm text-gray-200 truncate">{id}</div>
                  <div className={`text-xs font-mono ${passed ? 'text-green-300' : 'text-red-300'}`}>{passed ? 'PASS' : 'FAIL'}</div>
                </div>
                <div className="mt-1 text-xs text-gray-500 truncate">
                  {String(r?.created_at || '')}
                </div>
                <div className="mt-1 text-xs text-gray-500 truncate">
                  operator={String(r?.operator || '') || '-'} · connections={(Array.isArray(r?.connection_ids) ? r.connection_ids.length : 0)}
                </div>
              </button>
            )
          })}
        </div>
      </div>

      <div className="flex-1 flex flex-col overflow-hidden">
        <div className="h-10 border-b border-dark-border flex items-center justify-between px-4 bg-dark-panel shrink-0">
          <div className="text-sm font-medium text-gray-200">详情</div>
          <div className="flex items-center gap-2">
            {selectedJobId && (
              <button
                onClick={() => downloadGoLiveReport(selectedJobId)}
                className="px-2 py-1 rounded text-xs font-medium bg-[#21262d] hover:bg-[#30363d] text-gray-100 border border-[#30363d]"
              >
                下载 artifacts/data
              </button>
            )}
            {selectedJobId && (
              <button
                onClick={() => loadDetail(selectedJobId)}
                disabled={loadingDetail}
                className="px-2 py-1 rounded text-xs font-medium bg-[#21262d] hover:bg-[#30363d] text-gray-100 border border-[#30363d] disabled:opacity-50"
              >
                重新加载
              </button>
            )}
          </div>
        </div>

        <div className="flex-1 overflow-auto p-4 flex flex-col gap-4">
          {!selectedJobId && (
            <div className="text-sm text-gray-500">从左侧选择一条报告查看详情</div>
          )}

          {selectedJobId && (
            <div className="border border-[#30363d] bg-[#0d1117] rounded-lg p-3">
              <div className="flex items-center justify-between gap-3">
                <div className="text-sm font-medium text-gray-200">Summary</div>
                <div className={`text-xs font-mono ${detail?.passed ? 'text-green-300' : 'text-red-300'}`}>
                  {detail?.passed ? 'PASS' : 'FAIL'}
                </div>
              </div>
              <div className="mt-1 text-xs text-gray-400 font-mono break-all">job_id={selectedJobId}</div>
              <div className="mt-2 text-xs text-gray-500">
                created_at={String(detail?.created_at || '') || '-'} · finished_at={String(detail?.finished_at || '') || '-'} · elapsed_ms={String(detail?.elapsed_ms ?? '') || '-'}
              </div>
              <div className="mt-1 text-xs text-gray-500">
                operator={String(detail?.operator || '') || '-'} · connections={(Array.isArray(detail?.connection_ids) ? detail.connection_ids.join(', ') : '-')}
              </div>
            </div>
          )}

          {selectedJobId && (
            <div className="border border-[#30363d] bg-[#0d1117] rounded-lg overflow-hidden">
              <div className="px-3 py-2 border-b border-[#30363d] text-sm font-medium text-gray-200">Steps</div>
              <div className="overflow-auto">
                <table className="w-full text-sm">
                  <thead className="bg-[#161b22] text-gray-400">
                    <tr>
                      <th className="text-left px-3 py-2 font-medium">step</th>
                      <th className="text-left px-3 py-2 font-medium">conn</th>
                      <th className="text-left px-3 py-2 font-medium">status</th>
                      <th className="text-right px-3 py-2 font-medium">duration_ms</th>
                      <th className="text-left px-3 py-2 font-medium">code</th>
                      <th className="text-left px-3 py-2 font-medium">errors</th>
                    </tr>
                  </thead>
                  <tbody>
                    {detailSteps.map(s => (
                      <tr key={s.key} className="border-t border-[#30363d]">
                        <td className="px-3 py-2 text-gray-200 font-mono">{s.name}</td>
                        <td className="px-3 py-2 text-gray-400 font-mono">{s.connection_id || '-'}</td>
                        <td className={`px-3 py-2 font-mono ${statusClass(s.status)}`}>{s.status}</td>
                        <td className="px-3 py-2 text-right text-gray-200 font-mono">{s.duration_ms}</td>
                        <td className="px-3 py-2 text-gray-400 font-mono">{s.code || '-'}</td>
                        <td className="px-3 py-2 text-gray-400">
                          {s.errors.length > 0 ? s.errors.join(' | ') : '-'}
                        </td>
                      </tr>
                    ))}
                    {detailSteps.length === 0 && (
                      <tr>
                        <td className="px-3 py-3 text-gray-500" colSpan={6}>暂无 steps</td>
                      </tr>
                    )}
                  </tbody>
                </table>
              </div>
            </div>
          )}

          <div className="border border-[#30363d] bg-[#0d1117] rounded-lg overflow-hidden">
            <div className="px-3 py-2 border-b border-[#30363d] flex items-center justify-between gap-3">
              <div className="text-sm font-medium text-gray-200">对比两次</div>
              <button
                onClick={runCompare}
                disabled={compareLoading || !compareA.trim() || !compareB.trim()}
                className="px-2 py-1 rounded text-xs font-medium bg-blue-600 hover:bg-blue-500 text-white disabled:opacity-50"
              >
                对比
              </button>
            </div>
            <div className="p-3 flex flex-col gap-3">
              <div className="grid grid-cols-1 md:grid-cols-2 gap-3">
                <div className="flex flex-col gap-1">
                  <div className="text-xs text-gray-400">Report A</div>
                  <select
                    value={compareA}
                    onChange={e => setCompareA(e.target.value)}
                    className="h-9 bg-[#0d1117] border border-[#30363d] rounded px-2 text-sm text-gray-200 outline-none focus:border-blue-500"
                  >
                    <option value="">请选择</option>
                    {sortedRows.map(r => {
                      const id = String(r?.job_id || '').trim()
                      return <option key={id} value={id}>{id}</option>
                    })}
                  </select>
                </div>
                <div className="flex flex-col gap-1">
                  <div className="text-xs text-gray-400">Report B</div>
                  <select
                    value={compareB}
                    onChange={e => setCompareB(e.target.value)}
                    className="h-9 bg-[#0d1117] border border-[#30363d] rounded px-2 text-sm text-gray-200 outline-none focus:border-blue-500"
                  >
                    <option value="">请选择</option>
                    {sortedRows.map(r => {
                      const id = String(r?.job_id || '').trim()
                      return <option key={id} value={id}>{id}</option>
                    })}
                  </select>
                </div>
              </div>

              {compareData && (
                <div className="overflow-auto border border-[#30363d] rounded">
                  <table className="w-full text-sm">
                    <thead className="bg-[#161b22] text-gray-400">
                      <tr>
                        <th className="text-left px-3 py-2 font-medium">step</th>
                        <th className="text-left px-3 py-2 font-medium">conn</th>
                        <th className="text-left px-3 py-2 font-medium">A</th>
                        <th className="text-right px-3 py-2 font-medium">A_ms</th>
                        <th className="text-left px-3 py-2 font-medium">B</th>
                        <th className="text-right px-3 py-2 font-medium">B_ms</th>
                        <th className="text-right px-3 py-2 font-medium">Δ_ms</th>
                      </tr>
                    </thead>
                    <tbody>
                      {compareRows.map(r => (
                        <tr key={r.key} className="border-t border-[#30363d]">
                          <td className="px-3 py-2 text-gray-200 font-mono">{r.name}</td>
                          <td className="px-3 py-2 text-gray-400 font-mono">{r.connection_id || '-'}</td>
                          <td className={`px-3 py-2 font-mono ${statusClass(r.aStatus)}`}>{r.aStatus}</td>
                          <td className="px-3 py-2 text-right text-gray-200 font-mono">{r.aMs}</td>
                          <td className={`px-3 py-2 font-mono ${statusClass(r.bStatus)}`}>{r.bStatus}</td>
                          <td className="px-3 py-2 text-right text-gray-200 font-mono">{r.bMs}</td>
                          <td className={`px-3 py-2 text-right font-mono ${r.delta > 0 ? 'text-red-300' : r.delta < 0 ? 'text-green-300' : 'text-gray-300'}`}>{r.delta}</td>
                        </tr>
                      ))}
                      {compareRows.length === 0 && (
                        <tr>
                          <td className="px-3 py-3 text-gray-500" colSpan={7}>暂无对比数据</td>
                        </tr>
                      )}
                    </tbody>
                  </table>
                </div>
              )}
            </div>
          </div>
        </div>
      </div>
    </div>
  )
}
