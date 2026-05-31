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

pub mod adapter;
pub mod checks;
pub mod composite;
pub mod execution;
pub mod notification;
pub mod predicates;
pub mod require;
pub mod resolution;
pub mod structure;
pub mod walker;
pub mod xattr;

#[cfg(test)]
mod tests;

pub use crate::cred::CapabilitySet;
pub use checks::{DirectoryAtPath, EntityAtPath, ParentAndName, ResolveCtx, RootCtx};
pub use execution::{
    FileFsyncOp, FlockOp, FsOps, InodeStatOp, MountOutput, OpenFileGetFlOp, OpenFileIoctlOp,
    OpenFileLseekOp, OpenFileSetFlOp, OpenOp, PathWalkOp, ProjectedWriteContext,
};
pub use notification::{VFS_READABLE, VFS_WRITABLE};
pub use structure::{
    render_dentry_path, Credential, DEntry, DirCursor, DirEntry, FsNotifyInstance, FsNotifyKind,
    FsObjectId, InlineName, InodeKind, InodeMeta, OpenFile, OpenFileFlags, OpenFileIoctl,
    OpenFileIoctlCaller, OpenFileIoctlResult, ProjectionKey, ProjectionSchemaId, RNode,
    RNodeBacking, StructPayload, Timespec, VfsName, S_IFBLK, S_IFCHR, S_IFDIR, S_IFIFO, S_IFLNK,
    S_IFMT, S_IFREG, S_IFSOCK, S_ISGID, S_ISUID, S_ISVTX, VFS_NAME_MAX,
};
pub use walker::{step_open, step_walk, step_walk_in_mount_namespace, SYMLOOP_MAX};
pub use xattr::{XATTR_CREATE, XATTR_LIST_MAX, XATTR_NAME_MAX, XATTR_REPLACE, XATTR_SIZE_MAX};
