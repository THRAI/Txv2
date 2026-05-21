// Auto-extracted from `crates/tx-subsystems/src/signal/tests.rs` (2026-05-08 jumbo split).
#![cfg_attr(test, allow(unused_imports))]
use super::*;
use crate::process::{ProcessIdentity, bootstrap_init_process};
use crate::signal::adapter::step_engine::Cap;
use crate::signal::{SigDisposition, step_sigaction};
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

#[test]
fn step_reset_for_exec_sets_all_dispositions_to_sig_dfl() {
    let _g = setup();
    let proc_cap = fresh_init();

    // Install user handlers on a representative spread (catchable
    // standard signals; SIGKILL/SIGSTOP can't carry a handler).
    let _ = step_sigaction(
        &proc_cap,
        Signum::SIGINT,
        SigDisposition::handler(0xdead_beef),
    );
    let _ = step_sigaction(
        &proc_cap,
        Signum::SIGTERM,
        SigDisposition::handler(0xcafe_d00d),
    );
    let _ = step_sigaction(&proc_cap, Signum::SIGHUP, SigDisposition::handler(0xfeed));

    // Sanity: handlers are installed before reset.
    assert!(matches!(
        proc_cap.sig_disposition(Signum::SIGINT),
        Some(SigDisposition::Handler { .. })
    ));

    // Reset.
    let payload_guard = proc_cap.payload.lock();
    let payload = payload_guard.as_ref().expect("alive");
    payload.sig_actions().step_reset_for_exec();
    drop(payload_guard);

    // Every previously-handled slot is now Default.
    assert_eq!(
        proc_cap.sig_disposition(Signum::SIGINT),
        Some(SigDisposition::Default)
    );
    assert_eq!(
        proc_cap.sig_disposition(Signum::SIGTERM),
        Some(SigDisposition::Default)
    );
    assert_eq!(
        proc_cap.sig_disposition(Signum::SIGHUP),
        Some(SigDisposition::Default)
    );
}

#[test]
fn step_reset_for_exec_preserves_sig_ign() {
    let _g = setup();
    let proc_cap = fresh_init();

    // Install Ignore on SIGPIPE — POSIX says exec preserves
    // SIG_IGN so children of a process that ignored SIGPIPE keep
    // ignoring it across exec.
    let _ = step_sigaction(&proc_cap, Signum::SIGPIPE, SigDisposition::Ignore);

    // Also install a handler on SIGINT so we can confirm the
    // reset only touches handlers.
    let _ = step_sigaction(&proc_cap, Signum::SIGINT, SigDisposition::handler(0x1234));

    let payload_guard = proc_cap.payload.lock();
    let payload = payload_guard.as_ref().expect("alive");
    payload.sig_actions().step_reset_for_exec();
    drop(payload_guard);

    // SIGPIPE stays Ignore; SIGINT becomes Default.
    assert_eq!(
        proc_cap.sig_disposition(Signum::SIGPIPE),
        Some(SigDisposition::Ignore)
    );
    assert_eq!(
        proc_cap.sig_disposition(Signum::SIGINT),
        Some(SigDisposition::Default)
    );
}

#[test]
fn step_reset_for_exec_preserves_pending_signals() {
    let _g = setup();
    let proc_cap = fresh_init();

    // Pre-seed group_pending with SIGTERM via the Gewalt-free
    // `step_kill_process` path (catchable signals route through
    // post_signal onto the leader thread's pending; group_pending
    // we set directly via the per-payload accessor for clarity).
    {
        let payload = proc_cap.payload.lock();
        payload
            .as_ref()
            .expect("alive")
            .group_pending()
            .post(Signum::SIGTERM);
    }
    assert!(
        proc_cap
            .payload
            .lock()
            .as_ref()
            .unwrap()
            .group_pending()
            .is_pending(Signum::SIGTERM)
    );

    // Install a handler on SIGTERM and reset.
    let _ = step_sigaction(
        &proc_cap,
        Signum::SIGTERM,
        SigDisposition::handler(0xdeadbeef),
    );
    let payload_guard = proc_cap.payload.lock();
    let payload = payload_guard.as_ref().expect("alive");
    payload.sig_actions().step_reset_for_exec();
    drop(payload_guard);

    // Pending bit must survive the reset (POSIX: pending signals
    // are NOT cleared on exec).
    assert!(
        proc_cap
            .payload
            .lock()
            .as_ref()
            .unwrap()
            .group_pending()
            .is_pending(Signum::SIGTERM),
        "exec must preserve pending signals"
    );
    // Handler reverted to default.
    assert_eq!(
        proc_cap.sig_disposition(Signum::SIGTERM),
        Some(SigDisposition::Default)
    );
}
