import { X } from 'lucide-react'
import { useState, useEffect } from 'react'

export interface TabItem {
  id: string
  title: string
  type: 'query' | 'table' | 'explain' | 'query-builder' | 'ai-training' | 'go-live-reports' | 'go-live-audit' | 'advanced-center'
  payload?: any
}

export function Tabs({ 
  tabs, 
  activeTabId, 
  onTabClick, 
  onTabClose,
  onTabCloseOthers,
  onTabCloseAll,
  onTabAdd
}: { 
  tabs: TabItem[]
  activeTabId: string
  onTabClick: (id: string) => void
  onTabClose: (id: string) => void
  onTabCloseOthers: (id: string) => void
  onTabCloseAll: () => void
  onTabAdd?: () => void
}) {
  const [contextMenu, setContextMenu] = useState<{ x: number, y: number, tabId: string } | null>(null)

  useEffect(() => {
    const handleOutsideClick = () => setContextMenu(null)
    window.addEventListener('click', handleOutsideClick)
    return () => window.removeEventListener('click', handleOutsideClick)
  }, [])

  return (
    <div className="relative">
      <div className="flex bg-dark-panel border-b border-dark-border h-10 overflow-x-auto no-scrollbar items-center px-1">
        {tabs.map((tab) => (
          <div
            key={tab.id}
            onClick={() => onTabClick(tab.id)}
            onContextMenu={(e) => {
              e.preventDefault()
              setContextMenu({ x: e.clientX, y: e.clientY, tabId: tab.id })
            }}
            className={`group flex items-center h-8 px-3 mx-0.5 rounded-t-md border border-b-0 cursor-pointer min-w-[120px] max-w-[200px] select-none ${
              activeTabId === tab.id
                ? 'bg-[#0a0c10] border-dark-border text-blue-400'
                : 'bg-transparent border-transparent text-gray-400 hover:bg-[#161b22] hover:text-gray-200'
            }`}
          >
            <span className="truncate flex-1 text-sm">{tab.title}</span>
            {tabs.length > 1 && (
              <button
                onClick={(e) => {
                  e.stopPropagation()
                  onTabClose(tab.id)
                }}
                className="ml-2 p-0.5 rounded opacity-0 group-hover:opacity-100 hover:bg-gray-700/50 transition-all"
              >
                <X className="w-3 h-3" />
              </button>
            )}
          </div>
        ))}
        {onTabAdd && (
          <button
            onClick={onTabAdd}
            className="flex items-center justify-center h-8 w-8 mx-1 rounded text-gray-400 hover:bg-[#161b22] hover:text-gray-200 transition-colors shrink-0"
            title="New Query Tab"
          >
            <svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
              <line x1="12" y1="5" x2="12" y2="19"></line>
              <line x1="5" y1="12" x2="19" y2="12"></line>
            </svg>
          </button>
        )}
      </div>

      {contextMenu && (
        <div 
          className="fixed bg-[#161b22] border border-[#30363d] rounded-md shadow-2xl py-1 z-[100] min-w-[150px]"
          style={{ top: contextMenu.y, left: contextMenu.x }}
        >
          <button
            className="w-full text-left px-4 py-2 text-sm text-gray-300 hover:bg-blue-500 hover:text-white transition-colors"
            onClick={(e) => {
              e.stopPropagation()
              onTabClose(contextMenu.tabId)
              setContextMenu(null)
            }}
          >
            Close
          </button>
          <button
            className="w-full text-left px-4 py-2 text-sm text-gray-300 hover:bg-blue-500 hover:text-white transition-colors"
            onClick={(e) => {
              e.stopPropagation()
              onTabCloseOthers(contextMenu.tabId)
              setContextMenu(null)
            }}
          >
            Close Others
          </button>
          <button
            className="w-full text-left px-4 py-2 text-sm text-gray-300 hover:bg-blue-500 hover:text-white transition-colors"
            onClick={(e) => {
              e.stopPropagation()
              onTabCloseAll()
              setContextMenu(null)
            }}
          >
            Close All
          </button>
        </div>
      )}
    </div>
  )
}
