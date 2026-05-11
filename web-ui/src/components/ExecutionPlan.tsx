import { useEffect, useState } from 'react';
import { api } from '../api';
import { Database, Zap, Key, Search, FileText, Copy, RefreshCw, Table2 } from 'lucide-react';

interface ExecutionPlanProps {
  sql: string;
}

export function ExecutionPlan({ sql }: ExecutionPlanProps) {
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [planRows, setPlanRows] = useState<any[]>([]);
  const [refreshToken, setRefreshToken] = useState(0);
  const [viewMode, setViewMode] = useState<'visual' | 'table'>('visual');

  useEffect(() => {
    const fetchPlan = async () => {
      setLoading(true);
      setError(null);
      try {
        const res = await api.explainSql(sql);
        setPlanRows(res.rows || []);
      } catch (err: any) {
        setError(err?.response?.data?.message || err.message || 'Failed to explain SQL');
      } finally {
        setLoading(false);
      }
    };

    if (sql) {
      fetchPlan();
    }
  }, [sql, refreshToken]);

  const handleCopySql = async () => {
    try {
      await navigator.clipboard.writeText(sql);
      window.dispatchEvent(new CustomEvent('global-toast', { detail: { message: 'Explain SQL copied to clipboard', type: 'success' } }));
    } catch {
      window.dispatchEvent(new CustomEvent('global-toast', { detail: { message: 'Failed to copy explain SQL', type: 'error' } }));
    }
  };

  const planColumns = planRows.length > 0 ? Object.keys(planRows[0]) : [];

  if (loading) {
    return <div className="p-8 text-center text-blue-400 animate-pulse">Analyzing Execution Plan...</div>;
  }

  if (error) {
    return <div className="p-4 m-4 text-red-400 bg-red-500/10 border border-red-500/20 rounded-lg">{error}</div>;
  }

  if (!planRows.length) {
    return <div className="p-8 text-center text-gray-500">No execution plan available.</div>;
  }

  return (
    <div className="flex flex-col h-full bg-[#0d1117] text-sm overflow-hidden">
      <div className="p-4 border-b border-[#30363d] bg-[#161b22] shrink-0">
        <div className="flex items-center justify-between gap-3 mb-2">
          <div>
            <h3 className="font-semibold text-gray-200 flex items-center gap-2">
              <Zap className="w-4 h-4 text-blue-400" />
              Visual Execution Plan
            </h3>
            <div className="mt-1 flex items-center gap-2 flex-wrap text-[11px] text-gray-400">
              <span className="px-1.5 py-0.5 rounded bg-[#0d1117] border border-[#30363d]">{planRows.length} steps</span>
              <span className="px-1.5 py-0.5 rounded bg-[#0d1117] border border-[#30363d]">{viewMode === 'visual' ? 'Visual view' : 'Table view'}</span>
            </div>
          </div>
          <div className="flex items-center gap-2">
            <div className="flex items-center bg-[#21262d] rounded overflow-hidden border border-[#30363d]">
              <button
                onClick={() => setViewMode('visual')}
                className={`px-2 py-1 text-xs transition-colors ${viewMode === 'visual' ? 'bg-blue-500/20 text-blue-400' : 'text-gray-400 hover:bg-[#30363d]'}`}
              >
                Visual
              </button>
              <button
                onClick={() => setViewMode('table')}
                className={`px-2 py-1 text-xs transition-colors flex items-center gap-1 ${viewMode === 'table' ? 'bg-blue-500/20 text-blue-400' : 'text-gray-400 hover:bg-[#30363d]'}`}
              >
                <Table2 className="w-3 h-3" />
                Table
              </button>
            </div>
            <button
              onClick={() => setRefreshToken((value) => value + 1)}
              className="px-2 py-1 text-xs rounded border border-[#30363d] text-gray-300 hover:text-white hover:bg-[#21262d] transition-colors flex items-center gap-1"
            >
              <RefreshCw className="w-3 h-3" />
              Refresh
            </button>
            <button
              onClick={handleCopySql}
              className="px-2 py-1 text-xs rounded border border-[#30363d] text-gray-300 hover:text-white hover:bg-[#21262d] transition-colors flex items-center gap-1"
            >
              <Copy className="w-3 h-3" />
              Copy SQL
            </button>
          </div>
        </div>
        <pre className="text-xs text-gray-400 bg-[#0d1117] p-3 rounded-lg border border-[#30363d] overflow-x-auto">
          {sql}
        </pre>
      </div>
      
      <div className="flex-1 overflow-auto p-8 bg-dark-bg">
        {viewMode === 'visual' ? (
          <div className="flex flex-col items-center gap-6 max-w-4xl mx-auto pb-10">
            {planRows.map((row, i) => (
              <div key={i} className="flex flex-col items-center w-full relative">
                {i > 0 && (
                  <div className="w-0.5 h-6 bg-gradient-to-b from-blue-500/50 to-[#30363d] mb-6"></div>
                )}
                <div className="w-full bg-[#161b22] rounded-xl border border-[#30363d] p-5 shadow-lg hover:border-blue-500/30 transition-colors relative overflow-hidden group">
                  <div className="absolute top-0 left-0 w-1 h-full bg-blue-500/50 group-hover:bg-blue-400 transition-colors"></div>
                  
                  <div className="flex items-start justify-between mb-4">
                    <div className="flex items-center gap-3">
                      <div className="p-2 bg-[#0d1117] rounded-lg border border-[#30363d]">
                        <Database className="w-5 h-5 text-blue-400" />
                      </div>
                      <div>
                        <h4 className="text-lg font-bold text-gray-200">
                          {row.table || row.TABLE || 'Derived/Temp Table'}
                        </h4>
                        <span className="text-xs text-blue-400 font-medium px-2 py-0.5 bg-blue-500/10 rounded-full mt-1 inline-block">
                          {row.select_type || row.SELECT_TYPE || 'SIMPLE'}
                        </span>
                      </div>
                    </div>
                    <div className="text-right">
                      <div className="text-sm font-mono text-gray-400">ID: {row.id || row.ID || i+1}</div>
                      <div className="text-xs text-gray-500 mt-1">Est. Rows: <span className="text-gray-300 font-bold">{row.rows || row.ROWS || 0}</span></div>
                    </div>
                  </div>

                  <div className="grid grid-cols-3 gap-4 mt-4 bg-[#0d1117] p-4 rounded-lg border border-[#30363d]">
                    <div className="flex flex-col gap-1">
                      <span className="text-xs text-gray-500 uppercase tracking-wider flex items-center gap-1"><Search className="w-3 h-3" /> Access Type</span>
                      <span className="text-sm font-medium text-purple-400">{row.type || row.TYPE || 'ALL'}</span>
                    </div>
                    <div className="flex flex-col gap-1">
                      <span className="text-xs text-gray-500 uppercase tracking-wider flex items-center gap-1"><Key className="w-3 h-3" /> Used Key</span>
                      <span className="text-sm font-medium text-green-400">{row.key || row.KEY || 'None'}</span>
                    </div>
                    <div className="flex flex-col gap-1">
                      <span className="text-xs text-gray-500 uppercase tracking-wider flex items-center gap-1"><FileText className="w-3 h-3" /> Extra Info</span>
                      <span className="text-sm text-gray-300 truncate" title={row.Extra || row.EXTRA || '-'}>
                        {row.Extra || row.EXTRA || '-'}
                      </span>
                    </div>
                  </div>
                </div>
              </div>
            ))}
            <div className="w-0.5 h-6 bg-gradient-to-b from-[#30363d] to-transparent"></div>
            <div className="px-4 py-2 bg-[#161b22] border border-[#30363d] rounded-full text-sm text-gray-400 shadow-lg">
              Query Result
            </div>
          </div>
        ) : (
          <div className="overflow-auto rounded-lg border border-[#30363d] bg-[#161b22]">
            <table className="w-full text-left text-xs whitespace-nowrap">
              <thead className="bg-[#0d1117] text-gray-400">
                <tr>
                  {planColumns.map((column) => (
                    <th key={column} className="px-3 py-2 border-b border-[#30363d] font-medium">
                      {column}
                    </th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {planRows.map((row, rowIndex) => (
                  <tr key={rowIndex} className="border-b border-[#30363d]/50 hover:bg-[#0d1117]">
                    {planColumns.map((column) => (
                      <td key={`${rowIndex}-${column}`} className="px-3 py-2 text-gray-300 align-top">
                        {String(row[column] ?? '')}
                      </td>
                    ))}
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>
    </div>
  );
}
