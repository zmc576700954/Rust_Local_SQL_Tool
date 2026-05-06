import { useState, useEffect, useCallback, useMemo } from 'react'
import { api } from '../api'
import { DataTable } from './DataTable'
import { Skeleton } from './Skeleton'
import { DataCharts } from './DataCharts'

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

export function TableDataView({ tableName, isActive }: { tableName: string, isActive: boolean }) {
  const [data, setData] = useState<any[]>([])
  const [schema, setSchema] = useState<any>(null)
  const [total, setTotal] = useState(0)
  const [page, setPage] = useState(1)
  const [pageSize, setPageSize] = useState(50)
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)
  
  // Sorting and Filtering
  const [sorts, setSorts] = useState<{ column: string; desc: boolean }[]>([])
  const [filters, setFilters] = useState<{ column: string; operator: string; value: string }[]>([])
  
  const [viewType, setViewType] = useState<'table' | 'chart'>('table')

  const debouncedSorts = useDebounce(sorts, 300)
  const debouncedFilters = useDebounce(filters, 300)
  const debouncedPage = useDebounce(page, 300)
  const debouncedPageSize = useDebounce(pageSize, 300)

  const loadData = useCallback(async () => {
    setLoading(true)
    setError(null)
    try {
      const [dataRes, schemaRes] = await Promise.all([
        api.getTableData(
          tableName,
          debouncedPage,
          debouncedPageSize,
          debouncedFilters.length > 0 ? JSON.stringify(debouncedFilters) : undefined,
          debouncedSorts.length > 0 ? JSON.stringify(debouncedSorts) : undefined
        ),
        api.getTableSchema(tableName)
      ])
      setData(dataRes.data)
      setTotal(dataRes.total)
      setSchema(schemaRes)
    } catch (e: any) {
      setError(e.response?.data?.message || e.message)
    } finally {
      setLoading(false)
    }
  }, [tableName, debouncedPage, debouncedPageSize, debouncedFilters, debouncedSorts])

  useEffect(() => {
    loadData()
  }, [loadData])

  const handleRefresh = useCallback(() => {
    loadData()
  }, [loadData])

  const memoizedSchema = useMemo(() => schema, [schema])
  const memoizedData = useMemo(() => data, [data])

  return (
    <div className="flex flex-col h-full bg-dark-bg">
      <div className="flex-1 overflow-auto relative p-4">
        {loading ? (
          <div className="space-y-4">
            <Skeleton className="h-10 w-full" />
            <Skeleton className="h-10 w-full" />
            <Skeleton className="h-10 w-full" />
            <Skeleton className="h-10 w-full" />
            <Skeleton className="h-10 w-full" />
          </div>
        ) : error ? (
          <div className="text-red-400 p-4 bg-red-950/20 border border-red-500/20 rounded">
            Error: {error}
          </div>
        ) : memoizedSchema ? (
          viewType === 'table' ? (
            <DataTable 
              data={memoizedData} 
              schema={memoizedSchema} 
              tableName={tableName}
              sorts={sorts}
              setSorts={setSorts}
              filters={filters}
              setFilters={setFilters}
              onRefresh={handleRefresh}
              isActive={isActive}
            />
          ) : (
            <DataCharts data={memoizedData} />
          )
        ) : (
          <div className="text-gray-500 flex justify-center items-center h-full">
            No data found
          </div>
        )}
      </div>
      <div className="h-12 border-t border-dark-border bg-dark-panel flex items-center justify-between px-4 text-sm text-gray-400">
        <div className="flex items-center gap-4">
          <div>
            Total: {total} rows
          </div>
          <div className="flex items-center bg-[#21262d] rounded overflow-hidden border border-[#30363d]">
            <button
              onClick={() => setViewType('table')}
              className={`px-2 py-0.5 text-xs transition-colors ${viewType === 'table' ? 'bg-blue-500/20 text-blue-400' : 'text-gray-400 hover:bg-[#30363d]'}`}
            >
              Table
            </button>
            <button
              onClick={() => setViewType('chart')}
              className={`px-2 py-0.5 text-xs transition-colors ${viewType === 'chart' ? 'bg-blue-500/20 text-blue-400' : 'text-gray-400 hover:bg-[#30363d]'}`}
            >
              Chart
            </button>
          </div>
        </div>
        <div className="flex items-center gap-2">
          <button 
            disabled={page === 1}
            onClick={() => setPage(p => p - 1)}
            className="px-2 py-1 bg-[#21262d] hover:bg-[#30363d] rounded disabled:opacity-50"
          >
            Prev
          </button>
          <span>Page {page}</span>
          <button 
            disabled={page * pageSize >= total}
            onClick={() => setPage(p => p + 1)}
            className="px-2 py-1 bg-[#21262d] hover:bg-[#30363d] rounded disabled:opacity-50"
          >
            Next
          </button>
          <select 
            value={pageSize}
            onChange={(e) => {
              setPageSize(Number(e.target.value))
              setPage(1)
            }}
            className="ml-4 bg-[#21262d] border border-dark-border rounded px-2 py-1"
          >
            <option value={50}>50 / page</option>
            <option value={100}>100 / page</option>
            <option value={500}>500 / page</option>
          </select>
        </div>
      </div>
    </div>
  )
}
