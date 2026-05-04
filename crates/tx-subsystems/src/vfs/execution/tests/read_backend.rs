use super::*;

impl FsPageBacking for LookupReadFs {
    fn fetch_page<'g>(
        &self,
        fs_object_id: FsObjectId,
        offset: u64,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<Frame> {
        if fs_object_id == FsObjectId(0x5151) && offset == 0 {
            StepOutcome::Done(Frame::from_bytes(b"tx-read-ok").expect("frame"))
        } else {
            StepOutcome::Err(Errno::NoEntry)
        }
    }

    fn flush_page<'g>(
        &self,
        _fs_object_id: FsObjectId,
        _offset: u64,
        _frame: &Frame,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::NotImplemented)
    }

    fn truncate<'g>(
        &self,
        _fs_object_id: FsObjectId,
        _new_size: u64,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::NotImplemented)
    }

    fn fsync<'g>(
        &self,
        _fs_object_id: FsObjectId,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::NotImplemented)
    }
}

impl FsPageBacking for BlockThenReadFs {
    fn fetch_page<'g>(
        &self,
        fs_object_id: FsObjectId,
        offset: u64,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<Frame> {
        if fs_object_id != FsObjectId(0x6161) {
            return StepOutcome::Err(Errno::NoEntry);
        }

        match offset {
            0 => StepOutcome::Done(
                Frame::from_bytes(&[b'a'; crate::page_backed::FRAME_CAPACITY]).expect("frame"),
            ),
            x if x == crate::page_backed::FRAME_CAPACITY as u64 => {
                if !self.blocked_once.swap(true, Ordering::AcqRel) {
                    StepOutcome::Blocked(
                        crate::step::WakeCarrier::Queue(self.queue.clone()),
                        crate::step::InterestConditions { bits: 0x88 },
                    )
                } else {
                    StepOutcome::Done(Frame::from_bytes(b"tail-page").expect("frame"))
                }
            }
            _ => StepOutcome::Err(Errno::NoEntry),
        }
    }

    fn flush_page<'g>(
        &self,
        _fs_object_id: FsObjectId,
        _offset: u64,
        _frame: &Frame,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::NotImplemented)
    }

    fn truncate<'g>(
        &self,
        _fs_object_id: FsObjectId,
        _new_size: u64,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::NotImplemented)
    }

    fn fsync<'g>(
        &self,
        _fs_object_id: FsObjectId,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::NotImplemented)
    }
}

impl FsOps for LookupReadFs {
    fn lookup<'g>(
        &self,
        parent: FsObjectId,
        name: &[u8],
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<FsObjectId> {
        if parent == FsObjectId(70) && name == b"cold-read" {
            StepOutcome::Done(FsObjectId(0x5151))
        } else {
            StepOutcome::Err(Errno::NoEntry)
        }
    }

    fn load_inode_meta<'g>(
        &self,
        fs_object_id: FsObjectId,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<InodeMeta> {
        if fs_object_id == FsObjectId(0x5151) {
            StepOutcome::Done(InodeMeta {
                mode: InodeMeta::TYPE_REGULAR | 0o644,
                uid: 1,
                gid: 2,
                size: 10,
                atime: crate::vfs::structure::Timespec { sec: 1, nsec: 0 },
                mtime: crate::vfs::structure::Timespec { sec: 2, nsec: 0 },
                ctime: crate::vfs::structure::Timespec { sec: 3, nsec: 0 },
                nlinks: 1,
                blocks: 1,
                flags: 0,
            })
        } else {
            StepOutcome::Err(Errno::NoEntry)
        }
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
        _parent: FsObjectId,
        _name: &[u8],
        _mode: u16,
        _cred: &Credential,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<(FsObjectId, InodeMeta)> {
        StepOutcome::Err(Errno::NotImplemented)
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

impl FsOps for BlockThenReadFs {
    fn lookup<'g>(
        &self,
        parent: FsObjectId,
        name: &[u8],
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<FsObjectId> {
        if parent == FsObjectId(70) && name == b"blocked-read" {
            StepOutcome::Done(FsObjectId(0x6161))
        } else {
            StepOutcome::Err(Errno::NoEntry)
        }
    }

    fn load_inode_meta<'g>(
        &self,
        fs_object_id: FsObjectId,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<InodeMeta> {
        if fs_object_id == FsObjectId(0x6161) {
            StepOutcome::Done(InodeMeta {
                mode: InodeMeta::TYPE_REGULAR | 0o644,
                uid: 3,
                gid: 4,
                size: crate::page_backed::FRAME_CAPACITY as u64 + 9,
                atime: crate::vfs::structure::Timespec { sec: 0, nsec: 0 },
                mtime: crate::vfs::structure::Timespec { sec: 0, nsec: 0 },
                ctime: crate::vfs::structure::Timespec { sec: 0, nsec: 0 },
                nlinks: 1,
                blocks: 2,
                flags: 0,
            })
        } else {
            StepOutcome::Err(Errno::NoEntry)
        }
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
        _parent: FsObjectId,
        _name: &[u8],
        _mode: u16,
        _cred: &Credential,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<(FsObjectId, InodeMeta)> {
        StepOutcome::Err(Errno::NotImplemented)
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

impl FsPageBacking for AdvanceThenBlockReadFs {
    fn fetch_page<'g>(
        &self,
        fs_object_id: FsObjectId,
        offset: u64,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<Frame> {
        if fs_object_id != FsObjectId(0x7171) {
            return StepOutcome::Err(Errno::NoEntry);
        }

        match offset {
            0 => StepOutcome::Done(
                Frame::from_bytes(&[b'b'; crate::page_backed::FRAME_CAPACITY]).expect("frame"),
            ),
            x if x == crate::page_backed::FRAME_CAPACITY as u64 => {
                match self.second_page_calls.fetch_add(1, Ordering::AcqRel) {
                    0 => StepOutcome::AdvancedThenBlocked(
                        Progress::Units(7),
                        crate::step::WakeCarrier::Queue(self.queue.clone()),
                        crate::step::InterestConditions { bits: 0x99 },
                    ),
                    _ => StepOutcome::Done(Frame::from_bytes(b"tail-two").expect("frame")),
                }
            }
            _ => StepOutcome::Err(Errno::NoEntry),
        }
    }

    fn flush_page<'g>(
        &self,
        _fs_object_id: FsObjectId,
        _offset: u64,
        _frame: &Frame,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::NotImplemented)
    }

    fn truncate<'g>(
        &self,
        _fs_object_id: FsObjectId,
        _new_size: u64,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::NotImplemented)
    }

    fn fsync<'g>(
        &self,
        _fs_object_id: FsObjectId,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::NotImplemented)
    }
}

impl FsOps for AdvanceThenBlockReadFs {
    fn lookup<'g>(
        &self,
        parent: FsObjectId,
        name: &[u8],
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<FsObjectId> {
        if parent == FsObjectId(70) && name == b"advance-block-read" {
            StepOutcome::Done(FsObjectId(0x7171))
        } else {
            StepOutcome::Err(Errno::NoEntry)
        }
    }

    fn load_inode_meta<'g>(
        &self,
        fs_object_id: FsObjectId,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<InodeMeta> {
        if fs_object_id == FsObjectId(0x7171) {
            StepOutcome::Done(InodeMeta {
                mode: InodeMeta::TYPE_REGULAR | 0o644,
                uid: 5,
                gid: 6,
                size: crate::page_backed::FRAME_CAPACITY as u64 + 8,
                atime: crate::vfs::structure::Timespec { sec: 0, nsec: 0 },
                mtime: crate::vfs::structure::Timespec { sec: 0, nsec: 0 },
                ctime: crate::vfs::structure::Timespec { sec: 0, nsec: 0 },
                nlinks: 1,
                blocks: 2,
                flags: 0,
            })
        } else {
            StepOutcome::Err(Errno::NoEntry)
        }
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
        _parent: FsObjectId,
        _name: &[u8],
        _mode: u16,
        _cred: &Credential,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<(FsObjectId, InodeMeta)> {
        StepOutcome::Err(Errno::NotImplemented)
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
