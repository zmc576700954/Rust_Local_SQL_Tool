# PG 字段治理 S3：SET NOT NULL 可执行化 + CREATE INDEX CONCURRENTLY + 能力位开放

状态：已实现（2026-11-11，本地提交）
范围：PostgreSQL 专用执行器；MySQL/MariaDB 行为不变（不改其 SQL、事务、任务 JSON、脚本包）。
依赖：S2 已提交（`c422cbb`），`field_migration_backfill_pg`（11）与 `field_migration_verify_pg`（9）真库全绿。
范围确认：2026-11-11 用户确认「SET NOT NULL 可执行化、CREATE INDEX CONCURRENTLY 独立执行器、能力位 engines 增加 postgresql + 入口打通」三项纳入；改类型/改 collation 维持只出脚本；复合主键、分区表、非确定性排序规则文本主键、跨实例快照排除。

---

## 1. 目标

1. `SET NOT NULL` 从「仅脚本」升级为可执行：`CHECK NOT VALID` → 分批 NULL 扫描 → `VALIDATE CONSTRAINT` → `SET NOT NULL` → `DROP CHECK`，全程可断点续跑。
2. `CREATE INDEX CONCURRENTLY` 独立执行器：不进事务，失败自动清理残留 `INVALID` 索引，可重跑。
3. 字段治理能力位 `engines` 增加 `postgresql`，后端入口对 PG 连接开放。
4. 两个执行器复用 S2 既有机制：keyset 分页、类型化绑定、`SET LOCAL`/session 超时、限速、控制信号（pause/abort）、审计、checkpoint 引擎绑定。

## 2. 非范围

- 不执行改类型、改 collation（维持 S1/S2 的仅脚本输出）。
- 不支持复合主键、分区表（preflight BLOCK）、表达式/部分索引、`INCLUDE`、opclass。
- 不支持 `citext`/非确定性排序规则下的文本主键（沿用 S2 preflight BLOCK）。
- 不动 MySQL/MariaDB 任何路径；不改 `MigrationJob` 结构。
- 不做前端按钮/页面（FieldGovernance.tsx 无引擎门控，能力位打通即入口可见；UI 表单留待后续）。
- 不实现 `pg_cancel_backend`；进程重启后的孤儿连接清理维持 S3 边界外。

## 3. 数据模型改动

文件：`core_lib/src/ops/field_migration/migration_store.rs`

`MigrationStore` 增加两个 job 映射，随同一 store 文件持久化：

```rust
pub struct MigrationStore {
    jobs: HashMap<String, MigrationJob>,          // 既有，不变
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    notnull_jobs: HashMap<String, NotNullJob>,    // 新增
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    index_jobs: HashMap<String, IndexJob>,        // 新增
    // path 不变
}
```

- `skip_serializing_if = "HashMap::is_empty"`：无 PG 任务时存量 store 文件字节不变（MySQL 用户零漂移）；老文件缺字段反序列化为空映射。
- 两类新 job 复用 `MigrationHub` 的 controls/active（pause/abort/并发去重），不新建持久化机制。

### 3.1 NotNullJob

```rust
pub struct NotNullJob {
    pub id: String,
    pub created_at / updated_at: String,
    pub status: MigrationStatus,             // 复用既有枚举
    pub blocking: Option<BlockingDetail>,    // 复用；phase 见 4.1/4.2
    #[serde(default = "default_engine_mysql")]
    pub engine: String,                      // 续跑引擎校验，同 S2
    pub schema: Option<String>,
    pub table: String,
    pub column: String,
    pub constraint_name: String,             // fm_not_null_<column>
    pub spec_hash: String,                   // 表/列/约束名指纹，防串任务
    pub phase: NotNullPhase,                 // 状态机当前阶段
    pub scan: NotNullScanProgress,           // 扫描游标与累计
    pub config: BackfillConfig,              // 扫描批大小/间隔/限速（实现期补充，复用 S2 限速）
    pub execution: NotNullExecution,         // { atomic:false, rollback_scope:"phase", resumable:true }
    pub last_error: Option<String>,
}

pub enum NotNullPhase {
    Preflight, Scan, AddCheck, Validate, SetNotNull, DropCheck, Verify, Done,
}

pub struct NotNullScanProgress {
    pub last_pk: Option<String>,             // 文本形式，同 S2 checkpoint
    pub scanned_rows: u64,
    pub null_count: u64,
    pub null_samples: Vec<String>,           // 主键样本，上限 100
    pub pk_kind: Option<PkKind>,
}
```

### 3.2 IndexJob

```rust
pub struct IndexJob {
    pub id: String,
    pub created_at / updated_at: String,
    pub status: MigrationStatus,
    pub blocking: Option<BlockingDetail>,
    #[serde(default = "default_engine_mysql")]
    pub engine: String,
    pub spec: IndexSpec,
    pub spec_hash: String,
    pub phase: IndexPhase,                   // preflight/building/verifying/done（实现期补充，便于报进度）
    pub progress: Option<IndexProgress>,     // pg_stat_progress_create_index 快照
    pub last_error: Option<String>,
}

pub struct IndexSpec {
    pub schema: Option<String>,
    pub table: String,
    pub index_name: String,
    pub columns: Vec<String>,                // 仅普通列，有序
    pub unique: bool,
}
```

## 4. SET NOT NULL 执行器（新文件 `core_lib/src/ops/field_migration/pg_notnull.rs`）

### 4.1 阶段机

`Preflight → Scan → AddCheck → Validate → SetNotNull → DropCheck → Verify → Done`

每阶段入口幂等，续跑从 `job.phase` 继续；除 Scan 外每阶段是单条 DDL、自动提交（不开事务块）。

### 4.2 Preflight（任一失败 → Blocked，不写库）

1. `engine` 校验：空则绑定当前引擎；非空且非 `postgresql` → `Blocked(preflight:engine_mismatch)`。
2. 表/列存在；列 `is_nullable = 'YES'`，否则直接 `Completed`（no-op，notes 记录"已是 NOT NULL"）。
3. 分区表（`pg_partitioned_table` 命中）→ `Blocked(preflight:partitioned)`。
4. 单列主键存在且 `PkKind ∈ {Integer, Numeric, Text, Uuid}`（扫描分页需要）；文本主键非确定性排序规则 → `Blocked(preflight:pk_collation)`（同 S2）。
5. 主版本 `SHOW server_version_num` < 120000 → `Blocked(preflight:pg_version)`（PG 12 起 `SET NOT NULL` 才能利用已验证 CHECK 跳过全表扫描）。
6. 残留约束探测：`pg_constraint` 查 `fm_not_null_<column>`：
   - 不存在 → 从 `Scan` 开始；
   - 存在且 `NOT VALID` → 跳到 `Validate`；
   - 存在且已 validated 且列仍 nullable → 跳到 `SetNotNull`。
7. `spec_hash` 绑定：续跑时表/列/约束名任一变化 → `Blocked(preflight:spec_mismatch)`。

### 4.3 Scan（只读分批，可续跑）

```sql
SELECT <pk> FROM <schema>.<table>
WHERE <column> IS NULL AND <pk> > $1::<kind> [COLLATE "C"]
ORDER BY <pk> [COLLATE "C"]
LIMIT $2
```

- 每批短事务 + S2 同款 `SET LOCAL` 三条；复用 `wait_rate_limit` / 控制信号。
- `scanned_rows` 用每批 `count(*)` 口径累计（扫过的行数，含非 NULL）。
- 发现 NULL：`null_count += 批内数量`，主键样本追加（上限 100，超出只计数）。
- 扫完且 `null_count > 0` → `Blocked(scan:null_rows)`，`blocking.reason` 含 `null_count` 与样本；**再次运行从头重扫**（数据已被用户修复，旧游标无意义；phase 重置为 Scan、`last_pk=None`）。
- 扫完且 `null_count = 0` → 进入 AddCheck。

### 4.4 DDL 阶段（单语句自动提交，不进事务块）

执行前 session 级 `SET lock_timeout = '<policy>s'`、`SET statement_timeout = '0'`，执行后 `RESET`。

| 阶段 | SQL | 失败语义 |
| --- | --- | --- |
| AddCheck | `ALTER TABLE t ADD CONSTRAINT c CHECK (col IS NOT NULL) NOT VALID` | 已存在则跳过（按 4.2.6 探测结果直接跳转，正常路径不会重复执行） |
| Validate | `ALTER TABLE t VALIDATE CONSTRAINT c` | 违例（扫描后新写入 NULL）→ `Blocked(validate:violations)`，约束保留 NOT VALID；续跑重回 `Scan` 重扫 |
| SetNotNull | `ALTER TABLE t ALTER COLUMN col SET NOT NULL` | 有已验证 CHECK，PG ≥ 12 不扫表，短 ACCESS EXCLUSIVE |
| DropCheck | `ALTER TABLE t DROP CONSTRAINT c` | 不存在则跳过（幂等） |

中断残留说明：进程在 AddCheck 后被杀，库里残留 `fm_not_null_<column>` NOT VALID 约束——无害（不约束新写入的语义变化，NOT VALID 只影响校验）；job 续跑自愈；文档给出手工清理 SQL（`ALTER TABLE ... DROP CONSTRAINT ...`）。

### 4.5 Verify

`information_schema.columns.is_nullable = 'NO'` 且 `pg_attribute.attnotnull = true` 且残留约束已删除 → `Completed`；审计写执行摘要（阶段耗时、扫描行数、锁等待）。

`execution = { atomic: false, rollback_scope: "phase", resumable: true }`：不回滚已完成 DDL 阶段（逐阶段幂等续跑即"回滚"语义）。

## 5. CREATE INDEX CONCURRENTLY 执行器（新文件 `core_lib/src/ops/field_migration/pg_index.rs`）

### 5.1 Preflight

1. `engine` 校验同 4.2.1。
2. 表存在、非分区 → 分区 `Blocked(preflight:partitioned)`（PG 不支持对分区表 CONCURRENTLY）。
3. `columns` 全部存在；空列表 → Validation。
4. `index_name` 长度 ≤ 63 字节 → 超长 `Blocked(preflight:name_too_long)`，不静默截断。
5. 同名索引探测（`pg_class`/`pg_index`）：
   - 存在且 `indisvalid AND indisready` → `Completed`（no-op）；
   - 存在但 `NOT indisvalid OR NOT indisready`（上次失败残留）→ 先 `DROP INDEX CONCURRENTLY` 清理再继续；
   - 同名对象是约束/非索引 → `Blocked(preflight:name_conflict)`。
6. `spec_hash` 绑定，续跑校验一致。

### 5.2 执行

```sql
CREATE INDEX [UNIQUE] CONCURRENTLY <name> ON <schema>.<table> (<cols...>)
```

- 独立连接、单语句自动提交；**严禁放进事务块**（PG 会报错，代码上不走任何 BEGIN）。
- session 级 `SET lock_timeout`（policy 值）、`SET statement_timeout = '0'`。
- 执行期间每 5s best-effort 轮询 `pg_stat_progress_create_index`（phase、blocks_done/total、tuples_done/total）写入 `job.progress`；查不到/权限不足不阻塞主流程。
- unique 违例：build 报错 → 清理残留 INVALID 索引 → `Blocked(build:unique_violation)`；**不做重复值预检**（避免两次全表扫）。
- 其他失败：清理残留 INVALID 索引（`DROP INDEX CONCURRENTLY`，清理本身失败只记 notes）→ `Interrupted`，`resumable=true`，重跑从头执行（5.1.5 自愈）。

### 5.3 Verify

`pg_index.indisvalid AND indisready` 为真；记录 `pg_relation_size`；审计写执行摘要。

## 6. 能力位与入口

- `core_lib/src/ops/field_migration/mod.rs` descriptor：`engines` 由 `["mysql", "mariadb"]` 改为 `["mysql", "mariadb", "postgresql"]`；title/description 不动（避免无关 UI 文案 diff，e2e 无断言）。
- 后端 `run_analyze` 等入口已按 `descriptor.status` 门控、不按 `engines` 过滤（已确认），加能力位即对 PG 连接开放；前端 OpsCenter/FieldGovernance 无引擎门控（已确认），无需改动。
- notnull/index 执行器暴露最小 HTTP 端点（与 backfill 任务同构：create/run/status/list），鉴权复用 `authorize_write`；UI 表单不做（见第 2 节）。

## 7. 测试

环境 `FIELD_MIGRATION_PG_URL`，独立临时库用完即删，同 S2。env 缺失打印原因并返回，不 `#[ignore]`。

### 7.1 `core_lib/tests/field_migration_notnull_pg.rs`（节点 1 报数）

| 用例 | 断言 |
| --- | --- |
| `notnull_happy_path` | nullable 无 NULL → Completed；`is_nullable=NO`、`attnotnull=true`；`fm_not_null_*` 约束已删除 |
| `null_rows_block_then_fix_and_rerun` | 含 NULL → `Blocked(scan:null_rows)` 含 `null_count` 与样本（≤100）；UPDATE 修复后重跑（从头重扫）→ Completed |
| `scan_resume_after_terminate` | 大表小批 + 限速；扫描中途改列名注入失败（`pg_terminate_backend` 打到空闲连接时 sqlx 会自动换新连接，无法稳定中断）→ 续跑从 `last_pk` 继续并完成；`scanned_rows` 不重复累计 |
| `residual_not_valid_constraint_resumes_at_validate` | 预建 NOT VALID 同名约束 + 无 NULL → 跳过 AddCheck，直接 Validate 后完成 |
| `residual_validated_constraint_resumes_at_set_not_null` | 预建已 validated 同名约束 + 列仍 nullable → 从 SetNotNull 起完成 |
| `already_not_null_is_noop` | 列已 NOT NULL → Completed(notes 标记 no-op)，无 DDL 发生 |
| `validate_failure_returns_to_scan` | 扫描完成后、Validate 前经控制信号暂停，另一连接插入 NULL，恢复 → `Blocked(validate:violations)`；约束保留 NOT VALID；删 NULL 后续跑重扫并完成 |
| `text_pk_scan_uses_c_collation` | 文本主键混合大小写扫描分页正确，SQL 含 `COLLATE "C"` |
| `missing_pk_or_partitioned_blocks` | 无主键表 / 分区表 → 对应 preflight BLOCK，零写入 |
| `spec_mismatch_refuses_resume` | 改列名后续跑 → `Blocked(preflight:spec_mismatch)` |

PG < 12 的版本判断为纯函数，放 `pg_notnull.rs` 内 `#[cfg(test)]` 单测（不依赖真库版本）。

### 7.2 `core_lib/tests/field_migration_index_pg.rs`（节点 2 报数）

| 用例 | 断言 |
| --- | --- |
| `create_plain_index` | Completed；`indisvalid AND indisready`；记录 `pg_relation_size` |
| `create_unique_index` | 无重复数据 → Completed |
| `unique_violation_blocks_and_cleans_up` | 重复数据 → `Blocked(build:unique_violation)`；同名 INVALID 索引不存在（已清理） |
| `invalid_leftover_dropped_and_rebuilt` | 手工造同名 INVALID 索引 → job 清理后重建成功 |
| `existing_valid_index_is_noop` | 同名有效索引 → Completed(no-op)，未重建（`pg_stat_user_tables`/索引 xid 不变或 notes 标记） |
| `missing_table_or_column_blocks` | preflight BLOCK，零写入 |
| `name_too_long_blocks` | >63 字节 → `Blocked(preflight:name_too_long)`，不截断 |
| `partitioned_table_blocks` | `Blocked(preflight:partitioned)` |
| `progress_polling_best_effort` | 轮询在无权限/空视图时不影响主流程完成 |
| `spec_mismatch_refuses_resume` | 改 columns 后续跑 → `Blocked(preflight:spec_mismatch)` |

### 7.3 能力位

`mod.rs` 既有 descriptor 单测补充：`engines` 含 `postgresql` 且 mysql/mariadb 不变。

### 7.4 守卫

`scripts/test-summary.py` 的 `CORE_LIB_TARGET_GUARDS` 增加：

```python
"field_migration_notnull_pg": ("core_lib/field_migration_notnull_pg", ["FIELD_MIGRATION_PG_URL"]),
"field_migration_index_pg": ("core_lib/field_migration_index_pg", ["FIELD_MIGRATION_PG_URL"]),
```

## 8. 验证步骤

1. `cargo fmt -p core_lib -p web-server -- --check`（仅 S3 触及文件；全仓 fmt 在新工具链下对存量代码不通过，维持 S2 结论不做全仓重格式化）、`cargo check --workspace`。
2. 真库 notnull（节点 1，报数）：
   `FIELD_MIGRATION_PG_URL=... cargo test -p core_lib --test field_migration_notnull_pg --no-fail-fast -- --test-threads=1`
3. 真库 index（节点 2，报数）：
   `FIELD_MIGRATION_PG_URL=... cargo test -p core_lib --test field_migration_index_pg --no-fail-fast -- --test-threads=1`
4. 三套口径（带库均 `cargo test --workspace --no-fail-fast -- --test-threads=1`）：
   无库 / MySQL(33306)+PG / MariaDB(33307)+PG，各自同 env 跑 `scripts/test-summary.py`。
5. 本地提交，不 push。

## 9. 已定决策

| # | 决策 | 理由 |
| --- | --- | --- |
| 1 | notnull/index 用独立 job 类型，并入 `MigrationStore` 同一文件，`skip_serializing_if` 空映射不序列化 | 复用持久化/控制信号；无 PG 任务时 MySQL store 文件字节不变 |
| 2 | SET NOT NULL 作用于源表列，与 S1 脚本（`fm_not_null_<column>`）同口径 | 保持「脚本即执行计划」一致性；不引入目标表分叉 |
| 3 | scan 发现 NULL → Blocked，修复后从头重扫 | 数据已变，旧游标续扫结果不可信 |
| 4 | VALIDATE 失败 → 约束保留 NOT VALID，Blocked，续跑重回 Scan | NOT VALID 残留无害且自愈；符合 PG 官方推荐路径 |
| 5 | PG < 12 直接 BLOCK | 只有 12+ 能用已验证 CHECK 跳过 SET NOT NULL 全表扫 |
| 6 | unique 索引不做重复值预检 | 预检也是全表扫；依赖 build 报错 + INVALID 清理即可 |
| 7 | 同名 INVALID 索引自动 DROP INDEX CONCURRENTLY 后重建；VALID 同名 → no-op | 官方推荐恢复路径；幂等 |
| 8 | DDL 单语句自动提交，不开事务块；session 级 lock_timeout | CONCURRENTLY 禁入事务；notnull 各阶段独立幂等 |
| 9 | 中断残留的 NOT VALID 约束/INVALID 索引：文档化清理 SQL + job 续跑自愈 | 无害残留，不引入额外清理任务 |
| 10 | descriptor 只加 engines，文案不动；执行器暴露最小 HTTP 端点，前端表单不做 | 能力位即入口；UI 变更风险与收益不匹配 |
| 11 | null 样本上限 100 条主键 | 报告可读性；全量计数仍精确 |

## 10. 实现期发现（与方案的偏差）

1. **主键探测**：PG 快照里的 `column_key` 依赖 `indkey::int2[]` 转换，在 PG 上不可靠（恒为空），执行器改为直接查 `pg_index` + `pg_attribute`；复合主键按多行判 BLOCK。
2. **unique 违例判据**：PG 的实际文案是 `could not create unique index ...`（DETAIL 里的 `duplicate key value` 不一定进 message），判据三种都认。
3. **`NOT VALID` 仍约束新写入**：VALIDATE 失败用例改为「先写 NULL 再加 NOT VALID 约束」，否则插入阶段就被拒绝。
4. **节奏测试**：`pg_terminate_backend` 打空闲连接无效（sqlx 自动替换），续跑用例改为确定性故障注入（扫描中改列名），断言点不变。
5. **`config` / `phase` 字段**：`NotNullJob.config` 复用 S2 限速参数；`IndexJob.phase` 便于状态展示；均带 serde 默认值，不影响旧文件。

## 11. S4 边界清单

1. 改类型、改 collation 可执行化（表重写、维护窗口、USING 失败处理）。
2. 复合主键、分区表、表达式/部分唯一索引、`INCLUDE`/opclass。
3. `citext`/非确定性排序规则文本主键。
4. 跨数据库实例快照一致性。
5. 目标结构漂移细粒度指纹。
6. `pg_cancel_backend` 与进程重启孤儿连接清理。
7. 大表流式读取/写入。
8. 前端表单（notnull/index 参数化入口、进度展示）。
9. 多任务并发限速公平性与全局配额。
10. 索引进度持久化历史（当前仅保留最后一次快照）。
