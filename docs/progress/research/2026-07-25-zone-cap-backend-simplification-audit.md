# Zone/Cap Backend Simplification Audit

Date: 2026-07-25

## Scope And Checkout State

This is a read-only architecture and hot-path audit for making identity reads
RCU-backed over the existing Zone/EBR substrate. It examines the current dirty
worktree, not only `HEAD`. The worktree already contains an uncommitted
fixed-depth `SlabDirectory`, lock-free registry reads, four-state slot metadata,
intrusive per-CPU retirement bags, and slab EBR retirement. Those changes are
preserved as existing work and were not modified by this audit.

## Decision

The identity pilot should use an owner-private pointer-published binding:

```text
caller-owned Guard
  -> AtomicPtr<Slot<T>> Acquire load
  -> Live metadata validation
  -> IdentRef<'g, T>
  -> repeat for intermediate bindings under the same Guard
  -> one terminal retain CAS
  -> Cap<T>
```

The binding writer retains the published object with an owner-side `Cap<T>`.
Replace installs the next owner evidence before publishing the next pointer and
drops the old owner evidence only after the publication linearization point.
Withdraw publishes null before dropping the old owner evidence. If a reader
loads an old pointer whose slot has become `Retiring`, it reloads the published
pointer and retries when it changed. A still-published `Retiring` pointer is an
invariant failure.

This private `AtomicPtr` is safe because a reader holds a Guard before loading
it. Final `Cap` drop cannot reclaim or reuse the pointed-to slot until all such
guards quiesce. A publication token therefore does not need to repeat
`SlotKey + generation` merely for physical ABA protection; the reader obtains
the current generation from `SlotMeta`. Semantic revocation remains an owner
revision/dead/withdrawn check and is not supplied by EBR.

## Current Cost Shape

The current `Cap<T>` stores one raw `u32 SlotKey`. Every `Cap::deref`, clone,
drop, downgrade, `ident_ref`, trace-id read, and retain-count read resolves the
key again. The current resolution path is:

```text
Cap.raw
  -> registry published Acquire
  -> zone-id and TypeId checks
  -> erased resolver function call
  -> Zone::id Acquire and repeated zone-id check
  -> directory root/leaf/slab Acquire loads
  -> slab-id and slot-index checks
  -> Slot<T>
```

Clone then performs a retain CAS. Non-final drop performs a decrement CAS.
`Weak::observe` adds a generation/live metadata load, while `Weak::upgrade`
currently performs that load and then `IdentRef::to_cap` loads and validates
the same metadata again before its retain CAS.

The directory path is now fixed-depth and lock-free in this worktree, so it is
no longer the old Keg-lock/slab-list algorithm. Historical post-O(1) profiles
still attributed about 19 percent of the fork-spin sample to Zone operations
and characterized it as call-volume-bound. The identity pilot should therefore
remove repeated operations from readers rather than add another key cache.

## Ranked Simplifications

| Priority | Change | Expected effect | Risk / condition |
|---|---|---|---|
| P0 | Private pointer-published identity binding, one caller-owned Guard, terminal-only `to_cap` | Removes binding lock, intermediate Cap clone/drop, directory resolution, and retain RMWs | Needs replace/withdraw, retry, raw-key-zero, ABA, and SMP witnesses |
| P1 | Fuse `Weak::upgrade` into one metadata CAS loop after one resolution | Removes the duplicate stable-path metadata load/check for owned upgrades that remain | Keep the conceptual `Weak -> IdentRef -> Cap` guarded bridge and metrics attribution |
| P1 | Add a trusted typed registry fast path after the existing zone/type validation | Removes erased callback, repeated `Zone::id` Acquire/check, and repeated key decode | Measure generated code first; keep forged/stale-key rejection at the closed constructor boundary |
| P2 | A/B test pointer-backed `Cap<T>` | Makes clone/drop/deref direct SlotMeta/slot access and makes terminal `to_cap` return a true direct smart pointer | `Cap` grows from 4 to 8 bytes on 64-bit targets; dense `Vec<Cap>` and `Index<..., Cap<_>>` storage grows |
| P3 | Prove weaker retain/drop memory orderings or split retain metadata | May reduce RISC-V atomic ordering cost | Requires a complete publication/final-drop proof; exact `retain_count()` users forbid approximate accounting |
| P3 | Remove lazily derivable raw key from `IdentRef` after pointer-backed Cap | Avoids key reconstruction on the guarded read path | No 64-bit size win while generation remains; only useful as instruction-count cleanup |

### Pointer-backed Cap experiment

The safe candidate is pointer-only, not pointer-plus-generation:

```rust,ignore
pub struct Cap<T: 'static> {
    slot: NonNull<Slot<T>>,
    _marker: PhantomData<T>,
}
```

A live `Cap` retention prevents its slot from becoming free and prevents its
slab from becoming empty or being unpublished. Generation is therefore
redundant in `Cap`; it remains necessary in non-retaining `Weak<T>`. `key()`
can reconstruct the existing stable `SlotKey` from the containing slab and
slot index. Public equality, raw-key, trace-id, `Clone`, `Drop`, `Send/Sync`, and
exact retain-count semantics must remain unchanged.

This is not the first identity patch. It should be an isolated A/B after the
RCU pilot because the pilot removes most strong-reference operations from the
reader hot path. The representation change is justified only if the remaining
ownership/cross-yield path still profiles in `registry::slot_for`.

## Rejected Shortcuts

- Do not create a second public retained handle. `Cap<T>` remains the only
  generic identity-retaining reference; `IdentRef<'g, T>` remains the guarded
  pointer handle.
- Do not cache raw-key lookups per CPU. The current directory is already O(1),
  and another cache adds ABA/invalidation state without removing retain RMWs.
- Do not remove generation from `Weak<T>` or cache a long-lived Slot pointer in
  it. A Weak does not keep either the slot or slab alive.
- Do not use `NonZeroU32` as a transparent Cap optimization. Raw `SlotKey == 0`
  is valid for zone 1, slab 1, slot 0 and has a dedicated regression test.
- Do not make `Cap` `Copy`, make retain counts approximate, or move semantic
  revocation into EBR. Production socket/process/net-namespace paths inspect
  exact retention and rely on deterministic final drop.
- Do not create a fresh Guard in each binding load. Guard construction performs
  CPU pin/accounting, epoch publication, and a sequentially consistent fence;
  multi-hop identity resolution must reuse one caller-owned Guard.

## Pilot Sequence

1. Add substrate-private `PublishedBinding<T>` with writer-owned `Cap<T>` and
   an `AtomicPtr<Slot<T>>` read cell. Expose only guarded `observe`, writer
   replace, and writer withdraw to owner implementations.
2. Migrate `ThreadIdentity.payload`, then publish only the per-hart current
   userspace `ThreadIdentity` owner. A synchronous reader derives
   `ThreadPayload` through the identity-owned binding, observes the existing
   weak thread-owner-process edge under the same Guard, then observes process
   payload and address space. Keep the strong current-userspace payload anchor
   for timer IRQ, where a new Guard is forbidden, and leave poll-scoped current
   tables unchanged in the first patch. Pure synchronous direct arms retain
   nothing; only a real yield/storage/fanout boundary performs a terminal
   retain. Do not make `ThreadIdentity.owner_proc` strong: `ProcessPayload`
   already retains `Cap<ThreadIdentity>`, so a strong reverse edge would form
   a cycle.
3. Fuse the remaining `Weak::upgrade` implementation and measure it separately.
4. Remove the repeated typed resolver checks only if disassembly or counters
   show they survive optimization.
5. Run a pointer-backed Cap A/B only after the pilot trace identifies residual
   strong-reference resolution as material.

## Verification Gates

- Compile-fail: Guard, `IdentRef`, and guarded operational references cannot
  escape into a future, yield result, thread, or stored continuation.
- Race tests: replace, withdraw, old-pointer retry, raw key zero, generation
  reuse, reader-before-final-drop, and old-slot-live-through-another-owner.
- Retain gate: marked direct identity resolvers contain no intermediate
  `Weak::upgrade`, `IdentRef::to_cap`, `IdentitySlot::clone_cap`, or `Cap::clone`.
- Four-hart witness: a reader holds a real Guard while another hart replaces or
  withdraws the binding; early drain cannot reclaim, and bounded drain finishes
  after quiescence.
- Same-configuration `tx-observe` A/B: compare direct-trap current-thread,
  owner-process, address-space, total context, cap-upgrade, and payload-slot-lock
  rows. Historical CAS samples were attempts=1/retries=0, so success is lower
  call count and cacheline traffic, not fewer retries.

## Evidence

| Claim | File:line | Confidence |
|---|---|---|
| Cap is a 4-byte raw SlotKey and resolves every strong operation through the registry | `crates/tx-substrate/src/zone/cap.rs:103`, `:155`, `:247`, `:278`, `:346` | high |
| Weak observation resolves once and checks generation/live; upgrade then repeats metadata validation in `to_cap` | `crates/tx-substrate/src/zone/cap.rs:447`, `:467`, `:539` | high |
| Registry performs type/zone validation then erased dispatch | `crates/tx-substrate/src/zone/registry.rs:376`, `:485` | high |
| Typed resolver repeats the Zone id load/check | `crates/tx-substrate/src/zone/registry.rs:240`, `crates/tx-substrate/src/zone/mod.rs:124`, `:227` | high |
| Directory lookup is fixed-depth root/leaf/slab Acquire traversal | `crates/tx-substrate/src/zone/directory.rs:39` | high |
| A retained slot has a stable slab address and reconstructible key | `crates/tx-substrate/src/zone/slot.rs:41`, `crates/tx-substrate/src/zone/slab.rs:200` | high |
| Empty slabs unpublish before EBR retirement and release their frames only in the slab callback | `crates/tx-substrate/src/zone/keg.rs:144`, `crates/tx-substrate/src/zone/slab.rs:244` | high |
| Raw SlotKey zero is valid and covered | `crates/tx-substrate/src/zone/registry.rs:41`, `crates/tx-substrate/tests/zone.rs:183` | high |
| Active object model requires one Guard and terminal-only retention for binding traversal | `docs/design/00_meta-framework/object_model_v2.md:336` | high |
| RCU/single-binding publication is owner-private and does not provide revocation | `docs/design/00_meta-framework/OBJECT_API_LANES_v1.md:397`, `:450`, `:470`, `:970` | high |
| Guard construction publishes local epoch state and executes a SeqCst fence | `crates/tx-substrate/src/epoch/domain.rs:368`, `crates/tx-substrate/src/epoch/local.rs:200` | high |
| Historical sampled upgrades had one attempt and zero retries | `docs/progress/research/2026-06-02-sigprocmask-tail-audit.md:398` | high |
| Historical direct-trap context recovery spent 1.114 s over 10,007 calls | `docs/progress/research/2026-06-04-sigprocmask-payload-fast-path.md:284` | high |

## Verification Performed

- `cargo test -p tx-substrate --test zone`: 17 passed, 0 failed.
- `git diff --check -- crates/tx-substrate/src/zone crates/tx-substrate/src/epoch crates/tx-substrate/src/slot.rs`: passed.

No runtime source was changed. No fresh performance benchmark was run; the
performance numbers above are historical evidence and the current O(1)
directory/four-state implementation is an uncommitted worktree state.
