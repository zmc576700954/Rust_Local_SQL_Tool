import * as monaco from 'monaco-editor';

export type MonacoEditor = monaco.editor.IStandaloneCodeEditor;
export type Monaco = typeof monaco;

export interface TableInfo {
  table_name: string;
  table_comment?: string | null;
}

export interface AppError {
  message?: string;
  title?: string;
  solution?: string;
  [key: string]: unknown;
}

export interface ColumnInfo {
  column_name: string;
  data_type: string;
  column_type: string;
  is_nullable: string;
  column_comment?: string | null;
  column_key?: string;
  column_default?: string | null;
  extra?: string;
}

export interface IndexInfo {
  index_name: string;
  column_name: string;
  non_unique: number;
  index_type?: string;
}

export interface ForeignKeyInfo {
  constraint_name: string;
  column_name: string;
  referenced_table_name: string;
  referenced_column_name: string;
  update_rule?: string;
  delete_rule?: string;
}

export interface TableWithDetails {
  table_name: string;
  columns: ColumnInfo[];
  indexes: IndexInfo[];
  foreign_keys: ForeignKeyInfo[];
}

export interface ViewInfo {
  view_name: string;
}

export interface SchemaResponse {
  db_name: string;
  tables: TableWithDetails[];
  views: ViewInfo[];
}

export interface QueryExecutionResult {
  columns?: string[];
  rows: any[];
  execution_time_ms?: number;
  row_count?: number;
  affected_rows?: number;
  has_more?: boolean;
  next_offset?: number | null;
  chunk_offset?: number;
  chunk_size?: number;
  preview_cap?: number | null;
  truncated?: boolean;
  source_sql?: string;
  statement_index?: number;
  statement_label?: string;
  statement_kind?: string;
  status?: 'success' | 'error' | 'canceled';
  error?: {
    title: string;
    message: string;
    solution: string;
  } | null;
}

export interface QueryResultCompareSummary {
  baseline_row_count: number;
  current_row_count: number;
  added_count: number;
  removed_count: number;
  unchanged_count: number;
}

export interface QueryResultCompareReport {
  baseline_statement_label?: string | null;
  current_statement_label?: string | null;
  baseline_source_sql?: string | null;
  current_source_sql?: string | null;
  baseline_execution_time_ms?: number;
  current_execution_time_ms?: number;
  compared_at: number;
  summary: QueryResultCompareSummary;
  added_rows: any[];
  removed_rows: any[];
}

export interface QueryErrorInsight {
  source_sql: string;
  error_message: string;
  explanation: string;
  fixed_sql?: string | null;
  statement_label?: string | null;
  statement_kind?: string | null;
  generated_at: number;
}

export interface SessionInfoEntry {
  key: string;
  value: string | null;
}

export interface WorkbenchSessionInfo {
  db_id?: string | null;
  db_name?: string | null;
  connection_name?: string | null;
  read_only: boolean;
  fetched_at: number;
  summary: SessionInfoEntry[];
  session_variables: SessionInfoEntry[];
  global_variables: SessionInfoEntry[];
}

export interface AiRule {
  id: number;
  rule_type: string;
  rule_content: string;
  is_active: boolean;
  priority: number;
  description?: string;
  prompt_pattern?: string;
  sql_template?: string;
  hit_count?: number;
}

export interface KnowledgeItem {
  id: string | number;
  knowledge_type?: 'ddl' | 'documentation' | 'sql';
  db_connection_id?: string | null;
  title: string;
  content: string;
  description?: string | null;
  updated_at?: number;
  is_golden: boolean;
}

export interface SavedSqlBookmark {
  id: string;
  title: string;
  sql: string;
  description?: string | null;
  db_id?: string | null;
  db_label?: string | null;
  created_at: number;
  updated_at: number;
}

export interface AiPolicy {
  id?: number;
  name: string;
  description: string;
  prompt_template: string;
  is_active: boolean;
}

export interface ConfigData {
  db_url: string;
  active_db_id?: string;
  db_connections?: DbConnection[];
  [key: string]: unknown;
}

export type DbType =
  | 'mysql'
  | 'mariadb'
  | 'postgresql'
  | 'sqlite'
  | 'sqlserver'
  | 'mongodb'
  | 'redis'
  | 'oracle'

export type DbCapabilityLevel = 'a' | 'b' | 'c' | 'd'

export type DbConnectionSchema =
  | { type: 'mysql'; url: string }
  | { type: 'mariadb'; url: string }
  | { type: 'postgresql'; url: string }
  | { type: 'sqlite'; url: string }
  | { type: 'sqlserver'; url: string }
  | { type: 'mongodb'; url: string }
  | { type: 'redis'; url: string }
  | { type: 'oracle'; url: string }

export interface DbConnection {
  id: string;
  name?: string;
  url: string;
  group_name?: string | null;
  color?: string | null;
  is_favorite?: boolean;
  ssh?: {
    enabled?: boolean;
    host?: string;
    port?: number;
    username?: string;
    password?: string;
  } | null;
  ssl?: {
    enabled?: boolean;
    mode?: string;
  } | null;
  db_type?: DbType;
  capability_level?: DbCapabilityLevel;
  schema?: DbConnectionSchema;
  is_read_only?: boolean;
  security_profile?: {
    users?: Array<{
      username: string;
      host: string;
      role: string;
      status: 'active' | 'disabled';
    }>;
    object_permissions?: Array<{
      object_type: 'table' | 'view' | 'procedure';
      object_name: string;
      username: string;
      privileges: string[];
    }>;
  };
  [key: string]: unknown;
}

export type DataSyncStrategy = 'mirror' | 'upsert_only'

export interface DataDiff {
  table_name: string
  insert_count: number
  update_count: number
  delete_count: number
  inserts?: any[]
  updates?: any[]
  deletes?: any[]
}

export type JobStatus = 'idle' | 'queued' | 'running' | 'succeeded' | 'failed' | 'canceled'

export interface DataSyncJobStatus {
  job_id: string
  phase?: 'compare' | 'preview' | 'deploy'
  status: JobStatus
  progress?: number
  message?: string
  result?: any
  error?: any
}

export interface PerfSyncJobStatus {
  job_id: string
  status: JobStatus
  progress?: number
  message?: string
  result?: any
  error?: any
}

export interface PerfSyncCheckResult {
  tier: string
  expected_rows: Record<string, number>
  baseline_counts: Record<string, { source: number; target: number }>
  insufficient: Array<{ table_name: string; expected: number; source: number; target: number }>
  fill_plan?: Array<{
    table_name: string
    expected: number
    source_current: number
    target_current: number
    source_fill: number
    target_fill: number
  }>
}
