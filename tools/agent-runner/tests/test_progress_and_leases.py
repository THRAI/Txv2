import json
from pathlib import Path

import pytest

from tx_agent_runner.leases import validate_worktree_leases
from tx_agent_runner.progress import (
    WorktreeRecord,
    load_worktree_records,
    parse_git_worktree_porcelain,
)


def write_worktree(root: Path, name: str, **overrides: object) -> Path:
    path = root / "docs" / "progress" / "worktrees"
    path.mkdir(parents=True, exist_ok=True)
    data = {
        "schema": "tx.progress.worktree.v1",
        "id": name,
        "created": "2026-04-29",
        "updated": "2026-04-29",
        "status": "active",
        "branch": f"codex/{name}",
        "path": str(root / f"wt-{name}"),
        "base": "main",
        "plan": None,
        "owner": "test",
        "write_scope": [f"crates/{name}"],
        "verification": ["cargo test"],
        "notes": "fixture",
    }
    data.update(overrides)
    out = path / f"{name}.json"
    out.write_text(json.dumps(data), encoding="utf-8")
    return out


def test_load_worktree_records_returns_only_active_records(tmp_path: Path) -> None:
    active_path = tmp_path / "active-wt"
    active_path.mkdir()
    closed_path = tmp_path / "closed-wt"
    closed_path.mkdir()
    write_worktree(tmp_path, "active", path=str(active_path))
    write_worktree(
        tmp_path,
        "closed",
        status="closed",
        path=str(closed_path),
        branch="codex/closed",
    )

    records = load_worktree_records(tmp_path, active_only=True)

    assert [record.id for record in records] == ["active"]
    assert records[0].branch == "codex/active"
    assert records[0].write_scope == ["crates/active"]


def test_parse_git_worktree_porcelain_maps_branch_to_path() -> None:
    text = """worktree /repo
HEAD abc
branch refs/heads/main

worktree /tmp/wt
HEAD def
branch refs/heads/codex/demo
"""

    parsed = parse_git_worktree_porcelain(text)

    assert parsed["main"] == Path("/repo")
    assert parsed["codex/demo"] == Path("/tmp/wt")


def test_validate_worktree_leases_rejects_missing_paths(tmp_path: Path) -> None:
    record = WorktreeRecord(
        id="missing",
        status="active",
        branch="codex/missing",
        path=tmp_path / "missing",
        base="main",
        plan=None,
        owner="test",
        write_scope=["crates/missing"],
        verification=[],
        notes="",
        source_path=tmp_path / "docs/progress/worktrees/missing.json",
    )

    errors = validate_worktree_leases([record], {"codex/missing": record.path})

    assert any("does not exist" in error for error in errors)


def test_validate_worktree_leases_rejects_branch_mismatch(tmp_path: Path) -> None:
    actual = tmp_path / "actual"
    actual.mkdir()
    record = WorktreeRecord(
        id="demo",
        status="active",
        branch="codex/demo",
        path=tmp_path / "recorded",
        base="main",
        plan=None,
        owner="test",
        write_scope=["crates/demo"],
        verification=[],
        notes="",
        source_path=tmp_path / "docs/progress/worktrees/demo.json",
    )

    errors = validate_worktree_leases([record], {"codex/demo": actual})

    assert any("branch codex/demo points to" in error for error in errors)


def test_validate_worktree_leases_rejects_overlapping_write_scopes(tmp_path: Path) -> None:
    path_a = tmp_path / "a"
    path_b = tmp_path / "b"
    path_a.mkdir()
    path_b.mkdir()
    a = WorktreeRecord(
        id="a",
        status="active",
        branch="codex/a",
        path=path_a,
        base="main",
        plan=None,
        owner="a",
        write_scope=["crates/tx-kernel/src"],
        verification=[],
        notes="",
        source_path=tmp_path / "a.json",
    )
    b = WorktreeRecord(
        id="b",
        status="active",
        branch="codex/b",
        path=path_b,
        base="main",
        plan=None,
        owner="b",
        write_scope=["crates/tx-kernel/src/lib.rs"],
        verification=[],
        notes="",
        source_path=tmp_path / "b.json",
    )

    errors = validate_worktree_leases([a, b], {"codex/a": path_a, "codex/b": path_b})

    assert any("overlap" in error for error in errors)


def test_validate_worktree_leases_allows_distinct_worktree_record_files(tmp_path: Path) -> None:
    path_a = tmp_path / "a"
    path_b = tmp_path / "b"
    path_a.mkdir()
    path_b.mkdir()
    a = WorktreeRecord(
        id="a",
        status="active",
        branch="codex/a",
        path=path_a,
        base="main",
        plan=None,
        owner="a",
        write_scope=["docs/progress/worktrees/a.json"],
        verification=[],
        notes="",
        source_path=tmp_path / "a.json",
    )
    b = WorktreeRecord(
        id="b",
        status="active",
        branch="codex/b",
        path=path_b,
        base="main",
        plan=None,
        owner="b",
        write_scope=["docs/progress/worktrees/b.json"],
        verification=[],
        notes="",
        source_path=tmp_path / "b.json",
    )

    errors = validate_worktree_leases([a, b], {"codex/a": path_a, "codex/b": path_b})

    assert errors == []
