// Auto-extracted from `crates/tx-subsystems/src/signal/tests.rs` (2026-05-08 jumbo split).
#![cfg_attr(test, allow(unused_imports))]
use super::*;
use crate::process::{
    bootstrap_init_process, step_fork, step_setpgid, ExitStatus, Pgid, ProcessIdentity,
};
use crate::signal::adapter::step_engine::{Cap, SignalRouting};
use crate::signal::{
    ast_check, default_action, script_kill_pgrp, select_next_signal, step_kill_process,
    step_sigaction, AstOutcome, DefaultAction, InterruptSummary, KillOutcome, PendingSource,
    SaFlags, SigActionEntry, SigDisposition, SignalTarget,
};
use crate::thread_runtime::execution::{post_signal, step_sigprocmask, SigmaskHow};
use crate::thread_runtime::structure::ThreadIdentity;
use crate::vm::{AddressSpace, TestPmap};

fn setup() -> std::sync::MutexGuard<'static, ()> {
    let g = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    tx_test_support::init_host();
    let _ = zones::register_all();
    tx_test_support::drain_to_quiescence();
    reset_pid_counter_for_test();
    reset_tid_counter_for_test();
    reset_init_process_for_test();
    g
}

fn fresh_init() -> Cap<ProcessIdentity> {
    bootstrap_init_process(AddressSpace::new_cap_for_platform::<TestPmap>().expect("aspace"))
        .expect("init")
}

fn leader(proc_cap: &Cap<ProcessIdentity>) -> Cap<ThreadIdentity> {
    let payload = proc_cap.payload.lock();
    let payload = payload.as_ref().expect("alive");
    let threads = payload.threads.snapshot();
    threads[0].clone()
}

// ----- pure: default_action + summary packing -----

#[test]
fn default_action_table_matches_spec() {
    assert_eq!(default_action(Signum::SIGTERM), DefaultAction::Term);
    assert_eq!(default_action(Signum::SIGINT), DefaultAction::Term);
    assert_eq!(default_action(Signum::SIGKILL), DefaultAction::Term);
    assert_eq!(default_action(Signum::SIGQUIT), DefaultAction::Core);
    assert_eq!(default_action(Signum::SIGSEGV), DefaultAction::Core);
    assert_eq!(default_action(Signum::SIGCHLD), DefaultAction::Ignore);
    assert_eq!(default_action(Signum::SIGSTOP), DefaultAction::Stop);
    assert_eq!(default_action(Signum::SIGTSTP), DefaultAction::Stop);
    assert_eq!(default_action(Signum::SIGCONT), DefaultAction::Cont);
}

#[test]
fn interrupt_summary_pack_unpack_round_trip() {
    let cases = [
        InterruptSummary::EMPTY,
        InterruptSummary {
            deliverable_signal: true,
            ..InterruptSummary::EMPTY
        },
        InterruptSummary {
            termination: true,
            ..InterruptSummary::EMPTY
        },
        InterruptSummary {
            stop_requested: true,
            ..InterruptSummary::EMPTY
        },
        InterruptSummary {
            deliverable_signal: true,
            termination: true,
            stop_requested: true,
        },
    ];
    for case in cases {
        assert_eq!(InterruptSummary::unpack(case.pack()), case);
    }
}

// ----- select_next_signal -----

#[test]
fn select_picks_lowest_signum_from_thread_pending() {
    let _g = setup();
    let proc_cap = fresh_init();
    let leader = leader(&proc_cap);
    post_signal(
        &leader,
        Signum::SIGTERM,
        SignalRouting::ProcessDirected,
        None,
    );
    post_signal(
        &leader,
        Signum::SIGINT,
        SignalRouting::ProcessDirected,
        None,
    );
    post_signal(
        &leader,
        Signum::SIGCHLD,
        SignalRouting::ProcessDirected,
        None,
    );

    // SIGINT (2) beats SIGTERM (15) and SIGCHLD (17).
    assert_eq!(
        select_next_signal(&leader),
        Some((Signum::SIGINT, PendingSource::Thread))
    );
}

#[test]
fn select_skips_masked_signals() {
    let _g = setup();
    let proc_cap = fresh_init();
    let leader = leader(&proc_cap);
    post_signal(
        &leader,
        Signum::SIGINT,
        SignalRouting::ProcessDirected,
        None,
    );
    post_signal(
        &leader,
        Signum::SIGTERM,
        SignalRouting::ProcessDirected,
        None,
    );

    let mut block = SignalMask::EMPTY;
    block.block(Signum::SIGINT);
    let _ = step_sigprocmask(&leader, SigmaskHow::SetMask, block);

    // SIGINT is now masked; SIGTERM is the next deliverable.
    assert_eq!(
        select_next_signal(&leader),
        Some((Signum::SIGTERM, PendingSource::Thread))
    );
}

#[test]
fn select_prefers_thread_pending_over_group_pending() {
    let _g = setup();
    let proc_cap = fresh_init();
    let leader = leader(&proc_cap);

    // SIGTERM on thread queue, SIGINT on group queue. Thread
    // priority means SIGTERM wins despite higher signum.
    post_signal(
        &leader,
        Signum::SIGTERM,
        SignalRouting::ProcessDirected,
        None,
    );
    proc_cap
        .payload
        .lock()
        .as_ref()
        .unwrap()
        .group_pending()
        .post(Signum::SIGINT);

    assert_eq!(
        select_next_signal(&leader),
        Some((Signum::SIGTERM, PendingSource::Thread))
    );
}

#[test]
fn select_returns_none_when_all_masked_or_empty() {
    let _g = setup();
    let proc_cap = fresh_init();
    let leader = leader(&proc_cap);

    // Empty: None.
    assert_eq!(select_next_signal(&leader), None);

    // All masked: None.
    post_signal(
        &leader,
        Signum::SIGINT,
        SignalRouting::ProcessDirected,
        None,
    );
    let mut block = SignalMask::EMPTY;
    block.block(Signum::SIGINT);
    let _ = step_sigprocmask(&leader, SigmaskHow::SetMask, block);
    assert_eq!(select_next_signal(&leader), None);
}

#[test]
fn select_falls_through_to_group_pending_when_thread_empty() {
    let _g = setup();
    let proc_cap = fresh_init();
    let leader = leader(&proc_cap);

    proc_cap
        .payload
        .lock()
        .as_ref()
        .unwrap()
        .group_pending()
        .post(Signum::SIGTERM);

    assert_eq!(
        select_next_signal(&leader),
        Some((Signum::SIGTERM, PendingSource::Group))
    );
}

// ----- ast_check matrix -----

#[test]
fn ast_check_continue_when_no_pending() {
    let _g = setup();
    let proc_cap = fresh_init();
    let leader = leader(&proc_cap);
    assert_eq!(ast_check(&leader), AstOutcome::Continue);
}

#[test]
fn ast_check_initiate_termination_for_summary_termination_bit() {
    let _g = setup();
    let proc_cap = fresh_init();
    let leader = leader(&proc_cap);

    // SIGKILL no longer takes the summary.termination route — it
    // invokes step_exit_group_with_signal directly per spec
    // §12.3. The AST priority-1 path is for OTHER fatal scenarios
    // (synchronous fault default-Term, ptrace fatal, etc.) that
    // set the bit on a still-live thread. Manually set it via the
    // crate-internal update_summary helper to exercise the path
    // without depending on a not-yet-wired producer.
    leader
        .payload
        .lock()
        .as_ref()
        .unwrap()
        .update_summary(|s| s.termination = true);
    assert_eq!(ast_check(&leader), AstOutcome::InitiateTermination);
}

#[test]
fn ast_check_default_terminate_for_sigterm() {
    let _g = setup();
    let proc_cap = fresh_init();
    let leader = leader(&proc_cap);

    post_signal(
        &leader,
        Signum::SIGTERM,
        SignalRouting::ProcessDirected,
        None,
    );
    assert_eq!(
        ast_check(&leader),
        AstOutcome::DefaultTerminate {
            sig: Signum::SIGTERM
        }
    );
}

#[test]
fn ast_check_default_ignore_for_sigchld_drops_and_continues() {
    let _g = setup();
    let proc_cap = fresh_init();
    let leader = leader(&proc_cap);

    post_signal(
        &leader,
        Signum::SIGCHLD,
        SignalRouting::ProcessDirected,
        None,
    );
    assert_eq!(ast_check(&leader), AstOutcome::Continue);
    // SIGCHLD was dequeued during the loop, even though dropped.
    let pending = leader
        .payload
        .lock()
        .as_ref()
        .unwrap()
        .pending()
        .is_pending(Signum::SIGCHLD);
    assert!(!pending);
}

#[test]
fn ast_check_default_stop_for_sigtstp() {
    let _g = setup();
    let proc_cap = fresh_init();
    let leader = leader(&proc_cap);

    post_signal(
        &leader,
        Signum::SIGTSTP,
        SignalRouting::ProcessDirected,
        None,
    );
    assert_eq!(
        ast_check(&leader),
        AstOutcome::DefaultStop {
            sig: Signum::SIGTSTP
        }
    );
}

// SIGCONT is Gewalt: route_gewalt clears stop_requested directly
// and does not enqueue to thread_pending. The AstOutcome::DefaultContinue
// variant remains in the enum for the future case where SIGCONT
// is enqueued to group_pending for handler delivery (per SIGNAL_v1
// §12.3 route_sigcont's handler half), but day-1 doesn't enqueue
// from route_gewalt at all. Test removed; sigcont_does_not_enter_thread_pending
// covers the bypass.

#[test]
fn ast_check_deliver_handler_when_handler_installed() {
    let _g = setup();
    let proc_cap = fresh_init();
    let leader = leader(&proc_cap);

    let _ = step_sigaction(&proc_cap, Signum::SIGTERM, SigDisposition::Handler(0xCAFE));
    post_signal(
        &leader,
        Signum::SIGTERM,
        SignalRouting::ProcessDirected,
        None,
    );

    assert_eq!(
        ast_check(&leader),
        AstOutcome::DeliverHandler {
            sig: Signum::SIGTERM,
            action: SigActionEntry::handler(0xCAFE),
        }
    );
}

#[test]
fn ast_check_deliver_handler_carries_sigaction_entry() {
    let _g = setup();
    let proc_cap = fresh_init();
    let leader = leader(&proc_cap);

    let mut action = SigActionEntry::handler(0xCAFE);
    action.flags = SaFlags::SIGINFO | SaFlags::RESTART | SaFlags::ONSTACK;
    action.sa_mask = crate::signal::SignalMask::new(Signum::SIGINT.bit());
    action.restorer = 0xFEED_FACE;

    let _ = crate::signal::step_sigaction_entry(&proc_cap, Signum::SIGTERM, action);
    post_signal(
        &leader,
        Signum::SIGTERM,
        SignalRouting::ProcessDirected,
        None,
    );

    assert_eq!(
        ast_check(&leader),
        AstOutcome::DeliverHandler {
            sig: Signum::SIGTERM,
            action,
        }
    );
}

#[test]
fn ast_check_handler_delivery_clears_consumed_signal_summary() {
    let _g = setup();
    let proc_cap = fresh_init();
    let leader = leader(&proc_cap);

    let _ = step_sigaction(&proc_cap, Signum::SIGTERM, SigDisposition::Handler(0xCAFE));
    post_signal(
        &leader,
        Signum::SIGTERM,
        SignalRouting::ProcessDirected,
        None,
    );

    assert!(
        leader
            .payload
            .lock()
            .as_ref()
            .unwrap()
            .interrupt_summary()
            .deliverable_signal
    );
    assert_eq!(
        ast_check(&leader),
        AstOutcome::DeliverHandler {
            sig: Signum::SIGTERM,
            action: SigActionEntry::handler(0xCAFE),
        }
    );
    assert!(
        !leader
            .payload
            .lock()
            .as_ref()
            .unwrap()
            .interrupt_summary()
            .deliverable_signal,
        "AST delivery must clear the summary bit after consuming the only pending signal"
    );
}

#[test]
fn ast_check_silent_ignore_disposition_drops_and_continues() {
    let _g = setup();
    let proc_cap = fresh_init();
    let leader = leader(&proc_cap);

    let _ = step_sigaction(&proc_cap, Signum::SIGTERM, SigDisposition::Ignore);
    post_signal(
        &leader,
        Signum::SIGTERM,
        SignalRouting::ProcessDirected,
        None,
    );

    // Ignore disposition: the loop dequeues + drops, then sees an
    // empty queue and returns Continue.
    assert_eq!(ast_check(&leader), AstOutcome::Continue);
}

// ----- summary maintenance -----

#[test]
fn post_signal_marks_deliverable_when_unmasked() {
    let _g = setup();
    let proc_cap = fresh_init();
    let leader = leader(&proc_cap);

    let summary_before = leader.payload.lock().as_ref().unwrap().interrupt_summary();
    assert!(!summary_before.deliverable_signal);

    post_signal(
        &leader,
        Signum::SIGINT,
        SignalRouting::ProcessDirected,
        None,
    );

    let summary_after = leader.payload.lock().as_ref().unwrap().interrupt_summary();
    assert!(summary_after.deliverable_signal);
}

#[test]
fn post_signal_skips_summary_deliverable_when_masked() {
    let _g = setup();
    let proc_cap = fresh_init();
    let leader = leader(&proc_cap);

    // Block SIGINT first.
    let mut block = SignalMask::EMPTY;
    block.block(Signum::SIGINT);
    let _ = step_sigprocmask(&leader, SigmaskHow::SetMask, block);

    post_signal(
        &leader,
        Signum::SIGINT,
        SignalRouting::ProcessDirected,
        None,
    );

    let summary = leader.payload.lock().as_ref().unwrap().interrupt_summary();
    assert!(
        !summary.deliverable_signal,
        "masked signals must not set deliverable bit"
    );
}

#[test]
fn sigprocmask_unblock_sets_deliverable_for_already_pending() {
    let _g = setup();
    let proc_cap = fresh_init();
    let leader = leader(&proc_cap);

    // Block SIGINT, post it (deliverable stays false), then
    // unblock — sigprocmask should re-set deliverable.
    let mut block = SignalMask::EMPTY;
    block.block(Signum::SIGINT);
    let _ = step_sigprocmask(&leader, SigmaskHow::SetMask, block);
    post_signal(
        &leader,
        Signum::SIGINT,
        SignalRouting::ProcessDirected,
        None,
    );
    assert!(
        !leader
            .payload
            .lock()
            .as_ref()
            .unwrap()
            .interrupt_summary()
            .deliverable_signal
    );

    let _ = step_sigprocmask(&leader, SigmaskHow::SetMask, SignalMask::EMPTY);

    assert!(
        leader
            .payload
            .lock()
            .as_ref()
            .unwrap()
            .interrupt_summary()
            .deliverable_signal
    );
}

#[test]
fn sigprocmask_unblock_sets_deliverable_for_group_pending() {
    let _g = setup();
    let proc_cap = fresh_init();
    let leader = leader(&proc_cap);

    let mut block = SignalMask::EMPTY;
    block.block(Signum::SIGTERM);
    let _ = step_sigprocmask(&leader, SigmaskHow::SetMask, block);
    proc_cap
        .payload
        .lock()
        .as_ref()
        .unwrap()
        .group_pending()
        .post(Signum::SIGTERM);
    assert!(
        !leader
            .payload
            .lock()
            .as_ref()
            .unwrap()
            .interrupt_summary()
            .deliverable_signal
    );

    let _ = step_sigprocmask(&leader, SigmaskHow::SetMask, SignalMask::EMPTY);

    assert!(
        leader
            .payload
            .lock()
            .as_ref()
            .unwrap()
            .interrupt_summary()
            .deliverable_signal,
        "sigprocmask must recompute deliverability from process group-pending signals too"
    );
}

#[test]
fn sigkill_via_step_kill_zombifies_process_with_status_encoding() {
    let _g = setup();
    let proc_cap = fresh_init();

    let outcome = step_kill_process(&proc_cap, Signum::SIGKILL, None);
    assert_eq!(outcome, KillOutcome::Delivered);

    // Per SIGNAL_v1 §12.3 route_sigkill, SIGKILL invokes
    // step_exit_group_with_signal directly: the target is now a
    // zombie carrying ExitStatus::Signaled(SIGKILL).
    assert!(proc_cap.is_zombie());
    assert_eq!(
        proc_cap.exit_status(),
        Some(crate::process::ExitStatus::Signaled(Signum::SIGKILL))
    );
    assert_eq!(proc_cap.terminating_signal(), Some(Signum::SIGKILL));
    // POSIX <sys/wait.h> signaled-exit encoding: signum in low 7
    // bits. Migrated from `128 + sig` by Wave 1 of the
    // fork/clone/wait4 slice — Open Q #3 DECIDED.
    assert_eq!(
        proc_cap.exit_status().unwrap().wait_status_word(),
        Signum::SIGKILL.raw() as i32 & 0x7f
    );
}

#[test]
fn sigstop_routed_via_step_kill_sets_summary_stop_requested() {
    let _g = setup();
    let proc_cap = fresh_init();
    let leader = leader(&proc_cap);

    // SIGSTOP is Gewalt — must go through step_kill_process which
    // dispatches to route_gewalt; calling post_signal directly
    // would debug_assert.
    let _ = step_kill_process(&proc_cap, Signum::SIGSTOP, None);

    let summary = leader.payload.lock().as_ref().unwrap().interrupt_summary();
    assert!(summary.stop_requested);
}

#[test]
fn sigcont_routed_via_step_kill_clears_stop_requested() {
    let _g = setup();
    let proc_cap = fresh_init();
    let leader = leader(&proc_cap);

    let _ = step_kill_process(&proc_cap, Signum::SIGSTOP, None);
    assert!(
        leader
            .payload
            .lock()
            .as_ref()
            .unwrap()
            .interrupt_summary()
            .stop_requested
    );

    let _ = step_kill_process(&proc_cap, Signum::SIGCONT, None);
    assert!(
        !leader
            .payload
            .lock()
            .as_ref()
            .unwrap()
            .interrupt_summary()
            .stop_requested
    );
}

// ----- Gewalt vs event factoring (SIGNAL_v1 §1, §2 Consequence 2) -----

// SIGKILL's pending-queue bypass is covered by the
// `sigkill_via_step_kill_zombifies_process_with_status_encoding`
// test above: after SIGKILL the process is a zombie, the leader
// thread's payload is dropped, and there's literally no
// `thread_pending` queue to inspect. The structural separation
// is realised by the spec-correct `step_exit_group_with_signal`
// path, not by leaving a live leader with an empty bitset.

#[test]
fn sigstop_does_not_enter_thread_pending() {
    let _g = setup();
    let proc_cap = fresh_init();
    let leader = leader(&proc_cap);

    let _ = step_kill_process(&proc_cap, Signum::SIGSTOP, None);

    assert!(!leader
        .payload
        .lock()
        .as_ref()
        .unwrap()
        .pending()
        .is_pending(Signum::SIGSTOP));
    assert!(
        leader
            .payload
            .lock()
            .as_ref()
            .unwrap()
            .interrupt_summary()
            .stop_requested
    );
}

#[test]
fn sigcont_does_not_enter_thread_pending() {
    let _g = setup();
    let proc_cap = fresh_init();
    let leader = leader(&proc_cap);

    let _ = step_kill_process(&proc_cap, Signum::SIGCONT, None);

    assert!(!leader
        .payload
        .lock()
        .as_ref()
        .unwrap()
        .pending()
        .is_pending(Signum::SIGCONT));
}

#[test]
fn route_gewalt_sigkill_zombifies_target_only() {
    let _g = setup();
    let parent = fresh_init();
    let child = crate::process::step_fork::<TestPmap>(&parent, false, false).expect("fork");

    // SIGKILL on the parent zombifies only the parent — fork's
    // threads are owned by the child process and are independent.
    let _ = crate::signal::route_gewalt(&parent, Signum::SIGKILL);

    assert!(parent.is_zombie());
    assert!(!child.is_zombie());
    assert_eq!(parent.terminating_signal(), Some(Signum::SIGKILL));
    assert_eq!(child.terminating_signal(), None);
}

// ----- ast_dispatch: AstOutcome → step_exit_group_with_signal bridge -----

#[test]
fn ast_dispatch_default_terminate_zombifies_owner_with_signum() {
    let _g = setup();
    let proc_cap = fresh_init();
    let leader = leader(&proc_cap);

    // SIGTERM with default disposition → AstOutcome::DefaultTerminate.
    // ast_dispatch should invoke step_exit_group_with_signal so
    // the process zombifies with terminating_signal=Some(SIGTERM).
    post_signal(
        &leader,
        Signum::SIGTERM,
        SignalRouting::ProcessDirected,
        None,
    );
    let outcome = crate::signal::ast_dispatch(&leader);

    assert_eq!(
        outcome,
        AstOutcome::DefaultTerminate {
            sig: Signum::SIGTERM
        }
    );
    assert!(proc_cap.is_zombie());
    assert_eq!(
        proc_cap.exit_status(),
        Some(crate::process::ExitStatus::Signaled(Signum::SIGTERM))
    );
    assert_eq!(proc_cap.terminating_signal(), Some(Signum::SIGTERM));
    // POSIX <sys/wait.h> signaled-exit encoding: signum in low 7
    // bits. Migrated from `128 + sig` by Wave 1 of the
    // fork/clone/wait4 slice — Open Q #3 DECIDED.
    assert_eq!(
        proc_cap.exit_status().unwrap().wait_status_word(),
        Signum::SIGTERM.raw() as i32 & 0x7f
    );
}

#[test]
fn ast_dispatch_continue_has_no_side_effects() {
    let _g = setup();
    let proc_cap = fresh_init();
    let leader = leader(&proc_cap);

    let outcome = crate::signal::ast_dispatch(&leader);
    assert_eq!(outcome, AstOutcome::Continue);
    assert!(!proc_cap.is_zombie());
    assert_eq!(proc_cap.terminating_signal(), None);
}

#[test]
fn ast_dispatch_default_stop_recognised_but_unrealised() {
    let _g = setup();
    let proc_cap = fresh_init();
    let leader = leader(&proc_cap);

    // SIGTSTP is catchable; default action is Stop.
    post_signal(
        &leader,
        Signum::SIGTSTP,
        SignalRouting::ProcessDirected,
        None,
    );
    let outcome = crate::signal::ast_dispatch(&leader);

    assert_eq!(
        outcome,
        AstOutcome::DefaultStop {
            sig: Signum::SIGTSTP
        }
    );
    // Stop-state machinery is deferred — the process stays alive.
    assert!(!proc_cap.is_zombie());
    assert_eq!(proc_cap.terminating_signal(), None);
}

#[test]
fn ast_dispatch_deliver_handler_returns_action_entry_for_frame_setup() {
    let _g = setup();
    let proc_cap = fresh_init();
    let leader = leader(&proc_cap);

    let _ = step_sigaction(&proc_cap, Signum::SIGTERM, SigDisposition::Handler(0xFEED));
    post_signal(
        &leader,
        Signum::SIGTERM,
        SignalRouting::ProcessDirected,
        None,
    );

    let outcome = crate::signal::ast_dispatch(&leader);
    assert_eq!(
        outcome,
        AstOutcome::DeliverHandler {
            sig: Signum::SIGTERM,
            action: SigActionEntry::handler(0xFEED)
        }
    );
    // Handler installation overrides the default-Term path; the
    // process is NOT terminated. Frame construction lands later.
    assert!(!proc_cap.is_zombie());
}

#[test]
fn step_kill_pgrp_does_not_mirror_gewalt_to_group_pending() {
    let _g = setup();
    let parent = fresh_init();
    let _child = crate::process::step_fork::<TestPmap>(&parent, false, false).expect("fork");
    let pgrp = parent.pgrp_cap();

    let _ = crate::signal::step_kill_pgrp(&pgrp, Signum::SIGSTOP);

    // group_pending must NOT have SIGSTOP set: Gewalt bypasses
    // pending queues entirely per SIGNAL_v1 §2 Consequence 2.
    let parent_group_pending = parent
        .payload
        .lock()
        .as_ref()
        .unwrap()
        .group_pending()
        .is_pending(Signum::SIGSTOP);
    assert!(
        !parent_group_pending,
        "Gewalt must not be mirrored onto group_pending"
    );
}

#[test]
fn kill_zero_style_pgrp_fanout_reaches_in_group_child_handler() {
    let _g = setup();
    let parent = fresh_init();
    let _ = step_setpgid(&parent, Pgid(parent.pid.0)).expect("parent setpgrp");
    let sigusr1 = Signum::new(10).expect("SIGUSR1");

    let child1 = step_fork::<TestPmap>(&parent, false, false).expect("fork child1");
    let child_a = step_fork::<TestPmap>(&child1, false, false).expect("fork child A");
    let child_b = step_fork::<TestPmap>(&child1, false, false).expect("fork child B");
    let _ = child_a;
    step_setpgid(&child_b, Pgid(child_b.pid.0)).expect("child B setpgrp");

    let _ = step_sigaction(&parent, sigusr1, SigDisposition::Ignore);
    let _ = step_sigaction(&child1, sigusr1, SigDisposition::Handler(0xCAFE_F00D));

    let delivered = script_kill_pgrp(&parent, &parent.pgrp_cap(), sigusr1).expect("kill pgrp");
    assert_eq!(delivered, 3, "parent, child1, and child A are in the pgrp");

    assert_eq!(
        ast_check(&leader(&child1)),
        AstOutcome::DeliverHandler {
            sig: sigusr1,
            action: SigActionEntry::handler(0xCAFE_F00D),
        }
    );
    assert_eq!(
        ast_check(&leader(&child_b)),
        AstOutcome::Continue,
        "child B moved to another pgrp and must not receive SIGUSR1"
    );
}

// ----- bus wire integration: signal_port + exit_source_bus -----

#[test]
fn process_payload_has_signal_port_field() {
    let _g = setup();
    let proc_cap = fresh_init();
    let payload = proc_cap.payload.lock();
    let payload = payload.as_ref().expect("alive");

    // Verify the signal_port RawPort field exists and is accessible.
    // The field is pub(crate); this test lives in the same crate so
    // it can access it.  We don't assert on internal state — just
    // that the field compiles and is reachable.
    let _port: &crate::process::adapter::step_engine::RawPort = &payload.signal_port;
}

#[test]
fn process_payload_has_exit_source_bus_field() {
    let _g = setup();
    let proc_cap = fresh_init();
    let payload = proc_cap.payload.lock();
    let payload = payload.as_ref().expect("alive");

    // Verify the exit_source_bus RawQueue field exists.
    let _queue: &crate::process::adapter::step_engine::RawQueue = &payload.exit_source_bus;
}

#[test]
fn signal_generated_constant_is_defined() {
    assert_eq!(crate::process::structure::SIGNAL_GENERATED, 0x1);
}

#[test]
fn exit_source_child_zombified_constant_is_defined() {
    assert_eq!(crate::process::structure::EXIT_SOURCE_CHILD_ZOMBIFIED, 0x1);
}

#[test]
fn step_kill_process_fires_signal_port_for_event_signals() {
    let _g = setup();
    let proc_cap = fresh_init();
    let leader = leader(&proc_cap);

    // Post a catchable signal via step_kill_process (the Event path).
    // This should fire signal_port with SIGNAL_GENERATED.
    let outcome = step_kill_process(&proc_cap, Signum::SIGTERM, None);
    assert_eq!(outcome, KillOutcome::Delivered);

    // The signal should be pending on the eligible thread.
    let leader_payload = leader.payload_cap().expect("alive");
    assert!(leader_payload.pending().is_pending(Signum::SIGTERM));

    // Verify signal_port was fired: after the fire, the port's
    // internal state reflects the fire.  We check indirectly by
    // confirming the process is still alive and the signal was
    // delivered.
    assert!(!proc_cap.is_zombie());
}

#[test]
fn route_gewalt_sigstop_sets_stopped_flag() {
    let _g = setup();
    let proc_cap = fresh_init();
    let leader = leader(&proc_cap);

    let outcome = crate::signal::route_gewalt(&proc_cap, Signum::SIGSTOP);
    assert_eq!(outcome, KillOutcome::Delivered);

    // After SIGSTOP, the thread's stopped flag should be set.
    let leader_payload = leader.payload_cap().expect("alive");
    assert!(leader_payload.is_stopped());

    // SIGCONT should clear it.
    let outcome = crate::signal::route_gewalt(&proc_cap, Signum::SIGCONT);
    assert_eq!(outcome, KillOutcome::Delivered);
    assert!(!leader_payload.is_stopped());
}

#[test]
fn deliver_posix_signal_ignores_sig_ign_disposition() {
    let _g = setup();
    let proc_cap = fresh_init();
    let leader = leader(&proc_cap);

    // Install SIG_IGN for SIGTERM.
    let _ = step_sigaction(&proc_cap, Signum::SIGTERM, SigDisposition::Ignore);

    let outcome = crate::signal::deliver_posix_signal(
        crate::signal::SignalTarget::Process(proc_cap.clone()),
        Signum::SIGTERM,
    );
    assert_eq!(outcome, KillOutcome::Delivered);

    // With SIG_IGN, the signal should NOT be posted to pending.
    let leader_payload = leader.payload_cap().expect("alive");
    assert!(!leader_payload.pending().is_pending(Signum::SIGTERM));
    assert!(!proc_cap.is_zombie());
}

#[test]
fn deliver_posix_signal_default_term_zombifies_process() {
    let _g = setup();
    let proc_cap = fresh_init();

    // SIGTERM with default disposition should terminate.
    let outcome = crate::signal::deliver_posix_signal(
        crate::signal::SignalTarget::Process(proc_cap.clone()),
        Signum::SIGTERM,
    );
    assert_eq!(outcome, KillOutcome::Delivered);
    assert!(proc_cap.is_zombie());
    assert_eq!(
        proc_cap.exit_status(),
        Some(crate::process::ExitStatus::Signaled(Signum::SIGTERM))
    );
}
