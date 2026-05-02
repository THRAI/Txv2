from pathlib import Path

from tx_agent_runner.progress import WorktreeRecord
from tx_agent_runner.prompts import build_worker_prompt


def sample_record() -> WorktreeRecord:
    return WorktreeRecord(
        id="2026-04-29-demo",
        status="active",
        branch="codex/demo",
        path=Path("/tmp/demo"),
        base="main",
        plan=None,
        owner="Codex demo lane",
        write_scope=["crates/demo"],
        verification=["cargo test -p demo"],
        notes="demo notes",
        source_path=Path("docs/progress/worktrees/2026-04-29-demo.json"),
    )


def test_status_prompt_is_read_only_and_contains_guardrails() -> None:
    prompt = build_worker_prompt(sample_record(), mode="status", task=None)

    assert "Worktree id: 2026-04-29-demo" in prompt
    assert "Owner: Codex demo lane" in prompt
    assert "Branch: codex/demo" in prompt
    assert "crates/demo" in prompt
    assert "cargo test -p demo" in prompt
    assert "You are not alone in the codebase" in prompt
    assert "Do not edit files" in prompt
    assert "changed files" in prompt
    assert "commands run" in prompt
    assert "blockers" in prompt
    assert "next step" in prompt


def test_execute_prompt_requires_task_and_forbids_out_of_scope_edits() -> None:
    prompt = build_worker_prompt(sample_record(), mode="execute", task="Implement demo")

    assert "Task: Implement demo" in prompt
    assert "Do not edit outside the write scope" in prompt
