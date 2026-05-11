import { useCallback, useEffect, useMemo, useRef, useState, type ChangeEvent } from 'react'
import { AlignLeft, CheckSquare, ChevronDown, ChevronRight, Database, Eye, FileUp, FolderPlus, Plus, RefreshCw, Square, Star, Table, Trash2, X, Pencil, Copy, PlugZap, Unplug } from 'lucide-react'
import type { DbConnection, SchemaResponse } from '../types'
import { tr } from '../i18n'
import { api } from '../api'

function redactUrl(url: string) {
  return String(url || '').replace(/:[^:@]+@/, ':***@')
}

function displayHost(url: string) {
  const s = String(url || '')
  const afterAt = s.split('@')[1]
  if (afterAt) return afterAt
  return s
}

function extractDbName(url: string) {
  const s = String(url || '')
  const m = s.match(/\/([^/?#]+)(?:\?|#|$)/)
  const db = m?.[1] || ''
  if (!db || db.includes('@') || db.includes(':')) return ''
  return db
}

type DbTestDiagnostic = {
  status?: string
  category?: string
  code?: string
  message?: string
  hint?: string
  detail?: string
}

type DbTestResponse = {
  success?: boolean
  databases?: string[]
  capabilities_probed?: boolean
  capabilities_ok?: boolean | null
  stage?: string
  server_version?: string | null
  diagnostic?: DbTestDiagnostic
}

function getDbTestAdvice(code?: string, category?: string) {
  const normalized = String(code || '').toUpperCase()
  const byCode: Record<string, { title: string; actions: string[] }> = {
    DB_TEST_AUTH_FAILED: {
      title: tr('账号认证失败', 'Authentication failed'),
      actions: [
        tr('核对数据库用户名与密码是否正确。', 'Verify database username and password.'),
        tr('确认该账号允许从当前来源主机登录。', 'Ensure this account can log in from current host.'),
      ],
    },
    DB_TEST_NETWORK_FAILED: {
      title: tr('数据库网络不可达', 'Database network unreachable'),
      actions: [
        tr('检查数据库地址、端口及服务是否已启动。', 'Check database host/port and service status.'),
        tr('检查防火墙、安全组和白名单配置。', 'Check firewall, security group, and whitelist settings.'),
      ],
    },
    DB_TEST_SSL_FAILED: {
      title: tr('SSL 配置不匹配', 'SSL configuration mismatch'),
      actions: [
        tr('先尝试将 SSL 模式切换为 Preferred 或 Disabled 进行定位。', 'Try Preferred or Disabled SSL mode to isolate issues.'),
        tr('如需严格校验，请确认 CA/证书链完整有效。', 'For strict mode, ensure CA/certificate chain is valid.'),
      ],
    },
    DB_TEST_CONNECT_TIMEOUT: {
      title: tr('连接数据库超时', 'Database connect timeout'),
      actions: [
        tr('确认网络连通并降低首次连接延迟（VPN/代理/跨境线路）。', 'Check connectivity and reduce first-hop latency (VPN/proxy/cross-region).'),
        tr('云数据库场景请确认白名单和安全组已放行。', 'For cloud DB, verify whitelist and security group allow access.'),
      ],
    },
    DB_TEST_QUERY_TIMEOUT: {
      title: tr('查询数据库列表超时', 'Database list query timeout'),
      actions: [
        tr('稍后重试，检查数据库实例负载。', 'Retry later and check DB instance load.'),
        tr('确认账号具备读取数据库列表的权限。', 'Ensure account has permission to list databases.'),
      ],
    },
    DB_TEST_QUERY_FAILED: {
      title: tr('数据库列表读取失败', 'Failed to read database list'),
      actions: [
        tr('检查账号权限（SHOW DATABASES）。', 'Check account permission for SHOW DATABASES.'),
        tr('如开启代理层，请检查代理对元数据语句的限制。', 'If using proxy layer, verify metadata statement restrictions.'),
      ],
    },
    DB_TEST_CAPABILITY_PROBE_FAILED: {
      title: tr('Capability probe incomplete', 'Capability probe incomplete'),
      actions: [
        tr('The connection itself succeeded; you can save it first and probe database visibility later.', 'The connection itself succeeded; you can save it first and probe database visibility later.'),
        tr('If you need the database list, check SHOW DATABASES permission, proxy restrictions, or instance load.', 'If you need the database list, check SHOW DATABASES permission, proxy restrictions, or instance load.'),
      ],
    },
    DB_TEST_INVALID_URL: {
      title: tr('连接地址格式错误', 'Invalid connection URL'),
      actions: [
        tr('示例：mysql://user:password@host:3306/dbname', 'Example: mysql://user:password@host:3306/dbname'),
        tr('确认 URL 中用户名、密码和库名已正确编码。', 'Ensure username/password/database are correctly encoded in URL.'),
      ],
    },
    DB_TEST_MISSING_FIELDS: {
      title: tr('连接参数缺失', 'Missing connection fields'),
      actions: [
        tr('请填写 Host 与 Username。', 'Please fill Host and Username.'),
        tr('如使用 URL 方式，请改为完整 db_url。', 'If using URL mode, provide a complete db_url.'),
      ],
    },
    DB_TEST_SSH_MISSING_FIELDS: {
      title: tr('SSH 参数缺失', 'Missing SSH fields'),
      actions: [
        tr('启用 SSH 时必须填写 SSH Host、Username、Password。', 'When SSH is enabled, SSH Host/Username/Password are required.'),
        tr('默认 SSH 端口为 22，可按实际环境修改。', 'Default SSH port is 22, adjust as needed.'),
      ],
    },
    DB_TEST_SSH_CONNECT_FAILED: {
      title: tr('SSH 网络连接失败', 'SSH network connection failed'),
      actions: [
        tr('检查 SSH 地址、端口和网络连通性。', 'Check SSH host, port, and network reachability.'),
        tr('检查防火墙、安全组是否放行 SSH 端口。', 'Ensure firewall/security group allows SSH port.'),
      ],
    },
    DB_TEST_SSH_AUTH_FAILED: {
      title: tr('SSH 认证失败', 'SSH authentication failed'),
      actions: [
        tr('核对 SSH 用户名与密码是否正确。', 'Verify SSH username and password.'),
        tr('确认 SSH 账号具备登录权限。', 'Ensure SSH account has login permission.'),
      ],
    },
    DB_TEST_SSH_HANDSHAKE_FAILED: {
      title: tr('SSH 握手失败', 'SSH handshake failed'),
      actions: [
        tr('确认 SSH 服务端协议与加密套件兼容。', 'Verify SSH server protocol/cipher compatibility.'),
        tr('检查 SSH 服务状态并重试。', 'Check SSH service status and retry.'),
      ],
    },
    DB_TEST_SSH_HOSTKEY_FAILED: {
      title: tr('SSH 主机身份校验失败', 'SSH host identity verification failed'),
      actions: [
        tr('核对目标主机指纹，防止连接到错误主机。', 'Verify target host fingerprint to avoid wrong host.'),
        tr('确认运维侧未变更主机密钥。', 'Confirm host key was not rotated unexpectedly.'),
      ],
    },
    DB_TEST_SSH_CHANNEL_FAILED: {
      title: tr('SSH 通道创建失败', 'SSH channel creation failed'),
      actions: [
        tr('检查 SSH 服务器到数据库地址/端口的可达性。', 'Check reachability from SSH server to database host/port.'),
        tr('确认跳板机策略未禁止 direct-tcpip 转发。', 'Ensure bastion policy allows direct-tcpip forwarding.'),
      ],
    },
    DB_TEST_SSH_INIT_TIMEOUT: {
      title: tr('SSH 隧道初始化超时', 'SSH tunnel initialization timeout'),
      actions: [
        tr('检查 SSH 网络质量，适当减少网络抖动。', 'Check SSH network quality and reduce jitter.'),
        tr('确认目标环境无额外认证等待（如 MFA）。', 'Ensure no extra interactive auth step blocks startup (e.g. MFA).'),
      ],
    },
  }
  if (byCode[normalized]) return byCode[normalized]
  const fallbackTitle = category === 'success'
    ? tr('连接测试通过', 'Connection test passed')
    : tr('连接诊断建议', 'Connection diagnostics')
  const fallbackActions = category === 'success'
    ? [tr('可直接保存连接并展开表结构。', 'You can save this connection and load schema now.')]
    : [tr('请根据错误详情检查连接参数与网络策略。', 'Please verify connection settings and network policies based on details.')]
  return { title: fallbackTitle, actions: fallbackActions }
}

export function DbExplorerSidebar({
  configData,
  schemaData,
  isRefreshingSchema,
  onRefreshSchema,
  showSqlUpload,
  onSqlUpload,
  onSwitchActiveDb,
  onAddConnection,
  onUpdateConnection,
  onDuplicateConnection,
  onDisconnectConnection,
  onRenameGroup,
  onClearGroup,
  onBatchMoveConnections,
  onDeleteConnection,
  onOpenTable,
  onInsertTableName,
  onTableContextMenu,
}: {
  configData: any
  schemaData: SchemaResponse | null
  isRefreshingSchema: boolean
  onRefreshSchema: () => void
  showSqlUpload: boolean
  onSqlUpload: (e: ChangeEvent<HTMLInputElement>) => void
  onSwitchActiveDb: (dbId: string) => void
  onAddConnection: (name: string, url: string) => void
  onUpdateConnection: (connId: string, patch: Record<string, unknown>) => void
  onDuplicateConnection: (connId: string) => void
  onDisconnectConnection: (connId: string) => void
  onRenameGroup: (oldGroup: string, newGroup: string) => void
  onClearGroup: (groupName: string) => void
  onBatchMoveConnections: (connIds: string[], groupName: string | null) => void
  onDeleteConnection: (dbId: string) => void
  onOpenTable: (dbId: string, dbName: string, tableName: string) => void
  onInsertTableName: (tableName: string) => void
  onTableContextMenu: (x: number, y: number, table: { table_name: string }) => void
}) {
  const connections: DbConnection[] = useMemo(() => {
    const list = Array.isArray(configData?.db_connections) ? configData.db_connections : []
    return list as DbConnection[]
  }, [configData])

  const activeDbId = String(configData?.active_db_id || '')

  const [expanded, setExpanded] = useState<Record<string, boolean>>({})
  const [filter, setFilter] = useState('')
  const [tableFilters, setTableFilters] = useState<Record<string, string>>({})
  const [showAdd, setShowAdd] = useState(false)
  const [draftName, setDraftName] = useState('')
  const [draftUrl, setDraftUrl] = useState('')
  const [editingConnId, setEditingConnId] = useState<string | null>(null)
  const [editTab, setEditTab] = useState<'general' | 'advanced'>('general')
  const [editName, setEditName] = useState('')
  const [editUrl, setEditUrl] = useState('')
  const [editGroup, setEditGroup] = useState('')
  const [editColor, setEditColor] = useState('#3b82f6')
  const [editSshEnabled, setEditSshEnabled] = useState(false)
  const [editSshHost, setEditSshHost] = useState('')
  const [editSshPort, setEditSshPort] = useState('22')
  const [editSshUser, setEditSshUser] = useState('')
  const [editSshPassword, setEditSshPassword] = useState('')
  const [editSslEnabled, setEditSslEnabled] = useState(false)
  const [editSslMode, setEditSslMode] = useState('preferred')
  const [connMenu, setConnMenu] = useState<{ x: number; y: number; conn: DbConnection } | null>(null)
  const [groupRename, setGroupRename] = useState<{ oldName: string; newName: string } | null>(null)
  const [groupClearConfirm, setGroupClearConfirm] = useState<string | null>(null)
  const [dragConnId, setDragConnId] = useState<string | null>(null)
  const [dragOverGroup, setDragOverGroup] = useState<string | null>(null)
  const [groupCreate, setGroupCreate] = useState<{ name: string; connIds: string[] } | null>(null)
  const [batchMode, setBatchMode] = useState(false)
  const [selectedConnIds, setSelectedConnIds] = useState<string[]>([])
  const [batchTargetGroup, setBatchTargetGroup] = useState('')
  const [isTestingConnection, setIsTestingConnection] = useState(false)
  const [testConnectionResult, setTestConnectionResult] = useState<DbTestDiagnostic | null>(null)
  const [copyDiagOk, setCopyDiagOk] = useState(false)
  const editUrlRef = useRef<HTMLInputElement | null>(null)
  const editSshHostRef = useRef<HTMLInputElement | null>(null)
  const editSshUserRef = useRef<HTMLInputElement | null>(null)
  const editSshPasswordRef = useRef<HTMLInputElement | null>(null)
  const editSslModeRef = useRef<HTMLSelectElement | null>(null)
  const [schemaByDb, setSchemaByDb] = useState<Record<string, SchemaResponse>>({})
  const [loadingSchemaByDb, setLoadingSchemaByDb] = useState<Record<string, boolean>>({})

  useEffect(() => {
    if (!activeDbId || !schemaData) return
    setSchemaByDb((prev) => ({ ...prev, [activeDbId]: schemaData }))
  }, [activeDbId, schemaData])

  useEffect(() => {
    const close = () => setConnMenu(null)
    window.addEventListener('click', close)
    return () => window.removeEventListener('click', close)
  }, [])

  const loadSchemaByDbId = useCallback(async (dbId: string) => {
    if (!dbId || loadingSchemaByDb[dbId] || schemaByDb[dbId]) return
    setLoadingSchemaByDb((prev) => ({ ...prev, [dbId]: true }))
    try {
      const schema = await api.getSchema(dbId)
      setSchemaByDb((prev) => ({ ...prev, [dbId]: schema }))
    } catch (e: any) {
      const msg = e?.response?.data?.message || e?.message || tr('加载结构失败', 'Failed to load schema')
      window.dispatchEvent(new CustomEvent('global-toast', { detail: { message: msg, type: 'error' } }))
    } finally {
      setLoadingSchemaByDb((prev) => ({ ...prev, [dbId]: false }))
    }
  }, [loadingSchemaByDb, schemaByDb])

  const setExpandedState = useCallback((dbId: string, nextExpanded: boolean) => {
    setExpanded((prev) => ({ ...prev, [dbId]: nextExpanded }))
    if (nextExpanded) {
      loadSchemaByDbId(dbId)
    }
  }, [loadSchemaByDbId])

  const handleConnectionClick = useCallback((dbId: string, isActive: boolean, isExpanded: boolean) => {
    if (isActive) {
      setExpandedState(dbId, !isExpanded)
      return
    }
    onSwitchActiveDb(dbId)
    setExpandedState(dbId, true)
  }, [onSwitchActiveDb, setExpandedState])

  const filteredConnections = useMemo(() => {
    const q = filter.trim().toLowerCase()
    if (!q) return connections
    return connections.filter((c) => {
      const name = String((c as any).name || c.id || '').toLowerCase()
      const url = String(c.url || '').toLowerCase()
      return name.includes(q) || url.includes(q)
    })
  }, [connections, filter])

  const ungroupedLabel = tr('未分组', 'Ungrouped')

  const groupedConnections = useMemo(() => {
    const map: Record<string, DbConnection[]> = {}
    for (const c of filteredConnections) {
      const group = String((c as any).group_name || ungroupedLabel)
      if (!map[group]) map[group] = []
      map[group].push(c)
    }
    const groups = Object.keys(map).sort((a, b) => {
      if (a === ungroupedLabel) return 1
      if (b === ungroupedLabel) return -1
      return a.localeCompare(b)
    })
    for (const g of groups) {
      map[g].sort((a, b) => {
        const af = !!(a as any).is_favorite
        const bf = !!(b as any).is_favorite
        if (af !== bf) return af ? -1 : 1
        return String((a as any).name || a.id).localeCompare(String((b as any).name || b.id))
      })
    }
    return { groups, map }
  }, [filteredConnections, ungroupedLabel])

  const activeConn = useMemo(() => {
    if (!activeDbId) return null
    return connections.find((c) => c.id === activeDbId) || null
  }, [connections, activeDbId])

  const activeDbName = useMemo(() => {
    if (schemaData?.db_name) return schemaData.db_name
    if (activeConn?.url) return extractDbName(activeConn.url)
    return ''
  }, [schemaData, activeConn])

  const openEditDialog = useCallback((conn: DbConnection) => {
    setEditingConnId(conn.id)
    setEditTab('general')
    setEditName(String((conn as any).name || conn.id))
    setEditUrl(String(conn.url || ''))
    setEditGroup(String((conn as any).group_name || ''))
    setEditColor(String((conn as any).color || '#3b82f6'))
    const ssh = ((conn as any).ssh || {}) as any
    const ssl = ((conn as any).ssl || {}) as any
    setEditSshEnabled(!!ssh.enabled)
    setEditSshHost(String(ssh.host || ''))
    setEditSshPort(String(ssh.port || 22))
    setEditSshUser(String(ssh.username || ''))
    setEditSshPassword(String(ssh.password || ''))
    setEditSslEnabled(!!ssl.enabled)
    setEditSslMode(String(ssl.mode || 'preferred'))
    setTestConnectionResult(null)
  }, [])

  const testCurrentConnection = useCallback(async () => {
    if (!editUrl.trim()) return
    setIsTestingConnection(true)
    setTestConnectionResult(null)
    try {
      const res = await api.dbTest({
        db_url: editUrl.trim(),
        ssl_mode: editSslEnabled ? editSslMode : undefined,
        ssh_enabled: editSshEnabled,
        ssh_host: editSshHost.trim(),
        ssh_port: Number(editSshPort || 22),
        ssh_username: editSshUser.trim(),
        ssh_password: editSshPassword,
      }) as DbTestResponse
      if (res?.success === false) {
        setTestConnectionResult({
          status: res.diagnostic?.status || 'error',
          category: res.diagnostic?.category || 'unknown',
          code: res.diagnostic?.code || 'DB_TEST_FAILED',
          message: res.diagnostic?.message || 'Connection failed',
          hint: res.diagnostic?.hint,
          detail: res.diagnostic?.detail,
        })
        return
      }
      if (res?.capabilities_probed) {
        if (res?.capabilities_ok === true) {
          const count = Array.isArray(res?.databases) ? res.databases.length : 0
          setTestConnectionResult({
            status: 'success',
            category: 'success',
            code: 'DB_TEST_OK',
            message: `Connection successful, found ${count} databases.`,
          })
        } else {
          setTestConnectionResult({
            status: res.diagnostic?.status || 'warning',
            category: res.diagnostic?.category || 'query',
            code: res.diagnostic?.code || 'DB_TEST_CAPABILITY_PROBE_FAILED',
            message: res.diagnostic?.message || 'Connection successful, but capability probe did not complete.',
            hint: res.diagnostic?.hint,
            detail: res.diagnostic?.detail,
          })
        }
        return
      }
      setTestConnectionResult({
        status: 'success',
        category: 'success',
        code: 'DB_TEST_OK',
        message: res.diagnostic?.message || 'Connection successful.',
      })
    } catch (e: any) {
      const data = e?.response?.data || {}
      setTestConnectionResult({
        status: 'error',
        category: String(data?.type || 'unknown'),
        code: String(data?.code || 'DB_TEST_REQUEST_FAILED'),
        message: String(data?.message || data?.error || e?.message || 'Connection failed'),
        detail: String(data?.details || ''),
      })
    } finally {
      setIsTestingConnection(false)
    }
  }, [editUrl, editSshEnabled, editSshHost, editSshPassword, editSshPort, editSshUser, editSslEnabled, editSslMode])

  const testConnectionAdvice = useMemo(() => {
    if (!testConnectionResult) return null
    return getDbTestAdvice(testConnectionResult.code, testConnectionResult.category)
  }, [testConnectionResult])

  const applySslPresetAndRetest = useCallback(async (mode: 'preferred' | 'disabled') => {
    setEditSslEnabled(mode !== 'disabled')
    setEditSslMode(mode)
    setIsTestingConnection(true)
    setTestConnectionResult(null)
    try {
      const res = await api.dbTest({
        db_url: editUrl.trim(),
        ssl_mode: mode === 'disabled' ? 'disabled' : mode,
        ssh_enabled: editSshEnabled,
        ssh_host: editSshHost.trim(),
        ssh_port: Number(editSshPort || 22),
        ssh_username: editSshUser.trim(),
        ssh_password: editSshPassword,
      }) as DbTestResponse
      if (res?.success === false) {
        setTestConnectionResult({
          status: res.diagnostic?.status || 'error',
          category: res.diagnostic?.category || 'unknown',
          code: res.diagnostic?.code || 'DB_TEST_FAILED',
          message: res.diagnostic?.message || 'Connection failed',
          hint: res.diagnostic?.hint,
          detail: res.diagnostic?.detail,
        })
        return
      }
      if (res?.capabilities_probed) {
        if (res?.capabilities_ok === true) {
          const count = Array.isArray(res?.databases) ? res.databases.length : 0
          setTestConnectionResult({
            status: 'success',
            category: 'success',
            code: 'DB_TEST_OK',
            message: `Connection successful, found ${count} databases.`,
          })
        } else {
          setTestConnectionResult({
            status: res.diagnostic?.status || 'warning',
            category: res.diagnostic?.category || 'query',
            code: res.diagnostic?.code || 'DB_TEST_CAPABILITY_PROBE_FAILED',
            message: res.diagnostic?.message || 'Connection successful, but capability probe did not complete.',
            hint: res.diagnostic?.hint,
            detail: res.diagnostic?.detail,
          })
        }
        return
      }
      setTestConnectionResult({
        status: 'success',
        category: 'success',
        code: 'DB_TEST_OK',
        message: res.diagnostic?.message || 'Connection successful.',
      })
    } catch (e: any) {
      const data = e?.response?.data || {}
      setTestConnectionResult({
        status: 'error',
        category: String(data?.type || 'unknown'),
        code: String(data?.code || 'DB_TEST_REQUEST_FAILED'),
        message: String(data?.message || data?.error || e?.message || 'Connection failed'),
        detail: String(data?.details || ''),
      })
    } finally {
      setIsTestingConnection(false)
    }
  }, [editUrl, editSshEnabled, editSshHost, editSshPassword, editSshPort, editSshUser])

  const copyDiagnostic = useCallback(async () => {
    if (!testConnectionResult) return
    const payload = [
      `code=${testConnectionResult.code || ''}`,
      `category=${testConnectionResult.category || ''}`,
      `message=${testConnectionResult.message || ''}`,
      `hint=${testConnectionResult.hint || ''}`,
      `detail=${testConnectionResult.detail || ''}`,
    ].join('\n')
    try {
      if (navigator?.clipboard?.writeText) {
        await navigator.clipboard.writeText(payload)
      } else {
        const textarea = document.createElement('textarea')
        textarea.value = payload
        textarea.style.position = 'fixed'
        textarea.style.left = '-9999px'
        document.body.appendChild(textarea)
        textarea.focus()
        textarea.select()
        document.execCommand('copy')
        document.body.removeChild(textarea)
      }
      setCopyDiagOk(true)
      setTimeout(() => setCopyDiagOk(false), 1200)
    } catch {
      window.dispatchEvent(new CustomEvent('global-toast', {
        detail: { message: tr('复制失败，请手动复制详情。', 'Copy failed, please copy details manually.'), type: 'error' }
      }))
    }
  }, [testConnectionResult, tr])

  useEffect(() => {
    const code = String(testConnectionResult?.code || '')
    if (!code) return
    const focusLater = (el: HTMLInputElement | HTMLSelectElement | null, selectText = false) => {
      window.setTimeout(() => {
        if (!el) return
        try {
          el.focus()
          if (selectText && 'select' in el) (el as HTMLInputElement).select()
        } catch {}
      }, 0)
    }
    if (
      code === 'DB_TEST_INVALID_URL' ||
      code === 'DB_TEST_MISSING_FIELDS' ||
      code === 'DB_TEST_AUTH_FAILED' ||
      code === 'DB_TEST_NETWORK_FAILED' ||
      code === 'DB_TEST_CONNECT_TIMEOUT'
    ) {
      setEditTab('general')
      focusLater(editUrlRef.current, true)
      return
    }
    if (code === 'DB_TEST_SSL_FAILED') {
      setEditTab('advanced')
      setEditSslEnabled(true)
      focusLater(editSslModeRef.current)
      return
    }
    if (code === 'DB_TEST_SSH_MISSING_FIELDS') {
      setEditTab('advanced')
      setEditSshEnabled(true)
      if (!editSshHost.trim()) {
        focusLater(editSshHostRef.current, true)
        return
      }
      if (!editSshUser.trim()) {
        focusLater(editSshUserRef.current, true)
        return
      }
      focusLater(editSshPasswordRef.current, true)
      return
    }
    if (code === 'DB_TEST_SSH_AUTH_FAILED') {
      setEditTab('advanced')
      setEditSshEnabled(true)
      if (!editSshUser.trim()) {
        focusLater(editSshUserRef.current, true)
        return
      }
      focusLater(editSshPasswordRef.current, true)
      return
    }
    if (
      code === 'DB_TEST_SSH_CONNECT_FAILED' ||
      code === 'DB_TEST_SSH_INIT_TIMEOUT' ||
      code === 'DB_TEST_SSH_HANDSHAKE_FAILED' ||
      code === 'DB_TEST_SSH_HOSTKEY_FAILED' ||
      code === 'DB_TEST_SSH_CHANNEL_FAILED'
    ) {
      setEditTab('advanced')
      setEditSshEnabled(true)
      focusLater(editSshHostRef.current, true)
    }
  }, [testConnectionResult?.code, editSshHost, editSshUser])

  return (
    <div className="p-2 flex-1 overflow-y-auto relative">
      <div className="px-2 pt-2 pb-3 border-b border-dark-border">
        <div className="flex items-center justify-between gap-2">
          <div className="text-xs text-gray-500 uppercase font-bold tracking-wider">
            {tr(`连接 (${connections.length})`, `Connections (${connections.length})`)}
          </div>
          <div className="flex items-center gap-1">
            <button
              title={tr('新建分组', 'New Group')}
              onClick={() => setGroupCreate({ name: '', connIds: [] })}
              className="flex items-center justify-center w-7 h-7 rounded border border-[#30363d] bg-[#0d1117] hover:bg-[#161b22] text-gray-400 hover:text-gray-200 transition-colors"
            >
              <FolderPlus className="w-3.5 h-3.5" />
            </button>
            <button
              title={tr('批量移动', 'Batch Move')}
              onClick={() => {
                const next = !batchMode
                setBatchMode(next)
                if (!next) {
                  setSelectedConnIds([])
                  setBatchTargetGroup('')
                }
              }}
              className={`flex items-center justify-center w-7 h-7 rounded border transition-colors ${
                batchMode
                  ? 'border-blue-500/40 bg-blue-500/15 text-blue-300'
                  : 'border-[#30363d] bg-[#0d1117] text-gray-400 hover:bg-[#161b22] hover:text-gray-200'
              }`}
            >
              {batchMode ? <CheckSquare className="w-3.5 h-3.5" /> : <Square className="w-3.5 h-3.5" />}
            </button>
            {showSqlUpload && (
              <label
                className="flex items-center justify-center w-7 h-7 rounded border border-blue-500/30 bg-blue-500/10 hover:bg-blue-500/15 text-blue-400 hover:text-blue-300 transition-colors cursor-pointer"
                title={tr('导入离线 DDL', 'Upload offline DDL')}
              >
                <FileUp className="w-3.5 h-3.5" />
                <input type="file" accept=".sql" className="hidden" onChange={onSqlUpload} />
              </label>
            )}
            <button
              title={tr('刷新', 'Refresh')}
              onClick={onRefreshSchema}
              className="flex items-center justify-center w-7 h-7 rounded border border-[#30363d] bg-[#0d1117] hover:bg-[#161b22] text-gray-400 hover:text-gray-200 transition-colors"
            >
              <RefreshCw className={`w-3.5 h-3.5 ${isRefreshingSchema ? 'animate-spin' : ''}`} />
            </button>
            <button
              title={tr('新建连接', 'New Connection')}
              onClick={() => setShowAdd(true)}
              className="flex items-center justify-center w-7 h-7 rounded border border-[#30363d] bg-[#0d1117] hover:bg-[#161b22] text-gray-400 hover:text-gray-200 transition-colors"
            >
              <Plus className="w-3.5 h-3.5" />
            </button>
          </div>
        </div>
        <input
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          placeholder={tr('筛选连接...', 'Filter connections...')}
          className="mt-2 w-full bg-[#0d1117] border border-[#30363d] rounded px-2 py-1.5 text-xs text-gray-200 placeholder:text-gray-600 outline-none focus:border-blue-500/50"
        />
        {batchMode && (
          <div className="mt-2 flex items-center gap-2">
            <input
              value={batchTargetGroup}
              onChange={(e) => setBatchTargetGroup(e.target.value)}
              placeholder={tr('输入目标分组，留空表示未分组', 'Target group, empty for ungrouped')}
              className="flex-1 bg-[#0d1117] border border-[#30363d] rounded px-2 py-1.5 text-xs text-gray-200"
            />
            <button
              onClick={() => {
                onBatchMoveConnections(selectedConnIds, batchTargetGroup.trim() || null)
                setSelectedConnIds([])
                setBatchTargetGroup('')
                setBatchMode(false)
              }}
              disabled={selectedConnIds.length === 0}
              className="px-2 py-1.5 rounded text-xs border border-[#30363d] bg-[#21262d] text-gray-200 hover:bg-[#30363d] disabled:opacity-50"
            >
              {tr('应用', 'Apply')}
            </button>
          </div>
        )}
      </div>

      {filteredConnections.length === 0 ? (
        <div className="p-4 text-center text-gray-500 flex flex-col items-center mt-10">
          <Database className="w-10 h-10 mb-3 opacity-30" />
          <p className="text-sm font-medium mb-1">暂无连接</p>
          <p className="text-xs opacity-70 mb-4 px-2">点击右上角 + 添加数据库连接。</p>
        </div>
      ) : (
        <div className="pt-2">
          {groupedConnections.groups.map((groupName) => (
            <div
              key={groupName}
              className={`mb-3 rounded ${dragOverGroup === groupName ? 'ring-1 ring-blue-500/40 bg-blue-500/5' : ''}`}
              onDragOver={(e) => {
                e.preventDefault()
                setDragOverGroup(groupName)
              }}
              onDragLeave={() => setDragOverGroup(null)}
              onDrop={(e) => {
                e.preventDefault()
                if (!dragConnId) return
                const nextGroup = groupName === ungroupedLabel ? null : groupName
                onUpdateConnection(dragConnId, { group_name: nextGroup })
                setDragConnId(null)
                setDragOverGroup(null)
              }}
            >
              <div className="px-2 py-1 text-[10px] text-gray-500 uppercase tracking-wider font-bold flex items-center justify-between gap-2">
                <span>{groupName}</span>
                {groupName !== ungroupedLabel && (
                  <div className="flex items-center gap-1">
                    <button
                      onClick={() => setGroupRename({ oldName: groupName, newName: groupName })}
                      className="w-5 h-5 rounded border border-[#30363d] bg-[#0d1117] text-gray-500 hover:text-blue-400 hover:bg-blue-500/10 transition-colors flex items-center justify-center"
                      title={tr('重命名分组', 'Rename group')}
                    >
                      <Pencil className="w-3 h-3" />
                    </button>
                    <button
                      onClick={() => setGroupClearConfirm(groupName)}
                      className="w-5 h-5 rounded border border-[#30363d] bg-[#0d1117] text-gray-500 hover:text-orange-300 hover:bg-orange-500/10 transition-colors flex items-center justify-center"
                      title={tr('清空分组', 'Clear group')}
                    >
                      <Trash2 className="w-3 h-3" />
                    </button>
                  </div>
                )}
              </div>
              {groupedConnections.map[groupName].map((c) => {
            const isActive = c.id === activeDbId
            const isExpanded = !!expanded[c.id]
            const dbSchema = schemaByDb[c.id]
            const dbName = dbSchema?.db_name || (isActive ? activeDbName : extractDbName(c.url))
            const tableFilter = (tableFilters[c.id] || '').trim().toLowerCase()
            const visibleTables = (dbSchema?.tables || []).filter((t) =>
              !tableFilter || t.table_name.toLowerCase().includes(tableFilter)
            )
            const visibleViews = (dbSchema?.views || []).filter((v: any) => {
              const viewName = String(v.table_name || v.view_name || '')
              return !tableFilter || viewName.toLowerCase().includes(tableFilter)
            })
            return (
              <div key={c.id} className="mb-1">
                <div
                  className={`flex items-center gap-2 px-2 py-2 rounded cursor-pointer transition-colors ${
                    isActive ? 'bg-blue-500/10 border border-blue-500/20' : 'hover:bg-[#21262d]'
                  }`}
                  draggable
                  onDragStart={() => setDragConnId(c.id)}
                  onDragEnd={() => {
                    setDragConnId(null)
                    setDragOverGroup(null)
                  }}
                  onClick={() => handleConnectionClick(c.id, isActive, isExpanded)}
                  onContextMenu={(e) => {
                    e.preventDefault()
                    setConnMenu({ x: e.clientX, y: e.clientY, conn: c })
                  }}
                >
                  {batchMode && (
                    <button
                      onClick={(e) => {
                        e.stopPropagation()
                        setSelectedConnIds((prev) => {
                          const exists = prev.includes(c.id)
                          if (exists) return prev.filter((x) => x !== c.id)
                          return [...prev, c.id]
                        })
                      }}
                      className="w-4 h-4 flex items-center justify-center text-gray-500 hover:text-gray-200"
                    >
                      {selectedConnIds.includes(c.id) ? <CheckSquare className="w-4 h-4 text-blue-400" /> : <Square className="w-4 h-4" />}
                    </button>
                  )}
                  <button
                    onClick={(e) => {
                      e.stopPropagation()
                      const nextExpanded = !isExpanded
                      setExpandedState(c.id, nextExpanded)
                    }}
                    className="w-5 h-5 flex items-center justify-center text-gray-500 hover:text-gray-200"
                    title={isExpanded ? tr('折叠', 'Collapse') : tr('展开', 'Expand')}
                  >
                    {isExpanded ? <ChevronDown className="w-4 h-4" /> : <ChevronRight className="w-4 h-4" />}
                  </button>
                  <div
                    className="w-2.5 h-2.5 rounded-full border border-black/40"
                    style={{ backgroundColor: String((c as any).color || '#3b82f6') }}
                    title={tr('连接颜色', 'Connection color')}
                  />
                  <Database className={`w-4 h-4 ${isActive ? 'text-blue-400' : 'text-gray-400'}`} />
                  <div className="min-w-0 flex-1">
                    <div className="text-sm text-gray-200 font-medium truncate" title={String((c as any).name || c.id)}>
                      {!!(c as any).is_favorite && <Star className="inline w-3 h-3 text-yellow-400 mr-1.5 -mt-0.5" />}
                      {String((c as any).name || c.id)}
                      {isActive && (c as any).is_read_only && (
                        <span className="ml-2 text-[10px] bg-red-500/20 text-red-500 border border-red-500/30 px-1.5 py-0.5 rounded uppercase tracking-wider font-bold shadow-sm">
                          [只读]
                        </span>
                      )}
                    </div>
                    <div className="text-[10px] text-gray-500 font-mono truncate" title={redactUrl(displayHost(c.url))}>
                      {redactUrl(displayHost(c.url))}
                    </div>
                  </div>
                  <button
                    onClick={(e) => {
                      e.stopPropagation()
                      openEditDialog(c)
                    }}
                    className="p-1.5 rounded border border-[#30363d] bg-[#0d1117] text-gray-500 hover:text-blue-400 hover:bg-blue-500/10 transition-colors"
                    title={tr('编辑', 'Edit')}
                  >
                    <Pencil className="w-3.5 h-3.5" />
                  </button>
                  <button
                    onClick={(e) => {
                      e.stopPropagation()
                      onUpdateConnection(c.id, { is_favorite: !(c as any).is_favorite })
                    }}
                    className={`p-1.5 rounded border border-[#30363d] bg-[#0d1117] transition-colors ${
                      (c as any).is_favorite ? 'text-yellow-400 hover:bg-yellow-500/10' : 'text-gray-500 hover:text-yellow-300 hover:bg-yellow-500/10'
                    }`}
                    title={tr('收藏', 'Favorite')}
                  >
                    <Star className="w-3.5 h-3.5" />
                  </button>
                  <button
                    onClick={(e) => {
                      e.stopPropagation()
                      onDeleteConnection(c.id)
                    }}
                    className="p-1.5 rounded border border-[#30363d] bg-[#0d1117] text-gray-500 hover:text-red-400 hover:bg-red-500/10 transition-colors"
                    title={tr('删除', 'Delete')}
                  >
                    <Trash2 className="w-3.5 h-3.5" />
                  </button>
                </div>

                {isExpanded && (
                  <div className="ml-8 mt-1 mb-2">
                    {!dbSchema && loadingSchemaByDb[c.id] ? (
                      <div className="text-xs text-gray-600 px-2 py-2">
                        {tr('加载中...', 'Loading...')}
                      </div>
                    ) : !dbSchema ? (
                      <div className="text-xs text-gray-600 px-2 py-2">{tr('点击连接以加载表结构', 'Click connection to load schema')}</div>
                    ) : (
                      <>
                        <div className="flex items-center gap-2 px-2 py-1.5 rounded text-gray-300">
                          <Database className="w-4 h-4 text-gray-400" />
                          <span className="text-sm truncate" title={dbName || tr('(未知数据库)', '(unknown db)')}>{dbName || tr('(未知数据库)', '(unknown db)')}</span>
                        </div>
                        <div className="mt-2 px-2">
                          <input
                            value={tableFilters[c.id] || ''}
                            onChange={(e) => setTableFilters((prev) => ({ ...prev, [c.id]: e.target.value }))}
                            placeholder={tr('筛选表 / 视图...', 'Filter tables / views...')}
                            className="w-full bg-[#0d1117] border border-[#30363d] rounded px-2 py-1.5 text-xs text-gray-200 placeholder:text-gray-600 outline-none focus:border-blue-500/50"
                          />
                        </div>
                        <div className="mt-1">
                          <div className="px-2 py-1 text-[10px] text-gray-500 uppercase font-bold tracking-wider flex items-center justify-between">
                            <span>{tr(`表 (${visibleTables.length}/${dbSchema?.tables?.length || 0})`, `Tables (${visibleTables.length}/${dbSchema?.tables?.length || 0})`)}</span>
                          </div>
                          {visibleTables.length === 0 ? (
                            <div className="px-2 py-2 text-xs text-gray-600">{tr('无匹配表', 'No matching tables')}</div>
                          ) : visibleTables.map((t) => (
                            <div
                              key={t.table_name}
                              className="flex items-center gap-2 px-2 py-1.5 rounded cursor-pointer transition-colors duration-150 group hover:bg-[#21262d] text-gray-300"
                              onContextMenu={(e) => {
                                e.preventDefault()
                                onTableContextMenu(e.clientX, e.clientY, { table_name: t.table_name })
                              }}
                            >
                              <div
                                className="flex items-center gap-2 flex-1 overflow-hidden"
                                onDoubleClick={() => onOpenTable(c.id, String((c as any).name || c.id), t.table_name)}
                              >
                                <Table className="w-4 h-4 flex-shrink-0" />
                                <span className="text-sm truncate" title={t.table_name}>{t.table_name}</span>
                              </div>
                              <button
                                onClick={(e) => {
                                  e.stopPropagation()
                                  onOpenTable(c.id, String((c as any).name || c.id), t.table_name)
                                }}
                                className="p-1 hover:bg-blue-500/20 rounded text-gray-400 hover:text-blue-400 transition-all"
                                title={tr('打开表数据', 'Open table data')}
                              >
                                <Eye className="w-3.5 h-3.5" />
                              </button>
                              <button
                                onClick={(e) => {
                                  e.stopPropagation()
                                  onInsertTableName(t.table_name)
                                }}
                                className="opacity-0 group-hover:opacity-100 p-1 hover:bg-blue-500/20 rounded text-gray-400 hover:text-blue-400 transition-all"
                                title={tr('插入表名到编辑器', 'Insert table name into editor')}
                              >
                                <AlignLeft className="w-3.5 h-3.5" />
                              </button>
                            </div>
                          ))}
                        </div>

                        <div className="mt-3">
                          <div className="px-2 py-1 text-[10px] text-gray-500 uppercase font-bold tracking-wider flex items-center justify-between">
                            <span>{tr(`视图 (${visibleViews.length}/${dbSchema?.views?.length || 0})`, `Views (${visibleViews.length}/${dbSchema?.views?.length || 0})`)}</span>
                          </div>
                          {visibleViews.length === 0 ? (
                            <div className="px-2 py-2 text-xs text-gray-600">{tr('无匹配视图', 'No matching views')}</div>
                          ) : visibleViews.map((v: any) => {
                            const viewName = String(v.table_name || v.view_name)
                            return (
                              <div
                                key={viewName}
                                className="flex items-center gap-2 px-2 py-1.5 rounded text-gray-400 hover:bg-[#21262d] transition-colors cursor-pointer group"
                                onDoubleClick={() => onOpenTable(c.id, String((c as any).name || c.id), viewName)}
                              >
                                <Eye className="w-4 h-4" />
                                <span className="text-sm truncate flex-1" title={viewName}>{viewName}</span>
                                <button
                                  onClick={(e) => {
                                    e.stopPropagation()
                                    onOpenTable(c.id, String((c as any).name || c.id), viewName)
                                  }}
                                  className="p-1 hover:bg-blue-500/20 rounded text-gray-400 hover:text-blue-400 transition-all"
                                  title={tr('打开视图数据', 'Open view data')}
                                >
                                  <Eye className="w-3.5 h-3.5" />
                                </button>
                                <button
                                  onClick={(e) => {
                                    e.stopPropagation()
                                    onInsertTableName(viewName)
                                  }}
                                  className="p-1 hover:bg-blue-500/20 rounded text-gray-400 hover:text-blue-400 transition-all"
                                  title={tr('插入视图名到编辑器', 'Insert view name into editor')}
                                >
                                  <AlignLeft className="w-3.5 h-3.5" />
                                </button>
                              </div>
                            )
                          })}
                        </div>
                      </>
                    )}
                  </div>
                )}
              </div>
            )
              })}
            </div>
          ))}
        </div>
      )}

      {connMenu && (
        <div
          className="fixed z-[80] min-w-[180px] bg-[#161b22] border border-[#30363d] rounded-md shadow-2xl py-1 text-sm text-gray-300"
          style={{ top: connMenu.y, left: connMenu.x }}
          onClick={(e) => e.stopPropagation()}
        >
          <button
            className="w-full text-left px-3 py-1.5 hover:bg-blue-500/20 hover:text-blue-300 transition-colors flex items-center gap-2"
            onClick={() => {
              onSwitchActiveDb(connMenu.conn.id)
              setConnMenu(null)
            }}
          >
            <PlugZap className="w-3.5 h-3.5" />
            {tr('连接', 'Connect')}
          </button>
          <button
            className="w-full text-left px-3 py-1.5 hover:bg-blue-500/20 hover:text-blue-300 transition-colors flex items-center gap-2"
            onClick={() => {
              onDisconnectConnection(connMenu.conn.id)
              setConnMenu(null)
            }}
          >
            <Unplug className="w-3.5 h-3.5" />
            {tr('断开', 'Disconnect')}
          </button>
          <button
            className="w-full text-left px-3 py-1.5 hover:bg-blue-500/20 hover:text-blue-300 transition-colors flex items-center gap-2"
            onClick={() => {
              openEditDialog(connMenu.conn)
              setConnMenu(null)
            }}
          >
            <Pencil className="w-3.5 h-3.5" />
            {tr('编辑', 'Edit')}
          </button>
          <button
            className="w-full text-left px-3 py-1.5 hover:bg-blue-500/20 hover:text-blue-300 transition-colors flex items-center gap-2"
            onClick={() => {
              onDuplicateConnection(connMenu.conn.id)
              setConnMenu(null)
            }}
          >
            <Copy className="w-3.5 h-3.5" />
            {tr('复制连接', 'Duplicate')}
          </button>
          <button
            className="w-full text-left px-3 py-1.5 hover:bg-yellow-500/15 hover:text-yellow-300 transition-colors flex items-center gap-2"
            onClick={() => {
              onUpdateConnection(connMenu.conn.id, { is_favorite: !(connMenu.conn as any).is_favorite })
              setConnMenu(null)
            }}
          >
            <Star className="w-3.5 h-3.5" />
            {(connMenu.conn as any).is_favorite ? tr('取消收藏', 'Unfavorite') : tr('收藏', 'Favorite')}
          </button>
          <button
            className="w-full text-left px-3 py-1.5 hover:bg-red-500/20 hover:text-red-300 transition-colors flex items-center gap-2"
            onClick={() => {
              onDeleteConnection(connMenu.conn.id)
              setConnMenu(null)
            }}
          >
            <Trash2 className="w-3.5 h-3.5" />
            {tr('删除', 'Delete')}
          </button>
        </div>
      )}

      {groupRename && (
        <div className="fixed inset-0 z-[70] flex items-center justify-center bg-black/70 backdrop-blur-sm">
          <div className="bg-[#161b22] border border-[#30363d] rounded-xl shadow-2xl w-[520px] overflow-hidden">
            <div className="px-6 py-4 border-b border-[#30363d] flex items-center justify-between bg-[#0d1117]">
              <h3 className="text-gray-200 font-bold text-lg">{tr('重命名分组', 'Rename Group')}</h3>
              <button onClick={() => setGroupRename(null)} className="text-gray-500 hover:text-gray-300">
                <X className="w-5 h-5" />
              </button>
            </div>
            <div className="p-6 space-y-4">
              <div>
                <div className="text-xs text-gray-500 mb-1">{tr('新分组名', 'New group name')}</div>
                <input
                  value={groupRename.newName}
                  onChange={(e) => setGroupRename({ ...groupRename, newName: e.target.value })}
                  className="w-full bg-[#0d1117] border border-[#30363d] rounded px-3 py-2 text-sm text-gray-200"
                />
              </div>
              <div className="flex justify-end gap-2">
                <button onClick={() => setGroupRename(null)} className="px-3 py-2 rounded-lg text-sm border border-[#30363d] bg-[#0d1117] text-gray-300 hover:bg-[#161b22]">{tr('取消', 'Cancel')}</button>
                <button
                  onClick={() => {
                    onRenameGroup(groupRename.oldName, groupRename.newName)
                    setGroupRename(null)
                  }}
                  disabled={!groupRename.newName.trim() || groupRename.newName.trim() === groupRename.oldName}
                  className="px-3 py-2 rounded-lg text-sm bg-[#21262d] hover:bg-[#30363d] border border-[#30363d] text-gray-200 disabled:opacity-60"
                >
                  {tr('保存', 'Save')}
                </button>
              </div>
            </div>
          </div>
        </div>
      )}

      {groupClearConfirm && (
        <div className="fixed inset-0 z-[70] flex items-center justify-center bg-black/70 backdrop-blur-sm">
          <div className="bg-[#161b22] border border-[#30363d] rounded-xl shadow-2xl w-[520px] overflow-hidden">
            <div className="px-6 py-4 border-b border-[#30363d] bg-[#0d1117]">
              <h3 className="text-gray-200 font-bold text-lg">{tr('确认清空分组', 'Confirm Clear Group')}</h3>
            </div>
            <div className="p-6 space-y-4">
              <p className="text-sm text-gray-300">
                {tr(`将清空分组 "${groupClearConfirm}" 下所有连接的分组信息，确认继续吗？`, `Clear group "${groupClearConfirm}" for all connections?`)}
              </p>
              <div className="flex justify-end gap-2">
                <button onClick={() => setGroupClearConfirm(null)} className="px-3 py-2 rounded-lg text-sm border border-[#30363d] bg-[#0d1117] text-gray-300 hover:bg-[#161b22]">{tr('取消', 'Cancel')}</button>
                <button
                  onClick={() => {
                    onClearGroup(groupClearConfirm)
                    setGroupClearConfirm(null)
                  }}
                  className="px-3 py-2 rounded-lg text-sm bg-orange-500/20 hover:bg-orange-500/30 border border-orange-400/30 text-orange-200"
                >
                  {tr('确认清空', 'Confirm')}
                </button>
              </div>
            </div>
          </div>
        </div>
      )}

      {groupCreate && (
        <div className="fixed inset-0 z-[70] flex items-center justify-center bg-black/70 backdrop-blur-sm">
          <div className="bg-[#161b22] border border-[#30363d] rounded-xl shadow-2xl w-[620px] overflow-hidden">
            <div className="px-6 py-4 border-b border-[#30363d] flex items-center justify-between bg-[#0d1117]">
              <h3 className="text-gray-200 font-bold text-lg">{tr('新建分组', 'New Group')}</h3>
              <button onClick={() => setGroupCreate(null)} className="text-gray-500 hover:text-gray-300">
                <X className="w-5 h-5" />
              </button>
            </div>
            <div className="p-6 space-y-4">
              <div>
                <div className="text-xs text-gray-500 mb-1">{tr('分组名', 'Group name')}</div>
                <input
                  value={groupCreate.name}
                  onChange={(e) => setGroupCreate({ ...groupCreate, name: e.target.value })}
                  placeholder={tr('例如：生产环境', 'e.g. Production')}
                  className="w-full bg-[#0d1117] border border-[#30363d] rounded px-3 py-2 text-sm text-gray-200"
                />
              </div>
              <div>
                <div className="text-xs text-gray-500 mb-2">{tr('选择要移动到该分组的连接', 'Select connections to move')}</div>
                <div className="max-h-48 overflow-auto space-y-1 pr-1">
                  {connections.map((c) => (
                    <label key={c.id} className="flex items-center gap-2 text-sm text-gray-300 px-2 py-1.5 rounded hover:bg-[#21262d]">
                      <input
                        type="checkbox"
                        checked={groupCreate.connIds.includes(c.id)}
                        onChange={(e) => {
                          const next = e.target.checked
                            ? [...groupCreate.connIds, c.id]
                            : groupCreate.connIds.filter((x) => x !== c.id)
                          setGroupCreate({ ...groupCreate, connIds: next })
                        }}
                      />
                      <span className="truncate">{String((c as any).name || c.id)}</span>
                    </label>
                  ))}
                </div>
              </div>
              <div className="flex justify-end gap-2">
                <button onClick={() => setGroupCreate(null)} className="px-3 py-2 rounded-lg text-sm border border-[#30363d] bg-[#0d1117] text-gray-300 hover:bg-[#161b22]">{tr('取消', 'Cancel')}</button>
                <button
                  onClick={() => {
                    onBatchMoveConnections(groupCreate.connIds, groupCreate.name.trim())
                    setGroupCreate(null)
                  }}
                  disabled={!groupCreate.name.trim() || groupCreate.connIds.length === 0}
                  className="px-3 py-2 rounded-lg text-sm bg-[#21262d] hover:bg-[#30363d] border border-[#30363d] text-gray-200 disabled:opacity-60"
                >
                  {tr('创建并移动', 'Create & Move')}
                </button>
              </div>
            </div>
          </div>
        </div>
      )}

      {showAdd && (
        <div className="fixed inset-0 z-[60] flex items-center justify-center bg-black/70 backdrop-blur-sm">
          <div className="bg-[#161b22] border border-[#30363d] rounded-xl shadow-2xl w-[720px] overflow-hidden">
            <div className="px-6 py-4 border-b border-[#30363d] flex items-center justify-between bg-[#0d1117]">
              <h3 className="text-gray-200 font-bold text-lg">{tr('新建连接', 'New Connection')}</h3>
              <button
                onClick={() => {
                  setShowAdd(false)
                  setDraftName('')
                  setDraftUrl('')
                }}
                className="text-gray-500 hover:text-gray-300"
              >
                <X className="w-5 h-5" />
              </button>
            </div>
            <div className="p-6 space-y-4">
              <div className="grid grid-cols-2 gap-4">
                <div>
                  <div className="text-xs text-gray-500 mb-1">Name (名称)</div>
                  <input
                    value={draftName}
                    onChange={(e) => setDraftName(e.target.value)}
                    placeholder="e.g. My Prod DB"
                    className="w-full bg-[#0d1117] border border-[#30363d] rounded px-3 py-2 text-sm text-gray-200"
                  />
                </div>
                <div>
                  <div className="text-xs text-gray-500 mb-1">{tr('连接地址', 'Connection URL')}</div>
                  <input
                    value={draftUrl}
                    onChange={(e) => setDraftUrl(e.target.value)}
                    placeholder="mysql://user:pass@host:3306/db"
                    className="w-full bg-[#0d1117] border border-[#30363d] rounded px-3 py-2 text-sm text-gray-200"
                  />
                </div>
              </div>
              <div className="flex justify-end gap-2">
                <button
                  onClick={() => {
                    setShowAdd(false)
                    setDraftName('')
                    setDraftUrl('')
                  }}
                  className="px-3 py-2 rounded-lg text-sm border border-[#30363d] bg-[#0d1117] text-gray-300 hover:bg-[#161b22]"
                >
                  {tr('取消', 'Cancel')}
                </button>
                <button
                  onClick={() => {
                    onAddConnection(draftName, draftUrl)
                    setShowAdd(false)
                    setDraftName('')
                    setDraftUrl('')
                  }}
                  disabled={!draftUrl.trim()}
                  className="px-3 py-2 rounded-lg text-sm bg-[#21262d] hover:bg-[#30363d] border border-[#30363d] text-gray-200 disabled:opacity-60"
                >
                  {tr('添加', 'Add')}
                </button>
              </div>
            </div>
          </div>
        </div>
      )}

      {editingConnId && (
        <div className="fixed inset-0 z-[60] flex items-center justify-center bg-black/70 backdrop-blur-sm">
          <div className="bg-[#161b22] border border-[#30363d] rounded-xl shadow-2xl w-[720px] overflow-hidden">
            <div className="px-6 py-4 border-b border-[#30363d] flex items-center justify-between bg-[#0d1117]">
              <h3 className="text-gray-200 font-bold text-lg">{tr('编辑连接', 'Edit Connection')}</h3>
              <button onClick={() => setEditingConnId(null)} className="text-gray-500 hover:text-gray-300">
                <X className="w-5 h-5" />
              </button>
            </div>
            <div className="p-6 space-y-4">
              <div className="flex items-center gap-2 border-b border-[#30363d] pb-2">
                <button
                  onClick={() => setEditTab('general')}
                  className={`px-3 py-1.5 rounded text-sm ${editTab === 'general' ? 'bg-blue-500/20 text-blue-300 border border-blue-500/30' : 'text-gray-400 hover:text-gray-200'}`}
                >
                  {tr('常规', 'General')}
                </button>
                <button
                  onClick={() => setEditTab('advanced')}
                  className={`px-3 py-1.5 rounded text-sm ${editTab === 'advanced' ? 'bg-blue-500/20 text-blue-300 border border-blue-500/30' : 'text-gray-400 hover:text-gray-200'}`}
                >
                  {tr('高级', 'Advanced')}
                </button>
              </div>

              {editTab === 'general' ? (
                <>
                  <div className="grid grid-cols-2 gap-4">
                    <div>
                      <div className="text-xs text-gray-500 mb-1">{tr('名称', 'Name')}</div>
                      <input value={editName} onChange={(e) => setEditName(e.target.value)} className="w-full bg-[#0d1117] border border-[#30363d] rounded px-3 py-2 text-sm text-gray-200" />
                    </div>
                    <div>
                      <div className="text-xs text-gray-500 mb-1">{tr('连接地址', 'Connection URL')}</div>
                      <input ref={editUrlRef} value={editUrl} onChange={(e) => setEditUrl(e.target.value)} className="w-full bg-[#0d1117] border border-[#30363d] rounded px-3 py-2 text-sm text-gray-200" />
                    </div>
                  </div>
                  <div className="grid grid-cols-2 gap-4">
                    <div>
                      <div className="text-xs text-gray-500 mb-1">{tr('分组', 'Group')}</div>
                      <input value={editGroup} onChange={(e) => setEditGroup(e.target.value)} placeholder={tr('例如：生产环境', 'e.g. Production')} className="w-full bg-[#0d1117] border border-[#30363d] rounded px-3 py-2 text-sm text-gray-200" />
                    </div>
                    <div>
                      <div className="text-xs text-gray-500 mb-1">{tr('颜色', 'Color')}</div>
                      <div className="flex items-center gap-2">
                        <input type="color" value={editColor} onChange={(e) => setEditColor(e.target.value)} className="w-10 h-10 p-1 bg-[#0d1117] border border-[#30363d] rounded" />
                        <input value={editColor} onChange={(e) => setEditColor(e.target.value)} className="flex-1 bg-[#0d1117] border border-[#30363d] rounded px-3 py-2 text-sm text-gray-200" />
                      </div>
                    </div>
                  </div>
                </>
              ) : (
                <>
                  <div className="bg-[#0d1117] border border-[#30363d] rounded-lg p-4">
                    <div className="text-xs text-gray-500 mb-2 uppercase tracking-wider font-bold">
                      {tr('连接向导', 'Connection Wizard')}
                    </div>
                    <div className="grid grid-cols-1 gap-3">
                      <div>
                        <div className="text-xs text-gray-400 mb-1.5">{tr('步骤 1：传输方式', 'Step 1: Transport')}</div>
                        <div className="flex flex-wrap gap-2">
                          <button
                            onClick={() => setEditSshEnabled(false)}
                            className={`px-2.5 py-1.5 rounded text-xs border transition-colors ${
                              !editSshEnabled
                                ? 'border-blue-500/40 bg-blue-500/15 text-blue-300'
                                : 'border-[#30363d] bg-[#0b0f14] text-gray-300 hover:bg-[#161b22]'
                            }`}
                          >
                            {tr('直连数据库', 'Direct Connection')}
                          </button>
                          <button
                            onClick={() => setEditSshEnabled(true)}
                            className={`px-2.5 py-1.5 rounded text-xs border transition-colors ${
                              editSshEnabled
                                ? 'border-blue-500/40 bg-blue-500/15 text-blue-300'
                                : 'border-[#30363d] bg-[#0b0f14] text-gray-300 hover:bg-[#161b22]'
                            }`}
                          >
                            {tr('经 SSH 隧道', 'Via SSH Tunnel')}
                          </button>
                        </div>
                      </div>
                      <div>
                        <div className="text-xs text-gray-400 mb-1.5">{tr('步骤 2：SSL 策略', 'Step 2: SSL Policy')}</div>
                        <div className="flex flex-wrap gap-2">
                          <button
                            onClick={() => {
                              setEditSslEnabled(false)
                              setEditSslMode('disabled')
                            }}
                            className={`px-2.5 py-1.5 rounded text-xs border transition-colors ${
                              !editSslEnabled || editSslMode === 'disabled'
                                ? 'border-blue-500/40 bg-blue-500/15 text-blue-300'
                                : 'border-[#30363d] bg-[#0b0f14] text-gray-300 hover:bg-[#161b22]'
                            }`}
                          >
                            {tr('禁用 SSL', 'Disable SSL')}
                          </button>
                          <button
                            onClick={() => {
                              setEditSslEnabled(true)
                              setEditSslMode('preferred')
                            }}
                            className={`px-2.5 py-1.5 rounded text-xs border transition-colors ${
                              editSslEnabled && editSslMode === 'preferred'
                                ? 'border-blue-500/40 bg-blue-500/15 text-blue-300'
                                : 'border-[#30363d] bg-[#0b0f14] text-gray-300 hover:bg-[#161b22]'
                            }`}
                          >
                            {tr('首选 SSL', 'Preferred SSL')}
                          </button>
                          <button
                            onClick={() => {
                              setEditSslEnabled(true)
                              setEditSslMode('required')
                            }}
                            className={`px-2.5 py-1.5 rounded text-xs border transition-colors ${
                              editSslEnabled && editSslMode === 'required'
                                ? 'border-blue-500/40 bg-blue-500/15 text-blue-300'
                                : 'border-[#30363d] bg-[#0b0f14] text-gray-300 hover:bg-[#161b22]'
                            }`}
                          >
                            {tr('强制 SSL', 'Required SSL')}
                          </button>
                        </div>
                      </div>
                      <div>
                        <div className="text-xs text-gray-400 mb-1.5">{tr('步骤 3：快速操作', 'Step 3: Quick Actions')}</div>
                        <div className="flex flex-wrap gap-2">
                          <button
                            onClick={() => testCurrentConnection()}
                            disabled={!editUrl.trim() || isTestingConnection}
                            className="px-2.5 py-1.5 rounded text-xs border border-blue-500/30 bg-blue-500/10 text-blue-300 hover:bg-blue-500/20 disabled:opacity-60"
                          >
                            {isTestingConnection ? tr('测试中...', 'Testing...') : tr('立即测试连接', 'Test Now')}
                          </button>
                          {testConnectionResult?.code === 'DB_TEST_SSL_FAILED' && (
                            <>
                              <button
                                onClick={() => applySslPresetAndRetest('preferred')}
                                disabled={!editUrl.trim() || isTestingConnection}
                                className="px-2.5 py-1.5 rounded text-xs border border-[#30363d] bg-[#0b0f14] text-gray-200 hover:bg-[#161b22] disabled:opacity-60"
                              >
                                {tr('一键改为 Preferred 并重测', 'Use Preferred & Retest')}
                              </button>
                              <button
                                onClick={() => applySslPresetAndRetest('disabled')}
                                disabled={!editUrl.trim() || isTestingConnection}
                                className="px-2.5 py-1.5 rounded text-xs border border-[#30363d] bg-[#0b0f14] text-gray-200 hover:bg-[#161b22] disabled:opacity-60"
                              >
                                {tr('一键禁用 SSL 并重测', 'Disable SSL & Retest')}
                              </button>
                            </>
                          )}
                        </div>
                      </div>
                    </div>
                  </div>
                  <div className="bg-[#0d1117] border border-[#30363d] rounded-lg p-4">
                    <label className="flex items-center gap-2 text-sm text-gray-200 mb-3">
                      <input type="checkbox" checked={editSshEnabled} onChange={(e) => setEditSshEnabled(e.target.checked)} />
                      {tr('启用 SSH 隧道', 'Enable SSH Tunnel')}
                    </label>
                    <div className="grid grid-cols-2 gap-3">
                      <input ref={editSshHostRef} value={editSshHost} onChange={(e) => setEditSshHost(e.target.value)} placeholder={tr('SSH Host', 'SSH Host')} disabled={!editSshEnabled} className="w-full bg-[#0b0f14] border border-[#30363d] rounded px-3 py-2 text-sm text-gray-200 disabled:opacity-50" />
                      <input value={editSshPort} onChange={(e) => setEditSshPort(e.target.value)} placeholder={tr('SSH Port', 'SSH Port')} disabled={!editSshEnabled} className="w-full bg-[#0b0f14] border border-[#30363d] rounded px-3 py-2 text-sm text-gray-200 disabled:opacity-50" />
                      <input ref={editSshUserRef} value={editSshUser} onChange={(e) => setEditSshUser(e.target.value)} placeholder={tr('SSH Username', 'SSH Username')} disabled={!editSshEnabled} className="w-full bg-[#0b0f14] border border-[#30363d] rounded px-3 py-2 text-sm text-gray-200 disabled:opacity-50" />
                      <input ref={editSshPasswordRef} value={editSshPassword} onChange={(e) => setEditSshPassword(e.target.value)} placeholder={tr('SSH Password', 'SSH Password')} type="password" disabled={!editSshEnabled} className="w-full bg-[#0b0f14] border border-[#30363d] rounded px-3 py-2 text-sm text-gray-200 disabled:opacity-50" />
                    </div>
                  </div>
                  <div className="bg-[#0d1117] border border-[#30363d] rounded-lg p-4">
                    <label className="flex items-center gap-2 text-sm text-gray-200 mb-3">
                      <input type="checkbox" checked={editSslEnabled} onChange={(e) => setEditSslEnabled(e.target.checked)} />
                      {tr('启用 SSL', 'Enable SSL')}
                    </label>
                    <select
                      ref={editSslModeRef}
                      value={editSslMode}
                      onChange={(e) => setEditSslMode(e.target.value)}
                      disabled={!editSslEnabled}
                      className="w-full bg-[#0b0f14] border border-[#30363d] rounded px-3 py-2 text-sm text-gray-200 disabled:opacity-50"
                    >
                      <option value="disabled">{tr('禁用', 'Disabled')}</option>
                      <option value="preferred">{tr('首选', 'Preferred')}</option>
                      <option value="required">{tr('必需', 'Required')}</option>
                      <option value="verify_ca">{tr('验证 CA', 'Verify CA')}</option>
                    </select>
                  </div>
                </>
              )}
              {testConnectionResult && (
                <div
                  className={`text-xs rounded border px-3 py-2 ${
                    testConnectionResult.status === 'success'
                      ? 'border-green-500/40 bg-green-500/10 text-green-300'
                      : testConnectionResult.status === 'warning'
                        ? 'border-yellow-500/40 bg-yellow-500/10 text-yellow-300'
                        : 'border-red-500/40 bg-red-500/10 text-red-300'
                  }`}
                >
                  <div className="font-medium">
                    [{testConnectionResult.code || 'DB_TEST'}] {testConnectionResult.message}
                  </div>
                  {testConnectionResult.hint && (
                    <div className="mt-1 opacity-90">
                      {tr('建议：', 'Hint: ')}
                      {testConnectionResult.hint}
                    </div>
                  )}
                  {testConnectionAdvice && (
                    <div className="mt-2 rounded border border-white/10 bg-black/20 px-2 py-2">
                      <div className="font-medium opacity-95">{testConnectionAdvice.title}</div>
                      {testConnectionAdvice.actions.map((item, idx) => (
                        <div key={`${testConnectionResult.code || 'advice'}-${idx}`} className="mt-1 opacity-85">
                          {idx + 1}. {item}
                        </div>
                      ))}
                    </div>
                  )}
                  <div className="mt-2 flex flex-wrap items-center gap-2">
                    {testConnectionResult.code === 'DB_TEST_SSL_FAILED' && (
                      <>
                        <button
                          onClick={() => applySslPresetAndRetest('preferred')}
                          disabled={!editUrl.trim() || isTestingConnection}
                          className="px-2 py-1 rounded border border-blue-400/30 bg-blue-500/15 text-blue-200 hover:bg-blue-500/25 disabled:opacity-60"
                        >
                          {tr('切到 Preferred 并重测', 'Switch to Preferred & Retest')}
                        </button>
                        <button
                          onClick={() => applySslPresetAndRetest('disabled')}
                          disabled={!editUrl.trim() || isTestingConnection}
                          className="px-2 py-1 rounded border border-blue-400/30 bg-blue-500/15 text-blue-200 hover:bg-blue-500/25 disabled:opacity-60"
                        >
                          {tr('禁用 SSL 并重测', 'Disable SSL & Retest')}
                        </button>
                      </>
                    )}
                    <button
                      onClick={() => copyDiagnostic()}
                      className="px-2 py-1 rounded border border-[#6b7280]/40 bg-[#1f2937]/50 text-gray-200 hover:bg-[#374151] flex items-center gap-1"
                    >
                      <Copy className="w-3.5 h-3.5" />
                      {copyDiagOk ? tr('已复制', 'Copied') : tr('复制诊断', 'Copy Diagnostic')}
                    </button>
                  </div>
                  {testConnectionResult.detail && (
                    <div className="mt-1 opacity-70 break-all">{testConnectionResult.detail}</div>
                  )}
                </div>
              )}
              <div className="flex justify-end gap-2">
                <button
                  onClick={() => {
                    testCurrentConnection()
                  }}
                  disabled={!editUrl.trim() || isTestingConnection}
                  className="mr-auto px-3 py-2 rounded-lg text-sm border border-blue-500/30 bg-blue-500/10 text-blue-300 hover:bg-blue-500/20 disabled:opacity-60"
                >
                  {isTestingConnection ? tr('测试中...', 'Testing...') : tr('测试连接', 'Test Connection')}
                </button>
                <button onClick={() => setEditingConnId(null)} className="px-3 py-2 rounded-lg text-sm border border-[#30363d] bg-[#0d1117] text-gray-300 hover:bg-[#161b22]">{tr('取消', 'Cancel')}</button>
                <button
                  onClick={() => {
                    onUpdateConnection(editingConnId, {
                      name: editName.trim() || 'Unnamed',
                      url: editUrl.trim(),
                      group_name: editGroup.trim() || null,
                      color: editColor.trim() || '#3b82f6',
                      ssh: {
                        enabled: editSshEnabled,
                        host: editSshHost.trim(),
                        port: Number(editSshPort || 22),
                        username: editSshUser.trim(),
                        password: editSshPassword,
                      },
                      ssl: {
                        enabled: editSslEnabled,
                        mode: editSslMode,
                      },
                    })
                    setEditingConnId(null)
                  }}
                  disabled={!editUrl.trim()}
                  className="px-3 py-2 rounded-lg text-sm bg-[#21262d] hover:bg-[#30363d] border border-[#30363d] text-gray-200 disabled:opacity-60"
                >
                  {tr('保存', 'Save')}
                </button>
              </div>
            </div>
          </div>
        </div>
      )}
    </div>
  )
}
