from __future__ import annotations

from pathlib import Path

from .progress import WorktreeRecord


def validate_worktree_leases(
    records: list[WorktreeRecord],
    git_worktrees: dict[str, Path],
) -> list[str]:
    errors: list[str] = []
    for record in records:
        errors.extend(_validate_record_path(record, git_worktrees))
    errors.extend(_validate_scope_overlap(records))
    return errors


def _validate_record_path(
    record: WorktreeRecord,
    git_worktrees: dict[str, Path],
) -> list[str]:
    errors: list[str] = []
    if not record.path.exists():
        errors.append(f"{record.id}: worktree path {record.path} does not exist")
    actual = git_worktrees.get(record.branch)
    if actual is None:
        errors.append(f"{record.id}: branch {record.branch} is not in git worktree list")
    elif actual.resolve() != record.path.resolve():
        errors.append(
            f"{record.id}: branch {record.branch} points to {actual}, "
            f"but record path is {record.path}"
        )
    return errors


def _validate_scope_overlap(records: list[WorktreeRecord]) -> list[str]:
    errors: list[str] = []
    scopes: list[tuple[str, str, str]] = []
    for record in records:
        for scope in record.write_scope:
            normalized = _normalize_scope(scope)
            if normalized:
                scopes.append((record.id, scope, normalized))

    for left_index, left in enumerate(scopes):
        for right in scopes[left_index + 1 :]:
            left_id, left_raw, left_scope = left
            right_id, right_raw, right_scope = right
            if left_id == right_id:
                continue
            if _scopes_overlap(left_scope, right_scope):
                errors.append(
                    f"{left_id}:{left_raw} overlaps {right_id}:{right_raw}"
                )
    return errors


def _normalize_scope(scope: str) -> str:
    return scope.strip().strip("/").replace("\\", "/")


def _scopes_overlap(left: str, right: str) -> bool:
    if left == right:
        return True
    return right.startswith(left + "/") or left.startswith(right + "/")
