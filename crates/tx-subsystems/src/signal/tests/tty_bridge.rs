// Auto-extracted from `crates/tx-subsystems/src/signal/tests.rs` (2026-05-08 jumbo split).
#![cfg_attr(test, allow(unused_imports))]
use super::*;
use crate::cred::Uid;
use crate::device::{CharDeviceBinding, CharDeviceOps, DevT};
use crate::execution::{Errno, Guard};
use crate::process::{bootstrap_init_process, step_fork, ProcessIdentity};
use crate::signal::{deliver_tty_dispatch, signum_for_job_control, DispatchOutcome};
use crate::tty::execution::{JobControlSignal, SignalDispatch, SignalTarget};
use crate::tty::structure::{TtyIdentity, TtyKind, TtyPayload};
use crate::vm::{AddressSpace, TestPmap};
use tx_substrate::zone::{self, PayloadCap};

struct NoopOps;
impl CharDeviceOps for NoopOps {
    fn read(
        &self,
        _out: &mut [u8],
        _g: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<usize, tx_substrate::step_v3::ByteProgress> {
        tx_substrate::step_v3::StepOutcome::Done(0)
    }
    fn write(
        &self,
        b: &[u8],
        _g: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<usize, tx_substrate::step_v3::ByteProgress> {
        tx_substrate::step_v3::StepOutcome::Done(b.len())
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
    reset_init_process_for_test();
    g
}

fn fresh_init() -> Cap<ProcessIdentity> {
    bootstrap_init_process(AddressSpace::new_cap_for_platform::<TestPmap>().expect("aspace"))
        .expect("init")
}

fn fresh_tty(name: &str) -> Cap<TtyIdentity> {
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
        // PR-9 phase 5 (D5 Path A): `cred` lives in `AtomicSlot<Cap<Cred>>`.
        let p = parent.payload.lock();
        let payload = p.as_ref().unwrap();
        let new = crate::cred::Cred {
            uid: Uid(1000),
            euid: Uid(1000),
            suid: Uid(1000),
            gid: crate::cred::Gid(0),
            egid: crate::cred::Gid(0),
            sgid: crate::cred::Gid(0),
            effective_caps: crate::cred::CapabilitySet::EMPTY,
            permitted_caps: crate::cred::CapabilitySet::EMPTY,
        };
        let new_cap = crate::cred::sign_cred(new).expect("zone slab has capacity in tests");
        let _old = payload.replace_cred(new_cap);
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

    crate::process::step_exit_group(&parent, ExitStatus::Exited(0));

    assert_eq!(deliver_tty_dispatch(&parent, dispatch), Err(Errno::ESRCH));
}
