from __future__ import annotations

from .progress import WorktreeRecord


def build_worker_prompt(record: WorktreeRecord, *, mode: str, task: str | None) -> str:
    if mode not in {"status", "execute"}:
        raise ValueError(f"unsupported mode: {mode}")
    if mode == "execute" and not task:
        raise ValueError("execute mode requires --task")

    read_only = mode == "status"
    task_text = task or "Inspect the lane status and report current state. Do not edit files."
    edit_rule = (
        "Do not edit files. Use read-only inspection commands only."
        if read_only
        else "Do not edit outside the write scope. Do not revert edits made by others."
    )

    return "\n".join(
        [
            "# Tx Codex Worker Assignment",
            "",
            "You are a bounded worker for txKernel parallel development.",
            "You are not alone in the codebase; other workers may be active in other worktrees.",
            "",
            f"Worktree id: {record.id}",
            f"Owner: {record.owner}",
            f"Branch: {record.branch}",
            f"Worktree path: {record.path}",
            f"Mode: {mode}",
            f"Task: {task_text}",
            "",
            "Write scope:",
            *_bullet_lines(record.write_scope),
            "",
            "Verification commands expected for this lane:",
            *(_bullet_lines(record.verification) or ["- None recorded"]),
            "",
            "Lane notes:",
            record.notes or "None",
            "",
            "Rules:",
            f"- {edit_rule}",
            "- Keep changes scoped to this assignment.",
            "- Preserve unrelated user or worker changes.",
            "- Run only verification commands relevant to this lane.",
            "- If scope is ambiguous or blocked, stop and report the blocker.",
            "",
            "Final report format:",
            "- changed files: list paths or say none",
            "- commands run: list commands and pass/fail result",
            "- blockers: list blockers or say none",
            "- next step: one concrete next action",
            "",
        ]
    )


def _bullet_lines(values: list[str]) -> list[str]:
    return [f"- {value}" for value in values]
