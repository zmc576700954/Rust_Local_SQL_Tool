import { useState } from 'react'
import { TableDataView } from './TableDataView'
import { TableDesigner } from './TableDesigner'

export function TableWorkspace({
  tableName,
  isActive,
  dbId,
  transactionId,
  onTransactionStateChange,
}: {
  tableName: string
  isActive: boolean
  dbId?: string
  transactionId?: string | null
  onTransactionStateChange?: (state: 'active' | 'idle') => void
}) {
  const [view, setView] = useState<'data' | 'designer'>('data')

  return (
    <div className="flex flex-col h-full">
      <div className="flex items-center gap-4 px-4 py-2 border-b border-dark-border bg-dark-panel shrink-0">
        <button
          onClick={() => setView('data')}
          className={`text-sm font-medium ${view === 'data' ? 'text-blue-400' : 'text-gray-400 hover:text-gray-200'}`}
        >
          Data
        </button>
        <button
          onClick={() => setView('designer')}
          className={`text-sm font-medium ${view === 'designer' ? 'text-blue-400' : 'text-gray-400 hover:text-gray-200'}`}
        >
          Designer
        </button>
      </div>
      <div className="flex-1 overflow-hidden">
        {view === 'data'
          ? <TableDataView tableName={tableName} isActive={isActive} dbId={dbId} transactionId={transactionId} onTransactionStateChange={onTransactionStateChange} />
          : <TableDesigner tableName={tableName} isActive={isActive} dbId={dbId} />}
      </div>
    </div>
  )
}

