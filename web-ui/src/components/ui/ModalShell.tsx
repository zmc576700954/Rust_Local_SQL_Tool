import { type ReactNode } from 'react'
import { X } from 'lucide-react'

interface ModalShellProps {
  isOpen: boolean
  onClose: () => void
  title?: string
  maxWidth?: string
  padding?: boolean
  children: ReactNode
}

export function ModalShell({
  isOpen,
  onClose,
  title,
  maxWidth = 'max-w-2xl',
  padding = true,
  children,
}: ModalShellProps) {
  if (!isOpen) return null

  return (
    <div className="fixed inset-0 bg-black/60 backdrop-blur-sm z-[100] flex items-center justify-center p-4">
      <div className={`bg-dark-panel border border-dark-border rounded-xl shadow-2xl ${maxWidth} w-full mx-4 flex flex-col max-h-[80vh] overflow-hidden`}>
        {title && (
          <div className="px-6 py-4 border-b border-dark-border flex items-center justify-between bg-dark-bg shrink-0">
            <h3 className="text-gray-200 font-bold text-lg">{title}</h3>
            <button onClick={onClose} className="text-gray-500 hover:text-white transition-colors focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-blue-500/50 rounded">
              <X className="w-5 h-5" />
            </button>
          </div>
        )}
        <div className={`${padding ? 'p-6' : ''} flex-1 overflow-y-auto`}>
          {children}
        </div>
      </div>
    </div>
  )
}
