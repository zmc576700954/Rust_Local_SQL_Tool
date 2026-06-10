import { ArrowDown, ArrowUp, ChevronDown, ChevronUp, Trash2 } from 'lucide-react'
import type { SortRule } from './types'
import { validateExpressionClient } from './helpers'
import { tr } from '../../i18n'

interface Props {
  rule: SortRule
  index: number
  total: number
  availableColumns: string[]
  onChange: (next: SortRule) => void
  onDelete: () => void
  onMoveUp: () => void
  onMoveDown: () => void
}

export function SortRuleRow({ rule, index, total, availableColumns, onChange, onDelete, onMoveUp, onMoveDown }: Props) {
  const isCol = rule.kind === 'column'
  const exprError = !isCol ? validateExpressionClient(rule.expression || '') : null

  return (
    <div className="border border-[#30363d] rounded p-2 bg-[#0d1117] space-y-2">
      <div className="flex items-center gap-1 text-xs text-gray-400">
        <span className="font-mono text-blue-300">#{index + 1}</span>
        <div className="flex-1" />
        <button
          type="button"
          onClick={onMoveUp}
          disabled={index === 0}
          title={tr('上移', 'Move up')}
          className="p-1 rounded hover:bg-[#21262d] disabled:opacity-30 disabled:hover:bg-transparent"
        >
          <ChevronUp className="w-3 h-3" />
        </button>
        <button
          type="button"
          onClick={onMoveDown}
          disabled={index === total - 1}
          title={tr('下移', 'Move down')}
          className="p-1 rounded hover:bg-[#21262d] disabled:opacity-30 disabled:hover:bg-transparent"
        >
          <ChevronDown className="w-3 h-3" />
        </button>
        <button
          type="button"
          onClick={onDelete}
          title={tr('删除', 'Delete')}
          className="p-1 rounded hover:bg-red-500/20 text-red-300"
        >
          <Trash2 className="w-3 h-3" />
        </button>
      </div>

      <div className="flex items-center gap-1 text-xs">
        <button
          type="button"
          onClick={() => onChange({ ...rule, kind: 'column', expression: undefined })}
          className={`px-2 py-0.5 rounded border ${isCol ? 'border-blue-500/50 bg-blue-500/10 text-blue-200' : 'border-[#30363d] text-gray-400 hover:border-[#30363d] hover:bg-[#21262d]'}`}
        >
          {tr('列', 'Column')}
        </button>
        <button
          type="button"
          onClick={() => onChange({ ...rule, kind: 'expression', column: undefined })}
          className={`px-2 py-0.5 rounded border ${!isCol ? 'border-blue-500/50 bg-blue-500/10 text-blue-200' : 'border-[#30363d] text-gray-400 hover:border-[#30363d] hover:bg-[#21262d]'}`}
        >
          {tr('表达式', 'Expression')}
        </button>
      </div>

      {isCol ? (
        <select
          value={rule.column || ''}
          onChange={(e) => onChange({ ...rule, column: e.target.value })}
          className="w-full bg-[#161b22] border border-[#30363d] rounded px-2 py-1 text-xs text-gray-200 focus:outline-none focus:border-blue-500/50"
        >
          <option value="">{tr('选择列…', 'Select column…')}</option>
          {availableColumns.map(c => (
            <option key={c} value={c}>{c}</option>
          ))}
        </select>
      ) : (
        <div>
          <input
            type="text"
            value={rule.expression || ''}
            placeholder={tr('输入排序表达式，如 LENGTH(name)', 'Enter sort expression, e.g. LENGTH(name)')}
            onChange={(e) => onChange({ ...rule, expression: e.target.value })}
            className={`w-full bg-[#161b22] border rounded px-2 py-1 text-xs text-gray-200 font-mono focus:outline-none ${exprError ? 'border-red-500/50' : 'border-[#30363d] focus:border-blue-500/50'}`}
          />
          {exprError && <div className="text-[10px] text-red-400 mt-1">{exprError}</div>}
        </div>
      )}

      <div className="flex items-center gap-1 text-xs">
        <button
          type="button"
          onClick={() => onChange({ ...rule, desc: false })}
          className={`flex items-center gap-1 px-2 py-0.5 rounded border ${!rule.desc ? 'border-blue-500/50 bg-blue-500/10 text-blue-200' : 'border-[#30363d] text-gray-400 hover:bg-[#21262d]'}`}
        >
          <ArrowUp className="w-3 h-3" />
          {tr('升序', 'Asc')}
        </button>
        <button
          type="button"
          onClick={() => onChange({ ...rule, desc: true })}
          className={`flex items-center gap-1 px-2 py-0.5 rounded border ${rule.desc ? 'border-blue-500/50 bg-blue-500/10 text-blue-200' : 'border-[#30363d] text-gray-400 hover:bg-[#21262d]'}`}
        >
          <ArrowDown className="w-3 h-3" />
          {tr('降序', 'Desc')}
        </button>
        <div className="flex-1" />
        <select
          value={rule.nulls}
          onChange={(e) => onChange({ ...rule, nulls: e.target.value as SortRule['nulls'] })}
          title={tr('空值位置', 'Nulls position')}
          className="bg-[#161b22] border border-[#30363d] rounded px-1 py-0.5 text-[10px] text-gray-300 focus:outline-none focus:border-blue-500/50"
        >
          <option value="default">{tr('默认', 'Default')}</option>
          <option value="first">{tr('空值在前', 'Nulls first')}</option>
          <option value="last">{tr('空值在后', 'Nulls last')}</option>
        </select>
      </div>
    </div>
  )
}
