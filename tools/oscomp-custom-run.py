#!/usr/bin/env python3
"""Build a private OSComp sdcard image for one custom RV64 userspace ELF.

The generic mode compiles or injects one ELF, writes it into a fresh ext4
image under /musl, and uses the known basic-musl OSComp script slot to run it.
The libc-bench mode rebuilds external/libc-bench from a patched copy and uses
the known libcbench-musl slot, which keeps kernel dispatch unchanged.
"""

from __future__ import annotations

import argparse
import os
import re
import shutil
import shlex
import signal
import subprocess
import sys
import tempfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_SOURCE = ROOT / "target/oscomp/testdata-full/sdcard-rv.img"
DEFAULT_PRIVATE_DATA = ROOT / "target/oscomp/custom-run/testdata"
DEFAULT_SUBMIT = ROOT / "target/oscomp/submit"
DEFAULT_SERIAL = ROOT / "target/oscomp/custom-run/os_serial_out_rv.txt"
DEFAULT_BUSYBOX = ROOT / "tools/images/vendor/busybox-riscv64-musl"
PTHREAD_COND_SIGNAL_PROBE = ROOT / "tools/shell-tests/pthread_cond_signal_probe.c"
LIBCBENCH_SOURCE = ROOT / "external/libc-bench"
TX_OBSERVE_BEGIN_NR = 333
TX_OBSERVE_TRACE_ON_NR = 334
TX_OBSERVE_TRACE_OFF_NR = 335

SCRIPT_FOR_SLOT = {
    "basic-musl": "basic_testcode.sh",
    "libcbench-musl": "libcbench_testcode.sh",
}


def render_custom_testcode(marker_group: str, binary_name: str, argv: list[str] | None = None) -> str:
    args = " ".join(argv or [])
    command = f"./{binary_name}"
    if args:
        command = f"{command} {args}"
    return "\n".join(
        [
            "#!/bin/sh",
            f'./busybox echo "#### OS COMP TEST GROUP START {marker_group} ####"',
            f"./busybox echo custom-run:exec:{binary_name}",
            command,
            './busybox echo "custom-run:status:$?"',
            f'./busybox echo "#### OS COMP TEST GROUP END {marker_group} ####"',
            "",
        ]
    )


def render_libcbench_testcode() -> str:
    return "\n".join(
        [
            "#!/bin/sh",
            './busybox echo "#### OS COMP TEST GROUP START libcbench-musl ####"',
            "./libc-bench",
            './busybox echo "#### OS COMP TEST GROUP END libcbench-musl ####"',
            "",
        ]
    )


PTHREAD_BENCH_LINES = [
    "\tRUN(b_pthread_createjoin_serial1, 0);",
    "\tRUN(b_pthread_createjoin_serial2, 0);",
    "\tRUN(b_pthread_create_serial1, 0);",
    "\tRUN(b_pthread_uselesslock, 0);",
    "\tRUN(b_pthread_createjoin_minimal1, 0);",
    "\tRUN(b_pthread_createjoin_minimal2, 0);",
]

MALLOC_BENCH_LINES = [
    "\tRUN(b_malloc_sparse, 0);",
    "\tRUN(b_malloc_bubble, 0);",
    "\tRUN(b_malloc_tiny1, 0);",
    "\tRUN(b_malloc_tiny2, 0);",
    "\tRUN(b_malloc_big1, 0);",
    "\tRUN(b_malloc_big2, 0);",
    "\tRUN(b_malloc_thread_stress, 0);",
    "\tRUN(b_malloc_thread_local, 0);",
]

STDIO_BENCH_LINES = [
    "\tRUN(b_stdio_putcgetc, 0);",
    "\tRUN(b_stdio_putcgetc_unlocked, 0);",
]

REGEX_BENCH_LINES = [
    '\tRUN(b_regex_compile, "(a|b|c)*d*b");',
    '\tRUN(b_regex_search, "(a|b|c)*d*b");',
    '\tRUN(b_regex_search, "a{25}b");',
]

LIBCBENCH_ONLY_LINES = {
    "malloc": MALLOC_BENCH_LINES,
    "malloc-sparse": ["\tRUN(b_malloc_sparse, 0);"],
    "malloc-bubble": ["\tRUN(b_malloc_bubble, 0);"],
    "malloc-big1": ["\tRUN(b_malloc_big1, 0);"],
    "malloc-big2": ["\tRUN(b_malloc_big2, 0);"],
    "malloc-vm": [
        "\tRUN(b_malloc_sparse, 0);",
        "\tRUN(b_malloc_bubble, 0);",
        "\tRUN(b_malloc_big1, 0);",
        "\tRUN(b_malloc_big2, 0);",
    ],
    "stdio": STDIO_BENCH_LINES,
    "stdio-putcgetc": ["\tRUN(b_stdio_putcgetc, 0);"],
    "stdio-putcgetc-unlocked": ["\tRUN(b_stdio_putcgetc_unlocked, 0);"],
    "regex": REGEX_BENCH_LINES,
    "regex-compile": ['\tRUN(b_regex_compile, "(a|b|c)*d*b");'],
    "pthread": PTHREAD_BENCH_LINES,
    "pthread-serial1": ["\tRUN(b_pthread_createjoin_serial1, 0);"],
    "pthread-serial2": ["\tRUN(b_pthread_createjoin_serial2, 0);"],
    "pthread-create-serial1": ["\tRUN(b_pthread_create_serial1, 0);"],
    "pthread-minimal1": ["\tRUN(b_pthread_createjoin_minimal1, 0);"],
    "pthread-minimal2": ["\tRUN(b_pthread_createjoin_minimal2, 0);"],
    "mm-io-pthread": MALLOC_BENCH_LINES + STDIO_BENCH_LINES + PTHREAD_BENCH_LINES,
}


def patch_libcbench_main(
    source: str,
    threshold: int | None,
    phase: str,
    only: str = "all",
    trace_bracket: bool = False,
    body_trace_bracket: bool = False,
) -> str:
    if phase != "pthread":
        raise ValueError(f"unsupported libc-bench phase: {phase}")
    if threshold is not None and (trace_bracket or body_trace_bracket):
        raise ValueError("--observe-threshold and observe bracketing are mutually exclusive")
    if trace_bracket and body_trace_bracket:
        raise ValueError("--observe-bracket and --observe-body-bracket are mutually exclusive")
    if body_trace_bracket:
        selected = LIBCBENCH_ONLY_LINES.get(only)
        if selected is None or len(selected) != 1:
            raise ValueError("--observe-body-bracket requires a single --libcbench-only benchmark")
    if (threshold is not None or trace_bracket or body_trace_bracket) and "#include <sys/syscall.h>" not in source:
        lines = source.splitlines(keepends=True)
        insert_at = None
        for idx, line in enumerate(lines):
            if line.startswith("#include "):
                insert_at = idx + 1
        if insert_at is None:
            raise ValueError("could not find libc-bench include block")
        lines.insert(insert_at, "#include <sys/syscall.h>\nextern long syscall(long, ...);\n")
        source = "".join(lines)
    if body_trace_bracket:
        source = patch_libcbench_run_bench_body_trace(source)
    probe = "" if threshold is None else f"\tsyscall({TX_OBSERVE_BEGIN_NR}, {threshold});\n"
    trace_on = f"\tsyscall({TX_OBSERVE_TRACE_ON_NR});\n" if trace_bracket else ""
    trace_off = f"\tsyscall({TX_OBSERVE_TRACE_OFF_NR});\n" if trace_bracket else ""
    anchor = "\tRUN(b_pthread_createjoin_serial1, 0);\n"
    if (probe and probe in source) or (trace_on and trace_on in source):
        return source
    if only in LIBCBENCH_ONLY_LINES:
        replacement = (
            "int main()\n{\n"
            + probe
            + trace_on
            + "\n".join(LIBCBENCH_ONLY_LINES[only])
            + "\n"
            + trace_off
            + "}\n"
        )
        patched, count = re.subn(r"int main\(\)\s*\{.*?\}\s*$", replacement, source, count=1, flags=re.S)
        if count != 1:
            raise ValueError("could not replace libc-bench main body")
        return patched
    if only != "all":
        raise ValueError(f"unsupported libc-bench selection: {only}")
    if threshold is None and not trace_bracket:
        return source
    if anchor not in source:
        raise ValueError("could not find pthread benchmark anchor")
    return source.replace(anchor, probe + trace_on + anchor + trace_off, 1)


def patch_libcbench_run_bench_body_trace(source: str) -> str:
    target = "\tclock_gettime(CLOCK_REALTIME, &tv0);\n\tbench(params);\n\tprint_stats(tv0);\n"
    replacement = (
        "\tclock_gettime(CLOCK_REALTIME, &tv0);\n"
        f"\tsyscall({TX_OBSERVE_TRACE_ON_NR});\n"
        "\tbench(params);\n"
        f"\tsyscall({TX_OBSERVE_TRACE_OFF_NR});\n"
        "\tprint_stats(tv0);\n"
    )
    patched, count = re.subn(re.escape(target), replacement, source, count=1)
    if count != 1:
        raise ValueError("could not patch libc-bench run_bench body trace window")
    return patched


def patch_libcbench_pthread(
    source: str,
    outer_repeat: int | None,
    inner_repeat: int | None,
    serial_repeat: int | None,
) -> str:
    patched = source
    if outer_repeat is not None and outer_repeat <= 0:
        raise ValueError("--libcbench-outer-repeat must be positive")
    if inner_repeat is not None and inner_repeat <= 0:
        raise ValueError("--libcbench-inner-repeat must be positive")
    if serial_repeat is not None and serial_repeat <= 0:
        raise ValueError("--libcbench-serial-repeat must be positive")
    if outer_repeat is not None:
        target = "for (j=0; j<50; j++)"
        replacement = f"for (j=0; j<{outer_repeat}; j++)"
        patched, count = re.subn(re.escape(target), replacement, patched)
        if count == 0:
            raise ValueError("could not find libc-bench pthread outer loop")
    if inner_repeat is not None:
        target = "sizeof td/sizeof *td"
        replacement = str(inner_repeat)
        patched, count = re.subn(re.escape(target), replacement, patched)
        if count == 0:
            raise ValueError("could not find libc-bench pthread inner loop")
    if serial_repeat is not None:
        target = "for (i=0; i<2500; i++)"
        replacement = f"for (i=0; i<{serial_repeat}; i++)"
        patched, count = re.subn(re.escape(target), replacement, patched)
        if count == 0:
            raise ValueError("could not find libc-bench pthread serial loop")
    return patched


def find_tool(candidates: list[str]) -> str:
    for candidate in candidates:
        path = shutil.which(candidate)
        if path:
            return path
        if Path(candidate).exists():
            return candidate
    raise RuntimeError(f"missing required tool; tried: {', '.join(candidates)}")


def find_debugfs() -> str:
    return find_tool(
        [
            "/opt/homebrew/Cellar/e2fsprogs/1.47.4/sbin/debugfs",
            "/opt/homebrew/sbin/debugfs",
            "debugfs",
        ]
    )


def find_mkfs_ext4() -> str:
    return find_tool(
        [
            "/opt/homebrew/Cellar/e2fsprogs/1.47.4/sbin/mkfs.ext4",
            "/opt/homebrew/sbin/mkfs.ext4",
            "mkfs.ext4",
        ]
    )


def default_cc() -> str:
    if shutil.which("zig"):
        return "zig cc -target riscv64-linux-musl"
    return "riscv64-linux-musl-gcc"


def run(
    cmd: list[str],
    *,
    cwd: Path = ROOT,
    env: dict[str, str] | None = None,
    timeout: int | None = None,
    check: bool = True,
) -> int:
    print("+ " + " ".join(str(part) for part in cmd), flush=True)
    proc = subprocess.Popen(cmd, cwd=cwd, env=env, start_new_session=True)
    try:
        status = proc.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        os.killpg(proc.pid, signal.SIGTERM)
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            os.killpg(proc.pid, signal.SIGKILL)
            proc.wait()
        raise RuntimeError(f"command timed out after {timeout}s: {' '.join(cmd)}")
    if check and status != 0:
        raise RuntimeError(f"command exited with {status}: {' '.join(cmd)}")
    return status


def debugfs_mkdir(debugfs: str, image: Path, ext4_path: str) -> None:
    parent = os.path.dirname(ext4_path) or "/"
    name = os.path.basename(ext4_path)
    script = f"cd {parent}\nmkdir {name}\n"
    subprocess.run([debugfs, "-w", "-f", "-", str(image)], input=script, text=True, check=True)


def debugfs_write(debugfs: str, image: Path, local_path: Path, ext4_path: str) -> None:
    parent = os.path.dirname(ext4_path) or "/"
    name = os.path.basename(ext4_path)
    mode = local_path.stat().st_mode & 0o777
    script = f"cd {parent}\nwrite {local_path} {name}\nsif {name} mode 0{oct(mode)[2:]}\n"
    subprocess.run([debugfs, "-w", "-f", "-", str(image)], input=script, text=True, check=True)


def copy_judge_data(source_data: Path, private_data: Path) -> None:
    private_data.mkdir(parents=True, exist_ok=True)
    if not source_data.exists():
        return
    for item in source_data.iterdir():
        if item.name.startswith("sdcard-"):
            continue
        dest = private_data / item.name
        if item.is_dir():
            if dest.exists():
                shutil.rmtree(dest)
            shutil.copytree(item, dest, symlinks=True)
        elif item.is_file() or item.is_symlink():
            shutil.copy2(item, dest, follow_symlinks=False)


def build_image(payload_musl: Path, output: Path, size_mb: int) -> None:
    debugfs = find_debugfs()
    mkfs = find_mkfs_ext4()
    output.parent.mkdir(parents=True, exist_ok=True)
    if output.exists():
        output.unlink()
    total = sum(path.stat().st_size for path in payload_musl.rglob("*") if path.is_file())
    needed_mb = max(size_mb, int(total / 1048576) + 16)
    run([mkfs, "-F", "-b", "4096", str(output), f"{needed_mb}M"])
    debugfs_mkdir(debugfs, output, "/musl")
    dirs = sorted(
        (path for path in payload_musl.rglob("*") if path.is_dir()),
        key=lambda path: len(path.relative_to(payload_musl).parts),
    )
    for directory in dirs:
        rel = directory.relative_to(payload_musl)
        debugfs_mkdir(debugfs, output, "/musl/" + str(rel))
    for file_path in sorted(path for path in payload_musl.rglob("*") if path.is_file()):
        rel = file_path.relative_to(payload_musl)
        debugfs_write(debugfs, output, file_path, "/musl/" + str(rel))
    print(f"image: {output} ({needed_mb}M)")


def compile_c(source: Path, output: Path, cc: str, extra_cflags: list[str]) -> None:
    output.parent.mkdir(parents=True, exist_ok=True)
    cmd = [*shlex.split(cc), "-static", "-O2", "-o", str(output), str(source), *extra_cflags]
    run(cmd)


def build_libcbench(
    build_dir: Path,
    cc: str,
    threshold: int | None,
    phase: str,
    only: str,
    outer_repeat: int | None,
    inner_repeat: int | None,
    serial_repeat: int | None,
    trace_bracket: bool,
    body_trace_bracket: bool,
) -> Path:
    if not LIBCBENCH_SOURCE.exists():
        raise RuntimeError(f"missing {LIBCBENCH_SOURCE}; initialize submodules first")
    if build_dir.exists():
        shutil.rmtree(build_dir)
    shutil.copytree(LIBCBENCH_SOURCE, build_dir)
    main_c = build_dir / "main.c"
    main_c.write_text(
        patch_libcbench_main(
            main_c.read_text(), threshold, phase, only, trace_bracket, body_trace_bracket
        )
    )
    pthread_c = build_dir / "pthread.c"
    pthread_c.write_text(
        patch_libcbench_pthread(pthread_c.read_text(), outer_repeat, inner_repeat, serial_repeat)
    )
    run(["make", "clean"], cwd=build_dir)
    env = os.environ.copy()
    env["CC"] = cc
    run(["make", "-j1"], cwd=build_dir, env=env)
    elf = build_dir / "libc-bench"
    if not elf.exists():
        raise RuntimeError(f"libc-bench build did not produce {elf}")
    return elf


def prepare_payload(args: argparse.Namespace, elf: Path, binary_name: str, script_name: str, marker_group: str) -> Path:
    work = Path(tempfile.mkdtemp(prefix="oscomp-custom-payload-"))
    musl = work / "musl"
    musl.mkdir(parents=True)
    busybox = Path(args.busybox)
    if not busybox.exists():
        raise RuntimeError(f"missing busybox: {busybox}")
    shutil.copy2(busybox, musl / "busybox")
    os.chmod(musl / "busybox", 0o755)
    shutil.copy2(elf, musl / binary_name)
    os.chmod(musl / binary_name, 0o755)
    if script_name == "libcbench_testcode.sh":
        script = render_libcbench_testcode()
    else:
        script = render_custom_testcode(marker_group, binary_name, args.program_arg)
    script_path = musl / script_name
    script_path.write_text(script)
    os.chmod(script_path, 0o755)
    return work


def qemu_cmdline(group: str, observe_threshold: int | None) -> str:
    parts = ["tx.boot.mode=oscomp", "init=/tx-test-init", "tx.test_init=1"]
    if observe_threshold is not None:
        parts.append(f"tx.oscomp.observe_threshold={observe_threshold}")
    else:
        parts.append("tx.oscomp.observe=0")
    parts.append(f"tx.oscomp.groups={group}")
    return " ".join(parts)


def run_guest(args: argparse.Namespace, group: str) -> None:
    if not args.skip_build:
        run(["cargo", "xtask", "build", "--target", "rv64-qemu"])
    if not args.skip_submit:
        run(["cargo", "xtask", "oscomp", "submit", "--target", "rv64-qemu", "--submit", str(args.submit)])
    run(
        [
            "make",
            f"OSCOMP_DATA={args.data}",
            f"OSCOMP_SUBMIT={args.submit}",
            f"OSCOMP_OUT_RV={args.serial}",
            f"OSCOMP_CMDLINE={qemu_cmdline(group, args.boot_observe_threshold)}",
            "oscomp-qemu-rv64",
        ],
        timeout=args.timeout,
    )
    if args.fault_decode:
        status = run(
            [
                "cargo",
                "xtask",
                "fault-decode",
                "--target",
                "rv64-qemu",
                "--serial",
                str(args.serial),
                "--all",
                "--brief",
            ],
            check=False,
        )
        if status != 0:
            print("fault-decode: no decodable trap lines or decoder returned nonzero")


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--c", dest="c_source", type=Path, help="Compile this C source as the guest ELF")
    mode.add_argument("--elf", type=Path, help="Use an existing RV64 static ELF")
    mode.add_argument("--libcbench", action="store_true", help="Rebuild and inject external/libc-bench")
    mode.add_argument(
        "--pthread-cond-signal-probe",
        action="store_true",
        help="Compile and run tools/shell-tests/pthread_cond_signal_probe.c",
    )
    parser.add_argument("--cc", default=default_cc())
    parser.add_argument("--cflag", action="append", dest="cflags", default=[], help="Extra C compiler flag")
    parser.add_argument("--name", default="custom-elf", help="Injected binary name for --c/--elf")
    parser.add_argument("--program-arg", action="append", default=[], help="Argument passed to generic ELF")
    parser.add_argument("--slot", choices=sorted(SCRIPT_FOR_SLOT), default="basic-musl")
    parser.add_argument("--marker-group", default="custom-run")
    parser.add_argument("--source-data", type=Path, default=DEFAULT_SOURCE.parent)
    parser.add_argument("--data", type=Path, default=DEFAULT_PRIVATE_DATA)
    parser.add_argument("--submit", type=Path, default=DEFAULT_SUBMIT)
    parser.add_argument("--serial", type=Path, default=DEFAULT_SERIAL)
    parser.add_argument("--busybox", type=Path, default=DEFAULT_BUSYBOX)
    parser.add_argument("--image-size-mb", type=int, default=64)
    parser.add_argument("--build-dir", type=Path, default=ROOT / "target/oscomp/custom-run/build")
    parser.add_argument(
        "--observe-threshold",
        type=int,
        help="Threshold embedded in the libc-bench guest syscall(333, N) probe",
    )
    parser.add_argument(
        "--observe-bracket",
        action="store_true",
        help="Wrap the selected libc-bench RUN calls with syscall(334)/syscall(335)",
    )
    parser.add_argument(
        "--observe-body-bracket",
        action="store_true",
        help="Wrap only the selected libc-bench benchmark body with syscall(334)/syscall(335)",
    )
    parser.add_argument(
        "--boot-observe-threshold",
        type=int,
        help="Optional kernel cmdline observe threshold armed before the script starts",
    )
    parser.add_argument("--libcbench-phase", default="pthread", choices=["pthread"])
    parser.add_argument(
        "--libcbench-outer-repeat",
        type=int,
        help="Override pthread.c's outer j<50 batch loop for serial2/minimal2 debugging",
    )
    parser.add_argument(
        "--libcbench-inner-repeat",
        type=int,
        help="Override pthread.c's td[50] inner create/join loop bound for serial2/minimal2 debugging",
    )
    parser.add_argument(
        "--libcbench-serial-repeat",
        type=int,
        help="Override pthread.c's fixed i<2500 serial loop bound for serial1/minimal1 debugging",
    )
    parser.add_argument(
        "--libcbench-only",
        default="all",
        choices=[
            "all",
            "malloc",
            "malloc-sparse",
            "malloc-bubble",
            "malloc-big1",
            "malloc-big2",
            "malloc-vm",
            "stdio",
            "stdio-putcgetc",
            "stdio-putcgetc-unlocked",
            "regex",
            "regex-compile",
            "pthread",
            "pthread-serial1",
            "pthread-serial2",
            "pthread-create-serial1",
            "pthread-minimal1",
            "pthread-minimal2",
            "mm-io-pthread",
        ],
    )
    parser.add_argument("--run", action="store_true", help="Boot QEMU after building the private image")
    parser.add_argument("--skip-build", action="store_true", help="Do not rebuild the kernel before QEMU")
    parser.add_argument("--skip-submit", action="store_true", help="Do not refresh target/oscomp/submit")
    parser.add_argument("--timeout", type=int, default=180, help="QEMU timeout in seconds")
    parser.add_argument("--fault-decode", action="store_true")
    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    args.data = args.data.resolve()
    args.submit = args.submit.resolve()
    args.serial = args.serial.resolve()
    args.source_data = args.source_data.resolve()

    if args.libcbench:
        elf = build_libcbench(
            args.build_dir / "libc-bench-src",
            args.cc,
            args.observe_threshold,
            args.libcbench_phase,
            args.libcbench_only,
            args.libcbench_outer_repeat,
            args.libcbench_inner_repeat,
            args.libcbench_serial_repeat,
            args.observe_bracket,
            args.observe_body_bracket,
        )
        binary_name = "libc-bench"
        slot = "libcbench-musl"
        script_name = SCRIPT_FOR_SLOT[slot]
        marker_group = "libcbench-musl"
    elif args.pthread_cond_signal_probe:
        if not PTHREAD_COND_SIGNAL_PROBE.exists():
            raise RuntimeError(f"missing probe source: {PTHREAD_COND_SIGNAL_PROBE}")
        slot = args.slot
        script_name = SCRIPT_FOR_SLOT[slot]
        marker_group = (
            args.marker_group
            if args.marker_group != "custom-run"
            else "pthread-cond-signal-probe"
        )
        binary_name = args.name if args.name != "custom-elf" else "pthread-cond-signal-probe"
        elf = args.build_dir / binary_name
        compile_c(PTHREAD_COND_SIGNAL_PROBE, elf, args.cc, args.cflags)
    else:
        slot = args.slot
        script_name = SCRIPT_FOR_SLOT[slot]
        marker_group = args.marker_group
        binary_name = args.name
        if args.c_source:
            elf = args.build_dir / binary_name
            compile_c(args.c_source.resolve(), elf, args.cc, args.cflags)
        else:
            elf = args.elf.resolve()
            if not elf.exists():
                raise RuntimeError(f"missing ELF: {elf}")

    payload = prepare_payload(args, elf, binary_name, script_name, marker_group)
    try:
        copy_judge_data(args.source_data, args.data)
        image = args.data / "sdcard-rv.img"
        build_image(payload / "musl", image, args.image_size_mb)
    finally:
        shutil.rmtree(payload, ignore_errors=True)

    print(f"data:   {args.data}")
    print(f"group:  {slot}")
    print(f"serial: {args.serial}")
    if args.run:
        run_guest(args, slot)
    else:
        print("run:    pass --run to boot QEMU")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main(sys.argv[1:]))
    except Exception as exc:
        print(f"error: {exc}", file=sys.stderr)
        raise SystemExit(1)
