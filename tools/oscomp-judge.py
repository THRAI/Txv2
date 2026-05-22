#!/usr/bin/env python3
"""本地 OSComp 评分脚本，不依赖 pygrading。
用法: python3 tools/oscomp-judge.py <serial_out.txt> <testdata_dir>
"""

import json
import os
import re
import subprocess
import sys

ltp_ret_pat = re.compile(r"^FAIL LTP CASE\s+(\S+)\s+:\s+(-?\d+)\s*$")
ltp_run_pat = re.compile(r"^RUN LTP CASE\s+(\S+)(?:\s+:.*)?\s*$")
ansi_pat = re.compile(r"\x1b\[[0-9;]*m")
ltp_status_pat = re.compile(r"\b(TPASS|TFAIL|TBROK|TCONF|TWARN):")


def parse_ltp_detail_counts(group_lines):
    """Parse per-assertion LTP results from serial output.

    The bundled judge scripts count coloured TPASS/TFAIL lines. Our local
    serial normalization strips ANSI colour escapes, so reproduce that logic
    here against both coloured and plain logs. The runner prints
    "FAIL LTP CASE ... : 0" for successful LTP cases, so the return line is
    only a case boundary; TPASS/TFAIL detail lines carry the useful local
    score.
    """
    current = None
    counts = {}
    ret_by_case = {}

    def ensure(name):
        return counts.setdefault(
            name,
            {"passed": 0, "failed": 0, "broken": 0, "skipped": 0, "warnings": 0},
        )

    for raw_line in group_lines:
        line = ansi_pat.sub("", raw_line.strip())
        m = ltp_run_pat.match(line)
        if m:
            current = m.group(1)
            ensure(current)
            continue

        m = ltp_ret_pat.match(line)
        if m:
            ret_by_case[m.group(1)] = int(m.group(2))
            if current == m.group(1):
                current = None
            continue

        if current is None:
            continue

        m = ltp_status_pat.search(line)
        if not m:
            continue
        bucket = {
            "TPASS": "passed",
            "TFAIL": "failed",
            "TBROK": "broken",
            "TCONF": "skipped",
            "TWARN": "warnings",
        }[m.group(1)]
        ensure(current)[bucket] += 1

    return counts, ret_by_case


def patch_ltp_zero_denominator(group, group_lines, data):
    """Local helper for OSComp LTP smoke runs.

    Some LTP binaries exit directly without printing the common "Summary:"
    block. The bundled judge then reports the case as 0/0, which hides
    crashes and makes return-code-only smoke tests look like passes. Keep the
    official summary counts when present; otherwise score the case as a
    one-point return-code test.
    """
    if not group.startswith("ltp-"):
        return data

    detail_counts, ret_by_case = parse_ltp_detail_counts(group_lines)

    patched = []
    for item in data:
        name = item.get("name")
        counts = detail_counts.get(name)
        if counts:
            total = sum(counts.values())
            if total > 0:
                item = dict(item)
                item["pass"] = counts["passed"]
                item["all"] = total
                item["score"] = counts["passed"]
                item["failed"] = counts["failed"]
                item["broken"] = counts["broken"]
                item["skipped"] = counts["skipped"]
                item["warnings"] = counts["warnings"]
        if item.get("all", 0) == 0:
            if name in ret_by_case:
                ok = 1 if ret_by_case[name] == 0 else 0
                item = dict(item)
                item["pass"] = ok
                item["all"] = 1
                item["score"] = ok
        patched.append(item)
    return patched


def main():
    if len(sys.argv) < 3:
        print(f"用法: {sys.argv[0]} <serial_out.txt> <testdata_dir>")
        sys.exit(1)

    serial_file = sys.argv[1]
    testdata_dir = sys.argv[2]

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

        data = patch_ltp_zero_denominator(group, group_lines, data)
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
