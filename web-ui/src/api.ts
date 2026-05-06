import axios from 'axios'
import { getErrorMessage } from './utils/ErrorDictionary'
import { redactSensitiveText } from './utils'

export const HTTP_TIMEOUT_MS = 120000
export const JOB_POLL_REQUEST_TIMEOUT_MS = 10000
export const JOB_POLL_INTERVAL_MS = 1200

const client = axios.create({
  baseURL: '/backend',
  timeout: HTTP_TIMEOUT_MS,
})

client.interceptors.response.use(
  (response) => {
    return response;
  },
  (error) => {
    const silent = error?.config?.headers?.['x-silent-error'] === '1';
    let message = "网络错误，请稍后重试。";
    const status = error?.response?.status as number | undefined;
    const data = error?.response?.data as any | undefined;

    if (data) {
      const { code, message: serverMessage, details } = data;
      if (code) {
        const merged = details ? `${serverMessage || "发生未知错误"}（${details}）` : (serverMessage || "发生未知错误");
        message = getErrorMessage(code, merged);
      } else if (serverMessage) {
        message = details ? `${serverMessage}（${details}）` : serverMessage;
      }
    } else if (status) {
      if (status === 401) message = "未授权（401）：请检查登录状态或 API Key。";
      if (status === 403) message = "禁止访问（403）：请检查权限或服务商风控。";
      if (status === 404) message = "接口不存在（404）：请确认后端已更新并重启。";
    }
    
    if (!silent) {
      window.dispatchEvent(new CustomEvent('global-toast', {
        detail: { message: redactSensitiveText(message), type: 'error' }
      }));
    }
    
    return Promise.reject(error);
  }
)

export const api = {
  getConfig: () => client.get('/config').then(res => res.data),
  updateConfig: (config: any) => client.post('/config', config).then(res => res.data),
  dbTest: (payload: { host: string; port?: number; username: string; password: string }) =>
    client.post('/db/test', payload).then(res => res.data),
  getSchema: () => client.get('/schema').then(res => res.data),
  parseSchema: (sqlContent: string) => client.post('/schema/parse', { sql_content: sqlContent }).then(res => res.data),
  chatToSql: (query: string, chatHistory?: any[]) => client.post('/chat', { query, chat_history: chatHistory }).then(res => res.data),
  executeSql: (sql: string, force?: boolean) => client.post('/execute', { sql, force }).then(res => res.data),
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
  getTableData: (tableName: string, page: number, pageSize: number, filters?: string, orders?: string) => 
    client.get(`/table/data`, { params: { table_name: tableName, page, page_size: pageSize, filters, orders } }).then(res => res.data),
  getTableSchema: (tableName: string) => 
    client.get(`/table/schema?table_name=${tableName}`).then(res => res.data),
  previewDdl: (oldTable: any | null, newTable: any | null) =>
    client.post('/table/ddl/preview', { old_table: oldTable, new_table: newTable }).then(res => res.data),
  executeDdl: (sql: string) => 
    client.post('/table/ddl', { sql }).then(res => res.data),
  crudInsert: (tableName: string, data: any) => 
    client.post('/crud/insert', { table_name: tableName, data }).then(res => res.data),
  crudUpdate: (tableName: string, data: any, condition: Record<string, any>) => 
    client.post('/crud/update', { table_name: tableName, data, condition }).then(res => res.data),
  crudDelete: (tableName: string, condition: Record<string, any>) => 
    client.post('/crud/delete', { table_name: tableName, condition }).then(res => res.data),
  generateMockData: (tableName: string, rowCount: number, rules?: Record<string, string>) => 
    client.post('/tools/mock-data', { table_name: tableName, row_count: rowCount, rules }).then(res => res.data),
  exportData: (tableName: string, exportType: string) => 
    client.post('/tools/export', { table_name: tableName, export_type: exportType }, { responseType: 'text' }).then(res => res.data),
  importData: (tableName: string, data: any[], mapping: Record<string, string>, skipErrors: boolean) => 
    client.post('/tools/import', { table_name: tableName, data, mapping, skip_errors: skipErrors }).then(res => res.data),
  exportJobStart: (payload: any) =>
    client.post('/tools/jobs/export/start', payload, { headers: { 'x-silent-error': '1' } }).then(res => res.data),
  goLiveJobStart: (payload: any) =>
    client.post('/tools/jobs/go-live/start', payload, { headers: { 'x-silent-error': '1' } }).then(res => res.data),
  importJobStart: (payload: any) =>
    client.post('/tools/jobs/import/start', payload, { headers: { 'x-silent-error': '1' } }).then(res => res.data),
  importSqlJobStart: (payload: any) =>
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
  syncDataDml: (diffs: any[], selections: Record<string, string[]>, primary_key: string) => 
    client.post('/tools/data-sync/dml', { diffs, selections, primary_key }).then(res => res.data.dml_statements),
  dataSyncCompareStart: (payload: any) =>
    client.post('/tools/data-sync/compare', payload, { headers: { 'x-silent-error': '1' } }).then(res => res.data),
  dataSyncPreviewStart: (payload: any) =>
    client.post('/tools/data-sync/preview', payload, { headers: { 'x-silent-error': '1' } }).then(res => res.data),
  dataSyncDeployStart: (payload: any) =>
    client.post('/tools/data-sync/deploy', payload, { headers: { 'x-silent-error': '1' } }).then(res => res.data),
  dataSyncJobStatus: async (job_id: string) => {
    try {
      return await client.get(`/tools/data-sync/jobs/${encodeURIComponent(job_id)}`, { headers: { 'x-silent-error': '1' }, timeout: JOB_POLL_REQUEST_TIMEOUT_MS }).then(res => res.data)
    } catch (e: any) {
      if (e?.response?.status === 404) {
        return await client.get(`/jobs/${encodeURIComponent(job_id)}`, { headers: { 'x-silent-error': '1' }, timeout: JOB_POLL_REQUEST_TIMEOUT_MS }).then(res => res.data)
      }
      throw e
    }
  },
  // Perf Sync API
  perfSyncStart: (payload: any) =>
    client.post('/tools/perf-sync/start', payload, { headers: { 'x-silent-error': '1' } }).then(res => res.data),
  perfSyncCheck: (payload: any) =>
    client.post('/tools/perf-sync/check', payload, { headers: { 'x-silent-error': '1' } }).then(res => res.data),
  perfSyncJobStatus: (job_id: string) =>
    client.get(`/tools/perf-sync/jobs/${encodeURIComponent(job_id)}`, { headers: { 'x-silent-error': '1' }, timeout: JOB_POLL_REQUEST_TIMEOUT_MS }).then(res => res.data),
  // Data Transfer API
  transferUpload: (formData: FormData) =>
    client.post('/tools/data-transfer/upload', formData, { headers: { 'Content-Type': 'multipart/form-data' } }).then(res => res.data),
  transferExecute: (config: any) =>
    client.post('/tools/data-transfer/execute', config).then(res => res.data.dml),
  // History
  getHistory: () => client.get('/sql/history').then(res => res.data),
  clearHistory: () => client.post('/sql/history').then(res => res.data),
  // Explain
  explainSql: (sql: string) => client.post('/sql/explain', { sql }).then(res => res.data),
  // AI Connection Manager
  getAiModels: () => client.get('/api/ai/models').then(res => res.data),
  fetchProviderModels: (provider: string, apiKey: string, baseUrl?: string) => 
    client.post('/api/ai/provider/models', { provider, api_key: apiKey, base_url: baseUrl }).then(res => res.data.models),
  getAiHealth: () => client.get('/api/ai/health').then(res => res.data),
  // AI Agents
  aiQuery: (query: string) => client.post('/api/ai/query', { query }).then(res => res.data),
  aiExplainError: (errorMsg: string, failedQuery: string) => client.post('/api/ai/explain_error', { error_msg: errorMsg, failed_query: failedQuery }).then(res => res.data),
  // AI Knowledge Base
  getKnowledge: (dbConnectionId?: string) => client.get('/api/ai/knowledge', { params: { db_connection_id: dbConnectionId } }).then(res => res.data),
  addKnowledge: (knowledge: any) => client.post('/api/ai/knowledge', knowledge).then(res => res.data),
  updateKnowledge: (knowledge: any) => client.put('/api/ai/knowledge', knowledge).then(res => res.data),
  deleteKnowledge: (id: string | number) => client.post('/api/ai/knowledge/delete', { id }).then(res => res.data),
}
