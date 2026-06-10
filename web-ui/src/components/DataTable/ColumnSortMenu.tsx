import { useEffect, useRef } from 'react'
import type { SortRule } from '../SortPanel/types'
import { createRule } from '../SortPanel/helpers'
import { tr } from '../../i18n'

interface Props {
  column: string
  x: number
  y: number
  sorts: SortRule[]
  setSorts: (sorts: SortRule[]) => void
  onOpenPanel?: () => void
  onClose: () => void
}

export function ColumnSortMenu({ column, x, y, sorts, setSorts, onOpenPanel, onClose }: Props) {
  const ref = useRef<HTMLDivElement>(null)

  useEffect(() => {
    const handler = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) onClose()
    }
    const keyHandler = (e: KeyboardEvent) => { if (e.key === 'Escape') onClose() }
    document.addEventListener('mousedown', handler)
    document.addEventListener('keydown', keyHandler)
    return () => {
      document.removeEventListener('mousedown', handler)
      document.removeEventListener('keydown', keyHandler)
    }
  }, [onClose])

  const sortAsc = () => {
    setSorts([createRule({ column, desc: false })])
    onClose()
  }
  const sortDesc = () => {
    setSorts([createRule({ column, desc: true })])
    onClose()
  }
  const addToGroup = () => {
    const next = sorts.filter(s => !(s.kind === 'column' && s.column === column))
    next.push(createRule({ column, desc: false }))
    setSorts(next)
    onClose()
  }
  const clearAll = () => {
    setSorts([])
    onClose()
  }
  const openPanel = () => {
    onOpenPanel?.()
    onClose()
  }

  return (
    <div
      ref={ref}
      style={{ left: x, top: y }}
      className="fixed z-[60] min-w-[180px] bg-[#161b22] border border-[#30363d] rounded shadow-xl py-1 text-xs text-gray-200"
      role="menu"
    >
      <button type="button" onClick={sortAsc}
        className="w-full text-left px-3 py-1.5 hover:bg-[#21262d]">
        {tr('按此列升序', 'Sort ascending by this column')}
      </button>
      <button type="button" onClick={sortDesc}
        className="w-full text-left px-3 py-1.5 hover:bg-[#21262d]">
        {tr('按此列降序', 'Sort descending by this column')}
      </button>
      <button type="button" onClick={addToGroup}
        className="w-full text-left px-3 py-1.5 hover:bg-[#21262d]">
        {tr('加入排序组合', 'Add to sort group')}
      </button>
      <div className="h-px bg-[#30363d] my-1" />
      {onOpenPanel && (
        <button type="button" onClick={openPanel}
          className="w-full text-left px-3 py-1.5 hover:bg-[#21262d]">
          {tr('打开排序面板', 'Open sort panel')}…
        </button>
      )}
      <button type="button" onClick={clearAll}
        disabled={sorts.length === 0}
        className="w-full text-left px-3 py-1.5 hover:bg-[#21262d] disabled:opacity-40 disabled:hover:bg-transparent text-red-300">
        {tr('清除全部排序', 'Clear all sorts')}
      </button>
    </div>
  )
}
