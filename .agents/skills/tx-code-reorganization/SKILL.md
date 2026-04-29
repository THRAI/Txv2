---
name: tx-code-reorganization
description: Use when splitting large txKernel code files, moving module facades into directories, reorganizing functions by state/lifecycle/helpers, or adding code comments that explain module data structures and mutation flow.
---

# tx-code-reorganization

Use this skill for behavior-preserving code shape work: shrinking large modules,
moving tests, clarifying module boundaries, and adding comments that make the
state machine readable.

## Read First

- `AGENTS.md`
- `docs/design/00_meta-framework/MODULE_MAP_v1.md`
- `docs/design/00_meta-framework/CI_REPORTING_v1.md`
- The domain skill for the code being reorganized, such as `tx-hal-axhal`.
- Recent extraction decisions under `docs/progress/decisions/` when they name
  the subsystem being touched.

## Workflow

1. Size and shape the target:
   - `rg --files <scope>`
   - `wc -l <files>`
   - `rg -n "struct|enum|trait|impl|pub\\(|fn " <scope>`
2. Preserve behavior first. Move code by responsibility before changing logic.
3. If a file is becoming a module directory, move `foo.rs` to `foo/mod.rs`, then
   add child modules for distinct responsibilities.
4. Keep exports clean: `mod child;` plus narrow `pub(crate) use child::{...}`.
   Prefer explicit namespaces for configuration/topology constants.
5. Move inline unit tests to `tests.rs` when they need private access; use
   integration tests only for public behavior.
6. Reorder each module to match its header:
   - core data structures and state first;
   - core lifecycle or data-flow functions next;
   - helpers after, grouped by topic.
7. Comment by section, not by function. Top docs should name:
   - the core data structures/state maintained;
   - the main state-mutating or data-flow functions;
   - helper groups;
   - relevant progress decisions when the split preserves a design rule.
8. Before finishing, update progress memory with what changed, verification,
   next step, and blockers.

## Guardrails

- Authored Rust source files must stay at or below 1,500 lines. Split by
  responsibility before adding more behavior.
- Avoid giant import surfaces. If import lists become noisy, narrow exports or
  move shared vocabulary into a focused child module.
- Do not hide domain boundaries behind broad preludes.
- Do not mix mechanical reorganization with semantic changes unless the user
  explicitly asks for both.
- Do not revert unrelated dirty work.

## Verification

Run the narrow package tests first, then the repo gates that match the change:

```sh
cargo fmt --check
cargo test -p <package>
cargo xtask lint arch
cargo xtask lint unused
cargo xtask progress validate
cargo xtask ci
git diff --check
```
