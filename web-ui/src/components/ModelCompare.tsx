import { useEffect, useMemo, useState } from 'react';
import { api } from '../api';
import { useToast } from './Toast';
import { tr } from '../i18n';
import { DiffViewer } from './DiffViewer';
import type { SchemaDiff } from './DiffViewer';

interface ModelCompareProps {
  onCancel: () => void;
}

const MODEL_COMPARE_STATE_KEY = 'tool:model-compare:state';

export function ModelCompare({ onCancel }: ModelCompareProps) {
  const { toast } = useToast();
  const [isLoading, setIsLoading] = useState(false);
  const [connections, setConnections] = useState<any[]>([]);
  const [sourceDbId, setSourceDbId] = useState('');
  const [targetDbId, setTargetDbId] = useState('');
  const [diff, setDiff] = useState<SchemaDiff | null>(null);
  const [errorText, setErrorText] = useState('');

  useEffect(() => {
    const loadConfig = async () => {
      setIsLoading(true);
      setErrorText('');
      try {
        const cfg = await api.getConfig();
        const list = Array.isArray(cfg?.db_connections) ? cfg.db_connections : [];
        setConnections(list);
        const activeId = String(cfg?.active_db_id || '');
        const savedRaw = window.localStorage.getItem(MODEL_COMPARE_STATE_KEY);
        const saved = savedRaw ? JSON.parse(savedRaw) : null;
        const savedSource = String(saved?.source_db_id || '');
        const savedTarget = String(saved?.target_db_id || '');

        const hasSource = list.some((c: any) => String(c?.id || '') === savedSource);
        const hasTarget = list.some((c: any) => String(c?.id || '') === savedTarget);
        const nextTarget = hasTarget ? savedTarget : activeId;
        if (nextTarget) setTargetDbId(nextTarget);

        const fallbackSource = list.find((c: any) => String(c?.id || '') !== (nextTarget || activeId)) || list[0];
        const nextSource = hasSource ? savedSource : String(fallbackSource?.id || '');
        if (nextSource) setSourceDbId(nextSource);
      } catch (e: any) {
        const msg = e?.message || String(e);
        setErrorText(msg);
        toast(tr('加载连接失败：', 'Failed to load connections: ') + msg, 'error');
      } finally {
        setIsLoading(false);
      }
    };
    loadConfig();
  }, [toast]);

  useEffect(() => {
    if (!sourceDbId && !targetDbId) return;
    window.localStorage.setItem(
      MODEL_COMPARE_STATE_KEY,
      JSON.stringify({
        source_db_id: sourceDbId,
        target_db_id: targetDbId,
      })
    );
  }, [sourceDbId, targetDbId]);

  const selectedSource = useMemo(
    () => connections.find((c) => String(c.id) === String(sourceDbId)),
    [connections, sourceDbId]
  );
  const selectedTarget = useMemo(
    () => connections.find((c) => String(c.id) === String(targetDbId)),
    [connections, targetDbId]
  );

  const summary = useMemo(() => {
    if (!diff?.tables) return { total: 0, added: 0, removed: 0, modified: 0, unchanged: 0 };
    const total = diff.tables.length;
    const added = diff.tables.filter((t) => t.status === 'added').length;
    const removed = diff.tables.filter((t) => t.status === 'removed').length;
    const modified = diff.tables.filter((t) => t.status === 'modified').length;
    const unchanged = diff.tables.filter((t) => t.status === 'unchanged').length;
    return { total, added, removed, modified, unchanged };
  }, [diff]);

  const compareModels = async () => {
    if (!sourceDbId || !targetDbId) {
      toast(tr('请选择源库和目标库。', 'Please select source and target.'), 'info');
      return;
    }
    if (sourceDbId === targetDbId) {
      toast(tr('源库和目标库不能相同。', 'Source and target cannot be same.'), 'info');
      return;
    }
    setIsLoading(true);
    setErrorText('');
    try {
      const result = await api.syncSchemaDiff(sourceDbId, targetDbId);
      setDiff(result as SchemaDiff);
      toast(tr('模型对比完成。', 'Model comparison completed.'), 'success');
    } catch (e: any) {
      const msg = e?.message || String(e);
      setErrorText(msg);
      toast(tr('模型对比失败：', 'Model comparison failed: ') + msg, 'error');
      setDiff(null);
    } finally {
      setIsLoading(false);
    }
  };

  return (
    <div className="flex flex-col h-full bg-[#161b22] text-gray-300 rounded-xl overflow-hidden shadow-2xl border border-[#30363d]">
      <div className="px-6 py-4 border-b border-[#30363d] flex items-center justify-between bg-[#0d1117] shrink-0">
        <h3 className="text-gray-200 font-bold text-lg">{tr('模型对比', 'Model Compare')}</h3>
        <button onClick={onCancel} className="text-gray-500 hover:text-white transition-colors">
          {tr('关闭', 'Close')}
        </button>
      </div>

      <div className="px-6 py-3 border-b border-[#30363d] bg-[#0d1117] grid grid-cols-[1fr_1fr_auto] gap-2 items-end shrink-0">
        <div>
          <div className="text-xs text-gray-500 mb-1">{tr('源库模型', 'Source Model')}</div>
          <select
            value={sourceDbId}
            onChange={(e) => setSourceDbId(e.target.value)}
            className="w-full bg-[#161b22] border border-[#30363d] rounded px-3 py-1.5 text-sm text-gray-200"
          >
            <option value="">{tr('-- 选择源库 --', '-- Select Source --')}</option>
            {connections.map((c) => (
              <option key={c.id} value={c.id}>
                {c.name || c.id}
              </option>
            ))}
          </select>
        </div>
        <div>
          <div className="text-xs text-gray-500 mb-1">{tr('目标库模型', 'Target Model')}</div>
          <select
            value={targetDbId}
            onChange={(e) => setTargetDbId(e.target.value)}
            className="w-full bg-[#161b22] border border-[#30363d] rounded px-3 py-1.5 text-sm text-gray-200"
          >
            <option value="">{tr('-- 选择目标库 --', '-- Select Target --')}</option>
            {connections.map((c) => (
              <option key={c.id} value={c.id}>
                {c.name || c.id}
              </option>
            ))}
          </select>
        </div>
        <button
          onClick={compareModels}
          disabled={!sourceDbId || !targetDbId || isLoading}
          className="h-[34px] px-4 rounded border border-blue-500/40 text-sm text-blue-300 hover:bg-blue-500/10 disabled:opacity-50"
        >
          {isLoading ? tr('对比中...', 'Comparing...') : tr('开始对比', 'Compare')}
        </button>
      </div>

      <div className="px-6 py-2 border-b border-[#30363d] bg-[#0d1117] text-xs text-gray-500">
        {tr('源库：', 'Source: ')}
        {selectedSource?.name || sourceDbId || '-'}
        {'  |  '}
        {tr('目标库：', 'Target: ')}
        {selectedTarget?.name || targetDbId || '-'}
      </div>

      {errorText && (
        <div className="mx-6 mt-4 border border-red-500/30 bg-red-500/10 rounded p-3 text-sm text-red-200 whitespace-pre-wrap">{errorText}</div>
      )}

      {diff && (
        <div className="mx-6 mt-4 grid grid-cols-5 gap-2">
          <div className="border border-[#30363d] rounded p-2 bg-[#0d1117] text-center">
            <div className="text-xs text-gray-500">Total</div>
            <div className="text-sm text-gray-200 font-bold">{summary.total}</div>
          </div>
          <div className="border border-green-500/20 rounded p-2 bg-green-500/5 text-center">
            <div className="text-xs text-green-300">{tr('新增', 'Added')}</div>
            <div className="text-sm text-green-300 font-bold">{summary.added}</div>
          </div>
          <div className="border border-red-500/20 rounded p-2 bg-red-500/5 text-center">
            <div className="text-xs text-red-300">{tr('删除', 'Removed')}</div>
            <div className="text-sm text-red-300 font-bold">{summary.removed}</div>
          </div>
          <div className="border border-blue-500/20 rounded p-2 bg-blue-500/5 text-center">
            <div className="text-xs text-blue-300">{tr('变更', 'Modified')}</div>
            <div className="text-sm text-blue-300 font-bold">{summary.modified}</div>
          </div>
          <div className="border border-[#30363d] rounded p-2 bg-[#0d1117] text-center">
            <div className="text-xs text-gray-500">{tr('一致', 'Unchanged')}</div>
            <div className="text-sm text-gray-200 font-bold">{summary.unchanged}</div>
          </div>
        </div>
      )}

      <div className="flex-1 min-h-0 px-6 py-4 overflow-y-auto">
        {!diff && !isLoading && !errorText && (
          <div className="h-full flex items-center justify-center text-sm text-gray-500">
            {tr('选择源库/目标库后点击“开始对比”。', 'Select source/target and click Compare.')}
          </div>
        )}
        {diff && <DiffViewer diff={diff} />}
      </div>
    </div>
  );
}
