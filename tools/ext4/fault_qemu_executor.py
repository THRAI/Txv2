#!/usr/bin/env python3
"""Fail-closed QEMU executor scaffold for ext4 fault campaign jobs.

This script converts a
tx.ext4.fault_job_request.v1 into a per-job execution plan, stages private role
image copies, runs the planned shell-test until its cut marker, and preserves
crash/replay image copies. It writes a result manifest only after the default
shell-test path observes its hard-kill contract, every required e2fsck -fn
check succeeds, and declared replay-matrix commands complete. Linux RW replay
and Tx remount always resolve to repository-owned runners; the Linux runner
fail-closes before the primary run unless the host is Linux root with loop-mount
tooling. Declared semantic oracles default to the repository-owned
fault_semantic_oracle.py runner. Set TX_EXT4_FAULT_PLAN_ONLY=1 for plan-only
preflight.
"""

from __future__ import annotations

import hashlib
import json
import os
import platform
import shlex
import shutil
import subprocess
import sys
from pathlib import Path
from typing import Any


class FaultQemuExecutorError(Exception):
    pass


HOMEBREW_E2FSCK = Path("/opt/homebrew/opt/e2fsprogs/sbin/e2fsck")


def main(argv: list[str] | None = None) -> int:
    args = list(sys.argv[1:] if argv is None else argv)
    if len(args) != 1:
        print("error: expected one job-request.json path", file=sys.stderr)
        return 1
    try:
        request_path = Path(args[0])
        request = load_request(request_path)
        plan_path = write_executor_plan(request_path, request)
        execute_prepared_runner(plan_path)
    except FaultQemuExecutorError as err:
        print(f"error: {err}", file=sys.stderr)
        return 1

    plan = json.loads(plan_path.read_text(encoding="utf-8"))
    result = plan.get("result")
    if isinstance(result, dict) and result.get("status") == "written":
        print(f"fault qemu executor result: {result['path']}")
        return 0

    print(f"fault qemu executor plan: {plan_path}")
    reason = result.get("reason") if isinstance(result, dict) else None
    if isinstance(reason, str) and reason:
        print(f"error: refusing to write crash evidence: {reason}", file=sys.stderr)
    else:
        print("error: refusing to write crash evidence", file=sys.stderr)
    return 1


def load_request(path: Path) -> dict[str, Any]:
    if not path.is_file():
        raise FaultQemuExecutorError(f"missing job request: {path}")
    try:
        request = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as err:
        raise FaultQemuExecutorError(f"invalid job request JSON {path}: {err}") from err
    if request.get("schema") != "tx.ext4.fault_job_request.v1":
        raise FaultQemuExecutorError("job request schema mismatch")
    require_object(request, "job")
    require_object(request, "role_images")
    require_object(request, "qemu")
    return request


def require_object(parent: dict[str, Any], field: str) -> dict[str, Any]:
    value = parent.get(field)
    if not isinstance(value, dict):
        raise FaultQemuExecutorError(f"job request missing object {field}")
    return value


def require_string(parent: dict[str, Any], field: str) -> str:
    value = parent.get(field)
    if not isinstance(value, str) or not value:
        raise FaultQemuExecutorError(f"job request missing {field}")
    return value


def require_positive_int(parent: dict[str, Any], field: str) -> int:
    value = parent.get(field)
    if not isinstance(value, int) or value <= 0:
        raise FaultQemuExecutorError(f"job request {field} must be positive")
    return value


def write_executor_plan(request_path: Path, request: dict[str, Any]) -> Path:
    job_dir = request_path.parent
    refuse_stale_result_manifest(job_dir)
    job = require_object(request, "job")
    role_images = require_object(request, "role_images")
    qemu = require_object(request, "qemu")
    cut_marker = require_string(qemu, "cut_marker")
    require_script_contains_cut_marker(Path(require_string(qemu, "script")), cut_marker)
    staged = stage_role_images(job_dir, role_images)
    serial_log = require_string(job, "serial_log")
    shell_test_command = [
        "cargo",
        "xtask",
        "shell-test",
        "--target",
        require_string(qemu, "target"),
        "--profile",
        require_string(qemu, "profile"),
        "--script",
        require_string(qemu, "script"),
        "--serial-log",
        serial_log,
        "--timeout-ms",
        str(require_positive_int(qemu, "timeout_ms")),
        "--stop-after-needle",
        cut_marker,
        "--extra-rv64-ext4",
        staged["test"],
        "--extra-rv64-ext4",
        staged["scratch"],
        "--extra-rv64-ext4",
        staged["workload"],
    ]
    plan = {
        "schema": "tx.ext4.fault_qemu_executor_plan.v1",
        "request": str(request_path),
        "campaign_plan_sha256": request.get("campaign_plan_sha256"),
        "job": {
            "case": require_string(job, "case"),
            "cut": require_string(job, "cut"),
            "iteration": require_positive_int(job, "iteration"),
            "crash_image": require_string(job, "crash_image"),
            "replay_image": require_string(job, "replay_image"),
            "checks": job.get("checks"),
            "replay_matrix": job.get("replay_matrix"),
            "semantic_oracles": job.get("semantic_oracles"),
        },
        "staged_role_images": staged,
        "shell_test_command": shell_test_command,
        "runner": {
            "cwd": str(Path.cwd()),
            "serial_log": serial_log,
            "command": shell_test_command,
            "cut_marker": cut_marker,
            "status": "prepared-not-run",
        },
        "hard_kill": {
            "required": True,
            "status": "not-implemented",
            "cut": require_string(job, "cut"),
            "marker": cut_marker,
        },
        "replay_matrix_preflight": {"status": "not-run"},
        "semantic_oracle_preflight": {"status": "not-run"},
        "result": {
            "path": str(job_dir / "result.json"),
            "status": "not-written",
            "reason": "hard-kill observation and replay e2fsck are required",
        },
        "notes": (
            "This plan is not crash evidence until shell-test observes the requested "
            "hard-kill path, the executor preserves the images, and replay e2fsck "
            "succeeds."
        ),
    }
    path = job_dir / "executor-plan.json"
    path.write_text(json.dumps(plan, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return path


def execute_prepared_runner(plan_path: Path) -> None:
    if os.environ.get("TX_EXT4_FAULT_PLAN_ONLY") == "1":
        return

    plan = json.loads(plan_path.read_text(encoding="utf-8"))
    if not preflight_replay_matrix(plan):
        write_plan(plan_path, plan)
        return
    if not preflight_semantic_oracles(plan):
        write_plan(plan_path, plan)
        return
    runner = require_object(plan, "runner")
    command = runner_command(plan)
    serial_log = Path(require_string(runner, "serial_log"))
    serial_log.parent.mkdir(parents=True, exist_ok=True)
    command_source = (
        "environment-override"
        if os.environ.get("TX_EXT4_FAULT_RUNNER_COMMAND")
        else "shell-test-command"
    )
    runner["command"] = command
    runner["command_source"] = command_source
    runner["status"] = "running"
    write_plan(plan_path, plan)

    env = os.environ.copy()
    env["TX_EXT4_FAULT_SERIAL_LOG"] = str(serial_log)
    try:
        process = subprocess.Popen(
            command,
            cwd=require_string(runner, "cwd"),
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            bufsize=1,
        )
    except OSError as err:
        runner["status"] = "launch-failed"
        runner["error"] = str(err)
        write_plan(plan_path, plan)
        raise FaultQemuExecutorError(f"runner launch failed: {err}") from err

    marker = require_string(runner, "cut_marker")
    observed = False
    assert process.stdout is not None
    for line in process.stdout:
        if marker in line:
            observed = True
            break

    if not observed:
        process.wait()
        runner["status"] = "cut-not-observed"
        runner["exit_code"] = process.returncode
        write_plan(plan_path, plan)
        raise FaultQemuExecutorError(f"runner exited before cut marker {marker!r}")

    if command_source == "shell-test-command":
        try:
            process.communicate(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
            runner["status"] = "shell-test-timeout-after-cut"
            runner["exit_code"] = process.returncode
            write_plan(plan_path, plan)
            raise FaultQemuExecutorError("shell-test did not exit after observing cut marker")
        runner["status"] = "exited-after-cut"
    else:
        process.terminate()
        try:
            process.communicate(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
        runner["status"] = "terminated-after-cut"
    runner["exit_code"] = process.returncode
    hard_kill = require_object(plan, "hard_kill")
    if command_source == "shell-test-command":
        hard_kill["status"] = "observed-by-shell-test"
        hard_kill["reason"] = (
            "shell-test observed the cut marker and exited after its own QEMU kill path"
        )
    else:
        hard_kill["status"] = "not-observed"
        hard_kill["reason"] = "runner termination is not deterministic QEMU hard kill evidence"

    if not serial_log.is_file():
        runner["status"] = "cut-observed-serial-missing"
        write_plan(plan_path, plan)
        raise FaultQemuExecutorError("cut marker observed on runner stdout but serial log is missing")

    preserve_crash_and_replay_images(plan)
    produce_result_if_verified(plan_path, plan)
    write_plan(plan_path, plan)


def runner_command(plan: dict[str, Any]) -> list[str]:
    override = os.environ.get("TX_EXT4_FAULT_RUNNER_COMMAND")
    if override:
        command = shlex.split(override)
        if not command:
            raise FaultQemuExecutorError("empty TX_EXT4_FAULT_RUNNER_COMMAND")
        return command
    command = plan.get("shell_test_command")
    if not isinstance(command, list) or not all(isinstance(arg, str) and arg for arg in command):
        raise FaultQemuExecutorError("executor plan has invalid shell-test command")
    return command


def preserve_crash_and_replay_images(plan: dict[str, Any]) -> None:
    job = require_object(plan, "job")
    staged = require_object(plan, "staged_role_images")
    scratch = Path(require_string(staged, "scratch"))
    crash = Path(require_string(job, "crash_image"))
    replay = Path(require_string(job, "replay_image"))
    for path, label in ((crash, "crash"), (replay, "replay")):
        if path.exists():
            raise FaultQemuExecutorError(f"refusing to overwrite existing {label} image: {path}")
        path.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(scratch, path)
    plan["preserved_images"] = {
        "crash": str(crash),
        "replay": str(replay),
        "source": str(scratch),
        "status": "copied-after-runner-termination",
    }


def produce_result_if_verified(plan_path: Path, plan: dict[str, Any]) -> None:
    runner = require_object(plan, "runner")
    hard_kill = require_object(plan, "hard_kill")
    result = require_result_object(plan, plan_path)
    if runner.get("command_source") != "shell-test-command":
        result["reason"] = "runner command source is not shell-test-command"
        return
    if hard_kill.get("status") != "observed-by-shell-test":
        result["reason"] = "shell-test did not observe its hard-kill path"
        return

    job = require_object(plan, "job")
    crash = Path(require_string(job, "crash_image"))
    replay = Path(require_string(job, "replay_image"))
    if not crash.is_file() or not replay.is_file():
        result["reason"] = "preserved crash and replay images are required"
        return

    checks = job.get("checks")
    if not isinstance(checks, list) or not checks:
        result["reason"] = "required e2fsck checks are missing"
        return

    e2fsck_seen = False
    executed_checks: list[dict[str, Any]] = []
    for check in checks:
        if not isinstance(check, dict) or check.get("tool") != "e2fsck":
            result["reason"] = "only declared e2fsck checks are supported"
            return
        args = check.get("args")
        log_value = check.get("log")
        if (
            not isinstance(args, list)
            or not all(isinstance(arg, str) and arg for arg in args)
            or not isinstance(log_value, str)
            or not log_value
        ):
            result["reason"] = "e2fsck checks must declare -fn and a log path"
            return
        if args != ["-fn", str(replay)]:
            result["reason"] = "e2fsck check must run -fn against the replay image"
            return
        e2fsck_seen = True
        log_path = Path(log_value)
        command = e2fsck_command() + args
        try:
            completed = subprocess.run(
                command,
                cwd=require_string(runner, "cwd"),
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                check=False,
            )
        except OSError as err:
            result["reason"] = f"e2fsck launch failed: {err}"
            return
        log_path.parent.mkdir(parents=True, exist_ok=True)
        log_path.write_text(completed.stdout, encoding="utf-8")
        log_sha256 = hashlib.sha256(completed.stdout.encode("utf-8")).hexdigest()
        executed_checks.append(
            {
                "tool": "e2fsck",
                "args": args,
                "log": str(log_path),
                "log_sha256": log_sha256,
                "exit_code": completed.returncode,
            }
        )
        if completed.returncode != 0:
            result["reason"] = "e2fsck -fn failed"
            result["e2fsck_exit"] = completed.returncode
            plan["executed_checks"] = executed_checks
            return

    if not e2fsck_seen:
        result["reason"] = "required e2fsck checks are missing"
        return

    replay_matrix = execute_replay_matrix(plan, job, result)
    if replay_matrix is None:
        return
    semantic_oracles = execute_semantic_oracles(plan, job, result)
    if semantic_oracles is None:
        return

    result_path = Path(require_string(result, "path"))
    if result_path.exists():
        raise FaultQemuExecutorError(f"refusing to overwrite result manifest: {result_path}")
    manifest = {
        "schema": "tx.ext4.fault_job_result.v1",
        "campaign_plan_sha256": plan.get("campaign_plan_sha256"),
        "case": require_string(job, "case"),
        "cut": require_string(job, "cut"),
        "iteration": require_positive_int(job, "iteration"),
        "hard_kill_observed": True,
        "replay_attempted": True,
        "e2fsck_exit": 0,
        "e2fsck_checks": executed_checks,
    }
    if job.get("replay_matrix") is not None:
        manifest["replay_matrix"] = replay_matrix
    if job.get("semantic_oracles") is not None:
        manifest["semantic_oracles"] = semantic_oracles
    result_path.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    plan["executed_checks"] = executed_checks
    result["status"] = "written"
    result["reason"] = "shell-test hard-kill observation, replay, e2fsck, and semantic oracles succeeded"
    result["e2fsck_exit"] = 0


def execute_semantic_oracles(
    plan: dict[str, Any], job: dict[str, Any], result: dict[str, Any]
) -> list[dict[str, Any]] | None:
    entries = job.get("semantic_oracles")
    if entries is None:
        return []
    if not isinstance(entries, list) or not entries:
        result["reason"] = "semantic oracles are invalid"
        return None
    resolved_command = semantic_oracle_command(result)
    if resolved_command is None:
        return None
    command, _ = resolved_command
    runner = require_object(plan, "runner")
    cwd = require_string(runner, "cwd")
    replay = Path(require_string(job, "replay_image"))
    prepared_images: set[Path] = set()
    observations: list[dict[str, Any]] = []
    for oracle_index, entry in enumerate(entries, start=1):
        if not isinstance(entry, dict):
            result["reason"] = "semantic oracle entry is invalid"
            return None
        try:
            entry_id = require_string(entry, "id")
            image = Path(require_string(entry, "image"))
            log = Path(require_string(entry, "log"))
        except FaultQemuExecutorError:
            result["reason"] = "semantic oracle entry is invalid"
            return None
        expected = entry.get("expected")
        if not isinstance(expected, dict):
            result["reason"] = "semantic oracle entry is missing expected"
            return None
        if image == replay:
            result["reason"] = "semantic oracle image must be a separate copy"
            return None
        if image in prepared_images or image.exists():
            result["reason"] = f"refusing to overwrite semantic oracle image: {image}"
            return None
        if log.exists():
            result["reason"] = f"refusing to overwrite semantic oracle log: {log}"
            return None
        request_path = image.parent / f"semantic-oracle-{oracle_index:02d}-request.json"
        if request_path.exists():
            result["reason"] = f"refusing to overwrite semantic oracle request: {request_path}"
            return None
        image.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(replay, image)
        request_path.write_text(
            json.dumps(
                {
                    "schema": "tx.ext4.fault_semantic_oracle_request.v1",
                    "id": entry_id,
                    "image": str(image),
                    "log": str(log),
                    "expected": expected,
                },
                indent=2,
                sort_keys=True,
            )
            + "\n",
            encoding="utf-8",
        )
        try:
            completed = subprocess.run(
                command + [str(request_path)],
                cwd=cwd,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                check=False,
            )
        except OSError as err:
            result["reason"] = f"semantic oracle command launch failed: {err}"
            return None
        if completed.returncode != 0:
            result["reason"] = f"semantic oracle entry failed: {entry_id}"
            return None
        if not log.is_file():
            result["reason"] = f"semantic oracle entry did not write log: {entry_id}"
            return None
        observation = dict(entry)
        observation["log_sha256"] = hashlib.sha256(log.read_bytes()).hexdigest()
        observation["image_sha256"] = hashlib.sha256(image.read_bytes()).hexdigest()
        observation["exit_code"] = 0
        observations.append(observation)
        prepared_images.add(image)
    return observations


def execute_replay_matrix(
    plan: dict[str, Any], job: dict[str, Any], result: dict[str, Any]
) -> list[dict[str, Any]] | None:
    entries = job.get("replay_matrix")
    if entries is None:
        return []
    if not isinstance(entries, list) or not entries:
        result["reason"] = "replay/remount matrix is invalid"
        return None

    runner = require_object(plan, "runner")
    cwd = require_string(runner, "cwd")
    replay = Path(require_string(job, "replay_image"))
    prepared_images: set[Path] = set()
    for entry in entries:
        if not isinstance(entry, dict):
            result["reason"] = "replay/remount matrix entry is invalid"
            return None
        image = Path(require_string(entry, "image"))
        if image == replay:
            result["reason"] = "replay/remount matrix image must be a separate copy"
            return None
        if image in prepared_images:
            continue
        if image.exists():
            result["reason"] = f"refusing to overwrite replay matrix image: {image}"
            return None
        image.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(replay, image)
        prepared_images.add(image)

    executed: list[dict[str, Any]] = []
    for entry in entries:
        assert isinstance(entry, dict)
        entry_id = require_string(entry, "id")
        image = Path(require_string(entry, "image"))
        log = Path(require_string(entry, "log"))
        if entry_id == "linux-rw-replay":
            command = matrix_command("TX_EXT4_FAULT_LINUX_REPLAY_COMMAND", result)
            if command is None:
                return None
            observation = run_matrix_command(command + [str(image)], entry, log, cwd, result)
        elif entry_id == "linux-post-replay-e2fsck":
            args = entry.get("args")
            if args != ["-fn", str(image)]:
                result["reason"] = "post-replay e2fsck must run -fn against the Linux replay image"
                return None
            observation = run_matrix_command(e2fsck_command() + args, entry, log, cwd, result)
        elif entry_id == "tx-remount":
            command = matrix_command("TX_EXT4_FAULT_TX_REMOUNT_COMMAND", result)
            if command is None:
                return None
            observation = run_matrix_command(
                command
                + [require_string(job, "case"), require_string(job, "cut"), str(image)],
                entry,
                log,
                cwd,
                result,
            )
        else:
            result["reason"] = f"unsupported replay/remount matrix entry: {entry_id}"
            return None
        if observation is None:
            return None
        executed.append(observation)
    return executed


def preflight_replay_matrix(plan: dict[str, Any]) -> bool:
    job = require_object(plan, "job")
    entries = job.get("replay_matrix")
    preflight = plan.get("replay_matrix_preflight")
    if not isinstance(preflight, dict):
        preflight = {}
        plan["replay_matrix_preflight"] = preflight
    if entries is None:
        preflight["status"] = "not-required"
        return True
    result = plan.get("result")
    if not isinstance(result, dict):
        result = require_result_object(plan, Path("executor-plan.json"))
    if not isinstance(entries, list) or not entries:
        result["reason"] = "replay/remount matrix is invalid"
        preflight["status"] = "blocked"
        preflight["reason"] = result["reason"]
        return False

    for entry in entries:
        if not isinstance(entry, dict):
            result["reason"] = "replay/remount matrix entry is invalid"
            preflight["status"] = "blocked"
            preflight["reason"] = result["reason"]
            return False
        entry_id = require_string(entry, "id")
        if entry_id == "linux-rw-replay":
            command = matrix_command("TX_EXT4_FAULT_LINUX_REPLAY_COMMAND", result)
        elif entry_id == "linux-post-replay-e2fsck":
            command = e2fsck_command()
        elif entry_id == "tx-remount":
            command = matrix_command("TX_EXT4_FAULT_TX_REMOUNT_COMMAND", result)
        else:
            result["reason"] = f"unsupported replay/remount matrix entry: {entry_id}"
            preflight["status"] = "blocked"
            preflight["reason"] = result["reason"]
            return False
        if command is None:
            preflight["status"] = "blocked"
            preflight["reason"] = result["reason"]
            return False
        if not command_available(command):
            result["reason"] = f"replay/remount command is not executable: {command[0]}"
            preflight["status"] = "blocked"
            preflight["reason"] = result["reason"]
            return False

    preflight["status"] = "ready"
    return True


def preflight_semantic_oracles(plan: dict[str, Any]) -> bool:
    job = require_object(plan, "job")
    entries = job.get("semantic_oracles")
    preflight = plan.get("semantic_oracle_preflight")
    if not isinstance(preflight, dict):
        preflight = {}
        plan["semantic_oracle_preflight"] = preflight
    if entries is None:
        preflight["status"] = "not-required"
        return True
    result = require_result_object(plan, Path("executor-plan.json"))
    if not isinstance(entries, list) or not entries:
        result["reason"] = "semantic oracles are invalid"
        preflight["status"] = "blocked"
        preflight["reason"] = result["reason"]
        return False
    resolved_command = semantic_oracle_command(result)
    if resolved_command is None:
        preflight["status"] = "blocked"
        preflight["reason"] = result["reason"]
        return False
    command, command_source = resolved_command
    if not command_available(command):
        result["reason"] = f"semantic oracle command is not executable: {command[0]}"
        preflight["status"] = "blocked"
        preflight["reason"] = result["reason"]
        return False
    for entry in entries:
        if not isinstance(entry, dict) or not isinstance(entry.get("expected"), dict):
            result["reason"] = "semantic oracle entry is invalid"
            preflight["status"] = "blocked"
            preflight["reason"] = result["reason"]
            return False
    preflight["status"] = "ready"
    preflight["command_source"] = command_source
    preflight["command"] = command
    return True


def command_available(command: list[str]) -> bool:
    executable = command[0]
    if "/" in executable:
        path = Path(executable)
        return path.is_file() and os.access(path, os.X_OK)
    return shutil.which(executable) is not None


def matrix_command(environment: str, result: dict[str, Any]) -> list[str] | None:
    if environment in os.environ:
        result["reason"] = f"{environment} is not allowed; replay/remount runners are repository-owned"
        return None
    if environment == "TX_EXT4_FAULT_LINUX_REPLAY_COMMAND":
        return repository_linux_rw_replay_command(result)
    if environment == "TX_EXT4_FAULT_TX_REMOUNT_COMMAND":
        return repository_tx_remount_command(result)
    result["reason"] = f"unsupported replay/remount command environment: {environment}"
    return None


def repository_linux_rw_replay_command(result: dict[str, Any]) -> list[str] | None:
    path = repository_linux_rw_replay_path()
    if not path.is_file():
        result["reason"] = f"repository Linux RW replay command is missing: {path}"
        return None
    if not os.access(path, os.X_OK):
        result["reason"] = f"repository Linux RW replay command is not executable: {path}"
        return None
    if platform.system() != "Linux":
        result["reason"] = "Linux RW replay requires a Linux host"
        return None
    if os.geteuid() != 0:
        result["reason"] = "Linux RW replay requires root for a loop mount"
        return None
    missing = [tool for tool in ("mount", "umount", "e2fsck") if shutil.which(tool) is None]
    if missing:
        result["reason"] = "Linux RW replay requires tools: " + ", ".join(missing)
        return None
    return [str(path)]


def repository_tx_remount_command(result: dict[str, Any]) -> list[str] | None:
    path = repository_tx_remount_path()
    if not path.is_file():
        result["reason"] = f"repository Tx remount command is missing: {path}"
        return None
    if not os.access(path, os.X_OK):
        result["reason"] = f"repository Tx remount command is not executable: {path}"
        return None
    return [str(path)]


def repository_linux_rw_replay_path() -> Path:
    return Path(__file__).resolve().with_name("fault_linux_rw_replay.py")


def repository_tx_remount_path() -> Path:
    return Path(__file__).resolve().with_name("fault_tx_remount.py")


def semantic_oracle_command(result: dict[str, Any]) -> tuple[list[str], str] | None:
    override = os.environ.get("TX_EXT4_FAULT_SEMANTIC_ORACLE_COMMAND")
    if override is not None:
        command = shlex.split(override)
        if not command:
            result["reason"] = "empty TX_EXT4_FAULT_SEMANTIC_ORACLE_COMMAND"
            return None
        return command, "environment-override"
    path = repository_semantic_oracle_path()
    if not path.is_file():
        result["reason"] = f"repository semantic oracle command is missing: {path}"
        return None
    if not os.access(path, os.X_OK):
        result["reason"] = f"repository semantic oracle command is not executable: {path}"
        return None
    return [str(path)], "repository-default"


def repository_semantic_oracle_path() -> Path:
    return Path(__file__).resolve().with_name("fault_semantic_oracle.py")


def run_matrix_command(
    command: list[str], entry: dict[str, Any], log: Path, cwd: str, result: dict[str, Any]
) -> dict[str, Any] | None:
    try:
        completed = subprocess.run(
            command,
            cwd=cwd,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            check=False,
        )
    except OSError as err:
        result["reason"] = f"replay/remount command launch failed: {err}"
        return None
    log.parent.mkdir(parents=True, exist_ok=True)
    log.write_text(completed.stdout, encoding="utf-8")
    observation = dict(entry)
    observation["log_sha256"] = hashlib.sha256(completed.stdout.encode("utf-8")).hexdigest()
    observation["exit_code"] = completed.returncode
    image = Path(require_string(entry, "image"))
    if not image.is_file():
        result["reason"] = f"replay/remount matrix image is missing after run: {image}"
        return None
    observation["image_sha256"] = hashlib.sha256(image.read_bytes()).hexdigest()
    if completed.returncode != 0:
        result["reason"] = f"replay/remount matrix entry failed: {entry.get('id')}"
        return None
    return observation


def require_result_object(plan: dict[str, Any], plan_path: Path) -> dict[str, Any]:
    result = plan.get("result")
    if isinstance(result, dict):
        return result
    result = {"path": str(plan_path.parent / "result.json"), "status": "not-written"}
    plan["result"] = result
    return result


def e2fsck_command() -> list[str]:
    override = os.environ.get("TX_EXT4_FAULT_E2FSCK_COMMAND")
    if override is not None:
        command = shlex.split(override)
        if not command:
            raise FaultQemuExecutorError("empty TX_EXT4_FAULT_E2FSCK_COMMAND")
        return command
    path = shutil.which("e2fsck")
    if path:
        return [path]
    if HOMEBREW_E2FSCK.is_file() and os.access(HOMEBREW_E2FSCK, os.X_OK):
        return [str(HOMEBREW_E2FSCK)]
    raise FaultQemuExecutorError(
        "no usable e2fsck command on PATH or Homebrew e2fsprogs prefix"
    )


def write_plan(path: Path, plan: dict[str, Any]) -> None:
    path.write_text(json.dumps(plan, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def require_script_contains_cut_marker(script: Path, cut_marker: str) -> None:
    if not script.is_file():
        raise FaultQemuExecutorError(f"missing shell-test script: {script}")
    try:
        text = script.read_text(encoding="utf-8")
    except UnicodeDecodeError as err:
        raise FaultQemuExecutorError(f"invalid shell-test script {script}: {err}") from err
    if cut_marker not in text:
        raise FaultQemuExecutorError(
            f"missing cut marker {cut_marker!r} in shell-test script {script}"
        )


def refuse_stale_result_manifest(job_dir: Path) -> None:
    result_path = job_dir / "result.json"
    if result_path.exists():
        raise FaultQemuExecutorError(
            f"stale result manifest exists at {result_path}; refusing to prepare a new run"
        )


def stage_role_images(job_dir: Path, role_images: dict[str, Any]) -> dict[str, str]:
    roles_dir = job_dir / "roles"
    roles_dir.mkdir(parents=True, exist_ok=True)
    staged: dict[str, str] = {}
    for role in ("test", "scratch", "workload"):
        source = Path(require_string(role_images, role))
        if not source.is_file():
            raise FaultQemuExecutorError(f"missing {role.upper()} role image: {source}")
        target = roles_dir / f"{role}.img"
        shutil.copyfile(source, target)
        staged[role] = str(target)
    return staged


if __name__ == "__main__":
    raise SystemExit(main())
