import { useState, useMemo, useEffect } from 'react';
import { ArrowDown, ArrowUp, ArrowUpDown, ChevronLeft, ChevronRight } from 'lucide-react';
import { tr } from '../i18n';

interface SimpleDataTableProps {
  data: any[];
}

export function SimpleDataTable({ data }: SimpleDataTableProps) {
  const [sortConfig, setSortConfig] = useState<{ key: string; direction: 'asc' | 'desc' } | null>(null);
  const [currentPage, setCurrentPage] = useState(1);
  const [contextMenu, setContextMenu] = useState<{ x: number, y: number, rowIdx: number, col: string, val: any } | null>(null);
  const pageSize = 100;

  useEffect(() => {
    const handleClick = () => setContextMenu(null);
    window.addEventListener('click', handleClick);
    return () => window.removeEventListener('click', handleClick);
  }, []);

  const columns = useMemo(() => {
    if (!data || data.length === 0) return [];
    return Object.keys(data[0]);
  }, [data]);

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

  if (!data || data.length === 0) return null;

  return (
    <div className="flex flex-col w-full h-full overflow-hidden">
      <div className="flex-1 overflow-auto">
        <table className="w-full text-left text-sm whitespace-nowrap">
          <thead className="bg-[#0d1117] sticky top-0 shadow-sm text-gray-400 text-xs tracking-wider z-10">
            <tr>
              {columns.map(k => (
                <th 
                  key={k} 
                  className="py-2.5 px-4 font-medium border-r border-[#30363d]/50 cursor-pointer hover:bg-[#21262d] transition-colors group select-none"
                  onClick={() => requestSort(k)}
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
                </th>
              ))}
            </tr>
          </thead>
          <tbody className="divide-y divide-[#30363d]/50">
            {paginatedData.map((row: any, i: number) => (
              <tr key={i} className="hover:bg-[#161b22] even:bg-[#0d1117]">
                {columns.map((k) => {
                  const val = row[k];
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
                    >
                      {val === null ? (
                        <span className="text-gray-600 italic">NULL</span>
                      ) : typeof val === 'boolean' ? (
                        <span className={`px-1.5 py-0.5 rounded text-[10px] font-bold uppercase ${val ? 'bg-green-500/20 text-green-400' : 'bg-red-500/20 text-red-400'}`}>
                          {val ? 'TRUE' : 'FALSE'}
                        </span>
                      ) : (
                        String(val)
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
            className="w-full text-left px-4 py-2 hover:bg-[#21262d] transition-colors"
            onClick={async () => {
              try {
                await navigator.clipboard.writeText(String(contextMenu.val));
                window.dispatchEvent(new CustomEvent('global-toast', { detail: { message: tr('单元格内容已复制', 'Cell value copied to clipboard'), type: 'success' } }));
              } catch {
                window.dispatchEvent(new CustomEvent('global-toast', { detail: { message: tr('复制单元格内容失败', 'Failed to copy cell value'), type: 'error' } }));
              }
              setContextMenu(null);
            }}
          >
            {tr('复制单元格', 'Copy Cell')}
          </button>
          <button 
            className="w-full text-left px-4 py-2 hover:bg-[#21262d] transition-colors"
            onClick={async () => {
              const row = paginatedData[contextMenu.rowIdx];
              const rowString = columns.map(col => {
                const v = row[col];
                if (v === null || v === undefined) return '';
                if (typeof v === 'object') return JSON.stringify(v);
                return String(v).replace(/\t/g, ' ').replace(/\n/g, ' ');
              }).join('\t');
              try {
                await navigator.clipboard.writeText(rowString);
                window.dispatchEvent(new CustomEvent('global-toast', { detail: { message: tr('行已复制（TSV）', 'Row copied to clipboard (TSV)'), type: 'success' } }));
              } catch {
                window.dispatchEvent(new CustomEvent('global-toast', { detail: { message: tr('复制行失败', 'Failed to copy row'), type: 'error' } }));
              }
              setContextMenu(null);
            }}
          >
            {tr('复制行（Excel/TSV）', 'Copy Row (Excel/TSV)')}
          </button>
        </div>
      )}
    </div>
  );
}
