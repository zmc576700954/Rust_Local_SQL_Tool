import { useState } from 'react'
import { tr } from '../i18n'

export interface AgentStep {
  type: 'thinking' | 'tool_call' | 'tool_result' | 'sql_draft' | 'final_sql' | 'explanation' | 'error' | 'token_usage'
  text?: string
  tool?: string
  args?: unknown
  result?: string
  callId?: string
  sql?: string
  taskType?: string
  message?: string
  promptTokens?: number
  completionTokens?: number
  totalTokens?: number
}

interface AgentProcessPanelProps {
  steps: AgentStep[]
  isRunning: boolean
}

const toolLabels: Record<string, { zh: string; en: string }> = {
  query_schema: { zh: '查询表结构...', en: 'Querying table structure...' },
  execute_sql: { zh: '执行 SQL 验证...', en: 'Executing SQL to validate...' },
  query_rules: { zh: '搜索规则模式...', en: 'Searching rule patterns...' },
  query_knowledge: { zh: '搜索知识库...', en: 'Searching knowledge base...' },
}

export default function AgentProcessPanel({ steps, isRunning }: AgentProcessPanelProps) {
  const [expanded, setExpanded] = useState(true)

  if (steps.length === 0 && !isRunning) return null

  return (
    <div className="border border-neutral-200 dark:border-neutral-700 rounded-lg mb-3 overflow-hidden">
      <button
        onClick={() => setExpanded(!expanded)}
        className="w-full flex items-center justify-between px-3 py-2 bg-neutral-50 dark:bg-neutral-800 text-sm font-medium text-neutral-700 dark:text-neutral-300 hover:bg-neutral-100 dark:hover:bg-neutral-750 transition-colors"
      >
        <span className="flex items-center gap-2">
          {isRunning && (
            <span className="inline-block w-2 h-2 rounded-full bg-blue-500 animate-pulse" />
          )}
          {tr('Agent 处理过程', 'Agent Process')}
        </span>
        <span className="text-xs text-neutral-400">{expanded ? '▲' : '▼'}</span>
      </button>

      {expanded && (
        <div className="px-3 py-2 space-y-1.5 max-h-60 overflow-y-auto">
          {steps.map((step, i) => (
            <StepItem key={i} step={step} />
          ))}
          {isRunning && steps.length === 0 && (
            <div className="text-xs text-neutral-400 animate-pulse">
              {tr('思考中...', 'Thinking...')}
            </div>
          )}
        </div>
      )}
    </div>
  )
}

function StepItem({ step }: { step: AgentStep }) {
  const [showDetail, setShowDetail] = useState(false)

  switch (step.type) {
    case 'thinking':
      return step.text ? (
        <div className="text-xs text-neutral-500 dark:text-neutral-400 pl-2 border-l-2 border-neutral-200 dark:border-neutral-600">
          {step.text.length > 200 && !showDetail
            ? `${step.text.slice(0, 200)}... `
            : step.text}
          {step.text.length > 200 && (
            <button
              onClick={() => setShowDetail(!showDetail)}
              className="text-blue-500 hover:underline ml-1"
            >
              {showDetail ? tr('收起', 'less') : tr('展开', 'more')}
            </button>
          )}
        </div>
      ) : null

    case 'tool_call':
      return (
        <div className="flex items-center gap-2 text-xs">
          <span className="inline-block w-1.5 h-1.5 rounded-full bg-amber-400" />
          <span className="text-amber-600 dark:text-amber-400">
            {step.tool
              ? (toolLabels[step.tool]
                  ? tr(toolLabels[step.tool].zh, toolLabels[step.tool].en)
                  : tr(`调用 ${step.tool}...`, `Calling ${step.tool}...`))
              : tr('调用工具...', 'Calling tool...')}
          </span>
        </div>
      )

    case 'tool_result':
      return (
        <div className="text-xs text-neutral-400 dark:text-neutral-500 pl-4">
          {step.tool} {tr('结果已收到', 'result received')}
        </div>
      )

    case 'sql_draft':
      return (
        <div className="text-xs">
          <span className="text-neutral-400">{tr('SQL 草稿: ', 'SQL draft: ')}</span>
          <code className="text-amber-600 dark:text-amber-400 bg-amber-50 dark:bg-amber-900/20 px-1 rounded">
            {step.sql?.slice(0, 120)}{step.sql && step.sql.length > 120 ? '...' : ''}
          </code>
        </div>
      )

    case 'final_sql':
      return (
        <div className="flex items-center gap-2 text-xs">
          <span className="inline-block w-1.5 h-1.5 rounded-full bg-green-500" />
          <span className="text-green-600 dark:text-green-400 font-medium">
            {tr('SQL 已生成', 'SQL generated')}
          </span>
          {step.taskType && (
            <span className="text-neutral-400">({step.taskType})</span>
          )}
        </div>
      )

    case 'explanation':
      return step.text ? (
        <div className="text-xs text-neutral-600 dark:text-neutral-300 pl-2 border-l-2 border-green-300 dark:border-green-600">
          {step.text}
        </div>
      ) : null

    case 'token_usage':
      return (
        <div className="flex items-center gap-2 text-xs text-neutral-400 dark:text-neutral-500">
          <span className="inline-block w-1.5 h-1.5 rounded-full bg-neutral-300 dark:bg-neutral-600" />
          <span>
            {tr('Token 消耗', 'Token usage')}: {step.promptTokens?.toLocaleString()} → {step.completionTokens?.toLocaleString()} = {step.totalTokens?.toLocaleString()}
          </span>
        </div>
      )

    case 'error':
      return (
        <div className="flex items-center gap-2 text-xs">
          <span className="inline-block w-1.5 h-1.5 rounded-full bg-red-500" />
          <span className="text-red-600 dark:text-red-400">{step.message || tr('错误', 'Error')}</span>
        </div>
      )

    default:
      return null
  }
}
