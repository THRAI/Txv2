from __future__ import annotations

import subprocess
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from .progress import WorktreeRecord


@dataclass(frozen=True)
class CodexWorkerResult:
    id: str
    status: str
    returncode: int
    prompt_path: Path
    events_path: Path
    final_path: Path
    final_message: str
    stderr: str
    git_status: str

    def to_summary(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "status": self.status,
            "returncode": self.returncode,
            "prompt_path": str(self.prompt_path),
            "events_path": str(self.events_path),
            "final_path": str(self.final_path),
            "final_message": self.final_message,
            "stderr": self.stderr,
            "git_status": self.git_status,
        }


def run_codex_worker(
    *,
    record: WorktreeRecord,
    prompt: str,
    run_dir: Path,
    codex_bin: str,
    sandbox: str,
) -> CodexWorkerResult:
    run_dir.mkdir(parents=True, exist_ok=True)
    stem = _artifact_stem(record.id)
    prompt_path = run_dir / f"{stem}.prompt.md"
    events_path = run_dir / f"{stem}.events.jsonl"
    final_path = run_dir / f"{stem}.final.md"
    prompt_path.write_text(prompt, encoding="utf-8")

    command = [
        codex_bin,
        "exec",
        "--cd",
        str(record.path),
        "--sandbox",
        sandbox,
        "--json",
        "--output-last-message",
        str(final_path),
        "-",
    ]
    result = subprocess.run(
        command,
        input=prompt,
        text=True,
        capture_output=True,
        cwd=record.path,
        check=False,
    )
    events_path.write_text(result.stdout, encoding="utf-8")
    final_message = final_path.read_text(encoding="utf-8") if final_path.exists() else ""
    git_status = _git_status(record.path)
    status = "complete" if result.returncode == 0 else "blocked"
    return CodexWorkerResult(
        id=record.id,
        status=status,
        returncode=result.returncode,
        prompt_path=prompt_path,
        events_path=events_path,
        final_path=final_path,
        final_message=final_message,
        stderr=result.stderr,
        git_status=git_status,
    )


def write_dry_run_prompt(record: WorktreeRecord, prompt: str, run_dir: Path) -> dict[str, Any]:
    run_dir.mkdir(parents=True, exist_ok=True)
    stem = _artifact_stem(record.id)
    prompt_path = run_dir / f"{stem}.prompt.md"
    prompt_path.write_text(prompt, encoding="utf-8")
    return {
        "id": record.id,
        "status": "dry-run",
        "returncode": None,
        "prompt_path": str(prompt_path),
        "events_path": None,
        "final_path": None,
        "final_message": "",
        "stderr": "",
        "git_status": _git_status(record.path),
    }


def _git_status(cwd: Path) -> str:
    result = subprocess.run(
        ["git", "status", "--short"],
        cwd=cwd,
        text=True,
        capture_output=True,
        check=False,
    )
    return result.stdout


def _artifact_stem(value: str) -> str:
    return value.replace("/", "_")
