//! Shared syscall-side path resolution helpers.
//!
//! This module is intentionally shim-local. It models Linux syscall entry
//! concerns (`dirfd`, absolute-path anchoring, and parent/name splitting)
//! without introducing a second VFS-core path-resolution vocabulary.
//! `ResolvedPath` is the syscall-side completed walk wrapper; VFS core keeps
//! using `vfs::resolution::PathResolution` for walker-internal evidence.

use super::*;
use crate::adapter::step_engine::{self as step_engine, Cap, StepOutcome};

#[derive(Clone, Debug)]
pub(super) struct ResolvedPath {
    dentry: Cap<DEntry>,
}

impl ResolvedPath {
    pub(super) fn at(
        dirfd: i32,
        path: &[u8],
        cred: &Credential,
        ctx: &SyscallCtx<'_>,
    ) -> Result<Self, i32> {
        let rooted_at = dirfd_anchor_errno(dirfd, path, ctx)?;
        Self::from_process_root(rooted_at, path, cred, &ctx.process)
    }

    pub(super) fn from_root(
        rooted_at: Cap<DEntry>,
        path: &[u8],
        cred: &Credential,
    ) -> Result<Self, i32> {
        Self::walk(rooted_at, path, cred, None)
    }

    pub(super) fn from_process_root(
        rooted_at: Cap<DEntry>,
        path: &[u8],
        cred: &Credential,
        process: &Cap<ProcessIdentity>,
    ) -> Result<Self, i32> {
        Self::walk(
            rooted_at,
            path,
            cred,
            process.mount_namespace_cap().as_ref(),
        )
    }

    pub(super) fn into_dentry(self) -> Cap<DEntry> {
        self.dentry
    }

    fn walk(
        rooted_at: Cap<DEntry>,
        path: &[u8],
        cred: &Credential,
        mount_namespace: Option<&Cap<tx_subsystems::mount::MountNamespace>>,
    ) -> Result<Self, i32> {
        let guard = step_engine::guard();
        let outcome = if let Some(mnt_ns) = mount_namespace {
            tx_subsystems::vfs::step_walk_in_mount_namespace(rooted_at, path, cred, mnt_ns, &guard)
        } else {
            step_walk(rooted_at, path, cred, &guard)
        };
        drop(guard);
        match outcome {
            StepOutcome::Done(dentry) => Ok(Self { dentry }),
            StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => Err(EIO_VALUE),
            StepOutcome::Err(errno) => Err(errno_to_i32(Errno::from(errno))),
        }
    }
}

pub(super) fn dirfd_anchor_errno(
    dirfd: i32,
    path: &[u8],
    ctx: &SyscallCtx<'_>,
) -> Result<Cap<DEntry>, i32> {
    if path.starts_with(b"/") || dirfd == AT_FDCWD {
        return ctx.process.cwd().ok_or(ENOENT_VALUE);
    }
    if dirfd < 0 {
        return Err(EBADF_VALUE);
    }
    let open_file = ctx.process.fd(dirfd as u32).ok_or(EBADF_VALUE)?;
    open_file.opendir_dentry().ok_or(ENOTDIR_VALUE)
}

pub(super) fn dirfd_anchor_for_path(
    dirfd: i32,
    path: &[u8],
    ctx: &SyscallCtx<'_>,
) -> Result<Cap<DEntry>, SyscallResult> {
    dirfd_anchor_errno(dirfd, path, ctx).map_err(SyscallResult::Error)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ParentName<'p> {
    pub parent_path: &'p [u8],
    pub basename: &'p [u8],
}

/// Split a path into `(parent, basename)` for parent/name syscalls.
///
/// The slash itself stays with the parent so absolute parents remain absolute
/// and the walker can restart from the mount root.
pub(super) fn split_parent_name(path: &[u8]) -> ParentName<'_> {
    match path.iter().rposition(|b| *b == b'/') {
        None => ParentName {
            parent_path: &[],
            basename: path,
        },
        Some(idx) => ParentName {
            parent_path: &path[..=idx],
            basename: &path[idx + 1..],
        },
    }
}
