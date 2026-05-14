//! Syscall dispatch context — the per-call handle bundling the caller's
//! process, thread, address-space, and credential caps.
//!
//! See the parent `mod.rs` dispatch doc for the two-site discipline and
//! the `txdoc:THREAD-5-4-THE-TWO-SITE-DISCIPLINE` anchor.

use crate::adapter::step_engine;
use crate::adapter::step_engine::Cap;
use tx_subsystems::cred::Cred;
use tx_subsystems::process::ProcessIdentity;
use tx_subsystems::thread_runtime::ThreadIdentity;
use tx_subsystems::vfs::structure::Credential;
use tx_subsystems::vm::AddressSpace;

pub struct SyscallCtx<'a> {
    pub process: Cap<ProcessIdentity>,
    pub thread: Cap<ThreadIdentity>,
    pub aspace: Cap<AddressSpace>,
    /// Sliced lifetime so future fields (signal-mask snapshot, cred
    /// snapshot) can be added without ripping every call site.
    pub _lifetime: core::marker::PhantomData<&'a ()>,
}

impl<'a> SyscallCtx<'a> {
    /// Construct a fresh context. Phase 2a callers (the syscall
    /// dispatch tests; the future trap-shell wrapper in Phase 6) take
    /// the three Caps from the resolved per-thread payload and pass
    /// them in.
    pub fn new(
        process: Cap<ProcessIdentity>,
        thread: Cap<ThreadIdentity>,
        aspace: Cap<AddressSpace>,
    ) -> Self {
        Self {
            process,
            thread,
            aspace,
            _lifetime: core::marker::PhantomData,
        }
    }

    /// Snapshot the current process's full credential.
    ///
    /// Returns a fresh [`Cred`] value (`Copy`); the payload's
    /// `AtomicSlot<Cap<Cred>>` is loaded once (PR-9 phase 5 / D5 Path
    /// A — was `SpinMutex<Cred>`), the resulting cap is derefed to
    /// `&Cred`, and the value is copied out; the cap clone drops
    /// before return, so the snapshot is independent of the slot and
    /// safe to hold across `.await` points. Holding a reference
    /// through the cap would still be sound (the cap retain-count
    /// keeps the slab entry live), but callers that need the
    /// long-lived cap shape should use [`Self::cred_cap`] instead.
    ///
    /// Falls back to [`Cred::root`] for zombies (impossible in
    /// practice from inside a live syscall arm — the caller is by
    /// definition alive). The defensive default keeps every
    /// downstream arm's signature noise-free; callers that need to
    /// distinguish zombie vs. alive use `ctx.process.is_zombie()`
    /// directly.
    ///
    /// Companion to [`Self::walker_cred`] (the walker-side
    /// projection consumed by VFS path resolution).
    pub fn cred(&self) -> Cred {
        self.process.cred().unwrap_or_else(Cred::root)
    }

    /// Walker-side projection of the current cred. Builds a fresh
    /// [`Credential`] from `self.cred()` via the
    /// `From<&Cred> for Credential` bridge (Wave 1) — uses **euid**
    /// and **egid** (the POSIX rule for DAC checks), and forwards
    /// `effective_caps` so the walker can short-circuit on
    /// `CAP_DAC_OVERRIDE` without re-locking the per-process cred.
    ///
    /// Returned by value (never as a reference into the lock) so the
    /// snapshot can be held across `.await` points in callers like
    /// `sys_execve` that drive the multi-phase `exec_script`.
    pub fn walker_cred(&self) -> Credential {
        Credential::from(&self.cred())
    }

    /// Snapshot the current process's `Cap<Cred>`. Returns a cloned
    /// strong cap; the slab entry stays live until the cap drops.
    ///
    /// PR-9 phase 5 (D5 Path A): the cred-mutators
    /// (`step_setuid` / `step_setgid` / ...) replace the slot's
    /// inhabitant per call; this accessor reads whichever cap is
    /// current. Concurrent mutators between this read and the
    /// `SubjectContext` construction yield a cap pointing at the
    /// pre-mutation cred — the syscall arm sees a coherent snapshot
    /// for the duration of its script frame.
    ///
    /// Defensive fallback: zombies have no payload, so no cred-cap.
    /// In that case we mint a fresh `Cap<Cred>` from `Cred::root()`.
    /// Reaching this fallback inside a live syscall is impossible by
    /// construction (the calling process is by definition alive).
    pub fn cred_cap(&self) -> Cap<Cred> {
        self.process.cred_cap().unwrap_or_else(|| {
            tx_subsystems::cred::sign_cred(Cred::root())
                .expect("zone slab has capacity for defensive root cred")
        })
    }
}

/// PR-9 phase 5 (D5 Path A) — build a `KernelScriptCtx` whose subject
/// is populated from `SyscallCtx`. The four wired arms (sys_read,
/// sys_write, sys_pipe2, sys_clone) call this at script entry so the
/// step body receives `ctx.subject()` rather than `None`.
///
/// Restrictions cap is a fresh placeholder per call until PR-K lands
/// the real append-only stack (D5 §7). Each call mints one
/// `Cap<RestrictionStackHandle>` from the substrate placeholder zone;
/// the cap drops at script-frame exit (EBR retires the slab).
///
/// **Failure mode**: zone-slab exhaustion mints a defensive
/// placeholder cap from `Cred::root()` and panics on
/// restrictions-cap failure (the placeholder zone is sized for one
/// cap per concurrent syscall — exhaustion is a kernel-wide pressure
/// event PR-K will revisit). Production callers should not hit this
/// path; for now the conservative-panic matches today's
/// `expect("zone slab has capacity")` discipline elsewhere in this
/// module.
pub fn build_subject_script_ctx(ctx: &SyscallCtx<'_>) -> crate::KernelScriptCtx {
    let cred_cap = ctx.cred_cap();
    let restrictions_cap = tx_subsystems::cred::placeholder_restrictions_cap()
        .expect("placeholder restrictions zone has capacity per syscall entry");
    let authority = crate::KernelSubjectAuthority::new(cred_cap, restrictions_cap);
    let subject = crate::KernelSubjectContext::from_thread(
        ctx.process.clone(),
        ctx.thread.clone(),
        authority,
    );
    crate::KernelScriptCtx::new().with_subject(subject)
}
