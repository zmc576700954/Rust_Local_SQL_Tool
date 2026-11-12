# S4 任务清单（改类型可执行化）

评审通过前不动代码。完成后逐项勾选。

## A. 数据模型与存储（migration_store.rs）

- [x] A1 `TypeChangeJob` / `TypeChangePhase` / `TypeChangeClass` / `TypeChangeExecution` / `TypeChangeVerify` 结构（serde 默认值，见 spec §4）
- [x] A2 `MigrationStore` 增 `typechange_jobs`（空映射不序列化）；load/save/mark_running_as_interrupted 覆盖第四类
- [x] A3 `MigrationHub` 增 `get_typechange` / `update_typechange` / `list_typechange`；`is_active`/controls 复用
- [x] A4 单测：回环、老文件兼容（无 typechange 字段）、空映射字节不变

## B. 分类器（纯函数）

- [x] B1 `classify_type_change(source, target)` 白名单实现（spec §5 五类组合），复用 `normalize_pg_type`
- [x] B2 typmod 解析（`name` / `name(n)` / `name(p,s)`），解析失败按 Rewrite
- [x] B3 单测：白名单全组合 + typmod 边界 + 别名归一 + 未收录类型

## C. 执行器（pg_typechange.rs）

- [x] C1 Preflight 八项（表存在/relkind、列与漂移、regtype、分类与开闸、非确定性、依赖、属主、前置快照）
- [x] C2 DDL 助手收敛：`exec_ddl_auto_commit` 共享化，pg_notnull/pg_index 改调（行为不变）
- [x] C3 Applying：单语句 `ALTER ... TYPE ... USING col::target` + 错误映射（lock_timeout/conversion/no_cast/dependent/privilege/中断）
- [x] C4 Verifying：类型匹配 + 行数一致 + 索引 indisvalid + 默认值快照
- [x] C5 控制信号（阶段边界 pause/abort）与四类审计事件
- [x] C6 mod.rs 注册 `pub mod pg_typechange;`

## D. HTTP 端点（web-server）

- [x] D1 create/run/status/list 四个端点（两套 router 同步注册）
- [x] D2 鉴权链路：authorize_write + ensure_scenario_engine + authorize_backfill

## E. 真库测试（field_migration_typechange_pg.rs）

- [x] E1 13 个用例（spec §8.2），FIELD_MIGRATION_PG_URL 独立临时库
- [x] E2 真库跑通并**报数（唯一节点）**

## F. 收尾

- [x] F1 test-summary.py 守卫加 `field_migration_typechange_pg`
- [x] F2 本期改动文件 rustfmt 干净；cargo check --workspace --tests 通过
- [x] F3 三套口径跑数（无库 / MySQL+PG / MariaDB+PG）
- [x] F4 spec 状态更新为已实现 + 实现期发现补记；tasks.md 全勾
- [x] F5 工作记忆追加；本地提交（不 push）
