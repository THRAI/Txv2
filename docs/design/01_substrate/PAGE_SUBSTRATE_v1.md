# Page Substrate

<!-- txdoc:01-SUBSTRATE-PAGE-SUBSTRATE-V1 -->

**Status.** v1 (2026-04-19). Draft.

**Purpose.** Specify the substrate layer that sits below the VM subsystem and above the HAL: the physical frame allocator, the per-frame metadata (`FrameMeta`), the pmap setup stages, the slab-based kernel heap, and the handoffs between them. This document is the contract between HAL's boot-time delivery and the kernel's runtime page management.

**Scope.** Everything from "HAL has delivered `BootInfo.memory_regions`" to "the frame allocator, heap, and `FrameMeta` are steady-state and subsystems can allocate." Does *not* cover `PageContainer`, `PageContainerKind`, `RNodeBacking`, or the user-visible VM object model — those live in `PAGE_BACKED_v1.md`.

**Audience.** VM subsystem implementers, anyone writing code that consumes frames or the kernel heap, reviewers auditing boot sequencing.

**Key design commitments** (established in prior rounds; restated here because they shape everything):

1. **No swap.** Anonymous pages are not evicted to backing storage. Memory pressure surfaces as synchronous allocation failure; callers either fail the operation or, for kernel-critical allocations, panic. File-backed pages may be evicted (to their backing filesystem's pager), but anonymous pages are pinned until explicit teardown.

2. **No KPTI + kernel mappings always present.** Every page-table root (bootstrap and every AddressSpace) has the kernel high-half mapped. The direct map, kernel text/rodata/data, and MMIO windows are visible during both user and kernel execution. No satp/DMW swap on syscall or trap entry.

3. **Per-hart kernel stack, stackless coroutines.** No per-thread kernel stacks. The hart's kernel stack services trap handling, coroutine polling, and kernel work indiscriminately. Task state lives in heap-allocated `Future` state machines and in `ExecContext` — not on a per-task stack.

4. **Boot-time thorough frame collection.** Memory map is parsed once at boot; all RAM not reserved (kernel image, initrd, FrameMeta array, PT_NODE_POOL) becomes the frame allocator's free pool. No hot-add, no memory offlining. Simplifies the allocator to a static-size bitmap plus per-frame metadata.

5. **BootInfo uses `'static` references, not `Vec`/`String`.** This matches the axHal-style HAL contract in [`HAL_v1.md`](HAL_v1.md). The consequence: no kernel heap is required before the frame allocator is up.

**Companion documents.**

- [`HAL_v1.md`](HAL_v1.md) — axHal-style static platform selection, boot sequence, pmap primitives (`PmapReservation`, kernel mapping reserve/commit/rollback, unmap invalidations, future `PmapCommitBatch`), `PT_NODE_POOL`, trap infrastructure. This document's preconditions are HAL's deliverables.
- [`../00_meta-framework/MODULE_MAP_v1.md`](../00_meta-framework/MODULE_MAP_v1.md) §3 — foundation/HAL layout and boundary rules.
- [`../00_meta-framework/object_model_v2.md`](../00_meta-framework/object_model_v2.md) §3, §7 — Frame as compound-payload entity; MapPin / CachePin / DmaToken as typed evidence.
- [`02_INVARIANTS_v5.md`](../../Txv3/02_INVARIANTS_v5.md) — STEP, MAP, and HAL/substrate boundary discipline.
- [`../03_memory-vm/MEMORY_IO_ARCHITECTURE_v1.md`](../03_memory-vm/MEMORY_IO_ARCHITECTURE_v1.md) — canonical allocation-gateway, memory-pressure, reclaim-provider, and file-I/O ownership contract above this substrate.
- [`../03_memory-vm/PAGE_BACKED_v1.md`](../03_memory-vm/PAGE_BACKED_v1.md) — consumes frame and page-cache substrate primitives.

### Zone-derived type policy
<!-- txdoc:PAGE-SUBSTRATE-ZONE-DERIVED-TYPE-POLICY-1 -->

PAGE_SUBSTRATE sits below semantic subsystems, but it supplies the compound
payload evidence used by them:

| Substrate declaration | Public evidence | Reclamation role |
|---|---|---|
| physical frame | `FrameMeta` plus typed contributors (`MapPin`, `CachePin`, `DmaPin`, `GiftPin`) | compound payload counters and retainers, no fake `Zone<VmEntry>` |
| free bitmap rows | bitmap bits and reservations | allocator state, not entities |
| pmap intermediate nodes | HAL/substrate-owned page-table nodes | pmap materialization, not caps |
| slab pages / zone backing pages | substrate allocation units | hidden storage for policy-based zones |

The frame allocator does not expose raw policy parameters to VM or filesystem
code. Upper subsystems receive role-shaped frame evidence through VM/PageBacked
operations.

---

## 1. The substrate's job
<!-- txdoc:PAGE-SUBSTRATE-THE-SUBSTRATES-JOB-1 -->

Between HAL and the VM subsystem sits a layer responsible for three things:

- **Physical frames.** A bitmap-backed `PageAllocator` handing out typed frame reservations and owned-frame tokens, with companion per-PPN metadata (`FrameMeta`). Raw PPNs remain observable for encoding PTEs and direct-map access, but they are not freeing authority.
- **Kernel heap.** A slab allocator, backed by the frame allocator, registered as `#[global_allocator]`. Serves `Box`, `Vec`, `String`, and every `alloc` call made by the kernel above this layer.
- **Pmap extension.** Growing the kernel's page table from the bootstrap state (256 MB direct map + small MMIO window) to cover all of RAM and all platform-discovered MMIO. This happens before the frame allocator comes up, using `PT_NODE_POOL` as the intermediate-page source.

Nothing else lives here. Specifically:

- **No `PageContainer`, no `RNode`, no `VmEntry`.** Those are kernel-level; this document ends before they exist.
- **No user AddressSpace machinery.** A user AddressSpace is created above this layer by inheriting the kernel portion of the page table (always-present kernel mappings) and adding a fresh user-side tree.
- **No coroutine or reactor.** The substrate runs during synchronous boot and synchronously on the calling hart after that. It is not a driver, not a service, not a subsystem.

---

## 2. Preconditions from HAL
<!-- txdoc:PAGE-SUBSTRATE-PRECONDITIONS-FROM-HAL-1 -->

By the time this substrate begins, HAL has delivered:

| Deliverable | Source (HAL §) | State |
|---|---|---|
| `BootInfo` (static) | HAL_v1 §7 | Published; `memory_regions`, `kernel_image`, `initrd` populated |
| Bootstrap page table (satp/DMW live) | HAL_v1 §5.2, §10.5 | MMU on; kernel direct map covers first 1 GiB in the high half; kernel image has a high alias; RV64 QEMU enters Rust through the high alias and removes the temporary low identity bridge after BootInfo consumes the firmware DTB |
| `PT_NODE_POOL` / early PT nodes | HAL_v1 §10.1 | Static early page-table nodes available for pmap intermediate-table allocation; `PmapIf::alloc_pt_node()` callable |
| `PlatformInfo` | HAL_v1 §8 | Published; MMIO region table ready for mapping |
| Early UART + logger | HAL_v1 §9 | Usable for diagnostics during substrate bring-up |
| Minimal trap infrastructure | HAL_v1 §5.2, §11 | Minimal panic vector installed; full kernel trap vector installed later in H3 |
| `PmapReservation` / `PmapUnmapResult` / shootdown surface | HAL_v1 §10 | Available for kernel pmap mutations; post-shootdown frame accounting remains substrate-owned |

This substrate does not require:

- Heap. BootInfo is `'static`-referenced; substrate initialization before slab bring-up (§5) uses stack locals and static storage only.
- Reactor. Substrate bring-up is synchronous; every operation runs to completion on the BSP before returning.
- SMP. APs are not yet booted when BSP substrate initialization begins. Stage
  3 (§4.3) occurs before AP bring-up; each AP must call
  `tx_substrate::init_on_ap(cpu)` before publishing itself online so epoch and
  zone per-CPU state exists before scheduler admission.

**Ordering constraint.** The axHal-style HAL boot mainline is the H0-H4 sequence from HAL_v1 §5. Substrate begins inside H3:

```text
H0 firmware/reset handoff
 → H1 platform __start: stack, BSS, bootstrap pmap, minimal trap vector,
      early console, static BootInfo skeleton, preserved firmware registers
 → H2 tx_hal::entry::<P, K>: translate BootHandoff, validate BootInfo,
      install early per-cpu pointer
 → H3 tx_kernel::kernel_main::<P>(BootHandoff):
      P::init_early(handoff)
      substrate::init::<P>()          ← this document's entry point
      P::init_later(handoff)
      install full trap vectors
      reactor/scheduler/subsystems
 → H4 downstream subsystem init and userspace entry
```

The earlier smoke boot contract may reach `tx_kernel::kernel_main::<P>` and
emit the serial sentinel before the full substrate preconditions are live. That
is an executable handoff test only. This page substrate contract begins when
the platform has graduated to the substrate-ready boot contract from HAL_v1
§5.0: static `BootInfo`, bootstrap pmap/direct map, early PT-node pool,
minimal trap vector, platform MMIO facts, and pmap mutation surface are all
available.

RV64 QEMU currently has the low-to-high bootstrap-pmap slice: an Sv39 root with
a temporary 1 GiB identity leaf for QEMU RAM during the H1/H2 crossing, a 1 GiB
high direct-map leaf for the same RAM window, a coarse high kernel alias for
early execution/storage coverage, a high-VMA/low-LMA linker layout, an
assembly-only low trampoline that rewrites `sp`/`gp` before `rust_entry`, a
high sentinel that proves `pc`/`sp`/`gp`, live identity teardown, a fixed
PT-node pool, direct-map extension, boot MMIO mapping, and
typed frame-allocator-backed PT-node allocation after substrate init. This proves
the MMU handoff and gives substrate code a place to hang early page-table
allocation tests.
The boot-owned statics that back `BootInfo` and the bootstrap pmap are captured
through the board-private `BootStaticBag`. RV64 QEMU's pre-entry assembly uses
only suffixed `_load` symbols while translation is off; high Rust constructs
the bag once after the `satp`/jump boundary, publishes `BootInfo`, proves high
`pc`/`sp`/`gp`, clears the identity bridge, then consumes the bag into the
post-entry typestate before substrate runs. The bag may still remember the DTB
as a raw value, but the parsing/dereference authority is gone. Substrate
consumes published HAL facts rather than reconstructing linker, static, or
firmware-pointer addresses. `BootInfo` must also mark firmware/loader RAM below
the kernel load base as reserved; otherwise this phase can carve `FrameMeta[]`
over OpenSBI-owned pages. Remaining substrate-ready pmap work includes final
process-root materialization, range protect, committed intermediate teardown,
and SMP/global shootdown aggregation.

`substrate::init::<P>()` runs before any SMP bring-up and before downstream subsystem init. By its return, the following are live:

- The frame allocator, serving typed `reserve_frame()` / `reserve_run()` calls.
- The `FrameMeta` array, one dense entry per covered RAM PPN, placed in direct-mapped memory.
- The current direct map has been extended or validated to cover all RAM needed
  by allocator metadata, allocator-free RAM, and direct-map zeroing.
- Platform MMIO regions have kernel mappings installed from `PlatformInfo`.
- Pmap intermediate allocation uses typed page-table frames first and retains
  `PT_NODE_POOL` as an exhaustion fallback.
- A zeroed permanent frame is claimed as the kernel zero frame and recorded as
  a never-free anchor.
- The slab-based kernel heap is initialized and registered as the no-std global
  allocator on kernel targets.

After `substrate::init::<P>()` returns, subsequent CoreInit phases (publisher
tables, trace infrastructure, etc.) can use `Box`, `Vec`, and any
`alloc`-dependent API.

### 2.1 PAGE_SUBSTRATE_v1 completion checklist
<!-- txdoc:PAGE-SUBSTRATE-COMPLETION-CHECKLIST-1 -->

The executable RV64 QEMU path has frame allocation, boot-memory planning,
direct-map/MMIO pmap extension, typed PT-node allocation, kernel-only
shootdown accounting, safe in-place kernel leaf protect, committed
intermediate-table ownership/lifetime tracking for kernel mappings, final high-kernel alias
permissions for text/rodata/data/bss/boot-stack pages, `PmapRoot`/ASID
create-destroy for VM-owned roots, ASID-scoped page shootdown batches,
no-alloc VM-facing page-range reserve/commit, unmap, and protect wrappers,
permanent anchors for boot metadata and bootstrap page tables, the permanent
zero frame, the slab heap, and an RV64 panic/spin trap vector plus typed trap
classification installed at HAL entry and after `init_later`. RV64 QEMU also
initializes AP-local epoch/zone state before online publication and uses SBI
RFENCE for remote pmap shootdown once APs are online. The remaining
PAGE_SUBSTRATE_v1 exit criteria are:

- superpage/multi-frame shootdown accounting beyond page-sized pins;
- kernel-managed SMP shootdown on platforms other than the LA64 QEMU
  generation-mailbox implementation when firmware RFENCE is unavailable;
- the full trap shell/user-return path before user/VM faults are enabled.

### 2.2 Address and pointer boundary
<!-- txdoc:PAGE-SUBSTRATE-ADDRESS-AND-POINTER-BOUNDARY-1 -->

PAGE_SUBSTRATE is one of the few layers allowed to traffic in address values as
first-class data. It owns `Ppn`/`PhysAddr` frame identity, `FrameMeta[ppn - ppn_base]`
indexing, direct-map conversion, pmap-facing map-count transitions, and the
boot handoff from HAL's pmap facts to steady-state allocation.

The rule is intentionally narrow:

- raw `Ppn`, `PhysAddr`, and kernel `VirtAddr` values are accepted at allocator,
  FrameMeta, pmap, and direct-map helper boundaries;
- explicit arithmetic over address-bearing integers is encapsulated in helpers
  such as `ppn_to_vaddr`, `vaddr_to_ppn`, alignment, range splitting, and pmap
  index extraction;
- direct-map helpers may produce a kernel pointer for copying, zeroing, or page
  table access, but the pointer is scoped to that substrate operation;
- upper semantic subsystems receive role evidence (`OwnedFrame`, `MapPin`,
  `CachePin`, `DmaPin`, `GiftPin`, `PtFrame`) or entity evidence (`Cap<T>`,
  `Weak<T>`, `IdentRef<'g, T>`), not freeing authority hidden in a raw address.

This keeps the address dialect local. Above PAGE_SUBSTRATE, ordinary kernel code
should not compute `DIRECT_MAP_BASE + phys`, interpret a `usize` as a pointer,
or retain raw page-table addresses. It asks substrate/VM to materialize the
operation and then continues in semantic evidence terms.

---

## 3. Data structures
<!-- txdoc:PAGE-SUBSTRATE-DATA-STRUCTURES-1 -->

### 3.1 FrameMeta
<!-- txdoc:PAGE-SUBSTRATE-DATA-STRUCTURES-FRAMEMETA-1 -->

One entry per physical page in the system. Indexed by PPN. Flat array, contiguous in direct-mapped memory.

```rust
#[repr(C, align(8))]
pub struct FrameMeta {
    /// Packed counters. All-zero means the frame is free (matching the
    /// bitmap); any nonzero field indicates the frame is held.
    ///
    /// Bit layout:
    ///   bits  0..10  — refcount    (generic owner/retainer count)
    ///   bits 10..20  — map_count   (PTEs referencing this frame)
    ///   bits 20..28  — cache_ref   (PageContainer page-index entries referencing this frame)
    ///   bits 28..32  — pin_count   (DmaToken and other pinning holders)
    state: AtomicU32,

    /// Physical-lifecycle flags. Separate word so updates don't contend with
    /// `state`'s CAS traffic. File-page dirty/writeback state is not stored
    /// here; PageSlot owns that semantic state.
    ///
    /// Bit layout:
    ///   bits 0-1  — reserved for physical-lifecycle extensions
    ///   bit 2    — direct_mapped (in kernel direct map; never reclaimed)
    ///   bit 3    — reserved      (kernel image, PT_NODE_POOL, FrameMeta array)
    ///   bits 4..16 — reserved for future use
    flags: AtomicU16,
}

const _: () = assert!(core::mem::size_of::<FrameMeta>() == 8);
```

**Size.** 8 bytes per physical frame (6 bytes of content, padded to 8 for alignment). For 4 GB RAM at 4 KiB pages (1M frames) = 8 MiB of FrameMeta. For 128 MiB RAM (32K frames) = 256 KiB. For 16 GiB = 32 MiB. Acceptable overhead (~0.2% of RAM).

**Bit-width rationale.**

- `refcount` (10 bits, max 1023): generic frame owner/retainer count. It includes `OwnedFrame`, permanent anchors, pmap page-table ownership, retained frame handles, and `GiftPin` transfer retention. Role-specific liveness stays in `map_count`, `cache_ref`, and `pin_count`; v1 has no separate allocation pin.
- `map_count` (10 bits, max 1023): PTEs installing this frame across address spaces. An anonymous page in a 1000-process fork scenario hits ~1000; 1023 is tight but workable. If we need headroom, we widen to 12 bits by shrinking cache_ref to 6; revisit under benchmarking.
- `cache_ref` (8 bits, max 255): PageContainer page-index inclusions. Typically 1 (the page lives in one PC); reflink elevates to N where N is the number of reflinking PCs. 255 is ample for anticipated use cases.
- `pin_count` (4 bits, max 15): concurrent DMA operations on this page. 15 concurrent outstanding DMA requests is more than any sane device driver needs.

**Overflow discipline.** Each counter increment is a CAS that checks bounds: attempting to exceed the max fails the CAS, caller receives `Err(ErrTooManyRefs)`, which propagates as `ENOMEM` at the syscall boundary. No silent wrap.

**Free predicate.** `FrameMeta.state == 0` means no semantic owner or role pin holds the frame. In the v1 bitmap backend, allocator-free frames are represented by both `state == 0` and a set free-bitmap bit. A `FrameReservation` is the one transient exception: it has cleared the bitmap bit but has not yet committed `refcount = 1`, so it is allocator-claimed and rollback-only, not publishable. In the planned CPU-magazine backend, `state == 0` frames may be owned by a CPU magazine instead of the global bitmap.

```
state == 0 && bitmap_bit_set  ⇔  frame is globally allocator-free in v1
```

Violations indicate a bug; the allocator asserts this on every alloc/free in debug builds.

### 3.2 Free bitmap
<!-- txdoc:PAGE-SUBSTRATE-DATA-STRUCTURES-FREE-BITMAP-1 -->

One bit per covered dense PPN. Set = free. Cleared = allocated.

```rust
/// Words of the free bitmap; bit ((ppn - ppn_base) % 64) of word
/// ((ppn - ppn_base) / 64) encodes the free state of PPN `ppn`.
/// Atomic for concurrent alloc paths.
static FREE_BITMAP: &'static [AtomicU64] = ...;  // placed at boot
```

**Size.** 1 bit per 4 KiB page = 32 KiB per GiB of RAM. For 4 GB RAM = 128 KiB. Negligible.

**Placement.** Allocated in direct-mapped memory at boot, adjacent to FrameMeta. See §4.5.

**Count bookkeeping.** A separate `AtomicUsize` tracks free-page count for diagnostics:

```rust
static FREE_PAGE_COUNT: AtomicUsize = AtomicUsize::new(0);
```

Not consulted on the alloc fast path; maintained for `/proc/meminfo` and debug logging.

### 3.3 Layout invariants
<!-- txdoc:PAGE-SUBSTRATE-DATA-STRUCTURES-LAYOUT-INVARIANTS-1 -->

The FrameMeta array and free bitmap are flat, contiguous, and indexed by dense
offset from `ppn_base`, where `ppn_base` is the lowest page covered by
normalized RAM. The covered interval is `[ppn_base, ppn_base + frame_count)`.
Raw `Ppn` values remain the external identity; the backend subtracts
`ppn_base` before indexing metadata or bitmap words.

Gaps inside the covered interval (holes in the memory map, MMIO carveouts,
hardware-reserved ranges, kernel image, initrd, metadata storage, bootstrap
page tables) have entries but their bitmap bit is *cleared* and their
FrameMeta is marked `reserved`. This avoids wasting metadata on low physical
addresses below RAM while preserving O(1) raw-PPN lookup within the RAM window.

For a system with RAM from `0x8000_0000` to `0xC000_0000` and no RAM below,
`ppn_base = 0x80000` and `frame_count = 0x40000`. A raw PPN
`0x80002` indexes metadata row `2`.

---

## 4. Bring-up sequence
<!-- txdoc:PAGE-SUBSTRATE-BRING-UP-SEQUENCE-1 -->

`substrate::init()` runs to completion on the BSP during CoreInit. It executes five phases in order. Each phase establishes preconditions the next relies on.

### 4.1 Phase 1: Parse and validate memory regions
<!-- txdoc:PAGE-SUBSTRATE-BRING-UP-SEQUENCE-PHASE-1-PARSE-AND-VALIDATE-MEMORY-REGIONS-1 -->

Consume `BootInfo.memory_regions`. For each region:

- Classify as `Usable` (RAM available for general allocation), `Reserved` (kernel image, firmware, device-tree blob, initrd, other), or `Mmio`/`Nonram` (non-RAM physical addresses; devices).
- Record the usable-region list as a static slice ordered by base address.
- Compute total RAM = sum of usable-region sizes.
- Compute `ppn_base` and `max_ppn` from normalized RAM, then cover the dense
  interval `[ppn_base, max_ppn)`.

Validation:
- Regions must not overlap. Overlap → panic (firmware bug).
- Kernel image range (from linker symbols `__kernel_start`, `__kernel_end`) must fall entirely within a `Reserved` region. If it overlaps a `Usable` region, the implementation subtracts it (splits the usable region around the kernel image).
- Initrd range, if present, must likewise be `Reserved` or subtracted from usable.
- Bootstrap pmap page-table ranges from `BootstrapPmapInfo.reserved_page_tables`
  are subtracted from usable regions.

After phase 1, `total_ram`, `ppn_base`, `max_ppn`, and a normalized usable-region list are in hand. No allocation yet.

### 4.2 Phase 2: Extend the kernel direct map
<!-- txdoc:PAGE-SUBSTRATE-BRING-UP-SEQUENCE-PHASE-2-EXTEND-THE-KERNEL-DIRECT-MAP-1 -->

`tx_substrate::init::<P>()` computes the page-covered RAM end from `BootInfo`
and calls `PmapIf::extend_direct_map()` before carving allocator metadata when
RAM exceeds the published bootstrap direct-map window. The current executable
direct-map mutation path is limited to idempotent 1 GiB kernel leaves; finer
2 MiB / 4 KiB kernel mappings are used in phase 3 for platform MMIO.

The bootstrap page table covers the first 1 GB of physical RAM at the direct-map base. If `max_physical_address > 1 GiB`, extend.

Strategy: **1 GB L2 superpages**. Sv39 / LA64 both support 1 GB leaf entries at L2. Each L2 slot in the kernel root covers 1 GB. The kernel high-half has 256 L2 slots (top half of the root's 512 entries); even for 256 GB of RAM, this is comfortable.

Algorithm:

```
for each GB-boundary up to max_physical_address:
    if L2 slot for that GB is already mapped (bootstrap covered it):
        continue
    reserve a kernel direct-map 1 GiB leaf
    commit a PTE pointing at (GB_addr | V | R | W | X=0 | G | A | D)
```

No intermediate tables required. No PT_NODE_POOL consumption.

After phase 2, every physical byte of RAM is addressable via the kernel direct map at `direct_map_base + ppn * 4096`.

**Issue shootdowns?** No: the bootstrap root is the only page table live at this point (no APs up, no user processes). New entries become visible to the BSP via `sfence.vma` (RV) / `invtlb` (LA) on the installing hart. No cross-core invalidation needed.

### 4.3 Phase 3: Map additional platform regions
<!-- txdoc:PAGE-SUBSTRATE-BRING-UP-SEQUENCE-PHASE-3-MAP-ADDITIONAL-PLATFORM-REGIONS-1 -->

Consume `PlatformInfo.mmio_regions` (device MMIO windows not already in the early MMIO window). For each page-covered region, install a mapping in the kernel portion of the bootstrap page table using `PmapIf::reserve_kernel_mapping()` followed immediately by `PmapIf::commit_kernel_mapping()`.

Granularity: prefer 2 MB pages when the physical address, virtual address, and remaining length are all 2 MB aligned. Fall back to 4 KB pages for aligned tails or small device windows. Intermediate page-table pages come from `PT_NODE_POOL` (HAL §7.4.2) because this phase runs before the frame allocator and slab exist.

The current executable API is still boot-oriented: substrate commits
reservations in the same loop, but abandoned reservations now have an explicit
rollback path that clears newly-created branch PTEs and returns `PT_NODE_POOL`
intermediates. Kernel unmap returns a `PmapUnmapResult` with an invalidation
token; the current RV64 QEMU path uses local `sfence.vma` plus SBI RFENCE to
online remote harts, while higher-level SMP/global aggregation remains a later
substrate/VM slice.

After phase 3, the kernel can access any registered MMIO region through a known virtual address.

### 4.4 Phase 4: Place FrameMeta and free bitmap
<!-- txdoc:PAGE-SUBSTRATE-BRING-UP-SEQUENCE-PHASE-4-PLACE-FRAMEMETA-AND-FREE-BITMAP-1 -->

Compute sizes:

```
frame_count     = max_ppn - ppn_base
frame_meta_size = frame_count * 8  (bytes)
bitmap_size     = (frame_count + 63) / 64 * 8  (bytes, rounded to word)
total_meta      = frame_meta_size + bitmap_size
```

Find placement: scan usable regions (post phase-1 normalization) for the first region with sufficient size at the start. Carve `total_meta` bytes off that region's low end. Mark the carved range as reserved (it will never be free in the bitmap).

Zero the placed regions:

```rust
unsafe {
    core::ptr::write_bytes(frame_meta_ptr as *mut u8, 0, frame_meta_size);
    core::ptr::write_bytes(bitmap_ptr as *mut u8, 0, bitmap_size);
}
```

FrameMeta all-zero = "nothing held" (consistent with free). Bitmap all-zero = "nothing free yet" — we will fill in free bits in phase 5.

Publish pointers:

```rust
static mut FRAME_META_BASE: *mut FrameMeta = ...;
static mut BITMAP_BASE: *mut AtomicU64 = ...;
static mut PPN_BASE: Ppn = ...;
static mut FRAME_COUNT: usize = ...;
```

Wrapped behind read-only accessors after substrate init completes.

### 4.5 Phase 5: Populate the free bitmap
<!-- txdoc:PAGE-SUBSTRATE-BRING-UP-SEQUENCE-PHASE-5-POPULATE-THE-FREE-BITMAP-1 -->

For each usable region *after* FrameMeta/bitmap carve-out:

- Set the bitmap bits covering PPNs in that region.
- Increment `FREE_PAGE_COUNT` by the region's size in pages.

For reserved regions (kernel image, initrd, FrameMeta/bitmap, PT_NODE_POOL, MMIO, non-RAM):

- Leave bitmap bits cleared.
- Set `flags.reserved = 1` on their FrameMeta entries.
- Set `flags.direct_mapped = 1` on FrameMeta entries for PPNs that correspond to kernel-text/rodata/data (ensures they are never reclaimed even if some code path mistakenly tries to return them to the allocator).

After phase 5, `reserve_frame()` can succeed.

### 4.6 Phase 6: Bring up the slab allocator
<!-- txdoc:PAGE-SUBSTRATE-BRING-UP-SEQUENCE-PHASE-6-BRING-UP-THE-SLAB-ALLOCATOR-1 -->

The slab is implemented as a set of per-size-class pools; each pool draws pages from the frame allocator on demand.

```rust
pub struct SlabHeap<P: SlabPageProvider> { /* size classes + page provider */ }
pub unsafe trait SlabPageProvider { /* reserve_run, release_run, direct map */ }
pub struct KernelGlobalAllocator;
```

Small size classes: 8, 16, 32, 64, 128, 256, 512, 1024, and 2048 bytes. Page-sized and larger allocations go directly to the frame allocator as contiguous page runs rounded up to page granularity. This keeps the v1 implementation simple while still making 4 KiB allocations page-backed.

On first allocation of size class `N`:
- The slab pool calls `reserve_run(1, 1, ZeroPolicy::UninitFullOverwrite)?.commit()` to get a fresh page it will subdivide.
- The page stores a small page header at the front and carves the remaining space into `N`-byte objects.
- The objects are pushed onto the pool's free list.

On free:
- The object is pushed back onto its slab's free list.
- When an entire page's objects are all free, the page is removed from the class free list and returned to the frame allocator. v1 has no per-CPU slab cache.

**Register as global allocator.** After bring-up:

```rust
#[global_allocator]
static ALLOCATOR: SlabAllocator = SlabAllocator;
```

The `KernelGlobalAllocator` type delegates `GlobalAlloc::alloc` / `dealloc` to the global `SlabHeap<GlobalPageProvider>`. `tx_substrate::init::<P>()` initializes that heap after the frame allocator and typed PT-node source are installed, then runs a no-alloc smoke allocation. Once installed, `Box::new()`, `Vec::new()`, etc., work throughout the kernel.

**Before this point, no Rust-level allocation is legal.** The substrate initialization itself is written in no-alloc style.

### 4.7 Phase 7: Transition pmap's intermediate-page source
<!-- txdoc:PAGE-SUBSTRATE-BRING-UP-SEQUENCE-PHASE-7-TRANSITION-PMAPS-INTERMEDIATE-PAGE-SOURCE-1 -->

After the frame allocator is installed, pmap's intermediate-page-table-page
source switches from `PT_NODE_POOL` to typed page allocation. The transition no
longer waits for the slab: reserving a page-table node is no-alloc and consumes
one `OwnedFrame` with `into_page_table_frame()`, which marks the returned
page's FrameMeta with `flags.reserved = 1` and `flags.direct_mapped = 1`
(intermediate page-table pages are direct-mapped for pmap walk efficiency, and
reserved against normal-pool return).

`tx_substrate::init::<P>()` installs `page_allocator::reserve_page_table_node`
into `PmapIf::install_pt_node_allocator()` after the bitmap allocator is live.
`PT_NODE_POOL` is retained as a fallback reserve. If typed page allocation
returns `AllocError::Exhausted` during a critical pmap operation (e.g., mapping
in more MMIO for a newly-probed device), pmap can draw from the pool. But in
steady state, all new intermediate pages come from the frame allocator.

---

## 5. Frame allocator API
<!-- txdoc:PAGE-SUBSTRATE-FRAME-ALLOCATOR-API-1 -->

The public surface, callable after `substrate::init()` returns.

### 5.1 Backend trait and installed substrate functions
<!-- txdoc:PAGE-SUBSTRATE-FRAME-ALLOCATOR-API-SINGLE-PAGE-ALLOCATION-1 -->

```rust
pub trait PageAllocator: Sized {
    fn reserve_frame(&self, policy: ZeroPolicy)
        -> Result<FrameReservation<'_, Self>, AllocError>;

    fn reserve_run(&self, count: usize, align: usize, policy: ZeroPolicy)
        -> Result<FrameRunReservation<'_, Self>, AllocError>;

    fn free_count(&self) -> usize;
    fn total_count(&self) -> usize;
    fn backend_diagnostics(&self) -> AllocatorDiagnostics;
}

pub fn install_bitmap_allocator(
    allocator: &'static BitmapPageAllocator<'static>
) -> Result<(), AllocError>;

pub fn reserve_frame(policy: ZeroPolicy)
    -> Result<FrameReservation<'static, BitmapPageAllocator<'static>>, AllocError>;

pub fn reserve_run(count: usize, align: usize, policy: ZeroPolicy)
    -> Result<FrameRunReservation<'static, BitmapPageAllocator<'static>>, AllocError>;
```

The trait is not used through `dyn PageAllocator`. Generic test/backend code may
take `A: PageAllocator`; normal substrate callers use the installed substrate
functions, which delegate to the boot-installed concrete backend. v1 installs a
`BitmapPageAllocator`; v2 may replace the internals with CPU magazines without
changing the token surface.

`BitmapPageAllocator` carries `base_ppn` in addition to `total_count`. Metadata
and bitmap arrays are dense; public reservations and diagnostics report raw
PPNs. Contiguous-run alignment is also interpreted in raw PPN units, so a
request for `align = 512` still means a 2 MiB physical boundary even when
`ppn_base` is not zero.

`reserve_frame()` scans the bitmap for a set bit, clears it atomically via CAS,
decrements the free count, applies the requested `ZeroPolicy`, and returns a
linear `FrameReservation`. On scan failure it returns `AllocError::Exhausted`.
For `ZeroPolicy::Zeroed`, the backend calls the installed direct-map scrubber
before returning the reservation; if no scrubber is installed, it rolls the
frame back and returns `AllocError::ZeroScrubUnavailable`.

`ZeroPolicy::UninitFullOverwrite` is allowed only when the caller will overwrite
the whole frame before any user-visible exposure.

**Scan strategy.** Linear scan with a hint: `NEXT_ALLOC_HINT: AtomicUsize` remembers the last word-index where a free bit was found. `reserve_frame()` starts scanning from the hint, wraps around on miss, updates the hint on hit. Simple; avoids pathological O(max_ppn) scans under steady-state fragmentation.

**Contention.** Multiple CPUs allocating simultaneously race on the bitmap words. Each CAS fails under contention; the caller retries the scan. On 2-8 core systems this is adequate. If profiling shows the bitmap word CAS becoming a bottleneck, per-CPU caches can be added later without changing the public API (the hint mechanism generalizes to per-CPU hints trivially).

**Backend policy.** v1 is a global atomic bitmap with a scan hint. A future
BSD-keg-like backend may add per-CPU magazines that hold `state == 0` frames
owned by the allocator but not globally allocatable. Refills move batches from
the global bitmap to a CPU magazine without changing total free count;
allocation from a magazine commits ownership and decrements free count; frees
return to the current CPU cache and drain globally when full. CPU offline drains
its magazine. Contiguous runs bypass CPU magazines and use the global backend,
optionally draining caches and retrying on failure.

### 5.2 Reservation-phase API
<!-- txdoc:PAGE-SUBSTRATE-FRAME-ALLOCATOR-API-RESERVATION-PHASE-API-1 -->

For STEP-4 compliance, operations must be able to reserve a frame during the `reserve` sub-phase (fallible) and commit it during the `commit` sub-phase (infallible). Drop-on-failure of the reservation returns the frame to the allocator.

```rust
#[must_use]
pub struct FrameReservation<'a, A: PageAllocator> { /* allocator claim */ }

pub struct OwnedFrame<'a, A: PageAllocator> { /* refcount owner */ }

impl<'a, A: PageAllocator> FrameReservation<'a, A> {
    pub fn ppn(&self) -> Ppn;
    pub fn commit(self) -> OwnedFrame<'a, A>;
}
```

Semantics:

- `FrameReservation` is an allocator claim. The bitmap bit is cleared, `FrameMeta.state` is still zero, and the PPN is not publishable.
- `FrameReservation::commit()` consumes the reservation and returns `OwnedFrame`, setting `FrameMeta.refcount = 1`.
- Dropping an uncommitted reservation rolls the frame back into the allocator.
- Dropping `OwnedFrame` decrements refcount; the frame returns to the allocator only if the whole packed state reaches zero.
- Role handoff is acquire-role-counter first, publish binding second, drop `OwnedFrame` last. Role tokens include `MapPin`, `CachePin`, and `DmaPin`.
- Transfer handoff is acquire-retained-frame evidence first, freeze the old
  writable publication second, publish the transfer descriptor third, and drop
  the source owner last. `GiftPin` is the transfer token for this path. It
  retains through `refcount` in v1 so pipe descriptors and `tee` duplicates do
  not consume the narrow DMA-oriented `pin_count`.

This matches the `substrate::{zone, index, credit}::reserve / commit` pattern from `SUBSYSTEM_ANATOMY §4`.

### 5.3 Multi-frame contiguous allocation
<!-- txdoc:PAGE-SUBSTRATE-FRAME-ALLOCATOR-API-MULTI-FRAME-CONTIGUOUS-ALLOCATION-1 -->

For DMA and intermediate-page-table batches:

```rust
pub struct FrameRunReservation<'a, A: PageAllocator> { /* allocator claim */ }
pub struct OwnedFrameRun<'a, A: PageAllocator> { /* refcount owners */ }

impl<'a, A: PageAllocator> FrameRunReservation<'a, A> {
    pub fn base(&self) -> Ppn;
    pub fn count(&self) -> usize;
    pub fn commit(self) -> OwnedFrameRun<'a, A>;
}

impl<'a, A: PageAllocator> OwnedFrameRun<'a, A> {
    pub fn base(&self) -> Ppn;
    pub fn count(&self) -> usize;
    pub fn split(self) -> Vec<OwnedFrame<'a, A>>;
}
```

Finds `count` consecutive set bits in the bitmap at an `align`-aligned PPN boundary. Atomic via a two-phase approach: scan finds a candidate range, attempts to clear all bits in that range via individual CASes; on partial failure (another CPU grabbed one), returns the ones it got and retries. Bounded by the number of retries before falling back to `None`.

Contiguous allocation is rare (DMA buffers at driver init; pmap batches). Slow-path performance is acceptable.

### 5.4 Special frame classes
<!-- txdoc:PAGE-SUBSTRATE-FRAME-ALLOCATOR-API-ZERO-THE-PAGE-VS-TRUST-THE-CALLER-1 -->

```rust
pub struct PermanentFrame<'a, A: PageAllocator> { /* never-free anchor */ }
pub struct PtFrame<'a, A: PageAllocator> { /* pmap-owned page-table page */ }
pub struct DeviceFrame { /* MMIO/device PPN, not allocator-owned */ }
```

- Reserved RAM frames have `flags.reserved` set and never enter allocator pools unless a typed permanent owner claims them.
- Permanent frames (zero frame, kernel metadata anchors) hold `refcount = 1` and `reserved = true`; dropping the Rust token does not return them to the pool.
- Page-table intermediates are produced by consuming `OwnedFrame` into `PtFrame`, which sets `reserved | direct_mapped`. They release only through the pmap teardown path.
- Device/MMIO frames are represented separately and never carry allocator ownership.

### 5.5 Accessor helpers
<!-- txdoc:PAGE-SUBSTRATE-FRAME-ALLOCATOR-API-ACCESSOR-HELPERS-1 -->

```rust
#[inline]
pub fn ppn_to_vaddr(ppn: PPN) -> VAddr {
    VAddr(DIRECT_MAP_BASE + (ppn.0 as usize) * PAGE_SIZE)
}

#[inline]
pub fn vaddr_to_ppn(vaddr: VAddr) -> Option<PPN> {
    if vaddr.0 >= DIRECT_MAP_BASE && vaddr.0 < DIRECT_MAP_BASE + total_ram_bytes() {
        Some(PPN(((vaddr.0 - DIRECT_MAP_BASE) / PAGE_SIZE) as u32))
    } else {
        None
    }
}

#[inline]
pub fn frame_meta(ppn: PPN) -> &'static FrameMeta {
    unsafe { &*FRAME_META_BASE.add(ppn.0 as usize) }
}
```

These are used pervasively by pmap and PageContainer code. Inlined; single direct-map-base-relative address computation.

### 5.6 Statistics
<!-- txdoc:PAGE-SUBSTRATE-FRAME-ALLOCATOR-API-STATISTICS-1 -->

```rust
pub fn free_count() -> Result<usize, AllocError>;
pub fn total_count() -> Result<usize, AllocError>;
pub fn backend_diagnostics() -> Result<AllocatorDiagnostics, AllocError>;
```

For `/proc/meminfo` and diagnostics.

---

## 6. FrameMeta CAS discipline
<!-- txdoc:PAGE-SUBSTRATE-FRAMEMETA-CAS-DISCIPLINE-1 -->

Operations on `FrameMeta.state` are packed-counter CASes. Each counter has exactly one class of writer:

| Counter | Incremented by | Decremented by |
|---|---|---|
| refcount | OwnedFrame / permanent / retained-owner / GiftPin acquisition | OwnedFrame drop, GiftPin drop, or explicit owner teardown |
| map_count | PTE install | PTE teardown |
| cache_ref | PageContainer page-index insert | PageContainer page-index remove |
| pin_count | DmaPin acquire | DmaPin release |

`GiftPin` is typed transfer evidence, not a new semantic page entity and not a
DMA pin. Its acquire path is a live-frame CAS on `refcount`: if the packed state
word is zero, acquire fails with the same sentinel semantics as map/cache/DMA
upgrades. This gives VM a linear token it can place in `UserPageGift` while
leaving `pin_count` reserved for hardware-facing long-term DMA pressure.

Each increment is a bounded CAS loop:

```rust
fn increment_map_count(meta: &FrameMeta) -> Result<(), ErrTooManyRefs> {
    loop {
        let old = meta.state.load(Ordering::Acquire);
        let old_map = (old >> 10) & 0x3FF;
        if old_map == 0x3FF {
            return Err(ErrTooManyRefs);  // 10-bit saturation
        }
        let new = old + (1 << 10);
        match meta.state.compare_exchange_weak(
            old, new, Ordering::AcqRel, Ordering::Acquire
        ) {
            Ok(_) => return Ok(()),
            Err(_) => continue,  // retry
        }
    }
}
```

Decrement is symmetric, asserting underflow never occurs (debug-only; a decrement from zero is a kernel bug).

**Reaching zero.** When a decrement brings a counter from 1 to 0, the caller checks if the *entire* state word is now zero. If so, the frame has hit semantic death: no retention, no maps, no cache inclusions, no DMA pins. The caller is responsible for:

1. Asserting flags.reserved is clear (reserved frames never reach the free path).
2. Calling the allocator return path to set the bitmap bit.

```rust
// Sketch: PTE teardown
fn pte_teardown(pte: Pte, ppn: PPN) {
    pmap.clear_pte(pte);
    shootdown_batch.push_invalidation(pte_vaddr);
    // ... later, after shootdown completes ...
    let meta = frame_meta(ppn);
    decrement_map_count(meta);
    if meta.state.load(Ordering::Acquire) == 0 && !meta.is_reserved() {
        return_to_free_pool(ppn);
    }
}
```

**Race between concurrent decrementers.** Two threads may each bring a counter to zero on different counters at nearly the same time. The "state is zero after my decrement" check races: only one thread sees state == 0, and that thread returns the frame to the allocator. The other thread's decrement completes before it reads state; its state read sees a nonzero value (the other thread's decrement is still in flight) or sees zero and races on the return path. Double-free is prevented by the bitmap CAS: the bit is already set, the second return observes the failure and moves on.

Formally: returning a frame to the allocator is idempotent against concurrent calls, because the bitmap bit is a linearization point.

**Pinning interactions with reclamation.** Reclamation must never race with legitimate access. A PageContainer scanning its page index for a page, finding a Frame, and trying to acquire a CachePin is racing with another thread decrementing the final cache_ref. The CAS discipline for acquire:

```rust
fn acquire_cache_ref(meta: &FrameMeta) -> Result<CachePin, ErrGone> {
    loop {
        let old = meta.state.load(Ordering::Acquire);
        if old == 0 {
            return Err(ErrGone);  // SENTINEL-like check
        }
        let old_cache = (old >> 20) & 0xFF;
        if old_cache == 0xFF {
            return Err(ErrTooManyRefs);
        }
        let new = old + (1 << 20);
        match meta.state.compare_exchange_weak(
            old, new, Ordering::AcqRel, Ordering::Acquire
        ) {
            Ok(_) => return Ok(CachePin { ppn: compute_ppn(meta) }),
            Err(_) => continue,
        }
    }
}
```

The `old == 0` check is the "is this frame semantically dead?" gate, analogous to SENTINEL_DEAD on other entities. If it's zero, the page is mid-reclamation or already reclaimed; acquire fails; the PageContainer's observer re-checks its page index under a fresh epoch guard.

This matches the object_model's SENTINEL-guarded upgrade discipline (object_model_v2 §5). The "sentinel" for Frame is "state == 0"; reclamation is atomic via the allocator bitmap CAS.

---

## 7. Pmap integration
<!-- txdoc:PAGE-SUBSTRATE-PMAP-INTEGRATION-1 -->

Pmap operations come in three flavors, all HAL-defined (§7.6, §7.7, §7.9):

- `PmapReservation::reserve(vaddr, count)` — reserve PTE slots.
- `PmapCommitBatch::commit(reservation, pte_values)` — install PTEs.
- `ShootdownBatch::push_invalidation(vaddr_range)` + `issue_and_wait()` — invalidate TLBs.

This substrate adds one piece: **what FrameMeta operations accompany each pmap event.**

### 7.1 PTE install
<!-- txdoc:PAGE-SUBSTRATE-PMAP-INTEGRATION-PTE-INSTALL-1 -->

Pmap installs a PTE pointing at PPN `p`. Before the commit, the caller must have already incremented `p`'s map_count:

```rust
let meta = frame_meta(ppn);
increment_map_count(meta)?;   // fallible via overflow

let reservation = pmap.reserve(vaddr, 1)?;
let batch = PmapCommitBatch::new();
batch.install_pte(reservation, pte_value_for(ppn, perms));
batch.commit();
// commit is infallible per HAL contract
```

If the map_count increment fails (overflow), the caller does not proceed to pmap. The reservation (if taken) drops cleanly via its Drop impl.

If the pmap reservation fails (e.g., intermediate-page allocation failure), the map_count is decremented; the CAS is guaranteed to succeed because we never released it.

### 7.2 PTE teardown
<!-- txdoc:PAGE-SUBSTRATE-PMAP-INTEGRATION-PTE-TEARDOWN-1 -->

```rust
let result = P::unmap_kernel_mapping(vaddr, PmapReserveKind::Page4K)?;
let mut batch = KernelShootdownBatch::<_, 8>::new();
batch.push_page_unmap_result(result, map_pin)?;
batch.issue_and_release::<P>();
```

**Critical ordering.** map_count must *not* be decremented before shootdown completes. Otherwise, a stale TLB entry on another core could point at a freed-and-reallocated-for-other-use frame. The shootdown batch tracks pending decrements and performs them after `issue_and_wait()` returns.

```rust
pub struct KernelShootdownBatch<'a, A: PageAllocator, const N: usize> {
    pending: [MaybeUninit<PendingMapRelease<'a, A>>; N],
    len: usize,
}

struct PendingMapRelease<'a, A: PageAllocator> {
    result: PmapUnmapResult,
    map_pin: MapPin<'a, A>,
}

impl<'a, A: PageAllocator, const N: usize> KernelShootdownBatch<'a, A, N> {
    pub fn push_page_unmap_result(
        &mut self,
        result: PmapUnmapResult,
        map_pin: MapPin<'a, A>,
    ) -> Result<(), ShootdownPushError<'a, A>> {
        // v1 accepts page-sized mappings only and checks result.phys
        // matches map_pin.ppn().
    }

    pub fn issue_and_release<P: PmapIf>(self) {
        for entry in self.pending {
            P::shootdown_kernel_mapping(entry.result.invalidation());
            drop(entry.map_pin); // releases map_count after shootdown
        }
    }
}
```

Dropping an unissued `KernelShootdownBatch` is a debug assertion and intentionally does not drop the pending `MapPin`s. Leaking the pins keeps the frames live, which is safer than releasing map counts before invalidation. The full process-root batch will generalize this page-sized kernel form to ASID-scoped ranges and multi-page results.

### 7.3 PTE protect
<!-- txdoc:PAGE-SUBSTRATE-PMAP-INTEGRATION-PTE-PROTECT-1 -->

Safe permission changes update an existing leaf in place and return an
invalidation. This is valid when the mapping exists at the requested granularity
and the architecture accepts the requested permissions. Absent mappings return
`None`; unsafe cases such as "caller asked for 4 KiB protect but the pmap holds
a 2 MiB leaf" return `InvalidRequest` so VM can leave the binding authoritative
and let fault/rematerialization handle the split path.

The current executable RV64 subset exposes this for kernel mappings through
`PmapIf::protect_kernel_mapping()`. The process-root version will use the same
ordering discipline as teardown: update the leaf, issue ASID/global shootdown,
and only then let higher-level VM publication observe the materialization as
fully changed.

`tx_hal::pmap` exposes no-alloc page-range wrappers over the HAL single-page
root operations. `PmapRangeReservation<P, N>` stores up to `N` reserved 4 KiB
leaves and rolls back the reserved prefix on drop unless `commit()` is called.
Range unmap and protect helpers collect per-page `PmapUnmapResult` /
`PmapInvalidation` values into caller-provided slices, so VM can combine these
with range locks, recipes, and the substrate ASID-scoped shootdown batch without
making the board own VM policy.

### 7.4 Kernel virtually contiguous allocation
<!-- txdoc:PAGE-SUBSTRATE-KERNEL-VMALLOC-1 -->

The global allocator follows a `kvmalloc`-style policy for large Rust objects.
Moderate requests first try the direct-map contiguous allocator and fall back
to vmalloc when fragmentation prevents a run. Requests above the allocator's
bounded direct-try limit go directly to vmalloc because the current bitmap
backend has no buddy allocator's constant-time order availability test.

The vmalloc window supplies contiguous virtual addresses backed by independent
4 KiB frames. Population reserves empty leaves and uses
`PmapIf::commit_new_kernel_mapping()`, so it does not flush the TLB once per new
page; one range publication follows the completed PTE batch. Teardown first
clears every leaf, chains the still-owned frames through their now-dead object
storage without allocating metadata, issues one batched kernel shootdown, and
only then returns the frames. This preserves the required `unmap -> shootdown
-> physical reuse` ordering while making cost linear in PTE updates rather than
linear in global TLB flushes.

### 7.4 Pmap's own intermediate pages
<!-- txdoc:PAGE-SUBSTRATE-PMAP-INTEGRATION-PMAPS-OWN-INTERMEDIATE-PAGES-1 -->

Intermediate page-table pages (L1, L2 tables in Sv39 / LA equivalent) are themselves physical frames, allocated from the frame allocator after Phase 7 of bring-up. When the pmap's tree restructures, intermediate pages may be freed too.

These pages have `flags.reserved = 1` (set by `OwnedFrame::into_page_table_frame()`) so ordinary owned-frame drops do not return them to the pool silently. Explicit release goes through pmap teardown, which clears the reserved/direct-map page-table-frame flags and releases the owned refcount.

The RV64 QEMU executable subset keeps a board-private sidecar registry for
committed intermediate ownership. A branch PTE preserves only the physical
address of the child table, while the release authority is the `PtNode` token
that knows whether the table came from the static `PT_NODE_POOL` or a typed
frame-allocator-backed page-table frame. On commit, newly allocated
intermediates are registered; on unmap, empty L0/L1 tables are pruned and their
registered `PtNode` is released through the same pmap-only path used by
rollback. Process-root pmap materialization may replace this fixed registry
with a root-owned sidecar, but the invariant is stable: branch-PTE teardown must
recover typed release authority before any page-table frame can become free.

---

## 8. No-swap discipline
<!-- txdoc:PAGE-SUBSTRATE-NO-SWAP-DISCIPLINE-1 -->

Anonymous pages are never written to backing storage. This affects two aspects of the substrate:

### 8.1 Reclaim-on-failure
<!-- txdoc:PAGE-SUBSTRATE-NO-SWAP-DISCIPLINE-RECLAIM-ON-FAILURE-1 -->

`reserve_frame()` returns `AllocError::Exhausted` when the allocator has no free frame. The caller (typically high in the stack) decides whether to:

- **Fail the operation.** For user-initiated allocations (mmap, fork's address-space clone, file-cache fill), return `ENOMEM` to userspace.
- **Use the managed slow path.** A waitable allocation enters the
  `AllocationGateway` above the allocator. The gateway asks the global memory
  pressure coordinator for bounded owner-driven reclaim/writeback progress,
  waits when its allocation class permits, and retries.
- **Panic.** For boot-time or kernel-critical allocations (can't make forward progress), panic with a clear message.

The substrate itself does not implement reclaim and does not call upper layers
while holding allocator state. It exposes allocation results and pressure
snapshots. The global coordinator orchestrates policy, but only registered
resource owners can claim and evict their objects.

### 8.2 Anonymous-page pinning
<!-- txdoc:PAGE-SUBSTRATE-NO-SWAP-DISCIPLINE-ANONYMOUS-PAGE-PINNING-1 -->

An anonymous page is held by:
- Its map_count (PTE references), and/or
- Its cache_ref (PageContainer page-index inclusion when the anon page lives in a PC), and/or
- Its refcount (owned-frame or retained-owner holders).

Under no-swap, none of these are "soft" references — the page is pinned as long as any are nonzero. There is no daemon that would unmap an anonymous page to free memory. The only way an anonymous page becomes reclaimable is explicit teardown: munmap, PageContainer drop, process exit, etc.

Consequence: **memory pressure affects only file-backed pages** (which have a clean, reclaimable state via writeback-or-drop) and **page caches that can be truncated**. Anonymous memory is a hard commit. Applications that allocate more anonymous memory than RAM permits will hit `ENOMEM` on mmap/brk/fork and must handle it.

This is a deliberate design choice. It makes kernel memory accounting deterministic and eliminates swap-related complexity. It also means we cannot over-commit memory the way Linux does by default — a 4 GB machine cannot run a process that allocates 6 GB of anonymous memory and touches only 3 GB of it.

---

## 9. Shootdown batching
<!-- txdoc:PAGE-SUBSTRATE-SHOOTDOWN-BATCHING-1 -->

The HAL provides `ShootdownBatch` (§7.9) as a raw primitive. The substrate adds one discipline: **all PTE teardowns within a step's commit sub-phase go through a single batch, which issues at the commit point.**

A step performing multiple pmap changes (e.g., munmap of a large range) constructs one `ShootdownBatch`, pushes all invalidations, then calls `issue_and_wait()` as part of commit. This minimizes cross-core IPI overhead (one IPI per step, not one per PTE).

```rust
// Sketch: step_munmap
fn step_munmap(vaddr: VAddr, len: usize, addr_space: &AddressSpace) -> StepOutcome<()> {
    let mut batch = ShootdownBatch::new();
    for page_vaddr in vaddr..vaddr+len step PAGE_SIZE {
        batch.teardown_pte(&mut addr_space.pmap, page_vaddr);
    }
    // ... (recipes BTree withdrawals also go into the bundle per MODULE_MAP §12.1)
    batch.issue_and_wait();
    Done(())
}
```

The step is synchronous — `issue_and_wait()` spins until all remote harts have acked the invalidation. Under stackless coroutines and per-hart kernel stacks, this is a blocking spin on the current hart; the hart is not doing anything else during the wait (that hart's reactor isn't running other Futures until the step returns).

This is fine at small-to-medium scale. For very large munmaps (gigabytes), the spin time becomes user-visible; we can batch in chunks (`step_munmap` returns `Advanced(chunk_size)` and the script re-invokes). This is consistent with STEP-2 (bounded steps).

---

## 10. Deviations from HAL doc
<!-- txdoc:PAGE-SUBSTRATE-DEVIATIONS-FROM-HAL-DOC-1 -->

Two deliberate overrides of the HAL design document. Both are design decisions that push back against HAL's defaults.

### 10.1 Per-hart stack instead of per-task `KernelStack`
<!-- txdoc:PAGE-SUBSTRATE-DEVIATIONS-FROM-HAL-DOC-PER-HART-STACK-INSTEAD-OF-PER-TASK-KERNELSTACK-1 -->

HAL §6.4 specifies `KernelStack` as a per-thread allocation. Under stackless coroutines with per-hart stacks, this does not apply: there are no per-thread kernel stacks.

The revised model:

- One kernel stack per hart, allocated at hart bring-up (BSP during early boot; APs during `boot_secondary_cpus`). Size: 16 KiB, with a guard page below.
- The per-hart stack services trap handling, coroutine polling, and all kernel work on that hart.
- During the low-to-high pmap transition, the stack move is only an alias
  rewrite for this same per-hart storage: identity stack VA → high/direct-map
  stack VA. It is not a task switch and does not allocate a per-thread stack.
- Tasks (Futures) are polled on the hart's stack; when they yield or complete, the stack is released for the next work.
- `ExecContext` (HAL §6.2) is retained as the per-task container holding the trap frame, signal mask, and other per-task state — but it is *not* a kernel stack; it is a heap-allocated struct referenced by the task's Future.
- `TaskContext` (HAL §6.3) is not used in its classical form (there is no "save SP here, switch to that SP"). If an equivalent concept is needed (e.g., for FP register lazy save), it lives inside `ExecContext`.

This spec does not define `ExecContext` beyond what HAL specifies; the per-hart-stack revision is scoped to "do not allocate per-task kernel stacks, and do not implement context-switch machinery that assumes them."

### 10.2 `BootInfo` uses `'static` references
<!-- txdoc:PAGE-SUBSTRATE-DEVIATIONS-FROM-HAL-DOC-BOOTINFO-USES-STATIC-REFERENCES-1 -->

Earlier HAL drafts specified `BootInfo` with `String` and `Vec<MemoryRegion>`
fields, requiring a heap to be live during `init_after_heap()`. The active HAL
contract now matches this page substrate requirement:

```rust
pub struct BootInfo {
    pub memory_regions: &'static [MemoryRegion],
    pub kernel_image: PhysRange,
    pub initrd: Option<PhysRange>,
    pub cmdline: Option<&'static str>,
}
```

All `'static` references point into `.boot.data` or into the BSS-resident EarlyBootInfo (which is kept alive for the kernel's lifetime). No heap allocation is required to publish `BootInfo`.

Consequence: boot-info promotion is a platform-owned pre-substrate step that
copies or points to static storage only. It runs before the substrate's
`init()`.

**This change is compatible with no KPTI + per-hart stack + stackless coroutines.** It removes the early heap entirely from the boot sequence; the slab is the only kernel heap, and it comes up after the frame allocator.

### 10.3 `PT_NODE_POOL` retained as fallback
<!-- txdoc:PAGE-SUBSTRATE-DEVIATIONS-FROM-HAL-DOC-PT-NODE-POOL-RETAINED-AS-FALLBACK-1 -->

HAL §7.4.2 marks `PT_NODE_POOL` as an "intermediate mechanism." This spec retains it permanently as a fallback reserve (see §4.7). The pool remains statically allocated (a few KB of BSS); it is used after allocator installation only when typed page allocation returns `AllocError::Exhausted` during a pmap operation. This is defensive; in practice the pool may never be drawn from after boot.

---

## 11. Panics and failures
<!-- txdoc:PAGE-SUBSTRATE-PANICS-AND-FAILURES-1 -->

| Condition | Response |
|---|---|
| Memory regions overlap | Panic at phase 1 |
| Kernel image outside any usable or reserved region | Panic at phase 1 |
| Insufficient RAM for FrameMeta + bitmap placement | Panic at phase 4 |
| `reserve_frame` fails at any bring-up phase | Panic (the first calls are for direct-map extension and slab bootstrap; both critical) |
| `pmap.reserve` fails during phase 2 or phase 3 | Panic (MMIO mapping failures at boot are unrecoverable) |
| FrameMeta counter underflow (debug) | Panic; indicates a reference-counting bug |
| Double-free of a PPN | Caught by the bitmap CAS; second free is a no-op in release; panic in debug |
| Reserved frame freed | Panic in debug through the typed owner path; ignored by the normal allocator return path in release |

After substrate init returns, failures are expected to flow through `Result` return values, not panics. The substrate is panic-heavy only during bring-up, where there is no recoverable state.

---

## 12. What this document does not cover
<!-- txdoc:PAGE-SUBSTRATE-WHAT-THIS-DOCUMENT-DOES-NOT-COVER-1 -->

- **Reclaim policy.** Which pages to evict under memory pressure, when, by what heuristic. This is a VM-subsystem concern; see forthcoming `PAGE_BACKED_v1.md` and `RECLAIM.md`.
- **NUMA.** We do not support NUMA in Phase 1. If we add it, the frame allocator grows per-node bitmaps; the FrameMeta array stays flat (one entry per PPN, with a node_id flag).
- **Hot-add / hot-remove.** Not supported. Memory layout is fixed at boot.
- **Huge pages.** The frame allocator handles 4 KiB frames only. Huge-page support (2 MB, 1 GB) is a pmap-level concern: install a 2 MB superpage PTE pointing at an aligned 4 KiB-granularity `reserve_run()` allocation of 512 contiguous frames.
- **Memory poisoning / ECC errors.** HAL does not currently report these; out of scope.
- **`PageContainer`.** All of it. Lives in `PAGE_BACKED_v1.md`.
- **`AddressSpace` and user pmaps.** The substrate sets up the kernel's high-half page-table content. User AddressSpace creation — which shares the kernel high-half and adds a fresh user-side tree — is a VM-subsystem concern.
- **Reactor, coroutines, ready queue.** Lives in the forthcoming `REACTOR.md`.

---

## 13. Summary
<!-- txdoc:PAGE-SUBSTRATE-SUMMARY-1 -->

The page substrate is small in surface area but touches every memory-using component.

- **Frame allocator:** bitmap-backed `PageAllocator`, typed reservations, `OwnedFrame` ownership, role pins, and special frame classes for permanent, device, and page-table frames.
- **`FrameMeta`:** 8 bytes per physical page, packed CAS-discipline counters for refcount / map_count / cache_ref / pin_count. Flat array, direct-mapped.
- **Pmap stages:** bootstrap (asm) → direct-map extension (1 GiB leaves, no intermediates) → boot MMIO mapping (2 MiB / 4 KiB leaves, PT_NODE_POOL intermediates) → frame-allocator-backed intermediates with PT_NODE_POOL fallback.
- **Kernel heap:** slab with power-of-two small classes from 8 B to 2 KiB; page-sized and larger allocations use frame-allocator page runs.
- **Shootdown:** HAL-provided batching primitive; substrate uses one batch per step-commit.
- **No swap:** anonymous pages are hard commitments until explicit teardown;
  managed allocation may reclaim registered caches but cannot evict anonymous
  content.
- **No KPTI:** kernel mappings are always present in all page tables.

The substrate's `init()` runs during CoreInit and establishes all of the above before any SMP bring-up, any reactor, or any subsystem. After init, typed page allocation, `FrameMeta` manipulation, the pmap typed-intermediate source, and the slab/global heap are available throughout the kernel.

Everything on top of this — `PageContainer`, `RNodeBacking`, user AddressSpace management, reclaim, file-cache writeback — consumes this API and adds its own discipline. This document is the contract.

---

## References
<!-- txdoc:PAGE-SUBSTRATE-REFERENCES-1 -->

- [`HAL_v1.md`](HAL_v1.md) — boot sequence, trap, pmap primitives, TLB shootdown.
- [`MODULE_MAP_v1.md`](../00_meta-framework/MODULE_MAP_v1.md) — foundation/HAL layout.
- [`object_model_v2.md`](../00_meta-framework/object_model_v2.md) §3.3 (compound payload predicates), §5 (reference hierarchy), §6 (reclamation), §7.5 (operational contributions).
- [`02_INVARIANTS_v5.md`](../../Txv3/02_INVARIANTS_v5.md) — STEP-4, OBL-*, ARCH-*.
- [`SUBSYSTEM_ANATOMY_v2_1.md`](../00_meta-framework/SUBSYSTEM_ANATOMY_v2_1.md) §4 (substrate primitives: zone, index, credit, mutation; this document adds the frame allocator as a sibling).
- [`MEMORY_IO_ARCHITECTURE_v1.md`](../03_memory-vm/MEMORY_IO_ARCHITECTURE_v1.md) — dual-plane ownership, allocation gateway, global pressure coordination, and `FrameMeta` authority boundary.
