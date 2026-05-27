# Decision: Xtask Module Split

**Date:** 2026-04-27

## Decision

- Split `xtask/src/main.rs` into command-family modules.
- Keep `main.rs` as a tiny binary entrypoint and `lib.rs` as dispatch glue.
- Add `xtask/README.md` as the module map for future command work.

## Context

The original `xtask/src/main.rs` had grown into a multi-thousand-line command
surface. That made it harder for agents to inspect only the command family they
needed and increased the chance of accidental cross-command edits.

## Consequences

- Command ownership is explicit: CI, lint, progress JSON, QEMU, images, OSComp,
  submit packaging, target policy, and shared utilities live in separate files.
- Future command work should add or edit the owning module instead of growing
  `lib.rs`.
- Focused tests should live beside the module that owns the rule.

## Alternatives Considered

- Leave `xtask` in one file. This keeps imports simple but hurts context
  hygiene.
- Split every helper into tiny files immediately. This would add ceremony before
  the command surface is large enough to need that much granularity.
