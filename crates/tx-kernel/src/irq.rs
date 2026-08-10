//! IRQ dispatch plus deferred UART and virtio-net bottom halves.
//!
//! tx-kernel owns one global `IrqDispatchTable`. Boot-time
//! `install_irq_handlers::<P>()` populates the platform-declared UART and RTC
//! slots, then publishes the table to the platform via
//! `<P as IrqIf>::install_dispatch_table`.
//!
//! Per Open Q #4 (`docs/progress/plans/2026-05-06-pre-elf-runtime-completion.md`)
//! registration is explicit, not linkme: tests can build a controlled
//! subset, boot ordering is preserved, and IRQ dispatch stays out of
//! linker-section magic.
//!
//! Tier-2 device IRQs flow through the boot-frozen typed device table rather
//! than platform-global accessors. tx-kernel never names a board constant.
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

pub mod device;

use crate::adapter::step_engine::{spin_mutex, SpinMutex};
use core::sync::atomic::{
    AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering,
};
use tx_hal::{
    ConsoleIf, CpuId, IrqDispatchTable, IrqHandled, IrqHandlerFn, IrqIf, TxPlatform,
    IRQ_DISPATCH_TABLE_SIZE,
};
use tx_services::time::{platform::HalRtcDevice, RtcDeviceOps as TimeRtcDeviceOps};
use tx_substrate::wake::MailboxSchedulerHint;
use tx_subsystems::device::RtcEventMask;
use tx_subsystems::device_binding::{BoundDeviceKey, BoundDeviceRegistration, DeviceIrqContext};
use tx_subsystems::net::NetDeviceRegistration;

/// Dispatch through the boot-frozen typed table first, then retain the legacy
/// HAL table as a migration fallback for UART, RTC, and not-yet-bound devices.
pub(crate) fn dispatch_external_irq<P: TxPlatform>(irq: u32) -> IrqHandled {
    if let Some(runtime) = crate::devices::runtime::device_runtime_snapshot() {
        let handled = runtime.outcome.irq_table.dispatch_irq(irq);
        if !matches!(handled, IrqHandled::NotMine) {
            return handled;
        }
    }
    P::dispatch_irq(irq)
}

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

/// Single task-context owner for the pending-buffer-to-TTY handoff.
///
/// Both BSP and AP reactors drain device bottom halves. The pending lock only
/// serializes the snapshot: without a wider owner, one hart can snapshot an
/// earlier chunk, lose the race into the line discipline, and submit it after
/// a later chunk. Contenders must not spin because the owner may be running on
/// another hart and may itself need cross-hart progress.
static CONSOLE_RX_INGEST_OWNED: AtomicBool = AtomicBool::new(false);

struct ConsoleRxIngestGuard;

impl Drop for ConsoleRxIngestGuard {
    fn drop(&mut self) {
        CONSOLE_RX_INGEST_OWNED.store(false, Ordering::Release);
    }
}

fn try_acquire_console_rx_ingest() -> Option<ConsoleRxIngestGuard> {
    CONSOLE_RX_INGEST_OWNED
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .ok()
        .map(|_| ConsoleRxIngestGuard)
}

fn pending_uart_rx_nonempty<P: IrqIf>() -> bool {
    let _local_execution = P::exclude_local_execution();
    UART_RX_PENDING.lock().len != 0
}

fn restore_pending_uart_rx_front<P: IrqIf>(bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    let _local_execution = P::exclude_local_execution();
    let mut pending = UART_RX_PENDING.lock();
    let restore_len = bytes.len().min(UART_RX_PENDING_CAP);
    let retained_new = pending.len.min(UART_RX_PENDING_CAP - restore_len);
    pending.bytes.copy_within(..retained_new, restore_len);
    pending.bytes[..restore_len].copy_from_slice(&bytes[..restore_len]);
    pending.len = restore_len + retained_new;
}

/// Non-blocking single-reader ownership for the platform console RX source.
///
/// The UART IRQ top half and the reactor's polling fallback both call
/// `ConsoleIf::read_bytes`.  On a 16550-style UART, checking `LSR.DR` and
/// consuming `RBR` are separate MMIO accesses.  Without one shared owner an
/// IRQ (or another hart) can consume `RBR` after a poller observed `DR`, then
/// the resumed poller reads the stale receive register and submits the byte a
/// second time.  IRQ context must never wait for task context, so contenders
/// skip this drain and let the current owner consume the FIFO.
static CONSOLE_RX_READER_OWNED: AtomicBool = AtomicBool::new(false);
static UART_RX_IRQ_DEFERRED_MASKED: AtomicBool = AtomicBool::new(false);

struct ConsoleRxReaderGuard;

impl Drop for ConsoleRxReaderGuard {
    fn drop(&mut self) {
        CONSOLE_RX_READER_OWNED.store(false, Ordering::Release);
    }
}

/// Try to drain bytes from the one platform console RX source.
///
/// Returns zero when another IRQ/hart/reactor poll currently owns the source.
/// Polling therefore remains available as a firmware/IRQ fallback without
/// allowing two consumers to overlap the platform's hardware read sequence.
enum ConsoleRxReadResult {
    Read(usize),
    Busy,
}

fn try_read_console_bytes_result<P: ConsoleIf>(buf: &mut [u8]) -> ConsoleRxReadResult {
    if buf.is_empty() {
        return ConsoleRxReadResult::Read(0);
    }
    if CONSOLE_RX_READER_OWNED
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        return ConsoleRxReadResult::Busy;
    }

    let _reader = ConsoleRxReaderGuard;
    ConsoleRxReadResult::Read(<P as ConsoleIf>::read_bytes(buf))
}

#[cfg(test)]
pub(crate) fn try_read_console_bytes<P: ConsoleIf>(buf: &mut [u8]) -> usize {
    match try_read_console_bytes_result::<P>(buf) {
        ConsoleRxReadResult::Read(n) => n,
        ConsoleRxReadResult::Busy => 0,
    }
}

const DEFERRED_IRQ_IDLE: u8 = 0;
const DEFERRED_IRQ_PUBLISHING: u8 = 1;
const DEFERRED_IRQ_PENDING: u8 = 2;
const DEFERRED_IRQ_DRAINING: u8 = 3;
#[derive(Clone, Copy)]
struct DeferredIrqClaim {
    irq: u32,
    owner: CpuId,
    bound: BoundDeviceKey,
    registration: &'static NetDeviceRegistration,
}

#[derive(Clone, Copy)]
enum DeferredIrqDrain {
    Idle,
    Owned(DeferredIrqClaim),
    WrongHart,
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
    bound: AtomicU32,
    registration: AtomicPtr<NetDeviceRegistration>,
}

impl DeferredIrqSlot {
    const fn new() -> Self {
        Self {
            phase: AtomicU8::new(DEFERRED_IRQ_IDLE),
            irq: AtomicU32::new(0),
            owner: AtomicUsize::new(0),
            bound: AtomicU32::new(0),
            registration: AtomicPtr::new(core::ptr::null_mut()),
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
        self.bound
            .store(u32::from(claim.bound.0), Ordering::Relaxed);
        self.registration.store(
            core::ptr::from_ref(claim.registration).cast_mut(),
            Ordering::Relaxed,
        );
        self.phase.store(DEFERRED_IRQ_PENDING, Ordering::Release);
    }

    fn begin_drain(&self, current: CpuId) -> DeferredIrqDrain {
        if self.phase.load(Ordering::Acquire) != DEFERRED_IRQ_PENDING {
            return DeferredIrqDrain::Idle;
        }
        let owner = CpuId(self.owner.load(Ordering::Relaxed));
        if owner != current {
            return DeferredIrqDrain::WrongHart;
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
        let registration = self.registration.load(Ordering::Relaxed);
        assert!(
            !registration.is_null(),
            "published deferred IRQ claim has no device registration"
        );
        DeferredIrqDrain::Owned(DeferredIrqClaim {
            irq: self.irq.load(Ordering::Relaxed),
            owner,
            bound: BoundDeviceKey(
                u16::try_from(self.bound.load(Ordering::Relaxed))
                    .expect("published bound-device key fits u16"),
            ),
            registration: unsafe { &*registration },
        })
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
        self.bound.store(0, Ordering::Relaxed);
        self.registration
            .store(core::ptr::null_mut(), Ordering::Relaxed);
        self.phase.store(DEFERRED_IRQ_IDLE, Ordering::Release);
    }

    fn assert_draining(&self, claim: DeferredIrqClaim) {
        debug_assert_eq!(self.phase.load(Ordering::Acquire), DEFERRED_IRQ_DRAINING);
        debug_assert_eq!(self.irq.load(Ordering::Relaxed), claim.irq);
        debug_assert_eq!(self.owner.load(Ordering::Relaxed), claim.owner.0);
        debug_assert_eq!(self.bound.load(Ordering::Relaxed), u32::from(claim.bound.0));
        debug_assert_eq!(
            self.registration.load(Ordering::Relaxed).cast_const(),
            core::ptr::from_ref(claim.registration),
        );
    }

    #[cfg(test)]
    fn reset(&self) {
        self.irq.store(0, Ordering::Relaxed);
        self.owner.store(0, Ordering::Relaxed);
        self.bound.store(0, Ordering::Relaxed);
        self.registration
            .store(core::ptr::null_mut(), Ordering::Relaxed);
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct NetIrqStats {
    pub claims: u64,
    pub completions: u64,
    pub wrong_hart_drains: u64,
    /// Retained for the observation record layout. Typed top halves resolve
    /// the registration before publishing a claim, so this remains zero.
    pub missing_device_drains: u64,
}

pub(crate) fn net_irq_stats() -> NetIrqStats {
    NetIrqStats {
        claims: NET_IRQ_CLAIMS.load(Ordering::Acquire),
        completions: NET_IRQ_COMPLETIONS.load(Ordering::Acquire),
        wrong_hart_drains: NET_IRQ_WRONG_HART_DRAINS.load(Ordering::Acquire),
        missing_device_drains: 0,
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
    CONSOLE_RX_READER_OWNED.store(false, Ordering::Release);
    CONSOLE_RX_INGEST_OWNED.store(false, Ordering::Release);
}

#[cfg(test)]
pub fn reset_pending_net_irq_for_test() {
    for slot in &NET_RX_DEFERRED_CLAIMS {
        slot.reset();
    }
    NET_IRQ_CLAIMS.store(0, Ordering::Release);
    NET_IRQ_COMPLETIONS.store(0, Ordering::Release);
    NET_IRQ_WRONG_HART_DRAINS.store(0, Ordering::Release);
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
    let uart_irq = <P as IrqIf>::uart_irq();
    if uart_irq != 0 {
        register_irq_handler(uart_irq, uart_rx_irq_handler::<P>);
    }
    let rtc_irq = <P as IrqIf>::rtc_irq();
    if rtc_irq != 0 {
        tx_fs::devfs::rtc_event_source_id();
        register_irq_handler(rtc_irq, rtc_alarm_irq_handler::<P>);
    }
    <P as IrqIf>::install_dispatch_table(dispatch_table_static());
    if uart_irq != 0 {
        <P as IrqIf>::set_priority(uart_irq, 1);
        <P as IrqIf>::unmask(uart_irq);
    }
    if rtc_irq != 0 {
        <P as IrqIf>::set_priority(rtc_irq, 1);
        <P as IrqIf>::unmask(rtc_irq);
    }
}

/// Per-device network top half used by the boot-frozen typed IRQ table.
/// Device ACK and queue polling remain in task context; the retained bound key
/// selects the exact registration without a namespace-name lookup.
pub fn typed_net_rx_irq_handler<P: TxPlatform>(
    context: &'static DeviceIrqContext,
    irq: u32,
) -> IrqHandled {
    assert_eq!(
        irq, context.route.resource.line,
        "typed network handler received the wrong IRQ",
    );
    let Some(registration) = crate::devices::runtime::device_runtime_snapshot()
        .and_then(|runtime| runtime.outcome.bound_devices.get(context.bound))
        .and_then(|bound| match bound.registration {
            BoundDeviceRegistration::Net(registration) => Some(registration),
            BoundDeviceRegistration::Char(_)
            | BoundDeviceRegistration::Block(_)
            | BoundDeviceRegistration::Controller => None,
        })
    else {
        return IrqHandled::NotMine;
    };
    publish_deferred_net_claim::<P>(irq, context.bound, registration)
}

fn publish_deferred_net_claim<P: TxPlatform>(
    irq: u32,
    bound: BoundDeviceKey,
    registration: &'static NetDeviceRegistration,
) -> IrqHandled {
    let owner = <P as tx_hal::SmpIf>::current_cpu_id();
    let slot = NET_RX_DEFERRED_CLAIMS
        .get(owner.0)
        .expect("network IRQ claimant hart exceeds MAX_HARTS");
    // PLIC keeps a claimed source unavailable until completion, but simple
    // level controllers such as the 2K1000 LIOINTC only expose STATUS & ENABLE.
    // Mask before deferring so the asserted device source cannot be claimed a
    // second time while this hart's one deferred slot still owns the first.
    <P as IrqIf>::mask(irq);
    slot.publish(DeferredIrqClaim {
        irq,
        owner,
        bound,
        registration,
    });
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
        DeferredIrqDrain::WrongHart => {
            NET_IRQ_WRONG_HART_DRAINS.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        DeferredIrqDrain::Owned(claim) => claim,
    };

    let _ = claim.registration.ops.ack_interrupt_and_fire();
    slot.release_before_completion(claim);
    <P as IrqIf>::complete(claim.irq);
    <P as IrqIf>::unmask(claim.irq);
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
/// Returns `IrqHandled::Wake` when bytes were buffered or the level source was
/// masked for task-context recovery. Returns `IrqHandled::Done` only for
/// spurious or already-drained IRQs.
/// The handler is installed only after the console TTY is published, so the
/// top half does not acquire the task-context console-cap lock.
enum ConsoleRxBufferResult {
    Buffered(usize),
    Empty,
    MustDefer,
}

fn buffer_console_rx<P: ConsoleIf, const DRAIN_MAX: usize>() -> ConsoleRxBufferResult {
    // IRQ context must never wait for a task-context holder on another hart.
    // Leave the FIFO untouched on contention; the top-half caller masks the
    // level source until task context releases the shared state and rearms it.
    let Some(mut pending) = UART_RX_PENDING.try_lock() else {
        return ConsoleRxBufferResult::MustDefer;
    };
    let space = UART_RX_PENDING_CAP - pending.len;
    if space == 0 {
        return ConsoleRxBufferResult::MustDefer;
    }
    let mut buf = [0u8; DRAIN_MAX];
    let n = match try_read_console_bytes_result::<P>(&mut buf[..space.min(DRAIN_MAX)]) {
        ConsoleRxReadResult::Read(n) => n,
        ConsoleRxReadResult::Busy => return ConsoleRxBufferResult::MustDefer,
    };
    if n == 0 {
        // Spurious IRQ or FIFO already drained.
        return ConsoleRxBufferResult::Empty;
    }
    // Buffer bytes for non-IRQ ingestion. Bytes that overflow the
    // pending buffer (UART_RX_PENDING_CAP) are silently dropped — this
    // is acceptable for a boot console where the reactor loop drains
    // frequently.
    let start = pending.len;
    let end = start + n;
    pending.bytes[start..end].copy_from_slice(&buf[..n]);
    pending.len = end;
    ConsoleRxBufferResult::Buffered(n)
}

pub fn uart_rx_irq_handler<P: TxPlatform>(irq: u32) -> IrqHandled {
    match buffer_console_rx::<P, UART_RX_DRAIN_MAX>() {
        ConsoleRxBufferResult::Buffered(_) => IrqHandled::Wake,
        ConsoleRxBufferResult::Empty => IrqHandled::Done,
        ConsoleRxBufferResult::MustDefer => {
            // The source is level-triggered. Leaving it enabled while the FIFO
            // is still asserted can trap-loop on the interrupted lock holder.
            UART_RX_IRQ_DEFERRED_MASKED.store(true, Ordering::Release);
            <P as IrqIf>::mask(irq);
            IrqHandled::Wake
        }
    }
}

/// Poll the console FIFO into the same ordered buffer used by the IRQ top
/// half. Direct polling must not bypass pending bytes and submit a newer chunk
/// to the TTY first.
pub(crate) fn poll_console_rx_into_pending<P: TxPlatform>() -> usize {
    // The polling fallback shares `UART_RX_PENDING` with the level-triggered
    // IRQ top half. Keep local IRQs excluded across the whole FIFO drain so an
    // interrupt cannot repeatedly re-enter while this context owns the pending
    // lock and leave the interrupted holder unable to resume.
    let _local_execution = <P as IrqIf>::exclude_local_execution();
    match buffer_console_rx::<P, UART_RX_PENDING_CAP>() {
        ConsoleRxBufferResult::Buffered(n) => n,
        ConsoleRxBufferResult::Empty | ConsoleRxBufferResult::MustDefer => 0,
    }
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
    // The boot console is a singleton routed to the boot hart on the supported
    // SMP platforms. Keep its deferred line-discipline and echo path on that
    // same hart: an AP may help run the reactor, but it must not take console
    // ownership between two CPU0 UART interrupts.
    if <P as tx_hal::SmpIf>::current_cpu_id().0 != 0 {
        return 0;
    }

    let Some(ingest_owner) = try_acquire_console_rx_ingest() else {
        return 0;
    };

    let mut ingest_owner = Some(ingest_owner);
    let mut processed = 0usize;
    loop {
        // Snapshot and clear the pending buffer under the lock, then release
        // before calling step_ingest (which takes its own locks). Keep the
        // wider ingest ownership until every chunk observed here is submitted,
        // so another reactor cannot overtake this one between snapshots.
        let Some((bytes, n)) = (|| {
            // The UART top half takes this same lock in IRQ context. Exclude
            // local interrupt execution while task context owns it, otherwise
            // a UART interrupt on this hart can spin forever on the interrupted
            // holder.
            let local_execution = <P as IrqIf>::exclude_local_execution();
            let mut pending = UART_RX_PENDING.lock();
            if pending.len == 0 {
                return None;
            }
            let mut snapshot = [0u8; UART_RX_PENDING_CAP];
            snapshot[..pending.len].copy_from_slice(&pending.bytes[..pending.len]);
            let n = pending.len;
            pending.len = 0;
            // End the IRQ-off section before the 512-byte snapshot is moved
            // into the closure result.
            drop(pending);
            drop(local_execution);
            Some((snapshot, n))
        })() else {
            // Release before the final recheck. A producer that appended
            // before this release may have woken a contender that observed us
            // as the owner and returned; the recheck either reclaims ownership
            // and drains that byte or observes a successor already doing so.
            drop(ingest_owner.take());
            if !pending_uart_rx_nonempty::<P>() {
                break;
            }
            let Some(next_owner) = try_acquire_console_rx_ingest() else {
                break;
            };
            ingest_owner = Some(next_owner);
            continue;
        };

        let consumed = crate::init::ingest_console_tty_bytes::<P>(&bytes[..n]);
        processed = processed.saturating_add(consumed);
        if consumed != n {
            restore_pending_uart_rx_front::<P>(&bytes[consumed.min(n)..n]);
            break;
        }
    }
    // No pending-buffer, reader, ingest, or TTY lock may remain held when the
    // level source is reopened. If the hardware FIFO is still non-empty, the
    // fresh IRQ can now make progress instead of re-entering a lock holder.
    drop(ingest_owner);
    if UART_RX_IRQ_DEFERRED_MASKED.swap(false, Ordering::AcqRel) {
        let irq = <P as IrqIf>::uart_irq();
        if irq != 0 {
            <P as IrqIf>::unmask(irq);
        }
    }
    processed
}

#[cfg(test)]
pub(crate) mod tests;
