import { AlertTriangle, CheckCircle2, Clock, Loader2, Play, RefreshCw, ShieldAlert } from 'lucide-react'
import { useCallback, useEffect, useState } from 'react'
import { api } from '../../api'
import { tr } from '../../i18n'

interface InspectionCenterProps {
  dbId?: string
  dbType?: string
}

interface CheckDescriptor {
  id: string
  title: string
  rule: string
}

interface InspectionPreset {
  id: string
  title: string
  description: string
  check_ids: string[]
}

interface InspectionSchedule {
  enabled: boolean
  mode: 'interval' | 'cron'
  interval_minutes: number
  cron_expr: string
  preset_id: string
  check_ids: string[]
  db_id: string
  db_name: string
  updated_at: number
  last_run_at: number | null
  next_run_at: number | null
  last_error: string | null
}

interface EvidenceItem {
  key: string
  severity: 'info' | 'warn' | 'critical'
  message: string
  fields: Record<string, unknown>
}

interface CheckResult {
  check_id: string
  title: string
  status: 'ok' | 'warning' | 'critical' | 'skipped'
  rule: string
  summary: string
  evidence: EvidenceItem[]
  checked_objects: number
  duration_ms: number
  skipped_reason: string | null
}

interface InspectionSnapshot {
  snapshot_id: string
  db_id: string
  db_name: string
  db_fingerprint: string
  started_at: number
  finished_at: number
  trigger: string
  status: 'ok' | 'warning' | 'critical'
  check_ids: string[]
  checks: CheckResult[]
}

interface DiffEvidence {
  check_id: string
  evidence_key: string
  severity: 'info' | 'warn' | 'critical'
  message: string
  fields: Record<string, unknown>
}

interface ChangedEvidence {
  check_id: string
  evidence_key: string
  before: DiffEvidence
  after: DiffEvidence
}

interface SnapshotDiff {
  previous_snapshot_id: string | null
  current_snapshot_id: string
  added: DiffEvidence[]
  resolved: DiffEvidence[]
  changed: ChangedEvidence[]
  check_status_changes: Array<{ check_id: string; before: string; after: string }>
}

interface AlertRecord {
  alert_id: string
  check_id: string
  evidence_key: string
  severity: 'info' | 'warn' | 'critical'
  title: string
  message: string
  first_seen_at: number
  last_seen_at: number
  occurrences: number
  state: 'active' | 'resolved'
  acknowledged: boolean
  resolved_at: number | null
  snapshot_id: string
}

interface SnapshotSummary {
  snapshot_id: string
  db_name: string
  started_at: number
  finished_at: number
  trigger: string
  status: 'ok' | 'warning' | 'critical'
  evidence_count: number
  critical_count: number
  warning_count: number
  info_count: number
  skipped_count: number
}

interface LatestResponse {
  target: Record<string, unknown> | null
  snapshot: InspectionSnapshot | null
  previous_snapshot_id: string | null
  diff: SnapshotDiff | null
  alerts: AlertRecord[]
  unread_count: number
}

interface RunResult {
  snapshot: InspectionSnapshot
  diff: SnapshotDiff
  alerts: AlertRecord[]
}

const CHECK_STATUS_LABEL: Record<string, string> = {
  ok: '正常',
  warning: '警告',
  critical: '严重',
  skipped: '已跳过',
}

const SEVERITY_LABEL: Record<string, string> = {
  info: '提示',
  warn: '警告',
  critical: '严重',
}

function checkStatusClass(status: string): string {
  if (status === 'critical') return 'text-red-400'
  if (status === 'warning') return 'text-amber-400'
  if (status === 'skipped') return 'text-gray-400'
  return 'text-emerald-400'
}

function severityClass(severity: string): string {
  if (severity === 'critical') return 'text-red-400'
  if (severity === 'warn') return 'text-amber-400'
  return 'text-gray-400'
}

function formatTime(ts?: number | null): string {
  if (!ts) return tr('未运行', 'not run')
  return new Date(ts * 1000).toLocaleString()
}

function describeError(err: unknown): string {
  const value = err as { response?: { data?: { error?: string; message?: string } }; message?: string }
  return value?.response?.data?.error || value?.response?.data?.message || value?.message || String(err)
}

function describeFields(fields: Record<string, unknown> | null | undefined): string {
  if (!fields || Object.keys(fields).length === 0) return ''
  return Object.entries(fields)
    .map(([key, value]) => `${key}=${typeof value === 'string' ? value : JSON.stringify(value)}`)
    .join(' · ')
}

export function InspectionCenter({ dbId }: InspectionCenterProps) {
  const [presets, setPresets] = useState<InspectionPreset[]>([])
  const [checks, setChecks] = useState<CheckDescriptor[]>([])
  const [scopeNote, setScopeNote] = useState('')
  const [schedule, setSchedule] = useState<InspectionSchedule | null>(null)
  const [target, setTarget] = useState<Record<string, unknown> | null>(null)
  const [latest, setLatest] = useState<LatestResponse | null>(null)
  const [history, setHistory] = useState<SnapshotSummary[]>([])
  const [busy, setBusy] = useState<string | null>(null)
  const [error, setError] = useState('')
  const [notice, setNotice] = useState('')

  const [enabled, setEnabled] = useState(false)
  const [mode, setMode] = useState<'interval' | 'cron'>('interval')
  const [intervalMinutes, setIntervalMinutes] = useState(1440)
  const [cronExpr, setCronExpr] = useState('0 3 * * *')
  const [presetId, setPresetId] = useState('quick_health')

  const applySchedule = useCallback((value: InspectionSchedule) => {
    setSchedule(value)
    setEnabled(value.enabled)
    setMode(value.mode)
    setIntervalMinutes(value.interval_minutes)
    setCronExpr(value.cron_expr)
    setPresetId(value.preset_id)
  }, [])

  const loadMeta = useCallback(async () => {
    try {
      const [metaResp, scheduleResp] = await Promise.all([
        api.opsInspectionPresets(),
        api.opsInspectionGetSchedule(),
      ])
      setPresets(metaResp.presets || [])
      setChecks(metaResp.checks || [])
      setScopeNote(metaResp.scope || '')
      applySchedule(scheduleResp.schedule)
      setTarget(scheduleResp.target || null)
    } catch (err) {
      setError(describeError(err))
    }
  }, [applySchedule])

  const loadResults = useCallback(async () => {
    try {
      const [latestResp, historyResp] = await Promise.all([
        api.opsInspectionLatest(dbId ? { db_id: dbId } : undefined),
        api.opsInspectionHistory({ db_id: dbId, limit: 10 }),
      ])
      setLatest(latestResp)
      setHistory(historyResp.snapshots || [])
    } catch (err) {
      // 数据库暂时不可连接时，计划仍可查看；这里只提示结果读取失败。
      setLatest(null)
      setError(describeError(err))
    }
  }, [dbId])

  useEffect(() => {
    void loadMeta()
  }, [loadMeta])

  useEffect(() => {
    void loadResults()
  }, [loadResults])

  const selectedPreset = presets.find((preset) => preset.id === presetId)

  const handleSave = async () => {
    setBusy('save')
    setError('')
    setNotice('')
    try {
      const resp = await api.opsInspectionSaveSchedule({
        enabled,
        mode,
        interval_minutes: intervalMinutes,
        cron_expr: cronExpr,
        preset_id: presetId,
        db_id: dbId,
      })
      applySchedule(resp.schedule)
      setTarget(resp.target || null)
      setNotice(
        resp.schedule.enabled
          ? tr('巡检计划已保存并启用', 'Inspection schedule saved and enabled')
          : tr('巡检计划已保存（当前停用）', 'Inspection schedule saved (disabled)')
      )
    } catch (err) {
      setError(describeError(err))
    } finally {
      setBusy(null)
    }
  }

  const handleRun = async () => {
    setBusy('run')
    setError('')
    setNotice('')
    try {
      const result: RunResult = await api.opsInspectionRun({
        db_id: dbId,
        preset_id: presetId,
        check_ids: schedule?.preset_id === presetId ? schedule?.check_ids : undefined,
      })
      setLatest({
        target,
        snapshot: result.snapshot,
        previous_snapshot_id: result.diff.previous_snapshot_id,
        diff: result.diff,
        alerts: result.alerts,
        unread_count: result.alerts.filter((alert) => alert.state === 'active' && !alert.acknowledged).length,
      })
      setNotice(
        tr(
          `巡检完成：${result.snapshot.status}，证据 ${result.snapshot.checks.reduce((sum, item) => sum + item.evidence.length, 0)} 条`,
          `Inspection finished: ${result.snapshot.status}`
        )
      )
      void loadResults()
    } catch (err) {
      setError(describeError(err))
    } finally {
      setBusy(null)
    }
  }

  const handleAck = async (alertId: string) => {
    setBusy(`ack:${alertId}`)
    setError('')
    try {
      await api.opsInspectionAckAlert(alertId)
      void loadResults()
    } catch (err) {
      setError(describeError(err))
    } finally {
      setBusy(null)
    }
  }

  const snapshot = latest?.snapshot || null
  const diff = latest?.diff || null
  const alerts = latest?.alerts || []

  return (
    <div className="flex flex-col gap-4">
      <div className="rounded-lg border border-dark-border bg-dark-panel p-4">
        <div className="flex flex-wrap items-start justify-between gap-3">
          <div>
            <h2 className="text-lg font-semibold text-white">
              {tr('运维中心 · 定期巡检', 'Ops Center · Periodic Inspection')}
            </h2>
            <p className="mt-1 max-w-3xl text-xs text-gray-400">
              {tr(
                '首期仅支持 MySQL/MariaDB；全部检查均为只读查询，不经过允许 apply 的执行通道。',
                'MySQL/MariaDB only. Every check is a read-only query and never uses the apply channel.'
              )}
            </p>
          </div>
          <div className="flex flex-wrap items-center gap-2 text-xs">
            <span className="rounded border border-dark-border px-2 py-1 text-gray-300">
              {tr('目标', 'Target')}: {String(target?.display_name || schedule?.db_name || dbId || tr('跟随当前连接', 'active connection'))}
            </span>
            {target?.db_fingerprint ? (
              <span className="rounded border border-dark-border px-2 py-1 font-mono text-[11px] text-gray-500">
                {String(target.db_fingerprint).slice(0, 12)}
              </span>
            ) : null}
          </div>
        </div>
        <div className="mt-3 flex items-start gap-2 rounded border border-amber-500/40 bg-amber-500/10 p-2.5 text-xs text-amber-300">
          <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0" />
          <span>
            {tr(
              '调度器运行在应用进程内：只有应用保持运行才会按计划巡检；关闭应用后巡检停止，不会在后台继续运行。',
              'The scheduler runs inside this app process. Closing the app stops inspections; nothing keeps running in the background.'
            )}
          </span>
        </div>
      </div>

      {(error || notice) && (
        <div
          className={`rounded-lg border p-3 text-sm ${
            error
              ? 'border-red-500/40 bg-red-500/10 text-red-300'
              : 'border-emerald-500/40 bg-emerald-500/10 text-emerald-300'
          }`}
        >
          {error || notice}
        </div>
      )}

      <div className="rounded-lg border border-dark-border bg-dark-panel p-4">
        <div className="mb-3 flex flex-wrap items-center justify-between gap-2">
          <h3 className="text-sm font-semibold text-white">{tr('场景与计划', 'Scenario & schedule')}</h3>
          <span className="text-xs text-gray-500">
            {tr('上次执行', 'Last run')}: {formatTime(schedule?.last_run_at)} · {tr('下次', 'Next')}:{' '}
            {schedule?.enabled ? formatTime(schedule?.next_run_at) : tr('已停用', 'disabled')}
          </span>
        </div>

        <div className="grid gap-3 lg:grid-cols-2">
          <div className="space-y-3">
            <div className="flex items-center gap-3 text-sm">
              <label className="flex items-center gap-2">
                <input
                  type="checkbox"
                  checked={enabled}
                  onChange={(event) => setEnabled(event.target.checked)}
                />
                <span className="text-gray-300">{tr('启用定期巡检', 'Enable schedule')}</span>
              </label>
            </div>

            <div className="space-y-2">
              <div className="text-xs font-medium text-gray-400">{tr('触发方式', 'Trigger')}</div>
              <div className="flex flex-wrap items-center gap-3 text-sm text-gray-300">
                <label className="flex items-center gap-2">
                  <input
                    type="radio"
                    checked={mode === 'interval'}
                    onChange={() => setMode('interval')}
                  />
                  {tr('固定间隔', 'Interval')}
                </label>
                <label className="flex items-center gap-2">
                  <input type="radio" checked={mode === 'cron'} onChange={() => setMode('cron')} />
                  {tr('Cron 表达式', 'Cron expression')}
                </label>
              </div>
              {mode === 'interval' ? (
                <div className="flex items-center gap-2 text-xs text-gray-400">
                  {tr('每', 'Every')}
                  <input
                    type="number"
                    min={1}
                    max={10080}
                    value={intervalMinutes}
                    onChange={(event) => setIntervalMinutes(Number(event.target.value) || 1)}
                    className="w-24 rounded border border-dark-border bg-dark-bg px-2 py-1 text-gray-200"
                  />
                  {tr('分钟', 'minutes')}
                </div>
              ) : (
                <div className="space-y-1 text-xs text-gray-400">
                  <input
                    value={cronExpr}
                    onChange={(event) => setCronExpr(event.target.value)}
                    className="w-full rounded border border-dark-border bg-dark-bg px-2 py-1 font-mono text-gray-200"
                    placeholder="0 3 * * *"
                  />
                  <div>
                    {tr(
                      '5 字段：分 时 日 月 周；支持 *、n、a-b、*/n、逗号列表。按本机时区计算。',
                      '5 fields: minute hour day month weekday. Supports *, n, a-b, */n and comma lists.'
                    )}
                  </div>
                </div>
              )}
            </div>

            <div className="space-y-1">
              <label className="text-xs font-medium text-gray-400">{tr('预设场景', 'Preset')}</label>
              <select
                value={presetId}
                onChange={(event) => setPresetId(event.target.value)}
                className="w-full rounded border border-dark-border bg-dark-bg px-2 py-1 text-sm text-gray-200"
              >
                {presets.map((preset) => (
                  <option key={preset.id} value={preset.id}>
                    {preset.title}
                  </option>
                ))}
              </select>
              {selectedPreset && (
                <div className="text-xs text-gray-500">
                  {selectedPreset.description} · {selectedPreset.check_ids.length} 项检查
                </div>
              )}
            </div>

            <div className="flex flex-wrap gap-2">
              <button
                onClick={() => void handleSave()}
                disabled={busy !== null}
                className="flex items-center gap-1.5 rounded border border-blue-500/40 bg-blue-500/10 px-3 py-1.5 text-xs text-blue-300 hover:bg-blue-500/20 disabled:opacity-50"
              >
                {busy === 'save' ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <Clock className="h-3.5 w-3.5" />}
                {tr('保存计划', 'Save schedule')}
              </button>
              <button
                onClick={() => void handleRun()}
                disabled={busy !== null}
                className="flex items-center gap-1.5 rounded border border-emerald-500/40 bg-emerald-500/10 px-3 py-1.5 text-xs text-emerald-300 hover:bg-emerald-500/20 disabled:opacity-50"
              >
                {busy === 'run' ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <Play className="h-3.5 w-3.5" />}
                {tr('立即巡检', 'Run now')}
              </button>
              <button
                onClick={() => {
                  void loadMeta()
                  void loadResults()
                }}
                disabled={busy !== null}
                className="flex items-center gap-1.5 rounded border border-dark-border px-3 py-1.5 text-xs text-gray-300 hover:bg-dark-bg disabled:opacity-50"
              >
                <RefreshCw className="h-3.5 w-3.5" />
                {tr('刷新', 'Refresh')}
              </button>
            </div>
            {schedule?.last_error && (
              <div className="rounded border border-red-500/40 bg-red-500/10 p-2 text-xs text-red-300">
                {tr('上次错误', 'Last error')}: {schedule.last_error}
              </div>
            )}
          </div>

          <div className="space-y-2">
            <div className="text-xs font-medium text-gray-400">{tr('检查项与判定规则', 'Checks and rules')}</div>
            <div className="max-h-72 space-y-1.5 overflow-y-auto">
              {checks.map((check) => (
                <div key={check.id} className="rounded border border-dark-border bg-dark-bg p-2 text-xs">
                  <div className="flex items-center justify-between gap-2">
                    <span className="font-medium text-gray-200">{check.title}</span>
                    {schedule?.check_ids.includes(check.id) && (
                      <span className="text-[10px] text-emerald-400">{tr('本次启用', 'enabled')}</span>
                    )}
                  </div>
                  <div className="mt-1 text-gray-500">{check.rule}</div>
                </div>
              ))}
            </div>
            {scopeNote && <div className="text-[11px] text-gray-500">{scopeNote}</div>}
          </div>
        </div>
      </div>

      <div className="rounded-lg border border-dark-border bg-dark-panel p-4">
        <div className="mb-3 flex flex-wrap items-center justify-between gap-2">
          <h3 className="text-sm font-semibold text-white">{tr('最近一次结果', 'Latest result')}</h3>
          {snapshot && (
            <span className="text-xs text-gray-500">
              {formatTime(snapshot.finished_at)} · {snapshot.trigger} · {snapshot.snapshot_id.slice(0, 8)}
            </span>
          )}
        </div>
        {!snapshot ? (
          <div className="text-xs text-gray-500">
            {tr('尚无巡检快照。点击“立即巡检”生成第一份快照。', 'No snapshot yet. Run an inspection to create one.')}
          </div>
        ) : (
          <div className="space-y-3">
            <div className="flex flex-wrap gap-2 text-xs">
              <span className={`rounded border px-2 py-1 ${checkStatusClass(snapshot.status)}`}>
                {CHECK_STATUS_LABEL[snapshot.status] || snapshot.status}
              </span>
              {(() => {
                const evidence = snapshot.checks.flatMap((check) => check.evidence)
                const critical = evidence.filter((item) => item.severity === 'critical').length
                const warn = evidence.filter((item) => item.severity === 'warn').length
                const skipped = snapshot.checks.filter((check) => check.status === 'skipped').length
                return (
                  <>
                    <span className="rounded border border-red-500/40 px-2 py-1 text-red-300">
                      {tr('严重', 'critical')}: {critical}
                    </span>
                    <span className="rounded border border-amber-500/40 px-2 py-1 text-amber-300">
                      {tr('警告', 'warning')}: {warn}
                    </span>
                    <span className="rounded border border-dark-border px-2 py-1 text-gray-400">
                      {tr('跳过的检查', 'skipped checks')}: {skipped}
                    </span>
                  </>
                )
              })()}
            </div>
            <div className="space-y-2">
              {snapshot.checks.map((check) => (
                <div key={check.check_id} className="rounded border border-dark-border bg-dark-bg p-3 text-xs">
                  <div className="flex flex-wrap items-center justify-between gap-2">
                    <div className="flex items-center gap-2">
                      <span className={`font-medium ${checkStatusClass(check.status)}`}>
                        {CHECK_STATUS_LABEL[check.status] || check.status}
                      </span>
                      <span className="text-gray-200">{check.title}</span>
                    </div>
                    <span className="text-gray-500">
                      {tr('检查对象', 'objects')}: {check.checked_objects} · {check.duration_ms}ms
                    </span>
                  </div>
                  <div className="mt-1 text-gray-500">{check.rule}</div>
                  <div className="mt-1 text-gray-400">{check.summary}</div>
                  {check.skipped_reason && (
                    <div className="mt-1 text-amber-300">
                      {tr('跳过原因', 'Skipped')}: {check.skipped_reason}
                    </div>
                  )}
                  {check.evidence.length > 0 && (
                    <div className="mt-2 space-y-1">
                      {check.evidence.slice(0, 12).map((item) => (
                        <div key={`${check.check_id}:${item.key}`} className="rounded bg-dark-panel px-2 py-1">
                          <span className={`mr-2 ${severityClass(item.severity)}`}>
                            {SEVERITY_LABEL[item.severity] || item.severity}
                          </span>
                          <span className="text-gray-300">{item.message}</span>
                          {describeFields(item.fields) && (
                            <div className="mt-0.5 break-all font-mono text-[10px] text-gray-500">
                              {describeFields(item.fields)}
                            </div>
                          )}
                        </div>
                      ))}
                      {check.evidence.length > 12 && (
                        <div className="text-gray-500">
                          {tr(`还有 ${check.evidence.length - 12} 条证据`, `${check.evidence.length - 12} more evidence items`)}
                        </div>
                      )}
                    </div>
                  )}
                </div>
              ))}
            </div>
          </div>
        )}
      </div>

      <div className="rounded-lg border border-dark-border bg-dark-panel p-4">
        <div className="mb-3 flex flex-wrap items-center justify-between gap-2">
          <h3 className="text-sm font-semibold text-white">{tr('与上次对比', 'Diff vs previous')}</h3>
          {diff?.previous_snapshot_id && (
            <span className="text-xs text-gray-500">
              {tr('对比快照', 'against')}: {diff.previous_snapshot_id.slice(0, 8)}
            </span>
          )}
        </div>
        {!diff || diff.previous_snapshot_id === null ? (
          <div className="text-xs text-gray-500">
            {tr('这是第一份快照，暂无可对比的基线。', 'This is the first snapshot; no baseline to compare.')}
          </div>
        ) : diff.added.length + diff.resolved.length + diff.changed.length === 0 && diff.check_status_changes.length === 0 ? (
          <div className="flex items-center gap-2 text-xs text-emerald-400">
            <CheckCircle2 className="h-4 w-4" />
            {tr('与上次一致，无新增、消失或变化项。', 'No added, resolved or changed items since last run.')}
          </div>
        ) : (
          <div className="grid gap-3 md:grid-cols-3">
            <div>
              <div className="mb-1 text-xs font-medium text-red-300">
                {tr('新增', 'Added')} ({diff.added.length})
              </div>
              <div className="space-y-1">
                {diff.added.map((item) => (
                  <div key={`added:${item.check_id}:${item.evidence_key}`} className="rounded border border-red-500/30 bg-dark-bg p-2 text-[11px] text-gray-300">
                    <span className={severityClass(item.severity)}>{SEVERITY_LABEL[item.severity]}</span> {item.message}
                  </div>
                ))}
              </div>
            </div>
            <div>
              <div className="mb-1 text-xs font-medium text-emerald-300">
                {tr('已消失', 'Resolved')} ({diff.resolved.length})
              </div>
              <div className="space-y-1">
                {diff.resolved.map((item) => (
                  <div key={`resolved:${item.check_id}:${item.evidence_key}`} className="rounded border border-emerald-500/30 bg-dark-bg p-2 text-[11px] text-gray-300">
                    {item.message}
                  </div>
                ))}
              </div>
            </div>
            <div>
              <div className="mb-1 text-xs font-medium text-amber-300">
                {tr('变化', 'Changed')} ({diff.changed.length})
              </div>
              <div className="space-y-1">
                {diff.changed.map((item) => (
                  <div key={`changed:${item.check_id}:${item.evidence_key}`} className="rounded border border-amber-500/30 bg-dark-bg p-2 text-[11px] text-gray-300">
                    <div>{item.evidence_key}</div>
                    <div className="text-gray-500">
                      {item.before.message} → {item.after.message}
                    </div>
                  </div>
                ))}
              </div>
            </div>
          </div>
        )}
      </div>

      <div className="rounded-lg border border-dark-border bg-dark-panel p-4">
        <div className="mb-3 flex flex-wrap items-center justify-between gap-2">
          <h3 className="text-sm font-semibold text-white">{tr('告警历史', 'Alert history')}</h3>
          <span className="text-xs text-gray-500">
            {tr('未读', 'unread')}: {latest?.unread_count ?? 0}
          </span>
        </div>
        {alerts.length === 0 ? (
          <div className="text-xs text-gray-500">{tr('暂无告警记录。', 'No alerts yet.')}</div>
        ) : (
          <div className="max-h-80 space-y-1.5 overflow-y-auto">
            {alerts.map((alert) => (
              <div
                key={alert.alert_id}
                className={`rounded border p-2.5 text-xs ${
                  alert.state === 'active'
                    ? 'border-amber-500/40 bg-amber-500/5'
                    : 'border-dark-border bg-dark-bg'
                }`}
              >
                <div className="flex flex-wrap items-center justify-between gap-2">
                  <div className="flex items-center gap-2">
                    <ShieldAlert className={`h-3.5 w-3.5 ${severityClass(alert.severity)}`} />
                    <span className={severityClass(alert.severity)}>
                      {SEVERITY_LABEL[alert.severity] || alert.severity}
                    </span>
                    <span className="text-gray-200">{alert.title}</span>
                    <span className="text-gray-500">
                      {alert.state === 'active' ? tr('活跃', 'active') : tr('已恢复', 'resolved')}
                    </span>
                    {alert.acknowledged && <span className="text-gray-500">{tr('已确认', 'acked')}</span>}
                  </div>
                  {alert.state === 'active' && !alert.acknowledged && (
                    <button
                      onClick={() => void handleAck(alert.alert_id)}
                      disabled={busy !== null}
                      className="rounded border border-dark-border px-2 py-0.5 text-[11px] text-gray-300 hover:bg-dark-panel disabled:opacity-50"
                    >
                      {busy === `ack:${alert.alert_id}` ? tr('处理中', 'working') : tr('确认', 'Acknowledge')}
                    </button>
                  )}
                </div>
                <div className="mt-1 text-gray-400">{alert.message}</div>
                <div className="mt-1 text-[10px] text-gray-500">
                  {tr('首次', 'first')}: {formatTime(alert.first_seen_at)} · {tr('最近', 'last')}:{' '}
                  {formatTime(alert.last_seen_at)} · {tr('次数', 'count')}: {alert.occurrences}
                </div>
              </div>
            ))}
          </div>
        )}
      </div>

      <div className="rounded-lg border border-dark-border bg-dark-panel p-4">
        <h3 className="mb-2 text-sm font-semibold text-white">{tr('快照历史', 'Snapshot history')}</h3>
        {history.length === 0 ? (
          <div className="text-xs text-gray-500">{tr('暂无快照历史。', 'No snapshot history.')}</div>
        ) : (
          <div className="max-h-56 space-y-1 overflow-y-auto text-[11px]">
            {history.map((item) => (
              <div key={item.snapshot_id} className="flex flex-wrap items-center gap-2 rounded bg-dark-bg px-2 py-1">
                <span className={checkStatusClass(item.status)}>{CHECK_STATUS_LABEL[item.status] || item.status}</span>
                <span className="font-mono text-gray-500">{item.snapshot_id.slice(0, 8)}</span>
                <span className="text-gray-400">{formatTime(item.finished_at)}</span>
                <span className="text-gray-500">
                  {tr('证据', 'evidence')}: {item.evidence_count} · {tr('严重', 'critical')}: {item.critical_count} ·
                  {' '}{tr('警告', 'warning')}: {item.warning_count} · {tr('跳过', 'skipped')}: {item.skipped_count}
                </span>
                <span className="text-gray-600">{item.trigger}</span>
              </div>
            ))}
          </div>
        )}
      </div>

      <div className="rounded-lg border border-dark-border bg-dark-panel p-4 text-xs text-gray-500">
        <div className="flex items-center gap-2 text-gray-300">
          <ShieldAlert className="h-4 w-4 text-emerald-400" />
          {tr(
            '只读边界：巡检只执行 SELECT（information_schema / performance_schema / mysql.user），不写库、不调用 apply 执行通道。权限不足的检查会标记为“已跳过”并给出原因。',
            'Read-only: inspections only run SELECTs and never use the apply channel. Checks lacking permissions are marked skipped with a reason.'
          )}
        </div>
      </div>
    </div>
  )
}
