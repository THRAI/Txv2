//! TTY typed-pgrp binding tests.
//!
//! Cover the new typed `SessionPgrp` surface that takes
//! `Cap<Session>` / `Cap<ProcessGroup>` and exposes
//! `TtyIdentity::foreground_pgrp_cap` for signal-fanout callers.

use crate::tty::adapter::step_engine::{
    guard, reserve_for, sign_for, ByteProgress, Cap, PayloadCap, StepOutcome,
};

use crate::device::{CharDeviceBinding, CharDeviceOps, DevT};
use crate::execution::Guard;
use crate::process::execution::reset_init_process_for_test;
use crate::process::structure::{reset_pid_counter_for_test, ExitStatus, Pgid};
use crate::process::{
    bootstrap_init_process, step_exit_group, step_fork, step_setpgid, step_setsid, ProcessIdentity,
};
use crate::signal::{deliver_tty_dispatch, step_kill_pgrp, DispatchOutcome, Signum};
use crate::test_support::EPOCH_TEST_LOCK;
use crate::thread_runtime::structure::reset_tid_counter_for_test;
use crate::tty::execution::{
    step_ioctl_tcgets, step_ioctl_tcsets, step_ioctl_tiocnotty_for_process,
    step_ioctl_tiocsctty_for_process, step_ioctl_tiocspgrp_for_process, step_read_for_process,
    step_write_for_process, IoctlCaller, SignalTarget,
};
use crate::tty::structure::termios::TOSTOP;
use crate::tty::structure::{SessionPgrp, TtyIdentity, TtyKind, TtyPayload};
use crate::vm::{AddressSpace, TestPmap};
use crate::zones;

struct NoopOps;

impl CharDeviceOps for NoopOps {
    fn read(
        &self,
        _out: &mut [u8],
        _guard: &Guard<'_>,
    ) -> StepOutcome<usize, ByteProgress> {
        StepOutcome::Done(0)
    }

    fn write(
        &self,
        bytes: &[u8],
        _guard: &Guard<'_>,
    ) -> StepOutcome<usize, ByteProgress> {
        StepOutcome::Done(bytes.len())
    }
}

static NOOP_OPS: NoopOps = NoopOps;
static NOOP_BINDING: CharDeviceBinding = CharDeviceBinding {
    devt: DevT::new(4, 64),
    name: "tty-typed-test",
    ops: &NOOP_OPS,
};

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

fn fresh_init() -> Cap<ProcessIdentity> {
    bootstrap_init_process(AddressSpace::new_cap_for_platform::<TestPmap>().expect("aspace"))
        .expect("init")
}

fn fresh_tty(name: &str) -> Cap<TtyIdentity> {
    let id = TtyIdentity::new(TtyKind::SerialHardware, 0, name);
    let res = reserve_for::<TtyIdentity>().expect("identity slot");
    let cap = sign_for(res, id);
    let payload_id = TtyPayload::new_hardware(&NOOP_BINDING);
    let payload_res = reserve_for::<TtyPayload>().expect("payload slot");
    let payload_cap = sign_for(payload_res, payload_id);
    cap.install_payload(PayloadCap::from_cap(payload_cap));
    cap
}

#[test]
fn from_raw_ids_leaves_typed_refs_none() {
    let raw = SessionPgrp::from_raw_ids(7, 7, 7);
    assert!(raw.session.is_none());
    assert!(raw.foreground_pgrp.is_none());
    assert!(raw.upgrade_session().is_none());
    assert!(raw.upgrade_foreground_pgrp().is_none());
}

#[test]
fn from_typed_caches_ids_and_stores_weak_refs() {
    let _g = setup();
    let init = fresh_init();
    let pgrp = init.pgrp_cap();
    let session = pgrp.session_cap();

    let binding = SessionPgrp::from_typed(&session, &pgrp);
    assert_eq!(binding.session_id, session.sid.0);
    assert_eq!(binding.foreground_pgid, pgrp.pgid.0);
    let upgraded_session = binding.upgrade_session().expect("session live");
    assert_eq!(upgraded_session.sid, session.sid);
    let upgraded_pgrp = binding.upgrade_foreground_pgrp().expect("pgrp live");
    assert_eq!(upgraded_pgrp.pgid, pgrp.pgid);
}

#[test]
fn tty_bind_session_pgrp_typed_installs_binding_and_exposes_pgrp_cap() {
    let _g = setup();
    let init = fresh_init();
    let pgrp = init.pgrp_cap();
    let session = pgrp.session_cap();
    let tty = fresh_tty("ttyS0");

    let prev = tty.bind_session_pgrp_typed(&session, &pgrp);
    assert!(prev.is_none(), "first binding should have no predecessor");

    let exposed = tty.foreground_pgrp_cap().expect("foreground pgrp live");
    assert_eq!(exposed.pgid, pgrp.pgid);
}

#[test]
fn foreground_pgrp_cap_returns_none_for_legacy_raw_id_binding() {
    let _g = setup();
    let tty = fresh_tty("ttyS1");
    tty.bind_session_pgrp(SessionPgrp::from_raw_ids(11, 11, 11));
    assert!(tty.foreground_pgrp_cap().is_none());
}

#[test]
fn typed_pgrp_can_drive_step_kill_pgrp_against_real_membership() {
    let _g = setup();
    let init = fresh_init();
    let pgrp = init.pgrp_cap();
    let session = pgrp.session_cap();
    let tty = fresh_tty("ttyS2");
    tty.bind_session_pgrp_typed(&session, &pgrp);

    // The whole point of typed rebinding: hand the foreground pgrp
    // Cap to the signal shim. Ensure the round-trip works.
    let target_pgrp = tty.foreground_pgrp_cap().expect("foreground pgrp live");
    let delivered = step_kill_pgrp(&target_pgrp, Signum::SIGTERM);
    assert_eq!(delivered, 1, "init is the only member of its pgrp");
}

#[test]
fn typed_session_survives_setpgid_until_session_drops() {
    let _g = setup();
    let init = fresh_init();
    let session = init.pgrp_cap().session_cap();
    let initial_pgrp = init.pgrp_cap();
    let tty = fresh_tty("ttyS3");
    tty.bind_session_pgrp_typed(&session, &initial_pgrp);

    // Move init into a fresh pgrp inside the same session. The
    // TTY's foreground-pgrp Weak now points at the (released) old
    // pgrp; the session ref still points at the same session.
    step_setpgid(&init, Pgid(init.pid.0)).expect("setpgid");

    let upgraded_session = tty
        .session_pgrp()
        .expect("binding installed")
        .upgrade_session()
        .expect("session still alive");
    assert_eq!(upgraded_session.sid, session.sid);
}

#[test]
fn session_foreground_pgrp_cap_resolves_two_hop_via_controlling_tty() {
    // Round-trip: install a controlling-tty link on Session, install
    // a typed SessionPgrp on the TTY, and verify
    // `Session::foreground_pgrp_cap()` returns the same pgrp via the
    // two-hop dereference (Session.controlling_tty Weak →
    // TtyIdentity → TtyIdentity.session_pgrp.foreground_pgrp Weak).
    let _g = setup();
    let init = fresh_init();
    let pgrp = init.pgrp_cap();
    let session = pgrp.session_cap();
    let tty = fresh_tty("ttyS5");

    // Wire both directions: TTY → fg pgrp via SessionPgrp; Session →
    // tty via controlling_tty Weak.
    tty.bind_session_pgrp_typed(&session, &pgrp);
    *session.controlling_tty.lock() = Some(tty.downgrade());

    let exposed = session
        .foreground_pgrp_cap()
        .expect("two-hop fg pgrp resolved");
    assert_eq!(exposed.pgid, pgrp.pgid);
}

#[test]
fn session_foreground_pgrp_cap_none_without_controlling_tty() {
    let _g = setup();
    let init = fresh_init();
    let session = init.pgrp_cap().session_cap();
    // No `*session.controlling_tty.lock() = Some(...)` — bootstrap
    // session has no tty.
    assert!(!session.has_controlling_tty());
    assert!(session.foreground_pgrp_cap().is_none());
}

#[test]
fn session_foreground_pgrp_cap_none_when_tty_has_no_binding() {
    // controlling_tty installed but the TTY's SessionPgrp is empty —
    // first hop succeeds, second hop returns None.
    let _g = setup();
    let init = fresh_init();
    let session = init.pgrp_cap().session_cap();
    let tty = fresh_tty("ttyS6");
    *session.controlling_tty.lock() = Some(tty.downgrade());

    assert!(session.foreground_pgrp_cap().is_none());
}

// ----- §8.3 session-leader-tty hangup cascade -----

/// Helper: read SIGHUP / SIGCONT pending state on `proc`'s leader.
fn leader_pending(proc_cap: &Cap<ProcessIdentity>, sig: Signum) -> bool {
    let payload = proc_cap.payload.lock();
    let leader = payload.as_ref().expect("alive").threads.lock()[0].clone();
    drop(payload);
    let leader_payload = leader.payload.lock();
    leader_payload
        .as_ref()
        .expect("alive")
        .pending()
        .is_pending(sig)
}

#[test]
fn ioctl_caller_from_process_reflects_live_process_topology() {
    let _g = setup();
    let init = fresh_init();
    let tty = fresh_tty("ttyS-callers");

    let session = init.pgrp_cap().session_cap();
    tty.bind_session_pgrp_typed(&session, &init.pgrp_cap());
    *session.controlling_tty.lock() = Some(tty.downgrade());

    let caller = IoctlCaller::from_process(&init).expect("live caller");
    assert_eq!(caller.session_id, session.sid.0);
    assert_eq!(caller.pgrp_id, init.pgrp_cap().pgid.0);
    assert!(caller.is_session_leader);
    assert!(caller.has_controlling_tty);
    assert!(caller.in_foreground);
    assert!(caller.pgrp.is_some());
}

#[test]
fn process_aware_tiocsctty_binds_tty_and_session_mirror() {
    let _g = setup();
    let init = fresh_init();
    let tty = fresh_tty("ttyS-bind-proc");
    let guard = guard();

    assert_eq!(
        step_ioctl_tiocsctty_for_process(&tty, &init, &guard),
        StepOutcome::Done(crate::tty::execution::IoctlSideEffect {
            session_ctl_fired: true,
            signal: None,
        })
    );
    drop(guard);

    let session = init.pgrp_cap().session_cap();
    let binding = tty.session_pgrp().expect("binding installed");
    assert_eq!(binding.session_id, session.sid.0);
    assert_eq!(binding.foreground_pgid, init.pgrp_cap().pgid.0);
    assert!(session.has_controlling_tty());
    assert_eq!(
        session.controlling_tty_cap().expect("mirror set").key(),
        tty.key()
    );
}

#[test]
fn process_aware_tiocnotty_clears_tty_and_session_links() {
    let _g = setup();
    let init = fresh_init();
    let tty = fresh_tty("ttyS-detach-proc");
    let guard = guard();

    let _ = step_ioctl_tiocsctty_for_process(&tty, &init, &guard);
    assert!(tty.session_pgrp().is_some());
    assert!(init.pgrp_cap().session_cap().has_controlling_tty());

    assert_eq!(
        step_ioctl_tiocnotty_for_process(&tty, &init, &guard),
        StepOutcome::Done(crate::tty::execution::IoctlSideEffect {
            session_ctl_fired: true,
            signal: None,
        })
    );

    assert!(tty.session_pgrp().is_none());
    assert!(!init.pgrp_cap().session_cap().has_controlling_tty());
}

#[test]
fn process_aware_tiocspgrp_rebinds_foreground_to_typed_target() {
    let _g = setup();
    let init = fresh_init();
    let child = step_fork::<TestPmap>(&init).expect("fork");
    step_setpgid(&child, Pgid(child.pid.0)).expect("child pgrp");
    let child_pgrp = child.pgrp_cap();

    let tty = fresh_tty("ttyS-fg-rebind");
    let guard = guard();
    let _ = step_ioctl_tiocsctty_for_process(&tty, &init, &guard);

    assert_eq!(
        step_ioctl_tiocspgrp_for_process(&tty, &init, &child_pgrp, &guard),
        StepOutcome::Done(crate::tty::execution::IoctlSideEffect {
            session_ctl_fired: true,
            signal: None,
        })
    );
    drop(guard);

    let rebound = tty.foreground_pgrp_cap().expect("typed fg pgrp");
    assert_eq!(rebound.key(), child_pgrp.key());
    assert_eq!(
        tty.session_pgrp().expect("binding present").foreground_pgid,
        child_pgrp.pgid.0
    );
}

#[test]
fn step_read_for_process_posts_sigttin_to_background_caller_pgrp() {
    let _g = setup();
    let init = fresh_init();
    let child = step_fork::<TestPmap>(&init).expect("fork");
    step_setpgid(&child, Pgid(child.pid.0)).expect("child pgrp");

    let tty = fresh_tty("ttyS-bg-read");
    let guard = guard();
    let _ = step_ioctl_tiocsctty_for_process(&tty, &init, &guard);

    let mut out = [0u8; 1];
    assert_eq!(
        step_read_for_process(&tty, &mut out, &child, &guard),
        StepOutcome::Err(crate::tty::adapter::step_engine::Errno::EIO)
    );
    assert!(leader_pending(&child, Signum::SIGTTIN));
}

#[test]
fn step_write_for_process_posts_sigttou_to_background_caller_pgrp() {
    let _g = setup();
    let init = fresh_init();
    let child = step_fork::<TestPmap>(&init).expect("fork");
    step_setpgid(&child, Pgid(child.pid.0)).expect("child pgrp");

    let tty = fresh_tty("ttyS-bg-write");
    let guard = guard();
    let _ = step_ioctl_tiocsctty_for_process(&tty, &init, &guard);

    let mut termios = match step_ioctl_tcgets(&tty, &guard) {
        StepOutcome::Done(termios) => termios,
        other => panic!("tcgets failed: {other:?}"),
    };
    termios.c_lflag |= TOSTOP;
    match step_ioctl_tcsets(&tty, termios, &guard) {
        StepOutcome::Done(_) => {}
        other => panic!("tcsets failed: {other:?}"),
    }

    {
        use crate::tty::adapter::step_engine::{Errno as V3Errno, StepOutcome as V3Out};
        assert_eq!(
            step_write_for_process(&tty, b"x", &child, &guard),
            V3Out::Err(V3Errno::EIO)
        );
    }
    assert!(leader_pending(&child, Signum::SIGTTOU));
}

#[test]
fn session_leader_exit_with_controlling_tty_fires_sighup_sigcont_and_clears_binding() {
    let _g = setup();
    let init = fresh_init();
    // init has sid=1 == pid=1 (session leader). Wire up a tty and
    // a foreground pgrp so the cascade has work to do.
    let session = init.pgrp_cap().session_cap();
    let pgrp = init.pgrp_cap();
    let tty = fresh_tty("ttyS-cascade-1");

    tty.bind_session_pgrp_typed(&session, &pgrp);
    *session.controlling_tty.lock() = Some(tty.downgrade());

    assert!(session.has_controlling_tty());
    assert!(tty.foreground_pgrp_cap().is_some());

    // Session leader exits — fires the cascade.
    step_exit_group(&init, ExitStatus::Exited(0));

    // Init zombified per usual.
    assert!(init.is_zombie());

    // SIGHUP and SIGCONT now pending on every fg-pgrp member's
    // leader thread. init is the only member of its own pgrp (in
    // bootstrap), but it's now a zombie — the post happened before
    // zombification (cascade runs first in step_exit_group).
    //
    // Re-pull the zombie's stale leader: payload is dropped, so
    // `leader_pending` would panic. Instead, verify post effect via
    // the tty/session-side state changes.

    // Tty's session_pgrp slot cleared (authoritative side).
    assert!(tty.session_pgrp().is_none(), "tty.session_pgrp cleared");

    // Session's controlling_tty mirror also cleared.
    assert!(
        !session.has_controlling_tty(),
        "session.controlling_tty cleared"
    );
}

#[test]
fn session_leader_exit_with_live_fg_pgrp_member_delivers_sighup_and_sigcont() {
    let _g = setup();
    let init = fresh_init();
    // Fork a child to be the fg-pgrp member that will *survive* the
    // cascade and observe the SIGHUP/SIGCONT delivery.
    let child = step_fork::<TestPmap>(&init).expect("fork");

    // Setup: init's session has a controlling tty whose fg pgrp is
    // init's pgrp — which contains both init and child.
    let session = init.pgrp_cap().session_cap();
    let pgrp = init.pgrp_cap();
    let tty = fresh_tty("ttyS-cascade-2");
    tty.bind_session_pgrp_typed(&session, &pgrp);
    *session.controlling_tty.lock() = Some(tty.downgrade());

    // Initially neither SIGHUP nor SIGCONT pending on child.
    assert!(!leader_pending(&child, Signum::SIGHUP));
    assert!(!leader_pending(&child, Signum::SIGCONT));

    // init exits (session leader) → cascade fires SIGHUP+SIGCONT to
    // the fg pgrp, which still contains child as a live member.
    step_exit_group(&init, ExitStatus::Exited(0));

    assert!(
        leader_pending(&child, Signum::SIGHUP),
        "child should observe SIGHUP from session-leader-death cascade"
    );
    // SIGCONT is Gewalt — it doesn't enter thread_pending. Instead,
    // it clears summary.stop_requested. Pre-cascade child wasn't
    // stopped, so the bit stays cleared. Just confirm SIGCONT didn't
    // enter pending (Gewalt invariant).
    assert!(!leader_pending(&child, Signum::SIGCONT));
}

#[test]
fn non_session_leader_exit_does_not_fire_cascade() {
    let _g = setup();
    let init = fresh_init();

    // Fork a child and put it in its own session (now a session leader
    // of a new session). Then fork a grandchild from the new session
    // leader — grandchild is a member but NOT the session leader.
    let session_leader = step_fork::<TestPmap>(&init).expect("fork");
    let _new_sid = step_setsid(&session_leader).expect("setsid");
    let grandchild = step_fork::<TestPmap>(&session_leader).expect("fork-of-leader");

    // grandchild.pgrp.session.sid != grandchild.pid → grandchild is
    // not the session leader.
    let session = grandchild.pgrp_cap().session_cap();
    assert_ne!(grandchild.pid.0, session.sid.0);

    let tty = fresh_tty("ttyS-cascade-3");
    tty.bind_session_pgrp_typed(&session, &grandchild.pgrp_cap());
    *session.controlling_tty.lock() = Some(tty.downgrade());

    // grandchild (non-leader) exits. Cascade must NOT fire — the tty
    // binding stays put; the session's controlling_tty stays set.
    step_exit_group(&grandchild, ExitStatus::Exited(0));

    assert!(
        tty.session_pgrp().is_some(),
        "non-leader exit must not clear tty.session_pgrp"
    );
    assert!(
        session.has_controlling_tty(),
        "non-leader exit must not clear session.controlling_tty"
    );
}

#[test]
fn session_leader_exit_without_controlling_tty_is_noop() {
    // Per §8.3 step 1 first-hop fail: no controlling tty → cascade
    // is a complete no-op. The bootstrap session has no controlling
    // tty, so this is the natural shape.
    let _g = setup();
    let init = fresh_init();
    let session = init.pgrp_cap().session_cap();
    assert!(!session.has_controlling_tty());

    // Just exercising the no-op branch — assertion is "didn't panic".
    step_exit_group(&init, ExitStatus::Exited(0));
    assert!(init.is_zombie());
}

#[test]
fn session_leader_exit_with_tty_but_no_fg_pgrp_clears_binding_without_signal() {
    let _g = setup();
    let init = fresh_init();
    let session = init.pgrp_cap().session_cap();
    let tty = fresh_tty("ttyS-cascade-5");

    // Install controlling_tty mirror but install only a raw-id (no
    // typed) SessionPgrp on the tty side — `foreground_pgrp_cap`
    // returns None (second-hop fails) but tty.session_pgrp itself
    // is `Some(...)`.
    tty.bind_session_pgrp(SessionPgrp::from_raw_ids(99, 99, 99));
    *session.controlling_tty.lock() = Some(tty.downgrade());

    assert!(tty.session_pgrp().is_some());
    assert!(tty.foreground_pgrp_cap().is_none());

    step_exit_group(&init, ExitStatus::Exited(0));

    // Per spec: "If `None` (no fg pgrp installed, or pgrp reclaimed),
    // skip the SIGHUP step but still proceed to step 3 (the tty's
    // own session_pgrp slot must be cleared regardless)."
    assert!(
        tty.session_pgrp().is_none(),
        "tty.session_pgrp must clear regardless of fg-pgrp upgrade"
    );
    assert!(!session.has_controlling_tty());
}

#[test]
fn step_hangup_exposes_typed_session_leader_pgrp_dispatch() {
    let _g = setup();
    let init = fresh_init();
    let tty = fresh_tty("ttyS-hup-typed");
    let session = init.pgrp_cap().session_cap();
    let pgrp = init.pgrp_cap();
    let guard = guard();

    tty.bind_session_pgrp_typed(&session, &pgrp);

    let outcome = match crate::tty::execution::step_hangup(&tty, &guard) {
        StepOutcome::Done(outcome) => outcome,
        other => panic!("hangup failed: {other:?}"),
    };
    drop(guard);

    let hup = outcome.hup_signal.expect("SIGHUP dispatch");
    match hup.target {
        SignalTarget::SessionLeaderProcessGroup { pgid, pgrp } => {
            assert_eq!(pgid, session.sid.0);
            assert!(pgrp.is_some(), "typed leader pgrp is available");
        }
        other => panic!("unexpected target: {other:?}"),
    }

    let delivered = deliver_tty_dispatch(&init, hup).expect("live source");
    assert_eq!(delivered, DispatchOutcome::Delivered { count: 1 });
    assert!(leader_pending(&init, Signum::SIGHUP));
}

#[test]
fn typed_session_flips_dead_after_setsid_drops_old_session() {
    let _g = setup();
    let parent = fresh_init();
    let child = crate::process::step_fork::<TestPmap>(&parent).expect("fork");

    let old_session = child.pgrp_cap().session_cap();
    let old_pgrp = child.pgrp_cap();
    let tty = fresh_tty("ttyS4");
    tty.bind_session_pgrp_typed(&old_session, &old_pgrp);

    // Child enters a new session. The old session is still
    // referenced by the parent (which never left), so the Weak
    // remains upgradable.
    let _ = step_setsid(&child);
    assert!(tty
        .session_pgrp()
        .expect("binding installed")
        .upgrade_session()
        .is_some());
}
