use tx_ext4_format::Ext4FormatError;
use tx_ext4_format::journal::{
    JBD2_BLOCK_COMMIT, JBD2_BLOCK_DESCRIPTOR, JBD2_BLOCK_REVOKE, JBD2_MAGIC, Jbd2Commit,
    Jbd2Descriptor, Jbd2Header, Jbd2Revoke, Jbd2Superblock, Jbd2Tag,
};
use tx_ext4_format::ondisk::crc32c_append;

const BLOCK_SIZE: usize = 4096;

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
        u32::from_be_bytes(block[4..8].try_into().unwrap()),
        JBD2_BLOCK_DESCRIPTOR
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
    assert_eq!(
        u32::from_be_bytes(block[4..8].try_into().unwrap()),
        JBD2_BLOCK_REVOKE
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
fn superblock_state_update_preserves_unknown_bytes_and_recomputes_crc32c() {
    let mut bytes = [0u8; BLOCK_SIZE];
    bytes[..4].copy_from_slice(&JBD2_MAGIC.to_be_bytes());
    bytes[4..8].copy_from_slice(&4u32.to_be_bytes());
    bytes[8..12].copy_from_slice(&17u32.to_be_bytes());
    bytes[12..16].copy_from_slice(&(BLOCK_SIZE as u32).to_be_bytes());
    bytes[16..20].copy_from_slice(&128u32.to_be_bytes());
    bytes[20..24].copy_from_slice(&1u32.to_be_bytes());
    bytes[24..28].copy_from_slice(&18u32.to_be_bytes());
    bytes[28..32].copy_from_slice(&7u32.to_be_bytes());
    bytes[40..44].copy_from_slice(&0x0000_0008u32.to_be_bytes());
    bytes[48..64].copy_from_slice(&[0x5a; 16]);
    bytes[80] = 4;
    bytes[84..252].fill(0xC3);
    let initial_checksum = crc32c_append(0xFFFF_FFFF, &bytes);
    bytes[252..256].copy_from_slice(&initial_checksum.to_be_bytes());

    let superblock = Jbd2Superblock::parse(&bytes).unwrap();
    superblock.write_state(&mut bytes, 19, 0).unwrap();

    let updated = Jbd2Superblock::parse(&bytes).unwrap();
    assert_eq!(updated.sequence, 19);
    assert_eq!(updated.start, 0);
    assert_eq!(&bytes[84..252], &[0xC3; 168]);
    let recorded = u32::from_be_bytes(bytes[252..256].try_into().unwrap());
    bytes[252..256].fill(0);
    assert_eq!(recorded, crc32c_append(0xFFFF_FFFF, &bytes));
}
