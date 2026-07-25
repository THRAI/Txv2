# The txKernel VM — Technical Report

A single-document treatment of txKernel's virtual-memory subsystem, for readers
who prefer one continuous report to the [chapter series](README.md). It assumes
familiarity with the traditional Unix/Linux VM (MMU, page tables, `mm_struct`,
VMAs, demand paging, copy-on-write, the page cache). The chapter series teaches
each traditional concept inline; this report does not.

Section numbers track the chapters: §0–§12 correspond to Chapters 0–12.

---

## Abstract

txKernel's VM keeps the conventional *shape* of a Unix virtual-memory system — a
per-process address space, VMA-like mappings in a balanced tree, hardware page
tables, a demand-paging fault handler, anonymous and file-backed memory,
copy-on-write `fork`, and a page cache — but reorganizes it around one principle:
**authoritative binding versus derived materialization** (the kernel-wide ARCH-5
publication rule). The recipes tree is the truth of what should be mapped; the
page table is a cache rebuilt from it; a physical frame is alive under a
disjunction of independent claims. This split recurs at three scales —
AddressSpace, VmEntry, Frame — and from it follow lock-free fault reads,
range-scoped coordination instead of a global `mmap_lock`, cheap `fork`, and a
sharp identity/payload boundary at the syscall edge. Three commitments simplify
the design relative to Linux: no swap, no KPTI, no global rmap.

This report also documents where the shipped code diverges from the design doc
(`VM_v1_2.md`); the most significant is the per-mapping `PrivatePageSet` (§8), a
scoped authoritative index for anonymous copy-on-write that the spec's
"no back-index" language does not describe.

---

## §1. The address space: three structures

```rust
// vm/structure/address_space.rs:40
pub struct AddressSpace {
    recipes: RecipeIndex,          // authoritative binding: (VA range) → VmEntry
    pmap: VmPmap,                  // derived materialization: the page table
    range_lock: RangeLock,         // range-scoped coordination
    stats: AddressSpaceStatsCell,  // non-authoritative observability
    next_private_anon_write_fault_page: AtomicUsize,  // fault-clustering hint
    next_mmap_search_start: AtomicUsize,              // free-gap search hint
}
```

Three load-bearing fields. `recipes` is the VMA tree analog and the single source
of truth (§3). `pmap` owns the hardware page table and holds no truth of its own
(§4–5). `range_lock` admits/excludes operations by declared VA range — *not* a
whole-address-space mutex (§6). `stats` may lag and is never read for correctness.
There is **no per-AddressSpace mutex**.

**The justification invariant** binds `recipes` and `pmap`: for every VA `X`, if
the pmap holds a PTE for `X`, the recipes must contain a `VmEntry` covering `X`
whose permissions permit the PTE's access and whose backing resolves to the PTE's
frame. Materialization is justified by binding; the fault handler only *adds* a
PTE while holding a freshly observed recipe, and mutators *withdraw the recipe
before tearing down PTEs* — never the reverse. This is txKernel's instance of
**ARCH-5** (`INVARIANTS_v4.md`).

An `AddressSpace` is zone-allocated and held as `Cap<AddressSpace>`
(`address_space.rs:71`), shared across a process's threads. Construction is
generic over `P: PmapIf` because building one means asking the HAL for a
page-table root with the shared kernel high-half stitched in (§5).

Every VM operation has the same skeleton: observe recipes (lock-free, epoch
guard) → acquire a `RangeLock` reservation if mutating/publishing → mutate recipes
and/or publish/tear down PTEs in invariant order → release, dropping across any
I/O wait and re-observing on resume.

---

## §2. Binding and materialization: the split at three levels

The organizing principle. **Authoritative binding** is decided truth;
**derived materialization** is everything justified by it and may not outlive or
contradict it. In the VM it appears at three scales:

| Level | Authoritative binding | Derived materialization | Divergence |
|---|---|---|---|
| AddressSpace | `recipes`: `(VA range) → VmEntry` | `pmap`: PTEs | `mprotect`/`munmap`/`fork` rewrite binding, discard PTEs |
| VmEntry | recipe value (`VmEntryBacking`, offset) | `owners`: `Cap<PageContainer>`/`Cap<PrivatePageSet>` | binding immutable + cheap to clone; payload caps reclaim independently |
| Frame | `FrameMeta` identity slot | `map_count ∨ cache_ref` (∨ `pin_count`) | unmapped-but-cached, or cached-out-but-pinned |

**Level 1.** A PTE is a cache of a recipe decision; discarding one is always safe
because it carries no truth. `mprotect` rewrites the recipe and throws PTEs away;
the next fault rebuilds them.

**Level 2.** The recipe *value* stored in the tree is `VmEntryBacking { Page
{ offset } }` — no capability (`types.rs:353`). The `Cap<PageContainer>` and
`Cap<PrivatePageSet>` live behind `owners: Arc<VmEntryOwners>` (`types.rs:385`).
The binding is cheap to clone and immutable; the payload caps outlive individual
mappings. `VmCap<T>` is `Arc<Cap<T>>` (`types.rs:273`). *(The design doc shows one
`VmBacking` with the cap inline; the code split it — a refinement.)*

**Level 3.** A frame's liveness is `map_count > 0 ∨ cache_ref > 0`
(`object_model_v2.md:131`), the exact physical-page analog of the VFS inode's
`nlinks > 0 ∨ open_refs > 0`, with `MapPin`/`CachePin` playing `LinkPin`/`OpenPin`.
The full `FrameMeta` word packs `refcount(0..10) | map_count(10..20) |
cache_ref(20..28) | pin_count(28..32)` (`PAGE_SUBSTRATE_v1.md` §3.1); the headline
disjunction is mapped ∨ cached, with refcount/pin_count as owner/DMA extensions.

**The reference ladder** (`Weak → IdentRef<'g> → Cap → OperationalEvidence`,
`object_model_v2.md` §4) is reused: `Cap<AddressSpace>` pins identity,
`VmCap<…>` pins entry payload, `MapPin`/`CachePin` are frame payload pins.
Higher rung entails lower; downgrade is free, upgrade may fail.

**Five motivations:** (1) `mprotect`/`munmap` are binding rewrites, not
page-table surgery; (2) `fork` demotes materialization and shares the binding;
(3) unmap ≠ evict (the frame disjunction); (4) lock-free fault reads, narrow-locked
writes; (5) typed user addresses are coordinates, not authority.

---

## §3. Recipes: the authoritative binding

A recipe is a `VmEntry` (`types.rs:391`): `range`, `prot`, `flags`, a
`VmEntryBacking` value, an optional `ufd_registration`, and `owners` (the payload
caps). It is **immutable once published**; changing a mapping replaces the entry,
which is what makes lock-free reads sound.

`RecipeIndex` (`recipe.rs:163`) is not a locked balanced tree. It is an immutable,
structurally-shared tree published behind `current: AtomicPtr<RecipeTree>`.
Readers load it under an epoch guard and dereference — no lock, no atomic RMW
(`pinned`, `recipe.rs:296`). Writers take a tiny `mutation` lock (excluding only
other writers), build a path-copied replacement sharing all unchanged subtrees,
swap the root atomically, and retire the old root through EBR. This is RCU in
spirit with Rust lifetimes binding the reader's borrow to the guard. The default
backend is a treap (`recipe_tree.rs:19`); a B+-tree is selectable via
`tx_vm_recipe_bplus`.

Read API (all take an epoch guard): `lookup` (`:313`, the fault handler's first
move; `None` → SIGSEGV), `find_free_range` (`:330`, `mmap` placement),
`overlapping` (`:339`), `snapshot` (`:343`, fork/exec/proc), `stats` (`:326`).

Write API (all take the mutation lock, touch **only the binding**, return
`VmMapCommit`): `commit_map` (`:445`), `unmap` (`:476`), `protect` (`:489`),
`remap` (`:538`). The caller does the materialization follow-up in
invariant-preserving order (withdraw/rewrite recipe, *then* tear down PTEs).

`fork` is O(1) on the binding side: `clone_shared` (`:282`) clones only the root;
the child shares every node. Divergence later path-copies only the affected path.
`commit_map` coalesces adjacent compatible entries (`insert_coalescing_adjacent`,
`:462`) to keep the tree compact under heap growth.

---

## §4. Pmap: the derived materialization (VM side)

```rust
// vm/pmap.rs:157
pub struct VmPmap { root: Option<PmapRoot>, ops: VmPmapOps, state: Mutex<VmPmapState> }
```

`root` is opaque HAL evidence; `ops` are the platform `PmapIf` primitives;
`state` holds the **resident shadow store** (which pages are materialized) and
counters. A published mapping is `PmapMapping { ppn, prot, pin: MaterializedPagePin }`
(`pmap.rs:85`) — the `pin` is the `MapPin` holding the frame's mapped disjunct.
Read-only observation uses `PmapMappingSnapshot` (`:109`, no pin leaked) via
`lookup` (`:185`) / `walk_range` (`:200`, used by `mincore`).

Publishing is a reserve→commit HAL handshake (`publish_page_with_replacement`,
`pmap.rs:237`): idempotent on an identical `(ppn, prot)` (race winner safe);
`MappingMismatch` on a conflicting mapping unless `replace_existing`; the shadow
store updated in lockstep. `publish_new_pages_best_effort` (`:308`) is the
speculative-prefault batch.

The resident store has two backends (`pmap/resident.rs`): `VecPmapResidentStore`
(`:142`, sorted Vec, O(n) suffix shift on range drain) and
`ChunkedPmapResidentStore` (`:236`, 64-entry chunks, no global shift), selected by
`tx_vm_pmap_chunked_resident`. `drain_range` reports `shifted_entries` (`:68`), the
metric the chunked backend drives to zero.

`teardown_range` (`pmap.rs:380`) removes PTEs, drops `MapPin`s, and batches the
shootdown — called *after* the recipe is withdrawn. `protect_range` (`:468`) is
the in-place demotion path (notably `fork`'s read-only demotion).

---

## §5. The HAL boundary: page tables, satp, TLB shootdown

`PmapIf` (`tx-hal/src/lib.rs:670`) is the VM↔hardware seam: node allocation
(`alloc_pt_node` `:675`, `free_pt_node` `:679`), per-address-space mapping
(`create_pmap_root` `:730`, `reserve_mapping` `:747`, `commit_mapping` `:758`,
`unmap_mapping` `:765`, `protect_mapping` `:773`, `activate_user_pmap` `:803`),
and ASID-scoped shootdown (`shootdown_mappings` `:784`). Reserve allocates missing
intermediate nodes; commit writes the leaf; an uncommitted reservation rolls back.

On the RISC-V QEMU virt board, mapping a 4 KiB page is a three-level Sv39 walk
(`pmap/address_space.rs:434`), allocating L1/L0 tables from a `PtFrame`
(`page_allocator/tokens.rs:175`) or the early `PT_NODE_POOL`.

**No KPTI / shared kernel half:** a new root copies the upper L2 slots from the
boot root (`root.0[256..].copy_from_slice(&kernel_root.0[256..])`,
`address_space.rs:94`) — duplicating only top-level pointers; the kernel L1/L0
tables are shared by reference. `fork` never touches the kernel half; there is no
kernel/user page-table switch on syscall.

**Address-space switch** is `activate_user_pmap` (`lib.rs:476`): mark the ASID
resident on this hart, write `satp` (skip if unchanged), with **no global
`sfence.vma`** — ASIDs tag TLB entries, so switching does not flush; stale entries
are invalidated precisely at teardown.

**SMP shootdown** is `remote_sfence_vma_asid_batch` (`lib.rs:1407`): compute
target harts from an ASID-residency bitmap (`ASID_RESIDENCY`, `:89`), excluding
the local hart, and `sbi_remote_sfence_vma_asid` IPI only those that ran this
address space. A single-threaded process pays no cross-hart cost. LoongArch64
implements the same contract with a PGDL/PGDH root and `invtlb`.

---

## §6. RangeLock: coordination without object locks

```rust
// vm/structure/range_lock.rs:26
pub enum LockMode { ExclusiveWriter, Materializer }
```

`ExclusiveWriter` is taken by binding mutators (`mmap`/`munmap`/`mprotect`/`mremap`/
fork/exec); `Materializer` by PTE publishers (the fault handler, prefault). Two
`ExclusiveWriter`s, or a writer and a materializer, conflict if their ranges
overlap; **two `Materializer`s never conflict** (page uniqueness is enforced below,
in the pmap/page-index). `RangeLock` only excludes a binding mutation from running
concurrently with a materialization in the same range.

**The declared-range rule** (`VM_v1_2.md` §3.4): the conflict domain is the range
the operation declares, never the extent of a `VmEntry` it touches. A `mprotect`
on `[0x1000,0x2000)` and a fault at `0x5000` in the same VMA do not conflict.
Conflict = declared ranges overlap. This is the structural answer to `mmap_lock`
contention.

Structure: an AVL interval tree (`max_end`-augmented), fixed-array-backed (16
active + 16 pending writers, `range_lock.rs:305, 459`), behind a short-held spin
lock. `acquire_step` (`:152`) returns `Done(guard)` or yields a wait token;
`acquire_pair_step` (`:166`) acquires two ranges atomically (`mremap`). A blocked
acquire yields on a wait source (`RANGE_LOCK_RELEASE_MASK`, `:23`); a wake is a
hint, not a grant — retry from scratch. `RangeGuard` Drop (`:70`) releases and
wakes waiters, unconditionally (RAII).

Fairness is **writer-preferred, writers FIFO**: a `Materializer` is blocked by any
*pending* writer (`materializer_blocked`, `:320`), so queued writers cannot be
starved by a fault stream; writers order by monotonic id. Rationale: bindings are
truth, materializations are derived; the truth must make progress.

**Cross-async-wait discipline** (`§3.6`, the key operational rule): a reservation
protects only the synchronous phase. If a step yields (disk I/O), it **drops the
reservation and re-observes everything on resume**. The fault handler embodies
this literally (§7), and re-observation closes the TOCTOU window the dropped lock
opens.

---

## §7. The fault handler

`fault_script_with_ufd_dispatch` (`execution.rs:255`) is `handle_mm_fault`'s
analog: an `async fn` loop with two yield points, each of which drops its
reservation before awaiting and re-runs from the top on resume.

**Phase 1 — Resolve** (`try_fault_script_resolve`, `:308`): take `Materializer` on
the faulting page range; `require_fault_recipe` (`checks.rs:16`) looks up the
recipe. `lookup` miss → `NoRecipe`; permission denied → `ProtectionViolation`
(both → SIGSEGV). Success yields a `VmFaultOutcome` carrying the observed entry,
access, and a `private_identity` snapshot.

**Phase 3 — Materialize + Publish** (`try_fault_script_materialize_and_publish`,
`:393`): materialize the frame (may block → drop reservation, await, retry):
`PrivateAnon` read → shared zero frame RO; `PrivateAnon` write → fresh private
frame in the `PrivatePageSet` (§8); `Page` → page cache via `materialize_page`
(§9, may block on disk). Then `try_fault_script_publish` (`:344`) re-takes
`Materializer` and **re-observes before installing the PTE**:
`require_fault_publication` (`checks.rs:35`) re-reads the recipe and returns
`StaleRecipe` if the entry, permissions, private-set identity, or backing/page-index
diverged during the wait. Only on a confirmed-current recipe does
`publish_page_with_replacement` install the PTE. This is the publication rule as
code: **the PTE is installed only while holding a recipe re-confirmed an instant
before.**

The CoW write fault (a write to a `SharedCow` page mapped RO by fork) arrives as a
write whose recipe permits writing but whose PTE is RO; the materialize step
copies, re-owns (§8), and publishes with `replace_existing = true`. After a
private-anon write, `prefault_private_anon_write_batch` (`:442`) opportunistically
materializes neighbours (best-effort).

A userfaultfd branch (`:285`) sits between resolve and materialize: if the recipe
carries a `ufd_registration` tag, the fault dispatches to a userspace agent
(`UFFDIO_COPY`/`ZEROPAGE`); a miss falls through to normal materialization.

`VmFaultError` (`types.rs:1548`) → signal is the trap layer's policy
(`thread_future.rs:858, 1088`): `NoRecipe`/`ProtectionViolation` → SIGSEGV,
`PageBeyondSize` → SIGBUS, `WouldBlock`/`StaleRecipe` → retry.

---

## §8. Anonymous memory and copy-on-write (the divergence)

Anonymous memory (`VmEntryBacking::PrivateAnon`): a read fault installs the shared
zero frame RO (`page_allocator/mod.rs:445`); the first write needs a private,
writable frame *and a record of which frame*. With no swap and no global rmap,
that record is the **`PrivatePageSet`** (`private.rs:196`) — and here the code
diverges from the spec's "no back-index" (`VM_v1_2.md` §7.3).

It is attached to a `VmEntry` via `owners.private` (`VmCap<PrivatePageSet>`),
zone-allocated, keyed by **`VmPageOff`** (offset within the entry's range, stable
across `mremap`/split, `private.rs:38`) rather than absolute VA. Its contents are
**authoritative, not cache**: a written private page is unrecoverable from any
other source (`private.rs:21`). Each `PrivateFrame` (`:63`) holds a `CachePin`
(the `cache_ref` disjunct); the `MapPin` is taken separately at PTE install.

CoW state machine (`PrivateFrameState`, `:48`): `∅ →(install_if_absent, :945)→
Exclusive →(fork_share, :1077)→ SharedCow →(replace_if_match, :1007)→ Exclusive`.
`install_if_absent`/`replace_if_match` are CAS linearization points; `take_if_match`
(`:1025`, page gifting) and `demote_if_match` (`:1037`) round out the API; `split`
(`:1094`) structurally shares the treap on entry split.

`fork_aspace` (`execution.rs:111`) under a whole-range `ExclusiveWriter`:
(1) `clone_shared` the recipe tree (O(1), §3); (2) `fork_share` each private set
(shared treap, all frames → `SharedCow`); (3) `protect_range(…, without_write())`
demote the parent's PTEs RO + shootdown. Recipe says writable, PTE says RO — CoW
armed; the next write breaks it one page at a time. *(fork serializes the parent
over the full range — a v1 simplification, §9.5.)*

This is **not** the forbidden global rmap: it is per-mapping, offset-keyed, used
only by its own mapping's fault/CoW paths. There is still no frame→all-PTEs index;
no swap needs one.

---

## §9. Page-backed mappings and the page cache

A page-backed recipe's `owners.page` is a `Cap<PageContainer>` (`page_backed/
mod.rs:390`) — the `address_space` analog, shared across all mappers and the FS
read/write path. `PageContainerKind` (`:279`): `Anon` (shared anon/shm, RAM-only),
`File { mount, fs_object_id }` (fetch/flush via the mount), `Device { base_ppn,
page_count }` (fixed MMIO window). The recipe stores only the offset.

`materialize_page_for_fault_step` (`:702`) returns `MaterializedPage { ppn,
map_pin, newly_installed, dirty }` (`:306`). `File`: page-index hit returns the
cached frame; miss fetches through the mount (**blocks on disk** — the reason the
fault handler drops its reservation) and `install_if_absent`s it. `Anon`: miss
allocates a zeroed frame. `Device`: `base_ppn + offset`, no cache.

`cache_ref` vs `map_count` are real `FrameMeta` counters with independent
inc/dec sites (`frame_meta.rs:170/174` map, `:178/182` cache). A file page can be
cached with `map_count == 0` (unmapped but resident) — `munmap` drops `MapPin`
(`map_count`), the page-index keeps `CachePin` (`cache_ref`), so the frame
survives for the next `read`/`mmap`. "Unmap" and "evict" touch different counters;
liveness is their disjunction.

`MAP_SHARED` writes hit the cached frame and flush via `msync` (`execution.rs:1287`
→ `step_fsync`, may block; only `File` containers flush). `MAP_PRIVATE` file
writes copy-on-write into a `PrivatePageSet` (§8) — so a written private-file page
becomes authoritative anonymous-like content; the two backings meet at the CoW
break.

---

## §10. VM system calls: the tx-shims seam

The syscall table → `do_mmap` layer, in a separate crate
(`tx-shims/src/linux_syscall/vm.rs`). Every handler follows
**decode → translate → fast path → drive async → errno**:

1. decode `[u64; 6]` + POSIX validation (length, alignment, recognized flags);
2. translate Linux bits to typed vocabulary (`PROT_*` → `Prot`, `MAP_*` →
   `VmEntryFlags`/`MapPlacement`, addr+len → `UserRange`) — the address-as-
   coordinate boundary;
3. synchronous `try_*` fast path (common case, no reactor round-trip); only
   `WouldBlock` falls through;
4. wrap in a step-op (`VmMapOp`/`VmProtectOp`/…, `vm/step_ops.rs`) and `drive(…,
   DriveMode::Waiting)` through the reactor;
5. map result via `vmmap_error_to_i32`/`Errno` — the single errno chokepoint.

Handlers: `sys_mmap:223`, `sys_munmap:504`, `sys_mprotect:823`, `sys_mremap:894`,
`sys_brk:107` (heap delta as anon map/unmap), `sys_madvise:1008`
(`MADV_DONTNEED` = pmap teardown keeping the recipe), `sys_msync:1060`,
`sys_mincore:763` (`walk_range`), `sys_mlock*` (set `locked` flag — pages already
pinned under no-swap). Dispatch hot lane `dispatch_vm_hot` (`mod.rs:792`) inlines
mmap/munmap/mprotect.

**exec/ELF:** `build_aspace_from_image` (`scripts.rs:212`) installs one recipe per
`PT_LOAD` (`VmBacking::Page` for the file portion, `PrivateAnon` BSS tail where
`memsz > filesz`) plus a stack at `USER_STACK_TOP_DEFAULT` (`:52`) — recipes only;
the new image demand-faults. `populate_detached_user_range` (`:341`) writes the
initial stack page-by-page. **Gifting (vmsplice):** `gift_user_pages_step`
(`gift.rs:228`) transfers frame ownership; `classify_user_gift_page` (`:371`)
detaches private-anon pages from the `PrivatePageSet`. **userfaultfd:**
`userfaultfd.rs:137` creates the cap; `UFFDIO_REGISTER` tags VMAs read by §7.

---

## §11. Projection: VM state into `/proc/<pid>/maps`

`vm/project.rs` is the read-only projection home: procfs consumes its helpers
rather than touching `AddressSpace` internals. `project_address_space` (`:33`)
snapshots `recipes_snapshot()` and maps each entry to `VmMappingProjection`
(`:19`), reducing the backing to `VmBackingProjection` (`:27`) — **stripping the
`owners` caps**. A consumer learns "page-backed at offset N" but cannot reach the
page cache or pin a frame. This is the VFS `Projected { schema, key }` RNode
mechanism applied to the VM.

Projection reads go **direct via the recipe snapshot, not through `vm::checks`**
(those are mutation-gating, §7), and do not trust the lagging `AddressSpaceStats`
(`types.rs:843`) for per-mapping data. `render_maps` (`tx-fs/src/procfs/read.rs:201`)
re-snapshots the recipes; **one maps line = one recipe** (recipes are the VMAs, so
there is no VMA/page-table reconciliation). `render_smaps` (`:260`) and
`render_status` (`:388`, summing `flags.locked` for `VmLck`) read the same
snapshot. Observation is strictly lower-privilege than mutation: look at the
binding without being handed the payload.

---

## §12. Capstone: one heap page

`mmap` anon → recipe only, no PTE/frame (demand paging; the maps line already
exists). Read fault → RO PTE to shared zero frame (`map_count++`). Write fault →
private frame in `PrivatePageSet` (`Exclusive`, `cache_ref=1`), writable PTE
replaces it (`map_count=1`). `fork` → recipe tree shared O(1), private set shared
(`SharedCow`, `cache_ref=2`), parent PTE demoted RO + shootdown. Child write →
copy to new frame, re-own `Exclusive`, writable PTE. `munmap` → recipe withdrawn
*then* PTE torn down; both disjuncts → 0 → frame freed (for a file page,
`cache_ref` would keep it — unmap ≠ evict). `exec` → all PTEs torn down, recipes
rebuilt from the new ELF; kernel high-half never touched.

Every step is the same principle at three levels: the recipe leads, the PTE
follows, the frame lives under a disjunction. Linux reaches the same behaviors via
`mmap_lock` + in-place page-table surgery + `anon_vma` + an overloaded refcount;
txKernel reaches them by keeping binding and materialization separate and letting
them disagree until reconciled.

---

## Divergences from the design docs

- **`PrivatePageSet` (§8).** `VM_v1_2.md` §7.3 says private anonymous frames have
  no back-index (tracked only via PTEs). The code instead keeps a per-mapping,
  offset-keyed, **authoritative** index. This is the largest divergence; the
  tutorial follows the code. It does not violate the no-*global*-rmap commitment.
- **`VmEntryBacking` vs `VmBacking` (§2).** The doc shows one `VmBacking` with the
  cap inline; the code splits the stored value (`VmEntryBacking`, no cap) from the
  payload (`owners`). A refinement of the same split.
- **Recipe backend (§3).** The doc names a `PersistentBTree`; the code generalizes
  to a `RecipeBackend` trait with a treap default and a B+ alternative.
- **fork serialization (§8).** A whole-range `ExclusiveWriter`, acknowledged as a
  v1 simplification (`§9.5`).

## Source map

| Area | Primary source |
|---|---|
| AddressSpace | `crates/tx-subsystems/src/vm/structure/address_space.rs:40` |
| Recipes | `crates/tx-subsystems/src/vm/structure/recipe.rs:163`; `recipe_tree.rs` |
| Pmap (VM) | `crates/tx-subsystems/src/vm/pmap.rs:157`; `pmap/resident.rs` |
| PmapIf / board | `crates/tx-hal/src/lib.rs:670`; `boards/tx-hal-riscv64-qemu-virt/src/{pmap/address_space.rs,lib.rs}` |
| RangeLock | `crates/tx-subsystems/src/vm/structure/range_lock.rs:114` |
| Fault handler | `crates/tx-subsystems/src/vm/execution.rs:255`; `checks.rs` |
| PrivatePageSet | `crates/tx-subsystems/src/vm/structure/private.rs:196` |
| Page-backed | `crates/tx-subsystems/src/page_backed/mod.rs:390` |
| Syscalls | `crates/tx-shims/src/linux_syscall/{vm.rs,userfaultfd.rs,mod.rs}` |
| Projection | `crates/tx-subsystems/src/vm/project.rs`; `crates/tx-fs/src/procfs/read.rs` |
| Specs | `docs/design/03_memory-vm/{VM_v1_2.md,PAGE_BACKED_v1.md}`; `docs/design/01_substrate/PAGE_SUBSTRATE_v1.md`; `docs/design/00_meta-framework/object_model_v2.md` |
