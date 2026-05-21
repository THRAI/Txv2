// Auto-extracted from `tests.rs` (2026-05-21 musl ABI audit).
#![cfg_attr(test, allow(unused_imports))]
use super::*;

const NR_SIGALTSTACK: u64 = 132;
const E_INVAL: i32 = 22;
const E_NOMEM: i32 = 12;

const SS_DISABLE: i32 = 2;
const SS_ONSTACK: i32 = 1;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct TestStackT {
    ss_sp: u64,
    ss_flags: i32,
    _pad: u32,
    ss_size: u64,
}

fn sigaltstack_setup() -> (TestSetup, Cap<ProcessIdentity>, Cap<ThreadIdentity>) {
    let setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    (setup, proc_cap, thread)
}

#[test]
fn dispatch_sigaltstack_registers_and_reports_old_stack() {
    let (_setup, proc_cap, thread) = sigaltstack_setup();
    let ctx = make_ctx(proc_cap, thread);

    let new_stack = TestStackT {
        ss_sp: 0x7000_0000,
        ss_flags: 0,
        _pad: 0,
        ss_size: 8192,
    };
    let mut old = TestStackT::default();
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_SIGALTSTACK,
            [
                &new_stack as *const TestStackT as u64,
                &mut old as *mut TestStackT as u64,
                0,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));
    assert_eq!(
        old,
        TestStackT {
            ss_sp: 0,
            ss_flags: SS_DISABLE,
            _pad: 0,
            ss_size: 0,
        }
    );

    let mut queried = TestStackT::default();
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_SIGALTSTACK,
            [0, &mut queried as *mut TestStackT as u64, 0, 0, 0, 0],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));
    assert_eq!(
        queried,
        TestStackT {
            ss_sp: new_stack.ss_sp,
            ss_flags: 0,
            _pad: 0,
            ss_size: new_stack.ss_size,
        }
    );
}

#[test]
fn dispatch_sigaltstack_rejects_onstack_and_too_small_stack() {
    let (_setup, proc_cap, thread) = sigaltstack_setup();
    let ctx = make_ctx(proc_cap, thread);

    let onstack = TestStackT {
        ss_sp: 0x7000_0000,
        ss_flags: SS_ONSTACK,
        _pad: 0,
        ss_size: 8192,
    };
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_SIGALTSTACK,
            [&onstack as *const TestStackT as u64, 0, 0, 0, 0, 0],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Error(E_INVAL));

    let too_small = TestStackT {
        ss_sp: 0x7000_0000,
        ss_flags: 0,
        _pad: 0,
        ss_size: 1024,
    };
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_SIGALTSTACK,
            [&too_small as *const TestStackT as u64, 0, 0, 0, 0, 0],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Error(E_NOMEM));
}
