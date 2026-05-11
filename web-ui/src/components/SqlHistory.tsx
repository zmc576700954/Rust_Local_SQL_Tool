import { useState, useEffect, useCallback, useMemo, memo } from 'react';
import { api } from '../api';
import { Sparkles, History as HistoryIcon, Trash2, Search, Copy, Play, Database } from 'lucide-react';
import { useToast } from './Toast';

import { parseError, sanitizeForLog } from '../utils';
import { tr } from '../i18n';

interface History {
  id: string;
  sql: string;
  status: string;
  execution_time_ms: number;
  executed_at: number;
  db_id?: string | null;
  row_count?: number | null;
  affected_rows?: number | null;
  statement_kind?: string | null;
}

interface SqlHistoryProps {
  historyVersion: number;
  activeDbId?: string | null;
  onOpenSql: (sql: string) => void;
  onInsertSql: (sql: string) => void;
  onRunSql: (sql: string) => void;
}

const SaveSnippetForm = memo(({ sql, onCancel, onSuccess }: { sql: string, onCancel: () => void, onSuccess: () => void }) => {
  const { toast } = useToast();
  const [saveTitle, setSaveTitle] = useState('');
  const [saveDesc, setSaveDesc] = useState('');
  const [isTrainAI, setIsTrainAI] = useState(false);

  const handleSaveSnippet = async () => {
    if (!saveTitle.trim()) {
      toast(tr('保存片段需要标题。', 'Title is required to save snippet.'), "error");
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
      toast(
        isTrainAI
          ? tr('已用新 SQL 训练 AI！', 'AI trained successfully with new SQL!')
          : tr('片段保存成功！', 'Snippet saved successfully!'),
        "success"
      );
      onSuccess();
    } catch (e: unknown) {
        toast(tr('保存片段失败：', 'Failed to save snippet: ') + parseError(e).message, "error");
      }
  };

  return (
    <div className="mt-3 p-3 bg-[#0d1117] rounded border border-[#30363d]" onClick={e => e.stopPropagation()}>
      <h4 className="text-xs font-semibold text-purple-400 mb-2 flex items-center gap-1">
        <Sparkles className="w-3 h-3" /> {tr('保存智能片段', 'Save Smart Snippet')}
      </h4>
      <input
        type="text"
        placeholder={tr('标题 / 问题', 'Title / Question')}
        className="w-full mb-2 p-1.5 text-xs border rounded bg-[#161b22] border-[#30363d] text-gray-200 outline-none focus:border-purple-500"
        value={saveTitle}
        onChange={e => setSaveTitle(e.target.value)}
      />
      <input
        type="text"
        placeholder={tr('描述 / 上下文（可选）', 'Description / Context (optional)')}
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
          {tr('使用该片段训练 AI', 'Train AI with this snippet')}
        </label>
      </div>
      <div className="flex justify-end gap-2">
        <button onClick={onCancel} className="text-xs text-gray-400 hover:text-gray-200 px-2 py-1">{tr('取消', 'Cancel')}</button>
        <button onClick={handleSaveSnippet} className="text-xs bg-purple-600 hover:bg-purple-500 text-white px-3 py-1 rounded transition-colors shadow-sm">{tr('保存', 'Save')}</button>
      </div>
    </div>
  );
});

export function SqlHistory({ historyVersion, activeDbId, onOpenSql, onInsertSql, onRunSql }: SqlHistoryProps) {
  const { toast } = useToast();
  const [history, setHistory] = useState<History[]>([]);
  const [loading, setLoading] = useState(false);

  const [savingId, setSavingId] = useState<string | null>(null);
  const [searchQuery, setSearchQuery] = useState('');
  const [statusFilter, setStatusFilter] = useState<'all' | 'success' | 'error' | 'canceled'>('all');
  const [currentDbOnly, setCurrentDbOnly] = useState(false);

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
  }, [fetchHistory, historyVersion]);

  const handleClearHistory = useCallback(async () => {
    try {
      await api.clearHistory();
      fetchHistory();
      toast(tr('历史已清空。', 'History cleared.'), 'success');
    } catch (e: unknown) {
      console.error(sanitizeForLog(e));
      toast(tr('清空历史失败。', 'Failed to clear history.'), 'error');
    }
  }, [fetchHistory, toast]);

  const filteredHistory = useMemo(() => {
    const normalizedSearch = searchQuery.trim().toLowerCase();
    return history.filter((item) => {
      if (statusFilter !== 'all' && item.status !== statusFilter) {
        return false;
      }
      if (currentDbOnly && activeDbId && String(item.db_id || '') !== activeDbId) {
        return false;
      }
      if (!normalizedSearch) {
        return true;
      }
      const haystack = [
        item.sql,
        item.db_id || '',
        item.statement_kind || '',
        item.status,
      ].join('\n').toLowerCase();
      return haystack.includes(normalizedSearch);
    });
  }, [activeDbId, currentDbOnly, history, searchQuery, statusFilter]);

  const handleCopySql = useCallback(async (sql: string) => {
    try {
      await navigator.clipboard.writeText(sql);
      toast(tr('SQL 已复制。', 'SQL copied to clipboard.'), 'success');
    } catch (e: unknown) {
      console.error(sanitizeForLog(e));
      toast(tr('复制 SQL 失败。', 'Failed to copy SQL.'), 'error');
    }
  }, [toast]);

  const statusClassName = (status: string) => {
    if (status === 'success') return 'bg-green-500/20 text-green-400';
    if (status === 'canceled') return 'bg-amber-500/20 text-amber-300';
    return 'bg-red-500/20 text-red-400';
  };

  const resetFilters = () => {
    setSearchQuery('');
    setStatusFilter('all');
    setCurrentDbOnly(false);
  };

  return (
    <div className="flex flex-col h-full bg-[#0d1117] border-l border-[#30363d] w-full text-sm">
      <div className="flex items-center border-b border-[#30363d] p-3 bg-[#0d1117]">
        <HistoryIcon className="w-4 h-4 text-gray-400 mr-2" />
        <h2 className="font-semibold text-gray-200">{tr('执行历史', 'Execution History')}</h2>
        <span className="ml-2 text-[10px] px-1.5 py-0.5 rounded bg-[#161b22] text-gray-400 border border-[#30363d]">
          {filteredHistory.length}/{history.length}
        </span>
        <div className="flex-1"></div>
        <button onClick={handleClearHistory} className="text-gray-400 hover:text-red-400 transition-colors" title={tr('清空全部历史', 'Clear All History')}>
          <Trash2 className="w-4 h-4" />
        </button>
      </div>

      <div className="border-b border-[#30363d] p-3 bg-[#11161d] space-y-3">
        <div className="relative">
          <Search className="w-3.5 h-3.5 text-gray-500 absolute left-2.5 top-2.5" />
          <input
            type="text"
            value={searchQuery}
            onChange={(e) => setSearchQuery(e.target.value)}
            placeholder={tr('搜索 SQL / DB / 类型 / 状态', 'Search SQL / DB / type / status')}
            className="w-full pl-8 pr-3 py-2 text-xs border rounded bg-[#161b22] border-[#30363d] text-gray-200 outline-none focus:border-blue-500"
          />
        </div>
        <div className="flex items-center gap-2 flex-wrap">
          {(['all', 'success', 'error', 'canceled'] as const).map((status) => (
            <button
              key={status}
              onClick={() => setStatusFilter(status)}
              className={`px-2 py-1 text-xs rounded border transition-colors ${
                statusFilter === status
                  ? 'bg-blue-500/20 text-blue-300 border-blue-500/30'
                  : 'bg-[#161b22] text-gray-400 border-[#30363d] hover:text-gray-200'
              }`}
            >
              {status.toUpperCase()}
            </button>
          ))}
          {activeDbId && (
            <button
              onClick={() => setCurrentDbOnly((value) => !value)}
              className={`px-2 py-1 text-xs rounded border transition-colors flex items-center gap-1 ${
                currentDbOnly
                  ? 'bg-purple-500/20 text-purple-300 border-purple-500/30'
                  : 'bg-[#161b22] text-gray-400 border-[#30363d] hover:text-gray-200'
              }`}
            >
              <Database className="w-3 h-3" />
              {tr('当前连接', 'Current DB')}
            </button>
          )}
          <button
            onClick={resetFilters}
            disabled={!searchQuery && statusFilter === 'all' && !currentDbOnly}
            className="px-2 py-1 text-xs rounded border bg-[#161b22] text-gray-400 border-[#30363d] hover:text-gray-200 disabled:opacity-50"
          >
            {tr('重置筛选', 'Reset filters')}
          </button>
        </div>
      </div>

      <div className="flex-1 overflow-y-auto p-4">
        {loading ? (
          <div className="text-center text-gray-500 mt-10">{tr('加载中...', 'Loading...')}</div>
        ) : filteredHistory.length === 0 ? (
          <div className="text-center text-gray-500 text-xs mt-10 flex flex-col items-center">
            <HistoryIcon className="w-8 h-8 mb-2 opacity-20" />
            <p>{tr('没有匹配的历史记录。', 'No matching history found.')}</p>
          </div>
        ) : (
          <div className="space-y-3">
            {filteredHistory.map(h => (
              <div key={h.id} className="p-3 bg-[#161b22] rounded border border-[#30363d] group cursor-pointer hover:border-blue-500/50 transition-colors" onDoubleClick={() => onOpenSql(h.sql)}>
                <div className="flex justify-between items-start mb-1">
                  <div className="flex items-center gap-2 flex-wrap">
                    <span className={`text-xs px-1.5 py-0.5 rounded ${statusClassName(h.status)}`}>
                      {h.status}
                    </span>
                    {h.statement_kind && (
                      <span className="text-[10px] px-1.5 py-0.5 rounded bg-[#0d1117] text-gray-400 border border-[#30363d]">
                        {h.statement_kind}
                      </span>
                    )}
                    {h.db_id && (
                      <span className="text-[10px] px-1.5 py-0.5 rounded bg-[#0d1117] text-blue-300 border border-blue-500/20">
                        {h.db_id}
                      </span>
                    )}
                  </div>
                  <span className="text-[10px] text-gray-500">{new Date(h.executed_at * 1000).toLocaleString()}</span>
                </div>
                <pre className="text-xs bg-[#0d1117] p-2 rounded overflow-x-auto text-gray-300 mb-2">
                  {h.sql}
                </pre>
                <div className="flex justify-between items-center mt-2 gap-3 flex-wrap">
                  <div className="flex items-center gap-2 flex-wrap text-xs text-gray-500">
                    <span>{h.execution_time_ms}ms</span>
                    {typeof h.row_count === 'number' && (
                      <span>{tr(`${h.row_count} 行`, `${h.row_count} rows`)}</span>
                    )}
                    {typeof h.affected_rows === 'number' && (
                      <span>{tr(`影响 ${h.affected_rows} 行`, `${h.affected_rows} affected`)}</span>
                    )}
                  </div>
                  <div className="flex gap-2 items-center flex-wrap">
                    {h.status === 'success' && (
                      <button
                        onClick={(e) => {
                          e.stopPropagation();
                          setSavingId(h.id);
                        }}
                        className="text-purple-400 hover:text-purple-300 text-xs flex items-center gap-1"
                      >
                        <Sparkles className="w-3 h-3" /> {tr('保存为智能片段', 'Save as Smart Snippet')}
                      </button>
                    )}
                    <button
                      onClick={(e) => {
                        e.stopPropagation();
                        onRunSql(h.sql);
                      }}
                      className="px-2 py-1 rounded bg-green-600/90 hover:bg-green-500 text-white text-xs transition-colors flex items-center gap-1"
                    >
                      <Play className="w-3 h-3" />
                      {tr('重新执行', 'Run Again')}
                    </button>
                    <button
                      onClick={(e) => {
                        e.stopPropagation();
                        onOpenSql(h.sql);
                      }}
                      className="px-2 py-1 rounded bg-blue-600/90 hover:bg-blue-500 text-white text-xs transition-colors"
                    >
                      {tr('打开', 'Open')}
                    </button>
                    <button
                      onClick={(e) => {
                        e.stopPropagation();
                        onInsertSql(h.sql);
                      }}
                      className="text-blue-400 hover:underline text-xs"
                    >
                      {tr('插入', 'Insert')}
                    </button>
                    <button
                      onClick={(e) => {
                        e.stopPropagation();
                        void handleCopySql(h.sql);
                      }}
                      className="text-gray-400 hover:text-gray-200 text-xs flex items-center gap-1"
                    >
                      <Copy className="w-3 h-3" />
                      {tr('复制 SQL', 'Copy SQL')}
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
