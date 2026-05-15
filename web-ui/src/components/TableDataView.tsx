import { useState, useEffect, useCallback, useMemo, useRef } from 'react'
import { api } from '../api'
import { DataTable } from './DataTable'
import { Skeleton } from './Skeleton'
import { DataCharts } from './DataCharts'
import { tr } from '../i18n'

function useDebounce<T>(value: T, delay: number): T {
  const [debouncedValue, setDebouncedValue] = useState<T>(value);
  useEffect(() => {
    const handler = setTimeout(() => {
      setDebouncedValue(value);
    }, delay);
    return () => {
      clearTimeout(handler);
    };
  }, [value, delay]);
  return debouncedValue;
}

export function TableDataView({ tableName, isActive, dbId, transactionId, onTransactionStateChange }: { tableName: string, isActive: boolean, dbId?: string, transactionId?: string | null, onTransactionStateChange?: (state: 'active' | 'idle') => void }) {
  const [data, setData] = useState<any[]>([])
  const [schema, setSchema] = useState<any>(null)
  const [total, setTotal] = useState<number | null>(null)
  const [hasMore, setHasMore] = useState(false)
  const [page, setPage] = useState(1)
  const [pageSize, setPageSize] = useState(100)
  const [pageNavigation, setPageNavigation] = useState<'reset' | 'next' | 'prev' | 'steady'>('reset')
  const [pageBoundaries, setPageBoundaries] = useState<Record<number, { first: any; last: any }>>({})
  const [dataLoading, setDataLoading] = useState(false)
  const [schemaLoading, setSchemaLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [dataRevision, setDataRevision] = useState(0)
  
  // Sorting and Filtering
  const [sorts, setSorts] = useState<{ column: string; desc: boolean }[]>([])
  const [filters, setFilters] = useState<{ column: string; operator: string; value: string }[]>([])
  
  const [viewType, setViewType] = useState<'table' | 'chart'>('table')

  const debouncedSorts = useDebounce(sorts, 300)
  const debouncedFilters = useDebounce(filters, 300)
  const loading = dataLoading || schemaLoading
  const pageNavigationRef = useRef(pageNavigation)
  const pageBoundariesRef = useRef(pageBoundaries)

  useEffect(() => {
    pageNavigationRef.current = pageNavigation
  }, [pageNavigation])

  useEffect(() => {
    pageBoundariesRef.current = pageBoundaries
  }, [pageBoundaries])

  const keysetColumn = useMemo(() => {
    if (!schema?.indexes || !schema?.columns) return null

    const primaryKeyColumns = schema.indexes
      .filter((index: any) => index.index_name === 'PRIMARY')
      .map((index: any) => index.column_name)
    if (primaryKeyColumns.length === 1) {
      return primaryKeyColumns[0]
    }

    const uniqueIndexColumns = new Map<string, string[]>()
    for (const index of schema.indexes) {
      if (index.index_name === 'PRIMARY' || index.non_unique) continue
      const columns = uniqueIndexColumns.get(index.index_name) || []
      columns.push(index.column_name)
      uniqueIndexColumns.set(index.index_name, columns)
    }

    for (const [, columns] of uniqueIndexColumns) {
      if (columns.length !== 1) continue
      const column = schema.columns.find((item: any) => item.column_name === columns[0])
      if (column?.is_nullable === 'NO') {
        return columns[0]
      }
    }

    return null
  }, [schema])

  const canUseKeyset = useMemo(() => {
    if (!keysetColumn) return false
    if (sorts.length === 0) return true
    return sorts.length === 1 && sorts[0].column === keysetColumn
  }, [keysetColumn, sorts])

  const effectiveSorts = useMemo(() => {
    if (sorts.length > 0) return sorts
    if (!keysetColumn) return []
    return [{ column: keysetColumn, desc: false }]
  }, [keysetColumn, sorts])

  const loadData = useCallback(async () => {
    setDataLoading(true)
    setError(null)
    try {
      let requestPage = page
      let requestSorts = effectiveSorts
      const requestFilters = [...debouncedFilters]
      let shouldReverseRows = false
      const navigation = pageNavigationRef.current
      const boundaries = pageBoundariesRef.current

      if (canUseKeyset) {
        const keysetDesc = Boolean(requestSorts[0]?.desc)
        if (page > 1 && navigation === 'next') {
          const previousPage = boundaries[page - 1]
          if (previousPage?.last !== undefined && previousPage?.last !== null) {
            requestPage = 1
            requestFilters.push({
              column: keysetColumn,
              operator: keysetDesc ? 'less_than' : 'greater_than',
              value: String(previousPage.last),
            })
          }
        } else if (page > 1 && navigation === 'prev') {
          const followingPage = boundaries[page + 1]
          if (followingPage?.first !== undefined && followingPage?.first !== null) {
            requestPage = 1
            requestSorts = [{ column: keysetColumn, desc: !keysetDesc }]
            requestFilters.push({
              column: keysetColumn,
              operator: keysetDesc ? 'greater_than' : 'less_than',
              value: String(followingPage.first),
            })
            shouldReverseRows = true
          }
        }
      }

        const dataRes = await api.getTableData(
          tableName,
          requestPage,
          pageSize,
          requestFilters.length > 0 ? JSON.stringify(requestFilters) : undefined,
          requestSorts.length > 0 ? JSON.stringify(requestSorts) : undefined,
          dbId
      )
      const nextData = shouldReverseRows ? [...dataRes.data].reverse() : dataRes.data
      setData(nextData)
      setDataRevision(prev => prev + 1)
      setTotal(typeof dataRes.total === 'number' ? dataRes.total : null)
      setHasMore(Boolean(dataRes.has_more))
      if (canUseKeyset && nextData.length > 0 && keysetColumn) {
        setPageBoundaries(prev => ({
          ...prev,
          [page]: {
            first: nextData[0]?.[keysetColumn],
            last: nextData[nextData.length - 1]?.[keysetColumn],
          }
        }))
      }
    } catch (e: any) {
      setError(e.response?.data?.message || e.message)
    } finally {
      setPageNavigation('steady')
      setDataLoading(false)
    }
  }, [tableName, page, pageSize, debouncedFilters, dbId, canUseKeyset, effectiveSorts, keysetColumn])

  const loadSchema = useCallback(async () => {
    setSchemaLoading(true)
    setError(null)
    try {
      const schemaRes = await api.getTableSchema(tableName, dbId)
      setSchema(schemaRes)
    } catch (e: any) {
      setError(e.response?.data?.message || e.message)
    } finally {
      setSchemaLoading(false)
    }
  }, [tableName, dbId])

  useEffect(() => {
    setPage(1)
    setHasMore(false)
    setPageNavigation('reset')
    setPageBoundaries({})
  }, [tableName, dbId, pageSize, debouncedSorts, debouncedFilters])

  useEffect(() => {
    loadData()
  }, [loadData])

  useEffect(() => {
    setSchema(null)
    loadSchema()
  }, [loadSchema])

  const handleRefresh = useCallback(() => {
    loadData()
    loadSchema()
  }, [loadData, loadSchema])

  const memoizedSchema = useMemo(() => schema, [schema])
  const memoizedData = useMemo(() => data, [data])
  const showBlockingError = Boolean(error && !memoizedSchema)
  const showInitialLoading = loading && !memoizedSchema && !showBlockingError
  const hasActiveFilters = filters.length > 0
  const hasActiveSorts = sorts.length > 0
  const hasActiveGridState = hasActiveFilters || hasActiveSorts

  const describeFilter = useCallback((filter: { column: string; operator: string; value: string }) => {
    const operatorLabels: Record<string, string> = {
      equals: '=',
      not_equals: '!=',
      contains: 'contains',
      starts_with: 'starts with',
      ends_with: 'ends with',
      greater_than: '>',
      less_than: '<',
      between: 'between',
      in: 'in',
      not_in: 'not in',
      is_null: 'is null',
      is_not_null: 'is not null',
    }
    const operatorLabel = operatorLabels[filter.operator] || filter.operator
    return filter.value ? `${filter.column} ${operatorLabel} ${filter.value}` : `${filter.column} ${operatorLabel}`
  }, [])

  const handleResetGridState = useCallback(() => {
    setFilters([])
    setSorts([])
    setPage(1)
    setPageNavigation('reset')
    setPageBoundaries({})
  }, [])

  const handleRemoveFilterAt = useCallback((index: number) => {
    setFilters(prev => prev.filter((_, currentIndex) => currentIndex !== index))
    setPage(1)
    setPageNavigation('reset')
    setPageBoundaries({})
  }, [])

  return (
    <div className="flex flex-col h-full bg-dark-bg">
      <div className="flex-1 overflow-auto relative p-4">
        {showInitialLoading ? (
          <div className="space-y-4">
            <Skeleton className="h-10 w-full" />
            <Skeleton className="h-10 w-full" />
            <Skeleton className="h-10 w-full" />
            <Skeleton className="h-10 w-full" />
            <Skeleton className="h-10 w-full" />
          </div>
        ) : showBlockingError ? (
          <div className="text-red-400 p-4 bg-red-950/20 border border-red-500/20 rounded">
            {tr('错误', 'Error')}: {error}
          </div>
        ) : memoizedSchema ? (
          <div className="relative h-full">
            {error && (
              <div className="mb-3 rounded border border-red-500/20 bg-red-950/20 p-3 text-sm text-red-300">
                {tr('閿欒', 'Error')}: {error}
              </div>
            )}
            {viewType === 'table' ? (
              <DataTable 
                data={memoizedData} 
                schema={memoizedSchema} 
                tableName={tableName}
                dbId={dbId}
                transactionId={transactionId}
                onTransactionStateChange={onTransactionStateChange}
                sorts={sorts}
                setSorts={setSorts}
                filters={filters}
                setFilters={setFilters}
                onRefresh={handleRefresh}
                isActive={isActive}
                isRefreshing={loading}
                refreshError={error}
                dataRevision={dataRevision}
              />
            ) : (
              <DataCharts data={memoizedData} />
            )}
            {loading && (
              <div className="pointer-events-none absolute right-3 top-3 rounded border border-blue-500/20 bg-[#0d1117]/90 px-3 py-1 text-xs text-blue-200 shadow-lg">
                Refreshing latest server data...
              </div>
            )}
          </div>
        ) : (
          <div className="text-gray-500 flex justify-center items-center h-full">
            {tr('未找到数据', 'No data found')}
          </div>
        )}
      </div>
      <div className="min-h-12 border-t border-dark-border bg-dark-panel flex items-center justify-between px-4 py-2 text-sm text-gray-400 gap-4">
        <div className="flex items-center gap-2 flex-wrap min-w-0">
          <div>
            {typeof total === 'number' ? `Total: ${total} rows` : tr('总数未计算，可继续翻页', 'Total not calculated; continue paging')}
          </div>
          {hasActiveFilters && (
            <span className="text-xs px-2 py-0.5 rounded-full border border-blue-500/30 bg-blue-500/10 text-blue-300">
              {tr('筛选', 'Filters')} {filters.length}
            </span>
          )}
          {filters.map((filter, index) => (
            <button
              key={`${filter.column}-${filter.operator}-${filter.value}-${index}`}
              onClick={() => handleRemoveFilterAt(index)}
              className="max-w-[260px] truncate text-xs px-2 py-0.5 rounded-full border border-blue-500/20 bg-[#0d1117] text-blue-200 hover:border-blue-400/40 hover:text-white transition-colors"
              title={describeFilter(filter)}
            >
              {describeFilter(filter)} ×
            </button>
          ))}
          {hasActiveSorts && (
            <span className="text-xs px-2 py-0.5 rounded-full border border-purple-500/30 bg-purple-500/10 text-purple-300">
              {tr('排序', 'Sorts')} {sorts.length}
            </span>
          )}
          <button
            onClick={handleResetGridState}
            disabled={!hasActiveGridState}
            className="text-xs text-gray-400 hover:text-white bg-[#21262d] hover:bg-[#30363d] px-2 py-0.5 rounded border border-[#30363d] transition-colors disabled:opacity-50 disabled:hover:bg-[#21262d] disabled:hover:text-gray-400"
          >
            {tr('重置筛选/排序', 'Reset filters/sorts')}
          </button>
          <div className="flex items-center bg-[#21262d] rounded overflow-hidden border border-[#30363d]">
            <button
              onClick={() => setViewType('table')}
              className={`px-2 py-0.5 text-xs transition-colors ${viewType === 'table' ? 'bg-blue-500/20 text-blue-400' : 'text-gray-400 hover:bg-[#30363d]'}`}
            >
              {tr('表格', 'Table')}
            </button>
            <button
              onClick={() => setViewType('chart')}
              className={`px-2 py-0.5 text-xs transition-colors ${viewType === 'chart' ? 'bg-blue-500/20 text-blue-400' : 'text-gray-400 hover:bg-[#30363d]'}`}
            >
              {tr('图表', 'Chart')}
            </button>
          </div>
        </div>
        <div className="flex items-center gap-2 shrink-0">
          <button 
            disabled={page === 1}
            onClick={() => {
              setPageNavigation('prev')
              setPage(p => p - 1)
            }}
            className="px-2 py-1 bg-[#21262d] hover:bg-[#30363d] rounded disabled:opacity-50"
          >
            {tr('上一页', 'Prev')}
          </button>
          <span>{tr(`第 ${page} 页`, `Page ${page}`)}</span>
          <button 
            disabled={!hasMore}
            onClick={() => {
              setPageNavigation('next')
              setPage(p => p + 1)
            }}
            className="px-2 py-1 bg-[#21262d] hover:bg-[#30363d] rounded disabled:opacity-50"
          >
            {tr('下一页', 'Next')}
          </button>
          <select 
            value={pageSize}
            onChange={(e) => {
              setPageNavigation('reset')
              setPageSize(Number(e.target.value))
              setPage(1)
            }}
            className="ml-4 bg-[#21262d] border border-dark-border rounded px-2 py-1"
          >
            <option value={50}>{tr('50 / 页', '50 / page')}</option>
            <option value={100}>{tr('100 / 页', '100 / page')}</option>
            <option value={200}>{tr('200 / page', '200 / page')}</option>
            <option value={500}>{tr('500 / 页', '500 / page')}</option>
          </select>
        </div>
      </div>
    </div>
  )
}
