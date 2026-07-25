# Chapter 7 — Page tables behind a proof-object API

In Chapter 4 the trampoline built page tables by hand, in assembly, with raw
stores. That was a one-time bootstrap. Steady-state page mapping — extending the
direct map, mapping MMIO, building per-process address spaces, tearing them down —
needs a real API. `PmapIf` is that API, and it is one of the two load-bearing HAL
traits (the other is `TrapIf`, Chapter 9).

## The traditional picture, and what's wrong with it

The classic page-mapping interface is `map(va, pa, flags)` and `unmap(va)`. It
has two latent hazards:

1. **Allocation can fail in the middle.** Walking to the leaf may need to allocate
   intermediate page-table pages. If the third allocation fails after the first
   two succeeded, you're half-mapped and must unwind by hand. Most kernels handle
   this with goto-cleanup ladders that are easy to get wrong.
2. **Unmap and TLB invalidation are coupled, and the ordering is subtle.** On SMP,
   clearing a PTE is fast, but another core may still have the old translation
   cached. Free the frame before every core has flushed and you have a
   use-after-free that a stale TLB entry can still reach.

txKernel's `PmapIf` answers both with a **prepare / commit / shootdown**
discipline carried by linear proof objects. The doc states it at
`txdoc:HAL-PMAPIF-…-1`: "the kernel reserves slots fallibly, commits values
infallibly, and tears down with mandatory shootdown coordination."

> **Divergence — concrete types, not associated types.** This is the single
> biggest doc/code gap, so read it carefully. The doc presents *two* `PmapIf`
> surfaces: a "current executable" narrow one, and a "full planned" one with
> associated types — `type Pte; type PmapRoot; type Asid;` plus a
> `PmapCommitBatch<P>` aggregator. **The associated-type surface was never
> built.** The shipped `PmapIf` (`crates/tx-hal/src/lib.rs:670`) uses *concrete
> shared types* defined in `tx-hal`: `PmapRoot`, `Asid(u16)`, `PmapReservation`,
> `PmapInvalidation`, `PmapUnmapResult`. There is no `PmapCommitBatch`; commits
> are single-mapping. And `commit_*` takes an extra `PmapPermissions` argument the
> doc's signatures don't show. Everywhere this chapter says "the API," it means the
> shipped concrete-type API. See [Appendix A](appendix-a-design-vs-code-ledger.md).

Why concrete over associated? Associated types would propagate a `P` parameter
into every type that names a reservation or a root — `PmapReservation<P>`,
`PmapRoot` as `P::PmapRoot` — pushing `P` through substrate and VM signatures
everywhere. Concrete shared types keep the proof objects monomorphization-free to
*name* while the board still owns the actual PTE encoding behind the trait
methods. It's a pragmatic trade the same shape as the `TrapFrameMut` vtable
(Chapter 9).

## Sv39 PTE encoding — the board's secret

What `PmapIf` hides is the architecture's PTE format. On this board that's Sv39,
encoded in `pmap/pte.rs`. The flag bits (`pte.rs:37`):

```rust
pub(crate) const PTE_V: u64 = 1 << 0;   // Valid
pub(crate) const PTE_R: u64 = 1 << 1;   // Read
pub(crate) const PTE_W: u64 = 1 << 2;   // Write
pub(crate) const PTE_X: u64 = 1 << 3;   // eXecute
pub(crate) const PTE_U: u64 = 1 << 4;   // User-accessible
pub(crate) const PTE_G: u64 = 1 << 5;   // Global (not flushed by ASID-scoped sfence)
pub(crate) const PTE_A: u64 = 1 << 6;   // Accessed
pub(crate) const PTE_D: u64 = 1 << 7;   // Dirty
```

A leaf PTE packs the physical page number above bit 10 and always sets V/A/D
(`pte.rs:118`):

```rust
pub(crate) fn encode_leaf_pte(phys: PhysAddr, flags: u64) -> u64 {
    ((phys.0 as u64 >> 12) << 10) | flags | PTE_V | PTE_A | PTE_D
}
```

(Setting A and D up front sidesteps hardware A/D-bit faults; the kernel never
relies on them being hardware-managed.) A *branch* PTE — one that points at the
next-level table rather than mapping a page — sets only V (`pte.rs:122`):

```rust
pub(crate) fn encode_branch_pte(phys: PhysAddr) -> u64 {
    ((phys.0 as u64 >> 12) << 10) | PTE_V
}
```

and the leaf/branch distinction is recovered by inspecting the R/W/X bits
(`pte.rs:129`): a valid PTE with no R/W/X is a branch; with any R/W/X it's a leaf.
This is exactly the rule the trampoline relied on in Chapter 4.

### From HAL permissions to PTE flags

The HAL's permission vocabulary is `PmapPermissions` (`crates/tx-hal/src/lib.rs:454`),
a bitset with `READ`/`WRITE`/`EXECUTE`/`USER`/`GLOBAL`/`DEVICE` and the convenience
combos `KERNEL_RO`/`KERNEL_RW`/`KERNEL_RX`. The board maps it to Sv39 bits in
`encode_leaf_pte_with_permissions` (`pte.rs:95`) — a straight bit-for-bit
translation. Before any encode, `validate_rv64_leaf_permissions` (`pte.rs:50`)
rejects combinations Sv39 cannot express safely:

```rust
if (user && !allow_user) || (!readable && !executable) || (writable && !readable) {
    return Err(PmapError::InvalidRequest);
}
```

No write-without-read, no user bit on a kernel mapping, no neither-readable-nor-
executable leaf. These are the architecture's rules, surfaced as a typed error
instead of a silently-wrong PTE.

### The one place a PTE becomes a pointer

Recall Chapter 3's rule: an address is a value until a *named* conversion makes it
dereferenceable. Page tables live in physical memory; to edit one you need a
pointer. There is exactly one helper that crosses that line (`pte.rs:144`):

```rust
pub(crate) unsafe fn page_table_mut_from_phys(phys: PhysAddr) -> &'static mut PageTable {
    #[cfg(target_arch = "riscv64")]
    { unsafe { &mut *(direct_map_virt(phys.0) as *mut PageTable) } }
    // host build: identity
}
```

It forms the pointer through the *direct map* (`direct_map_virt(phys)` =
`DIRECT_MAP_BASE + phys`), which is why the direct map must exist and be stable
(Chapter 8). Every page-table edit in the board goes through this one helper; in
debug builds (`tx_pmap_debug`) it even validates the phys is in-contract and dumps
the caller's `ra` if not. Concentrating the unsafe conversion in one audited spot
is the Chapter 3 discipline applied to the most pointer-heavy code in the kernel.

## The reservation proof object

`PmapReservation` (`crates/tx-hal/src/lib.rs:513`) is the linear proof that a PTE
slot has been prepared:

```rust
#[derive(Debug, Eq, PartialEq)]
#[must_use]
pub struct PmapReservation {
    virt: VirtAddr,
    phys: PhysAddr,
    kind: PmapReserveKind,                  // Page4K | Superpage2M | Superpage1G
    intermediates: PmapReservationIntermediates,   // { l2, l1, l0: Option<PtNode> }
}
```

`#[must_use]` means a reservation you forget to commit or roll back produces a
compiler warning. The `intermediates` field holds the page-table nodes that
`reserve` allocated on the way down to the leaf — so the *reservation itself*
carries the cleanup obligation.

The lifecycle is three calls (shown here for the kernel half; the process-root
versions in Chapter 8 are identical in shape):

```rust
fn reserve_kernel_mapping(virt, phys, kind) -> Result<Option<PmapReservation>, PmapError>;
fn rollback_kernel_mapping(reservation: PmapReservation);
fn commit_kernel_mapping(reservation: PmapReservation, permissions: PmapPermissions);
```

- **`reserve`** walks to the leaf, allocating any missing intermediate tables and
  recording them in the reservation. It may fail (out of PT nodes, alignment
  wrong, already mapped) — and on failure *no state has changed*. It returns
  `Ok(None)` if the mapping is already present with the expected encoding
  (idempotent), or `Ok(Some(reservation))` if a fresh slot is ready.
- **`commit`** writes the leaf PTE (with `permissions`) into the prepared slot.
  By the time you hold a reservation, the slot exists and the intermediates are
  allocated, so commit cannot fail — it just stores the PTE and fences.
- **`rollback`** is for the abandon path: it zeroes any speculatively-installed
  branch PTEs and frees the intermediate nodes the reservation was carrying.

> **Divergence — explicit rollback, not Drop-only.** The doc's `PmapReservation`
> has a `Drop` impl that frees the intermediates automatically if you don't
> commit (`txdoc:HAL-PMAPIF-…-THE-RESERVATION-PROOF-OBJECT-1`). The shipped type
> has no such `Drop`; cleanup is the *explicit* `rollback_kernel_mapping` /
> `rollback_mapping` call. `#[must_use]` nudges you to handle it, but the unwind
> is a method call, not a destructor. (The doc's "full surface" also had
> `PmapReservation<P>` generic over the platform; the shipped one is not generic.)

## Page-table node allocation: pool then frames

`reserve` needs intermediate page-table pages from somewhere, and that *somewhere*
changes across boot. The doc's migration table (`txdoc:HAL-APPENDIX-A-…`) calls
it `PT_NODE_POOL`. The board (`pmap/pt_node.rs`) implements two sources behind
`alloc_pt_node`:

1. **Boot pool** — a fixed `PT_NODE_POOL_ENTRIES` (8) page region carved by the
   linker (Chapter 4), tracked by an `AtomicUsize` bitmap. This is the only source
   before substrate exists.
2. **Typed frames** — after `tx_substrate::init` installs an allocator via
   `install_pt_node_allocator(allocator)` (a one-shot CAS, `pt_node.rs:140`), new
   nodes come from typed page-table frames, with the boot pool kept as an
   *exhaustion fallback*.

The `PtNode` type (`crates/tx-hal/src/lib.rs:375`) remembers its source so it can
be freed correctly:

```rust
pub struct PtNode {
    pub phys: PhysAddr,
    source: PtNodeSource,   // BootPool | TypedFrame(PtNodeReleaser)
}
```

`release_typed_frame` returns the frame to substrate (typed source) or signals the
board to flip its boot-pool bitmap bit (boot source). This is the same two-phase
"static during bringup, typed after substrate" pattern the doc describes for the
direct map and trap vector.

## `PmapReserveKind` — granularity is explicit

Every reserve/unmap/protect names a granularity (`crates/tx-hal/src/lib.rs:437`):

```rust
pub enum PmapReserveKind { Superpage1G, Superpage2M, Page4K }

impl PmapReserveKind {
    pub const fn size(self) -> usize { /* 1 GiB | 2 MiB | 4 KiB */ }
}
```

mapping straight onto Sv39's three leaf levels (root→1 GiB, L1→2 MiB, L0→4 KiB).
The VPN index helpers in `topology.rs` (`rv64_1g_leaf_index` /
`rv64_2m_leaf_index` / `rv64_4k_leaf_index`, all `(virt >> shift) & 0x1ff`) let
the mapping code talk in levels rather than shifts.

## What's hidden, and what's promised

The doc's summary (`txdoc:HAL-PMAPIF-…-WHATS-HIDDEN-BEHIND-THE-TRAIT-1`) is a good
closing checklist. The *board* knows: PTE format (Sv39 vs Sv48 vs LA64), how
permissions encode, how to walk the tables, how ASIDs encode, how to issue
`sfence.vma`. The *consumer* (substrate, VM) knows only: reservations are linear
and `#[must_use]`; commits are infallible once you hold the reservation; unmap and
shootdown are coupled; the direct map exists and is stable. Nothing above the trait
references a single Sv39 bit.

## What you should take away

- `PmapIf` is a prepare/commit/shootdown API over linear `#[must_use]` proof
  objects, hiding the Sv39 PTE format entirely.
- Reservations carry the intermediate page-table nodes they allocated; commit is
  infallible, rollback is an explicit call (not a `Drop`).
- Sv39 leaf vs branch is the R/W/X-bits distinction; `page_table_mut_from_phys` is
  the single audited place a `PhysAddr` becomes an editable pointer, via the direct
  map.
- PT nodes come from a boot pool before substrate, typed frames after — selected
  by a one-shot `install_pt_node_allocator`.

Next: [Chapter 8 — Kernel half vs process roots](ch08-kernel-half-vs-process-roots.md),
where we see how the same API serves both the global kernel page table and
per-process address spaces, plus ASID management and the satp-write fast path.
</content>
