#!/usr/bin/env python3
"""Fail-closed QEMU executor scaffold for ext4 fault campaign jobs.

This script converts a
tx.ext4.fault_job_request.v1 into a per-job execution plan, stages private role
image copies, runs the planned shell-test until its cut marker, and preserves
crash/replay image copies. It writes a result manifest only after the default
shell-test path observes its hard-kill contract, every required e2fsck -fn
check succeeds, and declared replay-matrix commands complete. Linux RW replay
and Tx remount always resolve to repository-owned runners; the Linux runner
fail-closes before the primary run unless its own preflight can provide a real
Linux root loop mount directly or through privileged Docker. Declared semantic oracles default to the repository-owned
fault_semantic_oracle.py runner. Set TX_EXT4_FAULT_PLAN_ONLY=1 for plan-only
preflight.
"""

from __future__ import annotations

import hashlib
import json
import os
import shlex
import shutil
import subprocess
import sys
from pathlib import Path
from typing import Any


class FaultQemuExecutorError(Exception):
    pass


HOMEBREW_E2FSCK = Path("/opt/homebrew/opt/e2fsprogs/sbin/e2fsck")
SHELL_TEST_MAX_ATTEMPTS = 3


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


def copy_image_cow(source: Path, target: Path) -> None:
    if not source.is_file():
        raise FaultQemuExecutorError(f"missing image source: {source}")
    if target.exists():
        raise FaultQemuExecutorError(f"refusing to overwrite existing image clone target: {target}")
    target.parent.mkdir(parents=True, exist_ok=True)
    if sys.platform.startswith("linux"):
        command = ["cp", "--reflink=always", str(source), str(target)]
    elif sys.platform == "darwin":
        command = ["cp", "-c", str(source), str(target)]
    else:
        raise FaultQemuExecutorError(
            f"CoW image clone is required for Tier 1 image staging on this host OS: {sys.platform}"
        )
    try:
        completed = subprocess.run(
            command,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            check=False,
        )
    except OSError as err:
        raise FaultQemuExecutorError(f"failed to launch CoW image clone command: {err}") from err
    if completed.returncode != 0:
        detail = (completed.stderr or completed.stdout).strip()
        if detail:
            raise FaultQemuExecutorError(
                f"CoW image clone failed with exit {completed.returncode}: {detail}"
            )
        raise FaultQemuExecutorError(f"CoW image clone failed with exit {completed.returncode}")


def tail_text(text: str, max_lines: int = 80) -> str:
    lines = text.splitlines()
    return "\n".join(lines[-max_lines:])


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
        "role_images": {
            "test": require_string(role_images, "test"),
            "scratch": require_string(role_images, "scratch"),
            "workload": require_string(role_images, "workload"),
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
    marker = require_string(runner, "cut_marker")
    if command_source == "shell-test-command":
        run_shell_test_until_cut_with_retries(plan_path, plan, command, serial_log, marker)
    else:
        runner["command"] = command
        runner["command_source"] = command_source
        runner["status"] = "running"
        write_plan(plan_path, plan)
        process = spawn_runner_process(command, runner, serial_log, plan_path, plan)
        assert process.stdout is not None
        observed = False
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


def spawn_runner_process(
    command: list[str],
    runner: dict[str, Any],
    serial_log: Path,
    plan_path: Path,
    plan: dict[str, Any],
) -> subprocess.Popen[str]:
    env = os.environ.copy()
    env["TX_EXT4_FAULT_SERIAL_LOG"] = str(serial_log)
    try:
        return subprocess.Popen(
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


def run_shell_test_until_cut_with_retries(
    plan_path: Path,
    plan: dict[str, Any],
    command: list[str],
    serial_log: Path,
    marker: str,
) -> None:
    runner = require_object(plan, "runner")
    attempts: list[dict[str, Any]] = []
    stop_line = f"shell-test: stop needle observed: {marker}"
    last_error = f"shell-test did not confirm stop needle {marker!r}"
    for attempt in range(1, SHELL_TEST_MAX_ATTEMPTS + 1):
        if attempt > 1:
            restage_role_images_for_retry(plan_path.parent, plan)
            if serial_log.exists():
                serial_log.unlink()
        runner["command"] = command
        runner["command_source"] = "shell-test-command"
        runner["status"] = "running"
        runner["attempt"] = attempt
        write_plan(plan_path, plan)
        process = spawn_runner_process(command, runner, serial_log, plan_path, plan)
        output, _ = process.communicate()
        attempt_record = {
            "attempt": attempt,
            "exit_code": process.returncode,
            "output_tail": tail_text(output),
        }
        if process.returncode == 0 and stop_line in output:
            attempt_record["status"] = "exited-after-cut"
            attempts.append(attempt_record)
            runner["attempts"] = attempts
            runner["output_tail"] = tail_text(output)
            runner["exit_code"] = process.returncode
            runner["status"] = "exited-after-cut"
            return
        if process.returncode != 0:
            attempt_record["status"] = "shell-test-failed-before-cut"
            last_error = (
                f"shell-test exited with {process.returncode} before confirmed stop needle"
            )
        else:
            attempt_record["status"] = "cut-not-observed"
            last_error = f"shell-test did not confirm stop needle {marker!r}"
        attempts.append(attempt_record)
        runner["attempts"] = attempts
        runner["output_tail"] = tail_text(output)
        runner["exit_code"] = process.returncode
        runner["status"] = attempt_record["status"]
        if attempt == SHELL_TEST_MAX_ATTEMPTS or "role_images" not in plan:
            write_plan(plan_path, plan)
            raise FaultQemuExecutorError(last_error)
        runner["status"] = "retrying-before-cut"
        write_plan(plan_path, plan)


def restage_role_images_for_retry(job_dir: Path, plan: dict[str, Any]) -> None:
    staged = require_object(plan, "staged_role_images")
    for role in ("test", "scratch", "workload"):
        path = Path(require_string(staged, role))
        if path.exists():
            path.unlink()
    role_images = require_object(plan, "role_images")
    plan["staged_role_images"] = stage_role_images(job_dir, role_images)


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
        copy_image_cow(scratch, path)
    plan["preserved_images"] = {
        "crash": str(crash),
        "replay": str(replay),
        "source": str(scratch),
        "status": "copied-after-runner-termination",
    }
    record_and_remove_staged_role_images(plan)


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as file:
        for chunk in iter(lambda: file.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def record_and_remove_staged_role_images(plan: dict[str, Any]) -> None:
    staged = require_object(plan, "staged_role_images")
    digests: dict[str, str] = {}
    removed: list[str] = []
    for role in ("test", "scratch", "workload"):
        if role not in staged:
            continue
        path = Path(require_string(staged, role))
        if not path.is_file():
            raise FaultQemuExecutorError(
                f"missing staged {role} role image before retention cleanup: {path}"
            )
        digests[role] = sha256_file(path)
    for role in ("test", "scratch", "workload"):
        if role not in staged:
            continue
        path = Path(require_string(staged, role))
        try:
            path.unlink()
        except OSError as err:
            raise FaultQemuExecutorError(f"failed to remove staged {role} role image {path}: {err}") from err
        removed.append(str(path))
    plan["staged_role_image_digests"] = digests
    plan["staged_role_images_retention"] = {
        "status": "removed-after-digest-recorded",
        "removed": removed,
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

    if not validate_e2fsck_checks(checks, replay, result):
        return

    if not recover_replay_image(plan, job, replay, result):
        return

    executed_checks: list[dict[str, Any]] = []
    for check in checks:
        args = check.get("args")
        log_value = check.get("log")
        assert isinstance(args, list)
        assert isinstance(log_value, str)
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


def validate_e2fsck_checks(checks: list[Any], replay: Path, result: dict[str, Any]) -> bool:
    e2fsck_seen = False
    for check in checks:
        if not isinstance(check, dict) or check.get("tool") != "e2fsck":
            result["reason"] = "only declared e2fsck checks are supported"
            return False
        args = check.get("args")
        log_value = check.get("log")
        if (
            not isinstance(args, list)
            or not all(isinstance(arg, str) and arg for arg in args)
            or not isinstance(log_value, str)
            or not log_value
        ):
            result["reason"] = "e2fsck checks must declare -fn and a log path"
            return False
        if args != ["-fn", str(replay)]:
            result["reason"] = "e2fsck check must run -fn against the replay image"
            return False
        e2fsck_seen = True
    if not e2fsck_seen:
        result["reason"] = "required e2fsck checks are missing"
        return False
    return True


def recover_replay_image(
    plan: dict[str, Any], job: dict[str, Any], replay: Path, result: dict[str, Any]
) -> bool:
    command = matrix_command("TX_EXT4_FAULT_TX_REMOUNT_COMMAND", result)
    if command is None:
        return False
    runner = require_object(plan, "runner")
    cwd = require_string(runner, "cwd")
    log = replay.with_name("replay-recovery.log")
    if log.exists():
        result["reason"] = f"refusing to overwrite replay recovery log: {log}"
        return False
    full_command = command + [
        require_string(job, "case"),
        require_string(job, "cut"),
        str(replay),
    ]
    initial_image_sha256 = sha256_file(replay) if replay.is_file() else None
    crash_value = job.get("crash_image")
    crash = Path(crash_value) if isinstance(crash_value, str) and crash_value else None
    attempts: list[dict[str, Any]] = []
    completed: subprocess.CompletedProcess[str] | None = None
    for attempt in range(1, 4):
        try:
            completed = subprocess.run(
                full_command,
                cwd=cwd,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                check=False,
            )
        except OSError as err:
            result["reason"] = f"replay image recovery launch failed: {err}"
            return False
        log.write_text(completed.stdout, encoding="utf-8")
        if completed.returncode == 0:
            break
        failed_image_sha256 = sha256_file(replay) if replay.is_file() else None
        attempt_observation = {
            "attempt": attempt,
            "exit_code": completed.returncode,
            "image_sha256": failed_image_sha256,
            "log": str(preserve_failed_matrix_log(log, attempt)),
            "log_sha256": hashlib.sha256(completed.stdout.encode("utf-8")).hexdigest(),
            "reason": "replay image recovery failed",
        }
        if failed_image_sha256 != initial_image_sha256:
            attempt_observation["mutated_replay_image"] = True
            if not restore_replay_from_crash_image(crash, replay, attempt_observation, result):
                attempts.append(attempt_observation)
                break
            initial_image_sha256 = sha256_file(replay) if replay.is_file() else None
        attempts.append(attempt_observation)
        if attempt == 3:
            result["reason"] = "replay image recovery failed after retries"

    if completed is None:
        result["reason"] = "replay image recovery did not run"
        return False
    observation = {
        "command": full_command,
        "command_source": "repository-default",
        "attempt": len(attempts) + 1,
        "exit_code": completed.returncode,
        "image": str(replay),
        "image_sha256": sha256_file(replay) if replay.is_file() else None,
        "log": str(log),
        "log_sha256": hashlib.sha256(completed.stdout.encode("utf-8")).hexdigest(),
        "status": "ok" if completed.returncode == 0 else "failed",
    }
    if attempts:
        observation["attempts"] = attempts
    plan["replay_image_recovery"] = observation
    if completed.returncode != 0:
        result.setdefault("reason", "replay image recovery failed")
        return False
    return True


def restore_replay_from_crash_image(
    crash: Path | None,
    replay: Path,
    attempt_observation: dict[str, Any],
    result: dict[str, Any],
) -> bool:
    unavailable_reason = (
        "replay image recovery failed after mutating replay image "
        "and crash image restore is unavailable"
    )
    if crash is None or not crash.is_file():
        result["reason"] = unavailable_reason
        return False
    try:
        crash_sha256 = sha256_file(crash)
        replay.unlink(missing_ok=True)
        copy_image_cow(crash, replay)
        restored_sha256 = sha256_file(replay)
    except (FaultQemuExecutorError, OSError) as err:
        result["reason"] = f"{unavailable_reason}: {err}"
        return False
    attempt_observation["restored_from_crash_image"] = str(crash)
    attempt_observation["restored_from_crash_image_sha256"] = crash_sha256
    attempt_observation["restored_replay_image_sha256"] = restored_sha256
    return True


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
        copy_image_cow(replay, image)
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
            prepare_replay_matrix_image(replay, image, prepared_images)
            observation = run_matrix_command(command + [str(image)], entry, log, cwd, result)
        elif entry_id == "linux-post-replay-e2fsck":
            args = entry.get("args")
            if args != ["-fn", str(image)]:
                result["reason"] = "post-replay e2fsck must run -fn against the Linux replay image"
                return None
            prepare_replay_matrix_image(replay, image, prepared_images)
            observation = run_matrix_command(e2fsck_command() + args, entry, log, cwd, result)
        elif entry_id == "tx-remount":
            command = matrix_command("TX_EXT4_FAULT_TX_REMOUNT_COMMAND", result)
            if command is None:
                return None
            observation = run_tx_remount_matrix_command(
                command
                + [require_string(job, "case"), require_string(job, "cut"), str(image)],
                entry,
                replay,
                log,
                cwd,
                result,
                prepared_images,
            )
        else:
            result["reason"] = f"unsupported replay/remount matrix entry: {entry_id}"
            return None
        if observation is None:
            return None
        executed.append(observation)
    return executed


def prepare_replay_matrix_image(replay: Path, image: Path, prepared_images: set[Path]) -> None:
    if image in prepared_images:
        return
    copy_image_cow(replay, image)
    prepared_images.add(image)


def run_tx_remount_matrix_command(
    command: list[str],
    entry: dict[str, Any],
    replay: Path,
    log: Path,
    cwd: str,
    result: dict[str, Any],
    prepared_images: set[Path],
) -> dict[str, Any] | None:
    image = Path(require_string(entry, "image"))
    attempts = []
    for attempt in range(1, 4):
        prepare_replay_matrix_image(replay, image, prepared_images)
        observation = run_matrix_command(command, entry, log, cwd, result)
        if observation is not None:
            observation["attempt"] = attempt
            if attempts:
                observation["attempts"] = attempts
            return observation
        attempts.append(
            {
                "attempt": attempt,
                "reason": result.get("reason", "tx-remount failed"),
                "log": str(preserve_failed_matrix_log(log, attempt)),
            }
        )
        image.unlink(missing_ok=True)
        log.unlink(missing_ok=True)
        prepared_images.discard(image)
    result["reason"] = "replay/remount matrix entry failed after retries: tx-remount"
    result["tx_remount_attempts"] = attempts
    return None


def preserve_failed_matrix_log(log: Path, attempt: int) -> Path:
    if not log.is_file():
        return log
    attempt_log = log.with_name(f"{log.stem}.attempt-{attempt}{log.suffix}")
    log.replace(attempt_log)
    return attempt_log


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

    commands: list[dict[str, Any]] = []
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
        commands.append(
            {
                "id": entry_id,
                "command_source": replay_matrix_command_source(entry_id),
                "command": command,
            }
        )

    preflight["status"] = "ready"
    preflight["commands"] = commands
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


def replay_matrix_command_source(entry_id: str) -> str:
    if entry_id in ("linux-rw-replay", "tx-remount"):
        return "repository-default"
    if entry_id == "linux-post-replay-e2fsck":
        return "e2fsck"
    return "unknown"


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
    preflight = subprocess.run(
        [str(path), "--preflight"],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        check=False,
    )
    if preflight.returncode != 0:
        result["reason"] = linux_replay_preflight_error(preflight)
        return None
    return [str(path)]


def linux_replay_preflight_error(completed: subprocess.CompletedProcess[str]) -> str:
    text = completed.stderr.strip() or completed.stdout.strip()
    for line in reversed(text.splitlines()):
        line = line.strip()
        if line.startswith("error: "):
            return line.removeprefix("error: ")
        if line:
            return line
    return f"Linux RW replay preflight failed with exit {completed.returncode}"


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
        copy_image_cow(source, target)
        staged[role] = str(target)
    return staged


if __name__ == "__main__":
    raise SystemExit(main())
