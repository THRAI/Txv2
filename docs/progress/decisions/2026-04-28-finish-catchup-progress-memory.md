# Decision: Finish Catch-up Progress Memory

**Date:** 2026-04-28

## Decision

- Every completed txKernel task must do a progress catch-up before the final
  response.
- The catch-up updates `docs/progress/STATUS.md` and any relevant
  plan/worktree/handoff/decision/research record.
- The catch-up must record changed surface, verification, next step, and
  blockers so future agents can resume without replaying chat.

## Context

- txKernel already keeps durable progress memory, but completion updates were
  phrased as good practice rather than a hard workflow exit condition.
- Agentic development in this workspace often spans multiple small boot,
  infrastructure, and documentation slices. Without an end-of-task catch-up,
  the repo can pass tests while project memory falls behind.

## Consequences

- `AGENTS.md`, `tx-progress-memory`, `tx-agentic-development`, and
  `.agents/commands/` now make finish catch-up mandatory.
- `finish-catchup.md` is the reusable command prompt for this exit step.
- Agents should mention the progress files they updated in final responses.

## Alternatives Considered

- Rely only on final chat summaries. Rejected because they are not queryable by
  future agents and disappear across handoffs.
- Require a new JSON record for every tiny task. Rejected because concise
  `STATUS.md` updates are enough when no plan/worktree/handoff is involved.
