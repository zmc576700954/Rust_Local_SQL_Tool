import { useState, useEffect, useCallback, memo } from 'react';
import { api } from '../api';
import { Sparkles, History as HistoryIcon, Trash2 } from 'lucide-react';
import { useToast } from './Toast';

import { parseError, sanitizeForLog } from '../utils';

interface History {
  id: string;
  sql: string;
  status: string;
  execution_time_ms: number;
  executed_at: number;
}

interface SqlHistoryProps {
  onInsertSql: (sql: string) => void;
}

const SaveSnippetForm = memo(({ sql, onCancel, onSuccess }: { sql: string, onCancel: () => void, onSuccess: () => void }) => {
  const { toast } = useToast();
  const [saveTitle, setSaveTitle] = useState('');
  const [saveDesc, setSaveDesc] = useState('');
  const [isTrainAI, setIsTrainAI] = useState(false);

  const handleSaveSnippet = async () => {
    if (!saveTitle.trim()) {
      toast("Title is required to save snippet.", "error");
      return;
    }
    try {
      await api.addKnowledge({
        knowledge_type: 'sql',
        title: saveTitle,
        description: saveDesc,
        content: sql,
        is_golden: isTrainAI,
      });
      toast(isTrainAI ? "AI trained successfully with new SQL!" : "Snippet saved successfully!", "success");
      onSuccess();
    } catch (e: unknown) {
        toast("Failed to save snippet: " + parseError(e).message, "error");
      }
  };

  return (
    <div className="mt-3 p-3 bg-[#0d1117] rounded border border-[#30363d]" onClick={e => e.stopPropagation()}>
      <h4 className="text-xs font-semibold text-purple-400 mb-2 flex items-center gap-1">
        <Sparkles className="w-3 h-3" /> Save Smart Snippet
      </h4>
      <input
        type="text"
        placeholder="Title / Question"
        className="w-full mb-2 p-1.5 text-xs border rounded bg-[#161b22] border-[#30363d] text-gray-200 outline-none focus:border-purple-500"
        value={saveTitle}
        onChange={e => setSaveTitle(e.target.value)}
      />
      <input
        type="text"
        placeholder="Description / Context (optional)"
        className="w-full mb-2 p-1.5 text-xs border rounded bg-[#161b22] border-[#30363d] text-gray-200 outline-none focus:border-purple-500"
        value={saveDesc}
        onChange={e => setSaveDesc(e.target.value)}
      />
      <div className="flex items-center gap-2 mb-3">
        <input 
          type="checkbox" 
          id={`trainAI-${sql.length}`}
          checked={isTrainAI}
          onChange={e => setIsTrainAI(e.target.checked)}
          className="w-3.5 h-3.5 rounded border-gray-300 text-purple-600 focus:ring-purple-500 bg-[#161b22]"
        />
        <label htmlFor={`trainAI-${sql.length}`} className="text-xs text-gray-300 flex items-center gap-1 cursor-pointer">
          Train AI with this snippet
        </label>
      </div>
      <div className="flex justify-end gap-2">
        <button onClick={onCancel} className="text-xs text-gray-400 hover:text-gray-200 px-2 py-1">Cancel</button>
        <button onClick={handleSaveSnippet} className="text-xs bg-purple-600 hover:bg-purple-500 text-white px-3 py-1 rounded transition-colors shadow-sm">Save</button>
      </div>
    </div>
  );
});

export function SqlHistory({ onInsertSql }: SqlHistoryProps) {
  const [history, setHistory] = useState<History[]>([]);
  const [loading, setLoading] = useState(false);

  const [savingId, setSavingId] = useState<string | null>(null);

  const handleCancelSave = useCallback(() => setSavingId(null), []);
  const handleSuccessSave = useCallback(() => setSavingId(null), []);

  const fetchHistory = useCallback(async () => {
    try {
      setLoading(true);
      const res = await api.getHistory();
      // sort by executed_at descending
      const sorted = (res || []).sort((a: History, b: History) => b.executed_at - a.executed_at);
      setHistory(sorted);
    } catch (e: unknown) {
      console.error(sanitizeForLog(e));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    fetchHistory();
  }, [fetchHistory]);

  const handleClearHistory = useCallback(async () => {
    try {
      await api.clearHistory();
      fetchHistory();
    } catch (e: unknown) {
      console.error(sanitizeForLog(e));
    }
  }, [fetchHistory]);

  return (
    <div className="flex flex-col h-full bg-[#0d1117] border-l border-[#30363d] w-full text-sm">
      <div className="flex items-center border-b border-[#30363d] p-3 bg-[#0d1117]">
        <HistoryIcon className="w-4 h-4 text-gray-400 mr-2" />
        <h2 className="font-semibold text-gray-200">Execution History</h2>
        <div className="flex-1"></div>
        <button onClick={handleClearHistory} className="text-gray-400 hover:text-red-400 transition-colors" title="Clear All History">
          <Trash2 className="w-4 h-4" />
        </button>
      </div>

      <div className="flex-1 overflow-y-auto p-4">
        {loading ? (
          <div className="text-center text-gray-500 mt-10">Loading...</div>
        ) : history.length === 0 ? (
          <div className="text-center text-gray-500 text-xs mt-10 flex flex-col items-center">
            <HistoryIcon className="w-8 h-8 mb-2 opacity-20" />
            <p>No history found.</p>
          </div>
        ) : (
          <div className="space-y-3">
            {history.map(h => (
              <div key={h.id} className="p-3 bg-[#161b22] rounded border border-[#30363d] group cursor-pointer hover:border-blue-500/50 transition-colors" onDoubleClick={() => onInsertSql(h.sql)}>
                <div className="flex justify-between items-start mb-1">
                  <span className={`text-xs px-1.5 py-0.5 rounded ${h.status === 'success' ? 'bg-green-500/20 text-green-400' : 'bg-red-500/20 text-red-400'}`}>
                    {h.status}
                  </span>
                  <span className="text-[10px] text-gray-500">{new Date(h.executed_at * 1000).toLocaleString()}</span>
                </div>
                <pre className="text-xs bg-[#0d1117] p-2 rounded overflow-x-auto text-gray-300 mb-1">
                  {h.sql}
                </pre>
                <div className="flex justify-between items-center mt-2">
                  <span className="text-xs text-gray-500">{h.execution_time_ms}ms</span>
                  <div className="hidden group-hover:flex gap-2 items-center">
                    {h.status === 'success' && (
                      <button 
                        onClick={(e) => { 
                          e.stopPropagation(); 
                          setSavingId(h.id); 
                        }} 
                        className="text-purple-400 hover:text-purple-300 text-xs flex items-center gap-1"
                      >
                        <Sparkles className="w-3 h-3" /> Save as Smart Snippet
                      </button>
                    )}
                    <button onClick={(e) => { e.stopPropagation(); onInsertSql(h.sql); }} className="text-blue-400 hover:underline text-xs">
                      Insert
                    </button>
                  </div>
                </div>
                {savingId === h.id && (
                  <SaveSnippetForm 
                    sql={h.sql} 
                    onCancel={handleCancelSave} 
                    onSuccess={handleSuccessSave} 
                  />
                )}
              </div>
            ))}
          </div>
        )}
      </div>
    </div>
  );
}
