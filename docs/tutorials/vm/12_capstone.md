# Chapter 12 — Capstone: one heap page, end to end

We have taken the VM apart. Now watch one ordinary physical page move through the
whole machine, and see all three levels of the split move with it. The scenario
is the most common thing a program does: allocate some heap, write to it, fork,
and exit. Nothing exotic — but every chapter of this series fires at least once.

The running example, kept from Chapter 2: a single anonymous heap page at virtual
address `V`.

## Cast

- **Recipe** — the authoritative binding, a `VmEntry` in the `recipes` tree
  (Chapters 1, 3).
- **PTE** — the derived materialization, a leaf in the `pmap` (Chapters 4, 5).
- **Frame** — a physical page, alive under `map_count ∨ cache_ref` (Chapter 2).
- **PrivatePageSet** — the per-mapping authoritative store of written anonymous
  pages (Chapter 8).

## 1. `malloc` → `mmap`: the binding is born

The allocator calls `mmap(NULL, len, PROT_READ|PROT_WRITE, MAP_PRIVATE|
MAP_ANONYMOUS, …)`. The syscall seam (Chapter 10) decodes the six registers,
translates the flags to `Prot { read, write }` and `VmEntryFlags { shared: false }`,
finds a free range via `find_free_range` (Chapter 3), and tries `try_mmap`. It
takes an `ExclusiveWriter` over the chosen range (Chapter 6), commits one recipe —
`VmEntry { range: [V, V+page), prot: rw, backing: PrivateAnon }` — into the tree
(Chapter 3), and returns `V`.

**State now:** one recipe. **No PTE. No frame. No private set.** Pure binding;
zero materialization. This is demand paging (Chapter 0): the promise exists, the
delivery has not happened. `/proc/self/maps` already shows the line — because a
maps line *is* a recipe (Chapter 11):

```
00007f…000-00007f…1000 rw-p 00000000 00:00 0    [anon]
```

## 2. First touch is a read: the zero frame

The program reads `*V` before writing (a calloc-style zeroing check, say). The MMU
finds no PTE and traps. The fault handler (Chapter 7) runs:

1. **Resolve:** take `Materializer` on `[V, V+page)`, `require_fault_recipe` looks
   up `V`, finds the recipe, confirms read is permitted.
2. **Materialize:** `PrivateAnon` + read access → install the **shared zero
   frame** read-only (Chapter 8). No allocation; a thousand processes share it.
3. **Publish:** re-observe the recipe (unchanged), `publish_page` installs a
   read-only PTE pointing at the zero frame, taking a `MapPin`.

**State now:** recipe (unchanged) + a read-only PTE → zero frame. The zero frame's
`map_count` ticked up by one. The binding always said "writable anonymous"; the
materialization is read-only zero, because nothing has been written. They
disagree, harmlessly, by design.

## 3. First write: a private frame, recorded authoritatively

The program writes `*V = x`. The MMU finds a *read-only* PTE on a write and traps.
The fault handler runs again, now on a write:

1. **Resolve:** recipe permits write (`prot.write`). The existing PTE is read-only
   and points at the zero frame — this is a write to unwritten anonymous memory.
2. **Materialize:** `PrivateAnon` + write → allocate a fresh private frame,
   record it in the mapping's `PrivatePageSet` via `install_if_absent`, state
   `Exclusive` (Chapter 8). The set holds a `CachePin` on it (`cache_ref = 1`).
3. **Publish:** re-observe, then `publish_page_with_replacement(…, replace_existing
   = true)` swaps the read-only zero-frame PTE for a writable PTE on the private
   frame, taking a `MapPin` (`map_count = 1`). The zero frame's `map_count` drops.

**State now:** recipe + writable PTE → private frame; the frame recorded in the
`PrivatePageSet`. The frame is alive by **both** disjuncts — `cache_ref = 1` (the
set) and `map_count = 1` (the PTE). The bytes the program wrote now exist nowhere
else: the private set is *authoritative* (Chapter 8). The prefault batch (Chapter
7) may opportunistically materialize a few neighbouring pages while we are here.

## 4. `fork`: share the binding, demote the materialization

The program forks. `fork_aspace` (Chapter 8) runs under an `ExclusiveWriter` over
the whole user range:

1. **Binding (recipes):** `clone_shared` — the child's recipe tree shares every
   node, O(1) (Chapter 3). The child now has the same `[V, V+page)` recipe.
2. **Binding (private set):** `fork_share` — the child's `PrivatePageSet` shares
   the parent's treap; the page's state flips `Exclusive → SharedCow`. Both
   processes reference the one private frame, *recorded as shared*.
3. **Materialization (pmap):** `protect_range(…, without_write())` demotes the
   parent's PTE to read-only and shoots it down (Chapter 5: ASID-scoped, only
   harts that ran this address space). The child has no PTE yet.

**State now:** two recipes (parent & child, sharing tree nodes) → one
`SharedCow` private frame; parent PTE read-only, child PTE absent. The frame's
`cache_ref` is now 2 (both sets reference it). The binding (both say "writable")
and the materialization (read-only / absent) disagree on purpose — this is CoW
armed.

## 5. Child writes: the CoW break

The child writes `*V = y`. Its MMU finds no PTE, traps; the fault handler sees a
write to a `SharedCow` page:

1. **Materialize:** allocate a new frame, **copy** the shared page's bytes into
   it, and re-own the child's side via `replace_if_match` (`SharedCow →
   Exclusive`, Chapter 8) — a CAS that linearizes against any concurrent writer.
2. **Publish:** install a writable PTE on the child's fresh private frame.

The parent is untouched; its page is still `SharedCow` pointing at the original
frame. If the parent now writes, *its* fault does the same break and re-owns its
side — and the original frame's `cache_ref` falls as each side diverges. One page
copied, only because it was written; every unwritten page in the fork stays
shared. That is copy-on-write, and it read cleanly because binding and
materialization were always separate things allowed to disagree (Chapter 2).

## 6. `munmap`: unmap is not free

The child frees the allocation. `munmap` (Chapter 10) takes an `ExclusiveWriter`,
withdraws the recipe from the tree (Chapter 3) **first**, then `teardown_range`
(Chapter 4) removes the PTE, drops its `MapPin` (`map_count → 0`), and batches the
TLB shootdown (Chapter 5). The recipe's `PrivatePageSet` cap drops with the entry;
the private frame's `CachePin` releases (`cache_ref → 0`).

**State now:** binding withdrawn, materialization torn down, and with *both*
disjuncts at zero, the child's private frame is **freed** — its `FrameMeta.state`
hits zero (Chapter 2). The ordering mattered: recipe first, then PTE, so no
concurrent fault could re-materialize against a binding that was already gone
(Chapter 1's justification invariant).

For a *file-backed* page this is where unmap ≠ evict would show (Chapter 9):
`map_count` would hit zero but `cache_ref` would stay positive, and the frame
would survive in the page cache. For our private anonymous page, the set was its
only other holder, so it goes.

## 7. `exec`: the whole address space, gone

Finally the process `exec`s a new program. `exec_aspace` (Chapter 10) tears down
every PTE in the old address space (uncontended — exec's prologue reduced the
thread group to one), and `build_aspace_from_image` (Chapter 10) constructs a
fresh set of recipes from the new ELF's `PT_LOAD` segments and a new stack — again
*recipes only*, demand-faulting the new program's code and data on first touch,
returning us to step 1 for a different binary. The kernel high-half is never
touched through any of this; it was shared by reference from the start (Chapter
5).

## The three levels, in one trace

Read the scenario again through the split:

| Step | AddressSpace level | VmEntry level | Frame level |
|---|---|---|---|
| `mmap` | recipe committed; no PTE | binding value `PrivateAnon`; no payload yet | — |
| read fault | read-only PTE published | — | zero frame `map_count++` |
| write fault | writable PTE replaces it | private set populated (payload) | private frame `map_count=1, cache_ref=1` |
| `fork` | recipe tree shared O(1); parent PTE demoted | private set shared, `SharedCow` | `cache_ref=2`, one PTE RO |
| child write | child PTE published | child re-owns `Exclusive` | new frame; original `cache_ref--` |
| `munmap` | recipe withdrawn, then PTE | set cap dropped | both disjuncts → 0 → freed |
| `exec` | all PTEs torn down, recipes rebuilt | — | — |

Every row is the same idea: **the binding is the truth, the materialization is a
cache rebuilt from it, and the frame is alive under a disjunction of independent
claims.** Once you see that the recipe leads and the PTE follows — at the address
space, inside the entry, and down at the frame — the entire VM is one principle
applied three times. Linux arrives at the same behaviors through `mmap_lock`,
in-place page-table surgery, `anon_vma`, and an overloaded refcount; txKernel
gets them by keeping binding and materialization separate and letting them
disagree until a fault, a write, or a teardown reconciles them.

## The mapping table, once more

| Traditional VM | txKernel | Chapter |
|---|---|---|
| `mm_struct` | `AddressSpace` (recipes + pmap + RangeLock) | 1 |
| `vm_area_struct` + the VMA tree | `VmEntry` + `RecipeIndex` (EBR, path-copy) | 3 |
| page tables / `set_pte_at` | `VmPmap` / `publish_page` over `PmapIf` | 4, 5 |
| `struct page` refcounts | `FrameMeta` `map_count ∨ cache_ref ∨ pin_count` | 2, 9 |
| `mmap_lock` | `RangeLock` (range-scoped, two modes) | 6 |
| `handle_mm_fault` | `fault_script_with_ufd_dispatch` | 7 |
| `anon_vma` / CoW | `PrivatePageSet` (scoped, authoritative) + PTE demotion | 8 |
| `filemap_fault` / page cache | `PageContainer` / `materialize_page` | 9 |
| TLB shootdown / `switch_mm` | ASID-scoped `sbi_remote_sfence_vma_asid` / `activate_user_pmap` | 5 |
| syscall table → `do_mmap` | `tx-shims` `sys_*` → step-op `drive` | 10 |
| `/proc/<pid>/maps` (`task_mmu.c`) | `project_address_space` → procfs | 11 |

## Where to go next

- The consolidated **[TECHNICAL_REPORT.md](TECHNICAL_REPORT.md)** restates all of
  this as one continuous document.
- The [VFS series](../vfs/README.md) is the same split applied to files — Chapter
  2 there is the sibling of Chapter 2 here.
- The [reactor-and-threads series](../reactor-and-threads/README.md) is the async
  machinery (`drive`, steps, wait sources) that the fault handler and syscall seam
  ride on.
- The design specs: `docs/design/03_memory-vm/VM_v1_2.md` (note the
  `PrivatePageSet` divergence, Chapter 8), `PAGE_BACKED_v1.md`,
  `PAGE_SUBSTRATE_v1.md`, and the object model in
  `docs/design/00_meta-framework/object_model_v2.md`.

## Source anchors

The capstone synthesizes the whole series; per-claim anchors are in the chapter
each step references. The principal entry points one more time:

- `mmap`/`munmap`/`exec` seam: `crates/tx-shims/src/linux_syscall/vm.rs:223, 504`; `crates/tx-subsystems/src/vm/scripts.rs:212`
- fault handler: `crates/tx-subsystems/src/vm/execution.rs:255`
- `fork_aspace` / CoW: `crates/tx-subsystems/src/vm/execution.rs:111`; `crates/tx-subsystems/src/vm/structure/private.rs:945, 1007, 1077`
- recipes / pmap / rangelock: `recipe.rs:163`, `pmap.rs:157`, `range_lock.rs:114`
- frame disjunction: `docs/design/00_meta-framework/object_model_v2.md:131`
