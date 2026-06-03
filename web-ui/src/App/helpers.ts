/** Utility functions and constants extracted from App.tsx */

import type { QueryExecutionResult, QueryResultCompareReport, SavedSqlBookmark } from '../types';

export const QUERY_CHUNK_SIZE = 200;
export const SQL_BOOKMARKS_KEY = 'sql_workbench_bookmarks_v1';
export const MAX_SQL_BOOKMARKS = 50;
export const DEFAULT_QUERY_SQL = '-- Generated SQL will appear here\n';

export const buildDefaultBookmarkTitle = (sql: string) => {
  const firstLine = sql
    .split(/\r?\n/)
    .map((line) => line.trim())
    .find(Boolean);

  if (!firstLine) return 'SQL Bookmark';
  return firstLine.length > 56 ? `${firstLine.slice(0, 56)}…` : firstLine;
};

export const normalizeSavedBookmarks = (raw: unknown): SavedSqlBookmark[] => {
  if (!Array.isArray(raw)) return [];

  return raw
    .filter((item): item is SavedSqlBookmark => Boolean(
      item
      && typeof item === 'object'
      && typeof (item as SavedSqlBookmark).id === 'string'
      && typeof (item as SavedSqlBookmark).title === 'string'
      && typeof (item as SavedSqlBookmark).sql === 'string'
    ))
    .map((item) => ({
      ...item,
      description: item.description || null,
      db_id: item.db_id || null,
      db_label: item.db_label || null,
      created_at: typeof item.created_at === 'number' ? item.created_at : Date.now(),
      updated_at: typeof item.updated_at === 'number' ? item.updated_at : Date.now(),
    }))
    .slice(0, MAX_SQL_BOOKMARKS);
};

export function createCancelToken(): string {
  if (typeof crypto !== 'undefined' && typeof crypto.randomUUID === 'function') {
    return crypto.randomUUID();
  }
  return `cancel-${Date.now()}-${Math.random().toString(16).slice(2)}`;
}

export const stringifyJsonArtifact = (value: unknown) =>
  JSON.stringify(value, (_key, innerValue) => typeof innerValue === 'bigint' ? innerValue.toString() : innerValue, 2);

export const normalizeComparableValue = (value: unknown): unknown => {
  if (typeof value === 'bigint') return value.toString();
  if (Array.isArray(value)) return value.map((item) => normalizeComparableValue(item));
  if (value && typeof value === 'object') {
    return Object.keys(value as Record<string, unknown>)
      .sort()
      .reduce<Record<string, unknown>>((acc, key) => {
        acc[key] = normalizeComparableValue((value as Record<string, unknown>)[key]);
        return acc;
      }, {});
  }
  return value;
};

export const serializeComparableRow = (row: unknown) => JSON.stringify(normalizeComparableValue(row));

export const cloneQueryExecutionResultSnapshot = (result: QueryExecutionResult): QueryExecutionResult =>
  JSON.parse(stringifyJsonArtifact(result)) as QueryExecutionResult;

export const canCompareQueryResult = (result: QueryExecutionResult | null | undefined): result is QueryExecutionResult => {
  if (!result || result.status !== 'success' || result.error) return false;
  return Array.isArray(result.rows) && (result.rows.length > 0 || (result.columns?.length ?? 0) > 0);
};

export const buildQueryResultCompareReport = (
  baseline: QueryExecutionResult | null | undefined,
  current: QueryExecutionResult | null | undefined
): QueryResultCompareReport | null => {
  if (!canCompareQueryResult(baseline) || !canCompareQueryResult(current)) {
    return null;
  }

  const baselineCounts = new Map<string, { count: number; row: any }>();
  for (const row of baseline.rows) {
    const key = serializeComparableRow(row);
    const existing = baselineCounts.get(key);
    if (existing) {
      existing.count += 1;
    } else {
      baselineCounts.set(key, { count: 1, row });
    }
  }

  let unchangedCount = 0;
  const addedRows: any[] = [];
  for (const row of current.rows) {
    const key = serializeComparableRow(row);
    const existing = baselineCounts.get(key);
    if (existing && existing.count > 0) {
      existing.count -= 1;
      unchangedCount += 1;
    } else {
      addedRows.push(row);
    }
  }

  const removedRows: any[] = [];
  baselineCounts.forEach(({ count, row }) => {
    for (let index = 0; index < count; index += 1) {
      removedRows.push(row);
    }
  });

  return {
    baseline_statement_label: baseline.statement_label || null,
    current_statement_label: current.statement_label || null,
    baseline_source_sql: baseline.source_sql || null,
    current_source_sql: current.source_sql || null,
    baseline_execution_time_ms: baseline.execution_time_ms,
    current_execution_time_ms: current.execution_time_ms,
    compared_at: Date.now(),
    summary: {
      baseline_row_count: baseline.rows.length,
      current_row_count: current.rows.length,
      added_count: addedRows.length,
      removed_count: removedRows.length,
      unchanged_count: unchangedCount,
    },
    added_rows: addedRows,
    removed_rows: removedRows,
  };
};
