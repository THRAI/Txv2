# Chapter 13 — Interrupts: explicit registration, not link-time magic

`IrqIf` is the HAL's interrupt-controller axis. On the RV64 QEMU `virt` board the
controller is the PLIC (Platform-Level Interrupt Controller). This chapter walks
the trait, the board's PLIC implementation, and the deliberate decision to
register IRQ handlers through an *explicit table* rather than the `linkme`
link-time-slice mechanism the codebase uses elsewhere — a decision the doc argues
for in seven points.

## The `IrqIf` surface

`crates/tx-hal/src/lib.rs:1134`:

```rust
pub trait IrqIf {
    const MAX_IRQ: u32 = 0;
    const UART_IRQ: u32 = 0;                 // board's console UART IRQ number

    fn in_irq_context() -> bool { false }
    fn interrupts_enabled() -> bool { true }
    fn claim() -> u32 { 0 }                  // PLIC claim → pending IRQ (0 = none)
    fn complete(_irq: u32) {}                // PLIC complete (EOI)
    fn mask(_irq: u32) {}
    fn unmask(_irq: u32) {}
    fn set_priority(_irq: u32, _priority: u8) {}
    fn install_dispatch_table(_table: &'static IrqDispatchTable) {}
    fn dispatch_irq(_irq: u32) -> IrqHandled { IrqHandled::Done }
}
```

The board fills these in over the PLIC MMIO (`boards/.../lib.rs:515`). `UART_IRQ`
is `10` ("QEMU `virt`'s 16550 UART is wired at PLIC IRQ 10. Source:
`qemu/hw/riscv/virt.c::UART0_IRQ`"). `claim`/`complete`/`mask`/`unmask`/`set_priority`
are thin wrappers over PLIC register pokes scoped to the current hart's PLIC
*context*.

The handler return type encodes the scheduler interaction
(`crates/tx-hal/src/lib.rs:1179`):

```rust
pub enum IrqHandled { Done, Wake, NotMine }
```

`Wake` means "a handler made a task runnable" — Chapter 9's `on_external_irq` turns
that into `TrapAction::Reschedule`; `Done`/`NotMine` become `Resume`.

## The handler table the kernel owns

The dispatch table is a HAL type (`crates/tx-hal/src/lib.rs:1186`), an array of
`Option<IrqHandlerFn>` of fixed size `IRQ_DISPATCH_TABLE_SIZE` (1024):

```rust
pub type IrqHandlerFn = fn(irq: u32) -> IrqHandled;
pub struct IrqDispatchTable { pub entries: [Option<IrqHandlerFn>; 1024] }
```

But the *table instance* lives in `tx-kernel`, not the board
(`crates/tx-kernel/src/irq.rs:45`):

```rust
static IRQ_DISPATCH_TABLE: SpinMutex<IrqDispatchTable> =
    spin_mutex(IrqDispatchTable::new(), b"debug.lock.kernel.irq_dispatch_table");
```

Handlers are added with `register_irq_handler(irq, handler)` (`irq.rs:85`), which
is idempotent on an identical fn-pointer and *panics on a conflicting* one. The
boot path calls it from `install_irq_handlers::<P>()` (`irq.rs:159`), which we met
in Chapter 6's boot sequence:

```rust
pub(crate) fn install_irq_handlers<P: IrqIf + ConsoleIf>() {
    let irq = <P as IrqIf>::UART_IRQ;
    register_irq_handler(irq, uart_rx_irq_handler::<P>);     // 1. register
    <P as IrqIf>::install_dispatch_table(dispatch_table_static());  // 2. publish to platform
    <P as IrqIf>::set_priority(irq, 1);                      // 3. prioritize
    <P as IrqIf>::unmask(irq);                               // 4. unmask LAST
}
```

The order is the safety property: every handler is registered *before* the table
is published, and the IRQ is unmasked *last*. The board's `install_dispatch_table`
(`lib.rs:558`) just stores the `&'static` pointer in an atomic and pokes the
16550's IER bit to make the device actually raise RX IRQs. Then `dispatch_irq`
(`lib.rs:572`) is a table lookup: valid IRQ → installed table → `entries[irq]` →
call it; unhandled → mask it and return `Done`.

## Why not `linkme`?

txKernel *does* use `linkme` (compiler-collected link-time slices) for some
registries — notably init hooks. So why build IRQ dispatch by hand with a
`SpinMutex` and explicit registration? The doc devotes a numbered argument to this
(`txdoc:HAL-IRQIF-…-WHY-NOT-LINKME-FOR-IRQ-DISPATCH-1`), and the kernel module's
header comment (`crates/tx-kernel/src/irq.rs:8`) cites it directly. The seven
points, condensed:

1. **Boot ordering must be explicit.** Unmask happens after registration; a
   link-time slice gives no control over *when* the table becomes live.
2. **Tests need controlled subsets.** A test can register exactly the handlers it
   wants (`reset_dispatch_table_for_test`); a `linkme` slice is whatever the link
   pulled in.
3. **Dispatch stays out of linker-section magic.** The hottest, most
   correctness-sensitive path (interrupt dispatch) shouldn't depend on
   section-collection semantics that vary by linker.
4. **Conflict detection is eager.** `register_irq_handler` panics on a conflicting
   handler at registration; a slice would silently take whatever's there.
5–7. (Observability, explicit ownership, and the general §21 rule that trap and
   syscall dispatch are *forbidden* from `linkme`.)

The broad §21 discipline: `linkme` is approved for *additive, order-independent*
registries (init hooks), and **forbidden** for anything on the trap/syscall/IRQ
dispatch path. IRQ dispatch is explicitly on the wrong side of that line.

## IRQ-context safety: the deferred-drain pattern

There's a subtle hazard the board's UART handler illustrates. Interrupt handlers
run with `irq_depth > 0`, and in that state the kernel forbids creating an
epoch-based-reclamation (EBR) guard — `epoch::guard()` would trip a
`debug_assert!`, and conceptually a re-entrant guard in an IRQ handler would stall
reclamation forever.

But ingesting a received byte into the TTY line discipline *needs* an epoch guard.
So `uart_rx_irq_handler` (`crates/tx-kernel/src/irq.rs:182`) does the minimum in
IRQ context and defers the rest:

```rust
pub fn uart_rx_irq_handler<P: ConsoleIf>(_irq: u32) -> IrqHandled {
    let mut buf = [0u8; UART_RX_DRAIN_MAX];
    let n = <P as ConsoleIf>::read_bytes(&mut buf);
    if n == 0 { return IrqHandled::Done; }                 // spurious / already drained
    if crate::init::console_tty().is_none() { return IrqHandled::NotMine; }
    // Buffer into a SpinMutex-protected ring; NO epoch guard here.
    let mut pending = UART_RX_PENDING.lock();
    /* copy buf into the pending ring, clamped to capacity */
    IrqHandled::Wake
}
```

The handler reads the FIFO into a `SpinMutex`-guarded ring buffer and returns
`Wake`. The actual line-discipline ingest happens later in
`drain_uart_rx_pending` (`irq.rs:222`), which the reactor loop calls between
task polls — *with* `irq_depth == 0`, so creating an epoch guard is legal:

```rust
pub(crate) fn drain_uart_rx_pending() -> usize {
    let (bytes, n) = { /* snapshot + clear the ring under the lock */ };
    let Some(tty) = crate::init::console_tty() else { return 0; };
    let guard = step_engine::guard();          // legal here: not in IRQ context
    match step_ingest(&tty, &bytes[..n], &guard) { /* … */ }
}
```

This split — **buffer in IRQ context, process in task context** — is the
general pattern for reconciling interrupt handlers with a reclamation scheme that
forbids guards in interrupt context. The `Wake` return is what schedules the
reactor to come do the deferred drain.

## What you should take away

- `IrqIf` is a thin PLIC-shaped trait (`claim`/`complete`/`mask`/`unmask`/
  `dispatch_irq`); `UART_IRQ = 10` flows the board's IRQ number to the kernel so
  no kernel code names a board constant.
- The dispatch table instance lives in `tx-kernel` behind a `SpinMutex`; handlers
  are registered explicitly, before publication, and the IRQ is unmasked last.
- IRQ dispatch is deliberately *not* `linkme`: boot order, test subsets, eager
  conflict detection, and the §21 rule keep trap/syscall/IRQ dispatch out of
  link-time slices.
- IRQ handlers buffer into a `SpinMutex` ring and return `Wake`; the reactor
  drains in task context where epoch guards are legal.

Next: [Chapter 14 — Time, per-CPU state, and SMP mechanics](ch14-time-percpu-smp.md).
</content>
