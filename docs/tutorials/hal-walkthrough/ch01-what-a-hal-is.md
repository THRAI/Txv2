# Chapter 1 — What a HAL is, and what txKernel refuses to be

## The traditional picture

Every portable kernel needs a seam between the parts that are the same on every
machine (the scheduler, the VFS, the syscall table) and the parts that are not
(how you turn on the MMU, how you take a trap, how you talk to the timer). That
seam is the Hardware Abstraction Layer.

Historically a HAL is built one of a few ways, and each has a characteristic
failure mode:

- **`#ifdef` soup.** The arch-specific code is sprinkled through the kernel
  behind `#ifdef __riscv` / `#ifdef __loongarch`. There is no seam at all; the
  "portable" code is portable only because someone keeps adding cases. Adding a
  third architecture means auditing every file.
- **A runtime ops vtable.** A `struct hal_ops { void (*write_console)(...); ...
  }` is filled in at boot and passed around. This is real abstraction, but every
  call is an indirect call through a function pointer the compiler cannot see
  through, and the "which platform am I" question is answered at runtime — so the
  binary still contains every platform's code, and a bug in selection is a
  runtime bug.
- **A HAL manager object.** A global, late-bound service (`hal_init()` populates
  a singleton; everyone calls `hal->...`). This adds initialization ordering
  hazards: who is allowed to call the HAL before it is "up"? What owns the
  semantic objects the HAL allocated?

## The txKernel decision

txKernel takes a fourth path, borrowed in spirit from ArceOS's `axhal`/`axplat`:
**a statically selected platform family.** The decisions are normative and listed
in the doc at `txdoc:HAL-DESIGN-DECISIONS-1`. The shape:

1. One **trait crate** (`tx-hal`) defines the vocabulary: every HAL axis is a
   trait, plus the shared value types and proof objects. No machine code, no
   board constants.
2. Each board is **one platform crate** (`tx-hal-<arch>-<board>`) that implements
   those traits on a single unit struct `pub struct Platform;`.
3. **Platform selection is a `type` alias at the binary**, resolved at link time.
   One board crate → one kernel ELF. There is no `Box<dyn TxPlatform>`, no
   `match arch { … }`, no runtime manager, no `__ostd_main` handoff.

The phrase the doc uses for what is *banned* is worth memorizing, because the
rest of the codebase is organized to make these impossible
(`tx-hal-axhal` skill, "Preserve" list):

- no runtime HAL-manager type;
- no boxed dynamic HAL trait object;
- no `__ostd_main`;
- no HAL-owned semantic entities (the HAL does not own processes, files, or
  any user-visible object);
- no HAL callback slot for subsystem policy (the HAL never calls *up* into VM or
  the scheduler to ask what to do).

## The four-crate diamond

The doc draws it at `txdoc:HAL-CRATE-SPLIT-AND-PLATFORM-SELECTION-THE-FOUR-CRATE-PATTERN-1`,
and the tree matches exactly:

```
        tx-kernel-riscv64-qemu-virt   (binary crate)
                  /         \
         tx-kernel           tx-hal-riscv64-qemu-virt   (board crate)
                  \         /
                    tx-hal                              (trait crate)
                       |
                meta-framework primitive types
```

Why four crates and not three? Because the binary is the only place that is
allowed to depend on *both* the generic kernel and a concrete platform. If you
tried to fold the binary into the platform crate, the platform crate would have
to depend on `tx-kernel` (to call `kernel_main`), and `tx-kernel` already depends
on `tx-hal` which the platform crate also depends on — the diamond would close in
the wrong place and the "no path from `tx-hal` to `tx-kernel`" rule would break.
The binary is the seam, and it is tiny.

Here is the entire board binary, `boards/tx-kernel-riscv64-qemu-virt/src/main.rs`
(59 lines, reproduced nearly in full):

```rust
#![no_std]
#![no_main]

use core::panic::PanicInfo;
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

Read what the binary does and — more importantly — what it does *not* do. It:

- names exactly one `ActivePlatform`;
- implements `KernelMain<ActivePlatform>` by tail-calling the generic
  `tx_kernel::kernel_main::<ActivePlatform>`;
- exports the C-ABI `rust_entry` that the platform's `_start` assembly jumps to;
- provides the panic handler appropriate to a binary crate.

It does **not** define `_start`, linker symbols, MMIO constants, DTB parsing, or
any board-selection logic. The doc calls this "required board-binary minimalism"
(`txdoc:HAL-CRATE-SPLIT-AND-PLATFORM-SELECTION-BOARD-BINARY-MINIMALISM-1`), and it
is the rule that lets a future LA64 or VisionFive2 port reuse the generic kernel
mainline untouched.

> **Divergence — the trap-dispatch symbol.** The minimalism rule has exactly one
> real-world exception, and it lives in this same file. The binary also exports:
>
> ```rust
> #[no_mangle]
> pub extern "C" fn tx_kernel_riscv64_qemu_trap_dispatch(
>     frame: *mut tx_hal_riscv64_qemu_virt::Rv64TrapFrame,
> ) -> tx_hal::TrapAction {
>     let Some(frame) = (unsafe { frame.as_mut() }) else {
>         return tx_hal::TrapAction::Terminate;
>     };
>     tx_hal_riscv64_qemu_virt::dispatch_trap_frame::<tx_kernel::trap::KernelTrapDispatcher>(frame)
> }
> ```
>
> This is a named symbol the platform's trap-vector assembly calls into. The doc
> (`txdoc:HAL-TRAPIF-…-THE-SHELL-TO-SINK-CONTRACT-1`) imagined a generic
> `rust_trap_entry<P, K>` living *in the platform crate*; reality routes the trap
> through a `#[no_mangle]` symbol exported from the *binary*, which is the only
> crate that can name both the concrete `Rv64TrapFrame` and the kernel's
> `KernelTrapDispatcher`. We unpack this fully in
> [Chapter 9](ch09-trap-shell-to-sink.md); for now, just note that "the binary is
> minimal" is true *except* for the one symbol that has to see both sides of the
> diamond at once.

## Enforcing "no runtime dispatch"

The promise that there is no `#[cfg(target_arch)]` in the generic kernel is not
honor-system; it is a CI grep gate (`txdoc:HAL-CRATE-SPLIT-AND-PLATFORM-SELECTION-NO-RUNTIME-DISPATCH-1`):

```sh
grep -r "#\[cfg(target_arch" tx-kernel/
```

must come back empty (outside vendored low-level helpers). Combined with
`tx-hal` having no concrete arch code, and the binary being the only crate naming
a `Platform`, the type system and the linker together guarantee that all HAL
calls monomorphize through `P: TxPlatform`. There is genuinely nothing to
dispatch at runtime — `<P as TimeIf>::read_ns()` compiles to a direct call to the
board's timer read.

## What you should take away

- The HAL is a *family of traits*, selected once, at the binary, by a `type`
  alias.
- The generic kernel is parameterized over `P: TxPlatform` and contains no
  architecture conditionals.
- The board binary is the diamond's closing vertex and is deliberately tiny —
  with one pragmatic exception (the trap-dispatch symbol) that we will earn the
  right to understand by Chapter 9.

Next: [Chapter 2 — The `TxPlatform` supertrait and zero-sized dispatch](ch02-txplatform-supertrait.md),
where we look at *how* one type parameter can carry every hardware axis with zero
runtime cost.
</content>
