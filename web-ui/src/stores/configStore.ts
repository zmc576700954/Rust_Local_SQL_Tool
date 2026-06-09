import { create } from 'zustand';
import type { ConfigData, DbConnection, DbType } from '../types';
import { api } from '../api';

interface ConfigState {
  configData: ConfigData | null;
  aiModelsData: unknown[] | null;
  dbType: DbType;
  isReady: boolean;
  isAiSwitching: boolean;

  // Actions
  fetchConfig: () => Promise<void>;
  fetchAiModels: () => Promise<void>;
  updateConfig: (patch: Partial<ConfigData>) => Promise<void>;
  setDbType: (dbType: DbType) => void;
  setIsReady: (ready: boolean) => void;
  setIsAiSwitching: (switching: boolean) => void;

  // Connection management
  switchActiveDb: (dbId: string) => Promise<void>;
  addConnection: (conn: DbConnection) => Promise<void>;
  updateConnection: (id: string, patch: Partial<DbConnection>) => Promise<void>;
  deleteConnection: (id: string) => Promise<void>;
  duplicateConnection: (id: string) => Promise<void>;
  disconnectConnection: (dbId: string) => Promise<void>;
  renameGroup: (groupName: string, newName: string) => Promise<void>;
  ungroupConnections: (groupName: string) => Promise<void>;
  batchMoveConnections: (ids: string[], targetGroup: string | null) => Promise<void>;
}

export const useConfigStore = create<ConfigState>()((set, get) => ({
  configData: null,
  aiModelsData: null,
  dbType: 'mysql',
  isReady: false,
  isAiSwitching: false,

  fetchConfig: async () => {
    try {
      const config = await api.getConfig();
      const dbType = (config.db_connections?.find(
        (c: DbConnection) => c.id === config.active_db_id
      )?.db_type ?? 'mysql') as DbType;
      set({ configData: config, dbType, isReady: true });
    } catch (e) {
      console.error('Failed to fetch config:', e);
      set({ isReady: false });
    }
  },

  fetchAiModels: async () => {
    try {
      const models = await api.getAiModels();
      set({ aiModelsData: models });
    } catch (e) {
      console.error('Failed to fetch AI models:', e);
    }
  },

  updateConfig: async (patch) => {
    const prev = get().configData;
    if (!prev) return;
    const merged = { ...prev, ...patch };
    try {
      await api.updateConfig(merged);
      set({ configData: merged });
    } catch (e) {
      console.error('Failed to update config:', e);
    }
  },

  setDbType: (dbType) => set({ dbType }),
  setIsReady: (ready) => set({ isReady: ready }),
  setIsAiSwitching: (switching) => set({ isAiSwitching: switching }),

  switchActiveDb: async (dbId) => {
    const config = get().configData;
    if (!config) return;
    set({ isAiSwitching: true });
    try {
      const updated = { ...config, active_db_id: dbId };
      await api.updateConfig(updated);
      const dbType = (updated.db_connections?.find(
        (c: DbConnection) => c.id === dbId
      )?.db_type ?? 'mysql') as DbType;
      set({ configData: updated, dbType, isAiSwitching: false });
    } catch (e) {
      console.error('Failed to switch active DB:', e);
      set({ isAiSwitching: false });
    }
  },

  addConnection: async (conn) => {
    const config = get().configData;
    if (!config) return;
    const connections = [...(config.db_connections ?? []), conn];
    const updated = { ...config, db_connections: connections };
    try {
      await api.updateConfig(updated);
      set({ configData: updated });
    } catch (e) {
      console.error('Failed to add connection:', e);
    }
  },

  updateConnection: async (id, patch) => {
    const config = get().configData;
    if (!config) return;
    const connections = (config.db_connections ?? []).map((c) =>
      c.id === id ? { ...c, ...patch } : c
    );
    const updated = { ...config, db_connections: connections };
    try {
      await api.updateConfig(updated);
      set({ configData: updated });
    } catch (e) {
      console.error('Failed to update connection:', e);
    }
  },

  deleteConnection: async (id) => {
    const config = get().configData;
    if (!config) return;
    const connections = (config.db_connections ?? []).filter((c) => c.id !== id);
    const updated = { ...config, db_connections: connections };
    // If deleting the active connection, clear active_db_id
    if (config.active_db_id === id) {
      updated.active_db_id = undefined;
    }
    try {
      await api.updateConfig(updated);
      set({ configData: updated });
    } catch (e) {
      console.error('Failed to delete connection:', e);
    }
  },

  duplicateConnection: async (id) => {
    const config = get().configData;
    if (!config) return;
    const source = (config.db_connections ?? []).find((c) => c.id === id);
    if (!source) return;
    const dup: DbConnection = {
      ...source,
      id: `${source.id}_copy_${Date.now()}`,
      name: `${source.name ?? source.id} (copy)`,
    };
    const connections = [...(config.db_connections ?? []), dup];
    const updated = { ...config, db_connections: connections };
    try {
      await api.updateConfig(updated);
      set({ configData: updated });
    } catch (e) {
      console.error('Failed to duplicate connection:', e);
    }
  },

  disconnectConnection: async (dbId) => {
    const config = get().configData;
    if (!config) return;
    if (config.active_db_id === dbId) {
      const updated = { ...config, active_db_id: undefined };
      try {
        await api.updateConfig(updated);
        set({ configData: updated });
      } catch (e) {
        console.error('Failed to disconnect:', e);
      }
    }
  },

  renameGroup: async (groupName, newName) => {
    const config = get().configData;
    if (!config) return;
    const connections = (config.db_connections ?? []).map((c) =>
      c.group_name === groupName ? { ...c, group_name: newName } : c
    );
    const updated = { ...config, db_connections: connections };
    try {
      await api.updateConfig(updated);
      set({ configData: updated });
    } catch (e) {
      console.error('Failed to rename group:', e);
    }
  },

  ungroupConnections: async (groupName) => {
    const config = get().configData;
    if (!config) return;
    const connections = (config.db_connections ?? []).map((c) =>
      c.group_name === groupName ? { ...c, group_name: null } : c
    );
    const updated = { ...config, db_connections: connections };
    try {
      await api.updateConfig(updated);
      set({ configData: updated });
    } catch (e) {
      console.error('Failed to ungroup:', e);
    }
  },

  batchMoveConnections: async (ids, targetGroup) => {
    const config = get().configData;
    if (!config) return;
    const connections = (config.db_connections ?? []).map((c) =>
      ids.includes(c.id) ? { ...c, group_name: targetGroup } : c
    );
    const updated = { ...config, db_connections: connections };
    try {
      await api.updateConfig(updated);
      set({ configData: updated });
    } catch (e) {
      console.error('Failed to batch move connections:', e);
    }
  },
}));