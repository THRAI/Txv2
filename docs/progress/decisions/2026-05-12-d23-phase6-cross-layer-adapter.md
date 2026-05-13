# D23 — Phase 6 adapter migration (tx-shims, tx-kernel, tx-ext4, tx-scripts)

Date: 2026-05-12
Status: landed (this branch)

## Why

D22 closed phase 5 (tx-fs). Phase 6 covers the four cross-layer
consumer crates from the refactor plan: `tx-shims` (syscall arms),
`tx-kernel` (init / trap_handoff / thread_future), `tx-ext4` (FsOps
backend), and `tx-scripts` (exec script). ~250 substrate lines and
~24 reactor lines combined.

**First phase to migrate reactor refs** — earlier subsystems used the
reactor only via `tx_reactor::wait::{Channel, Mask}` (already wrapped
by the stacked `wait_routing` adapters). tx-kernel's init.rs reaches
deeper: `tx_reactor::{hart_loop, userspace, HartId, SharedReactor,
InitialSchedMeta, RescheduleSignal, ast}`. Phase 6 introduces a
new `boot_runtime` domain in `tx-kernel`'s adapter to wrap these.

## What changed

### Per-crate dependency edits

All four crates' `Cargo.toml` gain
`tx-platform-adapter = { path = "../tx-platform-adapter" }`.

### Per-crate adapters

* **`crates/tx-shims/src/adapter.rs`** — `step_engine` (substrate)
  with standard re-exports + `sign_zone_for`; `reactor_entry`
  (reactor) wrapping `tx_reactor::userspace`. tx-shims is mostly a
  thin syscall-arm layer; substrate use is dominated by
  `epoch::guard()` calls (50 in production) and step_v3 outcome
  types.
* **`crates/tx-kernel/src/adapter.rs`** — `step_engine` (substrate)
  + `boot_runtime` (reactor). The richest reactor adapter so far,
  wrapping the BSP / AP startup primitives: `HartId`, `hart_loop`,
  `userspace`, `wait`, `SharedReactor`, `InitialSchedMeta`,
  `RescheduleSignal`, `ast`.
* **`crates/tx-ext4/src/adapter.rs`** — `step_engine` only.
  tx-ext4's surface mirrors tmpfs but adds page_allocator
  primitives.
* **`crates/tx-scripts/src/adapter.rs`** — `step_engine` only.
  Smallest surface — the exec script uses only the standard step
  engine types + EBR guard.

### Production migration

22 production files migrated (tx-kernel 7, tx-shims 12, tx-ext4 2,
tx-scripts 1) via the established bulk-sed + per-file-import
workflow. Reactor migration script added for tx-kernel's
`tx_reactor::hart_loop / wait:: / userspace / HartId /
SharedReactor / InitialSchedMeta / RescheduleSignal / ast`
patterns.

## Boundary-report delta

```
                            baseline  after p5   after p6   cumulative Δ
substrate outside (lines)     2547      1621      1444       −1103 (43%)
substrate outside (files)      164       156       152         −12
substrate inside  (lines)        0        72        92        +92
substrate inside  (files)        0        12        16        +16
reactor   outside (lines)       72        61        37         −35 (49%)
reactor   outside (files)       41        34        30         −11
reactor   inside  (lines)        0         8        10        +10
reactor   inside  (files)        0         6         8         +8
adapters declared                0        24        30        +30
```

**First phase where reactor outside-adapter ratchet moves
appreciably** (−24 lines on phase 6, plus −11 cumulative across
earlier phases). The 49% reactor reduction is bigger in percentage
terms than the 43% substrate reduction at this point.

**Bundling ratio**: 177 substrate + 24 reactor outside lines
removed, 20 substrate + 2 reactor inside lines added → 9:1. Lower
than trait-signature-heavy phases (tty 38:1) because tx-shims has
many `epoch::guard()` callsites that don't bundle.

## Verification

- `cargo build --workspace --exclude <board-kernels>` — clean.
- `cargo test -p tx-shims --lib` — 233 / 233 pass.
- `cargo test -p tx-kernel --lib -- --test-threads=1` — 43 / 43 pass.
- `cargo test -p tx-ext4 --lib` — 7 / 7 pass.
- `cargo test -p tx-scripts --lib` — 47 / 47 pass.
- `cargo test -p tx-subsystems --lib -- --test-threads=1` — 623 /
  623 still pass.
- `cargo test -p tx-fs --lib` — 39 / 39 still pass.
- `cargo xtask lint arch` — ok.
- `cargo xtask lint docs` — ok.
- `cargo xtask boundary-report` — shows 30 declared adapters
  across 6 crates.

## Pattern notes

* **lib.rs without `use` statements** (tx-shims): my inject script
  bailed because it picks the last `^use ` line. Workaround: add
  the import manually. Could automate by falling back to inserting
  after `pub mod adapter;`.
* **sed `'1a ...'` against doc-comment header**: inserted in the
  middle of `//!` outer-doc-comments, which fragmented them and
  triggered E0753. Fix: insert after the first real `use` line,
  not after line 1.
* **Per-prefix sed substitution risk**: my `\bzone::reserve_for`
  pattern matched `tx_substrate::zone::reserve_for` and produced
  the invalid `tx_substrate::step_engine::reserve_for` path. Worth
  preferring more anchored patterns (e.g. require the full
  `tx_substrate::zone::` prefix and replace it entirely) for
  cross-crate migrations.

## Next step

Phase 7 from the refactor plan: **tests sweep**. Every adapter
file's `outside_adapter (files)` count is currently inflated by
the test code that still does direct substrate imports
(`#[cfg(test)] use tx_substrate::step_v3::{Errno as V3Errno, ...};`).
Migrating those is purely mechanical — the same adapter re-exports
already cover the types tests need. Estimated drop:

* substrate outside-files: 152 → ~50 (only `tx-substrate` itself
  and a handful of legitimate exemptions)
* substrate outside-lines: 1444 → ~300 (the long tail of single-
  call sites)

After phase 7 we should be in striking distance of the eventual
CI gate (phase 8).

## Blocker

None.
