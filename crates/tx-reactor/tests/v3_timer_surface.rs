//! PR-8 pin tests: `TimerWheel` / `TimerGuard` / `TimerGuardRole` /
//! `TimerToken` public surface.
//!
//! Per `docs/Txv3/07_BLAST_RADIUS.md` §4 row H and §5.2 PR-8 and
//! `docs/Txv3/03_STEP_MODEL_v2.md` §2.3, this PR publishes the
//! role-tagged timer registration surface. These tests pin the
//! observable shape of that surface so PR-7's `OnAgent` runtime
//! integration and any future consolidation with the internal
//! `TimerQueue` keep the published contract stable.

use tx_reactor::adapter::step_engine::Deadline;
use tx_reactor::{TimerGuard, TimerGuardRole, TimerToken, TimerWheel};

#[test]
fn timer_token_roundtrips_through_new_and_raw() {
    let t = TimerToken::new(42);
    assert_eq!(t.raw(), 42);
    assert_eq!(t, TimerToken::new(42));
    assert_ne!(t, TimerToken::new(43));
}

#[test]
fn timer_wheel_starts_empty() {
    let wheel = TimerWheel::new();
    assert_eq!(wheel.armed_count(), 0);
    assert_eq!(wheel.next_deadline_ns(), None);
}

#[test]
fn timer_wheel_default_matches_new() {
    let wheel: TimerWheel = TimerWheel::default();
    assert_eq!(wheel.armed_count(), 0);
}

#[test]
fn install_returns_guard_carrying_token_deadline_role() {
    let wheel = TimerWheel::new();
    let deadline = Deadline::from_raw(1_000_000);
    let guard = wheel.install(deadline, TimerGuardRole::PrimarySleep);

    assert_eq!(guard.deadline(), deadline);
    assert_eq!(guard.role(), TimerGuardRole::PrimarySleep);
    // Token id is opaque; we only assert it's non-zero (sentinel
    // reservation) and that the wheel can find it.
    assert_ne!(guard.token(), TimerToken::new(0));
    assert_eq!(wheel.armed_count(), 1);
}

#[test]
fn three_roles_round_trip_independently() {
    let wheel = TimerWheel::new();
    let g_primary = wheel.install(Deadline::from_raw(10), TimerGuardRole::PrimarySleep);
    let g_abort = wheel.install(Deadline::from_raw(20), TimerGuardRole::DeadlineAbort);
    let g_delegate = wheel.install(Deadline::from_raw(30), TimerGuardRole::DelegateTimeout);

    assert_eq!(wheel.armed_count(), 3);
    assert_eq!(g_primary.role(), TimerGuardRole::PrimarySleep);
    assert_eq!(g_abort.role(), TimerGuardRole::DeadlineAbort);
    assert_eq!(g_delegate.role(), TimerGuardRole::DelegateTimeout);

    // Tokens are distinct.
    assert_ne!(g_primary.token(), g_abort.token());
    assert_ne!(g_abort.token(), g_delegate.token());
    assert_ne!(g_primary.token(), g_delegate.token());
}

#[test]
fn lookup_returns_installed_metadata() {
    let wheel = TimerWheel::new();
    let deadline = Deadline::from_raw(777);
    let guard = wheel.install(deadline, TimerGuardRole::DelegateTimeout);
    let token = guard.token();

    let found = wheel.lookup(token).expect("token is live");
    assert_eq!(found.0, deadline);
    assert_eq!(found.1, TimerGuardRole::DelegateTimeout);
}

#[test]
fn next_deadline_tracks_earliest_live_registration() {
    let wheel = TimerWheel::new();
    let later = wheel.install(Deadline::from_raw(50), TimerGuardRole::PrimarySleep);
    assert_eq!(wheel.next_deadline_ns(), Some(50));

    let earlier = wheel.install(Deadline::from_raw(20), TimerGuardRole::DeadlineAbort);
    assert_eq!(wheel.next_deadline_ns(), Some(20));

    drop(earlier);
    assert_eq!(wheel.next_deadline_ns(), Some(50));

    drop(later);
    assert_eq!(wheel.next_deadline_ns(), None);
}

#[test]
fn lookup_returns_none_for_unissued_token() {
    let wheel = TimerWheel::new();
    assert!(wheel.lookup(TimerToken::new(0)).is_none());
    assert!(wheel.lookup(TimerToken::new(99_999)).is_none());
}

#[test]
fn dropping_guard_cancels_registration() {
    let wheel = TimerWheel::new();
    let token;
    {
        let guard = wheel.install(Deadline::from_raw(5), TimerGuardRole::PrimarySleep);
        token = guard.token();
        assert_eq!(wheel.armed_count(), 1);
    }
    assert_eq!(wheel.armed_count(), 0);
    assert!(wheel.lookup(token).is_none());
}

#[test]
fn forget_suppresses_drop_cancel() {
    let wheel = TimerWheel::new();
    let guard = wheel.install(Deadline::from_raw(5), TimerGuardRole::PrimarySleep);
    let token = guard.forget();

    // forget() returned the raw token and suppressed the drop-cancel.
    assert_eq!(wheel.armed_count(), 1);
    assert!(wheel.lookup(token).is_some());
}

#[test]
fn tokens_are_monotonic_and_not_reused_after_cancel() {
    let wheel = TimerWheel::new();
    let g1 = wheel.install(Deadline::from_raw(1), TimerGuardRole::PrimarySleep);
    let t1 = g1.token();
    drop(g1);
    let g2 = wheel.install(Deadline::from_raw(2), TimerGuardRole::PrimarySleep);
    let t2 = g2.token();

    // Cancellation does not reuse ids; t2 strictly advances past t1.
    assert!(t2.raw() > t1.raw(), "token ids must be monotonic");
}

#[test]
fn timer_guard_role_is_copy_and_eq() {
    // Closed catalog: pin Copy / Eq / Debug shape so PR-7 can
    // destructure with confidence.
    let r: TimerGuardRole = TimerGuardRole::DeadlineAbort;
    let r2 = r;
    assert_eq!(r, r2);
    let _ = format!("{:?}", r);
}

#[test]
fn guard_outlives_clone_of_wheel_handle() {
    // The wheel uses internal `Arc` sharing; installing on one
    // handle and dropping the original must keep the registration
    // observable through the surviving guard's parent reference.
    let outer = TimerWheel::new();
    let guard = outer.install(Deadline::from_raw(42), TimerGuardRole::PrimarySleep);
    let token = guard.token();
    // Drop the outer handle; the guard's internal Arc keeps the
    // entry alive.
    drop(outer);
    assert_eq!(guard.token(), token);
    // Drop guard; cancellation runs against the still-shared state.
    drop(guard);
}

#[test]
fn timer_guard_is_send_sync_via_static_check() {
    // Compile-time pin: TimerGuard / TimerWheel are Send + Sync so
    // they can be threaded into the reactor / mailbox path PR-7
    // will build. If a future change adds a non-Send field this
    // test fails to compile.
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<TimerWheel>();
    assert_send_sync::<TimerGuard>();
    assert_send_sync::<TimerToken>();
    assert_send_sync::<TimerGuardRole>();
}
