#!/usr/bin/env python3
"""清理异常退出后残留的独占测试库。

默认 **dry-run**：只列出将要删除的库，不执行任何 DROP。
确认列表无误后加 `--apply` 才真正删除。

用法：
  python3 scripts/cleanup-test-databases.py
  python3 scripts/cleanup-test-databases.py --apply
  python3 scripts/cleanup-test-databases.py --prefix it_fm_ --prefix it_imp_ --apply
  python3 scripts/cleanup-test-databases.py --mysql-container my-mysql --pg-container my-pg

默认容器名与仓库 compose 一致（phase3-mysql / phase3-mariadb / phase3-pg），
凭据默认 root/password 与 postgres/password；可用 --user/--password 覆盖。

只匹配白名单前缀，永远不会碰 e2e、mysql、postgres、information_schema 等库。
"""

from __future__ import annotations

import argparse
import subprocess
import sys

DEFAULT_PREFIXES = [
    "it_imp_",
    "it_xdb_",
    "it_sync_",
    "it_pg_",
    "it_verify_",
    "it_s3_",
    "it_inspect_",
    "it_perf_",
    "it_s2_",
    "it_fm_",
    "it_auth_",
]

SYSTEM_DATABASES = {
    "information_schema",
    "performance_schema",
    "mysql",
    "sys",
    "postgres",
    "template0",
    "template1",
}


def run(cmd: list[str], stdin: str | None = None) -> str:
    proc = subprocess.run(cmd, input=stdin, capture_output=True, text=True)
    if proc.returncode != 0:
        raise RuntimeError(
            f"命令失败({proc.returncode}): {' '.join(cmd)}\n{proc.stderr.strip()}"
        )
    return proc.stdout


def running_containers() -> set[str]:
    try:
        out = run(["docker", "ps", "--format", "{{.Names}}"])
    except (OSError, RuntimeError):
        return set()
    return {line.strip() for line in out.splitlines() if line.strip()}


def mysql_query(container: str, user: str, password: str, sql: str) -> str:
    client = "mariadb"
    try:
        run(["docker", "exec", container, "which", client])
    except RuntimeError:
        client = "mysql"
    return run(
        ["docker", "exec", container, client, f"-u{user}", f"-p{password}", "-N", "-e", sql]
    )


def pg_query(container: str, user: str, password: str, sql: str) -> str:
    return run(
        [
            "docker",
            "exec",
            "-e",
            f"PGPASSWORD={password}",
            container,
            "psql",
            "-U",
            user,
            "-tAc",
            sql,
        ]
    )


def quote_mysql(name: str) -> str:
    return "`" + name.replace("`", "``") + "`"


def quote_pg(name: str) -> str:
    return '"' + name.replace('"', '""') + '"'


def main() -> int:
    parser = argparse.ArgumentParser(description="列出/清理残留的独占测试库（默认 dry-run）")
    parser.add_argument("--apply", action="store_true", help="真正执行 DROP；默认只列出")
    parser.add_argument("--prefix", action="append", dest="prefixes", help="库名前缀，可重复")
    parser.add_argument("--mysql-container", default="phase3-mysql")
    parser.add_argument("--mariadb-container", default="phase3-mariadb")
    parser.add_argument("--pg-container", default="phase3-pg")
    parser.add_argument("--mysql-user", default="root")
    parser.add_argument("--mysql-password", default="password")
    parser.add_argument("--pg-user", default="postgres")
    parser.add_argument("--pg-password", default="password")
    args = parser.parse_args()

    prefixes = tuple(args.prefixes or DEFAULT_PREFIXES)
    containers = running_containers()

    engines = [
        ("mysql", args.mysql_container, "mysql"),
        ("mariadb", args.mariadb_container, "mysql"),
        ("postgres", args.pg_container, "pg"),
    ]

    targets: list[tuple[str, str, str, str]] = []  # engine, container, database, kind
    for engine, container, kind in engines:
        if container not in containers:
            print(f"[跳过] {engine}: 容器 {container} 未运行")
            continue
        if kind == "mysql":
            out = mysql_query(container, args.mysql_user, args.mysql_password, "SHOW DATABASES")
        else:
            out = pg_query(
                container,
                args.pg_user,
                args.pg_password,
                "SELECT datname FROM pg_database WHERE datistemplate = false",
            )
        for line in out.splitlines():
            name = line.strip()
            if not name or name in SYSTEM_DATABASES:
                continue
            if name.startswith(prefixes):
                targets.append((engine, container, name, kind))

    if not targets:
        print("没有发现匹配前缀的残留测试库。")
        return 0

    print(f"匹配前缀: {', '.join(prefixes)}")
    print(f"{'引擎':<10} {'容器':<16} {'数据库':<44} 动作")
    print("-" * 88)
    for engine, container, name, _ in targets:
        print(f"{engine:<10} {container:<16} {name:<44} {'DROP' if args.apply else '将删除(dry-run)'}")

    if not args.apply:
        print(f"\n共 {len(targets)} 个库将被删除。确认无误后加 --apply 执行。")
        return 0

    failures: list[str] = []
    for engine, container, name, kind in targets:
        try:
            if kind == "mysql":
                mysql_query(
                    container,
                    args.mysql_user,
                    args.mysql_password,
                    f"DROP DATABASE IF EXISTS {quote_mysql(name)}",
                )
            else:
                pg_query(
                    container,
                    args.pg_user,
                    args.pg_password,
                    f"DROP DATABASE IF EXISTS {quote_pg(name)} WITH (FORCE)",
                )
            print(f"[已删除] {engine}/{container}: {name}")
        except RuntimeError as err:
            failures.append(f"{engine}/{container}: {name}: {err}")
            print(f"[失败] {engine}/{container}: {name}: {err}", file=sys.stderr)

    if failures:
        print(f"\n{len(failures)} 个库删除失败，请检查错误后重试。", file=sys.stderr)
        return 1
    print(f"\n已删除 {len(targets)} 个库。")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
