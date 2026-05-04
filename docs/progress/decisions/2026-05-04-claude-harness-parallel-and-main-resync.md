# Decision: Claude Harness Parallel and Main Resync via Cherry-pick

**Date:** 2026-05-04

## Decision

- The repo now ships a Claude Code harness parallel alongside the existing `.agents/` codex layout:
  - `CLAUDE.md` is a symlink to `AGENTS.md` so any harness that auto-loads `CLAUDE.md` finds the same canonical guide.
  - `.claude/settings.json` defines a `SessionStart` hook that emits `AGENTS.md` as `additionalContext`, guaranteeing tacit pickup at session start in builds where `CLAUDE.md` auto-load is not honored.
  - `.claude/skills/` and `.claude/commands/` symlinks were not retained: probes confirmed Claude Code does not scan project-local skills or commands in this build, so the symlinks were misleading. `.agents/skills/` and `.agents/commands/` remain the single source of truth and are reachable via `Read` from any agent that is given the path.
- `claude/objective-davinci-37d55d` was resynced to current `origin/main` by `git reset --hard origin/main` followed by `git cherry-pick origin/main..backup/pre-main-resync-2026-05-04`. All ten PageBacked + VM commits replayed cleanly with zero conflicts and the resulting tree compiles and passes the named focused tests.

## Context

- Probes from a fresh top-level session and a subagent both showed:
  - `AGENTS.md` is on disk in every worktree but is not auto-injected into either parent or subagent context.
  - `.claude/skills/<name>/SKILL.md` files are not registered as invokable skills by the harness; the listed skills are user/plugin-level only.
  - `.claude/commands/*.md` are not surfaced as project-local slash commands.
  - The auto-memory section is loaded for top-level sessions only, never for subagents.
- The branch was 10 ahead, 7 behind `origin/main`. Inspection showed the merge-base (`4b2b9bd`) is the tip of the merged PR #14 (`codex/vm-pagebacked-impl`), so the ten local commits are net-new VM/PageBacked work on top of that PR. The seven incoming commits are the HAL / useraccessif / irqif work that landed after PR #14, with no semantic overlap on the files our commits touch.

## Consequences

- Top-level sessions in this worktree (or any worktree once `.claude/settings.json` and `CLAUDE.md` propagate via `main`) will receive `AGENTS.md` as additionalContext at session start. Subagents still need explicit context handed to them in the spawn prompt; there is no harness-level pass-through.
- Future subagent prompts that should follow txKernel rules must paste the relevant rules or doc paths inline. Treat `.agents/skills/` and `.agents/commands/` as `Read`-able operational docs, not as registrable skills/commands.
- Cherry-pick over reset preserved each PageBacked/VM commit as an independent atomic unit; commit SHAs were rewritten but messages and per-commit boundaries were not.
- A backup ref `backup/pre-main-resync-2026-05-04` retains the pre-resync history if anything must be recovered later.
- Verification used: `cargo xtask progress validate` (23 records ok), `cargo check --workspace --all-targets --exclude tx-kernel-riscv64-qemu-virt --exclude tx-kernel-riscv64-m1dock-mock --exclude tx-kernel-loongarch64-qemu-virt`, `cargo test -p tx-kernel vm -- --test-threads=1` (49 ok), `cargo test -p tx-kernel page_backed -- --test-threads=1` (25 ok).

## Alternatives Considered

- **`git merge origin/main`.** Rejected once it became clear the work is non-overlapping; a merge commit would have added noise without semantic value, and the repo otherwise uses merge commits for PR integration rather than mid-branch sync. Cherry-pick keeps the local branch linear and ready for a single PR-level merge later.
- **Keep `.claude/skills` and `.claude/commands` symlinks.** Rejected because the probes proved they are not honored by the harness; leaving them implies an invokability that does not exist.
- **Write a CLAUDE.md copy of AGENTS.md instead of a symlink.** Rejected to avoid drift; the symlink keeps the canonical doc single-sourced.
