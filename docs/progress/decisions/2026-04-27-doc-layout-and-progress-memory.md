# Decision: Doc Layout And Progress Memory

**Date:** 2026-04-27

## Decision

- Collect active architecture contracts under `docs/design/`.
- Collect imported EBR/Zone mechanics references under `docs/ebr-zone/`.
- Record durable work memory under `docs/progress/`.
- Store reusable agent command prompts under `.agents/commands/`.
- Add skills for design-reference gathering and progress-memory recording.

## Context

The repository root had active architecture docs, imported reference sketches,
workspace code, and tooling side by side. That made it too easy for agents to
load the wrong source or treat historical notes as current contracts.

HumanLayer's `.claude` layout separates scoped agents, reusable commands, and
durable memory/research flows. txKernel adopts that pattern locally without
copying their exact file tree: `.agents/skills` remains the task guidance layer,
`.agents/commands` becomes the reusable prompt layer, and `docs/progress` is the
shared memory layer.

The source scan is recorded in
`docs/progress/research/2026-04-27-humanlayer-progress-memory.md`.

## Consequences

- `AGENTS.md`, skills, and manifests must point at `docs/design/...` paths.
- Link checks and stale-vocabulary checks remain required after doc edits.
- Progress notes may summarize implementation state, but architecture decisions
  that change kernel semantics must still be promoted into `docs/design/`.
- External research belongs under `docs/progress/research/` with links and
  caveats instead of being left only in chat.

## Alternatives Considered

- Keep the numbered folders at the repository root. This preserved old links but
  kept the root cluttered.
- Put all docs under one flat `docs/` folder. This hid the distinction between
  active contracts, imported mechanics, and progress memory.
