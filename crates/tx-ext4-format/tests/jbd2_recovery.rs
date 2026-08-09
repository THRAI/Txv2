use tx_ext4_format::journal::{
    JBD2_BLOCK_SIZE, JBD2_BLOCK_SUPERBLOCK_V2, JBD2_MAGIC, Jbd2MetadataUpdate, Jbd2Revoke,
    Jbd2Superblock, Jbd2TransactionImage,
};
use tx_ext4_format::ondisk::{Superblock, crc32c_append};
use tx_ext4_format::pager::{BlockImage, JournalGeometry, Page4K};
use tx_ext4_format::{
    Ext4FormatError, RecoveryReport, clean_replayed_journal, recover_if_required, replay_journal,
};

#[derive(Clone)]
struct MemImage {
    blocks: Vec<Page4K>,
}

impl MemImage {
    fn new(blocks: usize) -> Self {
        Self {
            blocks: vec![[0; JBD2_BLOCK_SIZE]; blocks],
        }
    }

    fn block(&self, block: u64) -> &Page4K {
        &self.blocks[block as usize]
    }

    fn block_mut(&mut self, block: u64) -> &mut Page4K {
        &mut self.blocks[block as usize]
    }
}

impl BlockImage for MemImage {
    fn total_blocks(&self) -> u64 {
        self.blocks.len() as u64
    }

    fn read_block(&self, block: u64, out: &mut Page4K) -> tx_ext4_format::Result<()> {
        let source = self
            .blocks
            .get(block as usize)
            .ok_or(Ext4FormatError::OutOfBounds)?;
        out.copy_from_slice(source);
        Ok(())
    }

    fn write_block(&mut self, block: u64, data: &Page4K) -> tx_ext4_format::Result<()> {
        let target = self
            .blocks
            .get_mut(block as usize)
            .ok_or(Ext4FormatError::OutOfBounds)?;
        target.copy_from_slice(data);
        Ok(())
    }

    fn barrier(&mut self) -> tx_ext4_format::Result<()> {
        Ok(())
    }
}

fn geometry() -> JournalGeometry {
    JournalGeometry {
        superblock: Jbd2Superblock {
            block_type: JBD2_BLOCK_SUPERBLOCK_V2,
            block_size: JBD2_BLOCK_SIZE as u32,
            max_len: 8,
            first: 1,
            sequence: 42,
            start: 1,
            uuid: [0xA5; 16],
        },
        blocks: vec![40, 44, 48, 52, 56, 60, 64, 68],
        superblock_page: None,
    }
}

fn geometry_with_superblock_page() -> JournalGeometry {
    let mut geometry = geometry();
    let superblock = &geometry.superblock;
    let mut page = [0; JBD2_BLOCK_SIZE];
    page[..4].copy_from_slice(&JBD2_MAGIC.to_be_bytes());
    page[4..8].copy_from_slice(&superblock.block_type.to_be_bytes());
    page[12..16].copy_from_slice(&superblock.block_size.to_be_bytes());
    page[16..20].copy_from_slice(&superblock.max_len.to_be_bytes());
    page[20..24].copy_from_slice(&superblock.first.to_be_bytes());
    page[24..28].copy_from_slice(&superblock.sequence.to_be_bytes());
    page[28..32].copy_from_slice(&superblock.start.to_be_bytes());
    page[48..64].copy_from_slice(&superblock.uuid);
    geometry.superblock_page = Some(page);
    geometry
}

#[test]
fn replay_installs_a_committed_metadata_after_image() {
    let geometry = geometry();
    let mut image = MemImage::new(80);
    let after = [0x5A; JBD2_BLOCK_SIZE];
    let record = Jbd2TransactionImage::encode_legacy(
        42,
        geometry.superblock.uuid,
        vec![Jbd2MetadataUpdate::new(9, after)],
    )
    .unwrap();

    *image.block_mut(44) = record.descriptor;
    *image.block_mut(48) = record.metadata_blocks[0];
    *image.block_mut(52) = record.commit;

    let report = replay_journal(&mut image, &geometry).unwrap();

    assert_eq!(report.transactions, 1);
    assert_eq!(report.blocks_replayed, 1);
    assert_eq!(image.block(9), &after);
}

#[test]
fn replay_does_not_apply_a_committed_after_image_revoked_by_the_same_transaction() {
    let geometry = geometry();
    let mut image = MemImage::new(80);
    let before = [0x11; JBD2_BLOCK_SIZE];
    let stale_after_image = [0x5A; JBD2_BLOCK_SIZE];
    let record = Jbd2TransactionImage::encode_legacy_with_revokes(
        42,
        geometry.superblock.uuid,
        vec![Jbd2MetadataUpdate::new(9, stale_after_image)],
        vec![9],
    )
    .unwrap();

    *image.block_mut(9) = before;
    *image.block_mut(44) = record.descriptor;
    *image.block_mut(48) = record.metadata_blocks[0];
    *image.block_mut(52) = record.revokes[0];
    *image.block_mut(56) = record.commit;

    let report = replay_journal(&mut image, &geometry).unwrap();

    assert_eq!(report.transactions, 1);
    assert_eq!(report.blocks_replayed, 0);
    assert_eq!(image.block(9), &before);
}

#[test]
fn replay_honors_every_revoke_page_in_one_committed_transaction() {
    let geometry = geometry();
    let mut image = MemImage::new(80);
    let before = [0x11; JBD2_BLOCK_SIZE];
    let mut revoked_blocks: Vec<u32> = (100..).take(Jbd2Revoke::MAX_BLOCKS_PER_PAGE).collect();
    revoked_blocks.push(9);
    let record = Jbd2TransactionImage::encode_legacy_with_revokes(
        42,
        geometry.superblock.uuid,
        vec![Jbd2MetadataUpdate::new(9, [0x5A; JBD2_BLOCK_SIZE])],
        revoked_blocks,
    )
    .unwrap();

    assert_eq!(record.revokes.len(), 2);
    *image.block_mut(9) = before;
    *image.block_mut(44) = record.descriptor;
    *image.block_mut(48) = record.metadata_blocks[0];
    *image.block_mut(52) = record.revokes[0];
    *image.block_mut(56) = record.revokes[1];
    *image.block_mut(60) = record.commit;

    let report = replay_journal(&mut image, &geometry).unwrap();

    assert_eq!(report.transactions, 1);
    assert_eq!(report.blocks_replayed, 0);
    assert_eq!(image.block(9), &before);
}

#[test]
fn replay_rejects_a_commit_checksum_that_this_profile_cannot_validate() {
    let geometry = geometry();
    let mut image = MemImage::new(80);
    let after = [0x5A; JBD2_BLOCK_SIZE];
    let record = Jbd2TransactionImage::encode_legacy(
        42,
        geometry.superblock.uuid,
        vec![Jbd2MetadataUpdate::new(9, after)],
    )
    .unwrap();
    *image.block_mut(44) = record.descriptor;
    *image.block_mut(48) = record.metadata_blocks[0];
    *image.block_mut(52) = record.commit;
    image.block_mut(52)[12] = 4;
    image.block_mut(52)[13] = 4;
    image.block_mut(52)[16..20].copy_from_slice(&0xDEAD_BEEFu32.to_be_bytes());

    assert_eq!(
        replay_journal(&mut image, &geometry),
        Err(Ext4FormatError::Unsupported)
    );
    assert_eq!(image.block(9), &[0; JBD2_BLOCK_SIZE]);
}

#[test]
fn stale_journal_is_not_replayed_when_ext4_is_clean() {
    let geometry = geometry();
    let mut image = MemImage::new(80);
    let before = [0x11; JBD2_BLOCK_SIZE];
    let after = [0x5A; JBD2_BLOCK_SIZE];
    let record = Jbd2TransactionImage::encode_legacy(
        42,
        geometry.superblock.uuid,
        vec![Jbd2MetadataUpdate::new(9, after)],
    )
    .unwrap();
    *image.block_mut(9) = before;
    *image.block_mut(44) = record.descriptor;
    *image.block_mut(48) = record.metadata_blocks[0];
    *image.block_mut(52) = record.commit;

    let report = recover_if_required(&mut image, &Superblock::default(), &geometry).unwrap();

    assert_eq!(report, RecoveryReport::NotRequired);
    assert_eq!(image.block(9), &before);
}

#[test]
fn recovery_required_replays_and_cleans_the_discovered_journal() {
    let geometry = geometry_with_superblock_page();
    let mut image = MemImage::new(80);
    let after = [0x5A; JBD2_BLOCK_SIZE];
    let record = Jbd2TransactionImage::encode_legacy(
        42,
        geometry.superblock.uuid,
        vec![Jbd2MetadataUpdate::new(9, after)],
    )
    .unwrap();
    *image.block_mut(44) = record.descriptor;
    *image.block_mut(48) = record.metadata_blocks[0];
    *image.block_mut(52) = record.commit;

    let mut superblock = Superblock::default();
    superblock.feature_incompat |= Superblock::FEATURE_INCOMPAT_RECOVER;
    let report = recover_if_required(&mut image, &superblock, &geometry).unwrap();

    assert_eq!(
        report,
        RecoveryReport::Replayed(tx_ext4_format::JournalReplayReport {
            transactions: 1,
            blocks_replayed: 1,
            next_sequence: 43,
        })
    );
    assert_eq!(image.block(9), &after);
    assert_eq!(Jbd2Superblock::parse(image.block(40)).unwrap().start, 0,);
}

#[test]
fn checksummed_journal_superblock_rejects_a_bad_checksum() {
    let mut page = geometry_with_superblock_page().superblock_page.unwrap();
    page[40..44].copy_from_slice(&0x0000_0008u32.to_be_bytes());
    page[0x50] = 4;
    page[0xFC..0x100].copy_from_slice(&0xDEAD_BEEFu32.to_be_bytes());

    assert_eq!(Jbd2Superblock::parse(&page), Err(Ext4FormatError::Corrupt));
}

#[test]
fn checksummed_journal_superblock_accepts_a_valid_checksum() {
    let mut page = geometry_with_superblock_page().superblock_page.unwrap();
    page[40..44].copy_from_slice(&0x0000_0008u32.to_be_bytes());
    page[0x50] = 4;
    page[0xFC..0x100].fill(0);
    let checksum = crc32c_append(
        crc32c_append(crc32c_append(0xFFFF_FFFF, &page[..0xFC]), &[0; 4]),
        &page[0x100..],
    );
    page[0xFC..0x100].copy_from_slice(&checksum.to_be_bytes());

    assert!(Jbd2Superblock::parse(&page).is_ok());
}

#[test]
fn replay_ignores_an_incomplete_transaction_tail() {
    let geometry = geometry();
    let mut image = MemImage::new(80);
    let before = [0x11; JBD2_BLOCK_SIZE];
    let after = [0x22; JBD2_BLOCK_SIZE];
    let record = Jbd2TransactionImage::encode_legacy(
        42,
        geometry.superblock.uuid,
        vec![Jbd2MetadataUpdate::new(9, after)],
    )
    .unwrap();

    *image.block_mut(9) = before;
    *image.block_mut(44) = record.descriptor;
    *image.block_mut(48) = record.metadata_blocks[0];

    let report = replay_journal(&mut image, &geometry).unwrap();

    assert_eq!(report.transactions, 0);
    assert_eq!(report.blocks_replayed, 0);
    assert_eq!(image.block(9), &before);
}

#[test]
fn replay_wraps_over_the_ring_tail() {
    let mut geometry = geometry();
    geometry.superblock.start = 6;
    let mut image = MemImage::new(80);
    let after = [0x33; JBD2_BLOCK_SIZE];
    let record = Jbd2TransactionImage::encode_legacy(
        42,
        geometry.superblock.uuid,
        vec![Jbd2MetadataUpdate::new(10, after)],
    )
    .unwrap();

    *image.block_mut(64) = record.descriptor;
    *image.block_mut(68) = record.metadata_blocks[0];
    *image.block_mut(44) = record.commit;

    let report = replay_journal(&mut image, &geometry).unwrap();

    assert_eq!(report.transactions, 1);
    assert_eq!(image.block(10), &after);
}

#[test]
fn replay_cleanup_publishes_a_clean_superblock_for_the_next_mount() {
    let geometry = geometry_with_superblock_page();
    let mut image = MemImage::new(80);
    let after = [0x4C; JBD2_BLOCK_SIZE];
    let record = Jbd2TransactionImage::encode_legacy(
        geometry.superblock.sequence,
        geometry.superblock.uuid,
        vec![Jbd2MetadataUpdate::new(9, after)],
    )
    .unwrap();
    *image.block_mut(44) = record.descriptor;
    *image.block_mut(48) = record.metadata_blocks[0];
    *image.block_mut(52) = record.commit;

    let replay = replay_journal(&mut image, &geometry).unwrap();
    clean_replayed_journal(&mut image, &geometry, replay.next_sequence).unwrap();

    let clean_page = *image.block(40);
    let clean_superblock = Jbd2Superblock::parse(&clean_page).unwrap();
    assert_eq!(clean_superblock.start, 0);
    assert_eq!(clean_superblock.sequence, replay.next_sequence);

    let clean_geometry = JournalGeometry {
        superblock: clean_superblock,
        blocks: geometry.blocks,
        superblock_page: Some(clean_page),
    };
    assert_eq!(
        replay_journal(&mut image, &clean_geometry)
            .unwrap()
            .transactions,
        0,
        "a subsequent mount must not rescan a transaction whose home blocks are durable"
    );
}
