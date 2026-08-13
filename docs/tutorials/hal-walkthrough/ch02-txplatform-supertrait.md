# Chapter 2 — The `TxPlatform` supertrait and zero-sized dispatch

## The problem a supertrait solves

The HAL is not one concern; it is a dozen. Boot, console, page mapping, traps,
signals, IRQs, time, per-CPU state, cache, DMA, SMP, power, entropy, tracing —
each is its own trait so a board author can reason about one axis at a time, and
so a consumer can ask for only the slice it needs. But the kernel mainline wants
to write *one* bound, `P: TxPlatform`, and have access to all of it.

That is what `TxPlatform` is: a **marker supertrait** that aggregates every axis.
It has no methods of its own. From `crates/tx-hal/src/lib.rs:1386`:

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
    + SignalFrameIf
    + IrqIf
    + TimeIf
    + PercpuIf
    + CacheIf
    + DmaIf
    + SmpIf
    + PowerIf
    + EntropyIf
    + ObserverIf
    + 'static
{
}

impl<T> TxPlatform for T where
    T: PlatformConfig + BootPlatformIf + InitIf + BootInfoIf + PlatformInfoIf
        + AuxvIf + ConsoleIf + PmapIf + TrapIf + SignalFrameIf + IrqIf + TimeIf
        + PercpuIf + CacheIf + DmaIf + SmpIf + PowerIf + EntropyIf + ObserverIf
        + 'static
{
}
```

The blanket impl is the important half: **any** type that implements all the
constituent traits *automatically* implements `TxPlatform`. The board author
never writes `impl TxPlatform for Platform {}`; they implement the axes, and
`TxPlatform`-ness falls out. That is also why a board can't accidentally claim to
be a platform while missing an axis — the trait bound simply won't be satisfied
and the binary won't link.

> **Divergence — what's actually in the supertrait.** The doc's listing
> (`txdoc:HAL-THE-TXPLATFORM-SUPERTRAIT-1`) does not include `EntropyIf` or
> `ObserverIf`; both were added after the doc was written. `EntropyIf` arrived
> with the CSPRNG/`AT_RANDOM` work (it has its own doc section, §13A) and
> `ObserverIf` is the tracing-ring hook. The doc also lists `PowerIf` with three
> methods (`system_off`, `reboot`, `cpu_off`); the shipped `PowerIf` has only
> `system_off()`. The intent — "one bound carries every axis" — is intact; the
> membership list drifted. See [Appendix A](appendix-a-design-vs-code-ledger.md).

## Why `'static`, and why a unit struct

`TxPlatform: 'static` is there because the platform type carries **no runtime
state**. Every method on every axis is an *associated function* — there is no
`&self`. The board exposes:

```rust
pub struct Platform;   // boards/tx-hal-riscv64-qemu-virt/src/lib.rs:44
```

A zero-sized unit struct. This is the entire "instance" of the platform. Because
the methods are associated functions, kernel code can write

```rust
let now = <P as TimeIf>::read_ns();
```

without ever holding a `Platform` value. There is nothing to hold — `Platform` is
zero bytes, and `read_ns` is a static call. Compare this to the runtime-vtable
HAL from Chapter 1: there, `hal->read_ns()` is a load-then-indirect-call; here,
`<P as TimeIf>::read_ns()` monomorphizes to a direct `call` (or is inlined
outright), because `P` is a concrete type known at compile time.

This is the payoff of static selection. The abstraction is *free*: you get the
full trait-based decoupling at authoring time, and at runtime there is no
function-pointer table, no dispatch, and no per-call cost beyond the work itself.

## The kernel mainline bound

The generic mainline declares the single bound and flows `P` through every HAL
call. The doc's idealized skeleton (`txdoc:HAL-THE-TXPLATFORM-SUPERTRAIT-THE-KERNEL-MAINLINE-BOUND-1`)
is:

```rust
pub fn kernel_main<P: TxPlatform>(handoff: BootHandoff) -> ! {
    P::init_early(handoff);
    substrate::init::<P>();
    P::init_later(handoff);
    // downstream subsystem init…
    P::system_off()
}
```

The real entry point in `crates/tx-kernel/src/lib.rs:67` is one line:

```rust
pub fn kernel_main<P: TxPlatform + 'static>(handoff: BootHandoff) -> ! {
    init::CoreInit::<P>::boot(handoff)
}
```

The linear skeleton has moved into `init::CoreInit::<P>::boot`, which is a much
richer sequence gated on a `SUBSTRATE_BOOT_READY` flag. We follow that real
sequence in [Chapter 6](ch06-handoff-shell-mainline.md). The structural point
stands: one type parameter `P`, threaded everywhere, monomorphized once per
board.

A subsystem that needs only part of the HAL declares a *narrower* bound. For
example the IRQ wiring takes exactly what it uses
(`crates/tx-kernel/src/irq.rs:159`):

```rust
pub(crate) fn install_irq_handlers<P: IrqIf + ConsoleIf>() { … }
```

This is good hygiene: the function's signature documents its hardware
dependencies, and you can substitute a test platform that implements only those
two axes.

## `FpSimdIf`: the axis that is deliberately *not* in the supertrait

One trait is conspicuously absent from `TxPlatform`: `FpSimdIf` (floating-point /
SIMD save-restore). This is intentional (`txdoc:HAL-THE-TXPLATFORM-SUPERTRAIT-1`).
A v1 board may compile without FPU support at all; if `FpSimdIf` were a
supertrait member, every board would be forced to implement it. Instead,
consumers that need FPU state reach for `FpSimdIf` *separately*, and a board's
`Platform` may or may not implement it.

The fallback when a board omits it is clean and is itself a HAL contract
(`txdoc:HAL-FPSIMDIF-OPTIONAL-V1-DEFAULT-POLICY-1`): a user FPU instruction traps
as an illegal instruction, `classify` returns `IllegalInstruction`, and the sink
delivers `SIGILL`. (As we'll see in Chapter 9, the RV64 board actually goes one
better and lazily *enables* the FPU on first use rather than killing the thread —
but the architectural escape hatch is the SIGILL path.)

The lesson generalizes: **optional capability → separate trait, not a supertrait
member.** Mandatory axes go in `TxPlatform`; anything a board may legitimately
lack stays out, and consumers bound on it explicitly.

## What you should take away

- `TxPlatform` is a methodless marker supertrait plus a blanket impl; a board
  earns it by implementing the axes.
- The platform is a zero-sized unit struct; all HAL methods are associated
  functions, so calls are static and free of vtable cost.
- Mandatory axes are supertrait members; optional ones (`FpSimdIf`) are reached
  for separately.

Next: [Chapter 3 — The address dialect](ch03-address-dialect.md), where we look
at how the HAL talks about physical and virtual addresses without ever conflating
"an address value" with "permission to dereference it."
</content>
