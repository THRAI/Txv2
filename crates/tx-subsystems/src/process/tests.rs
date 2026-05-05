//! Topology tests for the process subsystem.
//!
//! These exercise the entity graph (Process / Thread / ProcessGroup /
//! Session) and the lifecycle steps (`bootstrap_init_process`,
//! `step_fork`, `step_exit_group`, `step_setpgid`, `step_setsid`) without
//! signal state, credentials, rlimits, or fd-table coupling. The
//! identity/payload split is the primary subject under test: zombies
//! retain identity but drop payload.

use crate::process::execution::step_exit_group_with_signal;
use crate::process::structure::{reset_pid_counter_for_test, Pgid, Pid, ProcessIdentity};
use crate::process::{
    bootstrap_init_process, step_exit_group, step_fork, step_setpgid, step_setsid, ForkError,
    SetpgidError,
};
use crate::signal::Signum;
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
fn bootstrap_init_creates_pid_1_with_session_and_pgrp() {
    let _g = setup();
    let init = bootstrap();

    assert_eq!(init.pid, Pid::INIT);
    assert_eq!(init.parent_pid, Pid::RESERVED);
    assert!(!init.is_zombie());
    assert_eq!(init.live_thread_count(), 1);

    let pgrp = init.pgrp_cap();
    assert_eq!(pgrp.pgid, Pgid(Pid::INIT.0));
    let session = pgrp.session_cap();
    assert_eq!(session.sid.0, Pid::INIT.0);
    assert!(!session.has_controlling_tty());
}

#[test]
fn fork_creates_child_with_leader_thread_and_inherits_pgrp() {
    let _g = setup();
    let parent = bootstrap();

    let child = step_fork::<TestPmap>(&parent).expect("fork");

    assert_ne!(child.pid, parent.pid);
    assert_eq!(child.parent_pid, parent.pid);
    assert!(!child.is_zombie());
    assert_eq!(child.live_thread_count(), 1);

    // Child inherits parent's pgrp.
    assert_eq!(child.pgrp_cap().pgid, parent.pgrp_cap().pgid);
}

#[test]
fn fork_clones_address_space_into_distinct_cap() {
    let _g = setup();
    let parent = bootstrap();
    let parent_aspace = parent.aspace_cap().expect("parent live");

    let child = step_fork::<TestPmap>(&parent).expect("fork");
    let child_aspace = child.aspace_cap().expect("child live");

    // The Cap keys must differ — child has its own address space slot.
    assert_ne!(parent_aspace.key(), child_aspace.key());
}

#[test]
fn fork_registers_child_in_parent_pgrp_member_list() {
    let _g = setup();
    let parent = bootstrap();
    let pgrp = parent.pgrp_cap();
    let before = pgrp.member_slot_count();

    let _child = step_fork::<TestPmap>(&parent).expect("fork");

    assert_eq!(pgrp.member_slot_count(), before + 1);
}

#[test]
fn fork_on_zombie_parent_returns_parent_zombie() {
    let _g = setup();
    let parent = bootstrap();
    step_exit_group(&parent, 0);
    assert!(parent.is_zombie());

    let result = step_fork::<TestPmap>(&parent);
    assert!(matches!(result, Err(ForkError::ParentZombie)));
}

#[test]
fn last_thread_exit_zombifies_process_keeps_identity() {
    let _g = setup();
    let proc_cap = bootstrap();
    let leader = first_thread(&proc_cap);

    step_thread_exit(leader, 7);

    assert!(proc_cap.is_zombie());
    assert_eq!(proc_cap.exit_status(), Some(7));
    assert_eq!(proc_cap.pid, Pid::INIT);
}

#[test]
fn exit_group_zombifies_process_at_once_and_records_status() {
    let _g = setup();
    let proc_cap = bootstrap();

    step_exit_group(&proc_cap, 42);

    assert!(proc_cap.is_zombie());
    assert_eq!(proc_cap.exit_status(), Some(42));
    assert_eq!(proc_cap.live_thread_count(), 0);
}

#[test]
fn setpgid_to_target_pid_creates_new_group_in_same_session() {
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent).expect("fork");

    let original_session = child.pgrp_cap().session_cap();
    let original_session_id = original_session.sid;

    step_setpgid(&child, Pgid(child.pid.0)).expect("setpgid");

    let new_pgrp = child.pgrp_cap();
    assert_eq!(new_pgrp.pgid, Pgid(child.pid.0));
    assert_eq!(new_pgrp.session_cap().sid, original_session_id);
    assert_ne!(new_pgrp.pgid, parent.pgrp_cap().pgid);
}

#[test]
fn setpgid_with_existing_group_id_is_unimplemented() {
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent).expect("fork");

    // child.pgid != child.pid, and we don't yet support joining an
    // existing group by id (would require session-walk).
    let result = step_setpgid(&child, parent.pgrp_cap().pgid);
    assert!(matches!(result, Err(SetpgidError::Unimplemented)));
}

#[test]
fn setsid_creates_fresh_session_and_pgrp_at_target_pid() {
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent).expect("fork");
    let parent_session_id = parent.pgrp_cap().session_cap().sid;

    let new_sid = step_setsid(&child).expect("setsid");

    assert_eq!(new_sid.0, child.pid.0);
    let pgrp = child.pgrp_cap();
    assert_eq!(pgrp.pgid.0, child.pid.0);
    let session = pgrp.session_cap();
    assert_eq!(session.sid, new_sid);
    assert_ne!(session.sid, parent_session_id);
    assert!(!session.has_controlling_tty());
}

#[test]
fn pid_pgid_sid_share_value_space_but_are_distinct_types() {
    let _g = setup();
    let init = bootstrap();

    // After bootstrap: pid=1, pgid=1, sid=1 — same numeric values, but
    // the types prevent accidental swapping in code.
    assert_eq!(init.pid, Pid::INIT);
    assert_eq!(init.pgrp_cap().pgid, Pgid(1));
    assert_eq!(init.pgrp_cap().session_cap().sid.0, 1);
}

#[test]
fn pgrp_member_weak_observation_returns_live_process_until_identity_drops() {
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent).expect("fork");
    let pgrp = parent.pgrp_cap();

    // The pgrp has two members: parent and child.
    assert_eq!(pgrp.member_slot_count(), 2);

    let guard = tx_substrate::epoch::guard();
    let live: usize = pgrp
        .members
        .lock()
        .iter()
        .filter(|w| w.upgrade(&guard).is_some())
        .count();
    drop(guard);
    assert_eq!(live, 2);

    // Drop child identity. The weak in pgrp.members is now stale.
    drop(child);
    let _ = tx_substrate::epoch::drain_with_budget(usize::MAX);
    let _ = tx_substrate::epoch::drain_with_budget(usize::MAX);

    let guard = tx_substrate::epoch::guard();
    let live_after: usize = pgrp
        .members
        .lock()
        .iter()
        .filter(|w| w.upgrade(&guard).is_some())
        .count();
    drop(guard);
    assert_eq!(live_after, 1);
}

#[test]
fn step_exit_group_with_signal_records_signum_and_status_encoding() {
    let _g = setup();
    let proc_cap = bootstrap();

    step_exit_group_with_signal(&proc_cap, Signum::SIGTERM);

    assert!(proc_cap.is_zombie());
    assert_eq!(proc_cap.terminating_signal(), Some(Signum::SIGTERM));
    // Day-1 shell-convention status: 128 + signum.
    assert_eq!(
        proc_cap.exit_status(),
        Some(128 + Signum::SIGTERM.raw() as i32)
    );
    assert_eq!(proc_cap.live_thread_count(), 0);
}

#[test]
fn step_exit_group_with_signal_overrides_terminating_signal_on_double_call() {
    let _g = setup();
    let proc_cap = bootstrap();

    // First call sets terminating_signal=SIGTERM and zombifies.
    step_exit_group_with_signal(&proc_cap, Signum::SIGTERM);
    assert_eq!(proc_cap.terminating_signal(), Some(Signum::SIGTERM));

    // Second call on the same identity should be a no-op for the
    // payload (already None) but still updates the recorded signal —
    // demonstrates idempotent slot semantics. Defensive coverage of
    // the double-zombify path.
    step_exit_group_with_signal(&proc_cap, Signum::SIGKILL);
    assert_eq!(proc_cap.terminating_signal(), Some(Signum::SIGKILL));
}

#[test]
fn step_exit_group_does_not_set_terminating_signal() {
    let _g = setup();
    let proc_cap = bootstrap();

    step_exit_group(&proc_cap, 7);

    assert!(proc_cap.is_zombie());
    assert_eq!(proc_cap.exit_status(), Some(7));
    assert_eq!(proc_cap.terminating_signal(), None);
}
