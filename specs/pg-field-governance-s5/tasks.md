# S5 任务清单

状态图例：`[ ]` 未开始 / `[x]` 完成 / `[-]` 不做（写明理由）

## A. 数据模型
- [x] `BackfillSpec` 增 `pk_columns` / `pk_kinds`（默认空，跳过序列化）
- [x] `Checkpoint` 增 `last_pks`；`BatchRecord` 增 `end_pks`
- [x] `TypeChangeJob` 增 `using_expr` / `fk_plan`（含 `FkPlanEntry` 进度位）
- [x] `IndexProgress` 增 `partitions_total/done/current_partition`
- [x] store 回环单测：老 JSON 反序列化、空值不序列化（`s5_fields_round_trip_and_legacy_json_parses`、`single_pk_spec_omits_s5_fields`）

## B. 复合主键 keyset
- [x] `create` 放开 PG 多列主键（MySQL 保持单列）
- [x] 通用 tuple 接口：`pk_columns`/`pk_kinds`/`push_key_range`/`order_key`/`row_expr`/`join_key`
- [x] `fetch_key_page` / `run_insert` / `batch_sql_parts` 支持 tuple（单列 SQL 逐字不变）
- [x] checkpoint 写入/续跑/自愈（`last_pks` 优先，长度与 kinds 校验）
- [x] V1–V4 多列 JOIN 与范围谓词
- [x] S1 脚本层同步支持多列主键（`pg_scripts`：建表/回填/校验/回滚模板）
- [x] 单测：复合 `build_batch_sql`（`composite_pk_batch_sql_uses_row_comparison` 等 3 个）
- [x] 真库：`composite_pk_backfill` / `composite_pk_resume` / `composite_pk_text_collate`

## C. 部分唯一索引 ON CONFLICT
- [x] `on_conflict_target` 返回 `ConflictTarget{columns,predicate}`
- [x] 批 SQL 渲染 `WHERE pred`（单列非部分路径逐字不变）
- [x] 表达式索引排除 + BLOCK 分支 + notes 残余风险
- [x] 真库：`partial_unique_index_conflict_target`

## D. 分区表
- [x] backfill：源分区放行、目标分区 BLOCK（`preflight:partitioned_target`）
- [x] notnull：父表 DDL + 逐叶子分区 VALIDATE（幂等/自愈）
- [x] index：`ON ONLY` 占位 + 逐分区 CONCURRENTLY + ATTACH + 幂等清理
- [x] typechange：`relkind='p'` 放行 + 分区索引健康检查 + 分区键列 BLOCK（`preflight:partition_key`）
- [x] 真库：`partitioned_source_backfill` / `partitioned_target_blocked` / `partitioned_table_notnull` / `partitioned_index_recipe` / `partitioned_table_typechange`

## E. freeform USING
- [x] 请求/任务字段 + 长度与 `;` 校验
- [x] preflight 干跑 `SELECT (expr) FROM tbl LIMIT 0`
- [x] 提供 USING → 强制 Rewrite 开闸、解除非确定性阻断
- [x] `spec_hash` 纳入 `using_expr`
- [x] 真库：`custom_using_expr` / `bad_using_expr_blocks` / `timestamp_tz_explicit`

## F. FK 编排
- [x] `fk_counterparts` 入参 + preflight 枚举/覆盖校验/属主/类型一致
- [x] 执行序五步幂等 + 失败步骤标注 + 续跑自愈
- [x] 验证：双端类型/行数/`convalidated`/索引健康
- [x] `spec_hash` 纳入 `fk_plan`
- [x] 真库：`fk_orchestration` / `fk_counterparts_mismatch_blocks`（原 `foreign_key_block` 语义收窄）

## G. HTTP 与收尾
- [x] type-change create 入参追加 `using_expr` / `fk_counterparts`（两套 router）
- [x] spec 状态更新、工作记忆追加
- [x] 三套口径跑数 + `test-summary.py`（三套 720/0，28 targets）
- [x] 本地提交（不 push）

## H. 实现期发现（超出原计划，已落地）
- [x] `risk.rs`：`E02_PARTITIONED` 在 PG 由 Block 降为 High；`E14_NO_SINGLE_PK` 在 PG 复合主键下不再产生（MySQL 不变）
- [x] `pg_scripts`：多列主键建表/回填/校验/回滚模板（原缺陷会让复合主键任务的 `ON CONFLICT (a,b)` 无对应约束）
- [x] `load_snapshot` / `index_advisor` 三处 `CROSS JOIN pg_database` 缺库名过滤（并发建库时结构哈希抖动 → 续跑误判 `schema_drift`）
- [x] `pg_typechange`：分区键列改类型 PG 硬禁止，补 `preflight:partition_key`
