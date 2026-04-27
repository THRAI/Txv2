# Decision: Xtask Progress Command Surface

**Date:** 2026-04-27

## Decision

- Make `cargo xtask progress` the authoritative interface for operational JSON.
- Support typed `list`, `new`, `claim`, `close`, and `validate` commands.
- Treat `jq` as an optional ad hoc query tool, not as schema validation.

## Context

Agentic development needs records that can be created before content is fully
known, filled in incrementally, queried by stable schema, and checked without
loading unrelated context. Plain `jq .` only proves that JSON parses; it does
not catch schema drift, bad references, stale IDs, or overlapping write scopes.

## Consequences

- Plans, handoffs, and worktree records carry explicit `schema` fields.
- `cargo xtask progress validate` checks structure, IDs, dates, references,
  repo-relative paths, and active write-scope overlap.
- Agent commands and skills should prefer `cargo xtask progress ...` helpers.
- Future agent tools can query `cargo xtask progress list all --json` rather
  than parsing Markdown or shell script output.

## Alternatives Considered

- Keep using `jq .` only. This is simple but too weak for coordinating multiple
  agents safely.
- Move to a separate schema validator. That would add another required host
  tool before the workspace has enough JSON surface to justify it.
