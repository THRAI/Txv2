from __future__ import annotations

import json
import subprocess
import time
from concurrent.futures import ThreadPoolExecutor, as_completed
from dataclasses import dataclass
from pathlib import Path
from typing import Any, TypedDict

from langgraph.graph import END, StateGraph

from .codex_exec import run_codex_worker, write_dry_run_prompt
from .leases import validate_worktree_leases
from .progress import WorktreeRecord, get_git_worktrees, load_worktree_records
from .prompts import build_worker_prompt


@dataclass(frozen=True)
class RunConfig:
    repo_root: Path
    worktree_ids: list[str] | None
    mode: str
    task: str | None
    max_workers: int
    dry_run: bool
    codex_bin: str
    output_dir: Path
    run_id: str | None = None
    progress_validate: bool = True


class RunnerState(TypedDict, total=False):
    config: RunConfig
    records: list[WorktreeRecord]
    lease_records: list[WorktreeRecord]
    errors: list[str]
    prompts: dict[str, str]
    run_dir: Path
    workers: list[dict[str, Any]]
    progress_validation: dict[str, Any]
    summary: dict[str, Any]


def run_agent_graph(config: RunConfig) -> dict[str, Any]:
    graph = _build_graph()
    state = graph.invoke({"config": config})
    return state["summary"]


def _build_graph():
    builder = StateGraph(RunnerState)
    builder.add_node("load", _load_records)
    builder.add_node("validate", _validate_records)
    builder.add_node("prepare", _prepare_run)
    builder.add_node("dispatch", _dispatch_workers)
    builder.add_node("verify", _verify_progress)
    builder.add_node("summarize", _summarize)
    builder.set_entry_point("load")
    builder.add_edge("load", "validate")
    builder.add_edge("validate", "prepare")
    builder.add_edge("prepare", "dispatch")
    builder.add_edge("dispatch", "verify")
    builder.add_edge("verify", "summarize")
    builder.add_edge("summarize", END)
    return builder.compile()


def _load_records(state: RunnerState) -> RunnerState:
    config = state["config"]
    active_records = load_worktree_records(config.repo_root, active_only=True)
    if config.worktree_ids:
        wanted = set(config.worktree_ids)
        records = [record for record in active_records if record.id in wanted]
        missing = sorted(wanted - {record.id for record in records})
        if missing:
            raise ValueError(f"missing worktree id(s): {', '.join(missing)}")
    else:
        records = active_records
    return {"records": records, "lease_records": active_records}


def _validate_records(state: RunnerState) -> RunnerState:
    config = state["config"]
    records = state["records"]
    try:
        git_worktrees = get_git_worktrees(config.repo_root)
        errors = validate_worktree_leases(state["lease_records"], git_worktrees)
    except Exception as exc:  # pragma: no cover - defensive reporting path
        errors = [str(exc)]
    return {"errors": errors}


def _prepare_run(state: RunnerState) -> RunnerState:
    config = state["config"]
    if state.get("errors"):
        return {}
    run_id = config.run_id or time.strftime("%Y%m%d-%H%M%S")
    run_dir = config.output_dir / run_id
    run_dir.mkdir(parents=True, exist_ok=True)
    prompts = {
        record.id: build_worker_prompt(record, mode=config.mode, task=config.task)
        for record in state["records"]
    }
    _write_json(run_dir / "state.json", _state_snapshot(config, state["records"]))
    return {"run_dir": run_dir, "prompts": prompts}


def _dispatch_workers(state: RunnerState) -> RunnerState:
    config = state["config"]
    if state.get("errors"):
        return {"workers": []}
    records = state["records"]
    prompts = state["prompts"]
    run_dir = state["run_dir"]
    if config.dry_run:
        workers = [
            write_dry_run_prompt(record, prompts[record.id], run_dir)
            for record in records
        ]
        return {"workers": workers}

    sandbox = "read-only" if config.mode == "status" else "workspace-write"
    workers: list[dict[str, Any]] = []
    with ThreadPoolExecutor(max_workers=max(1, config.max_workers)) as executor:
        futures = [
            executor.submit(
                run_codex_worker,
                record=record,
                prompt=prompts[record.id],
                run_dir=run_dir,
                codex_bin=config.codex_bin,
                sandbox=sandbox,
            )
            for record in records
        ]
        for future in as_completed(futures):
            workers.append(future.result().to_summary())
    workers.sort(key=lambda item: item["id"])
    return {"workers": workers}


def _verify_progress(state: RunnerState) -> RunnerState:
    config = state["config"]
    if state.get("errors") or config.dry_run or not config.progress_validate:
        return {"progress_validation": {"status": "skipped", "returncode": None}}
    result = subprocess.run(
        ["cargo", "xtask", "progress", "validate"],
        cwd=config.repo_root,
        text=True,
        capture_output=True,
        check=False,
    )
    return {
        "progress_validation": {
            "status": "passed" if result.returncode == 0 else "failed",
            "returncode": result.returncode,
            "stdout": result.stdout,
            "stderr": result.stderr,
        }
    }


def _summarize(state: RunnerState) -> RunnerState:
    config = state["config"]
    errors = state.get("errors", [])
    workers = state.get("workers", [])
    progress_validation = state.get("progress_validation", {})
    if errors:
        aggregate = "blocked"
    elif not workers:
        aggregate = "empty"
    elif all(worker["status"] == "dry-run" for worker in workers):
        aggregate = "dry-run"
    elif all(worker["status"] == "complete" for worker in workers) and progress_validation.get(
        "status", "passed"
    ) in {"passed", "skipped"}:
        aggregate = "complete"
    else:
        aggregate = "blocked"

    summary = {
        "run_id": (state.get("run_dir") or Path("")).name,
        "mode": config.mode,
        "dry_run": config.dry_run,
        "max_workers": config.max_workers,
        "aggregate_status": aggregate,
        "errors": errors,
        "workers": workers,
        "progress_validation": progress_validation,
    }
    run_dir = state.get("run_dir")
    if run_dir:
        _write_json(run_dir / "summary.json", summary)
    return {"summary": summary}


def _state_snapshot(config: RunConfig, records: list[WorktreeRecord]) -> dict[str, Any]:
    return {
        "repo_root": str(config.repo_root),
        "mode": config.mode,
        "task": config.task,
        "max_workers": config.max_workers,
        "dry_run": config.dry_run,
        "codex_bin": config.codex_bin,
        "records": [record.to_summary() for record in records],
    }


def _write_json(path: Path, value: dict[str, Any]) -> None:
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")
