use tx_ext4_format::mutation::{FsyncStamp, MutationOrigin};
use tx_ext4_format::ondisk::{
    crc32c, crc32c_append, metadata_csum32, BitmapMut, BitmapView, CommitHeader, DirEntryIter,
    DxCountLimit, DxEntry, DxEntryIter, DxRootInfo, Ext4FormatError, Extent, ExtentHeader,
    ExtentIdx, ExtentNode, GroupDesc, Inode, JournalBlockTag, JournalHeader, Superblock,
    JBD2_BLOCK_COMMIT, JBD2_BLOCK_DESCRIPTOR, JBD2_MAGIC,
};
use tx_ext4_format::pager::{
    BlockImage, DirEntryLite, Ext4Pager, InodeMetaLite, InodeNo, PageRead, WritebackReceipt,
    BLOCK_SIZE,
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
fn pager_plans_mapped_write_without_mutating_home_block() {
    let image = mock_image();
    let original = *image.block(21);
    let mut pager = Ext4Pager::open(image).unwrap();
    let page = filled_page(0xEE);

    let plan = pager
        .plan_write_page(InodeNo::new(12), 1, &page, FsyncStamp::new(9))
        .unwrap();

    assert_eq!(plan.origin, MutationOrigin::FlushPage);
    assert_eq!(plan.object, 12);
    assert_eq!(plan.fsync_stamp, FsyncStamp::new(9));
    assert_eq!(plan.metadata, Vec::new());
    assert_eq!(plan.allocations, Vec::new());
    assert_eq!(plan.data.len(), 1);
    assert_eq!(plan.data[0].logical_page, 1);
    assert_eq!(plan.data[0].physical_block, 21);
    assert_eq!(plan.data[0].bytes, page);
    assert_eq!(pager.image().block(21), &original);
    assert_eq!(
        pager
            .plan_write_page(InodeNo::new(12), 2, &page, FsyncStamp::new(10))
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
    assert_eq!(receipt.target_block, 4);
    assert_eq!(receipt.descriptor_block, 41);
    assert_eq!(receipt.payload_block, 42);
    assert_eq!(receipt.commit_block, 43);
    assert_eq!(pager.image().barriers, 2);

    let descriptor = pager.image().block(receipt.descriptor_block);
    assert_eq!(
        JournalHeader::parse(&descriptor[..12]).unwrap(),
        JournalHeader {
            magic: JBD2_MAGIC,
            block_type: JBD2_BLOCK_DESCRIPTOR,
            sequence: 1,
        }
    );
    let tag = JournalBlockTag::parse(&descriptor[12..20]).unwrap();
    assert_eq!(tag.block, receipt.target_block as u32);
    assert_eq!(tag.flags, JournalBlockTag::FLAG_LAST_TAG);

    assert_ne!(
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

#[test]
fn append_multiple_dir_entries_all_findable() {
    // Reproduces the iozone -t dirent layer: several files created
    // back-to-back in one directory must all stay findable by name.
    let image = mock_image();
    let mut pager = Ext4Pager::open(image).unwrap();
    let names: [&[u8]; 4] = [b"dummy.0", b"dummy.1", b"dummy.2", b"dummy.3"];
    for (i, name) in names.iter().enumerate() {
        pager
            .append_dir_entry(InodeNo::new(2), name, InodeNo::new(20 + i as u32), 1)
            .unwrap();
    }
    for (i, name) in names.iter().enumerate() {
        assert_eq!(
            pager.lookup(InodeNo::new(2), name).unwrap(),
            Some(InodeNo::new(20 + i as u32)),
            "dir entry {:?} lost after subsequent appends",
            core::str::from_utf8(name).unwrap()
        );
    }
}

#[test]
fn write_page_to_hole_and_tail_reads_back() {
    // Exercises the iozone write-back primitive (write_page →
    // allocate_block + attach_data_block): fill a hole and extend the
    // tail of inode 12, read every page back, confirm pre-existing data
    // is untouched.
    let mut image = mock_image();
    mark_block_bitmap_used(&mut image, 48); // free blocks start at 48
    let mut pager = Ext4Pager::open(image).unwrap();
    let writes = [(2u64, 0xC2u8), (4, 0xC4), (5, 0xC5)];
    for (lb, fill) in writes {
        pager
            .write_page(InodeNo::new(12), lb, &filled_page(fill))
            .unwrap();
    }
    // The VFS sets the logical size via serialize_inode_meta; mirror that
    // so read_page doesn't treat logical 4/5 as past-EOF.
    pager
        .set_inode_size(InodeNo::new(12), 6 * BLOCK_SIZE as u64)
        .unwrap();
    for (lb, fill) in writes {
        let mut got = [0u8; BLOCK_SIZE];
        pager.read_page(InodeNo::new(12), lb, &mut got).unwrap();
        assert_eq!(got, filled_page(fill), "logical {lb} readback mismatch");
    }
    let mut got = [0u8; BLOCK_SIZE];
    pager.read_page(InodeNo::new(12), 0, &mut got).unwrap();
    assert_eq!(got, filled_page(0x20), "logical 0 clobbered by writeback");
}

fn mark_block_bitmap_used(image: &mut MemImage, count: usize) {
    let mut bm = BitmapMut::new(image.block_mut(2));
    for bit in 0..count {
        bm.set(bit).unwrap();
    }
}

#[test]
fn create_multiple_regular_files_all_findable() {
    // Full iozone -t create path: create several regular files back-to-back
    // in one directory (allocate_inode + write_inode + append_dir_entry),
    // then confirm every one is findable by name.
    let mut image = mock_image();
    mark_inode_bitmap_used(&mut image, 13); // inodes 1..=13 already used
    let mut pager = Ext4Pager::open(image).unwrap();
    let names: [&[u8]; 4] = [b"dummy.0", b"dummy.1", b"dummy.2", b"dummy.3"];
    let mut inos = Vec::new();
    for name in names {
        inos.push(
            pager
                .create_regular_file(InodeNo::new(2), name, 0o644, 0, 0, 0)
                .unwrap(),
        );
    }
    for (i, name) in names.iter().enumerate() {
        assert_eq!(
            pager.lookup(InodeNo::new(2), name).unwrap(),
            Some(inos[i]),
            "created file {:?} not findable",
            core::str::from_utf8(name).unwrap()
        );
    }
}

fn mark_inode_bitmap_used(image: &mut MemImage, count: usize) {
    let mut bm = BitmapMut::new(image.block_mut(3));
    for bit in 0..count {
        bm.set(bit).unwrap();
    }
}

fn mock_image() -> MemImage {
    let mut image = MemImage::new(128);

    let sb = Superblock {
        inodes_count: 64,
        blocks_count: 64,
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
    journal_inode.size = 8 * BLOCK_SIZE as u64;
    journal_inode.blocks_512 = 64;
    journal_inode.links_count = 1;
    journal_inode.flags = Inode::EXTENTS_FL;
    journal_inode
        .set_extent_root(&[Extent {
            logical_block: 0,
            len: 8,
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

fn filled_page(byte: u8) -> [u8; BLOCK_SIZE] {
    [byte; BLOCK_SIZE]
}
