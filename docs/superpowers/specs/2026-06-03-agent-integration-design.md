# Agent 集成设计方案：rig-core 替换当前 SQL 生成交互

> 日期：2026-06-03
> 状态：Reviewed
> 范围：core_lib AI 层 + web-server + 前端全链路重构

## 背景与动机

当前 SQL 生成采用**单次 LLM 调用**模式：构建完整 prompt（含 schema、knowledge、history）→ 发送给 LLM → 解析返回 JSON。LLM 无法在推理过程中主动查询 schema、执行 SQL 验证或参考已有规则，导致：

- 大型 schema 浪费 token（全量注入而非按需查询）
- 生成的 SQL 无法自动验证，错误只能由用户发现
- 无自修正能力，一次失败就需要用户手动重试
- 无流式反馈，用户等待体验差

本方案用 rig-core（Rust 原生 Agent 框架，7.5k stars）完全替换现有 AI 层，引入 Tool Use + ReAct 循环 + SSE Streaming。

## 当前架构

```
AppConfig
  → AiGateway (自定义 HTTP, 6 providers)
    → Planner (单次调用, 规则快速路径 + LLM)
      → extractor.rs (JSON → markdown → raw SQL 解析)
```

**涉及文件：**
- `core_lib/src/ai/gateway.rs` — 自定义 HTTP 客户端
- `core_lib/src/ai/planner.rs` — SQL 生成编排器
- `core_lib/src/ai_agent.rs` — AiRouter 查询分发
- `core_lib/src/ai/prompting.rs` — Prompt 模板构建
- `core_lib/src/ai/extractor.rs` — 结构化输出解析
- `core_lib/src/ai/policy_store.rs` — 策略管理
- `web-server/src/ai_handlers.rs` — HTTP handler
- `web-ui/src/queryAiActions.ts` — 前端 AI 调用
- `web-ui/src/api.ts` — API 客户端

## 目标架构

```
AppConfig
  → rig Provider Clients (OpenAI / Anthropic / Custom)
    → SqlAgent (rig Agent + Tool loop)
      ├── QuerySchemaTool
      ├── ExecuteSqlTool
      ├── QueryRulesTool
      └── QueryKnowledgeTool
      → SSE streaming → 前端实时展示
```

## Provider 映射

| 当前 Provider | rig 实现 | 说明 |
|---|---|---|
| OpenAI | `rig::providers::openai` | 原生支持 |
| Anthropic | `rig::providers::anthropic` | 原生支持 |
| Deepseek | `rig::providers::openai` + base_url | OpenAI 兼容 API |
| Moonshot | `rig::providers::openai` + base_url | OpenAI 兼容 API |
| 智谱 (Zhipu) | `rig::providers::openai` + base_url | OpenAI 兼容 API |
| Custom | `rig::providers::openai` + base_url | 用户自定义 |

**tier/thinking 参数处理：** 通过 rig 的 `additional_params` 注入自定义 JSON 字段（temperature、max_tokens、reasoning_effort、thinking config），保持与现有 tier 系统的兼容。

**Token Pool 机制：** 在 rig provider client 外层封装 token pool 逻辑，每个 API key 对应一个 rig client 实例，选择逻辑同现有 `choose_pool_token`。

## Agent 定义

**Agent 生命周期：** `SqlAgent` 由 `SqlAgentBuilder` 在每次请求时构建（因为需要注入当前请求的 db_client、rule_store 等运行时状态）。Provider client 实例可从连接池复用。规则快速路径在 Agent 构建前执行：命中则直接返回，不创建 Agent 实例。

```rust
// core_lib/src/ai/agent.rs
pub struct SqlAgentBuilder {
    config: AppConfig,
    db_client: Option<DbClient>,
    rule_store: Option<RuleStore>,
    knowledge_base: Option<KnowledgeBase>,
    policy: Policy,
}

impl SqlAgentBuilder {
    pub fn build(self) -> SqlAgent;
}

pub struct SqlAgent {
    agent: rig::agent::Agent<...>,
}

impl SqlAgent {
    /// 主入口：带 streaming 的 Agent 调用
    pub async fn run_streaming(
        &self,
        query: &str,
        chat_history: &[ChatMessage],
    ) -> impl Stream<Item = AgentEvent>;

    /// 兼容入口：非 streaming，收集所有事件后返回最终结果
    pub async fn run(
        &self,
        query: &str,
        chat_history: &[ChatMessage],
    ) -> Result<AgentResult, AiError>;
}
```

## Tool 定义

### QuerySchemaTool

```rust
pub struct QuerySchemaTool {
    db_client: DbClient,
    db_name: String,
}

// 输入参数 JSON Schema:
{
    "table_name": "string (optional, 不传则返回所有表列表)",
    "include_columns": "boolean (default true)",
    "include_indexes": "boolean (default false)",
    "include_foreign_keys": "boolean (default false)"
}

// 返回: 表结构详情的 JSON 文本
```

### ExecuteSqlTool

```rust
pub struct ExecuteSqlTool {
    db_client: DbClient,
}

// 输入参数:
{
    "sql": "string (要执行的 SQL)",
    "limit": "integer (default 20, 返回行数上限)"
}

// 安全限制: 只允许 SELECT / SHOW / DESCRIBE / EXPLAIN
// 返回: 执行结果或错误信息
```

### QueryRulesTool

```rust
pub struct QueryRulesTool {
    rule_store: RuleStore,
    policy: Policy,
}

// 输入参数:
{
    "query": "string (自然语言查询)"
}

// 返回: 匹配的规则列表（prompt_pattern + sql_template + confidence）
```

### QueryKnowledgeTool

```rust
pub struct QueryKnowledgeTool {
    knowledge_base: KnowledgeBase,
    db_connection_id: Option<String>,
}

// 输入参数:
{
    "query": "string (搜索关键词)",
    "limit": "integer (default 5)"
}

// 返回: 匹配的知识条目列表
```

## SSE Streaming 协议

### 后端端点

```
POST /backend/chat/stream          — Agent streaming（新）
POST /backend/api/ai/query/stream  — Agent streaming（新，带 mode 支持）
POST /backend/chat                 — 保留，兼容旧版
POST /backend/api/ai/query         — 保留，兼容旧版
```

### 事件类型

| event | data 格式 | 说明 |
|---|---|---|
| `thinking` | `{ "text": "..." }` | Agent 推理过程 |
| `tool_call` | `{ "tool": "query_schema", "args": {...} }` | 正在调用工具 |
| `tool_result` | `{ "tool": "query_schema", "result": "..." }` | 工具返回结果 |
| `sql_draft` | `{ "sql": "..." }` | 中间 SQL 草稿 |
| `final_sql` | `{ "sql": "...", "task_type": "..." }` | 最终验证通过的 SQL |
| `explanation` | `{ "text": "..." }` | 最终解释 |
| `error` | `{ "message": "..." }` | 错误信息 |
| `done` | `{}` | 流结束 |

## ReAct 循环流程

```
用户输入 → 规则引擎匹配?
  ├── 直接命中 → 返回 SQL（跳过 Agent，秒级响应）
  ├── 建议命中 → 注入 extra_guidance 到 Agent preamble
  └── 未命中   → 进入 Agent ReAct 循环 ↓

Agent ReAct 循环（rig 管理）:
  1. Agent 接收用户请求 + preamble（含 dialect、输出格式约束）
  2. Agent 自主决定调用哪些 Tool：
     - 可能先查 schema → 再查 rules → 再生成 SQL → 再执行验证
     - 执行出错 → 收到错误 → 自动修正 → 重新执行
  3. 最多 max_turns=5 轮（可配置），超时返回最后状态
  4. 最终输出结构化 JSON（复用现有 output_schema 定义）
```

## 前端变更

### api.ts 新增

```typescript
chatToSqlStream: (query: string, chatHistory?: any[]): ReadableStream =>
  fetch('/backend/chat/stream', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ query, chat_history: chatHistory })
  }).then(res => res.body)
```

### queryAiActions.ts 新增

```typescript
export async function runGenerateSqlStream(params: {
  query: string
  onThinking: (text: string) => void
  onToolCall: (tool: string, args: any) => void
  onToolResult: (tool: string, result: string) => void
  onSqlDraft: (sql: string) => void
  onFinalSql: (sql: string, taskType: string) => void
  onExplanation: (text: string) => void
  onError: (message: string) => void
  onDone: () => void
}) { /* SSE 解析逻辑 */ }
```

### 新增组件：AgentProcessPanel

展示 Agent 推理过程的可折叠面板：
- thinking 气泡（灰色，可折叠）
- 工具调用状态条（"正在查询表结构..." + spinner）
- 工具返回结果（折叠显示）
- SQL 草稿（带 loading 动画，最终确认后高亮）

位于查询编辑器下方、结果面板上方。

## 模块变更总结

### 删除
- `core_lib/src/ai/gateway.rs` — 被 rig provider 替代
- `core_lib/src/ai/planner.rs` — 被 SqlAgent 替代
- `core_lib/src/ai_agent.rs` — 被 SqlAgent 替代
- `core_lib/src/ai/types.rs` — ChatMessage/AiError 重新定义

### 新增
- `core_lib/src/ai/agent.rs` — SqlAgent 定义、Provider 解析、Agent 构建
- `core_lib/src/ai/tools/mod.rs` — Tool 模块入口
- `core_lib/src/ai/tools/schema.rs` — QuerySchemaTool
- `core_lib/src/ai/tools/executor.rs` — ExecuteSqlTool
- `core_lib/src/ai/tools/rules.rs` — QueryRulesTool
- `core_lib/src/ai/tools/knowledge.rs` — QueryKnowledgeTool
- `core_lib/src/ai/events.rs` — AgentEvent 枚举定义

### 保留（可能小幅调整）
- `core_lib/src/ai/prompting.rs` — Prompt 模板迁入 Agent preamble 构建
- `core_lib/src/ai/extractor.rs` — 仍用于解析 Agent 最终输出
- `core_lib/src/ai/policy_store.rs` — 策略控制 Agent 行为（新增 `agent_max_turns` 字段，默认 5）
- `core_lib/src/ai/mod.rs` — 更新模块导出

### 调整
- `web-server/src/ai_handlers.rs` — 新增 streaming handlers，重构现有 handlers 调用 SqlAgent
- `web-server/src/main.rs` — 注册新路由
- `web-ui/src/api.ts` — 新增 streaming API 方法
- `web-ui/src/queryAiActions.ts` — 新增 streaming 调用逻辑
- `web-ui/src-tauri/src/lib.rs` — Tauri 命令适配新 SqlAgent 接口

## 错误处理

| 场景 | 策略 |
|---|---|
| Agent 循环超时 | `max_turns=5`，超时返回 `AgentEvent::error` + 最后推理状态 |
| ExecuteSqlTool 安全 | 只允许 SELECT/SHOW/DESCRIBE/EXPLAIN，DML/DDL 直接拒绝返回错误给 Agent |
| 工具调用失败 | Agent 收到错误信息后自主决定重试或放弃 |
| Provider 不支持 tool calling | 降级为单次调用模式（同现有行为），日志记录降级原因 |
| SSE 连接断开 | 后端 Agent 继续执行但停止推送，下次请求重新开始 |
| rig API 不兼容的 provider 参数 | 通过 `additional_params` 注入自定义 JSON 字段 |

## 依赖变更

```toml
# core_lib/Cargo.toml
[dependencies]
rig-core = { version = "0.36", features = ["derive"] }  # Tool derive macro
tokio-stream = "0.1"        # SSE streaming support
```

## 验收标准

1. 所有 6 个 provider 在 rig 下正常工作（含 tier/thinking 参数）
2. Agent 能自主调用 4 个工具完成 SQL 生成
3. SQL 执行错误时 Agent 能自动修正（至少一次重试）
4. SSE streaming 端点正常推送所有事件类型
5. 前端实时展示 Agent 推理过程
6. 规则直接命中时秒级响应（跳过 Agent）
7. 现有 `/chat` 和 `/api/ai/query` 非 streaming 端点保持兼容
8. `cargo test --workspace` 全部通过
9. Tauri 桌面端 AI 功能正常
