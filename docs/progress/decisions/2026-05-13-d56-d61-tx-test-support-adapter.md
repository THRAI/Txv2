# D56–D61: tx-test-support adapter — test-bootstrap boundary burndown

**Date:** 2026-05-13
**Commits:** D56 (038f50e) → D60 (939702c); D61 ratchet lowered in this session

## Context

After Phase 7 (D17–D48) routed all production substrate/reactor refs through
`#[platform_adapter]` modules, and D49 installed the initial ratchet at 208,
D50–D55 drove the ceiling to 199. The remaining 199 outside-adapter lines
broke down as:

- 108 lines: `tx_substrate::epoch` (EBR guard, drain — partially in adapters)
- 87 lines: `tx_substrate::testing` (init_host_for_test_once) and double
  `drain_with_budget` in test setup functions scattered across ~87 test files

The 87 testing-API lines were the straightforward burndown target: a dedicated
adapter crate would wrap them behind a `#[platform_adapter]` module, and the
workspace test files would be migrated to consume `tx_test_support::*`.

## Decision

Create `crates/tx-test-support` — a lightweight no-std adapter crate with a
single `#[platform_adapter(platform = "substrate", domain = "step_engine")]`
module that exposes three verbs:

- `init_host()` — calls `tx_substrate::testing::init_host_for_test_once()`
- `drain_to_quiescence()` — loops with two consecutive zero-reclaim passes
  before returning (correct quiescence check; superior to a bare double drain)
- `drain_once_unbounded()` — returns `EpochSummary` (alias for `DrainStats`)
  for busy-wait loops that need a single drain per iteration

## Rationale

- **Symmetry with production adapters.** All production substrate entry points
  are already behind `#[platform_adapter]`. Test helpers deserve the same
  boundary discipline so the ratchet has teeth for test code too.
- **`drain_to_quiescence` semantics.** The double-drain pattern
  (`drain_with_budget(usize::MAX)` × 2) is a cargo-cult approximation of
  quiescence. The wrapper loop (drain until two consecutive zero-reclaim rounds)
  is both cleaner and provably correct; migrating all call sites improves
  semantics uniformly.
- **`drain_once_unbounded` for busy-wait loops.** A handful of test helpers
  (`vm/tests.rs` `wait_for_counting_pmap_counters`) loop checking a condition
  and drain once per iteration. These get `drain_once_unbounded()` to preserve
  intent; `drain_to_quiescence()` would busy-loop past the condition.

## Migration scope (D56–D60)

| Commit | Scope | Files |
|--------|-------|-------|
| D56 | Create tx-test-support crate | 3 new files (Cargo.toml, lib.rs, adapter.rs) |
| D57 | tx-subsystems lib tests (src/**) | ~15 files |
| D58 | tx-shims/tx-fs/tx-kernel/tx-ext4/tx-scripts lib tests | ~20 files |
| D59 | tx-subsystems integration tests (tests/) | 16 files |
| D60 | tx-shims integration tests (tests/) | 10 files |

## Outcome

- Substrate outside-adapter lines: 199 → 23 (−176)
- Ratchet ceiling lowered: 199 → 23 (D61)
- All migrated test suites verified green before each commit
- `cargo xtask lint boundary` passes at ceiling 23

## Residual 23 lines

The remaining 23 outside-adapter lines are in `crates/tx-reactor/` (agent_reply,
mailbox, timer, wait_source, hart_loop, integration tests). These use
`tx_substrate::epoch` directly and require a dedicated tx-reactor EBR adapter
pass — out of scope for this burndown round.
