//! Syscall dispatch context — the per-call handle bundling the caller's
//! process, thread, address-space, and credential caps.
//!
//! See the parent `mod.rs` dispatch doc for the two-site discipline and
//! the `txdoc:THREAD-5-4-THE-TWO-SITE-DISCIPLINE` anchor.

use alloc::sync::{Arc, Weak as ArcWeak};
use core::ops::{Deref, DerefMut};

use crate::adapter::step_engine::Cap;
use tx_substrate::step::DelegateRegistry;
use tx_substrate::wake::mailbox::{MailboxEvent, MailboxSchedulerHint, TaskMailbox};
use tx_subsystems::cred::{Cred, CredSnapshot};
use tx_subsystems::process::ProcessIdentity;
use tx_subsystems::thread_runtime::ThreadIdentity;
use tx_subsystems::vfs::structure::Credential;
use tx_subsystems::vm::AddressSpace;
use tx_time::DeadlineRegistrarHandle;

pub type MailboxPostFn = fn(ArcWeak<TaskMailbox>, MailboxEvent);
pub type MailboxRefPostFn = fn(&TaskMailbox, MailboxEvent) -> bool;
pub type MailboxRefPostWithHintFn = fn(&TaskMailbox, MailboxEvent, MailboxSchedulerHint) -> bool;

/// Script context plus the time capability supplied by the syscall boundary.
///
/// `ScriptCtx` belongs to the substrate step vocabulary and cannot depend on
/// `tx-time`. Keeping the capability in this shim-local wrapper prevents the
/// retired substrate timer registrar from leaking back into script driving.
pub struct SubjectScriptCtx {
    inner: crate::KernelScriptCtx,
    timer_registrar: Option<DeadlineRegistrarHandle>,
}

impl SubjectScriptCtx {
    pub fn timer_registrar(&self) -> Option<&DeadlineRegistrarHandle> {
        self.timer_registrar.as_ref()
    }

    pub fn with_deadline(mut self, deadline: tx_substrate::step::Deadline) -> Self {
        self.inner = self.inner.with_deadline(deadline);
        self
    }
}

impl Deref for SubjectScriptCtx {
    type Target = crate::KernelScriptCtx;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for SubjectScriptCtx {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

pub struct SyscallCtx<'a> {
    pub process: Cap<ProcessIdentity>,
    pub thread: Cap<ThreadIdentity>,
    pub aspace: Cap<AddressSpace>,
    /// Per-task mailbox for yield resolution (drive-taskmb).
    pub mailbox: Option<Arc<TaskMailbox>>,
    /// Optional owner-aware mailbox post operation supplied by the
    /// kernel thread runtime when syscall dispatch has reactor context.
    pub mailbox_post: Option<MailboxPostFn>,
    /// Optional owner-aware post operation for wait-source and bus producer
    /// paths that have already upgraded the subscriber mailbox.
    pub mailbox_ref_post: Option<MailboxRefPostFn>,
    /// Optional owner-aware post operation that also preserves the
    /// producer-provided scheduler hint.
    pub mailbox_ref_post_with_hint: Option<MailboxRefPostWithHintFn>,
    /// Time-service deadline registrar for timer-yield resolution.
    pub timer_registrar: Option<DeadlineRegistrarHandle>,
    /// Reactor delegate registry for OnAgent yield resolution
    /// (drive-taskmb).
    pub delegate_registry: Option<Arc<DelegateRegistry>>,
    /// Syscall-entry credential snapshot.
    ///
    /// Per `cred_service_v_1` §"In flight": canonical credential state
    /// lives in `ProcessPayload.cred` (`AtomicSlot<Cap<Cred>>`); the
    /// script holds a by-value metadata copy captured **once** at
    /// syscall entry. `SyscallCtx::new` populates this from
    /// `process.cred_snapshot()` (or falls back to
    /// `CredSnapshot::root()` for zombies — impossible in practice
    /// from inside a live syscall arm). Subsequent calls to
    /// [`Self::cred`] read from this snapshot without re-loading the
    /// atomic slot, so a mid-syscall `setuid` on the same process does
    /// not perturb authorization decisions taken later in the same
    /// script frame.
    cred_snapshot: CredSnapshot,
    /// Sliced lifetime so future fields (signal-mask snapshot, NOSUID
    /// mount hint) can be added without ripping every call site.
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
        // Capture the syscall-entry credential snapshot exactly once.
        // Per `cred_service_v_1` §"In flight" the script holds a
        // by-value metadata copy, not a live re-reader; subsequent
        // mid-syscall `setuid` on the same process must not perturb
        // checks already taken under this snapshot.
        //
        // `cred_snapshot()` returns `None` only for zombies (payload
        // dropped — cred unobservable). A live syscall arm reaches
        // this code path with `self.process` alive by definition; the
        // `CredSnapshot::root()` fallback is purely defensive and
        // mirrors today's `Cred::root()` fallback in `Self::cred`.
        let cred_snapshot = process.cred_snapshot().unwrap_or_else(CredSnapshot::root);
        Self::from_parts_with_cred_snapshot(process, thread, aspace, cred_snapshot)
    }

    /// Construct from an already-captured syscall-entry credential snapshot.
    ///
    /// This keeps the same in-flight credential contract as [`Self::new`],
    /// while letting the thread runtime instrument or batch the individual
    /// setup steps without paying for a second credential snapshot.
    pub fn from_parts_with_cred_snapshot(
        process: Cap<ProcessIdentity>,
        thread: Cap<ThreadIdentity>,
        aspace: Cap<AddressSpace>,
        cred_snapshot: CredSnapshot,
    ) -> Self {
        Self {
            process,
            thread,
            aspace,
            mailbox: None,
            mailbox_post: None,
            mailbox_ref_post: None,
            mailbox_ref_post_with_hint: None,
            timer_registrar: None,
            delegate_registry: None,
            cred_snapshot,
            _lifetime: core::marker::PhantomData,
        }
    }

    /// Attach a task mailbox for yield resolution (drive-taskmb).
    pub fn with_mailbox(mut self, mailbox: Arc<TaskMailbox>) -> Self {
        self.mailbox = Some(mailbox);
        self
    }

    /// Attach the owner-aware mailbox post operation for signal and
    /// wake-producing syscall paths. Test and bootstrap contexts leave
    /// this unset and use the direct mailbox fallback.
    pub fn with_mailbox_post(mut self, post: MailboxPostFn) -> Self {
        self.mailbox_post = Some(post);
        self
    }

    /// Attach the owner-aware mailbox-ref post operation for wait-source and
    /// bus producer paths whose delivery loop already upgraded the mailbox.
    pub fn with_mailbox_ref_post(mut self, post: MailboxRefPostFn) -> Self {
        self.mailbox_ref_post = Some(post);
        self
    }

    /// Attach the owner-aware mailbox-ref post operation for producers that
    /// carry an explicit scheduler hint.
    pub fn with_mailbox_ref_post_with_hint(mut self, post: MailboxRefPostWithHintFn) -> Self {
        self.mailbox_ref_post_with_hint = Some(post);
        self
    }

    /// Publish a mailbox event through the injected owner-aware route
    /// when available, falling back to direct best-effort posting for
    /// host tests and no-reactor bootstrap contexts.
    pub fn post_mailbox_event(&self, mailbox: ArcWeak<TaskMailbox>, event: MailboxEvent) {
        if let Some(post) = self.mailbox_post {
            post(mailbox, event);
            return;
        }
        let Some(mailbox) = mailbox.upgrade() else {
            return;
        };
        let _ = mailbox.post(event);
    }

    /// Publish through the injected owner-aware route for already-upgraded
    /// mailboxes, falling back to direct best-effort posting for tests and
    /// no-reactor bootstrap contexts.
    pub fn post_mailbox_ref_event(&self, mailbox: &TaskMailbox, event: MailboxEvent) -> bool {
        self.post_mailbox_ref_event_with_hint(mailbox, event, MailboxSchedulerHint::Normal)
    }

    /// Publish through the injected owner-aware route for already-upgraded
    /// mailboxes while preserving the producer's scheduler hint.
    pub fn post_mailbox_ref_event_with_hint(
        &self,
        mailbox: &TaskMailbox,
        event: MailboxEvent,
        hint: MailboxSchedulerHint,
    ) -> bool {
        if let Some(post) = self.mailbox_ref_post_with_hint {
            return post(mailbox, event, hint);
        }
        if let Some(post) = self.mailbox_ref_post {
            return post(mailbox, event);
        }
        mailbox.post_with_scheduler_hint(event, hint)
    }

    /// Attach the time-service deadline registrar for timer-yield resolution.
    pub fn with_timer_registrar<R>(mut self, registrar: R) -> Self
    where
        R: Into<DeadlineRegistrarHandle>,
    {
        self.timer_registrar = Some(registrar.into());
        self
    }

    /// Attach the reactor delegate registry for OnAgent yield
    /// resolution (drive-taskmb).
    pub fn with_delegate_registry(mut self, registry: Arc<DelegateRegistry>) -> Self {
        self.delegate_registry = Some(registry);
        self
    }

    /// Syscall-entry credential value.
    ///
    /// Returns the `Cred` captured into [`Self::cred_snapshot`] at
    /// `SyscallCtx::new`. Per `cred_service_v_1` §"In flight" this is
    /// **not** a fresh load of `ProcessPayload.cred` — once the
    /// snapshot is taken, every check, projection, and read inside
    /// the same syscall frame sees the same value, so a concurrent
    /// `setuid` on the same process (impossible in v1's single-thread-
    /// per-syscall model but architecturally permitted in phase 2)
    /// cannot perturb authorization decisions mid-script.
    ///
    /// Callers that genuinely need the live value (procfs adapters
    /// rendering the *current* status of an arbitrary process) should
    /// call `Cap<ProcessIdentity>::cred()` on the target directly.
    ///
    /// Companion to [`Self::walker_cred`] (the walker-side
    /// projection consumed by VFS path resolution).
    pub fn cred(&self) -> Cred {
        self.cred_snapshot.cred()
    }

    /// Borrow the syscall-entry [`CredSnapshot`] directly.
    ///
    /// Used by check sites that want to thread the snapshot through
    /// authorization predicates by reference, matching the
    /// `cred::checks::require_*(snapshot, foreign_input, &guard)`
    /// shape described in `cred_service_v_1` §"Checks surface".
    pub fn cred_snapshot(&self) -> &CredSnapshot {
        &self.cred_snapshot
    }

    /// Walker-side projection of the current cred. Builds a fresh
    /// [`Credential`] directly from [`Self::cred_snapshot`] via the
    /// `From<&CredSnapshot> for Credential` bridge — uses **euid**
    /// and **egid** (the POSIX rule for DAC checks) and forwards
    /// `effective_caps` so the walker can short-circuit on
    /// `CAP_DAC_OVERRIDE` without re-locking the per-process cred.
    ///
    /// Returned by value so the projection can be held across
    /// `.await` points in callers like `sys_execve` that drive the
    /// multi-phase `exec_script`. The projection reflects the same
    /// frozen syscall-entry snapshot every other `ctx.cred*` accessor
    /// reads from.
    pub fn walker_cred(&self) -> Credential {
        Credential::from(&self.cred_snapshot)
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
pub fn build_subject_script_ctx(ctx: &SyscallCtx<'_>) -> SubjectScriptCtx {
    let cred_cap = ctx.cred_cap();
    let restrictions_cap = tx_subsystems::cred::placeholder_restrictions_cap()
        .expect("placeholder restrictions zone has capacity per syscall entry");
    let authority = crate::KernelSubjectAuthority::new(cred_cap, restrictions_cap);
    let subject = crate::KernelSubjectContext::from_thread(
        ctx.process.clone(),
        ctx.thread.clone(),
        authority,
    );
    let mut script_ctx = crate::KernelScriptCtx::new().with_subject(subject);
    if let Some(ref mailbox) = ctx.mailbox {
        script_ctx = script_ctx.with_mailbox(Arc::clone(mailbox));
    }
    if let Some(ref dr) = ctx.delegate_registry {
        script_ctx = script_ctx.with_delegate_registry(Arc::clone(dr));
    }
    SubjectScriptCtx {
        inner: script_ctx,
        timer_registrar: ctx.timer_registrar.clone(),
    }
}
