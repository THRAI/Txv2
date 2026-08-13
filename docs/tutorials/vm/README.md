# The txKernel VM

A tutorial series on how txKernel builds virtual memory — the layer that turns
one set of memory syscalls (`mmap`, `munmap`, `mprotect`, `mremap`, `brk`,
`madvise`, …) and the CPU's page-fault trap into per-process address spaces — and
on the one structural idea that makes txKernel's VM different from a textbook
one.

## Who this is for

Developers who know the traditional Unix/Linux VM: the MMU and page tables, the
TLB, `mm_struct` and VMAs (`vm_area_struct`), demand paging through
`handle_mm_fault`, the `mmap` family, copy-on-write `fork`, the page cache, and
TLB shootdown. You do not need to know txKernel first; each chapter teaches the
traditional concept, then shows txKernel's version next to it.

If you have read the [VFS series](../vfs/README.md), this is the same idea
applied to memory — and Chapter 2 makes the connection explicit. If you have not,
this series stands alone.

## The one-sentence thesis

**A mapping's *authoritative binding* and its *materialization* are different
things with different lifetimes, so txKernel stores and reclaims them
separately.** The binding is *what should be mapped here* — recorded in a
per-address-space tree of recipes. The materialization is *what the hardware is
actually using right now* — page-table entries (PTEs) and the physical frames
they point at. A traditional VM fuses the two: a VMA and its page tables are
edited together under one big lock, and a `struct page`'s life is one overloaded
refcount. txKernel splits them, and the split recurs at three levels.

## The split lives at three levels

| Level | Authoritative binding (truth) | Derived materialization (cache) | Why they diverge |
|---|---|---|---|
| **AddressSpace** | recipes tree: `(VA range) → VmEntry` | pmap: PTEs, rebuildable, no retention | `mprotect`/`munmap`/`fork` rewrite the binding and *throw the PTEs away* |
| **VmEntry** | the recipe *value* (`VmEntryBacking`, offset) | `owners`: `Cap<PageContainer>` / `Cap<PrivatePageSet>` | the binding is immutable and cheap to clone; the payload caps reclaim on their own schedule |
| **Frame** | the `FrameMeta` identity slot | `map_count ∨ cache_ref` (∨ `pin_count`) | a page can be unmapped-but-cached, or cached-out-but-DMA-pinned |

Read the table top to bottom and you have the whole series. Everything else is
how each row is realized and what it buys.

## Why this is not gratuitous

The two hardest correctness problems in any VM are *exactly* binding-outlives-
materialization or materialization-outlives-binding problems:

- **`mprotect` a range while another thread faults in it.** The permission change
  must win, but the in-flight fault must not install a stale PTE. *Binding
  changed; materialization must be rebuilt, not patched.*
- **`fork` a multi-gigabyte process.** You cannot copy the pages. You demote the
  *materialization* to read-only and share the *binding*, and let the next write
  rebuild one page. *One binding, two address spaces, copy-on-write at the
  materialization layer.*

In a fused design these are special cases bolted on with flags, `mmap_lock` write
mode, and rmap walks. In txKernel they fall out of the layering: the recipe is
the truth, the PTE is a cache you can always rebuild from the truth, and the
`RangeLock` coordinates only the narrow window where the two must agree.

## How to read the code in this series

Code blocks are **simplified pseudocode** — error arms elided, some generics
dropped, control flow straightened — but **type names, field names, and method
names are accurate** and match the real source. Every chapter ends with
`file:line` anchors so you can read the real thing.

Where the shipped code diverges from the design doc (`docs/design/03_memory-vm/
VM_v1_2.md`, which is older than the code), the tutorial follows the **code** and
says so. The biggest such divergence — the per-mapping `PrivatePageSet` — gets a
whole chapter (Chapter 8).

## The series

Four arcs. **Chapters 0–3** build the conceptual core: traditional VM, the three
structures, the split, and the authoritative binding. **Chapters 4–5** are the
materialization and the hardware beneath it. **Chapters 6–9** are the machinery:
coordination, the fault handler, and the two kinds of backing. **Chapters 10–11**
are the outward surface: the syscalls and the `/proc` projection. **Chapter 12**
ties it together.

| # | File | Topic |
|---|------|-------|
| 0 | [00_what-is-virtual-memory.md](00_what-is-virtual-memory.md) | Traditional MMU/page-tables/TLB/VMAs, and the two ideas that reshape them |
| 1 | [01_the-address-space.md](01_the-address-space.md) | `AddressSpace` = recipes + pmap + RangeLock |
| 2 | [02_binding-and-materialization.md](02_binding-and-materialization.md) | **The split** — three levels, the reference ladder, why it exists |
| 3 | [03_recipes.md](03_recipes.md) | The recipes tree: the authoritative binding, EBR + path-copy |
| 4 | [04_pmap.md](04_pmap.md) | Pmap: the derived materialization (VM side) |
| 5 | [05_hal-boundary.md](05_hal-boundary.md) | `PmapIf`, the Sv39 walk, satp, and SMP TLB shootdown |
| 6 | [06_rangelock.md](06_rangelock.md) | RangeLock: range-scoped coordination without object locks |
| 7 | [07_the-fault-handler.md](07_the-fault-handler.md) | The fault handler: resolve → materialize → publish |
| 8 | [08_anonymous-memory-and-cow.md](08_anonymous-memory-and-cow.md) | Anonymous memory, the `PrivatePageSet`, and copy-on-write |
| 9 | [09_page-backed-and-the-page-cache.md](09_page-backed-and-the-page-cache.md) | File/tmpfs/shm/device backing through the page cache |
| 10 | [10_vm-system-calls.md](10_vm-system-calls.md) | The syscall seam: decode → fast path → drive async → errno |
| 11 | [11_projection.md](11_projection.md) | How VM state becomes `/proc/<pid>/maps` |
| 12 | [12_capstone.md](12_capstone.md) | Capstone: one heap page from `mmap` to `exec` teardown |

A single-file consolidated **[TECHNICAL_REPORT.md](TECHNICAL_REPORT.md)** covers
the same material in report form for readers who prefer one long document.

## The mapping table

Every chapter returns to this. The whole series is an expansion of it.

| Traditional VM | txKernel | Anchor |
|---|---|---|
| `mm_struct` | `AddressSpace` | `vm/structure/address_space.rs:40` |
| `vm_area_struct` (VMA) | `VmEntry` (a recipe value) | `vm/structure/types.rs:391` |
| the VMA tree (maple tree / rbtree) | recipes tree (`RecipeIndex`, EBR + path-copy) | `vm/structure/recipe.rs:163` |
| page tables (PTEs) | pmap, a *derived materialization* | `vm/pmap.rs:233` |
| `pte_t` install / `set_pte_at` | `publish_page` over `PmapIf` | `vm/pmap.rs:233`; `tx-hal/src/lib.rs:670` |
| `struct page` + `_refcount`/`_mapcount` | `FrameMeta` compound counters | `page_allocator/frame_meta.rs:219` |
| `mmap_lock` (`mmap_sem`) | `RangeLock` (range-scoped) | `vm/structure/range_lock.rs:114` |
| `handle_mm_fault` / `do_fault` | `fault_script_with_ufd_dispatch` | `vm/execution.rs:255` |
| anonymous memory + `anon_vma` | `VmBacking::PrivateAnon` + `PrivatePageSet` | `vm/structure/private.rs:196` |
| copy-on-write on `fork` | recipe share + PTE demotion | `vm/execution.rs:111` |
| file `mmap` / `filemap_fault` | `VmBacking::Page` + `materialize_page` | `page_backed/mod.rs:390` |
| the page cache (`address_space`) | `PageContainer` + `cache_ref` | `page_backed/mod.rs:390` |
| TLB shootdown / `flush_tlb_mm` | ASID-scoped `sbi_remote_sfence_vma_asid` | `boards/tx-hal-riscv64-qemu-virt/src/lib.rs:1407` |
| `satp` / `switch_mm` | `activate_user_pmap` | `boards/tx-hal-riscv64-qemu-virt/src/lib.rs:476` |
| the syscall table → `do_mmap` | `tx-shims` `sys_mmap` → `VmMapOp` | `tx-shims/src/linux_syscall/vm.rs:223` |
| `/proc/<pid>/maps` (`task_mmu.c`) | `project_address_space` → procfs | `vm/project.rs:33`; `tx-fs/src/procfs/read.rs:201` |

## What is genuinely the same as a normal VM

So you do not over-attribute novelty: the *shape* of the VM is conventional.
There is an `mm_struct`-like address space, a VMA-like mapping object held in a
balanced tree, hardware page tables, a demand-paging fault handler, anonymous and
file-backed memory, copy-on-write `fork`, a page cache, and the POSIX `mmap`
surface. If you have read the Linux VM you will recognise every box. The novelty
is concentrated in **one place**: each box is split along the
authoritative-binding ⟂ derived-materialization axis, and the reference types
(`Cap`, `VmCap`, typed frame pins) plus the `RangeLock` make that split explicit
and enforced. Chapter 2 is where that idea lives; everything else is how it plays
out.
