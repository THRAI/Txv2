# D21 — Phase 4 adapter migration (page_backed, vm)

Date: 2026-05-12
Status: landed (this branch)

## Why

D20 closed phase 3 (tty). Phase 4 is the memory subsystems —
`page_backed/` (parent file plus 6 production submodules) and `vm/`
(11 production sibling files). Combined ~350 substrate lines. VM
also has the most exotic substrate surface in the codebase, the
userfaultfd-delegate plumbing (`DelegateRegistry`, `DelegateRequest`
/ `DelegateReply`, `UfdRequest` / `UfdReply`, `AbortReason`,
`AgentCancelPolicy`, `TokenDropPolicy`, `YieldShape`,
`AddressSpaceShootdownBatch`, `ShootdownError`, `TaskMailbox`), plus
the standard step_v3 / zone / epoch / SpinMutex / page_allocator
set.

## What changed

### page_backed

- `git mv crates/tx-subsystems/src/page_backed.rs page_backed/mod.rs`
- New `crates/tx-subsystems/src/page_backed/adapter.rs` with one
  `step_engine` domain (no reactor surface in production). Bundles
  step_v3, zone, epoch, page_allocator, and SpinMutex, with
  `sign_zone_for` and pass-through `reserve_for` / `sign_for`.
- All 6 production sibling files (`cross_variant`, `fs_page_backing`,
  `lifecycle`, `reflink`, `targeted_read`, `user_buffer`) consume
  the adapter.

### vm

- New `crates/tx-subsystems/src/vm/adapter.rs` with `step_engine`
  (including the full userfaultfd-delegate surface, TaskMailbox,
  shootdown primitives, and page-allocator) and `wait_routing`
  (stacked substrate + reactor) domains.
- `vm/mod.rs` adds `pub mod adapter;`.
- All 11 production files migrated.

### Workflow polish

- The bulk-sed approach established in phase 3 carried over with
  new substitutions for the vm-specific sub-APIs (`shootdown::*`,
  `wake::TaskMailbox`, the full userfaultfd-delegate set).
- Discovered a sharp edge: my `inject` script inserts after the
  last `^use ` line, which fails when the last "use" is inside a
  multi-line `use X::{` block. Wrote a small recovery script that
  detects the broken position (previous line ending in `{`) and
  moves the injected use line above the multi-line block. 5 files
  needed this fix.
- After the bulk migration, `cargo fix --lib` cleaned up 37 of 43
  unused-import warnings. The remaining 6 were imports referenced
  only by `#[cfg(test)]` test modules via `use super::*;` — cargo
  fix couldn't see they were used. Re-added them via
  `#[allow(unused_imports)]` (8 affected tty files) so cargo fix
  doesn't strip them again in future passes.

## Boundary-report delta

```
                            baseline  after p3   after p4   cumulative Δ
substrate outside (lines)     2547      1955      1813       −734 (29%)
substrate outside (files)      164       159       156         −8
substrate inside  (lines)        0        48        62        +62
substrate inside  (files)        0         8        10        +10
reactor   outside (lines)       72        62        61        −11
reactor   outside (files)       41        34        34         −7
reactor   inside  (lines)        0         7         8         +8
reactor   inside  (files)        0         5         6         +6
adapters declared                0        18        22        +22
```

Bundling ratio this phase: 142 outside removed / 14 inside added ≈
10:1. Lower than phase 3 (38:1) because page_backed and vm both
have richer adapter bodies — vm's adapter alone re-exports 20+
distinct substrate types.

## Verification

- `cargo build -p tx-subsystems` — clean, 6 warnings (all in tests,
  all `#[cfg(test)]` paths via `use super::*;` reach into
  test-only-needed imports the lib build can't see).
- `cargo test -p tx-subsystems --lib page_backed::` — 81 / 81 pass.
- `cargo test -p tx-subsystems --lib vm::` — 95 / 95 pass.
- `cargo test -p tx-subsystems --lib -- --test-threads=1` — full
  host suite 623 / 623 passing, 11 ignored.
- `cargo xtask lint arch` — ok (the residual zone references
  flagged earlier turned out to be all role-typed `Cap<T>` /
  `Zone<T>` without `Policy`, which the existing rule already
  permits).
- `cargo xtask boundary-report` — shows 22 declared adapters.

## VM zone-policy audit

Per D20's TODO: verified the `lint arch::raw Zone<T,Policy>` rule
against vm and page_backed residual zone references. All such
references are role-typed (`Cap<T>`, `PayloadCap<T>`, `Weak<T>`,
`Zone<T>` without `Policy` — the upper-layer-allowed form). No
violations, no rule update needed.

## Pattern notes

* **Multi-line `use X::{` blocks** are the next sharp edge for
  bulk-inject scripts. Detection: the line after `last-use-grep`'s
  match ends in `{`. Fix: move the injected line above the
  multi-line block.
* **Tests reaching production imports via `use super::*;`**: cargo
  fix can't see these as used, so it strips them. Mark imports
  with `#[allow(unused_imports)]` if cargo fix would otherwise
  remove them.
* **VM adapter is the richest yet**: 20+ types re-exported. Worth
  considering whether shared sub-API role-bundles (e.g. a common
  `step_v3_full` re-export module in tx-substrate itself) should
  hoist this duplication when the next subsystem with the same
  delegate surface appears.

## Next step

Phase 5 from the refactor plan: FS layer — `tx-fs::tmpfs` (the
single biggest offender at 136 substrate lines) and `tx-fs::devfs`
(59 lines). These live in a separate crate (`tx-fs`), so the
adapter pattern crosses crate boundaries for the first time. Need
to decide: per-crate adapter modules, or hoist a shared
`tx-fs::adapter` mod that both consume.

## Blocker

None.
