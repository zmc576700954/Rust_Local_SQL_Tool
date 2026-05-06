import { useMemo, useState, useEffect } from 'react'
import { motion } from 'framer-motion'
import { X, Save, RefreshCw, Archive, Database, Server, Cpu, HeartPulse, Plus, Pencil, Trash2, CheckCircle2, AlertTriangle } from 'lucide-react'
import { api } from '../api'
import { parseError, sanitizeForLog } from '../utils'
import { useToast } from './Toast'
import { dbLevelDisplayName, dbTypeDisplayName } from '../utils/dbCapabilities'

export function SettingsPanel({ onClose, onPolicyChange, onConfigChange }: { onClose: () => void, onPolicyChange: () => void, onConfigChange?: () => void }) {
  const { toast } = useToast()
  const [activeTab, setActiveTab] = useState<'agents' | 'ai' | 'db'>('agents')
  const [policy, setPolicy] = useState<any>(null)
  const [config, setConfig] = useState<any>(null)
  const [aiModels, setAiModels] = useState<any>(null)
  const [isLoading, setIsLoading] = useState(false)
  const [isConfigLoading, setIsConfigLoading] = useState(false)
  const [isAiModelsLoading, setIsAiModelsLoading] = useState(false)

  const [isProfileEditorOpen, setIsProfileEditorOpen] = useState(false)
  const [profileDraft, setProfileDraft] = useState<any>(null)
  const [isSavingProfile, setIsSavingProfile] = useState(false)

  const [isHealthLoading, setIsHealthLoading] = useState(false)
  const [healthReport, setHealthReport] = useState<any>(null)
  const [healthError, setHealthError] = useState<any>(null)

  const [fetchedModels, setFetchedModels] = useState<{id: string, added: boolean}[]>([])
  const [isFetchingModels, setIsFetchingModels] = useState(false)
  const [customTiersJson, setCustomTiersJson] = useState('')
  const [tiersTemplate, setTiersTemplate] = useState('')

  const [newConnName, setNewConnName] = useState('')
  const [newConnUrl, setNewConnUrl] = useState('')

  async function loadPolicy() {
    try {
      const data = await api.getPolicy()
      setPolicy(data)
    } catch (e) {
      console.error('Failed to load policy:', sanitizeForLog(e))
    }
  }

  async function loadConfig() {
    setIsConfigLoading(true)
    try {
      const data = await api.getConfig()
      setConfig(data)
    } catch (e) {
      toast('加载配置失败：' + parseError(e).message, 'error')
    } finally {
      setIsConfigLoading(false)
    }
  }

  async function loadAiModels() {
    setIsAiModelsLoading(true)
    try {
      const data = await api.getAiModels()
      setAiModels(data)
    } catch (e) {
      toast('加载模型列表失败：' + parseError(e).message, 'error')
    } finally {
      setIsAiModelsLoading(false)
    }
  }

  useEffect(() => {
    loadPolicy()
    loadConfig()
    loadAiModels()
  }, [])

  const activeProfile = useMemo(() => {
    const profiles = config?.ai_profiles
    const activeId = config?.active_ai_profile_id
    if (!Array.isArray(profiles) || !activeId) return null
    return profiles.find((p: any) => p?.id === activeId) || null
  }, [config])

  const resolvedModelId = useMemo(() => {
    return aiModels?.active_model_id || config?.active_model_id || config?.model_name || ''
  }, [aiModels, config])

  const resolvedTier = useMemo(() => {
    return aiModels?.active_tier || config?.active_tier || 'balanced'
  }, [aiModels, config])

  const resolvedModelsList = useMemo(() => {
    const list = aiModels?.models || config?.ai_models
    return Array.isArray(list) ? list : []
  }, [aiModels, config])

  const selectedModel = useMemo(() => {
    const list = resolvedModelsList
    const id = resolvedModelId
    return list.find((m: any) => m?.id === id) || null
  }, [resolvedModelsList, resolvedModelId])

  const availableTiers = useMemo(() => {
    if (selectedModel?.custom_tiers && Array.isArray(selectedModel.custom_tiers) && selectedModel.custom_tiers.length > 0) {
      return selectedModel.custom_tiers;
    }
    return [
      { id: 'fast', display_name: 'fast' },
      { id: 'balanced', display_name: 'balanced' },
      { id: 'high', display_name: 'high' },
      { id: 'ultra', display_name: 'ultra' },
    ];
  }, [selectedModel])

  const supportsTier = selectedModel?.supports_tier !== false

  const patchAndSaveConfig = async (patch: Record<string, unknown>, successMessage?: string) => {
    if (!config) return
    setIsLoading(true)
    try {
      const next = { ...config, ...patch }
      const saved = await api.updateConfig(next)
      setConfig(saved)
      await loadAiModels()
      onConfigChange?.()
      if (successMessage) toast(successMessage, 'success')
    } catch (e) {
      toast('保存配置失败：' + parseError(e).message, 'error')
    } finally {
      setIsLoading(false)
    }
  }

  const handleReset = async () => {
    if (!confirm('Are you sure you want to reset the agent policy to default? All local learning will be lost.')) return
    setIsLoading(true)
    try {
      await api.resetPolicy()
      await loadPolicy()
      onPolicyChange()
      toast('Policy reset to default.', 'success')
    } catch (e) {
      toast('Failed to reset: ' + parseError(e).message, 'error')
    } finally {
      setIsLoading(false)
    }
  }

  const handleSnapshot = async () => {
    setIsLoading(true)
    try {
      const res = await api.snapshotPolicy()
      toast(`Snapshot created: ${res.name}`, 'success')
    } catch (e) {
      toast('Failed to create snapshot: ' + parseError(e).message, 'error')
    } finally {
      setIsLoading(false)
    }
  }

  const handleRollback = async () => {
    const name = prompt('Enter snapshot name to rollback (e.g. policy_20240101_120000.json):')
    if (!name) return
    setIsLoading(true)
    try {
      await api.rollbackPolicy(name)
      await loadPolicy()
      onPolicyChange()
      toast('Policy rolled back successfully.', 'success')
    } catch (e) {
      toast('Failed to rollback: ' + parseError(e).message, 'error')
    } finally {
      setIsLoading(false)
    }
  }

  const openNewProfile = () => {
    setProfileDraft({
      id: `profile_${Date.now()}`,
      name: 'New Profile',
      provider: 'openai',
      mode: 'direct',
      api_key: '',
      relay_url: '',
      pool: { tokens: [] }
    })
    setIsProfileEditorOpen(true)
    setFetchedModels([])
    setCustomTiersJson('')
    setTiersTemplate('')
  }

  const openEditProfile = (p: any) => {
    setProfileDraft({
      ...p,
      api_key: p?.api_key || '',
      relay_url: p?.relay_url || '',
      pool: { ...(p?.pool || {}), tokens: Array.isArray(p?.pool?.tokens) ? p.pool.tokens : [] }
    })
    setIsProfileEditorOpen(true)
    setFetchedModels([])
    setCustomTiersJson('')
    setTiersTemplate('')
  }

  const templates = useMemo(() => {
    return {
      openai_reasoning: `[\n  { "id": "low", "display_name": "low", "reasoning_effort": "low" },\n  { "id": "medium", "display_name": "medium", "reasoning_effort": "medium" },\n  { "id": "high", "display_name": "high", "reasoning_effort": "high" },\n  { "id": "xhigh", "display_name": "xhigh", "reasoning_effort": "xhigh" }\n]`,
      deepseek_thinking: `[\n  { "id": "standard", "display_name": "standard" },\n  { "id": "thinking", "display_name": "thinking", "thinking_type": "enabled" }\n]`,
      moonshot_thinking: `[\n  { "id": "enabled", "display_name": "thinking enabled", "thinking_type": "enabled" },\n  { "id": "disabled", "display_name": "thinking disabled", "thinking_type": "disabled" }\n]`,
      zhipu_thinking: `[\n  { "id": "enabled", "display_name": "thinking enabled", "thinking_type": "enabled" },\n  { "id": "disabled", "display_name": "thinking disabled", "thinking_type": "disabled" }\n]`,
      anthropic_adaptive: `[\n  { "id": "low", "display_name": "adaptive low", "thinking_type": "adaptive", "reasoning_effort": "low", "thinking_display": "summarized" },\n  { "id": "medium", "display_name": "adaptive medium", "thinking_type": "adaptive", "reasoning_effort": "medium", "thinking_display": "summarized" },\n  { "id": "high", "display_name": "adaptive high", "thinking_type": "adaptive", "reasoning_effort": "high", "thinking_display": "summarized" }\n]`,
    }
  }, [])

  const defaultTemplateKey = useMemo(() => {
    const p = String(profileDraft?.provider || 'openai')
    if (p === 'deepseek') return 'deepseek_thinking'
    if (p === 'moonshot') return 'moonshot_thinking'
    if (p === 'zhipu') return 'zhipu_thinking'
    if (p === 'anthropic') return 'anthropic_adaptive'
    return 'openai_reasoning'
  }, [profileDraft?.provider])

  const saveProfile = async () => {
    if (!config || !profileDraft) return
    const id = String(profileDraft.id || '').trim()
    const name = String(profileDraft.name || '').trim()
    if (!id || !name) {
      toast('Profile 的 ID 与名称不能为空。', 'error')
      return
    }

    const list = Array.isArray(config.ai_profiles) ? [...config.ai_profiles] : []
    const existsIdx = list.findIndex((p: any) => p?.id === id)

    const nextProfile = {
      id,
      name,
      provider: profileDraft.provider || 'openai',
      mode: profileDraft.mode || 'direct',
      api_key: String(profileDraft.api_key || '').trim() || null,
      relay_url: String(profileDraft.relay_url || '').trim() || null,
      pool: {
        ...(profileDraft.pool || {}),
        tokens: Array.isArray(profileDraft.pool?.tokens) ? profileDraft.pool.tokens : []
      }
    }

    if (existsIdx >= 0) {
      list[existsIdx] = nextProfile
    } else {
      list.push(nextProfile)
    }

    setIsSavingProfile(true)
    try {
      const patch: Record<string, unknown> = { ai_profiles: list }
      const saved = await api.updateConfig({ ...config, ...patch })
      setConfig(saved)
      onConfigChange?.()
      toast('Profile 已保存。', 'success')
      setIsProfileEditorOpen(false)
      setProfileDraft(null)
    } catch (e) {
      toast('保存 Profile 失败：' + parseError(e).message, 'error')
    } finally {
      setIsSavingProfile(false)
    }
  }

  const deleteProfile = async (profileId: string) => {
    if (!config) return
    const list = Array.isArray(config.ai_profiles) ? [...config.ai_profiles] : []
    if (list.length <= 1) {
      toast('至少需要保留 1 个 Profile。', 'error')
      return
    }
    const nextProfiles = list.filter((p: any) => p?.id !== profileId)
    const nextActiveId =
      config.active_ai_profile_id === profileId ? (nextProfiles[0]?.id || null) : config.active_ai_profile_id

    await patchAndSaveConfig(
      { ai_profiles: nextProfiles, active_ai_profile_id: nextActiveId },
      'Profile 已删除。'
    )
  }

  const setActiveProfileId = async (profileId: string) => {
    if (!config) return
    const list = Array.isArray(config.ai_profiles) ? config.ai_profiles : []
    const p = list.find((x: any) => x?.id === profileId)
    if (!p) return

    const patch = {
      active_ai_profile_id: profileId,
      ai_provider: p.provider,
      ai_mode: p.mode,
      api_key: p.api_key || null,
      relay_url: p.relay_url || null,
      token_pool: Array.isArray(p?.pool?.tokens) ? p.pool.tokens : []
    }

    await patchAndSaveConfig(patch, `已切换到 Profile：${p.name}`)
  }

  const saveModelAndTier = async (modelId: string, tier: string) => {
    const patch: Record<string, unknown> = {
      active_model_id: modelId,
      model_name: modelId,
      active_tier: tier
    }
    await patchAndSaveConfig(patch, 'AI 运行态已更新。')
  }

  const runHealthCheck = async () => {
    setIsHealthLoading(true)
    setHealthReport(null)
    setHealthError(null)
    try {
      const res = await api.getAiHealth()
      setHealthReport(res)
      toast('Health 检测通过。', 'success')
    } catch (e) {
      const err = parseError(e)
      const status = (e as any)?.response?.status as number | undefined
      const code = (e as any)?.response?.data?.code as string | undefined
      const details = (e as any)?.response?.data?.details as string | undefined
      setHealthError({ status, code, details, ...err })
      toast('Health 检测失败：' + err.title, 'error')
    } finally {
      setIsHealthLoading(false)
    }
  }

  const healthSuggestions = useMemo(() => {
    if (healthReport) {
      const tips: string[] = []
      if (!healthReport.active_ai_profile_id) tips.push('未设置 active_ai_profile_id，建议先选择一个 Profile。')
      if (healthReport.mode === 'pool' && (!activeProfile?.pool?.tokens || activeProfile.pool.tokens.length === 0)) {
        tips.push('Pool 模式需要至少 1 个 Token；建议在 Profile 的 Pool Tokens 中添加多条 Key。')
      }
      if (healthReport.mode !== 'direct' && !healthReport.endpoint) tips.push('Relay/LocalRelay/Pool 模式需要 relay_url 或可用默认 endpoint。')
      if (healthReport.latency_ms && healthReport.latency_ms > 5000) tips.push('延迟偏高，建议切换到更快的 Tier 或更近的中转节点。')
      return tips
    }
    if (healthError) {
      const tips: string[] = []
      if (healthError.status === 401 || healthError.code === 'ERR_AI_AUTH') tips.push('检查 Profile 的 API Key/Token 是否有效，或重新生成。')
      if (healthError.status === 403 || healthError.code === 'ERR_AI_FORBIDDEN') tips.push('403 通常是权限/额度/风控问题；建议更换 Key 或切换到 Pool。')
      if (healthError.status === 404 || healthError.code === 'ERR_AI_MODEL_NOT_FOUND') tips.push('模型不存在；建议切换模型或检查中转站是否支持该模型。')
      if (healthError.status === 0) tips.push('网络不可达；请确认后端服务已启动、代理配置正确。')
      return tips
    }
    return []
  }, [healthReport, healthError, activeProfile])

  return (
    <div className="fixed inset-0 bg-black/60 backdrop-blur-sm z-50 flex items-center justify-center">
      <motion.div
        initial={{ scale: 0.95, opacity: 0 }}
        animate={{ scale: 1, opacity: 1 }}
        exit={{ scale: 0.95, opacity: 0 }}
        className="bg-[#161b22] border border-[#30363d] rounded-xl shadow-2xl w-[600px] overflow-hidden flex flex-col"
      >
        <div className="px-6 py-4 border-b border-[#30363d] flex items-center justify-between bg-[#0d1117]">
          <h3 className="text-gray-200 font-bold text-lg">Settings</h3>
          <button onClick={onClose} className="text-gray-500 hover:text-gray-300">
            <X className="w-5 h-5" />
          </button>
        </div>

        <div className="flex border-b border-[#30363d]">
          <button 
            className={`px-6 py-3 text-sm font-medium transition-colors ${activeTab === 'agents' ? 'text-blue-400 border-b-2 border-blue-400' : 'text-gray-400 hover:text-gray-200'}`}
            onClick={() => setActiveTab('agents')}
          >
            Agents & Policy
          </button>
          <button 
            className={`px-6 py-3 text-sm font-medium transition-colors ${activeTab === 'ai' ? 'text-blue-400 border-b-2 border-blue-400' : 'text-gray-400 hover:text-gray-200'}`}
            onClick={() => setActiveTab('ai')}
          >
            AI Profiles
          </button>
          <button 
            className={`px-6 py-3 text-sm font-medium transition-colors ${activeTab === 'db' ? 'text-blue-400 border-b-2 border-blue-400' : 'text-gray-400 hover:text-gray-200'}`}
            onClick={() => setActiveTab('db')}
          >
            Database & API
          </button>
        </div>

        <div className="p-6 overflow-y-auto max-h-[60vh]">
          {activeTab === 'agents' && (
            <div className="space-y-6">
              <div className="bg-[#0d1117] border border-[#30363d] rounded-lg p-4">
                <h4 className="text-gray-200 font-medium mb-2 flex items-center gap-2">
                  <Server className="w-4 h-4 text-blue-400" />
                  Local Policy Evolution
                </h4>
                <p className="text-xs text-gray-500 mb-4 leading-relaxed">
                  The AI Agent adapts its threshold parameters automatically as you save more rules and execute queries successfully. 
                  Below are the currently active effective thresholds.
                </p>
                
                {policy ? (
                  <div className="grid grid-cols-2 gap-4">
                    <div className="bg-[#161b22] border border-[#30363d] p-3 rounded">
                      <div className="text-[10px] text-gray-500 uppercase font-bold tracking-wider mb-1">Direct Match Threshold</div>
                      <div className="text-lg font-mono text-green-400">{policy.rule_direct_threshold.toFixed(2)}</div>
                    </div>
                    <div className="bg-[#161b22] border border-[#30363d] p-3 rounded">
                      <div className="text-[10px] text-gray-500 uppercase font-bold tracking-wider mb-1">Suggest Match Threshold</div>
                      <div className="text-lg font-mono text-blue-400">{policy.rule_suggest_threshold.toFixed(2)}</div>
                    </div>
                  </div>
                ) : (
                  <div className="text-sm text-gray-500 animate-pulse">Loading policy...</div>
                )}
              </div>

              <div className="flex gap-3">
                <button 
                  onClick={handleSnapshot}
                  disabled={isLoading}
                  className="flex-1 flex items-center justify-center gap-2 bg-[#21262d] hover:bg-[#30363d] border border-[#30363d] text-gray-300 py-2 rounded-lg text-sm transition-colors"
                >
                  <Save className="w-4 h-4" /> Snapshot
                </button>
                <button 
                  onClick={handleRollback}
                  disabled={isLoading}
                  className="flex-1 flex items-center justify-center gap-2 bg-[#21262d] hover:bg-[#30363d] border border-[#30363d] text-gray-300 py-2 rounded-lg text-sm transition-colors"
                >
                  <Archive className="w-4 h-4" /> Rollback
                </button>
                <button 
                  onClick={handleReset}
                  disabled={isLoading}
                  className="flex-1 flex items-center justify-center gap-2 bg-red-500/10 hover:bg-red-500/20 border border-red-500/20 text-red-400 py-2 rounded-lg text-sm transition-colors"
                >
                  <RefreshCw className="w-4 h-4" /> Reset Default
                </button>
              </div>
            </div>
          )}

          {activeTab === 'ai' && (
            <div className="space-y-6">
              <div className="bg-[#0d1117] border border-[#30363d] rounded-lg p-4">
                <h4 className="text-gray-200 font-medium mb-2 flex items-center gap-2">
                  <Cpu className="w-4 h-4 text-blue-400" />
                  运行态切换 (Profile / Model / Tier)
                </h4>
                <p className="text-xs text-gray-500 mb-4 leading-relaxed">
                  Profile 负责连接方式与凭证；Model/Tier 影响推理能力与延迟。切换后无需重启服务。
                </p>

                {(isConfigLoading || isAiModelsLoading) && (
                  <div className="text-sm text-gray-500 animate-pulse">Loading AI settings...</div>
                )}

                {!(isConfigLoading || isAiModelsLoading) && (
                  <>
                    <div className="grid grid-cols-2 gap-4">
                      <div className="bg-[#161b22] border border-[#30363d] p-3 rounded">
                        <div className="text-[10px] text-gray-500 uppercase font-bold tracking-wider mb-2">Active Profile</div>
                        <select
                          value={config?.active_ai_profile_id || ''}
                          onChange={(e) => setActiveProfileId(e.target.value)}
                          disabled={isLoading || !config}
                          className="w-full bg-[#0d1117] border border-[#30363d] rounded px-2 py-1.5 text-sm text-gray-200"
                        >
                          {(Array.isArray(config?.ai_profiles) ? config.ai_profiles : []).map((p: any) => (
                            <option key={p.id} value={p.id}>{p.name} ({p.mode}/{p.provider})</option>
                          ))}
                        </select>
                        <div className="mt-2 text-xs text-gray-500">
                          当前：{activeProfile ? `${activeProfile.name}` : '未选择'}
                        </div>
                      </div>

                      <div className="bg-[#161b22] border border-[#30363d] p-3 rounded">
                        <div className="text-[10px] text-gray-500 uppercase font-bold tracking-wider mb-2">Model</div>
                        <select
                          value={resolvedModelId}
                          onChange={(e) => {
                            const newModelId = e.target.value;
                            const newModel = resolvedModelsList.find((m: any) => m.id === newModelId);
                            let newTier = resolvedTier;
                            if (newModel?.custom_tiers && Array.isArray(newModel.custom_tiers) && newModel.custom_tiers.length > 0) {
                              if (!newModel.custom_tiers.find((t: any) => t.id === resolvedTier)) {
                                newTier = newModel.custom_tiers[0].id;
                              }
                            } else {
                              if (!['fast', 'balanced', 'high', 'ultra'].includes(resolvedTier)) {
                                newTier = 'balanced';
                              }
                            }
                            saveModelAndTier(newModelId, newTier);
                          }}
                          disabled={isLoading || resolvedModelsList.length === 0}
                          className="w-full bg-[#0d1117] border border-[#30363d] rounded px-2 py-1.5 text-sm text-gray-200"
                        >
                          {resolvedModelsList.map((m: any) => (
                            <option key={m.id} value={m.id}>{m.display_name || m.id}</option>
                          ))}
                        </select>
                        <div className="mt-2 text-xs text-gray-500">
                          {selectedModel ? `max_context=${selectedModel.max_context}` : ''}
                        </div>
                      </div>
                    </div>

                    <div className="grid grid-cols-2 gap-4 mt-4">
                      <div className="bg-[#161b22] border border-[#30363d] p-3 rounded">
                        <div className="text-[10px] text-gray-500 uppercase font-bold tracking-wider mb-2">Tier</div>
                        <select
                          value={resolvedTier}
                          onChange={(e) => saveModelAndTier(resolvedModelId, e.target.value)}
                          disabled={isLoading || !supportsTier}
                          className="w-full bg-[#0d1117] border border-[#30363d] rounded px-2 py-1.5 text-sm text-gray-200 disabled:opacity-60"
                        >
                          {availableTiers.map((t: any) => (
                            <option key={t.id} value={t.id}>{t.display_name}</option>
                          ))}
                        </select>
                        {!supportsTier && (
                          <div className="mt-2 text-xs text-gray-500">该模型不支持 tier，已自动降级为 balanced。</div>
                        )}
                      </div>

                      <div className="bg-[#161b22] border border-[#30363d] p-3 rounded">
                        <div className="text-[10px] text-gray-500 uppercase font-bold tracking-wider mb-2">Health Check</div>
                        <button
                          onClick={runHealthCheck}
                          disabled={isHealthLoading}
                          className="w-full flex items-center justify-center gap-2 bg-[#21262d] hover:bg-[#30363d] border border-[#30363d] text-gray-200 py-2 rounded-lg text-sm transition-colors disabled:opacity-60"
                        >
                          <HeartPulse className={`w-4 h-4 ${isHealthLoading ? 'animate-spin' : ''}`} />
                          {isHealthLoading ? 'Checking...' : 'Run Health Check'}
                        </button>
                        <div className="mt-2 text-xs text-gray-500">
                          结果会包含当前 profile/model/tier 的可用性探测与建议。
                        </div>
                      </div>
                    </div>
                  </>
                )}
              </div>

              <div className="bg-[#0d1117] border border-[#30363d] rounded-lg p-4">
                <div className="flex items-center justify-between mb-3">
                  <h4 className="text-gray-200 font-medium flex items-center gap-2">
                    <Server className="w-4 h-4 text-blue-400" />
                    Profiles
                  </h4>
                  <button
                    onClick={openNewProfile}
                    disabled={!config}
                    className="flex items-center gap-2 bg-[#21262d] hover:bg-[#30363d] border border-[#30363d] text-gray-200 px-3 py-1.5 rounded-lg text-sm transition-colors disabled:opacity-60"
                  >
                    <Plus className="w-4 h-4" /> 新增
                  </button>
                </div>

                {!config && (
                  <div className="text-sm text-gray-500 animate-pulse">Loading config...</div>
                )}

                {config && (
                  <div className="space-y-2">
                    {(Array.isArray(config.ai_profiles) ? config.ai_profiles : []).map((p: any) => {
                      const isActive = p?.id === config.active_ai_profile_id
                      return (
                        <div key={p.id} className={`flex items-center justify-between gap-3 bg-[#161b22] border rounded-lg px-3 py-2 ${isActive ? 'border-blue-500/40' : 'border-[#30363d]'}`}>
                          <div className="flex items-center gap-2 min-w-0">
                            {isActive ? <CheckCircle2 className="w-4 h-4 text-blue-400" /> : <div className="w-4 h-4" />}
                            <div className="min-w-0">
                              <div className="text-sm text-gray-200 truncate">{p.name}</div>
                              <div className="text-[11px] text-gray-500 truncate">{p.id} · {p.mode}/{p.provider}</div>
                            </div>
                          </div>
                          <div className="flex items-center gap-2">
                            <button
                              onClick={() => setActiveProfileId(p.id)}
                              disabled={isLoading || isActive}
                              className="text-xs px-2 py-1 rounded border border-[#30363d] bg-[#0d1117] text-gray-300 hover:text-white hover:bg-[#21262d] disabled:opacity-50"
                            >
                              {isActive ? 'Active' : 'Use'}
                            </button>
                            <button
                              onClick={() => openEditProfile(p)}
                              disabled={isLoading}
                              className="p-1.5 rounded border border-[#30363d] bg-[#0d1117] text-gray-400 hover:text-blue-400 hover:bg-blue-500/10 transition-colors"
                              title="Edit"
                            >
                              <Pencil className="w-3.5 h-3.5" />
                            </button>
                            <button
                              onClick={() => deleteProfile(p.id)}
                              disabled={isLoading}
                              className="p-1.5 rounded border border-[#30363d] bg-[#0d1117] text-gray-400 hover:text-red-400 hover:bg-red-500/10 transition-colors"
                              title="Delete"
                            >
                              <Trash2 className="w-3.5 h-3.5" />
                            </button>
                          </div>
                        </div>
                      )
                    })}
                  </div>
                )}
              </div>

              {(healthReport || healthError) && (
                <div className={`border rounded-lg p-4 ${healthReport ? 'bg-green-950/10 border-green-500/20' : 'bg-red-950/10 border-red-500/20'}`}>
                  <div className={`flex items-center gap-2 font-medium ${healthReport ? 'text-green-400' : 'text-red-400'}`}>
                    {healthReport ? <CheckCircle2 className="w-4 h-4" /> : <AlertTriangle className="w-4 h-4" />}
                    {healthReport ? 'Health Report' : 'Health Error'}
                  </div>
                  {healthReport && (
                    <div className="mt-3 grid grid-cols-2 gap-3 text-sm">
                      <div className="bg-[#0d1117] border border-[#30363d] rounded p-3">
                        <div className="text-[10px] text-gray-500 uppercase font-bold tracking-wider mb-1">Endpoint</div>
                        <div className="text-gray-200 break-all">{healthReport.endpoint}</div>
                      </div>
                      <div className="bg-[#0d1117] border border-[#30363d] rounded p-3">
                        <div className="text-[10px] text-gray-500 uppercase font-bold tracking-wider mb-1">Profile / Model / Tier</div>
                        <div className="text-gray-200 break-all">{healthReport.active_ai_profile_id || '-'} · {healthReport.model_id} · {healthReport.tier}</div>
                      </div>
                      <div className="bg-[#0d1117] border border-[#30363d] rounded p-3">
                        <div className="text-[10px] text-gray-500 uppercase font-bold tracking-wider mb-1">Latency</div>
                        <div className="text-gray-200">{healthReport.latency_ms ?? '-'} ms</div>
                      </div>
                      <div className="bg-[#0d1117] border border-[#30363d] rounded p-3">
                        <div className="text-[10px] text-gray-500 uppercase font-bold tracking-wider mb-1">Preview</div>
                        <div className="text-gray-200 break-words">{healthReport.result_preview || '-'}</div>
                      </div>
                    </div>
                  )}
                  {healthError && (
                    <div className="mt-3 text-sm text-gray-200">
                      <div className="bg-[#0d1117] border border-[#30363d] rounded p-3">
                        <div className="text-[10px] text-gray-500 uppercase font-bold tracking-wider mb-1">Error</div>
                        <div className="text-gray-200 break-words">{healthError.title}：{healthError.message}</div>
                        {(healthError.code || healthError.status) && (
                          <div className="text-xs text-gray-500 mt-2">code={healthError.code || '-'} status={healthError.status || '-'}</div>
                        )}
                        {healthError.details && (
                          <div className="text-xs text-gray-500 mt-1 break-words">{healthError.details}</div>
                        )}
                      </div>
                    </div>
                  )}

                  {healthSuggestions.length > 0 && (
                    <div className="mt-3 bg-[#0d1117] border border-[#30363d] rounded p-3">
                      <div className="text-[10px] text-gray-500 uppercase font-bold tracking-wider mb-2">建议</div>
                      <ul className="list-disc list-inside text-sm text-gray-200 space-y-1">
                        {healthSuggestions.map((s, idx) => <li key={idx}>{s}</li>)}
                      </ul>
                    </div>
                  )}
                </div>
              )}
            </div>
          )}

          {activeTab === 'db' && (
            <div className="space-y-6">
              <div className="bg-[#0d1117] border border-[#30363d] rounded-lg p-4">
                <div className="flex items-center justify-between mb-4">
                  <h4 className="text-gray-200 font-medium flex items-center gap-2">
                    <Database className="w-4 h-4 text-blue-400" />
                    Database Connections
                  </h4>
                </div>

                <div className="mb-6 p-4 bg-[#161b22] border border-[#30363d] rounded-lg space-y-3">
                  <div className="text-sm text-gray-300 font-medium mb-1">Add New Connection</div>
                  <div className="grid grid-cols-2 gap-3">
                    <div>
                      <div className="text-xs text-gray-500 mb-1">Name (名称)</div>
                      <input 
                        value={newConnName}
                        onChange={(e) => setNewConnName(e.target.value)}
                        placeholder="e.g. My Prod DB"
                        className="w-full bg-[#0d1117] border border-[#30363d] rounded px-3 py-1.5 text-sm text-gray-200"
                      />
                    </div>
                    <div>
                      <div className="text-xs text-gray-500 mb-1">Connection URL</div>
                      <input 
                        value={newConnUrl}
                        onChange={(e) => setNewConnUrl(e.target.value)}
                        placeholder="mysql://user:pass@host:3306/db"
                        className="w-full bg-[#0d1117] border border-[#30363d] rounded px-3 py-1.5 text-sm text-gray-200"
                      />
                    </div>
                  </div>
                  <div className="flex justify-end">
                    <button
                      onClick={async () => {
                        if (!newConnUrl.trim()) {
                          toast('Connection URL cannot be empty', 'error');
                          return;
                        }
                        const newConn = {
                          id: 'db-' + Date.now(),
                          name: newConnName.trim() || 'Unnamed',
                          url: newConnUrl.trim(),
                          is_read_only: false
                        };
                        const list = Array.isArray(config?.db_connections) ? [...config.db_connections] : [];
                        list.push(newConn);
                        await patchAndSaveConfig({ db_connections: list }, 'New connection added.');
                        setNewConnName('');
                        setNewConnUrl('');
                      }}
                      disabled={isLoading || !config}
                      className="flex items-center gap-2 bg-[#21262d] hover:bg-[#30363d] border border-[#30363d] text-gray-200 px-3 py-1.5 rounded-lg text-sm transition-colors disabled:opacity-60"
                    >
                      <Plus className="w-4 h-4" /> Add
                    </button>
                  </div>
                </div>

                {!config ? (
                  <div className="text-sm text-gray-500 animate-pulse">Loading connections...</div>
                ) : (
                  <div className="space-y-3">
                    {(Array.isArray(config.db_connections) ? config.db_connections : []).map((conn: any, idx: number) => {
                      const isActive = conn.id === config.active_db_id;
                      const typeText = dbTypeDisplayName(conn.db_type)
                      const levelText = dbLevelDisplayName(conn.capability_level)
                      return (
                        <div key={conn.id || idx} className={`flex flex-col gap-2 bg-[#161b22] border rounded-lg p-3 ${isActive ? 'border-blue-500/40' : 'border-[#30363d]'}`}>
                          <div className="flex items-center justify-between">
                            <div className="flex items-center gap-2">
                              {isActive ? <CheckCircle2 className="w-4 h-4 text-blue-400" /> : <div className="w-4 h-4" />}
                              <div>
                                <div className="text-sm text-gray-200 font-medium">{conn.name || conn.id}</div>
                                <div className="text-xs text-gray-500 font-mono mt-0.5 break-all">{conn.url.replace(/:[^:@]+@/, ':***@')}</div>
                                <div className="text-xs text-gray-500 mt-1">{typeText} / {levelText}</div>
                              </div>
                            </div>
                            <button
                              onClick={async () => {
                                if (!window.confirm(`Are you sure you want to delete the database connection "${conn.name || conn.id}"?`)) return;
                                const list = config.db_connections.filter((c: any) => c.id !== conn.id);
                                let nextActiveId = config.active_db_id;
                                if (isActive) {
                                  nextActiveId = list.length > 0 ? list[0].id : null;
                                }
                                await patchAndSaveConfig({ db_connections: list, active_db_id: nextActiveId }, 'Connection deleted.');
                              }}
                              disabled={isLoading}
                              className="p-1.5 rounded border border-[#30363d] bg-[#0d1117] text-gray-400 hover:text-red-400 hover:bg-red-500/10 transition-colors"
                              title="Delete"
                            >
                              <Trash2 className="w-3.5 h-3.5" />
                            </button>
                          </div>
                          <div className="flex items-center gap-2 mt-2 ml-6">
                            <label className="flex items-center gap-1.5 text-sm text-gray-300 cursor-pointer">
                              <input 
                                type="checkbox" 
                                checked={!!conn.is_read_only}
                                onChange={async (e) => {
                                  const list = [...config.db_connections];
                                  list[idx] = { ...conn, is_read_only: e.target.checked };
                                  await patchAndSaveConfig({ db_connections: list }, 'Connection updated.');
                                }}
                                className="rounded border-[#30363d] bg-[#0d1117] text-blue-500 focus:ring-blue-500/20"
                              />
                              Read-Only Mode
                            </label>
                          </div>
                        </div>
                      )
                    })}
                  </div>
                )}
              </div>
            </div>
          )}
        </div>
      </motion.div>

      {isProfileEditorOpen && profileDraft && (
        <div className="fixed inset-0 z-[60] flex items-center justify-center bg-black/70 backdrop-blur-sm">
          <motion.div
            initial={{ scale: 0.97, opacity: 0 }}
            animate={{ scale: 1, opacity: 1 }}
            exit={{ scale: 0.97, opacity: 0 }}
            className="bg-[#161b22] border border-[#30363d] rounded-xl shadow-2xl w-[680px] overflow-hidden"
          >
            <div className="px-6 py-4 border-b border-[#30363d] flex items-center justify-between bg-[#0d1117]">
              <h3 className="text-gray-200 font-bold text-lg">Edit Profile</h3>
              <button
                onClick={() => { setIsProfileEditorOpen(false); setProfileDraft(null) }}
                className="text-gray-500 hover:text-gray-300"
              >
                <X className="w-5 h-5" />
              </button>
            </div>
            <div className="p-6 space-y-4">
              <div className="grid grid-cols-2 gap-4">
                <div>
                  <div className="text-xs text-gray-500 mb-1">ID</div>
                  <input
                    value={profileDraft.id || ''}
                    onChange={(e) => setProfileDraft({ ...profileDraft, id: e.target.value })}
                    className="w-full bg-[#0d1117] border border-[#30363d] rounded px-3 py-2 text-sm text-gray-200"
                  />
                </div>
                <div>
                  <div className="text-xs text-gray-500 mb-1">Name</div>
                  <input
                    value={profileDraft.name || ''}
                    onChange={(e) => setProfileDraft({ ...profileDraft, name: e.target.value })}
                    className="w-full bg-[#0d1117] border border-[#30363d] rounded px-3 py-2 text-sm text-gray-200"
                  />
                  <div className="mt-4 pt-4 border-t border-[#30363d]">
                    <div className="text-xs text-gray-500 mb-1 flex items-center justify-between">
                      <span>高级：自定义模型推理等级 (Custom Tiers JSON)</span>
                    </div>
                    <div className="flex gap-2 mb-2">
                      <select
                        value={tiersTemplate || defaultTemplateKey}
                        onChange={(e) => setTiersTemplate(e.target.value)}
                        className="flex-1 bg-[#0d1117] border border-[#30363d] rounded px-3 py-1.5 text-sm text-gray-200"
                      >
                        <option value="openai_reasoning">OpenAI reasoning.effort</option>
                        <option value="deepseek_thinking">DeepSeek thinking</option>
                        <option value="moonshot_thinking">Kimi thinking</option>
                        <option value="zhipu_thinking">GLM thinking</option>
                        <option value="anthropic_adaptive">Claude adaptive thinking</option>
                      </select>
                      <button
                        onClick={() => {
                          const key = tiersTemplate || defaultTemplateKey
                          const tpl = (templates as any)[key]
                          if (tpl) setCustomTiersJson(tpl)
                        }}
                        className="px-3 py-1.5 rounded bg-blue-600 text-white hover:bg-blue-500 text-sm"
                      >
                        填充
                      </button>
                    </div>
                    <textarea
                      placeholder={templates[defaultTemplateKey as keyof typeof templates]}
                      value={customTiersJson}
                      onChange={(e) => setCustomTiersJson(e.target.value)}
                      className="w-full h-24 bg-[#0d1117] border border-[#30363d] rounded px-3 py-2 text-sm text-gray-200 font-mono"
                    />
                    <div className="text-[10px] text-gray-500 mt-1">若填写，新添加的模型将绑定这些推理等级。支持字段: id, display_name, temperature, max_tokens, reasoning_effort, thinking_type, thinking_budget_tokens, thinking_display</div>
                  </div>
                </div>
              </div>

              <div className="grid grid-cols-2 gap-4">
                <div>
                  <div className="text-xs text-gray-500 mb-1">Provider</div>
                  <select
                    value={profileDraft.provider || 'openai'}
                    onChange={(e) => setProfileDraft({ ...profileDraft, provider: e.target.value })}
                    className="w-full bg-[#0d1117] border border-[#30363d] rounded px-3 py-2 text-sm text-gray-200"
                  >
                    <option value="openai">openai</option>
                    <option value="deepseek">deepseek</option>
                    <option value="moonshot">moonshot (Kimi)</option>
                    <option value="zhipu">zhipu (GLM)</option>
                    <option value="anthropic">anthropic</option>
                    <option value="custom">custom</option>
                  </select>
                </div>
                <div>
                  <div className="text-xs text-gray-500 mb-1">Mode</div>
                  <select
                    value={profileDraft.mode || 'direct'}
                    onChange={(e) => setProfileDraft({ ...profileDraft, mode: e.target.value })}
                    className="w-full bg-[#0d1117] border border-[#30363d] rounded px-3 py-2 text-sm text-gray-200"
                  >
                    <option value="direct">direct</option>
                    <option value="relay">relay</option>
                    <option value="local_relay">local_relay</option>
                    <option value="pool">pool</option>
                  </select>
                </div>
              </div>

              <div className="grid grid-cols-2 gap-4">
                <div>
                  <div className="text-xs text-gray-500 mb-1">API Key / Token</div>
                  <input
                    type="password"
                    value={profileDraft.api_key || ''}
                    onChange={(e) => setProfileDraft({ ...profileDraft, api_key: e.target.value })}
                    placeholder="留空表示不设置"
                    className="w-full bg-[#0d1117] border border-[#30363d] rounded px-3 py-2 text-sm text-gray-200"
                  />
                </div>
                <div>
                  <div className="text-xs text-gray-500 mb-1">Relay URL</div>
                  <input
                    value={profileDraft.relay_url || ''}
                    onChange={(e) => setProfileDraft({ ...profileDraft, relay_url: e.target.value })}
                    placeholder="例如 https://api.openai.com/v1/chat/completions"
                    className="w-full bg-[#0d1117] border border-[#30363d] rounded px-3 py-2 text-sm text-gray-200"
                  />
                </div>
              </div>

              <div>
                  <div className="text-xs text-gray-500 mb-1">Pool Tokens（每行一个）</div>
                  <textarea
                    value={(profileDraft.pool?.tokens || []).join('\n')}
                    onChange={(e) => setProfileDraft({ ...profileDraft, pool: { ...(profileDraft.pool || {}), tokens: e.target.value.split('\n').map((x: string) => x.trim()).filter(Boolean) } })}
                    className="w-full h-28 bg-[#0d1117] border border-[#30363d] rounded px-3 py-2 text-sm text-gray-200 font-mono"
                  />
                </div>

                <div>
                  <div className="text-xs text-gray-500 mb-1 flex items-center justify-between">
                    <span>动态模型获取 (Fetch Models)</span>
                    <button
                      onClick={async () => {
                        setIsFetchingModels(true)
                        try {
                          const models = await api.fetchProviderModels(profileDraft.provider || 'openai', profileDraft.api_key || '', profileDraft.relay_url || '')
                          const existingIds = (config?.ai_models || []).map((m: any) => m.id)
                          setFetchedModels(models.map((id: string) => ({ id, added: existingIds.includes(id) })))
                          toast(`成功获取 ${models.length} 个模型`, 'success')
                        } catch (e: any) {
                          toast('获取模型失败：' + e.message, 'error')
                        } finally {
                          setIsFetchingModels(false)
                        }
                      }}
                      disabled={isFetchingModels}
                      className="text-blue-400 hover:text-blue-300 disabled:opacity-50"
                    >
                      {isFetchingModels ? '获取中...' : '从提供商拉取'}
                    </button>
                  </div>
                  <div className="bg-[#0d1117] border border-[#30363d] rounded p-2 max-h-32 overflow-y-auto space-y-1">
                    {fetchedModels.length > 0 ? fetchedModels.map(m => (
                      <div key={m.id} className="flex items-center justify-between text-sm py-1 border-b border-[#30363d] last:border-0">
                        <span className="text-gray-300">{m.id}</span>
                        <button
                          disabled={m.added}
                          onClick={async () => {
                            if (!config) return;
                            let parsedTiers = undefined;
                            if (customTiersJson.trim()) {
                              try {
                                parsedTiers = JSON.parse(customTiersJson);
                                if (!Array.isArray(parsedTiers)) throw new Error('必须是 JSON 数组');
                              } catch (err: any) {
                                toast('自定义 Tiers JSON 格式错误: ' + err.message, 'error');
                                return;
                              }
                            }
                            const newModel = { id: m.id, provider: profileDraft.provider, display_name: m.id, supports_tier: true, max_context: 128000, custom_tiers: parsedTiers };
                            const patch = { ai_models: [...(config.ai_models || []), newModel] };
                            const saved = await api.updateConfig({ ...config, ...patch });
                            setConfig(saved); onConfigChange?.();
                            setFetchedModels(prev => prev.map(p => p.id === m.id ? { ...p, added: true } : p));
                            toast(`模型 ${m.id} 已添加到系统`, 'success');
                          }}
                          className={`px-2 py-0.5 rounded text-xs ${m.added ? 'bg-gray-700 text-gray-500' : 'bg-blue-600 text-white hover:bg-blue-500'}`}
                        >{m.added ? '已添加' : '添加'}</button>
                      </div>
                    )) : <div className="text-gray-600 text-xs py-2 text-center">输入 Base URL 和 API Key 后，点击右上角拉取最新模型</div>}
                  </div>
                  <div className="mt-2 flex gap-2">
                    <input 
                      type="text" 
                      placeholder="手动输入模型 ID..." 
                      className="flex-1 bg-[#0d1117] border border-[#30363d] rounded px-3 py-1 text-sm text-gray-200"
                      onKeyDown={async (e) => {
                        if (e.key === 'Enter') {
                          const val = e.currentTarget.value.trim();
                          if (!val || !config) return;
                          let parsedTiers = undefined;
                          if (customTiersJson.trim()) {
                            try {
                              parsedTiers = JSON.parse(customTiersJson);
                              if (!Array.isArray(parsedTiers)) throw new Error('必须是 JSON 数组');
                            } catch (err: any) {
                              toast('自定义 Tiers JSON 格式错误: ' + err.message, 'error');
                              return;
                            }
                          }
                          const newModel = { id: val, provider: profileDraft.provider, display_name: val, supports_tier: true, max_context: 128000, custom_tiers: parsedTiers };
                          const patch = { ai_models: [...(config.ai_models || []), newModel] };
                          const saved = await api.updateConfig({ ...config, ...patch });
                          setConfig(saved); onConfigChange?.();
                          e.currentTarget.value = '';
                          toast(`手动模型 ${val} 已添加到系统`, 'success');
                        }
                      }}
                    />
                    <div className="text-xs text-gray-500 flex items-center">按回车添加</div>
                  </div>
                </div>
              </div>

            <div className="px-6 py-4 border-t border-[#30363d] bg-[#0d1117] flex items-center justify-end gap-3">
              <button
                onClick={() => { setIsProfileEditorOpen(false); setProfileDraft(null) }}
                className="px-4 py-2 rounded-lg text-sm font-medium text-gray-300 hover:bg-[#30363d] transition-colors"
              >
                Cancel
              </button>
              <button
                onClick={saveProfile}
                disabled={isSavingProfile}
                className="flex items-center gap-2 px-4 py-2 rounded-lg text-sm font-medium bg-blue-500/20 hover:bg-blue-500/30 border border-blue-500/30 text-blue-300 transition-colors disabled:opacity-60"
              >
                <Save className="w-4 h-4" />
                {isSavingProfile ? 'Saving...' : 'Save'}
              </button>
            </div>
          </motion.div>
        </div>
      )}
    </div>
  )
}
