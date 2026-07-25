# Chapter 3 — Recipes: the authoritative binding

Chapter 1 named `recipes` as the first field of an `AddressSpace`; Chapter 2
called it the authoritative binding of the whole address space. This chapter
opens it. It is txKernel's VMA tree — but built so that reads are lock-free and
`fork` is nearly free, which is what the binding/materialization split buys when
you take it seriously.

## What a recipe is

A recipe is a `VmEntry`: one contiguous run of address space with uniform
properties, exactly like a Linux `vm_area_struct`.

```rust
// vm/structure/types.rs:391
pub struct VmEntry {
    pub range: UserRange,                    // [start, end), page-aligned
    pub prot: Prot,                          // r/w/x
    pub flags: VmEntryFlags,                 // shared, grows_down, locked
    backing: VmEntryBacking,                 // None | PrivateAnon | Page { offset }
    pub ufd_registration: Option<UfdRegistration>,
    owners: Arc<VmEntryOwners>,              // the payload caps (Chapter 2)
}
```

`Prot` (`types.rs:201`) and `VmEntryFlags` (`types.rs:254`) are plain bitfield
structs with the helpers the rest of the subsystem leans on — `prot.permits(access)`
for the fault handler's permission check, `prot.without_write()` for CoW demotion.

The mapping in the tree is `(VA range) → VmEntry`, keyed by `range.start()`. A
point lookup for a faulting address finds the entry whose range contains it; a
range query finds every entry overlapping a span.

> **A recipe is immutable once published.** You never mutate a `VmEntry` inside
> the tree. Changing a mapping (new permissions, a narrower range) means
> *replacing* the entry with a fresh one. This is what makes lock-free reads
> sound: a reader that observed an entry sees it whole and unchanging, never a
> half-applied edit. Mutation is replacement, and replacement is atomic.

## The structure: not a locked balanced tree

A textbook VMA tree is a balanced tree (rbtree, maple tree) protected by a lock;
readers and writers both take the lock. txKernel's `RecipeIndex` is built
differently, to make the binding readable without locking:

```rust
// vm/structure/recipe.rs:163
pub(in crate::vm) struct RecipeIndex {
    current: AtomicPtr<RecipeTree>,          // the published immutable tree
    mutation: VmSpinMutex<()>,               // writers only; readers never take it
    // (test-only publish counters elided)
}
```

The tree behind `current` is **immutable and structurally shared**. A reader
loads the pointer under an epoch guard and dereferences it — no lock, no atomic
read-modify-write:

```rust
// vm/structure/recipe.rs:296
fn pinned<'g>(&self, guard: &'g Guard<'_>) -> &'g RecipeTree {
    let raw = self.current.load(Ordering::Acquire);
    // SAFETY: the guard pins this CPU at the publication epoch, so the tree
    // behind `raw` is not yet reclaimed. Writers retire the old pointer only
    // after the swap; EBR guarantees no pinned reader sees a freed tree.
    unsafe { &*raw }
}
```

A writer never edits in place. It takes the `mutation` lock (excluding *other
writers* only), builds a **path-copied replacement tree** that shares every
unchanged subtree with the old one, and atomically swaps `current` to the new
root. The old root is retired through **EBR** (epoch-based reclamation): it is
freed only once every reader that could have loaded it has dropped its guard.

This is the same pattern as a persistent (immutable, structurally-shared) data
structure published behind a pointer — RCU in spirit, with Rust lifetimes
binding the reader's borrow to the guard so it *cannot* escape and observe a
freed tree. The result: **readers of the binding never block and never spin**,
which is the mechanism behind Chapter 2's Motivation 4.

> **Two backends, one interface.** `RecipeTree` is generic over a backend. The
> default is a balanced **treap** (`TreapRecipeIndex`, `recipe_tree.rs:19`); a
> **B+-tree** variant (`BPlusRecipeIndex`) is selectable with the
> `tx_vm_recipe_bplus` cfg. Both implement the same `RecipeBackend` trait and
> both do path-copy publication; the choice is a performance knob, not a
> semantic one. The spec calls this a `PersistentBTree`; the code generalizes it.

## The read API (lock-free, guard-scoped)

Every read takes an epoch `Guard` and returns owned/borrowed values valid for the
guard:

```rust
// vm/structure/recipe.rs
fn lookup(&self, addr: UserVirtAddr, guard) -> Option<VmEntry>          // :313
fn find_free_range(&self, window, page_count, guard) -> Option<UserRange> // :330
fn overlapping(&self, range: UserRange, guard) -> Vec<VmEntry>          // :339
fn snapshot(&self, guard) -> Vec<VmEntry>                               // :343
fn stats(&self, guard) -> AddressSpaceStats                            // :326
```

- **`lookup`** is the fault handler's first move: given a faulting address, find
  the recipe that justifies materializing it (Chapter 7). `None` means
  SIGSEGV — there is no binding here.
- **`find_free_range`** is `mmap`'s placement search: scan for a gap big enough
  for a non-fixed mapping (Chapter 10). The gap is not reserved by the search —
  another mapping could grab it before the writer commits, which is fine, because
  the range is re-chosen on retry.
- **`overlapping`** enumerates the entries a range operation will split or
  replace.
- **`snapshot`** copies out every entry — used by `fork`/`exec` and by the
  `/proc` projection (Chapter 11).

These are *observation* methods. None of them takes the mutation lock; all are
safe to run concurrently with each other and with a writer.

## The write API (binding only — never the pmap)

Writes take the mutation lock, path-copy, and publish. Each returns a
`VmMapCommit` (what changed, for stats and pmap follow-up) or a `VmMapError`:

```rust
// vm/structure/recipe.rs
fn commit_map(&self, entry: VmEntry, placement: MapPlacement) -> Result<VmMapCommit, _>  // :445
fn unmap(&self, range: UserRange) -> Result<VmMapCommit, _>                              // :476
fn protect(&self, range, prot) -> Result<VmMapCommit, _>                                 // :489
fn remap(&self, ...) -> Result<VmMapCommit, _>                                           // :538
fn set_locked(&self, ...) -> ...                                                         // :521
fn tag_ufd_registration(&self, ...) -> ...                                              // :595
```

The critical thing to internalize: **these functions touch only the binding.**
`protect` rewrites the recipes in the range to carry new permissions; it does
**not** walk the page table. `unmap` withdraws recipes; it does **not** tear down
PTEs. The caller — the syscall script (Chapter 10) or the fault path — is
responsible for the materialization follow-up, *in the invariant-preserving
order*: withdraw or rewrite the recipe first, then tear down the now-stale PTEs.

That ordering is the whole reason the justification invariant holds under
concurrency. A reader that snapshots the recipes after the swap sees the new
binding; the stale PTEs that briefly remain are torn down next, and any
concurrent fault is excluded from the range by the `RangeLock` writer the caller
holds (Chapter 6). The binding leads; the materialization follows.

`commit_map` takes a `MapPlacement` (`RequireFree` for a fresh non-overlapping
mapping, `FixedReplace` for `MAP_FIXED` that overwrites whatever is there) and is
where coalescing happens — see below.

## Why `fork` is nearly free on the binding side

Here is the persistent tree paying off. `fork` must give the child the parent's
entire set of mappings. With a path-copied immutable tree, that is one pointer
clone:

```rust
// vm/structure/recipe.rs:282
pub(in crate::vm) fn clone_shared(&self, guard: &Guard<'_>) -> Self {
    let initial = Box::into_raw(Box::new(self.pinned(guard).clone()));
    Self { current: AtomicPtr::new(initial), /* fresh mutation lock */ }
}
```

The child's tree *shares every node* with the parent's. No per-entry copy; the
binding side of `fork` is O(1) in the number of mappings. The pages themselves
are handled by CoW demotion at the pmap and `PrivatePageSet` layers (Chapter 8),
not here — the binding clone is cheap precisely because it copies no payload.

When a later operation makes the child's binding diverge (a CoW write that
allocates a private set, say), only the path to the affected entry is copied;
every untouched mapping keeps sharing the parent's nodes. This is the same
structural sharing that makes the read path safe, now making mutation cheap.

## Coalescing: keeping the tree small

A heap that grows a page at a time, or many adjacent anonymous `mmap`s, would
bloat the tree into thousands of one-page entries. `commit_map` fights this by
**coalescing adjacent compatible entries** at commit time:

```rust
// vm/structure/recipe.rs:462 (inside commit_map, RequireFree arm)
let (rewritten, stats_delta, touched_entries) =
    insert_coalescing_adjacent(current, entry)?;
```

If the new entry abuts an existing one with identical `prot`, `flags`, backing,
and ufd tag, they merge into one wider entry. The authoritative binding is
unchanged — the same addresses map the same way — but the representation stays
compact. This is an optimization, not a correctness requirement; the spec lists
it as optional and the code does it for anonymous growth.

## Where we go from here

You have the authoritative binding: an immutable, structurally-shared tree of
`VmEntry`s, read lock-free under an epoch guard and written by path-copy-and-swap,
touching only the binding and leaving the materialization to the caller. Chapter
4 turns to that materialization — the pmap — and shows how PTEs are published from
recipes and torn back down, with a resident shadow store tracking what is
currently materialized.

## Source anchors

- `VmEntry` / `Prot` / `VmEntryFlags`: `crates/tx-subsystems/src/vm/structure/types.rs:391, 201, 254`
- `RecipeIndex` struct (`current` + `mutation`): `crates/tx-subsystems/src/vm/structure/recipe.rs:163`
- Lock-free read (`pinned`): same file, `:296`
- Read API (`lookup`/`find_free_range`/`overlapping`/`snapshot`/`stats`): same file, `:313, 330, 339, 343, 326`
- Write API (`commit_map`/`unmap`/`protect`/`remap`): same file, `:445, 476, 489, 538`
- `fork` clone (`clone_shared`): same file, `:282`
- Coalescing (`insert_coalescing_adjacent`): same file, `:462`
- Backend selection (treap default, B+ behind cfg): `crates/tx-subsystems/src/vm/structure/recipe_tree.rs:18–22`, `TreapRecipeIndex` `:597`
- `MapPlacement`: `crates/tx-subsystems/src/vm/structure/types.rs`
- Persistent-tree / EBR rationale: `docs/design/03_memory-vm/VM_v1_2.md` §2 ("Recipes implementation note")
