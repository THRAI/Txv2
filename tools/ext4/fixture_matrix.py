#!/usr/bin/env python3
"""Generate reproducible host ext4 fixtures for the migration oracle.

Only the host-e2fsprogs lane is generated here. Linux-mount and qemu-crash
fixtures remain catalog entries and must be produced by their owning runners.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import tempfile
from pathlib import Path
from typing import NamedTuple


REQUIRED_TOOLS = ("debugfs", "dumpe2fs", "e2fsck", "mke2fs")
FIXED_TIME = "1735689600"
HOST_LANE = "host-e2fsprogs"


class FixtureSpec(NamedTuple):
    name: str
    lane: str
    capabilities: tuple[str, ...]
    oracle_paths: tuple[str, ...]
    feature_set: str = "extent,^64bit,^metadata_csum"
    size_mib: int = 32


FIXTURES = {
    spec.name: spec
    for spec in (
        FixtureSpec("clean-linear", HOST_LANE, ("linear-directory", "inline-extent", "regular-file"), ("/fixture/hello.txt",)),
        FixtureSpec("metadata-csum-64bit", HOST_LANE, ("64bit", "metadata-csum", "regular-file"), ("/fixture/checksummed.bin",), "extent,64bit,metadata_csum"),
        FixtureSpec("htree-large-dir", HOST_LANE, ("htree-directory", "directory-tail-csum", "10000-entries"), ("/indexed", "/indexed/entry-00000", "/indexed/entry-09999"), "extent,dir_index,64bit,metadata_csum", 96),
        FixtureSpec("sparse-large-file", HOST_LANE, ("sparse-file", "greater-than-4gib", "hole-read"), ("/fixture/sparse.bin",), "extent,64bit,metadata_csum", 64),
        FixtureSpec("fragmented-depth2-extents", "linux-mount", ("fragmented-file", "depth-2-extent", "multi-group"), ("/fixture/fragmented.bin",), "extent,64bit,metadata_csum", 512),
        FixtureSpec("links-and-symlinks", HOST_LANE, ("hard-link", "fast-symlink", "block-symlink"), ("/fixture/source", "/fixture/hard", "/fixture/fast", "/fixture/long")),
        FixtureSpec("dirty-journal", "qemu-crash", ("dirty-journal", "committed-not-checkpointed", "replay"), ("/fixture/committed",), "extent,64bit,metadata_csum,has_journal", 64),
        FixtureSpec("unlinked-open-orphan", "qemu-crash", ("unlinked-open", "orphan-recovery", "inode-reclaim"), ("/fixture",), "extent,64bit,metadata_csum,has_journal", 64),
    )
}


def find_tool(name: str) -> Path | None:
    found = shutil.which(name)
    if found:
        return Path(found).resolve()
    for prefix in (Path("/opt/homebrew/opt/e2fsprogs/sbin"), Path("/opt/homebrew/sbin"), Path("/usr/local/opt/e2fsprogs/sbin"), Path("/usr/local/sbin")):
        candidate = prefix / name
        if candidate.is_file():
            return candidate
    return None


def require_tools() -> dict[str, Path]:
    resolved = {name: find_tool(name) for name in REQUIRED_TOOLS}
    missing = sorted(name for name, path in resolved.items() if path is None)
    if missing:
        raise RuntimeError("missing required e2fsprogs tools: " + ", ".join(missing))
    return {name: path for name, path in resolved.items() if path is not None}


def tool_versions(tools: dict[str, Path]) -> dict[str, str]:
    versions = {}
    for name, path in tools.items():
        proc = subprocess.run([str(path), "-V"], text=True, capture_output=True, check=False)
        text = "\n".join(part.strip() for part in (proc.stdout, proc.stderr) if part.strip())
        versions[name] = text.splitlines()[0] if text else f"exit {proc.returncode}"
    return versions


def file_sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def fixture_manifest(spec: FixtureSpec, image: Path, tools: dict[str, Path], versions: dict[str, str], *, fsck_output: str = "", observations: dict[str, str] | None = None) -> dict[str, object]:
    return {
        "schema": "tx.ext4.fixture.v1",
        "fixture": spec.name,
        "lane": spec.lane,
        "capabilities": list(spec.capabilities),
        "oracle_paths": list(spec.oracle_paths),
        "feature_set": spec.feature_set,
        "image": {"path": image.name, "bytes": image.stat().st_size, "sha256": file_sha256(image)},
        "tools": {name: {"path": str(tools[name]), "version": versions[name]} for name in sorted(tools)},
        "fsck": fsck_output,
        "observations": observations or {},
    }


def run(argv: list[str], *, env: dict[str, str] | None = None, input_text: str | None = None) -> subprocess.CompletedProcess[str]:
    proc = subprocess.run(argv, env=env, input=input_text, text=True, capture_output=True, check=False)
    if proc.returncode != 0:
        raise RuntimeError(f"command failed ({proc.returncode}): {' '.join(argv)}\nstdout:\n{proc.stdout}\nstderr:\n{proc.stderr}")
    return proc


def base_env() -> dict[str, str]:
    env = dict(os.environ)
    env["E2FSPROGS_FAKE_TIME"] = FIXED_TIME
    env["SOURCE_DATE_EPOCH"] = FIXED_TIME
    return env


def fixture_uuid(name: str) -> str:
    raw = hashlib.sha256(("tx-ext4-fixture:" + name).encode()).hexdigest()[:32]
    return f"{raw[:8]}-{raw[8:12]}-4{raw[13:16]}-a{raw[17:20]}-{raw[20:32]}"


def populate_commands(spec: FixtureSpec, workspace: Path) -> list[str]:
    payload = workspace / "payload.bin"
    payload.write_bytes((f"tx-ext4 fixture {spec.name}\n").encode())
    empty = workspace / "empty"
    empty.write_bytes(b"")
    commands = ["mkdir /fixture"]
    if spec.name in ("clean-linear", "metadata-csum-64bit"):
        commands.append(f"write {payload} /fixture/{'hello.txt' if spec.name == 'clean-linear' else 'checksummed.bin'}")
    elif spec.name == "htree-large-dir":
        commands = ["mkdir /indexed"]
        commands.extend(f"write {empty} /indexed/entry-{index:05d}" for index in range(10_000))
    elif spec.name == "sparse-large-file":
        sparse = workspace / "sparse.bin"
        with sparse.open("wb") as stream:
            stream.seek(5 * 1024**3)
            stream.write(b"tx-ext4 sparse tail\n")
        commands.append(f"write -s {sparse} /fixture/sparse.bin")
    elif spec.name == "links-and-symlinks":
        commands.extend([f"write {payload} /fixture/source", "ln /fixture/source /fixture/hard", "sif /fixture/source links_count 2", "symlink /fixture/fast source", f"symlink /fixture/long {'segment/' * 20}target"])
    else:
        raise ValueError(f"fixture {spec.name} requires lane {spec.lane}")
    return commands


def generate_host_fixture(spec: FixtureSpec, output_dir: Path) -> Path:
    if spec.lane != HOST_LANE:
        raise ValueError(f"fixture {spec.name} requires lane {spec.lane}")
    tools = require_tools()
    versions = tool_versions(tools)
    output_dir.mkdir(parents=True, exist_ok=True)
    image = output_dir / f"{spec.name}.ext4"
    manifest_path = output_dir / f"{spec.name}.json"
    with tempfile.TemporaryDirectory(prefix=f"tx-ext4-{spec.name}-") as tmp:
        workspace = Path(tmp)
        with image.open("wb") as stream:
            stream.truncate(spec.size_mib * 1024 * 1024)
        env = base_env()
        run([str(tools["mke2fs"]), "-q", "-F", "-t", "ext4", "-b", "4096", "-I", "256", "-U", fixture_uuid(spec.name), "-O", spec.feature_set, "-E", f"lazy_itable_init=0,lazy_journal_init=0,hash_seed={fixture_uuid(spec.name + '-dirhash')}", str(image)], env=env)
        run([str(tools["debugfs"]), "-w", "-f", "-", str(image)], env=env, input_text="\n".join(populate_commands(spec, workspace)) + "\n")
        if spec.name == "htree-large-dir":
            run([str(tools["e2fsck"]), "-fyD", str(image)], env=env)
        fsck = run([str(tools["e2fsck"]), "-fn", str(image)], env=env)
        observations = {}
        for path in spec.oracle_paths:
            observations[path] = run([str(tools["debugfs"]), "-R", f"stat {path}", str(image)], env=env).stdout.strip()
        if spec.name == "htree-large-dir":
            observations["htree-dump:/indexed"] = run([str(tools["debugfs"]), "-R", "htree_dump /indexed", str(image)], env=env).stdout.strip()
        observations["dumpe2fs-header"] = run([str(tools["dumpe2fs"]), "-h", str(image)], env=env).stdout.strip()
    manifest_path.write_text(json.dumps(fixture_manifest(spec, image, tools, versions, fsck_output=(fsck.stdout + fsck.stderr).strip(), observations=observations), indent=2, sort_keys=True) + "\n")
    return manifest_path


def list_catalog() -> None:
    for spec in FIXTURES.values():
        print(f"{spec.name}\t{spec.lane}\t{','.join(spec.capabilities)}")


def main() -> int:
    parser = argparse.ArgumentParser()
    subparsers = parser.add_subparsers(dest="command", required=True)
    subparsers.add_parser("list")
    generate = subparsers.add_parser("generate")
    generate.add_argument("fixture", choices=sorted(FIXTURES))
    generate.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.command == "list":
        list_catalog()
        return 0
    spec = FIXTURES[args.fixture]
    if spec.lane != HOST_LANE:
        parser.error(f"{spec.name} must be generated by the {spec.lane} lane")
    print(generate_host_fixture(spec, args.output))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
