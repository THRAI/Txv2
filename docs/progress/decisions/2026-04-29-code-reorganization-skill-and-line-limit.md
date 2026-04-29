# 2026-04-29: Code reorganization skill and Rust file line limit

## Context

The RV64 pmap cleanup split a large board pmap file into responsibility-shaped
modules, moved private unit tests into a sibling test module, and added
top-of-module plus group-level comments. That workflow is reusable enough to
capture as an agent skill, and the same cleanup exposed a preventable failure
mode: authored Rust source files should not grow back toward multi-thousand-line
modules.

## Decision

Add `.agents/skills/tx-code-reorganization/SKILL.md` and register it in the
skill manifest. The skill guides behavior-preserving module splits, facade
movement from `foo.rs` to `foo/mod.rs`, function ordering by
data-structure/lifecycle/helper groups, group-level comments, test movement,
and verification.

Extend `cargo xtask lint arch` with a hard size check for authored Rust source
files: `.rs` files outside `target/` and `external/` must stay at or below
1,500 lines. The rule also checks `xtask` source files before architecture-only
lint rules skip them.

The line limit is scoped to Rust source for now. Some active design Markdown
specs are intentionally larger than 1,500 lines and need separate docs-focused
splitting decisions instead of being blocked by the code reorganization lint.

## Verification

- `cargo fmt`
- `cargo test -p xtask`
- `cargo xtask lint arch`
- `cargo xtask progress validate`
- `cargo xtask ci`
- `git diff --check`

## Next Step

Use `tx-code-reorganization` before future large-module splits, and split any
new Rust file before it exceeds the lint limit.

## Blockers

No blocker. A future docs cleanup can decide whether active Markdown specs need
a separate size/readability policy.
