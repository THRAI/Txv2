//! VFS subsystem facade per `SUBSYSTEM_ANATOMY_v2_1` four-module layout.
//!
//! - `structure/` owns durable value vocabulary and zone-allocated live
//!   nodes (DEntry, RNode, OpenFile).
//! - `checks/` owns pure observation contexts and path-resolution
//!   witnesses (RootCtx, ResolveCtx, EntityAtPath, ...).
//! - `execution/` owns the FsOps backend trait, the mount-time
//!   MountOutput value, and the OpenFile step bodies.
//!
//! All public types are re-exported here so external imports keep using
//! `crate::vfs::FooBar` paths.

pub mod checks;
pub mod execution;
pub mod structure;
pub mod walker;

#[cfg(test)]
mod tests;

pub use crate::cred::CapabilitySet;
pub use checks::{DirectoryAtPath, EntityAtPath, ParentAndName, ResolveCtx, RootCtx};
pub use execution::{FsOpsV3, MountOutput};
pub use structure::{
    render_dentry_path, Credential, DEntry, DirCursor, DirEntry, FsObjectId, InlineName, InodeKind,
    InodeMeta, OpenFile, OpenFileFlags, OpenFileIoctl, OpenFileIoctlCaller, OpenFileIoctlResult,
    RNode, RNodeBacking, StructPayload, Timespec, VfsName, S_IFBLK, S_IFCHR, S_IFDIR, S_IFIFO,
    S_IFLNK, S_IFMT, S_IFREG, S_IFSOCK, S_ISGID, S_ISUID, S_ISVTX, VFS_NAME_MAX,
};
pub use walker::{step_open_v3, step_walk_v3, SYMLOOP_MAX};
