use smoltcp::time::Instant;
use tx_subsystems::net::delegate::{
    net_delegate_clear, net_delegate_queue, DelegateWireSet, NetDelegateTaskConfig,
};

use super::{set_test_time_ns, setup, CoreInit, TestPlatform};

fn tick_is_ready() -> bool {
    net_delegate_queue().peek() & DelegateWireSet::TICK.bits() != 0
}

#[test]
fn boot_net_deadline_task_fires_tick_at_deadline() {
    let _serial = setup();
    CoreInit::<TestPlatform>::init_boot_reactor_for_test();
    net_delegate_clear(DelegateWireSet::POLL | DelegateWireSet::TICK);

    CoreInit::<TestPlatform>::refresh_net_deadline_for_test(Some(Instant::from_millis(5)))
        .expect("deadline arm");
    CoreInit::<TestPlatform>::submit_net_deadline_task_for_test().expect("deadline task submit");

    let armed = CoreInit::<TestPlatform>::step_boot_reactor_once_for_test()
        .expect("deadline task arm step");
    assert_eq!(armed.next_deadline_ns, Some(5_000_000));
    assert!(!tick_is_ready(), "arming the deadline must not fire TICK");

    set_test_time_ns(5_000_000);
    let fired = CoreInit::<TestPlatform>::step_boot_reactor_once_for_test()
        .expect("deadline task timeout step");
    assert!(fired.observed_timer_wakes(), "reactor timer must expire");
    assert!(
        tick_is_ready(),
        "deadline timeout must publish delegate TICK"
    );
}

#[test]
fn boot_net_deadline_task_rearms_without_stale_tick() {
    let _serial = setup();
    CoreInit::<TestPlatform>::init_boot_reactor_for_test();
    net_delegate_clear(DelegateWireSet::POLL | DelegateWireSet::TICK);

    CoreInit::<TestPlatform>::refresh_net_deadline_for_test(Some(Instant::from_millis(5)))
        .expect("first deadline arm");
    CoreInit::<TestPlatform>::submit_net_deadline_task_for_test().expect("deadline task submit");
    CoreInit::<TestPlatform>::step_boot_reactor_once_for_test().expect("first arm step");

    CoreInit::<TestPlatform>::refresh_net_deadline_for_test(Some(Instant::from_millis(10)))
        .expect("second deadline arm");
    let rearmed = CoreInit::<TestPlatform>::step_boot_reactor_once_for_test().expect("rearm step");
    assert_eq!(rearmed.next_deadline_ns, Some(10_000_000));

    set_test_time_ns(5_000_000);
    CoreInit::<TestPlatform>::step_boot_reactor_once_for_test()
        .expect("stale timer cancellation step");
    assert!(
        !tick_is_ready(),
        "stale deadline wake must not publish delegate TICK"
    );

    set_test_time_ns(10_000_000);
    let current = CoreInit::<TestPlatform>::step_boot_reactor_once_for_test()
        .expect("current timer wake step");
    assert!(current.observed_timer_wakes());
    assert!(tick_is_ready(), "current deadline must publish TICK");
}

#[test]
fn boot_net_deadline_tick_wakes_delegate_task() {
    let _serial = setup();
    CoreInit::<TestPlatform>::init_boot_reactor_for_test();
    net_delegate_clear(DelegateWireSet::POLL | DelegateWireSet::TICK);

    CoreInit::<TestPlatform>::submit_net_delegate_task_for_test(NetDelegateTaskConfig::run_steps(
        1,
    ))
    .expect("delegate task submit");
    CoreInit::<TestPlatform>::submit_net_deadline_task_for_test().expect("deadline task submit");
    let parked = CoreInit::<TestPlatform>::step_boot_reactor_once_for_test()
        .expect("park delegate and deadline tasks");
    assert!(
        parked.stats.polled >= 2,
        "delegate and deadline tasks should both park"
    );

    CoreInit::<TestPlatform>::refresh_net_deadline_for_test(Some(Instant::from_millis(5)))
        .expect("deadline arm");
    CoreInit::<TestPlatform>::step_boot_reactor_once_for_test().expect("deadline arm step");

    set_test_time_ns(5_000_000);
    let tick =
        CoreInit::<TestPlatform>::step_boot_reactor_once_for_test().expect("deadline tick step");
    assert!(tick.observed_timer_wakes());
    assert!(
        tick.stats.completed >= 1,
        "bounded delegate should complete after consuming TICK"
    );
    assert!(
        !tick_is_ready(),
        "delegate task should clear TICK during net_delegate_step_once"
    );
}
