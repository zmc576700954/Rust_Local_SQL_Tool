import React, { Suspense } from 'react'
import { AlignLeft, BookMarked, Command, MoreHorizontal, Play, Save, Server, Sparkles, X } from 'lucide-react'
import { motion } from 'framer-motion'
import { Skeleton } from './Skeleton'
import { TypingEffect } from './TypingEffect'
import { tr } from '../i18n'
import type { Monaco, MonacoEditor } from '../types'

const Editor = React.lazy(() => import('@monaco-editor/react'))
const GENERATED_SQL_PLACEHOLDER = '-- Generated SQL will appear here\n'

interface QueryEditorActionPanelState {
  sql: string
  lastQuery: string
  isGenerating: boolean
  isExecuting: boolean
  isCancelingExecution?: boolean
  transactionMode?: 'auto' | 'manual'
  transactionState?: 'idle' | 'active' | 'committing' | 'rolling_back'
  lastExplanation: string | null
}

interface QueryEditorActionPanelProps {
  tabState: QueryEditorActionPanelState
  dbType: string
  dbConnected: boolean
  isSavingRule: boolean
  isCompactActionBar: boolean
  showMoreActions: boolean
  resolveModelsList: any[]
  resolveActiveModelId: string
  resolveActiveTier: string
  activeModelSupportsTier: boolean
  isAiSwitching: boolean
  onSqlChange: (sql: string) => void
  onEditorMount: (editor: MonacoEditor, monaco: Monaco) => void
  onSaveRule: () => void
  onSaveBookmark: () => void
  onOpenCommandPalette: () => void
  onAiOptimize: () => void
  onAiExplain: () => void
  onExplainPlan: () => void
  onOpenSessionInfo: () => void
  onFormatSql: () => void
  onToggleMoreActions: () => void
  onCloseMoreActions: () => void
  onExecute: () => void
  onCancelExecution: () => void
  onTransactionModeChange: (mode: 'auto' | 'manual') => void
  onCommitTransaction: () => void
  onRollbackTransaction: () => void
  onModelChange: (modelId: string) => void
  onTierChange: (tier: string) => void
}

export function QueryEditorActionPanel({
  tabState,
  dbType,
  dbConnected,
  isSavingRule,
  isCompactActionBar,
  showMoreActions,
  resolveModelsList,
  resolveActiveModelId,
  resolveActiveTier,
  activeModelSupportsTier,
  isAiSwitching,
  onSqlChange,
  onEditorMount,
  onSaveRule,
  onSaveBookmark,
  onOpenCommandPalette,
  onAiOptimize,
  onAiExplain,
  onExplainPlan,
  onOpenSessionInfo,
  onFormatSql,
  onToggleMoreActions,
  onCloseMoreActions,
  onExecute,
  onCancelExecution,
  onTransactionModeChange,
  onCommitTransaction,
  onRollbackTransaction,
  onModelChange,
  onTierChange,
}: QueryEditorActionPanelProps) {
  const disableAiSqlActions =
    tabState.isGenerating || !tabState.sql.trim() || tabState.sql.trim() === GENERATED_SQL_PLACEHOLDER
  const disableBookmarkAction =
    tabState.isExecuting || !tabState.sql.trim() || tabState.sql.trim() === GENERATED_SQL_PLACEHOLDER.trim()
  const transactionMode = tabState.transactionMode || 'auto'
  const transactionState = tabState.transactionState || 'idle'
  const transactionBusy = transactionState === 'committing' || transactionState === 'rolling_back'
  const transactionActive = transactionState === 'active'

  return (
    <div className="flex-1 border-b border-dark-border relative group min-h-0">
      {tabState.lastQuery && tabState.sql && !tabState.isExecuting && (
        <motion.button
          initial={{ opacity: 0, y: 10 }}
          animate={{ opacity: 1, y: 0 }}
          whileHover={{ scale: 1.05 }}
          onClick={onSaveRule}
          disabled={isSavingRule}
          className="absolute top-4 right-4 z-10 hidden group-hover:flex items-center gap-1.5 px-3 py-1.5 bg-blue-500/10 hover:bg-blue-500/20 border border-blue-500/30 rounded text-xs text-blue-400 font-medium transition-all shadow-lg backdrop-blur-sm"
        >
          <Save className="w-3.5 h-3.5" />
          {isSavingRule ? 'Saving...' : 'Save as Rule'}
        </motion.button>
      )}

      {tabState.isGenerating && (
        <div className="absolute inset-0 z-20 bg-[#0a0c10]/80 backdrop-blur-sm p-6 flex flex-col gap-4 pt-12 pointer-events-none">
          <Skeleton className="h-6 w-3/4" />
          <Skeleton className="h-6 w-1/2" />
          <Skeleton className="h-6 w-2/3" />
          <Skeleton className="h-6 w-5/6" />
          <div className="flex items-center justify-center h-full text-blue-400 font-medium animate-pulse flex-col gap-3">
            <Sparkles className="w-8 h-8" />
            <span>{'AI \u6b63\u5728\u601d\u8003\u5e76\u751f\u6210 SQL...'}</span>
          </div>
        </div>
      )}

      <Suspense fallback={<div className="flex-1 flex items-center justify-center"><Skeleton className="h-full w-full" /></div>}>
        <Editor
          height="100%"
          defaultLanguage="sql"
          theme="vs-dark"
          value={tabState.sql}
          onChange={(val) => onSqlChange(val || '')}
          onMount={(editor, monaco) => onEditorMount(editor, monaco)}
          options={{
            minimap: { enabled: false },
            fontSize: 14,
            fontFamily: "'JetBrains Mono', 'Fira Code', monospace",
            padding: { top: 24 },
            scrollBeyondLastLine: false,
            smoothScrolling: true,
            cursorBlinking: 'smooth',
          }}
        />
      </Suspense>

      <div className="absolute bottom-4 right-4 flex gap-3 items-center whitespace-nowrap max-w-[calc(100%-2rem)] overflow-x-auto py-1">
        <span className="text-xs font-medium text-blue-400 bg-blue-500/10 px-2 py-1 rounded border border-blue-500/20 mr-2 shadow-sm whitespace-nowrap">
          {dbType} Agent
        </span>

        <div className="flex items-center bg-[#21262d] rounded overflow-hidden border border-[#30363d]">
          <button
            onClick={() => onTransactionModeChange('auto')}
            disabled={tabState.isExecuting || transactionBusy}
            className={`px-2 py-1 text-xs transition-colors disabled:opacity-60 ${transactionMode === 'auto' ? 'bg-blue-500/20 text-blue-400' : 'text-gray-400 hover:bg-[#30363d]'}`}
          >
            Auto Commit
          </button>
          <button
            onClick={() => onTransactionModeChange('manual')}
            disabled={tabState.isExecuting || transactionBusy}
            className={`px-2 py-1 text-xs transition-colors disabled:opacity-60 ${transactionMode === 'manual' ? 'bg-amber-500/20 text-amber-300' : 'text-gray-400 hover:bg-[#30363d]'}`}
          >
            Manual Tx
          </button>
        </div>

        {transactionMode === 'manual' && (
          <>
            <span className={`text-xs px-2 py-1 rounded border whitespace-nowrap ${
              transactionState === 'active'
                ? 'text-amber-300 border-amber-500/30 bg-amber-500/10'
                : transactionBusy
                  ? 'text-blue-300 border-blue-500/30 bg-blue-500/10'
                  : 'text-gray-400 border-[#30363d] bg-[#161b22]'
            }`}>
              {transactionState === 'active'
                ? 'Txn Active'
                : transactionState === 'committing'
                  ? 'Committing'
                  : transactionState === 'rolling_back'
                    ? 'Rolling Back'
                    : 'Txn Idle'}
            </span>
            <button
              onClick={onCommitTransaction}
              disabled={!transactionActive || tabState.isExecuting || transactionBusy}
              className="flex items-center justify-center gap-2 px-3 py-2 bg-emerald-600/20 border border-emerald-500/30 hover:bg-emerald-600/30 text-emerald-300 rounded text-sm transition-colors disabled:opacity-50 disabled:cursor-not-allowed"
            >
              Commit
            </button>
            <button
              onClick={onRollbackTransaction}
              disabled={!transactionActive || tabState.isExecuting || transactionBusy}
              className="flex items-center justify-center gap-2 px-3 py-2 bg-amber-600/20 border border-amber-500/30 hover:bg-amber-600/30 text-amber-300 rounded text-sm transition-colors disabled:opacity-50 disabled:cursor-not-allowed"
            >
              Rollback
            </button>
          </>
        )}

        {!isCompactActionBar && resolveModelsList.length > 0 && (
          <div className="flex items-center gap-2 mr-1">
            <select
              value={resolveActiveModelId}
              onChange={(e) => onModelChange(e.target.value)}
              disabled={isAiSwitching}
              className="h-9 bg-dark-panel border border-dark-border hover:border-gray-500 text-gray-200 rounded px-2 text-xs transition-colors disabled:opacity-60"
              title={'\u5feb\u901f\u5207\u6362\u6a21\u578b'}
            >
              {resolveModelsList.map((m: any) => (
                <option key={m.id} value={m.id}>{m.display_name || m.id}</option>
              ))}
            </select>
            <select
              value={resolveActiveTier}
              onChange={(e) => onTierChange(e.target.value)}
              disabled={isAiSwitching || !activeModelSupportsTier}
              className="h-9 bg-dark-panel border border-dark-border hover:border-gray-500 text-gray-200 rounded px-2 text-xs transition-colors disabled:opacity-60"
              title={activeModelSupportsTier ? '\u5feb\u901f\u5207\u6362 tier' : '\u8be5\u6a21\u578b\u4e0d\u652f\u6301 tier'}
            >
              <option value="fast">fast</option>
              <option value="balanced">balanced</option>
              <option value="high">high</option>
              <option value="ultra">ultra</option>
            </select>
          </div>
        )}

        {!isCompactActionBar && tabState.lastExplanation && (
          <span className="text-xs text-blue-400 bg-blue-500/10 border border-blue-500/20 px-3 py-1.5 rounded-full mr-2 max-w-sm truncate" title={tabState.lastExplanation}>
            <TypingEffect text={tabState.lastExplanation} />
          </span>
        )}

        {(!tabState.sql || tabState.sql.trim() === '-- Generated SQL will appear here') && !tabState.isGenerating && (
          <span className="text-xs text-gray-500 hidden sm:inline-block border border-gray-700/50 bg-gray-800/30 px-2 py-1 rounded-full mr-1">
            {'\u6309 '}<kbd className="font-mono bg-black/50 px-1 rounded mx-0.5 text-gray-400">Cmd+K</kbd>{' \u5524\u8d77 AI \u6307\u4ee4'}
          </span>
        )}

        <motion.button
          whileHover={{ scale: 1.02 }}
          whileTap={{ scale: 0.98 }}
          className="flex items-center justify-center gap-2 px-3 py-2 bg-dark-panel border border-dark-border hover:border-gray-500 hover:text-white rounded text-sm text-gray-300 transition-colors shadow-sm ripple whitespace-nowrap leading-none"
          onClick={onOpenCommandPalette}
          title={tr('打开命令面板（AI / 片段 / 书签）', 'Open command palette (AI / snippets / bookmarks)')}
        >
          <Command className="w-4 h-4" />
          <span className="whitespace-nowrap">{tr('\u8be2\u95ee AI', 'Ask AI')} <span className="opacity-50 ml-1 text-xs bg-dark-bg px-1 rounded border border-dark-border">Cmd K</span></span>
        </motion.button>

        {!isCompactActionBar && (
          <motion.button
            whileHover={{ scale: 1.02 }}
            whileTap={{ scale: 0.98 }}
            className="flex items-center justify-center gap-2 px-3 py-2 bg-dark-panel border border-dark-border hover:border-amber-500 hover:text-amber-300 rounded text-sm text-gray-300 transition-colors shadow-sm ripple whitespace-nowrap leading-none disabled:opacity-50 disabled:cursor-not-allowed"
            onClick={onSaveBookmark}
            disabled={disableBookmarkAction}
            title={tr('保存当前 SQL 为书签', 'Save current SQL as bookmark')}
          >
            <BookMarked className="w-4 h-4" />
            <span className="whitespace-nowrap">{tr('保存书签', 'Save Bookmark')}</span>
          </motion.button>
        )}

        {!isCompactActionBar && (
          <>
            <motion.button
              whileHover={{ scale: 1.02 }}
              whileTap={{ scale: 0.98 }}
              className="flex items-center justify-center gap-2 px-3 py-2 bg-dark-panel border border-dark-border hover:border-blue-500 hover:text-blue-400 rounded text-sm text-gray-300 transition-colors shadow-sm ripple whitespace-nowrap leading-none"
              onClick={onAiOptimize}
              disabled={disableAiSqlActions}
            >
              <Sparkles className="w-4 h-4" />
              <span className="whitespace-nowrap">{'AI \u4f18\u5316 (Optimize)'}</span>
            </motion.button>
            <motion.button
              whileHover={{ scale: 1.02 }}
              whileTap={{ scale: 0.98 }}
              className="flex items-center justify-center gap-2 px-3 py-2 bg-dark-panel border border-dark-border hover:border-purple-500 hover:text-purple-400 rounded text-sm text-gray-300 transition-colors shadow-sm ripple whitespace-nowrap leading-none"
              onClick={onAiExplain}
              disabled={disableAiSqlActions}
            >
              <Sparkles className="w-4 h-4" />
              <span className="whitespace-nowrap">{'AI \u89e3\u91ca (Explain)'}</span>
            </motion.button>
            <motion.button
              whileHover={{ scale: 1.02 }}
              whileTap={{ scale: 0.98 }}
              className="flex items-center justify-center gap-2 px-3 py-2 bg-dark-panel border border-dark-border hover:border-gray-500 hover:text-white rounded text-sm text-gray-300 transition-colors shadow-sm ripple whitespace-nowrap leading-none"
              onClick={onExplainPlan}
              title="Execution Plan"
              disabled={!tabState.sql.trim() || !dbConnected}
            >
              <span className="whitespace-nowrap">{'Explain (\u6267\u884c\u8ba1\u5212)'}</span>
            </motion.button>
            <motion.button
              whileHover={{ scale: 1.02 }}
              whileTap={{ scale: 0.98 }}
              className="flex items-center justify-center gap-2 px-3 py-2 bg-dark-panel border border-dark-border hover:border-cyan-500 hover:text-cyan-300 rounded text-sm text-gray-300 transition-colors shadow-sm ripple whitespace-nowrap leading-none"
              onClick={onOpenSessionInfo}
              title="Session Info"
              disabled={!dbConnected}
            >
              <Server className="w-4 h-4" />
              <span className="whitespace-nowrap">Session Info</span>
            </motion.button>
            <motion.button
              whileHover={{ scale: 1.02 }}
              whileTap={{ scale: 0.98 }}
              className="flex items-center justify-center gap-2 px-3 py-2 bg-[#21262d] border border-dark-border hover:bg-[#30363d] hover:border-gray-500 hover:text-white rounded text-sm text-gray-300 transition-colors shadow-sm ripple whitespace-nowrap leading-none"
              onClick={onFormatSql}
              title="Format SQL (Shift+Alt+F)"
            >
              <AlignLeft className="w-4 h-4" />
              <span className="whitespace-nowrap">Format</span>
            </motion.button>
          </>
        )}

        {isCompactActionBar && (
          <div
            className="relative"
            onClick={(e) => {
              e.stopPropagation()
            }}
          >
            <button
              className="flex items-center justify-center h-9 w-9 bg-[#21262d] border border-dark-border hover:bg-[#30363d] hover:border-gray-500 hover:text-white rounded text-sm text-gray-300 transition-colors shadow-sm"
              onClick={onToggleMoreActions}
              title={'\u66f4\u591a\u64cd\u4f5c'}
            >
              <MoreHorizontal className="w-4 h-4" />
            </button>
            {showMoreActions && (
              <div className="absolute bottom-11 right-0 w-56 bg-[#161b22] border border-[#30363d] rounded-lg shadow-2xl overflow-hidden z-30">
                {resolveModelsList.length > 0 && (
                  <div className="px-3 py-2 border-b border-[#30363d] space-y-2">
                    <select
                      value={resolveActiveModelId}
                      onChange={(e) => onModelChange(e.target.value)}
                      disabled={isAiSwitching}
                      className="h-8 w-full bg-dark-panel border border-dark-border hover:border-gray-500 text-gray-200 rounded px-2 text-xs transition-colors disabled:opacity-60"
                      title={'\u5feb\u901f\u5207\u6362\u6a21\u578b'}
                    >
                      {resolveModelsList.map((m: any) => (
                        <option key={m.id} value={m.id}>{m.display_name || m.id}</option>
                      ))}
                    </select>
                    <select
                      value={resolveActiveTier}
                      onChange={(e) => onTierChange(e.target.value)}
                      disabled={isAiSwitching || !activeModelSupportsTier}
                      className="h-8 w-full bg-dark-panel border border-dark-border hover:border-gray-500 text-gray-200 rounded px-2 text-xs transition-colors disabled:opacity-60"
                      title={activeModelSupportsTier ? '\u5feb\u901f\u5207\u6362 tier' : '\u8be5\u6a21\u578b\u4e0d\u652f\u6301 tier'}
                    >
                      <option value="fast">fast</option>
                      <option value="balanced">balanced</option>
                      <option value="high">high</option>
                      <option value="ultra">ultra</option>
                    </select>
                  </div>
                )}
                <button
                  onClick={() => {
                    onCloseMoreActions()
                    onSaveBookmark()
                  }}
                  disabled={disableBookmarkAction}
                  className="w-full text-left px-3 py-2 text-sm text-gray-200 hover:bg-[#21262d] disabled:opacity-50"
                >
                  {tr('保存书签', 'Save Bookmark')}
                </button>
                <button
                  onClick={() => {
                    onCloseMoreActions()
                    onAiOptimize()
                  }}
                  disabled={disableAiSqlActions}
                  className="w-full text-left px-3 py-2 text-sm text-gray-200 hover:bg-[#21262d] disabled:opacity-50"
                >
                  {'AI \u4f18\u5316 (Optimize)'}
                </button>
                <button
                  onClick={() => {
                    onCloseMoreActions()
                    onAiExplain()
                  }}
                  disabled={disableAiSqlActions}
                  className="w-full text-left px-3 py-2 text-sm text-gray-200 hover:bg-[#21262d] disabled:opacity-50"
                >
                  {'AI \u89e3\u91ca (Explain)'}
                </button>
                <button
                  onClick={() => {
                    onCloseMoreActions()
                    onExplainPlan()
                  }}
                  disabled={!tabState.sql.trim() || !dbConnected}
                  className="w-full text-left px-3 py-2 text-sm text-gray-200 hover:bg-[#21262d] disabled:opacity-50"
                >
                  {'Explain (\u6267\u884c\u8ba1\u5212)'}
                </button>
                <button
                  onClick={() => {
                    onCloseMoreActions()
                    onOpenSessionInfo()
                  }}
                  disabled={!dbConnected}
                  className="w-full text-left px-3 py-2 text-sm text-gray-200 hover:bg-[#21262d] disabled:opacity-50"
                >
                  Session Info
                </button>
                <button
                  onClick={() => {
                    onCloseMoreActions()
                    onFormatSql()
                  }}
                  className="w-full text-left px-3 py-2 text-sm text-gray-200 hover:bg-[#21262d]"
                >
                  Format
                </button>
              </div>
            )}
          </div>
        )}

        <motion.button
          whileHover={{ scale: 1.02 }}
          whileTap={{ scale: 0.95 }}
          onClick={onExecute}
          disabled={tabState.isExecuting || !tabState.sql.trim() || !dbConnected}
          title={!dbConnected ? 'Execute requires live database connection' : 'Execute SQL (Cmd+Enter)'}
          className={`flex items-center justify-center gap-2 px-5 py-2 rounded text-sm font-medium text-white transition-all shadow-[0_0_15px_rgba(59,130,246,0.2)] ripple relative overflow-hidden disabled:opacity-50 disabled:cursor-not-allowed whitespace-nowrap leading-none ${
            tabState.isExecuting ? 'bg-blue-700 cursor-wait' : 'bg-dark-accent hover:bg-blue-500 hover:shadow-[0_0_20px_rgba(59,130,246,0.4)]'
          }`}
        >
          {tabState.isExecuting && (
            <div className="absolute inset-0 shimmer-bg z-0"></div>
          )}
          <Play className="w-4 h-4 fill-current relative z-10" />
          <span className="relative z-10 whitespace-nowrap">{tabState.isExecuting ? 'Executing...' : 'Run'}</span>
        </motion.button>

        {tabState.isExecuting && (
          <button
            onClick={onCancelExecution}
            disabled={Boolean(tabState.isCancelingExecution)}
            className="flex items-center justify-center gap-2 px-4 py-2 rounded text-sm font-medium text-white bg-red-600 hover:bg-red-500 disabled:opacity-50 disabled:cursor-wait transition-colors"
          >
            <X className="w-4 h-4" />
            <span>{tabState.isCancelingExecution ? 'Canceling...' : 'Cancel'}</span>
          </button>
        )}
      </div>
    </div>
  )
}
