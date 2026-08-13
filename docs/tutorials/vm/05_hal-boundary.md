# Chapter 5 — The HAL boundary: page tables, satp, and TLB shootdown

Chapter 4 stopped at the `ops` function pointers — the pmap handed reservations
and shootdown batches to "the platform." This chapter is the platform: the
`PmapIf` HAL trait that defines the VM↔hardware seam, and its RISC-V Sv39
implementation. This is where virtual memory stops being a data structure and
becomes silicon.

It is also where three of txKernel's commitments become concrete: no KPTI (the
shared kernel half), no swap (frames pinned, never paged), and real SMP
correctness (ASID-scoped cross-hart shootdown).

## `PmapIf`: the seam

`PmapIf` (`tx-hal/src/lib.rs:670`) is the trait a board implements to give the VM
a page table. The VM is generic over it (`P: PmapIf` threads through
`AddressSpace::new_for_platform`, `VmPmap::new_for_platform`); the board provides
the one real implementation selected at build time. Its methods fall into three
groups.

**Page-table node allocation** — the page table is itself made of physical
frames, and they have to come from somewhere:

```rust
fn alloc_pt_node() -> Result<PtNode, AllocError>;      // :675
fn free_pt_node(node: PtNode);                          // :679
fn install_pt_node_allocator(a: PtNodeAllocator) -> …;  // :681
```

**Per-address-space mapping** — the methods the pmap's `ops` point at:

```rust
fn create_pmap_root() -> Result<PmapRoot, PmapError>;   // :730
fn destroy_pmap_root(root: PmapRoot);                    // :734
fn reserve_mapping(root, virt, phys, kind) -> …;         // :747
fn commit_mapping(root, reservation, permissions);       // :758
fn unmap_mapping(root, virt, kind) -> …;                 // :765
fn protect_mapping(root, virt, kind, permissions) -> …;  // :773
fn activate_user_pmap(root: &PmapRoot);                  // :803
```

**Shootdown** — invalidating stale TLB entries, scoped to an address space's
ASID:

```rust
fn shootdown_mapping(asid: Asid, invalidation: PmapInvalidation);    // :782
fn shootdown_mappings(asid: Asid, invalidations: &[PmapInvalidation]); // :784
```

The reserve/commit split you saw in Chapter 4 lives here: `reserve_mapping` walks
the tree and allocates any missing intermediate nodes, returning a
`PmapReservation`; `commit_mapping` writes the leaf PTE. A reservation that is
never committed rolls back, freeing the nodes it allocated — so a publish that
fails midway leaves the hardware untouched.

## The Sv39 three-level walk

On the RISC-V QEMU virt board, a `PmapRoot` is the physical frame holding the top
(L2) page table. Mapping a 4 KiB page walks three levels, allocating L1 and L0
tables on demand:

```
   VA[38:30] → L2 slot ─▶ (1 GiB) or pointer to L1 table
   VA[29:21] → L1 slot ─▶ (2 MiB) or pointer to L0 table
   VA[20:12] → L0 slot ─▶ the 4 KiB leaf PTE
   VA[11:0]  → page offset
```

`reserve_mapping` (`boards/tx-hal-riscv64-qemu-virt/src/pmap/address_space.rs:434`)
performs this walk: for a 4 KiB page it ensures an L1 table exists under the root,
then an L0 table under that, then prepares the leaf slot — allocating each missing
table via `alloc_pt_node`. The leaf PTE packs the physical page number with the
R/W/X/U/G/A/D bits derived from the recipe's `Prot`. `commit_mapping` writes it.

Page-table nodes come from a `PtFrame` — a typed frame token
(`page_allocator/tokens.rs:175`) proving the frame is dedicated to a page table —
allocated from the substrate frame allocator at runtime, or from a static
`PT_NODE_POOL` during early boot before the allocator is online. `free_pt_node`
returns runtime-allocated nodes; boot-pool nodes stay in the pool. (This is the
no-swap commitment showing through: a leaf, once mapped, stays mapped until an
explicit teardown — the walk never has to fault a table back in from disk.)

## The shared kernel half: no KPTI

Every address space needs the kernel mapped (so syscalls and traps can run), but
duplicating the kernel's page tables per process would be enormous waste and a
`fork` cost. txKernel shares them by reference. When a new root is created, the
upper half of the table — the kernel's L2 slots — is copied from the boot root:

```rust
// boards/tx-hal-riscv64-qemu-virt/src/pmap/address_space.rs:94
root.0[256..].copy_from_slice(&kernel_root.0[256..]);
```

On Sv39 the 512 L2 slots split evenly: slots `0..256` are user space (zeroed for
a fresh root), slots `256..512` are kernel space. The copy duplicates only the
*top-level pointers* — the L1/L0 tables they point at are **shared**, not copied.
So every address space sees the same kernel text, data, direct map, and MMIO
through the same underlying tables. If the kernel ever extends a high-half mapping
post-boot, all address spaces see it at once.

Two consequences the rest of the series relies on:

- **`fork` never touches the kernel half.** Cloning an address space copies only
  the user-side recipes (Chapter 3) and demotes user PTEs (Chapter 8); the kernel
  half is already shared by reference. (This is why Chapter 3's `clone_shared` can
  be O(1) — there is no kernel mapping to duplicate.)
- **No KPTI.** Because the kernel is always mapped in every address space, there
  is no kernel/user page-table switch on syscall entry. (KPTI exists on x86 to
  mitigate Meltdown by *unmapping* the kernel from user page tables; txKernel's
  threat model does not pay that cost.)

## Switching address spaces: the satp write

Running a thread means pointing the MMU at its address space's root — writing the
`satp` CSR. This is `switch_mm`'s job in Linux; here it is `activate_user_pmap`:

```rust
// boards/tx-hal-riscv64-qemu-virt/src/lib.rs:476  (simplified)
fn activate_user_pmap(root: &PmapRoot) {
    mark_asid_resident_on_current_cpu(root.asid());     // for shootdown targeting
    let satp = SATP_MODE_SV39 | (asid << 44) | root_ppn;
    let current = read_csr(satp);
    if current == satp { return; }                       // already active: skip
    write_csr(satp, satp);                               // NO global sfence.vma
}
```

Two optimizations matter:

- **Skip if unchanged.** Re-activating the same root (common when a thread yields
  and resumes) is a no-op — no CSR write, no fence.
- **No `sfence.vma` after the switch.** Each address space has its own **ASID**
  (address-space identifier), and the TLB tags entries by ASID. Switching ASIDs
  does not require flushing, because the new ASID's entries are distinct from the
  old's. A blanket `sfence.vma` here would flush the *whole* TLB on every context
  switch — exactly the cost ASIDs exist to avoid. Stale entries are instead
  invalidated precisely, at teardown, by shootdown.

This is the payoff of the binding/materialization discipline reaching all the way
to hardware: because PTEs are only ever *added* against a freshly observed recipe
and *removed* with an explicit shootdown, the switch itself needs no flush.

## TLB shootdown on SMP

When a PTE is torn down (`munmap`, CoW, `mprotect` teardown), every hart that
might have cached that translation must invalidate it. On a uniprocessor this is
a local `sfence.vma`. On SMP it is a cross-hart operation, and txKernel scopes it
tightly by ASID:

```rust
// boards/tx-hal-riscv64-qemu-virt/src/lib.rs:1407  (simplified)
pub(crate) fn remote_sfence_vma_asid_batch(asid: Asid, invalidations: &[PmapInvalidation]) {
    if invalidations.is_empty() { return; }
    let targets = remote_sfence_targets_for_asid(asid);   // only harts caching this ASID
    if targets.is_empty() { return; }                     // nobody else has it: done
    for invalidation in invalidations {
        sbi_remote_sfence_vma_asid(targets.bits(), 0,
            invalidation.virt().0, invalidation.size(), asid.0);  // SBI IPI + remote flush
    }
}
```

The `sbi_remote_sfence_vma_asid` SBI call sends IPIs to the target harts and waits
for them to flush, atomically from the caller's view. The targeting is the clever
part:

```rust
// boards/tx-hal-riscv64-qemu-virt/src/lib.rs:89
static ASID_RESIDENCY: [AtomicU64; ASID_CAPACITY] = …;   // one bitmask per ASID
```

`activate_user_pmap` sets the bit for `(asid, current_hart)` every time it
activates a root. `remote_sfence_targets_for_asid` reads that bitmask, so a
shootdown only IPIs harts that have *actually* run this address space — and
excludes the current hart, which flushes locally. A single-threaded process whose
ASID has only ever been resident on one hart pays **no** cross-hart cost; a
shootdown with no remote targets returns immediately. This is `mm_cpumask` in
spirit, kept per-ASID.

> **Decoding faults.** When a kernel fault or trap does occur on this board, the
> `cargo xtask fault-decode --target rv64-qemu` tool turns the raw
> `scause`/`sepc`/`stval` into a symbolized, demangled report (it understands the
> low-linked and high-VMA ELF layouts, RV64C compressed instructions, and even
> emits a synthetic line for kernel panics so they parse like hardware traps).
> When you are debugging a materialization bug, reach for it before hand-decoding
> CSRs — see the project guide.

## Other boards

The LoongArch64 board implements the same `PmapIf` contract with a different
page-table shape (a split PGDL/PGDH root and `invtlb`-based invalidation) — an
expected architecture variation behind the same trait. The VM above the seam does
not change; that is the point of putting the seam here.

## Where we go from here

You have the full materialization stack now: recipes (Chapter 3) → pmap shadow +
publish/teardown (Chapter 4) → `PmapIf` → Sv39 page tables, satp, and ASID-scoped
shootdown (this chapter). What we have not yet examined is how concurrent
operations are kept from stepping on each other — the narrow coordination that
lets all of this stay lock-free on the read path. That is `RangeLock`, Chapter 6.

## Source anchors

- `PmapIf` trait: `crates/tx-hal/src/lib.rs:670` (alloc `:675`, root `:730`, reserve/commit `:747/:758`, unmap/protect `:765/:773`, activate `:803`, shootdown `:782/:784`)
- Sv39 walk + node allocation (`reserve_mapping`): `boards/tx-hal-riscv64-qemu-virt/src/pmap/address_space.rs:434`
- Kernel high-half copy (`create_pmap_root`): same file, `:75, :94`
- `PtFrame` / page-table-node tokens: `crates/tx-substrate/src/page_allocator/tokens.rs:175`
- `activate_user_pmap` (satp write, skip-if-unchanged, no-fence): `boards/tx-hal-riscv64-qemu-virt/src/lib.rs:476`
- ASID-scoped shootdown (`remote_sfence_vma_asid_batch`): same file, `:1407`; `ASID_RESIDENCY` `:89`; targeting `:1443`
- LoongArch64 variant: `boards/tx-hal-loongarch64-qemu-virt/src/la64_pmap.rs`
- Commitments (no swap / no KPTI): `docs/design/03_memory-vm/VM_v1_2.md` §2; `docs/design/01_substrate/PAGE_SUBSTRATE_v1.md`
- `fault-decode` tooling: project `CLAUDE.md`; `cargo xtask fault-decode --help`
