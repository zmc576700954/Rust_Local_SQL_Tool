import { useState, useEffect } from 'react';
import { StepWizard } from './StepWizard';
import type { WizardStep } from './StepWizard';
import { api } from '../api';
import { useToast } from './Toast';
import { dbLevelDisplayName, dbTypeDisplayName } from '../utils/dbCapabilities'
import { sanitizeForLog } from '../utils'
import { tr } from '../i18n';

interface DataTransferProps {
  onCancel: () => void;
}

export function DataTransfer({ onCancel }: DataTransferProps) {
  const { toast } = useToast();
  
  const [sourceType, setSourceType] = useState<'local_file' | 'network_db'>('local_file');
  const [file, setFile] = useState<File | null>(null);
  const [delimiter, setDelimiter] = useState(',');
  const [encoding, setEncoding] = useState('utf-8');
  
  // For network db
  const [sourceDbId, setSourceDbId] = useState('');
  const [dbConnections, setDbConnections] = useState<any[]>([]);
  const [sourceTable, setSourceTable] = useState('');
  const [targetDbId, setTargetDbId] = useState('');
  
  const [targetTable, setTargetTable] = useState('');
  const [mode, setMode] = useState<'Append' | 'Replace'>('Append');
  
  const [sourceColumns, setSourceColumns] = useState<string[]>([]);
  const [previewData, setPreviewData] = useState<string[][]>([]);
  const [sourcePath, setSourcePath] = useState<string>(''); // For local file
  
  const [targetColumns, setTargetColumns] = useState<string[]>([]);
  
  // Mapping: source_col -> target_col
  const [mappings, setMappings] = useState<Record<string, string>>({});
  
  const [dml, setDml] = useState('');
  const [transferSummary, setTransferSummary] = useState<{ insert_count: number; update_count: number; unchanged_count: number; compare_based: boolean } | null>(null);
  const [isLoading, setIsLoading] = useState(false);
  const [jobId, setJobId] = useState<string | null>(null);

  useEffect(() => {
    api.getConfig().then(cfg => {
      if (cfg && cfg.db_connections) {
        setDbConnections(cfg.db_connections);
        if (cfg.active_db_id) {
          setTargetDbId(cfg.active_db_id);
        } else if (cfg.db_connections.length > 0) {
          setTargetDbId(cfg.db_connections[0].id);
        }
        if (cfg.db_connections.length > 0) {
          setSourceDbId(cfg.db_connections[0].id);
        }
      }
    }).catch(e => console.error(tr('加载配置失败', 'Failed to load config'), sanitizeForLog(e)));
  }, []);

  const isSqlFile = sourceType === 'local_file' && file?.name.endsWith('.sql');

  useEffect(() => {
    if (!jobId) return;
    let alive = true;
    const tick = async () => {
      try {
        const j = await api.toolJobStatus(jobId);
        if (!alive) return;
        if (j?.status === 'completed') {
          toast(tr('数据传输成功', 'Data transferred successfully'), 'success');
          setJobId(null);
          setIsLoading(false);
          onCancel();
        } else if (j?.status === 'error' || j?.status === 'canceled') {
          toast(j?.error || tr('传输失败', 'Transfer failed'), 'error');
          setJobId(null);
          setIsLoading(false);
        }
      } catch (e: any) {
        if (!alive) return;
        toast(tr('获取任务状态失败：', 'Failed to fetch job status: ') + (e?.message || ''), 'error');
        setJobId(null);
        setIsLoading(false);
      }
    };
    tick();
    const t = window.setInterval(tick, 1000);
    return () => {
      alive = false;
      window.clearInterval(t);
    };
  }, [jobId, onCancel, toast]);

  const handleSourceNext = async () => {
    setIsLoading(true);
    setTransferSummary(null);
    if (!targetDbId) {
      toast(tr('请选择目标连接', 'Please select target connection'), 'info');
      setIsLoading(false);
      return false;
    }
    if (sourceType === 'local_file') {
      if (!file) {
        toast(tr('请选择文件', 'Please select a file'), 'info');
        setIsLoading(false);
        return false;
      }
      try {
        const formData = new FormData();
        formData.append('file', file);
        formData.append('delimiter', delimiter);
        formData.append('encoding', encoding);
        const res = await api.transferUpload(formData);
        
        setSourcePath(res.source_path);

        if (file.name.endsWith('.sql')) {
          // Skip mapping for .sql files
          try {
            const config = {
              source_type: sourceType,
              source_path: res.source_path,
              source_db_id: null,
              source_table: null,
              target_db_id: targetDbId || null,
              target_url: "",
              target_table: targetTable,
              mode,
              mappings: []
            };
            const dmlRes = await api.transferExecute(config);
            setDml(typeof dmlRes === 'string' ? dmlRes : (dmlRes?.dml || ''));
            if (dmlRes && typeof dmlRes === 'object') {
              setTransferSummary({
                insert_count: Number(dmlRes.insert_count || 0),
                update_count: Number(dmlRes.update_count || 0),
                unchanged_count: Number(dmlRes.unchanged_count || 0),
                compare_based: !!dmlRes.compare_based,
              });
            }
          } catch (e: any) {
            const msg = e?.response?.data?.message || e.message;
            toast(tr('生成传输脚本失败：', 'Failed to generate transfer script: ') + msg, 'error');
            setIsLoading(false);
            return false;
          }
        } else {
          setSourceColumns(res.columns);
          setPreviewData(res.preview_data);
          
          // Initialize default mappings
          const defaultMappings: Record<string, string> = {};
          res.columns.forEach((c: string) => {
            defaultMappings[c] = c; // Map to same name by default
          });
          setMappings(defaultMappings);
        }
        
      } catch (e: any) {
        toast(tr('上传文件失败：', 'Failed to upload file: ') + e.message, 'error');
        setIsLoading(false);
        return false;
      }
    } else {
      if (!sourceDbId || !sourceTable) {
        toast(tr('请填写源库连接信息', 'Please provide source connection info'), 'info');
        setIsLoading(false);
        return false;
      }
      // For network DB, we would ideally fetch the schema.
      // But for simplicity in this task, we can just ask the user to type mapping manually or fetch if we had an endpoint.
      // Since we don't have a direct endpoint to get remote schema, we will just proceed and let them type.
      // Or we can assume they know the columns.
      toast(tr('已选择网络数据库源，下一步请手动指定列映射。', 'Network DB source selected. Please specify columns manually in next step.'), 'info');
      setSourceColumns([]);
      setPreviewData([]);
    }
    
    // Try to fetch target table columns if targetTable is provided
    if (targetTable) {
      try {
        const schema = await api.getTableSchema(targetTable, targetDbId);
        if (schema && schema.columns) {
          setTargetColumns(schema.columns.map((c: any) => c.name));
        }
      } catch (e) {
        console.error(tr('获取目标表结构失败', 'Failed to fetch target schema'), sanitizeForLog(e));
        // It's okay if target table doesn't exist yet, but in this tool we assume it does.
      }
    }
    
    setIsLoading(false);
    return true;
  };

  const handleMappingNext = async () => {
    // Validate mapping
    const validMappings = Object.keys(mappings).filter(k => mappings[k]);
    if (validMappings.length === 0) {
      toast(tr('请至少映射一列', 'Please map at least one column'), 'info');
      return false;
    }
    
    setIsLoading(true);
    // Generate DML preview
    try {
      const config = {
        source_type: sourceType,
        source_path: sourceType === 'local_file' ? sourcePath : null,
        source_db_id: sourceType === 'network_db' ? sourceDbId : null,
        source_table: sourceType === 'network_db' ? sourceTable : null,
        target_db_id: targetDbId || null,
        target_url: "", // The backend doesn't use this directly since it executes on the current DB
        target_table: targetTable,
        mode,
        mappings: validMappings.map(k => ({ source_col: k, target_col: mappings[k] }))
      };
      
      const dmlRes = await api.transferExecute(config);
      setDml(typeof dmlRes === 'string' ? dmlRes : (dmlRes?.dml || ''));
      if (dmlRes && typeof dmlRes === 'object') {
        setTransferSummary({
          insert_count: Number(dmlRes.insert_count || 0),
          update_count: Number(dmlRes.update_count || 0),
          unchanged_count: Number(dmlRes.unchanged_count || 0),
          compare_based: !!dmlRes.compare_based,
        });
      }
      return true;
    } catch (e: any) {
      const msg = e?.response?.data?.message || e.message;
      toast(tr('生成传输脚本失败：', 'Failed to generate transfer script: ') + msg, 'error');
      return false;
    } finally {
      setIsLoading(false);
    }
  };

  const handleExecute = async () => {
    if (!dml) {
      toast(tr('没有可执行的 DML', 'No DML generated'), 'info');
      return;
    }
    try {
      setIsLoading(true);
      const res = await api.importSqlJobStart({ sql: dml, force: true, db_id: targetDbId || undefined });
      setJobId(res.job_id);
      setIsLoading(false);
    } catch (e: any) {
      toast(tr('执行传输失败：', 'Failed to execute transfer: ') + e.message, 'error');
      setIsLoading(false);
      throw e;
    }
  };

  const handleCancel = async () => {
    if (jobId) {
      try {
        await api.toolJobCancel(jobId);
      } catch (e: any) {
        void e;
      }
    }
    onCancel();
  };

  const addManualMapping = () => {
    const newCol = `col_${Object.keys(mappings).length + 1}`;
    setMappings({ ...mappings, [newCol]: '' });
    if (!sourceColumns.includes(newCol)) {
      setSourceColumns([...sourceColumns, newCol]);
    }
  };

  const steps: WizardStep[] = [
    {
      id: 'source',
      title: tr('选择数据源', 'Select Source'),
      isValid: targetDbId.length > 0 && targetTable.length > 0 && (sourceType === 'local_file' ? file !== null : (sourceDbId.length > 0 && sourceTable.length > 0)) && (sourceType === 'network_db' || sourceColumns.length > 0 || (isSqlFile && dml.length > 0)),
      content: (
        <div className="flex flex-col gap-4 h-full">
          <div className="text-sm text-gray-300 font-bold">{tr('步骤 1：配置源与目标', 'Step 1: Configure Source & Target')}</div>
          
          <div className="flex gap-4">
            <div className="flex-1">
              <label className="block text-xs text-gray-400 mb-1">{tr('源类型', 'Source Type')}</label>
              <select
                value={sourceType}
                onChange={(e) => setSourceType(e.target.value as any)}
                className="w-full bg-[#0d1117] border border-[#30363d] rounded px-3 py-2 text-sm text-gray-300 outline-none focus:border-blue-500"
              >
                <option value="local_file">{tr('本地文件（CSV/SQL）', 'Local File (CSV/SQL)')}</option>
                <option value="network_db">{tr('网络数据库', 'Network Database')}</option>
              </select>
            </div>
            <div className="flex-1">
              <label className="block text-xs text-gray-400 mb-1">{tr('目标连接', 'Target Connection')}</label>
              <select
                value={targetDbId}
                onChange={(e) => setTargetDbId(e.target.value)}
                className="w-full bg-[#0d1117] border border-[#30363d] rounded px-3 py-2 text-sm text-gray-300 outline-none focus:border-blue-500"
              >
                <option value="">{tr('-- 选择目标连接 --', '-- Select Target Connection --')}</option>
                {dbConnections.map(conn => (
                  <option key={conn.id} value={conn.id}>
                    {conn.name} ({dbTypeDisplayName(conn.db_type)}/{dbLevelDisplayName(conn.capability_level)})
                  </option>
                ))}
              </select>
            </div>
            <div className="flex-1">
              <label className="block text-xs text-gray-400 mb-1">{tr('目标表名', 'Target Table Name')}</label>
              <input
                value={targetTable}
                onChange={(e) => setTargetTable(e.target.value)}
                className="w-full bg-[#0d1117] border border-[#30363d] rounded px-3 py-2 text-sm text-gray-300 outline-none focus:border-blue-500"
                placeholder={tr('例如 users', 'e.g. users')}
              />
            </div>
          </div>

          {sourceType === 'local_file' ? (
            <div className="flex flex-col gap-4">
              <div>
                <label className="block text-xs text-gray-400 mb-1">{tr('上传文件', 'Upload File')}</label>
                <input
                  type="file"
                  accept=".csv,.txt,.sql"
                  onChange={(e) => setFile(e.target.files?.[0] || null)}
                  className="w-full bg-[#0d1117] border border-[#30363d] rounded px-3 py-2 text-sm text-gray-300 outline-none focus:border-blue-500"
                />
              </div>
              <div className="flex gap-4">
                <div className="flex-1">
                  <label className="block text-xs text-gray-400 mb-1">{tr('分隔符（CSV/TXT）', 'Delimiter (for CSV/TXT)')}</label>
                  <input
                    value={delimiter}
                    onChange={(e) => setDelimiter(e.target.value)}
                    className="w-full bg-[#0d1117] border border-[#30363d] rounded px-3 py-2 text-sm text-gray-300 outline-none focus:border-blue-500"
                    placeholder=","
                  />
                </div>
                <div className="flex-1">
                  <label className="block text-xs text-gray-400 mb-1">{tr('编码', 'Encoding')}</label>
                  <select
                    value={encoding}
                    onChange={(e) => setEncoding(e.target.value)}
                    className="w-full bg-[#0d1117] border border-[#30363d] rounded px-3 py-2 text-sm text-gray-300 outline-none focus:border-blue-500"
                  >
                    <option value="utf-8">UTF-8</option>
                    <option value="gbk">GBK</option>
                    <option value="latin1">Latin1</option>
                  </select>
                </div>
              </div>
            </div>
          ) : (
            <div className="flex gap-4">
              <div className="flex-1">
                <label className="block text-xs text-gray-400 mb-1">{tr('源连接', 'Source Connection')}</label>
                <select
                  value={sourceDbId}
                  onChange={(e) => setSourceDbId(e.target.value)}
                  className="w-full bg-[#0d1117] border border-[#30363d] rounded px-3 py-2 text-sm text-gray-300 outline-none focus:border-blue-500"
                >
                  <option value="">{tr('-- 选择连接 --', '-- Select Connection --')}</option>
                  {dbConnections.map(conn => (
                    <option key={conn.id} value={conn.id}>
                      {conn.name} ({dbTypeDisplayName(conn.db_type)}/{dbLevelDisplayName(conn.capability_level)})
                    </option>
                  ))}
                </select>
              </div>
              <div className="flex-1">
                <label className="block text-xs text-gray-400 mb-1">{tr('源表', 'Source Table')}</label>
                <input
                  value={sourceTable}
                  onChange={(e) => setSourceTable(e.target.value)}
                  className="w-full bg-[#0d1117] border border-[#30363d] rounded px-3 py-2 text-sm text-gray-300 outline-none focus:border-blue-500"
                  placeholder={tr('例如 old_users', 'e.g. old_users')}
                />
              </div>
            </div>
          )}
          
          <div>
            <label className="block text-xs text-gray-400 mb-1">{tr('传输模式', 'Transfer Mode')}</label>
            <select
              value={mode}
              onChange={(e) => setMode(e.target.value as any)}
              className="w-full bg-[#0d1117] border border-[#30363d] rounded px-3 py-2 text-sm text-gray-300 outline-none focus:border-blue-500"
            >
              <option value="Append">{tr('追加（保留原有数据）', 'Append (Keep existing data)')}</option>
              <option value="Replace">{tr('替换（插入前清空）', 'Replace (Truncate before insert)')}</option>
            </select>
          </div>
          
          <button
            onClick={async () => {
              if (await handleSourceNext()) {
                toast(tr('配置校验通过，请点击下一步。', 'Configuration validated. Click Next.'), 'success');
              }
            }}
            className="self-start px-4 py-2 bg-[#21262d] border border-[#30363d] rounded hover:bg-[#30363d] text-sm text-white"
          >
            {tr('校验并解析源数据', 'Validate & Parse Source')}
          </button>
        </div>
      )
    },
    (!isSqlFile ? {
      id: 'mapping',
      title: tr('列映射', 'Column Mapping'),
      isValid: dml.length > 0,
      content: (
        <div className="flex flex-col gap-4 h-full">
          <div className="text-sm text-gray-300 font-bold">{tr('步骤 2：映射列', 'Step 2: Map Columns')}</div>
          
          {previewData.length > 0 && (
            <div className="bg-[#0d1117] p-3 rounded border border-[#30363d] overflow-auto max-h-32">
              <div className="text-xs text-gray-400 mb-2">{tr('文件预览', 'File Preview')}</div>
              <table className="w-full text-xs text-left">
                <thead>
                  <tr>
                    {sourceColumns.map((c, i) => <th key={i} className="px-2 py-1 bg-[#161b22]">{c}</th>)}
                  </tr>
                </thead>
                <tbody>
                  {previewData.map((row, i) => (
                    <tr key={i} className="border-t border-[#30363d]">
                      {row.map((cell, j) => <td key={j} className="px-2 py-1">{cell}</td>)}
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}

          <div className="flex-1 overflow-auto">
            <table className="w-full text-sm text-left">
              <thead>
                <tr className="bg-[#161b22] text-gray-400">
                  <th className="px-4 py-2 font-medium">{tr('源列', 'Source Column')}</th>
                  <th className="px-4 py-2 font-medium">{tr('目标列', 'Target Column')}</th>
                </tr>
              </thead>
              <tbody>
                {sourceColumns.map((col, i) => (
                  <tr key={i} className="border-b border-[#30363d]">
                    <td className="px-4 py-2 text-gray-300">
                      {sourceType === 'network_db' ? (
                        <input
                          value={col}
                          onChange={(e) => {
                            const newCols = [...sourceColumns];
                            newCols[i] = e.target.value;
                            setSourceColumns(newCols);
                            
                            const newMap = { ...mappings };
                            delete newMap[col];
                            newMap[e.target.value] = mappings[col] || '';
                            setMappings(newMap);
                          }}
                          className="bg-transparent border-b border-gray-600 focus:border-blue-500 outline-none w-full"
                        />
                      ) : col}
                    </td>
                    <td className="px-4 py-2">
                      {targetColumns.length > 0 ? (
                        <select
                          value={mappings[col] || ''}
                          onChange={(e) => setMappings({ ...mappings, [col]: e.target.value })}
                          className="w-full bg-[#0d1117] border border-[#30363d] rounded px-2 py-1 text-gray-300 outline-none"
                        >
                          <option value="">{tr('-- 忽略 --', '-- Ignore --')}</option>
                          {targetColumns.map(tc => (
                            <option key={tc} value={tc}>{tc}</option>
                          ))}
                        </select>
                      ) : (
                        <input
                          value={mappings[col] || ''}
                          onChange={(e) => setMappings({ ...mappings, [col]: e.target.value })}
                          className="w-full bg-[#0d1117] border border-[#30363d] rounded px-2 py-1 text-gray-300 outline-none"
                          placeholder={tr('目标列名', 'Target column name')}
                        />
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
            
            {sourceType === 'network_db' && (
              <button
                onClick={addManualMapping}
                className="mt-2 text-xs text-blue-400 hover:text-blue-300"
              >
                + {tr('新增列映射', 'Add Column Mapping')}
              </button>
            )}
          </div>
          <button
            onClick={async () => {
              if (await handleMappingNext()) {
                toast(tr('映射校验通过，请点击下一步预览。', 'Mapping validated. Click Next to preview.'), 'success');
              }
            }}
            className="self-start px-4 py-2 bg-[#21262d] border border-[#30363d] rounded hover:bg-[#30363d] text-sm text-white"
          >
            {tr('生成传输脚本', 'Generate Transfer Script')}
          </button>
        </div>
      )
    } : null),
    {
      id: 'preview',
      title: tr('预览与执行', 'Preview & Execute'),
      isValid: true,
      content: (
        <div className="flex flex-col gap-4 h-full">
          <div className="text-sm text-gray-300 font-bold">{tr('步骤 3：预览传输脚本', 'Step 3: Preview Transfer Script')}</div>
          {transferSummary && (
            <div className="text-xs text-gray-400 border border-[#30363d] rounded p-3 bg-[#0d1117]">
              <div>{tr(`新增记录：${transferSummary.insert_count}`, `Insert records: ${transferSummary.insert_count}`)}</div>
              <div>{tr(`修改记录：${transferSummary.update_count}`, `Update records: ${transferSummary.update_count}`)}</div>
              <div>{tr(`未变化记录：${transferSummary.unchanged_count}`, `Unchanged records: ${transferSummary.unchanged_count}`)}</div>
              <div>{transferSummary.compare_based ? tr('本次传输已基于源/目标连接对比', 'This transfer is compared between source/target connections') : tr('本次传输未启用记录级对比（按映射生成）', 'Record-level compare is not enabled for this transfer')}</div>
            </div>
          )}
          <div className="flex-1 bg-[#0d1117] border border-[#30363d] rounded overflow-hidden p-2">
            <pre className="text-xs text-green-400 font-mono h-full overflow-auto whitespace-pre-wrap">
              {dml || tr('-- 无可执行变更', '-- No changes to execute')}
            </pre>
          </div>
        </div>
      )
    }
  ].filter(Boolean) as WizardStep[];

  return (
    <StepWizard
      steps={steps}
      onCancel={handleCancel}
      onFinish={handleExecute}
      title={tr('数据传输', 'Data Transfer')}
      isLoading={isLoading}
    />
  );
}
