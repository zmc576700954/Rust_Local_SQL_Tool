import { useState, useMemo, useEffect, useCallback } from 'react';
import { ArrowDown, ArrowUp, ArrowUpDown, ChevronLeft, ChevronRight, Copy, Eye, X } from 'lucide-react';
import { tr } from '../i18n';

interface SimpleDataTableProps {
  data: any[];
}

type PreviewPayload = {
  title: string;
  value: string;
  format: 'text' | 'json';
  downloadExtension: 'txt' | 'json';
};

type ColumnLayoutState = {
  order: string[];
  hidden: string[];
  widths: Record<string, number>;
};

const DEFAULT_COLUMN_WIDTH = 220;

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

export function SimpleDataTable({ data }: SimpleDataTableProps) {
  const [sortConfig, setSortConfig] = useState<{ key: string; direction: 'asc' | 'desc' } | null>(null);
  const [currentPage, setCurrentPage] = useState(1);
  const [contextMenu, setContextMenu] = useState<{ x: number, y: number, rowIdx: number, col: string, val: any } | null>(null);
  const [previewCell, setPreviewCell] = useState<PreviewPayload | null>(null);
  const [showColumnMenu, setShowColumnMenu] = useState(false);
  const [resizingColumn, setResizingColumn] = useState<{ column: string; startX: number; startWidth: number } | null>(null);
  const pageSize = 100;

  useEffect(() => {
    const handleClick = () => {
      setContextMenu(null);
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

  const columns = useMemo(() => {
    if (!data || data.length === 0) return [];
    return Object.keys(data[0]);
  }, [data]);

  const layoutStorageKey = useMemo(() => columns.length > 0 ? `simple-data-table-layout:${columns.join('|')}` : null, [columns]);
  const [columnLayout, setColumnLayout] = useState<ColumnLayoutState>(() => buildDefaultColumnLayout([]));

  useEffect(() => {
    setColumnLayout(() => {
      if (!layoutStorageKey || columns.length === 0) return buildDefaultColumnLayout(columns);
      try {
        const raw = window.localStorage.getItem(layoutStorageKey);
        return normalizeColumnLayout(raw ? JSON.parse(raw) : null, columns);
      } catch {
        return buildDefaultColumnLayout(columns);
      }
    });
  }, [columns, layoutStorageKey]);

  useEffect(() => {
    if (!layoutStorageKey || columns.length === 0) return;
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

  const sortedData = useMemo(() => {
    if (!sortConfig) return data;
    
    return [...data].sort((a, b) => {
      const valA = a[sortConfig.key];
      const valB = b[sortConfig.key];

      if (valA === valB) return 0;
      
      // Handle nulls
      if (valA === null) return sortConfig.direction === 'asc' ? 1 : -1;
      if (valB === null) return sortConfig.direction === 'asc' ? -1 : 1;

      // Number comparison
      if (typeof valA === 'number' && typeof valB === 'number') {
        return sortConfig.direction === 'asc' ? valA - valB : valB - valA;
      }

      // String comparison
      const strA = String(valA).toLowerCase();
      const strB = String(valB).toLowerCase();
      
      if (strA < strB) return sortConfig.direction === 'asc' ? -1 : 1;
      if (strA > strB) return sortConfig.direction === 'asc' ? 1 : -1;
      
      return 0;
    });
  }, [data, sortConfig]);

  const totalPages = Math.ceil((sortedData?.length || 0) / pageSize);

  const paginatedData = useMemo(() => {
    const start = (currentPage - 1) * pageSize;
    return sortedData?.slice(start, start + pageSize) || [];
  }, [sortedData, currentPage]);

  const requestSort = (key: string) => {
    let direction: 'asc' | 'desc' = 'asc';
    if (sortConfig && sortConfig.key === key && sortConfig.direction === 'asc') {
      direction = 'desc';
    }
    setSortConfig({ key, direction });
    setCurrentPage(1);
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

  const stringifyJson = useCallback((value: unknown) => {
    return JSON.stringify(value, (_key, item) => typeof item === 'bigint' ? item.toString() : item, 2);
  }, []);

  const normalizePreviewPayload = useCallback((title: string, value: unknown): PreviewPayload => {
    if (value === null || value === undefined) {
      return {
        title,
        value: 'NULL',
        format: 'text',
        downloadExtension: 'txt',
      };
    }

    if (typeof value === 'object') {
      return {
        title,
        value: stringifyJson(value),
        format: 'json',
        downloadExtension: 'json',
      };
    }

    const textValue = String(value);
    const trimmed = textValue.trim();
    if (trimmed && (trimmed.startsWith('{') || trimmed.startsWith('['))) {
      try {
        return {
          title,
          value: stringifyJson(JSON.parse(trimmed)),
          format: 'json',
          downloadExtension: 'json',
        };
      } catch {
        // fall through to plain text
      }
    }

    return {
      title,
      value: textValue,
      format: 'text',
      downloadExtension: 'txt',
    };
  }, [stringifyJson]);

  const formatInlineCellValue = useCallback((value: unknown) => {
    const preview = normalizePreviewPayload('', value).value.replace(/\s+/g, ' ').trim();
    if (!preview) return '';
    return preview.length > 120 ? `${preview.slice(0, 120)}…` : preview;
  }, [normalizePreviewPayload]);

  const formatCellValue = useCallback((value: unknown) => {
    if (value === null || value === undefined) return '';
    if (typeof value === 'object') return stringifyJson(value);
    return String(value);
  }, [stringifyJson]);

  const emitToast = useCallback((message: string, type: 'success' | 'error') => {
    window.dispatchEvent(new CustomEvent('global-toast', { detail: { message, type } }));
  }, []);

  const copyTextToClipboard = useCallback(async (text: string, successMessage: string, errorMessage: string) => {
    try {
      await navigator.clipboard.writeText(text);
      emitToast(successMessage, 'success');
      return true;
    } catch {
      emitToast(errorMessage, 'error');
      return false;
    }
  }, [emitToast]);

  const buildInsertStatement = useCallback((row: Record<string, unknown>) => {
    const values = columns.map((column: string) => {
      const value = row[column];
      if (value === null || value === undefined) return 'NULL';
      if (typeof value === 'number' || typeof value === 'boolean') return String(value);
      const normalized = typeof value === 'object' ? stringifyJson(value) : String(value);
      return `'${normalized.replace(/'/g, "''")}'`;
    });

    return `INSERT INTO \`query_result\` (${columns.map((column: string) => `\`${column}\``).join(', ')}) VALUES (${values.join(', ')});`;
  }, [columns, stringifyJson]);

  const handleCopyCell = useCallback(async () => {
    if (!contextMenu) return;
    await copyTextToClipboard(
      formatCellValue(contextMenu.val) || 'NULL',
      tr('单元格内容已复制', 'Cell value copied to clipboard'),
      tr('复制单元格内容失败', 'Failed to copy cell value')
    );
    setContextMenu(null);
  }, [contextMenu, copyTextToClipboard, formatCellValue]);

  const handlePreviewCell = useCallback(() => {
    if (!contextMenu) return;
    setPreviewCell(normalizePreviewPayload(contextMenu.col, contextMenu.val));
    setContextMenu(null);
  }, [contextMenu, normalizePreviewPayload]);

  const handleCopyRowTsv = useCallback(async () => {
    if (!contextMenu) return;
    const row = paginatedData[contextMenu.rowIdx];
    const rowString = columns.map((col: string) => {
      const value = row[col];
      if (value === null || value === undefined) return '';
      if (typeof value === 'object') return JSON.stringify(value);
      return String(value).replace(/\t/g, ' ').replace(/\n/g, ' ');
    }).join('\t');
    await copyTextToClipboard(rowString, tr('行已复制（TSV）', 'Row copied to clipboard (TSV)'), tr('复制行失败', 'Failed to copy row'));
    setContextMenu(null);
  }, [contextMenu, paginatedData, columns, copyTextToClipboard]);

  const handleCopyRowJson = useCallback(async () => {
    if (!contextMenu) return;
    const row = paginatedData[contextMenu.rowIdx];
    await copyTextToClipboard(stringifyJson(row), tr('行 JSON 已复制', 'Row JSON copied to clipboard'), tr('复制行 JSON 失败', 'Failed to copy row JSON'));
    setContextMenu(null);
  }, [contextMenu, paginatedData, stringifyJson, copyTextToClipboard]);

  const handleCopyRowSql = useCallback(async () => {
    if (!contextMenu) return;
    const row = paginatedData[contextMenu.rowIdx];
    await copyTextToClipboard(buildInsertStatement(row), tr('行 SQL 已复制', 'Row SQL copied to clipboard'), tr('复制行 SQL 失败', 'Failed to copy row SQL'));
    setContextMenu(null);
  }, [contextMenu, paginatedData, buildInsertStatement, copyTextToClipboard]);

  const handleCopyPreviewValue = useCallback(async () => {
    if (!previewCell) return;
    await copyTextToClipboard(
      previewCell.value,
      tr('大字段内容已复制', 'Large value copied to clipboard'),
      tr('复制大字段失败', 'Failed to copy large value')
    );
  }, [previewCell, copyTextToClipboard]);

  const handleDownloadPreviewValue = useCallback(() => {
    if (!previewCell) return;
    const blob = new Blob([previewCell.value], {
      type: previewCell.format === 'json' ? 'application/json;charset=utf-8;' : 'text/plain;charset=utf-8;',
    });
    const url = URL.createObjectURL(blob);
    const link = document.createElement('a');
    link.href = url;
    link.setAttribute('download', `cell_${previewCell.title}_${new Date().toISOString().replace(/[:.]/g, '-')}.${previewCell.downloadExtension}`);
    document.body.appendChild(link);
    link.click();
    document.body.removeChild(link);
    URL.revokeObjectURL(url);
  }, [previewCell]);

  if (!data || data.length === 0) return null;

  return (
    <div className="flex flex-col w-full h-full overflow-hidden">
      <div className="flex items-center justify-between gap-3 border-b border-[#30363d]/50 bg-[#0d1117] px-4 py-2 text-xs text-gray-400">
        <div>
          Showing {visibleColumns.length}/{columns.length} columns
        </div>
        <div className="flex items-center gap-2" onClick={(event) => event.stopPropagation()}>
          <div className="relative">
            <button
              onClick={() => setShowColumnMenu((prev) => !prev)}
              className="rounded border border-[#30363d] bg-[#161b22] px-2 py-1 text-gray-300 hover:text-white transition-colors"
            >
              Columns
            </button>
            {showColumnMenu && (
              <div className="absolute right-0 top-full z-30 mt-2 w-72 rounded border border-[#30363d] bg-[#161b22] p-3 shadow-2xl">
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
            className="rounded border border-[#30363d] bg-[#161b22] px-2 py-1 text-gray-300 hover:text-white transition-colors disabled:opacity-40"
          >
            Reset Layout
          </button>
        </div>
      </div>
      <div className="flex-1 overflow-auto">
        <table className="w-full text-left text-sm whitespace-nowrap">
          <thead className="bg-[#0d1117] sticky top-0 shadow-sm text-gray-400 text-xs tracking-wider z-10">
            <tr>
              {visibleColumns.map(k => (
                <th 
                  key={k} 
                  className="py-2.5 px-4 font-medium border-r border-[#30363d]/50 cursor-pointer hover:bg-[#21262d] transition-colors group select-none relative"
                  onClick={() => requestSort(k)}
                  style={{ width: `${getColumnWidth(k)}px`, minWidth: `${getColumnWidth(k)}px`, maxWidth: `${getColumnWidth(k)}px` }}
                >
                  <div className="flex items-center justify-between gap-2">
                    <span>{k}</span>
                    <span className="text-gray-600 group-hover:text-gray-400 flex-shrink-0">
                      {sortConfig?.key === k ? (
                        sortConfig.direction === 'asc' ? <ArrowUp className="w-3 h-3 text-blue-400" /> : <ArrowDown className="w-3 h-3 text-blue-400" />
                      ) : (
                        <ArrowUpDown className="w-3 h-3 opacity-0 group-hover:opacity-100 transition-opacity" />
                      )}
                    </span>
                  </div>
                  <div
                    className="absolute top-0 right-0 h-full w-1.5 cursor-col-resize hover:bg-blue-500/30"
                    onMouseDown={(event) => {
                      event.stopPropagation();
                      setResizingColumn({ column: k, startX: event.clientX, startWidth: getColumnWidth(k) });
                    }}
                  />
                </th>
              ))}
            </tr>
          </thead>
          <tbody className="divide-y divide-[#30363d]/50">
            {paginatedData.map((row: any, i: number) => (
              <tr key={i} className="hover:bg-[#161b22] even:bg-[#0d1117]">
                {visibleColumns.map((k) => {
                  const val = row[k];
                  const inlineValue = formatInlineCellValue(val);
                  return (
                    <td 
                      key={k} 
                      onContextMenu={(e) => {
                        e.preventDefault();
                        setContextMenu({
                          x: e.clientX,
                          y: e.clientY,
                          rowIdx: i,
                          col: k,
                          val
                        });
                      }}
                      className={`py-1.5 px-4 border-r border-[#30363d]/50 max-w-[300px] truncate ${typeof val === 'number' ? 'text-right text-blue-400 font-mono' : 'text-gray-300'}`}
                      style={{ width: `${getColumnWidth(k)}px`, minWidth: `${getColumnWidth(k)}px`, maxWidth: `${getColumnWidth(k)}px` }}
                    >
                      {val === null ? (
                        <span className="text-gray-600 italic">NULL</span>
                      ) : typeof val === 'boolean' ? (
                        <span className={`px-1.5 py-0.5 rounded text-[10px] font-bold uppercase ${val ? 'bg-green-500/20 text-green-400' : 'bg-red-500/20 text-red-400'}`}>
                          {val ? 'TRUE' : 'FALSE'}
                        </span>
                      ) : (
                        <span className="block truncate" title={formatCellValue(val)}>
                          {inlineValue}
                        </span>
                      )}
                    </td>
                  );
                })}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      
      {totalPages > 1 && (
        <div className="flex items-center justify-between px-4 py-2.5 bg-[#0d1117] border-t border-[#30363d]/50 text-sm text-gray-400 shrink-0">
          <div>
            {tr(`第 ${currentPage} / ${totalPages} 页`, `Page ${currentPage} of ${totalPages}`)}
          </div>
          <div className="flex items-center gap-2">
            <button
              onClick={() => setCurrentPage(p => Math.max(1, p - 1))}
              disabled={currentPage === 1}
              className="flex items-center gap-1 px-2 py-1 rounded hover:bg-[#21262d] disabled:opacity-50 disabled:cursor-not-allowed transition-colors"
            >
              <ChevronLeft className="w-4 h-4" />
              <span>{tr('上一页', 'Prev')}</span>
            </button>
            <button
              onClick={() => setCurrentPage(p => Math.min(totalPages, p + 1))}
              disabled={currentPage === totalPages}
              className="flex items-center gap-1 px-2 py-1 rounded hover:bg-[#21262d] disabled:opacity-50 disabled:cursor-not-allowed transition-colors"
            >
              <span>{tr('下一页', 'Next')}</span>
              <ChevronRight className="w-4 h-4" />
            </button>
          </div>
        </div>
      )}

      {contextMenu && (
        <div 
          className="fixed z-50 bg-[#161b22] border border-[#30363d] rounded-md shadow-xl py-1 text-sm text-gray-200 min-w-[160px]"
          style={{ top: contextMenu.y, left: contextMenu.x }}
          onClick={(e) => e.stopPropagation()}
        >
          <button 
            className="w-full text-left px-4 py-2 hover:bg-[#21262d] transition-colors flex items-center gap-2"
            onClick={handlePreviewCell}
          >
            <Eye className="w-4 h-4" />
            {tr('打开单元格查看器', 'Open Cell Viewer')}
          </button>
          <button 
            className="w-full text-left px-4 py-2 hover:bg-[#21262d] transition-colors flex items-center gap-2"
            onClick={handleCopyCell}
          >
            <Copy className="w-4 h-4" />
            {tr('复制单元格', 'Copy Cell')}
          </button>
          <button 
            className="w-full text-left px-4 py-2 hover:bg-[#21262d] transition-colors flex items-center gap-2"
            onClick={handleCopyRowTsv}
          >
            <Copy className="w-4 h-4" />
            {tr('复制行（Excel/TSV）', 'Copy Row (Excel/TSV)')}
          </button>
          <button 
            className="w-full text-left px-4 py-2 hover:bg-[#21262d] transition-colors flex items-center gap-2"
            onClick={handleCopyRowJson}
          >
            <Copy className="w-4 h-4" />
            {tr('复制行 JSON', 'Copy Row JSON')}
          </button>
          <button 
            className="w-full text-left px-4 py-2 hover:bg-[#21262d] transition-colors flex items-center gap-2"
            onClick={handleCopyRowSql}
          >
            <Copy className="w-4 h-4" />
            {tr('复制行 SQL', 'Copy Row SQL')}
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
                  {previewCell.format === 'json'
                    ? tr('JSON 大字段预览', 'JSON large-value preview')
                    : tr('大字段预览', 'Large value preview')}
                  {` · ${previewCell.value.length} chars`}
                </div>
              </div>
              <div className="flex items-center gap-2">
                <button
                  onClick={() => void handleCopyPreviewValue()}
                  className="text-xs text-gray-300 hover:text-white bg-[#21262d] hover:bg-[#30363d] px-2 py-1 rounded border border-[#30363d] transition-colors"
                >
                  {tr('复制', 'Copy')}
                </button>
                <button
                  onClick={handleDownloadPreviewValue}
                  className="text-xs text-gray-300 hover:text-white bg-[#21262d] hover:bg-[#30363d] px-2 py-1 rounded border border-[#30363d] transition-colors"
                >
                  {tr('下载', 'Download')}
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
              <pre className="text-xs leading-6 text-gray-200 whitespace-pre-wrap break-words font-mono">
                {previewCell.value}
              </pre>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
