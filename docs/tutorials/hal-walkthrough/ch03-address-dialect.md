# Chapter 3 — The address dialect: values vs dereference authority

## The traditional picture

In a C kernel, addresses are `uintptr_t`, `void *`, and `phys_addr_t` if you are
lucky enough to have a kernel that distinguishes them. The distinction between
"physical address," "kernel virtual address," and "user virtual address" lives in
naming conventions and the reviewer's memory. Pointer arithmetic is unrestricted;
`*(volatile u32 *)mmio_base` works whether or not `mmio_base` has been mapped.
The bugs this produces — dereferencing a physical address with the MMU on,
treating a user pointer as directly accessible, doing arithmetic across a mapping
boundary — are some of the nastiest in kernel engineering, because they
typecheck.

## The txKernel decision

The HAL owns the low-level address dialect, and it draws a hard line that the doc
states at `txdoc:HAL-PLATFORMCONFIG-…-ADDRESS-VALUES-AND-DEREFERENCE-BOUNDARY-1`:

> A `VirtAddr` does not become a Rust pointer by construction; it must pass
> through a named HAL, substrate, or user-access conversion that states which
> mapping is being used.

Addresses are **values**, not **dereference authority**. The types exist to make
the kind of address visible in the type system; turning one into something you
can load through is always an explicit, named step.

From `crates/tx-hal/src/lib.rs:87`:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhysAddr(pub usize);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Ppn(pub usize);          // physical page number

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VirtAddr(pub usize);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VirtRange { pub start: VirtAddr, pub size: usize }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhysRange { pub start: PhysAddr, pub size: usize }
```

These are newtypes over `usize`. They are `Copy` (cheap to pass), they implement
equality and `Debug`, and crucially they implement **no arithmetic traits**.
There is no `impl Add<usize> for VirtAddr`. The doc is explicit that "no broad
address arithmetic API (`Add<usize>` style) is part of the contract" — instead,
arithmetic is exposed as *named, checked* helpers: page alignment, page-index
extraction, and named conversions like "phys → direct-map" or "boot-linked →
kernel alias."

## The discipline, in three tiers

The same doc section spells out who may speak in raw addresses and who must not:

1. **pmap, boot, user-access, and page-substrate code may speak typed address
   values** — because address layout is literally their job.
2. **Ordinary kernel subsystems should speak semantic evidence** — `Cap<T>`,
   `Weak<T>`, `IdentRef<'g, T>`, witnesses, reservations, role tokens — *not* raw
   physical or virtual addresses. A filesystem does not handle a `PhysAddr`.
3. **Board code may name linker/static symbols only inside its boot-static
   capture surface** (the `BootStaticBag`, Chapter 5); the rest of the board
   consumes typed methods that return physical facts, high kernel pointers, or
   direct-map pointers.

That last tier is enforced mechanically. `cargo xtask lint arch` rejects
`addr_of!`, raw `UnsafeCell::get() as usize`, and Rust linker-symbol `extern`
blocks anywhere in the board crate *except* the boot-static surface. And
`cargo xtask lint unused` runs dead-code checks as hard errors and forbids
`#[allow(dead_code)]` / `#[allow(unused…)]` escape hatches in normal code — so a
half-built boot helper must be either live or explicitly test-gated, never
quietly allowed.

Once a conversion *has* produced a valid high-kernel `&T`, `&mut T`,
`NonNull<T>`, or raw pointer for a narrow unsafe operation, normal Rust pointer
rules resume. User memory is the standing exception: user virtual addresses stay
as `UserPtr<T>` and are touched only through the eager-walk
`AddressSpace::copy_*_user` family (Chapter 11).

## `UserPtr<T>`: the user-address newtype

The HAL ships one user-facing pointer type, `crates/tx-hal/src/lib.rs:857`:

```rust
#[repr(transparent)]
pub struct UserPtr<T>(*mut T);

impl<T> UserPtr<T> {
    pub fn new(addr: usize) -> Self { Self(addr as *mut T) }
    pub fn addr(self) -> usize { self.0 as usize }
    pub fn as_ptr(self) -> *mut T { self.0 }
}
```

`#[repr(transparent)]` so it is ABI-identical to a raw pointer, but the type
prevents a user VA from being passed where a kernel pointer is expected. Note
what it does *not* have: a `Deref`. You cannot `*user_ptr`. The only way to read
through it is the address-space user-copy path, which walks the user page tables
explicitly. That is the type system encoding "a user pointer is interpreted
against the *user* page table, which may not even be the one currently
installed."

## `PlatformConfig`: the compile-time facts

The address dialect's companion is `PlatformConfig` — a trait of *associated
constants* that pins the compile-time facts a subsystem needs to compute layouts,
strides, and masks. From `crates/tx-hal/src/lib.rs:277`, with the RV64 QEMU
values from `boards/tx-hal-riscv64-qemu-virt/src/lib.rs:269` filled in:

| Constant | RV64 QEMU value | Meaning |
|---|---|---|
| `ARCH` | `Arch::Riscv64` | architecture tag |
| `BOARD` | `"qemu-riscv64-virt"` | diagnostic id; also feeds the boot sentinel |
| `PAGE_SIZE` / `PAGE_SHIFT` | `4096` / `12` | (trait defaults) |
| `PHYS_ADDR_BITS` | `56` | Sv39 physical address width |
| `VIRT_ADDR_BITS` | `39` | Sv39 virtual address width |
| `DIRECT_MAP_BASE` | `0xffff_ffc0_0000_0000` | base VA of the kernel direct map |
| `DIRECT_MAP_SIZE` | `128 GiB` | upper bound on mappable RAM |
| `KERNEL_VIRT_BASE` | `0xffff_ffff_8020_0000` | base VA of the kernel image |
| `USER_TOP` | `0x40_0000_0000` | highest user VA + 1 |
| `USER_RESERVED_TOP_SIZE` | `4 MiB` | helper band below `USER_TOP` |
| `USER_ALLOC_TOP` | `USER_TOP − 4 MiB` | highest VA ordinary mmap/brk may use |
| `KERNEL_STACK_SIZE` | `128 KiB` | per-hart kernel stack |
| `PAGE_TABLE_LEVELS` | `3` | Sv39 is 3-level |
| `ASID_BITS` | `16` | RV64 ASID width |
| `CACHE_LINE_SIZE` | `64` | bytes |
| `DMA_COHERENT` | `true` | QEMU virt has coherent DMA |

A subsystem monomorphized over `<P: PlatformConfig>` references these as
`P::PAGE_SIZE`, `P::DIRECT_MAP_BASE`, etc. — compile-time constants, no coupling
to a specific board crate.

> **Divergence — `PlatformConfig` has trait defaults.** The doc presents
> `PlatformConfig` as a trait of bare associated constants (no defaults). The
> shipped trait gives defaults for most of them (`PAGE_SIZE = 4096`,
> `PHYS_ADDR_BITS = 0`, `DIRECT_MAP_BASE = VirtAddr(0)`, …) so a minimal or host
> platform can implement only `ARCH` + `BOARD` and compile. It also adds a
> constant the doc never mentions: `SUBSTRATE_BOOT_READY: bool = false`, which
> the real mainline uses to decide whether to run the full substrate-backed boot
> (Chapter 6). The RV64 board sets it `true`; a smoke-only board leaves it
> `false`.

### What deliberately does *not* go in `PlatformConfig`

The doc (`txdoc:HAL-PLATFORMCONFIG-…-WHAT-DOES-NOT-BELONG-…-1`) is as careful
about exclusions as inclusions. `PlatformConfig` is for *invariant, compile-time*
facts only. It does **not** carry:

- **MMIO base addresses** — those vary and go through `PlatformInfoIf::platform_info()`.
- **Interrupt numbers** — board-specific, delivered through device init tables.
- **Linker symbols** (`_kernel_start`, `_bss_start`) — platform-private,
  surfaced through `BootInfoIf` if needed.
- **Anything that varies between boots** of the same board.

The board crate is free to keep private `const PLIC_BASE: usize = …` for its own
use; the constant only graduates to `PlatformConfig` when cross-subsystem code
needs it.

## What you should take away

- `PhysAddr` / `VirtAddr` / `Ppn` / ranges are *value* types with no arithmetic
  and no `Deref`; producing a usable pointer is always a named conversion.
- `UserPtr<T>` is a non-dereferenceable, `repr(transparent)` user VA; user memory
  is reached only via the address-space copy family.
- `PlatformConfig` holds invariant compile-time facts as associated constants;
  variable facts (MMIO, IRQ numbers) live in `PlatformInfo` and device init.
- The "addresses are values, not authority" rule is enforced by `cargo xtask lint
  arch`, not just convention.

This closes Part I. Next, Part II opens with
[Chapter 4 — From firmware reset to Rust](ch04-firmware-to-rust.md), where these
typed addresses get built by hand in assembly, before the type system is even
available.
</content>
