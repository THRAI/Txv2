use crate::ondisk::{
    encode_dir_entry, encode_journal_commit, encode_journal_descriptor, parse_journal_descriptor,
    BitmapMut, BitmapView, BlockMapping, CommitHeader, DirEntry, DirEntryIter, Extent,
    ExtentHeader, ExtentIdx, ExtentNode, GroupDesc, Inode, InodeLocation, InodeTableLayout,
    Superblock,
};
use crate::ondisk::{read_u16_le, read_u32_le, write_u16_le};
use crate::{Ext4FormatError, Result};
use alloc::vec;
use alloc::vec::Vec;

pub const BLOCK_SIZE: usize = 4096;
pub type Page4K = [u8; BLOCK_SIZE];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FilesystemStatsLite {
    pub block_size: u64,
    pub total_blocks: u64,
    pub free_blocks: u64,
    pub available_blocks: u64,
    pub total_inodes: u64,
    pub free_inodes: u64,
    pub max_name_len: u64,
}

pub trait BlockImage {
    fn total_blocks(&self) -> u64;
    fn read_block(&self, block: u64, out: &mut Page4K) -> Result<()>;
    fn write_block(&mut self, block: u64, data: &Page4K) -> Result<()>;
    fn barrier(&mut self) -> Result<()> {
        Ok(())
    }
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
pub struct RenameOutcome {
    /// Inode replaced at the destination and its remaining namespace links.
    /// A zero count tells the VFS backend to publish it as an orphan; physical
    /// reclamation still waits for the last live payload reference.
    pub displaced: Option<(InodeNo, u16)>,
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

enum TrimmedExtentNode {
    Leaf(Vec<Extent>),
    Index { depth: u16, indexes: Vec<ExtentIdx> },
}

impl TrimmedExtentNode {
    fn is_empty(&self) -> bool {
        match self {
            Self::Leaf(extents) => extents.is_empty(),
            Self::Index { indexes, .. } => indexes.is_empty(),
        }
    }

    fn first_logical(&self) -> Option<u32> {
        match self {
            Self::Leaf(extents) => extents.first().map(|extent| extent.logical_block),
            Self::Index { indexes, .. } => indexes.first().map(|index| index.logical_block),
        }
    }

    fn encode(&self, bytes: &mut [u8]) -> Result<()> {
        match self {
            Self::Leaf(extents) => ExtentNode::encode_leaf(extents, bytes),
            Self::Index { depth, indexes } => ExtentNode::encode_index(*depth, indexes, bytes),
        }
    }
}

#[derive(Clone, Copy)]
struct BlockRange {
    start: u64,
    len: u64,
}

impl BlockRange {
    fn new(start: u64, len: u64) -> Result<Self> {
        if len == 0 || start.checked_add(len).is_none() {
            return Err(Ext4FormatError::OutOfBounds);
        }
        Ok(Self { start, len })
    }

    fn end(self) -> u64 {
        self.start + self.len
    }
}

struct PendingExtentWrite {
    block: u64,
    before: Option<Page4K>,
    after: Page4K,
}

struct ExtentTreeMutation {
    inode: Inode,
    writes: Vec<PendingExtentWrite>,
    allocated_metadata: Vec<u64>,
}

struct ExtentInsertOutcome {
    first_logical: u32,
    split_right: Option<ExtentIdx>,
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

    pub fn into_inner(self) -> I {
        self.image
    }

    /// Read both allocation bitmaps in every live block group.
    ///
    /// Superblock free counters can be stale after an unclean shutdown and
    /// this driver deliberately performs metadata updates without relying on
    /// them for allocation. `statfs(2)` is infrequent, so scanning the group
    /// bitmaps is the authoritative and acceptably small path.
    pub fn filesystem_stats(&mut self) -> Result<FilesystemStatsLite> {
        let blocks_per_group = self.superblock.blocks_per_group as u64;
        let inodes_per_group = self.superblock.inodes_per_group as u64;
        if blocks_per_group == 0 || inodes_per_group == 0 {
            return Err(Ext4FormatError::Corrupt);
        }

        let first_data_block = self.superblock.first_data_block as u64;
        let total_blocks = core::cmp::min(self.superblock.blocks_count, self.image.total_blocks());
        let data_blocks = total_blocks.saturating_sub(first_data_block);
        let required_groups = div_ceil_u64(data_blocks, blocks_per_group) as usize;
        if required_groups > self.groups.len() {
            return Err(Ext4FormatError::Corrupt);
        }

        let total_inodes = self.superblock.inodes_count as u64;
        let mut free_blocks = 0u64;
        let mut free_inodes = 0u64;
        let mut bitmap = [0u8; BLOCK_SIZE];

        for (group_index, group) in self
            .groups
            .iter()
            .copied()
            .take(required_groups)
            .enumerate()
        {
            let group_block_start = group_index as u64 * blocks_per_group;
            let valid_blocks = core::cmp::min(
                blocks_per_group,
                data_blocks.saturating_sub(group_block_start),
            );
            self.image
                .read_block(group.block_bitmap_block(), &mut bitmap)?;
            free_blocks = free_blocks
                .checked_add(count_clear_bits(&bitmap, valid_blocks as usize)?)
                .ok_or(Ext4FormatError::OutOfBounds)?;

            let group_inode_start = group_index as u64 * inodes_per_group;
            let valid_inodes = core::cmp::min(
                inodes_per_group,
                total_inodes.saturating_sub(group_inode_start),
            );
            self.image
                .read_block(group.inode_bitmap_block(), &mut bitmap)?;
            free_inodes = free_inodes
                .checked_add(count_clear_bits(&bitmap, valid_inodes as usize)?)
                .ok_or(Ext4FormatError::OutOfBounds)?;
        }

        Ok(FilesystemStatsLite {
            block_size: self.superblock.block_size() as u64,
            total_blocks,
            free_blocks,
            // Final-stage tests run as uid 0. Until reserved-block accounting
            // is surfaced in Superblock, root can use every actually-free
            // block, so bavail and bfree are intentionally identical.
            available_blocks: free_blocks,
            total_inodes,
            free_inodes,
            max_name_len: 255,
        })
    }

    pub fn inode_meta(&mut self, inode: InodeNo) -> Result<InodeMetaLite> {
        self.read_inode(inode).map(inode_to_meta)
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
            BlockMapping::Hole | BlockMapping::Unwritten(_) => {
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
            BlockMapping::Unwritten(_) | BlockMapping::Hole | BlockMapping::NeedNode(_) => {
                return Err(Ext4FormatError::Unsupported)
            }
        };
        self.image.write_block(block, page)?;
        Ok(WritebackReceipt {
            inode,
            file_page_index,
            physical_block: block,
        })
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
        let disk_inode = self.read_inode(inode)?;
        let logical = logical_block(file_page_index)?;
        match self.resolve_inode_block(&disk_inode, logical)? {
            BlockMapping::Data(block) => self.image.write_block(block, page),
            BlockMapping::Unwritten(block) => {
                self.commit_new_extent_mapping(inode, disk_inode, logical, block, page, false, None)
            }
            BlockMapping::Hole => {
                let goal = self.extent_allocation_goal(&disk_inode, logical)?;
                let new_block = self.allocate_block_near(goal)?;
                self.commit_new_extent_mapping(
                    inode, disk_inode, logical, new_block, page, true, None,
                )
            }
            BlockMapping::NeedNode(_) => return Err(Ext4FormatError::Corrupt),
        }
    }

    /// Write one page while publishing a new logical size in the same inode
    /// update that attaches a new extent. Directory growth uses this path so
    /// a failure cannot leave a newly allocated block mapped beyond the old
    /// directory size.
    fn write_page_and_set_size(
        &mut self,
        inode: InodeNo,
        disk_inode: Inode,
        logical: u32,
        page: &Page4K,
        new_size: u64,
    ) -> Result<()> {
        match self.resolve_inode_block(&disk_inode, logical)? {
            BlockMapping::Data(block) => {
                self.image.write_block(block, page)?;
                let mut updated = disk_inode;
                updated.size = new_size;
                self.write_inode(inode, &updated)
            }
            BlockMapping::Unwritten(block) => self.commit_new_extent_mapping(
                inode,
                disk_inode,
                logical,
                block,
                page,
                false,
                Some(new_size),
            ),
            BlockMapping::Hole => {
                let goal = self.extent_allocation_goal(&disk_inode, logical)?;
                let new_block = self.allocate_block_near(goal)?;
                self.commit_new_extent_mapping(
                    inode,
                    disk_inode,
                    logical,
                    new_block,
                    page,
                    true,
                    Some(new_size),
                )
            }
            BlockMapping::NeedNode(_) => Err(Ext4FormatError::Corrupt),
        }
    }

    fn commit_new_extent_mapping(
        &mut self,
        inode: InodeNo,
        disk_inode: Inode,
        logical: u32,
        physical: u64,
        page: &Page4K,
        data_was_allocated: bool,
        new_size: Option<u64>,
    ) -> Result<()> {
        let mut mutation =
            match self.prepare_attach_data_block(inode, disk_inode, logical, physical) {
                Ok(mutation) => mutation,
                Err(err) => {
                    if data_was_allocated {
                        let _ = self.free_allocated_blocks(&[physical]);
                    }
                    return Err(err);
                }
            };

        let newly_allocated =
            mutation.allocated_metadata.len() as u64 + if data_was_allocated { 1 } else { 0 };
        let allocation_sectors = newly_allocated.checked_mul((BLOCK_SIZE / 512) as u64);
        let Some(new_blocks_512) = allocation_sectors else {
            self.release_extent_allocations(&mutation, physical, data_was_allocated);
            return Err(Ext4FormatError::OutOfBounds);
        };
        let Some(blocks_512) = mutation.inode.blocks_512.checked_add(new_blocks_512) else {
            self.release_extent_allocations(&mutation, physical, data_was_allocated);
            return Err(Ext4FormatError::OutOfBounds);
        };
        mutation.inode.blocks_512 = blocks_512;
        if let Some(new_size) = new_size {
            mutation.inode.size = new_size;
        }

        // Write data first. Until the extent nodes and inode are committed,
        // a freshly allocated data block is unreachable and can be safely
        // returned on every failure path. An unwritten block was already
        // owned by the inode and therefore must never be freed here.
        if let Err(err) = self.image.write_block(physical, page) {
            self.release_extent_allocations(&mutation, physical, data_was_allocated);
            return Err(err);
        }

        let committed = match self.commit_extent_writes(&mutation.writes) {
            Ok(count) => count,
            Err((err, count)) => {
                self.restore_extent_writes(&mutation.writes, count);
                self.release_extent_allocations(&mutation, physical, data_was_allocated);
                return Err(err);
            }
        };

        // Normal writeback leaves size to `serialize_inode_meta` /
        // `set_inode_size`. Directory growth passes `new_size` so extent
        // ownership and the enlarged directory boundary become visible in
        // one inode-table write.
        if let Err(err) = self.write_inode(inode, &mutation.inode) {
            self.restore_extent_writes(&mutation.writes, committed);
            self.release_extent_allocations(&mutation, physical, data_was_allocated);
            return Err(err);
        }
        Ok(())
    }

    fn release_extent_allocations(
        &mut self,
        mutation: &ExtentTreeMutation,
        physical: u64,
        data_was_allocated: bool,
    ) {
        let _ = self.free_allocated_blocks(&mutation.allocated_metadata);
        if data_was_allocated {
            let _ = self.free_allocated_blocks(&[physical]);
        }
    }

    /// Prepare an extent insertion without changing any existing extent
    /// node. New metadata blocks are reserved immediately, while old/new
    /// node images are retained in `writes` so the caller can commit or roll
    /// back the whole insertion around the inode-table update.
    fn prepare_attach_data_block(
        &mut self,
        inode_no: InodeNo,
        inode: Inode,
        logical: u32,
        physical: u64,
    ) -> Result<ExtentTreeMutation> {
        const INLINE_MAX_EXTENTS: usize = 4;
        const MAX_EXTENT_DEPTH: u16 = 5;

        let root_header = ExtentHeader::parse(inode.extent_root_bytes())?;
        let mut mutation = ExtentTreeMutation {
            inode,
            writes: Vec::new(),
            allocated_metadata: Vec::new(),
        };
        let prepared = (|| -> Result<()> {
            match ExtentNode::parse(mutation.inode.extent_root_bytes())? {
                ExtentNode::Leaf(mut extents) => {
                    if root_header.depth != 0 {
                        return Err(Ext4FormatError::Corrupt);
                    }
                    coalesce_insert_extent(&mut extents, logical, physical);
                    if extents.len() <= INLINE_MAX_EXTENTS {
                        mutation.inode.set_extent_root(&extents)?;
                        return Ok(());
                    }
                    // Inline root full: spill it into one external leaf. Future
                    // leaf overflow is handled by recursive splitting below.
                    let child = self.allocate_extent_metadata(&mut mutation, Some(physical))?;
                    let mut block = [0u8; BLOCK_SIZE];
                    ExtentNode::encode_leaf(&extents, &mut block)?;
                    mutation.writes.push(PendingExtentWrite {
                        block: child,
                        before: None,
                        after: block,
                    });
                    mutation.inode.set_extent_index_root(
                        &[ExtentIdx {
                            logical_block: extents[0].logical_block,
                            child,
                        }],
                        1,
                    )?;
                    Ok(())
                }
                ExtentNode::Index(mut indexes) => {
                    if root_header.depth == 0 || indexes.is_empty() {
                        return Err(Ext4FormatError::Corrupt);
                    }
                    let idx_pos = extent_index_position(&indexes, logical);
                    let child = indexes[idx_pos].child;
                    let outcome = self.prepare_insert_extent_node(
                        inode_no,
                        child,
                        root_header.depth - 1,
                        logical,
                        physical,
                        &mut mutation,
                    )?;
                    indexes[idx_pos].logical_block = outcome.first_logical;
                    if let Some(right) = outcome.split_right {
                        indexes.insert(idx_pos + 1, right);
                    }

                    if indexes.len() <= INLINE_MAX_EXTENTS {
                        mutation
                            .inode
                            .set_extent_index_root(&indexes, root_header.depth)?;
                        return Ok(());
                    }
                    if root_header.depth >= MAX_EXTENT_DEPTH {
                        return Err(Ext4FormatError::ExtentTreeFull {
                            inode: inode_no.get(),
                            logical_block: logical,
                            depth: root_header.depth,
                            entries: indexes.len() as u16,
                        });
                    }

                    // The four-entry inode root overflowed. Move its complete
                    // index into one external node and grow the root by one
                    // level. The external node has room for 340 entries.
                    let child = self.allocate_extent_metadata(&mut mutation, Some(child))?;
                    let mut block = [0u8; BLOCK_SIZE];
                    ExtentNode::encode_index(root_header.depth, &indexes, &mut block)?;
                    mutation.writes.push(PendingExtentWrite {
                        block: child,
                        before: None,
                        after: block,
                    });
                    mutation.inode.set_extent_index_root(
                        &[ExtentIdx {
                            logical_block: indexes[0].logical_block,
                            child,
                        }],
                        root_header.depth + 1,
                    )?;
                    Ok(())
                }
            }
        })();

        match prepared {
            Ok(()) => Ok(mutation),
            Err(err) => {
                let _ = self.free_allocated_blocks(&mutation.allocated_metadata);
                Err(err)
            }
        }
    }

    fn prepare_insert_extent_node(
        &mut self,
        inode_no: InodeNo,
        block_no: u64,
        expected_depth: u16,
        logical: u32,
        physical: u64,
        mutation: &mut ExtentTreeMutation,
    ) -> Result<ExtentInsertOutcome> {
        const MAX_EXTENT_DEPTH: u16 = 5;
        let node_capacity = (BLOCK_SIZE - 12) / 12;
        let mut before = [0u8; BLOCK_SIZE];
        self.image.read_block(block_no, &mut before)?;
        let header = ExtentHeader::parse(&before)?;
        if header.depth != expected_depth {
            return Err(Ext4FormatError::Corrupt);
        }

        match ExtentNode::parse(&before)? {
            ExtentNode::Leaf(mut extents) => {
                if expected_depth != 0 {
                    return Err(Ext4FormatError::Corrupt);
                }
                coalesce_insert_extent(&mut extents, logical, physical);
                if extents.len() <= node_capacity {
                    let mut after = [0u8; BLOCK_SIZE];
                    ExtentNode::encode_leaf(&extents, &mut after)?;
                    mutation.writes.push(PendingExtentWrite {
                        block: block_no,
                        before: Some(before),
                        after,
                    });
                    return Ok(ExtentInsertOutcome {
                        first_logical: extents[0].logical_block,
                        split_right: None,
                    });
                }

                let right_extents = extents.split_off(extents.len() / 2);
                let right_block =
                    self.allocate_extent_metadata(mutation, block_no.checked_add(1))?;
                let mut left_after = [0u8; BLOCK_SIZE];
                let mut right_after = [0u8; BLOCK_SIZE];
                ExtentNode::encode_leaf(&extents, &mut left_after)?;
                ExtentNode::encode_leaf(&right_extents, &mut right_after)?;
                mutation.writes.push(PendingExtentWrite {
                    block: block_no,
                    before: Some(before),
                    after: left_after,
                });
                mutation.writes.push(PendingExtentWrite {
                    block: right_block,
                    before: None,
                    after: right_after,
                });
                Ok(ExtentInsertOutcome {
                    first_logical: extents[0].logical_block,
                    split_right: Some(ExtentIdx {
                        logical_block: right_extents[0].logical_block,
                        child: right_block,
                    }),
                })
            }
            ExtentNode::Index(mut indexes) => {
                if indexes.is_empty() {
                    return Err(Ext4FormatError::Corrupt);
                }
                if expected_depth == 0 {
                    return Err(Ext4FormatError::Corrupt);
                }
                let idx_pos = extent_index_position(&indexes, logical);
                let child = indexes[idx_pos].child;
                let outcome = self.prepare_insert_extent_node(
                    inode_no,
                    child,
                    expected_depth - 1,
                    logical,
                    physical,
                    mutation,
                )?;
                indexes[idx_pos].logical_block = outcome.first_logical;
                if let Some(right) = outcome.split_right {
                    indexes.insert(idx_pos + 1, right);
                }

                if indexes.len() <= node_capacity {
                    let mut after = [0u8; BLOCK_SIZE];
                    ExtentNode::encode_index(expected_depth, &indexes, &mut after)?;
                    mutation.writes.push(PendingExtentWrite {
                        block: block_no,
                        before: Some(before),
                        after,
                    });
                    return Ok(ExtentInsertOutcome {
                        first_logical: indexes[0].logical_block,
                        split_right: None,
                    });
                }
                if expected_depth >= MAX_EXTENT_DEPTH {
                    return Err(Ext4FormatError::ExtentTreeFull {
                        inode: inode_no.get(),
                        logical_block: logical,
                        depth: expected_depth,
                        entries: indexes.len() as u16,
                    });
                }

                let right_indexes = indexes.split_off(indexes.len() / 2);
                let right_block =
                    self.allocate_extent_metadata(mutation, block_no.checked_add(1))?;
                let mut left_after = [0u8; BLOCK_SIZE];
                let mut right_after = [0u8; BLOCK_SIZE];
                ExtentNode::encode_index(expected_depth, &indexes, &mut left_after)?;
                ExtentNode::encode_index(expected_depth, &right_indexes, &mut right_after)?;
                mutation.writes.push(PendingExtentWrite {
                    block: block_no,
                    before: Some(before),
                    after: left_after,
                });
                mutation.writes.push(PendingExtentWrite {
                    block: right_block,
                    before: None,
                    after: right_after,
                });
                Ok(ExtentInsertOutcome {
                    first_logical: indexes[0].logical_block,
                    split_right: Some(ExtentIdx {
                        logical_block: right_indexes[0].logical_block,
                        child: right_block,
                    }),
                })
            }
        }
    }

    fn allocate_extent_metadata(
        &mut self,
        mutation: &mut ExtentTreeMutation,
        goal: Option<u64>,
    ) -> Result<u64> {
        let block = self.allocate_block_near(goal)?;
        mutation.allocated_metadata.push(block);
        Ok(block)
    }

    fn commit_extent_writes(
        &mut self,
        writes: &[PendingExtentWrite],
    ) -> core::result::Result<usize, (Ext4FormatError, usize)> {
        for (index, write) in writes.iter().enumerate() {
            if let Err(err) = self.image.write_block(write.block, &write.after) {
                return Err((err, index));
            }
        }
        Ok(writes.len())
    }

    fn restore_extent_writes(&mut self, writes: &[PendingExtentWrite], committed: usize) {
        for write in writes[..committed].iter().rev() {
            if let Some(before) = &write.before {
                let _ = self.image.write_block(write.block, before);
            }
        }
    }

    fn extent_allocation_goal(&mut self, inode: &Inode, logical: u32) -> Result<Option<u64>> {
        if logical > 0 {
            if let BlockMapping::Data(previous) = self.resolve_inode_block(inode, logical - 1)? {
                return Ok(previous.checked_add(1));
            }
        }
        if logical < u32::MAX {
            if let BlockMapping::Data(next) = self.resolve_inode_block(inode, logical + 1)? {
                return Ok(next.checked_sub(1));
            }
        }
        Ok(None)
    }

    /// Set the inode's logical size — backs `FsPageBacking::truncate`.
    /// Shrinking trims the extent tree and returns every block wholly beyond
    /// EOF. The VFS PageContainer path provides one coherent cache per inode
    /// and withdraws cached suffix pages after this operation succeeds.
    pub fn set_inode_size(&mut self, inode: InodeNo, new_size: u64) -> Result<()> {
        let current = self.read_inode(inode)?;
        self.set_inode_size_and_times(inode, new_size, current.mtime)
    }

    /// Persist a content-size change and its modification timestamps in the
    /// same inode-table update. Explicit truncate and page-cache writeback use
    /// this entry point so observers never see new bytes with an epoch-zero
    /// or otherwise stale `mtime`.
    pub fn set_inode_size_and_times(
        &mut self,
        inode: InodeNo,
        new_size: u64,
        now_sec: u32,
    ) -> Result<()> {
        let mut disk_inode = self.read_inode(inode)?;
        disk_inode.mtime = now_sec;
        disk_inode.ctime = now_sec;
        if new_size < disk_inode.size && disk_inode.flags & Inode::EXTENTS_FL != 0 {
            let keep_blocks = div_ceil_u64(new_size, BLOCK_SIZE as u64);
            let header = ExtentHeader::parse(disk_inode.extent_root_bytes())?;
            let root = ExtentNode::parse(disk_inode.extent_root_bytes())?;
            let mut freed = Vec::new();
            let trimmed = self.trim_extent_node(root, header.depth, keep_blocks, &mut freed)?;
            match trimmed {
                TrimmedExtentNode::Leaf(extents) => disk_inode.set_extent_root(&extents)?,
                TrimmedExtentNode::Index { indexes, .. } if indexes.is_empty() => {
                    disk_inode.set_extent_root(&[])?
                }
                TrimmedExtentNode::Index { depth, indexes } => {
                    disk_inode.set_extent_index_root(&indexes, depth)?
                }
            }
            let freed_blocks = normalize_block_ranges(&mut freed)?;
            disk_inode.blocks_512 = disk_inode
                .blocks_512
                .saturating_sub(freed_blocks * (BLOCK_SIZE / 512) as u64);
            disk_inode.size = new_size;
            self.write_inode(inode, &disk_inode)?;
            self.free_block_ranges(&freed)?;
            return Ok(());
        }
        disk_inode.size = new_size;
        self.write_inode(inode, &disk_inode)?;
        Ok(())
    }

    /// Namespace operations only own `i_links_count`. Always re-read the
    /// current inode before changing it: an earlier directory-entry update
    /// may have grown the same directory and replaced its size/extent root.
    fn set_inode_links(&mut self, inode_no: InodeNo, links_count: u16) -> Result<()> {
        let mut inode = self.read_inode(inode_no)?;
        inode.links_count = links_count;
        self.write_inode(inode_no, &inode)
    }

    /// Add another directory name for a non-directory inode and increment
    /// the inode's namespace link count.
    pub fn link_inode(&mut self, parent: InodeNo, name: &[u8], target: InodeNo) -> Result<()> {
        if self.lookup(parent, name)?.is_some() {
            return Err(Ext4FormatError::Corrupt);
        }
        let mut inode = self.read_inode(target)?;
        if inode.mode == 0 {
            return Err(Ext4FormatError::OutOfBounds);
        }
        if inode.is_dir() {
            return Err(Ext4FormatError::Unsupported);
        }
        inode.links_count = inode
            .links_count
            .checked_add(1)
            .ok_or(Ext4FormatError::OutOfBounds)?;
        let file_type = inode_file_type(&inode);
        self.append_dir_entry(parent, name, target, file_type)?;
        if let Err(err) = self.set_inode_links(target, inode.links_count) {
            let _ = self.remove_dir_entry(parent, name);
            return Err(err);
        }
        Ok(())
    }

    /// Remove one non-directory name and decrement the target's link count.
    /// Physical storage remains allocated until the ext4 backend's coherent
    /// PageContainer reaches its final reference and calls `destroy_inode`.
    pub fn unlink_inode(&mut self, parent: InodeNo, name: &[u8], target: InodeNo) -> Result<u16> {
        if self.lookup(parent, name)? != Some(target) {
            return Err(Ext4FormatError::OutOfBounds);
        }
        let mut inode = self.read_inode(target)?;
        if inode.is_dir() {
            return Err(Ext4FormatError::Unsupported);
        }
        if inode.links_count == 0 {
            return Err(Ext4FormatError::Corrupt);
        }
        let removed = self.remove_dir_entry(parent, name)?;
        if removed != target {
            return Err(Ext4FormatError::Corrupt);
        }
        inode.links_count -= 1;
        if let Err(err) = self.set_inode_links(target, inode.links_count) {
            let _ = self.append_dir_entry(parent, name, target, inode_file_type(&inode));
            return Err(err);
        }
        Ok(inode.links_count)
    }

    /// Remove an empty directory name and publish a zero-link orphan. The
    /// inode and its data block remain intact until `destroy_inode` is called
    /// at the last RNode payload reference.
    pub fn unlink_directory(
        &mut self,
        parent: InodeNo,
        name: &[u8],
        target: InodeNo,
    ) -> Result<()> {
        if self.lookup(parent, name)? != Some(target) {
            return Err(Ext4FormatError::OutOfBounds);
        }
        let mut target_inode = self.read_inode(target)?;
        if !target_inode.is_dir() {
            return Err(Ext4FormatError::Unsupported);
        }
        if !self.directory_is_empty(target)? {
            return Err(Ext4FormatError::NotEmpty);
        }
        let mut parent_inode = self.read_inode(parent)?;
        if !parent_inode.is_dir() {
            return Err(Ext4FormatError::Unsupported);
        }

        let removed = self.remove_dir_entry(parent, name)?;
        if removed != target {
            return Err(Ext4FormatError::Corrupt);
        }

        let old_target_links = target_inode.links_count;
        target_inode.links_count = 0;
        if let Err(err) = self.set_inode_links(target, target_inode.links_count) {
            let _ = self.append_dir_entry(parent, name, target, 2);
            return Err(err);
        }

        parent_inode.links_count = parent_inode.links_count.saturating_sub(1);
        if let Err(err) = self.set_inode_links(parent, parent_inode.links_count) {
            target_inode.links_count = old_target_links;
            let _ = self.set_inode_links(target, target_inode.links_count);
            let _ = self.append_dir_entry(parent, name, target, 2);
            return Err(err);
        }
        Ok(())
    }

    /// Move one namespace entry, optionally replacing an existing target.
    ///
    /// This operation owns the ext4 link-count side of rename. It never
    /// reclaims storage directly: a displaced zero-link inode is returned to
    /// the VFS backend, which publishes it to the orphan set and waits for the
    /// last RNode/PageContainer before calling `destroy_inode`.
    pub fn rename_inode(
        &mut self,
        old_parent: InodeNo,
        old_name: &[u8],
        new_parent: InodeNo,
        new_name: &[u8],
    ) -> Result<RenameOutcome> {
        if old_name.is_empty()
            || new_name.is_empty()
            || matches!(old_name, b"." | b"..")
            || matches!(new_name, b"." | b"..")
        {
            return Err(Ext4FormatError::OutOfBounds);
        }

        let old_ino = self
            .lookup(old_parent, old_name)?
            .ok_or(Ext4FormatError::OutOfBounds)?;
        if old_parent == new_parent && old_name == new_name {
            return Ok(RenameOutcome { displaced: None });
        }

        let source_inode = self.read_inode(old_ino)?;
        let source_is_dir = source_inode.is_dir();
        let source_file_type = inode_file_type(&source_inode);
        if source_is_dir && old_parent != new_parent {
            let mut cursor = new_parent;
            let mut reached_root = false;
            for _ in 0..=self.superblock.inodes_count {
                if cursor == old_ino {
                    return Err(Ext4FormatError::InvalidInput);
                }
                let parent = self
                    .lookup(cursor, b"..")?
                    .ok_or(Ext4FormatError::Corrupt)?;
                if parent == cursor {
                    reached_root = true;
                    break;
                }
                cursor = parent;
            }
            if !reached_root {
                return Err(Ext4FormatError::Corrupt);
            }
        }
        let displaced_ino = self.lookup(new_parent, new_name)?;
        if displaced_ino == Some(old_ino) {
            // POSIX: if old and new already name the same inode, rename is a
            // successful no-op and neither hard link is removed.
            return Ok(RenameOutcome { displaced: None });
        }

        let displaced_before = match displaced_ino {
            Some(inode) => Some((inode, self.read_inode(inode)?)),
            None => None,
        };
        if let Some((inode, target)) = displaced_before.as_ref() {
            if source_is_dir && !target.is_dir() {
                return Err(Ext4FormatError::NotDirectory);
            }
            if !source_is_dir && target.is_dir() {
                return Err(Ext4FormatError::IsDirectory);
            }
            if target.is_dir() && !self.directory_is_empty(*inode)? {
                return Err(Ext4FormatError::NotEmpty);
            }
            if target.links_count == 0 {
                return Err(Ext4FormatError::Corrupt);
            }
        }

        // Prepare every fallible metadata calculation before changing either
        // directory entry. Once namespace commit starts, subsequent failures
        // all pass through the rollback path below.
        let old_parent_before = self.read_inode(old_parent)?;
        let new_parent_before = if new_parent == old_parent {
            old_parent_before
        } else {
            self.read_inode(new_parent)?
        };
        let mut old_parent_after = old_parent_before;
        let mut new_parent_after = new_parent_before;
        if source_is_dir && old_parent != new_parent {
            old_parent_after.links_count = old_parent_after
                .links_count
                .checked_sub(1)
                .ok_or(Ext4FormatError::Corrupt)?;
            new_parent_after.links_count = new_parent_after
                .links_count
                .checked_add(1)
                .ok_or(Ext4FormatError::OutOfBounds)?;
        }
        if displaced_before
            .as_ref()
            .is_some_and(|(_, inode)| inode.is_dir())
        {
            new_parent_after.links_count = new_parent_after
                .links_count
                .checked_sub(1)
                .ok_or(Ext4FormatError::Corrupt)?;
        }
        let mut displaced_after = displaced_before;
        let displaced_remaining = displaced_after.as_mut().map(|(inode_no, inode)| {
            if inode.is_dir() {
                inode.links_count = 0;
            } else {
                inode.links_count -= 1;
            }
            (*inode_no, inode.links_count)
        });

        // First publish the destination name. Replacing in place avoids a
        // transient ENOSPC when rename is overwriting an existing entry.
        if let Some((target_ino, target_inode)) = displaced_before.as_ref() {
            self.replace_dir_entry(
                new_parent,
                new_name,
                old_ino,
                source_file_type,
                Some(*target_ino),
            )?;
            if let Err(err) = self.remove_dir_entry(old_parent, old_name) {
                let _ = self.replace_dir_entry(
                    new_parent,
                    new_name,
                    *target_ino,
                    inode_file_type(target_inode),
                    Some(old_ino),
                );
                return Err(err);
            }
        } else {
            self.append_dir_entry(new_parent, new_name, old_ino, source_file_type)?;
            if let Err(err) = self.remove_dir_entry(old_parent, old_name) {
                let _ = self.remove_dir_entry(new_parent, new_name);
                return Err(err);
            }
        }

        // Metadata is committed only after both namespace entries have been
        // changed. `append_dir_entry` may have grown either parent directory
        // and updated its size/extent tree. Never write the pre-rename parent
        // snapshot back wholesale here: that would hide the newly allocated
        // directory block. Re-read the current inode and change only nlink.
        let old_parent_links_changed =
            old_parent_after.links_count != old_parent_before.links_count;
        let new_parent_links_changed =
            new_parent_after.links_count != new_parent_before.links_count;
        let metadata_result = (|| {
            if source_is_dir && old_parent != new_parent {
                self.replace_dir_entry(old_ino, b"..", new_parent, 2, Some(old_parent))?;
            }
            if let Some((inode_no, inode)) = displaced_after.as_ref() {
                self.set_inode_links(*inode_no, inode.links_count)?;
            }
            if old_parent == new_parent {
                if new_parent_links_changed {
                    self.set_inode_links(old_parent, new_parent_after.links_count)?;
                }
            } else {
                if old_parent_links_changed {
                    self.set_inode_links(old_parent, old_parent_after.links_count)?;
                }
                if new_parent_links_changed {
                    self.set_inode_links(new_parent, new_parent_after.links_count)?;
                }
            }
            Ok(())
        })();

        if let Err(err) = metadata_result {
            if source_is_dir && old_parent != new_parent {
                let _ = self.replace_dir_entry(old_ino, b"..", old_parent, 2, Some(new_parent));
            }
            let _ = self.append_dir_entry(old_parent, old_name, old_ino, source_file_type);
            if let Some((target_ino, target_inode)) = displaced_before.as_ref() {
                let _ = self.replace_dir_entry(
                    new_parent,
                    new_name,
                    *target_ino,
                    inode_file_type(target_inode),
                    Some(old_ino),
                );
                let _ = self.set_inode_links(*target_ino, target_inode.links_count);
            } else {
                let _ = self.remove_dir_entry(new_parent, new_name);
            }
            // Roll back only the parent link counts. Namespace rollback may
            // itself grow a directory, so restoring a full old inode here
            // would repeat the same stale-size corruption.
            if old_parent == new_parent {
                if new_parent_links_changed {
                    let _ = self.set_inode_links(old_parent, old_parent_before.links_count);
                }
            } else {
                if old_parent_links_changed {
                    let _ = self.set_inode_links(old_parent, old_parent_before.links_count);
                }
                if new_parent_links_changed {
                    let _ = self.set_inode_links(new_parent, new_parent_before.links_count);
                }
            }
            return Err(err);
        }

        Ok(RenameOutcome {
            displaced: displaced_remaining,
        })
    }

    /// Reclaim an unlinked inode and every data/extent-tree block it owns.
    /// The VFS caller must already have proved that no payload references
    /// remain. Linked and already-cleared inodes are successful no-ops.
    pub fn destroy_inode(&mut self, inode_no: InodeNo) -> Result<()> {
        let inode = self.read_inode(inode_no)?;
        if inode.mode == 0 || inode.links_count != 0 {
            return Ok(());
        }

        let mut blocks = Vec::new();
        if inode.is_symlink() && inode.inline_symlink_target()?.is_some() {
            // Fast symlinks store their bytes inline in i_block.
        } else if inode.flags & Inode::EXTENTS_FL != 0 {
            let header = ExtentHeader::parse(inode.extent_root_bytes())?;
            let root = ExtentNode::parse(inode.extent_root_bytes())?;
            self.collect_extent_blocks(root, header.depth, &mut blocks)?;
        } else if inode.is_file() || inode.is_dir() || inode.is_symlink() {
            // Legacy indirect-block files need a separate walker. Never make
            // their inode number reusable while leaving referenced blocks
            // behind.
            return Err(Ext4FormatError::Unsupported);
        }
        if inode.file_acl != 0 {
            // External xattr blocks can be shared and carry their own
            // reference count. Reclaim requires decrementing that header
            // atomically; do not free/reuse the inode until that path exists.
            return Err(Ext4FormatError::Unsupported);
        }
        normalize_block_ranges(&mut blocks)?;

        // Clear the inode record before making its bitmap slot reusable. If
        // a later bitmap write fails, the result is a bounded disk-space leak
        // rather than a live inode pointing at blocks that may be reallocated.
        self.write_inode(inode_no, &Inode::default())?;
        self.free_block_ranges(&blocks)?;
        self.free_inode(inode_no)
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
                BlockMapping::Hole | BlockMapping::Unwritten(_) => page.fill(0),
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
                BlockMapping::Hole | BlockMapping::Unwritten(_) => Ok(Vec::new()),
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

    /// Allocate the first free block, preserving the legacy public API used
    /// by directory/inode helpers and host tests.
    pub fn allocate_block(&mut self) -> Result<u64> {
        self.allocate_block_near(None)
    }

    /// Allocate a free block near `goal`.
    ///
    /// Sequential file writeback passes the physical successor of the
    /// previous logical block. Searching that group and bit first lets the
    /// new block extend the existing extent instead of creating a one-block
    /// fragment. If the preferred region is full, groups and then bits wrap
    /// around exactly once.
    fn allocate_block_near(&mut self, goal: Option<u64>) -> Result<u64> {
        if self.superblock.blocks_per_group == 0 {
            return Err(Ext4FormatError::Corrupt);
        }
        let blocks_per_group = self.superblock.blocks_per_group as u64;
        let first_data_block = self.superblock.first_data_block as u64;
        let total_blocks = core::cmp::min(self.superblock.blocks_count, self.image.total_blocks());
        if self.groups.is_empty() || first_data_block >= total_blocks {
            return Err(Ext4FormatError::OutOfBounds);
        }

        let preferred_group = goal
            .filter(|block| *block >= first_data_block && *block < total_blocks)
            .map(|block| ((block - first_data_block) / blocks_per_group) as usize)
            .filter(|index| *index < self.groups.len())
            .unwrap_or(0);

        for group_offset in 0..self.groups.len() {
            let group_index = (preferred_group + group_offset) % self.groups.len();
            let group = self.groups[group_index];
            let group_start = first_data_block + group_index as u64 * blocks_per_group;
            if group_start >= total_blocks {
                continue;
            }
            let group_block_count = core::cmp::min(blocks_per_group, total_blocks - group_start);
            let bitmap_block = group.block_bitmap_block();
            let mut bitmap = [0u8; BLOCK_SIZE];
            self.image.read_block(bitmap_block, &mut bitmap)?;
            let view = BitmapView::new(&bitmap);
            let start_bit = if group_index == preferred_group {
                goal.filter(|block| {
                    *block >= group_start && *block < group_start + group_block_count
                })
                .map(|block| (block - group_start) as usize)
                .unwrap_or(0)
            } else {
                0
            };
            let bit_count = group_block_count as usize;
            let candidate = (start_bit..bit_count)
                .chain(0..start_bit)
                .find(|bit| !view.is_set(*bit));
            let Some(bit) = candidate else {
                continue;
            };
            BitmapMut::new(&mut bitmap).set(bit)?;
            self.image.write_block(bitmap_block, &bitmap)?;
            return Ok(group_start + bit as u64);
        }

        Err(Ext4FormatError::OutOfBounds)
    }

    fn free_allocated_blocks(&mut self, blocks: &[u64]) -> Result<()> {
        if blocks.is_empty() {
            return Ok(());
        }
        let mut ranges = Vec::with_capacity(blocks.len());
        for block in blocks.iter().copied() {
            ranges.push(BlockRange::new(block, 1)?);
        }
        normalize_block_ranges(&mut ranges)?;
        self.free_block_ranges(&ranges)
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
        if !disk_inode.is_dir() {
            return Err(Ext4FormatError::NotDirectory);
        }
        if disk_inode.size % BLOCK_SIZE as u64 != 0 {
            return Err(Ext4FormatError::Corrupt);
        }
        let page_count = div_ceil_u64(disk_inode.size, BLOCK_SIZE as u64);

        for page_index in 0..page_count {
            let mut page = [0u8; BLOCK_SIZE];
            let phys = match self.resolve_inode_block(&disk_inode, logical_block(page_index)?)? {
                BlockMapping::Data(b) => b,
                BlockMapping::Hole | BlockMapping::Unwritten(_) => continue,
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
        // No existing directory block has enough slack. Grow the directory
        // by one block and make the new entry consume that block's full
        // record.  The previous implementation returned OutOfBounds here,
        // which surfaced as ENOENT once Cargo filled the first 4 KiB
        // `.fingerprint` directory block during BuildStorm.
        let mut page = [0u8; BLOCK_SIZE];
        encode_dir_entry(new_ino.get(), BLOCK_SIZE as u16, file_type, name, &mut page)?;
        let new_size = page_count
            .checked_add(1)
            .and_then(|pages| pages.checked_mul(BLOCK_SIZE as u64))
            .ok_or(Ext4FormatError::OutOfBounds)?;
        self.write_page_and_set_size(
            dir_ino,
            disk_inode,
            logical_block(page_count)?,
            &page,
            new_size,
        )
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
                BlockMapping::Hole | BlockMapping::Unwritten(_) => continue,
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

    /// Replace the inode and type of an existing directory entry without
    /// changing its record length or name.
    fn replace_dir_entry(
        &mut self,
        dir_ino: InodeNo,
        name: &[u8],
        new_ino: InodeNo,
        file_type: u8,
        expected_old: Option<InodeNo>,
    ) -> Result<()> {
        let disk_inode = self.read_inode(dir_ino)?;
        let page_count = div_ceil_u64(disk_inode.size, BLOCK_SIZE as u64);

        for page_index in 0..page_count {
            let mut page = [0u8; BLOCK_SIZE];
            let phys = match self.resolve_inode_block(&disk_inode, logical_block(page_index)?)? {
                BlockMapping::Data(block) => block,
                BlockMapping::Hole | BlockMapping::Unwritten(_) => continue,
                BlockMapping::NeedNode(_) => return Err(Ext4FormatError::Unsupported),
            };
            self.image.read_block(phys, &mut page)?;

            let mut off = 0usize;
            while off + 8 <= BLOCK_SIZE {
                let rec_len = read_u16_le(&page, off + 4)? as usize;
                if rec_len == 0 || off + rec_len > BLOCK_SIZE {
                    break;
                }
                let inode = InodeNo::new(read_u32_le(&page, off)?);
                let name_len = page[off + 6] as usize;
                let name_end = off + 8 + name_len;
                if inode.get() != 0
                    && name_end <= BLOCK_SIZE
                    && name_len == name.len()
                    && &page[off + 8..name_end] == name
                {
                    if expected_old.is_some_and(|expected| expected != inode) {
                        return Err(Ext4FormatError::Corrupt);
                    }
                    page[off..off + 4].copy_from_slice(&new_ino.get().to_le_bytes());
                    page[off + 7] = file_type;
                    self.image.write_block(phys, &page)?;
                    return Ok(());
                }
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
        if let Err(err) = self.write_inode(new_ino, &inode) {
            let _ = self.free_inode(new_ino);
            return Err(err);
        }
        if let Err(err) =
            self.append_dir_entry(parent_ino, name, new_ino, 1 /* EXT4_FT_REG_FILE */)
        {
            inode.links_count = 0;
            let _ = self.write_inode(new_ino, &inode);
            let _ = self.destroy_inode(new_ino);
            return Err(err);
        }
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
        let parent_inode = self.read_inode(parent_ino)?;
        if !parent_inode.is_dir() {
            return Err(Ext4FormatError::NotDirectory);
        }
        let parent_links_after = parent_inode
            .links_count
            .checked_add(1)
            .ok_or(Ext4FormatError::OutOfBounds)?;

        let new_ino = self.allocate_inode()?;
        let data_block = match self.allocate_block() {
            Ok(block) => block,
            Err(err) => {
                let _ = self.free_inode(new_ino);
                return Err(err);
            }
        };

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
        if let Err(err) = self.image.write_block(data_block, &dir_data) {
            let _ = self.free_block_ranges(&[BlockRange {
                start: data_block,
                len: 1,
            }]);
            let _ = self.free_inode(new_ino);
            return Err(err);
        }

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
        if let Err(err) = self.write_inode(new_ino, &inode) {
            let _ = self.free_block_ranges(&[BlockRange {
                start: data_block,
                len: 1,
            }]);
            let _ = self.free_inode(new_ino);
            return Err(err);
        }
        if let Err(err) = self.append_dir_entry(parent_ino, name, new_ino, 2 /* EXT4_FT_DIR */) {
            inode.links_count = 0;
            let _ = self.write_inode(new_ino, &inode);
            let _ = self.destroy_inode(new_ino);
            return Err(err);
        }
        if let Err(err) = self.set_inode_links(parent_ino, parent_links_after) {
            let _ = self.remove_dir_entry(parent_ino, name);
            inode.links_count = 0;
            let _ = self.set_inode_links(new_ino, inode.links_count);
            let _ = self.destroy_inode(new_ino);
            return Err(err);
        }
        Ok(new_ino)
    }

    fn trim_extent_node(
        &mut self,
        node: ExtentNode,
        depth: u16,
        keep_blocks: u64,
        freed: &mut Vec<BlockRange>,
    ) -> Result<TrimmedExtentNode> {
        match node {
            ExtentNode::Leaf(extents) => {
                if depth != 0 {
                    return Err(Ext4FormatError::Corrupt);
                }
                let mut kept = Vec::with_capacity(extents.len());
                for mut extent in extents {
                    let actual_len = extent.actual_len();
                    if actual_len == 0 {
                        return Err(Ext4FormatError::Corrupt);
                    }
                    let extent_start = extent.logical_block as u64;
                    let keep_len = keep_blocks
                        .saturating_sub(extent_start)
                        .min(actual_len as u64) as u32;
                    let free_len = actual_len - keep_len;
                    if free_len != 0 {
                        freed.push(BlockRange::new(
                            extent.physical_start + keep_len as u64,
                            free_len as u64,
                        )?);
                    }
                    if keep_len != 0 {
                        extent.len = trimmed_extent_len(extent, keep_len)?;
                        kept.push(extent);
                    }
                }
                Ok(TrimmedExtentNode::Leaf(kept))
            }
            ExtentNode::Index(indexes) => {
                if depth == 0 {
                    return Err(Ext4FormatError::Corrupt);
                }
                let mut kept = Vec::with_capacity(indexes.len());
                for mut index in indexes {
                    let mut block = [0u8; BLOCK_SIZE];
                    self.image.read_block(index.child, &mut block)?;
                    let child_header = ExtentHeader::parse(&block)?;
                    if child_header.depth + 1 != depth {
                        return Err(Ext4FormatError::Corrupt);
                    }
                    let child = ExtentNode::parse(&block)?;
                    let trimmed =
                        self.trim_extent_node(child, child_header.depth, keep_blocks, freed)?;
                    if trimmed.is_empty() {
                        freed.push(BlockRange::new(index.child, 1)?);
                    } else {
                        index.logical_block =
                            trimmed.first_logical().ok_or(Ext4FormatError::Corrupt)?;
                        trimmed.encode(&mut block)?;
                        self.image.write_block(index.child, &block)?;
                        kept.push(index);
                    }
                }
                Ok(TrimmedExtentNode::Index {
                    depth,
                    indexes: kept,
                })
            }
        }
    }

    fn directory_is_empty(&mut self, inode: InodeNo) -> Result<bool> {
        let mut entries = [DirEntryLite::empty(); 8];
        let mut offset = 0u64;
        loop {
            let count = self.read_dir_entries_from(inode, offset, &mut entries)?;
            if count == 0 {
                return Ok(true);
            }
            for entry in entries.iter().take(count) {
                if entry.name() != b"." && entry.name() != b".." {
                    return Ok(false);
                }
            }
            offset += count as u64;
        }
    }

    fn collect_extent_blocks(
        &mut self,
        node: ExtentNode,
        depth: u16,
        blocks: &mut Vec<BlockRange>,
    ) -> Result<()> {
        match node {
            ExtentNode::Leaf(extents) => {
                if depth != 0 {
                    return Err(Ext4FormatError::Corrupt);
                }
                for extent in extents {
                    let actual_len = extent.actual_len();
                    if actual_len == 0 {
                        return Err(Ext4FormatError::Corrupt);
                    }
                    blocks.push(BlockRange::new(extent.physical_start, actual_len as u64)?);
                }
            }
            ExtentNode::Index(indexes) => {
                if depth == 0 {
                    return Err(Ext4FormatError::Corrupt);
                }
                for index in indexes {
                    let mut block = [0u8; BLOCK_SIZE];
                    self.image.read_block(index.child, &mut block)?;
                    let header = ExtentHeader::parse(&block)?;
                    if header.depth + 1 != depth {
                        return Err(Ext4FormatError::Corrupt);
                    }
                    self.collect_extent_blocks(ExtentNode::parse(&block)?, header.depth, blocks)?;
                    blocks.push(BlockRange::new(index.child, 1)?);
                }
            }
        }
        Ok(())
    }

    fn free_block_ranges(&mut self, ranges: &[BlockRange]) -> Result<()> {
        if ranges.is_empty() {
            return Ok(());
        }
        if self.superblock.blocks_per_group == 0 {
            return Err(Ext4FormatError::Corrupt);
        }
        let blocks_per_group = self.superblock.blocks_per_group as u64;
        let first_data_block = self.superblock.first_data_block as u64;
        let total_blocks = core::cmp::min(self.superblock.blocks_count, self.image.total_blocks());
        let mut range_index = 0usize;
        let mut block = ranges[0].start;

        while range_index < ranges.len() {
            if block < first_data_block || block >= total_blocks {
                return Err(Ext4FormatError::OutOfBounds);
            }
            let group_index = ((block - first_data_block) / blocks_per_group) as usize;
            let group = self
                .groups
                .get(group_index)
                .copied()
                .ok_or(Ext4FormatError::OutOfBounds)?;
            let group_start = first_data_block + group_index as u64 * blocks_per_group;
            let group_end = core::cmp::min(group_start + blocks_per_group, total_blocks);
            let mut bitmap = [0u8; BLOCK_SIZE];
            self.image
                .read_block(group.block_bitmap_block(), &mut bitmap)?;
            let mut view = BitmapMut::new(&mut bitmap);

            while range_index < ranges.len() {
                let range = ranges[range_index];
                if block < range.start || block >= range.end() {
                    return Err(Ext4FormatError::Corrupt);
                }
                if block >= group_end {
                    break;
                }
                let segment_end = core::cmp::min(range.end(), group_end);
                for candidate in block..segment_end {
                    let bit = (candidate - group_start) as usize;
                    if !view.is_set(bit) {
                        return Err(Ext4FormatError::Corrupt);
                    }
                    view.clear(bit)?;
                }
                block = segment_end;
                if block == range.end() {
                    range_index += 1;
                    if let Some(next) = ranges.get(range_index) {
                        block = next.start;
                    }
                }
            }
            self.image
                .write_block(group.block_bitmap_block(), &bitmap)?;
        }
        Ok(())
    }

    fn free_inode(&mut self, inode: InodeNo) -> Result<()> {
        if inode.get() == 0 || self.superblock.inodes_per_group == 0 {
            return Err(Ext4FormatError::OutOfBounds);
        }
        let inode_index = inode.get() as u64 - 1;
        if inode_index >= self.superblock.inodes_count as u64 {
            return Err(Ext4FormatError::OutOfBounds);
        }
        let inodes_per_group = self.superblock.inodes_per_group as u64;
        let group_index = (inode_index / inodes_per_group) as usize;
        let bit = (inode_index % inodes_per_group) as usize;
        let group = self
            .groups
            .get(group_index)
            .copied()
            .ok_or(Ext4FormatError::OutOfBounds)?;
        let mut bitmap = [0u8; BLOCK_SIZE];
        self.image
            .read_block(group.inode_bitmap_block(), &mut bitmap)?;
        let mut view = BitmapMut::new(&mut bitmap);
        if !view.is_set(bit) {
            return Err(Ext4FormatError::Corrupt);
        }
        view.clear(bit)?;
        self.image.write_block(group.inode_bitmap_block(), &bitmap)
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
                BlockMapping::Hole | BlockMapping::Unwritten(_) => continue,
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
                BlockMapping::Hole | BlockMapping::Unwritten(_) => {
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
                        if extent.contains(logical_block) {
                            return Ok(BlockMapping::Unwritten(
                                extent.physical_start
                                    + (logical_block - extent.logical_block) as u64,
                            ));
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
    if let Some(index) = extents.iter().position(|extent| extent.contains(logical)) {
        let existing = extents[index];
        if existing.is_initialized() {
            return;
        }

        // Materialising one block inside an unwritten extent splits the
        // unwritten range around the newly initialised block.
        let offset = logical - existing.logical_block;
        let suffix = existing.actual_len() - offset - 1;
        extents.remove(index);
        let mut insert_at = index;
        if offset != 0 {
            extents.insert(
                insert_at,
                Extent {
                    logical_block: existing.logical_block,
                    len: Extent::UNINITIALIZED_MASK + offset as u16,
                    physical_start: existing.physical_start,
                },
            );
            insert_at += 1;
        }
        extents.insert(
            insert_at,
            Extent {
                logical_block: logical,
                len: 1,
                physical_start: physical,
            },
        );
        if suffix != 0 {
            extents.insert(
                insert_at + 1,
                Extent {
                    logical_block: logical + 1,
                    len: Extent::UNINITIALIZED_MASK + suffix as u16,
                    physical_start: existing.physical_start + offset as u64 + 1,
                },
            );
        }
        return;
    }

    let pos = extents
        .iter()
        .position(|e| e.logical_block > logical)
        .unwrap_or(extents.len());
    extents.insert(
        pos,
        Extent {
            logical_block: logical,
            len: 1,
            physical_start: physical,
        },
    );

    // Random-order writeback can fill a one-block gap between two existing
    // extents. Merge both sides when their logical and physical ranges meet,
    // keeping the tree compact independently of flush order.
    let mut current = pos;
    if current > 0 && merge_adjacent_extents(extents, current - 1) {
        current -= 1;
    }
    let _ = merge_adjacent_extents(extents, current);
}

fn extent_index_position(indexes: &[ExtentIdx], logical: u32) -> usize {
    indexes
        .iter()
        .rposition(|index| index.logical_block <= logical)
        .unwrap_or(0)
}

fn merge_adjacent_extents(extents: &mut Vec<Extent>, left_index: usize) -> bool {
    let Some(right_index) = left_index.checked_add(1) else {
        return false;
    };
    if right_index >= extents.len() {
        return false;
    }
    let left = extents[left_index];
    let right = extents[right_index];
    if !left.is_initialized() || !right.is_initialized() {
        return false;
    }
    let left_len = left.actual_len();
    let right_len = right.actual_len();
    let Some(combined) = left_len.checked_add(right_len) else {
        return false;
    };
    if combined > Extent::UNINITIALIZED_MASK as u32
        || left.logical_block.checked_add(left_len) != Some(right.logical_block)
        || left.physical_start.checked_add(left_len as u64) != Some(right.physical_start)
    {
        return false;
    }
    extents[left_index].len = combined as u16;
    extents.remove(right_index);
    true
}

fn trimmed_extent_len(extent: Extent, kept: u32) -> Result<u16> {
    if kept == 0 || kept > extent.actual_len() {
        return Err(Ext4FormatError::Corrupt);
    }
    if extent.is_initialized() {
        kept.try_into().map_err(|_| Ext4FormatError::OutOfBounds)
    } else {
        if kept >= Extent::UNINITIALIZED_MASK as u32 {
            return Err(Ext4FormatError::OutOfBounds);
        }
        Ok(Extent::UNINITIALIZED_MASK + kept as u16)
    }
}

fn normalize_block_ranges(ranges: &mut Vec<BlockRange>) -> Result<u64> {
    ranges.sort_unstable_by_key(|range| range.start);
    let mut merged: Vec<BlockRange> = Vec::with_capacity(ranges.len());
    let mut total = 0u64;
    for range in ranges.iter().copied() {
        if range.len == 0 {
            return Err(Ext4FormatError::Corrupt);
        }
        if let Some(last) = merged.last_mut() {
            if range.start < last.end() {
                return Err(Ext4FormatError::Corrupt);
            }
            if range.start == last.end() {
                last.len = last
                    .len
                    .checked_add(range.len)
                    .ok_or(Ext4FormatError::OutOfBounds)?;
                total = total
                    .checked_add(range.len)
                    .ok_or(Ext4FormatError::OutOfBounds)?;
                continue;
            }
        }
        total = total
            .checked_add(range.len)
            .ok_or(Ext4FormatError::OutOfBounds)?;
        merged.push(range);
    }
    *ranges = merged;
    Ok(total)
}

fn inode_file_type(inode: &Inode) -> u8 {
    match inode.mode & Inode::S_IFMT {
        Inode::S_IFREG => 1,
        Inode::S_IFDIR => 2,
        0x2000 => 3, // EXT4_FT_CHRDEV
        0x6000 => 4, // EXT4_FT_BLKDEV
        0x1000 => 5, // EXT4_FT_FIFO
        0xC000 => 6, // EXT4_FT_SOCK
        Inode::S_IFLNK => 7,
        _ => 0,
    }
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

fn div_ceil_u64(value: u64, divisor: u64) -> u64 {
    if value == 0 {
        0
    } else {
        1 + (value - 1) / divisor
    }
}

fn count_clear_bits(bitmap: &[u8], valid_bits: usize) -> Result<u64> {
    if valid_bits > bitmap.len().saturating_mul(8) {
        return Err(Ext4FormatError::Corrupt);
    }

    let full_bytes = valid_bits / 8;
    let tail_bits = valid_bits % 8;
    let set_in_full = bitmap[..full_bytes]
        .iter()
        .map(|byte| byte.count_ones() as u64)
        .sum::<u64>();
    let set_in_tail = if tail_bits == 0 {
        0
    } else {
        (bitmap[full_bytes] & ((1u8 << tail_bits) - 1)).count_ones() as u64
    };
    Ok(valid_bits as u64 - set_in_full - set_in_tail)
}
