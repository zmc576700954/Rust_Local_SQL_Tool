import { useState, useMemo, useEffect, useRef, useCallback } from 'react';
import { ArrowDown, ArrowUp, Filter, Save, Undo, Plus, Trash2, Copy, Eye, X, ChevronDown, ChevronRight } from 'lucide-react';
import { api } from '../api';
import { useVirtualizer } from '@tanstack/react-virtual';
import { redactSensitiveText } from '../utils'

interface DataTableProps {
  data: any[];
  schema: any;
  tableName: string;
  dbId?: string;
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

type PreviewPayload = {
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

type SaveReviewUpdatePreview = {
  rowIdx: number;
  rowData: Record<string, any>;
  condition: Record<string, any>;
  changes: Array<{
    column: string;
    before: unknown;
    after: unknown;
  }>;
};

type SaveReviewDeletePreview = {
  rowIdx: number;
  rowData: Record<string, any>;
  condition: Record<string, any>;
};

type ValidationIssue = {
  rowIdx: number;
  col: string;
  isNew: boolean;
  message: string;
};

type StaleRecoveryContext = {
  condition: Record<string, any>;
  originalRowData?: Record<string, any>;
  pendingRowData?: Record<string, any>;
  changedColumns?: string[];
};

type StaleConflictDiffState = 'awaiting_refresh' | 'conflict' | 'already_applied' | 'local_pending' | 'server_only';

type StaleConflictDiffItem = {
  column: string;
  originalValue: unknown;
  pendingValue: unknown;
  latestValue: unknown;
  userChanged: boolean;
  serverChanged: boolean;
  state: StaleConflictDiffState;
};

type SaveFailureItem = {
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

type SaveAttemptReport = {
  attempted: number;
  succeeded: number;
  failed: number;
  failures: SaveFailureItem[];
};

type PendingStaleRecoveryState = {
  items: SaveFailureItem[];
  sawRefreshing: boolean;
  sourceDataRevision: number;
};

type StaleConflictQueueFilter = 'all' | 'high_risk' | 'needs_refresh' | 'safe_edits' | 'delete';
type StaleConflictQueueSort = 'risk_desc' | 'row_asc' | 'row_desc' | 'conflicts_desc' | 'action';
type StaleConflictOverviewGroupKey = 'high_risk' | 'needs_refresh' | 'delete' | 'safe_edits' | 'other';
type StaleConflictOverviewGroupState = Record<StaleConflictOverviewGroupKey, boolean>;
type StaleConflictOverviewSummary = {
  failure: SaveFailureItem;
  needsRefresh: boolean;
  isHighRisk: boolean;
  isSafeUpdate: boolean;
};
type StaleConflictReviewScope = {
  failureKeys: string[];
  label: string;
};

type ColumnLayoutState = {
  order: string[];
  hidden: string[];
  widths: Record<string, number>;
};

const DEFAULT_COLUMN_WIDTH = 220;
const APPROX_ROW_HEIGHT = 33;
const SAVE_REVIEW_PREVIEW_LIMIT = 3;
const STALE_CONFLICT_OVERVIEW_GROUP_ORDER: StaleConflictOverviewGroupKey[] = ['high_risk', 'needs_refresh', 'delete', 'safe_edits', 'other'];
const STALE_CONFLICT_OVERVIEW_GROUP_LABELS: Record<StaleConflictOverviewGroupKey, string> = {
  high_risk: 'High Risk',
  needs_refresh: 'Needs Refresh',
  delete: 'Deletes',
  safe_edits: 'Safe Edits',
  other: 'Other',
};
const STALE_CONFLICT_OVERVIEW_GROUP_HINTS: Record<StaleConflictOverviewGroupKey, string> = {
  high_risk: 'Direct conflicts that still need a column-by-column decision.',
  needs_refresh: 'Refresh before you can safely rebase or retry.',
  delete: 'Delete retries and latest-server-copy decisions.',
  safe_edits: 'Safe stale edits that can be batch merged quickly.',
  other: 'Remaining stale items on the current page.',
};
const STALE_CONFLICT_QUEUE_FILTER_LABELS: Record<StaleConflictQueueFilter, string> = {
  all: 'Visible Queue',
  high_risk: 'High Risk Queue',
  needs_refresh: 'Needs Refresh Queue',
  safe_edits: 'Safe Edits Queue',
  delete: 'Delete Queue',
};

function createStaleConflictOverviewCollapsedState(expandedGroup: StaleConflictOverviewGroupKey | null = 'high_risk'): StaleConflictOverviewGroupState {
  return {
    high_risk: expandedGroup !== 'high_risk',
    needs_refresh: expandedGroup !== 'needs_refresh',
    delete: expandedGroup !== 'delete',
    safe_edits: expandedGroup !== 'safe_edits',
    other: expandedGroup !== 'other',
  };
}

function matchesStaleConflictQueueFilter(summary: StaleConflictOverviewSummary, filter: StaleConflictQueueFilter) {
  switch (filter) {
    case 'high_risk':
      return summary.isHighRisk;
    case 'needs_refresh':
      return summary.needsRefresh;
    case 'safe_edits':
      return summary.isSafeUpdate;
    case 'delete':
      return summary.failure.action === 'delete';
    default:
      return true;
  }
}

function getStaleConflictOverviewGroup(summary: StaleConflictOverviewSummary): StaleConflictOverviewGroupKey {
  if (summary.isHighRisk) return 'high_risk';
  if (summary.needsRefresh) return 'needs_refresh';
  if (summary.failure.action === 'delete') return 'delete';
  if (summary.isSafeUpdate) return 'safe_edits';
  return 'other';
}

function getPreferredStaleConflictOverviewGroup(summaries: StaleConflictOverviewSummary[]) {
  const visibleGroups = new Set(summaries.map((summary) => getStaleConflictOverviewGroup(summary)));
  return STALE_CONFLICT_OVERVIEW_GROUP_ORDER.find((groupKey) => visibleGroups.has(groupKey)) || null;
}

function buildDefaultColumnLayout(columns: string[]): ColumnLayoutState {
  return {
    order: columns,
    hidden: [],
    widths: {},
  };
}

function normalizeColumnLayout(raw: unknown, columns: string[]): ColumnLayoutState {
  const fallback = buildDefaultColumnLayout(columns);
  if (!raw || typeof raw !== 'object') return fallback;

  const layout = raw as Partial<ColumnLayoutState>;
  const hiddenSet = new Set(Array.isArray(layout.hidden) ? layout.hidden.filter((column): column is string => columns.includes(column)) : []);
  const order = Array.isArray(layout.order)
    ? layout.order.filter((column): column is string => columns.includes(column))
    : [];
  const widths = Object.entries(layout.widths || {}).reduce<Record<string, number>>((acc, [column, width]) => {
    if (columns.includes(column) && typeof width === 'number' && Number.isFinite(width)) {
      acc[column] = Math.max(120, Math.min(640, Math.round(width)));
    }
    return acc;
  }, {});
  const mergedOrder = [...order, ...columns.filter((column) => !order.includes(column))];

  if (hiddenSet.size >= columns.length && columns.length > 0) {
    hiddenSet.delete(mergedOrder[0]);
  }

  return {
    order: mergedOrder,
    hidden: [...hiddenSet],
    widths,
  };
}

function useResetTableState(
  tableName: string,
  dbId: string | undefined,
  setters: {
    setEditingCell: (v: any) => void;
    setModifiedRows: (v: any) => void;
    setDeletedRowIdxs: (v: any) => void;
    setNewRows: (v: any) => void;
    setSaveAttemptReport: (v: any) => void;
    setPendingStaleRecovery: (v: any) => void;
    setActiveStaleConflictKey: (v: any) => void;
    setStaleConflictReviewScope: (v: any) => void;
    setStaleConflictSelections: (v: any) => void;
    setShowStaleConflictOverview: (v: any) => void;
    setShowSaveReviewModal: (v: any) => void;
    setStaleConflictQueueFilter: (v: any) => void;
    setStaleConflictOverviewQuery: (v: any) => void;
    setStaleConflictOverviewSort: (v: any) => void;
    setStaleConflictOverviewCollapsedGroups: (v: any) => void;
    setShowPasteModal: (v: any) => void;
    setPasteText: (v: any) => void;
    setIsReadingClipboard: (v: any) => void;
    setContextMenu: (v: any) => void;
    setPreviewCell: (v: any) => void;
    setFilterMenu: (v: any) => void;
    setShowColumnMenu: (v: any) => void;
  }
) {
  const settersRef = useRef(setters);
  settersRef.current = setters;

  useEffect(() => {
    const s = settersRef.current;
    s.setEditingCell(null);
    s.setModifiedRows({});
    s.setDeletedRowIdxs(new Set());
    s.setNewRows([]);
    s.setSaveAttemptReport(null);
    s.setPendingStaleRecovery(null);
    s.setActiveStaleConflictKey(null);
    s.setStaleConflictReviewScope(null);
    s.setStaleConflictSelections({});
    s.setShowStaleConflictOverview(false);
    s.setShowSaveReviewModal(false);
    s.setStaleConflictQueueFilter('all');
    s.setStaleConflictOverviewQuery('');
    s.setStaleConflictOverviewSort('risk_desc');
    s.setStaleConflictOverviewCollapsedGroups(createStaleConflictOverviewCollapsedState());
    s.setShowPasteModal(false);
    s.setPasteText('');
    s.setIsReadingClipboard(false);
    s.setContextMenu(null);
    s.setPreviewCell(null);
    s.setFilterMenu(null);
    s.setShowColumnMenu(false);
  }, [tableName, dbId]);
}

export function DataTable({ 
  data, 
  schema, 
  tableName, 
  dbId,
  sorts, 
  setSorts, 
  filters, 
  setFilters, 
  onRefresh,
  isActive,
  isRefreshing,
  refreshError,
  dataRevision,
}: DataTableProps) {
  const [editingCell, setEditingCell] = useState<{ rowIdx: number; col: string; isNew: boolean } | null>(null);
  
  // Track changes
  const [modifiedRows, setModifiedRows] = useState<{ [idx: number]: any }>({});
  const [deletedRowIdxs, setDeletedRowIdxs] = useState<Set<number>>(new Set());
  const [newRows, setNewRows] = useState<any[]>([]);
  
  // Context Menu
  const [contextMenu, setContextMenu] = useState<{ x: number, y: number, rowIdx: number, col?: string, isNew: boolean } | null>(null);
  const [previewCell, setPreviewCell] = useState<PreviewPayload | null>(null);
  const [showSaveReviewModal, setShowSaveReviewModal] = useState(false);
  const [isSaving, setIsSaving] = useState(false);
  const [saveAttemptReport, setSaveAttemptReport] = useState<SaveAttemptReport | null>(null);
  const [pendingStaleRecovery, setPendingStaleRecovery] = useState<PendingStaleRecoveryState | null>(null);
  const [activeStaleConflictKey, setActiveStaleConflictKey] = useState<string | null>(null);
  const [staleConflictReviewScope, setStaleConflictReviewScope] = useState<StaleConflictReviewScope | null>(null);
  const [staleConflictSelections, setStaleConflictSelections] = useState<Record<string, 'pending' | 'latest'>>({});
  const [showStaleConflictOverview, setShowStaleConflictOverview] = useState(false);
  const [staleConflictQueueFilter, setStaleConflictQueueFilter] = useState<StaleConflictQueueFilter>('all');
  const [staleConflictOverviewQuery, setStaleConflictOverviewQuery] = useState('');
  const [staleConflictOverviewSort, setStaleConflictOverviewSort] = useState<StaleConflictQueueSort>('risk_desc');
  const [staleConflictOverviewCollapsedGroups, setStaleConflictOverviewCollapsedGroups] = useState<StaleConflictOverviewGroupState>(() => createStaleConflictOverviewCollapsedState());
  const [showPasteModal, setShowPasteModal] = useState(false);
  const [pasteText, setPasteText] = useState('');
  const [pasteMode, setPasteMode] = useState<'auto' | 'tsv' | 'json'>('auto');
  const [isReadingClipboard, setIsReadingClipboard] = useState(false);

  // Filter Dropdown
  const [filterMenu, setFilterMenu] = useState<{ col: string } | null>(null);
  const [showColumnMenu, setShowColumnMenu] = useState(false);
  const [resizingColumn, setResizingColumn] = useState<{ column: string; startX: number; startWidth: number } | null>(null);

  const columns = useMemo<string[]>(() => {
    return schema.columns.map((c: any): string => c.column_name);
  }, [schema]);

  const layoutStorageKey = useMemo(() => `data-table-layout:${dbId || 'default'}:${tableName}`, [dbId, tableName]);
  const [columnLayout, setColumnLayout] = useState<ColumnLayoutState>(() => buildDefaultColumnLayout([]));

  useResetTableState(tableName, dbId, {
    setEditingCell,
    setModifiedRows,
    setDeletedRowIdxs,
    setNewRows,
    setSaveAttemptReport,
    setPendingStaleRecovery,
    setActiveStaleConflictKey,
    setStaleConflictReviewScope,
    setStaleConflictSelections,
    setShowStaleConflictOverview,
    setShowSaveReviewModal,
    setStaleConflictQueueFilter,
    setStaleConflictOverviewQuery,
    setStaleConflictOverviewSort,
    setStaleConflictOverviewCollapsedGroups,
    setShowPasteModal,
    setPasteText,
    setIsReadingClipboard,
    setContextMenu,
    setPreviewCell,
    setFilterMenu,
    setShowColumnMenu,
  });

  const primaryKeys = useMemo(() => {
    const pkIdxs = schema.indexes.filter((i: any) => i.index_name === 'PRIMARY');
    return pkIdxs.map((i: any) => i.column_name);
  }, [schema]);

  const columnMetaByName = useMemo(() => {
    return new Map<string, any>(schema.columns.map((column: any) => [column.column_name, column]));
  }, [schema]);

  useEffect(() => {
    setColumnLayout(() => {
      if (columns.length === 0) return buildDefaultColumnLayout([]);
      try {
        const raw = window.localStorage.getItem(layoutStorageKey);
        return normalizeColumnLayout(raw ? JSON.parse(raw) : null, columns);
      } catch {
        return buildDefaultColumnLayout(columns);
      }
    });
  }, [columns, layoutStorageKey]);

  useEffect(() => {
    if (columns.length === 0) return;
    window.localStorage.setItem(layoutStorageKey, JSON.stringify(columnLayout));
  }, [columnLayout, columns.length, layoutStorageKey]);

  const orderedColumns = useMemo(() => {
    if (columns.length === 0) return [];
    return normalizeColumnLayout(columnLayout, columns).order;
  }, [columnLayout, columns]);

  const visibleColumns = useMemo(() => {
    const hiddenSet = new Set(columnLayout.hidden);
    return orderedColumns.filter((column) => !hiddenSet.has(column));
  }, [columnLayout.hidden, orderedColumns]);

  const getColumnWidth = useCallback((column: string) => columnLayout.widths[column] || DEFAULT_COLUMN_WIDTH, [columnLayout.widths]);

  const emitToast = (message: string, type: 'success' | 'error') => {
    window.dispatchEvent(new CustomEvent('global-toast', { detail: { message, type } }));
  };

  const stringifyJson = useCallback((value: unknown) =>
    JSON.stringify(value, (_key, item) => typeof item === 'bigint' ? item.toString() : item, 2), []);

  const normalizePreviewPayload = useCallback((
    rowIdx: number,
    col: string,
    isNew: boolean,
    value: unknown
  ): PreviewPayload => {
    const title = `${tableName}.${col}`;

    if (value === null || value === undefined) {
      return {
        title,
        value: 'NULL',
        draft: '',
        format: 'text',
        downloadExtension: 'txt',
        rowIdx,
        col,
        isNew,
        originalValue: value,
      };
    }

    if (typeof value === 'object') {
      const jsonValue = stringifyJson(value);
      return {
        title,
        value: jsonValue,
        draft: jsonValue,
        format: 'json',
        downloadExtension: 'json',
        rowIdx,
        col,
        isNew,
        originalValue: value,
      };
    }

    const textValue = String(value);
    const trimmed = textValue.trim();
    if (trimmed && (trimmed.startsWith('{') || trimmed.startsWith('['))) {
      try {
        const jsonValue = stringifyJson(JSON.parse(trimmed));
        return {
          title,
          value: jsonValue,
          draft: textValue,
          format: 'json',
          downloadExtension: 'json',
          rowIdx,
          col,
          isNew,
          originalValue: value,
        };
      } catch {
        // fall through to plain text
      }
    }

    return {
      title,
      value: textValue,
      draft: textValue,
      format: 'text',
      downloadExtension: 'txt',
      rowIdx,
      col,
      isNew,
      originalValue: value,
    };
  }, [tableName, stringifyJson]);

  const formatInlineCellValue = useCallback((value: unknown) => {
    const preview = normalizePreviewPayload(0, '', false, value).value.replace(/\s+/g, ' ').trim();
    if (!preview) return '';
    return preview.length > 120 ? `${preview.slice(0, 120)}...` : preview;
  }, [normalizePreviewPayload]);

  const getRowSnapshot = useCallback((rowIdx: number, isNew: boolean) => {
    return isNew ? newRows[rowIdx] : (modifiedRows[rowIdx] || data[rowIdx]);
  }, [newRows, modifiedRows, data]);

  const buildInsertStatement = useCallback((rowData: Record<string, any>) => {
    const values = columns.map((column: string) => {
      const val = rowData[column];
      if (val === null || val === undefined) return 'NULL';
      if (typeof val === 'number' || typeof val === 'boolean') return String(val);
      const normalized = typeof val === 'object' ? stringifyJson(val) : String(val);
      return `'${normalized.replace(/'/g, "''")}'`;
    });

    return `INSERT INTO \`${tableName}\` (${columns.map((column: string) => `\`${column}\``).join(', ')}) VALUES (${values.join(', ')});`;
  }, [columns, tableName, stringifyJson]);

  const copyTextToClipboard = useCallback(async (text: string, successMessage: string, errorMessage: string) => {
    try {
      await navigator.clipboard.writeText(text);
      emitToast(successMessage, 'success');
      return true;
    } catch {
      emitToast(errorMessage, 'error');
      return false;
    }
  }, []);

  const formatCellValue = useCallback((value: unknown) => {
    if (value === null || value === undefined) return '';
    if (typeof value === 'object') return stringifyJson(value);
    return String(value);
  }, [stringifyJson]);

  const getComparableCellValue = useCallback((value: unknown) => {
    if (value === null) return 'null';
    if (value === undefined) return 'undefined';
    if (typeof value === 'object') return `object:${stringifyJson(value)}`;
    return `${typeof value}:${String(value)}`;
  }, [stringifyJson]);

  const areCellValuesEqual = useCallback((left: unknown, right: unknown) => {
    return getComparableCellValue(left) === getComparableCellValue(right);
  }, [getComparableCellValue]);

  const formatReviewValue = useCallback((value: unknown) => {
    if (value === null) return 'NULL';
    if (value === undefined) return '(missing)';
    if (value === '') return '""';
    return formatCellValue(value);
  }, [formatCellValue]);

  const formatConditionLabel = useCallback((condition: Record<string, any>) => {
    return Object.entries(condition)
      .map(([column, value]) => `${column}=${formatReviewValue(value)}`)
      .join(', ');
  }, [formatReviewValue]);

  const getRowLabel = useCallback((rowIdx: number, isNew: boolean) => {
    return `${isNew ? 'Draft row' : 'Row'} ${rowIdx + 1}`;
  }, []);

  const extractCrudErrorMessage = useCallback((error: any) => {
    return redactSensitiveText(error?.response?.data?.message || error?.message || 'Unknown save error');
  }, []);

  const inferColumnFromErrorMessage = useCallback((message: string, fallbackCol?: string) => {
    const patterns = [
      /column ['"`]?([a-zA-Z0-9_]+)['"`]?/i,
      /key ['"`]?(?:[a-zA-Z0-9_]+\.)?([a-zA-Z0-9_]+)['"`]?/i,
      /constraint ['"`]?(?:[a-zA-Z0-9_]+\.)?([a-zA-Z0-9_]+)['"`]?/i,
    ];

    for (const pattern of patterns) {
      const match = message.match(pattern);
      if (!match?.[1]) continue;
      const candidate = columns.find((column) => column.toLowerCase() === match[1].toLowerCase());
      if (candidate) return candidate;
    }

    const lowerMessage = message.toLowerCase();
    const includedColumn = columns.find((column) => lowerMessage.includes(column.toLowerCase()));
    return includedColumn || fallbackCol;
  }, [columns]);

  const getSaveFailureKindLabel = useCallback((kind: SaveFailureItem['kind']) => {
    switch (kind) {
      case 'stale_row':
        return 'Stale Row';
      case 'duplicate_key':
        return 'Unique Conflict';
      case 'not_null':
        return 'Required Value';
      case 'foreign_key':
        return 'Foreign Key';
      case 'value_too_long':
        return 'Length Limit';
      case 'invalid_value':
        return 'Invalid Value';
      case 'read_only':
        return 'Read Only';
      default:
        return 'Database Error';
    }
  }, []);

  const getSaveFailureKey = useCallback((failure: Pick<SaveFailureItem, 'action' | 'kind' | 'rowIdx' | 'isNew' | 'col'>) => {
    return `${failure.action}:${failure.isNew ? 'new' : 'existing'}:${failure.rowIdx}:${failure.col || ''}:${failure.kind}`;
  }, []);

  const buildSaveFailureItem = useCallback((params: {
    action: SaveFailureItem['action'];
    rowIdx: number;
    isNew: boolean;
    fallbackCol?: string;
    rawMessage: string;
    forcedKind?: SaveFailureItem['kind'];
    staleRecovery?: StaleRecoveryContext;
    dataRevisionAtFailure?: number;
  }): SaveFailureItem => {
    const { action, rowIdx, isNew, fallbackCol, rawMessage, forcedKind, staleRecovery, dataRevisionAtFailure } = params;
    const lower = rawMessage.toLowerCase();
    const detectedCol = inferColumnFromErrorMessage(rawMessage, fallbackCol);
    const rowLabel = getRowLabel(rowIdx, isNew);

    let kind: SaveFailureItem['kind'] = forcedKind || 'generic';
    let summary = `${rowLabel} ${action} failed`;
    let message = rawMessage;

    if (forcedKind === 'stale_row') {
      kind = 'stale_row';
      summary = `${rowLabel} no longer matched the original server row`;
      message = action === 'delete'
        ? 'This row may already have been deleted or changed on the server. Refresh the table before retrying.'
        : 'This row was changed or no longer matches the original values on the server. Reload the latest row before retrying.';
    } else if (
      lower.includes('duplicate entry')
      || lower.includes('duplicate key')
      || lower.includes('unique constraint')
      || lower.includes('already exists')
    ) {
      kind = 'duplicate_key';
      summary = `${rowLabel} conflicts with an existing unique value`;
      message = detectedCol
        ? `Column ${detectedCol} must stay unique. Change the value before retrying.`
        : 'This change conflicts with an existing unique or primary-key value. Adjust the row before retrying.';
    } else if (
      lower.includes('cannot be null')
      || lower.includes('null value in column')
      || lower.includes('not-null constraint')
    ) {
      kind = 'not_null';
      summary = `${rowLabel} is missing a required value`;
      message = detectedCol
        ? `Column ${detectedCol} is required and cannot be NULL or empty.`
        : 'A required column is missing a value.';
    } else if (
      lower.includes('foreign key constraint')
      || lower.includes('violates foreign key constraint')
      || lower.includes('is still referenced')
      || lower.includes('a foreign key constraint fails')
    ) {
      kind = 'foreign_key';
      summary = action === 'delete'
        ? `${rowLabel} is still referenced by related data`
        : `${rowLabel} contains a foreign-key value that does not exist`;
      message = action === 'delete'
        ? 'This row is referenced by other records and cannot be deleted until those references are removed.'
        : detectedCol
          ? `Column ${detectedCol} points to a record that does not exist or is no longer valid.`
          : 'One of the referenced values does not exist or is blocked by a foreign-key rule.';
    } else if (
      lower.includes('data too long')
      || lower.includes('value too long')
      || lower.includes('too long for column')
    ) {
      kind = 'value_too_long';
      summary = `${rowLabel} exceeds a column length limit`;
      message = detectedCol
        ? `Column ${detectedCol} is longer than the database allows. Shorten the value and retry.`
        : 'One of the edited values is longer than the database allows.';
    } else if (
      lower.includes('incorrect integer value')
      || lower.includes('invalid input syntax')
      || lower.includes('out of range value')
      || lower.includes('invalid json')
      || lower.includes('cannot convert')
      || lower.includes('truncated incorrect')
    ) {
      kind = 'invalid_value';
      summary = `${rowLabel} contains a value the database rejected`;
      message = detectedCol
        ? `Column ${detectedCol} contains a value that does not match the server-side type or format.`
        : 'One of the edited values does not match the server-side type or format.';
    } else if (
      lower.includes('read only')
      || lower.includes('read-only')
      || lower.includes('forbidden')
      || lower.includes('denied')
      || lower.includes('permission')
    ) {
      kind = 'read_only';
      summary = `Connection rejected the ${action} operation`;
      message = 'This connection appears to be read-only or missing write permission for the requested change.';
    }

    return {
      action,
      kind,
      rowIdx,
      isNew,
      col: detectedCol,
      message,
      rawMessage,
      summary,
      staleRecovery,
      dataRevisionAtFailure,
    };
  }, [columns, getRowLabel, inferColumnFromErrorMessage]);

  const normalizeDraftStringValue = useCallback((value: unknown) => {
    return typeof value === 'string' ? value.trim() : value;
  }, []);

  const isNullableColumn = useCallback((columnName: string) => {
    return String(columnMetaByName.get(columnName)?.is_nullable || '').toUpperCase() === 'YES';
  }, [columnMetaByName]);

  const isNumericColumn = useCallback((columnName: string) => {
    const dataType = String(columnMetaByName.get(columnName)?.data_type || '').toLowerCase();
    return ['tinyint', 'smallint', 'mediumint', 'int', 'integer', 'bigint', 'decimal', 'numeric', 'float', 'double', 'real'].includes(dataType);
  }, [columnMetaByName]);

  const isBooleanColumn = useCallback((columnName: string) => {
    const meta = columnMetaByName.get(columnName);
    const dataType = String(meta?.data_type || '').toLowerCase();
    const columnType = String(meta?.column_type || '').toLowerCase();
    return dataType === 'boolean' || dataType === 'bool' || (dataType === 'tinyint' && columnType.includes('tinyint(1)'));
  }, [columnMetaByName]);

  const isJsonColumn = useCallback((columnName: string) => {
    const dataType = String(columnMetaByName.get(columnName)?.data_type || '').toLowerCase();
    return dataType === 'json';
  }, [columnMetaByName]);

  const isAutoGeneratedColumn = useCallback((columnName: string) => {
    const extra = String(columnMetaByName.get(columnName)?.extra || '').toLowerCase();
    return extra.includes('auto_increment') || extra.includes('generated') || extra.includes('identity');
  }, [columnMetaByName]);

  const resolveLiteralColumnDefault = useCallback((columnName: string): { supported: boolean; value?: any } => {
    const meta = columnMetaByName.get(columnName);
    const rawDefault = meta?.column_default;
    if (rawDefault === null || rawDefault === undefined) {
      return { supported: false };
    }

    const normalized = String(rawDefault).trim();
    if (!normalized) {
      return { supported: true, value: '' };
    }

    const lowered = normalized.toLowerCase();
    if (
      lowered.includes('current_timestamp')
      || lowered.includes('current_date')
      || lowered.includes('current_time')
      || lowered.includes('uuid')
      || lowered === 'now()'
    ) {
      return { supported: false };
    }

    if (lowered === 'null') {
      return { supported: true, value: null };
    }

    const dataType = String(meta?.data_type || '').toLowerCase();
    const columnType = String(meta?.column_type || '').toLowerCase();
    if (dataType === 'json' || normalized.startsWith('{') || normalized.startsWith('[')) {
      try {
        return { supported: true, value: JSON.parse(normalized) };
      } catch {
        return { supported: true, value: normalized };
      }
    }

    if (['tinyint', 'boolean', 'bool'].includes(dataType) && (normalized === '1' || normalized === '0' || lowered === 'true' || lowered === 'false')) {
      if (columnType.includes('tinyint(1)') || dataType === 'boolean' || dataType === 'bool') {
        return { supported: true, value: normalized === '1' || lowered === 'true' };
      }
    }

    if (/^-?\d+(\.\d+)?$/.test(normalized) && ['tinyint', 'smallint', 'mediumint', 'int', 'integer', 'bigint', 'decimal', 'numeric', 'float', 'double', 'real'].includes(dataType)) {
      return { supported: true, value: Number(normalized) };
    }

    return { supported: true, value: normalized };
  }, [columnMetaByName]);

  const buildDuplicateRowDraft = useCallback((rowData: Record<string, any>) => {
    const nextRow = JSON.parse(stringifyJson(rowData)) as Record<string, any>;
    const clearedColumns = new Set<string>();

    for (const column of columns) {
      if (primaryKeys.includes(column) || isAutoGeneratedColumn(column)) {
        clearedColumns.add(column);
        nextRow[column] = isNullableColumn(column) ? null : '';
      }
    }

    return { row: nextRow, clearedColumns: [...clearedColumns] };
  }, [columns, isAutoGeneratedColumn, isNullableColumn, primaryKeys, stringifyJson]);

  const buildEmptyDraftRow = useCallback(() => {
    const nextRow: Record<string, any> = {};
    columns.forEach((column: string) => {
      nextRow[column] = '';
    });
    return nextRow;
  }, [columns]);

  const coerceImportedCellValue = useCallback((columnName: string, rawValue: unknown) => {
    if (rawValue === null || rawValue === undefined) return rawValue;
    if (typeof rawValue !== 'string') return rawValue;

    const value = rawValue.trim();
    if (value === '') return '';
    if (value.toLowerCase() === 'null' && isNullableColumn(columnName)) return null;

    const meta = columnMetaByName.get(columnName);
    const dataType = String(meta?.data_type || '').toLowerCase();
    const columnType = String(meta?.column_type || '').toLowerCase();

    if ((dataType === 'json' || value.startsWith('{') || value.startsWith('['))) {
      try {
        return JSON.parse(value);
      } catch {
        if (dataType === 'json') return value;
      }
    }

    if (['tinyint', 'boolean', 'bool'].includes(dataType) && (value === '1' || value === '0' || value.toLowerCase() === 'true' || value.toLowerCase() === 'false')) {
      if (columnType.includes('tinyint(1)') || dataType === 'boolean' || dataType === 'bool') {
        return value === '1' || value.toLowerCase() === 'true';
      }
    }

    if (/^-?\d+(\.\d+)?$/.test(value) && ['tinyint', 'smallint', 'mediumint', 'int', 'integer', 'bigint', 'decimal', 'numeric', 'float', 'double', 'real'].includes(dataType)) {
      return Number(value);
    }

    return rawValue;
  }, [columnMetaByName, isNullableColumn]);

  const materializeImportedDraftRow = useCallback((source: Record<string, unknown>) => {
    const row = buildEmptyDraftRow();
    Object.entries(source).forEach(([column, value]) => {
      if (!columns.includes(column)) return;
      row[column] = coerceImportedCellValue(column, value);
    });
    return row;
  }, [buildEmptyDraftRow, coerceImportedCellValue, columns]);

  const validateCellValue = useCallback((columnName: string, value: unknown, isNew: boolean) => {
    const meta = columnMetaByName.get(columnName);
    const normalizedValue = normalizeDraftStringValue(value);
    const maxLengthRaw = Number(meta?.character_maximum_length);
    const hasMaxLength = Number.isFinite(maxLengthRaw) && maxLengthRaw > 0;
    const allowGeneratedBlank = isNew && isAutoGeneratedColumn(columnName);

    if (normalizedValue === null) {
      if (!allowGeneratedBlank && !isNullableColumn(columnName)) {
        return 'This NOT NULL column cannot be saved as NULL.';
      }
      return null;
    }

    if (normalizedValue === undefined || normalizedValue === '') {
      if (allowGeneratedBlank) return null;
      if (isNumericColumn(columnName)) return 'Enter a numeric value before saving.';
      if (isBooleanColumn(columnName)) return 'Enter true/false or 1/0 before saving.';
      if (isJsonColumn(columnName)) return 'Enter valid JSON before saving.';
      return null;
    }

    if (typeof normalizedValue === 'string') {
      if (hasMaxLength && normalizedValue.length > maxLengthRaw) {
        return `Value exceeds max length ${maxLengthRaw}.`;
      }

      if (isNumericColumn(columnName) && !/^-?\d+(\.\d+)?$/.test(normalizedValue)) {
        return 'Numeric columns only accept number-like values.';
      }

      if (isBooleanColumn(columnName) && !['1', '0', 'true', 'false'].includes(normalizedValue.toLowerCase())) {
        return 'Boolean columns only accept true/false or 1/0.';
      }

      if (isJsonColumn(columnName)) {
        try {
          JSON.parse(normalizedValue);
        } catch {
          return 'JSON column contains invalid JSON text.';
        }
      }
    }

    if (typeof normalizedValue === 'number') {
      if (!Number.isFinite(normalizedValue)) {
        return 'Numeric value must be finite.';
      }
      if (isBooleanColumn(columnName) && ![0, 1].includes(normalizedValue)) {
        return 'Boolean columns only accept true/false or 1/0.';
      }
    }

    return null;
  }, [
    columnMetaByName,
    normalizeDraftStringValue,
    isAutoGeneratedColumn,
    isNullableColumn,
    isNumericColumn,
    isBooleanColumn,
    isJsonColumn,
  ]);

  const buildRowValidationIssues = useCallback((rowData: Record<string, any>, rowIdx: number, isNew: boolean): ValidationIssue[] => {
    return columns.flatMap((column: string) => {
      const message = validateCellValue(column, rowData?.[column], isNew);
      return message ? [{ rowIdx, col: column, isNew, message }] : [];
    });
  }, [columns, validateCellValue]);

  const buildValidationCellKey = useCallback((rowIdx: number, col: string, isNew: boolean) => {
    return `${isNew ? 'new' : 'existing'}:${rowIdx}:${col}`;
  }, []);

  const prepareInsertRowPayload = useCallback((rowData: Record<string, any>) => {
    return columns.reduce((acc: Record<string, any>, column: string) => {
      const value = rowData?.[column];
      if (value === undefined) return acc;
      if (isAutoGeneratedColumn(column) && (value === '' || value === null)) return acc;
      acc[column] = value;
      return acc;
    }, {} as Record<string, any>);
  }, [columns, isAutoGeneratedColumn]);

  // Handle click outside to close menus
  useEffect(() => {
    const handleClick = () => {
      setContextMenu(null);
      setFilterMenu(null);
      setShowColumnMenu(false);
    };
    window.addEventListener('click', handleClick);
    return () => window.removeEventListener('click', handleClick);
  }, []);

  useEffect(() => {
    if (!resizingColumn) return;

    const handleMouseMove = (event: MouseEvent) => {
      const nextWidth = Math.max(120, Math.min(640, resizingColumn.startWidth + event.clientX - resizingColumn.startX));
      setColumnLayout((prev) => ({
        ...prev,
        widths: {
          ...prev.widths,
          [resizingColumn.column]: nextWidth,
        },
      }));
    };

    const handleMouseUp = () => setResizingColumn(null);

    document.body.style.cursor = 'col-resize';
    document.body.style.userSelect = 'none';
    window.addEventListener('mousemove', handleMouseMove);
    window.addEventListener('mouseup', handleMouseUp);

    return () => {
      document.body.style.cursor = '';
      document.body.style.userSelect = '';
      window.removeEventListener('mousemove', handleMouseMove);
      window.removeEventListener('mouseup', handleMouseUp);
    };
  }, [resizingColumn]);

  const handleSort = (col: string, e: React.MouseEvent) => {
    e.stopPropagation();
    let newSorts = [...sorts];
    const existingIdx = newSorts.findIndex(s => s.column === col);
    
    if (e.shiftKey) {
      if (existingIdx >= 0) {
        if (!newSorts[existingIdx].desc) {
          newSorts[existingIdx].desc = true;
        } else {
          newSorts.splice(existingIdx, 1);
        }
      } else {
        newSorts.push({ column: col, desc: false });
      }
    } else {
      if (existingIdx >= 0) {
        if (!newSorts[existingIdx].desc) {
          newSorts = [{ column: col, desc: true }];
        } else {
          newSorts = [];
        }
      } else {
        newSorts = [{ column: col, desc: false }];
      }
    }
    setSorts(newSorts);
  };

  const moveColumn = useCallback((column: string, direction: 'left' | 'right') => {
    setColumnLayout((prev) => {
      const order = [...prev.order];
      const currentIndex = order.indexOf(column);
      if (currentIndex < 0) return prev;
      const nextIndex = direction === 'left' ? currentIndex - 1 : currentIndex + 1;
      if (nextIndex < 0 || nextIndex >= order.length) return prev;
      [order[currentIndex], order[nextIndex]] = [order[nextIndex], order[currentIndex]];
      return { ...prev, order };
    });
  }, []);

  const toggleColumnVisibility = useCallback((column: string) => {
    setColumnLayout((prev) => {
      const hiddenSet = new Set(prev.hidden);
      if (hiddenSet.has(column)) {
        hiddenSet.delete(column);
      } else {
        if (prev.order.filter((item) => !hiddenSet.has(item)).length <= 1) {
          return prev;
        }
        hiddenSet.add(column);
      }
      return { ...prev, hidden: [...hiddenSet] };
    });
  }, []);

  const resetColumnLayout = useCallback(() => {
    setColumnLayout(buildDefaultColumnLayout(columns));
    setShowColumnMenu(false);
  }, [columns]);

  const handleCellDoubleClick = (rowIdx: number, col: string, isNew: boolean) => {
    setEditingCell({ rowIdx, col, isNew });
  };

  const handleCellChange = (val: any, rowIdx: number, col: string, isNew: boolean) => {
    if (isNew) {
      const updated = [...newRows];
      updated[rowIdx] = { ...updated[rowIdx], [col]: val };
      setNewRows(updated);
    } else {
      setModifiedRows((prev) => {
        const nextRow = {
          ...(prev[rowIdx] || data[rowIdx]),
          [col]: val,
        };
        const originalRow = data[rowIdx];
        const hasChanges = columns.some((column: string) => !areCellValuesEqual(nextRow[column], originalRow?.[column]));
        if (!hasChanges) {
          const { [rowIdx]: _removed, ...rest } = prev;
          return rest;
        }
        return {
          ...prev,
          [rowIdx]: nextRow,
        };
      });
    }
  };

  const handleAddNewRow = () => {
    setNewRows([...newRows, buildEmptyDraftRow()]);
  };

  const appendDraftRows = useCallback((rows: Record<string, any>[]) => {
    if (rows.length === 0) return;
    setNewRows((prev) => [...prev, ...rows]);
  }, []);

  const parseImportedRows = useCallback((rawText: string, mode: 'auto' | 'tsv' | 'json') => {
    const text = rawText.trim();
    if (!text) {
      throw new Error('Paste content is empty');
    }

    const tryParseJson = () => {
      const parsed = JSON.parse(text);
      const records = Array.isArray(parsed) ? parsed : [parsed];
      if (records.length === 0) {
        throw new Error('JSON payload does not contain any rows');
      }
      return records.map((record) => {
        if (Array.isArray(record)) {
          const source = visibleColumns.reduce<Record<string, unknown>>((acc, column, index) => {
            acc[column] = record[index];
            return acc;
          }, {});
          return materializeImportedDraftRow(source);
        }
        if (!record || typeof record !== 'object') {
          throw new Error('JSON rows must be objects or arrays');
        }
        return materializeImportedDraftRow(record as Record<string, unknown>);
      });
    };

    const parseTsv = () => {
      const lines = text
        .split(/\r?\n/)
        .map((line) => line.replace(/\r/g, ''))
        .filter((line) => line.trim().length > 0);

      if (lines.length === 0) {
        throw new Error('TSV payload does not contain any rows');
      }

      const cells = lines.map((line) => line.split('\t'));
      const normalizedHeader = cells[0].map((cell) => cell.trim());
      const headerMatchesColumns = normalizedHeader.length > 0 && normalizedHeader.every((cell) => columns.includes(cell));
      const targetColumns = headerMatchesColumns ? normalizedHeader : visibleColumns;
      const startIndex = headerMatchesColumns ? 1 : 0;
      const bodyRows = cells.slice(startIndex);

      if (bodyRows.length === 0) {
        throw new Error('TSV payload only contains a header row');
      }

      return bodyRows.map((rowCells) => {
        const source = targetColumns.reduce<Record<string, unknown>>((acc, column, index) => {
          acc[column] = rowCells[index] ?? '';
          return acc;
        }, {});
        return materializeImportedDraftRow(source);
      });
    };

    if (mode === 'json') return tryParseJson();
    if (mode === 'tsv') return parseTsv();

    if (text.startsWith('{') || text.startsWith('[')) {
      try {
        return tryParseJson();
      } catch {
        return parseTsv();
      }
    }

    return parseTsv();
  }, [columns, materializeImportedDraftRow, visibleColumns]);

  const handleReadClipboard = useCallback(async () => {
    try {
      setIsReadingClipboard(true);
      const text = await navigator.clipboard.readText();
      setPasteText(text);
      emitToast('Clipboard content loaded', 'success');
    } catch {
      emitToast('Failed to read clipboard', 'error');
    } finally {
      setIsReadingClipboard(false);
    }
  }, []);

  const handleImportPastedRows = useCallback(() => {
    try {
      const rows = parseImportedRows(pasteText, pasteMode);
      appendDraftRows(rows);
      setShowPasteModal(false);
      setPasteText('');
      emitToast(`Imported ${rows.length} row draft${rows.length > 1 ? 's' : ''}`, 'success');
    } catch (error) {
      emitToast(error instanceof Error ? error.message : 'Failed to import pasted rows', 'error');
    }
  }, [appendDraftRows, parseImportedRows, pasteMode, pasteText]);

  const handleSetCellNull = (rowIdx: number, col: string, isNew: boolean) => {
    if (!isNullableColumn(col)) {
      emitToast('This column is NOT NULL', 'error');
      return;
    }
    handleCellChange(null, rowIdx, col, isNew);
    setContextMenu(null);
    emitToast('Cell set to NULL', 'success');
  };

  const handleApplyColumnDefault = (rowIdx: number, col: string, isNew: boolean) => {
    const resolved = resolveLiteralColumnDefault(col);
    if (!resolved.supported) {
      emitToast('This column default cannot be materialized client-side', 'error');
      return;
    }

    handleCellChange(Object.prototype.hasOwnProperty.call(resolved, 'value') ? resolved.value : '', rowIdx, col, isNew);
    setContextMenu(null);
    emitToast('Applied schema default to cell', 'success');
  };

  const handleDuplicateRow = (rowIdx: number, isNew: boolean) => {
    const rowData = getRowSnapshot(rowIdx, isNew);
    const { row, clearedColumns } = buildDuplicateRowDraft(rowData);
    setNewRows((prev) => [...prev, row]);
    setContextMenu(null);
    if (clearedColumns.length > 0) {
      emitToast(`Row duplicated. Cleared generated/primary columns: ${clearedColumns.join(', ')}`, 'success');
    } else {
      emitToast('Row duplicated into a new draft row', 'success');
    }
  };

  const handleCopyCell = async (rowIdx: number, col: string, isNew: boolean) => {
    const rowData = getRowSnapshot(rowIdx, isNew);
    const val = rowData?.[col];
    await copyTextToClipboard(formatCellValue(val) || 'NULL', 'Cell value copied to clipboard', 'Failed to copy cell value');
    setContextMenu(null);
  };

  const handlePreviewCell = (rowIdx: number, col: string, isNew: boolean) => {
    const rowData = getRowSnapshot(rowIdx, isNew);
    setPreviewCell(normalizePreviewPayload(rowIdx, col, isNew, rowData?.[col]));
    setContextMenu(null);
  };

  const handleCopyRow = async (rowIdx: number, isNew: boolean) => {
    const rowData = getRowSnapshot(rowIdx, isNew);
    const tsvStr = columns.map((c: string) => {
      const val = rowData?.[c];
      if (val === null || val === undefined) return '';
      return formatCellValue(val).replace(/\t/g, ' ').replace(/\n/g, ' ');
    }).join('\t');

    await copyTextToClipboard(tsvStr, 'Row copied to clipboard (TSV)', 'Failed to copy row TSV');
    setContextMenu(null);
  };

  const handleCopyRowJson = async (rowIdx: number, isNew: boolean) => {
    const rowData = getRowSnapshot(rowIdx, isNew);
    await copyTextToClipboard(stringifyJson(rowData), 'Row JSON copied to clipboard', 'Failed to copy row JSON');
    setContextMenu(null);
  };

  const handleCopyRowSql = async (rowIdx: number, isNew: boolean) => {
    const rowData = getRowSnapshot(rowIdx, isNew);
    await copyTextToClipboard(buildInsertStatement(rowData), 'Row SQL copied to clipboard', 'Failed to copy row SQL');
    setContextMenu(null);
  };

  const handleCopyPreviewValue = async () => {
    if (!previewCell) return;
    await copyTextToClipboard(previewCell.draft, 'Large value copied to clipboard', 'Failed to copy large value');
  };

  const handleDownloadPreviewValue = () => {
    if (!previewCell) return;
    const blob = new Blob([previewCell.draft], {
      type: previewCell.format === 'json' ? 'application/json;charset=utf-8;' : 'text/plain;charset=utf-8;',
    });
    const url = URL.createObjectURL(blob);
    const link = document.createElement('a');
    link.href = url;
    link.setAttribute('download', `cell_${previewCell.col}_${new Date().toISOString().replace(/[:.]/g, '-')}.${previewCell.downloadExtension}`);
    document.body.appendChild(link);
    link.click();
    document.body.removeChild(link);
    URL.revokeObjectURL(url);
  };

  const handleApplyPreviewEdit = () => {
    if (!previewCell) return;

    let nextValue: any = previewCell.draft;
    if (typeof previewCell.originalValue === 'object' && previewCell.originalValue !== null) {
      try {
        nextValue = JSON.parse(previewCell.draft);
      } catch {
        emitToast('Invalid JSON: unable to apply cell edit', 'error');
        return;
      }
    }

    handleCellChange(nextValue, previewCell.rowIdx, previewCell.col, previewCell.isNew);
    setPreviewCell(null);
    emitToast('Cell value updated', 'success');
  };

  const handleDeleteRow = () => {
    if (contextMenu) {
      if (contextMenu.isNew) {
        const updated = [...newRows];
        updated.splice(contextMenu.rowIdx, 1);
        setNewRows(updated);
      } else {
        const newSet = new Set(deletedRowIdxs);
        newSet.add(contextMenu.rowIdx);
        setDeletedRowIdxs(newSet);
      }
    }
  };

  const getConditionForOriginalRow = useCallback((rowIdx: number) => {
    const originalRow = data[rowIdx];
    const condition: Record<string, any> = {};
    if (primaryKeys.length > 0) {
      primaryKeys.forEach((pk: string) => {
        condition[pk] = originalRow[pk];
      });
    } else {
      // Fallback: use all columns
      columns.forEach((col: string) => {
        condition[col] = originalRow[col];
      });
    }
    return condition;
  }, [data, primaryKeys, columns]);

  const findRowIndexByCondition = useCallback((condition: Record<string, any>) => {
    const entries = Object.entries(condition || {});
    if (entries.length === 0) return -1;
    return data.findIndex((row) => entries.every(([column, value]) => areCellValuesEqual(row?.[column], value)));
  }, [data, areCellValuesEqual]);

  const findRecoveryRowIndex = useCallback((failure: SaveFailureItem) => {
    const staleRecovery = failure.staleRecovery;
    if (!staleRecovery?.condition) return -1;

    const exactMatchIndex = findRowIndexByCondition(staleRecovery.condition);
    if (exactMatchIndex >= 0) {
      return exactMatchIndex;
    }

    if (primaryKeys.length > 0 || failure.action !== 'update' || !staleRecovery.originalRowData) {
      return -1;
    }

    const changedColumns = new Set(staleRecovery.changedColumns || []);
    const candidateIndexes = data.reduce<number[]>((matches, row, rowIdx) => {
      const matchesUnchangedColumns = columns
        .filter((column) => !changedColumns.has(column))
        .every((column) => areCellValuesEqual(row?.[column], staleRecovery.originalRowData?.[column]));
      if (matchesUnchangedColumns) {
        matches.push(rowIdx);
      }
      return matches;
    }, []);

    return candidateIndexes.length === 1 ? candidateIndexes[0] : -1;
  }, [data, columns, primaryKeys.length, areCellValuesEqual, findRowIndexByCondition]);

  const activeStaleConflict = useMemo(() => {
    if (!activeStaleConflictKey) return null;
    return (saveAttemptReport?.failures || []).find((failure) => getSaveFailureKey(failure) === activeStaleConflictKey) || null;
  }, [activeStaleConflictKey, saveAttemptReport, getSaveFailureKey]);

  const staleFailures = useMemo(() => {
    return (saveAttemptReport?.failures || []).filter((failure) => failure.kind === 'stale_row');
  }, [saveAttemptReport]);

  const firstStaleFailure = useMemo(() => {
    return staleFailures[0] || null;
  }, [staleFailures]);

  const activeStaleConflictReviewFailures = useMemo(() => {
    if (!staleConflictReviewScope?.failureKeys?.length) return staleFailures;
    const failureMap = new Map(staleFailures.map((failure) => [getSaveFailureKey(failure), failure]));
    const scopedFailures = staleConflictReviewScope.failureKeys
      .map((failureKey) => failureMap.get(failureKey))
      .filter((failure): failure is SaveFailureItem => Boolean(failure));
    return scopedFailures.length > 0 ? scopedFailures : staleFailures;
  }, [staleConflictReviewScope, staleFailures, getSaveFailureKey]);

  const activeStaleConflictScopedIndex = useMemo(() => {
    if (!activeStaleConflict) return -1;
    return activeStaleConflictReviewFailures.findIndex((failure) => getSaveFailureKey(failure) === getSaveFailureKey(activeStaleConflict));
  }, [activeStaleConflict, activeStaleConflictReviewFailures, getSaveFailureKey]);

  const getStaleConflictStateLabel = useCallback((state: StaleConflictDiffState) => {
    switch (state) {
      case 'conflict':
        return 'Conflict';
      case 'already_applied':
        return 'Already Applied';
      case 'local_pending':
        return 'Your Edit Only';
      case 'server_only':
        return 'Server Changed';
      default:
        return 'Refresh Needed';
    }
  }, []);

  const getStaleConflictStateClasses = useCallback((state: StaleConflictDiffState) => {
    switch (state) {
      case 'conflict':
        return 'border-red-400/30 bg-red-500/10 text-red-100';
      case 'already_applied':
        return 'border-green-400/30 bg-green-500/10 text-green-100';
      case 'local_pending':
        return 'border-blue-400/30 bg-blue-500/10 text-blue-100';
      case 'server_only':
        return 'border-amber-400/30 bg-amber-500/10 text-amber-100';
      default:
        return 'border-[#30363d] bg-[#161b22] text-gray-300';
    }
  }, []);

  const getStaleFailureDetails = useCallback((failure: SaveFailureItem) => {
    if (failure.kind !== 'stale_row') return null;

    const staleRecovery = failure.staleRecovery;
    const originalRowData = staleRecovery?.originalRowData || {};
    const pendingRowData = staleRecovery?.pendingRowData || {};
    const changedColumns = staleRecovery?.changedColumns || [];
    const locatedRowIdx = findRecoveryRowIndex(failure);
    const latestRowData = locatedRowIdx >= 0 ? data[locatedRowIdx] || {} : null;
    const hasFreshLatestRow = failure.dataRevisionAtFailure === undefined
      ? locatedRowIdx >= 0
      : dataRevision > failure.dataRevisionAtFailure && locatedRowIdx >= 0;
    const changedColumnSet = new Set(changedColumns);
    const serverChangedColumns = latestRowData
      ? columns.filter((column) => !areCellValuesEqual(latestRowData?.[column], originalRowData?.[column]))
      : [];

    const detailColumns = failure.action === 'delete'
      ? [...new Set([
          ...Object.keys(staleRecovery?.condition || {}),
          ...serverChangedColumns,
        ])]
      : [...new Set([
          ...changedColumns,
          ...serverChangedColumns,
        ])];

    const diffItems: StaleConflictDiffItem[] = detailColumns.map((column) => {
      const originalValue = originalRowData?.[column];
      const pendingValue = Object.prototype.hasOwnProperty.call(pendingRowData, column)
        ? pendingRowData[column]
        : originalValue;
      const latestValue = latestRowData ? latestRowData?.[column] : undefined;
      const userChanged = changedColumnSet.has(column);
      const serverChanged = latestRowData ? !areCellValuesEqual(latestValue, originalValue) : false;

      let state: StaleConflictDiffState = 'awaiting_refresh';
      if (hasFreshLatestRow) {
        if (userChanged && serverChanged) {
          state = areCellValuesEqual(pendingValue, latestValue) ? 'already_applied' : 'conflict';
        } else if (userChanged) {
          state = 'local_pending';
        } else if (serverChanged) {
          state = 'server_only';
        } else {
          state = 'already_applied';
        }
      }

      return {
        column,
        originalValue,
        pendingValue,
        latestValue,
        userChanged,
        serverChanged,
        state,
      };
    });

    const conflictCount = diffItems.filter((item) => item.state === 'conflict').length;
    const localPendingCount = diffItems.filter((item) => item.state === 'local_pending').length;
    const serverOnlyCount = diffItems.filter((item) => item.state === 'server_only').length;
    const alreadyAppliedCount = diffItems.filter((item) => item.state === 'already_applied').length;

    return {
      locatedRowIdx,
      latestRowData,
      originalRowData,
      pendingRowData,
      changedColumns,
      diffItems,
      hasFreshLatestRow,
      conflictCount,
      localPendingCount,
      serverOnlyCount,
      alreadyAppliedCount,
    };
  }, [findRecoveryRowIndex, data, dataRevision, columns, areCellValuesEqual]);

  const activeStaleConflictDetails = useMemo(() => {
    if (!activeStaleConflict) return null;
    return getStaleFailureDetails(activeStaleConflict);
  }, [activeStaleConflict, getStaleFailureDetails]);

  const recommendedStaleConflictSelections = useMemo<Record<string, 'pending' | 'latest'>>(() => {
    if (!activeStaleConflict || activeStaleConflict.action !== 'update' || !activeStaleConflictDetails) return {};
    const nextSelections = activeStaleConflictDetails.changedColumns.reduce<Record<string, 'pending' | 'latest'>>((acc, column) => {
      acc[column] = 'pending';
      return acc;
    }, {});

    activeStaleConflictDetails.diffItems.forEach((item) => {
      if (!item.userChanged) return;
      if (item.state === 'conflict' || item.state === 'server_only' || item.state === 'already_applied') {
        nextSelections[item.column] = 'latest';
      } else {
        nextSelections[item.column] = 'pending';
      }
    });

    return nextSelections;
  }, [activeStaleConflict, activeStaleConflictDetails]);

  const activeSelectedPendingCount = useMemo(() => {
    if (!activeStaleConflictDetails) return 0;
    return activeStaleConflictDetails.changedColumns.filter((column) => (staleConflictSelections[column] || 'pending') === 'pending').length;
  }, [activeStaleConflictDetails, staleConflictSelections]);

  const recommendedPendingCount = useMemo(() => {
    if (!activeStaleConflictDetails) return 0;
    return activeStaleConflictDetails.changedColumns.filter((column) => (recommendedStaleConflictSelections[column] || 'pending') === 'pending').length;
  }, [activeStaleConflictDetails, recommendedStaleConflictSelections]);

  const staleConflictHasSelectionOverrides = useMemo(() => {
    if (!activeStaleConflictDetails) return false;
    return activeStaleConflictDetails.changedColumns.some((column) =>
      (staleConflictSelections[column] || 'pending') !== (recommendedStaleConflictSelections[column] || 'pending'),
    );
  }, [activeStaleConflictDetails, staleConflictSelections, recommendedStaleConflictSelections]);

  const staleFailureSummaries = useMemo(() => {
    return staleFailures.map((failure) => {
      const details = getStaleFailureDetails(failure);
      const needsRefresh = !details?.hasFreshLatestRow || (details?.locatedRowIdx ?? -1) < 0;
      const isHighRisk = failure.action === 'update' && (details?.conflictCount || 0) > 0;
      const isSafeUpdate = failure.action === 'update'
        && Boolean(details?.hasFreshLatestRow)
        && (details?.locatedRowIdx ?? -1) >= 0
        && (details?.conflictCount || 0) === 0;

      return {
        failure,
        details,
        needsRefresh,
        isHighRisk,
        isSafeUpdate,
      };
    });
  }, [staleFailures, getStaleFailureDetails]);

  const staleConflictOverviewCounts = useMemo(() => {
    return {
      all: staleFailureSummaries.length,
      highRisk: staleFailureSummaries.filter((summary) => summary.isHighRisk).length,
      needsRefresh: staleFailureSummaries.filter((summary) => summary.needsRefresh).length,
      safeEdits: staleFailureSummaries.filter((summary) => summary.isSafeUpdate).length,
      delete: staleFailureSummaries.filter((summary) => summary.failure.action === 'delete').length,
    };
  }, [staleFailureSummaries]);

  const filteredStaleFailureSummaries = useMemo(() => {
    return staleFailureSummaries.filter((summary) => matchesStaleConflictQueueFilter(summary, staleConflictQueueFilter));
  }, [staleFailureSummaries, staleConflictQueueFilter]);

  const visibleStaleFailureSummaries = useMemo(() => {
    const query = staleConflictOverviewQuery.trim().toLowerCase();
    const filteredByQuery = filteredStaleFailureSummaries.filter((summary) => {
      if (!query) return true;
      const searchText = [
        summary.failure.action,
        summary.failure.summary,
        summary.failure.message,
        summary.failure.rawMessage,
        getRowLabel(summary.failure.rowIdx, summary.failure.isNew),
        summary.failure.staleRecovery?.condition ? formatConditionLabel(summary.failure.staleRecovery.condition) : '',
        summary.failure.staleRecovery?.changedColumns?.join(' ') || '',
      ].join(' ').toLowerCase();
      return searchText.includes(query);
    });

    const ranked = [...filteredByQuery];
    ranked.sort((left, right) => {
      const leftConflictCount = left.details?.conflictCount || 0;
      const rightConflictCount = right.details?.conflictCount || 0;
      const leftRiskWeight = left.isHighRisk ? 0 : left.needsRefresh ? 1 : left.failure.action === 'delete' ? 2 : left.isSafeUpdate ? 3 : 4;
      const rightRiskWeight = right.isHighRisk ? 0 : right.needsRefresh ? 1 : right.failure.action === 'delete' ? 2 : right.isSafeUpdate ? 3 : 4;

      switch (staleConflictOverviewSort) {
        case 'row_asc':
          return left.failure.rowIdx - right.failure.rowIdx;
        case 'row_desc':
          return right.failure.rowIdx - left.failure.rowIdx;
        case 'action':
          return left.failure.action.localeCompare(right.failure.action) || left.failure.rowIdx - right.failure.rowIdx;
        case 'conflicts_desc':
          return rightConflictCount - leftConflictCount || leftRiskWeight - rightRiskWeight || left.failure.rowIdx - right.failure.rowIdx;
        default:
          return leftRiskWeight - rightRiskWeight || rightConflictCount - leftConflictCount || left.failure.rowIdx - right.failure.rowIdx;
      }
    });

    return ranked;
  }, [
    filteredStaleFailureSummaries,
    staleConflictOverviewQuery,
    staleConflictOverviewSort,
    getRowLabel,
    formatConditionLabel,
  ]);

  const visibleStaleFailureSummaryGroups = useMemo(() => {
    const buckets: Record<StaleConflictOverviewGroupKey, Array<(typeof visibleStaleFailureSummaries)[number]>> = {
      high_risk: [],
      needs_refresh: [],
      delete: [],
      safe_edits: [],
      other: [],
    };

    visibleStaleFailureSummaries.forEach((summary) => {
      buckets[getStaleConflictOverviewGroup(summary)].push(summary);
    });

    return STALE_CONFLICT_OVERVIEW_GROUP_ORDER
      .map((groupKey) => ({
        groupKey,
        label: STALE_CONFLICT_OVERVIEW_GROUP_LABELS[groupKey],
        hint: STALE_CONFLICT_OVERVIEW_GROUP_HINTS[groupKey],
        items: buckets[groupKey],
      }))
      .filter((group) => group.items.length > 0);
  }, [visibleStaleFailureSummaries]);

  const visibleStaleConflictOverviewCounts = useMemo(() => {
    return {
      safeEdits: visibleStaleFailureSummaries.filter((summary) => summary.isSafeUpdate).length,
      needsRefresh: visibleStaleFailureSummaries.filter((summary) => summary.needsRefresh).length,
    };
  }, [visibleStaleFailureSummaries]);

  const currentStaleConflictOverviewLabel = useMemo(() => {
    const baseLabel = STALE_CONFLICT_QUEUE_FILTER_LABELS[staleConflictQueueFilter];
    return staleConflictOverviewQuery.trim() ? `${baseLabel} Search` : baseLabel;
  }, [staleConflictQueueFilter, staleConflictOverviewQuery]);

  const allVisibleStaleConflictGroupsCollapsed = useMemo(() =>
    visibleStaleFailureSummaryGroups.length > 0
      && visibleStaleFailureSummaryGroups.every((group) => staleConflictOverviewCollapsedGroups[group.groupKey]),
  [visibleStaleFailureSummaryGroups, staleConflictOverviewCollapsedGroups]);

  const allVisibleStaleConflictGroupsExpanded = useMemo(() =>
    visibleStaleFailureSummaryGroups.length > 0
      && visibleStaleFailureSummaryGroups.every((group) => !staleConflictOverviewCollapsedGroups[group.groupKey]),
  [visibleStaleFailureSummaryGroups, staleConflictOverviewCollapsedGroups]);

  const updatedRowPreviews = useMemo<SaveReviewUpdatePreview[]>(() => {
    return Object.entries(modifiedRows)
      .filter(([idx]) => !deletedRowIdxs.has(Number(idx)))
      .map(([idx, rowData]) => {
        const rowIdx = Number(idx);
        const originalRow = data[rowIdx] || {};
        const changes = columns
          .filter((column: string) => !areCellValuesEqual(rowData?.[column], originalRow?.[column]))
          .map((column: string) => ({
            column,
            before: originalRow?.[column],
            after: rowData?.[column],
          }));

        return {
          rowIdx,
          rowData,
          condition: getConditionForOriginalRow(rowIdx),
          changes,
        };
      })
      .filter((row) => row.changes.length > 0);
  }, [modifiedRows, deletedRowIdxs, data, columns, areCellValuesEqual, getConditionForOriginalRow]);

  const deletedRowPreviews = useMemo<SaveReviewDeletePreview[]>(() => {
    return [...deletedRowIdxs].map((rowIdx) => ({
      rowIdx,
      rowData: data[rowIdx],
      condition: getConditionForOriginalRow(rowIdx),
    }));
  }, [deletedRowIdxs, data, getConditionForOriginalRow]);

  const insertedRowPreviews = useMemo(() => {
    return newRows.map((rowData, rowIdx) => ({ rowIdx, rowData }));
  }, [newRows]);

  const validationIssues = useMemo<ValidationIssue[]>(() => {
    const updatedIssues = Object.entries(modifiedRows)
      .filter(([idx]) => !deletedRowIdxs.has(Number(idx)))
      .flatMap(([idx, rowData]) => buildRowValidationIssues(rowData, Number(idx), false));
    const insertedIssues = newRows.flatMap((rowData, rowIdx) => buildRowValidationIssues(rowData, rowIdx, true));
    return [...updatedIssues, ...insertedIssues];
  }, [modifiedRows, deletedRowIdxs, newRows, buildRowValidationIssues]);

  const validationIssueMap = useMemo(() => {
    return new Map(validationIssues.map((issue) => [buildValidationCellKey(issue.rowIdx, issue.col, issue.isNew), issue.message]));
  }, [validationIssues, buildValidationCellKey]);

  const existingValidationRowSet = useMemo(() => {
    return new Set(validationIssues.filter((issue) => !issue.isNew).map((issue) => issue.rowIdx));
  }, [validationIssues]);

  const newValidationRowSet = useMemo(() => {
    return new Set(validationIssues.filter((issue) => issue.isNew).map((issue) => issue.rowIdx));
  }, [validationIssues]);

  const validationIssueCount = validationIssues.length;

  const saveReviewCounts = useMemo(() => ({
    inserted: insertedRowPreviews.length,
    updated: updatedRowPreviews.length,
    deleted: deletedRowPreviews.length,
    total: insertedRowPreviews.length + updatedRowPreviews.length + deletedRowPreviews.length,
  }), [insertedRowPreviews.length, updatedRowPreviews.length, deletedRowPreviews.length]);

  const saveFailureCellKeys = useMemo(() => {
    return new Set((saveAttemptReport?.failures || []).map((failure) => buildValidationCellKey(failure.rowIdx, failure.col || columns[0] || '', failure.isNew)));
  }, [saveAttemptReport, buildValidationCellKey, columns]);

  const existingSaveFailureRowSet = useMemo(() => {
    return new Set((saveAttemptReport?.failures || []).filter((failure) => !failure.isNew).map((failure) => failure.rowIdx));
  }, [saveAttemptReport]);

  const newSaveFailureRowSet = useMemo(() => {
    return new Set((saveAttemptReport?.failures || []).filter((failure) => failure.isNew).map((failure) => failure.rowIdx));
  }, [saveAttemptReport]);

  const saveFailureMessageMap = useMemo(() => {
    return new Map(
      (saveAttemptReport?.failures || []).map((failure) => [
        buildValidationCellKey(failure.rowIdx, failure.col || columns[0] || '', failure.isNew),
        `${getSaveFailureKindLabel(failure.kind)}: ${failure.message}`,
      ]),
    );
  }, [saveAttemptReport, buildValidationCellKey, columns, getSaveFailureKindLabel]);

  const requestSaveReview = useCallback(() => {
    if (!saveReviewCounts.total || isSaving || isRefreshing) return;
    setEditingCell(null);
    setSaveAttemptReport(null);
    setShowSaveReviewModal(true);
  }, [saveReviewCounts.total, isSaving, isRefreshing]);

  const confirmSave = useCallback(async () => {
    if (!saveReviewCounts.total || isSaving || isRefreshing) return;
    if (validationIssueCount > 0) {
      emitToast(`Fix ${validationIssueCount} validation error${validationIssueCount === 1 ? '' : 's'} before saving.`, 'error');
      return;
    }
    setIsSaving(true);
    setSaveAttemptReport(null);
    try {
      const nextDeletedRowIdxs = new Set(deletedRowIdxs);
      const nextModifiedRows = { ...modifiedRows };
      const nextNewRows = [...newRows];
      const failures: SaveFailureItem[] = [];

      for (const row of deletedRowPreviews) {
        try {
          const result = await api.crudDelete(tableName, row.condition, dbId);
          if ((result?.affected_rows ?? 0) === 0) {
            failures.push(buildSaveFailureItem({
              action: 'delete',
              rowIdx: row.rowIdx,
              isNew: false,
              fallbackCol: primaryKeys[0] || columns[0],
              rawMessage: 'Delete matched 0 rows.',
              forcedKind: 'stale_row',
              staleRecovery: {
                condition: row.condition,
                originalRowData: row.rowData,
              },
              dataRevisionAtFailure: dataRevision,
            }));
          } else {
            nextDeletedRowIdxs.delete(row.rowIdx);
          }
        } catch (error) {
          failures.push(buildSaveFailureItem({
            action: 'delete',
            rowIdx: row.rowIdx,
            isNew: false,
            fallbackCol: primaryKeys[0] || columns[0],
            rawMessage: extractCrudErrorMessage(error),
          }));
        }
      }

      for (const row of updatedRowPreviews) {
        try {
          const result = await api.crudUpdate(tableName, row.rowData, row.condition, dbId);
          if ((result?.affected_rows ?? 0) === 0) {
            failures.push(buildSaveFailureItem({
              action: 'update',
              rowIdx: row.rowIdx,
              isNew: false,
              fallbackCol: row.changes[0]?.column || primaryKeys[0] || columns[0],
              rawMessage: 'Update matched 0 rows.',
              forcedKind: 'stale_row',
              staleRecovery: {
                condition: row.condition,
                originalRowData: data[row.rowIdx],
                pendingRowData: row.rowData,
                changedColumns: row.changes.map((change) => change.column),
              },
              dataRevisionAtFailure: dataRevision,
            }));
          } else {
            delete nextModifiedRows[row.rowIdx];
          }
        } catch (error) {
          failures.push(buildSaveFailureItem({
            action: 'update',
            rowIdx: row.rowIdx,
            isNew: false,
            fallbackCol: row.changes[0]?.column || primaryKeys[0] || columns[0],
            rawMessage: extractCrudErrorMessage(error),
          }));
        }
      }

      for (const row of insertedRowPreviews) {
        try {
          await api.crudInsert(tableName, prepareInsertRowPayload(row.rowData), dbId);
          nextNewRows[row.rowIdx] = null;
        } catch (error) {
          failures.push(buildSaveFailureItem({
            action: 'insert',
            rowIdx: row.rowIdx,
            isNew: true,
            fallbackCol: columns[0],
            rawMessage: extractCrudErrorMessage(error),
          }));
        }
      }

      const retainedNewRows = nextNewRows.filter((row): row is Record<string, any> => row !== null);
      const retainedInsertIndexMap = new Map<number, number>();
      let retainedInsertCursor = 0;
      nextNewRows.forEach((row, originalIdx) => {
        if (row !== null) {
          retainedInsertIndexMap.set(originalIdx, retainedInsertCursor);
          retainedInsertCursor += 1;
        }
      });
      const normalizedFailures = failures.map((failure) => failure.action === 'insert'
        ? { ...failure, rowIdx: retainedInsertIndexMap.get(failure.rowIdx) ?? failure.rowIdx }
        : failure);
      const report: SaveAttemptReport = {
        attempted: saveReviewCounts.total,
        succeeded: saveReviewCounts.total - normalizedFailures.length,
        failed: normalizedFailures.length,
        failures: normalizedFailures,
      };

      setModifiedRows(nextModifiedRows);
      setDeletedRowIdxs(nextDeletedRowIdxs);
      setNewRows(retainedNewRows);
      setEditingCell(null);

      if (normalizedFailures.length === 0) {
        setShowSaveReviewModal(false);
        setSaveAttemptReport(null);
        emitToast(`Saved ${saveReviewCounts.total} pending change${saveReviewCounts.total > 1 ? 's' : ''}`, 'success');
        onRefresh();
      } else {
        setShowSaveReviewModal(true);
        setSaveAttemptReport(report);
        emitToast(
          normalizedFailures.length === saveReviewCounts.total
            ? `All ${normalizedFailures.length} save operation${normalizedFailures.length === 1 ? '' : 's'} failed.`
            : `Saved ${report.succeeded} change${report.succeeded === 1 ? '' : 's'}; ${report.failed} failed and were kept.`,
          normalizedFailures.length === saveReviewCounts.total ? 'error' : 'success',
        );
      }
    } catch (e: any) {
      setSaveAttemptReport(null);
      window.dispatchEvent(new CustomEvent('global-toast', { detail: { message: `Error saving changes: ${extractCrudErrorMessage(e)}`, type: 'error' } }));
    } finally {
      setIsSaving(false);
    }
  }, [
    saveReviewCounts.total,
    isSaving,
    isRefreshing,
    validationIssueCount,
    deletedRowIdxs,
    modifiedRows,
    newRows,
    deletedRowPreviews,
    tableName,
    dbId,
    updatedRowPreviews,
    insertedRowPreviews,
    prepareInsertRowPayload,
    columns,
    data,
    dataRevision,
    primaryKeys,
    getRowLabel,
    extractCrudErrorMessage,
    buildSaveFailureItem,
    onRefresh,
  ]);

  const handleUndo = useCallback(() => {
    setModifiedRows({});
    setDeletedRowIdxs(new Set());
    setNewRows([]);
    setEditingCell(null);
    setShowSaveReviewModal(false);
    setSaveAttemptReport(null);
    setPendingStaleRecovery(null);
    setActiveStaleConflictKey(null);
    setStaleConflictReviewScope(null);
    setStaleConflictSelections({});
    setShowStaleConflictOverview(false);
    setStaleConflictQueueFilter('all');
    setStaleConflictOverviewQuery('');
    setStaleConflictOverviewSort('risk_desc');
    setStaleConflictOverviewCollapsedGroups(createStaleConflictOverviewCollapsedState());
  }, []);

  const handleReloadServerCopy = useCallback(() => {
    handleUndo();
    onRefresh();
    emitToast('Reloaded the latest server copy and dropped local pending changes.', 'success');
  }, [handleUndo, onRefresh]);

  const isDirty = saveReviewCounts.total > 0;
  const hasActiveFilters = filters.length > 0;
  const hasActiveSorts = sorts.length > 0;
  const hasGridTweaks = hasActiveFilters || hasActiveSorts;

  const clearGridTweaks = () => {
    setFilters([]);
    setSorts([]);
    setFilterMenu(null);
  };

  useEffect(() => {
    const handleGlobalSave = () => {
      if (isActive && isDirty) {
        requestSaveReview();
      }
    };
    window.addEventListener('global-save', handleGlobalSave);
    return () => window.removeEventListener('global-save', handleGlobalSave);
  }, [isActive, isDirty, requestSaveReview]);

  const downloadSql = () => {
    if (!data || data.length === 0) return;
    const headers = Object.keys(data[0]);
    const sqlContent = data.map((row: any) => {
      const values = headers.map(h => {
        const val = row[h];
        if (val === null || val === undefined) return 'NULL';
        if (typeof val === 'number' || typeof val === 'boolean') return String(val);
        return `'${formatCellValue(val).replace(/'/g, "''")}'`;
      });
      return `INSERT INTO \`${tableName}\` (${headers.map(h => `\`${h}\``).join(', ')}) VALUES (${values.join(', ')});`;
    }).join('\n');

    const blob = new Blob([sqlContent], { type: 'application/sql;charset=utf-8;' });
    const url = URL.createObjectURL(blob);
    const link = document.createElement('a');
    link.href = url;
    link.setAttribute('download', `${tableName}_${new Date().toISOString().replace(/[:.]/g, '-')}.sql`);
    document.body.appendChild(link);
    link.click();
    document.body.removeChild(link);
  };

  const downloadCsv = () => {
    if (!data || data.length === 0) return;
    const headers = Object.keys(data[0]);
    const csvContent = [
      headers.join(','),
      ...data.map((row: any) => 
        headers.map(h => {
          let val = row[h];
          if (val === null || val === undefined) return '';
          val = formatCellValue(val).replace(/"/g, '""');
          return `"${val}"`;
        }).join(',')
      )
    ].join('\n');

    const blob = new Blob(['\uFEFF' + csvContent], { type: 'text/csv;charset=utf-8;' });
    const url = URL.createObjectURL(blob);
    const link = document.createElement('a');
    link.href = url;
    link.setAttribute('download', `${tableName}_${new Date().toISOString().replace(/[:.]/g, '-')}.csv`);
    document.body.appendChild(link);
    link.click();
    document.body.removeChild(link);
  };

  const parentRef = useRef<HTMLDivElement>(null);

  // Compute visible rows (excluding deleted)
  const visibleRowIndices = useMemo(() => {
    const indices = [];
    for (let i = 0; i < data.length; i++) {
      if (!deletedRowIdxs.has(i)) {
        indices.push(i);
      }
    }
    return indices;
  }, [data.length, deletedRowIdxs]);

  const rowVirtualizer = useVirtualizer({
    count: visibleRowIndices.length,
    getScrollElement: () => parentRef.current,
    estimateSize: () => APPROX_ROW_HEIGHT,
    overscan: 10,
  });

  const focusValidationIssue = useCallback((issue: ValidationIssue) => {
    setShowSaveReviewModal(false);
    setContextMenu(null);
    setFilterMenu(null);

    window.requestAnimationFrame(() => {
      if (issue.isNew) {
        parentRef.current?.scrollTo({
          top: rowVirtualizer.getTotalSize() + issue.rowIdx * APPROX_ROW_HEIGHT,
          behavior: 'smooth',
        });
      } else {
        const visibleIndex = visibleRowIndices.indexOf(issue.rowIdx);
        if (visibleIndex >= 0) {
          rowVirtualizer.scrollToIndex(visibleIndex, { align: 'center' });
        }
      }
      setEditingCell({ rowIdx: issue.rowIdx, col: issue.col, isNew: issue.isNew });
    });
  }, [rowVirtualizer, visibleRowIndices]);

  const focusFirstValidationIssue = useCallback(() => {
    if (!validationIssues[0]) return;
    focusValidationIssue(validationIssues[0]);
  }, [validationIssues, focusValidationIssue]);

  const clearResolvedFailuresFromReport = useCallback((resolvedFailures: SaveFailureItem[]) => {
    if (resolvedFailures.length === 0) return;

    const removedInsertIndexes = resolvedFailures
      .filter((failure) => failure.action === 'insert')
      .map((failure) => failure.rowIdx)
      .sort((left, right) => left - right);
    const resolvedKeys = new Set(
      resolvedFailures.map((failure) => getSaveFailureKey(failure)),
    );

    setSaveAttemptReport((prev) => {
      if (!prev) return prev;
      const remainingFailures = prev.failures
        .filter((failure) => !resolvedKeys.has(getSaveFailureKey(failure)))
        .map((failure) => {
          if (failure.action !== 'insert') return failure;
          const shift = removedInsertIndexes.filter((index) => index < failure.rowIdx).length;
          return shift > 0 ? { ...failure, rowIdx: failure.rowIdx - shift } : failure;
        });

      if (remainingFailures.length === 0) return null;

      return {
        attempted: prev.succeeded + remainingFailures.length,
        succeeded: prev.succeeded,
        failed: remainingFailures.length,
        failures: remainingFailures,
      };
    });
  }, [getSaveFailureKey]);

  const discardSaveFailures = useCallback((predicate: (failure: SaveFailureItem) => boolean, options?: {
    refreshAfter?: boolean;
    successMessage?: string;
    emptyMessage?: string;
  }) => {
    const matchingFailures = (saveAttemptReport?.failures || []).filter(predicate);
    if (matchingFailures.length === 0) {
      if (options?.emptyMessage) {
        emitToast(options.emptyMessage, 'error');
      }
      return;
    }

    const updateRowIdxs = new Set(
      matchingFailures.filter((failure) => failure.action === 'update').map((failure) => failure.rowIdx),
    );
    const deleteRowIdxs = new Set(
      matchingFailures.filter((failure) => failure.action === 'delete').map((failure) => failure.rowIdx),
    );
    const insertRowIdxs = matchingFailures
      .filter((failure) => failure.action === 'insert')
      .map((failure) => failure.rowIdx)
      .sort((left, right) => right - left);

    if (updateRowIdxs.size > 0) {
      setModifiedRows((prev) => {
        const next = { ...prev };
        updateRowIdxs.forEach((rowIdx) => {
          delete next[rowIdx];
        });
        return next;
      });
    }

    if (deleteRowIdxs.size > 0) {
      setDeletedRowIdxs((prev) => {
        const next = new Set(prev);
        deleteRowIdxs.forEach((rowIdx) => next.delete(rowIdx));
        return next;
      });
    }

    if (insertRowIdxs.length > 0) {
      setNewRows((prev) => {
        const next = [...prev];
        insertRowIdxs.forEach((rowIdx) => {
          if (rowIdx >= 0 && rowIdx < next.length) {
            next.splice(rowIdx, 1);
          }
        });
        return next;
      });
    }

    clearResolvedFailuresFromReport(matchingFailures);
    setEditingCell(null);
    setContextMenu(null);
    setFilterMenu(null);

    if (options?.refreshAfter) {
      onRefresh();
    }

    if (options?.successMessage) {
      emitToast(options.successMessage, 'success');
    }
  }, [saveAttemptReport, clearResolvedFailuresFromReport, onRefresh]);

  const handleDiscardAllFailedChanges = useCallback(() => {
    discardSaveFailures(
      () => true,
      {
        successMessage: 'Discarded all failed pending changes.',
        emptyMessage: 'No failed changes to discard.',
      },
    );
  }, [discardSaveFailures]);

  const handleDiscardAllStaleFailures = useCallback(() => {
    discardSaveFailures(
      (failure) => failure.kind === 'stale_row',
      {
        successMessage: 'Discarded stale-row conflicts from the pending change set.',
        emptyMessage: 'No stale-row conflicts to discard.',
      },
    );
  }, [discardSaveFailures]);

  const buildStaleConflictReviewScope = useCallback((failures: SaveFailureItem[], label: string): StaleConflictReviewScope | null => {
    const failureKeys = failures.map((item) => getSaveFailureKey(item));
    if (failureKeys.length === 0) return null;
    return {
      failureKeys,
      label,
    };
  }, [getSaveFailureKey]);

  const handleOpenStaleConflict = useCallback((failure: SaveFailureItem, options?: { scope?: StaleConflictReviewScope | null }) => {
    if (failure.kind !== 'stale_row') return;
    const defaultSelections = (failure.staleRecovery?.changedColumns || []).reduce<Record<string, 'pending' | 'latest'>>((acc, column) => {
      acc[column] = 'pending';
      return acc;
    }, {});
    setShowStaleConflictOverview(false);
    setActiveStaleConflictKey(getSaveFailureKey(failure));
    setStaleConflictReviewScope(options?.scope ?? null);
    setStaleConflictSelections(defaultSelections);
  }, [getSaveFailureKey]);

  const handleOpenFirstStaleConflict = useCallback(() => {
    if (!firstStaleFailure) {
      emitToast('No stale-row conflict is available to review.', 'error');
      return;
    }
    handleOpenStaleConflict(firstStaleFailure, {
      scope: buildStaleConflictReviewScope(staleFailures, 'All Stale Conflicts'),
    });
  }, [firstStaleFailure, handleOpenStaleConflict, buildStaleConflictReviewScope, staleFailures]);

  const matchesStaleConflictOverviewQuery = useCallback((summary: (typeof staleFailureSummaries)[number], queryValue: string) => {
    const query = queryValue.trim().toLowerCase();
    if (!query) return true;
    const searchText = [
      summary.failure.action,
      summary.failure.summary,
      summary.failure.message,
      summary.failure.rawMessage,
      getRowLabel(summary.failure.rowIdx, summary.failure.isNew),
      summary.failure.staleRecovery?.condition ? formatConditionLabel(summary.failure.staleRecovery.condition) : '',
      summary.failure.staleRecovery?.changedColumns?.join(' ') || '',
    ].join(' ').toLowerCase();
    return searchText.includes(query);
  }, [getRowLabel, formatConditionLabel]);

  const getPreferredStaleConflictOverviewFocus = useCallback((filter: StaleConflictQueueFilter, queryValue = '') => {
    const filteredSummaries = staleFailureSummaries.filter((summary) => matchesStaleConflictQueueFilter(summary, filter));
    const visibleSummaries = filteredSummaries.filter((summary) => matchesStaleConflictOverviewQuery(summary, queryValue));
    return getPreferredStaleConflictOverviewGroup(visibleSummaries.length > 0 ? visibleSummaries : filteredSummaries);
  }, [staleFailureSummaries, matchesStaleConflictOverviewQuery]);

  const handleSetStaleConflictOverviewFilter = useCallback((filter: StaleConflictQueueFilter) => {
    setStaleConflictQueueFilter(filter);
    setStaleConflictOverviewCollapsedGroups(
      createStaleConflictOverviewCollapsedState(getPreferredStaleConflictOverviewFocus(filter, staleConflictOverviewQuery)),
    );
  }, [getPreferredStaleConflictOverviewFocus, staleConflictOverviewQuery]);

  const handleToggleStaleConflictOverviewGroup = useCallback((groupKey: StaleConflictOverviewGroupKey) => {
    setStaleConflictOverviewCollapsedGroups((prev) => ({
      ...prev,
      [groupKey]: !prev[groupKey],
    }));
  }, []);

  const handleExpandAllVisibleStaleConflictGroups = useCallback(() => {
    if (visibleStaleFailureSummaryGroups.length === 0) return;
    setStaleConflictOverviewCollapsedGroups((prev) => {
      const next = { ...prev };
      visibleStaleFailureSummaryGroups.forEach((group) => {
        next[group.groupKey] = false;
      });
      return next;
    });
  }, [visibleStaleFailureSummaryGroups]);

  const handleCollapseAllVisibleStaleConflictGroups = useCallback(() => {
    if (visibleStaleFailureSummaryGroups.length === 0) return;
    setStaleConflictOverviewCollapsedGroups((prev) => {
      const next = { ...prev };
      visibleStaleFailureSummaryGroups.forEach((group) => {
        next[group.groupKey] = true;
      });
      return next;
    });
  }, [visibleStaleFailureSummaryGroups]);

  const handleOpenStaleConflictOverview = useCallback((filter: StaleConflictQueueFilter = 'all') => {
    if (staleFailures.length === 0) {
      emitToast('No stale-row conflicts are available to review.', 'error');
      return;
    }
    setStaleConflictQueueFilter(filter);
    setStaleConflictOverviewQuery('');
    setStaleConflictOverviewSort('risk_desc');
    setStaleConflictOverviewCollapsedGroups(
      createStaleConflictOverviewCollapsedState(getPreferredStaleConflictOverviewFocus(filter, '')),
    );
    setShowStaleConflictOverview(true);
  }, [staleFailures.length, getPreferredStaleConflictOverviewFocus]);

  const handleCloseStaleConflictOverview = useCallback(() => {
    setShowStaleConflictOverview(false);
  }, []);

  const handleCloseStaleConflict = useCallback(() => {
    setActiveStaleConflictKey(null);
    setStaleConflictReviewScope(null);
    setStaleConflictSelections({});
  }, []);

  const handleReturnToStaleConflictOverview = useCallback(() => {
    setActiveStaleConflictKey(null);
    setStaleConflictReviewScope(null);
    setStaleConflictSelections({});
    setShowStaleConflictOverview(true);
  }, []);

  const handleSetStaleConflictSelection = useCallback((column: string, choice: 'pending' | 'latest') => {
    setStaleConflictSelections((prev) => ({
      ...prev,
      [column]: choice,
    }));
  }, []);

  const handleApplyStaleConflictPreset = useCallback((mode: 'recommended' | 'mine' | 'server') => {
    if (!activeStaleConflict || activeStaleConflict.action !== 'update' || !activeStaleConflictDetails) return;
    const nextSelections = activeStaleConflictDetails.changedColumns.reduce<Record<string, 'pending' | 'latest'>>((acc, column) => {
      if (mode === 'recommended') {
        acc[column] = recommendedStaleConflictSelections[column] || 'pending';
      } else if (mode === 'server') {
        acc[column] = 'latest';
      } else {
        acc[column] = 'pending';
      }
      return acc;
    }, {});
    setStaleConflictSelections(nextSelections);
  }, [activeStaleConflict, activeStaleConflictDetails, recommendedStaleConflictSelections]);

  const handleOpenAdjacentStaleConflict = useCallback((direction: 'prev' | 'next') => {
    if (activeStaleConflictScopedIndex < 0) return;
    const delta = direction === 'next' ? 1 : -1;
    const target = activeStaleConflictReviewFailures[activeStaleConflictScopedIndex + delta];
    if (!target) {
      emitToast(direction === 'next' ? 'No newer stale conflict in the review queue.' : 'No earlier stale conflict in the review queue.', 'error');
      return;
    }
    handleOpenStaleConflict(target, {
      scope: buildStaleConflictReviewScope(activeStaleConflictReviewFailures, staleConflictReviewScope?.label || 'All Stale Conflicts'),
    });
  }, [
    activeStaleConflictScopedIndex,
    activeStaleConflictReviewFailures,
    handleOpenStaleConflict,
    buildStaleConflictReviewScope,
    staleConflictReviewScope,
  ]);

  const applySafeUpdateModeToStaleSummaries = useCallback((summaries: typeof staleFailureSummaries, mode: 'recommended' | 'mine', options?: {
    emptyMessage?: string;
    successPrefix?: string;
  }) => {
    const safeSummaries = summaries.filter((summary) => summary.isSafeUpdate && summary.details?.latestRowData && (summary.details?.locatedRowIdx ?? -1) >= 0);
    if (safeSummaries.length === 0) {
      emitToast(
        options?.emptyMessage || (mode === 'mine'
          ? 'No safe stale edits are ready for keeping your current edits on the current page.'
          : 'No safe stale edits are ready for recommended batch apply on the current page.'),
        'error',
      );
      return;
    }

    const resolvedFailures = safeSummaries.map((summary) => summary.failure);
    const resolvedKeys = new Set(resolvedFailures.map((failure) => getSaveFailureKey(failure)));

    setModifiedRows((prev) => {
      const next = { ...prev };
      safeSummaries.forEach((summary) => {
        const { failure, details } = summary;
        if (!details?.latestRowData) return;

        const pendingRowData = failure.staleRecovery?.pendingRowData || {};
        const changedColumns = failure.staleRecovery?.changedColumns || [];
        const recommendedSelections = mode === 'recommended'
          ? changedColumns.reduce<Record<string, 'pending' | 'latest'>>((acc, column) => {
              acc[column] = 'pending';
              return acc;
            }, {})
          : null;

        if (recommendedSelections) {
          details.diffItems.forEach((item) => {
            if (!item.userChanged) return;
            recommendedSelections[item.column] = item.state === 'local_pending' ? 'pending' : 'latest';
          });
        }

        const nextRowData = { ...details.latestRowData };
        let keepPendingCount = 0;
        changedColumns.forEach((column) => {
          const shouldKeepPending = mode === 'mine' || (recommendedSelections?.[column] || 'pending') === 'pending';
          if (shouldKeepPending) {
            nextRowData[column] = pendingRowData[column];
            if (!areCellValuesEqual(nextRowData[column], details.latestRowData?.[column])) {
              keepPendingCount += 1;
            }
          }
        });

        delete next[failure.rowIdx];
        if (keepPendingCount > 0) {
          next[details.locatedRowIdx] = nextRowData;
        } else {
          delete next[details.locatedRowIdx];
        }
      });
      return next;
    });

    clearResolvedFailuresFromReport(resolvedFailures);
    if (activeStaleConflict && resolvedKeys.has(getSaveFailureKey(activeStaleConflict))) {
      handleCloseStaleConflict();
    }
    emitToast(
      `${options?.successPrefix || (mode === 'mine' ? 'Kept your edits for' : 'Applied recommended resolution to')} ${safeSummaries.length} safe stale edit${safeSummaries.length === 1 ? '' : 's'}.`,
      'success',
    );
  }, [getSaveFailureKey, areCellValuesEqual, clearResolvedFailuresFromReport, activeStaleConflict, handleCloseStaleConflict]);

  const handleKeepMineForVisibleSafeStaleConflicts = useCallback(() => {
    applySafeUpdateModeToStaleSummaries(visibleStaleFailureSummaries, 'mine', {
      successPrefix: 'Kept your edits for',
    });
  }, [applySafeUpdateModeToStaleSummaries, visibleStaleFailureSummaries]);

  const handleApplyRecommendedToFilteredStaleConflicts = useCallback(() => {
    applySafeUpdateModeToStaleSummaries(visibleStaleFailureSummaries, 'recommended', {
      successPrefix: 'Applied recommended resolution to',
    });
  }, [applySafeUpdateModeToStaleSummaries, visibleStaleFailureSummaries]);

  const handleKeepMineForSingleStaleSummary = useCallback((failureKey: string) => {
    const summary = staleFailureSummaries.find((item) => getSaveFailureKey(item.failure) === failureKey);
    if (!summary) {
      emitToast('This stale conflict is no longer available.', 'error');
      return;
    }
    applySafeUpdateModeToStaleSummaries([summary], 'mine', {
      successPrefix: 'Kept your edits for',
    });
  }, [staleFailureSummaries, getSaveFailureKey, applySafeUpdateModeToStaleSummaries]);

  const handleOpenFirstStaleConflictGroup = useCallback((groupSummaries: typeof visibleStaleFailureSummaries, label: string) => {
    if (groupSummaries.length === 0) {
      emitToast('No stale conflicts are available in this group.', 'error');
      return;
    }
    handleOpenStaleConflict(groupSummaries[0].failure, {
      scope: buildStaleConflictReviewScope(groupSummaries.map((summary) => summary.failure), `${label} Group`),
    });
  }, [handleOpenStaleConflict, buildStaleConflictReviewScope]);

  const handleOpenFirstFilteredStaleConflict = useCallback(() => {
    if (visibleStaleFailureSummaries.length === 0) {
      emitToast('No stale conflicts match the current filter.', 'error');
      return;
    }
    handleOpenStaleConflict(visibleStaleFailureSummaries[0].failure, {
      scope: buildStaleConflictReviewScope(visibleStaleFailureSummaries.map((summary) => summary.failure), currentStaleConflictOverviewLabel),
    });
  }, [visibleStaleFailureSummaries, handleOpenStaleConflict, buildStaleConflictReviewScope, currentStaleConflictOverviewLabel]);

  const handleApplyRecommendedToSingleStaleSummary = useCallback((failureKey: string) => {
    const summary = staleFailureSummaries.find((item) => getSaveFailureKey(item.failure) === failureKey);
    if (!summary) {
      emitToast('This stale conflict is no longer available.', 'error');
      return;
    }
    applySafeUpdateModeToStaleSummaries([summary], 'recommended', {
      successPrefix: 'Applied recommended resolution to',
    });
  }, [staleFailureSummaries, getSaveFailureKey, applySafeUpdateModeToStaleSummaries]);

  const applyLatestServerCopyToStaleSummaries = useCallback((summaries: typeof staleFailureSummaries, options?: {
    successPrefix?: string;
  }) => {
    const failures = summaries.map((summary) => summary.failure);
    if (failures.length === 0) {
      emitToast('No stale conflicts are available for accepting the latest server copy.', 'error');
      return;
    }
    const resolvedKeys = new Set(failures.map((failure) => getSaveFailureKey(failure)));
    discardSaveFailures(
      (item) => resolvedKeys.has(getSaveFailureKey(item)),
      {
        successMessage: `${options?.successPrefix || 'Accepted latest server copy for'} ${failures.length} stale conflict${failures.length === 1 ? '' : 's'}.`,
      },
    );
    if (activeStaleConflict && resolvedKeys.has(getSaveFailureKey(activeStaleConflict))) {
      handleCloseStaleConflict();
    }
  }, [getSaveFailureKey, discardSaveFailures, activeStaleConflict, handleCloseStaleConflict]);

  const handleUseLatestForVisibleStaleConflicts = useCallback(() => {
    applyLatestServerCopyToStaleSummaries(visibleStaleFailureSummaries, {
      successPrefix: 'Accepted latest server copy for',
    });
  }, [applyLatestServerCopyToStaleSummaries, visibleStaleFailureSummaries]);

  const handleUseLatestForSingleStaleSummary = useCallback((failureKey: string) => {
    const summary = staleFailureSummaries.find((item) => getSaveFailureKey(item.failure) === failureKey);
    if (!summary) {
      emitToast('This stale conflict is no longer available.', 'error');
      return;
    }
    applyLatestServerCopyToStaleSummaries([summary], {
      successPrefix: 'Accepted latest server copy for',
    });
  }, [staleFailureSummaries, getSaveFailureKey, applyLatestServerCopyToStaleSummaries]);

  const queueStaleRecovery = useCallback((failuresToRecover: SaveFailureItem[], options?: {
    emptyMessage?: string;
  }) => {
    if (pendingStaleRecovery) {
      emitToast('A stale-row recovery is already in progress. Wait for the current refresh to finish.', 'error');
      return;
    }

    const recoverableFailures = failuresToRecover.filter((failure) =>
      failure.kind === 'stale_row'
      && !failure.isNew
      && (failure.action === 'update' || failure.action === 'delete')
      && Boolean(failure.staleRecovery?.condition),
    );

    if (recoverableFailures.length === 0) {
      emitToast(options?.emptyMessage || 'No recoverable stale-row changes were found.', 'error');
      return;
    }

    const recoverableKeys = new Set(recoverableFailures.map((failure) => getSaveFailureKey(failure)));
    const updateRowIdxs = new Set(
      recoverableFailures
        .filter((failure) => failure.action === 'update')
        .map((failure) => failure.rowIdx),
    );
    const deleteRowIdxs = new Set(
      recoverableFailures
        .filter((failure) => failure.action === 'delete')
        .map((failure) => failure.rowIdx),
    );

    if (updateRowIdxs.size > 0) {
      setModifiedRows((prev) => {
        const next = { ...prev };
        updateRowIdxs.forEach((rowIdx) => {
          delete next[rowIdx];
        });
        return next;
      });
    }

    if (deleteRowIdxs.size > 0) {
      setDeletedRowIdxs((prev) => {
        const next = new Set(prev);
        deleteRowIdxs.forEach((rowIdx) => next.delete(rowIdx));
        return next;
      });
    }

    setSaveAttemptReport((prev) => {
      if (!prev) return prev;
      return {
        ...prev,
        failures: prev.failures.map((failure) => recoverableKeys.has(getSaveFailureKey(failure))
          ? { ...failure, recoveryNote: undefined }
          : failure),
      };
    });
    setPendingStaleRecovery({
      items: recoverableFailures,
      sawRefreshing: false,
      sourceDataRevision: dataRevision,
    });
    setEditingCell(null);
    setContextMenu(null);
    setFilterMenu(null);
    onRefresh();
    emitToast(
      `Refreshing latest server data to recover ${recoverableFailures.length} stale change${recoverableFailures.length === 1 ? '' : 's'}.`,
      'success',
    );
  }, [pendingStaleRecovery, getSaveFailureKey, dataRevision, onRefresh]);

  const handleRecoverAllStaleFailures = useCallback(() => {
    queueStaleRecovery(
      (saveAttemptReport?.failures || []).filter((failure) => failure.kind === 'stale_row'),
      {
        emptyMessage: 'No stale-row conflicts to recover.',
      },
    );
  }, [queueStaleRecovery, saveAttemptReport]);

  const handleRefreshVisibleNeedsRefreshStaleConflicts = useCallback(() => {
    queueStaleRecovery(
      visibleStaleFailureSummaries.filter((summary) => summary.needsRefresh).map((summary) => summary.failure),
      {
        emptyMessage: 'No visible stale conflicts need a refresh right now.',
      },
    );
  }, [queueStaleRecovery, visibleStaleFailureSummaries]);

  const handleRefreshStaleConflictGroup = useCallback((groupSummaries: typeof visibleStaleFailureSummaries, label: string) => {
    queueStaleRecovery(
      groupSummaries.filter((summary) => summary.needsRefresh).map((summary) => summary.failure),
      {
        emptyMessage: `No ${label.toLowerCase()} stale conflicts need a refresh right now.`,
      },
    );
  }, [queueStaleRecovery]);

  const focusSaveFailure = useCallback((failure: SaveFailureItem) => {
    setShowSaveReviewModal(false);
    setShowStaleConflictOverview(false);
    setContextMenu(null);
    setFilterMenu(null);

    window.requestAnimationFrame(() => {
      if (failure.isNew) {
        parentRef.current?.scrollTo({
          top: rowVirtualizer.getTotalSize() + failure.rowIdx * APPROX_ROW_HEIGHT,
          behavior: 'smooth',
        });
      } else {
        const visibleIndex = visibleRowIndices.indexOf(failure.rowIdx);
        if (visibleIndex >= 0) {
          rowVirtualizer.scrollToIndex(visibleIndex, { align: 'center' });
        }
      }
      if (failure.col) {
        setEditingCell({ rowIdx: failure.rowIdx, col: failure.col, isNew: failure.isNew });
      }
    });
  }, [rowVirtualizer, visibleRowIndices]);

  const handleDiscardFailure = useCallback((failure: SaveFailureItem) => {
    discardSaveFailures(
      (item) =>
        item.action === failure.action
        && item.kind === failure.kind
        && item.rowIdx === failure.rowIdx
        && item.isNew === failure.isNew
        && (item.col || '') === (failure.col || ''),
      {
        successMessage: `${failure.summary} discarded from the pending change set.`,
      },
    );
  }, [discardSaveFailures]);

  const handleRecoverStaleFailure = useCallback((failure: SaveFailureItem) => {
    queueStaleRecovery(
      [failure],
      {
        emptyMessage: 'This stale change cannot be recovered automatically.',
      },
    );
  }, [queueStaleRecovery]);

  const handleRefreshStaleConflictContext = useCallback(() => {
    if (!activeStaleConflict) return;
    onRefresh();
    emitToast('Refreshing the current page to load the latest server values for this conflict.', 'success');
  }, [activeStaleConflict, onRefresh]);

  const resolveActiveStaleConflict = useCallback((options?: {
    selectionMode?: 'current' | 'recommended';
    advanceToNext?: boolean;
  }) => {
    if (!activeStaleConflict || activeStaleConflict.kind !== 'stale_row' || !activeStaleConflictDetails) return;
    if (activeStaleConflictDetails.locatedRowIdx < 0 || !activeStaleConflictDetails.latestRowData) {
      emitToast('Refresh the current page until the latest server row is visible before resolving this conflict.', 'error');
      return;
    }
    const nextFailure = options?.advanceToNext && activeStaleConflictScopedIndex >= 0
      ? activeStaleConflictReviewFailures[activeStaleConflictScopedIndex + 1] || activeStaleConflictReviewFailures[activeStaleConflictScopedIndex - 1] || null
      : null;
    const nextLabel = nextFailure ? ' Opened the next stale conflict.' : '';

    if (activeStaleConflict.action === 'delete') {
      setDeletedRowIdxs((prev) => {
        const next = new Set(prev);
        next.delete(activeStaleConflict.rowIdx);
        next.add(activeStaleConflictDetails.locatedRowIdx);
        return next;
      });
      if (nextFailure) {
        handleOpenStaleConflict(nextFailure, {
          scope: buildStaleConflictReviewScope(activeStaleConflictReviewFailures, staleConflictReviewScope?.label || 'All Stale Conflicts'),
        });
      } else {
        handleCloseStaleConflict();
      }
      clearResolvedFailuresFromReport([activeStaleConflict]);
      emitToast(`Marked the refreshed server row for delete again.${nextLabel}`, 'success');
      return;
    }

    const pendingRowData = activeStaleConflict.staleRecovery?.pendingRowData || {};
    const changedColumns = activeStaleConflict.staleRecovery?.changedColumns || [];
    const latestRowData = activeStaleConflictDetails.latestRowData;
    const effectiveSelections = options?.selectionMode === 'recommended'
      ? recommendedStaleConflictSelections
      : staleConflictSelections;
    const nextRowData = { ...latestRowData };
    let keptPendingCount = 0;

    changedColumns.forEach((column) => {
      if ((effectiveSelections[column] || 'pending') === 'pending') {
        nextRowData[column] = pendingRowData[column];
        if (!areCellValuesEqual(nextRowData[column], latestRowData[column])) {
          keptPendingCount += 1;
        }
      }
    });

    setModifiedRows((prev) => {
      const next = { ...prev };
      delete next[activeStaleConflict.rowIdx];
      if (keptPendingCount > 0) {
        next[activeStaleConflictDetails.locatedRowIdx] = nextRowData;
      } else {
        delete next[activeStaleConflictDetails.locatedRowIdx];
      }
      return next;
    });
    if (nextFailure) {
      handleOpenStaleConflict(nextFailure, {
        scope: buildStaleConflictReviewScope(activeStaleConflictReviewFailures, staleConflictReviewScope?.label || 'All Stale Conflicts'),
      });
    } else {
      handleCloseStaleConflict();
    }
    clearResolvedFailuresFromReport([activeStaleConflict]);
    emitToast(
      keptPendingCount > 0
        ? `Rebased ${keptPendingCount} edited column${keptPendingCount === 1 ? '' : 's'} onto the latest server row.${nextLabel}`
        : `Resolved this conflict to the latest server values and cleared the stale edit.${nextLabel}`,
      'success',
    );
  }, [
    activeStaleConflict,
    activeStaleConflictDetails,
    activeStaleConflictScopedIndex,
    activeStaleConflictReviewFailures,
    handleOpenStaleConflict,
    buildStaleConflictReviewScope,
    staleConflictReviewScope,
    recommendedStaleConflictSelections,
    staleConflictSelections,
    areCellValuesEqual,
    clearResolvedFailuresFromReport,
    handleCloseStaleConflict,
  ]);

  const handleResolveStaleConflictWithCurrentRow = useCallback(() => {
    resolveActiveStaleConflict();
  }, [resolveActiveStaleConflict]);

  const handleResolveStaleConflictWithCurrentRowAndNext = useCallback(() => {
    resolveActiveStaleConflict({ advanceToNext: true });
  }, [resolveActiveStaleConflict]);

  const handleApplyRecommendedStaleConflictResolution = useCallback(() => {
    resolveActiveStaleConflict({ selectionMode: 'recommended' });
  }, [resolveActiveStaleConflict]);

  const handleApplyRecommendedStaleConflictResolutionAndNext = useCallback(() => {
    resolveActiveStaleConflict({ selectionMode: 'recommended', advanceToNext: true });
  }, [resolveActiveStaleConflict]);

  const handleUseLatestServerCopyForConflict = useCallback((advanceToNext = false) => {
    if (!activeStaleConflict) return;
    const nextFailure = advanceToNext && activeStaleConflictScopedIndex >= 0
      ? activeStaleConflictReviewFailures[activeStaleConflictScopedIndex + 1] || activeStaleConflictReviewFailures[activeStaleConflictScopedIndex - 1] || null
      : null;
    discardSaveFailures(
      (item) =>
        item.action === activeStaleConflict.action
        && item.kind === activeStaleConflict.kind
        && item.rowIdx === activeStaleConflict.rowIdx
        && item.isNew === activeStaleConflict.isNew
        && (item.col || '') === (activeStaleConflict.col || ''),
      {
        successMessage: `${activeStaleConflict.summary} dropped and replaced with the latest server copy.${nextFailure ? ' Opened the next stale conflict.' : ''}`,
      },
    );
    if (nextFailure) {
      handleOpenStaleConflict(nextFailure, {
        scope: buildStaleConflictReviewScope(activeStaleConflictReviewFailures, staleConflictReviewScope?.label || 'All Stale Conflicts'),
      });
    } else {
      handleCloseStaleConflict();
    }
  }, [
    activeStaleConflict,
    activeStaleConflictScopedIndex,
    activeStaleConflictReviewFailures,
    discardSaveFailures,
    handleOpenStaleConflict,
    handleCloseStaleConflict,
    buildStaleConflictReviewScope,
    staleConflictReviewScope,
  ]);

  const handleUseLatestServerCopyForConflictAndNext = useCallback(() => {
    handleUseLatestServerCopyForConflict(true);
  }, [handleUseLatestServerCopyForConflict]);

  useEffect(() => {
    if (!activeStaleConflictKey) return;
    if (activeStaleConflict) return;
    handleCloseStaleConflict();
  }, [activeStaleConflictKey, activeStaleConflict, handleCloseStaleConflict]);

  useEffect(() => {
    if (showStaleConflictOverview && staleFailures.length === 0) {
      setShowStaleConflictOverview(false);
      setStaleConflictQueueFilter('all');
      setStaleConflictOverviewCollapsedGroups(createStaleConflictOverviewCollapsedState());
    }
  }, [showStaleConflictOverview, staleFailures.length]);

  useEffect(() => {
    if (!pendingStaleRecovery || pendingStaleRecovery.sawRefreshing || !isRefreshing) return;
    setPendingStaleRecovery((prev) => prev ? { ...prev, sawRefreshing: true } : prev);
  }, [pendingStaleRecovery, isRefreshing]);

  useEffect(() => {
    if (!pendingStaleRecovery?.sawRefreshing || isRefreshing) return;

    if (dataRevision === pendingStaleRecovery.sourceDataRevision) {
      if (refreshError) {
        setPendingStaleRecovery(null);
        emitToast(`Could not refresh the latest server data: ${refreshError}`, 'error');
      }
      return;
    }

    const resolvedKeys = new Set<string>();
    const unresolvedNotes = new Map<string, string>();
    const updateReapplyQueue: Array<{ rowIdx: number; rowData: Record<string, any> }> = [];
    const deleteReapplyRowIdxs: number[] = [];
    let reappliedEditCount = 0;
    let alreadyCurrentEditCount = 0;
    let remarkDeleteCount = 0;

    pendingStaleRecovery.items.forEach((failure) => {
      const recoveryKey = getSaveFailureKey(failure);
      const recoveredRowIdx = findRecoveryRowIndex(failure);
      if (recoveredRowIdx < 0) {
        unresolvedNotes.set(
          recoveryKey,
          failure.action === 'delete'
            ? 'Automatic recovery could not find this row on the refreshed page. Clear filters or navigate to the row, then retry or discard the stale delete.'
            : 'Automatic recovery could not find this row on the refreshed page. Clear filters or navigate to the row, then retry or discard the stale edit.',
        );
        return;
      }

      resolvedKeys.add(recoveryKey);

      if (failure.action === 'delete') {
        deleteReapplyRowIdxs.push(recoveredRowIdx);
        remarkDeleteCount += 1;
        return;
      }

      const staleRecovery = failure.staleRecovery;
      const pendingRowData = staleRecovery?.pendingRowData;
      if (!pendingRowData) {
        alreadyCurrentEditCount += 1;
        return;
      }

      const changedColumns = staleRecovery?.changedColumns || [];
      const refreshedRow = data[recoveredRowIdx] || {};
      const rebasedRowData = { ...refreshedRow };
      changedColumns.forEach((column) => {
        rebasedRowData[column] = pendingRowData[column];
      });
      const stillDirty = changedColumns.some((column) => !areCellValuesEqual(rebasedRowData[column], refreshedRow[column]));

      if (stillDirty) {
        updateReapplyQueue.push({ rowIdx: recoveredRowIdx, rowData: rebasedRowData });
        reappliedEditCount += 1;
      } else {
        alreadyCurrentEditCount += 1;
      }
    });

    if (updateReapplyQueue.length > 0) {
      setModifiedRows((prev) => {
        const next = { ...prev };
        updateReapplyQueue.forEach(({ rowIdx, rowData }) => {
          next[rowIdx] = rowData;
        });
        return next;
      });
    }

    if (deleteReapplyRowIdxs.length > 0) {
      setDeletedRowIdxs((prev) => {
        const next = new Set(prev);
        deleteReapplyRowIdxs.forEach((rowIdx) => next.add(rowIdx));
        return next;
      });
    }

    setSaveAttemptReport((prev) => {
      if (!prev) return prev;
      const remainingFailures = prev.failures
        .filter((failure) => !resolvedKeys.has(getSaveFailureKey(failure)))
        .map((failure) => {
          const recoveryNote = unresolvedNotes.get(getSaveFailureKey(failure));
          return recoveryNote ? { ...failure, recoveryNote } : failure;
        });

      if (remainingFailures.length === 0) return null;

      return {
        attempted: prev.succeeded + remainingFailures.length,
        succeeded: prev.succeeded,
        failed: remainingFailures.length,
        failures: remainingFailures,
      };
    });

    const successParts = [];
    if (reappliedEditCount > 0) {
      successParts.push(`${reappliedEditCount} edit${reappliedEditCount === 1 ? '' : 's'} re-applied`);
    }
    if (alreadyCurrentEditCount > 0) {
      successParts.push(`${alreadyCurrentEditCount} edit${alreadyCurrentEditCount === 1 ? '' : 's'} already matched the refreshed server row`);
    }
    if (remarkDeleteCount > 0) {
      successParts.push(`${remarkDeleteCount} delete${remarkDeleteCount === 1 ? '' : 's'} re-marked`);
    }
    if (successParts.length > 0) {
      emitToast(`Stale recovery complete: ${successParts.join(', ')}.`, 'success');
    }
    if (unresolvedNotes.size > 0) {
      emitToast(
        `${unresolvedNotes.size} stale change${unresolvedNotes.size === 1 ? '' : 's'} could not be reattached on the refreshed page.`,
        'error',
      );
    }

    setPendingStaleRecovery(null);
  }, [
    pendingStaleRecovery,
    isRefreshing,
    refreshError,
    dataRevision,
    getSaveFailureKey,
    findRecoveryRowIndex,
    data,
    areCellValuesEqual,
  ]);

  return (
    <div className="flex flex-col h-full relative">
      {isDirty && (
        <div className="flex items-center gap-2 mb-4 bg-blue-900/20 p-2 rounded border border-blue-500/30">
          <span className="text-sm text-blue-300 flex-1">
            {`You have unsaved changes | ${saveReviewCounts.inserted} insert${saveReviewCounts.inserted === 1 ? '' : 's'} | ${saveReviewCounts.updated} update${saveReviewCounts.updated === 1 ? '' : 's'} | ${saveReviewCounts.deleted} delete${saveReviewCounts.deleted === 1 ? '' : 's'}.`}
          </span>
          <button
            onClick={handleUndo}
            disabled={isSaving || isRefreshing}
            className="flex items-center gap-1 px-3 py-1 bg-gray-700 hover:bg-gray-600 rounded text-sm text-white transition-colors disabled:opacity-50"
          >
            <Undo className="w-4 h-4" /> Undo
          </button>
          <button
            onClick={requestSaveReview}
            disabled={isSaving || isRefreshing}
            className="flex items-center gap-1 px-3 py-1 bg-blue-600 hover:bg-blue-500 rounded text-sm text-white transition-colors disabled:opacity-50"
          >
            <Save className="w-4 h-4" /> Review & Save
          </button>
        </div>
      )}

      {validationIssueCount > 0 && (
        <div className="mb-4 flex items-center gap-3 rounded border border-red-500/30 bg-red-900/20 p-3">
          <div className="flex-1">
            <div className="text-sm font-medium text-red-200">
              {`${validationIssueCount} validation error${validationIssueCount === 1 ? '' : 's'} must be fixed before saving.`}
            </div>
            <div className="mt-1 text-xs text-red-200/80">
              {validationIssues[0]
                ? `${validationIssues[0].isNew ? 'Draft row' : 'Row'} ${validationIssues[0].rowIdx + 1}, ${validationIssues[0].col}: ${validationIssues[0].message}`
                : 'Review the highlighted cells in the grid.'}
            </div>
          </div>
          <button
            onClick={focusFirstValidationIssue}
            className="rounded border border-red-400/30 bg-red-500/10 px-3 py-1.5 text-xs text-red-100 hover:bg-red-500/20 transition-colors"
          >
            Jump to First Error
          </button>
        </div>
      )}

      {saveAttemptReport && saveAttemptReport.failed > 0 && (
        <div className="mb-4 flex items-center gap-3 rounded border border-amber-500/30 bg-amber-900/20 p-3">
          <div className="flex-1">
            <div className="text-sm font-medium text-amber-100">
              {saveAttemptReport.succeeded > 0
                ? `Partial save completed: ${saveAttemptReport.succeeded} succeeded, ${saveAttemptReport.failed} failed.`
                : `Last save attempt failed for ${saveAttemptReport.failed} pending change${saveAttemptReport.failed === 1 ? '' : 's'}.`}
            </div>
            <div className="mt-1 text-xs text-amber-100/80">
              {saveAttemptReport.failures[0]
                ? `[${getSaveFailureKindLabel(saveAttemptReport.failures[0].kind)}] ${saveAttemptReport.failures[0].summary}: ${saveAttemptReport.failures[0].message}`
                : 'Review the save failure details and retry or undo the remaining changes.'}
              {saveAttemptReport.failures.some((failure) => failure.kind === 'stale_row')
                ? ' Some rows no longer matched the original server snapshot; refresh that data before retrying.'
                : ''}
              {saveAttemptReport.succeeded > 0 ? ' Successful operations were removed from the pending draft set; review remaining failures before refreshing.' : ''}
              {pendingStaleRecovery ? ' Refreshing the latest server data to reattach stale changes...' : ''}
            </div>
          </div>
          <div className="flex flex-wrap items-center gap-2">
            {firstStaleFailure && (
              <button
                onClick={() => handleOpenStaleConflictOverview('all')}
                disabled={Boolean(pendingStaleRecovery)}
                className="rounded border border-blue-400/30 bg-[#161b22] px-3 py-1.5 text-xs text-blue-50 hover:bg-[#21262d] transition-colors disabled:opacity-50"
              >
                Conflict Queue
              </button>
            )}
            {firstStaleFailure && (
              <button
                onClick={handleOpenFirstStaleConflict}
                disabled={Boolean(pendingStaleRecovery) || isRefreshing}
                className="rounded border border-blue-400/30 bg-blue-500/10 px-3 py-1.5 text-xs text-blue-50 hover:bg-blue-500/20 transition-colors disabled:opacity-50"
              >
                Review First Conflict
              </button>
            )}
            {saveAttemptReport.failures.some((failure) => failure.kind === 'stale_row') && (
              <button
                onClick={handleRecoverAllStaleFailures}
                disabled={Boolean(pendingStaleRecovery) || isRefreshing}
                className="rounded border border-blue-400/30 bg-blue-500/10 px-3 py-1.5 text-xs text-blue-50 hover:bg-blue-500/20 transition-colors disabled:opacity-50"
              >
                {pendingStaleRecovery ? 'Refreshing Stale...' : 'Refresh & Recover Stale'}
              </button>
            )}
            {saveAttemptReport.failures.some((failure) => failure.kind === 'stale_row') && (
              <button
                onClick={handleDiscardAllStaleFailures}
                disabled={Boolean(pendingStaleRecovery)}
                className="rounded border border-amber-400/30 bg-amber-500/10 px-3 py-1.5 text-xs text-amber-50 hover:bg-amber-500/20 transition-colors"
              >
                Drop Stale
              </button>
            )}
            <button
              onClick={handleDiscardAllFailedChanges}
              disabled={Boolean(pendingStaleRecovery)}
              className="rounded border border-amber-400/30 bg-[#161b22] px-3 py-1.5 text-xs text-amber-50 hover:bg-[#21262d] transition-colors disabled:opacity-50"
            >
              Drop Failed
            </button>
            <button
              onClick={() => setShowSaveReviewModal(true)}
              className="rounded border border-amber-400/30 bg-amber-500/10 px-3 py-1.5 text-xs text-amber-50 hover:bg-amber-500/20 transition-colors"
            >
              Review Failures
            </button>
            <button
              onClick={handleReloadServerCopy}
              disabled={Boolean(pendingStaleRecovery)}
              className="rounded border border-[#30363d] bg-[#161b22] px-3 py-1.5 text-xs text-gray-200 hover:bg-[#21262d] transition-colors disabled:opacity-50"
            >
              Reload Server Copy
            </button>
          </div>
        </div>
      )}

      <div ref={parentRef} className="flex-1 overflow-auto rounded border border-dark-border bg-[#0d1117]">
        <table className="w-full text-left text-sm whitespace-nowrap">
          <thead className="bg-[#161b22] sticky top-0 shadow-sm text-gray-400 text-xs tracking-wider z-20">
            <tr>
              {visibleColumns.map((k: string) => {
                const sortItem = sorts.find(s => s.column === k);
                const hasFilter = filters.some(f => f.column === k);
                
                return (
                  <th 
                    key={k} 
                    className="py-2 px-3 font-medium border-r border-[#30363d] relative select-none"
                    style={{ width: `${getColumnWidth(k)}px`, minWidth: `${getColumnWidth(k)}px`, maxWidth: `${getColumnWidth(k)}px` }}
                  >
                    <div className="flex items-center justify-between gap-2">
                      <div 
                        className="flex-1 cursor-pointer hover:text-white flex items-center gap-1"
                        onClick={(e) => handleSort(k, e)}
                        title="Shift+Click for multi-column sort"
                      >
                        <span>{k}</span>
                        <span className="flex-shrink-0 text-blue-400">
                          {sortItem && (sortItem.desc ? <ArrowDown className="w-3 h-3" /> : <ArrowUp className="w-3 h-3" />)}
                        </span>
                      </div>
                      <div 
                        className={`cursor-pointer p-1 rounded hover:bg-gray-700 ${hasFilter ? 'text-blue-400' : 'text-gray-500'}`}
                        onClick={(e) => { e.stopPropagation(); setFilterMenu({ col: k }); }}
                      >
                        <Filter className="w-3 h-3" />
                      </div>
                    </div>
                    <div
                      className="absolute top-0 right-0 h-full w-1.5 cursor-col-resize hover:bg-blue-500/30"
                      onMouseDown={(event) => {
                        event.stopPropagation();
                        setResizingColumn({ column: k, startX: event.clientX, startWidth: getColumnWidth(k) });
                      }}
                    />
                    
                    {filterMenu?.col === k && (
                      <FilterDropdown 
                        col={k} 
                        filters={filters} 
                        setFilters={setFilters} 
                        onClose={() => setFilterMenu(null)} 
                      />
                    )}
                  </th>
                );
              })}
            </tr>
          </thead>
          <tbody className="divide-y divide-[#30363d]/50">
            {rowVirtualizer.getVirtualItems().length > 0 && rowVirtualizer.getVirtualItems()[0].start > 0 && (
              <tr>
                <td colSpan={visibleColumns.length} style={{ height: `${rowVirtualizer.getVirtualItems()[0].start}px` }} />
              </tr>
            )}
            {rowVirtualizer.getVirtualItems().map((virtualRow) => {
              const i = visibleRowIndices[virtualRow.index];
              const row = data[i];
              
              const isModifiedRow = !!modifiedRows[i];
              const displayRow = modifiedRows[i] || row;
              
              return (
                <tr 
                  key={virtualRow.key}
                  data-index={virtualRow.index}
                  ref={rowVirtualizer.measureElement}
                  className={`hover:bg-[#161b22] even:bg-[#0d1117] ${isModifiedRow ? 'bg-blue-900/10' : ''} ${existingValidationRowSet.has(i) ? 'bg-red-900/5' : ''} ${existingSaveFailureRowSet.has(i) ? 'bg-amber-900/5' : ''}`}
                >
                  {visibleColumns.map((k: string) => {
                    const validationMessage = validationIssueMap.get(buildValidationCellKey(i, k, false));
                    const hasSaveFailure = saveFailureCellKeys.has(buildValidationCellKey(i, k, false));
                    const saveFailureMessage = saveFailureMessageMap.get(buildValidationCellKey(i, k, false));
                    const isChangedCell = isModifiedRow && !areCellValuesEqual(displayRow[k], row[k]);
                    const titleText = validationMessage
                      ? `${formatCellValue(displayRow[k])}\nValidation: ${validationMessage}`
                      : hasSaveFailure
                        ? `${formatCellValue(displayRow[k])}\n${saveFailureMessage || 'Last save attempt failed for this row.'}`
                      : formatCellValue(displayRow[k]);

                    return (
                      <td 
                        key={k} 
                        className={`py-1 px-3 border-r border-[#30363d]/50 max-w-[300px] truncate ${isChangedCell ? 'bg-blue-500/20' : ''} ${validationMessage ? 'bg-red-500/10 ring-1 ring-inset ring-red-500/40' : ''} ${!validationMessage && hasSaveFailure ? 'bg-amber-500/10 ring-1 ring-inset ring-amber-500/40' : ''}`}
                        style={{ width: `${getColumnWidth(k)}px`, minWidth: `${getColumnWidth(k)}px`, maxWidth: `${getColumnWidth(k)}px` }}
                        onDoubleClick={() => handleCellDoubleClick(i, k, false)}
                        onContextMenu={(e) => {
                          e.preventDefault();
                          setContextMenu({ x: e.clientX, y: e.clientY, rowIdx: i, col: k, isNew: false });
                        }}
                      >
                        {editingCell?.rowIdx === i && editingCell?.col === k && !editingCell.isNew ? (
                          <input
                            autoFocus
                            type="text"
                            title={validationMessage || undefined}
                            className={`w-full bg-black text-white px-1 border rounded outline-none ${validationMessage ? 'border-red-500' : 'border-blue-500'}`}
                            value={displayRow[k] === null ? '' : displayRow[k]}
                            onChange={(e) => handleCellChange(e.target.value, i, k, false)}
                            onBlur={() => setEditingCell(null)}
                            onKeyDown={(e) => { if (e.key === 'Enter') setEditingCell(null); }}
                          />
                        ) : (
                          <CellDisplay
                            val={displayRow[k]}
                            inlineValue={formatInlineCellValue(displayRow[k])}
                            titleText={titleText}
                            invalidMessage={validationMessage}
                          />
                        )}
                      </td>
                    );
                  })}
                </tr>
              );
            })}
            {rowVirtualizer.getVirtualItems().length > 0 && 
             rowVirtualizer.getTotalSize() - rowVirtualizer.getVirtualItems()[rowVirtualizer.getVirtualItems().length - 1].end > 0 && (
              <tr>
                <td colSpan={visibleColumns.length} style={{ height: `${rowVirtualizer.getTotalSize() - rowVirtualizer.getVirtualItems()[rowVirtualizer.getVirtualItems().length - 1].end}px` }} />
              </tr>
            )}
            
            {newRows.map((row: any, i: number) => {
              const hasRowValidationIssue = newValidationRowSet.has(i);
              const hasRowSaveFailure = newSaveFailureRowSet.has(i);
              return (
                <tr 
                  key={`new-${i}`} 
                  className={`bg-green-900/10 hover:bg-green-900/20 ${hasRowValidationIssue ? 'bg-red-900/10' : ''} ${hasRowSaveFailure ? 'bg-amber-900/10' : ''}`}
                >
                  {visibleColumns.map((k: string) => {
                    const validationMessage = validationIssueMap.get(buildValidationCellKey(i, k, true));
                    const hasSaveFailure = saveFailureCellKeys.has(buildValidationCellKey(i, k, true));
                    const saveFailureMessage = saveFailureMessageMap.get(buildValidationCellKey(i, k, true));
                    const titleText = validationMessage
                      ? `${formatCellValue(row[k])}\nValidation: ${validationMessage}`
                      : hasSaveFailure
                        ? `${formatCellValue(row[k])}\n${saveFailureMessage || 'Last save attempt failed for this draft row.'}`
                      : formatCellValue(row[k]);

                    return (
                      <td 
                        key={k} 
                        className={`py-1 px-3 border-r border-green-500/30 max-w-[300px] truncate ${validationMessage ? 'bg-red-500/10 ring-1 ring-inset ring-red-500/40' : ''} ${!validationMessage && hasSaveFailure ? 'bg-amber-500/10 ring-1 ring-inset ring-amber-500/40' : ''}`}
                        style={{ width: `${getColumnWidth(k)}px`, minWidth: `${getColumnWidth(k)}px`, maxWidth: `${getColumnWidth(k)}px` }}
                        onDoubleClick={() => handleCellDoubleClick(i, k, true)}
                        onContextMenu={(e) => {
                          e.preventDefault();
                          setContextMenu({ x: e.clientX, y: e.clientY, rowIdx: i, col: k, isNew: true });
                        }}
                      >
                        {editingCell?.rowIdx === i && editingCell?.col === k && editingCell.isNew ? (
                          <input
                            autoFocus
                            type="text"
                            title={validationMessage || undefined}
                            className={`w-full bg-black text-white px-1 border rounded outline-none ${validationMessage ? 'border-red-500' : 'border-green-500'}`}
                            value={row[k] ?? ''}
                            onChange={(e) => handleCellChange(e.target.value, i, k, true)}
                            onBlur={() => setEditingCell(null)}
                            onKeyDown={(e) => { if (e.key === 'Enter') setEditingCell(null); }}
                          />
                        ) : (
                          <CellDisplay
                            val={row[k]}
                            isNewEmpty={row[k] === '' || row[k] === undefined}
                            inlineValue={formatInlineCellValue(row[k])}
                            titleText={titleText}
                            invalidMessage={validationMessage}
                          />
                        )}
                      </td>
                    );
                  })}
                </tr>
              );
            })}
          </tbody>
        </table>
        
        <div className="p-3 border-t border-[#30363d] flex justify-between items-center gap-3">
          <div className="flex items-center gap-2 flex-wrap">
            <button 
              onClick={handleAddNewRow}
              className="flex items-center gap-1 text-sm text-gray-400 hover:text-white px-3 py-1.5 rounded hover:bg-[#21262d] transition-colors"
            >
              <Plus className="w-4 h-4" /> Add Row
            </button>
            <button
              onClick={() => setShowPasteModal(true)}
              className="flex items-center gap-1 text-sm text-gray-400 hover:text-white px-3 py-1.5 rounded hover:bg-[#21262d] transition-colors"
            >
              <Copy className="w-4 h-4" /> Paste Rows
            </button>
            <div className="relative" onClick={(event) => event.stopPropagation()}>
              <button
                onClick={() => setShowColumnMenu((prev) => !prev)}
                className="text-xs text-gray-400 hover:text-white bg-[#21262d] hover:bg-[#30363d] px-3 py-1.5 rounded border border-[#30363d] transition-colors"
              >
                Columns
              </button>
              {showColumnMenu && (
                <div className="absolute bottom-full left-0 mb-2 w-72 rounded border border-[#30363d] bg-[#161b22] p-3 shadow-2xl z-30">
                  <div className="mb-2 flex items-center justify-between">
                    <div className="text-xs font-semibold uppercase tracking-wide text-gray-400">Column Layout</div>
                    <button
                      onClick={resetColumnLayout}
                      className="text-[11px] text-blue-300 hover:text-white transition-colors"
                    >
                      Reset
                    </button>
                  </div>
                  <div className="max-h-64 space-y-2 overflow-auto pr-1">
                    {orderedColumns.map((column, index) => {
                      const isVisible = !columnLayout.hidden.includes(column);
                      return (
                        <div key={column} className="flex items-center gap-2 rounded border border-[#30363d] bg-[#0d1117] px-2 py-1.5">
                          <input
                            type="checkbox"
                            checked={isVisible}
                            onChange={() => toggleColumnVisibility(column)}
                            className="accent-blue-500"
                          />
                          <span className="min-w-0 flex-1 truncate text-xs text-gray-200" title={column}>{column}</span>
                          <span className="text-[10px] text-gray-500">{Math.round(getColumnWidth(column))} px</span>
                          <button
                            onClick={() => moveColumn(column, 'left')}
                            disabled={index === 0}
                            className="rounded border border-[#30363d] px-1 py-0.5 text-[10px] text-gray-300 hover:text-white disabled:opacity-30"
                            title="Move left"
                          >
                            <ArrowUp className="w-3 h-3 rotate-[-90deg]" />
                          </button>
                          <button
                            onClick={() => moveColumn(column, 'right')}
                            disabled={index === orderedColumns.length - 1}
                            className="rounded border border-[#30363d] px-1 py-0.5 text-[10px] text-gray-300 hover:text-white disabled:opacity-30"
                            title="Move right"
                          >
                            <ArrowDown className="w-3 h-3 rotate-[-90deg]" />
                          </button>
                        </div>
                      );
                    })}
                  </div>
                  <div className="mt-2 text-[11px] text-gray-500">
                    Drag column edges in the header to resize. Layout is saved locally.
                  </div>
                </div>
              )}
            </div>
            <button
              onClick={resetColumnLayout}
              disabled={visibleColumns.length === columns.length && orderedColumns.every((column, index) => column === columns[index]) && Object.keys(columnLayout.widths).length === 0}
              className="text-xs text-gray-400 hover:text-white bg-[#21262d] hover:bg-[#30363d] px-3 py-1.5 rounded border border-[#30363d] transition-colors disabled:opacity-50 disabled:hover:bg-[#21262d] disabled:hover:text-gray-400"
            >
              Reset Layout
            </button>
            <button
              onClick={clearGridTweaks}
              disabled={!hasGridTweaks}
              className="text-xs text-gray-400 hover:text-white bg-[#21262d] hover:bg-[#30363d] px-3 py-1.5 rounded border border-[#30363d] transition-colors disabled:opacity-50 disabled:hover:bg-[#21262d] disabled:hover:text-gray-400"
            >
              Clear Filters/Sorts
            </button>
            {hasGridTweaks && (
              <span className="text-[11px] text-gray-500">
                {hasActiveFilters ? `${filters.length} filter${filters.length > 1 ? 's' : ''}` : '0 filters'}
                {hasActiveSorts ? ` · ${sorts.length} sort${sorts.length > 1 ? 's' : ''}` : ''}
              </span>
            )}
          </div>
          <div className="flex gap-2">
            <button 
              onClick={downloadCsv}
              className="text-xs text-gray-400 hover:text-white bg-[#21262d] hover:bg-[#30363d] px-3 py-1.5 rounded border border-[#30363d] transition-colors"
            >
              Download CSV
            </button>
            <button 
              onClick={downloadSql}
              className="text-xs text-gray-400 hover:text-white bg-[#21262d] hover:bg-[#30363d] px-3 py-1.5 rounded border border-[#30363d] transition-colors"
            >
              Download SQL
            </button>
          </div>
        </div>
      </div>

      {showSaveReviewModal && (
        <div className="fixed inset-0 z-[68] bg-black/60 backdrop-blur-sm flex items-center justify-center p-4">
          <div className="w-full max-w-5xl max-h-[85vh] bg-[#161b22] border border-[#30363d] rounded-lg shadow-2xl overflow-hidden flex flex-col">
            <div className="flex items-center justify-between px-4 py-3 border-b border-[#30363d]">
              <div>
                <div className="text-sm font-medium text-white">Review Pending Table Changes</div>
                <div className="text-xs text-gray-400">
                  Preview inserts, updates, and deletes before writing them to {tableName}.
                </div>
              </div>
              <button
                onClick={() => !isSaving && setShowSaveReviewModal(false)}
                disabled={isSaving}
                className="text-gray-400 hover:text-white transition-colors disabled:opacity-50"
              >
                <X className="w-4 h-4" />
              </button>
            </div>

            <div className="grid gap-3 border-b border-[#30363d] bg-[#0d1117] px-4 py-3 md:grid-cols-4">
              <div className="rounded border border-[#30363d] bg-[#161b22] px-3 py-2">
                <div className="text-[11px] uppercase tracking-wide text-gray-500">Inserts</div>
                <div className="mt-1 text-lg font-semibold text-green-300">{saveReviewCounts.inserted}</div>
              </div>
              <div className="rounded border border-[#30363d] bg-[#161b22] px-3 py-2">
                <div className="text-[11px] uppercase tracking-wide text-gray-500">Updates</div>
                <div className="mt-1 text-lg font-semibold text-blue-300">{saveReviewCounts.updated}</div>
              </div>
              <div className="rounded border border-[#30363d] bg-[#161b22] px-3 py-2">
                <div className="text-[11px] uppercase tracking-wide text-gray-500">Deletes</div>
                <div className="mt-1 text-lg font-semibold text-red-300">{saveReviewCounts.deleted}</div>
              </div>
              <div className="rounded border border-[#30363d] bg-[#161b22] px-3 py-2">
                <div className="text-[11px] uppercase tracking-wide text-gray-500">Total</div>
                <div className="mt-1 text-lg font-semibold text-white">{saveReviewCounts.total}</div>
              </div>
            </div>

            <div className="flex-1 overflow-auto p-4 space-y-4">
              {saveAttemptReport && saveAttemptReport.failed > 0 && (
                <section className="rounded border border-amber-500/30 bg-amber-500/5">
                  <div className="flex items-center justify-between border-b border-amber-500/20 px-4 py-2">
                    <div className="text-sm font-medium text-amber-100">Last save attempt</div>
                    <div className="text-xs text-amber-200/80">
                      {`${saveAttemptReport.succeeded} succeeded / ${saveAttemptReport.failed} failed`}
                    </div>
                  </div>
                  <div className="space-y-2 p-4">
                    <div className="text-xs text-amber-100/80">
                      {saveAttemptReport.succeeded > 0
                        ? 'Successful operations were removed from the pending draft set. Review the remaining failed rows below before retrying.'
                        : 'No operations were persisted. Review the failure details below, then fix and retry.'}
                      {pendingStaleRecovery ? ' Refreshing latest server data to recover stale rows...' : ''}
                    </div>
                    <div className="flex flex-wrap items-center gap-2">
                      {firstStaleFailure && (
                        <button
                          type="button"
                          onClick={() => handleOpenStaleConflictOverview('all')}
                          disabled={Boolean(pendingStaleRecovery)}
                          className="rounded border border-blue-400/30 bg-[#161b22] px-2.5 py-1 text-[11px] text-blue-50 hover:bg-[#21262d] transition-colors disabled:opacity-50"
                        >
                          Conflict Queue
                        </button>
                      )}
                      {firstStaleFailure && (
                        <button
                          type="button"
                          onClick={handleOpenFirstStaleConflict}
                          disabled={Boolean(pendingStaleRecovery) || isRefreshing}
                          className="rounded border border-blue-400/30 bg-blue-500/10 px-2.5 py-1 text-[11px] text-blue-50 hover:bg-blue-500/20 transition-colors disabled:opacity-50"
                        >
                          Review First Conflict
                        </button>
                      )}
                      {saveAttemptReport.failures.some((failure) => failure.kind === 'stale_row') && (
                        <button
                          type="button"
                          onClick={handleRecoverAllStaleFailures}
                          disabled={Boolean(pendingStaleRecovery) || isRefreshing}
                          className="rounded border border-blue-400/30 bg-blue-500/10 px-2.5 py-1 text-[11px] text-blue-50 hover:bg-blue-500/20 transition-colors disabled:opacity-50"
                        >
                          {pendingStaleRecovery ? 'Refreshing Stale...' : 'Refresh & Recover All Stale'}
                        </button>
                      )}
                      {saveAttemptReport.failures.some((failure) => failure.kind === 'stale_row') && (
                        <button
                          type="button"
                          onClick={handleDiscardAllStaleFailures}
                          disabled={Boolean(pendingStaleRecovery)}
                          className="rounded border border-amber-400/30 bg-amber-500/10 px-2.5 py-1 text-[11px] text-amber-50 hover:bg-amber-500/20 transition-colors disabled:opacity-50"
                        >
                          Drop All Stale
                        </button>
                      )}
                      <button
                        type="button"
                        onClick={handleDiscardAllFailedChanges}
                        disabled={Boolean(pendingStaleRecovery)}
                        className="rounded border border-amber-400/30 bg-[#161b22] px-2.5 py-1 text-[11px] text-amber-50 hover:bg-[#21262d] transition-colors disabled:opacity-50"
                      >
                        Drop All Failed Changes
                      </button>
                      <button
                        type="button"
                        onClick={handleReloadServerCopy}
                        disabled={Boolean(pendingStaleRecovery)}
                        className="rounded border border-[#30363d] bg-[#161b22] px-2.5 py-1 text-[11px] text-gray-200 hover:bg-[#21262d] transition-colors disabled:opacity-50"
                      >
                        Reload Server Copy
                      </button>
                    </div>
                    {saveAttemptReport.failures.map((failure, index) => (
                      <div
                        key={`save-failure-${failure.action}-${failure.isNew ? 'new' : 'existing'}-${failure.rowIdx}-${index}`}
                        className="rounded border border-[#30363d] bg-[#0d1117] px-3 py-2"
                      >
                        <button
                          type="button"
                          onClick={() => focusSaveFailure(failure)}
                          className="flex w-full items-start justify-between gap-3 text-left hover:text-white transition-colors"
                        >
                          <div>
                            <div className="flex flex-wrap items-center gap-2">
                              <span className="rounded border border-amber-400/30 bg-amber-500/10 px-1.5 py-0.5 text-[10px] font-medium uppercase tracking-wide text-amber-100">
                                {getSaveFailureKindLabel(failure.kind)}
                              </span>
                              <div className="text-xs font-medium text-amber-50">{failure.summary}</div>
                            </div>
                            <div className="mt-1 text-[11px] text-gray-300">{failure.message}</div>
                            {failure.rawMessage && failure.rawMessage !== failure.message && (
                              <div className="mt-1 text-[10px] text-gray-500 break-all">
                                Raw: {failure.rawMessage}
                              </div>
                            )}
                            {failure.recoveryNote && (
                              <div className="mt-1 text-[10px] text-blue-300/80">
                                {failure.recoveryNote}
                              </div>
                            )}
                          </div>
                          <span className="text-[11px] text-amber-200">Jump</span>
                        </button>
                        <div className="mt-2 flex flex-wrap items-center gap-2">
                          {failure.kind === 'stale_row' && (
                            <button
                              type="button"
                              onClick={() => handleOpenStaleConflict(failure, {
                                scope: buildStaleConflictReviewScope(staleFailures, 'All Stale Conflicts'),
                              })}
                              disabled={Boolean(pendingStaleRecovery)}
                              className="rounded border border-blue-400/30 bg-[#161b22] px-2.5 py-1 text-[11px] text-blue-50 hover:bg-[#21262d] transition-colors disabled:opacity-50"
                            >
                              Review Conflict
                            </button>
                          )}
                          {failure.kind === 'stale_row' && (failure.action === 'update' || failure.action === 'delete') && (
                            <button
                              type="button"
                              onClick={() => handleRecoverStaleFailure(failure)}
                              disabled={Boolean(pendingStaleRecovery) || isRefreshing}
                              className="rounded border border-blue-400/30 bg-blue-500/10 px-2.5 py-1 text-[11px] text-blue-50 hover:bg-blue-500/20 transition-colors disabled:opacity-50"
                            >
                              {failure.action === 'delete' ? 'Refresh & Re-mark Delete' : 'Refresh & Reapply Edit'}
                            </button>
                          )}
                          <button
                            type="button"
                            onClick={() => handleDiscardFailure(failure)}
                            disabled={Boolean(pendingStaleRecovery)}
                            className="rounded border border-amber-400/30 bg-[#161b22] px-2.5 py-1 text-[11px] text-amber-50 hover:bg-[#21262d] transition-colors disabled:opacity-50"
                          >
                            {failure.kind === 'stale_row' ? 'Drop Stale Change' : 'Drop This Change'}
                          </button>
                        </div>
                      </div>
                    ))}
                  </div>
                </section>
              )}

              {validationIssueCount > 0 && (
                <section className="rounded border border-red-500/30 bg-red-500/5">
                  <div className="flex items-center justify-between border-b border-red-500/20 px-4 py-2">
                    <div className="text-sm font-medium text-red-200">Validation errors</div>
                    <div className="text-xs text-red-300/80">{validationIssueCount} blocking</div>
                  </div>
                  <div className="space-y-2 p-4">
                    {validationIssues.slice(0, SAVE_REVIEW_PREVIEW_LIMIT * 2).map((issue, index) => (
                      <button
                        key={`validation-issue-${issue.isNew ? 'new' : 'existing'}-${issue.rowIdx}-${issue.col}-${index}`}
                        type="button"
                        onClick={() => focusValidationIssue(issue)}
                        className="flex w-full items-start justify-between gap-3 rounded border border-[#30363d] bg-[#0d1117] px-3 py-2 text-left hover:border-red-400/40 hover:bg-[#161b22] transition-colors"
                      >
                        <div>
                          <div className="text-xs font-medium text-red-100">
                            {`${issue.isNew ? 'Draft row' : 'Row'} ${issue.rowIdx + 1} · ${issue.col}`}
                          </div>
                          <div className="mt-1 text-[11px] text-gray-300">{issue.message}</div>
                        </div>
                        <span className="text-[11px] text-red-300">Jump</span>
                      </button>
                    ))}
                    {validationIssueCount > SAVE_REVIEW_PREVIEW_LIMIT * 2 && (
                      <div className="text-xs text-gray-500">
                        {`+ ${validationIssueCount - SAVE_REVIEW_PREVIEW_LIMIT * 2} more validation issue${validationIssueCount - SAVE_REVIEW_PREVIEW_LIMIT * 2 === 1 ? '' : 's'}`}
                      </div>
                    )}
                  </div>
                </section>
              )}

              {insertedRowPreviews.length > 0 && (
                <section className="rounded border border-green-500/30 bg-green-500/5">
                  <div className="flex items-center justify-between border-b border-green-500/20 px-4 py-2">
                    <div className="text-sm font-medium text-green-200">Inserted draft rows</div>
                    <div className="text-xs text-green-300/80">{insertedRowPreviews.length} pending</div>
                  </div>
                  <div className="space-y-3 p-4">
                    {insertedRowPreviews.slice(0, SAVE_REVIEW_PREVIEW_LIMIT).map((row) => (
                      <div key={`insert-preview-${row.rowIdx}`} className="rounded border border-[#30363d] bg-[#0d1117] p-3">
                        <div className="mb-2 text-xs font-medium text-gray-300">Draft Row #{row.rowIdx + 1}</div>
                        <pre className="max-h-40 overflow-auto whitespace-pre-wrap break-all text-[11px] leading-5 text-gray-200">{stringifyJson(row.rowData)}</pre>
                      </div>
                    ))}
                    {insertedRowPreviews.length > SAVE_REVIEW_PREVIEW_LIMIT && (
                      <div className="text-xs text-gray-500">
                        {`+ ${insertedRowPreviews.length - SAVE_REVIEW_PREVIEW_LIMIT} more inserted row preview${insertedRowPreviews.length - SAVE_REVIEW_PREVIEW_LIMIT === 1 ? '' : 's'}`}
                      </div>
                    )}
                  </div>
                </section>
              )}

              {updatedRowPreviews.length > 0 && (
                <section className="rounded border border-blue-500/30 bg-blue-500/5">
                  <div className="flex items-center justify-between border-b border-blue-500/20 px-4 py-2">
                    <div className="text-sm font-medium text-blue-200">Updated rows</div>
                    <div className="text-xs text-blue-300/80">{updatedRowPreviews.length} pending</div>
                  </div>
                  <div className="space-y-3 p-4">
                    {updatedRowPreviews.slice(0, SAVE_REVIEW_PREVIEW_LIMIT).map((row) => (
                      <div key={`update-preview-${row.rowIdx}`} className="rounded border border-[#30363d] bg-[#0d1117] p-3">
                        <div className="mb-3 text-xs text-gray-400">
                          <span className="font-medium text-gray-200">Row #{row.rowIdx + 1}</span>
                          <span>{` · ${formatConditionLabel(row.condition)}`}</span>
                        </div>
                        <div className="space-y-2">
                          {row.changes.map((change) => (
                            <div key={`${row.rowIdx}-${change.column}`} className="grid gap-2 rounded border border-[#30363d] bg-[#161b22] p-2 lg:grid-cols-[160px_minmax(0,1fr)_minmax(0,1fr)]">
                              <div className="text-xs font-medium text-gray-300">{change.column}</div>
                              <div>
                                <div className="mb-1 text-[10px] uppercase tracking-wide text-gray-500">Before</div>
                                <div className="max-h-24 overflow-auto whitespace-pre-wrap break-all rounded border border-[#30363d] bg-[#0d1117] px-2 py-1 text-[11px] text-gray-300">{formatReviewValue(change.before)}</div>
                              </div>
                              <div>
                                <div className="mb-1 text-[10px] uppercase tracking-wide text-gray-500">After</div>
                                <div className="max-h-24 overflow-auto whitespace-pre-wrap break-all rounded border border-blue-500/20 bg-blue-500/10 px-2 py-1 text-[11px] text-blue-100">{formatReviewValue(change.after)}</div>
                              </div>
                            </div>
                          ))}
                        </div>
                      </div>
                    ))}
                    {updatedRowPreviews.length > SAVE_REVIEW_PREVIEW_LIMIT && (
                      <div className="text-xs text-gray-500">
                        {`+ ${updatedRowPreviews.length - SAVE_REVIEW_PREVIEW_LIMIT} more updated row preview${updatedRowPreviews.length - SAVE_REVIEW_PREVIEW_LIMIT === 1 ? '' : 's'}`}
                      </div>
                    )}
                  </div>
                </section>
              )}

              {deletedRowPreviews.length > 0 && (
                <section className="rounded border border-red-500/30 bg-red-500/5">
                  <div className="flex items-center justify-between border-b border-red-500/20 px-4 py-2">
                    <div className="text-sm font-medium text-red-200">Deleted rows</div>
                    <div className="text-xs text-red-300/80">{deletedRowPreviews.length} pending</div>
                  </div>
                  <div className="space-y-3 p-4">
                    {deletedRowPreviews.slice(0, SAVE_REVIEW_PREVIEW_LIMIT).map((row) => (
                      <div key={`delete-preview-${row.rowIdx}`} className="rounded border border-[#30363d] bg-[#0d1117] p-3">
                        <div className="mb-2 text-xs text-gray-400">
                          <span className="font-medium text-gray-200">Row #{row.rowIdx + 1}</span>
                          <span>{` · ${formatConditionLabel(row.condition)}`}</span>
                        </div>
                        <pre className="max-h-40 overflow-auto whitespace-pre-wrap break-all text-[11px] leading-5 text-gray-200">{stringifyJson(row.rowData)}</pre>
                      </div>
                    ))}
                    {deletedRowPreviews.length > SAVE_REVIEW_PREVIEW_LIMIT && (
                      <div className="text-xs text-gray-500">
                        {`+ ${deletedRowPreviews.length - SAVE_REVIEW_PREVIEW_LIMIT} more deleted row preview${deletedRowPreviews.length - SAVE_REVIEW_PREVIEW_LIMIT === 1 ? '' : 's'}`}
                      </div>
                    )}
                  </div>
                </section>
              )}
            </div>

            <div className="flex items-center justify-between gap-3 border-t border-[#30363d] bg-[#0d1117] px-4 py-3">
              <div className="text-xs text-gray-500">
                {validationIssueCount > 0
                  ? 'Fix the blocking validation errors above before confirming save.'
                  : 'Deletes run first, then updates, then inserts. Nothing is written until you confirm.'}
              </div>
              <div className="flex items-center gap-2">
                <button
                  onClick={() => setShowSaveReviewModal(false)}
                  disabled={isSaving || Boolean(pendingStaleRecovery)}
                  className="rounded border border-[#30363d] bg-[#161b22] px-3 py-1.5 text-xs text-gray-300 hover:text-white transition-colors disabled:opacity-50"
                >
                  Cancel
                </button>
                <button
                  onClick={() => void confirmSave()}
                  disabled={isSaving || isRefreshing || !saveReviewCounts.total || validationIssueCount > 0 || Boolean(pendingStaleRecovery)}
                  className="rounded border border-blue-500/30 bg-blue-600 px-3 py-1.5 text-xs text-white hover:bg-blue-500 transition-colors disabled:opacity-50"
                >
                  {isSaving
                    ? 'Saving...'
                    : pendingStaleRecovery
                      ? 'Refreshing Stale Rows...'
                      : validationIssueCount > 0
                        ? `Blocked by ${validationIssueCount} Error${validationIssueCount === 1 ? '' : 's'}`
                        : `Confirm Save (${saveReviewCounts.total})`}
                </button>
              </div>
            </div>
          </div>
        </div>
      )}

      {showStaleConflictOverview && (
        <div className="fixed inset-0 z-[70] bg-black/60 backdrop-blur-sm flex items-center justify-center p-4">
          <div className="w-full max-w-5xl max-h-[85vh] bg-[#161b22] border border-[#30363d] rounded-lg shadow-2xl overflow-hidden flex flex-col">
            <div className="flex items-center justify-between px-4 py-3 border-b border-[#30363d]">
              <div>
                <div className="text-sm font-medium text-white">Stale Conflict Queue</div>
                <div className="text-xs text-gray-400">
                  {`${staleFailures.length} stale conflict${staleFailures.length === 1 ? '' : 's'} pending review on the current table page.`}
                </div>
              </div>
              <button
                type="button"
                onClick={handleCloseStaleConflictOverview}
                className="text-gray-400 hover:text-white transition-colors"
              >
                <X className="w-4 h-4" />
              </button>
            </div>

            <div className="grid gap-3 border-b border-[#30363d] bg-[#0d1117] px-4 py-3 md:grid-cols-5">
              {([
                ['all', 'All', staleConflictOverviewCounts.all],
                ['high_risk', 'High Risk', staleConflictOverviewCounts.highRisk],
                ['needs_refresh', 'Needs Refresh', staleConflictOverviewCounts.needsRefresh],
                ['safe_edits', 'Safe Edits', staleConflictOverviewCounts.safeEdits],
                ['delete', 'Deletes', staleConflictOverviewCounts.delete],
              ] as const).map(([filterKey, label, count]) => (
                <button
                  key={filterKey}
                  type="button"
                  onClick={() => handleSetStaleConflictOverviewFilter(filterKey)}
                  className={`rounded border px-3 py-2 text-left transition-colors ${staleConflictQueueFilter === filterKey ? 'border-blue-500/40 bg-blue-500/10 text-blue-100' : 'border-[#30363d] bg-[#161b22] text-gray-300 hover:text-white'}`}
                >
                  <div className="text-[11px] uppercase tracking-wide text-gray-500">{label}</div>
                  <div className="mt-1 text-lg font-semibold">{count}</div>
                </button>
              ))}
            </div>

            <div className="flex flex-wrap items-center gap-2 border-b border-[#30363d] px-4 py-3">
              <input
                value={staleConflictOverviewQuery}
                onChange={(event) => setStaleConflictOverviewQuery(event.target.value)}
                placeholder="Search row, condition, summary..."
                className="min-w-[240px] flex-1 rounded border border-[#30363d] bg-[#0d1117] px-3 py-1.5 text-xs text-gray-200 outline-none focus:border-blue-500"
              />
              <select
                value={staleConflictOverviewSort}
                onChange={(event) => setStaleConflictOverviewSort(event.target.value as StaleConflictQueueSort)}
                className="rounded border border-[#30363d] bg-[#0d1117] px-3 py-1.5 text-xs text-gray-200 outline-none focus:border-blue-500"
              >
                <option value="risk_desc">Sort: Risk</option>
                <option value="conflicts_desc">Sort: Conflict Count</option>
                <option value="row_asc">Sort: Row Asc</option>
                <option value="row_desc">Sort: Row Desc</option>
                <option value="action">Sort: Action</option>
              </select>
              <button
                type="button"
                onClick={handleExpandAllVisibleStaleConflictGroups}
                disabled={visibleStaleFailureSummaryGroups.length === 0 || allVisibleStaleConflictGroupsExpanded}
                className="rounded border border-[#30363d] bg-[#161b22] px-3 py-1.5 text-xs text-gray-200 hover:bg-[#21262d] transition-colors disabled:opacity-40"
              >
                Expand All Groups
              </button>
              <button
                type="button"
                onClick={handleCollapseAllVisibleStaleConflictGroups}
                disabled={visibleStaleFailureSummaryGroups.length === 0 || allVisibleStaleConflictGroupsCollapsed}
                className="rounded border border-[#30363d] bg-[#161b22] px-3 py-1.5 text-xs text-gray-200 hover:bg-[#21262d] transition-colors disabled:opacity-40"
              >
                Collapse All Groups
              </button>
              <button
                type="button"
                onClick={handleOpenFirstFilteredStaleConflict}
                disabled={visibleStaleFailureSummaries.length === 0}
                className="rounded border border-blue-400/30 bg-blue-500/10 px-3 py-1.5 text-xs text-blue-50 hover:bg-blue-500/20 transition-colors disabled:opacity-40"
              >
                Review First Visible
              </button>
              <button
                type="button"
                onClick={handleRefreshVisibleNeedsRefreshStaleConflicts}
                disabled={visibleStaleConflictOverviewCounts.needsRefresh === 0 || Boolean(pendingStaleRecovery) || isRefreshing}
                className="rounded border border-amber-400/30 bg-amber-500/10 px-3 py-1.5 text-xs text-amber-50 hover:bg-amber-500/20 transition-colors disabled:opacity-40"
              >
                {pendingStaleRecovery ? 'Refreshing Visible...' : 'Refresh Visible Needs Refresh'}
              </button>
              <button
                type="button"
                onClick={handleKeepMineForVisibleSafeStaleConflicts}
                disabled={visibleStaleConflictOverviewCounts.safeEdits === 0}
                className="rounded border border-green-400/30 bg-green-500/10 px-3 py-1.5 text-xs text-green-100 hover:bg-green-500/20 transition-colors disabled:opacity-40"
              >
                Keep Mine for Safe Edits
              </button>
              <button
                type="button"
                onClick={handleApplyRecommendedToFilteredStaleConflicts}
                disabled={visibleStaleConflictOverviewCounts.safeEdits === 0}
                className="rounded border border-blue-400/30 bg-[#161b22] px-3 py-1.5 text-xs text-blue-100 hover:bg-[#21262d] transition-colors disabled:opacity-40"
              >
                Apply Recommended to Safe Edits
              </button>
              <button
                type="button"
                onClick={handleUseLatestForVisibleStaleConflicts}
                disabled={visibleStaleFailureSummaries.length === 0 || Boolean(pendingStaleRecovery)}
                className="rounded border border-[#30363d] bg-[#161b22] px-3 py-1.5 text-xs text-gray-200 hover:bg-[#21262d] transition-colors disabled:opacity-40"
              >
                Use Latest for Visible
              </button>
              <button
                type="button"
                onClick={() => handleSetStaleConflictOverviewFilter('high_risk')}
                className="rounded border border-red-400/30 bg-red-500/10 px-3 py-1.5 text-xs text-red-100 hover:bg-red-500/20 transition-colors"
              >
                Show High Risk Only
              </button>
              <div className="text-xs text-gray-500">
                {`${visibleStaleFailureSummaries.length} visible / ${filteredStaleFailureSummaries.length} filtered | ${visibleStaleFailureSummaryGroups.length} group${visibleStaleFailureSummaryGroups.length === 1 ? '' : 's'}`}
              </div>
            </div>

            <div className="flex-1 overflow-auto p-4 space-y-3">
              {visibleStaleFailureSummaryGroups.length > 0 ? visibleStaleFailureSummaryGroups.map(({ groupKey, label, hint, items }) => (
                <div
                  key={`stale-group-${groupKey}`}
                  className={`overflow-hidden rounded border ${groupKey === 'high_risk' ? 'border-red-500/20' : groupKey === 'needs_refresh' ? 'border-amber-500/20' : groupKey === 'safe_edits' ? 'border-green-500/20' : 'border-[#30363d]'} bg-[#0d1117]`}
                >
                  <button
                    type="button"
                    onClick={() => handleToggleStaleConflictOverviewGroup(groupKey)}
                    className={`flex w-full items-start justify-between gap-3 px-3 py-2 text-left transition-colors ${groupKey === 'high_risk' ? 'bg-red-500/10 hover:bg-red-500/15' : groupKey === 'needs_refresh' ? 'bg-amber-500/10 hover:bg-amber-500/15' : groupKey === 'safe_edits' ? 'bg-green-500/10 hover:bg-green-500/15' : groupKey === 'delete' ? 'bg-red-500/5 hover:bg-red-500/10' : 'bg-[#11161d] hover:bg-[#161b22]'}`}
                  >
                    <div className="min-w-0 flex items-start gap-2">
                      {staleConflictOverviewCollapsedGroups[groupKey] ? (
                        <ChevronRight className="mt-0.5 h-4 w-4 shrink-0 text-gray-400" />
                      ) : (
                        <ChevronDown className="mt-0.5 h-4 w-4 shrink-0 text-gray-400" />
                      )}
                      <div className="min-w-0">
                        <div className="flex flex-wrap items-center gap-2">
                          <div className="text-xs font-semibold text-white">{label}</div>
                          {groupKey === 'high_risk' && (
                            <span className="rounded border border-red-400/30 bg-red-500/10 px-1.5 py-0.5 text-[10px] text-red-100">
                              Priority
                            </span>
                          )}
                        </div>
                        <div className="mt-1 text-[10px] text-gray-400">{hint}</div>
                      </div>
                    </div>
                    <div className="rounded border border-[#30363d] bg-[#161b22] px-2 py-0.5 text-[10px] text-gray-300">
                      {items.length}
                    </div>
                  </button>

                  {!staleConflictOverviewCollapsedGroups[groupKey] && (
                    <div className="space-y-3 border-t border-[#30363d] p-3">
                      <div className="flex flex-wrap items-center justify-between gap-2 rounded border border-[#30363d] bg-[#11161d] px-3 py-2">
                        <div className="text-[10px] text-gray-500">
                          {`${items.length} visible conflict${items.length === 1 ? '' : 's'} in this group.`}
                        </div>
                        <div className="flex flex-wrap items-center justify-end gap-2">
                          <button
                            type="button"
                            onClick={() => handleOpenFirstStaleConflictGroup(items, label)}
                            className="rounded border border-blue-400/30 bg-blue-500/10 px-2.5 py-1 text-[11px] text-blue-50 hover:bg-blue-500/20 transition-colors"
                          >
                            Review First
                          </button>
                          {groupKey === 'needs_refresh' && (
                            <button
                              type="button"
                              onClick={() => handleRefreshStaleConflictGroup(items, label)}
                              disabled={Boolean(pendingStaleRecovery) || isRefreshing}
                              className="rounded border border-amber-400/30 bg-amber-500/10 px-2.5 py-1 text-[11px] text-amber-50 hover:bg-amber-500/20 transition-colors disabled:opacity-40"
                            >
                              {pendingStaleRecovery ? 'Refreshing...' : 'Refresh Group'}
                            </button>
                          )}
                          {groupKey === 'safe_edits' && (
                            <button
                              type="button"
                              onClick={() => applySafeUpdateModeToStaleSummaries(items, 'mine', {
                                successPrefix: 'Kept your edits for',
                              })}
                              className="rounded border border-green-400/30 bg-green-500/10 px-2.5 py-1 text-[11px] text-green-100 hover:bg-green-500/20 transition-colors"
                            >
                              Keep Mine in Group
                            </button>
                          )}
                          {groupKey === 'safe_edits' && (
                            <button
                              type="button"
                              onClick={() => applySafeUpdateModeToStaleSummaries(items, 'recommended', {
                                successPrefix: 'Applied recommended resolution to',
                              })}
                              className="rounded border border-blue-400/30 bg-[#161b22] px-2.5 py-1 text-[11px] text-blue-100 hover:bg-[#21262d] transition-colors"
                            >
                              Apply Recommended
                            </button>
                          )}
                        </div>
                      </div>
                      {items.map(({ failure, details, isHighRisk, needsRefresh, isSafeUpdate }) => (
                        <div
                          key={`stale-queue-${getSaveFailureKey(failure)}`}
                          className="rounded border border-[#30363d] bg-[#11161d] p-3"
                        >
                          <div className="flex flex-wrap items-start justify-between gap-3">
                            <div className="min-w-0 flex-1">
                              <div className="flex flex-wrap items-center gap-2">
                                <span className={`rounded border px-1.5 py-0.5 text-[10px] font-medium uppercase tracking-wide ${failure.action === 'delete' ? 'border-red-400/30 bg-red-500/10 text-red-100' : 'border-blue-400/30 bg-blue-500/10 text-blue-100'}`}>
                                  {failure.action}
                                </span>
                                {isHighRisk && (
                                  <span className="rounded border border-red-400/30 bg-red-500/10 px-1.5 py-0.5 text-[10px] text-red-100">
                                    High Risk
                                  </span>
                                )}
                                {needsRefresh && (
                                  <span className="rounded border border-amber-400/30 bg-amber-500/10 px-1.5 py-0.5 text-[10px] text-amber-100">
                                    Needs Refresh
                                  </span>
                                )}
                                {isSafeUpdate && (
                                  <span className="rounded border border-green-400/30 bg-green-500/10 px-1.5 py-0.5 text-[10px] text-green-100">
                                    Safe Recommended Merge
                                  </span>
                                )}
                                <div className="text-xs font-medium text-white">{failure.summary}</div>
                              </div>
                              <div className="mt-1 text-[11px] text-gray-400">
                                {`${getRowLabel(failure.rowIdx, failure.isNew)} | ${failure.staleRecovery?.condition ? formatConditionLabel(failure.staleRecovery.condition) : 'No row condition available'}`}
                              </div>
                              <div className="mt-2 flex flex-wrap items-center gap-2 text-[10px] text-gray-500">
                                <span>{`Conflicts ${details?.conflictCount || 0}`}</span>
                                <span>{`Local-only ${details?.localPendingCount || 0}`}</span>
                                <span>{`Server-only ${details?.serverOnlyCount || 0}`}</span>
                                <span>{`Already applied ${details?.alreadyAppliedCount || 0}`}</span>
                              </div>
                              {failure.recoveryNote && (
                                <div className="mt-2 text-[10px] text-blue-300/80">
                                  {failure.recoveryNote}
                                </div>
                              )}
                            </div>
                            <div className="flex flex-wrap items-center justify-end gap-2">
                              <button
                                type="button"
                                onClick={() => handleOpenStaleConflict(failure, {
                                  scope: buildStaleConflictReviewScope(items.map((summary) => summary.failure), `${label} Group`),
                                })}
                                className="rounded border border-blue-400/30 bg-blue-500/10 px-2.5 py-1 text-[11px] text-blue-50 hover:bg-blue-500/20 transition-colors"
                              >
                                Review
                              </button>
                              {isSafeUpdate && (
                                <button
                                  type="button"
                                  onClick={() => handleKeepMineForSingleStaleSummary(getSaveFailureKey(failure))}
                                  className="rounded border border-green-400/30 bg-green-500/10 px-2.5 py-1 text-[11px] text-green-100 hover:bg-green-500/20 transition-colors"
                                >
                                  Keep Mine
                                </button>
                              )}
                              {isSafeUpdate && (
                                <button
                                  type="button"
                                  onClick={() => handleApplyRecommendedToSingleStaleSummary(getSaveFailureKey(failure))}
                                  className="rounded border border-blue-400/30 bg-[#161b22] px-2.5 py-1 text-[11px] text-blue-100 hover:bg-[#21262d] transition-colors"
                                >
                                  Apply Recommended
                                </button>
                              )}
                              <button
                                type="button"
                                onClick={() => handleUseLatestForSingleStaleSummary(getSaveFailureKey(failure))}
                                className="rounded border border-[#30363d] bg-[#161b22] px-2.5 py-1 text-[11px] text-gray-200 hover:bg-[#21262d] transition-colors"
                              >
                                Use Latest
                              </button>
                              <button
                                type="button"
                                onClick={() => focusSaveFailure(failure)}
                                className="rounded border border-[#30363d] bg-[#161b22] px-2.5 py-1 text-[11px] text-gray-300 hover:text-white transition-colors"
                              >
                                Jump to Row
                              </button>
                            </div>
                          </div>
                        </div>
                      ))}
                    </div>
                  )}
                </div>
              )) : (
                <div className="rounded border border-[#30363d] bg-[#0d1117] px-4 py-3 text-sm text-gray-400">
                  No stale conflicts match the current filter/search.
                </div>
              )}
            </div>
          </div>
        </div>
      )}

      {activeStaleConflict && activeStaleConflictDetails && (
        <div className="fixed inset-0 z-[69] bg-black/60 backdrop-blur-sm flex items-center justify-center p-4">
          <div className="w-full max-w-5xl max-h-[85vh] bg-[#161b22] border border-[#30363d] rounded-lg shadow-2xl overflow-hidden flex flex-col">
            <div className="flex items-center justify-between px-4 py-3 border-b border-[#30363d]">
              <div>
                <div className="text-sm font-medium text-white">
                  {activeStaleConflict.action === 'delete' ? 'Stale Delete Conflict Review' : 'Stale Edit Conflict Review'}
                </div>
                <div className="mt-1 text-xs text-gray-400">
                  {`${getRowLabel(activeStaleConflict.rowIdx, activeStaleConflict.isNew)} | ${activeStaleConflict.summary}`}
                  {activeStaleConflict.staleRecovery?.condition
                    ? ` | ${formatConditionLabel(activeStaleConflict.staleRecovery.condition)}`
                    : ''}
                </div>
                <div className="mt-1 text-[11px] text-gray-500">
                  {`Review scope: ${staleConflictReviewScope?.label || 'All Stale Conflicts'}`}
                </div>
              </div>
              <div className="flex items-center gap-2">
                {activeStaleConflictReviewFailures.length > 1 && activeStaleConflictScopedIndex >= 0 && (
                  <div className="text-[11px] text-gray-500">
                    {`${activeStaleConflictScopedIndex + 1} / ${activeStaleConflictReviewFailures.length}`}
                  </div>
                )}
                <button
                  type="button"
                  onClick={() => handleOpenAdjacentStaleConflict('prev')}
                  disabled={activeStaleConflictScopedIndex <= 0}
                  className="rounded border border-[#30363d] bg-[#161b22] px-2 py-1 text-[11px] text-gray-300 hover:text-white transition-colors disabled:opacity-40"
                >
                  Prev
                </button>
                <button
                  type="button"
                  onClick={() => handleOpenAdjacentStaleConflict('next')}
                  disabled={activeStaleConflictScopedIndex < 0 || activeStaleConflictScopedIndex >= activeStaleConflictReviewFailures.length - 1}
                  className="rounded border border-[#30363d] bg-[#161b22] px-2 py-1 text-[11px] text-gray-300 hover:text-white transition-colors disabled:opacity-40"
                >
                  Next
                </button>
                <button
                  type="button"
                  onClick={handleCloseStaleConflict}
                  className="text-gray-400 hover:text-white transition-colors"
                >
                  <X className="w-4 h-4" />
                </button>
              </div>
            </div>

            <div className="flex-1 overflow-auto p-4 space-y-4">
              <div className="grid gap-3 md:grid-cols-4">
                <div className="rounded border border-[#30363d] bg-[#0d1117] px-3 py-2">
                  <div className="text-[11px] uppercase tracking-wide text-gray-500">User changed</div>
                  <div className="mt-1 text-lg font-semibold text-blue-300">{activeStaleConflictDetails.changedColumns.length}</div>
                </div>
                <div className="rounded border border-[#30363d] bg-[#0d1117] px-3 py-2">
                  <div className="text-[11px] uppercase tracking-wide text-gray-500">Direct conflicts</div>
                  <div className="mt-1 text-lg font-semibold text-red-300">{activeStaleConflictDetails.conflictCount}</div>
                </div>
                <div className="rounded border border-[#30363d] bg-[#0d1117] px-3 py-2">
                  <div className="text-[11px] uppercase tracking-wide text-gray-500">Server-only changes</div>
                  <div className="mt-1 text-lg font-semibold text-amber-300">{activeStaleConflictDetails.serverOnlyCount}</div>
                </div>
                <div className="rounded border border-[#30363d] bg-[#0d1117] px-3 py-2">
                  <div className="text-[11px] uppercase tracking-wide text-gray-500">Already applied</div>
                  <div className="mt-1 text-lg font-semibold text-green-300">{activeStaleConflictDetails.alreadyAppliedCount}</div>
                </div>
              </div>

              {!activeStaleConflictDetails.hasFreshLatestRow && (
                <div className="rounded border border-amber-500/30 bg-amber-500/10 px-4 py-3 text-sm text-amber-100">
                  Refresh the current page before trusting the server-side diff below. The grid has not loaded a newer server snapshot since this stale conflict happened.
                </div>
              )}

              {activeStaleConflictDetails.locatedRowIdx < 0 && (
                <div className="rounded border border-red-500/30 bg-red-500/10 px-4 py-3 text-sm text-red-100">
                  The matching row is not visible on the current page. Clear filters or navigate until the row is visible, then refresh again to review/resolve this conflict in place.
                </div>
              )}

              {activeStaleConflict.action === 'update' && (
                <div className="rounded border border-blue-500/20 bg-blue-500/5 px-4 py-3 text-xs text-blue-100">
                  {`Recommended merge currently keeps ${recommendedPendingCount} edited column${recommendedPendingCount === 1 ? '' : 's'} from your pending draft and accepts the latest server copy for the remaining ${Math.max(activeStaleConflictDetails.changedColumns.length - recommendedPendingCount, 0)}.`}
                  {staleConflictHasSelectionOverrides
                    ? ' Your current selection differs from that recommended preset.'
                    : ' Your current selection matches that recommended preset.'}
                </div>
              )}

              <div className="space-y-3">
                {activeStaleConflictDetails.diffItems.length > 0 ? activeStaleConflictDetails.diffItems.map((item) => (
                  <div
                    key={`stale-conflict-${item.column}`}
                    className="rounded border border-[#30363d] bg-[#0d1117] p-3"
                  >
                    <div className="flex flex-wrap items-start justify-between gap-3">
                      <div>
                        <div className="flex flex-wrap items-center gap-2">
                          <div className="text-sm font-medium text-white">{item.column}</div>
                          <span className={`rounded border px-1.5 py-0.5 text-[10px] font-medium uppercase tracking-wide ${getStaleConflictStateClasses(item.state)}`}>
                            {getStaleConflictStateLabel(item.state)}
                          </span>
                          {item.userChanged && (
                            <span className="rounded border border-blue-400/20 bg-blue-500/10 px-1.5 py-0.5 text-[10px] text-blue-100">
                              Your edit
                            </span>
                          )}
                          {item.serverChanged && (
                            <span className="rounded border border-amber-400/20 bg-amber-500/10 px-1.5 py-0.5 text-[10px] text-amber-100">
                              Server changed
                            </span>
                          )}
                        </div>
                        <div className="mt-1 text-[11px] text-gray-500">
                          {item.state === 'conflict'
                            ? 'Both your pending edit and the server row changed this column.'
                            : item.state === 'local_pending'
                              ? 'Only your pending draft changed this column.'
                              : item.state === 'server_only'
                                ? 'Only the server row changed this column.'
                                : item.state === 'already_applied'
                                  ? 'Your pending value already matches the current server row for this column.'
                                  : 'Refresh current page to load the latest server value for this column.'}
                        </div>
                      </div>
                      {activeStaleConflict.action === 'update' && item.userChanged && (
                        <div className="flex items-center gap-1 rounded border border-[#30363d] bg-[#161b22] p-1">
                          <button
                            type="button"
                            onClick={() => handleSetStaleConflictSelection(item.column, 'pending')}
                            className={`rounded px-2 py-1 text-[11px] transition-colors ${staleConflictSelections[item.column] !== 'latest' ? 'bg-blue-600 text-white' : 'text-gray-300 hover:text-white'}`}
                          >
                            Use Mine
                          </button>
                          <button
                            type="button"
                            onClick={() => handleSetStaleConflictSelection(item.column, 'latest')}
                            className={`rounded px-2 py-1 text-[11px] transition-colors ${staleConflictSelections[item.column] === 'latest' ? 'bg-[#30363d] text-white' : 'text-gray-300 hover:text-white'}`}
                          >
                            Use Server
                          </button>
                        </div>
                      )}
                    </div>

                    <div className={`mt-3 grid gap-3 ${activeStaleConflict.action === 'delete' ? 'md:grid-cols-2' : 'md:grid-cols-3'}`}>
                      <div>
                        <div className="mb-1 text-[11px] uppercase tracking-wide text-gray-500">Original matched row</div>
                        <div className="rounded border border-[#30363d] bg-[#161b22] px-2 py-1 text-[11px] text-gray-200 whitespace-pre-wrap break-all">
                          {formatReviewValue(item.originalValue)}
                        </div>
                      </div>
                      {activeStaleConflict.action === 'update' && (
                        <div>
                          <div className="mb-1 text-[11px] uppercase tracking-wide text-blue-300">Your pending value</div>
                          <div className="rounded border border-blue-500/20 bg-blue-500/10 px-2 py-1 text-[11px] text-blue-100 whitespace-pre-wrap break-all">
                            {formatReviewValue(item.pendingValue)}
                          </div>
                        </div>
                      )}
                      <div>
                        <div className="mb-1 text-[11px] uppercase tracking-wide text-amber-300">
                          {activeStaleConflictDetails.hasFreshLatestRow ? 'Latest server row' : 'Current page snapshot'}
                        </div>
                        <div className="rounded border border-amber-500/20 bg-amber-500/10 px-2 py-1 text-[11px] text-amber-50 whitespace-pre-wrap break-all">
                          {activeStaleConflictDetails.latestRowData
                            ? formatReviewValue(item.latestValue)
                            : '(row not visible on current page)'}
                        </div>
                      </div>
                    </div>
                  </div>
                )) : (
                  <div className="rounded border border-[#30363d] bg-[#0d1117] px-4 py-3 text-sm text-gray-400">
                    No column-level diff is available for this conflict yet.
                  </div>
                )}
              </div>
            </div>

            <div className="flex items-center justify-between gap-3 border-t border-[#30363d] bg-[#0d1117] px-4 py-3">
              <div className="text-xs text-gray-500">
                {activeStaleConflict.action === 'delete'
                  ? activeStaleConflictDetails.hasFreshLatestRow
                    ? 'Latest server row is visible above; you can re-mark delete or accept the refreshed server copy.'
                    : 'Refresh the current page to verify the latest server row before re-marking delete.'
                  : `${activeSelectedPendingCount} edited column${activeSelectedPendingCount === 1 ? '' : 's'} currently set to keep your pending value.`}
              </div>
              <div className="flex flex-wrap items-center justify-end gap-2">
                {activeStaleConflict.action === 'update' && (
                  <>
                    <button
                      type="button"
                      onClick={() => handleApplyStaleConflictPreset('recommended')}
                      className="rounded border border-blue-400/30 bg-[#161b22] px-3 py-1.5 text-xs text-blue-100 hover:bg-[#21262d] transition-colors"
                    >
                      Use Recommended Preset
                    </button>
                    <button
                      type="button"
                      onClick={() => handleApplyStaleConflictPreset('mine')}
                      className="rounded border border-[#30363d] bg-[#161b22] px-3 py-1.5 text-xs text-gray-300 hover:text-white transition-colors"
                    >
                      Keep All Mine
                    </button>
                    <button
                      type="button"
                      onClick={() => handleApplyStaleConflictPreset('server')}
                      className="rounded border border-[#30363d] bg-[#161b22] px-3 py-1.5 text-xs text-gray-300 hover:text-white transition-colors"
                    >
                      Use Server For All
                    </button>
                  </>
                )}
                <button
                  type="button"
                  onClick={handleReturnToStaleConflictOverview}
                  className="rounded border border-[#30363d] bg-[#161b22] px-3 py-1.5 text-xs text-gray-300 hover:text-white transition-colors"
                >
                  Back to Queue
                </button>
                <button
                  type="button"
                  onClick={handleCloseStaleConflict}
                  className="rounded border border-[#30363d] bg-[#161b22] px-3 py-1.5 text-xs text-gray-300 hover:text-white transition-colors"
                >
                  Close
                </button>
                <button
                  type="button"
                  onClick={handleRefreshStaleConflictContext}
                  disabled={isRefreshing || Boolean(pendingStaleRecovery)}
                  className="rounded border border-amber-400/30 bg-amber-500/10 px-3 py-1.5 text-xs text-amber-50 hover:bg-amber-500/20 transition-colors disabled:opacity-50"
                >
                  {isRefreshing ? 'Refreshing...' : 'Refresh Latest Context'}
                </button>
                <button
                  type="button"
                  onClick={() => handleUseLatestServerCopyForConflict()}
                  disabled={Boolean(pendingStaleRecovery)}
                  className="rounded border border-[#30363d] bg-[#161b22] px-3 py-1.5 text-xs text-gray-200 hover:bg-[#21262d] transition-colors disabled:opacity-50"
                >
                  Use Latest Server Copy
                </button>
                <button
                  type="button"
                  onClick={handleUseLatestServerCopyForConflictAndNext}
                  disabled={Boolean(pendingStaleRecovery) || activeStaleConflictReviewFailures.length <= 1}
                  className="rounded border border-[#30363d] bg-[#161b22] px-3 py-1.5 text-xs text-gray-200 hover:bg-[#21262d] transition-colors disabled:opacity-40"
                >
                  Use Latest & Next
                </button>
                {activeStaleConflict.action === 'update' && (
                  <button
                    type="button"
                    onClick={handleApplyRecommendedStaleConflictResolution}
                    disabled={!activeStaleConflictDetails.hasFreshLatestRow || activeStaleConflictDetails.locatedRowIdx < 0 || Boolean(pendingStaleRecovery)}
                    className="rounded border border-blue-400/30 bg-blue-500/10 px-3 py-1.5 text-xs text-blue-50 hover:bg-blue-500/20 transition-colors disabled:opacity-50"
                  >
                    Apply Recommended
                  </button>
                )}
                <button
                  type="button"
                  onClick={handleResolveStaleConflictWithCurrentRow}
                  disabled={!activeStaleConflictDetails.hasFreshLatestRow || activeStaleConflictDetails.locatedRowIdx < 0 || Boolean(pendingStaleRecovery)}
                  className="rounded border border-blue-500/30 bg-blue-600 px-3 py-1.5 text-xs text-white hover:bg-blue-500 transition-colors disabled:opacity-50"
                >
                  {activeStaleConflict.action === 'delete' ? 'Mark Delete on Latest Row' : 'Apply Selected Resolution'}
                </button>
                <button
                  type="button"
                  onClick={activeStaleConflict.action === 'delete' ? handleResolveStaleConflictWithCurrentRowAndNext : handleApplyRecommendedStaleConflictResolutionAndNext}
                  disabled={!activeStaleConflictDetails.hasFreshLatestRow || activeStaleConflictDetails.locatedRowIdx < 0 || Boolean(pendingStaleRecovery) || activeStaleConflictReviewFailures.length <= 1}
                  className="rounded border border-blue-500/30 bg-blue-600/80 px-3 py-1.5 text-xs text-white hover:bg-blue-500 transition-colors disabled:opacity-40"
                >
                  {activeStaleConflict.action === 'delete' ? 'Mark & Next' : 'Apply Recommended & Next'}
                </button>
                {activeStaleConflict.action === 'update' && (
                  <button
                    type="button"
                    onClick={handleResolveStaleConflictWithCurrentRowAndNext}
                    disabled={!activeStaleConflictDetails.hasFreshLatestRow || activeStaleConflictDetails.locatedRowIdx < 0 || Boolean(pendingStaleRecovery) || activeStaleConflictReviewFailures.length <= 1}
                    className="rounded border border-blue-500/30 bg-blue-600/80 px-3 py-1.5 text-xs text-white hover:bg-blue-500 transition-colors disabled:opacity-40"
                  >
                    Apply Selected & Next
                  </button>
                )}
              </div>
            </div>
          </div>
        </div>
      )}

      {showPasteModal && (
        <div className="fixed inset-0 z-[65] bg-black/60 backdrop-blur-sm flex items-center justify-center p-4">
          <div className="w-full max-w-3xl bg-[#161b22] border border-[#30363d] rounded-lg shadow-2xl overflow-hidden">
            <div className="flex items-center justify-between px-4 py-3 border-b border-[#30363d]">
              <div>
                <div className="text-sm font-medium text-white">Paste Rows into Draft Grid</div>
                <div className="text-xs text-gray-400">
                  Supports TSV or JSON. TSV without headers maps to the current visible column order.
                </div>
              </div>
              <button
                onClick={() => setShowPasteModal(false)}
                className="text-gray-400 hover:text-white transition-colors"
              >
                <X className="w-4 h-4" />
              </button>
            </div>
            <div className="space-y-3 p-4">
              <div className="flex flex-wrap items-center gap-2">
                {([
                  ['auto', 'Auto'],
                  ['tsv', 'TSV'],
                  ['json', 'JSON'],
                ] as const).map(([value, label]) => (
                  <button
                    key={value}
                    onClick={() => setPasteMode(value)}
                    className={`rounded border px-3 py-1 text-xs transition-colors ${pasteMode === value ? 'border-blue-500/40 bg-blue-500/15 text-blue-300' : 'border-[#30363d] bg-[#0d1117] text-gray-300 hover:text-white'}`}
                  >
                    {label}
                  </button>
                ))}
                <button
                  onClick={() => void handleReadClipboard()}
                  disabled={isReadingClipboard}
                  className="rounded border border-[#30363d] bg-[#0d1117] px-3 py-1 text-xs text-gray-300 hover:text-white transition-colors disabled:opacity-50"
                >
                  {isReadingClipboard ? 'Reading Clipboard...' : 'Read Clipboard'}
                </button>
                <button
                  onClick={() => setPasteText('')}
                  disabled={!pasteText.trim()}
                  className="rounded border border-[#30363d] bg-[#0d1117] px-3 py-1 text-xs text-gray-300 hover:text-white transition-colors disabled:opacity-50"
                >
                  Clear
                </button>
              </div>
              <textarea
                value={pasteText}
                onChange={(event) => setPasteText(event.target.value)}
                placeholder={'Paste TSV or JSON rows here...\n\nExamples:\n- id\\tname\\tage\\n1\\tAlice\\t18\n- [{\"name\":\"Alice\",\"age\":18}]'}
                className="min-h-[320px] w-full rounded border border-[#30363d] bg-[#0d1117] p-3 text-xs leading-6 text-gray-200 font-mono outline-none focus:border-blue-500"
              />
              <div className="flex items-start justify-between gap-3">
                <div className="text-xs text-gray-500">
                  Imported rows are appended as local draft rows first. Nothing is written to the database until you click Save.
                </div>
                <div className="flex items-center gap-2">
                  <button
                    onClick={() => setShowPasteModal(false)}
                    className="rounded border border-[#30363d] bg-[#0d1117] px-3 py-1.5 text-xs text-gray-300 hover:text-white transition-colors"
                  >
                    Cancel
                  </button>
                  <button
                    onClick={handleImportPastedRows}
                    disabled={!pasteText.trim()}
                    className="rounded border border-blue-500/30 bg-blue-600 px-3 py-1.5 text-xs text-white hover:bg-blue-500 transition-colors disabled:opacity-50"
                  >
                    Import as Draft Rows
                  </button>
                </div>
              </div>
            </div>
          </div>
        </div>
      )}

      {contextMenu && (
        <div 
          className="fixed bg-[#1c2128] border border-[#30363d] shadow-xl rounded overflow-hidden z-50 min-w-[150px]"
          style={{ top: contextMenu.y, left: contextMenu.x }}
        >
          {contextMenu.col && (
            <button
              type="button"
              className="flex w-full items-center gap-2 px-4 py-2 text-left text-sm text-gray-300 hover:bg-[#30363d]"
              onClick={() => handlePreviewCell(contextMenu.rowIdx, contextMenu.col!, contextMenu.isNew)}
            >
              <Eye className="w-4 h-4" /> Open Cell Viewer
            </button>
          )}
          {contextMenu.col && (
            <button
              type="button"
              className="flex w-full items-center gap-2 px-4 py-2 text-left text-sm text-gray-300 hover:bg-[#30363d]"
              onClick={() => handleCopyCell(contextMenu.rowIdx, contextMenu.col!, contextMenu.isNew)}
            >
              <Copy className="w-4 h-4" /> Copy Cell Value
            </button>
          )}
          {contextMenu.col && (
            <button
              type="button"
              disabled={!isNullableColumn(contextMenu.col)}
              className="flex w-full items-center gap-2 px-4 py-2 text-left text-sm text-gray-300 hover:bg-[#30363d] disabled:cursor-not-allowed disabled:opacity-40"
              onClick={() => handleSetCellNull(contextMenu.rowIdx, contextMenu.col!, contextMenu.isNew)}
            >
              <X className="w-4 h-4" /> Set NULL
            </button>
          )}
          {contextMenu.col && (
            <button
              type="button"
              disabled={!resolveLiteralColumnDefault(contextMenu.col).supported}
              className="flex w-full items-center gap-2 px-4 py-2 text-left text-sm text-gray-300 hover:bg-[#30363d] disabled:cursor-not-allowed disabled:opacity-40"
              onClick={() => handleApplyColumnDefault(contextMenu.rowIdx, contextMenu.col!, contextMenu.isNew)}
            >
              <Save className="w-4 h-4" /> Apply Schema Default
            </button>
          )}
          <div className="h-px bg-[#30363d] my-1" />
          <button
            type="button"
            className="flex w-full items-center gap-2 px-4 py-2 text-left text-sm text-gray-300 hover:bg-[#30363d]"
            onClick={() => handleDuplicateRow(contextMenu.rowIdx, contextMenu.isNew)}
          >
            <Plus className="w-4 h-4" /> Duplicate Row
          </button>
          <button
            type="button"
            className="flex w-full items-center gap-2 px-4 py-2 text-left text-sm text-gray-300 hover:bg-[#30363d]"
            onClick={() => handleCopyRow(contextMenu.rowIdx, contextMenu.isNew)}
          >
            <Copy className="w-4 h-4" /> Copy Row (Excel/TSV)
          </button>
          <button
            type="button"
            className="flex w-full items-center gap-2 px-4 py-2 text-left text-sm text-gray-300 hover:bg-[#30363d]"
            onClick={() => handleCopyRowJson(contextMenu.rowIdx, contextMenu.isNew)}
          >
            <Copy className="w-4 h-4" /> Copy Row JSON
          </button>
          <button
            type="button"
            className="flex w-full items-center gap-2 px-4 py-2 text-left text-sm text-gray-300 hover:bg-[#30363d]"
            onClick={() => handleCopyRowSql(contextMenu.rowIdx, contextMenu.isNew)}
          >
            <Copy className="w-4 h-4" /> Copy Row SQL
          </button>
          <div className="h-px bg-[#30363d] my-1" />
          <button
            type="button"
            className="flex w-full items-center gap-2 px-4 py-2 text-left text-sm text-red-400 hover:bg-red-500/20"
            onClick={handleDeleteRow}
          >
            <Trash2 className="w-4 h-4" /> Delete Row
          </button>
        </div>
      )}

      {previewCell && (
        <div className="fixed inset-0 z-[60] bg-black/60 backdrop-blur-sm flex items-center justify-center p-4">
          <div className="w-full max-w-4xl max-h-[80vh] bg-[#161b22] border border-[#30363d] rounded-lg shadow-2xl flex flex-col overflow-hidden">
            <div className="flex items-center justify-between px-4 py-3 border-b border-[#30363d]">
              <div>
                <div className="text-sm font-medium text-white">{previewCell.title}</div>
                <div className="text-xs text-gray-400">
                  {previewCell.format === 'json' ? 'JSON large-value preview' : 'Large value preview'}
                  {` · ${previewCell.draft.length} chars`}
                </div>
              </div>
              <div className="flex items-center gap-2">
                <button
                  onClick={() => void handleCopyPreviewValue()}
                  className="text-xs text-gray-300 hover:text-white bg-[#21262d] hover:bg-[#30363d] px-2 py-1 rounded border border-[#30363d] transition-colors"
                >
                  Copy
                </button>
                <button
                  onClick={handleDownloadPreviewValue}
                  className="text-xs text-gray-300 hover:text-white bg-[#21262d] hover:bg-[#30363d] px-2 py-1 rounded border border-[#30363d] transition-colors"
                >
                  Download
                </button>
                <button
                  onClick={() => setPreviewCell(null)}
                  className="text-gray-400 hover:text-white transition-colors"
                >
                  <X className="w-4 h-4" />
                </button>
              </div>
            </div>
            <div className="p-4 overflow-auto">
              <textarea
                value={previewCell.draft}
                onChange={(e) => setPreviewCell((prev) => prev ? { ...prev, draft: e.target.value } : prev)}
                className="min-h-[360px] w-full rounded border border-[#30363d] bg-[#0d1117] p-3 text-xs leading-6 text-gray-200 whitespace-pre-wrap break-words font-mono outline-none focus:border-blue-500"
              />
            </div>
            <div className="flex items-center justify-between gap-3 border-t border-[#30363d] bg-[#0d1117] px-4 py-3">
              <div className="text-xs text-gray-500">
                {previewCell.format === 'json'
                  ? 'Edit JSON/text here, then apply it back to the grid.'
                  : 'Edit the full text here, then apply it back to the grid.'}
              </div>
              <div className="flex items-center gap-2">
                <button
                  onClick={() => setPreviewCell(null)}
                  className="text-xs text-gray-300 hover:text-white bg-[#21262d] hover:bg-[#30363d] px-3 py-1.5 rounded border border-[#30363d] transition-colors"
                >
                  Cancel
                </button>
                <button
                  onClick={handleApplyPreviewEdit}
                  className="text-xs text-white bg-blue-600 hover:bg-blue-500 px-3 py-1.5 rounded border border-blue-500/30 transition-colors"
                >
                  Apply to Cell
                </button>
              </div>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

function CellDisplay({
  val,
  isNewEmpty = false,
  inlineValue,
  titleText,
  invalidMessage,
}: {
  val: any,
  isNewEmpty?: boolean,
  inlineValue?: string,
  titleText?: string,
  invalidMessage?: string,
}) {
  if (isNewEmpty) return <span className="text-gray-600 italic" title={titleText}>Empty</span>;
  if (val === null) return <span className="text-gray-600 italic" title={titleText}>NULL</span>;
  if (typeof val === 'boolean') {
    return (
      <span
        title={titleText}
        className={`px-1.5 py-0.5 rounded text-[10px] font-bold uppercase ${val ? 'bg-green-500/20 text-green-400' : 'bg-red-500/20 text-red-400'}`}
      >
        {val ? 'TRUE' : 'FALSE'}
      </span>
    );
  }
  return (
    <span className={`block truncate ${invalidMessage ? 'text-red-100' : 'text-gray-300'}`} title={titleText}>
      {inlineValue || String(val)}
    </span>
  );
}

function FilterDropdown({ col, filters, setFilters, onClose }: { 
  col: string; 
  filters: any[]; 
  setFilters: any; 
  onClose: () => void;
}) {
  const existing = filters.find(f => f.column === col);
  const [operator, setOperator] = useState(existing?.operator || 'equals');
  const [value, setValue] = useState(existing?.value || '');
  const requiresValue = !['is_null', 'is_not_null'].includes(operator);
  const placeholderMap: Record<string, string> = {
    between: 'min, max',
    in: 'a, b, c',
    not_in: 'a, b, c',
  };
  const helperTextMap: Record<string, string> = {
    between: 'Use comma-separated min and max values.',
    in: 'Use comma-separated values.',
    not_in: 'Use comma-separated values.',
  };
  const inputPlaceholder = placeholderMap[operator] || 'Value...';
  const helperText = helperTextMap[operator] || null;

  const apply = () => {
    const newFilters = filters.filter(f => f.column !== col);
    const normalizedValue = value.trim();
    if (!requiresValue || normalizedValue) {
      newFilters.push({ column: col, operator, value: normalizedValue });
    }
    setFilters(newFilters);
    onClose();
  };

  const clear = () => {
    setFilters(filters.filter(f => f.column !== col));
    onClose();
  };

  return (
    <div 
      className="absolute top-full left-0 mt-1 bg-[#1c2128] border border-[#30363d] shadow-xl rounded p-3 z-50 w-56"
      onClick={e => e.stopPropagation()}
    >
      <div className="text-xs font-semibold mb-2 text-gray-300">Filter {col}</div>
      <select 
        value={operator} 
        onChange={e => setOperator(e.target.value)}
        className="w-full bg-[#0d1117] border border-[#30363d] rounded p-1 mb-2 text-sm text-gray-200 outline-none focus:border-blue-500"
      >
        <option value="equals">Equals</option>
        <option value="not_equals">Not Equals</option>
        <option value="contains">Contains</option>
        <option value="starts_with">Starts With</option>
        <option value="ends_with">Ends With</option>
        <option value="greater_than">Greater Than</option>
        <option value="less_than">Less Than</option>
        <option value="between">Between</option>
        <option value="in">In</option>
        <option value="not_in">Not In</option>
        <option value="is_null">Is NULL</option>
        <option value="is_not_null">Is NOT NULL</option>
      </select>
      {requiresValue ? (
        <>
          <input 
            type="text" 
            value={value} 
            onChange={e => setValue(e.target.value)}
            placeholder={inputPlaceholder}
            className="w-full bg-[#0d1117] border border-[#30363d] rounded p-1 text-sm text-gray-200 outline-none focus:border-blue-500"
            onKeyDown={e => { if (e.key === 'Enter') apply(); }}
          />
          <div className="mb-3 mt-1 min-h-[16px] text-[11px] text-gray-500">
            {helperText}
          </div>
        </>
      ) : (
        <div className="mb-3 rounded border border-[#30363d] bg-[#0d1117] px-2 py-1.5 text-xs text-gray-400">
          This filter does not require a value.
        </div>
      )}
      <div className="flex gap-2">
        <button 
          onClick={clear}
          className="flex-1 bg-gray-700 hover:bg-gray-600 rounded py-1 text-xs text-white transition-colors"
        >
          Clear
        </button>
        <button 
          onClick={apply}
          className="flex-1 bg-blue-600 hover:bg-blue-500 rounded py-1 text-xs text-white transition-colors"
        >
          Apply
        </button>
      </div>
    </div>
  );
}
