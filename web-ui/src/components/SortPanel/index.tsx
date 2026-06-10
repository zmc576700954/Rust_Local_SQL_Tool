import { useEffect, useState } from 'react'
import { Plus, X } from 'lucide-react'
import type { SortRule } from './types'
import { createRule } from './helpers'
import { SortRuleRow } from './SortRuleRow'
import { tr } from '../../i18n'

interface Props {
  open: boolean
  initialRules: SortRule[]
  availableColumns: string[]
  title?: string
  onClose: () => void
  onApply: (rules: SortRule[]) => void
}

export function SortPanel({ open, initialRules, availableColumns, title, onClose, onApply }: Props) {
  const [rules, setRules] = useState<SortRule[]>(initialRules)

  useEffect(() => {
    if (open) setRules(initialRules)
  }, [open, initialRules])

  if (!open) return null

  const addRule = () => setRules(prev => [...prev, createRule({ column: availableColumns[0] })])
  const clearAll = () => setRules([])

  const handleChange = (idx: number, next: SortRule) =>
    setRules(prev => prev.map((r, i) => (i === idx ? next : r)))
  const handleDelete = (idx: number) =>
    setRules(prev => prev.filter((_, i) => i !== idx))
  const moveBy = (idx: number, delta: number) =>
    setRules(prev => {
      const next = [...prev]
      const target = idx + delta
      if (target < 0 || target >= next.length) return prev
      ;[next[idx], next[target]] = [next[target], next[idx]]
      return next
    })

  return (
    <>
      <div
        className="fixed inset-0 bg-black/30 z-40"
        onClick={onClose}
      />
      <aside
        className="fixed inset-y-0 right-0 w-[360px] bg-[#161b22] border-l border-[#30363d] shadow-2xl z-50 flex flex-col"
        role="dialog"
        aria-label={title || tr('排序面板', 'Sort panel')}
      >
        <header className="flex items-center justify-between px-3 py-2 border-b border-[#30363d]">
          <h3 className="text-sm font-medium text-gray-200">
            {title || tr('排序面板', 'Sort panel')}
          </h3>
          <button
            type="button"
            onClick={onClose}
            className="p-1 rounded hover:bg-[#21262d] text-gray-400 hover:text-white"
            aria-label={tr('取消', 'Cancel')}
          >
            <X className="w-4 h-4" />
          </button>
        </header>

        <div className="flex-1 overflow-y-auto p-3 space-y-2">
          {rules.length === 0 ? (
            <div className="text-xs text-gray-500 text-center py-6">
              {tr('暂无排序规则', 'No sort rules')}
            </div>
          ) : rules.map((r, idx) => (
            <SortRuleRow
              key={r.id}
              rule={r}
              index={idx}
              total={rules.length}
              availableColumns={availableColumns}
              onChange={(next) => handleChange(idx, next)}
              onDelete={() => handleDelete(idx)}
              onMoveUp={() => moveBy(idx, -1)}
              onMoveDown={() => moveBy(idx, 1)}
            />
          ))}
          <button
            type="button"
            onClick={addRule}
            className="w-full flex items-center justify-center gap-1 py-1.5 text-xs text-blue-300 border border-dashed border-[#30363d] hover:border-blue-500/40 rounded"
          >
            <Plus className="w-3 h-3" />
            {tr('添加排序', 'Add sort')}
          </button>
        </div>

        <footer className="flex items-center gap-2 px-3 py-2 border-t border-[#30363d]">
          <button
            type="button"
            onClick={clearAll}
            disabled={rules.length === 0}
            className="text-xs px-3 py-1 rounded border border-[#30363d] text-gray-300 hover:bg-[#21262d] disabled:opacity-40"
          >
            {tr('清空', 'Clear all')}
          </button>
          <div className="flex-1" />
          <button
            type="button"
            onClick={onClose}
            className="text-xs px-3 py-1 rounded border border-[#30363d] text-gray-300 hover:bg-[#21262d]"
          >
            {tr('取消', 'Cancel')}
          </button>
          <button
            type="button"
            onClick={() => onApply(rules)}
            className="text-xs px-3 py-1 rounded bg-blue-500/30 border border-blue-500/50 text-blue-100 hover:bg-blue-500/40"
          >
            {tr('应用', 'Apply')}
          </button>
        </footer>
      </aside>
    </>
  )
}
