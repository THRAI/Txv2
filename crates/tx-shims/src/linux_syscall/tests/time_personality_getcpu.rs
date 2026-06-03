// Focused tests for the easy time/personality/getcpu syscall slice.
#![cfg_attr(test, allow(unused_imports))]

use super::*;

use crate::linux_syscall::{
    CLOCK_MONOTONIC, CLOCK_PROCESS_CPUTIME_ID, CLOCK_REALTIME, CLOCK_THREAD_CPUTIME_ID,
    NR_CLOCK_GETRES, NR_GETCPU, NR_PERSONALITY,
};

const E_INVAL: i32 = 22;
const LINUX_DEFAULT_PERSONALITY: i64 = 0;
const PERSONALITY_QUERY: u64 = u32::MAX as u64;

#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
struct TestTimespec {
    tv_sec: i64,
    tv_nsec: i64,
}

#[test]
fn dispatch_clock_getres_writes_one_nanosecond_resolution_for_known_clocks() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    for clk in [
        CLOCK_REALTIME,
        CLOCK_MONOTONIC,
        CLOCK_PROCESS_CPUTIME_ID,
        CLOCK_THREAD_CPUTIME_ID,
    ] {
        let mut res = TestTimespec::default();
        let res_uaddr = &mut res as *mut TestTimespec as u64;
        let result = block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_CLOCK_GETRES, [clk as u64, res_uaddr, 0, 0, 0, 0]),
            &ctx,
        ));

        assert_eq!(result, SyscallResult::Return(0), "clk_id {clk}");
        assert_eq!(
            res,
            TestTimespec {
                tv_sec: 0,
                tv_nsec: 1
            }
        );
    }
}

#[test]
fn dispatch_clock_getres_allows_null_tp_after_validating_clock_id() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    let ok = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_CLOCK_GETRES, [CLOCK_MONOTONIC as u64, 0, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(ok, SyscallResult::Return(0));

    let invalid = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_CLOCK_GETRES, [99, 0, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(invalid, SyscallResult::Error(E_INVAL));
}

#[test]
fn dispatch_getcpu_writes_cpu_and_node_zero_and_ignores_cache_pointer() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);
    let mut cpu = u32::MAX;
    let mut node = u32::MAX;
    let mut cache = 0xfeed_face_u64;

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_GETCPU,
            [
                &mut cpu as *mut u32 as u64,
                &mut node as *mut u32 as u64,
                &mut cache as *mut u64 as u64,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));

    assert_eq!(result, SyscallResult::Return(0));
    assert_eq!(cpu, 0);
    assert_eq!(node, 0);
    assert_eq!(cache, 0xfeed_face_u64);
}

#[test]
fn dispatch_getcpu_accepts_null_output_pointers() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_GETCPU, [0, 0, 0, 0, 0, 0]),
        &ctx,
    ));

    assert_eq!(result, SyscallResult::Return(0));
}

#[test]
fn dispatch_personality_queries_and_accepts_default_noop_set() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    let query = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_PERSONALITY, [PERSONALITY_QUERY, 0, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(query, SyscallResult::Return(LINUX_DEFAULT_PERSONALITY));

    let noop_set = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_PERSONALITY,
            [LINUX_DEFAULT_PERSONALITY as u64, 0, 0, 0, 0, 0],
        ),
        &ctx,
    ));
    assert_eq!(noop_set, SyscallResult::Return(LINUX_DEFAULT_PERSONALITY));
}

#[test]
fn dispatch_personality_rejects_unsupported_changes() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_PERSONALITY, [0x0008_0000, 0, 0, 0, 0, 0]),
        &ctx,
    ));

    assert_eq!(result, SyscallResult::Error(E_INVAL));
}
