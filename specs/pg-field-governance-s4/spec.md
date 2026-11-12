# PG 字段治理 S4：改类型可执行化（免重写自动 / 重写显式开闸）

状态：已实现（2026-11-12，本地提交）
范围：PostgreSQL 专用执行器；MySQL/MariaDB 行为不变。
依赖：S3 已提交（`879ac65`），notnull（10）+ index（10）真库全绿；能力位已含 postgresql。
范围确认：2026-11-11 用户选定方向「改类型可执行化（含重写路径开闸）+ 覆盖面扩展 + 运维化」三项，
拆为 S4（改类型）/ S5（覆盖面）/ S6（运维化）连续三期；本期为 S4。

---

## 1. 目标

把三类改动中最后一块「改类型」从仅脚本升级为可执行：

1. **免重写路径自动执行**：preflight 判定为元数据级变更（白名单）时，`ALTER TABLE ... ALTER COLUMN ... TYPE` 单语句直接执行，无需开闸。
2. **重写路径显式开闸**：判定为触发表重写时，请求必须带 `allow_rewrite=true`（且沿用 `production_confirmed` 门）才执行；未开闸则 BLOCK 并继续输出脚本。
3. **非确定性转换硬阻断**：`timestamp`↔`timestamptz` 等依赖 session `TimeZone` 的转换即使开闸也 BLOCK（沿用 S2「不确定即拒绝」原则）。
4. 复用 S2/S3 既有机制：spec 指纹、引擎绑定、控制信号（pause/abort）、审计、session 级 `lock_timeout` + `statement_timeout=0` 的 DDL 单语句自动提交。

## 2. 非范围（进 S5/S6 边界）

- 不支持复合主键、分区表、部分/表达式唯一索引（S5）。
- 不支持自定义 `USING` 表达式（v1 固定 `USING col::target`；`timestamp`↔`timestamptz` 的显式 `AT TIME ZONE` 方案进 S5）。
- 不做 FK 感知改类型（列参与任何方向的外键即 BLOCK；S5 候选）。
- 不做 collate 变更执行（维持仅脚本）。
- 不做 `pg_cancel_backend`、孤儿连接清理、并发限速公平性、进度历史（S6）。
- 不做前端表单；分析/风险/脚本产物不动（脚本仍是兜底出口）。

## 3. 背景与现状

- S1 的 `pg_scripts.rs::push_rewrite_scripts` 已产出 `alter-type` 脚本（`executable=false`），S4 不改分析/脚本产物。
- S3 已建立可复用骨架：`NotNullJob`/`IndexJob` + `MigrationStore` 空映射不序列化 + `MigrationHub` 控制信号 + `exec_ddl`（pg_notnull）/`exec_index_ddl`（pg_index）两个同构的 DDL 执行助手。
- `pg_evidence::normalize_pg_type` 提供类型名归一化（int4→integer、timestamptz→timestamp with time zone、varchar→character varying 等）。
- PG 行为依据：
  - `varchar(n)→varchar(m)`（m≥n 或不限长度）、`varchar→text`、`text→varchar`（不限长度）为二进制兼容，纯元数据变更。
  - PG ≥ 9.2：`numeric` typmod 放宽（scale 不降、整数位不减）、`timestamp[tz](p)` 精度放宽为纯元数据变更。
  - `text→varchar(n)`、`varchar` 缩窄、`int4→int8`、`numeric` 缩窄等触发全表扫描/重写，持有 `ACCESS EXCLUSIVE` 锁至完成。
  - `timestamp→timestamptz` 的 cast 依赖 session `TimeZone`，结果不确定。
  - DDL 失败是原子的：语句报错后列保持原状，无半成品状态；「续跑」= 重跑语句或自愈跳过。

## 4. 数据模型

`migration_store.rs` 增加第三类 job，沿用 S3 约定（空映射不序列化，无 S4 任务时存量 store 文件字节不变）：

```rust
pub struct MigrationStore {
    jobs: HashMap<String, MigrationJob>,          // 既有
    notnull_jobs: HashMap<String, NotNullJob>,    // S3
    index_jobs: HashMap<String, IndexJob>,        // S3
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    typechange_jobs: HashMap<String, TypeChangeJob>, // S4 新增
    // path 不变
}
```

```rust
pub struct TypeChangeJob {
    pub job_id: String,
    pub plan_id: String,
    pub db_id / db_name / db_fingerprint: String,
    pub schema: Option<String>,
    pub table: String,
    pub column: String,
    /// 执行时记录的列类型（format_type(atttypid, atttypmod)）；续跑做漂移校验。
    pub source_type: String,
    /// 请求的目标类型（用户原始串，执行前经 ::regtype 验证）。
    pub target_type: String,
    /// 表/列/源类型/目标类型指纹；续跑不一致即拒绝。
    pub spec_hash: String,
    #[serde(default = "default_engine_mysql")]
    pub engine: String,
    pub status: MigrationStatus,
    pub phase: TypeChangePhase,            // Preflight/Applying/Verifying/Done
    pub classification: TypeChangeClass,   // MetadataOnly / Rewrite
    #[serde(default)]
    pub allow_rewrite: bool,
    #[serde(default)]
    pub production_confirmed: bool,
    #[serde(default)]
    pub execution: TypeChangeExecution,    // { atomic:false, rollback_scope:"statement", resumable:true }
    #[serde(default)]
    pub verify: TypeChangeVerify,          // 前后行数、索引健康、默认值快照
    #[serde(default)]
    pub run_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocking: Option<BlockingDetail>,  // 复用；phase 用 "preflight:*"/"apply:*"
    pub last_error: Option<String>,
    pub notes: Vec<String>,
    pub created_at / updated_at / finished_at,
}
```

`MigrationHub` 增加 `get_typechange` / `update_typechange` / `list_typechange`；重启 running→interrupted 覆盖第四类任务；`is_active`/controls 复用。

## 5. 分类器（纯函数，无库单测）

`classify_type_change(source_type: &str, target_type: &str) -> TypeChangeClass`

输入为归一化后的类型串（含 typmod）。白名单（均为 PG 保证不重写的组合）：

| 源 | 目标 | 条件 |
| --- | --- | --- |
| `character varying(n)` | `character varying(m)` | m ≥ n 或 m 不限 |
| `character varying(n)` | `text` | 任意 |
| `text` | `character varying`（不限长度） | 任意 |
| `numeric(p,s)` | `numeric(p2,s2)` | s2 ≥ s 且 (p2−s2) ≥ (p−s) |
| `timestamp[with/without time zone](p)` | 同 zone 属性、精度 p2 | p2 ≥ p |

其余一律 `Rewrite`。解析失败/未收录类型按 `Rewrite` 处理（宁严勿宽，交给开闸门）。

typmod 解析只覆盖 `name` / `name(n)` / `name(p,s)` 三种形态，带空格的别名先经 `normalize_pg_type` 归一。

## 6. 执行器 `pg_typechange.rs`

阶段机：`Preflight → Applying → Verifying → Done`，每阶段幂等；状态/信号/审计复用 S3 模式。

### 6.1 Preflight（每项失败即 Blocked，phase 记 `preflight:*`）

1. 引擎为 PG；schema 解析（缺省 `public`）；表存在且 `relkind='r'`（分区表/视图/外表 BLOCK，S5）。
2. 列存在；`format_type(atttypid, atttypmod)` 得 live 类型：
   - 首跑：写入 `source_type` 与 `spec_hash`；
   - 续跑：live 类型 ≠ `source_type` 时——若已等于目标类型（归一化比较），视为中断期间已完成，跳 `Verifying`（自愈）；否则 BLOCK `preflight:schema_drift`。
3. 目标类型合法性：`SELECT $1::regtype` 解析失败 → BLOCK `preflight:bad_target_type`；归一化后等于源类型 → no-op 直接 `Done`。
4. 分类：`MetadataOnly` 直接放行；`Rewrite` 且 `!allow_rewrite` → BLOCK `preflight:rewrite_requires_gate`（notes 附表行数/大小与维护窗口建议）。
5. 非确定性转换硬阻断（即使开闸）：`timestamp without time zone`↔`timestamp with time zone`、`date`→`timestamp with time zone` → BLOCK `preflight:non_deterministic_cast`。
6. 依赖检查：
   - 依赖该列的视图/规则（`pg_depend` 沿 `refobjid`）→ BLOCK `preflight:dependent_views`；
   - 列参与任何方向的外键 → BLOCK `preflight:foreign_keys`（附约束名清单）；
   - 列在主键/唯一索引中：`MetadataOnly` 放行（索引不重建）；`Rewrite` 放行但 notes 记录索引重建成本（FK 引用已被上一条拦下）。
7. 属主校验：`pg_has_role(current_user, relowner, 'USAGE')` 或超级用户 → 否则 BLOCK `preflight:not_owner`。
8. 记录前置快照：行数、`pg_relation_size`、列默认值表达式、列上索引清单（供 verify 比对）。

### 6.2 Applying

- 单语句自动提交，复用（并收敛）S3 的 DDL 执行助手：session 级 `SET lock_timeout = policy.lock_wait_timeout_secs`、`SET statement_timeout = '0'`，执行后 RESET。S3 里 `pg_notnull::exec_ddl` 与 `pg_index::exec_index_ddl` 已同构，本期收敛为共享 `pub(crate) exec_ddl_auto_commit`，三个执行器共用（行为不变的重构）。
- SQL：`ALTER TABLE {qualified} ALTER COLUMN {col} TYPE {target} USING {col}::{target}`（`col` 与表名走 `quote_ident_pg`；`target` 已经 `::regtype` 验证，按原样拼接）。
- 错误映射（PG SQLSTATE/message）：
  - `55P03 lock_not_available` → Blocked `apply:lock_timeout`（可续跑）；
  - `22P02/22003/22007/22008` 等转换失败 → Blocked `apply:conversion_failed`，detail 带 PG DETAIL（违规值）；
  - `42804/42846` 无可行 cast → Blocked `apply:no_cast`；
  - `2BP01` 依赖对象 → Blocked `apply:dependent_objects`；
  - `42501` → Blocked `apply:insufficient_privilege`；
  - 连接中断/其他 → `Interrupted`（可续跑；续跑经 Preflight 自愈或重跑语句）。
- `execution = { atomic:false, rollback_scope:"statement", resumable:true }`：PG DDL 失败原子回滚列本身，无半成品；「续跑」语义 = 重跑语句。

### 6.3 Verifying

1. live 类型归一化后等于目标类型；
2. 行数等于前置快照行数；
3. 表上全部索引 `indisvalid=true`（重写会重建索引，验证健康）；
4. 记录列默认值表达式变化（只记录，不一致不判失败——PG 会自动重 cast，失败在 Applying 已报错）。
全部写入 `job.verify`，完成 → `Completed`；任一不符 → Blocked `verify:mismatch`。

### 6.4 控制信号与审计

- pause/abort 在阶段边界生效（Applying 单语句不可中途暂停，与 S3 一致）；abort → `Aborted`。
- 审计事件：`field_migration.typechange.created` / `.started` / `.blocked` / `.completed` / `.aborted`，payload 带 job_id、表、列、源/目标类型、classification、耗时。

## 7. HTTP 端点（web-server）

镜像 S3 的 notnull/index 形态，两套 router 同步注册：

- `POST /ops/field-governance/type-change`：`{ db_id?, schema?, table, column, target_type, allow_rewrite?, production_confirmed? }` → 创建 job（Preflight 在 run 时做；create 只做入库与指纹）。
- `POST /ops/field-governance/type-change/:job_id/run`：后台执行；`authorize_write` + `ensure_scenario_engine("field-governance")` + `policy.authorize_backfill(read_only, production_confirmed)`。
- `GET /ops/field-governance/type-change/:job_id`：状态（Running 且非 active 时落 Interrupted，同 S3）。
- `GET /ops/field-governance/type-change`：列表（limit 1–100，默认 20）。

## 8. 测试

### 8.1 单测（无库）

- `classify_type_change`：白名单全组合（含 typmod 边界：m=n、m 省略、p2−s2 恰好相等）、非白名单若干、未收录类型按 Rewrite、别名归一（varchar→character varying）。
- `MigrationStore`：typechange 回环、老文件（无该字段）反序列化、空映射不序列化字节不变。
- 错误映射函数：SQLSTATE → phase/reason 映射表。

### 8.2 真库 `core_lib/tests/field_migration_typechange_pg.rs`（`FIELD_MIGRATION_PG_URL`，独立临时库用完即删）

| 用例 | 断言 |
| --- | --- |
| `metadata_only_varchar_grow` | varchar(50)→varchar(200)，class=MetadataOnly，未开闸即完成；atttypmod 更新；行数/内容不变 |
| `metadata_only_varchar_to_text` | varchar(50)→text 完成，verify 全绿 |
| `metadata_only_numeric_relax` | numeric(10,2)→numeric(14,4) 完成（class=MetadataOnly） |
| `rewrite_gate_blocks` | text→varchar(10) 未开闸 → Blocked `preflight:rewrite_requires_gate`，列未变 |
| `rewrite_int_to_bigint` | int4→int8，开闸后完成；数据不变；主键索引 indisvalid；行数一致 |
| `rewrite_conversion_failure` | 含 'abc' 的 text→int4 → Blocked `apply:conversion_failed`（DETAIL 含违规值）；列保持 text；修复数据后 re-run 完成（resume=重跑语句） |
| `dependent_view_block` | 视图引用该列 → Blocked `preflight:dependent_views` |
| `foreign_key_block` | 列为被引用 PK → Blocked `preflight:foreign_keys`，detail 含约束名 |
| `non_deterministic_block` | timestamp→timestamptz 开闸也 Blocked `preflight:non_deterministic_cast` |
| `resume_self_heal` | 外部把列改成目标类型（模拟中断期完成）→ run 自愈跳 Verify 完成 |
| `lock_timeout_mapping` | 另一会话持 ACCESS EXCLUSIVE → Blocked `apply:lock_timeout`；释放后 re-run 完成 |
| `spec_hash_mismatch` | 篡改 job 的 target_type 后续跑 → 拒绝（Blocked `preflight:spec_mismatch`） |
| `not_owner_block` | 非属主角色 → Blocked `preflight:not_owner` |

节点报数：真库用例跑通后报一次数（本期待办唯一节点）。

## 9. 决策表（评审点）

| # | 决策点 | 现状提案 | 备选 |
| --- | --- | --- | --- |
| 1 | 免重写白名单范围 | 只收 PG 文档保证不重写的 5 类组合，解析失败按 Rewrite | 扩大收录（需逐类真库验证 relfilenode 不变） |
| 2 | 重写路径续跑语义 | 失败原子回滚列本身，续跑=重跑语句；中断期外部完成则自愈跳 Verify | 实现语句级进度跟踪（PG 无 ALTER TYPE 进度视图，不可行） |
| 3 | `USING` 表达式 | v1 固定 `col::target`，不支持自定义 | 开放 freeform USING（SQL 注入面 + 不可校验，不建议） |
| 4 | FK/视图依赖 | 一律 BLOCK，列清单进 detail | 自动级联处理（风险大，S5 再议） |
| 5 | `timestamp`↔`timestamptz` | 硬阻断，S5 出显式 `AT TIME ZONE` 方案 | 用 session TimeZone 执行（不确定性，拒绝） |
| 6 | 验证口径 | 类型匹配 + 行数一致 + 索引 indisvalid + 默认值快照记录 | 抽样逐值比对（成本与收益不匹配） |
| 7 | DDL 助手重构 | 收敛 S3 两份同构 `exec_ddl` 为共享函数（行为不变） | 再复制一份（三份漂移风险） |
| 8 | create 是否预跑 Preflight | 不预跑，Preflight 在 run 时做（与 S3 一致） | create 即 Preflight（创建即报错但增加一次库往返） |

## 10. S5/S6 边界清单

**S5（覆盖面）**：复合主键 keyset 分页；分区表（逐分区/父表 DDL 语义）；部分/表达式唯一索引作为 ON CONFLICT 目标；freeform USING 表达式（含 `timestamp`↔`timestamptz` 显式方案）；FK 感知改类型；`INCLUDE`/opclass。
**S6（运维化）**：abort/pause 触发 `pg_cancel_backend`；进程重启孤儿连接清理；并发限速公平性与全局配额；索引进度历史持久化。
**继续挂起**：跨实例快照一致性；目标结构漂移细粒度指纹；大表流式读写；前端表单；改 collation 执行；`citext`/非确定性排序规则文本主键。

## 11. 收尾要求（同 S2/S3）

- `scripts/test-summary.py` 守卫加入 `field_migration_typechange_pg`。
- fmt 只作用于本期改动文件；全仓 fmt 不重排（S2 既定结论）。
- 三套口径：无库 / MySQL(33306)+PG / MariaDB(33307)+PG，均 `--test-threads=1 --no-fail-fast`，报 `test-summary.py` 数字。
- 不 push、不 `#[ignore]`、不删用例、改断言写依据；MySQL/MariaDB 行为、SQL、任务 JSON 零变化。

## 12. 实现期发现（与方案的偏差）

1. **SQLSTATE 提取**：`OpsError` 将 sqlx 错误拍平为字符串，SQLSTATE 丢失；共享助手拆为
   `exec_ddl_raw`（返回原始 sqlx::Error）+ `exec_ddl_auto_commit`（包装），notnull/index
   走包装版本行为不变，typechange 用 raw 版本读 code/DETAIL。
2. **带 typmod 的别名**：`normalize_pg_type` 只处理整串别名，`decimal(10,2)`、
   `timestamp(3)` 不被展开；分类器内加 `expand_alias_with_typmod`（decimal/dec→numeric、
   timestamptz(p)→timestamp(p) with time zone、timestamp(p)→timestamp(p) without time zone、
   char(n)→character(n)）。
3. **目标类型重构而非拼接**：SQL 中的目标串来自解析产物 `render_target`（base 经 `::regtype`
   参数化验证、typmod 只含数字），timestamp 的 typmod 放回词中间（`timestamp(6) with time zone`），
   杜绝原始串注入面。
4. **锁超时用例持 SHARE 锁而非 ACCESS EXCLUSIVE**：preflight 的行数快照 `COUNT(*)` 也要
   ACCESS SHARE，持 ACCESS EXCLUSIVE 会把 COUNT 卡成永等（COUNT 无超时）；SHARE 只挡
   ALTER 需要的 ACCESS EXCLUSIVE，正好命中要验证的 `apply:lock_timeout` 映射。
   记录为产品侧已知缺口：preflight 快照查询本身无 lock_timeout，S6（cancel/超时治理）处理。
5. **numeric 缩放后的 text 输出**：`numeric(10,2)→numeric(14,4)` 后 `::text` 按 scale 补零
   （`12345.67`→`12345.6700`），值本身不变；用例以数值相等断言而非文本相等。
6. **首跑记录 source_type 的时机**：在分类/开闸判定**之前**记录并写入指纹——
   Rewrite 未开闸阻断的任务也已绑定源类型与指纹，后续自愈（外部完成→no-op）与
   指纹篡改拒绝用例都依赖这一点。
