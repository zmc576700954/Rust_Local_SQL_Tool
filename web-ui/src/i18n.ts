import { createContext, createElement, useContext, useEffect, useMemo, useState } from 'react'
import type { ReactNode } from 'react'

export type Locale = 'zh' | 'en'

const LOCALE_STORAGE_KEY = 'app_locale'

export function getSystemLocale(): Locale {
  if (typeof navigator === 'undefined') return 'en'
  const lang = String(navigator.language || '').toLowerCase()
  if (lang.startsWith('zh')) return 'zh'
  return 'en'
}

export function getLocale(): Locale {
  if (typeof window === 'undefined') return 'en'
  const saved = window.localStorage.getItem(LOCALE_STORAGE_KEY)
  if (saved === 'zh' || saved === 'en') return saved
  return getSystemLocale()
}

export function setLocale(locale: Locale) {
  if (typeof window === 'undefined') return
  window.localStorage.setItem(LOCALE_STORAGE_KEY, locale)
  window.dispatchEvent(new CustomEvent('app-locale-change', { detail: locale }))
}

export function tr(zh: string, en: string): string {
  return getLocale() === 'zh' ? zh : en
}

const PHRASE_PAIRS: Array<[string, string]> = [
  ['设置', 'Settings'],
  ['策略与规则', 'Agents & Policy'],
  ['AI 配置', 'AI Profiles'],
  ['数据库与接口', 'Database & API'],
  ['未连接数据库', 'No database connected'],
  ['导入 DDL', 'Import DDL'],
  ['结构同步', 'Schema Sync'],
  ['数据同步', 'Data Sync'],
  ['同步压测', 'Perf Sync'],
  ['上线门禁', 'Go-live Gate'],
  ['数据传输', 'Data Transfer'],
  ['数据分析图表', 'Data Analytics'],
  ['门禁报告', 'Go-live Reports'],
  ['门禁审计', 'Go-live Audit'],
  ['执行计划', 'Execution Plan'],
  ['取消执行', 'Cancel'],
  ['高危操作警告', 'Dangerous SQL Warning'],
  ['我已确认风险，强制执行', 'Force Execute'],
  ['等待执行', 'Awaiting Execution'],
  ['执行查询中...', 'Executing query...'],
  ['结果', 'Results'],
  ['表格', 'Table'],
  ['图表', 'Chart'],
  ['下载 CSV', 'Download CSV'],
  ['下载 SQL', 'Download SQL'],
  ['清空结果', 'Clear Results'],
  ['运行', 'Run'],
  ['执行中...', 'Executing...'],
  ['加载中...', 'Loading...'],
  ['工具', 'Tools'],
  ['结构', 'Schema'],
  ['智能片段', 'Smart Snippets'],
  ['历史', 'History'],
  ['本地 AI SQL', 'Local AI SQL'],
  ['AI 知识库', 'AI Knowledge Base'],
  ['文档', 'Doc'],
  ['片段', 'Snippets'],
  ['新增知识', 'Add Knowledge'],
  ['标题和内容不能为空', 'Title and Content are required'],
  ['标题 / 问题', 'Title / Question'],
  ['描述 / 自然语言问题', 'Description / Natural Language Query'],
  ['内容 / SQL', 'Content / SQL'],
  ['黄金样本', 'Golden Snippet'],
  ['普通样本', 'Normal Snippet'],
  ['更新时间', 'Updated'],
  ['选择数据库', 'Select Databases'],
  ['选择差异', 'Select Differences'],
  ['预览与执行', 'Preview & Execute'],
  ['步骤 1：选择源库和目标库', 'Step 1: Select Source and Target Databases'],
  ['步骤 2：选择要同步的表', 'Step 2: Select Tables to Sync'],
  ['步骤 3：预览 DDL 脚本', 'Step 3: Preview DDL Script'],
  ['源数据库', 'Source Database'],
  ['目标数据库', 'Target Database'],
  ['开始对比', 'Compare Databases'],
  ['生成 DDL', 'Generate DDL'],
  ['未发现差异。', 'No differences found.'],
  ['请返回上一步执行对比。', 'Please go back and run comparison.'],
  ['结构同步成功', 'Schema synced successfully'],
  ['新建连接', 'New Connection'],
  ['连接地址', 'Connection URL'],
  ['筛选连接...', 'Filter connections...'],
  ['刷新', 'Refresh'],
  ['折叠', 'Collapse'],
  ['展开', 'Expand'],
  ['连接', 'Connections'],
  ['表', 'Tables'],
  ['视图', 'Views'],
  ['(未知数据库)', '(unknown db)'],
  ['已插入 SQL', 'Inserted SQL'],
  ['格式化', 'Format'],
  ['保存', 'Save'],
  ['取消', 'Cancel'],
  ['关闭', 'Close'],
  ['新增', 'Add'],
  ['删除', 'Delete'],
  ['编辑', 'Edit'],
  ['查询构建器', 'Query Builder'],
  ['智能规则', 'Smart Rules'],
  ['快捷键帮助', 'Shortcuts Help'],
  ['刷新页面重试', 'Reload'],
  ['页面出错了', 'Something went wrong'],
  ['本地策略与规则', 'Local Policy Evolution'],
  ['数据库连接', 'Database Connections'],
  ['新增连接', 'Add New Connection'],
  ['只读模式', 'Read-Only Mode'],
  ['健康检查', 'Health Check'],
  ['运行健康检查', 'Run Health Check'],
  ['检查中...', 'Checking...'],
  ['建议', 'Suggestions'],
  ['模型', 'Model'],
  ['推理等级', 'Tier'],
  ['当前', 'Current'],
  ['未选择', 'Not selected'],
  ['未授权', 'Unauthorized'],
  ['禁止访问', 'Forbidden'],
  ['接口不存在', 'Endpoint not found'],
  ['网络错误，请稍后重试。', 'Network error, please try again later.'],
  ['发生未知错误', 'Unknown error'],
  ['加载配置失败：', 'Failed to load config: '],
  ['加载模型列表失败：', 'Failed to load model list: '],
  ['保存配置失败：', 'Failed to save config: '],
  ['重置失败：', 'Failed to reset: '],
  ['创建快照失败：', 'Failed to create snapshot: '],
  ['回滚失败：', 'Failed to rollback: '],
  ['策略已重置为默认。', 'Policy reset to default.'],
  ['策略回滚成功。', 'Policy rolled back successfully.'],
  ['Profile 已保存。', 'Profile saved.'],
  ['Profile 已删除。', 'Profile deleted.'],
  ['AI 运行态已更新。', 'AI runtime updated.'],
  ['Health 检测通过。', 'Health check passed.'],
  ['Health 检测失败：', 'Health check failed: '],
  ['Profile 的 ID 与名称不能为空。', 'Profile ID and name cannot be empty.'],
  ['至少需要保留 1 个 Profile。', 'At least one profile is required.'],
]

const zhToEnMap = new Map<string, string>(PHRASE_PAIRS.map(([zhText, enText]) => [zhText, enText]))
const enToZhMap = new Map<string, string>(PHRASE_PAIRS.map(([zhText, enText]) => [enText, zhText]))

function replaceByPrefix(message: string, locale: Locale): string {
  const prefixPairs: Array<[string, string]> = [
    ['已切换至 ', 'Switched to '],
    ['已切换模型：', 'Model switched: '],
    ['已切换 Tier：', 'Tier switched: '],
    ['已切换 Profile：', 'Profile switched: '],
    ['成功获取 ', 'Fetched '],
    [' 个模型', ' models'],
    ['模型 ', 'Model '],
    ['手动模型 ', 'Manual model '],
    [' 已添加到系统', ' added to system'],
    ['获取模型失败：', 'Failed to fetch models: '],
    ['保存 Profile 失败：', 'Failed to save profile: '],
    ['保存规则失败：', 'Failed to save rule: '],
  ]

  let output = message
  for (const [zhPrefix, enPrefix] of prefixPairs) {
    if (locale === 'en' && output.startsWith(zhPrefix)) {
      output = enPrefix + output.slice(zhPrefix.length)
    }
    if (locale === 'zh' && output.startsWith(enPrefix)) {
      output = zhPrefix + output.slice(enPrefix.length)
    }
  }
  return output
}

function translateLiteral(raw: string, locale: Locale): string {
  const source = String(raw)
  const trimmed = source.trim()
  if (!trimmed) return source
  const left = source.slice(0, source.indexOf(trimmed))
  const right = source.slice(source.indexOf(trimmed) + trimmed.length)

  let translated = trimmed
  if (locale === 'en') translated = zhToEnMap.get(trimmed) || trimmed
  else translated = enToZhMap.get(trimmed) || trimmed

  if (translated === trimmed) {
    translated = replaceByPrefix(trimmed, locale)
  }
  return left + translated + right
}

export function translateText(raw: string): string {
  return translateLiteral(raw, getLocale())
}

function translateElementAttributes(el: Element) {
  const attrs = ['title', 'placeholder', 'aria-label']
  for (const attr of attrs) {
    const val = el.getAttribute(attr)
    if (!val) continue
    const next = translateText(val)
    if (next !== val) el.setAttribute(attr, next)
  }
}

function shouldSkipNode(textNode: Text): boolean {
  const parent = textNode.parentElement
  if (!parent) return true
  const tag = parent.tagName.toLowerCase()
  if (tag === 'script' || tag === 'style') return true
  if (parent.closest('.monaco-editor')) return true
  return false
}

export function translateDom(root: ParentNode = document.body) {
  if (!root) return
  if (root instanceof Element) translateElementAttributes(root)
  if (root instanceof Element) {
    root.querySelectorAll('*').forEach((el) => translateElementAttributes(el))
  }
  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT)
  const textNodes: Text[] = []
  while (walker.nextNode()) {
    textNodes.push(walker.currentNode as Text)
  }
  for (const node of textNodes) {
    if (shouldSkipNode(node)) continue
    const before = node.nodeValue || ''
    const after = translateText(before)
    if (after !== before) node.nodeValue = after
  }
}

export function useAutoI18nDom() {
  useEffect(() => {
    const apply = () => translateDom(document.body)
    apply()
    const observer = new MutationObserver(() => apply())
    observer.observe(document.body, {
      childList: true,
      subtree: true,
      characterData: true,
      attributes: true,
      attributeFilter: ['title', 'placeholder', 'aria-label'],
    })
    const onLocaleChange = () => apply()
    window.addEventListener('app-locale-change', onLocaleChange)
    return () => {
      observer.disconnect()
      window.removeEventListener('app-locale-change', onLocaleChange)
    }
  }, [])
}

type I18nContextValue = {
  locale: Locale
  setLocale: (locale: Locale) => void
  tr: (zh: string, en: string) => string
}

const I18nContext = createContext<I18nContextValue | undefined>(undefined)

export function I18nProvider({ children }: { children: ReactNode }) {
  const [locale, setLocaleState] = useState<Locale>(() => getLocale())

  useEffect(() => {
    const handle = (e: Event) => {
      const next = (e as CustomEvent).detail as Locale | undefined
      if (next === 'zh' || next === 'en') setLocaleState(next)
    }
    window.addEventListener('app-locale-change', handle as EventListener)
    return () => window.removeEventListener('app-locale-change', handle as EventListener)
  }, [])

  const value = useMemo<I18nContextValue>(() => {
    return {
      locale,
      setLocale: (next) => {
        setLocale(next)
        setLocaleState(next)
      },
      tr: (zhText, enText) => (locale === 'zh' ? zhText : enText),
    }
  }, [locale])

  return createElement(I18nContext.Provider, { value }, children)
}

export function useI18n() {
  const ctx = useContext(I18nContext)
  if (!ctx) throw new Error('useI18n must be used within I18nProvider')
  return ctx
}
