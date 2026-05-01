# HAL — Platform Abstraction Layer

<!-- txdoc:01-SUBSTRATE-HAL-V1 -->

**Status.** v1 (2026-04-25).

**Purpose.** Specify the platform abstraction layer that sits below substrate and every kernel subsystem. HAL defines the contract a board crate must fulfill for a generic txKernel binary to boot, run, and shut down on it. This document fixes the axHal-style static platform shape, the trait surface, the crate split, the boot handoff, and the discipline around static registration.

**Scope.** Everything from "the firmware hands control to platform `__start`" to "`P::init_later()` returns and the kernel mainline begins downstream subsystem init." Does *not* cover substrate frame allocator mechanics (those live in [`PAGE_SUBSTRATE_v1.md`](PAGE_SUBSTRATE_v1.md)), tier-2 device construction (those live in [`DEVICE.md`](../06_devices/DEVICE.md)), or kernel-level SMP coordination (deferred to `SMP_v1.md`).

**Audience.** Anyone porting txKernel to a new board, anyone implementing a subsystem that consumes HAL traits (substrate, VM, device, reactor, scheduler, signal, syscall pipeline), reviewers auditing the dependency graph between the kernel and the platform.

**Companion documents.**

- [`PAGE_SUBSTRATE_v1.md`](PAGE_SUBSTRATE_v1.md) — substrate is HAL's primary consumer; §2's deliverables list is now realized here as platform obligations.
- [`DEVICE.md`](../06_devices/DEVICE.md) — tier-1 devices (PLIC, CLINT/timer, early UART) live in HAL; tier-2 device construction sits above HAL.
- [`00_meta-framework/SUBSYSTEM_ANATOMY_v2_1.md`](../00_meta-framework/SUBSYSTEM_ANATOMY_v2_1.md) — HAL is *not* a subsystem; it predates the four-module discipline and has its own organization.
- [`00_meta-framework/INVARIANTS_v4.md`](../00_meta-framework/INVARIANTS_v4.md) — MAP and HAL/foundation invariants; TLB shootdown ordering is realized by `PmapIf::shootdown` plus the substrate-side post-shootdown accounting.

**Interface status.** This document is the axHal-style replacement for the older OSTD-style HAL management notes. "axHal-style" means: one statically selected platform family, compile/link-time platform choice, no runtime HAL manager, no boxed dynamic HAL trait object, no HAL-owned semantic objects, and no subsystem callbacks into HAL initialization order. txKernel still gives the surface typed names (`TxPlatform`, `PmapIf`, `TrapIf`, etc.) because upper documents need proof objects and trap-frame views, but those traits describe a static platform module family rather than a managed HAL service.

---

## 1. Design decisions
<!-- txdoc:HAL-DESIGN-DECISIONS-1 -->

These decisions are normative. The rest of the document specifies how each one is realized.

1. **`tx-hal` is the shared interface crate for an axHal-style platform family.** It defines shared HAL vocabulary, proof-object types, trap-frame views, pmap batch types, platform configuration traits, and static registration slice types. It contains no board constants and no concrete machine code.

2. **Each board has one platform crate.** `tx-hal-<arch>-<board>` is the axHal-style concrete platform module: it owns the linker script, `__start`, bootstrap page table, early console, trap vectors, IRQ controller, timer source, concrete pmap, cache/DMA fences, platform discovery, and power hooks.

3. **Platform selection is static.** A board kernel binary chooses one `ActivePlatform` type at link time. There is no runtime platform dispatch. One board crate produces one kernel binary.

4. **PAGE_SUBSTRATE §2 deliverables become platform obligations.** `BootInfo`, bootstrap page table, bootstrap PT-node allocation, `PlatformInfo`, early console, trap infrastructure, and pmap prepare/commit/shootdown all enter through the statically selected `TxPlatform` axis.

5. **`PmapIf` and `TrapIf` are first-class HAL traits.** They are not axplat-shaped convenience traits. They carry the proof objects and trap-frame discipline required by txKernel's publication, VM, signal, and syscall models.

6. **`CacheIf`, `DmaIf`, and `SmpIf` are explicit axes.** Cache and DMA are small in v1 but prevent drivers, exec, and VM from smuggling architecture `cfg`. HAL owns only low-level SMP mechanics; the AP coordination protocol belongs to `SMP_v1`.

7. **`linkme` registration is allowed only for typed static dispatch tables.** HAL owns the dispatch shell; semantic handlers remain owned by VM, syscall, signal, scheduler, or device subsystems. Trap-cause dispatch is direct and named, not via linkme.

8. **Dependency direction.** HAL may depend on the meta-framework vocabulary and shared primitive types only. HAL does not depend on substrate, VM, VFS, process, signal, device, or reactor. Substrate and all higher subsystems may depend on the shared HAL interface, never on a concrete platform crate. The board binary is the only place that depends on both a concrete platform crate and the generic kernel crate, and is the only place that selects the `ActivePlatform` type.

9. **No OSTD-style HAL manager.** There is no global HAL service object, late-bound architecture table, or `__ostd_main` handoff. Boot is the H0-H4 sequence in §5; after H3, HAL remains a callable static platform surface, not an initialized subsystem with ownership of semantic resources.

10. **Portable boot shape is mandatory for every board.** New board
    implementations must follow the ArceOS/axplat-style separation in this
    document: the platform crate owns `_start` and raw firmware conventions,
    the board binary owns only `ActivePlatform` selection plus `rust_entry`,
    `tx_hal::entry::<P, K>` translates into `BootHandoff`, and
    `tx_kernel::kernel_main::<P>` stays generic. Booting another board must
    never require adding board imports, runtime architecture dispatch, or
    firmware-register knowledge to `tx-kernel`.

---

## 2. Crate split and platform selection
<!-- txdoc:HAL-CRATE-SPLIT-AND-PLATFORM-SELECTION-1 -->

### 2.1 The four-crate pattern
<!-- txdoc:HAL-CRATE-SPLIT-AND-PLATFORM-SELECTION-THE-FOUR-CRATE-PATTERN-1 -->

```
tx-hal (trait crate)
  ├─ depends on: meta-framework primitive types only
  ├─ exports: TxPlatform, PlatformConfig, all *If traits,
  │           BootProtocol, BootHandoff, BootInfo, PlatformInfo,
  │           TrapFrameView/Mut,
  │           pmap proof-object types, registration slice types,
  │           tx_hal::entry::<P, K>
  └─ no concrete machine code, no board constants

tx-hal-<arch>-<board> (implementor crate)
  ├─ depends on: tx-hal
  ├─ exports: pub struct Platform; impl TxPlatform for Platform { ... }
  ├─ owns: linker script, __start, bootstrap assembly,
  │        raw firmware register conventions, BootPlatformIf,
  │        early page table, early console, trap vectors,
  │        IRQ controller, timer source, concrete pmap,
  │        cache/DMA fences, BootInfo construction,
  │        PlatformInfo construction
  └─ one such crate per board (qemu-virt-rv64, visionfive2,
                               qemu-virt-la64, 2k1000la)

tx-kernel (generic kernel crate)
  ├─ depends on: tx-hal, substrate, vm, vfs, device, ...
  ├─ exports: pub fn kernel_main<P: TxPlatform>(BootHandoff) -> !
  ├─ no #[cfg(target_arch)] anywhere; all arch-specific work
  │  is reached through P
  └─ one such crate, shared by all boards

tx-kernel-<arch>-<board> (binary crate)
  ├─ depends on: tx-hal, tx-hal-<arch>-<board>, tx-kernel
  ├─ contents: type ActivePlatform = ...::Platform;
  │            impl KernelMain<ActivePlatform> for Kernel { ... }
  │            #[no_mangle] extern "C" fn rust_entry(...)
  └─ one such crate per board; each produces one ELF
```

### 2.2 Why the four-crate split (not three)
<!-- txdoc:HAL-CRATE-SPLIT-AND-PLATFORM-SELECTION-WHY-THE-FOUR-CRATE-SPLIT-NOT-THREE-1 -->

The board binary cannot live in the implementor crate because it has to depend on `tx-kernel` (which depends on `tx-hal`), and the implementor crate already depends on `tx-hal`. Having the binary depend on both gives a clean diamond:

```
        tx-kernel-riscv64-qemu-virt (binary)
                  /         \
       tx-kernel             tx-hal-riscv64-qemu-virt
                  \         /
                    tx-hal
                       |
                meta-framework
```

There is no path from `tx-hal` to `tx-kernel`; there is no path from `tx-kernel` to any specific platform crate. The diamond closes only at the binary.

### 2.3 Selecting the platform at the binary
<!-- txdoc:HAL-CRATE-SPLIT-AND-PLATFORM-SELECTION-SELECTING-THE-PLATFORM-AT-THE-BINARY-1 -->

Each board binary's `main.rs` (or equivalent entry crate) names the active platform, supplies a kernel continuation, and exposes the C-ABI entry that platform `__start` jumps to:

```rust
// tx-kernel-riscv64-qemu-virt/src/main.rs
#![no_std]
#![no_main]

use tx_hal::{BootHandoff, KernelMain};

type ActivePlatform = tx_hal_riscv64_qemu_virt::Platform;

struct Kernel;

impl KernelMain<ActivePlatform> for Kernel {
    fn kernel_main(handoff: BootHandoff) -> ! {
        tx_kernel::kernel_main::<ActivePlatform>(handoff)
    }
}

#[no_mangle]
pub extern "C" fn rust_entry(cpu_id: usize, firmware_arg: usize) -> ! {
    tx_hal::entry::<ActivePlatform, Kernel>(cpu_id, firmware_arg)
}
```

Everything else — linker script, `__start` assembly, page-table bootstrap, MMIO bases — is in `tx-hal-riscv64-qemu-virt`.

### 2.3A Required board-binary minimalism
<!-- txdoc:HAL-CRATE-SPLIT-AND-PLATFORM-SELECTION-BOARD-BINARY-MINIMALISM-1 -->

Every later board binary must stay this small:

- select exactly one concrete `ActivePlatform`;
- implement `KernelMain<ActivePlatform>` by tail-calling
  `tx_kernel::kernel_main::<ActivePlatform>(handoff)`;
- export `rust_entry(cpu_id, firmware_arg)`;
- provide panic handling appropriate to the binary crate.

It must not define `_start`, linker symbols, MMIO constants, DTB parsing,
firmware register decoding, runtime platform tables, or board-selection logic.
Those belong to the concrete platform crate. This rule is what lets a future
LA64, M1 Dock, VisionFive2, or hardware-in-loop port reuse the same generic
kernel mainline.

### 2.4 No runtime dispatch
<!-- txdoc:HAL-CRATE-SPLIT-AND-PLATFORM-SELECTION-NO-RUNTIME-DISPATCH-1 -->

There is no `Box<dyn TxPlatform>`. There are no `match arch { ... }` blocks in tx-kernel. All HAL calls are monomorphized through `P: TxPlatform`. This is enforced by:

- `tx-kernel` having no `#[cfg(target_arch = ...)]` directives outside of vendored low-level helpers
- `tx-hal` having no concrete arch implementations
- The board binary being the only crate that names a specific `Platform` type

A code-review rule check (`grep -r "#\[cfg(target_arch" tx-kernel/`) is sufficient as a CI gate.

---

## 3. The TxPlatform supertrait
<!-- txdoc:HAL-THE-TXPLATFORM-SUPERTRAIT-1 -->

`TxPlatform` is a marker supertrait that aggregates every HAL axis. Its purpose is to give the kernel mainline a single bound:

```rust
pub trait TxPlatform:
    PlatformConfig
    + BootPlatformIf
    + InitIf
    + BootInfoIf
    + PlatformInfoIf
    + AuxvIf
    + ConsoleIf
    + PmapIf
    + TrapIf
    + UserAccessIf
    + SignalFrameIf
    + IrqIf
    + TimeIf
    + PercpuIf
    + CacheIf
    + DmaIf
    + SmpIf
    + PowerIf
    + 'static
{
}
```

`FpSimdIf` is *not* a supertrait component because v1 may compile platforms without it; consumers that need FPU state save/restore reach for `FpSimdIf` separately and the platform's `Platform` type may or may not implement it.

`BootPlatformIf` is the small bridge between platform-owned firmware conventions
and generic Rust boot code. A platform reports whether its raw entry registers
came from SBI, direct RISC-V firmware, or LoongArch firmware and translates
`(cpu_id, firmware_arg)` into a typed `BootHandoff` before `tx-kernel` runs.

### 3.1 The `'static` bound
<!-- txdoc:HAL-THE-TXPLATFORM-SUPERTRAIT-THE-STATIC-BOUND-1 -->

`TxPlatform` is `'static` because the `Platform` type carries no runtime state — every method is an associated function. The platform crate exposes a unit struct:

```rust
pub struct Platform;
impl TxPlatform for Platform {}
// trait impls follow for each *If
```

This means kernel code can write `<P as TimeIf>::read_ns()` without holding any `Platform` instance. The methods are all "associated function" form; there is no `&self`.

### 3.2 The kernel mainline bound
<!-- txdoc:HAL-THE-TXPLATFORM-SUPERTRAIT-THE-KERNEL-MAINLINE-BOUND-1 -->

The generic kernel mainline is:

```rust
// tx-kernel/src/lib.rs
pub fn kernel_main<P: TxPlatform>(handoff: BootHandoff) -> ! {
    P::init_early(handoff);
    substrate::init::<P>();
    P::init_later(handoff);
    // downstream subsystem init...
    // does not return
    P::system_off()
}
```

A single type parameter `P` flows through every HAL call. Substrate, VM, device, and any other subsystem with HAL needs declares its own `<P: TxPlatform>` (or a narrower bound like `<P: PmapIf + CacheIf>` if it only needs a slice).

---

## 4. PlatformConfig — associated constants
<!-- txdoc:HAL-PLATFORMCONFIG-ASSOCIATED-CONSTANTS-1 -->

`PlatformConfig` is a trait of associated constants. It exposes the compile-time facts about a board that subsystems need to compute layouts, strides, masks, and limits.

```rust
pub trait PlatformConfig {
    /// Architecture identifier.
    const ARCH: Arch;

    /// Board identifier (debug/diagnostic only).
    const BOARD: &'static str;

    /// Page size in bytes. Always 4096 on supported platforms.
    const PAGE_SIZE: usize;

    /// log2(PAGE_SIZE).
    const PAGE_SHIFT: usize;

    /// Number of significant bits in a physical address.
    /// RV64 Sv39: 56. RV64 Sv48: 56. LA64: 48 (PALEN).
    const PHYS_ADDR_BITS: u8;

    /// Number of significant bits in a virtual address.
    /// RV64 Sv39: 39. RV64 Sv48: 48. LA64: 48.
    const VIRT_ADDR_BITS: u8;

    /// Base virtual address of the kernel direct map.
    /// All physical RAM is mapped here at offset = ppn * PAGE_SIZE.
    const DIRECT_MAP_BASE: VirtAddr;

    /// Maximum size in bytes of the direct map.
    /// Substrate uses 1 GiB superpages; this is the upper bound on RAM.
    const DIRECT_MAP_SIZE: usize;

    /// Base virtual address of the kernel image (text/rodata/data).
    const KERNEL_VIRT_BASE: VirtAddr;

    /// Highest legal user virtual address + 1.
    const USER_TOP: VirtAddr;

    /// Bytes below USER_TOP reserved for fixed user-helper pages.
    /// Phase 1 leaves this range unmapped except for ordinary stack trampolines;
    /// later phases may place sigreturn/VDSO/trampoline pages here.
    const USER_RESERVED_TOP_SIZE: usize;

    /// Highest address ordinary mmap/brk/stack placement may allocate below.
    const USER_ALLOC_TOP: VirtAddr;

    /// Per-hart kernel stack size.
    const KERNEL_STACK_SIZE: usize;

    /// Per-hart kernel stack alignment (typically PAGE_SIZE).
    const KERNEL_STACK_ALIGN: usize;

    /// Number of page-table levels.
    /// RV64 Sv39: 3. RV64 Sv48: 4. LA64: 4 (typical).
    const PAGE_TABLE_LEVELS: u8;

    /// Number of bits in an address-space identifier.
    /// RV64: 16 (Sv39/Sv48). LA64: 10.
    const ASID_BITS: u8;

    /// Cache-line size in bytes.
    const CACHE_LINE_SIZE: usize;

    /// True if the platform has coherent DMA (cache-coherent IO).
    /// QEMU virt boards: true. VisionFive2: true (JH7110 is coherent).
    /// 2K1000LA: confirm in board crate.
    const DMA_COHERENT: bool;
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum Arch {
    Riscv64,
    LoongArch64,
}
```

### 4.1 Why associated constants, not pub const
<!-- txdoc:HAL-PLATFORMCONFIG-ASSOCIATED-CONSTANTS-WHY-ASSOCIATED-CONSTANTS-NOT-PUB-CONST-1 -->

A board crate is free to have private `pub const PLIC_BASE: usize = 0x0c00_0000;` for its own internal use. But anything cross-subsystem code needs goes through `PlatformConfig` so that subsystem code is monomorphized over `<P: PlatformConfig>` and references constants as `P::PAGE_SIZE`, `P::DIRECT_MAP_BASE`, etc.

This gives the substrate compile-time constants without coupling it to a specific board crate.

### 4.2 Address values and dereference boundary
<!-- txdoc:HAL-PLATFORMCONFIG-ASSOCIATED-CONSTANTS-ADDRESS-VALUES-AND-DEREFERENCE-BOUNDARY-1 -->

HAL owns the low-level address dialect: `PhysAddr`, `Ppn`, `VirtAddr`,
`VirtRange`, page-table indexes, bootstrap symbol crossings, and PTE encoding.
Those are address **values**, not dereference authority. A `VirtAddr` does not
become a Rust pointer by construction; it must pass through a named HAL,
substrate, or user-access conversion that states which mapping is being used.

The discipline:

- pmap, boot, user-access, and page-substrate code may speak typed address
  values because address layout is their job;
- ordinary kernel subsystems should speak semantic evidence (`Cap<T>`,
  `Weak<T>`, `IdentRef<'g, T>`, witnesses, reservations, role tokens), not raw
  physical or virtual addresses;
- no broad address arithmetic API (`Add<usize>` style) is part of the contract;
  arithmetic is exposed as checked helpers such as page alignment, page-index
  extraction, and named conversions (`phys -> direct-map`, `boot-linked ->
  kernel alias`);
- board code may name linker/static symbols only inside its boot-static capture
  surface; the rest of the board consumes typed methods that return physical
  facts, high kernel pointers, or direct-map pointers as appropriate. RV64 QEMU
  calls this surface `BootStaticBag`: the low `.text.trampoline` uses only
  `_load` linker symbols while translation is off, then high Rust constructs
  `BootStaticBag<IdentityLive>` exactly once with high VMA/static facts and the
  firmware DTB value. The bag is neither `Copy` nor `Clone`, so the typestate
  transition is the guard against accidentally using identity-era dereference
  authority after the DTB and boot statics have been published. The transferred
  bag may retain the firmware DTB as a raw provenance value, but only the live
  state exposes it for parsing.
  `cargo xtask lint arch` rejects `addr_of!`, raw
  `UnsafeCell::get() as usize`, and Rust linker-symbol extern blocks elsewhere
  in that board crate. `cargo xtask lint unused` runs Rust unused/dead-code
  checks as hard errors and the arch lint rejects `#[allow(dead_code)]` /
  `#[allow(unused...)]` escape hatches in normal code, so staged boot helpers
  must either be live or explicitly test-gated.

Once a conversion has produced a valid high-kernel `&T`, `&mut T`, `NonNull<T>`,
or raw pointer for a narrow unsafe operation, normal Rust pointer/reference
rules apply. User memory is the exception: user virtual addresses stay as
`UserPtr<T>`/`UserRange`-style values and are accessed only through
`UserAccessIf`/copyin/copyout paths.

### 4.3 What does *not* belong in `PlatformConfig`
<!-- txdoc:HAL-PLATFORMCONFIG-ASSOCIATED-CONSTANTS-WHAT-DOES-NOT-BELONG-IN-PLATFORMCONFIG-1 -->

- MMIO base addresses (those go through `PlatformInfoIf::platform_info()`).
- Interrupt numbers (those are board-specific and go through device init tables).
- Linker symbols (`_kernel_start`, `_bss_start`, etc. — those are platform-private, exposed through `BootInfoIf` if needed).
- Anything that varies between boots of the same board.

`PlatformConfig` is for invariant, compile-time facts only.

---

## 5. The boot sequence
<!-- txdoc:HAL-THE-BOOT-SEQUENCE-1 -->

Boot proceeds in five stages, named H0 through H4. HAL specifies H0 through H3; H4 is downstream subsystem init, named here only as the destination.

### 5.0 Portable boot contract for implementations
<!-- txdoc:HAL-THE-BOOT-SEQUENCE-PORTABLE-BOOT-CONTRACT-1 -->

Every platform implementation follows the same shape, regardless of firmware,
CPU architecture, or board:

```text
firmware/reset
 → platform crate _start
 → board binary rust_entry(cpu_id, firmware_arg)
 → tx_hal::entry::<P, K>
 → tx_kernel::kernel_main::<P>(BootHandoff)
```

The raw meaning of `cpu_id` and `firmware_arg` is platform-owned. A platform
implements `BootPlatformIf` to translate those raw values into:

```rust
pub enum BootProtocol {
    RiscvSbi,
    RiscvDirect,
    LoongArchFirmware,
}

pub struct BootHandoff {
    pub cpu_id: CpuId,
    pub firmware_arg: BootArg,
    pub protocol: BootProtocol,
}
```

`tx-kernel` consumes `BootHandoff` only as typed boot context. It must not know
that RV64 QEMU uses `a0 = hart_id, a1 = DTB`, that a direct RISC-V board may use
a fixed hardware table, or that LA64 firmware has a different register
contract. If a new platform needs more decoded facts, add them to `BootInfo`,
`PlatformInfo`, or a narrow HAL trait; do not add board-specific conditionals to
`tx-kernel`.

There are two readiness levels:

| Level | Purpose | Required before |
|---|---|---|
| **Smoke boot contract** | Prove the portable handoff shape works: `_start` sets a stack, preserves firmware registers, clears BSS, reaches `rust_entry`, exposes `BootHandoff`, makes `ConsoleIf::write_bytes` usable, and prints a board-derived serial sentinel. | Any platform is considered executable. |
| **Substrate-ready boot contract** | Add the full HAL deliverables: static `BootInfo`, bootstrap pmap/direct map, early PT-node pool, minimal trap vector, platform MMIO facts, and pmap mutation surface. | Real page substrate, heap, drivers, BusyBox, or OSComp images. |

Later platform implementations must pass the smoke contract first, then extend
the same path to the substrate-ready contract. They must not create a second
board-specific kernel main or move boot policy above the platform crate.

### 5.0A Smoke sentinel contract
<!-- txdoc:HAL-THE-BOOT-SEQUENCE-SMOKE-SENTINEL-CONTRACT-1 -->

The first executable proof for a platform is a serial sentinel:

```text
txkernel:<P::BOARD>:boot:ok
```

The sentinel is emitted by generic `tx_kernel::kernel_main::<P>` after
`P::init_early(handoff)`, `tx_substrate::init::<P>()`, and
`P::init_later(handoff)` return in the smoke build. It is intentionally derived
from `P::BOARD` so the generic kernel proves it is using the selected platform
axis rather than a hard-coded board string.

QEMU-capable platforms should expose the sentinel through:

```sh
cargo xtask qemu --target <target> --profile smoke --expect-sentinel
```

RV64 QEMU's first sentinel is:

```text
txkernel:qemu-riscv64-virt:boot:ok
```

BusyBox, filesystem, and OSComp tests are layered after this sentinel is stable.
They must not be used to debug the basic firmware-to-kernel handoff.

### 5.1 Stage H0 — firmware / reset handoff
<!-- txdoc:HAL-THE-BOOT-SEQUENCE-STAGE-H0-FIRMWARE-RESET-HANDOFF-1 -->

The platform crate receives control from firmware. The shape of "firmware" varies:

| Platform | Pre-kernel firmware | Entry handoff |
|---|---|---|
| RV64 qemu-virt | OpenSBI (M-mode) | jumps to S-mode at `0x8020_0000` with `a0 = hart_id`, `a1 = DTB pointer` |
| RV64 VisionFive2 | U-Boot SPL → OpenSBI | same handoff convention; DTB loaded by U-Boot |
| LA64 qemu-virt | UEFI or direct kernel load | jumps to entry with `a0 = boot_arg`, `a1 = info_ptr` |
| LA64 2K1000LA | board firmware (PMON or UEFI) | board-specific; platform crate documents |

H0 ends when control reaches the platform's `__start` symbol with the architecture's normal kernel-entry register conventions.

H0 obligations on the platform crate:

- Define `__start` as the entry symbol named in the linker script.
- Document the firmware contract (which registers carry what).
- Implement `BootPlatformIf::BOOT_PROTOCOL` and, if needed, override
  `boot_handoff(cpu_id, firmware_arg)` to normalize the platform's raw
  registers into `BootHandoff`.
- Park or not-yet-start secondary CPUs. The BSP is the only hart that runs the
  generic H0-H3 path; its firmware hart id is not assumed to be zero.

### 5.2 Stage H1 — pre-Rust bootstrap
<!-- txdoc:HAL-THE-BOOT-SEQUENCE-STAGE-H1-PRE-RUST-BOOTSTRAP-1 -->

H1 runs in the platform crate's `__start` and any helper assembly it calls. It establishes the minimum environment for Rust code to run safely.

H1 obligations, in order:

1. **Set the BSP stack pointer** to a temporary per-hart stack slot of at least
   one page in `.bss.stack` or an equivalent platform-owned boot-stack section.
   The slot must be selected from the firmware CPU id when the platform can
   boot on a nonzero hart. The temporary stack is sufficient through H1 and H2;
   steady per-hart kernel stacks are installed later (in `init_early` or during
   AP bring-up).

2. **Clear the BSS section.** Linker symbols `_bss_start` and `_bss_end` bound the region.

3. **Preserve firmware handoff registers.** The raw CPU id and firmware argument must survive stack setup, BSS clearing, and any optional MMU/trap setup, then be passed unchanged to `rust_entry(cpu_id, firmware_arg)`.

4. **Install the bootstrap page table.** Build a minimal page table that maps:
   - The kernel image (text/rodata/data) at `KERNEL_VIRT_BASE`.
   - The first 1 GiB of physical RAM at `DIRECT_MAP_BASE` via a single L2 superpage.
   - The early-UART MMIO region at a known kernel virtual address.

   Activate the page table: `satp` write + `sfence.vma` on RV64; DMW + `csrwr` + `invtlb` on LA64. After this point, the kernel is running with MMU on.

   During the low-to-high transition, a platform may also keep a temporary
   identity leaf for the boot RAM window. That identity bridge is only for the
   bootstrap smoke path. The stack transition is a per-hart stack alias
   transition: if execution moves from identity to high-half addresses, the BSP
   rewrites `sp` to the high/direct-map alias of the same per-hart stack
   storage. This does not introduce per-thread kernel stacks; tasks remain
   stackless futures polled on the current hart's stack.

   The portable interface for this handoff is a board-private boot-static
   authority advanced by typestate. The authority owns all boot/static address
   facts and exposes two named pipelines:

   - a pre-entry pipeline that runs before the architecture activation jump:
     capture firmware/static facts, build bootstrap mappings, publish pmap and
     high-entry transition facts, and return the architecture activation value
     to assembly;
   - a post-entry pipeline that runs after the high/translated entry point:
     publish `BootInfo` while any required firmware/identity access remains
     valid, prove the new execution context, and install the steady-state
     authority. A board may remove temporary low mappings here only if its
     linked image and compiler-generated tables no longer depend on them.

   RV64 QEMU implements that interface with an assembly-only low
   `.text.trampoline` plus its board-private `BootStaticBag<IdentityLive>`.
   `_start` uses `_load` symbols to clear BSS, build the identity/direct-map/
   high-kernel bootstrap tables, install `satp`, rewrite `sp`/`gp`, and jump
   to the high VMA `rust_entry`. High Rust then constructs the bag exactly
   once, captures canonical high linker symbols as physical facts, publishes
   the bootstrap pmap facts, parses the DTB into static `BootInfo`, verifies
   high `pc`/`sp`/`gp`, clears the low identity leaf, and consumes the live bag
   into the steady handoff typestate.

5. **Install a minimal trap vector.** The vector must be sufficient to catch a panic and dump it through the early UART. Full trap discipline is established later in H3 by `TrapIf::install_kernel_trap_vector`.

6. **Initialize the early console** to the point where `ConsoleIf::write_bytes` can deliver characters. RV64 QEMU may use SBI console for the smoke contract; boards with direct UARTs may use MMIO once mapped. This unlocks `printk!` for any panic during the rest of boot.

7. **Construct the BootInfo skeleton.** Parse the firmware-handed pointer (DTB on RV64, info_ptr on LA64) and populate the static `BootInfo` with `memory_regions`, `kernel_image` linker bounds, `initrd` (if present), and `cmdline`. The skeleton uses `'static` references only — see §7 for the discipline.

8. **Jump to `rust_entry`.** This is the no-mangle Rust function in the board binary, defined in §2.3.

For the smoke boot contract, steps 1, 2, 3, 6, and 8 are mandatory. Steps 4,
5, and 7 become mandatory before the platform claims substrate-ready status or
runs BusyBox/OSComp images. H1 is intentionally restricted to what assembly and
minimal `core` Rust can do. No allocation. No traits called. The platform crate
may write H1 in pure assembly or in `unsafe` Rust against arch primitives; that
is a platform-internal choice.

### 5.3 Stage H2 — `tx_hal::entry::<P, K>(cpu_id, arg)`
<!-- txdoc:HAL-THE-BOOT-SEQUENCE-STAGE-H2-TX-HAL-ENTRY-P-K-CPU-ID-ARG-1 -->

H2 is the cross-platform shell, executed by every board. It lives in the trait crate.

The shell is parameterized over both the platform `P` and a kernel continuation type `K`. This avoids a `tx-hal → tx-kernel` dependency: the trait crate does not name `tx_kernel::kernel_main` directly. Instead, the board binary supplies a type that implements `KernelMain<P>`:

```rust
// tx-hal/src/entry.rs

pub trait KernelMain<P: TxPlatform> {
    /// The kernel's main entry. Diverges; never returns to entry().
    fn kernel_main(handoff: BootHandoff) -> !;
}

pub fn entry<P, K>(cpu_id: usize, firmware_arg: usize) -> !
where
    P: TxPlatform,
    K: KernelMain<P>,
{
    P::install_minimal_trap_vector();
    let handoff = P::boot_handoff(cpu_id, firmware_arg);

    // 1. Finalize BootInfo. The platform's __start built a skeleton;
    //    this is where any cross-platform validation happens
    //    (memory regions sane, kernel_image bounds in range, etc.).
    let _bi = P::boot_info();
    debug_assert!(!_bi.memory_regions.is_empty());

    // 2. Install early per-CPU pointer. This makes per-cpu reads
    //    legal everywhere from this point on.
    P::install_early_percpu(handoff.cpu_id);
    P::mark_cpu_online(handoff.cpu_id);

    // 3. Hand off to the kernel continuation.
    K::kernel_main(handoff)
}
```

H2 is deliberately thin. Anything specific to an arch (DMW activation, satp, page-table walking) was already done in H1. Anything specific to a subsystem (frame allocator, slab, VFS) is in H3.

H2's job is to be the place where cross-platform boot invariants are checked and the per-cpu pointer becomes legal. After H2, the kernel can assume `PercpuIf::current_cpu_id()` works.

### 5.4 Stage H3 — generic kernel mainline
<!-- txdoc:HAL-THE-BOOT-SEQUENCE-STAGE-H3-GENERIC-KERNEL-MAINLINE-1 -->

H3 is `tx_kernel::kernel_main::<P>`, the same code on every board. Its skeleton:

```rust
// tx-kernel/src/lib.rs
pub fn kernel_main<P: TxPlatform>(handoff: BootHandoff) -> ! {
    // 1. P::init_early — platform finishes its own setup
    //    (timer base, PLIC/ExtIOI, anything that needs printk live).
    P::init_early(handoff);

    // 2. Substrate: zones, frame allocator, slab, FrameMeta.
    //    Heap becomes available at the end of this call.
    substrate::init::<P>();

    // 3. P::init_later — platform brings up anything that needed
    //    the heap (e.g., extending the direct map past 1 GiB,
    //    discovering additional MMIO from PlatformInfo).
    P::init_later(handoff);

    // 4. Install full trap vectors. The minimal H1 vector is replaced
    //    by the full kernel trap path that dispatches to KernelTrapSink.
    P::install_kernel_trap_vector();

    // 5. Start APs after substrate and the full kernel trap vector are live.
    //    Early APs install per-cpu state, run secondary init hooks, initialize
    //    substrate-local epoch/zone state, install their trap vector, mark
    //    themselves online, then park until SMP_v1 assigns scheduler work.
    P::boot_secondary_cpus(secondary_cpu_entry::<P>);

    // 6. Reactor and scheduler init. Reactor needs heap (for queues),
    //    needs HAL (for timer) — heap is up, HAL is up.
    reactor::init::<P>();
    scheduler::init::<P>();

    // 7. Downstream subsystems. See H4.
    vfs::init::<P>();
    device::init::<P>();
    process::init::<P>();
    // ... etc.

    // 8. exec the init userspace. Does not return.
    exec::init_userspace::<P>()
}
```

The contract: `kernel_main` does not return. If `exec::init_userspace` somehow returns, `kernel_main` calls `P::system_off()`.

### 5.5 Stage H4 — downstream subsystem init
<!-- txdoc:HAL-THE-BOOT-SEQUENCE-STAGE-H4-DOWNSTREAM-SUBSYSTEM-INIT-1 -->

H4 is everything from `vfs::init` onward. It is named here only as the destination of H3. Each subsystem has its own initialization spec:

- VFS — see [`05_filesystem/`](../05_filesystem/).
- Device — see [`DEVICE.md §7`](../06_devices/DEVICE.md). DEVICE.md's phase-table 1–7 corresponds to this section.
- Process — see [`PROCESS_v1.md`](../04_process-signals/PROCESS_v1.md).

HAL has no further obligations after H3 except to keep its trait methods callable.

---

## 6. InitIf
<!-- txdoc:HAL-INITIF-1 -->

`InitIf` is the platform's two-stage init pair, plus secondary-core counterparts. It is the primary entry surface from the kernel mainline back into platform-specific work that needs to happen at a known point in boot.

```rust
pub trait InitIf {
    /// Called from kernel_main after H2, before substrate::init.
    /// At this point: heap is NOT available; early console IS available;
    /// per-cpu pointer IS installed; trap vectors are minimal.
    /// Platform should: finalize timer base, configure interrupt
    /// controller for masking (no IRQs taken yet), set up sscratch/$r21
    /// trap-scratch areas if not already done in H1.
    fn init_early(handoff: BootHandoff);

    /// Called from kernel_main after substrate::init returns.
    /// At this point: heap IS available; direct map covers all RAM;
    /// frame allocator is live; FrameMeta exists.
    /// Platform should: extend the kernel page table to cover any
    /// platform MMIO regions discovered in PlatformInfo, finalize
    /// any platform state that needed heap allocation.
    fn init_later(handoff: BootHandoff);

    /// Called once on each AP after its low trampoline and early per-CPU
    /// register setup complete. Single-CPU platform impls may keep the
    /// default no-op.
    fn init_early_secondary(cpu_id: CpuId) {}

    /// Called once on each AP after substrate is up on the BSP.
    /// Single-CPU platform impls may keep the default no-op.
    fn init_later_secondary(cpu_id: CpuId) {}
}
```

### 6.1 What `init_early` may not do
<!-- txdoc:HAL-INITIF-WHAT-INIT-EARLY-MAY-NOT-DO-1 -->

- Allocate. The slab is not up.
- Take traps. Trap delivery is masked until `kernel_main` installs the full trap vector after `init_later`.
- Wait. There is no scheduler yet.

### 6.2 What `init_later` may do but `init_early` cannot
<!-- txdoc:HAL-INITIF-WHAT-INIT-LATER-MAY-DO-BUT-INIT-EARLY-CANNOT-1 -->

- Call `Box::new`, `Vec::new`, etc. The slab is up.
- Map additional MMIO regions through `PmapIf` (see §10).
- Read `PlatformInfo.mmio_regions` to discover MMIO that needs mapping.

---

## 7. BootInfoIf
<!-- txdoc:HAL-BOOTINFOIF-1 -->

`BootInfoIf` is the boot-time fact channel: what the firmware told us about RAM, kernel image, initrd, and command line. Substrate phase 1 reads it.

```rust
pub trait BootInfoIf {
    /// Returns a 'static reference to the platform's BootInfo.
    /// The reference is stable for the entire kernel lifetime.
    fn boot_info() -> &'static BootInfo;
}

pub struct BootInfo {
    /// Usable physical memory regions, sorted by base address,
    /// non-overlapping. Reserved regions (kernel image, initrd,
    /// FrameMeta, PT_NODE_POOL) are still listed here as Reserved
    /// kind so substrate can subtract them.
    pub memory_regions: &'static [MemoryRegion],

    /// Kernel image bounds. Used by substrate to mark these PPNs
    /// reserved.
    pub kernel_image: PhysRange,

    /// initrd, if present. Used by VFS to mount the initial root
    /// filesystem. None if no initrd was supplied.
    pub initrd: Option<PhysRange>,

    /// Kernel command line, if present.
    pub cmdline: Option<&'static str>,
}

pub struct MemoryRegion {
    pub base: PhysAddr,
    pub size: usize,
    pub kind: MemoryRegionKind,
}

pub enum MemoryRegionKind {
    /// Normal RAM. Frame allocator may use these PPNs.
    Usable,
    /// Reserved (firmware, MMIO, kernel image, initrd, etc.).
    /// Frame allocator must mark these PPNs as never-free.
    Reserved,
}
```

### 7.1 The 'static-ref discipline
<!-- txdoc:HAL-BOOTINFOIF-THE-STATIC-REF-DISCIPLINE-1 -->

Every field of `BootInfo` and every `MemoryRegion` slice element is a `'static` reference into `.bootinfo` or `.rodata`. This is how the platform avoids needing the heap before substrate runs.

The platform crate sets this up by:

1. Allocating a static buffer in `.boot.bootinfo` large enough for the maximum number of memory regions the board could report.
2. In H1, parsing the firmware-handed pointer (DTB on RV64, info_ptr on LA64) and populating the static buffer.
3. Storing the slice as `&'static [MemoryRegion]` derived from the static buffer.

No `Vec`. No `String`. The slab is not up; allocation is not legal.

RV64 QEMU centralizes these boot-owned statics and linker-symbol crossings in
its board-private `BootStaticBag`. The low trampoline names only suffixed
`_load` symbols. The bag is constructed once after the high VMA entry, carries
the DTB value while identity-era dereference authority is live, and advances by
typestate after BootInfo and boot pmap facts have been published. The
transferred bag keeps the DTB only as a value fact; parsing authority exists
only on `BootStaticBag<IdentityLive>`. `BootInfoIf`, `PlatformInfoIf`, and
bootstrap pmap code consume the bag's semantic accessors instead of deriving
addresses from raw static pointers locally.

Boards that need a low-to-high or firmware-to-kernel transition should expose
the same conceptual pipeline even if their concrete bag type differs:

```text
Captured/IdentityLive
  -> pre-entry pipeline
  -> architecture activation / jump boundary
  -> post-entry pipeline
  -> HandoffReady
```

The HAL-level contract is the phase discipline and the published facts, not a
shared storage layout. RV64 QEMU keeps `BootStaticBag` private; another board
may use different fields or no temporary identity bridge, but it must still
make the authority transition explicit before generic kernel code runs.

### 7.2 Mutability
<!-- txdoc:HAL-BOOTINFOIF-MUTABILITY-1 -->

`BootInfo` is read-only after H1. `BootInfoIf::boot_info()` always returns the same reference. Any platform crate that wants to mutate it after H1 has a bug.

### 7.3 Why this is in HAL and not the kernel
<!-- txdoc:HAL-BOOTINFOIF-WHY-THIS-IS-IN-HAL-AND-NOT-THE-KERNEL-1 -->

The format of the firmware-handed data (DTB on RV64, possibly different on LA64) is platform-specific. Parsing it is platform-specific. `BootInfo` is the cross-platform interface that emerges from parsing. Substrate consumes `BootInfo`, never the raw firmware data.

---

## 8. PlatformInfoIf
<!-- txdoc:HAL-PLATFORMINFOIF-1 -->

`PlatformInfoIf` is the second boot-time fact channel: MMIO regions the platform wants the kernel to know about. Distinct from `BootInfo` because:

- `BootInfo` describes RAM and kernel image — facts substrate needs in phase 1.
- `PlatformInfo` describes MMIO — facts substrate needs in phase 3 (extend kernel page table to cover MMIO) and tier-2 device init needs.

```rust
pub trait PlatformInfoIf {
    fn platform_info() -> &'static PlatformInfo;
}

pub struct PlatformInfo {
    pub board: &'static str;
    pub spi_sd: Option<SpiSdInfo>;

    /// MMIO regions that should be mapped into the kernel page table.
    /// Includes the early UART, interrupt controller, timer, and any
    /// platform devices the board has at fixed addresses (e.g.,
    /// virtio-mmio range on qemu-virt).
    pub mmio_regions: &'static [MmioRegion],
    pub timebase_frequency_hz: u64,
    pub possible_cpu_count: usize,
}

pub struct MmioRegion {
    pub name: &'static str,           // diagnostic only
    pub phys: PhysRange,
    pub virt: VirtRange,              // where to map in kernel space
    pub flags: MmioFlags,
}

pub struct MmioFlags(pub u32);

impl MmioFlags {
    pub const DEVICE_NGNRNE: Self = Self(1 << 0); // strongly-ordered device memory
    pub const DEVICE_NGNRE: Self = Self(1 << 1);  // device memory, gathering allowed
    pub const READ: Self = Self(1 << 2);
    pub const WRITE: Self = Self(1 << 3);
}
```

### 8.1 Why MMIO does not go directly to drivers
<!-- txdoc:HAL-PLATFORMINFOIF-WHY-MMIO-DOES-NOT-GO-DIRECTLY-TO-DRIVERS-1 -->

A driver (UART, virtio-blk, PLIC) does not import `tx_hal_riscv64_qemu_virt::UART_BASE`. It receives its MMIO base through device init, which read `P::platform_info().mmio_regions` and looks up by name. This keeps device code portable across boards even when the same driver runs on different boards with different bases.

The exception is tier-1 HAL devices (PLIC, CLINT, early UART). These live *inside* the platform crate itself; their bases are platform-private. For these, the platform crate's `IrqIf`, `TimeIf`, and `ConsoleIf` impls reach for the constants directly — but no other crate sees those constants.

### 8.2 PlatformInfo and substrate phase 3
<!-- txdoc:HAL-PLATFORMINFOIF-PLATFORMINFO-AND-SUBSTRATE-PHASE-3-1 -->

PAGE_SUBSTRATE phase 3 walks `mmio_regions` and installs page-table mappings via `PmapIf`. This is why `MmioRegion.virt` exists: the platform decides where in kernel virtual space each MMIO region lives, and substrate installs the mapping there.

### 8.3 Auxv facts
<!-- txdoc:HAL-PLATFORMINFOIF-AUXV-FACTS-1 -->

`AuxvIf` is the arch/platform fact channel consumed by exec when building the userspace auxiliary vector. It follows the axHal-style rule: facts come from the statically selected platform, not from a runtime HAL manager or an architecture match in exec.

```rust
pub trait AuxvIf: PlatformConfig {
    fn arch_auxv_facts() -> ArchAuxvFacts;
}

pub struct ArchAuxvFacts {
    pub page_size: usize,
    pub hwcap: u64,
    pub hwcap2: u64,
    pub platform: &'static str,
}
```

Expected v1 values:

| Field | RV64 | LA64 |
|---|---|---|
| `page_size` | `P::PAGE_SIZE` | `P::PAGE_SIZE` |
| `hwcap` | platform/arch HWCAP bits | platform/arch HWCAP bits |
| `hwcap2` | `0` unless the ABI says otherwise | platform/arch HWCAP2 bits |
| `platform` | `"riscv64"` | `"loongarch64"` |

---

## 9. ConsoleIf
<!-- txdoc:HAL-CONSOLEIF-1 -->

`ConsoleIf` is the panic-time and `printk!`-time character output. It is the simplest HAL trait and the first one made live (H1).

```rust
pub trait ConsoleIf {
    /// Write bytes to the early console synchronously.
    /// May busy-loop on UART TX-ready. Must work even when interrupts
    /// are disabled and the heap is unavailable.
    fn write_bytes(bytes: &[u8]);

    /// Read bytes from the early console, non-blocking.
    /// Returns the number of bytes read; 0 if no input is available.
    /// May be a no-op until init_later for boards where the UART RX
    /// path needs heap-allocated buffering.
    fn read_bytes(buf: &mut [u8]) -> usize;
}
```

### 9.1 Tier-2 wrapping
<!-- txdoc:HAL-CONSOLEIF-TIER-2-WRAPPING-1 -->

After substrate is up, the early UART is wrapped by a tier-2 `CharDeviceBinding` (see [`DEVICE.md §2.1`](../06_devices/DEVICE.md)). Userspace opens `/dev/console` and reaches the same UART through the standard char-device path.

The tier-1 `ConsoleIf` path remains as a panic-time fallback. It is reachable from a panic handler that cannot rely on devfs being live (e.g., panic during devfs init).

### 9.2 Concurrency
<!-- txdoc:HAL-CONSOLEIF-CONCURRENCY-1 -->

`ConsoleIf::write_bytes` is callable from any context. It does not take a lock that participates in any priority-inversion class. The platform crate may use a low-level spin-lock to serialize multiple harts writing to the same UART, but that lock must not be acquired from any trap path or interrupt path.

---

## 10. PmapIf — pmap materialization proof objects
<!-- txdoc:HAL-PMAPIF-PMAP-MATERIALIZATION-PROOF-OBJECTS-1 -->

`PmapIf` is one of the two load-bearing traits of HAL. It is what makes txKernel's HAL different from a unikernel HAL. The substrate, the VM subsystem, and any code that touches kernel page tables consumes `PmapIf` to publish or withdraw page-table state.

The discipline: pmap mutations follow a prepare/commit/shootdown pattern. The kernel reserves slots fallibly, commits values infallibly, and tears down with mandatory shootdown coordination.

### 10.1 Trait surface
<!-- txdoc:HAL-PMAPIF-PMAP-MATERIALIZATION-PROOF-OBJECTS-TRAIT-SURFACE-1 -->

The implementation surface is staged. The first executable boot slice exposes
only the bootstrap pmap facts and PT-node pool hooks needed before the page
substrate exists:

```rust
pub struct BootstrapPmapInfo {
    pub root: PhysAddr,
    pub mapped: PhysRange,
    pub direct_map_base: VirtAddr,
    pub direct_map: VirtRange,
    pub kernel_image: VirtRange,
    pub identity: Option<VirtRange>,
    pub pt_node_pool: PhysRange,
    pub reserved_page_tables: &'static [PhysRange],
}

pub trait PmapIf {
    fn bootstrap_pmap_info() -> Option<&'static BootstrapPmapInfo>;
    fn alloc_pt_node() -> Result<PtNode, AllocError>;
    fn free_pt_node(node: PtNode);
    fn install_pt_node_allocator(allocator: PtNodeAllocator) -> Result<(), PmapError>;
    fn reserve_kernel_direct_map_1g(phys: PhysAddr)
        -> Result<Option<PmapReservation>, PmapError>;
    fn commit_kernel_direct_map_1g(reservation: PmapReservation);
    fn extend_direct_map(phys_end: PhysAddr) -> Result<(), PmapError>;
    fn reserve_kernel_mapping(
        virt: VirtAddr,
        phys: PhysAddr,
        kind: PmapReserveKind,
    ) -> Result<Option<PmapReservation>, PmapError>;
    fn rollback_kernel_mapping(reservation: PmapReservation);
    fn commit_kernel_mapping(reservation: PmapReservation);
    fn unmap_kernel_mapping(
        virt: VirtAddr,
        kind: PmapReserveKind,
    ) -> Result<Option<PmapUnmapResult>, PmapError>;
    fn protect_kernel_mapping(
        virt: VirtAddr,
        kind: PmapReserveKind,
        permissions: PmapPermissions,
    ) -> Result<Option<PmapInvalidation>, PmapError>;
    fn shootdown_kernel_mapping(invalidation: PmapInvalidation);
}
```

For RV64 QEMU's current bootstrap pmap, the kernel is linked at high VMA
`0xffff_ffff_8020_0000` with low load addresses beginning at `0x8020_0000`.
Firmware enters the low `.text.trampoline`, which keeps a temporary Sv39 1 GiB
identity leaf for the QEMU RAM window at `0x8000_0000`. It also publishes a
high direct-map alias at
`0xffff_ffc0_0000_0000 + phys` and a high kernel alias starting at
`0xffff_ffff_8020_0000`. The kernel alias uses board-owned 4 KiB leaf tables so
text is RX, rodata is R, data/bss/boot-stack pages are RW, and the remaining
reserved high-alias window is left unmapped. This is the low-to-high transition
slice: it proves the aliases, rewrites `sp`/`gp`, enters Rust through the high
alias, validates high `pc`/`sp`/`gp`, then clears the low identity bridge. The
`reserved_page_tables` slice names every bootstrap page-table page/range that
substrate must subtract from allocator-free RAM, including the root,
kernel-alias L1, kernel-alias L0 table range, and PT-node pool on RV64 QEMU.
RV64 QEMU also publishes the OpenSBI/kernel-loader gap below `0x8020_0000` as
reserved RAM so page-substrate metadata is not carved over firmware-owned
pages.
The executable pmap mutation surface is still intentionally narrow:
`extend_direct_map()` uses idempotent 1 GiB direct-map leaf reservations and
commits before the allocator is installed, while `reserve_kernel_mapping()` /
`commit_kernel_mapping()` cover boot-time kernel mappings at 2 MiB or 4 KiB
granularity for platform MMIO.
Abandoned 2 MiB / 4 KiB reservations can be rolled back, releasing any
`PT_NODE_POOL` intermediates allocated while reserving. Kernel 2 MiB / 4 KiB
mappings can also be unmapped into a `PmapUnmapResult`, whose invalidation is
then passed to `shootdown_kernel_mapping()`; RV64 QEMU now does the local
`sfence.vma` and, once APs are online, uses SBI RFENCE to issue matching remote
`sfence.vma` calls on the other harts. Safe same-granularity kernel
permission changes use `protect_kernel_mapping()` to update an existing leaf in
place and return an invalidation; absent mappings are left alone for the later
VM/fault path, and unsafe cases such as split-required superpages return
`InvalidRequest`. After `tx_substrate::init()` installs
the frame allocator, it calls `install_pt_node_allocator()` so new pmap
intermediates come from typed page-table frames first, with `PT_NODE_POOL`
retained as an exhaustion fallback.

The current process-root subset exposes `PmapRoot` and `Asid` as concrete
shared HAL types. RV64 QEMU allocates a fresh root page, copies the kernel
high-half entries from the bootstrap root, assigns an ASID from a fixed bitmap,
and tears user-half page-table trees down through the same committed `PtNode`
registry used by kernel pmap unmap. Single mapping reserve/commit, unmap, and
safe in-place protect are implemented for VM-owned roots; unsafe cases still
return `InvalidRequest` so VM can keep the recipe authoritative and fault or
rematerialize later. `tx_hal::pmap` provides the generic no-alloc page-range
session API over those single mapping operations; the board still owns the
actual PTE walk and mutation. Substrate has an ASID-scoped page shootdown batch
that holds `MapPin`s until after `PmapIf::shootdown_mapping()`.
Superpage/multi-frame accounting remains before userspace work; the current
remote shootdown implementation is RV64 QEMU SBI RFENCE rather than a full
kernel-managed IPI/ack protocol.

The full planned trait surface is:

```rust
pub trait PmapIf: PlatformConfig {
    /// Architecture-specific page-table entry type.
    /// Encodes PPN, permissions, attributes, valid bit.
    type Pte: Copy + Eq;

    /// Architecture-specific page-table root.
    /// Owns the top-level page-table page and any per-AS state
    /// including ASID assignment.
    type PmapRoot;

    /// Address-space identifier carried in shootdown batches so that
    /// per-ASID TLB invalidations can be issued instead of global ones.
    /// RV64: u16. LA64: u16 (10-bit value).
    type Asid: Copy + Eq;

    /// The kernel's page-table root. Used for kernel-half mutations
    /// (extending the direct map, mapping new MMIO regions). Has a
    /// reserved ASID (typically 0, treated as "global").
    fn kernel_pmap_root() -> &'static Self::PmapRoot;

    /// Read the ASID from a pmap root.
    fn root_asid(root: &Self::PmapRoot) -> Self::Asid;

    /// Allocate one intermediate page-table page from PT_NODE_POOL
    /// (pre-substrate) or from the frame allocator (post-substrate
    /// phase 7).
    fn alloc_pt_node() -> Result<PtNode, AllocError>;

    /// Install the post-frame-allocator PT-node source. Platforms retain
    /// PT_NODE_POOL as a fallback when the installed source is exhausted.
    fn install_pt_node_allocator(allocator: PtNodeAllocator) -> Result<(), PmapError>;

    /// Free an intermediate page-table page.
    fn free_pt_node(node: PtNode);

    /// Reserve a PTE slot fallibly. May allocate intermediate pages
    /// from alloc_pt_node. On failure, no state has changed.
    fn reserve(
        root: &Self::PmapRoot,
        va: VirtAddr,
        kind: PmapReserveKind,
    ) -> Result<PmapReservation<Self>, PmapError>;

    /// Commit a batch of reservations. Infallible: every reservation
    /// in the batch is guaranteed to install successfully.
    fn commit(batch: PmapCommitBatch<Self>) -> PmapCommitResult;

    /// Tear down mappings in the given ranges. Returns the cleared
    /// PPNs and a corresponding invalidation list, ASID-tagged so a
    /// later shootdown can be ASID-scoped.
    fn unmap(
        root: &Self::PmapRoot,
        ranges: &[VirtAddrRange],
    ) -> PmapUnmapResult<Self>;

    /// Issue a TLB shootdown for the given ASID-tagged invalidations.
    /// Infallible. EXC-2 (INVARIANTS §7): synchronous coordination
    /// with other harts; implemented by the platform via send_ipi
    /// + wait on SMP, or sfence.vma / invtlb on uniprocessor.
    fn shootdown(asid: Self::Asid, invalidations: &[VirtAddrRange]);

    /// Shootdown for kernel-half mappings, which are global on most
    /// architectures and need cross-ASID invalidation. Separate
    /// because RV64 sfence.vma with x0 rs2 invalidates global, and
    /// LA64 invtlb has a distinct opcode for global invalidation.
    fn shootdown_global(invalidations: &[VirtAddrRange]);
}

pub enum PmapReserveKind {
    /// Reserve a slot for one 4 KiB page.
    Page,
    /// Reserve a slot for a 2 MiB superpage (L1 leaf).
    Superpage2M,
    /// Reserve a slot for a 1 GiB superpage (L2 leaf).
    Superpage1G,
}

pub struct PtNode {
    ppn: Ppn,
    _phantom: PhantomData<*const ()>, // not Send
}
```

### 10.2 The reservation proof object
<!-- txdoc:HAL-PMAPIF-PMAP-MATERIALIZATION-PROOF-OBJECTS-THE-RESERVATION-PROOF-OBJECT-1 -->

`PmapReservation<P>` is a linear (non-`Copy`, `must_use`) proof that:

- A PTE slot at `va` exists in the page table (intermediate pages are allocated).
- The slot is currently empty.
- The intermediate-page allocations have been performed.

```rust
#[must_use]
pub struct PmapReservation<P: PmapIf + ?Sized> {
    va: VirtAddr,
    kind: PmapReserveKind,
    // intermediate pages allocated on the way down
    pt_nodes_allocated: SmallVec<[PtNode; 3]>,
    _phantom: PhantomData<P>,
}

impl<P: PmapIf + ?Sized> Drop for PmapReservation<P> {
    fn drop(&mut self) {
        // Frees pt_nodes_allocated (returns to PT_NODE_POOL or
        // frame allocator depending on bring-up phase).
        // The slot itself was never modified, so there is nothing
        // to undo at the page-table level.
    }
}
```

If the reservation drops without being committed, the intermediate pages it allocated are returned to their source. This matches the substrate's `FrameReservation` Drop discipline (PAGE_SUBSTRATE §5.2).

### 10.3 The commit batch
<!-- txdoc:HAL-PMAPIF-PMAP-MATERIALIZATION-PROOF-OBJECTS-THE-COMMIT-BATCH-1 -->

`PmapCommitBatch<P>` aggregates committed reservations + their PTE values. `commit` is infallible: by the time you have the batch, every slot is reserved, every intermediate page is allocated, and the only remaining work is to write PTE values into pre-existing slots.

```rust
pub struct PmapCommitBatch<P: PmapIf + ?Sized> {
    entries: SmallVec<[CommitEntry<P>; 4]>,
}

struct CommitEntry<P: PmapIf + ?Sized> {
    reservation: PmapReservation<P>,
    pte_value: P::Pte,
}

impl<P: PmapIf + ?Sized> PmapCommitBatch<P> {
    pub fn new() -> Self {
        Self { entries: SmallVec::new() }
    }

    pub fn install_pte(
        &mut self,
        reservation: PmapReservation<P>,
        pte_value: P::Pte,
    ) {
        self.entries.push(CommitEntry { reservation, pte_value });
    }

    pub fn commit(self) -> PmapCommitResult {
        P::commit(self)
    }
}

pub struct PmapCommitResult {
    pub installed_count: usize,
}
```

### 10.4 Unmap and shootdown
<!-- txdoc:HAL-PMAPIF-PMAP-MATERIALIZATION-PROOF-OBJECTS-UNMAP-AND-SHOOTDOWN-1 -->

Tearing down mappings is fundamentally different from installing them. The PTE write itself is fast; the slow and tricky part is TLB invalidation across harts. PAGE_SUBSTRATE §7.2 captured the critical ordering: **map_count must not be decremented before shootdown completes**, otherwise a stale TLB entry on another core could point at a freed-and-reallocated frame.

HAL's surface stops at issuing the shootdown. The map_count bookkeeping belongs to substrate (it owns FrameMeta), so the `ShootdownBatch` aggregator in PAGE_SUBSTRATE §7.2 wraps HAL's `shootdown` call with its own pre/post discipline. Per decision §1.8, HAL does not call into substrate.

The HAL-side return type from `unmap`:

```rust
pub struct PmapUnmapResult<P: PmapIf + ?Sized> {
    /// Cleared PPNs, in the order their PTEs were cleared.
    /// Caller (substrate) is responsible for decrement_map_count
    /// after shootdown completes.
    pub cleared_pte_ppns: SmallVec<[Ppn; 8]>,

    /// Virtual address ranges that need TLB invalidation.
    pub invalidations: SmallVec<[VirtAddrRange; 8]>,

    /// ASID to scope the shootdown to. Use root_asid(root) at unmap
    /// time; carried separately so the substrate-side aggregator
    /// can batch unmaps from multiple address spaces.
    pub asid: P::Asid,

    _phantom: PhantomData<P>,
}
```

The substrate-side aggregator (`ShootdownBatch` in PAGE_SUBSTRATE §7.2) consumes `PmapUnmapResult`, accumulates invalidations, calls `P::shootdown(asid, &invalidations)` exactly once, then walks the cleared PPNs to call `substrate::frame::decrement_map_count`. This keeps the dependency direction substrate → HAL only.

For kernel-half mappings (extending the direct map, mapping new MMIO regions), the substrate uses `kernel_pmap_root()` and the result's `asid` is the kernel ASID; the substrate calls `P::shootdown_global(&invalidations)` instead of `P::shootdown` to get cross-ASID invalidation.

The current executable kernel-only subset is smaller: `PmapUnmapResult` names
one cleared mapping and its `PmapInvalidation`, while
`KernelShootdownBatch` in PAGE_SUBSTRATE §7.2 owns the matching `MapPin`.
It calls `P::shootdown_kernel_mapping()` first and drops the `MapPin` only
afterward. On RV64 QEMU, that HAL call includes local `sfence.vma` plus SBI
remote RFENCE for online remote harts. That gives the same map-count ordering
without requiring HAL to know about `FrameMeta`.

### 10.5 The direct-map invariant
<!-- txdoc:HAL-PMAPIF-PMAP-MATERIALIZATION-PROOF-OBJECTS-THE-DIRECT-MAP-INVARIANT-1 -->

`PlatformConfig::DIRECT_MAP_BASE` mapping is not torn down via `PmapIf::unmap`. The direct map is established once during H1 (1 GiB superpage covering the first 1 GiB of RAM) and extended in substrate phase 2 (additional 1 GiB superpages to cover all RAM). It persists for the kernel lifetime.

Per-process address spaces (`AddressSpace` in VM_v1_2) all map the kernel high-half identically — the direct map, kernel text/rodata/data, and platform MMIO are visible during user execution as well as kernel execution. PAGE_SUBSTRATE §1 item 2 commits to this; HAL's job is to make it true.

### 10.6 What's hidden behind the trait
<!-- txdoc:HAL-PMAPIF-PMAP-MATERIALIZATION-PROOF-OBJECTS-WHATS-HIDDEN-BEHIND-THE-TRAIT-1 -->

The platform crate's `PmapIf` impl knows:

- Whether PTEs are Sv39, Sv48, or LA64 format.
- How to encode permissions into the PTE (Sv39 uses XWR bits; LA64 has different encoding).
- How to walk page tables on this arch.
- How to encode ASIDs.
- How to issue `sfence.vma` (RV64) or `invtlb` (LA64).

The substrate, VM, and any other consumer knows:

- Reservations are linear and droppable.
- Commits are infallible once you have the batch.
- Unmaps and shootdowns are coupled.
- The direct map exists and is stable.

---

## 11. TrapIf — trap frame discipline and return boundary
<!-- txdoc:HAL-TRAPIF-TRAP-FRAME-DISCIPLINE-AND-RETURN-BOUNDARY-1 -->

`TrapIf` is the second load-bearing HAL trait. It owns trap entry, trap-frame interpretation, and return-to-userspace. The kernel-side dispatch sink (next subsection) is what the trap shell calls *into*.

**Cross-reference.** Trap-cause dispatch from HAL is direct and named, not via linkme. See §21 for the registration discipline; this section covers the trap-frame view surface and the shell-to-sink contract.

The current executable RV64 QEMU subset installs a direct-mode `stvec` before
`BootPlatformIf::boot_handoff()` and reinstalls it from `kernel_main` after
`init_later()`. That vector now saves a full integer-register frame plus
`scause`/`sepc`/`stval`/`sstatus`, calls a board-binary dispatch symbol, and
routes the frame through a first `KernelTrapSink` implementation in
`tx-kernel`. Timer and IPI traps resume through the sink; external IRQ traps
resume as a stub; synchronous faults, syscalls, and user-facing traps terminate
through the panic path until their owners exist. `TrapIf` also exposes a typed
snapshot/classification API; RV64 QEMU decodes `scause` into page-fault,
illegal-instruction, breakpoint, user-ecall, supervisor-timer, and
supervisor-external classes. `TrapFrameMut` now has executable writeback
methods for PC, SP, syscall return/error, and user TLS, and the RV64 trap
return path writes saved `sepc`/`sstatus` back before `sret`. RV64 also has a
first unsafe `return_to_userspace` restore skeleton plus `sstatus` preparation
for user `sret`, but user execution is still not enabled: syscall dispatch,
VM page-fault policy, user trap stack switching, signal/user-return policy,
external IRQ device dispatch, and user-access recovery remain later slices.
When this executable trap path terminates on RV64 QEMU, the panic log prints
the legacy `scause`/`sepc`/`stval` summary and a full saved `trapframe:` dump;
`cargo xtask fault-decode --serial` understands both the old summary-only logs
and the richer trapframe block.

### 11.1 Trait surface
<!-- txdoc:HAL-TRAPIF-TRAP-FRAME-DISCIPLINE-AND-RETURN-BOUNDARY-TRAIT-SURFACE-1 -->

```rust
pub trait TrapIf {
    /// Architecture-specific saved trap frame. Opaque to non-platform
    /// code. Subsystem code reaches its fields only through view/view_mut.
    type RawTrapFrame;

    /// Classify the trap cause. Called immediately after entry from the
    /// trap shell, before dispatching to the appropriate KernelTrapSink
    /// method.
    fn classify(tf: &Self::RawTrapFrame) -> TrapClass;

    /// Read-only view over the trap frame's well-known fields.
    fn view(tf: &Self::RawTrapFrame) -> TrapFrameView<'_>;

    /// Mutable view. Used by syscall return-value writes, signal-frame
    /// setup, and exec's user-state initialization.
    fn view_mut(tf: &mut Self::RawTrapFrame) -> TrapFrameMut<'_>;

    /// Install the kernel-mode trap vector. Called from kernel_main
    /// after init_later. The vector points at the architecture's
    /// trap entry, which preserves registers, classifies, and calls
    /// into KernelTrapSink.
    fn install_kernel_trap_vector();

    /// Install the user-mode trap vector. Same semantics; separate
    /// because some architectures distinguish them (RV64: stvec is
    /// shared but separate scratch areas; LA64: similar).
    fn install_user_trap_vector();

    /// Return to user mode using the given trap frame.
    /// Diverges. Caller must have arranged for the trap frame's pc
    /// and registers to be in the desired state.
    unsafe fn return_to_userspace(tf: &Self::RawTrapFrame) -> !;
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum TrapClass {
    PageFault { write: bool, instruction: bool },
    Syscall,
    TimerInterrupt,
    ExternalInterrupt,
    /// Inter-processor interrupt. Distinct from external interrupts
    /// because IPIs use a separate trap cause on both RV64 and LA64,
    /// and they dispatch through SmpIf (§18) rather than IrqIf (§13).
    /// v1 uniprocessor: never seen (no other harts to send them).
    InterprocessorInterrupt,
    IllegalInstruction,
    Breakpoint,
    AlignmentFault,
    UnknownSync,
}
```

### 11.2 TrapFrameView and TrapFrameMut
<!-- txdoc:HAL-TRAPIF-TRAP-FRAME-DISCIPLINE-AND-RETURN-BOUNDARY-TRAPFRAMEVIEW-AND-TRAPFRAMEMUT-1 -->

These are stable, named-field views over architecturally varying register layouts. Subsystem code reads and writes through these views; it never sees `sepc`, `sstatus`, `era`, `prmd`, or any other register name.

`TrapFrameView` and `TrapFrameMut` are concrete structs, not `dyn`-traited handles. The `view`/`view_mut` methods on `TrapIf` construct them from the platform's `RawTrapFrame`. This keeps the HAL surface monomorphized — no dynamic dispatch, no virtual call cost on the trap path.

```rust
/// Read-only snapshot of a trap frame's well-known fields.
/// Constructed by TrapIf::view from the platform's RawTrapFrame.
pub struct TrapFrameView<'a> {
    pub pc: VirtAddr,
    pub sp: VirtAddr,
    pub syscall_number: u64,
    pub syscall_args: [u64; 6],
    pub fault_address: Option<VirtAddr>,
    pub faulting_instruction: Option<VirtAddr>,
    pub previous_privilege: PrevPrivilege,
    pub interrupts_enabled_before: bool,
    pub user_tls_register: u64,
    /// Lifetime tie to the underlying RawTrapFrame.
    _phantom: PhantomData<&'a ()>,
}

/// Mutable handle for writing back to a trap frame.
/// Constructed by TrapIf::view_mut. Carries a back-reference to the
/// raw frame (as a typed pointer the platform impl knows how to use)
/// so that set_* methods can write through to architecture-specific
/// fields without exposing them.
pub struct TrapFrameMut<'a> {
    raw: NonNull<()>,                       // type-erased &mut RawTrapFrame
    vtable: &'static TrapFrameMutVtable,
    _phantom: PhantomData<&'a mut ()>,
}

/// Function pointers populated by the platform's TrapIf::view_mut.
/// One TRAP_FRAME_MUT_VTABLE per platform; all reads through it
/// monomorphize when the platform is concrete.
pub struct TrapFrameMutVtable {
    pub set_pc: fn(NonNull<()>, VirtAddr),
    pub set_sp: fn(NonNull<()>, VirtAddr),
    pub set_syscall_return: fn(NonNull<()>, i64),
    pub set_syscall_error: fn(NonNull<()>, i32),
    pub set_user_tls_register: fn(NonNull<()>, u64),
    pub prepare_signal_frame: fn(NonNull<()>, SignalFrameSetup),
}

pub struct SignalFrameSetup {
    pub handler: VirtAddr,
    pub sig_no: u32,
    pub info_ptr: VirtAddr,
    pub ucontext_ptr: VirtAddr,
}

impl TrapFrameMut<'_> {
    pub fn set_pc(&mut self, pc: VirtAddr) {
        (self.vtable.set_pc)(self.raw, pc);
    }
    pub fn set_sp(&mut self, sp: VirtAddr) {
        (self.vtable.set_sp)(self.raw, sp);
    }
    pub fn set_syscall_return(&mut self, value: i64) {
        (self.vtable.set_syscall_return)(self.raw, value);
    }
    pub fn set_syscall_error(&mut self, errno: i32) {
        (self.vtable.set_syscall_error)(self.raw, errno);
    }
    pub fn set_user_tls_register(&mut self, value: u64) {
        (self.vtable.set_user_tls_register)(self.raw, value);
    }
    pub fn prepare_signal_frame(&mut self, setup: SignalFrameSetup) {
        (self.vtable.prepare_signal_frame)(self.raw, setup);
    }
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum PrevPrivilege {
    User,
    Kernel,
}
```

The vtable shape for `TrapFrameMut` is a pragmatic compromise. The alternative — making `TrapFrameMut` generic over `P` — would propagate the `P` parameter everywhere a syscall returns. The vtable confines the indirection to writeback methods (which are rare per syscall — typically one `set_syscall_return` call) while keeping the read path (`TrapFrameView`) fully concrete. The vtable itself is one `&'static` per platform; the cost of indirection is one indirect call per writeback, dwarfed by the cost of the syscall itself.

### 11.3 The shell-to-sink contract
<!-- txdoc:HAL-TRAPIF-TRAP-FRAME-DISCIPLINE-AND-RETURN-BOUNDARY-THE-SHELL-TO-SINK-CONTRACT-1 -->

The platform's trap entry (in assembly, called from the trap vector) does:

1. Save registers into a `RawTrapFrame` allocated on the per-hart kernel stack.
2. Switch to the kernel `gp` / `tp` register if the trap came from user mode.
3. Call into a Rust function in the platform crate that takes `&mut RawTrapFrame`.
4. That Rust function calls `TrapIf::classify`.
5. Based on the class, it calls one of the named entry points in `KernelTrapSink<P>`.

```rust
// In the platform crate's trap.rs (Rust side):
pub extern "C" fn rust_trap_entry<P, K>(tf: &mut P::RawTrapFrame)
where
    P: TxPlatform,
    K: KernelTrapSink<P>,
{
    let class = P::classify(tf);

    // Snapshot read-only fields needed for classification before
    // taking the mutable view; the snapshot is cheap (a few field
    // copies) and avoids re-borrowing tf.
    let snapshot = P::view(tf);
    let from_user = snapshot.previous_privilege == PrevPrivilege::User;
    let fault_address = snapshot.fault_address;
    let faulting_instruction = snapshot.faulting_instruction;
    drop(snapshot); // release the immutable borrow

    // The mutable view IS the handle to tf for sink methods.
    // After this point, tf must not be touched directly.
    let view = P::view_mut(tf);

    let action = match class {
        TrapClass::PageFault { write, instruction } => {
            let fault = FaultInfo {
                address: fault_address.unwrap_or(VirtAddr(0)),
                write,
                instruction,
                from_user,
            };
            K::on_page_fault(view, fault)
        }
        TrapClass::Syscall => K::on_syscall(view),
        TrapClass::TimerInterrupt => K::on_timer_interrupt(P::current_cpu_id()),
        TrapClass::ExternalInterrupt => K::on_external_irq(P::current_cpu_id()),
        TrapClass::InterprocessorInterrupt => K::on_ipi(P::current_cpu_id()),
        TrapClass::IllegalInstruction
        | TrapClass::AlignmentFault
        | TrapClass::Breakpoint
        | TrapClass::UnknownSync => {
            let fault = FaultInfo {
                address: faulting_instruction.unwrap_or(VirtAddr(0)),
                write: false,
                instruction: true,
                from_user,
            };
            K::on_illegal_or_sync_fault(view, fault)
        }
    };

    apply_trap_action::<P>(tf, action);
}
```

After the match arm returns, `view` is dropped (it carries `&mut` borrow of `tf` via the vtable handle), and `apply_trap_action::<P>(tf, action)` reclaims `tf` to perform the `TrapAction` (including `return_to_userspace` if appropriate).

The `KernelTrapSink<P>` trait is defined in `tx-hal` but its impl lives in `tx-kernel`:

```rust
// tx-hal/src/trap_sink.rs
pub trait KernelTrapSink<P: TxPlatform> {
    fn on_page_fault(
        view: TrapFrameMut<'_>,
        fault: FaultInfo,
    ) -> TrapAction;

    fn on_syscall(view: TrapFrameMut<'_>) -> TrapAction;

    fn on_timer_interrupt(cpu: CpuId) -> TrapAction;

    fn on_external_irq(cpu: CpuId) -> TrapAction;

    /// Called when an IPI arrives. Distinct from on_external_irq
    /// because IPIs are not driven by IrqIf; they come from peer
    /// harts via SmpIf::send_ipi.
    /// v1 uniprocessor: never called.
    fn on_ipi(cpu: CpuId) -> TrapAction;

    fn on_illegal_or_sync_fault(
        view: TrapFrameMut<'_>,
        fault: FaultInfo,
    ) -> TrapAction;
}

#[derive(Copy, Clone, Debug)]
pub enum TrapAction {
    /// Resume the trapped context (most syscalls, handled IRQs,
    /// resolved page faults).
    Resume,
    /// Reschedule before resuming (timer ticks, preemption-relevant IRQs).
    Reschedule,
    /// Deliver a pending signal at AST.
    DeliverSignal,
    /// Terminate the current thread (unrecoverable fault).
    Terminate,
}

pub struct FaultInfo {
    pub address: VirtAddr,
    pub write: bool,
    pub instruction: bool,
    pub from_user: bool,
}
```

The kernel implements `KernelTrapSink<P>` once, generic over `P`:

```rust
// tx-kernel/src/trap.rs
pub struct Kernel;

impl<P: TxPlatform> KernelTrapSink<P> for Kernel {
    fn on_page_fault(view: TrapFrameMut<'_>, fault: FaultInfo) -> TrapAction {
        vm::handle_page_fault::<P>(view, fault)
    }
    fn on_syscall(view: TrapFrameMut<'_>) -> TrapAction {
        syscall::dispatch::<P>(view)
    }
    fn on_timer_interrupt(cpu: CpuId) -> TrapAction {
        scheduler::on_timer_tick::<P>(cpu)
    }
    fn on_external_irq(cpu: CpuId) -> TrapAction {
        irq::dispatch_external::<P>(cpu)
    }
    fn on_ipi(cpu: CpuId) -> TrapAction {
        smp::dispatch_ipi::<P>(cpu)
    }
    fn on_illegal_or_sync_fault(view: TrapFrameMut<'_>, fault: FaultInfo) -> TrapAction {
        signal::deliver_synchronous_fault::<P>(view, fault)
    }
}
```

### 11.4 What's hidden behind the trait
<!-- txdoc:HAL-TRAPIF-TRAP-FRAME-DISCIPLINE-AND-RETURN-BOUNDARY-WHATS-HIDDEN-BEHIND-THE-TRAIT-1 -->

The platform crate knows:

- The architecture's trap-cause register (`scause` on RV64, `estat` on LA64).
- Which fields of the trap frame are at which offsets.
- How to save/restore the full register file.
- How to handle the `gp`/`tp` swap on user→kernel transitions.

Subsystem code knows:

- Trap classes (TrapClass enum).
- Trap-frame fields by name (pc, sp, syscall_number, fault_address, etc.).
- TrapAction return values.

No subsystem code references `sepc`, `sstatus`, `sscratch`, `era`, `prmd`, `badv`, or any other register-level name.

---

## 12. UserAccessIf — kernel-mode user access and fixup recovery
<!-- txdoc:HAL-USERACCESSIF-KERNEL-MODE-USER-ACCESS-AND-FIXUP-RECOVERY-1 -->

`UserAccessIf` is logically distinct from `TrapIf`. `TrapIf` answers "what happened at trap entry, and how do we return?" `UserAccessIf` answers "how does a kernel-mode user-memory access fail safely and become a syscall-level error?"

This split matters because:

- The fixup table is a build-time artifact (linker-section metadata).
- The recovery mechanism is a kernel-mode-fault-only path; it does not interact with user trap entry.
- Subsystems that copy to/from user (every syscall that takes a pointer) need a clean error-channel surface, not direct access to the trap path.

### 12.1 Trait surface
<!-- txdoc:HAL-USERACCESSIF-KERNEL-MODE-USER-ACCESS-AND-FIXUP-RECOVERY-TRAIT-SURFACE-1 -->

```rust
pub trait UserAccessIf {
    /// Copy `len` bytes from user space `src` to kernel space `dst`.
    /// Returns Ok(()) on success, Err(FaultInfo) on user-side fault.
    /// Safety: caller must ensure dst is a valid kernel pointer of
    /// at least `len` bytes; src is interpreted in the current
    /// AddressSpace's user mapping.
    unsafe fn copy_from_user(
        dst: KernelPtr<u8>,
        src: UserPtr<u8>,
        len: usize,
    ) -> Result<(), FaultInfo>;

    /// Mirror of copy_from_user with direction reversed.
    unsafe fn copy_to_user(
        dst: UserPtr<u8>,
        src: KernelPtr<u8>,
        len: usize,
    ) -> Result<(), FaultInfo>;

    /// Read a primitive value from user space.
    /// Specialized for compiler-friendly small types.
    unsafe fn read_user<T: Pod>(src: UserPtr<T>) -> Result<T, FaultInfo>;

    /// Write a primitive value to user space.
    unsafe fn write_user<T: Pod>(dst: UserPtr<T>, value: T) -> Result<(), FaultInfo>;
}
```

`KernelPtr<T>` and `UserPtr<T>` are typed pointer wrappers defined in the meta-framework primitives crate. They make kernel-vs-user pointer confusion a type error at the call site:

```rust
// In meta-framework primitives:
#[repr(transparent)]
pub struct KernelPtr<T>(*mut T);

#[repr(transparent)]
pub struct UserPtr<T>(*mut T);
```

A syscall that takes a user pointer receives `UserPtr<T>` from the syscall pipeline; it cannot accidentally pass it to a kernel-pointer-only function (the type is wrong). Conversion is explicit and goes through `UserAccessIf` — there is no `as_kernel_ptr()` method on `UserPtr<T>`.

### 12.2 The fixup table
<!-- txdoc:HAL-USERACCESSIF-KERNEL-MODE-USER-ACCESS-AND-FIXUP-RECOVERY-THE-FIXUP-TABLE-1 -->

Kernel-mode user accesses use a special trap-recovery mechanism. Each `copy_*_user` macro emits a fixup entry into a linker section:

```rust
// In tx-hal:
#[repr(C)]
pub struct FixupEntry {
    /// Range of kernel PCs covered by this fixup.
    pub pc_start: VirtAddr,
    pub pc_end: VirtAddr,
    /// Where to jump on fault.
    pub recovery_pc: VirtAddr,
}

#[linkme::distributed_slice]
pub static KERNEL_FIXUP_TABLE: [FixupEntry] = [..];
```

Each platform's `copy_from_user` impl emits a fixup entry through `linkme::distributed_slice(KERNEL_FIXUP_TABLE)`. The trap shell, when it sees a kernel-mode fault, consults `KERNEL_FIXUP_TABLE` (binary search by `pc_start`) and jumps to `recovery_pc` if a match exists.

**Cross-reference.** This is one of the three approved linkme uses. See §21.

### 12.3 The primary path is direct, not fixup
<!-- txdoc:HAL-USERACCESSIF-KERNEL-MODE-USER-ACCESS-AND-FIXUP-RECOVERY-THE-PRIMARY-PATH-IS-DIRECT-NOT-FIXUP-1 -->

The substrate-time `direct_map` and the resolve-side syscall surface (PAGE_SUBSTRATE handoff) make most user accesses *not* hit the fixup path. The primary path is:

1. Resolve the user pointer to a PPN via the address space's pmap.
2. Translate PPN to direct-map kernel virtual address.
3. Copy via the direct map.

The fixup path catches the residual cases: speculative reads of nominally-mapped pages that turn out to fault (e.g., a `PROT_NONE` page in the middle of a copy that the resolve step didn't pre-walk), and architecture-level corner cases where the trap fires despite the resolve.

### 12.4 FaultInfo in the user-access context
<!-- txdoc:HAL-USERACCESSIF-KERNEL-MODE-USER-ACCESS-AND-FIXUP-RECOVERY-FAULTINFO-IN-THE-USER-ACCESS-CONTEXT-1 -->

```rust
pub struct FaultInfo {
    pub address: VirtAddr,
    pub write: bool,
    pub instruction: bool,
    pub from_user: bool,
}
```

For user-access faults, `from_user` is false (the fault is in kernel mode), but `address` is the user address that faulted. This is what `EFAULT` becomes — the syscall returns `-EFAULT` and the `address` is what `siginfo` would carry if a SIGSEGV were appropriate.

### 12.5 Why this is a separate trait
<!-- txdoc:HAL-USERACCESSIF-KERNEL-MODE-USER-ACCESS-AND-FIXUP-RECOVERY-WHY-THIS-IS-A-SEPARATE-TRAIT-1 -->

If `UserAccessIf` lived inside `TrapIf`, then changes to copy-to-user semantics (e.g., adding `copy_from_user_atomic` for non-blocking contexts) would force the trap-frame view surface to change. Splitting the traits keeps the rate of change in each trait independent.

The platform crate impls them together — they share the assembly-level fault-recovery mechanism — but the kernel-side type surface is two traits.

### 12.6 SignalFrameIf — userspace signal-frame ABI
<!-- txdoc:HAL-USERACCESSIF-KERNEL-MODE-USER-ACCESS-AND-FIXUP-RECOVERY-SIGNALFRAMEIF-USERSPACE-SIGNAL-FRAME-ABI-1 -->

`SignalFrameIf` is the architecture ABI helper used by the signal subsystem. It does **not** install a HAL-owned signal hook table. The trap path reaches signal logic through `KernelTrapSink<P>` and named signal functions; the platform only supplies the per-arch frame layout and register rewrites needed to enter and leave a user signal handler.

```rust
pub trait SignalFrameIf: TrapIf + UserAccessIf {
    /// Write siginfo/ucontext/trampoline state to the selected user stack
    /// and rewrite the trap frame so return_to_userspace enters the handler.
    fn write_signal_frame(
        tf: TrapFrameMut<'_>,
        setup: SignalFrameWrite,
    ) -> Result<SignalFramePlacement, FaultInfo>;

    /// Read and validate the frame addressed by sigreturn's current user SP.
    fn read_signal_frame(user_sp: UserPtr<u8>) -> Result<SavedSignalFrame, FaultInfo>;

    /// Restore registers from a validated sigreturn frame into the
    /// current trap-frame view.
    fn restore_signal_frame(tf: TrapFrameMut<'_>, frame: &SavedSignalFrame);

    /// Rewind the syscall PC for SA_RESTART.
    fn rewind_syscall_pc(tf: TrapFrameMut<'_>);
}

pub struct SignalFrameWrite {
    pub stack_top: UserPtr<u8>,
    pub sig_no: u32,
    pub siginfo: UserSigInfoAbi,
    pub old_mask: UserSignalMaskAbi,
    pub flags: UserSaFlagsAbi,
    pub handler_pc: UserPtr<()>,
}

pub struct SignalFramePlacement {
    pub frame_addr: UserPtr<()>,
    pub trampoline_pc: UserPtr<()>,
}

pub struct SavedSignalFrame {
    pub saved_mask: UserSignalMaskAbi,
    // The platform-private saved register image is interpreted only by
    // SignalFrameIf::restore_signal_frame.
}
```

`UserSigInfoAbi`, `UserSignalMaskAbi`, and `UserSaFlagsAbi` are shared ABI value types, not imports from the signal subsystem. The signal subsystem translates its semantic `SigInfo`, `SignalMask`, and action flags into these ABI values before calling `SignalFrameIf`.

---

## 13. IrqIf
<!-- txdoc:HAL-IRQIF-1 -->

`IrqIf` is the platform's interrupt-controller surface. It owns claim/complete cycles, masking, and per-line handler installation.

**Cross-reference.** `linkme IRQ_HANDLERS` is a registration source consumed at device init time; the runtime IRQ path indexes the installed table. See §21.

### 13.1 Trait surface
<!-- txdoc:HAL-IRQIF-TRAIT-SURFACE-1 -->

```rust
pub trait IrqIf {
    /// Maximum IRQ number this platform supports.
    /// RV64 qemu-virt PLIC: 1024. LA64 ExtIOI: 256.
    const MAX_IRQ: u32;

    /// Claim the highest-priority pending IRQ on the current hart.
    /// Called from the trap shell after classify returns
    /// TrapClass::ExternalInterrupt.
    /// Returns 0 if no IRQ is actually pending (spurious).
    fn claim() -> u32;

    /// Acknowledge completion of an IRQ. Must be paired with claim.
    fn complete(irq: u32);

    /// Mask an IRQ at the controller level.
    fn mask(irq: u32);

    /// Unmask an IRQ at the controller level.
    fn unmask(irq: u32);

    /// Set the priority of an IRQ. Higher values are higher priority.
    /// Platforms that don't support priorities accept all values
    /// silently (e.g., LA64 ExtIOI on simple configurations).
    fn set_priority(irq: u32, priority: u8);

    /// Install the per-line dispatch table built by device init.
    /// Called once after device::init walks IRQ_HANDLERS.
    /// Subsequent claim/complete cycles use this table.
    fn install_dispatch_table(table: &'static IrqDispatchTable);
}

pub struct IrqDispatchTable {
    /// Entry for each IRQ number, indexed directly.
    /// None for unhandled IRQs (becomes a kernel warning + mask).
    pub entries: [Option<IrqHandlerFn>; Self::SIZE],
}

pub type IrqHandlerFn = fn(irq: u32) -> IrqHandled;

#[derive(Copy, Clone, Debug)]
pub enum IrqHandled {
    /// Handler ran to completion; complete() may be called.
    Done,
    /// Handler woke a thread that should run; reschedule recommended.
    Wake,
    /// Handler did nothing (shared IRQ that wasn't ours).
    NotMine,
}
```

### 13.2 The registration / installation split
<!-- txdoc:HAL-IRQIF-THE-REGISTRATION-INSTALLATION-SPLIT-1 -->

Devices register their handlers at link time:

```rust
// In a virtio-blk driver:
#[linkme::distributed_slice(tx_hal::IRQ_HANDLERS)]
pub static VIRTIO_BLK0_IRQ: IrqHandlerRegistration = IrqHandlerRegistration {
    irq: 8,                          // PLIC line 8 on qemu-virt
    handler: virtio_blk_irq_handler,
    name: "virtio-blk0",
};
```

`tx-hal` defines:

```rust
#[linkme::distributed_slice]
pub static IRQ_HANDLERS: [IrqHandlerRegistration] = [..];

pub struct IrqHandlerRegistration {
    pub irq: u32,
    pub handler: IrqHandlerFn,
    pub name: &'static str,
}
```

At device init time, the kernel walks the linkme slice and builds `IrqDispatchTable`:

```rust
// In device::init
let mut table = IrqDispatchTable::new();
for reg in IRQ_HANDLERS {
    if let Some(existing) = &table.entries[reg.irq as usize] {
        panic!("IRQ {} double-registered: {} vs {}",
               reg.irq, existing.name, reg.name);
    }
    table.entries[reg.irq as usize] = Some(reg.handler);
}
let table: &'static IrqDispatchTable = Box::leak(Box::new(table));
P::install_dispatch_table(table);
```

After this point, the runtime path is:

```rust
// In KernelTrapSink::on_external_irq:
fn on_external_irq(cpu: CpuId) -> TrapAction {
    let irq = P::claim();
    if irq == 0 { return TrapAction::Resume; }  // spurious

    let handler = INSTALLED_TABLE.entries[irq as usize];
    let result = match handler {
        Some(h) => h(irq),
        None => {
            log::warn!("unhandled IRQ {}, masking", irq);
            P::mask(irq);
            IrqHandled::Done
        }
    };

    P::complete(irq);

    match result {
        IrqHandled::Wake => TrapAction::Reschedule,
        IrqHandled::Done | IrqHandled::NotMine => TrapAction::Resume,
    }
}
```

The runtime path indexes the installed table directly. It does not iterate `IRQ_HANDLERS` per interrupt. This is the rule from the revision: linkme is a registration source, not a dispatch table.

### 13.3 Tier-1 IRQs (PLIC, ExtIOI internals)
<!-- txdoc:HAL-IRQIF-TIER-1-IRQS-PLIC-EXTIOI-INTERNALS-1 -->

The interrupt controller itself is a tier-1 device (DEVICE.md §2.1). Its register accesses, claim/complete cycle, masking — all of that is *inside* the platform crate's `IrqIf` impl. No tier-2 or tier-3 driver code touches the PLIC or ExtIOI directly; they go through `IrqIf`.

### 13.4 IPI vs external IRQ
<!-- txdoc:HAL-IRQIF-IPI-VS-EXTERNAL-IRQ-1 -->

External IRQs and IPIs are different on both architectures we support. RV64 has separate trap causes (`Interrupt::SupervisorSoft` for IPI vs `Interrupt::SupervisorExternal` for PLIC IRQs). LA64 distinguishes IPI from ExtIOI similarly.

`IrqIf` is for external IRQs only. IPIs go through `SmpIf` (§18). The trap shell distinguishes them via `TrapClass::ExternalInterrupt` versus `TrapClass::InterprocessorInterrupt` (§11.1), routing each to a separate `KernelTrapSink` method (`on_external_irq` and `on_ipi`).

---

## 14. TimeIf
<!-- txdoc:HAL-TIMEIF-1 -->

`TimeIf` is the platform's monotonic clock and the deadline source for the scheduler/reactor. It is small but performance-critical: `read_ns` is called from the scheduler's hot path.

```rust
pub trait TimeIf {
    /// Read the current monotonic time in nanoseconds since boot.
    /// Must be:
    /// - monotonically non-decreasing on a single hart;
    /// - approximately monotonic across harts (within hardware skew);
    /// - cheap (typically: read a single CSR, multiply by a constant).
    fn read_ns() -> u64;

    /// Program the per-hart timer to fire at the given absolute
    /// monotonic deadline. If the deadline is in the past, fire ASAP.
    fn set_deadline_ns(deadline: u64);

    /// Cancel any pending timer on the current hart.
    fn cancel_deadline();

    /// Prepare the current hart so a programmed timer deadline can wake or
    /// trap out of the platform idle path.
    fn enable_timer_wakeups();

    /// Read the timer-interrupt frequency, used for converting
    /// between cycles and nanoseconds during early init.
    fn frequency_hz() -> u64;
}
```

### 14.1 RV64 vs LA64 implementation
<!-- txdoc:HAL-TIMEIF-RV64-VS-LA64-IMPLEMENTATION-1 -->

| Platform | Time CSR | Timer set | Comment |
|---|---|---|---|
| RV64 | `time` (rdtime) | SBI `set_timer` (M-mode) | qemu-virt CLINT is virtualized by SBI |
| LA64 | `stable_counter` | `tcfg` CSR | Direct supervisor access to stable timer |

The platform crate handles these differences. Subsystem code calls `P::read_ns()` regardless.

### 14.2 The timer-interrupt path
<!-- txdoc:HAL-TIMEIF-THE-TIMER-INTERRUPT-PATH-1 -->

When the timer fires, the trap shell classifies it as `TrapClass::TimerInterrupt` and calls `KernelTrapSink::on_timer_interrupt(cpu)`. The kernel's impl is in the scheduler:

```rust
fn on_timer_interrupt(cpu: CpuId) -> TrapAction {
    scheduler::on_timer_tick::<P>(cpu)
}
```

The scheduler's timer tick handler walks the per-hart timer wheel (or whatever data structure the scheduler/reactor uses), wakes any threads whose deadlines have passed, and returns `TrapAction::Reschedule` if the wake set is non-empty.

### 14.3 What `TimeIf` does *not* provide
<!-- txdoc:HAL-TIMEIF-WHAT-TIMEIF-DOES-NOT-PROVIDE-1 -->

- Wall-clock time. That's a higher-level concept (CLOCK_REALTIME) implemented above HAL using `read_ns` plus a stored offset.
- Per-process / per-thread CPU time. That's the signal subsystem's responsibility; HAL provides the raw monotonic clock.
- High-precision timestamping for tracing. Tracing reads `read_ns` like any other consumer; it does not have a separate fast path.

---

## 15. PercpuIf
<!-- txdoc:HAL-PERCPUIF-1 -->

`PercpuIf` is the per-hart kernel state surface: kernel stack pointer, current CPU ID, kernel-side TLS register. It does *not* cover scheduler per-CPU state (run queue, idle thread) — that's the scheduler's territory.

```rust
pub trait PercpuIf {
    /// Return the current hart's CPU ID. Must be cheap (single
    /// register read on most architectures).
    /// RV64: read tp (after install_early_percpu sets it).
    /// LA64: read $r21.
    fn current_cpu_id() -> CpuId;

    /// Install the early per-CPU pointer for this hart. Called from
    /// tx_hal::entry (BSP) and from AP secondary entry. Sets the
    /// arch's per-CPU register (tp on RV64, $r21 on LA64) to point
    /// at the per-CPU area for this hart.
    fn install_early_percpu(cpu: CpuId);

    /// Read the kernel TLS register (the kernel's, not the user's).
    /// Used by the scheduler to find the current task.
    fn read_kernel_tls() -> u64;

    /// Write the kernel TLS register.
    fn write_kernel_tls(value: u64);

    /// Switch to a different per-hart kernel stack. Called during
    /// AP bring-up after the temporary stack has done its job.
    /// Does NOT switch tasks; that's the scheduler's job.
    unsafe fn install_kernel_stack(top: VirtAddr);
}

#[derive(Copy, Clone, Eq, PartialEq, Debug, Hash)]
pub struct CpuId(pub u32);
```

### 15.1 Why kernel TLS is here and user TLS is in TrapFrameMut
<!-- txdoc:HAL-PERCPUIF-WHY-KERNEL-TLS-IS-HERE-AND-USER-TLS-IS-IN-TRAPFRAMEMUT-1 -->

User TLS (`tp` on RV64 user mode, user-side `$r2` on LA64) is part of user state. It's saved into the trap frame on entry, restored on return. CLONE_SETTLS works by writing into `TrapFrameMut::set_user_tls_register` before returning to user mode. Subsystem code that creates threads doesn't need any HAL knowledge beyond the `set_user_tls_register` call.

Kernel TLS (`tp` on RV64 kernel mode, `$r21` on LA64) is part of kernel state. It points at the per-CPU area, which holds the current-task pointer and other per-hart state the scheduler maintains. It's invariant across user→kernel transitions on the same hart.

These are different things. They share an architecture register name (RV64: both use `tp`, with the user value saved in trap frame and the kernel value in the live `tp` register), but the conceptual split is real.

### 15.2 Per-CPU area structure
<!-- txdoc:HAL-PERCPUIF-PER-CPU-AREA-STRUCTURE-1 -->

The platform crate defines the per-CPU area structure and allocates one per possible hart. The kernel writes into it via the scheduler:

```rust
// Platform-private:
#[repr(C)]
struct PerCpuArea {
    cpu_id: CpuId,
    kernel_stack_top: VirtAddr,
    current_task: AtomicPtr<TaskFrame>,
    // ... whatever the platform/scheduler agrees on
}

static PERCPU: [PerCpuArea; MAX_CPUS] = [...];
```

The scheduler reaches `current_task` via `PercpuIf::read_kernel_tls()` cast through the platform-published `PerCpuArea` layout. There's a small amount of unsafety at the boundary (the platform defines the struct; the kernel reads fields by offset), but it's confined.

### 15.3 No `TlsIf`
<!-- txdoc:HAL-PERCPUIF-NO-TLSIF-1 -->

The revision asked for TLS to be folded into `PercpuIf` (kernel side) and `TrapFrameMut` (user side). That's what this section does. No separate `TlsIf`.

---

## 16. CacheIf
<!-- txdoc:HAL-CACHEIF-1 -->

`CacheIf` exists even on coherent platforms because exec, DMA, PTE publication, and instruction-cache coherence all need arch-specific fences or cache maintenance. On coherent QEMU targets, many methods may be no-ops, but the contract exists so consumer code does not `cfg` on architecture.

```rust
pub trait CacheIf {
    /// Full memory barrier (data + instruction).
    /// RV64: fence iorw,iorw + fence.i.
    /// LA64: dbar 0 + ibar 0.
    fn fence_all();

    /// Instruction-cache fence for the current hart only.
    /// RV64: fence.i.
    /// LA64: ibar 0.
    fn fence_i_local();

    /// Broadcast instruction-cache invalidation to all harts.
    /// RV64: SBI remote_fence_i.
    /// LA64: ibar 0 with cross-hart hardware coherence (typically
    /// no broadcast needed; ibar local suffices on coherent LA64).
    fn fence_i_all();

    /// Flush instruction-cache for a range. Used after exec loads
    /// a new program into memory and before jumping to user pc.
    fn flush_icache_range(start: VirtAddr, len: usize);

    /// Clean (write-back) data cache for a physical range.
    /// On coherent platforms (DMA_COHERENT = true): no-op.
    fn dcache_clean_range(start: PhysAddr, len: usize);

    /// Invalidate (discard, no write-back) data cache for a range.
    /// On coherent platforms: no-op.
    fn dcache_invalidate_range(start: PhysAddr, len: usize);

    /// Clean + invalidate. For DMA buffers transitioning between
    /// directions.
    /// On coherent platforms: no-op.
    fn dcache_clean_invalidate_range(start: PhysAddr, len: usize);
}
```

### 16.1 Consumer list
<!-- txdoc:HAL-CACHEIF-CONSUMER-LIST-1 -->

- **exec / ELF loader.** After loading executable pages, call `flush_icache_range` over the .text region before the first user-mode entry. Otherwise the i-cache may hold stale data from the previous use of those frames.
- **DMA.** Drivers call `dcache_clean_range` before handing a buffer to a device (write to memory must be visible to the device) and `dcache_invalidate_range` after the device has written into memory and before the CPU reads it. The `DmaIf` (§17) wraps these in direction-aware helpers.
- **PTE publication.** After installing PTEs, the hart that installed them issues `sfence.vma` / `invtlb` (this is `PmapIf::shootdown`, not `CacheIf`). But `CacheIf::fence_all` is what subsystems use when they need general memory ordering across HAL layers.
- **JIT / future module loading.** Out of scope for v1.

### 16.2 Why not `cfg`?
<!-- txdoc:HAL-CACHEIF-WHY-NOT-CFG-1 -->

Because the consumer code is *generic* over `P: TxPlatform`. A driver doesn't know whether it's on a coherent or non-coherent board until link time. It calls `P::dcache_clean_range`, and the impl is a no-op on coherent boards and a real cache flush on non-coherent ones. No `cfg` in driver code.

---

## 17. DmaIf
<!-- txdoc:HAL-DMAIF-1 -->

`DmaIf` is the layer where drivers learn whether DMA is coherent and what their physical addresses look like to a device. v1 assumes direct DMA on QEMU boards (DMA address = physical address), but the interface is present so drivers cannot smuggle that assumption.

```rust
#[derive(Copy, Clone, Debug)]
pub enum DmaDirection {
    /// CPU writes, device reads.
    ToDevice,
    /// Device writes, CPU reads.
    FromDevice,
    /// Both directions.
    Bidirectional,
}

pub trait DmaIf: PlatformConfig {
    /// True if the platform has cache-coherent DMA.
    /// When true, the sync_for_* methods may be no-ops.
    /// (PlatformConfig::DMA_COHERENT is the same fact; this is a
    /// convenience re-export.)
    const DMA_COHERENT: bool = <Self as PlatformConfig>::DMA_COHERENT;

    /// Translate a physical address to the address the device sees.
    /// On platforms with no IOMMU and identity DMA: paddr.0 as DmaAddr.
    /// On platforms with an IOMMU: the IOMMU-translated address.
    fn phys_to_dma(paddr: PhysAddr) -> DmaAddr;

    /// Reverse of phys_to_dma.
    fn dma_to_phys(daddr: DmaAddr) -> PhysAddr;

    /// Prepare a buffer for device access.
    /// Direction tells what the device will do with it.
    fn sync_for_device(paddr: PhysAddr, len: usize, dir: DmaDirection);

    /// Prepare a buffer for CPU access after the device finished.
    fn sync_for_cpu(paddr: PhysAddr, len: usize, dir: DmaDirection);
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub struct DmaAddr(pub u64);
```

### 17.1 v1 expected impls
<!-- txdoc:HAL-DMAIF-V1-EXPECTED-IMPLS-1 -->

For both qemu-virt RV64 and qemu-virt LA64:

- `DMA_COHERENT = true`.
- `phys_to_dma(p)` = `DmaAddr(p.0)`.
- `dma_to_phys(d)` = `PhysAddr(d.0)`.
- `sync_for_device` / `sync_for_cpu` = no-ops.

For VisionFive2: same (JH7110 is coherent).

For 2K1000LA: confirm in board crate. If non-coherent, the sync methods become real `dcache_clean_range` / `dcache_invalidate_range` calls.

### 17.2 Why drivers go through DmaIf
<!-- txdoc:HAL-DMAIF-WHY-DRIVERS-GO-THROUGH-DMAIF-1 -->

A virtio driver does not write `let dma_addr = phys_addr.0;`. It writes:

```rust
let dma = P::phys_to_dma(buf.phys_addr());
P::sync_for_device(buf.phys_addr(), buf.len(), DmaDirection::ToDevice);
queue.push_descriptor(dma, buf.len() as u32);
```

This is portable across coherent/non-coherent boards and across boards with/without IOMMUs (we don't have an IOMMU in v1, but the surface allows one to be added without driver changes).

---

## 18. SmpIf — low-level mechanics
<!-- txdoc:HAL-SMPIF-LOW-LEVEL-MECHANICS-1 -->

`SmpIf` exposes the low-level mechanics needed to start and control secondary CPUs. It does not specify the kernel-level SMP protocol. The scheduler handoff, reschedule IPI policy, TLB shootdown protocol, and stop-the-world protocol all belong to `SMP_v1`.

Until `SMP_v1` exists, PAGE_SUBSTRATE keeps the rule: APs are not executing
substrate consumers while substrate initialization is in progress. A platform
may start APs only after BSP substrate init and full trap-vector installation;
those APs must publish early per-CPU state, run `tx_substrate::init_on_ap(cpu)`,
install their kernel trap vector, mark online, and then enter a kernel-owned AP
runtime loop or a permanent park path. The runtime loop may use HAL wait and IPI
observation primitives, but queue policy and work selection stay above HAL. In
uniprocessor builds, all `SmpIf` methods except `current_cpu_id` and
`possible_cpus` may be no-ops or unsupported.

```rust
pub trait SmpIf {
    /// Current hart's CPU ID. (Same as PercpuIf::current_cpu_id;
    /// re-exposed here because some consumers want only SmpIf.)
    fn current_cpu_id() -> CpuId;

    /// Set of CPUs that exist on this platform.
    /// Returns the cpumask discovered from PlatformInfo/firmware facts.
    fn possible_cpus() -> CpuMask;

    /// Set of CPUs that have completed early per-CPU setup, AP-local substrate
    /// initialization, trap-vector installation, and online publication.
    fn online_cpus() -> CpuMask;

    fn possible_cpu_count() -> usize;
    fn online_cpu_count() -> usize;
    fn is_cpu_online(cpu: CpuId) -> bool;
    fn mark_cpu_online(cpu: CpuId);

    /// Boot all secondary CPUs, jumping each to `entry` after its
    /// arch-specific low trampoline. Returns the number that actually reached
    /// the online mask before the platform wait window closed.
    fn boot_secondary_cpus(entry: SecondaryEntry) -> usize;

    /// Prepare the current hart so an IPI can wake a low-power wait.
    /// This is a local hardware primitive; it does not dispatch scheduler work.
    fn enable_ipi_wakeups();

    /// Wait once for an interrupt or platform wake event. The caller owns the
    /// surrounding condition check and lost-wake discipline.
    fn wait_for_interrupt_once();

    /// Report whether an IPI of this kind is pending on the current hart.
    /// Used by kernel-owned AP loops that poll/ack low-level IPI state instead
    /// of handing scheduler policy to the trap vector.
    fn pending_ipi(kind: IpiKind) -> bool;

    /// Park the current hart indefinitely. Used for AP entry on
    /// uniprocessor builds (APs that get woken anyway) and for
    /// shutdown.
    fn park_this_cpu() -> !;

    /// Send an IPI to the given target CPU.
    /// Uniprocessor defaults assert if target != current.
    fn send_ipi(target: CpuId, kind: IpiKind);

    /// Send an IPI to all CPUs in the mask.
    /// Uniprocessor defaults assert if the mask cannot be handled locally.
    fn broadcast_ipi(mask: CpuMask, kind: IpiKind);

    /// Acknowledge a received IPI. Called from KernelTrapSink's
    /// IPI dispatch. May be a no-op on architectures where IPI
    /// acknowledgment is implicit in trap return.
    fn ack_ipi(kind: IpiKind);

    /// Clear observed IPI acknowledgements for a target mask before a
    /// low-level probe or kernel-managed handshake.
    fn clear_ipi_ack_cpus(kind: IpiKind, mask: CpuMask);

    /// Return CPUs that have acknowledged the given IPI kind since the last
    /// clear. Platforms may keep this as a debug/protocol bitmap; scheduler
    /// policy does not live here.
    fn ipi_ack_cpus(kind: IpiKind) -> CpuMask;

    /// Bounded wait for the target acknowledgement mask.
    fn wait_for_ipi_ack_cpus(mask: CpuMask, kind: IpiKind) -> usize;
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum IpiKind {
    /// Wake the target hart's reactor. Used for cross-hart wakeups
    /// when a thread on another hart becomes runnable.
    Reschedule,
    /// TLB shootdown. The originator has installed an invalidation
    /// list in a known location; the target hart processes it.
    /// (Actual coordination is in PmapIf::shootdown.)
    TlbShootdown,
    /// Stop-the-world request. Reserved for future use (kernel
    /// debugging, panic synchronization).
    Stop,
}

pub type SecondaryEntry = unsafe extern "C" fn(cpu_id: usize) -> !;

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub struct CpuMask(pub u64);  // v1: 64 CPU max
```

### 18.1 What HAL specifies
<!-- txdoc:HAL-SMPIF-LOW-LEVEL-MECHANICS-WHAT-HAL-SPECIFIES-1 -->

- BSP entry, AP low-level entry shape.
- Temporary per-hart stack selection for BSP and AP low entry.
- Per-CPU pointer setup on APs (via `PercpuIf::install_early_percpu`).
- Trap-vector installation on APs (via `TrapIf::install_kernel_trap_vector`).
- Possible/online CPU masks and the low-level online publication bit. The
  generic kernel publishes AP online only after AP-local substrate init.
- IPI send/receive primitives, pending-state observation, wake-from-wait
  enablement, and low-level acknowledgement observation.
- A single low-power wait primitive that a kernel-owned AP loop can compose
  with its own condition checks.
- CPU parking primitive.
- Platform remote-TLB primitive when firmware provides one. RV64 QEMU uses SBI
  RFENCE behind `PmapIf::shootdown_*`; this is not exposed as a scheduler
  policy hook.

### 18.2 What HAL does not specify (deferred to SMP_v1)
<!-- txdoc:HAL-SMPIF-LOW-LEVEL-MECHANICS-WHAT-HAL-DOES-NOT-SPECIFY-DEFERRED-TO-SMP-V1-1 -->

- CPU offline state machine and scheduler admission.
- Full AP runtime policy after the generic AP-local substrate/trap/online
  sequence (which queues to drain, which work to run, and when scheduler
  admission begins).
- Reschedule IPI protocol above low-level send/pending/ack mechanics (who sends,
  when, and how the receiver maps the wake to reactor or scheduler work).
- Kernel-managed TLB shootdown protocol details when a platform cannot rely on
  firmware RFENCE (the originator/recipient handshake).
- Stop-the-world protocol.
- Scheduler integration with IPIs.
- Reactor integration with IPIs.

`SMP_v1` will land as a separate document under tier 03 (execution).

### 18.3 v1 uniprocessor mode
<!-- txdoc:HAL-SMPIF-LOW-LEVEL-MECHANICS-V1-UNIPROCESSOR-MODE-1 -->

In v1 single-CPU builds:

- `possible_cpus()` returns `CpuMask(0b1)`.
- `online_cpus()` returns `CpuMask(0b1)` after the BSP calls `mark_cpu_online`.
- `boot_secondary_cpus()` is a no-op and returns `0`.
- `enable_ipi_wakeups()` is a no-op.
- `wait_for_interrupt_once()` may be a spin-loop fallback.
- `pending_ipi(_)` returns `false`.
- `send_ipi(target, _)` asserts if `target != current_cpu_id()`.
- `broadcast_ipi(mask, _)` asserts if `mask` has more than the current CPU bit set.
- `park_this_cpu()` enters a low-power wait loop.

This is enough to make the rest of the kernel work without conditional compilation around SMP-vs-uniprocessor. RV64 QEMU now exercises the SMP shape with `-smp 4`: DTB CPU discovery publishes `PlatformInfo.possible_cpu_count`, the platform starts APs through SBI HSM into a low trampoline, each AP installs early per-CPU state, initializes AP-local substrate epoch/zone state, installs the kernel trap vector, marks online, and enters a kernel-owned AP loop. That loop arms SSIP wakeup, waits with `wfi`, polls and acknowledges pending reschedule IPIs, and hands reschedule work back to reactor-owned runqueue draining. The pmap path can issue SBI RFENCE to those online APs, but final scheduler policy and kernel-managed IPI/ack shootdown remain deferred.

---

## 19. PowerIf
<!-- txdoc:HAL-POWERIF-1 -->

`PowerIf` is shutdown and reboot. It is small, but having it as a trait prevents shutdown paths from importing platform crates directly.

```rust
pub trait PowerIf {
    /// Power off the system. Diverges.
    /// RV64: SBI shutdown.
    /// LA64: ACPI/UEFI shutdown or PMON-specific shutdown port.
    /// QEMU: write to test device at platform-specific MMIO.
    fn system_off() -> !;

    /// Reboot the system. Diverges.
    /// RV64: SBI reboot or watchdog-triggered reset.
    /// LA64: ACPI reset register or board-specific reset.
    fn reboot() -> !;

    /// Park the current CPU permanently. Used for AP shutdown
    /// during normal operation. v1: same as SmpIf::park_this_cpu.
    fn cpu_off() -> !;
}
```

Used from:

- `kernel_main` if it ever returns (defensive — it shouldn't).
- The panic handler if `panic = "abort"` policy is shutdown rather than spin.
- The init reaper after the last process exits.
- Userspace `reboot(2)` / `power_off(2)` syscalls.

---

## 20. FpSimdIf — optional
<!-- txdoc:HAL-FPSIMDIF-OPTIONAL-1 -->

`FpSimdIf` is *optional*: a board crate may or may not implement it, and `TxPlatform` does *not* include it as a supertrait. Consumers that need FPU state save/restore reach for it directly.

```rust
pub trait FpSimdIf {
    /// True if the platform supports FPU/SIMD state.
    const SUPPORTED: bool;

    /// Architecture-specific FPU/SIMD state.
    type State: Default;

    /// Initialize a fresh state (zeros, default rounding mode, etc.).
    fn init_state() -> Self::State;

    /// Enable FPU/SIMD for the current hart. Called when a thread
    /// that needs FPU first executes an FPU instruction (lazy FPU,
    /// future) or eagerly at thread switch (v1).
    fn enable_for_current();

    /// Disable FPU/SIMD on the current hart.
    fn disable_for_current();

    /// Save the current FPU/SIMD state into the given storage.
    fn save(state: &mut Self::State);

    /// Restore FPU/SIMD state from the given storage.
    fn restore(state: &Self::State);
}
```

### 20.1 v1 default policy
<!-- txdoc:HAL-FPSIMDIF-OPTIONAL-V1-DEFAULT-POLICY-1 -->

If a platform does not implement `FpSimdIf`, user FPU/SIMD instructions trap as synchronous faults. `TrapIf::classify` returns `TrapClass::IllegalInstruction`; `KernelTrapSink::on_illegal_or_sync_fault` delivers SIGILL.

This is acceptable for v1 boot/init scenarios (the init userspace, busybox without FPU users) but will need to be replaced before glibc-linked programs run. Real lazy-FPU policy will land in a later doc.

### 20.2 Where FPU state lives
<!-- txdoc:HAL-FPSIMDIF-OPTIONAL-WHERE-FPU-STATE-LIVES-1 -->

When a platform does implement `FpSimdIf`, the FPU `State` is part of the per-thread context maintained by the scheduler/process subsystem. `TaskFrame` (the scheduler's per-thread struct) gets an `Option<<P as FpSimdIf>::State>` field. v1 keeps this `None`.

---

## 21. linkme registration discipline
<!-- txdoc:HAL-LINKME-REGISTRATION-DISCIPLINE-1 -->

This section codifies the rules for `linkme::distributed_slice` use across HAL and kernel.

### 21.1 The principle
<!-- txdoc:HAL-LINKME-REGISTRATION-DISCIPLINE-THE-PRINCIPLE-1 -->

`linkme` is a *registration source* mechanism. It enumerates static metadata at link time. It is not a dispatch mechanism. A linkme slice is enumerated *once*, typically at init time, to populate a real data structure (an installed dispatch table, a loaded init-hook list, a finalized fixup tree). Runtime dispatch uses the populated structure, not the slice.

This rule has one exception: `KERNEL_FIXUP_TABLE`, which is consulted from a trap path. But it is not a *handler* table; it is metadata about which kernel PCs have user-access fixup recoveries.

### 21.2 Approved slices
<!-- txdoc:HAL-LINKME-REGISTRATION-DISCIPLINE-APPROVED-SLICES-1 -->

```rust
// In tx-hal:

/// IRQ handler registrations. Devices register at link time;
/// device::init walks this once to build IrqDispatchTable, then
/// IrqIf::install_dispatch_table installs it. Runtime IRQ entry
/// indexes the installed table, NOT this slice.
#[linkme::distributed_slice]
pub static IRQ_HANDLERS: [IrqHandlerRegistration] = [..];

/// Init hooks. Used SPARINGLY for things that need link-time
/// enumeration (e.g., trace-schema registration). The hook list
/// is enumerated by kernel_main's init phase; subsystem init
/// ordering (substrate → vfs → ...) is sequenced by kernel_main,
/// NOT by INIT_HOOKS.
#[linkme::distributed_slice]
pub static INIT_HOOKS: [InitHook] = [..];

/// Kernel fixup table. Each copy_from_user / copy_to_user macro
/// emits a FixupEntry. Consulted from the kernel-mode trap path
/// when a fault hits a kernel PC. The trap path searches this
/// table by PC (binary search after init builds a sorted view).
/// Not a handler table.
#[linkme::distributed_slice]
pub static KERNEL_FIXUP_TABLE: [FixupEntry] = [..];

pub struct InitHook {
    pub name: &'static str,
    pub phase: InitPhase,
    pub run: fn(),
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum InitPhase {
    /// Run after substrate, before reactor. For things that need
    /// the heap but not the reactor (e.g., trace schema).
    PostSubstrate,
    /// Run after device::init, before exec::init_userspace.
    PostDevice,
}
```

### 21.3 Forbidden uses
<!-- txdoc:HAL-LINKME-REGISTRATION-DISCIPLINE-FORBIDDEN-USES-1 -->

- **Trap-cause dispatch.** Page fault, syscall, timer, IPI handlers are *not* linkme slices. They are direct calls into named methods on `KernelTrapSink<P>`. (§11.3.)

- **Syscall dispatch.** The syscall table is a fixed array indexed by syscall number. It is not a linkme slice. Adding a syscall means editing `tx_kernel/src/syscall/table.rs`, not adding a `#[distributed_slice]` somewhere.

- **Subsystem init ordering.** `substrate::init` must run before `vfs::init` must run before `device::init`. This ordering is encoded in `kernel_main` directly. INIT_HOOKS does not get to insert itself between substrate and vfs.

- **Page-fault handler chaining.** There is exactly one VM page-fault handler. If it can't resolve the fault, it returns to `KernelTrapSink::on_page_fault`, which dispatches to the synchronous-fault path. There is no chain of registered handlers.

### 21.4 Why these rules
<!-- txdoc:HAL-LINKME-REGISTRATION-DISCIPLINE-WHY-THESE-RULES-1 -->

If linkme were used for trap semantics, two things would go wrong:

1. **Ordering becomes implicit.** Slice element order depends on link order, which depends on Cargo dependency graph and feature flags. Trap dispatch must be deterministic.

2. **Subsystem boundaries leak.** If any crate can `#[distributed_slice(PAGE_FAULT_HANDLERS)]` at any time, the VM's ownership of page-fault semantics is not enforced.

Direct named calls keep semantic ownership enforceable. `KernelTrapSink` has exactly five methods, each with exactly one implementation in `tx-kernel`, each delegating to exactly one subsystem.

---

## 22. Per-arch implementation notes and required commitments
<!-- txdoc:HAL-PER-ARCH-IMPLEMENTATION-NOTES-AND-REQUIRED-COMMITMENTS-1 -->

This section is non-normative beyond the listed must-provide commitments. Concrete register details, MMIO base layouts, and assembly listings live in platform crate docs.

### 22.1 RISC-V 64
<!-- txdoc:HAL-PER-ARCH-IMPLEMENTATION-NOTES-AND-REQUIRED-COMMITMENTS-RISC-V-64-1 -->

The platform crate (e.g., `tx-hal-riscv64-qemu-virt`, `tx-hal-riscv64-visionfive2`) must:

- **Provide Sv39 or Sv48 PmapIf.** Sv39 for v1 (3-level page table, 39-bit virtual addresses). Sv48 may be supported as a feature flag on hardware that supports it. `PlatformConfig::PAGE_TABLE_LEVELS` distinguishes (3 vs 4).

- **Expose satp/asid discipline through PmapIf.** Each `AddressSpace` gets a `PmapRoot` that owns its top-level page-table page and an ASID. ASID assignment is platform-internal; the kernel sees only `Asid: Copy + Eq`.

- **Implement sfence.vma in `PmapIf::shootdown`.** Single-hart: `sfence.vma vaddr, asid` for each invalidation. Global shootdown uses `sfence.vma vaddr, x0` for cross-ASID invalidation. RV64 QEMU SMP additionally issues SBI `remote_sfence_vma` / `remote_sfence_vma_asid` to online remote harts; later `SMP_v1` owns any kernel-managed IPI/ack fallback.

- **Use sscratch for trap scratch.** The kernel's per-hart kernel stack pointer lives in sscratch during user-mode execution; trap entry swaps tp ↔ sscratch.

- **Implement IrqIf via PLIC.** PLIC base is platform-private. Claim/complete cycle uses PLIC's claim register.

- **Implement TimeIf via SBI / CLINT.** `read_ns` reads the `time` CSR; `set_deadline_ns` calls SBI `set_timer`.

- **Implement PowerIf via SBI.** SBI shutdown for `system_off`; SBI reset for `reboot`.

### 22.2 LoongArch 64
<!-- txdoc:HAL-PER-ARCH-IMPLEMENTATION-NOTES-AND-REQUIRED-COMMITMENTS-LOONGARCH-64-1 -->

The platform crate (e.g., `tx-hal-loongarch64-qemu-virt`, `tx-hal-loongarch64-2k1000la`) must:

- **Provide LA64 PmapIf.** 4-level page table (PWCH/PWCL configured for typical 48-bit VA). Direct map established via DMW (Direct Mapping Window) windows.

- **Expose DMW activation in H1.** Two DMW windows: one for the kernel direct map (cached), one for MMIO (uncached). DMW activation happens before MMU enable.

- **Implement invtlb in `PmapIf::shootdown`.** Single-hart: `invtlb 0x5, asid, vaddr` (invalidate by VA + ASID). `shootdown_global` uses `invtlb 0x6, x0, vaddr` (invalidate by VA, all ASIDs). Cross-hart deferred to SMP_v1.

- **Use $r21 for percpu.** `$r21` (a.k.a. `tp` in LA64 conventions) holds the per-CPU pointer in kernel mode. Trap entry preserves $r21 via the architecture's trap-frame save discipline.

- **Implement IrqIf via ExtIOI** (or LIOINTC on simpler platforms). Claim/complete cycle uses ExtIOI's claim register.

- **Implement TimeIf via stable timer.** `read_ns` reads `stable_counter`; `set_deadline_ns` writes the `tcfg` CSR.

- **Implement PowerIf via UEFI / firmware-specific port.** On 2K1000LA, this may be a board-specific shutdown register; on qemu-virt, the QEMU exit device.

---

## 23. Out of scope
<!-- txdoc:HAL-OUT-OF-SCOPE-1 -->

What this document does *not* specify:

- **Kernel-level SMP coordination.** AP online state machine, reschedule IPI protocol, TLB shootdown protocol, stop-the-world. Deferred to `SMP_v1.md` (tier 03).

- **Lazy FPU policy.** When FPUs become enabled, when state is saved, how `enable_for_current` interacts with task switch. Deferred until userspace runs FPU-using programs.

- **Page-out / swap.** PAGE_SUBSTRATE §1 commits to no-swap. HAL has no anti-thrashing or page-out interface because the kernel design does not have one.

- **Hot-add memory.** Same reasoning. Memory map is parsed once at boot; HAL has no add-region / remove-region surface.

- **Kernel-mode preemption.** The retired execution draft committed to cooperative async/await in kernelspace. HAL does not provide a "kernel preempt" primitive.

- **IOMMU surface.** v1 platforms have no IOMMU. `DmaIf::phys_to_dma` is identity. When an IOMMU-equipped platform lands, the trait stays the same; the impl changes.

- **PCIe / PCI bus.** PCIe ECAM ranges are listed in `PlatformInfo.mmio_regions` but PCI configuration access is the device subsystem's responsibility, not HAL's. v1 boards have no PCIe; tier-2 device init handles virtio-mmio directly.

- **High-precision tracing clocks.** Tracing uses `TimeIf::read_ns`. There is no separate fast-path tracing clock.

- **Crash-dump infrastructure.** Out of scope for v1.

---

## 24. References
<!-- txdoc:HAL-REFERENCES-1 -->

### Inside the project
<!-- txdoc:HAL-REFERENCES-INSIDE-THE-PROJECT-1 -->

- [`PAGE_SUBSTRATE_v1.md`](PAGE_SUBSTRATE_v1.md) — primary HAL consumer; §2 deliverables list is now realized as platform obligations here.
- [`DEVICE.md`](../06_devices/DEVICE.md) — tier-1 devices live in HAL; §7 phase table is the H4 expansion.
- [`INVARIANTS_v4.md`](../00_meta-framework/INVARIANTS_v4.md) — HAL/foundation dependency direction and TLB shootdown ordering; `PmapIf::shootdown` realizes the HAL side, while the wait-for-acks and post-shootdown frame accounting live in the substrate-side aggregator.
- [`00_meta-framework/SUBSYSTEM_ANATOMY_v2_1.md`](../00_meta-framework/SUBSYSTEM_ANATOMY_v2_1.md) — HAL is *not* a subsystem; it predates the four-module discipline.

### External (architectural inspiration only)
<!-- txdoc:HAL-REFERENCES-EXTERNAL-ARCHITECTURAL-INSPIRATION-ONLY-1 -->

- ArceOS axhal — static platform module organization: compile-time platform selection, platform-owned boot/trap/timer/IRQ/paging mechanics, no runtime HAL manager, and no HAL-owned semantic entities. HAL_v1 adopts this management style while spelling txKernel's stronger proof-object and trap-frame contracts as typed interfaces.
- ArceOS axplat — trait-based platform abstraction; useful as background for the trait-crate-plus-platform-crate split and two-stage init shape. We do not adopt the axplat trait surface itself, because `MemIf` is too thin for txKernel's pmap proof-object pattern, and axplat's BootInfo shape is incompatible with our `'static`-ref discipline.

### External (compatibility note)
<!-- txdoc:HAL-REFERENCES-EXTERNAL-COMPATIBILITY-NOTE-1 -->

- OSTD VM advisory — historical input only. The `BootInfo` / `PlatformInfo` split, the `TrapFrameView` / `TrapFrameMut` wrapper layer, and the `PmapReservation` proof-object pattern survive as txKernel interface ideas, but OSTD-style HAL management, `__ostd_main`, and manager-driven initialization do not.

---

## Appendix A: Migration from the implicit HAL contract
<!-- txdoc:HAL-APPENDIX-A-MIGRATION-FROM-THE-IMPLICIT-HAL-CONTRACT-1 -->

PAGE_SUBSTRATE_v1 §2 listed seven HAL deliverables. With HAL_v1, those are realized as trait obligations on `TxPlatform`:

| PAGE_SUBSTRATE §2 deliverable | HAL_v1 realization |
|---|---|
| `BootInfo` (static) | `BootInfoIf::boot_info()` returns `&'static BootInfo` (§7) |
| Bootstrap page table (satp live) | Established in H1 (§5.2); covered by `PlatformConfig::DIRECT_MAP_BASE` and the platform's H1 obligations |
| `PT_NODE_POOL` | `PmapIf::alloc_pt_node()` (§10.1); pool itself is platform-private during H1, then becomes the fallback after substrate installs the typed PT-node allocator in phase 7 |
| `PlatformInfo` | `PlatformInfoIf::platform_info()` returns `&'static PlatformInfo` (§8) |
| Early UART + logger | `ConsoleIf::write_bytes` (§9), live by end of H1 |
| Trap infrastructure | `TrapIf::install_kernel_trap_vector` (§11) called from kernel_main after init_later; minimal vector installed in H1 for panic |
| `PmapReservation` / `PmapCommitBatch` / `ShootdownBatch` (substrate-side) | `PmapIf::reserve` / `commit` / `shootdown` (HAL surface, §10.1) plus the proof-object types (§10.2–10.4); the substrate-side `ShootdownBatch` aggregator wraps HAL's `shootdown` per PAGE_SUBSTRATE §7.2 |

PAGE_SUBSTRATE §2's old ordering chain (`early_arch_init → ... → __ostd_main`) is replaced by the H0–H4 sequence in §5. `__ostd_main` is no longer named; the OSTD-style entry point was an artifact of the retired boot drafts.

PAGE_SUBSTRATE_v1 §2 has been rewritten as "HAL_v1 obligations consumed by substrate." Keep future substrate edits in that direction: substrate owns frame accounting and allocation; HAL owns only static platform facts and pmap/trap mechanics.
