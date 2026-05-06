import { useEffect, useMemo, useRef, useState } from 'react'
import { StepWizard } from './StepWizard'
import type { WizardStep } from './StepWizard'
import { api, JOB_POLL_INTERVAL_MS } from '../api'
import { useToast } from './Toast'
import { parseError } from '../utils'

interface GoLiveProps {
  onCancel: () => void
}

type GoLiveStepStatus = 'pass' | 'fail' | 'skip' | 'running' | 'pending' | 'unknown'

function stepStatusNormalize(raw: unknown): GoLiveStepStatus {
  const s = String(raw || '').toLowerCase()
  if (s === 'pass' || s === 'passed' || s === 'ok' || s === 'success' || s === 'succeeded') return 'pass'
  if (s === 'fail' || s === 'failed' || s === 'error') return 'fail'
  if (s === 'skip' || s === 'skipped') return 'skip'
  if (s === 'running' || s === 'in_progress') return 'running'
  if (s === 'pending' || s === 'queued') return 'pending'
  return 'unknown'
}

function stepStatusClass(status: GoLiveStepStatus): string {
  if (status === 'pass') return 'text-green-300'
  if (status === 'fail') return 'text-red-300'
  if (status === 'skip') return 'text-gray-400'
  if (status === 'running') return 'text-blue-300'
  if (status === 'pending') return 'text-gray-300'
  return 'text-gray-400'
}

function jobStatusLabel(status: string): string {
  const s = String(status || '').toLowerCase()
  if (s === 'pending') return '排队中'
  if (s === 'running') return '执行中'
  if (s === 'completed') return '已完成'
  if (s === 'error') return '失败'
  if (s === 'canceled') return '已取消'
  return String(status || '')
}

export function GoLive({ onCancel }: GoLiveProps) {
  const { toast } = useToast()
  const [payloadText, setPayloadText] = useState('{\n  \n}')
  const [showAdvanced, setShowAdvanced] = useState(false)
  const [operator, setOperator] = useState('')
  const [selectedSteps, setSelectedSteps] = useState<string[]>([
    'config',
    'mysql_connect',
    'sql_smoke',
    'export_import_smoke',
    'ai_smoke',
  ])
  const [maxTotalMs, setMaxTotalMs] = useState('')
  const [perStepMaxMs, setPerStepMaxMs] = useState<Record<string, string>>({})
  const [dbConnections, setDbConnections] = useState<any[]>([])
  const [connectionIds, setConnectionIds] = useState<string[]>([])
  const [jobId, setJobId] = useState('')
  const [job, setJob] = useState<any | null>(null)
  const [resumeJobId, setResumeJobId] = useState('')
  const [isLoading, setIsLoading] = useState(false)
  const [pollInterrupted, setPollInterrupted] = useState<{ job_id: string; message: string } | null>(null)
  const pollTimerRef = useRef<number | null>(null)

  useEffect(() => {
    const fetchConfig = async () => {
      try {
        const cfg = await api.getConfig()
        const conns = Array.isArray(cfg?.db_connections) ? cfg.db_connections : []
        const activeId = String(cfg?.active_db_id || '').trim()
        const withActive =
          activeId
            ? [{ id: 'active', name: `Active（${activeId}）` }, ...conns]
            : [{ id: 'active', name: 'Active' }, ...conns]
        setDbConnections(withActive)
      } catch (e: any) {
        toast('加载配置失败：' + (e?.message || String(e)), 'error')
      }
    }
    fetchConfig()
  }, [toast])

  useEffect(() => {
    const saved = window.localStorage.getItem('go-live:lastJobId') || ''
    if (saved) setResumeJobId(saved)
  }, [])

  useEffect(() => {
    return () => {
      if (pollTimerRef.current) {
        window.clearInterval(pollTimerRef.current)
        pollTimerRef.current = null
      }
    }
  }, [])

  const stopPolling = () => {
    if (pollTimerRef.current) {
      window.clearInterval(pollTimerRef.current)
      pollTimerRef.current = null
    }
  }

  const startPolling = (rawJobId: string) => {
    const id = String(rawJobId || '').trim()
    if (!id) return
    stopPolling()
    setPollInterrupted(null)
    setJobId(id)
    setResumeJobId(id)
    window.localStorage.setItem('go-live:lastJobId', id)

    const tick = async () => {
      try {
        const j = await api.toolJobStatus(id)
        setJob(j)
        const status = String(j?.status || '').toLowerCase()
        if (status === 'completed' || status === 'error' || status === 'canceled') {
          stopPolling()
        }
      } catch (e: any) {
        stopPolling()
        const err = parseError(e)
        setPollInterrupted({ job_id: id, message: err.message || String(e) })
      }
    }

    tick()
    pollTimerRef.current = window.setInterval(tick, JOB_POLL_INTERVAL_MS)
  }

  const handleRun = async () => {
    setIsLoading(true)
    try {
      const thresholds: any = {}
      const maxTotal = maxTotalMs.trim()
      if (maxTotal) {
        const v = Number(maxTotal)
        if (!Number.isFinite(v) || v < 0) {
          toast('max_total_ms 需要是非负数字', 'error')
          return
        }
        thresholds.max_total_ms = Math.round(v)
      }

      const perStep: Record<string, number> = {}
      for (const [k, raw] of Object.entries(perStepMaxMs)) {
        const t = String(raw || '').trim()
        if (!t) continue
        const v = Number(t)
        if (!Number.isFinite(v) || v < 0) {
          toast(`per_step_max_ms[${k}] 需要是非负数字`, 'error')
          return
        }
        perStep[k] = Math.round(v)
      }
      if (Object.keys(perStep).length > 0) thresholds.per_step_max_ms = perStep

      const operatorText = operator.trim()
      const basePayload: any = {
        steps: selectedSteps,
        connection_ids: connectionIds,
        operator: operatorText || undefined,
        thresholds: Object.keys(thresholds).length > 0 ? thresholds : undefined,
      }

      let payload = basePayload
      const t = payloadText.trim()
      if (t) {
        const advanced = JSON.parse(t)
        if (advanced && typeof advanced === 'object' && !Array.isArray(advanced)) {
          payload = { ...basePayload, ...advanced }
        } else {
          toast('高级 payload 必须是 JSON 对象', 'error')
          return
        }
      }
      const res = await api.goLiveJobStart(payload)
      const id = String(res?.job_id || '').trim()
      if (!id) {
        toast('启动失败：缺少 job_id', 'error')
        return
      }
      toast('上线门禁任务已启动', 'success')
      startPolling(id)
    } catch (e: any) {
      const err = parseError(e)
      toast('启动失败：' + (err.message || String(e)), 'error')
    } finally {
      setIsLoading(false)
    }
  }

  const handleCancelJob = async () => {
    if (!jobId) return
    setIsLoading(true)
    try {
      await api.toolJobCancel(jobId)
      toast('Job 已取消', 'success')
      startPolling(jobId)
    } catch (e: any) {
      const err = parseError(e)
      toast('取消失败：' + (err.message || String(e)), 'error')
    } finally {
      setIsLoading(false)
    }
  }

  const downloadArtifact = (artifact: string) => {
    const id = String(jobId || '').trim()
    if (!id) return
    const url = `/backend/tools/jobs/${encodeURIComponent(id)}/artifacts/${encodeURIComponent(artifact)}`
    const link = document.createElement('a')
    link.href = url
    document.body.appendChild(link)
    link.click()
    document.body.removeChild(link)
  }

  const normalizedSteps = useMemo(() => {
    const raw =
      (Array.isArray(job?.steps) ? job.steps : null) ||
      (Array.isArray(job?.result?.steps) ? job.result.steps : null) ||
      (Array.isArray(job?.result?.stages) ? job.result.stages : null) ||
      []

    return (raw as any[]).map((s, idx) => {
      const title = String(s?.name || s?.title || s?.id || `step-${idx + 1}`)
      const status = stepStatusNormalize(s?.status)
      const message = s?.message ?? s?.detail ?? s?.error ?? null
      return { title, status, message }
    })
  }, [job])

  const reportJson = useMemo(() => {
    const r = job?.report ?? job?.result ?? job?.result?.report ?? null
    if (r == null) return ''
    try {
      return JSON.stringify(r, null, 2)
    } catch {
      return String(r)
    }
  }, [job])

  const progress = useMemo(() => {
    const p = job?.progress
    const current = typeof p?.current === 'number' ? p.current : null
    const total = typeof p?.total === 'number' ? p.total : null
    const percent = typeof p?.percent === 'number' ? p.percent : null
    if (typeof current === 'number' && typeof total === 'number' && total > 0) {
      const v = Math.max(0, Math.min(100, Math.round((current / total) * 100)))
      return { percent: v, label: `${current} / ${total}` }
    }
    if (typeof percent === 'number') {
      const v = Math.max(0, Math.min(100, Math.round(percent)))
      return { percent: v, label: `${v}%` }
    }
    return null
  }, [job])

  const canRun = !isLoading && String(job?.status || '').toLowerCase() !== 'running'
  const canDownload = String(job?.status || '').toLowerCase() === 'completed'
  const canCancel = String(job?.status || '').toLowerCase() === 'running'

  const stepOptions = useMemo(() => {
    return [
      { id: 'config', label: 'config（配置/环境检查）' },
      { id: 'mysql_connect', label: 'mysql_connect（连接检查）' },
      { id: 'sql_smoke', label: 'sql_smoke（SQL Smoke）' },
      { id: 'export_import_smoke', label: 'export_import_smoke（导入导出 Smoke）' },
      { id: 'ai_smoke', label: 'ai_smoke（AI Smoke）' },
    ]
  }, [])

  const steps: WizardStep[] = [
    {
      id: 'go-live',
      title: '上线门禁',
      isValid: canRun,
      content: (
        <div className="flex flex-col gap-4 h-full">
          <div className="border border-[#30363d] bg-[#0d1117] rounded-lg p-3">
            <div className="text-sm font-medium text-gray-200">Operator（可选）</div>
            <input
              value={operator}
              onChange={e => setOperator(e.target.value)}
              placeholder="填写操作人，如：alice"
              className="mt-2 w-full px-3 py-2 rounded bg-[#0d1117] border border-[#30363d] text-gray-100 text-sm outline-none focus:border-blue-500"
              disabled={isLoading || String(job?.status || '').toLowerCase() === 'running'}
            />
          </div>

          <div className="border border-[#30363d] bg-[#0d1117] rounded-lg p-3">
            <div className="text-sm font-medium text-gray-200">Steps（多选）</div>
            <div className="mt-2 grid grid-cols-1 sm:grid-cols-2 gap-2">
              {stepOptions.map(opt => (
                <label key={opt.id} className="flex items-center gap-2 text-sm text-gray-200 select-none">
                  <input
                    type="checkbox"
                    checked={selectedSteps.includes(opt.id)}
                    onChange={() => {
                      setSelectedSteps(prev => prev.includes(opt.id) ? prev.filter(x => x !== opt.id) : [...prev, opt.id])
                    }}
                    disabled={isLoading || String(job?.status || '').toLowerCase() === 'running'}
                    className="accent-blue-500"
                  />
                  <span className="truncate">{opt.label}</span>
                </label>
              ))}
            </div>
            <div className="mt-2 text-xs text-gray-500">
              不选择任何 step 将按后端默认 steps 执行
            </div>
          </div>

          <div className="border border-[#30363d] bg-[#0d1117] rounded-lg p-3">
            <div className="text-sm font-medium text-gray-200">Thresholds（可选）</div>
            <div className="mt-2 grid grid-cols-1 sm:grid-cols-2 gap-3">
              <div className="flex flex-col gap-1">
                <div className="text-xs text-gray-400">max_total_ms</div>
                <input
                  value={maxTotalMs}
                  onChange={e => setMaxTotalMs(e.target.value)}
                  placeholder="例如：30000"
                  className="px-3 py-2 rounded bg-[#0d1117] border border-[#30363d] text-gray-100 text-sm outline-none focus:border-blue-500"
                  disabled={isLoading || String(job?.status || '').toLowerCase() === 'running'}
                />
              </div>
              <div className="flex flex-col gap-1">
                <div className="text-xs text-gray-400">per_step_max_ms</div>
                <div className="flex flex-col gap-2">
                  {stepOptions.map(opt => (
                    <div key={opt.id} className="flex items-center gap-2">
                      <div className="text-xs text-gray-500 w-36 truncate">{opt.id}</div>
                      <input
                        value={perStepMaxMs[opt.id] || ''}
                        onChange={e => setPerStepMaxMs(prev => ({ ...prev, [opt.id]: e.target.value }))}
                        placeholder="ms"
                        className="flex-1 px-3 py-2 rounded bg-[#0d1117] border border-[#30363d] text-gray-100 text-sm outline-none focus:border-blue-500"
                        disabled={isLoading || String(job?.status || '').toLowerCase() === 'running'}
                      />
                    </div>
                  ))}
                </div>
              </div>
            </div>
          </div>

          <div className="border border-[#30363d] bg-[#0d1117] rounded-lg p-3">
            <div className="text-sm font-medium text-gray-200">Connections（多选）</div>
            <div className="mt-2 flex flex-col gap-2">
              {dbConnections.map((c: any) => {
                const id = String(c?.id || '').trim()
                const name = String(c?.name || c?.id || '').trim()
                if (!id) return null
                return (
                  <label key={id} className="flex items-center gap-2 text-sm text-gray-200 select-none">
                    <input
                      type="checkbox"
                      checked={connectionIds.includes(id)}
                      onChange={() => {
                        setConnectionIds(prev => prev.includes(id) ? prev.filter(x => x !== id) : [...prev, id])
                      }}
                      disabled={isLoading || String(job?.status || '').toLowerCase() === 'running'}
                      className="accent-blue-500"
                    />
                    <span className="truncate">{name} <span className="text-gray-500 text-xs">{id}</span></span>
                  </label>
                )
              })}
            </div>
            <div className="mt-2 text-xs text-gray-500">
              不选择任何连接将默认使用 Active
            </div>
          </div>

          <div className="border border-[#30363d] bg-[#0d1117] rounded-lg p-3">
            <button
              onClick={() => setShowAdvanced(v => !v)}
              className="text-sm font-medium text-blue-400 hover:text-blue-300"
              type="button"
            >
              {showAdvanced ? '隐藏高级选项' : '显示高级选项'}
            </button>
            {showAdvanced && (
              <div className="mt-3">
                <div className="text-sm font-medium text-gray-200">高级 Payload（JSON）</div>
                <textarea
                  value={payloadText}
                  onChange={e => setPayloadText(e.target.value)}
                  className="mt-2 w-full min-h-[110px] bg-[#0d1117] border border-[#30363d] rounded p-3 font-mono text-xs text-gray-200 outline-none focus:border-blue-500"
                  placeholder="{ }"
                  disabled={isLoading || String(job?.status || '').toLowerCase() === 'running'}
                />
              </div>
            )}
            <div className="mt-2 text-xs text-gray-500">
              Run 会调用 /backend/tools/jobs/go-live/start；高级 payload 会覆盖同名字段
            </div>
          </div>

          <div className="border border-[#30363d] bg-[#0d1117] rounded-lg p-3">
            <div className="text-sm font-medium text-gray-200">恢复 Job</div>
            <div className="mt-2 flex items-center gap-2">
              <input
                value={resumeJobId}
                onChange={e => setResumeJobId(e.target.value)}
                placeholder="输入 job_id"
                className="flex-1 px-3 py-2 rounded bg-[#0d1117] border border-[#30363d] text-gray-100 text-sm"
                disabled={isLoading}
              />
              <button
                onClick={() => startPolling(resumeJobId)}
                disabled={!resumeJobId.trim() || isLoading}
                className="px-3 py-2 rounded text-sm font-medium bg-[#21262d] hover:bg-[#30363d] text-gray-100 border border-[#30363d] disabled:opacity-50"
              >
                恢复查看
              </button>
            </div>
          </div>

          {pollInterrupted && (
            <div className="border border-yellow-500/30 bg-yellow-500/10 rounded-lg p-3">
              <div className="text-sm font-medium text-yellow-200">轮询中断</div>
              <div className="mt-1 text-xs text-yellow-100 whitespace-pre-wrap">{pollInterrupted.message}</div>
              <div className="mt-2 flex items-center gap-2">
                <button
                  onClick={() => startPolling(pollInterrupted.job_id)}
                  className="px-3 py-2 rounded text-sm font-medium bg-blue-600 hover:bg-blue-500 text-white"
                >
                  继续轮询
                </button>
                <div className="text-xs text-yellow-100 font-mono">job_id={pollInterrupted.job_id}</div>
              </div>
            </div>
          )}

          {jobId && (
            <div className="border border-[#30363d] bg-[#0d1117] rounded-lg p-3">
              <div className="flex items-center justify-between gap-3">
                <div className="text-sm font-medium text-gray-200">Job</div>
                <div className="text-xs text-gray-400">{jobStatusLabel(job?.status)}</div>
              </div>
              <div className="mt-1 text-xs text-gray-400 font-mono break-all">job_id={jobId}</div>

              {progress && (
                <div className="mt-3">
                  <div className="flex items-center justify-between text-xs text-gray-400">
                    <div>progress</div>
                    <div className="font-mono">{progress.label}</div>
                  </div>
                  <div className="mt-2 h-2 bg-[#161b22] rounded overflow-hidden border border-[#30363d]">
                    <div className="h-full bg-blue-600" style={{ width: `${progress.percent}%` }} />
                  </div>
                </div>
              )}

              {normalizedSteps.length > 0 && (
                <div className="mt-3">
                  <div className="text-xs text-gray-400">steps</div>
                  <div className="mt-2 flex flex-col gap-2">
                    {normalizedSteps.map((s, idx) => (
                      <div key={`${s.title}-${idx}`} className="flex items-start justify-between gap-3">
                        <div className="min-w-0 flex-1">
                          <div className="text-sm text-gray-200 truncate">{s.title}</div>
                          {s.message != null && (
                            <div className="text-xs text-gray-500 whitespace-pre-wrap break-words mt-0.5">
                              {String(s.message)}
                            </div>
                          )}
                        </div>
                        <div className={`text-xs font-mono shrink-0 ${stepStatusClass(s.status)}`}>{s.status}</div>
                      </div>
                    ))}
                  </div>
                </div>
              )}

              <div className="mt-3 flex items-center justify-end gap-2">
                <button
                  onClick={() => downloadArtifact('data')}
                  disabled={!canDownload}
                  className="px-3 py-2 rounded text-sm font-medium bg-[#21262d] hover:bg-[#30363d] text-gray-100 border border-[#30363d] disabled:opacity-50"
                >
                  下载 artifacts/data
                </button>
                <button
                  onClick={handleCancelJob}
                  disabled={!canCancel || isLoading}
                  className="px-3 py-2 rounded text-sm font-medium bg-[#21262d] hover:bg-[#30363d] text-red-300 border border-[#30363d] disabled:opacity-50"
                >
                  Cancel Job
                </button>
              </div>
            </div>
          )}

          {reportJson && (
            <div className="border border-[#30363d] bg-[#0d1117] rounded-lg p-3 flex flex-col">
              <div className="text-sm font-medium text-gray-200">报告（JSON）</div>
              <textarea
                readOnly
                value={reportJson}
                className="mt-2 w-full min-h-[220px] bg-[#0d1117] border border-[#30363d] rounded p-3 font-mono text-xs text-gray-200 outline-none"
              />
            </div>
          )}
        </div>
      ),
    },
  ]

  return (
    <StepWizard
      title="上线门禁"
      steps={steps}
      onFinish={handleRun}
      onCancel={onCancel}
      isLoading={isLoading}
    />
  )
}
