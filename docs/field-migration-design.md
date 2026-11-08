# 字段治理 / JSON 拆表迁移设计

> 版本：v1（只读调研；本轮未改任何代码、未动 git、未启动应用）
> 范围：`core_lib/src/ops/`、`core_lib/src/sync/`、`core_lib/src/schema/`、`web-server`、`web-ui`（仅指出落点，不实现）
> 路线图上下文：`docs/ops-agent-roadmap.md` 阶段 2「数据治理」
> 目标场景：开发期字段冗余/语义变更；典型为表 A 的字段 `a.payload` 存 JSON 或长字符串，需要拆到表 B，迁完数据后下线 A 的该字段。

---

## 0. 结论先行

1. **这条链路不能做成「一键执行」。** 字段迁移横跨 L0 分析、L2 建表、L3 回填、L4 删列，现有 `ops::executor::execute` 只放行 `RiskClass::is_auto_executable()`（L0/L1），`ActionSpec` 也只有 `CreateIndex/DropIndex/AnalyzeTable` 三种。强行扩成单次执行会绕过现有安全基线，因此本设计采用**多阶段状态机 + 至少 3 个人工闸门**，`DROP COLUMN` 永远只生成脚本。
2. **风险预判必须是确定性规则，不是模型判断。** 证据由服务端采集，`BLOCK / HIGH / INFO` 由规则引擎计算；LLM 只能解释与排序，不能下调等级。语义描述只作为「待验证约束」参与判断，任何与数据冲突的声明都以数据为准。
3. **现有能力可直接复用的部分有限。** 可复用：`sync::mysql_engine::PkRange`、`AdaptiveChunker`、`row_checksum_expr`、`fetch_rows_in_range_stream`，`schema::ext` 的 `get_foreign_keys`/`get_views`，以及 `ops` 的审批令牌、审计、策略、计划存储、连接执行卫生（`SET SESSION lock_wait_timeout`、超时、`KILL QUERY`）。**不能直接复用**：`RowDataSyncEngine::compare`（两侧表名相同、按 `SELECT *` 的 JSON 全等比较，不支持 A→B 结构差异）、`checksum_rows`（整行 JSON 哈希，跨结构不可比）、`deploy`（把全部语句塞进一个事务，没有分批/checkpoint）、`transfer`（文件→表，且 `Replace` 会 truncate）。
4. **MVP 只做「只读分析 + 风险报告 + dry-run 脚本」。** 即不建表、不写数据、不删列；先验证证据准确性、风险判定与脚本可执行性，再逐步开放 L3 回填和 L4 人工下线包。
5. **明确不支持并拒绝硬做**：无主键/复合主键表、非 InnoDB、分区表、源表带触发器、跨实例/跨库迁移、MySQL < 5.7.8（无原生 JSON）、嵌套 JSON/对象数组的自动建表。遇到这些直接标「不支持/需人工介入」。

---

## 1. 范围、术语与现有能力边界

### 1.1 术语

| 术语 | 定义 |
|---|---|
| 旧列 | 待治理列，记为 `A.c`（A 为源表，c 为列名） |
| 新表 | 承载拆出数据的目标表 B，1:1 标量拆分或 1:N 数组拆分 |
| 1:1 拆分 | 一行源数据 → 一行目标数据（JSON 对象的多个 key 拆成多列） |
| 1:N 拆分 | 一行源数据 → N 行目标数据（如 JSON 数组元素） |
| 兼容期 | 旧列仍存在，旧读/旧写与新表并存的时间窗 |
| 下线 | 对旧列执行 `DROP COLUMN`，本设计**永不自动执行** |
| 列快照 | 下线前按主键把旧列完整复制到影子表，作为唯一可恢复数据源 |
| 剖面（profile） | 对列数据做的统计/采样证据，分 `full` 与 `sampled` 两种 |
| 漂移（drift） | 两次校验之间源数据或目标数据发生变化 |

### 1.2 代码事实边界（本轮已核对）

| 事实 | 位置 | 对本设计的影响 |
|---|---|---|
| `ActionSpec` 仅有 `CreateIndex/DropIndex/AnalyzeTable` | `core_lib/src/ops/types.rs:51-70` | 建表、加列、回填、删列都必须新增类型或只生成脚本 |
| `RiskClass` 分 L0-L4，L4 不自动执行 | `core_lib/src/ops/types.rs:17-44` | 字段下线天然归 L4 |
| 执行器只接受 `is_auto_executable()` 的动作 | `core_lib/src/ops/executor.rs:522-537` | 现有 `execute` 不能承接 L2/L3 |
| 审批令牌只为 L0/L1 动作签发 | `core_lib/src/ops/approval.rs:62-82` | L3 回填需要独立的审批范围，不能放宽现有闸门 |
| L4 SQL 已有硬拦截：`DROP COLUMN/TABLE`、`TRUNCATE`、`MODIFY/CHANGE/RENAME COLUMN` | `core_lib/src/ops/safety.rs:100-186` | 可直接作为字段迁移的最后一道防线 |
| 列证据已有：类型、可空、键、字符集、排序规则 | `core_lib/src/ops/index_advisor.rs:215-289` | 可复用，但缺默认值、最大长度、生成列表达式 |
| 表统计已有：`TABLE_ROWS/DATA_LENGTH/INDEX_LENGTH/UPDATE_TIME` | `core_lib/src/ops/index_advisor.rs:291-311` | 仅估算，不能作为校验事实 |
| FK/视图采集已有；触发器/存储过程列依赖没有 | `core_lib/src/schema/ext.rs:31-220` | 依赖分析需要新建触发器/例程扫描 |
| `RowDataSyncEngine::compare` 两侧使用同一个 `table_name`，按整行 JSON 比对 | `core_lib/src/sync/mysql_engine.rs:74-188`、`702-715` | 不能直接用于 A→B 结构不同的迁移校验 |
| PK 枚举、范围条件、单列 PK 辅助函数是私有函数 | `core_lib/src/sync/mysql_engine.rs:409-620` | 需要 `pub(crate)` 暴露或在新模块重写 |
| `AdaptiveChunker`、`PkRange`、`row_checksum_expr`、`fetch_rows_in_range_stream` 是 public | `core_lib/src/sync/mysql_engine.rs:20-25`、`826-904`、`908-924`、`929-979` | 分块/流式读取可复用；跨结构归一化校验必须新建 |
| `fetch_rows_in_range_stream` 固定 `SELECT *` | `core_lib/src/sync/mysql_engine.rs:826-904` | 按投影列读取需要新增变体 |
| 场景注册中 `field-governance` 目前是 `Disabled` | `core_lib/src/ops/registry.rs:157-165` | 需从只读分析开始启用 |
| 服务端未采集 MySQL 版本 | `core_lib/src` 无 `VERSION()`/`server_version` 调用 | 必须新增版本能力采集，否则无法判断 5.7/8.0 差异 |

### 1.3 本设计的硬边界

- 只支持 **MySQL / MariaDB**；PG/SQLite 在阶段 2 不开放字段迁移。
- 只支持 **同实例、同库**；跨库/跨实例留到后续阶段，且即便后续开放也只能「脚本 + 人工」。
- 只支持 **InnoDB**、**非分区**、**单列主键**（整数或可排序字符串）。
- 源表存在 **触发器** 时，禁止自动回填（触发器副作用与双写顺序不可控）。
- 不自动执行任何 **建表、加列、改类型、改名、删列**；这些只出脚本 + dry-run。
- 任何「无法采集/无权限/无法判定」的证据，对**写步骤**按 `BLOCK` 处理；只读分析可以继续并显式标注「未验证」。

---

## 2. 风险预判模型

### 2.1 证据清单（E01-E14）

所有证据由服务端只读采集，带 `evidence_ref`（SQL + 采集时间 + 采集模式）。`information_schema` 结果只是一部分，数据剖面必须显式标注 `full` 或 `sampled`。

| ID | 证据 | 采集方式（只读） | 关键字段 |
|---|---|---|---|
| E01 | 列定义 | `information_schema.COLUMNS` | `DATA_TYPE/COLUMN_TYPE/CHARACTER_MAXIMUM_LENGTH/NUMERIC_PRECISION/NUMERIC_SCALE/IS_NULLABLE/COLUMN_DEFAULT/EXTRA/CHARACTER_SET_NAME/COLLATION_NAME` |
| E02 | 表定义 | `information_schema.TABLES` + `SHOW CREATE TABLE` | 引擎、行格式、表排序规则、分区标志、`CREATE TABLE` 原文 |
| E03 | 索引与唯一约束 | `information_schema.STATISTICS` | 列是否出现在 `PRIMARY`、唯一索引、普通索引；`SEQ_IN_INDEX` |
| E04 | 外键 | `KEY_COLUMN_USAGE` + `REFERENTIAL_CONSTRAINTS`，双向 | 该列作为子表 FK / 作为父表被引用 |
| E05 | 生成列依赖 | `information_schema.COLUMNS.GENERATION_EXPRESSION` | 依赖该列的虚拟/存储生成列 |
| E06 | 视图/触发器/例程依赖 | `information_schema.VIEWS`、`TRIGGERS`、`ROUTINES`，按列名文本匹配 | 匹配到的对象名；权限不足记为「未验证」 |
| E07 | 表体量 | `TABLE_ROWS`（估算）+ 有界 `COUNT(*)`（超时/上限保护） | 行数、`DATA_LENGTH`、`INDEX_LENGTH` |
| E08 | NULL / 空串比例 | 单次聚合扫描 | `SUM(c IS NULL)`、`SUM(c = '')`、非空非空串计数 |
| E09 | JSON 解析成功率 | 非 JSON 类型列用 `JSON_VALID(c)`；JSON 类型列天然有效 | 合法数、非法数、按行 PK 采样 |
| E10 | JSON 顶层类型与 key 分布 | `JSON_TYPE(c)`、`JSON_KEYS(c)`；8.0 可用 `JSON_TABLE` 展开，5.7 用流式客户端统计 | 顶层类型分布、key 集合、每个 key 出现率 |
| E11 | JSON key 类型漂移 | 对每个 key 取 `JSON_TYPE(JSON_EXTRACT(c, '$.k'))` | 同 key 的类型集合（string/number/bool/object/array/null） |
| E12 | 字符串长度分布 | `CHAR_LENGTH/OCTET_LENGTH` 的 MAX/AVG + 分桶计数 | 最大值、分桶直方图、超阈值行数；P95/P99 仅在 full 模式给出 |
| E13 | 字符集/排序规则/大小写/脏数据 | 列与表排序规则；`COUNT(DISTINCT c)` 与 `COUNT(DISTINCT CAST(c AS BINARY))`；`c <> TRIM(c)`；控制字符正则 | 大小写折叠冲突、前后空白行数、控制字符行数、二进制列标志 |
| E14 | 写入活动/环境 | `UPDATE_TIME`、`information_schema.INNODB_TRX`、`SHOW PROCESSLIST`、`SHOW GRANTS`、`SELECT VERSION()` | 长事务、活跃写入、权限、版本、是否只读副本 |

补充约定：

- **E07 的 `TABLE_ROWS` 是估算值**，不能进入「校验通过」的判定；校验口径只用实际 `COUNT(*)` 与主键集合。
- **E10 的 key 分布必须是 full 模式才可作为执行依据**；`sampled` 模式只用于报告，不能授权 L3/L4。
- 样本采集必须按主键确定性抽取（如 `CRC32(pk) % 1000 = 0`），不能随机后不记录种子；样本值默认脱敏，不写入审计原文。
- 所有证据不可用时（权限、超时、连接中断）都产生 `EvidenceGap`，写步骤按 `BLOCK`。

### 2.2 三档定义与总原则

| 档位 | 含义 | 对执行的影响 |
|---|---|---|
| `BLOCK` | 已有明确证据表明自动迁移会产生不确定/不可逆的数据或结构后果 | 禁止进入自动回填与自动下线；只允许「只读报告 + 人工脚本」 |
| `HIGH` | 不阻断只读分析与 dry-run，但必须由用户针对该项显式确认后才可进入下一写阶段 | 每个 HIGH 项在审批记录中必须有一条确认记录，否则状态机不前进 |
| `INFO` | 已采集且无异常，或仅影响展示/解释 | 不拦截，但必须出现在报告里，避免「没提示=安全」的错觉 |

三条不可模糊的规则：

1. **不可判定 = 写步骤 BLOCK。** 例如触发器列表读取失败、JSON key 统计超时、字符集元数据为空；报告可以继续，但不能执行。
2. **`BLOCK` 不能被用户声明或模型解释抵消。** 用户可以说「这个字段以后只存用户偏好」，但不能因此忽略「存在非法 JSON」「存在外键」这类数据/结构事实。
3. **等级只由规则引擎计算。** LLM 产出的风险项必须合并到规则结果上，最终 `risk_floor` 由服务端重算；低于 floor 的计划直接判 `plan_invalid`。

### 2.3 判定表（默认阈值，写入策略文件后可调）

下表中的 `<n>`、`<p>` 为 `OpsPolicy` 默认值，建议集中放在 `field_migration` 策略段，禁止散落在 UI 文案里。

| 证据 | 条件 | 等级 | 理由与动作 |
|---|---|---|---|
| E01 | 列类型为 `BLOB/BINARY/GEOMETRY` 等无法安全文本解析的类型 | `BLOCK` | 不能做 JSON/字符串语义推断；只出报告 |
| E01 | 目标列长度 < 观测最大长度，或数值精度/标度不足以容纳所有值 | `BLOCK` | 必然截断/溢出 |
| E01 | 目标列长度在 `[90%, 100%]` 最大长度区间 | `HIGH` | 当前不溢出，但余量不足；必须确认是否放大或改用 TEXT |
| E02 | 源表引擎不是 InnoDB | `BLOCK` | 无事务/主键语义，无法安全分批回滚 |
| E02 | 源表是分区表 | `BLOCK` | 分块、DDL 与下线行为不可控 |
| E03 | 该列属于 `PRIMARY` 或任一唯一索引 | `BLOCK` | 拆出/下线会改变唯一性语义；必须人工重建约束 |
| E03 | 该列只在普通二级索引中 | `HIGH` | 下线后查询可能退化；需确认索引处理方案 |
| E04 | 该列参与外键（父或子） | `BLOCK` | 约束/孤儿/级联行为不可自动保证 |
| E05 | 存在生成列依赖该列 | `BLOCK` | 生成列表达式必须人工改写 |
| E06 | 视图/触发器/例程文本命中该列 | `BLOCK` | 依赖对象需人工修改；MySQL 无可靠列级依赖元数据，文本匹配是下限 |
| E06 | 无法读取 `TRIGGERS/ROUTINES/VIEWS`（权限不足） | `BLOCK`（写）/`HIGH`（分析） | 依赖未知，不允许自动下线 |
| E07 | 行数 > `migration_max_rows`（默认 2,000,000）或 `DATA_LENGTH` > `migration_max_bytes`（默认 5 GiB） | `BLOCK`（自动回填） | 仅允许脚本/人工或专用工具；dry-run 可做 |
| E07 | 行数 > 200,000 或 `DATA_LENGTH` > 512 MiB | `HIGH` | 回填耗时长，需维护窗口与限速 |
| E08 | 目标列 `NOT NULL`，且源列 NULL 行数 > 0，且无用户声明的填充规则 | `BLOCK` | 无法决定 NULL 的替代值 |
| E08 | 源列 NULL 行数 > 0 但目标可空 | `HIGH` | 需确认 NULL 语义（SQL NULL vs JSON null） |
| E08 | 空串行数 > 0 | `HIGH` | 空串与 JSON `null`/`{}` 语义不同，必须选策略 |
| E08 | 空串行数 > 20% | `BLOCK`（自动迁移） | 数据语义过度模糊 |
| E09 | 非 JSON 列中 `JSON_VALID=0` 的行数 > 0 | `BLOCK` | 默认不允许猜测修复；只能导出人工处理或隔离表方案 |
| E09 | 用户显式选择「非法行隔离」模式 | `HIGH` | 非法行进入隔离表，仍禁止自动下线 |
| E10 | key 数 > 100 或 key 名无法稳定映射 | `BLOCK`（自动建表） | 结构推断不稳定，需要显式映射 |
| E10 | 出现契约之外的新 key，且目标表没有 residual JSON/旁路存储 | `BLOCK` | 数据会丢失 |
| E10 | 出现契约之外的新 key，但有 residual JSON 存储 | `HIGH` | 需确认 residual 字段保留周期 |
| E10 | 新 key 出现率 > 10% | `BLOCK` | 说明语义边界已变化，应重新设计 |
| E10 | 契约声明为必需 key，但缺失行数 > 0 | `BLOCK` | 声明与数据冲突，不能当事实使用 |
| E11 | 同一 key 出现多种 JSON 类型 | `BLOCK`（有类型目标列）/`HIGH`（residual JSON） | 类型漂移不可静默强转 |
| E11 | 数值超过 i64/DECIMAL(65,30) 可表示范围 | `BLOCK`（有类型目标列） | 精度损失不可接受 |
| E12 | 最大长度可在目标类型内 | `INFO` | 记录分布 |
| E12 | 控制字符行数 > 0 | `HIGH` | 下游文本/索引/日志可能异常 |
| E13 | 列字符集为 `binary` 或 `NULL` | `BLOCK` | 文本语义不可判定 |
| E13 | `COUNT(DISTINCT CAST(c AS BINARY))` > `COUNT(DISTINCT c)` | `HIGH`；若目标要建唯一索引则 `BLOCK` | 排序规则折叠导致「看似不同、实际相等」 |
| E13 | `c <> TRIM(c)` 行数 > 5% | `HIGH` | 脏空白数据影响映射/唯一性判断 |
| E13 | 目标排序规则与源列不同（`_bin` ↔ `_ci` 等） | `HIGH` | 比较语义改变；必须显式确认 |
| E14 | 连接为只读 / 副本 | `BLOCK`（写） | 无法执行回填 |
| E14 | 缺少 `SELECT/INSERT/CREATE/ALTER` 权限 | `BLOCK`（对应步骤） | 执行必然失败 |
| E14 | 存在 > 30s 的长事务 | `HIGH` | DDL 需要 MDL；回填需避开 |
| E14 | 版本 < 5.7.8 且目标依赖 JSON 类型 | `BLOCK` | 无原生 JSON 支持 |
| E14 | 无单列主键（无 PK 或复合 PK） | `BLOCK`（自动回填） | 无法安全分片；人工导出/导入 |
| E14 | 目标表 B 已存在且非空，且不是本计划 checkpoint 的产物 | `BLOCK` | 可能覆盖他人数据 |
| 证据缺口 | 任一 E01-E14 采集失败/超时/权限不足 | `BLOCK`（写） | 未知即不允许写 |

### 2.4 剖面完整性与执行授权

| 模式 | 触发条件 | 可用于执行 | 说明 |
|---|---|---|---|
| `full` | 行数 ≤ `full_profile_max_rows`（默认 5,000,000）且数据 ≤ 10 GiB | 可以，仍需满足其余条件 | 单次聚合扫描 + 8.0 `JSON_TABLE` 或 5.7 流式解析 |
| `sampled` | 超过 full 上限，或用户显式选择 | **不可以** | 报告标注「抽样，未验证」；任何自动写步骤 `BLOCK` |
| `none` | 无权限/连接失败 | 不可以 | 只输出 `EvidenceGap` 清单 |

> 判定意图：抽样只能回答「有没有发现问题」，不能回答「全部数据都没问题」。因此抽样结果**永远不能把 `BLOCK` 降级为 `INFO`**；抽样未发现问题时结论是「未验证」，对应写步骤仍按 `BLOCK`。

### 2.5 用户语义描述如何参与判断

用户描述不是证据，而是一组**待验证约束**。数据结构统一为：

```text
SemanticContract {
  subject: { table, column }              // 必须与证据指向同一对象
  purpose: string                        // 字段未来语义，例如「只存用户偏好」
  expected_shape: { top_level: object|array|scalar, keys: [...] }
  required_keys: [...]
  value_domain: { key: type|enum|range } // 例如 {"theme":"enum(light,dark)"}
  null_policy: { target_nullable: bool, empty_string_means: null|empty|invalid }
  uniqueness: { key_set: [...], expected: bool }
  consumers: [...]
  write_owner: string
  cutover_window: { mode: stop_write|dual_write, hours: n }
  retention_days: n
}
```

处理规则：

1. **进入判断，但不单独产生 `INFO`。** 声明先转成 `Declaration { id, source=user, verified=false }`，只用于：
   - 生成目标表列名和类型的**候选**；
   - 与数据剖面做冲突检查；
   - 生成必须由用户确认的取舍项（切换方式、观察期、保留期）。
2. **数据优先。** 声明与数据冲突时，冲突项按下表定级，且声明不能把 `BLOCK` 降为 `HIGH`：

| 冲突 | 示例 | 判定 |
|---|---|---|
| 声明必需 key 缺失 | 声明 `user_id` 必需，但 3% 行无该 key | `BLOCK`（直到用户修改声明或给出填充规则） |
| 声明外 key 出现 | 声明「只存偏好」，数据出现 `order_id` | 有 residual 存储 → `HIGH`；无 residual → `BLOCK` |
| 声明类型与数据不符 | 声明 `count` 为整数，数据出现 `"3"` 字符串 | 有类型列 → `BLOCK`；residual → `HIGH` |
| 声明可空但数据全非空 | 声明允许 NULL，数据 0 个 NULL | `INFO`（声明更宽松，不构成风险） |
| 声明不可空但数据有 NULL | 数据 2% NULL | `BLOCK` |
| 声明唯一但数据有重复 | key 组合重复 120 行 | `BLOCK` |
| 声明消费者已改造但无法在库内验证 | 「旧接口已下线」 | `HIGH` + 必须人工勾选确认，不能自动判定为已改造 |

3. **声明不得覆盖结构事实。** 即使声明写「这个字段不再被索引引用」，E03/E04/E05/E06 的命中仍然 `BLOCK`；只有重新采集证据证明依赖已移除，才允许降级。
4. **声明进入计划哈希。** `declaration_hash` 与 `evidence_hash`、`plan_hash` 一并绑定审批；声明被修改后旧审批自动失效。
5. **展示口径。** 报告分三块：`已验证`（数据支持）、`声明未验证`（仅用户说法）、`冲突`（数据反对）。任何 `声明未验证` 项在 UI 上不能显示为绿色。
6. **LLM 使用方式。** 声明以结构化 JSON 注入 prompt，并固定标注 `source=user, verified=false`；prompt 硬规则要求：不得把声明写成证据、不得降低风险等级、无法确定时必须输出 `missing_information`。模型输出后，服务端按 2.3 重新计算 `risk_floor` 并以更高者为准。

---

## 3. 迁移策略

### 3.1 迁移模式决策树

字段迁移只有四种可接受模式，按顺序判断，命中即停：

1. **无单列主键 / 复合主键 → 不支持自动迁移。**
   - 仅生成「导出脚本 + 人工导入」；不允许进入回填状态机。
2. **源列在其生命周期内不会被并发修改（用户声明 + 最终漂移校验）→ 在线一次性回填。**
   - 默认不允许自动执行；必须用户显式确认「该列写入方已停用」，且回填后做一次全表漂移复核。
   - 等级：`HIGH`，回填执行前需二次确认。
3. **应用已部署双写（用户声明，无法在库内验证）→ 在线双写 + 回填。**
   - 回填可与线上写入并行；以「B 侧补齐 + 周期性对账」保证收敛。
   - 等级：`HIGH`，进入观察期前必须至少一次全量校验通过。
4. **以上都不满足 → 停写窗口。**
   - 回填可在窗口前完成大部分，窗口内做「冻结 → 增量/全量复核 → 校验 → 切换」。
   - 窗口长度按「复核一遍源表的耗时」估算，而不是按回填总耗时估算。
   - 等级：`HIGH`；窗口未确认时写步骤不前置。

> 默认策略：MVP 只实现「停写窗口」和「列不可变」两种；双写模式只出方案与对账工具，不自动编排应用切换。

### 3.2 阶段顺序（状态机）

```text
analyzed
  → plan_ready            // 只读证据 + 风险 + dry-run 脚本
  → table_created         // 人工执行建表脚本（L2）
  → backfilling           // L3，分批 + checkpoint
  → backfilled
  → verified_1            // 全量/分块校验通过
  → dual_write_ready      // 应用双写完成（外部）
  → delta_backfilled      // 增量补齐
  → verified_2            // 二次校验通过
  → read_switched         // 读路径切到 B（外部）
  → observed              // 观察期，无差异
  → source_frozen         // 旧列停止写入（外部）
  → snapshot_taken        // 旧列快照完成并校验
  → validated_final       // 最终漂移为 0
  → retire_ready          // 只差人工 DROP COLUMN 脚本
```

- `retire_ready` 之后**没有自动动作**；`DROP COLUMN` 由用户拿脚本到数据库执行，工具只记录人工回执与审计。
- 每个状态持久化在 `MigrationStore`，任何一步失败可停在当前状态重试；不允许「跳过校验直接前进」。
- 源表或目标表结构在状态机运行期间发生变化（`snapshot_hash` 改变），状态机自动回退到 `plan_ready` 并失效审批。

### 3.3 分批与幂等

| 项 | 设计 |
|---|---|
| 分片键 | 源表单列主键；不接受业务列或无索引列 |
| 分片方式 | `[start, end]` 闭区间，按主键升序；边界用 `PkRange` 表达 |
| 批次大小 | 初始 1000 行，按 `AdaptiveChunker`（目标 200-500ms/批）动态调整，上限 `migration_chunk_max_rows`（默认 10,000） |
| 单批事务 | 每批一个独立事务；提交成功后才写 checkpoint |
| checkpoint | `{plan_id, source_pk_last, target_rows_written, source_rows_read, chunk_hash, updated_at}`，落盘原子写 |
| 幂等键 | 1:1：目标主键 = 源主键；1:N：目标唯一键 = `(源主键, 元素序号)`；元素序号取 JSON 数组下标，重跑不产生重复 |
| 写入语义 | `INSERT ... ON DUPLICATE KEY UPDATE`，显式列清单；禁止 `REPLACE INTO`（会删除+重插，触发触发器与自增跳号） |
| 重试 | 从 checkpoint 继续；重跑同一区间允许覆盖，但必须重新计算该区间校验和 |
| 限速 | 批次间 `tokio::time::sleep` 可配（默认 0ms，dev 可设 50-200ms）；有 `max_rows_per_second` 上限 |
| 失败处理 | `BLOCK` 类错误（超长/溢出/唯一冲突）立即停；瞬时错误（锁等待/连接中断）按 checkpoint 重试一次，再失败转人工 |
| 连接卫生 | 复用 `ops::executor` 的做法：独立连接、`SET SESSION lock_wait_timeout`、语句超时、记录 `CONNECTION_ID()`，超时后 `KILL QUERY` |

### 3.4 双写 / 回填 / 切换顺序

推荐顺序（MVP 强制）：

1. 建 B（人工脚本，L2）。
2. 回填历史数据（L3，可在线）。
3. **冻结源表写入**（用户操作），再跑一次源表全量校验和，确认回填期间没有新写入被漏掉。
4. 校验通过后切换应用读路径（外部）。
5. 观察期（默认 7 天，可配），期间做周期性对账。
6. 确认无写入旧列后，打列快照。
7. 最终校验（源列 vs 快照；B vs 预期）。
8. 人工执行 `DROP COLUMN`。

有双写能力时，把 3 改为「开启双写 → 增量回填 → 校验 → 切换读 → 观察 → 停止旧列写入」。无论哪种模式，**校验不通过都不得进入 4 及之后的步骤**。

### 3.5 新表 B 的 schema 生成规则

只有「全部通过 2.3 判定且无 `BLOCK`」时才生成自动 schema；否则只生成「待人工补全的草案」，不产出可执行 DDL。

| 规则 | 具体约定 |
|---|---|
| 表名 | 默认 `<源表>_<列名>`；用户可指定；必须通过 `validate_identifier`，长度 ≤ 64，且不与现有对象冲突 |
| 主键 | 原样复制源表单列主键（类型、顺序）；B 的主键/唯一键是该列，不新增自增列 |
| 1:N 表 | 唯一键 `(source_pk, element_index)`；`element_index` 为 JSON 数组下标，类型 `INT UNSIGNED` |
| 标量取值列 | key 名 → 列名：仅允许 `[a-z0-9_]`，长度 ≤ 64；非法/保留字/归一化冲突 → `BLOCK` 并要求显式映射，禁止静默改名 |
| 整型 | 全部值可被 `BIGINT` 表示 → `BIGINT`；否则若在 `DECIMAL(65,30)` 可表示 → `DECIMAL(p,s)`；超界 → `BLOCK` |
| 浮点 | 使用 `DECIMAL(p,s)`，`p = 最大整数位 + 最大小数位 + 1`，上限 65/30；超限 → `BLOCK` |
| 字符串 | `max_char_length ≤ 255` → `VARCHAR(ceil(max*1.2))`；≤ 4096 → `VARCHAR` 或 `TEXT`；超过或含控制字符 → `TEXT`；始终按实际最大值校验 |
| 布尔 | 仅当所有值都是 JSON boolean → `TINYINT(1)`；混入数字/字符串 → `BLOCK`（有类型列）或 residual JSON |
| 时间 | 仅当声明明确且所有值用**同一格式**可解析 → `DATETIME`；否则不自动识别，保留字符串/residual |
| NULL | 有 NULL 且目标允许 → 列可空；有 NULL 且声明不允许 → `BLOCK`；全部非空 → `NOT NULL` |
| 数组 | 标量数组 → 1:N 子表；对象数组/嵌套对象 → **MVP 不自动建表**，进 residual JSON 或人工映射 |
| residual | 仅当存在未知 key、类型漂移或用户要求保留原文时生成 `residual_json JSON`；不默认添加 |
| 字符集/排序规则 | 文本列继承源列字符集/排序规则；源为 binary → `BLOCK`；显式写 `CHARACTER SET`/`COLLATE`，不依赖库默认 |
| 引擎 | `ENGINE=InnoDB`；不设分区、不设外键 |
| 索引 | 只建主键/唯一键；二级索引默认不建，验证通过后再用现有 `CreateIndex` 流程补 |
| 元数据列 | 默认不加 `_src_hash`、`_migrated_at` 等列，避免污染业务表；迁移状态由 checkpoint 与审计承载 |
| 已有 B | 只有空表或同一计划 checkpoint 续跑才允许；否则 `BLOCK` |

### 3.6 旧字段/旧表的兼容方式取舍

| 方式 | 适用 | 风险 | 本设计结论 |
|---|---|---|---|
| 保留旧列只读 | 大多数场景；旧消费方可改代码 | 数据库层无法阻止旧写；旧代码继续写会导致新旧不一致 | **MVP 默认**：生成「旧列只读」发布检查清单，不建触发器 |
| 兼容视图 | 只读报表/旧查询需要临时兼容 | MySQL 视图包含 `JSON_OBJECT` 等表达式后不可写；旧 ORM 写入会失败 | 只生成只读视图脚本，标注「不可写」；不自动创建 |
| 触发器双向同步 | 旧写方短期无法改造 | 每次写增加开销；死锁/递归/顺序问题；触发器本身是 DDL；需要版本管理 | 不作为 MVP 自动化路径；仅生成脚本并强制人工评审 |
| 旧表整体保留 | 旧消费方未迁移 | 双份数据长期不一致；存储成本 | 允许作为过渡，但必须在观察期后给出下线计划 |

结论：**优先保持旧列存在且只读，不做自动视图/触发器**；视图与触发器只作为人工可选产物，且必须在下线前删除/替换。

---

## 4. 数据校验口径

### 4.1 校验分层

校验必须由低到高逐层执行；任一层失败都停止，不允许「低层过了就认为高层一定过」。

| 层 | 校验项 | 通过标准 | 失败处理 |
|---|---|---|---|
| V1 | 覆盖行数 | 源端有效行数（按 NULL/空串策略过滤后）= 目标端应有的行数；1:N 为目标元素总数 | 直接 `FAIL`，定位缺失区间 |
| V2 | 主键集合 | 源端主键集合 = 目标端主键集合（1:N 为 `(pk, element_index)` 投影集合） | 输出缺失/多出主键列表（有界） |
| V3 | 逐行归一化哈希 | 两侧同一主键的规范哈希相等，逐行计数为 0 差异 | 进入 V4 定位 |
| V4 | 抽样人工复核 | 确定性抽样 + 区间边界行，逐字段与语义契约核对 | 标 `WARN` 或 `FAIL`，由用户裁决 |
| V5 | 分类计数 | SQL NULL / 空串 / JSON null / 对象 / 数组 / 标量 的计数与源端策略一致 | 直接 `FAIL` |

### 4.2 归一化规则（必须确定性）

校验不是比较「数据库里的原始字符串」，而是比较**按同一规则构造的规范对象**：

1. **空值标记**：SQL NULL 编码为 `N`；字符串 `""` 编码为 `S0`；JSON `null` 编码为 `J:null`。三者不可互换，除非用户显式选择了 `null_policy`。
2. **字符串**：按 UTF-8 原字节，不做 trim、不做大小写折叠、不做 Unicode 归一化；长度前缀编码，避免分隔符碰撞。
3. **整数/小数**：使用 JSON 数字原始词元（保留精度），规范化为十进制字符串：去指数、去前导零、去无意义尾零；禁止先转 `f64`。为此需启用 `serde_json` 的 `arbitrary_precision`，或从 MySQL 以字符串读取数字后再规范化（**不能复用 `mysql_rows` 的 `try_get::<String>` 路径做数值比较**，它会把类型信息抹平）。
4. **布尔**：`true→b1`，`false→b0`；数字 `1/0` 不视为布尔，除非契约声明。
5. **对象**：key 按字节序排序后递归编码；数组保持原顺序。
6. **行对象**：按 `(pk, element_index)` 排序后，逐字段拼接；目标列在 mapping 中缺失的字段视为 `N`，源端 key 缺失也视为 `N`（但「缺失」与「显式 null」要在报告中分开计数）。
7. **哈希**：单行使用 SHA-256 对规范编码取前 128 位；分块哈希使用「按主键排序后的行哈希再 SHA-256」，给出 64 位十六进制摘要。`CRC32` 只允许作为**预筛选**，不能作为最终一致结论（32 位碰撞不可忽略）。

### 4.3 分块比对与差异定位

- 每个分块先比较 `count + 分块摘要`；相等则跳过，不落行级数据。
- 分块不相等时，在该分块内（行数受 `migration_chunk_max_rows` 限制）按主键拉取两侧投影，构造两侧 `HashMap<pk, 规范行>`：
  - 源有、目标无 → `missing_target`
  - 目标有、源无 → `extra_target`
  - 两侧都有但哈希不同 → `value_mismatch`，再逐字段比较，输出**字段名 + 类别**（如 `type_drift`、`null_vs_empty`、`length_overflow`、`value_diff`）。
- 单次报告最多保留 `max_mismatch_details`（默认 100）行明细；其余只计数并给主键区间。明细不落原始敏感值，只落哈希/类别/主键；用户显式开启「显示原值」时才允许在本地 UI 展示，且不写入审计。
- 1:N 比对按 `(pk, element_index)`；若源数组长度与目标元素数不一致，先报 `cardinality_mismatch`，再列出缺/多的下标。
- 定位算法优先在分块内内存比对，而不是反复 `LEFT JOIN`；避免大表连接产生额外扫描与锁。

### 4.4 校验通过条件与门禁

一个迁移阶段只有同时满足以下条件，才能标记 `verified`：

1. V1-V3、V5 全部通过，`total_mismatch = 0`；
2. 源端与目标端的剖面口径一致（同一 `null_policy`、同一 mapping、同一字符集规则）；
3. 校验期间源表写入已冻结（或双写已开启并已完成至少一次增量收敛）；
4. 校验使用的源快照哈希与计划中 `evidence_hash` 对应的结构一致；
5. 审计已成功写入（fail closed）。

强制门禁：

- `verified` 之前禁止执行 `read_switched`、`snapshot_taken`、`retire_ready`。
- 任何 `BLOCK` 风险项存在时，即使校验通过也不允许推进到下线准备。
- 校验失败后不允许自动重跑「破坏性步骤」；只能重跑只读校验或从 checkpoint 继续回填。
- 如果校验期间检测到源数据漂移（两次源端摘要不同），本次校验作废，必须重新在冻结状态下校验。

### 4.5 最终下线前校验

`DROP COLUMN` 之前必须再做一次「三向确认」：

| 方向 | 内容 | 失败后果 |
|---|---|---|
| 源列 → 快照 | 快照表按主键/分块哈希必须与旧列完全一致 | 不允许下线 |
| B → 预期 | B 与 mapping/契约一致，且无 `BLOCK` 遗留 | 不允许下线 |
| 漂移 | 快照之后旧列没有新增/变更（源端摘要不变） | 重新打快照并重新校验 |

---

## 5. 回滚设计

### 5.1 备份方式取舍

| 方式 | 优点 | 缺点 | 采用 |
|---|---|---|---|
| 迁移前备份整表 | 恢复最简单 | 空间/时间大，且与本次迁移无关的列也被复制 | 不默认；仅在用户要求时生成 |
| 备份旧列（按主键快照） | 只复制必要列，可精确恢复该字段 | 需要额外表与空间；只覆盖快照时刻 | **下线前强制** |
| 反向脚本（B→A） | 不占额外存储 | 依赖 B 仍保留全部语义；B 有损映射时不可逆；新写入无法还原 | 仅作为辅助，不替代快照 |
| 保留旧列只读 | 无需恢复即可回切 | 旧列数据可能过期 | 兼容期默认 |

### 5.2 回滚时间窗与前提

| 阶段 | 可回滚方式 | 前提 | 新写入处理 |
|---|---|---|---|
| 建 B 后、回填前 | 停止任务，人工 `DROP TABLE B`（L4 脚本） | B 为空或确认无消费方 | 无新写入到 B |
| 回填中 | 停止任务，保留 checkpoint；必要时人工删 B | 源表未切读/未切写 | 回填可重跑 |
| 回填完成、切换前 | 人工删 B 或保留 B；源表不变 | 应用未切读 | 无 |
| 切换读后、停写旧列前 | 切回读 A；停用 B；对 B 的新写入做反向补齐 | 存在反向 mapping | **B 侧新写入必须反向迁移到 A**，否则回切丢数据 |
| 停写旧列、快照后、删列前 | 用快照恢复旧列数据；重新开放旧列写入 | 快照完整且可校验 | 快照后的新写入只在 B，需反向补齐 |
| 删列后 | 只能重建列 + 从快照/备份恢复 | 快照仍保留；结构变更需人工 DDL | **删列后的新写入无法恢复**，视为不可逆 |

默认时间窗：

- `rollback_window_hours`：切换读之后 72 小时内允许回切（可配）。
- `snapshot_retention_days`：列快照至少保留 30 天；保留期内不允许删除备份表。
- 超过窗口或备份过期后，回滚只能在人工评审下进行，且必须声明「可能丢失窗口内数据」。

### 5.3 不可回滚步骤（必须提前声明）

以下步骤一旦执行，数据库层面**不可用事务回滚**（MySQL DDL 隐式提交），且工具不会自动补偿：

1. `DROP COLUMN`（旧列下线）——即使有快照，快照之后的写入也会丢。
2. `DROP TABLE` 目标表 B 或快照表（删除备份后无恢复源）。
3. `TRUNCATE TABLE` B（本设计禁止自动执行，人工执行也视为不可逆）。
4. 切换读之后对 B 做不兼容的结构变更（没有反向 mapping 时无法回切）。
5. 修改 B 的唯一键/主键定义，导致按原键无法对齐。
6. 备份保留期到期后的清理。

UI 与报告中必须为每个不可逆步骤显示：`irreversible: true`、影响对象、已存在的备份、最后一次可回滚时间点。

### 5.4 列快照流程（下线前）

1. 创建快照表 `__mig_snapshot_<table>_<column>_<yyyymmddhhmm>`，显式列：源主键 + 旧列；`ENGINE=InnoDB`；不复制二级索引。
2. 按 PK 区间分批复制，复用与回填相同的分块/限速/checkpoint 机制。
3. 复制完成后按 4.1 的 V1-V3 校验快照与旧列，`mismatch=0` 才允许标记 `snapshot_taken`。
4. 快照表记录 `row_count`、`hash`、创建时间、对应 `plan_id`，写入审计。
5. 快照表只读使用，不参与业务；到期清理必须人工确认。

### 5.5 回滚失败与状态

- 回滚动作本身也要走审批/审计，并产生独立 `rollback_report`。
- 回滚 DDL 失败或超时 → 状态 `rollback_failed`，保留现场，不自动重试破坏性语句。
- 客户端超时但服务端可能已执行：复用 `ops::executor::compensate_created_indexes` 的判定思路——**以重新采集的真实结构为准**，不信任单条语句返回值。
- 回滚后必须重新采集 schema 与数据摘要，确认状态；无法确认时标 `unknown` 并停止后续所有动作。

---

## 6. 安全边界与既有链路关系

### 6.1 步骤分级与执行方式

| 步骤 | 等级 | 执行方式 | 是否复用现有链路 |
|---|---|---|---|
| 证据采集/剖面/依赖扫描 | L0 | 只读，自动 | 复用 `index_advisor` 读取 + 新增采集器 |
| 风险报告/dry-run 计划/脚本生成 | L0/L1 | 只读，自动生成 | 复用 `EvidencePack/Finding`、`hash_json`、审计 |
| 目标表 B 建表 | L2 | **只生成脚本 + dry-run**，用户执行 | 现有 `ActionSpec` 无 CreateTable，不复用执行器 |
| B 加辅助列/唯一键 | L2 | 只生成脚本 + dry-run | 唯一键可用现有 `CreateIndex`（L1）执行；加列不复用 |
| 历史数据回填 | L3 | 新执行器，分批、checkpoint、限速 | 不复用 `executor::execute`；复用审批/审计原语 |
| 数据校验 | L0 | 只读，自动 | 新建校验器；复用 `PkRange`/分块 |
| 切换读/双写 | 外部 | 用户应用发布，工具只记录回执 | 不涉及 DB 执行 |
| 旧列快照 | L3 | 新执行器，分批 + 校验 | 新建；复用回填分块 |
| 旧列下线 `DROP COLUMN` | L4 | **只生成脚本**，永不自动 | 复用 `safety` 的拦截作为最后防线 |
| 回滚 DDL（删 B/删快照） | L4 | 只生成脚本 | 同上 |
| B 上补建二级索引 | L1 | 可复用现有审批 + 执行 | `ActionSpec::CreateIndex` |
| `ANALYZE TABLE` B | L1 | 可复用现有审批 + 执行 | `ActionSpec::AnalyzeTable` |
| 回滚误建的索引 | L2 | 复用 `executor::rollback` 的补偿 | `ActionSpec::DropIndex` 的 rollback 路径 |

### 6.2 明确「只能生成脚本」的步骤

- `CREATE TABLE B`（含快照表/隔离表）
- `ALTER TABLE ... ADD COLUMN`
- `ALTER TABLE ... MODIFY COLUMN` / `CHANGE COLUMN` / `RENAME COLUMN`
- `ALTER TABLE ... DROP COLUMN`
- `CREATE VIEW` / `CREATE TRIGGER` 兼容层
- `DROP TABLE B` / `DROP TABLE 快照表` / `TRUNCATE TABLE`
- 任何 `ADD/DROP FOREIGN KEY`

这些步骤即使将来新增了 `ActionSpec`，也只能在 **L2 人工确认** 或 **L4 脚本** 下出现；本设计不把它们接入「一键执行」。

### 6.3 可以复用现有审批/执行链路的部分

| 能力 | 复用方式 | 约束 |
|---|---|---|
| 审批令牌 `approval.rs` | 复用令牌生成/消费/一次性/过期/plan_hash 绑定语义 | 现有 `issue` 拒绝非 auto 动作；**新增 L3 审批范围**，不放宽原闸门 |
| 审计 `audit.rs` | 复用 JSONL 追加、串行锁、fail-closed | 新增事件类型 `field_migration.*` |
| 策略 `policy.rs` | 复用全局开关/上限/只读判断 | 新增 `field_migration` 子策略，默认全部关闭 |
| 计划存储 `plan_store.rs` | 复用原子写/哈希/版本 | 多阶段状态另建 `MigrationStore`，不复用单计划结构 |
| 执行器连接卫生 `executor.rs` | 复用 `SET SESSION`、超时、`KILL QUERY`、审计后回写结果 | **不扩展 `execute` 支持 L3**；新建回填执行器 |
| 索引/分析动作 | 复用 `CreateIndex/AnalyzeTable` 全链路 | 只允许在 B 上、验证通过后 |
| 本地鉴权/CSRF | 复用 `web-server` 现有中间件 | 字段迁移接口同样受保护 |

### 6.4 必须新增的审批范围

现状：`approval::issue` 只对 `is_auto_executable()` 的动作签发，且执行器再次校验；L2 的 `DropIndex` 甚至无法通过 `execute` 执行，只能走 rollback。字段迁移的 L3 回填显然不能借用这套「自动执行」语义。

设计：

- 新增审批类型 `FieldMigrationPhase`，范围为 `backfill / snapshot / verify / retire_package`；
- 令牌仍绑定 `plan_hash + evidence_hash + declaration_hash + phase + checkpoint`；
- 回填令牌为**一次性、短时效、按阶段**，不覆盖下一次运行；
- 改变目标表名、mapping、null 策略、源表结构后，所有未使用令牌立即失效；
- `DROP COLUMN` 不在可审批枚举内，只有 `manual_scripts` 字段。

### 6.5 `DROP COLUMN` 永不自动执行的多重防线

1. `ActionSpec` **不新增** `DropColumn` 变体；不存在可执行的动作类型。
2. `ddl_compiler` 不生成删除列的 SQL；字段迁移模块只把它放进 `manual_scripts`。
3. `safety::classify_blocked_sql` 已把 `DROP COLUMN` 归为 L4 并阻断；作为最后一道 SQL 门。
4. 新回填执行器不复用通用 `execute` 的动作白名单。
5. UI 对 `retire_ready` 只提供「复制脚本 / 导出脚本」，不提供执行按钮。
6. 审计记录 `retire_script_exported`，但不记录「已执行」；人工回执需要用户上传/勾选，单独记录。

### 6.6 环境与总开关

- `field_migration.enabled`：默认 `false`；关闭时所有接口 404/403。
- `field_migration.allow_readonly_analysis`：默认 `true`（只读报告）。
- `field_migration.allow_backfill`：默认 `false`；生产环境单独开启。
- `field_migration.allow_snapshot`：默认 `false`。
- `field_migration.allow_retire_package`：默认 `true`（仅生成脚本，不执行）。
- 只读连接、`ctx.is_read_only`、ops 本地令牌任一不满足，写步骤直接拒绝。
- 所有阶段开始时重新采集 `source_schema_hash` 与 `target_schema_hash`；不一致则拒绝执行并回到 `plan_ready`。

---

## 7. 支持的维护操作清单

### 7.1 总表

| 操作 | 适用条件 | 风险等级 | 自动分析 | 自动回填 | 自动 DDL | 用户确认项 |
|---|---|---|---|---|---|---|
| 列改名 | 单列、无 DB 依赖或依赖已改；应用消费方可同步发布 | L3（DB 可逆，消费方断裂不可判定） | 是 | 否 | 否（脚本） | 新名、依赖报告、发布窗口、回滚脚本 |
| 列拆分（同表） | 新列可加且可空；旧列兼容期保留 | L3 | 是 | 是（L3，分批） | 否（脚本） | mapping、NULL/空串策略、长度、消费方、窗口 |
| 类型变更 | 新类型是旧类型的安全超集；全量值可转换 | L4 | 是 | 视方案 | 否（脚本） | 新类型、算法、维护窗口、备份、回滚 |
| 迁移到新表（JSON/长字符串拆表） | 同实例同库、InnoDB、单列 PK、无触发器/分区 | L2 建表 + L3 回填 + L4 下线 | 是 | 是（L3） | 否（脚本） | 目标 schema、mapping、null 策略、切换模式、观察期、保留期 |
| 列下线 | 校验通过、依赖清除、观察期满、快照已验证 | L4 | 是 | 否 | 否（脚本） | 表.列、backup_id、plan_id、保留期、停机窗口 |

> 等级说明：这里的 `L2/L3/L4` 是**数据库变更风险等级**，不是「可自动执行等级」。除总表中明确标注「自动回填」的步骤外，其余均只能脚本 + dry-run。

### 7.2 列改名

- **适用**：单列改名；MySQL 8.0 使用 `RENAME COLUMN`，5.7 使用 `CHANGE COLUMN` 并写全列定义；不改变类型/可空/默认值。
- **前置证据**：E01/E03/E04/E05/E06/E14；必须确认新名不冲突、长度 ≤ 64、非保留字。
- **风险**：DB 层通常可逆，但视图/触发器/例程/应用 SQL 在 MySQL 中不会自动跟随改名（元数据文本可能断裂）；应用侧断裂无法在库内完全验证。
- **自动化**：只读依赖扫描 + 脚本生成 + dry-run；不自动执行。
- **用户确认**：旧名/新名、依赖对象清单（视图/触发器/例程/外键/生成列）、应用发布顺序、旧名回滚脚本。
- **拒绝条件**：依赖对象无法读取；新名与既有对象冲突；大小写不敏感文件系统下仅大小写改名（按指纹冲突处理，要求两步改名）。

### 7.3 列拆分（同表）

- **适用**：一个列拆成同表多列；新列先 `ADD COLUMN`（可空），回填后旧列保留只读，最后人工删旧列。
- **前置证据**：E01-E14；新列长度/类型按 3.5 规则；旧列若有唯一/外键/生成列依赖 → 不允许删旧列（`BLOCK`），但回填仍可做。
- **风险**：回填期间旧列仍可能被写；新列与旧列语义漂移；新增列会触发表重建（5.7/8.0 行为不同，见 7.4）。
- **自动化**：分析、回填（L3）、校验；DDL 只脚本。
- **用户确认**：拆分映射、每个新列的 NULL/默认值、旧列只读时间点、应用双读/双写、下线时间。
- **拒绝条件**：无单列 PK；新列名冲突；目标长度不足；旧列参与唯一/外键且用户要求删列。

### 7.4 类型变更

- **适用**：明确的新旧类型映射，且全量值在目标类型可表示；优先走「新增临时列 + 回填 + 切换 + 删旧列」而不能直接 `MODIFY`。
- **前置证据**：E01、E12、E13、E14；全量扫描验证值域与精度；索引影响评估；版本能力。
- **风险**：
  - MySQL 5.7 与 8.0 的 online DDL 能力不同；`MODIFY COLUMN` 可能触发 COPY（锁写、耗时、磁盘翻倍）。
  - 字符集/排序规则随类型改变可能改变比较语义。
  - 数值/日期截断与四舍五入。
  - 索引需要重建，期间查询性能下降。
- **自动化**：只做分析、验证、脚本生成；**绝不自动执行 `MODIFY/CHANGE`**。若目标版本不支持无损转换，标「不支持自动变更」。
- **用户确认**：新旧类型、算法（`ALGORITHM=INPLACE/COPY`）、维护窗口、磁盘余量、备份、回滚脚本。
- **拒绝条件**：任何值无法无损转换；目标为 `NOT NULL` 且有 NULL；字符集/排序规则变更无法确认消费方；`ALGORITHM` 无法在目标版本使用（**不允许自动降级为 COPY**）。

### 7.5 迁移到新表（JSON/长字符串拆表）

- **适用**：同实例同库；源表 InnoDB、非分区、单列 PK；源列是 JSON 类型或可被 `JSON_VALID` 校验的文本；目标映射可完全覆盖观测 key，或用户明确选择 residual/隔离方案。
- **前置证据**：E01-E14 全量；full 剖面；版本 ≥ 5.7.8；B 不存在或为空。
- **风险等级**：建表 L2、回填 L3、下线 L4；**不是单次一键操作**。
- **自动化**：
  - 可自动：分析、风险报告、B schema 草案、回填 SQL、校验脚本、回滚脚本；
  - 不自动：建表、删列、删 B、视图/触发器；
  - 回填在 S2 阶段可自动（L3，审批 + checkpoint + 限速）。
- **用户确认**：目标表名与列映射、null/空串策略、未知 key 处理（residual/隔离/阻断）、1:N 元素序号语义、切换模式（停写/双写/列不可变）、观察期、快照保留期。
- **拒绝条件**：见第 8 节全部条目；以及 key 数 > 100、嵌套对象/对象数组自动建表、非法 JSON 且未选隔离方案。

### 7.6 列下线

- **适用**：旧列已无读/写消费方；依赖已清除；校验通过；观察期满；快照完成并校验；`retention_days` 已确认。
- **前置证据**：E03-E06 重新采集且均为空命中；E14 权限/版本；最终三向校验（4.5）。
- **风险**：L4；DDL 不可事务回滚；5.7 上删列可能 COPY 重建；删列后新写入无法恢复。
- **自动化**：**永不自动**。只生成 `retire_package`：脚本、影响对象、快照信息、回滚步骤、人工回执模板。
- **用户确认**：`table.column`、`plan_id`、`backup_id`、观察期证据、停机窗口、执行人/复核人、保留期。
- **拒绝条件**：任一依赖命中；校验未通过；快照缺失/校验失败；观察期不足；漂移非零；`retention_days` 未确认。

---

## 8. 明确不支持 / 必须人工介入的场景

| 场景 | 结论 | 原因与替代 |
|---|---|---|
| 无主键表 | **不支持自动迁移** | 无法安全分片、无法幂等、无法定位差异；只能导出/导入或专用工具 |
| 复合主键表 | **不支持自动迁移** | `sync` 的分块按单列 `primary_key` 设计；`fetch_min_max_pk`/范围条件不覆盖复合键 |
| 非 InnoDB | **不支持** | 无事务/崩溃恢复语义；回填与回滚不可保证 |
| 分区表 | **不支持** | 分区裁剪、DDL、删列行为差异大；需要人工设计 |
| 源表有触发器 | **不支持自动回填** | 触发器副作用/顺序不可控；只能停写 + 人工导出 |
| 跨实例/跨库 | **不支持（阶段 2）** | 本设计只覆盖同实例同库；跨实例需链路对账与网络故障处理 |
| MySQL < 5.7.8 且依赖 JSON | **不支持** | 无原生 JSON 类型 |
| online DDL 自动选择 | **不支持** | 5.7 与 8.0 能力差异大；脚本给出版本变体，必须在目标库 dry-run 后人工选择；`INPLACE` 失败**不允许自动回退 COPY** |
| 嵌套对象/对象数组自动建表 | **不支持（MVP）** | 结构推断与 1:N 建模不可靠；只允许 residual JSON 或人工 schema |
| key 数 > 100 | **不支持自动建表** | key 分布不稳定，宽表不可控；要求人工映射 |
| 无写入活动可观测性且用户不确认停写 | **不支持自动回填** | 无法证明回填期间没有新写入；只能停写窗口 |
| 大表超过 `migration_max_rows/bytes` | **不支持自动执行** | 只出报告/脚本；建议 gh-ost/pt-osc 等人工方案 |
| 字符集为 binary | **不支持自动解析** | 文本语义不可判定 |
| 目标唯一键存在重复 | **不支持自动迁移** | 必须先清洗或改唯一性设计 |
| 带外键引用的列 | **不支持自动下线** | 约束需人工处理；回填可读，但删列/改约束不行 |
| 生成列/函数索引依赖 | **不支持自动下线** | 依赖表达式需人工改写 |
| 加密表/特殊存储引擎 | **需人工评估** | 工具不假设其行为；只出报告 |

---

## 9. 分期建议

### S1（MVP，只读切片，风险最低）

**目标：证明「证据 + 风险预判 + 脚本」能提前发现坏数据/坏结构，而不写任何数据。**

交付：

- 启用 `field-governance` 只读场景；输入：表.列 + 目标描述 + 可选 `SemanticContract`。
- 采集 E01-E14；`full` 模式上限 5,000,000 行 / 10 GiB（与 2.4 一致）；超出自动降为 `sampled` 并禁止执行类结论。自动回填另受 2,000,000 行 / 5 GiB 限制。
- 输出风险报告：`BLOCK/HIGH/INFO` + 每条证据引用 + 声明冲突。
- 输出草案：B 的 `CREATE TABLE`、回填 SQL 模板、校验 SQL、回滚脚本、下线脚本（全部是**文本 + dry-run 结果**）。
- dry-run 只做只读解析与 SQL 展示，不连接执行。

S1 明确不做：建表、写数据、快照、删列、视图/触发器、跨库。

**验收（必须可复现）**：

1. 干净样本（10 万行，1:1 JSON，单列 PK）→ 无 `BLOCK`，脚本在影子库执行后校验 `mismatch=0`（脚本由人工执行）。
2. 注入 100 行非法 JSON → 报告 `BLOCK`，并在报告中给出行数/主键区间。
3. 列上有唯一索引/外键 → `BLOCK`。
4. 无主键表 → `BLOCK` + 「不支持自动迁移」。
5. JSON 出现对象数组 → `BLOCK` + 「需人工映射」。
6. 声明「只存偏好」但数据有 `order_id` → `HIGH` 或 `BLOCK`（按 residual 是否存在），且不能显示为已验证。
7. 同一份输入重复分析 → `evidence_hash`/`plan_hash` 稳定（除时间戳外）。

### S2（L3 回填）

补齐：

- `MigrationStore` 状态机 + 审批范围 + checkpoint。
- 回填执行器：分批、限速、幂等 upsert、断点续跑、失败分类。
- 校验 V1-V3、分块差异定位、报告。
- 仅支持「停写窗口 / 列不可变」两种模式；不支持双写编排。

**验收**：10 万行回填中断后从 checkpoint 续跑结果一致；注入 3 行目标差异能被精确报出；回填期间改源表结构自动拒绝执行。

### S3（快照 + 下线包 + 双写对账）

补齐：

- 旧列快照 + 快照校验。
- 观察期/漂移检测/周期性对账。
- `retire_package` 生成（脚本 + 影响对象 + 回滚步骤 + 人工回执）。
- B→A 反向补齐（仅在存在反向 mapping 时）。

**验收**：状态机不能跳过校验；快照缺失时无法进入 `retire_ready`；`DROP COLUMN` 只出现在 `manual_scripts`。

### S4（扩展）

- 同表列拆分、类型变更分析、列改名。
- 1:N 标量数组、跨库/跨实例（仍只脚本 + 人工）。
- PG/SQLite 另立设计，不复用 MySQL 结论。

---

## 10. 文件级落点

### 10.1 现有文件：复用/改造

| 文件 | 结论 | 具体内容 |
|---|---|---|
| `core_lib/src/ops/types.rs` | 复用 + 少量扩展 | 复用 `RiskClass`（已含 L0-L4）、`Finding/RiskFinding/ValidationCheck`、`hash_json`、`PlanStatus`；新增字段迁移类型放新模块，避免把 `EvidencePack` 撑大 |
| `core_lib/src/ops/registry.rs` | 改造 | `field-governance` 目前 `Disabled`，S1 改为「只读场景」；`risk_ceiling` 保持 L0（不允许从该场景直接执行写） |
| `core_lib/src/ops/index_advisor.rs` | 复用 | 复用 `load_snapshot_public` 的列/索引读取、`load_table_stats_public` 的体量估算；缺 `COLUMN_DEFAULT/CHARACTER_MAXIMUM_LENGTH/GENERATION_EXPRESSION`，在 evidence 采集器补查 |
| `core_lib/src/ops/mysql_rows.rs` | 部分复用 | 复用行读取/展示转换；**校验哈希不复用它**，因为它会把数值/JSON 类型抹成文本 |
| `core_lib/src/ops/safety.rs` | 复用 | `classify_blocked_sql` 继续作为最终 SQL 门；字段迁移不绕过 |
| `core_lib/src/ops/approval.rs` | 复用语义 + 新增方法 | 复用令牌一次性/过期/哈希绑定；新增 `FieldMigrationPhase` 审批，不放宽 `issue` 的 auto 限制 |
| `core_lib/src/ops/audit.rs` | 复用 | 新增事件类型，保持 fail-closed |
| `core_lib/src/ops/policy.rs` | 扩展 | 新增 `FieldMigrationPolicy`：开关、行数/字节上限、chunk 上限、采样阈值、观察期、快照保留期、限速 |
| `core_lib/src/ops/plan_store.rs` | 复用模式 | 原子写/哈希思路复用；多阶段状态用新 `MigrationStore` |
| `core_lib/src/ops/executor.rs` | 只复用卫生逻辑 | 连接、超时、`KILL QUERY`、审计回写；**不扩展通用 execute 支持 L3** |
| `core_lib/src/ops/ddl_compiler.rs` | 参考 | 仅用于索引/分析 SQL；不新增 Create/Alter/Drop Column 编译 |
| `core_lib/src/sync/mysql_engine.rs` | 部分复用（见 10.2） | 分块与流式读取复用；跨结构校验必须新建 |
| `core_lib/src/schema/ext.rs` | 复用 | `get_foreign_keys`（需补父表方向查询）、`get_views` 复用 |
| `core_lib/src/db/client.rs` | 新增小能力 | 采集 `SELECT VERSION()` / `@@version_comment`；现有代码无版本采集 |
| `core_lib/src/sql/util.rs` | 复用 | 标识符校验、引号、转义 |
| `core_lib/src/sync/transfer.rs` | **不复用** | 文件→表、`Append/Replace`；`Replace` 会 truncate，与迁移语义冲突 |
| `core_lib/src/sync/engine.rs` | 只读参考 | 结构 diff 可用于「期望 B vs 实际 B」的只读比对；不采用其破坏性 DDL |
| `web-server/src/routes.rs` | 扩展 | 新增字段迁移路由 |
| `web-server/src/ops_handlers.rs` | 复用模式 | 复用鉴权、ctx、审计、错误结构；新增分析/计划/回填/校验/快照/下线包 handler |
| `web-server/src/state.rs` | 扩展 | 挂载 `MigrationStore` 与字段迁移配置 |
| `web-ui/src/api.ts` | 扩展 | 新接口与类型 |
| `web-ui/src/types.ts` | 扩展 | 证据、风险、阶段、报告类型 |
| `web-ui/src/components/ops/OpsCenter.tsx` | 接入 | 新增「字段治理」入口；建议新组件 `FieldGovernance.tsx` |

### 10.2 `sync` 能力复用判定（读代码后的明确结论）

| 现有能力 | 位置 | 能否复用 | 结论 |
|---|---|---|---|
| `PkRange` | `sync/mysql_engine.rs:20-25` | 可以 | 直接用于分块边界与报告 |
| `AdaptiveChunker` | `sync/mysql_engine.rs:929-979` | 可以 | 直接用于回填批次大小控制 |
| `row_checksum_expr` | `sync/mysql_engine.rs:908-924` | 部分 | 只适合「同结构单列」快速预筛；A→B 结构不同，不能作为最终校验 |
| `fetch_rows_in_range_stream` | `sync/mysql_engine.rs:826-904` | 部分 | 固定 `SELECT *`；需要新增「按投影列 + 范围」的变体，或在 `sync` 抽出可复用的范围查询 |
| `checksum_rows` | `sync/mysql_engine.rs:702-715` | **否** | 整行 JSON 全等比较，A/B 结构不同必然不等 |
| `RowDataSyncEngine::compare` | `sync/mysql_engine.rs:74-188` | **否** | 两侧同一个 `table_name`，无法指定源表 A 与目标表 B |
| `deploy` | `sync/mysql_engine.rs:318-407` | **否** | 所有语句一个事务、无 checkpoint、无 mapping/归一化 |
| `fetch_min_max_pk` | `sync/mysql_engine.rs:496-539` | **需暴露** | 私有；建议改 `pub(crate)` 供 migration 复用，行为不变 |
| `fetch_pk_list_after` | `sync/mysql_engine.rs:541-620` | **需暴露** | 同上 |
| `build_pk_range_conditions` | `sync/mysql_engine.rs:425-494` | **需暴露** | 同上；否则只能在新模块重复实现 |
| `compare_pk_str` | `sync/mysql_engine.rs:409-420` | 需暴露 | 字符串主键边界比较，避免分块遗漏 |
| 跨结构规范哈希/差异定位 | 无 | **必须新建** | 核心新增能力 |

结论：**chunker 可复用；checksum 只能复用表达式做预筛，跨结构一致性必须新建 `verify.rs`。**

### 10.3 新增文件

| 文件 | 职责 |
|---|---|
| `core_lib/src/ops/field_migration/mod.rs` | 模块入口、对外 API、状态机导出 |
| `core_lib/src/ops/field_migration/types.rs` | `SemanticContract/Declaration/Evidence/FieldRisk/MigrationPlan/MigrationState/ValidationReport/RetirePackage` |
| `core_lib/src/ops/field_migration/evidence.rs` | E01-E14 采集、full/sampled 模式、`EvidenceGap` |
| `core_lib/src/ops/field_migration/dependency.rs` | 索引/唯一/外键/生成列/视图/触发器/例程依赖扫描 |
| `core_lib/src/ops/field_migration/risk.rs` | 2.3 判定表实现、`risk_floor`、声明冲突 |
| `core_lib/src/ops/field_migration/schema.rs` | B 表 schema 草案/类型推断/命名校验 |
| `core_lib/src/ops/field_migration/chunker.rs` | 分块计划、checkpoint、重试；调用 `sync` 的 public 原语 |
| `core_lib/src/ops/field_migration/backfill.rs` | 回填执行器（L3）、幂等 upsert、限速、失败分类 |
| `core_lib/src/ops/field_migration/verify.rs` | 规范编码、逐行/分块哈希、差异定位、报告 |
| `core_lib/src/ops/field_migration/snapshot.rs` | 旧列快照与快照校验 |
| `core_lib/src/ops/field_migration/rollback.rs` | 回滚评估、反向补齐前提、回滚报告 |
| `core_lib/src/ops/field_migration/retire.rs` | `retire_package` 生成，只产脚本 |
| `core_lib/src/ops/field_migration/store.rs` | 多阶段状态持久化、审批失效、并发保护 |
| `core_lib/src/ops/field_migration/scripts.rs` | 所有人工脚本模板（建表/校验/快照/删列/回滚/兼容视图） |
| `core_lib/tests/field_migration_rules.rs` | 规则表驱动单测、归一化/哈希稳定性、声明冲突 |
| `core_lib/tests/field_migration_mysql.rs` | dev MySQL 集成测试，默认 `#[ignore]`，按 `TEST_MYSQL_URL` 执行 |
| `web-ui/src/components/ops/FieldGovernance.tsx` | 证据/风险/阶段/脚本展示，无「执行删列」按钮 |

> 如启用 `serde_json` 的 `arbitrary_precision`，需要改 `Cargo.toml`（根 workspace 的 `serde_json` 依赖特性）；这是校验精度的前置条件，不能省略。

---

## 11. 验收与测试口径

### 11.1 必须覆盖的规则测试

- 2.3 判定表每一行至少一个用例；断言 `level` 与 `reason_code`，不允许只断言文字。
- 2.5 声明冲突矩阵每一行至少一个用例；断言声明不能降低 `BLOCK`。
- 3.5 schema 推断：整数/浮点/布尔/字符串/数组/对象/非法 key 各一组。
- 4.2 归一化：`NULL` vs `""` vs JSON `null`、数字精度（`1.0`/`1`/`1e3`/超长小数）、key 顺序、Unicode 原样。
- 4.3 差异分类：missing/extra/value_mismatch/cardinality_mismatch。
- 5.2 回滚前提：各阶段的可回滚判定；4.5 三向确认。
- `DROP COLUMN` 永不出现在任何可执行动作/`actions` 字段。

### 11.2 集成测试（dev MySQL，禁止生产）

1. 10 万行干净数据：回填 + 校验通过；再跑一次仍通过（幂等）。
2. 回填中途 kill：从 checkpoint 续跑，最终校验通过。
3. 目标端人为改 3 行：校验准确报出 3 个主键及字段类别。
4. 源端回填后再写 1 行：冻结前校验能发现漂移并阻止切换。
5. 非法 JSON / 唯一索引 / 外键 / 无 PK / 分区表 / 触发器：S1 报告 `BLOCK`，不产出可执行 DDL。
6. 快照后删除快照表再尝试 `retire_ready`：拒绝。
7. 5.7 与 8.0 各跑一次：脚本标注版本差异；`ALGORITHM` 不可用时不自动降级。

### 11.3 非功能要求

- 证据采集对源表只读；不得使用 `SELECT ... FOR UPDATE`。
- 回填/校验连接独立于用户查询连接；不共享长事务。
- 报告与审计不得包含完整原始敏感值；默认只含哈希、类别、主键。
- 所有阶段可中断；中断后重启不得重复写入或跳过 checkpoint。

---

## 12. 需要用户决策的取舍

| # | 取舍 | 建议 | 影响 |
|---|---|---|---|
| 1 | S1 是否只做只读 | **是** | 风险最低；代价是首版不能直接回填 |
| 2 | MVP 是否支持 1:N 数组 | **先不做**，只支持 1:1 标量对象 | 1:N 需要元素序号语义与额外校验，复杂度高 |
| 3 | 自动回填上限 | **200 万行 / 5 GiB** 起 | 超过只出脚本；可按环境调 |
| 4 | 切换模式 | **默认停写窗口**；双写只出方案 | 避免工具编排应用发布 |
| 5 | 观察期 / 快照保留 | **7 天 / 30 天** 起 | 影响下线前置时间 |
| 6 | 是否让工具创建 B | **否**，脚本 + 人工 | 保持 L2 边界；避免误建 |
| 7 | 是否用 LLM 生成语义契约 | **仅辅助生成，判定仍走规则** | 防止声明被当成事实 |
| 8 | 是否为字段迁移扩展 `ActionSpec` | **暂不扩展** | 防止绕过现有 L0/L1 执行白名单 |

---

## 13. 一页总结

- 风险预判 = 14 类证据 + 确定性三档规则 + 不可判定即阻断写步骤。
- 语义描述 = 带 ID 的未验证声明，只增约束、不降等级、不能压过数据与结构事实。
- 迁移 = 建表（L2 脚本）→ 分批回填（L3）→ 校验（V1-V5）→ 切换/观察（外部）→ 快照（L3）→ 下线（L4 脚本）。
- 校验 = 行数 + 主键集合 + 规范逐行哈希 + 分块差异定位；失败一律阻断下线。
- 回滚 = 下线前强制列快照；`DROP COLUMN`、`DROP TABLE`、`TRUNCATE`、切读后的不兼容 schema 变更声明为不可逆。
- 安全 = 多阶段人工闸门；`DROP COLUMN` 五重防线；复用审批/审计/策略，不扩展一键执行。
- 分期 = S1 只读分析/脚本（先证明价值）→ S2 停写回填 → S3 快照/下线包/双写对账 → S4 扩展。

