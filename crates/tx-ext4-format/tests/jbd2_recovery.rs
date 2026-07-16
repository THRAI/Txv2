use tx_ext4_format::journal::{
    Jbd2MetadataUpdate, Jbd2Superblock, Jbd2TransactionImage, JBD2_BLOCK_SIZE,
    JBD2_BLOCK_SUPERBLOCK_V2,
};
use tx_ext4_format::pager::{BlockImage, JournalGeometry, Page4K};
use tx_ext4_format::{replay_journal, Ext4FormatError};

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
    }
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
