import { useEffect, useMemo, useState } from 'react';
import { api } from '../api';
import { useToast } from './Toast';
import { StepWizard } from './StepWizard';
import type { WizardStep } from './StepWizard';
import type { DataDiff } from '../types';
import { dbLevelDisplayName, dbTypeDisplayName } from '../utils/dbCapabilities';
import { tr } from '../i18n';

interface VisualSyncWizardProps {
  onCancel: () => void;
}

type SyncMode = 'schema' | 'data';
const VISUAL_SYNC_STATE_KEY = 'tool:visual-sync:state';

export function VisualSyncWizard({ onCancel }: VisualSyncWizardProps) {
  const { toast } = useToast();
  const [isLoading, setIsLoading] = useState(false);
  const [mode, setMode] = useState<SyncMode>('schema');
  const [sourceDbId, setSourceDbId] = useState('');
  const [targetDbId, setTargetDbId] = useState('');
  const [dbConnections, setDbConnections] = useState<any[]>([]);

  const [tableName, setTableName] = useState('');
  const [primaryKey, setPrimaryKey] = useState('id');

  const [schemaDiff, setSchemaDiff] = useState<any | null>(null);
  const [selectedTables, setSelectedTables] = useState<string[]>([]);

  const [dataDiff, setDataDiff] = useState<DataDiff | null>(null);
  const [dataSelections, setDataSelections] = useState<Record<string, string[]>>({});

  const [previewSql, setPreviewSql] = useState('');
  const [errorText, setErrorText] = useState('');

  useEffect(() => {
    const loadConfig = async () => {
      setIsLoading(true);
      try {
        const config = await api.getConfig();
        const list = Array.isArray(config?.db_connections) ? config.db_connections : [];
        setDbConnections(list);
        const savedRaw = window.localStorage.getItem(VISUAL_SYNC_STATE_KEY);
        const saved = savedRaw ? JSON.parse(savedRaw) : null;
        const savedMode = String(saved?.mode || '');
        const savedSource = String(saved?.source_db_id || '');
        const savedTarget = String(saved?.target_db_id || '');
        const savedTable = String(saved?.table_name || '');
        const savedPk = String(saved?.primary_key || '');

        if (savedMode === 'schema' || savedMode === 'data') setMode(savedMode as SyncMode);
        if (savedTable) setTableName(savedTable);
        if (savedPk) setPrimaryKey(savedPk);

        const activeId = String(config?.active_db_id || '');
        const hasSource = list.some((c: any) => String(c?.id || '') === savedSource);
        const hasTarget = list.some((c: any) => String(c?.id || '') === savedTarget);
        const nextTarget = hasTarget ? savedTarget : activeId;
        if (nextTarget) setTargetDbId(nextTarget);
        const fallbackSource = list.find((c: any) => String(c?.id || '') !== (nextTarget || activeId)) || list[0];
        const nextSource = hasSource ? savedSource : String(fallbackSource?.id || '');
        if (nextSource) setSourceDbId(nextSource);
      } catch (e: any) {
        toast(tr('加载连接失败：', 'Failed to load connections: ') + (e?.message || String(e)), 'error');
      } finally {
        setIsLoading(false);
      }
    };
    loadConfig();
  }, [toast]);

  useEffect(() => {
    window.localStorage.setItem(
      VISUAL_SYNC_STATE_KEY,
      JSON.stringify({
        mode,
        source_db_id: sourceDbId,
        target_db_id: targetDbId,
        table_name: tableName,
        primary_key: primaryKey,
      })
    );
  }, [mode, sourceDbId, targetDbId, tableName, primaryKey]);

  const resetResult = () => {
    setSchemaDiff(null);
    setSelectedTables([]);
    setDataDiff(null);
    setDataSelections({});
    setPreviewSql('');
    setErrorText('');
  };

  const sourceConn = useMemo(
    () => dbConnections.find((c) => String(c.id) === String(sourceDbId)),
    [dbConnections, sourceDbId]
  );
  const targetConn = useMemo(
    () => dbConnections.find((c) => String(c.id) === String(targetDbId)),
    [dbConnections, targetDbId]
  );

  const canCompare =
    sourceDbId.length > 0 &&
    targetDbId.length > 0 &&
    sourceDbId !== targetDbId &&
    (mode === 'schema' || (tableName.trim().length > 0 && primaryKey.trim().length > 0));

  const compareResultReady =
    (mode === 'schema' && !!schemaDiff) ||
    (mode === 'data' && !!dataDiff);

  const previewReady = previewSql.trim().length > 0;

  const handleCompare = async () => {
    if (!canCompare) {
      toast(tr('请先填写完整参数。', 'Please complete all required fields.'), 'info');
      return false;
    }
    setIsLoading(true);
    setErrorText('');
    setPreviewSql('');
    try {
      if (mode === 'schema') {
        const diff = await api.syncSchemaDiff(sourceDbId, targetDbId);
        setSchemaDiff(diff);
        const defaults = (diff?.tables || [])
          .filter((t: any) => t.status !== 'unchanged')
          .map((t: any) => t.table_name);
        setSelectedTables(defaults);
      } else {
        const diff = await api.syncDataDiff(tableName.trim(), sourceDbId, targetDbId, primaryKey.trim());
        setDataDiff(diff);
        const ops: string[] = [];
        if (diff.insert_count > 0) ops.push('insert');
        if (diff.update_count > 0) ops.push('update');
        if (diff.delete_count > 0) ops.push('delete');
        setDataSelections({ [diff.table_name]: ops });
      }
      toast(tr('对比完成。', 'Comparison completed.'), 'success');
      return true;
    } catch (e: any) {
      const msg = e?.message || String(e);
      setErrorText(msg);
      toast(tr('对比失败：', 'Comparison failed: ') + msg, 'error');
      return false;
    } finally {
      setIsLoading(false);
    }
  };

  const handlePreview = async () => {
    if (!compareResultReady) return false;
    setIsLoading(true);
    setErrorText('');
    try {
      if (mode === 'schema') {
        const ddl = await api.syncSchemaDdl(sourceDbId, targetDbId, selectedTables);
        setPreviewSql(String(ddl || ''));
      } else if (dataDiff) {
        const dml = await api.syncDataDml([dataDiff] as any, dataSelections, primaryKey.trim());
        setPreviewSql(String(dml || ''));
      }
      toast(tr('预览生成完成。', 'Preview generated.'), 'success');
      return true;
    } catch (e: any) {
      const msg = e?.message || String(e);
      setErrorText(msg);
      toast(tr('预览生成失败：', 'Failed to generate preview: ') + msg, 'error');
      return false;
    } finally {
      setIsLoading(false);
    }
  };

  const handleExecute = async () => {
    if (!previewReady) {
      toast(tr('没有可执行 SQL。', 'No SQL to execute.'), 'info');
      return;
    }
    setIsLoading(true);
    setErrorText('');
    try {
      if (mode === 'schema') {
        await api.executeDdl(previewSql, targetDbId);
      } else {
        await api.executeSql(previewSql, false, targetDbId);
      }
      toast(tr('同步执行成功。', 'Sync executed successfully.'), 'success');
      onCancel();
    } catch (e: any) {
      const msg = e?.message || String(e);
      setErrorText(msg);
      toast(tr('执行失败：', 'Execution failed: ') + msg, 'error');
      throw e;
    } finally {
      setIsLoading(false);
    }
  };

  const steps: WizardStep[] = [
    {
      id: 'config',
      title: tr('配置', 'Config'),
      isValid: compareResultReady,
      content: (
        <div className="flex flex-col gap-4">
          <div className="text-sm text-gray-300 font-bold">{tr('步骤 1：选择同步模式与源/目标连接', 'Step 1: Select sync mode and source/target')}</div>
          <div className="flex gap-3">
            <button
              onClick={() => {
                setMode('schema');
                resetResult();
              }}
              className={`px-3 py-2 rounded border text-sm ${mode === 'schema' ? 'border-blue-500/50 bg-blue-500/10 text-blue-300' : 'border-[#30363d] text-gray-300 hover:bg-[#21262d]'}`}
            >
              {tr('结构同步', 'Schema Sync')}
            </button>
            <button
              onClick={() => {
                setMode('data');
                resetResult();
              }}
              className={`px-3 py-2 rounded border text-sm ${mode === 'data' ? 'border-blue-500/50 bg-blue-500/10 text-blue-300' : 'border-[#30363d] text-gray-300 hover:bg-[#21262d]'}`}
            >
              {tr('数据同步', 'Data Sync')}
            </button>
          </div>

          <div className="grid grid-cols-2 gap-3">
            <div>
              <div className="text-xs text-gray-500 mb-1">{tr('源连接', 'Source')}</div>
              <select
                value={sourceDbId}
                onChange={(e) => {
                  setSourceDbId(e.target.value);
                  resetResult();
                }}
                className="w-full bg-[#0d1117] border border-[#30363d] rounded px-3 py-2 text-sm text-gray-200"
              >
                <option value="">{tr('-- 选择源连接 --', '-- Select Source --')}</option>
                {dbConnections.map((c) => (
                  <option key={c.id} value={c.id}>
                    {c.name || c.id} ({dbTypeDisplayName(c.db_type)}/{dbLevelDisplayName(c.capability_level)})
                  </option>
                ))}
              </select>
            </div>
            <div>
              <div className="text-xs text-gray-500 mb-1">{tr('目标连接', 'Target')}</div>
              <select
                value={targetDbId}
                onChange={(e) => {
                  setTargetDbId(e.target.value);
                  resetResult();
                }}
                className="w-full bg-[#0d1117] border border-[#30363d] rounded px-3 py-2 text-sm text-gray-200"
              >
                <option value="">{tr('-- 选择目标连接 --', '-- Select Target --')}</option>
                {dbConnections.map((c) => (
                  <option key={c.id} value={c.id}>
                    {c.name || c.id} ({dbTypeDisplayName(c.db_type)}/{dbLevelDisplayName(c.capability_level)})
                  </option>
                ))}
              </select>
            </div>
          </div>

          {mode === 'data' && (
            <div className="grid grid-cols-2 gap-3">
              <div>
                <div className="text-xs text-gray-500 mb-1">{tr('表名', 'Table Name')}</div>
                <input
                  value={tableName}
                  onChange={(e) => {
                    setTableName(e.target.value);
                    resetResult();
                  }}
                  placeholder={tr('例如 users', 'e.g. users')}
                  className="w-full bg-[#0d1117] border border-[#30363d] rounded px-3 py-2 text-sm text-gray-200"
                />
              </div>
              <div>
                <div className="text-xs text-gray-500 mb-1">{tr('主键列', 'Primary Key')}</div>
                <input
                  value={primaryKey}
                  onChange={(e) => {
                    setPrimaryKey(e.target.value);
                    resetResult();
                  }}
                  placeholder="id"
                  className="w-full bg-[#0d1117] border border-[#30363d] rounded px-3 py-2 text-sm text-gray-200"
                />
              </div>
            </div>
          )}

          <div className="text-xs text-gray-500">
            {tr('源：', 'Source: ')}
            {sourceConn?.name || sourceDbId || '-'}
            {'  |  '}
            {tr('目标：', 'Target: ')}
            {targetConn?.name || targetDbId || '-'}
          </div>

          <button
            onClick={handleCompare}
            disabled={!canCompare || isLoading}
            className="self-start px-4 py-2 rounded border border-[#30363d] bg-[#21262d] hover:bg-[#30363d] text-sm text-gray-100 disabled:opacity-50"
          >
            {tr('开始对比', 'Compare')}
          </button>
        </div>
      ),
    },
    {
      id: 'select',
      title: tr('选择', 'Selection'),
      isValid: previewReady,
      content: (
        <div className="flex flex-col gap-4">
          <div className="text-sm text-gray-300 font-bold">
            {mode === 'schema' ? tr('步骤 2：选择需要同步的差异表', 'Step 2: Select changed tables') : tr('步骤 2：选择需要同步的操作类型', 'Step 2: Select operations')}
          </div>

          {mode === 'schema' && schemaDiff && (
            <div className="border border-[#30363d] rounded p-3 bg-[#0d1117] max-h-[360px] overflow-y-auto">
              {(schemaDiff.tables || []).filter((t: any) => t.status !== 'unchanged').length === 0 && (
                <div className="text-xs text-gray-500">{tr('未发现差异。', 'No differences found.')}</div>
              )}
              {(schemaDiff.tables || [])
                .filter((t: any) => t.status !== 'unchanged')
                .map((t: any) => (
                  <label key={t.table_name} className="flex items-center gap-3 py-1.5 text-sm text-gray-200">
                    <input
                      type="checkbox"
                      checked={selectedTables.includes(t.table_name)}
                      onChange={(e) => {
                        if (e.target.checked) setSelectedTables((prev) => [...prev, t.table_name]);
                        else setSelectedTables((prev) => prev.filter((x) => x !== t.table_name));
                      }}
                    />
                    <span className="font-mono">{t.table_name}</span>
                    <span className="text-xs text-gray-500">{t.status}</span>
                  </label>
                ))}
            </div>
          )}

          {mode === 'data' && dataDiff && (
            <div className="border border-[#30363d] rounded p-3 bg-[#0d1117]">
              <div className="text-sm text-gray-200 mb-2">{dataDiff.table_name}</div>
              <div className="flex flex-col gap-2 text-sm">
                {(['insert', 'update', 'delete'] as const).map((op) => {
                  const count =
                    op === 'insert' ? dataDiff.insert_count : op === 'update' ? dataDiff.update_count : dataDiff.delete_count;
                  const checked = (dataSelections[dataDiff.table_name] || []).includes(op);
                  return (
                    <label key={op} className="flex items-center gap-3">
                      <input
                        type="checkbox"
                        checked={checked}
                        onChange={(e) => {
                          const current = dataSelections[dataDiff.table_name] || [];
                          const next = e.target.checked ? [...current, op] : current.filter((x) => x !== op);
                          setDataSelections({ ...dataSelections, [dataDiff.table_name]: next });
                        }}
                      />
                      <span className="uppercase">{op}</span>
                      <span className="text-xs text-gray-500">{count}</span>
                    </label>
                  );
                })}
              </div>
            </div>
          )}

          <button
            onClick={handlePreview}
            disabled={!compareResultReady || isLoading || (mode === 'schema' && selectedTables.length === 0)}
            className="self-start px-4 py-2 rounded border border-[#30363d] bg-[#21262d] hover:bg-[#30363d] text-sm text-gray-100 disabled:opacity-50"
          >
            {tr('生成预览 SQL', 'Generate Preview SQL')}
          </button>
        </div>
      ),
    },
    {
      id: 'preview',
      title: tr('预览执行', 'Preview & Execute'),
      isValid: previewReady,
      content: (
        <div className="flex flex-col gap-4 h-full">
          <div className="text-sm text-gray-300 font-bold">{tr('步骤 3：预览并执行同步 SQL', 'Step 3: Preview and execute SQL')}</div>
          {errorText && (
            <div className="border border-red-500/30 bg-red-500/10 rounded p-3 text-sm text-red-200 whitespace-pre-wrap">
              {errorText}
            </div>
          )}
          <textarea
            readOnly
            value={previewSql}
            className="flex-1 min-h-[300px] bg-[#0d1117] border border-[#30363d] rounded p-3 font-mono text-xs text-gray-300"
          />
        </div>
      ),
    },
  ];

  return (
    <StepWizard
      title={tr('可视化同步向导', 'Visual Sync Wizard')}
      steps={steps}
      onCancel={onCancel}
      onFinish={handleExecute}
      isLoading={isLoading}
      finalWarningMessage={tr(
        '即将在目标数据库执行同步 SQL。该操作可能修改结构或数据，请确认源/目标连接无误。',
        'About to execute sync SQL on target database. It may change schema or data. Please confirm source/target.'
      )}
    />
  );
}
