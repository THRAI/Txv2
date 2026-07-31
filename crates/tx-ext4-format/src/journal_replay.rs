//! Mount-time replay for the supported legacy JBD2 transaction format.
//!
//! Recovery is deliberately expressed over the neutral [`BlockImage`] and a
//! discovered [`JournalGeometry`]. It has no mount, page-cache, or scheduling
//! state, so callers can replay the image before exposing the filesystem.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use crate::journal::{
    Jbd2Commit, Jbd2Descriptor, Jbd2Header, Jbd2Revoke, Jbd2Tag, JBD2_BLOCK_COMMIT,
    JBD2_BLOCK_DESCRIPTOR, JBD2_BLOCK_REVOKE, JBD2_MAGIC,
};
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
/// transaction. Recovery first records every committed transaction and revoke
/// page, then applies payloads that were not superseded by a later revoke.
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
    let mut recovered = Vec::new();

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
        let mut revoked_blocks = Vec::new();
        loop {
            let record_page = read_log_page(image, geometry, payload_cursor)?;
            let record_header = match Jbd2Header::parse(&record_page) {
                Ok(header) => header,
                Err(Ext4FormatError::BadMagic) | Err(Ext4FormatError::Corrupt) => break,
                Err(error) => return Err(error),
            };
            if record_header.sequence != descriptor.header.sequence {
                break;
            }
            match record_header.block_type {
                JBD2_BLOCK_REVOKE => {
                    revoked_blocks.extend(Jbd2Revoke::parse(&record_page)?.blocks);
                    record_blocks = record_blocks
                        .checked_add(1)
                        .ok_or(Ext4FormatError::OutOfBounds)?;
                    if record_blocks > limit - scanned {
                        break;
                    }
                    payload_cursor = advance(geometry, payload_cursor);
                }
                JBD2_BLOCK_COMMIT => {
                    let commit = Jbd2Commit::parse(&record_page)?;
                    if commit.header.sequence != descriptor.header.sequence {
                        break;
                    }
                    recovered.push(ReplayTransaction {
                        sequence: descriptor.header.sequence,
                        tags: descriptor.tags,
                        payloads,
                        revoked_blocks,
                    });
                    transactions = transactions
                        .checked_add(1)
                        .ok_or(Ext4FormatError::OutOfBounds)?;
                    expected_sequence = expected_sequence.wrapping_add(1).max(1);
                    cursor = advance(geometry, payload_cursor);
                    scanned = scanned
                        .checked_add(record_blocks)
                        .ok_or(Ext4FormatError::OutOfBounds)?;
                    break;
                }
                _ => break,
            }
        }
        if expected_sequence != header.sequence.wrapping_add(1).max(1) {
            break;
        }
    }

    let mut revoke_sequences: BTreeMap<u32, u32> = BTreeMap::new();
    for transaction in &recovered {
        for block in &transaction.revoked_blocks {
            revoke_sequences
                .entry(*block)
                .and_modify(|sequence| {
                    if sequence_after(transaction.sequence, *sequence) {
                        *sequence = transaction.sequence;
                    }
                })
                .or_insert(transaction.sequence);
        }
    }
    for transaction in recovered {
        let mut applied = false;
        for (tag, mut payload) in transaction.tags.iter().zip(transaction.payloads) {
            if revoke_sequences
                .get(&tag.target_block)
                .is_some_and(|sequence| sequence_after_or_equal(*sequence, transaction.sequence))
            {
                continue;
            }
            if tag.escaped {
                payload[..4].copy_from_slice(&JBD2_MAGIC.to_be_bytes());
            }
            image.write_block(tag.target_block as u64, &payload)?;
            blocks_replayed = blocks_replayed
                .checked_add(1)
                .ok_or(Ext4FormatError::OutOfBounds)?;
            applied = true;
        }
        if applied {
            image.barrier()?;
        }
    }

    Ok(JournalReplayReport {
        transactions,
        blocks_replayed,
        next_sequence: expected_sequence,
    })
}

struct ReplayTransaction {
    sequence: u32,
    tags: Vec<Jbd2Tag>,
    payloads: Vec<Page4K>,
    revoked_blocks: Vec<u32>,
}

/// JBD2 transaction identifiers form a wrapping sequence space. A valid
/// recovery window is far smaller than half that space, so this establishes a
/// stable before/after order across one wrap.
fn sequence_after(candidate: u32, reference: u32) -> bool {
    candidate != reference && candidate.wrapping_sub(reference) < 0x8000_0000
}

fn sequence_after_or_equal(candidate: u32, reference: u32) -> bool {
    candidate == reference || sequence_after(candidate, reference)
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
