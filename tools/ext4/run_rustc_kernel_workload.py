#!/usr/bin/env python3
"""Run the RV64 rustc witness through the current named-role shell-test API."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import subprocess
import sys

RESULT_MARKER = "TX_GUEST_RUST_BUILD status="
SUCCESS_MARKER = "TX_GUEST_RUST_BUILD status=0"
DEFAULT_GUEST_RUNNER = "/mnt/ext4-test/run-rustc-kernel-build.sh"


def scenario(guest_runner: str, timeout_ms: int) -> str:
    if not guest_runner.startswith("/"):
        raise ValueError("guest runner must be absolute")
    command = (
        "mkdir -p /mnt/ext4-test; mount -t ext4 -o rw /dev/block/vda /mnt/ext4-test; status=$?; "
        "if [ $status -eq 0 ]; then TX_EXT4_TEST_DEVICE=/dev/block/vda "
        "TX_EXT4_SCRATCH_DEVICE=/dev/block/vdb TX_EXT4_WORKLOAD_DEVICE=/dev/block/vdc "
        f"{guest_runner}; status=$?; fi; marker=TX_GUEST_RUST_BUILD; echo \"$marker status=$status\""
    )
    return "\n".join([
        "# Generated M1/M2 rustc witness.",
        'wait "/ # " within 90000',
        "sleep 500",
        f"send {json.dumps(command + chr(10))}",
        f'expect "{RESULT_MARKER}" within {timeout_ms}',
        "quit",
        "",
    ])


def command(root: Path, script: Path, serial_log: Path, images: list[Path]) -> list[str]:
    return [
        "cargo", "xtask", "shell-test", "--target", "rv64-qemu", "--profile", "alpine",
        "--append-cmdline", "tx.mount.sdcard=0", "--script", str(script), "--serial-log",
        str(serial_log), "--smp", "1", "--memory-mib", "4096", "--ext4-test-image",
        str(images[0]), "--ext4-scratch-image", str(images[1]), "--ext4-workload-image", str(images[2]),
    ]


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--test-image", required=True, type=Path)
    parser.add_argument("--scratch-image", required=True, type=Path)
    parser.add_argument("--workload-image", required=True, type=Path)
    parser.add_argument("--serial-log", required=True, type=Path)
    parser.add_argument("--guest-runner", default=DEFAULT_GUEST_RUNNER)
    parser.add_argument("--timeout-ms", type=int, default=21_600_000)
    parser.add_argument("--scenario", type=Path)
    args = parser.parse_args(argv)
    try:
        if args.timeout_ms <= 0:
            raise ValueError("--timeout-ms must be positive")
        images = [args.test_image, args.scratch_image, args.workload_image]
        for role, image in zip(("TEST", "SCRATCH", "WORKLOAD"), images):
            if not image.is_file():
                raise ValueError(f"missing {role} role image: {image}")
        if args.serial_log.exists():
            raise ValueError(f"refusing to overwrite serial log: {args.serial_log}")
        script = args.scenario or args.serial_log.with_suffix(".scenario")
        if script.exists():
            raise ValueError(f"refusing to overwrite scenario: {script}")
        script.parent.mkdir(parents=True, exist_ok=True)
        script.write_text(scenario(args.guest_runner, args.timeout_ms), encoding="utf-8")
        result = subprocess.run(command(Path.cwd(), script, args.serial_log, images), cwd=Path.cwd(), check=False)
        if result.returncode:
            return result.returncode
        serial = args.serial_log.read_text(encoding="utf-8", errors="replace")
        if SUCCESS_MARKER not in serial:
            raise ValueError("guest build did not report a successful status")
        return 0
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"run-rustc-kernel-workload: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
