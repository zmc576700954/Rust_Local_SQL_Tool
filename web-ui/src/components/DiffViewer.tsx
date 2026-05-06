import { Plus, Minus, Edit2, CheckCircle2 } from 'lucide-react';

interface ColumnInfo {
  column_name: string;
  column_type: string;
  is_nullable: string;
  column_comment?: string;
  column_key: string;
  column_default?: string;
  extra: string;
}

interface IndexInfo {
  index_name: string;
  column_name: string;
  non_unique: boolean;
  index_type: string;
}

interface ForeignKeyInfo {
  constraint_name: string;
  column_name: string;
  referenced_table_name: string;
  referenced_column_name: string;
  update_rule: string;
  delete_rule: string;
}

interface ColumnDiff { old: ColumnInfo; new: ColumnInfo }
interface IndexDiff { old: IndexInfo; new: IndexInfo }
interface FkDiff { old: ForeignKeyInfo; new: ForeignKeyInfo }

export interface TableDiff {
  table_name: string;
  status: string;
  columns_added: ColumnInfo[];
  columns_removed: ColumnInfo[];
  columns_modified: ColumnDiff[];
  indexes_added: IndexInfo[];
  indexes_removed: IndexInfo[];
  indexes_modified: IndexDiff[];
  fks_added: ForeignKeyInfo[];
  fks_removed: ForeignKeyInfo[];
  fks_modified: FkDiff[];
}

export interface SchemaDiff {
  tables: TableDiff[];
}

interface DiffViewerProps {
  diff: SchemaDiff;
}

export function DiffViewer({ diff }: DiffViewerProps) {
  if (!diff || !diff.tables || diff.tables.length === 0) {
    return <div className="p-4 text-gray-500 text-sm">No differences found or invalid diff data.</div>;
  }

  const modifiedTables = diff.tables.filter(t => t.status !== 'unchanged');

  if (modifiedTables.length === 0) {
    return (
      <div className="p-8 flex flex-col items-center justify-center text-gray-400 gap-3">
        <CheckCircle2 className="w-12 h-12 text-green-500/50" />
        <p>Source and Target schemas are completely synchronized.</p>
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-4">
      {modifiedTables.map(table => (
        <div key={table.table_name} className="border border-[#30363d] rounded-lg bg-[#0d1117] overflow-hidden">
          <div className="px-4 py-2 border-b border-[#30363d] bg-[#161b22] flex items-center gap-2">
            {table.status === 'added' && <Plus className="w-4 h-4 text-green-500" />}
            {table.status === 'removed' && <Minus className="w-4 h-4 text-red-500" />}
            {table.status === 'modified' && <Edit2 className="w-4 h-4 text-blue-500" />}
            <span className="font-mono text-sm font-bold text-gray-200">{table.table_name}</span>
            <span className={`text-xs px-2 py-0.5 rounded-full ${
              table.status === 'added' ? 'bg-green-500/10 text-green-400' :
              table.status === 'removed' ? 'bg-red-500/10 text-red-400' :
              'bg-blue-500/10 text-blue-400'
            }`}>
              {table.status}
            </span>
          </div>
          
          <div className="p-4 flex flex-col gap-3">
            {/* Columns */}
            {(table.columns_added.length > 0 || table.columns_removed.length > 0 || table.columns_modified.length > 0) && (
              <div className="text-sm">
                <div className="font-semibold text-gray-400 mb-2 text-xs uppercase tracking-wider">Columns</div>
                <div className="flex flex-col gap-1">
                  {table.columns_added.map(c => (
                    <div key={c.column_name} className="flex items-center gap-2 text-green-400 bg-green-500/5 px-3 py-1.5 rounded border border-green-500/10">
                      <Plus className="w-3.5 h-3.5" />
                      <span className="font-mono">{c.column_name}</span>
                      <span className="text-gray-500 text-xs ml-2">{c.column_type} {c.is_nullable === 'NO' ? 'NOT NULL' : ''}</span>
                    </div>
                  ))}
                  {table.columns_removed.map(c => (
                    <div key={c.column_name} className="flex items-center gap-2 text-red-400 bg-red-500/5 px-3 py-1.5 rounded border border-red-500/10">
                      <Minus className="w-3.5 h-3.5" />
                      <span className="font-mono line-through">{c.column_name}</span>
                      <span className="text-gray-500 text-xs ml-2">{c.column_type}</span>
                    </div>
                  ))}
                  {table.columns_modified.map(c => (
                    <div key={c.old.column_name} className="flex flex-col gap-1 bg-blue-500/5 px-3 py-2 rounded border border-blue-500/10">
                      <div className="flex items-center gap-2 text-blue-400 mb-1">
                        <Edit2 className="w-3.5 h-3.5" />
                        <span className="font-mono">{c.old.column_name}</span>
                      </div>
                      <div className="grid grid-cols-2 gap-4 text-xs font-mono">
                        <div className="text-red-400/80 bg-red-500/10 px-2 py-1 rounded">- {c.old.column_type} {c.old.is_nullable === 'NO' ? 'NOT NULL' : ''} {c.old.column_default ? `DEFAULT ${c.old.column_default}` : ''}</div>
                        <div className="text-green-400/80 bg-green-500/10 px-2 py-1 rounded">+ {c.new.column_type} {c.new.is_nullable === 'NO' ? 'NOT NULL' : ''} {c.new.column_default ? `DEFAULT ${c.new.column_default}` : ''}</div>
                      </div>
                    </div>
                  ))}
                </div>
              </div>
            )}

            {/* Indexes */}
            {(table.indexes_added.length > 0 || table.indexes_removed.length > 0 || table.indexes_modified.length > 0) && (
              <div className="text-sm mt-2">
                <div className="font-semibold text-gray-400 mb-2 text-xs uppercase tracking-wider">Indexes</div>
                <div className="flex flex-col gap-1">
                  {table.indexes_added.map(i => (
                    <div key={i.index_name} className="flex items-center gap-2 text-green-400 bg-green-500/5 px-3 py-1.5 rounded border border-green-500/10">
                      <Plus className="w-3.5 h-3.5" />
                      <span className="font-mono">{i.index_name}</span>
                      <span className="text-gray-500 text-xs ml-2">({i.column_name}) {i.non_unique ? '' : 'UNIQUE'}</span>
                    </div>
                  ))}
                  {table.indexes_removed.map(i => (
                    <div key={i.index_name} className="flex items-center gap-2 text-red-400 bg-red-500/5 px-3 py-1.5 rounded border border-red-500/10">
                      <Minus className="w-3.5 h-3.5" />
                      <span className="font-mono line-through">{i.index_name}</span>
                    </div>
                  ))}
                  {table.indexes_modified.map(i => (
                    <div key={i.old.index_name} className="flex flex-col gap-1 bg-blue-500/5 px-3 py-2 rounded border border-blue-500/10">
                      <div className="flex items-center gap-2 text-blue-400 mb-1">
                        <Edit2 className="w-3.5 h-3.5" />
                        <span className="font-mono">{i.old.index_name}</span>
                      </div>
                      <div className="grid grid-cols-2 gap-4 text-xs font-mono">
                        <div className="text-red-400/80 bg-red-500/10 px-2 py-1 rounded">- ({i.old.column_name}) {i.old.non_unique ? '' : 'UNIQUE'}</div>
                        <div className="text-green-400/80 bg-green-500/10 px-2 py-1 rounded">+ ({i.new.column_name}) {i.new.non_unique ? '' : 'UNIQUE'}</div>
                      </div>
                    </div>
                  ))}
                </div>
              </div>
            )}

            {/* Foreign Keys */}
            {(table.fks_added.length > 0 || table.fks_removed.length > 0 || table.fks_modified.length > 0) && (
              <div className="text-sm mt-2">
                <div className="font-semibold text-gray-400 mb-2 text-xs uppercase tracking-wider">Foreign Keys</div>
                <div className="flex flex-col gap-1">
                  {table.fks_added.map(f => (
                    <div key={f.constraint_name} className="flex items-center gap-2 text-green-400 bg-green-500/5 px-3 py-1.5 rounded border border-green-500/10">
                      <Plus className="w-3.5 h-3.5" />
                      <span className="font-mono">{f.constraint_name}</span>
                      <span className="text-gray-500 text-xs ml-2">({f.column_name}) {"->"} {f.referenced_table_name}({f.referenced_column_name})</span>
                    </div>
                  ))}
                  {table.fks_removed.map(f => (
                    <div key={f.constraint_name} className="flex items-center gap-2 text-red-400 bg-red-500/5 px-3 py-1.5 rounded border border-red-500/10">
                      <Minus className="w-3.5 h-3.5" />
                      <span className="font-mono line-through">{f.constraint_name}</span>
                    </div>
                  ))}
                  {table.fks_modified.map(f => (
                    <div key={f.old.constraint_name} className="flex flex-col gap-1 bg-blue-500/5 px-3 py-2 rounded border border-blue-500/10">
                      <div className="flex items-center gap-2 text-blue-400 mb-1">
                        <Edit2 className="w-3.5 h-3.5" />
                        <span className="font-mono">{f.old.constraint_name}</span>
                      </div>
                      <div className="grid grid-cols-2 gap-4 text-xs font-mono">
                        <div className="text-red-400/80 bg-red-500/10 px-2 py-1 rounded">- ({f.old.column_name}) {"->"} {f.old.referenced_table_name}({f.old.referenced_column_name})</div>
                        <div className="text-green-400/80 bg-green-500/10 px-2 py-1 rounded">+ ({f.new.column_name}) {"->"} {f.new.referenced_table_name}({f.new.referenced_column_name})</div>
                      </div>
                    </div>
                  ))}
                </div>
              </div>
            )}
          </div>
        </div>
      ))}
    </div>
  );
}
