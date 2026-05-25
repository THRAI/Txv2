#!/usr/bin/env python3
"""Build a trimmed OSComp SD card image with fine-grained test selection.

Extracts only the requested suites and (optionally) individual test cases
from the canonical sdcard-rv.img / sdcard-la.img, then writes a new, smaller
ext4 image.

Requirements: debugfs + mkfs.ext4 (brew install e2fsprogs on macOS).
No root or loopback mount needed.

Quick usage:
    # List available suites
    python3 tools/build-slim-sdcard.py --list-suites

    # List cases in a suite
    python3 tools/build-slim-sdcard.py --list-cases ltp-musl

    # Build slim image: basic-musl (all) + ltp-musl (3 specific cases)
    python3 tools/build-slim-sdcard.py \
        --suite basic-musl \
        --suite ltp-musl --ltp-cases accept01,writev01,read01 \
        -o sdcard-slim.img

    # Via TOML config:
    python3 tools/build-slim-sdcard.py slim.toml
"""

import argparse
import os
import shutil
import subprocess
import sys
import tempfile


# ── config model ──────────────────────────────────────────────────────────

class SuiteConfig:
    def __init__(self, name: str, cases: list[str] | None = None):
        self.name = name
        self.cases = cases

    def __repr__(self):
        c = f", cases={self.cases}" if self.cases else ""
        return f"SuiteConfig({self.name}{c})"


class SlimConfig:
    def __init__(self, *, source="target/oscomp/testdata/sdcard-rv.img",
                 output="target/oscomp/testdata/sdcard-slim.img",
                 size_mb=256, suites=None, ltp_cases=None):
        self.source = source
        self.output = output
        self.size_mb = size_mb
        self.suites = suites or []
        self.ltp_cases = ltp_cases or []


# ── tool discovery ────────────────────────────────────────────────────────

def _find_debugfs():
    for c in ["/opt/homebrew/Cellar/e2fsprogs/1.47.4/sbin/debugfs",
              "/opt/homebrew/sbin/debugfs", "debugfs"]:
        if shutil.which(c) or os.path.exists(c):
            return c
    raise RuntimeError("debugfs not found. brew install e2fsprogs")


def _find_mkfs_ext4():
    for c in ["/opt/homebrew/Cellar/e2fsprogs/1.47.4/sbin/mkfs.ext4",
              "/opt/homebrew/sbin/mkfs.ext4", "mkfs.ext4"]:
        if shutil.which(c) or os.path.exists(c):
            return c
    raise RuntimeError("mkfs.ext4 not found. brew install e2fsprogs")


# ── debugfs wrappers ──────────────────────────────────────────────────────

def _dfsls(debugfs, image, path):
    r = subprocess.run([debugfs, "-n", "-R", f"ls -l {path}", image],
                       capture_output=True, text=True)
    return r.stdout.strip().split("\n")


def _dfsdump(debugfs, image, ext4_path, local_path, *, executable=False):
    subprocess.run([debugfs, "-n", "-R", f"dump {ext4_path} {local_path}", image],
                   capture_output=True, text=True)
    ok = os.path.exists(local_path) and os.path.getsize(local_path) > 0
    if ok and executable:
        os.chmod(local_path, 0o755)
    return ok


def _dfswrite(debugfs, image, local_path, ext4_path):
    d = os.path.dirname(ext4_path) or "/"
    b = os.path.basename(ext4_path)
    # write file, then set permissions to 755 for scripts/binaries
    st = os.stat(local_path)
    mode = 0o100000 | (st.st_mode & 0o777)
    # debugfs sif takes decimal mode
    cmds = f"cd {d}\nwrite {local_path} {b}\nsif {b} mode 0{oct(mode)[2:]}\n"
    r = subprocess.run([debugfs, "-w", "-f", "-", image],
                       input=cmds, capture_output=True, text=True)
    return r.returncode == 0


def _dfsmkdir(debugfs, image, ext4_path):
    d = os.path.dirname(ext4_path) or "/"
    b = os.path.basename(ext4_path)
    r = subprocess.run([debugfs, "-w", "-f", "-", image],
                       input=f"cd {d}\nmkdir {b}\n",
                       capture_output=True, text=True)
    return r.returncode == 0


# ── suite definitions ─────────────────────────────────────────────────────

SUITE_DEFS = {
    "basic-musl": {
        "dirs": ["basic"],
        "scripts": ["basic_testcode.sh"],
        "extras": [],
        "case_dir": "basic",
        "build_subdirs": ["basic/mnt", "basic/test_chdir", "basic/test_mkdir"],
    },
    "busybox-musl": {
        "dirs": [],
        "scripts": ["busybox_testcode.sh"],
        "extras": ["busybox_cmd.txt"],
        "case_dir": None,
    },
    "libctest-musl": {
        "dirs": ["lib"],
        "scripts": ["libctest_testcode.sh", "run-static.sh", "run-dynamic.sh"],
        "extras": ["runtest.exe", "entry-static.exe", "entry-dynamic.exe"],
        "case_dir": None,
    },
    "lua-musl": {
        "dirs": [],
        "scripts": ["lua_testcode.sh", "test.sh"],
        "extras": ["lua", "date.lua", "file_io.lua", "max_min.lua", "random.lua",
                    "remove.lua", "round_num.lua", "sin30.lua", "sort.lua", "strings.lua"],
        "case_dir": None,
    },
    "libcbench-musl": {
        "dirs": [],
        "scripts": ["libcbench_testcode.sh"],
        "extras": ["libc-bench"],
        "case_dir": None,
    },
    "lmbench-musl": {
        "dirs": [],
        "scripts": ["lmbench_testcode.sh"],
        "extras": ["lmbench_all", "lmbench",
                    "lat_syscall", "lat_select", "lat_sig", "lat_pipe", "lat_proc",
                    "lat_pagefault", "lat_mmap", "lat_fs", "bw_pipe", "bw_file_rd",
                    "bw_mmap_rd", "lat_ctx", "hello", "lmdd"],
        "case_dir": None,
    },
    "iozone-musl": {
        "dirs": [],
        "scripts": ["iozone_testcode.sh"],
        "extras": ["iozone"],
        "case_dir": None,
    },
    "netperf-musl": {
        "dirs": [],
        "scripts": ["netperf_testcode.sh"],
        "extras": ["netperf", "netserver"],
        "case_dir": None,
    },
    "iperf-musl": {
        "dirs": [],
        "scripts": ["iperf_testcode.sh"],
        "extras": ["iperf3"],
        "case_dir": None,
    },
    "cyclictest-musl": {
        "dirs": [],
        "scripts": ["cyclictest_testcode.sh"],
        "extras": ["cyclictest", "hackbench"],
        "case_dir": None,
    },
    "ltp-musl": {
        "dirs": ["ltp"],
        "scripts": [],
        "extras": [],
        "case_dir": "ltp/testcases/bin",
        # When cases specified: extract these infrastructure items instead of
        # the full ltp/ tree
        "infra_dirs": ["lib",
                        "ltp/bin", "ltp/libkirk", "ltp/metadata",
                        "ltp/runtest", "ltp/scenario_groups",
                        "ltp/testscripts", "ltp/testcases/data"],
        "infra_files": ["ltp/kirk", "ltp/runltp-ng", "ltp/ltx",
                         "ltp/IDcheck.sh", "ltp/runltp", "ltp/ver_linux", "ltp/Version"],
    },
    "ltp-glibc": {
        "root": "glibc",
        "dirs": ["ltp"],
        "scripts": [],
        "extras": [],
        "case_dir": "ltp/testcases/bin",
        "infra_dirs": ["lib",
                        "ltp/bin", "ltp/libkirk", "ltp/metadata",
                        "ltp/runtest", "ltp/scenario_groups",
                        "ltp/testscripts", "ltp/testcases/data"],
        "infra_files": ["ltp/kirk", "ltp/runltp-ng", "ltp/ltx",
                         "ltp/IDcheck.sh", "ltp/runltp", "ltp/ver_linux", "ltp/Version"],
    },
    "unixbench-musl": {
        "dirs": [],
        "scripts": ["unixbench_testcode.sh"],
        "extras": ["arithoh", "context1", "dhry2reg", "whetstone-double",
                    "syscall", "pipe", "spawn", "execl", "fstime", "looper",
                    "multi.sh", "short", "int", "long", "float", "double", "hanoi"],
        "case_dir": None,
    },
}

BUSYBOX_FILES = ["busybox"]
BUSYBOX_APPLET_COPIES = ["ip", "ifconfig"]


# ── case listing ──────────────────────────────────────────────────────────

def list_basic_cases(debugfs, image):
    import re
    tmp = tempfile.mktemp(suffix="_run_all.sh")
    if not _dfsdump(debugfs, image, "/musl/basic/run-all.sh", tmp):
        return []
    with open(tmp) as f:
        content = f.read()
    os.unlink(tmp)
    m = re.search(r'tests="(.*?)"', content, re.DOTALL)
    if not m:
        return []
    return [t.strip() for t in m.group(1).strip().split() if t.strip()]


def list_ltp_cases_for_root(debugfs, image, root):
    tmp = tempfile.mktemp(suffix="_syscalls")
    if not _dfsdump(debugfs, image, f"/{root}/ltp/runtest/syscalls", tmp):
        return []
    cases = []
    with open(tmp) as f:
        for line in f:
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            parts = line.split()
            if parts:
                cases.append(parts[0])
    os.unlink(tmp)
    return sorted(set(cases))


def list_ltp_musl_cases(debugfs, image):
    return list_ltp_cases_for_root(debugfs, image, "musl")


def list_ltp_glibc_cases(debugfs, image):
    return list_ltp_cases_for_root(debugfs, image, "glibc")


def list_libctest_cases(debugfs, image):
    tmp = tempfile.mktemp(suffix="_run_static.sh")
    if not _dfsdump(debugfs, image, "/musl/run-static.sh", tmp):
        return []
    cases = []
    with open(tmp) as f:
        for line in f:
            line = line.strip()
            if "runtest.exe -w entry-static.exe" in line:
                parts = line.split()
                if len(parts) >= 4:
                    cases.append(parts[-1])
    os.unlink(tmp)
    return sorted(set(cases))


def list_lua_cases(debugfs, image):
    tmp = tempfile.mktemp(suffix="_lua_testcode.sh")
    if not _dfsdump(debugfs, image, "/musl/lua_testcode.sh", tmp):
        return []
    cases = []
    with open(tmp) as f:
        for line in f:
            line = line.strip()
            if "./test.sh" in line and ".lua" in line:
                parts = line.split()
                if len(parts) >= 2:
                    cases.append(parts[-1].replace(".lua", ""))
    os.unlink(tmp)
    return sorted(set(cases))


def list_busybox_cases(debugfs, image):
    tmp = tempfile.mktemp(suffix="_busybox_cmd.txt")
    if not _dfsdump(debugfs, image, "/musl/busybox_cmd.txt", tmp):
        return []
    cases = []
    with open(tmp) as f:
        for line in f:
            line = line.strip()
            if line and not line.startswith("#"):
                cases.append(line)
    os.unlink(tmp)
    return cases


CASE_LISTERS = {
    "basic-musl": list_basic_cases,
    "ltp-musl": list_ltp_musl_cases,
    "ltp-glibc": list_ltp_glibc_cases,
    "libctest-musl": list_libctest_cases,
    "lua-musl": list_lua_cases,
    "busybox-musl": list_busybox_cases,
}


# ── filtered testcode generators ──────────────────────────────────────────

def gen_basic_testcode(cases):
    lines = [
        './busybox echo "#### OS COMP TEST GROUP START basic-musl ####"',
        'cd ./basic',
    ]
    name_map = {"mkdir_": "mkdir"}
    for c in cases:
        lines.append(f"./{name_map.get(c, c)}")
    lines.append('cd ..')
    lines.append('./busybox echo "#### OS COMP TEST GROUP END basic-musl ####"')
    return "\n".join(lines) + "\n"


def gen_ltp_testcode(cases, libc="musl"):
    lines = [
        '#!/bin/sh',
        f'echo "#### OS COMP TEST GROUP START ltp-{libc} ####"',
        '',
    ]
    for c in cases:
        lines.append(f'echo "RUN LTP CASE {c}"')
        lines.append(f'"ltp/testcases/bin/{c}"')
        lines.append('ret=$?')
        lines.append(f'if [ "$ret" -eq 0 ]; then')
        lines.append(f'  echo "PASS LTP CASE {c} : $ret"')
        lines.append('else')
        lines.append(f'  echo "FAIL LTP CASE {c} : $ret"')
        lines.append('fi')
        lines.append('# OSComp LTP judges use this legacy FAIL line as the')
        lines.append('# end-of-case marker, including for successful ret=0 cases.')
        lines.append(f'if [ "$ret" -eq 0 ]; then')
        lines.append(f'  echo "FAIL LTP CASE {c} : $ret"')
        lines.append('fi')
        lines.append('')
    lines.append(f'echo "#### OS COMP TEST GROUP END ltp-{libc} ####"')
    return "\n".join(lines) + "\n"


def gen_ltp_glibc_testcode(cases):
    return gen_ltp_testcode(cases, libc="glibc")


def gen_lua_testcode(cases):
    lines = [
        './busybox echo "#### OS COMP TEST GROUP START lua-musl ####"',
        '',
    ]
    for c in cases:
        lines.append(f'./test.sh {c}.lua')
    lines.append('')
    lines.append('./busybox echo "#### OS COMP TEST GROUP END lua-musl ####"')
    return "\n".join(lines) + "\n"


def gen_libctest_testcode(cases):
    lines = [
        './busybox echo "#### OS COMP TEST GROUP START libctest-musl ####"',
    ]
    for c in cases:
        lines.append(f'./runtest.exe -w entry-static.exe {c}')
    lines.append('./busybox echo "#### OS COMP TEST GROUP END libctest-musl ####"')
    return "\n".join(lines) + "\n"


def gen_busybox_testcode(cases):
    lines = [
        '#!/busybox sh',
        '',
        './busybox echo "#### OS COMP TEST GROUP START busybox-musl ####"',
        '',
    ]
    for cmd in cases:
        lines.append(f'./busybox {cmd}')
        lines.append('RTN=$?')
        lines.append(f'if [[ $RTN -ne 0 && "{cmd}" != "false" ]] ;then')
        lines.append(f'    echo "testcase busybox {cmd} fail"')
        lines.append('else')
        lines.append(f'    echo "testcase busybox {cmd} success"')
        lines.append('fi')
    lines.append('')
    lines.append('./busybox echo "#### OS COMP TEST GROUP END busybox-musl ####"')
    return "\n".join(lines) + "\n"


TESTCODE_GENS = {
    "basic-musl": gen_basic_testcode,
    "ltp-musl": gen_ltp_testcode,
    "ltp-glibc": gen_ltp_glibc_testcode,
    "lua-musl": gen_lua_testcode,
    "libctest-musl": gen_libctest_testcode,
    "busybox-musl": gen_busybox_testcode,
}


# ── recursive dir extraction ──────────────────────────────────────────────

def extract_dir_recursive(debugfs, image, ext4_dir, local_dir):
    os.makedirs(local_dir, exist_ok=True)
    lines = _dfsls(debugfs, image, ext4_dir)
    for line in lines:
        line = line.strip()
        if not line:
            continue
        parts = line.split()
        if len(parts) < 3:
            continue
        # Find name: scan from end for the first non-numeric, non-date field
        name = None
        for i in range(len(parts) - 1, 4, -1):
            if parts[i] in (".", ".."):
                name = parts[i]
                break
            if ":" in parts[i] or parts[i][0].isdigit():
                continue
            name = parts[i]
            break
        if name is None:
            name = parts[-1]
        if name in (".", ".."):
            continue

        mode = parts[1]
        src = f"{ext4_dir}/{name}"
        dst = os.path.join(local_dir, name)
        if mode.startswith("4"):
            os.makedirs(dst, exist_ok=True)
            extract_dir_recursive(debugfs, image, src, dst)
        else:
            _dfsdump(debugfs, image, src, dst)

    return local_dir


# ── main build ────────────────────────────────────────────────────────────

def build_slim_sdcard(config: SlimConfig):
    source = config.source
    output = config.output
    debugfs = _find_debugfs()
    mkfs = _find_mkfs_ext4()

    if not os.path.exists(source):
        print(f"Error: source image not found: {source}", file=sys.stderr)
        return False

    # ── Step 0: Build extraction plan ──
    files = set(BUSYBOX_FILES)     # /musl/* paths
    dirs = set()                   # /musl/* dirs to extract recursively
    custom_testcodes = {}          # script_name → new content
    ltp_binaries = set()           # LTP binaries to extract from ltp/testcases/bin/
    basic_binaries = set()         # basic binaries from basic/
    glibc_files = set()            # /glibc/* paths
    glibc_dirs = set()             # /glibc/* dirs to extract recursively
    glibc_custom_testcodes = {}     # script_name → new content under /glibc
    glibc_ltp_binaries = set()      # glibc LTP binaries
    seen_suites = set()

    for suite in config.suites:
        name = suite.name
        if name not in SUITE_DEFS:
            print(f"Warning: unknown suite '{name}'", file=sys.stderr)
            continue
        if name in seen_suites:
            continue
        seen_suites.add(name)
        sd = SUITE_DEFS[name]

        if name == "ltp-musl":
            cases = suite.cases or config.ltp_cases
            if cases:
                ltp_binaries = set(cases)
                dirs.update(sd.get("infra_dirs", []))
                files.update(sd.get("infra_files", []))
                custom_testcodes["ltp_testcode.sh"] = gen_ltp_testcode(list(ltp_binaries))
            else:
                dirs.update(sd["dirs"])
        elif name == "ltp-glibc":
            cases = suite.cases or config.ltp_cases
            if cases:
                glibc_ltp_binaries = set(cases)
                glibc_dirs.update(sd.get("infra_dirs", []))
                glibc_files.update(sd.get("infra_files", []))
                glibc_custom_testcodes["ltp_testcode.sh"] = gen_ltp_glibc_testcode(
                    list(glibc_ltp_binaries)
                )
            else:
                glibc_dirs.update(sd["dirs"])
        elif name == "basic-musl" and suite.cases:
            basic_binaries = set(suite.cases)
            custom_testcodes["basic_testcode.sh"] = gen_basic_testcode(list(basic_binaries))
            for sub in sd.get("build_subdirs", []):
                dirs.add(sub)
        elif suite.cases and name in TESTCODE_GENS:
            custom_testcodes[sd["scripts"][0]] = TESTCODE_GENS[name](suite.cases)
            files.update(sd["extras"])
            dirs.update(sd.get("dirs", []))
        else:
            dirs.update(sd.get("dirs", []))
            files.update(sd["scripts"])
            files.update(sd.get("extras", []))

    # ── Print plan ──
    print(f"Suites: {sorted(seen_suites)}")
    print(
        f"Base files: {len(files)}, dirs: {len(dirs)}; "
        f"glibc files: {len(glibc_files)}, glibc dirs: {len(glibc_dirs)}"
    )
    if ltp_binaries:
        print(f"LTP case binaries: {len(ltp_binaries)}")
    if glibc_ltp_binaries:
        print(f"glibc LTP case binaries: {len(glibc_ltp_binaries)}")
    if basic_binaries:
        print(f"Basic case binaries: {len(basic_binaries)}")
    if custom_testcodes or glibc_custom_testcodes:
        print(
            f"Custom testcodes: musl={list(custom_testcodes.keys())}, "
            f"glibc={list(glibc_custom_testcodes.keys())}"
        )

    # ── Step 1: Extract ──
    work = tempfile.mkdtemp(prefix="slim-sdcard-")
    try:
        musl_dir = os.path.join(work, "extract", "musl")
        glibc_dir = os.path.join(work, "extract", "glibc")
        os.makedirs(musl_dir, exist_ok=True)
        if glibc_files or glibc_dirs or glibc_ltp_binaries or glibc_custom_testcodes:
            os.makedirs(glibc_dir, exist_ok=True)

        # Individual files
        for f in sorted(files):
            src = f"/musl/{f}"
            dst = os.path.join(musl_dir, f)
            os.makedirs(os.path.dirname(dst), exist_ok=True)
            ok = _dfsdump(debugfs, source, src, dst, executable=True)
            sz = os.path.getsize(dst) if ok else 0
            tag = f"{sz/1048576:.1f}M" if sz > 1048576 else str(sz)
            status = "" if ok else " MISSING"
            print(f"  {f} ({tag}){status}")

        for f in sorted(glibc_files):
            src = f"/glibc/{f}"
            dst = os.path.join(glibc_dir, f)
            os.makedirs(os.path.dirname(dst), exist_ok=True)
            ok = _dfsdump(debugfs, source, src, dst, executable=True)
            sz = os.path.getsize(dst) if ok else 0
            tag = f"{sz/1048576:.1f}M" if sz > 1048576 else str(sz)
            status = "" if ok else " MISSING"
            print(f"  [glibc] {f} ({tag}){status}")

        busybox_dst = os.path.join(musl_dir, "busybox")
        if os.path.exists(busybox_dst):
            for applet in BUSYBOX_APPLET_COPIES:
                applet_dst = os.path.join(musl_dir, applet)
                shutil.copy2(busybox_dst, applet_dst)
                os.chmod(applet_dst, 0o755)
                print(f"  [busybox applet] {applet} -> busybox")

        # Directories (recursive)
        for d in sorted(dirs):
            src = f"/musl/{d}"
            dst = os.path.join(musl_dir, d)
            extract_dir_recursive(debugfs, source, src, dst)
            print(f"  {d}/ (recursive)")

        for d in sorted(glibc_dirs):
            src = f"/glibc/{d}"
            dst = os.path.join(glibc_dir, d)
            extract_dir_recursive(debugfs, source, src, dst)
            print(f"  [glibc] {d}/ (recursive)")

        # LTP case binaries
        if ltp_binaries:
            bin_dir = os.path.join(musl_dir, "ltp", "testcases", "bin")
            os.makedirs(bin_dir, exist_ok=True)
            for case in sorted(ltp_binaries):
                src = f"/musl/ltp/testcases/bin/{case}"
                dst = os.path.join(bin_dir, case)
                ok = _dfsdump(debugfs, source, src, dst, executable=True)
                sz = os.path.getsize(dst) if ok else 0
                tag = f"{sz/1048576:.1f}M" if sz > 1048576 else str(sz)
                status = "" if ok else " NOT FOUND"
                print(f"  [ltp] {case} ({tag}){status}")

        if glibc_ltp_binaries:
            bin_dir = os.path.join(glibc_dir, "ltp", "testcases", "bin")
            os.makedirs(bin_dir, exist_ok=True)
            for case in sorted(glibc_ltp_binaries):
                src = f"/glibc/ltp/testcases/bin/{case}"
                dst = os.path.join(bin_dir, case)
                ok = _dfsdump(debugfs, source, src, dst, executable=True)
                sz = os.path.getsize(dst) if ok else 0
                tag = f"{sz/1048576:.1f}M" if sz > 1048576 else str(sz)
                status = "" if ok else " NOT FOUND"
                print(f"  [glibc ltp] {case} ({tag}){status}")

        # Basic case binaries
        if basic_binaries:
            basic_dir = os.path.join(musl_dir, "basic")
            os.makedirs(basic_dir, exist_ok=True)
            name_map = {"mkdir_": "mkdir"}
            for case in sorted(basic_binaries):
                binary = name_map.get(case, case)
                src = f"/musl/basic/{binary}"
                dst = os.path.join(basic_dir, binary)
                ok = _dfsdump(debugfs, source, src, dst, executable=True)
                sz = os.path.getsize(dst) if ok else 0
                status = "" if ok else " NOT FOUND"
                print(f"  [basic] {binary} ({sz}){status}")

        # Custom testcode scripts
        for script_name, content in custom_testcodes.items():
            dst = os.path.join(musl_dir, script_name)
            with open(dst, "w") as f:
                f.write(content)
            os.chmod(dst, 0o755)
            print(f"  [custom] {script_name} ({len(content)} bytes)")

        for script_name, content in glibc_custom_testcodes.items():
            dst = os.path.join(glibc_dir, script_name)
            with open(dst, "w") as f:
                f.write(content)
            os.chmod(dst, 0o755)
            print(f"  [glibc custom] {script_name} ({len(content)} bytes)")

        # Generate run-all.sh — chains all present testcode scripts in order.
        # The kernel can exec `cd /musl && sh run-all.sh` instead of a
        # hardcoded chain, which makes the slim image self-describing.
        testcode_order = [
            "basic_testcode.sh", "busybox_testcode.sh", "libctest_testcode.sh",
            "libcbench_testcode.sh", "lua_testcode.sh", "lmbench_testcode.sh",
            "iozone_testcode.sh", "netperf_testcode.sh", "iperf_testcode.sh",
            "cyclictest_testcode.sh", "unixbench_testcode.sh", "ltp_testcode.sh",
        ]
        # The kernel mounts the ext4 at rootfs /musl, so ext4 /musl/ →
        # rootfs /musl/musl/.  cd there so ./busybox and testcode.sh
        # relative references resolve correctly.
        run_all_lines = ["#!/bin/sh", "cd /musl/musl"]
        for tc in testcode_order:
            dst = os.path.join(musl_dir, tc)
            if os.path.exists(dst):
                run_all_lines.append(f"echo '=== {tc} ==='")
                run_all_lines.append(f"sh {tc}")
        run_all_lines.append("echo '=== all suites done ==='")
        run_all_content = "\n".join(run_all_lines) + "\n"
        run_all_dst = os.path.join(musl_dir, "run-all.sh")
        with open(run_all_dst, "w") as f:
            f.write(run_all_content)
        os.chmod(run_all_dst, 0o755)
        print(f"  [generated] run-all.sh ({len(run_all_content)} bytes, {len(run_all_lines)-3} suites)")

        # ── Step 2: Size calculation ──
        total_bytes = 0
        for dirpath, _, filenames in os.walk(os.path.join(work, "extract")):
            for fn in filenames:
                fp = os.path.join(dirpath, fn)
                try:
                    total_bytes += os.path.getsize(fp)
                except OSError:
                    pass
        needed_mb = max(config.size_mb, int(total_bytes / 1048576) + 10)
        print(f"\nExtracted: {total_bytes/1048576:.1f} MB → image size {needed_mb} MB")

        # ── Step 3: Create ext4 ──
        print(f"Creating {output} ...")
        subprocess.run([mkfs, "-F", "-b", "4096", output, f"{needed_mb}M"],
                       check=True, capture_output=True)

        # ── Step 4: Populate ──
        # Build sorted dir list
        all_dirs = []
        for dirpath, dirnames, _ in os.walk(os.path.join(work, "extract")):
            rel = os.path.relpath(dirpath, os.path.join(work, "extract"))
            if rel != ".":
                all_dirs.append("/" + rel)
        all_dirs.sort(key=lambda p: p.count("/"))

        for d in all_dirs:
            _dfsmkdir(debugfs, output, d)

        # Write files
        for dirpath, _, filenames in os.walk(os.path.join(work, "extract")):
            rel = os.path.relpath(dirpath, os.path.join(work, "extract"))
            ext4_dir = "/" + rel if rel != "." else "/"
            for fn in filenames:
                lp = os.path.join(dirpath, fn)
                ep = f"{ext4_dir}/{fn}" if ext4_dir != "/" else f"/{fn}"
                _dfswrite(debugfs, output, lp, ep)

        final_size = os.path.getsize(output)
        print(f"Done: {output} ({final_size/1048576:.1f} MB)")
        return True
    finally:
        shutil.rmtree(work, ignore_errors=True)


# ── CLI ───────────────────────────────────────────────────────────────────

def cmd_list_suites(config):
    source = config.source
    debugfs = _find_debugfs()
    if not os.path.exists(source):
        print(f"Error: source image not found: {source}", file=sys.stderr)
        return
    print(f"Available suites in {source}:\n")
    for name in SUITE_DEFS:
        nc = "?"
        if name in CASE_LISTERS:
            try:
                nc = str(len(CASE_LISTERS[name](debugfs, source)))
            except Exception:
                pass
        print(f"  {name:25s}  ({nc} cases)")


def cmd_list_cases(config, suite):
    source = config.source
    debugfs = _find_debugfs()
    if not os.path.exists(source):
        print(f"Error: source image not found: {source}", file=sys.stderr)
        return
    if suite not in CASE_LISTERS:
        print(f"No case-level filtering for '{suite}'")
        print(f"Supported: {', '.join(sorted(CASE_LISTERS))}")
        return
    cases = CASE_LISTERS[suite](debugfs, source)
    print(f"{suite} ({len(cases)} cases):")
    for c in cases:
        print(f"  {c}")


def parse_config(path):
    try:
        import tomllib
    except ImportError:
        try:
            import tomli as tomllib
        except ImportError:
            import json
            if path:
                with open(path) as f:
                    data = json.load(f)
            else:
                data = json.load(sys.stdin)
            suites = [SuiteConfig(name=s["name"], cases=s.get("cases"))
                      for s in data.get("suites", [])]
            return SlimConfig(
                source=data.get("source", "target/oscomp/testdata/sdcard-rv.img"),
                output=data.get("output", "target/oscomp/testdata/sdcard-slim.img"),
                size_mb=data.get("size_mb", 256),
                suites=suites,
            )
    if path:
        with open(path, "rb") as f:
            data = tomllib.load(f)
    else:
        data = tomllib.load(sys.stdin.buffer)
    suites = [SuiteConfig(name=s["name"], cases=s.get("cases"))
              for s in data.get("suites", [])]
    return SlimConfig(
        source=data.get("source", "target/oscomp/testdata/sdcard-rv.img"),
        output=data.get("output", "target/oscomp/testdata/sdcard-slim.img"),
        size_mb=data.get("size_mb", 256),
        suites=suites,
    )


def main():
    p = argparse.ArgumentParser(description="Build trimmed OSComp SD card image")
    p.add_argument("config", nargs="?")
    p.add_argument("--source", "-s", help="Source sdcard image")
    p.add_argument("--output", "-o", help="Output image path")
    p.add_argument("--list-suites", action="store_true")
    p.add_argument("--list-cases", metavar="SUITE")
    p.add_argument("--suite", action="append", dest="suites", default=[])
    p.add_argument("--ltp-cases", help="Comma-separated LTP case names")
    p.add_argument("--size-mb", type=int, default=None)
    args = p.parse_args()

    src = args.source or "target/oscomp/testdata/sdcard-rv.img"

    if args.list_suites:
        return cmd_list_suites(SlimConfig(source=src))
    if args.list_cases:
        return cmd_list_cases(SlimConfig(source=src), args.list_cases)

    if args.config:
        config = parse_config(args.config)
    elif args.suites:
        ltp_cases = args.ltp_cases.split(",") if args.ltp_cases else []
        suites = [SuiteConfig(name=s) for s in args.suites]
        config = SlimConfig(
            source=src,
            output=args.output or "target/oscomp/testdata/sdcard-slim.img",
            size_mb=args.size_mb or 256,
            suites=suites,
            ltp_cases=ltp_cases,
        )
    else:
        config = parse_config(None)

    if args.source:
        config.source = args.source
    if args.output:
        config.output = args.output
    if args.size_mb:
        config.size_mb = args.size_mb

    ok = build_slim_sdcard(config)
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
