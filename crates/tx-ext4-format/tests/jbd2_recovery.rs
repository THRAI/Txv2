use tx_ext4_format::journal::{
    Jbd2Commit, Jbd2Features, Jbd2Header, Jbd2MetadataUpdate, Jbd2Revoke, Jbd2Superblock,
    Jbd2TransactionImage, JBD2_BLOCK_SIZE, JBD2_BLOCK_SUPERBLOCK_V2, JBD2_MAGIC,
};
use tx_ext4_format::ondisk::Superblock;
use tx_ext4_format::pager::{BlockImage, JournalGeometry, Page4K};
use tx_ext4_format::{
    clean_replayed_journal, recover_if_required, replay_journal, Ext4FormatError, RecoveryReport,
};

#[derive(Clone)]
struct MemImage {
    blocks: Vec<Page4K>,
    writes: Vec<u64>,
    barriers: usize,
}

impl MemImage {
    fn new(blocks: usize) -> Self {
        Self {
            blocks: vec![[0; JBD2_BLOCK_SIZE]; blocks],
            writes: Vec::new(),
            barriers: 0,
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
        self.writes.push(block);
        let target = self
            .blocks
            .get_mut(block as usize)
            .ok_or(Ext4FormatError::OutOfBounds)?;
        target.copy_from_slice(data);
        Ok(())
    }

    fn barrier(&mut self) -> tx_ext4_format::Result<()> {
        self.barriers += 1;
        Ok(())
    }
}

fn commit_page(sequence: u32) -> Page4K {
    let mut page = [0; JBD2_BLOCK_SIZE];
    Jbd2Commit {
        header: Jbd2Header::commit(sequence),
        checksum_type: 0,
        checksum_size: 0,
        checksums: [0; 8],
        seconds: 0,
        nanoseconds: 0,
    }
    .encode(&mut page)
    .unwrap();
    page
}

fn revoke_page(sequence: u32, blocks: Vec<u64>, features: Jbd2Features) -> Page4K {
    let mut page = [0; JBD2_BLOCK_SIZE];
    Jbd2Revoke {
        header: Jbd2Header::revoke(sequence),
        blocks,
    }
    .encode_with_features(&mut page, features)
    .unwrap();
    page
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
        features: Jbd2Features::REVOKE,
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
    page[40..44].copy_from_slice(&geometry.features.feature_incompat().to_be_bytes());
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
fn replay_does_not_apply_an_older_image_revoked_by_a_later_committed_transaction() {
    let geometry = geometry();
    let mut image = MemImage::new(80);
    let before = [0x11; JBD2_BLOCK_SIZE];
    let stale_after_image = [0x5A; JBD2_BLOCK_SIZE];
    let first = Jbd2TransactionImage::encode_legacy(
        42,
        geometry.superblock.uuid,
        vec![Jbd2MetadataUpdate::new(9, stale_after_image)],
    )
    .unwrap();

    *image.block_mut(9) = before;
    *image.block_mut(44) = first.descriptor;
    *image.block_mut(48) = first.metadata_blocks[0];
    *image.block_mut(52) = first.commit;
    *image.block_mut(56) = revoke_page(43, vec![9], geometry.features);
    *image.block_mut(60) = commit_page(43);

    let report = replay_journal(&mut image, &geometry).unwrap();

    assert_eq!(report.transactions, 2);
    assert_eq!(report.blocks_replayed, 0);
    assert_eq!(image.block(9), &before);
    assert!(image.writes.is_empty());
}

#[test]
fn replay_applies_every_descriptor_in_one_committed_transaction() {
    let geometry = geometry();
    let mut image = MemImage::new(80);
    let first_after = [0x31; JBD2_BLOCK_SIZE];
    let second_after = [0x32; JBD2_BLOCK_SIZE];
    let first = Jbd2TransactionImage::encode_legacy(
        42,
        geometry.superblock.uuid,
        vec![Jbd2MetadataUpdate::new(9, first_after)],
    )
    .unwrap();
    let second = Jbd2TransactionImage::encode_legacy(
        42,
        geometry.superblock.uuid,
        vec![Jbd2MetadataUpdate::new(10, second_after)],
    )
    .unwrap();

    *image.block_mut(44) = first.descriptor;
    *image.block_mut(48) = first.metadata_blocks[0];
    *image.block_mut(52) = second.descriptor;
    *image.block_mut(56) = second.metadata_blocks[0];
    *image.block_mut(60) = second.commit;

    let report = replay_journal(&mut image, &geometry).unwrap();

    assert_eq!(report.transactions, 1);
    assert_eq!(report.blocks_replayed, 2);
    assert_eq!(image.block(9), &first_after);
    assert_eq!(image.block(10), &second_after);
}

#[test]
fn replay_preserves_64bit_descriptor_and_revoke_block_numbers() {
    let mut geometry = geometry();
    geometry.features = Jbd2Features::REVOKE_64BIT;
    let mut image = MemImage::new(80);
    let high_home = u64::from(u32::MAX) + 9;
    let record = Jbd2TransactionImage::encode_with_features_and_revokes(
        42,
        geometry.superblock.uuid,
        vec![Jbd2MetadataUpdate::new64(
            high_home,
            [0x5A; JBD2_BLOCK_SIZE],
        )],
        vec![high_home],
        geometry.features,
    )
    .unwrap();

    *image.block_mut(44) = record.descriptor;
    *image.block_mut(48) = record.metadata_blocks[0];
    *image.block_mut(52) = record.revokes[0];
    *image.block_mut(56) = record.commit;

    let report = replay_journal(&mut image, &geometry).unwrap();

    assert_eq!(report.transactions, 1);
    assert_eq!(report.blocks_replayed, 0);
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
fn checksummed_journal_superblock_rejects_csum_v2() {
    let mut page = geometry_with_superblock_page().superblock_page.unwrap();
    page[40..44].copy_from_slice(&0x0000_0008u32.to_be_bytes());
    page[0x50] = 4;
    page[0xFC..0x100].copy_from_slice(&0xDEAD_BEEFu32.to_be_bytes());

    assert_eq!(
        Jbd2Superblock::parse(&page),
        Err(Ext4FormatError::Unsupported)
    );
}

#[test]
fn checksummed_journal_superblock_rejects_csum_v3() {
    let mut page = geometry_with_superblock_page().superblock_page.unwrap();
    page[40..44].copy_from_slice(&0x0000_0010u32.to_be_bytes());

    assert_eq!(
        Jbd2Superblock::parse(&page),
        Err(Ext4FormatError::Unsupported)
    );
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
    assert_eq!(report.next_sequence, 43);
    assert_eq!(image.block(9), &before);
}

#[test]
fn recovery_discards_an_incomplete_transaction_and_cleans_the_journal() {
    let geometry = geometry_with_superblock_page();
    let mut image = MemImage::new(80);
    *image.block_mut(40) = geometry.superblock_page.unwrap();
    let first = Jbd2TransactionImage::encode_legacy(
        42,
        geometry.superblock.uuid,
        vec![Jbd2MetadataUpdate::new(9, [0x41; JBD2_BLOCK_SIZE])],
    )
    .unwrap();
    let second = Jbd2TransactionImage::encode_legacy(
        42,
        geometry.superblock.uuid,
        vec![Jbd2MetadataUpdate::new(10, [0x42; JBD2_BLOCK_SIZE])],
    )
    .unwrap();
    *image.block_mut(44) = first.descriptor;
    *image.block_mut(48) = first.metadata_blocks[0];
    *image.block_mut(52) = second.descriptor;

    let mut superblock = Superblock::default();
    superblock.feature_incompat |= Superblock::FEATURE_INCOMPAT_RECOVER;
    let report = recover_if_required(&mut image, &superblock, &geometry).unwrap();

    assert_eq!(
        report,
        RecoveryReport::Replayed(tx_ext4_format::JournalReplayReport {
            transactions: 0,
            blocks_replayed: 0,
            next_sequence: 43,
        })
    );
    assert_eq!(image.writes, vec![40]);
    assert_eq!(image.barriers, 1);
    assert_eq!(Jbd2Superblock::parse(image.block(40)).unwrap().start, 0);
}

#[test]
fn recovery_replays_a_committed_prefix_and_skips_the_incomplete_tail_sequence() {
    let geometry = geometry_with_superblock_page();
    let mut image = MemImage::new(80);
    *image.block_mut(40) = geometry.superblock_page.unwrap();
    let committed_after = [0x51; JBD2_BLOCK_SIZE];
    let incomplete_before = [0x12; JBD2_BLOCK_SIZE];
    let committed = Jbd2TransactionImage::encode_legacy(
        42,
        geometry.superblock.uuid,
        vec![Jbd2MetadataUpdate::new(9, committed_after)],
    )
    .unwrap();
    let incomplete = Jbd2TransactionImage::encode_legacy(
        43,
        geometry.superblock.uuid,
        vec![Jbd2MetadataUpdate::new(10, [0x52; JBD2_BLOCK_SIZE])],
    )
    .unwrap();
    *image.block_mut(10) = incomplete_before;
    *image.block_mut(44) = committed.descriptor;
    *image.block_mut(48) = committed.metadata_blocks[0];
    *image.block_mut(52) = committed.commit;
    *image.block_mut(56) = incomplete.descriptor;
    *image.block_mut(60) = incomplete.metadata_blocks[0];

    let mut superblock = Superblock::default();
    superblock.feature_incompat |= Superblock::FEATURE_INCOMPAT_RECOVER;
    let report = recover_if_required(&mut image, &superblock, &geometry).unwrap();

    assert_eq!(
        report,
        RecoveryReport::Replayed(tx_ext4_format::JournalReplayReport {
            transactions: 1,
            blocks_replayed: 1,
            next_sequence: 44,
        })
    );
    assert_eq!(image.block(9), &committed_after);
    assert_eq!(image.block(10), &incomplete_before);
    let clean_superblock = Jbd2Superblock::parse(image.block(40)).unwrap();
    assert_eq!(clean_superblock.sequence, 44);
    assert_eq!(clean_superblock.start, 0);
}

#[test]
fn recovery_rejects_an_unsupported_second_descriptor_before_writing_or_cleaning() {
    let geometry = geometry_with_superblock_page();
    let mut image = MemImage::new(80);
    *image.block_mut(40) = geometry.superblock_page.unwrap();
    let first = Jbd2TransactionImage::encode_legacy(
        42,
        geometry.superblock.uuid,
        vec![Jbd2MetadataUpdate::new(9, [0x51; JBD2_BLOCK_SIZE])],
    )
    .unwrap();
    let second = Jbd2TransactionImage::encode_legacy(
        42,
        geometry.superblock.uuid,
        vec![Jbd2MetadataUpdate::new(10, [0x52; JBD2_BLOCK_SIZE])],
    )
    .unwrap();
    *image.block_mut(44) = first.descriptor;
    *image.block_mut(48) = first.metadata_blocks[0];
    *image.block_mut(52) = second.descriptor;
    image.block_mut(52)[18..20].copy_from_slice(&0x0010u16.to_be_bytes());
    *image.block_mut(56) = second.metadata_blocks[0];
    *image.block_mut(60) = second.commit;

    let mut superblock = Superblock::default();
    superblock.feature_incompat |= Superblock::FEATURE_INCOMPAT_RECOVER;
    assert_eq!(
        recover_if_required(&mut image, &superblock, &geometry),
        Err(Ext4FormatError::Unsupported)
    );
    assert!(image.writes.is_empty());
    assert_eq!(image.barriers, 0);
    assert_eq!(Jbd2Superblock::parse(image.block(40)).unwrap().start, 1);
}

#[test]
fn recovery_rejects_a_corrupt_second_descriptor_before_writing_or_cleaning() {
    let geometry = geometry_with_superblock_page();
    let mut image = MemImage::new(80);
    *image.block_mut(40) = geometry.superblock_page.unwrap();
    let first = Jbd2TransactionImage::encode_legacy(
        42,
        geometry.superblock.uuid,
        vec![Jbd2MetadataUpdate::new(9, [0x61; JBD2_BLOCK_SIZE])],
    )
    .unwrap();
    let second = Jbd2TransactionImage::encode_legacy(
        42,
        geometry.superblock.uuid,
        vec![Jbd2MetadataUpdate::new(10, [0x62; JBD2_BLOCK_SIZE])],
    )
    .unwrap();
    *image.block_mut(44) = first.descriptor;
    *image.block_mut(48) = first.metadata_blocks[0];
    *image.block_mut(52) = second.descriptor;
    image.block_mut(52)[18..20].fill(0);
    *image.block_mut(56) = second.metadata_blocks[0];
    *image.block_mut(60) = second.commit;

    let mut superblock = Superblock::default();
    superblock.feature_incompat |= Superblock::FEATURE_INCOMPAT_RECOVER;
    assert_eq!(
        recover_if_required(&mut image, &superblock, &geometry),
        Err(Ext4FormatError::Corrupt)
    );
    assert!(image.writes.is_empty());
    assert_eq!(image.barriers, 0);
    assert_eq!(Jbd2Superblock::parse(image.block(40)).unwrap().start, 1);
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
        features: geometry.features,
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
