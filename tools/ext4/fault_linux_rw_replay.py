#!/usr/bin/env python3
"""Run the Linux RW replay leg of an ext4 fault matrix.

The fault executor appends one replay image path. The default path is a real
Linux root loop mount. On non-Linux hosts, the repository-owned runner may use a
privileged Docker Linux container; this keeps the executor command immutable
while still requiring a real Linux mount implementation.
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

DOCKER_IMAGE = os.environ.get("TX_EXT4_LINUX_REPLAY_DOCKER_IMAGE", "tx-ext4-e2fsprogs:local")


class LinuxReplayError(Exception):
    pass


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--preflight", action="store_true")
    parser.add_argument("image", type=Path, nargs="?")
    args = parser.parse_args(argv)
    try:
        if args.preflight:
            preflight_replay_environment()
            return 0
        if args.image is None:
            raise LinuxReplayError("missing replay matrix image")
        validate_image(args.image)
        backend = select_replay_backend()
        print(f"linux-rw-replay-backend:{backend}", flush=True)
        if backend == "host":
            return run_linux_rw_replay(args.image)
        if backend == "docker":
            return run_linux_rw_replay_docker(args.image)
        raise LinuxReplayError(f"unsupported Linux RW replay backend: {backend}")
    except LinuxReplayError as err:
        print(f"error: {err}", file=sys.stderr)
        return 1


def validate_host(image: Path) -> None:
    if platform.system() != "Linux":
        raise LinuxReplayError("Linux RW replay requires a Linux host")
    if os.geteuid() != 0:
        raise LinuxReplayError("Linux RW replay requires root for a loop mount")
    validate_image(image)
    missing = [tool for tool in ("mount", "umount", "e2fsck") if shutil.which(tool) is None]
    if missing:
        raise LinuxReplayError("Linux RW replay requires tools: " + ", ".join(missing))


def validate_image(image: Path) -> None:
    if not image.is_file():
        raise LinuxReplayError(f"missing replay matrix image: {image}")


def preflight_replay_environment() -> None:
    backend = select_replay_backend()
    print(f"linux-rw-replay-preflight:{backend}", flush=True)


def select_replay_backend() -> str:
    host_error = host_replay_blocker()
    if host_error is None:
        return "host"
    docker_error = docker_replay_blocker()
    if docker_error is None:
        return "docker"
    raise LinuxReplayError(f"{host_error}; Docker fallback blocked: {docker_error}")


def host_replay_blocker() -> str | None:
    if platform.system() != "Linux":
        return f"Linux RW replay requires a Linux host; current host is {platform.system()}"
    if os.geteuid() != 0:
        return f"Linux RW replay requires root for a loop mount; current uid is {os.geteuid()}"
    missing = [tool for tool in ("mount", "umount", "e2fsck") if shutil.which(tool) is None]
    if missing:
        return "Linux RW replay requires tools: " + ", ".join(missing)
    return None


def docker_replay_blocker() -> str | None:
    docker = shutil.which("docker")
    if docker is None:
        return "docker is unavailable"
    inspect = subprocess.run(
        [docker, "image", "inspect", DOCKER_IMAGE],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
        check=False,
    )
    if inspect.returncode != 0:
        return f"Docker image {DOCKER_IMAGE} is unavailable"
    probe = subprocess.run(
        [
            docker,
            "run",
            "--rm",
            "--privileged",
            DOCKER_IMAGE,
            "sh",
            "-lc",
            (
                "set -eu; tmp=$(mktemp -d); "
                "img=$tmp/probe.img; "
                "dd if=/dev/zero of=$img bs=1M count=16 >/dev/null 2>&1; "
                "mke2fs -q -t ext4 -F $img >/dev/null 2>&1; "
                "mkdir $tmp/mnt; "
                "mount -t ext4 -o loop,rw $img $tmp/mnt; "
                "umount $tmp/mnt; "
                "e2fsck -fn $img >/dev/null"
            ),
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
        check=False,
    )
    if probe.returncode != 0:
        return f"Docker Linux loop-mount probe failed: {last_stderr_line(probe.stderr)}"
    return None


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


def run_linux_rw_replay_docker(image: Path) -> int:
    docker = shutil.which("docker")
    if docker is None:
        raise LinuxReplayError("docker is unavailable")
    absolute = image.resolve()
    parent = absolute.parent
    script = (
        "set -eu; "
        "img=$1; "
        "mountdir=$(mktemp -d); "
        "mounted=0; "
        "cleanup() { "
        'if [ "$mounted" = 1 ]; then umount "$mountdir" || true; fi; '
        'rmdir "$mountdir" || true; '
        "}; "
        "trap cleanup EXIT; "
        'mount -t ext4 -o loop,rw "$img" "$mountdir"; '
        "mounted=1; "
        'umount "$mountdir"; '
        "mounted=0"
    )
    completed = subprocess.run(
        [
            docker,
            "run",
            "--rm",
            "--privileged",
            "-v",
            f"{parent}:/work",
            "-w",
            "/work",
            DOCKER_IMAGE,
            "sh",
            "-lc",
            script,
            "sh",
            absolute.name,
        ],
        check=False,
    )
    if completed.returncode != 0:
        raise LinuxReplayError(
            f"Docker Linux RW replay failed with exit {completed.returncode}"
        )
    return 0


def last_stderr_line(stderr: str) -> str:
    lines = [line.strip() for line in stderr.splitlines() if line.strip()]
    if not lines:
        return "no stderr"
    return lines[-1]


if __name__ == "__main__":
    raise SystemExit(main())
