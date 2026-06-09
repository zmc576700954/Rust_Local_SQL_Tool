import { create } from 'zustand';
import { persist } from 'zustand/middleware';

type TabType =
  | 'query'
  | 'table'
  | 'explain'
  | 'session-info'
  | 'go-live-reports'
  | 'go-live-audit'
  | 'perf-diagnostics'
  | 'advanced-center'
  | 'query-builder'
  | 'ai-training';

interface Tab {
  id: string;
  type: TabType;
  title: string;
  meta?: Record<string, unknown>;
}

interface TabState {
  sql: string;
  query: string;
  isGenerating: boolean;
  isExecuting: boolean;
  executeResult: unknown;
  executeResults: unknown[];
  transactionMode: boolean;
  transactionId: string | null;
  transactionState: 'idle' | 'active' | 'error';
  errorObj: unknown;
  chatHistory: unknown[];
  agentSteps: unknown[];
  [key: string]: unknown;
}

interface TabStoreState {
  tabs: Tab[];
  activeTabId: string | null;
  tabStates: Record<string, Partial<TabState>>;

  // Actions
  addTab: (tab: Tab, initialState?: Partial<TabState>) => void;
  closeTab: (id: string) => void;
  closeOthers: (keepId: string) => void;
  closeAll: () => void;
  setActiveTab: (id: string) => void;
  patchTabState: (id: string, patch: Partial<TabState>) => void;
  getActiveTabState: () => Partial<TabState> | undefined;
  updateTabMeta: (id: string, meta: Record<string, unknown>) => void;
}

const DEFAULT_TAB_STATE: TabState = {
  sql: '',
  query: '',
  isGenerating: false,
  isExecuting: false,
  executeResult: null,
  executeResults: [],
  transactionMode: false,
  transactionId: null,
  transactionState: 'idle',
  errorObj: null,
  chatHistory: [],
  agentSteps: [],
};

export const useTabStore = create<TabStoreState>()(
  persist(
    (set, get) => ({
      tabs: [],
      activeTabId: null,
      tabStates: {},

      addTab: (tab, initialState) => {
        const state = get();
        const existing = state.tabs.find((t) => t.id === tab.id);
        if (existing) {
          // Just activate the existing tab
          set({ activeTabId: tab.id });
          return;
        }
        set({
          tabs: [...state.tabs, tab],
          activeTabId: tab.id,
          tabStates: {
            ...state.tabStates,
            [tab.id]: { ...DEFAULT_TAB_STATE, ...initialState },
          },
        });
      },

      closeTab: (id) => {
        const state = get();
        const remaining = state.tabs.filter((t) => t.id !== id);
        const newTabStates = { ...state.tabStates };
        delete newTabStates[id];

        let newActiveId = state.activeTabId;
        if (state.activeTabId === id) {
          const idx = state.tabs.findIndex((t) => t.id === id);
          newActiveId = remaining[Math.min(idx, remaining.length - 1)]?.id ?? remaining[0]?.id ?? null;
        }

        set({ tabs: remaining, activeTabId: newActiveId, tabStates: newTabStates });
      },

      closeOthers: (keepId) => {
        const state = get();
        const kept = state.tabs.filter((t) => t.id === keepId);
        const newTabStates: Record<string, Partial<TabState>> = {};
        if (state.tabStates[keepId]) {
          newTabStates[keepId] = state.tabStates[keepId];
        }
        set({ tabs: kept, activeTabId: keepId, tabStates: newTabStates });
      },

      closeAll: () => set({ tabs: [], activeTabId: null, tabStates: {} }),

      setActiveTab: (id) => set({ activeTabId: id }),

      patchTabState: (id, patch) => {
        const state = get();
        const current = state.tabStates[id] ?? DEFAULT_TAB_STATE;
        set({
          tabStates: {
            ...state.tabStates,
            [id]: { ...current, ...patch },
          },
        });
      },

      getActiveTabState: () => {
        const state = get();
        if (!state.activeTabId) return undefined;
        return state.tabStates[state.activeTabId] ?? DEFAULT_TAB_STATE;
      },

      updateTabMeta: (id, meta) => {
        const state = get();
        const tabs = state.tabs.map((t) =>
          t.id === id ? { ...t, meta: { ...(t.meta ?? {}), ...meta } } : t
        );
        set({ tabs });
      },
    }),
    {
      name: 'tab-store',
      partialize: (state) => ({
        tabs: state.tabs,
        activeTabId: state.activeTabId,
      }),
    }
  )
);