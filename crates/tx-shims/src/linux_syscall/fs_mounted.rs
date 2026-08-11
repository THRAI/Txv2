//! Shim-local mounted VFS object helpers.
//!
//! `MountedDentry` captures the syscall-facing boundary from a VFS dentry to
//! the mounted filesystem payload that supplies `FsOps` and `FsPageBacking`.
//! It does not claim to solve fd-only descendant `RNode` lookup; those paths
//! still require a direct `containing_mount_weak` until the VFS stamping
//! contract is settled.

use super::*;
use crate::adapter::step_engine::{self as step_engine, Cap};
use alloc::sync::Arc;
use tx_subsystems::mount::MountPayload;
use tx_subsystems::page_backed::FsPageBacking;
use tx_subsystems::vfs::structure::RNode;
use tx_subsystems::vfs::FsOps;

pub(super) struct MountedDentry {
    _dentry: Cap<DEntry>,
    payload: Cap<MountPayload>,
}

pub(super) struct MountedNode {
    _rnode: Cap<RNode>,
    payload: Cap<MountPayload>,
}

impl MountedDentry {
    pub(super) fn find_ascending(dentry: &Cap<DEntry>) -> Option<Self> {
        let guard = step_engine::guard();
        let mut cursor = dentry.clone();
        loop {
            if let Some(weak) = cursor.rnode().containing_mount_weak() {
                if let Some(payload) = weak.upgrade(&guard) {
                    return Some(Self {
                        _dentry: cursor,
                        payload,
                    });
                }
            }
            cursor = cursor.parent_hint()?;
        }
    }

    pub(super) fn into_payload(self) -> Cap<MountPayload> {
        self.payload
    }

    pub(super) fn fs_ops(&self) -> Arc<dyn FsOps> {
        self.payload.fs_ops.clone()
    }

    pub(super) fn fs_page_backing(&self) -> Arc<dyn FsPageBacking> {
        self.payload.fs_page_backing.clone()
    }
}

impl MountedNode {
    pub(super) fn from_rnode_direct(rnode: &Cap<RNode>) -> Option<Self> {
        let guard = step_engine::guard();
        let payload = rnode.containing_mount_weak()?.upgrade(&guard)?;
        Some(Self {
            _rnode: rnode.clone(),
            payload,
        })
    }

    pub(super) fn into_payload(self) -> Cap<MountPayload> {
        self.payload
    }

    pub(super) fn payload(&self) -> Cap<MountPayload> {
        self.payload.clone()
    }

    pub(super) fn fs_ops(&self) -> Arc<dyn FsOps> {
        self.payload.fs_ops.clone()
    }

    pub(super) fn fs_page_backing(&self) -> Arc<dyn FsPageBacking> {
        self.payload.fs_page_backing().clone()
    }
}
