use super::*;

pub(super) struct Ext4MemImage {
    blocks: Vec<tx_ext4_format::pager::Page4K>,
}

impl Ext4MemImage {
    pub(super) fn from_blocks(blocks: Vec<tx_ext4_format::pager::Page4K>) -> Self {
        Self { blocks }
    }
}

static TEST_PROJECTION_SCHEMA: TestProjectionSchema = TestProjectionSchema;

unsafe impl<T: Send> Sync for TestSpinMutex<T> {}

impl<T> TestSpinMutex<T> {
    fn new(value: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }

    fn lock(&self) -> TestSpinMutexGuard<'_, T> {
        while self
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }

        TestSpinMutexGuard { mutex: self }
    }
}

impl<T> core::ops::Deref for TestSpinMutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // Safety: the spin mutex admits one guard at a time.
        unsafe { &*self.mutex.value.get() }
    }
}

impl<T> core::ops::DerefMut for TestSpinMutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // Safety: the spin mutex admits one mutable guard at a time.
        unsafe { &mut *self.mutex.value.get() }
    }
}

impl<T> Drop for TestSpinMutexGuard<'_, T> {
    fn drop(&mut self) {
        self.mutex.locked.store(false, Ordering::Release);
    }
}

impl Ext4FormatReadFs {
    pub(super) fn new(image: Ext4MemImage) -> Self {
        let pager = tx_ext4_format::pager::Ext4Pager::open(image).expect("ext4 pager open");
        Self {
            pager: TestSpinMutex::new(pager),
        }
    }

    fn map_error(err: tx_ext4_format::Ext4FormatError) -> Errno {
        match err {
            tx_ext4_format::Ext4FormatError::OutOfBounds
            | tx_ext4_format::Ext4FormatError::Truncated => Errno::NoEntry,
            tx_ext4_format::Ext4FormatError::Unsupported => Errno::NotImplemented,
            tx_ext4_format::Ext4FormatError::BadMagic
            | tx_ext4_format::Ext4FormatError::Corrupt => Errno::Invalid,
        }
    }

    fn map_meta(meta: tx_ext4_format::pager::InodeMetaLite) -> InodeMeta {
        InodeMeta {
            mode: meta.mode,
            uid: meta.uid,
            gid: meta.gid,
            size: meta.size,
            atime: crate::vfs::structure::Timespec {
                sec: meta.atime as i64,
                nsec: 0,
            },
            mtime: crate::vfs::structure::Timespec {
                sec: meta.mtime as i64,
                nsec: 0,
            },
            ctime: crate::vfs::structure::Timespec {
                sec: meta.ctime as i64,
                nsec: 0,
            },
            nlinks: meta.nlinks,
            blocks: meta.blocks_512,
            flags: meta.flags,
        }
    }
}

impl tx_ext4_format::pager::BlockImage for Ext4MemImage {
    fn total_blocks(&self) -> u64 {
        self.blocks.len() as u64
    }

    fn read_block(
        &self,
        block: u64,
        out: &mut tx_ext4_format::pager::Page4K,
    ) -> tx_ext4_format::Result<()> {
        let src = self
            .blocks
            .get(block as usize)
            .ok_or(tx_ext4_format::Ext4FormatError::OutOfBounds)?;
        *out = *src;
        Ok(())
    }

    fn write_block(
        &mut self,
        block: u64,
        data: &tx_ext4_format::pager::Page4K,
    ) -> tx_ext4_format::Result<()> {
        let dst = self
            .blocks
            .get_mut(block as usize)
            .ok_or(tx_ext4_format::Ext4FormatError::OutOfBounds)?;
        *dst = *data;
        Ok(())
    }
}

impl FsPageBacking for Ext4FormatReadFs {
    fn fetch_page<'g>(
        &self,
        fs_object_id: FsObjectId,
        offset: u64,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<Frame> {
        if !offset.is_multiple_of(tx_ext4_format::pager::BLOCK_SIZE as u64) {
            return StepOutcome::Err(Errno::Invalid);
        }

        let inode = match fs_object_id.0.try_into() {
            Ok(inode) => tx_ext4_format::pager::InodeNo::new(inode),
            Err(_) => return StepOutcome::Err(Errno::NoEntry),
        };
        let page_index = offset / tx_ext4_format::pager::BLOCK_SIZE as u64;
        let mut page = [0u8; tx_ext4_format::pager::BLOCK_SIZE];
        let mut pager = self.pager.lock();
        match pager.read_page(inode, page_index, &mut page) {
            Ok(_) => match Frame::from_bytes(&page) {
                Ok(frame) => StepOutcome::Done(frame),
                Err(errno) => StepOutcome::Err(errno),
            },
            Err(err) => StepOutcome::Err(Self::map_error(err)),
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

impl FsOps for Ext4FormatReadFs {
    fn lookup<'g>(
        &self,
        parent: FsObjectId,
        name: &[u8],
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<FsObjectId> {
        let inode = match parent.0.try_into() {
            Ok(inode) => tx_ext4_format::pager::InodeNo::new(inode),
            Err(_) => return StepOutcome::Err(Errno::NoEntry),
        };
        let mut pager = self.pager.lock();
        match pager.lookup(inode, name) {
            Ok(Some(found)) => StepOutcome::Done(FsObjectId(found.get() as u64)),
            Ok(None) => StepOutcome::Err(Errno::NoEntry),
            Err(err) => StepOutcome::Err(Self::map_error(err)),
        }
    }

    fn load_inode_meta<'g>(
        &self,
        fs_object_id: FsObjectId,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<InodeMeta> {
        let inode = match fs_object_id.0.try_into() {
            Ok(inode) => tx_ext4_format::pager::InodeNo::new(inode),
            Err(_) => return StepOutcome::Err(Errno::NoEntry),
        };
        let mut pager = self.pager.lock();
        match pager.inode_meta(inode) {
            Ok(meta) => StepOutcome::Done(Self::map_meta(meta)),
            Err(err) => StepOutcome::Err(Self::map_error(err)),
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

impl TestProjectionSchema {
    const FILE_TYPE: u32 = 0x71;
    const BYTES: &'static [u8] = b"projected-interface-read";
}

impl ProjectionSchema for TestProjectionSchema {
    fn schema_id(&self) -> u32 {
        0x7052
    }

    fn render<'g>(
        &self,
        key: &ProjectionKey,
        _ctx: &ProjectionReadCtx<'g>,
        out: &mut RenderBuffer,
        _guard: &'g epoch::Guard<'g>,
    ) -> Result<(), Errno> {
        if key.object_id != 0x8383 || key.file_type != Self::FILE_TYPE {
            return Err(Errno::NoEntry);
        }
        out.push_bytes(Self::BYTES)
    }
}

impl FsPageBacking for BackendInterfaceFs {
    fn fetch_page<'g>(
        &self,
        fs_object_id: FsObjectId,
        offset: u64,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<Frame> {
        match (fs_object_id, offset) {
            (FsObjectId(0x8282), 0) => {
                StepOutcome::Done(Frame::from_bytes(b"provided-pc-read").expect("frame"))
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

impl FsOps for BackendInterfaceFs {
    fn lookup<'g>(
        &self,
        parent: FsObjectId,
        name: &[u8],
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<FsObjectId> {
        if parent != FsObjectId(70) {
            return StepOutcome::Err(Errno::NoEntry);
        }

        match name {
            b"provided-pc" => StepOutcome::Done(FsObjectId(0x8282)),
            b"projected" => StepOutcome::Done(FsObjectId(0x8383)),
            b"struct-backed" => StepOutcome::Done(FsObjectId(0x8484)),
            _ => StepOutcome::Err(Errno::NoEntry),
        }
    }

    fn load_inode_meta<'g>(
        &self,
        fs_object_id: FsObjectId,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<InodeMeta> {
        match fs_object_id {
            FsObjectId(0x8282) => StepOutcome::Done(test_inode_meta(0x20)),
            FsObjectId(0x8383) => StepOutcome::Done(test_inode_meta(0x30)),
            FsObjectId(0x8484) => StepOutcome::Done(test_inode_meta(0x40)),
            _ => StepOutcome::Err(Errno::NoEntry),
        }
    }

    fn backing_for<'g>(
        &self,
        fs_object_id: FsObjectId,
        _guard: &'g epoch::Guard<'g>,
    ) -> StepOutcome<RNodeBackingInit> {
        match &self.backing {
            BackendInterfaceBacking::ProvidedPc(pc) if fs_object_id == FsObjectId(0x8282) => {
                StepOutcome::Done(RNodeBackingInit::PageBacked { pc: pc.clone() })
            }
            BackendInterfaceBacking::Projected if fs_object_id == FsObjectId(0x8383) => {
                StepOutcome::Done(RNodeBackingInit::Projected {
                    schema: &TEST_PROJECTION_SCHEMA,
                    key: ProjectionKey {
                        object_id: fs_object_id.0,
                        file_type: TestProjectionSchema::FILE_TYPE,
                    },
                })
            }
            BackendInterfaceBacking::StructBacked if fs_object_id == FsObjectId(0x8484) => {
                StepOutcome::Done(RNodeBackingInit::StructBacked {
                    payload: StructPayload::Deferred,
                })
            }
            _ => StepOutcome::Err(Errno::NoEntry),
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
