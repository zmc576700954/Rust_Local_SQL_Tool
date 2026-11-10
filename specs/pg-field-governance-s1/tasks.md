# PG 字段治理 S1 任务清单

> 依赖：`specs/pg-field-governance-s1/spec.md` 批准后执行。
> 纪律：分阶段提交、不 push、不 `#[ignore]`、不删用例、不放宽断言。

## T1 类型与只读门基础设施

- [x] `types.rs`：`EnvironmentEvidence` 增加 `engine_family`/`schema`/`server_version_num`/`table_kind`/`nondeterministic_collation`/`pg_authid_readable`/`no_password_roles`；`DependencyEvidence` 增加 `notes`；`ColumnProfile` 增加 `pg_cast_invalid_rows`/`pg_input_check`；扩展 `is_text_like`/`is_json_type`。
- [x] `read_only.rs`：新增 `pub fn validate_read_only_sql(DbType, &str)`；`fetch_read_only` 按引擎分派 PG。
- [x] 单测：PG 方言接受 SELECT、拒绝写语句（含 `INSERT ... ON CONFLICT`）。
- [x] `cargo test -p core_lib --lib field_migration` 通过。

## T2 PG 证据采集

- [x] 新增 `pg_evidence.rs`：`collect`、`load_columns_pg`、`load_environment_pg`、`target_exists_pg`、`source_sql_pg`、`profile_column_pg`、`probe_pg_casts`、声明证据、`build_evidence_pg`。
- [x] E01–E14 按 spec 第 5 节来源实现；不适用项写 `details.not_applicable`。
- [x] E09 `e09_mode(server_version_num)` + 单测两分支。
- [x] `evidence::collect` 分派 PG，MySQL 分支零改动。
- [x] `mod.rs` 声明模块。
- [x] 真库冒烟：E01/E08/E09/E10/E11/E13/E14 返回可读值。

## T3 PG 依赖扫描

- [x] `pg_evidence.rs` 内实现 E03–E06：`pg_index`/`pg_get_indexdef`、`pg_constraint`、`pg_attrdef`+`pg_depend`、`pg_rewrite`+`pg_depend` 视图、`pg_trigger`、`pg_proc` 文本匹配（notes 标下界）。
- [x] 主键/单列主键/唯一索引解析正确；`DependencyEvidence.scan_complete` 在任一查询失败时为 false。
- [x] 真库断言：E03 唯一索引、E04 外键、E05 生成列、E06 视图精确列级。

## T4 PG 风险规则

- [x] 新增 `pg_risk.rs`：`E02_PG_UNSUPPORTED_RELKIND`、`E13_PG_NONDETERMINISTIC_COLLATION`、`E13_PG_COLLATION_REWRITE`、`E01_PG_TYPE_REWRITE`、`E01_PG_USING_INVALID`、`E01_PG_USING_UNVERIFIED`、`E08_PG_SET_NOT_NULL_SCAN`。
- [x] `risk::evaluate` 末尾分派；PG 下跳过 `E02_NOT_INNODB` 与 MySQL 列级 `E13_BINARY_CHARSET`。
- [x] 单测覆盖每个新 code 的触发/不触发。
- [x] 真库断言：分区表、复合主键、非确定性 collation、类型变更、USING 失败。

## T5 PG 脚本生成

- [x] 新增 `pg_scripts.rs`：建表、回填、V1–V4 校验、回滚、下线；`alter-type`/`set-not-null`/`alter-collation` 条件生成。
- [x] `scripts::generate` 分派；dry-run 幂等校验兼容 `ON CONFLICT`。
- [x] 脚本断言：`ON CONFLICT`、`EXCLUDED`、`->`/`->>`、`jsonb_typeof`、`encode(convert_to(`、`AT TIME ZONE 'UTC'`；全部 `executable=false`。
- [x] `classify_blocked_sql` 对 `DROP COLUMN`/`DROP TABLE` 仍阻断。

## T6 集成测试

- [x] 新增 `core_lib/tests/field_migration_pg.rs`：env gate、临时数据库、测试表与数据。
- [x] 覆盖 spec 第 9 节 10 类用例。
- [x] 断言修改处写「依据」注释（若有）。
- [x] 真库运行 `--test-threads=1 --no-fail-fast` 全绿。

## T7 汇总与验收

- [x] `scripts/test-summary.py` 的 `CORE_LIB_TARGET_GUARDS` 增加 `field_migration_pg -> FIELD_MIGRATION_PG_URL`。
- [x] 重跑 MySQL 字段治理用例。
- [x] 三套口径（无库 / MySQL+PG / MariaDB+PG）记录真实数字。
- [x] `cargo test --workspace` 兜底。
- [x] 汇报：真库用例结果、三套口径数字、S2/S3 边界清单。
- [x] 分阶段 git commit（不 push）。
