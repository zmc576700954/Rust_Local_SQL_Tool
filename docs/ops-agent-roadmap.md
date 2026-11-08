# 数据库运维与安全 Agent 实施方案

> 版本：v1（只读调研版，未改任何业务代码）
> 范围：`core_lib`、`web-server`、`web-ui`、`web-ui/src-tauri`
> 目标：把产品重心从“通用数据库操作”转向“数据库运维与安全”，以 Agent 编排打通“发现问题 → 出方案 → 用户确认 → 一键落地 → 验证/回滚”的闭环。

---

## 0. 结论先行

1. **现有 AI 层不能直接承载多运维场景**。`core_lib/src/ai/` 没有独立的 gateway 和 planner：模型调用、工具循环、prompt 拼装、结果解析都围绕“生成/解释/优化/修复 SQL”写死；`AgentResult` 的输出结构固定为 SQL 意图（`sql`/`explanation`/`task_type` 等），不是通用运维计划。多轮工具调用、SSE、Policy、Schema、规则与知识库可以复用，但输出模型、场景注册、审批与执行必须新增一层 `ops` 抽象。
2. **现有 DDL 执行链路不适合承接“一键落地”**。`POST /backend/table/ddl` 直接执行客户端提交的单条 DDL，允许 `DROP/TRUNCATE`，没有 dry-run、影响评估、备份、锁/超时策略和审计；`DdlEngine` 还会自动生成 `DROP COLUMN`，并把 `MODIFY COLUMN` 误判为 `ALGORITHM=INPLACE, LOCK=NONE`（实际多数类型变更需要 `COPY`），复合索引也会生成错误 DDL。**运维执行必须另建 `ops` 执行器，不能复用这条路由。**
3. **建议的第一刀是“MySQL 单表索引优化闭环（仅新增索引）”**。它同时具备：schema 提取、EXPLAIN、DDL 预览可复用；动作可逆（`DROP INDEX` 回滚）、不迁移数据、风险最低；能一次性验证 Agent 编排底座的“证据 → 计划 → 审批 → 执行 → 验证 → 回滚”全链路。执行入口不复用旧 `/table/ddl`，由新的 `ops` 执行器承担。批量复制 Markdown 作为零风险独立小项同阶段交付。
4. **桌面端不建议再补一套业务命令**。Tauri 当前只有 6 个命令（查询/分页/取消/事务/单表 schema/性能探针），且只支持 MySQL；Web 端其余能力全部走 `/backend` HTTP。生产桌面没有 sidecar，standalone 时 HTTP 路径不可用。建议把 `web-server` 作为 Tauri sidecar 打包，让两端复用同一套 Agent/审批/审计逻辑；不要在 Tauri 里复制执行器。
5. **安全边界必须前置**：Agent 只产出结构化 `ActionSpec`（表/列/索引/批次），不产出可直接执行的自由 SQL；SQL 由服务端编译器生成并校验；执行需要一次性审批令牌；生产环境默认只允许只读与 dry-run。

---

## 1. 调研方法与事实边界

- 仅阅读代码与文档，未修改业务代码、未动 git、未启动应用。
- 重点阅读：
  - `core_lib/src/ai/*`、`core_lib/src/rule_engine.rs`、`knowledge_base.rs`、`perf_report.rs`、`timeout_policy.rs`
  - `core_lib/src/schema/*`、`sql/offline_parser.rs`、`sync/*`、`tools.rs`、`db/*`、`service/workbench.rs`、`config.rs`
  - `web-server/src/main.rs`、`state.rs`、`routes.rs`、`ai_handlers.rs`、`service_handlers.rs`、`handlers/*`
  - `web-ui/src/App/index.tsx`、`components/*`、`api.ts`、`types.ts`、`src-tauri/src/lib.rs`
- 已有设计文档（作为背景，不重复其内容）：
  - `docs/superpowers/specs/2026-06-03-agent-integration-design.md`
  - `docs/superpowers/specs/2026-06-03-database-operations-optimization-design.md`

---

## 2. 现状盘点

### 2.1 运行时与路由

| 事实 | 结论 |
|---|---|
| `web-server/src/main.rs` 自己构建 `Router::new()`，并在 `main.rs:1041` `.nest("/backend", api)` | 新路由实际生效位置是 `main.rs` 的 `api` 链 |
| `web-server/src/routes.rs` 有 `api_routes()` 但文件头标注 `#![allow(dead_code)]`，`main.rs` 没有引用 | `routes.rs` 是未生效的重复注册，形成路由双维护的技术债；本轮不建议顺手重构 |
| 现有 DDL 入口：`/backend/table/ddl/preview`、`/backend/table/ddl`（`main.rs:2217-2281`） | 预览只调 `DdlEngine::generate_preview`；执行直接 `sqlx::query(&req.sql).execute(db_client.mysql_pool()?)` |
| `/backend/sql/explain` 仅对当前连接做 `EXPLAIN` 前缀（`main.rs:2809` 附近） | 可复用为只读证据，但只返回原始行，无计划结构 |
| `/backend/execute`（`service_handlers.rs:86`）经 `WorkbenchService::execute_sql` | 有只读连接守卫和 `force` 危险 SQL 守卫，但 `force` 由客户端提交，不是服务端审批 |
| `web-server` 绑定本机，CORS 白名单含 `tauri://localhost`，没有认证中间件 | 本机工具定位；但运维写接口仍需额外的本地令牌/Origin 校验，防 CSRF/本机进程滥用 |
| Tauri `generate_handler!` 只注册 6 个命令（`web-ui/src-tauri/src/lib.rs:1634-1641`） | 桌面 standalone 不覆盖 AI/DDL/同步等 HTTP 能力 |
| `tauri.conf.json` 无 `externalBin`/sidecar | 生产桌面没有内嵌 web-server；`/backend` 在 standalone 下不可用 |

### 2.2 AI 编排层（`core_lib/src/ai/`）

| 模块/问题 | 现状 | 能否承载运维场景 |
|---|---|---|
| gateway | **没有独立 `gateway.rs`**。事实上的 gateway 是 `agent.rs::build_openai_agent` / `build_anthropic_agent` + `provider_utils.rs` + `config.resolve_ai_profile`；负责构造 OpenAI 兼容或 Anthropic client | 可复用模型调用，但无统一成本/限流/重试策略抽象 |
| planner | **没有 planner 模块**。`agent.rs` 承担 planner+executor：`TaskType { Generate, Explain, Optimize, Fix, General }`（`agent.rs:75-86`），`from_mode` 只识别 4 种 SQL 模式 | 不能直接承载“发现→方案→确认→落地”：没有显式计划对象、步骤依赖、审批节点 |
| 多步/工具调用 | rig 多轮 Agent，`max_turns` 按任务类型 2-6（`agent.rs:124-133`）；`build_tools` 注册 `QuerySchemaTool`、`ExecuteSqlTool`（只读）、`QueryRulesTool`、`QueryKnowledgeTool`（`agent.rs:145-167`） | 工具循环可复用；但工具集写死、没有写操作工具，也没有按场景注册 |
| prompt | `build_preamble`（`agent.rs:169-229`）固定 workflow/输出 JSON/SQL 格式，注入 schema briefing、extra guidance、task guidance | 可复用注入机制；但输出契约是 SQL，不是运维计划 |
| 结构化输出 | `StructuredSqlIntent`（`extractor.rs:3-26`）三层容错：JSON → Markdown JSON → 原始 SQL | 容错思路可复用；需要新增 `OpsPlan` 提取器与严格校验 |
| 审批 | `AgentResult` 有 `risk_level`、`needs_confirmation`（`events.rs:79-99`），但只是回传给前端的提示字段 | 没有服务端审批令牌、计划哈希、一次性消费 |
| 事件流 | `AgentEvent`：thinking/tool_call/tool_result/sql_draft/final_sql/explanation/error/token_usage/done（`events.rs:5-40`） | 可复用 SSE 机制；需新增 ops_evidence/ops_plan/ops_action_* 事件 |
| 策略 | `Policy` 是单一全局对象，10 个字段（`policy_store.rs:19-47`），`DangerousSqlPolicy::AllowWithForce` | 只能管 AI 参数；没有场景级/连接级策略、环境策略、审批策略 |
| prompting.rs | 主要用于 schema briefing 的表名选择；SQL prompt builder 已非主路径 | 不承载场景 prompt |

**planner 的输入/输出（事实）**：

| 方向 | 结构 | 字段 |
|---|---|---|
| 输入 | `AiChatRequest`（`core_lib/src/service/ai.rs:34-40`） | `query`、`mode`（generate/optimize/explain/fix）、`current_sql`、`chat_history`、`extra_guidance` |
| 输入补充 | 服务端拼装 | schema briefing（按 `schema_briefing.rs` 选表并摘要）、规则命中、知识库命中、数据库方言、`Policy.max_turns` |
| 输出 | `AgentResult`（`ai/events.rs:79-99`） | `sql`、`explanation`、`task_type`、`sql_empty_reason`、`missing_information`、`grounding_evidence`、`assumptions`、`referenced_tables`、`risk_level`、`needs_confirmation`、`matched_rule_id`、`events` |

结论：`query + schema + rules + knowledge + current_sql` 是现有 planner 的全部输入，输出永远是 SQL 意图；它没有“动作/计划/审批/执行”字段。多步执行仅指 rig 内部的只读工具循环，不是可审批的运维步骤。

**结论**：复用 Agent 的“模型 + 工具循环 + SSE + 提取器容错”，但必须新增：

- `OpsScenario` 场景注册与场景级 prompt/工具/策略；
- `OpsPlan` 结构化计划模型；
- 服务端计划校验、DDL 编译、审批、执行、回滚、审计；
- 只读证据工具（EXPLAIN/统计/采样/索引元数据/巡检）。

### 2.3 可复用后端模块矩阵

| 模块 | 解决什么问题 | 运维方向复用点 | 直接复用 / 需改造 / 不能复用 |
|---|---|---|---|
| `schema/extractor.rs` + `schema/ext.rs` | MySQL `information_schema` 提取表/列/索引/外键 | 索引优化、字段治理、巡检的证据来源 | **需改造**：仅 MySQL；`IndexInfo` 只有 `index_name/column_name/non_unique/index_type`，缺 `seq_in_index`、`cardinality`、可空性，复合索引无法表达 |
| `sql/offline_parser.rs` | 解析离线 `CREATE TABLE` 文本成 `SchemaResponse`（MySQLDialect） | 期望结构/漂移巡检、离线方案校验 | 可复用（只读） |
| `tools.rs::DdlEngine` | 由新旧 `TableWithDetails` 生成 ALTER/CREATE + 回滚文本 | 可参考生成逻辑 | **不能直接复用执行**：自动生成 `DROP COLUMN`；复合索引按单列重复生成；`MODIFY COLUMN` 误判 INPLACE；算法只是注释，不是真实 `ALGORITHM` 子句 |
| `tools.rs::SyncEngine` | schema diff（表/列/索引/外键） | 索引 diff、字段迁移前后结构对比 | 需改造（`DROP TABLE` 等破坏性输出不能进执行器） |
| `sync/engine.rs` | 数据 diff、DDL 选择、diff 行 | 迁移前后数据对比 | 可复用（只读） |
| `sync/mysql_engine.rs` | 按主键区间 compare/checksum、preview、deploy、流式读取、自适应 chunk | 字段拆分回填、迁移校验、巡检对账 | **强复用**：`PkRange`/`ChunkChecksum`/`AdaptiveChunker`/`row_checksum_expr` |
| `sync/transfer.rs` | CSV/本地文件 → 目标表，Append/Replace、列映射、执行报告 | 批量导入/迁移辅助 | 可复用；`Replace`（truncate）必须禁止自动执行 |
| `rule_engine.rs` | 自然语言 → SQL 模板/模块/建议规则 | 运维规则库（如索引前缀规则、巡检 SQL 模板） | 需扩展 `RuleType`/作用域；现有结构可承载 |
| `knowledge_base.rs` | DDL/Documentation/SQL 三类知识，按连接隔离，`is_golden` | 运维 runbook、字段语义、已知故障模式 | 可复用；建议新增 `Runbook` 语义或用 `Documentation` 承载 |
| `perf_report.rs` + `handlers/perf.rs` | 性能用例指标（duration/rows/bytes/throughput）、baseline、suite diff | 执行索引/参数后的效果验证 | 可复用；但只测延迟，不解析 EXPLAIN 计划内容 |
| `timeout_policy.rs` | 连接/查询/AI/HTTP 超时，环境变量可配 | 新增 DDL 锁等待、语句超时、执行预算 | 需扩展（当前无 DDL 专用超时、无 lock_wait_timeout） |
| `db/database_ops.rs` | 统一 `DatabaseOps` trait（query/command/kill/quote） | 只读证据采集、跨方言适配 | 可复用；但 `WorkbenchService` 实际仍走 `mysql_pool()` |
| `db/capability.rs` | 连接能力等级声明 | 场景可用性矩阵 | 可用；需与真实能力校验，避免“声明支持 PG 但 schema/索引提取缺失” |
| `service/workbench.rs` | SQL 分类、只读守卫、危险 SQL `force`、事务会话、取消 | 参考其守卫与取消机制 | 不能作为审批/回滚依据；`force` 无服务端凭据 |
| `handlers/go_live.rs` | 上线门禁检查、报告、JSONL 审计 | 审计日志格式、巡检报告结构 | **强复用**：append-only JSONL + 报告模式 |

### 2.3.1 按目标能力映射

| 目标能力 | 可直接复用 | 需改造 | 必须新建 |
|---|---|---|---|
| 索引优化 | `SchemaExtractor`、`EXPLAIN` 路由、`TableDesigner` 的 AI 建议、`DdlEngine` 的 CREATE/ALTER 思路、perf baseline | `IndexInfo` 复合索引/基数、`DdlEngine` 索引生成 | 索引元数据采集、索引规则、`ActionSpec` 编译、审批执行、复测 |
| 性能优化 | `perf_report`、`PerfDiagnosticsPanel`、EXPLAIN、schema 统计 | EXPLAIN 结构化解析、只读统计工具 | 性能诊断场景、锁/事务采集、DB 侧动作与代码建议的分流 |
| 字段迁移 | `sync/mysql_engine` 的 PK 区间 checksum/chunker、`transfer` 列映射、`SchemaExtractor`、offline parser | 校验规则、迁移状态机 | 字段语义模型、风险预判、分批回填、兼容期状态机、列快照备份 |
| 定期巡检 | `rule_engine`（SQL 模板）、`knowledge_base`（runbook）、sync 对账、go-live JSONL 审计、`ToolJob`/`job_semaphore` | 规则类型/作用域 | 巡检预设、调度器、历史基线、告警去重、告警通道 |
| Agent 编排 | rig 多轮工具、SSE、`Policy`、`extractor` 容错、只读 SQL 工具 | 场景注册、输出模型 | `ops` 计划/审批/执行/审计、场景 prompt、结构化校验 |
| 批量复制 Markdown | 两个表格的现有复制逻辑、结果面板、分页 | 多选、列元数据 | Markdown 生成/转义、复制预览 |

### 2.4 前端与入口

| 位置 | 现状 | 运维落点 |
|---|---|---|
| `web-ui/src/App/index.tsx` | 标签页驱动；`ToolsNav` 回调按 `type` 新建 tab（约 `App/index.tsx:1970-2019`）；已有 `perf-diagnostics`、`advanced-center`、`ai-training`、`go-live-*` 等 | 新增 `ops-center` / `inspection` tab；在 `TableContextMenu` 回调中直达场景 |
| `components/ToolsNav.tsx` | 工具菜单已较拥挤（schema-sync、data-sync、perf-sync、go-live、db-security、db-events、model-compare、visual-sync、data-transfer、data-analytics、advanced-center、go-live-reports、go-live-audit、query-builder、ai-training、perf-diagnostics） | 建议把“运维中心”放在显眼位置/分组首位，而不是继续平铺 |
| `components/TableContextMenu.tsx` | 仅 3 个表级动作（模拟数据/导入/导出） | 新增“索引建议 / 巡检该表 / 字段治理 / 查看 DDL” |
| `components/DataTable/index.tsx` | 可编辑表格；单行右键复制（单元格/行 TSV/行 JSON/行 SQL），无多选、无 Markdown | 批量 Markdown 复制的第二个落点 |
| `components/SimpleDataTable.tsx` | 查询结果只读表格；单行右键复制同上，无多选 | Markdown 复制主落点；需加分页级/选中行批量复制 |
| `components/QueryResultsPanel.tsx` | 结果面板，分页、复制 JSON、导出 | 工具栏加“复制为 Markdown”；向表格传列元数据 |
| `components/TableDesigner.tsx` | 已有“AI 建议索引”（仅表结构 prompt，无 EXPLAIN/数据分布证据），用户手工添加，`previewDdl` 后直接 `executeDdl` | 索引优化 MVP 的直接原型；必须补证据、审批、备份、锁策略 |
| `components/PerfDiagnosticsPanel.tsx` | 性能探针：输入 SQL、baseline、suite/diff、记录耗时与行数 | 复用为执行前后验证；不解析计划内容 |
| `components/DbSecurityManager.tsx` | 安全/权限相关，当前主要是 GRANT SQL 预览，不直接执行 | 可扩展为“安全体检”只读场景 |
| `web-ui/src/api.ts` | Tauri 运行时判断、`invokeDesktopOrHttp`、axios base `/backend`、只读 SQL 桌面快速路径 | 新增 ops API 与 SSE 封装；桌面失败时回退 HTTP |
| `web-ui/src-tauri/src/lib.rs` | `DesktopState` 只有连接/缓存/事务；`resolve_connection` 只支持 MySQL/MariaDB；`perf_probe` 只允许只读 | 桌面复制执行器风险高；建议 sidecar 复用服务端 |

### 2.5 现有安全控制（事实）

- 只读连接：`DbConnection.is_read_only`（`config.rs:268-291`）在 `execute_sql`/`execute_ddl` 前置检查。
- 危险 SQL：`is_dangerous_statement`（`sql/util.rs:411`）覆盖 INSERT/UPDATE/DELETE/DROP/TRUNCATE/ALTER/CREATE/GRANT/REVOKE/SET 等；`WorkbenchService` 要求客户端传 `force: true`（`workbench.rs:125-133`）。
- DDL：`execute_ddl` 仅校验“单条 + 是 DDL”，随后直接执行；没有 `DROP`/`TRUNCATE` 二次限制，没有审计、没有备份、没有 `ALGORITHM/LOCK`、没有锁等待超时（`main.rs:2224-2281`）。
- 事务：`transaction_sessions` 支持手动事务；DDL 走 pool 自动提交，且 MySQL DDL 隐式提交，事务不覆盖 DDL。
- 超时：`TimeoutPolicy` 有连接/查询/AI 超时，但没有 DDL 锁等待、语句级执行预算。
- 审计：仅 go-live 有 JSONL 审计；普通 DDL/DML 无审计。
- 服务安全：无认证中间件；本机绑定 + CORS 白名单。

---

## 3. 能力依赖与分期

### 3.1 依赖关系

```text
[1] Agent 编排底座（场景注册/证据/计划/审批/执行/审计/SSE）
   ├──> [3] 索引优化（MySQL 单表，先只增不删）
   │       └──> [5] 性能优化（只读诊断 + 复用索引动作 + ANALYZE）
   ├──> [7] 定期巡检（只读检查 + 调度 + 告警；AI 只做摘要）
   └──> [4] 字段优化/迁移（还需数据校验 + 分批回填 + 兼容状态机）
[2] 便捷操作优化（部分独立，可穿插）
[6] 批量复制 Markdown（完全独立，前端即可）
```

### 3.2 分期表

| 阶段 | 内容 | 依赖 | 退出标准 |
|---|---|---|---|
| **阶段 0：地基 + 首个闭环** | 1 的底座最小版；3 的索引 MVP；6 的 Markdown 复制；安全基线（环境标签、写权限、审批、审计、禁止清单） | 无 | 在 dev MySQL 上完成一次真实“EXPLAIN 证据 → 索引方案 → 审批 → CREATE INDEX → 复测 → DROP INDEX 回滚”；无生产写入 |
| **阶段 1：只读运维规模化** | 5 性能诊断与建议（DB 侧只开放 L1 动作）；7 巡检预设/调度/告警；2 便捷操作 | 阶段 0 | 只读场景全部可用；巡检连续运行无重复告警；性能建议可复测；代码侧建议只输出不改代码 |
| **阶段 2：数据治理** | 4 字段语义/冗余分析、风险预判、双表迁移 dry-run、分批回填、校验、兼容状态机、人工删列 | 阶段 1；`sync/mysql_engine` 校验 | 影子库完成一次 JSON 拆表迁移并校验一致；生产只允许 dry-run 与人工执行高危动作 |
| **阶段 3：多方言与桌面/团队化** | PG/SQLite 索引元数据与 DDL 编译；Tauri sidecar 或等价方案；更多安全/备份/容量场景；可选本地令牌/多用户 | 阶段 2 | Web 与桌面走同一路径；PG 至少支持只读诊断；所有写操作有审计 |

### 3.3 为什么第一刀选“索引优化闭环”

- **能最快验证战略价值**：它是“发现问题 → 出方案 → 确认 → 一键落地”最短的完整链路，不是单点工具。
- **风险最低**：只新增索引，不改数据、不删列、不重建表；回滚是 `DROP INDEX`，可自动生成并可验证。
- **复用度最高**：schema 提取、EXPLAIN、`TableDesigner` 的 AI 建议、`preview_ddl`/`execute_ddl`、perf baseline 都已存在。
- **能暴露全部平台问题**：证据采集、结构化方案、审批、执行、验证、审计、回滚，一个都不少。
- **对比 Markdown 复制**：它更快、零风险，但不验证 Agent 闭环；因此作为同一阶段的独立小项并行交付。

---

## 4. Agent 编排底座最小可用设计

### 4.1 目录与职责

新增 `core_lib/src/ops/`：

```text
core_lib/src/ops/
  mod.rs              # 对外导出、初始化
  types.rs            # OpsPlan / ActionSpec / EvidencePack / Risk / Validation
  scenario.rs         # OpsScenario trait + ScenarioRegistry
  context.rs          # OpsContext（连接、策略、超时、审计）
  prompt_store.rs     # 内置 prompt + 用户覆盖 + 版本哈希
  validate.rs         # JSON Schema + 领域校验
  ddl_compiler.rs     # ActionSpec -> 真实 SQL（唯一 SQL 生成入口）
  approval.rs         # 审批令牌、计划哈希、一次性消费
  executor.rs         # dry-run / apply、状态机、失败补偿
  audit.rs            # append-only JSONL
  backup.rs           # SHOW CREATE TABLE、影子表、列快照
  index_advisor.rs    # 索引场景
  index_meta.rs       # 复合索引元数据采集
  perf_advisor.rs     # 性能场景（阶段 1）
  field_governance.rs # 字段场景（阶段 2）
  migration.rs        # 分批回填 / checkpoint（阶段 2）
  inspection/         # 巡检（阶段 1）
    mod.rs
    presets.rs
    scheduler.rs
    store.rs
    alert.rs
```

新增 `core_lib/src/ai/extractor_ops.rs`、`core_lib/src/ai/agent_ops.rs`（或把 `agent.rs` 的通用部分抽出，但不要破坏现有 SQL Agent）。

### 4.2 场景如何注册

```rust
#[async_trait::async_trait]
pub trait OpsScenario: Send + Sync {
    fn id(&self) -> &'static str;
    fn title(&self) -> &'static str;
    fn input_schema(&self) -> serde_json::Value;
    fn risk_class(&self) -> RiskClass;

    /// 只读、确定性的证据采集；不调用 LLM。
    async fn collect_evidence(
        &self,
        ctx: &OpsContext,
        input: &serde_json::Value,
    ) -> Result<EvidencePack, OpsError>;

    /// 场景 prompt：场景目标 + 证据 + 输出 schema + 禁止事项。
    fn build_preamble(&self, evidence: &EvidencePack) -> String;

    /// 严格解析 LLM 输出；不允许自由 SQL 直接进入执行。
    async fn parse_plan(
        &self,
        raw: &str,
        evidence: &EvidencePack,
    ) -> Result<OpsPlan, OpsError>;

    /// 服务端编译 + 领域校验。
    fn compile_actions(
        &self,
        plan: &OpsPlan,
        snapshot: &SchemaSnapshot,
    ) -> Result<Vec<CompiledAction>, OpsError>;
}

pub struct ScenarioRegistry {
    scenarios: HashMap<&'static str, Arc<dyn OpsScenario>>,
}
```

- 注册点：`web-server/src/main.rs` 启动时构建 registry，放入 `AppState.ops_registry`。
- 前端通过 `GET /backend/ops/scenarios` 获取场景列表、输入 schema、风险等级、是否可写。
- 场景 ID 首批：`index_advisor`、`sql_perf_advisor`；阶段 1 加 `inspection`，阶段 2 加 `field_governance`。

### 4.3 输入 / 证据 / 计划模型

```rust
pub struct OpsPlan {
    pub plan_id: String,
    pub scenario_id: String,
    pub db_id: String,
    pub db_fingerprint: String,      // host/db/server_version 哈希
    pub evidence_hash: String,
    pub diagnosis: String,
    pub findings: Vec<Finding>,
    pub actions: Vec<ActionSpec>,
    pub risks: Vec<RiskFinding>,
    pub validation_checks: Vec<ValidationCheck>,
    pub rollback: RollbackPlan,
    pub code_advice: Vec<CodeAdvice>,   // 只展示，不执行
    pub confidence: f32,
    pub requires_approval: bool,
}

pub enum ActionSpec {
    CreateIndex { table: String, name: String, columns: Vec<String>, unique: bool, using: Option<String> },
    DropIndex { table: String, name: String, expected_definition: String },
    AnalyzeTable { table: String },
    CreateTable { table: String, columns: Vec<ColumnSpec>, indexes: Vec<IndexSpec> },
    AddColumn { table: String, column: ColumnSpec },
    Backfill { source: String, target: String, mapping: Vec<ColumnMapping>, batch_size: usize, mode: BackfillMode },
    // 刻意不提供 RawSql 变体
}
```

`EvidencePack` 至少包含：

```rust
pub struct EvidencePack {
    pub schema: SchemaSnapshot,          // 列、分组后的复合索引、外键、视图、触发器
    pub table_stats: TableStats,         // TABLE_ROWS / DATA_LENGTH / INDEX_LENGTH / DATA_FREE / AUTO_INCREMENT
    pub explain: Vec<ExplainPlan>,       // 结构化 EXPLAIN（MySQL 8 FORMAT=JSON，失败回退表格）
    pub samples: Vec<SampleSet>,         // 小样本、敏感列脱敏
    pub workload: Vec<SqlSample>,        // 当前 SQL / SQL history / perf suite
    pub index_usage: Option<IndexUsage>, // performance_schema，权限不足时 None
    pub rules: Vec<RuleRef>,
    pub knowledge: Vec<KnowledgeRef>,
}
```

**关键约束**：`EvidencePack` 由服务端采集，LLM 只能引用；prompt 中保留证据引用 ID，`OpsPlan.findings` 必须携带 `evidence_ref`，否则标为 `unverified`。

### 4.4 prompt 与策略管理

- 内置 prompt：`core_lib/src/ops/prompts/{scenario_id}.md`，用 `include_str!` 编译进二进制。
- 用户覆盖：`~/.local-ai-sql/ops/prompts/{scenario_id}.md`；加载时计算 sha256，写入审计。
- 场景 prompt 固定四段：
  1. `<role>`：数据库运维专家，只做分析与方案；
  2. `<evidence>`：结构化证据 + 引用 ID；
  3. `<output_schema>`：`OpsPlan` JSON schema；
  4. `<hard_rules>`：禁止自由 SQL、禁止臆造对象、无法确定必须输出 `missing_information`。
- 全局 `Policy` 继续管 temperature/max_tokens/max_turns；新增 `OpsPolicy`（本地 JSON）管：
  - 连接环境（dev/staging/prod）、是否允许写；
  - 审批 TTL、是否要求输入表名二次确认；
  - 最大影响行数、最大 DDL 表体积、是否只允许 online DDL；
  - 禁止动作类型、维护窗口、限速、全局暂停开关。
- 存储：`~/.local-ai-sql/ops/policy.json`；与 `PolicyStore` 的快照/回滚风格保持一致。

### 4.5 输出如何被结构化校验

1. LLM 返回 JSON → `extract_ops_plan`：直接 JSON → Markdown JSON → 失败即报错，**不做 SQL 回退**。
2. JSON Schema 校验（`jsonschema` crate）+ `serde` 反序列化。
3. 领域校验（`validate.rs`）：
   - 表/列/索引必须存在于证据快照；
   - 索引名不冲突、列顺序合法、不允许 JSON/TEXT 无前缀索引；
   - 不允许系统库（`mysql`/`information_schema`/`performance_schema`/`sys`）；
   - 不允许跨库操作；
   - 唯一索引必须有重复检查证据；
   - `ActionSpec` 白名单校验。
4. 编译校验：`ddl_compiler.rs` 只接受 `ActionSpec`，生成 SQL 时统一 quote/转义；编译后再用 `sqlparser` 解析，确认单语句、语句类型与 Action 匹配。
5. 计划哈希：`plan_hash = sha256(db_fingerprint + evidence_hash + 规范化 actions + rollback)`；审批与执行都绑定该哈希。
6. 失败修复：允许一次“修复 prompt”重试；仍失败则返回 `plan_invalid`，不生成审批按钮。

### 4.6 执行前如何审批

- `POST /backend/ops/plans/:plan_id/approve`：
  - 请求：`{ action_ids: [...], confirmation: "表名或确认短语", expected_plan_hash }`；
  - 服务端校验计划哈希、证据哈希、连接指纹、动作白名单、环境策略；
  - 生成 `ApprovalRecord { approval_id, plan_id, plan_hash, action_ids, db_fingerprint, expires_at, one_time: true }`，持久化到 `~/.local-ai-sql/ops/approvals.json`；
  - 返回审批令牌；执行接口必须携带该令牌。
- 高危动作（L3/L4）：
  - 必须在 UI 勾选每个动作 + 输入表名；
  - 生产环境默认 `prod_write_enabled=false`，只允许 dry-run；
  - 可选双人审批：本地单用户场景先不做，但接口预留 `approver` 字段。
- 审批不可复用：执行开始即标记 `consumed`；失败后重新审批。

### 4.7 失败如何回滚

- 每个 `CompiledAction` 带 `Compensation`：
  - `CreateIndex` → `DropIndex`；
  - `DropIndex` → 用原 `SHOW CREATE TABLE` 中的定义重建；
  - `AddColumn` → `DropColumn`（仅当尚未写入数据；否则转人工）；
  - `Backfill` → 删除目标表对应范围或按 checkpoint 重跑；
  - `CreateTable` → `DropTable`（仅当未进入兼容期）。
- 执行状态机：`pending → prechecking → approved → running → succeeded | failed | unknown`。
- 失败策略：
  - L1 动作失败：停止后续动作，自动补偿已完成的可逆动作；
  - L2+ 动作失败：停止并保留现场，不自动补偿，提示人工；
  - `unknown`（超时/连接中断）：禁止继续，重新采集结构后判定，必要时人工介入。
- 所有补偿动作本身也要写入审计，并生成新的 `compensation_plan_id`。

### 4.8 事件流

在 `AgentEvent` 之外新增 `OpsEvent`（或复用同一 SSE 通道）：

```text
ops_evidence     证据采集完成（schema/stats/explain 摘要）
ops_plan         计划生成（诊断、动作、风险、回滚）
ops_validation   校验结果（通过/阻断项）
ops_approval     审批通过（动作 ID、过期时间）
ops_action_started / ops_action_result / ops_action_skipped
ops_verify       执行后复测（EXPLAIN 对比、行数/checksum）
ops_done         整体结果 + 回滚入口
```

前端按事件流渲染时间线，而不是等最终 JSON。

---

## 5. 数据库变更安全设计

### 5.1 风险分级与执行策略

| 等级 | 示例 | 默认执行 |
|---|---|---|
| L0 只读 | SELECT/SHOW/EXPLAIN/统计/采样 | 自动 |
| L1 可逆轻量 DDL | `CREATE INDEX`、`DROP INDEX`（有原定义）、`ANALYZE TABLE` | 审批后可一键执行 |
| L2 结构变更 | `CREATE TABLE`、`ADD COLUMN`（可空/有默认） | 审批后执行；生产需维护窗口 |
| L3 数据变更 | 分批回填、受限 `UPDATE`（有 PK 条件、行数上限） | 先 dry-run，再审批；生产默认仅 dry-run |
| L4 不可逆/高危 | `DROP COLUMN`、`DROP TABLE`、`TRUNCATE`、`MODIFY` 类型、分区、引擎/字符集、`GRANT/REVOKE`、`SET GLOBAL` | **禁止自动执行**，只生成方案与脚本，人工执行 |

### 5.2 dry-run 预演

每次 `analyze` / `execute?dry_run=true` 必须完成：

1. **连接与环境校验**：连接存在、非系统库、非只读冲突、环境标签、写权限开关。
2. **结构快照**：`SHOW CREATE TABLE`、列/索引/外键/视图/触发器依赖、表引擎、行格式。
3. **统计**：`TABLE_ROWS`、`DATA_LENGTH`、`INDEX_LENGTH`、`DATA_FREE`、`AUTO_INCREMENT`（明确标注为估算值）。
4. **计划/影响**：
   - DML：`EXPLAIN` + 有界 `SELECT COUNT(*)`（超时/上限），得到预估影响行数；
   - DDL：按操作类型给出锁级别、是否重建表、预计索引大小、磁盘余量（无法可靠获取时拒绝大表在线 DDL）。
5. **权限**：`SHOW GRANTS` 校验是否具备 `INDEX/ALTER/CREATE`，不要求 `SUPER`。
6. **备份可行性**：结构备份可做；数据备份空间是否足够；影子表名是否冲突。
7. **风险输出**：阻断项（P0）、警告项（P1）、信息项（P2），每项带证据 SQL 与样本。

### 5.3 影响行数与锁级别评估

- **DML**：
  - 先用同一 `WHERE` 做 `SELECT COUNT(*)`，加超时与行数上限；
  - `UPDATE/DELETE` 必须命中主键或唯一键范围；否则拒绝生成可执行动作；
  - 预估影响行数超过 `max_affected_rows` 直接阻断。
- **DDL**：
  - 需要按 MySQL/MariaDB 版本 + 操作类型 + 引擎查能力矩阵；
  - **不使用现有 `classify_alter_algorithm` 的结论**：它把 `MODIFY COLUMN` 判为 `INPLACE/LOCK=NONE`，而实际类型变更通常需要 `COPY`；`ADD INDEX` 在 InnoDB 通常可 `INPLACE, LOCK=NONE`，`DROP INDEX` 可 `INPLACE`；
  - `CREATE INDEX`/`DROP INDEX` 编译时必须输出真实子句：`ALTER TABLE ... ADD INDEX ... ALGORITHM=INPLACE, LOCK=NONE`，而不是注释；
  - 服务端返回 `ER_ALTER_NOT_SUPPORTED`/锁等待超时 → 失败关闭，不自动降级为 `COPY`。
- **锁等待**：
  - 专用连接执行，先 `SET SESSION lock_wait_timeout = 5`、`SET SESSION innodb_lock_wait_timeout = 5`；
  - DDL 用 `tokio::time::timeout` 包裹；执行前记录 `CONNECTION_ID()`，超时后 `KILL QUERY`；
  - 禁止在连接池里跑长 DDL，避免污染其他请求。

### 5.4 备份与回滚脚本

| 操作 | 备份 | 回滚 |
|---|---|---|
| `CREATE INDEX` | 记录 `SHOW CREATE TABLE` 与现有索引定义 | `DROP INDEX`（自动生成） |
| `DROP INDEX` | 完整原索引定义（复合列顺序、唯一性、`USING`） | `CREATE INDEX` 原样重建 |
| `ADD COLUMN` | 记录原表 DDL | `DROP COLUMN`（仅未写入数据时自动；否则人工） |
| `MODIFY COLUMN` | 原列定义 + 受影响行快照 | 反向 `MODIFY`（需再次备份；默认人工） |
| 回填 DML | 目标表为空/可重建；checkpoint | 清空目标范围，按 checkpoint 重跑 |
| `DROP COLUMN` | **必须**先把该列按 PK 快照到影子表 | 从影子表恢复；无备份则禁止执行 |
| `CREATE TABLE` | 记录建表语句 | `DROP TABLE`（仅在兼容期前自动） |

所有备份产物路径、哈希、行数写入 `plan.artifacts` 与审计；执行前校验备份可用。

### 5.5 事务边界

- MySQL DDL 隐式提交，**DDL 不能回滚**；因此：
  - 一次只执行一个 DDL 动作，动作之间状态落盘；
  - 绝不在一个事务里混合 DDL 与 DML；
  - 不使用 `SET autocommit=0` 来“包住 DDL”。
- DML：
  - 小规模、原子性要求高 → 单事务，失败整体回滚；
  - 大批量回填 → 按主键区间分批，每批独立事务 + checkpoint，可恢复；
  - 禁止无 `WHERE` 的 `UPDATE/DELETE`；禁止 `INSERT ... SELECT` 无行数上限。
- 执行器在动作开始前持久化 `running`，结束后写入结果；进程重启后把 `running` 标为 `unknown`，重新采集结构判定。

### 5.6 必须禁止自动执行的操作

- `DROP DATABASE` / `DROP SCHEMA` / `DROP TABLE` / `TRUNCATE TABLE`
- `DROP COLUMN`、`RENAME TABLE`、`CHANGE COLUMN`/`MODIFY COLUMN` 类型变更
- `ADD/DROP PRIMARY KEY`、分区/引擎/字符集/排序规则变更
- `GRANT` / `REVOKE`、`SET GLOBAL`、修改服务端参数
- `KILL` 其他会话、`SET FOREIGN_KEY_CHECKS=0`
- 跨库/跨 schema 操作、系统库操作
- 无 `WHERE` 或无主键/唯一键条件的 `UPDATE/DELETE`
- 超过体积/行数阈值的表重建（应改用 gh-ost/pt-osc 并人工执行）
- 恢复/覆盖式导入、`Replace` 语义的批量导入
- 任何无法生成确定性回滚脚本的动作

### 5.7 环境与权限守卫

- `DbConnection` 新增：
  - `environment: Option<Environment>`（`dev`/`staging`/`prod`）
  - `ops_write_enabled: bool`（默认 false）
  - `ops_max_affected_rows`、`ops_max_table_bytes`（可选覆盖全局）
- 生产环境：
  - 默认只读 + dry-run；
  - 打开写权限需要显式配置 + 二次确认（输入库名/表名）；
  - L4 永远人工。
- 运维写接口增加本地令牌：
  - 启动时生成随机 `ops_token`，写入 `~/.local-ai-sql/ops/session.json`（0600）；
  - 前端通过配置/握手获取，写接口要求 `X-Ops-Token`；
  - 校验 `Origin`，拒绝非白名单来源；令牌随服务重启轮换。

### 5.8 审计

- append-only JSONL：`~/.local-ai-sql/ops/audit.jsonl`。
- 每条记录包含：
  - `ts`、`plan_id`、`scenario_id`、`db_id`、`db_fingerprint`、`environment`
  - `evidence_hash`、`plan_hash`、`prompt_version`
  - `action`、`compiled_sql`、`approval_id`、`approved_by`
  - `dry_run`、`result`、`affected_rows`、`duration_ms`、`error`
  - `backup_artifact`、`rollback_plan_id`
- 审计写入失败时**禁止执行**（fail closed）。
- 复用 `handlers/go_live.rs` 的 JSONL 报告模式，保持格式一致。

---

## 6. 七项能力逐项方案

### 6.1 能力 1：SQL 语句与数据库操作的 Agent 编排（主干）

**现状**：`agent.rs` 单 Agent + 4 个只读工具；`AgentResult` 的输出结构固定为 SQL 意图（`sql`/`explanation`/`task_type` 等）；无审批/执行。

**方案**：
- 保留现有 SQL Agent 作为 `sql_assistant` 场景，不改其行为。
- 新增 `ops` 场景体系（第 4 节）；运维场景与 SQL 场景共享模型配置、SSE、审计。
- 统一入口：
  - `POST /backend/ops/scenarios` 列表；
  - `POST /backend/ops/analyze`（非流式）与 `/backend/ops/analyze/stream`（SSE）；
  - 计划、审批、执行、回滚、审计分资源。
- 场景结果统一落到 `OpsPlan`；SQL 助手的结果可作为 `sql_review` 场景的输入。

**落点**：
- `core_lib/src/ai/agent_ops.rs`、`core_lib/src/ai/extractor_ops.rs`
- `core_lib/src/ops/*`
- `web-server/src/handlers/ops.rs`、`web-server/src/main.rs` 路由与 `AppState`
- `web-ui/src/components/ops/*`、`web-ui/src/api.ts`

**验收**：至少一个场景在 dev 库跑通全链路；计划篡改、过期审批、越权动作全部被拒。

### 6.2 能力 2：便捷操作类功能优化

**现状**：表右键只有 3 个动作；结果表只能单行复制；缺运维快捷入口；连接无环境标识。

**方案（只做与运维直接相关的 4 项）**：
1. 表右键快捷：查看前 100 行、生成 SELECT 模板、复制表 DDL、索引建议、巡检该表、字段治理。
2. 结果集：多选行、复制选中行/当前页为 Markdown、列选择器、冻结列。
3. 运维 SQL 预设：表大小/索引使用/锁等待/慢查询/未使用索引，只读，带作用域说明。
4. 连接环境徽标与保护：dev/staging/prod 色彩、只读徽标、生产写操作二次确认。

**落点**：
- `web-ui/src/components/TableContextMenu.tsx` + `App/index.tsx` 回调
- `web-ui/src/components/SimpleDataTable.tsx`、`QueryResultsPanel.tsx`、`DataTable/index.tsx`
- `web-ui/src/components/ops/SqlPresetMenu.tsx`（新增）
- `core_lib/src/config.rs`（环境字段）、连接设置 UI
- `web-ui/src/api.ts`（预设可走 `/backend/sql/explain` 或 `/backend/execute` 只读）

**验收**：高频操作不超过 2 次点击；预设 SQL 全部通过只读校验；生产标识不可被前端绕过。

### 6.3 能力 3：数据表索引优化

**现状**：`TableDesigner` 有“AI 建议索引”，但只把列名丢给模型，没有 EXPLAIN、基数、数据分布、工作负载；`IndexInfo` 不支持复合索引；`DdlEngine` 复合索引会生成重复单列索引；执行无审批/备份。

**方案**：
- 证据：
  - 新增 `IndexMetaCollector`：`information_schema.STATISTICS` 按 `INDEX_NAME` 分组，采集 `SEQ_IN_INDEX`、`COLUMN_NAME`、`NON_UNIQUE`、`CARDINALITY`、`INDEX_TYPE`、`NULLABLE`；
  - `EXPLAIN FORMAT=JSON`（MySQL 8）解析 `access_type/key/rows/used_key_parts/filtered/Extra`，MariaDB/低版本回退表格；
  - 表统计与索引使用（`performance_schema.table_io_waits_summary_by_index_usage`，权限不足则省略）。
- 规则 + AI：
  - 确定性候选：等值列在前、范围列在后、排序列/分组列、覆盖索引、避免低基数前导列、避免重复前缀；
  - AI 只负责排序/解释/补充假设，不得新增不存在的列；
  - 每个候选必须附 `evidence_ref`、预期收益、置信度、写放大风险。
- 输出动作：`CreateIndex`（MVP 仅此一种）；阶段 1 再开放 `DropIndex`（必须证明冗余/未使用 + 原定义备份）和 `AnalyzeTable`。
- 编译：
  - `ALTER TABLE `t` ADD INDEX `idx` (`c1`,`c2`) ALGORITHM=INPLACE, LOCK=NONE;`
  - 校验：索引名唯一、列存在、无 `JSON/TEXT` 无前缀、唯一索引先做重复检查。
- 执行与验证：
  - 执行前后各跑一次 EXPLAIN，比较 `rows`、`key`、`access_type`；
  - 可选复跑 perf suite baseline；
  - 未改善则提示可回滚。
- UI：
  - `IndexAdvisorPanel`：选表、选择工作负载（当前 SQL / 历史 / 手填）、证据摘要、候选列表、SQL 预览、审批、执行时间线、回滚。

**落点**：
- 新增 `core_lib/src/ops/index_advisor.rs`、`core_lib/src/ops/index_meta.rs`、`core_lib/src/ai/tools/explain.rs`
- 修改 `core_lib/src/schema/ext.rs` 或新增独立 `IndexMeta`（推荐新增，避免破坏现有 4 字段 `IndexInfo` 与前端/Tauri）
- `web-server/src/handlers/ops.rs`
- `web-ui/src/components/ops/IndexAdvisorPanel.tsx`
- 复用 `DdlEngine` 仅作为 CREATE TABLE/兼容逻辑参考，**执行 SQL 由 `ddl_compiler.rs` 生成**

**MVP 边界**：MySQL 8.0/MariaDB 10.x、单表、仅新增索引、不删索引、不改列、不自动执行 DROP。

### 6.4 能力 4：数据表单表/多表字段优化

**场景**：字段冗余、语义变更、新表新结构 + 旧表兼容；例如把表 A 的 JSON/长字符串字段拆到表 B，迁移数据后下线 A 的字段。

**方案**：
1. **分析（只读）**
   - 表结构、列类型/长度、NULL 率、平均/最大长度、JSON 合法率、distinct 基数、样本；
   - 依赖：索引、唯一约束、外键、视图、触发器、生成列；
   - 风险预判：类型转换失败、长度溢出、JSON 非法、唯一键重复、外键孤儿、数据丢失、下游依赖。
   - 用户语义：`SemanticContract { 字段含义, 允许空, 取值范围, 兼容要求, 消费方, 切换窗口 }`；进入 prompt 与计划，但不作为“事实”。
2. **方案（dry-run）**
   - 新表 DDL、列映射、回填 SQL、校验规则、兼容期状态机、回滚脚本；
   - 不生成删列动作；`DROP COLUMN` 只作为 L4 人工脚本输出。
3. **执行（阶段 2）**
   - 建新表（L2，审批）；
   - 按 PK 区间分批回填（L3，每批事务 + checkpoint + 限速）；
   - 校验：行数覆盖、PK 区间 CRC32 checksum（`sync/mysql_engine.rs`）、样本对比；
   - 兼容期：应用双写/读切换由用户改代码；工具只验证覆盖率与差异；
   - 观察期后，人工审批 `DROP COLUMN`，执行前强制列快照备份。
4. **状态机**
   - `planned → table_created → backfilled → verified → dual_write_ready → read_switched → old_column_retired`
   - 每个状态持久化，任何一步失败可停可恢复；`old_column_retired` 永远人工。

**落点**：
- `core_lib/src/ops/field_governance.rs`
- `core_lib/src/ops/validate.rs`（数据校验规则，与 4.1 统一）
- `core_lib/src/ops/migration.rs`（分批回填/checkpoint）
- 复用 `sync/mysql_engine.rs` 的 compare/checksum/chunker
- `web-ui/src/components/ops/FieldGovernancePanel.tsx`
- `web-server/src/handlers/ops.rs`

**验收**：影子库完成一次 JSON 拆表迁移，checksum 一致、可回滚；生产默认 dry-run；任何数据丢失风险阻断。

### 6.5 能力 5：数据表性能优化

**方案**：
- 只读诊断：
  - 用户 SQL 或 SQL history / perf suite 中的慢查询；
  - `EXPLAIN` 结构化分析（全表扫描、临时表、filesort、低选择性索引）；
  - 表统计、索引使用、锁等待、长事务（`performance_schema`/`information_schema.innodb_trx`，权限不足则降级）。
- 输出分两类：
  - **DB 侧动作**：`CreateIndex`、`DropIndex`（阶段 1 后）、`AnalyzeTable`；`OPTIMIZE TABLE` 仅审批+维护窗口；类型变更/分区/引擎/字符集只出方案。
  - **代码侧建议**：SQL 改写、谓词可索引化、分页、N+1、批量大小、连接复用、缓存、冷热分离；只展示，不执行。
- 一键执行只覆盖 L1；L2+ 必须审批；L4 禁止自动。
- 验证：执行后复跑 EXPLAIN 与 perf baseline，记录 `rows/key/access_type/duration` 变化；无改善提示回滚。

**落点**：
- `core_lib/src/ops/perf_advisor.rs`
- 新增只读工具 `core_lib/src/ai/tools/table_stats.rs`、`ai/tools/lock_diag.rs`
- 复用 `perf_report.rs`、`handlers/perf.rs`
- `web-ui/src/components/ops/PerfAdvisorPanel.tsx`
- 与 `PerfDiagnosticsPanel` 并列或嵌入

**禁止**：`SET GLOBAL`、buffer pool/日志参数、分区、引擎、字符集、`KILL` 其他会话；这些只给建议。

### 6.6 能力 6：批量数据复制为 Markdown 表格

**现状**：`DataTable` 与 `SimpleDataTable` 的右键仅支持单行复制：单元格、行 TSV、行 JSON、行 INSERT SQL；没有多选、没有 Markdown 表格，也没有列类型信息。

**方案（纯前端，零数据库风险）**：
- 新增 `web-ui/src/utils/markdownTable.ts`：
  - `toMarkdownTable(columns, rows, options)`；
  - 转义 `|`、反斜杠，单元格内换行转 `<br>`；
  - `NULL` 显示为空或 `NULL`（可配置）；
  - 数字/布尔不加引号；字符串保留原值，不额外加引号；
  - 保留列顺序；可选“截断单元格”但默认不截断，超长时提示字符数。
- 交互：
  - 表格增加 checkbox / Shift 区间选择；
  - 右键菜单：复制选中行 Markdown、复制当前页 Markdown、复制已加载全部 Markdown；
  - 结果面板工具栏增加“复制为 Markdown”；
  - 复制前预览弹窗，显示行数、字符数、前几行，避免剪贴板过大。
- 两个表格都接入：`SimpleDataTable`（查询结果）与 `DataTable`（表数据）。
- 验收：
  - 单元格含 `|`、换行、反引号、中文、emoji、JSON 字符串、NULL 的表都能生成合法 Markdown；
  - 1 万行性能可接受，超过阈值提示分批；
  - 单元测试覆盖转义与边界。

**落点**：
- `web-ui/src/utils/markdownTable.ts`（新增）
- `web-ui/src/components/SimpleDataTable.tsx`
- `web-ui/src/components/DataTable/index.tsx`
- `web-ui/src/components/QueryResultsPanel.tsx`
- `web-ui/src/components/CopyPreviewDialog.tsx`（新增复制预览弹窗）

**注意**：该能力不依赖 Agent，可独立提前交付；不要为了它引入后端接口。

### 6.7 能力 7：定期巡检

**现状**：仓库没有巡检、调度、告警相关代码；只有单次 perf suite 与 go-live 检查。

**方案**：
- 检查项全部只读，首批预设：
  1. **健康基础包**：表行数异常波动、主键/唯一键重复、关键字段 NULL 率、时间字段未来/过旧、`AUTO_INCREMENT` 使用率 > 80%、表/索引体积增长。
  2. **数据质量包**：数值越界/负数、JSON 非法率、长文本超长、枚举值漂移、业务唯一键重复、外键孤儿（可选）。
  3. **索引与性能包**：冗余/重复索引、未使用索引（需 performance_schema）、高频查询缺索引、锁等待/长事务、慢查询 top N。
  4. **安全合规包**：`SUPER`/`%` 主机账号、空密码/过期账号、过度授权、敏感字段明文采样（本地脱敏）、表/列注释缺失、字符集不一致。
  5. **结构与容量包**：schema drift（与知识库 DDL 对比）、分区缺失、磁盘/表空间增长、备份时效（本地目录/文件时间）。
- 自定义检查：
  - 只允许单条只读 `SELECT`，必须返回约定列（`severity/metric/value/threshold/message/sample`）；
  - 校验只读、禁止多语句、强制 `LIMIT` 与超时。
- 调度：
  - `InspectionScheduler` 在 `web-server` 启动后 `tokio::spawn`；
  - 配置支持“每 N 分钟/每天 HH:MM”，启动时补跑漏掉的窗口；
  - 复用 `job_semaphore` 控制并发；单次运行有超时与取消。
- 存储与告警：
  - 运行结果 JSONL：`~/.local-ai-sql/ops/inspections/runs.jsonl`；
  - 基线：最近 N 次滚动中位数，前几次只记录不告警；
  - 告警去重：`fingerprint = db + check_id + severity + 归一化 message`，冷却期 + 静默；
  - 通道：应用内通知/告警列表 + 通用 webhook（可选飞书/Slack）；MVP 不做邮件。
- AI（可选）：只做巡检结果摘要与优先级排序，不参与判定；摘要失败不影响告警。

**落点**：
- `core_lib/src/ops/inspection/*`
- `web-server/src/handlers/inspection.rs`
- `web-server/src/main.rs` 启动调度器、路由
- `web-ui/src/components/ops/InspectionCenter.tsx`、`InspectionRunDetail.tsx`
- `web-ui/src/api.ts`

**验收**：
- 5 个以上预设可配置、可运行、可告警；
- 首次运行不误报；连续两次异常才升级告警；
- 漏跑窗口在启动后补跑；
- 所有检查 SQL 通过只读与单语句校验。

---

## 7. 路由、AppState 与前端落点清单

### 7.1 新增 HTTP 路由（全部挂在 `/backend`）

| 方法/路径 | 作用 |
|---|---|
| `GET /ops/scenarios` | 场景列表、输入 schema、风险等级、是否可写 |
| `POST /ops/analyze` | 非流式：采集证据 + 生成计划 |
| `POST /ops/analyze/stream` | SSE：证据/计划/校验事件 |
| `GET /ops/plans` | 计划/执行历史（分页、按连接/场景过滤） |
| `GET /ops/plans/:plan_id` | 计划详情（含证据摘要、动作、SQL、回滚） |
| `POST /ops/plans/:plan_id/approve` | 审批，返回一次性令牌 |
| `POST /ops/plans/:plan_id/execute` | dry-run / apply（需审批令牌） |
| `POST /ops/plans/:plan_id/rollback` | 回滚（需审批令牌与理由） |
| `GET /ops/audit` | 审计查询（分页/过滤） |
| `GET /ops/policy` / `PUT /ops/policy` | 运维策略读写 |
| `GET/POST /inspections` | 巡检配置列表/创建 |
| `POST /inspections/:id/run` | 立即运行 |
| `GET /inspections/:id/runs` | 运行历史 |
| `GET /inspections/presets` | 内置预设 |
| `POST /inspections/:id/silence` | 静默告警 |

实现落点：`web-server/src/handlers/ops.rs`、`web-server/src/handlers/inspection.rs`，在 `main.rs:913-1006` 的 `api` 链追加；不要改 `routes.rs`（它未生效）。

### 7.2 AppState 新增字段

```rust
pub ops_registry: Arc<ops::ScenarioRegistry>,
pub ops_store: Arc<RwLock<ops::PlanStore>>,        // 计划/审批/运行状态，本地文件持久化
pub ops_policy: Arc<RwLock<ops::OpsPolicy>>,
pub inspection_store: Arc<RwLock<inspection::InspectionStore>>,
pub ops_tokens: Arc<ops::LocalToken>,             // 本地写接口令牌
```

在 `web-server/src/state.rs:307-327` 扩展；启动时加载 `~/.local-ai-sql/ops/`，并 spawn 巡检调度器。

### 7.3 前端

| 文件 | 改动 |
|---|---|
| `web-ui/src/components/ops/OpsCenter.tsx` | 新增：场景卡片、最近计划、运行历史 |
| `web-ui/src/components/ops/OpsPlanReview.tsx` | 新增：诊断/证据/动作勾选/SQL 预览/风险/回滚/确认输入 |
| `web-ui/src/components/ops/OpsRunTimeline.tsx` | 新增：SSE 执行时间线、失败/回滚状态 |
| `web-ui/src/components/ops/IndexAdvisorPanel.tsx` | 新增：索引 MVP 主界面 |
| `web-ui/src/components/ops/InspectionCenter.tsx` | 新增：巡检配置/预设/运行/告警 |
| `web-ui/src/utils/markdownTable.ts` | 新增：Markdown 生成与转义 |
| `web-ui/src/components/CopyPreviewDialog.tsx` | 新增：复制预览 |
| `web-ui/src/App/index.tsx` | 新增 `ops-center`、`inspection` tab 与导航回调 |
| `web-ui/src/components/ToolsNav.tsx` | 新增“运维中心/定期巡检”入口 |
| `web-ui/src/components/TableContextMenu.tsx` | 新增索引建议/巡检/字段治理 |
| `web-ui/src/components/SimpleDataTable.tsx` | 多选 + Markdown 复制菜单 |
| `web-ui/src/components/DataTable/index.tsx` | 同上 |
| `web-ui/src/components/QueryResultsPanel.tsx` | 工具栏 Markdown 复制按钮 |
| `web-ui/src/api.ts` | 新增 ops/巡检 API、SSE 订阅、`X-Ops-Token` |
| `web-ui/src/types.ts` | 新增 `OpsPlan`/`OpsAction`/`InspectionRun` 类型 |

---

## 8. 阶段 0 任务拆解（可直接排期）

| # | 任务 | 主要文件 | 验收 |
|---|---|---|---|
| 0.1 | 定义 `OpsPlan/ActionSpec/EvidencePack/Risk` | `core_lib/src/ops/types.rs` | serde/JSON Schema 测试 |
| 0.2 | 场景注册与 `OpsContext` | `ops/scenario.rs`、`ops/context.rs` | registry 单测；两个空场景可列出 |
| 0.3 | 只读证据工具：结构化 EXPLAIN、表统计、索引元数据 | `ai/tools/explain.rs`、`ai/tools/table_stats.rs`、`ops/index_meta.rs` | EXPLAIN 固定样本解析测试 |
| 0.4 | Ops Agent 运行器 + `extract_ops_plan` | `ai/agent_ops.rs`、`ai/extractor_ops.rs` | 合法/非法/超长输出测试 |
| 0.5 | 计划校验 + DDL 编译 | `ops/validate.rs`、`ops/ddl_compiler.rs` | 禁止动作、系统库、跨库、复合索引、转义测试 |
| 0.6 | 审批存储 + 一次性令牌 + 审计 | `ops/approval.rs`、`ops/audit.rs` | 篡改计划/过期/复用全部拒绝 |
| 0.7 | 执行器 dry-run/apply + 回滚 + 状态机 | `ops/executor.rs`、`ops/backup.rs` | dev MySQL `CREATE INDEX` → 复测 → `DROP INDEX` |
| 0.8 | Web handlers/routes/AppState | `web-server/src/handlers/ops.rs`、`main.rs`、`state.rs` | 路由测试；未审批执行返回 403 |
| 0.9 | 索引场景 | `ops/index_advisor.rs` | 3 组 EXPLAIN 样本给出合法候选 |
| 0.10 | 运维中心 + 计划审阅 UI | `web-ui/src/components/ops/*`、`App/index.tsx` | 全链路可点击完成 |
| 0.11 | Markdown 批量复制 | `utils/markdownTable.ts`、两个表格 | 转义/多选/预览测试 |

**阶段 0 不做**：删索引、字段迁移、巡检调度、PG/SQLite、自动执行 L3/L4、生产写入。

---

## 9. 额外值得做的运维/安全场景

按“读代码后发现”的优先级排序：

1. **锁与事务诊断（只读，建议阶段 1）**
   - 当前无 `performance_schema`/`innodb_trx` 采集；DDL 前最需要知道锁等待和长事务。
   - 复用只读证据工具；输出只建议，不 `KILL`。
2. **账号与权限安全体检（只读，阶段 1）**
   - `mysql.user`/`SHOW GRANTS`：`SUPER`、`%` 主机、空密码、过期账号、过度授权。
   - `DbSecurityManager` 已有 GRANT 预览，可扩展为只读体检 + 建议脚本；不自动 `GRANT/REVOKE`。
3. **数据一致性对账（只读，阶段 1）**
   - 复用 `sync/mysql_engine.rs` 的 PK 区间 checksum，把现有“数据同步比对”产品化为巡检项。
   - 支持双库/主从对账，输出缺失/多出/内容不一致行。
4. **备份时效与恢复演练（读检查 + 人工恢复，阶段 2）**
   - 检查备份文件时间/大小/可读性；恢复只允许到影子库，且人工。
5. **容量与增长预测（只读，阶段 1/2）**
   - 采集历史表大小、行数、索引大小，做增长趋势与阈值告警；无需改库。
6. **变更风险预检 / 发布门禁联动（只读，阶段 2）**
   - 把 `ops` 计划的 dry-run 结果接入现有 go-live gate：DDL 风险、影响行数、回滚完备性作为门禁项。
7. **依赖影响分析（只读，阶段 2 前置）**
   - 视图/触发器/存储过程/外键/生成列依赖；字段治理和删列必须先有它，否则无法判断破坏面。
8. **SQL 风险审计与排行（只读，阶段 1/2）**
   - 复用 SQL history，统计高危语句、无 WHERE 更新、全表扫描、慢查询频次；只提示不改写。
9. **敏感数据发现与脱敏采样（只读，阶段 2）**
   - 列名/注释/样本正则识别，采样必须本地脱敏、最小化、可关闭；审计中不落原文。
10. **配置漂移巡检（只读，阶段 2）**
    - 对比关键变量（`innodb_buffer_pool_size`、`max_connections`、`slow_query_log` 等）与基线；只建议，不 `SET GLOBAL`。

**架构承载判断**：1/2/3/5/7/8/9/10 都是只读，可直接复用 `EvidencePack` + 巡检框架；4 需要恢复执行，归入 L4 人工；6 是只读门禁，复用 go-live。

---

## 10. 桌面端（Tauri）同步策略

### 10.1 现状

- `web-ui/src/api.ts:16-20` 通过 `window.__TAURI__` 判断桌面；只读 SQL 优先 `invokeDesktopOrHttp('workbench_run')`，失败回退 HTTP。
- Tauri 只注册 6 个命令（`lib.rs:1634-1641`）：`workbench_run/cancel/transaction`、`table_page`、`table_schema`、`perf_probe`。
- `resolve_connection` 只支持 MySQL/MariaDB；`perf_probe` 只允许 `SELECT/SHOW/DESCRIBE/EXPLAIN`。
- `tauri.conf.json` 无 sidecar/externalBin。开发期 Vite 代理 `/backend` 到 `127.0.0.1:3000`；生产静态包没有代理，standalone 桌面无法访问 `/backend`。

### 10.2 结论与建议

- **不补一套业务命令**。复制执行器/审批/审计会产生两套状态，风险和一致性成本高。
- **首选方案：把 `web-server` 打包为 Tauri sidecar**：
  - `externalBin` 引入服务端二进制；启动时由 Tauri 拉起，随机端口/固定 127.0.0.1，注入 `X-Ops-Token`；
  - 前端仍走 `/backend`，Web 与桌面共享同一套 Agent/审批/审计/巡检；
  - 桌面离线时由 sidecar 本地运行，不需要用户手动启动服务。
- **过渡方案（若暂不做 sidecar）**：
  - 只读场景可补 3 个 Tauri 命令（`ops_scenarios`、`ops_analyze`、`inspection_run`），内部调用 `core_lib::ops`；
  - 写操作仍必须走 HTTP 服务端审批；桌面 UI 在 HTTP 不可用时禁用“执行”；
  - 明确提示“桌面 standalone 需启动本地服务”。
- **多方言**：桌面 `resolve_connection` 先扩 PG/SQLite 或统一走 sidecar；否则运维能力仅在 MySQL 可用。

---

## 11. 测试与验收策略

### 11.1 后端

- `core_lib/tests/ops_plan.rs`：
  - `ActionSpec` 序列化/反序列化；
  - 非法动作（`RawSql`、系统库、跨库、未存在列）拒绝；
  - 复合索引编译、标识符转义、默认值转义。
- `core_lib/tests/ops_safety.rs`：
  - 计划哈希篡改 → 审批拒绝；
  - 审批过期/复用 → 执行拒绝；
  - `DROP TABLE`/`TRUNCATE`/`DROP COLUMN` 不进入可执行动作；
  - `MODIFY COLUMN` 不会被标为 `LOCK=NONE`；
  - 执行超时/连接中断 → `unknown`，不继续后续动作。
- `core_lib/tests/ops_executor_mysql.rs`（需 `TEST_MYSQL_URL`，无则 skip）：
  - `CREATE INDEX` → 复测 → `DROP INDEX`；
  - 备份产物存在且可读；
  - 审计 JSONL 每条动作一行。
- `web-server` 路由测试（参考 `main.rs` 内已有 axum 测试）：
  - 未审批执行 403；
  - 只读连接写接口 403；
  - SSE 事件顺序。
- 巡检：
  - 预设 SQL 单测（只读、单语句、LIMIT）；
  - 首次运行不告警、连续异常升级、冷却去重；
  - 漏跑窗口补跑。

### 11.2 前端

- Vitest：
  - `markdownTable` 转义（`|`、换行、反斜杠、NULL、JSON、中文/emoji）；
  - 多选/区间选择；
  - 预览字符数。
- 组件测试：计划审阅中未勾选动作不能提交；高危动作必须输入表名。

### 11.3 手工验收（dev 环境）

1. 在 dev MySQL 建测试表（含复合索引、JSON 列、10 万行数据）。
2. 输入一条慢 SQL → 采集 EXPLAIN → 生成索引计划 → 审批 → 执行 `CREATE INDEX` → 复测 → 回滚 `DROP INDEX`。
3. 人为篡改计划哈希/复用审批 → 必须失败。
4. 断开数据库 → 执行中状态为 `unknown`，不自动继续。
5. 批量复制：选中 100 行 → Markdown 预览 → 粘贴到 AI 对话中结构正确。
6. 巡检：配置一条“NULL 率”检查 → 手动运行 → 告警 → 静默 → 恢复。

**禁止**：任何测试触碰生产连接；测试库名必须带 `_ops_dev` 前缀并在测试后恢复快照。

---

## 12. 风险与待决策项

| # | 待决策 | 建议 |
|---|---|---|
| 1 | 桌面端 sidecar 还是补 Tauri 命令 | 优先 sidecar；至少阶段 0 明确桌面 standalone 的降级提示 |
| 2 | 是否允许生产环境“一键执行” L1 | 默认关闭；先 dev/staging 验证，生产只读 + dry-run |
| 3 | 是否允许 AI 生成自由 SQL 执行 | 不允许；只允许 `ActionSpec` 编译，自由 SQL 仅用于只读探测 |
| 4 | 首期数据库范围 | MySQL/MariaDB；PG/SQLite 先只读诊断，索引元数据补齐后再开放写 |
| 5 | 是否自动删除索引/列 | 阶段 0/1 不删；删索引需冗余证明 + 原定义备份；删列永远人工 |
| 6 | 巡检告警通道 | 应用内 + 通用 webhook；邮件/短信不做 |
| 7 | 样本数据隐私 | 默认脱敏、最小采样、审计不落原文；允许按连接关闭采样 |
| 8 | 团队协作/多用户 | 当前按单机单用户设计；审计本地 append-only；多人需求再引入认证与角色 |
| 9 | 是否迁移 `routes.rs` | 不纳入本方案；新增路由写 `main.rs`，避免无关重构 |

### 12.1 已确认的技术债（需在实施中规避）

- `routes.rs` 未生效且与 `main.rs` 路由重复。
- `DdlEngine` 的算法分类不可信：`MODIFY COLUMN` 标为 `INPLACE, LOCK=NONE`；`DROP COLUMN` 一律假设 `INSTANT`，未按版本/引擎判断。
- `DdlEngine` 自动生成 `DROP COLUMN`；`SchemaSyncEngine` 对 removed table 生成 `DROP TABLE`。
- `IndexInfo` 不支持复合索引；`schema/extractor.rs` 只实现 MySQL。
- `execute_ddl` 直接执行客户端 SQL，含 `DROP/TRUNCATE`，无审批/审计/备份。
- `execute_sql` 的 `force` 由客户端提供，不是服务端审批。
- 桌面 standalone 无 HTTP 服务；Tauri 只支持 MySQL。

---

## 13. 关键代码索引

| 主题 | 位置 |
|---|---|
| AI 模块导出 | `core_lib/src/ai/mod.rs:1-8` |
| 任务类型/工具/max_turns | `core_lib/src/ai/agent.rs:73-167` |
| system prompt 拼装 | `core_lib/src/ai/agent.rs:169-229` |
| Agent 结果结构 | `core_lib/src/ai/events.rs:79-99` |
| SQL intent 提取 | `core_lib/src/ai/extractor.rs:3-85` |
| 全局 Policy | `core_lib/src/ai/policy_store.rs:19-47` |
| 只读 SQL 工具 | `core_lib/src/ai/tools/executor.rs:36-118` |
| 索引元数据结构 | `core_lib/src/schema/ext.rs:6-11`、`58` |
| MySQL schema 提取 | `core_lib/src/schema/extractor.rs:7-38`、`40+` |
| DDL 生成/算法分类 | `core_lib/src/tools.rs:47-222`、`441-475` |
| 数据同步校验/分块 | `core_lib/src/sync/mysql_engine.rs:20-40`、`74`、`318`、`908`、`929` |
| schema diff/DDL 选择 | `core_lib/src/sync/engine.rs:9-47` |
| 文件/DB 传输 | `core_lib/src/sync/transfer.rs:21-62` |
| SQL 执行安全守卫 | `core_lib/src/service/workbench.rs:107-133`、`497-537` |
| 危险语句分类 | `core_lib/src/sql/util.rs:386-431` |
| Web 路由 | `web-server/src/main.rs:913-1006`、`1040-1041` |
| DDL 预览/执行 | `web-server/src/main.rs:2217-2281` |
| AppState | `web-server/src/state.rs:307-327` |
| 未生效的路由文件 | `web-server/src/routes.rs:1-21` |
| Tauri 命令 | `web-ui/src-tauri/src/lib.rs:1634-1641` |
| 前端导航/tab | `web-ui/src/App/index.tsx:1970-2019` |
| 表右键 | `web-ui/src/components/TableContextMenu.tsx` |
| 结果表格复制 | `web-ui/src/components/SimpleDataTable.tsx`、`DataTable/index.tsx` |
| 表设计器 AI 索引建议 | `web-ui/src/components/TableDesigner.tsx` |
| 性能诊断 | `web-ui/src/components/PerfDiagnosticsPanel.tsx`、`web-server/src/handlers/perf.rs` |
| 前端 API/运行时切换 | `web-ui/src/api.ts:16-123` |
| 连接配置 | `core_lib/src/config.rs:268-291` |
| 超时策略 | `core_lib/src/timeout_policy.rs:3-40` |

---

## 14. 落地顺序速览

1. **先做安全基线**：环境标签/写权限/本地令牌/审计；把 `/table/ddl` 标记为“仅人工/兼容旧入口”，不接入 Agent。
2. **再做 ops 底座 + 索引 MVP**：只跑 dev，只做 `CREATE INDEX`，打通审批/执行/验证/回滚。
3. **并行交付 Markdown 批量复制**：零风险、独立价值。
4. **阶段 1 扩只读**：性能诊断、巡检、便捷入口；所有写动作维持 L1。
5. **阶段 2 数据治理**：先做只读风险预判与 dry-run，再开分批回填；删列永远人工。
6. **阶段 3 多方言/桌面**：sidecar 统一路径，PG/SQLite 先只读。
