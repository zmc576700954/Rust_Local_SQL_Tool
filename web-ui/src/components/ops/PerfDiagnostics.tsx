import { useCallback, useEffect, useState } from 'react'
import {
  AlertTriangle,
  CheckCircle2,
  Loader2,
  Play,
  RotateCcw,
  Search,
  ShieldCheck,
  Sparkles,
} from 'lucide-react'
import { api } from '../../api'
import { tr } from '../../i18n'

interface PerfWindowInfo {
  window_start?: string
  window_hours?: number
  source: string
  cumulative: boolean
  note: string
}

interface RowEstimateEvidence {
  value: number
  source: string
  trusted: boolean
  reason: string
}

interface DigestInfo {
  digest: string
  digest_text: string
  count_star: number
  total_latency_ms: number
  avg_latency_ms: number
  rows_examined_sum: number
  rows_examined_avg: number
  rows_sent_avg: number
  rows_affected_avg: number
  created_tmp_tables: number
  created_tmp_disk_tables: number
  sort_rows: number
  first_seen?: string
  last_seen?: string
}

interface ExplainSample {
  label: string
  sql: string
  access_type?: string
  key?: string
  possible_keys?: string
  rows_examined?: number
  filtered?: number
  extra?: string
  table_name?: string
  estimated_cost?: number
  actual_time_ms?: number
}

interface PerfTableEvidence {
  table: string
  stats: {
    engine?: string
    table_rows?: number
    data_length?: number
    index_length?: number
    update_time?: string
  }
  freshness: {
    update_time?: string
    innodb_last_update?: string
    stale_days?: number
    status: string
    notes: string[]
  }
  snapshot: {
    columns: Array<{ name: string; column_type: string }>
    indexes: Array<{ name: string; unique: boolean; columns: Array<{ name: string; seq: number }> }>
  }
}

interface ImplicitCastFinding {
  table: string
  column: string
  column_type: string
  operator: string
  literal_kind: string
  literal: string
  index_risk: boolean
  detail: string
}

interface PerfEvidence {
  target_sql: string
  sql_source: string
  digest?: DigestInfo
  window?: PerfWindowInfo
  row_estimate?: RowEstimateEvidence
  explain: ExplainSample[]
  tables: PerfTableEvidence[]
  estimated_scan_rows?: number
  implicit_casts: ImplicitCastFinding[]
  sort_or_temp: string[]
  actual_latency_ms?: number
  explain_analyze_raw?: string
  notes: string[]
  degraded_reasons: string[]
}

interface EvidencePack {
  db_name: string
  table: string
  snapshot: PerfTableEvidence['snapshot']
  stats: PerfTableEvidence['stats']
  explain: ExplainSample[]
  notes: string[]
  perf?: PerfEvidence
}

interface SlowQueryCandidate {
  digest: string
  digest_text: string
  schema_name?: string
  sample_sql?: string
  count_star: number
  total_latency_ms: number
  avg_latency_ms: number
  rows_examined_sum: number
  rows_examined_avg: number
  rows_sent_avg: number
  created_tmp_tables: number
  created_tmp_disk_tables: number
  sort_rows: number
  scan_flag?: boolean
  first_seen?: string
  last_seen?: string
  summary: string
}

interface SlowQueryScan {
  available: boolean
  degraded_reasons: string[]
  sort: string
  window?: PerfWindowInfo
  candidates: SlowQueryCandidate[]
  notes: string[]
}

interface ActionSpec {
  type: string
  table?: string
  name?: string
  columns?: string[]
  unique?: boolean
  using?: string
  expected_definition?: string
}

interface Finding {
  id: string
  title: string
  detail: string
  severity: 'info' | 'warn' | 'critical'
}

interface RiskFinding extends Finding {
  blocking: boolean
}

interface ValidationCheck {
  id: string
  title: string
  detail: string
  status: 'pass' | 'warn' | 'fail'
}

interface RewriteSuggestion {
  id: string
  title: string
  suggestion: string
  rationale: string
  expected_gain: string
  evidence_refs: string[]
}

interface QueryComparison {
  label: string
  before_access?: string
  after_access?: string
  before_key?: string
  after_key?: string
  before_rows?: number
  after_rows?: number
  before_cost?: number
  after_cost?: number
  before_actual_ms?: number
  after_actual_ms?: number
  improved: boolean
  verdict: string
}

interface VerificationReport {
  improved_queries: number
  total_queries: number
  summary: string
  comparisons: QueryComparison[]
  notes: string[]
}

interface ExecutionReport {
  dry_run: boolean
  error?: string
  rollback_performed: boolean
  compiled_sql: string[]
  actions: Array<{ action_id: string; sql: string; state: string; error?: string; duration_ms: number }>
  impact?: {
    estimated_rows?: number
    estimated_index_bytes?: number
    metadata_lock: string
    write_amplification: string
    notes: string[]
  }
  verification?: VerificationReport
}

interface OpsPlan {
  plan_id: string
  scenario_id: string
  db_name: string
  diagnosis: string
  findings: Finding[]
  actions: ActionSpec[]
  risks: RiskFinding[]
  validation_checks: ValidationCheck[]
  rollback: { summary: string; actions: ActionSpec[]; automatic: boolean }
  code_advice: string[]
  rewrite_suggestions: RewriteSuggestion[]
  verification_explain_analyze: boolean
  confidence: number
  plan_hash: string
  status: string
  evidence_summary?: {
    table: string
    column_count: number
    index_count: number
    row_estimate?: number
    explain_findings?: string[]
  }
  execution?: ExecutionReport
}

interface AnalyzeResponse {
  plan: OpsPlan
  evidence: EvidencePack
}

interface PerfDiagnosticsProps {
  dbId?: string
}

function describeError(error: unknown): string {
  if (!error) return ''
  if (typeof error === 'string') return error
  const anyError = error as {
    message?: string
    response?: { data?: { message?: string; error?: string } }
  }
  return (
    anyError.response?.data?.message ||
    anyError.response?.data?.error ||
    anyError.message ||
    String(error)
  )
}

function actionId(action: ActionSpec): string {
  const prefix = action.type
  const suffix = action.type === 'analyze_table' ? action.table : action.name
  return `${prefix}:${suffix || ''}`
}

function severityClass(severity: string): string {
  if (severity === 'critical') return 'text-red-400'
  if (severity === 'warn') return 'text-amber-400'
  return 'text-gray-400'
}

function statusClass(status: string): string {
  if (status === 'pass') return 'text-emerald-400'
  if (status === 'fail') return 'text-red-400'
  return 'text-amber-400'
}

function WindowBanner({ window }: { window?: PerfWindowInfo }) {
  if (!window) {
    return (
      <div className="rounded border border-amber-500/40 bg-amber-500/10 p-2 text-xs text-amber-300">
        {tr(
          '统计窗口不可知：延迟与扫描数为 performance_schema 累计值。',
          'Statistics window unknown: latency and scan counts are cumulative.'
        )}
      </div>
    )
  }
  return (
    <div className="rounded border border-blue-500/40 bg-blue-500/10 p-2 text-xs text-blue-200">
      <div className="font-medium">{window.note}</div>
      <div className="mt-1 text-blue-300/80">
        {tr('窗口起点', 'Window start')}: {window.window_start || '-'} ·{' '}
        {tr('累计口径', 'Cumulative')}: {window.cumulative ? 'true' : 'false'} ·{' '}
        {tr('来源', 'Source')}: {window.source}
      </div>
    </div>
  )
}

function ExplainRow({ sample }: { sample: ExplainSample }) {
  return (
    <li className="rounded border border-dark-border bg-dark-bg p-2 font-mono text-[11px] text-gray-300">
      <span className="text-gray-400">{sample.label}</span> {sample.table_name ? `[${sample.table_name}]` : ''}{' '}
      type={sample.access_type || '-'} key={sample.key || '-'} rows={sample.rows_examined ?? '-'}
      {sample.estimated_cost != null ? ` cost=${sample.estimated_cost.toFixed(2)}` : ''}
      {sample.actual_time_ms != null ? ` actual=${sample.actual_time_ms.toFixed(3)}ms` : ''}
      {sample.extra ? ` ${sample.extra}` : ''}
    </li>
  )
}

export function PerfDiagnostics({ dbId }: PerfDiagnosticsProps) {
  const [sql, setSql] = useState('')
  const [digest, setDigest] = useState<string | undefined>()
  const [sampleSql, setSampleSql] = useState<string | undefined>()
  const [explainAnalyze, setExplainAnalyze] = useState(false)
  const [timeoutMs, setTimeoutMs] = useState(5000)
  const [sort, setSort] = useState('total_latency')
  const [scan, setScan] = useState<SlowQueryScan | null>(null)
  const [analysis, setAnalysis] = useState<AnalyzeResponse | null>(null)
  const [selectedActions, setSelectedActions] = useState<string[]>([])
  const [approval, setApproval] = useState<{ token: string; expiresAt: number } | null>(null)
  const [dryRunReport, setDryRunReport] = useState<ExecutionReport | null>(null)
  const [execution, setExecution] = useState<ExecutionReport | null>(null)
  const [rollbackReason, setRollbackReason] = useState('')
  const [policyAllowsApply, setPolicyAllowsApply] = useState(false)
  const [busy, setBusy] = useState<string | null>(null)
  const [error, setError] = useState('')
  const [notice, setNotice] = useState('')

  const plan = analysis?.plan || null
  const evidence = analysis?.evidence || null
  const perf = evidence?.perf

  useEffect(() => {
    let cancelled = false
    void api
      .opsGetPolicy()
      .then((resp) => {
        if (!cancelled) setPolicyAllowsApply(Boolean(resp?.policy?.allow_apply))
      })
      .catch(() => undefined)
    return () => {
      cancelled = true
    }
  }, [])

  const resetResults = () => {
    setSelectedActions([])
    setApproval(null)
    setDryRunReport(null)
    setExecution(null)
    setRollbackReason('')
    setError('')
    setNotice('')
  }

  const handleListSlowQueries = useCallback(async () => {
    setBusy('list')
    setError('')
    setNotice('')
    try {
      const resp: SlowQueryScan = await api.opsSlowQueries({ db_id: dbId, sort, limit: 20 })
      setScan(resp)
      if (!resp.available) {
        setNotice(
          tr(
            'performance_schema 不可用：这不代表没有慢查询，请查看降级原因。',
            'performance_schema unavailable: this does NOT mean there are no slow queries.'
          )
        )
      }
    } catch (err) {
      setError(describeError(err))
    } finally {
      setBusy(null)
    }
  }, [dbId, sort])

  const selectCandidate = (candidate: SlowQueryCandidate) => {
    setDigest(candidate.digest)
    if (candidate.sample_sql) {
      setSql(candidate.sample_sql)
      setSampleSql(candidate.sample_sql)
    } else {
      setSampleSql(undefined)
      setNotice(
        tr(
          '该 digest 只有指标、没有可执行 SQL 样本（baseline_missing，diagnosable=false）；请粘贴实际 SQL 后再诊断。',
          'This digest has metrics but no executable SQL sample (baseline_missing, diagnosable=false). Paste the actual SQL to diagnose.'
        )
      )
    }
  }

  const handleAnalyze = async () => {
    if (!sql.trim()) {
      setError(tr('请粘贴或选择一条 SQL', 'Please paste or select a SQL statement'))
      return
    }
    setBusy('analyze')
    setError('')
    setNotice('')
    resetResults()
    try {
      const resp: AnalyzeResponse = await api.opsAnalyze({
        scenario_id: 'perf-diagnostics',
        db_id: dbId,
        sql: sql.trim(),
        digest,
        sample_sql: sampleSql,
        explain_analyze: explainAnalyze,
        explain_analyze_timeout_ms: timeoutMs,
      })
      setAnalysis(resp)
      setSelectedActions(resp.plan.actions.map(actionId))
      setNotice(
        tr(
          `已生成计划：${resp.plan.actions.length} 个可执行动作，${resp.plan.rewrite_suggestions.length} 条文本改写建议`,
          `Plan generated: ${resp.plan.actions.length} executable action(s), ${resp.plan.rewrite_suggestions.length} text suggestion(s)`
        )
      )
    } catch (err) {
      setError(describeError(err))
    } finally {
      setBusy(null)
    }
  }

  const toggleAction = (id: string) => {
    setSelectedActions((prev) =>
      prev.includes(id) ? prev.filter((item) => item !== id) : [...prev, id]
    )
    setApproval(null)
    setDryRunReport(null)
  }

  const handleDryRun = async () => {
    if (!plan || selectedActions.length === 0) return
    setBusy('dry-run')
    setError('')
    setNotice('')
    try {
      const resp = await api.opsExecute(plan.plan_id, {
        action_ids: selectedActions,
        dry_run: true,
      })
      setDryRunReport(resp.report)
      setNotice(tr('dry-run 完成：未执行任何 DDL', 'Dry-run completed without executing DDL'))
    } catch (err) {
      setError(describeError(err))
    } finally {
      setBusy(null)
    }
  }

  const handleApprove = async () => {
    if (!plan || selectedActions.length === 0) return
    setBusy('approve')
    setError('')
    setNotice('')
    try {
      const resp = await api.opsApprove(plan.plan_id, {
        plan_hash: plan.plan_hash,
        action_ids: selectedActions,
      })
      setApproval({ token: resp.approval_token, expiresAt: resp.expires_at })
      setAnalysis((prev) =>
        prev ? { ...prev, plan: { ...prev.plan, status: 'approved' } } : prev
      )
      setNotice(tr('审批令牌已签发（一次性，绑定计划哈希）', 'Approval token issued (single-use, plan-hash bound)'))
    } catch (err) {
      setError(describeError(err))
    } finally {
      setBusy(null)
    }
  }

  const handleExecute = async () => {
    if (!plan || !approval) return
    setBusy('execute')
    setError('')
    setNotice('')
    try {
      const resp = await api.opsExecute(plan.plan_id, {
        action_ids: selectedActions,
        approval_token: approval.token,
      })
      setExecution(resp.report)
      setAnalysis((prev) => (prev ? { plan: resp.plan, evidence: prev.evidence } : prev))
      setApproval(null)
      if (resp.report.error) {
        setError(resp.report.error)
      } else {
        setNotice(tr('执行完成，已按同一口径复测', 'Execution completed and verified with the same evidence standard'))
      }
    } catch (err) {
      setError(describeError(err))
    } finally {
      setBusy(null)
    }
  }

  const handleRollback = async () => {
    if (!plan || !rollbackReason.trim()) {
      setError(tr('回滚必须填写原因', 'A rollback reason is required'))
      return
    }
    setBusy('rollback')
    setError('')
    setNotice('')
    try {
      const resp = await api.opsRollback(plan.plan_id, rollbackReason.trim())
      setExecution(resp.report)
      setAnalysis((prev) => (prev ? { plan: resp.plan, evidence: prev.evidence } : prev))
      setNotice(tr('回滚完成', 'Rollback completed'))
    } catch (err) {
      setError(describeError(err))
    } finally {
      setBusy(null)
    }
  }

  const canApprove = Boolean(plan && selectedActions.length > 0 && plan.status === 'pending_approval')
  const canExecute = Boolean(plan && approval && plan.status === 'approved')
  const canRollback = Boolean(plan && ['executed', 'failed'].includes(plan.status))

  return (
    <div className="flex flex-col gap-4">
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
        <div className="flex flex-wrap items-center justify-between gap-3">
          <div>
            <h2 className="text-lg font-semibold text-white">
              {tr('性能诊断（慢 SQL → 方案）', 'Performance diagnostics (slow SQL → plan)')}
            </h2>
            <p className="mt-1 text-xs text-gray-400">
              {tr(
                '粘贴/选择 SQL → 证据 → 方案 → 勾选动作 → dry-run → 审批 → 执行 → 前后对比。SQL 改写建议只是文本，不可执行。',
                'Paste/select SQL → evidence → plan → select actions → dry-run → approval → execute → before/after. Rewrite suggestions are text only.'
              )}
            </p>
          </div>
          <span className="rounded border border-dark-border px-2 py-1 text-xs text-gray-400">
            {dbId || tr('跟随当前连接', 'active connection')}
          </span>
        </div>

        <div className="mt-4 grid gap-4 lg:grid-cols-2">
          <div className="flex flex-col gap-2">
            <label className="text-xs font-medium text-gray-400">
              {tr('目标 SQL（只允许单条只读查询）', 'Target SQL (single read-only statement)')}
            </label>
            <textarea
              value={sql}
              onChange={(event) => {
                setSql(event.target.value)
                setDigest(undefined)
                setSampleSql(undefined)
              }}
              rows={6}
              placeholder="SELECT * FROM orders WHERE user_id = 42 ORDER BY created_at DESC"
              className="min-h-[140px] rounded border border-dark-border bg-dark-bg p-2 font-mono text-xs text-gray-200 outline-none focus:border-blue-500"
            />
            <label className="flex items-center gap-2 text-xs text-gray-400">
              <input
                type="checkbox"
                checked={explainAnalyze}
                onChange={(event) => setExplainAnalyze(event.target.checked)}
              />
              {tr(
                '真实执行 EXPLAIN ANALYZE（会执行该查询，受会话超时约束）',
                'Run EXPLAIN ANALYZE (executes the query, bounded by session timeout)'
              )}
            </label>
            <div className="flex items-center gap-2 text-xs text-gray-400">
              <span>{tr('超时 (ms)', 'Timeout (ms)')}</span>
              <input
                type="number"
                min={500}
                max={60000}
                step={500}
                value={timeoutMs}
                onChange={(event) => setTimeoutMs(Number(event.target.value) || 5000)}
                className="h-8 w-28 rounded border border-dark-border bg-dark-bg px-2 text-xs text-gray-200 outline-none"
              />
            </div>
            <button
              onClick={() => void handleAnalyze()}
              disabled={busy !== null}
              className="mt-1 flex h-9 items-center justify-center gap-2 rounded bg-blue-600 px-3 text-sm font-medium text-white hover:bg-blue-500 disabled:opacity-50"
            >
              {busy === 'analyze' ? <Loader2 className="h-4 w-4 animate-spin" /> : <Sparkles className="h-4 w-4" />}
              {tr('采集证据并生成方案', 'Collect evidence & build plan')}
            </button>
          </div>

          <div className="flex flex-col gap-2">
            <div className="flex flex-wrap items-center gap-2">
              <label className="text-xs font-medium text-gray-400">
                {tr('performance_schema 慢查询', 'performance_schema slow queries')}
              </label>
              <select
                value={sort}
                onChange={(event) => setSort(event.target.value)}
                className="h-8 rounded border border-dark-border bg-dark-bg px-2 text-xs text-gray-200 outline-none"
              >
                <option value="total_latency">{tr('总延迟', 'Total latency')}</option>
                <option value="avg_latency">{tr('平均延迟', 'Avg latency')}</option>
                <option value="rows_examined">{tr('扫描行数', 'Rows examined')}</option>
              </select>
              <button
                onClick={() => void handleListSlowQueries()}
                disabled={busy !== null}
                className="flex h-8 items-center gap-1 rounded border border-dark-border px-2.5 text-xs text-gray-200 hover:bg-dark-surface disabled:opacity-50"
              >
                {busy === 'list' ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <Search className="h-3.5 w-3.5" />}
                {tr('读取慢查询', 'Load slow queries')}
              </button>
            </div>

            {scan && (
              <div className="space-y-2">
                <WindowBanner window={scan.window} />
                {!scan.available && scan.degraded_reasons.length > 0 && (
                  <div className="rounded border border-amber-500/40 bg-amber-500/10 p-2 text-xs text-amber-300">
                    {tr('降级原因：', 'Degraded: ')}
                    {scan.degraded_reasons.join('; ')}
                  </div>
                )}
                {scan.notes.length > 0 && (
                  <ul className="space-y-1 text-[11px] text-gray-500">
                    {scan.notes.map((note, index) => (
                      <li key={index}>- {note}</li>
                    ))}
                  </ul>
                )}
                <div className="max-h-64 space-y-1.5 overflow-y-auto">
                  {scan.candidates.length === 0 && (
                    <div className="rounded border border-dark-border bg-dark-bg p-2 text-xs text-gray-500">
                      {tr('没有候选；这不等于没有慢查询', 'No candidates; this does not mean there are no slow queries')}
                    </div>
                  )}
                  {scan.candidates.map((candidate) => (
                    <button
                      key={candidate.digest}
                      onClick={() => selectCandidate(candidate)}
                      className="w-full rounded border border-dark-border bg-dark-bg p-2 text-left text-[11px] text-gray-300 hover:border-blue-500/50"
                    >
                      <div className="flex flex-wrap items-center gap-2">
                        <span className="font-mono text-gray-500">{candidate.digest.slice(0, 12)}…</span>
                        {candidate.scan_flag != null && (
                          <span
                            className={`rounded px-1.5 py-0.5 ${
                              candidate.scan_flag
                                ? 'border border-amber-500/40 text-amber-300'
                                : 'border border-dark-border text-gray-500'
                            }`}
                          >
                            {candidate.scan_flag
                              ? tr('P_S 疑似全表扫描', 'P_S flagged scan')
                              : tr('未见扫描标记', 'No scan flag')}
                          </span>
                        )}
                        {!candidate.sample_sql && (
                          <span className="rounded border border-red-500/40 px-1.5 py-0.5 text-red-300">
                            baseline_missing · diagnosable=false
                          </span>
                        )}
                      </div>
                      <div className="mt-1 line-clamp-2 font-mono text-gray-400">{candidate.digest_text}</div>
                      <div className="mt-1 text-gray-500">{candidate.summary}</div>
                    </button>
                  ))}
                </div>
              </div>
            )}
          </div>
        </div>
      </div>

      {evidence && perf && (
        <div className="rounded-lg border border-dark-border bg-dark-panel p-4">
          <h3 className="mb-3 text-sm font-semibold text-white">{tr('证据', 'Evidence')}</h3>
          <div className="space-y-3">
            <WindowBanner window={perf.window} />

            {perf.row_estimate && (
              <div className="rounded border border-dark-border bg-dark-bg p-2 text-xs">
                <span className="text-gray-300">{tr('行数估算', 'Row estimate')}: </span>
                <span className={perf.row_estimate.trusted ? 'text-emerald-400' : 'text-amber-400'}>
                  {perf.row_estimate.value} · {perf.row_estimate.source} ·{' '}
                  {perf.row_estimate.trusted ? tr('可信', 'trusted') : tr('不可信', 'untrusted')}
                </span>
                <div className="mt-1 text-gray-500">{perf.row_estimate.reason}</div>
              </div>
            )}

            {perf.digest && (
              <div className="rounded border border-dark-border bg-dark-bg p-2 text-xs text-gray-300">
                <div className="font-medium text-gray-200">
                  {tr('累计指标', 'Cumulative metrics')} · digest {perf.digest.digest.slice(0, 12)}…
                </div>
                <div className="mt-1 text-gray-500">
                  {tr('执行', 'Executions')} {perf.digest.count_star} ·{' '}
                  {tr('总延迟', 'Total')} {perf.digest.total_latency_ms.toFixed(1)}ms ·{' '}
                  {tr('平均', 'Avg')} {perf.digest.avg_latency_ms.toFixed(2)}ms ·{' '}
                  {tr('扫描总和/平均', 'Rows examined sum/avg')} {perf.digest.rows_examined_sum}/
                  {perf.digest.rows_examined_avg} · {tr('临时表', 'Tmp tables')}{' '}
                  {perf.digest.created_tmp_tables}/{perf.digest.created_tmp_disk_tables}
                </div>
              </div>
            )}

            <div className="grid gap-3 text-xs md:grid-cols-2">
              <div className="rounded border border-dark-border bg-dark-bg p-3">
                <div className="text-gray-400">
                  {evidence.db_name}.{evidence.table} · {tr('估算扫描行数', 'Estimated scan rows')}{' '}
                  {perf.estimated_scan_rows ?? '-'}
                </div>
                {perf.tables.map((table) => (
                  <div key={table.table} className="mt-2 rounded border border-dark-border p-2">
                    <div className="font-mono text-gray-300">
                      {table.table} · rows={table.stats.table_rows ?? '-'} · data={table.stats.data_length ?? '-'}{' '}
                      · stats={table.freshness.status}
                    </div>
                    <div className="mt-1 text-[11px] text-gray-500">
                      update_time={table.freshness.update_time || '-'} innodb={table.freshness.innodb_last_update || '-'}
                    </div>
                    <div className="mt-1 flex flex-wrap gap-1">
                      {table.snapshot.indexes.map((index) => (
                        <span
                          key={index.name}
                          className="rounded border border-dark-border px-1.5 py-0.5 font-mono text-gray-400"
                        >
                          {index.name}({index.columns.map((c) => c.name).join(',')})
                        </span>
                      ))}
                    </div>
                  </div>
                ))}
              </div>

              <div className="rounded border border-dark-border bg-dark-bg p-3">
                <div className="text-gray-400">{tr('EXPLAIN', 'EXPLAIN')}</div>
                <ul className="mt-2 space-y-1">
                  {perf.explain.map((sample) => (
                    <ExplainRow key={sample.label} sample={sample} />
                  ))}
                </ul>
                {perf.actual_latency_ms != null && (
                  <div className="mt-2 text-[11px] text-emerald-400">
                    {tr('实际耗时', 'Actual latency')}: {perf.actual_latency_ms.toFixed(3)}ms
                  </div>
                )}
              </div>
            </div>

            {perf.implicit_casts.length > 0 && (
              <div className="rounded border border-dark-border bg-dark-bg p-2 text-xs">
                <div className="text-gray-300">{tr('疑似隐式类型转换', 'Suspected implicit casts')}</div>
                <ul className="mt-1 space-y-1 text-gray-500">
                  {perf.implicit_casts.map((cast) => (
                    <li key={`${cast.table}.${cast.column}`}>
                      <span className={cast.index_risk ? 'text-amber-300' : 'text-gray-400'}>
                        {cast.table}.{cast.column} {cast.operator} {cast.literal}
                      </span>{' '}
                      {cast.detail}
                    </li>
                  ))}
                </ul>
              </div>
            )}

            {perf.degraded_reasons.length > 0 && (
              <div className="rounded border border-amber-500/40 bg-amber-500/10 p-2 text-xs text-amber-300">
                {tr('降级原因', 'Degraded reasons')}: {perf.degraded_reasons.join('; ')}
              </div>
            )}
            {perf.notes.length > 0 && (
              <ul className="space-y-1 text-[11px] text-gray-500">
                {perf.notes.map((note, index) => (
                  <li key={index}>- {note}</li>
                ))}
              </ul>
            )}
          </div>
        </div>
      )}

      {plan && (
        <div className="rounded-lg border border-dark-border bg-dark-panel p-4">
          <div className="flex flex-wrap items-start justify-between gap-3">
            <div>
              <h3 className="text-sm font-semibold text-white">{tr('方案', 'Plan')}</h3>
              <p className="mt-1 text-xs text-gray-400">{plan.diagnosis}</p>
              <div className="mt-2 flex flex-wrap gap-2 text-[11px] text-gray-500">
                <span className="rounded border border-dark-border px-1.5 py-0.5">
                  {tr('状态', 'Status')}: {plan.status}
                </span>
                <span className="rounded border border-dark-border px-1.5 py-0.5">
                  {tr('置信度', 'Confidence')}: {Math.round(plan.confidence * 100)}%
                </span>
                <span className="rounded border border-dark-border px-1.5 py-0.5 font-mono">
                  hash: {plan.plan_hash.slice(0, 12)}…
                </span>
              </div>
            </div>
            <div className="flex flex-wrap gap-2">
              <button
                onClick={() => void handleDryRun()}
                disabled={busy !== null || selectedActions.length === 0}
                className="flex h-8 items-center gap-1 rounded border border-dark-border px-2.5 text-xs text-gray-200 hover:bg-dark-surface disabled:opacity-50"
              >
                {busy === 'dry-run' ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <ShieldCheck className="h-3.5 w-3.5" />}
                {tr('Dry-run', 'Dry-run')}
              </button>
              <button
                onClick={() => void handleApprove()}
                disabled={busy !== null || !canApprove}
                className="flex h-8 items-center gap-1 rounded bg-emerald-600 px-2.5 text-xs text-white hover:bg-emerald-500 disabled:opacity-50"
              >
                {busy === 'approve' ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <CheckCircle2 className="h-3.5 w-3.5" />}
                {tr('审批选中动作', 'Approve selected')}
              </button>
              <button
                onClick={() => void handleExecute()}
                disabled={busy !== null || !canExecute || !policyAllowsApply}
                title={!policyAllowsApply ? tr('当前策略默认只读，需显式开启 allow_apply', 'Apply disabled by policy') : ''}
                className="flex h-8 items-center gap-1 rounded bg-amber-600 px-2.5 text-xs text-white hover:bg-amber-500 disabled:opacity-50"
              >
                {busy === 'execute' ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <Play className="h-3.5 w-3.5" />}
                {tr('执行', 'Execute')}
              </button>
            </div>
          </div>

          <div className="mt-4 grid gap-4 lg:grid-cols-2">
            <div className="space-y-3">
              <div>
                <h4 className="mb-2 text-xs font-semibold text-gray-300">
                  {tr('可执行动作（审批/执行链路）', 'Executable actions (approval/execution chain)')}
                </h4>
                <div className="space-y-2">
                  {plan.actions.length === 0 && (
                    <div className="rounded border border-dark-border bg-dark-bg p-3 text-xs text-gray-500">
                      {tr('没有可自动执行的结构化动作', 'No executable structured actions')}
                    </div>
                  )}
                  {plan.actions.map((action) => {
                    const id = actionId(action)
                    const checked = selectedActions.includes(id)
                    const label =
                      action.type === 'analyze_table'
                        ? `ANALYZE TABLE ${action.table}`
                        : `CREATE INDEX ${action.name} ON ${action.table} (${(action.columns || []).join(', ')})`
                    return (
                      <label
                        key={id}
                        className={`flex cursor-pointer items-start gap-2 rounded border p-2.5 text-xs ${
                          checked ? 'border-blue-500/50 bg-blue-500/5' : 'border-dark-border bg-dark-bg'
                        }`}
                      >
                        <input
                          type="checkbox"
                          checked={checked}
                          onChange={() => toggleAction(id)}
                          disabled={!['pending_approval', 'approved'].includes(plan.status)}
                        />
                        <div className="min-w-0 font-mono text-gray-200">{label}</div>
                      </label>
                    )
                  })}
                </div>
              </div>

              <div>
                <h4 className="mb-2 text-xs font-semibold text-gray-300">
                  {tr('SQL 改写建议（仅文本，不可执行）', 'SQL rewrite suggestions (text only)')}
                </h4>
                {plan.rewrite_suggestions.length === 0 ? (
                  <div className="rounded border border-dark-border bg-dark-bg p-2 text-xs text-gray-500">
                    {tr('没有改写建议', 'No rewrite suggestions')}
                  </div>
                ) : (
                  <div className="space-y-1.5">
                    {plan.rewrite_suggestions.map((suggestion) => (
                      <div key={suggestion.id} className="rounded border border-dark-border bg-dark-bg p-2 text-xs">
                        <div className="font-medium text-gray-200">{suggestion.title}</div>
                        <div className="mt-1 text-gray-400">{suggestion.suggestion}</div>
                        <div className="mt-1 text-gray-500">
                          {tr('依据', 'Rationale')}: {suggestion.rationale}
                        </div>
                        <div className="mt-1 text-emerald-400/80">
                          {tr('预期收益', 'Expected gain')}: {suggestion.expected_gain}
                        </div>
                      </div>
                    ))}
                  </div>
                )}
              </div>

              <div>
                <h4 className="mb-2 text-xs font-semibold text-gray-300">{tr('回滚', 'Rollback')}</h4>
                <div className="rounded border border-dark-border bg-dark-bg p-2.5 text-xs text-gray-400">
                  {plan.rollback.summary}
                </div>
              </div>
            </div>

            <div className="space-y-3">
              <div>
                <h4 className="mb-2 text-xs font-semibold text-gray-300">
                  {tr('校验与风险', 'Validation & risks')}
                </h4>
                <div className="space-y-1.5">
                  {plan.validation_checks.map((check) => (
                    <div key={check.id} className="rounded border border-dark-border bg-dark-bg p-2 text-xs">
                      <div className={`font-medium ${statusClass(check.status)}`}>
                        [{check.status}] {check.title}
                      </div>
                      <div className="mt-1 break-all font-mono text-[11px] text-gray-500">{check.detail}</div>
                    </div>
                  ))}
                  {plan.risks.map((risk) => (
                    <div key={risk.id} className="rounded border border-dark-border bg-dark-bg p-2 text-xs">
                      <div className={`font-medium ${severityClass(risk.severity)}`}>
                        [{risk.severity}] {risk.title}
                        {risk.blocking ? tr(' · 阻断', ' · blocking') : ''}
                      </div>
                      <div className="mt-1 text-gray-500">{risk.detail}</div>
                    </div>
                  ))}
                </div>
              </div>
            </div>
          </div>

          {plan.findings.length > 0 && (
            <div className="mt-4">
              <h4 className="mb-2 text-xs font-semibold text-gray-300">{tr('发现', 'Findings')}</h4>
              <ul className="space-y-1 text-xs">
                {plan.findings.map((finding) => (
                  <li key={finding.id} className="rounded border border-dark-border bg-dark-bg p-2">
                    <span className={`font-medium ${severityClass(finding.severity)}`}>{finding.title}</span>
                    <span className="ml-2 text-gray-500">{finding.detail}</span>
                  </li>
                ))}
              </ul>
            </div>
          )}

          {approval && (
            <div className="mt-4 rounded border border-emerald-500/40 bg-emerald-500/10 p-3 text-xs text-emerald-300">
              {tr('审批令牌有效期至', 'Approval token expires at')}{' '}
              {new Date(approval.expiresAt * 1000).toLocaleTimeString()}
            </div>
          )}

          {(dryRunReport || execution) && (
            <div className="mt-4 space-y-2">
              <h4 className="text-xs font-semibold text-gray-300">
                {dryRunReport ? tr('Dry-run 结果', 'Dry-run result') : tr('执行结果', 'Execution result')}
              </h4>
              {(dryRunReport || execution)!.compiled_sql.map((compiled) => (
                <div
                  key={compiled}
                  className="break-all rounded border border-dark-border bg-dark-bg p-2 font-mono text-[11px] text-gray-300"
                >
                  {compiled}
                </div>
              ))}
              {(dryRunReport || execution)!.verification && (
                <div className="rounded border border-dark-border bg-dark-bg p-2 text-xs text-gray-300">
                  <div>
                    {tr('复测', 'Verification')}: {(dryRunReport || execution)!.verification!.summary}
                  </div>
                  {(dryRunReport || execution)!.verification!.comparisons.length > 0 && (
                    <div className="mt-2 overflow-x-auto">
                      <table className="w-full text-[11px]">
                        <thead className="text-gray-500">
                          <tr>
                            <th className="px-1 py-0.5 text-left">{tr('查询', 'Query')}</th>
                            <th className="px-1 py-0.5 text-left">key</th>
                            <th className="px-1 py-0.5 text-right">{tr('扫描行', 'Rows')}</th>
                            <th className="px-1 py-0.5 text-right">{tr('成本', 'Cost')}</th>
                            <th className="px-1 py-0.5 text-right">{tr('实际耗时', 'Actual')}</th>
                            <th className="px-1 py-0.5 text-left">{tr('结论', 'Verdict')}</th>
                          </tr>
                        </thead>
                        <tbody>
                          {(dryRunReport || execution)!.verification!.comparisons.map((item) => (
                            <tr key={item.label} className="border-t border-dark-border">
                              <td className="px-1 py-0.5 font-mono">{item.label}</td>
                              <td className="px-1 py-0.5 font-mono text-gray-500">
                                {item.before_key || '-'} → {item.after_key || '-'}
                              </td>
                              <td className="px-1 py-0.5 text-right font-mono text-gray-500">
                                {item.before_rows ?? '-'} → {item.after_rows ?? '-'}
                              </td>
                              <td className="px-1 py-0.5 text-right font-mono text-gray-500">
                                {item.before_cost != null ? item.before_cost.toFixed(2) : '-'} →{' '}
                                {item.after_cost != null ? item.after_cost.toFixed(2) : '-'}
                              </td>
                              <td className="px-1 py-0.5 text-right font-mono text-gray-500">
                                {item.before_actual_ms != null ? item.before_actual_ms.toFixed(3) : '-'} →{' '}
                                {item.after_actual_ms != null ? item.after_actual_ms.toFixed(3) : '-'}
                              </td>
                              <td className={`px-1 py-0.5 ${item.improved ? 'text-emerald-400' : 'text-amber-400'}`}>
                                {item.verdict}
                              </td>
                            </tr>
                          ))}
                        </tbody>
                      </table>
                    </div>
                  )}
                  {(dryRunReport || execution)!.verification!.notes.length > 0 && (
                    <ul className="mt-2 space-y-0.5 text-[11px] text-gray-500">
                      {(dryRunReport || execution)!.verification!.notes.map((note, index) => (
                        <li key={index}>- {note}</li>
                      ))}
                    </ul>
                  )}
                </div>
              )}
              {(dryRunReport || execution)!.impact && (
                <div className="rounded border border-dark-border bg-dark-bg p-2 text-xs text-gray-400">
                  {tr('估算行数', 'Rows')}: {(dryRunReport || execution)!.impact!.estimated_rows ?? '-'} ·{' '}
                  {tr('估算索引体积', 'Est. index bytes')}: {' '}
                  {(dryRunReport || execution)!.impact!.estimated_index_bytes ?? '-'}
                </div>
              )}
              {(dryRunReport || execution)!.error && (
                <div className="rounded border border-red-500/40 bg-red-500/10 p-2 text-xs text-red-300">
                  {(dryRunReport || execution)!.error}
                </div>
              )}
            </div>
          )}

          {canRollback && (
            <div className="mt-4 flex flex-wrap items-end gap-2 rounded border border-dark-border bg-dark-bg p-3">
              <div className="flex-1">
                <label className="text-xs text-gray-400">{tr('回滚原因（必填）', 'Rollback reason (required)')}</label>
                <input
                  value={rollbackReason}
                  onChange={(event) => setRollbackReason(event.target.value)}
                  className="mt-1 h-8 w-full rounded border border-dark-border bg-dark-panel px-2 text-xs text-gray-200 outline-none"
                  placeholder={tr('例如：执行后写入延迟升高', 'e.g. write latency increased')}
                />
              </div>
              <button
                onClick={() => void handleRollback()}
                disabled={busy !== null || !rollbackReason.trim()}
                className="flex h-8 items-center gap-1 rounded border border-amber-500/50 px-2.5 text-xs text-amber-300 hover:bg-amber-500/10 disabled:opacity-50"
              >
                {busy === 'rollback' ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <RotateCcw className="h-3.5 w-3.5" />}
                {tr('回滚本次新增索引', 'Rollback new indexes')}
              </button>
            </div>
          )}
        </div>
      )}

      <div className="flex items-center gap-2 rounded-lg border border-dark-border bg-dark-panel p-3 text-xs text-gray-500">
        <AlertTriangle className="h-4 w-4 text-amber-400" />
        {tr(
          '安全边界：EXPLAIN ANALYZE 会真实执行只读查询并受 max_execution_time 约束；SQL 改写建议不会进入执行链路。',
          'Safety: EXPLAIN ANALYZE executes the read-only query under max_execution_time; rewrite suggestions never enter the execution chain.'
        )}
      </div>
    </div>
  )
}
