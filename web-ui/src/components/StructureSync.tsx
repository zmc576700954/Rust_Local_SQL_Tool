import { useState, useEffect } from 'react';
import { StepWizard } from './StepWizard';
import type { WizardStep } from './StepWizard';
import { api } from '../api';
import { useToast } from './Toast';
import { dbLevelDisplayName, dbTypeDisplayName } from '../utils/dbCapabilities'
import { tr } from '../i18n';

interface StructureSyncProps {
  onCancel: () => void;
}

export function StructureSync({ onCancel }: StructureSyncProps) {
  const { toast } = useToast();
  const [sourceDbId, setSourceDbId] = useState('');
  const [targetDbId, setTargetDbId] = useState('');
  const [dbConnections, setDbConnections] = useState<any[]>([]);
  const [diff, setDiff] = useState<any>(null);
  const [selectedTables, setSelectedTables] = useState<string[]>([]);
  const [ddl, setDdl] = useState<string>('');
  const [isLoading, setIsLoading] = useState(false);
  const diffSummary = diff
    ? diff.tables.reduce((acc: any, t: any) => {
        if (t.status === 'added') acc.added += 1;
        else if (t.status === 'removed') acc.removed += 1;
        else if (t.status === 'modified') acc.modified += 1;
        return acc;
      }, { added: 0, removed: 0, modified: 0 })
    : null;

  useEffect(() => {
    const fetchConfig = async () => {
      try {
        const config = await api.getConfig();
        setDbConnections(config.db_connections || []);
        if (config.active_db_id) {
          setTargetDbId(config.active_db_id);
        }
      } catch (e: any) {
        toast(tr('加载数据库连接失败：', 'Failed to load db connections: ') + e.message, 'error');
      }
    };
    fetchConfig();
  }, []);

  const handleCompare = async () => {
    if (!sourceDbId || !targetDbId) {
      toast(tr('请选择源库和目标库', 'Please select both source and target databases'), 'error');
      return false;
    }
    setIsLoading(true);
    try {
      const diffResult = await api.syncSchemaDiff(sourceDbId, targetDbId);
      setDiff(diffResult);
      // Select all modified/added/removed by default
      const defaultSelected = diffResult.tables
        .filter((t: any) => t.status !== 'unchanged')
        .map((t: any) => t.table_name);
      setSelectedTables(defaultSelected);
      return true;
    } catch (e: any) {
      const msg = e?.response?.data?.message || e?.message || '';
      toast(tr('对比失败：', 'Failed to compare: ') + msg, 'error');
      return false;
    } finally {
      setIsLoading(false);
    }
  };

  const handleGenerateDdl = async () => {
    setIsLoading(true);
    try {
      const ddlResult = await api.syncSchemaDdl(sourceDbId, targetDbId, selectedTables);
      setDdl(ddlResult);
      return true;
    } catch (e: any) {
      toast(tr('生成 DDL 失败：', 'Failed to generate DDL: ') + e.message, 'error');
      return false;
    } finally {
      setIsLoading(false);
    }
  };

  const handleExecute = async () => {
    try {
      if (!ddl || ddl === '-- No changes detected') {
        toast(tr('没有可执行的 DDL', 'No DDL to execute'), 'info');
        return;
      }
      await api.executeDdl(ddl, targetDbId);
      toast(tr('结构同步成功', 'Schema synced successfully'), 'success');
      onCancel();
    } catch (e: any) {
      toast(tr('执行 DDL 失败：', 'Failed to execute DDL: ') + e.message, 'error');
      throw e;
    }
  };

  const steps: WizardStep[] = [
    {
      id: 'source',
      title: tr('选择数据库', 'Select Databases'),
      isValid: sourceDbId.length > 0 && targetDbId.length > 0,
      content: (
        <div className="flex flex-col gap-4 h-full">
          <div className="text-sm text-gray-300 font-bold">{tr('步骤 1：选择源库和目标库', 'Step 1: Select Source and Target Databases')}</div>
          <div className="flex flex-col gap-2">
            <label className="text-sm text-gray-400">{tr('源数据库', 'Source Database')}</label>
            <select
              value={sourceDbId}
              onChange={(e) => {
                setSourceDbId(e.target.value);
                setDiff(null);
                setDdl('');
              }}
              className="bg-[#0d1117] border border-[#30363d] rounded p-2 text-sm text-gray-300 outline-none focus:border-blue-500"
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
            <label className="text-sm text-gray-400">{tr('目标数据库', 'Target Database')}</label>
            <select
              value={targetDbId}
              onChange={(e) => {
                setTargetDbId(e.target.value);
                setDiff(null);
                setDdl('');
              }}
              className="bg-[#0d1117] border border-[#30363d] rounded p-2 text-sm text-gray-300 outline-none focus:border-blue-500"
            >
              <option value="">{tr('-- 选择目标库 --', '-- Select Target --')}</option>
              {dbConnections.map(conn => (
                <option key={conn.id} value={conn.id}>
                  {conn.name} ({dbTypeDisplayName(conn.db_type)}/{dbLevelDisplayName(conn.capability_level)}) ({conn.url})
                </option>
              ))}
            </select>
          </div>
          <button
            onClick={async () => {
              if (await handleCompare()) {
                toast(tr('对比完成，请点击下一步。', 'Comparison complete. Click Next.'), 'success');
              }
            }}
            disabled={!sourceDbId || !targetDbId}
            className="self-start mt-4 px-4 py-2 bg-[#21262d] border border-[#30363d] rounded hover:bg-[#30363d] text-sm text-white disabled:opacity-50"
          >
            {tr('开始对比', 'Compare Databases')}
          </button>
        </div>
      )
    },
    {
      id: 'diff',
      title: tr('选择差异', 'Select Differences'),
      isValid: diff !== null && selectedTables.length > 0,
      content: (
        <div className="flex flex-col gap-4 h-full">
          <div className="text-sm text-gray-300 font-bold">{tr('步骤 2：选择要同步的表', 'Step 2: Select Tables to Sync')}</div>
          {diff ? (
            <div className="flex-1 overflow-y-auto bg-[#0d1117] border border-[#30363d] rounded p-4">
              {diffSummary && (
                <div className="mb-3 text-xs text-gray-400">
                  {tr(
                    `结构变更统计：新增表 ${diffSummary.added}，删除表 ${diffSummary.removed}，修改表 ${diffSummary.modified}`,
                    `Summary: added ${diffSummary.added}, removed ${diffSummary.removed}, modified ${diffSummary.modified}`
                  )}
                </div>
              )}
              {diff.tables.filter((t: any) => t.status !== 'unchanged').length === 0 ? (
                <div className="text-gray-500 text-sm">{tr('未发现差异。', 'No differences found.')}</div>
              ) : (
                <div className="flex flex-col gap-2">
                  {diff.tables.filter((t: any) => t.status !== 'unchanged').map((t: any) => (
                    <label key={t.table_name} className="flex items-center gap-3 p-2 hover:bg-[#21262d] rounded cursor-pointer">
                      <input
                        type="checkbox"
                        checked={selectedTables.includes(t.table_name)}
                        onChange={(e) => {
                          if (e.target.checked) {
                            setSelectedTables([...selectedTables, t.table_name]);
                          } else {
                            setSelectedTables(selectedTables.filter(name => name !== t.table_name));
                          }
                        }}
                        className="w-4 h-4 rounded border-gray-600 bg-gray-700 text-blue-500 focus:ring-blue-500 focus:ring-offset-gray-800"
                      />
                      <span className="text-sm text-gray-300 font-medium">{t.table_name}</span>
                      <span className={`text-xs px-2 py-0.5 rounded ${
                        t.status === 'added' ? 'bg-green-500/20 text-green-400' :
                        t.status === 'removed' ? 'bg-red-500/20 text-red-400' :
                        'bg-yellow-500/20 text-yellow-400'
                      }`}>
                        {t.status === 'added'
                          ? tr('新增', 'ADDED')
                          : t.status === 'removed'
                          ? tr('删除', 'REMOVED')
                          : tr('变更', 'MODIFIED')}
                      </span>
                      {t.status === 'modified' && (
                        <span className="text-[11px] text-gray-500">
                          {tr(
                            `列 +${t.columns_added?.length || 0}/-${t.columns_removed?.length || 0}/~${t.columns_modified?.length || 0}，索引 +${t.indexes_added?.length || 0}/-${t.indexes_removed?.length || 0}/~${t.indexes_modified?.length || 0}，外键 +${t.fks_added?.length || 0}/-${t.fks_removed?.length || 0}/~${t.fks_modified?.length || 0}`,
                            `Cols +${t.columns_added?.length || 0}/-${t.columns_removed?.length || 0}/~${t.columns_modified?.length || 0}, Idx +${t.indexes_added?.length || 0}/-${t.indexes_removed?.length || 0}/~${t.indexes_modified?.length || 0}, FK +${t.fks_added?.length || 0}/-${t.fks_removed?.length || 0}/~${t.fks_modified?.length || 0}`
                          )}
                        </span>
                      )}
                    </label>
                  ))}
                </div>
              )}
            </div>
          ) : (
            <div className="text-gray-500 text-sm">{tr('请返回上一步执行对比。', 'Please go back and run comparison.')}</div>
          )}
          <button
            onClick={async () => {
              if (await handleGenerateDdl()) {
                toast(tr('DDL 已生成，请点击下一步。', 'DDL generated. Click Next.'), 'success');
              }
            }}
            disabled={selectedTables.length === 0}
            className="self-start px-4 py-2 bg-[#21262d] border border-[#30363d] rounded hover:bg-[#30363d] text-sm text-white disabled:opacity-50"
          >
            {tr('生成 DDL', 'Generate DDL')}
          </button>
        </div>
      )
    },
    {
      id: 'preview',
      title: tr('预览与执行', 'Preview & Execute'),
      isValid: ddl.length > 0,
      content: (
        <div className="flex flex-col gap-4 h-full">
          <div className="text-sm text-gray-300 font-bold">{tr('步骤 3：预览 DDL 脚本', 'Step 3: Preview DDL Script')}</div>
          <textarea
            readOnly
            value={ddl}
            className="flex-1 bg-[#0d1117] border border-[#30363d] rounded p-4 font-mono text-sm text-gray-300 outline-none resize-none"
          />
        </div>
      )
    }
  ];

  return (
    <StepWizard
      title={tr('结构同步', 'Structure Sync')}
      steps={steps}
      onCancel={onCancel}
      onFinish={handleExecute}
      finalWarningMessage={tr('你即将在目标数据库执行 DDL，操作不可撤销。', 'You are about to execute DDL statements on target database. This cannot be undone.')}
      isLoading={isLoading}
    />
  );
}
