# Chapter 0 — What is virtual memory

Before txKernel, the shared vocabulary. This chapter is the traditional Unix/
Linux VM in one pass, so the rest of the series can say "like Linux's X, but…"
without re-teaching X. If you can explain `mm_struct`, a page fault, and
copy-on-write `fork` from memory, skim to [The two ideas](#the-two-ideas-that-
reshape-this) at the end — that is where txKernel starts.

## The problem virtual memory solves

A process wants to believe it owns a large, private, contiguous address space.
The machine has a smaller, shared, fragmented pile of physical RAM. Virtual
memory is the illusion that reconciles the two, maintained jointly by hardware
(the MMU) and the kernel.

Every address a user program touches is a **virtual address (VA)**. The MMU
translates it to a **physical address (PA)** on every access, by walking a
per-process **page table**. If the translation exists and permits the access,
the CPU proceeds. If not, the CPU traps into the kernel: a **page fault**.

## The page table and the MMU

A page table is a tree the hardware walks. On RISC-V Sv39 (txKernel's primary
target) it is three levels deep, mapping a 39-bit VA to a PA in 4 KiB pages. Each
leaf is a **page-table entry (PTE)**: a physical page number plus permission bits
(read/write/execute/user) and status bits (valid/accessed/dirty).

```
   VA ──split──▶ [ L2 index | L1 index | L0 index | page offset ]
                     │          │          │
   satp ─▶ L2 table ─┘          │          │
                  └─▶ L1 table ─┘          │
                            └─▶ L0 table ──┘─▶ PTE ─▶ physical frame
```

The CPU caches recent translations in the **TLB** (translation lookaside
buffer). The TLB is why changing a page table is not enough: stale TLB entries
must be invalidated (**TLB shootdown**), and on a multiprocessor that means
telling *every* CPU that might have cached the translation.

The register that points the MMU at the current page table is `satp` on RISC-V
(`CR3` on x86). Switching processes means switching `satp` — `switch_mm` in
Linux.

## The kernel's bookkeeping: `mm_struct` and VMAs

The hardware page table says *where* memory is mapped, in a format optimized for
the MMU. It is a poor place to answer kernel questions like "is this address
mapped, and with what permissions, backed by what file?" — walking it is lossy
(it does not record the backing file) and it only describes pages that are
*currently resident*.

So Linux keeps a parallel, software-friendly description:

- **`mm_struct`** — the per-process address space. One per process (shared by
  threads). Holds the VMA collection, the page-table root, assorted counters
  (`total_vm`, `rss`), and the lock that guards it all (`mmap_lock`).
- **`vm_area_struct` (VMA)** — one contiguous run of address space with uniform
  properties: `[start, end)`, permissions (`vm_flags`), and what backs it (a
  file + offset, or anonymous). A process has many; they live in a balanced tree
  (historically an rbtree, now a maple tree) keyed by address.

The VMA is the *authoritative description* of a mapping. The page table is
*derived* from it: a PTE exists only because some VMA says that range should be
mapped. **Hold onto that relationship — it is the entire subject of this
series.** Linux has it too; it just does not enforce it as sharply as txKernel
will.

## Demand paging: the fault path

When you `mmap` a region, Linux does almost nothing: it creates a VMA and
returns. No page tables, no physical pages. The work happens lazily, on first
access, through the fault handler:

```
user touches an unmapped VA
   └─▶ MMU walk fails ─▶ CPU page-fault trap ─▶ handle_mm_fault()
          1. find the VMA covering the faulting VA   (none → SIGSEGV)
          2. check the access against vm_flags        (violation → SIGSEGV)
          3. get a physical page:
               anonymous  → allocate a zeroed frame
               file-backed→ read from the page cache (may block on I/O)
          4. install a PTE pointing at that frame
          5. return; the CPU retries the instruction
```

This is **demand paging**: physical memory is committed only when actually
touched. The VMA promises the mapping; the fault handler delivers it one page at
a time.

## Anonymous vs file-backed

Two kinds of memory, distinguished by what a fault fills the page *from*:

- **File-backed.** The mapping is a window onto a file. A fault reads the page
  from the **page cache** — the kernel's RAM cache of file contents, shared by
  every process mapping or `read`-ing that file. `MAP_SHARED` writes go back to
  the cache (and eventually the disk); `MAP_PRIVATE` writes copy-on-write.
- **Anonymous.** Not backed by any file: the heap (`brk`), the stack, `malloc`'s
  big allocations, `MAP_ANONYMOUS`. A read fault yields zeros; a write fault
  yields a private zeroed frame. Linux tracks these with `anon_vma` so it can
  later find every PTE pointing at an anonymous page (needed for swap).

## Copy-on-write and `fork`

`fork` must give the child a private copy of the parent's entire address space.
Copying gigabytes would be absurd, and most of it is never written. So both
processes **share** the physical pages, mapped **read-only**, and the VMAs are
duplicated. The first write to a shared page faults; the kernel allocates a fresh
copy for the writer, points its PTE at the copy, and leaves the original to the
other process. This is **copy-on-write (CoW)**.

CoW is the canonical case where *one logical binding* (the VMA says "this is
writable anonymous memory") and *its materialization* (a read-only PTE shared
with another process) deliberately disagree, and the fault handler reconciles
them. Again: hold onto that.

## The other syscalls

Beyond `mmap`/`munmap`, the surface that edits an address space:

- **`mprotect`** — change permissions on a range. May split a VMA.
- **`mremap`** — grow, shrink, or move a mapping.
- **`brk`** — the legacy heap pointer; modern allocators mostly use anonymous
  `mmap`, but `brk` persists.
- **`madvise`** — hints (`MADV_DONTNEED` drops pages; `MADV_WILLNEED` prefaults).
- **`msync`** — flush a file mapping's dirty pages back to disk.
- **`mincore`** — query which pages of a range are currently resident.

All of them edit VMAs and then reconcile the page tables. All of them, in Linux,
take `mmap_lock`.

## Where the traditional design strains

The Linux VM is battle-tested, but its difficulty concentrates in a few places,
and it is worth naming them now because txKernel's design is largely a response
to them:

- **`mmap_lock` contention.** One reader/writer semaphore guards the whole
  address space. A `mprotect` on one range and a fault on a *completely
  different* range serialize against each other. Per-VMA locking and RCU VMA
  walks have been bolted on over years to claw this back.
- **rmap and `anon_vma`.** To swap a page out, or migrate it, the kernel must
  find *every* PTE that maps a given physical page — the reverse of the normal
  VA→PA direction. The reverse-mapping machinery (`anon_vma`, the
  `address_space` interval tree) is among the subtlest, most memory-hungry code
  in the kernel.
- **The overloaded `struct page` refcount.** `_refcount` and `_mapcount`
  together encode "is this page free, mapped, cached, pinned for DMA, …" and the
  rules for reading them correctly are notoriously delicate.
- **VMA edits and page-table edits are coupled.** Splitting a VMA, tearing down
  PTEs, and shooting down the TLB happen together under the write lock, which is
  why the lock is held so widely.

## The two ideas that reshape this

txKernel keeps the *shape* — address space, VMA-like mapping, balanced tree,
demand-paging fault handler, CoW `fork`, page cache. It changes two things, and
the whole series follows from them.

**Idea 1 — A PTE is a cache you can always rebuild from the binding.** The
recipes tree (txKernel's VMA tree) is the single source of truth. Page-table
entries are *derived*: every PTE is justified by a recipe, and tearing the recipe
down invalidates the PTE. This makes `mprotect`, `munmap`, and CoW *cheap to
reason about* — you rewrite the binding and discard the materialization, rather
than surgically patching live page tables. And it means reads of the binding can
be lock-free (snapshot the tree under an epoch guard), with a narrow lock —
`RangeLock` — coordinating only the moment binding and materialization must
agree.

**Idea 2 — A physical frame's life is several independent claims, not one
refcount.** A frame is alive if it is mapped into some address space *or* present
in the page cache *or* pinned for DMA. Each claim is counted separately
(`map_count`, `cache_ref`, `pin_count`), so "unmapped but still cached" and
"evicted from cache but still DMA-pinned" are ordinary, representable states
rather than refcount-rule trivia. This is the *same* split as Idea 1, one layer
down: the frame's identity (its slot) outlives any particular claim on it.

These also let txKernel make three simplifying **commitments** the rest of the
series leans on:

- **No swap.** Anonymous pages are pinned until explicitly torn down. The fault
  handler never pages in from a swap device — which removes the single biggest
  reason Linux needs global rmap.
- **No KPTI.** Kernel mappings are present in every address space's page table
  (the upper half), shared by reference. `fork` never duplicates them. (Chapter
  5.)
- **No global rmap.** There is no frame→PTE reverse index across all address
  spaces. As we will see in Chapter 8, txKernel keeps a *scoped* per-mapping
  index for anonymous CoW — which the no-*global*-rmap rule still permits — but
  nothing resembling Linux's `anon_vma`.

## Where we go from here

Chapter 1 opens the `AddressSpace` and names its three parts. Chapter 2 — the
center of the series — states the binding/materialization split formally and
gives the five reasons it pays for itself. From there we descend through the
recipes tree, the pmap, the hardware, the lock, the fault handler, the two
backings, the syscalls, and the `/proc` projection, before reassembling
everything in the capstone.

## Source anchors

This chapter is background; txKernel anchors begin in Chapter 1. For the design
rationale and the commitments:

- VM subsystem spec: `docs/design/03_memory-vm/VM_v1_2.md` §intro, §1
- Page substrate (frames, `FrameMeta`, pmap stages): `docs/design/01_substrate/PAGE_SUBSTRATE_v1.md`
- Page-backed model (page cache, backings): `docs/design/03_memory-vm/PAGE_BACKED_v1.md`
- The object-model split this all instantiates: `docs/design/00_meta-framework/object_model_v2.md` §3
