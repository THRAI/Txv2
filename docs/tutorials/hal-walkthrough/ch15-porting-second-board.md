# Chapter 15 — Porting to a second board

Everything in the previous fourteen chapters earns its keep at exactly one moment:
when you add a second board and discover how much of the kernel you *don't* have to
touch. This chapter uses the in-tree `tx-hal-riscv64-m1dock-mock` board as a
worked example of the minimum a new board must provide, and reframes the whole
HAL through "what changes, and what cannot."

## The mock board as a measuring stick

`boards/tx-hal-riscv64-m1dock-mock` models a Sipeed M1 Dock (a Kendryte K210-class
target) running under QEMU. It's a "mock" because it borrows QEMU `virt`'s SBI
console and timer rather than driving real K210 hardware — which is exactly what
makes it a clean teaching example: it shows the *structural* minimum without the
noise of a full new device set. It is three files (`lib.rs`, `pmap.rs`,
`pmap_tests.rs`), ~2,300 lines total, versus the QEMU virt board's much larger
surface.

The first thing to notice is how *similar* its `Platform` looks
(`boards/tx-hal-riscv64-m1dock-mock/src/lib.rs:175`):

```rust
impl PlatformConfig for Platform {
    const ARCH: Arch = Arch::Riscv64;
    const BOARD: &'static str = "sipeed-m1-dock-mock";
    const SUBSTRATE_BOOT_READY: bool = true;
    const DIRECT_MAP_BASE: VirtAddr = VirtAddr(pmap::DIRECT_MAP_BASE);
    const KERNEL_VIRT_BASE: VirtAddr = VirtAddr(pmap::KERNEL_VIRT_BASE);
    const PAGE_TABLE_LEVELS: u8 = 3;
    const ASID_BITS: u8 = pmap::RV64_ASID_BITS;
    // …
}
```

Same trait, same constants-shaped surface, different values namespaced under its
own `pmap` module. It declares a distinct `PlatformInfo` (with an `spi_sd` SD-card
fact the virt board doesn't have, and its own `MMIO_REGIONS`), its own
`BOOT_PROTOCOL`, and — because it sets `SUBSTRATE_BOOT_READY = true` — it runs the
full substrate-backed boot. A board that only wanted to prove the handoff works
would set it `false` and stop at the boot sentinel.

## The readiness ladder

The doc's portable-boot contract (`txdoc:HAL-THE-BOOT-SEQUENCE-PORTABLE-BOOT-CONTRACT-1`)
defines two rungs, and `SUBSTRATE_BOOT_READY` is the switch between them:

1. **Smoke-ready** (`SUBSTRATE_BOOT_READY = false`). The board must: set up a
   stack, clear BSS, preserve the boot registers, provide a console, install a
   minimal trap vector, and reach `rust_entry`. The mainline (Chapter 6) then
   emits `txkernel:<BOARD>:boot:ok` and shuts down. This proves the H0–H2 path
   without a frame allocator, MMU management, or any subsystem.
2. **Substrate-ready** (`SUBSTRATE_BOOT_READY = true`). The board additionally
   provides a working `PmapIf` (direct map + reserve/commit/shootdown), real
   per-CPU state, and the trap/timer/IRQ axes wired well enough for substrate,
   the reactor, and userspace to run. Now the full
   `init_substrate_if_ready` sequence executes.

A new port climbs this ladder: get to the sentinel first, then implement `PmapIf`
and flip the flag. The mock board is at the top rung; a brand-new silicon port
would spend most of its early life at the bottom.

## What a new board must provide

Concretely, to be `TxPlatform` a board crate implements (Chapter 2's supertrait
list): `PlatformConfig` + `BootPlatformIf` + `InitIf` + `BootInfoIf` +
`PlatformInfoIf` + `AuxvIf` + `ConsoleIf` + `PmapIf` + `TrapIf` + `SignalFrameIf` +
`IrqIf` + `TimeIf` + `PercpuIf` + `CacheIf` + `DmaIf` + `SmpIf` + `PowerIf` +
`EntropyIf` + `ObserverIf`. In practice the work clusters:

- **Boot (Chapters 4–5):** a linker script, a `_start` trampoline (or an
  equivalent for a non-MMU-bringup firmware), and a boot-fact capture surface. This
  is the most arch-specific code and usually the hardest.
- **Memory (Chapters 7–8):** the PTE format and the `PmapIf` impl. For a second
  RISC-V board this is largely shared logic with different constants (the mock
  board's `pmap.rs` mirrors the virt board's structure); for a different ISA it's
  a fresh PTE encoder.
- **Traps (Chapters 9–10):** the vector assembly, the concrete frame type, the
  `TrapFrameMut` vtable, and `enter_userspace_with_context` with its resume-context
  longjmp. Plus the binary's `#[no_mangle]` trap-dispatch bridge.
- **Everything else:** mostly thin wrappers over firmware/MMIO (`ConsoleIf`,
  `TimeIf`, `IrqIf`, `SmpIf`, `PowerIf`), several of which can lean on trait
  defaults at first.

The binary crate (Chapter 1) is *trivially* new: copy the 59-line `main.rs`,
change the `type ActivePlatform` alias and the trap-dispatch frame type/symbol.

## What a new board can *never* touch

This is the real payoff, and it's enforced, not aspirational:

- **`tx-kernel` has no `#[cfg(target_arch)]`** — the CI grep gate (Chapter 1).
  Adding a board adds *zero* conditionals to the generic kernel.
- **The kernel mainline is unchanged.** `CoreInit::boot` (Chapter 6) is generic
  over `P`; a new board flows through the identical sequence. The only board input
  to the sequence is `P::SUBSTRATE_BOOT_READY` and the trait method bodies.
- **No subsystem learns the board exists.** VFS, the scheduler, the process model,
  the syscall table — none gains a branch. They were written against `P: TxPlatform`
  (or narrower bounds like `P: IrqIf + ConsoleIf`), and monomorphize against the
  new `Platform` for free.
- **The boot sentinel proves it.** Because `boot_sentinel` prints `P::BOARD`
  (Chapter 6), a correct port emits `txkernel:<your-board>:boot:ok` from *generic*
  code — the proof that the selected-platform axis, not a hard-coded string, is in
  play.

The doc frames this as the whole point of the four-crate split: the cost of a port
is bounded to one board crate plus a trivial binary, and the generic kernel is
provably untouched.

## The leverage, restated through the chapters

Each earlier chapter's design decision was, in the end, about this moment:

- **Static dispatch (Ch 2)** means the new board's methods inline with zero
  runtime cost — no vtable to register, no manager to initialize.
- **The address dialect (Ch 3)** means the new board's pmap speaks typed
  `PhysAddr`/`VirtAddr`, and `cargo xtask lint arch` keeps its raw-pointer unsafe
  confined to the boot-static surface.
- **`BootStaticBag` typestate (Ch 5)** is a pattern the new board copies to make
  *its* identity-bridge transition safe by construction.
- **The shell/sink split (Ch 9)** means the new board writes a vector and a frame
  type; the kernel's entire trap *policy* (`KernelTrapDispatcher`) is reused.
- **The stackless-coroutine return (Ch 10)** is the one genuinely hard thing a new
  board must reimplement — but its *shape* (per-hart resume context, two asm
  helpers, an `apply_trap_action` match) is a template, not a redesign.

## What you should take away

- A new board is one board crate (the real work) plus a near-trivial binary; the
  generic kernel and every subsystem are untouched, and CI proves it.
- `SUBSTRATE_BOOT_READY` is the rung switch: get to `txkernel:<BOARD>:boot:ok`
  first (smoke-ready), then implement `PmapIf` and flip to substrate-ready.
- The mock board shows the structural minimum: same trait surface, board-namespaced
  constants, its own `PlatformInfo`/MMIO, reusing the QEMU SBI console.
- The hard parts of a port are boot, pmap, and the trap/userspace-entry path;
  everything else is thin firmware wrappers or trait defaults.

Continue to the appendices:
[Appendix A — Design-vs-code ledger](appendix-a-design-vs-code-ledger.md) and
[Appendix B — Debugging traps with `cargo xtask fault-decode`](appendix-b-fault-decode.md).
</content>
