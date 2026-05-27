// Auto-extracted from `tests.rs` (2026-05-08 jumbo split).
#![cfg_attr(test, allow(unused_imports))]
use super::*;
use crate::adapter::step_engine::page_allocator;
use tx_subsystems::process::bootstrap_init_process;

use crate::linux_syscall::{
    CLOCK_MONOTONIC, FUTEX_32, FUTEX_CLOCK_REALTIME, FUTEX_CMP_REQUEUE, FUTEX_CMP_REQUEUE_PI,
    FUTEX_LOCK_PI, FUTEX_LOCK_PI2, FUTEX_PRIVATE_FLAG, FUTEX_REQUEUE, FUTEX_TRYLOCK_PI,
    FUTEX_UNLOCK_PI, FUTEX_WAIT, FUTEX_WAIT_BITSET, FUTEX_WAIT_REQUEUE_PI, FUTEX_WAKE,
    FUTEX_WAKE_BITSET, FUTEX_WAKE_OP, NR_FUTEX, NR_FUTEX2_REQUEUE, NR_FUTEX2_WAIT, NR_FUTEX2_WAKE,
    NR_FUTEX_WAITV, NR_TIMER_CREATE, NR_TIMER_SETTIME,
};
use std::sync::Arc;
use tx_reactor::{InitialSchedMeta, Reactor, SchedClass};
use tx_substrate::wake::{TaskMailbox, TimerWheel};
use tx_subsystems::process::execution::spawn_sibling_thread_for_test;
use tx_subsystems::signal::{step_sigaction, SigDisposition, Signum};

const E_INVAL: i32 = 22;
const E_AGAIN: i32 = 11;
const E_INTR: i32 = 4;
const E_DEADLK: i32 = 35;
const E_SRCH: i32 = 3;
const E_NOSYS: i32 = 38;
const E_TIMEDOUT: i32 = 110;
const SIGALRM_RAW: u8 = 14;
const FUTEX_WAITERS_BIT: u32 = 0x8000_0000;
const FUTEX_TID_MASK: u32 = 0x3fff_ffff;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct TestTimespec {
    tv_sec: i64,
    tv_nsec: i64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct TestItimerspec {
    it_interval: TestTimespec,
    it_value: TestTimespec,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct TestFutexWaitv {
    val: u64,
    uaddr: u64,
    flags: u32,
    reserved: u32,
}

fn futex_setup() -> TestSetup {
    let setup = setup();
    *TEST_REACTOR.lock().expect("test reactor lock") = None;
    tx_subsystems::futex::reset_for_test();
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

fn map_user_bytes_at(ctx: &SyscallCtx<'_>, uaddr: usize, bytes: &[u8]) -> u64 {
    use tx_subsystems::vm::{
        MapPlacement, Prot, UserRange, UserVirtAddr, VmBacking, VmEntryFlags, VmMapRequest,
        USER_PAGE_SIZE,
    };

    let page_start = uaddr & !(USER_PAGE_SIZE - 1);
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
    let copied = ctx
        .aspace
        .copy_to_user(tx_hal::UserPtr::<u8>::new(uaddr), bytes, &guard);
    drop(guard);
    assert_eq!(copied, StepOutcome::Done(bytes.len()));
    uaddr as u64
}

fn map_user_waitv_array(ctx: &SyscallCtx<'_>, waiters: &[TestFutexWaitv]) -> u64 {
    let bytes = unsafe {
        core::slice::from_raw_parts(
            waiters.as_ptr() as *const u8,
            core::mem::size_of_val(waiters),
        )
    };
    map_user_bytes_at(ctx, 0x5100_3000, bytes)
}

fn read_user_futex_word(ctx: &SyscallCtx<'_>, uaddr: u64) -> u32 {
    let guard = guard();
    let value = match ctx
        .aspace
        .read_user(tx_hal::UserPtr::<u32>::new(uaddr as usize), &guard)
    {
        StepOutcome::Done(value) => value,
        other => panic!("read_user futex word failed: {other:?}"),
    };
    drop(guard);
    value
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

fn create_posix_timer(ctx: &SyscallCtx<'_>, clockid: u32) -> u32 {
    let mut timer_id = -1i32;
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_TIMER_CREATE,
            [clockid as u64, 0, &mut timer_id as *mut i32 as u64, 0, 0, 0],
        ),
        ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));
    timer_id as u32
}

fn install_reactor_priority_for_test() {
    *TEST_REACTOR.lock().expect("test reactor lock") = Some(Reactor::new());
    tx_subsystems::reactor_priority::install_priority_donation_for_test(
        |owner, donor, rt_priority| {
            TEST_REACTOR
                .lock()
                .expect("test reactor lock")
                .as_ref()
                .expect("test reactor installed")
                .donate_priority(owner, donor, rt_priority)
        },
        |token| {
            TEST_REACTOR
                .lock()
                .expect("test reactor lock")
                .as_ref()
                .expect("test reactor installed")
                .drop_priority_donation(token)
        },
        |task| {
            TEST_REACTOR
                .lock()
                .expect("test reactor lock")
                .as_ref()
                .expect("test reactor installed")
                .task_effective_rt_priority(task)
        },
        |owner, lock, waiter, priority| {
            TEST_REACTOR
                .lock()
                .expect("test reactor lock")
                .as_ref()
                .expect("test reactor installed")
                .upsert_pi_waiter(owner, lock, waiter, priority)
        },
        |owner, lock| {
            TEST_REACTOR
                .lock()
                .expect("test reactor lock")
                .as_ref()
                .expect("test reactor installed")
                .remove_pi_waiter(owner, lock)
        },
        |task| {
            TEST_REACTOR
                .lock()
                .expect("test reactor lock")
                .as_ref()
                .expect("test reactor installed")
                .task_effective_priority_key(task)
        },
    );
}

static TEST_REACTOR: std::sync::Mutex<Option<Reactor>> = std::sync::Mutex::new(None);

fn test_reactor_submit(meta: InitialSchedMeta) -> tx_reactor::TaskKey {
    TEST_REACTOR
        .lock()
        .expect("test reactor lock")
        .as_ref()
        .expect("test reactor installed")
        .submit_task_with_meta(async {}, meta)
}

fn test_reactor_effective_priority(task: tx_reactor::TaskKey) -> u8 {
    TEST_REACTOR
        .lock()
        .expect("test reactor lock")
        .as_ref()
        .expect("test reactor installed")
        .task_effective_rt_priority(task)
        .expect("task has priority metadata")
}

fn rt_user_meta(priority: u8) -> InitialSchedMeta {
    let mut meta = InitialSchedMeta::fair().userspace_thread();
    meta.class = SchedClass::RtFifo;
    meta.rt_priority = priority;
    meta
}

fn poll_once<F: Future>(future: &mut F) -> Poll<F::Output> {
    let waker = Waker::noop().clone();
    let mut cx = Context::from_waker(&waker);
    // SAFETY: the caller keeps the future in place across this poll.
    let mut pinned = unsafe { Pin::new_unchecked(future) };
    pinned.as_mut().poll(&mut cx)
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

#[test]
fn dispatch_futex_wait_wakes_for_process_timer_signal_deadline() {
    let _setup = futex_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap.clone(), thread.clone());
    let (ctx, wheel) = ctx_with_mailbox_and_timer(ctx);
    let sigalrm = Signum::new(SIGALRM_RAW).expect("SIGALRM signum");
    let _ = step_sigaction(&proc_cap, sigalrm, SigDisposition::Handler(0xCAFE));

    let timer_id = create_posix_timer(&ctx, CLOCK_MONOTONIC);
    let timer = TestItimerspec {
        it_interval: TestTimespec {
            tv_sec: 0,
            tv_nsec: 0,
        },
        it_value: TestTimespec {
            tv_sec: 0,
            tv_nsec: 1_000,
        },
    };
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_TIMER_SETTIME,
                [
                    timer_id as u64,
                    0,
                    &timer as *const TestItimerspec as u64,
                    0,
                    0,
                    0
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );

    let uaddr = map_user_futex_word(&ctx, 0x1234);
    let req = SyscallRequest::new(NR_FUTEX, [uaddr, FUTEX_WAIT as u64, 0x1234, 0, 0, 0]);
    let waker = Waker::noop().clone();
    let mut cx = Context::from_waker(&waker);
    let fut = dispatch::<ShimsTestPmap>(req, &ctx);
    let mut pinned = Box::pin(fut);

    assert!(matches!(pinned.as_mut().poll(&mut cx), Poll::Pending));
    assert_eq!(wheel.fire_due(5_001_000_000), 1);

    let result = pinned.as_mut().poll(&mut cx).map(|result| {
        assert_eq!(result, SyscallResult::Error(E_INTR));
    });
    assert!(
        result.is_ready(),
        "process timer deadline should interrupt futex wait"
    );

    let payload = thread.payload_cap().expect("live thread");
    assert!(
        payload.pending().is_pending(sigalrm),
        "futex process-timer wake should publish SIGALRM"
    );
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

/// Linux only accepts `FUTEX_CLOCK_REALTIME` on timed wait-style futex
/// operations. `FUTEX_WAKE | FUTEX_CLOCK_REALTIME` is an invalid op pairing.
#[test]
fn dispatch_futex_clock_realtime_on_wake_returns_neg_enosys() {
    let _setup = futex_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);
    let uaddr = map_user_futex_word(&ctx, 0);

    let op = FUTEX_WAKE | FUTEX_CLOCK_REALTIME;
    let req = SyscallRequest::new(NR_FUTEX, [uaddr, op as u64, 2, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_NOSYS));
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

#[test]
fn dispatch_futex_waitv_mismatch_returns_neg_eagain() {
    let _setup = futex_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);
    let uaddr = map_user_futex_word(&ctx, 0x1234);
    let waiters = [TestFutexWaitv {
        val: 0x5678,
        uaddr,
        flags: FUTEX_32,
        reserved: 0,
    }];
    let waiters_uaddr = map_user_waitv_array(&ctx, &waiters);

    let req = SyscallRequest::new(NR_FUTEX_WAITV, [waiters_uaddr, 1, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));

    assert_eq!(result, SyscallResult::Error(E_AGAIN));
}

#[test]
fn dispatch_futex_waitv_zero_timeout_returns_neg_etimedout() {
    let _setup = futex_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);
    let (ctx, _wheel) = ctx_with_mailbox_and_timer(ctx);
    let uaddr = map_user_futex_word(&ctx, 0x1234);
    let waiters = [TestFutexWaitv {
        val: 0x1234,
        uaddr,
        flags: FUTEX_32,
        reserved: 0,
    }];
    let waiters_uaddr = map_user_waitv_array(&ctx, &waiters);
    let timeout = map_user_timespec(
        &ctx,
        TestTimespec {
            tv_sec: 0,
            tv_nsec: 0,
        },
    );

    let req = SyscallRequest::new(NR_FUTEX_WAITV, [waiters_uaddr, 1, 0, timeout, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));

    assert_eq!(result, SyscallResult::Error(E_TIMEDOUT));
}

#[test]
fn dispatch_futex_waitv_wake_returns_woken_index() {
    let _setup = futex_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);
    let (ctx, _wheel) = ctx_with_mailbox_and_timer(ctx);
    let first = map_user_futex_word_at(&ctx, 0x5100_0000, 0x11);
    let second = map_user_futex_word_at(&ctx, 0x5100_2000, 0x22);
    let waiters = [
        TestFutexWaitv {
            val: 0x11,
            uaddr: first,
            flags: FUTEX_32,
            reserved: 0,
        },
        TestFutexWaitv {
            val: 0x22,
            uaddr: second,
            flags: FUTEX_32,
            reserved: 0,
        },
    ];
    let waiters_uaddr = map_user_waitv_array(&ctx, &waiters);
    let req = SyscallRequest::new(NR_FUTEX_WAITV, [waiters_uaddr, 2, 0, 0, 0, 0]);

    let waker = Waker::noop().clone();
    let mut cx = Context::from_waker(&waker);
    let fut = dispatch::<ShimsTestPmap>(req, &ctx);
    let mut pinned = Box::pin(fut);
    let first_poll = pinned.as_mut().poll(&mut cx);
    assert!(
        matches!(first_poll, Poll::Pending),
        "waitv with matching futexes should park; got {first_poll:?}"
    );

    let wake_req = SyscallRequest::new(NR_FUTEX, [second, FUTEX_WAKE as u64, 1, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(wake_req, &ctx)),
        SyscallResult::Return(1)
    );

    let mut last = Poll::Pending;
    for _ in 0..256 {
        last = pinned.as_mut().poll(&mut cx);
        if let Poll::Ready(value) = last {
            assert_eq!(value, SyscallResult::Return(1));
            return;
        }
    }
    panic!("futex_waitv did not resolve after waking index 1; last poll = {last:?}");
}

/// `futex(uaddr, FUTEX_WAKE_OP, ...)` accepts a valid source and
/// target futex word and returns a best-effort wake count.
#[test]
fn dispatch_futex_wake_op_with_valid_uaddrs_returns_best_effort_count() {
    let _setup = futex_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);
    let uaddr = map_user_futex_word_at(&ctx, 0x5100_0000, 0);
    let uaddr2 = map_user_futex_word_at(&ctx, 0x5100_2000, 0);

    let req = SyscallRequest::new(NR_FUTEX, [uaddr, FUTEX_WAKE_OP as u64, 1, 1, uaddr2, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert!(matches!(result, SyscallResult::Return(_)));
}

#[test]
fn dispatch_futex_wake_op_applies_encoded_operation_to_uaddr2() {
    let _setup = futex_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);
    let uaddr = map_user_futex_word_at(&ctx, 0x5100_0000, 0);
    let uaddr2 = map_user_futex_word_at(&ctx, 0x5100_2000, 5);

    // FUTEX_OP(FUTEX_OP_ADD, 3, FUTEX_OP_CMP_EQ, 5):
    // atomically add 3 to *uaddr2, then because old == 5, perform
    // the second wake. No waiters are registered here, so the return
    // count is 0; the externally visible contract is the RMW.
    let encoded = (1u64 << 28) | (0u64 << 24) | (3u64 << 12) | 5u64;
    let req = SyscallRequest::new(
        NR_FUTEX,
        [uaddr, FUTEX_WAKE_OP as u64, 1, 1, uaddr2, encoded],
    );

    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));

    assert_eq!(result, SyscallResult::Return(0));
    assert_eq!(read_user_futex_word(&ctx, uaddr2), 8);
}

#[test]
fn dispatch_futex_wake_op_shifted_set_uses_shifted_operand() {
    let _setup = futex_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);
    let uaddr = map_user_futex_word_at(&ctx, 0x5100_0000, 0);
    let uaddr2 = map_user_futex_word_at(&ctx, 0x5100_2000, 5);

    // FUTEX_OP(FUTEX_OP_SET | FUTEX_OP_OPARG_SHIFT, 4, FUTEX_OP_CMP_EQ, 5):
    // Linux shifts oparg before applying every operation, including SET.
    let encoded = (8u64 << 28) | (0u64 << 24) | (4u64 << 12) | 5u64;
    let req = SyscallRequest::new(
        NR_FUTEX,
        [uaddr, FUTEX_WAKE_OP as u64, 1, 1, uaddr2, encoded],
    );

    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));

    assert_eq!(result, SyscallResult::Return(0));
    assert_eq!(read_user_futex_word(&ctx, uaddr2), 16);
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
        SyscallResult::Error(E_DEADLK)
    );

    let unlock_req = SyscallRequest::new(NR_FUTEX, [uaddr, FUTEX_UNLOCK_PI as u64, 0, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(unlock_req, &ctx)),
        SyscallResult::Return(0)
    );
}

#[test]
fn dispatch_futex_pi_lock_by_owner_returns_neg_edeadlk() {
    let _setup = futex_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);
    let uaddr = map_user_futex_word(&ctx, 0);

    let lock_req = SyscallRequest::new(NR_FUTEX, [uaddr, FUTEX_LOCK_PI as u64, 0, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(lock_req, &ctx)),
        SyscallResult::Return(0)
    );

    let second_lock_req = SyscallRequest::new(NR_FUTEX, [uaddr, FUTEX_LOCK_PI as u64, 0, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(second_lock_req, &ctx)),
        SyscallResult::Error(E_DEADLK)
    );
}

#[test]
fn dispatch_futex_pi_lock_returns_neg_esrch_for_unknown_owner_tid() {
    let _setup = futex_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);
    let unknown_owner_tid = 0x12345;
    assert_ne!(ctx.thread.tid.0, unknown_owner_tid);
    let uaddr = map_user_futex_word(&ctx, unknown_owner_tid);

    let lock_req = SyscallRequest::new(NR_FUTEX, [uaddr, FUTEX_LOCK_PI as u64, 0, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(lock_req, &ctx)),
        SyscallResult::Error(E_SRCH)
    );
}

#[test]
fn dispatch_futex_trylock_pi_returns_neg_esrch_for_unknown_owner_tid() {
    let _setup = futex_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);
    let unknown_owner_tid = 0x12346;
    assert_ne!(ctx.thread.tid.0, unknown_owner_tid);
    let uaddr = map_user_futex_word(&ctx, unknown_owner_tid);

    let try_req = SyscallRequest::new(NR_FUTEX, [uaddr, FUTEX_TRYLOCK_PI as u64, 0, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(try_req, &ctx)),
        SyscallResult::Error(E_SRCH)
    );
}

#[test]
fn dispatch_futex_pi_lock_blocks_and_unlock_hands_off_to_waiter() {
    let _setup = futex_setup();
    let (proc_cap, owner_thread) = fresh_proc_thread();
    let waiter_thread = spawn_sibling_thread_for_test(&proc_cap).expect("sibling thread");
    let owner_tid = owner_thread.tid.0;
    let waiter_tid = waiter_thread.tid.0;
    assert_ne!(owner_tid, waiter_tid);

    let owner_ctx = make_ctx(proc_cap.clone(), owner_thread);
    let waiter_ctx = make_ctx(proc_cap, waiter_thread).with_mailbox(Arc::new(TaskMailbox::new()));
    let uaddr = map_user_futex_word(&owner_ctx, 0);

    let lock_req = SyscallRequest::new(NR_FUTEX, [uaddr, FUTEX_LOCK_PI as u64, 0, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(lock_req, &owner_ctx)),
        SyscallResult::Return(0)
    );
    assert_eq!(read_user_futex_word(&owner_ctx, uaddr), owner_tid);

    let waiter_lock_req = SyscallRequest::new(NR_FUTEX, [uaddr, FUTEX_LOCK_PI as u64, 0, 0, 0, 0]);
    let mut waiter_future = dispatch::<ShimsTestPmap>(waiter_lock_req, &waiter_ctx);
    assert!(
        matches!(poll_once(&mut waiter_future), Poll::Pending),
        "contended FUTEX_LOCK_PI must park instead of returning -EAGAIN",
    );
    assert_eq!(
        read_user_futex_word(&owner_ctx, uaddr),
        FUTEX_WAITERS_BIT | owner_tid,
        "contended PI lock should publish FUTEX_WAITERS while the owner still holds the futex",
    );

    let unlock_req = SyscallRequest::new(NR_FUTEX, [uaddr, FUTEX_UNLOCK_PI as u64, 0, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(unlock_req, &owner_ctx)),
        SyscallResult::Return(0)
    );
    assert_eq!(
        read_user_futex_word(&owner_ctx, uaddr) & FUTEX_TID_MASK,
        waiter_tid
    );
    assert_eq!(
        poll_once(&mut waiter_future),
        Poll::Ready(SyscallResult::Return(0))
    );
}

#[test]
fn dispatch_futex_pi_lock_donates_priority_until_unlock_handoff() {
    let _setup = futex_setup();
    install_reactor_priority_for_test();
    let (proc_cap, owner_thread) = fresh_proc_thread();
    let waiter_thread = spawn_sibling_thread_for_test(&proc_cap).expect("sibling thread");
    let owner_tid = owner_thread.tid.0;
    let waiter_tid = waiter_thread.tid.0;
    let owner_task = test_reactor_submit(InitialSchedMeta::fair().userspace_thread());
    let waiter_task = test_reactor_submit(rt_user_meta(40));
    tx_subsystems::thread_runtime::bind_thread_task(&owner_thread, owner_task);
    tx_subsystems::thread_runtime::bind_thread_task(&waiter_thread, waiter_task);

    let owner_ctx = make_ctx(proc_cap.clone(), owner_thread);
    let waiter_ctx = make_ctx(proc_cap, waiter_thread).with_mailbox(Arc::new(TaskMailbox::new()));
    let uaddr = map_user_futex_word(&owner_ctx, 0);

    let lock_req = SyscallRequest::new(NR_FUTEX, [uaddr, FUTEX_LOCK_PI as u64, 0, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(lock_req, &owner_ctx)),
        SyscallResult::Return(0)
    );
    assert_eq!(read_user_futex_word(&owner_ctx, uaddr), owner_tid);
    assert_eq!(test_reactor_effective_priority(owner_task), 0);

    let waiter_lock_req = SyscallRequest::new(NR_FUTEX, [uaddr, FUTEX_LOCK_PI as u64, 0, 0, 0, 0]);
    let mut waiter_future = dispatch::<ShimsTestPmap>(waiter_lock_req, &waiter_ctx);
    assert!(matches!(poll_once(&mut waiter_future), Poll::Pending));
    assert_eq!(
        read_user_futex_word(&owner_ctx, uaddr),
        FUTEX_WAITERS_BIT | owner_tid
    );
    assert_eq!(
        test_reactor_effective_priority(owner_task),
        40,
        "RT waiter should donate its effective priority to the PI owner",
    );

    let unlock_req = SyscallRequest::new(NR_FUTEX, [uaddr, FUTEX_UNLOCK_PI as u64, 0, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(unlock_req, &owner_ctx)),
        SyscallResult::Return(0)
    );
    assert_eq!(
        read_user_futex_word(&owner_ctx, uaddr) & FUTEX_TID_MASK,
        waiter_tid
    );
    assert_eq!(
        poll_once(&mut waiter_future),
        Poll::Ready(SyscallResult::Return(0))
    );
    assert_eq!(
        test_reactor_effective_priority(owner_task),
        0,
        "unlock handoff should revoke the waiter's donation to the old owner",
    );
}

#[test]
fn dispatch_futex_lock_pi2_no_timeout_reuses_pi_handoff() {
    let _setup = futex_setup();
    let (proc_cap, owner_thread) = fresh_proc_thread();
    let waiter_thread = spawn_sibling_thread_for_test(&proc_cap).expect("sibling thread");
    let owner_tid = owner_thread.tid.0;
    let waiter_tid = waiter_thread.tid.0;

    let owner_ctx = make_ctx(proc_cap.clone(), owner_thread);
    let waiter_ctx = make_ctx(proc_cap, waiter_thread).with_mailbox(Arc::new(TaskMailbox::new()));
    let uaddr = map_user_futex_word(&owner_ctx, 0);

    let lock_req = SyscallRequest::new(NR_FUTEX, [uaddr, FUTEX_LOCK_PI as u64, 0, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(lock_req, &owner_ctx)),
        SyscallResult::Return(0)
    );
    assert_eq!(read_user_futex_word(&owner_ctx, uaddr), owner_tid);

    let waiter_lock_pi2_req =
        SyscallRequest::new(NR_FUTEX, [uaddr, FUTEX_LOCK_PI2 as u64, 0, 0, 0, 0]);
    let mut waiter_future = dispatch::<ShimsTestPmap>(waiter_lock_pi2_req, &waiter_ctx);
    assert!(
        matches!(poll_once(&mut waiter_future), Poll::Pending),
        "contended FUTEX_LOCK_PI2 without timeout should block like FUTEX_LOCK_PI",
    );

    let unlock_req = SyscallRequest::new(NR_FUTEX, [uaddr, FUTEX_UNLOCK_PI as u64, 0, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(unlock_req, &owner_ctx)),
        SyscallResult::Return(0)
    );
    assert_eq!(
        read_user_futex_word(&owner_ctx, uaddr) & FUTEX_TID_MASK,
        waiter_tid
    );
    assert_eq!(
        poll_once(&mut waiter_future),
        Poll::Ready(SyscallResult::Return(0))
    );
}

#[test]
fn dispatch_futex_lock_pi2_zero_timeout_on_contended_lock_returns_neg_etimedout() {
    let _setup = futex_setup();
    let (proc_cap, owner_thread) = fresh_proc_thread();
    let waiter_thread = spawn_sibling_thread_for_test(&proc_cap).expect("sibling thread");

    let owner_ctx = make_ctx(proc_cap.clone(), owner_thread);
    let waiter_ctx = make_ctx(proc_cap, waiter_thread).with_mailbox(Arc::new(TaskMailbox::new()));
    let uaddr = map_user_futex_word(&owner_ctx, 0);

    let lock_req = SyscallRequest::new(NR_FUTEX, [uaddr, FUTEX_LOCK_PI as u64, 0, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(lock_req, &owner_ctx)),
        SyscallResult::Return(0)
    );

    let timeout = map_user_timespec(
        &waiter_ctx,
        TestTimespec {
            tv_sec: 0,
            tv_nsec: 0,
        },
    );
    let lock_pi2_req =
        SyscallRequest::new(NR_FUTEX, [uaddr, FUTEX_LOCK_PI2 as u64, 0, timeout, 0, 0]);

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(lock_pi2_req, &waiter_ctx)),
        SyscallResult::Error(E_TIMEDOUT)
    );
}

#[test]
fn dispatch_futex_pi_requeue_constants_are_linux_abi() {
    let _setup = futex_setup();

    assert_eq!(FUTEX_WAIT_REQUEUE_PI, 11);
    assert_eq!(FUTEX_CMP_REQUEUE_PI, 12);
    assert_eq!(FUTEX_LOCK_PI2, 13);
}

#[test]
fn dispatch_futex_wait_requeue_pi_mismatch_returns_neg_eagain() {
    let _setup = futex_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);
    let source = map_user_futex_word_at(&ctx, 0x5100_0000, 0x1234);
    let target = map_user_futex_word_at(&ctx, 0x5100_1000, 0);

    let req = SyscallRequest::new(
        NR_FUTEX,
        [source, FUTEX_WAIT_REQUEUE_PI as u64, 0x5678, 0, target, 0],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(req, &ctx)),
        SyscallResult::Error(E_AGAIN)
    );
}

#[test]
fn dispatch_futex_cmp_requeue_pi_rejects_wake_count_other_than_one() {
    let _setup = futex_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);
    let source = map_user_futex_word_at(&ctx, 0x5100_0000, 0x1234);
    let target = map_user_futex_word_at(&ctx, 0x5100_1000, 0);

    let req = SyscallRequest::new(
        NR_FUTEX,
        [source, FUTEX_CMP_REQUEUE_PI as u64, 2, 1, target, 0x1234],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(req, &ctx)),
        SyscallResult::Error(E_INVAL)
    );
}

#[test]
fn dispatch_futex_cmp_requeue_pi_rejects_same_source_and_target() {
    let _setup = futex_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);
    let source = map_user_futex_word(&ctx, 0x1234);

    let req = SyscallRequest::new(
        NR_FUTEX,
        [source, FUTEX_CMP_REQUEUE_PI as u64, 1, 1, source, 0x1234],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(req, &ctx)),
        SyscallResult::Error(E_INVAL)
    );
}

#[test]
fn dispatch_futex_cmp_requeue_pi_compare_mismatch_returns_neg_eagain() {
    let _setup = futex_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);
    let source = map_user_futex_word_at(&ctx, 0x5100_0000, 0x1234);
    let target = map_user_futex_word_at(&ctx, 0x5100_1000, 0);

    let req = SyscallRequest::new(
        NR_FUTEX,
        [source, FUTEX_CMP_REQUEUE_PI as u64, 1, 1, target, 0x5678],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(req, &ctx)),
        SyscallResult::Error(E_AGAIN)
    );
}

#[test]
fn dispatch_futex_cmp_requeue_pi_returns_neg_esrch_for_unknown_target_owner_tid() {
    let _setup = futex_setup();
    let (proc_cap, owner_thread) = fresh_proc_thread();
    let waiter_thread = spawn_sibling_thread_for_test(&proc_cap).expect("sibling thread");
    let owner_ctx = make_ctx(proc_cap.clone(), owner_thread);
    let waiter_ctx = make_ctx(proc_cap, waiter_thread).with_mailbox(Arc::new(TaskMailbox::new()));
    let source = map_user_futex_word_at(&owner_ctx, 0x5100_0000, 0x1234);
    let target = map_user_futex_word_at(&owner_ctx, 0x5100_1000, 0x23456);

    let wait_req = SyscallRequest::new(
        NR_FUTEX,
        [source, FUTEX_WAIT_REQUEUE_PI as u64, 0x1234, 0, target, 0],
    );
    let mut wait_future = dispatch::<ShimsTestPmap>(wait_req, &waiter_ctx);
    assert!(matches!(poll_once(&mut wait_future), Poll::Pending));

    let cmp_req = SyscallRequest::new(
        NR_FUTEX,
        [source, FUTEX_CMP_REQUEUE_PI as u64, 1, 0, target, 0x1234],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(cmp_req, &owner_ctx)),
        SyscallResult::Error(E_SRCH)
    );
    assert!(matches!(poll_once(&mut wait_future), Poll::Pending));

    let wake_req = SyscallRequest::new(NR_FUTEX, [source, FUTEX_WAKE as u64, 1, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(wake_req, &owner_ctx)),
        SyscallResult::Return(1),
        "ESRCH must leave the waiter parked on the source futex",
    );
    assert_eq!(
        poll_once(&mut wait_future),
        Poll::Ready(SyscallResult::Error(E_AGAIN))
    );
}

#[test]
fn dispatch_futex_wait_requeue_pi_source_wake_returns_neg_eagain() {
    let _setup = futex_setup();
    let (proc_cap, owner_thread) = fresh_proc_thread();
    let waiter_thread = spawn_sibling_thread_for_test(&proc_cap).expect("sibling thread");
    let owner_ctx = make_ctx(proc_cap.clone(), owner_thread);
    let waiter_ctx = make_ctx(proc_cap, waiter_thread).with_mailbox(Arc::new(TaskMailbox::new()));
    let source = map_user_futex_word_at(&owner_ctx, 0x5100_0000, 0x1234);
    let target = map_user_futex_word_at(&owner_ctx, 0x5100_1000, 0);

    let wait_req = SyscallRequest::new(
        NR_FUTEX,
        [source, FUTEX_WAIT_REQUEUE_PI as u64, 0x1234, 0, target, 0],
    );
    let mut wait_future = dispatch::<ShimsTestPmap>(wait_req, &waiter_ctx);
    assert!(
        matches!(poll_once(&mut wait_future), Poll::Pending),
        "WAIT_REQUEUE_PI should park while source word still matches",
    );

    let wake_req = SyscallRequest::new(NR_FUTEX, [source, FUTEX_WAKE as u64, 1, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(wake_req, &owner_ctx)),
        SyscallResult::Return(1)
    );
    assert_eq!(
        poll_once(&mut wait_future),
        Poll::Ready(SyscallResult::Error(E_AGAIN))
    );
}

#[test]
fn dispatch_futex_cmp_requeue_pi_uncontended_target_acquires_for_waiter() {
    let _setup = futex_setup();
    let (proc_cap, owner_thread) = fresh_proc_thread();
    let waiter_thread = spawn_sibling_thread_for_test(&proc_cap).expect("sibling thread");
    let waiter_tid = waiter_thread.tid.0;
    let owner_ctx = make_ctx(proc_cap.clone(), owner_thread);
    let waiter_ctx = make_ctx(proc_cap, waiter_thread).with_mailbox(Arc::new(TaskMailbox::new()));
    let source = map_user_futex_word_at(&owner_ctx, 0x5100_0000, 0x1234);
    let target = map_user_futex_word_at(&owner_ctx, 0x5100_1000, 0);

    let wait_req = SyscallRequest::new(
        NR_FUTEX,
        [source, FUTEX_WAIT_REQUEUE_PI as u64, 0x1234, 0, target, 0],
    );
    let mut wait_future = dispatch::<ShimsTestPmap>(wait_req, &waiter_ctx);
    assert!(matches!(poll_once(&mut wait_future), Poll::Pending));

    let cmp_req = SyscallRequest::new(
        NR_FUTEX,
        [source, FUTEX_CMP_REQUEUE_PI as u64, 1, 0, target, 0x1234],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(cmp_req, &owner_ctx)),
        SyscallResult::Return(1)
    );
    assert_eq!(
        read_user_futex_word(&owner_ctx, target) & FUTEX_TID_MASK,
        waiter_tid
    );
    assert_eq!(
        poll_once(&mut wait_future),
        Poll::Ready(SyscallResult::Return(0))
    );
}

#[test]
fn dispatch_futex_cmp_requeue_pi_contended_target_hands_off_on_unlock() {
    let _setup = futex_setup();
    let (proc_cap, owner_thread) = fresh_proc_thread();
    let waiter_thread = spawn_sibling_thread_for_test(&proc_cap).expect("sibling thread");
    let owner_tid = owner_thread.tid.0;
    let waiter_tid = waiter_thread.tid.0;
    let owner_ctx = make_ctx(proc_cap.clone(), owner_thread);
    let waiter_ctx = make_ctx(proc_cap, waiter_thread).with_mailbox(Arc::new(TaskMailbox::new()));
    let source = map_user_futex_word_at(&owner_ctx, 0x5100_0000, 0x1234);
    let target = map_user_futex_word_at(&owner_ctx, 0x5100_1000, 0);

    let lock_req = SyscallRequest::new(NR_FUTEX, [target, FUTEX_LOCK_PI as u64, 0, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(lock_req, &owner_ctx)),
        SyscallResult::Return(0)
    );

    let wait_req = SyscallRequest::new(
        NR_FUTEX,
        [source, FUTEX_WAIT_REQUEUE_PI as u64, 0x1234, 0, target, 0],
    );
    let mut wait_future = dispatch::<ShimsTestPmap>(wait_req, &waiter_ctx);
    assert!(matches!(poll_once(&mut wait_future), Poll::Pending));

    let cmp_req = SyscallRequest::new(
        NR_FUTEX,
        [source, FUTEX_CMP_REQUEUE_PI as u64, 1, 0, target, 0x1234],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(cmp_req, &owner_ctx)),
        SyscallResult::Return(1)
    );
    assert_eq!(
        read_user_futex_word(&owner_ctx, target),
        FUTEX_WAITERS_BIT | owner_tid,
        "requeue to a contended PI target should mark FUTEX_WAITERS",
    );
    assert!(
        matches!(poll_once(&mut wait_future), Poll::Pending),
        "waiter remains parked until the PI owner unlocks",
    );

    let unlock_req = SyscallRequest::new(NR_FUTEX, [target, FUTEX_UNLOCK_PI as u64, 0, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(unlock_req, &owner_ctx)),
        SyscallResult::Return(0)
    );
    assert_eq!(
        read_user_futex_word(&owner_ctx, target) & FUTEX_TID_MASK,
        waiter_tid
    );
    assert_eq!(
        poll_once(&mut wait_future),
        Poll::Ready(SyscallResult::Return(0))
    );
}

#[test]
fn dispatch_futex_cmp_requeue_pi_donates_priority_to_contended_owner() {
    let _setup = futex_setup();
    install_reactor_priority_for_test();
    let (proc_cap, owner_thread) = fresh_proc_thread();
    let waiter_thread = spawn_sibling_thread_for_test(&proc_cap).expect("sibling thread");
    let waiter_tid = waiter_thread.tid.0;
    let owner_task = test_reactor_submit(InitialSchedMeta::fair().userspace_thread());
    let waiter_task = test_reactor_submit(rt_user_meta(55));
    tx_subsystems::thread_runtime::bind_thread_task(&owner_thread, owner_task);
    tx_subsystems::thread_runtime::bind_thread_task(&waiter_thread, waiter_task);

    let owner_ctx = make_ctx(proc_cap.clone(), owner_thread);
    let waiter_ctx = make_ctx(proc_cap, waiter_thread).with_mailbox(Arc::new(TaskMailbox::new()));
    let source = map_user_futex_word_at(&owner_ctx, 0x5100_0000, 0x1234);
    let target = map_user_futex_word_at(&owner_ctx, 0x5100_1000, 0);

    let lock_req = SyscallRequest::new(NR_FUTEX, [target, FUTEX_LOCK_PI as u64, 0, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(lock_req, &owner_ctx)),
        SyscallResult::Return(0)
    );
    assert_eq!(test_reactor_effective_priority(owner_task), 0);

    let wait_req = SyscallRequest::new(
        NR_FUTEX,
        [source, FUTEX_WAIT_REQUEUE_PI as u64, 0x1234, 0, target, 0],
    );
    let mut wait_future = dispatch::<ShimsTestPmap>(wait_req, &waiter_ctx);
    assert!(matches!(poll_once(&mut wait_future), Poll::Pending));

    let cmp_req = SyscallRequest::new(
        NR_FUTEX,
        [source, FUTEX_CMP_REQUEUE_PI as u64, 1, 0, target, 0x1234],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(cmp_req, &owner_ctx)),
        SyscallResult::Return(1)
    );
    assert_eq!(
        test_reactor_effective_priority(owner_task),
        55,
        "PI requeue to a contended target should donate to the current owner",
    );
    assert!(
        matches!(poll_once(&mut wait_future), Poll::Pending),
        "waiter remains parked behind the PI target owner",
    );

    let unlock_req = SyscallRequest::new(NR_FUTEX, [target, FUTEX_UNLOCK_PI as u64, 0, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(unlock_req, &owner_ctx)),
        SyscallResult::Return(0)
    );
    assert_eq!(
        read_user_futex_word(&owner_ctx, target) & FUTEX_TID_MASK,
        waiter_tid
    );
    assert_eq!(
        poll_once(&mut wait_future),
        Poll::Ready(SyscallResult::Return(0))
    );
    assert_eq!(
        test_reactor_effective_priority(owner_task),
        0,
        "handoff should revoke the requeued waiter's donation",
    );
}

#[test]
fn dispatch_futex_cmp_requeue_pi_propagates_priority_through_blocked_target_owner() {
    let _setup = futex_setup();
    install_reactor_priority_for_test();
    let (proc_cap, top_owner_thread) = fresh_proc_thread();
    let middle_thread = spawn_sibling_thread_for_test(&proc_cap).expect("middle sibling");
    let high_waiter_thread = spawn_sibling_thread_for_test(&proc_cap).expect("high sibling");
    let top_task = test_reactor_submit(InitialSchedMeta::fair().userspace_thread());
    let middle_task = test_reactor_submit(InitialSchedMeta::fair().userspace_thread());
    let high_task = test_reactor_submit(rt_user_meta(85));
    tx_subsystems::thread_runtime::bind_thread_task(&top_owner_thread, top_task);
    tx_subsystems::thread_runtime::bind_thread_task(&middle_thread, middle_task);
    tx_subsystems::thread_runtime::bind_thread_task(&high_waiter_thread, high_task);

    let top_ctx = make_ctx(proc_cap.clone(), top_owner_thread);
    let middle_ctx =
        make_ctx(proc_cap.clone(), middle_thread).with_mailbox(Arc::new(TaskMailbox::new()));
    let high_ctx =
        make_ctx(proc_cap, high_waiter_thread).with_mailbox(Arc::new(TaskMailbox::new()));
    let top_futex = map_user_futex_word_at(&top_ctx, 0x5130_0000, 0);
    let target_futex = map_user_futex_word_at(&top_ctx, 0x5130_1000, 0);
    let source_futex = map_user_futex_word_at(&top_ctx, 0x5130_2000, 0x1234);

    let lock_top_req = SyscallRequest::new(NR_FUTEX, [top_futex, FUTEX_LOCK_PI as u64, 0, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(lock_top_req, &top_ctx)),
        SyscallResult::Return(0)
    );
    let lock_target_req =
        SyscallRequest::new(NR_FUTEX, [target_futex, FUTEX_LOCK_PI as u64, 0, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(lock_target_req, &middle_ctx)),
        SyscallResult::Return(0)
    );

    let middle_wait_req =
        SyscallRequest::new(NR_FUTEX, [top_futex, FUTEX_LOCK_PI as u64, 0, 0, 0, 0]);
    let mut middle_future = dispatch::<ShimsTestPmap>(middle_wait_req, &middle_ctx);
    assert!(matches!(poll_once(&mut middle_future), Poll::Pending));

    let wait_req = SyscallRequest::new(
        NR_FUTEX,
        [
            source_futex,
            FUTEX_WAIT_REQUEUE_PI as u64,
            0x1234,
            0,
            target_futex,
            0,
        ],
    );
    let mut high_future = dispatch::<ShimsTestPmap>(wait_req, &high_ctx);
    assert!(matches!(poll_once(&mut high_future), Poll::Pending));

    let cmp_req = SyscallRequest::new(
        NR_FUTEX,
        [
            source_futex,
            FUTEX_CMP_REQUEUE_PI as u64,
            1,
            0,
            target_futex,
            0x1234,
        ],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(cmp_req, &top_ctx)),
        SyscallResult::Return(1)
    );
    assert_eq!(test_reactor_effective_priority(middle_task), 85);
    assert_eq!(
        test_reactor_effective_priority(top_task),
        85,
        "requeued boost inherited by a blocked target owner should propagate",
    );
    assert!(matches!(poll_once(&mut middle_future), Poll::Pending));
    assert!(matches!(poll_once(&mut high_future), Poll::Pending));
}

#[test]
fn dispatch_futex_cmp_requeue_pi_cycle_returns_neg_edeadlk_and_preserves_source_waiter() {
    let _setup = futex_setup();
    install_reactor_priority_for_test();
    let (proc_cap, a_thread) = fresh_proc_thread();
    let b_thread = spawn_sibling_thread_for_test(&proc_cap).expect("b sibling");
    let requeuer_thread = spawn_sibling_thread_for_test(&proc_cap).expect("requeuer sibling");
    let a_tid = a_thread.tid.0;
    let b_tid = b_thread.tid.0;
    let a_task = test_reactor_submit(rt_user_meta(65));
    let b_task = test_reactor_submit(rt_user_meta(25));
    tx_subsystems::thread_runtime::bind_thread_task(&a_thread, a_task);
    tx_subsystems::thread_runtime::bind_thread_task(&b_thread, b_task);

    let a_ctx = make_ctx(proc_cap.clone(), a_thread).with_mailbox(Arc::new(TaskMailbox::new()));
    let b_ctx = make_ctx(proc_cap.clone(), b_thread).with_mailbox(Arc::new(TaskMailbox::new()));
    let requeuer_ctx = make_ctx(proc_cap, requeuer_thread);
    let top_futex = map_user_futex_word_at(&a_ctx, 0x5150_0000, 0);
    let target_futex = map_user_futex_word_at(&a_ctx, 0x5150_1000, 0);
    let source_futex = map_user_futex_word_at(&a_ctx, 0x5150_2000, 0x1234);

    let lock_top_req = SyscallRequest::new(NR_FUTEX, [top_futex, FUTEX_LOCK_PI as u64, 0, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(lock_top_req, &a_ctx)),
        SyscallResult::Return(0)
    );
    let lock_target_req =
        SyscallRequest::new(NR_FUTEX, [target_futex, FUTEX_LOCK_PI as u64, 0, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(lock_target_req, &b_ctx)),
        SyscallResult::Return(0)
    );

    let b_waits_on_a_req =
        SyscallRequest::new(NR_FUTEX, [top_futex, FUTEX_LOCK_PI as u64, 0, 0, 0, 0]);
    let mut b_future = dispatch::<ShimsTestPmap>(b_waits_on_a_req, &b_ctx);
    assert!(matches!(poll_once(&mut b_future), Poll::Pending));

    let a_wait_requeue_req = SyscallRequest::new(
        NR_FUTEX,
        [
            source_futex,
            FUTEX_WAIT_REQUEUE_PI as u64,
            0x1234,
            0,
            target_futex,
            0,
        ],
    );
    let mut a_future = dispatch::<ShimsTestPmap>(a_wait_requeue_req, &a_ctx);
    assert!(matches!(poll_once(&mut a_future), Poll::Pending));

    let cmp_req = SyscallRequest::new(
        NR_FUTEX,
        [
            source_futex,
            FUTEX_CMP_REQUEUE_PI as u64,
            1,
            0,
            target_futex,
            0x1234,
        ],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(cmp_req, &requeuer_ctx)),
        SyscallResult::Error(E_DEADLK),
        "requeueing A behind B would close the B -> A PI owner chain",
    );
    assert_eq!(
        read_user_futex_word(&a_ctx, target_futex),
        b_tid,
        "failed requeue must not set FUTEX_WAITERS on the target",
    );

    let wake_req = SyscallRequest::new(NR_FUTEX, [source_futex, FUTEX_WAKE as u64, 1, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(wake_req, &requeuer_ctx)),
        SyscallResult::Return(1),
        "source waiter must remain parked after failed CMP_REQUEUE_PI",
    );
    assert_eq!(
        poll_once(&mut a_future),
        Poll::Ready(SyscallResult::Error(E_AGAIN)),
        "ordinary source wake should still resume WAIT_REQUEUE_PI with EAGAIN",
    );
    assert!(matches!(poll_once(&mut b_future), Poll::Pending));
    assert_eq!(
        read_user_futex_word(&a_ctx, top_futex),
        FUTEX_WAITERS_BIT | a_tid
    );
}

#[test]
fn dispatch_futex_cmp_requeue_pi_moves_one_plus_requeue_count_waiters() {
    let _setup = futex_setup();
    let (proc_cap, owner_thread) = fresh_proc_thread();
    let first_waiter = spawn_sibling_thread_for_test(&proc_cap).expect("first sibling");
    let second_waiter = spawn_sibling_thread_for_test(&proc_cap).expect("second sibling");
    let owner_tid = owner_thread.tid.0;
    let first_tid = first_waiter.tid.0;
    let second_tid = second_waiter.tid.0;
    let owner_ctx = make_ctx(proc_cap.clone(), owner_thread);
    let first_ctx =
        make_ctx(proc_cap.clone(), first_waiter).with_mailbox(Arc::new(TaskMailbox::new()));
    let second_ctx = make_ctx(proc_cap, second_waiter).with_mailbox(Arc::new(TaskMailbox::new()));
    let source = map_user_futex_word_at(&owner_ctx, 0x5100_0000, 0x1234);
    let target = map_user_futex_word_at(&owner_ctx, 0x5100_1000, 0);

    let lock_req = SyscallRequest::new(NR_FUTEX, [target, FUTEX_LOCK_PI as u64, 0, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(lock_req, &owner_ctx)),
        SyscallResult::Return(0)
    );

    let wait_req = SyscallRequest::new(
        NR_FUTEX,
        [source, FUTEX_WAIT_REQUEUE_PI as u64, 0x1234, 0, target, 0],
    );
    let mut first_future = dispatch::<ShimsTestPmap>(wait_req, &first_ctx);
    assert!(matches!(poll_once(&mut first_future), Poll::Pending));
    let wait_req = SyscallRequest::new(
        NR_FUTEX,
        [source, FUTEX_WAIT_REQUEUE_PI as u64, 0x1234, 0, target, 0],
    );
    let mut second_future = dispatch::<ShimsTestPmap>(wait_req, &second_ctx);
    assert!(matches!(poll_once(&mut second_future), Poll::Pending));

    let cmp_req = SyscallRequest::new(
        NR_FUTEX,
        [source, FUTEX_CMP_REQUEUE_PI as u64, 1, 1, target, 0x1234],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(cmp_req, &owner_ctx)),
        SyscallResult::Return(2)
    );
    assert_eq!(
        read_user_futex_word(&owner_ctx, target),
        FUTEX_WAITERS_BIT | owner_tid
    );

    let unlock_req = SyscallRequest::new(NR_FUTEX, [target, FUTEX_UNLOCK_PI as u64, 0, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(unlock_req, &owner_ctx)),
        SyscallResult::Return(0)
    );
    assert_eq!(
        read_user_futex_word(&owner_ctx, target),
        FUTEX_WAITERS_BIT | first_tid
    );
    assert_eq!(
        poll_once(&mut first_future),
        Poll::Ready(SyscallResult::Return(0))
    );
    assert!(matches!(poll_once(&mut second_future), Poll::Pending));

    let unlock_req = SyscallRequest::new(NR_FUTEX, [target, FUTEX_UNLOCK_PI as u64, 0, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(unlock_req, &first_ctx)),
        SyscallResult::Return(0)
    );
    assert_eq!(
        read_user_futex_word(&owner_ctx, target) & FUTEX_TID_MASK,
        second_tid
    );
    assert_eq!(
        poll_once(&mut second_future),
        Poll::Ready(SyscallResult::Return(0))
    );
}

#[test]
fn dispatch_futex_unlock_pi_retargets_remaining_requeue_donation_to_new_owner() {
    let _setup = futex_setup();
    install_reactor_priority_for_test();
    let (proc_cap, owner_thread) = fresh_proc_thread();
    let first_waiter = spawn_sibling_thread_for_test(&proc_cap).expect("first sibling");
    let second_waiter = spawn_sibling_thread_for_test(&proc_cap).expect("second sibling");
    let first_tid = first_waiter.tid.0;
    let second_tid = second_waiter.tid.0;
    let owner_task = test_reactor_submit(InitialSchedMeta::fair().userspace_thread());
    let first_task = test_reactor_submit(rt_user_meta(25));
    let second_task = test_reactor_submit(rt_user_meta(70));
    tx_subsystems::thread_runtime::bind_thread_task(&owner_thread, owner_task);
    tx_subsystems::thread_runtime::bind_thread_task(&first_waiter, first_task);
    tx_subsystems::thread_runtime::bind_thread_task(&second_waiter, second_task);

    let owner_ctx = make_ctx(proc_cap.clone(), owner_thread);
    let first_ctx =
        make_ctx(proc_cap.clone(), first_waiter).with_mailbox(Arc::new(TaskMailbox::new()));
    let second_ctx = make_ctx(proc_cap, second_waiter).with_mailbox(Arc::new(TaskMailbox::new()));
    let source = map_user_futex_word_at(&owner_ctx, 0x5110_0000, 0x1234);
    let target = map_user_futex_word_at(&owner_ctx, 0x5110_1000, 0);

    let lock_req = SyscallRequest::new(NR_FUTEX, [target, FUTEX_LOCK_PI as u64, 0, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(lock_req, &owner_ctx)),
        SyscallResult::Return(0)
    );

    let first_wait_req = SyscallRequest::new(
        NR_FUTEX,
        [source, FUTEX_WAIT_REQUEUE_PI as u64, 0x1234, 0, target, 0],
    );
    let mut first_future = dispatch::<ShimsTestPmap>(first_wait_req, &first_ctx);
    assert!(matches!(poll_once(&mut first_future), Poll::Pending));
    let second_wait_req = SyscallRequest::new(
        NR_FUTEX,
        [source, FUTEX_WAIT_REQUEUE_PI as u64, 0x1234, 0, target, 0],
    );
    let mut second_future = dispatch::<ShimsTestPmap>(second_wait_req, &second_ctx);
    assert!(matches!(poll_once(&mut second_future), Poll::Pending));

    let cmp_req = SyscallRequest::new(
        NR_FUTEX,
        [source, FUTEX_CMP_REQUEUE_PI as u64, 1, 1, target, 0x1234],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(cmp_req, &owner_ctx)),
        SyscallResult::Return(2)
    );
    assert_eq!(
        test_reactor_effective_priority(owner_task),
        70,
        "current owner should inherit the highest queued PI waiter",
    );

    let unlock_req = SyscallRequest::new(NR_FUTEX, [target, FUTEX_UNLOCK_PI as u64, 0, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(unlock_req, &owner_ctx)),
        SyscallResult::Return(0)
    );
    assert_eq!(
        read_user_futex_word(&owner_ctx, target) & FUTEX_TID_MASK,
        second_tid
    );
    assert!(matches!(poll_once(&mut first_future), Poll::Pending));
    assert_eq!(
        poll_once(&mut second_future),
        Poll::Ready(SyscallResult::Return(0))
    );
    assert_eq!(
        test_reactor_effective_priority(owner_task),
        0,
        "old owner donation should be fully revoked after handoff",
    );
    assert_eq!(
        test_reactor_effective_priority(second_task),
        25,
        "remaining waiter should donate to the highest-priority new owner after handoff",
    );

    let second_unlock_ctx = make_ctx(second_ctx.process.clone(), second_ctx.thread.clone());
    let unlock_req = SyscallRequest::new(NR_FUTEX, [target, FUTEX_UNLOCK_PI as u64, 0, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(unlock_req, &second_unlock_ctx)),
        SyscallResult::Return(0)
    );
    assert_eq!(
        read_user_futex_word(&owner_ctx, target) & FUTEX_TID_MASK,
        first_tid
    );
    assert_eq!(
        poll_once(&mut first_future),
        Poll::Ready(SyscallResult::Return(0))
    );
    assert_eq!(
        test_reactor_effective_priority(second_task),
        70,
        "second handoff should revoke the remaining donation and restore base RT priority",
    );
}

#[test]
fn dispatch_futex_unlock_pi_hands_off_to_highest_priority_waiter() {
    let _setup = futex_setup();
    install_reactor_priority_for_test();
    let (proc_cap, owner_thread) = fresh_proc_thread();
    let low_waiter = spawn_sibling_thread_for_test(&proc_cap).expect("low sibling");
    let high_waiter = spawn_sibling_thread_for_test(&proc_cap).expect("high sibling");
    let owner_tid = owner_thread.tid.0;
    let high_tid = high_waiter.tid.0;
    let owner_task = test_reactor_submit(InitialSchedMeta::fair().userspace_thread());
    let low_task = test_reactor_submit(rt_user_meta(20));
    let high_task = test_reactor_submit(rt_user_meta(90));
    tx_subsystems::thread_runtime::bind_thread_task(&owner_thread, owner_task);
    tx_subsystems::thread_runtime::bind_thread_task(&low_waiter, low_task);
    tx_subsystems::thread_runtime::bind_thread_task(&high_waiter, high_task);

    let owner_ctx = make_ctx(proc_cap.clone(), owner_thread);
    let low_ctx = make_ctx(proc_cap.clone(), low_waiter).with_mailbox(Arc::new(TaskMailbox::new()));
    let high_ctx = make_ctx(proc_cap, high_waiter).with_mailbox(Arc::new(TaskMailbox::new()));
    let futex = map_user_futex_word_at(&owner_ctx, 0x5128_0000, 0);

    let lock_req = SyscallRequest::new(NR_FUTEX, [futex, FUTEX_LOCK_PI as u64, 0, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(lock_req, &owner_ctx)),
        SyscallResult::Return(0)
    );

    let low_req = SyscallRequest::new(NR_FUTEX, [futex, FUTEX_LOCK_PI as u64, 0, 0, 0, 0]);
    let mut low_future = dispatch::<ShimsTestPmap>(low_req, &low_ctx);
    assert!(matches!(poll_once(&mut low_future), Poll::Pending));
    let high_req = SyscallRequest::new(NR_FUTEX, [futex, FUTEX_LOCK_PI as u64, 0, 0, 0, 0]);
    let mut high_future = dispatch::<ShimsTestPmap>(high_req, &high_ctx);
    assert!(matches!(poll_once(&mut high_future), Poll::Pending));
    assert_eq!(test_reactor_effective_priority(owner_task), 90);
    assert_eq!(
        read_user_futex_word(&owner_ctx, futex),
        FUTEX_WAITERS_BIT | owner_tid
    );

    let unlock_req = SyscallRequest::new(NR_FUTEX, [futex, FUTEX_UNLOCK_PI as u64, 0, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(unlock_req, &owner_ctx)),
        SyscallResult::Return(0)
    );
    assert_eq!(
        read_user_futex_word(&owner_ctx, futex) & FUTEX_TID_MASK,
        high_tid,
        "PI unlock should hand off to the highest-priority waiter, not FIFO order",
    );
    assert!(matches!(poll_once(&mut low_future), Poll::Pending));
    assert_eq!(
        poll_once(&mut high_future),
        Poll::Ready(SyscallResult::Return(0))
    );
}

#[test]
fn dispatch_futex_pi_lock_propagates_priority_through_blocked_owner_chain() {
    let _setup = futex_setup();
    install_reactor_priority_for_test();
    let (proc_cap, top_owner_thread) = fresh_proc_thread();
    let middle_thread = spawn_sibling_thread_for_test(&proc_cap).expect("middle sibling");
    let high_waiter_thread = spawn_sibling_thread_for_test(&proc_cap).expect("high sibling");
    let top_task = test_reactor_submit(InitialSchedMeta::fair().userspace_thread());
    let middle_task = test_reactor_submit(InitialSchedMeta::fair().userspace_thread());
    let high_task = test_reactor_submit(rt_user_meta(80));
    tx_subsystems::thread_runtime::bind_thread_task(&top_owner_thread, top_task);
    tx_subsystems::thread_runtime::bind_thread_task(&middle_thread, middle_task);
    tx_subsystems::thread_runtime::bind_thread_task(&high_waiter_thread, high_task);

    let top_ctx = make_ctx(proc_cap.clone(), top_owner_thread);
    let middle_ctx =
        make_ctx(proc_cap.clone(), middle_thread).with_mailbox(Arc::new(TaskMailbox::new()));
    let high_ctx =
        make_ctx(proc_cap, high_waiter_thread).with_mailbox(Arc::new(TaskMailbox::new()));
    let top_futex = map_user_futex_word_at(&top_ctx, 0x5120_0000, 0);
    let middle_futex = map_user_futex_word_at(&top_ctx, 0x5120_1000, 0);

    let lock_top_req = SyscallRequest::new(NR_FUTEX, [top_futex, FUTEX_LOCK_PI as u64, 0, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(lock_top_req, &top_ctx)),
        SyscallResult::Return(0)
    );
    let lock_middle_req =
        SyscallRequest::new(NR_FUTEX, [middle_futex, FUTEX_LOCK_PI as u64, 0, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(lock_middle_req, &middle_ctx)),
        SyscallResult::Return(0)
    );

    let middle_wait_req =
        SyscallRequest::new(NR_FUTEX, [top_futex, FUTEX_LOCK_PI as u64, 0, 0, 0, 0]);
    let mut middle_future = dispatch::<ShimsTestPmap>(middle_wait_req, &middle_ctx);
    assert!(matches!(poll_once(&mut middle_future), Poll::Pending));
    assert_eq!(
        test_reactor_effective_priority(top_task),
        0,
        "a fair blocked owner should not donate until it inherits priority",
    );

    let high_wait_req =
        SyscallRequest::new(NR_FUTEX, [middle_futex, FUTEX_LOCK_PI as u64, 0, 0, 0, 0]);
    let mut high_future = dispatch::<ShimsTestPmap>(high_wait_req, &high_ctx);
    assert!(matches!(poll_once(&mut high_future), Poll::Pending));
    assert_eq!(test_reactor_effective_priority(middle_task), 80);
    assert_eq!(
        test_reactor_effective_priority(top_task),
        80,
        "boost inherited by a blocked owner should propagate to the next PI owner",
    );
    assert!(matches!(poll_once(&mut middle_future), Poll::Pending));
    assert!(matches!(poll_once(&mut high_future), Poll::Pending));
}

#[test]
fn dispatch_futex_pi_lock_owner_chain_cycle_returns_neg_edeadlk() {
    let _setup = futex_setup();
    install_reactor_priority_for_test();
    let (proc_cap, a_thread) = fresh_proc_thread();
    let b_thread = spawn_sibling_thread_for_test(&proc_cap).expect("b sibling");
    let a_tid = a_thread.tid.0;
    let b_tid = b_thread.tid.0;
    let a_task = test_reactor_submit(rt_user_meta(30));
    let b_task = test_reactor_submit(rt_user_meta(40));
    tx_subsystems::thread_runtime::bind_thread_task(&a_thread, a_task);
    tx_subsystems::thread_runtime::bind_thread_task(&b_thread, b_task);

    let a_ctx = make_ctx(proc_cap.clone(), a_thread).with_mailbox(Arc::new(TaskMailbox::new()));
    let b_ctx = make_ctx(proc_cap, b_thread).with_mailbox(Arc::new(TaskMailbox::new()));
    let futex_a = map_user_futex_word_at(&a_ctx, 0x5140_0000, 0);
    let futex_b = map_user_futex_word_at(&a_ctx, 0x5140_1000, 0);

    let lock_a_req = SyscallRequest::new(NR_FUTEX, [futex_a, FUTEX_LOCK_PI as u64, 0, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(lock_a_req, &a_ctx)),
        SyscallResult::Return(0)
    );
    let lock_b_req = SyscallRequest::new(NR_FUTEX, [futex_b, FUTEX_LOCK_PI as u64, 0, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(lock_b_req, &b_ctx)),
        SyscallResult::Return(0)
    );

    let b_waits_on_a_req =
        SyscallRequest::new(NR_FUTEX, [futex_a, FUTEX_LOCK_PI as u64, 0, 0, 0, 0]);
    let mut b_future = dispatch::<ShimsTestPmap>(b_waits_on_a_req, &b_ctx);
    assert!(matches!(poll_once(&mut b_future), Poll::Pending));
    assert_eq!(
        read_user_futex_word(&a_ctx, futex_a),
        FUTEX_WAITERS_BIT | a_tid
    );

    let a_waits_on_b_req =
        SyscallRequest::new(NR_FUTEX, [futex_b, FUTEX_LOCK_PI as u64, 0, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(a_waits_on_b_req, &a_ctx)),
        SyscallResult::Error(E_DEADLK),
        "A -> B would close the B -> A PI owner-chain cycle",
    );
    assert_eq!(
        read_user_futex_word(&a_ctx, futex_b),
        b_tid,
        "failed cycle check must not set FUTEX_WAITERS or enqueue A on B",
    );

    let unlock_b_req = SyscallRequest::new(NR_FUTEX, [futex_b, FUTEX_UNLOCK_PI as u64, 0, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(unlock_b_req, &b_ctx)),
        SyscallResult::Return(0),
        "B should still unlock with no bogus A waiter after EDEADLK",
    );
    assert_eq!(read_user_futex_word(&a_ctx, futex_b), 0);
    assert!(matches!(poll_once(&mut b_future), Poll::Pending));
}

#[test]
fn dispatch_futex_nested_pi_timeout_or_cancel_deboosts_owner_chain() {
    let _setup = futex_setup();
    install_reactor_priority_for_test();
    let (proc_cap, top_owner_thread) = fresh_proc_thread();
    let middle_thread = spawn_sibling_thread_for_test(&proc_cap).expect("middle sibling");
    let high_thread = spawn_sibling_thread_for_test(&proc_cap).expect("high sibling");
    let high_tid = high_thread.tid.0;
    let top_task = test_reactor_submit(InitialSchedMeta::fair().userspace_thread());
    let middle_task = test_reactor_submit(InitialSchedMeta::fair().userspace_thread());
    let high_task = test_reactor_submit(rt_user_meta(80));
    tx_subsystems::thread_runtime::bind_thread_task(&top_owner_thread, top_task);
    tx_subsystems::thread_runtime::bind_thread_task(&middle_thread, middle_task);
    tx_subsystems::thread_runtime::bind_thread_task(&high_thread, high_task);

    let top_ctx = make_ctx(proc_cap.clone(), top_owner_thread);
    let middle_ctx =
        make_ctx(proc_cap.clone(), middle_thread).with_mailbox(Arc::new(TaskMailbox::new()));
    let high_ctx = make_ctx(proc_cap, high_thread).with_mailbox(Arc::new(TaskMailbox::new()));
    let top_futex = map_user_futex_word_at(&top_ctx, 0x5160_0000, 0);
    let middle_futex = map_user_futex_word_at(&top_ctx, 0x5160_1000, 0);

    let lock_top_req = SyscallRequest::new(NR_FUTEX, [top_futex, FUTEX_LOCK_PI as u64, 0, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(lock_top_req, &top_ctx)),
        SyscallResult::Return(0)
    );
    let lock_middle_req =
        SyscallRequest::new(NR_FUTEX, [middle_futex, FUTEX_LOCK_PI as u64, 0, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(lock_middle_req, &middle_ctx)),
        SyscallResult::Return(0)
    );

    let middle_wait_req =
        SyscallRequest::new(NR_FUTEX, [top_futex, FUTEX_LOCK_PI as u64, 0, 0, 0, 0]);
    let mut middle_future = dispatch::<ShimsTestPmap>(middle_wait_req, &middle_ctx);
    assert!(matches!(poll_once(&mut middle_future), Poll::Pending));
    assert_eq!(test_reactor_effective_priority(top_task), 0);

    let mut high_op = tx_subsystems::futex::FutexPiLockOp {
        uaddr: middle_futex,
        aspace: &high_ctx.aspace,
        owner_tid: high_tid,
        acquired: false,
        waiting: false,
        waiting_source_id: None,
    };
    let mut script_ctx = tx_subsystems::futex::adapter::step_engine::ScriptCtx::<
        tx_subsystems::futex::adapter::step_engine::ProcessIdentity,
    >::new();
    assert!(matches!(
        <tx_subsystems::futex::FutexPiLockOp<'_> as tx_subsystems::futex::adapter::step_engine::StepOp<
            tx_subsystems::futex::adapter::step_engine::ProcessIdentity,
        >>::step(&mut high_op, &mut script_ctx),
        tx_subsystems::futex::adapter::step_engine::StepOutcome::Yield { .. }
    ));
    assert_eq!(test_reactor_effective_priority(middle_task), 80);
    assert_eq!(test_reactor_effective_priority(top_task), 80);

    assert_eq!(
        <tx_subsystems::futex::FutexPiLockOp<'_> as tx_subsystems::futex::adapter::step_engine::StepOp<
            tx_subsystems::futex::adapter::step_engine::ProcessIdentity,
        >>::apply_resume(
            &mut high_op,
            tx_subsystems::futex::adapter::step_engine::ResumeOutcome::Aborted(
                tx_subsystems::futex::adapter::step_engine::AbortReason::Canceled,
            ),
        ),
        Err(tx_subsystems::futex::adapter::step_engine::Errno::EINTR)
    );
    assert_eq!(
        test_reactor_effective_priority(middle_task),
        0,
        "canceling the top donor should deboost the direct owner",
    );
    assert_eq!(
        test_reactor_effective_priority(top_task),
        0,
        "deboost must propagate through the blocked owner chain",
    );
    assert!(matches!(poll_once(&mut middle_future), Poll::Pending));
}

#[test]
fn dispatch_futex_nested_requeue_pi_timeout_or_cancel_deboosts_owner_chain() {
    let _setup = futex_setup();
    install_reactor_priority_for_test();
    let (proc_cap, top_owner_thread) = fresh_proc_thread();
    let middle_thread = spawn_sibling_thread_for_test(&proc_cap).expect("middle sibling");
    let high_thread = spawn_sibling_thread_for_test(&proc_cap).expect("high sibling");
    let high_tid = high_thread.tid.0;
    let top_task = test_reactor_submit(InitialSchedMeta::fair().userspace_thread());
    let middle_task = test_reactor_submit(InitialSchedMeta::fair().userspace_thread());
    let high_task = test_reactor_submit(rt_user_meta(90));
    tx_subsystems::thread_runtime::bind_thread_task(&top_owner_thread, top_task);
    tx_subsystems::thread_runtime::bind_thread_task(&middle_thread, middle_task);
    tx_subsystems::thread_runtime::bind_thread_task(&high_thread, high_task);

    let top_ctx = make_ctx(proc_cap.clone(), top_owner_thread);
    let middle_ctx =
        make_ctx(proc_cap.clone(), middle_thread).with_mailbox(Arc::new(TaskMailbox::new()));
    let high_ctx = make_ctx(proc_cap, high_thread).with_mailbox(Arc::new(TaskMailbox::new()));
    let top_futex = map_user_futex_word_at(&top_ctx, 0x5170_0000, 0);
    let target_futex = map_user_futex_word_at(&top_ctx, 0x5170_1000, 0);
    let source_futex = map_user_futex_word_at(&top_ctx, 0x5170_2000, 0x1234);

    let lock_top_req = SyscallRequest::new(NR_FUTEX, [top_futex, FUTEX_LOCK_PI as u64, 0, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(lock_top_req, &top_ctx)),
        SyscallResult::Return(0)
    );
    let lock_target_req =
        SyscallRequest::new(NR_FUTEX, [target_futex, FUTEX_LOCK_PI as u64, 0, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(lock_target_req, &middle_ctx)),
        SyscallResult::Return(0)
    );

    let middle_wait_req =
        SyscallRequest::new(NR_FUTEX, [top_futex, FUTEX_LOCK_PI as u64, 0, 0, 0, 0]);
    let mut middle_future = dispatch::<ShimsTestPmap>(middle_wait_req, &middle_ctx);
    assert!(matches!(poll_once(&mut middle_future), Poll::Pending));

    let mut high_op = tx_subsystems::futex::FutexWaitRequeuePiOp {
        uaddr: source_futex,
        uaddr2: target_futex,
        val: 0x1234,
        aspace: &high_ctx.aspace,
        waiter_tid: high_tid,
        acquired: false,
        source_woke: false,
        waiting: false,
        waiting_source_id: None,
    };
    let mut script_ctx = tx_subsystems::futex::adapter::step_engine::ScriptCtx::<
        tx_subsystems::futex::adapter::step_engine::ProcessIdentity,
    >::new();
    assert!(matches!(
        <tx_subsystems::futex::FutexWaitRequeuePiOp<'_> as tx_subsystems::futex::adapter::step_engine::StepOp<
            tx_subsystems::futex::adapter::step_engine::ProcessIdentity,
        >>::step(&mut high_op, &mut script_ctx),
        tx_subsystems::futex::adapter::step_engine::StepOutcome::Yield { .. }
    ));

    let cmp_req = SyscallRequest::new(
        NR_FUTEX,
        [
            source_futex,
            FUTEX_CMP_REQUEUE_PI as u64,
            1,
            0,
            target_futex,
            0x1234,
        ],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(cmp_req, &top_ctx)),
        SyscallResult::Return(1)
    );
    assert_eq!(test_reactor_effective_priority(middle_task), 90);
    assert_eq!(test_reactor_effective_priority(top_task), 90);

    assert_eq!(
        <tx_subsystems::futex::FutexWaitRequeuePiOp<'_> as tx_subsystems::futex::adapter::step_engine::StepOp<
            tx_subsystems::futex::adapter::step_engine::ProcessIdentity,
        >>::apply_resume(
            &mut high_op,
            tx_subsystems::futex::adapter::step_engine::ResumeOutcome::Aborted(
                tx_subsystems::futex::adapter::step_engine::AbortReason::TimedOut,
            ),
        ),
        Err(tx_subsystems::futex::adapter::step_engine::Errno::ETIMEDOUT)
    );
    assert_eq!(
        test_reactor_effective_priority(middle_task),
        0,
        "canceling a requeued donor should deboost the target owner",
    );
    assert_eq!(
        test_reactor_effective_priority(top_task),
        0,
        "requeue deboost must propagate through a blocked target owner",
    );
    assert!(matches!(poll_once(&mut middle_future), Poll::Pending));
}

#[test]
fn dispatch_futex2_wait_mismatch_returns_neg_eagain() {
    let _setup = futex_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);
    let uaddr = map_user_futex_word(&ctx, 0x1234);

    let req = SyscallRequest::new(
        NR_FUTEX2_WAIT,
        [uaddr, 0x5678, u32::MAX as u64, FUTEX_32 as u64, 0, 0],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));

    assert_eq!(result, SyscallResult::Error(E_AGAIN));
}

#[test]
fn dispatch_futex2_wait_zero_timeout_returns_neg_etimedout() {
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
        NR_FUTEX2_WAIT,
        [uaddr, 0x1234, u32::MAX as u64, FUTEX_32 as u64, timeout, 0],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));

    assert_eq!(result, SyscallResult::Error(E_TIMEDOUT));
}

#[test]
fn dispatch_futex2_wait_is_woken_by_futex2_wake() {
    let _setup = futex_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);
    let (ctx, _wheel) = ctx_with_mailbox_and_timer(ctx);
    let uaddr = map_user_futex_word(&ctx, 0x1234);
    let wait_req = SyscallRequest::new(NR_FUTEX2_WAIT, [uaddr, 0x1234, 0x8, FUTEX_32 as u64, 0, 0]);

    let waker = Waker::noop().clone();
    let mut cx = Context::from_waker(&waker);
    let fut = dispatch::<ShimsTestPmap>(wait_req, &ctx);
    let mut pinned = Box::pin(fut);
    let first_poll = pinned.as_mut().poll(&mut cx);
    assert!(
        matches!(first_poll, Poll::Pending),
        "futex2_wait with matching futex should park; got {first_poll:?}"
    );

    let miss_req = SyscallRequest::new(NR_FUTEX2_WAKE, [uaddr, 0x4, 1, FUTEX_32 as u64, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(miss_req, &ctx)),
        SyscallResult::Return(0)
    );

    let wake_req = SyscallRequest::new(NR_FUTEX2_WAKE, [uaddr, 0x8, 1, FUTEX_32 as u64, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(wake_req, &ctx)),
        SyscallResult::Return(1)
    );

    let mut last = Poll::Pending;
    for _ in 0..256 {
        last = pinned.as_mut().poll(&mut cx);
        if let Poll::Ready(value) = last {
            assert_eq!(value, SyscallResult::Return(0));
            return;
        }
    }
    panic!("futex2_wait did not resolve after futex2_wake; last poll = {last:?}");
}

#[test]
fn dispatch_futex2_requeue_cmp_mismatch_returns_neg_eagain() {
    let _setup = futex_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);
    let source = map_user_futex_word_at(&ctx, 0x5100_0000, 0x11);
    let target = map_user_futex_word_at(&ctx, 0x5100_2000, 0x22);
    let waiters = [
        TestFutexWaitv {
            val: 0x99,
            uaddr: source,
            flags: FUTEX_32,
            reserved: 0,
        },
        TestFutexWaitv {
            val: 0,
            uaddr: target,
            flags: FUTEX_32,
            reserved: 0,
        },
    ];
    let waiters_uaddr = map_user_waitv_array(&ctx, &waiters);

    let req = SyscallRequest::new(NR_FUTEX2_REQUEUE, [waiters_uaddr, 0, 0, 1, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(req, &ctx)),
        SyscallResult::Error(E_AGAIN)
    );
}

#[test]
fn dispatch_futex2_requeue_moves_waiter_to_target() {
    let _setup = futex_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);
    let (ctx, _wheel) = ctx_with_mailbox_and_timer(ctx);
    let source = map_user_futex_word_at(&ctx, 0x5100_0000, 0x11);
    let target = map_user_futex_word_at(&ctx, 0x5100_2000, 0x22);

    let wait_req = SyscallRequest::new(NR_FUTEX, [source, FUTEX_WAIT as u64, 0x11, 0, 0, 0]);
    let waker = Waker::noop().clone();
    let mut cx = Context::from_waker(&waker);
    let fut = dispatch::<ShimsTestPmap>(wait_req, &ctx);
    let mut pinned = Box::pin(fut);
    assert!(
        matches!(pinned.as_mut().poll(&mut cx), Poll::Pending),
        "source waiter should park before requeue"
    );

    let waiters = [
        TestFutexWaitv {
            val: 0x11,
            uaddr: source,
            flags: FUTEX_32,
            reserved: 0,
        },
        TestFutexWaitv {
            val: 0,
            uaddr: target,
            flags: FUTEX_32,
            reserved: 0,
        },
    ];
    let waiters_uaddr = map_user_waitv_array(&ctx, &waiters);
    let requeue_req = SyscallRequest::new(NR_FUTEX2_REQUEUE, [waiters_uaddr, 0, 0, 1, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(requeue_req, &ctx)),
        SyscallResult::Return(1)
    );

    let source_wake = SyscallRequest::new(NR_FUTEX, [source, FUTEX_WAKE as u64, 1, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(source_wake, &ctx)),
        SyscallResult::Return(0)
    );

    let target_wake = SyscallRequest::new(NR_FUTEX, [target, FUTEX_WAKE as u64, 1, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(target_wake, &ctx)),
        SyscallResult::Return(1)
    );

    let mut last = Poll::Pending;
    for _ in 0..256 {
        last = pinned.as_mut().poll(&mut cx);
        if let Poll::Ready(value) = last {
            assert_eq!(value, SyscallResult::Return(0));
            return;
        }
    }
    panic!("futex2_requeue waiter did not wake from target; last poll = {last:?}");
}
