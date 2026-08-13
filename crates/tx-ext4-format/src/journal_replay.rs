//! Mount-time replay for the supported legacy JBD2 transaction format.
//!
//! Recovery is deliberately expressed over the neutral [`BlockImage`] and a
//! discovered [`JournalGeometry`]. It has no mount, page-cache, or scheduling
//! state, so callers can replay the image before exposing the filesystem.

use crate::journal::{
    Jbd2Commit, Jbd2Descriptor, Jbd2Header, Jbd2Revoke, JBD2_BLOCK_COMMIT, JBD2_BLOCK_DESCRIPTOR,
    JBD2_BLOCK_REVOKE, JBD2_MAGIC,
};
use crate::ondisk::Superblock;
use crate::pager::{BlockImage, JournalGeometry, Page4K};
use crate::{Ext4FormatError, Result};

/// Result of one bounded journal scan.
///
/// `next_sequence` is the first sequence that was not replayed. A mount uses
/// it to initialize its future transaction allocator after it has made the
/// recovered home blocks durable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JournalReplayReport {
    pub transactions: u32,
    pub blocks_replayed: u32,
    pub next_sequence: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryReport {
    NotRequired,
    Replayed(JournalReplayReport),
}

/// Replay and clean a discovered journal only when ext4's recovery-required
/// bit says that the mount did not complete a clean detach.
pub fn recover_if_required<I: BlockImage>(
    image: &mut I,
    superblock: &Superblock,
    geometry: &JournalGeometry,
) -> Result<RecoveryReport> {
    if !superblock.needs_recovery() {
        return Ok(RecoveryReport::NotRequired);
    }
    let replay = replay_journal(image, geometry)?;
    clean_replayed_journal(image, geometry, replay.next_sequence)?;
    Ok(RecoveryReport::Replayed(replay))
}

/// Replay consecutive, committed legacy JBD2 transactions from `geometry`.
///
/// A descriptor is never applied until every tagged metadata page and its
/// matching commit record have been read. An incomplete tail is normal after a
/// power loss and terminates the scan without modifying home blocks for that
/// transaction. A transaction may contain zero or more revoke pages between
/// the metadata copies and commit record. Its revokes suppress matching stale
/// after-images from that transaction; allocator reuse remains a higher-layer
/// lifecycle responsibility.
pub fn replay_journal<I: BlockImage>(
    image: &mut I,
    geometry: &JournalGeometry,
) -> Result<JournalReplayReport> {
    validate_geometry(geometry)?;
    let mut expected_sequence = geometry.superblock.sequence;
    if geometry.superblock.start == 0 {
        return Ok(JournalReplayReport {
            transactions: 0,
            blocks_replayed: 0,
            next_sequence: expected_sequence,
        });
    }

    let mut cursor = geometry.superblock.start as usize;
    let mut scanned = 0usize;
    let limit = geometry.blocks.len() - geometry.superblock.first as usize;
    let mut transactions = 0u32;
    let mut blocks_replayed = 0u32;

    'scan: while scanned < limit {
        let descriptor_page = read_log_page(image, geometry, cursor)?;
        let header = match Jbd2Header::parse(&descriptor_page) {
            Ok(header) if header.block_type == JBD2_BLOCK_DESCRIPTOR => header,
            Ok(_) | Err(Ext4FormatError::BadMagic) => break,
            Err(error) => return Err(error),
        };
        if header.sequence != expected_sequence {
            break;
        }
        let descriptor = Jbd2Descriptor::parse_legacy(&descriptor_page)?;
        validate_tags(&descriptor, geometry)?;
        let mut record_blocks = descriptor
            .tags
            .len()
            .checked_add(2)
            .ok_or(Ext4FormatError::OutOfBounds)?;
        if record_blocks > limit - scanned {
            break;
        }

        let mut payload_cursor = advance(geometry, cursor);
        let mut payloads = alloc::vec::Vec::with_capacity(descriptor.tags.len());
        for _ in &descriptor.tags {
            payloads.push(read_log_page(image, geometry, payload_cursor)?);
            payload_cursor = advance(geometry, payload_cursor);
        }
        let mut commit_cursor = payload_cursor;
        let mut revoked_blocks = alloc::vec::Vec::new();
        let commit = loop {
            let record_page = read_log_page(image, geometry, commit_cursor)?;
            let header = match Jbd2Header::parse(&record_page) {
                Ok(header) => header,
                Err(Ext4FormatError::BadMagic) | Err(Ext4FormatError::Corrupt) => break 'scan,
                Err(error) => return Err(error),
            };
            match header.block_type {
                JBD2_BLOCK_COMMIT => match Jbd2Commit::parse(&record_page) {
                    Ok(commit) => break commit,
                    Err(Ext4FormatError::BadMagic) | Err(Ext4FormatError::Corrupt) => break 'scan,
                    Err(error) => return Err(error),
                },
                JBD2_BLOCK_REVOKE => {
                    record_blocks = record_blocks
                        .checked_add(1)
                        .ok_or(Ext4FormatError::OutOfBounds)?;
                    if record_blocks > limit - scanned {
                        break 'scan;
                    }
                    let revoke = match Jbd2Revoke::parse(&record_page) {
                        Ok(revoke) => revoke,
                        Err(Ext4FormatError::BadMagic) | Err(Ext4FormatError::Corrupt) => {
                            break 'scan;
                        }
                        Err(error) => return Err(error),
                    };
                    if revoke.header.sequence != descriptor.header.sequence {
                        break 'scan;
                    }
                    revoked_blocks.extend(revoke.blocks);
                    commit_cursor = advance(geometry, commit_cursor);
                }
                _ => break 'scan,
            }
        };
        if commit.header.sequence != descriptor.header.sequence {
            break;
        }
        validate_commit_checksum(&commit)?;

        for (tag, mut payload) in descriptor.tags.iter().zip(payloads) {
            if revoked_blocks.contains(&tag.target_block) {
                continue;
            }
            if tag.escaped {
                payload[..4].copy_from_slice(&JBD2_MAGIC.to_be_bytes());
            }
            image.write_block(tag.target_block as u64, &payload)?;
            image.invalidate_block(tag.target_block as u64);
            blocks_replayed = blocks_replayed
                .checked_add(1)
                .ok_or(Ext4FormatError::OutOfBounds)?;
        }
        image.barrier()?;
        transactions = transactions
            .checked_add(1)
            .ok_or(Ext4FormatError::OutOfBounds)?;
        expected_sequence = expected_sequence.wrapping_add(1).max(1);
        cursor = advance(geometry, commit_cursor);
        scanned = scanned
            .checked_add(record_blocks)
            .ok_or(Ext4FormatError::OutOfBounds)?;
    }

    Ok(JournalReplayReport {
        transactions,
        blocks_replayed,
        next_sequence: expected_sequence,
    })
}

/// The bounded legacy journal profile has no transaction checksum fields. A
/// journal that declares checksums uses a different record layout and cannot
/// be accepted until that format and its CRC contract are implemented.
fn validate_commit_checksum(commit: &Jbd2Commit) -> Result<()> {
    if commit.checksum_type == 0
        && commit.checksum_size == 0
        && commit.checksums.iter().all(|checksum| *checksum == 0)
    {
        return Ok(());
    }
    Err(Ext4FormatError::Unsupported)
}

/// Publish the clean journal state after replay has made recovered home blocks
/// durable. This prevents the next mount from treating already checkpointed
/// records as an active log.
pub fn clean_replayed_journal<I: BlockImage>(
    image: &mut I,
    geometry: &JournalGeometry,
    next_sequence: u32,
) -> Result<()> {
    validate_geometry(geometry)?;
    let mut page = geometry
        .superblock_page
        .ok_or(Ext4FormatError::Unsupported)?;
    geometry
        .superblock
        .write_state(&mut page, next_sequence.max(1), 0)?;
    let superblock_block = *geometry.blocks.first().ok_or(Ext4FormatError::Corrupt)?;
    image.write_block(superblock_block, &page)?;
    image.invalidate_block(superblock_block);
    image.barrier()
}

fn validate_geometry(geometry: &JournalGeometry) -> Result<()> {
    let max_len = geometry.superblock.max_len as usize;
    let first = geometry.superblock.first as usize;
    if max_len != geometry.blocks.len() || max_len < 2 || first == 0 || first >= max_len {
        return Err(Ext4FormatError::Corrupt);
    }
    if geometry.superblock.start != 0 {
        let start = geometry.superblock.start as usize;
        if start < first || start >= max_len {
            return Err(Ext4FormatError::Corrupt);
        }
    }
    Ok(())
}

fn validate_tags(descriptor: &Jbd2Descriptor, geometry: &JournalGeometry) -> Result<()> {
    for tag in &descriptor.tags {
        if tag.deleted
            || tag
                .uuid
                .is_some_and(|uuid| uuid != geometry.superblock.uuid)
        {
            return Err(Ext4FormatError::Unsupported);
        }
    }
    Ok(())
}

fn read_log_page<I: BlockImage>(
    image: &I,
    geometry: &JournalGeometry,
    logical: usize,
) -> Result<Page4K> {
    let physical = *geometry
        .blocks
        .get(logical)
        .ok_or(Ext4FormatError::OutOfBounds)?;
    let mut page = [0; crate::journal::JBD2_BLOCK_SIZE];
    image.read_block(physical, &mut page)?;
    Ok(page)
}

fn advance(geometry: &JournalGeometry, logical: usize) -> usize {
    if logical + 1 == geometry.blocks.len() {
        geometry.superblock.first as usize
    } else {
        logical + 1
    }
}
