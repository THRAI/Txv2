use tx_ext4_format::journal::{
    Jbd2Commit, Jbd2Descriptor, Jbd2Features, Jbd2Header, Jbd2Revoke, Jbd2Superblock, Jbd2Tag,
    JBD2_BLOCK_COMMIT, JBD2_BLOCK_DESCRIPTOR, JBD2_BLOCK_REVOKE, JBD2_BLOCK_SUPERBLOCK_V1,
    JBD2_FEATURE_INCOMPAT_64BIT, JBD2_FEATURE_INCOMPAT_ASYNC_COMMIT, JBD2_FEATURE_INCOMPAT_CSUM_V2,
    JBD2_FEATURE_INCOMPAT_CSUM_V3, JBD2_FEATURE_INCOMPAT_FAST_COMMIT, JBD2_FEATURE_INCOMPAT_REVOKE,
    JBD2_MAGIC,
};
use tx_ext4_format::ondisk::crc32c_append;
use tx_ext4_format::Ext4FormatError;

const BLOCK_SIZE: usize = 4096;

fn journal_seed(uuid: &[u8; 16]) -> u32 {
    crc32c_append(u32::MAX, uuid)
}

fn journal_block_checksum(uuid: &[u8; 16], block: &[u8]) -> u32 {
    crc32c_append(journal_seed(uuid), block)
}

fn install_superblock_v3_checksum(block: &mut [u8; BLOCK_SIZE]) {
    block[0xFC..0x100].fill(0);
    let checksum = crc32c_append(u32::MAX, &block[..1024]);
    block[0xFC..0x100].copy_from_slice(&checksum.to_be_bytes());
}

#[test]
fn descriptor_round_trip_preserves_multiple_legacy_tags() {
    let first_uuid = [0x11; 16];
    let mut final_tag = Jbd2Tag::new(9, 0x5678, None, true);
    final_tag.last = true;
    let descriptor = Jbd2Descriptor {
        header: Jbd2Header::descriptor(41),
        tags: vec![Jbd2Tag::new(7, 0x1234, Some(first_uuid), false), final_tag],
    };
    let mut block = [0u8; BLOCK_SIZE];

    descriptor.encode_legacy(&mut block).unwrap();

    assert_eq!(Jbd2Descriptor::parse_legacy(&block).unwrap(), descriptor);
    assert_eq!(
        &block[12..36],
        &[
            0x00, 0x00, 0x00, 0x07, 0x12, 0x34, 0x00, 0x00, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11,
            0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11,
        ]
    );
    assert_eq!(
        &block[36..44],
        &[0x00, 0x00, 0x00, 0x09, 0x56, 0x78, 0x00, 0x0A]
    );
    assert_eq!(block[44], 0, "legacy SAME_UUID tags remain exactly 8 bytes");
    assert_eq!(
        u32::from_be_bytes(block[4..8].try_into().unwrap()),
        JBD2_BLOCK_DESCRIPTOR
    );
}

#[test]
fn descriptor_64bit_tag_golden_layout_has_high_block_word() {
    let mut tag = Jbd2Tag::new64(0x0102_0304_0506_0708, 0x0910, None, true);
    tag.last = true;
    let descriptor = Jbd2Descriptor {
        header: Jbd2Header::descriptor(0x1112_1314),
        tags: vec![tag],
    };
    let mut block = [0u8; BLOCK_SIZE];

    descriptor
        .encode_with_features(&mut block, Jbd2Features::BLOCK_64BIT)
        .unwrap();

    assert_eq!(
        &block[12..24],
        &[0x05, 0x06, 0x07, 0x08, 0x09, 0x10, 0x00, 0x0A, 0x01, 0x02, 0x03, 0x04,]
    );
    assert_eq!(
        Jbd2Descriptor::parse_with_features(&block, Jbd2Features::BLOCK_64BIT).unwrap(),
        descriptor
    );
    assert_eq!(
        descriptor.encode_legacy(&mut block),
        Err(Ext4FormatError::OutOfBounds)
    );
}

#[test]
fn descriptor_v3_tag_has_fixed_16_byte_layout_and_crc32c_tail() {
    let uuid = [0xA5; 16];
    let features = Jbd2Features::REVOKE_64BIT_CSUM_V3;
    let mut tag = Jbd2Tag::new64(0x0102_0304_0506_0708, 0x1112_1314, Some(uuid), false);
    tag.last = true;
    let descriptor = Jbd2Descriptor {
        header: Jbd2Header::descriptor(0x2122_2324),
        tags: vec![tag],
    };
    let mut block = [0u8; BLOCK_SIZE];

    descriptor
        .encode_with_features_and_uuid(&mut block, features, uuid)
        .unwrap();

    assert_eq!(
        &block[12..28],
        &[
            0x05, 0x06, 0x07, 0x08, 0x00, 0x00, 0x00, 0x08, 0x01, 0x02, 0x03, 0x04, 0x11, 0x12,
            0x13, 0x14,
        ]
    );
    assert_eq!(&block[28..44], &uuid);
    let stored = u32::from_be_bytes(block[BLOCK_SIZE - 4..].try_into().unwrap());
    let mut zeroed = block;
    zeroed[BLOCK_SIZE - 4..].fill(0);
    assert_eq!(stored, journal_block_checksum(&uuid, &zeroed));
    assert_eq!(
        Jbd2Descriptor::parse_with_features_and_uuid(&block, features, uuid).unwrap(),
        descriptor
    );

    block[40] ^= 1;
    assert_eq!(
        Jbd2Descriptor::parse_with_features_and_uuid(&block, features, uuid),
        Err(Ext4FormatError::Corrupt)
    );
}

#[test]
fn descriptor_rejects_missing_last_tag() {
    let mut block = [0u8; 36];
    Jbd2Header::descriptor(2).encode(&mut block[..12]).unwrap();
    Jbd2Tag::new(1, 0, Some([0; 16]), false)
        .encode_legacy(&mut block[12..])
        .unwrap();

    assert_eq!(
        Jbd2Descriptor::parse_legacy(&block),
        Err(Ext4FormatError::Corrupt)
    );
}

#[test]
fn commit_round_trip_uses_full_jbd2_commit_layout() {
    let commit = Jbd2Commit {
        header: Jbd2Header::commit(88),
        checksum_type: 4,
        checksum_size: 4,
        checksums: [1, 2, 3, 4, 5, 6, 7, 8],
        seconds: 0x0102_0304_0506_0708,
        nanoseconds: 99,
    };
    let mut block = [0u8; Jbd2Commit::ENCODED_LEN];

    commit.encode(&mut block).unwrap();

    assert_eq!(Jbd2Commit::parse(&block).unwrap(), commit);
    assert_eq!(
        u32::from_be_bytes(block[4..8].try_into().unwrap()),
        JBD2_BLOCK_COMMIT
    );
}

#[test]
fn revoke_round_trip_tracks_each_revoked_home_block() {
    let revoke = Jbd2Revoke {
        header: Jbd2Header::revoke(123),
        blocks: vec![3, 8, 13],
    };
    let mut block = [0u8; 64];

    revoke.encode(&mut block).unwrap();

    assert_eq!(Jbd2Revoke::parse(&block).unwrap(), revoke);
    assert_eq!(&block[16..28], &[0, 0, 0, 3, 0, 0, 0, 8, 0, 0, 0, 13]);
    assert_eq!(
        u32::from_be_bytes(block[4..8].try_into().unwrap()),
        JBD2_BLOCK_REVOKE
    );
}

#[test]
fn revoke_64bit_entry_golden_layout_uses_eight_bytes() {
    let revoke = Jbd2Revoke {
        header: Jbd2Header::revoke(123),
        blocks: vec![0x0102_0304_0506_0708, 0x1112_1314_1516_1718],
    };
    let mut block = [0u8; 64];
    let features = Jbd2Features::REVOKE_64BIT;

    revoke.encode_with_features(&mut block, features).unwrap();

    assert_eq!(&block[12..16], &32u32.to_be_bytes());
    assert_eq!(
        &block[16..32],
        &[
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16,
            0x17, 0x18,
        ]
    );
    assert_eq!(
        Jbd2Revoke::parse_with_features(&block, features).unwrap(),
        revoke
    );
    assert_eq!(revoke.encode(&mut block), Err(Ext4FormatError::OutOfBounds));
}

#[test]
fn revoke_v3_reserves_and_verifies_checksum_tail() {
    let uuid = [0x4D; 16];
    let features = Jbd2Features::REVOKE_64BIT_CSUM_V3;
    let revoke = Jbd2Revoke {
        header: Jbd2Header::revoke(123),
        blocks: vec![0x0102_0304_0506_0708, 0x1112_1314_1516_1718],
    };
    let mut block = [0u8; BLOCK_SIZE];

    revoke
        .encode_with_features_and_uuid(&mut block, features, uuid)
        .unwrap();

    assert_eq!(&block[12..16], &32u32.to_be_bytes());
    let stored = u32::from_be_bytes(block[BLOCK_SIZE - 4..].try_into().unwrap());
    let mut zeroed = block;
    zeroed[BLOCK_SIZE - 4..].fill(0);
    assert_eq!(stored, journal_block_checksum(&uuid, &zeroed));
    assert_eq!(
        Jbd2Revoke::parse_with_features_and_uuid(&block, features, uuid).unwrap(),
        revoke
    );
    assert_eq!(Jbd2Revoke::max_blocks_per_page(features), 509);

    block[20] ^= 1;
    assert_eq!(
        Jbd2Revoke::parse_with_features_and_uuid(&block, features, uuid),
        Err(Ext4FormatError::Corrupt)
    );
}

#[test]
fn commit_v3_uses_uuid_seeded_full_block_checksum() {
    let uuid = [0x71; 16];
    let features = Jbd2Features::CSUM_V3;
    let commit = Jbd2Commit {
        header: Jbd2Header::commit(88),
        checksum_type: 7,
        checksum_size: 9,
        checksums: [0xDEAD_BEEF; 8],
        seconds: 0x0102_0304_0506_0708,
        nanoseconds: 99,
    };
    let mut block = [0u8; BLOCK_SIZE];

    commit
        .encode_with_features(&mut block, features, uuid)
        .unwrap();

    assert_eq!(block[12], 0);
    assert_eq!(block[13], 0);
    assert!(block[20..48].iter().all(|byte| *byte == 0));
    let stored = u32::from_be_bytes(block[16..20].try_into().unwrap());
    let mut zeroed = block;
    zeroed[16..20].fill(0);
    assert_eq!(stored, journal_block_checksum(&uuid, &zeroed));
    assert_eq!(
        Jbd2Commit::parse_with_features(&block, features, uuid)
            .unwrap()
            .header,
        commit.header
    );

    block[100] ^= 1;
    assert_eq!(
        Jbd2Commit::parse_with_features(&block, features, uuid),
        Err(Ext4FormatError::Corrupt)
    );
}

#[test]
fn header_rejects_wrong_magic() {
    let mut bytes = [0u8; Jbd2Header::ENCODED_LEN];
    bytes[..4].copy_from_slice(&(JBD2_MAGIC ^ 1).to_be_bytes());
    bytes[4..8].copy_from_slice(&JBD2_BLOCK_COMMIT.to_be_bytes());

    assert_eq!(Jbd2Header::parse(&bytes), Err(Ext4FormatError::BadMagic));
}

#[test]
fn superblock_parses_journal_ring_geometry_and_rejects_invalid_bounds() {
    let mut bytes = [0u8; BLOCK_SIZE];
    bytes[..4].copy_from_slice(&JBD2_MAGIC.to_be_bytes());
    bytes[4..8].copy_from_slice(&4u32.to_be_bytes());
    bytes[8..12].copy_from_slice(&17u32.to_be_bytes());
    bytes[12..16].copy_from_slice(&(BLOCK_SIZE as u32).to_be_bytes());
    bytes[16..20].copy_from_slice(&128u32.to_be_bytes());
    bytes[20..24].copy_from_slice(&1u32.to_be_bytes());
    bytes[24..28].copy_from_slice(&18u32.to_be_bytes());
    bytes[28..32].copy_from_slice(&7u32.to_be_bytes());
    bytes[48..64].copy_from_slice(&[0x5a; 16]);

    assert_eq!(
        Jbd2Superblock::parse(&bytes).unwrap(),
        Jbd2Superblock {
            block_type: 4,
            block_size: BLOCK_SIZE as u32,
            max_len: 128,
            first: 1,
            sequence: 18,
            start: 7,
            uuid: [0x5a; 16],
        }
    );

    bytes[20..24].copy_from_slice(&128u32.to_be_bytes());
    assert_eq!(Jbd2Superblock::parse(&bytes), Err(Ext4FormatError::Corrupt));
}

#[test]
fn superblock_strictly_accepts_only_revoke_and_64bit_features() {
    let mut bytes = [0u8; BLOCK_SIZE];
    bytes[..4].copy_from_slice(&JBD2_MAGIC.to_be_bytes());
    bytes[4..8].copy_from_slice(&4u32.to_be_bytes());
    bytes[8..12].copy_from_slice(&17u32.to_be_bytes());
    bytes[12..16].copy_from_slice(&(BLOCK_SIZE as u32).to_be_bytes());
    bytes[16..20].copy_from_slice(&128u32.to_be_bytes());
    bytes[20..24].copy_from_slice(&1u32.to_be_bytes());
    bytes[24..28].copy_from_slice(&18u32.to_be_bytes());
    bytes[28..32].copy_from_slice(&7u32.to_be_bytes());
    bytes[40..44].copy_from_slice(
        &(JBD2_FEATURE_INCOMPAT_REVOKE | JBD2_FEATURE_INCOMPAT_64BIT).to_be_bytes(),
    );
    bytes[48..64].copy_from_slice(&[0x5a; 16]);
    bytes[84..252].fill(0xC3);
    bytes[1024..].fill(0xA5);

    let (superblock, features) = Jbd2Superblock::parse_with_features(&bytes).unwrap();
    assert_eq!(features, Jbd2Features::REVOKE_64BIT);
    superblock.write_state(&mut bytes, 19, 0).unwrap();

    let updated = Jbd2Superblock::parse(&bytes).unwrap();
    assert_eq!(updated.sequence, 19);
    assert_eq!(updated.start, 0);
    assert_eq!(&bytes[84..252], &[0xC3; 168]);
    assert_eq!(&bytes[1024..], &[0xA5; BLOCK_SIZE - 1024]);

    for unsupported in [
        JBD2_FEATURE_INCOMPAT_ASYNC_COMMIT,
        JBD2_FEATURE_INCOMPAT_CSUM_V2,
        JBD2_FEATURE_INCOMPAT_FAST_COMMIT,
        0x8000_0000,
    ] {
        bytes[40..44].copy_from_slice(&unsupported.to_be_bytes());
        assert_eq!(
            Jbd2Superblock::parse(&bytes),
            Err(Ext4FormatError::Unsupported)
        );
    }

    bytes[40..44].fill(0);
    bytes[36..40].copy_from_slice(&1u32.to_be_bytes());
    assert_eq!(
        Jbd2Superblock::parse(&bytes),
        Err(Ext4FormatError::Unsupported)
    );
    bytes[36..40].fill(0);
    bytes[44..48].copy_from_slice(&1u32.to_be_bytes());
    assert_eq!(
        Jbd2Superblock::parse(&bytes),
        Err(Ext4FormatError::Unsupported)
    );
}

#[test]
fn superblock_v3_validates_type_crc_and_recomputes_crc_on_state_update() {
    let uuid = [0x5A; 16];
    let mut bytes = [0u8; BLOCK_SIZE];
    bytes[..4].copy_from_slice(&JBD2_MAGIC.to_be_bytes());
    bytes[4..8].copy_from_slice(&4u32.to_be_bytes());
    bytes[8..12].copy_from_slice(&17u32.to_be_bytes());
    bytes[12..16].copy_from_slice(&(BLOCK_SIZE as u32).to_be_bytes());
    bytes[16..20].copy_from_slice(&128u32.to_be_bytes());
    bytes[20..24].copy_from_slice(&1u32.to_be_bytes());
    bytes[24..28].copy_from_slice(&18u32.to_be_bytes());
    bytes[28..32].copy_from_slice(&7u32.to_be_bytes());
    bytes[40..44].copy_from_slice(
        &(JBD2_FEATURE_INCOMPAT_REVOKE
            | JBD2_FEATURE_INCOMPAT_64BIT
            | JBD2_FEATURE_INCOMPAT_CSUM_V3)
            .to_be_bytes(),
    );
    bytes[48..64].copy_from_slice(&uuid);
    bytes[0x50] = 4;
    bytes[0x100..].fill(0xC3);
    install_superblock_v3_checksum(&mut bytes);

    let (superblock, features) = Jbd2Superblock::parse_with_features(&bytes).unwrap();
    assert_eq!(features, Jbd2Features::REVOKE_64BIT_CSUM_V3);
    let before_tail = bytes[0x100..].to_vec();
    let old_checksum = bytes[0xFC..0x100].to_vec();

    superblock.write_state(&mut bytes, 19, 0).unwrap();

    assert_ne!(&bytes[0xFC..0x100], old_checksum.as_slice());
    assert_eq!(&bytes[0x100..], before_tail.as_slice());
    let stored = u32::from_be_bytes(bytes[0xFC..0x100].try_into().unwrap());
    let mut zeroed = bytes;
    zeroed[0xFC..0x100].fill(0);
    assert_eq!(stored, crc32c_append(u32::MAX, &zeroed[..1024]));
    assert_eq!(Jbd2Superblock::parse(&bytes).unwrap().start, 0);

    let mut mixed_v2_v3 = bytes;
    mixed_v2_v3[40..44].copy_from_slice(
        &(JBD2_FEATURE_INCOMPAT_REVOKE
            | JBD2_FEATURE_INCOMPAT_64BIT
            | JBD2_FEATURE_INCOMPAT_CSUM_V2
            | JBD2_FEATURE_INCOMPAT_CSUM_V3)
            .to_be_bytes(),
    );
    install_superblock_v3_checksum(&mut mixed_v2_v3);
    assert_eq!(
        Jbd2Superblock::parse(&mixed_v2_v3),
        Err(Ext4FormatError::Unsupported)
    );

    let mut wrong_type = bytes;
    wrong_type[0x50] = 1;
    install_superblock_v3_checksum(&mut wrong_type);
    assert_eq!(
        Jbd2Superblock::parse(&wrong_type),
        Err(Ext4FormatError::Unsupported)
    );

    let mut corrupt = bytes;
    corrupt[100] ^= 1;
    assert_eq!(
        Jbd2Superblock::parse(&corrupt),
        Err(Ext4FormatError::Corrupt)
    );
    assert_eq!(
        Jbd2Superblock::parse(&bytes[..1023]),
        Err(Ext4FormatError::Truncated)
    );
}

#[test]
fn v1_superblock_rejects_v2_feature_and_uuid_bytes_in_its_reserved_tail() {
    let mut bytes = [0u8; BLOCK_SIZE];
    bytes[..4].copy_from_slice(&JBD2_MAGIC.to_be_bytes());
    bytes[4..8].copy_from_slice(&JBD2_BLOCK_SUPERBLOCK_V1.to_be_bytes());
    bytes[8..12].copy_from_slice(&17u32.to_be_bytes());
    bytes[12..16].copy_from_slice(&(BLOCK_SIZE as u32).to_be_bytes());
    bytes[16..20].copy_from_slice(&128u32.to_be_bytes());
    bytes[20..24].copy_from_slice(&1u32.to_be_bytes());
    bytes[24..28].copy_from_slice(&18u32.to_be_bytes());
    bytes[28..32].copy_from_slice(&7u32.to_be_bytes());

    bytes[40..44].copy_from_slice(
        &(JBD2_FEATURE_INCOMPAT_REVOKE | JBD2_FEATURE_INCOMPAT_64BIT).to_be_bytes(),
    );
    assert_eq!(
        Jbd2Superblock::parse_with_features(&bytes),
        Err(Ext4FormatError::Unsupported)
    );

    bytes[40..44].fill(0);
    bytes[48..64].copy_from_slice(&[0x5a; 16]);
    assert_eq!(
        Jbd2Superblock::parse_with_features(&bytes),
        Err(Ext4FormatError::Unsupported)
    );

    bytes[48..64].fill(0);
    let (superblock, features) = Jbd2Superblock::parse_with_features(&bytes).unwrap();
    assert_eq!(features, Jbd2Features::NONE);
    assert_eq!(superblock.uuid, [0; 16]);
}
