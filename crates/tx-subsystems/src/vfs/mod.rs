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
pub mod fd_ready;
pub mod notification;
pub mod predicates;
pub mod require;
pub mod resolution;
pub mod structure;
pub mod walker;

#[cfg(test)]
mod tests;

pub use crate::cred::CapabilitySet;
pub use checks::{DirectoryAtPath, EntityAtPath, ParentAndName, ResolveCtx, RootCtx};
pub use execution::{
    CreateInParentOp, CreateThenWalkInMountNamespaceOp, CreateThenWalkOp, FileFsyncOp, FlockOp,
    FsOps, InodeStatOp, LinkInParentOp, LoadInodeMetaOp, LookupInParentOp, MkdirOp, MountOutput,
    OpenFileGetFlOp, OpenFileIoctlOp, OpenFileLseekOp, OpenFileSetFlOp, OpenInMountNamespaceOp,
    OpenNoFollowInMountNamespaceOp, OpenNoFollowOp, OpenOp, PathWalkOp, ReadLinkByIdOp,
    ResolveOpenTargetInMountNamespaceOp, ResolveOpenTargetOp, SymlinkOp, TruncateFsObjectOp,
    UnlinkFromParentOp, WalkInMountNamespaceWithOriginOp,
};
pub use fd_ready::{query_fd_ready, FdReadyMask, FdReadyQuery, FdReadyReport, FdWait};
pub use notification::{VFS_READABLE, VFS_WRITABLE};
pub use structure::{
    render_dentry_path, Credential, DEntry, DirCursor, DirEntry, FsObjectId, InlineName, InodeKind,
    InodeMeta, OpenFile, OpenFileFlags, OpenFileIoctl, OpenFileIoctlCaller, OpenFileIoctlResult,
    ProjectionKey, ProjectionSchemaId, RNode, RNodeBacking, StructPayload, Timespec, VfsName,
    S_IFBLK, S_IFCHR, S_IFDIR, S_IFIFO, S_IFLNK, S_IFMT, S_IFREG, S_IFSOCK, S_ISGID, S_ISUID,
    S_ISVTX, VFS_NAME_MAX, render_dentry_path_in_namespace,
};
pub use walker::{
    step_open, step_open_in_mount_namespace, step_open_in_mount_namespace_with_mount,
    step_open_in_mount_namespace_with_origin_mount, step_open_nofollow, step_walk,
    step_walk_in_mount_namespace, step_walk_in_mount_namespace_with_origin_mount,
    OpenFileWithMount, ResolvedDEntryWithMount, SYMLOOP_MAX,
};
