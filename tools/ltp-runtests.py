#!/usr/bin/env python3
"""List LTP runtest modules outside the syscalls split.

This is intentionally separate from tools/ltp-batches.py: batches are
Txv2's prefix-based split of runtest/syscalls, while runtest modules are
LTP's own files under target/sources/ltp-20240524/runtest.
"""

from __future__ import annotations

import argparse
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
RUNTEST_DIR = ROOT / "target" / "sources" / "ltp-20240524" / "runtest"
SKIP = {"Makefile", "syscalls", "staging"}


def entries_for(path: Path) -> list[tuple[str, str]]:
    entries: list[tuple[str, str]] = []
    for line in path.read_text(errors="ignore").splitlines():
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        parts = stripped.split(maxsplit=1)
        tag = parts[0]
        command = parts[1] if len(parts) > 1 else tag
        entries.append((tag, command))
    return entries


def module_files() -> list[Path]:
    if not RUNTEST_DIR.exists():
        raise SystemExit(f"missing runtest dir: {RUNTEST_DIR}")
    return [
        path
        for path in sorted(RUNTEST_DIR.iterdir())
        if path.is_file() and path.name not in SKIP
    ]


def main() -> int:
    parser = argparse.ArgumentParser(description="List non-syscalls LTP runtest modules")
    parser.add_argument("--list", action="store_true", help="list module names and counts")
    parser.add_argument("--module", help="print entries for one runtest module")
    parser.add_argument("--csv", action="store_true", help="print comma-separated entry tags")
    args = parser.parse_args()

    if args.list:
        for path in module_files():
            print(f"{path.name:34} {len(entries_for(path)):4d}")
        return 0

    if args.module:
        path = RUNTEST_DIR / args.module
        if not path.is_file() or path.name in SKIP:
            known = ", ".join(path.name for path in module_files())
            raise SystemExit(f"unknown runtest module {args.module!r}; known: {known}")
        entries = entries_for(path)
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
