//! Reactor-submission seam for `sys_clone`.
//!
//! `step_fork` produces a child `Cap<ProcessIdentity>` plus a leader
//! `Cap<ThreadIdentity>` but does **not** submit the child's thread
//! future to the reactor — that wiring lives in `tx-kernel` (the BSP
//! reactor + `crate::thread_future::PerHartSlotted` + `run_thread`).
//! Wave 2 of the fork/clone/wait4 slice will land the `sys_clone`
//! syscall arm in `tx-shims`; that arm needs to call back into
//! `tx-kernel`'s reactor submission path without taking a circular
//! crate dependency.
//!
//! The seam is a once-init function-pointer slot exported from this
//! crate: `tx-kernel`'s `init.rs::run_userspace_reactor_loop`
//! (or a sibling init step) installs a closure that builds
//! `PerHartSlotted::<P, _>::new(payload, run_thread::<P>(child_thread,
//! payload))` and submits via the boot `Reactor::submit_task`. The
//! `tx-shims` `sys_clone` arm reads the slot via
//! [`submit_child_thread`] and calls through.
//!
//! Why a function pointer (not the [`tx_hal::PmapIf`]-shaped trait
//! `tx-shims` uses to reach platform code today): the boot reactor
//! (`tx-kernel::init::BOOT_REACTOR`) is private to `tx-kernel`, not
//! per-platform; `PmapIf`'s generic-parameter path cannot thread a
//! reactor handle through. A function pointer captures the boot
//! reactor in its body without exporting it. The platform parameter
//! `P: tx_hal::PmapIf` (and its TxPlatform supertraits) is captured
//! at install time inside `tx-kernel`, so the slot signature stays
//! parameter-free at the seam.
//!
//! Wave 1 ships the slot machinery and the install seam; Wave 2 fires
//! the read site from `sys_clone`.

use core::sync::atomic::{AtomicPtr, Ordering};

mod adapter;

use adapter::step_engine::Cap;

use crate::process::ProcessIdentity;
use crate::thread_runtime::ThreadIdentity;

/// Function-pointer signature for the reactor-submission hook.
///
/// `child_process` and `child_thread` are the freshly minted caps
/// returned by `step_fork`; the implementation submits a
/// `PerHartSlotted::<P, _>::new(payload, run_thread::<P>(child_thread,
/// payload))` task to the boot reactor. `P` is captured by the
/// installer (`tx-kernel::init`) at boot time so the seam can stay
/// parameter-free.
pub type SubmitChildThreadFn =
    fn(child_process: Cap<ProcessIdentity>, child_thread: Cap<ThreadIdentity>);

/// Slot holding the installed [`SubmitChildThreadFn`]. `AtomicPtr`
/// is used (over `SpinMutex<Option<...>>`) so the read path
/// (`sys_clone` in Wave 2) is lock-free; the install path runs
/// once at boot before any user syscall fires.
static SUBMIT_CHILD_THREAD_FN: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// Install the reactor-submission hook. Called once at boot from
/// `tx-kernel`'s `init.rs` before the BSP reactor loop is entered
/// and any user syscall can fire.
///
/// Idempotent for the same function pointer (re-installing the
/// same fn pointer is a no-op); a different installer overwrites
/// the previous slot value without panicking. Tests reset between
/// runs via [`reset_for_test`].
pub fn install_submit_child_thread(f: SubmitChildThreadFn) {
    SUBMIT_CHILD_THREAD_FN.store(f as *mut (), Ordering::Release);
}

/// Read the installed hook, if any. Wave 2 of the fork/clone/wait4
/// slice will use this from `sys_clone` to submit a freshly-cloned
/// child to the reactor.
///
/// Returns `None` before the boot path has installed a hook (test
/// pre-bootstrap or boot-time pre-`run_userspace_reactor_loop`).
pub fn submit_child_thread_fn() -> Option<SubmitChildThreadFn> {
    let raw = SUBMIT_CHILD_THREAD_FN.load(Ordering::Acquire);
    if raw.is_null() {
        None
    } else {
        // SAFETY: the only writer is `install_submit_child_thread`,
        // which stores the result of `f as *mut ()` for an
        // `fn(Cap<ProcessIdentity>, Cap<ThreadIdentity>)`. The cast
        // back is the inverse and reproduces the same fn pointer.
        Some(unsafe { core::mem::transmute::<*mut (), SubmitChildThreadFn>(raw) })
    }
}

/// Convenience wrapper that submits a child thread through the
/// installed hook, panicking with a stable sentinel string if no
/// hook is installed.
///
/// Wave 2's `sys_clone` calls this. Reaching the panic arm is a
/// kernel-invariant violation: the boot path must install the hook
/// before any user syscall can fire.
pub fn submit_child_thread(child_process: Cap<ProcessIdentity>, child_thread: Cap<ThreadIdentity>) {
    let f = submit_child_thread_fn()
        .expect("reactor_submit: SUBMIT_CHILD_THREAD_FN not installed before sys_clone fired");
    f(child_process, child_thread);
}

/// Test-only: clear the installed hook so a subsequent
/// [`install_submit_child_thread`] call sees an empty slot. Used by
/// `tx-kernel`'s init tests to verify the install seam.
#[cfg(any(test, feature = "test-support"))]
pub fn reset_for_test() {
    SUBMIT_CHILD_THREAD_FN.store(core::ptr::null_mut(), Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_submit(_p: Cap<ProcessIdentity>, _t: Cap<ThreadIdentity>) {
        // No-op installer used to verify the slot mechanics; never
        // called because we don't synthesise valid Caps in this test.
    }

    #[test]
    fn submit_child_thread_fn_returns_none_before_install() {
        reset_for_test();
        assert!(submit_child_thread_fn().is_none());
    }

    #[test]
    fn install_then_read_round_trips_function_pointer() {
        reset_for_test();
        install_submit_child_thread(dummy_submit);
        let f = submit_child_thread_fn().expect("hook installed");
        // Use `as *const ()` first per `function_casts_as_integer` lint.
        assert_eq!(f as *const () as usize, dummy_submit as *const () as usize);
        reset_for_test();
    }

    #[test]
    fn reset_for_test_clears_slot() {
        install_submit_child_thread(dummy_submit);
        reset_for_test();
        assert!(submit_child_thread_fn().is_none());
    }
}
