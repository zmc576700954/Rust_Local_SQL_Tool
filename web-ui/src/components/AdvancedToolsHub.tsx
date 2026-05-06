import { useEffect, useMemo, useState } from 'react';
import { ShieldCheck, Bell, Layers, RefreshCw } from 'lucide-react';
import { tr } from '../i18n';
import { api } from '../api';

interface AdvancedToolsHubProps {
  onOpenTool: (toolId: string) => void;
}

interface ConfirmDialogProps {
  visible: boolean;
  title: string;
  message: string;
  confirmText: string;
  onCancel: () => void;
  onConfirm: () => void;
}

function ConfirmDialog({ visible, title, message, confirmText, onCancel, onConfirm }: ConfirmDialogProps) {
  if (!visible) return null;
  return (
    <div className="fixed inset-0 z-50 bg-black/60 flex items-center justify-center p-4">
      <div className="w-full max-w-md border border-[#30363d] bg-[#161b22] rounded-xl p-4">
        <div className="text-sm text-gray-100 font-semibold">{title}</div>
        <div className="text-xs text-gray-400 mt-2">{message}</div>
        <div className="mt-4 flex justify-end gap-2">
          <button
            onClick={onCancel}
            className="px-3 py-1.5 text-xs rounded border border-[#30363d] text-gray-300 hover:bg-[#21262d]"
          >
            {tr('取消', 'Cancel')}
          </button>
          <button
            onClick={onConfirm}
            className="px-3 py-1.5 text-xs rounded border border-red-500/30 text-red-300 hover:bg-red-500/10"
          >
            {confirmText}
          </button>
        </div>
      </div>
    </div>
  );
}

const RECENT_KEY = 'tool:advanced-hub:recent';
const TOOL_STATE_KEYS: Record<string, string> = {
  'db-security': 'tool:db-security:state',
  'db-events': 'tool:db-events:state',
  'model-compare': 'tool:model-compare:state',
  'visual-sync': 'tool:visual-sync:state',
};

const TOOL_ITEMS = [
  { id: 'db-security', name: tr('权限与用户管理', 'Permissions & Users'), icon: ShieldCheck, desc: tr('对象级权限、用户清单与授权 SQL 预览', 'Object-level grants, users and SQL preview') },
  { id: 'db-events', name: tr('事件与触发器', 'Events & Triggers'), icon: Bell, desc: tr('查看触发器/事件定义与筛选', 'Browse triggers/events with filters') },
  { id: 'model-compare', name: tr('模型对比', 'Model Compare'), icon: Layers, desc: tr('跨连接结构差异可视化', 'Visual schema diff across connections') },
  { id: 'visual-sync', name: tr('可视化同步向导', 'Visual Sync Wizard'), icon: RefreshCw, desc: tr('结构/数据同步统一向导', 'Unified wizard for schema/data sync') },
];

export function AdvancedToolsHub({ onOpenTool }: AdvancedToolsHubProps) {
  const [connections, setConnections] = useState<any[]>([]);
  const [stateVersion, setStateVersion] = useState(0);
  const [showResetConfirm, setShowResetConfirm] = useState(false);
  const [showClearToolConfirm, setShowClearToolConfirm] = useState(false);
  const [showClearRecentConfirm, setShowClearRecentConfirm] = useState(false);
  const [recentIds, setRecentIds] = useState<string[]>(() => {
    try {
      const raw = window.localStorage.getItem(RECENT_KEY);
      const parsed = raw ? JSON.parse(raw) : [];
      return Array.isArray(parsed) ? parsed.filter((x) => typeof x === 'string') : [];
    } catch {
      return [];
    }
  });
  const [selectedToolId, setSelectedToolId] = useState<string>(TOOL_ITEMS[0].id);

  const recentTools = useMemo(() => {
    const map = new Map(TOOL_ITEMS.map((t) => [t.id, t]));
    return recentIds.map((id) => map.get(id)).filter(Boolean) as typeof TOOL_ITEMS;
  }, [recentIds]);

  const selectedTool = useMemo(
    () => TOOL_ITEMS.find((t) => t.id === selectedToolId) || TOOL_ITEMS[0],
    [selectedToolId]
  );

  const selectedToolState = useMemo(() => {
    try {
      const key = TOOL_STATE_KEYS[selectedTool.id];
      const raw = key ? window.localStorage.getItem(key) : null;
      return raw ? JSON.parse(raw) : null;
    } catch {
      return null;
    }
  }, [selectedTool.id, recentIds, stateVersion]);

  useEffect(() => {
    const loadConnections = async () => {
      try {
        const cfg = await api.getConfig();
        const list = Array.isArray(cfg?.db_connections) ? cfg.db_connections : [];
        setConnections(list);
      } catch {
        setConnections([]);
      }
    };
    loadConnections();
  }, []);

  const connName = (id?: string) => {
    if (!id) return '-';
    const conn = connections.find((c) => String(c.id) === String(id));
    return conn?.name || id;
  };

  const statePreviewLines = useMemo(() => {
    const s = selectedToolState || {};
    if (selectedTool.id === 'db-security') {
      return [tr('连接', 'Connection') + `: ${connName(String(s.conn_id || ''))}`];
    }
    if (selectedTool.id === 'db-events') {
      return [
        tr('连接', 'Connection') + `: ${connName(String(s.conn_id || ''))}`,
        tr('标签', 'Tab') + `: ${String(s.active_tab || '-')}`,
        tr('筛选', 'Search') + `: ${String(s.search || '-')}`,
      ];
    }
    if (selectedTool.id === 'model-compare') {
      return [
        tr('源库', 'Source') + `: ${connName(String(s.source_db_id || ''))}`,
        tr('目标库', 'Target') + `: ${connName(String(s.target_db_id || ''))}`,
      ];
    }
    if (selectedTool.id === 'visual-sync') {
      return [
        tr('模式', 'Mode') + `: ${String(s.mode || '-')}`,
        tr('源库', 'Source') + `: ${connName(String(s.source_db_id || ''))}`,
        tr('目标库', 'Target') + `: ${connName(String(s.target_db_id || ''))}`,
        tr('表名', 'Table') + `: ${String(s.table_name || '-')}`,
        tr('主键', 'Primary Key') + `: ${String(s.primary_key || '-')}`,
      ];
    }
    return [];
  }, [selectedTool.id, selectedToolState, connections]);

  const openTool = (toolId: string) => {
    const next = [toolId, ...recentIds.filter((x) => x !== toolId)].slice(0, 6);
    setRecentIds(next);
    window.localStorage.setItem(RECENT_KEY, JSON.stringify(next));
    onOpenTool(toolId);
  };

  const clearCurrentToolState = () => {
    const key = TOOL_STATE_KEYS[selectedTool.id];
    if (!key) return;
    window.localStorage.removeItem(key);
    setStateVersion((v) => v + 1);
  };

  const clearRecent = () => {
    setRecentIds([]);
    window.localStorage.removeItem(RECENT_KEY);
  };

  const clearAllStates = () => {
    Object.values(TOOL_STATE_KEYS).forEach((key) => {
      window.localStorage.removeItem(key);
    });
    window.localStorage.removeItem(RECENT_KEY);
    setRecentIds([]);
    setStateVersion((v) => v + 1);
  };

  return (
    <div className="h-full overflow-y-auto bg-[#0a0c10] p-6">
      <div className="max-w-6xl mx-auto flex flex-col gap-6">
        <div className="border border-[#30363d] bg-[#161b22] rounded-xl p-5">
          <div className="flex items-start justify-between gap-3">
            <div>
              <div className="text-lg font-bold text-gray-100">{tr('高级工具中心', 'Advanced Tools Hub')}</div>
              <div className="text-sm text-gray-400 mt-1">
                {tr('统一访问高级数据库工具，支持最近使用记录。', 'Unified entry for advanced database tools with recent history.')}
              </div>
            </div>
            <button
              onClick={() => setShowResetConfirm(true)}
              className="px-3 py-1.5 text-xs rounded border border-red-500/30 text-red-300 hover:bg-red-500/10"
            >
              {tr('全部重置', 'Reset All')}
            </button>
          </div>
        </div>

        {recentTools.length > 0 && (
          <div className="border border-[#30363d] bg-[#161b22] rounded-xl p-5">
            <div className="flex items-center justify-between mb-3">
              <div className="text-sm font-semibold text-gray-200">{tr('最近使用', 'Recently Used')}</div>
              <button
                onClick={() => setShowClearRecentConfirm(true)}
                className="px-2 py-1 text-xs rounded border border-[#30363d] text-gray-300 hover:bg-[#21262d]"
              >
                {tr('清空', 'Clear')}
              </button>
            </div>
            <div className="flex flex-wrap gap-2">
              {recentTools.map((tool) => (
                <button
                  key={tool.id}
                  onClick={() => openTool(tool.id)}
                  className="px-3 py-1.5 text-xs rounded border border-[#30363d] bg-[#0d1117] text-gray-200 hover:bg-[#21262d]"
                >
                  {tool.name}
                </button>
              ))}
            </div>
          </div>
        )}

        <div className="grid grid-cols-1 lg:grid-cols-[1.2fr_1fr] gap-4">
          <div className="grid grid-cols-1 md:grid-cols-2 gap-4">
            {TOOL_ITEMS.map((tool) => (
              <button
                key={tool.id}
                onClick={() => setSelectedToolId(tool.id)}
                className={`text-left border rounded-xl p-4 transition-colors ${
                  selectedTool.id === tool.id
                    ? 'border-blue-500/40 bg-blue-500/10'
                    : 'border-[#30363d] bg-[#161b22] hover:bg-[#1b212b]'
                }`}
              >
                <div className="flex items-center gap-3">
                  <tool.icon className="w-5 h-5 text-blue-400" />
                  <div className="text-sm font-semibold text-gray-100">{tool.name}</div>
                </div>
                <div className="text-xs text-gray-400 mt-2">{tool.desc}</div>
              </button>
            ))}
          </div>

          <div className="border border-[#30363d] bg-[#161b22] rounded-xl p-4 flex flex-col gap-3">
            <div className="text-sm font-semibold text-gray-100">{tr('工具预览', 'Tool Preview')}</div>
            <div className="flex items-center gap-2">
              <selectedTool.icon className="w-4 h-4 text-blue-400" />
              <div className="text-sm text-gray-200">{selectedTool.name}</div>
            </div>
            <div className="text-xs text-gray-400">{selectedTool.desc}</div>
            <div className="border-t border-[#30363d] pt-3">
              <div className="text-xs text-gray-500 mb-2">{tr('最近状态', 'Last State')}</div>
              {statePreviewLines.length === 0 && (
                <div className="text-xs text-gray-500">{tr('暂无状态记录。', 'No state saved yet.')}</div>
              )}
              {statePreviewLines.map((line, idx) => (
                <div key={`${selectedTool.id}-${idx}`} className="text-xs text-gray-300 py-0.5">
                  {line}
                </div>
              ))}
            </div>
            <button
              onClick={() => openTool(selectedTool.id)}
              className="mt-auto px-3 py-2 text-sm rounded border border-blue-500/40 text-blue-300 hover:bg-blue-500/10"
            >
              {tr('打开此工具', 'Open Tool')}
            </button>
            <button
              onClick={() => setShowClearToolConfirm(true)}
              className="px-3 py-2 text-sm rounded border border-[#30363d] text-gray-300 hover:bg-[#21262d]"
            >
              {tr('清空此工具状态', 'Clear Tool State')}
            </button>
          </div>
        </div>
      </div>
      <ConfirmDialog
        visible={showClearToolConfirm}
        title={tr('确认清空工具状态', 'Confirm Clear Tool State')}
        message={tr(
          '将清空当前工具的本地状态缓存，且无法恢复。是否继续？',
          'This will clear local state cache of current tool and cannot be undone. Continue?'
        )}
        confirmText={tr('确认清空', 'Confirm Clear')}
        onCancel={() => setShowClearToolConfirm(false)}
        onConfirm={() => {
          clearCurrentToolState();
          setShowClearToolConfirm(false);
        }}
      />
      <ConfirmDialog
        visible={showClearRecentConfirm}
        title={tr('确认清空最近使用', 'Confirm Clear Recent')}
        message={tr(
          '将清空高级工具中心的最近使用记录。是否继续？',
          'This will clear recently used records in Advanced Tools Hub. Continue?'
        )}
        confirmText={tr('确认清空', 'Confirm Clear')}
        onCancel={() => setShowClearRecentConfirm(false)}
        onConfirm={() => {
          clearRecent();
          setShowClearRecentConfirm(false);
        }}
      />
      <ConfirmDialog
        visible={showResetConfirm}
        title={tr('确认全部重置', 'Confirm Reset All')}
        message={tr(
          '将清空最近使用记录及所有高级工具状态缓存，且无法恢复。是否继续？',
          'This will clear recent history and all advanced tool states. This action cannot be undone. Continue?'
        )}
        confirmText={tr('确认重置', 'Confirm Reset')}
        onCancel={() => setShowResetConfirm(false)}
        onConfirm={() => {
          clearAllStates();
          setShowResetConfirm(false);
        }}
      />
    </div>
  );
}
