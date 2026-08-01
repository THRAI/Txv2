//! Script-scoped identity context for the v3 step model.
//!
//! Per `docs/Txv3/01_CONCEPTS_v5.md` §2.1 and `docs/Txv3/04_SYSCALL_SHAPE_v1.md`
//! §2: every script frame carries exactly one `SubjectContext` describing
//! *who* the script is running as. The context is established at script
//! entry and threaded through `ScriptCtx`; helpers that need subject
//! authority take `&SubjectContext` explicitly. SUBJ-1 (`02_INVARIANTS_v5.md`)
//! is enforced by *absence*: there is no `current_subject_context()`
//! accessor and this module deliberately does not provide one.
//!
//! Wave 3 lands placeholder bodies: `ProcessIdentity`, `ThreadIdentity`,
//! `Credential`, and `RestrictionStackHandle` are unit-like
//! (`_private: ()`) types with `placeholder()` constructors. Wave 4+
//! flips these to `Cap<T>`-wrapped equivalents (`Cap<ProcessIdentity>`,
//! `Cap<Credential>`, …) per the v3 TDD migration plan; the
//! `SubjectContext` / `SubjectAuthority` accessors are stable across
//! that change since they return the inner identity types by value.
//!
//! `SubjectAuthority::authority` replacement (cred-service transition
//! commits) is a publication boundary per SUBJ-3. That replacement path
//! is *not* implemented here — it lands in a later PR alongside the
//! cred-service transition primitive — but the field shape leaves room
//! for it.
//!
//! txdoc cross-refs:
//! - `txdoc:TXV3-STEP-MODEL-V2`
//! - `txdoc:TXV3-CONCEPTS-V5`

use crate::step::WaitSourceId;
use crate::zone::{Cap, Zone, ZoneAllocated};

// ---------------------------------------------------------------------------
// D1 (2026-05-11) — Subject identity trait shapes.
//
// Per `docs/progress/decisions/2026-05-11-d1-scriptctx-trait-bound-identity.md`:
// `step_v3` declares what a subject identity must provide.
// `tx-subsystems` implements these on its concrete
// `process::ProcessIdentity` / `cred::Credential` / `policy::RestrictionStack`.
// `tx-kernel` binds the production alias:
//
//     pub type KernelScriptCtx =
//         step_v3::ScriptCtx<tx_subsystems::process::ProcessIdentity>;
//
// PR-9 wires up the generic `ScriptCtx<I>` and threads `&mut KernelScriptCtx`
// through the 7 canonical syscalls. This module ships only the trait
// declarations + the existing unit-typed placeholders.
//
// The placeholder `struct ProcessIdentity { _private: () }` below
// implements these traits trivially so today's tests and the 80 PR-2
// wraps keep compiling. They will be retired when PR-9 lands the
// `tx-subsystems::process::ProcessIdentity` impl.
// ---------------------------------------------------------------------------

/// What a subject identity must provide so `step_v3` algebra can name
/// it without depending on `tx-subsystems`.
///
/// Implementors live in the subsystem layer (typically
/// `tx-subsystems::process::ProcessIdentity`). The trait is
/// intentionally narrow — it only exposes the views that step bodies
/// or driver scaffolding need to dispatch authority checks and
/// publish exit events.
pub trait SubjectIdentity: 'static {
    /// View of the credential pair (uid/gid/effective/sgid set + caps)
    /// the authority check resolves against.
    ///
    /// `'static` so the trait surface can name `Cap<Self::Credential>`
    /// (`Cap<T>` requires `T: 'static`). PR-9 phase 4 reshapes
    /// `SubjectAuthority` to hold `Cap<I::Credential>` rather than
    /// `I::Credential` by value — production cred (`tx_subsystems::cred::Cred`)
    /// lives in zone-allocated `ProcessPayload` storage; the cap is the
    /// only handle that can be stored in `SubjectContext` without
    /// copying non-`Clone` interior state.
    type Credential: CredentialView + 'static;
    /// Append-only restriction-stack view consulted by seccomp /
    /// landlock / LSM walks. `'static` for the same reason as
    /// `Credential` — stored as `Cap<I::Restrictions>` in
    /// `SubjectAuthority` post-phase-4.
    type Restrictions: RestrictionStackView + 'static;
    /// Per-thread identity view. `None` when the subject is borrowed
    /// via `OnBehalfOf<P>` and no live thread maps to it. `'static` so
    /// the trait surface can name `Cap<Self::ThreadIdentity>` — stored
    /// as `Option<Cap<I::ThreadIdentity>>` in `SubjectContext`
    /// post-phase-4.
    type ThreadIdentity: 'static;

    /// Wait-source id of this subject's exit channel. `None` for
    /// zombies (the channel is unreachable after payload teardown).
    fn exit_source(&self) -> Option<WaitSourceId>;

    /// Low 32 bits of this subject's task trace identity.
    ///
    /// Used by `PayloadDriveBegin` to populate `task_id_low` so the
    /// daemon's `compute_flow_id` hashes the right identity (OBS-4 /
    /// γ-fix). Implementations should return the TID or PID low
    /// 32 bits; kernel actors with no stable identity return `0`.
    ///
    /// Default: `0`. Concrete subsystem impls override this to
    /// return their thread TID or process PID so flow arrows in
    /// Perfetto traces are attributed to the right task.
    fn task_id_low(&self) -> u32 {
        0
    }

    /// Whether `thread` has a signal currently deliverable under its
    /// active signal mask.
    ///
    /// This is the wait-adapt predicate from `THREAD_RUNTIME_v1` §5.3.
    /// The substrate trait gives generic script drivers a narrow way
    /// to ask the semantic owner for the truth-bearing interrupt
    /// summary without depending on process/thread-runtime concrete
    /// types. Placeholder identities report no pending interrupt.
    fn thread_deliverable_signal_pending(_thread: &Cap<Self::ThreadIdentity>) -> bool {
        false
    }

    /// Whether a currently deliverable signal should interrupt a blocked
    /// syscall with `EINTR`.
    ///
    /// This is deliberately narrower than
    /// [`Self::thread_deliverable_signal_pending`]: signals whose disposition
    /// is ignore (including default-ignored `SIGCHLD`) and handlers installed
    /// with `SA_RESTART` are wake hints, but must not abort the wait.
    fn thread_signal_interrupts_wait(_thread: &Cap<Self::ThreadIdentity>) -> bool {
        false
    }

    /// Whether fatal termination is in force for `thread`.
    fn thread_termination_in_force(_thread: &Cap<Self::ThreadIdentity>) -> bool {
        false
    }

    /// Whether `thread` has a pending stop request.
    fn thread_stop_requested(_thread: &Cap<Self::ThreadIdentity>) -> bool {
        false
    }
}

/// Narrow view of a credential. Exposes only what `step_v3` algebra
/// and authority-check helpers need. The concrete `Credential` type
/// in `tx-subsystems` carries the full POSIX uid/gid/sgid/cap set;
/// most of that surface is not part of this trait.
///
/// Implementors in `tx-subsystems` may add inherent methods for
/// subsystem-internal use; the trait surface stays small.
pub trait CredentialView {}

/// Narrow view of an append-only restriction stack. Exposes only
/// the walk interface a step-side authority check needs. Concrete
/// stack management (push/pop/inspect under specific locks) lives
/// in `tx-policy`.
pub trait RestrictionStackView {}

/// Process identity placeholder. Wave 4+ replaces with
/// `Cap<ProcessIdentity>` over a zone-allocated identity row.
///
/// Implements [`ZoneAllocated`] (PR-9 phase 4) so tests can mint a
/// real `Cap<ProcessIdentity>` via the normal zone reserve/sign path
/// — `SubjectContext` now stores `Cap<I>` rather than `I` by value.
#[derive(Debug, Eq, PartialEq)]
pub struct ProcessIdentity {
    _private: (),
}

static PLACEHOLDER_PROCESS_ZONE: Zone<ProcessIdentity> = Zone::const_new();

unsafe impl ZoneAllocated for ProcessIdentity {
    fn zone() -> &'static Zone<Self> {
        &PLACEHOLDER_PROCESS_ZONE
    }
}

impl ProcessIdentity {
    /// Construct a placeholder process identity. Wave 4+ removes this
    /// constructor in favor of zone allocation.
    pub const fn placeholder() -> Self {
        Self { _private: () }
    }
}

impl SubjectIdentity for ProcessIdentity {
    type Credential = Credential;
    type Restrictions = RestrictionStackHandle;
    type ThreadIdentity = ThreadIdentity;

    fn exit_source(&self) -> Option<WaitSourceId> {
        // Placeholder identity has no exit channel. Real
        // `tx_subsystems::process::ProcessIdentity` returns its
        // registered wait-source id (None for zombies).
        None
    }
}

/// Thread identity placeholder. Wave 4+ replaces with
/// `Cap<ThreadIdentity>`.
///
/// Implements [`ZoneAllocated`] (PR-9 phase 4) so tests can mint a
/// real `Cap<ThreadIdentity>` for `SubjectContext::from_thread`.
#[derive(Debug, Eq, PartialEq)]
pub struct ThreadIdentity {
    _private: (),
}

static PLACEHOLDER_THREAD_ZONE: Zone<ThreadIdentity> = Zone::const_new();

unsafe impl ZoneAllocated for ThreadIdentity {
    fn zone() -> &'static Zone<Self> {
        &PLACEHOLDER_THREAD_ZONE
    }
}

impl ThreadIdentity {
    /// Construct a placeholder thread identity.
    pub const fn placeholder() -> Self {
        Self { _private: () }
    }
}

/// Credential placeholder. Wave 4+ replaces with `Cap<Credential>`
/// over the cred-service identity row; SUBJ-3 commits replace the
/// pointer at a publication boundary.
///
/// Implements [`ZoneAllocated`] (PR-9 phase 4) so tests can mint a
/// real `Cap<Credential>` for `SubjectAuthority::new`.
#[derive(Debug, Eq, PartialEq)]
pub struct Credential {
    _private: (),
}

static PLACEHOLDER_CRED_ZONE: Zone<Credential> = Zone::const_new();

unsafe impl ZoneAllocated for Credential {
    fn zone() -> &'static Zone<Self> {
        &PLACEHOLDER_CRED_ZONE
    }
}

impl Credential {
    /// Construct a placeholder credential.
    pub const fn placeholder() -> Self {
        Self { _private: () }
    }
}

impl CredentialView for Credential {}

/// Handle to a `RestrictionStack`. The real append-only stack lives in
/// `step_v3/restriction_stack.rs` (W-restriction-stack); this is just a
/// placeholder reference so `SubjectAuthority` has a concrete type.
/// Wave 4+ replaces with the cap-typed handle that the restriction-stack
/// module exposes.
///
/// Implements [`ZoneAllocated`] (PR-9 phase 4) so tests can mint a
/// real `Cap<RestrictionStackHandle>` for `SubjectAuthority::new`.
#[derive(Debug, Eq, PartialEq)]
pub struct RestrictionStackHandle {
    _private: (),
}

static PLACEHOLDER_RESTRICTIONS_ZONE: Zone<RestrictionStackHandle> = Zone::const_new();

unsafe impl ZoneAllocated for RestrictionStackHandle {
    fn zone() -> &'static Zone<Self> {
        &PLACEHOLDER_RESTRICTIONS_ZONE
    }
}

impl RestrictionStackHandle {
    /// Construct a placeholder restriction-stack handle.
    pub const fn placeholder() -> Self {
        Self { _private: () }
    }
}

impl RestrictionStackView for RestrictionStackHandle {}

/// Subject authority: the credential and restriction-stack pair that
/// the script frame's authority lookups (cred check, seccomp / landlock
/// walk) resolve against.
///
/// Per SUBJ-3, replacement of the authority is a publication boundary
/// driven by a cred-service transition commit; the replacement primitive
/// is not in this PR, but the field shape leaves room for it.
///
/// Generic over `I: SubjectIdentity` per
/// [D1](../../../../../docs/progress/decisions/2026-05-11-d1-scriptctx-trait-bound-identity.md).
/// Default `I = ProcessIdentity` (the step_v3 placeholder) keeps
/// existing tests + the 80 PR-2 wraps compiling unchanged. Production
/// code binds `SubjectAuthority<tx_subsystems::process::ProcessIdentity>`
/// indirectly via `KernelScriptCtx`.
///
/// **Storage shape (PR-9 phase 4):** `Cap<I::Credential>` /
/// `Cap<I::Restrictions>` rather than by-value `I::Credential` /
/// `I::Restrictions`. Production identity rows live in zone-allocated
/// storage with non-`Clone` interior state (`SpinMutex`, `Vec<Cap<...>>`)
/// — they cannot be moved out of a `Cap`. Storing the cap is the only
/// shape that lets `SyscallCtx<'a>` populate a `SubjectContext` (PR-9
/// phase 5 wires this in the seven canonical syscall arms).
///
/// **No `Debug` / `Eq` / `Clone` derive** because `I::Credential` and
/// `I::Restrictions` aren't bounded on those traits and `Cap<T>` only
/// implements `Clone`/`Eq`/`Debug` unconditionally on the cap key,
/// which composes correctly here — but a derive on this wrapper would
/// pull in `Sync` bounds the placeholder types don't need. Tests
/// compare via individual accessor returns (`Cap<T>: PartialEq` via
/// raw key compares).
pub struct SubjectAuthority<I: SubjectIdentity = ProcessIdentity> {
    cred: Cap<I::Credential>,
    restrictions: Cap<I::Restrictions>,
}

impl<I: SubjectIdentity> SubjectAuthority<I> {
    /// Build a `SubjectAuthority` from its credential and
    /// restriction-stack handle.
    ///
    /// Takes `Cap<I::Credential>` and `Cap<I::Restrictions>` by value
    /// (PR-9 phase 4). Production callers pass caps minted from
    /// `ProcessIdentity::cred_cap()` (future PR-9 phase 5 follow-up)
    /// or equivalent; placeholder/test callers mint caps via the zone
    /// reserve/sign path.
    pub const fn new(cred: Cap<I::Credential>, restrictions: Cap<I::Restrictions>) -> Self {
        Self { cred, restrictions }
    }

    /// Credential half of the authority. Returns the cap by reference;
    /// callers that need an owned clone use `Cap::clone(authority.cred())`.
    pub const fn cred(&self) -> &Cap<I::Credential> {
        &self.cred
    }

    /// Restriction-stack handle half of the authority. Returns the cap
    /// by reference; callers clone explicitly when they need an owned
    /// handle.
    pub const fn restrictions(&self) -> &Cap<I::Restrictions> {
        &self.restrictions
    }

    /// **PR-11 phase 0: snapshot the owner's authority for an
    /// `OnBehalfOf<P>` borrow.**
    ///
    /// Constructs a `SubjectAuthority` whose `cred` / `restrictions`
    /// caps are clones of the owner's caps at the moment of borrow.
    /// This is the entry point that `with_on_behalf_of` calls to
    /// materialize the borrow-body's authority snapshot
    /// (`docs/Txv3/06_EXECUTION_SCOPE_v1.md` §4).
    ///
    /// **Snapshot-at-borrow-time semantics.** Per
    /// `06_EXECUTION_SCOPE_v1.md` §4: "the borrowed authority is a
    /// snapshot at borrow time. If the borrowed process changes its
    /// authority (suid exec) during the scope's lifetime, the borrow
    /// is *not* automatically updated." The cap-clone semantics
    /// encode this: the clone holds the slot live via EBR retain,
    /// even if the owner's slot is later replaced. v1 does not
    /// support mid-borrow authority refresh (Open Q 11.1).
    ///
    /// Takes the owner's `SubjectContext` by reference so the cred /
    /// restrictions cap clones come from the *already-validated*
    /// authority pair (SUBJ-3 publication boundary). Callers that
    /// need to construct the authority from raw caps use the
    /// [`Self::new`] constructor directly.
    pub fn derived_from(owner: &SubjectContext<I>) -> Self {
        Self {
            cred: owner.authority.cred.clone(),
            restrictions: owner.authority.restrictions.clone(),
        }
    }
}

/// Script-scoped identity context.
///
/// Established at script entry by exactly one of the two constructors:
/// - `from_thread`: native syscall trampoline materializes the context
///   from the running thread's task (`docs/Txv3/04_SYSCALL_SHAPE_v1.md`
///   §2; SUBJ-2(a)).
/// - `borrowed`: `OnBehalfOf<P>` scope materializes the context from a
///   borrowed process identity; the calling kthread is *not* the
///   subject's thread, so the thread slot is `None` (SUBJ-2(b)). The
///   full `OnBehalfOf<P>` borrow primitive lands in a later PR; this
///   constructor is the entry point.
///
/// SUBJ-1 forbids a global `current_subject_context()` accessor; this
/// module deliberately does not expose one. Helpers that need subject
/// authority take `&SubjectContext` explicitly.
///
/// Generic over `I: SubjectIdentity` per
/// [D1](../../../../../docs/progress/decisions/2026-05-11-d1-scriptctx-trait-bound-identity.md).
/// Default `I = ProcessIdentity` (the step_v3 placeholder) keeps
/// existing tests + the 80 PR-2 wraps compiling unchanged. Production
/// code binds `SubjectContext<tx_subsystems::process::ProcessIdentity>`
/// via `KernelScriptCtx`.
///
/// **Storage shape (PR-9 phase 4):** `Cap<I>` / `Option<Cap<I::ThreadIdentity>>`
/// rather than `I` / `Option<I::ThreadIdentity>` by value. Production
/// `ProcessIdentity` / `ThreadIdentity` (in `tx-subsystems`) carry
/// non-`Clone` interior state (`SpinMutex`, `Vec<Cap<...>>`) that
/// cannot be moved out of a `Cap`. Storing the cap is the only shape
/// that lets `SyscallCtx<'a>` populate a `SubjectContext` from its
/// already-held `Cap<ProcessIdentity>` / `Cap<ThreadIdentity>`. The
/// follow-up phase 5 of PR-9 wires this in the seven canonical syscall
/// arms (sys_open, sys_read, sys_write, sys_fork, sys_close,
/// sys_pipe2, sys_clone).
pub struct SubjectContext<I: SubjectIdentity = ProcessIdentity> {
    process: Cap<I>,
    thread: Option<Cap<I::ThreadIdentity>>,
    authority: SubjectAuthority<I>,
}

impl<I: SubjectIdentity> SubjectContext<I> {
    /// Native syscall entry: borrow the calling thread's identity.
    /// Per SUBJ-2(a) / `04_SYSCALL_SHAPE_v1.md` §2.
    ///
    /// Takes `Cap<I>` / `Cap<I::ThreadIdentity>` by value (PR-9 phase 4).
    /// Production callers (`SyscallCtx<'a>` syscall arms) pass clones
    /// of the caps they already hold (`ctx.process.clone()`,
    /// `ctx.thread.clone()`); these clones bump the retain count on
    /// the underlying zone slot rather than copying interior state.
    pub const fn from_thread(
        process: Cap<I>,
        thread: Cap<I::ThreadIdentity>,
        authority: SubjectAuthority<I>,
    ) -> Self {
        Self {
            process,
            thread: Some(thread),
            authority,
        }
    }

    /// `OnBehalfOf<P>` borrow: a kthread runs scripts under a borrowed
    /// process identity; the thread slot is `None` because the
    /// kthread's own thread identity is not the subject's. Per SUBJ-2(b)
    /// / `04_SYSCALL_SHAPE_v1.md` §2; the full `OnBehalfOf<P>` borrow
    /// primitive lands in a later PR.
    pub const fn borrowed(process: Cap<I>, authority: SubjectAuthority<I>) -> Self {
        Self {
            process,
            thread: None,
            authority,
        }
    }

    /// Process identity that the script frame runs as. Returns the
    /// cap by reference; callers that need an owned cap clone
    /// explicitly (`subject.process().clone()`).
    pub const fn process(&self) -> &Cap<I> {
        &self.process
    }

    /// Optional thread identity. `Some(&Cap<_>)` for native syscall
    /// entry, `None` for `OnBehalfOf<P>` borrows.
    pub const fn thread(&self) -> Option<&Cap<I::ThreadIdentity>> {
        self.thread.as_ref()
    }

    /// Subject authority (credential + restriction-stack handle).
    pub const fn authority(&self) -> &SubjectAuthority<I> {
        &self.authority
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_process_identity_implements_subject_identity_trait() {
        let pid = ProcessIdentity::placeholder();
        // Trait method dispatch.
        assert!(<ProcessIdentity as SubjectIdentity>::exit_source(&pid).is_none());
    }

    #[test]
    fn placeholder_credential_implements_credential_view() {
        // Compile-only: assert the bound.
        fn assert_bound<C: CredentialView>() {}
        assert_bound::<Credential>();
    }

    #[test]
    fn placeholder_restriction_handle_implements_restriction_stack_view() {
        // Compile-only: assert the bound.
        fn assert_bound<R: RestrictionStackView>() {}
        assert_bound::<RestrictionStackHandle>();
    }

    #[test]
    fn subject_identity_associated_types_resolve_for_placeholder() {
        // Compile-only: associated types must resolve so generic
        // bodies `<I: SubjectIdentity>` can name them.
        fn cred_of<I: SubjectIdentity>() -> core::marker::PhantomData<I::Credential> {
            core::marker::PhantomData
        }
        fn restrictions_of<I: SubjectIdentity>() -> core::marker::PhantomData<I::Restrictions> {
            core::marker::PhantomData
        }
        fn thread_of<I: SubjectIdentity>() -> core::marker::PhantomData<I::ThreadIdentity> {
            core::marker::PhantomData
        }
        let _ = cred_of::<ProcessIdentity>();
        let _ = restrictions_of::<ProcessIdentity>();
        let _ = thread_of::<ProcessIdentity>();
    }
}
