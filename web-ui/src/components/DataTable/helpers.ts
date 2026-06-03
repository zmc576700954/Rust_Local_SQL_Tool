/** Constants and helper functions for the DataTable component. */

import type {
  StaleConflictOverviewGroupKey,
  StaleConflictOverviewGroupState,
  StaleConflictOverviewSummary,
  StaleConflictQueueFilter,
  ColumnLayoutState,
} from './types';

export const DEFAULT_COLUMN_WIDTH = 220;
export const APPROX_ROW_HEIGHT = 33;
export const SAVE_REVIEW_PREVIEW_LIMIT = 3;

export const STALE_CONFLICT_OVERVIEW_GROUP_ORDER: StaleConflictOverviewGroupKey[] = ['high_risk', 'needs_refresh', 'delete', 'safe_edits', 'other'];

export const STALE_CONFLICT_OVERVIEW_GROUP_LABELS: Record<StaleConflictOverviewGroupKey, string> = {
  high_risk: 'High Risk',
  needs_refresh: 'Needs Refresh',
  delete: 'Deletes',
  safe_edits: 'Safe Edits',
  other: 'Other',
};

export const STALE_CONFLICT_OVERVIEW_GROUP_HINTS: Record<StaleConflictOverviewGroupKey, string> = {
  high_risk: 'Direct conflicts that still need a column-by-column decision.',
  needs_refresh: 'Refresh before you can safely rebase or retry.',
  delete: 'Delete retries and latest-server-copy decisions.',
  safe_edits: 'Safe stale edits that can be batch merged quickly.',
  other: 'Remaining stale items on the current page.',
};

export const STALE_CONFLICT_QUEUE_FILTER_LABELS: Record<StaleConflictQueueFilter, string> = {
  all: 'Visible Queue',
  high_risk: 'High Risk Queue',
  needs_refresh: 'Needs Refresh Queue',
  safe_edits: 'Safe Edits Queue',
  delete: 'Delete Queue',
};

export function createStaleConflictOverviewCollapsedState(expandedGroup: StaleConflictOverviewGroupKey | null = 'high_risk'): StaleConflictOverviewGroupState {
  return {
    high_risk: expandedGroup !== 'high_risk',
    needs_refresh: expandedGroup !== 'needs_refresh',
    delete: expandedGroup !== 'delete',
    safe_edits: expandedGroup !== 'safe_edits',
    other: expandedGroup !== 'other',
  };
}

export function matchesStaleConflictQueueFilter(summary: StaleConflictOverviewSummary, filter: StaleConflictQueueFilter) {
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

export function getStaleConflictOverviewGroup(summary: StaleConflictOverviewSummary): StaleConflictOverviewGroupKey {
  if (summary.isHighRisk) return 'high_risk';
  if (summary.needsRefresh) return 'needs_refresh';
  if (summary.failure.action === 'delete') return 'delete';
  if (summary.isSafeUpdate) return 'safe_edits';
  return 'other';
}

export function getPreferredStaleConflictOverviewGroup(summaries: StaleConflictOverviewSummary[]) {
  const visibleGroups = new Set(summaries.map((summary) => getStaleConflictOverviewGroup(summary)));
  return STALE_CONFLICT_OVERVIEW_GROUP_ORDER.find((groupKey) => visibleGroups.has(groupKey)) || null;
}

export function buildDefaultColumnLayout(columns: string[]): ColumnLayoutState {
  return {
    order: columns,
    hidden: [],
    widths: {},
  };
}

export function normalizeColumnLayout(raw: unknown, columns: string[]): ColumnLayoutState {
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
