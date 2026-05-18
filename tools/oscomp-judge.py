#!/usr/bin/env python3
"""本地 OSComp 评分脚本，不依赖 pygrading。
用法: python3 tools/oscomp-judge.py <serial_out.txt> <testdata_dir>
"""

import json
import os
import re
import subprocess
import sys


def main():
    if len(sys.argv) < 3:
        print(f"用法: {sys.argv[0]} <serial_out.txt> <testdata_dir>")
        sys.exit(1)

    serial_file = sys.argv[1]
    testdata_dir = sys.argv[2]

    with open(serial_file, "r", encoding="utf-8", errors="ignore") as f:
        lines = f.readlines()

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
        if group not in judges:
            print(f"[{group}] 无对应 judge 脚本，跳过")
            continue
        judge_path = judges[group]
        proc = subprocess.run(
            [sys.executable, judge_path],
            input="".join(group_lines).encode(),
            capture_output=True,
        )
        try:
            data = json.loads(proc.stdout.decode())
        except Exception:
            print(f"[{group}] judge 解析失败: {proc.stderr.decode()[:200]}")
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
            mark = "✓" if p == a else ("~" if p > 0 else "✗")
            print(f"  {mark} {item['name']}  {p}/{a}")

    print()
    print(f"总分: {total_pass}/{total_all}")


if __name__ == "__main__":
    main()
