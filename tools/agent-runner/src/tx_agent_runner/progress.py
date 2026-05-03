from __future__ import annotations

import json
import subprocess
from dataclasses import dataclass
from pathlib import Path
from typing import Any


@dataclass(frozen=True)
class WorktreeRecord:
    id: str
    status: str
    branch: str
    path: Path
    base: str
    plan: str | None
    owner: str
    write_scope: list[str]
    verification: list[str]
    notes: str
    source_path: Path

    def to_summary(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "status": self.status,
            "branch": self.branch,
            "path": str(self.path),
            "base": self.base,
            "plan": self.plan,
            "owner": self.owner,
            "write_scope": self.write_scope,
            "verification": self.verification,
            "notes": self.notes,
            "source_path": str(self.source_path),
        }


def load_worktree_records(
    repo_root: Path,
    *,
    active_only: bool = False,
    ids: list[str] | None = None,
) -> list[WorktreeRecord]:
    worktree_dir = repo_root / "docs" / "progress" / "worktrees"
    wanted = set(ids or [])
    records: list[WorktreeRecord] = []
    for path in sorted(worktree_dir.glob("*.json")):
        data = json.loads(path.read_text(encoding="utf-8"))
        if data.get("schema") != "tx.progress.worktree.v1":
            continue
        if active_only and data.get("status") != "active":
            continue
        if wanted and data.get("id") not in wanted:
            continue
        records.append(_record_from_json(path, data))
    if wanted:
        found = {record.id for record in records}
        missing = sorted(wanted - found)
        if missing:
            raise ValueError(f"missing worktree id(s): {', '.join(missing)}")
    return records


def _record_from_json(source_path: Path, data: dict[str, Any]) -> WorktreeRecord:
    return WorktreeRecord(
        id=str(data["id"]),
        status=str(data["status"]),
        branch=str(data["branch"]),
        path=Path(str(data["path"])),
        base=str(data["base"]),
        plan=data.get("plan"),
        owner=str(data["owner"]),
        write_scope=[str(value) for value in data.get("write_scope", [])],
        verification=[str(value) for value in data.get("verification", [])],
        notes=str(data.get("notes", "")),
        source_path=source_path,
    )


def get_git_worktrees(repo_root: Path) -> dict[str, Path]:
    result = subprocess.run(
        ["git", "worktree", "list", "--porcelain"],
        cwd=repo_root,
        check=True,
        capture_output=True,
        text=True,
    )
    return parse_git_worktree_porcelain(result.stdout)


def parse_git_worktree_porcelain(text: str) -> dict[str, Path]:
    worktrees: dict[str, Path] = {}
    current_path: Path | None = None
    for raw in text.splitlines():
        line = raw.strip()
        if not line:
            current_path = None
            continue
        if line.startswith("worktree "):
            current_path = Path(line.removeprefix("worktree "))
            continue
        if line.startswith("branch ") and current_path is not None:
            branch = line.removeprefix("branch ")
            branch = branch.removeprefix("refs/heads/")
            worktrees[branch] = current_path
    return worktrees
