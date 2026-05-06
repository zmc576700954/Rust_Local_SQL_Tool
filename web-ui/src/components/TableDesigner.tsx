import { useState, useEffect, useCallback } from 'react'
import { api } from '../api'
import { Save, Plus, Trash2, Sparkles } from 'lucide-react'
import { useToast } from './Toast'

import type { TableWithDetails, ColumnInfo, IndexInfo, ForeignKeyInfo } from '../types';

export function TableDesigner({ tableName, isActive }: { tableName: string, isActive: boolean }) {
  const [loading, setLoading] = useState(false)
  const [activeTab, setActiveTab] = useState<'columns' | 'indexes' | 'foreignKeys'>('columns')
  const [originalSchema, setOriginalSchema] = useState<TableWithDetails | null>(null)
  
  const [columns, setColumns] = useState<ColumnInfo[]>([])
  const [indexes, setIndexes] = useState<IndexInfo[]>([])
  const [foreignKeys, setForeignKeys] = useState<ForeignKeyInfo[]>([])
  
  const [previewSql, setPreviewSql] = useState<string | null>(null)
  const [executing, setExecuting] = useState(false)
  const [isSuggesting, setIsSuggesting] = useState(false)

  const { toast } = useToast()

  const loadSchema = useCallback(async () => {
    setLoading(true)
    try {
      const res = await api.getTableSchema(tableName)
      setOriginalSchema(res)
      setColumns(res.columns || [])
      setIndexes(res.indexes || [])
      setForeignKeys(res.foreign_keys || [])
    } catch (e: unknown) {
      toast((e as Error).message || 'Error', 'error')
    } finally {
      setLoading(false)
    }
  }, [tableName, toast])

  useEffect(() => {
    loadSchema()
  }, [loadSchema])

  const handleSave = useCallback(async () => {
    try {
      if (!originalSchema) return
      const newSchema: TableWithDetails = {
        table_name: originalSchema.table_name,
        columns,
        indexes,
        foreign_keys: foreignKeys,
      }
      const res = await api.previewDdl(originalSchema, newSchema)
      setPreviewSql(res.sql)
    } catch (e: any) {
      toast(e.message || 'Failed to generate DDL', 'error')
    }
  }, [originalSchema, columns, indexes, foreignKeys, toast])

  const confirmSave = async () => {
    if (!previewSql) return
    setExecuting(true)
    try {
      await api.executeDdl(previewSql)
      toast('Table updated successfully', 'success')
      setPreviewSql(null)
      loadSchema()
    } catch (e: any) {
      toast(e.message || 'Failed to execute DDL', 'error')
    } finally {
      setExecuting(false)
    }
  }

  const updateColumn = (idx: number, field: string, val: unknown) => {
    const newCols = [...columns]
    newCols[idx] = { ...newCols[idx], [field]: val } as ColumnInfo
    setColumns(newCols)
  }

  const addColumn = () => {
    setColumns([...columns, {
      column_name: 'new_column',
      data_type: 'varchar',
      column_type: 'varchar(255)',
      is_nullable: 'YES',
      column_comment: '',
      column_key: '',
      column_default: null,
      extra: ''
    }])
  }

  const removeColumn = (idx: number) => {
    setColumns(columns.filter((_, i) => i !== idx))
  }

  const handleAISuggestIndexes = async () => {
    setIsSuggesting(true);
    try {
      const prompt = `Based on the following columns for table '${tableName}', please suggest some reasonable indexes. Output ONLY a valid JSON array of index objects. Each object should have:
- index_name: string
- column_name: string (comma separated if multiple)
- non_unique: boolean
- index_type: string (e.g. BTREE)
Do not output any markdown formatting, just the raw JSON array.
Columns: ${JSON.stringify(columns.map(c => ({ name: c.column_name, type: c.column_type, key: c.column_key })))}\n\nCurrent Indexes: ${JSON.stringify(indexes)}`;
      
      const res = await api.aiQuery(prompt);
      let jsonStr = res.sql || res.explanation || "[]";
      
      if (jsonStr.includes("```json")) {
        jsonStr = jsonStr.split("```json")[1].split("```")[0].trim();
      } else if (jsonStr.includes("```")) {
        jsonStr = jsonStr.split("```")[1].split("```")[0].trim();
      }
      
      const suggestedIndexes = JSON.parse(jsonStr);
      if (Array.isArray(suggestedIndexes) && suggestedIndexes.length > 0) {
        setIndexes([...indexes, ...suggestedIndexes]);
        toast(`AI 建议了 ${suggestedIndexes.length} 个索引`, "success");
      } else {
        toast("AI 认为当前索引已经足够，没有新建议", "info");
      }
    } catch (e: any) {
      toast("AI 建议索引失败: " + (e.message || "Failed to parse AI response"), "error");
    } finally {
      setIsSuggesting(false);
    }
  };

  const addIndex = () => {
    setIndexes([...indexes, {
      index_name: 'new_idx',
      column_name: columns[0]?.column_name || '',
      non_unique: 1,
      index_type: 'BTREE'
    }])
  }

  const updateIndex = (idx: number, field: string, val: unknown) => {
    const newIdxs = [...indexes]
    newIdxs[idx] = { ...newIdxs[idx], [field]: val } as IndexInfo
    setIndexes(newIdxs)
  }

  const removeIndex = (idx: number) => {
    setIndexes(indexes.filter((_, i) => i !== idx))
  }

  const addForeignKey = () => {
    setForeignKeys([...foreignKeys, {
      constraint_name: 'new_fk',
      column_name: columns[0]?.column_name || '',
      referenced_table_name: '',
      referenced_column_name: '',
      update_rule: 'NO ACTION',
      delete_rule: 'NO ACTION'
    }])
  }

  const updateForeignKey = (idx: number, field: string, val: unknown) => {
    const newFks = [...foreignKeys]
    newFks[idx] = { ...newFks[idx], [field]: val } as ForeignKeyInfo
    setForeignKeys(newFks)
  }

  const removeForeignKey = (idx: number) => {
    setForeignKeys(foreignKeys.filter((_, i) => i !== idx))
  }

  useEffect(() => {
    const handleGlobalSave = () => {
      if (isActive) {
        handleSave();
      }
    };
    window.addEventListener('global-save', handleGlobalSave);
    return () => window.removeEventListener('global-save', handleGlobalSave);
  }, [isActive, handleSave]);

  if (loading) return <div className="p-4 text-gray-400">Loading schema...</div>

  return (
    <div className="flex flex-col h-full bg-[#0a0c10]">
      <div className="flex items-center justify-between p-4 border-b border-[#30363d] bg-[#161b22]">
        <div className="flex items-center gap-4">
          <h2 className="text-lg font-semibold text-gray-200">Table: {tableName}</h2>
          <div className="flex gap-2">
            <button 
              onClick={() => setActiveTab('columns')}
              className={`px-3 py-1 rounded text-sm ${activeTab === 'columns' ? 'bg-[#30363d] text-white' : 'text-gray-400 hover:text-white'}`}
            >
              Columns
            </button>
            <button 
              onClick={() => setActiveTab('indexes')}
              className={`px-3 py-1 rounded text-sm ${activeTab === 'indexes' ? 'bg-[#30363d] text-white' : 'text-gray-400 hover:text-white'}`}
            >
              Indexes
            </button>
            <button 
              onClick={() => setActiveTab('foreignKeys')}
              className={`px-3 py-1 rounded text-sm ${activeTab === 'foreignKeys' ? 'bg-[#30363d] text-white' : 'text-gray-400 hover:text-white'}`}
            >
              Foreign Keys
            </button>
          </div>
        </div>
        <button 
          onClick={handleSave}
          className="flex items-center gap-2 bg-blue-600 hover:bg-blue-500 px-4 py-2 rounded text-sm text-white transition-colors"
        >
          <Save className="w-4 h-4" />
          Preview & Save
        </button>
      </div>

      <div className="flex-1 overflow-auto p-4">
        {activeTab === 'columns' && (
          <div>
            <div className="flex justify-between items-center mb-2">
              <h3 className="text-gray-300 font-medium">Columns</h3>
              <button onClick={addColumn} className="text-xs bg-[#21262d] hover:bg-[#30363d] text-gray-300 px-2 py-1 rounded flex items-center gap-1">
                <Plus className="w-3 h-3" /> Add Column
              </button>
            </div>
            <table className="w-full text-left text-sm text-gray-300 border-collapse">
              <thead>
                <tr className="border-b border-[#30363d] bg-[#21262d]">
                  <th className="p-2">Name</th>
                  <th className="p-2">Type (Length)</th>
                  <th className="p-2">PK</th>
                  <th className="p-2">Not Null</th>
                  <th className="p-2">Auto Inc</th>
                  <th className="p-2">Default</th>
                  <th className="p-2">Comment</th>
                  <th className="p-2 w-10"></th>
                </tr>
              </thead>
              <tbody>
                {columns.map((col: any, idx: number) => (
                  <tr key={idx} className="border-b border-[#30363d]/50 hover:bg-[#21262d]/50">
                    <td className="p-2">
                      <input 
                        type="text" 
                        className="bg-transparent border border-[#30363d] rounded px-1 py-0.5 w-full"
                        value={col.column_name} 
                        onChange={e => updateColumn(idx, 'column_name', e.target.value)} 
                      />
                    </td>
                    <td className="p-2">
                      <input 
                        type="text" 
                        className="bg-transparent border border-[#30363d] rounded px-1 py-0.5 w-full"
                        value={col.column_type} 
                        onChange={e => updateColumn(idx, 'column_type', e.target.value)} 
                      />
                    </td>
                    <td className="p-2 text-center">
                      <input 
                        type="checkbox" 
                        checked={col.column_key === 'PRI'} 
                        onChange={e => updateColumn(idx, 'column_key', e.target.checked ? 'PRI' : '')} 
                      />
                    </td>
                    <td className="p-2 text-center">
                      <input 
                        type="checkbox" 
                        checked={col.is_nullable === 'NO'} 
                        onChange={e => updateColumn(idx, 'is_nullable', e.target.checked ? 'NO' : 'YES')} 
                      />
                    </td>
                    <td className="p-2 text-center">
                      <input 
                        type="checkbox" 
                        checked={col.extra?.includes('auto_increment')} 
                        onChange={e => updateColumn(idx, 'extra', e.target.checked ? 'auto_increment' : '')} 
                      />
                    </td>
                    <td className="p-2">
                      <input 
                        type="text" 
                        className="bg-transparent border border-[#30363d] rounded px-1 py-0.5 w-full"
                        value={col.column_default || ''} 
                        onChange={e => updateColumn(idx, 'column_default', e.target.value || null)} 
                      />
                    </td>
                    <td className="p-2">
                      <input 
                        type="text" 
                        className="bg-transparent border border-[#30363d] rounded px-1 py-0.5 w-full"
                        value={col.column_comment || ''} 
                        onChange={e => updateColumn(idx, 'column_comment', e.target.value)} 
                      />
                    </td>
                    <td className="p-2">
                      <button onClick={() => removeColumn(idx)} className="text-red-500 hover:text-red-400">
                        <Trash2 className="w-4 h-4" />
                      </button>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}

        {activeTab === 'indexes' && (
          <div>
            <div className="flex justify-between items-center mb-2">
              <h3 className="text-gray-300 font-medium">Indexes</h3>
              <div className="flex gap-2">
                <button onClick={handleAISuggestIndexes} disabled={isSuggesting} className="text-xs bg-purple-600/20 hover:bg-purple-600/40 text-purple-400 border border-purple-500/30 px-2 py-1 rounded flex items-center gap-1 transition-colors disabled:opacity-50 disabled:cursor-not-allowed">
                  <Sparkles className="w-3 h-3" /> {isSuggesting ? 'Thinking...' : 'AI Suggest Indexes'}
                </button>
                <button onClick={addIndex} className="text-xs bg-[#21262d] hover:bg-[#30363d] text-gray-300 px-2 py-1 rounded flex items-center gap-1">
                  <Plus className="w-3 h-3" /> Add Index
                </button>
              </div>
            </div>
            <table className="w-full text-left text-sm text-gray-300 border-collapse">
              <thead>
                <tr className="border-b border-[#30363d] bg-[#21262d]">
                  <th className="p-2">Index Name</th>
                  <th className="p-2">Column Name</th>
                  <th className="p-2">Unique</th>
                  <th className="p-2">Index Type</th>
                  <th className="p-2 w-10"></th>
                </tr>
              </thead>
              <tbody>
                {indexes.map((idxItem: any, idx: number) => (
                  <tr key={idx} className="border-b border-[#30363d]/50 hover:bg-[#21262d]/50">
                    <td className="p-2">
                      <input 
                        type="text" 
                        className="bg-transparent border border-[#30363d] rounded px-1 py-0.5 w-full"
                        value={idxItem.index_name} 
                        onChange={e => updateIndex(idx, 'index_name', e.target.value)} 
                      />
                    </td>
                    <td className="p-2">
                      <input 
                        type="text" 
                        className="bg-transparent border border-[#30363d] rounded px-1 py-0.5 w-full"
                        value={idxItem.column_name} 
                        onChange={e => updateIndex(idx, 'column_name', e.target.value)} 
                      />
                    </td>
                    <td className="p-2 text-center">
                      <input 
                        type="checkbox" 
                        checked={!idxItem.non_unique} 
                        onChange={e => updateIndex(idx, 'non_unique', !e.target.checked)} 
                      />
                    </td>
                    <td className="p-2">
                      <input 
                        type="text" 
                        className="bg-transparent border border-[#30363d] rounded px-1 py-0.5 w-full"
                        value={idxItem.index_type} 
                        onChange={e => updateIndex(idx, 'index_type', e.target.value)} 
                      />
                    </td>
                    <td className="p-2">
                      <button onClick={() => removeIndex(idx)} className="text-red-500 hover:text-red-400">
                        <Trash2 className="w-4 h-4" />
                      </button>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}

        {activeTab === 'foreignKeys' && (
          <div>
            <div className="flex justify-between items-center mb-2">
              <h3 className="text-gray-300 font-medium">Foreign Keys</h3>
              <button onClick={addForeignKey} className="text-xs bg-[#21262d] hover:bg-[#30363d] text-gray-300 px-2 py-1 rounded flex items-center gap-1">
                <Plus className="w-3 h-3" /> Add Foreign Key
              </button>
            </div>
            <table className="w-full text-left text-sm text-gray-300 border-collapse">
              <thead>
                <tr className="border-b border-[#30363d] bg-[#21262d]">
                  <th className="p-2">Constraint Name</th>
                  <th className="p-2">Column</th>
                  <th className="p-2">Ref Table</th>
                  <th className="p-2">Ref Column</th>
                  <th className="p-2">On Delete</th>
                  <th className="p-2">On Update</th>
                  <th className="p-2 w-10"></th>
                </tr>
              </thead>
              <tbody>
                {foreignKeys.map((fk: any, idx: number) => (
                  <tr key={idx} className="border-b border-[#30363d]/50 hover:bg-[#21262d]/50">
                    <td className="p-2">
                      <input 
                        type="text" 
                        className="bg-transparent border border-[#30363d] rounded px-1 py-0.5 w-full"
                        value={fk.constraint_name} 
                        onChange={e => updateForeignKey(idx, 'constraint_name', e.target.value)} 
                      />
                    </td>
                    <td className="p-2">
                      <input 
                        type="text" 
                        className="bg-transparent border border-[#30363d] rounded px-1 py-0.5 w-full"
                        value={fk.column_name} 
                        onChange={e => updateForeignKey(idx, 'column_name', e.target.value)} 
                      />
                    </td>
                    <td className="p-2">
                      <input 
                        type="text" 
                        className="bg-transparent border border-[#30363d] rounded px-1 py-0.5 w-full"
                        value={fk.referenced_table_name} 
                        onChange={e => updateForeignKey(idx, 'referenced_table_name', e.target.value)} 
                      />
                    </td>
                    <td className="p-2">
                      <input 
                        type="text" 
                        className="bg-transparent border border-[#30363d] rounded px-1 py-0.5 w-full"
                        value={fk.referenced_column_name} 
                        onChange={e => updateForeignKey(idx, 'referenced_column_name', e.target.value)} 
                      />
                    </td>
                    <td className="p-2">
                      <select 
                        className="bg-transparent border border-[#30363d] rounded px-1 py-0.5 w-full"
                        value={fk.delete_rule} 
                        onChange={e => updateForeignKey(idx, 'delete_rule', e.target.value)}
                      >
                        <option value="NO ACTION">NO ACTION</option>
                        <option value="CASCADE">CASCADE</option>
                        <option value="SET NULL">SET NULL</option>
                        <option value="RESTRICT">RESTRICT</option>
                      </select>
                    </td>
                    <td className="p-2">
                      <select 
                        className="bg-transparent border border-[#30363d] rounded px-1 py-0.5 w-full"
                        value={fk.update_rule} 
                        onChange={e => updateForeignKey(idx, 'update_rule', e.target.value)}
                      >
                        <option value="NO ACTION">NO ACTION</option>
                        <option value="CASCADE">CASCADE</option>
                        <option value="SET NULL">SET NULL</option>
                        <option value="RESTRICT">RESTRICT</option>
                      </select>
                    </td>
                    <td className="p-2">
                      <button onClick={() => removeForeignKey(idx)} className="text-red-500 hover:text-red-400">
                        <Trash2 className="w-4 h-4" />
                      </button>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>

      {previewSql !== null && (
        <div className="fixed inset-0 bg-black/50 flex items-center justify-center z-50">
          <div className="bg-[#161b22] border border-[#30363d] rounded-lg shadow-xl w-[600px] flex flex-col max-h-[80vh]">
            <div className="p-4 border-b border-[#30363d] flex justify-between items-center">
              <h3 className="text-lg font-semibold text-gray-200">Preview DDL</h3>
              <button onClick={() => setPreviewSql(null)} className="text-gray-400 hover:text-white">&times;</button>
            </div>
            <div className="p-4 overflow-auto flex-1">
              <pre className="bg-[#0d1117] p-4 rounded text-sm text-green-400 whitespace-pre-wrap font-mono">
                {previewSql}
              </pre>
            </div>
            <div className="p-4 border-t border-[#30363d] flex justify-end gap-2">
              <button 
                onClick={() => setPreviewSql(null)}
                className="px-4 py-2 rounded text-sm text-gray-300 hover:bg-[#30363d] transition-colors"
              >
                Cancel
              </button>
              <button 
                onClick={confirmSave}
                disabled={executing || previewSql.includes('No changes detected')}
                className="px-4 py-2 rounded text-sm bg-blue-600 hover:bg-blue-500 text-white disabled:opacity-50 disabled:cursor-not-allowed transition-colors"
              >
                {executing ? 'Executing...' : 'Apply Changes'}
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  )
}
