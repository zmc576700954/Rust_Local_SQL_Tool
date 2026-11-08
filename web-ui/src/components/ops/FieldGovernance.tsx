import { useEffect, useMemo, useState } from 'react'
import {
  AlertTriangle,
  CheckCircle2,
  ChevronDown,
  ChevronUp,
  ClipboardCopy,
  Database,
  FileCode2,
  Loader2,
  Lock,
  Pause,
  Play,
  Plus,
  RefreshCw,
  ShieldAlert,
  Square,
  Trash2,
} from 'lucide-react'
import { api } from '../../api'
import type {
  EmptyStringPolicy,
  FieldBackfillOptionsPayload,
  FieldDiffPage,
  FieldMigrationJob,
  FieldTargetColumn,
} from '../../api'
import { tr } from '../../i18n'
import type { SchemaResponse, TableWithDetails } from '../../types'

interface FieldGovernanceProps {
  dbId?: string
}

type RiskLevel = 'info' | 'high' | 'block'

interface FieldRisk {
  id: string
  code: string
  level: RiskLevel
  title: string
  detail: string
  evidence_refs?: string[]
  blocks_write: boolean
  requires_user_confirmation: boolean
}

interface RiskSummary {
  block: number
  high: number
  info: number
  undetermined_write_block: boolean
  risk_floor: string
}

interface EvidenceItem {
  id: string
  title: string
  status: string
  mode: string
  summary: string
  sql?: string[]
  details?: unknown
}

interface EvidenceGap {
  evidence_id: string
  reason: string
}

interface ColumnProfile {
  column: string
  data_type: string
  column_type: string
  nullable: boolean
  column_key?: string | null
  profile_mode: string
  sample_per_mille?: number | null
  rows_profiled: number
  null_rows: number
  empty_rows: number
  max_char_length: number
  max_byte_length: number
  untrimmed_rows: number
  control_char_rows: number
  distinct_rows: number
  distinct_binary_rows: number
  json_valid_rows: number
  json_invalid_rows: number
  json_object_rows: number
  json_array_rows: number
  json_scalar_rows: number
  json_other_rows: number
  json_keys?: string[]
  json_key_presence?: Array<{ key: string; rows: number; ratio: number }>
  json_key_types?: Array<{ key: string; types: Record<string, number> }>
  json_profiled: boolean
  notes?: string[]
}

interface Declaration {
  id: string
  source: string
  verified: boolean
  column?: string | null
  purpose?: string | null
  expected_top_level?: string | null
  expected_keys?: string[]
  required_keys?: string[]
  consumers?: string[]
  unique_keys?: string[]
  uniqueness_expected?: boolean
  cutover_mode?: string | null
  retention_days?: number | null
}

interface DeclarationEvidence {
  declaration_id: string
  kind: string
  key_set?: string[]
  rows: number
  detail?: string
}

interface ManualScript {
  id: string
  title: string
  kind: string
  phase: string
  sql: string
  executable: boolean
  irreversible: boolean
  notes?: string[]
}

interface DryRunCheck {
  script_id: string
  check: string
  status: string
  detail: string
}

interface DryRunReport {
  executed_against_database: boolean
  passed: boolean
  checks: DryRunCheck[]
  note: string
}

interface DependencyEvidence {
  primary_key_columns?: string[]
  single_column_pk?: string | null
  indexes?: Array<{ name: string; unique: boolean; primary: boolean; index_type: string; columns: string[] }>
  foreign_keys?: Array<{ constraint_name: string; direction: string; table: string; column: string }>
  generated_columns?: Array<{ name: string; expression: string; extra: string }>
  objects?: Array<{ kind: string; name: string; matched_columns: string[] }>
  source_triggers?: string[]
  scan_complete: boolean
  gaps?: string[]
}

interface EnvironmentEvidence {
  version?: string | null
  version_comment?: string | null
  engine?: string | null
  table_collation?: string | null
  partitioned: boolean
  read_only: boolean
  long_transactions: number
  version_supports_json: boolean
}

interface RetireChecklistItem {
  id: string
  category: string
  title: string
  detail: string
  auto_verified: boolean
  status: string
  blocks_retire: boolean
  evidence_refs?: string[]
}

interface RetirePackage {
  generated: boolean
  table: string
  column?: string | null
  target_table: string
  snapshot_table?: string | null
  observation_required_days: number
  observation_elapsed_days?: number | null
  observation_satisfied: boolean
  observation_note: string
  snapshot_retention_days: number
  rollback_window_hours: number
  blockers?: string[]
  auto_verified_count: number
  manual_confirmation_count: number
  checklist: RetireChecklistItem[]
  execution_order?: string[]
  recommended_window: string
  irreversible: boolean
  note: string
}

interface RollbackAssessment {
  shape: string
  conclusion: string
  summary: string
  reasons?: string[]
  prerequisites?: string[]
  lossy_aspects?: string[]
  irreversible: boolean
  reverse_sql?: string | null
  note: string
}

interface FieldGovernanceAnalysis {
  db_name: string
  table: string
  column?: string | null
  analyzed_columns: string[]
  profile_mode: string
  sample_per_mille?: number | null
  estimated_rows?: number | null
  data_length?: number | null
  index_length?: number | null
  row_count: number
  columns: ColumnProfile[]
  dependencies: DependencyEvidence
  environment: EnvironmentEvidence
  evidence: EvidenceItem[]
  gaps: EvidenceGap[]
  declarations?: Declaration[]
  declaration_hash: string
  declaration_evidence?: DeclarationEvidence[]
  target_table: string
  target_exists: boolean
  empty_string_policy?: EmptyStringPolicy
  target_columns?: FieldTargetColumn[]
  risks: FieldRisk[]
  risk_summary: RiskSummary
  scripts: ManualScript[]
  dry_run: DryRunReport
  retire_package?: RetirePackage
  rollback?: RollbackAssessment
}

interface AnalyzeResponse {
  plan: { plan_id: string; plan_hash: string; diagnosis: string }
  evidence: { db_name: string; table: string; extra?: FieldGovernanceAnalysis | null }
}

interface MappingDraft {
  source_key: string
  target_column: string
  target_type: string
  required: boolean
}

interface DeclarationDraft {
  purpose: string
  expected_top_level: string
  expected_keys: string
  required_keys: string
  consumers: string
  unique_keys: string
  uniqueness_expected: boolean
  target_nullable: 'inherit' | 'nullable' | 'not_null'
  empty_string_means: string
  cutover_mode: string
  retention_days: string
}

function emptyDeclaration(): DeclarationDraft {
  return {
    purpose: '',
    expected_top_level: '',
    expected_keys: '',
    required_keys: '',
    consumers: '',
    unique_keys: '',
    uniqueness_expected: false,
    target_nullable: 'inherit',
    empty_string_means: '',
    cutover_mode: '',
    retention_days: '',
  }
}

function splitCsv(value: string): string[] {
  return value
    .split(/[,，\n]/)
    .map((item) => item.trim())
    .filter(Boolean)
}

function renderChecklistGroup(title: string, items: RetireChecklistItem[]) {
  return (
    <div className="rounded border border-dark-border bg-dark-bg p-3 text-xs">
      <div className="mb-2 text-gray-400">
        {title}（{items.length}）
      </div>
      {items.length === 0 ? (
        <div className="text-gray-500">—</div>
      ) : (
        <ul className="space-y-1">
          {items.map((item) => (
            <li key={item.id}>
              <div className="flex flex-wrap items-center gap-1">
                <span
                  className={
                    item.status === 'pass'
                      ? 'text-emerald-400'
                      : item.status === 'fail'
                        ? 'text-red-400'
                        : item.status === 'warn'
                          ? 'text-amber-400'
                          : 'text-gray-400'
                  }
                >
                  [{item.status}]
                </span>
                <span className="font-semibold text-gray-200">{item.title}</span>
                {item.blocks_retire && item.status !== 'pass' && (
                  <span className="rounded border border-red-500/50 px-1 text-[10px] text-red-300">
                    {tr('阻断下线', 'blocks retire')}
                  </span>
                )}
              </div>
              <div className="mt-0.5 text-gray-500">{item.detail}</div>
            </li>
          ))}
        </ul>
      )}
    </div>
  )
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

function riskLevelClass(level: RiskLevel): string {
  if (level === 'block') return 'border-red-500/60 bg-red-500/10 text-red-300'
  if (level === 'high') return 'border-amber-500/50 bg-amber-500/10 text-amber-300'
  return 'border-sky-500/40 bg-sky-500/10 text-sky-300'
}

function riskLevelLabel(level: RiskLevel): string {
  if (level === 'block') return tr('阻断', 'BLOCK')
  if (level === 'high') return tr('高风险', 'HIGH')
  return tr('提示', 'INFO')
}

function migrationStatusClass(status: FieldMigrationJob['status']): string {
  if (status === 'completed') return 'border-emerald-500/50 bg-emerald-500/10 text-emerald-300'
  if (status === 'blocked' || status === 'aborted') return 'border-red-500/50 bg-red-500/10 text-red-300'
  if (status === 'interrupted' || status === 'paused') return 'border-amber-500/50 bg-amber-500/10 text-amber-300'
  if (status === 'running') return 'border-blue-500/50 bg-blue-500/10 text-blue-300'
  return 'border-dark-border bg-dark-bg text-gray-300'
}

function migrationStatusLabel(status: FieldMigrationJob['status']): string {
  const labels: Record<FieldMigrationJob['status'], [string, string]> = {
    pending: ['待启动', 'PENDING'],
    running: ['运行中', 'RUNNING'],
    paused: ['已暂停', 'PAUSED'],
    interrupted: ['已中断（可续跑）', 'INTERRUPTED'],
    blocked: ['已阻断', 'BLOCKED'],
    completed: ['已完成', 'COMPLETED'],
    aborted: ['已中止', 'ABORTED'],
  }
  const pair = labels[status]
  return pair ? tr(pair[0], pair[1]) : status
}

function evidenceStatusClass(status: string): string {
  if (status === 'collected') return 'text-emerald-400'
  if (status === 'partial') return 'text-amber-400'
  return 'text-red-400'
}

async function copyText(text: string): Promise<boolean> {
  if (!text) return false
  try {
    if (typeof navigator !== 'undefined' && navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(text)
      return true
    }
  } catch {
    // fall through to the legacy path
  }
  return false
}

export function FieldGovernance({ dbId }: FieldGovernanceProps) {
  const [schema, setSchema] = useState<SchemaResponse | null>(null)
  const [schemaError, setSchemaError] = useState('')
  const [loadingSchema, setLoadingSchema] = useState(false)

  const [table, setTable] = useState('')
  const [column, setColumn] = useState('')
  const [profileMode, setProfileMode] = useState<'auto' | 'full' | 'sampled'>('auto')
  const [samplePerMille, setSamplePerMille] = useState('100')
  const [emptyStringPolicy, setEmptyStringPolicy] = useState<EmptyStringPolicy>('preserve')
  const [observationStartedAt, setObservationStartedAt] = useState('')

  const [targetTable, setTargetTable] = useState('')
  const [pkColumn, setPkColumn] = useState('')
  const [targetCollation, setTargetCollation] = useState('')
  const [residualJson, setResidualJson] = useState(false)
  const [mappings, setMappings] = useState<MappingDraft[]>([])
  const [declarations, setDeclarations] = useState<DeclarationDraft[]>([emptyDeclaration()])

  const [analysis, setAnalysis] = useState<FieldGovernanceAnalysis | null>(null)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState('')
  const [notice, setNotice] = useState('')
  const [expandedEvidence, setExpandedEvidence] = useState<string | null>(null)
  const [copiedScript, setCopiedScript] = useState('')

  const [diffPhase, setDiffPhase] = useState<'v2' | 'v3' | 'v4'>('v4')
  const [diffPages, setDiffPages] = useState<FieldDiffPage[]>([])
  const [diffIndex, setDiffIndex] = useState(0)
  const [diffBusy, setDiffBusy] = useState(false)
  const [diffError, setDiffError] = useState('')
  const [diffCopied, setDiffCopied] = useState('')

  const [planId, setPlanId] = useState('')
  const [planHash, setPlanHash] = useState('')
  const [backfillBatchSize, setBackfillBatchSize] = useState('1000')
  const [backfillIntervalMs, setBackfillIntervalMs] = useState('0')
  const [backfillMaxRps, setBackfillMaxRps] = useState('0')
  const [backfillProdConfirmed, setBackfillProdConfirmed] = useState(false)
  const [backfillConfirmedRisks, setBackfillConfirmedRisks] = useState<string[]>([])
  const [backfillJob, setBackfillJob] = useState<FieldMigrationJob | null>(null)
  const [backfillBusy, setBackfillBusy] = useState(false)
  const [backfillError, setBackfillError] = useState('')
  const [backfillNotice, setBackfillNotice] = useState('')

  useEffect(() => {
    let cancelled = false
    if (!dbId) {
      setSchema(null)
      return
    }
    setLoadingSchema(true)
    setSchemaError('')
    api
      .getSchema(dbId)
      .then((resp: SchemaResponse) => {
        if (cancelled) return
        setSchema(resp)
      })
      .catch((err: unknown) => {
        if (cancelled) return
        setSchema(null)
        setSchemaError(describeError(err))
      })
      .finally(() => {
        if (!cancelled) setLoadingSchema(false)
      })
    return () => {
      cancelled = true
    }
  }, [dbId])

  useEffect(() => {
    const jobId = backfillJob?.job_id
    const status = backfillJob?.status
    if (!jobId || (status !== 'running' && status !== 'pending')) return
    const timer = window.setInterval(() => {
      api
        .opsFieldBackfillStatus(jobId)
        .then(setBackfillJob)
        .catch(() => {
          // 轮询失败静默，下一次继续
        })
    }, 2000)
    return () => window.clearInterval(timer)
  }, [backfillJob?.job_id, backfillJob?.status])

  const currentTable: TableWithDetails | undefined = useMemo(
    () => schema?.tables?.find((item) => item.table_name === table),
    [schema, table]
  )

  const tableOptions = useMemo(
    () => (schema?.tables || []).map((item) => item.table_name),
    [schema]
  )

  const columnOptions = useMemo(
    () => (currentTable?.columns || []).map((item) => item.column_name),
    [currentTable]
  )

  useEffect(() => {
    setColumn('')
    setMappings([])
  }, [table])

  const handleAnalyze = async () => {
    if (!table.trim()) {
      setError(tr('请先选择源表', 'Please select a source table'))
      return
    }
    setBusy(true)
    setError('')
    setNotice('')
    try {
      const payloadDeclarations = declarations
        .filter((item) => item.purpose.trim() || item.required_keys.trim() || item.expected_keys.trim())
        .map((item) => {
          const declaration: Record<string, unknown> = {}
          if (item.purpose.trim()) declaration.purpose = item.purpose.trim()
          if (item.expected_top_level.trim()) declaration.expected_top_level = item.expected_top_level.trim()
          const expectedKeys = splitCsv(item.expected_keys)
          if (expectedKeys.length > 0) declaration.expected_keys = expectedKeys
          const requiredKeys = splitCsv(item.required_keys)
          if (requiredKeys.length > 0) declaration.required_keys = requiredKeys
          const consumers = splitCsv(item.consumers)
          if (consumers.length > 0) declaration.consumers = consumers
          const uniqueKeys = splitCsv(item.unique_keys)
          if (uniqueKeys.length > 0) declaration.unique_keys = uniqueKeys
          if (item.uniqueness_expected) declaration.uniqueness_expected = true
          if (item.target_nullable !== 'inherit') {
            declaration.target_nullable = item.target_nullable === 'nullable'
          }
          if (item.empty_string_means.trim()) declaration.empty_string_means = item.empty_string_means.trim()
          if (item.cutover_mode.trim()) declaration.cutover_mode = item.cutover_mode.trim()
          const retention = Number(item.retention_days)
          if (Number.isFinite(retention) && retention > 0) declaration.retention_days = retention
          if (column.trim()) declaration.column = column.trim()
          return declaration
        })

      const targetMappings = mappings
        .filter((item) => item.target_column.trim())
        .map((item) => ({
          source_key: item.source_key.trim() || undefined,
          target_column: item.target_column.trim(),
          target_type: item.target_type.trim() || undefined,
          required: item.required || undefined,
        }))

      const target: Record<string, unknown> = {}
      if (targetTable.trim()) target.table = targetTable.trim()
      if (pkColumn.trim()) target.pk_column = pkColumn.trim()
      if (targetCollation.trim()) target.collation = targetCollation.trim()
      target.residual_json = residualJson
      if (targetMappings.length > 0) target.mappings = targetMappings

      const resp: AnalyzeResponse = await api.opsAnalyze({
        scenario_id: 'field-governance',
        db_id: dbId,
        table: table.trim(),
        column: column.trim() || undefined,
        target,
        declarations: payloadDeclarations,
        profile_mode: profileMode,
        sample_per_mille: profileMode === 'sampled' ? Number(samplePerMille) || undefined : undefined,
        empty_string_policy: emptyStringPolicy,
        observation_started_at: observationStartedAt.trim() || undefined,
      })
      const extra = resp?.evidence?.extra || null
      if (!extra) {
        setError(tr('后端未返回字段治理证据', 'Backend returned no field-governance evidence'))
        return
      }
      setAnalysis(extra)
      setDiffPages([])
      setDiffIndex(0)
      setDiffError('')
      setPlanId(resp.plan.plan_id)
      setPlanHash(resp.plan.plan_hash)
      setBackfillJob(null)
      setBackfillError('')
      setBackfillNotice('')
      setBackfillConfirmedRisks([])
      api
        .opsFieldBackfillList({ plan_id: resp.plan.plan_id, limit: 1 })
        .then((data) => {
          if (data?.jobs?.length) setBackfillJob(data.jobs[0])
        })
        .catch(() => {
          // 历史任务读取失败不阻断分析结果展示
        })
      setNotice(
        tr(
          `分析完成：阻断 ${extra.risk_summary.block} / 高风险 ${extra.risk_summary.high} / 提示 ${extra.risk_summary.info}`,
          `Analysis complete: block ${extra.risk_summary.block} / high ${extra.risk_summary.high} / info ${extra.risk_summary.info}`
        )
      )
    } catch (err) {
      setError(describeError(err))
    } finally {
      setBusy(false)
    }
  }

  const handleCopy = async (script: ManualScript) => {
    const ok = await copyText(script.sql)
    if (ok) {
      setCopiedScript(script.id)
      window.setTimeout(() => setCopiedScript(''), 2000)
    } else {
      setError(tr('复制失败，请手动选择脚本内容', 'Copy failed; select the script manually'))
    }
  }

  const addMapping = () => {
    setMappings((prev) => [...prev, { source_key: '', target_column: '', target_type: '', required: false }])
  }

  const updateMapping = (index: number, patch: Partial<MappingDraft>) => {
    setMappings((prev) => prev.map((item, i) => (i === index ? { ...item, ...patch } : item)))
  }

  const addDeclaration = () => setDeclarations((prev) => [...prev, emptyDeclaration()])

  const updateDeclaration = (index: number, patch: Partial<DeclarationDraft>) => {
    setDeclarations((prev) => prev.map((item, i) => (i === index ? { ...item, ...patch } : item)))
  }

  const buildDiffRequest = (
    phase: 'v2' | 'v3' | 'v4',
    lastPk?: string | null,
    pageSize = 100,
  ) => {
    if (!analysis?.column) return null
    return {
      db_id: dbId,
      table: analysis.table,
      column: analysis.column,
      target_table: analysis.target_table,
      pk_column: pkColumn.trim() || analysis.dependencies.single_column_pk || undefined,
      target_columns: analysis.target_columns || [],
      empty_string_policy: analysis.empty_string_policy || 'preserve',
      phase,
      last_pk: lastPk || null,
      page_size: pageSize,
    }
  }

  const loadFirstDiffPage = async () => {
    const request = buildDiffRequest(diffPhase)
    if (!request) {
      setDiffError(tr('请先选择源列并完成分析', 'Select a source column and run analysis first'))
      return
    }
    setDiffBusy(true)
    setDiffError('')
    setDiffCopied('')
    try {
      const page = await api.opsFieldDiffPage(request)
      setDiffPages([page])
      setDiffIndex(0)
    } catch (err) {
      setDiffError(describeError(err))
    } finally {
      setDiffBusy(false)
    }
  }

  const loadNextDiffPage = async () => {
    const current = diffPages[diffIndex]
    if (!current?.has_more || !current.next_last_pk) return
    const request = buildDiffRequest(diffPhase, current.next_last_pk)
    if (!request) return
    setDiffBusy(true)
    setDiffError('')
    try {
      const page = await api.opsFieldDiffPage(request)
      setDiffPages((prev) => [...prev.slice(0, diffIndex + 1), page])
      setDiffIndex((index) => index + 1)
    } catch (err) {
      setDiffError(describeError(err))
    } finally {
      setDiffBusy(false)
    }
  }

  const copyDiffPks = async (pks: string[], label: string) => {
    const ok = await copyText(pks.join('\n'))
    if (!ok) {
      setDiffError(tr('复制失败，请手动选择主键', 'Copy failed; select the primary keys manually'))
      return
    }
    setDiffCopied(label)
    window.setTimeout(() => setDiffCopied(''), 2000)
  }

  const copyAllDiffPks = async () => {
    if (!analysis?.column) return
    setDiffBusy(true)
    setDiffError('')
    setDiffCopied('')
    try {
      const all: string[] = []
      let lastPk: string | null = null
      for (let page = 0; ; page += 1) {
        if (page >= 10_000) {
          throw new Error(
            tr(
              '差异主键超过 1000 万条，已中止复制；请缩小范围',
              'More than 10 million diff PKs; copy aborted, narrow the scope'
            )
          )
        }
        const request = buildDiffRequest(diffPhase, lastPk, 1000)
        if (!request) return
        const result = await api.opsFieldDiffPage(request)
        all.push(...result.pks)
        if (!result.has_more || !result.next_last_pk) break
        lastPk = result.next_last_pk
      }
      await copyDiffPks(all, 'all')
    } catch (err) {
      setDiffError(describeError(err))
    } finally {
      setDiffBusy(false)
    }
  }

  const backfillOptions = (): FieldBackfillOptionsPayload => ({
    batch_size: Number(backfillBatchSize) || undefined,
    batch_interval_ms: Number(backfillIntervalMs) || 0,
    max_rows_per_second: Number(backfillMaxRps) || 0,
    production_confirmed: backfillProdConfirmed || undefined,
    confirmed_high_risks: backfillConfirmedRisks,
  })

  const handleBackfillApprove = async () => {
    if (!planId) {
      setBackfillError(tr('请先完成只读分析，获取计划 ID', 'Run the read-only analysis first'))
      return
    }
    setBackfillBusy(true)
    setBackfillError('')
    setBackfillNotice('')
    try {
      const approval = await api.opsFieldBackfillApprove({
        plan_id: planId,
        plan_hash: planHash || undefined,
        ...backfillOptions(),
      })
      setBackfillNotice(
        tr(
          `审批已签发：${approval.approval_id}（到期 ${new Date(approval.expires_at * 1000).toLocaleString()}）`,
          `Approval issued: ${approval.approval_id} (expires ${new Date(approval.expires_at * 1000).toLocaleString()})`
        )
      )
    } catch (err) {
      setBackfillError(describeError(err))
    } finally {
      setBackfillBusy(false)
    }
  }

  const handleBackfillStart = async () => {
    if (!planId) {
      setBackfillError(tr('请先完成只读分析，获取计划 ID', 'Run the read-only analysis first'))
      return
    }
    setBackfillBusy(true)
    setBackfillError('')
    setBackfillNotice('')
    try {
      const options = backfillOptions()
      const approval = await api.opsFieldBackfillApprove({
        plan_id: planId,
        plan_hash: planHash || undefined,
        ...options,
      })
      const job = await api.opsFieldBackfillStart({
        plan_id: planId,
        approval_token: approval.token,
        plan_hash: planHash || undefined,
        ...options,
      })
      setBackfillJob(job)
      setBackfillNotice(
        tr(`回填任务已启动：${job.job_id}`, `Backfill job started: ${job.job_id}`)
      )
    } catch (err) {
      setBackfillError(describeError(err))
    } finally {
      setBackfillBusy(false)
    }
  }

  const handleBackfillRefresh = async () => {
    if (!backfillJob) return
    setBackfillError('')
    try {
      const job = await api.opsFieldBackfillStatus(backfillJob.job_id)
      setBackfillJob(job)
    } catch (err) {
      setBackfillError(describeError(err))
    }
  }

  const handleBackfillPause = async () => {
    if (!backfillJob) return
    setBackfillBusy(true)
    setBackfillError('')
    try {
      setBackfillJob(await api.opsFieldBackfillPause(backfillJob.job_id))
      setBackfillNotice(tr('已请求暂停：将在当前批次结束后生效', 'Pause requested: takes effect after the current batch'))
    } catch (err) {
      setBackfillError(describeError(err))
    } finally {
      setBackfillBusy(false)
    }
  }

  const handleBackfillResume = async () => {
    if (!backfillJob) return
    setBackfillBusy(true)
    setBackfillError('')
    setBackfillNotice('')
    try {
      const needsNewToken =
        backfillJob.status === 'interrupted' || backfillJob.status === 'blocked'
      let token: string | undefined
      if (needsNewToken) {
        // 按既有任务签发续跑令牌：不依赖内存中的分析证据，服务重启后也可用；
        // 批参数直接沿用任务保存的 config。
        const approval = await api.opsFieldBackfillApprove({ job_id: backfillJob.job_id })
        token = approval.token
      }
      const job = await api.opsFieldBackfillResume(backfillJob.job_id, token)
      setBackfillJob(job)
      setBackfillNotice(
        tr(
          `已从 ${job.progress.resume_after_pk ?? '起点'} 续跑（第 ${job.progress.current_batch} 批）`,
          `Resumed after ${job.progress.resume_after_pk ?? 'start'} (batch ${job.progress.current_batch})`
        )
      )
    } catch (err) {
      setBackfillError(describeError(err))
    } finally {
      setBackfillBusy(false)
    }
  }

  const handleBackfillAbort = async () => {
    if (!backfillJob || !window.confirm(tr('确认中止回填？已写入的批次不会回滚。', 'Abort the backfill? Batches already written are kept.'))) {
      return
    }
    setBackfillBusy(true)
    setBackfillError('')
    try {
      setBackfillJob(await api.opsFieldBackfillAbort(backfillJob.job_id))
      setBackfillNotice(tr('回填已中止', 'Backfill aborted'))
    } catch (err) {
      setBackfillError(describeError(err))
    } finally {
      setBackfillBusy(false)
    }
  }

  const copyBlockingPks = async () => {
    const pks = backfillJob?.blocking?.pks || []
    if (pks.length === 0) return
    const ok = await copyText(pks.join('\n'))
    if (!ok) {
      setBackfillError(tr('复制失败，请手动选择主键', 'Copy failed; select the primary keys manually'))
      return
    }
    setBackfillNotice(tr('差异主键已复制', 'Diff PKs copied'))
    window.setTimeout(() => setBackfillNotice(''), 2000)
  }

  const blockRisks = (analysis?.risks || []).filter((risk) => risk.level === 'block')
  const highRisks = (analysis?.risks || []).filter((risk) => risk.level === 'high')
  const infoRisks = (analysis?.risks || []).filter((risk) => risk.level === 'info')
  const irreversibleScripts = (analysis?.scripts || []).filter((script) => script.irreversible)
  const currentDiffPage = diffPages[diffIndex]
  const migrationActive =
    backfillJob != null &&
    (backfillJob.status === 'running' || backfillJob.status === 'paused' || backfillJob.status === 'pending')
  const backfillResumable =
    backfillJob != null &&
    (backfillJob.status === 'interrupted' ||
      backfillJob.status === 'paused' ||
      backfillJob.status === 'blocked')

  return (
    <div className="flex flex-col gap-4">
      <div className="rounded-lg border border-dark-border bg-dark-panel p-4">
        <div className="flex flex-wrap items-center justify-between gap-3">
          <div>
            <h2 className="text-lg font-semibold text-white">
              {tr('字段治理 · 只读分析', 'Field governance · read-only analysis')}
            </h2>
            <p className="mt-1 text-xs text-gray-400">
              {tr(
                '采集 E01-E14 证据 → 三档风险（阻断 / 高风险 / 提示）→ dry-run 脚本包。本页只生成脚本，不执行任何写库操作。',
                'Collect E01-E14 evidence → three-tier risk (block / high / info) → dry-run script pack. This page only generates scripts and never writes to the database.'
              )}
            </p>
          </div>
          <span className="flex items-center gap-1 rounded border border-emerald-500/40 px-2 py-1 text-xs text-emerald-300">
            <Lock className="h-3.5 w-3.5" />
            {tr('只读 + dry-run', 'Read-only + dry-run')}
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
        <div className="flex items-center justify-between gap-2">
          <h3 className="text-sm font-semibold text-white">{tr('源表与目标', 'Source and target')}</h3>
          <button
            onClick={() => {
              if (!dbId) return
              setLoadingSchema(true)
              api
                .getSchema(dbId)
                .then(setSchema)
                .catch((err: unknown) => setSchemaError(describeError(err)))
                .finally(() => setLoadingSchema(false))
            }}
            className="flex items-center gap-1 rounded border border-dark-border px-2 py-1 text-xs text-gray-400 hover:bg-dark-bg"
          >
            {loadingSchema ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <RefreshCw className="h-3.5 w-3.5" />}
            {tr('刷新表结构', 'Refresh schema')}
          </button>
        </div>

        {schemaError && <div className="mt-2 text-xs text-red-400">{schemaError}</div>}

        <div className="mt-3 grid gap-3 md:grid-cols-2">
          <div className="flex flex-col gap-1">
            <label className="text-xs text-gray-400">{tr('源表', 'Source table')}</label>
            <select
              value={table}
              onChange={(event) => setTable(event.target.value)}
              className="h-9 rounded border border-dark-border bg-dark-bg px-2 font-mono text-sm text-white outline-none focus:border-blue-500"
            >
              <option value="">{tr('请选择表', 'Select a table')}</option>
              {tableOptions.map((name) => (
                <option key={name} value={name}>
                  {name}
                </option>
              ))}
            </select>
          </div>
          <div className="flex flex-col gap-1">
            <label className="text-xs text-gray-400">{tr('源列（留空 = 整表盘点）', 'Source column (empty = whole-table inventory)')}</label>
            <select
              value={column}
              onChange={(event) => setColumn(event.target.value)}
              className="h-9 rounded border border-dark-border bg-dark-bg px-2 font-mono text-sm text-white outline-none focus:border-blue-500"
            >
              <option value="">{tr('整表字段盘点', 'Whole-table inventory')}</option>
              {columnOptions.map((name) => (
                <option key={name} value={name}>
                  {name}
                </option>
              ))}
            </select>
          </div>
          <div className="flex flex-col gap-1">
            <label className="text-xs text-gray-400">{tr('目标表（留空自动推断）', 'Target table (empty = inferred)')}</label>
            <input
              value={targetTable}
              onChange={(event) => setTargetTable(event.target.value)}
              placeholder="orders_payload_ext"
              className="h-9 rounded border border-dark-border bg-dark-bg px-2 font-mono text-sm text-white outline-none focus:border-blue-500"
            />
          </div>
          <div className="flex flex-col gap-1">
            <label className="text-xs text-gray-400">{tr('目标主键列（留空沿用源表）', 'Target PK column (empty = source)')}</label>
            <input
              value={pkColumn}
              onChange={(event) => setPkColumn(event.target.value)}
              placeholder="id"
              className="h-9 rounded border border-dark-border bg-dark-bg px-2 font-mono text-sm text-white outline-none focus:border-blue-500"
            />
          </div>
        </div>

        <div className="mt-3 flex flex-wrap items-center gap-4">
          <div className="flex items-center gap-2">
            <label className="text-xs text-gray-400">{tr('剖面模式', 'Profile mode')}</label>
            <select
              value={profileMode}
              onChange={(event) => setProfileMode(event.target.value as 'auto' | 'full' | 'sampled')}
              className="h-8 rounded border border-dark-border bg-dark-bg px-2 text-xs text-white outline-none"
            >
              <option value="auto">auto</option>
              <option value="full">full</option>
              <option value="sampled">sampled</option>
            </select>
          </div>
          {profileMode === 'sampled' && (
            <div className="flex items-center gap-2">
              <label className="text-xs text-gray-400">{tr('抽样千分比', 'Sample per mille')}</label>
              <input
                value={samplePerMille}
                onChange={(event) => setSamplePerMille(event.target.value)}
                inputMode="numeric"
                className="h-8 w-20 rounded border border-dark-border bg-dark-bg px-2 text-xs text-white outline-none"
              />
            </div>
          )}
          <div className="flex flex-col gap-1">
            <div className="flex items-center gap-2">
              <label className="text-xs text-gray-400">{tr('空串策略', 'Empty string policy')}</label>
              <select
                value={emptyStringPolicy}
                onChange={(event) => setEmptyStringPolicy(event.target.value as EmptyStringPolicy)}
                className="h-8 rounded border border-dark-border bg-dark-bg px-2 text-xs text-white outline-none"
              >
                <option value="preserve">{tr('保留空串（默认）', 'Preserve (default)')}</option>
                <option value="to_null">{tr('空串转 NULL（高风险）', 'Convert to NULL (high risk)')}</option>
                <option value="block">{tr('命中空串即阻断', 'Block on empty string')}</option>
              </select>
            </div>
            <span className="text-[11px] text-gray-500">
              {tr('空串与 SQL NULL 严格区分，不会被静默合并。', 'Empty string and SQL NULL are kept distinct; no silent conversion.')}
            </span>
          </div>
          <div className="flex flex-col gap-1">
            <label className="text-xs text-gray-400">{tr('观察期起始日期（S3 下线清单）', 'Observation start date (S3 retire checklist)')}</label>
            <input
              type="date"
              value={observationStartedAt}
              onChange={(event) => setObservationStartedAt(event.target.value)}
              className="h-8 w-40 rounded border border-dark-border bg-dark-bg px-2 text-xs text-white outline-none"
            />
            <span className="text-[11px] text-gray-500">
              {tr('未填写时清单标记为必须人工确认；未满观察期只提示，不代替用户决定。', 'If empty, the checklist marks it as manual confirmation; a short observation period is only a warning.')}
            </span>
          </div>
          <label className="flex items-center gap-2 text-xs text-gray-400">
            <input type="checkbox" checked={residualJson} onChange={(event) => setResidualJson(event.target.checked)} />
            {tr('允许 residual JSON 兜底', 'Allow residual JSON column')}
          </label>
          <input
            value={targetCollation}
            onChange={(event) => setTargetCollation(event.target.value)}
            placeholder={tr('目标排序规则（可选）', 'Target collation (optional)')}
            className="h-8 w-56 rounded border border-dark-border bg-dark-bg px-2 font-mono text-xs text-white outline-none"
          />
        </div>

        <div className="mt-4 rounded border border-dark-border bg-dark-bg p-3">
          <div className="flex items-center justify-between">
            <h4 className="text-xs font-semibold text-gray-300">{tr('目标列映射', 'Target column mappings')}</h4>
            <button
              onClick={addMapping}
              className="flex items-center gap-1 rounded border border-dark-border px-2 py-1 text-xs text-gray-400 hover:bg-dark-panel"
            >
              <Plus className="h-3.5 w-3.5" />
              {tr('添加映射', 'Add mapping')}
            </button>
          </div>
          {mappings.length === 0 ? (
            <p className="mt-2 text-xs text-gray-500">
              {tr('未配置映射时，后端按 JSON key 推断目标列草案。', 'Without mappings the backend infers target columns from JSON keys.')}
            </p>
          ) : (
            <div className="mt-2 flex flex-col gap-2">
              {mappings.map((mapping, index) => (
                <div key={index} className="grid gap-2 md:grid-cols-[1fr_1fr_1fr_auto_auto]">
                  <input
                    value={mapping.source_key}
                    onChange={(event) => updateMapping(index, { source_key: event.target.value })}
                    placeholder={tr('源 JSON key', 'Source JSON key')}
                    className="h-8 rounded border border-dark-border bg-dark-panel px-2 font-mono text-xs text-white outline-none"
                  />
                  <input
                    value={mapping.target_column}
                    onChange={(event) => updateMapping(index, { target_column: event.target.value })}
                    placeholder={tr('目标列名', 'Target column')}
                    className="h-8 rounded border border-dark-border bg-dark-panel px-2 font-mono text-xs text-white outline-none"
                  />
                  <input
                    value={mapping.target_type}
                    onChange={(event) => updateMapping(index, { target_type: event.target.value })}
                    placeholder={tr('目标类型（可选）', 'Target type (optional)')}
                    className="h-8 rounded border border-dark-border bg-dark-panel px-2 font-mono text-xs text-white outline-none"
                  />
                  <label className="flex items-center gap-1 text-xs text-gray-400">
                    <input
                      type="checkbox"
                      checked={mapping.required}
                      onChange={(event) => updateMapping(index, { required: event.target.checked })}
                    />
                    {tr('必填', 'Required')}
                  </label>
                  <button
                    onClick={() => setMappings((prev) => prev.filter((_, i) => i !== index))}
                    className="flex items-center justify-center rounded border border-dark-border px-2 text-gray-400 hover:bg-dark-panel"
                    aria-label={tr('删除映射', 'Remove mapping')}
                  >
                    <Trash2 className="h-3.5 w-3.5" />
                  </button>
                </div>
              ))}
            </div>
          )}
        </div>

        <div className="mt-4 rounded border border-dark-border bg-dark-bg p-3">
          <div className="flex items-center justify-between">
            <div>
              <h4 className="text-xs font-semibold text-gray-300">{tr('语义描述', 'Semantic declarations')}</h4>
              <p className="mt-1 text-[11px] text-gray-500">
                {tr(
                  '语义描述作为「带 ID 的未验证声明」：只会追加约束，不会降低任何风险等级。',
                  'Declarations are unverified claims with IDs: they only add constraints and never lower a risk level.'
                )}
              </p>
            </div>
            <button
              onClick={addDeclaration}
              className="flex items-center gap-1 rounded border border-dark-border px-2 py-1 text-xs text-gray-400 hover:bg-dark-panel"
            >
              <Plus className="h-3.5 w-3.5" />
              {tr('添加声明', 'Add declaration')}
            </button>
          </div>
          <div className="mt-3 flex flex-col gap-3">
            {declarations.map((declaration, index) => (
              <div key={index} className="rounded border border-dark-border bg-dark-panel p-3">
                <div className="grid gap-2 md:grid-cols-2">
                  <input
                    value={declaration.purpose}
                    onChange={(event) => updateDeclaration(index, { purpose: event.target.value })}
                    placeholder={tr('用途（例如：只存用户偏好）', 'Purpose (e.g. user preferences only)')}
                    className="h-8 rounded border border-dark-border bg-dark-bg px-2 text-xs text-white outline-none md:col-span-2"
                  />
                  <input
                    value={declaration.expected_keys}
                    onChange={(event) => updateDeclaration(index, { expected_keys: event.target.value })}
                    placeholder={tr('预期 key（逗号分隔）', 'Expected keys (comma separated)')}
                    className="h-8 rounded border border-dark-border bg-dark-bg px-2 font-mono text-xs text-white outline-none"
                  />
                  <input
                    value={declaration.required_keys}
                    onChange={(event) => updateDeclaration(index, { required_keys: event.target.value })}
                    placeholder={tr('必填 key（逗号分隔）', 'Required keys (comma separated)')}
                    className="h-8 rounded border border-dark-border bg-dark-bg px-2 font-mono text-xs text-white outline-none"
                  />
                  <input
                    value={declaration.consumers}
                    onChange={(event) => updateDeclaration(index, { consumers: event.target.value })}
                    placeholder={tr('消费方（逗号分隔）', 'Consumers (comma separated)')}
                    className="h-8 rounded border border-dark-border bg-dark-bg px-2 text-xs text-white outline-none"
                  />
                  <input
                    value={declaration.unique_keys}
                    onChange={(event) => updateDeclaration(index, { unique_keys: event.target.value })}
                    placeholder={tr('唯一 key（逗号分隔）', 'Unique keys (comma separated)')}
                    className="h-8 rounded border border-dark-border bg-dark-bg px-2 font-mono text-xs text-white outline-none"
                  />
                  <select
                    value={declaration.target_nullable}
                    onChange={(event) =>
                      updateDeclaration(index, { target_nullable: event.target.value as DeclarationDraft['target_nullable'] })
                    }
                    className="h-8 rounded border border-dark-border bg-dark-bg px-2 text-xs text-white outline-none"
                  >
                    <option value="inherit">{tr('目标可空性：沿用', 'Nullability: inherit')}</option>
                    <option value="nullable">{tr('目标可空', 'Nullable')}</option>
                    <option value="not_null">{tr('目标 NOT NULL', 'Target NOT NULL')}</option>
                  </select>
                  <select
                    value={declaration.empty_string_means}
                    onChange={(event) => updateDeclaration(index, { empty_string_means: event.target.value })}
                    className="h-8 rounded border border-dark-border bg-dark-bg px-2 text-xs text-white outline-none"
                  >
                    <option value="">{tr('空串含义：未声明', 'Empty string: undeclared')}</option>
                    <option value="null">{tr('空串等价 NULL', 'Empty means NULL')}</option>
                    <option value="empty">{tr('空串是有效值', 'Empty is valid')}</option>
                    <option value="invalid">{tr('空串非法', 'Empty is invalid')}</option>
                  </select>
                  <select
                    value={declaration.cutover_mode}
                    onChange={(event) => updateDeclaration(index, { cutover_mode: event.target.value })}
                    className="h-8 rounded border border-dark-border bg-dark-bg px-2 text-xs text-white outline-none"
                  >
                    <option value="">{tr('切换方式：未声明', 'Cutover: undeclared')}</option>
                    <option value="stop_write">{tr('停写切换', 'Stop-write cutover')}</option>
                    <option value="dual_write">{tr('双写', 'Dual write')}</option>
                  </select>
                  <input
                    value={declaration.retention_days}
                    onChange={(event) => updateDeclaration(index, { retention_days: event.target.value })}
                    placeholder={tr('保留天数（可选）', 'Retention days (optional)')}
                    inputMode="numeric"
                    className="h-8 rounded border border-dark-border bg-dark-bg px-2 text-xs text-white outline-none"
                  />
                  <label className="flex items-center gap-2 text-xs text-gray-400">
                    <input
                      type="checkbox"
                      checked={declaration.uniqueness_expected}
                      onChange={(event) => updateDeclaration(index, { uniqueness_expected: event.target.checked })}
                    />
                    {tr('声明 key 唯一', 'Declare key uniqueness')}
                  </label>
                </div>
                <div className="mt-2 flex justify-end">
                  <button
                    onClick={() => setDeclarations((prev) => prev.filter((_, i) => i !== index))}
                    className="flex items-center gap-1 rounded border border-dark-border px-2 py-1 text-xs text-gray-400 hover:bg-dark-bg"
                  >
                    <Trash2 className="h-3.5 w-3.5" />
                    {tr('删除', 'Remove')}
                  </button>
                </div>
              </div>
            ))}
          </div>
        </div>

        <div className="mt-4">
          <button
            onClick={() => void handleAnalyze()}
            disabled={busy}
            className="flex h-9 items-center gap-2 rounded bg-blue-600 px-4 text-sm font-medium text-white hover:bg-blue-500 disabled:cursor-not-allowed disabled:opacity-50"
          >
            {busy ? <Loader2 className="h-4 w-4 animate-spin" /> : <Database className="h-4 w-4" />}
            {tr('采集证据并生成风险报告', 'Collect evidence & build risk report')}
          </button>
        </div>
      </div>

      {analysis && (
        <>
          {(blockRisks.length > 0 || analysis.risk_summary.undetermined_write_block) && (
            <div className="rounded-lg border-2 border-red-500/60 bg-red-500/10 p-4">
              <div className="flex items-center gap-2 text-sm font-semibold text-red-300">
                <ShieldAlert className="h-5 w-5" />
                {tr('存在阻断项：任何写步骤都必须停止', 'Blocking items found: all write steps must stop')}
              </div>
              <p className="mt-1 text-xs text-red-200/80">
                {tr(
                  '阻断项与不可判定项在 S1 中只做展示；后续阶段不得跳过这些阻断直接进入写库。',
                  'Blocking and undetermined items are display-only in S1; later stages must not skip them.'
                )}
              </p>
              {analysis.risk_summary.undetermined_write_block && (
                <div className="mt-2 rounded border border-red-500/50 bg-red-500/10 px-2 py-1 text-xs text-red-300">
                  {tr('不可判定 → 按阻断处理（证据缺口 / 抽样 / 依赖扫描不完整）', 'Undetermined → treated as blocking (evidence gaps / sampling / incomplete dependency scan)')}
                </div>
              )}
              {blockRisks.length > 0 && (
                <ul className="mt-2 space-y-1 text-xs text-red-200">
                  {blockRisks.map((risk) => (
                    <li key={risk.id}>
                      <span className="mr-2 rounded bg-red-500/20 px-1.5 py-0.5 font-mono text-red-300">{risk.code}</span>
                      {risk.title}：{risk.detail}
                    </li>
                  ))}
                </ul>
              )}
            </div>
          )}

          {irreversibleScripts.length > 0 && (
            <div className="rounded-lg border border-amber-500/50 bg-amber-500/10 p-3 text-xs text-amber-300">
              <div className="flex items-center gap-2 font-semibold">
                <AlertTriangle className="h-4 w-4" />
                {tr('不可逆步骤（执行前必须显式确认）', 'Irreversible steps (explicit confirmation required)')}
              </div>
              <ul className="mt-1 space-y-1">
                {irreversibleScripts.map((script) => (
                  <li key={script.id}>
                    {script.title}（{script.phase}）；{tr('本页不会执行', 'not executed on this page')}
                  </li>
                ))}
              </ul>
            </div>
          )}

          <div className="rounded-lg border border-dark-border bg-dark-panel p-4">
            <h3 className="mb-3 flex items-center gap-2 text-sm font-semibold text-white">
              <FileCode2 className="h-4 w-4 text-blue-400" />
              {tr('风险报告', 'Risk report')}
            </h3>
            <div className="grid gap-2 text-xs md:grid-cols-4">
              <div className="rounded border border-red-500/40 bg-red-500/10 p-2 text-red-300">
                {tr('阻断', 'Block')}: {analysis.risk_summary.block}
              </div>
              <div className="rounded border border-amber-500/40 bg-amber-500/10 p-2 text-amber-300">
                {tr('高风险', 'High')}: {analysis.risk_summary.high}
              </div>
              <div className="rounded border border-sky-500/40 bg-sky-500/10 p-2 text-sky-300">
                {tr('提示', 'Info')}: {analysis.risk_summary.info}
              </div>
              <div className="rounded border border-dark-border bg-dark-bg p-2 text-gray-300">
                {tr('风险下限', 'Risk floor')}: {analysis.risk_summary.risk_floor}
              </div>
            </div>
            <div className="mt-3 grid gap-3 md:grid-cols-2">
              {[blockRisks, highRisks, infoRisks].map((group, index) => (
                <div key={index} className="rounded border border-dark-border bg-dark-bg p-3 text-xs">
                  <div className="mb-2 text-gray-400">
                    {index === 0 ? tr('阻断', 'Block') : index === 1 ? tr('高风险', 'High') : tr('提示', 'Info')}（{group.length}）
                  </div>
                  {group.length === 0 ? (
                    <div className="text-gray-500">{tr('无', 'None')}</div>
                  ) : (
                    <ul className="space-y-1">
                      {group.map((risk) => (
                        <li key={risk.id} className={`rounded border px-2 py-1 ${riskLevelClass(risk.level)}`}>
                          <div className="flex flex-wrap items-center gap-1">
                            <span className="rounded bg-black/20 px-1 font-mono">{risk.code}</span>
                            <span className="font-semibold">{risk.title}</span>
                            <span className="text-[10px] uppercase opacity-80">{riskLevelLabel(risk.level)}</span>
                          </div>
                          <div className="mt-0.5 opacity-80">{risk.detail}</div>
                          </li>
                      ))}
                    </ul>
                  )}
                </div>
              ))}
            </div>
          </div>

          {analysis.retire_package?.generated && (
            <div className="rounded-lg border border-dark-border bg-dark-panel p-4">
              <h3 className="mb-3 flex flex-wrap items-center gap-2 text-sm font-semibold text-white">
                <ShieldAlert className="h-4 w-4 text-red-400" />
                {tr('下线准备（只出清单与脚本）', 'Retire readiness (checklist & scripts only)')}
                <span className="flex items-center gap-1 rounded border border-red-500/60 bg-red-500/20 px-1.5 py-0.5 text-[11px] font-semibold text-red-300">
                  <AlertTriangle className="h-3.5 w-3.5" />
                  {tr('不可逆', 'IRREVERSIBLE')}
                </span>
              </h3>
              <div className="grid gap-2 text-xs md:grid-cols-4">
                <div className="rounded border border-dark-border bg-dark-bg p-2 text-gray-300">
                  {tr('观察期', 'Observation')}: {analysis.retire_package.observation_elapsed_days ?? '-'} / {analysis.retire_package.observation_required_days} {tr('天', 'days')}
                </div>
                <div className="rounded border border-dark-border bg-dark-bg p-2 text-gray-300">
                  {tr('快照表', 'Snapshot')}: {analysis.retire_package.snapshot_table || '-'}
                </div>
                <div className="rounded border border-dark-border bg-dark-bg p-2 text-gray-300">
                  {tr('快照保留', 'Retention')}: {analysis.retire_package.snapshot_retention_days} {tr('天', 'days')}
                </div>
                <div className="rounded border border-dark-border bg-dark-bg p-2 text-gray-300">
                  {tr('自动核实 / 人工确认', 'Auto / manual')}: {analysis.retire_package.auto_verified_count} / {analysis.retire_package.manual_confirmation_count}
                </div>
              </div>
              <div
                className={`mt-2 rounded border p-2 text-xs ${
                  analysis.retire_package.observation_satisfied
                    ? 'border-emerald-500/40 bg-emerald-500/10 text-emerald-300'
                    : 'border-amber-500/50 bg-amber-500/10 text-amber-300'
                }`}
              >
                {analysis.retire_package.observation_note}
              </div>
              {(analysis.retire_package.blockers || []).length > 0 && (
                <div className="mt-2 rounded border border-red-500/40 bg-red-500/10 p-2 text-xs text-red-300">
                  {tr('未满足项', 'Blockers')}: {(analysis.retire_package.blockers || []).join('；')}
                </div>
              )}
              <div className="mt-3 grid gap-3 md:grid-cols-2">
                {renderChecklistGroup(tr('已自动核实', 'Auto-verified'), analysis.retire_package.checklist.filter((item) => item.auto_verified))}
                {renderChecklistGroup(tr('必须人工确认', 'Requires manual confirmation'), analysis.retire_package.checklist.filter((item) => !item.auto_verified))}
              </div>
              <div className="mt-3 rounded border border-dark-border bg-dark-bg p-3 text-xs text-gray-400">
                <div className="mb-1 font-semibold text-gray-300">{tr('建议执行顺序', 'Suggested order')}</div>
                <ol className="space-y-1">
                  {(analysis.retire_package.execution_order || []).map((step, index) => (
                    <li key={index}>{step}</li>
                  ))}
                </ol>
                <div className="mt-2 text-amber-300">
                  {tr('建议时机', 'Recommended window')}: {analysis.retire_package.recommended_window}
                </div>
                <div className="mt-2 text-gray-500">{analysis.retire_package.note}</div>
              </div>
            </div>
          )}

          {analysis.retire_package?.generated && analysis.rollback && (
            <div className="rounded-lg border border-dark-border bg-dark-panel p-4">
              <h3 className="mb-3 flex flex-wrap items-center gap-2 text-sm font-semibold text-white">
                <RefreshCw className="h-4 w-4 text-amber-400" />
                {tr('回滚评估', 'Rollback assessment')}
                <span
                  className={`rounded border px-1.5 py-0.5 text-[11px] font-mono ${
                    analysis.rollback.irreversible
                      ? 'border-red-500/60 bg-red-500/20 text-red-300'
                      : analysis.rollback.conclusion === 'reversible'
                        ? 'border-emerald-500/50 bg-emerald-500/10 text-emerald-300'
                        : 'border-amber-500/50 bg-amber-500/10 text-amber-300'
                  }`}
                >
                  {analysis.rollback.conclusion}
                </span>
                <span className="rounded border border-dark-border px-1.5 py-0.5 font-mono text-[11px] text-gray-400">
                  {analysis.rollback.shape}
                </span>
                {analysis.rollback.irreversible && (
                  <span className="flex items-center gap-1 rounded border border-red-500/60 bg-red-500/20 px-1.5 py-0.5 text-[11px] font-semibold text-red-300">
                    <AlertTriangle className="h-3.5 w-3.5" />
                    {tr('不可逆', 'IRREVERSIBLE')}
                  </span>
                )}
              </h3>
              <p className="text-xs text-gray-300">{analysis.rollback.summary}</p>
              {(analysis.rollback.reasons || []).length > 0 && (
                <ul className="mt-2 space-y-1 text-xs text-red-300">
                  {(analysis.rollback.reasons || []).map((reason, index) => (
                    <li key={index}>- {reason}</li>
                  ))}
                </ul>
              )}
              {(analysis.rollback.lossy_aspects || []).length > 0 && (
                <ul className="mt-2 space-y-1 text-xs text-amber-300">
                  {(analysis.rollback.lossy_aspects || []).map((item, index) => (
                    <li key={index}>- {item}</li>
                  ))}
                </ul>
              )}
              {(analysis.rollback.prerequisites || []).length > 0 && (
                <div className="mt-2 text-xs text-gray-400">
                  <div className="mb-1 font-semibold text-gray-300">{tr('前提条件', 'Prerequisites')}</div>
                  <ul className="space-y-1">
                    {(analysis.rollback.prerequisites || []).map((item, index) => (
                      <li key={index}>- {item}</li>
                    ))}
                  </ul>
                </div>
              )}
              {analysis.rollback.reverse_sql && (
                <pre className="mt-2 max-h-48 overflow-auto whitespace-pre-wrap rounded border border-dark-border bg-dark-bg p-2 font-mono text-[11px] text-gray-300">
                  {analysis.rollback.reverse_sql}
                </pre>
              )}
              <div className="mt-2 text-[11px] text-gray-500">{analysis.rollback.note}</div>
            </div>
          )}

          <div className="rounded-lg border border-dark-border bg-dark-panel p-4">
            <h3 className="mb-3 text-sm font-semibold text-white">{tr('证据（E01-E14）', 'Evidence (E01-E14)')}</h3>
            <div className="grid gap-2 text-xs md:grid-cols-2">
              <div className="rounded border border-dark-border bg-dark-bg p-2 text-gray-300">
                {analysis.db_name}.{analysis.table}
                {analysis.column ? ` · ${analysis.column}` : ` · ${tr('整表盘点', 'whole-table inventory')}`}
              </div>
              <div className="rounded border border-dark-border bg-dark-bg p-2 text-gray-300">
                {tr('剖面', 'Profile')}: {analysis.profile_mode}
                {analysis.sample_per_mille ? ` (${analysis.sample_per_mille}‰)` : ''} · {tr('行数', 'rows')}: {analysis.row_count}
              </div>
            </div>
            {analysis.gaps.length > 0 && (
              <div className="mt-2 rounded border border-red-500/40 bg-red-500/10 p-2 text-xs text-red-300">
                {tr('证据缺口（不可判定）', 'Evidence gaps (undetermined)')}:
                {analysis.gaps.map((gap) => ` ${gap.evidence_id}: ${gap.reason}`).join(';')}
              </div>
            )}
            <div className="mt-3 overflow-x-auto">
              <table className="w-full min-w-[720px] text-left text-xs">
                <thead className="text-gray-400">
                  <tr className="border-b border-dark-border">
                    <th className="px-2 py-1">ID</th>
                    <th className="px-2 py-1">{tr('证据', 'Evidence')}</th>
                    <th className="px-2 py-1">{tr('状态', 'Status')}</th>
                    <th className="px-2 py-1">{tr('模式', 'Mode')}</th>
                    <th className="px-2 py-1">{tr('摘要', 'Summary')}</th>
                    <th className="px-2 py-1" />
                  </tr>
                </thead>
                <tbody>
                  {analysis.evidence.map((item) => (
                    <tr key={item.id} className="border-b border-dark-border/60 align-top">
                      <td className="px-2 py-1 font-mono text-gray-300">{item.id}</td>
                      <td className="px-2 py-1 text-gray-300">{item.title}</td>
                      <td className={`px-2 py-1 font-mono ${evidenceStatusClass(item.status)}`}>{item.status}</td>
                      <td className="px-2 py-1 font-mono text-gray-500">{item.mode}</td>
                      <td className="px-2 py-1 text-gray-400">{item.summary}</td>
                      <td className="px-2 py-1">
                        {Boolean(item.sql?.length || item.details) && (
                          <button
                            onClick={() => setExpandedEvidence(expandedEvidence === item.id ? null : item.id)}
                            className="flex items-center gap-1 text-gray-400 hover:text-gray-200"
                          >
                            {expandedEvidence === item.id ? (
                              <ChevronUp className="h-3.5 w-3.5" />
                            ) : (
                              <ChevronDown className="h-3.5 w-3.5" />
                            )}
                          </button>
                        )}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
            {expandedEvidence && (
              <div className="mt-2 rounded border border-dark-border bg-dark-bg p-2">
                {(() => {
                  const item = analysis.evidence.find((entry) => entry.id === expandedEvidence)
                  if (!item) return null
                  return (
                    <div className="space-y-2 text-xs">
                      {(item.sql || []).map((sql, index) => (
                        <pre key={index} className="overflow-x-auto whitespace-pre-wrap font-mono text-[11px] text-gray-300">
                          {sql}
                        </pre>
                      ))}
                      {item.details != null && (
                        <pre className="max-h-64 overflow-auto whitespace-pre-wrap font-mono text-[11px] text-gray-400">
                          {JSON.stringify(item.details, null, 2)}
                        </pre>
                      )}
                    </div>
                  )
                })()}
              </div>
            )}

            {analysis.columns.length > 0 && (
              <div className="mt-4 overflow-x-auto">
                <h4 className="mb-2 text-xs font-semibold text-gray-300">{tr('列剖面', 'Column profiles')}</h4>
                <table className="w-full min-w-[900px] text-left text-xs">
                  <thead className="text-gray-400">
                    <tr className="border-b border-dark-border">
                      <th className="px-2 py-1">{tr('列', 'Column')}</th>
                      <th className="px-2 py-1">{tr('类型', 'Type')}</th>
                      <th className="px-2 py-1">{tr('可空', 'Nullable')}</th>
                      <th className="px-2 py-1">{tr('NULL', 'NULL')}</th>
                      <th className="px-2 py-1">{tr('空串', 'Empty')}</th>
                      <th className="px-2 py-1">{tr('最大字符', 'Max chars')}</th>
                      <th className="px-2 py-1">{tr('非法 JSON', 'Invalid JSON')}</th>
                      <th className="px-2 py-1">{tr('JSON keys', 'JSON keys')}</th>
                    </tr>
                  </thead>
                  <tbody>
                    {analysis.columns.map((entry) => (
                      <tr key={entry.column} className="border-b border-dark-border/60">
                        <td className="px-2 py-1 font-mono text-gray-200">{entry.column}</td>
                        <td className="px-2 py-1 font-mono text-gray-400">{entry.column_type}</td>
                        <td className="px-2 py-1 text-gray-400">{entry.nullable ? 'YES' : 'NO'}</td>
                        <td className="px-2 py-1 text-gray-400">{entry.null_rows}</td>
                        <td className="px-2 py-1 text-gray-400">{entry.empty_rows}</td>
                        <td className="px-2 py-1 text-gray-400">{entry.max_char_length}</td>
                        <td className={`px-2 py-1 font-mono ${entry.json_invalid_rows > 0 ? 'text-red-400' : 'text-gray-500'}`}>
                          {entry.json_invalid_rows}
                        </td>
                        <td className="px-2 py-1 font-mono text-gray-500">
                          {(entry.json_keys || []).length > 0 ? (entry.json_keys || []).join(', ') : '-'}
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            )}

            <div className="mt-4 grid gap-3 text-xs md:grid-cols-2">
              <div className="rounded border border-dark-border bg-dark-bg p-2">
                <div className="mb-1 text-gray-400">{tr('依赖对象', 'Dependencies')}</div>
                <div className="text-gray-300">
                  {tr('索引', 'Indexes')}: {(analysis.dependencies.indexes || []).length} ·{' '}
                  {tr('外键', 'Foreign keys')}: {(analysis.dependencies.foreign_keys || []).length} ·{' '}
                  {tr('生成列', 'Generated')}: {(analysis.dependencies.generated_columns || []).length} ·{' '}
                  {tr('视图/触发/例程', 'Views/triggers/routines')}: {(analysis.dependencies.objects || []).length}
                </div>
                <div className={`mt-1 ${analysis.dependencies.scan_complete ? 'text-emerald-400' : 'text-red-400'}`}>
                  {tr('扫描完整', 'Scan complete')}: {analysis.dependencies.scan_complete ? 'true' : 'false'}
                </div>
              </div>
              <div className="rounded border border-dark-border bg-dark-bg p-2">
                <div className="mb-1 text-gray-400">{tr('环境', 'Environment')}</div>
                <div className="text-gray-300">
                  {tr('引擎', 'Engine')}: {analysis.environment.engine || '-'} · {tr('排序规则', 'Collation')}:{' '}
                  {analysis.environment.table_collation || '-'}
                </div>
                <div className="mt-1 text-gray-400">
                  {tr('只读实例', 'Read only')}: {analysis.environment.read_only ? 'yes' : 'no'} ·{' '}
                  {tr('分区表', 'Partitioned')}: {analysis.environment.partitioned ? 'yes' : 'no'} ·{' '}
                  {tr('长事务', 'Long transactions')}: {analysis.environment.long_transactions}
                </div>
              </div>
            </div>

            {(analysis.declarations || []).length > 0 && (
              <div className="mt-4 rounded border border-dark-border bg-dark-bg p-3 text-xs">
                <div className="text-gray-400">{tr('语义声明（未验证，只增约束）', 'Semantic declarations (unverified, constraints only)')}</div>
                <ul className="mt-2 space-y-1">
                  {(analysis.declarations || []).map((declaration) => (
                    <li key={declaration.id} className="text-gray-300">
                      <span className="font-mono text-gray-500">{declaration.id}</span> · {declaration.source} ·{' '}
                      {declaration.verified ? tr('已验证', 'verified') : tr('未验证', 'unverified')}
                      {declaration.purpose ? ` · ${declaration.purpose}` : ''}
                    </li>
                  ))}
                </ul>
                {(analysis.declaration_evidence || []).length > 0 && (
                  <ul className="mt-2 space-y-1 text-gray-400">
                    {(analysis.declaration_evidence || []).map((evidence, index) => (
                      <li key={`${evidence.declaration_id}-${index}`}>
                        {evidence.declaration_id} / {evidence.kind}: rows={evidence.rows}
                        {evidence.key_set?.length ? ` keys=${evidence.key_set.join(',')}` : ''} {evidence.detail}
                      </li>
                    ))}
                  </ul>
                )}
              </div>
            )}
          </div>

          <div className="rounded-lg border border-dark-border bg-dark-panel p-4">
            <h3 className="mb-3 flex items-center gap-2 text-sm font-semibold text-white">
              <FileCode2 className="h-4 w-4 text-emerald-400" />
              {tr('Dry-run 脚本包（只生成，不执行）', 'Dry-run script pack (generated, never executed)')}
            </h3>
            {analysis.scripts.length === 0 ? (
              <p className="text-xs text-gray-500">
                {tr('整表盘点模式不生成脚本；请选择具体源列。', 'Whole-table inventory generates no scripts; select a specific source column.')}
              </p>
            ) : (
              <div className="flex flex-col gap-3">
                {analysis.scripts.map((script) => (
                  <div
                    key={script.id}
                    className={`rounded border p-3 ${
                      script.irreversible ? 'border-red-500/50 bg-red-500/10' : 'border-dark-border bg-dark-bg'
                    }`}
                  >
                    <div className="flex flex-wrap items-center justify-between gap-2">
                      <div className="flex flex-wrap items-center gap-2 text-xs">
                        <span className="font-semibold text-gray-200">{script.title}</span>
                        <span className="rounded border border-dark-border px-1.5 py-0.5 font-mono text-gray-400">
                          {script.kind}
                        </span>
                        <span className="rounded border border-dark-border px-1.5 py-0.5 font-mono text-gray-400">
                          {tr('阶段', 'phase')}: {script.phase}
                        </span>
                        <span className="rounded border border-emerald-500/40 px-1.5 py-0.5 text-emerald-300">
                          executable=false
                        </span>
                        {script.irreversible && (
                          <span className="flex items-center gap-1 rounded border border-red-500/60 bg-red-500/20 px-1.5 py-0.5 font-semibold text-red-300">
                            <AlertTriangle className="h-3.5 w-3.5" />
                            {tr('不可逆', 'IRREVERSIBLE')}
                          </span>
                        )}
                      </div>
                      <button
                        onClick={() => void handleCopy(script)}
                        className="flex items-center gap-1 rounded border border-dark-border px-2 py-1 text-xs text-gray-300 hover:bg-dark-panel"
                      >
                        {copiedScript === script.id ? <CheckCircle2 className="h-3.5 w-3.5 text-emerald-400" /> : <ClipboardCopy className="h-3.5 w-3.5" />}
                        {copiedScript === script.id ? tr('已复制', 'Copied') : tr('复制', 'Copy')}
                      </button>
                    </div>
                    {(script.notes || []).length > 0 && (
                      <ul className="mt-2 space-y-1 text-[11px] text-gray-500">
                        {(script.notes || []).map((note, index) => (
                          <li key={index}>- {note}</li>
                        ))}
                      </ul>
                    )}
                    <pre className="mt-2 max-h-64 overflow-auto whitespace-pre-wrap rounded border border-dark-border bg-dark-bg p-2 font-mono text-[11px] text-gray-300">
                      {script.sql}
                    </pre>
                  </div>
                ))}
              </div>
            )}

            <div className="mt-4 rounded border border-dark-border bg-dark-bg p-3 text-xs">
              <div className="flex flex-wrap items-center justify-between gap-2">
                <span className="text-gray-400">{tr('dry-run 检查', 'Dry-run checks')}</span>
                <span className={analysis.dry_run.passed ? 'text-emerald-400' : 'text-red-400'}>
                  {analysis.dry_run.passed ? tr('通过', 'PASS') : tr('未通过', 'FAIL')} ·{' '}
                  {tr('未连接数据库', 'not executed against DB')}: {analysis.dry_run.executed_against_database ? 'false' : 'true'}
                </span>
              </div>
              {(analysis.dry_run.checks || []).length > 0 && (
                <ul className="mt-2 space-y-1">
                  {analysis.dry_run.checks.map((check, index) => (
                    <li key={`${check.script_id}-${index}`} className="text-gray-400">
                      <span
                        className={
                          check.status === 'pass'
                            ? 'text-emerald-400'
                            : check.status === 'fail'
                              ? 'text-red-400'
                              : 'text-amber-400'
                        }
                      >
                        {check.status}
                      </span>{' '}
                      · {check.script_id} / {check.check}: {check.detail}
                    </li>
                  ))}
                </ul>
              )}
              <div className="mt-2 text-gray-500">{analysis.dry_run.note}</div>
            </div>
          </div>

          {analysis.column && (analysis.target_columns || []).length > 0 && (
            <div className="rounded-lg border border-dark-border bg-dark-panel p-4">
              <h3 className="mb-1 flex items-center gap-2 text-sm font-semibold text-white">
                <FileCode2 className="h-4 w-4 text-amber-400" />
                {tr('差异主键分页（V2/V3/V4，只读）', 'Diff primary-key pagination (V2/V3/V4, read-only)')}
              </h3>
              <p className="text-[11px] text-gray-500">
                {tr(
                  '差异总数由服务端 COUNT 返回；按主键区间翻页（页大小 100），不再用固定 LIMIT 100 静默截断。',
                  'Total diffs come from a server-side COUNT; pages advance by primary-key interval (page size 100), never a silent LIMIT 100.'
                )}
              </p>
              <div className="mt-3 flex flex-wrap items-center gap-2">
                {(['v2', 'v3', 'v4'] as const).map((phase) => (
                  <button
                    key={phase}
                    onClick={() => {
                      setDiffPhase(phase)
                      setDiffPages([])
                      setDiffIndex(0)
                      setDiffError('')
                    }}
                    className={`rounded border px-2 py-1 text-xs ${
                      diffPhase === phase
                        ? 'border-blue-500/60 bg-blue-500/10 text-blue-300'
                        : 'border-dark-border text-gray-400 hover:bg-dark-bg'
                    }`}
                  >
                    {phase.toUpperCase()}
                    {' · '}
                    {phase === 'v2'
                      ? tr('源有目标缺', 'missing in target')
                      : phase === 'v3'
                        ? tr('目标多余', 'extra in target')
                        : tr('逐行不一致', 'row mismatch')}
                  </button>
                ))}
                <button
                  onClick={() => void loadFirstDiffPage()}
                  disabled={diffBusy}
                  className="flex items-center gap-1 rounded border border-dark-border px-2 py-1 text-xs text-gray-300 hover:bg-dark-bg disabled:opacity-50"
                >
                  {diffBusy ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <RefreshCw className="h-3.5 w-3.5" />}
                  {tr('查询差异', 'Query diffs')}
                </button>
                <button
                  onClick={() => setDiffIndex((index) => Math.max(0, index - 1))}
                  disabled={diffBusy || diffIndex === 0}
                  className="rounded border border-dark-border px-2 py-1 text-xs text-gray-300 hover:bg-dark-bg disabled:opacity-50"
                >
                  {tr('上一页', 'Previous')}
                </button>
                <button
                  onClick={() => void loadNextDiffPage()}
                  disabled={diffBusy || !currentDiffPage?.has_more}
                  className="rounded border border-dark-border px-2 py-1 text-xs text-gray-300 hover:bg-dark-bg disabled:opacity-50"
                >
                  {tr('下一页', 'Next')}
                </button>
                <button
                  onClick={() => currentDiffPage && void copyDiffPks(currentDiffPage.pks, 'page')}
                  disabled={!currentDiffPage}
                  className="flex items-center gap-1 rounded border border-dark-border px-2 py-1 text-xs text-gray-300 hover:bg-dark-bg disabled:opacity-50"
                >
                  <ClipboardCopy className="h-3.5 w-3.5" />
                  {diffCopied === 'page' ? tr('已复制本页', 'Page copied') : tr('复制本页主键', 'Copy page PKs')}
                </button>
                <button
                  onClick={() => void copyAllDiffPks()}
                  disabled={diffBusy || !analysis.column}
                  className="flex items-center gap-1 rounded border border-amber-500/50 px-2 py-1 text-xs text-amber-300 hover:bg-amber-500/10 disabled:opacity-50"
                >
                  <ClipboardCopy className="h-3.5 w-3.5" />
                  {diffCopied === 'all' ? tr('已复制全部', 'All copied') : tr('复制全部差异主键', 'Copy all diff PKs')}
                </button>
              </div>
              {diffError && <div className="mt-2 text-xs text-red-400">{diffError}</div>}
              {currentDiffPage && (
                <div className="mt-3 rounded border border-dark-border bg-dark-bg p-3 text-xs">
                  <div className="flex flex-wrap gap-4 text-gray-300">
                    <span>
                      {tr('差异总数', 'Total diffs')}: <span className="font-mono text-white">{currentDiffPage.total}</span>
                    </span>
                    <span>
                      {tr('页码', 'Page')}: <span className="font-mono text-white">{diffIndex + 1}/{diffPages.length}</span>
                    </span>
                    <span>
                      {tr('页大小', 'Page size')}: <span className="font-mono text-white">{currentDiffPage.page_size}</span>
                    </span>
                    <span>
                      {tr('主键', 'PK')}: <span className="font-mono text-white">{currentDiffPage.pk_column}</span>
                    </span>
                  </div>
                  <div className="mt-1 text-gray-400">
                    {tr('本页主键', 'Page PKs')}: {currentDiffPage.pks.length}
                    {currentDiffPage.pks.length > 0
                      ? ` · ${currentDiffPage.pks[0]} ~ ${currentDiffPage.pks[currentDiffPage.pks.length - 1]}`
                      : ''}
                  </div>
                  {currentDiffPage.pks.length > 0 && (
                    <pre className="mt-2 max-h-48 overflow-auto whitespace-pre-wrap font-mono text-[11px] text-gray-300">
                      {currentDiffPage.pks.join('\n')}
                    </pre>
                  )}
                  {currentDiffPage.has_more && currentDiffPage.next_last_pk && (
                    <div className="mt-2 text-gray-500">
                      {tr('下一页游标（主键区间）', 'Next cursor (PK interval)')}:{' '}
                      <span className="font-mono">{currentDiffPage.next_last_pk}</span>
                    </div>
                  )}
                </div>
              )}
            </div>
          )}

          {analysis.column && (analysis.target_columns || []).length > 0 && (
            <div className="rounded-lg border border-dark-border bg-dark-panel p-4">
              <h3 className="mb-1 flex items-center gap-2 text-sm font-semibold text-white">
                <Play className="h-4 w-4 text-emerald-400" />
                {tr('回填执行（S2：分批 + checkpoint）', 'Backfill execution (S2: batches + checkpoint)')}
              </h3>
              <p className="text-[11px] text-gray-500">
                {tr(
                  '仅对目标表执行 CREATE，源表只读；每批写入前后做 V1-V4 校验，失败即停在该批。下线（DROP COLUMN）没有执行入口，只保留脚本。',
                  'Only CREATE runs on the target; the source stays read-only. Every batch is verified (V1-V4) before and after writing, and a failure stops at that batch. Retirement (DROP COLUMN) has no execution entry and stays script-only.'
                )}
              </p>

              <div className="mt-3 grid gap-2 md:grid-cols-4">
                <div className="flex flex-col gap-1">
                  <label className="text-xs text-gray-400">{tr('批大小', 'Batch size')}</label>
                  <input
                    value={backfillBatchSize}
                    onChange={(event) => setBackfillBatchSize(event.target.value)}
                    inputMode="numeric"
                    className="h-8 rounded border border-dark-border bg-dark-bg px-2 font-mono text-xs text-white outline-none"
                  />
                </div>
                <div className="flex flex-col gap-1">
                  <label className="text-xs text-gray-400">{tr('批间隔（毫秒）', 'Batch interval (ms)')}</label>
                  <input
                    value={backfillIntervalMs}
                    onChange={(event) => setBackfillIntervalMs(event.target.value)}
                    inputMode="numeric"
                    className="h-8 rounded border border-dark-border bg-dark-bg px-2 font-mono text-xs text-white outline-none"
                  />
                </div>
                <div className="flex flex-col gap-1">
                  <label className="text-xs text-gray-400">{tr('行/秒上限（0=不限）', 'Rows/sec cap (0 = unlimited)')}</label>
                  <input
                    value={backfillMaxRps}
                    onChange={(event) => setBackfillMaxRps(event.target.value)}
                    inputMode="numeric"
                    className="h-8 rounded border border-dark-border bg-dark-bg px-2 font-mono text-xs text-white outline-none"
                  />
                </div>
                <label className="flex items-start gap-2 self-end text-xs text-gray-400">
                  <input
                    type="checkbox"
                    checked={backfillProdConfirmed}
                    onChange={(event) => setBackfillProdConfirmed(event.target.checked)}
                    className="mt-0.5"
                  />
                  {tr('生产库显式确认（需策略开关）', 'Explicit production confirmation (policy switch required)')}
                </label>
              </div>

              {highRisks.length > 0 && (
                <div className="mt-3 rounded border border-amber-500/40 bg-amber-500/10 p-2 text-xs">
                  <div className="text-amber-300">{tr('HIGH 风险需逐项确认后才能启动', 'Every HIGH risk must be confirmed before start')}</div>
                  <ul className="mt-1 space-y-1">
                    {highRisks.map((risk) => (
                      <li key={risk.id} className="text-amber-200/90">
                        <label className="flex items-start gap-2">
                          <input
                            type="checkbox"
                            checked={backfillConfirmedRisks.includes(risk.id)}
                            onChange={(event) =>
                              setBackfillConfirmedRisks((prev) =>
                                event.target.checked
                                  ? [...prev, risk.id]
                                  : prev.filter((id) => id !== risk.id)
                              )
                            }
                            className="mt-0.5"
                          />
                          <span>
                            <span className="mr-1 rounded bg-black/20 px-1 font-mono">{risk.code}</span>
                            {risk.title}
                          </span>
                        </label>
                      </li>
                    ))}
                  </ul>
                </div>
              )}

              {(blockRisks.length > 0 || analysis.risk_summary.undetermined_write_block) && (
                <div className="mt-2 rounded border border-red-500/40 bg-red-500/10 px-2 py-1 text-xs text-red-300">
                  {tr('存在阻断项或不可判定项，回填不允许启动。', 'Blocking/undetermined items present; backfill cannot start.')}
                </div>
              )}

              <div className="mt-3 flex flex-wrap items-center gap-2">
                <button
                  onClick={() => void handleBackfillStart()}
                  disabled={
                    backfillBusy ||
                    migrationActive ||
                    blockRisks.length > 0 ||
                    analysis.risk_summary.undetermined_write_block
                  }
                  className="flex items-center gap-1 rounded bg-emerald-600 px-3 py-1.5 text-xs font-medium text-white hover:bg-emerald-500 disabled:cursor-not-allowed disabled:opacity-50"
                >
                  {backfillBusy ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <Play className="h-3.5 w-3.5" />}
                  {tr('审批并启动回填', 'Approve & start backfill')}
                </button>
                <button
                  onClick={() => void handleBackfillApprove()}
                  disabled={backfillBusy}
                  className="rounded border border-dark-border px-2 py-1 text-xs text-gray-300 hover:bg-dark-bg disabled:opacity-50"
                >
                  {tr('仅签发审批令牌', 'Issue approval token only')}
                </button>
                <button
                  onClick={() => void handleBackfillRefresh()}
                  disabled={!backfillJob || backfillBusy}
                  className="flex items-center gap-1 rounded border border-dark-border px-2 py-1 text-xs text-gray-300 hover:bg-dark-bg disabled:opacity-50"
                >
                  <RefreshCw className="h-3.5 w-3.5" />
                  {tr('刷新进度', 'Refresh')}
                </button>
                <button
                  onClick={() => void handleBackfillPause()}
                  disabled={!backfillJob || backfillJob.status !== 'running' || backfillBusy}
                  className="flex items-center gap-1 rounded border border-amber-500/50 px-2 py-1 text-xs text-amber-300 hover:bg-amber-500/10 disabled:opacity-50"
                >
                  <Pause className="h-3.5 w-3.5" />
                  {tr('暂停', 'Pause')}
                </button>
                <button
                  onClick={() => void handleBackfillResume()}
                  disabled={!backfillResumable || backfillBusy}
                  className="flex items-center gap-1 rounded border border-blue-500/50 px-2 py-1 text-xs text-blue-300 hover:bg-blue-500/10 disabled:opacity-50"
                >
                  <Play className="h-3.5 w-3.5" />
                  {tr('继续（从 checkpoint）', 'Resume (from checkpoint)')}
                </button>
                <button
                  onClick={() => void handleBackfillAbort()}
                  disabled={
                    !backfillJob || backfillJob.status === 'completed' || backfillJob.status === 'aborted' || backfillBusy
                  }
                  className="flex items-center gap-1 rounded border border-red-500/50 px-2 py-1 text-xs text-red-300 hover:bg-red-500/10 disabled:opacity-50"
                >
                  <Square className="h-3.5 w-3.5" />
                  {tr('中止', 'Abort')}
                </button>
              </div>

              {(backfillError || backfillNotice) && (
                <div className={`mt-2 text-xs ${backfillError ? 'text-red-400' : 'text-emerald-400'}`}>
                  {backfillError || backfillNotice}
                </div>
              )}

              {backfillJob && (
                <div className="mt-3 rounded border border-dark-border bg-dark-bg p-3 text-xs">
                  <div className="flex flex-wrap items-center gap-3">
                    <span className={`rounded border px-2 py-0.5 font-mono ${migrationStatusClass(backfillJob.status)}`}>
                      {migrationStatusLabel(backfillJob.status)}
                    </span>
                    <span className="font-mono text-gray-500">{backfillJob.job_id}</span>
                    <span className="text-gray-400">
                      {tr('阶段', 'Phase')}: <span className="font-mono text-white">{backfillJob.phase}</span>
                    </span>
                    <span className="text-gray-400">
                      {tr('运行次数', 'Runs')}: <span className="font-mono text-white">{backfillJob.run_count}</span>
                    </span>
                  </div>

                  <div className="mt-2 grid gap-2 md:grid-cols-4">
                    <div className="rounded border border-dark-border bg-dark-panel p-2 text-gray-300">
                      {tr('预期行数', 'Expected')}: <span className="font-mono text-white">{backfillJob.progress.expected_rows}</span>
                    </div>
                    <div className="rounded border border-emerald-500/40 bg-emerald-500/10 p-2 text-emerald-300">
                      {tr('成功行数', 'Success')}: <span className="font-mono">{backfillJob.progress.success_rows}</span>
                    </div>
                    <div className={`rounded border p-2 ${backfillJob.progress.failed_rows > 0 ? 'border-red-500/40 bg-red-500/10 text-red-300' : 'border-dark-border bg-dark-panel text-gray-300'}`}>
                      {tr('失败行数', 'Failed')}: <span className="font-mono">{backfillJob.progress.failed_rows}</span>
                    </div>
                    <div className="rounded border border-dark-border bg-dark-panel p-2 text-gray-300">
                      {tr('剩余行数', 'Remaining')}: <span className="font-mono text-white">{backfillJob.progress.remaining_rows}</span>
                    </div>
                    <div className="rounded border border-dark-border bg-dark-panel p-2 text-gray-300">
                      {tr('当前批', 'Current batch')}:{' '}
                      <span className="font-mono text-white">{backfillJob.progress.current_batch}</span> /{' '}
                      <span className="font-mono">{backfillJob.progress.total_batches}</span>
                    </div>
                    <div className="rounded border border-dark-border bg-dark-panel p-2 text-gray-300">
                      {tr('最近批次主键', 'Last batch PK')}:{' '}
                      <span className="font-mono text-white">{backfillJob.progress.last_pk ?? '-'}</span>
                    </div>
                    <div className="rounded border border-dark-border bg-dark-panel p-2 text-gray-300">
                      {tr('续跑起点（checkpoint）', 'Resume point (checkpoint)')}:{' '}
                      <span className="font-mono text-white">{backfillJob.progress.resume_after_pk ?? '-'}</span>
                      {backfillJob.progress.resume_skipped_batches > 0 && (
                        <span className="ml-1 text-gray-500">
                          {tr(`（直接跳过 ${backfillJob.progress.resume_skipped_batches} 批）`, `(skipped ${backfillJob.progress.resume_skipped_batches} batches)`)}
                        </span>
                      )}
                    </div>
                    <div className="rounded border border-dark-border bg-dark-panel p-2 text-gray-300">
                      {tr('已完成批次', 'Checkpoint batches')}:{' '}
                      <span className="font-mono text-white">{backfillJob.checkpoint?.completed_batches ?? 0}</span>
                      {' · '}
                      {tr('已写入', 'written')}:{' '}
                      <span className="font-mono text-white">{backfillJob.checkpoint?.written_rows ?? 0}</span>
                    </div>
                  </div>

                  <div className="mt-2 flex flex-wrap gap-4 text-gray-400">
                    <span>
                      {tr('批配置', 'Batch config')}:{' '}
                      <span className="font-mono text-white">
                        {backfillJob.config.batch_size}/{backfillJob.config.batch_interval_ms}ms/{backfillJob.config.max_rows_per_second}rps
                      </span>
                    </span>
                    {backfillJob.checkpoint?.last_pk != null && (
                      <span>
                        {tr('checkpoint 主键', 'Checkpoint PK')}:{' '}
                        <span className="font-mono text-white">{backfillJob.checkpoint.last_pk}</span>
                      </span>
                    )}
                    {backfillJob.last_error && <span className="text-red-400">{backfillJob.last_error}</span>}
                  </div>

                  {backfillJob.blocking && (
                    <div className="mt-3 rounded border-2 border-red-500/60 bg-red-500/10 p-2 text-red-200">
                      <div className="flex flex-wrap items-center gap-2 font-semibold text-red-300">
                        <ShieldAlert className="h-4 w-4" />
                        {tr('已阻断，未继续后续批次', 'Blocked; later batches were not run')}
                        <span className="rounded bg-black/20 px-1 font-mono">
                          {tr('批', 'batch')} {backfillJob.blocking.batch_no}
                        </span>
                        <span className="rounded bg-black/20 px-1 font-mono">{backfillJob.blocking.phase}</span>
                        {backfillJob.blocking.last_pk != null && (
                          <span className="rounded bg-black/20 px-1 font-mono">
                            {backfillJob.blocking.last_pk} → {backfillJob.blocking.end_pk ?? '...'}
                          </span>
                        )}
                      </div>
                      <div className="mt-1">{backfillJob.blocking.message}</div>
                      <div className="mt-1 font-mono text-red-300">
                        {tr('差异数', 'diffs')}={backfillJob.blocking.total_diffs}
                        {backfillJob.blocking.verify
                          ? ` · V1=${backfillJob.blocking.verify.source_rows}/${backfillJob.blocking.verify.target_rows} V2=${backfillJob.blocking.verify.v2} V3=${backfillJob.blocking.verify.v3} V4=${backfillJob.blocking.verify.v4}`
                          : ''}
                      </div>
                      {backfillJob.blocking.pks.length > 0 && (
                        <>
                          <div className="mt-2 flex items-center justify-between gap-2">
                            <span>
                              {tr('定位主键', 'Locating PKs')}（{backfillJob.blocking.pks.length}
                              {backfillJob.blocking.has_more ? '+' : ''}）
                            </span>
                            <button
                              onClick={() => void copyBlockingPks()}
                              className="flex items-center gap-1 rounded border border-red-500/50 px-2 py-0.5 text-red-200 hover:bg-red-500/20"
                            >
                              <ClipboardCopy className="h-3.5 w-3.5" />
                              {tr('复制主键', 'Copy PKs')}
                            </button>
                          </div>
                          <pre className="mt-1 max-h-40 overflow-auto whitespace-pre-wrap font-mono text-[11px] text-red-200">
                            {backfillJob.blocking.pks.join('\n')}
                          </pre>
                        </>
                      )}
                    </div>
                  )}

                  {backfillJob.recent_batches.length > 0 && (
                    <div className="mt-3 overflow-x-auto">
                      <table className="w-full min-w-[720px] text-left text-xs">
                        <thead className="text-gray-400">
                          <tr className="border-b border-dark-border">
                            <th className="px-2 py-1">{tr('批', 'Batch')}</th>
                            <th className="px-2 py-1">{tr('区间', 'Range')}</th>
                            <th className="px-2 py-1">{tr('写入', 'Written')}</th>
                            <th className="px-2 py-1">{tr('V1', 'V1')}</th>
                            <th className="px-2 py-1">V2/V3/V4</th>
                            <th className="px-2 py-1">{tr('校验', 'Verified')}</th>
                            <th className="px-2 py-1">{tr('耗时', 'Duration')}</th>
                          </tr>
                        </thead>
                        <tbody>
                          {backfillJob.recent_batches.slice(-20).map((batch) => (
                            <tr key={batch.batch_no} className="border-b border-dark-border/60 font-mono">
                              <td className="px-2 py-1 text-gray-300">{batch.batch_no}</td>
                              <td className="px-2 py-1 text-gray-400">
                                {batch.last_pk ?? '-'} → {batch.end_pk}
                              </td>
                              <td className="px-2 py-1 text-gray-300">{batch.written_rows}</td>
                              <td className="px-2 py-1 text-gray-400">
                                {batch.verify.source_rows}/{batch.verify.target_rows}
                              </td>
                              <td className={`px-2 py-1 ${batch.verify.v2 + batch.verify.v3 + batch.verify.v4 > 0 ? 'text-red-400' : 'text-gray-400'}`}>
                                {batch.verify.v2}/{batch.verify.v3}/{batch.verify.v4}
                              </td>
                              <td className={`px-2 py-1 ${batch.verified ? 'text-emerald-400' : 'text-red-400'}`}>
                                {batch.verified ? 'PASS' : 'FAIL'}
                              </td>
                              <td className="px-2 py-1 text-gray-500">{batch.duration_ms}ms</td>
                            </tr>
                          ))}
                        </tbody>
                      </table>
                    </div>
                  )}
                </div>
              )}
            </div>
          )}
        </>
      )}
    </div>
  )
}
