#!/usr/bin/env python3
"""List LTP runtest modules outside the syscalls split.

This is intentionally separate from tools/ltp-batches.py: batches are
Txv2's prefix-based split of runtest/syscalls, while runtest modules are
LTP's own files under target/sources/ltp-20240524/runtest.
"""

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import tempfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
RUNTEST_DIR = ROOT / "target" / "sources" / "ltp-20240524" / "runtest"
DEFAULT_SOURCE = ROOT / "target" / "oscomp" / "testdata" / "sdcard-rv.img"
SKIP = {"Makefile", "syscalls", "staging"}


def find_debugfs() -> str:
    for candidate in (
        "/opt/homebrew/Cellar/e2fsprogs/1.47.4/sbin/debugfs",
        "/opt/homebrew/sbin/debugfs",
        "debugfs",
    ):
        if Path(candidate).exists() or shutil.which(candidate):
            return candidate
    raise SystemExit("debugfs not found; install e2fsprogs or populate target/sources/ltp-20240524")


def dump_runtest_file(debugfs: str, source: Path, name: str) -> str:
    with tempfile.NamedTemporaryFile(prefix=f"ltp-runtest-{name}-", delete=False) as tmp:
        tmp_path = Path(tmp.name)
    try:
        result = subprocess.run(
            [debugfs, "-n", "-R", f"dump /musl/ltp/runtest/{name} {tmp_path}", str(source)],
            capture_output=True,
            text=True,
        )
        if result.returncode != 0 or not tmp_path.exists():
            raise SystemExit(
                f"cannot dump runtest module {name!r} from {source}: {result.stderr.strip()}"
            )
        return tmp_path.read_text(errors="ignore")
    finally:
        tmp_path.unlink(missing_ok=True)


def list_image_modules(debugfs: str, source: Path) -> list[str]:
    result = subprocess.run(
        [debugfs, "-n", "-R", "ls -l /musl/ltp/runtest", str(source)],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise SystemExit(
            f"cannot list /musl/ltp/runtest in {source}: {result.stderr.strip()}"
        )
    modules = []
    for line in result.stdout.splitlines():
        parts = line.split()
        if len(parts) < 9:
            continue
        name = parts[-1]
        if name not in {".", ".."} and name not in SKIP:
            modules.append(name)
    return sorted(modules)


def entries_from_text(text: str) -> list[tuple[str, str]]:
    entries: list[tuple[str, str]] = []
    for line in text.splitlines():
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        parts = stripped.split(maxsplit=1)
        tag = parts[0]
        command = parts[1] if len(parts) > 1 else tag
        entries.append((tag, command))
    return entries


def entries_for(path: Path) -> list[tuple[str, str]]:
    return entries_from_text(path.read_text(errors="ignore"))


def module_files() -> list[Path]:
    if not RUNTEST_DIR.exists():
        raise SystemExit(f"missing runtest dir: {RUNTEST_DIR}")
    return [
        path
        for path in sorted(RUNTEST_DIR.iterdir())
        if path.is_file() and path.name not in SKIP
    ]


def source_path(raw: str | None) -> Path:
    path = Path(raw) if raw else DEFAULT_SOURCE
    if not path.is_absolute():
        path = ROOT / path
    return path


def main() -> int:
    parser = argparse.ArgumentParser(description="List non-syscalls LTP runtest modules")
    parser.add_argument("--list", action="store_true", help="list module names and counts")
    parser.add_argument("--module", help="print entries for one runtest module")
    parser.add_argument("--csv", action="store_true", help="print comma-separated entry tags")
    parser.add_argument(
        "--source",
        default=os.environ.get("LTP_SDCARD"),
        help=(
            "fallback sdcard image to inspect when target/sources/ltp-20240524 "
            "is absent"
        ),
    )
    args = parser.parse_args()

    use_tree = RUNTEST_DIR.exists()
    image = source_path(args.source)
    debugfs = None if use_tree else find_debugfs()

    if args.list:
        if use_tree:
            for path in module_files():
                print(f"{path.name:34} {len(entries_for(path)):4d}")
        else:
            if not image.is_file():
                raise SystemExit(
                    f"missing runtest dir: {RUNTEST_DIR}; fallback image not found: {image}"
                )
            assert debugfs is not None
            for name in list_image_modules(debugfs, image):
                print(f"{name:34} {len(entries_from_text(dump_runtest_file(debugfs, image, name))):4d}")
        return 0

    if args.module:
        if args.module in SKIP:
            raise SystemExit(f"runtest module {args.module!r} is intentionally skipped")
        if use_tree:
            path = RUNTEST_DIR / args.module
            if not path.is_file():
                known = ", ".join(path.name for path in module_files())
                raise SystemExit(f"unknown runtest module {args.module!r}; known: {known}")
            entries = entries_for(path)
        else:
            if not image.is_file():
                raise SystemExit(
                    f"missing runtest dir: {RUNTEST_DIR}; fallback image not found: {image}"
                )
            assert debugfs is not None
            known = list_image_modules(debugfs, image)
            if args.module not in known:
                raise SystemExit(f"unknown runtest module {args.module!r}; known: {', '.join(known)}")
            entries = entries_from_text(dump_runtest_file(debugfs, image, args.module))
        if args.csv:
            print(",".join(tag for tag, _ in entries))
        else:
            for tag, command in entries:
                print(f"{tag}\t{command}")
        return 0

    parser.print_help()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
