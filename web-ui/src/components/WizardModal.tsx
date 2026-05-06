import { useEffect, useState } from 'react';
import { motion, AnimatePresence } from 'framer-motion';
import { X, Play } from 'lucide-react';
import { api, JOB_POLL_INTERVAL_MS } from '../api';
import { useToast } from './Toast';
import { MockDataConfig } from './MockDataConfig';
import { ImportWizard } from './ImportWizard';
import { StructureSync } from './StructureSync';
import { DataSync } from './DataSync';
import { PerfSync } from './PerfSync';
import { GoLive } from './GoLive';
import { DataTransfer } from './DataTransfer';
import { DbSecurityManager } from './DbSecurityManager';
import { DbEventsTriggers } from './DbEventsTriggers';
import { ModelCompare } from './ModelCompare';
import { VisualSyncWizard } from './VisualSyncWizard';
import { tr } from '../i18n';

interface WizardModalProps {
  isOpen: boolean;
  onClose: () => void;
  title: string;
  type: string;
  payload?: any;
}

export function WizardModal({ isOpen, onClose, title, type, payload }: WizardModalProps) {
  const { toast } = useToast();
  const [loading, setLoading] = useState(false);
  const [result, setResult] = useState<any>(null);
  const [rowCount, setRowCount] = useState(10);
  const [exportType, setExportType] = useState('csv');
  const [exportWhere, setExportWhere] = useState('');
  const [exportPrimaryKey, setExportPrimaryKey] = useState('');
  const [exportPkStart, setExportPkStart] = useState('');
  const [exportPkEnd, setExportPkEnd] = useState('');
  const [exportWindowLimit, setExportWindowLimit] = useState('');
  const [exportWindowOffset, setExportWindowOffset] = useState('');
  const [exportJobId, setExportJobId] = useState<string | null>(null);
  const [exportJob, setExportJob] = useState<any | null>(null);
  const [exportResumeJobId, setExportResumeJobId] = useState('');
  const [exportPollInterrupted, setExportPollInterrupted] = useState<{ job_id: string; message: string } | null>(null);
  const [exportPollKey, setExportPollKey] = useState(0);
  const [mockRules, setMockRules] = useState<Record<string, string>>({});
  const [paramValues, setParamValues] = useState<Record<string, string>>({});

  useEffect(() => {
    if (!exportJobId) return;
    let alive = true;
    window.localStorage.setItem('export:lastJobId', exportJobId);
    const tick = async () => {
      try {
        const j = await api.toolJobStatus(exportJobId);
        if (!alive) return;
        setExportJob(j);
        if (j?.status === 'completed') {
          setResult(JSON.stringify(j?.result || {}, null, 2));
          toast('Export completed successfully!', 'success');
          setLoading(false);
          setExportPollInterrupted(null);
          alive = false;
        } else if (j?.status === 'error' || j?.status === 'canceled') {
          toast(j?.error || 'Export failed', 'error');
          setLoading(false);
          setExportPollInterrupted(null);
          alive = false;
        }
      } catch (e: any) {
        if (!alive) return;
        setExportPollInterrupted({ job_id: exportJobId, message: e?.response?.data?.message || e?.message || String(e) });
        toast('Failed to fetch job status: ' + (e?.message || ''), 'error');
        setLoading(false);
        alive = false;
      }
    };
    tick();
    const t = window.setInterval(tick, JOB_POLL_INTERVAL_MS);
    return () => {
      alive = false;
      window.clearInterval(t);
    };
  }, [exportJobId, exportPollKey, toast]);

  useEffect(() => {
    if (!isOpen) return;
    const saved = window.localStorage.getItem('export:lastJobId') || '';
    if (saved) setExportResumeJobId(saved);
  }, [isOpen]);

  const restartExportPolling = () => setExportPollKey(k => k + 1);

  const handleAction = async () => {
    if (type === 'parameterized-query') {
      let finalSql = payload.sql;
      for (const param of payload.params) {
        const regex = new RegExp(`:${param}\\b`, 'g');
        const val = paramValues[param] || '';
        const isNumeric = /^-?\d+(\.\d+)?$/.test(val);
        finalSql = finalSql.replace(regex, isNumeric ? val : `'${val.replace(/'/g, "''")}'`);
      }
      payload.onExecute(finalSql);
      onClose();
      return;
    }

    setLoading(true);
    let keepLoading = false;
    try {
      if (type === 'mock-data') {
        const res = await api.generateMockData(payload.tableName, rowCount, mockRules);
        setResult(res.sql);
      } else if (type === 'export') {
        const res = await api.exportJobStart({
          table_name: payload.tableName,
          export_type: exportType,
          where_clause: exportWhere.trim() ? exportWhere.trim() : null,
          primary_key: exportPrimaryKey.trim() ? exportPrimaryKey.trim() : null,
          pk_start: exportPkStart.trim() ? exportPkStart.trim() : null,
          pk_end: exportPkEnd.trim() ? exportPkEnd.trim() : null,
          window_limit: exportWindowLimit.trim() ? Number(exportWindowLimit) : null,
          window_offset: exportWindowOffset.trim() ? Number(exportWindowOffset) : null,
        });
        setExportJobId(res.job_id);
        setExportResumeJobId(res.job_id);
        setExportPollInterrupted(null);
        setResult(null);
        toast('Export job started', 'success');
        keepLoading = true;
      } else {
        setResult('Coming soon: ' + type);
      }
    } catch (e: any) {
      const msg = e?.response?.data?.message || e?.response?.data?.error || e?.message || String(e);
      toast(`执行失败：${msg}`, 'error');
    } finally {
      if (!keepLoading) setLoading(false);
    }
  };

  const downloadExportArtifact = (artifact: 'data' | 'manifest') => {
    if (!exportJobId) return;
    const url = `/backend/tools/jobs/${encodeURIComponent(exportJobId)}/artifacts/${artifact}`;
    const link = document.createElement('a');
    link.href = url;
    document.body.appendChild(link);
    link.click();
    document.body.removeChild(link);
  };

  const cancelExportJob = async () => {
    if (!exportJobId) return;
    setLoading(true);
    try {
      await api.toolJobCancel(exportJobId);
      toast('Job canceled', 'success');
    } catch (e: any) {
      toast('Failed to cancel job: ' + e.toString(), 'error');
    } finally {
      setLoading(false);
    }
  };

  const resumeExportJob = () => {
    const id = exportResumeJobId.trim();
    if (!id) return;
    setExportJobId(id);
    setExportJob(null);
    setResult(null);
    setExportPollInterrupted(null);
    setLoading(true);
    restartExportPolling();
  };

  const continueExportPolling = () => {
    if (!exportJobId) return;
    setExportPollInterrupted(null);
    setLoading(true);
    restartExportPolling();
  };

  const handleDownload = () => {
    if (!result) return;
    const blob = new Blob([result], { type: 'text/plain;charset=utf-8;' });
    const url = URL.createObjectURL(blob);
    const link = document.createElement('a');
    link.href = url;
    link.setAttribute('download', `${type}_result.txt`);
    document.body.appendChild(link);
    link.click();
    document.body.removeChild(link);
  };

  if (!isOpen) return null;

  if (type === 'schema-sync') {
    return (
      <AnimatePresence>
        <div className="fixed inset-0 bg-black/60 backdrop-blur-sm z-[100] flex items-center justify-center p-4">
          <motion.div
            initial={{ scale: 0.95, opacity: 0 }}
            animate={{ scale: 1, opacity: 1 }}
            exit={{ scale: 0.95, opacity: 0 }}
            className="w-full max-w-4xl h-[85vh]"
          >
            <StructureSync onCancel={onClose} />
          </motion.div>
        </div>
      </AnimatePresence>
    );
  }

  if (type === 'data-sync') {
    return (
      <AnimatePresence>
        <div className="fixed inset-0 bg-black/60 backdrop-blur-sm z-[100] flex items-center justify-center p-4">
          <motion.div
            initial={{ scale: 0.95, opacity: 0 }}
            animate={{ scale: 1, opacity: 1 }}
            exit={{ scale: 0.95, opacity: 0 }}
            className="w-full max-w-4xl h-[85vh]"
          >
            <DataSync onCancel={onClose} />
          </motion.div>
        </div>
      </AnimatePresence>
    );
  }

  if (type === 'data-transfer') {
    return (
      <AnimatePresence>
        <div className="fixed inset-0 bg-black/60 backdrop-blur-sm z-[100] flex items-center justify-center p-4">
          <motion.div
            initial={{ scale: 0.95, opacity: 0 }}
            animate={{ scale: 1, opacity: 1 }}
            exit={{ scale: 0.95, opacity: 0 }}
            className="w-full max-w-4xl h-[85vh]"
          >
            <DataTransfer onCancel={onClose} />
          </motion.div>
        </div>
      </AnimatePresence>
    );
  }

  if (type === 'perf-sync') {
    return (
      <AnimatePresence>
        <div className="fixed inset-0 bg-black/60 backdrop-blur-sm z-[100] flex items-center justify-center p-4">
          <motion.div
            initial={{ scale: 0.95, opacity: 0 }}
            animate={{ scale: 1, opacity: 1 }}
            exit={{ scale: 0.95, opacity: 0 }}
            className="w-full max-w-4xl h-[85vh]"
          >
            <PerfSync onCancel={onClose} />
          </motion.div>
        </div>
      </AnimatePresence>
    );
  }

  if (type === 'go-live') {
    return (
      <AnimatePresence>
        <div className="fixed inset-0 bg-black/60 backdrop-blur-sm z-[100] flex items-center justify-center p-4">
          <motion.div
            initial={{ scale: 0.95, opacity: 0 }}
            animate={{ scale: 1, opacity: 1 }}
            exit={{ scale: 0.95, opacity: 0 }}
            className="w-full max-w-4xl h-[85vh]"
          >
            <GoLive onCancel={onClose} />
          </motion.div>
        </div>
      </AnimatePresence>
    );
  }

  if (type === 'db-security') {
    return (
      <AnimatePresence>
        <div className="fixed inset-0 bg-black/60 backdrop-blur-sm z-[100] flex items-center justify-center p-4">
          <motion.div
            initial={{ scale: 0.95, opacity: 0 }}
            animate={{ scale: 1, opacity: 1 }}
            exit={{ scale: 0.95, opacity: 0 }}
            className="w-full max-w-5xl h-[85vh]"
          >
            <DbSecurityManager onCancel={onClose} />
          </motion.div>
        </div>
      </AnimatePresence>
    );
  }

  if (type === 'db-events') {
    return (
      <AnimatePresence>
        <div className="fixed inset-0 bg-black/60 backdrop-blur-sm z-[100] flex items-center justify-center p-4">
          <motion.div
            initial={{ scale: 0.95, opacity: 0 }}
            animate={{ scale: 1, opacity: 1 }}
            exit={{ scale: 0.95, opacity: 0 }}
            className="w-full max-w-6xl h-[85vh]"
          >
            <DbEventsTriggers onCancel={onClose} />
          </motion.div>
        </div>
      </AnimatePresence>
    );
  }

  if (type === 'model-compare') {
    return (
      <AnimatePresence>
        <div className="fixed inset-0 bg-black/60 backdrop-blur-sm z-[100] flex items-center justify-center p-4">
          <motion.div
            initial={{ scale: 0.95, opacity: 0 }}
            animate={{ scale: 1, opacity: 1 }}
            exit={{ scale: 0.95, opacity: 0 }}
            className="w-full max-w-6xl h-[85vh]"
          >
            <ModelCompare onCancel={onClose} />
          </motion.div>
        </div>
      </AnimatePresence>
    );
  }

  if (type === 'visual-sync') {
    return (
      <AnimatePresence>
        <div className="fixed inset-0 bg-black/60 backdrop-blur-sm z-[100] flex items-center justify-center p-4">
          <motion.div
            initial={{ scale: 0.95, opacity: 0 }}
            animate={{ scale: 1, opacity: 1 }}
            exit={{ scale: 0.95, opacity: 0 }}
            className="w-full max-w-6xl h-[85vh]"
          >
            <VisualSyncWizard onCancel={onClose} />
          </motion.div>
        </div>
      </AnimatePresence>
    );
  }

  return (
    <AnimatePresence>
      <div className="fixed inset-0 bg-black/60 backdrop-blur-sm z-[100] flex items-center justify-center">
        <motion.div
          initial={{ scale: 0.95, opacity: 0 }}
          animate={{ scale: 1, opacity: 1 }}
          exit={{ scale: 0.95, opacity: 0 }}
          className="bg-[#161b22] border border-[#30363d] rounded-xl shadow-2xl max-w-2xl w-full mx-4 flex flex-col max-h-[80vh]"
        >
          <div className="px-6 py-4 border-b border-[#30363d] flex items-center justify-between bg-[#0d1117] shrink-0">
            <h3 className="text-gray-200 font-bold text-lg">{title}</h3>
            <button onClick={onClose} className="text-gray-500 hover:text-white transition-colors">
              <X className="w-5 h-5" />
            </button>
          </div>

          <div className="p-6 flex-1 overflow-y-auto">
            {type === 'mock-data' && (
              <div className="flex flex-col gap-4">
                <div className="flex items-center gap-4">
                  <label className="text-sm text-gray-300">Generate Row Count:</label>
                  <input 
                    type="number" 
                    value={rowCount} 
                    onChange={(e) => setRowCount(Number(e.target.value))}
                    className="bg-[#0d1117] border border-[#30363d] rounded px-3 py-1 text-sm text-white focus:outline-none focus:border-blue-500 w-24"
                    min="1"
                    max="1000"
                  />
                </div>
                <MockDataConfig 
                  tableName={payload?.tableName || ''} 
                  rules={mockRules} 
                  onChangeRules={setMockRules} 
                />
              </div>
            )}
            {type === 'export' && (
              <div className="flex flex-col gap-4">
                <div className="bg-[#0d1117] border border-[#30363d] rounded p-3">
                  <div className="text-xs text-gray-400">恢复 Export Job</div>
                  <div className="mt-2 flex items-center gap-2">
                    <input
                      value={exportResumeJobId}
                      onChange={e => setExportResumeJobId(e.target.value)}
                      placeholder="输入 job_id"
                      className="flex-1 bg-[#0d1117] border border-[#30363d] rounded px-3 py-2 text-sm text-gray-200 outline-none focus:border-blue-500"
                      disabled={exportJob?.status === 'running' || loading}
                    />
                    <button
                      className="px-3 py-2 rounded border border-[#30363d] hover:bg-[#30363d] text-sm text-gray-200"
                      onClick={resumeExportJob}
                      disabled={!exportResumeJobId.trim() || exportJob?.status === 'running' || loading}
                    >
                      恢复查看
                    </button>
                  </div>
                </div>

                {exportPollInterrupted && (
                  <div className="border border-yellow-500/30 bg-yellow-500/10 rounded p-3">
                    <div className="text-xs text-yellow-200">轮询中断</div>
                    <div className="mt-1 text-xs text-yellow-100 whitespace-pre-wrap">{exportPollInterrupted.message}</div>
                    <div className="mt-2 flex items-center justify-between gap-2">
                      <div className="text-xs text-yellow-100 font-mono">job_id={exportPollInterrupted.job_id}</div>
                      <button
                        className="px-3 py-2 rounded bg-blue-600 hover:bg-blue-500 text-sm text-white"
                        onClick={continueExportPolling}
                      >
                        继续轮询
                      </button>
                    </div>
                  </div>
                )}

                <div className="flex items-center gap-4">
                  <label className="text-sm text-gray-300">Export Format:</label>
                  <select 
                    value={exportType} 
                    onChange={(e) => setExportType(e.target.value)}
                    className="bg-[#0d1117] border border-[#30363d] rounded px-3 py-1 text-sm text-white focus:outline-none focus:border-blue-500"
                    disabled={!!exportJobId && exportJob?.status === 'running'}
                  >
                    <option value="txt">TXT</option>
                    <option value="csv">CSV</option>
                    <option value="xls">XLS（兼容模式）</option>
                    <option value="xlsx">XLSX（兼容模式）</option>
                    <option value="json">JSON</option>
                    <option value="xml">XML</option>
                    <option value="sql">SQL</option>
                  </select>
                </div>

                <div className="grid grid-cols-2 gap-3">
                  <div>
                    <label className="block text-xs text-gray-400 mb-1">Primary Key</label>
                    <input
                      value={exportPrimaryKey}
                      onChange={(e) => setExportPrimaryKey(e.target.value)}
                      className="w-full bg-[#0d1117] border border-[#30363d] rounded px-3 py-2 text-sm text-gray-200 outline-none focus:border-blue-500"
                      placeholder="id"
                      disabled={!!exportJobId && exportJob?.status === 'running'}
                    />
                  </div>
                  <div className="flex gap-2">
                    <div className="flex-1">
                      <label className="block text-xs text-gray-400 mb-1">PK Start</label>
                      <input
                        value={exportPkStart}
                        onChange={(e) => setExportPkStart(e.target.value)}
                        className="w-full bg-[#0d1117] border border-[#30363d] rounded px-3 py-2 text-sm text-gray-200 outline-none focus:border-blue-500"
                        placeholder="1"
                        disabled={!!exportJobId && exportJob?.status === 'running'}
                      />
                    </div>
                    <div className="flex-1">
                      <label className="block text-xs text-gray-400 mb-1">PK End</label>
                      <input
                        value={exportPkEnd}
                        onChange={(e) => setExportPkEnd(e.target.value)}
                        className="w-full bg-[#0d1117] border border-[#30363d] rounded px-3 py-2 text-sm text-gray-200 outline-none focus:border-blue-500"
                        placeholder="1000"
                        disabled={!!exportJobId && exportJob?.status === 'running'}
                      />
                    </div>
                  </div>
                </div>

                <div>
                  <label className="block text-xs text-gray-400 mb-1">Where</label>
                  <textarea
                    value={exportWhere}
                    onChange={(e) => setExportWhere(e.target.value)}
                    className="w-full bg-[#0d1117] border border-[#30363d] rounded px-3 py-2 text-sm text-gray-200 outline-none focus:border-blue-500 min-h-[70px]"
                    placeholder="status = 1"
                    disabled={!!exportJobId && exportJob?.status === 'running'}
                  />
                </div>

                <div className="grid grid-cols-2 gap-3">
                  <div>
                    <label className="block text-xs text-gray-400 mb-1">Window Limit</label>
                    <input
                      value={exportWindowLimit}
                      onChange={(e) => setExportWindowLimit(e.target.value)}
                      className="w-full bg-[#0d1117] border border-[#30363d] rounded px-3 py-2 text-sm text-gray-200 outline-none focus:border-blue-500"
                      placeholder="10000"
                      disabled={!!exportJobId && exportJob?.status === 'running'}
                    />
                  </div>
                  <div>
                    <label className="block text-xs text-gray-400 mb-1">Window Offset</label>
                    <input
                      value={exportWindowOffset}
                      onChange={(e) => setExportWindowOffset(e.target.value)}
                      className="w-full bg-[#0d1117] border border-[#30363d] rounded px-3 py-2 text-sm text-gray-200 outline-none focus:border-blue-500"
                      placeholder="0"
                      disabled={!!exportJobId && exportJob?.status === 'running'}
                    />
                  </div>
                </div>

                {exportJobId && (
                  <div className="bg-[#0d1117] border border-[#30363d] rounded p-3 text-xs text-gray-300">
                    <div className="flex items-center justify-between">
                      <div>Job: <span className="text-white">{exportJobId}</span></div>
                      <div className="text-gray-400">{exportJob?.status}</div>
                    </div>
                    <div className="mt-2 text-gray-400">
                      {typeof exportJob?.progress?.current === 'number' ? `Progress: ${exportJob.progress.current}${typeof exportJob?.progress?.total === 'number' ? ` / ${exportJob.progress.total}` : ''}` : null}
                    </div>
                    <div className="mt-3 flex gap-2 justify-end">
                      <button
                        className="px-3 py-1 rounded border border-[#30363d] hover:bg-[#30363d]"
                        onClick={() => downloadExportArtifact('data')}
                        disabled={exportJob?.status !== 'completed'}
                      >
                        {tr('下载数据', 'Download Data')}
                      </button>
                      <button
                        className="px-3 py-1 rounded border border-[#30363d] hover:bg-[#30363d]"
                        onClick={() => downloadExportArtifact('manifest')}
                        disabled={exportJob?.status !== 'completed'}
                      >
                        {tr('下载清单', 'Download Manifest')}
                      </button>
                      <button
                        className="px-3 py-1 rounded border border-[#30363d] hover:bg-[#30363d] text-red-300"
                        onClick={cancelExportJob}
                        disabled={exportJob?.status !== 'running'}
                      >
                        {tr('取消任务', 'Cancel Job')}
                      </button>
                    </div>
                  </div>
                )}
              </div>
            )}
            {type === 'import' && (
              <ImportWizard 
                tableName={payload.tableName} 
                columns={payload.columns} 
                onComplete={() => {
                  toast(tr('导入成功', 'Import completed successfully'), 'success');
                  onClose();
                }} 
              />
            )}
            {type === 'parameterized-query' && (
              <div className="flex flex-col gap-4">
                <div className="text-sm text-gray-400 mb-2">
                  Please provide values for the following parameters:
                </div>
                {payload?.params?.map((param: string) => (
                  <div key={param} className="flex flex-col gap-2">
                    <label className="text-sm text-gray-300 font-medium">:{param}</label>
                    <input 
                      type="text" 
                      value={paramValues[param] || ''} 
                      onChange={(e) => setParamValues({ ...paramValues, [param]: e.target.value })}
                      className="bg-[#0d1117] border border-[#30363d] rounded px-3 py-2 text-sm text-white focus:outline-none focus:border-blue-500 w-full"
                      placeholder={`Value for :${param}`}
                    />
                  </div>
                ))}
              </div>
            )}

            {type !== 'import' && type !== 'parameterized-query' && (
              <>
                {result ? (
                  <div className="flex flex-col h-full gap-4">
                    <div className="flex flex-col gap-2">
                      <div className="text-sm text-green-400 font-medium">Result / DDL Statements:</div>
                      <textarea 
                        readOnly 
                        value={result} 
                        className="w-full flex-1 min-h-[200px] bg-[#0d1117] border border-[#30363d] rounded p-4 font-mono text-sm text-gray-300 outline-none"
                      />
                      <button 
                        onClick={handleDownload}
                        className="self-end px-4 py-2 mt-2 rounded bg-[#21262d] border border-[#30363d] hover:bg-[#30363d] text-sm text-gray-300 transition-colors"
                      >
                        Download Result
                      </button>
                    </div>
                  </div>
                ) : (
                  <div className="flex items-center justify-center py-12 text-gray-500">
                    Click Execute to start the {title} process.
                  </div>
                )}
              </>
            )}
          </div>

          {type !== 'import' && (
            <div className="bg-[#0d1117] px-6 py-4 flex justify-end border-t border-[#30363d] shrink-0">
              <button
                onClick={handleAction}
                disabled={loading || (type === 'export' && exportJob?.status === 'running')}
                className={`px-4 py-2 rounded-lg text-sm font-bold text-white shadow-lg transition-all flex items-center gap-2 ${
                  loading ? 'bg-blue-700 opacity-50 cursor-wait' : 'bg-blue-600 hover:bg-blue-500'
                }`}
              >
                <Play className="w-4 h-4 fill-current" />
                {loading ? 'Executing...' : (type === 'export' && exportJob?.status === 'running' ? 'Running...' : 'Execute')}
              </button>
            </div>
          )}
        </motion.div>
      </div>
    </AnimatePresence>
  );
}
