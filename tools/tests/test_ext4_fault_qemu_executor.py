import json
import os
import hashlib
import importlib.util
import shlex
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "tools" / "ext4" / "fault_qemu_executor.py"


def load_executor_module():
    spec = importlib.util.spec_from_file_location("fault_qemu_executor", SCRIPT)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


class Ext4FaultQemuExecutorTests(unittest.TestCase):
    def test_e2fsck_command_uses_explicit_argv_override(self):
        module = load_executor_module()
        with patch.dict(
            os.environ,
            {"TX_EXT4_FAULT_E2FSCK_COMMAND": "e2fsck -n -c"},
            clear=True,
        ):
            self.assertEqual(module.e2fsck_command(), ["e2fsck", "-n", "-c"])

    def test_e2fsck_command_uses_path_lookup(self):
        module = load_executor_module()
        with patch.dict(os.environ, {}, clear=True), patch.object(
            module.shutil, "which", return_value="/tmp/path-e2fsck"
        ):
            self.assertEqual(module.e2fsck_command(), ["/tmp/path-e2fsck"])

    def test_e2fsck_command_uses_homebrew_fallback_when_path_missing(self):
        module = load_executor_module()
        with tempfile.TemporaryDirectory() as tmp:
            homebrew_e2fsck = Path(tmp) / "e2fsck"
            homebrew_e2fsck.write_text("#!/bin/sh\n", encoding="utf-8")
            homebrew_e2fsck.chmod(0o755)
            with patch.dict(os.environ, {}, clear=True), patch.object(
                module.shutil, "which", return_value=None
            ), patch.object(module, "HOMEBREW_E2FSCK", homebrew_e2fsck, create=True):
                self.assertEqual(module.e2fsck_command(), [str(homebrew_e2fsck)])

    def test_e2fsck_command_rejects_empty_override(self):
        module = load_executor_module()
        with patch.dict(os.environ, {"TX_EXT4_FAULT_E2FSCK_COMMAND": ""}, clear=True):
            with self.assertRaisesRegex(module.FaultQemuExecutorError, "empty TX_EXT4_FAULT_E2FSCK_COMMAND"):
                module.e2fsck_command()

    def test_e2fsck_command_rejects_missing_or_non_executable_default(self):
        module = load_executor_module()
        with tempfile.TemporaryDirectory() as tmp:
            homebrew_e2fsck = Path(tmp) / "e2fsck"
            homebrew_e2fsck.write_text("#!/bin/sh\n", encoding="utf-8")
            homebrew_e2fsck.chmod(0o644)
            with patch.dict(os.environ, {}, clear=True), patch.object(
                module.shutil, "which", return_value=None
            ), patch.object(module, "HOMEBREW_E2FSCK", homebrew_e2fsck, create=True):
                with self.assertRaisesRegex(module.FaultQemuExecutorError, "no usable e2fsck command"):
                    module.e2fsck_command()

    def test_image_copy_uses_cow_clone_command(self):
        module = load_executor_module()
        with tempfile.TemporaryDirectory() as tmp:
            source = Path(tmp) / "source.img"
            target = Path(tmp) / "target.img"
            source.write_bytes(b"image")
            completed = subprocess.CompletedProcess(
                args=["cp"],
                returncode=0,
                stdout="",
                stderr="",
            )
            with patch.object(module.subprocess, "run", return_value=completed) as run:
                module.copy_image_cow(source, target)

        command = run.call_args.args[0]
        self.assertIn("cp", command[0])
        self.assertIn(str(source), command)
        self.assertIn(str(target), command)
        self.assertIn("--reflink=always" if sys.platform.startswith("linux") else "-c", command)

    def test_stages_role_images_and_fails_before_fake_crash_evidence(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            job_dir = root / "out" / "write_fsync" / "after-commit" / "0001"
            test_image = root / "test.img"
            scratch_image = root / "scratch.img"
            workload_image = root / "workload.img"
            script = root / "case.txt"
            for path, payload in (
                (test_image, b"test-seed"),
                (scratch_image, b"scratch-seed"),
                (workload_image, b"workload-seed"),
            ):
                path.write_bytes(payload)
            script.write_text(
                'send "echo tx-ext4-fault-cut:write_fsync:after-commit\\n"\n'
                'expect "tx-ext4-fault-cut:write_fsync:after-commit" within 10000\n'
                "quit\n",
                encoding="utf-8",
            )
            request = {
                "schema": "tx.ext4.fault_job_request.v1",
                "plan": str(root / "out" / "campaign-plan.json"),
                "campaign_plan_sha256": "a" * 64,
                "job_index": 1,
                "job": {
                    "case": "write_fsync",
                    "cut": "after-commit",
                    "iteration": 1,
                    "serial_log": str(job_dir / "serial.log"),
                    "crash_image": str(job_dir / "crash.img"),
                    "replay_image": str(job_dir / "replay.img"),
                    "checks": [
                        {
                            "tool": "e2fsck",
                            "args": ["-fn", str(job_dir / "crash.img")],
                            "log": str(job_dir / "e2fsck-fn.log"),
                        }
                    ],
                    "replay_matrix": [
                        {
                            "id": "linux-rw-replay",
                            "image": str(job_dir / "linux-replay.img"),
                            "log": str(job_dir / "linux-rw-replay.log"),
                        }
                    ],
                },
                "role_images": {
                    "test": str(test_image),
                    "scratch": str(scratch_image),
                    "workload": str(workload_image),
                },
                "qemu": {
                    "target": "rv64-qemu",
                    "profile": "alpine",
                    "script": str(script),
                    "timeout_ms": 60000,
                    "cut_marker": "tx-ext4-fault-cut:write_fsync:after-commit",
                },
            }
            job_dir.mkdir(parents=True)
            request_path = job_dir / "job-request.json"
            request_path.write_text(json.dumps(request), encoding="utf-8")

            result = subprocess.run(
                ["python3", str(SCRIPT), str(request_path)],
                cwd=ROOT,
                check=False,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                env={**os.environ, "TX_EXT4_FAULT_PLAN_ONLY": "1"},
            )

            executor_plan = json.loads((job_dir / "executor-plan.json").read_text(encoding="utf-8"))

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("refusing to write crash evidence", result.stderr)
        self.assertEqual(executor_plan["schema"], "tx.ext4.fault_qemu_executor_plan.v1")
        self.assertEqual(executor_plan["campaign_plan_sha256"], "a" * 64)
        self.assertEqual(executor_plan["staged_role_images"]["scratch"], str(job_dir / "roles" / "scratch.img"))
        self.assertEqual(
            executor_plan["shell_test_command"][:5],
            ["cargo", "xtask", "shell-test", "--target", "rv64-qemu"],
        )
        self.assertEqual(executor_plan["runner"]["status"], "prepared-not-run")
        self.assertEqual(
            executor_plan["job"]["replay_matrix"], request["job"]["replay_matrix"]
        )
        self.assertEqual(executor_plan["runner"]["serial_log"], str(job_dir / "serial.log"))
        self.assertEqual(executor_plan["runner"]["cwd"], str(ROOT))
        self.assertEqual(executor_plan["hard_kill"]["marker"], "tx-ext4-fault-cut:write_fsync:after-commit")
        self.assertIn("--stop-after-needle", executor_plan["shell_test_command"])
        self.assertEqual(
            executor_plan["shell_test_command"][
                executor_plan["shell_test_command"].index("--stop-after-needle") + 1
            ],
            "tx-ext4-fault-cut:write_fsync:after-commit",
        )
        extra_images = [
            executor_plan["shell_test_command"][idx + 1]
            for idx, arg in enumerate(executor_plan["shell_test_command"])
            if arg == "--extra-rv64-ext4"
        ]
        self.assertEqual(
            extra_images,
            [
                str(job_dir / "roles" / "test.img"),
                str(job_dir / "roles" / "scratch.img"),
                str(job_dir / "roles" / "workload.img"),
            ],
        )
        self.assertFalse((job_dir / "result.json").exists())

    def test_refuses_to_plan_beside_a_stale_result_manifest(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            job_dir = root / "out" / "write_fsync" / "after-commit" / "0001"
            job_dir.mkdir(parents=True)
            for name in ("test", "scratch", "workload"):
                (root / f"{name}.img").write_bytes(name.encode("utf-8"))
            script = root / "case.txt"
            script.write_text(
                'expect "tx-ext4-fault-cut:write_fsync:after-commit" within 10000\n'
                "quit\n",
                encoding="utf-8",
            )
            request = {
                "schema": "tx.ext4.fault_job_request.v1",
                "job": {
                    "case": "write_fsync",
                    "cut": "after-commit",
                    "iteration": 1,
                    "serial_log": str(job_dir / "serial.log"),
                },
                "role_images": {
                    name: str(root / f"{name}.img")
                    for name in ("test", "scratch", "workload")
                },
                "qemu": {
                    "target": "rv64-qemu",
                    "profile": "alpine",
                    "script": str(script),
                    "timeout_ms": 60000,
                    "cut_marker": "tx-ext4-fault-cut:write_fsync:after-commit",
                },
            }
            request_path = job_dir / "job-request.json"
            request_path.write_text(json.dumps(request), encoding="utf-8")
            (job_dir / "result.json").write_text("{}\n", encoding="utf-8")

            result = subprocess.run(
                ["python3", str(SCRIPT), str(request_path)],
                cwd=ROOT,
                check=False,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("stale result manifest", result.stderr)

    def test_rejects_script_without_cut_marker(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            job_dir = root / "out" / "write_fsync" / "after-commit" / "0001"
            job_dir.mkdir(parents=True)
            for name in ("test", "scratch", "workload"):
                (root / f"{name}.img").write_bytes(name.encode("utf-8"))
            script = root / "case.txt"
            script.write_text("quit\n", encoding="utf-8")
            request = {
                "schema": "tx.ext4.fault_job_request.v1",
                "job": {
                    "case": "write_fsync",
                    "cut": "after-commit",
                    "iteration": 1,
                    "serial_log": str(job_dir / "serial.log"),
                },
                "role_images": {
                    name: str(root / f"{name}.img")
                    for name in ("test", "scratch", "workload")
                },
                "qemu": {
                    "target": "rv64-qemu",
                    "profile": "alpine",
                    "script": str(script),
                    "timeout_ms": 60000,
                    "cut_marker": "tx-ext4-fault-cut:write_fsync:after-commit",
                },
            }
            request_path = job_dir / "job-request.json"
            request_path.write_text(json.dumps(request), encoding="utf-8")

            result = subprocess.run(
                ["python3", str(SCRIPT), str(request_path)],
                cwd=ROOT,
                check=False,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("missing cut marker", result.stderr)
        self.assertFalse((job_dir / "executor-plan.json").exists())

    def test_default_runner_terminates_at_cut_and_preserves_images_without_result(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            job_dir = root / "out" / "write_fsync" / "after-commit" / "0001"
            job_dir.mkdir(parents=True)
            for name in ("test", "scratch", "workload"):
                (root / f"{name}.img").write_bytes(f"{name}-seed".encode("utf-8"))
            marker = "tx-ext4-fault-cut:write_fsync:after-commit"
            script = root / "case.txt"
            script.write_text(f'expect "{marker}" within 10000\nquit\n', encoding="utf-8")
            request = {
                "schema": "tx.ext4.fault_job_request.v1",
                "job": {
                    "case": "write_fsync",
                    "cut": "after-commit",
                    "iteration": 1,
                    "serial_log": str(job_dir / "serial.log"),
                    "crash_image": str(job_dir / "crash.img"),
                    "replay_image": str(job_dir / "replay.img"),
                },
                "role_images": {
                    name: str(root / f"{name}.img")
                    for name in ("test", "scratch", "workload")
                },
                "qemu": {
                    "target": "rv64-qemu",
                    "profile": "alpine",
                    "script": str(script),
                    "timeout_ms": 60000,
                    "cut_marker": marker,
                },
            }
            request_path = job_dir / "job-request.json"
            request_path.write_text(json.dumps(request), encoding="utf-8")
            runner_program = (
                "import os, pathlib, time; "
                "pathlib.Path(os.environ['TX_EXT4_FAULT_SERIAL_LOG']).write_text(" + repr(marker) + "); "
                "print(" + repr(marker) + ", flush=True); time.sleep(60)"
            )
            env = os.environ.copy()
            env["TX_EXT4_FAULT_RUNNER_COMMAND"] = shlex.join([sys.executable, "-c", runner_program])

            result = subprocess.run(
                ["python3", str(SCRIPT), str(request_path)],
                cwd=ROOT,
                check=False,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                env=env,
                timeout=10,
            )
            executor_plan = json.loads((job_dir / "executor-plan.json").read_text(encoding="utf-8"))

            self.assertNotEqual(result.returncode, 0)
            self.assertIn("refusing to write crash evidence", result.stderr)
            self.assertEqual(executor_plan["runner"]["status"], "terminated-after-cut")
            self.assertEqual(executor_plan["hard_kill"]["status"], "not-observed")
            self.assertEqual(
                executor_plan["preserved_images"]["status"], "copied-after-runner-termination"
            )
            self.assertEqual((job_dir / "crash.img").read_bytes(), b"scratch-seed")
            self.assertEqual((job_dir / "replay.img").read_bytes(), b"scratch-seed")
            self.assertFalse((job_dir / "result.json").exists())

    def test_shell_test_command_waits_for_runner_exit_after_cut(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            job_dir = root / "out" / "write_fsync" / "after-commit" / "0001"
            job_dir.mkdir(parents=True)
            marker = "tx-ext4-fault-cut:write_fsync:after-commit"
            serial_log = job_dir / "serial.log"
            scratch = job_dir / "roles" / "scratch.img"
            scratch.parent.mkdir()
            scratch.write_bytes(b"scratch-after-cut")
            runner_program = (
                "import os, pathlib; "
                "pathlib.Path(os.environ['TX_EXT4_FAULT_SERIAL_LOG']).write_text(" + repr(marker) + "); "
                "print(" + repr("shell-test: stop needle observed: " + marker) + ", flush=True)"
            )
            plan = {
                "schema": "tx.ext4.fault_qemu_executor_plan.v1",
                "job": {
                    "case": "write_fsync",
                    "cut": "after-commit",
                    "iteration": 1,
                    "crash_image": str(job_dir / "crash.img"),
                    "replay_image": str(job_dir / "replay.img"),
                },
                "staged_role_images": {"scratch": str(scratch)},
                "shell_test_command": [sys.executable, "-c", runner_program],
                "runner": {
                    "cwd": str(ROOT),
                    "serial_log": str(serial_log),
                    "cut_marker": marker,
                    "status": "prepared-not-run",
                },
                "hard_kill": {
                    "required": True,
                    "status": "not-implemented",
                    "marker": marker,
                },
            }
            plan_path = job_dir / "executor-plan.json"
            plan_path.write_text(json.dumps(plan), encoding="utf-8")

            module = load_executor_module()
            module.execute_prepared_runner(plan_path)
            executor_plan = json.loads(plan_path.read_text(encoding="utf-8"))
            crash_bytes = (job_dir / "crash.img").read_bytes()
            result_exists = (job_dir / "result.json").exists()

        self.assertEqual(executor_plan["runner"]["command_source"], "shell-test-command")
        self.assertEqual(executor_plan["runner"]["status"], "exited-after-cut")
        self.assertEqual(executor_plan["runner"]["exit_code"], 0)
        self.assertEqual(executor_plan["hard_kill"]["status"], "observed-by-shell-test")
        self.assertEqual(crash_bytes, b"scratch-after-cut")
        self.assertFalse(result_exists)

    def test_shell_test_command_retries_before_cut_with_fresh_role_images(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            job_dir = root / "out" / "write_fsync" / "after-commit" / "0001"
            source_dir = root / "sources"
            roles_dir = job_dir / "roles"
            source_dir.mkdir(parents=True)
            roles_dir.mkdir(parents=True)
            for role in ("test", "scratch", "workload"):
                (source_dir / f"{role}.img").write_bytes(f"{role}-seed".encode())
                (roles_dir / f"{role}.img").write_bytes(b"stale")
            marker = "tx-ext4-fault-cut:write_fsync:after-commit"
            serial_log = job_dir / "serial.log"
            attempts_file = job_dir / "attempts.txt"
            runner_program = (
                "import pathlib, sys; "
                f"attempts = pathlib.Path({str(attempts_file)!r}); "
                "count = int(attempts.read_text()) if attempts.exists() else 0; "
                "attempts.write_text(str(count + 1)); "
                "print('first attempt failed', flush=True) if count == 0 else "
                f"(pathlib.Path({str(serial_log)!r}).write_text({marker!r}), "
                f"print('shell-test: stop needle observed: {marker}', flush=True)); "
                "sys.exit(1 if count == 0 else 0)"
            )
            plan = {
                "schema": "tx.ext4.fault_qemu_executor_plan.v1",
                "job": {
                    "case": "write_fsync",
                    "cut": "after-commit",
                    "iteration": 1,
                    "crash_image": str(job_dir / "crash.img"),
                    "replay_image": str(job_dir / "replay.img"),
                },
                "role_images": {
                    "test": str(source_dir / "test.img"),
                    "scratch": str(source_dir / "scratch.img"),
                    "workload": str(source_dir / "workload.img"),
                },
                "staged_role_images": {
                    "test": str(roles_dir / "test.img"),
                    "scratch": str(roles_dir / "scratch.img"),
                    "workload": str(roles_dir / "workload.img"),
                },
                "shell_test_command": [sys.executable, "-c", runner_program],
                "runner": {
                    "cwd": str(ROOT),
                    "serial_log": str(serial_log),
                    "cut_marker": marker,
                    "status": "prepared-not-run",
                },
                "hard_kill": {
                    "required": True,
                    "status": "not-implemented",
                    "marker": marker,
                },
            }
            plan_path = job_dir / "executor-plan.json"
            plan_path.write_text(json.dumps(plan), encoding="utf-8")

            module = load_executor_module()
            module.execute_prepared_runner(plan_path)
            executor_plan = json.loads(plan_path.read_text(encoding="utf-8"))

        self.assertEqual(executor_plan["runner"]["status"], "exited-after-cut")
        self.assertEqual([attempt["status"] for attempt in executor_plan["runner"]["attempts"]], [
            "shell-test-failed-before-cut",
            "exited-after-cut",
        ])
        self.assertEqual(executor_plan["hard_kill"]["status"], "observed-by-shell-test")
        self.assertEqual(executor_plan["preserved_images"]["status"], "copied-after-runner-termination")
        self.assertEqual(executor_plan["staged_role_images_retention"]["status"], "removed-after-digest-recorded")
        self.assertFalse((roles_dir / "scratch.img").exists())

    def test_shell_test_command_requires_stop_needle_confirmation(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            job_dir = root / "out" / "write_fsync" / "after-commit" / "0001"
            job_dir.mkdir(parents=True)
            marker = "tx-ext4-fault-cut:write_fsync:after-commit"
            scratch = job_dir / "roles" / "scratch.img"
            scratch.parent.mkdir()
            scratch.write_bytes(b"scratch-after-cut")
            runner_program = (
                "print(" + repr('  [2] expect "' + marker + '" within 30000 ms') + ", flush=True)"
            )
            plan = {
                "schema": "tx.ext4.fault_qemu_executor_plan.v1",
                "job": {
                    "case": "write_fsync",
                    "cut": "after-commit",
                    "iteration": 1,
                    "crash_image": str(job_dir / "crash.img"),
                    "replay_image": str(job_dir / "replay.img"),
                },
                "staged_role_images": {"scratch": str(scratch)},
                "shell_test_command": [sys.executable, "-c", runner_program],
                "runner": {
                    "cwd": str(ROOT),
                    "serial_log": str(job_dir / "serial.log"),
                    "cut_marker": marker,
                    "status": "prepared-not-run",
                },
                "hard_kill": {
                    "required": True,
                    "status": "not-implemented",
                    "marker": marker,
                },
            }
            plan_path = job_dir / "executor-plan.json"
            plan_path.write_text(json.dumps(plan), encoding="utf-8")

            module = load_executor_module()
            with self.assertRaisesRegex(module.FaultQemuExecutorError, "did not confirm stop needle"):
                module.execute_prepared_runner(plan_path)
            executor_plan = json.loads(plan_path.read_text(encoding="utf-8"))

        self.assertEqual(executor_plan["runner"]["status"], "cut-not-observed")
        self.assertFalse((job_dir / "crash.img").exists())

    def test_shell_test_command_writes_result_after_replay_e2fsck(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            job_dir = root / "out" / "write_fsync" / "after-commit" / "0001"
            job_dir.mkdir(parents=True)
            marker = "tx-ext4-fault-cut:write_fsync:after-commit"
            serial_log = job_dir / "serial.log"
            scratch = job_dir / "roles" / "scratch.img"
            scratch.parent.mkdir()
            scratch.write_bytes(b"scratch-after-cut")
            replay_image = job_dir / "replay.img"
            check_log = job_dir / "e2fsck-fn.log"
            runner_program = (
                "import os, pathlib; "
                "pathlib.Path(os.environ['TX_EXT4_FAULT_SERIAL_LOG']).write_text(" + repr(marker) + "); "
                "print(" + repr("shell-test: stop needle observed: " + marker) + ", flush=True)"
            )
            e2fsck_program = (
                "import sys; "
                "expected = " + repr(["-fn", str(replay_image)]) + "; "
                "assert sys.argv[1:] == expected, (sys.argv[1:], expected); "
                "print('fake e2fsck', *sys.argv[1:])"
            )
            plan = {
                "schema": "tx.ext4.fault_qemu_executor_plan.v1",
                "job": {
                    "case": "write_fsync",
                    "cut": "after-commit",
                    "iteration": 1,
                    "crash_image": str(job_dir / "crash.img"),
                    "replay_image": str(replay_image),
                    "checks": [
                        {
                            "tool": "e2fsck",
                            "args": ["-fn", str(replay_image)],
                            "log": str(check_log),
                        }
                    ],
                },
                "staged_role_images": {"scratch": str(scratch)},
                "shell_test_command": [sys.executable, "-c", runner_program],
                "runner": {
                    "cwd": str(ROOT),
                    "serial_log": str(serial_log),
                    "cut_marker": marker,
                    "status": "prepared-not-run",
                },
                "hard_kill": {
                    "required": True,
                    "status": "not-implemented",
                    "marker": marker,
                },
            }
            plan_path = job_dir / "executor-plan.json"
            plan_path.write_text(json.dumps(plan), encoding="utf-8")

            module = load_executor_module()
            with patch.dict(
                os.environ,
                {
                    "TX_EXT4_FAULT_E2FSCK_COMMAND": shlex.join([sys.executable, "-c", e2fsck_program]),
                    "TX_EXT4_FAULT_RUNNER_COMMAND": "",
                },
                clear=False,
            ):
                module.execute_prepared_runner(plan_path)
            executor_plan = json.loads(plan_path.read_text(encoding="utf-8"))
            result_path = job_dir / "result.json"
            self.assertTrue(result_path.is_file())
            result = json.loads(result_path.read_text(encoding="utf-8"))
            e2fsck_log = check_log.read_text(encoding="utf-8")

        self.assertEqual(executor_plan["runner"]["command_source"], "shell-test-command")
        self.assertEqual(executor_plan["hard_kill"]["status"], "observed-by-shell-test")
        self.assertEqual(result["schema"], "tx.ext4.fault_job_result.v1")
        self.assertEqual(result["case"], "write_fsync")
        self.assertEqual(result["cut"], "after-commit")
        self.assertEqual(result["iteration"], 1)
        self.assertTrue(result["hard_kill_observed"])
        self.assertTrue(result["replay_attempted"])
        self.assertEqual(result["e2fsck_exit"], 0)
        self.assertEqual(
            result["e2fsck_checks"],
            [
                {
                    "tool": "e2fsck",
                    "args": ["-fn", str(replay_image)],
                    "log": str(check_log),
                    "log_sha256": hashlib.sha256(e2fsck_log.encode("utf-8")).hexdigest(),
                    "exit_code": 0,
                }
            ],
        )
        self.assertIn(f"-fn {replay_image}", e2fsck_log)

    def test_shell_test_command_keeps_stdout_open_until_serial_log_is_written(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            job_dir = root / "out" / "write_fsync" / "after-commit" / "0001"
            job_dir.mkdir(parents=True)
            marker = "tx-ext4-fault-cut:write_fsync:after-commit"
            serial_log = job_dir / "serial.log"
            scratch = job_dir / "roles" / "scratch.img"
            scratch.parent.mkdir()
            scratch.write_bytes(b"scratch-after-cut")
            replay_image = job_dir / "replay.img"
            check_log = job_dir / "e2fsck-fn.log"
            runner_program = (
                "import os, pathlib; "
                "print(" + repr("shell-test: stop needle observed: " + marker) + ", flush=True); "
                "print('shell-test: serial log pending', flush=True); "
                "pathlib.Path(os.environ['TX_EXT4_FAULT_SERIAL_LOG']).write_text("
                + repr(marker + "\\n")
                + ")"
            )
            e2fsck_program = "print('fake e2fsck ok')"
            plan = {
                "schema": "tx.ext4.fault_qemu_executor_plan.v1",
                "job": {
                    "case": "write_fsync",
                    "cut": "after-commit",
                    "iteration": 1,
                    "crash_image": str(job_dir / "crash.img"),
                    "replay_image": str(replay_image),
                    "checks": [
                        {
                            "tool": "e2fsck",
                            "args": ["-fn", str(replay_image)],
                            "log": str(check_log),
                        }
                    ],
                },
                "staged_role_images": {"scratch": str(scratch)},
                "shell_test_command": [sys.executable, "-c", runner_program],
                "runner": {
                    "cwd": str(ROOT),
                    "serial_log": str(serial_log),
                    "cut_marker": marker,
                    "status": "prepared-not-run",
                },
                "hard_kill": {
                    "required": True,
                    "status": "not-implemented",
                    "marker": marker,
                },
            }
            plan_path = job_dir / "executor-plan.json"
            plan_path.write_text(json.dumps(plan), encoding="utf-8")

            module = load_executor_module()
            with patch.dict(
                os.environ,
                {"TX_EXT4_FAULT_E2FSCK_COMMAND": shlex.join([sys.executable, "-c", e2fsck_program])},
                clear=False,
            ):
                module.execute_prepared_runner(plan_path)
            executor_plan = json.loads(plan_path.read_text(encoding="utf-8"))
            result_exists = (job_dir / "result.json").is_file()

        self.assertEqual(executor_plan["runner"]["status"], "exited-after-cut")
        self.assertEqual(executor_plan["result"]["status"], "written")
        self.assertTrue(result_exists)

    def test_shell_test_command_refuses_e2fsck_check_not_bound_to_replay_image(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            job_dir = root / "out" / "write_fsync" / "after-commit" / "0001"
            job_dir.mkdir(parents=True)
            marker = "tx-ext4-fault-cut:write_fsync:after-commit"
            serial_log = job_dir / "serial.log"
            scratch = job_dir / "roles" / "scratch.img"
            scratch.parent.mkdir()
            scratch.write_bytes(b"scratch-after-cut")
            replay_image = job_dir / "replay.img"
            check_log = job_dir / "e2fsck-fn.log"
            runner_program = (
                "import os, pathlib; "
                "pathlib.Path(os.environ['TX_EXT4_FAULT_SERIAL_LOG']).write_text(" + repr(marker) + "); "
                "print(" + repr("shell-test: stop needle observed: " + marker) + ", flush=True)"
            )
            e2fsck_program = "print('fake e2fsck ok')"
            plan = {
                "schema": "tx.ext4.fault_qemu_executor_plan.v1",
                "job": {
                    "case": "write_fsync",
                    "cut": "after-commit",
                    "iteration": 1,
                    "crash_image": str(job_dir / "crash.img"),
                    "replay_image": str(replay_image),
                    "checks": [
                        {
                            "tool": "e2fsck",
                            "args": ["-fn", str(job_dir / "crash.img")],
                            "log": str(check_log),
                        }
                    ],
                },
                "staged_role_images": {"scratch": str(scratch)},
                "shell_test_command": [sys.executable, "-c", runner_program],
                "runner": {
                    "cwd": str(ROOT),
                    "serial_log": str(serial_log),
                    "cut_marker": marker,
                    "status": "prepared-not-run",
                },
                "hard_kill": {
                    "required": True,
                    "status": "not-implemented",
                    "marker": marker,
                },
            }
            plan_path = job_dir / "executor-plan.json"
            plan_path.write_text(json.dumps(plan), encoding="utf-8")

            module = load_executor_module()
            with patch.dict(
                os.environ,
                {"TX_EXT4_FAULT_E2FSCK_COMMAND": shlex.join([sys.executable, "-c", e2fsck_program])},
                clear=False,
            ):
                module.execute_prepared_runner(plan_path)
            executor_plan = json.loads(plan_path.read_text(encoding="utf-8"))

        self.assertEqual(executor_plan["result"]["status"], "not-written")
        self.assertEqual(
            executor_plan["result"]["reason"],
            "e2fsck check must run -fn against the replay image",
        )
        self.assertFalse((job_dir / "result.json").exists())

    def test_refuses_result_when_default_linux_replay_is_host_blocked(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            job_dir = root / "out" / "write_fsync" / "after-commit" / "0001"
            job_dir.mkdir(parents=True)
            replay = job_dir / "replay.img"
            crash = job_dir / "crash.img"
            replay.write_bytes(b"replay")
            crash.write_bytes(b"crash")
            plan_path = job_dir / "executor-plan.json"
            plan = {
                "schema": "tx.ext4.fault_qemu_executor_plan.v1",
                "job": {
                    "case": "write_fsync",
                    "cut": "after-commit",
                    "iteration": 1,
                    "crash_image": str(crash),
                    "replay_image": str(replay),
                    "checks": [
                        {
                            "tool": "e2fsck",
                            "args": ["-fn", str(replay)],
                            "log": str(job_dir / "e2fsck-fn.log"),
                        }
                    ],
                    "replay_matrix": [
                        {
                            "id": "linux-rw-replay",
                            "image": str(job_dir / "linux-replay.img"),
                            "log": str(job_dir / "linux-rw-replay.log"),
                        }
                    ],
                },
                "runner": {"command_source": "shell-test-command", "cwd": str(ROOT)},
                "hard_kill": {"status": "observed-by-shell-test"},
                "result": {"path": str(job_dir / "result.json"), "status": "not-written"},
            }
            plan_path.write_text(json.dumps(plan), encoding="utf-8")

            module = load_executor_module()
            with patch.dict(
                os.environ,
                {"TX_EXT4_FAULT_E2FSCK_COMMAND": shlex.join([sys.executable, "-c", "print('fsck')"])},
                clear=False,
            ), patch.object(
                module.subprocess,
                "run",
                side_effect=[
                    subprocess.CompletedProcess(["e2fsck"], 0, "fsck\n", ""),
                    subprocess.CompletedProcess(
                        [
                            str(ROOT / "tools" / "ext4" / "fault_linux_rw_replay.py"),
                            "--preflight",
                        ],
                        1,
                        "",
                        "error: Linux RW replay requires a Linux host\n",
                    ),
                ],
            ):
                module.produce_result_if_verified(plan_path, plan)

        self.assertFalse((job_dir / "result.json").exists())
        self.assertEqual(
            plan["result"]["reason"],
            "Linux RW replay requires a Linux host",
        )

    def test_refuses_result_when_semantic_oracles_have_no_executor(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            job_dir = root / "out" / "write_fsync" / "after-commit" / "0001"
            job_dir.mkdir(parents=True)
            replay = job_dir / "replay.img"
            crash = job_dir / "crash.img"
            replay.write_bytes(b"replay")
            crash.write_bytes(b"crash")
            plan_path = job_dir / "executor-plan.json"
            plan = {
                "schema": "tx.ext4.fault_qemu_executor_plan.v1",
                "job": {
                    "case": "write_fsync",
                    "cut": "after-commit",
                    "iteration": 1,
                    "crash_image": str(crash),
                    "replay_image": str(replay),
                    "checks": [
                        {
                            "tool": "e2fsck",
                            "args": ["-fn", str(replay)],
                            "log": str(job_dir / "e2fsck-fn.log"),
                        }
                    ],
                    "semantic_oracles": [
                        {
                            "id": "debugfs-file-hash-namespace",
                            "image": str(job_dir / "semantic-debugfs.img"),
                            "log": str(job_dir / "semantic-debugfs.log"),
                            "expected": {"present": {"/tx-fault-scratch/fault-cut.txt": {}}},
                        }
                    ],
                },
                "runner": {"command_source": "shell-test-command", "cwd": str(ROOT)},
                "hard_kill": {"status": "observed-by-shell-test"},
                "result": {"path": str(job_dir / "result.json"), "status": "not-written"},
            }
            plan_path.write_text(json.dumps(plan), encoding="utf-8")

            module = load_executor_module()
            with patch.dict(
                os.environ,
                {
                    "TX_EXT4_FAULT_E2FSCK_COMMAND": shlex.join(
                        [sys.executable, "-c", "print('fsck')"]
                    ),
                    "TX_EXT4_FAULT_SEMANTIC_ORACLE_COMMAND": "",
                },
                clear=False,
            ):
                module.produce_result_if_verified(plan_path, plan)

        self.assertFalse((job_dir / "result.json").exists())
        self.assertEqual(
            plan["result"]["reason"],
            "empty TX_EXT4_FAULT_SEMANTIC_ORACLE_COMMAND",
        )

    def test_semantic_preflight_blocks_before_primary_runner_when_command_is_missing(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            job_dir = root / "out" / "write_fsync" / "after-commit" / "0001"
            job_dir.mkdir(parents=True)
            marker = "tx-ext4-fault-cut:write_fsync:after-commit"
            serial_log = job_dir / "serial.log"
            plan = {
                "schema": "tx.ext4.fault_qemu_executor_plan.v1",
                "job": {
                    "case": "write_fsync",
                    "cut": "after-commit",
                    "iteration": 1,
                    "crash_image": str(job_dir / "crash.img"),
                    "replay_image": str(job_dir / "replay.img"),
                    "semantic_oracles": [
                        {
                            "id": "debugfs-file-hash-namespace",
                            "image": str(job_dir / "semantic-debugfs.img"),
                            "log": str(job_dir / "semantic-debugfs.log"),
                            "expected": {"present": {"/tx-fault-scratch/fault-cut.txt": {}}},
                        }
                    ],
                },
                "runner": {
                    "cwd": str(ROOT),
                    "serial_log": str(serial_log),
                    "command": [sys.executable, "-c", "raise SystemExit(99)"],
                    "cut_marker": marker,
                    "status": "prepared-not-run",
                },
                "hard_kill": {"status": "not-implemented"},
                "result": {"path": str(job_dir / "result.json"), "status": "not-written"},
            }
            plan_path = job_dir / "executor-plan.json"
            plan_path.write_text(json.dumps(plan), encoding="utf-8")

            module = load_executor_module()
            with patch.dict(os.environ, {"TX_EXT4_FAULT_SEMANTIC_ORACLE_COMMAND": ""}, clear=False):
                module.execute_prepared_runner(plan_path)
            recorded = json.loads(plan_path.read_text(encoding="utf-8"))

        self.assertEqual(recorded["runner"]["status"], "prepared-not-run")
        self.assertEqual(recorded["semantic_oracle_preflight"]["status"], "blocked")
        self.assertIn("TX_EXT4_FAULT_SEMANTIC_ORACLE_COMMAND", recorded["result"]["reason"])
        self.assertFalse(serial_log.exists())

    def test_semantic_preflight_rejects_non_executable_command(self):
        module = load_executor_module()
        plan = {
            "job": {
                "semantic_oracles": [
                    {
                        "id": "debugfs-file-hash-namespace",
                        "image": "/tmp/semantic.img",
                        "log": "/tmp/semantic.log",
                        "expected": {"absent": ["/missing"]},
                    }
                ]
            },
            "result": {"status": "not-written"},
        }
        with patch.dict(
            os.environ,
            {"TX_EXT4_FAULT_SEMANTIC_ORACLE_COMMAND": "/tmp/not-an-executable-semantic-oracle"},
            clear=False,
        ):
            self.assertFalse(module.preflight_semantic_oracles(plan))

        self.assertEqual(plan["semantic_oracle_preflight"]["status"], "blocked")
        self.assertIn("not executable", plan["result"]["reason"])

    def test_semantic_preflight_uses_repository_default_runner(self):
        module = load_executor_module()
        plan = {
            "job": {
                "semantic_oracles": [
                    {
                        "id": "debugfs-file-hash-namespace",
                        "image": "/tmp/semantic.img",
                        "log": "/tmp/semantic.log",
                        "expected": {"absent": ["/missing"]},
                    }
                ]
            },
            "result": {"status": "not-written"},
        }
        with patch.dict(os.environ, {}, clear=True):
            self.assertTrue(module.preflight_semantic_oracles(plan))

        preflight = plan["semantic_oracle_preflight"]
        self.assertEqual(preflight["status"], "ready")
        self.assertEqual(preflight["command_source"], "repository-default")
        self.assertEqual(preflight["command"], [str(ROOT / "tools" / "ext4" / "fault_semantic_oracle.py")])

    def test_semantic_preflight_records_environment_override(self):
        module = load_executor_module()
        plan = {
            "job": {
                "semantic_oracles": [
                    {
                        "id": "debugfs-file-hash-namespace",
                        "image": "/tmp/semantic.img",
                        "log": "/tmp/semantic.log",
                        "expected": {"absent": ["/missing"]},
                    }
                ]
            },
            "result": {"status": "not-written"},
        }
        command = shlex.join([sys.executable, "-c", "raise SystemExit(0)"])
        with patch.dict(os.environ, {"TX_EXT4_FAULT_SEMANTIC_ORACLE_COMMAND": command}, clear=False):
            self.assertTrue(module.preflight_semantic_oracles(plan))

        preflight = plan["semantic_oracle_preflight"]
        self.assertEqual(preflight["command_source"], "environment-override")
        self.assertEqual(preflight["command"], shlex.split(command))

    def test_semantic_preflight_rejects_missing_repository_default_runner(self):
        module = load_executor_module()
        plan = {
            "job": {
                "semantic_oracles": [
                    {
                        "id": "debugfs-file-hash-namespace",
                        "image": "/tmp/semantic.img",
                        "log": "/tmp/semantic.log",
                        "expected": {"absent": ["/missing"]},
                    }
                ]
            },
            "result": {"status": "not-written"},
        }
        with patch.dict(os.environ, {}, clear=True), patch.object(
            module, "repository_semantic_oracle_path", return_value=Path("/missing/semantic-oracle")
        ):
            self.assertFalse(module.preflight_semantic_oracles(plan))

        self.assertEqual(plan["semantic_oracle_preflight"]["status"], "blocked")
        self.assertIn("repository semantic oracle command is missing", plan["result"]["reason"])

    def test_semantic_preflight_rejects_non_executable_repository_default_runner(self):
        module = load_executor_module()
        plan = {
            "job": {
                "semantic_oracles": [
                    {
                        "id": "debugfs-file-hash-namespace",
                        "image": "/tmp/semantic.img",
                        "log": "/tmp/semantic.log",
                        "expected": {"absent": ["/missing"]},
                    }
                ]
            },
            "result": {"status": "not-written"},
        }
        with patch.dict(os.environ, {}, clear=True), patch.object(module.os, "access", return_value=False):
            self.assertFalse(module.preflight_semantic_oracles(plan))

        self.assertEqual(plan["semantic_oracle_preflight"]["status"], "blocked")
        self.assertIn("repository semantic oracle command is not executable", plan["result"]["reason"])

    def test_runs_semantic_oracle_and_records_observation_after_existing_gates(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            job_dir = root / "out" / "write_fsync" / "after-commit" / "0001"
            job_dir.mkdir(parents=True)
            replay = job_dir / "replay.img"
            crash = job_dir / "crash.img"
            replay.write_bytes(b"replay")
            crash.write_bytes(b"crash")
            semantic_image = job_dir / "semantic-debugfs.img"
            semantic_log = job_dir / "semantic-debugfs.log"
            oracle_program = (
                "import json, pathlib, sys; "
                "request = json.loads(pathlib.Path(sys.argv[1]).read_text()); "
                "assert pathlib.Path(request['image']).read_bytes() == b'replay'; "
                "pathlib.Path(request['log']).write_text('semantic ok\\n'); "
                "print('semantic ok')"
            )
            plan = {
                "schema": "tx.ext4.fault_qemu_executor_plan.v1",
                "job": {
                    "case": "write_fsync",
                    "cut": "after-commit",
                    "iteration": 1,
                    "crash_image": str(crash),
                    "replay_image": str(replay),
                    "checks": [
                        {
                            "tool": "e2fsck",
                            "args": ["-fn", str(replay)],
                            "log": str(job_dir / "e2fsck-fn.log"),
                        }
                    ],
                    "replay_matrix": [
                        {
                            "id": "linux-rw-replay",
                            "image": str(job_dir / "linux-replay.img"),
                            "log": str(job_dir / "linux-rw-replay.log"),
                        },
                        {
                            "id": "linux-post-replay-e2fsck",
                            "image": str(job_dir / "linux-replay.img"),
                            "args": ["-fn", str(job_dir / "linux-replay.img")],
                            "log": str(job_dir / "linux-post-replay-e2fsck.log"),
                        },
                        {
                            "id": "tx-remount",
                            "image": str(job_dir / "tx-remount.img"),
                            "log": str(job_dir / "tx-remount.log"),
                        },
                    ],
                    "semantic_oracles": [
                        {
                            "id": "debugfs-file-hash-namespace",
                            "image": str(semantic_image),
                            "log": str(semantic_log),
                            "expected": {"present": {"/tx-fault-scratch/fault-cut.txt": {}}},
                        }
                    ],
                },
                "runner": {"command_source": "shell-test-command", "cwd": str(ROOT)},
                "hard_kill": {"status": "observed-by-shell-test"},
                "result": {"path": str(job_dir / "result.json"), "status": "not-written"},
            }
            plan_path = job_dir / "executor-plan.json"
            plan_path.write_text(json.dumps(plan), encoding="utf-8")
            command = shlex.join([sys.executable, "-c", "import sys; print('replayed', sys.argv[-1])"])

            module = load_executor_module()
            with mock.patch.object(module, "matrix_command", return_value=shlex.split(command)), mock.patch.object(
                module, "e2fsck_command", return_value=[sys.executable, "-c", "print('fsck')"]
            ), mock.patch.object(
                module,
                "semantic_oracle_command",
                return_value=([sys.executable, "-c", oracle_program], "test-double"),
            ):
                module.produce_result_if_verified(plan_path, plan)
            manifest = json.loads((job_dir / "result.json").read_text(encoding="utf-8"))
            semantic_image_bytes = semantic_image.read_bytes()

        self.assertEqual(semantic_image_bytes, b"replay")
        self.assertEqual(manifest["semantic_oracles"][0]["id"], "debugfs-file-hash-namespace")
        self.assertEqual(manifest["semantic_oracles"][0]["exit_code"], 0)
        self.assertEqual(
            manifest["semantic_oracles"][0]["log_sha256"],
            hashlib.sha256(b"semantic ok\n").hexdigest(),
        )
        self.assertEqual(
            manifest["semantic_oracles"][0]["image_sha256"],
            hashlib.sha256(b"replay").hexdigest(),
        )

    def test_matrix_preflight_blocks_before_primary_fault_runner(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            job_dir = root / "out" / "write_fsync" / "after-commit" / "0001"
            job_dir.mkdir(parents=True)
            marker = "tx-ext4-fault-cut:write_fsync:after-commit"
            serial_log = job_dir / "serial.log"
            runner_program = (
                "import os, pathlib; "
                "pathlib.Path(os.environ['TX_EXT4_FAULT_SERIAL_LOG']).write_text(" + repr(marker) + "); "
                "print(" + repr(marker) + ")"
            )
            plan = {
                "schema": "tx.ext4.fault_qemu_executor_plan.v1",
                "job": {
                    "case": "write_fsync",
                    "cut": "after-commit",
                    "iteration": 1,
                    "crash_image": str(job_dir / "crash.img"),
                    "replay_image": str(job_dir / "replay.img"),
                    "checks": [],
                    "replay_matrix": [
                        {
                            "id": "linux-rw-replay",
                            "image": str(job_dir / "linux-replay.img"),
                            "log": str(job_dir / "linux-rw-replay.log"),
                        }
                    ],
                },
                "runner": {
                    "cwd": str(ROOT),
                    "serial_log": str(serial_log),
                    "command": [sys.executable, "-c", runner_program],
                    "cut_marker": marker,
                    "status": "prepared-not-run",
                },
                "hard_kill": {"status": "not-implemented"},
                "result": {"path": str(job_dir / "result.json"), "status": "not-written"},
            }
            plan_path = job_dir / "executor-plan.json"
            plan_path.write_text(json.dumps(plan), encoding="utf-8")

            module = load_executor_module()
            with patch.dict(
                os.environ,
                {"TX_EXT4_FAULT_LINUX_REPLAY_COMMAND": ""},
                clear=False,
            ):
                module.execute_prepared_runner(plan_path)
            recorded = json.loads(plan_path.read_text(encoding="utf-8"))

        self.assertEqual(recorded["runner"]["status"], "prepared-not-run")
        self.assertEqual(recorded["replay_matrix_preflight"]["status"], "blocked")
        self.assertIn("TX_EXT4_FAULT_LINUX_REPLAY_COMMAND", recorded["result"]["reason"])
        self.assertFalse(serial_log.exists())
        self.assertFalse((job_dir / "result.json").exists())

    def test_matrix_preflight_uses_executable_tx_remount_default(self):
        module = load_executor_module()
        with tempfile.TemporaryDirectory() as tmp:
            runner = Path(tmp) / "fault-tx-remount"
            runner.write_text("#!/bin/sh\n", encoding="utf-8")
            runner.chmod(0o755)
            plan = {
                "job": {
                    "replay_matrix": [
                        {
                            "id": "tx-remount",
                            "image": "/tmp/tx-remount.img",
                            "log": "/tmp/tx-remount.log",
                        }
                    ]
                },
                "result": {"status": "not-written"},
            }
            with patch.dict(os.environ, {}, clear=True), patch.object(
                module, "repository_tx_remount_path", return_value=runner, create=True
            ):
                self.assertTrue(module.preflight_replay_matrix(plan))

        self.assertEqual(plan["replay_matrix_preflight"]["status"], "ready")
        self.assertEqual(
            plan["replay_matrix_preflight"]["commands"],
            [
                {
                    "id": "tx-remount",
                    "command_source": "repository-default",
                    "command": [str(runner)],
                }
            ],
        )

    def test_matrix_preflight_blocks_when_repository_linux_default_preflight_fails(self):
        module = load_executor_module()
        with tempfile.TemporaryDirectory() as tmp:
            runner = Path(tmp) / "fault-linux-rw-replay"
            runner.write_text("#!/bin/sh\n", encoding="utf-8")
            runner.chmod(0o755)
            plan = {
                "job": {
                    "replay_matrix": [
                        {
                            "id": "linux-rw-replay",
                            "image": "/tmp/linux-replay.img",
                            "log": "/tmp/linux-replay.log",
                        }
                    ]
                },
                "result": {"status": "not-written"},
            }
            completed = subprocess.CompletedProcess(
                [str(runner), "--preflight"],
                1,
                "",
                "error: Linux RW replay requires a Linux host; Docker fallback blocked: docker is unavailable\n",
            )
            with patch.dict(os.environ, {}, clear=True), patch.object(
                module, "repository_linux_rw_replay_path", return_value=runner, create=True
            ), patch.object(module.subprocess, "run", return_value=completed):
                self.assertFalse(module.preflight_replay_matrix(plan))

        self.assertEqual(plan["replay_matrix_preflight"]["status"], "blocked")
        self.assertIn("Linux RW replay requires a Linux host", plan["result"]["reason"])

    def test_matrix_preflight_accepts_repository_linux_default_docker_fallback(self):
        module = load_executor_module()
        with tempfile.TemporaryDirectory() as tmp:
            runner = Path(tmp) / "fault-linux-rw-replay"
            runner.write_text("#!/bin/sh\n", encoding="utf-8")
            runner.chmod(0o755)
            plan = {
                "job": {
                    "replay_matrix": [
                        {
                            "id": "linux-rw-replay",
                            "image": "/tmp/linux-replay.img",
                            "log": "/tmp/linux-replay.log",
                        }
                    ]
                },
                "result": {"status": "not-written"},
            }
            completed = subprocess.CompletedProcess(
                [str(runner), "--preflight"],
                0,
                "linux-rw-replay-preflight:docker\n",
                "",
            )
            with patch.dict(os.environ, {}, clear=True), patch.object(
                module, "repository_linux_rw_replay_path", return_value=runner, create=True
            ), patch.object(module.subprocess, "run", return_value=completed):
                self.assertTrue(module.preflight_replay_matrix(plan))

        self.assertEqual(plan["replay_matrix_preflight"]["status"], "ready")
        self.assertEqual(
            plan["replay_matrix_preflight"]["commands"][0],
            {
                "id": "linux-rw-replay",
                "command_source": "repository-default",
                "command": [str(runner)],
            },
        )

    def test_matrix_preflight_rejects_empty_semantic_runner_override(self):
        module = load_executor_module()
        plan = {
            "job": {
                "replay_matrix": [
                    {
                        "id": "tx-remount",
                        "image": "/tmp/tx-remount.img",
                        "log": "/tmp/tx-remount.log",
                    }
                ]
            },
            "result": {"status": "not-written"},
        }
        with patch.dict(os.environ, {"TX_EXT4_FAULT_TX_REMOUNT_COMMAND": ""}, clear=True):
            self.assertFalse(module.preflight_replay_matrix(plan))

        self.assertEqual(plan["replay_matrix_preflight"]["status"], "blocked")
        self.assertIn("TX_EXT4_FAULT_TX_REMOUNT_COMMAND is not allowed", plan["result"]["reason"])

    def test_matrix_preflight_rejects_semantic_runner_override(self):
        module = load_executor_module()
        plan = {
            "job": {
                "replay_matrix": [
                    {
                        "id": "tx-remount",
                        "image": "/tmp/tx-remount.img",
                        "log": "/tmp/tx-remount.log",
                    }
                ]
            },
            "result": {"status": "not-written"},
        }
        with patch.dict(
            os.environ,
            {"TX_EXT4_FAULT_TX_REMOUNT_COMMAND": "/bin/echo forged-remount"},
            clear=True,
        ):
            self.assertFalse(module.preflight_replay_matrix(plan))

        self.assertEqual(plan["replay_matrix_preflight"]["status"], "blocked")
        self.assertIn("TX_EXT4_FAULT_TX_REMOUNT_COMMAND is not allowed", plan["result"]["reason"])

    def test_executes_replay_matrix_commands_before_writing_result(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            job_dir = root / "out" / "write_fsync" / "after-commit" / "0001"
            job_dir.mkdir(parents=True)
            replay = job_dir / "replay.img"
            crash = job_dir / "crash.img"
            replay.write_bytes(b"replay")
            crash.write_bytes(b"crash")
            linux_replay = job_dir / "linux-replay.img"
            tx_remount = job_dir / "tx-remount.img"
            plan_path = job_dir / "executor-plan.json"
            plan = {
                "schema": "tx.ext4.fault_qemu_executor_plan.v1",
                "campaign_plan_sha256": "b" * 64,
                "job": {
                    "case": "write_fsync",
                    "cut": "after-commit",
                    "iteration": 1,
                    "crash_image": str(crash),
                    "replay_image": str(replay),
                    "checks": [
                        {
                            "tool": "e2fsck",
                            "args": ["-fn", str(replay)],
                            "log": str(job_dir / "e2fsck-fn.log"),
                        }
                    ],
                    "replay_matrix": [
                        {
                            "id": "linux-rw-replay",
                            "image": str(linux_replay),
                            "log": str(job_dir / "linux-rw-replay.log"),
                        },
                        {
                            "id": "linux-post-replay-e2fsck",
                            "image": str(linux_replay),
                            "args": ["-fn", str(linux_replay)],
                            "log": str(job_dir / "linux-post-replay-e2fsck.log"),
                        },
                        {
                            "id": "tx-remount",
                            "image": str(tx_remount),
                            "log": str(job_dir / "tx-remount.log"),
                        },
                    ],
                },
                "runner": {"command_source": "shell-test-command", "cwd": str(ROOT)},
                "hard_kill": {"status": "observed-by-shell-test"},
                "result": {"path": str(job_dir / "result.json"), "status": "not-written"},
            }
            plan_path.write_text(json.dumps(plan), encoding="utf-8")
            command = shlex.join([sys.executable, "-c", "import sys; print('replayed', sys.argv[-1])"])

            module = load_executor_module()
            with mock.patch.object(module, "matrix_command", return_value=shlex.split(command)), mock.patch.object(
                module, "e2fsck_command", return_value=[sys.executable, "-c", "print('fsck')"]
            ):
                module.produce_result_if_verified(plan_path, plan)
            result = json.loads((job_dir / "result.json").read_text(encoding="utf-8"))
            linux_replay_bytes = linux_replay.read_bytes()
            tx_remount_bytes = tx_remount.read_bytes()

        self.assertEqual([entry["id"] for entry in result["replay_matrix"]], [
            "linux-rw-replay",
            "linux-post-replay-e2fsck",
            "tx-remount",
        ])
        self.assertEqual(result["campaign_plan_sha256"], "b" * 64)
        self.assertEqual([entry["exit_code"] for entry in result["replay_matrix"]], [0, 0, 0])
        self.assertEqual(result["replay_matrix"][0]["image_sha256"], hashlib.sha256(b"replay").hexdigest())
        self.assertEqual(result["replay_matrix"][2]["image_sha256"], hashlib.sha256(b"replay").hexdigest())
        self.assertEqual(linux_replay_bytes, b"replay")
        self.assertEqual(tx_remount_bytes, b"replay")

    def test_replay_matrix_passes_job_case_and_cut_to_tx_remount(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            replay = root / "replay.img"
            replay.write_bytes(b"replay")
            remount = root / "tx-remount.img"
            command_calls = []
            plan = {"runner": {"cwd": str(ROOT)}}
            job = {
                "case": "rename_replace",
                "cut": "after-commit",
                "replay_image": str(replay),
                "replay_matrix": [
                    {
                        "id": "tx-remount",
                        "image": str(remount),
                        "log": str(root / "tx-remount.log"),
                    }
                ],
            }
            result = {}
            module = load_executor_module()

            def record_command(command, entry, log, cwd, current_result):
                command_calls.append(command)
                log.write_text("ok\n", encoding="utf-8")
                return {**entry, "exit_code": 0, "log_sha256": "a" * 64}

            with mock.patch.object(
                module,
                "matrix_command",
                return_value=["fault-tx-remount"],
            ), mock.patch.object(module, "run_matrix_command", side_effect=record_command):
                observations = module.execute_replay_matrix(plan, job, result)

        self.assertIsNotNone(observations)
        self.assertEqual(
            command_calls,
            [["fault-tx-remount", "rename_replace", "after-commit", str(remount)]],
        )


if __name__ == "__main__":
    unittest.main()
