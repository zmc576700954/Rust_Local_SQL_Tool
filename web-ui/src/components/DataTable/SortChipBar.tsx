import { ArrowDown, ArrowUp, Settings, X } from 'lucide-react'
import type { SortRule } from '../SortPanel/types'
import { tr } from '../../i18n'

interface Props {
  sorts: SortRule[]
  setSorts: (sorts: SortRule[]) => void
  onOpenPanel?: () => void
}

export function SortChipBar({ sorts, setSorts, onOpenPanel }: Props) {
  if (sorts.length === 0) return null

  const toggleDir = (idx: number) =>
    setSorts(sorts.map((s, i) => (i === idx ? { ...s, desc: !s.desc } : s)))
  const remove = (idx: number) =>
    setSorts(sorts.filter((_, i) => i !== idx))

  return (
    <div className="flex items-center gap-1 flex-wrap px-2 py-1 bg-[#0d1117]/60 border-b border-[#30363d] text-[11px]">
      <span className="text-gray-500">{tr('排序', 'Sort')}:</span>
      {sorts.map((s, idx) => {
        const label = s.kind === 'column' ? (s.column || '?') : `f(${(s.expression || '').slice(0, 16)}…)`
        return (
          <span key={s.id} className="inline-flex items-center gap-1 pl-2 pr-1 py-0.5 rounded-full bg-purple-500/15 border border-purple-500/30 text-purple-100">
            <span className="opacity-70">{idx + 1}.</span>
            <button type="button" onClick={() => toggleDir(idx)}
              className="inline-flex items-center gap-1 hover:underline">
              <span>{label}</span>
              {s.desc ? <ArrowDown className="w-3 h-3" /> : <ArrowUp className="w-3 h-3" />}
            </button>
            <button type="button" onClick={() => remove(idx)}
              className="text-purple-200/70 hover:text-white p-0.5 rounded hover:bg-purple-500/30"
              title={tr('删除', 'Delete')}>
              <X className="w-3 h-3" />
            </button>
          </span>
        )
      })}
      {onOpenPanel && (
        <button type="button" onClick={onOpenPanel}
          className="ml-1 inline-flex items-center gap-1 text-purple-300 hover:text-white">
          <Settings className="w-3 h-3" />
          {tr('排序面板', 'Sort panel')}
        </button>
      )}
    </div>
  )
}
