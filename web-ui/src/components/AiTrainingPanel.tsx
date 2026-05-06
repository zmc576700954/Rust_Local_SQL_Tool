import { useState, useEffect, useCallback } from 'react';
import { api } from '../api';
import { Database, FileText, Code, Trash2, Plus, Edit2, Check, Sparkles } from 'lucide-react';
import { useToast } from './Toast';
import { parseError } from '../utils';
import type { KnowledgeItem } from '../types';

interface AiTrainingPanelProps {
  onInsertSql?: (sql: string) => void;
}

export function AiTrainingPanel({ onInsertSql }: AiTrainingPanelProps) {
  const { toast } = useToast();
  const [activeTab, setActiveTab] = useState<'documentation' | 'sql' | 'ddl'>('documentation');
  const [items, setItems] = useState<KnowledgeItem[]>([]);
  const [loading, setLoading] = useState(false);
  const [isEditing, setIsEditing] = useState<string | number | null>(null);
  
  // Form state
  const [formData, setFormData] = useState<{title: string, content: string, description: string, is_golden: boolean}>({
    title: '', content: '', description: '', is_golden: false
  });

  const fetchKnowledge = useCallback(async () => {
    setLoading(true);
    try {
      const res = await api.getKnowledge();
      setItems(res || []);
    } catch (e: unknown) {
      toast("Failed to load AI knowledge: " + parseError(e).message, "error");
    } finally {
      setLoading(false);
    }
  }, [toast]);

  useEffect(() => {
    fetchKnowledge();
  }, [fetchKnowledge]);

  const handleSave = async (id?: string | number) => {
    if (!formData.title || !formData.content) {
      toast("Title and Content are required", "error");
      return;
    }

    try {
      const payload = {
        id: id || '',
        knowledge_type: activeTab,
        title: formData.title,
        content: formData.content,
        description: formData.description || null,
        is_golden: formData.is_golden,
      };

      if (id) {
        await api.updateKnowledge(payload);
        toast("Knowledge updated", "success");
      } else {
        await api.addKnowledge(payload);
        toast("Knowledge added", "success");
      }
      setIsEditing(null);
      fetchKnowledge();
    } catch (e: unknown) {
      toast("Failed to save: " + parseError(e).message, "error");
    }
  };

  const handleDelete = async (id: string | number) => {
    try {
      await api.deleteKnowledge(id);
      toast("Knowledge deleted", "success");
      fetchKnowledge();
    } catch (e: unknown) {
      toast("Failed to delete: " + parseError(e).message, "error");
    }
  };

  const startEdit = (item?: KnowledgeItem) => {
    if (item) {
      setFormData({
        title: item.title,
        content: item.content,
        description: item.description || '',
        is_golden: item.is_golden || false
      });
      setIsEditing(item.id);
    } else {
      setFormData({ title: '', content: '', description: '', is_golden: false });
      setIsEditing('new');
    }
  };

  const handleToggleGolden = async (item: KnowledgeItem) => {
    // Optimistic update and state rollback on failure
    const previousItems = [...items];
    setItems(items.map(i => i.id === item.id ? { ...i, is_golden: !i.is_golden } : i));

    try {
      await api.updateKnowledge({
        ...item,
        is_golden: !item.is_golden
      });
      toast(`Knowledge ${!item.is_golden ? 'marked as golden' : 'unmarked as golden'}`, "success");
    } catch (e: unknown) {
      // Rollback state on error
      setItems(previousItems);
      toast("Failed to update status: " + parseError(e).message, "error");
    }
  };

  const filteredItems = items.filter(i => i.knowledge_type === activeTab);

  return (
    <div className="flex flex-col h-full bg-[#0a0c10] text-gray-300">
      <div className="p-4 border-b border-[#30363d] bg-[#0d1117] flex items-center gap-2">
        <Sparkles className="w-5 h-5 text-purple-400" />
        <h2 className="font-semibold text-gray-200">AI Knowledge Base</h2>
      </div>
      
      <div className="flex border-b border-[#30363d] bg-[#0d1117] flex-wrap">
        <button
          onClick={() => setActiveTab('documentation')}
          className={`flex-1 py-2 px-1 text-xs font-medium flex items-center justify-center gap-1 ${activeTab === 'documentation' ? 'text-purple-400 border-b-2 border-purple-500 bg-[#161b22]' : 'text-gray-400 hover:text-gray-200 hover:bg-[#161b22]'}`}
        >
          <FileText className="w-3.5 h-3.5" />
          Doc
        </button>
        <button
          onClick={() => setActiveTab('sql')}
          className={`flex-1 py-2 px-1 text-xs font-medium flex items-center justify-center gap-1 ${activeTab === 'sql' ? 'text-blue-400 border-b-2 border-blue-500 bg-[#161b22]' : 'text-gray-400 hover:text-gray-200 hover:bg-[#161b22]'}`}
        >
          <Code className="w-3.5 h-3.5" />
          Snippets
        </button>
        <button
          onClick={() => setActiveTab('ddl')}
          className={`flex-1 py-2 px-1 text-xs font-medium flex items-center justify-center gap-1 ${activeTab === 'ddl' ? 'text-green-400 border-b-2 border-green-500 bg-[#161b22]' : 'text-gray-400 hover:text-gray-200 hover:bg-[#161b22]'}`}
        >
          <Database className="w-3.5 h-3.5" />
          DDL
        </button>
      </div>

      <div className="flex-1 overflow-y-auto p-4 space-y-4 relative">
        <div className="flex justify-between items-center mb-2">
          <p className="text-xs text-gray-500">
            {activeTab === 'documentation' && 'Business rules, terminology, and naming conventions.'}
            {activeTab === 'sql' && 'High-quality SQL examples for the AI to learn from.'}
            {activeTab === 'ddl' && 'Table structures and column comments.'}
          </p>
          {!isEditing && (
            <button 
              onClick={() => startEdit()} 
              className="flex items-center gap-1 text-xs bg-purple-500/10 text-purple-400 hover:bg-purple-500/20 px-2 py-1.5 rounded border border-purple-500/20 transition-colors"
            >
              <Plus className="w-3.5 h-3.5" />
              Add Knowledge
            </button>
          )}
        </div>

        {isEditing && (
          <div className="bg-[#161b22] border border-[#30363d] rounded-lg p-4 shadow-lg mb-6">
            <h3 className="text-sm font-medium text-gray-200 mb-3 flex items-center gap-2">
              {isEditing === 'new' ? <Plus className="w-4 h-4 text-purple-400"/> : <Edit2 className="w-4 h-4 text-purple-400"/>}
              {isEditing === 'new' ? 'Add' : 'Edit'} {activeTab.toUpperCase()}
            </h3>
            <div className="space-y-3">
              <div>
                <label className="block text-xs text-gray-400 mb-1">Title / Question</label>
                <input 
                  type="text" 
                  value={formData.title}
                  onChange={e => setFormData({...formData, title: e.target.value})}
                  className="w-full bg-[#0d1117] border border-[#30363d] rounded px-3 py-1.5 text-sm text-gray-200 focus:border-purple-500 outline-none"
                  placeholder={activeTab === 'sql' ? "e.g., How to calculate daily active users?" : "e.g., Active User Definition"}
                />
              </div>
              
              {activeTab === 'sql' && (
                <>
                  <div>
                    <label className="block text-xs text-gray-400 mb-1">Description / Natural Language Query</label>
                    <textarea 
                      value={formData.description}
                      onChange={e => setFormData({...formData, description: e.target.value})}
                      className="w-full bg-[#0d1117] border border-[#30363d] rounded px-3 py-1.5 text-sm text-gray-200 focus:border-purple-500 outline-none"
                      rows={2}
                      placeholder="The exact natural language question this SQL answers."
                    />
                  </div>
                  <div className="flex items-center gap-2 mt-2 mb-2">
                    <input 
                      type="checkbox" 
                      id="isGolden"
                      checked={formData.is_golden}
                      onChange={e => setFormData({...formData, is_golden: e.target.checked})}
                      className="w-4 h-4 rounded border-gray-300 text-purple-600 focus:ring-purple-500 bg-[#0d1117]"
                    />
                    <label htmlFor="isGolden" className="text-xs text-gray-300 flex items-center gap-1 cursor-pointer">
                      <Sparkles className="w-3.5 h-3.5 text-yellow-400" />
                      Mark as Golden (Train AI)
                    </label>
                  </div>
                </>
              )}

              <div>
                <label className="block text-xs text-gray-400 mb-1">Content / SQL</label>
                <textarea 
                  value={formData.content}
                  onChange={e => setFormData({...formData, content: e.target.value})}
                  className="w-full bg-[#0d1117] border border-[#30363d] rounded px-3 py-1.5 text-sm text-gray-200 focus:border-purple-500 outline-none font-mono"
                  rows={6}
                  placeholder={activeTab === 'sql' ? "SELECT ... FROM ..." : "Content..."}
                />
              </div>
              
              <div className="flex justify-end gap-2 pt-2">
                <button 
                  onClick={() => setIsEditing(null)}
                  className="px-3 py-1.5 text-xs text-gray-400 hover:text-gray-200 transition-colors"
                >
                  Cancel
                </button>
                <button 
                  onClick={() => handleSave(isEditing === 'new' ? undefined : isEditing)}
                  className="flex items-center gap-1 px-3 py-1.5 text-xs bg-purple-600 hover:bg-purple-500 text-white rounded transition-colors shadow-sm"
                >
                  <Check className="w-3.5 h-3.5" />
                  Save
                </button>
              </div>
            </div>
          </div>
        )}

        {loading && !isEditing ? (
          <div className="text-center text-gray-500 py-10 animate-pulse">Loading...</div>
        ) : filteredItems.length === 0 && !isEditing ? (
          <div className="text-center text-gray-500 py-10 flex flex-col items-center">
            <Sparkles className="w-10 h-10 mb-2 opacity-20" />
            <p>No knowledge found for {activeTab}.</p>
            <p className="text-xs mt-1">Add items to train the AI.</p>
          </div>
        ) : (
          <div className="space-y-3">
            {filteredItems.map(item => (
              <div 
                key={item.id} 
                className="bg-[#161b22] border border-[#30363d] rounded-lg p-4 group hover:border-[#8b5cf6]/50 transition-colors cursor-pointer"
                onDoubleClick={() => {
                  if (activeTab === 'sql' && onInsertSql) {
                    onInsertSql(item.content);
                  }
                }}
              >
                <div className="flex justify-between items-start mb-2">
                  <div className="flex items-center gap-2">
                    {item.is_golden && <span title="Golden Snippet"><Sparkles className="w-3.5 h-3.5 text-yellow-400" /></span>}
                    <h4 className="font-medium text-gray-200">{item.title}</h4>
                  </div>
                  <div className="flex gap-2 opacity-0 group-hover:opacity-100 transition-opacity items-center">
                    {activeTab === 'sql' && onInsertSql && (
                      <button 
                        onClick={(e) => { e.stopPropagation(); onInsertSql(item.content); }}
                        className="text-blue-400 hover:text-blue-300 text-xs px-2"
                        title="Insert to Editor"
                      >
                        Insert
                      </button>
                    )}
                    <button 
                      onClick={(e) => { e.stopPropagation(); startEdit(item); }}
                      className="text-gray-400 hover:text-purple-400"
                      title="Edit"
                    >
                      <Edit2 className="w-3.5 h-3.5" />
                    </button>
                    <button 
                      onClick={(e) => { e.stopPropagation(); handleDelete(item.id); }}
                      className="text-gray-400 hover:text-red-400"
                      title="Delete"
                    >
                      <Trash2 className="w-3.5 h-3.5" />
                    </button>
                  </div>
                </div>
                
                {item.description && (
                  <p className="text-xs text-gray-400 mb-2 italic">"{item.description}"</p>
                )}
                
                <div className="bg-[#0d1117] rounded p-3 overflow-x-auto">
                  <pre className="text-xs text-gray-300 font-mono whitespace-pre-wrap break-all">
                    {item.content}
                  </pre>
                </div>
                
                <div className="mt-3 flex justify-between items-center">
                  <div>
                    {activeTab === 'sql' && (
                      <label className="flex items-center gap-2 cursor-pointer text-xs text-gray-400 hover:text-gray-300 transition-colors" onClick={e => e.stopPropagation()}>
                        <div className="relative inline-block w-8 mr-1 align-middle select-none transition duration-200 ease-in">
                          <input 
                            type="checkbox" 
                            name="toggle" 
                            className="toggle-checkbox absolute block w-4 h-4 rounded-full bg-white border-4 appearance-none cursor-pointer transition-transform duration-200 ease-in-out"
                            style={{ transform: item.is_golden ? 'translateX(100%)' : 'translateX(0)', borderColor: item.is_golden ? '#8b5cf6' : '#4b5563' }}
                            checked={!!item.is_golden}
                            onChange={() => handleToggleGolden(item)}
                          />
                          <div className={`toggle-label block overflow-hidden h-4 rounded-full cursor-pointer transition-colors duration-200 ease-in-out ${item.is_golden ? 'bg-purple-500' : 'bg-gray-600'}`}></div>
                        </div>
                        {item.is_golden ? 'Golden (Active)' : 'Normal Snippet'}
                      </label>
                    )}
                  </div>
                  <span className="text-[10px] text-gray-500">
                    Updated: {item.updated_at ? new Date(item.updated_at * 1000).toLocaleString() : 'N/A'}
                  </span>
                </div>
              </div>
            ))}
          </div>
        )}
      </div>
    </div>
  );
}
