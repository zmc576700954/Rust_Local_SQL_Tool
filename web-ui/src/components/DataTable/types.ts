/** Type definitions for the DataTable component. */

export interface DataTableProps {
  data: any[];
  schema: any;
  tableName: string;
  dbId?: string;
  transactionId?: string | null;
  onTransactionStateChange?: (state: 'active' | 'idle') => void;
  sorts: { column: string; desc: boolean }[];
  setSorts: (sorts: { column: string; desc: boolean }[]) => void;
  filters: { column: string; operator: string; value: string }[];
  setFilters: (filters: { column: string; operator: string; value: string }[]) => void;
  onRefresh: () => void;
  isActive: boolean;
  isRefreshing: boolean;
  refreshError?: string | null;
  dataRevision: number;
}

export type PreviewPayload = {
  title: string;
  value: string;
  draft: string;
  format: 'text' | 'json';
  downloadExtension: 'txt' | 'json';
  rowIdx: number;
  col: string;
  isNew: boolean;
  originalValue: unknown;
};

export type SaveReviewUpdatePreview = {
  rowIdx: number;
  rowData: Record<string, any>;
  condition: Record<string, any>;
  changes: Array<{
    column: string;
    before: unknown;
    after: unknown;
  }>;
};

export type SaveReviewDeletePreview = {
  rowIdx: number;
  rowData: Record<string, any>;
  condition: Record<string, any>;
};

export type ValidationIssue = {
  rowIdx: number;
  col: string;
  isNew: boolean;
  message: string;
};

export type StaleRecoveryContext = {
  condition: Record<string, any>;
  originalRowData?: Record<string, any>;
  pendingRowData?: Record<string, any>;
  changedColumns?: string[];
};

export type StaleConflictDiffState = 'awaiting_refresh' | 'conflict' | 'already_applied' | 'local_pending' | 'server_only';

export type StaleConflictDiffItem = {
  column: string;
  originalValue: unknown;
  pendingValue: unknown;
  latestValue: unknown;
  userChanged: boolean;
  serverChanged: boolean;
  state: StaleConflictDiffState;
};

export type SaveFailureItem = {
  action: 'delete' | 'update' | 'insert';
  kind: 'stale_row' | 'duplicate_key' | 'not_null' | 'foreign_key' | 'value_too_long' | 'invalid_value' | 'read_only' | 'generic';
  rowIdx: number;
  isNew: boolean;
  col?: string;
  message: string;
  rawMessage: string;
  summary: string;
  recoveryNote?: string;
  staleRecovery?: StaleRecoveryContext;
  dataRevisionAtFailure?: number;
};

export type SaveAttemptReport = {
  attempted: number;
  succeeded: number;
  failed: number;
  failures: SaveFailureItem[];
};

export type PendingStaleRecoveryState = {
  items: SaveFailureItem[];
  sawRefreshing: boolean;
  sourceDataRevision: number;
};

export type StaleConflictQueueFilter = 'all' | 'high_risk' | 'needs_refresh' | 'safe_edits' | 'delete';
export type StaleConflictQueueSort = 'risk_desc' | 'row_asc' | 'row_desc' | 'conflicts_desc' | 'action';
export type StaleConflictOverviewGroupKey = 'high_risk' | 'needs_refresh' | 'delete' | 'safe_edits' | 'other';
export type StaleConflictOverviewGroupState = Record<StaleConflictOverviewGroupKey, boolean>;
export type StaleConflictOverviewSummary = {
  failure: SaveFailureItem;
  needsRefresh: boolean;
  isHighRisk: boolean;
  isSafeUpdate: boolean;
};
export type StaleConflictReviewScope = {
  failureKeys: string[];
  label: string;
};

export type ColumnLayoutState = {
  order: string[];
  hidden: string[];
  widths: Record<string, number>;
};
