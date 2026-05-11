import React, { useEffect, useMemo, useRef, useState } from 'react'
import { motion, AnimatePresence } from 'framer-motion'
import { Sparkles, Clock, Table, Settings, Cpu, Zap, HeartPulse, BookMarked, Play } from 'lucide-react'
import { tr } from '../i18n'
import type { KnowledgeItem, SavedSqlBookmark } from '../types'

type CommandPaletteActionType =
  | 'query'
  | 'table'
  | 'settings'
  | 'ai_profile'
  | 'ai_model'
  | 'ai_tier'
  | 'ai_health'
  | 'snippet'
  | 'bookmark_open'
  | 'bookmark_run'

interface CommandPaletteItem {
  type: CommandPaletteActionType
  label: string
  icon: any
  payload: any
  badge: string
  description?: string
}

export interface CommandPaletteProps {
  isOpen: boolean
  onClose: () => void
  query: string
  setQuery: (q: string) => void
  isGenerating: boolean
  handleGenerate: (q?: string) => void
  recentQueries: string[]
  savedBookmarks?: SavedSqlBookmark[]
  smartSnippets?: KnowledgeItem[]
  schemaData: any
  dbUrl?: string | null
  aiProfiles?: any[]
  activeAiProfileId?: string
  aiModels?: any[]
  activeModelId?: string
  activeTier?: string
  onAction: (actionType: CommandPaletteActionType, payload: any) => void
}

const summarizeText = (value: string, maxLength: number = 88) => {
  const collapsed = value.replace(/\s+/g, ' ').trim()
  if (collapsed.length <= maxLength) return collapsed
  return `${collapsed.slice(0, maxLength)}…`
}

export function CommandPalette({
  isOpen,
  onClose,
  query,
  setQuery,
  isGenerating,
  handleGenerate,
  recentQueries,
  savedBookmarks = [],
  smartSnippets = [],
  schemaData,
  dbUrl,
  aiProfiles,
  activeAiProfileId,
  aiModels,
  activeModelId,
  activeTier,
  onAction
}: CommandPaletteProps) {
  const [selectedIndex, setSelectedIndex] = useState(0)
  const inputRef = useRef<HTMLInputElement>(null)
  const q = query.trim().toLowerCase()

  const fuzzyMatch = (str: string, pattern: string) => {
    if (!pattern) return true
    let pIdx = 0
    const s = str.toLowerCase()
    for (let i = 0; i < s.length; i++) {
      if (s[i] === pattern[pIdx]) {
        pIdx++
      }
      if (pIdx === pattern.length) return true
    }
    return false
  }

  const matchesQuery = (...values: Array<string | null | undefined>) => {
    if (!q) return true
    return values.some((value) => {
      const normalized = String(value || '').replace(/\s+/g, ' ').trim().toLowerCase()
      if (!normalized) return false
      return normalized.includes(q) || fuzzyMatch(normalized, q)
    })
  }

  const items = useMemo<CommandPaletteItem[]>(() => {
    const next: CommandPaletteItem[] = []

    savedBookmarks
      .slice()
      .sort((a, b) => (b.updated_at || 0) - (a.updated_at || 0))
      .filter((bookmark) => matchesQuery(
        bookmark.title,
        bookmark.sql,
        bookmark.description || '',
        bookmark.db_label || '',
        'bookmark saved query favorite run open'
      ))
      .slice(0, 8)
      .forEach((bookmark) => {
        const description = bookmark.db_label
          ? `${bookmark.db_label} · ${summarizeText(bookmark.sql, 72)}`
          : summarizeText(bookmark.sql, 72)

        next.push({
          type: 'bookmark_open',
          label: `${tr('书签', 'Bookmark')}: ${bookmark.title}`,
          icon: BookMarked,
          payload: { sql: bookmark.sql, title: bookmark.title },
          badge: tr('打开', 'Open'),
          description,
        })

        if (dbUrl) {
          next.push({
            type: 'bookmark_run',
            label: `${tr('运行书签', 'Run bookmark')}: ${bookmark.title}`,
            icon: Play,
            payload: { sql: bookmark.sql, title: bookmark.title },
            badge: tr('执行', 'Run'),
            description,
          })
        }
      })

    smartSnippets
      .slice()
      .sort((a, b) => {
        const goldenDelta = Number(Boolean(b.is_golden)) - Number(Boolean(a.is_golden))
        if (goldenDelta !== 0) return goldenDelta
        return (b.updated_at || 0) - (a.updated_at || 0)
      })
      .filter((snippet) => matchesQuery(
        snippet.title,
        snippet.content,
        snippet.description || '',
        snippet.is_golden ? 'golden snippet' : 'snippet'
      ))
      .slice(0, 8)
      .forEach((snippet) => {
        next.push({
          type: 'snippet',
          label: `${tr('片段', 'Snippet')}: ${snippet.title}`,
          icon: Sparkles,
          payload: { sql: snippet.content, title: snippet.title },
          badge: snippet.is_golden ? tr('黄金', 'Golden') : tr('插入', 'Insert'),
          description: snippet.description || summarizeText(snippet.content, 72),
        })
      })

    recentQueries.forEach((recentQuery) => {
      if (matchesQuery(recentQuery)) {
        next.push({
          type: 'query',
          label: recentQuery,
          icon: Clock,
          payload: recentQuery,
          badge: tr('历史', 'Recent'),
        })
      }
    })

    if (schemaData?.tables) {
      schemaData.tables.forEach((table: any) => {
        if (matchesQuery(table.table_name)) {
          next.push({
            type: 'table',
            label: table.table_name,
            icon: Table,
            payload: table.table_name,
            badge: tr('表', 'Table'),
          })
        }
      })
    }

    if (matchesQuery('settings', '设置')) {
      next.push({
        type: 'settings',
        label: tr('设置', 'Settings'),
        icon: Settings,
        payload: null,
        badge: tr('应用', 'App'),
      })
    }

    if (matchesQuery('ai health', 'health check', 'health', '健康检查')) {
      next.push({
        type: 'ai_health',
        label: tr('AI 健康检查', 'AI Health Check'),
        icon: HeartPulse,
        payload: null,
        badge: tr('状态', 'Health'),
      })
    }

    if (Array.isArray(aiProfiles)) {
      aiProfiles.forEach((profile: any) => {
        const label = `${tr('配置', 'Profile')}: ${profile?.name || profile?.id || ''}`
        if (matchesQuery(label, profile?.id || '')) {
          const suffix = profile?.id === activeAiProfileId ? tr('（当前）', ' (active)') : ''
          next.push({
            type: 'ai_profile',
            label: `${label}${suffix}`,
            icon: Cpu,
            payload: profile?.id,
            badge: tr('AI', 'AI'),
          })
        }
      })
    }

    if (Array.isArray(aiModels)) {
      aiModels.forEach((model: any) => {
        const label = `${tr('模型', 'Model')}: ${model?.display_name || model?.id || ''}`
        if (matchesQuery(label, model?.id || '')) {
          const suffix = model?.id === activeModelId ? tr('（当前）', ' (active)') : ''
          next.push({
            type: 'ai_model',
            label: `${label}${suffix}`,
            icon: Zap,
            payload: model?.id,
            badge: tr('模型', 'Model'),
          })
        }
      })
    }

    ;['fast', 'balanced', 'high', 'ultra'].forEach((tier) => {
      const label = `${tr('等级', 'Tier')}: ${tier}`
      if (matchesQuery(label, tier)) {
        const suffix = tier === activeTier ? tr('（当前）', ' (active)') : ''
        next.push({
          type: 'ai_tier',
          label: `${label}${suffix}`,
          icon: Cpu,
          payload: tier,
          badge: tr('Tier', 'Tier'),
        })
      }
    })

    return next
  }, [
    activeAiProfileId,
    activeModelId,
    activeTier,
    aiModels,
    aiProfiles,
    q,
    recentQueries,
    savedBookmarks,
    dbUrl,
    schemaData,
    smartSnippets,
  ])

  useEffect(() => {
    setSelectedIndex(0)
  }, [q, items.length])

  const showAskAI = q.length > 0
  const totalItems = showAskAI ? items.length + 1 : items.length

  const handleKeyDown = (e: React.KeyboardEvent) => {
    if (totalItems === 0) return

    if (e.key === 'ArrowDown') {
      e.preventDefault()
      setSelectedIndex((prev) => (prev + 1) % totalItems)
    } else if (e.key === 'ArrowUp') {
      e.preventDefault()
      setSelectedIndex((prev) => (prev - 1 + totalItems) % totalItems)
    } else if (e.key === 'Enter' && !isGenerating) {
      e.preventDefault()
      if (showAskAI && selectedIndex === 0) {
        handleGenerate()
      } else {
        const itemIdx = showAskAI ? selectedIndex - 1 : selectedIndex
        if (itemIdx >= 0 && items[itemIdx]) {
          const item = items[itemIdx]
          onAction(item.type, item.payload)
        }
      }
    }
  }

  useEffect(() => {
    if (isOpen) {
      setTimeout(() => inputRef.current?.focus(), 50)
    } else {
      setQuery('')
    }
  }, [isOpen, setQuery])

  let dialect = tr('通用 SQL', 'General SQL')
  if (dbUrl) {
    if (dbUrl.startsWith('mysql')) dialect = 'MySQL'
    else if (dbUrl.startsWith('postgres')) dialect = 'PostgreSQL'
    else if (dbUrl.startsWith('sqlite')) dialect = 'SQLite'
    else if (dbUrl.startsWith('redis')) dialect = 'Redis'
    else if (dbUrl.startsWith('mongo')) dialect = 'MongoDB'
  }

  const tablesLoaded = schemaData?.tables?.length || 0

  return (
    <AnimatePresence>
      {isOpen && (
        <>
          <motion.div
            initial={{ opacity: 0 }}
            animate={{ opacity: 1 }}
            exit={{ opacity: 0 }}
            transition={{ duration: 0.15 }}
            className="absolute inset-0 bg-black/40 backdrop-blur-[2px] z-40"
            onClick={() => !isGenerating && onClose()}
          />

          <div className="absolute inset-0 flex items-start justify-center pt-32 z-50 pointer-events-none">
            <motion.div
              initial={{ opacity: 0, y: -20, scale: 0.98 }}
              animate={{ opacity: 1, y: 0, scale: 1 }}
              exit={{ opacity: 0, y: -10, scale: 0.98 }}
              transition={{ duration: 0.2, ease: [0.16, 1, 0.3, 1] }}
              className="w-[640px] bg-[#161b22] border border-[#30363d] rounded-xl shadow-2xl overflow-hidden flex flex-col pointer-events-auto tech-border"
            >
              <div className="bg-[#0d1117] border-b border-[#30363d] px-4 py-2 flex items-center justify-between">
                <div className="flex items-center gap-2">
                  <span className="text-lg font-semibold text-blue-400">AI</span>
                  <span className="text-xs font-bold text-blue-400 tracking-wide uppercase bg-blue-500/10 px-2 py-0.5 rounded border border-blue-500/20">
                    {tr(`${dialect} 命令面板已就绪`, `${dialect} palette is ready`)}
                  </span>
                </div>
                <div className="text-[10px] text-gray-500 flex items-center gap-1">
                  {tr('上下文', 'Context')}:
                  <span className="text-gray-400">
                    {tr(
                      `${tablesLoaded} 张表 · ${smartSnippets.length} 个片段 · ${savedBookmarks.length} 个书签`,
                      `${tablesLoaded} tables · ${smartSnippets.length} snippets · ${savedBookmarks.length} bookmarks`
                    )}
                  </span>
                </div>
              </div>

              <div className="p-4 flex items-center gap-3">
                <Sparkles className={`w-5 h-5 ${isGenerating ? 'text-dark-accent' : 'text-gray-400'}`} />
                <div className="flex items-center gap-2 flex-1">
                  <span className="text-sm font-medium text-gray-500 whitespace-nowrap border-r border-[#30363d] pr-2">
                    [{dialect}]
                  </span>
                  <input
                    ref={inputRef}
                    type="text"
                    placeholder={tr(
                      '让 AI 写 SQL、搜索表、片段或书签…',
                      'Ask AI, search tables, snippets, or bookmarks...'
                    )}
                    className="flex-1 bg-transparent border-none outline-none text-gray-100 text-lg placeholder-gray-500 font-sans"
                    value={query}
                    onChange={(e) => setQuery(e.target.value)}
                    onKeyDown={handleKeyDown}
                    disabled={isGenerating}
                  />
                </div>
                {isGenerating && (
                  <span className="text-xs text-dark-accent font-medium animate-pulse">{tr('生成中...', 'Generating...')}</span>
                )}
              </div>

              {isGenerating ? (
                <div className="px-4 pb-4">
                  <div className="h-[2px] w-full bg-dark-border rounded overflow-hidden relative">
                    <motion.div
                      className="absolute top-0 bottom-0 left-0 bg-dark-accent"
                      initial={{ width: '0%', x: '0%' }}
                      animate={{
                        width: ['0%', '30%', '100%'],
                        x: ['0%', '100%', '200%']
                      }}
                      transition={{
                        duration: 1.5,
                        ease: 'easeInOut',
                        repeat: Infinity
                      }}
                    />
                  </div>
                  <div className="mt-3 space-y-2">
                    <div className="h-2 w-3/4 bg-dark-border/50 rounded animate-pulse"></div>
                    <div className="h-2 w-1/2 bg-dark-border/50 rounded animate-pulse delay-75"></div>
                  </div>
                </div>
              ) : (
                <>
                  <div className="border-t border-[#30363d] max-h-[320px] overflow-y-auto pb-2">
                    {showAskAI && (
                      <div
                        className={`px-4 py-2.5 flex items-center gap-3 cursor-pointer transition-colors text-gray-300 text-sm group ${selectedIndex === 0 ? 'bg-[#21262d] text-white' : 'hover:bg-[#21262d]'}`}
                        onClick={() => handleGenerate()}
                      >
                        <Sparkles className={`w-4 h-4 ${selectedIndex === 0 ? 'text-blue-400' : 'text-gray-500'}`} />
                        <span className="flex-1">
                          {tr('让 AI 生成 SQL：', 'Ask AI to generate SQL for:')}{' '}
                          <span className="font-medium text-white">"{query}"</span>
                        </span>
                        <span className="text-[10px] text-gray-500">{tr('↵ 生成', '↵ generate')}</span>
                      </div>
                    )}

                    {items.length > 0 && (
                      <div className="px-4 py-2 text-xs font-semibold text-gray-500 uppercase tracking-wider sticky top-0 bg-[#161b22]/90 backdrop-blur-sm">
                        {tr('结果', 'Results')}
                      </div>
                    )}

                    {items.map((item, idx) => {
                      const actualIdx = showAskAI ? idx + 1 : idx
                      const isSelected = selectedIndex === actualIdx
                      const Icon = item.icon
                      return (
                        <div
                          key={`${item.type}-${item.label}-${idx}`}
                          className={`px-4 py-2 flex items-center gap-3 cursor-pointer transition-colors text-sm group ${isSelected ? 'bg-[#21262d] text-white' : 'text-gray-300 hover:bg-[#21262d]'}`}
                          onClick={() => onAction(item.type, item.payload)}
                        >
                          <Icon className={`w-4 h-4 shrink-0 ${isSelected ? 'text-blue-400' : 'text-gray-500'}`} />
                          <div className="flex-1 min-w-0">
                            <div className="flex items-center justify-between gap-3">
                              <span className="truncate">{item.label}</span>
                              <span className="text-[10px] text-gray-500 uppercase tracking-wider shrink-0">{item.badge}</span>
                            </div>
                            {item.description && (
                              <div className="text-xs text-gray-500 truncate mt-0.5">{item.description}</div>
                            )}
                          </div>
                        </div>
                      )
                    })}

                    {!showAskAI && items.length === 0 && (
                      <div className="px-4 py-6 text-center text-sm text-gray-500">
                        {tr('未找到结果', 'No results found')}
                      </div>
                    )}
                  </div>

                  <div className="p-2.5 px-4 text-[11px] text-gray-500 flex justify-between bg-[#0d1117] border-t border-[#30363d]">
                    <div className="flex items-center gap-4">
                      <div className="flex items-center gap-1.5">
                        <span className="bg-dark-border px-1.5 py-0.5 rounded text-gray-300">↑</span>
                        <span className="bg-dark-border px-1.5 py-0.5 rounded text-gray-300">↓</span>
                        <span>{tr('导航', 'to navigate')}</span>
                      </div>
                      <div className="flex items-center gap-1.5">
                        <span className="bg-dark-border px-1.5 py-0.5 rounded text-gray-300">↵</span>
                        <span>{tr('选择', 'to select')}</span>
                      </div>
                    </div>
                    <div className="flex items-center gap-1.5">
                      <span className="bg-dark-border px-1.5 py-0.5 rounded text-gray-300">esc</span>
                      <span>{tr('关闭', 'to close')}</span>
                    </div>
                  </div>
                </>
              )}
            </motion.div>
          </div>
        </>
      )}
    </AnimatePresence>
  )
}
