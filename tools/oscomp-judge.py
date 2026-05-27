#!/usr/bin/env python3
"""本地 OSComp 评分脚本，不依赖 pygrading。

默认使用 testdata 中的官方 judge，再对 LTP 的 0/0 老式输出补一层
TPASS/TFAIL/TBROK/TCONF/TWARN 解析，输出字段仍保持官方的 pass/all/score
结构。使用 --official-only 可查看未适配的官方原始结果。

用法: python3 tools/oscomp-judge.py [--official-only] <serial_out.txt> <testdata_dir>
"""

import json
import os
import re
import subprocess
import sys
from pathlib import Path

ltp_ret_pat = re.compile(r"^FAIL LTP CASE\s+(\S+)\s+:\s+(-?\d+)\s*$")
ltp_run_pat = re.compile(r"^RUN LTP CASE\s+(\S+)(?:\s+:.*)?\s*$")
ansi_pat = re.compile(r"\x1b\[[0-9;]*m")
ltp_status_pat = re.compile(r"\b(TPASS|TFAIL|TBROK|TCONF|TWARN):")

# Older LTP tests in the 20240524 tree use the legacy `test.h` harness and
# may print neither a Summary block nor TPASS/TFAIL detail lines.  The source
# still declares `TST_TOTAL`, so use that as a local fallback when the runner
# gives us only `FAIL LTP CASE <name> : <ret>`.
legacy_ltp_points = {
    "fallocate01": 2,
    "fallocate02": 8,
    "fcntl01": 1,
    "fcntl01_64": 1,
    "fcntl07": 4,
    "fcntl07_64": 4,
    "fcntl09": 2,
    "fcntl09_64": 2,
    "fcntl10": 2,
    "fcntl10_64": 2,
    "fcntl11": 1,
    "fcntl11_64": 1,
    "fcntl14": 1,
    "fcntl14_64": 1,
    "fcntl16": 1,
    "fcntl16_64": 1,
    "fcntl17": 1,
    "fcntl17_64": 1,
    "fcntl18": 1,
    "fcntl18_64": 1,
    "fcntl19": 1,
    "fcntl19_64": 1,
    "fcntl20": 1,
    "fcntl20_64": 1,
    "fcntl21": 1,
    "fcntl21_64": 1,
    "fcntl22": 1,
    "fcntl22_64": 1,
    "fcntl23": 1,
    "fcntl23_64": 1,
    "fcntl24": 1,
    "fcntl24_64": 1,
    "fcntl25": 1,
    "fcntl25_64": 1,
    "fcntl26": 1,
    "fcntl26_64": 1,
    "fcntl31": 5,
    "fcntl31_64": 5,
    "fcntl32": 9,
    "fcntl32_64": 9,
    "fdatasync01": 1,
    "fdatasync02": 2,
    "pipe04": 1,
    "pipe05": 1,
    "pipe09": 1,
    "sockioctl01": 8,
    "writev02": 1,
    "writev05": 1,
    "writev06": 1,
}

symlink01_alias_points = {
    "chdir01A": 3,
    "chmod01A": 3,
    "link01": 2,
    "lstat01A": 3,
    "lstat01A_64": 3,
    "open01A": 5,
    "readlink01A": 4,
    "rename01A": 2,
    "rmdir03A": 1,
    "stat04": 3,
    "stat04_64": 3,
    "unlink01": 1,
}

ROOT = Path(__file__).resolve().parents[1]
LTP_KERNEL_SRC = ROOT / "target/sources/ltp-20240524/testcases/kernel"
_ltp_source_map = None
_ltp_total_cache = {}


def ltp_source_map():
    global _ltp_source_map
    if _ltp_source_map is not None:
        return _ltp_source_map

    sources = {}
    if LTP_KERNEL_SRC.exists():
        for path in LTP_KERNEL_SRC.rglob("*.c"):
            sources.setdefault(path.stem, path)
    _ltp_source_map = sources
    return sources


def ltp_source_names_for_case(name):
    names = [name]
    for suffix in ("_64", "_16"):
        if name.endswith(suffix):
            names.append(name[: -len(suffix)])
    return names


def parse_ltp_tst_total(path):
    text = path.read_text(errors="ignore")
    defines = {
        m.group(1): int(m.group(2))
        for m in re.finditer(r"^\s*#\s*define\s+([A-Za-z_][A-Za-z0-9_]*)\s+(\d+)\b", text, re.M)
    }
    match = re.search(r"\bTST_TOTAL\s*=\s*([^;]+)\s*;", text)
    if not match:
        return None
    expr = match.group(1).strip()
    if expr.isdigit():
        return int(expr)
    size_match = re.fullmatch(
        r"sizeof\s*\(\s*([A-Za-z_][A-Za-z0-9_]*)\s*\)\s*/\s*sizeof\s*\(\s*\*\s*([A-Za-z_][A-Za-z0-9_]*)\s*\)",
        expr,
    )
    if size_match and size_match.group(1) == size_match.group(2):
        return count_c_array_items(text, size_match.group(1))
    if expr.startswith("ARRAY_SIZE"):
        array_match = re.fullmatch(
            r"ARRAY_SIZE\s*\(\s*([A-Za-z_][A-Za-z0-9_]*)\s*\)", expr
        )
        if array_match:
            return count_c_array_items(text, array_match.group(1))
    return defines.get(expr)


def count_c_array_items(text, array_name):
    match = re.search(
        r"^[^;\n]*\b" + re.escape(array_name) + r"\b[^;\n]*=\s*\{",
        text,
        re.M,
    )
    if not match:
        return None

    initializer = extract_c_initializer(text, match.end() - 1)
    if initializer is None:
        return None

    depth = 0
    count = 0
    in_string = False
    escaped = False
    for ch in initializer:
        if in_string:
            if escaped:
                escaped = False
            elif ch == "\\":
                escaped = True
            elif ch == '"':
                in_string = False
            continue
        if ch == '"':
            in_string = True
            continue
        if ch == "{":
            if depth == 0:
                count += 1
            depth += 1
        elif ch == "}":
            depth -= 1

    if count:
        return count
    return count_top_level_items(initializer)


def extract_c_initializer(text, brace_pos):
    depth = 0
    in_string = False
    escaped = False
    start = brace_pos + 1
    for idx, ch in enumerate(text[brace_pos:], brace_pos):
        if in_string:
            if escaped:
                escaped = False
            elif ch == "\\":
                escaped = True
            elif ch == '"':
                in_string = False
            continue
        if ch == '"':
            in_string = True
            continue
        if ch == "{":
            depth += 1
        elif ch == "}":
            depth -= 1
            if depth == 0:
                return text[start:idx]
    return None


def count_top_level_items(initializer):
    initializer = re.sub(r"/\*.*?\*/", "", initializer, flags=re.S)
    initializer = re.sub(r"//.*", "", initializer)
    depth = 0
    items = 0
    token = []
    in_string = False
    escaped = False
    for ch in initializer:
        if in_string:
            token.append(ch)
            if escaped:
                escaped = False
            elif ch == "\\":
                escaped = True
            elif ch == '"':
                in_string = False
            continue
        if ch == '"':
            in_string = True
            token.append(ch)
            continue
        if ch in "({[":
            depth += 1
            token.append(ch)
            continue
        if ch in ")}]":
            depth -= 1
            token.append(ch)
            continue
        if ch == "," and depth == 0:
            if "".join(token).strip():
                items += 1
            token = []
            continue
        token.append(ch)
    if "".join(token).strip():
        items += 1
    return items or None


def legacy_total_for_case(name):
    if name in _ltp_total_cache:
        return _ltp_total_cache[name]

    sources = ltp_source_map()
    total = None
    for source_name in ltp_source_names_for_case(name):
        path = sources.get(source_name)
        if path is None:
            continue
        total = parse_ltp_tst_total(path)
        if total is not None:
            break

    _ltp_total_cache[name] = total
    return total


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


def adapt_ltp_detail_counts(group, group_lines, data):
    """Adapt official LTP scores for logs without Summary blocks.

    The official judge scores the LTP Summary block. Some older LTP cases in
    this testdata print TPASS/TFAIL lines but no Summary, so official output is
    0/0. In that case only, count detail lines and keep the same pass/all/score
    shape. If a case has neither Summary nor detail lines, keep it as 0/0.
    """
    if not group.startswith("ltp-"):
        return data

    detail_counts, ret_by_case = parse_ltp_detail_counts(group_lines)

    patched = []
    for item in data:
        name = item.get("name")
        counts = detail_counts.get(name)
        detail_total = sum(counts.values()) if counts else 0
        if item.get("all", 0) == 0 and detail_total > 0:
            item = dict(item)
            item["pass"] = counts["passed"]
            item["all"] = detail_total
            item["score"] = counts["passed"]
            item["failed"] = counts["failed"]
            item["broken"] = counts["broken"]
            item["skipped"] = counts["skipped"]
            item["warnings"] = counts["warnings"]
        elif item.get("all", 0) == 0:
            total = (
                legacy_ltp_points.get(name)
                or symlink01_alias_points.get(name)
                or legacy_total_for_case(name)
            )
            ret = ret_by_case.get(name)
            if ret is not None and total is not None:
                item = dict(item)
                item["pass"] = total if ret == 0 else 0
                item["all"] = total
                item["score"] = item["pass"]
                item["legacy_ret"] = ret
        patched.append(item)
    return patched


def main():
    args = sys.argv[1:]
    official_only = False
    if "--official-only" in args:
        args.remove("--official-only")
        official_only = True

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
            judge_input = judge_compatible_input(group, group_lines)
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

        if not official_only:
            data = adapt_ltp_detail_counts(group, group_lines, data)
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


def judge_compatible_input(group, group_lines):
    """Adapt local focused-suite output to the official judge input dialect."""
    if base_group(group) != "ltp":
        return "".join(group_lines)

    rewritten = []
    current_case = None
    counts = None
    saw_summary = False
    for idx, line in enumerate(group_lines):
        line = normalize_ltp_result_token(line)
        stripped = line.strip()

        if stripped.startswith("RUN LTP CASE "):
            current_case = stripped.split()[-1]
            counts = new_ltp_counts()
            saw_summary = False

        kind = ltp_result_kind(line)
        if counts is not None and kind is not None:
            counts[kind] += 1

        if stripped == "Summary:":
            saw_summary = True

        if stripped.startswith("FAIL LTP CASE "):
            if current_case is not None and counts and not saw_summary and sum(counts.values()) > 0:
                rewritten.extend(format_ltp_summary(counts))
            current_case = None
            counts = None
            saw_summary = False

        rewritten.append(line)
        if stripped.startswith("PASS LTP CASE "):
            # Official OSComp LTP judges use "FAIL LTP CASE ..." as an
            # end-of-case marker and derive pass counts from preceding TPASS
            # lines. Keep focused logs readable while feeding the legacy
            # marker to the unmodified judge.
            marker = line.replace("PASS LTP CASE", "FAIL LTP CASE", 1)
            if not next_nonempty_line_is(group_lines, idx + 1, marker.strip()):
                if current_case is not None and counts and not saw_summary and sum(counts.values()) > 0:
                    rewritten.extend(format_ltp_summary(counts))
                rewritten.append(marker)
                current_case = None
                counts = None
                saw_summary = False
    return "".join(rewritten)


LTP_OLD_RESULT_TOKENS = [
    ("\x1b[1;32mTPASS\x1b[0m", "\x1b[1;32mTPASS: \x1b[0m"),
    ("\x1b[1;31mTFAIL\x1b[0m", "\x1b[1;31mTFAIL: \x1b[0m"),
    ("\x1b[1;31mTBROK\x1b[0m", "\x1b[1;31mTBROK: \x1b[0m"),
    ("\x1b[1;33mTCONF\x1b[0m", "\x1b[1;33mTCONF: \x1b[0m"),
    ("\x1b[1;35mTWARN\x1b[0m", "\x1b[1;35mTWARN: \x1b[0m"),
]


def normalize_ltp_result_token(line):
    """Normalize older LTP API result lines to the token shape parsed by OSComp."""
    for old_token, new_token in LTP_OLD_RESULT_TOKENS:
        marker = f"{old_token}  :"
        if marker in line:
            return line.replace(marker, new_token, 1)
    return line


def new_ltp_counts():
    return {"passed": 0, "failed": 0, "broken": 0, "skipped": 0, "warnings": 0}


def ltp_result_kind(line):
    if "TPASS:" in line:
        return "passed"
    if "TFAIL:" in line:
        return "failed"
    if "TBROK:" in line:
        return "broken"
    if "TCONF:" in line:
        return "skipped"
    if "TWARN:" in line:
        return "warnings"
    return None


def format_ltp_summary(counts):
    return [
        "\n",
        "Summary:\n",
        f"passed   {counts['passed']}\n",
        f"failed   {counts['failed']}\n",
        f"broken   {counts['broken']}\n",
        f"skipped  {counts['skipped']}\n",
        f"warnings {counts['warnings']}\n",
    ]


def next_nonempty_line_is(lines, start_idx, expected):
    for line in lines[start_idx:]:
        stripped = line.strip()
        if not stripped:
            continue
        return stripped == expected
    return False


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
