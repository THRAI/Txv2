//! Signal-shim and signal-state tests.
//!
//! Cover the day-1 surface: bitset + mask semantics, kill routing
//! (process and pgrp), sigaction installation, sigprocmask updates,
//! and zombie-ignored cases.

use crate::process::structure::{reset_pid_counter_for_test, ProcessIdentity};
use crate::process::{bootstrap_init_process, step_exit_group, step_fork};
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
    step_exit_group(&proc_cap, 0);

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
    step_exit_group(&child, 0);

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
    step_exit_group(&proc_cap, 0);

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

mod kill_permission {
    use super::*;
    use crate::cred::{signal_permitted, Capability, CapabilitySet, Cred, Gid, Uid};
    use crate::execution::Errno;
    use crate::process::structure::{ProcessIdentity, TargetProcCred};
    use crate::signal::{
        script_kill_pgrp, script_kill_probe, script_kill_process, KillScriptOutcome,
    };
    use tx_substrate::zone::Cap;

    fn set_cred(proc_cap: &Cap<ProcessIdentity>, cred: Cred) {
        let payload_guard = proc_cap.payload.lock();
        *payload_guard.as_ref().expect("alive").cred.lock() = cred;
    }

    fn limited_cred(uid: u32) -> Cred {
        Cred {
            uid: Uid(uid),
            euid: Uid(uid),
            gid: Gid(uid),
            egid: Gid(uid),
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
            gid: crate::cred::Gid(1),
            egid: crate::cred::Gid(1),
            effective_caps,
            permitted_caps: CapabilitySet::EMPTY,
        }
    }

    // ----- pure rule (no zone setup) -----

    #[test]
    fn same_euid_passes() {
        let src = cred_with(1000, 1000);
        let tgt = target_with(1000, 1000, false);
        assert!(signal_permitted(src, &tgt, Signum::SIGTERM));
    }

    #[test]
    fn different_euid_fails_without_capability() {
        let src = cred_with(1000, 1000);
        let tgt = target_with(2000, 2000, false);
        assert!(!signal_permitted(src, &tgt, Signum::SIGTERM));
    }

    #[test]
    fn cap_kill_overrides_euid_mismatch() {
        let mut effective_caps = CapabilitySet::EMPTY;
        effective_caps.add(Capability::KILL);
        let src = Cred {
            uid: Uid(1000),
            euid: Uid(1000),
            gid: crate::cred::Gid(0),
            egid: crate::cred::Gid(0),
            effective_caps,
            permitted_caps: CapabilitySet::EMPTY,
        };
        let tgt = target_with(2000, 2000, false);
        assert!(signal_permitted(src, &tgt, Signum::SIGTERM));
    }

    #[test]
    fn root_overrides_euid_mismatch() {
        let src = Cred::root();
        let tgt = target_with(2000, 2000, false);
        assert!(signal_permitted(src, &tgt, Signum::SIGTERM));
    }

    #[test]
    fn sigcont_same_session_passes_regardless_of_uid() {
        let src = cred_with(1000, 1000);
        let tgt = target_with(2000, 2000, true);
        assert!(signal_permitted(src, &tgt, Signum::SIGCONT));
    }

    #[test]
    fn sigcont_different_session_still_requires_cred_match() {
        let src = cred_with(1000, 1000);
        let tgt = target_with(2000, 2000, false);
        assert!(!signal_permitted(src, &tgt, Signum::SIGCONT));
    }

    // ----- integration tests against real Process / PGroup graph -----

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

    fn fresh_aspace() -> tx_substrate::zone::Cap<crate::vm::AddressSpace> {
        crate::vm::AddressSpace::new_cap_for_platform::<crate::vm::TestPmap>().expect("aspace")
    }

    fn fresh_init() -> tx_substrate::zone::Cap<crate::process::ProcessIdentity> {
        crate::process::bootstrap_init_process(fresh_aspace()).expect("init")
    }

    #[test]
    fn script_kill_process_same_uid_delivers() {
        let _g = setup();
        let parent = fresh_init();
        let child = crate::process::step_fork::<crate::vm::TestPmap>(&parent).expect("fork");
        // Both inherit root cred; same euid.

        let outcome = script_kill_process(&parent, &child, Signum::SIGTERM);
        assert_eq!(outcome, Ok(KillScriptOutcome::Delivered));
    }

    #[test]
    fn script_kill_process_different_uid_returns_eperm() {
        let _g = setup();
        let parent = fresh_init();
        let child = crate::process::step_fork::<crate::vm::TestPmap>(&parent).expect("fork");

        // Drop both to non-privileged uids that don't share. We set
        // creds directly because day-1 `step_setuid` retains caps
        // (the cap-drop-on-deprivilege transition machinery is
        // phase-2 per the cred doc); the kill rule itself is
        // independent of how a process arrived at its cred.
        set_cred(&parent, limited_cred(1000));
        set_cred(&child, limited_cred(2000));

        let outcome = script_kill_process(&parent, &child, Signum::SIGTERM);
        assert_eq!(outcome, Err(Errno::EPERM));
    }

    #[test]
    fn script_kill_process_zombie_target_returns_no_live_thread() {
        let _g = setup();
        let parent = fresh_init();
        let child = crate::process::step_fork::<crate::vm::TestPmap>(&parent).expect("fork");
        crate::process::step_exit_group(&child, 0);

        // Even without permission, target_proc_cred returns None for
        // a zombie, short-circuiting before the cred check.
        let outcome = script_kill_process(&parent, &child, Signum::SIGTERM);
        assert_eq!(outcome, Ok(KillScriptOutcome::NoLiveThread));
    }

    #[test]
    fn script_kill_process_zombie_source_returns_esrch() {
        let _g = setup();
        let parent = fresh_init();
        let child = crate::process::step_fork::<crate::vm::TestPmap>(&parent).expect("fork");
        crate::process::step_exit_group(&parent, 0);

        let outcome = script_kill_process(&parent, &child, Signum::SIGTERM);
        assert_eq!(outcome, Err(Errno::ESRCH));
    }

    #[test]
    fn script_kill_pgrp_partial_permission_returns_count_of_permitted() {
        let _g = setup();
        let parent = fresh_init();
        let child_a = crate::process::step_fork::<crate::vm::TestPmap>(&parent).expect("fork a");
        let child_b = crate::process::step_fork::<crate::vm::TestPmap>(&parent).expect("fork b");

        // Set up a partial-permission scenario:
        //   parent (sender):    uid=1000, no caps
        //   parent  (in pgrp):   uid=1000, no caps   — matches sender uid
        //   child_a (in pgrp):   uid=1000, no caps   — matches sender uid
        //   child_b (in pgrp):   uid=2000, no caps   — denied
        // Expect: 2 delivered (parent + child_a).
        set_cred(&parent, limited_cred(1000));
        set_cred(&child_a, limited_cred(1000));
        set_cred(&child_b, limited_cred(2000));

        let pgrp = parent.pgrp_cap();
        let delivered = script_kill_pgrp(&parent, &pgrp, Signum::SIGTERM).expect("not zombie");
        assert_eq!(delivered, 2);

        let pending_on = |proc: &Cap<ProcessIdentity>| {
            let payload = proc.payload.lock();
            payload
                .as_ref()
                .map(|p| p.threads.lock()[0].clone())
                .unwrap()
                .payload
                .lock()
                .as_ref()
                .map(|tp| tp.pending().is_pending(Signum::SIGTERM))
                .unwrap()
        };
        assert!(pending_on(&parent), "parent received the post");
        assert!(pending_on(&child_a), "child_a permitted");
        assert!(!pending_on(&child_b), "child_b denied");
    }

    #[test]
    fn signal_zero_is_permission_probe_no_delivery() {
        let _g = setup();
        let parent = fresh_init();
        let child = crate::process::step_fork::<crate::vm::TestPmap>(&parent).expect("fork");

        // Same uid — probe should succeed without delivery.
        let outcome = script_kill_probe(&parent, &child).expect("probe");
        assert_eq!(outcome, KillScriptOutcome::Probed);

        // Verify nothing was actually posted to child.
        let leader_pending = {
            let payload = child.payload.lock();
            let payload = payload.as_ref().unwrap();
            let leader = payload.threads.lock()[0].clone();
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
}

// ---------------------------------------------------------------------------
// TTY → signal end-to-end (typed dispatch)
// ---------------------------------------------------------------------------

mod tty_bridge {
    use super::*;
    use crate::cred::Uid;
    use crate::device::{CharDeviceBinding, CharDeviceOps, DevT};
    use crate::execution::{Errno, Guard, StepOutcome};
    use crate::process::{bootstrap_init_process, step_fork, ProcessIdentity};
    use crate::signal::{deliver_tty_dispatch, signum_for_job_control, DispatchOutcome};
    use crate::tty::execution::{JobControlSignal, SignalDispatch, SignalTarget};
    use crate::tty::structure::{TtyIdentity, TtyKind, TtyPayload};
    use crate::vm::{AddressSpace, TestPmap};
    use tx_substrate::zone::{self, PayloadCap};

    struct NoopOps;
    impl CharDeviceOps for NoopOps {
        fn read(&self, _out: &mut [u8], _g: &Guard<'_>) -> StepOutcome<usize> {
            StepOutcome::Done(0)
        }
        fn write(&self, b: &[u8], _g: &Guard<'_>) -> StepOutcome<usize> {
            StepOutcome::Done(b.len())
        }
    }
    static NOOP_OPS: NoopOps = NoopOps;
    static NOOP_BINDING: CharDeviceBinding = CharDeviceBinding {
        devt: DevT::new(4, 64),
        name: "tty-bridge-test",
        ops: &NOOP_OPS,
    };

    fn setup() -> std::sync::MutexGuard<'static, ()> {
        let g = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        init_host_for_test_once();
        let _ = zones::register_all();
        let _ = tx_substrate::epoch::drain_with_budget(usize::MAX);
        let _ = tx_substrate::epoch::drain_with_budget(usize::MAX);
        reset_pid_counter_for_test();
        reset_tid_counter_for_test();
        g
    }

    fn fresh_init() -> tx_substrate::zone::Cap<ProcessIdentity> {
        bootstrap_init_process(AddressSpace::new_cap_for_platform::<TestPmap>().expect("aspace"))
            .expect("init")
    }

    fn fresh_tty(name: &str) -> tx_substrate::zone::Cap<TtyIdentity> {
        let id = TtyIdentity::new(TtyKind::SerialHardware, 0, name);
        let res = zone::reserve_for::<TtyIdentity>().expect("identity");
        let cap = zone::sign_for(res, id);
        let payload = TtyPayload::new_hardware(&NOOP_BINDING);
        let pres = zone::reserve_for::<TtyPayload>().expect("payload");
        let pcap = zone::sign_for(pres, payload);
        cap.install_payload(PayloadCap::from_cap(pcap));
        cap
    }

    #[test]
    fn signum_for_job_control_maps_known_signals() {
        assert_eq!(
            signum_for_job_control(JobControlSignal::Int),
            Signum::SIGINT
        );
        assert_eq!(
            signum_for_job_control(JobControlSignal::Quit),
            Signum::SIGQUIT
        );
        assert_eq!(
            signum_for_job_control(JobControlSignal::Tstp),
            Signum::SIGTSTP
        );
        assert_eq!(
            signum_for_job_control(JobControlSignal::Ttin),
            Signum::SIGTTIN
        );
        assert_eq!(
            signum_for_job_control(JobControlSignal::Ttou),
            Signum::SIGTTOU
        );
        assert_eq!(
            signum_for_job_control(JobControlSignal::Hup),
            Signum::SIGHUP
        );
        assert_eq!(
            signum_for_job_control(JobControlSignal::Cont),
            Signum::SIGCONT
        );
        // SIGWINCH = 28 on Linux; we synthesise.
        assert_eq!(signum_for_job_control(JobControlSignal::Winch).raw(), 28);
    }

    #[test]
    fn deliver_tty_dispatch_with_no_typed_pgrp_returns_no_typed_pgrp() {
        let _g = setup();
        let init = fresh_init();
        let dispatch = SignalDispatch {
            target: SignalTarget::ForegroundProcessGroup {
                pgid: 1,
                pgrp: None,
            },
            signal: JobControlSignal::Int,
        };
        let outcome = deliver_tty_dispatch(&init, dispatch).expect("not zombie");
        assert_eq!(outcome, DispatchOutcome::NoTypedPgrp);
    }

    #[test]
    fn typed_tty_vintr_routes_sigint_to_foreground_pgrp() {
        let _g = setup();
        let parent = fresh_init();
        let child = step_fork::<TestPmap>(&parent).expect("fork");

        // Bind the TTY's foreground pgrp typed-style to parent's pgrp
        // (which has both parent and child as members).
        let pgrp = parent.pgrp_cap();
        let session = pgrp.session_cap();
        let tty = fresh_tty("ttyS-vintr");
        tty.bind_session_pgrp_typed(&session, &pgrp);

        // Construct a VINTR-style dispatch the way deferred_signal_for_tty
        // would: pull the foreground pgrp Weak out of the binding.
        let binding = tty.session_pgrp().expect("bound");
        let dispatch = SignalDispatch {
            target: SignalTarget::ForegroundProcessGroup {
                pgid: binding.foreground_pgid,
                pgrp: binding.foreground_pgrp,
            },
            signal: JobControlSignal::Int,
        };

        // parent (the source / "kernel acting on behalf of the
        // tty-emitting subsystem") drives delivery. Both processes are
        // root cred so the permission check passes.
        let outcome = deliver_tty_dispatch(&parent, dispatch).expect("not zombie");
        assert_eq!(outcome, DispatchOutcome::Delivered { count: 2 });

        // Both members observed the SIGINT post on their leader's
        // pending queue.
        for proc_cap in [&parent, &child] {
            let payload = proc_cap.payload.lock();
            let leader = payload.as_ref().unwrap().threads.lock()[0].clone();
            let leader_payload = leader.payload.lock();
            assert!(leader_payload
                .as_ref()
                .unwrap()
                .pending()
                .is_pending(Signum::SIGINT));
        }
    }

    #[test]
    fn deliver_tty_dispatch_skips_members_when_source_lacks_permission() {
        let _g = setup();
        let parent = fresh_init();
        let child = step_fork::<TestPmap>(&parent).expect("fork");

        // Make parent unprivileged and at uid=1000; child stays root.
        // parent attempts SIGINT to its own pgrp via the typed dispatch;
        // parent is permitted to itself (uid match) but denied to
        // child (1000 vs 0).
        {
            let p = parent.payload.lock();
            *p.as_ref().unwrap().cred.lock() = crate::cred::Cred {
                uid: Uid(1000),
                euid: Uid(1000),
                gid: crate::cred::Gid(0),
                egid: crate::cred::Gid(0),
                effective_caps: crate::cred::CapabilitySet::EMPTY,
                permitted_caps: crate::cred::CapabilitySet::EMPTY,
            };
        }

        let pgrp = parent.pgrp_cap();
        let session = pgrp.session_cap();
        let tty = fresh_tty("ttyS-deny");
        tty.bind_session_pgrp_typed(&session, &pgrp);

        let binding = tty.session_pgrp().expect("bound");
        let dispatch = SignalDispatch {
            target: SignalTarget::ForegroundProcessGroup {
                pgid: binding.foreground_pgid,
                pgrp: binding.foreground_pgrp,
            },
            signal: JobControlSignal::Int,
        };

        let outcome = deliver_tty_dispatch(&parent, dispatch).expect("not zombie");
        assert_eq!(
            outcome,
            DispatchOutcome::Delivered { count: 1 },
            "only parent is uid-permitted to itself; child denied"
        );

        let parent_pending = {
            let p = parent.payload.lock();
            let leader = p.as_ref().unwrap().threads.lock()[0].clone();
            let lp = leader.payload.lock();
            lp.as_ref().unwrap().pending().is_pending(Signum::SIGINT)
        };
        let child_pending = {
            let p = child.payload.lock();
            let leader = p.as_ref().unwrap().threads.lock()[0].clone();
            let lp = leader.payload.lock();
            lp.as_ref().unwrap().pending().is_pending(Signum::SIGINT)
        };
        assert!(parent_pending);
        assert!(!child_pending);
    }

    #[test]
    fn deliver_tty_dispatch_zombie_source_returns_esrch() {
        let _g = setup();
        let parent = fresh_init();

        let pgrp = parent.pgrp_cap();
        let session = pgrp.session_cap();
        let tty = fresh_tty("ttyS-zombie-source");
        tty.bind_session_pgrp_typed(&session, &pgrp);

        let binding = tty.session_pgrp().expect("bound");
        let dispatch = SignalDispatch {
            target: SignalTarget::ForegroundProcessGroup {
                pgid: binding.foreground_pgid,
                pgrp: binding.foreground_pgrp,
            },
            signal: JobControlSignal::Cont,
        };

        crate::process::step_exit_group(&parent, 0);

        assert_eq!(deliver_tty_dispatch(&parent, dispatch), Err(Errno::ESRCH));
    }
}
