//! Timer-domain surface pins for the reactor-owned `TimerEngine` route table.

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};

use tx_reactor::{MailboxEvent, Reactor, TaskMailbox};
use tx_services::time::{
    DeadlineNs, DeadlineRegistrar, DeviceTimerCallback, TimerRole, TimerTarget,
};
use tx_substrate::step::{InterestMask, WaitSourceId};
use tx_substrate::wake::{register_source, unregister_source, WaitSource};

static DEVICE_CALLBACK_FIRES: AtomicU64 = AtomicU64::new(0);
static DEVICE_CALLBACK_LOCK: Mutex<()> = Mutex::new(());

fn count_device_callback(payload: u64) {
    DEVICE_CALLBACK_FIRES.fetch_add(payload, Ordering::AcqRel);
}

#[test]
fn reactor_timer_path_has_no_legacy_wheel_or_router_dependency() {
    let registry = include_str!("../src/deadline_registry.rs");
    let runtime = include_str!("../src/runtime.rs");
    let hart_loop = include_str!("../src/hart_loop.rs");
    let wait = include_str!("../src/wait.rs");

    for source in [registry, runtime, hart_loop, wait] {
        assert!(!source.contains(concat!("Timer", "Wheel")));
        assert!(!source.contains(concat!("TimerWake", "Router")));
    }
}

#[test]
fn deadline_domain_routes_task_signal_wait_source_and_device_callback() {
    let _serial = DEVICE_CALLBACK_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    DEVICE_CALLBACK_FIRES.store(0, Ordering::Release);

    let reactor = Reactor::new();
    let registrar = reactor.deadline_registrar_handle();
    let task_mailbox = Arc::new(TaskMailbox::new());
    let signal_mailbox = Arc::new(TaskMailbox::new());
    let source_mailbox = Arc::new(TaskMailbox::new());
    let source = Arc::new(WaitSource::new(WaitSourceId::new(0x7E57)));
    let interests = InterestMask::new(0b100);
    register_source(Arc::clone(&source));
    let subscriber = source.register(
        Arc::downgrade(&source_mailbox),
        source_mailbox.next_generation(),
        interests,
    );

    let _task = registrar
        .register_deadline(
            DeadlineNs::new(10),
            TimerRole::PrimarySleep,
            TimerTarget::TaskMailbox(Arc::downgrade(&task_mailbox)),
        )
        .expect("task deadline registration");
    let _signal = registrar
        .register_deadline(
            DeadlineNs::new(10),
            TimerRole::ItimerReal,
            TimerTarget::SignalTarget {
                mailbox: Arc::downgrade(&signal_mailbox),
            },
        )
        .expect("signal deadline registration");
    let _source = registrar
        .register_deadline(
            DeadlineNs::new(10),
            TimerRole::PollTimeout,
            TimerTarget::WaitSource {
                source: source.id(),
                interests,
            },
        )
        .expect("wait-source deadline registration");
    let _device = registrar
        .register_deadline(
            DeadlineNs::new(10),
            TimerRole::DeviceEvent,
            TimerTarget::DeviceCallback(DeviceTimerCallback::new(count_device_callback, 7)),
        )
        .expect("device deadline registration");

    assert_eq!(reactor.next_deadline_ns(), Some(10));
    assert_eq!(reactor.advance_time_to(9), 0);
    assert_eq!(reactor.advance_time_to(10), 4);
    assert!(matches!(
        task_mailbox.poll(),
        Some(MailboxEvent::TimerFired { .. })
    ));
    assert!(matches!(
        signal_mailbox.poll(),
        Some(MailboxEvent::SignalTimerFired { .. })
    ));
    assert!(matches!(
        source_mailbox.poll(),
        Some(MailboxEvent::SourceFired {
            source: fired_source,
            interests: fired_interests,
            ..
        }) if fired_source == source.id() && fired_interests == interests
    ));
    assert_eq!(DEVICE_CALLBACK_FIRES.load(Ordering::Acquire), 7);
    assert_eq!(reactor.next_deadline_ns(), None);

    source.unregister(subscriber);
    unregister_source(source.id());
}

#[test]
fn dropped_deadline_guard_cancels_the_engine_route() {
    let reactor = Reactor::new();
    let registrar = reactor.deadline_registrar_handle();
    let mailbox = Arc::new(TaskMailbox::new());
    let guard = registrar
        .register_deadline(
            DeadlineNs::new(10),
            TimerRole::DeadlineAbort,
            TimerTarget::TaskMailbox(Arc::downgrade(&mailbox)),
        )
        .expect("deadline registration");

    drop(guard);
    assert_eq!(reactor.next_deadline_ns(), None);
    assert_eq!(reactor.advance_time_to(10), 0);
    assert!(mailbox.poll().is_none());
}
