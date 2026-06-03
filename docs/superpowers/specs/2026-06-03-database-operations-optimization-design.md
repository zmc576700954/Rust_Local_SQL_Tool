# 数据库操作优化设计文档

**日期**: 2026-06-03  
**状态**: 草案  
**范围**: core_lib 数据库层全面审计与优化

---

## 一、现状审计总结

### 1.1 当前支持的数据库引擎

| 引擎 | 配置声明 | 能力等级 | 实际实现状态 |
|------|---------|---------|-------------|
| MySQL | `DbType::MySQL` | A | **完整** — 唯一真正可工作的引擎 |
| MariaDB | `DbType::MariaDB` | A | **无实现** — 借用 MySQL pool，未做兼容验证 |
| PostgreSQL | `DbType::PostgreSQL` | A | **完全未实现** — db.rs 仅有 MySqlPool |
| SQLite | `DbType::SQLite` | A | **完全未实现** |
| SQL Server | `DbType::SQLServer` | B | **占位符** — PlaceholderDbAdapter 全返回错误 |
| MongoDB | `DbType::MongoDB` | C | **占位符** |
| Redis | `DbType::Redis` | D | **占位符** |
| Oracle | `DbType::Oracle` | B | **占位符** |

**核心问题**: 配置层声明了 8 种数据库，但 `db.rs` 只有 `MySqlPool`，其余 7 种在前端均可选择连接，连接后全部报错。这对用户是严重的误导。

---

### 1.2 逐模块问题清单

#### db.rs — 连接管理

| 问题 | 严重度 | 描述 |
|------|--------|------|
| 仅支持 MySQL | **P0** | `DbClient` 硬编码 `MySqlPool`，无法创建 PG/SQLite 连接 |
| `test_before_acquire(false)` | **P1** | 默认禁用连接健康检查，可能返回已断开的连接 |
| 连接池参数不可配置 | **P1** | `max_connections(10)`、`acquire_timeout(3s)` 硬编码，无法通过配置文件或环境变量调整 |
| 无连接重试/退避 | **P1** | 连接失败后无自动重试机制 |
| `extract_db_name` 简陋 | **P2** | 仅从 URL 路径取最后一段，未处理 URL 编码或 query 参数 |
| 无 SSH 隧道支持 | **P2** | `DbConnection.ssh` 字段存在但从未使用 |
| 无 SSL 配置支持 | **P2** | `DbConnection.ssl` 字段存在但从未使用 |

#### schema.rs / schema_ext.rs — 结构提取

| 问题 | 严重度 | 描述 |
|------|--------|------|
| 仅 MySQL information_schema | **P1** | 所有 SQL 使用 MySQL 特定的 `information_schema` 语法，PostgreSQL 的 `information_schema` 列名和行为不同 |
| 无视图/存储过程/触发器提取 | **P2** | `get_views()` 存在但未在主流程中使用 |
| `get_tables` 无分页 | **P2** | 大数据库可能有数千张表，一次性加载全部 |

#### db_protocol.rs — 统一抽象层

| 问题 | 严重度 | 描述 |
|------|--------|------|
| PlaceholderDbAdapter 全部返回错误 | **P0** | 定义了 `UnifiedQueryEngine`、`UnifiedMetadataProvider`、`UnifiedImportExport` 三个 trait，但唯一实现 `PlaceholderDbAdapter` 的所有方法都返回 `"Not implemented"` |
| trait 设计过于泛化 | **P2** | `BoxFuture` 动态分发在高频查询路径上有性能开销 |

#### crud.rs — CRUD 操作

| 问题 | 严重度 | 描述 |
|------|--------|------|
| 仅支持 MySQL 语法 | **P1** | 反引号 `` ` `` 标识符引用是 MySQL 特有的，PostgreSQL 使用双引号 `"` |
| 无批量 INSERT | **P1** | `import_data` handler 逐行 INSERT，大量数据时极慢 |
| SQL 值绑定类型不够完整 | **P2** | 不处理 `DateTime`、`Decimal` 等特殊类型，走 `val.to_string()` 兜底可能丢失精度 |

#### mysql_sync.rs — 数据同步引擎

| 问题 | 严重度 | 描述 |
|------|--------|------|
| `row_to_json()` 硬编码 `MySqlRow` | **P1** | 无法用于 PostgreSQL 或 SQLite |
| 主键比较用 `f64` 解析 | **P1** | `compare_pk_str` 对大整数（超过 2^53）会精度丢失 |
| `fetch_rows_in_range` 无 LIMIT | **P1** | 分块查询缺少 `LIMIT` 保护，如果数据量极大可能 OOM |
| 格式化值使用字符串拼接 | **P1** | `generate_statements` 中用 `format_value()` 拼接 SQL，存在 SQL 注入风险 |
| `deploy()` 函数正确使用事务 | **OK** | 这是做得好的地方 |

#### sync.rs / transfer.rs — 结构同步与传输

| 问题 | 严重度 | 描述 |
|------|--------|------|
| `format_value()` 重复实现 3 次 | **P2** | `sync.rs`、`mysql_sync.rs`、`transfer.rs` 各有一份 |
| `DataSyncEngine.data_sync()` 是空壳 | **P1** | `tools.rs:459-480` 只返回注释，不生成实际 SQL |
| transfer 用 `format!()` 拼接 SQL | **P1** | `transfer.rs` 的 `execute_transfer_with_report` 大量使用 `format!()` 而非参数化查询 |
| 大文件全量读入内存 | **P2** | SQL 文件用 `read_to_string` 读取，大文件会 OOM |

#### loadgen.rs — 数据生成器

| 问题 | 严重度 | 描述 |
|------|--------|------|
| 表名未加引号 | **P1** | `table_count()` 中 `SELECT COUNT(*) FROM {}` 无标识符引用 |
| 仅支持 MySQL 语法 | **P1** | `ENGINE=InnoDB`、`DATETIME` 类型等是 MySQL 特有的 |
| 批量 INSERT 无事务包裹 | **P2** | 每个批次单独执行，失败后无法回滚 |

#### error.rs — 错误处理

| 问题 | 严重度 | 描述 |
|------|--------|------|
| `sqlx::Error` → `AppError::InternalError` | **P2** | 丢失了 SQL 错误码、约束名等关键调试信息 |
| 无连接错误 vs 查询错误区分 | **P2** | 前端无法区分 "连接断开" 和 "SQL 语法错误" |
| 敏感信息脱敏做得好 | **OK** | `redact_sensitive()` 函数覆盖了 Bearer token、URL 密码、api_key、password |

#### web-server main.rs — API 层

| 问题 | 严重度 | 描述 |
|------|--------|------|
| `import_data` 逐行 INSERT | **P1** | 每行一个 SQL 执行，1000 行 = 1000 次网络往返 |
| 只读模式未实现 | **P2** | `is_read_only` 配置存在但 API 从未检查 |
| 多 DB 连接切换无连接池管理 | **P1** | `db_client_cache` 概念存在于文档但未见实际实现 |

---

### 1.3 重复代码统计

| 重复代码 | 出现位置 | 说明 |
|----------|----------|------|
| `format_value(&Value) -> String` | sync.rs:557, mysql_sync.rs:557, transfer.rs:140 | 三份几乎相同的 SQL 值格式化 |
| `escape_sql()` / `escape_sql_string()` | loadgen.rs:146, transfer.rs:133 | 两种命名，同一功能 |
| `row_cell_to_value()` / `row_to_json()` | transfer.rs:167, mysql_sync.rs:506 | 两份 MySqlRow → Value 转换 |
| 类型探测序列 (try i64/f64/bool/string/bytes) | transfer.rs:168-195, mysql_sync.rs:511-535 | 重复的逐类型 try-get 逻辑 |

---

## 二、优化方案设计

### 2.1 分期规划

```
Phase 1 (稳定性基础) ─── 连接管理、错误处理、SQL 注入修复
Phase 2 (多引擎支持) ─── PostgreSQL + SQLite 真正实现
Phase 3 (性能优化) ─── 批量操作、连接调优、流式查询
Phase 4 (功能完善) ─── SSH/SSL、只读模式、事务管理增强
Phase 5 (代码质量) ─── 去重、抽象层统一、测试覆盖
```

---

### Phase 1: 稳定性基础

#### 1A. 连接池管理重构

**目标**: 将 `db.rs` 从硬编码 MySQL 改为可配置、支持健康检查的连接管理器。

**设计方案**:

```
新文件: core_lib/src/db_pool.rs

enum DbPool {
    MySQL(sqlx::MySqlPool),
    Postgres(sqlx::PgPool),
    SQLite(sqlx::SqlitePool),
}
```

`DbClient` 改为持有 `DbPool` 枚举，内部按引擎类型分发。连接池参数通过 `AppConfig` 可配置：

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolConfig {
    pub max_connections: u32,       // 默认 10
    pub min_connections: u32,       // 默认 1
    pub acquire_timeout_ms: u64,    // 默认 5000
    pub idle_timeout_ms: u64,      // 默认 600000 (10min)
    pub max_lifetime_ms: u64,      // 默认 1800000 (30min)
    pub test_before_acquire: bool, // 默认 true
}
```

**关键改变**:
- `test_before_acquire` 默认改为 `true`（sqlx 文档推荐生产环境开启）
- 使用 `before_acquire` 回调代替简单的 ping，空闲超过 60 秒才 ping，避免频繁健康检查开销
- 连接参数可通过环境变量 `LOCAL_AI_SQL_POOL_MAX_CONNECTIONS` 等覆盖

**参考**: sqlx 官方文档 `PoolOptions::before_acquire` 推荐模式 — 空闲连接惰性检测，新连接用 `after_connect` 初始化。

#### 1B. SQL 注入防护

**目标**: 消除所有 `format!()` 拼接用户可控数据的 SQL。

**改造清单**:

| 文件 | 当前方式 | 改造方式 |
|------|----------|----------|
| `mysql_sync.rs:fetch_min_max_pk` | `format!("SELECT MIN(\`{pk}\`) ...")` | 表名/列名白名单验证 + `quote_identifier()` |
| `mysql_sync.rs:fetch_pk_list_after` | `format!("SELECT \`{pk}\` ...")` | 同上 |
| `mysql_sync.rs:fetch_rows_in_range` | `format!("SELECT * FROM \`{}\`")` | 同上 |
| `mysql_sync.rs:generate_statements` | `format_value()` 拼接 | 使用 `quote_identifier()` + 参数化值 |
| `transfer.rs:execute_transfer_with_report` | `format!()` 拼接全部 SQL | 引入 `IdentifierQuoter` 工具 |
| `loadgen.rs:table_count` | `format!("SELECT COUNT(*) FROM {}", table)` | 加标识符引用 |

**引入工具函数**:
```rust
// core_lib/src/sql_util.rs (新文件)

/// 标识符引用：MySQL 用反引号，PostgreSQL 用双引号
pub fn quote_ident_mysql(s: &str) -> String {
    format!("`{}`", s.replace('`', "``"))
}

pub fn quote_ident_pg(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// SQL 字符串值转义（参数化查询的后备）
pub fn escape_sql_string(s: &str) -> String {
    s.replace('\'', "''")
}

/// 验证标识符是否只含安全字符
pub fn validate_identifier(s: &str) -> Result<(), String> {
    if s.is_empty() || s.len() > 64 {
        return Err("Identifier length invalid".into());
    }
    if !s.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return Err(format!("Identifier contains illegal chars: {}", s));
    }
    Ok(())
}
```

**原则**: 表名/列名使用标识符引用（不是参数化，因为 SQL 不支持参数化标识符），值使用 sqlx 参数绑定。

#### 1C. 错误处理增强

**目标**: 保留 SQL 错误码信息，区分连接错误 vs 查询错误。

**改造 `AppError`**:
```rust
#[derive(Debug, Error)]
pub enum AppError {
    // 新增：携带 SQL 错误码
    #[error("Database query error: {message} (code: {code:?})")]
    DbQueryError { message: String, code: Option<String>, sql_state: Option<String> },

    // 新增：连接层错误（可重试）
    #[error("Database connection lost: {0}")]
    DbConnectionLost(String),

    // 保留原有变体...
}
```

**改造 `From<sqlx::Error>`**:
```rust
impl From<sqlx::Error> for AppError {
    fn from(err: sqlx::Error) -> Self {
        match &err {
            sqlx::Error::Database(db_err) => {
                // 提取 SQLSTATE 和错误消息
                AppError::DbQueryError {
                    message: db_err.message().to_string(),
                    code: db_err.code().map(|c| c.into_owned()),
                    sql_state: None,
                }
            }
            sqlx::Error::Io(_) | sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed => {
                AppError::DbConnectionLost(err.to_string())
            }
            sqlx::Error::RowNotFound => AppError::NotFound("No rows returned".into()),
            _ => AppError::InternalError(err.to_string()),
        }
    }
}
```

---

### Phase 2: 多引擎支持

#### 2A. PostgreSQL 支持

**新增文件**: `core_lib/src/pg_adapter.rs`

实现 `UnifiedQueryEngine` 和 `UnifiedMetadataProvider`：

```rust
pub struct PgAdapter {
    pool: sqlx::PgPool,
}

impl UnifiedQueryEngine for PgAdapter {
    fn execute(&self, req: UnifiedQueryRequest) -> ... { ... }
}

impl UnifiedMetadataProvider for PgAdapter {
    fn list_databases(&self) -> ... {
        // SELECT datname FROM pg_database WHERE datistemplate = false
    }
    fn list_tables(&self, database: &str) -> ... {
        // SELECT table_name FROM information_schema.tables WHERE table_schema = 'public'
    }
    fn get_table_schema(&self, table) -> ... {
        // PostgreSQL information_schema 查询
    }
}
```

**关键差异处理**:
- 标识符引用: `` ` `` → `"`
- `LIMIT` 语法: MySQL `LIMIT n` = PG `LIMIT n`（兼容）
- `information_schema` 列名: 基本一致，但 `COLUMN_KEY` 在 PG 中用 `constraint_type` 替代
- `AUTO_INCREMENT` → `SERIAL` / `GENERATED ALWAYS AS IDENTITY`
- `ENGINE=InnoDB` 等 MySQL 特有选项需跳过

**修改 `db.rs`**:
```rust
impl DbClient {
    pub async fn new(url: &str, db_type: &DbType) -> Result<Self, DbError> {
        match db_type {
            DbType::MySQL | DbType::MariaDB => {
                // 现有 MySqlPool 逻辑
            }
            DbType::PostgreSQL => {
                let pool = PgPoolOptions::new()
                    .max_connections(10)
                    .connect(url).await?;
                Ok(Self { pool: DbPool::Postgres(pool) })
            }
            DbType::SQLite => {
                let pool = SqlitePoolOptions::new()
                    .connect(url).await?;
                Ok(Self { pool: DbPool::SQLite(pool) })
            }
            _ => Err(DbError::Unsupported(db_type.display_name().into())),
        }
    }
}
```

#### 2B. SQLite 支持

SQLite 特殊性:
- 无连接池概念（单文件，单写多读）
- `information_schema` 不存在，用 `sqlite_master` 和 `pragma_table_info()`
- URL 格式: `sqlite:///path/to/db.sqlite` 或 `sqlite::memory:`

**新增文件**: `core_lib/src/sqlite_adapter.rs`

---

### Phase 3: 性能优化

#### 3A. 批量 INSERT 优化

**当前问题**: `import_data` API 逐行 INSERT，每次一个 SQL 网络往返。

**改造方案**:
```rust
// 改造前：逐行
for row in data {
    sqlx::query(&sql).bind(...).execute(&pool).await?;
}

// 改造后：批量 VALUES
let batch_size = 500;
for chunk in data.chunks(batch_size) {
    let placeholders: Vec<String> = chunk.iter().enumerate()
        .map(|(i, _)| {
            let start = i * col_count;
            let end = start + col_count;
            format!("({})", (start..end).map(|j| format!("${}", j + 1)).collect::<Vec<_>>().join(", "))
        })
        .collect();
    let sql = format!("INSERT INTO {} ({}) VALUES {}",
        table_ident, col_list, placeholders.join(", "));
    let mut query = sqlx::query(&sql);
    for row in chunk {
        for val in row { query = query.bind(val); }
    }
    query.execute(&pool).await?;
}
```

**性能预期**: 1000 行数据从 ~1000ms (1000 次网络往返) 降至 ~10ms (2 次批量)。

**参考**: MySQL 官方推荐 bulk insert，单条 INSERT 语句包含多组 VALUES 可减少 90%+ 的网络开销。

#### 3B. 连接池调优参数

```rust
// 生产环境推荐配置
PoolConfig {
    max_connections: 20,           // 根据 DB max_connections 调整
    min_connections: 2,            // 保持最小热连接
    acquire_timeout_ms: 5000,      // 5秒获取超时
    idle_timeout_ms: 600_000,      // 10分钟空闲回收
    max_lifetime_ms: 1_800_000,    // 30分钟最大生命周期
    test_before_acquire: true,     // 获取前健康检查
}
```

**参考**: HikariCP (Java 连接池黄金标准) 的默认值和 sqlx 文档推荐。sqlx 的 `before_acquire` 回调模式比简单的 `test_before_acquire(true)` 更高效 — 空闲 <60 秒的连接跳过 ping。

#### 3C. 流式查询（大数据量）

**当前问题**: `fetch_rows_in_range` 用 `fetch_all` 一次性加载全部数据到内存。

**改造**: 使用 `fetch` 返回 `Stream`，逐行处理：
```rust
use futures::TryStreamExt;

let mut rows_stream = sqlx::query(&sql).bind(...).fetch(&pool);
while let Some(row) = rows_stream.try_next().await? {
    // 逐行处理，内存占用 O(1)
}
```

**适用场景**: 数据导出、大表同步的 preview 阶段。

#### 3D. `fetch_rows_in_range` 安全限制

**当前问题**: 无 LIMIT 保护，理论上可返回整表数据。

**改造**: 添加强制 LIMIT:
```rust
sql.push_str(&format!(" ORDER BY `{}` LIMIT {}", primary_key, max_rows_per_chunk));
```

默认 `max_rows_per_chunk = 100_000`，可通过环境变量覆盖。

---

### Phase 4: 功能完善

#### 4A. SSH 隧道支持

**方案**: 使用 `ssh2` crate 建立隧道，在连接池 `after_connect` 回调中注入。

```rust
// DbConnection.ssh 已有字段结构（serde_json::Value）
// 需要解析为：
struct SshConfig {
    host: String,
    port: u16,       // 默认 22
    username: String,
    password: Option<String>,
    private_key: Option<String>,
}
```

**实现**: 创建 TCP 隧道 → 绑定本地端口 → 用 `127.0.0.1:local_port` 替代原始 host:port 连接。

**优先级**: P2 — 企业用户需要，但不是核心功能。

#### 4B. SSL/TLS 支持

**方案**: sqlx 已内置 TLS 支持（`tokio-rustls`），只需在连接选项中配置：
```rust
let options = MySqlConnectOptions::from_str(url)?
    .ssl_mode(MySqlSslMode::Required)
    .ssl_ca("/path/to/ca.pem");
```

**优先级**: P2。

#### 4C. 只读模式强制执行

**改造**: 在 `execute` handler 中检查当前连接的 `is_read_only` 标志：
```rust
if conn.is_read_only && is_mutation_sql(&sql) {
    return Err(AppError::Forbidden("Connection is read-only".into()));
}
```

`is_mutation_sql` 检查 SQL 是否以 `INSERT`/`UPDATE`/`DELETE`/`DROP`/`ALTER`/`TRUNCATE`/`CREATE`/`REPLACE` 开头。

#### 4D. 事务管理增强

**当前**: 前端有 `execute_transaction` API，后端支持 begin/commit/rollback。

**增强**:
- 事务超时: 超过 N 秒自动回滚（防止长事务锁表）
- 事务影响行数限制: 超过 N 行需要二次确认
- 死锁检测: 捕获 MySQL error 1213 / PG 40P01，返回友好提示

---

### Phase 5: 代码质量

#### 5A. 消除重复代码

**新建 `core_lib/src/sql_util.rs`**，集中存放:
- `format_sql_value(&Value, DbType) -> String` — 统一值格式化（按引擎类型选择语法）
- `escape_sql_string(&str) -> String` — 统一转义
- `quote_identifier(&str, DbType) -> String` — 统一标识符引用
- `validate_identifier(&str) -> Result<()>` — 标识符合法性检查
- `row_to_json_value(row, columns) -> Value` — 统一行转换（泛型化）

**删除**: `sync.rs`、`mysql_sync.rs`、`transfer.rs` 中各自的重复实现。

#### 5B. db_protocol.rs 完善

将 `PlaceholderDbAdapter` 替换为真正的引擎适配器：
- `MySqlAdapter` — 包装现有 MySQL 逻辑
- `PgAdapter` — Phase 2 实现
- `SqliteAdapter` — Phase 2 实现

`DbClient` 持有 `Box<dyn UnifiedQueryEngine>`，API handler 通过 trait 统一调用。

#### 5C. 测试覆盖

**当前测试状态**:
- `core_lib/tests/timeout_policy_test.rs` — 有
- `config.rs` 单元测试 — 有
- `error.rs` 单元测试 — 有
- `perf_report.rs` 单元测试 — 有
- **缺失**: `db.rs`、`schema.rs`、`crud.rs`、`mysql_sync.rs`、`sync.rs`、`transfer.rs` 均无单元测试

**补测优先级**:
1. `sql_util.rs` — 纯函数，易测试
2. `crud.rs` — 需要 MySQL/PG 测试容器
3. `mysql_sync.rs` — `compare`、`preview`、`generate_statements` 逻辑测试
4. `transfer.rs` — CSV 解析、SQL 生成测试

---

## 三、技术参考依据

### sqlx 连接池最佳实践

来源: sqlx 官方文档 (docs.rs/sqlx)

1. **`test_before_acquire` 默认 `true`** — sqlx 文档指出默认开启，建议生产环境保持。当前项目设为 `false` 是反模式。
2. **`before_acquire` 惰性检查** — 推荐模式: 空闲 <60s 跳过 ping，>60s 执行 ping。避免高频 ping 开销。
3. **`max_connections`** — 应设置为数据库 server 的 `max_connections` 的 50%-75%，避免耗尽服务端连接。
4. **`max_lifetime`** — 30 分钟是常见默认值，防止 MySQL `wait_timeout` 断开连接（MySQL 默认 8 小时）。

### SQL 注入防护

来源: OWASP SQL Injection Prevention Cheat Sheet

1. **参数化查询（首选）** — sqlx 的 `query().bind()` 已实现参数化，用于值绑定。
2. **标识符白名单** — 表名/列名无法参数化，必须用正则白名单 `[a-zA-Z0-9_]+` 验证。
3. **标识符引用** — MySQL 用反引号，PG 用双引号，SQLite 用双引号。
4. **永远不要用 `format!()` 拼接用户输入到 SQL 中**。

### 批量 INSERT 性能

来源: MySQL 官方文档 "INSERT Statement" + PostgreSQL "Populating a Database"

1. **多行 VALUES** — 单条 INSERT 包含多组 VALUES 比逐条 INSERT 快 10-100x（减少网络往返和事务开销）。
2. **禁用自动提交** — 批量操作应包裹在单个事务中。
3. **`LOAD DATA INFILE`** — MySQL 最快的批量导入方式，但需要文件权限。
4. **PostgreSQL `COPY`** — PG 的等效高速导入命令。

### 连接池配置参考

来源: HikariCP (业界最广泛使用的 Java 连接池) 配置指南

| 参数 | HikariCP 默认 | 本项目当前 | 建议值 |
|------|--------------|-----------|--------|
| maxPoolSize | 10 | 10 (硬编码) | 20 (可配置) |
| minIdle | same as max | 1 | 2 |
| connectionTimeout | 30s | 3s | 5s |
| idleTimeout | 10min | 10min | 10min |
| maxLifetime | 30min | 30min | 30min |
| connectionTestQuery | 空 (用 JDBC4 isValid) | 禁用 | `SELECT 1` / ping |

---

## 四、迁移路径与兼容性

### 向后兼容策略

1. `AppConfig` 新增 `pool_config: Option<PoolConfig>`，缺失时使用默认值
2. `DbClient::new()` 签名从 `new(url: &str)` 改为 `new(url: &str, db_type: &DbType)`，调用方需传入类型
3. `db_url` 字段保留，作为 MySQL 默认回退
4. API 响应格式不变，仅新增错误码字段

### 不破坏现有功能的改造顺序

1. **先添加 `sql_util.rs`** — 纯新增，不影响现有代码
2. **再重构 `db.rs`** — 保持 API 签名兼容，内部改用枚举
3. **逐步替换** `format!()` 调用为 `sql_util` 函数
4. **最后实现 PG/SQLite** — 新增适配器，不影响 MySQL 路径

---

## 五、风险评估

| 风险 | 影响 | 缓解措施 |
|------|------|----------|
| DbPool 枚举破坏现有 MySqlPool 调用链 | 高 | 逐步迁移，先保持 `DbClient` 对外 API 不变 |
| PG 信息模式语法差异导致 schema 提取失败 | 中 | 为每种引擎写独立的 schema 提取 SQL |
| 批量 INSERT 的 SQL 长度超过 `max_allowed_packet` | 中 | 动态计算批次大小，按行估算字节数 |
| SSH 隧道引入新的依赖和安全考量 | 低 | Phase 4 实现，用 feature flag 控制编译 |
| 标识符白名单可能漏掉合法但含特殊字符的列名 | 低 | 白名单验证失败时回退到标识符引用（带转义） |
