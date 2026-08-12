# Chapter 2 — Binding and materialization: the split

This is the chapter the series is built around. Chapter 1 showed you the three
fields of an `AddressSpace`; now we name the idea that shapes them, follow it
down through three levels of the VM, and — most importantly — work through *why
it exists*. If you read the [VFS series](../vfs/02_payload-identity-split.md),
this is the same split (identity ⟂ payload) applied to memory; the connection is
made explicit at the end.

## The one idea, stated once

> **Authoritative binding** is the source of truth: a fact the kernel *decides*
> and records. **Derived materialization** is everything justified *by* that
> truth: caches, hardware structures, copies — none of which may outlive or
> contradict the binding that justifies it.

This is a kernel-wide principle in txKernel — **ARCH-5**, the publication rule
(`INVARIANTS_v4.md`). It is the same rule the VFS uses to separate an inode's
identity from its page cache. In the VM it appears three times, at three scales,
and learning to see all three is the point of this chapter.

## The split at three levels

| Level | Authoritative binding | Derived materialization | The divergence that motivates it |
|---|---|---|---|
| **AddressSpace** | `recipes`: `(VA range) → VmEntry` | `pmap`: PTEs | rewrite the binding, discard the PTEs (`mprotect`, `munmap`, `fork`) |
| **VmEntry** | the recipe value (`VmEntryBacking`, offset) | `owners`: `Cap<PageContainer>` / `Cap<PrivatePageSet>` | the binding is immutable + cheap to clone; the payload caps reclaim on their own schedule |
| **Frame** | the `FrameMeta` identity slot | `map_count ∨ cache_ref` (∨ `pin_count`) | a page can be unmapped-but-cached, or cached-out-but-pinned |

We will take them top to bottom. Each row is a real pair of types in the code,
not an analogy.

### Level 1 — AddressSpace: recipes are truth, PTEs are cache

You met this in Chapter 1 as the justification invariant. Restated as the split:
the `recipes` tree is the *authoritative binding* of the whole address space, and
the `pmap` is its *derived materialization*. A PTE is never the truth of a
mapping; it is a cache of a decision the recipe already made.

The payoff is mechanical. To change permissions on a range, txKernel does **not**
walk the page table flipping bits in place. It rewrites the recipe (the truth)
and *throws the PTEs away* (`mprotect`, Chapter 7/10); the next fault rebuilds
them from the new truth. To `munmap`, it withdraws the recipe and tears the PTEs
down. To `fork`, it shares the recipe and demotes the PTEs to read-only. In every
case the binding is edited and the materialization is rebuilt — never surgically
mutated. *Because a PTE carries no truth, discarding one is always safe.*

### Level 2 — VmEntry: the binding splits from its payload caps

Open a `VmEntry` and the split recurs inside it:

```rust
// vm/structure/types.rs:391
pub struct VmEntry {
    pub range: UserRange,
    pub prot: Prot,
    pub flags: VmEntryFlags,
    backing: VmEntryBacking,                 // ← the binding *value*: no caps
    pub ufd_registration: Option<UfdRegistration>,
    owners: Arc<VmEntryOwners>,              // ← the payload caps live here
}

// vm/structure/types.rs:353  — stored in the tree, Copy, no retention
pub enum VmEntryBacking { None, PrivateAnon, Page { offset: u64 } }

// vm/structure/types.rs:385
struct VmEntryOwners {
    page: Option<VmCap<PageContainer>>,      // the page-cache payload
    private: Option<VmCap<PrivatePageSet>>,  // the CoW payload (Chapter 8)
}
```

The recipe *value* the tree stores is `VmEntryBacking` — a small `Copy` enum that
says "page-backed at offset N" but holds **no capability**. The actual
`Cap<PageContainer>` (the page-cache payload) and `Cap<PrivatePageSet>` (the
private-CoW payload) live behind `owners`, an `Arc<VmEntryOwners>`. Compare
`VmBacking` (`types.rs:343`), the *reconstructed* view that pairs the offset back
with its `VmCap<PageContainer>` for callers that need the payload.

Why bother? Three reasons, all the split:

- The binding value is **immutable and cheap to clone**. Splitting a VMA on a
  partial `mprotect` produces new `VmEntry`s with narrower ranges and the *same*
  backing value — a few words copied, with the heavy payload shared through the
  `Arc`.
- **`fork` shares the `Arc`** for the common case and path-copies only the
  private set that needs to diverge (Chapter 8).
- The payload caps **reclaim on their own schedule**: a `PageContainer` can
  outlive this particular mapping (it is the shared page cache), so it cannot be
  inlined into a binding that comes and goes.

`VmCap<T>` itself is `Arc<Cap<T>>` (`types.rs:273`) — a clonable handle to a
capability, so the same payload can be shared across the split-up pieces of a
mapping without bumping the entity refcount each time.

> **Spec note.** The design doc (`VM_v1_2.md` §4) shows a single `VmBacking` with
> the `Cap` inline. The shipped code split it into the stored value
> (`VmEntryBacking`) plus `owners`. The tutorial follows the code. This is a
> *refinement* of the same idea — an extra turn of the binding/payload crank
> inside the entry itself.

### Level 3 — Frame: liveness is a disjunction of claims

At the bottom, a physical frame. Its identity is a slot in the `FrameMeta` array
(one entry per physical page). Its *payload* — the right to keep the page's
contents alive — is not one refcount but a **disjunction of independent claims**,
packed into a 32-bit word (`page_allocator/frame_meta.rs:219`,
`PAGE_SUBSTRATE_v1.md` §3.1):

```
FrameMeta.state:  refcount(0..10) │ map_count(10..20) │ cache_ref(20..28) │ pin_count(28..32)
```

The headline liveness rule, straight from the object model
(`object_model_v2.md:131`):

```
Frame.payload_live  ⇔  map_count > 0  ∨  cache_ref > 0
```

- **`map_count`** — how many PTEs across all address spaces point at this frame.
  A `MapPin` increments it on PTE install, decrements on teardown.
- **`cache_ref`** — how many page-cache page-indexes include this frame. A
  `CachePin` holds it.
- (`refcount` is generic ownership; `pin_count` is outstanding DMA. They extend
  the disjunction — a DMA-pinned page stays live — but the *headline* is mapped
  ∨ cached.)

This is the **exact** physical-page analog of the VFS inode rule
`payload_live ⇔ nlinks > 0 ∨ open_refs > 0`, with `MapPin`/`CachePin` playing the
roles of `LinkPin`/`OpenPin`. A frame, like an inode, is alive under a
disjunction, and each disjunct is a separately-counted typed pin. "Unmapped but
still in the page cache" (`map_count == 0`, `cache_ref > 0`) is not a special
case — it is one corner of the disjunction, and it is the everyday state of a
file page nobody currently has mapped.

## The reference ladder, reused

The VFS series built a four-rung ladder of reference strengths
(`object_model_v2.md` §4). The VM uses the same ladder, so a reader of either
series transfers the mental model intact:

```
   Weak<T>                 nullable hint, no retention
      │  observe under an epoch guard
      ▼
  IdentRef<'g, T>          epoch-guarded borrow, stack-bound, no retention
      │  pin
      ▼
   Cap<T>                  refcounted pin on IDENTITY; 'static
      │  upgrade payload
      ▼
  T::OperationalEvidence   pins PAYLOAD (a typed pin, or Cap for co-located)
```

In the VM:

- **`Cap<AddressSpace>`** pins an address space's identity (Chapter 1). A process
  holds it; threads share it.
- **`VmCap<PageContainer>` / `VmCap<PrivatePageSet>`** in `owners` are the
  payload-pinning rung for an entry's backing.
- **`MapPin` / `CachePin`** are the typed payload pins on a *frame* — the
  bottom-level operational evidence, each holding one disjunct of
  `Frame.payload_live`.

The ladder's one-way property holds here too: **holding a higher rung entails
every lower rung.** A `MapPin` entails the frame is live; a `Cap<AddressSpace>`
entails the slot is valid. Downgrade is free; upgrade may fail because the thing
died. That asymmetry is what makes lock-free observation safe (Motivation 4).

## Why split? Five motivations

The split costs something — more types, more reasons a thing might not be freed
yet, the discipline that guards must not span `.await`. It pays for itself five
times.

### Motivation 1 — `mprotect`/`munmap` are binding rewrites, not page-table surgery

Changing a range's permissions is, in txKernel, a *recipe rewrite* under a
`RangeLock` writer, followed by tearing down the affected PTEs
(`execution.rs`, Chapter 7). The next fault re-materializes with the new
permissions. No in-place PTE patching, no question of "what if a fault races my
bit-flip" — because the materialization is *discarded*, not edited. A PTE carries
no truth, so destroying one is always safe; the truth is in the recipe, and the
recipe change is the single linearization point.

### Motivation 2 — `fork` demotes materialization and shares the binding

`fork` cannot copy gigabytes. It **clones the recipes root** (cheap: the tree is
structurally shared, Chapter 3), then for each private mapping **demotes the
parent's PTEs to read-only** so the next write traps (`execution.rs:111`,
Chapter 8). One binding, now referenced by two address spaces; the materialization
deliberately disagrees with it (writable recipe, read-only PTE) until a write
fault reconciles one page. CoW *is* the binding and materialization parting ways
on purpose — which only reads cleanly because they are separate things to begin
with.

### Motivation 3 — unmap ≠ evict (the Frame disjunction at work)

When you `munmap` a file mapping, the pages do not leave the page cache — another
process may have the file mapped, or it may be `read` again in a second. In
txKernel this is automatic: `munmap` drops the `MapPin`s (`map_count` falls), but
the `CachePin`s remain (`cache_ref` stays positive), so the frames stay live by
the *other* disjunct. The single overloaded refcount that makes this subtle in
Linux is replaced by two independent counts whose disjunction is the liveness
rule. "Mapped" and "cached" are different claims with different lifetimes, so they
get different counters.

### Motivation 4 — lock-free fault reads, narrow-locked writes

Because the recipes are the authoritative binding and PTEs are derived, *reading*
the binding never needs the write lock. A fault snapshots the recipes tree under
an **epoch guard** — a plain pointer dereference, no atomic RMW, no `mmap_lock`
read side — finds its `VmEntry`, and proceeds (Chapter 3, Chapter 7). Coordination
is needed only at the narrow window where a materialization is *published* against
the binding, and that is exactly what `RangeLock`'s `Materializer` mode scopes
(Chapter 6). The split is what lets the hot path be lock-free: you can read truth
without locking because materialization can't corrupt it.

### Motivation 5 — typed user addresses are coordinates, not authority

A `UserVirtAddr` / `UserRange` is *just a number with a unit* — a coordinate into
an address space. It is never dereferenceable and never confers authority by
itself (`VM_v1_2.md` §"Address boundary policy"). Authority lives in the `Cap`:
to touch a page you resolve the address *through* the recipes (which you reach via
the address space's identity Cap) to a `VmEntry`, then materialize through its
*payload* cap. User bytes cross the boundary only through the `copy_*_user`
helpers that walk recipes page-by-page. The address tells you *where*; the
capability tells you *whether you may*. Keeping those separate is the split
applied to the syscall boundary itself.

## The factoring is a spectrum

The object model's rule (`object_model_v2.md` §8.1.1) is: split where lifetimes
genuinely diverge, co-locate where they do not. The VM uses the whole spectrum,
deliberately:

| Thing | Factoring | Why |
|---|---|---|
| `AddressSpace` identity vs `recipes` vs `pmap` | three roles, one struct | the roles never share a lifetime question, but they belong to one entity |
| `VmEntry` value vs `owners` caps | value + indirected payload | binding is cheap/immutable; payload caps outlive individual mappings |
| `Frame` identity vs `map_count`/`cache_ref` | identity + compound-predicate payload | the same page is claimed by mappings and the cache independently |
| `PrivatePageSet` (Chapter 8) | a *second* authoritative binding hung off a `VmEntry` | per-mapping CoW ownership is its own truth, not derivable from the page table |

That last row is the series' biggest surprise and gets its own chapter.

## The payoff, stated once

Every hard VM problem is "the truth of a mapping and the hardware's copy of it
want to change at different moments." A fused design fights this with one big
lock, in-place page-table surgery, and a global reverse map. txKernel's split
makes the divergence *the normal case the types are built for*: the recipe is the
truth, the PTE is a cache you rebuild from it, the frame is alive under a
disjunction of claims, and the `RangeLock` guards only the seam. The rest of the
series is this principle worked out through the recipes tree, the pmap, the
hardware, the lock, the fault handler, and the two backings.

## Source anchors

- ARCH-5 / publication rule: `docs/design/00_meta-framework/INVARIANTS_v4.md`; `docs/design/03_memory-vm/VM_v1_2.md` §1, §1.2
- `VmEntry` / `VmEntryBacking` / `VmEntryOwners` / `VmBacking`: `crates/tx-subsystems/src/vm/structure/types.rs:391, 353, 385, 343`
- `VmCap<T>`: same file, `:273`
- `Prot` / `VmEntryFlags`: same file, `:201, 254`
- Frame compound payload (the disjunction): `docs/design/00_meta-framework/object_model_v2.md:131, 134–136`; `docs/design/01_substrate/PAGE_SUBSTRATE_v1.md` §3.1 (`frame_meta.rs:219`)
- Reference ladder: `docs/design/00_meta-framework/object_model_v2.md` §4
- Bifurcation rule (when to split): same doc, §8.1.1
- Address-as-coordinate policy: `docs/design/03_memory-vm/VM_v1_2.md` §"Address boundary policy"
- The sibling split: `docs/tutorials/vfs/02_payload-identity-split.md`
