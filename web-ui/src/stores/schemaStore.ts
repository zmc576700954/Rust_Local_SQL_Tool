import { create } from 'zustand';
import type { SchemaResponse } from '../types';
import { api } from '../api';

interface SchemaState {
  schemaData: SchemaResponse | null;
  isRefreshingSchema: boolean;

  // Actions
  fetchSchema: () => Promise<void>;
  refreshSchema: () => Promise<void>;
  setSchemaData: (data: SchemaResponse | null) => void;
}

export const useSchemaStore = create<SchemaState>()((set) => ({
  schemaData: null,
  isRefreshingSchema: false,

  fetchSchema: async () => {
    set({ isRefreshingSchema: true });
    try {
      const data = await api.getSchema();
      set({ schemaData: data, isRefreshingSchema: false });
    } catch (e) {
      console.error('Failed to fetch schema:', e);
      set({ isRefreshingSchema: false });
    }
  },

  refreshSchema: async () => {
    set({ isRefreshingSchema: true });
    try {
      const data = await api.getSchema();
      set({ schemaData: data, isRefreshingSchema: false });
    } catch (e) {
      console.error('Failed to refresh schema:', e);
      set({ isRefreshingSchema: false });
    }
  },

  setSchemaData: (data) => set({ schemaData: data }),
}));