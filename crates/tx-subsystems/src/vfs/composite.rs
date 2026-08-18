//! Composite StepOp wrappers — path resolution + FsOps call in one struct.
//!
//! Each op resolves a path to a DEntry via the VFS walker, then calls
//! the appropriate `FsOps` / `FsPageBacking` method.  The EBR guard
//! lives on the struct (`guard` field) rather than being extracted
//! from `ScriptCtx`, matching the existing OpenFile*Op convention in
//! `vfs::execution`.
//!
//! | Op | Syscalls | Backend call |
//! |----|----------|-------------|
//! | ChmodOp | fchmodat | FsOps::chmod_inode |
//! | ChownOp | fchownat | FsOps::chown_inode |
//! | AccessOp | faccessat, faccessat2 | walker-only |
//! | MkdirOp | mkdirat | FsOps::mkdir |
//! | UnlinkOp | unlinkat | FsOps::unlink |
//! | SymlinkOp | symlinkat | FsOps::symlink |
//! | LinkOp | linkat | FsOps::link |
//! | RenameOp | renameat2 | FsOps::rename |
//! | TruncateOp | truncate, ftruncate | FsPageBacking::truncate |
//! | StatOp | newfstatat, fstat | load_inode_meta |
//! | LstatOp | lstat | load_inode_meta |
//! | StatxOp | statx | load_inode_meta |
//! | ReadLinkOp | readlinkat | FsOps::read_link |

use alloc::boxed::Box;

use crate::vfs::adapter::step_engine::{
    self, Cap, Deadline, InterestMask, NoProgress, OneShotStepOp, ProcessIdentity, ResumeOutcome,
    ScriptCtx, StepOp, StepOutcome, SubjectIdentity, WaitSourceId, YieldShape,
};
use crate::vfs::notification;
use crate::vfs::walker;
use crate::vfs::{Credential, DEntry, FsObjectId, InlineName, InodeKind, InodeMeta, RNodeBacking};

// ============================================================================
// ChmodOp — fchmodat
// ============================================================================

pub struct ChmodOp<'a> {
    pub rooted_at: &'a Cap<DEntry>,
    pub path: &'a [u8],
    pub mode: u16,
    pub cred: &'a Credential,
    pub target: Option<Cap<DEntry>>,
}

impl<'a, I: SubjectIdentity> StepOp<I> for ChmodOp<'a> {
    type Output = ();
    type Progress = NoProgress;

    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<(), NoProgress> {
        let __guard = step_engine::guard();
        let target = match self.target.as_ref() {
            Some(d) => d.clone(),
            None => {
                let rooted_at = self.rooted_at.clone();
                let d = match walker::step_walk(rooted_at, self.path, self.cred, &__guard) {
                    StepOutcome::Done(d) => d,
                    StepOutcome::Err(e) => return StepOutcome::err(e),
                    _ => return StepOutcome::err(step_engine::Errno::EIO),
                };
                self.target = Some(d.clone());
                d
            }
        };
        let fs_ops = walker::fs_ops_for(&target, &__guard).expect("NoFsOps for ChmodOp");
        let outcome = fs_ops.chmod_inode(
            target.rnode().fs_object_id(),
            self.mode,
            self.cred,
            &__guard,
        );
        if matches!(outcome, StepOutcome::Done(())) {
            target.rnode().set_mode(self.mode);
        }
        outcome
    }
}

// ============================================================================
// ChownOp — fchownat
// ============================================================================

pub struct ChownOp<'a> {
    pub rooted_at: &'a Cap<DEntry>,
    pub path: &'a [u8],
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub cred: &'a Credential,
    pub target: Option<Cap<DEntry>>,
}

impl<'a, I: SubjectIdentity> StepOp<I> for ChownOp<'a> {
    type Output = ();
    type Progress = NoProgress;

    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<(), NoProgress> {
        let __guard = step_engine::guard();
        let target = match self.target.as_ref() {
            Some(d) => d.clone(),
            None => {
                let rooted_at = self.rooted_at.clone();
                let d = match walker::step_walk(rooted_at, self.path, self.cred, &__guard) {
                    StepOutcome::Done(d) => d,
                    StepOutcome::Err(e) => return StepOutcome::err(e),
                    _ => return StepOutcome::err(step_engine::Errno::EIO),
                };
                self.target = Some(d.clone());
                d
            }
        };
        let fs_ops = walker::fs_ops_for(&target, &__guard).expect("NoFsOps for ChownOp");
        fs_ops.chown_inode(
            target.rnode().fs_object_id(),
            self.uid,
            self.gid,
            self.cred,
            &__guard,
        )
    }
}

// ============================================================================
// AccessOp — faccessat / faccessat2
// ============================================================================

pub struct AccessOp<'a> {
    pub rooted_at: &'a Cap<DEntry>,
    pub path: &'a [u8],
    pub cred: &'a Credential,
}

impl<'a, I: SubjectIdentity> StepOp<I> for AccessOp<'a> {
    type Output = InodeMeta;
    type Progress = NoProgress;

    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<InodeMeta, NoProgress> {
        let __guard = step_engine::guard();
        let rooted_at = self.rooted_at.clone();
        match walker::step_walk(rooted_at, self.path, self.cred, &__guard) {
            StepOutcome::Done(d) => StepOutcome::done(d.rnode().meta()),
            StepOutcome::Err(e) => StepOutcome::err(e),
            _ => StepOutcome::err(step_engine::Errno::EIO),
        }
    }
}

impl OneShotStepOp<ProcessIdentity> for AccessOp<'_> {}
impl OneShotStepOp<crate::process::ProcessIdentity> for AccessOp<'_> {}

// ============================================================================
// MkdirOp — mkdirat
// ============================================================================

pub struct MkdirOp<'a> {
    pub rooted_at: &'a Cap<DEntry>,
    pub path: &'a [u8],
    pub mode: u16,
    pub cred: &'a Credential,
    pub parent: Option<(Cap<DEntry>, InlineName)>,
}

impl<'a, I: SubjectIdentity> StepOp<I> for MkdirOp<'a> {
    type Output = ();
    type Progress = NoProgress;

    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<(), NoProgress> {
        let __guard = step_engine::guard();
        // observe
        // upgrade — N/A: path-walk read-only
        // reserve — create_inode reserves zone slot
        // commit
        // publish — N/A: inode published via dentry
        let (parent, name) = match self.parent.as_ref() {
            Some((parent, name)) => (parent.clone(), *name),
            None => {
                let rooted_at = self.rooted_at.clone();
                let (parent_path, name_bytes) = split_parent_and_name(self.path);
                let parent_dentry = if parent_path.is_empty() {
                    rooted_at
                } else {
                    match walker::step_walk(rooted_at, parent_path, self.cred, &__guard) {
                        StepOutcome::Done(d) => d,
                        StepOutcome::Err(e) => return StepOutcome::err(e),
                        _ => return StepOutcome::err(step_engine::Errno::EIO),
                    }
                };
                let name = match InlineName::new(name_bytes) {
                    Ok(n) => n,
                    Err(_) => return StepOutcome::err(step_engine::Errno::ENAMETOOLONG),
                };
                self.parent = Some((parent_dentry.clone(), name));
                (parent_dentry, name)
            }
        };
        let fs_ops = walker::fs_ops_for(&parent, &__guard).expect("NoFsOps for MkdirOp");
        match fs_ops.mkdir(
            parent.rnode().fs_object_id(),
            name.as_bytes(),
            self.mode,
            self.cred,
            &__guard,
        ) {
            StepOutcome::Done(_id) => StepOutcome::Done(()),
            StepOutcome::Err(e) => StepOutcome::Err(e),
            StepOutcome::Continue { progress: _ } => StepOutcome::Continue {
                progress: NoProgress,
            },
            StepOutcome::Yield { progress: _, shape } => StepOutcome::Yield {
                progress: NoProgress,
                shape,
            },
        }
    }
}

impl OneShotStepOp<ProcessIdentity> for MkdirOp<'_> {}
impl OneShotStepOp<crate::process::ProcessIdentity> for MkdirOp<'_> {}

/// Composite StepOp: walk to parent dir, then call `FsOps::create_inode`
/// with an arbitrary `InodeKind` (used by `mknodat` for device nodes).
pub struct MknodOp<'a> {
    pub rooted_at: &'a Cap<DEntry>,
    pub path: &'a [u8],
    pub mode: u16,
    pub kind: InodeKind,
    pub cred: &'a Credential,
    pub parent: Option<(Cap<DEntry>, InlineName)>,
}

impl<'a, I: SubjectIdentity> StepOp<I> for MknodOp<'a> {
    type Output = ();
    type Progress = NoProgress;

    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<(), NoProgress> {
        let __guard = step_engine::guard();
        // observe
        // upgrade — N/A: path-walk read-only
        // reserve — create_inode reserves zone slot
        // commit
        // publish — N/A: inode published via dentry
        let (parent, name) = match self.parent.take() {
            Some(p) => p,
            None => {
                let rooted_at = self.rooted_at.clone();
                let (parent_path, name_bytes) = split_parent_and_name(self.path);
                let parent_dentry = if parent_path.is_empty() {
                    rooted_at
                } else {
                    match walker::step_walk(rooted_at, parent_path, self.cred, &__guard) {
                        StepOutcome::Done(d) => d,
                        StepOutcome::Err(e) => return StepOutcome::err(e),
                        _ => return StepOutcome::err(step_engine::Errno::EIO),
                    }
                };
                let name = match InlineName::new(name_bytes) {
                    Ok(n) => n,
                    Err(_) => return StepOutcome::err(step_engine::Errno::ENAMETOOLONG),
                };
                self.parent = Some((parent_dentry.clone(), name));
                (parent_dentry, name)
            }
        };
        let fs_ops = walker::fs_ops_for(&parent, &__guard).expect("NoFsOps for MknodOp");
        // `create_inode` takes a single packed `mode` whose S_IFMT bits
        // encode the inode kind (Linux convention); `self.kind` is held
        // only for callers that want a typed accessor.
        let _ = self.kind;
        match fs_ops.create_inode(
            parent.rnode().fs_object_id(),
            name.as_bytes(),
            self.mode,
            self.cred,
            &__guard,
        ) {
            StepOutcome::Done(_) => StepOutcome::Done(()),
            StepOutcome::Err(e) => StepOutcome::Err(e),
            StepOutcome::Continue { progress: _ } => StepOutcome::Continue {
                progress: NoProgress,
            },
            StepOutcome::Yield { progress: _, shape } => StepOutcome::Yield {
                progress: NoProgress,
                shape,
            },
        }
    }
}

// ============================================================================
// UnlinkOp — unlinkat
// ============================================================================

pub struct UnlinkOp<'a> {
    pub rooted_at: &'a Cap<DEntry>,
    pub path: &'a [u8],
    pub cred: &'a Credential,
    pub parent_and_child: Option<(Cap<DEntry>, InlineName, Cap<DEntry>)>,
}

impl<'a, I: SubjectIdentity> StepOp<I> for UnlinkOp<'a> {
    type Output = ();
    type Progress = NoProgress;

    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<(), NoProgress> {
        let __guard = step_engine::guard();
        let (parent, name, child) = match self.parent_and_child.take() {
            Some(p) => p,
            None => {
                let rooted_at = self.rooted_at.clone();
                let child_dentry = match super::resolution::driver::walk_to_completion(
                    rooted_at,
                    self.path,
                    super::resolution::state::WalkMode::EntityUnfollowed,
                    super::resolution::state::FinalSymlinkPolicy::NoFollow,
                    self.cred,
                    &__guard,
                ) {
                    Ok(resolved) => resolved.dentry,
                    Err(error) => return StepOutcome::err(error.into()),
                };
                let parent_dentry = child_dentry
                    .parent_hint()
                    .unwrap_or_else(|| child_dentry.clone());
                let child_name = child_dentry.name();
                self.parent_and_child =
                    Some((parent_dentry.clone(), child_name, child_dentry.clone()));
                (parent_dentry, child_name, child_dentry)
            }
        };
        let fs_ops = walker::fs_ops_for(&parent, &__guard).expect("NoFsOps for UnlinkOp");
        let outcome = fs_ops.unlink(
            parent.rnode().fs_object_id(),
            name.as_bytes(),
            child.rnode().fs_object_id(),
            &__guard,
        );
        if matches!(outcome, StepOutcome::Done(())) {
            parent.remove_cached_child(name);
        }
        outcome
    }
}

impl OneShotStepOp<ProcessIdentity> for UnlinkOp<'_> {}
impl OneShotStepOp<crate::process::ProcessIdentity> for UnlinkOp<'_> {}

// ============================================================================
// SymlinkOp — symlinkat
// ============================================================================

pub struct SymlinkOp<'a> {
    pub rooted_at: &'a Cap<DEntry>,
    pub target: &'a [u8],
    pub linkpath: &'a [u8],
    pub cred: &'a Credential,
    parent_and_name: Option<(Cap<DEntry>, InlineName)>,
}

impl<'a, I: SubjectIdentity> StepOp<I> for SymlinkOp<'a> {
    type Output = ();
    type Progress = NoProgress;

    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<(), NoProgress> {
        let __guard = step_engine::guard();
        let (parent, name) = match self.parent_and_name.take() {
            Some(p) => p,
            None => {
                let rooted_at = self.rooted_at.clone();
                let (parent_path, name_bytes) = split_parent_and_name(self.linkpath);
                let parent_dentry = if parent_path.is_empty() {
                    rooted_at
                } else {
                    match walker::step_walk(rooted_at, parent_path, self.cred, &__guard) {
                        StepOutcome::Done(d) => d,
                        StepOutcome::Err(e) => return StepOutcome::err(e),
                        _ => return StepOutcome::err(step_engine::Errno::EIO),
                    }
                };
                let name = match InlineName::new(name_bytes) {
                    Ok(n) => n,
                    Err(_) => return StepOutcome::err(step_engine::Errno::ENAMETOOLONG),
                };
                self.parent_and_name = Some((parent_dentry.clone(), name));
                (parent_dentry, name)
            }
        };
        let fs_ops = walker::fs_ops_for(&parent, &__guard).expect("NoFsOps for SymlinkOp");
        match fs_ops.symlink(
            parent.rnode().fs_object_id(),
            name.as_bytes(),
            self.target,
            self.cred,
            &__guard,
        ) {
            StepOutcome::Done(_id) => StepOutcome::Done(()),
            StepOutcome::Err(e) => StepOutcome::Err(e),
            StepOutcome::Continue { progress: _ } => StepOutcome::Continue {
                progress: NoProgress,
            },
            StepOutcome::Yield { progress: _, shape } => StepOutcome::Yield {
                progress: NoProgress,
                shape,
            },
        }
    }
}

impl OneShotStepOp<ProcessIdentity> for SymlinkOp<'_> {}
impl OneShotStepOp<crate::process::ProcessIdentity> for SymlinkOp<'_> {}

// ============================================================================
// LinkOp — linkat
// ============================================================================

pub struct LinkOp<'a> {
    pub rooted_at: &'a Cap<DEntry>,
    pub oldpath: &'a [u8],
    pub newpath: &'a [u8],
    pub cred: &'a Credential,
    pub state: Option<(FsObjectId, Cap<DEntry>, InlineName)>,
}

impl<'a, I: SubjectIdentity> StepOp<I> for LinkOp<'a> {
    type Output = ();
    type Progress = NoProgress;

    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<(), NoProgress> {
        let __guard = step_engine::guard();
        let (target_id, new_parent, new_name) = match self.state.take() {
            Some(p) => p,
            None => {
                let rooted_at = self.rooted_at.clone();
                let old_dentry =
                    match walker::step_walk(rooted_at.clone(), self.oldpath, self.cred, &__guard) {
                        StepOutcome::Done(d) => d,
                        StepOutcome::Err(e) => return StepOutcome::err(e),
                        _ => return StepOutcome::err(step_engine::Errno::EIO),
                    };
                let target_id = old_dentry.rnode().fs_object_id();
                let (new_parent_path, new_name_bytes) = split_parent_and_name(self.newpath);
                let new_parent = if new_parent_path.is_empty() {
                    rooted_at
                } else {
                    match walker::step_walk(rooted_at, new_parent_path, self.cred, &__guard) {
                        StepOutcome::Done(d) => d,
                        StepOutcome::Err(e) => return StepOutcome::err(e),
                        _ => return StepOutcome::err(step_engine::Errno::EIO),
                    }
                };
                let new_name = match InlineName::new(new_name_bytes) {
                    Ok(n) => n,
                    Err(_) => return StepOutcome::err(step_engine::Errno::ENAMETOOLONG),
                };
                self.state = Some((target_id, new_parent.clone(), new_name));
                (target_id, new_parent, new_name)
            }
        };
        let fs_ops = walker::fs_ops_for(&new_parent, &__guard).expect("NoFsOps for LinkOp");
        fs_ops.link(
            new_parent.rnode().fs_object_id(),
            new_name.as_bytes(),
            target_id,
            &__guard,
        )
    }
}

impl OneShotStepOp<ProcessIdentity> for LinkOp<'_> {}
impl OneShotStepOp<crate::process::ProcessIdentity> for LinkOp<'_> {}

// ============================================================================
// RenameOp — renameat2
// ============================================================================

pub struct RenameOp<'a> {
    pub rooted_at: &'a Cap<DEntry>,
    pub oldpath: &'a [u8],
    pub newpath: &'a [u8],
    pub cred: &'a Credential,
    /// Owned, pre-resolved retry state. Caps and inline names may cross a
    /// yield; guard-scoped witnesses and reservations are deliberately absent.
    pub state: Option<(Cap<DEntry>, InlineName, Cap<DEntry>, InlineName)>,
}

impl<'a, I: SubjectIdentity> StepOp<I> for RenameOp<'a> {
    type Output = ();
    type Progress = NoProgress;

    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<(), NoProgress> {
        let __guard = step_engine::guard();
        // observe
        // upgrade — N/A: path-walk read-only
        // reserve — N/A: rename reserves zone slots via FsOps
        // commit
        // publish — N/A: no signal attachments
        let (old_parent, old_name, new_parent, new_name) = match self.state.clone() {
            Some(p) => p,
            None => {
                let rooted_at = self.rooted_at.clone();
                let (old_parent_path, old_name_bytes) = split_parent_and_name(self.oldpath);
                let old_parent = if old_parent_path.is_empty() {
                    rooted_at.clone()
                } else {
                    match walker::step_walk(rooted_at.clone(), old_parent_path, self.cred, &__guard)
                    {
                        StepOutcome::Done(d) => d,
                        StepOutcome::Err(e) => return StepOutcome::err(e),
                        _ => return StepOutcome::err(step_engine::Errno::EIO),
                    }
                };
                let old_name = match InlineName::new(old_name_bytes) {
                    Ok(n) => n,
                    Err(_) => return StepOutcome::err(step_engine::Errno::ENAMETOOLONG),
                };
                let (new_parent_path, new_name_bytes) = split_parent_and_name(self.newpath);
                let new_parent = if new_parent_path.is_empty() {
                    rooted_at
                } else {
                    match walker::step_walk(rooted_at, new_parent_path, self.cred, &__guard) {
                        StepOutcome::Done(d) => d,
                        StepOutcome::Err(e) => return StepOutcome::err(e),
                        _ => return StepOutcome::err(step_engine::Errno::EIO),
                    }
                };
                let new_name = match InlineName::new(new_name_bytes) {
                    Ok(n) => n,
                    Err(_) => return StepOutcome::err(step_engine::Errno::ENAMETOOLONG),
                };
                self.state = Some((old_parent.clone(), old_name, new_parent.clone(), new_name));
                (old_parent, old_name, new_parent, new_name)
            }
        };
        let fs_ops = walker::fs_ops_for(&old_parent, &__guard).expect("NoFsOps for RenameOp");
        let outcome = fs_ops.rename(
            old_parent.rnode().fs_object_id(),
            old_name.as_bytes(),
            new_parent.rnode().fs_object_id(),
            new_name.as_bytes(),
            &__guard,
        );
        if matches!(outcome, StepOutcome::Done(())) {
            old_parent.remove_cached_child(old_name);
            new_parent.remove_cached_child(new_name);
        }
        outcome
    }
}

// ============================================================================
// TruncateOp — truncate / ftruncate
// ============================================================================

pub struct TruncateOp<'a> {
    pub rooted_at: &'a Cap<DEntry>,
    pub path: &'a [u8],
    pub length: u64,
    pub cred: &'a Credential,
    pub target: Option<Cap<DEntry>>,
}

impl<'a, I: SubjectIdentity> StepOp<I> for TruncateOp<'a> {
    type Output = ();
    type Progress = NoProgress;

    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<(), NoProgress> {
        let __guard = step_engine::guard();
        let target = match self.target.take() {
            Some(d) => d,
            None => {
                let rooted_at = self.rooted_at.clone();
                let d = match walker::step_walk(rooted_at, self.path, self.cred, &__guard) {
                    StepOutcome::Done(d) => d,
                    StepOutcome::Err(e) => return StepOutcome::err(e),
                    _ => return StepOutcome::err(step_engine::Errno::EIO),
                };
                self.target = Some(d.clone());
                d
            }
        };
        let page_backing = {
            let weak = target.rnode().containing_mount_weak();
            match weak.and_then(|w| w.upgrade(&__guard)) {
                Some(mp) => mp.fs_page_backing().clone(),
                None => return StepOutcome::err(step_engine::Errno::ENODEV),
            }
        };
        page_backing.truncate(target.rnode().fs_object_id(), self.length, &__guard)
    }
}
impl OneShotStepOp<ProcessIdentity> for TruncateOp<'_> {}
impl OneShotStepOp<crate::process::ProcessIdentity> for TruncateOp<'_> {}

// ============================================================================
// StatOp — newfstatat / fstat
// ============================================================================

pub struct StatOp<'a> {
    pub rooted_at: &'a Cap<DEntry>,
    pub path: &'a [u8],
    pub cred: &'a Credential,
    pub target: Option<Cap<DEntry>>,
}

impl<'a, I: SubjectIdentity> StepOp<I> for StatOp<'a> {
    type Output = (InodeMeta, FsObjectId);
    type Progress = NoProgress;

    fn step(
        &mut self,
        _ctx: &mut ScriptCtx<I>,
    ) -> StepOutcome<(InodeMeta, FsObjectId), NoProgress> {
        let __guard = step_engine::guard();
        let target = match self.target.take() {
            Some(d) => d,
            None => {
                let rooted_at = self.rooted_at.clone();
                let d = match walker::step_walk(rooted_at, self.path, self.cred, &__guard) {
                    StepOutcome::Done(d) => d,
                    StepOutcome::Err(e) => return StepOutcome::err(e),
                    _ => return StepOutcome::err(step_engine::Errno::EIO),
                };
                self.target = Some(d.clone());
                d
            }
        };
        let ino = target.rnode().fs_object_id();
        let meta = live_meta_for_dentry(&target, &__guard);
        StepOutcome::done((meta, ino))
    }
}

impl OneShotStepOp<ProcessIdentity> for StatOp<'_> {}
impl OneShotStepOp<crate::process::ProcessIdentity> for StatOp<'_> {}

// ============================================================================
// LstatOp — lstat
// ============================================================================

pub struct LstatOp<'a> {
    pub rooted_at: &'a Cap<DEntry>,
    pub path: &'a [u8],
    pub cred: &'a Credential,
    pub target: Option<Cap<DEntry>>,
}

impl<'a, I: SubjectIdentity> StepOp<I> for LstatOp<'a> {
    type Output = (InodeMeta, FsObjectId);
    type Progress = NoProgress;

    fn step(
        &mut self,
        _ctx: &mut ScriptCtx<I>,
    ) -> StepOutcome<(InodeMeta, FsObjectId), NoProgress> {
        let __guard = step_engine::guard();
        let target = match self.target.take() {
            Some(d) => d,
            None => {
                let rooted_at = self.rooted_at.clone();
                let d = match super::resolution::driver::walk_to_completion(
                    rooted_at,
                    self.path,
                    super::resolution::state::WalkMode::EntityUnfollowed,
                    super::resolution::state::FinalSymlinkPolicy::NoFollow,
                    self.cred,
                    &__guard,
                ) {
                    Ok(resolved) => resolved.dentry,
                    Err(e) => return StepOutcome::err(e.into()),
                };
                self.target = Some(d.clone());
                d
            }
        };
        let ino = target.rnode().fs_object_id();
        StepOutcome::done((live_meta_for_dentry(&target, &__guard), ino))
    }
}

impl OneShotStepOp<ProcessIdentity> for LstatOp<'_> {}
impl OneShotStepOp<crate::process::ProcessIdentity> for LstatOp<'_> {}

// ============================================================================
// StatxOp — statx
// ============================================================================

pub struct StatxOp<'a> {
    pub rooted_at: &'a Cap<DEntry>,
    pub path: &'a [u8],
    pub cred: &'a Credential,
}

#[derive(Clone, Debug)]
pub struct StatxResult {
    pub meta: InodeMeta,
}

impl<'a, I: SubjectIdentity> StepOp<I> for StatxOp<'a> {
    type Output = (StatxResult, FsObjectId);
    type Progress = NoProgress;

    fn step(
        &mut self,
        _ctx: &mut ScriptCtx<I>,
    ) -> StepOutcome<(StatxResult, FsObjectId), NoProgress> {
        let __guard = step_engine::guard();
        // `StatxOp` is a `OneShotStepOp`: `drive_oneshot` invokes this step
        // exactly once and rejects every non-terminal outcome.  Retaining a
        // second capability to the resolved dentry therefore cannot help a
        // retry, but it did add a capability refcount round trip to Cargo's
        // hottest metadata syscall.
        let rooted_at = self.rooted_at.clone();
        let target = match walker::step_walk(rooted_at, self.path, self.cred, &__guard) {
            StepOutcome::Done(d) => d,
            StepOutcome::Err(e) => return StepOutcome::err(e),
            _ => return StepOutcome::err(step_engine::Errno::EIO),
        };
        let ino = target.rnode().fs_object_id();
        let meta = live_meta_for_dentry(&target, &__guard);
        StepOutcome::done((StatxResult { meta }, ino))
    }
}

impl OneShotStepOp<ProcessIdentity> for StatxOp<'_> {}
impl OneShotStepOp<crate::process::ProcessIdentity> for StatxOp<'_> {}

// ============================================================================
// LstatxOp — statx with AT_SYMLINK_NOFOLLOW
// ============================================================================

pub struct LstatxOp<'a> {
    pub rooted_at: &'a Cap<DEntry>,
    pub path: &'a [u8],
    pub cred: &'a Credential,
    pub target: Option<Cap<DEntry>>,
}

impl<'a, I: SubjectIdentity> StepOp<I> for LstatxOp<'a> {
    type Output = (StatxResult, FsObjectId);
    type Progress = NoProgress;

    fn step(
        &mut self,
        _ctx: &mut ScriptCtx<I>,
    ) -> StepOutcome<(StatxResult, FsObjectId), NoProgress> {
        let __guard = step_engine::guard();
        let target = match self.target.take() {
            Some(d) => d,
            None => {
                let rooted_at = self.rooted_at.clone();
                let d = match super::resolution::driver::walk_to_completion(
                    rooted_at,
                    self.path,
                    super::resolution::state::WalkMode::EntityUnfollowed,
                    super::resolution::state::FinalSymlinkPolicy::NoFollow,
                    self.cred,
                    &__guard,
                ) {
                    Ok(resolved) => resolved.dentry,
                    Err(e) => return StepOutcome::err(e.into()),
                };
                self.target = Some(d.clone());
                d
            }
        };
        let ino = target.rnode().fs_object_id();
        let meta = live_meta_for_dentry(&target, &__guard);
        StepOutcome::done((StatxResult { meta }, ino))
    }
}

impl OneShotStepOp<ProcessIdentity> for LstatxOp<'_> {}
impl OneShotStepOp<crate::process::ProcessIdentity> for LstatxOp<'_> {}

fn live_meta_for_dentry(target: &Cap<DEntry>, guard: &crate::execution::Guard<'_>) -> InodeMeta {
    // Prefer the FS's live metadata so chmod/chown/truncate/write updates
    // surface through all stat-family calls. The cached RNode meta is the
    // snapshot from materialisation time.
    let ino = target.rnode().fs_object_id();
    let mut meta = match target.rnode().containing_mount_weak() {
        Some(weak) => match weak.upgrade(guard) {
            Some(payload) => match payload.fs_ops().load_inode_meta(ino, guard) {
                StepOutcome::Done(m) => m,
                _ => target.rnode().meta(),
            },
            None => target.rnode().meta(),
        },
        None => target.rnode().meta(),
    };
    if let RNodeBacking::PageBacked { pc } = target.rnode().backing() {
        meta.size = pc.size_bytes();
    }
    meta
}

// ============================================================================
// ReadLinkOp — readlinkat
// ============================================================================

pub struct ReadLinkOp<'a> {
    pub rooted_at: &'a Cap<DEntry>,
    pub path: &'a [u8],
    pub cred: &'a Credential,
    pub target: Option<Cap<DEntry>>,
}

impl<'a, I: SubjectIdentity> StepOp<I> for ReadLinkOp<'a> {
    type Output = Box<[u8]>;
    type Progress = NoProgress;

    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Box<[u8]>, NoProgress> {
        let __guard = step_engine::guard();
        let target = match self.target.take() {
            Some(d) => d,
            None => {
                let rooted_at = self.rooted_at.clone();
                let d = match walker::step_walk(rooted_at, self.path, self.cred, &__guard) {
                    StepOutcome::Done(d) => d,
                    StepOutcome::Err(e) => return StepOutcome::err(e),
                    _ => return StepOutcome::err(step_engine::Errno::EIO),
                };
                self.target = Some(d.clone());
                d
            }
        };
        let fs_ops = walker::fs_ops_for(&target, &__guard).expect("NoFsOps for ReadLinkOp");
        fs_ops.read_link(target.rnode().fs_object_id(), &__guard)
    }
}

impl OneShotStepOp<ProcessIdentity> for ReadLinkOp<'_> {}
impl OneShotStepOp<crate::process::ProcessIdentity> for ReadLinkOp<'_> {}

// ============================================================================
// Getdents64Op — getdents64
// ============================================================================

/// `getdents64` composite op: walk to directory, loop readdir into a
/// caller-supplied buffer as `linux_dirent64` records.
///
/// Each `step()` call fills as many entries as fit in `buf`.  When the
/// directory is exhausted (readdir returns `None`), the op returns
/// `Done(0)` to signal EOF.
pub struct Getdents64Op<'a> {
    pub rooted_at: &'a Cap<DEntry>,
    pub path: &'a [u8],
    pub cred: &'a Credential,
    /// Output buffer — caller provides.
    pub buf: &'a mut [u8],
    // Internal state
    target: Option<Cap<DEntry>>,
    cursor: u64,
    pos: usize,
}

/// Size of a `linux_dirent64` header (without the name).
const DIRENT64_HEADER_SIZE: usize = 19; // d_ino(8) + d_off(8) + d_reclen(2) + d_type(1)

impl<'a, I: SubjectIdentity> StepOp<I> for Getdents64Op<'a> {
    type Output = usize; // bytes written, 0 = EOF
    type Progress = NoProgress;

    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<usize, NoProgress> {
        let __guard = step_engine::guard();
        let target = match self.target.take() {
            Some(d) => {
                self.target = Some(d.clone());
                d
            }
            None => {
                let rooted_at = self.rooted_at.clone();
                let d = match walker::step_walk(rooted_at, self.path, self.cred, &__guard) {
                    StepOutcome::Done(d) => d,
                    StepOutcome::Err(e) => return StepOutcome::err(e),
                    _ => return StepOutcome::err(step_engine::Errno::EIO),
                };
                self.target = Some(d.clone());
                d
            }
        };

        let fs_ops = walker::fs_ops_for(&target, &__guard).expect("NoFsOps for Getdents64Op");
        let fs_object_id = target.rnode().fs_object_id();

        let cursor = crate::vfs::structure::DirCursor::from_u64(self.cursor);

        match fs_ops.readdir(fs_object_id, cursor, &__guard) {
            StepOutcome::Done(Some((entry, next_cursor))) => {
                let name_bytes = entry.name.as_bytes();
                let reclen = DIRENT64_HEADER_SIZE + name_bytes.len() + 1; // +1 for null terminator
                let aligned = (reclen + 7) & !7; // 8-byte aligned

                if self.pos + aligned > self.buf.len() {
                    // Buffer full — save cursor for next call.
                    self.cursor = cursor.as_u64();
                    return StepOutcome::done(self.pos);
                }

                // Write linux_dirent64 record.
                let buf = &mut self.buf[self.pos..];
                // d_ino
                buf[0..8].copy_from_slice(&entry.fs_object_id.as_u64().to_le_bytes());
                // d_off
                buf[8..16].copy_from_slice(&next_cursor.as_u64().to_le_bytes());
                // d_reclen
                buf[16..18].copy_from_slice(&(aligned as u16).to_le_bytes());
                // d_type
                buf[18] = match entry.kind {
                    crate::vfs::structure::InodeKind::Regular => 8u8, // DT_REG
                    crate::vfs::structure::InodeKind::Directory => 4u8, // DT_DIR
                    crate::vfs::structure::InodeKind::Symlink => 10u8, // DT_LNK
                    crate::vfs::structure::InodeKind::CharDevice => 2u8, // DT_CHR
                    crate::vfs::structure::InodeKind::BlockDevice => 6u8, // DT_BLK
                    crate::vfs::structure::InodeKind::Fifo => 1u8,    // DT_FIFO
                    crate::vfs::structure::InodeKind::Socket => 12u8, // DT_SOCK
                };
                // d_name
                buf[19..19 + name_bytes.len()].copy_from_slice(name_bytes);
                buf[19 + name_bytes.len()] = 0; // null terminator

                self.pos += aligned;
                self.cursor = next_cursor.as_u64();
                StepOutcome::done(self.pos)
            }
            StepOutcome::Done(None) => {
                // End of directory.
                let written = self.pos;
                self.pos = 0; // reset for potential re-read
                StepOutcome::done(written)
            }
            StepOutcome::Err(e) => StepOutcome::err(e),
            StepOutcome::Continue { .. } => StepOutcome::done(self.pos),
            StepOutcome::Yield { shape, .. } => StepOutcome::Yield {
                progress: NoProgress,
                shape,
            },
        }
    }
}

// ============================================================================
// Getdents64FdOp — fd-based getdents64
// ============================================================================

/// `getdents64` fd-based op: loops `FsOps::readdir` into a
/// caller-supplied buffer as `linux_dirent64` records.
///
/// Takes an already-opened `OpenFile` cap (resolved from fd by the
/// syscall layer).  Each `step()` fills one entry; EOF returns 0.
pub struct Getdents64FdOp<'a> {
    pub file: &'a Cap<crate::vfs::structure::OpenFile>,
    pub buf: &'a mut [u8],
    cursor: u64,
    pos: usize,
}

impl<'a, I: SubjectIdentity> StepOp<I> for Getdents64FdOp<'a> {
    type Output = usize;
    type Progress = NoProgress;

    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<usize, NoProgress> {
        let __guard = step_engine::guard();
        let rnode = self.file.rnode();
        let fs_object_id = rnode.fs_object_id();
        let fs_ops = walker::fs_ops_for_rnode(rnode, &__guard).expect("NoFsOps for Getdents64FdOp");

        let cursor = crate::vfs::structure::DirCursor::from_u64(self.cursor);

        match fs_ops.readdir(fs_object_id, cursor, &__guard) {
            StepOutcome::Done(Some((entry, next_cursor))) => {
                let name_bytes = entry.name.as_bytes();
                let reclen = 19 + name_bytes.len() + 1;
                let aligned = (reclen + 7) & !7;

                if self.pos + aligned > self.buf.len() {
                    self.cursor = cursor.as_u64();
                    return StepOutcome::done(self.pos);
                }

                let buf = &mut self.buf[self.pos..];
                buf[0..8].copy_from_slice(&entry.fs_object_id.as_u64().to_le_bytes());
                buf[8..16].copy_from_slice(&next_cursor.as_u64().to_le_bytes());
                buf[16..18].copy_from_slice(&(aligned as u16).to_le_bytes());
                buf[18] = match entry.kind {
                    crate::vfs::structure::InodeKind::Regular => 8u8,
                    crate::vfs::structure::InodeKind::Directory => 4u8,
                    crate::vfs::structure::InodeKind::Symlink => 10u8,
                    crate::vfs::structure::InodeKind::CharDevice => 2u8,
                    crate::vfs::structure::InodeKind::BlockDevice => 6u8,
                    crate::vfs::structure::InodeKind::Fifo => 1u8,
                    crate::vfs::structure::InodeKind::Socket => 12u8,
                };
                buf[19..19 + name_bytes.len()].copy_from_slice(name_bytes);
                buf[19 + name_bytes.len()] = 0;

                self.pos += aligned;
                self.cursor = next_cursor.as_u64();
                StepOutcome::done(self.pos)
            }
            StepOutcome::Done(None) => {
                let written = self.pos;
                self.pos = 0;
                StepOutcome::done(written)
            }
            StepOutcome::Err(e) => StepOutcome::err(e),
            StepOutcome::Continue { .. } => StepOutcome::done(self.pos),
            StepOutcome::Yield { shape, .. } => StepOutcome::Yield {
                progress: NoProgress,
                shape,
            },
        }
    }
}

// ============================================================================
// PpollOp — ppoll (single-fd v1)
// ============================================================================

/// `ppoll` composite op: block until one of the registered fds is
/// readable/writable, or a timeout expires.
///
/// v1: single fd only.  Yields [`YieldShape::OnWaitSource`] with the
/// fd's wait-source id; `drive()` parks on the reactor mailbox until
/// the fd's `WaitSource` fires, then returns `Done(1)` (ready).
pub struct PpollOp {
    /// `WaitSourceId` of the fd to park on.
    pub wait_source_id: WaitSourceId,
    /// Interest mask for the wait registration.
    pub interests: InterestMask,
    /// Timeout in milliseconds, or `None` for infinite.
    pub timeout_ms: Option<u64>,
    /// Private: set by step().
    pub started: bool,
}

impl<I: SubjectIdentity> StepOp<I> for PpollOp {
    type Output = usize;
    type Progress = NoProgress;

    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<usize, NoProgress> {
        if !self.started {
            self.started = true;
            // drive-taskmb: yield OnWaitSource. drive()'s
            // resolve_on_wait_source registers the task mailbox
            // with the fd's WaitSource, parks, and resumes when
            // the fd becomes readable.
            if self.timeout_ms.is_some() {
                // Timeout: for now, yield without timeout (the
                // Deadline-domain expiry needs plumbing).
                // Future: combine OnWaitSource + OnTimer via
                // composite yield.
                return notification::ppoll_wait(self.wait_source_id, self.interests);
            }
            // Infinite timeout — yield and park until the fd fires.
            return notification::ppoll_wait(self.wait_source_id, self.interests);
        }
        // Resumed after wake: fd is ready.
        StepOutcome::done(1)
    }

    fn apply_resume(&mut self, resume: ResumeOutcome) -> Result<(), step_engine::Errno> {
        match resume {
            ResumeOutcome::Retry => Ok(()),
            _ => Err(step_engine::Errno::EINVAL),
        }
    }
}

// ============================================================================
// NanosleepOp — nanosleep / clock_nanosleep
// ============================================================================

/// `nanosleep` composite op: suspend the calling task for at least
/// the requested duration.
///
/// v1 stub: returns `ENOSYS` (timer infrastructure not yet wired).
pub struct NanosleepOp {
    /// Requested sleep duration in nanoseconds.
    pub nanos: u64,
    /// Absolute deadline in nanoseconds (platform timebase).
    pub deadline_ns: u64,
    pub started: bool,
}

impl<I: SubjectIdentity> StepOp<I> for NanosleepOp {
    type Output = ();
    type Progress = NoProgress;

    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<(), NoProgress> {
        if !self.started {
            self.started = true;
            return StepOutcome::Yield {
                progress: NoProgress,
                shape: YieldShape::OnTimer {
                    token: step_engine::TimerId::new(1),
                    deadline: Deadline::from_raw(self.deadline_ns),
                },
            };
        }
        StepOutcome::done(())
    }

    /// Accept TimerExpired from resolve_on_timer (default impl rejects
    /// non-Retry resumes with EINVAL).
    fn apply_resume(&mut self, resume: ResumeOutcome) -> Result<(), step_engine::Errno> {
        match resume {
            ResumeOutcome::Retry | ResumeOutcome::TimerExpired(_) => Ok(()),
            _ => Err(step_engine::Errno::EINVAL),
        }
    }
}

// ============================================================================
// Shared helpers
// ============================================================================

fn split_parent_and_name(path: &[u8]) -> (&[u8], &[u8]) {
    match path.iter().rposition(|&b| b == b'/') {
        None => (b"", path),
        Some(pos) => (&path[..pos], &path[pos + 1..]),
    }
}
