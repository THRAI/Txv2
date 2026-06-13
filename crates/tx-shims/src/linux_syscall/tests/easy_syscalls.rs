// Focused no-new-design syscall backlog entries with fixed v1 semantics.

use super::*;

use crate::linux_syscall::{
    NR_GETGROUPS, NR_GETPRIORITY, NR_IOPRIO_GET, NR_IOPRIO_SET, NR_RESTART_SYSCALL,
    NR_SCHED_SETPARAM, NR_SETPRIORITY,
};

const EFAULT_VALUE: i32 = 14;
const ESRCH_VALUE: i32 = 3;
const PRIO_PROCESS: u64 = 0;
const PRIO_PGRP: u64 = 1;
const IOPRIO_WHO_PROCESS: u64 = 1;
const IOPRIO_WHO_PGRP: u64 = 2;
const IOPRIO_CLASS_NONE: u64 = 0;
const IOPRIO_CLASS_RT: u64 = 1;
const IOPRIO_CLASS_BE: u64 = 2;

#[repr(C)]
#[derive(Clone, Copy)]
struct TestSchedParam {
    sched_priority: i32,
}

#[test]
fn dispatch_getgroups_reports_zero_groups_without_touching_buffer() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);
    let mut groups = [0xA5A5_A5A5u32; 2];

    let zero_size = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_GETGROUPS, [0, 0, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(zero_size, SyscallResult::Return(0));

    let positive_size = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_GETGROUPS,
            [groups.len() as u64, groups.as_mut_ptr() as u64, 0, 0, 0, 0],
        ),
        &ctx,
    ));
    assert_eq!(positive_size, SyscallResult::Return(0));
    assert_eq!(groups, [0xA5A5_A5A5u32; 2]);

    let invalid_ptr = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_GETGROUPS, [1, 1, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(invalid_ptr, SyscallResult::Return(0));
}

#[test]
fn dispatch_getgroups_rejects_negative_gidsetsize() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_GETGROUPS, [u64::MAX, 0, 0, 0, 0, 0]),
        &ctx,
    ));

    assert_eq!(result, SyscallResult::Error(EINVAL_VALUE));
}

#[test]
fn dispatch_restart_syscall_is_explicitly_unimplemented() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_RESTART_SYSCALL, [0, 0, 0, 0, 0, 0]),
        &ctx,
    ));

    assert_eq!(result, SyscallResult::Error(ENOSYS_VALUE));
}

#[test]
fn dispatch_sched_setparam_accepts_priority_zero_for_self_and_rejects_bad_inputs() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);
    let mut zero = TestSchedParam { sched_priority: 0 };
    let mut nonzero = TestSchedParam { sched_priority: 1 };

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_SCHED_SETPARAM,
                [0, &mut zero as *mut TestSchedParam as u64, 0, 0, 0, 0],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_SCHED_SETPARAM,
                [
                    proc_cap.pid.0 as u64,
                    &mut zero as *mut TestSchedParam as u64,
                    0,
                    0,
                    0,
                    0,
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_SCHED_SETPARAM, [0, 0, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Error(EFAULT_VALUE)
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_SCHED_SETPARAM,
                [0, &mut nonzero as *mut TestSchedParam as u64, 0, 0, 0, 0,],
            ),
            &ctx,
        )),
        SyscallResult::Error(EINVAL_VALUE)
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_SCHED_SETPARAM,
                [999_999, &mut zero as *mut TestSchedParam as u64, 0, 0, 0, 0],
            ),
            &ctx,
        )),
        SyscallResult::Error(ESRCH_VALUE)
    );
}

#[test]
fn dispatch_priority_syscalls_store_nice_for_self_process() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    for who in [0, proc_cap.pid.0 as u64] {
        assert_eq!(
            block_on(dispatch::<ShimsTestPmap>(
                SyscallRequest::new(NR_GETPRIORITY, [PRIO_PROCESS, who, 0, 0, 0, 0]),
                &ctx,
            )),
            SyscallResult::Return(20)
        );
    }

    for who in [0, proc_cap.pid.0 as u64] {
        assert_eq!(
            block_on(dispatch::<ShimsTestPmap>(
                SyscallRequest::new(NR_SETPRIORITY, [PRIO_PROCESS, who, 19, 0, 0, 0]),
                &ctx,
            )),
            SyscallResult::Return(0)
        );
        assert_eq!(
            block_on(dispatch::<ShimsTestPmap>(
                SyscallRequest::new(NR_GETPRIORITY, [PRIO_PROCESS, who, 0, 0, 0, 0]),
                &ctx,
            )),
            SyscallResult::Return(1)
        );
        assert_eq!(
            block_on(dispatch::<ShimsTestPmap>(
                SyscallRequest::new(
                    NR_SETPRIORITY,
                    [PRIO_PROCESS, who, (-20i64) as u64, 0, 0, 0],
                ),
                &ctx,
            )),
            SyscallResult::Return(0)
        );
        assert_eq!(
            block_on(dispatch::<ShimsTestPmap>(
                SyscallRequest::new(NR_GETPRIORITY, [PRIO_PROCESS, who, 0, 0, 0, 0]),
                &ctx,
            )),
            SyscallResult::Return(40)
        );
    }

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_SETPRIORITY, [PRIO_PROCESS, 0, 20, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_GETPRIORITY, [PRIO_PROCESS, 0, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Return(1)
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_GETPRIORITY, [PRIO_PGRP, 0, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Return(1)
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_SETPRIORITY, [PRIO_PROCESS, 123_456, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Error(ESRCH_VALUE)
    );
}

#[test]
fn dispatch_ioprio_syscalls_report_default_and_accept_default_self_noop_only() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);
    let default_be = IOPRIO_CLASS_BE << 13;

    for who in [0, proc_cap.pid.0 as u64] {
        assert_eq!(
            block_on(dispatch::<ShimsTestPmap>(
                SyscallRequest::new(NR_IOPRIO_GET, [IOPRIO_WHO_PROCESS, who, 0, 0, 0, 0]),
                &ctx,
            )),
            SyscallResult::Return(default_be as i64)
        );
        assert_eq!(
            block_on(dispatch::<ShimsTestPmap>(
                SyscallRequest::new(
                    NR_IOPRIO_SET,
                    [IOPRIO_WHO_PROCESS, who, IOPRIO_CLASS_NONE, 0, 0, 0],
                ),
                &ctx,
            )),
            SyscallResult::Return(0)
        );
        assert_eq!(
            block_on(dispatch::<ShimsTestPmap>(
                SyscallRequest::new(
                    NR_IOPRIO_SET,
                    [IOPRIO_WHO_PROCESS, who, default_be, 0, 0, 0],
                ),
                &ctx,
            )),
            SyscallResult::Return(0)
        );
    }

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_IOPRIO_GET, [IOPRIO_WHO_PGRP, 0, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Error(EINVAL_VALUE)
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_IOPRIO_SET,
                [IOPRIO_WHO_PROCESS, 0, IOPRIO_CLASS_RT << 13, 0, 0, 0,],
            ),
            &ctx,
        )),
        SyscallResult::Error(EINVAL_VALUE)
    );
}
