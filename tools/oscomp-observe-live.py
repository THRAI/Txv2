#!/usr/bin/env python3
"""Integrated OSComp live-observe workflow.

This is the high-level path for long libc-bench captures: prepare the private
OSComp image, create the QEMU shared-memory backing file, run QEMU, drain
tx-observe rings to raw records, and export analyzer Parquet tables.
"""

from __future__ import annotations

import argparse
import fcntl
import json
import os
import signal
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_SOURCE_DATA = ROOT / "target/oscomp/testdata-full"
DEFAULT_TIMEOUT = 900
DEFAULT_SMP = 4
DEFAULT_RAM_SIZE = "1G"
DEFAULT_RING_BYTES = 2 * 1024 * 1024
DEFAULT_POLL_MS = 1
LOCK_FILENAME = ".observe-live.lock"
LIBCBENCH_TEST_SELECTIONS = {
    "pthread": "pthread",
    "pthread-serial1": "pthread-serial1",
    "pthread-serial2": "pthread-serial2",
    "pthread-create-serial1": "pthread-create-serial1",
    "pthread-minimal1": "pthread-minimal1",
    "pthread-minimal2": "pthread-minimal2",
    "malloc": "malloc",
    "vm": "malloc-vm",
    "malloc-vm": "malloc-vm",
    "malloc-sparse": "malloc-sparse",
    "malloc-bubble": "malloc-bubble",
    "malloc-big1": "malloc-big1",
    "malloc-big2": "malloc-big2",
    "stdio": "stdio",
    "stdio-putcgetc": "stdio-putcgetc",
    "stdio-putcgetc-unlocked": "stdio-putcgetc-unlocked",
    "regex": "regex",
    "regex-compile": "regex-compile",
    "mm-io-pthread": "mm-io-pthread",
}


@dataclass(frozen=True)
class WorkflowLayout:
    base: Path
    host_dir: Path
    guest_mem: Path
    stop_file: Path
    data: Path
    submit: Path
    build_dir: Path
    serial: Path
    test_initrd: Path
    names: Path
    rawrecords: Path
    analysis_dir: Path
    parquet_dir: Path
    cache_dir: Path
    analyze_txt: Path
    report: Path


@dataclass(frozen=True)
class WorkflowArgs:
    name: str
    output_dir: Path | None
    python_file: Path | None
    only: str
    timeout: int
    smp: int
    ram_size: str
    ring_bytes: int
    poll_ms: int
    source_data: Path
    skip_build: bool
    skip_submit: bool
    keep_guest_mem: bool
    dry_run: bool


@dataclass(frozen=True)
class PlannedCommand:
    label: str
    argv: list[str]


@dataclass(frozen=True)
class WorkflowPlan:
    layout: WorkflowLayout
    commands: list[PlannedCommand]


class TargetRunLock:
    """Non-blocking per-output-dir guard for live observe runs."""

    def __init__(self, layout: WorkflowLayout) -> None:
        self.layout = layout
        self.path = layout.base / LOCK_FILENAME
        self.fd: int | None = None

    def __enter__(self) -> "TargetRunLock":
        self.layout.base.mkdir(parents=True, exist_ok=True)
        fd = os.open(self.path, os.O_RDWR | os.O_CREAT, 0o644)
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as exc:
            detail = read_lock_detail(self.path)
            os.close(fd)
            suffix = f": {detail}" if detail else ""
            raise RuntimeError(
                f"observe-live target already has an active run: {self.layout.base}{suffix}"
            ) from exc
        except Exception:
            os.close(fd)
            raise

        self.fd = fd
        payload = {
            "schema": "tx-oscomp-observe-live-lock-v0",
            "pid": os.getpid(),
            "target": str(self.layout.base),
            "started_unix_ms": int(time.time() * 1000),
        }
        encoded = (json.dumps(payload, sort_keys=True) + "\n").encode()
        os.ftruncate(fd, 0)
        os.write(fd, encoded)
        os.fsync(fd)
        return self

    def __exit__(self, _exc_type, _exc, _tb) -> None:
        if self.fd is None:
            return
        try:
            fcntl.flock(self.fd, fcntl.LOCK_UN)
        finally:
            os.close(self.fd)
            self.fd = None


def read_lock_detail(path: Path) -> str:
    try:
        raw = path.read_text().strip()
    except OSError:
        return ""
    if not raw:
        return ""
    try:
        payload = json.loads(raw)
    except json.JSONDecodeError:
        return raw
    pid = payload.get("pid")
    started = payload.get("started_unix_ms")
    if pid is None:
        return raw
    if started is None:
        return f"pid={pid}"
    return f"pid={pid}, started_unix_ms={started}"


def select_libcbench_test(name: str) -> str:
    try:
        return LIBCBENCH_TEST_SELECTIONS[name]
    except KeyError:
        choices = ", ".join(sorted(LIBCBENCH_TEST_SELECTIONS))
        raise ValueError(f"unknown --test {name!r}; expected one of: {choices}")


def build_layout(root: Path, name: str, output_dir: Path | None = None) -> WorkflowLayout:
    base = output_dir if output_dir is not None else root / "target/oscomp/custom-run" / name
    host_dir = base / "host"
    analysis_dir = base / "analysis"
    return WorkflowLayout(
        base=base,
        host_dir=host_dir,
        guest_mem=host_dir / "guest-ram.bin",
        stop_file=host_dir / "stop",
        data=base / "data",
        submit=base / "submit",
        build_dir=base / "build",
        serial=base / "serial.txt",
        test_initrd=root / "target/images/test-init-initramfs-rv64-qemu.cpio",
        names=base / "names.json",
        rawrecords=host_dir / "trace.rawrecords",
        analysis_dir=analysis_dir,
        parquet_dir=analysis_dir / "parquet",
        cache_dir=analysis_dir / "cache",
        analyze_txt=analysis_dir / "analyze.txt",
        report=base / "report.json",
    )


def qemu_command(layout: WorkflowLayout, args: WorkflowArgs) -> list[str]:
    cmdline = "tx.boot.mode=oscomp init=/tx-test-init tx.test_init=1 tx.oscomp.observe=0 tx.oscomp.observe_live_drain=1 tx.oscomp.groups=libcbench-musl"
    return [
        "qemu-system-riscv64",
        "-object",
        f"memory-backend-file,id=txram,size={args.ram_size},mem-path={layout.guest_mem},share=on",
        "-machine",
        "virt,memory-backend=txram",
        "-kernel",
        str(layout.submit / "kernel-rv"),
        "-m",
        args.ram_size,
        "-nographic",
        "-smp",
        str(args.smp),
        "-bios",
        "default",
        "-drive",
        f"file={layout.data / 'sdcard-rv.img'},if=none,format=raw,id=x0,file.locking=off",
        "-device",
        "virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0",
        "-no-reboot",
        "-device",
        "virtio-net-device,netdev=net",
        "-netdev",
        "user,id=net",
        "-rtc",
        "base=utc",
        "-serial",
        f"file:{layout.serial}",
        "-initrd",
        str(layout.test_initrd),
        "-append",
        cmdline,
    ]


def build_plan(root: Path, args: WorkflowArgs, layout: WorkflowLayout) -> WorkflowPlan:
    custom_run = [
        sys.executable,
        str(root / "tools/oscomp-custom-run.py"),
        "--libcbench",
        "--libcbench-only",
        args.only,
        "--observe-bracket",
        "--source-data",
        str(args.source_data),
        "--data",
        str(layout.data),
        "--build-dir",
        str(layout.build_dir),
        "--submit",
        str(layout.submit),
        "--serial",
        str(layout.serial),
    ]
    if args.skip_build:
        custom_run.append("--skip-build")
    if args.skip_submit:
        custom_run.append("--skip-submit")

    commands = [PlannedCommand("prepare-image", custom_run)]
    if not args.skip_build:
        commands.append(
            PlannedCommand("build-kernel", ["cargo", "xtask", "build", "--target", "rv64-qemu"])
        )
    if not args.skip_submit:
        commands.append(
            PlannedCommand(
                "submit-kernel",
                [
                    "cargo",
                    "xtask",
                    "oscomp",
                    "submit",
                    "--target",
                    "rv64-qemu",
                    "--submit",
                    str(layout.submit),
                ],
            )
        )
    commands.extend(
        [
            PlannedCommand(
                "names",
                [
                    "cargo",
                    "xtask",
                    "observe",
                    "names",
                    "--kernel",
                    str(layout.submit / "kernel-rv"),
                    "--output",
                    str(layout.names),
                ],
            ),
            PlannedCommand("truncate-guest-mem", ["truncate", "-s", args.ram_size, str(layout.guest_mem)]),
            PlannedCommand(
                "test-initramfs",
                ["cargo", "xtask", "image", "test-init", "--profile", "busybox", "--target", "rv64-qemu"],
            ),
            PlannedCommand("qemu", qemu_command(layout, args)),
            PlannedCommand(
                "live-drain",
                [
                    "cargo",
                    "xtask",
                    "observe",
                    "live-guest-mem",
                    "--guest-mem",
                    str(layout.guest_mem),
                    "--kernel",
                    str(layout.submit / "kernel-rv"),
                    "--output-dir",
                    str(layout.host_dir),
                    "--hart-count",
                    str(args.smp),
                    "--ring-bytes",
                    str(args.ring_bytes),
                    "--poll-ms",
                    str(args.poll_ms),
                    "--stop-file",
                    str(layout.stop_file),
                    "--names",
                    str(layout.names),
                ],
            ),
            PlannedCommand(
                "analyze",
                analyze_command(layout, args.python_file),
            ),
        ]
    )
    return WorkflowPlan(layout=layout, commands=commands)


def analyze_command(layout: WorkflowLayout, python_file: Path | None) -> list[str]:
    cmd = [
        sys.executable,
        str(ROOT / "tools/tx-observe-analyze.py"),
        "--rawrecords",
        str(layout.rawrecords),
        "--names",
        str(layout.names),
        "--cache-dir",
        str(layout.cache_dir),
        "--parquet-dir",
        str(layout.parquet_dir),
        "--no-sort",
    ]
    if python_file is not None:
        cmd.extend(["--python-file", str(python_file)])
    return cmd


def run_command(command: PlannedCommand, *, cwd: Path, stdout_path: Path | None = None) -> None:
    print("+ " + " ".join(command.argv), flush=True)
    stdout = None
    handle = None
    try:
        if stdout_path is not None:
            stdout_path.parent.mkdir(parents=True, exist_ok=True)
            handle = stdout_path.open("w")
            stdout = handle
        status = subprocess.run(command.argv, cwd=cwd, stdout=stdout).returncode
    finally:
        if handle is not None:
            handle.close()
    if status != 0:
        raise RuntimeError(f"{command.label} exited with {status}")


def run_workflow(root: Path, args: WorkflowArgs) -> WorkflowLayout:
    layout = build_layout(root, args.name, args.output_dir)
    plan = build_plan(root, args, layout)

    if args.dry_run:
        for command in plan.commands:
            print(f"{command.label}: " + " ".join(command.argv))
        return layout

    with TargetRunLock(layout):
        prepare_dirs(layout)
        qemu_idx = next(
            (idx for idx, command in enumerate(plan.commands) if command.label == "qemu"),
            None,
        )
        if qemu_idx is None:
            raise RuntimeError("internal error: workflow plan has no qemu stage")
        for command in plan.commands[:qemu_idx]:
            run_command(command, cwd=root)

        run_qemu_with_drain(root, layout, plan.commands[qemu_idx], plan.commands[qemu_idx + 1], args.timeout)
        run_command(plan.commands[qemu_idx + 2], cwd=root, stdout_path=layout.analyze_txt)
        write_report(layout, args)
        if not args.keep_guest_mem:
            layout.guest_mem.unlink(missing_ok=True)
    return layout


def prepare_dirs(layout: WorkflowLayout) -> None:
    layout.base.mkdir(parents=True, exist_ok=True)
    layout.host_dir.mkdir(parents=True, exist_ok=True)
    layout.analysis_dir.mkdir(parents=True, exist_ok=True)
    layout.data.mkdir(parents=True, exist_ok=True)
    layout.submit.mkdir(parents=True, exist_ok=True)
    layout.stop_file.unlink(missing_ok=True)


def run_qemu_with_drain(
    root: Path,
    layout: WorkflowLayout,
    qemu: PlannedCommand,
    drain: PlannedCommand,
    timeout: int,
) -> None:
    print("+ " + " ".join(qemu.argv), flush=True)
    qemu_proc = subprocess.Popen(qemu.argv, cwd=root, start_new_session=True)
    time.sleep(0.5)
    print("+ " + " ".join(drain.argv), flush=True)
    drain_proc = subprocess.Popen(drain.argv, cwd=root, start_new_session=True)
    try:
        qemu_status = qemu_proc.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        terminate_process_group(qemu_proc)
        layout.stop_file.touch()
        terminate_process_group(drain_proc)
        raise RuntimeError(f"qemu timed out after {timeout}s")
    finally:
        layout.stop_file.touch()

    try:
        drain_status = drain_proc.wait(timeout=30)
    except subprocess.TimeoutExpired:
        terminate_process_group(drain_proc)
        raise RuntimeError("live drain did not exit after QEMU stopped")

    if qemu_status != 0:
        raise RuntimeError(f"qemu exited with {qemu_status}")
    if drain_status != 0:
        raise RuntimeError(f"live drain exited with {drain_status}")


def terminate_process_group(proc: subprocess.Popen) -> None:
    if proc.poll() is not None:
        return
    os.killpg(proc.pid, signal.SIGTERM)
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        os.killpg(proc.pid, signal.SIGKILL)
        proc.wait()


def write_report(layout: WorkflowLayout, args: WorkflowArgs) -> None:
    runtime_path = layout.host_dir / "runtime.json"
    runtime = None
    if runtime_path.exists():
        runtime = json.loads(runtime_path.read_text())
    report = {
        "schema": "tx-oscomp-observe-live-report-v0",
        "name": args.name,
        "base": str(layout.base),
        "serial": str(layout.serial),
        "rawrecords": str(layout.rawrecords),
        "runtime": str(runtime_path),
        "analyze": str(layout.analyze_txt),
        "parquet_dir": str(layout.parquet_dir),
        "runtime_summary": runtime,
    }
    layout.report.write_text(json.dumps(report, indent=2) + "\n")


def parse_args(argv: list[str]) -> WorkflowArgs:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--name", default=f"observe-live-{time.strftime('%Y%m%d-%H%M%S')}", help=argparse.SUPPRESS)
    parser.add_argument("--output-dir", type=Path)
    parser.add_argument("--python-file", type=Path)
    parser.add_argument(
        "--test",
        default="pthread",
        choices=sorted(LIBCBENCH_TEST_SELECTIONS),
        help="libc-bench subset to run",
    )
    parser.add_argument("--only", default="pthread", help=argparse.SUPPRESS)
    parser.add_argument("--timeout", type=int, default=DEFAULT_TIMEOUT, help=argparse.SUPPRESS)
    parser.add_argument("--smp", type=int, default=DEFAULT_SMP, help=argparse.SUPPRESS)
    parser.add_argument("--ram-size", default=DEFAULT_RAM_SIZE, help=argparse.SUPPRESS)
    parser.add_argument("--ring-bytes", type=int, default=DEFAULT_RING_BYTES, help=argparse.SUPPRESS)
    parser.add_argument("--poll-ms", type=int, default=DEFAULT_POLL_MS, help=argparse.SUPPRESS)
    parser.add_argument("--source-data", type=Path, default=DEFAULT_SOURCE_DATA, help=argparse.SUPPRESS)
    parser.add_argument("--skip-build", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--skip-submit", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--keep-guest-mem", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--dry-run", action="store_true")
    ns = parser.parse_args(argv)
    selected = ns.only if "--only" in argv else select_libcbench_test(ns.test)
    return WorkflowArgs(
        name=ns.name,
        output_dir=ns.output_dir.resolve() if ns.output_dir else None,
        python_file=ns.python_file,
        only=selected,
        timeout=ns.timeout,
        smp=ns.smp,
        ram_size=ns.ram_size,
        ring_bytes=ns.ring_bytes,
        poll_ms=ns.poll_ms,
        source_data=ns.source_data,
        skip_build=ns.skip_build,
        skip_submit=ns.skip_submit,
        keep_guest_mem=ns.keep_guest_mem,
        dry_run=ns.dry_run,
    )


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    layout = run_workflow(ROOT, args)
    print(f"observe-live: output {layout.base}")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main(sys.argv[1:]))
    except Exception as exc:
        print(f"error: {exc}", file=sys.stderr)
        raise SystemExit(1)
