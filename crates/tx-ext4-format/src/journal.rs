//! Stateless JBD2 record codecs used by the ext4 journal planner and replay.
//!
//! This module deliberately contains no journal cursor, cache, block-device, or
//! scheduling state. L5 owns transaction ordering and L6 owns durability.

use alloc::vec::Vec;

use crate::{Ext4FormatError, Result};

pub const JBD2_MAGIC: u32 = 0xC03B_3998;
pub const JBD2_BLOCK_DESCRIPTOR: u32 = 1;
pub const JBD2_BLOCK_COMMIT: u32 = 2;
pub const JBD2_BLOCK_REVOKE: u32 = 5;

const TAG_ESCAPE: u16 = 0x0001;
const TAG_SAME_UUID: u16 = 0x0002;
const TAG_DELETED: u16 = 0x0004;
const TAG_LAST: u16 = 0x0008;
const TAG_UNSUPPORTED: u16 = !(TAG_ESCAPE | TAG_SAME_UUID | TAG_DELETED | TAG_LAST);

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Jbd2Tag {
    pub target_block: u32,
    pub checksum: u16,
    pub uuid: Option<[u8; 16]>,
    pub escaped: bool,
    pub deleted: bool,
    pub same_uuid: bool,
    pub last: bool,
}

impl Jbd2Tag {
    const BASE_LEN: usize = 8;

    pub const fn new(
        target_block: u32,
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
        Self::BASE_LEN + usize::from(!self.same_uuid) * 16
    }

    pub fn encode_legacy(&self, bytes: &mut [u8]) -> Result<usize> {
        if self.same_uuid != self.uuid.is_none() {
            return Err(Ext4FormatError::Corrupt);
        }
        let len = self.encoded_len();
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
        write_u32(bytes, 0, self.target_block)?;
        write_u16(bytes, 4, self.checksum)?;
        write_u16(bytes, 6, flags)?;
        if let Some(uuid) = self.uuid {
            bytes[Self::BASE_LEN..len].copy_from_slice(&uuid);
        }
        Ok(len)
    }

    fn parse_legacy(bytes: &[u8]) -> Result<(Self, usize)> {
        require_len(bytes, Self::BASE_LEN)?;
        let flags = read_u16(bytes, 6)?;
        if flags & TAG_UNSUPPORTED != 0 {
            return Err(Ext4FormatError::Unsupported);
        }
        let same_uuid = flags & TAG_SAME_UUID != 0;
        let len = Self::BASE_LEN + usize::from(!same_uuid) * 16;
        require_len(bytes, len)?;
        let uuid = (!same_uuid).then(|| bytes[Self::BASE_LEN..len].try_into().unwrap());
        Ok((
            Self {
                target_block: read_u32(bytes, 0)?,
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
            let (tag, consumed) =
                Jbd2Tag::parse_legacy(bytes.get(offset..).ok_or(Ext4FormatError::Corrupt)?)?;
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
            let written =
                tag.encode_legacy(bytes.get_mut(offset..).ok_or(Ext4FormatError::Truncated)?)?;
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
    pub blocks: Vec<u32>,
}

impl Jbd2Revoke {
    const HEADER_LEN: usize = Jbd2Header::ENCODED_LEN + 4;

    pub fn parse(bytes: &[u8]) -> Result<Self> {
        require_len(bytes, Self::HEADER_LEN)?;
        let header = Jbd2Header::parse(bytes)?;
        if header.block_type != JBD2_BLOCK_REVOKE {
            return Err(Ext4FormatError::Corrupt);
        }
        let count = read_u32(bytes, Jbd2Header::ENCODED_LEN)? as usize;
        if count < Self::HEADER_LEN || count > bytes.len() {
            return Err(Ext4FormatError::Corrupt);
        }
        if (count - Self::HEADER_LEN) % 4 != 0 {
            return Err(Ext4FormatError::Corrupt);
        }
        let mut blocks = Vec::new();
        for offset in (Self::HEADER_LEN..count).step_by(4) {
            blocks.push(read_u32(bytes, offset)?);
        }
        Ok(Self { header, blocks })
    }

    pub fn encode(&self, bytes: &mut [u8]) -> Result<()> {
        if self.header.block_type != JBD2_BLOCK_REVOKE {
            return Err(Ext4FormatError::Corrupt);
        }
        let count = Self::HEADER_LEN
            .checked_add(
                self.blocks
                    .len()
                    .checked_mul(4)
                    .ok_or(Ext4FormatError::OutOfBounds)?,
            )
            .ok_or(Ext4FormatError::OutOfBounds)?;
        require_len(bytes, count)?;
        bytes.fill(0);
        self.header.encode(bytes)?;
        write_u32(bytes, Jbd2Header::ENCODED_LEN, count as u32)?;
        for (index, block) in self.blocks.iter().enumerate() {
            write_u32(bytes, Self::HEADER_LEN + index * 4, *block)?;
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
