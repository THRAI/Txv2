import json
import os
import stat
from pathlib import Path

from tx_agent_runner.graph import RunConfig, run_agent_graph


def write_worktree(root: Path, name: str, worktree_path: Path) -> None:
    path = root / "docs" / "progress" / "worktrees"
    path.mkdir(parents=True, exist_ok=True)
    data = {
        "schema": "tx.progress.worktree.v1",
        "id": name,
        "created": "2026-04-29",
        "updated": "2026-04-29",
        "status": "active",
        "branch": f"codex/{name}",
        "path": str(worktree_path),
        "base": "main",
        "plan": None,
        "owner": "test",
        "write_scope": [f"crates/{name}"],
        "verification": [],
        "notes": "",
    }
    (path / f"{name}.json").write_text(json.dumps(data), encoding="utf-8")


def fake_git(root: Path, branches: dict[str, Path]) -> Path:
    script = root / "git"
    lines = []
    for branch, path in branches.items():
        lines.append(f"worktree {path}\\nHEAD abc\\nbranch refs/heads/{branch}\\n")
    script.write_text(
        "#!/usr/bin/env sh\n"
        "if [ \"$1 $2 $3\" = \"worktree list --porcelain\" ]; then\n"
        f"  printf '%b' {json.dumps(''.join(lines))}\n"
        "elif [ \"$1 $2\" = \"status --short\" ]; then\n"
        "  exit 0\n"
        "else\n"
        "  exit 0\n"
        "fi\n",
        encoding="utf-8",
    )
    script.chmod(script.stat().st_mode | stat.S_IXUSR)
    return script


def test_dry_run_writes_prompts_and_summary_without_launching_codex(tmp_path: Path, monkeypatch) -> None:
    wt = tmp_path / "wt"
    wt.mkdir()
    write_worktree(tmp_path, "demo", wt)
    git_bin = fake_git(tmp_path, {"codex/demo": wt})
    monkeypatch.setenv("PATH", f"{tmp_path}{os.pathsep}{os.environ['PATH']}")

    summary = run_agent_graph(
        RunConfig(
            repo_root=tmp_path,
            worktree_ids=["demo"],
            mode="status",
            task=None,
            max_workers=2,
            dry_run=True,
            codex_bin="codex-that-should-not-run",
            output_dir=tmp_path / "out",
            run_id="run",
            progress_validate=False,
        )
    )

    assert summary["aggregate_status"] == "dry-run"
    assert (tmp_path / "out" / "run" / "demo.prompt.md").exists()
    assert (tmp_path / "out" / "run" / "summary.json").exists()


def test_run_mode_uses_max_workers_and_records_worker_results(tmp_path: Path, monkeypatch) -> None:
    wt_a = tmp_path / "wt-a"
    wt_b = tmp_path / "wt-b"
    wt_a.mkdir()
    wt_b.mkdir()
    write_worktree(tmp_path, "a", wt_a)
    write_worktree(tmp_path, "b", wt_b)
    fake_git(tmp_path, {"codex/a": wt_a, "codex/b": wt_b})
    fake_codex = tmp_path / "codex"
    fake_codex.write_text(
        "#!/usr/bin/env python3\n"
        "import pathlib, sys\n"
        "out = pathlib.Path(sys.argv[sys.argv.index('--output-last-message') + 1])\n"
        "out.write_text('final', encoding='utf-8')\n"
        "print('{\"event\":\"done\"}')\n",
        encoding="utf-8",
    )
    fake_codex.chmod(fake_codex.stat().st_mode | stat.S_IXUSR)
    monkeypatch.setenv("PATH", f"{tmp_path}{os.pathsep}{os.environ['PATH']}")

    summary = run_agent_graph(
        RunConfig(
            repo_root=tmp_path,
            worktree_ids=None,
            mode="status",
            task=None,
            max_workers=1,
            dry_run=False,
            codex_bin=str(fake_codex),
            output_dir=tmp_path / "out",
            run_id="run",
            progress_validate=False,
        )
    )

    assert summary["max_workers"] == 1
    assert summary["aggregate_status"] == "complete"
    assert {worker["id"] for worker in summary["workers"]} == {"a", "b"}
    assert all(worker["status"] == "complete" for worker in summary["workers"])


def test_selected_run_still_validates_all_active_write_scope_leases(
    tmp_path: Path,
    monkeypatch,
) -> None:
    wt_a = tmp_path / "wt-a"
    wt_b = tmp_path / "wt-b"
    wt_a.mkdir()
    wt_b.mkdir()
    write_worktree(tmp_path, "a", wt_a)
    write_worktree(tmp_path, "b", wt_b)
    record_b = tmp_path / "docs" / "progress" / "worktrees" / "b.json"
    data = json.loads(record_b.read_text(encoding="utf-8"))
    data["write_scope"] = ["crates/a/submodule"]
    record_b.write_text(json.dumps(data), encoding="utf-8")
    fake_git(tmp_path, {"codex/a": wt_a, "codex/b": wt_b})
    monkeypatch.setenv("PATH", f"{tmp_path}{os.pathsep}{os.environ['PATH']}")

    summary = run_agent_graph(
        RunConfig(
            repo_root=tmp_path,
            worktree_ids=["a"],
            mode="status",
            task=None,
            max_workers=1,
            dry_run=True,
            codex_bin="codex-that-should-not-run",
            output_dir=tmp_path / "out",
            run_id="run",
            progress_validate=False,
        )
    )

    assert summary["aggregate_status"] == "blocked"
    assert summary["workers"] == []
    assert any("overlaps" in error for error in summary["errors"])
