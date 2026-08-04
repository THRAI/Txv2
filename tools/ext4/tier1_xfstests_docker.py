#!/usr/bin/env python3
"""Run the ext4 Tier 1 xfstests selection in a repo-owned Docker wrapper."""

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import sys
from pathlib import Path

DEFAULT_IMAGE = os.environ.get(
    "TX_EXT4_XFSTESTS_DOCKER_IMAGE", "tx-ext4-e2fsprogs:local"
)
IMAGE_SIZE_MIB = int(os.environ.get("TX_EXT4_XFSTESTS_IMAGE_MIB", "512"))

CASE_HELPERS = {
    "generic/013": ("ltp/fsstress",),
    "generic/035": ("src/t_rename_overwrite",),
    "generic/091": ("src/feature", "src/min_dio_alignment", "ltp/fsx"),
    "generic/095": ("src/min_dio_alignment",),
    "generic/388": ("ltp/fsstress", "src/godown"),
    "generic/475": ("ltp/fsstress", "src/godown"),
}


class XfstestsDockerError(Exception):
    pass


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--preflight", action="store_true")
    parser.add_argument("--image", default=DEFAULT_IMAGE)
    parser.add_argument("--xfstests-root", type=Path, required=True)
    parser.add_argument("--work-dir", type=Path)
    parser.add_argument("--case", action="append", default=[])
    parser.add_argument("check_args", nargs=argparse.REMAINDER)
    args = parser.parse_args(argv)
    try:
        root = args.xfstests_root.resolve()
        cases = selected_cases(args.case, args.check_args)
        validate_source(root, cases)
        preflight_docker(args.image, root, cases)
        if args.preflight:
            print("xfstests-docker-preflight:ok", flush=True)
            return 0
        if args.work_dir is None:
            raise XfstestsDockerError("--work-dir is required outside --preflight")
        check_args = strip_remainder_separator(args.check_args)
        if not check_args:
            raise XfstestsDockerError("missing xfstests ./check arguments")
        return run_check(args.image, root, args.work_dir.resolve(), check_args)
    except XfstestsDockerError as err:
        print(f"error: {err}", file=sys.stderr)
        return 1


def selected_cases(case_flags: list[str], check_args: list[str]) -> list[str]:
    cases = list(case_flags)
    for arg in strip_remainder_separator(check_args):
        if arg.startswith("-"):
            continue
        cases.append(arg)
    return cases


def strip_remainder_separator(args: list[str]) -> list[str]:
    if args and args[0] == "--":
        return args[1:]
    return args


def validate_source(root: Path, cases: list[str]) -> None:
    check = root / "check"
    if not check.is_file():
        raise XfstestsDockerError(f"missing xfstests check script: {check}")
    missing = [
        str(path)
        for path in required_helper_paths(root, cases)
        if not path.is_file() or not os.access(path, os.X_OK)
    ]
    if missing:
        raise XfstestsDockerError(
            "pinned xfstests source is not built; missing executable helpers: "
            + ", ".join(missing)
        )


def required_helper_paths(root: Path, cases: list[str]) -> list[Path]:
    helpers = set()
    for case in cases:
        helpers.update(CASE_HELPERS.get(case, ()))
    return [root / helper for helper in sorted(helpers)]


def preflight_docker(image: str, root: Path, cases: list[str]) -> None:
    docker = shutil.which("docker")
    if docker is None:
        raise XfstestsDockerError("docker is unavailable")
    inspect = subprocess.run(
        [docker, "image", "inspect", image],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
        check=False,
    )
    if inspect.returncode != 0:
        raise XfstestsDockerError(f"Docker image {image} is unavailable")
    probe = subprocess.run(
        [
            docker,
            "run",
            "--rm",
            "--privileged",
            "--mount",
            f"type=bind,source={root},target=/xfstests,readonly",
            image,
            "sh",
            "-lc",
            docker_preflight_script(cases),
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
        check=False,
    )
    if probe.returncode != 0:
        raise XfstestsDockerError(
            f"Docker Linux xfstests preflight failed: {last_stderr_line(probe.stderr)}"
        )


def docker_preflight_script(cases: list[str]) -> str:
    helper_checks = []
    for helper in sorted({helper for case in cases for helper in CASE_HELPERS.get(case, ())}):
        helper_checks.append(f"test -x /xfstests/{helper}")
    helpers = "; ".join(helper_checks) or "true"
    return (
        "set -eu; "
        "command -v mount >/dev/null; "
        "command -v umount >/dev/null; "
        "command -v losetup >/dev/null; "
        "command -v mke2fs >/dev/null; "
        "command -v e2fsck >/dev/null; "
        "command -v xfs_io >/dev/null; "
        "test -x /xfstests/check; "
        f"{helpers}; "
        "tmp=$(mktemp -d); "
        "img=$tmp/probe.img; "
        "dd if=/dev/zero of=$img bs=1M count=16 >/dev/null 2>&1; "
        "mke2fs -q -t ext4 -F $img >/dev/null 2>&1; "
        "mkdir $tmp/mnt; "
        "mount -t ext4 -o loop,rw $img $tmp/mnt; "
        "umount $tmp/mnt; "
        "e2fsck -fn $img >/dev/null; "
        "rm -rf $tmp"
    )


def run_check(image: str, root: Path, work_dir: Path, check_args: list[str]) -> int:
    work_dir.mkdir(parents=True, exist_ok=True)
    test_img = work_dir / "xfstests-test.img"
    scratch_img = work_dir / "xfstests-scratch.img"
    ensure_image(test_img)
    ensure_image(scratch_img)
    for path in (work_dir / "mnt" / "test", work_dir / "mnt" / "scratch", work_dir / "results"):
        path.mkdir(parents=True, exist_ok=True)
    docker = shutil.which("docker")
    if docker is None:
        raise XfstestsDockerError("docker is unavailable")
    command = [
        docker,
        "run",
        "--rm",
        "--privileged",
        "--mount",
        f"type=bind,source={root},target=/xfstests",
        "--mount",
        f"type=bind,source={work_dir},target=/work",
        "-w",
        "/xfstests",
        image,
        "sh",
        "-lc",
        docker_run_script(),
        "sh",
        f"/work/{test_img.name}",
        f"/work/{scratch_img.name}",
    ]
    command.extend(check_args)
    return subprocess.run(command, check=False).returncode


def ensure_image(path: Path) -> None:
    if path.exists():
        return
    with path.open("wb") as image:
        image.truncate(IMAGE_SIZE_MIB * 1024 * 1024)


def docker_run_script() -> str:
    return r"""
set -eu
test_img=$1
scratch_img=$2
shift 2
mkdir -p /work/mnt/test /work/mnt/scratch /work/results
testdev=$(losetup --find --show "$test_img")
scratchdev=$(losetup --find --show "$scratch_img")
cleanup() {
    umount /work/mnt/test >/dev/null 2>&1 || true
    umount /work/mnt/scratch >/dev/null 2>&1 || true
    losetup -d "$testdev" >/dev/null 2>&1 || true
    losetup -d "$scratchdev" >/dev/null 2>&1 || true
}
trap cleanup EXIT
cat > /xfstests/local.config <<EOF
export FSTYP=ext4
export TEST_DEV=$testdev
export TEST_DIR=/work/mnt/test
export SCRATCH_DEV=$scratchdev
export SCRATCH_MNT=/work/mnt/scratch
export RESULT_BASE=/work/results
export RECREATE_TEST_DEV=true
export MKFS_OPTIONS="-F -b 4096"
export FSCK_OPTIONS="-fn"
EOF
./check "$@"
"""


def last_stderr_line(stderr: str) -> str:
    lines = [line.strip() for line in stderr.splitlines() if line.strip()]
    if not lines:
        return "no stderr"
    return lines[-1]


if __name__ == "__main__":
    raise SystemExit(main())
