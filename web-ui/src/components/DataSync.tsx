import { useState, useEffect, useRef } from 'react';
import { StepWizard } from './StepWizard';
import type { WizardStep } from './StepWizard';
import { api } from '../api';
import { useToast } from './Toast';
import { parseError } from '../utils';
import type { DataDiff, DataSyncJobStatus, DataSyncStrategy } from '../types';
import { dbLevelDisplayName, dbTypeDisplayName } from '../utils/dbCapabilities'

interface DataSyncProps {
  onCancel: () => void;
}

export function DataSync({ onCancel }: DataSyncProps) {
  const { toast } = useToast();
  const [tableName, setTableName] = useState('');
  const [primaryKey, setPrimaryKey] = useState('id');
  const [strategy, setStrategy] = useState<DataSyncStrategy>('mirror');
  const [sourceDbId, setSourceDbId] = useState('');
  const [targetDbId, setTargetDbId] = useState('');
  const [dbConnections, setDbConnections] = useState<any[]>([]);
  const [diffs, setDiffs] = useState<DataDiff[]>([]);
  // selections: table_name -> array of selected ops e.g. ["insert", "update", "delete"]
  const [selections, setSelections] = useState<Record<string, string[]>>({});
  const [dml, setDml] = useState<string>('');
  const [jobId, setJobId] = useState<string>('');
  const [compareJob, setCompareJob] = useState<DataSyncJobStatus | null>(null);
  const [previewJob, setPreviewJob] = useState<DataSyncJobStatus | null>(null);
  const [deployJob, setDeployJob] = useState<DataSyncJobStatus | null>(null);
  const [errorObj, setErrorObj] = useState<{ title: string; message: string } | null>(null);
  const [isLoading, setIsLoading] = useState(false);
  const pollTimerRef = useRef<number | null>(null);

  useEffect(() => {
    const fetchConfig = async () => {
      try {
        const config = await api.getConfig();
        setDbConnections(config.db_connections || []);
        if (config.active_db_id) {
          setTargetDbId(config.active_db_id);
        }
      } catch (e: any) {
        toast('Failed to load db connections: ' + e.message, 'error');
      }
    };
    fetchConfig();
  }, []);

  useEffect(() => {
    return () => {
      if (pollTimerRef.current) {
        window.clearInterval(pollTimerRef.current);
        pollTimerRef.current = null;
      }
    };
  }, []);

  useEffect(() => {
    if (strategy !== 'upsert_only') return;
    const next: Record<string, string[]> = {};
    for (const [k, ops] of Object.entries(selections)) {
      next[k] = ops.filter(op => op !== 'delete');
      if (next[k].length === 0) next[k] = ['insert', 'update'];
    }
    setSelections(next);
  }, [strategy]);

  const resetRunState = () => {
    if (pollTimerRef.current) {
      window.clearInterval(pollTimerRef.current);
      pollTimerRef.current = null;
    }
    setDiffs([]);
    setSelections({});
    setDml('');
    setJobId('');
    setCompareJob(null);
    setPreviewJob(null);
    setDeployJob(null);
    setErrorObj(null);
  };

  const normalizeJobStatus = (raw: any, job_id: string, phase: DataSyncJobStatus['phase']): DataSyncJobStatus => {
    const rawStatus = String(raw?.status || raw?.state || raw?.job_status || raw?.phase_status || '').toLowerCase();
    const mapStatus = (s: string) => {
      if (['queued', 'pending', 'created', 'new'].includes(s)) return 'queued';
      if (['running', 'in_progress', 'processing', 'working'].includes(s)) return 'running';
      if (['succeeded', 'success', 'done', 'completed', 'complete', 'ok', 'finished'].includes(s)) return 'succeeded';
      if (['failed', 'error', 'errored'].includes(s)) return 'failed';
      if (['canceled', 'cancelled', 'aborted'].includes(s)) return 'canceled';
      return (s as any) || 'running';
    };

    const rawProgress = raw?.progress ?? raw?.progress_pct ?? raw?.percent ?? raw?.percentage;
    let progress: number | undefined;
    if (typeof rawProgress === 'number') {
      progress = rawProgress;
    } else if (rawProgress && typeof rawProgress === 'object') {
      const cur = Number(rawProgress.current);
      const tot = Number(rawProgress.total);
      if (Number.isFinite(cur) && Number.isFinite(tot) && tot > 0) {
        progress = (cur / tot) * 100;
      }
    } else {
      const n = Number(rawProgress);
      if (Number.isFinite(n)) progress = n;
    }
    if (progress !== undefined && progress <= 1) progress = progress * 100;
    if (progress !== undefined) progress = Math.max(0, Math.min(100, progress));

    const message = raw?.progress?.message || raw?.message || raw?.msg || raw?.detail || raw?.stage || raw?.step;
    const errorMessage =
      raw?.error?.message ||
      raw?.error_message ||
      raw?.error ||
      raw?.err?.message ||
      raw?.failure_reason;
    const result = raw?.result ?? raw?.data ?? raw?.payload ?? raw?.output;

    return {
      job_id,
      phase,
      status: mapStatus(rawStatus),
      progress,
      message: message ? String(message) : undefined,
      error: errorMessage,
      result,
    };
  };

  const setPhaseJob = (phase: DataSyncJobStatus['phase'], next: DataSyncJobStatus) => {
    if (phase === 'compare') setCompareJob(next);
    if (phase === 'preview') setPreviewJob(next);
    if (phase === 'deploy') setDeployJob(next);
  };

  const pollJobUntilDone = async (job_id: string, phase: DataSyncJobStatus['phase']) => {
    if (pollTimerRef.current) {
      window.clearInterval(pollTimerRef.current);
      pollTimerRef.current = null;
    }

    return await new Promise<any>((resolve, reject) => {
      let stopped = false;

      const tick = async () => {
        if (stopped) return;
        try {
          const raw = await api.dataSyncJobStatus(job_id);
          const normalized = normalizeJobStatus(raw, job_id, phase);
          setPhaseJob(phase, normalized);
          if (normalized.status === 'succeeded') {
            stopped = true;
            if (pollTimerRef.current) window.clearInterval(pollTimerRef.current);
            pollTimerRef.current = null;
            resolve(raw);
          } else if (normalized.status === 'failed' || normalized.status === 'canceled') {
            stopped = true;
            if (pollTimerRef.current) window.clearInterval(pollTimerRef.current);
            pollTimerRef.current = null;
            reject(raw);
          }
        } catch (e) {
          stopped = true;
          if (pollTimerRef.current) window.clearInterval(pollTimerRef.current);
          pollTimerRef.current = null;
          reject(e);
        }
      };

      tick();
      pollTimerRef.current = window.setInterval(tick, 1200);
    });
  };

  const applyDiffResult = (diffResult: DataDiff) => {
    setDiffs([diffResult]);
    const ops: string[] = [];
    if (diffResult.insert_count > 0) ops.push('insert');
    if (diffResult.update_count > 0) ops.push('update');
    if (strategy === 'mirror' && diffResult.delete_count > 0) ops.push('delete');
    if (strategy === 'upsert_only' && ops.length === 0) ops.push('insert', 'update');
    setSelections({ [diffResult.table_name]: ops });
  };

  const isNotFound = (e: any) => e?.response?.status === 404;

  const renderJobCard = (job: DataSyncJobStatus | null, title: string) => {
    if (!job) return null;
    const statusLabel =
      job.status === 'queued' ? '排队中' :
      job.status === 'running' ? '执行中' :
      job.status === 'succeeded' ? '已完成' :
      job.status === 'failed' ? '失败' :
      job.status === 'canceled' ? '已取消' :
      job.status;
    const progress = typeof job.progress === 'number' ? job.progress : undefined;
    const isError = job.status === 'failed';
    return (
      <div className={`border rounded-lg p-3 ${isError ? 'border-red-500/30 bg-red-500/10' : 'border-[#30363d] bg-[#0d1117]'}`}>
        <div className="flex items-center justify-between gap-4">
          <div className="text-sm font-medium text-gray-200">{title}</div>
          <div className={`text-xs ${isError ? 'text-red-400' : 'text-gray-400'}`}>{statusLabel}</div>
        </div>
        {(job.message || progress !== undefined) && (
          <div className="mt-2 flex flex-col gap-2">
            {typeof progress === 'number' && (
              <div className="w-full h-2 bg-[#21262d] rounded overflow-hidden border border-[#30363d]">
                <div className="h-full bg-blue-500/60" style={{ width: `${progress}%` }} />
              </div>
            )}
            {job.message && <div className="text-xs text-gray-400">{job.message}</div>}
          </div>
        )}
        {job.status === 'failed' && job.error && (
          <div className="mt-2 text-xs text-red-300 whitespace-pre-wrap">{String(job.error)}</div>
        )}
      </div>
    );
  };

  const handleCompare = async () => {
    if (!tableName || !primaryKey || !sourceDbId || !targetDbId) {
      toast('请填写表名/主键，并选择源库与目标库。', 'info');
      return false;
    }
    setIsLoading(true);
    setErrorObj(null);
    setDiffs([]);
    setDml('');
    setSelections({});
    setJobId('');
    setCompareJob(null);
    setPreviewJob(null);
    setDeployJob(null);
    try {
      try {
        const res = await api.dataSyncCompareStart({
          table_name: tableName,
          source_db_id: sourceDbId,
          target_db_id: targetDbId,
          primary_key: primaryKey,
          mode: strategy,
        });

        const job_id = String(res?.job_id || res?.id || '');
        if (!job_id) throw new Error('Missing job_id');
        setCompareJob({ job_id, phase: 'compare', status: 'queued', progress: 0 });
        const finalPayload = await pollJobUntilDone(job_id, 'compare');
        setJobId(job_id);
        const placeholder: DataDiff = { table_name: tableName, insert_count: 0, update_count: 0, delete_count: 0 };
        setDiffs([placeholder]);
        setSelections({ [tableName]: strategy === 'mirror' ? ['insert', 'update', 'delete'] : ['insert', 'update'] });
        const differentChunks = finalPayload?.compare?.different_chunks ?? finalPayload?.compare?.differentChunks;
        if (typeof differentChunks === 'number') {
          toast(`对比完成：${differentChunks} 个分块存在差异。`, 'success');
        } else {
          toast('对比完成。', 'success');
        }
        return true;
      } catch (e: any) {
        if (isNotFound(e)) {
          const diffResult = await api.syncDataDiff(tableName, sourceDbId, targetDbId, primaryKey);
          applyDiffResult(diffResult);
          setCompareJob({ job_id: 'inline', phase: 'compare', status: 'succeeded', progress: 100 });
          toast('对比完成。', 'success');
          return true;
        }
        throw e;
      }
    } catch (e: any) {
      const msg = e?.error?.message || e?.error_message || e?.message;
      if (msg) {
        setErrorObj({ title: 'Compare 作业失败', message: String(msg) });
        toast('对比失败：' + String(msg), 'error');
      } else {
        const err = parseError(e);
        setErrorObj({ title: err.title, message: err.message });
        toast('对比失败：' + err.message, 'error');
      }
      return false;
    } finally {
      setIsLoading(false);
    }
  };

  const handleGenerateDml = async () => {
    if (!jobId && diffs.length === 0) {
      toast('暂无对比结果，请先执行 Compare。', 'info');
      return false;
    }
    if (Object.values(selections).every(ops => ops.length === 0)) {
      toast('请至少选择一种操作。', 'info');
      return false;
    }

    setIsLoading(true);
    setErrorObj(null);
    setDml('');
    setPreviewJob(null);
    try {
      const safeSelections: Record<string, string[]> = {};
      for (const [k, ops] of Object.entries(selections)) {
        safeSelections[k] = strategy === 'upsert_only' ? ops.filter(op => op !== 'delete') : ops;
      }

      if (!jobId) {
        const dmlResult = await api.syncDataDml(diffs as any[], safeSelections, primaryKey);
        setDml(dmlResult);
        setPreviewJob({ job_id: 'inline', phase: 'preview', status: 'succeeded', progress: 100 });
        toast('预览生成完成。', 'success');
        return true;
      }

      try {
        const res = await api.dataSyncPreviewStart({
          job_id: jobId,
          max_rows: 2000,
          actions: safeSelections[tableName] || safeSelections[diffs?.[0]?.table_name || ''] || [],
        });

        const job_id = String(res?.job_id || res?.id || jobId || '');
        if (!job_id) throw new Error('Missing job_id');
        setPreviewJob({ job_id, phase: 'preview', status: 'queued', progress: 0 });
        const finalPayload = await pollJobUntilDone(job_id, 'preview');
        const sql =
          finalPayload?.preview?.sql ||
          finalPayload?.preview?.dml_statements ||
          finalPayload?.sql ||
          finalPayload?.dml_statements;
        if (!sql) throw new Error('Preview job finished but SQL is empty');
        const diffResult = finalPayload?.preview?.diff;
        if (diffResult) applyDiffResult(diffResult);
        setDml(String(sql));
        toast('预览生成完成。', 'success');
        return true;
      } catch (e: any) {
        if (isNotFound(e)) {
          const dmlResult = await api.syncDataDml(diffs as any[], safeSelections, primaryKey);
          setDml(dmlResult);
          setPreviewJob({ job_id: 'inline', phase: 'preview', status: 'succeeded', progress: 100 });
          toast('预览生成完成。', 'success');
          return true;
        }
        throw e;
      }
    } catch (e: any) {
      const msg = e?.error?.message || e?.error_message || e?.message;
      if (msg) {
        setErrorObj({ title: 'Preview 作业失败', message: String(msg) });
        toast('预览生成失败：' + String(msg), 'error');
      } else {
        const err = parseError(e);
        setErrorObj({ title: err.title, message: err.message });
        toast('预览生成失败：' + err.message, 'error');
      }
      return false;
    } finally {
      setIsLoading(false);
    }
  };

  const handleExecute = async () => {
    if (!dml || dml === '-- No changes selected') {
      toast('没有可执行的 SQL。', 'info');
      return;
    }

    setIsLoading(true);
    setErrorObj(null);
    setDeployJob(null);
    try {
      const safeSelections: Record<string, string[]> = {};
      for (const [k, ops] of Object.entries(selections)) {
        safeSelections[k] = strategy === 'upsert_only' ? ops.filter(op => op !== 'delete') : ops;
      }

      try {
        if (jobId) {
          const res = await api.dataSyncDeployStart({ job_id: jobId });
          const job_id = String(res?.job_id || res?.id || jobId || '');
          if (!job_id) throw new Error('Missing job_id');
          setDeployJob({ job_id, phase: 'deploy', status: 'queued', progress: 0 });
          await pollJobUntilDone(job_id, 'deploy');
          toast('部署完成，数据已同步。', 'success');
          onCancel();
          return;
        }
        await api.executeSql(dml);
        setDeployJob({ job_id: 'inline', phase: 'deploy', status: 'succeeded', progress: 100 });
        toast('部署完成，数据已同步。', 'success');
        onCancel();
        return;
      } catch (e: any) {
        if (isNotFound(e)) {
          await api.executeSql(dml);
          setDeployJob({ job_id: 'inline', phase: 'deploy', status: 'succeeded', progress: 100 });
          toast('部署完成，数据已同步。', 'success');
          onCancel();
          return;
        }
        throw e;
      }
    } catch (e: any) {
      const msg = e?.error?.message || e?.error_message || e?.message;
      if (msg) {
        setErrorObj({ title: 'Deploy 作业失败', message: String(msg) });
        toast('部署失败：' + String(msg), 'error');
      } else {
        const err = parseError(e);
        setErrorObj({ title: err.title, message: err.message });
        toast('部署失败：' + err.message, 'error');
      }
      throw e;
    } finally {
      setIsLoading(false);
    }
  };

  const handleOpToggle = (table: string, op: string) => {
    const currentOps = selections[table] || [];
    if (currentOps.includes(op)) {
      setSelections({ ...selections, [table]: currentOps.filter(o => o !== op) });
    } else {
      setSelections({ ...selections, [table]: [...currentOps, op] });
    }
  };

  const steps: WizardStep[] = [
    {
      id: 'source',
      title: '配置 & 对比',
      isValid: !!compareJob && compareJob.status === 'succeeded' && diffs.length > 0,
      content: (
        <div className="flex flex-col gap-4 h-full">
          <div className="text-sm text-gray-300 font-bold">Step 1：选择源库/目标库与同步策略，然后发起对比</div>
          {errorObj && (
            <div className="bg-red-500/10 border border-red-500/30 text-red-200 p-4 rounded-lg">
              <div className="font-bold mb-1">{errorObj.title}</div>
              <div className="text-sm opacity-90 whitespace-pre-wrap">{errorObj.message}</div>
            </div>
          )}
          <div className="flex gap-4">
            <div className="flex-1">
              <label className="block text-xs text-gray-400 mb-1">目标表名</label>
              <input
                value={tableName}
                onChange={(e) => setTableName(e.target.value)}
                className="w-full bg-[#0d1117] border border-[#30363d] rounded px-3 py-2 text-sm text-gray-300 outline-none focus:border-blue-500"
                placeholder="e.g. users"
              />
            </div>
            <div className="flex-1">
              <label className="block text-xs text-gray-400 mb-1">主键列</label>
              <input
                value={primaryKey}
                onChange={(e) => setPrimaryKey(e.target.value)}
                className="w-full bg-[#0d1117] border border-[#30363d] rounded px-3 py-2 text-sm text-gray-300 outline-none focus:border-blue-500"
                placeholder="e.g. id"
              />
            </div>
          </div>
          <div className="flex flex-col gap-2">
            <label className="text-sm text-gray-400">同步策略</label>
            <div className="flex flex-wrap gap-3">
              <label className={`flex items-center gap-2 px-3 py-2 rounded border cursor-pointer transition-colors ${strategy === 'mirror' ? 'border-blue-500/60 bg-blue-500/10 text-blue-300' : 'border-[#30363d] bg-[#0d1117] text-gray-300 hover:bg-[#21262d]'}`}>
                <input
                  type="radio"
                  checked={strategy === 'mirror'}
                  onChange={() => {
                    resetRunState();
                    setStrategy('mirror');
                  }}
                />
                <div className="flex flex-col">
                  <span className="text-sm font-medium">镜像（Mirror）</span>
                  <span className="text-xs opacity-80">INSERT / UPDATE / DELETE</span>
                </div>
              </label>
              <label className={`flex items-center gap-2 px-3 py-2 rounded border cursor-pointer transition-colors ${strategy === 'upsert_only' ? 'border-blue-500/60 bg-blue-500/10 text-blue-300' : 'border-[#30363d] bg-[#0d1117] text-gray-300 hover:bg-[#21262d]'}`}>
                <input
                  type="radio"
                  checked={strategy === 'upsert_only'}
                  onChange={() => {
                    resetRunState();
                    setStrategy('upsert_only');
                  }}
                />
                <div className="flex flex-col">
                  <span className="text-sm font-medium">仅 Upsert</span>
                  <span className="text-xs opacity-80">只做 INSERT / UPDATE，不删除</span>
                </div>
              </label>
            </div>
          </div>
          <div className="flex flex-col gap-2">
            <label className="text-sm text-gray-400">源库（Source）</label>
            <select
              value={sourceDbId}
              onChange={(e) => {
                resetRunState();
                setSourceDbId(e.target.value);
              }}
              className="bg-[#0d1117] border border-[#30363d] rounded p-2 text-sm text-gray-300 outline-none focus:border-blue-500"
            >
              <option value="">-- Select Source --</option>
              {dbConnections.map(conn => (
                <option key={conn.id} value={conn.id}>
                  {conn.name} ({dbTypeDisplayName(conn.db_type)}/{dbLevelDisplayName(conn.capability_level)}) ({conn.url})
                </option>
              ))}
            </select>
          </div>
          <div className="flex flex-col gap-2">
            <label className="text-sm text-gray-400">目标库（Target）</label>
            <select
              value={targetDbId}
              onChange={(e) => {
                resetRunState();
                setTargetDbId(e.target.value);
              }}
              className="bg-[#0d1117] border border-[#30363d] rounded p-2 text-sm text-gray-300 outline-none focus:border-blue-500"
            >
              <option value="">-- Select Target --</option>
              {dbConnections.map(conn => (
                <option key={conn.id} value={conn.id}>
                  {conn.name} ({dbTypeDisplayName(conn.db_type)}/{dbLevelDisplayName(conn.capability_level)}) ({conn.url})
                </option>
              ))}
            </select>
          </div>
          {renderJobCard(compareJob, 'Compare 作业')}
          <button
            onClick={async () => {
              if (await handleCompare()) {
                toast('对比完成，请点击下一步。', 'success');
              }
            }}
            disabled={isLoading}
            className="self-start mt-4 px-4 py-2 bg-[#21262d] border border-[#30363d] rounded hover:bg-[#30363d] text-sm text-white disabled:opacity-50"
          >
            开始对比（Compare）
          </button>
        </div>
      )
    },
    {
      id: 'diff',
      title: '选择 & 预览',
      isValid: !!previewJob && previewJob.status === 'succeeded' && dml.length > 0,
      content: (
        <div className="flex flex-col gap-4 h-full">
          <div className="text-sm text-gray-300 font-bold">Step 2：选择本次要应用的变更，然后生成预览</div>
          {errorObj && (
            <div className="bg-red-500/10 border border-red-500/30 text-red-200 p-4 rounded-lg">
              <div className="font-bold mb-1">{errorObj.title}</div>
              <div className="text-sm opacity-90 whitespace-pre-wrap">{errorObj.message}</div>
            </div>
          )}
          {renderJobCard(previewJob, 'Preview 作业')}
          <div className="flex-1 overflow-y-auto bg-[#0d1117] border border-[#30363d] rounded p-4">
            {diffs.length === 0 ? (
              <div className="text-gray-500 text-sm">暂无对比结果，请返回上一步先执行 Compare。</div>
            ) : (
              <div className="flex flex-col gap-6">
                {diffs.map((diff) => (
                  <div key={diff.table_name} className="flex flex-col gap-2 border border-[#30363d] rounded-lg overflow-hidden">
                    <div className="bg-[#21262d] px-4 py-2 font-bold text-gray-200 border-b border-[#30363d]">
                      {diff.table_name}
                    </div>
                    <div className="p-4 flex flex-col gap-3">
                      {diff.insert_count > 0 && (
                        <label className="flex items-center gap-3 p-2 hover:bg-[#21262d] rounded cursor-pointer">
                          <input
                            type="checkbox"
                            checked={(selections[diff.table_name] || []).includes('insert')}
                            onChange={() => handleOpToggle(diff.table_name, 'insert')}
                            className="w-4 h-4 rounded border-gray-600 bg-gray-700 text-blue-500 focus:ring-blue-500 focus:ring-offset-gray-800"
                          />
                          <span className="text-sm text-gray-300 font-medium w-24">INSERT</span>
                          <span className="text-xs bg-green-500/20 text-green-400 px-2 py-0.5 rounded">
                            {diff.insert_count} rows
                          </span>
                        </label>
                      )}
                      {diff.update_count > 0 && (
                        <label className="flex items-center gap-3 p-2 hover:bg-[#21262d] rounded cursor-pointer">
                          <input
                            type="checkbox"
                            checked={(selections[diff.table_name] || []).includes('update')}
                            onChange={() => handleOpToggle(diff.table_name, 'update')}
                            className="w-4 h-4 rounded border-gray-600 bg-gray-700 text-blue-500 focus:ring-blue-500 focus:ring-offset-gray-800"
                          />
                          <span className="text-sm text-gray-300 font-medium w-24">UPDATE</span>
                          <span className="text-xs bg-yellow-500/20 text-yellow-400 px-2 py-0.5 rounded">
                            {diff.update_count} rows
                          </span>
                        </label>
                      )}
                      {(diff.delete_count > 0 || strategy === 'upsert_only') && (
                        <label className={`flex items-center gap-3 p-2 rounded ${strategy === 'upsert_only' ? 'opacity-60 cursor-not-allowed' : 'hover:bg-[#21262d] cursor-pointer'}`}>
                          <input
                            type="checkbox"
                            checked={(selections[diff.table_name] || []).includes('delete')}
                            onChange={() => {
                              if (strategy === 'upsert_only') return;
                              handleOpToggle(diff.table_name, 'delete');
                            }}
                            disabled={strategy === 'upsert_only'}
                            className="w-4 h-4 rounded border-gray-600 bg-gray-700 text-blue-500 focus:ring-blue-500 focus:ring-offset-gray-800 disabled:opacity-60"
                          />
                          <span className="text-sm text-gray-300 font-medium w-24">DELETE</span>
                          <span className="text-xs bg-red-500/20 text-red-400 px-2 py-0.5 rounded">
                            {diff.delete_count} rows
                          </span>
                          {strategy === 'upsert_only' && (
                            <span className="text-xs text-gray-500 ml-2">仅 Upsert 策略下不允许删除</span>
                          )}
                        </label>
                      )}
                      {diff.insert_count === 0 && diff.update_count === 0 && diff.delete_count === 0 && (
                        <div className="text-gray-500 text-sm italic">数据已一致，无需同步。</div>
                      )}
                    </div>
                  </div>
                ))}
              </div>
            )}
          </div>
          <button
            onClick={async () => {
              if (await handleGenerateDml()) {
                toast('预览已生成，请点击下一步。', 'success');
              }
            }}
            disabled={Object.values(selections).every(ops => ops.length === 0)}
            className="self-start px-4 py-2 bg-[#21262d] border border-[#30363d] rounded hover:bg-[#30363d] text-sm text-white disabled:opacity-50"
          >
            生成预览（Preview）
          </button>
        </div>
      )
    },
    {
      id: 'preview',
      title: '预览 & 部署',
      isValid: dml.length > 0,
      content: (
        <div className="flex flex-col gap-4 h-full">
          <div className="text-sm text-gray-300 font-bold">Step 3：确认预览 SQL，点击 Execute 发起 Deploy 作业</div>
          {errorObj && (
            <div className="bg-red-500/10 border border-red-500/30 text-red-200 p-4 rounded-lg">
              <div className="font-bold mb-1">{errorObj.title}</div>
              <div className="text-sm opacity-90 whitespace-pre-wrap">{errorObj.message}</div>
            </div>
          )}
          {renderJobCard(deployJob, 'Deploy 作业')}
          <textarea
            readOnly
            value={dml}
            className="flex-1 bg-[#0d1117] border border-[#30363d] rounded p-4 font-mono text-sm text-gray-300 outline-none resize-none"
          />
        </div>
      )
    }
  ];

  return (
    <StepWizard
      title="Data Sync"
      steps={steps}
      onCancel={onCancel}
      onFinish={handleExecute}
      finalWarningMessage="即将对目标数据库执行 Deploy 作业并应用上述变更。该操作会修改数据且不可撤销，请确认目标库选择无误。"
      isLoading={isLoading}
    />
  );
}
