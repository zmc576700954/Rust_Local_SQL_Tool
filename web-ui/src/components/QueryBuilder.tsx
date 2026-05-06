import { useState, useEffect } from 'react';
import { Database, Plus, Trash2, CheckSquare } from 'lucide-react';
import { format as formatSql } from 'sql-formatter';

interface TableNode {
  id: string;
  tableName: string;
  alias: string;
  columns: any[];
  selectedColumns: string[];
}

interface JoinRelation {
  id: string;
  leftTableId: string;
  leftColumn: string;
  rightTableId: string;
  rightColumn: string;
  type: 'INNER' | 'LEFT' | 'RIGHT';
}

interface FilterCondition {
  id: string;
  tableId: string;
  column: string;
  operator: string;
  value: string;
}

interface QueryBuilderProps {
  onApplySql: (sql: string) => void;
  schemaData: any;
}

export function QueryBuilder({ onApplySql, schemaData }: QueryBuilderProps) {
  const [nodes, setNodes] = useState<TableNode[]>([]);
  const [joins, setJoins] = useState<JoinRelation[]>([]);
  const [filters, setFilters] = useState<FilterCondition[]>([]);
  const [generatedSql, setGeneratedSql] = useState('');

  const availableTables = schemaData?.tables || [];

  const addTable = (tableName: string) => {
    const tableInfo = availableTables.find((t: any) => t.table_name === tableName);
    if (!tableInfo) return;
    
    const newNode: TableNode = {
      id: `table_${Date.now()}_${Math.floor(Math.random() * 1000)}`,
      tableName,
      alias: `t${nodes.length + 1}`,
      columns: tableInfo.columns || [],
      selectedColumns: []
    };
    
    setNodes([...nodes, newNode]);
  };

  const removeTable = (id: string) => {
    setNodes(nodes.filter(n => n.id !== id));
    setJoins(joins.filter(j => j.leftTableId !== id && j.rightTableId !== id));
    setFilters(filters.filter(f => f.tableId !== id));
  };

  const toggleColumn = (tableId: string, columnName: string) => {
    setNodes(nodes.map(n => {
      if (n.id === tableId) {
        const selected = n.selectedColumns.includes(columnName)
          ? n.selectedColumns.filter(c => c !== columnName)
          : [...n.selectedColumns, columnName];
        return { ...n, selectedColumns: selected };
      }
      return n;
    }));
  };

  const addJoin = () => {
    if (nodes.length < 2) return;
    const newJoin: JoinRelation = {
      id: `join_${Date.now()}`,
      leftTableId: nodes[0].id,
      leftColumn: nodes[0].columns[0]?.column_name || '',
      rightTableId: nodes[1].id,
      rightColumn: nodes[1].columns[0]?.column_name || '',
      type: 'INNER'
    };
    setJoins([...joins, newJoin]);
  };

  const updateJoin = (id: string, field: keyof JoinRelation, value: string) => {
    setJoins(joins.map(j => j.id === id ? { ...j, [field]: value } : j));
  };

  const removeJoin = (id: string) => {
    setJoins(joins.filter(j => j.id !== id));
  };

  const addFilter = () => {
    if (nodes.length === 0) return;
    const newFilter: FilterCondition = {
      id: `filter_${Date.now()}`,
      tableId: nodes[0].id,
      column: nodes[0].columns[0]?.column_name || '',
      operator: '=',
      value: ''
    };
    setFilters([...filters, newFilter]);
  };

  const updateFilter = (id: string, field: keyof FilterCondition, value: string) => {
    setFilters(filters.map(f => f.id === id ? { ...f, [field]: value } : f));
  };

  const removeFilter = (id: string) => {
    setFilters(filters.filter(f => f.id !== id));
  };

  // Generate SQL whenever dependencies change
  useEffect(() => {
    if (nodes.length === 0) {
      setGeneratedSql('');
      return;
    }

    let selectParts: string[] = [];
    nodes.forEach(n => {
      n.selectedColumns.forEach(c => {
        selectParts.push(`${n.alias}.${c}`);
      });
    });

    if (selectParts.length === 0) {
      selectParts = ['*'];
    }

    let sql = `SELECT ${selectParts.join(', ')} \nFROM ${nodes[0].tableName} ${nodes[0].alias}`;

    // Process Joins
    joins.forEach(j => {
      const leftNode = nodes.find(n => n.id === j.leftTableId);
      const rightNode = nodes.find(n => n.id === j.rightTableId);
      if (leftNode && rightNode) {
        sql += ` \n${j.type} JOIN ${rightNode.tableName} ${rightNode.alias} ON ${leftNode.alias}.${j.leftColumn} = ${rightNode.alias}.${j.rightColumn}`;
      }
    });

    // Process Filters
    if (filters.length > 0) {
      const filterParts = filters.map(f => {
        const node = nodes.find(n => n.id === f.tableId);
        if (!node) return '';
        const val = isNaN(Number(f.value)) ? `'${f.value}'` : f.value;
        return `${node.alias}.${f.column} ${f.operator} ${val}`;
      }).filter(Boolean);
      
      if (filterParts.length > 0) {
        sql += ` \nWHERE ${filterParts.join(' AND ')}`;
      }
    }

    try {
      const formatted = formatSql(sql, { language: 'mysql', keywordCase: 'upper' });
      setGeneratedSql(formatted);
    } catch {
      setGeneratedSql(sql);
    }

  }, [nodes, joins, filters]);

  return (
    <div className="flex flex-col h-full bg-[#0a0c10] text-gray-300">
      <div className="p-4 border-b border-[#30363d] flex items-center justify-between bg-[#161b22]">
        <div className="flex items-center gap-2">
          <Database className="w-5 h-5 text-blue-400" />
          <h2 className="text-lg font-semibold text-white">Visual Query Builder</h2>
        </div>
        <div className="flex items-center gap-3">
          <select 
            onChange={(e) => {
              if (e.target.value) {
                addTable(e.target.value);
                e.target.value = '';
              }
            }}
            className="bg-[#0d1117] border border-[#30363d] rounded px-3 py-1.5 text-sm text-white focus:outline-none focus:border-blue-500"
            value=""
          >
            <option value="">+ Add Table</option>
            {availableTables.map((t: any) => (
              <option key={t.table_name} value={t.table_name}>{t.table_name}</option>
            ))}
          </select>
          <button 
            onClick={() => onApplySql(generatedSql)}
            disabled={!generatedSql}
            className="px-4 py-1.5 bg-blue-600 hover:bg-blue-500 text-white rounded text-sm font-medium transition-colors disabled:opacity-50"
          >
            Apply to Editor
          </button>
        </div>
      </div>

      <div className="flex-1 flex flex-col overflow-hidden">
        {/* Visual Workspace Area */}
        <div className="flex-1 flex p-4 gap-4 overflow-auto bg-grid-pattern relative">
          {nodes.length === 0 ? (
            <div className="absolute inset-0 flex flex-col items-center justify-center text-gray-500 pointer-events-none">
              <Database className="w-12 h-12 mb-4 opacity-20" />
              <p>No tables selected.</p>
              <p className="text-sm">Use the "+ Add Table" button above to start building.</p>
            </div>
          ) : (
            <div className="flex flex-col gap-6 w-full">
              {/* Tables Section */}
              <div>
                <h3 className="text-sm font-bold text-gray-400 uppercase tracking-wider mb-3">Tables & Columns</h3>
                <div className="flex flex-wrap gap-4">
                  {nodes.map(node => (
                    <div key={node.id} className="bg-[#161b22] border border-[#30363d] rounded-lg shadow-lg w-64 flex flex-col max-h-80">
                      <div className="p-3 border-b border-[#30363d] bg-[#0d1117] flex justify-between items-center rounded-t-lg">
                        <span className="font-semibold text-blue-400 truncate" title={node.tableName}>{node.tableName} <span className="text-gray-500 text-xs ml-1">({node.alias})</span></span>
                        <button onClick={() => removeTable(node.id)} className="text-gray-500 hover:text-red-400"><Trash2 className="w-4 h-4" /></button>
                      </div>
                      <div className="p-2 overflow-y-auto flex-1">
                        {node.columns.map((c: any) => (
                          <label key={c.column_name} className="flex items-center gap-2 p-1.5 hover:bg-[#21262d] rounded cursor-pointer group">
                            <input 
                              type="checkbox" 
                              checked={node.selectedColumns.includes(c.column_name)}
                              onChange={() => toggleColumn(node.id, c.column_name)}
                              className="rounded border-gray-600 bg-[#0d1117] text-blue-500 focus:ring-blue-500 focus:ring-offset-[#161b22]"
                            />
                            <span className="text-sm text-gray-300 group-hover:text-white flex-1 truncate" title={c.column_name}>{c.column_name}</span>
                            <span className="text-[10px] text-gray-500">{c.data_type}</span>
                          </label>
                        ))}
                      </div>
                    </div>
                  ))}
                </div>
              </div>

              {/* Joins Section */}
              {nodes.length > 1 && (
                <div>
                  <div className="flex items-center gap-3 mb-3">
                    <h3 className="text-sm font-bold text-gray-400 uppercase tracking-wider">Relations (JOINs)</h3>
                    <button onClick={addJoin} className="flex items-center gap-1 text-xs bg-blue-500/10 text-blue-400 px-2 py-1 rounded hover:bg-blue-500/20">
                      <Plus className="w-3 h-3" /> Add Join
                    </button>
                  </div>
                  <div className="flex flex-col gap-2">
                    {joins.map(join => (
                      <div key={join.id} className="flex items-center gap-2 bg-[#161b22] border border-[#30363d] p-2 rounded">
                        <select 
                          value={join.leftTableId} 
                          onChange={(e) => updateJoin(join.id, 'leftTableId', e.target.value)}
                          className="bg-[#0d1117] border border-[#30363d] rounded px-2 py-1 text-sm w-32"
                        >
                          {nodes.map(n => <option key={n.id} value={n.id}>{n.tableName} ({n.alias})</option>)}
                        </select>
                        <select 
                          value={join.leftColumn} 
                          onChange={(e) => updateJoin(join.id, 'leftColumn', e.target.value)}
                          className="bg-[#0d1117] border border-[#30363d] rounded px-2 py-1 text-sm w-32"
                        >
                          {nodes.find(n => n.id === join.leftTableId)?.columns.map((c: any) => <option key={c.column_name} value={c.column_name}>{c.column_name}</option>)}
                        </select>
                        
                        <select 
                          value={join.type} 
                          onChange={(e) => updateJoin(join.id, 'type', e.target.value as any)}
                          className="bg-blue-500/20 text-blue-400 border border-blue-500/30 rounded px-2 py-1 text-sm font-bold"
                        >
                          <option value="INNER">INNER JOIN</option>
                          <option value="LEFT">LEFT JOIN</option>
                          <option value="RIGHT">RIGHT JOIN</option>
                        </select>

                        <select 
                          value={join.rightTableId} 
                          onChange={(e) => updateJoin(join.id, 'rightTableId', e.target.value)}
                          className="bg-[#0d1117] border border-[#30363d] rounded px-2 py-1 text-sm w-32"
                        >
                          {nodes.map(n => <option key={n.id} value={n.id}>{n.tableName} ({n.alias})</option>)}
                        </select>
                        <select 
                          value={join.rightColumn} 
                          onChange={(e) => updateJoin(join.id, 'rightColumn', e.target.value)}
                          className="bg-[#0d1117] border border-[#30363d] rounded px-2 py-1 text-sm w-32"
                        >
                          {nodes.find(n => n.id === join.rightTableId)?.columns.map((c: any) => <option key={c.column_name} value={c.column_name}>{c.column_name}</option>)}
                        </select>

                        <button onClick={() => removeJoin(join.id)} className="ml-auto text-gray-500 hover:text-red-400 p-1"><Trash2 className="w-4 h-4" /></button>
                      </div>
                    ))}
                    {joins.length === 0 && <p className="text-xs text-gray-500 italic">No joins defined.</p>}
                  </div>
                </div>
              )}

              {/* Filters Section */}
              {nodes.length > 0 && (
                <div>
                  <div className="flex items-center gap-3 mb-3">
                    <h3 className="text-sm font-bold text-gray-400 uppercase tracking-wider">Filters (WHERE)</h3>
                    <button onClick={addFilter} className="flex items-center gap-1 text-xs bg-blue-500/10 text-blue-400 px-2 py-1 rounded hover:bg-blue-500/20">
                      <Plus className="w-3 h-3" /> Add Filter
                    </button>
                  </div>
                  <div className="flex flex-col gap-2">
                    {filters.map(filter => (
                      <div key={filter.id} className="flex items-center gap-2 bg-[#161b22] border border-[#30363d] p-2 rounded">
                        <select 
                          value={filter.tableId} 
                          onChange={(e) => updateFilter(filter.id, 'tableId', e.target.value)}
                          className="bg-[#0d1117] border border-[#30363d] rounded px-2 py-1 text-sm w-32"
                        >
                          {nodes.map(n => <option key={n.id} value={n.id}>{n.tableName} ({n.alias})</option>)}
                        </select>
                        <select 
                          value={filter.column} 
                          onChange={(e) => updateFilter(filter.id, 'column', e.target.value)}
                          className="bg-[#0d1117] border border-[#30363d] rounded px-2 py-1 text-sm w-32"
                        >
                          {nodes.find(n => n.id === filter.tableId)?.columns.map((c: any) => <option key={c.column_name} value={c.column_name}>{c.column_name}</option>)}
                        </select>
                        <select 
                          value={filter.operator} 
                          onChange={(e) => updateFilter(filter.id, 'operator', e.target.value)}
                          className="bg-[#0d1117] border border-[#30363d] rounded px-2 py-1 text-sm w-20 text-center font-mono"
                        >
                          <option value="=">=</option>
                          <option value="!=">!=</option>
                          <option value=">">&gt;</option>
                          <option value="<">&lt;</option>
                          <option value=">=">&gt;=</option>
                          <option value="<=">&lt;=</option>
                          <option value="LIKE">LIKE</option>
                        </select>
                        <input 
                          type="text" 
                          value={filter.value}
                          onChange={(e) => updateFilter(filter.id, 'value', e.target.value)}
                          placeholder="Value..."
                          className="bg-[#0d1117] border border-[#30363d] rounded px-2 py-1 text-sm flex-1 font-mono"
                        />
                        <button onClick={() => removeFilter(filter.id)} className="text-gray-500 hover:text-red-400 p-1"><Trash2 className="w-4 h-4" /></button>
                      </div>
                    ))}
                    {filters.length === 0 && <p className="text-xs text-gray-500 italic">No filters defined.</p>}
                  </div>
                </div>
              )}

            </div>
          )}
        </div>

        {/* SQL Preview Area */}
        <div className="h-64 border-t border-[#30363d] bg-[#0d1117] flex flex-col shrink-0">
          <div className="px-4 py-2 border-b border-[#30363d] flex items-center justify-between bg-[#161b22]">
            <span className="text-xs font-bold text-gray-400 uppercase tracking-wider">Generated SQL Preview</span>
            <CheckSquare className="w-4 h-4 text-green-500" />
          </div>
          <div className="p-4 overflow-auto flex-1 font-mono text-sm text-green-400/90 whitespace-pre">
            {generatedSql || '-- Build your query visually to see SQL here'}
          </div>
        </div>
      </div>
    </div>
  );
}
