# PG 字段治理 S2：写路径（分批回填 + V1–V4 校验）

状态：待评审
范围：PostgreSQL 写路径；MySQL/MariaDB 行为不变；能力位不变（`engines` 仍只有 mysql/mariadb）。
依赖：S1 已提交（`a1d51a4`、`d91c7c3`），`field_migration_pg` 真库全绿。

---

## 1. 目标

1. 新增 PG 分批回填执行器：主键 keyset 分页 + `INSERT ... SELECT ... ON CONFLICT DO UPDATE`。
2. `last_pk` 类型化绑定（bigint/numeric/text/uuid），文本主键统一 `COLLATE "C"`。
3. 取连接按引擎分派，`execute_create` 同步改造。
4. 事务语义：默认每批一事务、失败只回滚当前批、可续跑；显式单事务模式才整任务原子。
5. checkpoint 增加 `engine` / `pk_kind` / `target_fingerprint`，续跑拒绝跨引擎复用。
6. V1–V4 校验：同库快照 + 类型规范化比较。
7. 事务内 `SET LOCAL` 超时；等待/限速逻辑复用。
8. 重写类操作（改类型、SET NOT NULL、改 collation）仍只出脚本；`CREATE INDEX CONCURRENTLY` 不进事务。

## 2. 非范围

- 不改 `engines` 能力位，不加 PG 入口。
- 不支持复合主键、分区表、源表触发器（沿用 `prepare` 的 BLOCK）。
- 不执行改类型 / SET NOT NULL / 改 collation。
- 不支持跨数据库实例的源/目标（单连接模型）；该分支只保留 `validation_mode=per_statement` 字段语义。
- 不改 MySQL 回填循环与 SQL 生成。

## 3. 数据模型改动

文件：`core_lib/src/ops/field_migration/migration_store.rs`

```rust
pub enum PkKind { Integer, Numeric, Text, Uuid } // 新增 Numeric/Uuid

pub struct BackfillConfig {
    // 既有字段不变
    #[serde(default)]
    pub single_transaction: bool, // 默认 false
}

pub struct BackfillSpec {
    // 既有字段不变
    #[serde(default)]
    pub schema: Option<String>,          // PG 限定名；MySQL 为 None
    #[serde(default)]
    pub source_data_type: String,        // 源列 data_type，用于重建 PG 表达式
}

pub struct Checkpoint {
    // 既有字段不变
    #[serde(default = "default_engine_mysql")]
    pub engine: String,                  // 旧文件缺省 = "mysql"
    #[serde(default)]
    pub pk_kind: Option<PkKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_fingerprint: Option<String>,
}

pub struct BatchVerify {
    // 既有字段不变
    #[serde(default)]
    pub validation_mode: String,         // PG = "snapshot"；MySQL 保持空串
}

pub struct BackfillExecution {
    pub atomic: bool,                    // 默认 false
    pub rollback_scope: String,          // 默认 "batch"
    pub resumable: bool,                 // 默认 true
    pub validation_mode: String,         // 默认 "per_statement"，PG 同库写 "snapshot"
}

pub struct MigrationJob {
    // 既有字段不变
    #[serde(default = "default_engine_mysql")]
    pub engine: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<BackfillExecution>, // 仅 PG 任务填写；MySQL 保持 None
}
```

- `default_engine_mysql()` 返回 `"mysql"`：老 checkpoint/任务都是 MySQL，续跑不受影响；PG 续跑看到 `"mysql"` 即拒绝。
- 新建任务在 `new_job` 中 `engine = ""`，首次运行由 `ctx` 绑定为 `mysql` / `mariadb` / `postgresql`；绑定后不再变。
- MySQL 任务的 JSON 不新增字段（`execution=None` 不序列化），避免前端/快照断言漂移。

## 4. `prepare` 改造（`core_lib/src/ops/field_migration/backfill.rs`）

- 计算 `is_pg = analysis.environment.engine_family == "postgresql"`。
- PG 跳过 InnoDB 引擎校验；其余 BLOCK 条件（分区、单列主键、只读、行数/字节上限、gaps、触发器、`empty_string_policy`）不变。
- `single_transaction && !is_pg` → `OpsError::Validation("单事务模式仅支持 PostgreSQL")`。
- 生成 `BackfillSpec` 时补 `schema = analysis.environment.schema`、`source_data_type = 源列 profile.data_type`。
- `PreparedBackfill` 增加 `execution: Option<BackfillExecution>`：
  - PG：`atomic = config.single_transaction`，`rollback_scope = single ? "job" : "batch"`，`resumable = !single`，`validation_mode = "snapshot"`。
  - MySQL：`None`。
- `new_job` 把 `execution` 写入任务。

## 5. PG 回填执行器（新文件 `core_lib/src/ops/field_migration/pg_backfill.rs`）

`backfill::run_job` 在进入 MySQL `run_inner` 前按引擎分派：

```rust
let result = if ctx.is_postgres() {
    pg_backfill::run_inner(&job_id, &ctx, &hub, &policy).await
} else {
    run_inner(&job_id, &ctx, &hub, &policy).await
};
```

### 5.1 预检（写前，任一失败 → Blocked，不写目标）

1. `job.engine` 为空 → 绑定当前引擎；非空且不等 → `Blocked(phase="preflight:engine_mismatch")`。
2. `checkpoint.engine` 非空（旧文件默认 `"mysql"`）且不等 → 同上；`checkpoint.pk_kind` 与探测值不等 → `Blocked(phase="preflight:pk_kind_mismatch")`。
3. `job.execution.resumable == false` 且有历史进度 → `Blocked(phase="preflight:not_resumable")`。
4. schema：`spec.schema` 缺省用 `current_schema()`；用 PG 版快照加载器按 schema 读取源/目标列。
5. 源表不得有触发器、不得分区；目标不存在时执行 L2 `CREATE TABLE IF NOT EXISTS`（`backfill::execute_create` 按引擎取连接）。
6. 目标表必须存在单列（`pk`）主键或唯一索引，供 `ON CONFLICT (pk)` 使用；否则 `Blocked(phase="preflight:on_conflict_target")`。
7. 文本主键：`pg_collation.collisdeterministic = false` → `Blocked(phase="preflight:pk_collation")`。
8. 目标非空且非本任务创建且无 checkpoint → `Blocked(phase="preflight:target_not_empty")`（与 MySQL 一致）。
9. 绑定 `target_fingerprint = target_schema_hash`；续跑时目标结构哈希不一致 → Blocked。

### 5.2 主键类型与绑定

`pk_kind_from_pg_type`（`information_schema.columns.data_type` / `format_type`）：

| PG 类型 | PkKind | 绑定写法 |
| --- | --- | --- |
| smallint/integer/bigint | Integer | `$1::bigint`，bind `i64` |
| numeric/decimal | Numeric | `$1::numeric`，bind `BigDecimal` |
| text/varchar/bpchar/name | Text | `$1::text COLLATE "C"`，bind `&str` |
| uuid | Uuid | `$1::uuid`，bind `uuid::Uuid` |
| 其他 | — | `Blocked(phase="preflight:pk_type")` |

- 文本主键：分页比较与 `ORDER BY` 都用 `COLLATE "C"`；冲突目标 `ON CONFLICT (pk)` 不带 collation。
- checkpoint 的 `last_pk` 保存文本形式（uuid 用规范小写）；续跑时重新解析并绑定。
- 所有读取（取主键页、范围计数、差异分页）也走参数绑定，不再拼字面量。

### 5.3 单批 SQL

```sql
INSERT INTO <schema>.<target> (<pk>, <cols...>)
SELECT <pk>, <pg_scripts::value_expr...>
FROM <schema>.<source>
WHERE <pk> > $1::<kind> [COLLATE "C"] AND <pk> <= $2::<kind> [COLLATE "C"]
ORDER BY <pk> [COLLATE "C"]
LIMIT $3
ON CONFLICT (<pk>) DO UPDATE SET c = EXCLUDED.c, ...
```

- 首批无下界：`WHERE pk <= $1`，`LIMIT $2`。
- `LIMIT` 用绑定参数（`i64`），不拼数字字面量。
- 执行前 `SET LOCAL`（每批事务内）：

```sql
SET LOCAL lock_timeout = '<lock_wait_timeout_secs>s';
SET LOCAL statement_timeout = '0';                    -- 不限制长批，交由 lock_timeout/空闲超时
SET LOCAL idle_in_transaction_session_timeout = '60s';
```

## 6. 事务与失败语义

- 默认（`single_transaction=false`）：
  - 每批 `BEGIN` → `SET LOCAL` → `INSERT ... SELECT` → `POST 校验` → `COMMIT`；备份/审计/checkpoint 在提交后写。
  - 失败：`ROLLBACK` 当前批，`last_error` 写入 `batch=<n> range=(<last_pk>, <end_pk>] written=<已提交行数>`；任务置 `Interrupted`，checkpoint 停在上一批，可续跑。
  - `job.execution = { atomic: false, rollback_scope: "batch", resumable: true, validation_mode: "snapshot" }`。
- `single_transaction=true`：
  - 一次 `BEGIN ISOLATION LEVEL REPEATABLE READ`，所有批在同一连接执行；批间校验在同一事务连接上执行（能看到未提交写入），不另开快照。
  - 中途失败：`ROLLBACK` 全部；不写 checkpoint；任务置 `Interrupted`，`last_error` 含失败批次与已完成行数；`{ atomic: true, rollback_scope: "job", resumable: false }`。
  - 成功：`COMMIT` 后再写 checkpoint（`completed_batches=total`、`written_rows=total`），随后用新快照做最终校验。
- 续跑时 `checkpoint.last_pk` 之后继续；已完成批次只做只读复核，不重写。

## 7. V1–V4 校验（新文件 `core_lib/src/ops/field_migration/pg_verify.rs`）

### 7.1 快照

- `PgSnapshot::begin(ctx)`：独立连接执行
  ```sql
  BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY
  ```
  V1/V2/V3/V4 在同一连接、同一事务内执行，结束后 `COMMIT`。
- `validation_mode = "snapshot"` 写入 `BatchVerify` 与 `job.execution`。
- `PgSnapshot::settings()` 返回 `SHOW transaction_isolation` 与 `current_setting('transaction_read_only')`，供测试断言。
- 单事务回填模式下，`verify_with_conn(&mut conn, ...)` 复用写事务连接，不新开事务。

### 7.2 规范化比较（方案第 4 节）

| 类型 | 源表达式 | 目标表达式 |
| --- | --- | --- |
| numeric | `CAST(<raw> AS numeric)` | `CAST(t.c AS numeric)` |
| bytea | `encode(<raw>, 'hex')` | `encode(t.c, 'hex')` |
| timestamptz | `CAST(<raw> AS timestamptz) AT TIME ZONE 'UTC'` | `t.c AT TIME ZONE 'UTC'` |
| json/jsonb | `CAST(<src> AS jsonb) [-> 'k']` | `CAST(t.c AS jsonb)` |
| uuid | `CAST(<raw> AS uuid)` | `t.c` |
| text | `CAST(<raw> AS text)` | `CAST(t.c AS text)` |

- 全部用 `IS DISTINCT FROM`：`NULL` 与 `''` 不合并，`NULL` 与 `NULL` 不算差异。
- `numeric` 不经过 f64；`1` / `1.0` / `1e3` 按数值相等。
- `jsonb` 归一化键顺序与数字表示；`json` 先 `::jsonb`。
- V1：源/目标行数（可按主键范围）；V2：源有目标无；V3：目标有源无；V4：两侧都有但不一致。
- 差异分页：`ORDER BY pk [COLLATE "C"] LIMIT $n`，主键值按类型绑定。

## 8. 事务内超时与限速

- `pg_timeout_statements(policy)` 固定产出 3 条 `SET LOCAL`（见 5.3），单测断言映射值。
- 批间等待、`max_rows_per_second` 限速直接调用 `backfill` 中现有函数（改为 `pub(crate)` 复用）。
- 控制信号（pause/abort）、审计、`KILL QUERY` 语义沿用现有逻辑；PG 只做 `idle_in_transaction_session_timeout`，不实现 `pg_cancel_backend`。

## 9. 脚本/连接改造

- `backfill.rs`：`mysql_pool()` 直连改为 `execute_create` 内部按引擎分派（`ctx.is_postgres()` → `db_client.pg_pool()`，其余保持 `mysql_pool()`）；MySQL SQL 与事务不变。
- `pg_scripts.rs`：
  - `value_expr` / `normalized_source_expr` 改 `pub(crate)` 供执行器复用。
  - `cast_value` 增加 uuid → `CAST(... AS uuid)`。
  - 校验表达式的 json 目标统一 `CAST(t.c AS jsonb)`；bytea 走 `encode(...,'hex')`；numeric 不用 f64；timestamptz 归一 UTC。
- `mod.rs` 注册 `pg_backfill`、`pg_verify`。

## 10. API 改动

`web-server/src/ops_handlers.rs`：

```rust
pub struct FieldBackfillOptions {
    // 既有字段不变
    #[serde(default)]
    pub single_transaction: Option<bool>,
}
```

`config()` 写入 `single_transaction: self.single_transaction.unwrap_or(false)`。

## 11. 测试

### 11.1 `core_lib/tests/field_migration_backfill_pg.rs`

环境：`FIELD_MIGRATION_PG_URL`；每个用例在 `phase3-pg` 建独立临时库，结束 `DROP DATABASE ... WITH (FORCE)`。

| 用例 | 断言 |
| --- | --- |
| `backfill_batches_commit_and_resume` | 10 万行、批 1 万；中断后目标行数整除批大小；续跑完成；checkpoint `engine=postgresql`、`pk_kind`、`target_fingerprint` 存在 |
| `batch_failure_rolls_back_only_current_batch` | 目标 CHECK 让第 3 批失败；目标只剩 2 批、checkpoint=2、`last_error` 含 batch/主键区间/已完成行数；修复约束后续跑完成 |
| `engine_mismatch_refuses_resume` | 手工把 checkpoint.engine 改为 `mysql`；任务 `Blocked(phase=preflight:engine_mismatch)`，目标 0 行 |
| `missing_unique_index_blocks_before_write` | 目标无主键/唯一索引；`Blocked(phase=preflight:on_conflict_target)`，目标 0 行 |
| `unique_index_suffices_for_on_conflict` | 目标仅 `UNIQUE(id)`；任务完成 |
| `insert_select_order_by_limit_takes_lowest_keys` | 乱序插入；首批目标主键 = 最小 N 个 |
| `text_pk_uses_c_collation` | 文本主键混合大小写；完成且无重复/遗漏；`build_batch_sql` 含 `COLLATE "C"` |
| `numeric_pk_pagination` | `numeric` 主键（含小数）完成 |
| `uuid_pk_pagination` | `uuid` 主键完成 |
| `rate_limit_and_timeout_mapping` | `pg_timeout_statements` 含 3 条 `SET LOCAL` 且值来自 policy；限速下耗时 ≥ 预期 |
| `single_transaction_failure_rolls_back_job` | 第 3 批失败 → 目标 0 行、无 checkpoint、`atomic=true/rollback_scope=job/resumable=false`；续跑 `Blocked(preflight:not_resumable)` |

### 11.2 `core_lib/tests/field_migration_verify_pg.rs`

| 用例 | 断言 |
| --- | --- |
| `numeric_normalization` | `1` / `1.0` / `1e3` 与目标 `1000` 无差异；`NaN` 自等；`1e3` vs `999` 有差异 |
| `bytea_hex_compare` | 相同字节无差异，不同字节有差异 |
| `timestamptz_cross_timezone` | 同一时刻的不同时区写法无差异；不同时刻有差异 |
| `json_vs_jsonb` | 键序/数字格式不同的等价 JSON 无差异 |
| `null_vs_empty_string` | `NULL` 与 `''` 判为差异 |
| `uuid_compare` | 文本 `uuid` 与目标 `uuid` 无差异；不同 uuid 有差异 |
| `empty_tables` | V1=0/0、V2/V3/V4=0，`passed()` |
| `snapshot_consistency` | 快照内两次计数一致；`SHOW transaction_isolation = repeatable read`、`transaction_read_only = on`；`validation_mode = "snapshot"` |
| `diff_pages` | V2/V3/V4 总数与分页主键正确、无重复 |

两个测试文件在 env 缺失时打印原因并返回（与 S1 一致），不 `#[ignore]`。

### 11.3 守卫

`scripts/test-summary.py` 的 `CORE_LIB_TARGET_GUARDS` 增加：

```python
"field_migration_backfill_pg": ("core_lib/field_migration_backfill_pg", ["FIELD_MIGRATION_PG_URL"]),
"field_migration_verify_pg": ("core_lib/field_migration_verify_pg", ["FIELD_MIGRATION_PG_URL"]),
```

## 12. 验证步骤

1. `cargo fmt --all -- --check`、`cargo check --workspace`。
2. 真库回填（节点 1，报数）：
   ```bash
   FIELD_MIGRATION_PG_URL='postgres://postgres:password@127.0.0.1:55432/postgres' \
     cargo test -p core_lib --test field_migration_backfill_pg -- --test-threads=1 --no-fail-fast
   ```
3. 真库校验（节点 2，报数）：
   ```bash
   FIELD_MIGRATION_PG_URL='postgres://postgres:password@127.0.0.1:55432/postgres' \
     cargo test -p core_lib --test field_migration_verify_pg -- --test-threads=1 --no-fail-fast
   ```
4. 三套口径（带库均加 `--test-threads=1 --no-fail-fast`）：
   - 无库：不设 `FIELD_MIGRATION_*`（MySQL/PG 相关 target 计为空跑）。
   - MySQL + PG：`FIELD_MIGRATION_MYSQL_URL=mysql://...:33306`、`FIELD_MIGRATION_PG_URL=...:55432`。
   - MariaDB + PG：`FIELD_MIGRATION_MYSQL_URL=...:33307`、`FIELD_MIGRATION_PG_URL=...:55432`。
   - 每次 `python3 scripts/test-summary.py <log>` 用相同 env 解析。
5. 本地提交，不 push。

## 13. 已定决策（无待用户拍板项）

| # | 决策 | 理由 |
| --- | --- | --- |
| 1 | 旧 checkpoint `engine` 缺省 `"mysql"` | 旧文件全部来自 MySQL；既保 MySQL 续跑，又禁止 PG 复用 |
| 2 | `atomic/rollback_scope/resumable` 放 `job.execution`，MySQL 为 `None` | 不污染 MySQL 任务 JSON，不套用 MySQL 语义 |
| 3 | `single_transaction` 仅 PG 可用，MySQL 显式传 true 报 Validation | 新选项，不影响既有 MySQL 行为 |
| 4 | 文本主键一律 `COLLATE "C"`；非确定性排序规则预检 BLOCK | 与 S1 风险判定一致，不静默换序 |
| 5 | 同库校验一律单连接 `REPEATABLE READ READ ONLY` 快照 | 当前执行器单连接模型；跨库只保留 `per_statement` 字段 |
| 6 | 类型绑定用 BigDecimal/Uuid 原生类型（sqlx 已启用 bigdecimal/uuid） | 满足 `$1::numeric` / `$1::uuid`，不拼字面量 |
| 7 | 主键仅支持整型/numeric/text/uuid，其他类型 BLOCK | 排序与绑定确定性；边界进 S3 |
| 8 | 校验表达式与 `pg_scripts` 共用生成器，json 目标补 `::jsonb`、uuid 补 `CAST` | 执行与脚本同口径 |

## 14. S3 边界清单

1. 改类型、`SET NOT NULL`（`CHECK NOT VALID` → `VALIDATE`）、改 collation：仍只出脚本。
2. `CREATE INDEX CONCURRENTLY` 不进事务，S3 决定独立执行器。
3. 跨数据库实例的源/目标校验快照。
4. 复合主键、分区表、表达式/部分唯一索引作为冲突目标。
5. 非 btree 主键、`citext`/非确定性排序规则下的文本主键。
6. 目标结构漂移的细粒度指纹（当前仅表级哈希）。
7. PG 取消查询（`pg_cancel_backend`）与进程重启后的连接清理。
8. 大表流式读取/写入（当前 `fetch_all` + `LIMIT`）。
9. 能力位 `engines` 增加 postgresql 与前端入口。
10. 多任务并发下的限速公平性与全局配额。
