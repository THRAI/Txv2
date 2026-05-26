use crate::ondisk::{
    encode_dir_entry, encode_journal_commit, encode_journal_descriptor, parse_journal_descriptor,
    BitmapMut, BitmapView, BlockMapping, CommitHeader, DirEntry, DirEntryIter, Extent, ExtentNode,
    GroupDesc, Inode, InodeLocation, InodeTableLayout, Superblock,
};
use crate::ondisk::{read_u16_le, write_u16_le};
use crate::{Ext4FormatError, Result};
use alloc::vec;
use alloc::vec::Vec;

pub const BLOCK_SIZE: usize = 4096;
pub type Page4K = [u8; BLOCK_SIZE];

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
    /// its number.  Panics on group-desc absence, returns `OutOfBounds` when
    /// the bitmap is full.
    pub fn allocate_inode(&mut self) -> Result<InodeNo> {
        let group = *self.groups.first().ok_or(Ext4FormatError::Corrupt)?;
        let bitmap_block = group.inode_bitmap_block();
        let mut bitmap = [0u8; BLOCK_SIZE];
        self.image.read_block(bitmap_block, &mut bitmap)?;
        let bit = BitmapView::new(&bitmap)
            .first_zero()
            .ok_or(Ext4FormatError::OutOfBounds)?;
        BitmapMut::new(&mut bitmap).set(bit)?;
        self.image.write_block(bitmap_block, &bitmap)?;
        Ok(InodeNo::new(bit as u32 + 1))
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

    /// Update the inode table metadata fields directly (no journal).
    pub fn write_inode_meta(&mut self, inode_no: InodeNo, meta: InodeMetaLite) -> Result<()> {
        let loc = self.inode_location(inode_no)?;
        let mut block = [0u8; BLOCK_SIZE];
        self.image.read_block(loc.block, &mut block)?;
        let mut inode = Inode::parse(&block[loc.offset..loc.offset + loc.len])?;
        apply_meta(&mut inode, meta);
        inode.encode(&mut block[loc.offset..loc.offset + loc.len])?;
        self.image.write_block(loc.block, &block)?;
        Ok(())
    }

    /// Allocate a free data block in group 0, mark it used, and return its
    /// absolute block number.
    pub fn allocate_block(&mut self) -> Result<u64> {
        let group = *self.groups.first().ok_or(Ext4FormatError::Corrupt)?;
        let bitmap_block = group.block_bitmap_block();
        let mut bitmap = [0u8; BLOCK_SIZE];
        self.image.read_block(bitmap_block, &mut bitmap)?;
        let bit = BitmapView::new(&bitmap)
            .first_zero()
            .ok_or(Ext4FormatError::OutOfBounds)?;
        BitmapMut::new(&mut bitmap).set(bit)?;
        self.image.write_block(bitmap_block, &bitmap)?;
        Ok(self.superblock.first_data_block as u64 + bit as u64)
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

    pub fn read_dir_entries_from(
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
