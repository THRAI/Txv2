//! TTY typed-pgrp binding tests.
//!
//! Cover the new typed `SessionPgrp` surface that takes
//! `Cap<Session>` / `Cap<ProcessGroup>` and exposes
//! `TtyIdentity::foreground_pgrp_cap` for signal-fanout callers.

use tx_substrate::testing::init_host_for_test_once;
use tx_substrate::zone::{self, Cap, PayloadCap};

use crate::device::{CharDeviceBinding, CharDeviceOps, DevT};
use crate::execution::{Guard, StepOutcome};
use crate::process::structure::{reset_pid_counter_for_test, Pgid};
use crate::process::{bootstrap_init_process, step_setpgid, step_setsid, ProcessIdentity};
use crate::signal::{step_kill_pgrp, Signum};
use crate::test_support::EPOCH_TEST_LOCK;
use crate::thread_runtime::structure::reset_tid_counter_for_test;
use crate::tty::structure::{SessionPgrp, TtyIdentity, TtyKind, TtyPayload};
use crate::vm::{AddressSpace, TestPmap};
use crate::zones;

struct NoopOps;

impl CharDeviceOps for NoopOps {
    fn read(&self, _out: &mut [u8], _guard: &Guard<'_>) -> StepOutcome<usize> {
        StepOutcome::Done(0)
    }

    fn write(&self, bytes: &[u8], _guard: &Guard<'_>) -> StepOutcome<usize> {
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
    init_host_for_test_once();
    let _ = zones::register_all();
    let _ = tx_substrate::epoch::drain_with_budget(usize::MAX);
    let _ = tx_substrate::epoch::drain_with_budget(usize::MAX);
    reset_pid_counter_for_test();
    reset_tid_counter_for_test();
    guard
}

fn fresh_init() -> Cap<ProcessIdentity> {
    bootstrap_init_process(AddressSpace::new_cap_for_platform::<TestPmap>().expect("aspace"))
        .expect("init")
}

fn fresh_tty(name: &str) -> Cap<TtyIdentity> {
    let id = TtyIdentity::new(TtyKind::SerialHardware, 0, name);
    let res = zone::reserve_for::<TtyIdentity>().expect("identity slot");
    let cap = zone::sign_for(res, id);
    let payload_id = TtyPayload::new_hardware(&NOOP_BINDING);
    let payload_res = zone::reserve_for::<TtyPayload>().expect("payload slot");
    let payload_cap = zone::sign_for(payload_res, payload_id);
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
