//! Mount-time replay for the supported legacy JBD2 transaction format.
//!
//! Recovery is deliberately expressed over the neutral [`BlockImage`] and a
//! discovered [`JournalGeometry`]. It has no mount, page-cache, or scheduling
//! state, so callers can replay the image before exposing the filesystem.

use crate::journal::{Jbd2Commit, Jbd2Descriptor, Jbd2Header, JBD2_BLOCK_DESCRIPTOR, JBD2_MAGIC};
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

/// Replay consecutive, committed legacy JBD2 transactions from `geometry`.
///
/// A descriptor is never applied until every tagged metadata page and its
/// matching commit record have been read. An incomplete tail is normal after a
/// power loss and terminates the scan without modifying home blocks for that
/// transaction. This first recovery slice intentionally rejects delete/revoke
/// tags: free/reuse safety is not yet part of the supported write surface.
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

    while scanned < limit {
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
        let record_blocks = descriptor
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
        let commit_page = read_log_page(image, geometry, payload_cursor)?;
        let commit = match Jbd2Commit::parse(&commit_page) {
            Ok(commit) => commit,
            Err(Ext4FormatError::BadMagic) | Err(Ext4FormatError::Corrupt) => break,
            Err(error) => return Err(error),
        };
        if commit.header.sequence != descriptor.header.sequence {
            break;
        }

        for (tag, mut payload) in descriptor.tags.iter().zip(payloads) {
            if tag.escaped {
                payload[..4].copy_from_slice(&JBD2_MAGIC.to_be_bytes());
            }
            image.write_block(tag.target_block as u64, &payload)?;
            blocks_replayed = blocks_replayed
                .checked_add(1)
                .ok_or(Ext4FormatError::OutOfBounds)?;
        }
        image.barrier()?;
        transactions = transactions
            .checked_add(1)
            .ok_or(Ext4FormatError::OutOfBounds)?;
        expected_sequence = expected_sequence.wrapping_add(1).max(1);
        cursor = advance(geometry, payload_cursor);
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
