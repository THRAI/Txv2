# Chapter 8 — Anonymous memory and copy-on-write

This is the chapter where the shipped code parts ways with the design doc, and the
divergence is the interesting part. The spec (`VM_v1_2.md` §7.3) says private
anonymous frames are tracked *only* via their PTEs — "no back-index." The code
does something better and more surprising: it hangs an **authoritative
per-mapping index** of private pages off each `VmEntry`, the `PrivatePageSet`. It
is a *scoped* rmap that the no-*global*-rmap commitment (Chapter 0) still permits,
and it is what makes copy-on-write correct without Linux's `anon_vma`.

The tutorial follows the code.

## Anonymous memory, and what a write fault needs

Anonymous memory — the heap, the stack, `malloc`'s big allocations,
`MAP_ANONYMOUS` — is backed by no file. Its recipe carries `VmEntryBacking::
PrivateAnon`. A read fault yields zeros; a write fault yields a private, writable
frame.

The read case is cheap and shared: a read fault on untouched anonymous memory
installs the system's shared zero-content frame, read-only
(`page_allocator/mod.rs:445`, the permanent zero frame, claimed once at boot and
never freed). A thousand processes reading untouched heap all point at the same
zero frame. The first *write* is where the work — and the bookkeeping problem —
begins: that page must now have its own private, writable physical frame, and the
kernel must remember which frame, so a *second* fault on the same page finds the
written data rather than re-zeroing it.

Where does that "which frame" live? In Linux, in the page tables (plus `anon_vma`
for the reverse direction needed by swap). In txKernel, with no swap and no global
rmap, it lives in the `PrivatePageSet`.

## The `PrivatePageSet`: a second authoritative binding

```rust
// vm/structure/private.rs:196
pub struct PrivatePageSet {
    pages: VmSpinMutex<PrivatePageTree>,   // a treap, keyed by VmPageOff
    key_base: u64,
}
```

It is attached to a `VmEntry` through the `owners.private` cap you met in Chapter
2 (`VmCap<PrivatePageSet>`), and it is **zone-allocated** so the cap is a cheap
refcount bump on `VmEntry::clone`. Two design choices in it matter:

**Keyed by `VmPageOff`, not absolute VA.** A `VmPageOff` (`private.rs:38`) is the
page offset *within the entry's range* — page 0 is the entry's first page. This is
deliberate (`private.rs:11`): absolute-VA keying breaks under `munmap`+`mmap`-same-
address, `MAP_FIXED`, `mremap`-move, and `VmEntry` split/merge. Offset keying
stays stable when the mapping moves.

**The contents are authoritative, not cache.** This is the line that contradicts
the spec, stated plainly in the source (`private.rs:21`):

> Private contents are **authoritative**, not cache: after a write, the bytes are
> unrecoverable from any other source.

A file page can be dropped and re-fetched from disk — it is a cache. A private
anonymous page that has been written *cannot* be reconstructed from anything; the
`PrivatePageSet` is the only place it is recorded. So it is a binding in the
Chapter 2 sense — a source of truth — and it lives at the bottom of the factoring
table from Chapter 2: a *second* authoritative binding, hung off a `VmEntry`,
distinct from the recipes tree.

The frame is kept alive by a `CachePin` held in each `PrivateFrame`
(`private.rs:63`) — the `cache_ref` disjunct of `Frame.payload_live` (Chapter 2).
The `MapPin` (the mapped disjunct) is acquired separately when the PTE is
installed. So a private page is alive by `cache_ref` *because the set retains it*,
independent of whether it is currently mapped — which is exactly why a second
fault finds it.

## The CoW state machine

Each entry in the set is a `PrivateFrame` with a state (`private.rs:48`):

```rust
pub enum PrivateFrameState {
    Exclusive,   // this mapping owns the frame outright; a writable PTE is fine
    SharedCow,   // read-aliased with a fork relative; first writer must copy
}
```

The transitions are the whole of CoW:

```
        write fault, missed the set          fork
   ∅ ───────────────────────────────▶ Exclusive ──────────▶ SharedCow
        (install_if_absent)                  ▲   fork_share      │
                                             │                   │ write fault
                                             └───────────────────┘
                                               replace_if_match
                                              (copy + re-own)
```

- **`∅ → Exclusive`** — a write fault that found no entry at this offset allocates
  a fresh private frame and publishes it with `install_if_absent` (`private.rs:945`),
  a CAS that loses gracefully if a concurrent write beat it to the same offset.
  New frames are always `Exclusive`.
- **`Exclusive → SharedCow`** — `fork` (below) marks every entry `SharedCow` and
  shares the treap with the child via `fork_share` (`private.rs:1077`).
- **`SharedCow → Exclusive`** — the first writer on either side allocates a new
  frame, copies the shared content into it, and re-owns its side with
  `replace_if_match` (`private.rs:1007`) — a CAS that linearizes against a
  concurrent writer on the same offset, so exactly one copy wins.

Two more operations round out the API: `take_if_match` (`private.rs:1025`) removes
an entry with an identity proof (used by page gifting, Chapter 10), and
`demote_if_match` (`private.rs:1037`) transitions a specific frame to `SharedCow`.
`split` (`private.rs:1094`) structurally shares the treap when a `VmEntry` is
split by a partial `mprotect`/`munmap`, rebasing offsets — the same persistent-
structure trick the recipes tree uses (Chapter 3).

## `fork`: share the binding, demote the materialization

Now the payoff. `fork` copies neither pages nor recipes-by-value; it shares both
and demotes the hardware:

```rust
// vm/execution.rs:111  (simplified)
pub fn fork_aspace<P: PmapIf>(parent: &AddressSpace) -> Result<AddressSpace, VmMapError> {
    let _full_guard = parent.range_lock
        .acquire_step(full_user_v1(), LockMode::ExclusiveWriter);   // serialize the parent

    let child_recipes = parent.recipes.clone_shared(&guard);        // O(1) tree share (Ch 3)
    let child = AddressSpace::new_with_recipes_for_platform::<P>(child_recipes)?;

    for entry in parent.recipes_snapshot() {
        let private = !entry.flags.shared;
        if let (true, Some(parent_set)) = (private, entry.private()) {
            let child_set = parent_set.fork_share()?;               // share + mark SharedCow
            child.recipes.replace_entry(entry.clone().with_private(Some(child_set)))?;
        }
        if private {
            parent.pmap.protect_range(entry.range, entry.prot.without_write())?;  // demote PTEs RO
        }
    }
    Ok(child)
}
```

Three moves, each a different layer of the split:

1. **Binding (recipes):** `clone_shared` — the child's recipe tree shares every
   node with the parent's, O(1) (Chapter 3).
2. **Binding (private set):** `fork_share` — the child's `PrivatePageSet` shares
   the parent's treap and marks every frame `SharedCow`. Both mappings now
   reference the same physical pages, *recorded as shared*.
3. **Materialization (pmap):** `protect_range(..., without_write())` — the
   parent's PTEs for private ranges are demoted to read-only and shot down. The
   child has no PTEs yet; it will fault them in read-only too.

Now the recipe says "writable private anonymous memory," and the PTE says "read-
only." They disagree **on purpose**. The next write to either side traps (the
recipe permits the write, but the PTE forbids it — `ProtectionViolation`? no: the
fault handler recognizes a write to a `SharedCow` page), allocates a private copy,
flips that page to `Exclusive` via `replace_if_match`, and publishes a writable
PTE with `replace_existing = true`. One page reconciled; every other page stays
shared until it too is written. That is copy-on-write, and it reads cleanly only
because the binding and the materialization were separate things that were
*allowed* to disagree.

> **Note: `fork` serializes the parent.** It takes an `ExclusiveWriter` over the
> *entire* user range (`full_user_v1()`), so every concurrent VM operation in the
> parent waits for the duration. This is a v1 simplification (the spec calls it
> out, §9.5; Linux has the analogous `mmap_lock` write-mode cost). Breaking fork
> into per-range walks is deferred optimization.

## Why this is not the rmap the commitment forbids

The no-global-rmap commitment (Chapter 0) rules out a frame→PTE reverse index
*across all address spaces* — the thing Linux needs for swap and page migration.
The `PrivatePageSet` is not that. It is **per-mapping**: it indexes a single
`VmEntry`'s private pages by offset, scoped to that one mapping, used only by that
mapping's own fault and CoW paths. There is still no way to ask "which PTEs across
the whole system map this frame" — and txKernel never needs to, because there is
no swap to drive an eviction that would ask. The scoped index gives CoW its
authoritative store without reintroducing the global reverse map. Different
problem, different (much smaller) structure.

## Where we go from here

Anonymous memory is the backing whose contents are *authoritative* — born from a
write, recorded nowhere else. Chapter 9 is the other backing: file/tmpfs/shm/
device memory, whose contents *are* a cache (of a file, of a device), materialized
through the `PageContainer` and the page cache, where the `cache_ref` disjunct
finally gets its full story.

## Source anchors

- `PrivatePageSet`: `crates/tx-subsystems/src/vm/structure/private.rs:196`
- Module doc (authoritative-not-cache; offset keying; the spec divergence): same file, `:1–24`
- `VmPageOff`: same file, `:38`
- `PrivateFrameState` (Exclusive/SharedCow) + transitions: same file, `:48`
- `PrivateFrame` (the `CachePin`): same file, `:63`
- `install_if_absent` / `replace_if_match` / `take_if_match` / `demote_if_match`: same file, `:945, 1007, 1025, 1037`
- `fork_share` / `split` / `lookup` / `len`: same file, `:1077, 1094, 935, 1065`
- `fork_aspace`: `crates/tx-subsystems/src/vm/execution.rs:111`
- Shared zero frame: `crates/tx-substrate/src/page_allocator/mod.rs:445`
- Spec divergence point: `docs/design/03_memory-vm/VM_v1_2.md` §7.2–7.3 (`txdoc:VM-7-3-PRIVATE-FRAMES-HAVE-NO-BACK-INDEX`); CoW plan `docs/progress/decisions/2026-05-12-pc-cow-implementation-plan.md`
- Fork serialization caveat: `docs/design/03_memory-vm/VM_v1_2.md` §9.5
