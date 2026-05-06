import React, { useState, useEffect, useRef } from 'react'
import { motion, AnimatePresence } from 'framer-motion'
import { Sparkles, Clock, Table, Settings, Cpu, Zap, HeartPulse } from 'lucide-react'

export interface CommandPaletteProps {
  isOpen: boolean
  onClose: () => void
  query: string
  setQuery: (q: string) => void
  isGenerating: boolean
  handleGenerate: (q?: string) => void
  recentQueries: string[]
  schemaData: any
  dbUrl?: string | null
  aiProfiles?: any[]
  activeAiProfileId?: string
  aiModels?: any[]
  activeModelId?: string
  activeTier?: string
  onAction: (actionType: 'query' | 'table' | 'settings' | 'ai_profile' | 'ai_model' | 'ai_tier' | 'ai_health', payload: any) => void
}

export function CommandPalette({
  isOpen,
  onClose,
  query,
  setQuery,
  isGenerating,
  handleGenerate,
  recentQueries,
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

  // Parse items based on query
  const q = query.trim().toLowerCase()
  
  // Fuzzy match simple helper
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

  const items: Array<{ type: 'query' | 'table' | 'settings' | 'ai_profile' | 'ai_model' | 'ai_tier' | 'ai_health', label: string, icon: any, payload: any }> = []

  // Recent queries
  recentQueries.forEach(rq => {
    if (fuzzyMatch(rq, q)) {
      items.push({ type: 'query', label: rq, icon: Clock, payload: rq })
    }
  })

  // Tables
  if (schemaData?.tables) {
    schemaData.tables.forEach((t: any) => {
      if (fuzzyMatch(t.table_name, q)) {
        items.push({ type: 'table', label: t.table_name, icon: Table, payload: t.table_name })
      }
    })
  }

  // Settings
  if (fuzzyMatch('Settings', q)) {
    items.push({ type: 'settings', label: 'Settings', icon: Settings, payload: null })
  }

  // AI Health
  if (fuzzyMatch('AI Health', q) || fuzzyMatch('Health Check', q) || fuzzyMatch('Health', q)) {
    items.push({ type: 'ai_health', label: 'AI Health Check', icon: HeartPulse, payload: null })
  }

  // AI Profile quick switch
  if (Array.isArray(aiProfiles)) {
    aiProfiles.forEach((p: any) => {
      const label = `Profile: ${p?.name || p?.id || ''}`
      if (fuzzyMatch(label, q) || fuzzyMatch(p?.id || '', q)) {
        const suffix = p?.id === activeAiProfileId ? ' (active)' : ''
        items.push({ type: 'ai_profile', label: `${label}${suffix}`, icon: Cpu, payload: p?.id })
      }
    })
  }

  // AI Model quick switch
  if (Array.isArray(aiModels)) {
    aiModels.forEach((m: any) => {
      const label = `Model: ${m?.display_name || m?.id || ''}`
      if (fuzzyMatch(label, q) || fuzzyMatch(m?.id || '', q)) {
        const suffix = m?.id === activeModelId ? ' (active)' : ''
        items.push({ type: 'ai_model', label: `${label}${suffix}`, icon: Zap, payload: m?.id })
      }
    })
  }

  // AI Tier quick switch
  ;['fast', 'balanced', 'high', 'ultra'].forEach((tier) => {
    const label = `Tier: ${tier}`
    if (fuzzyMatch(label, q)) {
      const suffix = tier === activeTier ? ' (active)' : ''
      items.push({ type: 'ai_tier', label: `${label}${suffix}`, icon: Cpu, payload: tier })
    }
  })

  // Reset selected index when items change
  useEffect(() => {
    setSelectedIndex(0)
  }, [q, items.length])

  // If query is not empty, the first item should probably be the "Ask AI" itself
  // But to support standard command palette, we can treat the "Ask AI" as a special action
  const showAskAI = q.length > 0
  const totalItems = showAskAI ? items.length + 1 : items.length

  const handleKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === 'ArrowDown') {
      e.preventDefault()
      setSelectedIndex(prev => (prev + 1) % totalItems)
    } else if (e.key === 'ArrowUp') {
      e.preventDefault()
      setSelectedIndex(prev => (prev - 1 + totalItems) % totalItems)
    } else if (e.key === 'Enter' && !isGenerating) {
      e.preventDefault()
      if (showAskAI && selectedIndex === 0) {
        // Trigger AI Generate
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

  // Auto focus input
  useEffect(() => {
    if (isOpen) {
      setTimeout(() => inputRef.current?.focus(), 50)
    } else {
      setQuery('')
    }
  }, [isOpen, setQuery])

  // Determine dialect
  let dialect = "General SQL";
  if (dbUrl) {
    if (dbUrl.startsWith('mysql')) dialect = "MySQL";
    else if (dbUrl.startsWith('postgres')) dialect = "PostgreSQL";
    else if (dbUrl.startsWith('sqlite')) dialect = "SQLite";
    else if (dbUrl.startsWith('redis')) dialect = "Redis";
    else if (dbUrl.startsWith('mongo')) dialect = "MongoDB";
  }

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
                  <span className="text-lg">🤖</span>
                  <span className="text-xs font-bold text-blue-400 tracking-wide uppercase bg-blue-500/10 px-2 py-0.5 rounded border border-blue-500/20">
                    {dialect} Agent is ready
                  </span>
                </div>
                <div className="text-[10px] text-gray-500 flex items-center gap-1">
                  Context: <span className="text-gray-400">{schemaData?.tables?.length || 0} tables loaded</span>
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
                    placeholder="Ask AI to write SQL, search tables, or commands..."
                    className="flex-1 bg-transparent border-none outline-none text-gray-100 text-lg placeholder-gray-500 font-sans"
                    value={query}
                    onChange={(e) => setQuery(e.target.value)}
                    onKeyDown={handleKeyDown}
                    disabled={isGenerating}
                  />
                </div>
                {isGenerating && (
                  <span className="text-xs text-dark-accent font-medium animate-pulse">Generating...</span>
                )}
              </div>

              {isGenerating ? (
                <div className="px-4 pb-4">
                  <div className="h-[2px] w-full bg-dark-border rounded overflow-hidden relative">
                    <motion.div 
                      className="absolute top-0 bottom-0 left-0 bg-dark-accent"
                      initial={{ width: "0%", x: "0%" }}
                      animate={{ 
                        width: ["0%", "30%", "100%"],
                        x: ["0%", "100%", "200%"]
                      }}
                      transition={{ 
                        duration: 1.5, 
                        ease: "easeInOut",
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
                  <div className="border-t border-[#30363d] max-h-[300px] overflow-y-auto pb-2">
                    {showAskAI && (
                      <div 
                        className={`px-4 py-2.5 flex items-center gap-3 cursor-pointer transition-colors text-gray-300 text-sm group ${selectedIndex === 0 ? 'bg-[#21262d] text-white' : 'hover:bg-[#21262d]'}`}
                        onClick={() => handleGenerate()}
                      >
                        <Sparkles className={`w-4 h-4 ${selectedIndex === 0 ? 'text-blue-400' : 'text-gray-500'}`} />
                        <span className="flex-1">Ask AI to generate SQL for: <span className="font-medium text-white">"{query}"</span></span>
                        <span className="text-[10px] text-gray-500">↵ to generate</span>
                      </div>
                    )}
                    
                    {items.length > 0 && (
                      <div className="px-4 py-2 text-xs font-semibold text-gray-500 uppercase tracking-wider sticky top-0 bg-[#161b22]/90 backdrop-blur-sm">
                        Results
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
                          <Icon className={`w-4 h-4 ${isSelected ? 'text-blue-400' : 'text-gray-500'}`} />
                          <div className="flex-1 flex items-center justify-between">
                            <span>{item.label}</span>
                            <span className="text-[10px] text-gray-500 uppercase tracking-wider">{item.type}</span>
                          </div>
                        </div>
                      )
                    })}
                    
                    {!showAskAI && items.length === 0 && (
                      <div className="px-4 py-6 text-center text-sm text-gray-500">
                        No results found
                      </div>
                    )}
                  </div>
                  
                  <div className="p-2.5 px-4 text-[11px] text-gray-500 flex justify-between bg-[#0d1117] border-t border-[#30363d]">
                    <div className="flex items-center gap-4">
                      <div className="flex items-center gap-1.5">
                        <span className="bg-dark-border px-1.5 py-0.5 rounded text-gray-300">↑</span>
                        <span className="bg-dark-border px-1.5 py-0.5 rounded text-gray-300">↓</span>
                        <span>to navigate</span>
                      </div>
                      <div className="flex items-center gap-1.5">
                        <span className="bg-dark-border px-1.5 py-0.5 rounded text-gray-300">↵</span>
                        <span>to select</span>
                      </div>
                    </div>
                    <div className="flex items-center gap-1.5">
                      <span className="bg-dark-border px-1.5 py-0.5 rounded text-gray-300">esc</span>
                      <span>to close</span>
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
