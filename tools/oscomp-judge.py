#!/usr/bin/env python3
"""本地 OSComp 评分脚本，不依赖 pygrading。

默认直接使用 testdata 中的官方 judge 口径，不再为 LTP 老格式输出补
Summary、TPASS/TFAIL 细项或 TST_TOTAL 分数。`--official-only` 仍被接受，
但现在只是兼容旧命令行的空操作。

用法: python3 tools/oscomp-judge.py [--official-only] <serial_out.txt> <testdata_dir>
"""

import json
import os
import re
import subprocess
import sys


def main():
    args = sys.argv[1:]
    if "--official-only" in args:
        args.remove("--official-only")

    if len(args) < 2:
        print(f"用法: {sys.argv[0]} [--official-only] <serial_out.txt> <testdata_dir>")
        sys.exit(1)

    serial_file = args[0]
    testdata_dir = args[1]

    with open(serial_file, "rb") as f:
        serial_text = f.read().decode("utf-8", errors="ignore")
    # QEMU/OpenSBI serial streams can contain NUL bytes and RV64 may emit
    # CRCRLF after tty ONLCR plus firmware-side newline handling.  Normalize
    # before feeding strict line-position judges such as basic-musl.
    lines = serial_text.replace("\0", "").replace("\r", "").splitlines(keepends=True)

    judges = {}
    for name in os.listdir(testdata_dir):
        if name.startswith("judge_"):
            group = name[len("judge_"):]
            if group.endswith(".py"):
                group = group[:-3]
            judges[group] = os.path.join(testdata_dir, name)

    start_pat = re.compile(r"#### OS COMP TEST GROUP START ([a-zA-Z0-9-]+) ####")
    end_str = "#### OS COMP TEST GROUP END"

    groups = {}  # group -> [lines]
    current_group = None
    buf = []

    for line in lines:
        m = start_pat.search(line)
        if m:
            if current_group is not None:
                groups[current_group] = buf
            current_group = m.group(1)
            buf = []
        elif end_str in line:
            if current_group is not None:
                groups[current_group] = buf
                current_group = None
                buf = []
        else:
            if current_group is not None:
                buf.append(line)

    if current_group is not None:
        groups[current_group] = buf

    total_pass = 0
    total_all = 0
    results = {}

    for group, group_lines in sorted(groups.items()):
        if group in judges:
            judge_path = judges[group]
            judge_input = "".join(group_lines)
            proc = subprocess.run(
                [sys.executable, judge_path],
                input=judge_input.encode(),
                capture_output=True,
            )
            try:
                data = json.loads(proc.stdout.decode())
            except Exception:
                print(f"[{group}] judge 解析失败: {proc.stderr.decode()[:200]}")
                continue
        else:
            data = fallback_results(group, group_lines)
            if data is None:
                print(f"[{group}] 无对应 judge 脚本，跳过")
                continue

        results[group] = data
        g_pass = sum(item.get("pass", item.get("score", 0)) for item in data)
        g_all = sum(item.get("all", 1) for item in data)
        total_pass += g_pass
        total_all += g_all
        print(f"[{group}] {g_pass}/{g_all}")
        for item in data:
            p = item.get("pass", item.get("score", 0))
            a = item.get("all", 1)
            mark = "?" if a == 0 else ("✓" if p == a else ("~" if p > 0 else "✗"))
            print(f"  {mark} {item['name']}  {p}/{a}")

    print()
    print(f"总分: {total_pass}/{total_all}")


def base_group(group):
    for suffix in ("-musl", "-glibc"):
        if group.endswith(suffix):
            return group[: -len(suffix)]
    return group


def fallback_results(group, group_lines):
    """Parse stable output emitted by official testsuits shell scripts."""
    group = base_group(group)
    text = "".join(group_lines)
    if group == "busybox":
        return command_results(text, r"testcase busybox (.*?) (success|fail)\s*$")
    if group in ("iperf", "netperf"):
        return command_results(
            text, rf"====== {re.escape(group)} (.*?) end: (success|fail) ======"
        )
    return None


def command_results(text, pattern):
    items = []
    for match in re.finditer(pattern, text, flags=re.MULTILINE):
        name, status = match.groups()
        items.append(
            {
                "name": name.strip(),
                "pass": 1 if status == "success" else 0,
                "all": 1,
            }
        )
    return items or None


if __name__ == "__main__":
    main()
