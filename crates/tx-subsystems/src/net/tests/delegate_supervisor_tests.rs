use super::*;
use crate::net::delegate::NetDelegateTimerWake;

#[test]
fn net_delegate_supervisor_refreshes_deadline_generations() {
    let base = smoltcp::time::Instant::ZERO;
    let mut supervisor = NetDelegateSupervisor::new(base, 1_000);
    let first_deadline = smoltcp::time::Instant::from_millis(5);
    let second_deadline = smoltcp::time::Instant::from_millis(8);

    let first = supervisor
        .refresh_deadline(Some(first_deadline))
        .expect("first timer arm");
    assert_eq!(first.generation, 1);
    assert_eq!(first.deadline, first_deadline);
    assert_eq!(first.deadline_ns, 5_001_000);
    assert_eq!(supervisor.armed(), Some(first));

    assert_eq!(supervisor.refresh_deadline(Some(first_deadline)), None);
    assert_eq!(supervisor.armed(), Some(first));

    let second = supervisor
        .refresh_deadline(Some(second_deadline))
        .expect("second timer arm");
    assert_eq!(second.generation, 2);
    assert_eq!(second.deadline, second_deadline);
    assert_eq!(second.deadline_ns, 8_001_000);
    assert_eq!(supervisor.armed(), Some(second));

    assert_eq!(supervisor.refresh_deadline(None), None);
    assert_eq!(supervisor.armed(), None);
    assert_eq!(supervisor.refresh_deadline(None), None);
}

#[test]
fn net_delegate_supervisor_timer_wake_fires_tick_for_current_generation() {
    crate::net::delegate::net_delegate_clear(
        crate::net::delegate::DelegateWireSet::POLL | crate::net::delegate::DelegateWireSet::TICK,
    );

    let mut supervisor = NetDelegateSupervisor::new(smoltcp::time::Instant::ZERO, 0);
    let arm = supervisor
        .refresh_deadline(Some(smoltcp::time::Instant::from_millis(3)))
        .expect("timer arm");
    let mut reactor = Reactor::new();
    let timer_channel = reactor.channel();
    reactor.submit(async move {
        let wake = net_delegate_wait_supervised_deadline(timer_channel, arm).await;
        assert_eq!(wake.generation, arm.generation);
        assert_eq!(wake.outcome, WaitOutcome::TimedOut);
        assert!(wake.tick_fired);
    });

    let idle = reactor.run_until_idle_with_clock(|| 0, |_| {});
    assert_eq!(idle.next_deadline_ns(), Some(3_000_000));
    assert_eq!(crate::net::delegate::net_delegate_queue().peek(), 0);

    let fired = reactor.run_until_idle_with_clock(|| 3_000_000, |_| {});
    assert_eq!(fired.timer_wakes(), 1);
    assert!(
        crate::net::delegate::net_delegate_queue().peek()
            & crate::net::delegate::DelegateWireSet::TICK.bits()
            != 0
    );
}

#[test]
fn net_delegate_supervisor_consumes_matching_timer_wake() {
    let mut supervisor = NetDelegateSupervisor::new(smoltcp::time::Instant::ZERO, 0);
    let arm = supervisor
        .refresh_deadline(Some(smoltcp::time::Instant::from_millis(4)))
        .expect("timer arm");

    assert!(supervisor.consume_wake(NetDelegateTimerWake {
        generation: arm.generation,
        outcome: WaitOutcome::TimedOut,
        tick_fired: true,
    }));
    assert_eq!(supervisor.armed(), None);
}

#[test]
fn net_delegate_supervisor_rejects_stale_timer_wake() {
    let mut supervisor = NetDelegateSupervisor::new(smoltcp::time::Instant::ZERO, 0);
    let first = supervisor
        .refresh_deadline(Some(smoltcp::time::Instant::from_millis(4)))
        .expect("first timer arm");
    let second = supervisor
        .refresh_deadline(Some(smoltcp::time::Instant::from_millis(8)))
        .expect("second timer arm");

    assert!(!supervisor.consume_wake(NetDelegateTimerWake {
        generation: first.generation,
        outcome: WaitOutcome::TimedOut,
        tick_fired: true,
    }));
    assert_eq!(supervisor.armed(), Some(second));
}
