# PG 字段治理 S2 任务清单

对应规格：[spec.md](./spec.md)

## 里程碑 A：执行器真库跑通（节点 1 报数）

- [x] A1 数据模型：`PkKind` 增 `Numeric/Uuid`；`BackfillConfig.single_transaction`；`BackfillSpec.schema/source_data_type`；`Checkpoint.engine/pk_kind/target_fingerprint`；`BatchVerify.validation_mode`；`BackfillExecution` 与 `MigrationJob.engine/execution`。
- [x] A2 `prepare`：PG 分派、单事务仅 PG、写入 `schema/source_data_type/execution`。
- [x] A3 `backfill.rs`：`run_job` 按引擎分派；`execute_create` 按引擎取连接；复用函数改 `pub(crate)`。
- [x] A4 新增 `pg_backfill.rs`：预检（engine/pk_kind/not_resumable/schema/触发器/分区/ON CONFLICT 唯一索引/文本排序规则/目标非空/指纹）。
- [x] A5 `pg_backfill.rs`：主键类型探测与绑定、单批 `INSERT ... SELECT ... ORDER BY ... LIMIT ... ON CONFLICT`、`SET LOCAL` 超时、限速/控制复用。
- [x] A6 事务语义：每批一事务 + 失败回滚当前批 + 错误含 batch/区间/已完成行数；`single_transaction` 整任务回滚且不可续跑。
- [x] A7 checkpoint：绑定 engine/pk_kind/target_fingerprint；续跑不一致拒绝；旧文件默认 `mysql`。
- [x] A8 新增 `core_lib/tests/field_migration_backfill_pg.rs` 全部用例。
- [x] A9 跑 `cargo test -p core_lib --test field_migration_backfill_pg -- --test-threads=1 --no-fail-fast` 并报数（节点 1）。

## 里程碑 B：V1–V4 真库跑通（节点 2 报数）

- [x] B1 新增 `pg_verify.rs`：`PgSnapshot`（REPEATABLE READ READ ONLY）、`validation_mode=snapshot`、`settings()`。
- [x] B2 规范化表达式：numeric（非 f64）、bytea hex、timestamptz UTC、json→jsonb、uuid、NULL≠''。
- [x] B3 V1/V2/V3/V4 计数、差异分页（参数绑定 + `COLLATE "C"`）。
- [x] B4 `pg_scripts.rs` 公开表达式生成器；uuid cast；json 目标 `::jsonb`。
- [x] B5 新增 `core_lib/tests/field_migration_verify_pg.rs` 全部用例。
- [x] B6 跑 `cargo test -p core_lib --test field_migration_verify_pg -- --test-threads=1 --no-fail-fast` 并报数（节点 2）。

## 里程碑 C：收尾

- [x] C1 `web-server` 增加 `single_transaction` 选项并透传。
- [x] C2 `scripts/test-summary.py` 增加两个 PG target 守卫。
- [x] C3 `cargo fmt --all -- --check`、`cargo check --workspace`。
- [x] C4 三套口径（无库 / MySQL+PG / MariaDB+PG，带库加 `--test-threads=1 --no-fail-fast`），用相同 env 运行 `test-summary.py`。
- [x] C5 汇总 S3 边界清单，本地提交（不 push）。

## 约束

- 不 push、不 `#[ignore]`、不删用例。
- 改断言必须写依据。
- MySQL 回填 SQL/事务/JSON 字段保持不变。
- 出现会改行为/用例/脚本语义的新分叉 → 先问用户，再实现。
- `engines` 能力位仍只有 mysql/mariadb。
