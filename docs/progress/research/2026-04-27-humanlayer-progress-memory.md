# Research: HumanLayer Progress Memory Pattern

**Date:** 2026-04-27

## Question

How should txKernel record decisions and progress so future agents can resume
work without inflating always-on instructions?

## Findings

- HumanLayer keeps reusable prompt workflows under `.claude/commands/`, including
  plan creation, handoff creation, handoff resume, codebase research, reviews,
  and implementation-plan iteration.
- HumanLayer keeps focused locator/analyzer agents under `.claude/agents/`,
  including codebase locator/analyzer/pattern-finder roles and thoughts
  locator/analyzer roles.
- Their handoff workflow writes durable handoff documents into a shared
  `thoughts` tree, with date/time filenames and enough context to transfer work
  to another session.
- Their plan workflow emphasizes complete context reads, focused research,
  explicit current-state analysis, scoped phases, automated/manual success
  criteria, and syncing/indexing the durable notes.
- Their thoughts locator treats memory as structured project data: research,
  plans, tickets, PR notes, discussions, and decisions are searched and returned
  by category before deeper analysis.

## Applicability To txKernel

- Keep HumanLayer available as `external/humanlayer-reference/.claude/` so
  agents can inspect the source workflow locally.
- Use `.agents/commands/` as our command-prompt layer.
- Use `.agents/skills/` as our scoped task-guidance layer.
- Use `docs/progress/` instead of a generic `thoughts/` tree because txKernel is
  currently a docs-plus-infra workspace and this keeps project memory near the
  docs without mixing it into active architecture contracts.
- Add `tx-progress-memory` so agents know when to read and write progress docs.
- Keep `AGENTS.md` small and let skills/commands carry workflow detail.

## Sources

- [HumanLayer `.claude/commands`](https://github.com/humanlayer/humanlayer/tree/main/.claude/commands)
- [HumanLayer `.claude/agents`](https://github.com/humanlayer/humanlayer/tree/main/.claude/agents)
- [HumanLayer `create_handoff.md`](https://raw.githubusercontent.com/humanlayer/humanlayer/main/.claude/commands/create_handoff.md)
- [HumanLayer `create_plan.md`](https://raw.githubusercontent.com/humanlayer/humanlayer/main/.claude/commands/create_plan.md)
- [HumanLayer `resume_handoff.md`](https://raw.githubusercontent.com/humanlayer/humanlayer/main/.claude/commands/resume_handoff.md)
- [HumanLayer `thoughts-locator.md`](https://raw.githubusercontent.com/humanlayer/humanlayer/main/.claude/agents/thoughts-locator.md)
