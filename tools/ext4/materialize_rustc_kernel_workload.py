#!/usr/bin/env python3
"""Materialize immutable TEST/SCRATCH/WORKLOAD images for the M1 rustc witness."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
from typing import Any

BLOCK_BYTES = 4096
TIER1_MKFS_FEATURES = "^orphan_file"
TIER1_JOURNAL_SIZE_MIB = 4
MIN_IMAGE_BYTES = 64 * 1024 * 1024
HEADROOM_BYTES = 512 * 1024 * 1024
TARGET_STDLIB = Path("lib/rustlib/riscv64gc-unknown-none-elf")
ROLES = ("test_seed", "scratch_seed", "workload")
ROOT = Path(__file__).parents[2]


class MaterializationError(Exception):
    pass


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def sha256_tree(root: Path) -> str:
    digest = hashlib.sha256()
    for path in sorted(root.rglob("*"), key=lambda item: item.as_posix()):
        relative = path.relative_to(root).as_posix().encode()
        if path.is_symlink():
            digest.update(b"symlink\0" + relative + b"\0" + os.readlink(path).encode() + b"\0")
        elif path.is_file():
            digest.update(b"file\0" + relative + b"\0" + sha256_file(path).encode() + b"\0")
        elif not path.is_dir():
            raise MaterializationError(f"unsupported tree entry: {path}")
    return digest.hexdigest()


def require_file(path: Path, label: str) -> None:
    if not path.is_file():
        raise MaterializationError(f"missing {label}: {path}")


def require_hex(value: str, length: int, label: str) -> None:
    if len(value) != length or value == "0" * length or any(ch not in "0123456789abcdef" for ch in value):
        raise MaterializationError(f"{label} must be a nonzero lowercase {length}-hex value")


def require_toolchain(root: Path) -> None:
    for path, label in ((root / "bin/rustc", "toolchain rustc"), (root / "bin/cargo", "toolchain cargo")):
        require_file(path, label)
    if not (root / TARGET_STDLIB).is_dir():
        raise MaterializationError("missing native RV64 riscv64gc-unknown-none-elf stdlib")


def source_head(source: Path) -> str:
    if not source.is_dir():
        raise MaterializationError(f"missing source tree: {source}")
    result = subprocess.run(["git", "status", "--porcelain"], cwd=source, capture_output=True, text=True, check=False)
    if result.returncode or result.stdout.strip():
        raise MaterializationError("source tree must be clean before materialization")
    return subprocess.run(["git", "rev-parse", "HEAD"], cwd=source, capture_output=True, text=True, check=True).stdout.strip()


def image_bytes(stage: Path) -> int:
    payload = sum(path.stat().st_size for path in stage.rglob("*") if path.is_file())
    return max(MIN_IMAGE_BYTES, ((payload + payload // 4 + HEADROOM_BYTES + BLOCK_BYTES - 1) // BLOCK_BYTES) * BLOCK_BYTES)


def find_mkfs() -> str:
    for candidate in (shutil.which("mkfs.ext4"), "/opt/homebrew/opt/e2fsprogs/sbin/mkfs.ext4", "/opt/homebrew/sbin/mkfs.ext4"):
        if candidate and Path(candidate).is_file() and os.access(candidate, os.X_OK):
            return candidate
    raise MaterializationError("mkfs.ext4 is required to materialize role images")


def find_e2fsck() -> str:
    for candidate in (shutil.which("e2fsck"), "/opt/homebrew/opt/e2fsprogs/sbin/e2fsck", "/opt/homebrew/sbin/e2fsck"):
        if candidate and Path(candidate).is_file() and os.access(candidate, os.X_OK):
            return candidate
    raise MaterializationError("e2fsck is required to settle materialized role images")


def make_ext4(mkfs: str, e2fsck: str, stage: Path, image: Path, label: str) -> None:
    image.parent.mkdir(parents=True, exist_ok=True)
    result = subprocess.run([mkfs, "-q", "-F", "-b", str(BLOCK_BYTES), "-O", TIER1_MKFS_FEATURES, "-J", f"size={TIER1_JOURNAL_SIZE_MIB}", "-L", label, "-d", str(stage), str(image), str(image_bytes(stage) // BLOCK_BYTES)], capture_output=True, text=True, check=False)
    if result.returncode:
        raise MaterializationError(f"mkfs.ext4 failed for {label}: {result.stderr.strip() or result.stdout.strip()}")
    result = subprocess.run([e2fsck, "-fy", str(image)], capture_output=True, text=True, check=False)
    if result.returncode > 1:
        raise MaterializationError(f"e2fsck failed for {label}: {result.stderr.strip() or result.stdout.strip()}")


def copy_tree(source: Path, destination: Path) -> None:
    shutil.copytree(source, destination, symlinks=True, ignore=shutil.ignore_patterns(".git", "target", "__pycache__", ".DS_Store"))


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--template", required=True, type=Path)
    parser.add_argument("--source-tree", required=True, type=Path)
    parser.add_argument("--vendor-tree", required=True, type=Path)
    parser.add_argument("--toolchain-root", required=True, type=Path)
    parser.add_argument("--toolchain-source-commit", required=True)
    parser.add_argument("--toolchain-config", required=True, type=Path)
    parser.add_argument("--rustc-vv", required=True, type=Path)
    parser.add_argument("--linker", required=True)
    parser.add_argument("--out", required=True, type=Path)
    args = parser.parse_args(argv)
    try:
        require_file(args.template, "workload template")
        template: dict[str, Any] = json.loads(args.template.read_text(encoding="utf-8"))
        if template.get("schema") != "tx.ext4.workload.v1" or template.get("contract_status") != "fixture-non-evidence":
            raise MaterializationError("template must be the fixture-non-evidence rustc workload contract")
        require_toolchain(args.toolchain_root)
        require_file(args.source_tree / "Cargo.lock", "Cargo.lock")
        require_file(args.toolchain_config, "toolchain config")
        require_file(args.rustc_vv, "rustc -vV witness")
        require_hex(args.toolchain_source_commit, 40, "toolchain source commit")
        if args.out.exists():
            raise MaterializationError(f"refusing to overwrite materialization output: {args.out}")
        source_commit = source_head(args.source_tree)
        mkfs, e2fsck = find_mkfs(), find_e2fsck()
        stage, images = args.out / "stage", args.out / "images"
        test, scratch, workload = stage / "test", stage / "scratch", stage / "workload"
        test.mkdir(parents=True)
        copy_tree(args.source_tree, test / "source")
        copy_tree(args.vendor_tree, test / "vendor")
        shutil.copy2(ROOT / "tools/ext4/guest/run-rustc-kernel-build.sh", test / "run-rustc-kernel-build.sh")
        os.chmod(test / "run-rustc-kernel-build.sh", 0o755)
        wrapper = test / "rustc-via-musl-loader"
        wrapper.write_text('#!/bin/sh\nexec "$TX_EXT4_MUSL_LOADER" "$TX_EXT4_TOOLCHAIN_ROOT/bin/rustc" "$@"\n', encoding="utf-8")
        os.chmod(wrapper, 0o755)
        (test / "cargo-home").mkdir()
        (test / "cargo-home/config.toml").write_text('[net]\noffline = true\n\n[source.crates-io]\nreplace-with = "vendored-sources"\n\n[source.vendored-sources]\ndirectory = "/mnt/ext4-test/vendor"\n', encoding="utf-8")
        scratch.mkdir(parents=True)
        workload.mkdir(parents=True)
        copy_tree(args.toolchain_root, workload / "toolchain")
        subprocess.run([str(ROOT / "tools/images/install-riscv64-rustc-resolv-shim.sh"), str(workload / "toolchain")], check=True)
        role_images = {"test_seed": images / "test-seed.ext4", "scratch_seed": images / "scratch-seed.ext4", "workload": images / "workload.ext4"}
        for role, label in zip(ROLES, ("TXTEST", "TXSCRATCH", "TXWORKLOAD")):
            make_ext4(mkfs, e2fsck, {"test_seed": test, "scratch_seed": scratch, "workload": workload}[role], role_images[role], label)
        manifest = json.loads(json.dumps(template))
        manifest.pop("contract_status", None)
        manifest.update({"tx_source_commit": source_commit, "cargo_lock_sha256": sha256_file(args.source_tree / "Cargo.lock"), "vendor_tree_sha256": sha256_tree(args.vendor_tree), "native_rv64_toolchain": {"source_commit": args.toolchain_source_commit, "config_sha256": sha256_file(args.toolchain_config), "rustc_sha256": sha256_file(args.toolchain_root / "bin/rustc"), "cargo_sha256": sha256_file(args.toolchain_root / "bin/cargo"), "rustc -vV": args.rustc_vv.read_text(encoding="utf-8").strip(), "linker": args.linker}, "resolver_shim": {"install_scope": "workload-only", "sha256": sha256_file(workload / "toolchain/lib/libtx-rustc-resolv-preload.so")}, "role_images": {role: {"sha256": sha256_file(image), "read_only": role == "workload"} for role, image in role_images.items()}})
        args.out.mkdir(exist_ok=True)
        (args.out / "rustc-kernel-build.json").write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        print(args.out / "rustc-kernel-build.json")
        return 0
    except (MaterializationError, OSError, subprocess.SubprocessError, json.JSONDecodeError) as error:
        print(f"materialize-rustc-kernel-workload: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
