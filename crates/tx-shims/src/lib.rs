#![no_std]
// txdoc:vfs-full-bringup-scaffold — relax workspace-wide -D warnings for
// the syscall surface during the vfs-full-bringup merge. The fs/mm
// modules carry placeholder syscall arms, deliberately broad enum
// matches, and identity-cast paths that are intentionally retained
// ahead of their consumers (D-PR-2 wave-4). The arch lint exempts this
// marker (see xtask/src/lint.rs).
#![allow(dead_code)] // txdoc:vfs-full-bringup-scaffold
#![allow(unused_imports)] // txdoc:vfs-full-bringup-scaffold
#![allow(clippy::unnecessary_cast)] // txdoc:vfs-full-bringup-scaffold
#![allow(non_snake_case)] // txdoc:vfs-full-bringup-scaffold
#![allow(clippy::extra_unused_type_parameters)] // txdoc:vfs-full-bringup-scaffold
#![allow(clippy::needless_return)] // txdoc:vfs-full-bringup-scaffold
#![allow(clippy::clone_on_copy)] // txdoc:vfs-full-bringup-scaffold
#![allow(clippy::redundant_guards)] // txdoc:vfs-full-bringup-scaffold

// Required so submodules under `linux_syscall/` can resolve `alloc::*`
// paths (e.g. `alloc::vec::Vec`, `alloc::sync::Arc`). The lib root does
// not reference `alloc::*` directly, but child modules do; this declaration
// brings the crate into the namespace they share.
#[cfg_attr(not(test), allow(unused_extern_crates))]
extern crate alloc;
pub mod adapter;
use crate::adapter::step_engine::ScriptCtx;
#[cfg(test)]
extern crate std;

pub mod linux_syscall;
pub mod posix_signal {}

/// Production `ScriptCtx` alias.
///
/// Per [D1](../../../../docs/progress/decisions/2026-05-11-d1-scriptctx-trait-bound-identity.md):
/// `step_v3` declares the `SubjectIdentity` trait; `tx-subsystems` owns
/// the concrete `process::ProcessIdentity` type and implements the
/// trait on it; **this alias** is the production binding that step
/// bodies + driver code consume.
///
/// Wraps that don't access identity-specific fields stay polymorphic
/// (`impl<I: SubjectIdentity> StepOp<I> for FooOp`). Wraps that need
/// to call methods on the concrete process identity opt into
/// `impl StepOp<tx_subsystems::process::ProcessIdentity> for FooOp`.
///
/// PR-9 of the v3 migration threads `&mut KernelScriptCtx` through
/// the 7 canonical syscalls (sys_open, sys_read, sys_write, sys_fork,
/// sys_execve, sys_close, sys_pipe).
pub type KernelScriptCtx = ScriptCtx<tx_subsystems::process::ProcessIdentity>;

/// Production `SubjectContext` alias parallel to [`KernelScriptCtx`].
pub type KernelSubjectContext =
    crate::adapter::step_engine::SubjectContext<tx_subsystems::process::ProcessIdentity>;

/// Production `SubjectAuthority` alias parallel to [`KernelScriptCtx`].
pub type KernelSubjectAuthority =
    crate::adapter::step_engine::SubjectAuthority<tx_subsystems::process::ProcessIdentity>;

#[cfg(test)]
mod kernel_script_ctx_tests {
    use super::{KernelScriptCtx, KernelSubjectAuthority, KernelSubjectContext};
    use crate::adapter::step_engine::{StepOp, StepOutcome};

    /// Compile-only smoke: production aliases resolve and `KernelScriptCtx`
    /// is constructible.
    #[test]
    fn kernel_script_ctx_constructible() {
        let _ = KernelScriptCtx::new();
    }

    /// Compile-only smoke: a generic `impl<I: SubjectIdentity> StepOp<I>
    /// for FooOp` can be driven against `&mut KernelScriptCtx`.
    #[test]
    fn polymorphic_step_op_works_with_kernel_script_ctx() {
        use crate::adapter::step_engine::{NoProgress, ScriptCtx, SubjectIdentity};

        struct PolyOp;
        impl<I: SubjectIdentity> StepOp<I> for PolyOp {
            type Output = u32;
            type Progress = NoProgress;
            fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<u32, NoProgress> {
                StepOutcome::Done(42)
            }
        }

        let mut op = PolyOp;
        let mut ctx = KernelScriptCtx::new();
        let outcome = op.step(&mut ctx);
        assert_eq!(outcome, StepOutcome::Done(42));
    }

    /// Compile-only smoke: alias trio is name-stable.
    #[test]
    fn alias_types_have_expected_shape() {
        fn _ctx_is_ctx(_: &KernelScriptCtx) {}
        fn _subject_is_subject(_: &KernelSubjectContext) {}
        fn _authority_is_authority(_: &KernelSubjectAuthority) {}
    }

    /// `KernelScriptCtx::new()` returns empty fields. Builders populate.
    /// **No subject** means PR-2 wraps that don't access subject keep
    /// working. Subject-aware ops will read `ctx.subject()` and either
    /// proceed or yield with an error if `None`.
    #[test]
    fn kernel_script_ctx_starts_with_no_subject_and_no_deadline() {
        let ctx = KernelScriptCtx::new();
        assert!(ctx.subject().is_none());
        assert!(ctx.deadline().is_none());
    }

    /// Demonstration: a polymorphic op that **reads the subject** from
    /// `ScriptCtx<I>`. This is the production-binding pattern phase 3
    /// of PR-9 unlocks — ops that previously couldn't depend on
    /// subject identity (because `ScriptCtx` was empty) now can. The
    /// same op works against `ScriptCtx<ProcessIdentity>` (placeholder)
    /// and `KernelScriptCtx` (real production identity) without code
    /// changes.
    #[test]
    fn polymorphic_op_reads_subject_from_script_ctx() {
        use crate::adapter::step_engine::{
            NoProgress, PlaceholderProcessSubject, ScriptCtx, StepOp, StepOutcome, SubjectIdentity,
        };

        // Op semantics: returns Done(true) if a subject is populated,
        // Done(false) otherwise. Real production ops would read
        // `ctx.subject().unwrap().authority().cred()` etc.
        struct HasSubjectOp;
        impl<I: SubjectIdentity> StepOp<I> for HasSubjectOp {
            type Output = bool;
            type Progress = NoProgress;
            fn step(&mut self, ctx: &mut ScriptCtx<I>) -> StepOutcome<bool, NoProgress> {
                StepOutcome::Done(ctx.subject().is_some())
            }
        }

        // Empty KernelScriptCtx → Done(false).
        let mut empty_ctx = KernelScriptCtx::new();
        let mut op = HasSubjectOp;
        assert_eq!(op.step(&mut empty_ctx), StepOutcome::Done(false));

        // Same op against placeholder ScriptCtx<ProcessIdentity> →
        // Done(false). Confirms the op is polymorphic across I.
        let mut placeholder_ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
        assert_eq!(op.step(&mut placeholder_ctx), StepOutcome::Done(false));
    }
}
