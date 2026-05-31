// Auto-extracted from `crates/tx-subsystems/src/signal/tests.rs` (2026-05-08 jumbo split).
#![cfg_attr(test, allow(unused_imports))]
use super::*;
use crate::cred::{signal_permitted, Capability, CapabilitySet, Cred, CredSnapshot, Gid, Uid};
use crate::execution::Errno;
use crate::process::structure::{ProcessIdentity, TargetProcCred};
use crate::signal::adapter::step_engine::Cap;
use crate::signal::{
    script_kill_pgrp, script_kill_probe, script_kill_process, step_sigaction, KillScriptOutcome,
    SigDisposition,
};

fn set_cred(proc_cap: &Cap<ProcessIdentity>, cred: Cred) {
    // PR-9 phase 5 (D5 Path A): `cred` lives in `AtomicSlot<Cap<Cred>>`.
    let payload_guard = proc_cap.payload.lock();
    let payload = payload_guard.as_ref().expect("alive");
    let new_cap = crate::cred::sign_cred(cred).expect("zone slab has capacity in tests");
    let _old = payload.replace_cred(new_cap);
}

fn limited_cred(uid: u32) -> Cred {
    Cred {
        uid: Uid(uid),
        euid: Uid(uid),
        suid: Uid(uid),
        gid: Gid(uid),
        egid: Gid(uid),
        sgid: Gid(uid),
        effective_caps: CapabilitySet::EMPTY,
        permitted_caps: CapabilitySet::EMPTY,
    }
}

fn target_with(uid: u32, euid: u32, same_session: bool) -> TargetProcCred {
    TargetProcCred {
        uid: Uid(uid),
        euid: Uid(euid),
        gid: crate::cred::Gid(0),
        egid: crate::cred::Gid(0),
        same_session,
    }
}

fn cred_with(uid: u32, euid: u32) -> Cred {
    let mut effective_caps = CapabilitySet::EMPTY;
    // Explicitly drop CAP_KILL so the rule path that's not the
    // cap-bypass actually exercises the uid match.
    effective_caps.remove(Capability::KILL);
    Cred {
        uid: Uid(uid),
        euid: Uid(euid),
        suid: Uid(euid),
        gid: crate::cred::Gid(1),
        egid: crate::cred::Gid(1),
        sgid: crate::cred::Gid(1),
        effective_caps,
        permitted_caps: CapabilitySet::EMPTY,
    }
}

// ----- pure rule (no zone setup) -----

#[test]
fn same_euid_passes() {
    let src = cred_with(1000, 1000);
    let tgt = target_with(1000, 1000, false);
    assert!(signal_permitted(
        &CredSnapshot::from_cred(src),
        &tgt,
        Signum::SIGTERM
    ));
}

#[test]
fn different_euid_fails_without_capability() {
    let src = cred_with(1000, 1000);
    let tgt = target_with(2000, 2000, false);
    assert!(!signal_permitted(
        &CredSnapshot::from_cred(src),
        &tgt,
        Signum::SIGTERM
    ));
}

#[test]
fn cap_kill_overrides_euid_mismatch() {
    let mut effective_caps = CapabilitySet::EMPTY;
    effective_caps.add(Capability::KILL);
    let src = Cred {
        uid: Uid(1000),
        euid: Uid(1000),
        suid: Uid(1000),
        gid: crate::cred::Gid(0),
        egid: crate::cred::Gid(0),
        sgid: crate::cred::Gid(0),
        effective_caps,
        permitted_caps: CapabilitySet::EMPTY,
    };
    let tgt = target_with(2000, 2000, false);
    assert!(signal_permitted(
        &CredSnapshot::from_cred(src),
        &tgt,
        Signum::SIGTERM
    ));
}

#[test]
fn root_overrides_euid_mismatch() {
    let src = Cred::root();
    let tgt = target_with(2000, 2000, false);
    assert!(signal_permitted(
        &CredSnapshot::from_cred(src),
        &tgt,
        Signum::SIGTERM
    ));
}

#[test]
fn sigcont_same_session_passes_regardless_of_uid() {
    let src = cred_with(1000, 1000);
    let tgt = target_with(2000, 2000, true);
    assert!(signal_permitted(
        &CredSnapshot::from_cred(src),
        &tgt,
        Signum::SIGCONT
    ));
}

#[test]
fn sigcont_different_session_still_requires_cred_match() {
    let src = cred_with(1000, 1000);
    let tgt = target_with(2000, 2000, false);
    assert!(!signal_permitted(
        &CredSnapshot::from_cred(src),
        &tgt,
        Signum::SIGCONT
    ));
}

// ----- integration tests against real Process / PGroup graph -----

fn setup() -> std::sync::MutexGuard<'static, ()> {
    let guard = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    tx_test_support::init_host();
    let _ = zones::register_all();
    tx_test_support::drain_to_quiescence();
    reset_pid_counter_for_test();
    reset_tid_counter_for_test();
    reset_init_process_for_test();
    guard
}

fn fresh_aspace() -> Cap<crate::vm::AddressSpace> {
    crate::vm::AddressSpace::new_cap_for_platform::<crate::vm::TestPmap>().expect("aspace")
}

fn fresh_init() -> Cap<crate::process::ProcessIdentity> {
    crate::process::bootstrap_init_process(fresh_aspace()).expect("init")
}

#[test]
fn script_kill_process_same_uid_delivers() {
    let _g = setup();
    let parent = fresh_init();
    let child =
        crate::process::step_fork::<crate::vm::TestPmap>(&parent, false, false).expect("fork");
    // Both inherit root cred; same euid.

    let outcome = script_kill_process(&parent, &child, Signum::SIGTERM, None);
    assert_eq!(outcome, Ok(KillScriptOutcome::Delivered));
}

#[test]
fn script_kill_process_different_uid_returns_eperm() {
    let _g = setup();
    let parent = fresh_init();
    let child =
        crate::process::step_fork::<crate::vm::TestPmap>(&parent, false, false).expect("fork");

    // Drop both to non-privileged uids that don't share. We set
    // creds directly because day-1 `step_setuid` retains caps
    // (the cap-drop-on-deprivilege transition machinery is
    // phase-2 per the cred doc); the kill rule itself is
    // independent of how a process arrived at its cred.
    set_cred(&parent, limited_cred(1000));
    set_cred(&child, limited_cred(2000));

    let outcome = script_kill_process(&parent, &child, Signum::SIGTERM, None);
    assert_eq!(outcome, Err(Errno::EPERM));
}

#[test]
fn script_kill_process_zombie_target_returns_no_live_thread() {
    let _g = setup();
    let parent = fresh_init();
    let child =
        crate::process::step_fork::<crate::vm::TestPmap>(&parent, false, false).expect("fork");
    crate::process::step_exit_group(&child, ExitStatus::Exited(0));

    // Even without permission, target_proc_cred returns None for
    // a zombie, short-circuiting before the cred check.
    let outcome = script_kill_process(&parent, &child, Signum::SIGTERM, None);
    assert_eq!(outcome, Ok(KillScriptOutcome::NoLiveThread));
}

#[test]
fn script_kill_process_zombie_source_returns_esrch() {
    let _g = setup();
    let parent = fresh_init();
    let child =
        crate::process::step_fork::<crate::vm::TestPmap>(&parent, false, false).expect("fork");
    crate::process::step_exit_group(&parent, ExitStatus::Exited(0));

    let outcome = script_kill_process(&parent, &child, Signum::SIGTERM, None);
    assert_eq!(outcome, Err(Errno::ESRCH));
}

#[test]
fn script_kill_pgrp_partial_permission_returns_count_of_permitted() {
    let _g = setup();
    let parent = fresh_init();
    let child_a =
        crate::process::step_fork::<crate::vm::TestPmap>(&parent, false, false).expect("fork a");
    let child_b =
        crate::process::step_fork::<crate::vm::TestPmap>(&parent, false, false).expect("fork b");

    // Set up a partial-permission scenario:
    //   parent (sender):    uid=1000, no caps
    //   parent  (in pgrp):   uid=1000, no caps   — matches sender uid
    //   child_a (in pgrp):   uid=1000, no caps   — matches sender uid
    //   child_b (in pgrp):   uid=2000, no caps   — denied
    // Expect: 2 delivered (parent + child_a).
    set_cred(&parent, limited_cred(1000));
    set_cred(&child_a, limited_cred(1000));
    set_cred(&child_b, limited_cred(2000));
    let sigusr1 = Signum::new(10).expect("SIGUSR1");
    step_sigaction(&parent, sigusr1, SigDisposition::Handler(0xCAFE));
    step_sigaction(&child_a, sigusr1, SigDisposition::Handler(0xCAFE));
    step_sigaction(&child_b, sigusr1, SigDisposition::Handler(0xCAFE));

    let pgrp = parent.pgrp_cap();
    let delivered = script_kill_pgrp(&parent, &pgrp, sigusr1).expect("not zombie");
    assert_eq!(delivered, 2);

    let pending_on = |proc: &Cap<ProcessIdentity>| {
        let payload = proc.payload.lock();
        payload
            .as_ref()
            .map(|p| p.threads.nth(0).unwrap())
            .unwrap()
            .payload
            .lock()
            .as_ref()
            .map(|tp| tp.pending().is_pending(sigusr1))
            .unwrap()
    };
    assert!(pending_on(&parent), "parent received the post");
    assert!(pending_on(&child_a), "child_a permitted");
    assert!(!pending_on(&child_b), "child_b denied");
}

#[test]
fn authorize_signal_send_yields_three_state_outcome() {
    // Pin the three-state contract of `cred::checks::authorize_signal_send`:
    //   • same uid                  → Ok(Authorized)
    //   • different uid, no caps    → Err(EPERM)
    //   • target zombie             → Ok(NoLiveTarget)
    //   • source zombie             → Err(ESRCH)
    // The four outcomes drive the dispatch branches in every signal
    // script that consumes the combinator.
    use crate::cred::checks::{authorize_signal_send, AuthOutcome};

    let _g = setup();
    let parent = fresh_init();
    let child =
        crate::process::step_fork::<crate::vm::TestPmap>(&parent, false, false).expect("fork");

    // Same uid (both root inherited from bootstrap).
    assert_eq!(
        authorize_signal_send(&parent, &child, Signum::SIGTERM),
        Ok(AuthOutcome::Authorized),
    );

    // Mismatched non-privileged uids.
    set_cred(&parent, limited_cred(1000));
    set_cred(&child, limited_cred(2000));
    assert_eq!(
        authorize_signal_send(&parent, &child, Signum::SIGTERM),
        Err(Errno::EPERM),
    );

    // Zombie target: re-bootstrap a fresh child, reap it, and check.
    crate::process::step_exit_group(&child, crate::process::ExitStatus::Exited(0));
    assert_eq!(
        authorize_signal_send(&parent, &child, Signum::SIGTERM),
        Ok(AuthOutcome::NoLiveTarget),
    );

    // Zombie source.
    let live_target =
        crate::process::step_fork::<crate::vm::TestPmap>(&parent, false, false).expect("fork");
    crate::process::step_exit_group(&parent, crate::process::ExitStatus::Exited(0));
    assert_eq!(
        authorize_signal_send(&parent, &live_target, Signum::SIGTERM),
        Err(Errno::ESRCH),
    );
}

#[test]
fn script_deliver_signal_to_thread_denied_for_mismatched_uid() {
    use crate::signal::{script_deliver_signal, KillOutcome, SignalTarget};

    let _g = setup();
    let parent = fresh_init();
    let child =
        crate::process::step_fork::<crate::vm::TestPmap>(&parent, false, false).expect("fork");

    set_cred(&parent, limited_cred(1000));
    set_cred(&child, limited_cred(2000));

    // Target the child's leader thread directly. The script must
    // resolve the thread → owning process → cred check, and the
    // mismatched uids without CAP_KILL must surface as EPERM.
    let leader = {
        let payload = child.payload.lock();
        let payload = payload.as_ref().expect("alive");
        payload.threads.nth(0).expect("leader")
    };

    let outcome =
        script_deliver_signal(&parent, SignalTarget::Thread(leader), Signum::SIGTERM, None);
    assert_eq!(outcome, Err(Errno::EPERM));

    // No post should have happened — verify the child's leader has
    // no pending SIGTERM.
    let pending = {
        let payload = child.payload.lock();
        let payload = payload.as_ref().unwrap();
        let leader = payload.threads.nth(0).unwrap();
        let lp = leader.payload.lock();
        lp.as_ref().unwrap().pending().is_pending(Signum::SIGTERM)
    };
    assert!(!pending, "denied delivery must not post");
}

#[test]
fn script_deliver_signal_to_thread_delivers_when_authorized() {
    use crate::signal::{script_deliver_signal, KillOutcome, SignalTarget};

    let _g = setup();
    let parent = fresh_init();
    let child =
        crate::process::step_fork::<crate::vm::TestPmap>(&parent, false, false).expect("fork");

    // Both inherit root cred → same euid → permitted.
    let leader = {
        let payload = child.payload.lock();
        let payload = payload.as_ref().expect("alive");
        payload.threads.nth(0).expect("leader")
    };

    let outcome =
        script_deliver_signal(&parent, SignalTarget::Thread(leader), Signum::SIGTERM, None);
    assert_eq!(outcome, Ok(KillOutcome::Delivered));
}

#[test]
fn script_deliver_signal_to_thread_posts_to_requested_tid() {
    use crate::process::execution::spawn_sibling_thread_for_test;
    use crate::signal::{script_deliver_signal, KillOutcome, SignalTarget};

    let _g = setup();
    let proc = fresh_init();
    let leader = {
        let payload = proc.payload.lock();
        let payload = payload.as_ref().expect("alive");
        payload.threads.nth(0).expect("leader")
    };
    let sibling = spawn_sibling_thread_for_test(&proc).expect("sibling thread");
    step_sigaction(&proc, Signum::SIGTERM, SigDisposition::Handler(0xCAFE_F00D));

    let outcome = script_deliver_signal(
        &proc,
        SignalTarget::Thread(sibling.clone()),
        Signum::SIGTERM,
        None,
    );
    assert_eq!(outcome, Ok(KillOutcome::Delivered));

    assert!(
        !leader
            .payload_cap()
            .expect("leader live")
            .pending()
            .is_pending(Signum::SIGTERM),
        "thread-directed delivery must not post to the leader"
    );
    assert!(
        sibling
            .payload_cap()
            .expect("sibling live")
            .pending()
            .is_pending(Signum::SIGTERM),
        "thread-directed delivery must post to the requested tid"
    );
}

#[test]
fn signal_zero_is_permission_probe_no_delivery() {
    let _g = setup();
    let parent = fresh_init();
    let child =
        crate::process::step_fork::<crate::vm::TestPmap>(&parent, false, false).expect("fork");

    // Same uid — probe should succeed without delivery.
    let outcome = script_kill_probe(&parent, &child).expect("probe");
    assert_eq!(outcome, KillScriptOutcome::Probed);

    // Verify nothing was actually posted to child.
    let leader_pending = {
        let payload = child.payload.lock();
        let payload = payload.as_ref().unwrap();
        let leader = payload.threads.nth(0).unwrap();
        let lp = leader.payload.lock();
        lp.as_ref().unwrap().pending().is_pending(Signum::SIGTERM)
    };
    assert!(!leader_pending, "probe must not deliver");

    // Drop both to mismatched non-privileged uids; the next probe
    // must be denied.
    set_cred(&parent, limited_cred(1000));
    set_cred(&child, limited_cred(2000));

    let outcome = script_kill_probe(&parent, &child);
    assert_eq!(outcome, Err(Errno::EPERM));
}
