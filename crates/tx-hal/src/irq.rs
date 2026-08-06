//! Platform interrupt-controller contract and portable dispatch types.

use crate::{IrqResource, LocalExecutionGuard, PciFunctionId};

pub trait IrqIf {
    const MAX_IRQ: u32 = 0;

    /// Platform-specific IRQ number for the boot console UART.
    ///
    /// Static fallback for platforms whose UART IRQ is compile-time fixed.
    /// Firmware-discovered platform families override [`Self::uart_irq`].
    /// See `docs/progress/plans/2026-05-06-pre-elf-runtime-completion.md`
    /// §"Open questions #6".
    const UART_IRQ: u32 = 0;

    /// Runtime UART IRQ number. Boards with firmware discovery can override
    /// this while retaining `UART_IRQ` as a static fallback.
    fn uart_irq() -> u32 {
        Self::UART_IRQ
    }

    /// Resolve one enumerated PCI function's INTx pin through the selected
    /// platform interrupt topology. `pin` uses PCI configuration-space values
    /// 1..=4 for INTA..INTD; zero means that the function has no INTx pin.
    fn pci_intx_irq(_function: PciFunctionId, _pin: u8) -> Option<IrqResource> {
        None
    }

    /// Static fallback IRQ number for a wake-capable persistent-clock RTC.
    ///
    /// Firmware-discovered platform families may keep a compile-time board
    /// profile value here while returning `0` from [`Self::rtc_irq`] when the
    /// active DTB has no compatible alarm source. The generic kernel uses the
    /// runtime accessor; HAL itself must not know devfs or RTC userspace state.
    const RTC_IRQ: u32 = 0;

    /// Runtime persistent-clock IRQ number. A zero value means that the
    /// selected platform has no proven wake-alarm route.
    fn rtc_irq() -> u32 {
        Self::RTC_IRQ
    }

    fn in_irq_context() -> bool {
        false
    }

    /// Return whether the current execution is using an architecture trap
    /// stack.
    ///
    /// Synchronous exceptions such as syscalls are not IRQ context, but they
    /// still run on a small per-hart trap stack on stackless platforms.
    /// Substrates use this fact to defer destructor-heavy maintenance until
    /// control has returned to a normal kernel/reactor stack.
    fn in_trap_context() -> bool {
        Self::in_irq_context()
    }

    fn interrupts_enabled() -> bool {
        true
    }

    /// Save maskable local interrupt admission and disable it until the returned
    /// guard is dropped. This does not provide CPU affinity or NMI exclusion.
    fn exclude_local_execution() -> LocalExecutionGuard;

    fn claim() -> u32 {
        0
    }

    fn complete(_irq: u32) {}

    fn mask(_irq: u32) {}

    fn unmask(_irq: u32) {}

    fn set_priority(_irq: u32, _priority: u8) {}

    fn install_dispatch_table(_table: &'static IrqDispatchTable) {}

    fn dispatch_irq(_irq: u32) -> IrqHandled {
        IrqHandled::Done
    }
}

pub const IRQ_DISPATCH_TABLE_SIZE: usize = 1024;

pub type IrqHandlerFn = fn(irq: u32) -> IrqHandled;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IrqHandled {
    Done,
    Wake,
    /// The handler requested a reactor wake and retained ownership of the
    /// controller completion. It must later call [`IrqIf::complete`] exactly
    /// once from the same controller context that performed the claim.
    DeferredWake,
    NotMine,
}

pub struct IrqDispatchTable {
    pub entries: [Option<IrqHandlerFn>; IRQ_DISPATCH_TABLE_SIZE],
}

impl IrqDispatchTable {
    pub const SIZE: usize = IRQ_DISPATCH_TABLE_SIZE;

    pub const fn new() -> Self {
        Self {
            entries: [None; IRQ_DISPATCH_TABLE_SIZE],
        }
    }
}

impl Default for IrqDispatchTable {
    fn default() -> Self {
        Self::new()
    }
}
