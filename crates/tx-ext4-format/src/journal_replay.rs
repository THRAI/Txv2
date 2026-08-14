//! Mount-time replay for the strictly supported JBD2 feature profiles.
//!
//! Recovery is deliberately expressed over the neutral [`BlockImage`] and a
//! discovered [`JournalGeometry`]. It has no mount, page-cache, or scheduling
//! state, so callers can replay the image before exposing the filesystem.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

use crate::journal::{
    Jbd2Commit, Jbd2Descriptor, Jbd2Header, Jbd2Revoke, Jbd2Tag, JBD2_BLOCK_COMMIT,
    JBD2_BLOCK_DESCRIPTOR, JBD2_BLOCK_REVOKE, JBD2_MAGIC,
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

struct PlannedUpdate {
    sequence: u32,
    tag: Jbd2Tag,
    payload: Page4K,
}

struct JournalScan {
    updates: Vec<PlannedUpdate>,
    latest_revokes: BTreeMap<u64, u32>,
    transactions: u32,
    next_sequence: u32,
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
    let scan = scan_journal(image, geometry)?;
    // An uncommitted tail is the normal result of a power loss. The scan keeps
    // only the committed prefix, so recovery may replay that prefix and then
    // publish a clean journal that discards the incomplete transaction. Only
    // malformed or unsupported records return an error before this point.
    let clean_page = prepare_clean_page(geometry, scan.next_sequence)?;
    let replay = apply_scan(image, &scan)?;
    let (superblock_block, page) = clean_page;
    write_clean_page(image, superblock_block, &page)?;
    Ok(RecoveryReport::Replayed(replay))
}

/// Replay consecutive, committed JBD2 transactions from `geometry` using the
/// exact record-layout feature token parsed from its journal superblock.
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
    let scan = scan_journal(image, geometry)?;
    apply_scan(image, &scan)
}

/// Perform Linux-style discovery before allowing any home-block mutation.
///
/// The scan accepts multiple descriptor and revoke records in one transaction,
/// records only transactions terminated by a valid commit record, and builds
/// the final revoke table across every committed transaction in the ring. All
/// decoded payload storage is bounded by the number of pages in that ring.
fn scan_journal<I: BlockImage>(image: &I, geometry: &JournalGeometry) -> Result<JournalScan> {
    validate_geometry(image, geometry)?;
    let mut expected_sequence = geometry.superblock.sequence;
    if geometry.superblock.start == 0 {
        return Ok(JournalScan {
            updates: Vec::new(),
            latest_revokes: BTreeMap::new(),
            transactions: 0,
            next_sequence: expected_sequence,
        });
    }

    let mut cursor = geometry.superblock.start as usize;
    let mut scanned = 0usize;
    let limit = geometry.blocks.len() - geometry.superblock.first as usize;
    let mut transactions = 0u32;
    let mut updates = Vec::new();
    let mut latest_revokes = BTreeMap::new();

    while scanned < limit {
        let first_page = read_log_page(image, geometry, cursor)?;
        let first_header = match Jbd2Header::parse(&first_page) {
            Ok(header) => header,
            Err(Ext4FormatError::BadMagic) => {
                return Ok(JournalScan {
                    updates,
                    latest_revokes,
                    transactions,
                    next_sequence: expected_sequence,
                });
            }
            Err(error) => return Err(error),
        };
        if first_header.sequence != expected_sequence {
            break;
        }

        let mut transaction_cursor = cursor;
        let mut transaction_blocks = 0usize;
        let mut transaction_updates = Vec::new();
        let mut transaction_revokes = Vec::new();
        let mut first_control = Some((first_page, first_header));
        let committed = loop {
            if transaction_blocks >= limit - scanned {
                break false;
            }
            let (record_page, header) = if let Some(first) = first_control.take() {
                first
            } else {
                let page = read_log_page(image, geometry, transaction_cursor)?;
                let header = match Jbd2Header::parse(&page) {
                    Ok(header) => header,
                    Err(Ext4FormatError::BadMagic) => break false,
                    Err(error) => return Err(error),
                };
                (page, header)
            };
            if header.sequence != expected_sequence {
                break false;
            }

            match header.block_type {
                JBD2_BLOCK_DESCRIPTOR => {
                    let descriptor = match Jbd2Descriptor::parse_with_features(
                        &record_page,
                        geometry.features,
                    ) {
                        Err(Ext4FormatError::Truncated) => {
                            return Err(Ext4FormatError::Corrupt);
                        }
                        result => result?,
                    };
                    validate_tags(&descriptor, geometry)?;
                    transaction_blocks = transaction_blocks
                        .checked_add(1)
                        .ok_or(Ext4FormatError::OutOfBounds)?;
                    transaction_cursor = advance(geometry, transaction_cursor);
                    if descriptor.tags.len() > limit - scanned - transaction_blocks {
                        break false;
                    }
                    for tag in descriptor.tags {
                        let payload = read_log_page(image, geometry, transaction_cursor)?;
                        transaction_updates.push(PlannedUpdate {
                            sequence: expected_sequence,
                            tag,
                            payload,
                        });
                        transaction_blocks += 1;
                        transaction_cursor = advance(geometry, transaction_cursor);
                    }
                }
                JBD2_BLOCK_REVOKE => {
                    let revoke = Jbd2Revoke::parse_with_features(&record_page, geometry.features)?;
                    transaction_revokes.extend(revoke.blocks);
                    transaction_blocks = transaction_blocks
                        .checked_add(1)
                        .ok_or(Ext4FormatError::OutOfBounds)?;
                    transaction_cursor = advance(geometry, transaction_cursor);
                }
                JBD2_BLOCK_COMMIT => {
                    let commit = Jbd2Commit::parse(&record_page)?;
                    validate_commit_checksum(&commit)?;
                    transaction_blocks = transaction_blocks
                        .checked_add(1)
                        .ok_or(Ext4FormatError::OutOfBounds)?;
                    transaction_cursor = advance(geometry, transaction_cursor);
                    break true;
                }
                _ => return Err(Ext4FormatError::Unsupported),
            }
        };

        if !committed {
            return Ok(JournalScan {
                updates,
                latest_revokes,
                transactions,
                // The matching transaction header proves that this sequence
                // was already exposed in the ring. Skip it when restarting so
                // a stale commit record with the same ID cannot make a later,
                // partially overwritten transaction appear committed.
                next_sequence: expected_sequence.wrapping_add(1).max(1),
            });
        }

        transactions = transactions
            .checked_add(1)
            .ok_or(Ext4FormatError::OutOfBounds)?;
        updates.extend(transaction_updates);
        for block in transaction_revokes {
            latest_revokes.insert(block, expected_sequence);
        }
        expected_sequence = expected_sequence.wrapping_add(1).max(1);
        cursor = transaction_cursor;
        scanned = scanned
            .checked_add(transaction_blocks)
            .ok_or(Ext4FormatError::OutOfBounds)?;
    }

    Ok(JournalScan {
        updates,
        latest_revokes,
        transactions,
        next_sequence: expected_sequence,
    })
}

fn apply_scan<I: BlockImage>(image: &mut I, scan: &JournalScan) -> Result<JournalReplayReport> {
    let replayable = scan
        .updates
        .iter()
        .filter(|update| !is_revoked(update, &scan.latest_revokes));
    for update in replayable.clone() {
        if update.tag.target_block >= image.total_blocks() {
            return Err(Ext4FormatError::OutOfBounds);
        }
    }
    let blocks_replayed =
        u32::try_from(replayable.count()).map_err(|_| Ext4FormatError::OutOfBounds)?;

    for update in scan
        .updates
        .iter()
        .filter(|update| !is_revoked(update, &scan.latest_revokes))
    {
        let mut payload = update.payload;
        if update.tag.escaped {
            payload[..4].copy_from_slice(&JBD2_MAGIC.to_be_bytes());
        }
        image.write_block(update.tag.target_block, &payload)?;
        image.invalidate_block(update.tag.target_block);
    }
    if scan.transactions != 0 {
        image.barrier()?;
    }

    Ok(JournalReplayReport {
        transactions: scan.transactions,
        blocks_replayed,
        next_sequence: scan.next_sequence,
    })
}

fn is_revoked(update: &PlannedUpdate, latest_revokes: &BTreeMap<u64, u32>) -> bool {
    latest_revokes
        .get(&update.tag.target_block)
        .is_some_and(|revoke_sequence| sequence_geq(*revoke_sequence, update.sequence))
}

/// JBD2 transaction IDs wrap. The journal ring is far smaller than half the
/// sequence space, so Linux's signed-difference comparison is unambiguous.
fn sequence_geq(candidate: u32, reference: u32) -> bool {
    candidate.wrapping_sub(reference) as i32 >= 0
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
    validate_geometry(image, geometry)?;
    let (superblock_block, page) = prepare_clean_page(geometry, next_sequence)?;
    write_clean_page(image, superblock_block, &page)
}

fn prepare_clean_page(geometry: &JournalGeometry, next_sequence: u32) -> Result<(u64, Page4K)> {
    let mut page = geometry
        .superblock_page
        .ok_or(Ext4FormatError::Unsupported)?;
    let (observed, features) = crate::journal::Jbd2Superblock::parse_with_features(&page)?;
    if observed != geometry.superblock || features != geometry.features {
        return Err(Ext4FormatError::Corrupt);
    }
    geometry
        .superblock
        .write_state(&mut page, next_sequence.max(1), 0)?;
    let superblock_block = *geometry.blocks.first().ok_or(Ext4FormatError::Corrupt)?;
    Ok((superblock_block, page))
}

fn write_clean_page<I: BlockImage>(
    image: &mut I,
    superblock_block: u64,
    page: &Page4K,
) -> Result<()> {
    image.write_block(superblock_block, page)?;
    image.invalidate_block(superblock_block);
    image.barrier()
}

fn validate_geometry<I: BlockImage>(image: &I, geometry: &JournalGeometry) -> Result<()> {
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
    let mut physical_blocks = BTreeSet::new();
    for block in &geometry.blocks {
        if *block >= image.total_blocks() || !physical_blocks.insert(*block) {
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
