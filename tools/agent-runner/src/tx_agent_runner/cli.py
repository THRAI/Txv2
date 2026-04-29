from __future__ import annotations

import argparse
import json
import subprocess
import sys
from pathlib import Path
from typing import Any

from .graph import RunConfig, run_agent_graph
from .leases import validate_worktree_leases
from .progress import get_git_worktrees, load_worktree_records


def main(argv: list[str] | None = None) -> int:
    parser = _parser()
    args = parser.parse_args(argv)
    repo_root = _repo_root(Path(args.repo_root) if args.repo_root else Path.cwd())
    try:
        if args.command == "inspect":
            return _inspect(args, repo_root)
        if args.command == "run":
            return _run(args, repo_root)
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
    )
    summary = run_agent_graph(config)
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


if __name__ == "__main__":
    raise SystemExit(main())
