const BASE_URL = process.env.E2E_BACKEND_URL || 'http://127.0.0.1:3000'
const MYSQL_URL = process.env.E2E_MYSQL_URL || 'mysql://root:password@127.0.0.1:3306/e2e'
const MARIADB_URL = process.env.E2E_MARIADB_URL || 'mysql://root:password@127.0.0.1:3307/e2e'

function fail(message) {
  const err = new Error(message)
  err.name = 'E2ESmokeError'
  throw err
}

async function httpJson(method, path, body) {
  const res = await fetch(`${BASE_URL}${path}`, {
    method,
    headers: { 'content-type': 'application/json' },
    body: body ? JSON.stringify(body) : undefined,
  })
  const text = await res.text()
  let data = null
  if (text) {
    try {
      data = JSON.parse(text)
    } catch {
      data = text
    }
  }
  return { ok: res.ok, status: res.status, data }
}

async function waitForJob(jobId, timeoutMs = 30000) {
  const start = Date.now()
  while (Date.now() - start < timeoutMs) {
    const res = await httpJson('GET', `/tools/jobs/${jobId}`)
    if (!res.ok) fail(`job status failed: ${res.status} ${JSON.stringify(res.data)}`)
    const status = res.data?.status
    if (status === 'completed') return res.data
    if (status === 'error') fail(`job error: ${res.data?.error || 'unknown'}`)
    if (status === 'canceled') fail('job canceled')
    await new Promise(r => setTimeout(r, 200))
  }
  fail(`job timeout: ${jobId}`)
}

async function downloadArtifact(jobId, artifact) {
  const res = await fetch(`${BASE_URL}/tools/jobs/${jobId}/artifacts/${artifact}`)
  if (!res.ok) fail(`download ${artifact} failed: ${res.status}`)
  return await res.text()
}

async function ensureConfig() {
  const current = await httpJson('GET', '/config')
  if (!current.ok) fail(`get config failed: ${current.status}`)
  const cfg = current.data || {}

  const db_connections = Array.isArray(cfg.db_connections) ? cfg.db_connections : []
  const patchConnections = [
    { id: 'e2e-mysql', name: 'E2E MySQL', url: MYSQL_URL },
    { id: 'e2e-mariadb', name: 'E2E MariaDB', url: MARIADB_URL },
  ]

  const merged = [...db_connections]
  for (const c of patchConnections) {
    const idx = merged.findIndex(x => x?.id === c.id)
    if (idx >= 0) merged[idx] = { ...merged[idx], ...c }
    else merged.push(c)
  }

  const next = {
    ...cfg,
    db_connections: merged,
    active_db_id: 'e2e-mysql',
  }

  const saved = await httpJson('POST', '/config', next)
  if (!saved.ok) fail(`save config failed: ${saved.status} ${JSON.stringify(saved.data)}`)
}

async function seedData() {
  await httpJson('POST', '/execute', { sql: 'DROP TABLE IF EXISTS e2e_smoke_items' })
  const createSql =
    'CREATE TABLE e2e_smoke_items (id BIGINT PRIMARY KEY, name VARCHAR(255) NOT NULL, score DOUBLE NOT NULL, created_at DATETIME NOT NULL)'
  const created = await httpJson('POST', '/execute', { sql: createSql })
  if (!created.ok) fail(`create table failed: ${created.status} ${JSON.stringify(created.data)}`)

  const values = []
  for (let i = 1; i <= 25; i++) {
    values.push(`(${i}, 'item-${i}', ${i * 1.5}, NOW())`)
  }
  const insertSql = `INSERT INTO e2e_smoke_items (id, name, score, created_at) VALUES ${values.join(', ')}`
  const inserted = await httpJson('POST', '/execute', { sql: insertSql })
  if (!inserted.ok) fail(`insert failed: ${inserted.status} ${JSON.stringify(inserted.data)}`)
}

async function paginationCheck() {
  const res = await httpJson(
    'GET',
    `/table/data?table_name=${encodeURIComponent('e2e_smoke_items')}&page=2&page_size=10`
  )
  if (!res.ok) fail(`pagination failed: ${res.status} ${JSON.stringify(res.data)}`)
  const data = Array.isArray(res.data?.data) ? res.data.data : []
  if (data.length !== 10) fail(`pagination length mismatch: ${data.length}`)
  const firstId = data?.[0]?.id
  if (firstId !== 11) fail(`pagination first id mismatch: ${firstId}`)
}

async function exportImportJobCheck() {
  await httpJson('POST', '/execute', { sql: 'DROP TABLE IF EXISTS e2e_smoke_items_imported' })
  const createSql =
    'CREATE TABLE e2e_smoke_items_imported (id BIGINT PRIMARY KEY, name VARCHAR(255) NOT NULL, score DOUBLE NOT NULL, created_at DATETIME NOT NULL)'
  const created = await httpJson('POST', '/execute', { sql: createSql })
  if (!created.ok) fail(`create imported table failed: ${created.status}`)

  const start = await httpJson('POST', '/tools/jobs/export/start', {
    table_name: 'e2e_smoke_items',
    export_type: 'json',
    where_clause: "name LIKE 'item-%'",
    primary_key: 'id',
    pk_start: '5',
    pk_end: '20',
    window_limit: 7,
    window_offset: 3,
  })
  if (!start.ok) fail(`export job start failed: ${start.status} ${JSON.stringify(start.data)}`)
  const jobId = start.data?.job_id
  if (!jobId) fail('export job id missing')

  const job = await waitForJob(jobId)
  if (!job?.artifacts?.data_path) fail('export job artifact missing')

  const manifestText = await downloadArtifact(jobId, 'manifest')
  const manifest = JSON.parse(manifestText)
  if (!manifest?.sha256) fail('manifest sha256 missing')

  const dataText = await downloadArtifact(jobId, 'data')
  const rows = JSON.parse(dataText)
  if (!Array.isArray(rows) || rows.length === 0) fail('exported data invalid')

  const mapping = { id: 'id', name: 'name', score: 'score', created_at: 'created_at' }
  const imp = await httpJson('POST', '/tools/jobs/import/start', {
    table_name: 'e2e_smoke_items_imported',
    data: rows,
    mapping,
    skip_errors: false,
  })
  if (!imp.ok) fail(`import job start failed: ${imp.status} ${JSON.stringify(imp.data)}`)
  const importJobId = imp.data?.job_id
  if (!importJobId) fail('import job id missing')
  await waitForJob(importJobId)

  const countRes = await httpJson('POST', '/execute', {
    sql: 'SELECT COUNT(*) AS c FROM e2e_smoke_items_imported',
  })
  if (!countRes.ok) fail(`count imported failed: ${countRes.status}`)
  const c = countRes.data?.rows?.[0]?.c
  if (typeof c !== 'number' || c !== rows.length) fail(`imported count mismatch: ${c} vs ${rows.length}`)
}

async function syncAndTransferSmoke() {
  const diff = await httpJson('POST', '/tools/schema-sync/diff', {
    source_db_id: 'e2e-mysql',
    target_db_id: 'e2e-mariadb',
  })
  if (!diff.ok) fail(`schema sync diff failed: ${diff.status} ${JSON.stringify(diff.data)}`)

  const transfer = await httpJson('POST', '/tools/data-transfer/execute', {
    source_type: 'network_db',
    source_db_id: 'e2e-mariadb',
    source_table: 'e2e_smoke_items',
    target_url: '',
    target_table: 'e2e_smoke_items_imported',
    mode: 'Append',
    mappings: [
      { source_col: 'id', target_col: 'id' },
      { source_col: 'name', target_col: 'name' },
      { source_col: 'score', target_col: 'score' },
      { source_col: 'created_at', target_col: 'created_at' },
    ],
  })
  if (!transfer.ok) fail(`data transfer failed: ${transfer.status} ${JSON.stringify(transfer.data)}`)
  if (typeof transfer.data?.dml !== 'string' || !transfer.data.dml.includes('INSERT INTO')) {
    fail('data transfer dml missing')
  }
}

async function failureSplitSmoke() {
  const current = await httpJson('GET', '/config')
  if (!current.ok) fail(`get config failed: ${current.status}`)
  const cfg = current.data || {}
  const profiles = Array.isArray(cfg.ai_profiles) ? cfg.ai_profiles : []
  if (profiles.length === 0) return

  const nextProfiles = profiles.map(p =>
    p?.id === cfg.active_ai_profile_id
      ? {
          ...p,
          provider: 'openai',
          mode: 'relay',
          api_key: 'test',
          relay_url: 'http://127.0.0.1:1/v1/chat/completions',
        }
      : p
  )

  const saved = await httpJson('POST', '/config', { ...cfg, ai_profiles: nextProfiles })
  if (!saved.ok) fail(`save proxy config failed: ${saved.status}`)

  const health = await fetch(`${BASE_URL}/api/ai/health`)
  const text = await health.text()
  let body = null
  try {
    body = JSON.parse(text)
  } catch {
    body = text
  }
  if (health.status !== 504) fail(`expected 504 for external failure, got ${health.status}: ${text}`)
  if (body?.code !== 'ERR_AI_TIMEOUT') fail(`expected ERR_AI_TIMEOUT, got ${JSON.stringify(body)}`)
  if (typeof body?.type !== 'string' || body.type.length === 0) fail(`expected stable error type, got ${JSON.stringify(body)}`)
  if (body?.type !== 'timeout') fail(`expected type=timeout, got ${JSON.stringify(body)}`)
}

async function main() {
  await ensureConfig()
  await seedData()
  await paginationCheck()
  await exportImportJobCheck()
  await syncAndTransferSmoke()
  await failureSplitSmoke()
  console.log('E2E SMOKE OK')
}

main().catch(e => {
  console.error(e?.stack || String(e))
  process.exit(1)
})
