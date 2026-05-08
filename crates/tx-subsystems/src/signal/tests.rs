//! Signal-shim and signal-state tests.
//!
//! Cover the day-1 surface: bitset + mask semantics, kill routing
//! (process and pgrp), sigaction installation, sigprocmask updates,
//! and zombie-ignored cases.

use crate::process::execution::reset_init_process_for_test;
use crate::process::structure::{reset_pid_counter_for_test, ProcessIdentity};
use crate::process::{bootstrap_init_process, step_exit_group, step_fork, ExitStatus};
use crate::signal::{
    step_kill_pgrp, step_kill_process, step_sigaction, KillOutcome, PendingSignalQueue,
    SigDisposition, SigDispositionChange, SignalMask, Signum,
};
use crate::test_support::EPOCH_TEST_LOCK;
use crate::thread_runtime::execution::{step_sigprocmask, SigmaskHow, SigprocmaskChange};
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
fn signum_constants_have_expected_bit_positions() {
    assert_eq!(Signum::SIGHUP.raw(), 1);
    assert_eq!(Signum::SIGHUP.bit(), 0x1);
    assert_eq!(Signum::SIGTERM.raw(), 15);
    assert_eq!(Signum::SIGTERM.bit(), 1u64 << 14);
    assert!(Signum::new(0).is_none());
    assert!(Signum::new(65).is_none());
    assert!(Signum::new(64).is_some());
}

#[test]
fn signal_mask_strips_uncatchable_bits_on_construct_and_block() {
    let mut m = SignalMask::new(u64::MAX);
    // SIGKILL (9) and SIGSTOP (19) bits should always be clear.
    assert!(!m.is_blocked(Signum::SIGKILL));
    assert!(!m.is_blocked(Signum::SIGSTOP));

    m.block(Signum::SIGKILL);
    m.block(Signum::SIGSTOP);
    assert!(!m.is_blocked(Signum::SIGKILL));
    assert!(!m.is_blocked(Signum::SIGSTOP));

    m.block(Signum::SIGTERM);
    assert!(m.is_blocked(Signum::SIGTERM));
    m.unblock(Signum::SIGTERM);
    assert!(!m.is_blocked(Signum::SIGTERM));
}

#[test]
fn pending_queue_post_clear_and_deliverable_with_mask() {
    let q = PendingSignalQueue::new();
    assert!(!q.is_pending(Signum::SIGTERM));

    q.post(Signum::SIGTERM);
    q.post(Signum::SIGINT);
    assert!(q.is_pending(Signum::SIGTERM));
    assert!(q.is_pending(Signum::SIGINT));

    let mut mask = SignalMask::EMPTY;
    mask.block(Signum::SIGTERM);
    let deliverable = q.deliverable_bits(mask);
    assert_eq!(deliverable & Signum::SIGTERM.bit(), 0); // blocked
    assert_ne!(deliverable & Signum::SIGINT.bit(), 0); // unblocked

    q.clear(Signum::SIGTERM);
    assert!(!q.is_pending(Signum::SIGTERM));
}

#[test]
fn kill_process_routes_signal_to_first_live_thread() {
    let _g = setup();
    let proc_cap = bootstrap();
    let leader = first_thread(&proc_cap);

    let outcome = step_kill_process(&proc_cap, Signum::SIGTERM);
    assert_eq!(outcome, KillOutcome::Delivered);

    let pending = leader
        .payload
        .lock()
        .as_ref()
        .map(|p| p.pending().is_pending(Signum::SIGTERM));
    assert_eq!(pending, Some(true));
}

#[test]
fn kill_zombie_process_returns_no_live_thread() {
    let _g = setup();
    let proc_cap = bootstrap();
    step_exit_group(&proc_cap, ExitStatus::Exited(0));

    let outcome = step_kill_process(&proc_cap, Signum::SIGTERM);
    assert_eq!(outcome, KillOutcome::NoLiveThread);
}

#[test]
fn kill_pgrp_fans_out_to_every_live_member() {
    let _g = setup();
    let parent = bootstrap();
    let child_a = step_fork::<TestPmap>(&parent).expect("fork a");
    let child_b = step_fork::<TestPmap>(&parent).expect("fork b");
    let pgrp = parent.pgrp_cap();

    let delivered = step_kill_pgrp(&pgrp, Signum::SIGINT);
    assert_eq!(delivered, 3); // parent + 2 children

    for proc_cap in [&parent, &child_a, &child_b] {
        let leader = first_thread(proc_cap);
        let pending = leader
            .payload
            .lock()
            .as_ref()
            .map(|p| p.pending().is_pending(Signum::SIGINT));
        assert_eq!(pending, Some(true));
    }
}

#[test]
fn kill_pgrp_skips_zombie_members_in_count() {
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent).expect("fork");
    step_exit_group(&child, ExitStatus::Exited(0));

    let pgrp = parent.pgrp_cap();
    let delivered = step_kill_pgrp(&pgrp, Signum::SIGTERM);
    assert_eq!(delivered, 1); // only parent received
}

#[test]
fn sigaction_installs_handler_and_returns_previous_disposition() {
    let _g = setup();
    let proc_cap = bootstrap();

    let first = step_sigaction(&proc_cap, Signum::SIGTERM, SigDisposition::Ignore);
    assert!(matches!(
        first,
        SigDispositionChange::Replaced {
            prev: SigDisposition::Default
        }
    ));

    let second = step_sigaction(
        &proc_cap,
        Signum::SIGTERM,
        SigDisposition::Handler(0xdead_beef),
    );
    assert!(matches!(
        second,
        SigDispositionChange::Replaced {
            prev: SigDisposition::Ignore
        }
    ));
}

#[test]
fn sigaction_refuses_to_change_uncatchable_disposition() {
    let _g = setup();
    let proc_cap = bootstrap();

    let outcome = step_sigaction(&proc_cap, Signum::SIGKILL, SigDisposition::Ignore);
    assert!(matches!(
        outcome,
        SigDispositionChange::Uncatchable(SigDisposition::Default)
    ));

    // Still Default afterwards.
    let payload_guard = proc_cap.payload.lock();
    let payload = payload_guard.as_ref().expect("alive");
    assert_eq!(
        payload.sig_actions().get(Signum::SIGKILL),
        SigDisposition::Default
    );
}

#[test]
fn sigaction_on_zombie_process_is_zombie_ignored() {
    let _g = setup();
    let proc_cap = bootstrap();
    step_exit_group(&proc_cap, ExitStatus::Exited(0));

    let outcome = step_sigaction(&proc_cap, Signum::SIGTERM, SigDisposition::Ignore);
    assert_eq!(outcome, SigDispositionChange::ZombieIgnored);
}

#[test]
fn sigprocmask_setmask_replaces_blocked_set() {
    let _g = setup();
    let proc_cap = bootstrap();
    let leader = first_thread(&proc_cap);

    let mut next = SignalMask::EMPTY;
    next.block(Signum::SIGTERM);
    next.block(Signum::SIGINT);

    let change = step_sigprocmask(&leader, SigmaskHow::SetMask, next);
    let SigprocmaskChange::Replaced { prev, new } = change else {
        panic!("expected Replaced, got {change:?}");
    };
    assert_eq!(prev, SignalMask::EMPTY);
    assert!(new.is_blocked(Signum::SIGTERM));
    assert!(new.is_blocked(Signum::SIGINT));
}

#[test]
fn sigprocmask_block_then_unblock_round_trips() {
    let _g = setup();
    let proc_cap = bootstrap();
    let leader = first_thread(&proc_cap);

    let mut to_block = SignalMask::EMPTY;
    to_block.block(Signum::SIGTERM);
    let _ = step_sigprocmask(&leader, SigmaskHow::Block, to_block);
    assert!(leader
        .payload
        .lock()
        .as_ref()
        .unwrap()
        .signal_mask()
        .is_blocked(Signum::SIGTERM));

    let _ = step_sigprocmask(&leader, SigmaskHow::Unblock, to_block);
    assert!(!leader
        .payload
        .lock()
        .as_ref()
        .unwrap()
        .signal_mask()
        .is_blocked(Signum::SIGTERM));
}

#[test]
fn deliverable_bits_filter_blocked_pending_correctly() {
    let _g = setup();
    let proc_cap = bootstrap();
    let leader = first_thread(&proc_cap);

    // Block SIGTERM, then post both SIGTERM and SIGINT.
    let mut block = SignalMask::EMPTY;
    block.block(Signum::SIGTERM);
    let _ = step_sigprocmask(&leader, SigmaskHow::Block, block);

    step_kill_process(&proc_cap, Signum::SIGTERM);
    step_kill_process(&proc_cap, Signum::SIGINT);

    let payload_guard = leader.payload.lock();
    let payload = payload_guard.as_ref().expect("alive");
    let mask = payload.signal_mask();
    let deliverable = payload.pending().deliverable_bits(mask);
    assert_eq!(deliverable & Signum::SIGTERM.bit(), 0);
    assert_ne!(deliverable & Signum::SIGINT.bit(), 0);
}

// ---------------------------------------------------------------------------
// Permission rule + script_kill (SIGNAL_v1 §32 + cred_service_v_1)
// ---------------------------------------------------------------------------

mod kill_permission;

// ---------------------------------------------------------------------------
// TTY → signal end-to-end (typed dispatch)
// ---------------------------------------------------------------------------

mod tty_bridge;

// ---------------------------------------------------------------------------
// Delivery sweep: select_next_signal + ast_check + summary maintenance
// ---------------------------------------------------------------------------

mod delivery;

// ----- Wave 2 ELF loader plan: SigActionTable::step_reset_for_exec --------
//
// Per `txdoc:EXEC-12-3-RESET-SIGNAL-DISPOSITIONS`,
// `txdoc:EXEC-16-SIGNAL-RESET-SEMANTICS`, and `SIGNAL_v1` §15.2: exec
// resets every user-installed handler to `SigDisposition::Default`,
// preserves `Default` and `Ignore` slots, and does NOT clear pending
// signals.
mod step_reset_for_exec_tests;
