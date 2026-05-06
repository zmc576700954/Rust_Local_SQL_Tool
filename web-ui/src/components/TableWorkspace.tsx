import { useState } from 'react'
import { TableDataView } from './TableDataView'
import { TableDesigner } from './TableDesigner'

export function TableWorkspace({ tableName, isActive }: { tableName: string, isActive: boolean }) {
  const [view, setView] = useState<'data' | 'designer'>('data')

  return (
    <div className="flex flex-col h-full">
      <div className="h-10 border-b border-dark-border flex items-center px-4 gap-4 bg-dark-bg">
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
        {view === 'data' ? <TableDataView tableName={tableName} isActive={isActive} /> : <TableDesigner tableName={tableName} isActive={isActive} />}
      </div>
    </div>
  )
}
