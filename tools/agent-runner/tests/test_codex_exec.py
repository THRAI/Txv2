import os
import stat
from pathlib import Path

from tx_agent_runner.codex_exec import run_codex_worker
from tx_agent_runner.progress import WorktreeRecord


def make_fake_codex(path: Path, exit_code: int = 0) -> Path:
    script = path / "fake-codex"
    script.write_text(
        f"""#!/usr/bin/env python3
import json
import os
import pathlib
import sys

pathlib.Path(os.environ["FAKE_CODEX_ARGS"]).write_text(json.dumps(sys.argv[1:]), encoding="utf-8")
out = pathlib.Path(sys.argv[sys.argv.index("--output-last-message") + 1])
out.write_text("final report", encoding="utf-8")
print(json.dumps({{"event": "done"}}))
sys.exit({exit_code})
""",
        encoding="utf-8",
    )
    script.chmod(script.stat().st_mode | stat.S_IXUSR)
    return script


def sample_record(path: Path) -> WorktreeRecord:
    return WorktreeRecord(
        id="demo",
        status="active",
        branch="codex/demo",
        path=path,
        base="main",
        plan=None,
        owner="test",
        write_scope=["crates/demo"],
        verification=[],
        notes="",
        source_path=Path("docs/progress/worktrees/demo.json"),
    )


def test_run_codex_worker_uses_expected_exec_flags(tmp_path: Path, monkeypatch) -> None:
    worktree = tmp_path / "worktree"
    worktree.mkdir()
    run_dir = tmp_path / "run"
    args_path = tmp_path / "args.json"
    monkeypatch.setenv("FAKE_CODEX_ARGS", str(args_path))

    result = run_codex_worker(
        record=sample_record(worktree),
        prompt="hello",
        run_dir=run_dir,
        codex_bin=str(make_fake_codex(tmp_path)),
        sandbox="read-only",
    )

    args = json_load(args_path)
    assert args[:2] == ["exec", "--cd"]
    assert str(worktree) in args
    assert "--sandbox" in args
    assert "read-only" in args
    assert "--json" in args
    assert "--output-last-message" in args
    assert result.status == "complete"
    assert result.final_message == "final report"
    assert (run_dir / "demo.events.jsonl").read_text(encoding="utf-8").strip()


def test_run_codex_worker_reports_nonzero_exit_as_blocked(tmp_path: Path, monkeypatch) -> None:
    worktree = tmp_path / "worktree"
    worktree.mkdir()
    args_path = tmp_path / "args.json"
    monkeypatch.setenv("FAKE_CODEX_ARGS", str(args_path))

    result = run_codex_worker(
        record=sample_record(worktree),
        prompt="hello",
        run_dir=tmp_path / "run",
        codex_bin=str(make_fake_codex(tmp_path, exit_code=7)),
        sandbox="workspace-write",
    )

    assert result.status == "blocked"
    assert result.returncode == 7


def json_load(path: Path) -> list[str]:
    import json

    return json.loads(path.read_text(encoding="utf-8"))
