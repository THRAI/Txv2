// Focused no-new-design syscall backlog entries with fixed v1 semantics.

use super::*;

use crate::linux_syscall::{
    SysinfoLayout, NR_GETGROUPS, NR_GETPRIORITY, NR_IOPRIO_GET, NR_IOPRIO_SET, NR_RESTART_SYSCALL,
    NR_SCHED_SETPARAM, NR_SETGROUPS, NR_SETPRIORITY, NR_SYSINFO,
};
use tx_subsystems::cross_crate_test_support::{clear_caps_for_test, set_cred_ids_for_test};

const EFAULT_VALUE: i32 = 14;
const EPERM_VALUE: i32 = 1;
const ESRCH_VALUE: i32 = 3;
const PRIO_PROCESS: u64 = 0;
const PRIO_PGRP: u64 = 1;
const IOPRIO_WHO_PROCESS: u64 = 1;
const IOPRIO_WHO_PGRP: u64 = 2;
const IOPRIO_CLASS_NONE: u64 = 0;
const IOPRIO_CLASS_RT: u64 = 1;
const IOPRIO_CLASS_BE: u64 = 2;
const NR_PRCTL_TEST: u64 = 167;
const NR_RISCV_HWPROBE_TEST: u64 = 258;
const NR_RISCV_FLUSH_ICACHE_TEST: u64 = 259;
const PR_GET_DUMPABLE: u64 = 3;
const PR_SET_DUMPABLE: u64 = 4;
const PR_GET_TIMING: u64 = 13;
const PR_SET_TIMING: u64 = 14;
const PR_TIMING_STATISTICAL: u64 = 0;
const PR_SET_NAME: u64 = 15;
const PR_GET_NAME: u64 = 16;
const PR_SET_NO_NEW_PRIVS: u64 = 38;
const RISCV_HWPROBE_KEY_BASE_BEHAVIOR: i64 = 3;
const RISCV_HWPROBE_KEY_HIGHEST_VIRT_ADDRESS: i64 = 7;
const RISCV_HWPROBE_KEY_TIME_CSR_FREQ: i64 = 8;
const RISCV_HWPROBE_BASE_BEHAVIOR_IMA: u64 = 1;
const RISCV_HWPROBE_WHICH_CPUS: u64 = 1;
const SYS_RISCV_FLUSH_ICACHE_LOCAL: u64 = 1;

#[repr(C)]
#[derive(Clone, Copy)]
struct TestSchedParam {
    sched_priority: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TestRiscvHwprobePair {
    key: i64,
    value: u64,
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
fn dispatch_setgroups_accepts_empty_list_for_privileged_caller() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_SETGROUPS, [0, 0, 0, 0, 0, 0]),
        &ctx,
    ));

    assert_eq!(result, SyscallResult::Return(0));
}

#[test]
fn dispatch_setgroups_validates_privilege_size_and_user_list() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);
    let groups = [1000u32, 1001u32];

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_SETGROUPS,
                [groups.len() as u64, groups.as_ptr() as u64, 0, 0, 0, 0],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_SETGROUPS, [65_537, groups.as_ptr() as u64, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Error(EINVAL_VALUE)
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_SETGROUPS, [1, 1, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Error(EFAULT_VALUE)
    );

    set_cred_ids_for_test(&proc_cap, 1000, 1000, 1000, 1000, 1000, 1000);
    clear_caps_for_test(&proc_cap);
    let unpriv_thread = first_thread(&proc_cap);
    let unpriv_ctx = make_ctx(proc_cap, unpriv_thread);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_SETGROUPS, [0, 0, 0, 0, 0, 0]),
            &unpriv_ctx,
        )),
        SyscallResult::Error(EPERM_VALUE)
    );
}

#[test]
fn dispatch_sysinfo_writes_linux_lp64_snapshot_and_rejects_bad_pointer() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);
    let mut info = SysinfoLayout {
        uptime: -1,
        loads: [u64::MAX; 3],
        totalram: 0,
        freeram: 0,
        sharedram: u64::MAX,
        bufferram: u64::MAX,
        totalswap: u64::MAX,
        freeswap: u64::MAX,
        procs: 0,
        pad: u16::MAX,
        totalhigh: u64::MAX,
        freehigh: u64::MAX,
        mem_unit: 0,
        _f: [],
    };

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_SYSINFO,
            [&mut info as *mut SysinfoLayout as u64, 0, 0, 0, 0, 0],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));
    assert!(info.uptime >= 5);
    assert_eq!(info.loads, [0; 3]);
    assert!(info.totalram >= info.freeram);
    assert!(info.totalram > 0);
    assert!(info.freeram > 0);
    assert_eq!(info.totalswap, 0);
    assert_eq!(info.freeswap, 0);
    assert!(info.procs >= 1);
    assert_eq!(info.pad, 0);
    assert_eq!(info.mem_unit, 1);

    let null_result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_SYSINFO, [0, 0, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(null_result, SyscallResult::Error(EFAULT_VALUE));
}

#[test]
fn dispatch_prctl_name_round_trips_linux_comm() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);
    let name = *b"tx-prctl-name-longer-than-comm\0";
    let mut out = [0xA5u8; 16];

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_PRCTL_TEST,
                [PR_SET_NAME, name.as_ptr() as u64, 0, 0, 0, 0],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );

    let mut expected = [0u8; 16];
    expected[..15].copy_from_slice(&name[..15]);
    assert_eq!(proc_cap.comm(), expected);

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_PRCTL_TEST,
                [PR_GET_NAME, out.as_mut_ptr() as u64, 0, 0, 0, 0],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    assert_eq!(out, expected);

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_PRCTL_TEST, [PR_SET_NAME, 1, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Error(EFAULT_VALUE)
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_PRCTL_TEST, [PR_GET_NAME, 1, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Error(EFAULT_VALUE)
    );
}

#[test]
fn dispatch_prctl_dumpable_timing_and_deferred_options() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_PRCTL_TEST, [PR_GET_DUMPABLE, 0, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Return(1)
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_PRCTL_TEST, [PR_SET_DUMPABLE, 0, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_PRCTL_TEST, [PR_GET_DUMPABLE, 0, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_PRCTL_TEST, [PR_SET_DUMPABLE, 2, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Error(EINVAL_VALUE)
    );

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_PRCTL_TEST, [PR_GET_TIMING, 0, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Return(PR_TIMING_STATISTICAL as i64)
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_PRCTL_TEST,
                [PR_SET_TIMING, PR_TIMING_STATISTICAL, 0, 0, 0, 0],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_PRCTL_TEST, [PR_SET_TIMING, 1, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Error(EINVAL_VALUE)
    );

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_PRCTL_TEST, [PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Error(ENOSYS_VALUE)
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_PRCTL_TEST, [9999, 0, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Error(EINVAL_VALUE)
    );
}

#[test]
fn dispatch_riscv_flush_icache_validates_reserved_flags() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_RISCV_FLUSH_ICACHE_TEST, [0x1000, 0x2000, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_RISCV_FLUSH_ICACHE_TEST,
                [0x1000, 0x2000, SYS_RISCV_FLUSH_ICACHE_LOCAL, 0, 0, 0],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_RISCV_FLUSH_ICACHE_TEST, [0, 0, 2, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Error(EINVAL_VALUE)
    );
}

#[test]
fn dispatch_riscv_hwprobe_reports_conservative_values_and_unknown_keys() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);
    let mut pairs = [
        TestRiscvHwprobePair {
            key: RISCV_HWPROBE_KEY_BASE_BEHAVIOR,
            value: u64::MAX,
        },
        TestRiscvHwprobePair {
            key: RISCV_HWPROBE_KEY_TIME_CSR_FREQ,
            value: 0,
        },
        TestRiscvHwprobePair {
            key: 9999,
            value: u64::MAX,
        },
    ];

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_RISCV_HWPROBE_TEST,
                [pairs.as_mut_ptr() as u64, pairs.len() as u64, 0, 0, 0, 0],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    assert_eq!(pairs[0].key, RISCV_HWPROBE_KEY_BASE_BEHAVIOR);
    assert_eq!(pairs[0].value, RISCV_HWPROBE_BASE_BEHAVIOR_IMA);
    assert_eq!(pairs[1].key, RISCV_HWPROBE_KEY_TIME_CSR_FREQ);
    assert_eq!(
        pairs[1].value,
        <ShimsTestPmap as tx_hal::TimeIf>::frequency_hz()
    );
    assert_eq!(pairs[2].key, -1);
    assert_eq!(pairs[2].value, 0);
}

#[test]
fn dispatch_riscv_hwprobe_validates_flags_and_cpu_masks() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);
    let mut pair = TestRiscvHwprobePair {
        key: RISCV_HWPROBE_KEY_BASE_BEHAVIOR,
        value: RISCV_HWPROBE_BASE_BEHAVIOR_IMA,
    };
    let mut cpus = 1u64;

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_RISCV_HWPROBE_TEST,
                [
                    &mut pair as *mut TestRiscvHwprobePair as u64,
                    1,
                    core::mem::size_of::<u64>() as u64,
                    &mut cpus as *mut u64 as u64,
                    RISCV_HWPROBE_WHICH_CPUS,
                    0,
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    assert_eq!(cpus, 1);

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_RISCV_HWPROBE_TEST, [0, 0, 0, 0, 2, 0]),
            &ctx,
        )),
        SyscallResult::Error(EINVAL_VALUE)
    );

    cpus = 0;
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_RISCV_HWPROBE_TEST,
                [
                    &mut pair as *mut TestRiscvHwprobePair as u64,
                    1,
                    core::mem::size_of::<u64>() as u64,
                    &mut cpus as *mut u64 as u64,
                    0,
                    0,
                ],
            ),
            &ctx,
        )),
        SyscallResult::Error(EINVAL_VALUE)
    );
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
fn dispatch_priority_syscalls_expose_fixed_nice_zero_for_self_process_only() {
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
        assert_eq!(
            block_on(dispatch::<ShimsTestPmap>(
                SyscallRequest::new(NR_SETPRIORITY, [PRIO_PROCESS, who, 19, 0, 0, 0]),
                &ctx,
            )),
            SyscallResult::Return(0)
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
    }

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_SETPRIORITY, [PRIO_PROCESS, 0, 20, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Error(EINVAL_VALUE)
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_GETPRIORITY, [PRIO_PGRP, 0, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Error(EINVAL_VALUE)
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
