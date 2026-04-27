---
name: tx-agentic-development
description: Use when planning or running multi-agent txKernel development, adapting HumanLayer-style locator/analyzer/pattern/research workflows without polluting the main context.
---

# tx-agentic-development

Use this skill when work benefits from isolated research, focused analysis, or
parallel implementation. It adapts the HumanLayer `.claude` workflow to
txKernel's docs, skills, and progress memory.

## Read First

- `AGENTS.md`
- `.agents/skills/tx-design-reference/SKILL.md`
- `.agents/skills/tx-progress-memory/SKILL.md`
- `docs/progress/STATUS.md`
- HumanLayer reference prompts under `external/humanlayer-reference/.claude/`:
  - `commands/create_plan.md`
  - `commands/implement_plan.md`
  - `commands/resume_handoff.md`
  - `commands/research_codebase.md`
  - `commands/create_worktree.md`
  - `agents/codebase-locator.md`
  - `agents/codebase-analyzer.md`
  - `agents/codebase-pattern-finder.md`
  - `agents/thoughts-locator.md`
  - `agents/thoughts-analyzer.md`

## Adapted Workflow

1. Main agent reads the user's task, current progress memory, and critical
   design docs itself.
2. If subagents are authorized for the session, delegate bounded sidecar tasks:
   locator finds files, analyzer explains current behavior, pattern-finder finds
   examples, progress-memory locator finds relevant decisions and handoffs.
3. Main agent reads the returned files needed for implementation and owns the
   final synthesis.
4. Workers get disjoint write scopes and are told they are not alone in the
   codebase.
5. Implementation plans, handoffs, and worktrees are recorded as JSON under
   `docs/progress/`; decisions and research stay as Markdown.
6. When the task finishes, run the finish catch-up workflow before the final
   response: update `docs/progress/STATUS.md`, update or close relevant
   plan/worktree/handoff JSON, record any new decision/research note, and
   validate changed operational JSON.

## Codex Rule

In Codex sessions, spawn subagents only when the user explicitly asks for
subagents, delegation, or parallel agent work. Without that authorization, use
the same roles as local research modes rather than spawning.

## Role Boundaries

- Locator: find files and directories; do not critique or propose changes.
- Analyzer: explain how current code/docs work with file references; do not
  redesign.
- Pattern-finder: show existing examples and conventions; do not rank them
  unless asked.
- Progress-memory locator/analyzer: find and summarize relevant decisions,
  JSON plans, JSON worktrees, research, and JSON handoffs.
- Worker: implement a bounded task in an assigned write scope and report changed
  files and verification.

## Done Means

- The main context contains only the task spec, critical docs, and synthesized
  subagent results.
- Parallel work has disjoint write scopes.
- Verification uses `cargo xtask check` or a narrower command named in the plan.
- Operational progress records pass `cargo xtask progress validate`.
- Any durable lesson is recorded in `docs/progress/`, not only in chat.
- Completed work has a catch-up entry in `docs/progress/` naming changed
  surface, verification, next step, and blockers.
