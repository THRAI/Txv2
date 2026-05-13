//! Closed catalog of delegate endpoint kinds.
//!
//! Per `docs/Txv3/05_DELEGATE_v1.md` (txdoc:DELEGATE-V1-ENDPOINT-1),
//! every `DelegateEndpoint<K>` is parameterized by an `EndpointKind`
//! that names which class of userspace agent it talks to. Wave 3 of
//! the v3 TDD migration plan only pins the catalog membership; the
//! typed per-kind request/reply layer (a `K: EndpointKind` trait with
//! `K::Request` and `K::Result` associated types) lands in PR-4.
//!
//! Doc tags pinned by the integration tests:
//! - `txdoc:TXV3-STEP-MODEL-V2`
//! - `txdoc:DELEGATE-V1-ENDPOINT-1`
//! - `txdoc:DELEGATE-V1-USES-1`

/// Closed catalog of delegate endpoint kinds. Per
/// `docs/Txv3/05_DELEGATE_v1.md`. Each kind names a class of
/// userspace agent the kernel may delegate to; the typed per-kind
/// request/reply layer (a `K: EndpointKind` trait with `K::Request`
/// and `K::Result` associated types) lands in PR-4 of the v3 TDD
/// migration. Wave 3 only pins the catalog membership.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EndpointKind {
    /// `userfaultfd(2)`-driven page-fault delegation.
    Ufd,
    /// FUSE filesystem delegation (any VFS step on a FUSE-backed
    /// inode yields OnAgent to the FUSE helper).
    Fuse,
    /// `fanotify(7)` permission-event delegation
    /// (`FAN_OPEN_PERM`, `FAN_ACCESS_PERM`).
    FanotifyPerm,
    /// `ptrace(2)` syscall-stop / signal-stop delegation.
    Ptrace,
    /// Synthetic kind for tests of the delegate machinery itself.
    /// Real userspace endpoints never use this; it exists so wave-3
    /// tests can exercise the delegate flow without standing up a
    /// real userfaultfd / FUSE / fanotify / ptrace handler.
    Synthetic,
}

impl EndpointKind {
    /// Returns `true` for kinds that name real Linux userspace ABIs
    /// (Ufd, Fuse, FanotifyPerm, Ptrace). `Synthetic` is the only
    /// `false` row.
    pub const fn is_real(&self) -> bool {
        !matches!(self, EndpointKind::Synthetic)
    }

    /// Returns `true` for kinds whose agent reply may legitimately
    /// inject file descriptors into the borrowing process. Per
    /// `docs/Txv3/05_DELEGATE_v1.md`, FUSE replies carry backing fds
    /// for OPEN replies; the others either use no fd injection
    /// (Ufd, FanotifyPerm) or are reserved for syscall control
    /// (Ptrace). `Synthetic` is conservatively `false`.
    pub const fn permits_fd_injection(&self) -> bool {
        matches!(self, EndpointKind::Fuse)
    }
}
