# Decision: JSON Agent Operational Records

**Date:** 2026-04-27

## Decision

- Store plans, handoffs, and worktree records as JSON.
- Keep decisions and research as Markdown.
- Add typed `cargo xtask progress` helpers, templates, and list scripts for
  operational records.

## Context

Agentic development needs records that agents can list, filter, and validate
without loading full prose documents. JSON gives stable keys for `xtask`, `jq`,
grep, and future agent tools while keeping narrative records readable where
prose is the right shape.

## Consequences

- Plans live at `docs/progress/plans/YYYY-MM-DD-short-title.json`.
- Handoffs live at `docs/progress/handoffs/YYYY-MM-DD-short-title.json`.
- Worktree records live at `docs/progress/worktrees/YYYY-MM-DD-short-title.json`.
- Operational commands must validate changed JSON with
  `cargo xtask progress validate`.
- `jq` is now an optional ad hoc query tool reported by `cargo xtask doctor`.

## Alternatives Considered

- Keep all progress memory as Markdown. This is easier to read but harder for
  agents to query and update reliably.
- Move every progress record to JSON. This makes decisions and research too
  cramped for nuanced context.
