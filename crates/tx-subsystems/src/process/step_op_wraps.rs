//! PR-2 StepOp wrap tests (process::execution scope).
//!
//! Each test exercises one `*Op` wrap end-to-end: build the op with
//! a fixture, drive `.step(&mut ScriptCtx)`, assert the outcome
//! variant shape. Coverage of the underlying step-fn semantics lives
//! in `process::tests`; the value here is the compile-check plus a
//! smoke that the wrap delegates with the expected argument plumbing.

use super::*;

use crate::process::adapter::step_engine::{
    PlaceholderProcessSubject, ScriptCtx, StepOp, StepOutcome,
};
use crate::process::structure::reset_pid_counter_for_test;
use crate::signal::Signum;
use crate::test_support::EPOCH_TEST_LOCK;
use crate::thread_runtime::structure::reset_tid_counter_for_test;
use crate::vm::{AddressSpace, TestPmap};
use crate::zones;

fn setup() -> std::sync::MutexGuard<'static, ()> {
    let guard = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    tx_test_support::init_host();
    let _ = zones::register_all();
    tx_test_support::drain_to_quiescence();
    reset_pid_counter_for_test();
    reset_tid_counter_for_test();
    reset_init_process_for_test();
    guard
}

fn fresh_aspace() -> Cap<AddressSpace> {
    AddressSpace::new_cap_for_platform::<TestPmap>().expect("fresh aspace")
}

fn bootstrap() -> Cap<ProcessIdentity> {
    bootstrap_init_process(fresh_aspace()).expect("bootstrap init")
}

#[test]
fn fork_op_delegates_to_step_fork() {
    let _g = setup();
    let parent = bootstrap();
    let mut op = ForkOp::<TestPmap> {
        parent: &parent,
        clone_vm: false,
        clone_sighand: false,
        _pmap: core::marker::PhantomData,
    };
    let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
    let outcome = op.step(&mut ctx);
    match outcome {
        StepOutcome::Done(result) => {
            let child = result.expect("step_fork should succeed");
            assert_ne!(child.pid, parent.pid);
        }
        _ => panic!("expected Done(_), got non-Done outcome"),
    }
}

#[test]
fn exit_group_op_delegates_to_step_exit_group() {
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent, false, false).expect("fork");
    let mut op = ExitGroupOp {
        process: &child,
        status: ExitStatus::Exited(0),
    };
    let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
    let outcome = op.step(&mut ctx);
    assert_eq!(outcome, StepOutcome::Done(()));
    assert!(child.is_zombie());
    assert_eq!(child.exit_status(), Some(ExitStatus::Exited(0)));
}

#[test]
fn exit_group_with_signal_op_delegates_to_step_exit_group_with_signal() {
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent, false, false).expect("fork");
    let mut op = ExitGroupWithSignalOp {
        process: &child,
        sig: Signum::SIGKILL,
    };
    let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
    let outcome = op.step(&mut ctx);
    assert_eq!(outcome, StepOutcome::Done(()));
    assert!(child.is_zombie());
    assert_eq!(
        child.exit_status(),
        Some(ExitStatus::Signaled(Signum::SIGKILL))
    );
}

#[test]
fn waitpid_nohang_op_no_children_returns_echild() {
    let _g = setup();
    let parent = bootstrap();
    let mut op = WaitpidNohangOp {
        parent: &parent,
        target: WaitTarget::Any,
    };
    let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
    let outcome = op.step(&mut ctx);
    match outcome {
        StepOutcome::Done(Err(WaitError::NoChildren)) => {}
        other => panic!("expected Done(Err(NoChildren)), got {other:?}"),
    }
}

#[test]
fn getcwd_op_returns_none_for_init_without_cwd() {
    let _g = setup();
    let parent = bootstrap();
    let mut op = GetcwdOp { target: &parent };
    let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
    let outcome = op.step(&mut ctx);
    assert_eq!(outcome, StepOutcome::Done(None));
}

#[test]
fn setpgid_op_unimplemented_for_non_self_pgid() {
    let _g = setup();
    let parent = bootstrap();
    let bogus = Pgid(parent.pid.0 + 999);
    let mut op = SetpgidOp {
        target: &parent,
        new_pgid: bogus,
    };
    let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
    let outcome = op.step(&mut ctx);
    match outcome {
        StepOutcome::Err(v3_errno)
            if Into::<crate::execution::Errno>::into(v3_errno)
                == crate::execution::Errno::ENOSYS => {}
        other => panic!("expected Err(ENOSYS), got {other:?}"),
    }
}

#[test]
fn setsid_op_delegates_to_step_setsid() {
    let _g = setup();
    let parent = bootstrap();
    let child = step_fork::<TestPmap>(&parent, false, false).expect("fork");
    let mut op = SetsidOp { target: &child };
    let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
    let outcome = op.step(&mut ctx);
    match outcome {
        StepOutcome::Done(sid) => assert_eq!(sid.0, child.pid.0),
        other => panic!("expected Done(sid), got {other:?}"),
    }
}

#[test]
fn close_cloexec_fds_op_is_noop_with_empty_set() {
    let _g = setup();
    let parent = bootstrap();
    let mut op = CloseCloexecFdsOp { process: &parent };
    let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
    let outcome = op.step(&mut ctx);
    assert_eq!(outcome, StepOutcome::Done(()));
    assert!(parent.fd_cloexec_snapshot().is_empty());
}

#[test]
fn reset_signal_dispositions_for_exec_op_runs_on_live_process() {
    let _g = setup();
    let parent = bootstrap();
    let mut op = ResetSignalDispositionsForExecOp { process: &parent };
    let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
    let outcome = op.step(&mut ctx);
    assert_eq!(outcome, StepOutcome::Done(()));
}

#[test]
fn install_brk_for_exec_op_seeds_brk_base_and_current() {
    let _g = setup();
    let parent = bootstrap();
    let new_base: u64 = 0x4000_0000;
    let mut op = InstallBrkForExecOp {
        process: &parent,
        new_brk_base: new_base,
    };
    let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
    let outcome = op.step(&mut ctx);
    assert_eq!(outcome, StepOutcome::Done(()));
    assert_eq!(parent.brk_base(), new_base);
    assert_eq!(parent.current_brk(), new_base);
}
