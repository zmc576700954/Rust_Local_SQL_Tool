# PG 字段治理 S1 规格（只读证据 + 风险判定 + 脚本生成）

状态：待批准（批准后才进入实现）
范围：S1 only。S2 写路径、S3 能力位/前端翻转不在本规格内。

## 1. 目标

在现有 MySQL/MariaDB 字段治理模块上补 PostgreSQL 只读分支，做到：

- E01–E14 在 PG 上按计划来源落地；`UPDATE_TIME`、`INNODB_TRX`、表级字符集/表级排序规则明确标「PG 不适用」并给可读原因，不用近似指标顶替。
- 只读门按引擎分派：MySQL 用 `MySqlDialect` + `mysql_rows::fetch_rows`，PG 用 `PostgreSqlDialect` + `pg_rows::fetch_rows`。
- `numeric` / `bytea` / `timestamptz` 由 `pg_rows` 的 `pg_cell_to_value` 按类型格式化，不统一 `try_get::<String>`。
- 风险三档沿用；PG 特有破坏性场景（改类型重写、`SET NOT NULL` 全表校验、`USING` 转换失败、改排序规则重写、分区表）全部落地。
- 脚本走 PG 方言（`->` / `->>`、`jsonb_typeof`、`ON CONFLICT`、`EXCLUDED`、`encode(convert_to(...))`），只生成不执行。
- E09 按 `server_version_num` 选路：PG16+ 用 `pg_input_is_valid`；低版本客户端采样并记 gap，不用正则近似。
- 能力位不动：`engines` 仍为 mysql/mariadb，前端入口保持不可点。

## 2. 已核实的事实（实现前提）

| 事实 | 位置 |
| --- | --- |
| `OpsContext::is_mysql_family()` / `is_postgres()` 已存在 | `core_lib/src/ops/registry.rs:81,88` |
| `pg_rows::fetch_rows` 为 `pub(crate)`，内部用 `pg_cell_to_value` 处理 numeric/bytea/timestamptz | `core_lib/src/ops/pg_rows.rs:16` |
| `quote_ident_pg` 已存在 | `core_lib/src/sql/util.rs:44` |
| `is_read_only_statement` 覆盖 Query/Explain/Show 等 | `core_lib/src/sql/util.rs:1708` |
| `index_advisor::load_snapshot_public` / `load_table_stats_public` 已有 PG 分支 | `core_lib/src/ops/index_advisor.rs` |
| `analyze` 顺序：collect → risk → scripts → retire | `core_lib/src/ops/field_migration/mod.rs:267` |
| `collect` 目前硬拒非 MySQL | `core_lib/src/ops/field_migration/evidence.rs`（`collect` 首段） |
| `scripts::generate` 全 MySQL 方言 | `core_lib/src/ops/field_migration/scripts.rs` |
| `retire::build` 纯本地计算，无 DB 查询 | `core_lib/src/ops/field_migration/retire.rs:22` |
| 场景 descriptor `engines=[mysql,mariadb]`，不改 | `core_lib/src/ops/field_migration/mod.rs:48` |
| 真库：`phase3-pg` 127.0.0.1:55432，PostgreSQL 16.15，ICU 可用 | 本机容器实测 |
| MySQL E01–E14 用例基线 | `core_lib/tests/field_migration_mysql.rs` |

## 3. 设计：最小侵入的 PG 分支

新增 3 个模块，避免改动庞大的 MySQL 主体：

```text
core_lib/src/ops/field_migration/
  pg_evidence.rs   // PG 的 E01–E14 采集 + PG 依赖扫描
  pg_risk.rs       // PG 特有风险规则（在通用规则之后追加）
  pg_scripts.rs    // PG 方言脚本 + dry-run
```

`mod.rs` 增加 `pub mod pg_evidence; pub mod pg_risk; pub mod pg_scripts;`。

### 3.1 分派点（只改 3 处入口）

1. `evidence::collect`
   - 保留 MySQL 逻辑不动。
   - `if ctx.is_postgres() { return pg_evidence::collect(ctx, input, policy).await; }`
   - 错误文案从「仅支持 MySQL / MariaDB」改为「仅支持 MySQL / MariaDB / PostgreSQL」。
2. `scripts::generate`
   - 函数开头：`if analysis.environment.engine_family == "postgresql" { return pg_scripts::generate(snapshot, analysis, input); }`
   - MySQL 路径零改动。
3. `risk::evaluate`
   - 末尾：`if analysis.environment.engine_family == "postgresql" { pg_risk::evaluate(analysis, input, policy); }`
   - 通用规则中仅在 PG 下跳过 `E02_NOT_INNODB`（按 `engine_family` 判断），其余通用规则复用。
4. `read_only.rs`
   - 新公开函数：
     ```rust
     pub fn validate_read_only_sql(
         db_type: crate::config::DbType,
         sql: &str,
     ) -> Result<(), OpsError>
     ```
     按 `db_type` 选 `MySqlDialect` / `PostgreSqlDialect`，解析后要求单条且 `is_read_only_statement`。
   - `fetch_read_only` 先调 `validate_read_only_sql(ctx.db_client.db_type(), sql)`，再按引擎分派 `mysql_rows::fetch_rows` / `pg_rows::fetch_rows`。
   - 该公开函数供集成测试直接验证「拒绝写语句」，不暴露执行能力。

## 4. 数据结构改动（types.rs，全部 `#[serde(default)]` 兼容旧证据）

`EnvironmentEvidence` 追加：

```rust
pub engine_family: String,                 // "postgresql"；空=旧 MySQL 证据
pub schema: Option<String>,                // PG current_schema()
pub server_version_num: Option<u64>,
pub table_kind: Option<String>,            // pg_class.relkind
pub nondeterministic_collation: bool,
pub pg_authid_readable: Option<bool>,
pub no_password_roles: Option<u64>,
```

`DependencyEvidence` 追加：

```rust
pub notes: Vec<String>,                    // 例：routines 文本匹配为下界
```

`ColumnProfile` 追加：

```rust
pub pg_cast_invalid_rows: Option<u64>,     // USING 预检；None=未预检
pub pg_input_check: Option<String>,        // "pg_input_is_valid" / "client_sample" / "not_applicable"
```

`ColumnProfile::is_text_like` 增加 `jsonb` / `bpchar` / `name` / `citext`；`is_json_type` 同时识别 `json` 与 `jsonb`。
`schema::parse_type_length`（risk 内部实际函数名以现有实现为准）扩展识别 `CHARACTER VARYING(n)`。

## 5. E01–E14 的 PG 来源与「不适用」标注

| 证据 | PG 来源 | 说明 |
| --- | --- | --- |
| E01 | `information_schema.columns` + `pg_attribute` + `format_type(atttypid,atttypmod)` + `attidentity`/`attgenerated` | `data_type` 取 `udt_name`（varchar/text/jsonb/numeric/timestamptz…），`column_type` 取 `format_type`；`COLUMN_TYPE`/`CHARACTER_SET_NAME` 不适用 |
| E02 | `information_schema.tables` + `pg_class.relkind/relpersistence` + `pg_partitioned_table` + `pg_inherits` | `engine`/`row_format`/表级 collation 标 N/A；`partitioned` = 分区父表或分区子表 |
| E03 | `pg_index` + `pg_get_indexdef` + `pg_constraint` | 主键/唯一/普通索引、表达式索引；`PRIMARY` 名称概念不适用 |
| E04 | `pg_constraint contype='f'` + `generate_subscripts(conkey)` | 子表/父表双向 |
| E05 | `pg_attrdef` + `pg_depend` + `pg_attribute.attgenerated` | 列级生成列；经函数的间接依赖如未记录在 `pg_depend` 则在 notes 标注边界 |
| E06 | 视图：`pg_rewrite` + `pg_depend` 精确列级；触发器：`pg_trigger` + `pg_get_triggerdef`；例程：`pg_proc` + `pg_get_functiondef` 文本匹配 | 例程匹配在 `DependencyEvidence.notes` 标「文本匹配=下界」 |
| E07 | `reltuples` + `pg_table_size`/`pg_indexes_size` + 有界 `COUNT(*)` | `DATA_FREE`、`UPDATE_TIME` 标 N/A（原因：PG 无碎片字段/无表级更新时间元数据） |
| E08 | `COUNT(*) FILTER` + `length`/`octet_length`/`btrim`/`COUNT(DISTINCT)` | 空串 `''` 与 NULL 分开；`char(n)` 空格填充写入 notes |
| E09 | PG16+：`pg_input_is_valid(col::text,'jsonb')`；PG<16：客户端采样 + gap | 决策函数 `e09_mode(server_version_num)` 单测两分支 |
| E10 | `jsonb_typeof(col::jsonb)` + `jsonb_object_keys` LATERAL | jsonb 会归一化重复 key/顺序/数字，写 notes |
| E11 | `jsonb_typeof(col::jsonb -> 'key')` | number 统一映射 `DOUBLE`（jsonb 不保留 integer/double 区分），写 notes |
| E12 | `length`/`octet_length`/`percentile_cont(0.5)` | sampled 用 `hashtext` 确定性分片 |
| E13 | DB 编码 + 列 collation + `COUNT(DISTINCT c COLLATE "C")` + `btrim` + `~ '[[:cntrl:]]'` | 表级字符集/表级排序规则标 N/A；列级 collation 用 `pg_collation.collisdeterministic` 判定 |
| E14 | `pg_stat_activity` 长事务、`pg_is_in_recovery()`、`default_transaction_read_only`、`version()`；授权用 `information_schema` + `has_table_privilege` | `INNODB_TRX` 标 N/A；长事务改由 `pg_stat_activity` 提供并注明语义不同；`pg_authid` 仅超级用户可读，否则 `pg_authid_readable=false` + E14 gap，不返回 0 |

不适用项统一进入对应 `EvidenceItem.details["not_applicable"] = [{ "item": "...", "reason": "..." }]`，不生成替代指标。

## 6. 只读 SQL 模板（PG）

- 采样（有单列主键）：`WHERE mod(abs(hashtext(CAST(pk AS text))::bigint), 1000) < n`；无主键用 `ctid` 同样哈希。
- E08 聚合：
  ```sql
  SELECT COUNT(*) AS rows_profiled,
         COUNT(*) FILTER (WHERE c IS NULL) AS null_rows,
         COUNT(*) FILTER (WHERE c IS NOT NULL AND CAST(c AS text) = '') AS empty_rows,
         MAX(length(CAST(c AS text))) AS max_chars,
         MAX(octet_length(CAST(c AS text))) AS max_bytes,
         AVG(length(CAST(c AS text))) AS avg_chars,
         COUNT(*) FILTER (WHERE c IS NOT NULL AND CAST(c AS text) <> btrim(CAST(c AS text))) AS untrimmed_rows,
         COUNT(*) FILTER (WHERE c IS NOT NULL AND CAST(c AS text) ~ '[[:cntrl:]]') AS control_rows,
         COUNT(DISTINCT CAST(c AS text)) AS distinct_rows,
         COUNT(DISTINCT CAST(c AS text) COLLATE "C") AS distinct_binary_rows
  FROM <source>
  ```
- E09 PG16+：
  ```sql
  SELECT COUNT(*) FILTER (
           WHERE c IS NOT NULL AND btrim(CAST(c AS text)) <> ''
             AND NOT pg_input_is_valid(CAST(c AS text), 'jsonb')) AS invalid_rows,
         COUNT(*) FILTER (
           WHERE c IS NOT NULL AND btrim(CAST(c AS text)) <> ''
             AND pg_input_is_valid(CAST(c AS text), 'jsonb')) AS valid_rows
  FROM <source>
  ```
- E10 顶层类型：
  ```sql
  SELECT CASE WHEN jsonb_typeof(v) = 'object' THEN 'OBJECT'
              WHEN jsonb_typeof(v) = 'array' THEN 'ARRAY'
              WHEN jsonb_typeof(v) = 'null' THEN 'NULL'
              ELSE 'SCALAR' END AS top, COUNT(*) AS n
  FROM (SELECT CAST(c AS jsonb) AS v FROM <source>
        WHERE c IS NOT NULL AND pg_input_is_valid(CAST(c AS text), 'jsonb')) s
  GROUP BY 1
  ```
- E10 key 分布/形状：`jsonb_object_keys(v)` LATERAL；形状用 `string_agg(k, ',' ORDER BY k)` 去重计数。
- E11 类型分布：`jsonb_typeof(v -> 'key')`，映射 string→STRING、number→DOUBLE、boolean→BOOLEAN、null→NULL、object→OBJECT、array→ARRAY。
- 声明唯一性：`jsonb_typeof(v)='object'` 时 `v ->> 'key'` 分组 `HAVING COUNT(*) > 1`。

所有 SQL 仍必须过 `validate_read_only_sql(DbType::PostgreSQL, sql)`。

## 7. 风险规则（PG 追加）

通用规则继续生效（E07/E08/E09/E10/E11/E13/E14、证据 gap 阻断写步骤等），PG 追加：

| code | 级别 | 触发 |
| --- | --- | --- |
| `E02_PG_UNSUPPORTED_RELKIND` | Block | `relkind NOT IN ('r','p')`（视图/外部表等） |
| `E02_PARTITIONED` | Block | 分区父表或分区子表（复用现规则） |
| `E13_PG_NONDETERMINISTIC_COLLATION` | Block | 任一列 collation `collisdeterministic=false` |
| `E13_PG_COLLATION_REWRITE` | Block | `target.collation` 与源列 collation 不同 |
| `E01_PG_TYPE_REWRITE` | Block | 标量映射的 `target_type` 与源 `format_type` 规范化后不同；脚本只出 `ALTER COLUMN TYPE ... USING` |
| `E01_PG_USING_INVALID` | Block | `pg_cast_invalid_rows > 0` |
| `E01_PG_USING_UNVERIFIED` | Block | 类型变更但 `pg_cast_invalid_rows=None`（PG<16 或预检失败） |
| `E08_PG_SET_NOT_NULL_SCAN` | High | 目标列 `required=true` 且源列可空/存在 NULL；脚本给 `CHECK NOT VALID → VALIDATE → SET NOT NULL` |

`E02_NOT_INNODB` 在 `engine_family=="postgresql"` 时不触发。
`USING` 预检只在 PG16+ 执行：`SELECT COUNT(*) FROM <source> WHERE c IS NOT NULL AND NOT pg_input_is_valid(CAST(c AS text), '<target_type>')`；`target_type` 仅允许 `[A-Za-z0-9_ (),]+` 并单引号转义。

## 8. PG 脚本生成（全部 `executable=false`）

- 建表：`CREATE TABLE IF NOT EXISTS "schema"."target" (...)`；类型走 `mysql_to_pg_type`（VARCHAR→character varying、TINYINT(1)→boolean、DECIMAL→numeric、JSON→jsonb、DATETIME→timestamp…）；无 `ENGINE`/`DEFAULT CHARSET`；目标 collation 以列级 `COLLATE "x"` 表达。
- 回填：
  ```sql
  INSERT INTO "schema"."target" ("pk", "col", ...)
  SELECT "pk", <expr>, ...
  FROM "schema"."source"
  WHERE "pk" > :last_pk AND "pk" <= :end_pk
  ORDER BY "pk"
  LIMIT :batch_size
  ON CONFLICT ("pk") DO UPDATE SET "col" = EXCLUDED."col", ...;
  ```
  表达式：
  - JSON → JSON 列：`src::jsonb -> 'key'`
  - JSON → 标量：`src::jsonb ->> 'key'`
  - numeric：`CAST(expr AS numeric)`；timestamptz：`CAST(expr AS timestamptz)`
  - bytea：`encode(convert_to(CAST(expr AS text), 'UTF8'), 'hex')`
  - 存在性保护：`CASE WHEN src IS NOT NULL AND jsonb_typeof(src::jsonb)='object' AND (src::jsonb -> 'key') IS NOT NULL THEN <expr> ELSE NULL END`
- 校验 V1–V4：行数；缺失主键；多余主键；规范化逐行哈希/比对：
  - 文本：`s.src IS DISTINCT FROM t.col`
  - numeric：`CAST(... AS numeric) IS DISTINCT FROM t.col`
  - timestamptz：两侧 `AT TIME ZONE 'UTC'` 后比较
  - bytea：两侧 `encode(..., 'hex')` 后比较
  - JSON：`(src::jsonb -> 'key') IS DISTINCT FROM t.col`（jsonb 规范化相等）
- 回滚：`DELETE FROM ... WHERE "pk" >= :min_pk`、`DROP TABLE IF EXISTS ...`。
- 下线：`ALTER TABLE "schema"."source" DROP COLUMN "col";`（不可逆）。
- 重写类脚本（仅当触发）：`alter-type`（`USING`）、`set-not-null`（三步）、`alter-collation`；全部 `executable=false`，不进入 `OpsPlan.actions`。
- dry-run：幂等校验同时接受 `ON DUPLICATE KEY UPDATE` 或 `ON CONFLICT ... DO UPDATE`；`DROP COLUMN/TABLE` 仍由 `classify_blocked_sql` 标为阻断。

## 9. 测试

新增 `core_lib/tests/field_migration_pg.rs`：

- env gate：`FIELD_MIGRATION_PG_URL`，缺省时测试打印原因并返回（与 MySQL 用例一致，不用 `#[ignore]`）。
- 每次测试创建唯一数据库（连接 admin 到 `postgres`，`CREATE DATABASE`），结束后删除；不改 `phase3-pg` 现有库对象。
- 测试表：
  - `fm_users(id bigint primary key)`
  - `fm_orders(id bigint generated always as identity primary key, user_id bigint not null references fm_users(id), payload jsonb, payload_text text, score numeric(12,4), blob_col bytea, created_at timestamptz, payload_theme text generated always as (payload ->> 'theme') stored, unique(payload_text), index(user_id))`
  - 数据 6 行，覆盖 JSON 对象/数组/标量/null、非法 JSON、空串、前后空格、超长文本、Numeric/bytea/timestamptz。
- 用例：
  1. `payload` jsonb：E01 类型、E05 生成列 BLOCK、E10 对象计数、E11 类型漂移 BLOCK、声明 required_key/expected_keys BLOCK。
  2. `payload_text`：E09 非法 JSON=3（含 update 后的 60 字符行）、E08 空串=1、E13 trim=1、E03 唯一索引、目标长度过小、声明 NULL 冲突。
  3. sampled 模式：`PROFILE_NOT_FULL`。
  4. 整表盘点：不生成脚本，E04 外键 BLOCK（`user_id`）。
  5. `build_plan`：`actions` 为空、脚本全 `executable=false`。
  6. 只读门：`validate_read_only_sql(PostgreSQL, ...)` 接受 SELECT，拒绝 INSERT/UPDATE/DELETE/ALTER/CREATE/TRUNCATE/`INSERT ... ON CONFLICT`。
  7. 类型格式化：分别分析 `score` / `blob_col` / `created_at`，断言无 E08/E12 gap，脚本分别含 `::numeric`、`encode(convert_to(`、`AT TIME ZONE 'UTC'`。
  8. BLOCK：分区表 `fm_partitioned` → `E02_PARTITIONED`；复合主键表 `fm_composite` → `E14_NO_SINGLE_PK`；`CREATE COLLATION fm_nd (provider=icu, locale='und-u-ks-level2', deterministic=false)` 后建表并用该 collation → `E13_PG_NONDETERMINISTIC_COLLATION`。
  9. `pg_authid` 降级：创建 `LOGIN` 非超级用户 `fm_reader`，授权 SELECT；用其 URL 分析，断言 `pg_authid_readable=false`、`no_password_roles` 序列化中缺省（不是 0）、存在 E14 gap 文案。
  10. E09 版本选路：单测 `e09_mode(150000)=ClientSample`、`e09_mode(160000)=PgInputIsValid`；真库按实际 `server_version_num` 断言 profile 的 `pg_input_check` 与版本一致。

`scripts/test-summary.py` 的 `CORE_LIB_TARGET_GUARDS` 增加：

```python
"field_migration_pg": "FIELD_MIGRATION_PG_URL",
```

（键名与脚本现有 target 命名风格保持一致；实现时对照现有字典结构。）

## 10. 验证与验收

实现后依次执行：

1. `FIELD_MIGRATION_PG_URL=postgres://postgres:password@127.0.0.1:55432/postgres cargo test -p core_lib --test field_migration_pg -- --test-threads=1 --no-fail-fast`
2. `FIELD_MIGRATION_PG_URL=... cargo test -p core_lib --test field_migration_mysql -- --test-threads=1 --no-fail-fast`（确认 MySQL 未回归）
3. `python3 scripts/test-summary.py` 三套口径：
   - 无库
   - MySQL + PG（`--test-threads=1 --no-fail-fast`）
   - MariaDB + PG（`--test-threads=1 --no-fail-fast`）
4. `cargo test --workspace`（能跑则跑，作为兜底）

验收标准：

- PG 新用例全绿；MySQL E01–E14 用例全绿。
- 三套口径 0 failed、0 ignored；target 数与 guard 一致。
- `engines` 仍为 `["mysql","mariadb"]`；`dbCapabilities` 等前端不改。
- 所有新增脚本 `executable=false`，无 `ActionSpec` 写动作。
- 每个「PG 不适用」项都有 `not_applicable` 原因，且不出现近似指标。

## 11. 明确留到 S2/S3

S2（写路径）：

- 分批回填执行器、`last_pk` 类型化绑定、PK keyset（不用 ctid）、引擎不匹配拒绝续跑。
- 批级 `rollback_scope` 语义、事务边界、checkpoint 格式。
- V1–V4 在事务快照下的一致性校验。
- 冲突目标索引预检（`ON CONFLICT` 必须有唯一索引）。

S3（能力位/前端）：

- `engines` 增加 `postgres`、`dbCapabilities.ts` 与 ops 入口逐项开放。
- 三引擎对齐文档与界面只读标注。
- 任何能力位翻转以 S2 真库全绿为前提。

## 12. 非目标

- 不修改 `backfill.rs` 的 MySQL 写路径。
- 不新增 PG 写动作、不执行任何脚本。
- 不引入 `#[ignore]`、不删除或放宽现有断言。
- 不改变 MySQL/MariaDB 的既有行为与文案（除 `collect` 的错误提示扩展引擎名）。

## 13. 变更文件清单

```text
core_lib/src/ops/field_migration/mod.rs           +3 模块声明、collect 分派
core_lib/src/ops/field_migration/evidence.rs       collect 允许 PG 并分派
core_lib/src/ops/field_migration/read_only.rs      引擎分派 + 公开校验函数
core_lib/src/ops/field_migration/risk.rs           PG 入口与 InnoDB 规则跳过
core_lib/src/ops/field_migration/scripts.rs        PG 分派
core_lib/src/ops/field_migration/types.rs          新增可选字段 + 类型判定扩展
core_lib/src/ops/field_migration/pg_evidence.rs    新增
core_lib/src/ops/field_migration/pg_risk.rs        新增
core_lib/src/ops/field_migration/pg_scripts.rs     新增
core_lib/tests/field_migration_pg.rs               新增
scripts/test-summary.py                            guard 增加
```

## 14. 批准项

请确认以下决策后再实现：

1. 采用「新增 pg_* 模块 + 3 处分派」而不是把 PG 逻辑塞进现有 MySQL 函数。
2. PG 类型变更一律 `Block`，只出 `ALTER COLUMN TYPE ... USING` 脚本，不生成可执行计划。
3. PG<16 的 E09/E10 文本 JSON 只做客户端采样并记 gap（写步骤不可判定），不引入正则近似。
4. 非超级用户下 `pg_authid` 不可读时返回 `pg_authid_readable=false` + gap，不输出「0 个无密码账号」。
5. 真库测试使用独立临时数据库，不触碰 `phase3-pg` 现有库对象；不新开第二容器（当前 16.15 已覆盖 PG16+ 分支，低版本分支用单测覆盖）。
