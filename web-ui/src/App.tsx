import React, { useState, useEffect, useRef, Suspense, useCallback, useMemo } from 'react'
import { Database, Table, Play, Settings, Command, Sparkles, BookMarked, Save, AlignLeft, Keyboard, X, Trash2, MoreHorizontal } from 'lucide-react'
import { motion, AnimatePresence } from 'framer-motion'
import { format as formatSql } from 'sql-formatter'
import { Onboarding } from './components/Onboarding'
import { SettingsPanel } from './components/SettingsPanel'
import { useToast } from './components/Toast'
import { SkeletonLoader, Skeleton } from './components/Skeleton'
import { SimpleDataTable } from './components/SimpleDataTable'
import { CommandPalette } from './components/CommandPalette'
import { TypingEffect } from './components/TypingEffect'
import { Tabs, type TabItem } from './components/Tabs'
import { TableContextMenu } from './components/TableContextMenu'
import { ToolsNav } from './components/ToolsNav'
import { WizardModal } from './components/WizardModal'
import { GoLiveReportsTab } from './components/GoLiveReportsTab'
import { GoLiveAuditTab } from './components/GoLiveAuditTab'
import { AdvancedToolsHub } from './components/AdvancedToolsHub'
import { SqlHistory } from './components/SqlHistory'
import { AiTrainingPanel } from './components/AiTrainingPanel'
import { DataCharts } from './components/DataCharts'
import { DbExplorerSidebar } from './components/DbExplorerSidebar'
import { api } from './api'

import { parseError, formatErr, sanitizeForLog } from './utils'
import type { AppError } from './utils'
import { dbTypeDisplayName } from './utils/dbCapabilities'
import { useAutoI18nDom } from './i18n'
import { tr } from './i18n'

import * as monaco from 'monaco-editor';
import type { SchemaResponse, ConfigData, QueryExecutionResult, AiRule, DbConnection, TableWithDetails, ColumnInfo, MonacoEditor, Monaco } from './types';

const Editor = React.lazy(() => import('@monaco-editor/react'));
const ExecutionPlan = React.lazy(() => import('./components/ExecutionPlan').then(m => ({ default: m.ExecutionPlan })));
const QueryBuilder = React.lazy(() => import('./components/QueryBuilder').then(m => ({ default: m.QueryBuilder })));
const TableWorkspace = React.lazy(() => import('./components/TableWorkspace').then(m => ({ default: m.TableWorkspace })));

function App() {
  useAutoI18nDom()
  const { toast } = useToast()
  const RESULT_PANEL_HEIGHT_KEY = 'query_results_panel_height'
  const SIDEBAR_WIDTH_KEY = 'sidebar_panel_width'
  const MIN_EDITOR_HEIGHT = 220
  const MIN_RESULTS_HEIGHT = 160
  const SPLITTER_HEIGHT = 8
  const MIN_SIDEBAR_WIDTH = 240
  const MAX_SIDEBAR_WIDTH = 560
  const [showConfirmModal, setShowConfirmModal] = useState(false)
  const [pendingDangerousSql, setPendingDangerousSql] = useState('')

  const [showVariablesModal, setShowVariablesModal] = useState(false)
  const [sqlVariables, setSqlVariables] = useState<{name: string, value: string}[]>([])
  const [pendingSqlWithVars, setPendingSqlWithVars] = useState('')

  const [showCommandPalette, setShowCommandPalette] = useState(false)
  const [showSettings, setShowSettings] = useState(false)
  const [tabs, setTabs] = useState<TabItem[]>([{ id: 'query-1', type: 'query', title: 'Query 1' }])
  const [activeTabId, setActiveTabId] = useState('query-1')
  
  // Per-tab state
  const [tabStates, setTabStates] = useState<Record<string, {
    sql: string;
    query: string;
    lastQuery: string;
    isGenerating: boolean;
    isExecuting: boolean;
    executeResult: QueryExecutionResult | null;
    errorObj: AppError | null;
    lastExplanation: string | null;
    resultsView: 'table' | 'chart';
    chatHistory: any[];
  }>>({
    'query-1': {
      sql: '-- Generated SQL will appear here\n',
      query: '',
      lastQuery: '',
      isGenerating: false,
      isExecuting: false,
      executeResult: null,
      errorObj: null,
      lastExplanation: null,
      resultsView: 'table',
      chatHistory: []
    }
  })

  const activeTabState = tabStates[activeTabId] || {
    sql: '-- Generated SQL will appear here\n',
    query: '',
    lastQuery: '',
    isGenerating: false,
    isExecuting: false,
    executeResult: null,
    errorObj: null,
    lastExplanation: null,
    resultsView: 'table',
    chatHistory: []
  };

  const updateActiveTabState = (patch: Partial<typeof activeTabState>) => {
    setTabStates(prev => ({
      ...prev,
      [activeTabId]: {
        ...(prev[activeTabId] || {
          sql: '-- Generated SQL will appear here\n',
          query: '',
          lastQuery: '',
          isGenerating: false,
          isExecuting: false,
          executeResult: null,
          errorObj: null,
          lastExplanation: null,
          resultsView: 'table',
          chatHistory: []
        }),
        ...patch
      }
    }));
  };
  
  // Real data state
  const [schemaData, setSchemaData] = useState<SchemaResponse | null>(null)
  const [configData, setConfigData] = useState<ConfigData | null>(null)
  const [aiModelsData, setAiModelsData] = useState<any>(null)
  const [isAiSwitching, setIsAiSwitching] = useState(false)
  const [dbType, setDbType] = useState<string>('MySQL')
  
  // App initialization state
  const [isReady, setIsReady] = useState(false)
  const [showOnboarding, setShowOnboarding] = useState(false)
  const [showRulesPanel, setShowRulesPanel] = useState(false)
  const [showHelpModal, setShowHelpModal] = useState(false)
  const [sidebarTab, setSidebarTab] = useState<'schema' | 'smart_snippets' | 'history'>('schema')
  const [rules, setRules] = useState<AiRule[]>([])
  const [isSavingRule, setIsSavingRule] = useState(false)
  const [recentQueries, setRecentQueries] = useState<string[]>([])
  const [isRefreshingSchema, setIsRefreshingSchema] = useState(false)

  const [contextMenu, setContextMenu] = useState<{ x: number, y: number, table: { table_name: string } } | null>(null)
  const [wizardConfig, setWizardConfig] = useState<{ isOpen: boolean, title: string, type: string, payload?: unknown }>({
    isOpen: false,
    title: '',
    type: ''
  })
  const [resultsPanelHeight, setResultsPanelHeight] = useState(() => {
    const raw = window.localStorage.getItem(RESULT_PANEL_HEIGHT_KEY)
    const parsed = raw ? Number(raw) : NaN
    return Number.isFinite(parsed) && parsed >= MIN_RESULTS_HEIGHT ? parsed : 260
  })
  const [isResizingResults, setIsResizingResults] = useState(false)
  const queryPaneRef = useRef<HTMLDivElement | null>(null)
  const [sidebarWidth, setSidebarWidth] = useState(() => {
    const raw = window.localStorage.getItem(SIDEBAR_WIDTH_KEY)
    const parsed = raw ? Number(raw) : NaN
    return Number.isFinite(parsed) ? Math.min(MAX_SIDEBAR_WIDTH, Math.max(MIN_SIDEBAR_WIDTH, parsed)) : 320
  })
  const [isResizingSidebar, setIsResizingSidebar] = useState(false)
  const appRootRef = useRef<HTMLDivElement | null>(null)
  const [isCompactActionBar, setIsCompactActionBar] = useState(false)
  const [showMoreActions, setShowMoreActions] = useState(false)

  const sqlRef = useRef(activeTabState.sql)
  const lastQueryRef = useRef(activeTabState.lastQuery)
  const activeTabIdRef = useRef(activeTabId)
  const tabsRef = useRef(tabs)
  const errorDecorationsRef = useRef<any>(null)

  useEffect(() => {
    sqlRef.current = activeTabState.sql
  }, [activeTabState.sql])

  const getActiveAiProfile = useCallback((cfg: any) => {
    const profiles = cfg?.ai_profiles
    const activeId = cfg?.active_ai_profile_id
    if (Array.isArray(profiles) && activeId) {
      return profiles.find((p: any) => p?.id === activeId) || null
    }
    return null
  }, [])

  const deriveAiConfigured = useCallback((cfg: any) => {
    const p = getActiveAiProfile(cfg)
    if (p) {
      if (p.mode === 'pool') return (Array.isArray(p?.pool?.tokens) && p.pool.tokens.length > 0) || !!p.token_pool_set
      if (p.mode === 'relay' || p.mode === 'local_relay') return !!p.relay_url
      return !!p.api_key || !!p.api_key_set
    }
    return (
      (cfg.ai_mode === 'pool' && ((Array.isArray(cfg.token_pool) && cfg.token_pool.length > 0) || !!cfg.token_pool_set)) ||
      (cfg.ai_mode !== 'pool' && (!!cfg.api_key || !!cfg.api_key_set)) ||
      (cfg.ai_mode === 'relay' && !!cfg.relay_url)
    )
  }, [getActiveAiProfile])

  const resolveActiveModelId = useMemo(() => {
    return aiModelsData?.active_model_id || (configData as any)?.active_model_id || (configData as any)?.model_name || ''
  }, [aiModelsData, configData])

  const resolveActiveTier = useMemo(() => {
    return aiModelsData?.active_tier || (configData as any)?.active_tier || 'balanced'
  }, [aiModelsData, configData])

  const resolveModelsList = useMemo(() => {
    const list = aiModelsData?.models || (configData as any)?.ai_models
    return Array.isArray(list) ? list : []
  }, [aiModelsData, configData])

  const resolveActiveModelLabel = useMemo(() => {
    const id = resolveActiveModelId
    const m = resolveModelsList.find((x: any) => x?.id === id)
    return m?.display_name || id || 'No Model'
  }, [resolveActiveModelId, resolveModelsList])

  const resolveProfilesList = useMemo(() => {
    const list = (configData as any)?.ai_profiles
    return Array.isArray(list) ? list : []
  }, [configData])

  const resolveActiveProfileId = useMemo(() => {
    return (configData as any)?.active_ai_profile_id || ''
  }, [configData])

  useEffect(() => {
    lastQueryRef.current = activeTabState.lastQuery
  }, [activeTabState.lastQuery])

  useEffect(() => {
    activeTabIdRef.current = activeTabId
  }, [activeTabId])

  useEffect(() => {
    tabsRef.current = tabs
  }, [tabs])

  useEffect(() => {
    window.localStorage.setItem(RESULT_PANEL_HEIGHT_KEY, String(resultsPanelHeight))
  }, [resultsPanelHeight])

  useEffect(() => {
    window.localStorage.setItem(SIDEBAR_WIDTH_KEY, String(sidebarWidth))
  }, [sidebarWidth])

  useEffect(() => {
    if (!isResizingResults) return

    const handleMouseMove = (e: MouseEvent) => {
      const pane = queryPaneRef.current
      if (!pane) return
      const rect = pane.getBoundingClientRect()
      const maxResultsHeight = Math.max(MIN_RESULTS_HEIGHT, rect.height - MIN_EDITOR_HEIGHT - SPLITTER_HEIGHT)
      const rawHeight = rect.bottom - e.clientY - SPLITTER_HEIGHT / 2
      const nextHeight = Math.round(Math.min(maxResultsHeight, Math.max(MIN_RESULTS_HEIGHT, rawHeight)))
      setResultsPanelHeight(nextHeight)
    }

    const handleMouseUp = () => {
      setIsResizingResults(false)
    }

    document.body.style.cursor = 'row-resize'
    document.body.style.userSelect = 'none'
    window.addEventListener('mousemove', handleMouseMove)
    window.addEventListener('mouseup', handleMouseUp)

    return () => {
      document.body.style.cursor = ''
      document.body.style.userSelect = ''
      window.removeEventListener('mousemove', handleMouseMove)
      window.removeEventListener('mouseup', handleMouseUp)
    }
  }, [isResizingResults])

  useEffect(() => {
    if (!isResizingSidebar) return

    const handleMouseMove = (e: MouseEvent) => {
      const root = appRootRef.current
      if (!root) return
      const rect = root.getBoundingClientRect()
      const raw = e.clientX - rect.left
      const next = Math.round(Math.min(MAX_SIDEBAR_WIDTH, Math.max(MIN_SIDEBAR_WIDTH, raw)))
      setSidebarWidth(next)
    }

    const handleMouseUp = () => {
      setIsResizingSidebar(false)
    }

    document.body.style.cursor = 'col-resize'
    document.body.style.userSelect = 'none'
    window.addEventListener('mousemove', handleMouseMove)
    window.addEventListener('mouseup', handleMouseUp)

    return () => {
      document.body.style.cursor = ''
      document.body.style.userSelect = ''
      window.removeEventListener('mousemove', handleMouseMove)
      window.removeEventListener('mouseup', handleMouseUp)
    }
  }, [isResizingSidebar])

  useEffect(() => {
    const updateActionBarMode = () => {
      const width = queryPaneRef.current?.clientWidth ?? window.innerWidth
      const compact = width < 1450
      setIsCompactActionBar(compact)
      if (!compact) setShowMoreActions(false)
    }
    updateActionBarMode()
    window.addEventListener('resize', updateActionBarMode)
    return () => window.removeEventListener('resize', updateActionBarMode)
  }, [activeTabId, tabs.length, sidebarWidth])

  useEffect(() => {
    if (!showMoreActions) return
    const close = () => setShowMoreActions(false)
    window.addEventListener('click', close)
    return () => window.removeEventListener('click', close)
  }, [showMoreActions])

  useEffect(() => {
    const saved = localStorage.getItem('recent_queries')
    if (saved) {
      try {
        setRecentQueries(JSON.parse(saved))
      } catch {
        // ignore
      }
    }
  }, [])

  const saveRecentQuery = (q: string) => {
    if (!q.trim()) return
    const newHistory = [q, ...recentQueries.filter(item => item !== q)].slice(0, 10)
    setRecentQueries(newHistory)
    localStorage.setItem('recent_queries', JSON.stringify(newHistory))
  }

  const loadRulesAndPolicy = async () => {
    try {
      const rulesData = await api.getRules()
      setRules(rulesData)
    } catch (e) {
      console.error(sanitizeForLog(e))
    }
  }

  const initData = async () => {
    updateActiveTabState({ errorObj: null })
    try {
      const config = await api.getConfig()
      setConfigData(config)
      try {
        const models = await api.getAiModels()
        setAiModelsData(models)
      } catch {
        setAiModelsData(null)
      }
      
      const activeConn = config.active_db_id ? config.db_connections?.find((c: DbConnection) => c.id === config.active_db_id) : null
      const dbUrl = activeConn ? activeConn.url : config.db_url
      const urlToUse = dbUrl || ''
      if ((activeConn as any)?.db_type) {
        setDbType(dbTypeDisplayName((activeConn as any).db_type))
      } else if (urlToUse.startsWith('postgres://') || urlToUse.startsWith('postgresql://')) {
        setDbType('PostgreSQL')
      } else if (urlToUse.startsWith('sqlite://')) {
        setDbType('SQLite')
      } else {
        setDbType('MySQL')
      }

      const aiConfigured = deriveAiConfigured(config)

      if (urlToUse) {
        const schema = await api.getSchema(config.active_db_id)
        setSchemaData(schema)
        setShowOnboarding(false)
      } else {
        const done = window.localStorage.getItem('onboarding_done') === '1'
        setShowOnboarding(!(done && aiConfigured))
      }
    } catch (e: unknown) {
      console.error("Failed to initialize:", sanitizeForLog(e))
      // If we can't get config or schema fails, show onboarding
      setShowOnboarding(true)
      const err = parseError(e)
      updateActiveTabState({ errorObj: err })
    } finally {
      setIsReady(true)
    }
  }

  const refreshConfigOnly = useCallback(async () => {
    try {
      const config = await api.getConfig()
      setConfigData(config)
      try {
        const models = await api.getAiModels()
        setAiModelsData(models)
      } catch {
        setAiModelsData(null)
      }

      const activeConn = config.active_db_id ? config.db_connections?.find((c: DbConnection) => c.id === config.active_db_id) : null
      const dbUrl = activeConn ? activeConn.url : config.db_url
      const urlToUse = dbUrl || ''
      if ((activeConn as any)?.db_type) {
        setDbType(dbTypeDisplayName((activeConn as any).db_type))
      } else if (urlToUse.startsWith('postgres://') || urlToUse.startsWith('postgresql://')) {
        setDbType('PostgreSQL')
      } else if (urlToUse.startsWith('sqlite://')) {
        setDbType('SQLite')
      } else {
        setDbType('MySQL')
      }
    } catch (e: unknown) {
      updateActiveTabState({ errorObj: parseError(e) })
    }
  }, [])

  const activeModelSupportsTier = useMemo(() => {
    const m = resolveModelsList.find((x: any) => x?.id === resolveActiveModelId)
    return m?.supports_tier !== false
  }, [resolveModelsList, resolveActiveModelId])

  const updateAiRuntime = useCallback(async (patch: Record<string, unknown>, successMsg: string) => {
    if (!configData) return
    setIsAiSwitching(true)
    try {
      await api.updateConfig({ ...(configData as any), ...patch })
      await refreshConfigOnly()
      toast(successMsg, 'success')
    } catch (e: unknown) {
      toast('更新 AI 配置失败：' + parseError(e).message, 'error')
    } finally {
      setIsAiSwitching(false)
    }
  }, [configData, refreshConfigOnly, toast])

  // Fetch initial config and schema
  useEffect(() => {
    initData()
    loadRulesAndPolicy()
  }, [])

  async function handleSaveRule() {
    if (isSavingRule) return
    const currentSql = sqlRef.current
    const currentLastQuery = lastQueryRef.current
    if (!currentLastQuery || !currentSql || !currentSql.trim() || currentSql.trim() === '-- Generated SQL will appear here\n') return
    setIsSavingRule(true)
    try {
      await api.saveRule(currentLastQuery, currentSql)
      await loadRulesAndPolicy()
      toast("Rule saved successfully! The AI will automatically use this template next time.", "success")
    } catch (e: unknown) {
      const err = e as { response?: { data?: string }, message?: string };
      toast("Failed to save rule: " + (err.response?.data || err.message), "error")
    } finally {
      setIsSavingRule(false)
    }
  }

  // Listen for global shortcuts
  useEffect(() => {
    const handleKeyDown = (e: KeyboardEvent) => {
      const isCmdOrCtrl = e.metaKey || e.ctrlKey;
      
      // Cmd+K: Command Palette
      if (isCmdOrCtrl && e.key === 'k') {
        e.preventDefault()
        setShowCommandPalette((prev) => !prev)
      }
      
      // Cmd+S: Save Rule (Intercept browser save)
      if (isCmdOrCtrl && e.key === 's') {
        e.preventDefault()
        const activeType = tabsRef.current.find(t => t.id === activeTabIdRef.current)?.type
        if (activeType === 'query') {
          handleSaveRule()
        } else {
          window.dispatchEvent(new CustomEvent('global-save'))
        }
      }
      
      // Cmd+/ : Help Modal
      if (isCmdOrCtrl && e.key === '/') {
        e.preventDefault()
        setShowHelpModal((prev) => !prev)
      }
      
      // Close on Esc
      if (e.key === 'Escape') {
        setShowCommandPalette(false)
        setShowConfirmModal(false)
        setShowRulesPanel(false)
        setShowHelpModal(false)
      }
    }
    window.addEventListener('keydown', handleKeyDown)
    return () => window.removeEventListener('keydown', handleKeyDown)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []) // Empty dependencies since we use refs in handleSaveRule

  const handleDeleteRule = async (id: string | number) => {
    try {
      await api.deleteRule(id)
      await loadRulesAndPolicy()
      toast("Rule deleted successfully", "success")
    } catch (e: unknown) {
      toast("Failed to delete rule: " + formatErr(e), "error")
    }
  }

  const handleTableDoubleClick = (table: { table_name: string }, forceDbId?: string, forceDbName?: string) => {
    const dbKey = String(forceDbId || configData?.active_db_id || 'default')
    const tabId = `table-${dbKey}-${table.table_name}`
    const existingTab = tabs.find(t => t.id === tabId)
    if (existingTab) {
      setActiveTabId(existingTab.id)
    } else {
      const titlePrefix = forceDbName ? `${forceDbName} · ` : ''
      const newTab: TabItem = {
        id: tabId,
        type: 'table',
        title: `${titlePrefix}${table.table_name}`,
        payload: { tableName: table.table_name, dbId: dbKey, dbName: forceDbName || '' }
      }
      setTabs([...tabs, newTab])
      setActiveTabId(newTab.id)
    }
  }

  const switchActiveDbFromSidebar = useCallback(async (dbId: string) => {
    if (!configData) return
    if (dbId === String(configData.active_db_id || '')) return
    try {
      await api.updateConfig({ ...configData, active_db_id: dbId })
      const selectedDb = (configData as any)?.db_connections?.find((c: DbConnection) => c.id === dbId)
      const dbName = selectedDb ? (selectedDb.name || selectedDb.id) : dbId
      await initData()
      toast(`已切换至 ${dbName} 库`, "success")
    } catch {
      toast("Failed to switch database", "error")
    }
  }, [configData, initData, toast])

  const addConnectionFromSidebar = useCallback(async (name: string, url: string) => {
    if (!configData) return
    if (!url.trim()) {
      toast('Connection URL cannot be empty', 'error')
      return
    }
    try {
      const newConn = {
        id: 'db-' + Date.now(),
        name: name.trim() || 'Unnamed',
        url: url.trim(),
        group_name: null,
        color: '#3b82f6',
        is_favorite: false,
        ssh: { enabled: false },
        ssl: { enabled: false, mode: 'preferred' },
        is_read_only: false
      }
      const list = Array.isArray((configData as any).db_connections) ? [...((configData as any).db_connections as any[])] : []
      list.push(newConn)
      await api.updateConfig({ ...(configData as any), db_connections: list, active_db_id: newConn.id })
      await initData()
      toast('New connection added.', 'success')
    } catch (e: unknown) {
      toast('Failed to add connection: ' + formatErr(e), 'error')
    }
  }, [configData, initData, toast])

  const updateConnectionFromSidebar = useCallback(async (connId: string, patch: Record<string, unknown>) => {
    if (!configData) return
    try {
      const list = Array.isArray((configData as any).db_connections) ? [...((configData as any).db_connections as any[])] : []
      const idx = list.findIndex((c: any) => c.id === connId)
      if (idx < 0) return
      list[idx] = { ...list[idx], ...patch }
      await api.updateConfig({ ...(configData as any), db_connections: list })
      await refreshConfigOnly()
      toast(tr('连接已更新', 'Connection updated.'), 'success')
    } catch (e: unknown) {
      toast(tr('更新连接失败：', 'Failed to update connection: ') + formatErr(e), 'error')
    }
  }, [configData, refreshConfigOnly, toast])

  const deleteConnectionFromSidebar = useCallback(async (dbId: string) => {
    if (!configData) return
    const currentList = Array.isArray((configData as any).db_connections) ? ((configData as any).db_connections as any[]) : []
    const target = currentList.find((c: any) => c.id === dbId)
    const label = target?.name || target?.id || dbId
    if (!window.confirm(`Are you sure you want to delete the database connection "${label}"?`)) return
    try {
      const list = currentList.filter((c: any) => c.id !== dbId)
      let nextActiveId: any = (configData as any).active_db_id
      if (String(nextActiveId || '') === dbId) {
        nextActiveId = list.length > 0 ? list[0].id : null
      }
      await api.updateConfig({ ...(configData as any), db_connections: list, active_db_id: nextActiveId })
      await initData()
      toast('Connection deleted.', 'success')
    } catch (e: unknown) {
      toast('Failed to delete connection: ' + formatErr(e), 'error')
    }
  }, [configData, initData, toast])

  const duplicateConnectionFromSidebar = useCallback(async (dbId: string) => {
    if (!configData) return
    try {
      const list = Array.isArray((configData as any).db_connections) ? [...((configData as any).db_connections as any[])] : []
      const target = list.find((c: any) => c.id === dbId)
      if (!target) return
      const copy = {
        ...target,
        id: 'db-' + Date.now(),
        name: `${String(target.name || target.id)} Copy`,
      }
      list.push(copy)
      await api.updateConfig({ ...(configData as any), db_connections: list })
      await refreshConfigOnly()
      toast(tr('连接已复制', 'Connection duplicated.'), 'success')
    } catch (e: unknown) {
      toast(tr('复制连接失败：', 'Failed to duplicate connection: ') + formatErr(e), 'error')
    }
  }, [configData, refreshConfigOnly, toast])

  const disconnectConnectionFromSidebar = useCallback(async (dbId: string) => {
    if (!configData) return
    if (String((configData as any).active_db_id || '') !== dbId) return
    try {
      await api.updateConfig({ ...(configData as any), active_db_id: null })
      await initData()
      toast(tr('连接已断开', 'Connection disconnected.'), 'success')
    } catch (e: unknown) {
      toast(tr('断开连接失败：', 'Failed to disconnect connection: ') + formatErr(e), 'error')
    }
  }, [configData, initData, toast])

  const renameGroupFromSidebar = useCallback(async (oldGroup: string, newGroup: string) => {
    if (!configData) return
    const trimmedOld = oldGroup.trim()
    const trimmedNew = newGroup.trim()
    if (!trimmedOld || !trimmedNew) return
    try {
      const list = Array.isArray((configData as any).db_connections) ? [...((configData as any).db_connections as any[])] : []
      const mapped = list.map((c: any) => {
        const g = String(c.group_name || tr('未分组', 'Ungrouped'))
        if (g === trimmedOld) return { ...c, group_name: trimmedNew }
        return c
      })
      await api.updateConfig({ ...(configData as any), db_connections: mapped })
      await refreshConfigOnly()
      toast(tr('分组已重命名', 'Group renamed.'), 'success')
    } catch (e: unknown) {
      toast(tr('重命名分组失败：', 'Failed to rename group: ') + formatErr(e), 'error')
    }
  }, [configData, refreshConfigOnly, toast, tr])

  const ungroupFromSidebar = useCallback(async (groupName: string) => {
    if (!configData) return
    const trimmed = groupName.trim()
    if (!trimmed || trimmed === tr('未分组', 'Ungrouped')) return
    try {
      const list = Array.isArray((configData as any).db_connections) ? [...((configData as any).db_connections as any[])] : []
      const mapped = list.map((c: any) => {
        const g = String(c.group_name || tr('未分组', 'Ungrouped'))
        if (g === trimmed) return { ...c, group_name: null }
        return c
      })
      await api.updateConfig({ ...(configData as any), db_connections: mapped })
      await refreshConfigOnly()
      toast(tr('分组已清空', 'Group cleared.'), 'success')
    } catch (e: unknown) {
      toast(tr('清空分组失败：', 'Failed to clear group: ') + formatErr(e), 'error')
    }
  }, [configData, refreshConfigOnly, toast, tr])

  const batchMoveConnectionsFromSidebar = useCallback(async (connIds: string[], groupName: string | null) => {
    if (!configData || connIds.length === 0) return
    try {
      const set = new Set(connIds)
      const list = Array.isArray((configData as any).db_connections) ? [...((configData as any).db_connections as any[])] : []
      const mapped = list.map((c: any) => (
        set.has(String(c.id))
          ? { ...c, group_name: groupName && groupName.trim() ? groupName.trim() : null }
          : c
      ))
      await api.updateConfig({ ...(configData as any), db_connections: mapped })
      await refreshConfigOnly()
      toast(tr('批量移动完成', 'Batch move completed.'), 'success')
    } catch (e: unknown) {
      toast(tr('批量移动失败：', 'Batch move failed: ') + formatErr(e), 'error')
    }
  }, [configData, refreshConfigOnly, toast, tr])

  const handleExplain = () => {
    const currentSql = sqlRef.current
    if (!currentSql || !currentSql.trim() || currentSql.trim() === '-- Generated SQL will appear here\n') return
    const newTabId = `explain-${Date.now()}`
    setTabs([...tabs, {
      id: newTabId,
      type: 'explain',
      title: tr('执行计划', 'Explain Plan'),
      payload: { sql: currentSql }
    }])
    setActiveTabId(newTabId)
  }

  const handleAIOptimize = async () => {
    const currentSql = sqlRef.current
    if (!currentSql || !currentSql.trim() || currentSql.trim() === '-- Generated SQL will appear here\n') return
    updateActiveTabState({ isGenerating: true, errorObj: null, lastExplanation: null })
    try {
      const result = await api.aiQuery(`Please optimize the following SQL query and explain the improvements:\n${currentSql}`)
      if (result.sql && result.sql !== currentSql && !result.sql.includes('Please optimize')) {
        updateActiveTabState({ sql: result.sql })
      }
      if (result.explanation) {
        updateActiveTabState({ lastExplanation: result.explanation })
      }
      toast("AI 优化完成 (AI Optimize complete)", "success")
    } catch (e: unknown) {
      updateActiveTabState({ errorObj: parseError(e) })
    } finally {
      updateActiveTabState({ isGenerating: false })
    }
  }

  const handleAIExplain = async () => {
    const currentSql = sqlRef.current
    if (!currentSql || !currentSql.trim() || currentSql.trim() === '-- Generated SQL will appear here\n') return
    updateActiveTabState({ isGenerating: true, errorObj: null, lastExplanation: null })
    try {
      const result = await api.aiQuery(`Please explain the following SQL query in detail:\n${currentSql}`)
      if (result.explanation) {
        updateActiveTabState({ lastExplanation: result.explanation })
      }
      toast("AI 解释完成 (AI Explain complete)", "success")
    } catch (e: unknown) {
      updateActiveTabState({ errorObj: parseError(e) })
    } finally {
      updateActiveTabState({ isGenerating: false })
    }
  }

  const handleFixWithAI = async () => {
    if (!activeTabState.errorObj || !activeTabState.errorObj.message) return;
    const currentSql = sqlRef.current;
    updateActiveTabState({ isGenerating: true });
    try {
      const result = await api.aiExplainError(activeTabState.errorObj.message, currentSql);
      if (result.fixed_query && result.fixed_query !== currentSql) {
        updateActiveTabState({ sql: result.fixed_query });
      }
      if (result.explanation) {
        updateActiveTabState({ lastExplanation: result.explanation });
      }
      updateActiveTabState({ errorObj: null });
      toast("AI 已尝试修复 (AI attempted to fix the error)", "success");
    } catch (e: unknown) {
      toast("AI 修复失败: " + formatErr(e), "error");
    } finally {
      updateActiveTabState({ isGenerating: false });
    }
  }

  const handleTabClose = (id: string) => {
    const newTabs = tabs.filter(t => t.id !== id)
    if (newTabs.length === 0) {
      newTabs.push({ id: 'query-1', type: 'query', title: 'Query 1' })
    }
    setTabs(newTabs)
    setTabStates(prev => {
      const next = { ...prev };
      delete next[id];
      return next;
    })
    if (activeTabId === id) {
      setActiveTabId(newTabs[newTabs.length - 1].id)
    }
  }

  const handleTabCloseOthers = (id: string) => {
    const newTabs = tabs.filter(t => t.id === id)
    if (newTabs.length === 0) {
      newTabs.push({ id: 'query-1', type: 'query', title: 'Query 1' })
    }
    setTabs(newTabs)
    setTabStates(prev => {
      const next = { ...prev };
      const keepIds = new Set(newTabs.map(t => t.id));
      Object.keys(next).forEach(key => {
        if (!keepIds.has(key)) delete next[key];
      });
      return next;
    })
    setActiveTabId(newTabs[0].id)
  }

  const handleTabCloseAll = () => {
    const newTabs: TabItem[] = [{ id: 'query-1', type: 'query', title: 'Query 1' }]
    setTabs(newTabs)
    setTabStates(prev => {
      const next = { ...prev };
      const keepIds = new Set(newTabs.map(t => t.id));
      Object.keys(next).forEach(key => {
        if (!keepIds.has(key)) delete next[key];
      });
      return next;
    })
    setActiveTabId(newTabs[0].id)
  }
  const handleSqlUpload = async (e: React.ChangeEvent<HTMLInputElement>) => {
    const file = e.target.files?.[0]
    if (!file) return
    
    try {
      const text = await file.text()
      const result = await api.parseSchema(text)
      setSchemaData(result)
      toast("Offline schema parsed successfully! AI now has table context.", "success")
    } catch (e: unknown) {
      toast("Failed to parse SQL file: " + formatErr(e), "error")
    } finally {
      if (e.target) e.target.value = ''
    }
  }
  const handleGenerate = async (overrideQuery?: string) => {
    const q = overrideQuery || activeTabState.query
    if (!q.trim()) return
    updateActiveTabState({ isGenerating: true, errorObj: null, lastExplanation: null })
    
    try {
      const chatHistory = activeTabState.chatHistory || [];
      const historyToPass = chatHistory.slice(-5).filter(msg => msg && Object.keys(msg).length > 0);
      
      const result = await api.chatToSql(q, historyToPass)
      
      const newHistory = [...chatHistory, { role: 'user', content: q }, { role: 'assistant', content: result.sql }];
      
      updateActiveTabState({ 
        sql: result.sql,
        lastExplanation: result.explanation || null,
        lastQuery: q,
        query: '',
        chatHistory: newHistory
      })
      setShowCommandPalette(false)
    } catch (e: unknown) {
      const err = parseError(e)
      updateActiveTabState({ errorObj: err })
      if (err.title.includes('Auth Error')) {
        // If it's a token error, show a prompt to reconfigure
        setTimeout(() => setShowOnboarding(true), 1500)
      }
    } finally {
      updateActiveTabState({ isGenerating: false })
    }
  }


  const handleExecute = async (force: boolean = false, overrideSql?: string) => {
    let currentSql = overrideSql || sqlRef.current

    if (!overrideSql) {
      const activeEditor = editorRefs.current[activeTabIdRef.current];
      if (activeEditor) {
        const selection = activeEditor.getSelection();
        if (selection && !selection.isEmpty()) {
          const model = activeEditor.getModel();
          const selectedText = model?.getValueInRange(selection);
          if (selectedText && selectedText.trim()) {
            currentSql = selectedText;
          }
        }
      }
    }

    if (!currentSql || !currentSql.trim()) return

    // Clear previous error decorations
    if (errorDecorationsRef.current) {
      errorDecorationsRef.current.clear();
      errorDecorationsRef.current = null;
    }
    
    // If not forced, check for variables first
    if (!force && !overrideSql) {
      // Find all :variables that are not inside quotes
      // A simple regex to find :varName
      const varRegex = /(?<!['"])\B:([a-zA-Z0-9_]+)\b/g;
      const matches = [...currentSql.matchAll(varRegex)];
      
      if (matches.length > 0) {
        const uniqueVars = Array.from(new Set(matches.map(m => m[1])));
        setSqlVariables(uniqueVars.map(name => ({ name, value: '' })));
        setPendingSqlWithVars(currentSql);
        setShowVariablesModal(true);
        return;
      }
    }

    updateActiveTabState({ isExecuting: true, errorObj: null })
    
    try {
      const result = await api.executeSql(currentSql, force, configData?.active_db_id)
      updateActiveTabState({ executeResult: result })
      setShowConfirmModal(false)

      if (/\b(CREATE|ALTER|DROP|TRUNCATE|RENAME|GRANT|REVOKE)\b/i.test(currentSql)) {
        api.getSchema(configData?.active_db_id).then(setSchemaData).catch(console.error)
      }
    } catch (e: unknown) {
      const err = parseError(e)
      if (err.title === '高危操作警告 (Dangerous SQL)') {
        setPendingDangerousSql(currentSql)
        setShowConfirmModal(true)
      } else {
        updateActiveTabState({ errorObj: err })
        const match = err.message.match(/at line (\d+)/i);
        if (match && match[1]) {
          const lineNum = parseInt(match[1], 10);
          const activeEditor = editorRefs.current[activeTabIdRef.current];
          if (activeEditor && monacoRef.current) {
            const model = activeEditor.getModel();
            if (model) {
              const lineContent = model.getLineContent(lineNum) || '';
              const decoration = {
                range: new monacoRef.current.Range(lineNum, 1, lineNum, lineContent.length + 1),
                options: {
                  isWholeLine: true,
                  inlineClassName: 'squiggly-error',
                  hoverMessage: { value: `**Error**: ${err.message}` }
                }
              };
              if (activeEditor.createDecorationsCollection) {
                errorDecorationsRef.current = activeEditor.createDecorationsCollection([decoration]);
              } else {
                const decs = activeEditor.deltaDecorations([], [decoration]);
                errorDecorationsRef.current = {
                  clear: () => activeEditor.deltaDecorations(decs, [])
                };
              }
            }
          }
        }
      }
    } finally {
      updateActiveTabState({ isExecuting: false })
    }
  }

  const formatSqlEditor = () => {
    const currentSql = sqlRef.current
    if (!currentSql || currentSql.trim() === '-- Generated SQL will appear here\n' || currentSql.trim() === '-- Generated SQL will appear here') return;
    try {
      const formatted = formatSql(currentSql, { language: 'mysql', keywordCase: 'upper' });
      updateActiveTabState({ sql: formatted });
    } catch (e) {
      console.error('Failed to format SQL:', sanitizeForLog(e));
    }
  };

  const editorRefs = useRef<Record<string, MonacoEditor>>({});
  const monacoRef = useRef<Monaco | null>(null);
  const [monacoReady, setMonacoReady] = useState(false);
  const hoverProviderRef = useRef<{ dispose: () => void } | null>(null);
  const completionProviderRef = useRef<{ dispose: () => void } | null>(null);

  const handleEditorDidMount = (editor: MonacoEditor, monaco: Monaco, tabId: string) => {
    editorRefs.current[tabId] = editor;
    monacoRef.current = monaco;
    setMonacoReady(true);
    editor.addCommand(monaco.KeyMod.CtrlCmd | monaco.KeyCode.Enter, () => {
      handleExecute(false);
    });
    editor.addCommand(monaco.KeyMod.Shift | monaco.KeyMod.Alt | monaco.KeyCode.KeyF, () => {
      formatSqlEditor();
    });
  };

  useEffect(() => {
    if (!monacoReady || !monacoRef.current || !schemaData?.tables) return;
    
    // Dispose previous provider if exists
    if (hoverProviderRef.current) {
      hoverProviderRef.current.dispose();
    }
    if (completionProviderRef.current) {
      completionProviderRef.current.dispose();
    }

    const monaco = monacoRef.current;
    hoverProviderRef.current = monaco.languages.registerHoverProvider('sql', {
      provideHover: (model: monaco.editor.ITextModel, position: monaco.Position) => {
        const word = model.getWordAtPosition(position);
        if (!word) return null;
        
        const tableName = word.word;
        const table = schemaData.tables.find((t: TableWithDetails) => 
          t.table_name.toLowerCase() === tableName.toLowerCase()
        );
        
        if (!table) return null;
        
        const columnMarkdown = table.columns?.map((c: ColumnInfo) => 
          `- **${c.column_name}** \`${c.data_type}\`${c.column_key === 'PRI' ? ' 🔑' : ''} ${c.column_comment ? `*${c.column_comment}*` : ''}`
        ).join('\n') || 'No columns';

        return {
          range: new monaco.Range(
            position.lineNumber,
            word.startColumn,
            position.lineNumber,
            word.endColumn
          ),
          contents: [
            { value: `**Table: ${table.table_name}**` },
            { value: columnMarkdown }
          ]
        };
      }
    });

    completionProviderRef.current = monaco.languages.registerCompletionItemProvider('sql', {
      provideCompletionItems: (model: monaco.editor.ITextModel, position: monaco.Position) => {
        const word = model.getWordUntilPosition(position);
        const range = {
          startLineNumber: position.lineNumber,
          endLineNumber: position.lineNumber,
          startColumn: word.startColumn,
          endColumn: word.endColumn
        };

        const suggestions: monaco.languages.CompletionItem[] = [];
        const columns = new Set<string>();

        schemaData.tables.forEach((t: TableWithDetails) => {
          suggestions.push({
            label: t.table_name,
            kind: monaco.languages.CompletionItemKind.Struct,
            insertText: t.table_name,
            range: range
          });

          if (t.columns) {
            t.columns.forEach((c: ColumnInfo) => {
              columns.add(c.column_name);
            });
          }
        });

        columns.forEach(col => {
          suggestions.push({
            label: col,
            kind: monaco.languages.CompletionItemKind.Field,
            insertText: col,
            range: range
          });
        });

        return { suggestions };
      }
    });

    return () => {
      if (hoverProviderRef.current) {
        hoverProviderRef.current.dispose();
      }
      if (completionProviderRef.current) {
        completionProviderRef.current.dispose();
      }
    };
  }, [schemaData, monacoReady]);

  const insertTextAtCursor = (text: string) => {
    const activeEditor = editorRefs.current[activeTabId];
    if (activeEditor) {
      const position = activeEditor.getPosition();
      if (!position) return;
      activeEditor.executeEdits('my-source', [{
        range: {
          startLineNumber: position.lineNumber,
          startColumn: position.column,
          endLineNumber: position.lineNumber,
          endColumn: position.column
        },
        text: text,
        forceMoveMarkers: true
      }]);
      activeEditor.focus();
    } else {
      updateActiveTabState({ sql: activeTabState.sql + text });
    }
  };

  const downloadSql = () => {
    if (!activeTabState.executeResult?.rows || activeTabState.executeResult.rows.length === 0) return;
    
    const rows = activeTabState.executeResult.rows;
    const headers = Object.keys(rows[0]);
    const tableName = 'query_result'; // Default for custom queries

    const sqlContent = rows.map((row: Record<string, unknown>) => {
      const values = headers.map(h => {
        const val = row[h];
        if (val === null || val === undefined) return 'NULL';
        if (typeof val === 'number' || typeof val === 'boolean') return String(val);
        // Escape single quotes for SQL string
        return `'${String(val).replace(/'/g, "''")}'`;
      });
      return `INSERT INTO \`${tableName}\` (${headers.map(h => `\`${h}\``).join(', ')}) VALUES (${values.join(', ')});`;
    }).join('\n');

    const blob = new Blob([sqlContent], { type: 'application/sql;charset=utf-8;' });
    const url = URL.createObjectURL(blob);
    const link = document.createElement('a');
    link.href = url;
    link.setAttribute('download', `${tableName}_${new Date().toISOString().replace(/[:.]/g, '-')}.sql`);
    document.body.appendChild(link);
    link.click();
    document.body.removeChild(link);
  };

  const downloadCsv = () => {
    if (!activeTabState.executeResult?.rows || activeTabState.executeResult.rows.length === 0) return;
    
    const rows = activeTabState.executeResult.rows;
    const headers = Object.keys(rows[0]);
    const csvContent = [
      headers.join(','),
      ...rows.map((row: Record<string, unknown>) => 
        headers.map(h => {
          let val = row[h];
          if (val === null || val === undefined) return '';
          val = String(val).replace(/"/g, '""');
          return `"${val}"`;
        }).join(',')
      )
    ].join('\n');

    const blob = new Blob(['\uFEFF' + csvContent], { type: 'text/csv;charset=utf-8;' }); // Add BOM for Excel
    const url = URL.createObjectURL(blob);
    const link = document.createElement('a');
    link.href = url;
    link.setAttribute('download', `query_result_${new Date().toISOString().replace(/[:.]/g, '-')}.csv`);
    document.body.appendChild(link);
    link.click();
    document.body.removeChild(link);
  };

  if (!isReady) {
    return <SkeletonLoader />
  }

  return (
    <div ref={appRootRef} className="flex h-screen bg-dark-bg text-dark-text overflow-hidden">
      {showOnboarding && (
        <Onboarding
          onComplete={() => {
            window.localStorage.setItem('onboarding_done', '1')
            initData()
          }}
        />
      )}
      
      {/* Sidebar */}
      <div
        className="border-r border-dark-border bg-dark-panel flex flex-col z-10 shrink-0"
        style={{ width: `${sidebarWidth}px` }}
      >
        <div className="p-4 border-b border-dark-border flex flex-col gap-2">
          <div className="flex items-center justify-between">
            <div className="flex items-center gap-2">
              <Database className="w-5 h-5 text-dark-accent" />
              <span className="font-semibold tracking-wide flex items-center gap-2">
                {tr('本地 AI SQL', 'Local AI SQL')}
                {configData?.db_connections?.find((c: DbConnection) => c.id === configData.active_db_id)?.is_read_only && (
                  <span className="text-[10px] bg-red-500/20 text-red-500 border border-red-500/30 px-1.5 py-0.5 rounded uppercase tracking-wider font-bold shadow-sm">
                    [只读]
                  </span>
                )}
              </span>
            </div>
            <div
              className="w-2 h-2 rounded-full bg-green-500 shadow-[0_0_8px_#22c55e]"
              title={
                configData?.active_db_id
                  ? (configData?.db_connections || []).find((c: DbConnection) => c.id === configData.active_db_id)?.url || "Connected"
                  : (configData?.db_url || tr('已连接', 'Connected'))
              }
            ></div>
          </div>
        </div>
        
        <div className="flex border-b border-dark-border">
          <button 
            className={`flex-1 py-2 text-sm font-medium ${sidebarTab === 'schema' ? 'text-dark-accent border-b-2 border-dark-accent' : 'text-gray-400 hover:text-gray-200'}`}
            onClick={() => setSidebarTab('schema')}
          >
            {tr('结构', 'Schema')}
          </button>
          <button 
            className={`flex-1 py-2 text-sm font-medium ${sidebarTab === 'smart_snippets' ? 'text-dark-accent border-b-2 border-dark-accent' : 'text-gray-400 hover:text-gray-200'}`}
            onClick={() => setSidebarTab('smart_snippets')}
          >
            {tr('智能片段', 'Smart Snippets')}
          </button>
          <button 
            className={`flex-1 py-2 text-sm font-medium ${sidebarTab === 'history' ? 'text-dark-accent border-b-2 border-dark-accent' : 'text-gray-400 hover:text-gray-200'}`}
            onClick={() => setSidebarTab('history')}
          >
            {tr('历史', 'History')}
          </button>
        </div>

        {sidebarTab === 'schema' ? (
          <DbExplorerSidebar
            configData={configData}
            schemaData={schemaData}
            isRefreshingSchema={isRefreshingSchema}
            onRefreshSchema={() => {
              setIsRefreshingSchema(true)
              api.getSchema(configData?.active_db_id).then(setSchemaData).finally(() => setIsRefreshingSchema(false))
            }}
            showSqlUpload={!configData?.db_url}
            onSqlUpload={handleSqlUpload}
            onSwitchActiveDb={(dbId) => {
              switchActiveDbFromSidebar(dbId)
            }}
            onAddConnection={(name, url) => {
              addConnectionFromSidebar(name, url)
            }}
            onUpdateConnection={(connId, patch) => {
              updateConnectionFromSidebar(connId, patch)
            }}
            onDuplicateConnection={(connId) => {
              duplicateConnectionFromSidebar(connId)
            }}
            onDisconnectConnection={(connId) => {
              disconnectConnectionFromSidebar(connId)
            }}
            onRenameGroup={(oldGroup, newGroup) => {
              renameGroupFromSidebar(oldGroup, newGroup)
            }}
            onClearGroup={(groupName) => {
              ungroupFromSidebar(groupName)
            }}
            onBatchMoveConnections={(connIds, groupName) => {
              batchMoveConnectionsFromSidebar(connIds, groupName)
            }}
            onDeleteConnection={(dbId) => {
              deleteConnectionFromSidebar(dbId)
            }}
            onOpenTable={(dbId, dbName, tableName) => {
              handleTableDoubleClick({ table_name: tableName }, dbId, dbName)
            }}
            onInsertTableName={(tableName) => {
              insertTextAtCursor(tableName)
              toast(tr(`已插入 ${tableName}`, `Inserted ${tableName}`), "success")
            }}
            onTableContextMenu={(x, y, table) => {
              setContextMenu({ x, y, table })
            }}
          />
        ) : sidebarTab === 'smart_snippets' ? (
          <div className="flex-1 overflow-hidden border-t border-dark-border">
            <AiTrainingPanel onInsertSql={(text) => {
              insertTextAtCursor(text)
              toast(tr('已插入 SQL', 'Inserted SQL'), "success")
            }} />
          </div>
        ) : (
          <div className="flex-1 overflow-hidden border-t border-dark-border">
            <SqlHistory onInsertSql={(text) => {
              insertTextAtCursor(text)
              toast(tr('已插入 SQL', 'Inserted SQL'), "success")
            }} />
          </div>
        )}
        <div className="p-3 border-t border-dark-border flex items-center gap-2 hover:bg-[#21262d] cursor-pointer transition-colors duration-150" onClick={() => setShowRulesPanel(true)}>
          <BookMarked className="w-4 h-4 text-gray-400" />
          <span className="text-sm text-gray-400 flex-1">{tr('智能规则', 'Smart Rules')}</span>
          {rules.length > 0 && (
            <span className="text-[10px] bg-blue-500/20 text-blue-400 px-1.5 rounded">{rules.length}</span>
          )}
        </div>
        <div className="p-3 border-t border-dark-border flex items-center gap-2 hover:bg-[#21262d] cursor-pointer transition-colors duration-150" onClick={() => setShowSettings(true)}>
          <Settings className="w-4 h-4 text-gray-400" />
          <span className="text-sm text-gray-400 flex-1">{tr('设置', 'Settings')}</span>
        </div>
        <div className="p-3 border-t border-dark-border flex items-center gap-2 hover:bg-[#21262d] cursor-pointer transition-colors duration-150" onClick={() => setShowHelpModal(true)}>
          <Keyboard className="w-4 h-4 text-gray-400" />
          <span className="text-sm text-gray-400 flex-1">{tr('快捷键帮助', 'Shortcuts Help')}</span>
        </div>
      </div>

      {/* Sidebar Resizer */}
      <div
        className={`w-1.5 shrink-0 cursor-col-resize bg-[#0d1117] hover:bg-[#161b22] ${isResizingSidebar ? 'bg-[#161b22]' : ''}`}
        onMouseDown={() => setIsResizingSidebar(true)}
        title="拖动调整左侧栏宽度"
      />

      {/* Main Content */}
      <div className="flex-1 flex flex-col relative bg-[#0a0c10]">
        {/* Tools Navigation Bar */}
        <ToolsNav 
          onSelectTool={(toolId) => {
            let title = '';
            if (toolId === 'schema-sync') title = tr('结构同步', 'Schema Sync');
            if (toolId === 'data-sync') title = tr('数据同步', 'Data Sync');
            if (toolId === 'perf-sync') title = tr('同步压测', 'Perf Sync');
            if (toolId === 'go-live') title = tr('上线门禁', 'Go-live Gate');
            if (toolId === 'db-security') title = tr('权限与用户管理', 'Permissions & Users');
            if (toolId === 'db-events') title = tr('事件与触发器', 'Events & Triggers');
            if (toolId === 'model-compare') title = tr('模型对比', 'Model Compare');
            if (toolId === 'visual-sync') title = tr('可视化同步向导', 'Visual Sync Wizard');
            if (toolId === 'data-transfer') title = tr('数据传输', 'Data Transfer');
            if (toolId === 'data-analytics') title = tr('数据分析图表', 'Data Analytics');
            if (toolId === 'advanced-center') {
              const newTabId = `advanced-center-${Date.now()}`
              setTabs([...tabs, { id: newTabId, type: 'advanced-center', title: tr('高级工具中心', 'Advanced Tools Hub') }])
              setActiveTabId(newTabId)
              return
            }
            if (toolId === 'go-live-reports') {
              const newTabId = `go-live-reports-${Date.now()}`
              setTabs([...tabs, { id: newTabId, type: 'go-live-reports', title: '门禁报告' }])
              setActiveTabId(newTabId)
              return
            }
            if (toolId === 'go-live-audit') {
              const newTabId = `go-live-audit-${Date.now()}`
              setTabs([...tabs, { id: newTabId, type: 'go-live-audit', title: '门禁审计' }])
              setActiveTabId(newTabId)
              return
            }
            if (toolId === 'query-builder') {
              const newTabId = `builder-${Date.now()}`
              setTabs([...tabs, { id: newTabId, type: 'query-builder', title: tr('查询构建器', 'Query Builder') }])
              setActiveTabId(newTabId)
              return;
            }
            if (toolId === 'ai-training') {
              const newTabId = `ai-training-${Date.now()}`
              setTabs([...tabs, { id: newTabId, type: 'ai-training', title: tr('AI 训练面板', 'AI Training') }])
              setActiveTabId(newTabId)
              return;
            }
            setWizardConfig({ isOpen: true, title, type: toolId });
          }} 
        />
        
        {/* Header Tabs */}
        <Tabs 
          tabs={tabs}
          activeTabId={activeTabId}
          onTabClick={setActiveTabId}
          onTabClose={handleTabClose}
          onTabCloseOthers={handleTabCloseOthers}
          onTabCloseAll={handleTabCloseAll}
          onTabAdd={() => {
            const id = `query-${Date.now()}`
            setTabs([...tabs, { id, type: 'query', title: `Query ${tabs.filter(t => t.type === 'query').length + 1}` }])
            setActiveTabId(id)
          }}
        />

        {/* Content Router */}
        <div className="flex-1 flex flex-col relative overflow-hidden">
          {tabs.map(tab => {
            const isActive = tab.id === activeTabId;
            const tabState = tabStates[tab.id] || {
              sql: '-- Generated SQL will appear here\n',
              query: '',
              lastQuery: '',
              isGenerating: false,
              isExecuting: false,
              executeResult: null,
              errorObj: null,
              lastExplanation: null,
              resultsView: 'table',
              chatHistory: []
            };
            return (
              <div key={tab.id} className={`absolute inset-0 flex flex-col ${isActive ? 'z-10 opacity-100 pointer-events-auto' : 'z-0 opacity-0 pointer-events-none'}`} style={{ display: isActive ? 'flex' : 'none' }}>
                {tab.type === 'query' ? (
                  <div ref={isActive ? queryPaneRef : undefined} className="flex flex-col h-full min-h-0">
                    {/* Editor Area */}
                    <div
                      className="flex-1 border-b border-dark-border relative group min-h-0"
                    >
                      {/* Save Rule Floating Button */}
                      {tabState.lastQuery && tabState.sql && !tabState.isExecuting && (
                        <motion.button
                          initial={{ opacity: 0, y: 10 }}
                          animate={{ opacity: 1, y: 0 }}
                          whileHover={{ scale: 1.05 }}
                          onClick={handleSaveRule}
                          disabled={isSavingRule}
                          className="absolute top-4 right-4 z-10 hidden group-hover:flex items-center gap-1.5 px-3 py-1.5 bg-blue-500/10 hover:bg-blue-500/20 border border-blue-500/30 rounded text-xs text-blue-400 font-medium transition-all shadow-lg backdrop-blur-sm"
                        >
                          <Save className="w-3.5 h-3.5" />
                          {isSavingRule ? "Saving..." : "Save as Rule"}
                        </motion.button>
                      )}
                      {tabState.isGenerating && (
                        <div className="absolute inset-0 z-20 bg-[#0a0c10]/80 backdrop-blur-sm p-6 flex flex-col gap-4 pt-12 pointer-events-none">
                          <Skeleton className="h-6 w-3/4" />
                          <Skeleton className="h-6 w-1/2" />
                          <Skeleton className="h-6 w-2/3" />
                          <Skeleton className="h-6 w-5/6" />
                          <div className="flex items-center justify-center h-full text-blue-400 font-medium animate-pulse flex-col gap-3">
                            <Sparkles className="w-8 h-8" />
                            <span>AI 正在思考并生成 SQL...</span>
                          </div>
                        </div>
                      )}
                      <Suspense fallback={<div className="flex-1 flex items-center justify-center"><Skeleton className="h-full w-full" /></div>}>
                        <Editor
                          height="100%"
                          defaultLanguage="sql"
                          theme="vs-dark"
                          value={tabState.sql}
                          onChange={(val) => {
                            if (errorDecorationsRef.current) {
                              errorDecorationsRef.current.clear();
                              errorDecorationsRef.current = null;
                            }
                            setTabStates(prev => ({ ...prev, [tab.id]: { ...(prev[tab.id] || tabState), sql: val || '' } }))
                          }}
                          onMount={(editor, monaco) => handleEditorDidMount(editor, monaco, tab.id)}
                          options={{
                            minimap: { enabled: false },
                            fontSize: 14,
                            fontFamily: "'JetBrains Mono', 'Fira Code', monospace",
                            padding: { top: 24 },
                            scrollBeyondLastLine: false,
                            smoothScrolling: true,
                            cursorBlinking: "smooth",
                          }}
                        />
                      </Suspense>

                        <div className="absolute bottom-4 right-4 flex gap-3 items-center whitespace-nowrap max-w-[calc(100%-2rem)] overflow-x-auto py-1">
                          <span className="text-xs font-medium text-blue-400 bg-blue-500/10 px-2 py-1 rounded border border-blue-500/20 mr-2 shadow-sm whitespace-nowrap">
                            {dbType} Agent
                          </span>
                        {!isCompactActionBar && resolveModelsList.length > 0 && (
                          <div className="flex items-center gap-2 mr-1">
                            <select
                              value={resolveActiveModelId}
                              onChange={(e) => updateAiRuntime({ active_model_id: e.target.value, model_name: e.target.value }, `已切换模型：${e.target.value}`)}
                              disabled={isAiSwitching}
                              className="h-9 bg-dark-panel border border-dark-border hover:border-gray-500 text-gray-200 rounded px-2 text-xs transition-colors disabled:opacity-60"
                              title="快速切换模型"
                            >
                              {resolveModelsList.map((m: any) => (
                                <option key={m.id} value={m.id}>{m.display_name || m.id}</option>
                              ))}
                            </select>
                            <select
                              value={resolveActiveTier}
                              onChange={(e) => updateAiRuntime({ active_tier: e.target.value }, `已切换 Tier：${e.target.value}`)}
                              disabled={isAiSwitching || !activeModelSupportsTier}
                              className="h-9 bg-dark-panel border border-dark-border hover:border-gray-500 text-gray-200 rounded px-2 text-xs transition-colors disabled:opacity-60"
                              title={activeModelSupportsTier ? "快速切换 tier" : "该模型不支持 tier"}
                            >
                              <option value="fast">fast</option>
                              <option value="balanced">balanced</option>
                              <option value="high">high</option>
                              <option value="ultra">ultra</option>
                            </select>
                          </div>
                        )}
                        {!isCompactActionBar && tabState.lastExplanation && (
                          <span className="text-xs text-blue-400 bg-blue-500/10 border border-blue-500/20 px-3 py-1.5 rounded-full mr-2 max-w-sm truncate" title={tabState.lastExplanation}>
                            <TypingEffect text={tabState.lastExplanation} />
                          </span>
                        )}
                        {(!tabState.sql || tabState.sql.trim() === '-- Generated SQL will appear here') && !tabState.isGenerating && (
                          <span className="text-xs text-gray-500 hidden sm:inline-block border border-gray-700/50 bg-gray-800/30 px-2 py-1 rounded-full mr-1">
                            按 <kbd className="font-mono bg-black/50 px-1 rounded mx-0.5 text-gray-400">Cmd+K</kbd> 唤起 AI 指令
                          </span>
                        )}
                        <motion.button 
                          whileHover={{ scale: 1.02 }}
                          whileTap={{ scale: 0.98 }}
                          className="flex items-center justify-center gap-2 px-3 py-2 bg-dark-panel border border-dark-border hover:border-gray-500 hover:text-white rounded text-sm text-gray-300 transition-colors shadow-sm ripple whitespace-nowrap leading-none"
                          onClick={() => setShowCommandPalette(true)}
                        >
                          <Command className="w-4 h-4" />
                          <span className="whitespace-nowrap">{tr('询问 AI', 'Ask AI')} <span className="opacity-50 ml-1 text-xs bg-dark-bg px-1 rounded border border-dark-border">Cmd K</span></span>
                        </motion.button>
                        {!isCompactActionBar && (
                          <>
                            <motion.button 
                              whileHover={{ scale: 1.02 }}
                              whileTap={{ scale: 0.98 }}
                              className="flex items-center justify-center gap-2 px-3 py-2 bg-dark-panel border border-dark-border hover:border-blue-500 hover:text-blue-400 rounded text-sm text-gray-300 transition-colors shadow-sm ripple whitespace-nowrap leading-none"
                              onClick={handleAIOptimize}
                              disabled={tabState.isGenerating || !tabState.sql.trim() || tabState.sql.trim() === '-- Generated SQL will appear here\n'}
                            >
                              <Sparkles className="w-4 h-4" />
                              <span className="whitespace-nowrap">AI 优化 (Optimize)</span>
                            </motion.button>
                            <motion.button 
                              whileHover={{ scale: 1.02 }}
                              whileTap={{ scale: 0.98 }}
                              className="flex items-center justify-center gap-2 px-3 py-2 bg-dark-panel border border-dark-border hover:border-purple-500 hover:text-purple-400 rounded text-sm text-gray-300 transition-colors shadow-sm ripple whitespace-nowrap leading-none"
                              onClick={handleAIExplain}
                              disabled={tabState.isGenerating || !tabState.sql.trim() || tabState.sql.trim() === '-- Generated SQL will appear here\n'}
                            >
                              <Sparkles className="w-4 h-4" />
                              <span className="whitespace-nowrap">AI 解释 (Explain)</span>
                            </motion.button>
                            <motion.button 
                              whileHover={{ scale: 1.02 }}
                              whileTap={{ scale: 0.98 }}
                              className="flex items-center justify-center gap-2 px-3 py-2 bg-dark-panel border border-dark-border hover:border-gray-500 hover:text-white rounded text-sm text-gray-300 transition-colors shadow-sm ripple whitespace-nowrap leading-none"
                              onClick={handleExplain}
                              title="Execution Plan"
                              disabled={!tabState.sql.trim() || !configData?.db_url}
                            >
                              <span className="whitespace-nowrap">Explain (执行计划)</span>
                            </motion.button>
                            <motion.button 
                              whileHover={{ scale: 1.02 }}
                              whileTap={{ scale: 0.98 }}
                              className="flex items-center justify-center gap-2 px-3 py-2 bg-[#21262d] border border-dark-border hover:bg-[#30363d] hover:border-gray-500 hover:text-white rounded text-sm text-gray-300 transition-colors shadow-sm ripple whitespace-nowrap leading-none"
                              onClick={formatSqlEditor}
                              title="Format SQL (Shift+Alt+F)"
                            >
                              <AlignLeft className="w-4 h-4" />
                              <span className="whitespace-nowrap">Format</span>
                            </motion.button>
                          </>
                        )}
                        {isCompactActionBar && (
                          <div
                            className="relative"
                            onClick={(e) => {
                              e.stopPropagation()
                            }}
                          >
                            <button
                              className="flex items-center justify-center h-9 w-9 bg-[#21262d] border border-dark-border hover:bg-[#30363d] hover:border-gray-500 hover:text-white rounded text-sm text-gray-300 transition-colors shadow-sm"
                              onClick={() => setShowMoreActions(v => !v)}
                              title="更多操作"
                            >
                              <MoreHorizontal className="w-4 h-4" />
                            </button>
                            {showMoreActions && (
                              <div className="absolute bottom-11 right-0 w-56 bg-[#161b22] border border-[#30363d] rounded-lg shadow-2xl overflow-hidden z-30">
                                {resolveModelsList.length > 0 && (
                                  <div className="px-3 py-2 border-b border-[#30363d] space-y-2">
                                    <select
                                      value={resolveActiveModelId}
                                      onChange={(e) => updateAiRuntime({ active_model_id: e.target.value, model_name: e.target.value }, `已切换模型：${e.target.value}`)}
                                      disabled={isAiSwitching}
                                      className="h-8 w-full bg-dark-panel border border-dark-border hover:border-gray-500 text-gray-200 rounded px-2 text-xs transition-colors disabled:opacity-60"
                                      title="快速切换模型"
                                    >
                                      {resolveModelsList.map((m: any) => (
                                        <option key={m.id} value={m.id}>{m.display_name || m.id}</option>
                                      ))}
                                    </select>
                                    <select
                                      value={resolveActiveTier}
                                      onChange={(e) => updateAiRuntime({ active_tier: e.target.value }, `已切换 Tier：${e.target.value}`)}
                                      disabled={isAiSwitching || !activeModelSupportsTier}
                                      className="h-8 w-full bg-dark-panel border border-dark-border hover:border-gray-500 text-gray-200 rounded px-2 text-xs transition-colors disabled:opacity-60"
                                      title={activeModelSupportsTier ? "快速切换 tier" : "该模型不支持 tier"}
                                    >
                                      <option value="fast">fast</option>
                                      <option value="balanced">balanced</option>
                                      <option value="high">high</option>
                                      <option value="ultra">ultra</option>
                                    </select>
                                  </div>
                                )}
                                <button
                                  onClick={() => {
                                    setShowMoreActions(false)
                                    handleAIOptimize()
                                  }}
                                  disabled={tabState.isGenerating || !tabState.sql.trim() || tabState.sql.trim() === '-- Generated SQL will appear here\n'}
                                  className="w-full text-left px-3 py-2 text-sm text-gray-200 hover:bg-[#21262d] disabled:opacity-50"
                                >
                                  AI 优化 (Optimize)
                                </button>
                                <button
                                  onClick={() => {
                                    setShowMoreActions(false)
                                    handleAIExplain()
                                  }}
                                  disabled={tabState.isGenerating || !tabState.sql.trim() || tabState.sql.trim() === '-- Generated SQL will appear here\n'}
                                  className="w-full text-left px-3 py-2 text-sm text-gray-200 hover:bg-[#21262d] disabled:opacity-50"
                                >
                                  AI 解释 (Explain)
                                </button>
                                <button
                                  onClick={() => {
                                    setShowMoreActions(false)
                                    handleExplain()
                                  }}
                                  disabled={!tabState.sql.trim() || !configData?.db_url}
                                  className="w-full text-left px-3 py-2 text-sm text-gray-200 hover:bg-[#21262d] disabled:opacity-50"
                                >
                                  Explain (执行计划)
                                </button>
                                <button
                                  onClick={() => {
                                    setShowMoreActions(false)
                                    formatSqlEditor()
                                  }}
                                  className="w-full text-left px-3 py-2 text-sm text-gray-200 hover:bg-[#21262d]"
                                >
                                  Format
                                </button>
                              </div>
                            )}
                          </div>
                        )}
                        <motion.button 
                          whileHover={{ scale: 1.02 }}
                          whileTap={{ scale: 0.95 }}
                          onClick={() => handleExecute(false)}
                          disabled={tabState.isExecuting || !tabState.sql.trim() || !configData?.db_url}
                          title={!configData?.db_url ? "Execute requires live database connection" : "Execute SQL (Cmd+Enter)"}
                          className={`flex items-center justify-center gap-2 px-5 py-2 rounded text-sm font-medium text-white transition-all shadow-[0_0_15px_rgba(59,130,246,0.2)] ripple relative overflow-hidden disabled:opacity-50 disabled:cursor-not-allowed whitespace-nowrap leading-none ${
                            tabState.isExecuting ? 'bg-blue-700 cursor-wait' : 'bg-dark-accent hover:bg-blue-500 hover:shadow-[0_0_20px_rgba(59,130,246,0.4)]'
                          }`}
                        >
                          {tabState.isExecuting && (
                            <div className="absolute inset-0 shimmer-bg z-0"></div>
                          )}
                          <Play className="w-4 h-4 fill-current relative z-10" />
                          <span className="relative z-10 whitespace-nowrap">{tabState.isExecuting ? 'Executing...' : 'Run'}</span>
                        </motion.button>
                      </div>
                    </div>

                    <div
                      className={`h-2 shrink-0 cursor-row-resize bg-[#0d1117] border-y border-dark-border flex items-center justify-center ${isResizingResults ? 'bg-[#161b22]' : ''}`}
                      onMouseDown={() => setIsResizingResults(true)}
                      title="拖动调整结果区域高度"
                    >
                      <div className={`h-1 w-12 rounded-full transition-colors ${isResizingResults ? 'bg-blue-500/70' : 'bg-gray-600/70'}`} />
                    </div>

                    {/* Results Area */}
                    <div className="bg-dark-bg flex flex-col relative shrink-0 min-h-0" style={{ height: `${resultsPanelHeight}px` }}>
                      <div className="h-8 border-b border-dark-border bg-dark-panel flex items-center justify-between px-4">
                        <div className="flex items-center gap-4">
                          <span className="text-xs font-bold tracking-wider text-gray-400 uppercase">Results</span>
                          {tabState.executeResult?.rows && tabState.executeResult.rows.length > 0 && (
                            <div className="flex items-center bg-[#21262d] rounded overflow-hidden border border-[#30363d]">
                              <button
                                onClick={() => setTabStates(prev => ({ ...prev, [tab.id]: { ...(prev[tab.id] || tabState), resultsView: 'table' } }))}
                                className={`px-2 py-0.5 text-xs transition-colors ${tabState.resultsView === 'table' ? 'bg-blue-500/20 text-blue-400' : 'text-gray-400 hover:bg-[#30363d]'}`}
                              >
                                Table
                              </button>
                              <button
                                onClick={() => setTabStates(prev => ({ ...prev, [tab.id]: { ...(prev[tab.id] || tabState), resultsView: 'chart' } }))}
                                className={`px-2 py-0.5 text-xs transition-colors ${tabState.resultsView === 'chart' ? 'bg-blue-500/20 text-blue-400' : 'text-gray-400 hover:bg-[#30363d]'}`}
                              >
                                Chart
                              </button>
                            </div>
                          )}
                        </div>
                        {tabState.executeResult && (
                          <div className="flex items-center gap-2">
                            <button
                              onClick={() => setTabStates(prev => ({ ...prev, [tab.id]: { ...(prev[tab.id] || tabState), executeResult: null } }))}
                              className="text-xs text-red-400 hover:text-red-300 bg-[#21262d] hover:bg-[#30363d] px-2 py-0.5 rounded border border-[#30363d] transition-colors flex items-center gap-1"
                            >
                              <Trash2 className="w-3 h-3" />
                              Clear Results
                            </button>
                            {tabState.executeResult.rows && tabState.executeResult.rows.length > 0 && (
                              <>
                                <button 
                                  onClick={downloadCsv}
                                  className="text-xs text-gray-400 hover:text-white bg-[#21262d] hover:bg-[#30363d] px-2 py-0.5 rounded border border-[#30363d] transition-colors"
                                >
                                  Download CSV
                                </button>
                                <button 
                                  onClick={downloadSql}
                                  className="text-xs text-gray-400 hover:text-white bg-[#21262d] hover:bg-[#30363d] px-2 py-0.5 rounded border border-[#30363d] transition-colors"
                                >
                                  Download SQL
                                </button>
                              </>
                            )}
                          </div>
                        )}
                      </div>
                      <div className="flex-1 overflow-auto bg-grid-pattern relative">
                        {tabState.isExecuting && (
                          <div className="absolute inset-0 z-30 bg-[#0a0c10]/80 backdrop-blur-sm p-4 flex flex-col gap-3">
                            <div className="flex items-center gap-4 mb-4">
                              <Skeleton className="h-6 w-24" />
                              <Skeleton className="h-6 w-32" />
                              <Skeleton className="h-6 w-20" />
                            </div>
                            {Array.from({ length: 5 }).map((_, i) => (
                              <Skeleton key={i} className="h-8 w-full" />
                            ))}
                            <div className="absolute inset-0 flex items-center justify-center pointer-events-none">
                              <div className="flex flex-col items-center gap-3 text-blue-400 font-medium animate-pulse">
                                <Database className="w-8 h-8" />
                                <span>执行查询中...</span>
                              </div>
                            </div>
                          </div>
                        )}
                        {tabState.errorObj && (
                          <div className="absolute inset-0 p-4 bg-red-950/20 backdrop-blur-sm z-20 flex flex-col items-center justify-center">
                            <div className="bg-[#161b22] border border-red-500/30 rounded-lg p-5 max-w-2xl w-full shadow-2xl">
                              <div className="flex items-center gap-2 text-red-400 font-semibold mb-2">
                                <span className="w-2 h-2 rounded-full bg-red-500"></span>
                                {tabState.errorObj.title}
                              </div>
                              <div className="text-gray-300 text-sm mb-4 p-3 bg-red-500/10 rounded border border-red-500/20 font-mono overflow-auto">
                                {tabState.errorObj.message}
                              </div>
                              <div className="text-sm">
                                <span className="text-blue-400 font-medium">💡 解决方案：</span>
                                <span className="text-gray-400">{tabState.errorObj.solution}</span>
                              </div>
                              <div className="mt-4 flex justify-end">
                                <button
                                  onClick={handleFixWithAI}
                                  className="flex items-center gap-2 px-4 py-2 bg-blue-600 hover:bg-blue-500 text-white rounded text-sm font-medium transition-colors shadow-lg shadow-blue-500/20"
                                >
                                  <Sparkles className="w-4 h-4" />
                                  Fix with AI (解释并修复)
                                </button>
                              </div>
                            </div>
                          </div>
                        )}
                        
                        {!tabState.executeResult && !tabState.errorObj && (
                          <div className="absolute inset-0 flex flex-col items-center justify-center text-gray-500 pointer-events-none p-6">
                            <div className="w-16 h-16 bg-[#161b22] rounded-2xl border border-[#30363d] flex items-center justify-center mb-4 shadow-lg">
                              <Play className="w-8 h-8 text-gray-600" />
                            </div>
                            <h3 className="text-lg font-medium text-gray-400 mb-2">等待执行 (Awaiting Execution)</h3>
                            <p className="text-sm text-center max-w-md">
                              在上方输入自然语言并按 <kbd className="bg-[#21262d] px-1.5 py-0.5 rounded text-gray-300 mx-1">Cmd/Ctrl + K</kbd> 生成 SQL，<br/>
                              确认无误后点击右上角的 <span className="text-green-500">Run</span> 按钮，数据结果将显示在此处。
                            </p>
                          </div>
                        )}
                        
                        {tabState.executeResult ? (
                          <div className="w-full h-full">
                            {tabState.executeResult.rows && tabState.executeResult.rows.length > 0 ? (
                              tabState.resultsView === 'table' ? (
                                <SimpleDataTable data={tabState.executeResult.rows} />
                              ) : (
                                <DataCharts data={tabState.executeResult.rows} />
                              )
                            ) : (
                              <div className="flex flex-col items-center justify-center h-full text-gray-500 gap-2">
                                <span className="text-green-500/80 font-medium">Query executed successfully</span>
                                <span>{tabState.executeResult.affected_rows} rows affected.</span>
                              </div>
                            )}
                          </div>
                        ) : (
                          <div className="flex flex-col items-center justify-center h-full text-gray-500 gap-2">
                            <Table className="w-8 h-8 opacity-20" />
                            <span>No results to display. Click Run to execute query.</span>
                          </div>
                        )}
                      </div>
                      <div className="h-7 border-t border-dark-border bg-dark-panel px-4 flex items-center text-xs text-gray-500 justify-between">
                        <div className="flex items-center gap-4">
                          <span className="flex items-center gap-1.5">
                            <div className={`w-2 h-2 rounded-full ${tabState.executeResult ? 'bg-green-500/50' : 'bg-gray-500/50'}`}></div> 
                            {tabState.executeResult?.rows?.length || 0} rows
                            {tabState.executeResult?.execution_time_ms !== undefined && ` | Executed in ${tabState.executeResult.execution_time_ms} ms`}
                          </span>
                          <span className="opacity-50">|</span>
                          <span>{resolveActiveModelLabel}{resolveActiveTier ? ` · ${resolveActiveTier}` : ''}</span>
                        </div>
                        <div className="flex items-center gap-2 text-dark-accent/80">
                          <Sparkles className="w-3 h-3" />
                          <span className="capitalize">{String((configData as any)?.ai_mode || 'Direct')} Mode</span>
                        </div>
                      </div>
                    </div>
                  </div>
                ) : tab.type === 'table' ? (
                  <Suspense fallback={<SkeletonLoader />}>
                    <TableWorkspace tableName={tab.payload?.tableName} dbId={tab.payload?.dbId} isActive={isActive} />
                  </Suspense>
                ) : tab.type === 'explain' ? (
                  <Suspense fallback={<SkeletonLoader />}>
                    <ExecutionPlan sql={tab.payload?.sql} />
                  </Suspense>
                ) : tab.type === 'go-live-reports' ? (
                  <GoLiveReportsTab isActive={isActive} />
                ) : tab.type === 'go-live-audit' ? (
                  <GoLiveAuditTab isActive={isActive} />
                ) : tab.type === 'advanced-center' ? (
                  <AdvancedToolsHub
                    onOpenTool={(toolId) => {
                      let title = '';
                      if (toolId === 'db-security') title = tr('权限与用户管理', 'Permissions & Users');
                      if (toolId === 'db-events') title = tr('事件与触发器', 'Events & Triggers');
                      if (toolId === 'model-compare') title = tr('模型对比', 'Model Compare');
                      if (toolId === 'visual-sync') title = tr('可视化同步向导', 'Visual Sync Wizard');
                      setWizardConfig({ isOpen: true, title, type: toolId });
                    }}
                  />
                ) : tab.type === 'query-builder' ? (
                  <Suspense fallback={<SkeletonLoader />}>
                    <QueryBuilder 
                      schemaData={schemaData} 
                      onApplySql={(generatedSql) => {
                        let queryTab = tabs.find(t => t.type === 'query');
                        if (!queryTab) {
                          queryTab = { id: 'query-1', type: 'query', title: 'Query 1' };
                          setTabs([...tabs, queryTab]);
                        }
                        setTabStates(prev => ({
                          ...prev,
                          [queryTab!.id]: {
                            ...(prev[queryTab!.id] || activeTabState),
                            sql: generatedSql
                          }
                        }));
                        setActiveTabId(queryTab.id);
                      }} 
                    />
                  </Suspense>
                ) : tab.type === 'ai-training' ? (
                  <AiTrainingPanel onInsertSql={(text) => {
                    insertTextAtCursor(text)
                    toast("Inserted SQL", "success")
                  }} />
                ) : null}
              </div>
            )
          })}
        </div>

        {/* Command Palette Overlay */}
        <CommandPalette
          isOpen={showCommandPalette}
          onClose={() => setShowCommandPalette(false)}
          query={activeTabState.query}
          setQuery={(val) => updateActiveTabState({ query: val })}
          isGenerating={activeTabState.isGenerating}
          handleGenerate={handleGenerate}
          recentQueries={recentQueries}
          schemaData={schemaData}
          dbUrl={configData?.db_url}
          aiProfiles={resolveProfilesList}
          activeAiProfileId={resolveActiveProfileId}
          aiModels={resolveModelsList}
          activeModelId={resolveActiveModelId}
          activeTier={resolveActiveTier}
          onAction={(actionType, payload) => {
            if (actionType === 'query') {
              updateActiveTabState({ query: payload })
              saveRecentQuery(payload)
              handleGenerate(payload)
            } else if (actionType === 'table') {
              insertTextAtCursor(payload)
              setShowCommandPalette(false)
            } else if (actionType === 'settings') {
              setShowCommandPalette(false)
              setShowSettings(true)
            } else if (actionType === 'ai_profile') {
              const profileId = String(payload || '')
              const p = resolveProfilesList.find((x: any) => x?.id === profileId)
              if (!p || !configData) return
              setShowCommandPalette(false)
              updateAiRuntime(
                {
                  active_ai_profile_id: profileId,
                  ai_provider: p.provider,
                  ai_mode: p.mode,
                  api_key: p.api_key || null,
                  relay_url: p.relay_url || null,
                  token_pool: Array.isArray(p?.pool?.tokens) ? p.pool.tokens : []
                },
                `已切换 Profile：${p.name || profileId}`
              )
            } else if (actionType === 'ai_model') {
              const modelId = String(payload || '')
              setShowCommandPalette(false)
              updateAiRuntime({ active_model_id: modelId, model_name: modelId }, `已切换模型：${modelId}`)
            } else if (actionType === 'ai_tier') {
              const tier = String(payload || '')
              setShowCommandPalette(false)
              updateAiRuntime({ active_tier: tier }, `已切换 Tier：${tier}`)
            } else if (actionType === 'ai_health') {
              setShowCommandPalette(false)
              api.getAiHealth().then((res) => {
                const ok = res?.ok ? 'OK' : 'FAILED'
                const msg = `AI Health: ${ok} · profile=${res?.active_ai_profile_id || '-'} · model=${res?.model_id || '-'} · tier=${res?.tier || '-'} · latency=${res?.latency_ms ?? '-'}ms`
                toast(msg, res?.ok ? 'success' : 'error')
              }).catch((e: unknown) => {
                toast('AI Health 检测失败：' + parseError(e).message, 'error')
              })
            }
          }}
        />

        {/* Rules Panel Overlay */}
        <AnimatePresence>
          {showRulesPanel && (
            <>
              <motion.div 
                initial={{ opacity: 0 }}
                animate={{ opacity: 1 }}
                exit={{ opacity: 0 }}
                className="absolute inset-0 bg-black/60 backdrop-blur-sm z-40"
                onClick={() => setShowRulesPanel(false)}
              />
              <motion.div 
                initial={{ x: '100%' }}
                animate={{ x: 0 }}
                exit={{ x: '100%' }}
                transition={{ type: 'spring', damping: 25, stiffness: 200 }}
                className="absolute right-0 top-0 bottom-0 w-[500px] bg-[#161b22] border-l border-[#30363d] z-50 flex flex-col shadow-2xl"
              >
                <div className="p-5 border-b border-[#30363d] flex items-center justify-between bg-[#0d1117]">
                  <div className="flex items-center gap-2">
                    <BookMarked className="w-5 h-5 text-blue-400" />
                    <h2 className="text-lg font-semibold text-white">Smart Rules</h2>
                  </div>
                  <button onClick={() => setShowRulesPanel(false)} className="text-gray-500 hover:text-white transition-colors">
                    esc
                  </button>
                </div>
                
                <div className="flex-1 overflow-y-auto p-5">
                  {rules.length === 0 ? (
                    <div className="h-full flex flex-col items-center justify-center text-gray-500">
                      <BookMarked className="w-12 h-12 mb-4 opacity-20" />
                      <p>No rules defined yet.</p>
                      <p className="text-sm mt-2 text-center max-w-[300px]">
                        When AI generates a useful query, hover over the editor and click "Save as Rule" to teach it for next time.
                      </p>
                    </div>
                  ) : (
                    <div className="space-y-4">
                      {rules.map((r, i) => (
                        <div key={i} className="bg-[#0d1117] border border-[#30363d] rounded-lg p-4 group relative">
                          <button 
                            onClick={() => handleDeleteRule(r.id)}
                            className="absolute top-3 right-3 text-gray-500 hover:text-red-400 opacity-0 group-hover:opacity-100 transition-opacity"
                          >
                            ×
                          </button>
                          <div className="flex items-center gap-2 mb-2">
                            <span className="px-2 py-0.5 bg-blue-500/10 text-blue-400 text-[10px] font-bold rounded uppercase tracking-wider">
                              {r.rule_type}
                            </span>
                            <span className="text-sm font-medium text-gray-200">{r.prompt_pattern}</span>
                          </div>
                          <div className="bg-[#161b22] p-3 rounded border border-[#30363d] text-xs text-green-400/80 font-mono overflow-x-auto">
                            <pre>{r.sql_template}</pre>
                          </div>
                          <div className="mt-2 text-[10px] text-gray-500 text-right">
                            Used {r.hit_count} times
                          </div>
                        </div>
                      ))}
                    </div>
                  )}
                </div>
              </motion.div>
            </>
          )}
        </AnimatePresence>

        {/* Dangerous SQL Confirmation Modal */}
        <AnimatePresence>
          {showVariablesModal && (
        <div className="fixed inset-0 bg-black/60 backdrop-blur-sm z-[60] flex items-center justify-center p-4">
          <div className="bg-[#161b22] border border-[#30363d] rounded-xl shadow-2xl w-full max-w-md overflow-hidden flex flex-col">
            <div className="px-6 py-4 border-b border-[#30363d] flex items-center justify-between bg-[#0d1117]">
              <h3 className="text-gray-200 font-bold text-lg">Enter Variable Values</h3>
              <button onClick={() => setShowVariablesModal(false)} className="text-gray-500 hover:text-gray-300">
                <X className="w-5 h-5" />
              </button>
            </div>
            <div className="p-6 space-y-4">
              {sqlVariables.map((v, i) => (
                <div key={v.name}>
                  <label className="block text-sm font-medium text-gray-300 mb-1">
                    :{v.name}
                  </label>
                  <input
                    type="text"
                    className="w-full bg-[#0d1117] border border-[#30363d] rounded-lg px-3 py-2 text-gray-200 focus:outline-none focus:border-blue-500 transition-colors"
                    value={v.value}
                    onChange={(e) => {
                      const newVars = [...sqlVariables];
                      newVars[i].value = e.target.value;
                      setSqlVariables(newVars);
                    }}
                    autoFocus={i === 0}
                    onKeyDown={(e) => {
                      if (e.key === 'Enter') {
                        const allFilled = sqlVariables.every(v => v.value.trim() !== '');
                        if (allFilled) {
                          let finalSql = pendingSqlWithVars;
                          sqlVariables.forEach(variable => {
                            // Only replace variables outside of quotes, but since we are doing a dumb replace here
                            // we assume the user intends to replace all occurrences.
                            const regex = new RegExp(`:${variable.name}\\b`, 'g');
                            // If value looks like a number or boolean, maybe don't quote? 
                            // But usually values are safely replaced via prepared statements. Since we are doing client side:
                            // If it's just a string replacement:
                            let val = variable.value;
                            if (isNaN(Number(val)) && val.toLowerCase() !== 'true' && val.toLowerCase() !== 'false' && val.toLowerCase() !== 'null') {
                              val = `'${val.replace(/'/g, "''")}'`;
                            }
                            finalSql = finalSql.replace(regex, val);
                          });
                          setShowVariablesModal(false);
                          handleExecute(false, finalSql);
                        }
                      }
                    }}
                  />
                </div>
              ))}
            </div>
            <div className="px-6 py-4 border-t border-[#30363d] bg-[#0d1117] flex justify-end gap-3">
              <button
                onClick={() => setShowVariablesModal(false)}
                className="px-4 py-2 text-sm font-medium text-gray-300 hover:text-white transition-colors"
              >
                Cancel
              </button>
              <button
                onClick={() => {
                  let finalSql = pendingSqlWithVars;
                  sqlVariables.forEach(variable => {
                    const regex = new RegExp(`:${variable.name}\\b`, 'g');
                    let val = variable.value;
                    if (isNaN(Number(val)) && val.toLowerCase() !== 'true' && val.toLowerCase() !== 'false' && val.toLowerCase() !== 'null') {
                      val = `'${val.replace(/'/g, "''")}'`;
                    }
                    finalSql = finalSql.replace(regex, val);
                  });
                  setShowVariablesModal(false);
                  handleExecute(false, finalSql);
                }}
                className="px-4 py-2 text-sm font-medium bg-blue-600 hover:bg-blue-700 text-white rounded-lg transition-colors flex items-center gap-2"
              >
                Execute
              </button>
            </div>
          </div>
        </div>
      )}

      {showConfirmModal && (
            <div className="fixed inset-0 bg-black/80 backdrop-blur-sm z-50 flex items-center justify-center">
              <motion.div
                initial={{ scale: 0.95, opacity: 0 }}
                animate={{ scale: 1, opacity: 1 }}
                exit={{ scale: 0.95, opacity: 0 }}
                className="bg-[#161b22] border border-red-500/50 rounded-xl shadow-2xl max-w-xl w-full mx-4 overflow-hidden"
              >
                <div className="bg-red-500/10 px-6 py-4 border-b border-red-500/20 flex items-center gap-3">
                  <div className="w-10 h-10 rounded-full bg-red-500/20 flex items-center justify-center flex-shrink-0">
                    <span className="text-red-500 text-xl">⚠️</span>
                  </div>
                  <div>
                    <h3 className="text-red-400 font-bold text-lg">高危操作警告</h3>
                    <p className="text-red-300/70 text-sm">系统检测到该 SQL 可能对数据造成不可逆的修改或删除。</p>
                  </div>
                </div>
                <div className="p-6">
                  <p className="text-gray-300 text-sm mb-3">即将执行的 SQL 语句：</p>
                  <div className="bg-[#0d1117] border border-[#30363d] rounded-lg p-4 font-mono text-red-400/90 text-sm overflow-x-auto whitespace-pre-wrap max-h-60">
                    {pendingDangerousSql}
                  </div>
                </div>
                <div className="bg-[#0d1117] px-6 py-4 flex justify-end gap-3 border-t border-[#30363d]">
                  <button
                    onClick={() => {
                      setShowConfirmModal(false)
                      setPendingDangerousSql('')
                    }}
                    className="px-4 py-2 rounded-lg text-sm font-medium text-gray-300 hover:bg-[#30363d] transition-colors"
                  >
                    取消执行 (Cancel)
                  </button>
                  <button
                    onClick={() => handleExecute(true, pendingDangerousSql)}
                    className="px-4 py-2 rounded-lg text-sm font-bold bg-red-600 hover:bg-red-500 text-white shadow-lg shadow-red-500/20 transition-all active:scale-95 flex items-center gap-2"
                  >
                    我已确认风险，强制执行
                  </button>
                </div>
              </motion.div>
            </div>
          )}
        </AnimatePresence>

        {/* Shortcuts Help Modal */}
        <AnimatePresence>
          {showHelpModal && (
            <div className="fixed inset-0 bg-black/60 backdrop-blur-sm z-50 flex items-center justify-center">
              <motion.div
                initial={{ scale: 0.95, opacity: 0, y: 10 }}
                animate={{ scale: 1, opacity: 1, y: 0 }}
                exit={{ scale: 0.95, opacity: 0, y: 10 }}
                className="bg-[#161b22] border border-[#30363d] rounded-xl shadow-2xl max-w-lg w-full mx-4 overflow-hidden"
              >
                <div className="px-6 py-4 border-b border-[#30363d] flex items-center gap-3 bg-[#0d1117]">
                  <Keyboard className="w-5 h-5 text-gray-400" />
                  <h3 className="text-gray-200 font-bold text-lg">Keyboard Shortcuts</h3>
                </div>
                <div className="p-2">
                  {[
                    { label: 'Ask AI to generate/modify SQL', win: ['Ctrl', 'K'], mac: ['⌘', 'K'] },
                    { label: 'Execute current SQL query', win: ['Ctrl', 'Enter'], mac: ['⌘', 'Enter'] },
                    { label: 'Format SQL code', win: ['Shift', 'Alt', 'F'], mac: ['⇧', '⌥', 'F'] },
                    { label: 'Save current SQL as Smart Rule', win: ['Ctrl', 'S'], mac: ['⌘', 'S'] },
                    { label: 'Close modals / Cancel', win: ['Esc'], mac: ['Esc'] },
                    { label: 'Show this help menu', win: ['Ctrl', '/'], mac: ['⌘', '/'] },
                  ].map((item, idx) => {
                    const isMac = navigator.platform.toUpperCase().indexOf('MAC') >= 0;
                    const keys = isMac ? item.mac : item.win;
                    return (
                      <div key={idx} className="flex items-center justify-between px-4 py-3 hover:bg-[#21262d] rounded-lg transition-colors group">
                        <span className="text-sm text-gray-300 group-hover:text-gray-100 transition-colors">{item.label}</span>
                        <div className="flex gap-1.5">
                          {keys.map((k, i) => (
                            <kbd key={i} className="px-2 py-1 bg-[#0d1117] border border-[#30363d] rounded text-xs font-mono text-gray-400 shadow-sm min-w-[24px] text-center">
                              {k}
                            </kbd>
                          ))}
                        </div>
                      </div>
                    );
                  })}
                </div>
                <div className="bg-[#0d1117] px-6 py-3 flex justify-end border-t border-[#30363d]">
                  <button
                    onClick={() => setShowHelpModal(false)}
                    className="px-4 py-1.5 rounded-lg text-sm font-medium text-gray-300 hover:bg-[#30363d] transition-colors"
                  >
                    Close
                  </button>
                </div>
              </motion.div>
            </div>
          )}
        </AnimatePresence>

        {/* Settings Panel */}
        <AnimatePresence>
          {showSettings && (
            <SettingsPanel 
              onClose={() => setShowSettings(false)} 
              onPolicyChange={loadRulesAndPolicy} 
              onConfigChange={refreshConfigOnly}
            />
          )}
        </AnimatePresence>

        {contextMenu && (
          <TableContextMenu 
            x={contextMenu.x}
            y={contextMenu.y}
            table={contextMenu.table}
            onClose={() => setContextMenu(null)}
            onGenerateMockData={(table) => {
              setWizardConfig({
                isOpen: true,
                title: `Generate Mock Data - ${table.table_name}`,
                type: 'mock-data',
                payload: { tableName: table.table_name, columns: table.columns }
              });
            }}
            onImport={(table) => {
              setWizardConfig({
                isOpen: true,
                title: `Import Wizard - ${table.table_name}`,
                type: 'import',
                payload: { tableName: table.table_name, columns: table.columns }
              });
            }}
            onExport={(table) => {
              setWizardConfig({
                isOpen: true,
                title: `Export Data - ${table.table_name}`,
                type: 'export',
                payload: { tableName: table.table_name }
              });
            }}
          />
        )}

        <WizardModal 
          isOpen={wizardConfig.isOpen}
          title={wizardConfig.title}
          type={wizardConfig.type}
          payload={wizardConfig.payload}
          onClose={() => setWizardConfig({ ...wizardConfig, isOpen: false })}
        />

      </div>
    </div>
  )
}

export default App
