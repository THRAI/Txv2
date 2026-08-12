import hashlib
import importlib.util
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "tools" / "ext4" / "fault_semantic_oracle.py"


def load_module():
    spec = importlib.util.spec_from_file_location("fault_semantic_oracle", SCRIPT)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


def write_fake_debugfs(path: Path) -> None:
    path.write_text(
        """#!/usr/bin/env python3
import sys

command = sys.argv[sys.argv.index("-R") + 1]
if command == "stat /missing":
    print("/missing: File not found", file=sys.stderr)
    raise SystemExit(1)
if command == "stat /present":
    print("Inode: 12   Type: regular    Size: 12")
    raise SystemExit(0)
if command == "cat /present":
    sys.stdout.buffer.write(b"payload-data")
    raise SystemExit(0)
print(f"unsupported command: {command}", file=sys.stderr)
raise SystemExit(2)
""",
        encoding="utf-8",
    )
    path.chmod(0o755)


class Ext4FaultSemanticOracleTests(unittest.TestCase):
    def test_debugfs_command_uses_explicit_argv_override(self):
        module = load_module()
        with mock.patch.dict(
            os.environ,
            {"TX_EXT4_FAULT_DEBUGFS_COMMAND": "debugfs -n -c"},
            clear=True,
        ):
            self.assertEqual(module.debugfs_command(), ["debugfs", "-n", "-c"])

    def test_debugfs_command_rejects_empty_override(self):
        module = load_module()
        with mock.patch.dict(os.environ, {"TX_EXT4_FAULT_DEBUGFS_COMMAND": ""}, clear=True):
            with self.assertRaisesRegex(module.SemanticOracleError, "empty TX_EXT4_FAULT_DEBUGFS_COMMAND"):
                module.debugfs_command()

    def test_resolve_debugfs_path_prefers_path_lookup(self):
        module = load_module()
        with mock.patch.object(module.shutil, "which", return_value="/tmp/path-debugfs"):
            self.assertEqual(module.resolve_debugfs_path(), Path("/tmp/path-debugfs"))

    def test_resolve_debugfs_path_uses_executable_homebrew_fallback(self):
        module = load_module()
        with tempfile.TemporaryDirectory() as tmp:
            homebrew_debugfs = Path(tmp) / "debugfs"
            homebrew_debugfs.write_text("#!/bin/sh\n", encoding="utf-8")
            homebrew_debugfs.chmod(0o755)
            with mock.patch.object(module.shutil, "which", return_value=None), mock.patch.object(
                module, "HOMEBREW_DEBUGFS", homebrew_debugfs
            ):
                self.assertEqual(module.resolve_debugfs_path(), homebrew_debugfs)

    def test_debugfs_command_rejects_missing_or_non_executable_default(self):
        module = load_module()
        with tempfile.TemporaryDirectory() as tmp:
            homebrew_debugfs = Path(tmp) / "debugfs"
            homebrew_debugfs.write_text("#!/bin/sh\n", encoding="utf-8")
            homebrew_debugfs.chmod(0o644)
            with mock.patch.dict(os.environ, {}, clear=True), mock.patch.object(
                module.shutil, "which", return_value=None
            ), mock.patch.object(module, "HOMEBREW_DEBUGFS", homebrew_debugfs):
                with self.assertRaisesRegex(module.SemanticOracleError, "no usable debugfs command"):
                    module.debugfs_command()

    def test_debugfs_runner_accepts_matching_present_absent_size_and_hash(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            image = root / "semantic.img"
            log = root / "semantic.log"
            image.write_bytes(b"not-a-real-ext4-image")
            debugfs = root / "fake-debugfs.py"
            write_fake_debugfs(debugfs)
            request = root / "request.json"
            request.write_text(
                json.dumps(
                    {
                        "schema": "tx.ext4.fault_semantic_oracle_request.v1",
                        "id": "debugfs-file-hash-namespace",
                        "image": str(image),
                        "log": str(log),
                        "expected": {
                            "present": {
                                "/present": {
                                    "size": 12,
                                    "sha256": hashlib.sha256(b"payload-data").hexdigest(),
                                }
                            },
                            "absent": ["/missing"],
                        },
                    }
                ),
                encoding="utf-8",
            )

            result = subprocess.run(
                ["python3", str(SCRIPT), str(request)],
                cwd=ROOT,
                env={**os.environ, "TX_EXT4_FAULT_DEBUGFS_COMMAND": str(debugfs)},
                check=False,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )

            log_text = log.read_text(encoding="utf-8")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("semantic oracle passed", result.stdout)
        self.assertIn("stat /present exit=0", log_text)
        self.assertIn("cat /present exit=0", log_text)
        self.assertIn("stat /missing exit=1", log_text)

    def test_debugfs_runner_fails_closed_on_size_mismatch(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            image = root / "semantic.img"
            log = root / "semantic.log"
            image.write_bytes(b"not-a-real-ext4-image")
            debugfs = root / "fake-debugfs.py"
            write_fake_debugfs(debugfs)
            request = root / "request.json"
            request.write_text(
                json.dumps(
                    {
                        "schema": "tx.ext4.fault_semantic_oracle_request.v1",
                        "id": "debugfs-file-hash-namespace",
                        "image": str(image),
                        "log": str(log),
                        "expected": {"present": {"/present": {"size": 13}}},
                    }
                ),
                encoding="utf-8",
            )

            result = subprocess.run(
                ["python3", str(SCRIPT), str(request)],
                cwd=ROOT,
                env={**os.environ, "TX_EXT4_FAULT_DEBUGFS_COMMAND": str(debugfs)},
                check=False,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )

            log_text = log.read_text(encoding="utf-8")

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("size mismatch", result.stderr)
        self.assertIn("stat /present exit=0", log_text)
