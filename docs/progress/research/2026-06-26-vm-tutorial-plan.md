# VM Tutorial Series — Authoring Plan

**Date:** 2026-06-26
**Status:** DRAFTED 2026-06-26. All 13 chapters + README + TECHNICAL_REPORT
written to `docs/tutorials/vm/`. Grew from 11 after a coverage review surfaced
three gaps (pmap HAL boundary, syscall seam, procfs projection). Anchors
spot-checked against source during authoring (address_space/types/recipe/
recipe_tree/pmap/resident/range_lock/execution/checks/private/page_backed/project
+ tx-hal PmapIf + board satp/shootdown + tx-shims vm.rs). Remaining: a full
anchor-by-anchor audit pass and a docs-link/stale-vocab check before publishing.
**Location (target):** `docs/tutorials/vm/`
**Sibling precedent / house style:** `docs/tutorials/vfs/` and
`docs/tutorials/reactor-and-threads/` (teach traditional concept first, then
txKernel beside it; pseudocode bodies with accurate type/field/method names;
`file:line` anchors per chapter; README + consolidated `TECHNICAL_REPORT.md`).

## Thesis

A mapping's **authoritative binding** and its **materialization** are different
things with different lifetimes, so txKernel stores and reclaims them
separately — and the split recurs at **three levels**:

| Level | Authoritative binding | Derived / payload | Why they diverge |
|---|---|---|---|
| **AddressSpace** | recipes tree `(VA range) → VmEntry` | pmap PTEs (rebuildable, no retention) | mprotect/munmap/fork = rewrite binding, discard PTEs |
| **VmEntry** | recipe value (`VmEntryBacking`, offset) in tree | `owners`: `Cap<PageContainer>` / `Cap<PrivatePageSet>` | binding immutable + cheap to clone; payload caps reclaim on own schedule |
| **Frame** | `FrameMeta` identity slot | `map_count ∨ cache_ref` (∨ pin_count) disjunction | a page can be unmapped-but-cached, or cached-out-but-DMA-pinned |

Reuses the VFS/object-model reference ladder verbatim
(`Weak → IdentRef<'g> → Cap → OperationalEvidence`).

## Doc-grounding check (done 2026-06-26)

- **Frame disjunction is canonical AND refined.** `object_model_v2.md:131`
  gives the 2-disjunct headline `Frame.payload_live ⇔ map_count>0 ∨ cache_ref>0`
  (typed pins `MapPin`/`CachePin`, lines 134–136 — direct `LinkPin ∨ OpenPin`
  analog). `PAGE_SUBSTRATE_v1.md:219–252` is the full 4-counter contract:
  `refcount(0–10) | map_count(10–20) | cache_ref(20–28) | pin_count(28–32)`,
  8 bytes, free ⇔ `state==0`, pins `MapPin/CachePin/DmaPin/GiftPin`. Code
  (`frame_meta.rs`) matches the substrate doc exactly. **Ch.2 teaches the
  2-disjunct headline; footnotes refcount/pin_count as owner/DMA extensions.**
- **Binding-vs-materialization is ARCH-5** (`INVARIANTS_v4.md`) +
  `object_model_v2 §5/§6`; `VM_v1_2 §1` is an explicit ARCH-5 instantiation.
- **VmEntry value/owners factoring** is real in code (`types.rs:385–415`) but
  silent in docs — treat as an implementation refinement of §3.2
  co-located-vs-indirected; "follow code, note doc silence."
- **Ch.2 weighting decision:** all three levels equally, led by
  binding-vs-materialization (the ARCH-5 throughline), descending to the Frame
  disjunction as the concrete payoff.
- **Anchor caveat:** design docs internally cite `02_INVARIANTS_v5.md` /
  `01_CONCEPTS_v5.md` at paths that DO NOT EXIST. Live canonical files are
  `INVARIANTS_v4.md` / `CONCEPTS_v4.md`. Point anchors there.

## Spec-vs-code divergences (teach the CODE; note the stale spec)

`VM_v1_2.md` is v1.2 (2026-04-20) and stale in load-bearing ways:

1. **`PrivatePageSet` treap is the biggest one.** Spec `§7.3` says "no rmap —
   private frames tracked only via PTEs." Code (`structure/private.rs:196–214`)
   hangs an *authoritative per-`VmEntry` treap* of private CoW frames off each
   mapping, with states `Exclusive`/`SharedCow` and CAS linearization
   (`install_if_absent:945`, `replace_if_match:1007`, `take_if_match:1025`,
   `demote_if_match:1037`, `fork_share:1077`, `split:1094`). It is a *scoped*
   rmap the global no-rmap rule still permits. Whole chapter (ch.7).
2. **`VmEntry` itself splits** (`types.rs:385–415`): recipe value holds
   `VmEntryBacking { Page { offset } }` (no Cap); `Cap<PageContainer>` /
   `Cap<PrivatePageSet>` live in `owners: Arc<VmEntryOwners>`. `VmCap<T>` =
   `Arc<Cap<T>>` (`types.rs:273–340`).
3. **Recipes** = pluggable treap/B+ backend behind `AtomicPtr<RecipeTree>` + EBR
   + path-copy + atomic root swap (`recipe.rs:163–170`), not literal
   `PersistentBTree`. Method is `lookup` not `range_containing`.
4. **RangeLock** = fixed-array AVL interval tree (16 slots) + separate
   `pending_writers` tree for writer-preference (`range_lock.rs:114–119,
   305–309, 428–463`). Blocked acquire → `WouldBlock`/`WaitToken` on
   `RANGE_LOCK_RELEASE_MASK`, not `Blocked(token)`.
5. **Fault handler** split into re-entrant `resolve` (`execution.rs:308–342`) +
   `materialize+publish` (`344–391`) phases, with a userfaultfd dispatch branch
   (`285–289`, PR-10) and a private-anon write prefault batch (`442+`).

## Traditional topics covered

MMU / page tables / TLB; `mm_struct` + VMAs; demand paging & fault path;
mmap/munmap/mprotect/mremap/brk surface; anon vs file-backed; CoW & fork; page
cache & `filemap_fault`; TLB shootdown (incl. SMP cross-hart IPIs); satp/ASID
switch; the syscall table → handler seam; `/proc/<pid>/maps`+smaps+status
projection; `mmap_lock` contention; rmap/`anon_vma`; swap. Each introduced
traditionally, then contrasted — including the explicit
**no-swap / no-KPTI / no-global-rmap** commitments.

## Chapter breakdown (two arcs + capstone, mirrors VFS)

- **0 — What is virtual memory.** Traditional MMU/PT/TLB, mm_struct+VMA, fault
  path, syscall surface; the Linux fusion pain (mmap_lock, rmap, anon_vma,
  swap); the two reshaping ideas (PTEs are a rebuildable cache; a frame's life
  is several independent claims); state no-swap/no-KPTI/no-rmap commitments.
- **1 — The address space: three structures.** `AddressSpace = recipes + pmap +
  range_lock` (+ stats, brk/mmap hints) vs `mm_struct`. Authoritative-binding /
  derived-materialization vocabulary + justification invariant.
  *Anchors:* `address_space.rs:40–47, 71–77`.
- **2 — The payload/identity split, VM edition. (HEADLINE.)** Three-level table;
  reference ladder on `Cap<AddressSpace>`/`VmCap<T>`/frame pins; five
  motivations grounded in real ops (mprotect/munmap cheap; fork CoW demotion;
  unmap≠evict; lock-free fault reads via epoch + RangeLock writes; typed addrs
  are boundary coordinates not authority). Lead with binding-vs-materialization
  (ARCH-5), descend to the Frame `map_count ∨ cache_ref` disjunction. Callback
  to VFS ch.2. *Anchors:* `types.rs:273–340, 385–415`; `frame_meta.rs` /
  `PAGE_SUBSTRATE_v1.md:219–252`; `object_model_v2.md:131,134–136`;
  `VM_v1_2 §1,§1.2`; `INVARIANTS_v4.md` ARCH-5.
- **3 — Recipes: the authoritative binding.** `VmEntry` fields; `VmBacking` vs
  `VmEntryBacking` + `owners` mini-split; persistent tree (AtomicPtr+EBR+
  path-copy+root swap); `lookup`/`find_free_range`/`overlapping`/`commit_map`/
  `unmap`/`protect`/`clone_shared`; commit-time coalescing. vs Linux maple tree.
  *Anchors:* `recipe.rs:163–170, 282–292, 313–345, 445–519`; `types.rs:343–416`.
- **4 — Pmap: the derived materialization (VM side).** PTE publish/teardown over
  the `PmapIf` boundary; resident shadow store (not authoritative; Vec vs
  chunked `tx_vm_pmap_chunked_resident` and why chunked exists — kills O(n)
  suffix shifts on big munmap/exit); `map_count` inc/dec; ShootdownBatch
  *issued* here, *executed* in ch.5. vs page tables (HW side deferred to ch.5).
  *Anchors:* `pmap.rs:145–148, 233–300, 380–460, 577–593`;
  `pmap/resident.rs:30–83, 110–130, 142–234, 236+`.
- **5 — The HAL boundary: page tables, satp, and TLB shootdown. (NEW.)** The
  `PmapIf` trait as the VM↔hardware seam; the Sv39 three-level walk + `PtFrame`/
  `PT_NODE_POOL` node allocation; kernel high-half sharing / no-KPTI
  (`root.0[256..].copy_from_slice`); SMP TLB shootdown — `sbi_remote_sfence_vma_asid`
  IPIs gated by an ASID-residency bitmap; satp switch + the no-`sfence`-after-
  switch optimization; the `fault-decode` xtask tooling note. vs Linux
  `flush_tlb_*`/`mm_cpumask`. *Anchors:* `tx-hal/src/lib.rs:670–807`;
  `boards/tx-hal-riscv64-qemu-virt/src/pmap/address_space.rs:283–287, 434–470`;
  `boards/.../src/lib.rs:476–513, 1407–1452`; `boards/.../src/sbi.rs:81`;
  `tokens.rs:175, 481`.
- **6 — RangeLock: coordination without object locks.** Declared-range rule;
  ExclusiveWriter vs Materializer; writer-preferred FIFO via pending-writers
  tree; fixed-array AVL interval tree; WaitToken + cross-async-wait discipline
  (drop guard before await, re-observe on wake). vs `mmap_lock`. *Anchors:*
  `range_lock.rs:25–29, 114–119, 305–309, 320–338, 428–463`; `VM_v1_2 §3.4–3.6`.
- **7 — The fault handler.** `resolve → materialize → publish`, re-entrant across
  async I/O, publication rule (re-observe recipe after wait), permission checks,
  SIGSEGV/SIGBUS, read vs write fault. Steps/reactor/async made concrete;
  cross-link reactor series. vs `handle_mm_fault`/`do_fault`. *Anchors:*
  `execution.rs:255–306, 308–342, 344–391`; `checks.rs:16–67`.
- **8 — Anonymous memory & copy-on-write. (DIVERGENCE chapter.)** PrivateAnon;
  `PrivatePageSet` treap as second authoritative binding; Exclusive/SharedCow;
  CoW linearization (install_if_absent/replace_if_match/demote_if_match);
  fork_share; zero frame. "No rmap… except a scoped per-mapping one." vs
  anon_vma/CoW. *Anchors:* `private.rs:49–89, 196–214, 945–1094`;
  `execution.rs:111–141`; `page_allocator/mod.rs:445–477`.
- **9 — Page-backed mappings & the page cache.** File/tmpfs/shm/device backing;
  PageContainer + materialize_page; shared vs private file maps; msync
  writeback; `cache_ref` vs `map_count` (cached-but-unmapped). vs
  `filemap_fault`/address_space. Cross-link VFS ch.7. *Anchors:*
  `page_backed/mod.rs:279–311, 390–395`; `frame_meta.rs:170–184`;
  `execution.rs:1287–1314`.
- **10 — VM system calls: the tx-shims seam. (REFRAMED.)** The syscall↔script
  two-layer model: decode `args[6]` + translate `PROT_*`/`MAP_*` → `Prot`/
  `VmEntryFlags`/`MapPlacement`; the **try-sync-then-drive-async** pattern
  (`try_mmap` fast path → `drive(VmMapOp, …, DriveMode::Waiting).await` on
  `WouldBlock`); errno mapping (`vmmap_error_to_i32`); the `dispatch_vm_hot`
  inline lane. The full gallery (mmap/munmap/mprotect/mremap/brk/madvise/msync/
  mincore/mlock*). Plus exec/ELF load (build_aspace_from_image, LoadSegment, BSS
  tail, stack populate) and the advanced consumers: page gifting (vmsplice) and
  the userfaultfd syscall + UFFDIO ioctls. vs Linux syscall table → `do_mmap`.
  *Anchors:* `tx-shims/src/linux_syscall/vm.rs:107,223,504,823,894,1008,1060`;
  `.../mod.rs:792`; `.../userfaultfd.rs:137,199,293,575`;
  `vm/step_ops.rs`; `scripts.rs:212–291, 341–526`; `gift.rs:228–368`.
- **11 — Projection: how VM state becomes `/proc/<pid>/maps`. (NEW.)** The
  read-only observation side: `vm/project.rs`
  (`project_address_space` → `AddressSpaceProjection`/`VmMappingProjection`,
  reads `recipes_snapshot()` under epoch, leaks no caps) → procfs
  `render_maps`/`render_smaps`/`render_status` → a `Projected { schema, key }`
  RNode (the VFS-ch.3/8 mechanism). Teaching point: projection is
  non-authoritative and goes **direct via snapshot, not through `vm::checks`**
  (those gate mutation). Each maps line = one recipe; recipes *are* the VMAs.
  vs Linux `fs/proc/task_mmu.c`. Cross-link VFS `Projected` backing.
  *Anchors:* `vm/project.rs:13–58`; `vm/structure/types.rs:843`;
  `tx-fs/src/procfs/read.rs:201,260,388`.
- **12 — Capstone.** One lifecycle: map anon heap page → read fault (zero) →
  write fault (private frame in set) → fork (CoW demotion, recipe shared) →
  child write (CoW break via replace_if_match) → parent munmap → frame walks
  `map_count → cache_ref → free` → exec teardown; peek at `/proc/self/maps`
  mid-flight to tie projection in. Re-populates mapping table.

Plus **README** (thesis + mapping table + "what's genuinely the same") and
**TECHNICAL_REPORT.md** (consolidated).

**Arc structure (13 ch).** Conceptual core 0–3; materialization + hardware 4–5;
coordination + fault 6–7; the backings 8–9; the outward surface (syscalls,
projection) 10–11; capstone 12.


## Authoring decisions

- Running example: anonymous private heap page through ch.7→12; file-backed
  mmap as secondary in ch.9.
- Follow the code, flag the stale spec (esp. PrivatePageSet vs §7.3).
- Pseudocode + accurate names + `file:line` anchors, spot-checked.
- Cross-links: ch.2 ↔ VFS ch.2; ch.7 ↔ reactor-and-threads; ch.9 ↔ VFS ch.7;
  ch.11 ↔ VFS `Projected` backing (ch.3/8).
- Deferred (not chapters): NUMA/huge-pages (out of scope in v1, mention in a
  tech-debt aside). TLB/SMP shootdown is now ch.5, not deferred.

## Coverage-review findings (2026-06-26, drove 11→13)

Three areas the first cut under-covered, now grounded:

- **pmap HAL boundary (→ ch.5).** `PmapIf` trait is the VM↔HW seam
  (`tx-hal/src/lib.rs:670–807`): create/destroy root, reserve/commit/unmap/
  protect mapping, shootdown, `activate_user_pmap`, `alloc/free_pt_node`.
  Sv39 3-level walk + PtFrame/PT_NODE_POOL in the board
  (`boards/tx-hal-riscv64-qemu-virt/src/pmap/address_space.rs:434–470`).
  No-KPTI kernel high-half copy at root create (`address_space.rs:283–287`).
  SMP shootdown = `sbi_remote_sfence_vma_asid` (`sbi.rs:81`) gated by
  ASID-residency bitmap; satp switch skips redundant `sfence`
  (`boards/.../lib.rs:476–513, 1407–1452`). Resident store chunked variant
  (`pmap/resident.rs:236+`) exists to kill O(n) suffix shift on big teardown.
  No major doc divergence (matches PAGE_SUBSTRATE_v1 / VM_v1_2 §2). LA64 board
  differs (PGDL/PGDH) — expected arch variation.
- **VM syscalls (→ ch.10).** Home is `tx-shims/src/linux_syscall/vm.rs`
  (handlers: brk:107, mmap:223, munmap:504, mincore:763, mprotect:823,
  mremap:894, madvise:1008, msync:1060, mlock*:562/610/677/706/754). Dispatch
  hot lane `dispatch_vm_hot` at `mod.rs:792`. Pattern per handler: decode
  args → translate Linux flags → `try_*()` sync fast path → on `WouldBlock`,
  `drive(VmOp, …, DriveMode::Waiting).await` → errno map (`vmmap_error_to_i32`).
  uffd syscall + UFFDIO ioctls in `userfaultfd.rs:137,199,293,575,617,657,707`.
  Not implemented: mmap2 (generic ABI uses mmap#222), process_vm_readv/writev,
  mseal.
- **Projection (→ ch.11).** `vm/project.rs:13–58` —
  `project_address_space`/`AddressSpaceProjection`/`VmMappingProjection`/
  `VmBackingProjection`, reads `aspace.recipes_snapshot()`, no caps leaked.
  `AddressSpaceStats {recipe_count, vm_size}` at `types.rs:843` is auxiliary;
  procfs reads recipes directly. Consumers: `tx-fs/src/procfs/read.rs`
  `render_maps:201`, `render_smaps:260`, `render_status:388` (VmLck summed from
  `flags.locked`; VmData currently 0). Goes direct via snapshot, NOT through
  `vm::checks` (those gate mutation: require_fault_recipe/publication/
  map_admission). Each maps line = one recipe; recipes ARE the VMAs.

## Verification done for this plan


- Four parallel code sweeps (structure, range_lock, scripts/execution,
  pmap/page-backed/private) — anchors above came from them; spot-check before
  publishing each chapter.
- Doc-grounding grep of object_model_v2 / PAGE_SUBSTRATE_v1 / PAGE_BACKED_v1 /
  VM_v1_2 (this session) — confirmed the three-level split and the canonical
  Frame disjunction; identified the v5-path anchor caveat.
