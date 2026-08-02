#!/usr/bin/env python3
"""Run the Tx remount/readback leg of an ext4 fault matrix.

The fault executor appends one matrix image path. The image is attached as the
SCRATCH role and checked by a repository-owned shell-test script.
"""

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import sys
from pathlib import Path


class TxRemountError(Exception):
    pass


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("case", nargs="?")
    parser.add_argument("cut", nargs="?")
    parser.add_argument("image", type=Path)
    args = parser.parse_args(argv)
    root = Path(__file__).resolve().parents[2]
    try:
        command = build_shell_test_command(args.image, root, dict(os.environ))
    except TxRemountError as err:
        print(f"error: {err}", file=sys.stderr)
        return 1
    completed = subprocess.run(command, cwd=root, check=False)
    return completed.returncode


def build_shell_test_command(image: Path, root: Path, env: dict[str, str]) -> list[str]:
    if not image.is_file():
        raise TxRemountError(f"missing replay matrix image: {image}")
    cargo = env.get("TX_EXT4_FAULT_TX_CARGO", "cargo")
    if shutil.which(cargo) is None:
        raise TxRemountError(f"missing cargo command: {cargo}")
    target = env.get("TX_EXT4_FAULT_TX_TARGET", "rv64-qemu")
    profile = env.get("TX_EXT4_FAULT_TX_PROFILE", "alpine")
    timeout = env.get("TX_EXT4_FAULT_TX_TIMEOUT_MS", "120000")
    if not timeout.isdigit() or int(timeout) <= 0:
        raise TxRemountError("TX_EXT4_FAULT_TX_TIMEOUT_MS must be positive")
    script_value = env.get(
        "TX_EXT4_FAULT_TX_SCRIPT", "tools/shell-tests/ext4-fault-tx-remount.txt"
    )
    script = Path(script_value)
    if not script.is_absolute():
        script = root / script
    if not script.is_file():
        raise TxRemountError(f"missing Tx remount shell-test script: {script}")
    return [
        cargo,
        "xtask",
        "shell-test",
        "--target",
        target,
        "--profile",
        profile,
        "--script",
        str(script),
        "--timeout-ms",
        timeout,
        "--extra-rv64-ext4",
        str(image),
    ]


if __name__ == "__main__":
    raise SystemExit(main())
