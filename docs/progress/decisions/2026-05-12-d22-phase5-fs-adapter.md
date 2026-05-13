# D22 — Phase 5 adapter migration (tx-fs: tmpfs, devfs)

Date: 2026-05-12
Status: landed (this branch)

## Why

D21 closed phase 4 (page_backed + vm). Phase 5 covers the FS layer
in the separate `tx-fs` crate — tmpfs (136 lines, the single biggest
substrate offender on entry) and devfs (59 lines). This is the
first **cross-crate** adapter migration: every earlier phase lived
inside the `tx-subsystems` crate, so the migration tested whether
`#[platform_adapter]` works across crate boundaries (it does — the
attribute is a proc-macro, the consumer crate just needs
`tx-platform-adapter` as a dependency).

## What changed

- `crates/tx-fs/Cargo.toml` gains
  `tx-platform-adapter = { path = "../tx-platform-adapter" }`.
- `tx-fs/src/tmpfs.rs` → `tmpfs/mod.rs`; new
  `tx-fs/src/tmpfs/adapter.rs` with one `step_engine` domain
  re-exporting step_v3 outcome types, zone role types, epoch
  `Guard` / `guard`, `SpinMutex`, and the standard `sign_zone_for`
  / pass-through verbs.
- `tx-fs/src/devfs.rs` → `devfs/mod.rs`; analogous
  `devfs/adapter.rs` with the same shape (devfs uses a subset —
  no SpinMutex, no Errno builders — but the adapter mirrors tmpfs
  for consistency).
- Production substrate references in `tmpfs/mod.rs` and
  `devfs/mod.rs` migrated via the established bulk-sed +
  per-file-imports workflow.

## Fix: `#[allow(unused_imports)]` arch-lint conflict

Phase 4 added `#[allow(unused_imports)]` markers to tty files so
cargo-fix wouldn't strip test-only imports reached via `use
super::*;`. The existing `lint arch::unused_allowance` rule
(D10/D12 policy) rejects these — undocumented `#[allow(unused…)]`
hides stale boot/API surfaces.

The correct fix is `#[cfg(test)]` on the import itself: in
non-test builds the import doesn't exist, so there's no unused
warning to allow; in test builds the import is present and the
test-only consumers in `use super::*;` find it. Applied to all 5
affected tty files (step_read, step_master_close, step_hangup,
step_ingest, step_poll_hardware, step_ioctl). Arch lint green.

Also trimmed unused `StepOp` / `SubjectIdentity` from three more
files (vm/structure/range_lock.rs, vm/execution.rs,
tty/structure/identity.rs) that didn't actually need them — the
bulk-inject had been over-inclusive. Now zero unused-import
warnings in tx-subsystems lib.

## Boundary-report delta

```
                            baseline  after p4   after p5   cumulative Δ
substrate outside (lines)     2547      1813      1621       −926 (36%)
substrate outside (files)      164       156       156          0
substrate inside  (lines)        0        62        72        +72
substrate inside  (files)        0        10        12        +12
reactor   outside (lines)       72        61        61        −11
reactor   outside (files)       41        34        34         −7
reactor   inside  (lines)        0         8         8         +8
reactor   inside  (files)        0         6         6         +6
adapters declared                0        22        24        +24
```

Phase 5 ratio: 192 outside removed / 10 inside added ≈ 19:1, again
because tmpfs's substrate use is dominated by trait-signature types
(StepOutcome, NoProgress, etc.) that all collapse cleanly to bare
adapter re-exports.

The outside-file count is unchanged at 156 because both tx-fs files
retain test-only substrate references; phase 7 will pull those out.

## Verification

- `cargo build -p tx-subsystems -p tx-fs` — clean, zero warnings.
- `cargo test -p tx-fs --lib` — 39 / 39 pass.
- `cargo test -p tx-subsystems --lib -- --test-threads=1` — full
  host suite 623 / 623 still passing, 11 ignored.
- `cargo xtask lint arch` — ok (after the `#[allow(unused_imports)]`
  cleanup).
- `cargo xtask lint docs` — ok.
- `cargo xtask boundary-report` — shows 24 declared adapters
  across two crates (tx-subsystems × 10, tx-fs × 2).

## Pattern notes

* **Cross-crate adapter works as expected.** The attribute proc-
  macro lives in `tx-platform-adapter`; any crate that wants
  `#[platform_adapter]` just adds it as a dependency and writes
  its own per-subsystem adapter modules. The boundary-report
  scanner recognises adapters via the literal attribute regardless
  of crate.
* **`#[cfg(test)]` on imports** is the right shape when the
  consumer is test-only. `#[allow(unused_imports)]` looks like a
  shortcut but conflicts with the workspace's anti-dead-code
  policy.

## Next step

Phase 6 from the refactor plan: cross-layer scaffolds —
`tx-shims`, `tx-kernel`, `tx-ext4`, `tx-scripts`. Combined ~250
substrate lines. These are mostly thin call sites that *consume*
adapters from below rather than defining their own, but each
crate still needs `tx-platform-adapter` as a dependency, and a
small adapter module for the few cases where the consumer code
directly mints substrate primitives (e.g. `tx-kernel/init.rs`'s
`Mask::from_bits(...)`).

## Blocker

None.
