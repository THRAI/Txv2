use crate::journal::Jbd2Superblock;
use crate::ondisk::{
    block_bitmap_csum32, crc32c, encode_dir_entry, encode_journal_commit,
    encode_journal_descriptor, group_desc_csum16, inode_csum32, parse_journal_descriptor,
    superblock_csum32, BitmapMut, BitmapView, BlockMapping, CommitHeader, DirEntry, DirEntryIter,
    Extent, ExtentIdx, ExtentNode, GroupDesc, Inode, InodeLocation, InodeTableLayout, Superblock,
};
use crate::ondisk::{read_u16_le, read_u32_le, write_u16_le};
use crate::{Ext4FormatError, Result};
use alloc::vec;
use alloc::vec::Vec;

use crate::mutation::{
    BlockClaim, Ext4MutationPlan, FsyncStamp, MetaRole, MetadataBlock, MutationOrigin,
    SealedDataWrite, SetAttr,
};

pub const BLOCK_SIZE: usize = 4096;
pub type Page4K = [u8; BLOCK_SIZE];

pub trait BlockImage {
    fn total_blocks(&self) -> u64;
    fn read_block(&self, block: u64, out: &mut Page4K) -> Result<()>;
    fn write_block(&mut self, block: u64, data: &Page4K) -> Result<()>;
    fn barrier(&mut self) -> Result<()>;

    /// Drop any cache entry that could hold a stale copy after an external
    /// journal replay or checkpoint overwrites this physical block.
    fn invalidate_block(&mut self, _block: u64) {}

    /// Drop all derived block-cache entries after a checkpoint whose home
    /// block set is owned by a separate L6 graph.
    fn invalidate_all(&mut self) {}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InodeNo(u32);

impl InodeNo {
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u32 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InodeMetaLite {
    pub mode: u16,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub nlinks: u32,
    pub blocks_512: u64,
    pub flags: u32,
    pub atime: u32,
    pub ctime: u32,
    pub mtime: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirEntryLite {
    pub inode: InodeNo,
    pub file_type: u8,
    name_len: u8,
    name: [u8; 255],
}

impl DirEntryLite {
    pub const fn empty() -> Self {
        Self {
            inode: InodeNo::new(0),
            file_type: 0,
            name_len: 0,
            name: [0; 255],
        }
    }

    pub fn name(&self) -> &[u8] {
        &self.name[..self.name_len as usize]
    }

    fn from_ondisk(entry: DirEntry<'_>) -> Result<Self> {
        if entry.name.len() > 255 {
            return Err(Ext4FormatError::Corrupt);
        }
        let mut out = Self::empty();
        out.inode = InodeNo::new(entry.inode);
        out.file_type = entry.file_type;
        out.name_len = entry.name.len() as u8;
        out.name[..entry.name.len()].copy_from_slice(entry.name);
        Ok(out)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageRead {
    Data { block: u64 },
    Hole,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WritebackReceipt {
    pub inode: InodeNo,
    pub file_page_index: u64,
    pub physical_block: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalReceipt {
    pub sequence: u32,
    pub target_block: u64,
    pub descriptor_block: u64,
    pub payload_block: u64,
    pub commit_block: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplayReport {
    pub transactions: u32,
    pub blocks_replayed: u32,
}

/// Physical mapping and immutable geometry of the mounted JBD2 journal file.
///
/// `blocks[logical]` identifies the ext4 physical block backing that journal
/// ring block. Higher layers use this map to derive device LBAs without
/// assuming that the journal inode is physically contiguous.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalGeometry {
    pub superblock: Jbd2Superblock,
    pub blocks: Vec<u64>,
    /// Original JBD2 superblock page, retained for a later state update.
    /// Pure replay only needs parsed geometry; a writer needs this page to
    /// preserve extension fields and regenerate its checksum.
    pub superblock_page: Option<Page4K>,
}

pub struct Ext4Pager<I> {
    image: I,
    superblock: Superblock,
    groups: Vec<GroupDesc>,
    inode_table: InodeTableLayout,
    journal_start: Option<u64>,
    next_sequence: u32,
}

impl<I: BlockImage> Ext4Pager<I> {
    pub fn open(image: I) -> Result<Self> {
        let mut pager = Self {
            image,
            superblock: Superblock::default(),
            groups: Vec::new(),
            inode_table: InodeTableLayout {
                inode_size: 256,
                inodes_per_group: 0,
                first_inode_table_block: 0,
            },
            journal_start: None,
            next_sequence: 1,
        };
        let mut block = [0u8; BLOCK_SIZE];
        pager.image.read_block(0, &mut block)?;
        let superblock = Superblock::parse(&block[1024..2048])?;
        if superblock.block_size() != BLOCK_SIZE as u32 {
            return Err(Ext4FormatError::Unsupported);
        }
        if superblock.feature_incompat & Superblock::FEATURE_INCOMPAT_EXTENTS == 0 {
            return Err(Ext4FormatError::Unsupported);
        }
        let groups = read_group_descs(&pager.image, &superblock)?;
        let group = *groups.first().ok_or(Ext4FormatError::Corrupt)?;
        pager.superblock = superblock;
        pager.groups = groups;
        pager.inode_table = InodeTableLayout::from_superblock_group(&superblock, &group);
        pager.journal_start = pager
            .read_inode(InodeNo::new(superblock.journal_inode))
            .ok()
            .and_then(|inode| match pager.resolve_inode_block(&inode, 0).ok()? {
                BlockMapping::Data(block) => Some(block),
                _ => None,
            });
        Ok(pager)
    }

    pub fn image(&self) -> &I {
        &self.image
    }

    pub const fn superblock(&self) -> Superblock {
        self.superblock
    }

    /// Persist ext4's recovery-required state before admitting a journalled
    /// read-write mount. A later clean detach is the only path allowed to
    /// clear it.
    pub fn mark_recovery_required(&mut self) -> Result<()> {
        let mut page = [0; BLOCK_SIZE];
        self.image.read_block(0, &mut page)?;
        let mut superblock = Superblock::parse(&page[1024..2048])?;
        superblock.feature_compat |= Superblock::FEATURE_COMPAT_HAS_JOURNAL;
        superblock.feature_incompat |= Superblock::FEATURE_INCOMPAT_RECOVER;
        page[1024 + 92..1024 + 96].copy_from_slice(&superblock.feature_compat.to_le_bytes());
        page[1024 + 96..1024 + 100].copy_from_slice(&superblock.feature_incompat.to_le_bytes());
        if superblock.has_metadata_csum() {
            let bytes = &mut page[1024..2048];
            bytes[1020..1024].fill(0);
            let checksum = superblock_csum32(bytes)?;
            bytes[1020..1024].copy_from_slice(&checksum.to_le_bytes());
        }
        self.image.write_block(0, &page)?;
        self.image.barrier()?;
        self.superblock = superblock;
        Ok(())
    }

    pub fn into_inner(self) -> I {
        self.image
    }

    /// Invalidate the image adapter's derived block cache after an L6-owned
    /// checkpoint. The format pager itself retains no metadata cache.
    pub fn settle_image_cache(&mut self) {
        self.image.invalidate_all();
    }

    pub fn inode_meta(&mut self, inode: InodeNo) -> Result<InodeMetaLite> {
        self.read_inode(inode).map(inode_to_meta)
    }

    pub fn journal_geometry(&mut self) -> Result<JournalGeometry> {
        let journal_inode = self.read_inode(InodeNo::new(self.superblock.journal_inode))?;
        let logical_blocks = div_ceil_u64(journal_inode.size, BLOCK_SIZE as u64);
        if logical_blocks < 2 || logical_blocks > u32::MAX as u64 {
            return Err(Ext4FormatError::Unsupported);
        }
        let mut blocks = Vec::new();
        for logical in 0..logical_blocks {
            match self.resolve_inode_block(&journal_inode, logical as u32)? {
                BlockMapping::Data(block) => blocks.push(block),
                BlockMapping::Hole | BlockMapping::NeedNode(_) => {
                    return Err(Ext4FormatError::Corrupt);
                }
            }
        }
        let mut superblock_page = [0; BLOCK_SIZE];
        self.image.read_block(blocks[0], &mut superblock_page)?;
        let superblock = Jbd2Superblock::parse(&superblock_page)?;
        let max_len = superblock.max_len as usize;
        if max_len > blocks.len() {
            return Err(Ext4FormatError::Corrupt);
        }
        blocks.truncate(max_len);
        Ok(JournalGeometry {
            superblock,
            blocks,
            superblock_page: Some(superblock_page),
        })
    }

    /// Read one inode once and return both its VFS metadata and inline extent
    /// root. L5 seeds this root during namespace metadata lookup; it must not
    /// issue this synchronous pager read from a PageContainer miss callback.
    pub fn inode_meta_and_extent_root(
        &mut self,
        inode: InodeNo,
    ) -> Result<(InodeMetaLite, Vec<u8>)> {
        let inode = self.read_inode(inode)?;
        Ok((inode_to_meta(inode), inode.extent_root_bytes().to_vec()))
    }

    pub fn read_page(
        &mut self,
        inode: InodeNo,
        file_page_index: u64,
        out: &mut Page4K,
    ) -> Result<PageRead> {
        let disk_inode = self.read_inode(inode)?;
        let page_start = file_page_index
            .checked_mul(BLOCK_SIZE as u64)
            .ok_or(Ext4FormatError::OutOfBounds)?;
        if page_start >= disk_inode.size {
            out.fill(0);
            return Ok(PageRead::Hole);
        }
        match self.resolve_inode_block(&disk_inode, logical_block(file_page_index)?)? {
            BlockMapping::Data(block) => {
                self.image.read_block(block, out)?;
                let valid_len = core::cmp::min(BLOCK_SIZE as u64, disk_inode.size - page_start);
                out[valid_len as usize..].fill(0);
                Ok(PageRead::Data { block })
            }
            BlockMapping::Hole => {
                out.fill(0);
                Ok(PageRead::Hole)
            }
            BlockMapping::NeedNode(_) => Err(Ext4FormatError::Unsupported),
        }
    }

    pub fn write_existing_page(
        &mut self,
        inode: InodeNo,
        file_page_index: u64,
        page: &Page4K,
    ) -> Result<WritebackReceipt> {
        let disk_inode = self.read_inode(inode)?;
        let block = match self.resolve_inode_block(&disk_inode, logical_block(file_page_index)?)? {
            BlockMapping::Data(block) => block,
            BlockMapping::Hole | BlockMapping::NeedNode(_) => {
                return Err(Ext4FormatError::Unsupported);
            }
        };
        self.image.write_block(block, page)?;
        Ok(WritebackReceipt {
            inode,
            file_page_index,
            physical_block: block,
        })
    }

    /// Build an immutable writeback plan for one already-mapped data page.
    ///
    /// This is deliberately side-effect free: it neither writes a data home
    /// block nor mutates inode, extent, or bitmap metadata. Hole writes are
    /// supported when their extent update still fits in the inode's inline
    /// extent root.
    pub fn plan_write_page(
        &mut self,
        inode: InodeNo,
        file_page_index: u64,
        page: &Page4K,
        fsync_stamp: FsyncStamp,
    ) -> Result<Ext4MutationPlan> {
        let disk_inode = self.read_inode(inode)?;
        let logical = logical_block(file_page_index)?;
        let mut plan =
            Ext4MutationPlan::new(MutationOrigin::FlushPage, inode.get() as u64, fsync_stamp);
        let block = match self.resolve_inode_block(&disk_inode, logical)? {
            BlockMapping::Data(block) => block,
            BlockMapping::Hole => {
                let (physical_block, group_index, bitmap_home, bitmap_before, bitmap_after) =
                    self.plan_block_allocation()?;
                let (group_desc_home, group_desc_before, group_desc_after) =
                    self.plan_group_free_block_decrement(group_index, &bitmap_after)?;
                let (superblock_home, superblock_before, superblock_after) =
                    self.plan_superblock_free_block_decrement()?;
                let mut inode_after = disk_inode;
                let mut extents = match ExtentNode::parse(inode_after.extent_root_bytes())? {
                    ExtentNode::Leaf(extents) => extents,
                    ExtentNode::Index(_) => return Err(Ext4FormatError::Unsupported),
                };
                coalesce_insert_extent(&mut extents, logical, physical_block);
                if extents.len() > 4 {
                    return Err(Ext4FormatError::Unsupported);
                }
                inode_after.set_extent_root(&extents)?;
                inode_after.blocks_512 = inode_after
                    .blocks_512
                    .saturating_add((BLOCK_SIZE / 512) as u64);
                let page_end = file_page_index
                    .checked_add(1)
                    .and_then(|page| page.checked_mul(BLOCK_SIZE as u64))
                    .ok_or(Ext4FormatError::OutOfBounds)?;
                inode_after.size = core::cmp::max(inode_after.size, page_end);

                let loc = self.inode_location(inode)?;
                let mut inode_table_before = [0u8; BLOCK_SIZE];
                self.image.read_block(loc.block, &mut inode_table_before)?;
                let mut inode_table_after = inode_table_before;
                let inode_bytes = &mut inode_table_after[loc.offset..loc.offset + loc.len];
                inode_after.encode(inode_bytes)?;
                if self.superblock.has_metadata_csum() {
                    inode_bytes[124..126].fill(0);
                    if inode_bytes.len() >= 132 {
                        inode_bytes[130..132].fill(0);
                    }
                    let checksum = inode_csum32(
                        self.superblock.metadata_csum_seed(),
                        inode.get(),
                        inode_after.generation,
                        inode_bytes,
                    )?;
                    inode_bytes[124..126].copy_from_slice(&(checksum as u16).to_le_bytes());
                    if inode_bytes.len() >= 132 {
                        inode_bytes[130..132]
                            .copy_from_slice(&((checksum >> 16) as u16).to_le_bytes());
                    }
                }

                plan.push_metadata(MetadataBlock {
                    home: bitmap_home,
                    role: MetaRole::BlockBitmap,
                    before_version: crc32c(0, &bitmap_before) as u64,
                    after: bitmap_after,
                    depends_on: Vec::new(),
                })
                .map_err(|_| Ext4FormatError::Corrupt)?;
                plan.push_metadata(MetadataBlock {
                    home: group_desc_home,
                    role: MetaRole::GroupDescriptor,
                    before_version: crc32c(0, &group_desc_before) as u64,
                    after: group_desc_after,
                    depends_on: Vec::new(),
                })
                .map_err(|_| Ext4FormatError::Corrupt)?;
                plan.push_metadata(MetadataBlock {
                    home: superblock_home,
                    role: MetaRole::Superblock,
                    before_version: crc32c(0, &superblock_before) as u64,
                    after: superblock_after,
                    depends_on: Vec::new(),
                })
                .map_err(|_| Ext4FormatError::Corrupt)?;
                plan.push_metadata(MetadataBlock {
                    home: loc.block,
                    role: MetaRole::InodeTable,
                    before_version: crc32c(0, &inode_table_before) as u64,
                    after: inode_table_after,
                    depends_on: Vec::new(),
                })
                .map_err(|_| Ext4FormatError::Corrupt)?;
                plan.allocations.push(BlockClaim { physical_block });
                physical_block
            }
            BlockMapping::NeedNode(_) => return Err(Ext4FormatError::Unsupported),
        };
        if plan.metadata.is_empty() {
            // A mapped data write still needs a committed metadata anchor so
            // fsync can distinguish ordered-data durability from background
            // writeback. The inode after-image is intentionally unchanged.
            let loc = self.inode_location(inode)?;
            let mut inode_table_before = [0u8; BLOCK_SIZE];
            self.image.read_block(loc.block, &mut inode_table_before)?;
            plan.push_metadata(MetadataBlock {
                home: loc.block,
                role: MetaRole::InodeTable,
                before_version: crc32c(0, &inode_table_before) as u64,
                after: inode_table_before,
                depends_on: Vec::new(),
            })
            .map_err(|_| Ext4FormatError::Corrupt)?;
        }
        plan.data.push(SealedDataWrite {
            logical_page: file_page_index,
            physical_block: block,
            bytes: *page,
        });
        Ok(plan)
    }

    /// Build the complete inode-table after-image for a bounded metadata-only
    /// update. This function is pure with respect to the home image: callers
    /// must admit the returned plan through `MutationHandle` before writing it.
    pub fn plan_setattr(
        &mut self,
        inode: InodeNo,
        update: SetAttr,
        fsync_stamp: FsyncStamp,
    ) -> Result<Ext4MutationPlan> {
        let location = self.inode_location(inode)?;
        let mut inode_table_before = [0u8; BLOCK_SIZE];
        self.image
            .read_block(location.block, &mut inode_table_before)?;
        let mut inode_table_after = inode_table_before;
        let inode_bytes = &mut inode_table_after[location.offset..location.offset + location.len];
        let mut disk_inode = Inode::parse(inode_bytes)?;

        match update {
            SetAttr::Mode(mode) => {
                disk_inode.mode = (disk_inode.mode & Inode::S_IFMT) | (mode & !Inode::S_IFMT);
            }
            SetAttr::Owner { uid, gid } => {
                if let Some(uid) = uid {
                    disk_inode.uid = uid;
                }
                if let Some(gid) = gid {
                    disk_inode.gid = gid;
                }
            }
            SetAttr::Times {
                atime_ns,
                mtime_ns,
                ctime_ns,
            } => {
                if let Some(atime_ns) = atime_ns {
                    disk_inode.atime = seconds_from_ns(atime_ns)?;
                }
                if let Some(mtime_ns) = mtime_ns {
                    disk_inode.mtime = seconds_from_ns(mtime_ns)?;
                }
                disk_inode.ctime = seconds_from_ns(ctime_ns)?;
            }
        }
        disk_inode.encode_preserving_unknown(inode_bytes)?;
        self.refresh_inode_checksum(inode, &disk_inode, inode_bytes)?;

        let mut plan =
            Ext4MutationPlan::new(MutationOrigin::SetAttr, inode.get() as u64, fsync_stamp);
        plan.push_metadata(MetadataBlock {
            home: location.block,
            role: MetaRole::InodeTable,
            before_version: crc32c(0, &inode_table_before) as u64,
            after: inode_table_after,
            depends_on: Vec::new(),
        })
        .map_err(|_| Ext4FormatError::Corrupt)?;
        Ok(plan)
    }

    pub fn plan_setattr_mode(
        &mut self,
        inode: InodeNo,
        mode: u16,
        fsync_stamp: FsyncStamp,
    ) -> Result<Ext4MutationPlan> {
        self.plan_setattr(inode, SetAttr::Mode(mode), fsync_stamp)
    }

    fn refresh_inode_checksum(
        &self,
        inode: InodeNo,
        disk_inode: &Inode,
        inode_bytes: &mut [u8],
    ) -> Result<()> {
        if !self.superblock.has_metadata_csum() {
            return Ok(());
        }
        inode_bytes[124..126].fill(0);
        if inode_bytes.len() >= 132 {
            inode_bytes[130..132].fill(0);
        }
        let checksum = inode_csum32(
            self.superblock.metadata_csum_seed(),
            inode.get(),
            disk_inode.generation,
            inode_bytes,
        )?;
        inode_bytes[124..126].copy_from_slice(&(checksum as u16).to_le_bytes());
        if inode_bytes.len() >= 132 {
            inode_bytes[130..132].copy_from_slice(&((checksum >> 16) as u16).to_le_bytes());
        }
        Ok(())
    }

    fn plan_block_allocation(&self) -> Result<(u64, usize, u64, Page4K, Page4K)> {
        if self.superblock.blocks_per_group == 0 {
            return Err(Ext4FormatError::Corrupt);
        }
        let blocks_per_group = self.superblock.blocks_per_group as u64;
        let first_data_block = self.superblock.first_data_block as u64;
        let total_blocks = core::cmp::min(self.superblock.blocks_count, self.image.total_blocks());

        for (group_index, group) in self.groups.iter().copied().enumerate() {
            let group_start = first_data_block + group_index as u64 * blocks_per_group;
            if group_start >= total_blocks {
                break;
            }
            let group_block_count = core::cmp::min(blocks_per_group, total_blocks - group_start);
            let bitmap_home = group.block_bitmap_block();
            let mut bitmap_before = [0u8; BLOCK_SIZE];
            self.image.read_block(bitmap_home, &mut bitmap_before)?;
            let mut bitmap_after = bitmap_before;
            let view = BitmapView::new(&bitmap_after);
            let Some(bit) = (0..group_block_count as usize).find(|bit| !view.is_set(*bit)) else {
                continue;
            };
            BitmapMut::new(&mut bitmap_after).set(bit)?;
            return Ok((
                group_start + bit as u64,
                group_index,
                bitmap_home,
                bitmap_before,
                bitmap_after,
            ));
        }

        Err(Ext4FormatError::OutOfBounds)
    }

    fn plan_group_free_block_decrement(
        &self,
        group_index: usize,
        bitmap_after: &Page4K,
    ) -> Result<(u64, Page4K, Page4K)> {
        let desc_size = self.superblock.group_desc_size();
        let byte_offset = group_index
            .checked_mul(desc_size)
            .ok_or(Ext4FormatError::OutOfBounds)?;
        let gdt_start = if self.superblock.block_size() == 1024 {
            2
        } else {
            1
        };
        let home = gdt_start + (byte_offset / BLOCK_SIZE) as u64;
        let offset = byte_offset % BLOCK_SIZE;
        if offset + desc_size > BLOCK_SIZE {
            return Err(Ext4FormatError::Unsupported);
        }
        let mut before = [0u8; BLOCK_SIZE];
        self.image.read_block(home, &mut before)?;
        let descriptor = GroupDesc::parse_sized(&before[offset..offset + desc_size], desc_size)?;
        let free = u32::from(descriptor.free_blocks_count)
            | (u32::from(descriptor.free_blocks_count_hi) << 16);
        let next = free.checked_sub(1).ok_or(Ext4FormatError::OutOfBounds)?;
        if desc_size < 64 && next > u16::MAX as u32 {
            return Err(Ext4FormatError::Corrupt);
        }
        let mut after = before;
        after[offset + 12..offset + 14].copy_from_slice(&(next as u16).to_le_bytes());
        if desc_size >= 64 {
            after[offset + 44..offset + 46].copy_from_slice(&((next >> 16) as u16).to_le_bytes());
        }
        if self.superblock.has_metadata_csum() {
            let bitmap_checksum = block_bitmap_csum32(
                self.superblock.metadata_csum_seed(),
                bitmap_after,
                self.superblock.blocks_per_group,
            )?;
            after[offset + 24..offset + 26]
                .copy_from_slice(&(bitmap_checksum as u16).to_le_bytes());
            if desc_size >= 64 {
                after[offset + 56..offset + 58]
                    .copy_from_slice(&((bitmap_checksum >> 16) as u16).to_le_bytes());
            }
            after[offset + 30..offset + 32].fill(0);
            let group_id = u32::try_from(group_index).map_err(|_| Ext4FormatError::OutOfBounds)?;
            let checksum = group_desc_csum16(
                self.superblock.metadata_csum_seed(),
                group_id,
                &after[offset..offset + desc_size],
            );
            after[offset + 30..offset + 32].copy_from_slice(&checksum.to_le_bytes());
        }
        Ok((home, before, after))
    }

    fn plan_superblock_free_block_decrement(&self) -> Result<(u64, Page4K, Page4K)> {
        let mut before = [0u8; BLOCK_SIZE];
        self.image.read_block(0, &mut before)?;
        let observed = Superblock::parse(&before[1024..2048])?;
        let next = observed
            .free_blocks_count
            .checked_sub(1)
            .ok_or(Ext4FormatError::OutOfBounds)?;
        let mut after = before;
        after[1024 + 12..1024 + 16].copy_from_slice(&(next as u32).to_le_bytes());
        after[1024 + 0x158..1024 + 0x15c].copy_from_slice(&((next >> 32) as u32).to_le_bytes());
        if observed.has_metadata_csum() {
            let superblock = &mut after[1024..2048];
            superblock[1020..1024].fill(0);
            let checksum = superblock_csum32(superblock)?;
            superblock[1020..1024].copy_from_slice(&checksum.to_le_bytes());
        }
        Ok((0, before, after))
    }

    /// Write a file page back to disk, allocating a fresh block and
    /// extending the inode's extent map when the target page is a hole.
    /// Backs `FsPageBacking::flush_page` (the read-only
    /// `write_existing_page` above only handles already-mapped blocks).
    pub fn write_page(
        &mut self,
        inode: InodeNo,
        file_page_index: u64,
        page: &Page4K,
    ) -> Result<()> {
        let mut disk_inode = self.read_inode(inode)?;
        let logical = logical_block(file_page_index)?;
        let block = match self.resolve_inode_block(&disk_inode, logical)? {
            BlockMapping::Data(block) => block,
            BlockMapping::Hole => {
                let new_block = self.allocate_block()?;
                self.attach_data_block(&mut disk_inode, logical, new_block)?;
                disk_inode.blocks_512 = disk_inode
                    .blocks_512
                    .saturating_add((BLOCK_SIZE / 512) as u64);
                // Size is owned by `serialize_inode_meta` (exact logical
                // size) and `set_inode_size`; writeback only persists the
                // data block + extent, never the size.
                self.write_inode(inode, &disk_inode)?;
                new_block
            }
            BlockMapping::NeedNode(_) => return Err(Ext4FormatError::Unsupported),
        };
        self.image.write_block(block, page)?;
        Ok(())
    }

    /// Attach a newly allocated `physical` block at `logical` to the inode's
    /// extent map, growing the trailing extent when the block continues it
    /// contiguously, otherwise inserting a fresh extent.
    ///
    /// The inode's inline root holds at most 4 extents. When a 5th distinct
    /// extent is needed (e.g. a file fragmented by concurrent multi-process
    /// writeback, or random-order writes), the extents spill into a freshly
    /// allocated extent block and the inode root becomes a single-entry,
    /// depth-1 index node pointing at it. A 4 KiB extent block holds up to 340
    /// extents, which covers the file sizes the test workloads use; deeper
    /// trees / multiple child blocks remain `Unsupported`.
    fn attach_data_block(&mut self, inode: &mut Inode, logical: u32, physical: u64) -> Result<()> {
        const INLINE_MAX_EXTENTS: usize = 4;
        let block_max_extents = (BLOCK_SIZE - 12) / 12;
        match ExtentNode::parse(inode.extent_root_bytes())? {
            ExtentNode::Leaf(mut extents) => {
                coalesce_insert_extent(&mut extents, logical, physical);
                if extents.len() <= INLINE_MAX_EXTENTS {
                    inode.set_extent_root(&extents)?;
                    return Ok(());
                }
                // Inline root full: spill all extents into a fresh extent block
                // and turn the inode root into a single-entry index node.
                let child = self.allocate_block()?;
                let mut block = [0u8; BLOCK_SIZE];
                ExtentNode::encode_leaf(&extents, &mut block)?;
                self.image.write_block(child, &block)?;
                inode.blocks_512 = inode.blocks_512.saturating_add((BLOCK_SIZE / 512) as u64);
                inode.set_extent_index_root(
                    &[ExtentIdx {
                        logical_block: extents[0].logical_block,
                        child,
                    }],
                    1,
                )?;
                Ok(())
            }
            ExtentNode::Index(indexes) => {
                if indexes.is_empty() {
                    return Err(Ext4FormatError::Corrupt);
                }
                // depth-1, single child block. Pick the child covering `logical`.
                let idx_pos = indexes
                    .iter()
                    .rposition(|i| i.logical_block <= logical)
                    .unwrap_or(0);
                let child = indexes[idx_pos].child;
                let mut block = [0u8; BLOCK_SIZE];
                self.image.read_block(child, &mut block)?;
                let mut extents = match ExtentNode::parse(&block)? {
                    ExtentNode::Leaf(list) => list,
                    ExtentNode::Index(_) => return Err(Ext4FormatError::Unsupported),
                };
                coalesce_insert_extent(&mut extents, logical, physical);
                if extents.len() > block_max_extents {
                    return Err(Ext4FormatError::Unsupported);
                }
                ExtentNode::encode_leaf(&extents, &mut block)?;
                self.image.write_block(child, &block)?;
                // Keep the index entry's key in sync if the lowest logical moved.
                let new_low = extents[0].logical_block;
                if new_low != indexes[idx_pos].logical_block {
                    let mut idxs = indexes.clone();
                    idxs[idx_pos].logical_block = new_low;
                    inode.set_extent_index_root(&idxs, 1)?;
                }
                Ok(())
            }
        }
    }

    /// Set the inode's logical size — backs `FsPageBacking::truncate`.
    /// Block reclamation on shrink is not yet implemented; the `size`
    /// field governs how many bytes reads return.
    pub fn set_inode_size(&mut self, inode: InodeNo, new_size: u64) -> Result<()> {
        let mut disk_inode = self.read_inode(inode)?;
        disk_inode.size = new_size;
        self.write_inode(inode, &disk_inode)?;
        Ok(())
    }

    pub fn lookup(&mut self, directory: InodeNo, name: &[u8]) -> Result<Option<InodeNo>> {
        let mut entries = [DirEntryLite::empty(); 8];
        let mut offset = 0u64;
        loop {
            let count = self.read_dir_entries_from(directory, offset, &mut entries)?;
            if count == 0 {
                return Ok(None);
            }
            for entry in entries.iter().take(count) {
                if entry.name() == name {
                    return Ok(Some(entry.inode));
                }
            }
            offset += count as u64;
        }
    }

    pub fn read_dir_entries(
        &mut self,
        directory: InodeNo,
        out: &mut [DirEntryLite],
    ) -> Result<usize> {
        self.read_dir_entries_from(directory, 0, out)
    }

    pub fn read_link(&mut self, inode: InodeNo) -> Result<Vec<u8>> {
        let disk_inode = self.read_inode(inode)?;
        if !disk_inode.is_symlink() {
            return Err(Ext4FormatError::Unsupported);
        }
        if let Some(target) = disk_inode.inline_symlink_target()? {
            return Ok(target.to_vec());
        }
        let mut out = vec![0; disk_inode.size as usize];
        for (idx, chunk) in out.chunks_mut(BLOCK_SIZE).enumerate() {
            let mut page = [0u8; BLOCK_SIZE];
            match self.resolve_inode_block(&disk_inode, logical_block(idx as u64)?)? {
                BlockMapping::Data(block) => self.image.read_block(block, &mut page)?,
                BlockMapping::Hole => page.fill(0),
                BlockMapping::NeedNode(_) => return Err(Ext4FormatError::Unsupported),
            }
            chunk.copy_from_slice(&page[..chunk.len()]);
        }
        Ok(out)
    }

    pub fn write_inode_meta_journaled(
        &mut self,
        inode: InodeNo,
        meta: InodeMetaLite,
    ) -> Result<JournalReceipt> {
        let journal_start = self.journal_start.ok_or(Ext4FormatError::Unsupported)?;
        let location = self.inode_location(inode)?;
        let mut home_block = [0u8; BLOCK_SIZE];
        self.image.read_block(location.block, &mut home_block)?;
        let mut disk_inode =
            Inode::parse(&home_block[location.offset..location.offset + location.len])?;
        apply_meta(&mut disk_inode, meta);
        disk_inode.encode(&mut home_block[location.offset..location.offset + location.len])?;

        let sequence = self.next_sequence;
        let descriptor_block = journal_start + 1 + ((sequence - 1) as u64 * 3);
        let payload_block = descriptor_block + 1;
        let commit_block = descriptor_block + 2;

        let mut descriptor = [0u8; BLOCK_SIZE];
        encode_journal_descriptor(sequence, location.block as u32, &mut descriptor)?;
        let mut commit = [0u8; BLOCK_SIZE];
        encode_journal_commit(sequence, &mut commit)?;

        self.image.write_block(descriptor_block, &descriptor)?;
        self.image.write_block(payload_block, &home_block)?;
        self.image.barrier()?;
        self.image.write_block(commit_block, &commit)?;
        self.image.barrier()?;
        self.next_sequence = self.next_sequence.saturating_add(1);

        Ok(JournalReceipt {
            sequence,
            target_block: location.block,
            descriptor_block,
            payload_block,
            commit_block,
        })
    }

    /// Read a symlink's target bytes.
    ///
    /// Handles both fast/inline symlinks (target in `i_block`, `size <= 60`)
    /// and block-based symlinks (target in the first data block).
    pub fn read_symlink(&mut self, inode: InodeNo) -> Result<Vec<u8>> {
        let disk_inode = self.read_inode(inode)?;
        if !disk_inode.is_symlink() {
            return Err(Ext4FormatError::Unsupported);
        }
        let size = disk_inode.size as usize;
        if size == 0 {
            return Ok(Vec::new());
        }
        if size <= 60 {
            // Fast symlink: target stored inline in i_block.
            Ok(disk_inode.extent_root_bytes()[..size].to_vec())
        } else {
            // Block-based symlink: target in the first data block.
            let mut page = [0u8; BLOCK_SIZE];
            match self.resolve_inode_block(&disk_inode, 0)? {
                BlockMapping::Data(block) => {
                    self.image.read_block(block, &mut page)?;
                    Ok(page[..size.min(BLOCK_SIZE)].to_vec())
                }
                BlockMapping::Hole => Ok(Vec::new()),
                BlockMapping::NeedNode(_) => Err(Ext4FormatError::Unsupported),
            }
        }
    }

    pub fn replay_journal_for_test(&mut self) -> Result<ReplayReport> {
        let journal_start = self.journal_start.ok_or(Ext4FormatError::Unsupported)?;
        let mut cursor = journal_start + 1;
        let mut transactions = 0u32;
        let mut blocks_replayed = 0u32;
        loop {
            if cursor + 2 >= self.image.total_blocks() {
                break;
            }
            let mut descriptor = [0u8; BLOCK_SIZE];
            self.image.read_block(cursor, &mut descriptor)?;
            let (header, tag) = match parse_journal_descriptor(&descriptor) {
                Ok((header, tag)) => (header, tag),
                _ => break,
            };
            let mut commit = [0u8; BLOCK_SIZE];
            self.image.read_block(cursor + 2, &mut commit)?;
            let commit = CommitHeader::parse(&commit)?;
            if commit.sequence != header.sequence {
                break;
            }
            let mut payload = [0u8; BLOCK_SIZE];
            self.image.read_block(cursor + 1, &mut payload)?;
            self.image.write_block(tag.block as u64, &payload)?;
            blocks_replayed += 1;
            transactions += 1;
            cursor += 3;
        }
        Ok(ReplayReport {
            transactions,
            blocks_replayed,
        })
    }

    /// Allocate a free inode in group 0, mark it used in the bitmap, and return
    /// its number. Returns `OutOfBounds` when every group bitmap is full.
    pub fn allocate_inode(&mut self) -> Result<InodeNo> {
        if self.superblock.inodes_per_group == 0 {
            return Err(Ext4FormatError::Corrupt);
        }
        let inodes_per_group = self.superblock.inodes_per_group as u64;
        let total_inodes = self.superblock.inodes_count as u64;

        for (group_index, group) in self.groups.iter().copied().enumerate() {
            let group_first_index = group_index as u64 * inodes_per_group;
            if group_first_index >= total_inodes {
                break;
            }
            let group_inode_count =
                core::cmp::min(inodes_per_group, total_inodes - group_first_index);
            let bitmap_block = group.inode_bitmap_block();
            let mut bitmap = [0u8; BLOCK_SIZE];
            self.image.read_block(bitmap_block, &mut bitmap)?;
            let view = BitmapView::new(&bitmap);
            let Some(bit) = (0..group_inode_count as usize).find(|bit| !view.is_set(*bit)) else {
                continue;
            };
            BitmapMut::new(&mut bitmap).set(bit)?;
            self.image.write_block(bitmap_block, &bitmap)?;
            return Ok(InodeNo::new((group_first_index + bit as u64 + 1) as u32));
        }

        Err(Ext4FormatError::OutOfBounds)
    }

    /// Write `inode` directly into the inode table (no journal).
    pub fn write_inode(&mut self, inode_no: InodeNo, inode: &Inode) -> Result<()> {
        let loc = self.inode_location(inode_no)?;
        let mut block = [0u8; BLOCK_SIZE];
        self.image.read_block(loc.block, &mut block)?;
        inode.encode(&mut block[loc.offset..loc.offset + loc.len])?;
        self.image.write_block(loc.block, &block)?;
        Ok(())
    }

    /// Allocate a free data block in group 0, mark it used, and return its
    /// absolute block number.
    pub fn allocate_block(&mut self) -> Result<u64> {
        if self.superblock.blocks_per_group == 0 {
            return Err(Ext4FormatError::Corrupt);
        }
        let blocks_per_group = self.superblock.blocks_per_group as u64;
        let first_data_block = self.superblock.first_data_block as u64;
        let total_blocks = core::cmp::min(self.superblock.blocks_count, self.image.total_blocks());

        for (group_index, group) in self.groups.iter().copied().enumerate() {
            let group_start = first_data_block + group_index as u64 * blocks_per_group;
            if group_start >= total_blocks {
                break;
            }
            let group_block_count = core::cmp::min(blocks_per_group, total_blocks - group_start);
            let bitmap_block = group.block_bitmap_block();
            let mut bitmap = [0u8; BLOCK_SIZE];
            self.image.read_block(bitmap_block, &mut bitmap)?;
            let view = BitmapView::new(&bitmap);
            let Some(bit) = (0..group_block_count as usize).find(|bit| !view.is_set(*bit)) else {
                continue;
            };
            BitmapMut::new(&mut bitmap).set(bit)?;
            self.image.write_block(bitmap_block, &bitmap)?;
            return Ok(group_start + bit as u64);
        }

        Err(Ext4FormatError::OutOfBounds)
    }

    /// Insert a new directory entry `(name → new_ino)` into an existing
    /// directory by finding slack space in its current data blocks.
    pub fn append_dir_entry(
        &mut self,
        dir_ino: InodeNo,
        name: &[u8],
        new_ino: InodeNo,
        file_type: u8,
    ) -> Result<()> {
        if name.len() > 255 {
            return Err(Ext4FormatError::OutOfBounds);
        }
        let new_min = (8usize + name.len() + 3) & !3;
        let disk_inode = self.read_inode(dir_ino)?;
        let page_count = div_ceil_u64(disk_inode.size, BLOCK_SIZE as u64);

        for page_index in 0..page_count {
            let mut page = [0u8; BLOCK_SIZE];
            let phys = match self.resolve_inode_block(&disk_inode, logical_block(page_index)?)? {
                BlockMapping::Data(b) => b,
                BlockMapping::Hole => continue,
                BlockMapping::NeedNode(_) => return Err(Ext4FormatError::Unsupported),
            };
            self.image.read_block(phys, &mut page)?;

            let mut off = 0usize;
            while off + 8 <= BLOCK_SIZE {
                let rec_len = read_u16_le(&page, off + 4)? as usize;
                if rec_len == 0 || off + rec_len > BLOCK_SIZE {
                    break;
                }
                let ino_here = u32::from_le_bytes(page[off..off + 4].try_into().unwrap());
                if ino_here == 0 {
                    if rec_len >= new_min {
                        encode_dir_entry(
                            new_ino.get(),
                            rec_len as u16,
                            file_type,
                            name,
                            &mut page[off..off + rec_len],
                        )?;
                        self.image.write_block(phys, &page)?;
                        return Ok(());
                    }
                } else {
                    let name_len = page[off + 6] as usize;
                    let used = (8 + name_len + 3) & !3;
                    let free = rec_len.saturating_sub(used);
                    if free >= new_min {
                        write_u16_le(&mut page, off + 4, used as u16)?;
                        encode_dir_entry(
                            new_ino.get(),
                            free as u16,
                            file_type,
                            name,
                            &mut page[off + used..off + rec_len],
                        )?;
                        self.image.write_block(phys, &page)?;
                        return Ok(());
                    }
                }
                off += rec_len;
            }
        }
        Err(Ext4FormatError::OutOfBounds)
    }

    /// Remove the directory entry named `name` from `dir_ino`.  Returns the
    /// inode number that was removed.
    pub fn remove_dir_entry(&mut self, dir_ino: InodeNo, name: &[u8]) -> Result<InodeNo> {
        let disk_inode = self.read_inode(dir_ino)?;
        let page_count = div_ceil_u64(disk_inode.size, BLOCK_SIZE as u64);

        for page_index in 0..page_count {
            let mut page = [0u8; BLOCK_SIZE];
            let phys = match self.resolve_inode_block(&disk_inode, logical_block(page_index)?)? {
                BlockMapping::Data(b) => b,
                BlockMapping::Hole => continue,
                BlockMapping::NeedNode(_) => return Err(Ext4FormatError::Unsupported),
            };
            self.image.read_block(phys, &mut page)?;

            let mut prev_off: Option<usize> = None;
            let mut off = 0usize;
            while off + 8 <= BLOCK_SIZE {
                let rec_len = read_u16_le(&page, off + 4)? as usize;
                if rec_len == 0 || off + rec_len > BLOCK_SIZE {
                    break;
                }
                let ino_here = u32::from_le_bytes(page[off..off + 4].try_into().unwrap());
                if ino_here != 0 {
                    let name_len = page[off + 6] as usize;
                    let name_end = off + 8 + name_len;
                    if name_end <= BLOCK_SIZE
                        && name_len == name.len()
                        && &page[off + 8..name_end] == name
                    {
                        let found_ino = InodeNo::new(ino_here);
                        if let Some(prev) = prev_off {
                            let prev_rec = read_u16_le(&page, prev + 4)? as usize;
                            let merged = (prev_rec + rec_len) as u16;
                            write_u16_le(&mut page, prev + 4, merged)?;
                        } else {
                            page[off..off + 4].fill(0);
                        }
                        self.image.write_block(phys, &page)?;
                        return Ok(found_ino);
                    }
                }
                prev_off = Some(off);
                off += rec_len;
            }
        }
        Err(Ext4FormatError::OutOfBounds)
    }

    /// Create a new regular file in `parent_ino`.  Returns the new inode.
    pub fn create_regular_file(
        &mut self,
        parent_ino: InodeNo,
        name: &[u8],
        mode: u16,
        uid: u32,
        gid: u32,
        now_sec: u32,
    ) -> Result<InodeNo> {
        let new_ino = self.allocate_inode()?;
        let mut inode = Inode::default();
        inode.mode = Inode::S_IFREG | (mode & 0o7777);
        inode.uid = uid;
        inode.gid = gid;
        inode.atime = now_sec;
        inode.ctime = now_sec;
        inode.mtime = now_sec;
        inode.links_count = 1;
        inode.flags = Inode::EXTENTS_FL;
        inode.set_extent_root(&[])?;
        self.write_inode(new_ino, &inode)?;
        self.append_dir_entry(parent_ino, name, new_ino, 1 /* EXT4_FT_REG_FILE */)?;
        Ok(new_ino)
    }

    /// Create a new directory in `parent_ino` with `.` and `..` entries.
    pub fn create_directory(
        &mut self,
        parent_ino: InodeNo,
        name: &[u8],
        mode: u16,
        uid: u32,
        gid: u32,
        now_sec: u32,
    ) -> Result<InodeNo> {
        let new_ino = self.allocate_inode()?;
        let data_block = self.allocate_block()?;

        let mut dir_data = [0u8; BLOCK_SIZE];
        encode_dir_entry(
            new_ino.get(),
            12,
            2, /* dir */
            b".",
            &mut dir_data[0..12],
        )?;
        encode_dir_entry(
            parent_ino.get(),
            (BLOCK_SIZE - 12) as u16,
            2,
            b"..",
            &mut dir_data[12..],
        )?;
        self.image.write_block(data_block, &dir_data)?;

        let mut inode = Inode::default();
        inode.mode = Inode::S_IFDIR | (mode & 0o7777);
        inode.uid = uid;
        inode.gid = gid;
        inode.size = BLOCK_SIZE as u64;
        inode.atime = now_sec;
        inode.ctime = now_sec;
        inode.mtime = now_sec;
        inode.links_count = 2;
        inode.blocks_512 = 8;
        inode.flags = Inode::EXTENTS_FL;
        inode.set_extent_root(&[Extent {
            logical_block: 0,
            len: 1,
            physical_start: data_block,
        }])?;
        self.write_inode(new_ino, &inode)?;
        self.append_dir_entry(parent_ino, name, new_ino, 2 /* EXT4_FT_DIR */)?;
        Ok(new_ino)
    }

    fn read_inode(&mut self, inode: InodeNo) -> Result<Inode> {
        let location = self.inode_location(inode)?;
        let mut block = [0u8; BLOCK_SIZE];
        self.image.read_block(location.block, &mut block)?;
        Inode::parse(&block[location.offset..location.offset + location.len])
    }

    fn read_dir_entries_from(
        &mut self,
        directory: InodeNo,
        skip_entries: u64,
        out: &mut [DirEntryLite],
    ) -> Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let disk_inode = self.read_inode(directory)?;
        if !disk_inode.is_dir() {
            return Err(Ext4FormatError::Unsupported);
        }

        let mut written = 0usize;
        let mut seen = 0u64;
        let page_count = div_ceil_u64(disk_inode.size, BLOCK_SIZE as u64);
        for page_index in 0..page_count {
            let mut page = [0u8; BLOCK_SIZE];
            match self.resolve_inode_block(&disk_inode, logical_block(page_index)?)? {
                BlockMapping::Data(block) => self.image.read_block(block, &mut page)?,
                BlockMapping::Hole => continue,
                BlockMapping::NeedNode(_) => return Err(Ext4FormatError::Unsupported),
            }
            for entry in DirEntryIter::new(&page) {
                let entry = entry?;
                if seen < skip_entries {
                    seen += 1;
                    continue;
                }
                out[written] = DirEntryLite::from_ondisk(entry)?;
                written += 1;
                seen += 1;
                if written == out.len() {
                    return Ok(written);
                }
            }
        }
        Ok(written)
    }

    pub fn read_dir_entries_from_offset(
        &mut self,
        directory: InodeNo,
        start_offset: u64,
        out: &mut [DirEntryLite],
        next_offsets: &mut [u64],
    ) -> Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        if next_offsets.len() < out.len() {
            return Err(Ext4FormatError::Corrupt);
        }
        let disk_inode = self.read_inode(directory)?;
        if !disk_inode.is_dir() {
            return Err(Ext4FormatError::Unsupported);
        }
        if start_offset >= disk_inode.size {
            return Ok(0);
        }

        let mut written = 0usize;
        let page_count = div_ceil_u64(disk_inode.size, BLOCK_SIZE as u64);
        let mut page_index = start_offset / BLOCK_SIZE as u64;
        let mut offset_in_page = (start_offset % BLOCK_SIZE as u64) as usize;

        while page_index < page_count {
            let mut page = [0u8; BLOCK_SIZE];
            match self.resolve_inode_block(&disk_inode, logical_block(page_index)?)? {
                BlockMapping::Data(block) => self.image.read_block(block, &mut page)?,
                BlockMapping::Hole => {
                    page_index += 1;
                    offset_in_page = 0;
                    continue;
                }
                BlockMapping::NeedNode(_) => return Err(Ext4FormatError::Unsupported),
            }

            while offset_in_page < BLOCK_SIZE {
                let absolute_offset = page_index * BLOCK_SIZE as u64 + offset_in_page as u64;
                if absolute_offset >= disk_inode.size {
                    return Ok(written);
                }
                if BLOCK_SIZE - offset_in_page < 8 {
                    return Err(Ext4FormatError::Truncated);
                }

                let rec_len = read_u16_le(&page, offset_in_page + 4)? as usize;
                if rec_len == 0 {
                    break;
                }
                if rec_len < 8 || offset_in_page + rec_len > BLOCK_SIZE {
                    return Err(Ext4FormatError::Corrupt);
                }

                let next_offset = absolute_offset + rec_len as u64;
                let inode = read_u32_le(&page, offset_in_page)?;
                if inode != 0 {
                    let name_len = page[offset_in_page + 6] as usize;
                    if name_len > rec_len - 8 {
                        return Err(Ext4FormatError::Corrupt);
                    }
                    let entry = DirEntry {
                        inode,
                        rec_len: rec_len as u16,
                        file_type: page[offset_in_page + 7],
                        name: &page[offset_in_page + 8..offset_in_page + 8 + name_len],
                    };
                    out[written] = DirEntryLite::from_ondisk(entry)?;
                    next_offsets[written] = next_offset;
                    written += 1;
                    if written == out.len() {
                        return Ok(written);
                    }
                }
                offset_in_page += rec_len;
            }

            page_index += 1;
            offset_in_page = 0;
        }

        Ok(written)
    }

    fn inode_location(&self, inode: InodeNo) -> Result<InodeLocation> {
        if inode.get() == 0 {
            return Err(Ext4FormatError::OutOfBounds);
        }
        let inode_index = inode.get() - 1;
        if self.superblock.inodes_per_group == 0 {
            return Err(Ext4FormatError::Corrupt);
        }
        let group_index = inode_index / self.superblock.inodes_per_group;
        let index_in_group = inode_index % self.superblock.inodes_per_group;
        let group = self
            .groups
            .get(group_index as usize)
            .ok_or(Ext4FormatError::OutOfBounds)?;
        InodeTableLayout {
            inode_size: self.superblock.inode_size as usize,
            inodes_per_group: self.superblock.inodes_per_group,
            first_inode_table_block: group.inode_table_block(),
        }
        .locate_index(index_in_group, BLOCK_SIZE)
    }

    fn resolve_inode_block(&mut self, inode: &Inode, logical_block: u32) -> Result<BlockMapping> {
        let mut node = ExtentNode::parse(inode.extent_root_bytes())?;
        for _ in 0..8 {
            match node {
                ExtentNode::Leaf(extents) => {
                    for extent in extents {
                        if let Some(block) = extent.physical_for(logical_block) {
                            return Ok(BlockMapping::Data(block));
                        }
                    }
                    return Ok(BlockMapping::Hole);
                }
                ExtentNode::Index(indexes) => {
                    if indexes.is_empty() {
                        return Ok(BlockMapping::Hole);
                    }
                    let mut selected = None;
                    for index in &indexes {
                        if index.logical_block <= logical_block {
                            selected = Some(index.child);
                        } else {
                            break;
                        }
                    }
                    let child = selected.unwrap_or(indexes[0].child);
                    let mut block = [0u8; BLOCK_SIZE];
                    self.image.read_block(child, &mut block)?;
                    node = ExtentNode::parse(&block)?;
                }
            }
        }
        Err(Ext4FormatError::Corrupt)
    }
}

/// Insert `(logical → physical)` into a logical-sorted extent list, growing the
/// preceding extent in place when the new block continues it contiguously
/// (the common sequential case → one growing extent), otherwise inserting a
/// fresh single-block extent at the sorted position.
fn coalesce_insert_extent(extents: &mut Vec<Extent>, logical: u32, physical: u64) {
    let pos = extents
        .iter()
        .position(|e| e.logical_block > logical)
        .unwrap_or(extents.len());
    if pos > 0 {
        let prev = &mut extents[pos - 1];
        let contiguous = prev.logical_block + prev.len as u32 == logical
            && prev.physical_start + prev.len as u64 == physical
            && (prev.len as u32) < Extent::UNINITIALIZED_MASK as u32;
        if contiguous {
            prev.len += 1;
            return;
        }
    }
    extents.insert(
        pos,
        Extent {
            logical_block: logical,
            len: 1,
            physical_start: physical,
        },
    );
}

fn read_group_descs<I: BlockImage>(image: &I, superblock: &Superblock) -> Result<Vec<GroupDesc>> {
    let group_count = superblock.group_count()? as usize;
    let desc_size = superblock.group_desc_size();
    let mut groups = Vec::new();
    let mut block = [0u8; BLOCK_SIZE];
    let gdt_start = if superblock.block_size() == 1024 {
        2
    } else {
        1
    };
    for idx in 0..group_count {
        let byte_offset = idx * desc_size;
        let block_id = gdt_start + (byte_offset / BLOCK_SIZE) as u64;
        let in_block = byte_offset % BLOCK_SIZE;
        image.read_block(block_id, &mut block)?;
        groups.push(GroupDesc::parse_sized(
            &block[in_block..in_block + desc_size],
            desc_size,
        )?);
    }
    Ok(groups)
}

fn inode_to_meta(inode: Inode) -> InodeMetaLite {
    InodeMetaLite {
        mode: inode.mode,
        uid: inode.uid,
        gid: inode.gid,
        size: inode.size,
        nlinks: inode.links_count as u32,
        blocks_512: inode.blocks_512,
        flags: inode.flags,
        atime: inode.atime,
        ctime: inode.ctime,
        mtime: inode.mtime,
    }
}

fn apply_meta(inode: &mut Inode, meta: InodeMetaLite) {
    inode.mode = meta.mode;
    inode.uid = meta.uid;
    inode.gid = meta.gid;
    inode.size = meta.size;
    inode.links_count = meta.nlinks as u16;
    inode.blocks_512 = meta.blocks_512;
    inode.flags = meta.flags;
    inode.atime = meta.atime;
    inode.ctime = meta.ctime;
    inode.mtime = meta.mtime;
}

fn logical_block(file_page_index: u64) -> Result<u32> {
    file_page_index
        .try_into()
        .map_err(|_| Ext4FormatError::OutOfBounds)
}

fn seconds_from_ns(timestamp_ns: u64) -> Result<u32> {
    (timestamp_ns / 1_000_000_000)
        .try_into()
        .map_err(|_| Ext4FormatError::OutOfBounds)
}

fn div_ceil_u64(value: u64, divisor: u64) -> u64 {
    if value == 0 {
        0
    } else {
        1 + (value - 1) / divisor
    }
}
