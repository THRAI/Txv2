# D18 — Phase 1 adapter migration (mount, futex, cred, signal)

Date: 2026-05-12
Status: landed (this branch)

## Why

D17 proved the `#[platform_adapter]` pattern on `pipe`. Phase 1
replicates it across the four mid-sized single-file subsystems named
in the refactor plan (D17 §"Next step"): `mount`, `futex`, `cred`,
`signal`. Each is single-file or has one already-separated tests
sub-file, which lets the migration use the same `mod.rs` +
`adapter.rs` split the pipe pilot established.

## What changed

Each of the four subsystems gets the same shape:

1. `crates/tx-subsystems/src/<name>.rs` → `<name>/mod.rs` via `git mv`
   (keeps blame history).
2. New `crates/tx-subsystems/src/<name>/adapter.rs` declaring one or
   two `#[platform_adapter]`-marked modules. Naming reflects the
   subsystem's substrate footprint:

   * **mount** — one `runtime` domain. Mount has no step engine or
     wake routing in production; it just needs the zone role types
     (`Cap`, `PayloadCap`, `Entity`, …) and `SpinMutex` for the
     global `MOUNT_TABLE`. The adapter re-exports those and provides
     a `sign_zone_for` verb that bundles `reserve_for` + `sign_for`
     (mirrors pipe's verb).

   * **futex** — two domains, mirroring pipe's shape since futex
     also bridges substrate and reactor. `step_engine` re-exports
     step_v3 types and provides the futex-side
     `yield_until_wake(source_id, mask)` verb (wraps `Yield {
     progress: NoProgress, shape: OnWaitSource { source, interests }
     }`). `wait_routing` stacks substrate + reactor attributes and
     wraps `WaitSource` / `Channel` / `Mask` as
     `new_wait_source` / `fire_legacy_channel` / `notify_v3_source`
     (same names pipe uses — the verbs are structural, not
     subsystem-specific).

   * **cred** — one `step_engine` domain. Substrate-only: cred has
     no reactor surface. Re-exports `CredentialView`,
     `RestrictionStackHandle`, `Guard`, `Cap`, `Zone`, etc. Provides
     `sign_zone_for`. The substrate qualifications on the seven
     `impl StepOp` blocks for the set{uid,gid,reuid,resuid,resgid,
     regid,apply_suid_for_exec}Op mutators were stripped via five
     `replace_all` substitutions.

   * **signal** — one `step_engine` domain. Substrate-only, but
     introduces a new wrap target: `tx_substrate::epoch::guard()`
     calls (8 of them) in production. The adapter re-exports
     `guard` and `Guard` so call sites become `step_engine::guard()`
     and `&Guard<'_>`. Also re-exports `SignalRouting`,
     `OperationalCapExt`, `SpinMutex`, and the step_v3 types used
     by the kill / sigaction `StepOp` impls.

3. Production call sites rewritten to consume the adapter:
   `tx_substrate::*` no longer appears outside `#[cfg(test)]` in any
   of the four `mod.rs` files. Tests retain raw substrate use (phase
   7 work).

## Boundary-report delta

```
                            baseline  after pilot  after phase 1   delta vs baseline
substrate outside (lines)     2547      2515         2420            −127
substrate outside (files)      164       164          164                0  (test refs still flag the file)
substrate inside  (lines)        0        10           27             +27
substrate inside  (files)        0         1            5              +5
reactor   outside (lines)       72        71           70              −2
reactor   outside (files)       41        40           39              −2
reactor   inside  (lines)        0         3            4              +4
reactor   inside  (files)        0         1            2              +2
adapters declared                0         3            9              +9
```

The 5:1 ratio of outside-lines-removed vs inside-lines-added is the
bundling payoff: `reserve_for + sign_for` collapses to one
`sign_zone_for`, the 4-line `Yield { progress, shape: OnWaitSource
{ source, interests } }` collapses to one `yield_until_wake`, and so
on. The wrappers ARE the value, not just an audit anchor.

## Verification

- `cargo build -p tx-subsystems` — clean after each subsystem.
- `cargo test -p tx-subsystems --lib pipe::` / `mount::` / `futex::`
  / `cred::` / `signal::` — 34 + 7 + 16 + 36 + 65 = 158 / 158 pass.
- `cargo test -p tx-subsystems --lib -- --test-threads=1` — full
  host suite 623 / 623 passing, 11 ignored (matches D17 baseline).
- `cargo xtask lint arch` — ok.
- `cargo xtask boundary-report` — shows 9 declared adapters with
  the expected platform/domain/reason manifests.

## Verbs catalogued across phase 0 + phase 1

The shared vocabulary is starting to crystallize. Verbs that
recurred and could plausibly be hoisted into a shared adapter helper
if the pattern keeps repeating:

* `sign_zone_for<T: ZoneAllocated>(value: T) -> Result<Cap<T>, ZoneError>`
  (pipe, mount, cred — three duplicated implementations)
* `new_wait_source(id: u64) -> Arc<WaitSource>` (pipe, futex — two
  duplicated implementations)
* `fire_legacy_channel(channel: &Channel, mask_bits: u64)` (pipe,
  futex)
* `notify_v3_source(source: &Arc<WaitSource>, mask_bits: u64)`
  (pipe, futex)

Not hoisting yet — the plan calls for per-subsystem adapter
ownership, and three duplicates is not a strong enough signal. If
phase 2-3 (vfs, process, tty) reproduce the same verbs without
domain-specific shape, that becomes the trigger to extract a shared
helper crate. Until then, the duplication is the explicit cost of
keeping each adapter audit-anchored to its subsystem.

## Next step

Phase 2 from the refactor plan: the multi-file core subsystems
`vfs/` and `process/`. Combined estimate ~600 substrate lines.
These will exercise multi-file adapter scope for the first time —
the `vfs::adapter::*` and `process::adapter::*` modules will sit
under existing directories rather than triggering a new directory
restructure.

## Blocker

None.
