import { useState } from 'react'
import { Table, Key, AlignLeft } from 'lucide-react'
import { api } from '../api'
import { sanitizeForLog } from '../utils'

export function SchemaView({ table, dbName, onInsertText }: { table: any, dbName: string, onInsertText?: (text: string) => void }) {
  const [activeTab, setActiveTab] = useState<'columns' | 'indexes' | 'data'>('columns')
  const [dataRows, setDataRows] = useState<any[]>([])
  const [isLoading, setIsLoading] = useState(false)

  const handleLoadData = async () => {
    setIsLoading(true)
    try {
      const res = await api.executeSql(`SELECT * FROM \`${table.table_name}\` LIMIT 1000;`)
      setDataRows(res.rows || [])
      setActiveTab('data')
    } catch (e) {
      console.error(sanitizeForLog(e))
    } finally {
      setIsLoading(false)
    }
  }

  if (!table) return null;

  return (
    <div className="flex flex-col h-full bg-[#0a0c10] text-gray-300">
      {/* Header Info */}
      <div className="p-4 border-b border-dark-border bg-dark-panel flex items-center gap-3">
        <div className="p-2 bg-dark-bg rounded-md border border-dark-border">
          <Table className="w-5 h-5 text-blue-400" />
        </div>
        <div>
          <h2 className="text-lg font-semibold text-white tracking-wide">{table.table_name}</h2>
          <p className="text-xs text-gray-500">Database: <span className="text-gray-400">{dbName || 'Unknown'}</span></p>
        </div>
      </div>

      {/* Tabs */}
      <div className="flex px-4 border-b border-dark-border bg-dark-panel pt-2 gap-4">
        <button 
          onClick={() => setActiveTab('columns')}
          className={`pb-2 text-sm font-medium border-b-2 transition-colors ${activeTab === 'columns' ? 'border-dark-accent text-white' : 'border-transparent text-gray-500 hover:text-gray-300'}`}
        >
          Columns ({table.columns?.length || 0})
        </button>
        <button 
          onClick={() => setActiveTab('indexes')}
          className={`pb-2 text-sm font-medium border-b-2 transition-colors ${activeTab === 'indexes' ? 'border-dark-accent text-white' : 'border-transparent text-gray-500 hover:text-gray-300'}`}
        >
          Indexes ({table.indexes?.length || 0})
        </button>
        <button 
          onClick={() => {
             setActiveTab('data')
             if (dataRows.length === 0) handleLoadData()
          }}
          className={`pb-2 text-sm font-medium border-b-2 transition-colors ${activeTab === 'data' ? 'border-dark-accent text-white' : 'border-transparent text-gray-500 hover:text-gray-300'}`}
        >
          Data
        </button>
      </div>

      {/* Content Area */}
      <div className="flex-1 overflow-auto bg-[#0a0c10]">
        {activeTab === 'columns' && (
          <table className="w-full text-left text-sm">
            <thead className="bg-dark-panel sticky top-0 shadow-sm text-gray-400 text-xs uppercase tracking-wider">
              <tr>
                <th className="py-2.5 px-4 font-medium">Name</th>
                <th className="py-2.5 px-4 font-medium">Type</th>
                <th className="py-2.5 px-4 font-medium">Nullable</th>
                <th className="py-2.5 px-4 font-medium">Key</th>
                <th className="py-2.5 px-4 font-medium">Comment</th>
              </tr>
            </thead>
            <tbody className="divide-y divide-dark-border/50">
              {table.columns?.map((col: any, idx: number) => (
                <tr key={idx} className="hover:bg-[#161b22] transition-colors even:bg-[#0d1117] group">
                  <td className="py-2.5 px-4 font-medium text-blue-300 flex items-center justify-between">
                    <div className="flex items-center gap-2">
                      {col.column_key === 'PRI' && <Key className="w-3.5 h-3.5 text-yellow-500" />}
                      {col.column_name}
                    </div>
                    {onInsertText && (
                      <button 
                        onClick={(e) => {
                          e.stopPropagation();
                          onInsertText(col.column_name);
                        }}
                        className="opacity-0 group-hover:opacity-100 p-1 hover:bg-blue-500/20 rounded text-gray-400 hover:text-blue-400 transition-all"
                        title="Insert column name into editor"
                      >
                        <AlignLeft className="w-3.5 h-3.5" />
                      </button>
                    )}
                  </td>
                  <td className="py-2.5 px-4 text-purple-300">{col.data_type}</td>
                  <td className="py-2.5 px-4 text-gray-400">{col.is_nullable}</td>
                  <td className="py-2.5 px-4 text-gray-400">{col.column_key}</td>
                  <td className="py-2.5 px-4 text-gray-500">{col.column_comment}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}

        {activeTab === 'indexes' && (
          <table className="w-full text-left text-sm">
            <thead className="bg-dark-panel sticky top-0 shadow-sm text-gray-400 text-xs uppercase tracking-wider">
              <tr>
                <th className="py-2.5 px-4 font-medium">Index Name</th>
                <th className="py-2.5 px-4 font-medium">Column</th>
                <th className="py-2.5 px-4 font-medium">Unique</th>
                <th className="py-2.5 px-4 font-medium">Type</th>
              </tr>
            </thead>
            <tbody className="divide-y divide-dark-border/50">
              {table.indexes?.map((idx: any, i: number) => (
                <tr key={i} className="hover:bg-[#161b22] transition-colors even:bg-[#0d1117]">
                  <td className="py-2.5 px-4 font-medium text-gray-200">{idx.index_name}</td>
                  <td className="py-2.5 px-4 text-blue-300">{idx.column_name}</td>
                  <td className="py-2.5 px-4">
                    <span className={`px-2 py-0.5 rounded text-xs ${!idx.non_unique ? 'bg-yellow-500/20 text-yellow-400' : 'bg-gray-700 text-gray-300'}`}>
                      {!idx.non_unique ? 'YES' : 'NO'}
                    </span>
                  </td>
                  <td className="py-2.5 px-4 text-gray-400">{idx.index_type}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}

        {activeTab === 'data' && (
          <div className="h-full">
            {isLoading ? (
              <div className="flex justify-center items-center h-full text-gray-500">Loading data...</div>
            ) : dataRows.length > 0 ? (
              <div className="overflow-auto w-full h-full">
                <table className="w-full text-left text-sm whitespace-nowrap">
                  <thead className="bg-dark-panel sticky top-0 shadow-sm text-gray-400 text-xs uppercase tracking-wider">
                    <tr>
                      {Object.keys(dataRows[0] || {}).map(k => (
                        <th key={k} className="py-2.5 px-4 font-medium border-r border-dark-border/50">{k}</th>
                      ))}
                    </tr>
                  </thead>
                  <tbody className="divide-y divide-dark-border/50">
                    {dataRows.map((row, i) => (
                      <tr key={i} className="hover:bg-[#161b22] even:bg-[#0d1117]">
                        {Object.values(row).map((val: any, j) => (
                          <td key={j} className="py-1.5 px-4 text-gray-300 border-r border-dark-border/50 max-w-[300px] truncate">
                            {val === null ? <span className="text-gray-600 italic">NULL</span> : String(val)}
                          </td>
                        ))}
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            ) : (
              <div className="flex justify-center items-center h-full text-gray-500">
                No data found in table.
              </div>
            )}
          </div>
        )}
      </div>
    </div>
  )
}
