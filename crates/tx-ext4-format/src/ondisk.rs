use core::convert::TryInto;

use alloc::vec::Vec;

pub use crate::Ext4FormatError;

use crate::Result;

pub const EXT4_SUPER_MAGIC: u16 = 0xEF53;
pub const JBD2_MAGIC: u32 = 0xC03B_3998;
pub const JBD2_BLOCK_DESCRIPTOR: u32 = 1;
pub const JBD2_BLOCK_COMMIT: u32 = 2;
const EXTENT_MAGIC: u16 = 0xF30A;
const EXTENT_ROOT_BYTES: usize = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Superblock {
    pub inodes_count: u32,
    pub blocks_count: u64,
    pub free_blocks_count: u64,
    pub free_inodes_count: u32,
    pub first_data_block: u32,
    pub log_block_size: u32,
    pub blocks_per_group: u32,
    pub inodes_per_group: u32,
    pub inode_size: u16,
    pub desc_size: u16,
    pub feature_compat: u32,
    pub feature_incompat: u32,
    pub feature_ro_compat: u32,
    pub uuid: [u8; 16],
    pub journal_inode: u32,
    pub hash_seed: [u32; 4],
    pub default_hash_version: u8,
    pub checksum_type: u8,
    pub checksum_seed: u32,
    pub checksum: u32,
}

impl Superblock {
    pub const FEATURE_COMPAT_HAS_JOURNAL: u32 = 0x0004;
    pub const FEATURE_INCOMPAT_RECOVER: u32 = 0x0004;
    pub const FEATURE_INCOMPAT_EXTENTS: u32 = 0x0040;
    pub const FEATURE_INCOMPAT_64BIT: u32 = 0x0080;
    pub const FEATURE_INCOMPAT_CSUM_SEED: u32 = 0x2000;
    pub const FEATURE_RO_COMPAT_HUGE_FILE: u32 = 0x0008;
    pub const FEATURE_RO_COMPAT_METADATA_CSUM: u32 = 0x0400;

    pub fn parse(bytes: &[u8]) -> Result<Self> {
        require_len(bytes, 1024)?;
        let magic = read_u16_le(bytes, 56)?;
        if magic != EXT4_SUPER_MAGIC {
            return Err(Ext4FormatError::BadMagic);
        }
        Ok(Self {
            inodes_count: read_u32_le(bytes, 0)?,
            blocks_count: read_u32_le(bytes, 4)? as u64
                | ((read_u32_le(bytes, 0x150)? as u64) << 32),
            free_blocks_count: read_u32_le(bytes, 12)? as u64
                | ((read_u32_le(bytes, 0x158)? as u64) << 32),
            free_inodes_count: read_u32_le(bytes, 16)?,
            first_data_block: read_u32_le(bytes, 20)?,
            log_block_size: read_u32_le(bytes, 24)?,
            blocks_per_group: read_u32_le(bytes, 32)?,
            inodes_per_group: read_u32_le(bytes, 40)?,
            inode_size: read_u16_le(bytes, 88)?,
            desc_size: read_u16_le(bytes, 254)?,
            feature_compat: read_u32_le(bytes, 92)?,
            feature_incompat: read_u32_le(bytes, 96)?,
            feature_ro_compat: read_u32_le(bytes, 100)?,
            uuid: slice_at(bytes, 104, 16)?.try_into().unwrap(),
            journal_inode: read_u32_le(bytes, 224)?,
            hash_seed: [
                read_u32_le(bytes, 236)?,
                read_u32_le(bytes, 240)?,
                read_u32_le(bytes, 244)?,
                read_u32_le(bytes, 248)?,
            ],
            default_hash_version: *slice_at(bytes, 252, 1)?.first().unwrap(),
            checksum_type: *slice_at(bytes, 373, 1)?.first().unwrap(),
            checksum_seed: read_u32_le(bytes, 624)?,
            checksum: read_u32_le(bytes, 1020)?,
        })
    }

    pub fn encode(&self, bytes: &mut [u8]) -> Result<()> {
        require_len(bytes, 1024)?;
        bytes.fill(0);
        write_u32_le(bytes, 0, self.inodes_count)?;
        write_u32_le(bytes, 4, self.blocks_count as u32)?;
        write_u32_le(bytes, 12, self.free_blocks_count as u32)?;
        write_u32_le(bytes, 16, self.free_inodes_count)?;
        write_u32_le(bytes, 20, self.first_data_block)?;
        write_u32_le(bytes, 0x150, (self.blocks_count >> 32) as u32)?;
        write_u32_le(bytes, 0x158, (self.free_blocks_count >> 32) as u32)?;
        write_u32_le(bytes, 24, self.log_block_size)?;
        write_u32_le(bytes, 32, self.blocks_per_group)?;
        write_u32_le(bytes, 40, self.inodes_per_group)?;
        write_u16_le(bytes, 56, EXT4_SUPER_MAGIC)?;
        write_u32_le(bytes, 84, 11)?;
        write_u16_le(bytes, 88, self.inode_size)?;
        write_u32_le(bytes, 92, self.feature_compat)?;
        write_u32_le(bytes, 96, self.feature_incompat)?;
        write_u32_le(bytes, 100, self.feature_ro_compat)?;
        slice_at_mut(bytes, 104, 16)?.copy_from_slice(&self.uuid);
        write_u32_le(bytes, 224, self.journal_inode)?;
        write_u32_le(bytes, 236, self.hash_seed[0])?;
        write_u32_le(bytes, 240, self.hash_seed[1])?;
        write_u32_le(bytes, 244, self.hash_seed[2])?;
        write_u32_le(bytes, 248, self.hash_seed[3])?;
        slice_at_mut(bytes, 252, 1)?[0] = self.default_hash_version;
        write_u16_le(bytes, 254, self.desc_size)?;
        slice_at_mut(bytes, 373, 1)?[0] = self.checksum_type;
        write_u32_le(bytes, 624, self.checksum_seed)?;
        write_u32_le(bytes, 1020, self.checksum)?;
        Ok(())
    }

    pub fn block_size(&self) -> u32 {
        1024u32 << self.log_block_size
    }

    pub fn group_desc_size(&self) -> usize {
        if self.feature_incompat & Self::FEATURE_INCOMPAT_64BIT != 0 {
            core::cmp::max(self.desc_size as usize, 64)
        } else {
            32
        }
    }

    pub fn group_count(&self) -> Result<u32> {
        if self.blocks_per_group == 0 {
            return Err(Ext4FormatError::Corrupt);
        }
        let data_blocks = self
            .blocks_count
            .saturating_sub(self.first_data_block as u64);
        Ok(div_ceil_u64(data_blocks, self.blocks_per_group as u64) as u32)
    }

    pub fn has_metadata_csum(&self) -> bool {
        self.feature_ro_compat & Self::FEATURE_RO_COMPAT_METADATA_CSUM != 0
    }

    /// Ext4 sets this incompatibility bit before a read-write journalled mount
    /// admits mutations and clears it only after clean detach.
    pub const fn needs_recovery(&self) -> bool {
        self.feature_incompat & Self::FEATURE_INCOMPAT_RECOVER != 0
    }

    pub fn metadata_csum_seed(&self) -> u32 {
        if self.feature_incompat & Self::FEATURE_INCOMPAT_CSUM_SEED != 0 {
            self.checksum_seed
        } else {
            crc32c_append(0xFFFF_FFFF, &self.uuid)
        }
    }
}

impl Default for Superblock {
    fn default() -> Self {
        Self {
            inodes_count: 0,
            blocks_count: 0,
            free_blocks_count: 0,
            free_inodes_count: 0,
            first_data_block: 0,
            log_block_size: 2,
            blocks_per_group: 0,
            inodes_per_group: 0,
            inode_size: 256,
            desc_size: 64,
            feature_compat: 0,
            feature_incompat: Self::FEATURE_INCOMPAT_EXTENTS,
            feature_ro_compat: 0,
            uuid: [0; 16],
            journal_inode: 8,
            hash_seed: [0; 4],
            default_hash_version: 1,
            checksum_type: 0,
            checksum_seed: 0,
            checksum: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GroupDesc {
    pub block_bitmap: u32,
    pub inode_bitmap: u32,
    pub inode_table: u32,
    pub free_blocks_count_hi: u16,
    pub free_inodes_count_hi: u16,
    pub used_dirs_count_hi: u16,
    pub free_blocks_count: u16,
    pub free_inodes_count: u16,
    pub used_dirs_count: u16,
    pub flags: u16,
    pub block_bitmap_hi: u32,
    pub inode_bitmap_hi: u32,
    pub inode_table_hi: u32,
    pub checksum: u16,
}

impl GroupDesc {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        Self::parse_sized(bytes, bytes.len())
    }

    pub fn parse_sized(bytes: &[u8], desc_size: usize) -> Result<Self> {
        require_len(bytes, 32)?;
        let mut desc = Self {
            block_bitmap: read_u32_le(bytes, 0)?,
            inode_bitmap: read_u32_le(bytes, 4)?,
            inode_table: read_u32_le(bytes, 8)?,
            free_blocks_count_hi: 0,
            free_inodes_count_hi: 0,
            used_dirs_count_hi: 0,
            free_blocks_count: read_u16_le(bytes, 12)?,
            free_inodes_count: read_u16_le(bytes, 14)?,
            used_dirs_count: read_u16_le(bytes, 16)?,
            flags: read_u16_le(bytes, 18)?,
            block_bitmap_hi: 0,
            inode_bitmap_hi: 0,
            inode_table_hi: 0,
            checksum: read_u16_le(bytes, 30)?,
        };
        if desc_size >= 64 && bytes.len() >= 64 {
            desc.block_bitmap_hi = read_u32_le(bytes, 32)?;
            desc.inode_bitmap_hi = read_u32_le(bytes, 36)?;
            desc.inode_table_hi = read_u32_le(bytes, 40)?;
            desc.free_blocks_count_hi = read_u16_le(bytes, 44)?;
            desc.free_inodes_count_hi = read_u16_le(bytes, 46)?;
            desc.used_dirs_count_hi = read_u16_le(bytes, 48)?;
        }
        Ok(desc)
    }

    pub fn encode(&self, bytes: &mut [u8]) -> Result<()> {
        require_len(bytes, 32)?;
        bytes.fill(0);
        write_u32_le(bytes, 0, self.block_bitmap)?;
        write_u32_le(bytes, 4, self.inode_bitmap)?;
        write_u32_le(bytes, 8, self.inode_table)?;
        write_u16_le(bytes, 12, self.free_blocks_count)?;
        write_u16_le(bytes, 14, self.free_inodes_count)?;
        write_u16_le(bytes, 16, self.used_dirs_count)?;
        write_u16_le(bytes, 18, self.flags)?;
        write_u16_le(bytes, 30, self.checksum)?;
        if bytes.len() >= 64 {
            write_u32_le(bytes, 32, self.block_bitmap_hi)?;
            write_u32_le(bytes, 36, self.inode_bitmap_hi)?;
            write_u32_le(bytes, 40, self.inode_table_hi)?;
            write_u16_le(bytes, 44, self.free_blocks_count_hi)?;
            write_u16_le(bytes, 46, self.free_inodes_count_hi)?;
            write_u16_le(bytes, 48, self.used_dirs_count_hi)?;
        }
        Ok(())
    }

    pub fn block_bitmap_block(&self) -> u64 {
        self.block_bitmap as u64 | ((self.block_bitmap_hi as u64) << 32)
    }

    pub fn inode_bitmap_block(&self) -> u64 {
        self.inode_bitmap as u64 | ((self.inode_bitmap_hi as u64) << 32)
    }

    pub fn inode_table_block(&self) -> u64 {
        self.inode_table as u64 | ((self.inode_table_hi as u64) << 32)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InodeTableLayout {
    pub inode_size: usize,
    pub inodes_per_group: u32,
    pub first_inode_table_block: u64,
}

impl InodeTableLayout {
    pub fn from_superblock_group(superblock: &Superblock, group: &GroupDesc) -> Self {
        Self {
            inode_size: superblock.inode_size as usize,
            inodes_per_group: superblock.inodes_per_group,
            first_inode_table_block: group.inode_table as u64,
        }
    }

    pub fn locate(&self, inode: u32, block_size: usize) -> Result<InodeLocation> {
        if inode == 0 {
            return Err(Ext4FormatError::OutOfBounds);
        }
        let inode_index = (inode - 1) as u64;
        if inode_index >= self.inodes_per_group as u64 {
            return Err(Ext4FormatError::Unsupported);
        }
        let byte_offset = inode_index as usize * self.inode_size;
        let block = self.first_inode_table_block + (byte_offset / block_size) as u64;
        let offset = byte_offset % block_size;
        if offset + self.inode_size > block_size {
            return Err(Ext4FormatError::Unsupported);
        }
        Ok(InodeLocation {
            block,
            offset,
            len: self.inode_size,
        })
    }

    pub fn locate_index(&self, index_in_group: u32, block_size: usize) -> Result<InodeLocation> {
        if index_in_group >= self.inodes_per_group {
            return Err(Ext4FormatError::OutOfBounds);
        }
        let byte_offset = index_in_group as usize * self.inode_size;
        let block = self.first_inode_table_block + (byte_offset / block_size) as u64;
        let offset = byte_offset % block_size;
        if offset + self.inode_size > block_size {
            return Err(Ext4FormatError::Unsupported);
        }
        Ok(InodeLocation {
            block,
            offset,
            len: self.inode_size,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InodeLocation {
    pub block: u64,
    pub offset: usize,
    pub len: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Inode {
    pub mode: u16,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub atime: u32,
    pub ctime: u32,
    pub mtime: u32,
    pub dtime: u32,
    pub links_count: u16,
    pub blocks_512: u64,
    pub flags: u32,
    pub generation: u32,
    pub file_acl: u64,
    pub checksum_lo: u16,
    pub checksum_hi: u16,
    pub extra_isize: u16,
    pub crtime: u32,
    pub project_id: u32,
    i_block: [u8; EXTENT_ROOT_BYTES],
}

impl Inode {
    pub const EXTENTS_FL: u32 = 0x0008_0000;
    pub const S_IFMT: u16 = 0xF000;
    pub const S_IFLNK: u16 = 0xA000;
    pub const S_IFDIR: u16 = 0x4000;
    pub const S_IFREG: u16 = 0x8000;
    pub const INDEX_FL: u32 = 0x0000_1000;

    pub fn parse(bytes: &[u8]) -> Result<Self> {
        require_len(bytes, 128)?;
        let mut i_block = [0u8; EXTENT_ROOT_BYTES];
        i_block.copy_from_slice(slice_at(bytes, 40, EXTENT_ROOT_BYTES)?);
        Ok(Self {
            mode: read_u16_le(bytes, 0)?,
            uid: read_u16_le(bytes, 2)? as u32 | ((read_u16_le(bytes, 120)? as u32) << 16),
            gid: read_u16_le(bytes, 24)? as u32 | ((read_u16_le(bytes, 122)? as u32) << 16),
            size: read_u32_le(bytes, 4)? as u64 | ((read_u32_le(bytes, 108)? as u64) << 32),
            atime: read_u32_le(bytes, 8)?,
            ctime: read_u32_le(bytes, 12)?,
            mtime: read_u32_le(bytes, 16)?,
            dtime: read_u32_le(bytes, 20)?,
            links_count: read_u16_le(bytes, 26)?,
            blocks_512: read_u32_le(bytes, 28)? as u64 | ((read_u16_le(bytes, 116)? as u64) << 32),
            flags: read_u32_le(bytes, 32)?,
            generation: read_u32_le(bytes, 100)?,
            file_acl: read_u32_le(bytes, 104)? as u64 | ((read_u16_le(bytes, 118)? as u64) << 32),
            checksum_lo: read_u16_le(bytes, 124)?,
            checksum_hi: read_u16_le_optional(bytes, 130).unwrap_or(0),
            extra_isize: read_u16_le_optional(bytes, 128).unwrap_or(0),
            crtime: read_u32_le_optional(bytes, 144).unwrap_or(0),
            project_id: read_u32_le_optional(bytes, 156).unwrap_or(0),
            i_block,
        })
    }

    pub fn encode(&self, bytes: &mut [u8]) -> Result<()> {
        require_len(bytes, 128)?;
        bytes.fill(0);
        self.encode_preserving_unknown(bytes)
    }

    /// Encode modeled inode fields without clearing bytes owned by a newer
    /// ext4 revision or an unsupported feature. Metadata after-image planning
    /// starts from the complete on-disk inode and uses this method so an
    /// unrelated field cannot be lost during a bounded update.
    pub fn encode_preserving_unknown(&self, bytes: &mut [u8]) -> Result<()> {
        require_len(bytes, 128)?;
        write_u16_le(bytes, 0, self.mode)?;
        write_u16_le(bytes, 2, self.uid as u16)?;
        write_u32_le(bytes, 4, self.size as u32)?;
        write_u32_le(bytes, 8, self.atime)?;
        write_u32_le(bytes, 12, self.ctime)?;
        write_u32_le(bytes, 16, self.mtime)?;
        write_u32_le(bytes, 20, self.dtime)?;
        write_u16_le(bytes, 24, self.gid as u16)?;
        write_u16_le(bytes, 26, self.links_count)?;
        write_u32_le(bytes, 28, self.blocks_512 as u32)?;
        write_u32_le(bytes, 32, self.flags)?;
        bytes[40..100].copy_from_slice(&self.i_block);
        write_u32_le(bytes, 100, self.generation)?;
        write_u32_le(bytes, 104, self.file_acl as u32)?;
        write_u32_le(bytes, 108, (self.size >> 32) as u32)?;
        write_u16_le(bytes, 116, ((self.blocks_512 >> 32) & 0xFFFF) as u16)?;
        write_u16_le(bytes, 118, ((self.file_acl >> 32) & 0xFFFF) as u16)?;
        write_u16_le(bytes, 120, (self.uid >> 16) as u16)?;
        write_u16_le(bytes, 122, (self.gid >> 16) as u16)?;
        write_u16_le(bytes, 124, self.checksum_lo)?;
        if bytes.len() >= 160 {
            write_u16_le(bytes, 128, self.extra_isize)?;
            write_u16_le(bytes, 130, self.checksum_hi)?;
            write_u32_le(bytes, 144, self.crtime)?;
            write_u32_le(bytes, 156, self.project_id)?;
        }
        Ok(())
    }

    pub fn set_extent_root(&mut self, extents: &[Extent]) -> Result<()> {
        let needed = 12 + extents.len() * 12;
        if needed > EXTENT_ROOT_BYTES {
            return Err(Ext4FormatError::Unsupported);
        }
        self.i_block = [0; EXTENT_ROOT_BYTES];
        let header = ExtentHeader {
            entries: extents.len() as u16,
            max: 4,
            depth: 0,
            generation: 0,
        };
        header.encode(&mut self.i_block[..12])?;
        for (idx, extent) in extents.iter().enumerate() {
            extent.encode(&mut self.i_block[12 + idx * 12..24 + idx * 12])?;
        }
        Ok(())
    }

    pub fn set_extent_index_root(&mut self, indexes: &[ExtentIdx], depth: u16) -> Result<()> {
        if depth == 0 {
            return Err(Ext4FormatError::Corrupt);
        }
        ExtentNode::encode_index(depth, indexes, &mut self.i_block)
    }

    pub fn extent_root_bytes(&self) -> &[u8] {
        &self.i_block
    }

    pub fn inline_symlink_target(&self) -> Result<Option<&[u8]>> {
        if !self.is_symlink() || self.size > EXTENT_ROOT_BYTES as u64 {
            return Ok(None);
        }
        Ok(Some(slice_at(&self.i_block, 0, self.size as usize)?))
    }

    pub fn set_inline_symlink_target(&mut self, target: &[u8]) -> Result<()> {
        if target.len() > EXTENT_ROOT_BYTES {
            return Err(Ext4FormatError::Unsupported);
        }
        self.i_block = [0; EXTENT_ROOT_BYTES];
        self.i_block[..target.len()].copy_from_slice(target);
        self.size = target.len() as u64;
        Ok(())
    }

    pub fn is_dir(&self) -> bool {
        self.mode & Self::S_IFMT == Self::S_IFDIR
    }

    pub fn is_file(&self) -> bool {
        self.mode & Self::S_IFMT == Self::S_IFREG
    }

    pub fn is_symlink(&self) -> bool {
        self.mode & Self::S_IFMT == Self::S_IFLNK
    }

    pub fn is_htree_indexed(&self) -> bool {
        self.is_dir() && self.flags & Self::INDEX_FL != 0
    }

    pub fn map_extent_block(&self, logical_block: u32) -> Result<BlockMapping> {
        let node = ExtentNode::parse(self.extent_root_bytes())?;
        match node {
            ExtentNode::Leaf(extents) => {
                for extent in extents {
                    if let Some(block) = extent.physical_for(logical_block) {
                        return Ok(BlockMapping::Data(block));
                    }
                }
                Ok(BlockMapping::Hole)
            }
            ExtentNode::Index(indexes) => {
                let mut selected = None;
                for idx in indexes {
                    if idx.logical_block <= logical_block {
                        selected = Some(idx.child);
                    }
                }
                selected
                    .map(BlockMapping::NeedNode)
                    .ok_or(Ext4FormatError::Corrupt)
            }
        }
    }
}

impl Default for Inode {
    fn default() -> Self {
        Self {
            mode: 0,
            uid: 0,
            gid: 0,
            size: 0,
            atime: 0,
            ctime: 0,
            mtime: 0,
            dtime: 0,
            links_count: 0,
            blocks_512: 0,
            flags: 0,
            generation: 0,
            file_acl: 0,
            checksum_lo: 0,
            checksum_hi: 0,
            extra_isize: 0,
            crtime: 0,
            project_id: 0,
            i_block: [0; EXTENT_ROOT_BYTES],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockMapping {
    Data(u64),
    Hole,
    NeedNode(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtentHeader {
    pub entries: u16,
    pub max: u16,
    pub depth: u16,
    pub generation: u32,
}

impl ExtentHeader {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        require_len(bytes, 12)?;
        if read_u16_le(bytes, 0)? != EXTENT_MAGIC {
            return Err(Ext4FormatError::BadMagic);
        }
        let header = Self {
            entries: read_u16_le(bytes, 2)?,
            max: read_u16_le(bytes, 4)?,
            depth: read_u16_le(bytes, 6)?,
            generation: read_u32_le(bytes, 8)?,
        };
        if header.entries > header.max {
            return Err(Ext4FormatError::Corrupt);
        }
        Ok(header)
    }

    pub fn encode(&self, bytes: &mut [u8]) -> Result<()> {
        require_len(bytes, 12)?;
        write_u16_le(bytes, 0, EXTENT_MAGIC)?;
        write_u16_le(bytes, 2, self.entries)?;
        write_u16_le(bytes, 4, self.max)?;
        write_u16_le(bytes, 6, self.depth)?;
        write_u32_le(bytes, 8, self.generation)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    pub logical_block: u32,
    pub len: u16,
    pub physical_start: u64,
}

impl Extent {
    pub const UNINITIALIZED_MASK: u16 = 0x8000;

    pub fn parse(bytes: &[u8]) -> Result<Self> {
        require_len(bytes, 12)?;
        Ok(Self {
            logical_block: read_u32_le(bytes, 0)?,
            len: read_u16_le(bytes, 4)?,
            physical_start: read_u32_le(bytes, 8)? as u64 | ((read_u16_le(bytes, 6)? as u64) << 32),
        })
    }

    pub fn encode(&self, bytes: &mut [u8]) -> Result<()> {
        require_len(bytes, 12)?;
        write_u32_le(bytes, 0, self.logical_block)?;
        write_u16_le(bytes, 4, self.len)?;
        write_u16_le(bytes, 6, (self.physical_start >> 32) as u16)?;
        write_u32_le(bytes, 8, self.physical_start as u32)?;
        Ok(())
    }

    pub fn initialized_len(&self) -> u32 {
        (self.len & !Self::UNINITIALIZED_MASK) as u32
    }

    pub fn is_initialized(&self) -> bool {
        self.len & Self::UNINITIALIZED_MASK == 0
    }

    pub fn contains(&self, logical_block: u32) -> bool {
        logical_block >= self.logical_block
            && logical_block < self.logical_block.saturating_add(self.initialized_len())
    }

    pub fn physical_for(&self, logical_block: u32) -> Option<u64> {
        if self.contains(logical_block) && self.is_initialized() {
            Some(self.physical_start + (logical_block - self.logical_block) as u64)
        } else {
            None
        }
    }

    pub fn parse_all(bytes: &[u8]) -> Result<ExtentList> {
        match ExtentNode::parse(bytes)? {
            ExtentNode::Leaf(extents) => Ok(extents),
            ExtentNode::Index(_) => Err(Ext4FormatError::Unsupported),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtentIdx {
    pub logical_block: u32,
    pub child: u64,
}

impl ExtentIdx {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        require_len(bytes, 12)?;
        Ok(Self {
            logical_block: read_u32_le(bytes, 0)?,
            child: read_u32_le(bytes, 4)? as u64 | ((read_u16_le(bytes, 8)? as u64) << 32),
        })
    }

    pub fn encode(&self, bytes: &mut [u8]) -> Result<()> {
        require_len(bytes, 12)?;
        write_u32_le(bytes, 0, self.logical_block)?;
        write_u32_le(bytes, 4, self.child as u32)?;
        write_u16_le(bytes, 8, (self.child >> 32) as u16)?;
        write_u16_le(bytes, 10, 0)?;
        Ok(())
    }
}

pub type ExtentList = Vec<Extent>;
pub type ExtentIdxList = Vec<ExtentIdx>;

pub enum ExtentNode {
    Leaf(ExtentList),
    Index(ExtentIdxList),
}

impl ExtentNode {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let header = ExtentHeader::parse(bytes)?;
        let mut offset = 12usize;
        if header.depth == 0 {
            let mut extents = Vec::new();
            for _ in 0..header.entries {
                extents.push(Extent::parse(slice_at(bytes, offset, 12)?)?);
                offset += 12;
            }
            extents.sort_unstable_by_key(|extent| extent.logical_block);
            Ok(Self::Leaf(extents))
        } else {
            let mut indexes = Vec::new();
            for _ in 0..header.entries {
                indexes.push(ExtentIdx::parse(slice_at(bytes, offset, 12)?)?);
                offset += 12;
            }
            indexes.sort_unstable_by_key(|index| index.logical_block);
            Ok(Self::Index(indexes))
        }
    }

    pub fn encode_leaf(extents: &[Extent], bytes: &mut [u8]) -> Result<()> {
        prepare_extent_node(0, extents.len(), bytes)?;
        for (idx, extent) in extents.iter().enumerate() {
            extent.encode(slice_at_mut(bytes, 12 + idx * 12, 12)?)?;
        }
        Ok(())
    }

    pub fn encode_index(depth: u16, indexes: &[ExtentIdx], bytes: &mut [u8]) -> Result<()> {
        if depth == 0 {
            return Err(Ext4FormatError::Corrupt);
        }
        prepare_extent_node(depth, indexes.len(), bytes)?;
        for (idx, index) in indexes.iter().enumerate() {
            index.encode(slice_at_mut(bytes, 12 + idx * 12, 12)?)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirEntry<'a> {
    pub inode: u32,
    pub rec_len: u16,
    pub file_type: u8,
    pub name: &'a [u8],
}

pub struct DirEntryIter<'a> {
    data: &'a [u8],
    offset: usize,
}

impl<'a> DirEntryIter<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, offset: 0 }
    }
}

impl<'a> Iterator for DirEntryIter<'a> {
    type Item = Result<DirEntry<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.offset >= self.data.len() {
                return None;
            }
            if self.data.len() - self.offset < 8 {
                self.offset = self.data.len();
                return Some(Err(Ext4FormatError::Truncated));
            }
            let start = self.offset;
            let rec_len = match read_u16_le(self.data, start + 4) {
                Ok(v) => v as usize,
                Err(e) => return Some(Err(e)),
            };
            if rec_len == 0 {
                self.offset = self.data.len();
                return None;
            }
            if rec_len < 8 || start + rec_len > self.data.len() {
                self.offset = self.data.len();
                return Some(Err(Ext4FormatError::Corrupt));
            }
            self.offset += rec_len;
            let inode = read_u32_le(self.data, start).ok()?;
            if inode == 0 {
                continue;
            }
            let name_len = self.data[start + 6] as usize;
            if name_len > rec_len - 8 {
                return Some(Err(Ext4FormatError::Corrupt));
            }
            return Some(Ok(DirEntry {
                inode,
                rec_len: rec_len as u16,
                file_type: self.data[start + 7],
                name: &self.data[start + 8..start + 8 + name_len],
            }));
        }
    }
}

pub fn encode_dir_entry(
    inode: u32,
    rec_len: u16,
    file_type: u8,
    name: &[u8],
    out: &mut [u8],
) -> Result<()> {
    if name.len() > 255 || rec_len as usize > out.len() || rec_len < 8 + name.len() as u16 {
        return Err(Ext4FormatError::OutOfBounds);
    }
    out[..rec_len as usize].fill(0);
    write_u32_le(out, 0, inode)?;
    write_u16_le(out, 4, rec_len)?;
    out[6] = name.len() as u8;
    out[7] = file_type;
    out[8..8 + name.len()].copy_from_slice(name);
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DxRootInfo {
    pub hash_version: u8,
    pub info_length: u8,
    pub indirect_levels: u8,
    pub flags: u8,
}

impl DxRootInfo {
    pub const INFO_LENGTH: u8 = 8;
    pub const DX_HASH_LEGACY: u8 = 0;
    pub const DX_HASH_HALF_MD4: u8 = 1;
    pub const DX_HASH_TEA: u8 = 2;
    pub const DX_HASH_LEGACY_UNSIGNED: u8 = 3;
    pub const DX_HASH_HALF_MD4_UNSIGNED: u8 = 4;
    pub const DX_HASH_TEA_UNSIGNED: u8 = 5;

    pub fn parse(bytes: &[u8]) -> Result<Self> {
        require_len(bytes, 8)?;
        if read_u32_le(bytes, 0)? != 0 {
            return Err(Ext4FormatError::Corrupt);
        }
        let info = Self {
            hash_version: bytes[4],
            info_length: bytes[5],
            indirect_levels: bytes[6],
            flags: bytes[7],
        };
        if info.info_length < Self::INFO_LENGTH {
            return Err(Ext4FormatError::Corrupt);
        }
        Ok(info)
    }

    pub fn encode(&self, bytes: &mut [u8]) -> Result<()> {
        require_len(bytes, 8)?;
        bytes[..8].fill(0);
        bytes[4] = self.hash_version;
        bytes[5] = self.info_length;
        bytes[6] = self.indirect_levels;
        bytes[7] = self.flags;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DxCountLimit {
    pub limit: u16,
    pub count: u16,
}

impl DxCountLimit {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        require_len(bytes, 4)?;
        let value = Self {
            limit: read_u16_le(bytes, 0)?,
            count: read_u16_le(bytes, 2)?,
        };
        if value.count > value.limit {
            return Err(Ext4FormatError::Corrupt);
        }
        Ok(value)
    }

    pub fn encode(&self, bytes: &mut [u8]) -> Result<()> {
        require_len(bytes, 4)?;
        if self.count > self.limit {
            return Err(Ext4FormatError::Corrupt);
        }
        write_u16_le(bytes, 0, self.limit)?;
        write_u16_le(bytes, 2, self.count)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DxEntry {
    pub hash: u32,
    pub block: u32,
}

impl DxEntry {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        require_len(bytes, 8)?;
        Ok(Self {
            hash: read_u32_le(bytes, 0)?,
            block: read_u32_le(bytes, 4)?,
        })
    }

    pub fn encode(&self, bytes: &mut [u8]) -> Result<()> {
        require_len(bytes, 8)?;
        write_u32_le(bytes, 0, self.hash)?;
        write_u32_le(bytes, 4, self.block)?;
        Ok(())
    }
}

pub struct DxEntryIter<'a> {
    data: &'a [u8],
    remaining: usize,
}

impl<'a> DxEntryIter<'a> {
    pub fn new(data: &'a [u8], count: usize) -> Self {
        Self {
            data,
            remaining: count,
        }
    }
}

impl Iterator for DxEntryIter<'_> {
    type Item = Result<DxEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        if self.data.len() < 8 {
            self.remaining = 0;
            return Some(Err(Ext4FormatError::Truncated));
        }
        let entry = DxEntry::parse(&self.data[..8]);
        self.data = &self.data[8..];
        self.remaining -= 1;
        Some(entry)
    }
}

pub fn dx_hash(name: &[u8], hash_version: u8, seed: &[u32; 4]) -> Result<u32> {
    match hash_version {
        DxRootInfo::DX_HASH_LEGACY | DxRootInfo::DX_HASH_LEGACY_UNSIGNED => Ok(legacy_hash(name)),
        DxRootInfo::DX_HASH_HALF_MD4 | DxRootInfo::DX_HASH_HALF_MD4_UNSIGNED => {
            Ok(half_md4_compat_hash(name, seed))
        }
        DxRootInfo::DX_HASH_TEA | DxRootInfo::DX_HASH_TEA_UNSIGNED => {
            Ok(tea_compat_hash(name, seed))
        }
        _ => Err(Ext4FormatError::Unsupported),
    }
}

fn legacy_hash(name: &[u8]) -> u32 {
    let mut hash = 0u32;
    for byte in name {
        hash = hash.wrapping_mul(33).wrapping_add(*byte as u32);
    }
    hash
}

fn half_md4_compat_hash(name: &[u8], seed: &[u32; 4]) -> u32 {
    let mut hash = seed[0];
    for byte in name {
        hash = hash.wrapping_mul(1_103_515_245).wrapping_add(*byte as u32);
    }
    hash
}

fn tea_compat_hash(name: &[u8], seed: &[u32; 4]) -> u32 {
    let mut hash = seed[0];
    let mut buf = [0u32; 4];
    for chunk in name.chunks(16) {
        buf.fill(0);
        for (idx, bytes) in chunk.chunks(4).enumerate() {
            let mut value = 0u32;
            for byte in bytes {
                value = (value << 8) | *byte as u32;
            }
            buf[idx] = value;
        }
        for _ in 0..4 {
            hash = hash.wrapping_add(buf[0] ^ buf[1]);
        }
    }
    hash
}

pub struct BitmapView<'a> {
    bytes: &'a [u8],
}

impl<'a> BitmapView<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    pub fn is_set(&self, bit: usize) -> bool {
        let byte = bit / 8;
        let mask = 1u8 << (bit % 8);
        self.bytes
            .get(byte)
            .map(|value| value & mask != 0)
            .unwrap_or(false)
    }

    pub fn first_zero(&self) -> Option<usize> {
        self.bytes.iter().enumerate().find_map(|(byte_idx, byte)| {
            if *byte == 0xFF {
                None
            } else {
                (0..8)
                    .find(|bit| byte & (1u8 << bit) == 0)
                    .map(|bit| byte_idx * 8 + bit)
            }
        })
    }
}

pub struct BitmapMut<'a> {
    bytes: &'a mut [u8],
}

impl<'a> BitmapMut<'a> {
    pub fn new(bytes: &'a mut [u8]) -> Self {
        Self { bytes }
    }

    pub fn is_set(&self, bit: usize) -> bool {
        BitmapView::new(self.bytes).is_set(bit)
    }

    pub fn set(&mut self, bit: usize) -> Result<()> {
        let (byte, mask) = self.byte_mask(bit)?;
        self.bytes[byte] |= mask;
        Ok(())
    }

    pub fn clear(&mut self, bit: usize) -> Result<()> {
        let (byte, mask) = self.byte_mask(bit)?;
        self.bytes[byte] &= !mask;
        Ok(())
    }

    pub fn allocate_run(&mut self, len: usize) -> Result<usize> {
        if len == 0 {
            return Err(Ext4FormatError::OutOfBounds);
        }
        let total_bits = self.bytes.len() * 8;
        if len > total_bits {
            return Err(Ext4FormatError::OutOfBounds);
        }
        for start in 0..=total_bits - len {
            if (start..start + len).all(|bit| !self.is_set(bit)) {
                for bit in start..start + len {
                    self.set(bit)?;
                }
                return Ok(start);
            }
        }
        Err(Ext4FormatError::OutOfBounds)
    }

    fn byte_mask(&self, bit: usize) -> Result<(usize, u8)> {
        let byte = bit / 8;
        if byte >= self.bytes.len() {
            return Err(Ext4FormatError::OutOfBounds);
        }
        Ok((byte, 1u8 << (bit % 8)))
    }
}

pub fn encode_journal_descriptor(sequence: u32, target_block: u32, out: &mut [u8]) -> Result<()> {
    require_len(out, 20)?;
    out.fill(0);
    JournalHeader {
        magic: JBD2_MAGIC,
        block_type: JBD2_BLOCK_DESCRIPTOR,
        sequence,
    }
    .encode(&mut out[..12])?;
    JournalBlockTag {
        block: target_block,
        flags: JournalBlockTag::FLAG_LAST_TAG,
    }
    .encode(&mut out[12..20])?;
    Ok(())
}

pub fn parse_journal_descriptor(bytes: &[u8]) -> Result<(JournalHeader, JournalBlockTag)> {
    require_len(bytes, 20)?;
    let header = JournalHeader::parse(&bytes[..12])?;
    if header.block_type != JBD2_BLOCK_DESCRIPTOR {
        return Err(Ext4FormatError::Corrupt);
    }
    Ok((header, JournalBlockTag::parse(&bytes[12..20])?))
}

pub fn encode_journal_commit(sequence: u32, out: &mut [u8]) -> Result<()> {
    CommitHeader {
        sequence,
        seconds: 0,
        nanoseconds: 0,
    }
    .encode(out)
}

pub fn crc32c(seed: u32, bytes: &[u8]) -> u32 {
    !crc32c_append(!seed, bytes)
}

pub fn crc32c_append(mut crc: u32, bytes: &[u8]) -> u32 {
    for byte in bytes {
        crc ^= *byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0x82F6_3B78 & mask);
        }
    }
    crc
}

pub fn metadata_csum32(seed: u32, parts: &[&[u8]]) -> u32 {
    let mut crc = seed;
    for part in parts {
        crc = crc32c_append(crc, part);
    }
    crc
}

pub fn superblock_csum32(superblock_bytes: &[u8]) -> Result<u32> {
    require_len(superblock_bytes, 1024)?;
    Ok(crc32c_append(0xFFFF_FFFF, &superblock_bytes[..1020]))
}

pub fn group_desc_csum16(seed: u32, group_id: u32, desc_bytes: &[u8]) -> u16 {
    let group = group_id.to_le_bytes();
    (metadata_csum32(seed, &[&group, desc_bytes]) & 0xFFFF) as u16
}

pub fn block_bitmap_csum32(seed: u32, bitmap_bytes: &[u8], block_count: u32) -> Result<u32> {
    let byte_count = (block_count as usize)
        .checked_add(7)
        .ok_or(Ext4FormatError::OutOfBounds)?
        / 8;
    Ok(crc32c_append(seed, slice_at(bitmap_bytes, 0, byte_count)?))
}

pub fn inode_bitmap_csum32(seed: u32, bitmap_bytes: &[u8], inode_count: u32) -> Result<u32> {
    let byte_count = (inode_count as usize)
        .checked_add(7)
        .ok_or(Ext4FormatError::OutOfBounds)?
        / 8;
    Ok(crc32c_append(seed, slice_at(bitmap_bytes, 0, byte_count)?))
}

pub fn inode_csum32(
    seed: u32,
    inode_number: u32,
    generation: u32,
    inode_bytes: &[u8],
) -> Result<u32> {
    require_len(inode_bytes, 128)?;
    let inode_number = inode_number.to_le_bytes();
    let generation = generation.to_le_bytes();
    let zero = [0u8; 2];
    let checksum_hi_offset = 130.min(inode_bytes.len());
    let mut checksum = metadata_csum32(
        seed,
        &[
            &inode_number,
            &generation,
            &inode_bytes[..124],
            &zero,
            &inode_bytes[126..checksum_hi_offset],
        ],
    );
    if inode_bytes.len() >= 132 {
        checksum = metadata_csum32(checksum, &[&zero, &inode_bytes[132..]]);
    } else {
        checksum = crc32c_append(checksum, &inode_bytes[checksum_hi_offset..]);
    }
    Ok(checksum)
}

pub fn dirblock_csum32(seed: u32, inode: u32, generation: u32, block_bytes: &[u8]) -> u32 {
    let inode = inode.to_le_bytes();
    let generation = generation.to_le_bytes();
    metadata_csum32(seed, &[&inode, &generation, block_bytes])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalHeader {
    pub magic: u32,
    pub block_type: u32,
    pub sequence: u32,
}

impl JournalHeader {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        require_len(bytes, 12)?;
        let header = Self {
            magic: read_u32_be(bytes, 0)?,
            block_type: read_u32_be(bytes, 4)?,
            sequence: read_u32_be(bytes, 8)?,
        };
        if header.magic != JBD2_MAGIC {
            return Err(Ext4FormatError::BadMagic);
        }
        Ok(header)
    }

    pub fn encode(&self, bytes: &mut [u8]) -> Result<()> {
        require_len(bytes, 12)?;
        write_u32_be(bytes, 0, self.magic)?;
        write_u32_be(bytes, 4, self.block_type)?;
        write_u32_be(bytes, 8, self.sequence)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalBlockTag {
    pub block: u32,
    pub flags: u16,
}

impl JournalBlockTag {
    pub const FLAG_LAST_TAG: u16 = 0x0008;

    pub fn parse(bytes: &[u8]) -> Result<Self> {
        require_len(bytes, 8)?;
        Ok(Self {
            block: read_u32_be(bytes, 0)?,
            flags: read_u16_be(bytes, 6)?,
        })
    }

    pub fn encode(&self, bytes: &mut [u8]) -> Result<()> {
        require_len(bytes, 8)?;
        write_u32_be(bytes, 0, self.block)?;
        write_u16_be(bytes, 4, 0)?;
        write_u16_be(bytes, 6, self.flags)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommitHeader {
    pub sequence: u32,
    pub seconds: u32,
    pub nanoseconds: u32,
}

impl CommitHeader {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        require_len(bytes, 32)?;
        let header = JournalHeader::parse(&bytes[..12])?;
        if header.block_type != JBD2_BLOCK_COMMIT {
            return Err(Ext4FormatError::Corrupt);
        }
        Ok(Self {
            sequence: header.sequence,
            seconds: read_u32_be(bytes, 24)?,
            nanoseconds: read_u32_be(bytes, 28)?,
        })
    }

    pub fn encode(&self, bytes: &mut [u8]) -> Result<()> {
        require_len(bytes, 32)?;
        bytes.fill(0);
        JournalHeader {
            magic: JBD2_MAGIC,
            block_type: JBD2_BLOCK_COMMIT,
            sequence: self.sequence,
        }
        .encode(&mut bytes[..12])?;
        write_u32_be(bytes, 24, self.seconds)?;
        write_u32_be(bytes, 28, self.nanoseconds)?;
        Ok(())
    }
}

pub(crate) fn read_u16_le(bytes: &[u8], offset: usize) -> Result<u16> {
    Ok(u16::from_le_bytes(
        slice_at(bytes, offset, 2)?.try_into().unwrap(),
    ))
}

fn read_u16_le_optional(bytes: &[u8], offset: usize) -> Option<u16> {
    bytes
        .get(offset..offset + 2)
        .map(|value| u16::from_le_bytes(value.try_into().unwrap()))
}

pub(crate) fn read_u32_le(bytes: &[u8], offset: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(
        slice_at(bytes, offset, 4)?.try_into().unwrap(),
    ))
}

fn read_u32_le_optional(bytes: &[u8], offset: usize) -> Option<u32> {
    bytes
        .get(offset..offset + 4)
        .map(|value| u32::from_le_bytes(value.try_into().unwrap()))
}

fn read_u16_be(bytes: &[u8], offset: usize) -> Result<u16> {
    Ok(u16::from_be_bytes(
        slice_at(bytes, offset, 2)?.try_into().unwrap(),
    ))
}

fn read_u32_be(bytes: &[u8], offset: usize) -> Result<u32> {
    Ok(u32::from_be_bytes(
        slice_at(bytes, offset, 4)?.try_into().unwrap(),
    ))
}

pub(crate) fn write_u16_le(bytes: &mut [u8], offset: usize, value: u16) -> Result<()> {
    slice_at_mut(bytes, offset, 2)?.copy_from_slice(&value.to_le_bytes());
    Ok(())
}

pub(crate) fn write_u32_le(bytes: &mut [u8], offset: usize, value: u32) -> Result<()> {
    slice_at_mut(bytes, offset, 4)?.copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn write_u16_be(bytes: &mut [u8], offset: usize, value: u16) -> Result<()> {
    slice_at_mut(bytes, offset, 2)?.copy_from_slice(&value.to_be_bytes());
    Ok(())
}

fn write_u32_be(bytes: &mut [u8], offset: usize, value: u32) -> Result<()> {
    slice_at_mut(bytes, offset, 4)?.copy_from_slice(&value.to_be_bytes());
    Ok(())
}

fn require_len(bytes: &[u8], len: usize) -> Result<()> {
    if bytes.len() < len {
        Err(Ext4FormatError::Truncated)
    } else {
        Ok(())
    }
}

fn prepare_extent_node(depth: u16, entries: usize, bytes: &mut [u8]) -> Result<()> {
    require_len(bytes, 12 + entries * 12)?;
    let max = (bytes.len().saturating_sub(12) / 12) as u16;
    if entries > max as usize {
        return Err(Ext4FormatError::Unsupported);
    }
    bytes.fill(0);
    ExtentHeader {
        entries: entries as u16,
        max,
        depth,
        generation: 0,
    }
    .encode(&mut bytes[..12])
}

fn div_ceil_u64(value: u64, divisor: u64) -> u64 {
    if value == 0 {
        0
    } else {
        1 + (value - 1) / divisor
    }
}

fn slice_at(bytes: &[u8], offset: usize, len: usize) -> Result<&[u8]> {
    bytes
        .get(offset..offset + len)
        .ok_or(Ext4FormatError::Truncated)
}

fn slice_at_mut(bytes: &mut [u8], offset: usize, len: usize) -> Result<&mut [u8]> {
    bytes
        .get_mut(offset..offset + len)
        .ok_or(Ext4FormatError::Truncated)
}
