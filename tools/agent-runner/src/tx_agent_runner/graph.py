from __future__ import annotations

import json
import subprocess
import time
import uuid
from concurrent.futures import ThreadPoolExecutor, as_completed
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable, Sequence, TypedDict

from langgraph.checkpoint.memory import MemorySaver
from langgraph.graph import END, StateGraph
from langgraph.types import Command, Send, interrupt

from .codex_exec import run_codex_worker, write_dry_run_prompt
from .leases import validate_worktree_leases
from .progress import WorktreeRecord, get_git_worktrees, load_worktree_records
from .prompts import build_worker_prompt

# ---------------------------------------------------------------------------
# Config
# ---------------------------------------------------------------------------


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
    # New flags for multi-stage orchestration
    with_research: bool = False
    with_plan: bool = False
    approval: str = "auto"  # "auto" | "manual"


# ---------------------------------------------------------------------------
# State
# ---------------------------------------------------------------------------


class RunnerState(TypedDict, total=False):
    config: RunConfig
    records: list[WorktreeRecord]
    lease_records: list[WorktreeRecord]
    errors: list[str]
    prompts: dict[str, str]
    run_dir: Path
    workers: list[dict[str, Any]]
    # Research subgraph — per-worker channels (merged by synthesize)
    locate_findings: dict[str, Any]
    analyze_findings: dict[str, Any]
    pattern_findings: dict[str, Any]
    research_findings: dict[str, Any]
    # Plan output
    plan: dict[str, Any]
    progress_validation: dict[str, Any]
    summary: dict[str, Any]


# ---------------------------------------------------------------------------
# Public entry points
# ---------------------------------------------------------------------------


def run_agent_graph(
    config: RunConfig,
    *,
    thread_id: str | None = None,
    stream_callback: Callable[[str, str, dict[str, Any]], None] | None = None,
) -> dict[str, Any]:
    """Run the agent graph and return the summary dict.

    Parameters
    ----------
    config:
        Run configuration (mode, workers, etc.).
    thread_id:
        Stable thread id for checkpointing. Auto-generated when None.
    stream_callback:
        When provided, the graph is streamed and ``callback(node_name, event_type, data)``
        is called for every stream event. ``event_type`` is one of ``"values"``,
        ``"updates"``, ``"checkpoints"``.
    """
    graph = _build_graph()
    run_config = {
        "configurable": {"thread_id": thread_id or uuid.uuid4().hex[:12]},
        "recursion_limit": 50,
    }

    if stream_callback:
        final_state = _stream_graph(graph, config, run_config, stream_callback)
    else:
        final_state = graph.invoke({"config": config}, run_config)

    return final_state.get("summary", {})


def _stream_graph(
    graph,
    config: RunConfig,
    run_config: dict[str, Any],
    callback: Callable[[str, str, dict[str, Any]], None],
) -> dict[str, Any]:
    """Stream graph events to callback, returning final state."""
    last_state: dict[str, Any] = {}
    for event in graph.stream(
        {"config": config},
        run_config,
        stream_mode=["updates", "values"],
        subgraphs=True,
    ):
        for stream_mode, payload in event:
            if stream_mode == "updates":
                for node_name, node_output in payload.items():
                    callback(node_name, "updates", node_output)
            elif stream_mode == "values":
                callback("__values__", "values", payload)
                last_state = payload
    return last_state


# ---------------------------------------------------------------------------
# Graph construction
# ---------------------------------------------------------------------------


def _build_graph():
    builder = StateGraph(RunnerState)

    # ---- nodes ----
    builder.add_node("load", _load_records)
    builder.add_node("validate", _validate_records)
    builder.add_node("research", _research_orchestrator)  # fan-out hub
    builder.add_node("locate", _locate_files)             # research worker
    builder.add_node("analyze", _analyze_code)            # research worker
    builder.add_node("find_patterns", _find_patterns)     # research worker
    builder.add_node("synthesize", _synthesize_research)  # research sink
    builder.add_node("plan", _generate_plan)
    builder.add_node("prepare", _prepare_run)
    builder.add_node("dispatch", _dispatch_workers)
    builder.add_node("verify", _verify_progress)
    builder.add_node("summarize", _summarize)

    # ---- topology ----
    builder.set_entry_point("load")
    builder.add_edge("load", "validate")

    # validate → prepare  or  validate → summarize (on error)
    builder.add_conditional_edges(
        "validate",
        _route_after_validate,
        {"ok": "research", "blocked": "summarize"},
    )

    # research (optional: skip to prepare or plan when with_research=False / with_plan=False)
    builder.add_conditional_edges(
        "research",
        _route_after_research,
        {"prepare": "prepare", "plan": "plan"},
    )
    # research workers converge to synthesize
    builder.add_edge("locate", "synthesize")
    builder.add_edge("analyze", "synthesize")
    builder.add_edge("find_patterns", "synthesize")
    builder.add_edge("synthesize", "research")  # loop back for routing decision

    # plan → prepare
    builder.add_edge("plan", "prepare")

    # prepare → dispatch
    builder.add_edge("prepare", "dispatch")

    # dispatch → verify
    builder.add_edge("dispatch", "verify")

    # verify → summarize
    builder.add_edge("verify", "summarize")
    builder.add_edge("summarize", END)

    return builder.compile(checkpointer=MemorySaver())


# ---------------------------------------------------------------------------
# Routing helpers
# ---------------------------------------------------------------------------


def _route_after_validate(state: RunnerState) -> str:
    if state.get("errors"):
        return "blocked"
    return "ok"


def _route_after_research(state: RunnerState) -> str:
    """Decide next step after research (or skip research entirely)."""
    config = state["config"]
    if config.with_plan:
        return "plan"
    return "prepare"


# ---------------------------------------------------------------------------
# Node: load
# ---------------------------------------------------------------------------


def _load_records(state: RunnerState) -> RunnerState:
    config = state["config"]
    active_records = load_worktree_records(config.repo_root, active_only=True)
    if config.worktree_ids:
        wanted = set(config.worktree_ids)
        records = [r for r in active_records if r.id in wanted]
        missing = sorted(wanted - {r.id for r in records})
        if missing:
            raise ValueError(f"missing worktree id(s): {', '.join(missing)}")
    else:
        records = active_records
    return {"records": records, "lease_records": active_records}


# ---------------------------------------------------------------------------
# Node: validate
# ---------------------------------------------------------------------------


def _validate_records(state: RunnerState) -> RunnerState:
    config = state["config"]
    records = state["records"]
    try:
        git_worktrees = get_git_worktrees(config.repo_root)
        errors = validate_worktree_leases(state["lease_records"], git_worktrees)
    except Exception as exc:  # pragma: no cover - defensive reporting path
        errors = [str(exc)]
    return {"errors": errors}


# ---------------------------------------------------------------------------
# Research subgraph
# ---------------------------------------------------------------------------


def _research_orchestrator(state: RunnerState) -> dict[str, Any] | list[Send]:
    """Fan-out to parallel research workers or skip to next stage."""
    config = state["config"]
    if not config.with_research:
        # Nothing to do — router will skip to prepare/plan
        return {}

    # If we already synthesized, don't re-run
    if state.get("research_findings"):
        return {}

    task = config.task or "Investigate the codebase"
    return [
        Send("locate", {"task": task, "repo_root": str(config.repo_root)}),
        Send("analyze", {"task": task, "repo_root": str(config.repo_root)}),
        Send("find_patterns", {"task": task, "repo_root": str(config.repo_root)}),
    ]


def _locate_files(state: dict[str, Any]) -> dict[str, Any]:
    """HumanLayer locator role: find relevant files."""
    task = state.get("task", "")
    repo_root = Path(state.get("repo_root", "."))
    # Stub: real impl spawns a codebase-locator agent.  Writes to own channel
    # so parallel workers don't clobber each other.
    return {"locate_findings": {"task": task, "files": [], "status": "stub"}}


def _analyze_code(state: dict[str, Any]) -> dict[str, Any]:
    """HumanLayer analyzer role: explain current behavior."""
    task = state.get("task", "")
    return {"analyze_findings": {"task": task, "behavior": "", "status": "stub"}}


def _find_patterns(state: dict[str, Any]) -> dict[str, Any]:
    """HumanLayer pattern-finder role: find existing conventions."""
    task = state.get("task", "")
    return {"pattern_findings": {"task": task, "conventions": [], "status": "stub"}}


def _synthesize_research(state: RunnerState) -> RunnerState:
    """Merge parallel research findings into a single dict."""
    findings = {
        "locate": state.get("locate_findings", {}),
        "analyze": state.get("analyze_findings", {}),
        "patterns": state.get("pattern_findings", {}),
    }
    return {"research_findings": findings}


# ---------------------------------------------------------------------------
# Node: plan
# ---------------------------------------------------------------------------


def _generate_plan(state: RunnerState) -> RunnerState:
    """Generate an implementation plan from research findings.

    Stub: real impl would use the research_findings + task to produce a plan JSON.
    """
    config = state["config"]
    plan = {
        "task": config.task,
        "phases": [],
        "research": state.get("research_findings", {}),
    }
    return {"plan": plan}


# ---------------------------------------------------------------------------
# Node: prepare
# ---------------------------------------------------------------------------


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


# ---------------------------------------------------------------------------
# Node: dispatch
# ---------------------------------------------------------------------------


def _dispatch_workers(state: RunnerState) -> RunnerState:
    config = state["config"]
    if state.get("errors"):
        return {"workers": []}

    # Human-in-the-loop gate: pause before dispatch when approval == "manual"
    if config.approval == "manual":
        decision = _request_approval(state)
        if decision.get("action") == "abort":
            return {
                "workers": [],
                "errors": state.get("errors", []) + ["aborted by user before dispatch"],
            }

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


def _request_approval(state: RunnerState) -> dict[str, Any]:
    """Call LangGraph ``interrupt()`` to pause execution and wait for human input.

    The caller resumes with ``Command(resume={"action": "approve"})`` or
    ``Command(resume={"action": "abort"})``.
    """
    records = state["records"]
    prompts = state["prompts"]
    summary_lines = [f"- {r.id}: {r.branch}  ({len(r.write_scope)} scope entries)" for r in records]
    return interrupt({
        "stage": "dispatch",
        "mode": state["config"].mode,
        "workers": [r.id for r in records],
        "prompts_preview": {rid: p[:200] for rid, p in prompts.items()},
        "summary": "\n".join(summary_lines),
    })


# ---------------------------------------------------------------------------
# Node: verify
# ---------------------------------------------------------------------------


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


# ---------------------------------------------------------------------------
# Node: summarize
# ---------------------------------------------------------------------------


def _summarize(state: RunnerState) -> RunnerState:
    config = state["config"]
    errors = state.get("errors", [])
    workers = state.get("workers", [])
    progress_validation = state.get("progress_validation", {})
    if errors:
        aggregate = "blocked"
    elif not workers:
        aggregate = "empty"
    elif all(w["status"] == "dry-run" for w in workers):
        aggregate = "dry-run"
    elif all(w["status"] == "complete" for w in workers) and progress_validation.get(
        "status", "passed"
    ) in {"passed", "skipped"}:
        aggregate = "complete"
    else:
        aggregate = "blocked"

    summary = {
        "run_id": str((state.get("run_dir") or Path(".")).name),
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


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _state_snapshot(config: RunConfig, records: list[WorktreeRecord]) -> dict[str, Any]:
    return {
        "repo_root": str(config.repo_root),
        "mode": config.mode,
        "task": config.task,
        "max_workers": config.max_workers,
        "dry_run": config.dry_run,
        "codex_bin": config.codex_bin,
        "records": [r.to_summary() for r in records],
    }


def _write_json(path: Path, value: dict[str, Any]) -> None:
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")
