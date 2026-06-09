import { create } from 'zustand';
import { persist } from 'zustand/middleware';

interface WorkspaceState {
  // Layout
  sidebarWidth: number;
  resultsPanelHeight: number;
  sidebarTab: 'schema' | 'snippets' | 'history';
  isCompactActionBar: boolean;

  // Modal toggles
  showSettings: boolean;
  showOnboarding: boolean;
  showCommandPalette: boolean;
  showRulesPanel: boolean;
  showHelpModal: boolean;
  showMoreActions: boolean;
  showConfirmModal: boolean;
  showVariablesModal: boolean;

  // Context
  contextMenu: {
    x: number;
    y: number;
    items: Array<{ label: string; action: string; [key: string]: unknown }>;
  } | null;
  wizardConfig: unknown;

  // Resizing flags (not persisted)
  isResizingSidebar: boolean;
  isResizingResults: boolean;

  // Actions
  setSidebarWidth: (w: number) => void;
  setResultsPanelHeight: (h: number) => void;
  setSidebarTab: (tab: 'schema' | 'snippets' | 'history') => void;
  setCompactActionBar: (compact: boolean) => void;
  toggleSettings: () => void;
  setShowSettings: (show: boolean) => void;
  toggleOnboarding: () => void;
  setShowOnboarding: (show: boolean) => void;
  toggleCommandPalette: () => void;
  setShowCommandPalette: (show: boolean) => void;
  toggleRulesPanel: () => void;
  setShowRulesPanel: (show: boolean) => void;
  toggleHelpModal: () => void;
  setShowHelpModal: (show: boolean) => void;
  setShowMoreActions: (show: boolean) => void;
  setShowConfirmModal: (show: boolean) => void;
  setShowVariablesModal: (show: boolean) => void;
  setContextMenu: (menu: WorkspaceState['contextMenu']) => void;
  setWizardConfig: (config: unknown) => void;
  setIsResizingSidebar: (resizing: boolean) => void;
  setIsResizingResults: (resizing: boolean) => void;
}

export const useWorkspaceStore = create<WorkspaceState>()(
  persist(
    (set, get) => ({
      sidebarWidth: 240,
      resultsPanelHeight: 260,
      sidebarTab: 'schema',
      isCompactActionBar: false,

      showSettings: false,
      showOnboarding: false,
      showCommandPalette: false,
      showRulesPanel: false,
      showHelpModal: false,
      showMoreActions: false,
      showConfirmModal: false,
      showVariablesModal: false,

      contextMenu: null,
      wizardConfig: null,

      isResizingSidebar: false,
      isResizingResults: false,

      setSidebarWidth: (w) => set({ sidebarWidth: w }),
      setResultsPanelHeight: (h) => set({ resultsPanelHeight: h }),
      setSidebarTab: (tab) => set({ sidebarTab: tab }),
      setCompactActionBar: (compact) => set({ isCompactActionBar: compact }),

      toggleSettings: () => set({ showSettings: !get().showSettings }),
      setShowSettings: (show) => set({ showSettings: show }),
      toggleOnboarding: () => set({ showOnboarding: !get().showOnboarding }),
      setShowOnboarding: (show) => set({ showOnboarding: show }),
      toggleCommandPalette: () => set({ showCommandPalette: !get().showCommandPalette }),
      setShowCommandPalette: (show) => set({ showCommandPalette: show }),
      toggleRulesPanel: () => set({ showRulesPanel: !get().showRulesPanel }),
      setShowRulesPanel: (show) => set({ showRulesPanel: show }),
      toggleHelpModal: () => set({ showHelpModal: !get().showHelpModal }),
      setShowHelpModal: (show) => set({ showHelpModal: show }),
      setShowMoreActions: (show) => set({ showMoreActions: show }),
      setShowConfirmModal: (show) => set({ showConfirmModal: show }),
      setShowVariablesModal: (show) => set({ showVariablesModal: show }),

      setContextMenu: (menu) => set({ contextMenu: menu }),
      setWizardConfig: (config) => set({ wizardConfig: config }),
      setIsResizingSidebar: (resizing) => set({ isResizingSidebar: resizing }),
      setIsResizingResults: (resizing) => set({ isResizingResults: resizing }),
    }),
    {
      name: 'workspace-store',
      partialize: (state) => ({
        sidebarWidth: state.sidebarWidth,
        resultsPanelHeight: state.resultsPanelHeight,
        sidebarTab: state.sidebarTab,
        isCompactActionBar: state.isCompactActionBar,
      }),
    }
  )
);