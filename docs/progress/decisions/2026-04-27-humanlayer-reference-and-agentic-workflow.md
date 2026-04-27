# Decision: HumanLayer Reference And Agentic Workflow

**Date:** 2026-04-27

## Decision

- Add HumanLayer as a reference-only submodule at
  `external/humanlayer-reference`.
- Configure the submodule as a sparse checkout for `/.claude/*`.
- Add `tx-agentic-development` as the local skill for HumanLayer-style
  locator/analyzer/pattern-finder workflows.
- Add local `.agents/commands/agentic-*` prompts for research, planning, and
  implementation.

## Context

HumanLayer's workflow keeps the main agent context small by spawning focused
agents for file location, code analysis, pattern discovery, and project-memory
search. txKernel wants the same shape, but its authoritative sources are local:
`AGENTS.md`, `.agents/skills/`, `docs/design/`, and `docs/progress/`.

Git submodules cannot point directly at a subdirectory, so the submodule points
at the HumanLayer repository and uses sparse checkout to materialize only
`.claude/`.

## Consequences

- HumanLayer prompts are reference material, not txKernel instructions.
- txKernel skills and commands must adapt external workflow ideas into local
  paths and constraints.
- In Codex sessions, subagents are spawned only when the user explicitly asks
  for subagents, delegation, or parallel agent work.
- Multi-agent work should record plans, handoffs, and decisions in
  `docs/progress/`.

## Alternatives Considered

- Copy HumanLayer prompts into `.agents/`. This would drift quickly and blur
  provenance.
- Link to GitHub only. This avoids a submodule but gives agents no stable local
  reference.
- Submodule the full repository without sparse checkout. This adds avoidable
  workspace noise.
