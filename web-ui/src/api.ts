import axios from 'axios'
import { getErrorMessage } from './utils/ErrorDictionary'
import { redactSensitiveText } from './utils'
import { getLocale, tr } from './i18n'
import type { RowData, TableWithDetails, KnowledgeItem, ConfigData } from './types'

export const HTTP_TIMEOUT_MS = 120000
export const JOB_POLL_REQUEST_TIMEOUT_MS = 10000
export const JOB_POLL_INTERVAL_MS = 1200
const DESKTOP_BACKEND_COMMAND = 'desktop_backend_base_url'
const WEB_BACKEND_BASE_URL = '/backend'
const DESKTOP_BACKEND_UNAVAILABLE_PREFIX = '本地后端服务不可用'

type TauriInvoke = <T = unknown>(command: string, args?: Record<string, unknown>) => Promise<T>

let backendBaseUrlPromise: Promise<string> | null = null
let resolvedBackendBaseUrl: string | null = null

export function isDesktopRuntime(): boolean {
  if (typeof window === 'undefined') return false
  const w = window as Window & { __TAURI__?: unknown; __TAURI_INTERNALS__?: unknown }
  return Boolean(w.__TAURI__ || w.__TAURI_INTERNALS__)
}

function normalizeDesktopError(error: unknown): Error {
  if (error instanceof Error) return error
  if (typeof error === 'string') return new Error(error)
  const errorObj = error as { message?: unknown }
  if (typeof errorObj?.message === 'string') return new Error(errorObj.message)
  try {
    return new Error(JSON.stringify(error))
  } catch {
    return new Error(String(error ?? 'Unknown error'))
  }
}

/**
 * Desktop: ask the Tauri shell for the sidecar's base URL. The shell resolves
 * this only after the local web-server announced readiness, so every request
 * waits until the backend is actually up. Web: keep the same-origin prefix.
 */
async function resolveBackendBaseUrl(): Promise<string> {
  if (!isDesktopRuntime()) {
    resolvedBackendBaseUrl = WEB_BACKEND_BASE_URL
    return WEB_BACKEND_BASE_URL
  }
  if (!backendBaseUrlPromise) {
    backendBaseUrlPromise = import('@tauri-apps/api/core')
      .then((mod) => mod.invoke as TauriInvoke)
      .then((invoke) => invoke<string>(DESKTOP_BACKEND_COMMAND))
      .then((baseUrl) => {
        resolvedBackendBaseUrl = baseUrl
        return baseUrl
      })
      .catch((error) => {
        throw new Error(
          `${DESKTOP_BACKEND_UNAVAILABLE_PREFIX}：${normalizeDesktopError(error).message}`
        )
      })
  }
  return backendBaseUrlPromise
}

/** Absolute backend base URL once resolved; `/backend` (same-origin) otherwise. */
export function getBackendBaseUrl(): string {
  return resolvedBackendBaseUrl || WEB_BACKEND_BASE_URL
}

/** Builds an absolute URL for endpoints used outside axios (downloads, fetch). */
export function backendUrl(path: string): string {
  const normalized = path.startsWith('/') ? path : `/${path}`
  return `${getBackendBaseUrl()}${normalized}`
}

async function runExecuteRequest(
  sql: string,
  force?: boolean,
  db_id?: string,
  chunk_offset?: number,
  chunk_size?: number,
  cancel_token?: string,
  transaction_id?: string
) {
  const payload = { sql, force, db_id, chunk_offset, chunk_size, cancel_token, transaction_id }
  const res = await client.post('/execute', payload)
  return res.data
}

const client = axios.create({
  timeout: HTTP_TIMEOUT_MS,
})

export interface ChatMessage {
  role: string;
  content: string;
}

type AiQueryPayload = {
  query: string
  mode?: 'generate' | 'optimize' | 'explain'
  current_sql?: string
  chat_history?: ChatMessage[]
}

let opsTokenPromise: Promise<string> | null = null

async function getOpsToken(force = false): Promise<string> {
  if (!opsTokenPromise || force) {
    opsTokenPromise = client
      .get('/ops/session-token', { headers: { 'x-silent-error': '1' } })
      .then((res) => String(res.data?.token || ''))
      .catch((error) => {
        opsTokenPromise = null
        throw error
      })
  }
  return opsTokenPromise
}

async function opsWriteHeaders(): Promise<Record<string, string>> {
  const token = await getOpsToken()
  return { 'x-ops-token': token }
}

client.interceptors.request.use(async (config) => {
  config.baseURL = await resolveBackendBaseUrl()
  const locale = getLocale()
  config.headers = config.headers || {}
  ;(config.headers as Record<string, string>)['Accept-Language'] = locale
  ;(config.headers as Record<string, string>)['x-locale'] = locale
  return config
})

client.interceptors.response.use(
  (response) => {
    return response;
  },
  (error) => {
    const silent = error?.config?.headers?.['x-silent-error'] === '1';
    let message = tr('网络错误，请稍后重试。', 'Network error, please try again later.');
    const status = error?.response?.status as number | undefined;
    const data = error?.response?.data as { code?: string; message?: string; details?: string } | undefined;
    const isCanceled = data?.code === 'ERR_CANCELED';
    const backendUnavailable = error instanceof Error
      && error.message.startsWith(DESKTOP_BACKEND_UNAVAILABLE_PREFIX);

    if (backendUnavailable) {
      message = error.message;
    } else if (data) {
      const { code, message: serverMessage, details } = data;
      if (code) {
        const fallback = tr('发生未知错误', 'Unknown error')
        const merged = details ? `${serverMessage || fallback}（${details}）` : (serverMessage || fallback);
        message = getErrorMessage(code, merged);
      } else if (serverMessage) {
        message = details ? `${serverMessage}（${details}）` : serverMessage;
      }
    } else if (status) {
      if (status === 401) message = tr('未授权（401）：请检查登录状态或 API Key。', 'Unauthorized (401): please check login status or API key.');
      if (status === 403) message = tr('禁止访问（403）：请检查权限或服务商风控。', 'Forbidden (403): please check permissions or provider restriction.');
      if (status === 404) message = tr('接口不存在（404）：请确认后端已更新并重启。', 'Not found (404): please confirm backend is updated and restarted.');
    }
    
    if (!silent && !isCanceled) {
      window.dispatchEvent(new CustomEvent('global-toast', {
        detail: { message: redactSensitiveText(message), type: 'error' }
      }));
    }
    
    return Promise.reject(error);
  }
)

interface DataSyncComparePayload {
  source_db_id: string;
  target_db_id: string;
  tables?: string[];
  table_name?: string;
  primary_key?: string;
  mode?: string;
  chunk_size?: number;
  actions?: Record<string, string[]>;
}

interface DataSyncPreviewPayload {
  source_db_id?: string;
  target_db_id?: string;
  table_name?: string;
  primary_key?: string;
  diffs?: RowData[];
  job_id?: string;
  compare_result?: unknown;
  max_rows?: number;
  actions?: string[];
}

interface DataSyncDeployPayload {
  source_db_id?: string;
  target_db_id?: string;
  dml_statements?: string[];
  job_id?: string;
  preview_result?: unknown;
  actions?: Record<string, string[]>;
}

interface ExportJobPayload {
  table_name: string;
  export_type: string;
  db_id?: string;
  where_clause?: string | null;
  primary_key?: string | null;
  pk_start?: string | null;
  pk_end?: string | null;
  window_limit?: number | null;
  window_offset?: number | null;
}

interface GoLiveJobPayload {
  dml: string;
  db_id?: string;
  compare_based?: boolean;
}

interface ImportJobPayload {
  table_name: string;
  data: RowData[];
  mapping: Record<string, string>;
  skip_errors: boolean;
  db_id?: string;
}

interface ImportSqlJobPayload {
  sql_content?: string;
  sql?: string;
  db_id?: string;
  force?: boolean;
}

interface PerfSyncStartPayload {
  source_db_id?: string;
  target_db_id?: string;
  tables?: string[];
  primary_key?: string;
  tier?: string;
  chunk_size?: number;
  max_rows?: number;
  expected_rows?: Record<string, number>;
  loadgen?: {
    tier: string;
    fill: boolean;
    reset: boolean;
    inject: boolean;
    seed: number;
    batch: number;
  };
}

interface PerfSyncCheckPayload {
  source_db_id?: string;
  target_db_id?: string;
  tables?: string[];
  primary_key?: string;
  tier?: string;
}

export type EmptyStringPolicy = 'preserve' | 'to_null' | 'block'

export interface FieldTargetColumn {
  name: string
  source_key?: string | null
  sql_type: string
  nullable: boolean
  required: boolean
  note?: string | null
}

export interface FieldDiffPage {
  phase: string
  total: number
  page_size: number
  pk_column: string
  last_pk?: string | null
  next_last_pk?: string | null
  has_more: boolean
  pks: string[]
}

// ── 字段回填（S2 执行链路） ──────────────────────────────

export type MigrationStatus =
  | 'pending'
  | 'running'
  | 'paused'
  | 'interrupted'
  | 'blocked'
  | 'completed'
  | 'aborted'

export type MigrationPhase =
  | 'pending'
  | 'preflight'
  | 'backfilling'
  | 'verifying'
  | 'completed'
  | 'paused'
  | 'interrupted'
  | 'blocked'
  | 'aborted'

export interface FieldBackfillConfig {
  batch_size: number
  batch_interval_ms: number
  max_rows_per_second: number
}

export interface FieldBatchVerify {
  source_rows: number
  target_rows: number
  v2: number
  v3: number
  v4: number
}

export interface FieldCheckpoint {
  completed_batches: number
  last_pk?: string | null
  written_rows: number
  verified: boolean
  verify?: FieldBatchVerify | null
  updated_at: number
}

export interface FieldBatchRecord {
  batch_no: number
  last_pk?: string | null
  end_pk: string
  source_rows: number
  written_rows: number
  verified: boolean
  verify: FieldBatchVerify
  duration_ms: number
  at: number
}

export interface FieldDiffDetail {
  phase: string
  total: number
  pks: string[]
  has_more: boolean
}

export interface FieldBlockingDetail {
  batch_no: number
  last_pk?: string | null
  end_pk?: string | null
  phase: string
  total_diffs: number
  pks: string[]
  has_more: boolean
  differences: FieldDiffDetail[]
  verify?: FieldBatchVerify | null
  message: string
}

export interface FieldMigrationProgress {
  expected_rows: number
  processed_rows: number
  success_rows: number
  failed_rows: number
  remaining_rows: number
  total_batches: number
  current_batch: number
  resume_after_pk?: string | null
  current_first_pk?: string | null
  last_pk?: string | null
  connection_id?: number | null
}

export interface FieldMigrationJob {
  job_id: string
  plan_id: string
  plan_hash: string
  migration_hash: string
  approval_id?: string | null
  db_id: string
  db_name: string
  db_fingerprint: string
  spec: {
    source_table: string
    source_column: string
    target_table: string
    pk_column: string
    pk_kind?: 'integer' | 'text' | null
    target_columns: FieldTargetColumn[]
    empty_string_policy: EmptyStringPolicy
    create_table_sql?: string | null
  }
  config: FieldBackfillConfig
  status: MigrationStatus
  phase: MigrationPhase
  production_confirmed: boolean
  checkpoint?: FieldCheckpoint | null
  progress: FieldMigrationProgress
  blocking?: FieldBlockingDetail | null
  recent_batches: FieldBatchRecord[]
  source_schema_hash: string
  target_schema_hash: string
  target_created_by_job: boolean
  expected_rows: number
  created_at: number
  updated_at: number
  started_at: number
  finished_at?: number | null
  last_error?: string | null
  run_count: number
}

export interface FieldBackfillOptionsPayload {
  batch_size?: number
  batch_interval_ms?: number
  max_rows_per_second?: number
  production_confirmed?: boolean
  confirmed_high_risks?: string[]
}

export interface FieldBackfillApproveResponse {
  approval_id: string
  token: string
  expires_at: number
  migration_hash: string
  plan_id: string
  plan_hash: string
  config: FieldBackfillConfig
  high_risks: Array<{ id: string; code: string; title: string; detail: string }>
}

interface TransferConfigPayload {
  source_type: string;
  source_path?: string | null;
  source_db_id?: string | null;
  source_table?: string | null;
  target_db_id?: string | null;
  target_url?: string;
  target_table?: string;
  mode?: string;
  mappings?: unknown[];
  column_mapping?: Record<string, string>;
  skip_errors?: boolean;
}

export const api = {
  getConfig: () => client.get('/config').then(res => res.data),
  updateConfig: (config: ConfigData) => client.post('/config', config).then(res => res.data),
  dbTest: (payload: {
    host?: string
    port?: number
    username?: string
    password?: string
    db_url?: string
    ssl_mode?: string
    ssh_enabled?: boolean
    ssh_host?: string
    ssh_port?: number
    ssh_username?: string
    ssh_password?: string
    probe_capabilities?: boolean
  }) =>
    client.post('/db/test', payload).then(res => res.data),
  perfProbe: (payload: {
    operation?:
      | 'connect_cold'
      | 'connect_warm'
      | 'query_select_small'
      | 'query_write_small'
      | 'explain_plan'
      | 'catalog_first_paint'
      | 'table_first_page'
      | 'cancel_latency'
    db_id?: string
    sql?: string
    table_name?: string
    iterations?: number
  }) => client.post('/diagnostics/perf/probe', payload).then(res => res.data),
  listPerfSuites: (limit?: number) =>
    client.get('/diagnostics/perf/suites', { params: { limit } }).then(res => res.data),
  savePerfSuite: (suite: Record<string, unknown>) =>
    client.post('/diagnostics/perf/suites', suite).then(res => res.data),
  getPerfSuite: (suiteId: string) =>
    client.get(`/diagnostics/perf/suites/${encodeURIComponent(suiteId)}`, {
      headers: { 'x-silent-error': '1' }
    }).then(res => res.data),
  getPerfSuiteBaseline: () =>
    client.get('/diagnostics/perf/suites/baseline').then(res => res.data),
  pinPerfSuiteBaseline: (suite_id: string) =>
    client.post('/diagnostics/perf/suites/baseline', { suite_id }).then(res => res.data),
  listPerfSuiteDiffs: (limit?: number, current_suite_id?: string, baseline_suite_id?: string) =>
    client.get('/diagnostics/perf/suite-diffs', {
      params: { limit, current_suite_id, baseline_suite_id },
      headers: { 'x-silent-error': '1' }
    }).then(res => res.data),
  savePerfSuiteDiff: (report: Record<string, unknown>) =>
    client.post('/diagnostics/perf/suite-diffs', report).then(res => res.data),
  getSchema: (db_id?: string) => client.get('/schema', { params: { db_id } }).then(res => res.data),
  parseSchema: (sqlContent: string) => client.post('/schema/parse', { sql_content: sqlContent }).then(res => res.data),
  chatToSql: (query: string, chatHistory?: ChatMessage[], mode?: string, currentSql?: string) =>
    client.post('/chat', { query, chat_history: chatHistory, mode, current_sql: currentSql }).then(res => res.data),
  chatToSqlStream: async (query: string, chatHistory?: ChatMessage[], mode?: string, currentSql?: string, signal?: AbortSignal) => {
    const locale = getLocale()
    const baseUrl = await resolveBackendBaseUrl()
    const response = await fetch(`${baseUrl}/chat/stream`, {
      method: 'POST',
      headers: {
        'Content-Type': 'application/json',
        'Accept-Language': locale,
        'x-locale': locale,
      },
      body: JSON.stringify({ query, chat_history: chatHistory, mode, current_sql: currentSql }),
      signal,
    })
    if (!response.ok) {
      const text = await response.text().catch(() => '')
      throw new Error(text || `HTTP ${response.status}`)
    }
    return response.body
  },
  executeSql: (sql: string, force?: boolean, db_id?: string, cancel_token?: string, transaction_id?: string) =>
    runExecuteRequest(sql, force, db_id, undefined, undefined, cancel_token, transaction_id),
  executeSqlChunk: (sql: string, chunk_offset: number, chunk_size: number, db_id?: string, transaction_id?: string) =>
    runExecuteRequest(sql, true, db_id, chunk_offset, chunk_size, undefined, transaction_id),
  executeTransactionAction: (
    action: 'begin' | 'commit' | 'rollback',
    transaction_id: string,
    db_id?: string
  ) =>
    client.post('/execute/transaction', { action, transaction_id, db_id }).then(res => res.data),
  cancelExecution: (cancel_token: string, db_id?: string) =>
    client
      .post('/execute/cancel', { cancel_token, db_id }, { headers: { 'x-silent-error': '1' } })
      .then(res => res.data),
  parseNavicat: (xmlContent: string) => client.post('/navicat/parse', { xml_content: xmlContent }).then(res => res.data),
  // Rules
  getRules: () => client.get('/rules').then(res => res.data),
  saveRule: (prompt: string, sql: string) => client.post('/rules/save', { prompt, sql }).then(res => res.data),
  deleteRule: (id: string | number) => client.post('/rules/delete', { id }).then(res => res.data),
  // Policy
  getPolicy: () => client.get('/policy').then(res => res.data),
  resetPolicy: () => client.post('/policy/reset').then(res => res.data),
  snapshotPolicy: () => client.post('/policy/snapshot').then(res => res.data),
  rollbackPolicy: (name: string) => client.post('/policy/rollback', { name }).then(res => res.data),
  // Tables
  getTableData: (tableName: string, page: number, pageSize: number, filters?: string, orders?: string, db_id?: string) =>
    client.get(`/table/data`, { params: { table_name: tableName, page, page_size: pageSize, filters, orders, db_id } }).then(res => res.data),
  getTableSchema: (tableName: string, db_id?: string) =>
    client.get(`/table/schema`, { params: { table_name: tableName, db_id } }).then(res => res.data),
  previewDdl: (oldTable: TableWithDetails | null, newTable: TableWithDetails | null) =>
    client.post('/table/ddl/preview', { old_table: oldTable, new_table: newTable }).then(res => res.data),
  executeDdl: (sql: string, db_id?: string) => 
    client.post('/table/ddl', { sql, db_id }).then(res => res.data),
  crudInsert: (tableName: string, data: RowData, db_id?: string, transaction_id?: string) =>
    client.post('/crud/insert', { table_name: tableName, data, db_id, transaction_id }).then(res => res.data),
  crudUpdate: (tableName: string, data: RowData, condition: RowData, db_id?: string, transaction_id?: string) =>
    client.post('/crud/update', { table_name: tableName, data, condition, db_id, transaction_id }).then(res => res.data),
  crudDelete: (tableName: string, condition: RowData, db_id?: string, transaction_id?: string) =>
    client.post('/crud/delete', { table_name: tableName, condition, db_id, transaction_id }).then(res => res.data),
  generateMockData: (tableName: string, rowCount: number, rules?: Record<string, string>, db_id?: string) => 
    client.post('/tools/mock-data', { table_name: tableName, row_count: rowCount, rules, db_id }).then(res => res.data),
  exportData: (tableName: string, exportType: string, db_id?: string) => 
    client.post('/tools/export', { table_name: tableName, export_type: exportType, db_id }, { responseType: 'text' }).then(res => res.data),
  importData: (tableName: string, data: RowData[], mapping: Record<string, string>, skipErrors: boolean, db_id?: string) =>
    client.post('/tools/import', { table_name: tableName, data, mapping, skip_errors: skipErrors, db_id }).then(res => res.data),
  exportJobStart: (payload: ExportJobPayload) =>
    client.post('/tools/jobs/export/start', payload, { headers: { 'x-silent-error': '1' } }).then(res => res.data),
  goLiveJobStart: (payload: GoLiveJobPayload) =>
    client.post('/tools/jobs/go-live/start', payload, { headers: { 'x-silent-error': '1' } }).then(res => res.data),
  importJobStart: (payload: ImportJobPayload) =>
    client.post('/tools/jobs/import/start', payload, { headers: { 'x-silent-error': '1' } }).then(res => res.data),
  importSqlJobStart: (payload: ImportSqlJobPayload) =>
    client.post('/tools/jobs/import-sql/start', payload, { headers: { 'x-silent-error': '1' } }).then(res => res.data),
  toolJobStatus: (job_id: string) =>
    client.get(`/tools/jobs/${encodeURIComponent(job_id)}`, { headers: { 'x-silent-error': '1' }, timeout: JOB_POLL_REQUEST_TIMEOUT_MS }).then(res => res.data),
  toolJobCancel: (job_id: string) =>
    client.post(`/tools/jobs/${encodeURIComponent(job_id)}/cancel`, null, { headers: { 'x-silent-error': '1' } }).then(res => res.data),
  toolJobArtifactData: async (job_id: string, artifact: string = 'data') => {
    const res = await client.get(
      `/tools/jobs/${encodeURIComponent(job_id)}/artifacts/${encodeURIComponent(artifact)}`,
      { headers: { 'x-silent-error': '1' }, responseType: 'text' }
    )
    const text = res.data
    if (typeof text === 'string') {
      const t = text.trim()
      if (!t) return null
      try {
        return JSON.parse(t)
      } catch {
        return t
      }
    }
    return text
  },
  goLiveReports: (limit?: number) =>
    client.get('/tools/go-live/reports', { params: { limit }, headers: { 'x-silent-error': '1' } }).then(res => res.data),
  goLiveAudit: (limit?: number) =>
    client.get('/tools/go-live/audit', { params: { limit }, headers: { 'x-silent-error': '1' } }).then(res => res.data),
  // Structure Sync API
  syncSchemaDiff: (source_db_id: string, target_db_id: string) => 
    client.post('/tools/schema-sync/diff', { source_db_id, target_db_id }).then(res => res.data.diff),
  syncSchemaDdl: (source_db_id: string, target_db_id: string, selected_tables: string[]) => 
    client.post('/tools/schema-sync/ddl', { source_db_id, target_db_id, selected_tables }).then(res => res.data.ddl_statements),
  // Data Sync API
  syncDataDiff: (table_name: string, source_db_id: string, target_db_id: string, primary_key: string) => 
    client.post('/tools/data-sync/diff', { table_name, source_db_id, target_db_id, primary_key }).then(res => res.data.diff),
  syncDataDml: (diffs: RowData[], selections: Record<string, string[]>, primary_key: string) =>
    client.post('/tools/data-sync/dml', { diffs, selections, primary_key }).then(res => res.data.dml_statements),
  dataSyncCompareStart: (payload: DataSyncComparePayload) =>
    client.post('/tools/data-sync/compare', payload, { headers: { 'x-silent-error': '1' } }).then(res => res.data),
  dataSyncPreviewStart: (payload: DataSyncPreviewPayload) =>
    client.post('/tools/data-sync/preview', payload, { headers: { 'x-silent-error': '1' } }).then(res => res.data),
  dataSyncDeployStart: (payload: DataSyncDeployPayload) =>
    client.post('/tools/data-sync/deploy', payload, { headers: { 'x-silent-error': '1' } }).then(res => res.data),
  dataSyncJobStatus: async (job_id: string) => {
    try {
      return await client.get(`/tools/data-sync/jobs/${encodeURIComponent(job_id)}`, { headers: { 'x-silent-error': '1' }, timeout: JOB_POLL_REQUEST_TIMEOUT_MS }).then(res => res.data)
    } catch (e: unknown) {
      const axiosError = e as { response?: { status?: number } }
      if (axiosError?.response?.status === 404) {
        return await client.get(`/jobs/${encodeURIComponent(job_id)}`, { headers: { 'x-silent-error': '1' }, timeout: JOB_POLL_REQUEST_TIMEOUT_MS }).then(res => res.data)
      }
      throw e
    }
  },
  // Perf Sync API
  perfSyncStart: (payload: PerfSyncStartPayload) =>
    client.post('/tools/perf-sync/start', payload, { headers: { 'x-silent-error': '1' } }).then(res => res.data),
  perfSyncCheck: (payload: PerfSyncCheckPayload) =>
    client.post('/tools/perf-sync/check', payload, { headers: { 'x-silent-error': '1' } }).then(res => res.data),
  perfSyncJobStatus: (job_id: string) =>
    client.get(`/tools/perf-sync/jobs/${encodeURIComponent(job_id)}`, { headers: { 'x-silent-error': '1' }, timeout: JOB_POLL_REQUEST_TIMEOUT_MS }).then(res => res.data),
  // Data Transfer API
  transferUpload: (formData: FormData) =>
    client.post('/tools/data-transfer/upload', formData, { headers: { 'Content-Type': 'multipart/form-data' } }).then(res => res.data),
  transferExecute: (config: TransferConfigPayload) =>
    client.post('/tools/data-transfer/execute', config).then(res => res.data),
  // History
  getHistory: () => client.get('/sql/history').then(res => res.data),
  clearHistory: () => client.post('/sql/history').then(res => res.data),
  // Explain
  explainSql: (sql: string) => client.post('/sql/explain', { sql }).then(res => res.data),
  getSessionInfo: (db_id?: string) =>
    client.get('/sql/session-info', {
      params: { db_id },
      headers: { 'x-silent-error': '1' }
    }).then(res => res.data),
  // AI Connection Manager
  getAiModels: () => client.get('/api/ai/models').then(res => res.data),
  fetchProviderModels: (provider: string, apiKey: string, baseUrl?: string) => 
    client.post('/api/ai/provider/models', { provider, api_key: apiKey, base_url: baseUrl }).then(res => res.data.models),
  getAiHealth: () => client.get('/api/ai/health').then(res => res.data),
  // AI Agents
  aiQuery: (payload: AiQueryPayload | string) => {
    const request = typeof payload === 'string' ? { query: payload } : payload
    return client.post('/api/ai/query', request).then(res => res.data)
  },
  aiExplainError: (errorMsg: string, failedQuery: string) => client.post('/api/ai/explain_error', { error_msg: errorMsg, failed_query: failedQuery }).then(res => res.data),
  // AI Knowledge Base
  getKnowledge: (dbConnectionId?: string) => client.get('/api/ai/knowledge', { params: { db_connection_id: dbConnectionId } }).then(res => res.data),
  addKnowledge: (knowledge: Partial<KnowledgeItem>) => client.post('/api/ai/knowledge', knowledge).then(res => res.data),
  updateKnowledge: (knowledge: Partial<KnowledgeItem>) => client.put('/api/ai/knowledge', knowledge).then(res => res.data),
  deleteKnowledge: (id: string | number) => client.post('/api/ai/knowledge/delete', { id }).then(res => res.data),
  // Ops Center (阶段 0：MySQL 单表索引优化)
  opsScenarios: () => client.get('/ops/scenarios').then(res => res.data),
  opsSessionToken: () => getOpsToken(true),
  opsGetPolicy: () => client.get('/ops/policy').then(res => res.data),
  opsUpdatePolicy: async (policy: Record<string, unknown>) =>
    client.post('/ops/policy', policy, { headers: await opsWriteHeaders() }).then(res => res.data),
  opsAnalyze: async (payload: {
    scenario_id?: string
    db_id?: string
    table?: string
    workload_sql?: string[]
    sql?: string
    notes?: string
    use_llm?: boolean
    digest?: string
    sample_sql?: string
    explain_analyze?: boolean
    explain_analyze_timeout_ms?: number
    // 字段治理（只读分析）：源列、目标映射、语义声明、剖面模式
    column?: string
    target?: Record<string, unknown>
    declarations?: Array<Record<string, unknown>>
    profile_mode?: string
    sample_per_mille?: number
    empty_string_policy?: EmptyStringPolicy
  }) => client.post('/ops/analyze', payload, { headers: await opsWriteHeaders() }).then(res => res.data),
  // 字段治理 V2/V3/V4 差异主键分页（只读）
  opsFieldDiffPage: async (payload: {
    db_id?: string
    table: string
    column: string
    target_table: string
    pk_column?: string
    target_columns: FieldTargetColumn[]
    empty_string_policy?: EmptyStringPolicy
    phase: 'v2' | 'v3' | 'v4'
    last_pk?: string | null
    page_size?: number
  }) => client.post('/ops/field-governance/diff-page', payload, { headers: await opsWriteHeaders() }).then(res => res.data as FieldDiffPage),
  // 字段回填执行（S2）：审批令牌 → 启动 → 进度/暂停/继续/中止
  opsFieldBackfillApprove: async (payload: {
    plan_id: string
    plan_hash?: string
  } & FieldBackfillOptionsPayload) =>
    client.post('/ops/field-governance/backfill/approve', payload, { headers: await opsWriteHeaders() })
      .then(res => res.data as FieldBackfillApproveResponse),
  opsFieldBackfillStart: async (payload: {
    plan_id: string
    approval_token: string
    plan_hash?: string
  } & FieldBackfillOptionsPayload) =>
    client.post('/ops/field-governance/backfill', payload, { headers: await opsWriteHeaders() })
      .then(res => res.data as FieldMigrationJob),
  opsFieldBackfillList: async (params: { plan_id?: string; limit?: number }) =>
    client.get('/ops/field-governance/backfill', { params, headers: { 'x-silent-error': '1' } })
      .then(res => res.data as { jobs: FieldMigrationJob[] }),
  opsFieldBackfillStatus: async (jobId: string) =>
    client.get(`/ops/field-governance/backfill/${encodeURIComponent(jobId)}`, {
      headers: { 'x-silent-error': '1' },
      timeout: JOB_POLL_REQUEST_TIMEOUT_MS,
    }).then(res => res.data as FieldMigrationJob),
  opsFieldBackfillPause: async (jobId: string) =>
    client.post(`/ops/field-governance/backfill/${encodeURIComponent(jobId)}/pause`, {}, {
      headers: await opsWriteHeaders(),
    }).then(res => res.data as FieldMigrationJob),
  opsFieldBackfillResume: async (jobId: string, approvalToken?: string) =>
    client.post(`/ops/field-governance/backfill/${encodeURIComponent(jobId)}/resume`, approvalToken ? { approval_token: approvalToken } : {}, {
      headers: await opsWriteHeaders(),
    }).then(res => res.data as FieldMigrationJob),
  opsFieldBackfillAbort: async (jobId: string) =>
    client.post(`/ops/field-governance/backfill/${encodeURIComponent(jobId)}/abort`, {}, {
      headers: await opsWriteHeaders(),
    }).then(res => res.data as FieldMigrationJob),
  opsSlowQueries: async (params: { db_id?: string; sort?: string; limit?: number }) =>
    client.get('/ops/slow-queries', { params, headers: await opsWriteHeaders() }).then(res => res.data),
  opsListPlans: (limit = 20) => client.get('/ops/plans', { params: { limit } }).then(res => res.data),
  opsGetPlan: (planId: string) => client.get(`/ops/plans/${encodeURIComponent(planId)}`).then(res => res.data),
  opsApprove: async (planId: string, payload: { plan_hash: string; action_ids: string[] }) =>
    client.post(`/ops/plans/${encodeURIComponent(planId)}/approve`, payload, { headers: await opsWriteHeaders() }).then(res => res.data),
  opsExecute: async (planId: string, payload: { action_ids?: string[]; approval_token?: string; dry_run?: boolean }) =>
    client.post(`/ops/plans/${encodeURIComponent(planId)}/execute`, payload, {
      headers: await opsWriteHeaders(),
      timeout: HTTP_TIMEOUT_MS * 2,
    }).then(res => res.data),
  opsRollback: async (planId: string, reason: string) =>
    client.post(`/ops/plans/${encodeURIComponent(planId)}/rollback`, { reason }, {
      headers: await opsWriteHeaders(),
      timeout: HTTP_TIMEOUT_MS * 2,
    }).then(res => res.data),
  opsAudit: (limit = 100) => client.get('/ops/audit', { params: { limit } }).then(res => res.data),
  opsApprovals: () => client.get('/ops/approvals').then(res => res.data),
  // Ops Center · 定期巡检（只读）
  opsInspectionPresets: () => client.get('/ops/inspection/presets').then(res => res.data),
  opsInspectionGetSchedule: () => client.get('/ops/inspection/schedule').then(res => res.data),
  opsInspectionSaveSchedule: async (payload: {
    enabled: boolean
    mode: 'interval' | 'cron'
    interval_minutes: number
    cron_expr: string
    preset_id: string
    check_ids?: string[]
    db_id?: string
  }) =>
    client.post('/ops/inspection/schedule', payload, { headers: await opsWriteHeaders() }).then(res => res.data),
  opsInspectionRun: async (payload: {
    db_id?: string
    preset_id?: string
    check_ids?: string[]
    thresholds?: Record<string, number>
  }) =>
    client.post('/ops/inspection/run', payload, {
      headers: await opsWriteHeaders(),
      timeout: HTTP_TIMEOUT_MS * 2,
    }).then(res => res.data),
  opsInspectionLatest: (params?: { db_id?: string; limit?: number; include_resolved?: boolean }) =>
    client.get('/ops/inspection/latest', { params }).then(res => res.data),
  opsInspectionHistory: (params?: { db_id?: string; limit?: number }) =>
    client.get('/ops/inspection/history', { params }).then(res => res.data),
  opsInspectionAlerts: (params?: { db_id?: string; limit?: number; include_resolved?: boolean }) =>
    client.get('/ops/inspection/alerts', { params }).then(res => res.data),
  opsInspectionAckAlert: async (alertId: string) =>
    client.post(`/ops/inspection/alerts/${encodeURIComponent(alertId)}/ack`, {}, {
      headers: await opsWriteHeaders(),
    }).then(res => res.data),
}
