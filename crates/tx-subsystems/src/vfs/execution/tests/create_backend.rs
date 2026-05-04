use super::*;

impl FsOps for CreateOnlyFs {
    fn lookup<'g>(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<FsObjectId> {
        StepOutcome::Err(Errno::NotImplemented)
    }

    fn load_inode_meta<'g>(
        &self,
        _fs_object_id: FsObjectId,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<InodeMeta> {
        StepOutcome::Err(Errno::NotImplemented)
    }

    fn serialize_inode_meta<'g>(
        &self,
        _fs_object_id: FsObjectId,
        _meta: &InodeMeta,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::NotImplemented)
    }

    fn create_inode<'g>(
        &self,
        parent: FsObjectId,
        name: &[u8],
        _mode: u16,
        _cred: &Credential,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<(FsObjectId, InodeMeta)> {
        let object_id = FsObjectId(parent.0 ^ 0x4444_0000_0000_0000 ^ u64::from(name[0]));
        StepOutcome::Done((
            object_id,
            InodeMeta {
                mode: InodeMeta::TYPE_REGULAR | 0o600,
                uid: 12,
                gid: 34,
                size: 99,
                atime: crate::vfs::structure::Timespec { sec: 1, nsec: 2 },
                mtime: crate::vfs::structure::Timespec { sec: 3, nsec: 4 },
                ctime: crate::vfs::structure::Timespec { sec: 5, nsec: 6 },
                nlinks: 2,
                blocks: 7,
                flags: 8,
            },
        ))
    }

    fn unlink<'g>(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::NotImplemented)
    }

    fn rename<'g>(
        &self,
        _old_parent: FsObjectId,
        _old_name: &[u8],
        _new_parent: FsObjectId,
        _new_name: &[u8],
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::NotImplemented)
    }

    fn link<'g>(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::NotImplemented)
    }

    fn mkdir<'g>(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _mode: u16,
        _cred: &Credential,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<(FsObjectId, InodeMeta)> {
        StepOutcome::Err(Errno::NotImplemented)
    }

    fn rmdir<'g>(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::NotImplemented)
    }

    fn symlink<'g>(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _link_target: &[u8],
        _cred: &Credential,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<(FsObjectId, InodeMeta)> {
        StepOutcome::Err(Errno::NotImplemented)
    }

    fn readdir<'g>(
        &self,
        _fs_object_id: FsObjectId,
        _cursor: DirCursor,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<Option<(DirEntry, DirCursor)>> {
        StepOutcome::Err(Errno::NotImplemented)
    }

    fn destroy_inode<'g>(
        &self,
        _fs_object_id: FsObjectId,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::NotImplemented)
    }
}

impl FsOps for BlockThenCreateFs {
    fn lookup<'g>(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<FsObjectId> {
        StepOutcome::Err(Errno::NotImplemented)
    }

    fn load_inode_meta<'g>(
        &self,
        _fs_object_id: FsObjectId,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<InodeMeta> {
        StepOutcome::Err(Errno::NotImplemented)
    }

    fn serialize_inode_meta<'g>(
        &self,
        _fs_object_id: FsObjectId,
        _meta: &InodeMeta,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::NotImplemented)
    }

    fn create_inode<'g>(
        &self,
        parent: FsObjectId,
        name: &[u8],
        _mode: u16,
        _cred: &Credential,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<(FsObjectId, InodeMeta)> {
        if !self.blocked_once.swap(true, Ordering::AcqRel) {
            return StepOutcome::Blocked(
                crate::step::WakeCarrier::Queue(self.queue.clone()),
                crate::step::InterestConditions { bits: 0x10 },
            );
        }

        StepOutcome::Done((
            FsObjectId(parent.0 ^ 0x2222_0000_0000_0000 ^ u64::from(name[0])),
            InodeMeta {
                mode: InodeMeta::TYPE_REGULAR | 0o640,
                uid: 56,
                gid: 78,
                size: 123,
                atime: crate::vfs::structure::Timespec { sec: 7, nsec: 8 },
                mtime: crate::vfs::structure::Timespec { sec: 9, nsec: 10 },
                ctime: crate::vfs::structure::Timespec { sec: 11, nsec: 12 },
                nlinks: 3,
                blocks: 4,
                flags: 5,
            },
        ))
    }

    fn unlink<'g>(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::NotImplemented)
    }

    fn rename<'g>(
        &self,
        _old_parent: FsObjectId,
        _old_name: &[u8],
        _new_parent: FsObjectId,
        _new_name: &[u8],
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::NotImplemented)
    }

    fn link<'g>(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::NotImplemented)
    }

    fn mkdir<'g>(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _mode: u16,
        _cred: &Credential,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<(FsObjectId, InodeMeta)> {
        StepOutcome::Err(Errno::NotImplemented)
    }

    fn rmdir<'g>(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::NotImplemented)
    }

    fn symlink<'g>(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _link_target: &[u8],
        _cred: &Credential,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<(FsObjectId, InodeMeta)> {
        StepOutcome::Err(Errno::NotImplemented)
    }

    fn readdir<'g>(
        &self,
        _fs_object_id: FsObjectId,
        _cursor: DirCursor,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<Option<(DirEntry, DirCursor)>> {
        StepOutcome::Err(Errno::NotImplemented)
    }

    fn destroy_inode<'g>(
        &self,
        _fs_object_id: FsObjectId,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::NotImplemented)
    }
}

impl FsOps for AdvanceThenBlockCreateFs {
    fn lookup<'g>(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<FsObjectId> {
        StepOutcome::Err(Errno::NotImplemented)
    }

    fn load_inode_meta<'g>(
        &self,
        _fs_object_id: FsObjectId,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<InodeMeta> {
        StepOutcome::Err(Errno::NotImplemented)
    }

    fn serialize_inode_meta<'g>(
        &self,
        _fs_object_id: FsObjectId,
        _meta: &InodeMeta,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::NotImplemented)
    }

    fn create_inode<'g>(
        &self,
        parent: FsObjectId,
        name: &[u8],
        _mode: u16,
        _cred: &Credential,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<(FsObjectId, InodeMeta)> {
        if !self.blocked_once.swap(true, Ordering::AcqRel) {
            return StepOutcome::AdvancedThenBlocked(
                Progress::Units(1),
                crate::step::WakeCarrier::Queue(self.queue.clone()),
                crate::step::InterestConditions { bits: 0x20 },
            );
        }

        StepOutcome::Done((
            FsObjectId(parent.0 ^ 0x3333_0000_0000_0000 ^ u64::from(name[0])),
            InodeMeta {
                mode: InodeMeta::TYPE_REGULAR | 0o644,
                uid: 90,
                gid: 91,
                size: 456,
                atime: crate::vfs::structure::Timespec { sec: 13, nsec: 14 },
                mtime: crate::vfs::structure::Timespec { sec: 15, nsec: 16 },
                ctime: crate::vfs::structure::Timespec { sec: 17, nsec: 18 },
                nlinks: 4,
                blocks: 5,
                flags: 6,
            },
        ))
    }

    fn unlink<'g>(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::NotImplemented)
    }

    fn rename<'g>(
        &self,
        _old_parent: FsObjectId,
        _old_name: &[u8],
        _new_parent: FsObjectId,
        _new_name: &[u8],
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::NotImplemented)
    }

    fn link<'g>(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::NotImplemented)
    }

    fn mkdir<'g>(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _mode: u16,
        _cred: &Credential,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<(FsObjectId, InodeMeta)> {
        StepOutcome::Err(Errno::NotImplemented)
    }

    fn rmdir<'g>(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::NotImplemented)
    }

    fn symlink<'g>(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _link_target: &[u8],
        _cred: &Credential,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<(FsObjectId, InodeMeta)> {
        StepOutcome::Err(Errno::NotImplemented)
    }

    fn readdir<'g>(
        &self,
        _fs_object_id: FsObjectId,
        _cursor: DirCursor,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<Option<(DirEntry, DirCursor)>> {
        StepOutcome::Err(Errno::NotImplemented)
    }

    fn destroy_inode<'g>(
        &self,
        _fs_object_id: FsObjectId,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::NotImplemented)
    }
}

impl FsOps for AdvanceTwiceThenBlockThenCreateFs {
    fn lookup<'g>(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<FsObjectId> {
        StepOutcome::Err(Errno::NotImplemented)
    }

    fn load_inode_meta<'g>(
        &self,
        _fs_object_id: FsObjectId,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<InodeMeta> {
        StepOutcome::Err(Errno::NotImplemented)
    }

    fn serialize_inode_meta<'g>(
        &self,
        _fs_object_id: FsObjectId,
        _meta: &InodeMeta,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::NotImplemented)
    }

    fn create_inode<'g>(
        &self,
        parent: FsObjectId,
        name: &[u8],
        _mode: u16,
        _cred: &Credential,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<(FsObjectId, InodeMeta)> {
        match self.call_count.fetch_add(1, Ordering::AcqRel) {
            0 | 1 => StepOutcome::Advanced(Progress::Units(1)),
            2 => StepOutcome::Blocked(
                crate::step::WakeCarrier::Queue(self.queue.clone()),
                crate::step::InterestConditions { bits: 0x40 },
            ),
            _ => StepOutcome::Done((
                FsObjectId(parent.0 ^ 0x5555_0000_0000_0000 ^ u64::from(name[0])),
                InodeMeta {
                    mode: InodeMeta::TYPE_REGULAR | 0o666,
                    uid: 21,
                    gid: 22,
                    size: 789,
                    atime: crate::vfs::structure::Timespec { sec: 19, nsec: 20 },
                    mtime: crate::vfs::structure::Timespec { sec: 21, nsec: 22 },
                    ctime: crate::vfs::structure::Timespec { sec: 23, nsec: 24 },
                    nlinks: 5,
                    blocks: 6,
                    flags: 7,
                },
            )),
        }
    }

    fn unlink<'g>(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::NotImplemented)
    }

    fn rename<'g>(
        &self,
        _old_parent: FsObjectId,
        _old_name: &[u8],
        _new_parent: FsObjectId,
        _new_name: &[u8],
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::NotImplemented)
    }

    fn link<'g>(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::NotImplemented)
    }

    fn mkdir<'g>(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _mode: u16,
        _cred: &Credential,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<(FsObjectId, InodeMeta)> {
        StepOutcome::Err(Errno::NotImplemented)
    }

    fn rmdir<'g>(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::NotImplemented)
    }

    fn symlink<'g>(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _link_target: &[u8],
        _cred: &Credential,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<(FsObjectId, InodeMeta)> {
        StepOutcome::Err(Errno::NotImplemented)
    }

    fn readdir<'g>(
        &self,
        _fs_object_id: FsObjectId,
        _cursor: DirCursor,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<Option<(DirEntry, DirCursor)>> {
        StepOutcome::Err(Errno::NotImplemented)
    }

    fn destroy_inode<'g>(
        &self,
        _fs_object_id: FsObjectId,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::NotImplemented)
    }
}
