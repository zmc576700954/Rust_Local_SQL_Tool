# PG 字段治理 S5：覆盖面扩展（复合主键 / 分区表 / 部分索引 / USING / FK 编排）

状态：已实现（2026-11-12；三套口径 720/0，28 targets）
范围：PostgreSQL 专用；MySQL/MariaDB 行为、SQL、任务 JSON 零变化。
依赖：S4 已提交（`237ccdc`），typechange 13 用例真库全绿；S3 的 notnull/index 各 10 用例全绿。
范围确认：2026-11-12 用户就四个分叉全部选择「推荐档」——
分区表全量四执行器 / freeform USING 三重门 / FK 显式对端编排 / 部分索引放行（表达式索引仍 BLOCK）。

---

## 1. 目标

把 S2–S4 执行器的「单列主键 + 普通表 + 非分区唯一索引 + 固定 USING + FK 一律阻断」五个窄口打开：

1. **复合主键 keyset**：回填与校验支持多列主键（行比较、tuple checkpoint、复合 ON CONFLICT）。
2. **分区表**：backfill 源端放开；notnull 父表 DDL + 逐分区 VALIDATE；index 逐分区 CONCURRENTLY + 父表占位 + ATTACH；typechange 父表级联放行。
3. **部分唯一索引**作为 `ON CONFLICT` 目标（表达式索引仍 BLOCK，记已知缺口）。
4. **freeform `USING` 表达式**（含 `timestamp`↔`timestamptz` 显式 `AT TIME ZONE`），三重门。
5. **FK 感知改类型**：显式声明对端列的拆装编排（DROP → ALTER 双端 → ADD NOT VALID → VALIDATE）。

## 2. 非范围（S6 边界）

- 表达式唯一索引作为 ON CONFLICT 目标（分页键语义 + 渲染面，已知缺口）。
- 分区目标表的回填（PG 全版本不支持分区表 `ON CONFLICT DO UPDATE`，维持 BLOCK + 手工路径）。
- 复合主键的 MySQL 支持（MySQL 路径零变化）。
- 复合外键（多列 FK）的编排（单列 FK 才编排，复合 FK 一律 BLOCK）。
- `INCLUDE` / opclass / 排序规则变更执行。
- `pg_cancel_backend`、孤儿连接清理、限速公平性、索引进度历史持久化（S6）。
- 前端表单（`FieldGovernance.tsx` 不动）。

## 3. 背景与现状

- 回填链路主键为单值：`BackfillSpec.pk_column`、`Checkpoint.last_pk`、`push_range`/`order_expr`/`bind_value` 单键，`has_on_conflict_target` 要求 `indnkeyatts=1 AND indpred IS NULL AND indexprs IS NULL`。
- `backfill.rs::create` 硬性要求 `dependencies.primary_key_columns.len()==1`，且 `environment.partitioned` 直接拒绝（MySQL 仍在用这条）。
- `pg_index.rs` 对分区表 BLOCK（`preflight:partitioned`）；PG 不支持对分区父表直接 `CREATE INDEX CONCURRENTLY`。
- `pg_typechange.rs` 对 `relkind='p'` BLOCK、对 `USING` 固定 `col::target`、对 FK 任一方向 BLOCK。
- PG 行为依据：
  - 行比较 `(a,b) > ($1,$2)` 按字典序，等价于 `a > $1 OR (a = $1 AND b > $2)`，可用复合索引。
  - `VALIDATE CONSTRAINT` 可对单个分区执行；父表约束的 `convalidated` 只有在所有分区验证后才为真（PG 11+）。
  - 分区索引配方：`CREATE INDEX ... ON ONLY parent` → 每个分区 `CREATE INDEX CONCURRENTLY` → `ALTER INDEX parent ATTACH PARTITION part_idx`（PG 11+）。
  - `ALTER TABLE parent ALTER COLUMN TYPE` 单语句级联全部子分区，锁为父表 + 全部分区 ACCESS EXCLUSIVE。
  - `ON CONFLICT (cols) WHERE pred` 通过部分唯一索引推断（`index_predicate`），谓词必须与索引谓词等价。
  - 改 FK 参与列的类型必须先删除 FK 约束（即使收窄/放宽不触发重写），重建用 `NOT VALID` + `VALIDATE` 降低锁时长。

## 4. 数据模型变更（`migration_store.rs`）

全部新字段带 `#[serde(default)]`（或 `skip_serializing_if`），无 S5 任务时存量 store 字节不变：

```rust
// BackfillSpec 追加
#[serde(default, skip_serializing_if = "Vec::is_empty")]
pub pk_columns: Vec<String>,   // 复合主键列，顺序即比较顺序；空 = 单列（用 pk_column）
#[serde(default, skip_serializing_if = "Vec::is_empty")]
pub pk_kinds: Vec<PkKind>,     // 与 pk_columns 等长；单列时空

// Checkpoint 追加
#[serde(default, skip_serializing_if = "Option::is_none")]
pub last_pks: Option<Vec<String>>, // 复合主键边界；单列主键保持 last_pk

// BatchRecord 追加
#[serde(default, skip_serializing_if = "Option::is_none")]
pub end_pks: Option<Vec<String>>,

// TypeChangeJob 追加
#[serde(default, skip_serializing_if = "Option::is_none")]
pub using_expr: Option<String>,          // freeform USING；进 spec_hash
#[serde(default, skip_serializing_if = "Vec::is_empty")]
pub fk_plan: Vec<FkPlanEntry>,           // 拆装编排清单；进 spec_hash
pub struct FkPlanEntry {
    pub constraint: String,
    pub referencing_schema: String, pub referencing_table: String, pub referencing_column: String,
    pub referenced_schema: String,  pub referenced_table: String,  pub referenced_column: String,
    pub target_type: String,     // 与主列目标类型一致
    pub dropped: bool,           // 编排进度（幂等续跑）
    pub altered: bool,
    pub recreated: bool,         // ADD NOT VALID 完成
    pub validated: bool,
}

// IndexProgress 追加
#[serde(default)] pub partitions_total: u64,
#[serde(default)] pub partitions_done: u64,
#[serde(default, skip_serializing_if = "Option::is_none")] pub current_partition: Option<String>,
```

`MigrationHub` 不新增存储类别（沿用 S2–S4 四类 job）。重启 running→interrupted 覆盖不变。

## 5. 复合主键 keyset（回填 + V1–V4）

### 5.1 规格与入口

- `backfill.rs::create`：`is_pg` 时允许 `primary_key_columns.len() >= 1`；`pk_columns = primary_key_columns`，`pk_column = 第一个`（兼容既有字段/MySQL）。MySQL 分支保持「必须单列」。
- PG preflight 由源表快照确定每列类型 → `pk_kinds`；任一列不在整型/numeric/text/uuid 白名单 → BLOCK `preflight:pk_type`；文本列逐个做确定性排序规则检查（`preflight:pk_collation`）。
- 目标列映射不得覆盖任一主键列（沿用单列规则，扩展到集合）。

### 5.2 SQL 形态

单列路径 SQL 形态与 S2 **逐字保持不变**（既有测试断言 `$1::text` / `ORDER BY`/`LIMIT` 结构）。

复合路径：

```sql
INSERT INTO dst (a, b, v) SELECT a, b, expr FROM src
WHERE TRUE
  AND (a COLLATE "C", b) > (($1::text COLLATE "C"), $2::bigint)
  AND (a COLLATE "C", b) <= (($3::text COLLATE "C"), $4::bigint)
ORDER BY a COLLATE "C", b
LIMIT $5
ON CONFLICT (a, b) DO UPDATE SET v = EXCLUDED.v
```

- 行构造器内文本列统一 `COLLATE "C"`，与 `ORDER BY` 一致；绑定仍类型化（不拼字面量）。
- V1–V4 的 JOIN 条件改为等多列 `t.a = s.a AND t.b = s.b`；范围谓词复用同一 tuple 生成器。

### 5.3 checkpoint 与续跑

- 复合主键：`Checkpoint.last_pks = Some(vec![...])`，`last_pk` 仅作展示（tuples 的 `a=1,b=2` 形式）。
- 单列主键：`last_pks = None`，`last_pk` 语义不变（既有断言不动）。
- 续跑：`last_pks` 优先；`completed_batches > 0` 且 tuple 缺失/长度不符 → BLOCK `preflight:checkpoint_invalid`；`pk_kinds` 与 checkpoint 记录不一致 → BLOCK `preflight:pk_kind_mismatch`。
- 单事务模式、批事务、resume 抽检、final 重查全部走同一 tuple 接口。

## 6. 部分唯一索引作为 ON CONFLICT 目标

`has_on_conflict_target` 升级为 `on_conflict_target(...) -> Option<ConflictTarget>`：

```rust
pub struct ConflictTarget { pub columns: Vec<String>, pub predicate: Option<String> }
```

- 候选：`indisunique AND indnkeyatts = pk_columns.len() AND indexprs IS NULL`，键列集合等于 `pk_columns` 集合（顺序无关，PG 推断不要求顺序）。
- 选择顺序：非部分索引（`indpred IS NULL`）优先；只有部分索引可用时取其 `pg_get_expr(indpred, indrelid)` 作为 `WHERE` 谓词。
- 批 SQL：`ON CONFLICT (a, b) WHERE {pred} DO UPDATE SET ...`；单列 + 非部分索引时 SQL 与 S2 逐字一致。
- 残余风险：只有部分索引时，谓词外的行不参与冲突处理，重复主键可能插入成功 → 由批后 V1/V2 校验拦截并 BLOCK；preflight notes 记录该风险。
- 表达式索引（`indexprs IS NOT NULL`）不参与推断；仅有表达式唯一索引 → BLOCK `preflight:on_conflict_target`（已知缺口）。

## 7. 分区表（四执行器）

### 7.1 backfill

- 源分区表：`create` 放行（仅 PG；MySQL 保持拦截）；preflight 记录 `relkind='p'`，keyset 经父表透明分页（`ORDER BY` 主键全局有序）。
- 目标分区表：BLOCK `preflight:partitioned_target`——PG 全版本不支持分区表 `ON CONFLICT DO UPDATE`；notes 给「逐分区 INSERT ... SELECT ... ON CONFLICT」手工路径。
- 源或目标为分区表的任务，checkpoint 增记 `partitioned_source/target` 到 notes（不新字段）。

### 7.2 notnull

- `relkind='p'` 放行：`ADD CHECK ... NOT VALID` 在父表执行（自动下推到各分区）。
- VALIDATE 逐分区执行：`pg_inherits` 递归枚举叶子分区，逐分区 `ALTER TABLE part VALIDATE CONSTRAINT fm_not_null_col`；已完成分区（`convalidated`）跳过 → 幂等/可续跑。
- `SET NOT NULL` 在父表执行（PG 12+ 快速路径依赖已验证 CHECK）；`DROP CONSTRAINT` 在父表执行。
- 进度：`notes` 记录 `validated x/y partitions`；中断重跑按 `convalidated` 自愈。

### 7.3 index

- 分区表改为配方执行（PG 11+）：
  1. `CREATE INDEX {name} ON ONLY {parent} ({cols})`（父表占位，无数据）；
  2. 逐叶子分区 `CREATE INDEX CONCURRENTLY {part_index_name} ON {part} ({cols})`（名字 ≤63 字节，超长截断+短哈希）；
  3. `ALTER INDEX {parent_index} ATTACH PARTITION {part_index}`。
- 幂等：父索引已存在且有效 → 跳过步骤 1；分区已有同名有效索引 → 跳过步骤 2；已 ATTACH（`pg_inherits` 中 index 父子关系）→ 跳过步骤 3。
- INVALID 残留：分区索引 INVALID → `DROP INDEX CONCURRENTLY` 后重建；父索引 INVALID → 整个任务 BLOCK 给手工清理（父索引不能 CONCURRENTLY 重建）。
- unique 违例映射沿用 `build:unique_violation`；进度写入 `IndexProgress.partitions_*`。
- 非分区表路径 SQL 与 S3 逐字一致。

### 7.4 typechange

- `relkind='p'` 放行：单语句 `ALTER TABLE parent ALTER COLUMN ... TYPE ...` 级联全部分区；preflight notes 明示「锁范围 = 父表 + 全部分区 ACCESS EXCLUSIVE，重写成本按全部分区总量计」。
- 行数快照取父表 `COUNT(*)`；索引健康检查扩展到全部分区（递归），任一 `indisvalid=false` 判失败。
- 分区表上的 FK/视图依赖检查沿用（PG 元数据对父表可见）。

## 8. freeform USING（三重门）

- 入参 `using_expr: Option<String>`（HTTP + `TypeChangeRequest`）；上限 2000 字符；含 `;` 直接拒绝（单语句约束）。
- **门 1 解析干跑**：preflight 执行 `SELECT ({using_expr}) FROM {qualified} LIMIT 0`，报错 → BLOCK `preflight:bad_using_expr`（不执行、不评估数据）。
- **门 2 强制开闸**：提供 `using_expr` 时分类恒为 `Rewrite`，必须 `allow_rewrite=true` 且 `production_confirmed`；否则 BLOCK `preflight:rewrite_requires_gate`。
- **门 3 指纹绑定**：`spec_hash` 纳入 `using_expr`，续跑篡改即拒绝 `preflight:spec_mismatch`。
- 非确定性硬阻断：提供 `using_expr` 时解除（显式 `AT TIME ZONE` 属用户责任，由门 1–3 兜底）；未提供时维持 `preflight:non_deterministic_cast`。
- SQL：`ALTER TABLE ... ALTER COLUMN ... TYPE {target} USING {using_expr}`；`using_expr` 原样拼接（文档标注注入面由三重门收敛）。
- FK 编排 + `using_expr` 同时出现 → BLOCK `preflight:fk_with_custom_using`（对端列无法安全复用同一表达式）。

## 9. FK 感知改类型（显式对端编排）

### 9.1 入参与计划

`fk_counterparts: Vec<{ schema?, table, column }>`（HTTP + 请求）；preflight 用 `pg_constraint` 枚举主列参与的全部单列 FK，生成 `FkPlanEntry`：

- 主列为被引用列（`confrelid = 本表`）：对端 = 引用表列（`conrelid`/`conkey[0]`）。
- 主列为引用列（`conrelid = 本表`）：对端 = 被引用表列（`confrelid`/`confkey[0]`）。
- 复合 FK（`array_length(conkey,1) > 1`）→ BLOCK `preflight:composite_fk`。
- 每个 FK 必须被 `fk_counterparts` 精确覆盖；缺声明 → BLOCK `preflight:fk_counterparts_missing`（detail 列出约束与对端列）。
- 对端列 live 类型必须等于主列 `source_type`；对端表属主校验；否则 BLOCK `preflight:fk_type_mismatch` / `preflight:not_owner`。
- `fk_plan` 写入 job 并进 `spec_hash`。

### 9.2 执行序（每步幂等）

1. 逐 FK `ALTER TABLE {referencing} DROP CONSTRAINT IF EXISTS {name}` → `dropped=true`；
2. 主列 ALTER TYPE（live != target 才执行；target 由 `render_target` 生成，`USING` 沿用 §8，FK 场景禁自定义 USING）；
3. 逐对端列 ALTER TYPE（同样 live != target 才执行）；
4. 逐 FK `ALTER TABLE {referencing} ADD CONSTRAINT {name} FOREIGN KEY ({col}) REFERENCES {referenced} ({col}) NOT VALID` → `recreated=true`；
5. 逐 FK `ALTER TABLE {referencing} VALIDATE CONSTRAINT {name}` → `validated=true`。

- 任一步失败：`Interrupted`，`last_error` 标注失败步骤与剩余步骤；重跑按 `fk_plan` 进度 + live 类型自愈。
- 验证：主列 + 对端列类型均为目标；主表行数不变；全部 FK `convalidated=true`；索引健康不变。

## 10. HTTP（web-server，两套 router 同步）

- `POST /ops/field-governance/type-change` 入参追加：`using_expr?: string`、`fk_counterparts?: [{schema?, table, column}]`。
- `POST /ops/field-governance/index` 不变（分区配方对用户透明）。
- backfill create 入参不变（复合主键/分区由分析产物驱动）。
- 其余端点形态不变。

## 11. 测试

### 11.1 单测（无库）

- 复合主键 tuple SQL 生成（`build_batch_sql`）：行比较、COLLATE、绑定数量、`ON CONFLICT (a, b)`。
- `ConflictTarget` 渲染：非部分 / 部分（含 `WHERE`）/ 表达式索引排除。
- `IndexSpec` 分区索引名派生（≤63 字节、稳定性）。
- `spec_hash` 纳入 `using_expr` 与 `fk_plan`。
- store 回环：新字段默认值、老 JSON 反序列化、空值不序列化。

### 11.2 真库

`field_migration_backfill_pg.rs` 追加：
| 用例 | 断言 |
| --- | --- |
| `composite_pk_backfill` | `(tenant_id, id)` 复合主键 1000 行，keyset 分批完成；无重复/遗漏；checkpoint `last_pks` 与行数一致 |
| `composite_pk_resume` | 中断后从 tuple 续跑，V1–V4 全绿 |
| `composite_pk_text_collate` | 文本 + 整数复合键，`COLLATE "C"` 排序与 `ORDER BY` 一致 |
| `partial_unique_index_conflict_target` | 目标表仅部分唯一索引（`WHERE deleted_at IS NULL`），ON CONFLICT 带 WHERE，回填完成 |
| `partitioned_source_backfill` | 源为分区表（父表），keyset 跨分区完成 |
| `partitioned_target_blocked` | 目标为分区表 → BLOCK `preflight:partitioned_target` |

`field_migration_notnull_pg.rs` 追加：
| 用例 | 断言 |
| --- | --- |
| `partitioned_table_notnull` | 分区父表 SET NOT NULL 完成；逐分区 VALIDATE 后 `convalidated=true` |

`field_migration_index_pg.rs` 追加：
| 用例 | 断言 |
| --- | --- |
| `partitioned_index_recipe` | 父表 ONLY 占位 + 逐分区 CONCURRENTLY + ATTACH；全部 `indisvalid`；重跑幂等 |

`field_migration_typechange_pg.rs` 追加：
| 用例 | 断言 |
| --- | --- |
| `partitioned_table_typechange` | 父表 text→varchar(200) 级联分区；全部分区列类型更新 |
| `custom_using_expr` | `text→bigint USING NULLIF(amount,'')::bigint` 开闸后完成（空串/NULL 都转 NULL，SUM=30） |
| `bad_using_expr_blocks` | 非法 USING（`amount::no_such_type`）→ BLOCK `preflight:bad_using_expr`，列类型与数据不动 |
| `timestamp_tz_explicit` | `timestamp→timestamptz USING col AT TIME ZONE 'UTC'` 完成（未提供 USING 仍 BLOCK） |
| `fk_orchestration` | 父/子表 FK 列同改 varchar 加长：DROP→ALTER 双端→ADD NOT VALID→VALIDATE，完成后 FK `convalidated=true`、双端类型一致 |
| `fk_counterparts_mismatch_blocks` | 声明错对端（`children.id` 而非 `parent_id`）→ BLOCK `preflight:fk_counterparts_missing`，detail 指出真实对端列 |
| `foreign_key_block`（既有，语义收窄） | 未声明对端 → BLOCK `preflight:fk_counterparts_missing`（原 `preflight:foreign_keys`），detail 含约束名 |

节点报数：每完成一个子项（复合主键 / 部分索引 / 分区 / USING / FK）报一次，不等全部完成。

## 12. 决策表（评审点，2026-11-12 已确认）

| # | 决策点 | 结论 | 备注 |
| --- | --- | --- | --- |
| 1 | 分区表支持面 | 全量四执行器 | 用户选推荐档 |
| 2 | freeform USING 门 | 干跑解析 + 强制开闸 + 指纹绑定 | 提供 USING 即解除非确定性阻断 |
| 3 | FK 感知深度 | 显式对端编排（DROP/ALTER/ADD NOT VALID/VALIDATE） | 复合 FK BLOCK |
| 4 | ON CONFLICT 扩展 | 部分索引放行，表达式索引 BLOCK | 表达式索引记已知缺口 |
| 5 | 复合主键 checkpoint | 新增 `last_pks`，`last_pk` 仅展示 | 单列路径字节不变 |
| 6 | 分区目标回填 | BLOCK | PG 不支持分区表 ON CONFLICT DO UPDATE |
| 7 | 复合主键 MySQL | 不支持（保持单列） | 零变化约束 |

## 13. 收尾要求

- `scripts/test-summary.py` 守卫生效（target 名不变，无需新增条目；若新增测试文件则同步）。
- fmt 只作用于本期改动文件。
- 三套口径：无库 / MySQL(33306)+PG / MariaDB(33307)+PG，`--test-threads=1 --no-fail-fast`，报 `test-summary.py` 数字。
- 不 push、不 `#[ignore]`、不删用例、改断言写依据；MySQL/MariaDB 行为/SQL/任务 JSON 零变化。
- 工作记忆 `.workbuddy/memory/2026-11-12.md` 追加；本地提交。

## 14. 已知缺口（转 S6）

- 表达式唯一索引作为 ON CONFLICT 目标。
- 分区目标表回填（需逐分区写入器 + 分区键路由）。
- 复合 FK 编排。
- preflight 快照查询无 `lock_timeout`（S4 §12 记录，S6 超时治理）。
- 索引父表 INVALID 残留只能人工清理。

## 15. 实现期发现与偏差（2026-11-12 实施记录）

### 15.1 计划外但必须修的三处

1. **S1 脚本层不支持多列主键**（`pg_scripts.rs`）
   `create_table_sql_pg` 硬编码单列 `pk_name/pk_type`，`build_backfill_sql_pg` / `build_verify_sql_pg` /
   `rollback_delete_sql` 全为单键。若只改执行器，用户按 S1 生成的脚本建表会得到单列主键，
   执行器的 `ON CONFLICT (a, b)` 必然 42P10 失败（端到端断裂）。
   处理：主键推导改为「多列优先、单列沿用显式 `pk_column`」；建表/回填/校验/回滚四段模板全部多列化，
   单列输出逐字不变（`key_join` 在单列时产出 `t.id = s.id`）。

2. **`risk.rs` 的两条 Block 未按引擎收窄**
   - `E02_PARTITIONED`：PG 源分区表可与父表 keyset 对接，降为 `High`（`blocks_write=false`）；MySQL 保持 `Block`。
   - `E14_NO_SINGLE_PK`：PG 复合主键走行比较分页，不再产生该风险；无主键（列集为空）在 PG 仍 `Block`。
   既有断言 `field_migration_pg.rs`（S1 真库集成）同步更新并写明依据。

3. **`CROSS JOIN pg_database` 缺库名过滤**（`pg_backfill::load_snapshot`、`index_advisor` 两处）
   `pg_database` 未过滤导致每个列按实例内库数量膨胀，`TableSnapshot::snapshot_hash()` 随并发建/删库抖动，
   续跑被误判 `preflight:schema_drift` / `preflight:target_schema_drift`
   （实测：并发跑 `field_migration_backfill_pg` 时 flaky 2/3）。
   处理：三处均加 `AND d.datname = current_database()`。
   **影响**：修复前写入的 checkpoint 结构哈希会失配，存量任务续跑会 Block（需重建任务）；
   本项目为本地工具，无生产存量，接受。

### 15.2 PG 硬限制：分区键列不可改类型

实测 `ALTER TABLE parted ALTER COLUMN id TYPE bigint` 报
`cannot alter column "id" because it is part of the partition key of relation "parted"`。
原计划 §7.4 只写「单语句级联全部分区」，未区分列角色。
处理：preflight 新增 `preflight:partition_key`（查 `pg_partitioned_table.partattrs`），
非分区键列（如 payload/note）仍走单语句级联。用例 `partitioned_table_typechange` 同时覆盖两侧。

### 15.3 FK 语义收窄带来的既有断言变更

`foreign_key_block`（S4）phase 由 `preflight:foreign_keys` 改为
`preflight:fk_counterparts_missing`：阻断条件从「存在外键」收窄为「存在外键且未声明对端」。
断言按新语义更新并在测试内写明依据（spec §9）。

### 15.4 复合主键续跑用例改用确定性中断

原计划用 `pg_terminate_backend` 制造中断，但「抢在任务完成前 kill」在并发测试下 flaky（实测 2/3 失败）。
改为：目标表预建 `CHECK (seq < 900)`，第 3 批确定性失败 → `Interrupted`（已提交 600 行）→
`DROP CONSTRAINT` 后 `last_pks=(tenant_id=1, seq=600)` 续跑到 2000 行。
副作用是更直接地验证了 §5.3 的 tuple checkpoint 语义。

### 15.5 实测数字

| 口径 | 结果 | 耗时 |
| --- | --- | --- |
| 无库 | 720 passed / 0 failed / 28 targets（空跑 131） | 1.2s |
| MySQL 8.0.46(33306) + PG | 720 / 0 / 28 targets（0 空跑） | 32.8s |
| MariaDB 11.8.9(33307) + PG | 720 / 0 / 28 targets（0 空跑） | 34.3s |

单 target：backfill 17、index 10、notnull 11、typechange 19、verify 9、S1 集成 1。
