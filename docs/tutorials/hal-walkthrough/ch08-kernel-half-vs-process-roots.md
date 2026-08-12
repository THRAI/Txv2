# Chapter 8 — Kernel half vs process roots

The `PmapIf` API from Chapter 7 serves two very different clients with the same
proof-object discipline: the **kernel half** (the one global page table — direct
map, kernel image, MMIO) and **per-process roots** (one Sv39 root per address
space). This chapter walks both, then the two cross-cutting concerns that make
process roots work: ASID management and the `satp` write on context switch.

## Two families of methods on one trait

`PmapIf` has parallel method families. The kernel-half family operates on the
implicit kernel root:

```rust
fn reserve_kernel_mapping(virt, phys, kind) -> Result<Option<PmapReservation>, PmapError>;
fn commit_kernel_mapping(reservation, permissions: PmapPermissions);
fn rollback_kernel_mapping(reservation);
fn unmap_kernel_mapping(virt, kind) -> Result<Option<PmapUnmapResult>, PmapError>;
fn protect_kernel_mapping(virt, kind, permissions) -> Result<Option<PmapInvalidation>, PmapError>;
fn shootdown_kernel_mapping(invalidation);
// plus the direct-map specials: reserve/commit_kernel_direct_map_1g, extend_direct_map
```

The process-root family takes an explicit `&PmapRoot`:

```rust
fn create_pmap_root() -> Result<PmapRoot, PmapError>;
fn destroy_pmap_root(root);
fn reserve_mapping(root, virt, phys, kind) -> Result<Option<PmapReservation>, PmapError>;
fn commit_mapping(root, reservation, permissions);
fn unmap_mapping(root, virt, kind) -> Result<Option<PmapUnmapResult>, PmapError>;
fn protect_mapping(root, virt, kind, permissions) -> Result<Option<PmapInvalidation>, PmapError>;
fn shootdown_mapping(asid, invalidation);
fn activate_user_pmap(root);
```

On the RV64 board every one of these is implemented (no `Unsupported` stubs) by
delegating to the `pmap` submodule (`boards/tx-hal-riscv64-qemu-virt/src/lib.rs:351`
onward). The kernel-half methods live in `pmap/kernel_space.rs`; the process-root
methods in `pmap/address_space.rs`.

## The kernel half and the direct-map invariant

The kernel half is established once and then only *extended*, never torn down at
its base. The bootstrap mappings (built by the Chapter 4 trampoline, adopted in
Chapter 5) are published as a `BootstrapPmapInfo` (`crates/tx-hal/src/lib.rs:658`):

```rust
pub struct BootstrapPmapInfo {
    pub root: PhysAddr,
    pub mapped: PhysRange,
    pub direct_map_base: VirtAddr,
    pub direct_map: VirtRange,
    pub kernel_image: VirtRange,
    pub identity: Option<VirtRange>,             // None after the bridge is dropped
    pub pt_node_pool: PhysRange,
    pub reserved_page_tables: &'static [PhysRange],   // substrate subtracts these from free RAM
}
```

`reserved_page_tables` is how substrate learns which physical pages are *already*
page tables (the root, the kernel-alias L1, the L0 table range, the PT-node pool)
so it doesn't hand them to the frame allocator. `identity` goes `None` the moment
`drop_identity_bridge` runs (Chapter 5).

**The direct map is special and permanent.** `extend_direct_map(phys_end)`
(`kernel_space.rs`) reserves and commits additional 1 GiB superpages to cover all
of RAM (substrate calls it once it knows the RAM size), but the direct map is
*never* unmapped — `unmap_kernel_mapping` explicitly rejects a 1 GiB request with
`InvalidRequest`. The doc pins this as the direct-map invariant
(`txdoc:HAL-PMAPIF-…-THE-DIRECT-MAP-INVARIANT-1`): it's established in H1, extended
in substrate phase 2, and persists for the kernel lifetime. Every process root
maps the kernel high-half *identically*, so the direct map, kernel text, and MMIO
are visible during user execution as well as kernel execution. Chapter 7's
`page_table_mut_from_phys` depends on this — it forms page-table pointers through
the direct map, which only works because the direct map is always there.

Kernel-half shootdown is the simplest correct thing plus a remote kick
(`lib.rs:411`):

```rust
fn shootdown_kernel_mapping(invalidation: PmapInvalidation) {
    pmap::shootdown_kernel_mapping(invalidation);   // local sfence.vma
    remote_sfence_vma(invalidation);                // SBI RFENCE to online harts
}
```

The map-count bookkeeping the doc worries about (`txdoc:HAL-PMAPIF-…-UNMAP-AND-SHOOTDOWN-1`)
stays in *substrate*, not HAL — substrate owns `FrameMeta`, so a substrate-side
batch wraps the HAL `shootdown_*` call with its pre/post discipline (drop the
`MapPin` only after shootdown returns). HAL never calls into substrate; the
dependency arrow points one way (decision §1.8).

## Creating a process root

`create_pmap_root` (`address_space.rs:75`) is where a new address space is born:

```rust
pub(super) fn create_pmap_root_from_bag<State>(bag: &BootStaticBag<State>)
    -> Result<PmapRoot, PmapError>
{
    let asid = alloc_asid()?;                         // 1. an ASID from the bitmap
    let node = match alloc_pt_node_from_bag(bag) {    // 2. a fresh root page
        Ok(node) => node,
        Err(_) => { free_asid(asid); return Err(PmapError::Exhausted); }
    };
    let root = unsafe { page_table_mut_from_phys(node.phys) };
    root.0.fill(0);                                   // 3. zero the user half
    let kernel_root = unsafe { bag.bootstrap_root_mut() };
    root.0[256..].copy_from_slice(&kernel_root.0[256..]);  // 4. copy kernel high-half
    Ok(PmapRoot::new(node, asid))
}
```

Step 4 is the heart of it: Sv39 has 512 root entries; slots `[0..256)` are the
user half and `[256..512)` are the kernel half. Copying the kernel root's upper
256 entries into every new root is what makes the direct map and kernel image
visible in every address space without per-process work. The user half starts
empty; the VM subsystem fills it via `reserve_mapping`/`commit_mapping`.

`PmapRoot` itself is a concrete `tx-hal` type (`crates/tx-hal/src/lib.rs:566`):

```rust
#[must_use]
pub struct PmapRoot { node: PtNode, asid: Asid }
```

— the root page plus its ASID, bundled. (Contrast the doc's "full surface" where
this was the associated type `P::PmapRoot`; see Chapter 7's divergence note.)

Tearing one down (`destroy_pmap_root`, `address_space.rs:99`) walks only the
*user* half `[..256]`, recursively releasing branch sub-trees through the
committed-PT-node registry, zeroes the slots, invalidates the root's translations,
frees the ASID, and returns the root node. The kernel half is never freed — it was
shared, not owned.

## ASIDs: a bitmap and a reuse hazard

ASIDs let the hardware tag TLB entries by address space, so a context switch
doesn't require flushing the whole TLB. The board allocates them from a fixed
1024-slot bitmap (`address_space.rs:54`):

```rust
const ASID_BITMAP_WORDS: usize = 16;
pub(crate) const ASID_CAPACITY: usize = ASID_BITMAP_WORDS * 64;   // 1024
static ALLOCATED_ASIDS: [AtomicU64; ASID_BITMAP_WORDS] = [const { AtomicU64::new(0) }; 16];
```

ASID 0 is reserved (treated as global/kernel). `alloc_asid` is an atomic
CAS-loop; exhaustion returns `Exhausted`. The subtle correctness point is *reuse*:
when a root is destroyed and its ASID later reassigned, stale TLB entries tagged
with that ASID must not survive. The board handles this at teardown —
`destroy_pmap_root` invalidates the root's translations before the ASID is freed,
and `activate_user_pmap` (below) documents the full reuse-fencing argument.

## The `satp` write — and the fast path that matters

`activate_user_pmap` (`lib.rs:476`) is called by the thread runtime immediately
before entering userspace (Chapter 10), to point the MMU at the process's root:

```rust
fn activate_user_pmap(root: &PmapRoot) {
    mark_asid_resident_on_current_cpu(root.asid());
    unsafe {
        const SATP_MODE_SV39: usize = 0x8 << 60;
        let satp = SATP_MODE_SV39 | ((root.asid().0 as usize) << 44) | (root.phys().0 >> 12);

        // Fast path: returning to the same address space (the common syscall return).
        let current: usize;
        core::arch::asm!("csrr {satp}, satp", satp = out(reg) current, options(nomem, nostack));
        if current == satp {
            return;                  // no CSR write, no fence — TLB still valid
        }
        core::arch::asm!("csrw satp, {satp}", satp = in(reg) satp, options(nostack));
    }
}
```

Two things make this correct *and* fast:

1. **Same-ASID fast path.** The overwhelmingly common case — a syscall returns to
   the same process — does a single `csrr` compare and bails with no CSR write and
   no fence, because the TLB entries for that ASID are still valid.
2. **No `sfence.vma` on the switch.** Even when switching roots, the code writes
   `satp` *without* a global fence. The inline comment lays out the privileged-spec
   argument: TLB entries are ASID-tagged so switching ASIDs needs no fence; invalid
   (`V=0`) PTEs are never cached, so invalid→valid map commits are picked up by the
   next hardware walk unfenced (the commit path only fences when overwriting a
   *valid* PTE); and ASID reuse is fenced at teardown, not here.

The comment also records *why* this matters: the previous unconditional
`sfence.vma` on every userspace entry flushed the whole TLB per syscall return,
which under QEMU's TCG meant a full `tlb_flush` (≈ milliseconds) on every return —
the dominant term in LTP shell-test runtime. The fast path turned a per-syscall
TLB flush into a single CSR read. This is a concrete reminder that the HAL's hot
paths (`activate_user_pmap`, `read_ns`, the trap entry) are performance-critical,
and the proof-object discipline doesn't get to cost a flush per call.

## Process-root mapping, briefly

`reserve_mapping` / `commit_mapping` / `unmap_mapping` / `protect_mapping` mirror
the kernel-half versions exactly (same `PmapReservation`, same `#[must_use]`, same
explicit rollback), with two differences: they take a `&PmapRoot`, and they
validate against the *user* address ceiling (`validate_user_mapping_virt` rejects
anything reaching above `SV39_USER_ALLOC_TOP`, `topology.rs:82`). Reservation
allocates intermediate tables and carries them in the token; commit installs the
leaf with the requested `PmapPermissions` (including `USER`) and registers the
intermediates so a later unmap can reclaim them; `protect_mapping` updates a
same-granularity present leaf in place and returns a `PmapInvalidation`, leaving
unsafe cases (a superpage that would need splitting) to return `InvalidRequest` so
VM keeps its recipe authoritative and rematerializes on fault. This last point is
a recurring HAL stance: when an operation can't be done *safely and simply* at the
HAL level, it returns `InvalidRequest` rather than doing something clever, and the
owning subsystem handles the hard case.

## What you should take away

- One `PmapIf` serves both the global kernel half and per-process roots, with
  parallel method families distinguished by an explicit `&PmapRoot`.
- The direct map is permanent and shared into every process root by copying the
  kernel high-half (`root[256..]`) at `create_pmap_root`; `page_table_mut_from_phys`
  relies on it.
- ASIDs come from a 1024-slot bitmap (0 reserved); reuse is fenced at teardown.
- `activate_user_pmap`'s same-ASID fast path skips the CSR write entirely, and even
  a real switch avoids a global `sfence.vma` — a measured, spec-justified
  optimization that dominated syscall-return cost under TCG.

This closes Part III. Part IV turns to the other load-bearing trait:
[Chapter 9 — Trap entry and the shell-to-sink contract](ch09-trap-shell-to-sink.md).
</content>
