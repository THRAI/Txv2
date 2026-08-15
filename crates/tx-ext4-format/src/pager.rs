use crate::journal::Jbd2Superblock;
use crate::ondisk::{
    block_bitmap_csum32, crc32c, encode_dir_entry, encode_journal_commit,
    encode_journal_descriptor, group_desc_csum16, inode_bitmap_csum32, inode_csum32,
    parse_journal_descriptor, superblock_csum32, BitmapMut, BitmapView, BlockMapping, CommitHeader,
    DirEntry, DirEntryIter, Extent, ExtentHeader, ExtentIdx, ExtentNode, GroupDesc, Inode,
    InodeLocation, InodeTableLayout, Superblock,
};
use crate::ondisk::{
    dirblock_csum32, extent_block_csum32, read_u16_le, read_u32_le, write_u16_le, write_u32_le,
};
use crate::{Ext4FormatError, Result};
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec;
use alloc::vec::Vec;

use crate::mutation::{
    BlockClaim, Ext4MutationPlan, FsyncStamp, MetaRole, MetadataBlock, MutationOrigin,
    SealedDataWrite, SetAttr,
};

pub const BLOCK_SIZE: usize = 4096;
pub type Page4K = [u8; BLOCK_SIZE];
const EXT4_MAX_EXTENT_DEPTH: u16 = 5;

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

enum TruncateSubtreeState {
    Removed,
    Retained {
        node: ExtentNode,
        first_key: u32,
        changed: bool,
    },
}

struct TruncateSubtreePlan {
    state: TruncateSubtreeState,
    original_first_key: Option<u32>,
    original_logical_end: Option<u32>,
    released: Vec<u64>,
    retained_data: Vec<PhysicalBlockRange>,
    retained_homes: Vec<u64>,
    metadata: Vec<MetadataBlock>,
}

#[derive(Clone, Copy)]
struct PhysicalBlockRange {
    start: u64,
    end: u64,
}

impl PhysicalBlockRange {
    fn contains(self, block: u64) -> bool {
        self.start <= block && block < self.end
    }

    fn overlaps(self, other: Self) -> bool {
        self.start < other.end && other.start < self.end
    }
}

/// Collapse a sequence of planner snapshots into one atomic mutation.
/// Metadata homes must form an exact checksum chain; the first snapshot owns
/// the on-disk before-version and the last snapshot owns the committed bytes.
fn merge_chained_mutation(combined: &mut Ext4MutationPlan, next: &Ext4MutationPlan) -> Result<()> {
    if combined.object != next.object {
        return Err(Ext4FormatError::Corrupt);
    }
    for metadata in &next.metadata {
        if let Some(existing) = combined
            .metadata
            .iter_mut()
            .find(|existing| existing.home == metadata.home)
        {
            if existing.role != metadata.role
                || crc32c(0, &existing.after) as u64 != metadata.before_version
            {
                return Err(Ext4FormatError::Corrupt);
            }
            existing.after = metadata.after;
            for dependency in &metadata.depends_on {
                if !existing.depends_on.contains(dependency) {
                    existing.depends_on.push(*dependency);
                }
            }
        } else {
            combined
                .push_metadata(metadata.clone())
                .map_err(|_| Ext4FormatError::Corrupt)?;
        }
    }
    combined.data.extend(next.data.iter().cloned());
    combined
        .allocations
        .extend(next.allocations.iter().copied());
    combined.revokes.extend(next.revokes.iter().copied());
    combined
        .deferred_frees
        .extend(next.deferred_frees.iter().copied());
    combined
        .allocations
        .sort_unstable_by_key(|claim| claim.physical_block);
    combined
        .allocations
        .dedup_by_key(|claim| claim.physical_block);
    combined.revokes.sort_unstable();
    combined.revokes.dedup();
    combined.deferred_frees.sort_unstable();
    combined.deferred_frees.dedup();
    Ok(())
}

pub trait BlockImage {
    fn total_blocks(&self) -> u64;
    fn read_block(&self, block: u64, out: &mut Page4K) -> Result<()>;

    /// Read one file-data block, allowing the image adapter to prefetch
    /// adjacent blocks into its private cache. Metadata callers continue to
    /// use `read_block` so a speculative data read cannot widen metadata I/O.
    fn read_data_block(&self, block: u64, out: &mut Page4K) -> Result<()> {
        self.read_block(block, out)
    }

    fn write_block(&mut self, block: u64, data: &Page4K) -> Result<()>;

    /// Write one physically contiguous block run. Image adapters with a
    /// scatter/gather-capable device should override this method; the default
    /// preserves existing format tests and compatibility images.
    fn write_blocks(&mut self, first_block: u64, data: &[&Page4K]) -> Result<()> {
        for (index, page) in data.iter().enumerate() {
            let block = first_block
                .checked_add(index as u64)
                .ok_or(Ext4FormatError::OutOfBounds)?;
            self.write_block(block, page)?;
        }
        Ok(())
    }

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
    pub generation: u32,
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
    pending_metadata: BTreeMap<u64, Page4K>,
    superblock: Superblock,
    groups: Vec<GroupDesc>,
    inode_table: InodeTableLayout,
    reserved_blocks: BTreeSet<u64>,
    /// Non-authoritative next-fit hints. Allocation bitmaps remain the source
    /// of truth; advancing either hint for a plan which is later abandoned is
    /// therefore harmless and only changes the next scan's starting point.
    next_block_hint: u64,
    next_inode_hint: u64,
    journal_start: Option<u64>,
    next_sequence: u32,
}

struct PlannedGroupAllocation {
    group_index: usize,
    bitmap_home: u64,
    bitmap_before: Page4K,
    bitmap_after: Page4K,
    count: u32,
}

struct PlannedBlockAllocations {
    claims: Vec<u64>,
    groups: Vec<PlannedGroupAllocation>,
}

struct IndexedUninitializedConversion {
    physical_block: u64,
    extent_metadata: Vec<MetadataBlock>,
    allocation: Option<PlannedBlockAllocations>,
    inode_after: Option<Inode>,
}

impl<I: BlockImage> Ext4Pager<I> {
    pub fn open(image: I) -> Result<Self> {
        let mut pager = Self {
            image,
            pending_metadata: BTreeMap::new(),
            superblock: Superblock::default(),
            groups: Vec::new(),
            inode_table: InodeTableLayout {
                inode_size: 256,
                inodes_per_group: 0,
                first_inode_table_block: 0,
            },
            reserved_blocks: BTreeSet::new(),
            next_block_hint: 0,
            next_inode_hint: 0,
            journal_start: None,
            next_sequence: 1,
        };
        let mut block = [0u8; BLOCK_SIZE];
        pager.read_block(0, &mut block)?;
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
        pager.reserve_filesystem_metadata()?;
        if superblock.journal_inode != 0 {
            if let Ok(journal_inode) = pager.read_inode(InodeNo::new(superblock.journal_inode)) {
                pager.journal_start = match pager.resolve_inode_block(&journal_inode, 0) {
                    Ok(BlockMapping::Data(block)) => Some(block),
                    _ => None,
                };
                if let (Ok(data_blocks), Ok(node_homes)) = (
                    pager.collect_extent_data_blocks(journal_inode.extent_root_bytes()),
                    pager.collect_extent_node_homes(journal_inode.extent_root_bytes()),
                ) {
                    pager.reserved_blocks.extend(data_blocks);
                    pager.reserved_blocks.extend(node_homes);
                }
            }
        }
        Ok(pager)
    }

    pub fn image(&self) -> &I {
        &self.image
    }

    pub const fn superblock(&self) -> Superblock {
        self.superblock
    }

    /// Complete bounded classic-orphan cleanup after journal replay.
    ///
    /// This mirrors the supported destroy lifecycle for zero-link regular files,
    /// empty directories, and fast symlinks, but applies it during mount recovery
    /// before the filesystem is exposed.
    pub fn recover_classic_orphan_chain(&mut self) -> Result<u32> {
        let mut recovered = 0u32;
        loop {
            let head = self.current_last_orphan()?;
            if head == 0 {
                return Ok(recovered);
            }
            if recovered >= self.superblock.inodes_count {
                return Err(Ext4FormatError::Corrupt);
            }
            let plan = self.plan_destroy_inode(InodeNo::new(head), FsyncStamp::new(1))?;
            for block in &plan.metadata {
                self.image.write_block(block.home, &block.after)?;
                self.image.invalidate_block(block.home);
            }
            self.image.barrier()?;
            self.refresh_layout_after_recovery_write()?;
            recovered = recovered
                .checked_add(1)
                .ok_or(Ext4FormatError::OutOfBounds)?;
        }
    }

    fn current_last_orphan(&self) -> Result<u32> {
        let mut page = [0; BLOCK_SIZE];
        self.read_block(0, &mut page)?;
        Ok(Superblock::parse(&page[1024..2048])?.last_orphan)
    }

    fn refresh_layout_after_recovery_write(&mut self) -> Result<()> {
        let mut page = [0; BLOCK_SIZE];
        self.read_block(0, &mut page)?;
        self.superblock = Superblock::parse(&page[1024..2048])?;
        self.groups = read_group_descs(&self.image, &self.superblock)?;
        Ok(())
    }

    /// Persist ext4's recovery-required state before admitting a journalled
    /// read-write mount. A later clean detach is the only path allowed to
    /// clear it.
    pub fn mark_recovery_required(&mut self) -> Result<()> {
        let mut page = [0; BLOCK_SIZE];
        self.read_block(0, &mut page)?;
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

    /// Count free blocks and inodes from the allocation bitmaps.
    ///
    /// The superblock counters can lag a journalled mutation.  Reading
    /// through `self.read_block` also observes accepted metadata after-images
    /// that have not reached their home blocks yet, so `statfs(2)` reports the
    /// same allocation state as the rest of this mounted pager.
    pub fn filesystem_stats(&self) -> Result<FilesystemStatsLite> {
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
            self.read_block(group.block_bitmap_block(), &mut bitmap)?;
            free_blocks = free_blocks
                .checked_add(count_clear_bits(&bitmap, valid_blocks as usize)?)
                .ok_or(Ext4FormatError::OutOfBounds)?;

            let group_inode_start = group_index as u64 * inodes_per_group;
            let valid_inodes = core::cmp::min(
                inodes_per_group,
                total_inodes.saturating_sub(group_inode_start),
            );
            self.read_block(group.inode_bitmap_block(), &mut bitmap)?;
            free_inodes = free_inodes
                .checked_add(count_clear_bits(&bitmap, valid_inodes as usize)?)
                .ok_or(Ext4FormatError::OutOfBounds)?;
        }

        Ok(FilesystemStatsLite {
            block_size: self.superblock.block_size() as u64,
            total_blocks,
            free_blocks,
            available_blocks: free_blocks,
            total_inodes,
            free_inodes,
            max_name_len: 255,
        })
    }

    fn read_block(&self, block: u64, out: &mut Page4K) -> Result<()> {
        if let Some(pending) = self.pending_metadata.get(&block) {
            out.copy_from_slice(pending);
            return Ok(());
        }
        self.image.read_block(block, out)
    }

    /// Publish metadata after-images that have been accepted by the mounted
    /// mutation runtime but not yet checkpointed to their home blocks.
    pub fn stage_mutation_after_images(&mut self, mutation: &Ext4MutationPlan) {
        // Ordered data may target a block which still has an old incarnation
        // in the image adapter's read cache.  Invalidate exactly those homes
        // at admission; the later write repopulates them with current bytes.
        for data in &mutation.data {
            self.image.invalidate_block(data.physical_block);
        }
        for metadata in &mutation.metadata {
            self.pending_metadata.insert(metadata.home, metadata.after);
            self.image.invalidate_block(metadata.home);
        }
    }

    /// Apply an immutable mutation directly to its home blocks.
    ///
    /// This is the compatibility executor for mounts which deliberately do
    /// not install the asynchronous JBD2 runtime.  The mount-wide mutation
    /// admission permit keeps planning and application serialized, so the
    /// plan cannot become stale between those two steps.  `BlockImage` writes
    /// complete before returning; writing ordered data first therefore keeps
    /// metadata from exposing an uninitialised data block without paying for
    /// descriptor, commit, and checkpoint I/O on every short-lived file.
    ///
    /// No barrier is issued here.  This path has the same graceful-shutdown
    /// (rather than power-loss journal) contract as `mount_ext4_read_write`.
    pub fn apply_mutation_direct(&mut self, mutation: &Ext4MutationPlan) -> Result<()> {
        let mut first = 0usize;
        while first < mutation.data.len() {
            let mut end = first + 1;
            while end < mutation.data.len()
                && mutation.data[end].physical_block
                    == mutation.data[end - 1].physical_block.saturating_add(1)
            {
                end += 1;
            }
            let pages: Vec<&Page4K> = mutation.data[first..end]
                .iter()
                .map(|write| &write.bytes)
                .collect();
            self.image
                .write_blocks(mutation.data[first].physical_block, &pages)?;
            first = end;
        }
        for metadata in &mutation.metadata {
            self.image.write_block(metadata.home, &metadata.after)?;
        }
        Ok(())
    }

    pub fn apply_l6_write_page(
        &mut self,
        start_lba: u64,
        sectors_per_block: u64,
        page: &Page4K,
    ) -> Result<()> {
        if sectors_per_block == 0 || start_lba % sectors_per_block != 0 {
            return Err(Ext4FormatError::Corrupt);
        }
        let block = start_lba / sectors_per_block;
        self.image.write_block(block, page)?;
        Ok(())
    }

    pub fn apply_l6_barrier(&mut self) -> Result<()> {
        self.image.barrier()
    }

    /// Retire the after-image overlay after an L6-owned checkpoint.
    ///
    /// Admission invalidates every data and metadata home touched by the
    /// transaction, and `write_block` makes the checkpointed bytes visible to
    /// subsequent reads.  Clearing the entire image cache here would evict
    /// unrelated executable, library, and source blocks after every namespace
    /// operation.
    pub fn settle_image_cache(&mut self) {
        self.pending_metadata.clear();
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
        self.read_block(blocks[0], &mut superblock_page)?;
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
                self.image.read_data_block(block, out)?;
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
        let inline_uninitialized =
            convert_inline_uninitialized_extent(disk_inode.extent_root_bytes(), logical)?;
        let indexed_uninitialized = if inline_uninitialized.is_none() {
            let depth_one = self.plan_depth_one_uninitialized_conversion(&disk_inode, logical)?;
            if depth_one.is_some() {
                depth_one
            } else {
                let depth_two =
                    self.plan_depth_two_uninitialized_conversion(&disk_inode, logical)?;
                if depth_two.is_some() {
                    depth_two
                } else {
                    self.plan_deep_uninitialized_conversion(&disk_inode, logical)?
                }
            }
        } else {
            None
        };
        // The pre-journal direct writer already grew holes below a depth-one
        // extent root.  Preserve that capability in the immutable mutation
        // planner: otherwise the fifth fragmented extent spills the inode root
        // to depth one, and the very next extending buffered write regresses to
        // `Unsupported` even though the selected leaf still has ample room.
        let indexed_update = if indexed_uninitialized.is_none() && inline_uninitialized.is_none() {
            let depth_one = self.plan_depth_one_hole_allocation(&disk_inode, logical)?;
            if depth_one.is_some() {
                depth_one
            } else {
                self.plan_depth_two_hole_allocation(&disk_inode, logical)?
            }
        } else {
            indexed_uninitialized
        };
        let block = if let Some((physical_block, extents)) = inline_uninitialized {
            let mut inode_after = disk_inode;
            inode_after.set_extent_root(&extents)?;
            let page_end = file_page_index
                .checked_add(1)
                .and_then(|page| page.checked_mul(BLOCK_SIZE as u64))
                .ok_or(Ext4FormatError::OutOfBounds)?;
            inode_after.size = core::cmp::max(inode_after.size, page_end);

            let loc = self.inode_location(inode)?;
            let mut inode_table_before = [0u8; BLOCK_SIZE];
            self.read_block(loc.block, &mut inode_table_before)?;
            let mut inode_table_after = inode_table_before;
            let inode_bytes = &mut inode_table_after[loc.offset..loc.offset + loc.len];
            inode_after.encode(inode_bytes)?;
            self.refresh_inode_checksum(inode, &inode_after, inode_bytes)?;
            plan.push_metadata(MetadataBlock {
                home: loc.block,
                role: MetaRole::InodeTable,
                before_version: crc32c(0, &inode_table_before) as u64,
                after: inode_table_after,
                depends_on: Vec::new(),
            })
            .map_err(|_| Ext4FormatError::Corrupt)?;
            physical_block
        } else if let Some(indexed_uninitialized) = indexed_update {
            let IndexedUninitializedConversion {
                physical_block,
                extent_metadata,
                allocation,
                mut inode_after,
            } = indexed_uninitialized;
            for metadata in extent_metadata {
                plan.push_metadata(metadata)
                    .map_err(|_| Ext4FormatError::Corrupt)?;
            }
            if let Some(allocation) = allocation {
                let allocated_count = u32::try_from(allocation.claims.len())
                    .map_err(|_| Ext4FormatError::OutOfBounds)?;
                let group_desc_updates =
                    self.plan_group_free_block_decrements(&allocation.groups)?;
                let (superblock_home, superblock_before, superblock_after) =
                    self.plan_superblock_free_block_decrement(allocated_count)?;
                for group in &allocation.groups {
                    plan.push_metadata(MetadataBlock {
                        home: group.bitmap_home,
                        role: MetaRole::BlockBitmap,
                        before_version: crc32c(0, &group.bitmap_before) as u64,
                        after: group.bitmap_after,
                        depends_on: Vec::new(),
                    })
                    .map_err(|_| Ext4FormatError::Corrupt)?;
                }
                for (home, before, after) in group_desc_updates {
                    plan.push_metadata(MetadataBlock {
                        home,
                        role: MetaRole::GroupDescriptor,
                        before_version: crc32c(0, &before) as u64,
                        after,
                        depends_on: Vec::new(),
                    })
                    .map_err(|_| Ext4FormatError::Corrupt)?;
                }
                plan.push_metadata(MetadataBlock {
                    home: superblock_home,
                    role: MetaRole::Superblock,
                    before_version: crc32c(0, &superblock_before) as u64,
                    after: superblock_after,
                    depends_on: Vec::new(),
                })
                .map_err(|_| Ext4FormatError::Corrupt)?;
                plan.allocations.extend(
                    allocation
                        .claims
                        .into_iter()
                        .map(|physical_block| BlockClaim { physical_block }),
                );
            }
            let page_end = file_page_index
                .checked_add(1)
                .and_then(|page| page.checked_mul(BLOCK_SIZE as u64))
                .ok_or(Ext4FormatError::OutOfBounds)?;
            if inode_after.is_some() || page_end > disk_inode.size {
                let mut inode_after = inode_after.take().unwrap_or(disk_inode);
                inode_after.size = core::cmp::max(inode_after.size, page_end);
                let loc = self.inode_location(inode)?;
                let mut inode_table_before = [0u8; BLOCK_SIZE];
                self.read_block(loc.block, &mut inode_table_before)?;
                let mut inode_table_after = inode_table_before;
                let inode_bytes = &mut inode_table_after[loc.offset..loc.offset + loc.len];
                inode_after.encode(inode_bytes)?;
                self.refresh_inode_checksum(inode, &inode_after, inode_bytes)?;
                plan.push_metadata(MetadataBlock {
                    home: loc.block,
                    role: MetaRole::InodeTable,
                    before_version: crc32c(0, &inode_table_before) as u64,
                    after: inode_table_after,
                    depends_on: Vec::new(),
                })
                .map_err(|_| Ext4FormatError::Corrupt)?;
            }
            physical_block
        } else {
            match self.resolve_inode_block(&disk_inode, logical)? {
                BlockMapping::Data(block) => block,
                BlockMapping::Hole => {
                    let goal = self.extent_allocation_goal(&disk_inode, logical)?;
                    let mut allocation = self.plan_block_allocations_near(1, goal)?;
                    let physical_block = allocation.claims[0];
                    let mut inode_after = disk_inode;
                    let mut extents = match ExtentNode::parse(inode_after.extent_root_bytes())? {
                        ExtentNode::Leaf(extents) => extents,
                        ExtentNode::Index(_) => return Err(Ext4FormatError::Unsupported),
                    };
                    coalesce_insert_extent(&mut extents, logical, physical_block);
                    let mut extent_metadata = Vec::new();
                    if extents.len() > 4 {
                        let replanned =
                            self.plan_block_allocations_near(2, Some(physical_block))?;
                        if replanned.claims[0] != physical_block {
                            return Err(Ext4FormatError::Corrupt);
                        }
                        allocation = replanned;
                        let child = allocation.claims[1];
                        let mut child_before = [0u8; BLOCK_SIZE];
                        self.read_block(child, &mut child_before)?;
                        let mut child_after = child_before;
                        ExtentNode::encode_leaf(&extents, &mut child_after)?;
                        inode_after.set_extent_index_root(
                            &[ExtentIdx {
                                logical_block: extents[0].logical_block,
                                child,
                            }],
                            1,
                        )?;
                        extent_metadata.push(MetadataBlock {
                            home: child,
                            role: MetaRole::ExtentNode,
                            before_version: crc32c(0, &child_before) as u64,
                            after: child_after,
                            depends_on: Vec::new(),
                        });
                    } else {
                        inode_after.set_extent_root(&extents)?;
                    }
                    let group_desc_updates =
                        self.plan_group_free_block_decrements(&allocation.groups)?;
                    let (superblock_home, superblock_before, superblock_after) = self
                        .plan_superblock_free_block_decrement(
                            u32::try_from(allocation.claims.len())
                                .map_err(|_| Ext4FormatError::OutOfBounds)?,
                        )?;
                    inode_after.blocks_512 = inode_after
                        .blocks_512
                        .saturating_add(allocation.claims.len() as u64 * (BLOCK_SIZE / 512) as u64);
                    let page_end = file_page_index
                        .checked_add(1)
                        .and_then(|page| page.checked_mul(BLOCK_SIZE as u64))
                        .ok_or(Ext4FormatError::OutOfBounds)?;
                    inode_after.size = core::cmp::max(inode_after.size, page_end);

                    let loc = self.inode_location(inode)?;
                    let mut inode_table_before = [0u8; BLOCK_SIZE];
                    self.read_block(loc.block, &mut inode_table_before)?;
                    let mut inode_table_after = inode_table_before;
                    let inode_bytes = &mut inode_table_after[loc.offset..loc.offset + loc.len];
                    inode_after.encode(inode_bytes)?;
                    self.refresh_inode_checksum(inode, &inode_after, inode_bytes)?;

                    for group in &allocation.groups {
                        plan.push_metadata(MetadataBlock {
                            home: group.bitmap_home,
                            role: MetaRole::BlockBitmap,
                            before_version: crc32c(0, &group.bitmap_before) as u64,
                            after: group.bitmap_after,
                            depends_on: Vec::new(),
                        })
                        .map_err(|_| Ext4FormatError::Corrupt)?;
                    }
                    for (home, before, after) in group_desc_updates {
                        plan.push_metadata(MetadataBlock {
                            home,
                            role: MetaRole::GroupDescriptor,
                            before_version: crc32c(0, &before) as u64,
                            after,
                            depends_on: Vec::new(),
                        })
                        .map_err(|_| Ext4FormatError::Corrupt)?;
                    }
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
                    for metadata in extent_metadata {
                        plan.push_metadata(metadata)
                            .map_err(|_| Ext4FormatError::Corrupt)?;
                    }
                    plan.allocations.extend(
                        allocation
                            .claims
                            .into_iter()
                            .map(|physical_block| BlockClaim { physical_block }),
                    );
                    physical_block
                }
                BlockMapping::NeedNode(_) => return Err(Ext4FormatError::Unsupported),
            }
        };
        if plan.metadata.is_empty() {
            // A mapped data write still needs a committed metadata anchor so
            // fsync can distinguish ordered-data durability from background
            // writeback. The inode after-image is intentionally unchanged.
            let loc = self.inode_location(inode)?;
            let mut inode_table_before = [0u8; BLOCK_SIZE];
            self.read_block(loc.block, &mut inode_table_before)?;
            plan.push_metadata(MetadataBlock {
                home: loc.block,
                role: MetaRole::InodeTable,
                before_version: crc32c(0, &inode_table_before) as u64,
                after: inode_table_before,
                depends_on: Vec::new(),
            })
            .map_err(|_| Ext4FormatError::Corrupt)?;
        }
        self.refresh_extent_node_checksums(inode, &disk_inode, &mut plan.metadata);
        plan.data.push(SealedDataWrite {
            logical_page: file_page_index,
            physical_block: block,
            bytes: *page,
        });
        Ok(plan)
    }

    /// Allocate one hole below a depth-one extent leaf and describe the
    /// complete change as an immutable journal mutation.
    ///
    /// This is the transactional counterpart of the bounded depth-one support
    /// in `attach_data_block`: it updates the selected leaf, the inode root key
    /// when necessary, and the inode block count, while leaving bitmap and
    /// free-count after-images to the common `plan_write_page` path.
    fn plan_depth_one_hole_allocation(
        &mut self,
        inode: &Inode,
        logical_block: u32,
    ) -> Result<Option<IndexedUninitializedConversion>> {
        let root_header = ExtentHeader::parse(inode.extent_root_bytes())?;
        if root_header.depth != 1 {
            return Ok(None);
        }
        if !matches!(
            self.resolve_inode_block(inode, logical_block)?,
            BlockMapping::Hole
        ) {
            return Ok(None);
        }

        let mut indexes = match ExtentNode::parse(inode.extent_root_bytes())? {
            ExtentNode::Index(indexes) if !indexes.is_empty() => indexes,
            _ => return Err(Ext4FormatError::Corrupt),
        };
        let selected = indexes
            .iter()
            .rposition(|index| index.logical_block <= logical_block)
            .unwrap_or(0);
        let home = indexes[selected].child;
        let mut before = [0u8; BLOCK_SIZE];
        self.read_block(home, &mut before)?;
        let header = ExtentHeader::parse(&before)?;
        if header.depth != 0 {
            return Err(Ext4FormatError::Corrupt);
        }
        let mut extents = match ExtentNode::parse(&before)? {
            ExtentNode::Leaf(extents) => extents,
            ExtentNode::Index(_) => return Err(Ext4FormatError::Corrupt),
        };

        let goal = self.extent_allocation_goal(inode, logical_block)?;
        let mut allocation = self.plan_block_allocations_near(1, goal)?;
        let physical_block = allocation.claims[0];
        coalesce_insert_extent(&mut extents, logical_block, physical_block);
        if extents.len() > usize::from(header.max) {
            let root_growth = indexes.len() >= usize::from(root_header.max);
            let replanned = self.plan_block_allocations_near(
                if root_growth { 4 } else { 2 },
                Some(physical_block),
            )?;
            if replanned.claims[0] != physical_block {
                return Err(Ext4FormatError::Corrupt);
            }
            allocation = replanned;

            let right_home = allocation.claims[1];
            let right_extents = extents.split_off(extents.len() / 2);
            let mut leaf_after = before;
            ExtentNode::encode_leaf(&extents, &mut leaf_after)?;
            let mut right_before = [0u8; BLOCK_SIZE];
            self.read_block(right_home, &mut right_before)?;
            let mut right_after = right_before;
            ExtentNode::encode_leaf(&right_extents, &mut right_after)?;

            indexes[selected].logical_block = extents[0].logical_block;
            indexes.insert(
                selected + 1,
                ExtentIdx {
                    logical_block: right_extents[0].logical_block,
                    child: right_home,
                },
            );
            let mut inode_after = *inode;
            let mut extent_metadata = vec![
                MetadataBlock {
                    home,
                    role: MetaRole::ExtentNode,
                    before_version: crc32c(0, &before) as u64,
                    after: leaf_after,
                    depends_on: Vec::new(),
                },
                MetadataBlock {
                    home: right_home,
                    role: MetaRole::ExtentNode,
                    before_version: crc32c(0, &right_before) as u64,
                    after: right_after,
                    depends_on: Vec::new(),
                },
            ];
            if root_growth {
                let left_root_home = allocation.claims[2];
                let right_root_home = allocation.claims[3];
                let right_root_indexes = indexes.split_off(indexes.len() / 2);
                let mut left_root_before = [0u8; BLOCK_SIZE];
                self.read_block(left_root_home, &mut left_root_before)?;
                let mut right_root_before = [0u8; BLOCK_SIZE];
                self.read_block(right_root_home, &mut right_root_before)?;
                let mut left_root_after = left_root_before;
                let mut right_root_after = right_root_before;
                ExtentNode::encode_index(1, &indexes, &mut left_root_after)?;
                ExtentNode::encode_index(1, &right_root_indexes, &mut right_root_after)?;
                inode_after.set_extent_index_root(
                    &[
                        ExtentIdx {
                            logical_block: indexes[0].logical_block,
                            child: left_root_home,
                        },
                        ExtentIdx {
                            logical_block: right_root_indexes[0].logical_block,
                            child: right_root_home,
                        },
                    ],
                    root_header
                        .depth
                        .checked_add(1)
                        .ok_or(Ext4FormatError::OutOfBounds)?,
                )?;
                extent_metadata.push(MetadataBlock {
                    home: left_root_home,
                    role: MetaRole::ExtentNode,
                    before_version: crc32c(0, &left_root_before) as u64,
                    after: left_root_after,
                    depends_on: Vec::new(),
                });
                extent_metadata.push(MetadataBlock {
                    home: right_root_home,
                    role: MetaRole::ExtentNode,
                    before_version: crc32c(0, &right_root_before) as u64,
                    after: right_root_after,
                    depends_on: Vec::new(),
                });
            } else {
                inode_after.set_extent_index_root(&indexes, root_header.depth)?;
            }
            inode_after.blocks_512 = inode_after
                .blocks_512
                .checked_add(
                    u64::try_from(allocation.claims.len())
                        .map_err(|_| Ext4FormatError::OutOfBounds)?
                        * (BLOCK_SIZE / 512) as u64,
                )
                .ok_or(Ext4FormatError::OutOfBounds)?;
            return Ok(Some(IndexedUninitializedConversion {
                physical_block,
                extent_metadata,
                allocation: Some(allocation),
                inode_after: Some(inode_after),
            }));
        }
        let mut after = before;
        ExtentNode::encode_leaf(&extents, &mut after)?;

        let mut inode_after = *inode;
        let new_low = extents[0].logical_block;
        if indexes[selected].logical_block != new_low {
            indexes[selected].logical_block = new_low;
            inode_after.set_extent_index_root(&indexes, root_header.depth)?;
        }
        inode_after.blocks_512 = inode_after
            .blocks_512
            .checked_add(
                u64::try_from(allocation.claims.len()).map_err(|_| Ext4FormatError::OutOfBounds)?
                    * (BLOCK_SIZE / 512) as u64,
            )
            .ok_or(Ext4FormatError::OutOfBounds)?;

        Ok(Some(IndexedUninitializedConversion {
            physical_block,
            extent_metadata: vec![MetadataBlock {
                home,
                role: MetaRole::ExtentNode,
                before_version: crc32c(0, &before) as u64,
                after,
                depends_on: Vec::new(),
            }],
            allocation: Some(allocation),
            inode_after: Some(inode_after),
        }))
    }

    /// Convert one unwritten block from a depth-one extent tree without
    /// allocating data blocks. A full leaf splits in place; when the inline
    /// root is full, its index entries are promoted into two new depth-one
    /// nodes and the inode root grows to depth two.
    fn plan_depth_one_uninitialized_conversion(
        &mut self,
        inode: &Inode,
        logical_block: u32,
    ) -> Result<Option<IndexedUninitializedConversion>> {
        let root_header = ExtentHeader::parse(inode.extent_root_bytes())?;
        if root_header.depth == 0 {
            return Ok(None);
        }
        if root_header.depth != 1 {
            return Ok(None);
        }
        let indexes = match ExtentNode::parse(inode.extent_root_bytes())? {
            ExtentNode::Index(indexes) if !indexes.is_empty() => indexes,
            _ => return Err(Ext4FormatError::Corrupt),
        };
        let selected = indexes
            .iter()
            .rposition(|index| index.logical_block <= logical_block)
            .unwrap_or(0);
        let home = indexes[selected].child;
        let mut before = [0u8; BLOCK_SIZE];
        self.read_block(home, &mut before)?;
        let header = ExtentHeader::parse(&before)?;
        if header.depth != 0 {
            return Err(Ext4FormatError::Corrupt);
        }
        let mut extents = match ExtentNode::parse(&before)? {
            ExtentNode::Leaf(extents) => extents,
            ExtentNode::Index(_) => return Err(Ext4FormatError::Corrupt),
        };
        let Some(physical_block) = convert_uninitialized_extent_list(&mut extents, logical_block)?
        else {
            return Ok(None);
        };
        if extents.len() > usize::from(header.max) {
            let root_growth = indexes.len() >= usize::from(root_header.max);
            let allocation = self.plan_block_allocations(if root_growth { 3 } else { 1 })?;
            let right_home = allocation.claims[0];
            let right_extents = extents.split_off(extents.len() / 2);
            let mut leaf_after = before;
            ExtentNode::encode_leaf(&extents, &mut leaf_after)?;
            let mut right_before = [0u8; BLOCK_SIZE];
            self.read_block(right_home, &mut right_before)?;
            let mut right_after = right_before;
            ExtentNode::encode_leaf(&right_extents, &mut right_after)?;
            let mut root_indexes = indexes;
            root_indexes[selected].logical_block = extents[0].logical_block;
            root_indexes.insert(
                selected + 1,
                ExtentIdx {
                    logical_block: right_extents[0].logical_block,
                    child: right_home,
                },
            );
            let mut inode_after = *inode;
            let mut extent_metadata = vec![
                MetadataBlock {
                    home,
                    role: MetaRole::ExtentNode,
                    before_version: crc32c(0, &before) as u64,
                    after: leaf_after,
                    depends_on: Vec::new(),
                },
                MetadataBlock {
                    home: right_home,
                    role: MetaRole::ExtentNode,
                    before_version: crc32c(0, &right_before) as u64,
                    after: right_after,
                    depends_on: Vec::new(),
                },
            ];
            if root_growth {
                let left_root_home = allocation.claims[1];
                let right_root_home = allocation.claims[2];
                let split = root_indexes.len() / 2;
                let right_root_indexes = root_indexes.split_off(split);
                let mut left_root_before = [0u8; BLOCK_SIZE];
                self.read_block(left_root_home, &mut left_root_before)?;
                let mut right_root_before = [0u8; BLOCK_SIZE];
                self.read_block(right_root_home, &mut right_root_before)?;
                let mut left_root_after = left_root_before;
                let mut right_root_after = right_root_before;
                ExtentNode::encode_index(1, &root_indexes, &mut left_root_after)?;
                ExtentNode::encode_index(1, &right_root_indexes, &mut right_root_after)?;
                inode_after.set_extent_index_root(
                    &[
                        ExtentIdx {
                            logical_block: root_indexes[0].logical_block,
                            child: left_root_home,
                        },
                        ExtentIdx {
                            logical_block: right_root_indexes[0].logical_block,
                            child: right_root_home,
                        },
                    ],
                    root_header
                        .depth
                        .checked_add(1)
                        .ok_or(Ext4FormatError::OutOfBounds)?,
                )?;
                extent_metadata.push(MetadataBlock {
                    home: left_root_home,
                    role: MetaRole::ExtentNode,
                    before_version: crc32c(0, &left_root_before) as u64,
                    after: left_root_after,
                    depends_on: Vec::new(),
                });
                extent_metadata.push(MetadataBlock {
                    home: right_root_home,
                    role: MetaRole::ExtentNode,
                    before_version: crc32c(0, &right_root_before) as u64,
                    after: right_root_after,
                    depends_on: Vec::new(),
                });
            } else {
                inode_after.set_extent_index_root(&root_indexes, root_header.depth)?;
            }
            inode_after.blocks_512 = inode_after
                .blocks_512
                .checked_add(
                    u64::try_from(allocation.claims.len())
                        .map_err(|_| Ext4FormatError::OutOfBounds)?
                        * (BLOCK_SIZE / 512) as u64,
                )
                .ok_or(Ext4FormatError::OutOfBounds)?;
            return Ok(Some(IndexedUninitializedConversion {
                physical_block,
                extent_metadata,
                allocation: Some(allocation),
                inode_after: Some(inode_after),
            }));
        }
        let mut after = before;
        ExtentNode::encode_leaf(&extents, &mut after)?;
        Ok(Some(IndexedUninitializedConversion {
            physical_block,
            extent_metadata: vec![MetadataBlock {
                home,
                role: MetaRole::ExtentNode,
                before_version: crc32c(0, &before) as u64,
                after,
                depends_on: Vec::new(),
            }],
            allocation: None,
            inode_after: None,
        }))
    }

    /// Allocate one hole through a depth-two root -> parent -> leaf path.
    /// Leaf and parent splits are carried upward in the same immutable plan;
    /// a full inline root grows to depth three.
    fn plan_depth_two_hole_allocation(
        &mut self,
        inode: &Inode,
        logical_block: u32,
    ) -> Result<Option<IndexedUninitializedConversion>> {
        let root_header = ExtentHeader::parse(inode.extent_root_bytes())?;
        if root_header.depth != 2 {
            return Ok(None);
        }
        if !matches!(
            self.resolve_inode_block(inode, logical_block)?,
            BlockMapping::Hole
        ) {
            return Ok(None);
        }
        let mut root_indexes = match ExtentNode::parse(inode.extent_root_bytes())? {
            ExtentNode::Index(indexes) if !indexes.is_empty() => indexes,
            _ => return Err(Ext4FormatError::Corrupt),
        };
        let root_pos = root_indexes
            .iter()
            .rposition(|index| index.logical_block <= logical_block)
            .unwrap_or(0);
        let parent_home = root_indexes[root_pos].child;
        let mut parent_before = [0u8; BLOCK_SIZE];
        self.read_block(parent_home, &mut parent_before)?;
        let parent_header = ExtentHeader::parse(&parent_before)?;
        if parent_header.depth != 1 {
            return Err(Ext4FormatError::Corrupt);
        }
        let mut parent_indexes = match ExtentNode::parse(&parent_before)? {
            ExtentNode::Index(indexes) if !indexes.is_empty() => indexes,
            _ => return Err(Ext4FormatError::Corrupt),
        };
        let parent_pos = parent_indexes
            .iter()
            .rposition(|index| index.logical_block <= logical_block)
            .unwrap_or(0);
        let home = parent_indexes[parent_pos].child;
        let mut before = [0u8; BLOCK_SIZE];
        self.read_block(home, &mut before)?;
        let header = ExtentHeader::parse(&before)?;
        if header.depth != 0 {
            return Err(Ext4FormatError::Corrupt);
        }
        let mut extents = match ExtentNode::parse(&before)? {
            ExtentNode::Leaf(extents) => extents,
            ExtentNode::Index(_) => return Err(Ext4FormatError::Corrupt),
        };

        let goal = self.extent_allocation_goal(inode, logical_block)?;
        let mut allocation = self.plan_block_allocations_near(1, goal)?;
        let physical_block = allocation.claims[0];
        coalesce_insert_extent(&mut extents, logical_block, physical_block);
        let mut inode_after = *inode;
        if extents.len() <= usize::from(header.max) {
            let mut after = before;
            ExtentNode::encode_leaf(&extents, &mut after)?;
            let mut extent_metadata = vec![MetadataBlock {
                home,
                role: MetaRole::ExtentNode,
                before_version: crc32c(0, &before) as u64,
                after,
                depends_on: Vec::new(),
            }];
            let new_leaf_low = extents[0].logical_block;
            if parent_indexes[parent_pos].logical_block != new_leaf_low {
                parent_indexes[parent_pos].logical_block = new_leaf_low;
                let mut parent_after = parent_before;
                ExtentNode::encode_index(parent_header.depth, &parent_indexes, &mut parent_after)?;
                extent_metadata.push(MetadataBlock {
                    home: parent_home,
                    role: MetaRole::ExtentNode,
                    before_version: crc32c(0, &parent_before) as u64,
                    after: parent_after,
                    depends_on: Vec::new(),
                });
                if parent_pos == 0
                    && root_indexes[root_pos].logical_block != parent_indexes[0].logical_block
                {
                    root_indexes[root_pos].logical_block = parent_indexes[0].logical_block;
                    inode_after.set_extent_index_root(&root_indexes, root_header.depth)?;
                }
            }
            inode_after.blocks_512 = inode_after
                .blocks_512
                .checked_add((BLOCK_SIZE / 512) as u64)
                .ok_or(Ext4FormatError::OutOfBounds)?;
            return Ok(Some(IndexedUninitializedConversion {
                physical_block,
                extent_metadata,
                allocation: Some(allocation),
                inode_after: Some(inode_after),
            }));
        }

        let parent_split = parent_indexes.len() >= usize::from(parent_header.max);
        let root_growth = parent_split && root_indexes.len() >= usize::from(root_header.max);
        let replanned = self.plan_block_allocations_near(
            match (parent_split, root_growth) {
                (false, false) => 2,
                (true, false) => 3,
                (true, true) => 5,
                (false, true) => return Err(Ext4FormatError::Corrupt),
            },
            Some(physical_block),
        )?;
        if replanned.claims[0] != physical_block {
            return Err(Ext4FormatError::Corrupt);
        }
        allocation = replanned;

        let right_home = allocation.claims[1];
        let right_extents = extents.split_off(extents.len() / 2);
        let mut leaf_after = before;
        ExtentNode::encode_leaf(&extents, &mut leaf_after)?;
        let mut right_before = [0u8; BLOCK_SIZE];
        self.read_block(right_home, &mut right_before)?;
        let mut right_after = right_before;
        ExtentNode::encode_leaf(&right_extents, &mut right_after)?;
        parent_indexes[parent_pos].logical_block = extents[0].logical_block;
        parent_indexes.insert(
            parent_pos + 1,
            ExtentIdx {
                logical_block: right_extents[0].logical_block,
                child: right_home,
            },
        );
        let mut parent_after = parent_before;
        let mut extent_metadata = vec![
            MetadataBlock {
                home,
                role: MetaRole::ExtentNode,
                before_version: crc32c(0, &before) as u64,
                after: leaf_after,
                depends_on: Vec::new(),
            },
            MetadataBlock {
                home: right_home,
                role: MetaRole::ExtentNode,
                before_version: crc32c(0, &right_before) as u64,
                after: right_after,
                depends_on: Vec::new(),
            },
        ];
        if parent_split {
            let right_parent_home = allocation.claims[2];
            let right_parent_indexes = parent_indexes.split_off(parent_indexes.len() / 2);
            let mut right_parent_before = [0u8; BLOCK_SIZE];
            self.read_block(right_parent_home, &mut right_parent_before)?;
            let mut right_parent_after = right_parent_before;
            ExtentNode::encode_index(parent_header.depth, &parent_indexes, &mut parent_after)?;
            ExtentNode::encode_index(
                parent_header.depth,
                &right_parent_indexes,
                &mut right_parent_after,
            )?;
            root_indexes[root_pos].logical_block = parent_indexes[0].logical_block;
            root_indexes.insert(
                root_pos + 1,
                ExtentIdx {
                    logical_block: right_parent_indexes[0].logical_block,
                    child: right_parent_home,
                },
            );
            extent_metadata.push(MetadataBlock {
                home: parent_home,
                role: MetaRole::ExtentNode,
                before_version: crc32c(0, &parent_before) as u64,
                after: parent_after,
                depends_on: Vec::new(),
            });
            extent_metadata.push(MetadataBlock {
                home: right_parent_home,
                role: MetaRole::ExtentNode,
                before_version: crc32c(0, &right_parent_before) as u64,
                after: right_parent_after,
                depends_on: Vec::new(),
            });
            if root_growth {
                let left_root_home = allocation.claims[3];
                let right_root_home = allocation.claims[4];
                let right_root_indexes = root_indexes.split_off(root_indexes.len() / 2);
                let mut left_root_before = [0u8; BLOCK_SIZE];
                self.read_block(left_root_home, &mut left_root_before)?;
                let mut right_root_before = [0u8; BLOCK_SIZE];
                self.read_block(right_root_home, &mut right_root_before)?;
                let mut left_root_after = left_root_before;
                let mut right_root_after = right_root_before;
                ExtentNode::encode_index(root_header.depth, &root_indexes, &mut left_root_after)?;
                ExtentNode::encode_index(
                    root_header.depth,
                    &right_root_indexes,
                    &mut right_root_after,
                )?;
                inode_after.set_extent_index_root(
                    &[
                        ExtentIdx {
                            logical_block: root_indexes[0].logical_block,
                            child: left_root_home,
                        },
                        ExtentIdx {
                            logical_block: right_root_indexes[0].logical_block,
                            child: right_root_home,
                        },
                    ],
                    root_header
                        .depth
                        .checked_add(1)
                        .ok_or(Ext4FormatError::OutOfBounds)?,
                )?;
                extent_metadata.push(MetadataBlock {
                    home: left_root_home,
                    role: MetaRole::ExtentNode,
                    before_version: crc32c(0, &left_root_before) as u64,
                    after: left_root_after,
                    depends_on: Vec::new(),
                });
                extent_metadata.push(MetadataBlock {
                    home: right_root_home,
                    role: MetaRole::ExtentNode,
                    before_version: crc32c(0, &right_root_before) as u64,
                    after: right_root_after,
                    depends_on: Vec::new(),
                });
            } else {
                inode_after.set_extent_index_root(&root_indexes, root_header.depth)?;
            }
        } else {
            ExtentNode::encode_index(parent_header.depth, &parent_indexes, &mut parent_after)?;
            extent_metadata.push(MetadataBlock {
                home: parent_home,
                role: MetaRole::ExtentNode,
                before_version: crc32c(0, &parent_before) as u64,
                after: parent_after,
                depends_on: Vec::new(),
            });
            if root_indexes[root_pos].logical_block != parent_indexes[0].logical_block {
                root_indexes[root_pos].logical_block = parent_indexes[0].logical_block;
                inode_after.set_extent_index_root(&root_indexes, root_header.depth)?;
            }
        }
        inode_after.blocks_512 = inode_after
            .blocks_512
            .checked_add(
                u64::try_from(allocation.claims.len()).map_err(|_| Ext4FormatError::OutOfBounds)?
                    * (BLOCK_SIZE / 512) as u64,
            )
            .ok_or(Ext4FormatError::OutOfBounds)?;
        Ok(Some(IndexedUninitializedConversion {
            physical_block,
            extent_metadata,
            allocation: Some(allocation),
            inode_after: Some(inode_after),
        }))
    }

    /// Convert an unwritten block through one depth-two root -> parent -> leaf
    /// path. A full parent carries one new sibling into a non-full inode root.
    fn plan_depth_two_uninitialized_conversion(
        &mut self,
        inode: &Inode,
        logical_block: u32,
    ) -> Result<Option<IndexedUninitializedConversion>> {
        let root_header = ExtentHeader::parse(inode.extent_root_bytes())?;
        if root_header.depth != 2 {
            return Ok(None);
        }
        let mut root_indexes = match ExtentNode::parse(inode.extent_root_bytes())? {
            ExtentNode::Index(indexes) if !indexes.is_empty() => indexes,
            _ => return Err(Ext4FormatError::Corrupt),
        };
        let root_pos = root_indexes
            .iter()
            .rposition(|index| index.logical_block <= logical_block)
            .unwrap_or(0);
        let parent_home = root_indexes[root_pos].child;
        let mut parent_before = [0u8; BLOCK_SIZE];
        self.read_block(parent_home, &mut parent_before)?;
        let parent_header = ExtentHeader::parse(&parent_before)?;
        if parent_header.depth != 1 {
            return Err(Ext4FormatError::Corrupt);
        }
        let mut parent_indexes = match ExtentNode::parse(&parent_before)? {
            ExtentNode::Index(indexes) if !indexes.is_empty() => indexes,
            _ => return Err(Ext4FormatError::Corrupt),
        };
        let parent_pos = parent_indexes
            .iter()
            .rposition(|index| index.logical_block <= logical_block)
            .unwrap_or(0);
        let home = parent_indexes[parent_pos].child;
        let mut before = [0u8; BLOCK_SIZE];
        self.read_block(home, &mut before)?;
        let header = ExtentHeader::parse(&before)?;
        if header.depth != 0 {
            return Err(Ext4FormatError::Corrupt);
        }
        let mut extents = match ExtentNode::parse(&before)? {
            ExtentNode::Leaf(extents) => extents,
            ExtentNode::Index(_) => return Err(Ext4FormatError::Corrupt),
        };
        let Some(physical_block) = convert_uninitialized_extent_list(&mut extents, logical_block)?
        else {
            return Ok(None);
        };
        if extents.len() <= usize::from(header.max) {
            let mut after = before;
            ExtentNode::encode_leaf(&extents, &mut after)?;
            return Ok(Some(IndexedUninitializedConversion {
                physical_block,
                extent_metadata: vec![MetadataBlock {
                    home,
                    role: MetaRole::ExtentNode,
                    before_version: crc32c(0, &before) as u64,
                    after,
                    depends_on: Vec::new(),
                }],
                allocation: None,
                inode_after: None,
            }));
        }
        let parent_split = parent_indexes.len() >= usize::from(parent_header.max);
        let root_growth = parent_split && root_indexes.len() >= usize::from(root_header.max);
        let allocation = self.plan_block_allocations(match (parent_split, root_growth) {
            (false, false) => 1,
            (true, false) => 2,
            (true, true) => 4,
            (false, true) => return Err(Ext4FormatError::Corrupt),
        })?;
        let right_home = allocation.claims[0];
        let right_extents = extents.split_off(extents.len() / 2);
        let mut leaf_after = before;
        ExtentNode::encode_leaf(&extents, &mut leaf_after)?;
        let mut right_before = [0u8; BLOCK_SIZE];
        self.read_block(right_home, &mut right_before)?;
        let mut right_after = right_before;
        ExtentNode::encode_leaf(&right_extents, &mut right_after)?;
        parent_indexes[parent_pos].logical_block = extents[0].logical_block;
        parent_indexes.insert(
            parent_pos + 1,
            ExtentIdx {
                logical_block: right_extents[0].logical_block,
                child: right_home,
            },
        );
        let mut parent_after = parent_before;
        let mut inode_after = *inode;
        let mut extent_metadata = vec![
            MetadataBlock {
                home,
                role: MetaRole::ExtentNode,
                before_version: crc32c(0, &before) as u64,
                after: leaf_after,
                depends_on: Vec::new(),
            },
            MetadataBlock {
                home: right_home,
                role: MetaRole::ExtentNode,
                before_version: crc32c(0, &right_before) as u64,
                after: right_after,
                depends_on: Vec::new(),
            },
        ];
        if parent_split {
            let right_parent_home = allocation.claims[1];
            let split = parent_indexes.len() / 2;
            let right_parent_indexes = parent_indexes.split_off(split);
            let mut right_parent_before = [0u8; BLOCK_SIZE];
            self.read_block(right_parent_home, &mut right_parent_before)?;
            let mut right_parent_after = right_parent_before;
            ExtentNode::encode_index(1, &parent_indexes, &mut parent_after)?;
            ExtentNode::encode_index(1, &right_parent_indexes, &mut right_parent_after)?;
            root_indexes[root_pos].logical_block = parent_indexes[0].logical_block;
            root_indexes.insert(
                root_pos + 1,
                ExtentIdx {
                    logical_block: right_parent_indexes[0].logical_block,
                    child: right_parent_home,
                },
            );
            extent_metadata.push(MetadataBlock {
                home: parent_home,
                role: MetaRole::ExtentNode,
                before_version: crc32c(0, &parent_before) as u64,
                after: parent_after,
                depends_on: Vec::new(),
            });
            extent_metadata.push(MetadataBlock {
                home: right_parent_home,
                role: MetaRole::ExtentNode,
                before_version: crc32c(0, &right_parent_before) as u64,
                after: right_parent_after,
                depends_on: Vec::new(),
            });
            if root_growth {
                let left_root_home = allocation.claims[2];
                let right_root_home = allocation.claims[3];
                let split = root_indexes.len() / 2;
                let right_root_indexes = root_indexes.split_off(split);
                let mut left_root_before = [0u8; BLOCK_SIZE];
                self.read_block(left_root_home, &mut left_root_before)?;
                let mut right_root_before = [0u8; BLOCK_SIZE];
                self.read_block(right_root_home, &mut right_root_before)?;
                let mut left_root_after = left_root_before;
                let mut right_root_after = right_root_before;
                ExtentNode::encode_index(root_header.depth, &root_indexes, &mut left_root_after)?;
                ExtentNode::encode_index(
                    root_header.depth,
                    &right_root_indexes,
                    &mut right_root_after,
                )?;
                inode_after.set_extent_index_root(
                    &[
                        ExtentIdx {
                            logical_block: root_indexes[0].logical_block,
                            child: left_root_home,
                        },
                        ExtentIdx {
                            logical_block: right_root_indexes[0].logical_block,
                            child: right_root_home,
                        },
                    ],
                    root_header
                        .depth
                        .checked_add(1)
                        .ok_or(Ext4FormatError::OutOfBounds)?,
                )?;
                extent_metadata.push(MetadataBlock {
                    home: left_root_home,
                    role: MetaRole::ExtentNode,
                    before_version: crc32c(0, &left_root_before) as u64,
                    after: left_root_after,
                    depends_on: Vec::new(),
                });
                extent_metadata.push(MetadataBlock {
                    home: right_root_home,
                    role: MetaRole::ExtentNode,
                    before_version: crc32c(0, &right_root_before) as u64,
                    after: right_root_after,
                    depends_on: Vec::new(),
                });
            } else {
                inode_after.set_extent_index_root(&root_indexes, root_header.depth)?;
            }
        } else {
            ExtentNode::encode_index(1, &parent_indexes, &mut parent_after)?;
            extent_metadata.push(MetadataBlock {
                home: parent_home,
                role: MetaRole::ExtentNode,
                before_version: crc32c(0, &parent_before) as u64,
                after: parent_after,
                depends_on: Vec::new(),
            });
        }
        inode_after.blocks_512 = inode_after
            .blocks_512
            .checked_add(
                u64::try_from(allocation.claims.len()).map_err(|_| Ext4FormatError::OutOfBounds)?
                    * (BLOCK_SIZE / 512) as u64,
            )
            .ok_or(Ext4FormatError::OutOfBounds)?;
        Ok(Some(IndexedUninitializedConversion {
            physical_block,
            extent_metadata,
            allocation: Some(allocation),
            inode_after: Some(inode_after),
        }))
    }

    /// Convert an unwritten block through a depth-three-or-deeper indexed path.
    /// A split carries the new right sibling through the captured ancestors in
    /// one immutable plan, growing the inline root only when every ancestor is
    /// full.
    fn plan_deep_uninitialized_conversion(
        &mut self,
        inode: &Inode,
        logical_block: u32,
    ) -> Result<Option<IndexedUninitializedConversion>> {
        let root_header = ExtentHeader::parse(inode.extent_root_bytes())?;
        if root_header.depth < 3 {
            return Ok(None);
        }
        if root_header.depth > EXT4_MAX_EXTENT_DEPTH {
            return Err(Ext4FormatError::Corrupt);
        }
        validate_extent_node_layout(inode.extent_root_bytes(), root_header)?;
        let mut root_indexes = match ExtentNode::parse(inode.extent_root_bytes())? {
            ExtentNode::Index(indexes) if !indexes.is_empty() => indexes,
            _ => return Err(Ext4FormatError::Corrupt),
        };
        let root_pos = root_indexes
            .iter()
            .rposition(|index| index.logical_block <= logical_block)
            .unwrap_or(0);
        let mut expected_depth = root_header
            .depth
            .checked_sub(1)
            .ok_or(Ext4FormatError::Corrupt)?;
        let mut home = root_indexes[root_pos].child;
        let mut visited_homes = Vec::new();
        let mut path = Vec::new();

        let (leaf_home, leaf_before, leaf_header, mut extents, physical_block) = loop {
            if visited_homes.contains(&home) {
                return Err(Ext4FormatError::Corrupt);
            }
            visited_homes.push(home);

            let mut before = [0u8; BLOCK_SIZE];
            self.read_block(home, &mut before)?;
            let header = ExtentHeader::parse(&before)?;
            if header.depth != expected_depth {
                return Err(Ext4FormatError::Corrupt);
            }
            validate_extent_node_layout(&before, header)?;
            if expected_depth == 0 {
                let mut extents = match ExtentNode::parse(&before)? {
                    ExtentNode::Leaf(extents) => extents,
                    ExtentNode::Index(_) => return Err(Ext4FormatError::Corrupt),
                };
                let Some(physical_block) =
                    convert_uninitialized_extent_list(&mut extents, logical_block)?
                else {
                    return Ok(None);
                };
                if extents.len() <= usize::from(header.max) {
                    let mut after = before;
                    ExtentNode::encode_leaf(&extents, &mut after)?;
                    return Ok(Some(IndexedUninitializedConversion {
                        physical_block,
                        extent_metadata: vec![MetadataBlock {
                            home,
                            role: MetaRole::ExtentNode,
                            before_version: crc32c(0, &before) as u64,
                            after,
                            depends_on: Vec::new(),
                        }],
                        allocation: None,
                        inode_after: None,
                    }));
                }
                break (home, before, header, extents, physical_block);
            }
            let indexes = match ExtentNode::parse(&before)? {
                ExtentNode::Index(indexes) if !indexes.is_empty() => indexes,
                _ => return Err(Ext4FormatError::Corrupt),
            };
            let selected = indexes
                .iter()
                .rposition(|index| index.logical_block <= logical_block)
                .unwrap_or(0);
            let child_home = indexes[selected].child;
            path.push((home, before, header, indexes, selected));
            home = child_home;
            expected_depth = expected_depth
                .checked_sub(1)
                .ok_or(Ext4FormatError::Corrupt)?;
        };

        if extents.len() <= usize::from(leaf_header.max) {
            return Err(Ext4FormatError::Corrupt);
        }
        if extents.len() < 2 {
            return Err(Ext4FormatError::Corrupt);
        }

        let mut allocation_count = 1usize;
        let mut carry_reaches_root = true;
        for (_, _, header, indexes, _) in path.iter().rev() {
            if indexes.len() < usize::from(header.max) {
                carry_reaches_root = false;
                break;
            }
            allocation_count = allocation_count
                .checked_add(1)
                .ok_or(Ext4FormatError::OutOfBounds)?;
        }
        if carry_reaches_root && root_indexes.len() >= usize::from(root_header.max) {
            if root_header.depth >= EXT4_MAX_EXTENT_DEPTH {
                return Err(Ext4FormatError::Unsupported);
            }
            allocation_count = allocation_count
                .checked_add(2)
                .ok_or(Ext4FormatError::OutOfBounds)?;
        }
        let allocation = self.plan_block_allocations(allocation_count)?;
        if allocation
            .claims
            .iter()
            .any(|claim| visited_homes.contains(claim))
        {
            return Err(Ext4FormatError::Corrupt);
        }

        let right_leaf_home = allocation.claims[0];
        let right_extents = extents.split_off(extents.len() / 2);
        let mut leaf_after = leaf_before;
        ExtentNode::encode_leaf(&extents, &mut leaf_after)?;
        let mut right_leaf_before = [0u8; BLOCK_SIZE];
        self.read_block(right_leaf_home, &mut right_leaf_before)?;
        let mut right_leaf_after = right_leaf_before;
        ExtentNode::encode_leaf(&right_extents, &mut right_leaf_after)?;
        let mut extent_metadata = vec![
            MetadataBlock {
                home: leaf_home,
                role: MetaRole::ExtentNode,
                before_version: crc32c(0, &leaf_before) as u64,
                after: leaf_after,
                depends_on: Vec::new(),
            },
            MetadataBlock {
                home: right_leaf_home,
                role: MetaRole::ExtentNode,
                before_version: crc32c(0, &right_leaf_before) as u64,
                after: right_leaf_after,
                depends_on: Vec::new(),
            },
        ];
        let mut claim_index = 1usize;
        let mut left_key = extents[0].logical_block;
        let mut right_index = ExtentIdx {
            logical_block: right_extents[0].logical_block,
            child: right_leaf_home,
        };
        let mut carry = true;

        while let Some((parent_home, before, header, mut indexes, selected)) = path.pop() {
            indexes[selected].logical_block = left_key;
            indexes.insert(selected + 1, right_index);
            if indexes.len() <= usize::from(header.max) {
                let mut after = before;
                ExtentNode::encode_index(header.depth, &indexes, &mut after)?;
                extent_metadata.push(MetadataBlock {
                    home: parent_home,
                    role: MetaRole::ExtentNode,
                    before_version: crc32c(0, &before) as u64,
                    after,
                    depends_on: Vec::new(),
                });
                carry = false;
                break;
            }

            let right_home = *allocation
                .claims
                .get(claim_index)
                .ok_or(Ext4FormatError::Corrupt)?;
            claim_index += 1;
            let right_indexes = indexes.split_off(indexes.len() / 2);
            let mut after = before;
            ExtentNode::encode_index(header.depth, &indexes, &mut after)?;
            let mut right_before = [0u8; BLOCK_SIZE];
            self.read_block(right_home, &mut right_before)?;
            let mut right_after = right_before;
            ExtentNode::encode_index(header.depth, &right_indexes, &mut right_after)?;
            extent_metadata.push(MetadataBlock {
                home: parent_home,
                role: MetaRole::ExtentNode,
                before_version: crc32c(0, &before) as u64,
                after,
                depends_on: Vec::new(),
            });
            extent_metadata.push(MetadataBlock {
                home: right_home,
                role: MetaRole::ExtentNode,
                before_version: crc32c(0, &right_before) as u64,
                after: right_after,
                depends_on: Vec::new(),
            });
            left_key = indexes[0].logical_block;
            right_index = ExtentIdx {
                logical_block: right_indexes[0].logical_block,
                child: right_home,
            };
        }

        let mut inode_after = *inode;
        if carry {
            root_indexes[root_pos].logical_block = left_key;
            root_indexes.insert(root_pos + 1, right_index);
            if root_indexes.len() <= usize::from(root_header.max) {
                inode_after.set_extent_index_root(&root_indexes, root_header.depth)?;
            } else {
                let left_root_home = *allocation
                    .claims
                    .get(claim_index)
                    .ok_or(Ext4FormatError::Corrupt)?;
                claim_index += 1;
                let right_root_home = *allocation
                    .claims
                    .get(claim_index)
                    .ok_or(Ext4FormatError::Corrupt)?;
                claim_index += 1;
                let right_root_indexes = root_indexes.split_off(root_indexes.len() / 2);
                let mut left_root_before = [0u8; BLOCK_SIZE];
                self.read_block(left_root_home, &mut left_root_before)?;
                let mut left_root_after = left_root_before;
                ExtentNode::encode_index(root_header.depth, &root_indexes, &mut left_root_after)?;
                let mut right_root_before = [0u8; BLOCK_SIZE];
                self.read_block(right_root_home, &mut right_root_before)?;
                let mut right_root_after = right_root_before;
                ExtentNode::encode_index(
                    root_header.depth,
                    &right_root_indexes,
                    &mut right_root_after,
                )?;
                inode_after.set_extent_index_root(
                    &[
                        ExtentIdx {
                            logical_block: root_indexes[0].logical_block,
                            child: left_root_home,
                        },
                        ExtentIdx {
                            logical_block: right_root_indexes[0].logical_block,
                            child: right_root_home,
                        },
                    ],
                    root_header
                        .depth
                        .checked_add(1)
                        .ok_or(Ext4FormatError::OutOfBounds)?,
                )?;
                extent_metadata.push(MetadataBlock {
                    home: left_root_home,
                    role: MetaRole::ExtentNode,
                    before_version: crc32c(0, &left_root_before) as u64,
                    after: left_root_after,
                    depends_on: Vec::new(),
                });
                extent_metadata.push(MetadataBlock {
                    home: right_root_home,
                    role: MetaRole::ExtentNode,
                    before_version: crc32c(0, &right_root_before) as u64,
                    after: right_root_after,
                    depends_on: Vec::new(),
                });
            }
        }
        if claim_index != allocation.claims.len() {
            return Err(Ext4FormatError::Corrupt);
        }
        inode_after.blocks_512 = inode_after
            .blocks_512
            .checked_add(
                u64::try_from(allocation.claims.len()).map_err(|_| Ext4FormatError::OutOfBounds)?
                    * (BLOCK_SIZE / 512) as u64,
            )
            .ok_or(Ext4FormatError::OutOfBounds)?;
        Ok(Some(IndexedUninitializedConversion {
            physical_block,
            extent_metadata,
            allocation: Some(allocation),
            inode_after: Some(inode_after),
        }))
    }

    /// Build one immutable writeback transaction for a contiguous page run.
    ///
    /// Every single-page planner turn is evaluated against the preceding
    /// turn's metadata after-images. The resulting chain is then collapsed to
    /// one before/final-after pair per metadata home. This lets one L4 batch
    /// allocate holes without publishing intermediate bitmap or extent states.
    pub fn plan_write_pages(
        &mut self,
        inode: InodeNo,
        start_file_page: u64,
        pages: &[Page4K],
        fsync_stamp: FsyncStamp,
    ) -> Result<Ext4MutationPlan> {
        self.plan_write_pages_with_size(inode, start_file_page, pages, None, fsync_stamp)
    }

    /// [`Self::plan_write_pages`] with an optional byte-precise final inode
    /// size. Buffered writeback uses this to fold close-visible EOF into the
    /// same ordered transaction as data and allocation metadata.
    pub fn plan_write_pages_with_size(
        &mut self,
        inode: InodeNo,
        start_file_page: u64,
        pages: &[Page4K],
        exact_size: Option<u64>,
        fsync_stamp: FsyncStamp,
    ) -> Result<Ext4MutationPlan> {
        if pages.is_empty() {
            return Err(Ext4FormatError::Unsupported);
        }
        let pending_before = self.pending_metadata.clone();
        let planned = (|| {
            let mut combined =
                Ext4MutationPlan::new(MutationOrigin::FlushPage, inode.get() as u64, fsync_stamp);
            for (offset, page) in pages.iter().enumerate() {
                let file_page_index = start_file_page
                    .checked_add(offset as u64)
                    .ok_or(Ext4FormatError::OutOfBounds)?;
                let part = self.plan_write_page(inode, file_page_index, page, fsync_stamp)?;
                merge_chained_mutation(&mut combined, &part)?;
                self.stage_mutation_after_images(&part);
            }
            if let Some(exact_size) = exact_size {
                self.set_write_plan_inode_size(inode, exact_size, &mut combined)?;
            }
            Ok(combined)
        })();
        self.pending_metadata = pending_before;
        planned
    }

    fn set_write_plan_inode_size(
        &self,
        inode: InodeNo,
        exact_size: u64,
        plan: &mut Ext4MutationPlan,
    ) -> Result<()> {
        if plan.data.iter().any(|write| {
            write
                .logical_page
                .checked_mul(BLOCK_SIZE as u64)
                .is_none_or(|start| start >= exact_size)
        }) {
            return Err(Ext4FormatError::Corrupt);
        }
        let location = self.inode_location(inode)?;
        let metadata = plan
            .metadata
            .iter_mut()
            .find(|metadata| {
                metadata.home == location.block && metadata.role == MetaRole::InodeTable
            })
            .ok_or(Ext4FormatError::Corrupt)?;
        let inode_bytes = &mut metadata.after[location.offset..location.offset + location.len];
        let mut disk_inode = Inode::parse(inode_bytes)?;
        disk_inode.size = exact_size;
        disk_inode.encode_preserving_unknown(inode_bytes)?;
        self.refresh_inode_checksum(inode, &disk_inode, inode_bytes)
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
        self.read_block(location.block, &mut inode_table_before)?;
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

    /// Build one immutable recursive truncate subtree plan without mutating
    /// any home block. Child metadata is emitted before parent metadata.
    fn plan_truncate_subtree(
        &self,
        home: Option<u64>,
        before: &[u8],
        expected_depth: u16,
        new_end: u32,
        visited_homes: &mut Vec<u64>,
    ) -> Result<TruncateSubtreePlan> {
        let header = ExtentHeader::parse(before)?;
        if header.depth != expected_depth {
            return Err(Ext4FormatError::Corrupt);
        }
        validate_extent_node_layout(before, header)?;
        if let Some(home) = home {
            if visited_homes.contains(&home) {
                return Err(Ext4FormatError::Corrupt);
            }
            visited_homes.push(home);
        }

        let mut plan = match ExtentNode::parse(before)? {
            ExtentNode::Leaf(extents) => {
                if expected_depth != 0 {
                    return Err(Ext4FormatError::Corrupt);
                }
                let original = extents.clone();
                let original_first_key = original.first().map(|extent| extent.logical_block);
                let original_logical_end = original
                    .last()
                    .map(|extent| {
                        extent
                            .logical_block
                            .checked_add(extent.initialized_len())
                            .ok_or(Ext4FormatError::Corrupt)
                    })
                    .transpose()?;
                let (retained, released) = truncate_extents_at(extents, new_end)?;
                let retained_data = retained
                    .iter()
                    .map(|extent| {
                        Ok(PhysicalBlockRange {
                            start: extent.physical_start,
                            end: extent
                                .physical_start
                                .checked_add(u64::from(extent.initialized_len()))
                                .ok_or(Ext4FormatError::OutOfBounds)?,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                let state = retained
                    .first()
                    .map_or(TruncateSubtreeState::Removed, |first| {
                        TruncateSubtreeState::Retained {
                            first_key: first.logical_block,
                            changed: retained != original,
                            node: ExtentNode::Leaf(retained.clone()),
                        }
                    });
                TruncateSubtreePlan {
                    state,
                    original_first_key,
                    original_logical_end,
                    released,
                    retained_data,
                    retained_homes: Vec::new(),
                    metadata: Vec::new(),
                }
            }
            ExtentNode::Index(indexes) => {
                if expected_depth == 0 {
                    return Err(Ext4FormatError::Corrupt);
                }
                let mut retained_indexes = Vec::new();
                let mut original_first_key = None;
                let mut original_logical_end = None;
                let mut released = Vec::new();
                let mut retained_data = Vec::new();
                let mut retained_homes = Vec::new();
                let mut metadata = Vec::new();
                for index in &indexes {
                    let mut child_before = Box::new([0u8; BLOCK_SIZE]);
                    self.read_block(index.child, child_before.as_mut())?;
                    let child = self.plan_truncate_subtree(
                        Some(index.child),
                        child_before.as_ref(),
                        expected_depth - 1,
                        new_end,
                        visited_homes,
                    )?;
                    let child_first = child.original_first_key.ok_or(Ext4FormatError::Corrupt)?;
                    let child_end = child.original_logical_end.ok_or(Ext4FormatError::Corrupt)?;
                    if child_first != index.logical_block
                        || original_logical_end.is_some_and(|end| child_first < end)
                    {
                        return Err(Ext4FormatError::Corrupt);
                    }
                    original_first_key.get_or_insert(child_first);
                    original_logical_end = Some(child_end);
                    released.extend(child.released);
                    retained_data.extend(child.retained_data);
                    retained_homes.extend(child.retained_homes);
                    metadata.extend(child.metadata);
                    if let TruncateSubtreeState::Retained { first_key, .. } = child.state {
                        retained_indexes.push(ExtentIdx {
                            logical_block: first_key,
                            child: index.child,
                        });
                    }
                }
                let state =
                    retained_indexes
                        .first()
                        .map_or(TruncateSubtreeState::Removed, |first| {
                            TruncateSubtreeState::Retained {
                                first_key: first.logical_block,
                                changed: retained_indexes != indexes,
                                node: ExtentNode::Index(retained_indexes.clone()),
                            }
                        });
                TruncateSubtreePlan {
                    state,
                    original_first_key,
                    original_logical_end,
                    released,
                    retained_data,
                    retained_homes,
                    metadata,
                }
            }
        };

        match (&plan.state, home) {
            (TruncateSubtreeState::Removed, Some(home)) => {
                if plan.released.contains(&home) {
                    return Err(Ext4FormatError::Corrupt);
                }
                plan.released.push(home);
            }
            (TruncateSubtreeState::Retained { node, changed, .. }, Some(home)) => {
                plan.retained_homes.push(home);
                if !changed {
                    return Ok(plan);
                }
                let before: &Page4K = before.try_into().map_err(|_| Ext4FormatError::Corrupt)?;
                let mut after = *before;
                match node {
                    ExtentNode::Leaf(extents) => ExtentNode::encode_leaf(extents, &mut after)?,
                    ExtentNode::Index(indexes) => {
                        ExtentNode::encode_index(expected_depth, indexes, &mut after)?
                    }
                }
                if after != *before {
                    plan.metadata.push(MetadataBlock {
                        home,
                        role: MetaRole::ExtentNode,
                        before_version: crc32c(0, before) as u64,
                        after,
                        depends_on: Vec::new(),
                    });
                }
            }
            (_, None) => {}
        }
        Ok(plan)
    }

    fn plan_extent_release_tree(
        &self,
        root: &[u8],
        new_end: u32,
    ) -> Result<(u16, TruncateSubtreePlan)> {
        let root_depth = ExtentHeader::parse(root)?.depth;
        if root_depth > EXT4_MAX_EXTENT_DEPTH {
            return Err(Ext4FormatError::Corrupt);
        }
        let mut visited_homes = Vec::new();
        let mut subtree =
            self.plan_truncate_subtree(None, root, root_depth, new_end, &mut visited_homes)?;
        let collapse_home = match &subtree.state {
            TruncateSubtreeState::Retained {
                node: ExtentNode::Index(indexes),
                ..
            } if root_depth == 1 && indexes.len() == 1 => Some(indexes[0].child),
            _ => None,
        };
        if let Some(home) = collapse_home {
            let mut leaf_bytes = [0u8; BLOCK_SIZE];
            if let Some(after) = subtree
                .metadata
                .iter()
                .find(|block| block.home == home && block.role == MetaRole::ExtentNode)
                .map(|block| block.after)
            {
                leaf_bytes = after;
            } else {
                self.read_block(home, &mut leaf_bytes)?;
            }
            if let ExtentNode::Leaf(extents) = ExtentNode::parse(&leaf_bytes)? {
                if extents.len() <= 4 {
                    let first_key = extents
                        .first()
                        .ok_or(Ext4FormatError::Corrupt)?
                        .logical_block;
                    subtree
                        .metadata
                        .retain(|block| block.home != home || block.role != MetaRole::ExtentNode);
                    let retained = subtree
                        .retained_homes
                        .iter()
                        .position(|retained| *retained == home)
                        .ok_or(Ext4FormatError::Corrupt)?;
                    subtree.retained_homes.remove(retained);
                    subtree.released.push(home);
                    subtree.state = TruncateSubtreeState::Retained {
                        node: ExtentNode::Leaf(extents),
                        first_key,
                        changed: true,
                    };
                }
            }
        }
        let released_set = subtree.released.iter().copied().collect::<BTreeSet<_>>();
        let mut retained_data = subtree.retained_data.clone();
        retained_data.sort_unstable_by_key(|range| range.start);
        if released_set.len() != subtree.released.len()
            || released_set
                .iter()
                .any(|block| self.reserved_blocks.contains(block))
            || subtree
                .retained_homes
                .iter()
                .any(|home| released_set.contains(home))
            || retained_data
                .iter()
                .any(|range| released_set.range(range.start..range.end).next().is_some())
            || retained_data.iter().any(|range| {
                self.reserved_blocks
                    .range(range.start..range.end)
                    .next()
                    .is_some()
            })
            || retained_data
                .windows(2)
                .any(|ranges| ranges[0].overlaps(ranges[1]))
            || subtree.retained_homes.iter().any(|home| {
                self.reserved_blocks.contains(home)
                    || physical_ranges_contain(&retained_data, *home)
            })
        {
            return Err(Ext4FormatError::Corrupt);
        }
        Ok((root_depth, subtree))
    }

    fn reserve_filesystem_metadata(&mut self) -> Result<()> {
        self.reserved_blocks.insert(0);
        let inode_table_bytes = u64::from(self.superblock.inodes_per_group)
            .checked_mul(u64::from(self.superblock.inode_size))
            .ok_or(Ext4FormatError::OutOfBounds)?;
        let inode_table_blocks = div_ceil_u64(inode_table_bytes, BLOCK_SIZE as u64);
        for group_index in 0..self.groups.len() {
            let group = self.groups[group_index];
            self.reserved_blocks
                .insert(self.group_desc_home(group_index)?);
            self.reserved_blocks.insert(group.block_bitmap_block());
            self.reserved_blocks.insert(group.inode_bitmap_block());
            for offset in 0..inode_table_blocks {
                self.reserved_blocks.insert(
                    group
                        .inode_table_block()
                        .checked_add(offset)
                        .ok_or(Ext4FormatError::OutOfBounds)?,
                );
            }
        }
        Ok(())
    }

    pub fn plan_truncate_size(
        &mut self,
        inode: InodeNo,
        new_size: u64,
        fsync_stamp: FsyncStamp,
    ) -> Result<Ext4MutationPlan> {
        let location = self.inode_location(inode)?;
        let mut inode_table_before = [0u8; BLOCK_SIZE];
        self.read_block(location.block, &mut inode_table_before)?;
        let mut inode_table_after = inode_table_before;
        let inode_bytes = &mut inode_table_after[location.offset..location.offset + location.len];
        let mut disk_inode = Inode::parse(inode_bytes)?;
        if !disk_inode.is_file() {
            return Err(Ext4FormatError::Unsupported);
        }

        let mut released = Vec::new();
        let mut extent_metadata = Vec::new();
        if new_size < disk_inode.size {
            let new_end = u32::try_from(div_ceil_u64(new_size, BLOCK_SIZE as u64))
                .map_err(|_| Ext4FormatError::OutOfBounds)?;
            let (root_depth, subtree) =
                self.plan_extent_release_tree(disk_inode.extent_root_bytes(), new_end)?;
            match subtree.state {
                TruncateSubtreeState::Removed => disk_inode.set_extent_root(&[])?,
                TruncateSubtreeState::Retained { node, changed, .. } if changed => match node {
                    ExtentNode::Leaf(extents) => disk_inode.set_extent_root(&extents)?,
                    ExtentNode::Index(indexes) => {
                        disk_inode.set_extent_index_root(&indexes, root_depth)?
                    }
                },
                TruncateSubtreeState::Retained { .. } => {}
            }
            released = subtree.released;
            extent_metadata = subtree.metadata;
        }
        let released_count =
            u64::try_from(released.len()).map_err(|_| Ext4FormatError::OutOfBounds)?;
        let mut plan =
            Ext4MutationPlan::new(MutationOrigin::Truncate, inode.get() as u64, fsync_stamp);
        if !released.is_empty() {
            let releases = self.plan_block_releases(&released)?;
            let group_desc_updates = self.plan_group_free_block_increments(&releases)?;
            let (superblock_home, superblock_before, superblock_after) =
                self.plan_superblock_free_block_increment(released_count)?;
            for release in releases {
                plan.push_metadata(MetadataBlock {
                    home: release.bitmap_home,
                    role: MetaRole::BlockBitmap,
                    before_version: crc32c(0, &release.bitmap_before) as u64,
                    after: release.bitmap_after,
                    depends_on: Vec::new(),
                })
                .map_err(|_| Ext4FormatError::Corrupt)?;
            }
            for (home, before, after) in group_desc_updates {
                plan.push_metadata(MetadataBlock {
                    home,
                    role: MetaRole::GroupDescriptor,
                    before_version: crc32c(0, &before) as u64,
                    after,
                    depends_on: Vec::new(),
                })
                .map_err(|_| Ext4FormatError::Corrupt)?;
            }
            plan.push_metadata(MetadataBlock {
                home: superblock_home,
                role: MetaRole::Superblock,
                before_version: crc32c(0, &superblock_before) as u64,
                after: superblock_after,
                depends_on: Vec::new(),
            })
            .map_err(|_| Ext4FormatError::Corrupt)?;
            for physical_block in released.iter().copied() {
                plan.defer_free(physical_block);
            }
        }
        for metadata in extent_metadata {
            plan.push_metadata(metadata)
                .map_err(|_| Ext4FormatError::Corrupt)?;
        }
        disk_inode.blocks_512 = disk_inode
            .blocks_512
            .checked_sub(released_count * (BLOCK_SIZE as u64 / 512))
            .ok_or(Ext4FormatError::Corrupt)?;
        disk_inode.size = new_size;
        disk_inode.encode_preserving_unknown(inode_bytes)?;
        self.refresh_inode_checksum(inode, &disk_inode, inode_bytes)?;

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

    /// Build a bounded destroy plan for a VFS-proven dead inode.
    ///
    /// Supports zero-link regular files and empty directories with initialized
    /// extent trees, plus inline fast symlinks. Unwritten extents, non-empty
    /// directories, block-backed symlinks, and nonzero-link inodes remain
    /// fail-closed.
    pub fn plan_destroy_inode(
        &mut self,
        inode: InodeNo,
        fsync_stamp: FsyncStamp,
    ) -> Result<Ext4MutationPlan> {
        if self.superblock.inodes_per_group == 0 {
            return Err(Ext4FormatError::Corrupt);
        }
        let inode_index = inode
            .get()
            .checked_sub(1)
            .ok_or(Ext4FormatError::OutOfBounds)?;
        let inode_group = usize::try_from(inode_index / self.superblock.inodes_per_group)
            .map_err(|_| Ext4FormatError::OutOfBounds)?;
        let inode_bit = usize::try_from(inode_index % self.superblock.inodes_per_group)
            .map_err(|_| Ext4FormatError::OutOfBounds)?;
        let group = self
            .groups
            .get(inode_group)
            .ok_or(Ext4FormatError::OutOfBounds)?;

        let location = self.inode_location(inode)?;
        let mut inode_table_before = [0u8; BLOCK_SIZE];
        self.read_block(location.block, &mut inode_table_before)?;
        let mut inode_table_after = inode_table_before;
        let inode_bytes = &mut inode_table_after[location.offset..location.offset + location.len];
        let disk_inode = Inode::parse(inode_bytes)?;
        if disk_inode.links_count != 0 {
            return Err(Ext4FormatError::Unsupported);
        }

        let releases_directory = disk_inode.is_dir();
        let mut freed = if disk_inode.is_file() || releases_directory {
            if disk_inode.flags & Inode::EXTENTS_FL == 0 {
                return Err(Ext4FormatError::Unsupported);
            }
            let (_, subtree) = self.plan_extent_release_tree(disk_inode.extent_root_bytes(), 0)?;
            if !matches!(&subtree.state, TruncateSubtreeState::Removed)
                || !subtree.metadata.is_empty()
                || !subtree.retained_data.is_empty()
                || !subtree.retained_homes.is_empty()
            {
                return Err(Ext4FormatError::Corrupt);
            }
            if releases_directory {
                let data_blocks =
                    self.collect_extent_data_blocks(disk_inode.extent_root_bytes())?;
                self.ensure_destroy_directory_is_empty(inode, &data_blocks)?;
            }
            subtree.released
        } else if disk_inode.is_symlink() {
            if disk_inode.inline_symlink_target()?.is_none() {
                return Err(Ext4FormatError::Unsupported);
            }
            Vec::new()
        } else {
            return Err(Ext4FormatError::Unsupported);
        };
        freed.sort_unstable();
        if freed.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(Ext4FormatError::Corrupt);
        }

        let releases = self.plan_block_releases(&freed)?;

        let inode_bitmap_home = group.inode_bitmap_block();
        let mut inode_bitmap_before = [0u8; BLOCK_SIZE];
        self.read_block(inode_bitmap_home, &mut inode_bitmap_before)?;
        let mut inode_bitmap_after = inode_bitmap_before;
        if !BitmapView::new(&inode_bitmap_after).is_set(inode_bit) {
            return Err(Ext4FormatError::Corrupt);
        }
        BitmapMut::new(&mut inode_bitmap_after).clear(inode_bit)?;
        // Make the just-released slot the next allocation candidate. The hint
        // is deliberately non-authoritative: if this immutable destroy plan
        // is abandoned, the still-set on-disk bitmap bit simply makes the
        // allocator continue scanning. On commit, prompt reuse keeps inode
        // numbers and inode-table cache lines local while the generation field
        // still prevents stale dentries/page containers from aliasing it.
        self.next_inode_hint = u64::from(inode_index);

        let mut deleted_inode = Inode::default();
        // The inode bitmap is cleared by this same plan. Retaining a file
        // type here makes e2fsprogs treat the freed slot as an orphaned live
        // inode even after `s_last_orphan` has been cleared.
        deleted_inode.mode = 0;
        deleted_inode.ctime = disk_inode.ctime;
        // `i_dtime` carries the next orphan only while unlink keeps the inode
        // live. Once destroy clears the inode bitmap, it must not retain that
        // chain state or a deletion timestamp as a stale orphan pointer.
        deleted_inode.dtime = 0;
        deleted_inode.generation = disk_inode.generation;
        deleted_inode.extra_isize = disk_inode.extra_isize;
        deleted_inode.encode(inode_bytes)?;
        self.refresh_inode_checksum(inode, &deleted_inode, inode_bytes)?;

        let orphan_next = if disk_inode.links_count == 0 {
            Some((inode, disk_inode.dtime))
        } else {
            None
        };
        let released_blocks =
            u32::try_from(freed.len()).map_err(|_| Ext4FormatError::Unsupported)?;
        let released_dirs = if releases_directory { 1 } else { 0 };
        let group_desc_updates = self.plan_group_destroy_count_updates(
            &releases,
            inode_group,
            &inode_bitmap_after,
            released_dirs,
        )?;
        let (superblock_home, superblock_before, superblock_after) =
            self.plan_superblock_destroy_counts(released_blocks as u64, orphan_next)?;

        let mut plan =
            Ext4MutationPlan::new(MutationOrigin::Destroy, inode.get() as u64, fsync_stamp);
        for physical_block in freed.iter().copied() {
            plan.defer_free(physical_block);
        }
        for release in releases {
            plan.push_metadata(MetadataBlock {
                home: release.bitmap_home,
                role: MetaRole::BlockBitmap,
                before_version: crc32c(0, &release.bitmap_before) as u64,
                after: release.bitmap_after,
                depends_on: Vec::new(),
            })
            .map_err(|_| Ext4FormatError::Corrupt)?;
        }
        plan.push_metadata(MetadataBlock {
            home: inode_bitmap_home,
            role: MetaRole::InodeBitmap,
            before_version: crc32c(0, &inode_bitmap_before) as u64,
            after: inode_bitmap_after,
            depends_on: Vec::new(),
        })
        .map_err(|_| Ext4FormatError::Corrupt)?;
        for (home, before, after) in group_desc_updates {
            plan.push_metadata(MetadataBlock {
                home,
                role: MetaRole::GroupDescriptor,
                before_version: crc32c(0, &before) as u64,
                after,
                depends_on: Vec::new(),
            })
            .map_err(|_| Ext4FormatError::Corrupt)?;
        }
        plan.push_metadata(MetadataBlock {
            home: superblock_home,
            role: MetaRole::Superblock,
            before_version: crc32c(0, &superblock_before) as u64,
            after: superblock_after,
            depends_on: Vec::new(),
        })
        .map_err(|_| Ext4FormatError::Corrupt)?;
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

    fn ensure_destroy_directory_is_empty(&self, dir_ino: InodeNo, blocks: &[u64]) -> Result<()> {
        for block in blocks {
            let mut dir_block = [0u8; BLOCK_SIZE];
            self.read_block(*block, &mut dir_block)?;
            for entry in DirEntryIter::new(&dir_block) {
                let entry = entry?;
                match entry.name {
                    b"." if entry.inode == dir_ino.get() => {}
                    b".." => {}
                    _ => return Err(Ext4FormatError::Unsupported),
                }
            }
        }
        Ok(())
    }

    fn collect_extent_data_blocks(&self, root: &[u8]) -> Result<Vec<u64>> {
        let depth = ExtentHeader::parse(root)?.depth;
        if depth > EXT4_MAX_EXTENT_DEPTH {
            return Err(Ext4FormatError::Corrupt);
        }
        let mut visited = Vec::new();
        let mut blocks = Vec::new();
        self.collect_extent_data_subtree(root, depth, &mut visited, &mut blocks)?;
        blocks.sort_unstable();
        if blocks.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(Ext4FormatError::Corrupt);
        }
        Ok(blocks)
    }

    fn collect_extent_node_homes(&self, root: &[u8]) -> Result<Vec<u64>> {
        let depth = ExtentHeader::parse(root)?.depth;
        if depth > EXT4_MAX_EXTENT_DEPTH {
            return Err(Ext4FormatError::Corrupt);
        }
        let mut visited = Vec::new();
        let mut homes = Vec::new();
        self.collect_extent_node_homes_subtree(root, depth, &mut visited, &mut homes)?;
        homes.sort_unstable();
        Ok(homes)
    }

    fn collect_extent_node_homes_subtree(
        &self,
        before: &[u8],
        expected_depth: u16,
        visited: &mut Vec<u64>,
        homes: &mut Vec<u64>,
    ) -> Result<()> {
        let header = ExtentHeader::parse(before)?;
        if header.depth != expected_depth {
            return Err(Ext4FormatError::Corrupt);
        }
        if expected_depth == 0 {
            if !matches!(ExtentNode::parse(before)?, ExtentNode::Leaf(_)) {
                return Err(Ext4FormatError::Corrupt);
            }
            return Ok(());
        }
        let indexes = match ExtentNode::parse(before)? {
            ExtentNode::Index(indexes) => indexes,
            ExtentNode::Leaf(_) => return Err(Ext4FormatError::Corrupt),
        };
        for index in indexes {
            if visited.contains(&index.child) {
                return Err(Ext4FormatError::Corrupt);
            }
            visited.push(index.child);
            homes.push(index.child);
            let mut child = [0u8; BLOCK_SIZE];
            self.read_block(index.child, &mut child)?;
            self.collect_extent_node_homes_subtree(&child, expected_depth - 1, visited, homes)?;
        }
        Ok(())
    }

    fn collect_extent_data_subtree(
        &self,
        before: &[u8],
        expected_depth: u16,
        visited: &mut Vec<u64>,
        blocks: &mut Vec<u64>,
    ) -> Result<()> {
        let header = ExtentHeader::parse(before)?;
        if header.depth != expected_depth {
            return Err(Ext4FormatError::Corrupt);
        }
        match ExtentNode::parse(before)? {
            ExtentNode::Leaf(extents) => {
                if expected_depth != 0 {
                    return Err(Ext4FormatError::Corrupt);
                }
                for extent in extents {
                    if !extent.is_initialized() {
                        return Err(Ext4FormatError::Unsupported);
                    }
                    for offset in 0..extent.initialized_len() {
                        blocks.push(
                            extent
                                .physical_start
                                .checked_add(offset as u64)
                                .ok_or(Ext4FormatError::OutOfBounds)?,
                        );
                    }
                }
            }
            ExtentNode::Index(indexes) => {
                if expected_depth == 0 {
                    return Err(Ext4FormatError::Corrupt);
                }
                for index in indexes {
                    if visited.contains(&index.child) {
                        return Err(Ext4FormatError::Corrupt);
                    }
                    visited.push(index.child);
                    let mut child = [0u8; BLOCK_SIZE];
                    self.read_block(index.child, &mut child)?;
                    self.collect_extent_data_subtree(&child, expected_depth - 1, visited, blocks)?;
                }
            }
        }
        Ok(())
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

    fn refresh_extent_node_checksums(
        &self,
        inode: InodeNo,
        disk_inode: &Inode,
        metadata: &mut [MetadataBlock],
    ) {
        if !self.superblock.has_metadata_csum() {
            return;
        }
        let seed = self.superblock.metadata_csum_seed();
        for block in metadata.iter_mut() {
            if block.role != MetaRole::ExtentNode {
                continue;
            }
            let checksum =
                extent_block_csum32(seed, inode.get(), disk_inode.generation, &block.after);
            block.after[BLOCK_SIZE - 4..].copy_from_slice(&checksum.to_le_bytes());
        }
    }

    /// Prefer the physical successor/predecessor of an existing logical
    /// neighbour so extending writes coalesce into the inode's current extent.
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

    fn plan_block_allocation(&mut self) -> Result<(u64, usize, u64, Page4K, Page4K)> {
        let allocations = self.plan_block_allocations(1)?;
        let physical_block = allocations.claims[0];
        let group = allocations
            .groups
            .into_iter()
            .next()
            .ok_or(Ext4FormatError::Corrupt)?;
        Ok((
            physical_block,
            group.group_index,
            group.bitmap_home,
            group.bitmap_before,
            group.bitmap_after,
        ))
    }

    fn plan_block_allocations(&mut self, amount: usize) -> Result<PlannedBlockAllocations> {
        self.plan_block_allocations_near(amount, None)
    }

    /// Plan `amount` free blocks beginning near an extent goal or the mount's
    /// next-fit cursor. Every candidate is revalidated against the current
    /// bitmap after-image, so the cursor never carries allocation authority.
    fn plan_block_allocations_near(
        &mut self,
        amount: usize,
        goal: Option<u64>,
    ) -> Result<PlannedBlockAllocations> {
        if amount == 0 {
            return Err(Ext4FormatError::OutOfBounds);
        }
        if self.superblock.blocks_per_group == 0 {
            return Err(Ext4FormatError::Corrupt);
        }
        let blocks_per_group = self.superblock.blocks_per_group as u64;
        let first_data_block = self.superblock.first_data_block as u64;
        let total_blocks = core::cmp::min(self.superblock.blocks_count, self.image.total_blocks());
        if self.groups.is_empty() || first_data_block >= total_blocks {
            return Err(Ext4FormatError::OutOfBounds);
        }

        let scan_start = goal
            .filter(|block| *block >= first_data_block && *block < total_blocks)
            .or_else(|| {
                (self.next_block_hint >= first_data_block && self.next_block_hint < total_blocks)
                    .then_some(self.next_block_hint)
            })
            .unwrap_or(first_data_block);
        let preferred_group = usize::try_from((scan_start - first_data_block) / blocks_per_group)
            .map_err(|_| Ext4FormatError::OutOfBounds)?;
        if preferred_group >= self.groups.len() {
            return Err(Ext4FormatError::Corrupt);
        }

        let mut claims = Vec::new();
        let mut groups = Vec::new();
        for group_offset in 0..self.groups.len() {
            let group_index = (preferred_group + group_offset) % self.groups.len();
            let group = self.groups[group_index];
            let group_start = first_data_block + group_index as u64 * blocks_per_group;
            if group_start >= total_blocks {
                continue;
            }
            let group_block_count = core::cmp::min(blocks_per_group, total_blocks - group_start);
            let bitmap_home = group.block_bitmap_block();
            if bitmap_home == 0
                || bitmap_home >= total_blocks
                || group.inode_bitmap_block() == 0
                || group.inode_bitmap_block() >= total_blocks
                || group.inode_table_block() == 0
                || group.inode_table_block() >= total_blocks
            {
                return Err(Ext4FormatError::Corrupt);
            }
            let mut bitmap_before = [0u8; BLOCK_SIZE];
            self.read_block(bitmap_home, &mut bitmap_before)?;
            let mut bitmap_after = bitmap_before;
            let remaining = amount - claims.len();
            let mut free = Vec::new();
            let bit_count = group_block_count as usize;
            let start_bit = if group_index == preferred_group {
                usize::try_from(scan_start - group_start)
                    .map_err(|_| Ext4FormatError::OutOfBounds)?
            } else {
                0
            };
            for bit in (start_bit..bit_count).chain(0..start_bit) {
                if BitmapView::new(&bitmap_after).is_set(bit)
                    || self.reserved_blocks.contains(&(group_start + bit as u64))
                {
                    continue;
                }
                free.push(bit);
                if free.len() == remaining {
                    break;
                }
            }
            if free.is_empty() {
                continue;
            }
            for bit in &free {
                BitmapMut::new(&mut bitmap_after).set(*bit)?;
            }
            let count = u32::try_from(free.len()).map_err(|_| Ext4FormatError::OutOfBounds)?;
            claims.extend(free.into_iter().map(|bit| group_start + bit as u64));
            groups.push(PlannedGroupAllocation {
                group_index,
                bitmap_home,
                bitmap_before,
                bitmap_after,
                count,
            });
            if claims.len() == amount {
                let next = claims
                    .last()
                    .copied()
                    .and_then(|block| block.checked_add(1))
                    .unwrap_or(first_data_block);
                self.next_block_hint = if next < total_blocks {
                    next
                } else {
                    first_data_block
                };
                return Ok(PlannedBlockAllocations { claims, groups });
            }
        }

        Err(Ext4FormatError::OutOfBounds)
    }

    fn group_desc_home(&self, group_index: usize) -> Result<u64> {
        let byte_offset = group_index
            .checked_mul(self.superblock.group_desc_size())
            .ok_or(Ext4FormatError::OutOfBounds)?;
        let gdt_start = if self.superblock.block_size() == 1024 {
            2
        } else {
            1
        };
        Ok(gdt_start + (byte_offset / BLOCK_SIZE) as u64)
    }

    fn plan_inode_allocation(&mut self) -> Result<(InodeNo, usize, u64, Page4K, Page4K)> {
        if self.superblock.inodes_per_group == 0 {
            return Err(Ext4FormatError::Corrupt);
        }
        let inodes_per_group = self.superblock.inodes_per_group as u64;
        let total_inodes = self.superblock.inodes_count as u64;
        if self.groups.is_empty() || total_inodes == 0 {
            return Err(Ext4FormatError::OutOfBounds);
        }
        let scan_start = (self.next_inode_hint < total_inodes)
            .then_some(self.next_inode_hint)
            .unwrap_or(0);
        let preferred_group = usize::try_from(scan_start / inodes_per_group)
            .map_err(|_| Ext4FormatError::OutOfBounds)?;
        if preferred_group >= self.groups.len() {
            return Err(Ext4FormatError::Corrupt);
        }

        for group_offset in 0..self.groups.len() {
            let group_index = (preferred_group + group_offset) % self.groups.len();
            let group = self.groups[group_index];
            let group_first_index = group_index as u64 * inodes_per_group;
            if group_first_index >= total_inodes {
                continue;
            }
            let group_inode_count =
                core::cmp::min(inodes_per_group, total_inodes - group_first_index);
            let bitmap_home = group.inode_bitmap_block();
            let mut bitmap_before = [0u8; BLOCK_SIZE];
            self.read_block(bitmap_home, &mut bitmap_before)?;
            let mut bitmap_after = bitmap_before;
            let view = BitmapView::new(&bitmap_after);
            let bit_count = group_inode_count as usize;
            let start_bit = if group_index == preferred_group {
                usize::try_from(scan_start - group_first_index)
                    .map_err(|_| Ext4FormatError::OutOfBounds)?
            } else {
                0
            };
            let Some(bit) = (start_bit..bit_count)
                .chain(0..start_bit)
                .find(|bit| !view.is_set(*bit))
            else {
                continue;
            };
            BitmapMut::new(&mut bitmap_after).set(bit)?;
            let inode_number = group_first_index
                .checked_add(bit as u64)
                .and_then(|index| index.checked_add(1))
                .ok_or(Ext4FormatError::OutOfBounds)?;
            let inode_number =
                u32::try_from(inode_number).map_err(|_| Ext4FormatError::OutOfBounds)?;
            let allocated_index = group_first_index + bit as u64;
            self.next_inode_hint = if allocated_index + 1 < total_inodes {
                allocated_index + 1
            } else {
                0
            };
            return Ok((
                InodeNo::new(inode_number),
                group_index,
                bitmap_home,
                bitmap_before,
                bitmap_after,
            ));
        }

        Err(Ext4FormatError::OutOfBounds)
    }

    fn block_group_for_physical(&self, physical_block: u64) -> Result<(usize, u64, usize)> {
        if self.superblock.blocks_per_group == 0 {
            return Err(Ext4FormatError::Corrupt);
        }
        let blocks_per_group = self.superblock.blocks_per_group as u64;
        let first_data_block = self.superblock.first_data_block as u64;
        if physical_block < first_data_block {
            return Err(Ext4FormatError::Unsupported);
        }
        let total_blocks = core::cmp::min(self.superblock.blocks_count, self.image.total_blocks());
        if physical_block >= total_blocks {
            return Err(Ext4FormatError::OutOfBounds);
        }
        let relative = physical_block - first_data_block;
        let group_index = usize::try_from(relative / blocks_per_group)
            .map_err(|_| Ext4FormatError::OutOfBounds)?;
        let bit = usize::try_from(relative % blocks_per_group)
            .map_err(|_| Ext4FormatError::OutOfBounds)?;
        let group = self
            .groups
            .get(group_index)
            .ok_or(Ext4FormatError::OutOfBounds)?;
        Ok((group_index, group.block_bitmap_block(), bit))
    }

    fn plan_block_releases(&self, released: &[u64]) -> Result<Vec<PlannedGroupAllocation>> {
        let mut groups = Vec::new();
        for physical_block in released.iter().copied() {
            let (group_index, bitmap_home, bit) = self.block_group_for_physical(physical_block)?;
            let group_pos = groups
                .iter()
                .position(|group: &PlannedGroupAllocation| group.group_index == group_index);
            let group_pos = if let Some(pos) = group_pos {
                pos
            } else {
                let mut bitmap_before = [0u8; BLOCK_SIZE];
                self.read_block(bitmap_home, &mut bitmap_before)?;
                groups.push(PlannedGroupAllocation {
                    group_index,
                    bitmap_home,
                    bitmap_before,
                    bitmap_after: bitmap_before,
                    count: 0,
                });
                groups.len() - 1
            };
            let group = &mut groups[group_pos];
            if group.bitmap_home != bitmap_home {
                return Err(Ext4FormatError::Corrupt);
            }
            if !BitmapView::new(&group.bitmap_after).is_set(bit) {
                return Err(Ext4FormatError::Corrupt);
            }
            BitmapMut::new(&mut group.bitmap_after).clear(bit)?;
            group.count = group
                .count
                .checked_add(1)
                .ok_or(Ext4FormatError::OutOfBounds)?;
        }
        Ok(groups)
    }

    fn group_inode_count(&self, group_index: usize) -> Result<usize> {
        if self.superblock.inodes_per_group == 0 {
            return Err(Ext4FormatError::Corrupt);
        }
        let group_first_index = group_index as u64 * self.superblock.inodes_per_group as u64;
        let total_inodes = self.superblock.inodes_count as u64;
        if group_first_index >= total_inodes {
            return Err(Ext4FormatError::OutOfBounds);
        }
        usize::try_from(core::cmp::min(
            self.superblock.inodes_per_group as u64,
            total_inodes - group_first_index,
        ))
        .map_err(|_| Ext4FormatError::OutOfBounds)
    }

    fn sync_group_itable_unused(
        &self,
        group_index: usize,
        inode_bitmap_after: &Page4K,
        desc_size: usize,
        offset: usize,
        after: &mut Page4K,
    ) -> Result<()> {
        // `bg_itable_unused` is validated together with group metadata
        // checksums. Preserve it on non-metadata-csum filesystems rather than
        // manufacturing an uninitialized-group claim we cannot authenticate.
        if !self.superblock.has_metadata_csum() {
            return Ok(());
        }
        let group_inode_count = self.group_inode_count(group_index)?;
        let bitmap = BitmapView::new(inode_bitmap_after);
        let mut unused = 0usize;
        for bit in (0..group_inode_count).rev() {
            if bitmap.is_set(bit) {
                break;
            }
            unused += 1;
        }
        let unused = u32::try_from(unused).map_err(|_| Ext4FormatError::OutOfBounds)?;
        if desc_size < 64 && unused > u16::MAX as u32 {
            return Err(Ext4FormatError::Corrupt);
        }
        after[offset + 28..offset + 30].copy_from_slice(&(unused as u16).to_le_bytes());
        if desc_size >= 64 {
            after[offset + 60..offset + 62].copy_from_slice(&((unused >> 16) as u16).to_le_bytes());
        }
        Ok(())
    }

    fn new_inode_extra_isize(&self) -> u16 {
        let available = self.superblock.inode_size.saturating_sub(128);
        core::cmp::min(
            core::cmp::max(
                self.superblock.required_extra_isize,
                self.superblock.desired_extra_isize,
            ),
            available,
        )
    }

    fn refresh_dirblock_checksum(
        &self,
        dir_ino: InodeNo,
        disk_inode: &Inode,
        block: &mut Page4K,
    ) -> Result<()> {
        if !self.superblock.has_metadata_csum() {
            return Ok(());
        }
        self.ensure_dirblock_checksum_tail(block)?;
        block[BLOCK_SIZE - 4..BLOCK_SIZE].fill(0);
        let checksum = dirblock_csum32(
            self.superblock.metadata_csum_seed(),
            dir_ino.get(),
            disk_inode.generation,
            &block[..BLOCK_SIZE - 12],
        );
        write_u32_le(block, BLOCK_SIZE - 4, checksum)
    }

    fn ensure_dirblock_checksum_tail(&self, block: &mut Page4K) -> Result<()> {
        const EXT4_FT_DIR_CSUM: u8 = 0xDE;
        let tail = BLOCK_SIZE.checked_sub(12).ok_or(Ext4FormatError::Corrupt)?;
        if block[tail..tail + 4] == [0, 0, 0, 0]
            && read_u16_le(block, tail + 4)? == 12
            && block[tail + 6] == 0
            && block[tail + 7] == EXT4_FT_DIR_CSUM
        {
            return Ok(());
        }

        let mut off = 0usize;
        while off + 8 <= BLOCK_SIZE {
            let rec_len = read_u16_le(block, off + 4)? as usize;
            if rec_len == 0 {
                break;
            }
            if rec_len < 8 || off + rec_len > BLOCK_SIZE {
                return Err(Ext4FormatError::Corrupt);
            }
            if off < tail && off + rec_len == BLOCK_SIZE {
                let name_len = block[off + 6] as usize;
                let used = (8 + name_len + 3) & !3;
                let shortened = rec_len.checked_sub(12).ok_or(Ext4FormatError::Corrupt)?;
                if shortened < used {
                    return Err(Ext4FormatError::Unsupported);
                }
                write_u16_le(block, off + 4, shortened as u16)?;
                break;
            }
            if off == tail {
                break;
            }
            off += rec_len;
        }

        block[tail..].fill(0);
        write_u16_le(block, tail + 4, 12)?;
        block[tail + 7] = EXT4_FT_DIR_CSUM;
        Ok(())
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
        self.read_block(home, &mut before)?;
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

    fn plan_group_free_block_decrements(
        &self,
        allocations: &[PlannedGroupAllocation],
    ) -> Result<Vec<(u64, Page4K, Page4K)>> {
        if let [allocation] = allocations {
            if allocation.count == 1 {
                return Ok(vec![self.plan_group_free_block_decrement(
                    allocation.group_index,
                    &allocation.bitmap_after,
                )?]);
            }
        }
        let desc_size = self.superblock.group_desc_size();
        let gdt_start = if self.superblock.block_size() == 1024 {
            2
        } else {
            1
        };
        let mut updates = Vec::new();
        for allocation in allocations {
            let byte_offset = allocation
                .group_index
                .checked_mul(desc_size)
                .ok_or(Ext4FormatError::OutOfBounds)?;
            let home = gdt_start + (byte_offset / BLOCK_SIZE) as u64;
            let offset = byte_offset % BLOCK_SIZE;
            if offset + desc_size > BLOCK_SIZE {
                return Err(Ext4FormatError::Unsupported);
            }
            let update_pos = updates
                .iter()
                .position(|(existing, _, _)| *existing == home);
            let update_pos = if let Some(pos) = update_pos {
                pos
            } else {
                let mut before = [0u8; BLOCK_SIZE];
                self.read_block(home, &mut before)?;
                updates.push((home, before, before));
                updates.len() - 1
            };
            let after = &mut updates[update_pos].2;
            let descriptor = GroupDesc::parse_sized(&after[offset..offset + desc_size], desc_size)?;
            let free = u32::from(descriptor.free_blocks_count)
                | (u32::from(descriptor.free_blocks_count_hi) << 16);
            let next = free
                .checked_sub(allocation.count)
                .ok_or(Ext4FormatError::OutOfBounds)?;
            if desc_size < 64 && next > u16::MAX as u32 {
                return Err(Ext4FormatError::Corrupt);
            }
            after[offset + 12..offset + 14].copy_from_slice(&(next as u16).to_le_bytes());
            if desc_size >= 64 {
                after[offset + 44..offset + 46]
                    .copy_from_slice(&((next >> 16) as u16).to_le_bytes());
            }
            if self.superblock.has_metadata_csum() {
                let bitmap_checksum = block_bitmap_csum32(
                    self.superblock.metadata_csum_seed(),
                    &allocation.bitmap_after,
                    self.superblock.blocks_per_group,
                )?;
                after[offset + 24..offset + 26]
                    .copy_from_slice(&(bitmap_checksum as u16).to_le_bytes());
                if desc_size >= 64 {
                    after[offset + 56..offset + 58]
                        .copy_from_slice(&((bitmap_checksum >> 16) as u16).to_le_bytes());
                }
                after[offset + 30..offset + 32].fill(0);
                let group_id = u32::try_from(allocation.group_index)
                    .map_err(|_| Ext4FormatError::OutOfBounds)?;
                let checksum = group_desc_csum16(
                    self.superblock.metadata_csum_seed(),
                    group_id,
                    &after[offset..offset + desc_size],
                );
                after[offset + 30..offset + 32].copy_from_slice(&checksum.to_le_bytes());
            }
        }
        Ok(updates)
    }

    fn plan_group_free_block_increments(
        &self,
        releases: &[PlannedGroupAllocation],
    ) -> Result<Vec<(u64, Page4K, Page4K)>> {
        let desc_size = self.superblock.group_desc_size();
        let gdt_start = if self.superblock.block_size() == 1024 {
            2
        } else {
            1
        };
        let mut updates = Vec::new();
        for release in releases {
            let byte_offset = release
                .group_index
                .checked_mul(desc_size)
                .ok_or(Ext4FormatError::OutOfBounds)?;
            let home = gdt_start + (byte_offset / BLOCK_SIZE) as u64;
            let offset = byte_offset % BLOCK_SIZE;
            if offset + desc_size > BLOCK_SIZE {
                return Err(Ext4FormatError::Unsupported);
            }
            let update_pos = updates
                .iter()
                .position(|(existing, _, _)| *existing == home);
            let update_pos = if let Some(pos) = update_pos {
                pos
            } else {
                let mut before = [0u8; BLOCK_SIZE];
                self.read_block(home, &mut before)?;
                updates.push((home, before, before));
                updates.len() - 1
            };
            let after = &mut updates[update_pos].2;
            let descriptor = GroupDesc::parse_sized(&after[offset..offset + desc_size], desc_size)?;
            let free = u32::from(descriptor.free_blocks_count)
                | (u32::from(descriptor.free_blocks_count_hi) << 16);
            let next = free
                .checked_add(release.count)
                .ok_or(Ext4FormatError::OutOfBounds)?;
            if desc_size < 64 && next > u16::MAX as u32 {
                return Err(Ext4FormatError::Corrupt);
            }
            after[offset + 12..offset + 14].copy_from_slice(&(next as u16).to_le_bytes());
            if desc_size >= 64 {
                after[offset + 44..offset + 46]
                    .copy_from_slice(&((next >> 16) as u16).to_le_bytes());
            }
            if self.superblock.has_metadata_csum() {
                let bitmap_checksum = block_bitmap_csum32(
                    self.superblock.metadata_csum_seed(),
                    &release.bitmap_after,
                    self.superblock.blocks_per_group,
                )?;
                after[offset + 24..offset + 26]
                    .copy_from_slice(&(bitmap_checksum as u16).to_le_bytes());
                if desc_size >= 64 {
                    after[offset + 56..offset + 58]
                        .copy_from_slice(&((bitmap_checksum >> 16) as u16).to_le_bytes());
                }
                after[offset + 30..offset + 32].fill(0);
                let group_id =
                    u32::try_from(release.group_index).map_err(|_| Ext4FormatError::OutOfBounds)?;
                let checksum = group_desc_csum16(
                    self.superblock.metadata_csum_seed(),
                    group_id,
                    &after[offset..offset + desc_size],
                );
                after[offset + 30..offset + 32].copy_from_slice(&checksum.to_le_bytes());
            }
        }
        Ok(updates)
    }

    fn plan_group_destroy_count_updates(
        &self,
        releases: &[PlannedGroupAllocation],
        inode_group: usize,
        inode_bitmap_after: &Page4K,
        released_dirs: u32,
    ) -> Result<Vec<(u64, Page4K, Page4K)>> {
        if releases.is_empty() {
            return Ok(vec![self.plan_group_destroy_counts(
                inode_group,
                None,
                inode_bitmap_after,
                0,
                released_dirs,
            )?]);
        }
        let desc_size = self.superblock.group_desc_size();
        let byte_offset = inode_group
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

        let mut updates = self.plan_group_free_block_increments(releases)?;
        let update_pos = updates
            .iter()
            .position(|(existing, _, _)| *existing == home);
        let update_pos = if let Some(pos) = update_pos {
            pos
        } else {
            let mut before = [0u8; BLOCK_SIZE];
            self.read_block(home, &mut before)?;
            updates.push((home, before, before));
            updates.len() - 1
        };
        let after = &mut updates[update_pos].2;
        let descriptor = GroupDesc::parse_sized(&after[offset..offset + desc_size], desc_size)?;
        let free_inodes = u32::from(descriptor.free_inodes_count)
            | (u32::from(descriptor.free_inodes_count_hi) << 16);
        let used_dirs = u32::from(descriptor.used_dirs_count)
            | (u32::from(descriptor.used_dirs_count_hi) << 16);
        let next_free_inodes = free_inodes
            .checked_add(1)
            .ok_or(Ext4FormatError::OutOfBounds)?;
        let next_used_dirs = used_dirs
            .checked_sub(released_dirs)
            .ok_or(Ext4FormatError::OutOfBounds)?;
        if desc_size < 64
            && (next_free_inodes > u16::MAX as u32 || next_used_dirs > u16::MAX as u32)
        {
            return Err(Ext4FormatError::Corrupt);
        }
        after[offset + 14..offset + 16].copy_from_slice(&(next_free_inodes as u16).to_le_bytes());
        after[offset + 16..offset + 18].copy_from_slice(&(next_used_dirs as u16).to_le_bytes());
        if desc_size >= 64 {
            after[offset + 46..offset + 48]
                .copy_from_slice(&((next_free_inodes >> 16) as u16).to_le_bytes());
            after[offset + 48..offset + 50]
                .copy_from_slice(&((next_used_dirs >> 16) as u16).to_le_bytes());
        }
        self.sync_group_itable_unused(inode_group, inode_bitmap_after, desc_size, offset, after)?;
        if self.superblock.has_metadata_csum() {
            let inode_bitmap_checksum = inode_bitmap_csum32(
                self.superblock.metadata_csum_seed(),
                inode_bitmap_after,
                self.superblock.inodes_per_group,
            )?;
            after[offset + 26..offset + 28]
                .copy_from_slice(&(inode_bitmap_checksum as u16).to_le_bytes());
            if desc_size >= 64 {
                after[offset + 58..offset + 60]
                    .copy_from_slice(&((inode_bitmap_checksum >> 16) as u16).to_le_bytes());
            }
            after[offset + 30..offset + 32].fill(0);
            let group_id = u32::try_from(inode_group).map_err(|_| Ext4FormatError::OutOfBounds)?;
            let checksum = group_desc_csum16(
                self.superblock.metadata_csum_seed(),
                group_id,
                &after[offset..offset + desc_size],
            );
            after[offset + 30..offset + 32].copy_from_slice(&checksum.to_le_bytes());
        }
        Ok(updates)
    }

    fn plan_group_free_inode_decrement(
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
        self.read_block(home, &mut before)?;
        let descriptor = GroupDesc::parse_sized(&before[offset..offset + desc_size], desc_size)?;
        let free = u32::from(descriptor.free_inodes_count)
            | (u32::from(descriptor.free_inodes_count_hi) << 16);
        let next = free.checked_sub(1).ok_or(Ext4FormatError::OutOfBounds)?;
        if desc_size < 64 && next > u16::MAX as u32 {
            return Err(Ext4FormatError::Corrupt);
        }
        let mut after = before;
        after[offset + 14..offset + 16].copy_from_slice(&(next as u16).to_le_bytes());
        if desc_size >= 64 {
            after[offset + 46..offset + 48].copy_from_slice(&((next >> 16) as u16).to_le_bytes());
        }
        self.sync_group_itable_unused(group_index, bitmap_after, desc_size, offset, &mut after)?;
        if self.superblock.has_metadata_csum() {
            let bitmap_checksum = inode_bitmap_csum32(
                self.superblock.metadata_csum_seed(),
                bitmap_after,
                self.superblock.inodes_per_group,
            )?;
            after[offset + 26..offset + 28]
                .copy_from_slice(&(bitmap_checksum as u16).to_le_bytes());
            if desc_size >= 64 {
                after[offset + 58..offset + 60]
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

    fn plan_group_destroy_counts(
        &self,
        group_index: usize,
        block_bitmap_after: Option<&Page4K>,
        inode_bitmap_after: &Page4K,
        released_blocks: u32,
        released_dirs: u32,
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
        self.read_block(home, &mut before)?;
        let descriptor = GroupDesc::parse_sized(&before[offset..offset + desc_size], desc_size)?;
        let free_blocks = u32::from(descriptor.free_blocks_count)
            | (u32::from(descriptor.free_blocks_count_hi) << 16);
        let free_inodes = u32::from(descriptor.free_inodes_count)
            | (u32::from(descriptor.free_inodes_count_hi) << 16);
        let used_dirs = u32::from(descriptor.used_dirs_count)
            | (u32::from(descriptor.used_dirs_count_hi) << 16);
        let next_free_blocks = free_blocks
            .checked_add(released_blocks)
            .ok_or(Ext4FormatError::OutOfBounds)?;
        let next_free_inodes = free_inodes
            .checked_add(1)
            .ok_or(Ext4FormatError::OutOfBounds)?;
        let next_used_dirs = used_dirs
            .checked_sub(released_dirs)
            .ok_or(Ext4FormatError::OutOfBounds)?;
        if desc_size < 64
            && (next_free_blocks > u16::MAX as u32
                || next_free_inodes > u16::MAX as u32
                || next_used_dirs > u16::MAX as u32)
        {
            return Err(Ext4FormatError::Corrupt);
        }
        let mut after = before;
        after[offset + 12..offset + 14].copy_from_slice(&(next_free_blocks as u16).to_le_bytes());
        after[offset + 14..offset + 16].copy_from_slice(&(next_free_inodes as u16).to_le_bytes());
        after[offset + 16..offset + 18].copy_from_slice(&(next_used_dirs as u16).to_le_bytes());
        if desc_size >= 64 {
            after[offset + 44..offset + 46]
                .copy_from_slice(&((next_free_blocks >> 16) as u16).to_le_bytes());
            after[offset + 46..offset + 48]
                .copy_from_slice(&((next_free_inodes >> 16) as u16).to_le_bytes());
            after[offset + 48..offset + 50]
                .copy_from_slice(&((next_used_dirs >> 16) as u16).to_le_bytes());
        }
        self.sync_group_itable_unused(
            group_index,
            inode_bitmap_after,
            desc_size,
            offset,
            &mut after,
        )?;
        if self.superblock.has_metadata_csum() {
            if let Some(block_bitmap_after) = block_bitmap_after {
                let block_bitmap_checksum = block_bitmap_csum32(
                    self.superblock.metadata_csum_seed(),
                    block_bitmap_after,
                    self.superblock.blocks_per_group,
                )?;
                after[offset + 24..offset + 26]
                    .copy_from_slice(&(block_bitmap_checksum as u16).to_le_bytes());
                if desc_size >= 64 {
                    after[offset + 56..offset + 58]
                        .copy_from_slice(&((block_bitmap_checksum >> 16) as u16).to_le_bytes());
                }
            }
            let inode_bitmap_checksum = inode_bitmap_csum32(
                self.superblock.metadata_csum_seed(),
                inode_bitmap_after,
                self.superblock.inodes_per_group,
            )?;
            after[offset + 26..offset + 28]
                .copy_from_slice(&(inode_bitmap_checksum as u16).to_le_bytes());
            if desc_size >= 64 {
                after[offset + 58..offset + 60]
                    .copy_from_slice(&((inode_bitmap_checksum >> 16) as u16).to_le_bytes());
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

    fn plan_group_mkdir_count_updates(
        &self,
        inode_group: usize,
        inode_bitmap_after: &Page4K,
        block_group: usize,
        block_bitmap_after: &Page4K,
    ) -> Result<Vec<(u64, Page4K, Page4K)>> {
        let mut updates =
            vec![self.plan_group_free_block_decrement(block_group, block_bitmap_after)?];
        let desc_size = self.superblock.group_desc_size();
        let byte_offset = inode_group
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
        let update_pos = updates
            .iter()
            .position(|(existing, _, _)| *existing == home);
        let update_pos = if let Some(pos) = update_pos {
            pos
        } else {
            let mut before = [0u8; BLOCK_SIZE];
            self.read_block(home, &mut before)?;
            updates.push((home, before, before));
            updates.len() - 1
        };
        let after = &mut updates[update_pos].2;
        let descriptor = GroupDesc::parse_sized(&after[offset..offset + desc_size], desc_size)?;
        let free_inodes = u32::from(descriptor.free_inodes_count)
            | (u32::from(descriptor.free_inodes_count_hi) << 16);
        let used_dirs = u32::from(descriptor.used_dirs_count)
            | (u32::from(descriptor.used_dirs_count_hi) << 16);
        let next_free_inodes = free_inodes
            .checked_sub(1)
            .ok_or(Ext4FormatError::OutOfBounds)?;
        let next_used_dirs = used_dirs
            .checked_add(1)
            .ok_or(Ext4FormatError::OutOfBounds)?;
        if desc_size < 64
            && (next_free_inodes > u16::MAX as u32 || next_used_dirs > u16::MAX as u32)
        {
            return Err(Ext4FormatError::Corrupt);
        }
        after[offset + 14..offset + 16].copy_from_slice(&(next_free_inodes as u16).to_le_bytes());
        after[offset + 16..offset + 18].copy_from_slice(&(next_used_dirs as u16).to_le_bytes());
        if desc_size >= 64 {
            after[offset + 46..offset + 48]
                .copy_from_slice(&((next_free_inodes >> 16) as u16).to_le_bytes());
            after[offset + 48..offset + 50]
                .copy_from_slice(&((next_used_dirs >> 16) as u16).to_le_bytes());
        }
        self.sync_group_itable_unused(inode_group, inode_bitmap_after, desc_size, offset, after)?;
        if self.superblock.has_metadata_csum() {
            let seed = self.superblock.metadata_csum_seed();
            let inode_bitmap_checksum =
                inode_bitmap_csum32(seed, inode_bitmap_after, self.superblock.inodes_per_group)?;
            after[offset + 26..offset + 28]
                .copy_from_slice(&(inode_bitmap_checksum as u16).to_le_bytes());
            if desc_size >= 64 {
                after[offset + 58..offset + 60]
                    .copy_from_slice(&((inode_bitmap_checksum >> 16) as u16).to_le_bytes());
            }
            after[offset + 30..offset + 32].fill(0);
            let group_id = u32::try_from(inode_group).map_err(|_| Ext4FormatError::OutOfBounds)?;
            let checksum = group_desc_csum16(
                self.superblock.metadata_csum_seed(),
                group_id,
                &after[offset..offset + desc_size],
            );
            after[offset + 30..offset + 32].copy_from_slice(&checksum.to_le_bytes());
        }
        Ok(updates)
    }

    fn plan_superblock_free_block_decrement(&self, amount: u32) -> Result<(u64, Page4K, Page4K)> {
        let mut before = [0u8; BLOCK_SIZE];
        self.read_block(0, &mut before)?;
        let observed = Superblock::parse(&before[1024..2048])?;
        let next = observed
            .free_blocks_count
            .checked_sub(u64::from(amount))
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

    fn plan_superblock_mkdir_counts(&self) -> Result<(u64, Page4K, Page4K)> {
        let mut before = [0u8; BLOCK_SIZE];
        self.read_block(0, &mut before)?;
        let observed = Superblock::parse(&before[1024..2048])?;
        let next_free_blocks = observed
            .free_blocks_count
            .checked_sub(1)
            .ok_or(Ext4FormatError::OutOfBounds)?;
        let next_free_inodes = observed
            .free_inodes_count
            .checked_sub(1)
            .ok_or(Ext4FormatError::OutOfBounds)?;
        let mut after = before;
        after[1024 + 12..1024 + 16].copy_from_slice(&(next_free_blocks as u32).to_le_bytes());
        after[1024 + 0x158..1024 + 0x15c]
            .copy_from_slice(&((next_free_blocks >> 32) as u32).to_le_bytes());
        after[1024 + 16..1024 + 20].copy_from_slice(&next_free_inodes.to_le_bytes());
        if observed.has_metadata_csum() {
            let superblock = &mut after[1024..2048];
            superblock[1020..1024].fill(0);
            let checksum = superblock_csum32(superblock)?;
            superblock[1020..1024].copy_from_slice(&checksum.to_le_bytes());
        }
        Ok((0, before, after))
    }

    fn plan_superblock_free_inode_decrement(&self) -> Result<(u64, Page4K, Page4K)> {
        let mut before = [0u8; BLOCK_SIZE];
        self.read_block(0, &mut before)?;
        let observed = Superblock::parse(&before[1024..2048])?;
        let next = observed
            .free_inodes_count
            .checked_sub(1)
            .ok_or(Ext4FormatError::OutOfBounds)?;
        let mut after = before;
        after[1024 + 16..1024 + 20].copy_from_slice(&next.to_le_bytes());
        if observed.has_metadata_csum() {
            let superblock = &mut after[1024..2048];
            superblock[1020..1024].fill(0);
            let checksum = superblock_csum32(superblock)?;
            superblock[1020..1024].copy_from_slice(&checksum.to_le_bytes());
        }
        Ok((0, before, after))
    }

    fn plan_superblock_free_block_increment(&self, released: u64) -> Result<(u64, Page4K, Page4K)> {
        let mut before = [0u8; BLOCK_SIZE];
        self.read_block(0, &mut before)?;
        let observed = Superblock::parse(&before[1024..2048])?;
        let next = observed
            .free_blocks_count
            .checked_add(released)
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

    fn plan_superblock_orphan_head(
        &self,
        orphan_inode: InodeNo,
    ) -> Result<(u64, Page4K, Page4K, u32)> {
        let mut before = [0u8; BLOCK_SIZE];
        self.read_block(0, &mut before)?;
        let observed = Superblock::parse(&before[1024..2048])?;
        let previous = observed.last_orphan;
        if previous == orphan_inode.get() {
            return Err(Ext4FormatError::Corrupt);
        }
        let mut after = before;
        after[1024 + 232..1024 + 236].copy_from_slice(&orphan_inode.get().to_le_bytes());
        if observed.has_metadata_csum() {
            let superblock = &mut after[1024..2048];
            superblock[1020..1024].fill(0);
            let checksum = superblock_csum32(superblock)?;
            superblock[1020..1024].copy_from_slice(&checksum.to_le_bytes());
        }
        Ok((0, before, after, previous))
    }

    fn plan_superblock_destroy_counts(
        &self,
        released_blocks: u64,
        orphan_next: Option<(InodeNo, u32)>,
    ) -> Result<(u64, Page4K, Page4K)> {
        let mut before = [0u8; BLOCK_SIZE];
        self.read_block(0, &mut before)?;
        let observed = Superblock::parse(&before[1024..2048])?;
        let mut next_last_orphan = observed.last_orphan;
        if let Some((orphan_inode, next)) = orphan_next {
            if observed.last_orphan == orphan_inode.get() {
                next_last_orphan = next;
            } else if observed.last_orphan != 0 {
                return Err(Ext4FormatError::Unsupported);
            }
        }
        let next_free_blocks = observed
            .free_blocks_count
            .checked_add(released_blocks)
            .ok_or(Ext4FormatError::OutOfBounds)?;
        let next_free_inodes = observed
            .free_inodes_count
            .checked_add(1)
            .ok_or(Ext4FormatError::OutOfBounds)?;
        let mut after = before;
        after[1024 + 12..1024 + 16].copy_from_slice(&(next_free_blocks as u32).to_le_bytes());
        after[1024 + 0x158..1024 + 0x15c]
            .copy_from_slice(&((next_free_blocks >> 32) as u32).to_le_bytes());
        after[1024 + 16..1024 + 20].copy_from_slice(&next_free_inodes.to_le_bytes());
        after[1024 + 232..1024 + 236].copy_from_slice(&next_last_orphan.to_le_bytes());
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
                self.read_block(child, &mut block)?;
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
        // Walk the directory exactly once. The old implementation requested
        // eight entries at a time through `read_dir_entries_from`, whose
        // `skip_entries` contract restarted at byte zero for every batch.
        // Looking up a late entry in a large Cargo `deps` directory therefore
        // reread prefixes 0, 8, 16, ... and was quadratic even when all ext4
        // blocks were already cached.
        let disk_inode = self.read_inode(directory)?;
        if !disk_inode.is_dir() {
            return Err(Ext4FormatError::Unsupported);
        }

        let page_count = div_ceil_u64(disk_inode.size, BLOCK_SIZE as u64);
        for page_index in 0..page_count {
            let mut page = [0u8; BLOCK_SIZE];
            match self.resolve_inode_block(&disk_inode, logical_block(page_index)?)? {
                BlockMapping::Data(block) => self.read_block(block, &mut page)?,
                BlockMapping::Hole => continue,
                BlockMapping::NeedNode(_) => return Err(Ext4FormatError::Unsupported),
            }
            for entry in DirEntryIter::new(&page) {
                let entry = entry?;
                if entry.name == name {
                    return Ok(Some(InodeNo::new(entry.inode)));
                }
            }
        }
        Ok(None)
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
                BlockMapping::Data(block) => self.read_block(block, &mut page)?,
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
        self.read_block(location.block, &mut home_block)?;
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
                    self.read_block(block, &mut page)?;
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
            self.read_block(cursor, &mut descriptor)?;
            let (header, tag) = match parse_journal_descriptor(&descriptor) {
                Ok((header, tag)) => (header, tag),
                _ => break,
            };
            let mut commit = [0u8; BLOCK_SIZE];
            self.read_block(cursor + 2, &mut commit)?;
            let commit = CommitHeader::parse(&commit)?;
            if commit.sequence != header.sequence {
                break;
            }
            let mut payload = [0u8; BLOCK_SIZE];
            self.read_block(cursor + 1, &mut payload)?;
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
            self.read_block(bitmap_block, &mut bitmap)?;
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
        self.read_block(loc.block, &mut block)?;
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
            self.read_block(bitmap_block, &mut bitmap)?;
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
        let plan = self.plan_append_dir_entry_mutation(
            dir_ino,
            name,
            new_ino,
            file_type,
            MutationOrigin::Create,
            new_ino.get() as u64,
            FsyncStamp::new(0),
        )?;
        self.apply_mutation_direct(&plan)
    }

    fn plan_append_dir_entry_after_image(
        &mut self,
        dir_ino: InodeNo,
        name: &[u8],
        new_ino: InodeNo,
        file_type: u8,
    ) -> Result<(u64, Page4K, Page4K)> {
        if name.len() > 255 {
            return Err(Ext4FormatError::OutOfBounds);
        }
        let new_min = (8usize + name.len() + 3) & !3;
        let disk_inode = self.read_inode(dir_ino)?;
        if !disk_inode.is_dir() || disk_inode.is_htree_indexed() {
            return Err(Ext4FormatError::Unsupported);
        }
        let page_count = div_ceil_u64(disk_inode.size, BLOCK_SIZE as u64);

        for page_index in 0..page_count {
            let phys = match self.resolve_inode_block(&disk_inode, logical_block(page_index)?)? {
                BlockMapping::Data(b) => b,
                BlockMapping::Hole => continue,
                BlockMapping::NeedNode(_) => return Err(Ext4FormatError::Unsupported),
            };
            let mut before = [0u8; BLOCK_SIZE];
            self.read_block(phys, &mut before)?;
            let mut after = before;

            let mut off = 0usize;
            while off + 8 <= BLOCK_SIZE {
                let rec_len = read_u16_le(&after, off + 4)? as usize;
                if rec_len == 0 || off + rec_len > BLOCK_SIZE {
                    break;
                }
                let ino_here = u32::from_le_bytes(after[off..off + 4].try_into().unwrap());
                if ino_here == 0 {
                    if rec_len >= new_min {
                        encode_dir_entry(
                            new_ino.get(),
                            rec_len as u16,
                            file_type,
                            name,
                            &mut after[off..off + rec_len],
                        )?;
                        self.refresh_dirblock_checksum(dir_ino, &disk_inode, &mut after)?;
                        return Ok((phys, before, after));
                    }
                } else {
                    let name_len = after[off + 6] as usize;
                    let name_end = off + 8 + name_len;
                    if name_end <= BLOCK_SIZE
                        && name_len == name.len()
                        && &after[off + 8..name_end] == name
                    {
                        return Err(Ext4FormatError::Unsupported);
                    }
                    let used = (8 + name_len + 3) & !3;
                    let free = rec_len.saturating_sub(used);
                    if free >= new_min {
                        write_u16_le(&mut after, off + 4, used as u16)?;
                        encode_dir_entry(
                            new_ino.get(),
                            free as u16,
                            file_type,
                            name,
                            &mut after[off + used..off + rec_len],
                        )?;
                        self.refresh_dirblock_checksum(dir_ino, &disk_inode, &mut after)?;
                        return Ok((phys, before, after));
                    }
                }
                off += rec_len;
            }
        }
        Err(Ext4FormatError::OutOfBounds)
    }

    /// Plan a directory entry publication, growing the parent directory when
    /// every existing block is full.  The growth is expressed through the
    /// normal write planner so the new extent, parent inode, allocation
    /// bitmap, group descriptors, superblock counters, and directory block
    /// become one immutable mutation.
    fn plan_append_dir_entry_mutation(
        &mut self,
        dir_ino: InodeNo,
        name: &[u8],
        new_ino: InodeNo,
        file_type: u8,
        origin: MutationOrigin,
        object: u64,
        fsync_stamp: FsyncStamp,
    ) -> Result<Ext4MutationPlan> {
        match self.plan_append_dir_entry_after_image(dir_ino, name, new_ino, file_type) {
            Ok((home, before, after)) => {
                let mut plan = Ext4MutationPlan::new(origin, object, fsync_stamp);
                plan.push_metadata(MetadataBlock {
                    home,
                    role: MetaRole::DirectoryBlock,
                    before_version: crc32c(0, &before) as u64,
                    after,
                    depends_on: Vec::new(),
                })
                .map_err(|_| Ext4FormatError::Corrupt)?;
                Ok(plan)
            }
            Err(Ext4FormatError::OutOfBounds) => {
                if name.len() > 255 {
                    return Err(Ext4FormatError::OutOfBounds);
                }
                let disk_inode = self.read_inode(dir_ino)?;
                if !disk_inode.is_dir() || disk_inode.is_htree_indexed() {
                    return Err(Ext4FormatError::Unsupported);
                }
                if disk_inode.size % BLOCK_SIZE as u64 != 0 {
                    return Err(Ext4FormatError::Corrupt);
                }
                let page_index = div_ceil_u64(disk_inode.size, BLOCK_SIZE as u64);
                let mut page = [0u8; BLOCK_SIZE];
                encode_dir_entry(new_ino.get(), BLOCK_SIZE as u16, file_type, name, &mut page)?;
                self.refresh_dirblock_checksum(dir_ino, &disk_inode, &mut page)?;

                let mut plan = self.plan_write_page(dir_ino, page_index, &page, fsync_stamp)?;
                if plan.data.len() != 1
                    || plan.data[0].logical_page != page_index
                    || plan.data[0].bytes != page
                {
                    return Err(Ext4FormatError::Corrupt);
                }
                let write = plan.data.pop().ok_or(Ext4FormatError::Corrupt)?;
                if !plan
                    .allocations
                    .iter()
                    .any(|claim| claim.physical_block == write.physical_block)
                {
                    return Err(Ext4FormatError::Corrupt);
                }
                let mut before = [0u8; BLOCK_SIZE];
                self.read_block(write.physical_block, &mut before)?;
                plan.push_metadata(MetadataBlock {
                    home: write.physical_block,
                    role: MetaRole::DirectoryBlock,
                    before_version: crc32c(0, &before) as u64,
                    after: write.bytes,
                    depends_on: Vec::new(),
                })
                .map_err(|_| Ext4FormatError::Corrupt)?;
                plan.origin = origin;
                plan.object = object;
                Ok(plan)
            }
            Err(error) => Err(error),
        }
    }

    /// Build the hard-link mutation for an existing regular file, including
    /// parent-directory growth when its current blocks have no dirent slack.
    /// Directory hard links remain unsupported.
    pub fn plan_link_dir_entry(
        &mut self,
        dir_ino: InodeNo,
        name: &[u8],
        target_ino: InodeNo,
        fsync_stamp: FsyncStamp,
    ) -> Result<Ext4MutationPlan> {
        let pending_before = self.pending_metadata.clone();
        let planned = self.plan_link_dir_entry_inner(dir_ino, name, target_ino, fsync_stamp);
        self.pending_metadata = pending_before;
        planned
    }

    fn plan_link_dir_entry_inner(
        &mut self,
        dir_ino: InodeNo,
        name: &[u8],
        target_ino: InodeNo,
        fsync_stamp: FsyncStamp,
    ) -> Result<Ext4MutationPlan> {
        let mut plan = self.plan_append_dir_entry_mutation(
            dir_ino,
            name,
            target_ino,
            1,
            MutationOrigin::Link,
            target_ino.get() as u64,
            fsync_stamp,
        )?;
        self.stage_mutation_after_images(&plan);

        let location = self.inode_location(target_ino)?;
        let mut inode_table_before = [0u8; BLOCK_SIZE];
        self.read_block(location.block, &mut inode_table_before)?;
        let mut inode_table_after = inode_table_before;
        let inode_bytes = &mut inode_table_after[location.offset..location.offset + location.len];
        let mut disk_inode = Inode::parse(inode_bytes)?;
        if !disk_inode.is_file() {
            return Err(Ext4FormatError::Unsupported);
        }
        disk_inode.links_count = disk_inode
            .links_count
            .checked_add(1)
            .ok_or(Ext4FormatError::OutOfBounds)?;
        disk_inode.ctime = fsync_stamp
            .raw()
            .try_into()
            .map_err(|_| Ext4FormatError::OutOfBounds)?;
        disk_inode.encode_preserving_unknown(inode_bytes)?;
        self.refresh_inode_checksum(target_ino, &disk_inode, inode_bytes)?;

        let mut link =
            Ext4MutationPlan::new(MutationOrigin::Link, target_ino.get() as u64, fsync_stamp);
        link.push_metadata(MetadataBlock {
            home: location.block,
            role: MetaRole::InodeTable,
            before_version: crc32c(0, &inode_table_before) as u64,
            after: inode_table_after,
            depends_on: Vec::new(),
        })
        .map_err(|_| Ext4FormatError::Corrupt)?;
        merge_chained_mutation(&mut plan, &link)?;
        Ok(plan)
    }

    /// Build the bounded same-directory rename mutation.
    ///
    /// This Tier 1 slice rewrites the existing dirent in place, so it supports
    /// names that fit in the original record. Cross-directory moves,
    /// overwrite and cross-directory directory moves stay fail-closed until
    /// their complete nlink/`..`/orphan plans exist. A directory rename whose
    /// parent does not change is safe here: its `..` entry and both parent
    /// link counts remain unchanged, and the dirent keeps its original type.
    pub fn plan_rename_dir_entry(
        &mut self,
        dir_ino: InodeNo,
        old_name: &[u8],
        new_name: &[u8],
        target_ino: InodeNo,
        fsync_stamp: FsyncStamp,
    ) -> Result<Ext4MutationPlan> {
        if old_name.is_empty() || new_name.is_empty() || new_name.len() > 255 {
            return Err(Ext4FormatError::OutOfBounds);
        }
        let target_inode = self.read_inode(target_ino)?;
        let file_type = if target_inode.is_dir() {
            2
        } else if target_inode.is_file() {
            1
        } else {
            return Err(Ext4FormatError::Unsupported);
        };

        // Rename through a chained directory snapshot rather than requiring
        // the destination name to fit inside the source record. Cargo and
        // rustc routinely publish linker outputs by renaming a short random
        // temporary name to a longer final artifact name. The removal creates
        // slack first; append then observes that exact after-image through the
        // pager's temporary metadata overlay. Home blocks remain untouched
        // until the complete mutation is admitted.
        let pending_before = self.pending_metadata.clone();
        let planned = (|| {
            let (found_ino, old_home, old_before, old_after) =
                self.plan_remove_dir_entry_after_image(dir_ino, old_name)?;
            if found_ino != target_ino {
                return Err(Ext4FormatError::Corrupt);
            }
            self.pending_metadata.insert(old_home, old_after);
            let (new_home, new_before, new_after) =
                self.plan_append_dir_entry_after_image(dir_ino, new_name, target_ino, file_type)?;

            let mut plan =
                Ext4MutationPlan::new(MutationOrigin::Rename, target_ino.get() as u64, fsync_stamp);
            if old_home == new_home {
                if crc32c(0, &old_after) as u64 != crc32c(0, &new_before) as u64 {
                    return Err(Ext4FormatError::Corrupt);
                }
                plan.push_metadata(MetadataBlock {
                    home: old_home,
                    role: MetaRole::DirectoryBlock,
                    before_version: crc32c(0, &old_before) as u64,
                    after: new_after,
                    depends_on: Vec::new(),
                })
                .map_err(|_| Ext4FormatError::Corrupt)?;
            } else {
                plan.push_metadata(MetadataBlock {
                    home: old_home,
                    role: MetaRole::DirectoryBlock,
                    before_version: crc32c(0, &old_before) as u64,
                    after: old_after,
                    depends_on: Vec::new(),
                })
                .map_err(|_| Ext4FormatError::Corrupt)?;
                plan.push_metadata(MetadataBlock {
                    home: new_home,
                    role: MetaRole::DirectoryBlock,
                    before_version: crc32c(0, &new_before) as u64,
                    after: new_after,
                    depends_on: Vec::new(),
                })
                .map_err(|_| Ext4FormatError::Corrupt)?;
            }
            Ok(plan)
        })();
        self.pending_metadata = pending_before;
        planned
    }

    /// Build the bounded cross-directory regular-file rename mutation.
    ///
    /// This Tier 1 slice supports only absent destinations and directories with
    /// existing slack space. Directory moves and overwrite across parents stay
    /// fail-closed until their nlink, `..`, and orphan lifecycles are admitted.
    pub fn plan_cross_dir_rename_dir_entry(
        &mut self,
        old_dir_ino: InodeNo,
        old_name: &[u8],
        new_dir_ino: InodeNo,
        new_name: &[u8],
        target_ino: InodeNo,
        fsync_stamp: FsyncStamp,
    ) -> Result<Ext4MutationPlan> {
        if old_dir_ino == new_dir_ino {
            return Err(Ext4FormatError::Unsupported);
        }

        let target_inode = self.read_inode(target_ino)?;
        if !target_inode.is_file() {
            return Err(Ext4FormatError::Unsupported);
        }

        let mut plan =
            Ext4MutationPlan::new(MutationOrigin::Rename, target_ino.get() as u64, fsync_stamp);
        let (found_ino, old_home, old_before, old_after) =
            self.plan_remove_dir_entry_after_image(old_dir_ino, old_name)?;
        if found_ino != target_ino {
            return Err(Ext4FormatError::Corrupt);
        }
        plan.push_metadata(MetadataBlock {
            home: old_home,
            role: MetaRole::DirectoryBlock,
            before_version: crc32c(0, &old_before) as u64,
            after: old_after,
            depends_on: Vec::new(),
        })
        .map_err(|_| Ext4FormatError::Corrupt)?;

        // The destination of rustc's archive publication is the shared
        // `target/*/deps` directory. It quickly exhausts the slack in its
        // existing directory blocks, while the source `tmp.a` lives in a
        // fresh temporary subdirectory. The bounded append helper alone can
        // only use existing slack and incorrectly surfaced this ordinary
        // growth case as `OutOfBounds`/ENOENT. Build the destination through
        // the full append planner so a new directory block, its extent and
        // allocation metadata are part of the same atomic rename mutation.
        let append = self.plan_append_dir_entry_mutation(
            new_dir_ino,
            new_name,
            target_ino,
            1,
            MutationOrigin::Rename,
            target_ino.get() as u64,
            fsync_stamp,
        )?;
        merge_chained_mutation(&mut plan, &append)?;
        Ok(plan)
    }

    /// Build the bounded same-directory regular-file rename-overwrite
    /// mutation. The overwritten inode's storage is deliberately not freed;
    /// later orphan/destroy owns that lifecycle.
    pub fn plan_rename_overwrite_dir_entry(
        &mut self,
        dir_ino: InodeNo,
        old_name: &[u8],
        new_name: &[u8],
        old_ino: InodeNo,
        overwritten_ino: InodeNo,
        fsync_stamp: FsyncStamp,
    ) -> Result<Ext4MutationPlan> {
        if old_ino == overwritten_ino {
            return Err(Ext4FormatError::Unsupported);
        }

        // The source and destination names need not live in the same directory
        // block.  Large Cargo incremental directories routinely keep the old
        // final artifact in an early block and append the new `.part` file to
        // the last block.  Build both edits through a temporary overlay so a
        // same-block pair still collapses to one metadata after-image while a
        // cross-block pair is committed atomically as two metadata homes.
        let pending_before = self.pending_metadata.clone();
        let planned_dir = (|| {
            let (found_ino, old_home, old_before, old_after) =
                self.plan_remove_dir_entry_after_image(dir_ino, old_name)?;
            if found_ino != old_ino {
                return Err(Ext4FormatError::Corrupt);
            }
            self.pending_metadata.insert(old_home, old_after);
            let (new_home, new_before, new_after) = self.plan_replace_dir_entry_inode_after_image(
                dir_ino,
                new_name,
                overwritten_ino,
                old_ino,
            )?;
            Ok((
                old_home, old_before, old_after, new_home, new_before, new_after,
            ))
        })();
        self.pending_metadata = pending_before;
        let (old_home, old_before, old_after, new_home, new_before, new_after) = planned_dir?;

        let old_inode = self.read_inode(old_ino)?;
        if !old_inode.is_file() {
            return Err(Ext4FormatError::Unsupported);
        }

        let location = self.inode_location(overwritten_ino)?;
        let mut inode_table_before = [0u8; BLOCK_SIZE];
        self.read_block(location.block, &mut inode_table_before)?;
        let mut inode_table_after = inode_table_before;
        let inode_bytes = &mut inode_table_after[location.offset..location.offset + location.len];
        let mut overwritten_inode = Inode::parse(inode_bytes)?;
        if !overwritten_inode.is_file() {
            return Err(Ext4FormatError::Unsupported);
        }
        overwritten_inode.links_count = overwritten_inode
            .links_count
            .checked_sub(1)
            .ok_or(Ext4FormatError::Corrupt)?;
        overwritten_inode.ctime = fsync_stamp
            .raw()
            .try_into()
            .map_err(|_| Ext4FormatError::OutOfBounds)?;
        overwritten_inode.encode_preserving_unknown(inode_bytes)?;
        self.refresh_inode_checksum(overwritten_ino, &overwritten_inode, inode_bytes)?;

        let mut plan =
            Ext4MutationPlan::new(MutationOrigin::Rename, old_ino.get() as u64, fsync_stamp);
        if old_home == new_home {
            if crc32c(0, &old_after) as u64 != crc32c(0, &new_before) as u64 {
                return Err(Ext4FormatError::Corrupt);
            }
            plan.push_metadata(MetadataBlock {
                home: old_home,
                role: MetaRole::DirectoryBlock,
                before_version: crc32c(0, &old_before) as u64,
                after: new_after,
                depends_on: Vec::new(),
            })
            .map_err(|_| Ext4FormatError::Corrupt)?;
        } else {
            plan.push_metadata(MetadataBlock {
                home: old_home,
                role: MetaRole::DirectoryBlock,
                before_version: crc32c(0, &old_before) as u64,
                after: old_after,
                depends_on: Vec::new(),
            })
            .map_err(|_| Ext4FormatError::Corrupt)?;
            plan.push_metadata(MetadataBlock {
                home: new_home,
                role: MetaRole::DirectoryBlock,
                before_version: crc32c(0, &new_before) as u64,
                after: new_after,
                depends_on: Vec::new(),
            })
            .map_err(|_| Ext4FormatError::Corrupt)?;
        }
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

    fn plan_replace_dir_entry_inode_after_image(
        &mut self,
        dir_ino: InodeNo,
        name: &[u8],
        expected_ino: InodeNo,
        replacement_ino: InodeNo,
    ) -> Result<(u64, Page4K, Page4K)> {
        let disk_inode = self.read_inode(dir_ino)?;
        if !disk_inode.is_dir() || disk_inode.is_htree_indexed() {
            return Err(Ext4FormatError::Unsupported);
        }
        let page_count = div_ceil_u64(disk_inode.size, BLOCK_SIZE as u64);

        for page_index in 0..page_count {
            let phys = match self.resolve_inode_block(&disk_inode, logical_block(page_index)?)? {
                BlockMapping::Data(block) => block,
                BlockMapping::Hole => continue,
                BlockMapping::NeedNode(_) => return Err(Ext4FormatError::Unsupported),
            };
            let mut before = [0u8; BLOCK_SIZE];
            self.read_block(phys, &mut before)?;
            let mut after = before;

            let mut off = 0usize;
            while off + 8 <= BLOCK_SIZE {
                let rec_len = read_u16_le(&after, off + 4)? as usize;
                if rec_len == 0 || off + rec_len > BLOCK_SIZE {
                    break;
                }
                let ino_here = u32::from_le_bytes(after[off..off + 4].try_into().unwrap());
                if ino_here != 0 {
                    let name_len = after[off + 6] as usize;
                    let name_end = off + 8 + name_len;
                    if name_end > BLOCK_SIZE {
                        return Err(Ext4FormatError::Corrupt);
                    }
                    if name_len == name.len() && &after[off + 8..name_end] == name {
                        if InodeNo::new(ino_here) != expected_ino {
                            return Err(Ext4FormatError::Corrupt);
                        }
                        if after[off + 7] == 2 {
                            return Err(Ext4FormatError::Unsupported);
                        }
                        after[off..off + 4].copy_from_slice(&replacement_ino.get().to_le_bytes());
                        self.refresh_dirblock_checksum(dir_ino, &disk_inode, &mut after)?;
                        return Ok((phys, before, after));
                    }
                }
                off += rec_len;
            }
        }

        Err(Ext4FormatError::OutOfBounds)
    }

    /// Build the bounded namespace mutation for unlinking one directory
    /// entry. The plan removes the dirent and decrements the target inode's
    /// link count. When the link count reaches zero, it also links the inode
    /// into the classic superblock orphan chain; data and inode storage are
    /// still owned by the later destroy lifecycle once open-file pins drain.
    pub fn plan_unlink_dir_entry(
        &mut self,
        dir_ino: InodeNo,
        name: &[u8],
        target_ino: InodeNo,
        fsync_stamp: FsyncStamp,
    ) -> Result<Ext4MutationPlan> {
        let (found_ino, dir_home, dir_before, dir_after) =
            self.plan_remove_dir_entry_after_image(dir_ino, name)?;
        if found_ino != target_ino {
            return Err(Ext4FormatError::Corrupt);
        }

        let location = self.inode_location(target_ino)?;
        let mut inode_table_before = [0u8; BLOCK_SIZE];
        self.read_block(location.block, &mut inode_table_before)?;
        let mut inode_table_after = inode_table_before;
        let inode_bytes = &mut inode_table_after[location.offset..location.offset + location.len];
        let mut disk_inode = Inode::parse(inode_bytes)?;
        if disk_inode.is_dir() {
            return Err(Ext4FormatError::Unsupported);
        }
        disk_inode.links_count = disk_inode
            .links_count
            .checked_sub(1)
            .ok_or(Ext4FormatError::Corrupt)?;
        disk_inode.ctime = fsync_stamp
            .raw()
            .try_into()
            .map_err(|_| Ext4FormatError::OutOfBounds)?;
        let orphan_superblock_update = if disk_inode.links_count == 0 {
            let (home, before, after, previous_last_orphan) =
                self.plan_superblock_orphan_head(target_ino)?;
            disk_inode.dtime = previous_last_orphan;
            Some((home, before, after))
        } else {
            None
        };
        disk_inode.encode_preserving_unknown(inode_bytes)?;
        self.refresh_inode_checksum(target_ino, &disk_inode, inode_bytes)?;

        let mut plan =
            Ext4MutationPlan::new(MutationOrigin::Unlink, target_ino.get() as u64, fsync_stamp);
        plan.push_metadata(MetadataBlock {
            home: dir_home,
            role: MetaRole::DirectoryBlock,
            before_version: crc32c(0, &dir_before) as u64,
            after: dir_after,
            depends_on: Vec::new(),
        })
        .map_err(|_| Ext4FormatError::Corrupt)?;
        plan.push_metadata(MetadataBlock {
            home: location.block,
            role: MetaRole::InodeTable,
            before_version: crc32c(0, &inode_table_before) as u64,
            after: inode_table_after,
            depends_on: Vec::new(),
        })
        .map_err(|_| Ext4FormatError::Corrupt)?;
        if let Some((home, before, after)) = orphan_superblock_update {
            plan.push_metadata(MetadataBlock {
                home,
                role: MetaRole::Superblock,
                before_version: crc32c(0, &before) as u64,
                after,
                depends_on: Vec::new(),
            })
            .map_err(|_| Ext4FormatError::Corrupt)?;
        }
        Ok(plan)
    }

    /// Build the bounded namespace mutation for removing an empty directory.
    /// The plan removes the parent dirent and adjusts link counts, but leaves
    /// inode/data block reclamation to the later orphan/destroy lifecycle.
    pub fn plan_rmdir_dir_entry(
        &mut self,
        dir_ino: InodeNo,
        name: &[u8],
        target_ino: InodeNo,
        fsync_stamp: FsyncStamp,
    ) -> Result<Ext4MutationPlan> {
        let (found_ino, dir_home, dir_before, dir_after) =
            self.plan_remove_dir_entry_after_image(dir_ino, name)?;
        if found_ino != target_ino {
            return Err(Ext4FormatError::Corrupt);
        }

        let parent_location = self.inode_location(dir_ino)?;
        let target_location = self.inode_location(target_ino)?;
        let mut parent_table_before = [0u8; BLOCK_SIZE];
        self.read_block(parent_location.block, &mut parent_table_before)?;
        let mut parent_table_after = parent_table_before;
        let mut target_table_before = parent_table_before;
        let mut target_table_after = parent_table_after;
        if target_location.block != parent_location.block {
            self.read_block(target_location.block, &mut target_table_before)?;
            target_table_after = target_table_before;
        }

        {
            let target_inode_bytes = &mut target_table_after
                [target_location.offset..target_location.offset + target_location.len];
            let mut target_inode = Inode::parse(target_inode_bytes)?;
            if !target_inode.is_dir() {
                return Err(Ext4FormatError::Unsupported);
            }
            self.require_empty_directory(target_ino, &target_inode, dir_ino)?;
            target_inode.links_count = 0;
            target_inode.ctime = fsync_stamp
                .raw()
                .try_into()
                .map_err(|_| Ext4FormatError::OutOfBounds)?;
            target_inode.encode_preserving_unknown(target_inode_bytes)?;
            self.refresh_inode_checksum(target_ino, &target_inode, target_inode_bytes)?;
        }

        if target_location.block == parent_location.block {
            parent_table_after = target_table_after;
        }
        {
            let parent_inode_bytes = &mut parent_table_after
                [parent_location.offset..parent_location.offset + parent_location.len];
            let mut parent_inode = Inode::parse(parent_inode_bytes)?;
            if !parent_inode.is_dir() {
                return Err(Ext4FormatError::Unsupported);
            }
            parent_inode.links_count = parent_inode
                .links_count
                .checked_sub(1)
                .ok_or(Ext4FormatError::Corrupt)?;
            parent_inode.ctime = fsync_stamp
                .raw()
                .try_into()
                .map_err(|_| Ext4FormatError::OutOfBounds)?;
            parent_inode.encode_preserving_unknown(parent_inode_bytes)?;
            self.refresh_inode_checksum(dir_ino, &parent_inode, parent_inode_bytes)?;
        }

        let mut plan =
            Ext4MutationPlan::new(MutationOrigin::Unlink, target_ino.get() as u64, fsync_stamp);
        plan.push_metadata(MetadataBlock {
            home: dir_home,
            role: MetaRole::DirectoryBlock,
            before_version: crc32c(0, &dir_before) as u64,
            after: dir_after,
            depends_on: Vec::new(),
        })
        .map_err(|_| Ext4FormatError::Corrupt)?;
        plan.push_metadata(MetadataBlock {
            home: parent_location.block,
            role: MetaRole::InodeTable,
            before_version: crc32c(0, &parent_table_before) as u64,
            after: parent_table_after,
            depends_on: Vec::new(),
        })
        .map_err(|_| Ext4FormatError::Corrupt)?;
        if target_location.block != parent_location.block {
            plan.push_metadata(MetadataBlock {
                home: target_location.block,
                role: MetaRole::InodeTable,
                before_version: crc32c(0, &target_table_before) as u64,
                after: target_table_after,
                depends_on: Vec::new(),
            })
            .map_err(|_| Ext4FormatError::Corrupt)?;
        }
        Ok(plan)
    }

    fn require_empty_directory(
        &mut self,
        dir_ino: InodeNo,
        disk_inode: &Inode,
        parent_ino: InodeNo,
    ) -> Result<()> {
        if !disk_inode.is_dir() || disk_inode.is_htree_indexed() {
            return Err(Ext4FormatError::Unsupported);
        }
        let mut seen_dot = false;
        let mut seen_dotdot = false;
        let page_count = div_ceil_u64(disk_inode.size, BLOCK_SIZE as u64);
        for page_index in 0..page_count {
            let phys = match self.resolve_inode_block(disk_inode, logical_block(page_index)?)? {
                BlockMapping::Data(block) => block,
                BlockMapping::Hole => continue,
                BlockMapping::NeedNode(_) => return Err(Ext4FormatError::Unsupported),
            };
            let mut page = [0u8; BLOCK_SIZE];
            self.read_block(phys, &mut page)?;
            for entry in DirEntryIter::new(&page) {
                let entry = entry?;
                match entry.name {
                    b"." if entry.inode == dir_ino.get() => seen_dot = true,
                    b".." if entry.inode == parent_ino.get() => seen_dotdot = true,
                    _ => return Err(Ext4FormatError::Unsupported),
                }
            }
        }
        if seen_dot && seen_dotdot {
            Ok(())
        } else {
            Err(Ext4FormatError::Corrupt)
        }
    }

    fn plan_remove_dir_entry_after_image(
        &mut self,
        dir_ino: InodeNo,
        name: &[u8],
    ) -> Result<(InodeNo, u64, Page4K, Page4K)> {
        let disk_inode = self.read_inode(dir_ino)?;
        let page_count = div_ceil_u64(disk_inode.size, BLOCK_SIZE as u64);

        for page_index in 0..page_count {
            let phys = match self.resolve_inode_block(&disk_inode, logical_block(page_index)?)? {
                BlockMapping::Data(b) => b,
                BlockMapping::Hole => continue,
                BlockMapping::NeedNode(_) => return Err(Ext4FormatError::Unsupported),
            };
            let mut before = [0u8; BLOCK_SIZE];
            self.read_block(phys, &mut before)?;
            let mut after = before;

            let mut prev_off: Option<usize> = None;
            let mut off = 0usize;
            while off + 8 <= BLOCK_SIZE {
                let rec_len = read_u16_le(&after, off + 4)? as usize;
                if rec_len == 0 || off + rec_len > BLOCK_SIZE {
                    break;
                }
                let ino_here = u32::from_le_bytes(after[off..off + 4].try_into().unwrap());
                if ino_here != 0 {
                    let name_len = after[off + 6] as usize;
                    let name_end = off + 8 + name_len;
                    if name_end <= BLOCK_SIZE
                        && name_len == name.len()
                        && &after[off + 8..name_end] == name
                    {
                        let found_ino = InodeNo::new(ino_here);
                        if let Some(prev) = prev_off {
                            let prev_rec = read_u16_le(&after, prev + 4)? as usize;
                            let merged = (prev_rec + rec_len) as u16;
                            write_u16_le(&mut after, prev + 4, merged)?;
                        } else {
                            after[off..off + 4].fill(0);
                        }
                        self.refresh_dirblock_checksum(dir_ino, &disk_inode, &mut after)?;
                        return Ok((found_ino, phys, before, after));
                    }
                }
                prev_off = Some(off);
                off += rec_len;
            }
        }

        Err(Ext4FormatError::OutOfBounds)
    }

    /// Remove the directory entry named `name` from `dir_ino`.  Returns the
    /// inode number that was removed.
    pub fn remove_dir_entry(&mut self, dir_ino: InodeNo, name: &[u8]) -> Result<InodeNo> {
        let (found_ino, phys, _before, after) =
            self.plan_remove_dir_entry_after_image(dir_ino, name)?;
        self.image.write_block(phys, &after)?;
        Ok(found_ino)
    }

    /// Build the bounded regular-file create mutation.
    ///
    /// This records inode allocation, inode-table initialization, and parent
    /// dirent publication as immutable after-images. Directory creation remains
    /// separate because it also allocates and initializes a directory data block
    /// plus parent nlink updates.
    pub fn plan_create_regular_file(
        &mut self,
        parent_ino: InodeNo,
        name: &[u8],
        mode: u16,
        uid: u32,
        gid: u32,
        fsync_stamp: FsyncStamp,
    ) -> Result<(InodeNo, Ext4MutationPlan)> {
        let pending_before = self.pending_metadata.clone();
        let planned =
            self.plan_create_regular_file_inner(parent_ino, name, mode, uid, gid, fsync_stamp);
        self.pending_metadata = pending_before;
        planned
    }

    fn plan_create_regular_file_inner(
        &mut self,
        parent_ino: InodeNo,
        name: &[u8],
        mode: u16,
        uid: u32,
        gid: u32,
        fsync_stamp: FsyncStamp,
    ) -> Result<(InodeNo, Ext4MutationPlan)> {
        let (new_ino, group_index, bitmap_home, bitmap_before, bitmap_after) =
            self.plan_inode_allocation()?;
        let mut plan = self.plan_append_dir_entry_mutation(
            parent_ino,
            name,
            new_ino,
            1,
            MutationOrigin::Create,
            new_ino.get() as u64,
            fsync_stamp,
        )?;
        self.stage_mutation_after_images(&plan);
        let (group_desc_home, group_desc_before, group_desc_after) =
            self.plan_group_free_inode_decrement(group_index, &bitmap_after)?;
        let (superblock_home, superblock_before, superblock_after) =
            self.plan_superblock_free_inode_decrement()?;

        let generation = self.next_inode_generation(new_ino)?;
        let mut inode = Inode::default();
        inode.generation = generation;
        inode.extra_isize = self.new_inode_extra_isize();
        inode.mode = Inode::S_IFREG | (mode & 0o7777);
        inode.uid = uid;
        inode.gid = gid;
        inode.atime = fsync_stamp
            .raw()
            .try_into()
            .map_err(|_| Ext4FormatError::OutOfBounds)?;
        inode.ctime = inode.atime;
        inode.mtime = inode.atime;
        inode.links_count = 1;
        inode.flags = Inode::EXTENTS_FL;
        inode.set_extent_root(&[])?;

        let location = self.inode_location(new_ino)?;
        let mut inode_table_before = [0u8; BLOCK_SIZE];
        self.read_block(location.block, &mut inode_table_before)?;
        let mut inode_table_after = inode_table_before;
        let inode_bytes = &mut inode_table_after[location.offset..location.offset + location.len];
        inode.encode(inode_bytes)?;
        self.refresh_inode_checksum(new_ino, &inode, inode_bytes)?;

        let mut create =
            Ext4MutationPlan::new(MutationOrigin::Create, new_ino.get() as u64, fsync_stamp);
        create
            .push_metadata(MetadataBlock {
                home: bitmap_home,
                role: MetaRole::InodeBitmap,
                before_version: crc32c(0, &bitmap_before) as u64,
                after: bitmap_after,
                depends_on: Vec::new(),
            })
            .map_err(|_| Ext4FormatError::Corrupt)?;
        create
            .push_metadata(MetadataBlock {
                home: group_desc_home,
                role: MetaRole::GroupDescriptor,
                before_version: crc32c(0, &group_desc_before) as u64,
                after: group_desc_after,
                depends_on: Vec::new(),
            })
            .map_err(|_| Ext4FormatError::Corrupt)?;
        create
            .push_metadata(MetadataBlock {
                home: superblock_home,
                role: MetaRole::Superblock,
                before_version: crc32c(0, &superblock_before) as u64,
                after: superblock_after,
                depends_on: Vec::new(),
            })
            .map_err(|_| Ext4FormatError::Corrupt)?;
        create
            .push_metadata(MetadataBlock {
                home: location.block,
                role: MetaRole::InodeTable,
                before_version: crc32c(0, &inode_table_before) as u64,
                after: inode_table_after,
                depends_on: Vec::new(),
            })
            .map_err(|_| Ext4FormatError::Corrupt)?;
        merge_chained_mutation(&mut plan, &create)?;
        Ok((new_ino, plan))
    }

    /// Build the bounded directory create mutation.
    ///
    /// This is the directory counterpart to `plan_create_regular_file`: it
    /// records inode allocation, data-block allocation, child directory
    /// initialization, parent dirent publication, and parent/child inode-table
    /// updates as immutable after-images. More complex allocation layouts stay
    /// fail-closed until the full namespace/orphan lifecycle is wired.
    pub fn plan_create_directory(
        &mut self,
        parent_ino: InodeNo,
        name: &[u8],
        mode: u16,
        uid: u32,
        gid: u32,
        fsync_stamp: FsyncStamp,
    ) -> Result<(InodeNo, u64, Ext4MutationPlan)> {
        let pending_before = self.pending_metadata.clone();
        let planned =
            self.plan_create_directory_inner(parent_ino, name, mode, uid, gid, fsync_stamp);
        self.pending_metadata = pending_before;
        planned
    }

    fn plan_create_directory_inner(
        &mut self,
        parent_ino: InodeNo,
        name: &[u8],
        mode: u16,
        uid: u32,
        gid: u32,
        fsync_stamp: FsyncStamp,
    ) -> Result<(InodeNo, u64, Ext4MutationPlan)> {
        let (new_ino, inode_group, inode_bitmap_home, inode_bitmap_before, inode_bitmap_after) =
            self.plan_inode_allocation()?;
        let mut plan = self.plan_append_dir_entry_mutation(
            parent_ino,
            name,
            new_ino,
            2,
            MutationOrigin::Create,
            new_ino.get() as u64,
            fsync_stamp,
        )?;
        self.stage_mutation_after_images(&plan);
        let (data_block, block_group, block_bitmap_home, block_bitmap_before, block_bitmap_after) =
            self.plan_block_allocation()?;
        let group_desc_updates = self.plan_group_mkdir_count_updates(
            inode_group,
            &inode_bitmap_after,
            block_group,
            &block_bitmap_after,
        )?;
        let (superblock_home, superblock_before, superblock_after) =
            self.plan_superblock_mkdir_counts()?;

        let mut child_dir_before = [0u8; BLOCK_SIZE];
        self.read_block(data_block, &mut child_dir_before)?;
        let mut child_dir_after = [0u8; BLOCK_SIZE];
        encode_dir_entry(new_ino.get(), 12, 2, b".", &mut child_dir_after[0..12])?;
        encode_dir_entry(
            parent_ino.get(),
            (BLOCK_SIZE - 12) as u16,
            2,
            b"..",
            &mut child_dir_after[12..],
        )?;

        let generation = self.next_inode_generation(new_ino)?;
        let mut inode = Inode::default();
        inode.generation = generation;
        inode.extra_isize = self.new_inode_extra_isize();
        inode.mode = Inode::S_IFDIR | (mode & 0o7777);
        inode.uid = uid;
        inode.gid = gid;
        inode.size = BLOCK_SIZE as u64;
        inode.atime = fsync_stamp
            .raw()
            .try_into()
            .map_err(|_| Ext4FormatError::OutOfBounds)?;
        inode.ctime = inode.atime;
        inode.mtime = inode.atime;
        inode.links_count = 2;
        inode.blocks_512 = (BLOCK_SIZE / 512) as u64;
        inode.flags = Inode::EXTENTS_FL;
        inode.set_extent_root(&[Extent {
            logical_block: 0,
            len: 1,
            physical_start: data_block,
        }])?;
        self.refresh_dirblock_checksum(new_ino, &inode, &mut child_dir_after)?;

        let parent_location = self.inode_location(parent_ino)?;
        let new_location = self.inode_location(new_ino)?;
        let mut parent_table_before = [0u8; BLOCK_SIZE];
        self.read_block(parent_location.block, &mut parent_table_before)?;
        let mut parent_table_after = parent_table_before;
        let mut new_table_before = parent_table_before;
        let mut new_table_after = parent_table_after;
        if new_location.block != parent_location.block {
            self.read_block(new_location.block, &mut new_table_before)?;
            new_table_after = new_table_before;
        }

        {
            let inode_bytes =
                &mut new_table_after[new_location.offset..new_location.offset + new_location.len];
            inode.encode(inode_bytes)?;
            self.refresh_inode_checksum(new_ino, &inode, inode_bytes)?;
        }
        if new_location.block == parent_location.block {
            parent_table_after = new_table_after;
        }
        {
            let parent_inode_bytes = &mut parent_table_after
                [parent_location.offset..parent_location.offset + parent_location.len];
            let mut parent_inode = Inode::parse(parent_inode_bytes)?;
            if !parent_inode.is_dir() || parent_inode.is_htree_indexed() {
                return Err(Ext4FormatError::Unsupported);
            }
            parent_inode.links_count = parent_inode
                .links_count
                .checked_add(1)
                .ok_or(Ext4FormatError::OutOfBounds)?;
            parent_inode.ctime = fsync_stamp
                .raw()
                .try_into()
                .map_err(|_| Ext4FormatError::OutOfBounds)?;
            parent_inode.encode_preserving_unknown(parent_inode_bytes)?;
            self.refresh_inode_checksum(parent_ino, &parent_inode, parent_inode_bytes)?;
        }
        if new_location.block == parent_location.block {
            new_table_after = parent_table_after;
        }

        let mut create =
            Ext4MutationPlan::new(MutationOrigin::Create, new_ino.get() as u64, fsync_stamp);
        create
            .push_metadata(MetadataBlock {
                home: block_bitmap_home,
                role: MetaRole::BlockBitmap,
                before_version: crc32c(0, &block_bitmap_before) as u64,
                after: block_bitmap_after,
                depends_on: Vec::new(),
            })
            .map_err(|_| Ext4FormatError::Corrupt)?;
        create
            .push_metadata(MetadataBlock {
                home: inode_bitmap_home,
                role: MetaRole::InodeBitmap,
                before_version: crc32c(0, &inode_bitmap_before) as u64,
                after: inode_bitmap_after,
                depends_on: Vec::new(),
            })
            .map_err(|_| Ext4FormatError::Corrupt)?;
        for (home, before, after) in group_desc_updates {
            create
                .push_metadata(MetadataBlock {
                    home,
                    role: MetaRole::GroupDescriptor,
                    before_version: crc32c(0, &before) as u64,
                    after,
                    depends_on: Vec::new(),
                })
                .map_err(|_| Ext4FormatError::Corrupt)?;
        }
        create
            .push_metadata(MetadataBlock {
                home: superblock_home,
                role: MetaRole::Superblock,
                before_version: crc32c(0, &superblock_before) as u64,
                after: superblock_after,
                depends_on: Vec::new(),
            })
            .map_err(|_| Ext4FormatError::Corrupt)?;
        create
            .push_metadata(MetadataBlock {
                home: parent_location.block,
                role: MetaRole::InodeTable,
                before_version: crc32c(0, &parent_table_before) as u64,
                after: parent_table_after,
                depends_on: Vec::new(),
            })
            .map_err(|_| Ext4FormatError::Corrupt)?;
        if new_location.block != parent_location.block {
            create
                .push_metadata(MetadataBlock {
                    home: new_location.block,
                    role: MetaRole::InodeTable,
                    before_version: crc32c(0, &new_table_before) as u64,
                    after: new_table_after,
                    depends_on: Vec::new(),
                })
                .map_err(|_| Ext4FormatError::Corrupt)?;
        }
        create
            .push_metadata(MetadataBlock {
                home: data_block,
                role: MetaRole::DirectoryBlock,
                before_version: crc32c(0, &child_dir_before) as u64,
                after: child_dir_after,
                depends_on: Vec::new(),
            })
            .map_err(|_| Ext4FormatError::Corrupt)?;
        create.allocations.push(BlockClaim {
            physical_block: data_block,
        });
        merge_chained_mutation(&mut plan, &create)?;
        Ok((new_ino, data_block, plan))
    }

    /// Build the bounded fast-symlink create mutation.
    ///
    /// Tier 1 only admits inline symlink targets that fit in `i_block`; longer
    /// targets require data-block allocation and remain fail-closed.
    pub fn plan_create_fast_symlink(
        &mut self,
        parent_ino: InodeNo,
        name: &[u8],
        link_target: &[u8],
        uid: u32,
        gid: u32,
        fsync_stamp: FsyncStamp,
    ) -> Result<(InodeNo, Ext4MutationPlan)> {
        let pending_before = self.pending_metadata.clone();
        let planned = self.plan_create_fast_symlink_inner(
            parent_ino,
            name,
            link_target,
            uid,
            gid,
            fsync_stamp,
        );
        self.pending_metadata = pending_before;
        planned
    }

    fn plan_create_fast_symlink_inner(
        &mut self,
        parent_ino: InodeNo,
        name: &[u8],
        link_target: &[u8],
        uid: u32,
        gid: u32,
        fsync_stamp: FsyncStamp,
    ) -> Result<(InodeNo, Ext4MutationPlan)> {
        let (new_ino, group_index, bitmap_home, bitmap_before, bitmap_after) =
            self.plan_inode_allocation()?;
        let mut plan = self.plan_append_dir_entry_mutation(
            parent_ino,
            name,
            new_ino,
            7,
            MutationOrigin::Create,
            new_ino.get() as u64,
            fsync_stamp,
        )?;
        self.stage_mutation_after_images(&plan);
        let (group_desc_home, group_desc_before, group_desc_after) =
            self.plan_group_free_inode_decrement(group_index, &bitmap_after)?;
        let (superblock_home, superblock_before, superblock_after) =
            self.plan_superblock_free_inode_decrement()?;

        let generation = self.next_inode_generation(new_ino)?;
        let mut inode = Inode::default();
        inode.generation = generation;
        inode.extra_isize = self.new_inode_extra_isize();
        inode.mode = Inode::S_IFLNK | 0o777;
        inode.uid = uid;
        inode.gid = gid;
        inode.atime = fsync_stamp
            .raw()
            .try_into()
            .map_err(|_| Ext4FormatError::OutOfBounds)?;
        inode.ctime = inode.atime;
        inode.mtime = inode.atime;
        inode.links_count = 1;
        inode.set_inline_symlink_target(link_target)?;

        let location = self.inode_location(new_ino)?;
        let mut inode_table_before = [0u8; BLOCK_SIZE];
        self.read_block(location.block, &mut inode_table_before)?;
        let mut inode_table_after = inode_table_before;
        let inode_bytes = &mut inode_table_after[location.offset..location.offset + location.len];
        inode.encode(inode_bytes)?;
        self.refresh_inode_checksum(new_ino, &inode, inode_bytes)?;

        let mut create =
            Ext4MutationPlan::new(MutationOrigin::Create, new_ino.get() as u64, fsync_stamp);
        create
            .push_metadata(MetadataBlock {
                home: bitmap_home,
                role: MetaRole::InodeBitmap,
                before_version: crc32c(0, &bitmap_before) as u64,
                after: bitmap_after,
                depends_on: Vec::new(),
            })
            .map_err(|_| Ext4FormatError::Corrupt)?;
        create
            .push_metadata(MetadataBlock {
                home: group_desc_home,
                role: MetaRole::GroupDescriptor,
                before_version: crc32c(0, &group_desc_before) as u64,
                after: group_desc_after,
                depends_on: Vec::new(),
            })
            .map_err(|_| Ext4FormatError::Corrupt)?;
        create
            .push_metadata(MetadataBlock {
                home: superblock_home,
                role: MetaRole::Superblock,
                before_version: crc32c(0, &superblock_before) as u64,
                after: superblock_after,
                depends_on: Vec::new(),
            })
            .map_err(|_| Ext4FormatError::Corrupt)?;
        create
            .push_metadata(MetadataBlock {
                home: location.block,
                role: MetaRole::InodeTable,
                before_version: crc32c(0, &inode_table_before) as u64,
                after: inode_table_after,
                depends_on: Vec::new(),
            })
            .map_err(|_| Ext4FormatError::Corrupt)?;
        merge_chained_mutation(&mut plan, &create)?;
        Ok((new_ino, plan))
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

    /// Return the next persistent incarnation for an inode bitmap slot.
    ///
    /// Unlink keeps the previous generation in the cleared inode so a later
    /// allocation of the same numeric inode cannot inherit stale VFS, page
    /// cache, or advisory-lock state from its former occupant.
    fn next_inode_generation(&mut self, inode: InodeNo) -> Result<u32> {
        let previous = self.read_inode(inode)?.generation;
        let next = previous.wrapping_add(1);
        Ok(if next == 0 { 1 } else { next })
    }

    fn read_inode(&mut self, inode: InodeNo) -> Result<Inode> {
        let location = self.inode_location(inode)?;
        let mut block = [0u8; BLOCK_SIZE];
        self.read_block(location.block, &mut block)?;
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
                BlockMapping::Data(block) => self.read_block(block, &mut page)?,
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
                BlockMapping::Data(block) => self.read_block(block, &mut page)?,
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
                    self.read_block(child, &mut block)?;
                    node = ExtentNode::parse(&block)?;
                }
            }
        }
        Err(Ext4FormatError::Corrupt)
    }
}

fn validate_extent_node_layout(bytes: &[u8], header: ExtentHeader) -> Result<()> {
    if header.depth == 0 {
        let mut previous_end = None;
        for entry in 0..usize::from(header.entries) {
            let start = 12 + entry * 12;
            let extent = Extent::parse(
                bytes
                    .get(start..start + 12)
                    .ok_or(Ext4FormatError::Truncated)?,
            )?;
            let len = extent.initialized_len();
            if len == 0 {
                return Err(Ext4FormatError::Corrupt);
            }
            if previous_end.is_some_and(|end| extent.logical_block < end) {
                return Err(Ext4FormatError::Corrupt);
            }
            previous_end = Some(
                extent
                    .logical_block
                    .checked_add(len)
                    .ok_or(Ext4FormatError::Corrupt)?,
            );
        }
    } else {
        let mut previous_key = None;
        for entry in 0..usize::from(header.entries) {
            let start = 12 + entry * 12;
            let index = ExtentIdx::parse(
                bytes
                    .get(start..start + 12)
                    .ok_or(Ext4FormatError::Truncated)?,
            )?;
            if previous_key.is_some_and(|key| index.logical_block <= key) {
                return Err(Ext4FormatError::Corrupt);
            }
            previous_key = Some(index.logical_block);
        }
    }
    Ok(())
}

fn physical_ranges_contain(ranges: &[PhysicalBlockRange], block: u64) -> bool {
    let index = ranges.partition_point(|range| range.start <= block);
    index != 0 && ranges[index - 1].contains(block)
}

fn truncate_extents_at(extents: Vec<Extent>, new_end: u32) -> Result<(Vec<Extent>, Vec<u64>)> {
    let mut retained = Vec::new();
    let mut released = Vec::new();
    for mut extent in extents {
        if !extent.is_initialized() {
            return Err(Ext4FormatError::Unsupported);
        }
        let extent_end = extent
            .logical_block
            .checked_add(extent.initialized_len())
            .ok_or(Ext4FormatError::Corrupt)?;
        if extent_end <= new_end {
            retained.push(extent);
            continue;
        }
        let keep = new_end.saturating_sub(extent.logical_block);
        for offset in keep..extent.initialized_len() {
            released.push(
                extent
                    .physical_start
                    .checked_add(offset as u64)
                    .ok_or(Ext4FormatError::OutOfBounds)?,
            );
        }
        if keep != 0 {
            extent.len = u16::try_from(keep).map_err(|_| Ext4FormatError::Corrupt)?;
            retained.push(extent);
        }
    }
    Ok((retained, released))
}

fn convert_inline_uninitialized_extent(
    root: &[u8],
    logical: u32,
) -> Result<Option<(u64, Vec<Extent>)>> {
    let mut extents = match ExtentNode::parse(root)? {
        ExtentNode::Leaf(extents) => extents,
        ExtentNode::Index(_) => return Ok(None),
    };
    let Some(physical) = convert_uninitialized_extent_list(&mut extents, logical)? else {
        return Ok(None);
    };
    if extents.len() > 4 {
        return Err(Ext4FormatError::Unsupported);
    }
    Ok(Some((physical, extents)))
}

fn convert_uninitialized_extent_list(
    extents: &mut Vec<Extent>,
    logical: u32,
) -> Result<Option<u64>> {
    let Some(position) = extents
        .iter()
        .position(|extent| !extent.is_initialized() && extent.contains(logical))
    else {
        return Ok(None);
    };
    let original = extents[position];
    let prefix_len = logical
        .checked_sub(original.logical_block)
        .ok_or(Ext4FormatError::Corrupt)?;
    let total_len = original.initialized_len();
    let suffix_len = total_len
        .checked_sub(prefix_len)
        .and_then(|remaining| remaining.checked_sub(1))
        .ok_or(Ext4FormatError::Corrupt)?;
    let physical = original
        .physical_start
        .checked_add(u64::from(prefix_len))
        .ok_or(Ext4FormatError::OutOfBounds)?;

    let mut replacement = Vec::new();
    if prefix_len != 0 {
        replacement.push(Extent {
            logical_block: original.logical_block,
            len: uninitialized_extent_len(prefix_len)?,
            physical_start: original.physical_start,
        });
    }
    replacement.push(Extent {
        logical_block: logical,
        len: 1,
        physical_start: physical,
    });
    if suffix_len != 0 {
        replacement.push(Extent {
            logical_block: logical.checked_add(1).ok_or(Ext4FormatError::Corrupt)?,
            len: uninitialized_extent_len(suffix_len)?,
            physical_start: physical
                .checked_add(1)
                .ok_or(Ext4FormatError::OutOfBounds)?,
        });
    }
    extents.splice(position..=position, replacement);
    Ok(Some(physical))
}

fn uninitialized_extent_len(len: u32) -> Result<u16> {
    if len == 0 || len >= u32::from(Extent::UNINITIALIZED_MASK) {
        return Err(Ext4FormatError::Corrupt);
    }
    Ok(Extent::UNINITIALIZED_MASK | len as u16)
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
        generation: inode.generation,
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
