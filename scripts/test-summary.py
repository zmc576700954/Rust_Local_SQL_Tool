#!/usr/bin/env python3
"""汇总 `cargo test --workspace` 的逐 target 结果，并单独统计“空跑”。

用法：
  cargo test --workspace --no-fail-fast 2>&1 | tee /tmp/test.log
  python3 scripts/test-summary.py /tmp/test.log

  # 或者由脚本执行 cargo 并落盘原始日志
  python3 scripts/test-summary.py --run --log /tmp/test.log

  # 校验预期值（不匹配则退出码 1）
  python3 scripts/test-summary.py /tmp/test.log --expect passed=491,empty=52,targets=20

为什么需要它：`cargo test --workspace` 不打印 workspace 级汇总，单看某个
`test result` 容易把单 target 当成总数，也会漏掉集成 target 与内嵌守卫测试。

“空跑”指：依赖数据库环境变量的用例在变量未设置时提前 return，却被计为 passed。
脚本只按分组统计当前环境确实未设置的组，环境变量已设置时不把对应用例算作空跑。
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
from dataclasses import dataclass, field
from pathlib import Path

ANSI = re.compile(r"\x1b\[[0-9;]*m")
SEG_RUN = re.compile(
    r"^\s*Running (unittests|tests)(?: |/)(\S+)\s+\(target/\S*?/([^/()]+)\)\s*$"
)
SEG_DOC = re.compile(r"^\s*Doc-tests (\S+)\s*$")
RESULT = re.compile(
    r"^test result: (?:ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored; "
    r"(\d+) measured; (\d+) filtered out; finished in ([\d.]+)s"
)
TEST_LINE = re.compile(r"^test (\S+) \.\.\. (ok|FAILED|ignored)")
HASH_SUFFIX = re.compile(r"-[0-9a-f]{16}$")

# 依赖环境变量的空跑分组：(分组名, 匹配规则, 依赖的环境变量, 说明)
# web-server 的集成测试是 bin 内的 cfg(test) 模块，用测试名模块前缀识别。
WEB_SERVER_MODULES = {
    "data_sync_integration_mysql::mysql_tests::": (
        "web-server/data_sync_integration_mysql_mysql",
        ["DATA_SYNC_MYSQL_URL"],
    ),
    "data_sync_integration_mysql::mariadb_tests::": (
        "web-server/data_sync_integration_mysql_mariadb",
        ["DATA_SYNC_MARIADB_URL"],
    ),
    "data_sync_integration_mysql::cross_flavor_": (
        "web-server/data_sync_integration_mysql_cross",
        ["DATA_SYNC_MYSQL_URL", "DATA_SYNC_MARIADB_URL"],
    ),
    "data_sync_integration_pg::": (
        "web-server/data_sync_integration_pg",
        ["DATA_SYNC_PG_URL"],
    ),
    "import_sql_integration_mysql::": (
        "web-server/import_sql_integration_mysql",
        ["IMPORT_SQL_MYSQL_URL"],
    ),
    "import_sql_integration_pg::": (
        "web-server/import_sql_integration_pg",
        ["IMPORT_SQL_PG_URL"],
    ),
    "schema_sync_integration_mysql::": (
        "web-server/schema_sync_integration_mysql",
        ["IMPORT_SQL_MYSQL_URL"],
    ),
    "schema_sync_integration_pg::": (
        "web-server/schema_sync_integration_pg",
        ["IMPORT_SQL_PG_URL"],
    ),
    "log_scrub::tests::real_auth_failure_keeps_error_code_and_reason": (
        "web-server/log_scrub",
        ["IMPORT_SQL_MYSQL_URL"],
    ),
}

# core_lib/tests/ 下的真库集成 target：target 内所有用例都由 env 守卫。
CORE_LIB_TARGET_GUARDS = {
    "field_migration_backfill_mysql": (
        "core_lib/field_migration_backfill_mysql",
        ["FIELD_MIGRATION_MYSQL_URL"],
    ),
    "field_migration_mysql": (
        "core_lib/field_migration_mysql",
        ["FIELD_MIGRATION_MYSQL_URL"],
    ),
    "field_migration_s3_mysql": (
        "core_lib/field_migration_s3_mysql",
        ["FIELD_MIGRATION_MYSQL_URL"],
    ),
    "field_migration_verify_mysql": (
        "core_lib/field_migration_verify_mysql",
        ["FIELD_MIGRATION_MYSQL_URL"],
    ),
    "inspection_mysql": ("core_lib/inspection_mysql", ["INSPECTION_MYSQL_URL"]),
    "perf_diagnostics_mysql": ("core_lib/perf_diagnostics_mysql", ["PERF_MYSQL_URL"]),
}


@dataclass
class Target:
    crate: str
    kind: str
    name: str
    passed: int = 0
    failed: int = 0
    ignored: int = 0
    duration: float = 0.0
    tests: list[tuple[str, str]] = field(default_factory=list)
    has_result: bool = False


def cargo_binary() -> str:
    """cargo 不一定在非交互 PATH 中，回退到 rustup 默认安装位置。"""
    found = shutil.which("cargo")
    if found:
        return found
    fallback = Path.home() / ".cargo" / "bin" / "cargo"
    return str(fallback) if fallback.is_file() else "cargo"


def resolve_crate_map(root: Path) -> dict[str, str]:
    """target 名（连字符归一化为下划线）-> package 名。"""
    try:
        out = subprocess.run(
            [cargo_binary(), "metadata", "--no-deps", "--format-version", "1"],
            cwd=root,
            check=True,
            capture_output=True,
            text=True,
        ).stdout
    except (OSError, subprocess.CalledProcessError) as err:
        print(f"warning: 无法读取 cargo metadata，crate 列将退化为 target 名：{err}", file=sys.stderr)
        return {}
    mapping: dict[str, str] = {}
    for package in json.loads(out).get("packages", []):
        for target in package.get("targets", []):
            name = target.get("name", "").replace("-", "_")
            if name:
                mapping[name] = package.get("name", name)
    return mapping


def target_kind(header_kind: str, spec: str) -> str:
    if header_kind == "doc":
        return "doc"
    if header_kind == "tests":
        return "integration"
    if spec.endswith("src/lib.rs"):
        return "lib"
    if spec.endswith("src/main.rs") or spec.startswith("src/bin/"):
        return "bin"
    return "other"


def guard_for(target: Target, test_name: str) -> tuple[str, list[str]] | None:
    if target.crate == "web-server":
        for prefix, info in WEB_SERVER_MODULES.items():
            if test_name.startswith(prefix):
                return info
        return None
    if target.crate == "core_lib":
        return CORE_LIB_TARGET_GUARDS.get(target.name)
    return None


def parse(text: str, crate_map: dict[str, str]) -> list[Target]:
    targets: list[Target] = []
    current: Target | None = None
    missing_result: list[str] = []
    for raw in text.splitlines():
        line = ANSI.sub("", raw)
        run = SEG_RUN.match(line)
        if run:
            if current is not None and not current.has_result:
                missing_result.append(f"{current.crate}/{current.name}")
            header_kind, spec, artifact = run.groups()
            artifact = HASH_SUFFIX.sub("", artifact)
            crate = crate_map.get(artifact.replace("-", "_"), artifact)
            current = Target(crate=crate, kind=target_kind(header_kind, spec), name=artifact)
            targets.append(current)
            continue
        doc = SEG_DOC.match(line)
        if doc:
            if current is not None and not current.has_result:
                missing_result.append(f"{current.crate}/{current.name}")
            artifact = doc.group(1)
            crate = crate_map.get(artifact.replace("-", "_"), artifact)
            current = Target(crate=crate, kind="doc", name=artifact)
            targets.append(current)
            continue
        result = RESULT.match(line)
        if result:
            if current is None:
                raise SystemExit(f"error: 发现 test result 但没有对应的 target 段头：{line}")
            current.passed, current.failed, current.ignored = map(int, result.group(1, 2, 3))
            current.duration = float(result.group(6))
            current.has_result = True
            continue
        test = TEST_LINE.match(line)
        if test and current is not None:
            current.tests.append((test.group(1), test.group(2)))

    if missing_result:
        raise SystemExit("error: 以下 target 段没有对应的 test result 行：" + ", ".join(missing_result))
    if not targets:
        raise SystemExit("error: 输入中没有找到任何 target 段头，请确认传入的是 cargo test 的完整输出")
    return targets


def empty_runs(targets: list[Target]) -> tuple[dict[str, int], int, list[str]]:
    groups: dict[str, int] = {}
    details: list[str] = []
    unset_cache: dict[str, bool] = {}

    def env_unset(names: list[str]) -> bool:
        for name in names:
            if name not in unset_cache:
                unset_cache[name] = name not in os.environ
            if not unset_cache[name]:
                return False
        return True

    for target in targets:
        for test_name, status in target.tests:
            if status != "ok":
                continue
            guard = guard_for(target, test_name)
            if guard is None:
                continue
            group, envs = guard
            if not env_unset(envs):
                continue
            groups[group] = groups.get(group, 0) + 1
            details.append(f"{group}: {test_name}")
    total = sum(groups.values())
    return groups, total, details


def report(targets: list[Target], expect: dict[str, int]) -> int:
    header = f"{'crate':<12} {'type':<12} {'target':<36} {'passed':>7} {'failed':>7} {'ignored':>8} {'time(s)':>8}"
    print(header)
    print("-" * len(header))
    for target in targets:
        print(
            f"{target.crate:<12} {target.kind:<12} {target.name:<36} "
            f"{target.passed:>7} {target.failed:>7} {target.ignored:>8} {target.duration:>8.2f}"
        )

    total_passed = sum(t.passed for t in targets)
    total_failed = sum(t.failed for t in targets)
    total_ignored = sum(t.ignored for t in targets)
    print("-" * len(header))
    print(
        f"{'TOTAL':<12} {'':<12} {'':<36} "
        f"{total_passed:>7} {total_failed:>7} {total_ignored:>8} {sum(t.duration for t in targets):>8.2f}"
    )
    print(f"target 数: {len(targets)}")

    groups, empty_total, _ = empty_runs(targets)
    print("\n空跑（环境变量未设置时提前 return，但仍计为 passed）：")
    if groups:
        for name in sorted(groups):
            print(f"  {name:<45} {groups[name]:>3}")
    else:
        print("  (无：相关环境变量均已设置，或没有匹配到守卫用例)")
    print(f"  {'合计':<45} {empty_total:>3}")

    # 一致性校验：逐个 target 的用例明细必须与 test result 行对得上。
    problems: list[str] = []
    for target in targets:
        observed = len(target.tests)
        recorded = target.passed + target.failed + target.ignored
        if observed != recorded:
            problems.append(
                f"{target.crate}/{target.name}: 明细 {observed} 条 != test result {recorded} 条"
            )
    if empty_total > total_passed:
        problems.append(f"空跑 {empty_total} 条 > passed {total_passed} 条")
    if total_failed and expect.get("failed", 0) == 0:
        problems.append(f"存在 failed={total_failed}，与预期 failed=0 不符")

    for key, actual in (("passed", total_passed), ("empty", empty_total), ("targets", len(targets))):
        if key in expect and expect[key] != actual:
            problems.append(f"预期 {key}={expect[key]}，实际 {actual}")

    if problems:
        print("\n校验失败：")
        for problem in problems:
            print(f"  - {problem}")
        return 1
    print("\n校验通过：逐 target 明细与 test result 行一致。")
    return 0


def parse_expect(raw: str | None) -> dict[str, int]:
    if not raw:
        return {}
    expected: dict[str, int] = {}
    for item in raw.split(","):
        key, _, value = item.partition("=")
        if not key or not value.isdigit():
            raise SystemExit(f"error: --expect 格式应为 key=number,...，收到 {raw!r}")
        expected[key.strip()] = int(value)
    return expected


def main() -> int:
    parser = argparse.ArgumentParser(description="汇总 cargo test --workspace 结果并统计空跑")
    parser.add_argument("log", nargs="?", help="cargo test 输出文件；省略时读 stdin")
    parser.add_argument("--run", action="store_true", help="由脚本执行 cargo test --workspace --no-fail-fast")
    parser.add_argument("--log", dest="run_log", default="target/test-summary-raw.log", help="--run 模式保存原始日志的路径")
    parser.add_argument("--expect", help="预期值，例如 passed=491,empty=52,targets=20")
    args = parser.parse_args()

    root = Path(__file__).resolve().parent.parent
    crate_map = resolve_crate_map(root)
    expect = parse_expect(args.expect)

    cargo_code = 0
    if args.run:
        log_path = Path(args.run_log)
        log_path.parent.mkdir(parents=True, exist_ok=True)
        with log_path.open("w", encoding="utf-8") as handle:
            handle.write("")
            proc = subprocess.run(
                [cargo_binary(), "test", "--workspace", "--no-fail-fast"],
                cwd=root,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
            )
            handle.write(proc.stdout)
        cargo_code = proc.returncode
        text = proc.stdout
        print(f"原始日志: {log_path}")
    elif args.log:
        text = Path(args.log).read_text(encoding="utf-8")
    else:
        text = sys.stdin.read()

    targets = parse(text, crate_map)
    code = report(targets, expect)
    if code != 0:
        return code
    return cargo_code


if __name__ == "__main__":
    raise SystemExit(main())
