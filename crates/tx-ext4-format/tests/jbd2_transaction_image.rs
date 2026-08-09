use tx_ext4_format::journal::{
    JBD2_BLOCK_SIZE, JBD2_MAGIC, Jbd2Commit, Jbd2Descriptor, Jbd2MetadataUpdate, Jbd2Revoke,
    Jbd2TransactionImage,
};

#[test]
fn transaction_image_encodes_descriptor_metadata_and_commit() {
    let mut escaped = [0u8; JBD2_BLOCK_SIZE];
    escaped[..4].copy_from_slice(&JBD2_MAGIC.to_be_bytes());
    escaped[4] = 0xA5;
    let ordinary = [0x5A; JBD2_BLOCK_SIZE];
    let image = Jbd2TransactionImage::encode_legacy(
        77,
        [0x44; 16],
        vec![
            Jbd2MetadataUpdate::new(9, escaped),
            Jbd2MetadataUpdate::new(13, ordinary),
        ],
    )
    .unwrap();

    let descriptor = Jbd2Descriptor::parse_legacy(&image.descriptor).unwrap();
    assert_eq!(descriptor.header.sequence, 77);
    assert_eq!(descriptor.tags.len(), 2);
    assert_eq!(descriptor.tags[0].target_block, 9);
    assert!(descriptor.tags[0].escaped);
    assert_eq!(descriptor.tags[0].uuid, Some([0x44; 16]));
    assert_eq!(descriptor.tags[1].target_block, 13);
    assert!(descriptor.tags[1].same_uuid);
    assert!(descriptor.tags[1].last);
    assert_eq!(image.metadata_blocks[0][..4], [0; 4]);
    assert_eq!(image.metadata_blocks[0][4], 0xA5);
    assert_eq!(image.metadata_blocks[1], ordinary);
    assert_eq!(
        Jbd2Commit::parse(&image.commit).unwrap().header.sequence,
        77
    );
}

#[test]
fn freeing_transaction_image_encodes_sorted_revokes_before_commit() {
    let image = Jbd2TransactionImage::encode_legacy_with_revokes(
        77,
        [0x44; 16],
        vec![Jbd2MetadataUpdate::new(9, [0x5A; JBD2_BLOCK_SIZE])],
        vec![33, 8, 33],
    )
    .unwrap();

    assert_eq!(
        Jbd2Revoke::parse(image.revokes.first().expect("revoke page"))
            .unwrap()
            .blocks,
        vec![8, 33]
    );
}

#[test]
fn freeing_transaction_image_splits_revokes_across_pages() {
    let revokes: Vec<u32> = (100..).take(Jbd2Revoke::MAX_BLOCKS_PER_PAGE + 1).collect();
    let image = Jbd2TransactionImage::encode_legacy_with_revokes(
        77,
        [0x44; 16],
        vec![Jbd2MetadataUpdate::new(9, [0x5A; JBD2_BLOCK_SIZE])],
        revokes.clone(),
    )
    .unwrap();

    assert_eq!(image.revokes.len(), 2);
    assert_eq!(
        Jbd2Revoke::parse(&image.revokes[0]).unwrap().blocks.len(),
        Jbd2Revoke::MAX_BLOCKS_PER_PAGE
    );
    assert_eq!(
        Jbd2Revoke::parse(&image.revokes[1]).unwrap().blocks,
        revokes[Jbd2Revoke::MAX_BLOCKS_PER_PAGE..]
    );
}
