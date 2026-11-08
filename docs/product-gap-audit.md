# 产品功能体检报告（存量代码只读审计）

- 审计对象：`/Users/zhumingchen/Rust_Local_SQL_Tool`（core_lib / web-server / e2e-runner / web-ui + Tauri 壳）
- 审计基线：分支 `main`，HEAD `8ec87ce`，工作区干净（`git status --porcelain` 为空）
- 审计方式：只读阅读源码与测试，未修改业务代码、未执行 git 写操作、未启动应用
- 严重度定义：
  - 🔴 **阻断使用**：该场景下功能不可用或在用户不知情时发生数据错误
  - 🟠 **影响正确性**：结果/状态与实际数据库不一致，或安全/一致性策略被绕过
  - 🟡 **体验缺陷**：功能可用，但提示、反馈、性能或一致性明显不足

> 说明：结论优先依据代码。`CLAUDE.md` 与 `docs/superpowers/specs/2026-06-03-database-operations-optimization-design.md` 中的能力描述与实际实现存在偏差，本文在“文档承诺 vs 代码实际”处单独对比，不把文档当证据。

---

## 0. 结论速览

| # | 问题 | 严重度 | 证据 |
|---|------|--------|------|
| 1 | 桌面（Tauri）打包态只有 6 个 invoke 命令，其余功能依赖相对路径 `/backend`，而壳内既无 sidecar 也无绝对后端地址 | 🔴 桌面 | [lib.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-ui/src-tauri/src/lib.rs#L1470-L1482) / [api.ts](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-ui/src/api.ts#L117-L123) |
| 2 | PostgreSQL / SQLite 被标为 A 级“完整支持”，但连接测试、SQL 执行、结构浏览、表数据、DDL、同步全部写死 MySQL 连接池 | 🔴 | [perf.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-server/src/handlers/perf.rs#L1175-L1179) / [capability.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/core_lib/src/db/capability.rs#L28-L41) |
| 3 | SSH 隧道 / SSL 只在“测试连接”时生效，真正运行期连接完全不使用 | 🔴 | [main.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-server/src/main.rs#L597-L612) / [client.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/core_lib/src/db/client.rs#L221-L228) |
| 4 | 保存配置时“先落盘再连接”，失败后配置已变但旧连接仍在用；断开/删除激活连接后旧连接池继续服务 | 🟠 | [main.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-server/src/main.rs#L1384-L1409) / [main.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-server/src/main.rs#L1647-L1665) |
| 5 | 导入 / 导出 / 生成测试数据只作用于“当前激活连接”，请求体里根本没有 `db_id` 字段；多连接标签页下会写错库 | 🟠 | [go_live.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-server/src/handlers/go_live.rs#L268-L275) / [main.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-server/src/main.rs#L2470-L2492) |
| 6 | 数据表格保存时事务开启失败被吞掉并继续写，桌面下事务在 Tauri 中开启而 CRUD 走 HTTP，必现 “session not found” | 🟠 | [DataTable](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-ui/src/components/DataTable/index.tsx#L1545-L1565) / [api.ts](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-ui/src/api.ts#L374-L386) |
| 7 | 复制连接复制的是脱敏后的 URL（密码为 `***`），新连接不可用 | 🟠 | [App](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-ui/src/App/index.tsx#L834-L845) |
| 8 | DDL / EXPLAIN / 导入导出任务没有任何数据库超时；查询超时在无 cancel token 时不 KILL 服务端语句 | 🟠 | [main.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-server/src/main.rs#L2261-L2265) / [perf.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-server/src/handlers/perf.rs#L1197-L1210) |
| 9 | 对非以 `SELECT` 开头的查询静默追加 `LIMIT 1000`；含 `LIMIT` 子串则完全不限制，大结果集可拖垮前端 | 🟠 | [perf.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-server/src/handlers/perf.rs#L1090-L1100) |
| 10 | `routes.rs` 的新路由与服务层从未被调用，生产实际走 `main.rs` 里的旧路由/旧 handler，两套实现并存 | 🟠 维护性地基 | [routes.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-server/src/routes.rs#L21) / [main.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-server/src/main.rs#L913-L936) |

---

## 一、底层连接与执行链路

### 1.1 DbClient 与连接缓存的实际行为

**实现现状**

- 激活连接只有一个：`AppState.db_client: RwLock<Option<DbClient>>`；非激活连接按 `db_id`/URL 进入 `db_client_cache`，TTL 600s（[main.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-server/src/main.rs#L746-L750)、[go_live.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-server/src/handlers/go_live.rs#L2296-L2325)）。
- `DbClient::new` 用 `connect_with` **立即建立真实连接**，并按 `DbPoolConfig` 建池；引擎只支持 MySQL / PostgreSQL / SQLite，其余返回 `Unsupported`（[client.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/core_lib/src/db/client.rs#L221-L294)）。
- 缓存键优先用 `db_id`，缓存命中时还比较 URL 是否变化；变化则新建（这一点是对的）。

**问题**

1. **保存配置会无条件重连，且存在“连接为空”的窗口**（🟠）
   [main.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-server/src/main.rs#L1384-L1409)：先 `save().await`，再判断“有激活 URL 就重连”。
   - 重连逻辑没有判断“URL 是否变化”，只在注释里写了 changed；因此每次保存任意设置（包括只改 AI 配置）都会 `pool.close().await` 并重建连接。
   - `take()` 后 `db_client` 变为 `None`，随后 `await close()`（sqlx 的 close 会等待连接归还，长查询期间可能阻塞数十秒），最后再写回。这个窗口内并发请求会拿到 “Database not connected”。
   - 连接失败时配置已落盘、内存配置已更新为**新 URL**，但 `db_client` 仍是**旧库连接**。UI 显示新库，实际执行仍在旧库。
   - 当 `get_active_db_url()` 为 `None`（断开、或删除最后一个连接）时，代码完全不进入重连分支，旧连接继续存活。

2. **断开/删除连接不会真正断开**（🟠）
   [update_config](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-server/src/main.rs#L1398-L1409) 只在 `url.is_some()` 时清理旧 client；[resolve_db_client_for_request](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-server/src/main.rs#L1647-L1665) 在 `db_id` 为空时直接回退到 `state.db_client`。
   结合前端“断开连接”只提交 `active_db_id: undefined`（[App](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-ui/src/App/index.tsx#L848-L858)），用户点“断开”后侧栏仍能重新拉到旧库结构，仍可执行语句。

3. **并发取用没有 single-flight**（🟡→🟠 资源侧）
   `get_temp_db_client` 是“读缓存 → 释放读锁 → 建连接 → 写缓存”，同一 `db_id` 的两个并发请求会各自建一个池，后写覆盖先写；被覆盖的池没有调用 `close()`，属于非优雅释放（不是永久泄漏，但会造成瞬时双倍连接）。
   同时临时连接走 `DbClient::new_default`（默认 `PoolConfig`），**忽略 `AppConfig` 里的池配置**，与激活连接不一致（[go_live.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-server/src/handlers/go_live.rs#L2308-L2325)）。

4. **TTL 语义**：过期只在读取时判断，过期条目不会被清理；因为键数量受连接数限制，影响可接受。`if active_db_id == db_id` 时直接复用激活连接、绕过 TTL，这是合理设计（见第五节）。

5. **失效重连不靠自研逻辑**：`sqlx` 池在取连接前会测试连接，MySQL 重启后池能自愈；真正缺的是“激活连接建立失败/断连后的用户可见状态”。服务器启动时若配置库连不上，只是启动成功、`db_client=None`，前端只能看到“未连接”，看不到失败原因（日志里才有）。

### 1.2 超时策略

`TimeoutPolicy` 默认值：`db_connect=10s`、`db_query=60s`（[timeout_policy.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/core_lib/src/timeout_policy.rs#L1-L52)）。各路径实际覆盖情况：

| 路径 | 是否生效 | 证据 / 备注 |
|------|---------|------|
| `/execute` SELECT 分支 | ✅ 60s；有 cancel token 时超时后 KILL QUERY | [perf.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-server/src/handlers/perf.rs#L1197-L1210) |
| `/execute` 写/DDL 分支 | ✅ 60s；无 token 时只丢弃 future | [perf.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-server/src/handlers/perf.rs#L1268-L1288) |
| `/execute/transaction` begin/commit/rollback | ✅ 60s | [main.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-server/src/main.rs#L1100-L1130) |
| `/table/data` | ✅ 60s，但错误文案写死“30 秒” | [main.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-server/src/main.rs#L2120-L2134) |
| `/db/test` | ✅ 10s 连接 + 60s ping，但用的是 `TimeoutPolicy::default()` 而非应用状态 | [main.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-server/src/main.rs#L563-L615) |
| `/table/ddl`（`execute_ddl`） | ❌ 无任何超时 | [main.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-server/src/main.rs#L2261-L2265) |
| `/sql/explain` | ❌ 无超时 | [main.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-server/src/main.rs#L2826-L2836) |
| 导出 / 导入任务 | ❌ 无数据库超时、无任务看门狗；取消是 abort 任务，不会 KILL 服务端语句 | [go_live.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-server/src/handlers/go_live.rs#L205-L300) |
| 前端整体请求 | 120s 兜底 | [api.ts](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-ui/src/api.ts#L117-L121) |

要点：

- **超时文案硬编码**：默认策略是 60s，但 `/execute` 与 `/table/data` 的错误文案写“已超过 30 秒”，会误导排障（[perf.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-server/src/handlers/perf.rs#L1209)、[main.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-server/src/main.rs#L2130-L2134)）。
- **无 cancel token 的超时不会终止数据库语句**：`active_query` 为 `None` 时超时只是 drop future，MySQL 端继续执行、连接被归还池；后续请求可能拿到一个“仍在跑旧语句”的连接。分块续拉 `executeSqlChunk` 不传 cancel token（[api.ts](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-ui/src/api.ts#L367-L372)），导出/导入也不注册取消句柄。
- **DDL 无超时**是硬伤：`ALTER TABLE` 在千万行表上锁表时，HTTP 连接会一直挂到前端 120s 超时，后端任务继续执行，用户既看不到进度也无法取消。

### 1.3 取消、分块流式、事务一致性

**取消（execute_cancel）**

- 只支持 MySQL：`KILL QUERY {connection_id}`（[client.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/core_lib/src/db/client.rs#L323-L330)）。
- 只有查询执行时带了 `cancel_token` 才会注册 `active_query`，才能被取消（[perf.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-server/src/handlers/perf.rs#L1146-L1176)）。
- 取消与查询自然完成的竞态：`canceled` 标志在 KILL 成功后才置位；若语句刚跑完，前端可能收到“已取消”但实际已生效（写语句尤其需要注意）🟡。
- 取消事务会话中的语句时，KILL 作用于该会话的固定连接；若此时同一连接上跑的是另一条 CRUD 语句（事务会话由 `Mutex` 串行化，正常情况下不会并发），边界行为依赖锁顺序，建议补测试。

**分块（chunk_offset / chunk_size）**

- 每次 200 行、预览上限 1000 行（[main.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-server/src/main.rs#L795-L798)）。
- 只对“以 `SELECT` 开头”的语句做 chunk 改写；`WITH`、`(` 开头、或 `select` 前有注释以外的会被走 `else if` 分支追加 `LIMIT 1000`，且不返回 `truncated`/`preview_cap` 标记 → **静默截断**（[perf.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-server/src/handlers/perf.rs#L1090-L1100)）。
- `contains("LIMIT")` 是子串判断：`SELECT * FROM limits`（列名/表名/字符串里出现 LIMIT）会**完全不加限制**，可能一次拉回数十万行 JSON。
- chunk 续拉是“重新执行同一 SQL + OFFSET”，没有快照/游标语义：数据变动时会重复或漏行；大表 OFFSET 越深越慢。这是常见取舍，但目前没有向用户提示。
- 执行历史保存的是**改写后的 SQL**（含 `LIMIT 200 OFFSET 0`），不是用户输入的原文（[perf.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-server/src/handlers/perf.rs#L1346-L1365)）；桌面同样如此（[lib.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-ui/src-tauri/src/lib.rs#L999-L1015)）。

**事务（execute_transaction / transaction_state）**

- 事务会话只支持 MySQL：会话连接类型是 `MySqlConnection`，通过 `mysql_acquire` 获取（[main.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-server/src/main.rs#L780-L840)）。
- Web 有 10 分钟空闲回收，回收时执行 `ROLLBACK`（[main.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-server/src/main.rs#L1062-L1098)）。**桌面端没有这个回收器**，只记录 `last_accessed` 未使用（[lib.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-ui/src-tauri/src/lib.rs#L960-L996)）→ 桌面长时间不提交会一直占着连接与行锁。
- Web 端 `COMMIT/ROLLBACK` 失败时直接 `return Err`，**没有移除会话**（[main.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-server/src/main.rs#L1928-L1936)）。连接若已断（MySQL 重启、网络断开），用户会陷入“每次 commit 都失败、也无法 rollback”的状态，只能等 10 分钟回收。
- 用户在查询框直接执行 `BEGIN`/`START TRANSACTION`：服务端不认为它是事务（`transaction_id` 为空），后续语句走池里的其他连接，**“同一事务”的假设不成立**，并且执行完的连接会带着未结束事务归还连接池。这是确定存在的语义错误（是否造成其他请求继承事务取决于 sqlx 对未跟踪事务的清理，建议实测确认）。
- `transaction_state` 由各接口返回，正常的 begin/commit/rollback 路径一致；但 `DataTable` 在 begin 失败后继续写（见 2.2），此时 `transaction_state` 与真实状态不一致。

### 1.4 Web（HTTP）与桌面（Tauri invoke）两条路径

Tauri 壳只注册了 6 个命令：`workbench_run`、`workbench_cancel`、`workbench_transaction`、`table_page`、`table_schema`、`perf_probe`（[lib.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-ui/src-tauri/src/lib.rs#L1470-L1482)）。其余全部经 axios 走 `/backend`，失败时按 `DESKTOP_HTTP_FALLBACK:` 前缀降级。

| 能力 | Web | 桌面 | 差异 / 风险 |
|------|-----|------|------------|
| 连接配置读写、测试、增删 | HTTP | 无 invoke，纯 HTTP | 打包态无后端即全部不可用 🔴 |
| 结构浏览 | HTTP `/schema` | `table_schema` + 其余 HTTP | 桌面端 schema 命令仅 MySQL |
| 表分页 | HTTP `/table/data` | `table_page` | 两边都只支持 MySQL；桌面忽略 SSH/SSL（直接 fallback） |
| SQL 执行 | `/execute` | `workbench_run` | 桌面按**前缀**判断 SELECT（`WITH`/`(` 被当写语句）；`force` 被丢弃（[lib.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-ui/src-tauri/src/lib.rs#L1022-L1038)） |
| 危险 SQL 校验 | sqlparser AST | 无 | 桌面依赖前端预检，后端不兜底 🟠 |
| 只读连接校验 | 有 | 无 | 桌面在“手动事务”模式下可对只读连接写入 🟠 |
| 事务 | 服务端会话表 + 回收器 | 独立会话表，无回收器 | 跨进程；桌面 begin 在 Tauri、CRUD 在 HTTP（见下） |
| SQL 历史 | 完整（db_id、行数、类型） | 字段大量为 None | 桌面历史信息缺失 |
| 超时策略 | 可配（默认 60s） | 永远 `TimeoutPolicy::default()` | 桌面忽略配置 |
| 执行计划 / 会话信息 / 导出 / 同步 / AI | HTTP | 无 invoke | 打包态不可用 |

**降级可靠性**

- `baseURL` 是相对路径 `/backend`（[api.ts](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-ui/src/api.ts#L117-L123)）。Web 模式由 Vite 代理（[vite.config.ts](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-ui/vite.config.ts#L16-L24)）或同源 web-server 提供；**打包后的 Tauri 页面 origin 是 `tauri://localhost` 之类，相对 `/backend` 不会到达 127.0.0.1:3000**。
- 壳配置里没有 sidecar / 外部可执行文件 / 启动后端的逻辑（[tauri.conf.json](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-ui/src-tauri/tauri.conf.json#L1-L40)），web-server 的 CORS 虽然允许 `tauri://localhost`（[main.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-server/src/main.rs#L897-L912)），但前端从未使用绝对地址，这段 CORS 实际没被用上。
- 降级只覆盖走 axios 的调用；流式 AI `/backend/chat/stream` 与导出下载用硬编码路径（[api.ts](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-ui/src/api.ts#L1-L40)、[WizardModal](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-ui/src/components/WizardModal.tsx#L132-L150)），失败时表现为空响应或下载失败，不会触发降级提示。
- 桌面端 `resolve_connection` 对 PostgreSQL/SQLite/SSH/SSL 直接返回 `fallback_required("unsupported_connection")`（[lib.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-ui/src-tauri/src/lib.rs#L360-L395)），在开发态（后端在跑）能降级成功，打包态则为不可用。

**跨进程事务**（条件性 🟠）

`api.executeTransactionAction` 在桌面走 invoke（Tauri 内存中的事务表），而 `crudInsert/Update/Delete` 永远是 HTTP（[api.ts](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-ui/src/api.ts#L374-L386)）。只要数据表格标签页进入“手动事务”模式，就会出现 begin 在 A 进程、写入请求到 B 进程，B 找不到事务 → 404 `transaction session not found`；即使能找到，也是两条不同连接，不在同一事务里。

### 1.5 各数据库类型：声明 vs 实际

`core_lib::db::capability` 声明 PostgreSQL/SQLite 为 Level A、支持结构内省/导入导出/结构同步/数据同步；SQL Server/Oracle 在 `runtime_capabilities` 里甚至声明了结构内省与导入导出（[capability.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/core_lib/src/db/capability.rs#L20-L75)）。实际：

| 引擎 | 连接池 | 连接测试 | SQL 执行 | 结构浏览 | 表数据 | DDL | CRUD（非事务） | 事务 | 同步/导入导出 |
|------|--------|---------|---------|---------|--------|-----|--------------|------|--------------|
| MySQL / MariaDB | ✅ | ✅ 仅 MySQL 解析 | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅（仅 MySQL 引擎） |
| PostgreSQL | ✅ 建池 | ❌ 用 MySQL 解析器 | ❌ `mysql_pool()?` | ❌ MySQL-only | ❌ | ❌ | ✅ | ❌ | ❌ |
| SQLite | ✅ 建池 | ❌ | ❌ | ❌ | ❌ | ❌ | ✅ | ❌ | ❌ |
| SQL Server | ❌ `Unsupported` | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ |
| MongoDB / Redis / Oracle | ❌ `Unsupported` | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ |

关键证据：

- `/db/test` 直接 `MySqlConnectOptions::from_str(&url)`（[main.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-server/src/main.rs#L563-L590)）。用 `postgres://` 连接会得到“连接地址格式错误”，而不是“测试成功”。
- `/execute` 的连接获取固定为 `db_client.mysql_pool()`（[perf.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-server/src/handlers/perf.rs#L1175-L1179)）；非 MySQL 直接得到 `PoolClosed`，用户看到“connection closed”，但连接其实是好的。
- 结构抽取全部 `mysql_pool`（[schema.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/core_lib/src/schema.rs#L1-L80)），`fetch_schema_for_db` 还会把错误 `.ok()?` 吞掉变成 `None`（[main.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-server/src/main.rs#L1480-L1500)）。
- `DatabaseOps`（包含 PostgreSQL/SQLite 的查询实现）**没有任何调用方**（[database_ops.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/core_lib/src/db/database_ops.rs#L1-L105)）；`core_lib::crud` 的非事务分支按池类型分派，是 PG/SQLite 唯一真正可用的写路径。
- 导入/导出/同步引擎均按 MySQL 实现（`sync/mysql_engine.rs` 等），但同步入口按 `runtime_capabilities` 放行 PG → 会走到 MySQL 实现并失败。
- 测试侧：`web-ui/src-tauri` 0 个测试，`e2e-runner` 0 个测试且 PostgreSQL/SQLite 只用于性能基准，不覆盖产品 HTTP 路径；因此这些“声明能力”从未被端到端验证。

### 1.6 结构性发现：重构只做了一半（🟠 维护性地基）

- `web-server/src/routes.rs:21` 定义了 `api_routes()`，但**全仓库没有调用**；`main.rs` 自己 `Router::new()` 并在 913-936 行挂载旧 handler。`core_lib::service::*`（WorkbenchService / CrudService / SchemaService / ServiceContext）只被 `service_handlers.rs` 使用，而 `service_handlers` 只被未调用的 `routes.rs` 使用 → 整个“服务层”在当前生产路径上是死代码。
- 后果：`core_lib/src/service/*` 的测试（如 `service/schema.rs` 9 个测试）覆盖的是死代码；真正生效的 `handlers/perf.rs`、`main.rs` 逻辑没有对应测试。两套实现的行为（超时、分块、错误文案）已经开始分叉。
- 桌面壳、web-server 各自维护一份 `db_client_cache`、事务表、取消表与历史写入逻辑；没有共享层，行为差异无法通过一次修改同步。

---

## 二、UI 侧待完善问题

### 2.1 死代码 / 未接线 / 占位

| 位置 | 问题 | 严重度 |
|------|------|--------|
| [routes.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-server/src/routes.rs#L21) | 新路由未接线（见 1.6） | 🟠 |
| [configStore.ts](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-ui/src/stores/configStore.ts#L1-L220) | 整个 store 无任何组件使用；`switchActiveDb/deleteConnection/updateConfig` 只在 `console.error` 里静默失败、并乐观更新本地状态 | 🟡（未接线） |
| [SchemaView.tsx](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-ui/src/components/SchemaView.tsx#L1-L40) | 组件从未被 import（结构展示实际在 `DbExplorerSidebar` + `TableWorkspace` 里实现） | 🟡 |
| [DataTable.tsx.bak](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-ui/src/components/DataTable.tsx.bak) | 211KB 备份文件留在源码目录，Vite 不编译但会干扰检索与审查 | 🟡 |
| [WizardModal](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-ui/src/components/WizardModal.tsx#L128-L136) | 未识别 type 时 `setResult('Coming soon: ' + type)` 兜底；当前 App 的入口都能命中，但仍是占位路径 | 🟡 |
| [DbSecurityManager](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-ui/src/components/DbSecurityManager.tsx#L1-L120) | 账号权限管理**完全在本地**：读 `conn.security_profile` 生成 GRANT 模板，不查询真实用户/权限，也不执行 | 🟠（功能缺失） |
| [DbEventsTriggers](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-ui/src/components/DbEventsTriggers.tsx#L136-L175) | 非 MySQL 时列表直接为空，无“不支持”提示；只读查询，不支持创建/删除 | 🟡 |
| [MockDataConfig](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-ui/src/components/MockDataConfig.tsx) | 生成的是 INSERT SQL 文本，不直接插入数据，用户需自行复制执行 | 🟡 |

全仓库 `TODO/FIXME/XXX/HACK` 为 0 处，也没有空 `onClick`；因此“半成品”不是靠注释暴露，而是靠能力缺口暴露。

### 2.2 被吞掉的错误 / 状态不一致

1. **数据表格事务开启失败继续写入**（🟠，且桌面必现）
   [DataTable](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-ui/src/components/DataTable/index.tsx#L1545-L1565)：`begin` 抛错后只设置 `transactionState='active'`，注释写着 “If it already exists or fails, we continue anyway”。随后所有 CRUD 携带这个 `transactionId`：
   - Web：服务端返回 `transaction session not found`，每行写入失败，但 UI 已显示“事务进行中”；
   - 桌面：事务在 Tauri 进程，CRUD 走 HTTP，必现。
2. **复制连接得到脱敏 URL**（🟠）
   [App](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-ui/src/App/index.tsx#L834-L845) 用 `configData`（服务端已脱敏，密码为 `***`）复制并生成新 id；服务端 `merge_secrets_from` 只按相同 id 回填密码，新 id 无法恢复 → 新连接不可用。服务端测试 `get_config_does_not_leak_secrets` 正好证明了 URL 会被脱敏（[main.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-server/src/main.rs#L1424-L1460)）。
3. **前端连接操作失败后状态回滚不完整**（🟡）
   `configStore` 的三处 `catch` 只 `console.error`；`switchActiveDb` 失败后不重新拉配置，UI 仍显示旧激活库，而服务端配置已保存新库（与 1.1 的服务端问题叠加）。该 store 当前未接线，但它是“正确的做法”的现成实现，后续接入时应先修。
4. **索引/外键抽取失败被吞掉**（🟠）
   [main.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-server/src/main.rs#L1568-L1588)：`indexes`/`foreign_keys` 用 `unwrap_or_default()`。`TableDesigner` 基于这份结构生成 ALTER；抽取失败时它会认为“表没有索引/外键”，可能生成丢约束的 DDL。
5. **流式导出非 MySQL 时返回 200 空文件**（🟠）
   [export_data](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-server/src/main.rs#L2361-L2380)：`if let Some(pool) = mysql_pool { ... }` 否则直接结束响应；行错误 `if let Ok(row)` 被丢弃 → 导出的 CSV 静默缺行。

### 2.3 加载 / 空态 / 大数据量

- 结果面板有骨架 loading 与“未执行”空态（[QueryResultsPanel](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-ui/src/components/QueryResultsPanel.tsx#L226-L254)、[L525-L570](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-ui/src/components/QueryResultsPanel.tsx#L525-L570)），这部分是完整的，不列为问题。
- **表数据总行数永远为空**：`/table/data` 返回 `total: None, total_status: "calculating"`（[main.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-server/src/main.rs#L2124-L2140)），前端未读取 `total_status`，只显示“总数未计算，可继续翻页”。这是明确的半成品。
- **结果集渲染**：`SimpleDataTable` 只做页内 `slice`，不做虚拟滚动；单页 200 行没问题，但“含 LIMIT 子串不限制”+ 大结果集会一次性传输、一次性进 DOM（见 1.3）。
- **超长单元格**：结果表直接展示原始字符串/JSON（[SimpleDataTable](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-ui/src/components/SimpleDataTable.tsx)），无截断/展开策略，宽表 + 大文本时横向滚动与重渲染成本高。
- `App/index.tsx` 2629 行、22 个 `useState`、16 个 `useEffect`、20 个 `useCallback`；`DataTable/index.tsx` 4483 行。任何一次执行/连接切换都会触碰多个全局状态，属于后续维护风险（不是当前功能缺陷）。

### 2.4 App.tsx 中的半成品交互

| 交互 | 现状 | 证据 |
|------|------|------|
| 左侧导航只有 3 个 tab（结构/片段/历史） | “工具”都塞进 `ToolsNav` + `WizardModal`，多连接/多标签缺少统一入口 | [App](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-ui/src/App/index.tsx#L1845-L1864) |
| 结果排序 | 不是对当前结果排序，而是把原 SQL 包成 `SELECT * FROM (...) AS __sorted__ ORDER BY` 再执行：含 `;` 或多语句会失败，UNION 列重名会失败，且丢失分页位置 | [App](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-ui/src/App/index.tsx#L2600-L2620) |
| 结果“加载更多” | 重新执行 SQL + OFFSET，无快照；不传 cancel token | [api.ts](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-ui/src/api.ts#L367-L372) |
| 多语句执行 | 前端拆分后串行执行，失败即 `break`；自动模式下前面的语句已提交，没有“整体回滚/继续执行”选项 | [App](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-ui/src/App/index.tsx#L1285-L1360) |
| DDL 后刷新结构 | 前端会重新拉 `/schema`，但 `/execute` 执行 DDL 时不清结构缓存，30s 内仍是旧结构；只有 `/table/ddl` 会清缓存 | [main.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-server/src/main.rs#L795-L800) / [execute_ddl](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-server/src/main.rs#L2210-L2240) |
| 视图 | 视图与表共用同一个可编辑网格，没有只读区分；对视图编辑会走到 CRUD 并报“不可更新” | [DbExplorerSidebar](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-ui/src/components/DbExplorerSidebar.tsx#L476-L520) |
| 连接失败 | `initData` 拉到 schema 失败就弹 onboarding；已有连接的用户在库临时不可用时会被强制进入向导 | [App](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-ui/src/App/index.tsx#L487-L525) |
| 移动端 | 未处理；结果面板的下载/排序按钮依赖桌面宽度 | —— |

### 2.5 前端测试覆盖

| 文件 | 覆盖内容 |
|------|---------|
| `web-ui/src/sqlStatements.test.ts` | SQL 语句拆分 |
| `web-ui/src/sqlSafety.test.ts` | 危险 SQL 判定 |
| `web-ui/src/utils/dbCapabilities.test.ts` | 能力表渲染/取值 |

空白：

- 0 个组件测试、0 个 `App` 集成测试、0 个 `api.ts` 降级/错误处理测试；
- 连接缓存/切换/删除、事务状态机、分块续拉、排序改写、导入导出流程全部无测试；
- Tauri 壳 0 个测试；上述 1.4 的跨进程问题没有任何测试能拦住。

---

## 三、常规数据库工具功能基线盘点

判定口径：**已有**＝主路径可用且有后端实现；**半成品**＝有 UI/有部分实现但存在阻断性缺口；**缺失**＝没有可用入口或实现。

| 维度 | 判定 | 证据 / 缺口 |
|------|------|------------|
| 连接管理：增删改、复制、重命名、分组、断开 | 半成品 | 入口齐全（[DbExplorerSidebar](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-ui/src/components/DbExplorerSidebar.tsx#L1-L120)、[App](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-ui/src/App/index.tsx#L765-L860)）；**复制不可用、断开不生效**（1.1/2.2） |
| 连接测试 / SSH / SSL | 半成品 | 测试仅 MySQL；SSH/SSL 仅测试期生效（1.5） |
| 库/表/视图浏览 | 半成品 | 表/视图/列/索引/外键齐全；**不能在连接内切换 database/schema**（`db_name` 取自 URL，[client.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/core_lib/src/db/client.rs#L120-L150)）；PG/SQLite 不可用 |
| 表结构查看 | 已有 | `/table/schema`，但索引/外键失败会被吞（2.2） |
| DDL：建表 | 缺失 | 无“新建表”入口；`TableDesigner` 只接收已存在的 `tableName`（[TableWorkspace](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-ui/src/components/TableWorkspace.tsx#L60-L110)），只能手写 SQL |
| DDL：改表/删表/加列/索引 | 已有 | `TableDesigner` + `/table/preview-ddl` + `/table/ddl`（MySQL） |
| 数据浏览：筛选、排序、分页 | 已有 | `/table/data` 支持 filters/orders/offset 分页，参数化与标识符转义正确（[main.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-server/src/main.rs#L2044-L2100)）；无总行数（2.3） |
| 数据编辑 / 新增行 / 删除 | 半成品 | 网格编辑走 CRUD；事务路径在桌面/失败场景不一致（2.2）；PK 识别依赖 schema，抽取失败时风险上升 |
| 查询编辑器：补全 | 半成品 | 只有表/列补全，无关键字、别名、上下文推导（[App](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-ui/src/App/index.tsx#L430-L470)） |
| 查询编辑器：多语句 | 已有（非原子） | 前端拆分串行执行，失败即停；自动模式已提交部分不回滚 |
| 查询编辑器：执行历史 | 已有 | `/sql/history`；但存的是改写后 SQL（含 LIMIT/OFFSET），桌面历史缺 db_id/行数/类型 |
| 结果集：排序 | 半成品 | 重跑 SQL + 派生表，特定 SQL 失败（2.4） |
| 结果集：分页/加载更多 | 已有 | chunk 200/上限 1000；无快照语义 |
| 结果集：导出 | 已有 | 结果面板客户端导出 JSON/CSV/SQL；服务端导出任务支持 csv/json/sql/txt/xml，但**只导出激活库** |
| 结果集：复制 | 已有 | 单元格/行/JSON 复制 |
| 执行计划 | 半成品 | `EXPLAIN` 文本；无 `EXPLAIN ANALYZE`、JSON、树形/可视化；仅 MySQL，且无超时（1.2） |
| 事务控制 | 半成品 | 编辑器提供 auto/manual + commit/rollback；仅 MySQL；桌面独立会话且无回收；断连后会话卡死（1.3） |
| 账号/权限管理 | 缺失 | `DbSecurityManager` 仅本地模板，不查询、不执行（2.1） |
| 导入 | 半成品 | CSV/JSON/XML/SQL/TXT 向导 + 任务；**无 `db_id`**、只支持 MySQL、超时/取消不可靠；旧 `/import` 接口占位符写成 `$1`（MySQL 需要 `?`）且未被前端使用（[main.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-server/src/main.rs#L300-L380)） |
| 导出（任务型） | 半成品 | keyset 分窗口导出，格式较全；**无 `db_id`**、无超时，非激活表会导错库 |
| 库结构同步 / 数据同步 | 半成品 | 有 diff + 执行任务，但引擎仅 MySQL，PG 被能力表误放行 |
| 生成测试数据 | 半成品 | 只生成 SQL 文本，不执行；只针对激活库 |

---

## 四、看起来像问题、实际是设计如此（不凑数）

1. **结果预览上限 1000 行、每次 200 行** 是显式设计，前端会展示 `preview_cap`/`has_more`（[QueryResultsPanel](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-ui/src/components/QueryResultsPanel.tsx#L300-L334)）。
2. **表数据不返回总行数**：服务端明确返回 `total_status: "calculating"`，UI 文案也写了“总数未计算”，属于已知取舍；但它没有“异步计算完成后回填”的机制，所以按“半成品”记，不按 bug 记。
3. **非激活连接按 URL 重建 client、切换激活库时清空临时缓存**：这是为了避免“改了 URL 还在用旧连接”，方向正确；问题只在于没有 `close()`（见 1.1）。
4. **激活连接复用绕过 TTL**：合理的性能设计。
5. **数据表格按行 CRUD 且默认非事务**：这是常见取舍；真正的问题是“事务开启失败仍继续”，不是“按行写入”。
6. **前端危险 SQL 预检只覆盖 UPDATE/DELETE/ALTER/DROP/TRUNCATE**：服务端 sqlparser 会兜底拦截 INSERT/CREATE/GRANT 等，Web 路径没有问题；只有桌面 invoke 路径缺兜底（1.4）。
7. **桌面只把 SELECT 走 invoke**：意图明显是“只读操作直连、写操作交给后端”，方向合理；但它与“相对 `/backend`”组合后在打包态失效。

---

## 五、按优先级排序的补充清单

### P0 — 先修地基（不做后续功能都建立在流沙上）

1. **统一后端入口/运行时**（🔴）
   - 桌面：要么在壳内拉起 web-server（sidecar）并把 `baseURL` 指向 `http://127.0.0.1:<port>/backend`，要么把配置/结构/CRUD/工具全部做成 invoke。现状是两者都没有。
   - 或者明确宣布“桌面壳不是交付形态”，删除/隔离 `src-tauri`，避免半可用状态。
2. **补齐 PG/SQLite 的引擎分派**（🔴）
   - 把所有 `db_client.mysql_pool()` 的调用点改为按 `DbPool` 分派（`DatabaseOps` 已经写好，只需接线）。
   - `/db/test` 改为按 `DbType` 选择 `MySql/Pg/SqliteConnectOptions`。
   - 结构抽取、CRUD 事务会话、DDL、EXPLAIN、导入导出任务同步改造。
   - 在改造完成前，把能力表降级为“仅连接/基础 CRUD”，避免误导。
3. **SSH/SSL 运行期生效或明确禁用**（🔴）
   - 当前只有 `/db/test` 建隧道/设 TLS（[main.rs](file:///Users/zhumingchen/Rust_Local_SQL_Tool/web-server/src/main.rs#L490-L560)）；把隧道句柄与 SSL 选项下沉到 `DbClient`，让激活连接与临时连接复用。
4. **修正连接生命周期**（🟠）
   - `update_config`：仅在 URL 变化时重连；先连新库、成功后再替换并关闭旧池；失败时回滚配置或明确报错状态；`active_db_id=None` 时关闭旧池并清理 `db_client`。
   - `DbClient::close` 放到新连接就绪之后，避免 `db_client` 为空窗口。
5. **修事务一致性**（🟠）
   - 桌面事务表与 CRUD 走同一进程（或让 CRUD 也走 invoke）。
   - `DataTable` 不再吞 `begin` 失败。
   - `commit/rollback` 失败时也移除会话并给出明确错误。
   - 桌面补空闲回收。
   - 拦截用户手工 `BEGIN/START TRANSACTION`（或显式将其映射到事务会话）。
6. **补超时**：DDL、EXPLAIN、导入/导出任务、取消维度（🟠）；超时文案改为读取策略值。

### P1 — 常规工具“该有但没有/只做了一半”

7. **导入/导出/生成数据带上 `db_id`**（🟠，写错库风险优先）
8. **新建表 / 新建视图入口**（缺失，必须手写 SQL 才能建表）
9. **库/schema 切换**（`SHOW DATABASES` / 当前连接内切库）
10. **账号权限管理查真实数据**（至少 `SHOW GRANTS` / `mysql.user` 只读展示，再谈授权执行）
11. **行数统计**：异步 count 任务 + 回填（替换 `total_status: calculating`）
12. **结果排序改为对已取结果前端排序，或由服务端在代理结果上带 ORDER BY 且保留分页语义**
13. **视图只读模式**（`TableDataView` 按 `isView` 禁用编辑）
14. **查询历史保存原文**，把改写后的 SQL/偏移单独存字段
15. **修复复制连接**：复制时要求重新输入密码，或服务端按“允许复制脱敏 URL”的 id 映射回填密钥
16. **执行计划增强**：`EXPLAIN ANALYZE`、JSON 视图、PG 的 `EXPLAIN (FORMAT JSON)`

### P2 — 可以往后放

17. 超时/取消竞态与“已取消但已生效”的语义提示
18. 临时连接池配置与 SSH/SSL 的一致性
19. 死代码清理：`routes.rs` 与 `core_lib::service`（接线或删除）、`configStore`、`SchemaView`、`DataTable.tsx.bak`、旧 `/import` `/export` `/db/test` 重复实现
20. `App.tsx` / `DataTable` 巨型组件拆分，抽出连接状态机
21. 前端与 Tauri 测试补齐（尤其 `api.ts` 降级、事务状态机、缓存生命周期）
22. 大结果集虚拟滚动、单元格截断/展开
23. 新手引导与“连接失败”提示优化（不要把库临时不可用当成首次配置）

---

## 六、文档承诺 vs 代码实际（供决策）

| 文档说法 | 代码实际 |
|----------|---------|
| `CLAUDE.md`：支持 MySQL / PostgreSQL / SQLite | 只有 MySQL 全功能；PG/SQLite 可连池 + 非事务 CRUD，其余全断 |
| `CLAUDE.md`：桌面模式直接调用 core_lib，Web 模式走 Axum | 桌面只覆盖 6 个命令，其余回到 HTTP，且打包态 HTTP 不可达 |
| 能力表：PG/SQLite Level A，支持结构同步/数据同步/导入导出 | `sync`、schema、DDL 均为 MySQL 实现 |
| 能力表（runtime）：SQL Server/Oracle 支持结构内省/导入导出 | `DbClient::new` 直接 `Unsupported`，连不上 |

建议在修 P0-2 之前，先把能力表与 UI 提示改成“实验性/仅连接”，否则用户会按 A 级预期使用而必然失败。

---

## 七、附：关键代码位置索引

- 生产路由：`web-server/src/main.rs:913-936`（旧 handler）；`web-server/src/routes.rs:21`（死代码）
- 激活连接与配置：`web-server/src/main.rs:1384-1409`、`1647-1665`
- 临时连接缓存：`web-server/src/handlers/go_live.rs:2296-2325`
- 执行主路径：`web-server/src/handlers/perf.rs:1016-1365`
- 事务会话与回收：`web-server/src/main.rs:780-840`、`1062-1098`、`1895-1936`
- 数据库测试：`web-server/src/main.rs:563-615`
- 结构抽取：`core_lib/src/schema.rs`（MySQL-only）；`core_lib/src/db/database_ops.rs`（PG/SQLite 实现，未接线）
- 连接池：`core_lib/src/db/client.rs:221-330`
- 桌面命令与降级：`web-ui/src-tauri/src/lib.rs:360-410`、`1017-1140`、`1470-1482`
- 前端请求层：`web-ui/src/api.ts:40-130`、`360-395`
- 数据表格：`web-ui/src/components/DataTable/index.tsx:1545-1565`
- 连接侧栏：`web-ui/src/components/DbExplorerSidebar.tsx`
- 连接操作：`web-ui/src/App/index.tsx:765-870`
