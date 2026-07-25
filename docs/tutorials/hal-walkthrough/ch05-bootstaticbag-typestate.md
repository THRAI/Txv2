# Chapter 5 — Typestate as a boot-safety guard (`BootStaticBag`)

Chapter 4 left us at the high `rust_entry`, MMU on, three VA windows live —
including a **temporary low identity bridge** that exists only so the trampoline's
final instructions could keep executing across the `satp` write. That bridge is a
loaded gun: as long as it's mapped, any code that still holds a low address can
dereference it and *appear* to work, right up until the bridge is torn down and
the same code starts faulting. Worse, an address captured during the identity era
is indistinguishable, at the `usize` level, from a perfectly good runtime address.

txKernel's answer is to make "identity-era authority is live" a **typestate** that
the compiler tracks, and to make the bag that carries boot facts *neither `Copy`
nor `Clone`* so the transition is a one-way, move-only event. This is the
board-private realization of the doc's boot-static authority pipeline
(`txdoc:HAL-BOOTINFOIF-THE-STATIC-REF-DISCIPLINE-1`).

## The two states

`boot_static.rs:20`:

```rust
pub(crate) struct IdentityLive;
pub(crate) struct IdentityDropped;
```

Two zero-sized marker structs. They are used only as the `State` type parameter of
the bag:

```rust
pub(crate) struct BootStaticBag<State> {
    kernel_start: BootLinkedAddr,
    kernel_end: BootLinkedAddr,
    text_start: BootLinkedAddr,  text_end: BootLinkedAddr,
    rodata_start: BootLinkedAddr, rodata_end: BootLinkedAddr,
    data_start: BootLinkedAddr,  data_end: BootLinkedAddr,
    bss_start: BootLinkedAddr,   bss_end: BootLinkedAddr,
    boot_stack_bottom: BootLinkedAddr, boot_stack_top: BootLinkedAddr,
    global_pointer: BootLinkedAddr,
    rust_entry: BootLinkedAddr,
    trap_vector: BootLinkedAddr,
    bootstrap_root: BootLinkedAddr,
    kernel_alias_l1: BootLinkedAddr,
    kernel_alias_l0_tables: BootLinkedAddr,
    pt_node_pool: BootLinkedAddr,
    dtb: FirmwareDtb,
    _state: PhantomData<State>,
}
```

There is no `#[derive(Clone, Copy)]`. The bag can only be *moved*, never
duplicated. The `_state: PhantomData<State>` field costs zero bytes but lets the
type system distinguish `BootStaticBag<IdentityLive>` from
`BootStaticBag<IdentityDropped>` — and lets methods be defined on only one of
them.

### `BootLinkedAddr` — a fact, not a pointer

Each field is a `BootLinkedAddr` (`boot_static.rs:40`), a newtype over `usize`
that knows how to *re-express* a captured value as the right kind of address:

```rust
impl BootLinkedAddr {
    pub(crate) const fn phys(self) -> PhysAddr { PhysAddr(self.0) }
    pub(crate) const fn kernel_alias_va(self) -> Option<VirtAddr> {
        let offset = self.0.checked_sub(QEMU_KERNEL_PHYS_BASE)?;
        if offset >= KERNEL_BOOTSTRAP_ALIAS_SIZE { return None; }
        Some(VirtAddr(KERNEL_VIRT_BASE + offset))
    }
    // direct_va, identity_va (test-only) …
}
```

`from_runtime_addr` (used on the rv64 build) takes a *high* runtime address and
folds it back to its physical fact by subtracting `KERNEL_VIRT_BASE`. The point:
the bag stores **physical facts**, and exposes typed accessors that say which
mapping you want (`.phys()`, `.kernel_alias_va()`). Consumers never do the
arithmetic themselves — exactly the Chapter 3 discipline, applied to boot.

## The single global slot and its guard states

The bag lives in one static cell, and the cell's enum encodes the transition with
two *extra* states beyond the two typestates:

`boot_static.rs:270`:

```rust
enum StoredBootStaticBag {
    Uninit,
    IdentityLive(BootStaticBag<IdentityLive>),
    Taken,                                    // transition-in-progress sentinel
    IdentityDropped(BootStaticBag<IdentityDropped>),
}
```

`Uninit` → `IdentityLive` → `Taken` → `IdentityDropped` is the only legal path,
and every method that touches the slot panics on any other transition. `Taken` is
the clever bit: it's the momentary state while the bag has been moved *out* of the
global for advancement but the dropped form hasn't been moved back in. If anything
tries to observe the global during that window, it gets a clear panic
("transition in progress") rather than a half-updated bag.

## The pipeline, as it actually runs

Recall from Chapter 2 that `kernel_main` is one line. The bag pipeline runs even
earlier — inside `BootPlatformIf::boot_handoff`, which `tx_hal::entry` calls before
it hands off to the kernel (Chapter 6). From
`boards/tx-hal-riscv64-qemu-virt/src/lib.rs:292`:

```rust
fn boot_handoff(cpu_id: usize, firmware_arg: usize) -> BootHandoff {
    let bag = BootStaticBag::<IdentityLive>::capture_once(firmware_arg);
    pmap::adopt_high_linked_bootstrap_pmap(bag);

    BootStaticBag::<IdentityLive>::take_global()
        .publish_boot_info_before_identity_drop(firmware_arg)
        .complete_post_entry_pipeline()
        .install_global();

    BootHandoff {
        cpu_id: CpuId(cpu_id),
        firmware_arg: BootArg(firmware_arg),
        protocol: Self::BOOT_PROTOCOL,
    }
}
```

That chained call is the whole low→high authority transition. Walk it:

**1. `capture_once(dtb_addr)`** (`boot_static.rs:302`). Reads the linker symbols
and the DTB pointer into a fresh `BootStaticBag<IdentityLive>`, stores it as the
global, and returns a `&'static mut` to it. Calling it twice panics
("constructed more than once") — boot facts are captured exactly once.

**2. `pmap::adopt_high_linked_bootstrap_pmap(bag)`** (`pmap/mod.rs:127`). Tells
the pmap subsystem to adopt the trampoline-built page tables as its
steady-state bootstrap pmap, re-expressing the linker symbols as high-linked
facts and publishing `BootstrapPmapInfo` (Chapter 8).

**3. `take_global()`** (`boot_static.rs:322`). Moves the `IdentityLive` bag *out*
of the global, leaving `Taken` behind. From here the bag is owned by the call
chain; the typestate transition can proceed.

**4. `publish_boot_info_before_identity_drop(firmware_arg)`** (`lib.rs:1549`). This
is the crucial ordering constraint encoded in a method name: BootInfo is parsed
from the DTB **while the identity bridge is still live**, because parsing the DTB
may need to read firmware memory reachable through it. We look at the parse below.
It returns `self` (still `IdentityLive`) for chaining.

**5. `complete_post_entry_pipeline()`** (`pmap/mod.rs:313`). This is where the
typestate flips. On rv64 it:
   - takes a **high sentinel** — reads the *current* `pc`, `sp`, and `gp`
     (`HighSentinel::current()`, `pmap/mod.rs:379`, via `auipc`/`mv`);
   - validates all three are inside the high kernel alias range
     (`validate_high_sentinel`, `pmap/mod.rs:395`) — proving execution genuinely
     crossed into the high half and nothing still rides a low address;
   - calls `drop_lower()` → `drop_identity_bridge()` (`pmap/mod.rs:361`), which
     zeroes root slot for `QEMU_RAM_BASE` (the identity leaf), clears
     `BootstrapPmapInfo.identity`, and issues `sfence.vma`;
   - returns a `BootStaticBag<IdentityDropped>`.

   The consume-by-value signature `fn complete_post_entry_pipeline(self) ->
   BootStaticBag<IdentityDropped>` is the safety property in the type system:
   **the `IdentityLive` bag is consumed** by this call. After it returns, no
   `IdentityLive` value exists anywhere, so no code can call an `IdentityLive`-only
   method — and those are exactly the methods with identity-era dereference
   authority. The bridge is gone *and* the capability to use it is gone, together.

**6. `install_global()`** (`boot_static.rs:534`). Moves the `IdentityDropped` bag
back into the global (the `Taken` → `IdentityDropped` step). From now on,
`BootInfoIf` and `PlatformInfoIf` read through `global_ref()`.

If anything in the sentinel check fails, the code does **not** continue with a
bad mapping — `require_current_high_sentinel_or_spin` (`pmap/mod.rs:300`) spins
forever, turning a would-be silent corruption into an obvious hang you can catch
with `fault-decode`.

## DTB parsing into `'static` facts, no allocator

`publish_boot_info_from_fdt` (`lib.rs:1557`) is where the firmware blob becomes
the cross-platform `BootInfo`. The mechanics honor the no-heap rule
(`txdoc:HAL-BOOTINFOIF-THE-STATIC-REF-DISCIPLINE-1`):

- The bag owns *static buffers* — a `[MemoryRegion; MAX_MEMORY_REGIONS]` (8) and a
  16 KiB cmdline buffer — carved out of BSS, sized for the worst case the board
  could report.
- `parse_boot_info_from_fdt` (`dtb.rs`) walks the device tree and fills those
  buffers, returning a small `DtbBootInfo` summary (region count, initrd range,
  cmdline length, timebase frequency, CPU count).
- The slices handed to `BootInfo` are `core::slice::from_raw_parts` over those
  static buffers — `&'static [MemoryRegion]`, no `Vec`, no `String`.

If the DTB can't be parsed at all, there's a hard-coded fallback: one usable
region at `QEMU_VIRT_RAM_BASE` of `QEMU_VIRT_FALLBACK_RAM_SIZE`, default timebase,
one CPU. The kernel always gets *a* valid `BootInfo`.

The resulting `BootInfo` (the cross-platform type from `crates/tx-hal/src/lib.rs:162`):

```rust
pub struct BootInfo {
    pub memory_regions: &'static [MemoryRegion],   // usable + reserved, sorted
    pub kernel_image: PhysRange,                    // so substrate marks it reserved
    pub initrd: Option<PhysRange>,                  // VFS mounts the initial root
    pub cmdline: Option<&'static str>,
}
```

Substrate consumes *this*, never the raw DTB. The doc's rationale
(`txdoc:HAL-BOOTINFOIF-WHY-THIS-IS-IN-HAL-AND-NOT-THE-KERNEL-1`): the firmware
data format is platform-specific; `BootInfo` is the portable interface that
emerges from parsing it. A LA64 board parses a different firmware structure into
the same `BootInfo`.

`PlatformInfo` (MMIO regions, timebase, CPU count) is published the same way and
read via `PlatformInfoIf::platform_info()`. The split exists because `BootInfo`
describes RAM (needed in substrate phase 1) while `PlatformInfo` describes MMIO
(needed in phase 3, when the kernel page table is extended to cover devices).

## Why this design earns its complexity

A simpler kernel would parse the DTB into some globals and unmap the identity
bridge whenever convenient, trusting code review to ensure nothing uses a low
address afterward. txKernel instead spends a typestate parameter and a move-only
bag to convert that trust into a compile-time guarantee: *you cannot name an
identity-era fact through an authority that no longer exists, because the value
carrying that authority was consumed at the transition.* The `Taken` sentinel and
the spin-on-bad-sentinel turn the remaining runtime hazards into loud failures
instead of quiet ones. For boot code — which runs once, is hard to test, and
fails in ways that look like hardware bugs — that trade is worth it.

## What you should take away

- `BootStaticBag<State>` is move-only (no `Copy`/`Clone`); the `State` type
  parameter tracks whether identity-era dereference authority is still live.
- The pipeline `capture_once → adopt pmap → take_global →
  publish_boot_info_before_identity_drop → complete_post_entry_pipeline →
  install_global` is the low→high authority transition, and it runs inside
  `boot_handoff`.
- `complete_post_entry_pipeline(self)` *consumes* the `IdentityLive` bag, so after
  the bridge is dropped the capability to use it is gone from the type system too;
  a high-sentinel check (pc/sp/gp in the high alias) gates the drop.
- DTB → `'static BootInfo`/`PlatformInfo` uses BSS-carved buffers, never the heap;
  substrate consumes the portable types, never the raw blob.

Next: [Chapter 6 — The portable handoff shell and the real mainline](ch06-handoff-shell-mainline.md),
where `tx_hal::entry` ties H1's assembly to the generic kernel, and we see how the
doc's tidy `kernel_main` skeleton became the real `CoreInit` sequence.
</content>
