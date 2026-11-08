import { useCallback, useEffect, useMemo, useState } from 'react'
import {
  AlertTriangle,
  CheckCircle2,
  Database,
  Loader2,
  Play,
  RotateCcw,
  ShieldCheck,
  Sparkles,
} from 'lucide-react'
import { api } from '../../api'
import { tr } from '../../i18n'

interface OpsCenterProps {
  dbId?: string
  dbType?: string
  isActive?: boolean
}

interface ScenarioDescriptor {
  id: string
  title: string
  description: string
  engines: string[]
  status: string
  risk_ceiling: string
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

interface EvidenceSummary {
  table: string
  column_count: number
  index_count: number
  row_estimate?: number
  explain_findings?: string[]
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
  confidence: number
  plan_hash: string
  status: string
  evidence_summary?: EvidenceSummary
  execution?: ExecutionReport
}

interface EvidencePack {
  db_name: string
  table: string
  snapshot: {
    columns: Array<{ name: string; column_type: string; nullable: boolean }>
    indexes: Array<{ name: string; unique: boolean; columns: Array<{ name: string; seq: number }> }>
  }
  stats: {
    engine?: string
    table_rows?: number
    data_length?: number
    index_length?: number
  }
  explain: Array<{
    label: string
    sql: string
    access_type?: string
    key?: string
    rows_examined?: number
    extra?: string
  }>
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
  verification?: {
    improved_queries: number
    total_queries: number
    summary: string
  }
}

interface AnalyzeResponse {
  plan: OpsPlan
  evidence: EvidencePack
}

const ACTION_ID_PREFIX: Record<string, string> = {
  create_index: 'create_index',
  drop_index: 'drop_index',
  analyze_table: 'analyze_table',
}

function actionId(action: ActionSpec): string {
  const prefix = ACTION_ID_PREFIX[action.type] || action.type
  const suffix =
    action.type === 'analyze_table' ? action.table : action.name
  return `${prefix}:${suffix || ''}`
}

function describeError(error: unknown): string {
  if (!error) return ''
  if (typeof error === 'string') return error
  const anyError = error as { message?: string; response?: { data?: { message?: string; error?: string } } }
  return (
    anyError.response?.data?.message ||
    anyError.response?.data?.error ||
    anyError.message ||
    String(error)
  )
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

export function OpsCenter({ dbId, dbType }: OpsCenterProps) {
  const [scenarios, setScenarios] = useState<ScenarioDescriptor[]>([])
  const [policy, setPolicy] = useState<Record<string, any> | null>(null)
  const [table, setTable] = useState('')
  const [workload, setWorkload] = useState('')
  const [useLlm, setUseLlm] = useState(false)
  const [analysis, setAnalysis] = useState<AnalyzeResponse | null>(null)
  const [selectedActions, setSelectedActions] = useState<string[]>([])
  const [approval, setApproval] = useState<{ token: string; expiresAt: number; planHash: string } | null>(null)
  const [dryRunReport, setDryRunReport] = useState<ExecutionReport | null>(null)
  const [execution, setExecution] = useState<ExecutionReport | null>(null)
  const [rollbackReason, setRollbackReason] = useState('')
  const [busy, setBusy] = useState<string | null>(null)
  const [error, setError] = useState('')
  const [notice, setNotice] = useState('')
  const [audit, setAudit] = useState<Array<Record<string, any>>>([])

  const plan = analysis?.plan || null
  const evidence = analysis?.evidence || null

  const workloadSamples = useMemo(
    () =>
      workload
        .split('\n')
        .map((line) => line.trim())
        .filter((line) => line.length > 0),
    [workload]
  )

  const loadMeta = useCallback(async () => {
    try {
      const [scenarioResp, policyResp] = await Promise.all([api.opsScenarios(), api.opsGetPolicy()])
      setScenarios(scenarioResp.scenarios || [])
      setPolicy(policyResp)
    } catch (err) {
      setError(describeError(err))
    }
  }, [])

  const loadAudit = useCallback(async () => {
    try {
      const resp = await api.opsAudit(40)
      setAudit(resp.records || [])
    } catch {
      // 审计读取失败不阻塞主流程
    }
  }, [])

  useEffect(() => {
    void loadMeta()
    void loadAudit()
  }, [loadMeta, loadAudit])

  const resetResults = () => {
    setSelectedActions([])
    setApproval(null)
    setDryRunReport(null)
    setExecution(null)
    setRollbackReason('')
    setError('')
    setNotice('')
  }

  const handleAnalyze = async () => {
    if (!table.trim()) {
      setError(tr('请先填写表名', 'Please enter a table name'))
      return
    }
    setBusy('analyze')
    setError('')
    setNotice('')
    resetResults()
    try {
      const resp: AnalyzeResponse = await api.opsAnalyze({
        scenario_id: 'mysql-index-advisor',
        db_id: dbId,
        table: table.trim(),
        workload_sql: workloadSamples,
        use_llm: useLlm,
      })
      setAnalysis(resp)
      setSelectedActions(resp.plan.actions.map(actionId))
      setNotice(
        tr(
          `已生成计划，共 ${resp.plan.actions.length} 个候选动作`,
          `Plan generated with ${resp.plan.actions.length} candidate action(s)`
        )
      )
      void loadAudit()
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
      void loadAudit()
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
      setApproval({
        token: resp.approval_token,
        expiresAt: resp.expires_at,
        planHash: resp.plan_hash,
      })
      setAnalysis((prev) =>
        prev ? { ...prev, plan: { ...prev.plan, status: 'approved' } } : prev
      )
      setNotice(
        tr(
          '审批令牌已签发，绑定当前计划哈希，且只能使用一次',
          'Approval token issued, bound to plan hash and single-use'
        )
      )
      void loadAudit()
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
      const failed = Boolean(resp.report.error)
      if (failed) {
        setError(resp.report.error || tr('执行失败', 'Execution failed'))
        setNotice(
          resp.report.rollback_performed
            ? tr('执行失败，已补偿本次新增索引', 'Execution failed; created indexes were compensated')
            : tr('执行失败，请检查审计与索引状态', 'Execution failed; check audit and index state')
        )
      } else {
        setNotice(tr('索引变更已执行并完成复测', 'Index change executed and verified'))
      }
      void loadAudit()
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
      void loadAudit()
    } catch (err) {
      setError(describeError(err))
    } finally {
      setBusy(null)
    }
  }

  const canApprove = Boolean(plan && selectedActions.length > 0 && plan.status === 'pending_approval')
  const canExecute = Boolean(plan && approval && plan.status === 'approved')
  const canRollback = Boolean(plan && ['executed', 'failed'].includes(plan.status))
  const policyAllowsApply = Boolean(policy?.policy?.allow_apply)
  const policyEnv = String(policy?.policy?.environment || 'dev')

  return (
    <div className="flex h-full flex-col gap-4 overflow-y-auto bg-dark-bg p-4 text-gray-200">
      <div className="rounded-lg border border-dark-border bg-dark-panel p-4">
        <div className="flex flex-wrap items-center justify-between gap-3">
          <div>
            <h2 className="text-lg font-semibold text-white">
              {tr('运维中心 · MySQL 索引优化', 'Ops Center · MySQL Index Advisor')}
            </h2>
            <p className="mt-1 text-xs text-gray-400">
              {tr(
                '阶段 0：采集证据 → 生成计划 → 审批 → 服务端编译 DDL → 执行 → 复测 → 回滚。Agent 不产出可执行 SQL。',
                'Stage 0: evidence → plan → approval → server-side DDL compile → execute → verify → rollback. Agent never emits executable SQL.'
              )}
            </p>
          </div>
          <div className="flex flex-wrap items-center gap-2 text-xs">
            <span className="rounded border border-dark-border px-2 py-1 text-gray-300">
              {tr('当前数据库', 'DB')}: {dbId || tr('跟随当前连接', 'active connection')}
            </span>
            <span className="rounded border border-dark-border px-2 py-1 text-gray-300">
              {dbType || 'mysql'}
            </span>
            <span
              className={`rounded border px-2 py-1 ${
                policyAllowsApply
                  ? 'border-amber-500/50 text-amber-300'
                  : 'border-emerald-500/40 text-emerald-300'
              }`}
            >
              {policyAllowsApply
                ? tr('已开启执行', 'Apply enabled')
                : tr('默认只读 + dry-run', 'Read-only + dry-run by default')}
            </span>
            <span className="rounded border border-dark-border px-2 py-1 text-gray-400">
              env: {policyEnv}
            </span>
          </div>
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
        <div className="grid gap-3 md:grid-cols-[260px_1fr]">
          <div className="flex flex-col gap-2">
            <label className="text-xs font-medium text-gray-400">{tr('表名', 'Table')}</label>
            <div className="flex items-center gap-2 rounded border border-dark-border bg-dark-bg px-2">
              <Database className="h-4 w-4 text-blue-400" />
              <input
                value={table}
                onChange={(event) => setTable(event.target.value)}
                placeholder="orders"
                className="h-9 flex-1 bg-transparent font-mono text-sm text-white outline-none"
              />
            </div>
            <label className="mt-2 flex items-center gap-2 text-xs text-gray-400">
              <input
                type="checkbox"
                checked={useLlm}
                onChange={(event) => setUseLlm(event.target.checked)}
              />
              {tr('使用模型辅助（复用现有 AI 调用栈）', 'AI-assisted (reuses existing AI stack)')}
            </label>
            <button
              onClick={() => void handleAnalyze()}
              disabled={busy !== null}
              className="mt-2 flex h-9 items-center justify-center gap-2 rounded bg-blue-600 px-3 text-sm font-medium text-white hover:bg-blue-500 disabled:cursor-not-allowed disabled:opacity-50"
            >
              {busy === 'analyze' ? <Loader2 className="h-4 w-4 animate-spin" /> : <Sparkles className="h-4 w-4" />}
              {tr('采集证据并生成计划', 'Collect evidence & build plan')}
            </button>
          </div>
          <div className="flex flex-col gap-2">
            <label className="text-xs font-medium text-gray-400">
              {tr('工作负载 SQL（每行一条）', 'Workload SQL (one per line)')}
            </label>
            <textarea
              value={workload}
              onChange={(event) => setWorkload(event.target.value)}
              rows={5}
              placeholder="SELECT * FROM orders WHERE user_id = 1 AND status = 'paid' ORDER BY created_at DESC"
              className="min-h-[120px] rounded border border-dark-border bg-dark-bg p-2 font-mono text-xs text-gray-200 outline-none focus:border-blue-500"
            />
            <p className="text-xs text-gray-500">
              {tr(
                '只会执行 EXPLAIN，不会执行这些查询本身。',
                'Only EXPLAIN is executed; the queries themselves are never executed.'
              )}
            </p>
          </div>
        </div>
      </div>

      {evidence && (
        <div className="rounded-lg border border-dark-border bg-dark-panel p-4">
          <h3 className="mb-3 text-sm font-semibold text-white">{tr('证据', 'Evidence')}</h3>
          <div className="grid gap-3 text-xs md:grid-cols-2">
            <div className="rounded border border-dark-border bg-dark-bg p-3">
              <div className="text-gray-400">
                {evidence.db_name}.{evidence.table}
              </div>
              <div className="mt-2 text-gray-300">
                {tr('列', 'Columns')}: {evidence.snapshot.columns.length} ·{' '}
                {tr('索引', 'Indexes')}: {evidence.snapshot.indexes.length} ·{' '}
                {tr('估算行数', 'Rows')}: {evidence.stats.table_rows ?? '-'}
              </div>
              <div className="mt-2 flex flex-wrap gap-1">
                {evidence.snapshot.indexes.map((index) => (
                  <span
                    key={index.name}
                    className="rounded border border-dark-border px-1.5 py-0.5 font-mono text-gray-400"
                  >
                    {index.name}({index.columns.map((c) => c.name).join(',')})
                  </span>
                ))}
              </div>
            </div>
            <div className="rounded border border-dark-border bg-dark-bg p-3">
              <div className="text-gray-400">{tr('EXPLAIN 摘要', 'EXPLAIN summary')}</div>
              {evidence.explain.length === 0 ? (
                <div className="mt-2 text-gray-500">
                  {tr('没有工作负载样本', 'No workload samples')}
                </div>
              ) : (
                <ul className="mt-2 space-y-1 font-mono text-[11px] text-gray-300">
                  {evidence.explain.map((sample) => (
                    <li key={sample.label}>
                      {sample.label}: type={sample.access_type || '-'} key={sample.key || '-'} rows={sample.rows_examined ?? '-'} {sample.extra || ''}
                    </li>
                  ))}
                </ul>
              )}
            </div>
          </div>
          {evidence.notes.length > 0 && (
            <ul className="mt-3 space-y-1 text-xs text-gray-500">
              {evidence.notes.map((note, index) => (
                <li key={index}>- {note}</li>
              ))}
            </ul>
          )}
        </div>
      )}

      {plan && (
        <div className="rounded-lg border border-dark-border bg-dark-panel p-4">
          <div className="flex flex-wrap items-start justify-between gap-3">
            <div>
              <h3 className="text-sm font-semibold text-white">{tr('计划', 'Plan')}</h3>
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
                {tr('Dry-run 影响评估', 'Dry-run impact')}
              </button>
              <button
                onClick={() => void handleApprove()}
                disabled={busy !== null || !canApprove}
                className="flex h-8 items-center gap-1 rounded bg-emerald-600 px-2.5 text-xs text-white hover:bg-emerald-500 disabled:cursor-not-allowed disabled:opacity-50"
              >
                {busy === 'approve' ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <CheckCircle2 className="h-3.5 w-3.5" />}
                {tr('审批选中动作', 'Approve selected')}
              </button>
              <button
                onClick={() => void handleExecute()}
                disabled={busy !== null || !canExecute || !policyAllowsApply}
                title={!policyAllowsApply ? tr('当前策略默认只读，需显式开启 allow_apply', 'Apply disabled by policy') : ''}
                className="flex h-8 items-center gap-1 rounded bg-amber-600 px-2.5 text-xs text-white hover:bg-amber-500 disabled:cursor-not-allowed disabled:opacity-50"
              >
                {busy === 'execute' ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <Play className="h-3.5 w-3.5" />}
                {tr('一键执行', 'Execute')}
              </button>
            </div>
          </div>

          <div className="mt-4 grid gap-4 lg:grid-cols-2">
            <div className="space-y-3">
              <div>
                <h4 className="mb-2 text-xs font-semibold text-gray-300">
                  {tr('候选动作', 'Candidate actions')}
                </h4>
                <div className="space-y-2">
                  {plan.actions.length === 0 && (
                    <div className="rounded border border-dark-border bg-dark-bg p-3 text-xs text-gray-500">
                      {tr('没有可执行的索引候选', 'No executable candidates')}
                    </div>
                  )}
                  {plan.actions.map((action) => {
                    const id = actionId(action)
                    const checked = selectedActions.includes(id)
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
                        <div className="min-w-0">
                          <div className="font-mono text-gray-200">
                            CREATE INDEX {action.name}
                          </div>
                          <div className="mt-1 text-gray-500">
                            {action.table}({(action.columns || []).join(', ')})
                          </div>
                        </div>
                      </label>
                    )
                  })}
                </div>
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
              {tr('审批令牌已签发，有效期至', 'Approval token issued, expires at')}{' '}
              {new Date(approval.expiresAt * 1000).toLocaleTimeString()}。
              {tr('令牌一次性使用，执行前不会落盘明文。', ' Single-use; not stored in plaintext.')}
            </div>
          )}

          {(dryRunReport || execution) && (
            <div className="mt-4 space-y-2">
              <h4 className="text-xs font-semibold text-gray-300">
                {dryRunReport ? tr('Dry-run 结果', 'Dry-run result') : tr('执行结果', 'Execution result')}
              </h4>
              {(dryRunReport || execution)!.compiled_sql.map((sql) => (
                <div key={sql} className="break-all rounded border border-dark-border bg-dark-bg p-2 font-mono text-[11px] text-gray-300">
                  {sql}
                </div>
              ))}
              {(dryRunReport || execution)!.impact && (
                <div className="rounded border border-dark-border bg-dark-bg p-2 text-xs text-gray-400">
                  {tr('估算行数', 'Rows')}: {(dryRunReport || execution)!.impact!.estimated_rows ?? '-'} ·{' '}
                  {tr('估算索引体积', 'Est. index bytes')}: {(dryRunReport || execution)!.impact!.estimated_index_bytes ?? '-'}
                  <div className="mt-1">{(dryRunReport || execution)!.impact!.metadata_lock}</div>
                  <div>{(dryRunReport || execution)!.impact!.write_amplification}</div>
                </div>
              )}
              {(dryRunReport || execution)!.verification && (
                <div className="rounded border border-dark-border bg-dark-bg p-2 text-xs text-gray-300">
                  {tr('复测', 'Verification')}: {(dryRunReport || execution)!.verification!.summary}
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

      <div className="rounded-lg border border-dark-border bg-dark-panel p-4">
        <div className="mb-2 flex items-center justify-between">
          <h3 className="text-sm font-semibold text-white">{tr('最近审计', 'Recent audit')}</h3>
          <button
            onClick={() => void loadAudit()}
            className="text-xs text-blue-300 hover:text-blue-200"
          >
            {tr('刷新', 'Refresh')}
          </button>
        </div>
        {audit.length === 0 ? (
          <div className="text-xs text-gray-500">{tr('暂无审计记录', 'No audit records yet')}</div>
        ) : (
          <div className="max-h-56 space-y-1 overflow-y-auto font-mono text-[11px] text-gray-400">
            {audit.map((record) => (
              <div key={String(record.id)} className="flex items-start gap-2 rounded bg-dark-bg px-2 py-1">
                <span className="shrink-0 text-gray-600">
                  {new Date(Number(record.timestamp) * 1000).toLocaleTimeString()}
                </span>
                <span
                  className={
                    record.outcome === 'failed'
                      ? 'text-red-400'
                      : record.outcome === 'started'
                        ? 'text-amber-400'
                        : 'text-emerald-400'
                  }
                >
                  {String(record.event)}
                </span>
                <span className="min-w-0 truncate text-gray-500">
                  {record.plan_id ? `plan=${record.plan_id}` : ''}
                  {record.action_id ? ` action=${record.action_id}` : ''}
                </span>
              </div>
            ))}
          </div>
        )}
      </div>

      <div className="rounded-lg border border-dark-border bg-dark-panel p-4 text-xs text-gray-500">
        <div className="flex items-center gap-2 text-gray-300">
          <AlertTriangle className="h-4 w-4 text-amber-400" />
          {tr(
            '安全边界：DROP COLUMN/TABLE、TRUNCATE、类型变更、GRANT/REVOKE、SET GLOBAL 一律禁止自动执行；现有 execute_ddl 仅保留给人工入口。',
            'Safety: DROP COLUMN/TABLE, TRUNCATE, type changes, GRANT/REVOKE and SET GLOBAL are never auto-executed.'
          )}
        </div>
        {scenarios.length > 0 && (
          <div className="mt-2 flex flex-wrap gap-2">
            {scenarios.map((scenario) => (
              <span
                key={scenario.id}
                className={`rounded border px-2 py-1 ${
                  scenario.status === 'enabled'
                    ? 'border-emerald-500/40 text-emerald-300'
                    : 'border-dark-border text-gray-500'
                }`}
                title={scenario.description}
              >
                {scenario.title} · {scenario.status}
              </span>
            ))}
          </div>
        )}
      </div>
    </div>
  )
}
