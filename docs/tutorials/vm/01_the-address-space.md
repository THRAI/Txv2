# Chapter 1 — The address space: three structures

Linux's per-process address space is `mm_struct`: a VMA tree, a page-table root,
some counters, and `mmap_lock`. txKernel's is `AddressSpace`, and opening it up
is the fastest way to see the whole design, because its fields *are* the three
layers the rest of the series explains.

## The struct

```rust
// vm/structure/address_space.rs:40
pub struct AddressSpace {
    recipes: RecipeIndex,                       // authoritative binding
    pmap: VmPmap,                               // derived materialization
    range_lock: RangeLock,                      // coordination
    stats: AddressSpaceStatsCell,               // non-authoritative observability
    next_private_anon_write_fault_page: AtomicUsize,  // fault-clustering hint
    next_mmap_search_start: AtomicUsize,        // free-gap search hint
}
```

Three load-bearing fields, two hints, one observability cell. Compare
`mm_struct`, which fuses the VMA tree, the `pgd` (page-table root), the lock, and
the counters into one object edited under one lock. txKernel keeps them as
*distinct things with distinct roles*, and the role names are the vocabulary of
the series.

### `recipes` — the authoritative binding

`RecipeIndex` is the tree of mappings: `(VA range) → VmEntry`. It is the
**single source of truth** for "what should be mapped in this address space."
This is the analog of the VMA tree, and Chapter 3 is devoted to it. The crucial
property, established here and relied on everywhere: **nothing is mapped unless a
recipe says so.** A recipe is the *authoritative binding*.

### `pmap` — the derived materialization

`VmPmap` owns the hardware page table — the PTEs the MMU actually walks. It is a
**derived materialization**: every PTE exists *because* some recipe justifies it.
The pmap holds no truth of its own; it can be torn down and rebuilt from the
recipes at any page, and several operations (`mprotect`, `mremap`) do exactly
that. Chapter 4 covers the VM side of the pmap; Chapter 5 the hardware beneath
it.

The name is deliberate: it is a *map of physical materialization*, not the truth
of the mapping.

### `range_lock` — coordination, not a mutex

`RangeLock` is **not** an `mmap_lock`. It does not guard the whole address space.
It admits or excludes operations *by the VA range they declare*: two operations
conflict only if their ranges overlap. A `mprotect` on `[0x1000, 0x2000)` and a
fault at `0x9000` do not contend at all. Chapter 6 is the full treatment; for now
the headline is that coordination is **range-scoped**, which is the structural
answer to Linux's `mmap_lock` contention.

### The non-authoritative remainder

`stats` (`AddressSpaceStatsCell`) caches cheap observability numbers —
`recipe_count`, `vm_size` — for `/proc` and rlimit approximations. It is
**explicitly allowed to lag** the authoritative state; nothing reads it to make a
correctness decision. (Chapter 11 shows that even `/proc/<pid>/maps` does *not*
trust it — it re-snapshots the recipes.)

The two `AtomicUsize` fields are pure hints: where to start the next free-gap
search, and which page the last private-anon write fault clustered around (to
batch-prefault neighbours). Wrong hints cost a little work, never correctness.

> **There is no per-AddressSpace mutex.** Look again at the fields: the only
> coordination primitive is `range_lock`. The recipes are read lock-free under an
> epoch guard and written under a tiny internal mutation lock (Chapter 3); the
> pmap has per-leaf hardware atomicity (Chapter 4); range-level conflicts go
> through `RangeLock`. The "big lock" simply is not there.

## The justification invariant

The relationship between `recipes` and `pmap` is not informal. It is the
invariant the whole subsystem maintains (`VM_v1_2.md` §1):

> For every virtual address `X` in an `AddressSpace`, if the pmap holds a PTE for
> `X`, then the recipes must contain a `VmEntry` covering `X` whose permissions
> permit that PTE's access modes and whose backing resolves to the frame the PTE
> points at.

In words: **every materialization is justified by a binding.** A PTE with no
backing recipe is a bug; a PTE that grants more than its recipe allows is a bug.
The fault handler (Chapter 7) only ever *adds* a PTE while holding a recipe it
just observed; the mutating syscalls (Chapters 6, 10) *withdraw the recipe first,
then tear down the PTEs*, never the reverse. The ordering is what keeps the
invariant true under concurrency.

This is txKernel's instance of a kernel-wide principle — **ARCH-5**, the
publication rule (`INVARIANTS_v4.md`): authoritative bindings are the truth,
derived materializations are published *from* the truth and never outlive it.
Chapter 2 names the principle; this chapter is just where you first see it bite.

## How an AddressSpace is created and held

`AddressSpace` is a zone-allocated entity — it has a slot and is reached through
a capability, exactly like the VFS entities in that series:

```rust
// vm/structure/address_space.rs:71
pub fn new_cap_for_platform<P: PmapIf>() -> Result<Cap<AddressSpace>, VmPmapError> {
    let reservation = step_engine::reserve_for::<AddressSpace>()?;
    Ok(step_engine::sign_for(reservation, Self::new_for_platform::<P>()?))
}
```

You get a `Cap<AddressSpace>` — a refcounted pin on the address space's
*identity*. A process holds one (in `ProcessPayload`); the threads of a process
share it. This is the top of the three-level split: the **AddressSpace identity**
(reached via `Cap`/`Weak`/`IdentRef`) is distinct from the **recipes** (the
binding values it contains) which are distinct from the **pmap** (the
materialization). Chapter 2 lays out all three levels; here you have just met the
first one.

Construction takes a platform type parameter `P: PmapIf` because building an
address space means asking the HAL to create a page-table root — including
stitching in the shared kernel high-half (Chapter 5). The `VmPmap::new_for_
platform::<P>()` call is where that happens.

> **Why `Send + Sync` is hand-written here.** `AddressSpace` contains `MapPin`
> tokens that are deliberately `!Send` (they enforce per-CPU pinning at the
> allocator). The struct lifts that into a `Send + Sync` "shared by discipline"
> shape (`address_space.rs:37`) so a `Cap<AddressSpace>` can flow through a
> process shared across CPUs. The safety argument is the epoch + pmap discipline
> the rest of the series describes — it is not a free pass, it is a claim the
> design has to earn, and the following chapters are that earning.

## The shape of every VM operation

With the three fields named, every operation in the series has the same skeleton,
and you can already predict it:

1. **Observe** the recipes (lock-free, under an epoch guard) to learn the current
   binding.
2. **Acquire** a `RangeLock` reservation over the declared range — *only if* the
   operation mutates the binding or publishes a materialization.
3. **Mutate** the recipes (the authoritative step) and/or **publish/tear down**
   pmap PTEs, in the invariant-preserving order.
4. **Release** the reservation; **drop it across any I/O wait** and re-observe on
   resume.

A read-only operation (a fault that finds its page already mapped, `mincore`)
may stop after step 1. A pure binding rewrite (`mprotect`) does 1–4 and discards
the materialization. A fault does 1–4 and *adds* a materialization. The recipes
are always the truth; the pmap is always derived; the `RangeLock` guards only the
window where the two must agree.

## Where we go from here

You have seen the three structures and the invariant that binds them. Chapter 2
steps back and states the binding/materialization split as a principle — at all
three levels (AddressSpace, VmEntry, Frame) — and walks the five concrete
problems it solves. Then Chapter 3 opens `recipes`, the first and most important
of the three fields.

## Source anchors

- `AddressSpace` struct + fields: `crates/tx-subsystems/src/vm/structure/address_space.rs:40`
- `Send`/`Sync` rationale: same file, `:27–38`
- Construction (`new_cap_for_platform`, `new_for_platform`): same file, `:54–77`
- `Cap`/`Zone`/`ZoneAllocated` wiring: same file, `:19–25`
- `recipes` / `pmap` / `range_lock` types: `RecipeIndex` (`recipe.rs:163`), `VmPmap` (`pmap.rs`), `RangeLock` (`range_lock.rs:114`)
- Justification invariant + ARCH-5: `docs/design/03_memory-vm/VM_v1_2.md` §1; `docs/design/00_meta-framework/INVARIANTS_v4.md` (ARCH-5)
- The object-model identity/payload split this mirrors: `docs/design/00_meta-framework/object_model_v2.md` §3
