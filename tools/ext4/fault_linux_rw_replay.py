#!/usr/bin/env python3
"""Run the Linux RW replay leg of an ext4 fault matrix.

The fault executor appends one replay image path. This runner intentionally
requires a Linux root host with loop-mount tooling; it never emulates a Linux
mount on another host.
"""

from __future__ import annotations

import argparse
import os
import platform
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path


class LinuxReplayError(Exception):
    pass


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("image", type=Path)
    args = parser.parse_args(argv)
    try:
        validate_host(args.image)
        return run_linux_rw_replay(args.image)
    except LinuxReplayError as err:
        print(f"error: {err}", file=sys.stderr)
        return 1


def validate_host(image: Path) -> None:
    if platform.system() != "Linux":
        raise LinuxReplayError("Linux RW replay requires a Linux host")
    if os.geteuid() != 0:
        raise LinuxReplayError("Linux RW replay requires root for a loop mount")
    if not image.is_file():
        raise LinuxReplayError(f"missing replay matrix image: {image}")
    missing = [tool for tool in ("mount", "umount", "e2fsck") if shutil.which(tool) is None]
    if missing:
        raise LinuxReplayError("Linux RW replay requires tools: " + ", ".join(missing))


def run_linux_rw_replay(image: Path) -> int:
    with tempfile.TemporaryDirectory(prefix="tx-ext4-linux-replay-") as mount_dir:
        mount_point = Path(mount_dir)
        mounted = False
        try:
            mount = subprocess.run(
                ["mount", "-t", "ext4", "-o", "loop,rw", str(image), str(mount_point)],
                check=False,
            )
            if mount.returncode != 0:
                raise LinuxReplayError(f"Linux RW replay mount failed: exit {mount.returncode}")
            mounted = True
            unmount = subprocess.run(["umount", str(mount_point)], check=False)
            if unmount.returncode != 0:
                raise LinuxReplayError(f"Linux RW replay unmount failed: exit {unmount.returncode}")
            mounted = False
        finally:
            if mounted:
                subprocess.run(["umount", str(mount_point)], check=False)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
