import { useEffect, useState } from 'react';
import { Upload, ArrowRight, Check, AlertCircle } from 'lucide-react';
import { api, JOB_POLL_INTERVAL_MS } from '../api';
import { useToast } from './Toast';
import { tr } from '../i18n';

interface ImportWizardProps {
  tableName: string;
  columns: any[];
  onComplete: () => void;
}

export function ImportWizard({ tableName, columns, onComplete }: ImportWizardProps) {
  const { toast } = useToast();
  const [file, setFile] = useState<File | null>(null);
  const [sourceHeaders, setSourceHeaders] = useState<string[]>([]);
  const [parsedData, setParsedData] = useState<any[]>([]);
  const [mapping, setMapping] = useState<Record<string, string>>({}); // dbColumn -> sourceHeader
  const [step, setStep] = useState(1);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState('');
  const [skipErrors, setSkipErrors] = useState(false);
  const [jobId, setJobId] = useState<string | null>(null);
  const [job, setJob] = useState<any | null>(null);
  const [resumeJobId, setResumeJobId] = useState('');
  const [pollInterrupted, setPollInterrupted] = useState<{ job_id: string; message: string } | null>(null);
  const [pollKey, setPollKey] = useState(0);
  const [sqlText, setSqlText] = useState('');
  const isSqlFile = !!file?.name.endsWith('.sql');

  useEffect(() => {
    const saved = window.localStorage.getItem(`import:lastJobId:${tableName}`) || '';
    if (saved) setResumeJobId(saved);
  }, [tableName]);

  const handleFileChange = async (e: React.ChangeEvent<HTMLInputElement>) => {
    const selected = e.target.files?.[0];
    if (!selected) return;
    setFile(selected);
    
    try {
      const lowerName = selected.name.toLowerCase();
      const headBuf = await selected.slice(0, 8).arrayBuffer();
      const head = new Uint8Array(headBuf);
      const isZip =
        head.length >= 4 &&
        head[0] === 0x50 &&
        head[1] === 0x4b &&
        head[2] === 0x03 &&
        head[3] === 0x04;
      const isOle =
        head.length >= 8 &&
        head[0] === 0xd0 &&
        head[1] === 0xcf &&
        head[2] === 0x11 &&
        head[3] === 0xe0 &&
        head[4] === 0xa1 &&
        head[5] === 0xb1 &&
        head[6] === 0x1a &&
        head[7] === 0xe1;

      if (lowerName.endsWith('.xlsx') && isZip) {
        throw new Error('检测到真实 XLSX（二进制 ZIP）。当前仅支持兼容模式：文件内容为 CSV/TSV 纯文本，请在 Excel 中“另存为 CSV/TSV”，或使用本工具导出的 xlsx（兼容模式）。');
      }
      if (lowerName.endsWith('.xls') && isOle) {
        throw new Error('检测到真实 XLS（二进制 OLE）。当前仅支持兼容模式：文件内容为 CSV/TSV 纯文本，请在 Excel 中“另存为 CSV/TSV”，或使用本工具导出的 xls（兼容模式）。');
      }

      const text = await selected.text();
      let headers: string[] = [];
      let data: any[] = [];
      
      if (lowerName.endsWith('.json')) {
        const json = JSON.parse(text);
        if (Array.isArray(json) && json.length > 0) {
          headers = Object.keys(json[0]);
          data = json;
        } else {
          throw new Error('Invalid JSON format: expected array of objects');
        }
      } else if (lowerName.endsWith('.csv')) {
        const rows = text.split('\n').filter(r => r.trim());
        if (rows.length > 0) {
          headers = rows[0].split(',').map(h => h.trim().replace(/^"|"$/g, ''));
          data = rows.slice(1).map(row => {
            const values = row.split(',').map(v => v.trim().replace(/^"|"$/g, ''));
            const obj: any = {};
            headers.forEach((h, i) => {
              obj[h] = values[i];
            });
            return obj;
          });
        }
      } else if (lowerName.endsWith('.txt') || lowerName.endsWith('.xls') || lowerName.endsWith('.xlsx')) {
        const rows = text.split('\n').filter(r => r.trim());
        if (rows.length > 0) {
          const first = rows[0];
          const delimiter = first.includes('\t') ? '\t' : (first.includes(',') ? ',' : null);
          if (!delimiter) {
            throw new Error('无法识别分隔符：首行未发现 \\t 或 ,。当前仅支持 xls/xlsx 兼容模式（CSV/TSV 纯文本）。');
          }
          headers = rows[0].split(delimiter).map(h => h.trim().replace(/^"|"$/g, ''));
          data = rows.slice(1).map(row => {
            const values = row.split(delimiter).map(v => v.trim().replace(/^"|"$/g, ''));
            const obj: any = {};
            headers.forEach((h, i) => {
              obj[h] = values[i];
            });
            return obj;
          });
        }
      } else if (lowerName.endsWith('.xml')) {
        const parser = new DOMParser();
        const doc = parser.parseFromString(text, 'application/xml');
        const parseError = doc.querySelector('parsererror');
        if (parseError) throw new Error('Invalid XML format');

        const colNodes = Array.from(doc.querySelectorAll('export > columns > column'));
        headers = colNodes.map(n => n.getAttribute('name') || '').filter(Boolean);

        const rowNodes = Array.from(doc.querySelectorAll('export > rows > row'));
        data = rowNodes.map(r => {
          const obj: any = {};
          const cols = Array.from(r.querySelectorAll('col'));
          cols.forEach(c => {
            const name = c.getAttribute('name');
            if (!name) return;
            obj[name] = c.textContent ?? '';
          });
          return obj;
        });

        if (headers.length === 0 && data.length > 0) {
          headers = Object.keys(data[0]);
        }
      } else if (lowerName.endsWith('.sql')) {
        setSqlText(text);
        setSourceHeaders([]);
        setParsedData([]);
        setMapping({});
        setStep(3);
        setError('');
        return;
      } else {
        throw new Error('不支持的文件格式。支持 TXT/CSV/JSON/XML/SQL，以及 xls/xlsx（兼容模式：CSV/TSV 纯文本）。');
      }
      
      setSourceHeaders(headers);
      setParsedData(data);
      setSqlText('');
      
      // Auto map matching names
      const initialMapping: Record<string, string> = {};
      columns.forEach(c => {
        const match = headers.find(h => h.toLowerCase() === c.column_name.toLowerCase());
        if (match) {
          initialMapping[c.column_name] = match;
        }
      });
      setMapping(initialMapping);
      setStep(2);
      setError('');
    } catch (err: any) {
      setError(err.message || 'Error parsing file');
      toast(err.message || 'Error parsing file', 'error');
      setFile(null);
    }
  };

  useEffect(() => {
    if (!jobId) return;
    let alive = true;
    window.localStorage.setItem(`import:lastJobId:${tableName}`, jobId);
    const tick = async () => {
      try {
        const j = await api.toolJobStatus(jobId);
        if (!alive) return;
        setJob(j);
        if (j?.status === 'completed') {
          toast(tr('导入成功', 'Import completed successfully'), 'success');
          onComplete();
          setLoading(false);
          setPollInterrupted(null);
          alive = false;
        } else if (j?.status === 'error' || j?.status === 'canceled') {
          setError(j?.error || tr('导入失败', 'Import failed'));
          setLoading(false);
          setPollInterrupted(null);
          alive = false;
        }
      } catch (e: any) {
        if (!alive) return;
        setPollInterrupted({ job_id: jobId, message: e?.response?.data?.message || e?.message || tr('获取任务状态失败', 'Failed to fetch job status') });
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
  }, [jobId, pollKey, onComplete, toast, tableName]);

  const handleImport = async () => {
    setLoading(true);
    setError('');
    setPollInterrupted(null);
    try {
      const res = isSqlFile
        ? await api.importSqlJobStart({ sql: sqlText, force: true })
        : await api.importJobStart({ table_name: tableName, data: parsedData, mapping, skip_errors: skipErrors });
      setJobId(res.job_id);
      setResumeJobId(res.job_id);
    } catch (err: any) {
      const msg = err?.response?.data?.message || err?.response?.data?.error || err?.message || tr('导入失败', 'Import failed');
      setError(msg);
      toast(msg, 'error');
      setLoading(false);
    }
  };

  const handleCancelJob = async () => {
    if (!jobId) return;
    setLoading(true);
    try {
      await api.toolJobCancel(jobId);
    } finally {
      setLoading(false);
    }
  };

  const restartPolling = () => setPollKey(k => k + 1);

  const resumePolling = () => {
    const id = resumeJobId.trim();
    if (!id) return;
    setJobId(id);
    setJob(null);
    setError('');
    setPollInterrupted(null);
    setLoading(true);
    restartPolling();
  };

  const continuePolling = () => {
    if (!jobId) return;
    setPollInterrupted(null);
    setLoading(true);
    restartPolling();
  };

  return (
    <div className="flex flex-col gap-6 text-sm text-gray-300">
      <div className="bg-[#0d1117] border border-[#30363d] rounded p-3">
        <div className="text-xs text-gray-400">{tr('恢复导入任务', 'Resume Import Job')}</div>
        <div className="mt-2 flex items-center gap-2">
          <input
            value={resumeJobId}
            onChange={e => setResumeJobId(e.target.value)}
            placeholder="输入 job_id"
            className="flex-1 bg-[#0d1117] border border-[#30363d] rounded px-3 py-2 text-sm text-gray-200 outline-none focus:border-blue-500"
            disabled={loading}
          />
          <button
            className="px-3 py-2 rounded border border-[#30363d] hover:bg-[#30363d] text-sm text-gray-200"
            onClick={resumePolling}
            disabled={!resumeJobId.trim() || loading}
          >
            恢复查看
          </button>
        </div>
      </div>

      {pollInterrupted && (
        <div className="border border-yellow-500/30 bg-yellow-500/10 rounded p-3">
          <div className="text-xs text-yellow-200">轮询中断</div>
          <div className="mt-1 text-xs text-yellow-100 whitespace-pre-wrap">{pollInterrupted.message}</div>
          <div className="mt-2 flex items-center justify-between gap-2">
            <div className="text-xs text-yellow-100 font-mono">job_id={pollInterrupted.job_id}</div>
            <button className="px-3 py-2 rounded bg-blue-600 hover:bg-blue-500 text-sm text-white" onClick={continuePolling}>
              继续轮询
            </button>
          </div>
        </div>
      )}

      {/* Step Indicators */}
      <div className="flex items-center justify-between border-b border-[#30363d] pb-4">
        <div className={`flex items-center gap-2 ${step >= 1 ? 'text-blue-400' : 'text-gray-500'}`}>
          <div className="w-6 h-6 rounded-full flex items-center justify-center border border-current">1</div>
          <span>{tr('上传', 'Upload')}</span>
        </div>
        <div className={`w-12 h-[1px] ${step >= 2 ? 'bg-blue-400' : 'bg-gray-600'}`}></div>
        <div className={`flex items-center gap-2 ${step >= 2 ? 'text-blue-400' : 'text-gray-500'}`}>
          <div className="w-6 h-6 rounded-full flex items-center justify-center border border-current">2</div>
          <span>{tr('字段映射', 'Map Fields')}</span>
        </div>
        <div className={`w-12 h-[1px] ${step >= 3 ? 'bg-blue-400' : 'bg-gray-600'}`}></div>
        <div className={`flex items-center gap-2 ${step >= 3 ? 'text-blue-400' : 'text-gray-500'}`}>
          <div className="w-6 h-6 rounded-full flex items-center justify-center border border-current">3</div>
          <span>{tr('导入', 'Import')}</span>
        </div>
      </div>

      {error && (
        <div className="bg-red-500/10 border border-red-500/50 text-red-400 p-3 rounded flex items-start gap-2">
          <AlertCircle className="w-4 h-4 mt-0.5 shrink-0" />
          <span>{error}</span>
        </div>
      )}

      {step === 1 && (
        <div className="flex flex-col items-center justify-center py-12 border-2 border-dashed border-[#30363d] rounded-lg hover:border-blue-500/50 transition-colors">
          <Upload className="w-8 h-8 mb-4 text-gray-400" />
          <p className="mb-2">{tr('拖拽文件到此处或点击上传', 'Drag and drop or click to upload')}</p>
          <p className="text-xs text-gray-500 mb-6">{tr('支持 TXT / CSV / JSON / XML / SQL / XLS / XLSX（兼容模式）', 'Supports TXT / CSV / JSON / XML / SQL / XLS / XLSX (compat mode)')}</p>
          <input 
            type="file" 
            accept=".txt,.csv,.json,.xml,.sql,.xls,.xlsx"
            onChange={handleFileChange}
            className="block w-full max-w-xs text-sm text-gray-400 file:mr-4 file:py-2 file:px-4 file:rounded file:border-0 file:text-sm file:font-semibold file:bg-blue-500/20 file:text-blue-400 hover:file:bg-blue-500/30 cursor-pointer"
          />
        </div>
      )}

      {step === 2 && (
        <div className="flex flex-col gap-4">
          <div className="grid grid-cols-2 gap-4 font-semibold text-gray-400 mb-2">
            <div>{tr('目标列（数据库）', 'Target Column (DB)')}</div>
            <div>{tr('源字段（文件）', 'Source Field (File)')}</div>
          </div>
          <div className="max-h-[300px] overflow-y-auto pr-2 flex flex-col gap-3">
            {columns.map(col => (
              <div key={col.column_name} className="grid grid-cols-2 gap-4 items-center">
                <div className="flex items-center gap-2">
                  <span className="font-mono text-blue-300">{col.column_name}</span>
                  <span className="text-xs text-gray-500">{col.column_type}</span>
                  {col.is_nullable === 'NO' && <span className="text-xs text-red-400" title={tr('必填', 'Required')}>*</span>}
                </div>
                <select 
                  value={mapping[col.column_name] || ''}
                  onChange={e => setMapping({ ...mapping, [col.column_name]: e.target.value })}
                  className="bg-[#0d1117] border border-[#30363d] rounded px-2 py-1.5 focus:outline-none focus:border-blue-500"
                >
                  <option value="">{tr('-- 跳过 / 默认 --', '-- Skip / Default --')}</option>
                  {sourceHeaders.map(h => (
                    <option key={h} value={h}>{h}</option>
                  ))}
                </select>
              </div>
            ))}
          </div>
          <div className="flex justify-end mt-4">
            <button 
              className="bg-blue-600 hover:bg-blue-500 text-white px-4 py-2 rounded flex items-center gap-2"
              onClick={() => setStep(3)}
            >
              {tr('下一步', 'Next')} <ArrowRight className="w-4 h-4" />
            </button>
          </div>
        </div>
      )}

      {step === 3 && (
        <div className="flex flex-col gap-6">
          <div className="bg-[#0d1117] border border-[#30363d] rounded p-4">
            <h3 className="font-semibold text-white mb-2">{tr('导入摘要', 'Import Summary')}</h3>
            <ul className="space-y-2">
              <li>{tr('文件', 'File')}: <span className="text-white">{file?.name}</span></li>
              {!isSqlFile && (
                <>
                  <li>{tr('待导入行数', 'Rows to import')}: <span className="text-white">{parsedData.length}</span></li>
                  <li>{tr('已映射列数', 'Mapped columns')}: <span className="text-white">{Object.values(mapping).filter(Boolean).length} / {columns.length}</span></li>
                </>
              )}
              {isSqlFile && (
                <li>{tr('模式', 'Mode')}: <span className="text-white">{tr('执行 SQL 脚本', 'Execute SQL script')}</span></li>
              )}
            </ul>
          </div>
          
          {!isSqlFile && (
            <label className="flex items-center gap-2 cursor-pointer">
              <input 
                type="checkbox" 
                checked={skipErrors}
                onChange={e => setSkipErrors(e.target.checked)}
                className="rounded bg-[#0d1117] border-[#30363d] text-blue-500 focus:ring-blue-500 focus:ring-offset-[#161b22]"
              />
              <span>{tr('跳过错误行（发生错误时继续）', 'Skip rows with errors (continue on error)')}</span>
            </label>
          )}

          {jobId && (
            <div className="bg-[#0d1117] border border-[#30363d] rounded p-4 text-xs text-gray-300">
              <div className="flex items-center justify-between">
                <div>{tr('任务', 'Job')}: <span className="text-white">{jobId}</span></div>
                <div className="text-gray-400">{job?.status}</div>
              </div>
              <div className="mt-2 text-gray-400">
                {typeof job?.progress?.current === 'number' ? `Progress: ${job.progress.current}${typeof job?.progress?.total === 'number' ? ` / ${job.progress.total}` : ''}` : null}
              </div>
            </div>
          )}

          <div className="flex justify-end gap-3 mt-2">
            <button 
              className="px-4 py-2 rounded border border-[#30363d] hover:bg-[#30363d]"
              onClick={() => setStep(2)}
              disabled={loading || isSqlFile || !!jobId}
            >
              {tr('上一步', 'Back')}
            </button>
            {jobId && (
              <button
                className="px-4 py-2 rounded border border-[#30363d] hover:bg-[#30363d] text-red-300"
                onClick={handleCancelJob}
                disabled={loading}
              >
                {tr('取消任务', 'Cancel Job')}
              </button>
            )}
            <button 
              className="bg-blue-600 hover:bg-blue-500 text-white px-4 py-2 rounded flex items-center gap-2 disabled:opacity-50"
              onClick={handleImport}
              disabled={loading || !!jobId}
            >
              {loading ? tr('启动中...', 'Starting...') : tr('开始导入', 'Start Import')}
              {!loading && <Check className="w-4 h-4" />}
            </button>
          </div>
        </div>
      )}
    </div>
  );
}
