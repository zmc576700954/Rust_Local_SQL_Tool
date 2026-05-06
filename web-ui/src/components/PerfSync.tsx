import { useEffect, useMemo, useRef, useState } from 'react'
import { StepWizard } from './StepWizard'
import type { WizardStep } from './StepWizard'
import { api, JOB_POLL_INTERVAL_MS } from '../api'
import { useToast } from './Toast'
import { parseError } from '../utils'
import type { PerfSyncCheckResult, PerfSyncJobStatus } from '../types'
import { dbLevelDisplayName, dbTypeDisplayName } from '../utils/dbCapabilities'

interface PerfSyncProps {
  onCancel: () => void
}

const defaultTiers = [
  { id: '1m', display_name: '1m (100万行/表)' },
  { id: '10m', display_name: '10m (1000万行/表)' },
  { id: '100m', display_name: '100m (1亿行/表)' },
]

export function PerfSync({ onCancel }: PerfSyncProps) {
  const { toast } = useToast()
  const [dbConnections, setDbConnections] = useState<any[]>([])
  const [sourceDbId, setSourceDbId] = useState('')
  const [targetDbId, setTargetDbId] = useState('')
  const [tier, setTier] = useState('1m')
  const [autoFill, setAutoFill] = useState(false)
  const [resetData, setResetData] = useState(false)
  const [injectDiff, setInjectDiff] = useState(false)
  const [chunkSize, setChunkSize] = useState<number>(1000)
  const [maxRows, setMaxRows] = useState<number>(20000)

  const [jobId, setJobId] = useState<string>('')
  const [job, setJob] = useState<PerfSyncJobStatus | null>(null)
  const [resumeJobId, setResumeJobId] = useState<string>('')
  const [finalPayload, setFinalPayload] = useState<any>(null)
  const [checkResult, setCheckResult] = useState<PerfSyncCheckResult | null>(null)
  const [confirmFill, setConfirmFill] = useState<PerfSyncCheckResult | null>(null)
  const [errorObj, setErrorObj] = useState<{ title: string; message: string } | null>(null)
  const [isLoading, setIsLoading] = useState(false)
  const [pollInterrupted, setPollInterrupted] = useState<{ job_id: string; message: string } | null>(null)
  const pollTimerRef = useRef<number | null>(null)

  useEffect(() => {
    const fetchConfig = async () => {
      try {
        const cfg = await api.getConfig()
        const conns = Array.isArray(cfg?.db_connections) ? cfg.db_connections : []
        setDbConnections(conns)

        const defaultTarget = String(cfg?.active_db_id || '')
        if (defaultTarget) setTargetDbId(defaultTarget)
        if (conns.length > 0) {
          const fallbackSource = conns.find((c: any) => String(c?.id || '') !== defaultTarget) || conns[0]
          if (fallbackSource?.id) setSourceDbId(String(fallbackSource.id))
        }

        setTier('1m')
        const savedJobId = window.localStorage.getItem('perf-sync:lastJobId') || ''
        if (savedJobId) setResumeJobId(savedJobId)
      } catch (e: any) {
        toast('加载配置失败：' + (e?.message || String(e)), 'error')
      }
    }
    fetchConfig()
  }, [toast])

  useEffect(() => {
    return () => {
      if (pollTimerRef.current) {
        window.clearInterval(pollTimerRef.current)
        pollTimerRef.current = null
      }
    }
  }, [])

  const tierOptions = defaultTiers

  const normalizeJobStatus = (raw: any, job_id: string): PerfSyncJobStatus => {
    const rawStatus = String(raw?.status || raw?.state || raw?.job_status || raw?.phase_status || '').toLowerCase()
    const mapStatus = (s: string) => {
      if (['queued', 'pending', 'created', 'new'].includes(s)) return 'queued'
      if (['running', 'in_progress', 'processing', 'working'].includes(s)) return 'running'
      if (['succeeded', 'success', 'done', 'completed', 'complete', 'ok', 'finished'].includes(s)) return 'succeeded'
      if (['failed', 'error', 'errored'].includes(s)) return 'failed'
      if (['canceled', 'cancelled', 'aborted'].includes(s)) return 'canceled'
      return (s as any) || 'running'
    }

    const rawProgress = raw?.progress ?? raw?.progress_pct ?? raw?.percent ?? raw?.percentage
    let progress: number | undefined
    if (typeof rawProgress === 'number') {
      progress = rawProgress
    } else if (rawProgress && typeof rawProgress === 'object') {
      const cur = Number(rawProgress.current)
      const tot = Number(rawProgress.total)
      if (Number.isFinite(cur) && Number.isFinite(tot) && tot > 0) {
        progress = (cur / tot) * 100
      }
    } else {
      const n = Number(rawProgress)
      if (Number.isFinite(n)) progress = n
    }
    if (progress !== undefined && progress <= 1) progress = progress * 100
    if (progress !== undefined) progress = Math.max(0, Math.min(100, progress))

    const message = raw?.progress?.message || raw?.message || raw?.msg || raw?.detail || raw?.stage || raw?.step
    const errorMessage =
      raw?.error?.message ||
      raw?.error_message ||
      raw?.error ||
      raw?.err?.message ||
      raw?.failure_reason
    const result = raw?.result ?? raw?.data ?? raw?.payload ?? raw?.output ?? raw?.report

    return {
      job_id,
      status: mapStatus(rawStatus),
      progress,
      message: message ? String(message) : undefined,
      error: errorMessage,
      result,
    }
  }

  const stopPolling = () => {
    if (pollTimerRef.current) {
      window.clearInterval(pollTimerRef.current)
      pollTimerRef.current = null
    }
  }

  const startPolling = (job_id: string) => {
    const id = String(job_id || '').trim()
    if (!id) return
    stopPolling()
    setPollInterrupted(null)
    setJobId(id)
    setResumeJobId(id)
    window.localStorage.setItem('perf-sync:lastJobId', id)

    const tick = async () => {
      try {
        const raw = await api.perfSyncJobStatus(id)
        const normalized = normalizeJobStatus(raw, id)
        setJob(normalized)
        if (normalized.status === 'succeeded') {
          stopPolling()
          setFinalPayload(raw)
          setIsLoading(false)
          toast('同步压测已完成。', 'success')
          return
        }
        if (normalized.status === 'failed' || normalized.status === 'canceled') {
          stopPolling()
          setIsLoading(false)
          const err = normalized.error ? String(normalized.error) : '任务失败'
          setErrorObj({ title: '同步压测失败', message: err })
          toast('同步压测失败：' + err, 'error')
          return
        }
      } catch (e: any) {
        stopPolling()
        setIsLoading(false)
        if (e?.response?.status === 404) {
          setErrorObj({ title: 'Job 不存在', message: `job_id=${id}` })
          toast('Job 不存在：' + id, 'error')
          return
        }
        const err = parseError(e)
        setPollInterrupted({ job_id: id, message: err.message || String(e) })
      }
    }

    tick()
    pollTimerRef.current = window.setInterval(tick, JOB_POLL_INTERVAL_MS)
  }

  const extractSummaryFields = (obj: any): Array<{ key: string; value: string }> => {
    if (!obj || typeof obj !== 'object') return []
    const entries = Object.entries(obj)
      .filter(([, v]) => ['string', 'number', 'boolean'].includes(typeof v))
      .slice(0, 10)
      .map(([k, v]) => ({ key: k, value: String(v) }))
    return entries
  }

  const reportSummary = useMemo(() => {
    const report = finalPayload?.report || finalPayload?.result || finalPayload?.data || finalPayload
    if (!report) return []
    const summary =
      report?.summary ||
      report?.report_summary ||
      report?.reportSummary ||
      report?.metrics ||
      report?.stats ||
      null
    return extractSummaryFields(summary && typeof summary === 'object' ? summary : report)
  }, [finalPayload])

  const copyJson = async (value: any) => {
    try {
      const text = typeof value === 'string' ? value : JSON.stringify(value, null, 2)
      await navigator.clipboard.writeText(text)
      toast('已复制 JSON 到剪贴板', 'success')
    } catch {
      toast('复制失败：浏览器未授权剪贴板权限', 'error')
    }
  }

  const isValid = Boolean(sourceDbId && targetDbId && chunkSize > 0 && maxRows > 0 && tier.trim())

  const handleCheck = async () => {
    if (!isValid) {
      toast('请先填写完整参数（source/target/tier/chunk_size/max_rows）。', 'info')
      return null
    }
    try {
      const raw = await api.perfSyncCheck({
        source_db_id: sourceDbId,
        target_db_id: targetDbId,
        tier: tier.trim(),
      })
      const normalized: PerfSyncCheckResult = {
        tier: String(raw?.tier || tier.trim()),
        expected_rows: raw?.expected_rows || {},
        baseline_counts: raw?.baseline_counts || {},
        insufficient: Array.isArray(raw?.insufficient) ? raw.insufficient : [],
        fill_plan: Array.isArray(raw?.fill_plan) ? raw.fill_plan : [],
      }
      setCheckResult(normalized)
      return normalized
    } catch (e: any) {
      const err = parseError(e)
      setErrorObj({ title: err.title || '检测失败', message: err.message || String(e) })
      toast('检测失败：' + (err.message || String(e)), 'error')
      return null
    }
  }

  const handleRun = async () => {
    if (!isValid) {
      toast('请先填写完整参数（source/target/tier/chunk_size/max_rows）。', 'info')
      return
    }

    setIsLoading(true)
    setErrorObj(null)
    setFinalPayload(null)
    setConfirmFill(null)
    setJobId('')
    setJob(null)
    setPollInterrupted(null)
    try {
      const checked = await handleCheck()
      if (checked && checked.insufficient.length > 0 && !autoFill) {
        setIsLoading(false)
        setConfirmFill(checked)
        return
      }

      const res = await api.perfSyncStart({
        source_db_id: sourceDbId,
        target_db_id: targetDbId,
        tier: tier.trim(),
        chunk_size: chunkSize,
        max_rows: maxRows,
        loadgen: autoFill
          ? {
              tier: tier.trim(),
              fill: true,
              reset: resetData,
              inject: injectDiff,
              seed: 1,
              batch: 1000,
            }
          : undefined,
      })

      const nextJobId = String(res?.job_id || res?.id || '')
      if (!nextJobId) throw new Error('Missing job_id')
      setJob({ job_id: nextJobId, status: 'queued', progress: 0 })
      startPolling(nextJobId)
    } catch (e: any) {
      const err = parseError(e)
      setErrorObj({ title: err.title || '同步压测失败', message: err.message || String(e) })
      toast('同步压测失败：' + (err.message || String(e)), 'error')
      setIsLoading(false)
    } finally {
      void 0
    }
  }

  const handleConfirmFillAndRun = async () => {
    if (!confirmFill) return
    setIsLoading(true)
    setErrorObj(null)
    setFinalPayload(null)
    setJobId('')
    setJob(null)
    setPollInterrupted(null)
    try {
      setAutoFill(true)
      const res = await api.perfSyncStart({
        source_db_id: sourceDbId,
        target_db_id: targetDbId,
        tier: tier.trim(),
        chunk_size: chunkSize,
        max_rows: maxRows,
        loadgen: {
          tier: tier.trim(),
          fill: true,
          reset: resetData,
          inject: injectDiff,
          seed: 1,
          batch: 1000,
        },
      })

      const nextJobId = String(res?.job_id || res?.id || '')
      if (!nextJobId) throw new Error('Missing job_id')
      setJob({ job_id: nextJobId, status: 'queued', progress: 0 })
      setConfirmFill(null)
      startPolling(nextJobId)
    } catch (e: any) {
      const err = parseError(e)
      setErrorObj({ title: err.title || '同步压测失败', message: err.message || String(e) })
      toast('同步压测失败：' + (err.message || String(e)), 'error')
      setIsLoading(false)
    } finally {
      void 0
    }
  }

  const statusLabel = (status: PerfSyncJobStatus['status']) => {
    if (status === 'queued') return '排队中'
    if (status === 'running') return '执行中'
    if (status === 'succeeded') return '已完成'
    if (status === 'failed') return '失败'
    if (status === 'canceled') return '已取消'
    return status
  }

  const renderJobCard = () => {
    if (!job) return null
    const progress = typeof job.progress === 'number' ? job.progress : undefined
    const isError = job.status === 'failed'
    return (
      <div className={`border rounded-lg p-3 ${isError ? 'border-red-500/30 bg-red-500/10' : 'border-[#30363d] bg-[#0d1117]'}`}>
        <div className="flex items-center justify-between gap-4">
          <div className="text-sm font-medium text-gray-200">Job {job.job_id}</div>
          <div className={`text-xs ${isError ? 'text-red-400' : 'text-gray-400'}`}>{statusLabel(job.status)}</div>
        </div>
        {(job.message || progress !== undefined) && (
          <div className="mt-2 flex flex-col gap-2">
            {typeof progress === 'number' && (
              <div className="w-full h-2 bg-[#21262d] rounded overflow-hidden border border-[#30363d]">
                <div className="h-full bg-blue-500/60" style={{ width: `${progress}%` }} />
              </div>
            )}
            {job.message && <div className="text-xs text-gray-400">{job.message}</div>}
          </div>
        )}
        {job.status === 'failed' && job.error && (
          <div className="mt-2 text-xs text-red-300 whitespace-pre-wrap">{String(job.error)}</div>
        )}
      </div>
    )
  }

  const steps: WizardStep[] = [
    {
      id: 'perf-sync',
      title: '同步压测',
      isValid,
      content: (
        <div className="flex flex-col gap-4 h-full">
          <div className="border border-[#30363d] bg-[#0d1117] rounded-lg p-3">
            <div className="text-sm font-medium text-gray-200">恢复 Job</div>
            <div className="mt-2 flex items-center gap-2">
              <input
                value={resumeJobId}
                onChange={e => setResumeJobId(e.target.value)}
                placeholder="输入 job_id"
                className="flex-1 px-3 py-2 rounded bg-[#0d1117] border border-[#30363d] text-gray-100 text-sm"
              />
              <button
                onClick={() => startPolling(resumeJobId)}
                disabled={!resumeJobId.trim() || isLoading}
                className="px-3 py-2 rounded text-sm font-medium bg-[#21262d] hover:bg-[#30363d] text-gray-100 border border-[#30363d]"
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

          <div className="flex items-center justify-between gap-3">
            <div className="text-xs text-gray-400">建议先检测数据量，避免跑到一半才报错</div>
            <div className="flex items-center gap-2">
              <button
                onClick={handleCheck}
                disabled={!isValid || isLoading}
                className="px-3 py-2 rounded text-sm font-medium bg-[#21262d] hover:bg-[#30363d] text-gray-100 border border-[#30363d]"
              >
                检测数据量
              </button>
              <button
                onClick={handleRun}
                disabled={!isValid || isLoading}
                className={`px-3 py-2 rounded text-sm font-medium ${
                  isLoading ? 'bg-gray-700 text-gray-400 cursor-not-allowed' : 'bg-blue-600 hover:bg-blue-500 text-white'
                }`}
              >
                {isLoading ? '执行中…' : 'Execute（一键运行）'}
              </button>
            </div>
          </div>

          {checkResult && (
            <div className="border border-[#30363d] bg-[#0d1117] rounded-lg p-3">
              <div className="text-sm font-medium text-gray-200">检测结果（tier {checkResult.tier}）</div>
              <div className="mt-2 grid grid-cols-1 gap-2 text-xs">
                {Object.entries(checkResult.expected_rows || {}).map(([k, expected]) => {
                  const c = checkResult.baseline_counts?.[k]
                  const s = c?.source ?? 0
                  const t = c?.target ?? 0
                  const ok = s >= Number(expected) && t >= Number(expected)
                  return (
                    <div key={k} className={`flex items-center justify-between gap-3 ${ok ? 'text-gray-300' : 'text-yellow-300'}`}>
                      <div className="font-mono">{k}</div>
                      <div className="font-mono">expected {Number(expected)} / source {s} / target {t}</div>
                    </div>
                  )
                })}
              </div>
              {checkResult.insufficient.length > 0 && (
                <div className="mt-2 text-xs text-yellow-300">
                  数据量不足：{checkResult.insufficient.map(x => x.table_name).join(', ')}
                </div>
              )}
            </div>
          )}

          {confirmFill && (
            <div className="border border-yellow-500/30 bg-yellow-500/10 rounded-lg p-3">
              <div className="text-sm font-medium text-yellow-200">数据量不足，需要先补齐基准数据</div>
              <div className="text-xs text-yellow-100 mt-1">
                选择“补齐并继续”会在 source/target 上按 tier {confirmFill.tier} 增量填充压测基准表（默认不清空现有数据）。
              </div>

              <div className="mt-2 text-xs text-yellow-100 whitespace-pre-wrap">
                {confirmFill.insufficient
                  .slice(0, 10)
                  .map(x => `${x.table_name}: expected ${x.expected}, source ${x.source}, target ${x.target}`)
                  .join('\n')}
              </div>

              {Array.isArray(confirmFill.fill_plan) && confirmFill.fill_plan.length > 0 && (
                <div className="mt-3 border border-yellow-500/20 bg-black/20 rounded p-2">
                  <div className="text-xs text-yellow-100 font-medium">填充计划预览（将补齐的行数）</div>
                  <div className="mt-1 grid grid-cols-1 gap-1 text-xs">
                    {confirmFill.fill_plan.slice(0, 10).map(x => (
                      <div key={x.table_name} className="flex items-center justify-between gap-3 text-yellow-100">
                        <div className="font-mono">{x.table_name}</div>
                        <div className="font-mono">
                          +source {x.source_fill} / +target {x.target_fill}
                        </div>
                      </div>
                    ))}
                  </div>
                </div>
              )}

              <div className="mt-3 flex flex-col gap-2">
                <label className="flex items-center gap-2 text-sm text-yellow-100 select-none">
                  <input
                    type="checkbox"
                    checked={resetData}
                    onChange={e => setResetData(e.target.checked)}
                    className="accent-blue-500"
                  />
                  重置并重建数据（TRUNCATE）
                </label>
                <label className="flex items-center gap-2 text-sm text-yellow-100 select-none">
                  <input
                    type="checkbox"
                    checked={injectDiff}
                    onChange={e => setInjectDiff(e.target.checked)}
                    className="accent-blue-500"
                  />
                  注入差异（用于验证 mirror / upsert_only 修复）
                </label>
              </div>

              <div className="mt-3 flex items-center gap-2">
                <button
                  onClick={() => setConfirmFill(null)}
                  disabled={isLoading}
                  className="px-3 py-2 rounded text-sm font-medium bg-[#21262d] hover:bg-[#30363d] text-gray-100 border border-[#30363d]"
                >
                  取消
                </button>
                <button
                  onClick={handleConfirmFillAndRun}
                  disabled={isLoading}
                  className={`px-3 py-2 rounded text-sm font-medium ${
                    isLoading ? 'bg-gray-700 text-gray-400 cursor-not-allowed' : 'bg-yellow-600 hover:bg-yellow-500 text-black'
                  }`}
                >
                  {isLoading ? '执行中…' : '补齐并继续'}
                </button>
              </div>
            </div>
          )}

          <div className="grid grid-cols-2 gap-4">
            <div className="flex flex-col gap-1">
              <label className="text-xs text-gray-400">Source 连接</label>
              <select
                value={sourceDbId}
                onChange={e => setSourceDbId(e.target.value)}
                className="bg-[#0d1117] border border-[#30363d] rounded px-3 py-2 text-sm text-white focus:outline-none focus:border-blue-500"
              >
                <option value="" disabled>请选择</option>
                {dbConnections.map(c => (
                  <option key={String(c?.id)} value={String(c?.id)}>
                    {String(c?.name || c?.id)} ({dbTypeDisplayName(c?.db_type)}/{dbLevelDisplayName(c?.capability_level)}){c?.url ? ` (${String(c.url).slice(0, 60)})` : ''}
                  </option>
                ))}
              </select>
            </div>

            <div className="flex flex-col gap-1">
              <label className="text-xs text-gray-400">Target 连接</label>
              <select
                value={targetDbId}
                onChange={e => setTargetDbId(e.target.value)}
                className="bg-[#0d1117] border border-[#30363d] rounded px-3 py-2 text-sm text-white focus:outline-none focus:border-blue-500"
              >
                <option value="" disabled>请选择</option>
                {dbConnections.map(c => (
                  <option key={String(c?.id)} value={String(c?.id)}>
                    {String(c?.name || c?.id)} ({dbTypeDisplayName(c?.db_type)}/{dbLevelDisplayName(c?.capability_level)}){c?.url ? ` (${String(c.url).slice(0, 60)})` : ''}
                  </option>
                ))}
              </select>
            </div>

            <div className="flex flex-col gap-1">
              <label className="text-xs text-gray-400">Tier</label>
              <select
                value={tier}
                onChange={e => setTier(e.target.value)}
                className="bg-[#0d1117] border border-[#30363d] rounded px-3 py-2 text-sm text-white focus:outline-none focus:border-blue-500"
              >
                {tierOptions.map(t => (
                  <option key={String(t?.id)} value={String(t?.id)}>
                    {String(t?.display_name || t?.id)}
                  </option>
                ))}
              </select>
            </div>

            <div className="flex flex-col gap-3 mt-5">
              <label className="flex items-center gap-2 text-sm text-gray-300 select-none">
                <input
                  type="checkbox"
                  checked={autoFill}
                  onChange={e => {
                    const v = e.target.checked
                    setAutoFill(v)
                    if (v) setResetData(true)
                    if (!v) setInjectDiff(false)
                  }}
                  className="accent-blue-500"
                />
                填充标准数据集（压测基准表）
              </label>
              {autoFill && (
                <label className="flex items-center gap-2 text-sm text-gray-300 select-none">
                  <input
                    type="checkbox"
                    checked={resetData}
                    onChange={e => setResetData(e.target.checked)}
                    className="accent-blue-500"
                  />
                  重置并重建数据（TRUNCATE）
                </label>
              )}
              <label className={`flex items-center gap-2 text-sm select-none ${autoFill ? 'text-gray-300' : 'text-gray-600'}`}>
                <input
                  type="checkbox"
                  checked={injectDiff}
                  onChange={e => setInjectDiff(e.target.checked)}
                  className="accent-blue-500"
                  disabled={!autoFill}
                />
                注入差异
              </label>
            </div>

            <div className="flex flex-col gap-1">
              <label className="text-xs text-gray-400">chunk_size</label>
              <input
                type="number"
                value={chunkSize}
                min={1}
                onChange={e => setChunkSize(Number(e.target.value))}
                className="bg-[#0d1117] border border-[#30363d] rounded px-3 py-2 text-sm text-white focus:outline-none focus:border-blue-500"
              />
            </div>

            <div className="flex flex-col gap-1">
              <label className="text-xs text-gray-400">max_rows</label>
              <input
                type="number"
                value={maxRows}
                min={1}
                onChange={e => setMaxRows(Number(e.target.value))}
                className="bg-[#0d1117] border border-[#30363d] rounded px-3 py-2 text-sm text-white focus:outline-none focus:border-blue-500"
              />
            </div>
          </div>

          {errorObj && (
            <div className="border border-red-500/30 bg-red-500/10 rounded-lg p-3">
              <div className="text-sm font-medium text-red-300">{errorObj.title}</div>
              <div className="text-xs text-red-200 mt-1 whitespace-pre-wrap">{errorObj.message}</div>
            </div>
          )}

          {renderJobCard()}

          {finalPayload && (
            <div className="flex flex-col gap-3">
              <div className="flex items-center justify-between">
                <div className="text-sm font-medium text-gray-200">最终报告摘要</div>
                <button
                  onClick={() => copyJson(finalPayload)}
                  className="px-3 py-1.5 rounded bg-[#21262d] border border-[#30363d] hover:bg-[#30363d] text-xs text-gray-200 transition-colors"
                >
                  复制 JSON
                </button>
              </div>

              {reportSummary.length > 0 && (
                <div className="grid grid-cols-2 gap-2 border border-[#30363d] bg-[#0d1117] rounded-lg p-3">
                  {reportSummary.map(item => (
                    <div key={item.key} className="flex items-center justify-between gap-3">
                      <div className="text-xs text-gray-400 truncate">{item.key}</div>
                      <div className="text-xs text-gray-200 font-mono truncate">{item.value}</div>
                    </div>
                  ))}
                </div>
              )}

              <textarea
                readOnly
                value={JSON.stringify(finalPayload, null, 2)}
                className="w-full min-h-[220px] bg-[#0d1117] border border-[#30363d] rounded p-4 font-mono text-xs text-gray-300 outline-none"
              />
            </div>
          )}

          {jobId && !finalPayload && (
            <div className="text-xs text-gray-500">
              正在轮询 /tools/perf-sync/jobs/{encodeURIComponent(jobId)}
            </div>
          )}
        </div>
      ),
    },
  ]

  return (
    <StepWizard
      steps={steps}
      title="同步压测 (Perf Sync)"
      onCancel={onCancel}
      onFinish={handleRun}
      isLoading={isLoading}
      finalWarningMessage="该操作会对 source/target 做同步对比与压测，请确保选择正确的连接。"
    />
  )
}
