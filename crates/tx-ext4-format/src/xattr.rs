extern crate alloc;

use alloc::vec::Vec;

use crate::ondisk::{
    metadata_csum32, read_u16_le, read_u32_le, write_u16_le, write_u32_le, Inode, Superblock,
};
use crate::{Ext4FormatError, Result};

// Format reference: Linux v6.17 `fs/ext4/xattr.h` and
// `fs/ext4/xattr.c` (`ext4_xattr_header`, `ext4_xattr_entry`,
// `check_xattrs`, and `ext4_xattr_block_csum`). The parser is a
// small Tx-native port of those layout/validation rules, not copied
// Linux code.
pub const EXT4_XATTR_MAGIC: u32 = 0xEA02_0000;
pub const EXT4_XATTR_INDEX_USER: u8 = 1;
pub const EXT4_XATTR_INDEX_SECURITY: u8 = 6;

const EXT4_XATTR_ENTRY_FIXED: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InlineXattr {
    pub name: Vec<u8>,
    pub value: Vec<u8>,
}

#[derive(Clone, Debug)]
struct RawEntry<'a> {
    name_index: u8,
    name: &'a [u8],
    value_offs: usize,
    value_inum: u32,
    value_size: usize,
}

pub fn parse_inline_xattrs(inode: &Inode, inode_bytes: &[u8]) -> Result<Vec<InlineXattr>> {
    // Linux `IHDR/IFIRST`: inline values are addressed relative to
    // the first entry after the inode-body xattr header.
    let start = 128usize
        .checked_add(inode.extra_isize as usize)
        .ok_or(Ext4FormatError::Corrupt)?;
    if start == inode_bytes.len() {
        return Ok(Vec::new());
    }
    if start > inode_bytes.len() {
        return Err(Ext4FormatError::Corrupt);
    }
    let storage = &inode_bytes[start..];
    if storage.len() < 4 {
        return Ok(Vec::new());
    }
    let magic = read_u32_le(storage, 0)?;
    if magic == 0 {
        return Ok(Vec::new());
    }
    if magic != EXT4_XATTR_MAGIC {
        return Err(Ext4FormatError::Corrupt);
    }

    parse_xattr_entries(storage, 4, 4)
}

pub fn encode_inline_xattrs(
    inode: &Inode,
    inode_bytes: &mut [u8],
    attrs: &[InlineXattr],
) -> Result<bool> {
    let start = 128usize
        .checked_add(inode.extra_isize as usize)
        .ok_or(Ext4FormatError::Corrupt)?;
    if start > inode_bytes.len() {
        return Err(Ext4FormatError::Corrupt);
    }
    let storage = &mut inode_bytes[start..];
    storage.fill(0);
    if attrs.is_empty() {
        return Ok(true);
    }
    if storage.len() < 8 {
        return Ok(false);
    }
    write_u32_le(storage, 0, EXT4_XATTR_MAGIC)?;
    encode_xattr_entries(storage, 4, 4, attrs)
}

pub fn parse_external_xattr_block(
    superblock: &Superblock,
    block_nr: u64,
    block: &[u8],
) -> Result<Vec<InlineXattr>> {
    if block.len() < 32 {
        return Err(Ext4FormatError::Truncated);
    }
    let magic = read_u32_le(block, 0)?;
    if magic != EXT4_XATTR_MAGIC {
        return Err(Ext4FormatError::Corrupt);
    }
    let blocks = read_u32_le(block, 8)?;
    if blocks != 1 {
        return Err(Ext4FormatError::Unsupported);
    }
    if superblock.has_metadata_csum() {
        let expected = read_u32_le(block, 16)?;
        if expected != xattr_block_checksum(superblock, block_nr, block)? {
            return Err(Ext4FormatError::Corrupt);
        }
    }

    parse_xattr_entries(block, 32, 0)
}

pub fn encode_external_xattr_block(
    superblock: &Superblock,
    block_nr: u64,
    block: &mut [u8],
    attrs: &[InlineXattr],
) -> Result<()> {
    if block.len() < 32 {
        return Err(Ext4FormatError::Truncated);
    }
    block.fill(0);
    write_u32_le(block, 0, EXT4_XATTR_MAGIC)?;
    write_u32_le(block, 4, 1)?;
    write_u32_le(block, 8, 1)?;
    if !encode_xattr_entries(block, 32, 0, attrs)? {
        return Err(Ext4FormatError::Unsupported);
    }
    if superblock.has_metadata_csum() {
        let checksum = xattr_block_checksum(superblock, block_nr, block)?;
        write_u32_le(block, 16, checksum)?;
    }
    Ok(())
}

pub fn xattr_block_checksum(superblock: &Superblock, block_nr: u64, block: &[u8]) -> Result<u32> {
    if block.len() < 20 {
        return Err(Ext4FormatError::Truncated);
    }
    let block_nr = block_nr.to_le_bytes();
    let zero = 0u32.to_le_bytes();
    Ok(metadata_csum32(
        metadata_checksum_seed(superblock),
        &[&block_nr, &block[..16], &zero, &block[20..]],
    ))
}

fn metadata_checksum_seed(superblock: &Superblock) -> u32 {
    if superblock.checksum_seed != 0 {
        superblock.checksum_seed
    } else {
        metadata_csum32(!0, &[&superblock.uuid])
    }
}

fn parse_xattr_entries(
    storage: &[u8],
    entry_start: usize,
    value_base: usize,
) -> Result<Vec<InlineXattr>> {
    if entry_start > storage.len() || value_base > storage.len() {
        return Err(Ext4FormatError::Corrupt);
    }

    let mut entries = Vec::new();
    let mut offset = entry_start;
    loop {
        if offset + 4 > storage.len() {
            return Err(Ext4FormatError::Truncated);
        }
        if read_u32_le(storage, offset)? == 0 {
            break;
        }
        let entry_end = parse_raw_entry(storage, offset, &mut entries)?;
        offset = align4(entry_end);
    }
    // Linux `check_xattrs` requires values to start after the
    // zeroed last-entry marker, preventing name/value overlap.
    let values_floor = offset.checked_add(4).ok_or(Ext4FormatError::Corrupt)?;

    let mut out = Vec::new();
    for entry in entries {
        if entry.value_inum != 0 {
            return Err(Ext4FormatError::Unsupported);
        }
        if entry.name_index != EXT4_XATTR_INDEX_USER {
            continue;
        }
        if entry.value_size > 0 {
            if entry.value_offs % 4 != 0 {
                return Err(Ext4FormatError::Corrupt);
            }
            let value_start = value_base
                .checked_add(entry.value_offs)
                .ok_or(Ext4FormatError::Corrupt)?;
            if value_start < values_floor {
                return Err(Ext4FormatError::Corrupt);
            }
            let value_end = entry
                .value_size
                .checked_add(value_start)
                .ok_or(Ext4FormatError::Corrupt)?;
            let padded_end = align4(entry.value_size)
                .checked_add(value_start)
                .ok_or(Ext4FormatError::Corrupt)?;
            if padded_end > storage.len() {
                return Err(Ext4FormatError::Truncated);
            }
            let Some(value) = storage.get(value_start..value_end) else {
                return Err(Ext4FormatError::Truncated);
            };
            out.push(InlineXattr {
                name: user_xattr_name(entry.name),
                value: value.to_vec(),
            });
        } else {
            out.push(InlineXattr {
                name: user_xattr_name(entry.name),
                value: Vec::new(),
            });
        }
    }
    out.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

fn encode_xattr_entries(
    storage: &mut [u8],
    entry_start: usize,
    value_base: usize,
    attrs: &[InlineXattr],
) -> Result<bool> {
    let mut entry_offset = entry_start;
    let mut value_offset = entry_start;
    for attr in attrs {
        let suffix = user_xattr_suffix(&attr.name)?;
        value_offset = align4(
            value_offset
                .checked_add(EXT4_XATTR_ENTRY_FIXED)
                .and_then(|v| v.checked_add(suffix.len()))
                .ok_or(Ext4FormatError::Corrupt)?,
        );
    }
    value_offset = value_offset
        .checked_add(4)
        .ok_or(Ext4FormatError::Corrupt)?;

    if value_offset > storage.len() {
        return Ok(false);
    }
    for attr in attrs {
        let suffix = user_xattr_suffix(&attr.name)?;
        let entry_end = entry_offset
            .checked_add(EXT4_XATTR_ENTRY_FIXED)
            .and_then(|v| v.checked_add(suffix.len()))
            .ok_or(Ext4FormatError::Corrupt)?;
        if entry_end > storage.len() || suffix.len() > u8::MAX as usize {
            return Ok(false);
        }

        let value_start = if attr.value.is_empty() {
            0
        } else {
            let start = align4(value_offset);
            let value_end = start
                .checked_add(attr.value.len())
                .ok_or(Ext4FormatError::Corrupt)?;
            let padded_end = align4(value_end);
            if padded_end > storage.len() {
                return Ok(false);
            }
            storage[start..value_end].copy_from_slice(&attr.value);
            value_offset = padded_end;
            start
        };
        let value_offs = if attr.value.is_empty() {
            0
        } else {
            value_start
                .checked_sub(value_base)
                .ok_or(Ext4FormatError::Corrupt)?
        };
        if value_offs > u16::MAX as usize || attr.value.len() > u32::MAX as usize {
            return Ok(false);
        }

        storage[entry_offset] = suffix.len() as u8;
        storage[entry_offset + 1] = EXT4_XATTR_INDEX_USER;
        write_u16_le(storage, entry_offset + 2, value_offs as u16)?;
        write_u32_le(storage, entry_offset + 4, 0)?;
        write_u32_le(storage, entry_offset + 8, attr.value.len() as u32)?;
        write_u32_le(storage, entry_offset + 12, 0)?;
        storage[entry_offset + 16..entry_offset + 16 + suffix.len()].copy_from_slice(suffix);
        entry_offset = align4(entry_end);
    }
    if entry_offset
        .checked_add(4)
        .is_none_or(|end| end > storage.len())
    {
        return Ok(false);
    }
    storage[entry_offset..entry_offset + 4].fill(0);
    Ok(true)
}

fn parse_raw_entry<'a>(
    storage: &'a [u8],
    offset: usize,
    entries: &mut Vec<RawEntry<'a>>,
) -> Result<usize> {
    let entry_fixed = storage
        .get(offset..offset + EXT4_XATTR_ENTRY_FIXED)
        .ok_or(Ext4FormatError::Truncated)?;
    let name_len = entry_fixed[0] as usize;
    let name_index = entry_fixed[1];
    let value_offs = read_u16_le(storage, offset + 2)? as usize;
    let value_inum = read_u32_le(storage, offset + 4)?;
    let value_size = read_u32_le(storage, offset + 8)? as usize;
    let name_start = offset + EXT4_XATTR_ENTRY_FIXED;
    let name_end = name_start
        .checked_add(name_len)
        .ok_or(Ext4FormatError::Corrupt)?;
    let Some(name) = storage.get(name_start..name_end) else {
        return Err(Ext4FormatError::Truncated);
    };
    if name.contains(&0) {
        return Err(Ext4FormatError::Corrupt);
    }
    entries.push(RawEntry {
        name_index,
        name,
        value_offs,
        value_inum,
        value_size,
    });
    Ok(name_end)
}

fn user_xattr_name(suffix: &[u8]) -> Vec<u8> {
    let mut name = Vec::with_capacity(b"user.".len() + suffix.len());
    name.extend_from_slice(b"user.");
    name.extend_from_slice(suffix);
    name
}

fn user_xattr_suffix(name: &[u8]) -> Result<&[u8]> {
    name.strip_prefix(b"user.")
        .filter(|suffix| !suffix.is_empty())
        .ok_or(Ext4FormatError::Unsupported)
}

fn align4(value: usize) -> usize {
    (value + 3) & !3
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ondisk::{write_u16_le, write_u32_le, Inode, Superblock};

    const INODE_SIZE: usize = 256;
    const INLINE_VALUE_BASE: usize = 4;

    fn encode_entry(
        storage: &mut [u8],
        offset: usize,
        name_index: u8,
        name: &[u8],
        value_offs: u16,
        value: &[u8],
    ) -> usize {
        storage[offset] = name.len() as u8;
        storage[offset + 1] = name_index;
        write_u16_le(storage, offset + 2, value_offs).unwrap();
        write_u32_le(storage, offset + 4, 0).unwrap();
        write_u32_le(storage, offset + 8, value.len() as u32).unwrap();
        write_u32_le(storage, offset + 12, 0).unwrap();
        storage[offset + 16..offset + 16 + name.len()].copy_from_slice(name);
        let value_start = INLINE_VALUE_BASE + value_offs as usize;
        storage[value_start..value_start + value.len()].copy_from_slice(value);
        align4(offset + 16 + name.len())
    }

    fn encode_block_entry(
        block: &mut [u8],
        offset: usize,
        name_index: u8,
        name: &[u8],
        value_offs: u16,
        value: &[u8],
    ) -> usize {
        block[offset] = name.len() as u8;
        block[offset + 1] = name_index;
        write_u16_le(block, offset + 2, value_offs).unwrap();
        write_u32_le(block, offset + 4, 0).unwrap();
        write_u32_le(block, offset + 8, value.len() as u32).unwrap();
        write_u32_le(block, offset + 12, 0).unwrap();
        block[offset + 16..offset + 16 + name.len()].copy_from_slice(name);
        let value_start = value_offs as usize;
        block[value_start..value_start + value.len()].copy_from_slice(value);
        align4(offset + 16 + name.len())
    }

    #[test]
    fn parse_inline_user_xattr_from_inode_body() {
        let mut raw = [0u8; INODE_SIZE];
        let mut inode = Inode::default();
        inode.mode = Inode::S_IFREG | 0o644;
        inode.extra_isize = 32;
        inode.encode(&mut raw).unwrap();

        let storage_start = 128 + inode.extra_isize as usize;
        let storage = &mut raw[storage_start..];
        write_u32_le(storage, 0, EXT4_XATTR_MAGIC).unwrap();
        let next = encode_entry(storage, 4, EXT4_XATTR_INDEX_USER, b"alpha", 64, b"bravo");
        storage[next..next + 4].fill(0);

        let inode = Inode::parse(&raw).unwrap();
        let attrs = parse_inline_xattrs(&inode, &raw).unwrap();
        assert_eq!(attrs.len(), 1);
        assert_eq!(attrs[0].name, b"user.alpha");
        assert_eq!(attrs[0].value, b"bravo");
    }

    #[test]
    fn parse_inline_xattrs_lists_names_lexicographically_and_filters_unknown_namespaces() {
        let mut raw = [0u8; INODE_SIZE];
        let mut inode = Inode::default();
        inode.mode = Inode::S_IFREG | 0o644;
        inode.extra_isize = 32;
        inode.encode(&mut raw).unwrap();

        let storage_start = 128 + inode.extra_isize as usize;
        let storage = &mut raw[storage_start..];
        write_u32_le(storage, 0, EXT4_XATTR_MAGIC).unwrap();
        let next = encode_entry(storage, 4, EXT4_XATTR_INDEX_USER, b"zeta", 76, b"last");
        let next = encode_entry(
            storage,
            next,
            EXT4_XATTR_INDEX_SECURITY,
            b"selinux",
            84,
            b"ignored",
        );
        let next = encode_entry(storage, next, EXT4_XATTR_INDEX_USER, b"alpha", 88, b"a");
        storage[next..next + 4].fill(0);

        let inode = Inode::parse(&raw).unwrap();
        let attrs = parse_inline_xattrs(&inode, &raw).unwrap();
        assert_eq!(attrs.len(), 2);
        assert_eq!(attrs[0].name, b"user.alpha");
        assert_eq!(attrs[1].name, b"user.zeta");
    }

    #[test]
    fn parse_inline_xattrs_rejects_ea_inode_values_for_now() {
        let mut raw = [0u8; INODE_SIZE];
        let mut inode = Inode::default();
        inode.mode = Inode::S_IFREG | 0o644;
        inode.extra_isize = 32;
        inode.encode(&mut raw).unwrap();

        let storage_start = 128 + inode.extra_isize as usize;
        let storage = &mut raw[storage_start..];
        write_u32_le(storage, 0, EXT4_XATTR_MAGIC).unwrap();
        storage[4] = 5;
        storage[5] = EXT4_XATTR_INDEX_USER;
        write_u16_le(storage, 6, 0).unwrap();
        write_u32_le(storage, 8, 42).unwrap();
        write_u32_le(storage, 12, 5).unwrap();
        storage[20..25].copy_from_slice(b"alpha");

        let inode = Inode::parse(&raw).unwrap();
        assert_eq!(
            parse_inline_xattrs(&inode, &raw),
            Err(crate::Ext4FormatError::Unsupported)
        );
    }

    #[test]
    fn parse_external_xattr_block_reads_user_attrs() {
        let sb = Superblock::default();
        let mut block = [0u8; 4096];
        write_u32_le(&mut block, 0, EXT4_XATTR_MAGIC).unwrap();
        write_u32_le(&mut block, 4, 1).unwrap();
        write_u32_le(&mut block, 8, 1).unwrap();
        let next = encode_block_entry(&mut block, 32, EXT4_XATTR_INDEX_USER, b"zeta", 128, b"last");
        let next = encode_block_entry(
            &mut block,
            next,
            EXT4_XATTR_INDEX_SECURITY,
            b"selinux",
            132,
            b"ignored",
        );
        let next = encode_block_entry(
            &mut block,
            next,
            EXT4_XATTR_INDEX_USER,
            b"alpha",
            140,
            b"bravo",
        );
        block[next..next + 4].fill(0);

        let attrs = parse_external_xattr_block(&sb, 7, &block).unwrap();
        assert_eq!(attrs.len(), 2);
        assert_eq!(attrs[0].name, b"user.alpha");
        assert_eq!(attrs[0].value, b"bravo");
        assert_eq!(attrs[1].name, b"user.zeta");
        assert_eq!(attrs[1].value, b"last");
    }

    #[test]
    fn parse_external_xattr_block_validates_metadata_checksum_when_enabled() {
        let mut sb = Superblock {
            feature_ro_compat: Superblock::FEATURE_RO_COMPAT_METADATA_CSUM,
            checksum_seed: 0x1234_5678,
            ..Superblock::default()
        };
        let mut block = [0u8; 4096];
        write_u32_le(&mut block, 0, EXT4_XATTR_MAGIC).unwrap();
        write_u32_le(&mut block, 4, 1).unwrap();
        write_u32_le(&mut block, 8, 1).unwrap();
        let next = encode_block_entry(
            &mut block,
            32,
            EXT4_XATTR_INDEX_USER,
            b"alpha",
            96,
            b"bravo",
        );
        block[next..next + 4].fill(0);
        let checksum = xattr_block_checksum(&sb, 9, &block).unwrap();
        write_u32_le(&mut block, 16, checksum).unwrap();

        assert!(parse_external_xattr_block(&sb, 9, &block).is_ok());
        sb.checksum_seed = 0x8765_4321;
        assert_eq!(
            parse_external_xattr_block(&sb, 9, &block),
            Err(crate::Ext4FormatError::Corrupt)
        );
    }

    #[test]
    fn encode_inline_xattrs_round_trips_with_linux_value_base() {
        let mut raw = [0u8; INODE_SIZE];
        let mut inode = Inode::default();
        inode.mode = Inode::S_IFREG | 0o644;
        inode.extra_isize = 32;
        inode.encode(&mut raw).unwrap();

        let attrs = [
            InlineXattr {
                name: b"user.alpha".to_vec(),
                value: b"bravo".to_vec(),
            },
            InlineXattr {
                name: b"user.zeta".to_vec(),
                value: b"last".to_vec(),
            },
        ];
        assert!(encode_inline_xattrs(&inode, &mut raw, &attrs).unwrap());

        let inode = Inode::parse(&raw).unwrap();
        assert_eq!(parse_inline_xattrs(&inode, &raw).unwrap(), attrs);
    }

    #[test]
    fn encode_external_xattr_block_round_trips_and_refreshes_checksum() {
        let sb = Superblock {
            feature_ro_compat: Superblock::FEATURE_RO_COMPAT_METADATA_CSUM,
            checksum_seed: 0x1234_5678,
            ..Superblock::default()
        };
        let attrs = [
            InlineXattr {
                name: b"user.alpha".to_vec(),
                value: b"bravo".to_vec(),
            },
            InlineXattr {
                name: b"user.zeta".to_vec(),
                value: b"last".to_vec(),
            },
        ];
        let mut block = [0u8; 4096];

        encode_external_xattr_block(&sb, 11, &mut block, &attrs).unwrap();

        assert_eq!(
            read_u32_le(&block, 16).unwrap(),
            xattr_block_checksum(&sb, 11, &block).unwrap()
        );
        assert_eq!(parse_external_xattr_block(&sb, 11, &block).unwrap(), attrs);
    }
}
