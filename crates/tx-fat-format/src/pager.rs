//! FAT pager: BlockImage trait and FatPager.
//!
//! `FatPager<I: BlockImage>` is the format-level pager that consumes
//! a `BlockImage` to read/write logical blocks (512 bytes), walk FAT
//! chains, and enumerate directory entries.

use alloc::vec::Vec;

use crate::ondisk::{
    self, BPB, BPBParseError, DirEntry, FatType, LFNDirEntry, ATTR_LFN, ATTR_LFN_MASK,
    DIR_ENTRY_FREE, DIR_ENTRY_LAST, DIR_ENTRY_SIZE, LFN_LAST_MASK, LFN_MAX_ENTRIES,
};

// ====================================================================
// BlockImage trait
// ====================================================================

/// Logical block size (always 512 bytes for FAT).
pub const BLOCK_SIZE: usize = 512;
pub type Page4K = [u8; BLOCK_SIZE];

/// Maximum length of a decoded LFN in bytes (UTF-8).
/// 260 UCS-2 chars × 3 bytes max per UTF-8 sequence = 780.
pub const MAX_LFN_LENGTH: usize = 780;

/// A block-level storage interface, identical in semantics to
/// `tx_ext4_format::pager::BlockImage` but independent so that
/// `tx-fat-format` has no dependency on `tx-ext4-format`.
pub trait BlockImage {
    /// Total number of logical blocks (sectors).
    fn total_blocks(&self) -> u64;
    /// Read one logical block into `out`.
    fn read_block(&self, block: u64, out: &mut Page4K) -> Result<()>;
    /// Write one logical block from `data`.
    fn write_block(&mut self, block: u64, data: &Page4K) -> Result<()>;
    /// Optional write barrier.
    fn barrier(&mut self) -> Result<()> {
        Ok(())
    }
}

// ====================================================================
// FatFormatError
// ====================================================================

/// Format-level error for FAT pager operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FatFormatError {
    /// BPB parsing failed.
    BadBPB(BPBParseError),
    /// On-disk data is corrupt or unexpected.
    Corrupt,
    /// Operation would read past end of volume.
    OutOfBounds,
    /// Feature not supported (e.g. write on read-only image).
    Unsupported,
    /// I/O error from the block device.
    IO,
    /// File too large (> 4 GB for FAT32).
    FileTooLarge,
    /// Entry not found.
    NotFound,
}

impl From<BPBParseError> for FatFormatError {
    fn from(e: BPBParseError) -> Self {
        FatFormatError::BadBPB(e)
    }
}

pub type Result<T> = core::result::Result<T, FatFormatError>;

// ====================================================================
// FatPager
// ====================================================================

/// A `DirEntryLite` is a simplified directory entry for consumers
/// (e.g. the kernel adapter). It carries the short name (raw 11 bytes),
/// the decoded LFN (if any), the first cluster, the file size, and the
/// attribute byte.
#[derive(Debug, Clone)]
pub struct DirEntryLite {
    /// Raw 8.3 short name (11 bytes, space-padded).
    pub short_name: [u8; 11],
    /// Decoded VFAT long filename as UTF-8 bytes (empty if 8.3 only).
    pub lfn_utf8: Vec<u8>,
    /// Attribute byte from the 8.3 entry.
    pub attr: u8,
    /// First cluster number.
    pub first_cluster: u32,
    /// File size in bytes.
    pub size: u32,
    /// Write date (for mtime computation).
    pub write_date: u16,
    /// Write time (for mtime computation).
    pub write_time: u16,
}

impl DirEntryLite {
    /// Returns an empty sentinel entry.
    pub const fn empty() -> Self {
        Self {
            short_name: [0; 11],
            lfn_utf8: Vec::new(),
            attr: 0,
            first_cluster: 0,
            size: 0,
            write_date: 0,
            write_time: 0,
        }
    }

    /// The best name to display: LFN if present, otherwise trimmed 8.3 with dot.
    pub fn display_name(&self) -> &[u8] {
        if !self.lfn_utf8.is_empty() {
            &self.lfn_utf8
        } else {
            self.short_name_bytes()
        }
    }

    /// The 8.3 short name trimmed and rendered with a dot. Returns a
    /// slice of the short name for efficiency — callers that need the
    /// dot should compute it from the raw 11 bytes.
    fn short_name_bytes(&self) -> &[u8] {
        // Name portion (bytes 0-7), extension portion (bytes 8-10).
        // For now, return the full 11 bytes — callers format with dot.
        &self.short_name[..]
    }
}

/// The FAT pager: wraps a `BlockImage` with parsed BPB and cached FAT content.
pub struct FatPager<I> {
    image: I,
    pub bpb: BPB,
    /// Cached FAT table content for the active FAT.
    fat_cache: Vec<u8>,
}

impl<I: BlockImage> FatPager<I> {
    /// Open a FAT volume by reading sector 0 and parsing the BPB.
    /// For FAT12/16, the entire FAT is cached in memory.
    /// For FAT32, the entire first FAT is cached (on-demand for
    /// very large FATs would require a paging strategy; see design doc §8.2).
    pub fn open(image: I) -> Result<Self> {
        let mut sector0 = [0u8; BLOCK_SIZE];
        image.read_block(0, &mut sector0).map_err(|_| FatFormatError::IO)?;
        let bpb = BPB::parse(&sector0)?;

        // Read the active FAT into cache
        let fat_start = bpb.fat_start_sector() as u64;
        let fat_sectors = bpb.sectors_per_fat() as u64;
        let fat_bytes = fat_sectors * BLOCK_SIZE as u64;

        // Guard against overly large FAT tables
        if fat_bytes > 16 * 1024 * 1024 {
            // 16 MiB sanity limit
            return Err(FatFormatError::Corrupt);
        }

        let mut fat_cache = alloc::vec![0u8; fat_bytes as usize];
        for i in 0..fat_sectors {
            let sector = fat_start + i;
            let offset = (i as usize) * BLOCK_SIZE;
            image
                .read_block(sector, (&mut fat_cache[offset..offset + BLOCK_SIZE]).try_into().unwrap())
                .map_err(|_| FatFormatError::IO)?;
        }

        Ok(Self { image, bpb, fat_cache })
    }

    /// Read a raw FAT entry for a cluster.
    ///
    /// Returns the entry value (unmasked for the FAT type).  When the
    /// cluster is out of range for the cached FAT, returns an
    /// end-of-chain sentinel so callers treat it as a terminal cluster
    /// rather than a dangling free entry (0).
    pub fn read_fat_entry(&self, cluster: u32) -> u32 {
        let fat_type = self.bpb.fat_type;
        let offset = match fat_type {
            FatType::FAT12 => (cluster as usize * 3) / 2,
            FatType::FAT16 => cluster as usize * 2,
            FatType::FAT32 => cluster as usize * 4,
        };

        // Out-of-range cluster — return EOC sentinel so callers
        // don't interpret a free-cluster (0) as valid.
        if offset + 4 > self.fat_cache.len() {
            return match fat_type {
                FatType::FAT12 => 0xFFF,
                FatType::FAT16 => 0xFFFF,
                FatType::FAT32 => 0x0FFF_FFFF,
            };
        }

        let raw = u32::from_le_bytes([
            self.fat_cache[offset],
            self.fat_cache[offset + 1],
            self.fat_cache[offset + 2],
            self.fat_cache[offset + 3],
        ]);

        // FAT12: if cluster is odd, shift right by 4
        match fat_type {
            FatType::FAT12 => {
                if cluster & 1 != 0 {
                    raw >> 4
                } else {
                    raw & 0x0FFF
                }
            }
            _ => ondisk::fat_entry(raw, fat_type),
        }
    }

    /// Write a FAT entry and flush the affected sector(s) to both
    /// FAT copies on disk.
    fn write_fat_entry(&mut self, cluster: u32, value: u32) -> Result<()> {
        let fat_type = self.bpb.fat_type;
        let offset = match fat_type {
            FatType::FAT12 => (cluster as usize * 3) / 2,
            FatType::FAT16 => cluster as usize * 2,
            FatType::FAT32 => cluster as usize * 4,
        };

        if offset + 4 > self.fat_cache.len() {
            return Err(FatFormatError::OutOfBounds);
        }

        match fat_type {
            FatType::FAT12 => {
                // Read the existing 16-bit value, update 12 bits.
                let old = u16::from_le_bytes([self.fat_cache[offset], self.fat_cache[offset + 1]]);
                let new = if cluster & 1 != 0 {
                    // Odd cluster: high 12 bits. Preserve low nibble of
                    // the next entry (or zero it).
                    (old & 0x000F) | ((value as u16) << 4)
                } else {
                    // Even cluster: low 12 bits. Preserve high nibble.
                    (old & 0xF000) | (value as u16 & 0x0FFF)
                };
                self.fat_cache[offset..offset + 2].copy_from_slice(&new.to_le_bytes());
            }
            FatType::FAT16 => {
                self.fat_cache[offset..offset + 2]
                    .copy_from_slice(&(value as u16).to_le_bytes());
            }
            FatType::FAT32 => {
                // FAT32 uses 28-bit entries; preserve high 4 bits.
                let old = u32::from_le_bytes([
                    self.fat_cache[offset],
                    self.fat_cache[offset + 1],
                    self.fat_cache[offset + 2],
                    self.fat_cache[offset + 3],
                ]);
                let new = (old & 0xF000_0000) | (value & 0x0FFF_FFFF);
                self.fat_cache[offset..offset + 4].copy_from_slice(&new.to_le_bytes());
            }
        }

        // Flush the affected sector(s) to both FAT copies.
        let fat_start = self.bpb.fat_start_sector() as u64;
        let fat_sectors = self.bpb.sectors_per_fat() as u64;
        let sector = fat_start + (offset as u64 / BLOCK_SIZE as u64);
        let sector_off = offset as u64 % BLOCK_SIZE as u64;

        for copy in 0..self.bpb.num_fats as u64 {
            let target_sector = sector + copy * fat_sectors;
            let start = (sector_off / BLOCK_SIZE as u64) as usize * BLOCK_SIZE;
            // Read-modify-write the sector so we don't clobber
            // adjacent entries.
            let mut sec = [0u8; BLOCK_SIZE];
            self.image.read_block(target_sector, &mut sec)?;
            let in_sector = offset - start;
            let write_len = match fat_type {
                FatType::FAT12 => 2,
                FatType::FAT16 => 2,
                FatType::FAT32 => 4,
            };
            sec[in_sector..in_sector + write_len]
                .copy_from_slice(&self.fat_cache[offset..offset + write_len]);
            self.image.write_block(target_sector, &sec)?;
        }

        Ok(())
    }

    /// Find a free cluster (FAT entry == 0) starting from cluster 2.
    fn find_free_cluster(&self) -> Result<u32> {
        let total = self.bpb.total_clusters();
        for cluster in 2..(total + 2) {
            let entry = ondisk::fat_entry(self.read_fat_entry(cluster), self.bpb.fat_type);
            if entry == 0 {
                return Ok(cluster);
            }
        }
        Err(FatFormatError::OutOfBounds) // disk full
    }

    /// Free an entire cluster chain by marking every entry as 0
    /// (free).  Does nothing if cluster is 0 (empty file).
    pub fn free_cluster_chain(&mut self, start_cluster: u32) -> Result<()> {
        if start_cluster == 0 {
            return Ok(());
        }
        let mut cluster = start_cluster;
        let fat_type = self.bpb.fat_type;
        let max_iter = 4_194_304; // safety cap
        let mut count = 0;

        loop {
            count += 1;
            if count > max_iter {
                return Err(FatFormatError::Corrupt);
            }
            let next = ondisk::fat_entry(self.read_fat_entry(cluster), fat_type);
            self.write_fat_entry(cluster, 0)?;
            if ondisk::is_eoc(next, fat_type) {
                break;
            }
            if ondisk::is_bad(next, fat_type) {
                return Err(FatFormatError::Corrupt);
            }
            cluster = next;
        }
        Ok(())
    }

    /// Rename a directory entry in a subdirectory by updating its
    /// 11-byte short name.
    pub fn rename_dirent_in_subdir(
        &mut self,
        parent_cluster: u32,
        entry_index: u32,
        new_short_name: &[u8; 11],
    ) -> Result<()> {
        let chain = self.walk_fat_chain(parent_cluster)?;
        let cluster_size = self.bpb.bytes_per_cluster() as usize;
        let total_len = chain.len() * cluster_size;
        let mut buf = alloc::vec![0u8; total_len];

        for (i, &cluster) in chain.iter().enumerate() {
            let offset = i * cluster_size;
            self.read_cluster(cluster, &mut buf[offset..offset + cluster_size])?;
        }

        let byte_offset = entry_index as usize * DIR_ENTRY_SIZE;
        if byte_offset + 11 > buf.len() {
            return Err(FatFormatError::OutOfBounds);
        }
        buf[byte_offset..byte_offset + 11].copy_from_slice(new_short_name);

        for (i, &cluster) in chain.iter().enumerate() {
            let offset = i * cluster_size;
            self.write_cluster(cluster, &buf[offset..offset + cluster_size])?;
        }
        Ok(())
    }

    /// Rename a directory entry in the FAT12/16 root dir by updating
    /// its 11-byte short name.
    pub fn rename_dirent_in_root(
        &mut self,
        entry_index: u32,
        new_short_name: &[u8; 11],
    ) -> Result<()> {
        let root_start = self.bpb.root_dir_start_sector() as u64;
        let root_sectors = self.bpb.root_dir_sectors() as u64;
        let root_bytes = (root_sectors as usize) * BLOCK_SIZE;
        let mut buf = alloc::vec![0u8; root_bytes];
        self.read_blocks(root_start, &mut buf)?;

        let byte_offset = entry_index as usize * DIR_ENTRY_SIZE;
        if byte_offset + 11 > buf.len() {
            return Err(FatFormatError::OutOfBounds);
        }
        buf[byte_offset..byte_offset + 11].copy_from_slice(new_short_name);

        for i in 0..root_sectors {
            let start = i as usize * BLOCK_SIZE;
            let end = start + BLOCK_SIZE;
            let mut sector = [0u8; BLOCK_SIZE];
            sector.copy_from_slice(&buf[start..end]);
            self.image
                .write_block(root_start + i, &sector)
                .map_err(|_| FatFormatError::IO)?;
        }
        Ok(())
    }

    /// Mark a directory entry as deleted by writing 0xE5 as its first
    /// byte.  Works for both subdirectory and root directory entries.
    /// `entry_index` is the 0-based index in the raw directory buffer.
    pub fn delete_dirent_in_subdir(
        &mut self,
        parent_cluster: u32,
        entry_index: u32,
    ) -> Result<()> {
        let chain = self.walk_fat_chain(parent_cluster)?;
        let cluster_size = self.bpb.bytes_per_cluster() as usize;
        let total_len = chain.len() * cluster_size;
        let mut buf = alloc::vec![0u8; total_len];

        for (i, &cluster) in chain.iter().enumerate() {
            let offset = i * cluster_size;
            self.read_cluster(cluster, &mut buf[offset..offset + cluster_size])?;
        }

        let byte_offset = entry_index as usize * DIR_ENTRY_SIZE;
        if byte_offset >= buf.len() {
            return Err(FatFormatError::OutOfBounds);
        }
        buf[byte_offset] = 0xE5;

        for (i, &cluster) in chain.iter().enumerate() {
            let offset = i * cluster_size;
            self.write_cluster(cluster, &buf[offset..offset + cluster_size])?;
        }
        Ok(())
    }

    /// Mark a directory entry as deleted in the FAT12/16 root dir.
    pub fn delete_dirent_in_root(&mut self, entry_index: u32) -> Result<()> {
        let root_start = self.bpb.root_dir_start_sector() as u64;
        let root_sectors = self.bpb.root_dir_sectors() as u64;
        let root_bytes = (root_sectors as usize) * BLOCK_SIZE;
        let mut buf = alloc::vec![0u8; root_bytes];
        self.read_blocks(root_start, &mut buf)?;

        let byte_offset = entry_index as usize * DIR_ENTRY_SIZE;
        if byte_offset >= buf.len() {
            return Err(FatFormatError::OutOfBounds);
        }
        buf[byte_offset] = 0xE5;

        for i in 0..root_sectors {
            let start = i as usize * BLOCK_SIZE;
            let end = start + BLOCK_SIZE;
            let mut sector = [0u8; BLOCK_SIZE];
            sector.copy_from_slice(&buf[start..end]);
            self.image
                .write_block(root_start + i, &sector)
                .map_err(|_| FatFormatError::IO)?;
        }
        Ok(())
    }

    /// Allocate a single cluster: find a free one, mark it EOC, and
    /// return the cluster number.
    pub fn alloc_cluster(&mut self) -> Result<u32> {
        let cluster = self.find_free_cluster()?;
        let eoc = match self.bpb.fat_type {
            FatType::FAT12 => 0xFFF,
            FatType::FAT16 => 0xFFFF,
            FatType::FAT32 => 0x0FFF_FFFF,
        };
        self.write_fat_entry(cluster, eoc)?;
        // Zero out the cluster data.
        let cluster_size = self.bpb.bytes_per_cluster() as usize;
        let zeros = alloc::vec![0u8; cluster_size];
        self.write_cluster(cluster, &zeros)?;
        Ok(cluster)
    }

    /// Walk the FAT chain starting from `first_cluster`, collecting all
    /// cluster numbers in order. Returns `Ok(vec![clusters])` or
    /// `Err` on a bad cluster or infinite loop (sanity cap at 4M clusters).
    pub fn walk_fat_chain(&self, first_cluster: u32) -> Result<Vec<u32>> {
        let fat_type = self.bpb.fat_type;
        let mut chain = Vec::new();
        let mut cluster = first_cluster;
        let max_clusters = 4_194_304; // 4M × 4KB = 16 GiB sanity cap

        loop {
            if ondisk::is_bad(cluster, fat_type) {
                return Err(FatFormatError::Corrupt);
            }
            if !(2..self.bpb.total_clusters() + 2).contains(&cluster) {
                return Err(FatFormatError::OutOfBounds);
            }
            chain.push(cluster);
            if chain.len() > max_clusters {
                return Err(FatFormatError::Corrupt); // infinite loop guard
            }
            let next = self.read_fat_entry(cluster);
            if ondisk::is_eoc(next, fat_type) {
                break;
            }
            cluster = next;
        }
        Ok(chain)
    }

    /// Read `count` logical blocks starting from `lba` into `buf`.
    pub fn read_blocks(&self, lba: u64, buf: &mut [u8]) -> Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        let block_count = buf.len().div_ceil(BLOCK_SIZE);
        for i in 0..block_count {
            let start = i * BLOCK_SIZE;
            let end = (start + BLOCK_SIZE).min(buf.len());
            let mut sector = [0u8; BLOCK_SIZE];
            self.image
                .read_block(lba + i as u64, &mut sector)
                .map_err(|_| FatFormatError::IO)?;
            let len = end - start;
            buf[start..end].copy_from_slice(&sector[..len]);
        }
        Ok(())
    }

    /// Write one cluster from `buf`. `buf` must provide at least
    /// `bytes_per_cluster` bytes; only the first `bytes_per_cluster`
    /// bytes are written.
    pub fn write_cluster(&mut self, cluster: u32, buf: &[u8]) -> Result<()> {
        let cluster_size = self.bpb.bytes_per_cluster() as usize;
        if buf.len() < cluster_size {
            return Err(FatFormatError::OutOfBounds);
        }
        let lba = self.cluster_to_lba(cluster)?;
        let block_count = cluster_size.div_ceil(BLOCK_SIZE);
        for i in 0..block_count {
            let start = i * BLOCK_SIZE;
            let end = (start + BLOCK_SIZE).min(cluster_size);
            let mut sector = [0u8; BLOCK_SIZE];
            let len = end - start;
            sector[..len].copy_from_slice(&buf[start..end]);
            self.image
                .write_block(lba + i as u64, &sector)
                .map_err(|_| FatFormatError::IO)?;
        }
        Ok(())
    }

    /// Read one cluster into `buf`. `buf` must be at least `bytes_per_cluster` bytes.
    pub fn read_cluster(&self, cluster: u32, buf: &mut [u8]) -> Result<()> {
        let cluster_size = self.bpb.bytes_per_cluster() as usize;
        if buf.len() < cluster_size {
            return Err(FatFormatError::OutOfBounds);
        }
        let lba = self.cluster_to_lba(cluster)?;
        self.read_blocks(lba, &mut buf[..cluster_size])
    }

    /// Convert a logical cluster number to an LBA (sector index).
    pub fn cluster_to_lba(&self, cluster: u32) -> Result<u64> {
        if cluster < 2 {
            return Err(FatFormatError::OutOfBounds);
        }
        let data_start = self.bpb.data_start_sector() as u64;
        let cluster_offset = (cluster - 2) as u64 * self.bpb.sectors_per_cluster as u64;
        Ok(data_start + cluster_offset)
    }

    /// Read all blocks in a cluster chain into `buf`, which must be
    /// large enough to hold all cluster data. Returns the number of
    /// bytes valid in `buf` (the file size may be less than the
    /// allocated cluster chain).
    pub fn read_cluster_chain(
        &self,
        chain: &[u32],
        file_size: u32,
        buf: &mut [u8],
    ) -> Result<u32> {
        let cluster_size = self.bpb.bytes_per_cluster() as usize;
        let total_bytes = (chain.len() * cluster_size).min(buf.len());
        for (i, &cluster) in chain.iter().enumerate() {
            let offset = i * cluster_size;
            let end = (offset + cluster_size).min(total_bytes);
            self.read_cluster(cluster, &mut buf[offset..end])?;
        }
        // Truncate to file size — the last cluster may be partially used.
        let valid = file_size.min(total_bytes as u32);
        Ok(valid)
    }

    // === Directory reading ===

    /// Read directory entries from a cluster chain.
    ///
    /// For the FAT12/16 root directory (which is not in a cluster chain),
    /// call `read_root_dir_entries` instead.
    ///
    /// Returns a `Vec<DirEntryLite>` with decoded LFN names where present.
    pub fn read_dir_entries(&self, first_cluster: u32) -> Result<Vec<DirEntryLite>> {
        let chain = self.walk_fat_chain(first_cluster)?;
        let total_len = chain.len() * self.bpb.bytes_per_cluster() as usize;
        let mut buf = alloc::vec![0u8; total_len];

        for (i, &cluster) in chain.iter().enumerate() {
            let cluster_size = self.bpb.bytes_per_cluster() as usize;
            let offset = i * cluster_size;
            self.read_cluster(cluster, &mut buf[offset..offset + cluster_size])?;
        }

        Ok(self.parse_dir_buffer(&buf))
    }

    /// Append a new 8.3 directory entry to a subdirectory.  Returns
    /// the entry index (0-based) within the raw directory buffer.
    pub fn append_dirent_in_subdir(
        &mut self,
        parent_cluster: u32,
        short_name: &[u8; 11],
        attr: u8,
        first_cluster: u32,
        size: u32,
        write_date: u16,
        write_time: u16,
    ) -> Result<u32> {
        let chain = self.walk_fat_chain(parent_cluster)?;
        let cluster_size = self.bpb.bytes_per_cluster() as usize;
        let total_len = chain.len() * cluster_size;
        let mut buf = alloc::vec![0u8; total_len];

        for (i, &cluster) in chain.iter().enumerate() {
            let offset = i * cluster_size;
            self.read_cluster(cluster, &mut buf[offset..offset + cluster_size])?;
        }

        let (entry_index, wrote) = Self::write_dirent_into_buf(
            &mut buf, short_name, attr, first_cluster, size, write_date, write_time,
        );
        if !wrote {
            return Err(FatFormatError::OutOfBounds); // directory full
        }

        // Write back.
        for (i, &cluster) in chain.iter().enumerate() {
            let offset = i * cluster_size;
            self.write_cluster(cluster, &buf[offset..offset + cluster_size])?;
        }

        Ok(entry_index)
    }

    /// Append a new 8.3 directory entry to the FAT12/16 root directory.
    pub fn append_dirent_in_root(
        &mut self,
        short_name: &[u8; 11],
        attr: u8,
        first_cluster: u32,
        size: u32,
        write_date: u16,
        write_time: u16,
    ) -> Result<u32> {
        let root_start = self.bpb.root_dir_start_sector() as u64;
        let root_sectors = self.bpb.root_dir_sectors() as u64;
        let root_bytes = (root_sectors as usize) * BLOCK_SIZE;
        let mut buf = alloc::vec![0u8; root_bytes];
        self.read_blocks(root_start, &mut buf)?;

        let (entry_index, wrote) = Self::write_dirent_into_buf(
            &mut buf, short_name, attr, first_cluster, size, write_date, write_time,
        );
        if !wrote {
            return Err(FatFormatError::OutOfBounds);
        }

        // Write back contiguous root sectors.
        for i in 0..root_sectors {
            let start = i as usize * BLOCK_SIZE;
            let end = start + BLOCK_SIZE;
            let mut sector = [0u8; BLOCK_SIZE];
            sector.copy_from_slice(&buf[start..end]);
            self.image
                .write_block(root_start + i, &sector)
                .map_err(|_| FatFormatError::IO)?;
        }

        Ok(entry_index)
    }

    /// Find a free slot in `buf` and write an 8.3 directory entry.
    /// Returns `(entry_index, true)` on success or `(0, false)` if
    /// the directory is full.
    fn write_dirent_into_buf(
        buf: &mut [u8],
        short_name: &[u8; 11],
        attr: u8,
        first_cluster: u32,
        size: u32,
        write_date: u16,
        write_time: u16,
    ) -> (u32, bool) {
        let num_entries = buf.len() / DIR_ENTRY_SIZE;
        for i in 0..num_entries {
            let offset = i * DIR_ENTRY_SIZE;
            let first_byte = buf[offset];
            // Free (0xE5) or end-of-directory (0x00) slot.
            if first_byte != 0xE5 && first_byte != 0x00 {
                continue;
            }

            // Clear the entry.
            buf[offset..offset + DIR_ENTRY_SIZE].fill(0);

            // Write short name (11 bytes).
            buf[offset..offset + 11].copy_from_slice(short_name);

            // Attribute (offset 11).
            buf[offset + 11] = attr;

            // Reserved / NT byte (offset 12) = 0.

            // First cluster: lo at offset 26-27, hi at offset 20-21.
            let cluster_lo = (first_cluster & 0xFFFF) as u16;
            let cluster_hi = ((first_cluster >> 16) & 0xFFFF) as u16;
            buf[offset + 26..offset + 28].copy_from_slice(&cluster_lo.to_le_bytes());
            buf[offset + 20..offset + 22].copy_from_slice(&cluster_hi.to_le_bytes());

            // File size (offset 28-31).
            buf[offset + 28..offset + 32].copy_from_slice(&size.to_le_bytes());

            // Write time (offset 22-23).
            buf[offset + 22..offset + 24].copy_from_slice(&write_time.to_le_bytes());

            // Write date (offset 24-25).
            buf[offset + 24..offset + 26].copy_from_slice(&write_date.to_le_bytes());

            return (i as u32, true);
        }
        (0, false)
    }

    /// Update a directory entry in a subdirectory by searching for
    /// the entry whose `first_cluster` matches `target_cluster`.
    /// Updates `size`, `write_date`, and `write_time`.
    pub fn update_dirent_in_subdir(
        &mut self,
        parent_cluster: u32,
        target_cluster: u32,
        size: u32,
        write_date: u16,
        write_time: u16,
    ) -> Result<()> {
        let chain = self.walk_fat_chain(parent_cluster)?;
        let cluster_size = self.bpb.bytes_per_cluster() as usize;
        let total_len = chain.len() * cluster_size;
        let mut buf = alloc::vec![0u8; total_len];

        for (i, &cluster) in chain.iter().enumerate() {
            let offset = i * cluster_size;
            self.read_cluster(cluster, &mut buf[offset..offset + cluster_size])?;
        }

        if !Self::patch_dirent_in_buf(&mut buf, target_cluster, size, write_date, write_time) {
            return Err(FatFormatError::NotFound);
        }

        // Write back all clusters.
        for (i, &cluster) in chain.iter().enumerate() {
            let offset = i * cluster_size;
            self.write_cluster(cluster, &buf[offset..offset + cluster_size])?;
        }

        Ok(())
    }

    /// Update a directory entry in the FAT12/16 root directory region
    /// by searching for the entry whose `first_cluster` matches
    /// `target_cluster`.
    pub fn update_dirent_in_root(
        &mut self,
        target_cluster: u32,
        size: u32,
        write_date: u16,
        write_time: u16,
    ) -> Result<()> {
        let root_start = self.bpb.root_dir_start_sector() as u64;
        let root_sectors = self.bpb.root_dir_sectors() as u64;
        let root_bytes = (root_sectors as usize) * BLOCK_SIZE;
        let mut buf = alloc::vec![0u8; root_bytes];
        self.read_blocks(root_start, &mut buf)?;

        if !Self::patch_dirent_in_buf(&mut buf, target_cluster, size, write_date, write_time) {
            return Err(FatFormatError::NotFound);
        }

        // Write back: root dir is contiguous sectors.
        for i in 0..root_sectors {
            let start = i as usize * BLOCK_SIZE;
            let end = start + BLOCK_SIZE;
            let mut sector = [0u8; BLOCK_SIZE];
            sector.copy_from_slice(&buf[start..end]);
            self.image
                .write_block(root_start + i, &sector)
                .map_err(|_| FatFormatError::IO)?;
        }

        Ok(())
    }

    /// Search `buf` (a raw directory content buffer) for a dirent
    /// whose first cluster matches `target_cluster`, and update its
    /// size/time/date fields in-place. Returns `true` if found.
    fn patch_dirent_in_buf(
        buf: &mut [u8],
        target_cluster: u32,
        size: u32,
        write_date: u16,
        write_time: u16,
    ) -> bool {
        use crate::ondisk::DIR_ENTRY_SIZE;

        let num_entries = buf.len() / DIR_ENTRY_SIZE;
        for i in 0..num_entries {
            let offset = i * DIR_ENTRY_SIZE;
            let first_byte = buf[offset];
            if first_byte == 0x00 {
                break; // end of directory
            }
            if first_byte == 0xE5 {
                continue; // deleted entry
            }
            let attr = buf[offset + 11];
            // Skip LFN entries.
            if (attr & 0x3F) == 0x0F {
                continue;
            }
            // Volume label — skip.
            if attr & 0x08 != 0 {
                continue;
            }

            // Read first cluster from the dirent.
            let cluster_lo = u16::from_le_bytes([buf[offset + 26], buf[offset + 27]]) as u32;
            let cluster_hi = u16::from_le_bytes([buf[offset + 20], buf[offset + 21]]) as u32;
            let entry_cluster = (cluster_hi << 16) | cluster_lo;

            if entry_cluster == target_cluster {
                // Update size (offset 28-31, LE u32).
                buf[offset + 28..offset + 32].copy_from_slice(&size.to_le_bytes());
                // Update write time (offset 22-23, LE u16).
                buf[offset + 22..offset + 24].copy_from_slice(&write_time.to_le_bytes());
                // Update write date (offset 24-25, LE u16).
                buf[offset + 24..offset + 26].copy_from_slice(&write_date.to_le_bytes());
                return true;
            }
        }
        false
    }

    /// Read the FAT12/16 root directory (fixed region, not a cluster chain).
    pub fn read_root_dir_entries(&self) -> Result<Vec<DirEntryLite>> {
        if self.bpb.fat_type == FatType::FAT32 {
            return self.read_dir_entries(self.bpb.root_cluster);
        }
        let root_start = self.bpb.root_dir_start_sector() as u64;
        let root_sectors = self.bpb.root_dir_sectors() as u64;
        let root_bytes = (root_sectors as usize) * BLOCK_SIZE;
        let mut buf = alloc::vec![0u8; root_bytes];
        self.read_blocks(root_start, &mut buf)?;
        Ok(self.parse_dir_buffer(&buf))
    }

    /// Parse a raw directory buffer into `DirEntryLite` entries,
    /// assembling LFN fragments.
    fn parse_dir_buffer(&self, buf: &[u8]) -> Vec<DirEntryLite> {
        let mut entries = Vec::new();
        let num_raw = buf.len() / DIR_ENTRY_SIZE;

        // Collect raw bytes per entry for LFN assembly
        let mut raw_entries: Vec<&[u8]> = Vec::with_capacity(num_raw);
        for i in 0..num_raw {
            let start = i * DIR_ENTRY_SIZE;
            let end = start + DIR_ENTRY_SIZE;
            raw_entries.push(&buf[start..end]);
        }

        // Two-pass: first collect LFN fragments, then assemble with 8.3 entries
        let mut lfn_buffer: [LFNDirEntry; LFN_MAX_ENTRIES] =
            core::array::from_fn(|_| LFNDirEntry {
                seq: 0,
                chars: [0xFFFF; 13],
                checksum: 0,
            });
        let mut lfn_count: usize = 0;

        for raw in raw_entries.iter() {
            let first_byte = raw[0];
            if first_byte == DIR_ENTRY_LAST {
                break; // end of directory
            }
            if first_byte == DIR_ENTRY_FREE {
                lfn_count = 0; // reset LFN buffer on free entry
                continue;
            }

            let attr = raw[11];
            if (attr & ATTR_LFN_MASK) == ATTR_LFN {
                // LFN entry
                if let Some(lfn) = LFNDirEntry::parse(raw) {
                    let seq = lfn.sequence_index() as usize;
                    if seq > 0 && seq <= LFN_MAX_ENTRIES {
                        lfn_buffer[seq - 1] = lfn;
                        lfn_count = lfn_count.max(seq);
                    }
                }
                continue;
            }

            // Regular 8.3 entry
            if let Some(dir) = DirEntry::parse(raw) {
                // Skip volume labels
                if dir.is_volume_label() {
                    lfn_count = 0;
                    continue;
                }

                // Build LFN from fragments
                let lfn_utf8 = if lfn_count > 0 {
                    let assembled = self.assemble_lfn(&lfn_buffer, lfn_count, &dir);
                    lfn_count = 0;
                    assembled
                } else {
                    Vec::new()
                };

                entries.push(DirEntryLite {
                    short_name: dir.name,
                    lfn_utf8,
                    attr: dir.attr,
                    first_cluster: dir.first_cluster(),
                    size: dir.size,
                    write_date: dir.write_date,
                    write_time: dir.write_time,
                });
            }
        }

        entries
    }

    /// Assemble a VFAT long filename from LFN fragments and verify the checksum.
    fn assemble_lfn(
        &self,
        lfn_buffer: &[LFNDirEntry; LFN_MAX_ENTRIES],
        count: usize,
        dir: &DirEntry,
    ) -> Vec<u8> {
        let mut chars: alloc::vec::Vec<u16> = Vec::with_capacity(13 * count);

        // LFN fragments are stored in reverse order (last fragment first in sequence).
        // The last fragment has bits 0x40 set.
        // We need to find the last fragment and read forward.
        let mut seq = 1;
        while seq <= count && (lfn_buffer[seq - 1].seq & LFN_LAST_MASK) == 0 {
            seq += 1;
        }
        if seq > count {
            // No last-fragment marker found — corrupt LFN, fall back to 8.3
            return Vec::new();
        }

        // Read from last fragment (seq) down to 1
        for i in (1..=seq).rev() {
            let lfn = &lfn_buffer[i - 1];
            if lfn.sequence_index() as usize != i {
                // Sequence mismatch
                return Vec::new();
            }
            // Verify checksum
            let expected = ondisk::lfn_checksum(&dir.name);
            if lfn.checksum != expected {
                // Checksum mismatch — corrupt LFN
                return Vec::new();
            }
            for &ch in &lfn.chars {
                if ch == 0x0000 || ch == 0xFFFF {
                    break; // null terminator or padding
                }
                chars.push(ch);
            }
        }

        // Convert UCS-2 to UTF-8
        ucs2_to_utf8(&chars)
    }

    // === File read helpers ===

    /// Read the entire content of a file given its first cluster and size.
    /// Returns the file data as a byte vector.
    pub fn read_file(&self, first_cluster: u32, size: u32) -> Result<Vec<u8>> {
        if size == 0 {
            return Ok(Vec::new());
        }
        let chain = self.walk_fat_chain(first_cluster)?;
        let cluster_size = self.bpb.bytes_per_cluster() as usize;
        let total_allocated = chain.len() * cluster_size;
        let mut buf = alloc::vec![0u8; total_allocated];
        self.read_cluster_chain(&chain, size, &mut buf)?;
        buf.truncate(size as usize);
        Ok(buf)
    }
}

// ====================================================================
// UCS-2 to UTF-8 conversion
// ====================================================================

/// Convert a slice of UCS-2LE code units to UTF-8 bytes.
fn ucs2_to_utf8(ucs2: &[u16]) -> Vec<u8> {
    let mut utf8 = Vec::with_capacity(ucs2.len() * 3);
    for &ch in ucs2 {
        match ch {
            // ASCII range
            0x0001..=0x007F => utf8.push(ch as u8),
            // 2-byte UTF-8 (U+0080 to U+07FF)
            0x0080..=0x07FF => {
                utf8.push(0xC0 | (ch >> 6) as u8);
                utf8.push(0x80 | (ch & 0x3F) as u8);
            }
            // 3-byte UTF-8 (U+0800 to U+FFFF, excluding surrogates)
            0x0800..=0xD7FF | 0xE000..=0xFFFF => {
                utf8.push(0xE0 | (ch >> 12) as u8);
                utf8.push(0x80 | ((ch >> 6) & 0x3F) as u8);
                utf8.push(0x80 | (ch & 0x3F) as u8);
            }
            // Surrogates and U+0000 — skip
            _ => {}
        }
    }
    utf8
}

// ====================================================================
// Tests
// ====================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ondisk::FatType;
    use alloc::vec;

    /// A fake BlockImage that serves pre-loaded sector data.
    struct FakeImage {
        sectors: Vec<[u8; BLOCK_SIZE]>,
    }

    impl FakeImage {
        fn new(num_sectors: usize, boot_sector: &[u8; BLOCK_SIZE]) -> Self {
            let mut sectors = vec![[0u8; BLOCK_SIZE]; num_sectors];
            sectors[0] = *boot_sector;
            Self { sectors }
        }
    }

    impl BlockImage for FakeImage {
        fn total_blocks(&self) -> u64 {
            self.sectors.len() as u64
        }

        fn read_block(&self, block: u64, out: &mut Page4K) -> Result<()> {
            if block as usize >= self.sectors.len() {
                return Err(FatFormatError::OutOfBounds);
            }
            *out = self.sectors[block as usize];
            Ok(())
        }

        fn write_block(&mut self, _block: u64, _data: &Page4K) -> Result<()> {
            Err(FatFormatError::Unsupported)
        }
    }

    /// Build a minimal FAT32 image with:
    /// - 512-byte sectors, 1 sector per cluster (simplest case)
    /// - 1 reserved sector, 2 FATs, 1 sector per FAT
    /// - Root cluster = 2
    /// - 16 total sectors
    fn make_minimal_fat32() -> (FakeImage, [u8; BLOCK_SIZE]) {
        let mut boot = [0u8; BLOCK_SIZE];
        boot[0] = 0xEB;
        boot[1] = 0x3C;
        boot[2] = 0x90;
        // bytes_per_sector = 512
        boot[11] = 0x00;
        boot[12] = 0x02;
        // sectors_per_cluster = 1
        boot[13] = 1;
        // reserved_sectors = 1
        boot[14] = 1;
        boot[15] = 0;
        // num_fats = 2
        boot[16] = 2;
        // root_entries = 0 (FAT32)
        // total_sectors_16 = 0
        // media = 0xF8
        boot[21] = 0xF8;
        // sectors_per_fat_16 = 0
        // total_sectors_32 = 16
        boot[32] = 16;
        boot[33] = 0;
        boot[34] = 0;
        boot[35] = 0;
        // sectors_per_fat_32 = 1
        boot[36] = 1;
        boot[37] = 0;
        boot[38] = 0;
        boot[39] = 0;
        // root_cluster = 2
        boot[44] = 2;
        boot[45] = 0;
        boot[46] = 0;
        boot[47] = 0;

        // FAT: cluster 0 = media marker, cluster 1 = EOC, cluster 2 = EOC
        let mut fat = [0u8; BLOCK_SIZE];
        fat[0] = 0xF8;
        fat[1] = 0xFF;
        fat[2] = 0xFF;
        fat[3] = 0x0F; // 0x0FFFFFF8 = EOC
        fat[4] = 0xFF;
        fat[5] = 0xFF;
        fat[6] = 0xFF;
        fat[7] = 0x0F; // 0x0FFFFFFF = EOC
        fat[8] = 0xFF;
        fat[9] = 0xFF;
        fat[10] = 0xFF;
        fat[11] = 0x0F; // 0x0FFFFFFF = EOC

        let mut image = FakeImage::new(16, &boot);
        // FAT1 at sector 1, FAT2 at sector 2
        image.sectors[1] = fat;
        image.sectors[2] = fat;
        // Data starts at sector 3 (after reserved + 2 FATs)
        // Sector 3 = cluster 2
        // Write a directory entry for "HELLO   TXT" at cluster 2
        let mut root_dir = [0u8; BLOCK_SIZE];
        let entry = &mut root_dir[0..32];
        entry[0..5].copy_from_slice(b"HELLO");
        // pad name with spaces
        for i in 5..8 {
            entry[i] = b' ';
        }
        entry[8..11].copy_from_slice(b"TXT");
        entry[11] = 0x20; // archive
        // first cluster = 3
        entry[26] = 3;
        entry[27] = 0;
        entry[20] = 0;
        entry[21] = 0;
        // size = 13
        entry[28] = 13;
        entry[29] = 0;
        entry[30] = 0;
        entry[31] = 0;

        image.sectors[3] = root_dir;

        // FAT: cluster 3 = EOC
        fat[12] = 0xFF;
        fat[13] = 0xFF;
        fat[14] = 0xFF;
        fat[15] = 0x0F;
        image.sectors[1] = fat;
        image.sectors[2] = fat;

        (image, boot)
    }

    #[test]
    fn open_fat32() {
        let (image, _) = make_minimal_fat32();
        let pager = FatPager::open(image).expect("should open");
        assert_eq!(pager.bpb.fat_type, FatType::FAT32);
        assert_eq!(pager.bpb.root_cluster, 2);
    }

    #[test]
    fn read_fat_entry() {
        let (image, _) = make_minimal_fat32();
        let pager = FatPager::open(image).unwrap();
        // Cluster 0: media marker (0x0FFFFFF8)
        let entry = pager.read_fat_entry(0);
        assert_eq!(ondisk::fat_entry(entry, FatType::FAT32), 0x0FFF_FFF8);
        // Cluster 2: EOC
        let entry = pager.read_fat_entry(2);
        assert!(ondisk::is_eoc(entry, FatType::FAT32));
    }

    #[test]
    fn walk_fat_chain() {
        let (image, _) = make_minimal_fat32();
        let pager = FatPager::open(image).unwrap();
        // Cluster 2 chain: just cluster 2 (EOC immediately)
        let chain = pager.walk_fat_chain(2).unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0], 2);
    }

    #[test]
    fn read_root_dir() {
        let (image, _) = make_minimal_fat32();
        let pager = FatPager::open(image).unwrap();
        let entries = pager.read_dir_entries(2).unwrap();
        assert!(!entries.is_empty());
        assert_eq!(&entries[0].short_name[0..5], b"HELLO");
        assert_eq!(entries[0].size, 13);
        assert_eq!(entries[0].first_cluster, 3);
    }

    #[test]
    fn ucs2_to_utf8_ascii() {
        let ucs2: Vec<u16> = "hello".encode_utf16().collect();
        let utf8 = ucs2_to_utf8(&ucs2);
        assert_eq!(utf8, b"hello");
    }

    #[test]
    fn ucs2_to_utf8_cjk() {
        // 中文 = U+4E2D U+6587
        let ucs2 = [0x4E2Du16, 0x6587u16];
        let utf8 = ucs2_to_utf8(&ucs2);
        // 中 = E4 B8 AD, 文 = E6 96 87
        assert_eq!(utf8, [0xE4, 0xB8, 0xAD, 0xE6, 0x96, 0x87]);
    }

    #[test]
    fn ucs2_to_utf8_skips_nulls() {
        let ucs2 = [0x0041u16, 0x0000u16, 0x0042u16];
        let utf8 = ucs2_to_utf8(&ucs2);
        assert_eq!(utf8, b"AB"); // null skipped, B included
    }

    // ================================================================
    // FAT12-specific tests
    // ================================================================

    /// Build a minimal FAT12 image:
    /// - 512-byte sectors, 1 sector per cluster, 1 reserved, 2 FATs,
    ///   1 sector per FAT, 4 root entries (1 sector), 9 total sectors.
    fn make_minimal_fat12() -> (FakeImage, [u8; BLOCK_SIZE]) {
        let mut boot = [0u8; BLOCK_SIZE];
        boot[0] = 0xEB;
        boot[1] = 0x3C;
        boot[2] = 0x90;
        // bytes_per_sector = 512
        boot[11] = 0x00;
        boot[12] = 0x02;
        // sectors_per_cluster = 1
        boot[13] = 1;
        // reserved_sectors = 1
        boot[14] = 1;
        boot[15] = 0;
        // num_fats = 2
        boot[16] = 2;
        // root_entries = 4 (tiny root dir, 1 sector)
        boot[17] = 4;
        boot[18] = 0;
        // total_sectors_16 = 9
        boot[19] = 9;
        boot[20] = 0;
        // media = 0xF0 (floppy — will be classified as FAT12)
        boot[21] = 0xF0;
        // sectors_per_fat_16 = 1
        boot[22] = 1;
        boot[23] = 0;

        // FAT12 entries: 12 bits each, packed across byte boundaries.
        // Byte layout: [0]=F0 [1]=FF [2]=FF [3]=FF [4]=FF [5]=FF [6]=0F
        // Cluster 0 (even, off 0): LE16 [0,1] & 0xFFF = 0xFF0 (media)
        // Cluster 1 (odd,  off 1): LE32 [1..4] >> 4 → & 0xFFF = 0xFFF (EOC)
        // Cluster 2 (even, off 3): LE16 [3,4] & 0xFFF = 0xFFF (EOC)
        // Cluster 3 (odd,  off 4): LE32 [4..7] >> 4 → & 0xFFF = 0xFFF (EOC)
        let mut fat = [0u8; BLOCK_SIZE];
        fat[0] = 0xF0;
        fat[1] = 0xFF;
        fat[2] = 0xFF;
        fat[3] = 0xFF;
        fat[4] = 0xFF;
        fat[5] = 0xFF;
        fat[6] = 0x0F;

        // Total sectors: 1 boot + 2 FATs + 1 root dir = 4. Data at sector 4.
        // Sector 4 = cluster 2.
        // Root dir at sector 3.
        let mut image = FakeImage::new(9, &boot);
        image.sectors[1] = fat;
        image.sectors[2] = fat;

        // Write a root directory entry at sector 3: "README  TXT"
        let mut root_dir = [0u8; BLOCK_SIZE];
        let entry = &mut root_dir[0..32];
        entry[0..6].copy_from_slice(b"README");
        entry[6] = b' ';
        entry[7] = b' ';
        entry[8..11].copy_from_slice(b"TXT");
        entry[11] = 0x20; // archive
        // first cluster = 3
        entry[26] = 3;
        entry[27] = 0;
        entry[20] = 0;
        entry[21] = 0;
        // size = 256
        entry[28] = 0x00;
        entry[29] = 0x01; // 256
        entry[30] = 0;
        entry[31] = 0;

        // Cluster 3 is already EOC in the initial FAT layout above.
        image.sectors[3] = root_dir;

        (image, boot)
    }

    #[test]
    fn open_fat12() {
        let (image, _) = make_minimal_fat12();
        let pager = FatPager::open(image).expect("should open FAT12");
        assert_eq!(pager.bpb.fat_type, FatType::FAT12);
        assert_eq!(pager.bpb.sectors_per_fat(), 1);
    }

    #[test]
    fn fat12_entry_even_cluster() {
        let (image, _) = make_minimal_fat12();
        let pager = FatPager::open(image).unwrap();
        // Cluster 0 (even): bytes[0..1] = 0xFFF0, low 12 bits = 0xFF0
        let entry = pager.read_fat_entry(0);
        assert_eq!(ondisk::fat_entry(entry, FatType::FAT12), 0xFF0);
    }

    #[test]
    fn fat12_entry_odd_cluster() {
        let (image, _) = make_minimal_fat12();
        let pager = FatPager::open(image).unwrap();
        // Cluster 1 (odd): bytes[1..2] = 0xFFFF, shift right 4 = 0xFFF
        let entry = pager.read_fat_entry(1);
        assert_eq!(ondisk::fat_entry(entry, FatType::FAT12), 0xFFF);
        assert!(ondisk::is_eoc(entry, FatType::FAT12));
    }

    #[test]
    fn fat12_eoc_detection() {
        let (image, _) = make_minimal_fat12();
        let pager = FatPager::open(image).unwrap();
        assert!(ondisk::is_eoc(pager.read_fat_entry(1), FatType::FAT12));
        assert!(ondisk::is_eoc(pager.read_fat_entry(2), FatType::FAT12));
    }

    #[test]
    fn fat12_out_of_range_cluster_returns_eoc() {
        let (image, _) = make_minimal_fat12();
        let pager = FatPager::open(image).unwrap();
        // FAT12 with 1 sector = 512 bytes. Max cluster = 512*2/3 ≈ 341.
        // Cluster 500 is way out of range.
        let entry = pager.read_fat_entry(500);
        // Should return EOC sentinel (0xFFF), not 0 (free cluster).
        assert_eq!(ondisk::fat_entry(entry, FatType::FAT12), 0xFFF);
        assert!(ondisk::is_eoc(entry, FatType::FAT12));
    }

    #[test]
    fn fat12_root_dir_entries() {
        let (image, _) = make_minimal_fat12();
        let pager = FatPager::open(image).unwrap();
        let entries = pager.read_root_dir_entries().unwrap();
        assert!(!entries.is_empty());
        assert_eq!(&entries[0].short_name[0..6], b"README");
        assert_eq!(entries[0].size, 256);
        assert_eq!(entries[0].first_cluster, 3);
    }
}
