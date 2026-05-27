from __future__ import annotations

import argparse
import json
import subprocess
import sys
from pathlib import Path
from typing import Any

from .graph import RunConfig, _build_graph, run_agent_graph
from .leases import validate_worktree_leases
from .progress import get_git_worktrees, load_worktree_records

try:
    from langgraph.errors import GraphInterrupt
except ImportError:  # pragma: no cover
    GraphInterrupt = Exception


def main(argv: list[str] | None = None) -> int:
    parser = _parser()
    args = parser.parse_args(argv)
    repo_root = _repo_root(Path(args.repo_root) if args.repo_root else Path.cwd())
    try:
        if args.command == "inspect":
            return _inspect(args, repo_root)
        if args.command == "run":
            return _run(args, repo_root)
        if args.command == "resume":
            return _resume(args)
        if args.command == "status":
            return _thread_status(args)
    except GraphInterrupt as exc:
        _print_interrupt_and_handoff(exc, args)
        return 10  # distinct exit code: interrupted, resumeable
    except Exception as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1
    parser.error("missing command")
    return 2


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="tx-agent-runner")
    parser.add_argument("--repo-root", help="Repository root. Defaults to git top-level.")
    subcommands = parser.add_subparsers(dest="command")

    inspect = subcommands.add_parser("inspect", help="Inspect known worker lanes")
    inspect.add_argument("--worktrees", choices=["active"], required=True)
    inspect.add_argument("--json", action="store_true", help="Print JSON")

    run = subcommands.add_parser("run", help="Run Codex workers")
    selector = run.add_mutually_exclusive_group(required=True)
    selector.add_argument("--worktrees", choices=["active"])
    selector.add_argument("--worktree-id", action="append", dest="worktree_ids")
    run.add_argument("--mode", choices=["status", "execute"], default="status")
    run.add_argument("--task")
    run.add_argument("--max-workers", type=int, default=2)
    run.add_argument("--dry-run", action="store_true")
    run.add_argument("--json", action="store_true", help="Print JSON")
    run.add_argument("--codex-bin", default="codex")
    run.add_argument("--output-dir", type=Path, default=Path("target/tx-agent-runs"))
    run.add_argument("--with-research", action="store_true",
                     help="Run research subgraph (locate + analyze + find patterns) before dispatch")
    run.add_argument("--with-plan", action="store_true",
                     help="Generate an implementation plan from research findings")
    run.add_argument("--approval", choices=["auto", "manual"], default="auto",
                     help="Dispatch approval mode: 'auto' runs immediately, 'manual' pauses for human review")
    run.add_argument("--stream", action="store_true",
                     help="Stream graph events to stdout in real time")
    run.add_argument("--thread-id",
                     help="Stable thread id for checkpointing (auto-generated if not set)")
    run.add_argument("--interactive", action="store_true",
                     help="Prompt on stdin at interrupt points instead of exiting")

    resume = subcommands.add_parser("resume", help="Resume an interrupted graph run")
    resume.add_argument("--thread-id", required=True,
                        help="Thread id of the interrupted run")
    resume.add_argument("--action", choices=["approve", "abort"], required=True,
                        help="Resume action: 'approve' continues, 'abort' cancels")
    resume.add_argument("--json", action="store_true", help="Print JSON")

    st = subcommands.add_parser("status", help="Show thread checkpoint state")
    st.add_argument("--thread-id", required=True,
                    help="Thread id to inspect")
    st.add_argument("--json", action="store_true", help="Print JSON")

    return parser


def _inspect(args: argparse.Namespace, repo_root: Path) -> int:
    records = load_worktree_records(repo_root, active_only=args.worktrees == "active")
    errors = validate_worktree_leases(records, get_git_worktrees(repo_root))
    payload = {
        "records": [record.to_summary() for record in records],
        "errors": errors,
    }
    if args.json:
        print(json.dumps(payload, indent=2, sort_keys=True))
    else:
        for record in records:
            print(
                f"{record.id} status={record.status} owner={record.owner} "
                f"branch={record.branch} path={record.path}"
            )
        for error in errors:
            print(f"error: {error}", file=sys.stderr)
    return 1 if errors else 0


def _run(args: argparse.Namespace, repo_root: Path) -> int:
    if args.mode == "execute" and not args.task:
        raise ValueError("execute mode requires --task")
    if args.max_workers < 1:
        raise ValueError("--max-workers must be at least 1")
    output_dir = args.output_dir
    if not output_dir.is_absolute():
        output_dir = repo_root / output_dir
    config = RunConfig(
        repo_root=repo_root,
        worktree_ids=args.worktree_ids,
        mode=args.mode,
        task=args.task,
        max_workers=args.max_workers,
        dry_run=args.dry_run,
        codex_bin=args.codex_bin,
        output_dir=output_dir,
        with_research=args.with_research,
        with_plan=args.with_plan,
        approval=args.approval,
    )

    if args.stream:
        summary = run_agent_graph(
            config,
            thread_id=args.thread_id,
            stream_callback=_make_stream_printer(),
        )
    else:
        summary = run_agent_graph(config, thread_id=args.thread_id)
    if args.json:
        print(json.dumps(summary, indent=2, sort_keys=True))
    else:
        print(f"run {summary['run_id']} status={summary['aggregate_status']}")
        for worker in summary["workers"]:
            print(f"{worker['id']} status={worker['status']}")
        for error in summary["errors"]:
            print(f"error: {error}", file=sys.stderr)
    return 0 if summary["aggregate_status"] in {"complete", "dry-run"} else 1


def _repo_root(start: Path) -> Path:
    result = subprocess.run(
        ["git", "rev-parse", "--show-toplevel"],
        cwd=start,
        text=True,
        capture_output=True,
        check=False,
    )
    if result.returncode == 0:
        return Path(result.stdout.strip())
    return start.resolve()


def _make_stream_printer():
    """Return a callback that prints graph stream events to stderr."""
    def _print(node: str, event_type: str, data: dict[str, Any]) -> None:
        if event_type == "updates":
            compact = {k: _summarize_value(v) for k, v in data.items()}
            print(f"[{node}] {compact}", file=sys.stderr)
    return _print


def _summarize_value(v: Any) -> Any:
    if isinstance(v, list):
        return f"list[{len(v)}]"
    if isinstance(v, dict):
        return f"dict[{len(v)}]"
    if isinstance(v, str) and len(v) > 60:
        return v[:57] + "..."
    return v


# ---------------------------------------------------------------------------
# resume / status / interrupt handling
# ---------------------------------------------------------------------------


def _resume(args: argparse.Namespace) -> int:
    """Resume an interrupted graph run."""
    graph = _build_graph()
    run_config = {"configurable": {"thread_id": args.thread_id}}

    from langgraph.types import Command
    result = graph.invoke(Command(resume={"action": args.action}), run_config)
    summary = result.get("summary", {})

    if args.json:
        print(json.dumps(summary, indent=2, sort_keys=True))
    else:
        print(f"resumed {args.thread_id}  status={summary.get('aggregate_status', 'unknown')}")
        for w in summary.get("workers", []):
            print(f"  {w['id']} status={w['status']}")
        for error in summary.get("errors", []):
            print(f"  error: {error}", file=sys.stderr)
    return 0 if summary.get("aggregate_status") in {"complete", "dry-run"} else 1


def _thread_status(args: argparse.Namespace) -> int:
    """Print the current state of a graph thread."""
    graph = _build_graph()
    run_config = {"configurable": {"thread_id": args.thread_id}}

    try:
        snapshot = graph.get_state(run_config)
    except Exception:
        print(f"thread {args.thread_id}: no checkpoint found", file=sys.stderr)
        return 1

    if args.json:
        payload = {
            "thread_id": args.thread_id,
            "next": list(snapshot.next) if snapshot.next else [],
            "step": snapshot.metadata.get("step", "?"),
            "values_summary": _summarize_state(snapshot.values),
        }
        print(json.dumps(payload, indent=2, sort_keys=True))
    else:
        next_nodes = ", ".join(snapshot.next) if snapshot.next else "(finished)"
        step = snapshot.metadata.get("step", "?")
        print(f"thread {args.thread_id}  step={step}  next={{{next_nodes}}}")
        if snapshot.values:
            errors = snapshot.values.get("errors", [])
            workers = snapshot.values.get("workers", [])
            if errors:
                print(f"  errors: {len(errors)}")
                for e in errors:
                    print(f"    - {e}")
            if workers:
                print(f"  workers: {len(workers)}")
                for w in workers:
                    print(f"    {w['id']}  {w['status']}")
    return 0


def _summarize_state(values: dict[str, Any]) -> dict[str, Any]:
    out: dict[str, Any] = {}
    for k, v in values.items():
        if isinstance(v, list):
            out[k] = f"list[{len(v)}]"
        elif isinstance(v, dict):
            out[k] = f"dict[{len(v)}]"
        elif isinstance(v, str) and len(v) > 80:
            out[k] = v[:77] + "..."
        else:
            out[k] = v
    return out


def _print_interrupt_and_handoff(exc: GraphInterrupt, args: argparse.Namespace) -> None:
    """Print a human-readable handoff when the graph interrupts."""
    thread_id = getattr(args, "thread_id", None) or "?"
    interrupts = getattr(exc, "args", [None])[0] if hasattr(exc, "args") else None

    print(f"\n{'=' * 60}", file=sys.stderr)
    print(f"Graph interrupted — run is paused, not failed.", file=sys.stderr)
    print(f"", file=sys.stderr)
    print(f"  Thread id:  {thread_id}", file=sys.stderr)
    if isinstance(interrupts, (list, tuple)):
        for i, iv in enumerate(interrupts):
            stage = iv.value.get("stage", "?") if hasattr(iv, "value") else "?"
            print(f"  Interrupt {i}: stage={stage}", file=sys.stderr)
    print(f"", file=sys.stderr)
    print(f"  Resume:     tx-agent-runner resume --thread-id {thread_id} --action approve", file=sys.stderr)
    print(f"  Abort:      tx-agent-runner resume --thread-id {thread_id} --action abort", file=sys.stderr)
    print(f"  Inspect:    tx-agent-runner status --thread-id {thread_id}", file=sys.stderr)
    print(f"{'=' * 60}\n", file=sys.stderr)


if __name__ == "__main__":
    raise SystemExit(main())
