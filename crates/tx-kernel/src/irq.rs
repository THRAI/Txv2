//! Pre-ELF Phase 5 (item 9): IRQ dispatch table + UART RX handler.
//!
//! tx-kernel owns one global `IrqDispatchTable`. Boot-time
//! `install_irq_handlers::<P>()` populates the UART slot with
//! `uart_rx_irq_handler::<P>`, publishes the table to the platform via
//! `<P as IrqIf>::install_dispatch_table`, then unmasks the IRQ.
//!
//! Per Open Q #4 (`docs/progress/plans/2026-05-06-pre-elf-runtime-completion.md`)
//! registration is explicit, not linkme: tests can build a controlled
//! subset, boot ordering is preserved, and IRQ dispatch stays out of
//! linker-section magic.
//!
//! Per Open Q #6 the UART IRQ number flows through
//! `<P as IrqIf>::UART_IRQ`; tx-kernel never names a board constant
//! directly.

use tx_hal::{
    ConsoleIf, IrqDispatchTable, IrqHandled, IrqHandlerFn, IrqIf, IRQ_DISPATCH_TABLE_SIZE,
};
use tx_subsystems::tty::execution::step_ingest;
use crate::adapter::step_engine::{self as step_engine, SpinMutex, StepOutcome};

/// The single global IRQ dispatch table tx-kernel publishes to the
/// platform. The platform crate stores a raw `&'static
/// IrqDispatchTable` pointer through `IrqIf::install_dispatch_table`;
/// the `SpinMutex` here exists so `register_irq_handler` can mutate
/// the table from the boot path. Per Cross-cutting risk #4 in the
/// pre-ELF plan, registration runs strictly before `unmask`, so the
/// platform never sees a half-built table.
static IRQ_DISPATCH_TABLE: SpinMutex<IrqDispatchTable> = SpinMutex::new(IrqDispatchTable::new());

/// Maximum bytes drained per UART RX IRQ. The 16550 RX FIFO is small;
/// this cap keeps a single IRQ from stalling the trap shell while
/// still draining a typical line in one shot.
const UART_RX_DRAIN_MAX: usize = 64;

/// Register `handler` as the dispatch entry for IRQ number `irq`.
///
/// Idempotent on identical handler; panics on conflict (same slot,
/// different fn pointer). The boot wiring calls this once per device
/// from `install_irq_handlers` before the IRQ is unmasked.
pub fn register_irq_handler(irq: u32, handler: IrqHandlerFn) {
    let idx = irq as usize;
    assert!(
        idx < IRQ_DISPATCH_TABLE_SIZE,
        "register_irq_handler: irq {irq} >= IRQ_DISPATCH_TABLE_SIZE ({IRQ_DISPATCH_TABLE_SIZE})",
    );
    let mut table = IRQ_DISPATCH_TABLE.lock();
    match table.entries[idx] {
        Some(existing) if (existing as *const ()) == (handler as *const ()) => {
            // Idempotent re-registration. Safe under boot retries.
        }
        Some(_) => {
            panic!("register_irq_handler: irq {irq} already registered to a different handler",)
        }
        None => table.entries[idx] = Some(handler),
    }
}

/// Test-only: clear the entire dispatch table. Pairs with the
/// kernel-test reset machinery that scrubs every global between
/// boot-wiring runs.
#[cfg(test)]
pub fn reset_dispatch_table_for_test() {
    let mut table = IRQ_DISPATCH_TABLE.lock();
    *table = IrqDispatchTable::new();
}

/// Snapshot the handler currently registered for `irq`, if any.
/// Test surface used to assert `install_irq_handlers` populated the
/// expected slots.
#[cfg(test)]
pub(crate) fn handler_for(irq: u32) -> Option<IrqHandlerFn> {
    let idx = irq as usize;
    if idx >= IRQ_DISPATCH_TABLE_SIZE {
        return None;
    }
    IRQ_DISPATCH_TABLE.lock().entries[idx]
}

/// `&'static IrqDispatchTable` reference the boot wiring publishes to
/// the platform. Returned through a borrow rather than a leaked
/// pointer because the table is a static.
fn dispatch_table_static() -> &'static IrqDispatchTable {
    // SAFETY: `IRQ_DISPATCH_TABLE` is a `'static` SpinMutex; the
    // platform installer only reads `entries`, never mutates the
    // mutex, so handing out a `&IrqDispatchTable` over the locked
    // body is safe — the mutex guard just bounds the lifetime of the
    // reference. We cast away the guard to expose `'static`; the
    // table itself lives forever and the platform stores the pointer
    // for the kernel lifetime.
    //
    // The `unsafe` block is justified because (a) the SpinMutex
    // wraps the table for write coordination, not aliasing — there
    // are no `&mut IrqDispatchTable` escapes outside
    // `register_irq_handler`'s critical section, which always runs
    // before `install_dispatch_table` per the boot ordering
    // (Cross-cutting risk #4 in the pre-ELF plan); (b) the only
    // platform-side reads happen from `dispatch_irq` after the table
    // is installed, by which time all handlers have been registered.
    let guard = IRQ_DISPATCH_TABLE.lock();
    let ptr: *const IrqDispatchTable = &*guard;
    unsafe { &*ptr }
}

/// Install all kernel IRQ handlers and publish the dispatch table to
/// the platform. One-shot; called from `init.rs` after
/// `register_console_hardware` has populated the `CONSOLE_TTY` slot.
pub(crate) fn install_irq_handlers<P: IrqIf + ConsoleIf>() {
    let irq = <P as IrqIf>::UART_IRQ;
    register_irq_handler(irq, uart_rx_irq_handler::<P>);
    <P as IrqIf>::install_dispatch_table(dispatch_table_static());
    <P as IrqIf>::set_priority(irq, 1);
    <P as IrqIf>::unmask(irq);
}

/// UART RX IRQ handler. Drains pending bytes from the platform
/// console via `ConsoleIf::read_bytes`, then ingests them into the
/// boot console TTY through `tty::execution::step_ingest`.
///
/// Returns `IrqHandled::Wake` on any byte ingested (so the trap shell
/// reschedules the reactor and the blocked `read` future re-polls).
/// Returns `IrqHandled::NotMine` if the console TTY hasn't been
/// registered yet (boot race; `install_irq_handlers` runs after
/// `register_console_hardware` so this should never happen in
/// production, but the defensive check keeps a stray pre-boot IRQ
/// from panicking).
pub fn uart_rx_irq_handler<P: ConsoleIf>(_irq: u32) -> IrqHandled {
    let mut buf = [0u8; UART_RX_DRAIN_MAX];
    let n = <P as ConsoleIf>::read_bytes(&mut buf);
    if n == 0 {
        // Spurious IRQ or already drained.
        return IrqHandled::Done;
    }
    let Some(tty) = crate::init::console_tty() else {
        return IrqHandled::NotMine;
    };
    let guard = step_engine::guard();
    use StepOutcome as V3Out;
    match step_ingest(&tty, &buf[..n], &guard) {
        V3Out::Done(outcome) => {
            if outcome.consumed > 0 {
                IrqHandled::Wake
            } else {
                IrqHandled::Done
            }
        }
        // step_ingest only ever returns Done or Err(NotLive). On the
        // hangup path the carrier is already retired; treat it as
        // Done so the trap shell completes the IRQ without
        // rescheduling.
        _ => IrqHandled::Done,
    }
}

#[cfg(test)]
mod tests;
