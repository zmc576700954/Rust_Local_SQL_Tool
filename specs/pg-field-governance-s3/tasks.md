# S3 任务清单

依据 `spec.md`（2026-11-11 用户确认范围：SET NOT NULL 可执行化、CREATE INDEX CONCURRENTLY、能力位开放；改类型/改 collation 维持仅脚本）。

原则：不 push、不 `#[ignore]`、不删用例、改断言写依据；MySQL/MariaDB 行为与 JSON 零变化；出现改行为/用例/脚本语义的分叉先问再做。

## A. 数据模型与存储

- [x] A1 `migration_store.rs`：`MigrationStore` 增 `notnull_jobs` / `index_jobs`（serde default + 空映射不序列化），老文件兼容；`MigrationHub` 控制信号/并发去重对新 job 类型可用
- [x] A2 `NotNullJob` / `NotNullPhase` / `NotNullScanProgress` / `NotNullExecution` 结构与 JSON 序列化
- [x] A3 `IndexJob` / `IndexSpec` / `IndexProgress` 结构与 JSON 序列化
- [x] A4 `spec_hash`、`engine` 绑定与续跑校验（沿用 S2 模式）

## B. SET NOT NULL 执行器（`pg_notnull.rs`）

- [x] B1 preflight：引擎/表列存在/已 NOT NULL no-op/分区/主键类型/排序规则/PG≥12/残留约束探测（Not VALID→Validate，validated→SetNotNull）
- [x] B2 Scan：keyset 分批 NULL 扫描（类型化绑定 + `COLLATE "C"`）、`SET LOCAL`、限速与控制信号、`null_count` + 样本（≤100）、续跑游标
- [x] B3 Scan 收尾：null_count>0 → `Blocked(scan:null_rows)`；重跑从头扫
- [x] B4 DDL 阶段机：AddCheck（幂等跳过）→ Validate（违例 → Blocked 重回 Scan）→ SetNotNull → DropCheck；session 级 lock_timeout、statement_timeout=0
- [x] B5 Verify：`is_nullable=NO` + `attnotnull` + 约束已删；审计摘要
- [x] B6 版本判断纯函数 + `#[cfg(test)]` 单测

## C. CREATE INDEX CONCURRENTLY 执行器（`pg_index.rs`）

- [x] C1 preflight：引擎/表存在/非分区/列存在/列非空/名长 ≤63 字节/同名索引探测（valid→no-op、invalid→DROP CONCURRENTLY 清理、非索引同名→BLOCK）
- [x] C2 执行：独立连接单语句自动提交（不进事务）、session 超时设置
- [x] C3 进度：5s best-effort 轮询 `pg_stat_progress_create_index` 写入 `job.progress`
- [x] C4 失败语义：unique 违例 → 清理 + `Blocked(build:unique_violation)`；其他失败 → 清理 + Interrupted 可重跑
- [x] C5 Verify：`indisvalid AND indisready` + `pg_relation_size`；审计摘要

## D. 能力位与 API

- [x] D1 descriptor `engines` += `postgresql`（文案不动）；既有单测补断言
- [x] D2 web-server：notnull/index 最小 HTTP 端点（create/run/status/list），复用 `authorize_write`
- [x] D3 确认 backfill/analyze 既有端点对 PG 连接无隐藏门控（如有则打通并补说明）

## E. 真库测试（节点报数）

- [x] E1 `field_migration_notnull_pg.rs` 10 用例（spec §7.1）→ 节点 1 报数
- [x] E2 `field_migration_index_pg.rs` 10 用例（spec §7.2）→ 节点 2 报数

## F. 收尾

- [x] F1 `test-summary.py` 守卫加两个新 target
- [x] F2 `cargo fmt`（仅 S3 触及文件）+ `cargo check --workspace`
- [x] F3 三套口径：无库 / MySQL+PG / MariaDB+PG（带库 `--no-fail-fast -- --test-threads=1`），`test-summary.py` 同 env 解析
- [x] F4 更新 spec 状态为已实现、写 S4 边界清单（spec §10）
- [x] F5 本地提交，不 push；最终汇报真库用例结果 + 三套口径数字
