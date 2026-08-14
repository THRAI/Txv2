//! Stateless JBD2 record codecs used by the ext4 journal planner and replay.
//!
//! This module deliberately contains no journal cursor, cache, block-device, or
//! scheduling state. L5 owns transaction ordering and L6 owns durability.

use alloc::vec::Vec;

use crate::{Ext4FormatError, Result};

pub const JBD2_MAGIC: u32 = 0xC03B_3998;
pub const JBD2_BLOCK_DESCRIPTOR: u32 = 1;
pub const JBD2_BLOCK_COMMIT: u32 = 2;
pub const JBD2_BLOCK_SUPERBLOCK_V1: u32 = 3;
pub const JBD2_BLOCK_SUPERBLOCK_V2: u32 = 4;
pub const JBD2_BLOCK_REVOKE: u32 = 5;
pub const JBD2_BLOCK_SIZE: usize = 4096;

pub const JBD2_FEATURE_INCOMPAT_REVOKE: u32 = 0x0000_0001;
pub const JBD2_FEATURE_INCOMPAT_64BIT: u32 = 0x0000_0002;
pub const JBD2_FEATURE_INCOMPAT_ASYNC_COMMIT: u32 = 0x0000_0004;
pub const JBD2_FEATURE_INCOMPAT_CSUM_V2: u32 = 0x0000_0008;
pub const JBD2_FEATURE_INCOMPAT_CSUM_V3: u32 = 0x0000_0010;
pub const JBD2_FEATURE_INCOMPAT_FAST_COMMIT: u32 = 0x0000_0020;

/// Strictly supported JBD2 on-disk feature profile.
///
/// The codec accepts only legacy records plus the standard revoke and 64-bit
/// block-number extensions. Checksum-v2/v3, async commit, fast commit, and
/// unknown feature bits require different record-validation contracts and are
/// rejected rather than being silently masked off.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Jbd2Features {
    pub revoke: bool,
    pub block_64bit: bool,
}

impl Jbd2Features {
    pub const NONE: Self = Self {
        revoke: false,
        block_64bit: false,
    };
    pub const REVOKE: Self = Self {
        revoke: true,
        block_64bit: false,
    };
    pub const BLOCK_64BIT: Self = Self {
        revoke: false,
        block_64bit: true,
    };
    pub const REVOKE_64BIT: Self = Self {
        revoke: true,
        block_64bit: true,
    };

    const SUPPORTED_INCOMPAT: u32 = JBD2_FEATURE_INCOMPAT_REVOKE | JBD2_FEATURE_INCOMPAT_64BIT;

    pub fn from_raw(
        feature_compat: u32,
        feature_incompat: u32,
        feature_ro_compat: u32,
    ) -> Result<Self> {
        if feature_compat != 0
            || feature_ro_compat != 0
            || feature_incompat & !Self::SUPPORTED_INCOMPAT != 0
        {
            return Err(Ext4FormatError::Unsupported);
        }
        Ok(Self {
            revoke: feature_incompat & JBD2_FEATURE_INCOMPAT_REVOKE != 0,
            block_64bit: feature_incompat & JBD2_FEATURE_INCOMPAT_64BIT != 0,
        })
    }

    pub const fn feature_incompat(self) -> u32 {
        (if self.revoke {
            JBD2_FEATURE_INCOMPAT_REVOKE
        } else {
            0
        }) | (if self.block_64bit {
            JBD2_FEATURE_INCOMPAT_64BIT
        } else {
            0
        })
    }
}

const TAG_ESCAPE: u16 = 0x0001;
const TAG_SAME_UUID: u16 = 0x0002;
const TAG_DELETED: u16 = 0x0004;
const TAG_LAST: u16 = 0x0008;
const TAG_UNSUPPORTED: u16 = !(TAG_ESCAPE | TAG_SAME_UUID | TAG_DELETED | TAG_LAST);

/// One metadata home block copied into a JBD2 transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Jbd2MetadataUpdate {
    pub home_block: u64,
    pub bytes: [u8; JBD2_BLOCK_SIZE],
}

impl Jbd2MetadataUpdate {
    pub const fn new(home_block: u32, bytes: [u8; JBD2_BLOCK_SIZE]) -> Self {
        Self {
            home_block: home_block as u64,
            bytes,
        }
    }

    pub const fn new64(home_block: u64, bytes: [u8; JBD2_BLOCK_SIZE]) -> Self {
        Self { home_block, bytes }
    }
}

/// Encoded record pages for one supported JBD2 transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Jbd2TransactionImage {
    pub descriptor: [u8; JBD2_BLOCK_SIZE],
    pub metadata_blocks: Vec<[u8; JBD2_BLOCK_SIZE]>,
    pub revokes: Vec<[u8; JBD2_BLOCK_SIZE]>,
    pub commit: [u8; JBD2_BLOCK_SIZE],
}

impl Jbd2TransactionImage {
    /// Encode a legacy 32-bit descriptor, its metadata journal copies, and
    /// commit page.
    pub fn encode_legacy(
        sequence: u32,
        journal_uuid: [u8; 16],
        updates: Vec<Jbd2MetadataUpdate>,
    ) -> Result<Self> {
        Self::encode_with_features(sequence, journal_uuid, updates, Jbd2Features::NONE)
    }

    /// Encode a legacy transaction with zero or more revoke pages between its
    /// metadata copies and commit record.
    pub fn encode_legacy_with_revokes(
        sequence: u32,
        journal_uuid: [u8; 16],
        updates: Vec<Jbd2MetadataUpdate>,
        revoked_blocks: Vec<u32>,
    ) -> Result<Self> {
        Self::encode_with_features_and_revokes(
            sequence,
            journal_uuid,
            updates,
            revoked_blocks.into_iter().map(u64::from).collect(),
            Jbd2Features::REVOKE,
        )
    }

    /// Encode a transaction without revoke records under an explicitly
    /// validated JBD2 feature profile.
    ///
    /// Metadata that begins with the JBD2 magic is escaped in the journal copy;
    /// replay restores the magic after observing the descriptor tag.
    pub fn encode_with_features(
        sequence: u32,
        journal_uuid: [u8; 16],
        updates: Vec<Jbd2MetadataUpdate>,
        features: Jbd2Features,
    ) -> Result<Self> {
        Self::encode_with_features_and_revokes(
            sequence,
            journal_uuid,
            updates,
            Vec::new(),
            features,
        )
    }

    /// Encode a transaction with zero or more revoke pages under an explicitly
    /// validated JBD2 feature profile.
    pub fn encode_with_features_and_revokes(
        sequence: u32,
        journal_uuid: [u8; 16],
        updates: Vec<Jbd2MetadataUpdate>,
        mut revoked_blocks: Vec<u64>,
        features: Jbd2Features,
    ) -> Result<Self> {
        if updates.is_empty() {
            return Err(Ext4FormatError::Corrupt);
        }
        if !features.revoke && !revoked_blocks.is_empty() {
            return Err(Ext4FormatError::Unsupported);
        }
        let update_count = updates.len();
        let mut tags = Vec::new();
        let mut metadata_blocks = Vec::new();
        for (index, update) in updates.into_iter().enumerate() {
            let mut bytes = update.bytes;
            let escaped = bytes[..4] == JBD2_MAGIC.to_be_bytes();
            if escaped {
                bytes[..4].fill(0);
            }
            let mut tag = Jbd2Tag::new64(
                update.home_block,
                0,
                (index == 0).then_some(journal_uuid),
                index != 0,
            );
            tag.escaped = escaped;
            tag.last = index + 1 == update_count;
            tags.push(tag);
            metadata_blocks.push(bytes);
        }

        let mut descriptor = [0; JBD2_BLOCK_SIZE];
        Jbd2Descriptor {
            header: Jbd2Header::descriptor(sequence),
            tags,
        }
        .encode_with_features(&mut descriptor, features)?;

        revoked_blocks.sort_unstable();
        revoked_blocks.dedup();
        let mut revokes = Vec::new();
        let revoke_page_capacity = Jbd2Revoke::max_blocks_per_page(features);
        for blocks in revoked_blocks.chunks(revoke_page_capacity) {
            let mut page = [0; JBD2_BLOCK_SIZE];
            Jbd2Revoke {
                header: Jbd2Header::revoke(sequence),
                blocks: blocks.to_vec(),
            }
            .encode_with_features(&mut page, features)?;
            revokes.push(page);
        }

        let mut commit = [0; JBD2_BLOCK_SIZE];
        Jbd2Commit {
            header: Jbd2Header::commit(sequence),
            checksum_type: 0,
            checksum_size: 0,
            checksums: [0; 8],
            seconds: 0,
            nanoseconds: 0,
        }
        .encode(&mut commit)?;

        Ok(Self {
            descriptor,
            metadata_blocks,
            revokes,
            commit,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Jbd2Header {
    pub block_type: u32,
    pub sequence: u32,
}

impl Jbd2Header {
    pub const ENCODED_LEN: usize = 12;

    pub const fn descriptor(sequence: u32) -> Self {
        Self {
            block_type: JBD2_BLOCK_DESCRIPTOR,
            sequence,
        }
    }

    pub const fn commit(sequence: u32) -> Self {
        Self {
            block_type: JBD2_BLOCK_COMMIT,
            sequence,
        }
    }

    pub const fn revoke(sequence: u32) -> Self {
        Self {
            block_type: JBD2_BLOCK_REVOKE,
            sequence,
        }
    }

    pub fn parse(bytes: &[u8]) -> Result<Self> {
        require_len(bytes, Self::ENCODED_LEN)?;
        if read_u32(bytes, 0)? != JBD2_MAGIC {
            return Err(Ext4FormatError::BadMagic);
        }
        Ok(Self {
            block_type: read_u32(bytes, 4)?,
            sequence: read_u32(bytes, 8)?,
        })
    }

    pub fn encode(self, bytes: &mut [u8]) -> Result<()> {
        require_len(bytes, Self::ENCODED_LEN)?;
        write_u32(bytes, 0, JBD2_MAGIC)?;
        write_u32(bytes, 4, self.block_type)?;
        write_u32(bytes, 8, self.sequence)
    }
}

/// Immutable geometry recorded in JBD2's first journal-file block.
///
/// The journal inode supplies the physical mapping for this logical ring. This
/// record only describes the ring's relative block indices; L5 owns its cursor
/// and L6 owns submission and durability.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Jbd2Superblock {
    pub block_type: u32,
    pub block_size: u32,
    pub max_len: u32,
    pub first: u32,
    pub sequence: u32,
    pub start: u32,
    pub uuid: [u8; 16],
}

impl Jbd2Superblock {
    pub const ENCODED_LEN: usize = 64;

    pub fn parse(bytes: &[u8]) -> Result<Self> {
        Self::parse_with_features(bytes).map(|(superblock, _features)| superblock)
    }

    /// Parse ring geometry and return the strictly validated on-disk feature
    /// profile needed by descriptor and revoke codecs.
    pub fn parse_with_features(bytes: &[u8]) -> Result<(Self, Jbd2Features)> {
        require_len(bytes, Self::ENCODED_LEN)?;
        let header = Jbd2Header::parse(bytes)?;
        if header.block_type != JBD2_BLOCK_SUPERBLOCK_V1
            && header.block_type != JBD2_BLOCK_SUPERBLOCK_V2
        {
            return Err(Ext4FormatError::Unsupported);
        }
        let block_size = read_u32(bytes, 12)?;
        if block_size != JBD2_BLOCK_SIZE as u32 {
            return Err(Ext4FormatError::Unsupported);
        }
        let max_len = read_u32(bytes, 16)?;
        let first = read_u32(bytes, 20)?;
        let start = read_u32(bytes, 28)?;
        if max_len <= 1 || first == 0 || first >= max_len {
            return Err(Ext4FormatError::Corrupt);
        }
        if start != 0 && (start < first || start >= max_len) {
            return Err(Ext4FormatError::Corrupt);
        }
        let (features, uuid) = match header.block_type {
            JBD2_BLOCK_SUPERBLOCK_V1 => {
                // V1 ends before the V2 feature and UUID fields. Treating
                // these reserved bytes as V2 metadata could manufacture a
                // feature token that the on-disk format never declared.
                if bytes[36..64].iter().any(|byte| *byte != 0) {
                    return Err(Ext4FormatError::Unsupported);
                }
                (Jbd2Features::NONE, [0; 16])
            }
            JBD2_BLOCK_SUPERBLOCK_V2 => (
                Jbd2Features::from_raw(
                    read_u32(bytes, 36)?,
                    read_u32(bytes, 40)?,
                    read_u32(bytes, 44)?,
                )?,
                bytes[48..64].try_into().unwrap(),
            ),
            _ => unreachable!("superblock type was checked above"),
        };
        Ok((
            Self {
                block_type: header.block_type,
                block_size,
                max_len,
                first,
                sequence: read_u32(bytes, 24)?,
                start,
                uuid,
            },
            features,
        ))
    }

    /// Update only the dynamic recovery state of a journal superblock page.
    ///
    /// The caller supplies the page it discovered through the journal inode,
    /// rather than rebuilding the page from this abbreviated parsed view. This
    /// keeps feature and future-extension fields intact while allowing L5 to
    /// publish `s_sequence` and `s_start` around transaction commit.
    pub fn write_state(
        &self,
        page: &mut [u8; JBD2_BLOCK_SIZE],
        sequence: u32,
        start: u32,
    ) -> Result<()> {
        let observed = Self::parse(page)?;
        if observed.block_type != self.block_type
            || observed.block_size != self.block_size
            || observed.max_len != self.max_len
            || observed.first != self.first
            || observed.uuid != self.uuid
        {
            return Err(Ext4FormatError::Corrupt);
        }
        if start != 0 && (start < self.first || start >= self.max_len) {
            return Err(Ext4FormatError::Corrupt);
        }

        write_u32(page, 24, sequence)?;
        write_u32(page, 28, start)?;

        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Jbd2Tag {
    pub target_block: u64,
    pub checksum: u16,
    pub uuid: Option<[u8; 16]>,
    pub escaped: bool,
    pub deleted: bool,
    pub same_uuid: bool,
    pub last: bool,
}

impl Jbd2Tag {
    const LEGACY_BASE_LEN: usize = 8;
    const BLOCK_64BIT_BASE_LEN: usize = 12;

    pub const fn new(
        target_block: u32,
        checksum: u16,
        uuid: Option<[u8; 16]>,
        same_uuid: bool,
    ) -> Self {
        Self::new64(target_block as u64, checksum, uuid, same_uuid)
    }

    pub const fn new64(
        target_block: u64,
        checksum: u16,
        uuid: Option<[u8; 16]>,
        same_uuid: bool,
    ) -> Self {
        Self {
            target_block,
            checksum,
            uuid,
            escaped: false,
            deleted: false,
            same_uuid,
            last: false,
        }
    }

    pub fn encoded_len(&self) -> usize {
        self.encoded_len_with_features(Jbd2Features::NONE)
    }

    pub fn encoded_len_with_features(&self, features: Jbd2Features) -> usize {
        let base_len = if features.block_64bit {
            Self::BLOCK_64BIT_BASE_LEN
        } else {
            Self::LEGACY_BASE_LEN
        };
        base_len + usize::from(!self.same_uuid) * 16
    }

    pub fn encode_legacy(&self, bytes: &mut [u8]) -> Result<usize> {
        self.encode_with_features(bytes, Jbd2Features::NONE)
    }

    pub fn encode_with_features(&self, bytes: &mut [u8], features: Jbd2Features) -> Result<usize> {
        if self.same_uuid != self.uuid.is_none() {
            return Err(Ext4FormatError::Corrupt);
        }
        if !features.block_64bit && self.target_block > u32::MAX as u64 {
            return Err(Ext4FormatError::OutOfBounds);
        }
        let base_len = if features.block_64bit {
            Self::BLOCK_64BIT_BASE_LEN
        } else {
            Self::LEGACY_BASE_LEN
        };
        let len = self.encoded_len_with_features(features);
        require_len(bytes, len)?;
        let mut flags = 0;
        if self.escaped {
            flags |= TAG_ESCAPE;
        }
        if self.same_uuid {
            flags |= TAG_SAME_UUID;
        }
        if self.deleted {
            flags |= TAG_DELETED;
        }
        if self.last {
            flags |= TAG_LAST;
        }
        write_u32(bytes, 0, self.target_block as u32)?;
        write_u16(bytes, 4, self.checksum)?;
        write_u16(bytes, 6, flags)?;
        if features.block_64bit {
            write_u32(
                bytes,
                Self::LEGACY_BASE_LEN,
                (self.target_block >> 32) as u32,
            )?;
        }
        if let Some(uuid) = self.uuid {
            bytes[base_len..len].copy_from_slice(&uuid);
        }
        Ok(len)
    }

    fn parse_with_features(bytes: &[u8], features: Jbd2Features) -> Result<(Self, usize)> {
        let base_len = if features.block_64bit {
            Self::BLOCK_64BIT_BASE_LEN
        } else {
            Self::LEGACY_BASE_LEN
        };
        require_len(bytes, base_len)?;
        let flags = read_u16(bytes, 6)?;
        if flags & TAG_UNSUPPORTED != 0 {
            return Err(Ext4FormatError::Unsupported);
        }
        let same_uuid = flags & TAG_SAME_UUID != 0;
        let len = base_len + usize::from(!same_uuid) * 16;
        require_len(bytes, len)?;
        let uuid = (!same_uuid).then(|| bytes[base_len..len].try_into().unwrap());
        let target_block = u64::from(read_u32(bytes, 0)?)
            | if features.block_64bit {
                u64::from(read_u32(bytes, Self::LEGACY_BASE_LEN)?) << 32
            } else {
                0
            };
        Ok((
            Self {
                target_block,
                checksum: read_u16(bytes, 4)?,
                uuid,
                escaped: flags & TAG_ESCAPE != 0,
                deleted: flags & TAG_DELETED != 0,
                same_uuid,
                last: flags & TAG_LAST != 0,
            },
            len,
        ))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Jbd2Descriptor {
    pub header: Jbd2Header,
    pub tags: Vec<Jbd2Tag>,
}

impl Jbd2Descriptor {
    pub fn parse_legacy(bytes: &[u8]) -> Result<Self> {
        Self::parse_with_features(bytes, Jbd2Features::NONE)
    }

    pub fn parse_with_features(bytes: &[u8], features: Jbd2Features) -> Result<Self> {
        let header = Jbd2Header::parse(bytes)?;
        if header.block_type != JBD2_BLOCK_DESCRIPTOR {
            return Err(Ext4FormatError::Corrupt);
        }
        let mut tags = Vec::new();
        let mut offset = Jbd2Header::ENCODED_LEN;
        loop {
            if offset == bytes.len() {
                return Err(Ext4FormatError::Corrupt);
            }
            let (tag, consumed) = Jbd2Tag::parse_with_features(
                bytes.get(offset..).ok_or(Ext4FormatError::Corrupt)?,
                features,
            )?;
            offset = offset
                .checked_add(consumed)
                .ok_or(Ext4FormatError::Corrupt)?;
            let last = tag.last;
            tags.push(tag);
            if last {
                return Ok(Self { header, tags });
            }
        }
    }

    pub fn encode_legacy(&self, bytes: &mut [u8]) -> Result<()> {
        self.encode_with_features(bytes, Jbd2Features::NONE)
    }

    pub fn encode_with_features(&self, bytes: &mut [u8], features: Jbd2Features) -> Result<()> {
        if self.header.block_type != JBD2_BLOCK_DESCRIPTOR || self.tags.is_empty() {
            return Err(Ext4FormatError::Corrupt);
        }
        bytes.fill(0);
        self.header.encode(bytes)?;
        let mut offset = Jbd2Header::ENCODED_LEN;
        for (index, tag) in self.tags.iter().enumerate() {
            if tag.last != (index + 1 == self.tags.len()) {
                return Err(Ext4FormatError::Corrupt);
            }
            let written = tag.encode_with_features(
                bytes.get_mut(offset..).ok_or(Ext4FormatError::Truncated)?,
                features,
            )?;
            offset = offset
                .checked_add(written)
                .ok_or(Ext4FormatError::OutOfBounds)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Jbd2Commit {
    pub header: Jbd2Header,
    pub checksum_type: u8,
    pub checksum_size: u8,
    pub checksums: [u32; 8],
    pub seconds: u64,
    pub nanoseconds: u32,
}

impl Jbd2Commit {
    pub const ENCODED_LEN: usize = 60;

    pub fn parse(bytes: &[u8]) -> Result<Self> {
        require_len(bytes, Self::ENCODED_LEN)?;
        let header = Jbd2Header::parse(bytes)?;
        if header.block_type != JBD2_BLOCK_COMMIT {
            return Err(Ext4FormatError::Corrupt);
        }
        let mut checksums = [0; 8];
        for (index, checksum) in checksums.iter_mut().enumerate() {
            *checksum = read_u32(bytes, 16 + index * 4)?;
        }
        Ok(Self {
            header,
            checksum_type: bytes[12],
            checksum_size: bytes[13],
            checksums,
            seconds: read_u64(bytes, 48)?,
            nanoseconds: read_u32(bytes, 56)?,
        })
    }

    pub fn encode(&self, bytes: &mut [u8]) -> Result<()> {
        if self.header.block_type != JBD2_BLOCK_COMMIT {
            return Err(Ext4FormatError::Corrupt);
        }
        require_len(bytes, Self::ENCODED_LEN)?;
        bytes.fill(0);
        self.header.encode(bytes)?;
        bytes[12] = self.checksum_type;
        bytes[13] = self.checksum_size;
        for (index, checksum) in self.checksums.iter().enumerate() {
            write_u32(bytes, 16 + index * 4, *checksum)?;
        }
        write_u64(bytes, 48, self.seconds)?;
        write_u32(bytes, 56, self.nanoseconds)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Jbd2Revoke {
    pub header: Jbd2Header,
    pub blocks: Vec<u64>,
}

impl Jbd2Revoke {
    const HEADER_LEN: usize = Jbd2Header::ENCODED_LEN + 4;
    pub const MAX_BLOCKS_PER_PAGE: usize = (JBD2_BLOCK_SIZE - Self::HEADER_LEN) / 4;

    pub const fn page_count(block_count: usize) -> usize {
        block_count.div_ceil(Self::MAX_BLOCKS_PER_PAGE)
    }

    pub const fn max_blocks_per_page(features: Jbd2Features) -> usize {
        (JBD2_BLOCK_SIZE - Self::HEADER_LEN) / Self::entry_len(features)
    }

    pub const fn page_count_with_features(block_count: usize, features: Jbd2Features) -> usize {
        block_count.div_ceil(Self::max_blocks_per_page(features))
    }

    const fn entry_len(features: Jbd2Features) -> usize {
        if features.block_64bit {
            8
        } else {
            4
        }
    }

    pub fn parse(bytes: &[u8]) -> Result<Self> {
        Self::parse_with_features(bytes, Jbd2Features::REVOKE)
    }

    pub fn parse_with_features(bytes: &[u8], features: Jbd2Features) -> Result<Self> {
        if !features.revoke {
            return Err(Ext4FormatError::Unsupported);
        }
        require_len(bytes, Self::HEADER_LEN)?;
        let header = Jbd2Header::parse(bytes)?;
        if header.block_type != JBD2_BLOCK_REVOKE {
            return Err(Ext4FormatError::Corrupt);
        }
        let count = read_u32(bytes, Jbd2Header::ENCODED_LEN)? as usize;
        if count < Self::HEADER_LEN || count > bytes.len() {
            return Err(Ext4FormatError::Corrupt);
        }
        let entry_len = Self::entry_len(features);
        if (count - Self::HEADER_LEN) % entry_len != 0 {
            return Err(Ext4FormatError::Corrupt);
        }
        let mut blocks = Vec::new();
        for offset in (Self::HEADER_LEN..count).step_by(entry_len) {
            blocks.push(if features.block_64bit {
                read_u64(bytes, offset)?
            } else {
                u64::from(read_u32(bytes, offset)?)
            });
        }
        Ok(Self { header, blocks })
    }

    pub fn encode(&self, bytes: &mut [u8]) -> Result<()> {
        self.encode_with_features(bytes, Jbd2Features::REVOKE)
    }

    pub fn encode_with_features(&self, bytes: &mut [u8], features: Jbd2Features) -> Result<()> {
        if !features.revoke {
            return Err(Ext4FormatError::Unsupported);
        }
        if self.header.block_type != JBD2_BLOCK_REVOKE {
            return Err(Ext4FormatError::Corrupt);
        }
        if self.blocks.len() > Self::max_blocks_per_page(features) {
            return Err(Ext4FormatError::OutOfBounds);
        }
        if !features.block_64bit && self.blocks.iter().any(|block| *block > u32::MAX as u64) {
            return Err(Ext4FormatError::OutOfBounds);
        }
        let entry_len = Self::entry_len(features);
        let count = Self::HEADER_LEN
            .checked_add(
                self.blocks
                    .len()
                    .checked_mul(entry_len)
                    .ok_or(Ext4FormatError::OutOfBounds)?,
            )
            .ok_or(Ext4FormatError::OutOfBounds)?;
        require_len(bytes, count)?;
        bytes.fill(0);
        self.header.encode(bytes)?;
        write_u32(bytes, Jbd2Header::ENCODED_LEN, count as u32)?;
        for (index, block) in self.blocks.iter().enumerate() {
            let offset = Self::HEADER_LEN + index * entry_len;
            if features.block_64bit {
                write_u64(bytes, offset, *block)?;
            } else {
                write_u32(bytes, offset, *block as u32)?;
            }
        }
        Ok(())
    }
}

fn require_len(bytes: &[u8], len: usize) -> Result<()> {
    if bytes.len() < len {
        Err(Ext4FormatError::Truncated)
    } else {
        Ok(())
    }
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16> {
    Ok(u16::from_be_bytes(
        bytes
            .get(offset..offset + 2)
            .ok_or(Ext4FormatError::Truncated)?
            .try_into()
            .unwrap(),
    ))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    Ok(u32::from_be_bytes(
        bytes
            .get(offset..offset + 4)
            .ok_or(Ext4FormatError::Truncated)?
            .try_into()
            .unwrap(),
    ))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64> {
    Ok(u64::from_be_bytes(
        bytes
            .get(offset..offset + 8)
            .ok_or(Ext4FormatError::Truncated)?
            .try_into()
            .unwrap(),
    ))
}

fn write_u16(bytes: &mut [u8], offset: usize, value: u16) -> Result<()> {
    bytes
        .get_mut(offset..offset + 2)
        .ok_or(Ext4FormatError::Truncated)?
        .copy_from_slice(&value.to_be_bytes());
    Ok(())
}

fn write_u32(bytes: &mut [u8], offset: usize, value: u32) -> Result<()> {
    bytes
        .get_mut(offset..offset + 4)
        .ok_or(Ext4FormatError::Truncated)?
        .copy_from_slice(&value.to_be_bytes());
    Ok(())
}

fn write_u64(bytes: &mut [u8], offset: usize, value: u64) -> Result<()> {
    bytes
        .get_mut(offset..offset + 8)
        .ok_or(Ext4FormatError::Truncated)?
        .copy_from_slice(&value.to_be_bytes());
    Ok(())
}
