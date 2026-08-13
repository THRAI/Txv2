---
date: 2026-07-09
topic: "Object-model boilerplate pattern tax and how much is macro-reducible"
status: draft
scope:
  - crates/tx-subsystems/src/ipc
  - crates/tx-substrate/src/zone
  - crates/tx-platform-adapter
  - docs/design/00_meta-framework/object_model_v2.md
  - docs/design/00_meta-framework/SUBSYSTEM_ANATOMY_v2_1.md
---

# Research: Object-Model Pattern Tax

## Question

The factored object model (zone-backed entities, role-shaped `Cap<T>`/`Weak<T>`/
`IdentRef` types, five-phase step discipline) forces a fixed per-entity skeleton.
How much line count is mechanical transcription vs load-bearing semantics, is it
macro-reducible, and does the design intend it to be hand-written?

## Short Answer

Of a ~5,500–7,000 line pattern tax across tx-subsystems + tx-substrate,
**~2,500–3,200 lines (~40–50%) are mechanical transcription** the design docs
would happily see generated. The role-shaped type surface and the five-phase
commit discipline are load-bearing and mandated; the zone/registration plumbing,
the SysV-IPC triplication, and the adapter re-export blocks are not. No doc
prescribes hand-transcription or forbids a derive macro — and the team already
wrote a partial macro (`stub_zone!`), proving they treat it as macro-able.

## Findings

### 1. Full anatomy of one entity (`ipc/sysv_shm`)

File skeleton (`wc -l`): `structure.rs` 268, `execution.rs` 515, `tests.rs` 391,
`checks.rs` 72, `projection.rs` 36, `mod.rs` 15 — total 1,297. sysv_shm has no
adapter.rs/notification.rs/witness (it borrows
`crate::process::adapter::step_engine` and has zero wait-sources). sem/msg/mq
each add a `notification.rs`; witnesses live in net, not IPC.

The mechanical glue is ~140 of the 268 lines in `structure.rs`:

**(a) Zone statics + `unsafe impl ZoneAllocated`** — `sysv_shm/structure.rs:157-170`:
```rust
static SHM_IDENTITY_ZONE: Zone<ShmSegmentIdentity> = Zone::const_new();
static SHM_PAYLOAD_ZONE:  Zone<ShmSegmentPayload>  = Zone::const_new();
unsafe impl ZoneAllocated for ShmSegmentIdentity {
    fn zone() -> &'static Zone<Self> { &SHM_IDENTITY_ZONE }
}
unsafe impl ZoneAllocated for ShmSegmentPayload {
    fn zone() -> &'static Zone<Self> { &SHM_PAYLOAD_ZONE }
}
```
The only per-entity semantic input across the whole block is one optional line
(`type Policy = PayloadPolicy<Self>` at `sysv_sem/structure.rs:124`).

**(b) `register_zones()`** — `sysv_shm/structure.rs:172-177`: a hand-written fn
per entity listing its types, wired into a 22-entry dispatch in
`zones.rs::register_all()` (`zones.rs:26-49`).

**(c) Registry + accessor set** (~90 lines, `structure.rs:146-268`):
`SpinMutex<BTreeMap<u32, Cap<..>>>` + id counter + `lookup_/register_/
mark_removed/reclaim_/all_/highest_` free fns. A hand-rolled `IndexTable` the
code itself (`structure.rs:6-9`) calls a stand-in "until `AllocIndex` lands."

**(d) Atomic-shadowed identity accessors** — `structure.rs:93-109`:
`perm()/uid()/gid()/key_raw()`, atomics because `Cap<T>` is `Deref`-only
(documented 85-88).

**Real per-entity semantics** (not tax): the `execution.rs` step bodies
(page-container allocation, attach/detach VMA tracking) and the `IpcPerm` model.

### 2. Derive-macro opportunity

The trait (`zone/mod.rs:248-261`) has only two per-type inputs — the type name
and an optional `Policy` (defaulted to `RetainedEntityPolicy<Self>`):
```rust
pub unsafe trait ZoneAllocated: Sized + 'static {
    type Policy: ZonePolicy = RetainedEntityPolicy<Self>;
    fn zone() -> &'static Zone<Self>;
}
```

The team already wrote `stub_zone!` (`process/nsproxy.rs:605-613`), used 5x:
```rust
macro_rules! stub_zone {
    ($t:ty, $z:ident) => {
        unsafe impl ZoneAllocated for $t {
            fn zone() -> &'static Zone<Self> { &$z }
        }
    };
}
```
But it is weak: it takes the zone static `$z` as a parameter, so the caller
still hand-declares the static AND still hand-writes the `register_zone_for`
line. It removes ~2 of ~7 lines/site.

Nothing fundamental blocks a full `#[derive(ZoneAllocated)]`. The obstacles are
minor: (1) Policy variation — handled by `#[zone(policy = payload)]`; (2) zone
naming — a derive mints its own hidden static; (3) registration — currently a
hand-maintained call tree with **no `inventory`/`linkme` anywhere**. A derive
that also submits via `linkme::distributed_slice` collapses `register_all` +
40 `register_zones` fns to one loop.

**Saving (assumption: derive mints static + impl + auto-registers):
~1,500–1,900 lines.** Risk MED — `no_std` needs `linkme` (not `inventory`);
must audit boot-order determinism, since registration is presently a fixed
sequence.

### 3. IPC triplication (shm / sem / msg / mq)

Diffed with renames normalized:

- **checks.rs** (shm 72 / sem 65 / msg 67): `require_can_read/write` +
  `require_owner_or_admin` **byte-identical modulo renames** (~48 of ~52 code
  lines). Only `require_*_exists` differs — shm returns bare `EINVAL`
  (`checks.rs:70-72`); sem/msg add an `EIDRM`-if-removed branch. **~90%
  duplicate.**
- **projection.rs** (shm 36 / sem 33 / msg 42): identical shape; differ only in
  field list and sort key. **~80% duplicate.** (mq projection is a stub.)
- **structure.rs** (shm 268 / sem 199 / msg 217 / mq 141): zone/registry/
  register scaffold ~1:1; identity field cluster + accessors identical. **~50–60%
  duplicate**; payload bodies genuinely differ.
- **notification.rs** (sem 48 / msg 105 / mq 51) — **where unification breaks
  down.** sem has one `changed_channel`; msg has two (`_CAN_SEND`/`_CAN_RECV`,
  `new_wait_channels` returning a 6-tuple, `abort_removed_with_post`). Different
  wait topologies, not renames.

Also: `with_payload!` is defined twice — `sysv_sem/execution.rs:36` and
`sysv_msg/execution.rs:40`, identical but for `$array`/`$queue`.

Consolidation: a `SysvIpcObject`/`HasIpcPerm` trait + generic `IpcRegistry<T>`
unifies identity + registry + perms while keeping payload bodies and
notification.rs bespoke. **Saving ~700–1,000 lines.** Risk MED.

### 4. Adapter re-export blocks

The `pub use tx_substrate::zone::{...}` block appears in **19** adapter.rs files.
Canonical (`process/adapter.rs:43-48`) is a 24-symbol set. Byte-diff: process ≡
signal identical; process vs vfs/cred/io_uring differ only in rustfmt symbol
ordering (same set); timerfd is a subset. So ~15 of 19 export the identical set.

`#[platform_adapter]` is almost a pure marker (`tx-platform-adapter/src/lib.rs:
293-334`): it validates args (platform, snake_case domain, `reason >= 12` chars)
and injects one `#[doc(hidden)] pub const __PLATFORM_ADAPTER_* : &str = "..."`
manifest line, then re-emits the module unchanged. **No structural codegen.**

Why they exist: Rust cannot seal an extern crate, so raw `tx_substrate::` calls
are legal only inside a `#[platform_adapter]` module; `cargo xtask
boundary-report` flags everything else as `outside_adapter`. The adapter files
are a hand-maintained boundary allowlist, so a shared `substrate_zone_prelude!`
macro (or a common re-export module) replaces the duplication without weakening
the boundary check. **Saving ~250–300 lines.** Risk LOW.

### 5. Is the tax intended? (doc cross-check)

**Mandated semantic surface — do NOT macro away:**
- Five-phase commit discipline — `txdoc:SUBSYSTEM-ANATOMY-FIVE-PHASE-COMMIT-1`
  (§2) + `txdoc:SUBSYSTEM-ANATOMY-EXECUTION-MODULE-1`, grounded in invariant
  STEP-4 (observe -> upgrade -> reserve -> commit -> publish).
- Role-shaped types — `txdoc:SUBSYSTEM-ANATOMY-ZONE-TYPE-MANIFEST-1` +
  object_model §2.5 `txdoc:OBJECT-MODEL-UPGRADE-BRIDGE-1`: upper language says
  "this is a process identity," not "`Zone<T, RcPolicy>`."
- Witness discipline — `txdoc:SUBSYSTEM-ANATOMY-CHECKS-MODULE-1` + WIT-3.
- Four-module layout — `txdoc:SUBSYSTEM-ANATOMY-FOUR-MODULE-LAYOUT-1` (the file
  split is mandated; the duplicated content within is not).

**Merely mechanical transcription — docs would welcome codegen:**
- object_model §7.3 `txdoc:OBJECT-MODEL-EVIDENCE-DERIVATION-1` literally uses
  derive language: `derive(Addressability, T) = Cap<T>`.
- The zone static + impl + `register_zones` is nowhere mandated to be
  hand-written; the SHM registry is explicitly a temporary stand-in.
- No doc prescribes hand-transcription, forbids a derive macro, or forbids
  generic `IndexTable` reuse.

## Applicability To txKernel

Ranked reduction plan (zero change to mandated type surface or five-phase
discipline):

| # | Action | Lines saved | Risk |
|---|---|---|---|
| 1 | `#[derive(ZoneAllocated)]` minting static + impl + `linkme` auto-registration; collapse `register_all` + 40 `register_zones` fns to one loop | ~1,500–1,900 | MED — `no_std` needs `linkme`; audit boot order |
| 2 | Generic `IpcRegistry<T>` + `HasIpcPerm` trait across shm/sem/msg/mq | ~700–1,000 | MED — keep payloads + notification bespoke |
| 3 | Shared `substrate_zone_prelude!` for the 19 adapter blocks | ~250–300 | LOW |
| 4 | Hoist the doubly-defined `with_payload!` macro | ~15 | LOW |

**Total realistically reducible: ~2,500–3,200 lines.** Item 1 is highest
leverage; items 3–4 are near-free quick wins. The 1,591 `Cap<>` sites and 183
`StepOp` transitions are load-bearing semantics, not tax.

## Method / Caveats

- Read-only investigation; no files edited. Duplication percentages are from
  rename-normalized diffs of the named files, not a token-level clone detector.
- Line savings assume the stated codegen approach lands cleanly; the `linkme`
  boot-order audit (item 1) is the main unquantified risk.

## Sources

- Overview: `docs/research/2026-07-09-rust-line-bloat-investigation.md`
- `crates/tx-subsystems/src/ipc/{sysv_shm,sysv_sem,sysv_msg,posix_mq}/`
- `crates/tx-substrate/src/zone/mod.rs`,
  `crates/tx-subsystems/src/process/nsproxy.rs:605`
- `crates/tx-platform-adapter/src/lib.rs`
- `docs/design/00_meta-framework/object_model_v2.md`,
  `docs/design/00_meta-framework/SUBSYSTEM_ANATOMY_v2_1.md`
