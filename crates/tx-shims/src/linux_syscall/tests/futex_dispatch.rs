// Auto-extracted from `tests.rs` (2026-05-08 jumbo split).
#![cfg_attr(test, allow(unused_imports))]
use super::*;
use crate::adapter::step_engine::page_allocator;
use tx_subsystems::process::bootstrap_init_process;

use crate::linux_syscall::{
    FUTEX_CLOCK_REALTIME, FUTEX_CMP_REQUEUE, FUTEX_LOCK_PI, FUTEX_PRIVATE_FLAG, FUTEX_REQUEUE,
    FUTEX_TRYLOCK_PI, FUTEX_UNLOCK_PI, FUTEX_WAIT, FUTEX_WAIT_BITSET, FUTEX_WAKE,
    FUTEX_WAKE_BITSET, FUTEX_WAKE_OP, NR_FUTEX,
};
use std::sync::Arc;
use tx_substrate::wake::{MailboxSchedulerHint, TaskMailbox, TimerWheel};

const E_INVAL: i32 = 22;
const E_AGAIN: i32 = 11;
const E_TIMEDOUT: i32 = 110;

#[repr(C)]
#[derive(Clone, Copy)]
struct TestTimespec {
    tv_sec: i64,
    tv_nsec: i64,
}

fn futex_setup() -> TestSetup {
    let setup = setup();
    match page_allocator::claim_zero_frame() {
        Ok(_) | Err(page_allocator::AllocError::AlreadyInstalled) => {}
        Err(error) => panic!("claim zero frame for futex tests: {error:?}"),
    }
    setup
}

fn fresh_proc_thread() -> (Cap<ProcessIdentity>, Cap<ThreadIdentity>) {
    let process = bootstrap_init_process(fresh_aspace()).expect("bootstrap init for futex tests");
    let thread = process.nth_thread(0).expect("leader thread");
    (process, thread)
}

fn map_user_futex_word_at(ctx: &SyscallCtx<'_>, uaddr: usize, value: u32) -> u64 {
    use tx_subsystems::vm::{
        MapPlacement, Prot, UserRange, UserVirtAddr, VmBacking, VmEntryFlags, VmMapRequest,
        USER_PAGE_SIZE,
    };

    let page_start = uaddr;
    let range =
        UserRange::new_aligned(UserVirtAddr(page_start), USER_PAGE_SIZE).expect("aligned range");
    let map_req = VmMapRequest::fixed(
        range,
        MapPlacement::FixedReplace,
        Prot::READ_WRITE,
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );
    ctx.aspace.try_mmap(map_req).expect("mmap anon for test");

    let guard = guard();
    let copied = ctx.aspace.copy_to_user(
        tx_hal::UserPtr::<u8>::new(uaddr),
        &value.to_ne_bytes(),
        &guard,
    );
    drop(guard);
    assert_eq!(copied, StepOutcome::Done(core::mem::size_of::<u32>()));
    uaddr as u64
}

fn map_user_futex_word(ctx: &SyscallCtx<'_>, value: u32) -> u64 {
    map_user_futex_word_at(ctx, 0x5100_0000, value)
}

fn map_user_timespec(ctx: &SyscallCtx<'_>, ts: TestTimespec) -> u64 {
    use tx_subsystems::vm::{
        MapPlacement, Prot, UserRange, UserVirtAddr, VmBacking, VmEntryFlags, VmMapRequest,
        USER_PAGE_SIZE,
    };

    let uaddr = 0x5100_1000;
    let page_start = uaddr;
    let range =
        UserRange::new_aligned(UserVirtAddr(page_start), USER_PAGE_SIZE).expect("aligned range");
    let map_req = VmMapRequest::fixed(
        range,
        MapPlacement::FixedReplace,
        Prot::READ_WRITE,
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );
    ctx.aspace.try_mmap(map_req).expect("mmap anon for test");

    let local = ts;
    let bytes = unsafe {
        core::slice::from_raw_parts(
            core::ptr::addr_of!(local) as *const u8,
            core::mem::size_of::<TestTimespec>(),
        )
    };
    let guard = guard();
    let copied = ctx
        .aspace
        .copy_to_user(tx_hal::UserPtr::<u8>::new(uaddr), bytes, &guard);
    drop(guard);
    assert_eq!(
        copied,
        StepOutcome::Done(core::mem::size_of::<TestTimespec>())
    );
    uaddr as u64
}

fn ctx_with_mailbox_and_timer(ctx: SyscallCtx<'static>) -> (SyscallCtx<'static>, TimerWheel) {
    let mailbox = Arc::new(TaskMailbox::new());
    let wheel = TimerWheel::new();
    let ctx = ctx.with_mailbox(mailbox).with_timer_wheel(wheel.clone());
    (ctx, wheel)
}

/// `futex(uaddr, FUTEX_WAIT, val, ...)` with `*uaddr != val`
/// returns `-EAGAIN` immediately (first-call mismatch — the
/// futex's "fast path" guard short-circuits before parking).
#[test]
fn dispatch_futex_wait_with_mismatched_val_returns_neg_eagain() {
    let _setup = futex_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);
    let uaddr = map_user_futex_word(&ctx, 0x1234);

    let req = SyscallRequest::new(NR_FUTEX, [uaddr, FUTEX_WAIT as u64, 0x5678, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_AGAIN));
}

/// `futex(0, FUTEX_WAIT, ...)` returns `-EINVAL` — null uaddr
/// is rejected at the step level.
#[test]
fn dispatch_futex_wait_zero_uaddr_returns_neg_einval() {
    let _setup = futex_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);

    let req = SyscallRequest::new(NR_FUTEX, [0, FUTEX_WAIT as u64, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_INVAL));
}

#[test]
fn dispatch_futex_wait_with_zero_timeout_returns_neg_etimedout() {
    let _setup = futex_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);
    let (ctx, _wheel) = ctx_with_mailbox_and_timer(ctx);
    let uaddr = map_user_futex_word(&ctx, 0x1234);
    let timeout = map_user_timespec(
        &ctx,
        TestTimespec {
            tv_sec: 0,
            tv_nsec: 0,
        },
    );

    let req = SyscallRequest::new(NR_FUTEX, [uaddr, FUTEX_WAIT as u64, 0x1234, timeout, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_TIMEDOUT));
}

/// `futex(uaddr, FUTEX_WAKE, n, ...)` reports actual registered
/// waiters. With no parked waiter, Linux returns 0.
#[test]
fn dispatch_futex_wake_returns_n() {
    let _setup = futex_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);
    let uaddr = map_user_futex_word(&ctx, 0);

    let req = SyscallRequest::new(NR_FUTEX, [uaddr, FUTEX_WAKE as u64, 3, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
}

#[test]
fn direct_trap_futex_wake_uses_wake_handoff_hint() {
    let _setup = futex_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let mailbox = Arc::new(TaskMailbox::new());
    let ctx = make_ctx(proc_cap.clone(), thread.clone()).with_mailbox(mailbox.clone());
    let uaddr = map_user_futex_word(&ctx, 0x1234);

    let wait_req = SyscallRequest::new(NR_FUTEX, [uaddr, FUTEX_WAIT as u64, 0x1234, 0, 0, 0]);
    let waker = Waker::noop().clone();
    let mut cx = Context::from_waker(&waker);
    let mut wait = Box::pin(dispatch::<ShimsTestPmap>(wait_req, &ctx));
    assert!(
        matches!(wait.as_mut().poll(&mut cx), Poll::Pending),
        "matching futex wait should park before the wake"
    );

    let wake_req = SyscallRequest::new(NR_FUTEX, [uaddr, FUTEX_WAKE as u64, 1, 0, 0, 0]);
    let result = crate::linux_syscall::dispatch_direct_trap_oneshot(
        &wake_req,
        &proc_cap,
        &thread,
        &ctx.aspace,
    );

    assert_eq!(result, Some(SyscallResult::Return(1)));
    assert_eq!(
        mailbox.take_scheduler_hint(),
        MailboxSchedulerHint::WakeHandoff
    );
}

/// `futex(uaddr, FUTEX_REQUEUE, ...)` accepts a valid source and
/// target futex word and returns a best-effort wake/requeue count.
#[test]
fn dispatch_futex_requeue_with_valid_uaddrs_returns_best_effort_count() {
    let _setup = futex_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);
    let source: u32 = 0;
    let target: u32 = 0;
    let uaddr = &source as *const u32 as u64;
    let uaddr2 = &target as *const u32 as u64;

    let req = SyscallRequest::new(NR_FUTEX, [uaddr, FUTEX_REQUEUE as u64, 1, 1, uaddr2, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert!(matches!(result, SyscallResult::Return(_)));
}

#[test]
fn dispatch_futex_cmp_requeue_mismatch_returns_neg_eagain() {
    let _setup = futex_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);
    let source = map_user_futex_word_at(&ctx, 0x5100_0000, 0x1234);
    let target = map_user_futex_word_at(&ctx, 0x5100_2000, 0);

    let req = SyscallRequest::new(
        NR_FUTEX,
        [source, FUTEX_CMP_REQUEUE as u64, 1, 1, target, 0x5678],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_AGAIN));
}

/// `futex(uaddr, FUTEX_WAKE | FUTEX_PRIVATE_FLAG, n, ...)`
/// behaves identically to plain `FUTEX_WAKE` — the PRIVATE flag
/// is masked off before the op match. musl emits the
/// `_PRIVATE` form for in-process guards.
#[test]
fn dispatch_futex_with_private_flag_works() {
    let _setup = futex_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);
    let uaddr = map_user_futex_word(&ctx, 0);

    let op = FUTEX_WAKE | FUTEX_PRIVATE_FLAG;
    let req = SyscallRequest::new(NR_FUTEX, [uaddr, op as u64, 5, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
}

/// `futex(uaddr, FUTEX_WAKE | FUTEX_CLOCK_REALTIME, n, ...)`
/// behaves identically to plain `FUTEX_WAKE` — the
/// CLOCK_REALTIME flag is masked off before the op match
/// (timeout support deferred to Slice 4).
#[test]
fn dispatch_futex_with_clock_realtime_flag_works() {
    let _setup = futex_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);
    let uaddr = map_user_futex_word(&ctx, 0);

    let op = FUTEX_WAKE | FUTEX_CLOCK_REALTIME;
    let req = SyscallRequest::new(NR_FUTEX, [uaddr, op as u64, 2, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
}

#[test]
fn dispatch_futex_wake_bitset_zero_bitset_returns_neg_einval() {
    let _setup = futex_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);
    let uaddr = map_user_futex_word(&ctx, 0);

    let req = SyscallRequest::new(NR_FUTEX, [uaddr, FUTEX_WAKE_BITSET as u64, 1, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_INVAL));
}

#[test]
fn dispatch_futex_wait_bitset_with_zero_timeout_returns_neg_etimedout() {
    let _setup = futex_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);
    let (ctx, _wheel) = ctx_with_mailbox_and_timer(ctx);
    let uaddr = map_user_futex_word(&ctx, 0x1234);
    let timeout = map_user_timespec(
        &ctx,
        TestTimespec {
            tv_sec: 0,
            tv_nsec: 0,
        },
    );

    let req = SyscallRequest::new(
        NR_FUTEX,
        [uaddr, FUTEX_WAIT_BITSET as u64, 0x1234, timeout, 0, 0x2],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_TIMEDOUT));
}

/// `futex(uaddr, FUTEX_WAKE_OP, ...)` accepts a valid source and
/// target futex word and returns a best-effort wake count.
#[test]
fn dispatch_futex_wake_op_with_valid_uaddrs_returns_best_effort_count() {
    let _setup = futex_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);
    let source: u32 = 0;
    let target: u32 = 0;
    let uaddr = &source as *const u32 as u64;
    let uaddr2 = &target as *const u32 as u64;

    let req = SyscallRequest::new(NR_FUTEX, [uaddr, FUTEX_WAKE_OP as u64, 1, 1, uaddr2, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert!(matches!(result, SyscallResult::Return(_)));
}

#[test]
fn dispatch_futex_pi_lock_trylock_unlock_round_trip() {
    let _setup = futex_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);
    let uaddr = map_user_futex_word(&ctx, 0);

    let lock_req = SyscallRequest::new(NR_FUTEX, [uaddr, FUTEX_LOCK_PI as u64, 0, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(lock_req, &ctx)),
        SyscallResult::Return(0)
    );

    let try_req = SyscallRequest::new(NR_FUTEX, [uaddr, FUTEX_TRYLOCK_PI as u64, 0, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(try_req, &ctx)),
        SyscallResult::Error(E_AGAIN)
    );

    let unlock_req = SyscallRequest::new(NR_FUTEX, [uaddr, FUTEX_UNLOCK_PI as u64, 0, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(unlock_req, &ctx)),
        SyscallResult::Return(0)
    );
}
