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
//! | ChmodOp | fchmodat | FsOps::step_chmod |
//! | ChownOp | fchownat | FsOps::step_chown |
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

use crate::execution::Guard;
use crate::page_backed::FsPageBacking;
use crate::vfs::adapter::step_engine::{
    self, Cap, NoProgress, OneShotStepOp, ProcessIdentity, ScriptCtx, StepOp, StepOutcome,
    SubjectIdentity,
};
use crate::vfs::{Credential, DEntry, FsObjectId, FsOps, InlineName, InodeMeta};
use crate::vfs::walker;

// ============================================================================
// ChmodOp — fchmodat
// ============================================================================

pub struct ChmodOp<'a> {
    pub rooted_at: &'a Cap<DEntry>,
    pub path: &'a [u8],
    pub mode: u16,
    pub cred: &'a Credential,
    pub guard: &'a Guard<'a>,
    target: Option<Cap<DEntry>>,
}

impl<'a, I: SubjectIdentity> StepOp<I> for ChmodOp<'a> {
    type Output = ();
    type Progress = NoProgress;

    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<(), NoProgress> {
        let target = match self.target.take() {
            Some(d) => d,
            None => {
                let rooted_at = self.rooted_at.clone();
                let d = match walker::step_walk(rooted_at, self.path, self.cred, self.guard) {
                    StepOutcome::Done(d) => d,
                    StepOutcome::Err(e) => return StepOutcome::err(e),
                    _ => return StepOutcome::err(step_engine::Errno::EIO),
                };
                self.target = Some(d.clone());
                d
            }
        };
        let fs_ops =
            walker::fs_ops_for(&target, self.guard).expect("NoFsOps for ChmodOp");
        fs_ops.step_chmod(target.rnode().fs_object_id(), self.mode, self.cred, self.guard)
    }
}

impl OneShotStepOp<ProcessIdentity> for ChmodOp<'_> {}

// ============================================================================
// ChownOp — fchownat
// ============================================================================

pub struct ChownOp<'a> {
    pub rooted_at: &'a Cap<DEntry>,
    pub path: &'a [u8],
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub cred: &'a Credential,
    pub guard: &'a Guard<'a>,
    target: Option<Cap<DEntry>>,
}

impl<'a, I: SubjectIdentity> StepOp<I> for ChownOp<'a> {
    type Output = ();
    type Progress = NoProgress;

    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<(), NoProgress> {
        let target = match self.target.take() {
            Some(d) => d,
            None => {
                let rooted_at = self.rooted_at.clone();
                let d = match walker::step_walk(rooted_at, self.path, self.cred, self.guard) {
                    StepOutcome::Done(d) => d,
                    StepOutcome::Err(e) => return StepOutcome::err(e),
                    _ => return StepOutcome::err(step_engine::Errno::EIO),
                };
                self.target = Some(d.clone());
                d
            }
        };
        let fs_ops =
            walker::fs_ops_for(&target, self.guard).expect("NoFsOps for ChownOp");
        fs_ops.step_chown(
            target.rnode().fs_object_id(),
            self.uid,
            self.gid,
            self.cred,
            self.guard,
        )
    }
}

impl OneShotStepOp<ProcessIdentity> for ChownOp<'_> {}

// ============================================================================
// AccessOp — faccessat / faccessat2
// ============================================================================

pub struct AccessOp<'a> {
    pub rooted_at: &'a Cap<DEntry>,
    pub path: &'a [u8],
    pub cred: &'a Credential,
    pub guard: &'a Guard<'a>,
}

impl<'a, I: SubjectIdentity> StepOp<I> for AccessOp<'a> {
    type Output = ();
    type Progress = NoProgress;

    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<(), NoProgress> {
        let rooted_at = self.rooted_at.clone();
        match walker::step_walk(rooted_at, self.path, self.cred, self.guard) {
            StepOutcome::Done(_d) => StepOutcome::done(()),
            StepOutcome::Err(e) => StepOutcome::err(e),
            _ => StepOutcome::err(step_engine::Errno::EIO),
        }
    }
}

impl OneShotStepOp<ProcessIdentity> for AccessOp<'_> {}

// ============================================================================
// MkdirOp — mkdirat
// ============================================================================

pub struct MkdirOp<'a> {
    pub rooted_at: &'a Cap<DEntry>,
    pub path: &'a [u8],
    pub mode: u16,
    pub cred: &'a Credential,
    pub guard: &'a Guard<'a>,
    parent: Option<(Cap<DEntry>, InlineName)>,
}

impl<'a, I: SubjectIdentity> StepOp<I> for MkdirOp<'a> {
    type Output = ();
    type Progress = NoProgress;

    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<(), NoProgress> {
        let (parent, name) = match self.parent.take() {
            Some(p) => p,
            None => {
                let rooted_at = self.rooted_at.clone();
                let (parent_path, name_bytes) = split_parent_and_name(self.path);
                let parent_dentry = if parent_path.is_empty() {
                    rooted_at
                } else {
                    match walker::step_walk(rooted_at, parent_path, self.cred, self.guard) {
                        StepOutcome::Done(d) => d,
                        StepOutcome::Err(e) => return StepOutcome::err(e),
                        _ => return StepOutcome::err(step_engine::Errno::EIO),
                    }
                };
                let name = match InlineName::new(name_bytes) {
                    Ok(n) => n,
                    Err(_) => return StepOutcome::err(step_engine::Errno::ENAMETOOLONG),
                };
                self.parent = Some((parent_dentry.clone(), name.clone()));
                (parent_dentry, name)
            }
        };
        let fs_ops =
            walker::fs_ops_for(&parent, self.guard).expect("NoFsOps for MkdirOp");
        match fs_ops.mkdir(
            parent.rnode().fs_object_id(),
            name.as_bytes(),
            self.mode,
            self.cred,
            self.guard,
        ) {
            StepOutcome::Done(_id) => StepOutcome::Done(()),
            StepOutcome::Err(e) => StepOutcome::Err(e),
            StepOutcome::Continue { progress: _ } => StepOutcome::Continue { progress: NoProgress },
            StepOutcome::Yield { progress: _, shape } => StepOutcome::Yield { progress: NoProgress, shape },
        }
    }
}

impl OneShotStepOp<ProcessIdentity> for MkdirOp<'_> {}

// ============================================================================
// UnlinkOp — unlinkat
// ============================================================================

pub struct UnlinkOp<'a> {
    pub rooted_at: &'a Cap<DEntry>,
    pub path: &'a [u8],
    pub cred: &'a Credential,
    pub guard: &'a Guard<'a>,
    parent_and_child: Option<(Cap<DEntry>, InlineName, FsObjectId)>,
}

impl<'a, I: SubjectIdentity> StepOp<I> for UnlinkOp<'a> {
    type Output = ();
    type Progress = NoProgress;

    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<(), NoProgress> {
        let (parent, name, child_id) = match self.parent_and_child.take() {
            Some(p) => p,
            None => {
                let rooted_at = self.rooted_at.clone();
                let child_dentry =
                    match walker::step_walk(rooted_at, self.path, self.cred, self.guard) {
                        StepOutcome::Done(d) => d,
                        StepOutcome::Err(e) => return StepOutcome::err(e),
                        _ => return StepOutcome::err(step_engine::Errno::EIO),
                    };
                let parent_dentry = child_dentry
                    .parent_hint()
                    .unwrap_or_else(|| child_dentry.clone());
                let child_name = child_dentry.name().clone();
                let child_id = child_dentry.rnode().fs_object_id();
                self.parent_and_child =
                    Some((parent_dentry.clone(), child_name.clone(), child_id));
                (parent_dentry, child_name, child_id)
            }
        };
        let fs_ops =
            walker::fs_ops_for(&parent, self.guard).expect("NoFsOps for UnlinkOp");
        fs_ops.unlink(
            parent.rnode().fs_object_id(),
            name.as_bytes(),
            child_id,
            self.guard,
        )
    }
}

impl OneShotStepOp<ProcessIdentity> for UnlinkOp<'_> {}

// ============================================================================
// SymlinkOp — symlinkat
// ============================================================================

pub struct SymlinkOp<'a> {
    pub rooted_at: &'a Cap<DEntry>,
    pub target: &'a [u8],
    pub linkpath: &'a [u8],
    pub cred: &'a Credential,
    pub guard: &'a Guard<'a>,
    parent_and_name: Option<(Cap<DEntry>, InlineName)>,
}

impl<'a, I: SubjectIdentity> StepOp<I> for SymlinkOp<'a> {
    type Output = ();
    type Progress = NoProgress;

    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<(), NoProgress> {
        let (parent, name) = match self.parent_and_name.take() {
            Some(p) => p,
            None => {
                let rooted_at = self.rooted_at.clone();
                let (parent_path, name_bytes) = split_parent_and_name(self.linkpath);
                let parent_dentry = if parent_path.is_empty() {
                    rooted_at
                } else {
                    match walker::step_walk(rooted_at, parent_path, self.cred, self.guard) {
                        StepOutcome::Done(d) => d,
                        StepOutcome::Err(e) => return StepOutcome::err(e),
                        _ => return StepOutcome::err(step_engine::Errno::EIO),
                    }
                };
                let name = match InlineName::new(name_bytes) {
                    Ok(n) => n,
                    Err(_) => return StepOutcome::err(step_engine::Errno::ENAMETOOLONG),
                };
                self.parent_and_name = Some((parent_dentry.clone(), name.clone()));
                (parent_dentry, name)
            }
        };
        let fs_ops =
            walker::fs_ops_for(&parent, self.guard).expect("NoFsOps for SymlinkOp");
        match fs_ops.symlink(
            parent.rnode().fs_object_id(),
            name.as_bytes(),
            self.target,
            self.cred,
            self.guard,
        ) {
            StepOutcome::Done(_id) => StepOutcome::Done(()),
            StepOutcome::Err(e) => StepOutcome::Err(e),
            StepOutcome::Continue { progress: _ } => StepOutcome::Continue { progress: NoProgress },
            StepOutcome::Yield { progress: _, shape } => StepOutcome::Yield { progress: NoProgress, shape },
        }
    }
}

impl OneShotStepOp<ProcessIdentity> for SymlinkOp<'_> {}

// ============================================================================
// LinkOp — linkat
// ============================================================================

pub struct LinkOp<'a> {
    pub rooted_at: &'a Cap<DEntry>,
    pub oldpath: &'a [u8],
    pub newpath: &'a [u8],
    pub cred: &'a Credential,
    pub guard: &'a Guard<'a>,
    state: Option<(FsObjectId, Cap<DEntry>, InlineName)>,
}

impl<'a, I: SubjectIdentity> StepOp<I> for LinkOp<'a> {
    type Output = ();
    type Progress = NoProgress;

    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<(), NoProgress> {
        let (target_id, new_parent, new_name) = match self.state.take() {
            Some(p) => p,
            None => {
                let rooted_at = self.rooted_at.clone();
                let old_dentry = match walker::step_walk(
                    rooted_at.clone(),
                    self.oldpath,
                    self.cred,
                    self.guard,
                ) {
                    StepOutcome::Done(d) => d,
                    StepOutcome::Err(e) => return StepOutcome::err(e),
                    _ => return StepOutcome::err(step_engine::Errno::EIO),
                };
                let target_id = old_dentry.rnode().fs_object_id();
                let (new_parent_path, new_name_bytes) = split_parent_and_name(self.newpath);
                let new_parent = if new_parent_path.is_empty() {
                    rooted_at
                } else {
                    match walker::step_walk(rooted_at, new_parent_path, self.cred, self.guard) {
                        StepOutcome::Done(d) => d,
                        StepOutcome::Err(e) => return StepOutcome::err(e),
                        _ => return StepOutcome::err(step_engine::Errno::EIO),
                    }
                };
                let new_name = match InlineName::new(new_name_bytes) {
                    Ok(n) => n,
                    Err(_) => return StepOutcome::err(step_engine::Errno::ENAMETOOLONG),
                };
                self.state = Some((target_id, new_parent.clone(), new_name.clone()));
                (target_id, new_parent, new_name)
            }
        };
        let fs_ops =
            walker::fs_ops_for(&new_parent, self.guard).expect("NoFsOps for LinkOp");
        fs_ops.link(
            new_parent.rnode().fs_object_id(),
            new_name.as_bytes(),
            target_id,
            self.guard,
        )
    }
}

impl OneShotStepOp<ProcessIdentity> for LinkOp<'_> {}

// ============================================================================
// RenameOp — renameat2
// ============================================================================

pub struct RenameOp<'a> {
    pub rooted_at: &'a Cap<DEntry>,
    pub oldpath: &'a [u8],
    pub newpath: &'a [u8],
    pub cred: &'a Credential,
    pub guard: &'a Guard<'a>,
    state: Option<(Cap<DEntry>, InlineName, Cap<DEntry>, InlineName)>,
}

impl<'a, I: SubjectIdentity> StepOp<I> for RenameOp<'a> {
    type Output = ();
    type Progress = NoProgress;

    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<(), NoProgress> {
        let (old_parent, old_name, new_parent, new_name) = match self.state.take() {
            Some(p) => p,
            None => {
                let rooted_at = self.rooted_at.clone();
                let (old_parent_path, old_name_bytes) = split_parent_and_name(self.oldpath);
                let old_parent = if old_parent_path.is_empty() {
                    rooted_at.clone()
                } else {
                    match walker::step_walk(
                        rooted_at.clone(),
                        old_parent_path,
                        self.cred,
                        self.guard,
                    ) {
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
                    match walker::step_walk(rooted_at, new_parent_path, self.cred, self.guard) {
                        StepOutcome::Done(d) => d,
                        StepOutcome::Err(e) => return StepOutcome::err(e),
                        _ => return StepOutcome::err(step_engine::Errno::EIO),
                    }
                };
                let new_name = match InlineName::new(new_name_bytes) {
                    Ok(n) => n,
                    Err(_) => return StepOutcome::err(step_engine::Errno::ENAMETOOLONG),
                };
                self.state = Some((
                    old_parent.clone(),
                    old_name.clone(),
                    new_parent.clone(),
                    new_name.clone(),
                ));
                (old_parent, old_name, new_parent, new_name)
            }
        };
        let fs_ops =
            walker::fs_ops_for(&old_parent, self.guard).expect("NoFsOps for RenameOp");
        fs_ops.rename(
            old_parent.rnode().fs_object_id(),
            old_name.as_bytes(),
            new_parent.rnode().fs_object_id(),
            new_name.as_bytes(),
            self.guard,
        )
    }
}

impl OneShotStepOp<ProcessIdentity> for RenameOp<'_> {}

// ============================================================================
// TruncateOp — truncate / ftruncate
// ============================================================================

pub struct TruncateOp<'a> {
    pub rooted_at: &'a Cap<DEntry>,
    pub path: &'a [u8],
    pub length: u64,
    pub cred: &'a Credential,
    pub guard: &'a Guard<'a>,
    target: Option<Cap<DEntry>>,
}

impl<'a, I: SubjectIdentity> StepOp<I> for TruncateOp<'a> {
    type Output = ();
    type Progress = NoProgress;

    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<(), NoProgress> {
        let target = match self.target.take() {
            Some(d) => d,
            None => {
                let rooted_at = self.rooted_at.clone();
                let d = match walker::step_walk(rooted_at, self.path, self.cred, self.guard) {
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
            match weak.and_then(|w| w.upgrade(self.guard)) {
                Some(mp) => mp.fs_page_backing().clone(),
                None => return StepOutcome::err(step_engine::Errno::ENODEV),
            }
        };
        page_backing.truncate(target.rnode().fs_object_id(), self.length, self.guard)
    }
}

// ============================================================================
// StatOp — newfstatat / fstat
// ============================================================================

pub struct StatOp<'a> {
    pub rooted_at: &'a Cap<DEntry>,
    pub path: &'a [u8],
    pub cred: &'a Credential,
    pub guard: &'a Guard<'a>,
    target: Option<Cap<DEntry>>,
}

impl<'a, I: SubjectIdentity> StepOp<I> for StatOp<'a> {
    type Output = InodeMeta;
    type Progress = NoProgress;

    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<InodeMeta, NoProgress> {
        let target = match self.target.take() {
            Some(d) => d,
            None => {
                let rooted_at = self.rooted_at.clone();
                let d = match walker::step_walk(rooted_at, self.path, self.cred, self.guard) {
                    StepOutcome::Done(d) => d,
                    StepOutcome::Err(e) => return StepOutcome::err(e),
                    _ => return StepOutcome::err(step_engine::Errno::EIO),
                };
                self.target = Some(d.clone());
                d
            }
        };
        StepOutcome::done(target.rnode().meta())
    }
}

impl OneShotStepOp<ProcessIdentity> for StatOp<'_> {}

// ============================================================================
// LstatOp — lstat
// ============================================================================

pub struct LstatOp<'a> {
    pub rooted_at: &'a Cap<DEntry>,
    pub path: &'a [u8],
    pub cred: &'a Credential,
    pub guard: &'a Guard<'a>,
    target: Option<Cap<DEntry>>,
}

impl<'a, I: SubjectIdentity> StepOp<I> for LstatOp<'a> {
    type Output = InodeMeta;
    type Progress = NoProgress;

    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<InodeMeta, NoProgress> {
        // v1: same as StatOp — the synchronous walker always follows
        // symlinks.  When the state-machine walker supports
        // FinalSymlinkPolicy::NoFollow, this will diverge.
        let target = match self.target.take() {
            Some(d) => d,
            None => {
                let rooted_at = self.rooted_at.clone();
                let d = match walker::step_walk(rooted_at, self.path, self.cred, self.guard) {
                    StepOutcome::Done(d) => d,
                    StepOutcome::Err(e) => return StepOutcome::err(e),
                    _ => return StepOutcome::err(step_engine::Errno::EIO),
                };
                self.target = Some(d.clone());
                d
            }
        };
        StepOutcome::done(target.rnode().meta())
    }
}

impl OneShotStepOp<ProcessIdentity> for LstatOp<'_> {}

// ============================================================================
// StatxOp — statx
// ============================================================================

pub struct StatxOp<'a> {
    pub rooted_at: &'a Cap<DEntry>,
    pub path: &'a [u8],
    pub cred: &'a Credential,
    pub guard: &'a Guard<'a>,
    target: Option<Cap<DEntry>>,
}

#[derive(Clone, Debug)]
pub struct StatxResult {
    pub meta: InodeMeta,
}

impl<'a, I: SubjectIdentity> StepOp<I> for StatxOp<'a> {
    type Output = StatxResult;
    type Progress = NoProgress;

    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<StatxResult, NoProgress> {
        let target = match self.target.take() {
            Some(d) => d,
            None => {
                let rooted_at = self.rooted_at.clone();
                let d = match walker::step_walk(rooted_at, self.path, self.cred, self.guard) {
                    StepOutcome::Done(d) => d,
                    StepOutcome::Err(e) => return StepOutcome::err(e),
                    _ => return StepOutcome::err(step_engine::Errno::EIO),
                };
                self.target = Some(d.clone());
                d
            }
        };
        StepOutcome::done(StatxResult {
            meta: target.rnode().meta(),
        })
    }
}

impl OneShotStepOp<ProcessIdentity> for StatxOp<'_> {}

// ============================================================================
// ReadLinkOp — readlinkat
// ============================================================================

pub struct ReadLinkOp<'a> {
    pub rooted_at: &'a Cap<DEntry>,
    pub path: &'a [u8],
    pub cred: &'a Credential,
    pub guard: &'a Guard<'a>,
    target: Option<Cap<DEntry>>,
}

impl<'a, I: SubjectIdentity> StepOp<I> for ReadLinkOp<'a> {
    type Output = Box<[u8]>;
    type Progress = NoProgress;

    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Box<[u8]>, NoProgress> {
        let target = match self.target.take() {
            Some(d) => d,
            None => {
                let rooted_at = self.rooted_at.clone();
                let d = match walker::step_walk(rooted_at, self.path, self.cred, self.guard) {
                    StepOutcome::Done(d) => d,
                    StepOutcome::Err(e) => return StepOutcome::err(e),
                    _ => return StepOutcome::err(step_engine::Errno::EIO),
                };
                self.target = Some(d.clone());
                d
            }
        };
        let fs_ops =
            walker::fs_ops_for(&target, self.guard).expect("NoFsOps for ReadLinkOp");
        fs_ops.read_link(target.rnode().fs_object_id(), self.guard)
    }
}

impl OneShotStepOp<ProcessIdentity> for ReadLinkOp<'_> {}

// ============================================================================
// Shared helpers
// ============================================================================

fn split_parent_and_name(path: &[u8]) -> (&[u8], &[u8]) {
    match path.iter().rposition(|&b| b == b'/') {
        None => (b"", path),
        Some(pos) => (&path[..pos], &path[pos + 1..]),
    }
}
