import { useState, useEffect } from 'react';
import { api } from '../api';
import { Loader2 } from 'lucide-react';
import { useToast } from './Toast';

interface ColumnInfo {
  column_name: string;
  column_type: string;
}

interface TableWithDetails {
  table_name: string;
  columns: ColumnInfo[];
}

interface MockDataConfigProps {
  tableName: string;
  rules: Record<string, string>;
  onChangeRules: (rules: Record<string, string>) => void;
}

export function MockDataConfig({ tableName, rules, onChangeRules }: MockDataConfigProps) {
  const [table, setTable] = useState<TableWithDetails | null>(null);
  const [loading, setLoading] = useState(true);
  const { toast } = useToast();

  useEffect(() => {
    let active = true;
    api.getTableSchema(tableName).then((res) => {
      if (active) {
        setTable(res);
        setLoading(false);
      }
    }).catch((e: any) => {
      if (active) {
        setLoading(false);
        toast(e.message || 'Failed to load schema', 'error');
      }
    });
    return () => { active = false; };
  }, [tableName, toast]);

  if (loading) {
    return <div className="flex items-center gap-2 text-sm text-gray-500 py-4"><Loader2 className="w-4 h-4 animate-spin" /> Loading schema...</div>;
  }

  if (!table) {
    return <div className="text-sm text-red-500 py-4">Failed to load table schema.</div>;
  }

  const commonRules = [
    { label: 'Auto (AI Generated)', value: '' },
    { label: 'Email', value: 'email format' },
    { label: 'Phone Number', value: 'phone number format' },
    { label: 'UUID', value: 'valid UUID v4' },
    { label: 'Name (Person)', value: 'person full name' },
    { label: 'Company Name', value: 'company name' },
    { label: 'Random Number (1-100)', value: 'integer between 1 and 100' },
    { label: 'Boolean (true/false)', value: 'boolean value (true or false)' },
    { label: 'Date (Past Year)', value: 'random date in the past year' },
    { label: 'Custom (Type below)', value: 'custom' },
  ];

  return (
    <div className="flex flex-col gap-4 bg-[#0d1117] border border-[#30363d] rounded-lg p-4">
      <h4 className="text-sm font-bold text-gray-300 uppercase tracking-wider mb-2">Field Rules Configuration</h4>
      
      <div className="flex flex-col gap-3">
        {table.columns.map((col) => {
          const currentVal = rules[col.column_name] || '';
          const isCustom = !commonRules.find(r => r.value === currentVal) && currentVal !== '';
          const selectVal = isCustom ? 'custom' : currentVal;

          return (
            <div key={col.column_name} className="flex flex-col gap-1.5 pb-3 border-b border-[#30363d]/50 last:border-0 last:pb-0">
              <div className="flex items-center gap-2">
                <span className="font-mono text-sm text-blue-400 font-bold w-32 shrink-0 truncate" title={col.column_name}>{col.column_name}</span>
                <span className="text-xs text-gray-500 w-24 shrink-0 truncate" title={col.column_type}>{col.column_type}</span>
                
                <select
                  value={selectVal}
                  onChange={(e) => {
                    const val = e.target.value;
                    if (val === 'custom') {
                      onChangeRules({ ...rules, [col.column_name]: 'custom rule here...' });
                    } else if (val === '') {
                      const newRules = { ...rules };
                      delete newRules[col.column_name];
                      onChangeRules(newRules);
                    } else {
                      onChangeRules({ ...rules, [col.column_name]: val });
                    }
                  }}
                  className="bg-[#161b22] border border-[#30363d] rounded px-2 py-1 text-xs text-gray-300 focus:outline-none focus:border-blue-500 w-48 shrink-0"
                >
                  {commonRules.map(r => (
                    <option key={r.label} value={r.value}>{r.label}</option>
                  ))}
                </select>

                {selectVal === 'custom' && (
                  <input
                    type="text"
                    value={currentVal}
                    onChange={(e) => onChangeRules({ ...rules, [col.column_name]: e.target.value })}
                    className="flex-1 bg-[#161b22] border border-[#30363d] rounded px-2 py-1 text-xs text-gray-300 focus:outline-none focus:border-blue-500 min-w-0"
                    placeholder="Enter custom regex, enum, or description..."
                  />
                )}
              </div>
            </div>
          );
        })}
      </div>
    </div>
  );
}