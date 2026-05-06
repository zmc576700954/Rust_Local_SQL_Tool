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
  columns: string[];
  rows: any[];
  execution_time_ms?: number;
  row_count: number;
  affected_rows?: number;
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
  url: string;
  db_type?: DbType;
  capability_level?: DbCapabilityLevel;
  schema?: DbConnectionSchema;
  is_read_only?: boolean;
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
