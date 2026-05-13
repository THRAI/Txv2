# D49 — Phase 8: Boundary Ratchet CI Gate

**Date:** 2026-05-13
**Scope:** xtask/src/{lint,boundary_report,ci}.rs
**Depends on:** D24-D48 Phase 7 migration (substrate refs routed through per-crate `#[platform_adapter]` modules)

---

## 1. Context

Phase 7 drove `tx_substrate::*` outside-adapter line count from 1446 to
208 and `tx_reactor::*` from 37 to 4. Without a CI gate, a single new
direct-substrate call site introduced in a future PR would silently
erode that work. Phase 8 locks in the achieved numbers as ceilings.

---

## 2. What landed

- `xtask/src/boundary_report.rs` factored out `scan_workspace()` and
  added `pub(crate) outside_adapter_totals(root) -> (usize, usize)` so
  the lint command can reuse the scanner.
- `xtask/src/lint.rs` adds `lint boundary`:
  - constants `MAX_SUBSTRATE_OUTSIDE_ADAPTER = 208` and
    `MAX_REACTOR_OUTSIDE_ADAPTER = 4`
  - returns non-zero with a directive message when either threshold is
    exceeded; the error tells the offender to either route through an
    existing `#[platform_adapter]` module or add a new one
- `xtask/src/ci.rs` wires `cargo xtask lint boundary` into the `ci`
  command sequence with `txdoc:CI-GATE-BOUNDARY-RATCHET`.

---

## 3. Ratchet semantics

The two ceiling constants act as a one-way ratchet:

- **Lowering** (substrate < 208 or reactor < 4) is a routine outcome of
  further convergence work — burn down test-bootstrap residue or
  inline-adapter inflation, lower the constant, commit.
- **Raising** requires an explicit decision note. The error message
  reminds the regressing author that the ceiling is not a "fix the
  number to compile" knob. If a legitimate new platform reach is
  introduced (e.g. a new substrate sub-API like `shootdown` that no
  existing adapter exposes), the right answer is a new adapter module
  with `#[platform_adapter(platform = "substrate", domain = "...", ...)]`
  so the call site lands in `inside_adapter` instead of crossing the
  ceiling.

---

## 4. Verification

```
$ cargo xtask lint boundary
Architecture Boundary Ratchet
=============================
substrate outside adapters:  208 lines  (ceiling 208)  ok
reactor   outside adapters:    4 lines  (ceiling 4)  ok
```

The gate passes at the exact ceiling because Phase 7 stopped at the
test-bootstrap floor (`tx_substrate::testing::init_host_for_test_once`
and `tx_substrate::epoch::drain_with_budget` are the bulk of the 208
residual lines; the 4 reactor lines are similar).

---

## 5. Future burndown candidates

The 208-line floor is dominated by:

1. **Test bootstrap (~140 lines):** `init_host_for_test_once` and
   `drain_with_budget`. Could be routed through a workspace-shared
   `tx-test-support` crate with its own adapter — but the value/cost
   trade-off favours leaving them where they are; they aren't
   production reach.
2. **Inline `mod adapter` definitions (~25 lines):** the inline
   adapters in `aio.rs`, `signalfd.rs`, `userfaultfd.rs`, `io_uring.rs`,
   `reactor_submit.rs`. boundary-report counts the `pub use
   tx_substrate::*;` lines inside those modules. The simplest reduction
   is to move each inline adapter into a sibling `adapter.rs` so the
   scanner sees the file is the adapter rather than treating each ref
   as outside. Mechanical; no semantic change.
3. **Doc-comment paths (~10 lines):** rustdoc intra-doc links like
   `[\`tx_substrate::step_v3::StepOutcome\`]` that survived the
   migration. Replace with bare names where possible.

Lowering the ceiling to ~80 (just test bootstrap) is plausible once
items 2 and 3 land.

---

## 6. Acceptance

- [x] `cargo xtask lint boundary` passes at the locked numbers.
- [x] `cargo xtask ci` includes the boundary gate.
- [x] Usage banner mentions `boundary`.
- [x] ADR documents the ratchet semantics so a future regressor sees
      the policy before reaching for the constants.
