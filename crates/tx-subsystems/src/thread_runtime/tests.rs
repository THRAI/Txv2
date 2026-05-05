//! Thread-runtime topology tests.
//!
//! Focus on the thread-side half of the identity/payload split and the
//! parent-bookkeeping that `step_thread_exit` performs.

use crate::process::execution::reset_init_process_for_test;
use crate::process::structure::{reset_pid_counter_for_test, ProcessIdentity};
use crate::process::{bootstrap_init_process, step_fork, ExitStatus};
use crate::test_support::EPOCH_TEST_LOCK;
use crate::thread_runtime::step_thread_exit;
use crate::thread_runtime::structure::{reset_tid_counter_for_test, ThreadIdentity};
use crate::vm::{AddressSpace, TestPmap};
use crate::zones;
use tx_substrate::testing::init_host_for_test_once;
use tx_substrate::zone::Cap;

fn setup() -> std::sync::MutexGuard<'static, ()> {
    let guard = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    init_host_for_test_once();
    let _ = zones::register_all();
    let _ = tx_substrate::epoch::drain_with_budget(usize::MAX);
    let _ = tx_substrate::epoch::drain_with_budget(usize::MAX);
    reset_pid_counter_for_test();
    reset_tid_counter_for_test();
    reset_init_process_for_test();
    guard
}

fn fresh_aspace() -> Cap<AddressSpace> {
    AddressSpace::new_cap_for_platform::<TestPmap>().expect("fresh aspace")
}

fn bootstrap() -> Cap<ProcessIdentity> {
    bootstrap_init_process(fresh_aspace()).expect("bootstrap init")
}

fn first_thread(proc_cap: &Cap<ProcessIdentity>) -> Cap<ThreadIdentity> {
    let payload_guard = proc_cap.payload.lock();
    let payload = payload_guard.as_ref().expect("alive");
    let threads = payload.threads.lock();
    threads[0].clone()
}

#[test]
fn thread_exit_sets_status_and_drops_thread_payload() {
    let _g = setup();
    let proc_cap = bootstrap();
    let leader = first_thread(&proc_cap);

    assert!(!leader.is_zombie());
    step_thread_exit(leader.clone(), 3);

    assert!(leader.is_zombie());
    assert_eq!(leader.exit_status(), Some(3));
}

#[test]
fn last_thread_exit_zombifies_owner_process() {
    let _g = setup();
    let proc_cap = bootstrap();
    let leader = first_thread(&proc_cap);

    step_thread_exit(leader, 99);

    assert!(proc_cap.is_zombie());
    assert_eq!(proc_cap.exit_status(), Some(ExitStatus::Exited(99)));
    assert_eq!(proc_cap.live_thread_count(), 0);
}

#[test]
fn weak_owner_proc_upgrades_while_process_lives() {
    let _g = setup();
    let proc_cap = bootstrap();
    let leader = first_thread(&proc_cap);

    let upgraded = leader.upgrade_owner_proc();
    assert!(
        upgraded.is_some(),
        "owner should upgrade while process is live"
    );
    assert_eq!(upgraded.unwrap().pid, proc_cap.pid);
}

#[test]
fn weak_owner_proc_survives_payload_drop() {
    let _g = setup();
    let proc_cap = bootstrap();
    let leader = first_thread(&proc_cap);

    // Zombify by exit_group; identity persists, payload gone.
    crate::process::step_exit_group(&proc_cap, ExitStatus::Exited(0));
    assert!(proc_cap.is_zombie());

    // Weak still resolves to the (zombie) identity.
    let upgraded = leader.upgrade_owner_proc();
    assert!(
        upgraded.is_some(),
        "weak should still resolve to zombie identity"
    );
}

#[test]
fn weak_owner_proc_flips_dead_after_identity_drop() {
    let _g = setup();
    let proc_cap = bootstrap();
    let leader = first_thread(&proc_cap);

    // Zombify so the process payload is gone but the identity is still
    // retained by `proc_cap`.
    crate::process::step_exit_group(&proc_cap, ExitStatus::Exited(0));
    assert!(leader.upgrade_owner_proc().is_some());

    // Drop the strong handles. Weak observers should no longer find
    // it after epoch drain. Releasing the test's local Cap is not
    // sufficient: bootstrap_init_process registers the init Cap in
    // the global INIT_PROCESS slot, which is the second strong
    // retainer. Tests release it explicitly via
    // reset_init_process_for_test.
    drop(proc_cap);
    reset_init_process_for_test();
    let _ = tx_substrate::epoch::drain_with_budget(usize::MAX);
    let _ = tx_substrate::epoch::drain_with_budget(usize::MAX);

    assert!(
        leader.upgrade_owner_proc().is_none(),
        "weak should observe dead after identity drops"
    );
}

#[test]
fn fork_assigns_distinct_tids_to_parent_and_child_leader_threads() {
    let _g = setup();
    let parent = bootstrap();
    let parent_leader = first_thread(&parent);

    let child = step_fork::<TestPmap>(&parent).expect("fork");
    let child_leader = first_thread(&child);

    assert_ne!(parent_leader.tid, child_leader.tid);
    // Owner reverse-pointers go to the right places.
    assert_eq!(
        parent_leader.upgrade_owner_proc().expect("alive").pid,
        parent.pid
    );
    assert_eq!(
        child_leader.upgrade_owner_proc().expect("alive").pid,
        child.pid
    );
}
