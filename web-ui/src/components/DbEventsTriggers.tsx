import { useEffect, useMemo, useState } from 'react';
import { api } from '../api';
import { useToast } from './Toast';
import { tr } from '../i18n';

interface DbEventsTriggersProps {
  onCancel: () => void;
}

const DB_EVENTS_STATE_KEY = 'tool:db-events:state';

type TriggerRow = {
  TRIGGER_NAME: string;
  EVENT_OBJECT_TABLE: string;
  ACTION_TIMING: string;
  EVENT_MANIPULATION: string;
  ACTION_STATEMENT: string;
  ACTION_ORIENTATION?: string;
  DEFINER?: string;
  CREATED?: string;
};

type EventRow = {
  EVENT_NAME: string;
  STATUS: string;
  EVENT_TYPE: string;
  INTERVAL_VALUE?: string;
  INTERVAL_FIELD?: string;
  STARTS?: string;
  ENDS?: string;
  DEFINER?: string;
  EVENT_DEFINITION: string;
  LAST_EXECUTED?: string;
};

export function DbEventsTriggers({ onCancel }: DbEventsTriggersProps) {
  const { toast } = useToast();
  const [isLoading, setIsLoading] = useState(false);
  const [configData, setConfigData] = useState<any>(null);
  const [connections, setConnections] = useState<any[]>([]);
  const [selectedConnId, setSelectedConnId] = useState('');
  const [activeTab, setActiveTab] = useState<'triggers' | 'events'>('triggers');
  const [search, setSearch] = useState('');

  const [triggers, setTriggers] = useState<TriggerRow[]>([]);
  const [events, setEvents] = useState<EventRow[]>([]);
  const [selectedTriggerName, setSelectedTriggerName] = useState('');
  const [selectedEventName, setSelectedEventName] = useState('');
  const [errorText, setErrorText] = useState('');

  const selectedConn = useMemo(
    () => connections.find((c) => String(c.id) === String(selectedConnId)),
    [connections, selectedConnId]
  );

  const isMysqlLike = useMemo(() => {
    const dbType = String(selectedConn?.db_type || '').toLowerCase();
    if (!dbType) return true;
    return dbType === 'mysql' || dbType === 'mariadb';
  }, [selectedConn]);

  const filteredTriggers = useMemo(() => {
    const kw = search.trim().toLowerCase();
    if (!kw) return triggers;
    return triggers.filter((t) => {
      const key = `${t.TRIGGER_NAME} ${t.EVENT_OBJECT_TABLE} ${t.EVENT_MANIPULATION} ${t.ACTION_TIMING}`.toLowerCase();
      return key.includes(kw);
    });
  }, [triggers, search]);

  const filteredEvents = useMemo(() => {
    const kw = search.trim().toLowerCase();
    if (!kw) return events;
    return events.filter((e) => {
      const key = `${e.EVENT_NAME} ${e.STATUS} ${e.EVENT_TYPE}`.toLowerCase();
      return key.includes(kw);
    });
  }, [events, search]);

  const selectedTrigger = useMemo(
    () => triggers.find((t) => t.TRIGGER_NAME === selectedTriggerName) || null,
    [triggers, selectedTriggerName]
  );
  const selectedEvent = useMemo(
    () => events.find((e) => e.EVENT_NAME === selectedEventName) || null,
    [events, selectedEventName]
  );

  const previewSql = useMemo(() => {
    if (activeTab === 'triggers' && selectedTrigger) {
      return [
        `-- ${tr('触发器定义预览', 'Trigger Definition Preview')}`,
        `CREATE TRIGGER \`${selectedTrigger.TRIGGER_NAME}\``,
        `${selectedTrigger.ACTION_TIMING} ${selectedTrigger.EVENT_MANIPULATION}`,
        `ON \`${selectedTrigger.EVENT_OBJECT_TABLE}\``,
        `FOR EACH ROW`,
        `${selectedTrigger.ACTION_STATEMENT || '-- statement missing'}`,
      ].join('\n');
    }
    if (activeTab === 'events' && selectedEvent) {
      const schedule =
        String(selectedEvent.EVENT_TYPE || '').toUpperCase() === 'RECURRING'
          ? `EVERY ${selectedEvent.INTERVAL_VALUE || '1'} ${selectedEvent.INTERVAL_FIELD || 'DAY'}`
          : `AT ${selectedEvent.STARTS || 'CURRENT_TIMESTAMP'}`;
      return [
        `-- ${tr('事件定义预览', 'Event Definition Preview')}`,
        `CREATE EVENT \`${selectedEvent.EVENT_NAME}\``,
        `ON SCHEDULE ${schedule}`,
        selectedEvent.STARTS ? `STARTS '${selectedEvent.STARTS}'` : '',
        selectedEvent.ENDS ? `ENDS '${selectedEvent.ENDS}'` : '',
        `DO ${selectedEvent.EVENT_DEFINITION || '-- definition missing'}`,
      ]
        .filter(Boolean)
        .join('\n');
    }
    return '';
  }, [activeTab, selectedTrigger, selectedEvent]);

  const loadMeta = async () => {
    setIsLoading(true);
    setErrorText('');
    try {
      const config = await api.getConfig();
      setConfigData(config);
      const list = Array.isArray(config?.db_connections) ? config.db_connections : [];
      setConnections(list);
      const savedRaw = window.localStorage.getItem(DB_EVENTS_STATE_KEY);
      const saved = savedRaw ? JSON.parse(savedRaw) : null;
      const savedConn = String(saved?.conn_id || '');
      const savedTab = String(saved?.active_tab || '');
      const savedSearch = String(saved?.search || '');
      const hasSavedConn = list.some((c: any) => String(c?.id || '') === savedConn);
      const nextId = hasSavedConn ? savedConn : (selectedConnId || String(config?.active_db_id || list?.[0]?.id || ''));
      setSelectedConnId(nextId);
      if (savedTab === 'triggers' || savedTab === 'events') setActiveTab(savedTab as 'triggers' | 'events');
      if (savedSearch) setSearch(savedSearch);
    } catch (e: any) {
      const msg = e?.message || String(e);
      setErrorText(msg);
      toast(tr('加载连接失败：', 'Failed to load connections: ') + msg, 'error');
    } finally {
      setIsLoading(false);
    }
  };

  const loadObjects = async (connId: string) => {
    if (!connId) return;
    setIsLoading(true);
    setErrorText('');
    try {
      if (!isMysqlLike) {
        setTriggers([]);
        setEvents([]);
        return;
      }

      const triggerSql = `
        SELECT TRIGGER_NAME, EVENT_OBJECT_TABLE, ACTION_TIMING, EVENT_MANIPULATION, ACTION_STATEMENT, ACTION_ORIENTATION, DEFINER, CREATED
        FROM information_schema.TRIGGERS
        WHERE TRIGGER_SCHEMA = DATABASE()
        ORDER BY EVENT_OBJECT_TABLE, TRIGGER_NAME
      `;
      const eventSql = `
        SELECT EVENT_NAME, STATUS, EVENT_TYPE, INTERVAL_VALUE, INTERVAL_FIELD, STARTS, ENDS, DEFINER, EVENT_DEFINITION, LAST_EXECUTED
        FROM information_schema.EVENTS
        WHERE EVENT_SCHEMA = DATABASE()
        ORDER BY EVENT_NAME
      `;

      const [triggerRes, eventRes] = await Promise.all([
        api.executeSql(triggerSql, false, connId),
        api.executeSql(eventSql, false, connId),
      ]);

      const triggerRows = Array.isArray(triggerRes?.rows) ? (triggerRes.rows as TriggerRow[]) : [];
      const eventRows = Array.isArray(eventRes?.rows) ? (eventRes.rows as EventRow[]) : [];
      setTriggers(triggerRows);
      setEvents(eventRows);
      setSelectedTriggerName(triggerRows[0]?.TRIGGER_NAME || '');
      setSelectedEventName(eventRows[0]?.EVENT_NAME || '');
    } catch (e: any) {
      const msg = e?.message || String(e);
      setErrorText(msg);
      toast(tr('加载事件/触发器失败：', 'Failed to load events/triggers: ') + msg, 'error');
      setTriggers([]);
      setEvents([]);
    } finally {
      setIsLoading(false);
    }
  };

  useEffect(() => {
    loadMeta();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    if (!selectedConnId) return;
    loadObjects(selectedConnId);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selectedConnId]);

  useEffect(() => {
    window.localStorage.setItem(
      DB_EVENTS_STATE_KEY,
      JSON.stringify({
        conn_id: selectedConnId,
        active_tab: activeTab,
        search,
        selected_trigger: selectedTriggerName,
        selected_event: selectedEventName,
      })
    );
  }, [selectedConnId, activeTab, search, selectedTriggerName, selectedEventName]);

  const saveDraftToConfig = async () => {
    if (!configData || !selectedConnId) return;
    setIsLoading(true);
    try {
      const list = Array.isArray(configData.db_connections) ? [...configData.db_connections] : [];
      const idx = list.findIndex((c: any) => String(c.id) === String(selectedConnId));
      if (idx >= 0) {
        list[idx] = {
          ...list[idx],
          event_trigger_draft: {
            updated_at: Date.now(),
            active_tab: activeTab,
            search,
            selected_trigger: selectedTriggerName,
            selected_event: selectedEventName,
          },
        };
      }
      const saved = await api.updateConfig({ ...configData, db_connections: list });
      setConfigData(saved);
      toast(tr('草稿已保存到连接配置。', 'Draft saved to connection config.'), 'success');
    } catch (e: any) {
      toast(tr('保存草稿失败：', 'Failed to save draft: ') + (e?.message || String(e)), 'error');
    } finally {
      setIsLoading(false);
    }
  };

  return (
    <div className="flex flex-col h-full bg-[#161b22] text-gray-300 rounded-xl overflow-hidden shadow-2xl border border-[#30363d]">
      <div className="px-6 py-4 border-b border-[#30363d] flex items-center justify-between bg-[#0d1117] shrink-0">
        <h3 className="text-gray-200 font-bold text-lg">{tr('事件 / 触发器', 'Events / Triggers')}</h3>
        <button onClick={onCancel} className="text-gray-500 hover:text-white transition-colors">
          {tr('关闭', 'Close')}
        </button>
      </div>

      <div className="px-6 py-3 border-b border-[#30363d] bg-[#0d1117] flex items-center gap-2 shrink-0">
        <select
          value={selectedConnId}
          onChange={(e) => setSelectedConnId(e.target.value)}
          className="min-w-[260px] bg-[#161b22] border border-[#30363d] rounded px-3 py-1.5 text-sm text-gray-200"
        >
          <option value="">{tr('-- 选择连接 --', '-- Select connection --')}</option>
          {connections.map((c) => (
            <option key={c.id} value={c.id}>
              {c.name || c.id}
            </option>
          ))}
        </select>
        <input
          value={search}
          onChange={(e) => setSearch(e.target.value)}
          placeholder={tr('筛选名称...', 'Filter name...')}
          className="flex-1 bg-[#161b22] border border-[#30363d] rounded px-3 py-1.5 text-sm text-gray-200"
        />
        <button
          onClick={() => loadObjects(selectedConnId)}
          disabled={!selectedConnId || isLoading}
          className="px-3 py-1.5 rounded border border-[#30363d] text-sm text-gray-200 hover:bg-[#30363d] disabled:opacity-50"
        >
          {tr('刷新', 'Refresh')}
        </button>
        <button
          onClick={saveDraftToConfig}
          disabled={!selectedConnId || isLoading}
          className="px-3 py-1.5 rounded border border-blue-500/40 text-sm text-blue-300 hover:bg-blue-500/10 disabled:opacity-50"
        >
          {tr('保存草稿', 'Save Draft')}
        </button>
      </div>

      {!isMysqlLike && selectedConnId && (
        <div className="mx-6 mt-4 border border-yellow-500/30 bg-yellow-500/10 rounded p-3 text-sm text-yellow-100">
          {tr('当前连接不是 MySQL/MariaDB，暂不支持事件/触发器信息读取。', 'Current connection is not MySQL/MariaDB. Events/triggers browsing is not supported.')}
        </div>
      )}

      {errorText && (
        <div className="mx-6 mt-4 border border-red-500/30 bg-red-500/10 rounded p-3 text-sm text-red-200 whitespace-pre-wrap">{errorText}</div>
      )}

      <div className="flex-1 min-h-0 px-6 py-4">
        <div className="h-full grid grid-cols-[320px_1fr] gap-4">
          <div className="min-h-0 border border-[#30363d] rounded overflow-hidden flex flex-col">
            <div className="flex border-b border-[#30363d]">
              <button
                className={`flex-1 py-2 text-sm ${activeTab === 'triggers' ? 'text-blue-400 border-b-2 border-blue-400' : 'text-gray-400 hover:text-gray-200'}`}
                onClick={() => setActiveTab('triggers')}
              >
                {tr('触发器', 'Triggers')} ({filteredTriggers.length})
              </button>
              <button
                className={`flex-1 py-2 text-sm ${activeTab === 'events' ? 'text-blue-400 border-b-2 border-blue-400' : 'text-gray-400 hover:text-gray-200'}`}
                onClick={() => setActiveTab('events')}
              >
                {tr('事件', 'Events')} ({filteredEvents.length})
              </button>
            </div>
            <div className="flex-1 overflow-y-auto bg-[#0d1117]">
              {activeTab === 'triggers' &&
                filteredTriggers.map((row) => (
                  <button
                    key={row.TRIGGER_NAME}
                    onClick={() => setSelectedTriggerName(row.TRIGGER_NAME)}
                    className={`w-full text-left px-3 py-2 border-b border-[#30363d] hover:bg-[#21262d] ${
                      selectedTriggerName === row.TRIGGER_NAME ? 'bg-blue-500/10' : ''
                    }`}
                  >
                    <div className="text-sm text-gray-200 truncate">{row.TRIGGER_NAME}</div>
                    <div className="text-xs text-gray-500 truncate">
                      {row.ACTION_TIMING} {row.EVENT_MANIPULATION} ON {row.EVENT_OBJECT_TABLE}
                    </div>
                  </button>
                ))}
              {activeTab === 'events' &&
                filteredEvents.map((row) => (
                  <button
                    key={row.EVENT_NAME}
                    onClick={() => setSelectedEventName(row.EVENT_NAME)}
                    className={`w-full text-left px-3 py-2 border-b border-[#30363d] hover:bg-[#21262d] ${
                      selectedEventName === row.EVENT_NAME ? 'bg-blue-500/10' : ''
                    }`}
                  >
                    <div className="text-sm text-gray-200 truncate">{row.EVENT_NAME}</div>
                    <div className="text-xs text-gray-500 truncate">
                      {row.STATUS} · {row.EVENT_TYPE}
                    </div>
                  </button>
                ))}
            </div>
          </div>

          <div className="min-h-0 border border-[#30363d] rounded bg-[#0d1117] p-4 flex flex-col gap-3">
            <div className="text-sm text-gray-300 font-medium">
              {activeTab === 'triggers' ? tr('详情与定义', 'Detail & Definition') : tr('详情与定义', 'Detail & Definition')}
            </div>

            {activeTab === 'triggers' && selectedTrigger && (
              <div className="text-xs text-gray-400 grid grid-cols-2 gap-2">
                <div>name: {selectedTrigger.TRIGGER_NAME}</div>
                <div>table: {selectedTrigger.EVENT_OBJECT_TABLE}</div>
                <div>timing: {selectedTrigger.ACTION_TIMING}</div>
                <div>event: {selectedTrigger.EVENT_MANIPULATION}</div>
                <div>definer: {selectedTrigger.DEFINER || '-'}</div>
                <div>created: {selectedTrigger.CREATED || '-'}</div>
              </div>
            )}

            {activeTab === 'events' && selectedEvent && (
              <div className="text-xs text-gray-400 grid grid-cols-2 gap-2">
                <div>name: {selectedEvent.EVENT_NAME}</div>
                <div>status: {selectedEvent.STATUS}</div>
                <div>type: {selectedEvent.EVENT_TYPE}</div>
                <div>
                  schedule: {selectedEvent.INTERVAL_VALUE || '-'} {selectedEvent.INTERVAL_FIELD || ''}
                </div>
                <div>starts: {selectedEvent.STARTS || '-'}</div>
                <div>ends: {selectedEvent.ENDS || '-'}</div>
                <div>definer: {selectedEvent.DEFINER || '-'}</div>
                <div>last: {selectedEvent.LAST_EXECUTED || '-'}</div>
              </div>
            )}

            <textarea
              readOnly
              value={previewSql}
              placeholder={tr('选择左侧条目后预览定义 SQL。', 'Select an item on the left to preview SQL definition.')}
              className="flex-1 min-h-[220px] bg-[#161b22] border border-[#30363d] rounded p-3 font-mono text-xs text-gray-300 outline-none resize-none"
            />
          </div>
        </div>
      </div>
    </div>
  );
}
