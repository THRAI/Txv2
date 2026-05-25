use tx_ext4_format::ondisk::{
    crc32c, crc32c_append, encode_journal_commit, encode_journal_descriptor, metadata_csum32,
    parse_journal_descriptor, BitmapMut, BitmapView, CommitHeader, DirEntryIter, DxCountLimit,
    DxEntry, DxEntryIter, DxRootInfo, Ext4FormatError, Extent, ExtentHeader, ExtentIdx, ExtentNode,
    GroupDesc, Inode, JournalBlockTag, JournalHeader, Superblock, JBD2_BLOCK_COMMIT,
    JBD2_BLOCK_DESCRIPTOR, JBD2_MAGIC,
};
use tx_ext4_format::pager::{
    BlockImage, DirEntryLite, Ext4Pager, InodeMetaLite, InodeNo, MetadataUpdate, PageRead,
    WritebackReceipt, BLOCK_SIZE,
};
use tx_ext4_format::xattr::{
    encode_external_xattr_block, encode_inline_xattrs, xattr_entry_hash, xattr_inode_hash,
    InlineXattr,
};

#[derive(Clone)]
struct MemImage {
    blocks: Vec<[u8; BLOCK_SIZE]>,
    barriers: usize,
}

impl MemImage {
    fn new(blocks: usize) -> Self {
        Self {
            blocks: vec![[0; BLOCK_SIZE]; blocks],
            barriers: 0,
        }
    }

    fn block(&self, block: u64) -> &[u8; BLOCK_SIZE] {
        &self.blocks[block as usize]
    }

    fn block_mut(&mut self, block: u64) -> &mut [u8; BLOCK_SIZE] {
        &mut self.blocks[block as usize]
    }
}

impl BlockImage for MemImage {
    fn total_blocks(&self) -> u64 {
        self.blocks.len() as u64
    }

    fn read_block(&self, block: u64, out: &mut [u8; BLOCK_SIZE]) -> tx_ext4_format::Result<()> {
        let src = self
            .blocks
            .get(block as usize)
            .ok_or(Ext4FormatError::OutOfBounds)?;
        out.copy_from_slice(src);
        Ok(())
    }

    fn write_block(&mut self, block: u64, data: &[u8; BLOCK_SIZE]) -> tx_ext4_format::Result<()> {
        let dst = self
            .blocks
            .get_mut(block as usize)
            .ok_or(Ext4FormatError::OutOfBounds)?;
        dst.copy_from_slice(data);
        Ok(())
    }

    fn barrier(&mut self) -> tx_ext4_format::Result<()> {
        self.barriers += 1;
        Ok(())
    }
}

#[test]
fn ondisk_structs_roundtrip_and_iterate() {
    let sb = Superblock {
        inodes_count: 64,
        blocks_count: 128,
        log_block_size: 2,
        blocks_per_group: 128,
        inodes_per_group: 64,
        inode_size: 256,
        feature_incompat: Superblock::FEATURE_INCOMPAT_EXTENTS,
        feature_ro_compat: Superblock::FEATURE_RO_COMPAT_HUGE_FILE,
        journal_inode: 8,
        ..Superblock::default()
    };
    let mut sb_bytes = [0u8; 1024];
    sb.encode(&mut sb_bytes).unwrap();
    assert_eq!(Superblock::parse(&sb_bytes).unwrap(), sb);
    assert_eq!(
        Superblock::parse(&sb_bytes).unwrap().block_size(),
        BLOCK_SIZE as u32
    );

    let desc = GroupDesc {
        block_bitmap: 2,
        inode_bitmap: 3,
        inode_table: 4,
        free_blocks_count: 42,
        free_inodes_count: 7,
        used_dirs_count: 1,
        ..GroupDesc::default()
    };
    let mut desc_bytes = [0u8; 64];
    desc.encode(&mut desc_bytes).unwrap();
    assert_eq!(GroupDesc::parse(&desc_bytes).unwrap(), desc);

    let mut inode = Inode::default();
    inode.mode = 0x8000 | 0o644;
    inode.size = 12 * 1024;
    inode.blocks_512 = 24;
    inode.links_count = 1;
    inode.flags = Inode::EXTENTS_FL;
    inode
        .set_extent_root(&[
            Extent {
                logical_block: 0,
                len: 2,
                physical_start: 20,
            },
            Extent {
                logical_block: 3,
                len: 1,
                physical_start: 30,
            },
        ])
        .unwrap();
    let mut inode_bytes = [0u8; 256];
    inode.encode(&mut inode_bytes).unwrap();
    let parsed_inode = Inode::parse(&inode_bytes).unwrap();
    assert_eq!(parsed_inode.mode, inode.mode);
    assert_eq!(parsed_inode.size, inode.size);
    assert_eq!(
        parsed_inode.map_extent_block(3).unwrap(),
        tx_ext4_format::ondisk::BlockMapping::Data(30)
    );
    assert_eq!(
        parsed_inode.map_extent_block(2).unwrap(),
        tx_ext4_format::ondisk::BlockMapping::Hole
    );

    let header = ExtentHeader::parse(parsed_inode.extent_root_bytes()).unwrap();
    assert_eq!(header.entries, 2);
    assert_eq!(
        Extent::parse_all(parsed_inode.extent_root_bytes())
            .unwrap()
            .len(),
        2
    );

    let mut dir_block = [0u8; BLOCK_SIZE];
    tx_ext4_format::ondisk::encode_dir_entry(12, 16, 1, b"hello", &mut dir_block[..16]).unwrap();
    tx_ext4_format::ondisk::encode_dir_entry(13, 4080, 2, b"world", &mut dir_block[16..]).unwrap();
    let entries: Vec<_> = DirEntryIter::new(&dir_block)
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].name, b"hello");
    assert_eq!(entries[1].inode, 13);

    let bitmap = BitmapView::new(&[0b0001_0101]);
    assert!(bitmap.is_set(0));
    assert!(!bitmap.is_set(1));
    assert!(bitmap.is_set(2));
    assert_eq!(crc32c(0, b"123456789"), 0xE306_9283);

    let header = JournalHeader {
        magic: JBD2_MAGIC,
        block_type: JBD2_BLOCK_DESCRIPTOR,
        sequence: 9,
    };
    let mut journal_block = [0u8; BLOCK_SIZE];
    header.encode(&mut journal_block[..12]).unwrap();
    JournalBlockTag {
        block: 4,
        flags: JournalBlockTag::FLAG_LAST_TAG,
    }
    .encode(&mut journal_block[12..20])
    .unwrap();
    assert_eq!(JournalHeader::parse(&journal_block[..12]).unwrap(), header);
    assert_eq!(
        JournalBlockTag::parse(&journal_block[12..20])
            .unwrap()
            .block,
        4
    );

    let commit = CommitHeader {
        sequence: 9,
        seconds: 1,
        nanoseconds: 2,
    };
    let mut commit_block = [0u8; BLOCK_SIZE];
    commit.encode(&mut commit_block).unwrap();
    assert_eq!(
        JournalHeader::parse(&commit_block[..12])
            .unwrap()
            .block_type,
        JBD2_BLOCK_COMMIT
    );
    assert_eq!(
        JournalHeader::parse(&commit_block[..12]).unwrap().magic,
        JBD2_MAGIC
    );
    assert_eq!(CommitHeader::parse(&commit_block).unwrap(), commit);
}

#[test]
fn pager_reads_inode_meta_and_4k_pages() {
    let image = mock_image();
    let mut pager = Ext4Pager::open(image).unwrap();

    let meta = pager.inode_meta(InodeNo::new(12)).unwrap();
    assert_eq!(
        meta,
        InodeMetaLite {
            mode: 0x8000 | 0o644,
            uid: 1000,
            gid: 1000,
            size: 4 * BLOCK_SIZE as u64,
            nlinks: 1,
            blocks_512: 24,
            flags: Inode::EXTENTS_FL,
            atime: 11,
            ctime: 12,
            mtime: 13,
        }
    );

    let mut page = [0u8; BLOCK_SIZE];
    assert_eq!(
        pager.read_page(InodeNo::new(12), 0, &mut page).unwrap(),
        PageRead::Data { block: 20 }
    );
    assert_eq!(page, filled_page(0x20));

    assert_eq!(
        pager.read_page(InodeNo::new(12), 2, &mut page).unwrap(),
        PageRead::Hole
    );
    assert_eq!(page, [0u8; BLOCK_SIZE]);

    assert_eq!(
        pager.read_page(InodeNo::new(12), 3, &mut page).unwrap(),
        PageRead::Data { block: 30 }
    );
    assert_eq!(page, filled_page(0x30));
}

#[test]
fn pager_zero_fills_beyond_eof_inside_last_page() {
    let mut image = mock_image();
    let mut short_inode = Inode::default();
    short_inode.mode = 0x8000 | 0o644;
    short_inode.size = 13;
    short_inode.blocks_512 = 8;
    short_inode.links_count = 1;
    short_inode.flags = Inode::EXTENTS_FL;
    short_inode
        .set_extent_root(&[Extent {
            logical_block: 0,
            len: 1,
            physical_start: 31,
        }])
        .unwrap();
    write_inode(&mut image, 14, &short_inode);
    *image.block_mut(31) = filled_page(0xAB);
    image.block_mut(31)[..13].copy_from_slice(b"short payload");

    let mut pager = Ext4Pager::open(image).unwrap();
    let mut page = [0u8; BLOCK_SIZE];
    assert_eq!(
        pager.read_page(InodeNo::new(14), 0, &mut page).unwrap(),
        PageRead::Data { block: 31 }
    );
    assert_eq!(&page[..13], b"short payload");
    assert!(page[13..].iter().all(|byte| *byte == 0));

    assert_eq!(
        pager.read_page(InodeNo::new(14), 1, &mut page).unwrap(),
        PageRead::Hole
    );
    assert_eq!(page, [0u8; BLOCK_SIZE]);
}

#[test]
fn pager_lists_and_looks_up_linear_directories() {
    let image = mock_image();
    let mut pager = Ext4Pager::open(image).unwrap();

    assert_eq!(
        pager.lookup(InodeNo::new(2), b"hello").unwrap(),
        Some(InodeNo::new(12))
    );
    assert_eq!(pager.lookup(InodeNo::new(2), b"missing").unwrap(), None);

    let mut entries = [DirEntryLite::empty(); 8];
    let count = pager
        .read_dir_entries(InodeNo::new(2), &mut entries)
        .unwrap();
    assert_eq!(count, 4);
    assert_dir_entry(&entries[0], b".", InodeNo::new(2), 2);
    assert_dir_entry(&entries[1], b"..", InodeNo::new(2), 2);
    assert_dir_entry(&entries[2], b"hello", InodeNo::new(12), 1);
    assert_dir_entry(&entries[3], b"nested", InodeNo::new(13), 2);

    assert_eq!(
        pager.lookup(InodeNo::new(13), b"child").unwrap(),
        Some(InodeNo::new(12))
    );
}

#[test]
fn pager_rejects_non_4k_images_and_hole_writeback() {
    let mut image = mock_image();
    let mut bad_sb = Superblock::parse(&image.block(0)[1024..2048]).unwrap();
    bad_sb.log_block_size = 1;
    bad_sb.encode(&mut image.block_mut(0)[1024..2048]).unwrap();
    assert!(matches!(
        Ext4Pager::open(image),
        Err(Ext4FormatError::Unsupported)
    ));

    let image = mock_image();
    let mut pager = Ext4Pager::open(image).unwrap();
    assert_eq!(
        pager
            .write_existing_page(InodeNo::new(12), 2, &filled_page(0xAA))
            .unwrap_err(),
        Ext4FormatError::Unsupported
    );
}

#[test]
fn pager_writeback_and_journal_replay_on_mock_image() {
    let image = mock_image();
    let mut pager = Ext4Pager::open(image).unwrap();

    let page = filled_page(0xEE);
    assert_eq!(
        pager
            .write_existing_page(InodeNo::new(12), 1, &page)
            .unwrap(),
        WritebackReceipt {
            inode: InodeNo::new(12),
            file_page_index: 1,
            physical_block: 21,
        }
    );
    assert_eq!(pager.image().block(21), &page);

    let mut updated = pager.inode_meta(InodeNo::new(12)).unwrap();
    updated.size = 3 * BLOCK_SIZE as u64;
    updated.mtime = 99;
    let receipt = pager
        .write_inode_meta_journaled(InodeNo::new(12), updated)
        .unwrap();
    assert_eq!(receipt.sequence, 1);
    assert_eq!(receipt.target_blocks, vec![4]);
    assert_eq!(receipt.descriptor_block, 41);
    assert_eq!(receipt.payload_blocks, vec![42]);
    assert_eq!(receipt.commit_block, 43);
    assert_eq!(pager.image().barriers, 3);

    let descriptor = pager.image().block(receipt.descriptor_block);
    assert_eq!(
        JournalHeader::parse(&descriptor[..12]).unwrap(),
        JournalHeader {
            magic: JBD2_MAGIC,
            block_type: JBD2_BLOCK_DESCRIPTOR,
            sequence: 1,
        }
    );
    let tags = parse_journal_descriptor(descriptor).unwrap().1;
    let tag = tags[0];
    assert_eq!(tag.block, receipt.target_blocks[0] as u32);
    assert_eq!(tag.flags, JournalBlockTag::FLAG_LAST_TAG);

    assert_eq!(
        pager.inode_meta(InodeNo::new(12)).unwrap().size,
        updated.size
    );
    let replay = pager.replay_journal_for_test().unwrap();
    assert_eq!(replay.transactions, 1);
    assert_eq!(replay.blocks_replayed, 1);
    assert_eq!(
        pager.inode_meta(InodeNo::new(12)).unwrap().size,
        updated.size
    );
    assert_eq!(pager.inode_meta(InodeNo::new(12)).unwrap().mtime, 99);
}

#[test]
fn journal_descriptor_round_trips_multiple_target_tags() {
    let mut block = [0u8; BLOCK_SIZE];
    encode_journal_descriptor(7, &[4, 9, 12], &mut block).unwrap();
    let (header, tags) = parse_journal_descriptor(&block).unwrap();
    assert_eq!(
        header,
        JournalHeader {
            magic: JBD2_MAGIC,
            block_type: JBD2_BLOCK_DESCRIPTOR,
            sequence: 7,
        }
    );
    assert_eq!(tags.len(), 3);
    assert_eq!(tags[0].block, 4);
    assert_eq!(tags[0].flags, 0);
    assert_eq!(tags[1].block, 9);
    assert_eq!(tags[1].flags, 0);
    assert_eq!(tags[2].block, 12);
    assert_eq!(tags[2].flags, JournalBlockTag::FLAG_LAST_TAG);
}

#[test]
fn metadata_transaction_coalesces_commits_and_checkpoints_home_blocks() {
    let mut image = mock_image();
    *image.block_mut(5) = filled_page(0x05);
    let mut pager = Ext4Pager::open(image).unwrap();

    let update_a = filled_page(0xA1);
    let stale = filled_page(0x55);
    let update_b = filled_page(0xB2);
    let receipt = pager
        .commit_metadata_transaction(&[
            MetadataUpdate {
                target_block: 4,
                data: update_a,
            },
            MetadataUpdate {
                target_block: 5,
                data: stale,
            },
            MetadataUpdate {
                target_block: 5,
                data: update_b,
            },
        ])
        .unwrap();

    assert_eq!(receipt.sequence, 1);
    assert_eq!(receipt.target_blocks, vec![4, 5]);
    assert_eq!(receipt.descriptor_block, 41);
    assert_eq!(receipt.payload_blocks, vec![42, 43]);
    assert_eq!(receipt.commit_block, 44);
    assert_eq!(pager.image().barriers, 3);
    assert_eq!(pager.image().block(4), &update_a);
    assert_eq!(pager.image().block(5), &update_b);

    let (_, tags) = parse_journal_descriptor(pager.image().block(41)).unwrap();
    assert_eq!(
        tags.iter().map(|tag| tag.block).collect::<Vec<_>>(),
        vec![4, 5]
    );
    assert_eq!(
        CommitHeader::parse(pager.image().block(44))
            .unwrap()
            .sequence,
        1
    );
}

#[test]
fn journal_replay_applies_committed_multi_block_records_only() {
    let mut image = mock_image();
    let before_a = *image.block(4);
    let before_b = *image.block(5);

    let update_a = filled_page(0xC1);
    let update_b = filled_page(0xC2);
    let mut descriptor = [0u8; BLOCK_SIZE];
    encode_journal_descriptor(1, &[4, 5], &mut descriptor).unwrap();
    *image.block_mut(41) = descriptor;
    *image.block_mut(42) = update_a;
    *image.block_mut(43) = update_b;

    let mut pager = Ext4Pager::open(image.clone()).unwrap();
    let replay = pager.replay_journal_for_test().unwrap();
    assert_eq!(replay.transactions, 0);
    assert_eq!(pager.image().block(4), &before_a);
    assert_eq!(pager.image().block(5), &before_b);

    let mut commit = [0u8; BLOCK_SIZE];
    encode_journal_commit(1, &mut commit).unwrap();
    *image.block_mut(44) = commit;
    let mut pager = Ext4Pager::open(image).unwrap();
    let replay = pager.replay_journal_for_test().unwrap();
    assert_eq!(replay.transactions, 1);
    assert_eq!(replay.blocks_replayed, 2);
    assert_eq!(pager.image().block(4), &update_a);
    assert_eq!(pager.image().block(5), &update_b);
}

#[test]
fn xattr_external_block_creation_journals_bitmap_xattr_and_inode_pointer() {
    let mut image = mock_image();
    {
        let mut bitmap = BitmapMut::new(image.block_mut(2));
        for bit in 0..56 {
            bitmap.set(bit).unwrap();
        }
    }
    let mut inode = Inode::default();
    inode.mode = 0x8000 | 0o600;
    inode.size = BLOCK_SIZE as u64;
    inode.blocks_512 = 8;
    inode.links_count = 1;
    inode.flags = Inode::EXTENTS_FL;
    inode.extra_isize = 32;
    inode
        .set_extent_root(&[Extent {
            logical_block: 0,
            len: 1,
            physical_start: 31,
        }])
        .unwrap();
    write_inode(&mut image, 14, &inode);

    let mut pager = Ext4Pager::open(image).unwrap();
    let value = [0xAB; 200];
    assert!(pager
        .set_xattr(InodeNo::new(14), b"user.large", &value, true, false)
        .unwrap());

    assert!(BitmapView::new(pager.image().block(2)).is_set(56));
    assert_eq!(
        u32::from_le_bytes(pager.image().block(56)[0..4].try_into().unwrap()),
        tx_ext4_format::xattr::EXT4_XATTR_MAGIC
    );
    let raw = &pager.image().block(4)[3328..3584];
    assert_eq!(Inode::parse(raw).unwrap().file_acl, 56);

    let attrs = pager.xattrs(InodeNo::new(14)).unwrap();
    assert_eq!(attrs.len(), 1);
    assert_eq!(attrs[0].name, b"user.large");
    assert_eq!(attrs[0].value, value);
}

#[test]
fn xattr_shared_external_block_update_cows_and_decrements_old_refcount() {
    let mut image = mock_image();
    install_inode12_inline_and_external_xattrs(&mut image, 32);
    {
        let mut bitmap = BitmapMut::new(image.block_mut(2));
        for bit in 0..56 {
            bitmap.set(bit).unwrap();
        }
    }
    let mut second = Inode::default();
    second.mode = 0x8000 | 0o600;
    second.size = BLOCK_SIZE as u64;
    second.blocks_512 = 16;
    second.links_count = 1;
    second.flags = Inode::EXTENTS_FL;
    second.extra_isize = 32;
    second.file_acl = 32;
    second
        .set_extent_root(&[Extent {
            logical_block: 0,
            len: 1,
            physical_start: 31,
        }])
        .unwrap();
    write_inode(&mut image, 14, &second);
    image.block_mut(32)[4..8].copy_from_slice(&2u32.to_le_bytes());

    let mut pager = Ext4Pager::open(image).unwrap();
    let private = [0xCD; 200];
    assert!(pager
        .set_xattr(InodeNo::new(14), b"user.omega", &private, false, false)
        .unwrap());

    assert_eq!(
        u32::from_le_bytes(pager.image().block(32)[4..8].try_into().unwrap()),
        1
    );
    assert!(BitmapView::new(pager.image().block(2)).is_set(56));
    assert_eq!(read_inode_from_image(pager.image(), 14).file_acl, 56);
    assert_eq!(read_inode_from_image(pager.image(), 12).file_acl, 32);

    let attrs_14 = pager.xattrs(InodeNo::new(14)).unwrap();
    assert_eq!(
        attrs_14
            .iter()
            .find(|attr| attr.name == b"user.omega")
            .unwrap()
            .value,
        private
    );
    let attrs_12 = pager.xattrs(InodeNo::new(12)).unwrap();
    assert_eq!(
        attrs_12
            .iter()
            .find(|attr| attr.name == b"user.omega")
            .unwrap()
            .value,
        b"external"
    );
}

#[test]
fn xattr_last_external_removal_frees_block_and_moves_remaining_attrs_inline() {
    let mut image = mock_image();
    install_inode12_inline_and_external_xattrs(&mut image, 32);
    let before_group = GroupDesc::parse(&image.block(1)[..64]).unwrap();
    let before_sb = Superblock::parse(&image.block(0)[1024..2048]).unwrap();

    let mut pager = Ext4Pager::open(image).unwrap();
    assert!(pager.remove_xattr(InodeNo::new(12), b"user.omega").unwrap());

    assert!(!BitmapView::new(pager.image().block(2)).is_set(32));
    let group = GroupDesc::parse(&pager.image().block(1)[..64]).unwrap();
    assert_eq!(
        group.free_blocks_count,
        before_group.free_blocks_count.saturating_add(1)
    );
    let sb = Superblock::parse(&pager.image().block(0)[1024..2048]).unwrap();
    assert_eq!(
        sb.free_blocks_count,
        before_sb.free_blocks_count.saturating_add(1)
    );

    let inode = read_inode_from_image(pager.image(), 12);
    assert_eq!(inode.file_acl, 0);
    assert_eq!(inode.blocks_512, 24);
    let attrs = pager.xattrs(InodeNo::new(12)).unwrap();
    assert_eq!(attrs.len(), 2);
    assert!(attrs.iter().any(|attr| attr.name == b"user.alpha"));
    assert!(attrs.iter().any(|attr| attr.name == b"user.zeta"));
}

#[test]
fn xattr_external_create_is_visible_to_later_create_checks() {
    let mut image = mock_image();
    install_inode12_inline_and_external_xattrs(&mut image, 32);
    let mut pager = Ext4Pager::open(image).unwrap();

    assert!(pager
        .set_xattr(InodeNo::new(12), b"user.beta", b"created", true, false)
        .unwrap());
    assert!(!pager
        .set_xattr(InodeNo::new(12), b"user.beta", b"again", true, false)
        .unwrap());
}

#[test]
fn xattr_oversized_external_value_fails_without_accounting_drift() {
    let mut image = mock_image();
    {
        let mut bitmap = BitmapMut::new(image.block_mut(2));
        for bit in 0..56 {
            bitmap.set(bit).unwrap();
        }
    }
    let mut inode = Inode::default();
    inode.mode = 0x8000 | 0o600;
    inode.size = BLOCK_SIZE as u64;
    inode.blocks_512 = 8;
    inode.links_count = 1;
    inode.flags = Inode::EXTENTS_FL;
    inode.extra_isize = 32;
    inode
        .set_extent_root(&[Extent {
            logical_block: 0,
            len: 1,
            physical_start: 31,
        }])
        .unwrap();
    write_inode(&mut image, 14, &inode);
    let before_group = GroupDesc::parse(&image.block(1)[..64]).unwrap();
    let before_sb = Superblock::parse(&image.block(0)[1024..2048]).unwrap();
    let mut pager = Ext4Pager::open(image).unwrap();

    let too_large = vec![0xFE; BLOCK_SIZE];
    assert_eq!(
        pager.set_xattr(InodeNo::new(14), b"user.huge", &too_large, false, false),
        Err(Ext4FormatError::Unsupported)
    );

    let after_group = GroupDesc::parse(&pager.image().block(1)[..64]).unwrap();
    let after_sb = Superblock::parse(&pager.image().block(0)[1024..2048]).unwrap();
    assert_eq!(
        after_group.free_blocks_count,
        before_group.free_blocks_count
    );
    assert_eq!(after_sb.free_blocks_count, before_sb.free_blocks_count);

    let private = [0xCD; 200];
    assert!(pager
        .set_xattr(InodeNo::new(14), b"user.omega", &private, false, false)
        .unwrap());
    assert_eq!(read_inode_from_image(pager.image(), 14).file_acl, 56);
    let final_group = GroupDesc::parse(&pager.image().block(1)[..64]).unwrap();
    let final_sb = Superblock::parse(&pager.image().block(0)[1024..2048]).unwrap();
    assert_eq!(
        final_group.free_blocks_count,
        before_group.free_blocks_count.saturating_sub(1)
    );
    assert_eq!(
        final_sb.free_blocks_count,
        before_sb.free_blocks_count.saturating_sub(1)
    );
}

#[test]
fn xattr_reads_ea_inode_value_spanning_multiple_blocks() {
    let mut image = mock_image();
    let value = ea_inode_value();
    install_inode12_external_ea_inode_xattr(&mut image, 32, 14, b"large", &value);

    let mut pager = Ext4Pager::open(image).unwrap();
    let attrs = pager.xattrs(InodeNo::new(12)).unwrap();
    let large = attrs
        .iter()
        .find(|attr| attr.name == b"user.large")
        .expect("ea-inode xattr");

    assert_eq!(large.value, value);
}

#[test]
fn xattr_rejects_ea_inode_hash_size_and_flag_mismatch() {
    let value = ea_inode_value();
    for corrupt in [
        EaInodeCorruption::BadEntryHash,
        EaInodeCorruption::BadInodeHash,
        EaInodeCorruption::BadSize,
        EaInodeCorruption::MissingFlag,
    ] {
        let mut image = mock_image();
        install_inode12_external_ea_inode_xattr(&mut image, 32, 14, b"large", &value);
        match corrupt {
            EaInodeCorruption::BadEntryHash => {
                image.block_mut(32)[44..48].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
            }
            EaInodeCorruption::BadInodeHash => {
                let mut inode = read_inode_from_image(&image, 14);
                inode.atime ^= 1;
                write_inode(&mut image, 14, &inode);
            }
            EaInodeCorruption::BadSize => {
                let mut inode = read_inode_from_image(&image, 14);
                inode.size -= 1;
                write_inode(&mut image, 14, &inode);
            }
            EaInodeCorruption::MissingFlag => {
                let mut inode = read_inode_from_image(&image, 14);
                inode.flags &= !Inode::EA_INODE_FL;
                write_inode(&mut image, 14, &inode);
            }
        }

        let mut pager = Ext4Pager::open(image).unwrap();
        assert_eq!(
            pager.xattrs(InodeNo::new(12)),
            Err(Ext4FormatError::Corrupt)
        );
    }
}

#[test]
fn xattr_mutation_refuses_existing_ea_inode_values() {
    let mut image = mock_image();
    let value = ea_inode_value();
    install_inode12_external_ea_inode_xattr(&mut image, 32, 14, b"large", &value);

    let mut pager = Ext4Pager::open(image).unwrap();
    assert_eq!(
        pager.set_xattr(InodeNo::new(12), b"user.alpha", b"new", false, false),
        Err(Ext4FormatError::Unsupported)
    );
    assert_eq!(
        pager.remove_xattr(InodeNo::new(12), b"user.large"),
        Err(Ext4FormatError::Unsupported)
    );

    let attrs = pager.xattrs(InodeNo::new(12)).unwrap();
    assert_eq!(
        attrs
            .iter()
            .find(|attr| attr.name == b"user.large")
            .unwrap()
            .value,
        value
    );
}

#[test]
fn pager_resolves_indexed_extents_and_inodes_across_groups() {
    let mut image = mock_image();
    let mut sb = Superblock::parse(&image.block(0)[1024..2048]).unwrap();
    sb.inodes_count = 128;
    sb.blocks_count = 128;
    sb.blocks_per_group = 64;
    sb.inodes_per_group = 64;
    sb.desc_size = 64;
    sb.feature_incompat |= Superblock::FEATURE_INCOMPAT_64BIT;
    sb.encode(&mut image.block_mut(0)[1024..2048]).unwrap();

    let group1 = GroupDesc {
        block_bitmap: 66,
        inode_bitmap: 67,
        inode_table: 70,
        free_blocks_count: 10,
        free_inodes_count: 63,
        used_dirs_count: 0,
        ..GroupDesc::default()
    };
    group1.encode(&mut image.block_mut(1)[64..128]).unwrap();

    let mut leaf = [0u8; BLOCK_SIZE];
    ExtentNode::encode_leaf(
        &[Extent {
            logical_block: 0,
            len: 1,
            physical_start: 80,
        }],
        &mut leaf,
    )
    .unwrap();
    *image.block_mut(76) = leaf;
    *image.block_mut(80) = filled_page(0x80);

    let mut grouped_inode = Inode::default();
    grouped_inode.mode = 0x8000 | 0o600;
    grouped_inode.size = BLOCK_SIZE as u64;
    grouped_inode.blocks_512 = 8;
    grouped_inode.links_count = 1;
    grouped_inode.flags = Inode::EXTENTS_FL;
    grouped_inode
        .set_extent_index_root(
            &[ExtentIdx {
                logical_block: 0,
                child: 76,
            }],
            1,
        )
        .unwrap();
    write_inode_at_table(&mut image, 70, 1, &grouped_inode);

    let mut pager = Ext4Pager::open(image).unwrap();
    let meta = pager.inode_meta(InodeNo::new(65)).unwrap();
    assert_eq!(meta.size, BLOCK_SIZE as u64);

    let mut page = [0u8; BLOCK_SIZE];
    assert_eq!(
        pager.read_page(InodeNo::new(65), 0, &mut page).unwrap(),
        PageRead::Data { block: 80 }
    );
    assert_eq!(page, filled_page(0x80));
}

#[test]
fn bitmap_mutation_and_metadata_checksum_helpers_match_ext4_chaining() {
    let mut bytes = [0b0000_0011u8, 0];
    let mut bitmap = BitmapMut::new(&mut bytes);
    assert_eq!(bitmap.allocate_run(3).unwrap(), 2);
    assert!(bitmap.is_set(2));
    assert!(bitmap.is_set(3));
    assert!(bitmap.is_set(4));
    bitmap.clear(3).unwrap();
    assert!(!bitmap.is_set(3));
    assert_eq!(bitmap.allocate_run(2).unwrap(), 5);
    assert!(bitmap.set(3).is_ok());
    assert_eq!(bitmap.set(32).unwrap_err(), Ext4FormatError::OutOfBounds);

    let seed = 0xFFFF_FFFF;
    let chained = crc32c_append(crc32c_append(seed, b"abc"), b"def");
    assert_eq!(
        metadata_csum32(seed, &[b"abc".as_slice(), b"def".as_slice()]),
        chained
    );
    assert_eq!(crc32c(0, b"123456789"), 0xE306_9283);
}

#[test]
fn htree_dx_records_and_legacy_hash_roundtrip() {
    let info = DxRootInfo {
        hash_version: DxRootInfo::DX_HASH_LEGACY,
        info_length: DxRootInfo::INFO_LENGTH,
        indirect_levels: 1,
        flags: 0,
    };
    let mut info_bytes = [0xAAu8; 8];
    info.encode(&mut info_bytes).unwrap();
    assert_eq!(DxRootInfo::parse(&info_bytes).unwrap(), info);

    let count_limit = DxCountLimit {
        limit: 508,
        count: 2,
    };
    let mut count_bytes = [0u8; 4];
    count_limit.encode(&mut count_bytes).unwrap();
    assert_eq!(DxCountLimit::parse(&count_bytes).unwrap(), count_limit);

    let mut entries = [0u8; 16];
    DxEntry {
        hash: 0x1000,
        block: 4,
    }
    .encode(&mut entries[0..8])
    .unwrap();
    DxEntry {
        hash: 0x2000,
        block: 9,
    }
    .encode(&mut entries[8..16])
    .unwrap();
    let parsed: Vec<_> = DxEntryIter::new(&entries, 2)
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(parsed[0].hash, 0x1000);
    assert_eq!(parsed[1].block, 9);

    let seed = [0x1234_5678, 0x8765_4321, 0, 0];
    assert_eq!(
        tx_ext4_format::ondisk::dx_hash(b"abc", DxRootInfo::DX_HASH_LEGACY, &seed).unwrap(),
        ((0u32.wrapping_mul(33).wrapping_add(b'a' as u32))
            .wrapping_mul(33)
            .wrapping_add(b'b' as u32))
        .wrapping_mul(33)
        .wrapping_add(b'c' as u32)
    );
    assert_ne!(
        tx_ext4_format::ondisk::dx_hash(b"abc", DxRootInfo::DX_HASH_TEA, &seed).unwrap(),
        0
    );
}

fn mock_image() -> MemImage {
    let mut image = MemImage::new(128);

    let sb = Superblock {
        inodes_count: 64,
        blocks_count: 64,
        free_blocks_count: 32,
        log_block_size: 2,
        blocks_per_group: 64,
        inodes_per_group: 64,
        inode_size: 256,
        feature_incompat: Superblock::FEATURE_INCOMPAT_EXTENTS,
        feature_ro_compat: Superblock::FEATURE_RO_COMPAT_HUGE_FILE,
        journal_inode: 8,
        ..Superblock::default()
    };
    sb.encode(&mut image.block_mut(0)[1024..2048]).unwrap();

    GroupDesc {
        block_bitmap: 2,
        inode_bitmap: 3,
        inode_table: 4,
        free_blocks_count: 32,
        free_inodes_count: 52,
        used_dirs_count: 1,
        ..GroupDesc::default()
    }
    .encode(&mut image.block_mut(1)[..64])
    .unwrap();

    let mut journal_inode = Inode::default();
    journal_inode.mode = 0x8000 | 0o600;
    journal_inode.size = 16 * BLOCK_SIZE as u64;
    journal_inode.blocks_512 = 128;
    journal_inode.links_count = 1;
    journal_inode.flags = Inode::EXTENTS_FL;
    journal_inode
        .set_extent_root(&[Extent {
            logical_block: 0,
            len: 16,
            physical_start: 40,
        }])
        .unwrap();
    write_inode(&mut image, 8, &journal_inode);

    let mut root_inode = Inode::default();
    root_inode.mode = 0x4000 | 0o755;
    root_inode.size = BLOCK_SIZE as u64;
    root_inode.blocks_512 = 8;
    root_inode.links_count = 3;
    root_inode.flags = Inode::EXTENTS_FL;
    root_inode
        .set_extent_root(&[Extent {
            logical_block: 0,
            len: 1,
            physical_start: 16,
        }])
        .unwrap();
    write_inode(&mut image, 2, &root_inode);

    let mut file_inode = Inode::default();
    file_inode.mode = 0x8000 | 0o644;
    file_inode.uid = 1000;
    file_inode.gid = 1000;
    file_inode.size = 4 * BLOCK_SIZE as u64;
    file_inode.atime = 11;
    file_inode.ctime = 12;
    file_inode.mtime = 13;
    file_inode.links_count = 1;
    file_inode.blocks_512 = 24;
    file_inode.flags = Inode::EXTENTS_FL;
    file_inode
        .set_extent_root(&[
            Extent {
                logical_block: 0,
                len: 2,
                physical_start: 20,
            },
            Extent {
                logical_block: 3,
                len: 1,
                physical_start: 30,
            },
        ])
        .unwrap();
    write_inode(&mut image, 12, &file_inode);

    let mut nested_inode = Inode::default();
    nested_inode.mode = 0x4000 | 0o755;
    nested_inode.size = BLOCK_SIZE as u64;
    nested_inode.blocks_512 = 8;
    nested_inode.links_count = 2;
    nested_inode.flags = Inode::EXTENTS_FL;
    nested_inode
        .set_extent_root(&[Extent {
            logical_block: 0,
            len: 1,
            physical_start: 17,
        }])
        .unwrap();
    write_inode(&mut image, 13, &nested_inode);

    encode_directory(
        image.block_mut(16),
        &[
            (2, 2, b".".as_slice()),
            (2, 2, b"..".as_slice()),
            (12, 1, b"hello".as_slice()),
            (13, 2, b"nested".as_slice()),
        ],
    );
    encode_directory(
        image.block_mut(17),
        &[
            (13, 2, b".".as_slice()),
            (2, 2, b"..".as_slice()),
            (12, 1, b"child".as_slice()),
        ],
    );

    *image.block_mut(20) = filled_page(0x20);
    *image.block_mut(21) = filled_page(0x21);
    *image.block_mut(30) = filled_page(0x30);

    image
}

fn assert_dir_entry(entry: &DirEntryLite, name: &[u8], inode: InodeNo, file_type: u8) {
    assert_eq!(entry.name(), name);
    assert_eq!(entry.inode, inode);
    assert_eq!(entry.file_type, file_type);
}

fn encode_directory(block: &mut [u8; BLOCK_SIZE], entries: &[(u32, u8, &[u8])]) {
    block.fill(0);
    let mut offset = 0usize;
    for (idx, (inode, file_type, name)) in entries.iter().enumerate() {
        let rec_len = if idx == entries.len() - 1 {
            (BLOCK_SIZE - offset) as u16
        } else {
            (8 + name.len()).next_multiple_of(4) as u16
        };
        tx_ext4_format::ondisk::encode_dir_entry(
            *inode,
            rec_len,
            *file_type,
            name,
            &mut block[offset..],
        )
        .unwrap();
        offset += rec_len as usize;
    }
}

fn write_inode(image: &mut MemImage, ino: u32, inode: &Inode) {
    write_inode_at_table(image, 4, ino, inode);
}

fn write_inode_at_table(image: &mut MemImage, inode_table_block: u64, ino: u32, inode: &Inode) {
    let index = (ino - 1) as usize;
    let offset = index * 256;
    let block = inode_table_block as usize + offset / BLOCK_SIZE;
    let in_block = offset % BLOCK_SIZE;
    inode
        .encode(&mut image.block_mut(block as u64)[in_block..in_block + 256])
        .unwrap();
}

fn install_inode12_inline_and_external_xattrs(image: &mut MemImage, block: u64) {
    let mut inode = read_inode_from_image(image, 12);
    inode.extra_isize = 32;
    inode.file_acl = block;
    inode.blocks_512 = 32;
    let index = 11usize;
    let offset = index * 256;
    let table_block = 4 + offset / BLOCK_SIZE;
    let in_block = offset % BLOCK_SIZE;
    let raw = &mut image.block_mut(table_block as u64)[in_block..in_block + 256];
    inode.encode(raw).unwrap();
    encode_inline_xattrs(
        &inode,
        raw,
        &[
            InlineXattr {
                name: b"user.alpha".to_vec(),
                value: b"bravo".to_vec(),
            },
            InlineXattr {
                name: b"user.zeta".to_vec(),
                value: b"last".to_vec(),
            },
        ],
    )
    .unwrap();
    BitmapMut::new(image.block_mut(2))
        .set(block as usize)
        .unwrap();
    encode_external_xattr_block(
        &Superblock::parse(&image.block(0)[1024..2048]).unwrap(),
        block,
        image.block_mut(block),
        &[InlineXattr {
            name: b"user.omega".to_vec(),
            value: b"external".to_vec(),
        }],
    )
    .unwrap();
}

#[derive(Clone, Copy)]
enum EaInodeCorruption {
    BadEntryHash,
    BadInodeHash,
    BadSize,
    MissingFlag,
}

fn ea_inode_value() -> Vec<u8> {
    let mut value = vec![0xA5; BLOCK_SIZE + 17];
    value[BLOCK_SIZE..].fill(0x5A);
    value
}

fn install_inode12_external_ea_inode_xattr(
    image: &mut MemImage,
    xattr_block: u64,
    ea_ino: u32,
    suffix: &[u8],
    value: &[u8],
) {
    let mut inode = read_inode_from_image(image, 12);
    inode.extra_isize = 32;
    inode.file_acl = xattr_block;
    inode.blocks_512 = inode.blocks_512.saturating_add((BLOCK_SIZE / 512) as u64);
    write_inode(image, 12, &inode);

    BitmapMut::new(image.block_mut(2))
        .set(xattr_block as usize)
        .unwrap();

    let value_hash = xattr_inode_hash(
        &Superblock::parse(&image.block(0)[1024..2048]).unwrap(),
        value,
    );
    let entry_hash = xattr_entry_hash(suffix, value_hash);
    let block = image.block_mut(xattr_block);
    block.fill(0);
    write_u32_le(block, 0, tx_ext4_format::xattr::EXT4_XATTR_MAGIC);
    write_u32_le(block, 4, 1);
    write_u32_le(block, 8, 1);
    block[32] = suffix.len() as u8;
    block[33] = tx_ext4_format::xattr::EXT4_XATTR_INDEX_USER;
    write_u16_le(block, 34, 0);
    write_u32_le(block, 36, ea_ino);
    write_u32_le(block, 40, value.len() as u32);
    write_u32_le(block, 44, entry_hash);
    block[48..48 + suffix.len()].copy_from_slice(suffix);
    let last = (48 + suffix.len() + 3) & !3;
    block[last..last + 4].fill(0);

    let mut ea_inode = Inode::default();
    ea_inode.mode = Inode::S_IFREG | 0o600;
    ea_inode.size = value.len() as u64;
    ea_inode.blocks_512 = (2 * BLOCK_SIZE / 512) as u64;
    ea_inode.links_count = 1;
    ea_inode.flags = Inode::EXTENTS_FL | Inode::EA_INODE_FL;
    ea_inode.atime = value_hash;
    ea_inode
        .set_extent_root(&[Extent {
            logical_block: 0,
            len: 2,
            physical_start: 60,
        }])
        .unwrap();
    write_inode(image, ea_ino, &ea_inode);
    image.block_mut(60).copy_from_slice(&value[..BLOCK_SIZE]);
    image.block_mut(61)[..value.len() - BLOCK_SIZE].copy_from_slice(&value[BLOCK_SIZE..]);
}

fn write_u16_le(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn write_u32_le(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn read_inode_from_image(image: &MemImage, ino: u32) -> Inode {
    let index = (ino - 1) as usize;
    let offset = index * 256;
    let block = 4 + offset / BLOCK_SIZE;
    let in_block = offset % BLOCK_SIZE;
    Inode::parse(&image.block(block as u64)[in_block..in_block + 256]).unwrap()
}

fn filled_page(byte: u8) -> [u8; BLOCK_SIZE] {
    [byte; BLOCK_SIZE]
}
