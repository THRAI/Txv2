use tx_ext4_format::journal::{
    Jbd2Commit, Jbd2Descriptor, Jbd2Features, Jbd2MetadataUpdate, Jbd2Revoke, Jbd2TransactionImage,
    JBD2_BLOCK_SIZE, JBD2_MAGIC,
};
use tx_ext4_format::ondisk::crc32c_append;

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
            .iter()
            .copied()
            .map(u64::from)
            .collect::<Vec<_>>()
    );
}

#[test]
fn transaction_image_carries_64bit_descriptor_and_revoke_layouts() {
    let features = Jbd2Features::REVOKE_64BIT;
    let home_block = 0x0102_0304_0506_0708;
    let revoked_block = 0x1112_1314_1516_1718;
    let image = Jbd2TransactionImage::encode_with_features_and_revokes(
        77,
        [0x44; 16],
        vec![Jbd2MetadataUpdate::new64(
            home_block,
            [0x5A; JBD2_BLOCK_SIZE],
        )],
        vec![revoked_block],
        features,
    )
    .unwrap();

    let descriptor = Jbd2Descriptor::parse_with_features(&image.descriptor, features).unwrap();
    assert_eq!(descriptor.tags[0].target_block, home_block);
    assert_eq!(
        Jbd2Revoke::parse_with_features(&image.revokes[0], features)
            .unwrap()
            .blocks,
        vec![revoked_block]
    );
}

#[test]
fn transaction_image_encodes_linux_checksum_v3_records() {
    let sequence = 0x3132_3334;
    let uuid = [0x44; 16];
    let features = Jbd2Features::REVOKE_64BIT_CSUM_V3;
    let mut escaped = [0x5A; JBD2_BLOCK_SIZE];
    escaped[..4].copy_from_slice(&JBD2_MAGIC.to_be_bytes());
    let image = Jbd2TransactionImage::encode_with_features_and_revokes(
        sequence,
        uuid,
        vec![Jbd2MetadataUpdate::new64(0x0102_0304_0506_0708, escaped)],
        vec![0x1112_1314_1516_1718],
        features,
    )
    .unwrap();

    let descriptor =
        Jbd2Descriptor::parse_with_features_and_uuid(&image.descriptor, features, uuid).unwrap();
    assert_eq!(descriptor.tags[0].checksum, {
        let seed = crc32c_append(u32::MAX, &uuid);
        let seed = crc32c_append(seed, &sequence.to_be_bytes());
        crc32c_append(seed, &image.metadata_blocks[0])
    });
    assert_eq!(&image.metadata_blocks[0][..4], &[0; 4]);
    assert_eq!(
        Jbd2Revoke::parse_with_features_and_uuid(&image.revokes[0], features, uuid)
            .unwrap()
            .blocks,
        vec![0x1112_1314_1516_1718]
    );
    assert_eq!(
        Jbd2Commit::parse_with_features(&image.commit, features, uuid)
            .unwrap()
            .header
            .sequence,
        sequence
    );
}
