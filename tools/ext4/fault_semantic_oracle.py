#!/usr/bin/env python3
"""Validate one ext4 fault semantic oracle against a copied replay image.

The request is a ``tx.ext4.fault_semantic_oracle_request.v1`` JSON object with
``id``, ``image``, ``log``, and ``expected`` fields. The only supported oracle
uses debugfs-compatible ``stat`` and ``cat`` output to verify namespace
presence/absence, file sizes, and content hashes.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
from pathlib import Path
from typing import Any


class SemanticOracleError(Exception):
    pass


HOMEBREW_DEBUGFS = Path("/opt/homebrew/opt/e2fsprogs/sbin/debugfs")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("request", type=Path)
    args = parser.parse_args(argv)
    lines: list[str] = []
    log: Path | None = None
    try:
        request = load_request(args.request)
        log = Path(require_string(request, "log"))
        verify_request(request, lines)
    except SemanticOracleError as err:
        lines.append(f"error: {err}")
        if log is not None:
            write_log(log, lines)
        print(f"error: {err}", file=sys.stderr)
        return 1

    write_log(log, lines)
    print(f"semantic oracle passed: {args.request}")
    return 0


def load_request(path: Path) -> dict[str, Any]:
    if not path.is_file():
        raise SemanticOracleError(f"missing semantic oracle request: {path}")
    try:
        request = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as err:
        raise SemanticOracleError(f"invalid semantic oracle request JSON: {err}") from err
    if not isinstance(request, dict):
        raise SemanticOracleError("semantic oracle request is not an object")
    if request.get("schema") != "tx.ext4.fault_semantic_oracle_request.v1":
        raise SemanticOracleError("semantic oracle request schema mismatch")
    return request


def verify_request(request: dict[str, Any], lines: list[str]) -> None:
    if require_string(request, "id") != "debugfs-file-hash-namespace":
        raise SemanticOracleError(f"unsupported semantic oracle id: {request.get('id')!r}")
    image = Path(require_string(request, "image"))
    require_string(request, "log")
    if not image.is_file():
        raise SemanticOracleError(f"missing semantic oracle image: {image}")
    expected = validate_expected(request.get("expected"))
    command = debugfs_command()
    if not command_available(command):
        raise SemanticOracleError(f"debugfs command is not executable: {command[0]}")
    lines.append("debugfs_command=" + shlex.join(command))
    for path, checks in expected["present"].items():
        stat = run_debugfs(command, image, f"stat {path}", lines, f"stat {path}")
        if stat.returncode != 0:
            raise SemanticOracleError(f"present path stat failed: {path}")
        if "size" in checks:
            actual_size = parse_debugfs_size(stat.stdout)
            if actual_size != checks["size"]:
                raise SemanticOracleError(
                    f"size mismatch for {path}: expected={checks['size']} actual={actual_size}"
                )
        if "sha256" in checks:
            content = run_debugfs(command, image, f"cat {path}", lines, f"cat {path}")
            if content.returncode != 0:
                raise SemanticOracleError(f"present path content read failed: {path}")
            actual_sha256 = hashlib.sha256(content.stdout).hexdigest()
            lines.append(f"cat {path} sha256={actual_sha256}")
            if actual_sha256 != checks["sha256"]:
                raise SemanticOracleError(
                    f"sha256 mismatch for {path}: expected={checks['sha256']} actual={actual_sha256}"
                )
    for path in expected["absent"]:
        stat = run_debugfs(command, image, f"stat {path}", lines, f"stat {path}")
        if stat.returncode == 0 or not reports_missing_path(stat):
            raise SemanticOracleError(f"absent path is present or indeterminate: {path}")


def validate_expected(value: Any) -> dict[str, Any]:
    if not isinstance(value, dict) or not value:
        raise SemanticOracleError("semantic oracle expected must be a non-empty object")
    if set(value) - {"present", "absent"}:
        raise SemanticOracleError("semantic oracle expected has unsupported fields")
    present = value.get("present", {})
    absent = value.get("absent", [])
    if not isinstance(present, dict) or not isinstance(absent, list):
        raise SemanticOracleError("semantic oracle expected present/absent types are invalid")
    if not present and not absent:
        raise SemanticOracleError("semantic oracle expected has no paths")
    checked_present: dict[str, dict[str, Any]] = {}
    for path, checks in present.items():
        validate_path(path)
        if not isinstance(checks, dict) or set(checks) - {"size", "sha256"}:
            raise SemanticOracleError(f"semantic oracle present checks are invalid for {path}")
        if "size" in checks and (not isinstance(checks["size"], int) or checks["size"] < 0):
            raise SemanticOracleError(f"semantic oracle size is invalid for {path}")
        if "sha256" in checks and not is_sha256(checks["sha256"]):
            raise SemanticOracleError(f"semantic oracle sha256 is invalid for {path}")
        checked_present[path] = checks
    checked_absent: list[str] = []
    for path in absent:
        validate_path(path)
        if path in checked_absent:
            raise SemanticOracleError(f"semantic oracle absent path is duplicated: {path}")
        checked_absent.append(path)
    if set(checked_present).intersection(checked_absent):
        raise SemanticOracleError("semantic oracle path is both present and absent")
    return {"present": checked_present, "absent": checked_absent}


def validate_path(path: Any) -> None:
    if not isinstance(path, str) or not path.startswith("/") or any(ch.isspace() for ch in path):
        raise SemanticOracleError(f"semantic oracle path is invalid: {path!r}")


def is_sha256(value: Any) -> bool:
    return isinstance(value, str) and len(value) == 64 and all(ch in "0123456789abcdef" for ch in value)


def debugfs_command() -> list[str]:
    override = os.environ.get("TX_EXT4_FAULT_DEBUGFS_COMMAND")
    if override is not None:
        command = shlex.split(override)
        if not command:
            raise SemanticOracleError("empty TX_EXT4_FAULT_DEBUGFS_COMMAND")
        return command
    path = resolve_debugfs_path()
    if path is None:
        raise SemanticOracleError(
            "no usable debugfs command on PATH or Homebrew e2fsprogs prefix"
        )
    return [str(path)]


def resolve_debugfs_path() -> Path | None:
    found = shutil.which("debugfs")
    if found:
        return Path(found)
    if HOMEBREW_DEBUGFS.is_file() and os.access(HOMEBREW_DEBUGFS, os.X_OK):
        return HOMEBREW_DEBUGFS
    return None


def command_available(command: list[str]) -> bool:
    executable = command[0]
    if "/" in executable:
        path = Path(executable)
        return path.is_file() and os.access(path, os.X_OK)
    return shutil.which(executable) is not None


def run_debugfs(
    command: list[str], image: Path, request: str, lines: list[str], label: str
) -> subprocess.CompletedProcess[bytes]:
    try:
        completed = subprocess.run(
            command + ["-R", request, str(image)],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
    except OSError as err:
        raise SemanticOracleError(f"debugfs launch failed: {err}") from err
    lines.append(f"{label} exit={completed.returncode}")
    lines.append(f"{label} stdout_sha256={hashlib.sha256(completed.stdout).hexdigest()}")
    lines.append(f"{label} stderr_sha256={hashlib.sha256(completed.stderr).hexdigest()}")
    return completed


def parse_debugfs_size(stdout: bytes) -> int:
    match = re.search(r"\bSize:\s*(\d+)\b", stdout.decode("utf-8", errors="replace"))
    if match is None:
        raise SemanticOracleError("debugfs stat output is missing Size")
    return int(match.group(1), 10)


def reports_missing_path(completed: subprocess.CompletedProcess[bytes]) -> bool:
    text = (completed.stdout + completed.stderr).decode("utf-8", errors="replace").lower()
    return "file not found" in text or "not found by" in text


def require_string(parent: dict[str, Any], field: str) -> str:
    value = parent.get(field)
    if not isinstance(value, str) or not value:
        raise SemanticOracleError(f"semantic oracle request missing {field}")
    return value


def write_log(path: Path, lines: list[str]) -> None:
    if path.exists():
        raise SemanticOracleError(f"refusing to overwrite semantic oracle log: {path}")
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")


if __name__ == "__main__":
    raise SystemExit(main())
