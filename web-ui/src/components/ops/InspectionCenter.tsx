import { AlertTriangle, CheckCircle2, Clock, Loader2, Play, RefreshCw, Send, ShieldAlert } from 'lucide-react'
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

interface ChannelDelivery {
  channel: string
  status: 'pending' | 'retrying' | 'success' | 'failed' | 'skipped'
  attempts: number
  last_error: string | null
  last_attempt_at: number | null
  delivered_at: number | null
  attempt_times: number[]
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
  deliveries: ChannelDelivery[]
}

interface NotificationEmailSettings {
  enabled: boolean
  host: string
  port: number
  security: 'plain' | 'start_tls' | 'ssl'
  username: string
  from: string
  to: string[]
}

interface NotificationWebhookSettings {
  enabled: boolean
  header_names: string[]
}

interface NotificationSettings {
  email: NotificationEmailSettings
  webhook: NotificationWebhookSettings
  max_retries: number
  backoff_seconds: number
  max_per_window: number
  window_seconds: number
}

interface NotificationConfig {
  settings: NotificationSettings
  secrets: {
    smtp_password_set: boolean
    webhook_url_set: boolean
    webhook_url_masked: string
    webhook_headers: string[]
  }
  channels: string[]
  scope_note: string
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
  scope_note?: string
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

const CHANNEL_LABEL: Record<string, string> = {
  in_app: '站内',
  email: '邮件',
  webhook: 'Webhook',
}

const DELIVERY_LABEL: Record<string, string> = {
  pending: '待发送',
  retrying: '重试中',
  success: '成功',
  failed: '失败',
  skipped: '已跳过',
}

function deliveryClass(status: string): string {
  if (status === 'success') return 'border-emerald-500/40 text-emerald-300'
  if (status === 'failed') return 'border-red-500/40 text-red-300'
  if (status === 'retrying') return 'border-amber-500/40 text-amber-300'
  if (status === 'skipped') return 'border-amber-500/30 text-amber-200'
  return 'border-dark-border text-gray-400'
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

  const [notificationConfig, setNotificationConfig] = useState<NotificationConfig | null>(null)
  const [emailEnabled, setEmailEnabled] = useState(false)
  const [emailHost, setEmailHost] = useState('')
  const [emailPort, setEmailPort] = useState(587)
  const [emailSecurity, setEmailSecurity] = useState<'plain' | 'start_tls' | 'ssl'>('start_tls')
  const [emailUsername, setEmailUsername] = useState('')
  const [emailFrom, setEmailFrom] = useState('')
  const [emailTo, setEmailTo] = useState('')
  const [smtpPassword, setSmtpPassword] = useState('')
  const [webhookEnabled, setWebhookEnabled] = useState(false)
  const [webhookUrl, setWebhookUrl] = useState('')
  const [webhookHeadersText, setWebhookHeadersText] = useState('')
  const [maxRetries, setMaxRetries] = useState(2)
  const [backoffSeconds, setBackoffSeconds] = useState(5)
  const [maxPerWindow, setMaxPerWindow] = useState(20)
  const [windowSeconds, setWindowSeconds] = useState(3600)
  const [lastRunScopeNote, setLastRunScopeNote] = useState('')

  const applyNotificationConfig = useCallback((value: NotificationConfig) => {
    setNotificationConfig(value)
    setEmailEnabled(value.settings.email.enabled)
    setEmailHost(value.settings.email.host)
    setEmailPort(value.settings.email.port)
    setEmailSecurity(value.settings.email.security)
    setEmailUsername(value.settings.email.username)
    setEmailFrom(value.settings.email.from)
    setEmailTo((value.settings.email.to || []).join(', '))
    setWebhookEnabled(value.settings.webhook.enabled)
    setMaxRetries(value.settings.max_retries)
    setBackoffSeconds(value.settings.backoff_seconds)
    setMaxPerWindow(value.settings.max_per_window)
    setWindowSeconds(value.settings.window_seconds)
    setSmtpPassword('')
    setWebhookUrl('')
    setWebhookHeadersText('')
  }, [])

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
      const [metaResp, scheduleResp, notificationResp] = await Promise.all([
        api.opsInspectionPresets(),
        api.opsInspectionGetSchedule(),
        api.opsInspectionGetNotifications(),
      ])
      setPresets(metaResp.presets || [])
      setChecks(metaResp.checks || [])
      setScopeNote(metaResp.scope || '')
      applySchedule(scheduleResp.schedule)
      setTarget(scheduleResp.target || null)
      applyNotificationConfig(notificationResp as NotificationConfig)
    } catch (err) {
      setError(describeError(err))
    }
  }, [applySchedule, applyNotificationConfig])

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

  const handleSaveNotifications = async () => {
    setBusy('notifications')
    setError('')
    setNotice('')
    try {
      const recipients = emailTo
        .split(',')
        .map((value) => value.trim())
        .filter(Boolean)
      let headerNames = notificationConfig?.settings.webhook.header_names || []
      let headerValues: Record<string, string> | undefined
      if (webhookHeadersText.trim()) {
        headerValues = {}
        for (const line of webhookHeadersText.split('\n')) {
          const separator = line.indexOf(':')
          if (separator <= 0) continue
          const name = line.slice(0, separator).trim()
          const value = line.slice(separator + 1).trim()
          if (name && value) headerValues[name] = value
        }
        headerNames = Object.keys(headerValues)
      }
      const payload: Parameters<typeof api.opsInspectionSaveNotifications>[0] = {
        email: {
          enabled: emailEnabled,
          host: emailHost.trim(),
          port: Number(emailPort) || 587,
          security: emailSecurity,
          username: emailUsername.trim(),
          from: emailFrom.trim(),
          to: recipients,
        },
        webhook: {
          enabled: webhookEnabled,
          header_names: headerNames,
        },
        max_retries: Number(maxRetries),
        backoff_seconds: Number(backoffSeconds),
        max_per_window: Number(maxPerWindow),
        window_seconds: Number(windowSeconds),
      }
      if (smtpPassword) payload.smtp_password = smtpPassword
      if (webhookUrl.trim()) payload.webhook_url = webhookUrl.trim()
      if (headerValues) payload.webhook_headers = headerValues
      const resp = await api.opsInspectionSaveNotifications(payload)
      applyNotificationConfig(resp as NotificationConfig)
      setNotice(
        tr(
          '通知配置已保存（密码 / URL 仅写入本地 0600 凭据文件）',
          'Notification settings saved (secrets stored in a 0600 local file)'
        )
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
      setLastRunScopeNote(result.scope_note || '')
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
              '巡检调度器与通知发送都在本应用进程内：关闭应用后巡检与通知都会停止，桌面通知也仅在应用运行时有效，不会有人在后台持续监控。',
              'Both the scheduler and notifications run inside this app process. Closing the app stops inspections and notifications; desktop notifications only work while the app is running.'
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
          <h3 className="text-sm font-semibold text-white">{tr('通知渠道', 'Notification channels')}</h3>
          <span className="text-xs text-gray-500">
            {tr('已启用', 'enabled')}: {(notificationConfig?.channels || ['in_app']).map((id) => CHANNEL_LABEL[id] || id).join(' / ')}
          </span>
        </div>

        <div className="grid gap-4 lg:grid-cols-2">
          <div className="space-y-3">
            <label className="flex items-center gap-2 text-sm text-gray-300">
              <input type="checkbox" checked={emailEnabled} onChange={(event) => setEmailEnabled(event.target.checked)} />
              {tr('邮件通知（SMTP）', 'Email (SMTP)')}
            </label>
            <div className="grid grid-cols-2 gap-2">
              <input
                value={emailHost}
                onChange={(event) => setEmailHost(event.target.value)}
                placeholder={tr('SMTP 主机', 'SMTP host')}
                className="rounded border border-dark-border bg-dark-bg px-2 py-1 text-xs text-gray-200"
              />
              <input
                type="number"
                value={emailPort}
                onChange={(event) => setEmailPort(Number(event.target.value) || 0)}
                placeholder={tr('端口', 'Port')}
                className="rounded border border-dark-border bg-dark-bg px-2 py-1 text-xs text-gray-200"
              />
              <select
                value={emailSecurity}
                onChange={(event) => setEmailSecurity(event.target.value as 'plain' | 'start_tls' | 'ssl')}
                className="rounded border border-dark-border bg-dark-bg px-2 py-1 text-xs text-gray-200"
              >
                <option value="start_tls">STARTTLS</option>
                <option value="ssl">SSL/TLS</option>
                <option value="plain">{tr('明文（仅内网调试）', 'Plain (debug only)')}</option>
              </select>
              <input
                value={emailUsername}
                onChange={(event) => setEmailUsername(event.target.value)}
                placeholder={tr('用户名', 'Username')}
                className="rounded border border-dark-border bg-dark-bg px-2 py-1 text-xs text-gray-200"
              />
              <input
                type="password"
                value={smtpPassword}
                onChange={(event) => setSmtpPassword(event.target.value)}
                placeholder={
                  notificationConfig?.secrets.smtp_password_set
                    ? tr('密码已保存（留空不修改）', 'Password saved (blank keeps it)')
                    : tr('SMTP 密码', 'SMTP password')
                }
                className="rounded border border-dark-border bg-dark-bg px-2 py-1 text-xs text-gray-200"
              />
              <input
                value={emailFrom}
                onChange={(event) => setEmailFrom(event.target.value)}
                placeholder={tr('发件人', 'From')}
                className="rounded border border-dark-border bg-dark-bg px-2 py-1 text-xs text-gray-200"
              />
              <input
                value={emailTo}
                onChange={(event) => setEmailTo(event.target.value)}
                placeholder={tr('收件人，逗号分隔', 'Recipients, comma separated')}
                className="col-span-2 rounded border border-dark-border bg-dark-bg px-2 py-1 text-xs text-gray-200"
              />
            </div>

            <label className="flex items-center gap-2 text-sm text-gray-300">
              <input
                type="checkbox"
                checked={webhookEnabled}
                onChange={(event) => setWebhookEnabled(event.target.checked)}
              />
              {tr('Webhook（POST JSON）', 'Webhook (POST JSON)')}
            </label>
            <input
              type="password"
              value={webhookUrl}
              onChange={(event) => setWebhookUrl(event.target.value)}
              placeholder={
                notificationConfig?.secrets.webhook_url_set
                  ? tr(`URL 已保存：${notificationConfig.secrets.webhook_url_masked}（留空不修改）`, `URL saved: ${notificationConfig.secrets.webhook_url_masked} (blank keeps it)`)
                  : tr('https://...（含 token 的完整 URL）', 'https://... (full URL incl. token)')
              }
              className="w-full rounded border border-dark-border bg-dark-bg px-2 py-1 text-xs text-gray-200"
            />
            <textarea
              value={webhookHeadersText}
              onChange={(event) => setWebhookHeadersText(event.target.value)}
              rows={2}
              placeholder={tr(
                '自定义头，每行 Name: value（值仅写入本地 0600 文件）',
                'Custom headers, one Name: value per line (values stored in a 0600 local file)'
              )}
              className="w-full rounded border border-dark-border bg-dark-bg px-2 py-1 font-mono text-[11px] text-gray-200"
            />
            {notificationConfig?.secrets.webhook_headers.length ? (
              <div className="text-[11px] text-gray-500">
                {tr('已保存的头', 'Saved headers')}: {notificationConfig.secrets.webhook_headers.join(', ')}
              </div>
            ) : null}
          </div>

          <div className="space-y-2 text-xs text-gray-400">
            <div className="grid grid-cols-2 gap-2">
              <label className="space-y-1">
                <span>{tr('最大重试次数', 'Max retries')}</span>
                <input
                  type="number"
                  min={0}
                  max={10}
                  value={maxRetries}
                  onChange={(event) => setMaxRetries(Number(event.target.value) || 0)}
                  className="w-full rounded border border-dark-border bg-dark-bg px-2 py-1 text-gray-200"
                />
              </label>
              <label className="space-y-1">
                <span>{tr('退避间隔（秒）', 'Backoff seconds')}</span>
                <input
                  type="number"
                  min={0}
                  max={600}
                  value={backoffSeconds}
                  onChange={(event) => setBackoffSeconds(Number(event.target.value) || 0)}
                  className="w-full rounded border border-dark-border bg-dark-bg px-2 py-1 text-gray-200"
                />
              </label>
              <label className="space-y-1">
                <span>{tr('每窗口发送上限', 'Max sends per window')}</span>
                <input
                  type="number"
                  min={1}
                  value={maxPerWindow}
                  onChange={(event) => setMaxPerWindow(Number(event.target.value) || 1)}
                  className="w-full rounded border border-dark-border bg-dark-bg px-2 py-1 text-gray-200"
                />
              </label>
              <label className="space-y-1">
                <span>{tr('窗口长度（秒）', 'Window seconds')}</span>
                <input
                  type="number"
                  min={1}
                  value={windowSeconds}
                  onChange={(event) => setWindowSeconds(Number(event.target.value) || 1)}
                  className="w-full rounded border border-dark-border bg-dark-bg px-2 py-1 text-gray-200"
                />
              </label>
            </div>
            <p>
              {tr(
                '4xx（凭据 / 地址错误）不重试；5xx 与连接失败按“退避间隔 × 第几次尝试”退避重试。SMTP 相反：5xx 不重试，4xx 退避重试。',
                'HTTP 4xx (bad credentials/address) is not retried; 5xx and connection failures back off. SMTP is inverted: 5xx is permanent, 4xx retries.'
              )}
            </p>
            <p>
              {tr(
                '同一条 finding（check_id + evidence_key）未恢复期间只推送一次，严重度升级时再推；整体受上面的时间窗上限约束。',
                'An unresolved finding (check_id + evidence_key) is pushed once; a severity escalation pushes again. Global rate limit applies.'
              )}
            </p>
            <p className="text-amber-300">{notificationConfig?.scope_note || scopeNote}</p>
            <button
              onClick={() => void handleSaveNotifications()}
              disabled={busy !== null}
              className="flex items-center gap-1.5 rounded border border-blue-500/40 bg-blue-500/10 px-3 py-1.5 text-xs text-blue-300 hover:bg-blue-500/20 disabled:opacity-50"
            >
              {busy === 'notifications' ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <Send className="h-3.5 w-3.5" />}
              {tr('保存通知配置', 'Save notification settings')}
            </button>
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
            {lastRunScopeNote ? (
              <div className="rounded border border-amber-500/30 bg-amber-500/5 px-2 py-1 text-[11px] text-amber-300">
                {lastRunScopeNote}
              </div>
            ) : null}
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
                {alert.deliveries?.length ? (
                  <div className="mt-1.5 flex flex-wrap gap-1.5">
                    {alert.deliveries.map((delivery) => (
                      <span
                        key={delivery.channel}
                        title={delivery.last_error || undefined}
                        className={`rounded border px-1.5 py-0.5 text-[10px] ${deliveryClass(delivery.status)}`}
                      >
                        {CHANNEL_LABEL[delivery.channel] || delivery.channel}:{' '}
                        {DELIVERY_LABEL[delivery.status] || delivery.status}
                        {delivery.attempts > 1 ? ` ×${delivery.attempts}` : ''}
                        {delivery.last_error ? tr('（有错误）', ' (error)') : ''}
                      </span>
                    ))}
                  </div>
                ) : null}
                {alert.deliveries?.some((delivery) => delivery.last_error) ? (
                  <div className="mt-1 break-all text-[10px] text-red-300">
                    {alert.deliveries
                      .filter((delivery) => delivery.last_error)
                      .map((delivery) => `${CHANNEL_LABEL[delivery.channel] || delivery.channel}: ${delivery.last_error}`)
                      .join(' · ')}
                  </div>
                ) : null}
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
