---
name: tx-progress-memory
description: Use when recording decisions, progress, plans, handoffs, research notes, or resuming txKernel work across agent sessions.
---

# tx-progress-memory

Use this skill when work needs durable memory beyond the current chat: decisions,
plans, handoffs, status updates, research notes, or resume context.

## Read First

- `docs/progress/README.md`
- `docs/progress/STATUS.md`
- `docs/progress/templates/plan.json`
- `docs/progress/templates/handoff.json`
- `docs/progress/templates/worktree.json`
- The newest relevant file under `docs/progress/decisions/`,
  `docs/progress/plans/`, `docs/progress/handoffs/`, or
  `docs/progress/worktrees/`, or `docs/progress/research/`
- `.agents/commands/README.md` if you need a reusable command shape
- `.agents/commands/finish-catchup.md` before declaring a task complete

## Record Types

- Decision: permanent architecture or workflow choice with context,
  alternatives, and consequences.
- Plan: JSON scoped work that is not finished yet.
- Handoff: JSON continuation context for another agent or future session.
- Worktree: JSON record of branch, path, owner, write scope, and verification.
- Research: external or codebase findings with links and caveats.
- Status: short current-state checkpoint in `docs/progress/STATUS.md`.

## Rules

- Use dated filenames. Plans, handoffs, and worktrees use
  `YYYY-MM-DD-short-kebab-title.json`; decisions and research use
  `YYYY-MM-DD-short-kebab-title.md`.
- Keep status current and terse; move detail into plans, decisions, research, or
  handoffs.
- Every completed task needs a progress catch-up before the final response:
  update `docs/progress/STATUS.md`, update or close relevant JSON operational
  records, and add a dated decision or research note when the task changed
  architecture, workflow, or durable understanding.
- A catch-up records what changed, verification, next step, and blockers. For
  tiny tasks this can be one concise `STATUS.md` bullet; for multi-step work,
  use the relevant plan/handoff/worktree record plus a status pointer.
- Progress docs are project memory, not architecture contracts, unless a
  decision explicitly says which active design doc must be updated.
- Prefer links to files and commands over prose summaries that will drift.
- Keep operational JSON valid with `cargo xtask progress validate`.
- Use `cargo xtask progress list all --json` for machine-readable queries.

## Done Means

- The durable artifact states what changed, why, current state, next step, and
  blockers if any.
- JSON records pass `cargo xtask progress validate`.
- `docs/progress/STATUS.md` points to the newest durable artifact when useful.
- No active architecture decision lives only in chat.
- The final response can name the progress catch-up that was written or updated.
