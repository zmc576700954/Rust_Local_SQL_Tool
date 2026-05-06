import { useState, useMemo, useEffect, useRef, useCallback } from 'react';
import { ArrowDown, ArrowUp, Filter, Save, Undo, Plus, Trash2, Copy } from 'lucide-react';
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
  isActive
}: DataTableProps) {
  const [editingCell, setEditingCell] = useState<{ rowIdx: number; col: string; isNew: boolean } | null>(null);
  
  // Track changes
  const [modifiedRows, setModifiedRows] = useState<{ [idx: number]: any }>({});
  const [deletedRowIdxs, setDeletedRowIdxs] = useState<Set<number>>(new Set());
  const [newRows, setNewRows] = useState<any[]>([]);
  
  // Context Menu
  const [contextMenu, setContextMenu] = useState<{ x: number, y: number, rowIdx: number, col?: string, isNew: boolean } | null>(null);

  // Filter Dropdown
  const [filterMenu, setFilterMenu] = useState<{ col: string } | null>(null);

  const columns = useMemo(() => {
    return schema.columns.map((c: any) => c.column_name);
  }, [schema]);

  const primaryKeys = useMemo(() => {
    const pkIdxs = schema.indexes.filter((i: any) => i.index_name === 'PRIMARY');
    return pkIdxs.map((i: any) => i.column_name);
  }, [schema]);

  // Handle click outside to close menus
  useEffect(() => {
    const handleClick = () => {
      setContextMenu(null);
      setFilterMenu(null);
    };
    window.addEventListener('click', handleClick);
    return () => window.removeEventListener('click', handleClick);
  }, []);

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

  const handleCellDoubleClick = (rowIdx: number, col: string, isNew: boolean) => {
    setEditingCell({ rowIdx, col, isNew });
  };

  const handleCellChange = (val: string, rowIdx: number, col: string, isNew: boolean) => {
    if (isNew) {
      const updated = [...newRows];
      updated[rowIdx] = { ...updated[rowIdx], [col]: val };
      setNewRows(updated);
    } else {
      setModifiedRows(prev => ({
        ...prev,
        [rowIdx]: {
          ...(prev[rowIdx] || data[rowIdx]),
          [col]: val
        }
      }));
    }
  };

  const handleAddNewRow = () => {
    const emptyRow: any = {};
    columns.forEach((c: string) => { emptyRow[c] = ''; });
    setNewRows([...newRows, emptyRow]);
  };

  const handleCopyCell = async (rowIdx: number, col: string, isNew: boolean) => {
    let val;
    if (isNew) {
      val = newRows[rowIdx][col];
    } else {
      val = (modifiedRows[rowIdx] || data[rowIdx])[col];
    }
    
    try {
      await navigator.clipboard.writeText(String(val ?? ''));
      window.dispatchEvent(new CustomEvent('global-toast', { detail: { message: 'Cell value copied to clipboard', type: 'success' } }));
    } catch {
      window.dispatchEvent(new CustomEvent('global-toast', { detail: { message: 'Failed to copy cell value', type: 'error' } }));
    }
    setContextMenu(null);
  };

  const handleCopyRow = async (rowIdx: number, isNew: boolean) => {
    let rowData;
    if (isNew) {
      rowData = newRows[rowIdx];
    } else {
      rowData = modifiedRows[rowIdx] || data[rowIdx];
    }
    
    try {
      const tsvStr = columns.map((c: string) => {
        const val = rowData[c];
        if (val === null || val === undefined) return '';
        return String(val).replace(/\t/g, ' ').replace(/\n/g, ' ');
      }).join('\t');

      await navigator.clipboard.writeText(tsvStr);
      window.dispatchEvent(new CustomEvent('global-toast', { detail: { message: 'Row copied to clipboard (TSV)', type: 'success' } }));
    } catch {
      window.dispatchEvent(new CustomEvent('global-toast', { detail: { message: 'Failed to copy row TSV', type: 'error' } }));
    }
    setContextMenu(null);
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

  const handleSave = useCallback(async () => {
    try {
      // 1. Delete rows
      for (const idx of deletedRowIdxs) {
        const condition = getConditionForOriginalRow(idx);
        await api.crudDelete(tableName, condition, dbId);
      }
      
      // 2. Update rows
      for (const [idx, updatedRow] of Object.entries(modifiedRows)) {
        if (deletedRowIdxs.has(Number(idx))) continue;
        const condition = getConditionForOriginalRow(Number(idx));
        await api.crudUpdate(tableName, updatedRow, condition, dbId);
      }

      // 3. Insert new rows
      for (const row of newRows) {
        await api.crudInsert(tableName, row, dbId);
      }

    } catch (e: any) {
      window.dispatchEvent(new CustomEvent('global-toast', { detail: { message: redactSensitiveText("Error saving changes: " + (e.response?.data?.message || e.message || '')), type: 'error' } }));
    } finally {
      // Reset state and refresh
      setModifiedRows({});
      setDeletedRowIdxs(new Set());
      setNewRows([]);
      onRefresh();
    }
  }, [deletedRowIdxs, modifiedRows, newRows, tableName, dbId, onRefresh, getConditionForOriginalRow]);

  const handleUndo = () => {
    setModifiedRows({});
    setDeletedRowIdxs(new Set());
    setNewRows([]);
    setEditingCell(null);
  };

  const isDirty = Object.keys(modifiedRows).length > 0 || deletedRowIdxs.size > 0 || newRows.length > 0;

  useEffect(() => {
    const handleGlobalSave = () => {
      if (isActive && isDirty) {
        handleSave();
      }
    };
    window.addEventListener('global-save', handleGlobalSave);
    return () => window.removeEventListener('global-save', handleGlobalSave);
  }, [isActive, isDirty, modifiedRows, deletedRowIdxs, newRows, handleSave]);

  const downloadSql = () => {
    if (!data || data.length === 0) return;
    const headers = Object.keys(data[0]);
    const sqlContent = data.map((row: any) => {
      const values = headers.map(h => {
        const val = row[h];
        if (val === null || val === undefined) return 'NULL';
        if (typeof val === 'number' || typeof val === 'boolean') return String(val);
        return `'${String(val).replace(/'/g, "''")}'`;
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
          val = String(val).replace(/"/g, '""');
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
    estimateSize: () => 33, // Approx height of a row
    overscan: 10,
  });

  return (
    <div className="flex flex-col h-full relative">
      {isDirty && (
        <div className="flex items-center gap-2 mb-4 bg-blue-900/20 p-2 rounded border border-blue-500/30">
          <span className="text-sm text-blue-300 flex-1">You have unsaved changes.</span>
          <button onClick={handleUndo} className="flex items-center gap-1 px-3 py-1 bg-gray-700 hover:bg-gray-600 rounded text-sm text-white transition-colors">
            <Undo className="w-4 h-4" /> Undo
          </button>
          <button onClick={handleSave} className="flex items-center gap-1 px-3 py-1 bg-blue-600 hover:bg-blue-500 rounded text-sm text-white transition-colors">
            <Save className="w-4 h-4" /> Save
          </button>
        </div>
      )}

      <div ref={parentRef} className="flex-1 overflow-auto rounded border border-dark-border bg-[#0d1117]">
        <table className="w-full text-left text-sm whitespace-nowrap">
          <thead className="bg-[#161b22] sticky top-0 shadow-sm text-gray-400 text-xs tracking-wider z-20">
            <tr>
              {columns.map((k: string) => {
                const sortItem = sorts.find(s => s.column === k);
                const hasFilter = filters.some(f => f.column === k);
                
                return (
                  <th 
                    key={k} 
                    className="py-2 px-3 font-medium border-r border-[#30363d] relative select-none"
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
                <td colSpan={columns.length} style={{ height: `${rowVirtualizer.getVirtualItems()[0].start}px` }} />
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
                  className={`hover:bg-[#161b22] even:bg-[#0d1117] ${isModifiedRow ? 'bg-blue-900/10' : ''}`}
                >
                  {columns.map((k: string) => (
                    <td 
                      key={k} 
                      className={`py-1 px-3 border-r border-[#30363d]/50 max-w-[300px] truncate ${isModifiedRow && modifiedRows[i][k] !== row[k] ? 'bg-blue-500/20' : ''}`}
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
                          className="w-full bg-black text-white px-1 border border-blue-500 rounded outline-none"
                          value={displayRow[k] === null ? '' : displayRow[k]}
                          onChange={(e) => handleCellChange(e.target.value, i, k, false)}
                          onBlur={() => setEditingCell(null)}
                          onKeyDown={(e) => { if (e.key === 'Enter') setEditingCell(null); }}
                        />
                      ) : (
                        <CellDisplay val={displayRow[k]} />
                      )}
                    </td>
                  ))}
                </tr>
              );
            })}
            {rowVirtualizer.getVirtualItems().length > 0 && 
             rowVirtualizer.getTotalSize() - rowVirtualizer.getVirtualItems()[rowVirtualizer.getVirtualItems().length - 1].end > 0 && (
              <tr>
                <td colSpan={columns.length} style={{ height: `${rowVirtualizer.getTotalSize() - rowVirtualizer.getVirtualItems()[rowVirtualizer.getVirtualItems().length - 1].end}px` }} />
              </tr>
            )}
            
            {newRows.map((row: any, i: number) => (
              <tr 
                key={`new-${i}`} 
                className="bg-green-900/10 hover:bg-green-900/20"
              >
                {columns.map((k: string) => (
                  <td 
                    key={k} 
                    className="py-1 px-3 border-r border-green-500/30 max-w-[300px] truncate"
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
                        className="w-full bg-black text-white px-1 border border-green-500 rounded outline-none"
                        value={row[k] || ''}
                        onChange={(e) => handleCellChange(e.target.value, i, k, true)}
                        onBlur={() => setEditingCell(null)}
                        onKeyDown={(e) => { if (e.key === 'Enter') setEditingCell(null); }}
                      />
                    ) : (
                      <CellDisplay val={row[k]} isNewEmpty={row[k] === '' || row[k] === undefined} />
                    )}
                  </td>
                ))}
              </tr>
            ))}
          </tbody>
        </table>
        
        <div className="p-3 border-t border-[#30363d] flex justify-between items-center">
          <button 
            onClick={handleAddNewRow}
            className="flex items-center gap-1 text-sm text-gray-400 hover:text-white px-3 py-1.5 rounded hover:bg-[#21262d] transition-colors"
          >
            <Plus className="w-4 h-4" /> Add Row
          </button>
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

      {contextMenu && (
        <div 
          className="fixed bg-[#1c2128] border border-[#30363d] shadow-xl rounded overflow-hidden z-50 min-w-[150px]"
          style={{ top: contextMenu.y, left: contextMenu.x }}
        >
          {contextMenu.col && (
            <div 
              className="px-4 py-2 text-sm text-gray-300 hover:bg-[#30363d] cursor-pointer flex items-center gap-2"
              onClick={() => handleCopyCell(contextMenu.rowIdx, contextMenu.col!, contextMenu.isNew)}
            >
              <Copy className="w-4 h-4" /> Copy Cell Value
            </div>
          )}
          <div 
            className="px-4 py-2 text-sm text-gray-300 hover:bg-[#30363d] cursor-pointer flex items-center gap-2"
            onClick={() => handleCopyRow(contextMenu.rowIdx, contextMenu.isNew)}
          >
            <Copy className="w-4 h-4" /> Copy Row (Excel/TSV)
          </div>
          <div className="h-px bg-[#30363d] my-1" />
          <div 
            className="px-4 py-2 text-sm text-red-400 hover:bg-red-500/20 cursor-pointer flex items-center gap-2"
            onClick={handleDeleteRow}
          >
            <Trash2 className="w-4 h-4" /> Delete Row
          </div>
        </div>
      )}
    </div>
  );
}

function CellDisplay({ val, isNewEmpty = false }: { val: any, isNewEmpty?: boolean }) {
  if (isNewEmpty) return <span className="text-gray-600 italic">Empty</span>;
  if (val === null) return <span className="text-gray-600 italic">NULL</span>;
  if (typeof val === 'boolean') {
    return (
      <span className={`px-1.5 py-0.5 rounded text-[10px] font-bold uppercase ${val ? 'bg-green-500/20 text-green-400' : 'bg-red-500/20 text-red-400'}`}>
        {val ? 'TRUE' : 'FALSE'}
      </span>
    );
  }
  return <span className="text-gray-300">{String(val)}</span>;
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

  const apply = () => {
    const newFilters = filters.filter(f => f.column !== col);
    if (value.trim()) {
      newFilters.push({ column: col, operator, value: value.trim() });
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
        <option value="contains">Contains</option>
        <option value="greater_than">Greater Than</option>
        <option value="less_than">Less Than</option>
      </select>
      <input 
        type="text" 
        value={value} 
        onChange={e => setValue(e.target.value)}
        placeholder="Value..."
        className="w-full bg-[#0d1117] border border-[#30363d] rounded p-1 mb-3 text-sm text-gray-200 outline-none focus:border-blue-500"
        onKeyDown={e => { if (e.key === 'Enter') apply(); }}
      />
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
