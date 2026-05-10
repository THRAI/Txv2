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

/// Process identity placeholder. Wave 4+ replaces with
/// `Cap<ProcessIdentity>` over a zone-allocated identity row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessIdentity {
    _private: (),
}

impl ProcessIdentity {
    /// Construct a placeholder process identity. Wave 4+ removes this
    /// constructor in favor of zone allocation.
    pub const fn placeholder() -> Self {
        Self { _private: () }
    }
}

/// Thread identity placeholder. Wave 4+ replaces with
/// `Cap<ThreadIdentity>`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ThreadIdentity {
    _private: (),
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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Credential {
    _private: (),
}

impl Credential {
    /// Construct a placeholder credential.
    pub const fn placeholder() -> Self {
        Self { _private: () }
    }
}

/// Handle to a `RestrictionStack`. The real append-only stack lives in
/// `step_v3/restriction_stack.rs` (W-restriction-stack); this is just a
/// placeholder reference so `SubjectAuthority` has a concrete type.
/// Wave 4+ replaces with the cap-typed handle that the restriction-stack
/// module exposes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RestrictionStackHandle {
    _private: (),
}

impl RestrictionStackHandle {
    /// Construct a placeholder restriction-stack handle.
    pub const fn placeholder() -> Self {
        Self { _private: () }
    }
}

/// Subject authority: the credential and restriction-stack pair that
/// the script frame's authority lookups (cred check, seccomp / landlock
/// walk) resolve against.
///
/// Per SUBJ-3, replacement of the authority is a publication boundary
/// driven by a cred-service transition commit; the replacement primitive
/// is not in this PR, but the field shape leaves room for it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SubjectAuthority {
    cred: Credential,
    restrictions: RestrictionStackHandle,
}

impl SubjectAuthority {
    /// Build a `SubjectAuthority` from its credential and
    /// restriction-stack handle.
    pub const fn new(cred: Credential, restrictions: RestrictionStackHandle) -> Self {
        Self { cred, restrictions }
    }

    /// Credential half of the authority.
    pub const fn cred(&self) -> Credential {
        self.cred
    }

    /// Restriction-stack handle half of the authority.
    pub const fn restrictions(&self) -> RestrictionStackHandle {
        self.restrictions
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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SubjectContext {
    process: ProcessIdentity,
    thread: Option<ThreadIdentity>,
    authority: SubjectAuthority,
}

impl SubjectContext {
    /// Native syscall entry: borrow the calling thread's identity.
    /// Per SUBJ-2(a) / `04_SYSCALL_SHAPE_v1.md` §2.
    pub const fn from_thread(
        process: ProcessIdentity,
        thread: ThreadIdentity,
        authority: SubjectAuthority,
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
    pub const fn borrowed(process: ProcessIdentity, authority: SubjectAuthority) -> Self {
        Self {
            process,
            thread: None,
            authority,
        }
    }

    /// Process identity that the script frame runs as.
    pub const fn process(&self) -> ProcessIdentity {
        self.process
    }

    /// Optional thread identity. `Some` for native syscall entry,
    /// `None` for `OnBehalfOf<P>` borrows.
    pub const fn thread(&self) -> Option<ThreadIdentity> {
        self.thread
    }

    /// Subject authority (credential + restriction-stack handle).
    pub const fn authority(&self) -> &SubjectAuthority {
        &self.authority
    }
}
