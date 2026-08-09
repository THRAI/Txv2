#!/usr/bin/env python3
"""Run read-only e2fsprogs checks in the repository's Linux Docker image."""

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import sys
from pathlib import Path


DEFAULT_IMAGE = "tx-ext4-xfstests-tier1:local"


class E2fsprogsDockerError(Exception):
    pass


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--tool", required=True, choices=("e2fsck", "debugfs"))
    parsed, args = parser.parse_known_args(argv)
    try:
        return run(parsed.tool, args)
    except E2fsprogsDockerError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1


def run(tool: str, args: list[str]) -> int:
    if not args:
        raise E2fsprogsDockerError(f"{tool} requires an image path")
    image = Path(args[-1])
    if not image.is_file():
        raise E2fsprogsDockerError(f"missing ext4 image: {image}")
    docker = shutil.which("docker")
    if docker is None:
        raise E2fsprogsDockerError("docker is unavailable")

    absolute = image.resolve()
    image_args = [*args[:-1], f"/fixture/{absolute.name}"]
    mount = f"type=bind,source={absolute.parent},target=/fixture,readonly"
    completed = subprocess.run(
        [
            docker,
            "run",
            "--rm",
            "--mount",
            mount,
            os.environ.get("TX_EXT4_E2FSPROGS_DOCKER_IMAGE", DEFAULT_IMAGE),
            tool,
            *image_args,
        ],
        check=False,
    )
    return completed.returncode


if __name__ == "__main__":
    raise SystemExit(main())
