import { Layers, RefreshCw, MoveRight, MousePointerSquareDashed, BrainCircuit, HeartPulse, Rocket, FileText, ShieldCheck } from 'lucide-react';

interface ToolsNavProps {
  onSelectTool: (toolId: string) => void;
}

export function ToolsNav({ onSelectTool }: ToolsNavProps) {
  const tools = [
    { id: 'query-builder', name: '可视化查询', icon: MousePointerSquareDashed },
    { id: 'ai-training', name: 'AI 训练面板', icon: BrainCircuit },
    { id: 'schema-sync', name: '结构同步', icon: Layers },
    { id: 'data-sync', name: '数据同步', icon: RefreshCw },
    { id: 'perf-sync', name: '同步压测', icon: HeartPulse },
    { id: 'go-live', name: '上线门禁', icon: Rocket },
    { id: 'go-live-reports', name: '门禁报告', icon: FileText },
    { id: 'go-live-audit', name: '门禁审计', icon: ShieldCheck },
    { id: 'data-transfer', name: '数据传输', icon: MoveRight },
  ];

  return (
    <div className="flex items-center h-12 px-4 border-b border-[#30363d] bg-[#0d1117] gap-2 shrink-0">
      <span className="text-xs font-bold tracking-wider text-gray-500 uppercase mr-4">Tools</span>
      {tools.map(tool => (
        <button
          key={tool.id}
          onClick={() => onSelectTool(tool.id)}
          className="flex items-center gap-2 px-3 py-1.5 rounded-md text-sm text-gray-300 hover:text-white hover:bg-[#21262d] transition-colors border border-transparent hover:border-[#30363d]"
        >
          <tool.icon className="w-4 h-4 text-blue-400" />
          {tool.name}
        </button>
      ))}
    </div>
  );
}
