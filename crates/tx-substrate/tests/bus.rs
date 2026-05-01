use core::{ptr::NonNull, task::Waker};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    task::Wake,
};

use tx_hal::{IrqIf, PercpuIf, SmpIf};
use tx_substrate::bus::{
    retire_wire_owner, DeclaredPort, DeclaredQueue, DeclaredSubscriptionError,
    DeclaredSubscriptionGraphKey, DeclaredWireError, RawPort, RawQueue, RawSubscriptionError,
    RawSubscriptionState, RawTrace, RawWireError, StaticRawPort, StaticRawQueue, SubscriptionGraph,
    SubscriptionGraphError, SubscriptionGraphReady, TraceDeclaration, TracePayload,
    WireDeclaration, WireDeclarationError, WireEventSet, WireKind, WireOwnerManifest,
    WireOwnerReclaimError, WireOwnerRetireFence,
};
use tx_substrate::epoch;

static EPOCH_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
static OWNER_RECLAIM_COUNT: AtomicUsize = AtomicUsize::new(0);
static STATIC_TEST_QUEUE: StaticRawQueue = StaticRawQueue::new();
static STATIC_TEST_PORT: StaticRawPort = StaticRawPort::new();
static STATIC_TYPED_QUEUE: StaticRawQueue = StaticRawQueue::new();
static STATIC_TYPED_PORT: StaticRawPort = StaticRawPort::new();

struct TestPlatform;

impl PercpuIf for TestPlatform {}
impl IrqIf for TestPlatform {}
impl SmpIf for TestPlatform {}

struct CountWake {
    wakes: Arc<AtomicUsize>,
}

struct ManifestOwner {
    queue: RawQueue,
    port: RawPort,
}

struct MacroManifestOwner {
    queue: DeclaredQueue<Readiness>,
    port: DeclaredPort<Lifecycle>,
}

unsafe impl WireOwnerManifest for ManifestOwner {
    fn retire_embedded_wires(
        &self,
        guard: &epoch::Guard<'_>,
    ) -> Result<WireOwnerRetireFence, WireOwnerReclaimError> {
        let mut fence = WireOwnerRetireFence::from_retirement(self.queue.retire(0x1, guard))?;
        fence.include(self.port.retire(0x2, guard))?;
        Ok(fence)
    }

    unsafe fn reclaim_owner(owner: NonNull<Self>) {
        unsafe {
            drop(Box::from_raw(owner.as_ptr()));
        }
        OWNER_RECLAIM_COUNT.fetch_add(1, Ordering::AcqRel);
    }
}

tx_substrate::bus::bus_wire_owner_manifest! {
    unsafe impl WireOwnerManifest for MacroManifestOwner {
        reclaim_owner(owner) {
            unsafe {
                drop(Box::from_raw(owner.as_ptr()));
            }
            OWNER_RECLAIM_COUNT.fetch_add(1, Ordering::AcqRel);
        }

        wires {
            queue => retire(Readiness::BROKEN);
            port => retire(Lifecycle::GONE);
        }
    }
}

impl Wake for CountWake {
    fn wake(self: Arc<Self>) {
        self.wakes.fetch_add(1, Ordering::SeqCst);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.wakes.fetch_add(1, Ordering::SeqCst);
    }
}

fn counting_waker(wakes: Arc<AtomicUsize>) -> Waker {
    Waker::from(Arc::new(CountWake { wakes }))
}

fn assert_send_sync<T: Send + Sync>() {}

fn reset_epoch() -> std::sync::MutexGuard<'static, ()> {
    let guard = EPOCH_TEST_LOCK.lock().expect("epoch test lock");
    unsafe {
        epoch::testing::reset_for_test();
    }
    OWNER_RECLAIM_COUNT.store(0, Ordering::Release);
    epoch::init_on_bsp::<TestPlatform>().expect("epoch init");
    guard
}

unsafe fn count_owner_reclaim(_ptr: *mut u8) {
    OWNER_RECLAIM_COUNT.fetch_add(1, Ordering::AcqRel);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Readiness(u64);

impl Readiness {
    const HAS_DATA: Self = Self(0x1);
    const BROKEN: Self = Self(0x2);
    const BOTH: Self = Self(Self::HAS_DATA.0 | Self::BROKEN.0);
    const INVALID: Self = Self(0x4);
}

impl WireEventSet for Readiness {
    const DECLARED_BITS: u64 = Readiness::BOTH.0;

    fn bits(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Lifecycle(u64);

impl Lifecycle {
    const EXITED: Self = Self(0x1);
    const GONE: Self = Self(0x2);
    const ALL: Self = Self(Self::EXITED.0 | Self::GONE.0);
    const INVALID: Self = Self(0x8);
}

impl WireEventSet for Lifecycle {
    const DECLARED_BITS: u64 = Lifecycle::ALL.0;

    fn bits(self) -> u64 {
        self.0
    }
}

const MACRO_HAS_DATA: u16 = 0x1;
const MACRO_BROKEN: u16 = 0x2;
const MACRO_EXITED: u32 = 0x1;
const MACRO_GONE: u32 = 0x2;

tx_substrate::bus::bus_readiness! {
    struct MacroReadiness {
        const HAS_DATA = MACRO_HAS_DATA;
        const BROKEN = MACRO_BROKEN;
    }
}

tx_substrate::bus::bus_lifecycle! {
    struct MacroLifecycle {
        const EXITED = MACRO_EXITED;
        const GONE = MACRO_GONE;
    }
}

tx_substrate::bus::bus_tracepoint! {
    pub struct MacroTracePayload {
        pub pid: u32,
        pub code: u16,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ManualTracePayload {
    ino: u64,
    flags: u32,
}

impl TracePayload for ManualTracePayload {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct EmptyDeclaration(u64);

impl WireEventSet for EmptyDeclaration {
    const DECLARED_BITS: u64 = 0;

    fn bits(self) -> u64 {
        self.0
    }
}

#[test]
fn raw_wires_are_send_sync_for_cross_hart_wake_paths() {
    assert_send_sync::<RawQueue>();
    assert_send_sync::<RawPort>();
    assert_send_sync::<StaticRawQueue>();
    assert_send_sync::<StaticRawPort>();
    assert_send_sync::<DeclaredQueue<Readiness>>();
    assert_send_sync::<DeclaredPort<Lifecycle>>();
    assert_send_sync::<SubscriptionGraph<4>>();
    assert_send_sync::<DeclaredSubscriptionGraphKey<Readiness>>();
    assert_send_sync::<TraceDeclaration<ManualTracePayload>>();
    assert_send_sync::<RawTrace<ManualTracePayload>>();
    assert_send_sync::<WireOwnerRetireFence>();
}

#[test]
fn static_raw_queue_storage_produces_cloneable_raw_handles_without_arc_allocation() {
    let queue = STATIC_TEST_QUEUE.raw();
    let queue_from_static = RawQueue::from_static(&STATIC_TEST_QUEUE);
    let wakes = Arc::new(AtomicUsize::new(0));
    let mut subscription = queue.subscribe(0x1, counting_waker(Arc::clone(&wakes)));

    assert_eq!(queue_from_static.fire(0x1), 1);
    assert_eq!(wakes.load(Ordering::SeqCst), 1);
    assert!(subscription.take_ready());
    assert_eq!(queue.peek(), 0x1);

    queue.clear(0x1);
    assert_eq!(queue_from_static.peek(), 0);
    drop(subscription);
}

#[test]
fn static_raw_port_storage_produces_cloneable_raw_handles_without_arc_allocation() {
    let port = STATIC_TEST_PORT.raw();
    let port_from_static = RawPort::from_static(&STATIC_TEST_PORT);
    let wakes = Arc::new(AtomicUsize::new(0));
    let mut subscription = port.subscribe(0x1, counting_waker(Arc::clone(&wakes)));

    assert_eq!(port_from_static.fire(0x1), 1);
    assert_eq!(wakes.load(Ordering::SeqCst), 1);
    assert!(subscription.take_ready());
    assert_eq!(port.fire(0x1), 1);
    assert_eq!(wakes.load(Ordering::SeqCst), 2);
    drop(subscription);
}

#[test]
fn declared_static_queue_and_port_validate_events_over_static_storage() {
    let queue = DeclaredQueue::from_static(
        &STATIC_TYPED_QUEUE,
        WireDeclaration::<Readiness>::queue("static.readable"),
    )
    .expect("valid static queue declaration");
    let port = DeclaredPort::from_static(
        &STATIC_TYPED_PORT,
        WireDeclaration::<Lifecycle>::port("static.lifecycle"),
    )
    .expect("valid static port declaration");
    let queue_wakes = Arc::new(AtomicUsize::new(0));
    let port_wakes = Arc::new(AtomicUsize::new(0));
    let mut queue_subscription = queue.subscribe(
        Readiness::HAS_DATA,
        counting_waker(Arc::clone(&queue_wakes)),
    );
    let mut port_subscription =
        port.subscribe(Lifecycle::EXITED, counting_waker(Arc::clone(&port_wakes)));

    assert_eq!(queue.fire(Readiness::HAS_DATA), 1);
    assert_eq!(port.fire(Lifecycle::EXITED), 1);
    assert_eq!(queue_wakes.load(Ordering::SeqCst), 1);
    assert_eq!(port_wakes.load(Ordering::SeqCst), 1);
    assert!(queue_subscription.take_ready());
    assert!(port_subscription.take_ready());
    assert_eq!(
        queue.try_fire(Readiness::INVALID),
        Err(DeclaredWireError::Declaration(
            WireDeclarationError::UndeclaredBits
        ))
    );
    assert_eq!(
        port.try_fire(Lifecycle::INVALID),
        Err(DeclaredWireError::Declaration(
            WireDeclarationError::UndeclaredBits
        ))
    );

    queue.clear(Readiness::HAS_DATA);
    drop(queue_subscription);
    drop(port_subscription);
}

#[test]
fn declared_queue_exposes_metadata_and_rejects_undeclared_bits() {
    let declaration = WireDeclaration::<Readiness>::queue("pipe.read_wq");
    let queue = DeclaredQueue::new(declaration).expect("valid queue declaration");
    let wakes = Arc::new(AtomicUsize::new(0));
    let mut subscription = queue
        .try_subscribe(Readiness::HAS_DATA, counting_waker(Arc::clone(&wakes)))
        .expect("declared interest");

    assert_eq!(queue.declaration().name(), "pipe.read_wq");
    assert_eq!(queue.declaration().kind(), WireKind::Queue);
    assert_eq!(queue.declaration().declared_bits(), Readiness::BOTH.0);
    assert_eq!(
        queue.try_fire(Readiness::INVALID),
        Err(DeclaredWireError::Declaration(
            WireDeclarationError::UndeclaredBits
        ))
    );
    assert_eq!(queue.fire(Readiness::HAS_DATA), 1);
    assert_eq!(queue.peek_bits(), Readiness::HAS_DATA.0);
    assert_eq!(wakes.load(Ordering::SeqCst), 1);
    assert!(subscription.take_ready());
    assert_eq!(
        subscription.try_update(
            Readiness::INVALID,
            counting_waker(Arc::new(AtomicUsize::new(0)))
        ),
        Err(DeclaredSubscriptionError::Declaration(
            WireDeclarationError::UndeclaredBits
        ))
    );

    queue.clear(Readiness::HAS_DATA);
    assert_eq!(queue.peek_bits(), 0);
    assert_eq!(queue.terminate(Readiness::BROKEN), 1);
    assert_eq!(subscription.state(), RawSubscriptionState::Terminal);
}

#[test]
fn declared_port_wraps_edge_delivery_and_terminal_state() {
    let port = DeclaredPort::new(WireDeclaration::<Lifecycle>::port("process.exit_port"))
        .expect("valid port declaration");
    let wakes = Arc::new(AtomicUsize::new(0));
    let late_wakes = Arc::new(AtomicUsize::new(0));
    let mut subscription = port
        .try_subscribe(Lifecycle::EXITED, counting_waker(Arc::clone(&wakes)))
        .expect("declared interest");

    assert_eq!(port.declaration().kind(), WireKind::Port);
    assert_eq!(
        port.try_fire(Lifecycle::INVALID),
        Err(DeclaredWireError::Declaration(
            WireDeclarationError::UndeclaredBits
        ))
    );
    assert_eq!(port.fire(Lifecycle::EXITED), 1);
    assert_eq!(port.fire(Lifecycle::EXITED), 0);
    assert_eq!(wakes.load(Ordering::SeqCst), 1);
    assert!(subscription.take_ready());

    assert_eq!(port.terminate(Lifecycle::GONE), 1);
    assert_eq!(subscription.state(), RawSubscriptionState::Terminal);
    assert_eq!(
        port.try_subscribe(Lifecycle::EXITED, counting_waker(Arc::clone(&late_wakes)))
            .err(),
        Some(DeclaredWireError::Raw(RawWireError::Terminal))
    );
    assert_eq!(late_wakes.load(Ordering::SeqCst), 0);

    let terminal_subscription =
        port.subscribe(Lifecycle::EXITED, counting_waker(Arc::clone(&late_wakes)));
    assert_eq!(
        terminal_subscription.state(),
        RawSubscriptionState::Terminal
    );
    assert_eq!(late_wakes.load(Ordering::SeqCst), 1);
}

#[test]
fn declaration_macros_generate_typed_queue_and_port_event_sets() {
    assert_send_sync::<MacroReadiness>();
    assert_send_sync::<MacroLifecycle>();

    let readiness = MacroReadiness::HAS_DATA | MacroReadiness::BROKEN;
    assert_eq!(MacroReadiness::DECLARED_BITS, 0x3);
    assert_eq!(readiness.bits(), 0x3);
    assert!(readiness.contains(MacroReadiness::HAS_DATA));
    assert!(!readiness.is_empty());
    assert!((!MacroReadiness::HAS_DATA).contains(MacroReadiness::BROKEN));

    let lifecycle = MacroLifecycle::EXITED | MacroLifecycle::GONE;
    assert_eq!(MacroLifecycle::DECLARED_BITS, 0x3);
    assert_eq!((lifecycle & MacroLifecycle::EXITED).bits(), 0x1);
    assert!(MacroLifecycle::default().is_empty());

    let queue = DeclaredQueue::new(WireDeclaration::<MacroReadiness>::queue("macro.readable"))
        .expect("macro queue declaration");
    let port = DeclaredPort::new(WireDeclaration::<MacroLifecycle>::port("macro.lifecycle"))
        .expect("macro port declaration");
    let queue_wakes = Arc::new(AtomicUsize::new(0));
    let port_wakes = Arc::new(AtomicUsize::new(0));
    let mut queue_subscription = queue
        .try_subscribe(
            MacroReadiness::HAS_DATA,
            counting_waker(Arc::clone(&queue_wakes)),
        )
        .expect("macro queue subscription");
    let mut port_subscription = port
        .try_subscribe(
            MacroLifecycle::EXITED,
            counting_waker(Arc::clone(&port_wakes)),
        )
        .expect("macro port subscription");

    assert_eq!(queue.fire(MacroReadiness::HAS_DATA), 1);
    assert_eq!(port.fire(MacroLifecycle::EXITED), 1);
    assert_eq!(queue_wakes.load(Ordering::SeqCst), 1);
    assert_eq!(port_wakes.load(Ordering::SeqCst), 1);
    assert!(queue_subscription.take_ready());
    assert!(port_subscription.take_ready());
    assert_eq!(
        queue.try_fire(MacroReadiness::from_bits(0x4)),
        Err(DeclaredWireError::Declaration(
            WireDeclarationError::UndeclaredBits
        ))
    );
    assert_eq!(
        port.try_fire(MacroLifecycle::from_bits(0x4)),
        Err(DeclaredWireError::Declaration(
            WireDeclarationError::UndeclaredBits
        ))
    );
}

#[test]
fn raw_trace_declares_typed_payload_and_accepts_noop_emit() {
    let declaration = TraceDeclaration::<ManualTracePayload>::new("vfs.open");
    let trace = RawTrace::new(declaration);
    let payload = ManualTracePayload {
        ino: 42,
        flags: 0x100,
    };

    assert_eq!(trace.declaration().name(), "vfs.open");
    trace.emit(payload);
    RawTrace::new(TraceDeclaration::<()>::new("empty.trace")).emit(());
}

#[test]
fn tracepoint_macro_generates_typed_payloads() {
    let payload = MacroTracePayload::new(17, 3);
    let trace = RawTrace::new(TraceDeclaration::<MacroTracePayload>::new("sched.event"));

    assert_eq!(payload.pid, 17);
    assert_eq!(payload.code, 3);
    trace.emit(payload);
}

#[test]
fn declared_wires_reject_empty_declarations() {
    assert_eq!(
        DeclaredQueue::new(WireDeclaration::<EmptyDeclaration>::queue("bad.empty")).map(|_| ()),
        Err(WireDeclarationError::EmptyDeclaration)
    );
    assert_eq!(
        DeclaredPort::new(WireDeclaration::<EmptyDeclaration>::port("bad.empty")).map(|_| ()),
        Err(WireDeclarationError::EmptyDeclaration)
    );
}

#[test]
fn declared_queue_retire_records_epoch_terminal_handshake() {
    let _epoch = reset_epoch();
    let queue = DeclaredQueue::new(WireDeclaration::<Readiness>::queue("pipe.read_wq"))
        .expect("valid queue declaration");
    let wakes = Arc::new(AtomicUsize::new(0));
    let mut subscription = queue.subscribe(Readiness::HAS_DATA, counting_waker(Arc::clone(&wakes)));
    let guard = epoch::guard();

    let retirement = queue.retire(Readiness::BROKEN, &guard);

    assert_eq!(retirement.kind(), WireKind::Queue);
    assert_eq!(retirement.terminal_bits(), Readiness::BROKEN.0);
    assert_eq!(retirement.woken(), 1);
    assert!(retirement.newly_terminal());
    assert_eq!(retirement.guard_epoch(), guard.entered_epoch());
    assert_eq!(retirement.guard_cpu(), guard.cpu_id());
    assert_eq!(wakes.load(Ordering::SeqCst), 1);
    assert_eq!(subscription.state(), RawSubscriptionState::Terminal);
    assert!(subscription.take_ready());

    let repeated = queue.retire(Readiness::BROKEN, &guard);
    assert_eq!(repeated.woken(), 0);
    assert!(!repeated.newly_terminal());
}

#[test]
fn raw_port_silent_retire_drains_without_waking_and_records_epoch() {
    let _epoch = reset_epoch();
    let port = RawPort::new();
    let wakes = Arc::new(AtomicUsize::new(0));
    let mut subscription = port.subscribe(0x1, counting_waker(Arc::clone(&wakes)));
    let guard = epoch::guard();

    let retirement = port.retire_silently(&guard);

    assert_eq!(retirement.kind(), WireKind::Port);
    assert_eq!(retirement.terminal_bits(), 0);
    assert_eq!(retirement.woken(), 0);
    assert!(retirement.newly_terminal());
    assert_eq!(retirement.guard_epoch(), guard.entered_epoch());
    assert_eq!(retirement.guard_cpu(), guard.cpu_id());
    assert_eq!(wakes.load(Ordering::SeqCst), 0);
    assert_eq!(subscription.state(), RawSubscriptionState::Terminal);
    assert!(subscription.take_ready());
}

#[test]
fn owner_retire_fence_queues_storage_reclaim_after_embedded_wire_retire() {
    let _epoch = reset_epoch();
    let queue = RawQueue::new();
    let port = RawPort::new();
    let queue_wakes = Arc::new(AtomicUsize::new(0));
    let port_wakes = Arc::new(AtomicUsize::new(0));
    let mut queue_subscription = queue.subscribe(0x1, counting_waker(Arc::clone(&queue_wakes)));
    let mut port_subscription = port.subscribe(0x2, counting_waker(Arc::clone(&port_wakes)));
    let guard = epoch::guard();

    let mut fence =
        WireOwnerRetireFence::from_retirement(queue.retire(0x1, &guard)).expect("queue fence");
    fence
        .include(port.retire(0x2, &guard))
        .expect("same-guard port fence");

    assert_eq!(fence.wires(), 2);
    assert_eq!(fence.woken(), 2);
    assert_eq!(fence.guard_epoch(), guard.entered_epoch());
    assert_eq!(fence.guard_cpu(), guard.cpu_id());
    assert_eq!(queue.subscriber_count(), 0);
    assert_eq!(port.subscriber_count(), 0);
    assert_eq!(queue_wakes.load(Ordering::SeqCst), 1);
    assert_eq!(port_wakes.load(Ordering::SeqCst), 1);
    assert_eq!(queue_subscription.state(), RawSubscriptionState::Terminal);
    assert_eq!(port_subscription.state(), RawSubscriptionState::Terminal);
    assert!(queue_subscription.take_ready());
    assert!(port_subscription.take_ready());

    let reclaim = unsafe {
        fence
            .retire_owner_storage(NonNull::<u8>::dangling(), count_owner_reclaim)
            .expect("owner storage retire")
    };
    assert_eq!(reclaim.wires(), 2);
    assert_eq!(reclaim.woken(), 2);
    assert_eq!(reclaim.guard_epoch(), guard.entered_epoch());
    assert_eq!(reclaim.guard_cpu(), guard.cpu_id());

    let blocked = epoch::drain_with_budget(usize::MAX);
    assert_eq!(blocked.reclaimed, 0);
    assert_eq!(OWNER_RECLAIM_COUNT.load(Ordering::Acquire), 0);

    drop(guard);

    let drained = epoch::drain_with_budget(usize::MAX);
    assert_eq!(drained.reclaimed, 1);
    assert_eq!(OWNER_RECLAIM_COUNT.load(Ordering::Acquire), 1);
}

#[test]
fn owner_retire_fence_rejects_repeated_and_mismatched_wire_retirements() {
    let _epoch = reset_epoch();
    let queue = RawQueue::new();
    let port = RawPort::new();
    let guard = epoch::guard();

    let first = queue.retire_silently(&guard);
    let repeated = queue.retire_silently(&guard);
    assert_eq!(
        WireOwnerRetireFence::from_retirement(repeated),
        Err(WireOwnerReclaimError::WireAlreadyTerminal)
    );

    let mut fence = WireOwnerRetireFence::from_retirement(first).expect("first fence");
    drop(guard);
    epoch::drain_with_budget(usize::MAX);

    let later_guard = epoch::guard();
    let later = port.retire_silently(&later_guard);
    assert_eq!(
        fence.include(later),
        Err(WireOwnerReclaimError::GuardMismatch)
    );
}

#[test]
fn typed_owner_manifest_retires_wires_and_queues_typed_reclaim() {
    let _epoch = reset_epoch();
    let owner = Box::new(ManifestOwner {
        queue: RawQueue::new(),
        port: RawPort::new(),
    });
    let owner = NonNull::new(Box::into_raw(owner)).expect("box pointer is non-null");
    let owner_ref = unsafe { owner.as_ref() };
    let queue = owner_ref.queue.clone();
    let port = owner_ref.port.clone();
    let queue_wakes = Arc::new(AtomicUsize::new(0));
    let port_wakes = Arc::new(AtomicUsize::new(0));
    let mut queue_subscription = queue.subscribe(0x1, counting_waker(Arc::clone(&queue_wakes)));
    let mut port_subscription = port.subscribe(0x2, counting_waker(Arc::clone(&port_wakes)));
    let guard = epoch::guard();

    let reclaim = unsafe { retire_wire_owner(owner, &guard) }.expect("typed owner retire");

    assert_eq!(reclaim.wires(), 2);
    assert_eq!(reclaim.woken(), 2);
    assert_eq!(reclaim.guard_epoch(), guard.entered_epoch());
    assert_eq!(reclaim.guard_cpu(), guard.cpu_id());
    assert_eq!(queue.subscriber_count(), 0);
    assert_eq!(port.subscriber_count(), 0);
    assert_eq!(queue_wakes.load(Ordering::SeqCst), 1);
    assert_eq!(port_wakes.load(Ordering::SeqCst), 1);
    assert_eq!(queue_subscription.state(), RawSubscriptionState::Terminal);
    assert_eq!(port_subscription.state(), RawSubscriptionState::Terminal);
    assert!(queue_subscription.take_ready());
    assert!(port_subscription.take_ready());

    let blocked = epoch::drain_with_budget(usize::MAX);
    assert_eq!(blocked.reclaimed, 0);
    assert_eq!(OWNER_RECLAIM_COUNT.load(Ordering::Acquire), 0);

    drop(guard);

    let drained = epoch::drain_with_budget(usize::MAX);
    assert_eq!(drained.reclaimed, 1);
    assert_eq!(OWNER_RECLAIM_COUNT.load(Ordering::Acquire), 1);
}

#[test]
fn owner_manifest_macro_retires_declared_wires_and_queues_typed_reclaim() {
    let _epoch = reset_epoch();
    let owner = Box::new(MacroManifestOwner {
        queue: DeclaredQueue::new(WireDeclaration::<Readiness>::queue("macro.owner.queue"))
            .expect("macro owner queue"),
        port: DeclaredPort::new(WireDeclaration::<Lifecycle>::port("macro.owner.port"))
            .expect("macro owner port"),
    });
    let owner = NonNull::new(Box::into_raw(owner)).expect("box pointer is non-null");
    let owner_ref = unsafe { owner.as_ref() };
    let queue = owner_ref.queue.clone();
    let port = owner_ref.port.clone();
    let queue_wakes = Arc::new(AtomicUsize::new(0));
    let port_wakes = Arc::new(AtomicUsize::new(0));
    let mut queue_subscription = queue.subscribe(
        Readiness::HAS_DATA,
        counting_waker(Arc::clone(&queue_wakes)),
    );
    let mut port_subscription =
        port.subscribe(Lifecycle::EXITED, counting_waker(Arc::clone(&port_wakes)));
    let guard = epoch::guard();

    let reclaim = unsafe { retire_wire_owner(owner, &guard) }.expect("macro owner retire");

    assert_eq!(reclaim.wires(), 2);
    assert_eq!(reclaim.woken(), 2);
    assert_eq!(reclaim.guard_epoch(), guard.entered_epoch());
    assert_eq!(reclaim.guard_cpu(), guard.cpu_id());
    assert_eq!(queue.subscriber_count(), 0);
    assert_eq!(port.subscriber_count(), 0);
    assert_eq!(queue_wakes.load(Ordering::SeqCst), 1);
    assert_eq!(port_wakes.load(Ordering::SeqCst), 1);
    assert_eq!(queue_subscription.state(), RawSubscriptionState::Terminal);
    assert_eq!(port_subscription.state(), RawSubscriptionState::Terminal);
    assert!(queue_subscription.take_ready());
    assert!(port_subscription.take_ready());

    let blocked = epoch::drain_with_budget(usize::MAX);
    assert_eq!(blocked.reclaimed, 0);
    assert_eq!(OWNER_RECLAIM_COUNT.load(Ordering::Acquire), 0);

    drop(guard);

    let drained = epoch::drain_with_budget(usize::MAX);
    assert_eq!(drained.reclaimed, 1);
    assert_eq!(OWNER_RECLAIM_COUNT.load(Ordering::Acquire), 1);
}

#[test]
fn subscription_graph_owns_long_lived_queue_and_port_subscriptions() {
    let queue = RawQueue::new();
    let port = RawPort::new();
    let queue_wakes = Arc::new(AtomicUsize::new(0));
    let port_wakes = Arc::new(AtomicUsize::new(0));
    let mut graph = SubscriptionGraph::<4>::new();

    let queue_key = graph
        .subscribe_queue(&queue, 0x1, counting_waker(Arc::clone(&queue_wakes)))
        .expect("queue graph subscription");
    let port_key = graph
        .subscribe_port(&port, 0x2, counting_waker(Arc::clone(&port_wakes)))
        .expect("port graph subscription");

    assert_eq!(graph.capacity(), 4);
    assert_eq!(graph.len(), 2);
    assert!(!graph.is_empty());
    assert_eq!(graph.kind(queue_key), Ok(WireKind::Queue));
    assert_eq!(graph.kind(port_key), Ok(WireKind::Port));
    assert_eq!(queue.subscriber_count(), 1);
    assert_eq!(port.subscriber_count(), 1);

    assert_eq!(queue.fire(0x1), 1);
    assert_eq!(port.fire(0x2), 1);
    assert_eq!(queue_wakes.load(Ordering::SeqCst), 1);
    assert_eq!(port_wakes.load(Ordering::SeqCst), 1);
    assert_eq!(graph.take_ready(queue_key), Ok(true));
    assert_eq!(graph.take_ready(port_key), Ok(true));
    assert_eq!(graph.take_ready(queue_key), Ok(false));

    graph.remove(queue_key).expect("remove queue graph entry");
    assert_eq!(graph.len(), 1);
    assert_eq!(queue.subscriber_count(), 0);
    assert_eq!(
        graph.take_ready(queue_key),
        Err(SubscriptionGraphError::StaleKey)
    );
    assert_eq!(queue.fire(0x1), 0);

    drop(graph);
    assert_eq!(port.subscriber_count(), 0);
}

#[test]
fn subscription_graph_accepts_declared_queue_and_port_helpers() {
    let queue = DeclaredQueue::new(WireDeclaration::<Readiness>::queue("typed.graph.queue"))
        .expect("declared queue");
    let port = DeclaredPort::new(WireDeclaration::<Lifecycle>::port("typed.graph.port"))
        .expect("declared port");
    let queue_wakes = Arc::new(AtomicUsize::new(0));
    let port_wakes = Arc::new(AtomicUsize::new(0));
    let replacement_wakes = Arc::new(AtomicUsize::new(0));
    let mut graph = SubscriptionGraph::<4>::new();

    assert_eq!(
        graph.subscribe_declared_queue(
            &queue,
            Readiness::INVALID,
            counting_waker(Arc::clone(&queue_wakes)),
        ),
        Err(SubscriptionGraphError::Declaration(
            WireDeclarationError::UndeclaredBits
        ))
    );

    let queue_key = graph
        .subscribe_declared_queue(
            &queue,
            Readiness::HAS_DATA,
            counting_waker(Arc::clone(&queue_wakes)),
        )
        .expect("declared queue graph subscription");
    let port_key = graph
        .subscribe_declared_port(
            &port,
            Lifecycle::EXITED,
            counting_waker(Arc::clone(&port_wakes)),
        )
        .expect("declared port graph subscription");

    assert_eq!(queue_key.index(), queue_key.raw().index());
    assert_eq!(queue_key.generation(), queue_key.raw().generation());
    assert_eq!(graph.kind_declared(queue_key), Ok(WireKind::Queue));
    assert_eq!(graph.kind_declared(port_key), Ok(WireKind::Port));
    assert_eq!(
        graph.state_declared(queue_key),
        Ok(RawSubscriptionState::Subscribed)
    );
    assert_eq!(queue.subscriber_count(), 1);
    assert_eq!(port.subscriber_count(), 1);

    assert_eq!(queue.fire(Readiness::HAS_DATA), 1);
    assert_eq!(port.fire(Lifecycle::EXITED), 1);
    assert_eq!(queue_wakes.load(Ordering::SeqCst), 1);
    assert_eq!(port_wakes.load(Ordering::SeqCst), 1);
    assert_eq!(graph.take_declared_ready(queue_key), Ok(true));
    assert_eq!(graph.take_declared_ready(port_key), Ok(true));
    assert_eq!(graph.take_declared_ready(queue_key), Ok(false));

    assert_eq!(
        graph.update_declared_queue(
            queue_key,
            Readiness::INVALID,
            counting_waker(Arc::clone(&replacement_wakes)),
        ),
        Err(SubscriptionGraphError::Declaration(
            WireDeclarationError::UndeclaredBits
        ))
    );
    graph
        .update_declared_queue(
            queue_key,
            Readiness::BROKEN,
            counting_waker(Arc::clone(&replacement_wakes)),
        )
        .expect("declared queue graph update");
    queue.clear(Readiness::HAS_DATA);
    assert_eq!(queue.fire(Readiness::HAS_DATA), 0);
    assert_eq!(queue.fire(Readiness::BROKEN), 1);
    assert_eq!(replacement_wakes.load(Ordering::SeqCst), 1);

    graph
        .remove_declared(queue_key)
        .expect("remove declared queue graph entry");
    assert_eq!(graph.len(), 1);
    assert_eq!(queue.subscriber_count(), 0);
    assert_eq!(
        graph.take_declared_ready(queue_key),
        Err(SubscriptionGraphError::StaleKey)
    );

    drop(graph);
    assert_eq!(port.subscriber_count(), 0);
}

#[test]
fn subscription_graph_collects_ready_and_terminal_entries_for_epoll_scan() {
    let queue = RawQueue::new();
    let port = RawPort::new();
    let queue_wakes = Arc::new(AtomicUsize::new(0));
    let port_wakes = Arc::new(AtomicUsize::new(0));
    let mut graph = SubscriptionGraph::<4>::new();

    let queue_key = graph
        .subscribe_queue(&queue, 0x1, counting_waker(Arc::clone(&queue_wakes)))
        .expect("queue graph subscription");
    let port_key = graph
        .subscribe_port(&port, 0x2, counting_waker(Arc::clone(&port_wakes)))
        .expect("port graph subscription");
    let mut ready = [SubscriptionGraphReady::default(); 1];

    assert_eq!(graph.collect_ready(&mut ready), 0);

    assert_eq!(queue.fire(0x1), 1);
    assert_eq!(port.fire(0x2), 1);
    assert_eq!(queue_wakes.load(Ordering::SeqCst), 1);
    assert_eq!(port_wakes.load(Ordering::SeqCst), 1);

    assert_eq!(graph.collect_ready(&mut ready), 1);
    assert_eq!(ready[0].key(), queue_key);
    assert_eq!(ready[0].kind(), WireKind::Queue);
    assert_eq!(ready[0].state(), RawSubscriptionState::Subscribed);
    assert_eq!(graph.take_ready(queue_key), Ok(false));

    assert_eq!(graph.collect_ready(&mut ready), 1);
    assert_eq!(ready[0].key(), port_key);
    assert_eq!(ready[0].kind(), WireKind::Port);
    assert_eq!(ready[0].state(), RawSubscriptionState::Subscribed);
    assert_eq!(graph.take_ready(port_key), Ok(false));

    assert_eq!(port.terminate(0x2), 1);
    assert_eq!(port_wakes.load(Ordering::SeqCst), 2);
    assert_eq!(graph.collect_ready(&mut ready), 1);
    assert_eq!(ready[0].key(), port_key);
    assert_eq!(ready[0].kind(), WireKind::Port);
    assert_eq!(ready[0].state(), RawSubscriptionState::Terminal);
    assert_eq!(
        graph.take_ready(port_key),
        Err(SubscriptionGraphError::RawSubscription(
            RawSubscriptionError::Terminal
        ))
    );
}

#[test]
fn subscription_graph_clear_tears_down_all_entries_and_stales_keys() {
    let queue = RawQueue::new();
    let port = RawPort::new();
    let queue_wakes = Arc::new(AtomicUsize::new(0));
    let port_wakes = Arc::new(AtomicUsize::new(0));
    let mut graph = SubscriptionGraph::<4>::new();

    let queue_key = graph
        .subscribe_queue(&queue, 0x1, counting_waker(Arc::clone(&queue_wakes)))
        .expect("queue graph subscription");
    let port_key = graph
        .subscribe_port(&port, 0x2, counting_waker(Arc::clone(&port_wakes)))
        .expect("port graph subscription");

    assert_eq!(graph.len(), 2);
    assert_eq!(queue.subscriber_count(), 1);
    assert_eq!(port.subscriber_count(), 1);

    assert_eq!(graph.clear(), 2);

    assert!(graph.is_empty());
    assert_eq!(queue.subscriber_count(), 0);
    assert_eq!(port.subscriber_count(), 0);
    assert_eq!(
        graph.state(queue_key),
        Err(SubscriptionGraphError::StaleKey)
    );
    assert_eq!(graph.state(port_key), Err(SubscriptionGraphError::StaleKey));
    assert_eq!(queue.fire(0x1), 0);
    assert_eq!(port.fire(0x2), 0);
    assert_eq!(queue_wakes.load(Ordering::SeqCst), 0);
    assert_eq!(port_wakes.load(Ordering::SeqCst), 0);
    assert_eq!(graph.clear(), 0);
}

#[test]
fn subscription_graph_rejects_full_stale_empty_and_wrong_kind_operations() {
    let queue = RawQueue::new();
    let port = RawPort::new();
    let wakes = Arc::new(AtomicUsize::new(0));
    let replacement_wakes = Arc::new(AtomicUsize::new(0));
    let mut graph = SubscriptionGraph::<1>::new();

    assert_eq!(
        graph.subscribe_queue(&queue, 0, counting_waker(Arc::clone(&wakes))),
        Err(SubscriptionGraphError::EmptyInterest)
    );

    let key = graph
        .subscribe_queue(&queue, 0x1, counting_waker(Arc::clone(&wakes)))
        .expect("queue graph subscription");
    assert_eq!(
        graph.subscribe_port(&port, 0x1, counting_waker(Arc::clone(&wakes))),
        Err(SubscriptionGraphError::Full)
    );
    assert_eq!(
        graph.update_port(key, 0x1, counting_waker(Arc::clone(&replacement_wakes))),
        Err(SubscriptionGraphError::KindMismatch)
    );
    assert_eq!(
        graph.update_queue(key, 0, counting_waker(Arc::clone(&replacement_wakes))),
        Err(SubscriptionGraphError::EmptyInterest)
    );

    graph
        .update_queue(key, 0x2, counting_waker(Arc::clone(&replacement_wakes)))
        .expect("update queue graph subscription");
    assert_eq!(queue.fire(0x1), 0);
    queue.clear(0x1);
    assert_eq!(queue.fire(0x2), 1);
    assert_eq!(wakes.load(Ordering::SeqCst), 0);
    assert_eq!(replacement_wakes.load(Ordering::SeqCst), 1);

    graph.remove(key).expect("remove graph entry");
    let reused = graph
        .subscribe_queue(&queue, 0x1, counting_waker(Arc::clone(&wakes)))
        .expect("reused slot has new generation");
    assert_eq!(reused.index(), key.index());
    assert_ne!(reused.generation(), key.generation());
    assert_eq!(graph.state(key), Err(SubscriptionGraphError::StaleKey));
}

#[test]
fn subscription_graph_reports_terminal_raw_wire_errors() {
    let queue = RawQueue::new();
    let _epoch = reset_epoch();
    let guard = epoch::guard();
    queue.retire_silently(&guard);
    drop(guard);

    let mut graph = SubscriptionGraph::<1>::new();
    let wakes = Arc::new(AtomicUsize::new(0));

    assert_eq!(
        graph.subscribe_queue(&queue, 0x1, counting_waker(Arc::clone(&wakes))),
        Err(SubscriptionGraphError::RawWire(RawWireError::Terminal))
    );
}

#[test]
fn raw_queue_subscription_reports_unsubscribed_after_explicit_unsubscribe() {
    let queue = RawQueue::new();
    let wakes = Arc::new(AtomicUsize::new(0));
    let replacement_wakes = Arc::new(AtomicUsize::new(0));
    let mut subscription = queue.subscribe(0x1, counting_waker(Arc::clone(&wakes)));

    assert_eq!(subscription.state(), RawSubscriptionState::Subscribed);
    assert!(subscription.is_subscribed());
    assert_eq!(queue.subscriber_count(), 1);

    assert!(subscription.unsubscribe());
    assert_eq!(subscription.state(), RawSubscriptionState::Unsubscribed);
    assert!(!subscription.is_subscribed());
    assert_eq!(queue.subscriber_count(), 0);
    assert_eq!(
        subscription.try_take_ready(),
        Err(RawSubscriptionError::Unsubscribed)
    );
    assert_eq!(
        subscription.try_update(0x2, counting_waker(Arc::clone(&replacement_wakes))),
        Err(RawSubscriptionError::Unsubscribed)
    );
    assert!(!subscription.unsubscribe());

    assert_eq!(queue.fire(0x1), 0);
    assert_eq!(wakes.load(Ordering::SeqCst), 0);
    assert_eq!(replacement_wakes.load(Ordering::SeqCst), 0);
}

#[test]
fn raw_port_subscription_reports_unsubscribed_after_explicit_unsubscribe() {
    let port = RawPort::new();
    let wakes = Arc::new(AtomicUsize::new(0));
    let replacement_wakes = Arc::new(AtomicUsize::new(0));
    let mut subscription = port.subscribe(0x1, counting_waker(Arc::clone(&wakes)));

    assert_eq!(subscription.state(), RawSubscriptionState::Subscribed);
    assert!(subscription.is_subscribed());
    assert_eq!(port.subscriber_count(), 1);

    assert!(subscription.unsubscribe());
    assert_eq!(subscription.state(), RawSubscriptionState::Unsubscribed);
    assert!(!subscription.is_subscribed());
    assert_eq!(port.subscriber_count(), 0);
    assert_eq!(
        subscription.try_take_ready(),
        Err(RawSubscriptionError::Unsubscribed)
    );
    assert_eq!(
        subscription.try_update(0x2, counting_waker(Arc::clone(&replacement_wakes))),
        Err(RawSubscriptionError::Unsubscribed)
    );
    assert!(!subscription.unsubscribe());

    assert_eq!(port.fire(0x1), 0);
    assert_eq!(wakes.load(Ordering::SeqCst), 0);
    assert_eq!(replacement_wakes.load(Ordering::SeqCst), 0);
}

#[test]
fn raw_queue_terminal_mask_wakes_drains_and_reports_terminal_state() {
    let queue = RawQueue::new();
    let first_wakes = Arc::new(AtomicUsize::new(0));
    let second_wakes = Arc::new(AtomicUsize::new(0));
    let late_wakes = Arc::new(AtomicUsize::new(0));
    let mut first = queue.subscribe(0x1, counting_waker(Arc::clone(&first_wakes)));
    let mut second = queue.subscribe(0x4, counting_waker(Arc::clone(&second_wakes)));

    assert_eq!(queue.subscriber_count(), 2);
    assert_eq!(queue.terminate(0x8), 2);

    assert!(queue.is_terminal());
    assert_eq!(queue.peek(), 0x8);
    assert_eq!(queue.subscriber_count(), 0);
    assert_eq!(first_wakes.load(Ordering::SeqCst), 1);
    assert_eq!(second_wakes.load(Ordering::SeqCst), 1);
    assert_eq!(first.state(), RawSubscriptionState::Terminal);
    assert_eq!(second.state(), RawSubscriptionState::Terminal);
    assert_eq!(first.try_take_ready(), Err(RawSubscriptionError::Terminal));
    assert!(first.take_ready());
    assert_eq!(
        second.try_update(0x8, counting_waker(Arc::clone(&late_wakes))),
        Err(RawSubscriptionError::Terminal)
    );

    assert_eq!(queue.try_fire(0x1), Err(RawWireError::Terminal));
    assert_eq!(queue.fire(0x1), 0);
    assert_eq!(queue.try_clear(0x8), Err(RawWireError::Terminal));
    queue.clear(0x8);
    assert_eq!(queue.peek(), 0x8);

    let mut late = queue.subscribe(0x1, counting_waker(Arc::clone(&late_wakes)));
    assert_eq!(late.state(), RawSubscriptionState::Terminal);
    assert_eq!(
        queue
            .try_subscribe(0x1, counting_waker(Arc::clone(&late_wakes)))
            .err(),
        Some(RawWireError::Terminal)
    );
    assert!(late.take_ready());
    assert_eq!(late_wakes.load(Ordering::SeqCst), 1);
    assert_eq!(queue.terminate(0x8), 0);
}

#[test]
fn raw_port_terminal_event_wakes_drains_and_reports_terminal_state() {
    let port = RawPort::new();
    let first_wakes = Arc::new(AtomicUsize::new(0));
    let second_wakes = Arc::new(AtomicUsize::new(0));
    let late_wakes = Arc::new(AtomicUsize::new(0));
    let mut first = port.subscribe(0x1, counting_waker(Arc::clone(&first_wakes)));
    let mut second = port.subscribe(0x4, counting_waker(Arc::clone(&second_wakes)));

    assert_eq!(port.subscriber_count(), 2);
    assert_eq!(port.terminate(0x80), 2);

    assert!(port.is_terminal());
    assert_eq!(port.subscriber_count(), 0);
    assert_eq!(first_wakes.load(Ordering::SeqCst), 1);
    assert_eq!(second_wakes.load(Ordering::SeqCst), 1);
    assert_eq!(first.state(), RawSubscriptionState::Terminal);
    assert_eq!(second.state(), RawSubscriptionState::Terminal);
    assert_eq!(first.try_take_ready(), Err(RawSubscriptionError::Terminal));
    assert!(first.take_ready());
    assert_eq!(
        second.try_update(0x80, counting_waker(Arc::clone(&late_wakes))),
        Err(RawSubscriptionError::Terminal)
    );

    assert_eq!(port.try_fire(0x1), Err(RawWireError::Terminal));
    assert_eq!(port.fire(0x1), 0);

    let mut late = port.subscribe(0x1, counting_waker(Arc::clone(&late_wakes)));
    assert_eq!(late.state(), RawSubscriptionState::Terminal);
    assert_eq!(
        port.try_subscribe(0x1, counting_waker(Arc::clone(&late_wakes)))
            .err(),
        Some(RawWireError::Terminal)
    );
    assert!(late.take_ready());
    assert_eq!(late_wakes.load(Ordering::SeqCst), 1);
    assert_eq!(port.terminate(0x80), 0);
}

#[test]
fn raw_port_terminal_without_gone_event_drains_silently() {
    let port = RawPort::new();
    let wakes = Arc::new(AtomicUsize::new(0));
    let mut subscription = port.subscribe(0x1, counting_waker(Arc::clone(&wakes)));

    assert_eq!(port.terminate(0), 0);

    assert!(port.is_terminal());
    assert_eq!(port.subscriber_count(), 0);
    assert_eq!(wakes.load(Ordering::SeqCst), 0);
    assert_eq!(subscription.state(), RawSubscriptionState::Terminal);
    assert_eq!(
        subscription.try_take_ready(),
        Err(RawSubscriptionError::Terminal)
    );
    assert!(subscription.take_ready());
}
