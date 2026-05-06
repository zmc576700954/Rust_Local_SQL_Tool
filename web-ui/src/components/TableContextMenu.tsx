import { useEffect, useRef } from 'react';
import { Database, FileDown, FileUp } from 'lucide-react';

interface TableContextMenuProps {
  x: number;
  y: number;
  table: any;
  onClose: () => void;
  onGenerateMockData: (table: any) => void;
  onExport: (table: any) => void;
  onImport: (table: any) => void;
}

export function TableContextMenu({ x, y, table, onClose, onGenerateMockData, onExport, onImport }: TableContextMenuProps) {
  const menuRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const handleClickOutside = (e: MouseEvent) => {
      if (menuRef.current && !menuRef.current.contains(e.target as Node)) {
        onClose();
      }
    };
    document.addEventListener('mousedown', handleClickOutside);
    return () => document.removeEventListener('mousedown', handleClickOutside);
  }, [onClose]);

  if (!table) return null;

  return (
    <div 
      ref={menuRef}
      className="fixed z-50 bg-[#161b22] border border-[#30363d] rounded-md shadow-2xl py-1 min-w-[160px] text-sm text-gray-300"
      style={{ top: y, left: x }}
    >
      <div className="px-3 py-1.5 border-b border-[#30363d] text-xs font-semibold text-gray-500 mb-1">
        {table.table_name}
      </div>
      <button 
        className="w-full text-left px-3 py-1.5 hover:bg-blue-500/20 hover:text-blue-400 transition-colors flex items-center gap-2"
        onClick={() => {
          onGenerateMockData(table);
          onClose();
        }}
      >
        <Database className="w-3.5 h-3.5" />
        生成测试数据
      </button>
      <button 
        className="w-full text-left px-3 py-1.5 hover:bg-blue-500/20 hover:text-blue-400 transition-colors flex items-center gap-2"
        onClick={() => {
          onImport(table);
          onClose();
        }}
      >
        <FileUp className="w-3.5 h-3.5" />
        导入向导
      </button>
      <button 
        className="w-full text-left px-3 py-1.5 hover:bg-blue-500/20 hover:text-blue-400 transition-colors flex items-center gap-2"
        onClick={() => {
          onExport(table);
          onClose();
        }}
      >
        <FileDown className="w-3.5 h-3.5" />
        导出向导
      </button>
    </div>
  );
}
