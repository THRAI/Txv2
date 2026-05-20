// Auto-extracted from `tests.rs` (2026-05-08 jumbo split).
#![cfg_attr(test, allow(unused_imports))]
use super::*;
use tx_subsystems::process::bootstrap_init_process;

use crate::linux_syscall::{
    FUTEX_CLOCK_REALTIME, FUTEX_PRIVATE_FLAG, FUTEX_REQUEUE, FUTEX_WAIT, FUTEX_WAKE, FUTEX_WAKE_OP,
    NR_FUTEX,
};

const E_INVAL: i32 = 22;
const E_AGAIN: i32 = 11;
const E_NOSYS: i32 = 38;

fn futex_setup() -> TestSetup {
    setup()
}

fn fresh_proc_thread() -> (Cap<ProcessIdentity>, Cap<ThreadIdentity>) {
    let process = bootstrap_init_process(fresh_aspace()).expect("bootstrap init for futex tests");
    let thread = process.nth_thread(0).expect("leader thread");
    (process, thread)
}

/// `futex(uaddr, FUTEX_WAIT, val, ...)` with `*uaddr != val`
/// returns `-EAGAIN` immediately (first-call mismatch — the
/// futex's "fast path" guard short-circuits before parking).
#[test]
fn dispatch_futex_wait_with_mismatched_val_returns_neg_eagain() {
    let _setup = futex_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);
    // User word holds 0x1234; FUTEX_WAIT with val=0x5678 must
    // observe the mismatch and short-circuit.
    let word: u32 = 0x1234;
    let uaddr = &word as *const u32 as u64;

    use tx_subsystems::vm::{
        UserVirtAddr, UserRange, VmMapRequest, MapPlacement, Prot, VmEntryFlags, VmBacking, USER_PAGE_SIZE,
    };
    let page_start = (uaddr as usize) & !(USER_PAGE_SIZE - 1);
    let range = UserRange::new_aligned(UserVirtAddr(page_start), USER_PAGE_SIZE).expect("aligned range");
    let map_req = VmMapRequest::fixed(
        range,
        MapPlacement::FixedReplace,
        Prot::READ_WRITE,
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );
    ctx.aspace.try_mmap(map_req).expect("mmap anon for test");

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

/// `futex(uaddr, FUTEX_WAKE, n, ...)` returns `n` (best-effort
/// wake-N). v1 doesn't track per-bucket waiter counts so the
/// return value is the requested maximum.
#[test]
fn dispatch_futex_wake_returns_n() {
    let _setup = futex_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);
    let word: u32 = 0;
    let uaddr = &word as *const u32 as u64;

    let req = SyscallRequest::new(NR_FUTEX, [uaddr, FUTEX_WAKE as u64, 3, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(3));
}

/// `futex(uaddr, FUTEX_REQUEUE, ...)` returns `-ENOSYS` — only
/// FUTEX_WAIT and FUTEX_WAKE are supported in v1.
#[test]
fn dispatch_futex_unsupported_op_returns_neg_enosys() {
    let _setup = futex_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);
    let word: u32 = 0;
    let uaddr = &word as *const u32 as u64;

    let req = SyscallRequest::new(NR_FUTEX, [uaddr, FUTEX_REQUEUE as u64, 1, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_NOSYS));
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
    let word: u32 = 0;
    let uaddr = &word as *const u32 as u64;

    let op = FUTEX_WAKE | FUTEX_PRIVATE_FLAG;
    let req = SyscallRequest::new(NR_FUTEX, [uaddr, op as u64, 5, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(5));
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
    let word: u32 = 0;
    let uaddr = &word as *const u32 as u64;

    let op = FUTEX_WAKE | FUTEX_CLOCK_REALTIME;
    let req = SyscallRequest::new(NR_FUTEX, [uaddr, op as u64, 2, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(2));
}

/// `futex(uaddr, FUTEX_WAKE_OP, ...)` returns `-ENOSYS`. Pinned
/// here as the canonical "complex op out of v1's surface" case.
#[test]
fn dispatch_futex_wake_op_returns_neg_enosys() {
    let _setup = futex_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);
    let word: u32 = 0;
    let uaddr = &word as *const u32 as u64;

    let req = SyscallRequest::new(NR_FUTEX, [uaddr, FUTEX_WAKE_OP as u64, 1, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_NOSYS));
}
