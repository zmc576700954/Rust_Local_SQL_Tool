import { useEffect, useMemo, useState } from 'react';
import { api } from '../api';
import { useToast } from './Toast';
import { StepWizard } from './StepWizard';
import type { WizardStep } from './StepWizard';
import { tr } from '../i18n';

interface DbSecurityManagerProps {
  onCancel: () => void;
}

const DB_SECURITY_STATE_KEY = 'tool:db-security:state';

type SecurityUser = {
  username: string;
  host: string;
  role: string;
  status: 'active' | 'disabled';
};

type ObjectPermission = {
  object_type: 'table' | 'view' | 'procedure';
  object_name: string;
  username: string;
  privileges: string[];
};

type SecurityProfile = {
  users: SecurityUser[];
  object_permissions: ObjectPermission[];
};

const DEFAULT_PROFILE: SecurityProfile = {
  users: [],
  object_permissions: [],
};

const PRIVILEGE_OPTIONS = ['SELECT', 'INSERT', 'UPDATE', 'DELETE', 'EXECUTE'];

export function DbSecurityManager({ onCancel }: DbSecurityManagerProps) {
  const { toast } = useToast();
  const [isLoading, setIsLoading] = useState(false);
  const [configData, setConfigData] = useState<any>(null);
  const [connections, setConnections] = useState<any[]>([]);
  const [selectedConnId, setSelectedConnId] = useState('');
  const [profile, setProfile] = useState<SecurityProfile>(DEFAULT_PROFILE);
  const [previewSql, setPreviewSql] = useState('');

  useEffect(() => {
    const load = async () => {
      setIsLoading(true);
      try {
        const config = await api.getConfig();
        setConfigData(config);
        const list = Array.isArray(config?.db_connections) ? config.db_connections : [];
        setConnections(list);
        const savedRaw = window.localStorage.getItem(DB_SECURITY_STATE_KEY);
        const saved = savedRaw ? JSON.parse(savedRaw) : null;
        const savedConn = String(saved?.conn_id || '');
        const hasSaved = list.some((c: any) => String(c?.id || '') === savedConn);
        const defaultId = hasSaved ? savedConn : String(config?.active_db_id || list?.[0]?.id || '');
        setSelectedConnId(defaultId);
      } catch (e: any) {
        toast(tr('加载连接配置失败：', 'Failed to load config: ') + (e?.message || String(e)), 'error');
      } finally {
        setIsLoading(false);
      }
    };
    load();
  }, [toast]);

  useEffect(() => {
    if (!selectedConnId) return;
    window.localStorage.setItem(
      DB_SECURITY_STATE_KEY,
      JSON.stringify({
        conn_id: selectedConnId,
      })
    );
  }, [selectedConnId]);

  useEffect(() => {
    const conn = connections.find((c) => String(c.id) === String(selectedConnId));
    const nextProfile = (conn?.security_profile || DEFAULT_PROFILE) as SecurityProfile;
    const users = Array.isArray(nextProfile.users) ? nextProfile.users : [];
    const permissions = Array.isArray(nextProfile.object_permissions) ? nextProfile.object_permissions : [];
    setProfile({ users, object_permissions: permissions });
    setPreviewSql('');
  }, [connections, selectedConnId]);

  const selectedConn = useMemo(
    () => connections.find((c) => String(c.id) === String(selectedConnId)),
    [connections, selectedConnId]
  );

  const updateUser = (idx: number, patch: Partial<SecurityUser>) => {
    const users = [...profile.users];
    users[idx] = { ...users[idx], ...patch };
    setProfile({ ...profile, users });
  };

  const updatePermission = (idx: number, patch: Partial<ObjectPermission>) => {
    const objectPermissions = [...profile.object_permissions];
    objectPermissions[idx] = { ...objectPermissions[idx], ...patch };
    setProfile({ ...profile, object_permissions: objectPermissions });
  };

  const togglePrivilege = (idx: number, privilege: string) => {
    const objectPermissions = [...profile.object_permissions];
    const row = objectPermissions[idx];
    const list = Array.isArray(row.privileges) ? row.privileges : [];
    const next = list.includes(privilege) ? list.filter((p) => p !== privilege) : [...list, privilege];
    objectPermissions[idx] = { ...row, privileges: next };
    setProfile({ ...profile, object_permissions: objectPermissions });
  };

  const buildPreviewSql = () => {
    const lines: string[] = [];
    for (const user of profile.users) {
      const username = String(user.username || '').trim();
      const host = String(user.host || '%').trim() || '%';
      if (!username) continue;
      lines.push(`CREATE USER IF NOT EXISTS '${username}'@'${host}' IDENTIFIED BY '***';`);
      if (String(user.status || 'active') === 'disabled') {
        lines.push(`ALTER USER '${username}'@'${host}' ACCOUNT LOCK;`);
      } else {
        lines.push(`ALTER USER '${username}'@'${host}' ACCOUNT UNLOCK;`);
      }
    }
    for (const p of profile.object_permissions) {
      const username = String(p.username || '').trim();
      const objName = String(p.object_name || '').trim();
      const objectType = String(p.object_type || 'table').toUpperCase();
      const privileges = Array.isArray(p.privileges) && p.privileges.length > 0 ? p.privileges.join(', ') : 'SELECT';
      if (!username || !objName) continue;
      lines.push(`GRANT ${privileges} ON ${objectType} \`${objName}\` TO '${username}'@'%';`);
    }
    lines.push('FLUSH PRIVILEGES;');
    setPreviewSql(lines.join('\n'));
  };

  const saveSecurityProfile = async () => {
    if (!configData || !selectedConnId) {
      toast(tr('请先选择连接。', 'Please select a connection first.'), 'error');
      return;
    }
    setIsLoading(true);
    try {
      const dbConnections = Array.isArray(configData.db_connections) ? [...configData.db_connections] : [];
      const idx = dbConnections.findIndex((c: any) => String(c.id) === String(selectedConnId));
      if (idx < 0) throw new Error(tr('连接不存在', 'Connection not found'));
      dbConnections[idx] = { ...dbConnections[idx], security_profile: profile };
      const saved = await api.updateConfig({ ...configData, db_connections: dbConnections });
      setConfigData(saved);
      setConnections(Array.isArray(saved?.db_connections) ? saved.db_connections : []);
      toast(tr('权限与用户配置已保存。', 'Security profile saved.'), 'success');
      onCancel();
    } catch (e: any) {
      toast(tr('保存失败：', 'Failed to save: ') + (e?.message || String(e)), 'error');
      throw e;
    } finally {
      setIsLoading(false);
    }
  };

  const step1Valid = !!selectedConnId;
  const step2Valid = step1Valid && profile.users.every((u) => String(u.username || '').trim().length > 0);
  const step3Valid = step2Valid && profile.object_permissions.every((p) => String(p.object_name || '').trim() && String(p.username || '').trim());

  const steps: WizardStep[] = [
    {
      id: 'connection',
      title: tr('选择连接', 'Select Connection'),
      isValid: step1Valid,
      content: (
        <div className="flex flex-col gap-4">
          <div className="text-sm text-gray-300 font-bold">
            {tr('步骤 1：选择要管理权限的数据库连接', 'Step 1: Select a database connection')}
          </div>
          <select
            value={selectedConnId}
            onChange={(e) => setSelectedConnId(e.target.value)}
            className="bg-[#0d1117] border border-[#30363d] rounded p-2 text-sm text-gray-300 outline-none focus:border-blue-500"
          >
            <option value="">{tr('-- 请选择连接 --', '-- Select connection --')}</option>
            {connections.map((conn) => (
              <option key={conn.id} value={conn.id}>
                {conn.name || conn.id}
              </option>
            ))}
          </select>
          {selectedConn && (
            <div className="text-xs text-gray-500 border border-[#30363d] rounded p-3 bg-[#0d1117] break-all">
              {tr('连接地址：', 'Connection URL: ')}
              {String(selectedConn.url || '')}
            </div>
          )}
        </div>
      ),
    },
    {
      id: 'users',
      title: tr('用户管理', 'Users'),
      isValid: step2Valid,
      content: (
        <div className="flex flex-col gap-4">
          <div className="flex items-center justify-between">
            <div className="text-sm text-gray-300 font-bold">{tr('步骤 2：维护用户清单', 'Step 2: Maintain users')}</div>
            <button
              type="button"
              onClick={() => setProfile({ ...profile, users: [...profile.users, { username: '', host: '%', role: 'readonly', status: 'active' }] })}
              className="px-3 py-1 rounded border border-[#30363d] text-xs text-gray-200 hover:bg-[#30363d]"
            >
              {tr('新增用户', 'Add User')}
            </button>
          </div>
          <div className="flex flex-col gap-2 max-h-[340px] overflow-y-auto">
            {profile.users.length === 0 && <div className="text-xs text-gray-500">{tr('暂无用户，请先新增。', 'No users yet. Add one first.')}</div>}
            {profile.users.map((u, idx) => (
              <div key={`${idx}-${u.username}`} className="border border-[#30363d] rounded p-3 bg-[#0d1117]">
                <div className="grid grid-cols-4 gap-2">
                  <input
                    value={u.username}
                    onChange={(e) => updateUser(idx, { username: e.target.value })}
                    placeholder={tr('用户名', 'Username')}
                    className="bg-[#161b22] border border-[#30363d] rounded px-2 py-1.5 text-xs text-gray-200 outline-none focus:border-blue-500"
                  />
                  <input
                    value={u.host}
                    onChange={(e) => updateUser(idx, { host: e.target.value })}
                    placeholder={tr('主机(%)', 'Host(%)')}
                    className="bg-[#161b22] border border-[#30363d] rounded px-2 py-1.5 text-xs text-gray-200 outline-none focus:border-blue-500"
                  />
                  <select
                    value={u.role}
                    onChange={(e) => updateUser(idx, { role: e.target.value })}
                    className="bg-[#161b22] border border-[#30363d] rounded px-2 py-1.5 text-xs text-gray-200 outline-none focus:border-blue-500"
                  >
                    <option value="readonly">readonly</option>
                    <option value="readwrite">readwrite</option>
                    <option value="dba">dba</option>
                  </select>
                  <div className="flex gap-2">
                    <select
                      value={u.status}
                      onChange={(e) => updateUser(idx, { status: e.target.value as SecurityUser['status'] })}
                      className="flex-1 bg-[#161b22] border border-[#30363d] rounded px-2 py-1.5 text-xs text-gray-200 outline-none focus:border-blue-500"
                    >
                      <option value="active">{tr('启用', 'Active')}</option>
                      <option value="disabled">{tr('禁用', 'Disabled')}</option>
                    </select>
                    <button
                      type="button"
                      onClick={() => setProfile({ ...profile, users: profile.users.filter((_, i) => i !== idx) })}
                      className="px-2 rounded border border-red-500/30 text-red-300 hover:bg-red-500/10 text-xs"
                    >
                      {tr('删', 'Del')}
                    </button>
                  </div>
                </div>
              </div>
            ))}
          </div>
        </div>
      ),
    },
    {
      id: 'permissions',
      title: tr('对象权限', 'Object Grants'),
      isValid: step3Valid,
      content: (
        <div className="flex flex-col gap-4">
          <div className="flex items-center justify-between">
            <div className="text-sm text-gray-300 font-bold">{tr('步骤 3：配置对象级权限并预览 SQL', 'Step 3: Configure grants and preview SQL')}</div>
            <div className="flex gap-2">
              <button
                type="button"
                onClick={() =>
                  setProfile({
                    ...profile,
                    object_permissions: [
                      ...profile.object_permissions,
                      { object_type: 'table', object_name: '', username: profile.users[0]?.username || '', privileges: ['SELECT'] },
                    ],
                  })
                }
                className="px-3 py-1 rounded border border-[#30363d] text-xs text-gray-200 hover:bg-[#30363d]"
              >
                {tr('新增权限', 'Add Grant')}
              </button>
              <button
                type="button"
                onClick={buildPreviewSql}
                className="px-3 py-1 rounded border border-blue-500/40 text-xs text-blue-300 hover:bg-blue-500/10"
              >
                {tr('生成 SQL 预览', 'Build SQL')}
              </button>
            </div>
          </div>
          <div className="flex flex-col gap-2 max-h-[220px] overflow-y-auto">
            {profile.object_permissions.length === 0 && (
              <div className="text-xs text-gray-500">{tr('暂无权限项，点击“新增权限”。', 'No grants yet. Click Add Grant.')}</div>
            )}
            {profile.object_permissions.map((p, idx) => (
              <div key={`${idx}-${p.object_name}`} className="border border-[#30363d] rounded p-3 bg-[#0d1117]">
                <div className="grid grid-cols-4 gap-2">
                  <select
                    value={p.object_type}
                    onChange={(e) => updatePermission(idx, { object_type: e.target.value as ObjectPermission['object_type'] })}
                    className="bg-[#161b22] border border-[#30363d] rounded px-2 py-1.5 text-xs text-gray-200 outline-none focus:border-blue-500"
                  >
                    <option value="table">TABLE</option>
                    <option value="view">VIEW</option>
                    <option value="procedure">PROCEDURE</option>
                  </select>
                  <input
                    value={p.object_name}
                    onChange={(e) => updatePermission(idx, { object_name: e.target.value })}
                    placeholder={tr('对象名', 'Object name')}
                    className="bg-[#161b22] border border-[#30363d] rounded px-2 py-1.5 text-xs text-gray-200 outline-none focus:border-blue-500"
                  />
                  <select
                    value={p.username}
                    onChange={(e) => updatePermission(idx, { username: e.target.value })}
                    className="bg-[#161b22] border border-[#30363d] rounded px-2 py-1.5 text-xs text-gray-200 outline-none focus:border-blue-500"
                  >
                    <option value="">{tr('-- 选择用户 --', '-- Select user --')}</option>
                    {profile.users.map((u, i) => (
                      <option key={`${u.username}-${i}`} value={u.username}>
                        {u.username}
                      </option>
                    ))}
                  </select>
                  <button
                    type="button"
                    onClick={() => setProfile({ ...profile, object_permissions: profile.object_permissions.filter((_, i) => i !== idx) })}
                    className="px-2 rounded border border-red-500/30 text-red-300 hover:bg-red-500/10 text-xs"
                  >
                    {tr('删除', 'Delete')}
                  </button>
                </div>
                <div className="mt-2 flex gap-2 flex-wrap">
                  {PRIVILEGE_OPTIONS.map((priv) => {
                    const active = (p.privileges || []).includes(priv);
                    return (
                      <button
                        key={priv}
                        type="button"
                        onClick={() => togglePrivilege(idx, priv)}
                        className={`px-2 py-1 rounded text-xs border transition-colors ${
                          active
                            ? 'border-blue-500/40 bg-blue-500/10 text-blue-300'
                            : 'border-[#30363d] bg-[#161b22] text-gray-300 hover:bg-[#21262d]'
                        }`}
                      >
                        {priv}
                      </button>
                    );
                  })}
                </div>
              </div>
            ))}
          </div>
          <textarea
            readOnly
            value={previewSql}
            placeholder={tr('点击“生成 SQL 预览”后显示权限脚本（仅预览，不会自动执行）。', 'Click Build SQL to preview grant statements (preview only).')}
            className="min-h-[120px] bg-[#0d1117] border border-[#30363d] rounded p-3 font-mono text-xs text-gray-300 outline-none resize-none"
          />
        </div>
      ),
    },
  ];

  return (
    <StepWizard
      title={tr('对象级权限与用户管理', 'Object Permissions & Users')}
      steps={steps}
      onCancel={onCancel}
      onFinish={saveSecurityProfile}
      isLoading={isLoading}
      finalWarningMessage={tr(
        '即将把该连接的权限模板保存到本地配置。此操作不会自动执行数据库 GRANT，仅用于后续统一管理与审计。',
        'This saves permission templates to local config only. It will not execute GRANT statements automatically.'
      )}
    />
  );
}
