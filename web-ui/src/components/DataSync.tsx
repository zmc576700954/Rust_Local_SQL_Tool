import { useState, useEffect, useRef } from 'react';
import { StepWizard } from './StepWizard';
import type { WizardStep } from './StepWizard';
import { api } from '../api';
import { useToast } from './Toast';
import { parseError } from '../utils';
import type { DataDiff, DataSyncJobStatus, DataSyncStrategy } from '../types';
import { dbLevelDisplayName, dbTypeDisplayName } from '../utils/dbCapabilities'
import { tr } from '../i18n';
import { Button } from './ui';

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
  // PostgreSQL 分阶段数据同步（与 MySQL 单表 compare/preview/deploy 流程并行）。
  const [pgTables, setPgTables] = useState('');
  const [pgBatchSize, setPgBatchSize] = useState('1000');
  const [pgCreateMissing, setPgCreateMissing] = useState(true);
  const [pgMirrorConfirmed, setPgMirrorConfirmed] = useState(false);
  const [pgSingleTransaction, setPgSingleTransaction] = useState(false);
  const [pgDeferConstraints, setPgDeferConstraints] = useState(false);
  const [pgAllowNoPk, setPgAllowNoPk] = useState(false);
  const [pgResume, setPgResume] = useState(false);
  const [pgJob, setPgJob] = useState<any | null>(null);
  const pgTimerRef = useRef<number | null>(null);
  const pgCancelledRef = useRef(false);
  const sourceConn = dbConnections.find((c) => String(c.id) === sourceDbId);
  const targetConn = dbConnections.find((c) => String(c.id) === targetDbId);
  const isPgPair =
    sourceDbId.length > 0 &&
    targetDbId.length > 0 &&
    sourceConn?.db_type === 'postgresql' &&
    targetConn?.db_type === 'postgresql';
  // MySQL / MariaDB 已迁移到与 PG 同一套分阶段流水线；两族不能混用。
  const isMySqlPair =
    sourceDbId.length > 0 &&
    targetDbId.length > 0 &&
    (sourceConn?.db_type === 'mysql' || sourceConn?.db_type === 'mariadb') &&
    (targetConn?.db_type === 'mysql' || targetConn?.db_type === 'mariadb');
  const isStagedPair = isPgPair || isMySqlPair;
  const recordPreview = diffs[0]
    ? {
        inserts: (diffs[0].inserts || []).slice(0, 5),
        updates: (diffs[0].updates || []).slice(0, 5),
        deletes: (diffs[0].deletes || []).slice(0, 5),
      }
    : null;

  useEffect(() => {
    const fetchConfig = async () => {
      try {
        const config = await api.getConfig();
        setDbConnections(config.db_connections || []);
        if (config.active_db_id) {
          setTargetDbId(config.active_db_id);
        }
      } catch (e: unknown) {
        toast(tr('加载数据库连接失败：', 'Failed to load db connections: ') + (e instanceof Error ? e.message : String(e)), 'error');
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
      pgCancelledRef.current = true;
      if (pgTimerRef.current) {
        window.clearTimeout(pgTimerRef.current);
        pgTimerRef.current = null;
      }
    };
  }, []);

  // 分阶段模式默认 upsert；mirror 必须由用户显式选择并二次确认。
  useEffect(() => {
    if (isStagedPair) setStrategy('upsert_only');
    // MySQL/MariaDB 的 DDL 会隐式提交，单事务不可用，切过去时强制关掉。
    if (isMySqlPair) setPgSingleTransaction(false);
  }, [isStagedPair, isMySqlPair]);

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
    setPgJob(null);
    setPgMirrorConfirmed(false);
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

  const pollPgJob = async (job_id: string): Promise<any | null> => {
    for (;;) {
      if (pgCancelledRef.current) return null;
      const raw = await api.pgDataSyncJobStatus(job_id);
      setPgJob(raw);
      const status = String(raw?.status || '').toLowerCase();
      if (status === 'completed' || status === 'error' || status === 'failed') return raw;
      await new Promise<void>((resolve) => {
        pgTimerRef.current = window.setTimeout(resolve, 1200);
      });
    }
  };

  const handlePgExecute = async () => {
    if (!sourceDbId || !targetDbId || sourceDbId === targetDbId) {
      toast(tr('请选择不同的源库与目标库。', 'Choose different source and target databases.'), 'info');
      return;
    }
    if (strategy === 'mirror' && !pgMirrorConfirmed) {
      toast(
        tr('mirror 会删除目标库中源库不存在的数据，请先勾选确认。', 'Mirror deletes target rows missing from the source; confirm it first.'),
        'info'
      );
      return;
    }
    if (pgSingleTransaction && pgResume) {
      toast(tr('单事务与断点续跑互斥，请二选一。', 'Single transaction and resume are mutually exclusive.'), 'info');
      return;
    }
    if (isMySqlPair && pgSingleTransaction) {
      toast(
        tr(
          'MySQL/MariaDB 的建表/建索引/AUTO_INCREMENT 对齐都会隐式提交，单事务无法覆盖整个作业，请关闭单事务。',
          'MySQL/MariaDB DDL (CREATE TABLE/INDEX, AUTO_INCREMENT) implicitly commits, so a job-wide single transaction is unavailable.'
        ),
        'info'
      );
      return;
    }

    setIsLoading(true);
    setErrorObj(null);
    setPgJob(null);
    pgCancelledRef.current = false;
    try {
      const tables = pgTables
        .split(',')
        .map((t) => t.trim())
        .filter(Boolean);
      const batch = Number(pgBatchSize);
      const res = await api.pgDataSyncRun({
        source_db_id: sourceDbId,
        target_db_id: targetDbId,
        tables: tables.length > 0 ? tables : undefined,
        mode: strategy,
        batch_size: Number.isFinite(batch) && batch > 0 ? Math.floor(batch) : undefined,
        create_missing_tables: pgCreateMissing,
        mirror_confirmed: strategy === 'mirror' ? pgMirrorConfirmed : undefined,
        single_transaction: isMySqlPair ? false : pgSingleTransaction,
        defer_constraints: pgDeferConstraints,
        allow_no_pk: pgAllowNoPk,
        resume: pgResume,
      });
      const pgJobId = String(res?.job_id || '');
      if (!pgJobId) throw new Error('Missing job_id');
      const final = await pollPgJob(pgJobId);
      if (!final) return;
      const failed =
        String(final?.status || '').toLowerCase() === 'error' ||
        !!final?.error ||
        final?.report?.verification_status === 'failed' ||
        !!final?.report?.error;
      if (failed) {
        const msg = String(final?.error || final?.report?.error || '数据同步失败');
        setErrorObj({ title: tr('数据同步失败', 'Data sync failed'), message: msg });
        toast(tr('数据同步失败：', 'Data sync failed: ') + msg, 'error');
      } else {
        toast(tr('数据同步完成。', 'Data sync completed.'), 'success');
      }
    } catch (e: unknown) {
      const msg =
        (e as any)?.response?.data?.message ||
        (e as any)?.error?.message ||
        (e instanceof Error ? e.message : String(e));
      setErrorObj({ title: tr('数据同步失败', 'Data sync failed'), message: msg });
      toast(tr('数据同步失败：', 'Data sync failed: ') + msg, 'error');
    } finally {
      setIsLoading(false);
    }
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
      <div className={`border rounded-lg p-3 ${isError ? 'border-red-500/30 bg-red-500/10' : 'border-dark-border bg-dark-bg'}`}>
        <div className="flex items-center justify-between gap-4">
          <div className="text-sm font-medium text-gray-200">{title}</div>
          <div className={`text-xs ${isError ? 'text-red-400' : 'text-gray-400'}`}>{statusLabel}</div>
        </div>
        {(job.message || progress !== undefined) && (
          <div className="mt-2 flex flex-col gap-2">
            {typeof progress === 'number' && (
              <div className="w-full h-2 bg-dark-surface rounded overflow-hidden border border-dark-border">
                <div className="h-full bg-blue-500/60" style={{ width: `${progress}%` }} />
              </div>
            )}
            {job.message && <div className="text-xs text-gray-400">{job.message}</div>}
          </div>
        )}
        {job.status === 'failed' && !!job.error && (
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
      } catch (e: unknown) {
        if (isNotFound(e)) {
          const diffResult = await api.syncDataDiff(tableName, sourceDbId, targetDbId, primaryKey);
          applyDiffResult(diffResult);
          setCompareJob({ job_id: 'inline', phase: 'compare', status: 'succeeded', progress: 100 });
          toast('对比完成。', 'success');
          return true;
        }
        throw e;
      }
    } catch (e: unknown) {
      const msg = (e as any)?.response?.data?.message || (e as any)?.error?.message || (e as any)?.error_message || (e instanceof Error ? e.message : String(e));
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
      } catch (e: unknown) {
        if (isNotFound(e)) {
          const dmlResult = await api.syncDataDml(diffs as any[], safeSelections, primaryKey);
          setDml(dmlResult);
          setPreviewJob({ job_id: 'inline', phase: 'preview', status: 'succeeded', progress: 100 });
          toast('预览生成完成。', 'success');
          return true;
        }
        throw e;
      }
    } catch (e: unknown) {
      const msg = (e as any)?.response?.data?.message || (e as any)?.error?.message || (e as any)?.error_message || (e instanceof Error ? e.message : String(e));
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
        await api.executeSql(dml, true, targetDbId);
        setDeployJob({ job_id: 'inline', phase: 'deploy', status: 'succeeded', progress: 100 });
        toast('部署完成，数据已同步。', 'success');
        onCancel();
        return;
      } catch (e: unknown) {
        if (isNotFound(e)) {
          await api.executeSql(dml, true, targetDbId);
          setDeployJob({ job_id: 'inline', phase: 'deploy', status: 'succeeded', progress: 100 });
          toast('部署完成，数据已同步。', 'success');
          onCancel();
          return;
        }
        throw e;
      }
    } catch (e: unknown) {
      const msg = (e as any)?.response?.data?.message || (e as any)?.error?.message || (e as any)?.error_message || (e instanceof Error ? e.message : String(e));
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

  const mysqlSteps: WizardStep[] = [
    {
      id: 'source',
      title: tr('配置与对比', 'Config & Compare'),
      isValid: !!compareJob && compareJob.status === 'succeeded' && diffs.length > 0,
      content: (
        <div className="flex flex-col gap-4 h-full">
          <div className="text-sm text-gray-300 font-bold">{tr('步骤 1：选择源库/目标库与同步策略，然后发起对比', 'Step 1: Choose source/target and strategy, then compare')}</div>
          {errorObj && (
            <div className="bg-red-500/10 border border-red-500/30 text-red-200 p-4 rounded-lg">
              <div className="font-bold mb-1">{errorObj.title}</div>
              <div className="text-sm opacity-90 whitespace-pre-wrap">{errorObj.message}</div>
            </div>
          )}
          <div className="flex gap-4">
            <div className="flex-1">
              <label className="block text-xs text-gray-400 mb-1">{tr('目标表名', 'Target Table')}</label>
              <input
                value={tableName}
                onChange={(e) => setTableName(e.target.value)}
                className="w-full bg-dark-bg border border-dark-border rounded px-3 py-2 text-sm text-gray-300 outline-none focus:border-blue-500"
                placeholder={tr('例如 users', 'e.g. users')}
              />
            </div>
            <div className="flex-1">
              <label className="block text-xs text-gray-400 mb-1">{tr('主键列', 'Primary Key')}</label>
              <input
                value={primaryKey}
                onChange={(e) => setPrimaryKey(e.target.value)}
                className="w-full bg-dark-bg border border-dark-border rounded px-3 py-2 text-sm text-gray-300 outline-none focus:border-blue-500"
                placeholder={tr('例如 id', 'e.g. id')}
              />
            </div>
          </div>
          <div className="flex flex-col gap-2">
            <label className="text-sm text-gray-400">{tr('同步策略', 'Strategy')}</label>
            <div className="flex flex-wrap gap-3">
              <label className={`flex items-center gap-2 px-3 py-2 rounded border cursor-pointer transition-colors ${strategy === 'mirror' ? 'border-blue-500/60 bg-blue-500/10 text-blue-300' : 'border-dark-border bg-dark-bg text-gray-300 hover:bg-dark-surface'}`}>
                <input
                  type="radio"
                  checked={strategy === 'mirror'}
                  onChange={() => {
                    resetRunState();
                    setStrategy('mirror');
                  }}
                />
                <div className="flex flex-col">
                  <span className="text-sm font-medium">{tr('镜像（同步删除）', 'Mirror (with deletes)')}</span>
                  <span className="text-xs opacity-80">INSERT / UPDATE / DELETE</span>
                </div>
              </label>
              <label className={`flex items-center gap-2 px-3 py-2 rounded border cursor-pointer transition-colors ${strategy === 'upsert_only' ? 'border-blue-500/60 bg-blue-500/10 text-blue-300' : 'border-dark-border bg-dark-bg text-gray-300 hover:bg-dark-surface'}`}>
                <input
                  type="radio"
                  checked={strategy === 'upsert_only'}
                  onChange={() => {
                    resetRunState();
                    setStrategy('upsert_only');
                  }}
                />
                <div className="flex flex-col">
                  <span className="text-sm font-medium">{tr('仅 Upsert（不删除）', 'Upsert Only (no deletes)')}</span>
                  <span className="text-xs opacity-80">只做 INSERT / UPDATE，不删除</span>
                </div>
              </label>
            </div>
          </div>
          <div className="flex flex-col gap-2">
            <label className="text-sm text-gray-400">{tr('源库', 'Source')}</label>
            <select
              value={sourceDbId}
              onChange={(e) => {
                resetRunState();
                setSourceDbId(e.target.value);
              }}
              className="bg-dark-bg border border-dark-border rounded p-2 text-sm text-gray-300 outline-none focus:border-blue-500"
            >
              <option value="">{tr('-- 选择源库 --', '-- Select Source --')}</option>
              {dbConnections.map(conn => (
                <option key={conn.id} value={conn.id}>
                  {conn.name} ({dbTypeDisplayName(conn.db_type)}/{dbLevelDisplayName(conn.capability_level)}) ({conn.url})
                </option>
              ))}
            </select>
          </div>
          <div className="flex flex-col gap-2">
            <label className="text-sm text-gray-400">{tr('目标库', 'Target')}</label>
            <select
              value={targetDbId}
              onChange={(e) => {
                resetRunState();
                setTargetDbId(e.target.value);
              }}
              className="bg-dark-bg border border-dark-border rounded p-2 text-sm text-gray-300 outline-none focus:border-blue-500"
            >
              <option value="">{tr('-- 选择目标库 --', '-- Select Target --')}</option>
              {dbConnections.map(conn => (
                <option key={conn.id} value={conn.id}>
                  {conn.name} ({dbTypeDisplayName(conn.db_type)}/{dbLevelDisplayName(conn.capability_level)}) ({conn.url})
                </option>
              ))}
            </select>
          </div>
          {renderJobCard(compareJob, tr('对比作业', 'Compare Job'))}
          <Button
            variant="secondary" size="md"
            onClick={async () => {
              if (await handleCompare()) {
                toast(tr('对比完成，请点击下一步。', 'Comparison complete. Click Next.'), 'success');
              }
            }}
            disabled={isLoading}
            className="self-start mt-4 disabled:opacity-50"
          >
            {tr('开始对比', 'Compare')}
          </Button>
        </div>
      )
    },
    {
      id: 'diff',
      title: tr('选择与预览', 'Select & Preview'),
      isValid: !!previewJob && previewJob.status === 'succeeded' && dml.length > 0,
      content: (
        <div className="flex flex-col gap-4 h-full">
          <div className="text-sm text-gray-300 font-bold">{tr('步骤 2：选择本次要应用的变更，然后生成预览', 'Step 2: Select changes and generate preview')}</div>
          {errorObj && (
            <div className="bg-red-500/10 border border-red-500/30 text-red-200 p-4 rounded-lg">
              <div className="font-bold mb-1">{errorObj.title}</div>
              <div className="text-sm opacity-90 whitespace-pre-wrap">{errorObj.message}</div>
            </div>
          )}
          {renderJobCard(previewJob, tr('预览作业', 'Preview Job'))}
          {recordPreview && (
            <div className="text-xs text-gray-500 border border-dark-border rounded p-3 bg-dark-bg">
              {tr('记录变更样本：', 'Record change samples:')}
              <div>{tr(`新增样本 ${recordPreview.inserts.length} 条`, `${recordPreview.inserts.length} insert samples`)}</div>
              <div>{tr(`修改样本 ${recordPreview.updates.length} 条`, `${recordPreview.updates.length} update samples`)}</div>
              <div>{tr(`删除样本 ${recordPreview.deletes.length} 条`, `${recordPreview.deletes.length} delete samples`)}</div>
            </div>
          )}
          <div className="flex-1 overflow-y-auto bg-dark-bg border border-dark-border rounded p-4">
            {diffs.length === 0 ? (
              <div className="text-gray-500 text-sm">{tr('暂无对比结果，请返回上一步先执行对比。', 'No compare result, please go back and run compare.')}</div>
            ) : (
              <div className="flex flex-col gap-6">
                {diffs.map((diff) => (
                  <div key={diff.table_name} className="flex flex-col gap-2 border border-dark-border rounded-lg overflow-hidden">
                    <div className="bg-dark-surface px-4 py-2 font-bold text-gray-200 border-b border-dark-border">
                      {diff.table_name}
                    </div>
                    <div className="p-4 flex flex-col gap-3">
                      {diff.insert_count > 0 && (
                        <label className="flex items-center gap-3 p-2 hover:bg-dark-surface rounded cursor-pointer transition-colors">
                          <input
                            type="checkbox"
                            checked={(selections[diff.table_name] || []).includes('insert')}
                            onChange={() => handleOpToggle(diff.table_name, 'insert')}
                            className="w-4 h-4 rounded border-gray-600 bg-gray-700 text-blue-500 focus:ring-blue-500 focus:ring-offset-gray-800"
                          />
                          <span className="text-sm text-gray-300 font-medium w-24">INSERT</span>
                          <span className="text-xs bg-green-500/20 text-green-400 px-2 py-0.5 rounded">{tr(`${diff.insert_count} 行`, `${diff.insert_count} rows`)}</span>
                        </label>
                      )}
                      {diff.update_count > 0 && (
                        <label className="flex items-center gap-3 p-2 hover:bg-dark-surface rounded cursor-pointer transition-colors">
                          <input
                            type="checkbox"
                            checked={(selections[diff.table_name] || []).includes('update')}
                            onChange={() => handleOpToggle(diff.table_name, 'update')}
                            className="w-4 h-4 rounded border-gray-600 bg-gray-700 text-blue-500 focus:ring-blue-500 focus:ring-offset-gray-800"
                          />
                          <span className="text-sm text-gray-300 font-medium w-24">UPDATE</span>
                          <span className="text-xs bg-yellow-500/20 text-yellow-400 px-2 py-0.5 rounded">{tr(`${diff.update_count} 行`, `${diff.update_count} rows`)}</span>
                        </label>
                      )}
                      {(diff.delete_count > 0 || strategy === 'upsert_only') && (
                        <label className={`flex items-center gap-3 p-2 rounded ${strategy === 'upsert_only' ? 'opacity-60 cursor-not-allowed' : 'hover:bg-dark-surface cursor-pointer'}`}>
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
                          <span className="text-xs bg-red-500/20 text-red-400 px-2 py-0.5 rounded">{tr(`${diff.delete_count} 行`, `${diff.delete_count} rows`)}</span>
                          {strategy === 'upsert_only' && (
                            <span className="text-xs text-gray-500 ml-2">{tr('仅 Upsert 策略下不允许删除', 'Delete is not allowed in upsert-only mode')}</span>
                          )}
                        </label>
                      )}
                      {diff.insert_count === 0 && diff.update_count === 0 && diff.delete_count === 0 && (
                        <div className="text-gray-500 text-sm italic">{tr('数据已一致，无需同步。', 'Data is already consistent.')}</div>
                      )}
                    </div>
                  </div>
                ))}
              </div>
            )}
          </div>
          <Button
            variant="secondary" size="md"
            onClick={async () => {
              if (await handleGenerateDml()) {
                toast(tr('预览已生成，请点击下一步。', 'Preview generated. Click Next.'), 'success');
              }
            }}
            disabled={Object.values(selections).every(ops => ops.length === 0)}
            className="self-start disabled:opacity-50"
          >
            {tr('生成预览', 'Generate Preview')}
          </Button>
        </div>
      )
    },
    {
      id: 'preview',
      title: tr('预览与部署', 'Preview & Deploy'),
      isValid: dml.length > 0,
      content: (
        <div className="flex flex-col gap-4 h-full">
          <div className="text-sm text-gray-300 font-bold">{tr('步骤 3：确认预览 SQL，点击执行发起部署作业', 'Step 3: Confirm SQL and execute deploy')}</div>
          {errorObj && (
            <div className="bg-red-500/10 border border-red-500/30 text-red-200 p-4 rounded-lg">
              <div className="font-bold mb-1">{errorObj.title}</div>
              <div className="text-sm opacity-90 whitespace-pre-wrap">{errorObj.message}</div>
            </div>
          )}
          {renderJobCard(deployJob, tr('部署作业', 'Deploy Job'))}
          <textarea
            readOnly
            value={dml}
            className="flex-1 bg-dark-bg border border-dark-border rounded p-4 font-mono text-sm text-gray-300 outline-none resize-none"
          />
        </div>
      )
    }
  ];

  const pgJobStatus = String(pgJob?.status || '').toLowerCase();
  const pgReport = pgJob?.report;
  const pgSteps: WizardStep[] = [
    {
      id: 'pg-config',
      title: tr('连接与选项', 'Connections & Options'),
      isValid:
        sourceDbId.length > 0 &&
        targetDbId.length > 0 &&
        sourceDbId !== targetDbId &&
        (strategy !== 'mirror' || pgMirrorConfirmed),
      content: (
        <div className="flex flex-col gap-4 h-full">
          <div className="text-sm text-gray-300 font-bold">
            {tr('步骤 1：选择 PostgreSQL 源库/目标库并配置同步选项（默认 upsert）', 'Step 1: choose PostgreSQL source/target and configure options (upsert by default)')}
          </div>
          {errorObj && (
            <div className="bg-red-500/10 border border-red-500/30 text-red-200 p-4 rounded-lg">
              <div className="font-bold mb-1">{errorObj.title}</div>
              <div className="text-sm opacity-90 whitespace-pre-wrap">{errorObj.message}</div>
            </div>
          )}
          <div className="flex gap-4">
            <div className="flex-1">
              <label className="block text-xs text-gray-400 mb-1">{tr('源库', 'Source')}</label>
              <select
                value={sourceDbId}
                onChange={(e) => {
                  resetRunState();
                  setSourceDbId(e.target.value);
                }}
                className="w-full bg-dark-bg border border-dark-border rounded p-2 text-sm text-gray-300 outline-none focus:border-blue-500"
              >
                <option value="">{tr('-- 选择源库 --', '-- Select Source --')}</option>
                {dbConnections.map(conn => (
                  <option key={conn.id} value={conn.id}>
                    {conn.name} ({dbTypeDisplayName(conn.db_type)}/{dbLevelDisplayName(conn.capability_level)}) ({conn.url})
                  </option>
                ))}
              </select>
            </div>
            <div className="flex-1">
              <label className="block text-xs text-gray-400 mb-1">{tr('目标库', 'Target')}</label>
              <select
                value={targetDbId}
                onChange={(e) => {
                  resetRunState();
                  setTargetDbId(e.target.value);
                }}
                className="w-full bg-dark-bg border border-dark-border rounded p-2 text-sm text-gray-300 outline-none focus:border-blue-500"
              >
                <option value="">{tr('-- 选择目标库 --', '-- Select Target --')}</option>
                {dbConnections.map(conn => (
                  <option key={conn.id} value={conn.id}>
                    {conn.name} ({dbTypeDisplayName(conn.db_type)}/{dbLevelDisplayName(conn.capability_level)}) ({conn.url})
                  </option>
                ))}
              </select>
            </div>
          </div>
          <div className="flex flex-col gap-2">
            <label className="text-sm text-gray-400">{tr('同步策略', 'Strategy')}</label>
            <div className="flex flex-wrap gap-3">
              <label className={`flex items-center gap-2 px-3 py-2 rounded border cursor-pointer transition-colors ${strategy === 'upsert_only' ? 'border-blue-500/60 bg-blue-500/10 text-blue-300' : 'border-dark-border bg-dark-bg text-gray-300 hover:bg-dark-surface'}`}>
                <input
                  type="radio"
                  checked={strategy === 'upsert_only'}
                  onChange={() => {
                    resetRunState();
                    setStrategy('upsert_only');
                  }}
                />
                <div className="flex flex-col">
                  <span className="text-sm font-medium">{tr('仅 Upsert（默认，不删除）', 'Upsert Only (default, no deletes)')}</span>
                  <span className="text-xs opacity-80">INSERT / UPDATE</span>
                </div>
              </label>
              <label className={`flex items-center gap-2 px-3 py-2 rounded border cursor-pointer transition-colors ${strategy === 'mirror' ? 'border-blue-500/60 bg-blue-500/10 text-blue-300' : 'border-dark-border bg-dark-bg text-gray-300 hover:bg-dark-surface'}`}>
                <input
                  type="radio"
                  checked={strategy === 'mirror'}
                  onChange={() => {
                    resetRunState();
                    setStrategy('mirror');
                  }}
                />
                <div className="flex flex-col">
                  <span className="text-sm font-medium">{tr('镜像（同步删除）', 'Mirror (with deletes)')}</span>
                  <span className="text-xs opacity-80">INSERT / UPDATE / DELETE</span>
                </div>
              </label>
            </div>
          </div>
          {strategy === 'mirror' && (
            <div className="border border-red-500/40 bg-red-500/10 rounded p-3 flex flex-col gap-2">
              <div className="text-xs text-red-200">
                {tr('mirror 会删除目标库中源库不存在的行，该操作不可撤销。', 'Mirror deletes target rows missing from the source and cannot be undone.')}
              </div>
              <label className="flex items-center gap-2 text-sm text-red-200">
                <input
                  type="checkbox"
                  checked={pgMirrorConfirmed}
                  onChange={(e) => setPgMirrorConfirmed(e.target.checked)}
                />
                {tr('我确认执行 mirror 的删除语义', 'I confirm the delete semantics of mirror mode')}
              </label>
            </div>
          )}
          <div className="flex gap-4">
            <div className="flex-1">
              <label className="block text-xs text-gray-400 mb-1">{tr('表名（可选，逗号分隔；留空同步默认 schema 下全部表）', 'Tables (optional, comma separated; empty = all tables in default schema)')}</label>
              <input
                value={pgTables}
                onChange={(e) => setPgTables(e.target.value)}
                className="w-full bg-dark-bg border border-dark-border rounded px-3 py-2 text-sm text-gray-300 outline-none focus:border-blue-500"
                placeholder={tr('例如 users,orders', 'e.g. users,orders')}
              />
            </div>
            <div className="w-40">
              <label className="block text-xs text-gray-400 mb-1">{tr('批次大小', 'Batch size')}</label>
              <input
                value={pgBatchSize}
                onChange={(e) => setPgBatchSize(e.target.value)}
                className="w-full bg-dark-bg border border-dark-border rounded px-3 py-2 text-sm text-gray-300 outline-none focus:border-blue-500"
                placeholder="1000"
              />
            </div>
          </div>
          <div className="flex flex-col gap-2 border border-dark-border rounded p-3 bg-dark-bg">
            <label className="flex items-center gap-2 text-sm text-gray-300">
              <input type="checkbox" checked={pgCreateMissing} onChange={(e) => setPgCreateMissing(e.target.checked)} />
              {tr('目标库缺失表时自动创建', 'Create missing target tables')}
            </label>
            <label className={`flex items-center gap-2 text-sm ${isMySqlPair ? 'text-gray-500' : 'text-gray-300'}`}>
              <input
                type="checkbox"
                checked={isMySqlPair ? false : pgSingleTransaction}
                disabled={isMySqlPair}
                onChange={(e) => {
                  setPgSingleTransaction(e.target.checked);
                  if (e.target.checked) setPgResume(false);
                }}
              />
              {isMySqlPair
                ? tr('单事务（MySQL/MariaDB 不支持：DDL 隐式提交）', 'Single transaction (unavailable: MySQL/MariaDB DDL implicitly commits)')
                : tr('单事务（不可续跑）', 'Single transaction (not resumable)')}
            </label>
            <label className="flex items-center gap-2 text-sm text-gray-300">
              <input
                type="checkbox"
                checked={pgResume}
                onChange={(e) => {
                  setPgResume(e.target.checked);
                  if (e.target.checked) setPgSingleTransaction(false);
                }}
              />
              {tr('断点续跑（读取 data-sync/<job_key>.json）', 'Resume from checkpoint (data-sync/<job_key>.json)')}
            </label>
            <label className="flex items-center gap-2 text-sm text-gray-300">
              <input type="checkbox" checked={pgDeferConstraints} onChange={(e) => setPgDeferConstraints(e.target.checked)} />
              {isMySqlPair
                ? tr('外键环处理：数据灌完后再建外键（不使用 SET FOREIGN_KEY_CHECKS=0）', 'FK cycles: create FKs after loading data (never SET FOREIGN_KEY_CHECKS=0)')
                : tr('延迟外键约束（外键成环时使用 session_replication_role=replica，需要超级用户）', 'Defer constraints (cycle handling via session_replication_role=replica; requires superuser)')}
            </label>
            <label className="flex items-center gap-2 text-sm text-gray-300">
              <input type="checkbox" checked={pgAllowNoPk} onChange={(e) => setPgAllowNoPk(e.target.checked)} />
              {tr('允许无主键表（整表复制，不可续跑）', 'Allow tables without a primary key (full copy, not resumable)')}
            </label>
          </div>
        </div>
      )
    },
    {
      id: 'pg-run',
      title: tr('确认与执行', 'Confirm & Run'),
      isValid: !isLoading && (!pgJob || pgJobStatus === 'error'),
      content: (
        <div className="flex flex-col gap-4 h-full">
          <div className="text-sm text-gray-300 font-bold">
            {tr('步骤 2：确认后执行分阶段同步（Plan → 建结构 → 灌数据 → 索引/外键 → 序列对齐 → 校验）', 'Step 2: run the staged pipeline (Plan → structure → data → indexes/FKs → sequences → verify)')}
          </div>
          <div className="text-xs text-gray-400 border border-dark-border rounded p-3 bg-dark-bg flex flex-col gap-1">
            <div>{tr('源库：', 'Source: ')}{sourceConn?.name || sourceDbId}</div>
            <div>{tr('目标库：', 'Target: ')}{targetConn?.name || targetDbId}</div>
            <div>{tr('策略：', 'Strategy: ')}{strategy === 'mirror' ? tr('镜像（含删除）', 'Mirror (with deletes)') : tr('仅 Upsert', 'Upsert only')}</div>
            <div>{tr('表范围：', 'Tables: ')}{pgTables.trim() || tr('默认 schema 下全部表', 'all tables in default schema')}</div>
            <div>{tr('单事务：', 'Single transaction: ')}{pgSingleTransaction ? tr('是（不可续跑）', 'yes (not resumable)') : tr('否', 'no')}</div>
            <div>{tr('断点续跑：', 'Resume: ')}{pgResume ? tr('是', 'yes') : tr('否', 'no')}</div>
          </div>
          {errorObj && (
            <div className="bg-red-500/10 border border-red-500/30 text-red-200 p-4 rounded-lg">
              <div className="font-bold mb-1">{errorObj.title}</div>
              <div className="text-sm opacity-90 whitespace-pre-wrap">{errorObj.message}</div>
            </div>
          )}
          {pgJob && (
            <div className={`border rounded-lg p-3 ${pgJobStatus === 'error' ? 'border-red-500/30 bg-red-500/10' : 'border-dark-border bg-dark-bg'}`}>
              <div className="flex items-center justify-between gap-4">
                <div className="text-sm font-medium text-gray-200">{tr('同步作业', 'Sync Job')}</div>
                <div className={`text-xs ${pgJobStatus === 'error' ? 'text-red-400' : 'text-gray-400'}`}>
                  {pgJob.status} / {pgJob.stage}
                </div>
              </div>
              {!!pgJob.progress?.message && <div className="mt-2 text-xs text-gray-400">{pgJob.progress.message}</div>}
              {!!pgJob.error && <div className="mt-2 text-xs text-red-300 whitespace-pre-wrap">{String(pgJob.error)}</div>}
              {pgReport && (
                <div className="mt-3 text-xs text-gray-400 flex flex-col gap-1">
                  <div>job_key：<span className="font-mono">{pgReport.job_key}</span></div>
                  <div>
                    {tr('写入行数：', 'Rows written: ')}{pgReport.total_rows}；
                    {tr('校验：', ' Verification: ')}{pgReport.verification_status}
                  </div>
                  <div>
                    atomic：{String(pgReport.atomic)}；rolled_back：{String(pgReport.rolled_back)}；
                    {tr('可续跑：', ' Resumable: ')}{String(pgReport.resumable)}
                  </div>
                  {!!pgReport.checkpoint_file && <div>checkpoint：<span className="font-mono">{pgReport.checkpoint_file}</span></div>}
                  {Array.isArray(pgReport.non_transactional_tables) && pgReport.non_transactional_tables.length > 0 && (
                    <div>{tr('非事务表：', 'Non-transactional tables: ')}{pgReport.non_transactional_tables.join(', ')}</div>
                  )}
                  {Array.isArray(pgReport.warnings) && pgReport.warnings.map((w: string, i: number) => (
                    <div key={i} className="text-yellow-300">{w}</div>
                  ))}
                </div>
              )}
              {Array.isArray(pgJob.tables) && pgJob.tables.length > 0 && (
                <div className="mt-3 overflow-x-auto">
                  <table className="text-xs text-gray-400 w-full">
                    <thead>
                      <tr className="text-left border-b border-dark-border">
                        <th className="py-1 pr-3">{tr('表', 'Table')}</th>
                        <th className="py-1 pr-3">{tr('状态', 'Status')}</th>
                        <th className="py-1 pr-3">{tr('写入', 'Rows')}</th>
                        <th className="py-1 pr-3">{tr('校验', 'Verify')}</th>
                      </tr>
                    </thead>
                    <tbody>
                      {pgJob.tables.map((t: any) => (
                        <tr key={t.table_name} className="border-b border-dark-border/50">
                          <td className="py-1 pr-3 font-mono">{t.table_name}</td>
                          <td className="py-1 pr-3">{t.status}</td>
                          <td className="py-1 pr-3">{t.rows_copied}</td>
                          <td className="py-1 pr-3">{t.verification}</td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </div>
              )}
            </div>
          )}
        </div>
      )
    }
  ];
  const steps: WizardStep[] = isStagedPair ? pgSteps : mysqlSteps;

  return (
    <StepWizard
      title={tr('数据同步', 'Data Sync')}
      steps={steps}
      onCancel={onCancel}
      onFinish={isStagedPair ? handlePgExecute : handleExecute}
      finalWarningMessage={
        isStagedPair
          ? (strategy === 'mirror'
            ? tr('mirror 模式会删除目标库中源库不存在的行，该操作不可撤销。请确认目标库选择无误。', 'Mirror mode deletes target rows missing from the source and cannot be undone. Confirm the target database.')
            : tr('即将对目标库执行分阶段数据同步，该操作会修改数据。请确认目标库选择无误。', 'You are about to run the staged data sync against the target database. This modifies data.'))
          : tr('即将对目标数据库执行部署作业并应用上述变更。该操作会修改数据且不可撤销，请确认目标库选择无误。', 'You are about to deploy changes to target database. This action is irreversible.')
      }
      isLoading={isLoading}
    />
  );
}
