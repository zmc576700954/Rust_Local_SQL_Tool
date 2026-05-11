import React, { useState, useEffect, useRef, Suspense, useCallback, useMemo } from 'react'
import { Database, Settings, BookMarked, Keyboard, X } from 'lucide-react'
import { motion, AnimatePresence } from 'framer-motion'
import { format as formatSql } from 'sql-formatter'
import { Onboarding } from './components/Onboarding'
import { SettingsPanel } from './components/SettingsPanel'
import { useToast } from './components/Toast'
import { SkeletonLoader } from './components/Skeleton'
import { CommandPalette } from './components/CommandPalette'
import { QueryEditorActionPanel } from './components/QueryEditorActionPanel'
import { QueryResultsPanel } from './components/QueryResultsPanel'
import { Tabs, type TabItem } from './components/Tabs'
import { TableContextMenu } from './components/TableContextMenu'
import { ToolsNav } from './components/ToolsNav'
import { WizardModal } from './components/WizardModal'
import { GoLiveReportsTab } from './components/GoLiveReportsTab'
import { GoLiveAuditTab } from './components/GoLiveAuditTab'
import { AdvancedToolsHub } from './components/AdvancedToolsHub'
import { PerfDiagnosticsPanel } from './components/PerfDiagnosticsPanel'
import { SqlHistory } from './components/SqlHistory'
import { AiTrainingPanel } from './components/AiTrainingPanel'
import { DbExplorerSidebar } from './components/DbExplorerSidebar'
import { api } from './api'
import { runAiExplain, runAiOptimize, runExplainErrorWithAi, runFixWithAi, runGenerateSql } from './queryAiActions'
import { getStatementKind, getStatementLabel, isPotentiallyDangerousSql, splitSqlStatements } from './sqlStatements'
  
import { parseError, formatErr, sanitizeForLog } from './utils'
import type { AppError } from './utils'
import { dbTypeDisplayName } from './utils/dbCapabilities'
import { useAutoI18nDom } from './i18n'
import { tr } from './i18n'

import * as monaco from 'monaco-editor';
import type { SchemaResponse, ConfigData, QueryExecutionResult, QueryResultCompareReport, AiRule, DbConnection, TableWithDetails, ColumnInfo, MonacoEditor, Monaco, KnowledgeItem, SavedSqlBookmark, QueryErrorInsight } from './types';

const ExecutionPlan = React.lazy(() => import('./components/ExecutionPlan').then(m => ({ default: m.ExecutionPlan })));
const SessionInfoPanel = React.lazy(() => import('./components/SessionInfoPanel').then(m => ({ default: m.SessionInfoPanel })));
const QueryBuilder = React.lazy(() => import('./components/QueryBuilder').then(m => ({ default: m.QueryBuilder })));
const TableWorkspace = React.lazy(() => import('./components/TableWorkspace').then(m => ({ default: m.TableWorkspace })));
const QUERY_CHUNK_SIZE = 200
const SQL_BOOKMARKS_KEY = 'sql_workbench_bookmarks_v1'
const MAX_SQL_BOOKMARKS = 50

const buildDefaultBookmarkTitle = (sql: string) => {
  const firstLine = sql
    .split(/\r?\n/)
    .map((line) => line.trim())
    .find(Boolean)

  if (!firstLine) return 'SQL Bookmark'
  return firstLine.length > 56 ? `${firstLine.slice(0, 56)}…` : firstLine
}

const normalizeSavedBookmarks = (raw: unknown): SavedSqlBookmark[] => {
  if (!Array.isArray(raw)) return []

  return raw
    .filter((item): item is SavedSqlBookmark => Boolean(
      item
      && typeof item === 'object'
      && typeof (item as SavedSqlBookmark).id === 'string'
      && typeof (item as SavedSqlBookmark).title === 'string'
      && typeof (item as SavedSqlBookmark).sql === 'string'
    ))
    .map((item) => ({
      ...item,
      description: item.description || null,
      db_id: item.db_id || null,
      db_label: item.db_label || null,
      created_at: typeof item.created_at === 'number' ? item.created_at : Date.now(),
      updated_at: typeof item.updated_at === 'number' ? item.updated_at : Date.now(),
    }))
    .slice(0, MAX_SQL_BOOKMARKS)
}

function createCancelToken(): string {
  if (typeof crypto !== 'undefined' && typeof crypto.randomUUID === 'function') {
    return crypto.randomUUID()
  }
  return `cancel-${Date.now()}-${Math.random().toString(16).slice(2)}`
}

const DEFAULT_QUERY_SQL = '-- Generated SQL will appear here\n'

const stringifyJsonArtifact = (value: unknown) =>
  JSON.stringify(value, (_key, innerValue) => typeof innerValue === 'bigint' ? innerValue.toString() : innerValue, 2)

const normalizeComparableValue = (value: unknown): unknown => {
  if (typeof value === 'bigint') return value.toString()
  if (Array.isArray(value)) return value.map((item) => normalizeComparableValue(item))
  if (value && typeof value === 'object') {
    return Object.keys(value as Record<string, unknown>)
      .sort()
      .reduce<Record<string, unknown>>((acc, key) => {
        acc[key] = normalizeComparableValue((value as Record<string, unknown>)[key])
        return acc
      }, {})
  }
  return value
}

const serializeComparableRow = (row: unknown) => JSON.stringify(normalizeComparableValue(row))

const cloneQueryExecutionResultSnapshot = (result: QueryExecutionResult): QueryExecutionResult =>
  JSON.parse(stringifyJsonArtifact(result)) as QueryExecutionResult

const canCompareQueryResult = (result: QueryExecutionResult | null | undefined): result is QueryExecutionResult => {
  if (!result || result.status !== 'success' || result.error) return false
  return Array.isArray(result.rows) && (result.rows.length > 0 || (result.columns?.length ?? 0) > 0)
}

const buildQueryResultCompareReport = (
  baseline: QueryExecutionResult | null | undefined,
  current: QueryExecutionResult | null | undefined
): QueryResultCompareReport | null => {
  if (!canCompareQueryResult(baseline) || !canCompareQueryResult(current)) {
    return null
  }

  const baselineCounts = new Map<string, { count: number; row: any }>()
  for (const row of baseline.rows) {
    const key = serializeComparableRow(row)
    const existing = baselineCounts.get(key)
    if (existing) {
      existing.count += 1
    } else {
      baselineCounts.set(key, { count: 1, row })
    }
  }

  let unchangedCount = 0
  const addedRows: any[] = []
  for (const row of current.rows) {
    const key = serializeComparableRow(row)
    const existing = baselineCounts.get(key)
    if (existing && existing.count > 0) {
      existing.count -= 1
      unchangedCount += 1
    } else {
      addedRows.push(row)
    }
  }

  const removedRows: any[] = []
  baselineCounts.forEach(({ count, row }) => {
    for (let index = 0; index < count; index += 1) {
      removedRows.push(row)
    }
  })

  return {
    baseline_statement_label: baseline.statement_label || null,
    current_statement_label: current.statement_label || null,
    baseline_source_sql: baseline.source_sql || null,
    current_source_sql: current.source_sql || null,
    baseline_execution_time_ms: baseline.execution_time_ms,
    current_execution_time_ms: current.execution_time_ms,
    compared_at: Date.now(),
    summary: {
      baseline_row_count: baseline.rows.length,
      current_row_count: current.rows.length,
      added_count: addedRows.length,
      removed_count: removedRows.length,
      unchanged_count: unchangedCount,
    },
    added_rows: addedRows,
    removed_rows: removedRows,
  }
}

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
  const createDefaultQueryTabState = (sql: string = DEFAULT_QUERY_SQL) => ({
    sql,
    query: '',
    lastQuery: '',
    isGenerating: false,
    isExplainingError: false,
    isExecuting: false,
    executeResult: null as QueryExecutionResult | null,
    executeResults: [] as QueryExecutionResult[],
    activeResultIndex: 0,
    isLoadingMoreResults: false,
    isCancelingExecution: false,
    currentCancelToken: null,
    executingSql: null,
    executionDbId: null,
    transactionMode: 'auto' as const,
    transactionId: null as string | null,
    transactionState: 'idle' as const,
    errorObj: null,
    lastExplanation: null,
    lastErrorInsight: null as QueryErrorInsight | null,
    compareBaselineResult: null as QueryExecutionResult | null,
    compareBaselineCapturedAt: null as number | null,
    resultsView: 'table' as const,
    chatHistory: []
  })
  
  // Per-tab state
  const [tabStates, setTabStates] = useState<Record<string, {
    sql: string;
    query: string;
    lastQuery: string;
    isGenerating: boolean;
    isExplainingError?: boolean;
    isExecuting: boolean;
    executeResult: QueryExecutionResult | null;
    executeResults: QueryExecutionResult[];
    activeResultIndex: number;
    isLoadingMoreResults?: boolean;
    isCancelingExecution?: boolean;
    currentCancelToken?: string | null;
    executingSql?: string | null;
    executionDbId?: string | null;
    transactionMode: 'auto' | 'manual';
    transactionId?: string | null;
    transactionState?: 'idle' | 'active' | 'committing' | 'rolling_back';
    errorObj: AppError | null;
    lastExplanation: string | null;
    lastErrorInsight?: QueryErrorInsight | null;
    compareBaselineResult?: QueryExecutionResult | null;
    compareBaselineCapturedAt?: number | null;
    resultsView: 'table' | 'chart';
    chatHistory: any[];
  }>>({
    'query-1': createDefaultQueryTabState()
  })

  const activeTabState = tabStates[activeTabId] || createDefaultQueryTabState();

  const updateActiveTabState = (patch: Partial<typeof activeTabState>) => {
    setTabStates(prev => ({
      ...prev,
      [activeTabId]: {
        ...(prev[activeTabId] || createDefaultQueryTabState()),
        ...patch
      }
    }));
  };

  const normalizeExecuteResult = useCallback((result: any, sourceSql: string, statementIndex: number = 0): QueryExecutionResult => {
    const rows = Array.isArray(result?.rows) ? result.rows : []
    return {
      columns: Array.isArray(result?.columns) ? result.columns : (rows[0] ? Object.keys(rows[0]) : []),
      rows,
      execution_time_ms: typeof result?.execution_time_ms === 'number' ? result.execution_time_ms : undefined,
      row_count: typeof result?.row_count === 'number' ? result.row_count : rows.length,
      affected_rows: typeof result?.affected_rows === 'number' ? result.affected_rows : undefined,
      has_more: Boolean(result?.has_more),
      next_offset: typeof result?.next_offset === 'number' ? result.next_offset : null,
      chunk_offset: typeof result?.chunk_offset === 'number' ? result.chunk_offset : 0,
      chunk_size: typeof result?.chunk_size === 'number' ? result.chunk_size : undefined,
      preview_cap: typeof result?.preview_cap === 'number' ? result.preview_cap : null,
      truncated: Boolean(result?.truncated),
      source_sql: sourceSql,
      statement_index: statementIndex,
      statement_label: getStatementLabel(sourceSql, statementIndex),
      statement_kind: getStatementKind(sourceSql),
      status: 'success',
      error: null,
    }
  }, [])

  const buildFailedExecuteResult = useCallback((sourceSql: string, error: AppError, statementIndex: number = 0, status: 'error' | 'canceled' = 'error'): QueryExecutionResult => ({
    columns: [],
    rows: [],
    row_count: 0,
    affected_rows: 0,
    chunk_offset: 0,
    has_more: false,
    next_offset: null,
    preview_cap: null,
    truncated: false,
    source_sql: sourceSql,
    statement_index: statementIndex,
    statement_label: getStatementLabel(sourceSql, statementIndex),
    statement_kind: getStatementKind(sourceSql),
    status,
    error,
  }), [])
  
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
  const [savedBookmarks, setSavedBookmarks] = useState<SavedSqlBookmark[]>([])
  const [sqlSnippets, setSqlSnippets] = useState<KnowledgeItem[]>([])
  const [isRefreshingSchema, setIsRefreshingSchema] = useState(false)
  const [historyVersion, setHistoryVersion] = useState(0)

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

  useEffect(() => {
    const raw = window.localStorage.getItem(SQL_BOOKMARKS_KEY)
    if (!raw) return
    try {
      setSavedBookmarks(normalizeSavedBookmarks(JSON.parse(raw)))
    } catch {
      // ignore invalid bookmark cache
    }
  }, [])

  const saveRecentQuery = (q: string) => {
    if (!q.trim()) return
    const newHistory = [q, ...recentQueries.filter(item => item !== q)].slice(0, 10)
    setRecentQueries(newHistory)
    localStorage.setItem('recent_queries', JSON.stringify(newHistory))
  }

  const refreshSqlSnippets = useCallback(async () => {
    try {
      const activeDbId = configData?.active_db_id ? String(configData.active_db_id) : undefined
      const knowledge = await api.getKnowledge(activeDbId)
      const nextSnippets = Array.isArray(knowledge)
        ? knowledge
            .filter((item: KnowledgeItem) => {
              if (item?.knowledge_type !== 'sql' || !item?.content?.trim()) return false
              if (!item.db_connection_id) return true
              return item.db_connection_id === activeDbId
            })
            .sort((a, b) => {
              const goldenDelta = Number(Boolean(b.is_golden)) - Number(Boolean(a.is_golden))
              if (goldenDelta !== 0) return goldenDelta
              return (b.updated_at || 0) - (a.updated_at || 0)
            })
        : []
      setSqlSnippets(nextSnippets)
    } catch (e) {
      console.error('Failed to load SQL snippets', sanitizeForLog(e))
    }
  }, [configData?.active_db_id])

  useEffect(() => {
    if (!showCommandPalette && sidebarTab !== 'smart_snippets') return
    void refreshSqlSnippets()
  }, [refreshSqlSnippets, showCommandPalette, sidebarTab])

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

  const openSqlInQueryTab = useCallback((sql: string) => {
    const currentTabs = tabsRef.current
    const currentActiveTabId = activeTabIdRef.current
    const activeQueryTab = currentTabs.find((tab) => tab.id === currentActiveTabId && tab.type === 'query')
    const targetQueryTab = activeQueryTab || currentTabs.find((tab) => tab.type === 'query')

    if (targetQueryTab) {
      setTabStates((prev) => ({
        ...prev,
        [targetQueryTab.id]: {
          ...(prev[targetQueryTab.id] || createDefaultQueryTabState()),
          sql,
          executeResult: null,
          executeResults: [],
          activeResultIndex: 0,
          isExplainingError: false,
          errorObj: null,
          lastErrorInsight: null,
        }
      }))
      setActiveTabId(targetQueryTab.id)
      return targetQueryTab.id
    }

    const newTabId = `query-${Date.now()}`
    const newTab: TabItem = {
      id: newTabId,
      type: 'query',
      title: `Query ${currentTabs.filter((tab) => tab.type === 'query').length + 1}`
    }
    setTabs((prev) => [...prev, newTab])
    setTabStates((prev) => ({
      ...prev,
      [newTabId]: createDefaultQueryTabState(sql)
    }))
    setActiveTabId(newTabId)
    return newTabId
  }, [])

  const handleSaveBookmark = useCallback(() => {
    const currentSql = sqlRef.current?.trim() ? sqlRef.current : activeTabState.sql
    if (!currentSql.trim()) {
      toast(tr('当前没有可保存的 SQL', 'There is no SQL to bookmark'), 'error')
      return
    }

    const defaultTitle = buildDefaultBookmarkTitle(currentSql)
    const promptedTitle = window.prompt(tr('输入书签标题', 'Bookmark title'), defaultTitle)
    if (promptedTitle === null) return

    const title = promptedTitle.trim() || defaultTitle
    const activeDbId = configData?.active_db_id ? String(configData.active_db_id) : null
    const activeConnection = configData?.db_connections?.find((conn: DbConnection) => String(conn.id) === activeDbId)
    const now = Date.now()
    const existing = savedBookmarks.find((item) => item.title === title && (item.db_id || null) === activeDbId)

    const nextBookmark: SavedSqlBookmark = existing
      ? {
          ...existing,
          sql: currentSql,
          db_id: activeDbId,
          db_label: activeConnection?.name || activeConnection?.id || existing.db_label || null,
          updated_at: now,
        }
      : {
          id: `bookmark-${now}`,
          title,
          sql: currentSql,
          description: null,
          db_id: activeDbId,
          db_label: activeConnection?.name || activeConnection?.id || null,
          created_at: now,
          updated_at: now,
        }

    const next = [
      nextBookmark,
      ...savedBookmarks.filter((item) => item.id !== nextBookmark.id),
    ].slice(0, MAX_SQL_BOOKMARKS)

    setSavedBookmarks(next)
    window.localStorage.setItem(SQL_BOOKMARKS_KEY, JSON.stringify(next))

    toast(
      existing
        ? tr('书签已更新', 'Bookmark updated')
        : tr('SQL 已保存为书签', 'SQL bookmarked'),
      'success'
    )
  }, [activeTabState.sql, configData, savedBookmarks, toast])

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
    await runAiOptimize({
      currentSql: sqlRef.current,
      updateActiveTabState,
      toast,
    })
  }

  const handleAIExplain = async () => {
    await runAiExplain({
      currentSql: sqlRef.current,
      updateActiveTabState,
      toast,
    })
  }

  const handleFixWithAI = async () => {
    const activeResult = activeTabState.executeResults?.[activeTabState.activeResultIndex] || activeTabState.executeResult
    await runFixWithAi({
      currentSql: activeResult?.source_sql || sqlRef.current,
      errorObj: activeTabState.errorObj || activeResult?.error || null,
      statementLabel: activeResult?.statement_label || null,
      statementKind: activeResult?.statement_kind || null,
      updateActiveTabState,
      toast,
    })
  }

  const handleExplainErrorWithAI = async () => {
    const activeResult = activeTabState.executeResults?.[activeTabState.activeResultIndex] || activeTabState.executeResult
    await runExplainErrorWithAi({
      currentSql: activeResult?.source_sql || sqlRef.current,
      errorObj: activeTabState.errorObj || activeResult?.error || null,
      statementLabel: activeResult?.statement_label || null,
      statementKind: activeResult?.statement_kind || null,
      updateActiveTabState,
      toast,
    })
  }

  const handleApplyErrorSuggestion = () => {
    const suggestedSql = activeTabState.lastErrorInsight?.fixed_sql
    if (!suggestedSql || !suggestedSql.trim()) return
    if (errorDecorationsRef.current) {
      errorDecorationsRef.current.clear()
      errorDecorationsRef.current = null
    }
    updateActiveTabState({ sql: suggestedSql })
    toast('Applied AI SQL suggestion to the editor', 'success')
  }

  const handleOpenSessionInfo = useCallback(() => {
    const sourceQueryTabId = activeTabIdRef.current
    const sourceTabState = tabStates[sourceQueryTabId] || activeTabState
    const dbId = sourceTabState.executionDbId || configData?.active_db_id || null
    const currentTabs = tabsRef.current
    const connection = configData?.db_connections?.find((item: DbConnection) => String(item.id) === String(dbId || ''))
    const dbLabel = connection?.name || connection?.id || null
    const tabTitle = dbLabel ? `${dbLabel} Session` : 'Session Info'

    const existingTab = currentTabs.find((tab) =>
      tab.type === 'session-info'
      && tab.payload?.sourceQueryTabId === sourceQueryTabId
      && String(tab.payload?.dbId || '') === String(dbId || '')
    )
    if (existingTab) {
      setActiveTabId(existingTab.id)
      return
    }

    const newTabId = `session-info-${Date.now()}`
    setTabs((prev) => [
      ...prev,
      {
        id: newTabId,
        type: 'session-info',
        title: tabTitle,
        payload: {
          dbId,
          dbLabel,
          dbType,
          sourceQueryTabId,
        },
      },
    ])
    setActiveTabId(newTabId)
  }, [activeTabState, configData, dbType, tabStates])

  const rollbackTransactionSession = useCallback(async (
    tabStateToClose?: typeof activeTabState,
    options: { silent?: boolean } = {}
  ) => {
    if (
      !tabStateToClose
      || tabStateToClose.transactionMode !== 'manual'
      || tabStateToClose.transactionState !== 'active'
      || !tabStateToClose.transactionId
    ) {
      return true
    }

    try {
      await api.executeTransactionAction(
        'rollback',
        tabStateToClose.transactionId,
        tabStateToClose.executionDbId || configData?.active_db_id
      )
      if (!options.silent) {
        toast('Transaction rolled back', 'success')
      }
      return true
    } catch (e: unknown) {
      if (!options.silent) {
        toast(`Rollback failed: ${parseError(e).message}`, 'error')
      }
      return false
    }
  }, [configData?.active_db_id, toast])

  const handleTabClose = (id: string) => {
    void rollbackTransactionSession(tabStates[id], { silent: true })
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
    tabs
      .filter(t => t.id !== id)
      .forEach(tab => void rollbackTransactionSession(tabStates[tab.id], { silent: true }))
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
    tabs.forEach(tab => void rollbackTransactionSession(tabStates[tab.id], { silent: true }))
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

  const handleTransactionModeChange = (tabId: string, mode: 'auto' | 'manual') => {
    const currentTabState = tabStates[tabId] || createDefaultQueryTabState()
    if (mode === 'auto' && currentTabState.transactionState === 'active') {
      toast('Commit or rollback the current transaction first', 'error')
      return
    }

    setTabStates(prev => ({
      ...prev,
      [tabId]: {
        ...(prev[tabId] || currentTabState),
        transactionMode: mode,
        transactionId: mode === 'manual' ? (currentTabState.transactionId || createCancelToken()) : null,
        transactionState: mode === 'manual'
          ? (currentTabState.transactionState === 'active' ? 'active' : 'idle')
          : 'idle',
      }
    }))
  }

  const handleTransactionAction = async (tabId: string, action: 'commit' | 'rollback') => {
    const currentTabState = tabStates[tabId] || createDefaultQueryTabState()
    const transactionId = currentTabState.transactionId
    if (currentTabState.transactionMode !== 'manual' || !transactionId) {
      return
    }
    if (currentTabState.transactionState !== 'active') {
      toast(`No active transaction to ${action}`, 'error')
      return
    }

    setTabStates(prev => ({
      ...prev,
      [tabId]: {
        ...(prev[tabId] || currentTabState),
        transactionState: action === 'commit' ? 'committing' : 'rolling_back',
      }
    }))

    try {
      await api.executeTransactionAction(
        action,
        transactionId,
        currentTabState.executionDbId || configData?.active_db_id
      )
      setTabStates(prev => ({
        ...prev,
        [tabId]: {
          ...(prev[tabId] || currentTabState),
          transactionState: 'idle',
          errorObj: null,
          currentCancelToken: null,
          executingSql: null,
        }
      }))
      if (action === 'commit') {
        api.getSchema(currentTabState.executionDbId || configData?.active_db_id).then(setSchemaData).catch(console.error)
      }
      toast(action === 'commit' ? 'Transaction committed' : 'Transaction rolled back', 'success')
    } catch (e: unknown) {
      setTabStates(prev => ({
        ...prev,
        [tabId]: {
          ...(prev[tabId] || currentTabState),
          transactionState: 'active',
        }
      }))
      toast(`${action === 'commit' ? 'Commit' : 'Rollback'} failed: ${parseError(e).message}`, 'error')
    }
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
    await runGenerateSql({
      overrideQuery,
      activeTabState,
      updateActiveTabState,
      setShowCommandPalette,
      setShowOnboarding,
    })
  }


  const handleExecute = async (force: boolean = false, overrideSql?: string, overrideTabId?: string) => {
    const executingTabId = overrideTabId || activeTabIdRef.current
    const fallbackTabState = tabStates[executingTabId]
      || (overrideTabId ? createDefaultQueryTabState(overrideSql || DEFAULT_QUERY_SQL) : activeTabState)
    const patchExecutingTabState = (patch: Partial<typeof activeTabState>) => {
      setTabStates(prev => ({
        ...prev,
        [executingTabId]: {
          ...(prev[executingTabId] || fallbackTabState),
          ...patch,
        }
      }))
    }
    let currentSql = overrideSql || sqlRef.current

    if (!overrideSql) {
      const activeEditor = editorRefs.current[executingTabId];
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

    const statements = splitSqlStatements(currentSql)
    const hasMultipleStatements = statements.length > 1

    if (!force && statements.some((statement) => isPotentiallyDangerousSql(statement))) {
      setPendingDangerousSql(currentSql)
      setShowConfirmModal(true)
      return
    }

    const cancelToken = createCancelToken()
    const executionDbId = configData?.active_db_id
    const transactionMode = fallbackTabState.transactionMode || 'auto'
    const transactionId = transactionMode === 'manual'
      ? (fallbackTabState.transactionId || createCancelToken())
      : null
    patchExecutingTabState({
      isExecuting: true,
      isCancelingExecution: false,
      currentCancelToken: cancelToken,
      executingSql: currentSql,
      executionDbId: executionDbId || null,
      transactionId,
      transactionState: transactionMode === 'manual'
        ? (fallbackTabState.transactionState === 'active' ? 'active' : fallbackTabState.transactionState === 'idle' ? 'idle' : fallbackTabState.transactionState)
        : 'idle',
      isExplainingError: false,
      errorObj: null,
      lastErrorInsight: null,
      executeResult: null,
      executeResults: [],
      activeResultIndex: 0,
      isLoadingMoreResults: false,
    })
    
    try {
      const collectedResults: QueryExecutionResult[] = []
      for (const [statementIndex, statementSql] of statements.entries()) {
        try {
          const result = await api.executeSql(statementSql, force, executionDbId, cancelToken, transactionId || undefined)
          collectedResults.push(normalizeExecuteResult(result, statementSql, statementIndex))
        } catch (e: unknown) {
          const err = parseError(e)
          const canceledText = `${err.title} ${err.message}`.toLowerCase()
          const isCanceled = canceledText.includes('canceled') || canceledText.includes('cancelled')

          if (!hasMultipleStatements) {
            throw e
          }

          collectedResults.push(buildFailedExecuteResult(
            statementSql,
            err,
            statementIndex,
            isCanceled ? 'canceled' : 'error'
          ))
          break
        }
      }

      const nextExecuteResult = collectedResults[0] || null
      patchExecutingTabState({
        executeResult: nextExecuteResult,
        executeResults: collectedResults,
        activeResultIndex: 0,
        transactionId,
        transactionState: transactionMode === 'manual' ? 'active' : 'idle',
        isLoadingMoreResults: false,
        errorObj: null,
      })
      setShowConfirmModal(false)
      setHistoryVersion(prev => prev + 1)

      if (/\b(CREATE|ALTER|DROP|TRUNCATE|RENAME|GRANT|REVOKE)\b/i.test(currentSql)) {
        api.getSchema(executionDbId).then(setSchemaData).catch(console.error)
      }
    } catch (e: unknown) {
      const err = parseError(e)
      const canceledText = `${err.title} ${err.message}`.toLowerCase()
      const isCanceled = canceledText.includes('canceled') || canceledText.includes('cancelled')
      if (err.title.includes('Dangerous SQL')) {
        setPendingDangerousSql(currentSql)
        setShowConfirmModal(true)
      } else if (isCanceled) {
        patchExecutingTabState({
          errorObj: null,
          executeResult: null,
          executeResults: [],
          activeResultIndex: 0,
          transactionId,
          transactionState: transactionMode === 'manual' ? 'active' : 'idle',
        })
        toast('Query canceled', 'info')
      } else {
        patchExecutingTabState({
          errorObj: err,
          executeResult: null,
          executeResults: [],
          activeResultIndex: 0,
          transactionId,
          transactionState: transactionMode === 'manual' ? 'active' : 'idle',
        })
        const match = err.message.match(/at line (\d+)/i);
        if (match && match[1]) {
          const lineNum = parseInt(match[1], 10);
          const activeEditor = editorRefs.current[executingTabId];
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
      patchExecutingTabState({
        isExecuting: false,
        isCancelingExecution: false,
        currentCancelToken: null,
        executingSql: null,
      })
    }
  }

  const handleCancelExecution = async (tabId: string) => {
    const tabState = tabStates[tabId]
    const cancelToken = tabState?.currentCancelToken
    if (!tabState?.isExecuting || !cancelToken) {
      return
    }
    const executionDbId = tabState?.executionDbId || configData?.active_db_id

    setTabStates(prev => ({
      ...prev,
      [tabId]: {
        ...(prev[tabId] || tabState),
        isCancelingExecution: true,
      }
    }))

    try {
      const result = await api.cancelExecution(cancelToken, executionDbId)
      if (!result?.canceled) {
        setTabStates(prev => ({
          ...prev,
          [tabId]: {
            ...(prev[tabId] || tabState),
            isCancelingExecution: false,
          }
        }))
      }
    } catch (e: unknown) {
      setTabStates(prev => ({
        ...prev,
        [tabId]: {
          ...(prev[tabId] || tabState),
          isCancelingExecution: false,
        }
      }))
      toast(`Cancel failed: ${parseError(e).message}`, 'error')
    }
  }

  const handleLoadMoreResults = async (tabId: string) => {
    const tabState = tabStates[tabId]
    const activeResultIndex = tabState?.activeResultIndex || 0
    const executeResult = tabState?.executeResults?.[activeResultIndex] || tabState?.executeResult
    if (!executeResult?.has_more || executeResult.next_offset === null || executeResult.next_offset === undefined || !executeResult.source_sql) {
      return
    }
    const executionDbId = tabState?.executionDbId || configData?.active_db_id

    setTabStates(prev => ({
      ...prev,
      [tabId]: {
        ...(prev[tabId] || tabState),
        isLoadingMoreResults: true,
      }
    }))

    try {
      const nextResult = await api.executeSqlChunk(
        executeResult.source_sql,
        executeResult.next_offset,
        executeResult.chunk_size || QUERY_CHUNK_SIZE,
        executionDbId,
        tabState?.transactionMode === 'manual' && tabState?.transactionState === 'active'
          ? (tabState.transactionId || undefined)
          : undefined
      )
      const normalizedNext = normalizeExecuteResult(nextResult, executeResult.source_sql)

      setTabStates(prev => {
        const currentTabState = prev[tabId]
        const currentResults = currentTabState?.executeResults || []
        const currentResult = currentResults[activeResultIndex] || currentTabState?.executeResult
        if (!currentTabState || !currentResult) {
          return prev
        }

        const mergedRows = [...(currentResult.rows || []), ...normalizedNext.rows]
        const mergedResult: QueryExecutionResult = {
          ...normalizedNext,
          rows: mergedRows,
          row_count: mergedRows.length,
          columns: (currentResult.columns && currentResult.columns.length > 0)
            ? currentResult.columns
            : normalizedNext.columns,
          execution_time_ms: currentResult.execution_time_ms ?? normalizedNext.execution_time_ms,
          affected_rows: currentResult.affected_rows ?? normalizedNext.affected_rows,
          source_sql: executeResult.source_sql,
          statement_index: currentResult.statement_index,
          statement_label: currentResult.statement_label,
          statement_kind: currentResult.statement_kind,
          status: currentResult.status,
          error: currentResult.error,
        }
        const nextResults = currentResults.length > 0
          ? currentResults.map((item, index) => index === activeResultIndex ? mergedResult : item)
          : [mergedResult]
        return {
          ...prev,
          [tabId]: {
            ...currentTabState,
            isLoadingMoreResults: false,
            executeResult: mergedResult,
            executeResults: nextResults,
          }
        }
      })
    } catch (e: unknown) {
      setTabStates(prev => ({
        ...prev,
        [tabId]: {
          ...(prev[tabId] || tabState),
          isLoadingMoreResults: false,
        }
      }))
      toast(`Load more failed: ${parseError(e).message}`, 'error')
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

  const getActiveQueryResult = (tabStateToRead = activeTabState) => {
    const activeResultIndex = tabStateToRead.activeResultIndex || 0
    return tabStateToRead.executeResults?.[activeResultIndex] || tabStateToRead.executeResult || null
  }

  const downloadTextArtifact = (content: string, mimeType: string, filename: string, addUtf8Bom: boolean = false) => {
    const payload = addUtf8Bom ? '\uFEFF' + content : content
    const blob = new Blob([payload], { type: mimeType })
    const url = URL.createObjectURL(blob)
    const link = document.createElement('a')
    link.href = url
    link.setAttribute('download', filename)
    document.body.appendChild(link)
    link.click()
    document.body.removeChild(link)
    URL.revokeObjectURL(url)
  }

  const handleSetCompareBaseline = () => {
    const executeResult = getActiveQueryResult()
    if (!canCompareQueryResult(executeResult)) {
      toast('Only successful row-based results can be saved as a baseline', 'error')
      return
    }

    updateActiveTabState({
      compareBaselineResult: cloneQueryExecutionResultSnapshot(executeResult),
      compareBaselineCapturedAt: Date.now(),
    })
    toast('Saved current result as compare baseline', 'success')
  }

  const handleClearCompareBaseline = () => {
    if (!activeTabState.compareBaselineResult) return
    updateActiveTabState({
      compareBaselineResult: null,
      compareBaselineCapturedAt: null,
    })
    toast('Cleared compare baseline', 'success')
  }

  const buildActiveCompareReport = () =>
    buildQueryResultCompareReport(activeTabState.compareBaselineResult, getActiveQueryResult())

  const handleCopyCompareJson = async () => {
    const compareReport = buildActiveCompareReport()
    if (!compareReport) {
      toast('No comparable baseline/result pair is available yet', 'error')
      return
    }

    try {
      await navigator.clipboard.writeText(stringifyJsonArtifact(compareReport))
      toast('Compare report copied to clipboard', 'success')
    } catch {
      toast('Failed to copy compare report', 'error')
    }
  }

  const handleDownloadCompareJson = () => {
    const compareReport = buildActiveCompareReport()
    if (!compareReport) {
      toast('No comparable baseline/result pair is available yet', 'error')
      return
    }

    downloadTextArtifact(
      stringifyJsonArtifact(compareReport),
      'application/json;charset=utf-8;',
      `query_compare_${new Date().toISOString().replace(/[:.]/g, '-')}.json`
    )
  }

  const formatResultExportValue = (value: unknown) => {
    if (value === null || value === undefined) return ''
    if (typeof value === 'object') return stringifyJsonArtifact(value)
    return String(value)
  }

  const downloadSql = () => {
    const executeResult = getActiveQueryResult()
    if (!executeResult?.rows || executeResult.rows.length === 0) return;
    
    const rows = executeResult.rows;
    const headers = Object.keys(rows[0]);
    const tableName = 'query_result'; // Default for custom queries

    const sqlContent = rows.map((row: Record<string, unknown>) => {
      const values = headers.map(h => {
        const val = row[h];
        if (val === null || val === undefined) return 'NULL';
        if (typeof val === 'number' || typeof val === 'boolean') return String(val);
        // Escape single quotes for SQL string
        return `'${formatResultExportValue(val).replace(/'/g, "''")}'`;
      });
      return `INSERT INTO \`${tableName}\` (${headers.map(h => `\`${h}\``).join(', ')}) VALUES (${values.join(', ')});`;
    }).join('\n');

    downloadTextArtifact(
      sqlContent,
      'application/sql;charset=utf-8;',
      `${tableName}_${new Date().toISOString().replace(/[:.]/g, '-')}.sql`
    )
  };

  const downloadCsv = () => {
    const executeResult = getActiveQueryResult()
    if (!executeResult?.rows || executeResult.rows.length === 0) return;
    
    const rows = executeResult.rows;
    const headers = Object.keys(rows[0]);
    const csvContent = [
      headers.join(','),
      ...rows.map((row: Record<string, unknown>) => 
        headers.map(h => {
          let val = row[h];
          if (val === null || val === undefined) return '';
          val = formatResultExportValue(val).replace(/"/g, '""');
          return `"${val}"`;
        }).join(',')
      )
    ].join('\n');

    downloadTextArtifact(
      csvContent,
      'text/csv;charset=utf-8;',
      `query_result_${new Date().toISOString().replace(/[:.]/g, '-')}.csv`,
      true
    )
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
            <SqlHistory
              historyVersion={historyVersion}
              activeDbId={configData?.active_db_id ? String(configData.active_db_id) : null}
              onOpenSql={(text) => {
                openSqlInQueryTab(text)
                toast(tr('已打开 SQL', 'Opened SQL'), 'success')
              }}
              onInsertSql={(text) => {
                insertTextAtCursor(text)
                toast(tr('已插入 SQL', 'Inserted SQL'), "success")
              }}
              onRunSql={(text) => {
                const targetTabId = openSqlInQueryTab(text)
                void handleExecute(false, text, targetTabId)
                toast(tr('已重新执行 SQL', 'Re-ran SQL'), 'success')
              }}
            />
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
            if (toolId === 'perf-diagnostics') {
              const newTabId = `perf-diagnostics-${Date.now()}`
              setTabs([...tabs, { id: newTabId, type: 'perf-diagnostics', title: 'Perf Diagnostics' }])
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
            setTabStates(prev => ({ ...prev, [id]: createDefaultQueryTabState() }))
            setActiveTabId(id)
          }}
        />

        {/* Content Router */}
        <div className="flex-1 flex flex-col relative overflow-hidden">
          {tabs.map(tab => {
            const isActive = tab.id === activeTabId;
            const tabState = tabStates[tab.id] || createDefaultQueryTabState();
            const compareReport = buildQueryResultCompareReport(
              tabState.compareBaselineResult,
              getActiveQueryResult(tabState)
            )
            return (
              <div key={tab.id} className={`absolute inset-0 flex flex-col ${isActive ? 'z-10 opacity-100 pointer-events-auto' : 'z-0 opacity-0 pointer-events-none'}`} style={{ display: isActive ? 'flex' : 'none' }}>
                {tab.type === 'query' ? (
                  <div ref={isActive ? queryPaneRef : undefined} className="flex flex-col h-full min-h-0">
                    <QueryEditorActionPanel
                      tabState={tabState}
                      dbType={dbType}
                      dbConnected={Boolean(configData?.db_url)}
                      isSavingRule={isSavingRule}
                      isCompactActionBar={isCompactActionBar}
                      showMoreActions={showMoreActions}
                      resolveModelsList={resolveModelsList}
                      resolveActiveModelId={resolveActiveModelId}
                      resolveActiveTier={resolveActiveTier}
                      activeModelSupportsTier={activeModelSupportsTier}
                      isAiSwitching={isAiSwitching}
                      onSqlChange={(sql) => {
                        if (errorDecorationsRef.current) {
                          errorDecorationsRef.current.clear();
                          errorDecorationsRef.current = null;
                        }
                        setTabStates(prev => ({
                          ...prev,
                          [tab.id]: {
                            ...(prev[tab.id] || tabState),
                            sql,
                            lastErrorInsight: null,
                          }
                        }))
                      }}
                      onEditorMount={(editor, monaco) => handleEditorDidMount(editor, monaco, tab.id)}
                      onSaveRule={handleSaveRule}
                      onSaveBookmark={handleSaveBookmark}
                      onOpenCommandPalette={() => setShowCommandPalette(true)}
                      onAiOptimize={handleAIOptimize}
                      onAiExplain={handleAIExplain}
                      onExplainPlan={handleExplain}
                      onOpenSessionInfo={handleOpenSessionInfo}
                      onFormatSql={formatSqlEditor}
                      onToggleMoreActions={() => setShowMoreActions(v => !v)}
                      onCloseMoreActions={() => setShowMoreActions(false)}
                      onExecute={() => handleExecute(false)}
                      onCancelExecution={() => handleCancelExecution(tab.id)}
                      onTransactionModeChange={(mode) => handleTransactionModeChange(tab.id, mode)}
                      onCommitTransaction={() => void handleTransactionAction(tab.id, 'commit')}
                      onRollbackTransaction={() => void handleTransactionAction(tab.id, 'rollback')}
                      onModelChange={(modelId) => updateAiRuntime({ active_model_id: modelId, model_name: modelId }, `\u5df2\u5207\u6362\u6a21\u578b\uff1a${modelId}`)}
                      onTierChange={(tier) => updateAiRuntime({ active_tier: tier }, `\u5df2\u5207\u6362 Tier\uff1a${tier}`)}
                    />

                    <QueryResultsPanel
                      tabState={tabState}
                      resultsPanelHeight={resultsPanelHeight}
                      isResizingResults={isResizingResults}
                      onStartResize={() => setIsResizingResults(true)}
                      onSetResultsView={(view) => setTabStates(prev => ({ ...prev, [tab.id]: { ...(prev[tab.id] || tabState), resultsView: view } }))}
                      onSelectResult={(index) => setTabStates(prev => {
                        const currentTabState = prev[tab.id] || tabState
                        const nextExecuteResult = currentTabState.executeResults?.[index] || null
                        return {
                          ...prev,
                          [tab.id]: {
                            ...currentTabState,
                            activeResultIndex: index,
                            executeResult: nextExecuteResult,
                          }
                        }
                      })}
                      onLoadMoreResults={() => handleLoadMoreResults(tab.id)}
                      onClearResults={() => setTabStates(prev => ({
                        ...prev,
                        [tab.id]: {
                          ...(prev[tab.id] || tabState),
                          executeResult: null,
                          executeResults: [],
                          activeResultIndex: 0,
                          isExplainingError: false,
                          errorObj: null,
                          lastErrorInsight: null,
                          isLoadingMoreResults: false,
                        }
                      }))}
                      onExplainErrorWithAi={handleExplainErrorWithAI}
                      onFixWithAi={handleFixWithAI}
                      onApplySuggestedSql={handleApplyErrorSuggestion}
                      compareReport={compareReport}
                      onSetCompareBaseline={handleSetCompareBaseline}
                      onClearCompareBaseline={handleClearCompareBaseline}
                      onCopyCompareJson={() => void handleCopyCompareJson()}
                      onDownloadCompareJson={handleDownloadCompareJson}
                      onDownloadCsv={downloadCsv}
                      onDownloadSql={downloadSql}
                      resolveActiveModelLabel={resolveActiveModelLabel}
                      resolveActiveTier={resolveActiveTier}
                      aiMode={(configData as any)?.ai_mode}
                      queryChunkSize={QUERY_CHUNK_SIZE}
                    />
                  </div>
                ) : tab.type === 'table' ? (
                  <Suspense fallback={<SkeletonLoader />}>
                    <TableWorkspace tableName={tab.payload?.tableName} dbId={tab.payload?.dbId} isActive={isActive} />
                  </Suspense>
                ) : tab.type === 'explain' ? (
                  <Suspense fallback={<SkeletonLoader />}>
                    <ExecutionPlan sql={tab.payload?.sql} />
                  </Suspense>
                ) : tab.type === 'session-info' ? (
                  <Suspense fallback={<SkeletonLoader />}>
                    <SessionInfoPanel
                      dbId={tab.payload?.dbId ? String(tab.payload.dbId) : null}
                      dbLabel={tab.payload?.dbLabel || null}
                      dbType={tab.payload?.dbType || dbType}
                      isActive={isActive}
                      transactionMode={tab.payload?.sourceQueryTabId ? tabStates[tab.payload.sourceQueryTabId]?.transactionMode : undefined}
                      transactionState={tab.payload?.sourceQueryTabId ? tabStates[tab.payload.sourceQueryTabId]?.transactionState : undefined}
                    />
                  </Suspense>
                ) : tab.type === 'go-live-reports' ? (
                  <GoLiveReportsTab isActive={isActive} />
                ) : tab.type === 'go-live-audit' ? (
                  <GoLiveAuditTab isActive={isActive} />
                ) : tab.type === 'perf-diagnostics' ? (
                  <PerfDiagnosticsPanel
                    configData={configData}
                    schemaData={schemaData}
                    isActive={isActive}
                  />
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
                            ...(prev[queryTab!.id] || createDefaultQueryTabState()),
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
          savedBookmarks={savedBookmarks}
          smartSnippets={sqlSnippets}
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
            } else if (actionType === 'snippet') {
              const snippetSql = typeof payload?.sql === 'string' ? payload.sql : ''
              if (!snippetSql.trim()) return
              const activeTab = tabsRef.current.find((tab) => tab.id === activeTabIdRef.current)
              if (activeTab?.type === 'query') {
                insertTextAtCursor(snippetSql)
              } else {
                openSqlInQueryTab(snippetSql)
              }
              setShowCommandPalette(false)
              toast(
                activeTab?.type === 'query'
                  ? tr('已插入智能片段', 'Inserted smart snippet')
                  : tr('已打开智能片段', 'Opened smart snippet'),
                'success'
              )
            } else if (actionType === 'bookmark_open') {
              const bookmarkSql = typeof payload?.sql === 'string' ? payload.sql : ''
              if (!bookmarkSql.trim()) return
              openSqlInQueryTab(bookmarkSql)
              setShowCommandPalette(false)
              toast(tr('已打开书签 SQL', 'Opened bookmarked SQL'), 'success')
            } else if (actionType === 'bookmark_run') {
              const bookmarkSql = typeof payload?.sql === 'string' ? payload.sql : ''
              if (!bookmarkSql.trim()) return
              const targetTabId = openSqlInQueryTab(bookmarkSql)
              setShowCommandPalette(false)
              void handleExecute(false, bookmarkSql, targetTabId)
              toast(tr('已执行书签 SQL', 'Ran bookmarked SQL'), 'success')
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
