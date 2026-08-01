//! IRQ dispatch plus deferred UART and virtio-net bottom halves.
//!
//! tx-kernel owns one global `IrqDispatchTable`. Boot-time
//! `install_irq_handlers::<P>()` populates the platform-declared UART, RTC,
//! and network slots, then publishes the table to the platform via
//! `<P as IrqIf>::install_dispatch_table`.
//!
//! Per Open Q #4 (`docs/progress/plans/2026-05-06-pre-elf-runtime-completion.md`)
//! registration is explicit, not linkme: tests can build a controlled
//! subset, boot ordering is preserved, and IRQ dispatch stays out of
//! linker-section magic.
//!
//! Per Open Q #6 the UART IRQ number flows through
//! `<P as IrqIf>::UART_IRQ`; tx-kernel never names a board constant
//! directly.
//!
//! # IRQ-context safety
//!
//! `uart_rx_irq_handler` is called inside an `enter_irq_context()` scope
//! (irq_depth > 0), so it must **not** call `epoch::guard()` — the
//! domain's `debug_assert!` would fire, and conceptually EBR guards are
//! illegal in interrupt handlers anyway (re-entrant guard creation would
//! stall reclaim indefinitely).
//!
//! UART copies FIFO bytes into an IRQ-safe buffer. Virtio-net publishes an
//! atomic deferred-claim slot and intentionally leaves controller completion
//! outstanding. The reactor drains both paths in task context; the net bottom
//! half ACKs the level-triggered device before completing the original claim
//! on its claimant hart.

use crate::adapter::step_engine::{spin_mutex, SpinMutex};
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use tx_hal::{
    ConsoleIf, CpuId, IrqDispatchTable, IrqHandled, IrqHandlerFn, IrqIf, TxPlatform,
    IRQ_DISPATCH_TABLE_SIZE,
};
use tx_services::time::{platform::HalRtcDevice, RtcDeviceOps as TimeRtcDeviceOps};
use tx_substrate::wake::MailboxSchedulerHint;
use tx_subsystems::device::RtcEventMask;

/// The single global IRQ dispatch table tx-kernel publishes to the
/// platform. The platform crate stores a raw `&'static
/// IrqDispatchTable` pointer through `IrqIf::install_dispatch_table`;
/// the `SpinMutex` here exists so `register_irq_handler` can mutate
/// the table from the boot path. Per Cross-cutting risk #4 in the
/// pre-ELF plan, registration runs strictly before `unmask`, so the
/// platform never sees a half-built table.
static IRQ_DISPATCH_TABLE: SpinMutex<IrqDispatchTable> = spin_mutex(
    IrqDispatchTable::new(),
    b"debug.lock.kernel.irq_dispatch_table",
);

/// Maximum bytes drained per UART RX IRQ. The 16550 RX FIFO is small;
/// this cap keeps a single IRQ from stalling the trap shell while
/// still draining a typical line in one shot.
const UART_RX_DRAIN_MAX: usize = 64;

/// Capacity of the deferred UART RX ring buffer.  Sized to hold several
/// full lines of shell input without overflow.
const UART_RX_PENDING_CAP: usize = 512;

/// Bytes received via UART RX IRQ that have not yet been ingested into
/// the TTY line discipline.  The IRQ handler fills this buffer (IRQ
/// context, no epoch guard allowed); the reactor loop drains it via
/// `drain_uart_rx_pending` (task context, epoch guard OK).
struct UartRxPending {
    bytes: [u8; UART_RX_PENDING_CAP],
    len: usize,
}

impl UartRxPending {
    const fn new() -> Self {
        Self {
            bytes: [0u8; UART_RX_PENDING_CAP],
            len: 0,
        }
    }
}

static UART_RX_PENDING: SpinMutex<UartRxPending> =
    spin_mutex(UartRxPending::new(), b"debug.lock.kernel.uart_rx_pending");

const DEFERRED_IRQ_IDLE: u8 = 0;
const DEFERRED_IRQ_PUBLISHING: u8 = 1;
const DEFERRED_IRQ_PENDING: u8 = 2;
const DEFERRED_IRQ_DRAINING: u8 = 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DeferredIrqClaim {
    irq: u32,
    owner: CpuId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DeferredIrqDrain {
    Idle,
    Owned(DeferredIrqClaim),
    WrongHart { owner: CpuId },
}

/// One hart's lock-free deferred controller claim.
///
/// Claim metadata is written only while `phase == Publishing`, published by
/// the Release transition to `Pending`, and immutable until the owning hart
/// moves it through `Draining` back to `Idle`. Keeping the fields separate
/// avoids opaque bit packing while the phase CAS prevents partial publication.
struct DeferredIrqSlot {
    phase: AtomicU8,
    irq: AtomicU32,
    owner: AtomicUsize,
}

impl DeferredIrqSlot {
    const fn new() -> Self {
        Self {
            phase: AtomicU8::new(DEFERRED_IRQ_IDLE),
            irq: AtomicU32::new(0),
            owner: AtomicUsize::new(0),
        }
    }

    fn publish(&self, claim: DeferredIrqClaim) {
        assert_ne!(claim.irq, 0, "deferred IRQ claim must be non-zero");
        assert_eq!(
            self.phase.compare_exchange(
                DEFERRED_IRQ_IDLE,
                DEFERRED_IRQ_PUBLISHING,
                Ordering::AcqRel,
                Ordering::Acquire,
            ),
            Ok(DEFERRED_IRQ_IDLE),
            "deferred IRQ slot already owns a controller claim",
        );
        self.irq.store(claim.irq, Ordering::Relaxed);
        self.owner.store(claim.owner.0, Ordering::Relaxed);
        self.phase.store(DEFERRED_IRQ_PENDING, Ordering::Release);
    }

    fn begin_drain(&self, current: CpuId) -> DeferredIrqDrain {
        if self.phase.load(Ordering::Acquire) != DEFERRED_IRQ_PENDING {
            return DeferredIrqDrain::Idle;
        }
        let owner = CpuId(self.owner.load(Ordering::Relaxed));
        if owner != current {
            return DeferredIrqDrain::WrongHart { owner };
        }
        if self
            .phase
            .compare_exchange(
                DEFERRED_IRQ_PENDING,
                DEFERRED_IRQ_DRAINING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return DeferredIrqDrain::Idle;
        }
        DeferredIrqDrain::Owned(DeferredIrqClaim {
            irq: self.irq.load(Ordering::Relaxed),
            owner,
        })
    }

    fn retry(&self, claim: DeferredIrqClaim) {
        self.assert_draining(claim);
        self.phase.store(DEFERRED_IRQ_PENDING, Ordering::Release);
    }

    /// Release software ownership immediately before controller completion.
    ///
    /// A new level assertion cannot be claimed until the outstanding
    /// controller transaction completes. Publishing `Idle` first therefore
    /// guarantees that a claim delivered immediately after `complete()` finds
    /// an available software slot.
    fn release_before_completion(&self, claim: DeferredIrqClaim) {
        self.assert_draining(claim);
        self.irq.store(0, Ordering::Relaxed);
        self.owner.store(0, Ordering::Relaxed);
        self.phase.store(DEFERRED_IRQ_IDLE, Ordering::Release);
    }

    fn assert_draining(&self, claim: DeferredIrqClaim) {
        debug_assert_eq!(self.phase.load(Ordering::Acquire), DEFERRED_IRQ_DRAINING);
        debug_assert_eq!(self.irq.load(Ordering::Relaxed), claim.irq);
        debug_assert_eq!(self.owner.load(Ordering::Relaxed), claim.owner.0);
    }

    #[cfg(test)]
    fn reset(&self) {
        self.irq.store(0, Ordering::Relaxed);
        self.owner.store(0, Ordering::Relaxed);
        self.phase.store(DEFERRED_IRQ_IDLE, Ordering::Release);
    }
}

/// A claim is published into the claimant hart's slot and drained by looking
/// up the current hart's slot. Other harts therefore observe `Idle` instead of
/// contending for one global slot or manufacturing false wrong-hart reports.
static NET_RX_DEFERRED_CLAIMS: [DeferredIrqSlot; tx_hal::MAX_HARTS] =
    [const { DeferredIrqSlot::new() }; tx_hal::MAX_HARTS];
static NET_IRQ_CLAIMS: AtomicU64 = AtomicU64::new(0);
static NET_IRQ_COMPLETIONS: AtomicU64 = AtomicU64::new(0);
static NET_IRQ_WRONG_HART_DRAINS: AtomicU64 = AtomicU64::new(0);
static NET_IRQ_MISSING_DEVICE_DRAINS: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct NetIrqStats {
    pub claims: u64,
    pub completions: u64,
    pub wrong_hart_drains: u64,
    pub missing_device_drains: u64,
}

pub(crate) fn net_irq_stats() -> NetIrqStats {
    NetIrqStats {
        claims: NET_IRQ_CLAIMS.load(Ordering::Acquire),
        completions: NET_IRQ_COMPLETIONS.load(Ordering::Acquire),
        wrong_hart_drains: NET_IRQ_WRONG_HART_DRAINS.load(Ordering::Acquire),
        missing_device_drains: NET_IRQ_MISSING_DEVICE_DRAINS.load(Ordering::Acquire),
    }
}

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

/// Test-only: clear the deferred UART RX buffer.
#[cfg(test)]
pub fn reset_pending_uart_rx_for_test() {
    let mut pending = UART_RX_PENDING.lock();
    pending.len = 0;
}

#[cfg(test)]
pub fn reset_pending_net_irq_for_test() {
    for slot in &NET_RX_DEFERRED_CLAIMS {
        slot.reset();
    }
    NET_IRQ_CLAIMS.store(0, Ordering::Release);
    NET_IRQ_COMPLETIONS.store(0, Ordering::Release);
    NET_IRQ_WRONG_HART_DRAINS.store(0, Ordering::Release);
    NET_IRQ_MISSING_DEVICE_DRAINS.store(0, Ordering::Release);
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
pub(crate) fn install_irq_handlers<P: TxPlatform>() {
    let uart_irq = <P as IrqIf>::UART_IRQ;
    register_irq_handler(uart_irq, uart_rx_irq_handler::<P>);
    let rtc_irq = <P as IrqIf>::RTC_IRQ;
    if rtc_irq != 0 {
        tx_fs::devfs::rtc_event_source_id();
        register_irq_handler(rtc_irq, rtc_alarm_irq_handler::<P>);
    }
    let net_irq = <P as IrqIf>::NET_IRQ;
    if net_irq != 0 {
        register_irq_handler(net_irq, net_rx_irq_handler::<P>);
    }
    <P as IrqIf>::install_dispatch_table(dispatch_table_static());
    <P as IrqIf>::set_priority(uart_irq, 1);
    <P as IrqIf>::unmask(uart_irq);
    if rtc_irq != 0 {
        <P as IrqIf>::set_priority(rtc_irq, 1);
        <P as IrqIf>::unmask(rtc_irq);
    }
    if net_irq != 0 {
        <P as IrqIf>::set_priority(net_irq, 1);
        <P as IrqIf>::unmask(net_irq);
    }
}

/// Virtio-net IRQ top half.
///
/// The driver ACK path takes locks, so IRQ context only publishes the claimed
/// IRQ and claimant hart. `DeferredWake` tells the trap dispatcher to leave
/// controller completion outstanding; the controller gateway then throttles
/// this source until [`drain_net_rx_irq`] clears the device and completes the
/// claim from the same hart.
pub fn net_rx_irq_handler<P: TxPlatform>(irq: u32) -> IrqHandled {
    assert_eq!(
        irq,
        <P as IrqIf>::NET_IRQ,
        "network handler received the wrong IRQ",
    );
    let owner = <P as tx_hal::SmpIf>::current_cpu_id();
    let slot = NET_RX_DEFERRED_CLAIMS
        .get(owner.0)
        .expect("network IRQ claimant hart exceeds MAX_HARTS");
    slot.publish(DeferredIrqClaim { irq, owner });
    NET_IRQ_CLAIMS.fetch_add(1, Ordering::AcqRel);
    IrqHandled::DeferredWake
}

/// Drain one deferred virtio-net claim in task context.
///
/// Returns `true` only after the device source was acknowledged, its queues
/// were polled, and the original controller claim was completed. A call from a
/// non-owner hart leaves the claim untouched for the claimant reactor.
pub(crate) fn drain_net_rx_irq<P: TxPlatform>() -> bool {
    let current = <P as tx_hal::SmpIf>::current_cpu_id();
    let Some(slot) = NET_RX_DEFERRED_CLAIMS.get(current.0) else {
        return false;
    };
    let claim = match slot.begin_drain(current) {
        DeferredIrqDrain::Idle => return false,
        DeferredIrqDrain::WrongHart { .. } => {
            NET_IRQ_WRONG_HART_DRAINS.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        DeferredIrqDrain::Owned(claim) => claim,
    };

    let Some(registration) = tx_subsystems::net::net_device_by_name(b"eth0") else {
        NET_IRQ_MISSING_DEVICE_DRAINS.fetch_add(1, Ordering::Relaxed);
        slot.retry(claim);
        return false;
    };

    let _ = registration.ops.ack_interrupt_and_fire();
    slot.release_before_completion(claim);
    <P as IrqIf>::complete(claim.irq);
    NET_IRQ_COMPLETIONS.fetch_add(1, Ordering::AcqRel);
    true
}

/// RTC alarm IRQ handler.
///
/// The RTC wait queue is initialized during [`install_irq_handlers`], before
/// the IRQ is unmasked, so this path only publishes pending device bits and
/// fires the existing wait source. It must not inspect `/dev` paths, open
/// RNodes, or run ioctl policy.
pub fn rtc_alarm_irq_handler<P: TxPlatform>(_irq: u32) -> IrqHandled {
    let _ = HalRtcDevice::<P>::new().acknowledge_alarm_irq();
    tx_fs::devfs::publish_rtc_event_with_post(RtcEventMask::ALARM, |mailbox, event| {
        crate::init::post_mailbox_ref_event_with_hint_from_current_hart::<P>(
            mailbox,
            event,
            MailboxSchedulerHint::Normal,
        )
    });
    IrqHandled::Wake
}

/// UART RX IRQ handler. Drains pending bytes from the platform
/// console FIFO via `ConsoleIf::read_bytes` and stores them in the
/// `UART_RX_PENDING` ring buffer for deferred processing by
/// `drain_uart_rx_pending` (called from the reactor loop in non-IRQ
/// context).
///
/// **IRQ-context safety**: this function runs with `irq_depth > 0` and
/// therefore must NOT call `epoch::guard()`.  All TTY line-discipline
/// work is deferred to `drain_uart_rx_pending`.
///
/// Returns `IrqHandled::Wake` when bytes were buffered (reactor should
/// reschedule the blocked `read` future).  Returns `IrqHandled::Done`
/// for spurious or already-drained IRQs.  Returns
/// `IrqHandled::NotMine` if the console TTY hasn't been registered yet
/// (defensive check against a stray pre-boot IRQ).
pub fn uart_rx_irq_handler<P: ConsoleIf>(_irq: u32) -> IrqHandled {
    let mut buf = [0u8; UART_RX_DRAIN_MAX];
    let n = <P as ConsoleIf>::read_bytes(&mut buf);
    if n == 0 {
        // Spurious IRQ or FIFO already drained.
        return IrqHandled::Done;
    }
    if crate::init::console_tty().is_none() {
        // Pre-boot race: TTY not yet registered. Discard the bytes and
        // return NotMine so the caller knows the IRQ was unexpected.
        return IrqHandled::NotMine;
    }
    // Buffer bytes for non-IRQ ingestion. Bytes that overflow the
    // pending buffer (UART_RX_PENDING_CAP) are silently dropped — this
    // is acceptable for a boot console where the reactor loop drains
    // frequently.
    let mut pending = UART_RX_PENDING.lock();
    let start = pending.len;
    let space = UART_RX_PENDING_CAP - start;
    let copy = n.min(space);
    let end = start + copy;
    pending.bytes[start..end].copy_from_slice(&buf[..copy]);
    pending.len = end;
    IrqHandled::Wake
}

/// Drain any bytes buffered by `uart_rx_irq_handler` into the boot
/// console TTY via `step_ingest`.
///
/// **Must be called from non-IRQ context** (irq_depth == 0) so that
/// creating an epoch guard is legal.  The reactor loop calls this after
/// every task-poll iteration (alongside `drain_sbi_console_into_tty`).
///
/// Returns number of bytes processed (0 if buffer was empty).
///
/// Note: when the FIFO/SBI buffer is large enough to swallow the entire
/// inbound chunk via [`drain_sbi_console_into_tty`] during the same WFI
/// wake, this returns 0 — the IRQ-deferred path is only exercised when
/// the SBI poll buffer fills first. See the 2026-05-13 sizing note on
/// `drain_sbi_console_into_tty` for why we keep both paths.
pub(crate) fn drain_uart_rx_pending<P: tx_hal::TxPlatform>() -> usize {
    // Snapshot and clear the pending buffer under the lock, then
    // release before calling step_ingest (which takes its own locks).
    let (bytes, n) = {
        let mut pending = UART_RX_PENDING.lock();
        if pending.len == 0 {
            return 0;
        }
        let mut snapshot = [0u8; UART_RX_PENDING_CAP];
        snapshot[..pending.len].copy_from_slice(&pending.bytes[..pending.len]);
        let n = pending.len;
        pending.len = 0;
        (snapshot, n)
    };

    crate::init::ingest_console_tty_bytes::<P>(&bytes[..n])
}

#[cfg(test)]
mod tests;
