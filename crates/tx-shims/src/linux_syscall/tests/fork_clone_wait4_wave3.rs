// Auto-extracted from `tests.rs` (2026-05-08 jumbo split).
#![cfg_attr(test, allow(unused_imports))]
use super::*;

use tx_hal::UserTrapContext;
use tx_subsystems::process::{step_exit_group, ExitStatus};
use tx_subsystems::reactor_submit;

/// No-op reactor-submit seam for tests that fork via `sys_clone`.
/// The blocking-wait tests just need the seam to not panic; they
/// don't poll the child, so a trivial seam is enough.
fn install_noop_submit_seam() {
    fn noop(_p: Cap<ProcessIdentity>, _t: Cap<ThreadIdentity>) {}
    reactor_submit::install_submit_child_thread(noop);
}

/// Stamp a parent trap context so `sys_clone` finds the saved
/// context. Mirrors the Wave 2 helper.
fn seed_parent_trap_context(thread: &Cap<ThreadIdentity>) {
    let regs = [0usize; 32];
    let parent_ctx = UserTrapContext {
        regs,
        pc: 0x4000_2000,
        status: 0x123,
        fp: tx_hal::UserFpContext::empty(),
    };
    thread
        .payload_cap()
        .expect("alive thread payload")
        .store_saved_user_context(Some(parent_ctx));
}

/// `wait4(-1, NULL, 0, NULL)` from a process with no children
/// returns `-ECHILD`.
#[test]
fn dispatch_wait4_no_children_returns_neg_echild() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    // pid = -1 (Any), wstatus = NULL, options = 0, rusage = NULL.
    let req = SyscallRequest::new(NR_WAIT4, [(-1i64) as u64, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(10), "expected -ECHILD");
}

/// `wait4(-1, NULL, WNOHANG, NULL)` with a live (non-zombie) child
/// returns `0` (no zombie ready). The child is still alive after
/// the call.
#[test]
fn dispatch_wait4_wnohang_no_zombies_returns_zero() {
    let _setup = setup();
    install_noop_submit_seam();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    seed_parent_trap_context(&thread);
    let ctx = make_ctx(proc_cap.clone(), thread);

    // Fork once.
    let clone_req = SyscallRequest::new(NR_CLONE, [SIGCHLD, 0, 0, 0, 0, 0]);
    let _ = block_on(dispatch::<ShimsTestPmap>(clone_req, &ctx));
    assert_eq!(proc_cap.children().len(), 1);
    let child = proc_cap.children()[0].clone();
    assert!(!child.is_zombie());

    // wait4(-1, NULL, WNOHANG, NULL) → Return(0).
    let req = SyscallRequest::new(NR_WAIT4, [(-1i64) as u64, 0, WNOHANG as u64, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));

    // Child is still alive (not reaped).
    assert!(!child.is_zombie());
    assert_eq!(proc_cap.children().len(), 1);
}

/// `wait4(-1, NULL, WNOHANG, NULL)` after the child zombifies
/// returns the child's pid and reaps the zombie (parent.children
/// shrinks).
#[test]
fn dispatch_wait4_wnohang_zombie_ready_reaps_and_returns_pid() {
    let _setup = setup();
    install_noop_submit_seam();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    seed_parent_trap_context(&thread);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let clone_req = SyscallRequest::new(NR_CLONE, [SIGCHLD, 0, 0, 0, 0, 0]);
    let _ = block_on(dispatch::<ShimsTestPmap>(clone_req, &ctx));
    let child = proc_cap.children()[0].clone();
    let child_pid = child.pid.0 as i64;

    // Zombify the child via step_exit_group.
    step_exit_group(&child, ExitStatus::Exited(0));
    assert!(child.is_zombie());

    let req = SyscallRequest::new(NR_WAIT4, [(-1i64) as u64, 0, WNOHANG as u64, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(child_pid));

    // Child has been reaped: parent.children no longer contains it.
    assert_eq!(
        proc_cap.children().len(),
        0,
        "wait4 must reap the zombie out of parent.children"
    );
}

/// `wait4(-1, &mut ws, WNOHANG, NULL)` with `ExitStatus::Exited(42)`
/// writes `0x2a00` to the user wstatus address per the POSIX
/// `<sys/wait.h>` encoding (`(code & 0xff) << 8`).
#[test]
fn dispatch_wait4_wnohang_writes_status_word_for_exited() {
    let _setup = setup();
    install_noop_submit_seam();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    seed_parent_trap_context(&thread);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let clone_req = SyscallRequest::new(NR_CLONE, [SIGCHLD, 0, 0, 0, 0, 0]);
    let _ = block_on(dispatch::<ShimsTestPmap>(clone_req, &ctx));
    let child = proc_cap.children()[0].clone();

    step_exit_group(&child, ExitStatus::Exited(42));

    let mut wstatus: i32 = -1;
    let wstatus_addr = &mut wstatus as *mut i32 as u64;
    let req = SyscallRequest::new(
        NR_WAIT4,
        [(-1i64) as u64, wstatus_addr, WNOHANG as u64, 0, 0, 0],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert!(matches!(result, SyscallResult::Return(_)));

    // POSIX: Exited(42) → (42 & 0xff) << 8 = 0x2a00.
    assert_eq!(
        wstatus, 0x2a00,
        "wstatus must encode Exited(42) as (42 << 8) per <sys/wait.h>"
    );
}

/// `wait4(child_a.pid, NULL, WNOHANG, NULL)` skips zombies that
/// are not the requested pid. Two-fork shape: A is alive, B is
/// zombie; selector targets A → Return(0). Then zombify A and call
/// again → Return(A.pid).
#[test]
fn dispatch_wait4_specific_pid_skips_other_zombies() {
    let _setup = setup();
    install_noop_submit_seam();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    seed_parent_trap_context(&thread);
    let ctx = make_ctx(proc_cap.clone(), thread);

    // Fork A.
    let clone_req = SyscallRequest::new(NR_CLONE, [SIGCHLD, 0, 0, 0, 0, 0]);
    let _ = block_on(dispatch::<ShimsTestPmap>(clone_req, &ctx));
    let child_a = proc_cap.children()[0].clone();
    let child_a_pid = child_a.pid.0 as i64;

    // Fork B (re-seed the parent ctx because step_fork doesn't
    // touch saved_user_context).
    seed_parent_trap_context(&first_thread(&proc_cap));
    let _ = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_CLONE, [SIGCHLD, 0, 0, 0, 0, 0]),
        &ctx,
    ));
    let children = proc_cap.children();
    assert_eq!(children.len(), 2);
    let child_b = children
        .iter()
        .find(|c| c.pid.0 != child_a.pid.0)
        .expect("two distinct children")
        .clone();

    // Zombify B; A still alive.
    step_exit_group(&child_b, ExitStatus::Exited(0));
    assert!(child_b.is_zombie());
    assert!(!child_a.is_zombie());

    // wait4(A.pid, ...) WNOHANG: A is alive, so no zombie matches
    // the selector → Return(0). B's zombie state must NOT satisfy
    // a Pid(A.pid) selector.
    let req = SyscallRequest::new(NR_WAIT4, [child_a_pid as u64, 0, WNOHANG as u64, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(
        result,
        SyscallResult::Return(0),
        "wait4(A.pid) must not reap zombie B"
    );

    // Now zombify A; call wait4(A.pid, ...) again → Return(A.pid).
    step_exit_group(&child_a, ExitStatus::Exited(0));
    let req = SyscallRequest::new(NR_WAIT4, [child_a_pid as u64, 0, WNOHANG as u64, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(child_a_pid));
}

/// Blocking wait4 — load-bearing test. Fork a child; spawn a host
/// driver future that polls `sys_wait4(-1, NULL, 0, NULL)`. First
/// poll → Pending (no zombie). Manually call `step_exit_group` on
/// the child (which routes through `post_sigchld_to_parent` and
/// fires the parent's `exit_source` channel). Subsequent polls →
/// Ready with the child's pid.
#[test]
fn dispatch_wait4_blocking_resolves_when_child_zombifies() {
    let _setup = setup();
    install_noop_submit_seam();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    seed_parent_trap_context(&thread);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let clone_req = SyscallRequest::new(NR_CLONE, [SIGCHLD, 0, 0, 0, 0, 0]);
    let _ = block_on(dispatch::<ShimsTestPmap>(clone_req, &ctx));
    let child = proc_cap.children()[0].clone();
    let child_pid_i64 = child.pid.0 as i64;
    assert!(!child.is_zombie());

    // Drive the wait4 future manually so we can interleave the
    // child's exit between polls. Same shape as
    // `dispatch_read_blocks_until_tty_input_then_returns_byte`.
    let waker = Waker::noop().clone();
    let mut cx = Context::from_waker(&waker);

    // Blocking variant — options = 0 (no WNOHANG).
    let req = SyscallRequest::new(NR_WAIT4, [(-1i64) as u64, 0, 0, 0, 0, 0]);
    let fut = dispatch::<ShimsTestPmap>(req, &ctx);
    let mut pinned = Box::pin(fut);

    // First poll: no zombie ready, parks on exit_source via
    // wait_source::wait_on_token.
    let first = pinned.as_mut().poll(&mut cx);
    assert!(
        matches!(first, Poll::Pending),
        "blocking wait4 with no ready zombie should park; got {first:?}"
    );

    // Now zombify the child. step_exit_group →
    // post_sigchld_to_parent → parent.fire_exit_source(...) — fires
    // the EXIT_SOURCE_CHILD_ZOMBIFIED bit on the parent's channel,
    // which wakes the WaitFuture.
    step_exit_group(&child, ExitStatus::Exited(0));
    assert!(child.is_zombie());

    // Spin-poll a bounded number of times so a stuck future fails
    // fast rather than hanging the test.
    let mut last = Poll::Pending;
    for _ in 0..256 {
        last = pinned.as_mut().poll(&mut cx);
        if let Poll::Ready(value) = last {
            assert_eq!(
                value,
                SyscallResult::Return(child_pid_i64),
                "wait4 should observe the now-zombie child"
            );
            // The reap retired the child from parent.children.
            assert_eq!(proc_cap.children().len(), 0);
            return;
        }
    }
    panic!("dispatch_wait4 did not resolve after child zombified; last poll = {last:?}");
}

/// Non-NULL `rusage` receives a zero-filled musl/Linux LP64
/// `struct rusage` image. Usage accounting is not wired yet, but
/// libc callers that pass a buffer should not see `EINVAL`.
#[test]
fn dispatch_wait4_rusage_nonzero_writes_zeroed_rusage() {
    let _setup = setup();
    install_noop_submit_seam();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    seed_parent_trap_context(&thread);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let clone_req = SyscallRequest::new(NR_CLONE, [SIGCHLD, 0, 0, 0, 0, 0]);
    let _ = block_on(dispatch::<ShimsTestPmap>(clone_req, &ctx));
    let child = proc_cap.children()[0].clone();
    step_exit_group(&child, ExitStatus::Exited(0));

    let mut rusage = [0xa5u8; 256];
    let req = SyscallRequest::new(
        NR_WAIT4,
        [
            (-1i64) as u64,
            0,
            WNOHANG as u64,
            rusage.as_mut_ptr() as u64,
            0,
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(child.pid.0 as i64));
    assert_eq!(rusage, [0u8; 256]);
}

/// `WUNTRACED` (0x2) and `WCONTINUED` (0x8) are accepted but
/// silently ignored — Linux ignores unknown options bits for
/// `wait4`. Verifies the arm doesn't fail with `-EINVAL` on
/// these bits; with no zombie ready and no `WNOHANG`, the call
/// would block, so we add `WNOHANG` to keep the test bounded.
#[test]
fn dispatch_wait4_unknown_options_bits_ignored() {
    let _setup = setup();
    install_noop_submit_seam();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    seed_parent_trap_context(&thread);
    let ctx = make_ctx(proc_cap.clone(), thread);

    // Need a child so the call returns Return(0) (no ready zombie)
    // rather than -ECHILD (no children).
    let clone_req = SyscallRequest::new(NR_CLONE, [SIGCHLD, 0, 0, 0, 0, 0]);
    let _ = block_on(dispatch::<ShimsTestPmap>(clone_req, &ctx));

    const WUNTRACED: i32 = 0x2;
    const WCONTINUED: i32 = 0x8;
    let options = WNOHANG | WUNTRACED | WCONTINUED;
    let req = SyscallRequest::new(NR_WAIT4, [(-1i64) as u64, 0, options as u64, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    // WNOHANG + no zombie ready → Return(0). The crucial
    // assertion is "not -EINVAL" — the unknown bits were tolerated.
    assert_eq!(
        result,
        SyscallResult::Return(0),
        "WUNTRACED/WCONTINUED must be silently ignored, not rejected"
    );
}

/// `wait4(i32::MIN, ...)` overflows on negate (the `WaitTarget`
/// translation would compute `Pgrp(-i32::MIN as u32)` which is
/// undefined behaviour); we reject upfront with `-EINVAL` per
/// LTP `wait403`.
#[test]
fn dispatch_wait4_intmin_pid_returns_neg_einval() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    let req = SyscallRequest::new(NR_WAIT4, [(i32::MIN as i64) as u64, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(22), "expected -EINVAL");
}

/// `wait4(-pgid, NULL, WNOHANG, NULL)` matches a zombie child
/// whose pgid equals `pgid`. By default a fork inherits the
/// parent's pgrp, so the child's pgid == init's pgid == 1; we
/// pass `pid = -1` (which would catch any child) plus a separate
/// run with `pid = -(child.pgid)` to verify the pgrp selector
/// path.
#[test]
fn dispatch_wait4_pgid_selector_picks_grouped_zombie() {
    let _setup = setup();
    install_noop_submit_seam();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    seed_parent_trap_context(&thread);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let clone_req = SyscallRequest::new(NR_CLONE, [SIGCHLD, 0, 0, 0, 0, 0]);
    let _ = block_on(dispatch::<ShimsTestPmap>(clone_req, &ctx));
    let child = proc_cap.children()[0].clone();
    let child_pid_i64 = child.pid.0 as i64;
    let child_pgid = child.pgrp_cap().pgid.0 as i64;

    step_exit_group(&child, ExitStatus::Exited(0));
    assert!(child.is_zombie());

    // pid = -(pgid). The selector becomes Pgrp(child_pgid). Since
    // the child's pgid == child_pgid (inherited from parent), this
    // matches.
    let neg_pgid = -child_pgid;
    let req = SyscallRequest::new(NR_WAIT4, [neg_pgid as u64, 0, WNOHANG as u64, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(
        result,
        SyscallResult::Return(child_pid_i64),
        "wait4(-pgid, ...) must reap a zombie in the target pgrp"
    );
}
