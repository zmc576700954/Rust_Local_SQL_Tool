import React, { useState } from 'react'
import { Database, Server, CheckCircle2, ChevronRight, Key, FileUp } from 'lucide-react'
import { api } from '../api'
import { parseError, type AppError } from '../utils'
import { tr } from '../i18n'

export function Onboarding({ onComplete }: { onComplete: () => void }) {
  const [step, setStep] = useState(1)
  const [host, setHost] = useState('127.0.0.1')
  const [port, setPort] = useState<number>(3306)
  const [showAdvanced, setShowAdvanced] = useState(false)
  const [username, setUsername] = useState('root')
  const [password, setPassword] = useState('')
  const [databases, setDatabases] = useState<string[]>([])
  const [selectedDatabase, setSelectedDatabase] = useState('')
  const [aiProvider, setAiProvider] = useState<'openai' | 'deepseek' | 'anthropic' | 'custom'>('openai')
  const [aiMode, setAiMode] = useState<'direct' | 'relay' | 'pool'>('direct')
  const [apiKey, setApiKey] = useState('')
  const [relayUrl, setRelayUrl] = useState('')
  const [tokensText, setTokensText] = useState('')
  
  const [isLoading, setIsLoading] = useState(false)
  const [errorObj, setErrorObj] = useState<AppError | null>(null)
  const [parsedConnections, setParsedConnections] = useState<any[]>([])

  type DbTestResponse = {
    success?: boolean
    databases?: string[]
    capabilities_probed?: boolean
    capabilities_ok?: boolean | null
    stage?: string
    server_version?: string | null
    diagnostic?: {
      message?: string
      hint?: string
      code?: string
      detail?: string
    }
  }

  const getDbTestSolutionByCode = (code?: string, fallback?: string) => {
    const normalized = String(code || '').toUpperCase()
    const map: Record<string, string> = {
      DB_TEST_AUTH_FAILED: tr('请核对数据库用户名密码及账号来源主机权限。', 'Verify DB username/password and account host privilege.'),
      DB_TEST_NETWORK_FAILED: tr('请检查数据库地址、端口、防火墙和白名单。', 'Check DB host/port, firewall, and whitelist.'),
      DB_TEST_SSL_FAILED: tr('请调整 SSL 模式并校验证书配置。', 'Adjust SSL mode and verify certificate settings.'),
      DB_TEST_CONNECT_TIMEOUT: tr('数据库连接超时，请检查网络与安全组策略。', 'Connection timed out, check network and security group rules.'),
      DB_TEST_CAPABILITY_PROBE_FAILED: tr('Capability probe failed. Check SHOW DATABASES permission or retry later.', 'Capability probe failed. Check SHOW DATABASES permission or retry later.'),
      DB_TEST_SSH_AUTH_FAILED: tr('SSH 认证失败，请检查 SSH 用户名和密码。', 'SSH authentication failed, verify SSH username/password.'),
      DB_TEST_SSH_CONNECT_FAILED: tr('SSH 网络失败，请检查 SSH 地址端口和网络连通性。', 'SSH network failed, check SSH host/port and connectivity.'),
      DB_TEST_SSH_CHANNEL_FAILED: tr('SSH 隧道通道创建失败，请检查跳板机到数据库的可达性。', 'SSH channel creation failed, verify bastion-to-DB reachability.'),
    }
    return map[normalized] || fallback || tr('请检查连接参数后重试。', 'Please verify connection settings and retry.')
  }

  const handleDbTest = async () => {
    setIsLoading(true)
    setErrorObj(null)
    setDatabases([])
    setSelectedDatabase('')
    try {
      const res = await api.dbTest({
        host: host.trim(),
        port: showAdvanced ? port : 3306,
        username: username.trim(),
        password,
        probe_capabilities: true,
      }) as DbTestResponse
      if (res?.success === false) {
        setErrorObj({
          title: res?.diagnostic?.code || 'DB Test Failed',
          message: res?.diagnostic?.message || 'Connection failed',
          solution: getDbTestSolutionByCode(
            res?.diagnostic?.code,
            res?.diagnostic?.hint || res?.diagnostic?.detail || ''
          ),
        })
        return
      }
      if (res?.capabilities_probed && res?.capabilities_ok === false) {
        setErrorObj({
          title: res?.diagnostic?.code || 'Capability Probe Failed',
          message: res?.diagnostic?.message || 'Connection successful, but capability probe failed.',
          solution: getDbTestSolutionByCode(
            res?.diagnostic?.code,
            res?.diagnostic?.hint || res?.diagnostic?.detail || ''
          ),
        })
        return
      }
      const list = Array.isArray(res.databases) ? res.databases : []
      if (list.length === 0) {
        setErrorObj({
          title: 'Database List Empty',
          message: 'Connection successful, but no databases were returned.',
          solution: 'Check SHOW DATABASES permission or metadata visibility.'
        })
        return
      }
      setDatabases(list)
      setSelectedDatabase(list.includes('mysql') ? 'mysql' : list[0])
    } catch (e: any) {
      setErrorObj(parseError(e))
    } finally {
      setIsLoading(false)
    }
  }

  const handleFileUpload = async (e: React.ChangeEvent<HTMLInputElement>) => {
    const file = e.target.files?.[0]
    if (!file) return
    
    setIsLoading(true)
    setErrorObj(null)
    
    try {
      const text = await file.text()
      const result = await api.parseNavicat(text)
      if (result.connections && result.connections.length > 0) {
        setParsedConnections(result.connections)
      } else {
        setErrorObj({
          title: 'Navicat 导入失败',
          message: '未找到 MySQL/MariaDB 连接',
          solution: '请确保您导出的 NCX 文件中包含至少一个有效的 MySQL 或 MariaDB 连接。'
        })
      }
    } catch (e: any) {
      setErrorObj(parseError(e))
    } finally {
      setIsLoading(false)
      // Reset input
      if (e.target) e.target.value = ''
    }
  }

  const applyNavicatConnection = (conn: any) => {
    setHost(conn.host || '127.0.0.1')
    setPort(Number(conn.port || 3306))
    setUsername(conn.username || 'root')
    setPassword('')
    setDatabases([])
    setSelectedDatabase('')
    setParsedConnections([])
  }

  const handleSave = async () => {
    setIsLoading(true)
    setErrorObj(null)
    try {
      const safeUser = encodeURIComponent(username.trim())
      const safePass = encodeURIComponent(password)
      const dbPort = showAdvanced ? port : 3306
      const authPart = password ? `${safeUser}:${safePass}` : safeUser
      const dbUrl = selectedDatabase
        ? `mysql://${authPart}@${host.trim()}:${dbPort}/${encodeURIComponent(selectedDatabase)}`
        : undefined

      const config = {
        db_url: dbUrl,
        ai_provider: aiProvider,
        ai_mode: aiMode,
        api_key: apiKey || undefined,
        relay_url: (aiMode === 'relay' || aiMode === 'pool') && relayUrl.trim() ? relayUrl.trim() : undefined,
        token_pool: aiMode === 'pool' ? tokensText.split('\n').map(t => t.trim()).filter(t => t) : [],
        model_name: 'gpt-3.5-turbo'
      }
      
      await api.updateConfig(config)
      onComplete()
    } catch (e: any) {
      setErrorObj(parseError(e))
    } finally {
      setIsLoading(false)
    }
  }

  return (
    <div className="fixed inset-0 bg-[#0a0c10] z-50 flex items-center justify-center p-6">
      <div className="w-full max-w-3xl bg-[#161b22] border border-[#30363d] rounded-2xl shadow-2xl overflow-hidden flex flex-col min-h-[600px]">
        
        {/* Header */}
        <div className="px-8 py-6 border-b border-[#30363d] bg-gradient-to-r from-[#161b22] to-[#1e242d]">
          <div className="flex items-center gap-3">
            <div className="p-2.5 bg-blue-500/10 rounded-xl border border-blue-500/20">
              <Database className="w-8 h-8 text-blue-400" />
            </div>
            <div>
              <h1 className="text-2xl font-bold text-white tracking-tight">Welcome to Local AI SQL</h1>
              <p className="text-gray-400 mt-1">Let's connect your local environment in 2 easy steps.</p>
            </div>
          </div>
        </div>

        {/* Content */}
        <div className="flex-1 p-8 flex flex-col">
          {errorObj && (
            <div className="mb-6 p-4 bg-red-500/10 border border-red-500/20 rounded-lg">
              <h3 className="text-red-400 font-medium mb-1">{errorObj.title || 'Error'}</h3>
              <p className="text-red-400/80 text-sm mb-2">{errorObj.message}</p>
              {errorObj.solution && (
                <p className="text-red-400/60 text-xs">{errorObj.solution}</p>
              )}
            </div>
          )}

          {step === 1 ? (
            <div className="flex-1 animation-fade-in">
              <h2 className="text-lg font-semibold text-white mb-6 flex items-center gap-2">
                <span className="flex items-center justify-center w-6 h-6 rounded-full bg-blue-500/20 text-blue-400 text-sm">1</span>
                Connect Database
              </h2>
              
              <div className="grid grid-cols-2 gap-4 mb-8">
                <div className="p-4 border border-[#30363d] bg-[#0d1117] rounded-xl hover:border-blue-500/50 cursor-pointer transition-colors relative overflow-hidden group">
                  <div className="absolute top-0 left-0 w-1 h-full bg-blue-500"></div>
                  <Server className="w-6 h-6 text-gray-400 group-hover:text-blue-400 transition-colors mb-3" />
                  <h3 className="text-white font-medium mb-1">Local MySQL / MariaDB</h3>
                  <p className="text-xs text-gray-500">Connect directly to your local database service.</p>
                </div>
                
                <div className="relative p-4 border border-[#30363d] bg-[#0d1117] rounded-xl hover:border-blue-500/50 cursor-pointer transition-colors overflow-hidden group">
                  <Database className="w-6 h-6 text-gray-500 group-hover:text-blue-400 mb-3 transition-colors" />
                  <h3 className="text-white font-medium mb-1">Import from Navicat (.ncx)</h3>
                  <p className="text-xs text-gray-500">{tr('上传导出的连接配置。', 'Upload exported connections.')}</p>
                  <input 
                    type="file" 
                    accept=".ncx,.xml"
                    onChange={handleFileUpload}
                    className="absolute inset-0 opacity-0 cursor-pointer"
                    title={tr('上传 Navicat NCX 文件', 'Upload Navicat NCX file')}
                  />
                </div>
              </div>

              {parsedConnections.length > 0 && (
                <div className="mb-6 p-4 border border-blue-500/30 bg-blue-500/5 rounded-xl animation-fade-in">
                  <h4 className="text-sm font-medium text-blue-400 mb-3">Select a connection to import:</h4>
                  <div className="space-y-2 max-h-[150px] overflow-y-auto">
                    {parsedConnections.map((conn, idx) => (
                      <div 
                        key={idx} 
                        onClick={() => applyNavicatConnection(conn)}
                        className="flex items-center justify-between p-2.5 bg-[#0d1117] border border-[#30363d] rounded-lg hover:border-blue-500 cursor-pointer"
                      >
                        <div>
                          <div className="text-sm font-medium text-gray-200">{conn.name}</div>
                          <div className="text-xs text-gray-500">{conn.username}@{conn.host}:{conn.port}</div>
                        </div>
                        <FileUp className="w-4 h-4 text-blue-500 opacity-50" />
                      </div>
                    ))}
                  </div>
                </div>
              )}

              <div className="space-y-4">
                <div className="mb-4 p-3 bg-blue-500/10 border border-blue-500/20 rounded-lg flex items-start gap-2">
                  <div className="text-blue-400 mt-0.5">💡</div>
                  <div className="text-xs text-blue-300/80 leading-relaxed">
                    初次使用？您可以直接输入数据库地址密码，或者导入 Navicat 的连接配置。若当前无法连接真实库，也可直接点击底部的 <b>{tr('“跳过数据库”', '"Skip Database"')}</b> 跳过此步。
                  </div>
                </div>

                <div>
                  <label className="text-sm font-medium text-gray-300 mb-1.5 flex items-center gap-1.5">
                    Server Address
                    <span className="text-xs font-normal text-gray-500 bg-gray-800/50 px-2 py-0.5 rounded-full border border-gray-700/50">
                      服务器IP或域名
                    </span>
                  </label>
                  <input
                    type="text"
                    value={host}
                    onChange={e => setHost(e.target.value)}
                    className="w-full bg-[#0d1117] border border-[#30363d] rounded-lg px-4 py-2.5 text-white text-sm focus:outline-none focus:border-blue-500 transition-colors"
                    placeholder="127.0.0.1"
                  />
                </div>
                <div className="grid grid-cols-2 gap-4">
                  <div>
                    <label className="block text-sm font-medium text-gray-300 mb-1.5">Username</label>
                    <input
                      type="text"
                      value={username}
                      onChange={e => setUsername(e.target.value)}
                      className="w-full bg-[#0d1117] border border-[#30363d] rounded-lg px-4 py-2.5 text-white text-sm focus:outline-none focus:border-blue-500 transition-colors"
                      placeholder="root"
                    />
                  </div>
                  <div>
                    <label className="block text-sm font-medium text-gray-300 mb-1.5">Password</label>
                    <input
                      type="password"
                      value={password}
                      onChange={e => setPassword(e.target.value)}
                      className="w-full bg-[#0d1117] border border-[#30363d] rounded-lg px-4 py-2.5 text-white text-sm focus:outline-none focus:border-blue-500 transition-colors"
                      placeholder="••••••••"
                    />
                  </div>
                </div>

                <div className="flex items-center justify-between">
                  <button
                    type="button"
                    onClick={() => setShowAdvanced(v => !v)}
                    className="text-xs text-gray-400 hover:text-gray-200"
                  >
                    {showAdvanced ? 'Hide Advanced' : 'Show Advanced'}
                  </button>
                  <button
                    type="button"
                    onClick={handleDbTest}
                    disabled={isLoading || !host.trim() || !username.trim()}
                    className="px-4 py-2 bg-dark-panel border border-dark-border hover:bg-[#21262d] rounded text-sm text-gray-200 disabled:opacity-50"
                  >
                    {isLoading ? 'Testing...' : 'Test Connection'}
                  </button>
                </div>

                {showAdvanced && (
                  <div className="grid grid-cols-2 gap-4">
                    <div>
                      <label className="block text-sm font-medium text-gray-300 mb-1.5">Port</label>
                      <input
                        type="number"
                        value={port}
                        onChange={e => setPort(Number(e.target.value || 3306))}
                        className="w-full bg-[#0d1117] border border-[#30363d] rounded-lg px-4 py-2.5 text-white text-sm focus:outline-none focus:border-blue-500 transition-colors"
                        placeholder="3306"
                      />
                    </div>
                    <div></div>
                  </div>
                )}

                {databases.length > 0 && (
                  <div className="animation-fade-in">
                    <label className="block text-sm font-medium text-gray-300 mb-1.5">Database</label>
                    <select
                      value={selectedDatabase}
                      onChange={e => setSelectedDatabase(e.target.value)}
                      className="w-full bg-[#0d1117] border border-[#30363d] rounded-lg px-4 py-2.5 text-white text-sm focus:outline-none focus:border-blue-500 transition-colors"
                    >
                      {databases.map(db => (
                        <option key={db} value={db}>{db}</option>
                      ))}
                    </select>
                  </div>
                )}
              </div>
            </div>
          ) : (
            <div className="flex-1 animation-fade-in">
              <h2 className="text-lg font-semibold text-white mb-6 flex items-center gap-2">
                <span className="flex items-center justify-center w-6 h-6 rounded-full bg-blue-500/20 text-blue-400 text-sm">2</span>
                Configure AI Connection
              </h2>

              <div className="space-y-4">
                <div className="animation-fade-in">
                  <label className="text-sm font-medium text-gray-300 mb-1.5 flex items-center gap-1.5">
                    AI Provider
                    <span className="text-xs font-normal text-gray-500 bg-gray-800/50 px-2 py-0.5 rounded-full border border-gray-700/50">
                      选择您的大模型提供商
                    </span>
                  </label>
                  <select
                    value={aiProvider}
                    onChange={e => setAiProvider(e.target.value as any)}
                    className="w-full bg-[#0d1117] border border-[#30363d] rounded-lg px-4 py-2.5 text-white text-sm focus:outline-none focus:border-blue-500 transition-colors"
                  >
                    <option value="openai">OpenAI (ChatGPT)</option>
                    <option value="deepseek">DeepSeek</option>
                    <option value="anthropic">Anthropic (Claude)</option>
                    <option value="custom">Custom (如 Ollama, 本地代理等)</option>
                  </select>
                </div>

                <div className="animation-fade-in mt-4">
                  <label className="text-sm font-medium text-gray-300 mb-1.5 flex items-center gap-1.5">
                    Connection Mode
                    <span className="text-xs font-normal text-gray-500 bg-gray-800/50 px-2 py-0.5 rounded-full border border-gray-700/50">
                      直连 / 单Key中转 / 多Key池化轮询
                    </span>
                  </label>
                  <div className="flex bg-[#0d1117] p-1 rounded-lg border border-[#30363d]">
                    {(['direct', 'relay', 'pool'] as const).map(mode => (
                      <button
                        key={mode}
                        onClick={() => setAiMode(mode)}
                        className={`flex-1 py-2 text-sm font-medium rounded-md capitalize transition-colors ${
                          aiMode === mode ? 'bg-[#21262d] text-white shadow-sm' : 'text-gray-400 hover:text-gray-200'
                        }`}
                      >
                        {mode}
                      </button>
                    ))}
                  </div>
                </div>

                {aiMode === 'direct' && (
                  <div className="animation-fade-in mt-4">
                    <label className="text-sm font-medium text-gray-300 mb-1.5 flex items-center gap-1.5">
                      API Key
                    </label>
                    <div className="relative">
                      <Key className="absolute left-3 top-2.5 w-4 h-4 text-gray-500" />
                      <input 
                        type="password" 
                        value={apiKey}
                        onChange={e => setApiKey(e.target.value)}
                        className="w-full bg-[#0d1117] border border-[#30363d] rounded-lg pl-10 pr-4 py-2.5 text-white text-sm focus:outline-none focus:border-blue-500"
                        placeholder="填入您的 API Key (如 sk-...)"
                      />
                    </div>
                  </div>
                )}

                {(aiMode === 'relay' || aiMode === 'pool') && (
                  <div className="animation-fade-in mt-4">
                    <label className="text-sm font-medium text-gray-300 mb-1.5 flex flex-col items-start gap-1">
                      <div className="flex items-center gap-1.5">
                        Base URL (Proxy / Relay)
                        <span className="text-xs font-normal text-gray-500 bg-gray-800/50 px-2 py-0.5 rounded-full border border-gray-700/50">
                          代理或中转地址
                        </span>
                      </div>
                      <span className="text-[11px] text-gray-500 font-normal">若无法直连官方，或使用自定义模型，请在此填入代理商提供的地址（如 https://api.xxx.com/v1）</span>
                    </label>
                    <input 
                      type="text" 
                      value={relayUrl}
                      onChange={e => setRelayUrl(e.target.value)}
                      className="w-full bg-[#0d1117] border border-[#30363d] rounded-lg px-4 py-2.5 text-white text-sm focus:outline-none focus:border-blue-500"
                      placeholder="留空则使用厂商默认 URL"
                    />
                  </div>
                )}

                {aiMode === 'relay' && (
                  <div className="animation-fade-in mt-4">
                    <label className="block text-sm font-medium text-gray-300 mb-1.5">API Key (Optional for some relays)</label>
                    <input 
                      type="password" 
                      value={apiKey}
                      onChange={e => setApiKey(e.target.value)}
                      className="w-full bg-[#0d1117] border border-[#30363d] rounded-lg px-4 py-2.5 text-white text-sm focus:outline-none focus:border-blue-500"
                    />
                  </div>
                )}

                {aiMode === 'pool' && (
                  <div className="animation-fade-in mt-4">
                    <label className="block text-sm font-medium text-gray-300 mb-1.5">Token Pool (One token per line)</label>
                    <textarea 
                      value={tokensText}
                      onChange={e => setTokensText(e.target.value)}
                      rows={4}
                      className="w-full bg-[#0d1117] border border-[#30363d] rounded-lg px-4 py-2.5 text-white text-sm focus:outline-none focus:border-blue-500 font-mono resize-none"
                      placeholder="sk-token1...\nsk-token2...\n"
                    />
                    <p className="text-xs text-gray-500 mt-2">
                      Requests will be round-robined across these tokens. Automatically handles 429 Too Many Requests.
                    </p>
                  </div>
                )}
              </div>
            </div>
          )}
        </div>

        {/* Footer Actions */}
        <div className="px-8 py-5 border-t border-[#30363d] bg-[#0d1117] flex items-center justify-between">
          {step === 2 ? (
            <button 
              onClick={() => setStep(1)}
              className="px-5 py-2 text-sm font-medium text-gray-400 hover:text-white transition-colors"
            >
              {tr('上一步', 'Back')}
            </button>
          ) : <div></div>}
          
          {step === 1 ? (
            <div className="flex items-center gap-3">
              <button
                onClick={() => {
                  setDatabases([])
                  setSelectedDatabase('')
                  setStep(2)
                }}
                className="px-5 py-2.5 text-sm font-medium text-gray-300 bg-dark-panel border border-dark-border hover:bg-[#21262d] rounded-lg transition-colors"
              >
                {tr('跳过数据库', 'Skip Database')}
              </button>
              <button 
                onClick={() => setStep(2)}
                className="flex items-center gap-2 px-6 py-2.5 bg-blue-600 hover:bg-blue-500 text-white rounded-lg font-medium transition-colors"
              >
                {tr('继续', 'Continue')} <ChevronRight className="w-4 h-4" />
              </button>
            </div>
          ) : (
            <button 
              onClick={handleSave}
              disabled={isLoading}
              className="flex items-center gap-2 px-6 py-2.5 bg-green-600 hover:bg-green-500 text-white rounded-lg font-medium transition-colors disabled:opacity-50"
            >
              {isLoading ? tr('保存中...', 'Saving...') : tr('保存并启动', 'Save & Launch')}
              {!isLoading && <CheckCircle2 className="w-4 h-4" />}
            </button>
          )}
        </div>

      </div>
    </div>
  )
}
