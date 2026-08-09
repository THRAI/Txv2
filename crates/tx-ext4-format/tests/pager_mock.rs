use tx_ext4_format::mutation::{FsyncStamp, MutationOrigin, SetAttr};
use tx_ext4_format::ondisk::{
    BitmapMut, BitmapView, CommitHeader, DirEntryIter, DxCountLimit, DxEntry, DxEntryIter,
    DxRootInfo, Ext4FormatError, Extent, ExtentHeader, ExtentIdx, ExtentNode, GroupDesc, Inode,
    JBD2_BLOCK_COMMIT, JBD2_BLOCK_DESCRIPTOR, JBD2_MAGIC, JournalBlockTag, JournalHeader,
    Superblock, block_bitmap_csum32, crc32c, crc32c_append, dirblock_csum32, group_desc_csum16,
    inode_bitmap_csum32, inode_csum32, metadata_csum32, superblock_csum32,
};
use tx_ext4_format::pager::{
    BLOCK_SIZE, BlockImage, DirEntryLite, Ext4Pager, InodeMetaLite, InodeNo, PageRead,
    WritebackReceipt,
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
fn read_write_admission_persists_recovery_required_before_mutation() {
    let image = mock_image();
    let mut pager = Ext4Pager::open(image).unwrap();

    pager.mark_recovery_required().unwrap();

    let superblock = Superblock::parse(&pager.image().block(0)[1024..2048]).unwrap();
    assert!(superblock.needs_recovery());
    assert_ne!(
        superblock.feature_compat & Superblock::FEATURE_COMPAT_HAS_JOURNAL,
        0
    );
    assert_eq!(pager.image().barriers, 1);
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
    assert_eq!(plan.metadata.len(), 1);
    assert_eq!(
        plan.metadata[0].role,
        tx_ext4_format::mutation::MetaRole::InodeTable
    );
    assert_eq!(plan.metadata[0].home, 4);
    assert_eq!(plan.allocations, Vec::new());
    assert_eq!(plan.data.len(), 1);
    assert_eq!(plan.data[0].logical_page, 1);
    assert_eq!(plan.data[0].physical_block, 21);
    assert_eq!(plan.data[0].bytes, page);
    assert_eq!(pager.image().block(21), &original);
}

#[test]
fn pager_plans_mapped_write_batch_without_duplicate_metadata_anchor() {
    let image = mock_image();
    let original_20 = *image.block(20);
    let original_21 = *image.block(21);
    let mut pager = Ext4Pager::open(image).unwrap();
    let pages = [filled_page(0xA0), filled_page(0xA1)];

    let plan = pager
        .plan_write_pages(InodeNo::new(12), 0, &pages, FsyncStamp::new(18))
        .unwrap();

    assert_eq!(plan.origin, MutationOrigin::FlushPage);
    assert_eq!(plan.metadata.len(), 1);
    assert_eq!(plan.metadata[0].home, 4);
    assert_eq!(plan.data.len(), 2);
    assert_eq!(plan.data[0].logical_page, 0);
    assert_eq!(plan.data[0].physical_block, 20);
    assert_eq!(plan.data[0].bytes, pages[0]);
    assert_eq!(plan.data[1].logical_page, 1);
    assert_eq!(plan.data[1].physical_block, 21);
    assert_eq!(plan.data[1].bytes, pages[1]);
    assert_eq!(pager.image().block(20), &original_20);
    assert_eq!(pager.image().block(21), &original_21);
}

#[test]
fn pager_rejects_write_batch_that_crosses_a_hole() {
    let image = mock_image();
    let mut pager = Ext4Pager::open(image).unwrap();
    let pages = [filled_page(0xB0), filled_page(0xB1)];

    assert_eq!(
        pager.plan_write_pages(InodeNo::new(12), 1, &pages, FsyncStamp::new(19)),
        Err(Ext4FormatError::Unsupported)
    );
}

#[test]
fn pager_plans_hole_write_with_bitmap_and_inode_after_images() {
    let mut image = mock_image();
    mark_block_bitmap_used(&mut image, 48);
    let bitmap_before = *image.block(2);
    let group_desc_before = *image.block(1);
    let superblock_before = *image.block(0);
    let inode_table_before = *image.block(4);
    let data_before = *image.block(48);
    let mut pager = Ext4Pager::open(image).unwrap();
    let page = filled_page(0xE4);

    let plan = pager
        .plan_write_page(InodeNo::new(12), 4, &page, FsyncStamp::new(11))
        .unwrap();

    assert_eq!(plan.data.len(), 1);
    assert_eq!(plan.data[0].logical_page, 4);
    assert_eq!(plan.data[0].physical_block, 48);
    assert_eq!(plan.data[0].bytes, page);
    assert_eq!(plan.allocations.len(), 1);
    assert_eq!(plan.allocations[0].physical_block, 48);

    let bitmap = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::BlockBitmap)
        .unwrap();
    assert_eq!(bitmap.home, 2);
    assert!(BitmapView::new(&bitmap.after).is_set(48));

    let inode_table = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::InodeTable)
        .unwrap();
    assert_eq!(inode_table.home, 4);
    let inode = Inode::parse(&inode_table.after[11 * 256..12 * 256]).unwrap();
    assert_eq!(inode.size, 5 * BLOCK_SIZE as u64);
    assert_eq!(inode.blocks_512, 32);
    assert_eq!(
        inode.map_extent_block(4).unwrap(),
        tx_ext4_format::ondisk::BlockMapping::Data(48)
    );

    let group_desc = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::GroupDescriptor)
        .unwrap();
    assert_eq!(group_desc.home, 1);
    assert_eq!(
        GroupDesc::parse(&group_desc.after[..32])
            .unwrap()
            .free_blocks_count,
        GroupDesc::parse(&group_desc_before[..32])
            .unwrap()
            .free_blocks_count
            - 1
    );

    let superblock = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::Superblock)
        .unwrap();
    assert_eq!(superblock.home, 0);
    assert_eq!(
        Superblock::parse(&superblock.after[1024..2048])
            .unwrap()
            .free_blocks_count,
        Superblock::parse(&superblock_before[1024..2048])
            .unwrap()
            .free_blocks_count
            - 1
    );

    assert_eq!(pager.image().block(2), &bitmap_before);
    assert_eq!(pager.image().block(1), &group_desc_before);
    assert_eq!(pager.image().block(0), &superblock_before);
    assert_eq!(pager.image().block(4), &inode_table_before);
    assert_eq!(pager.image().block(48), &data_before);
}

#[test]
fn pager_plans_metadata_checksum_after_images_for_hole_write() {
    let mut image = mock_image();
    let mut superblock = Superblock::parse(&image.block(0)[1024..2048]).unwrap();
    superblock.feature_ro_compat |= Superblock::FEATURE_RO_COMPAT_METADATA_CSUM;
    superblock
        .encode(&mut image.block_mut(0)[1024..2048])
        .unwrap();
    let mut pager = Ext4Pager::open(image).unwrap();

    let plan = pager
        .plan_write_page(InodeNo::new(12), 4, &filled_page(0xD4), FsyncStamp::new(12))
        .unwrap();
    let group_desc = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::GroupDescriptor)
        .unwrap();
    let mut desc_bytes = group_desc.after[..32].to_vec();
    let stored_desc_checksum = u16::from_le_bytes(desc_bytes[30..32].try_into().unwrap());
    desc_bytes[30..32].fill(0);
    let seed = crc32c_append(0xFFFF_FFFF, &superblock.uuid);
    assert_eq!(
        stored_desc_checksum,
        group_desc_csum16(seed, 0, &desc_bytes)
    );

    let bitmap = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::BlockBitmap)
        .unwrap();
    let stored_bitmap_checksum = u16::from_le_bytes(group_desc.after[24..26].try_into().unwrap());
    assert_eq!(
        stored_bitmap_checksum,
        block_bitmap_csum32(seed, &bitmap.after, 64).unwrap() as u16
    );

    let inode_table = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::InodeTable)
        .unwrap();
    let inode_bytes = &inode_table.after[2816..3072];
    let stored_inode_checksum = u32::from(u16::from_le_bytes(
        inode_bytes[124..126].try_into().unwrap(),
    )) | (u32::from(u16::from_le_bytes(
        inode_bytes[130..132].try_into().unwrap(),
    )) << 16);
    assert_eq!(
        stored_inode_checksum,
        inode_csum32(
            seed,
            12,
            u32::from_le_bytes(inode_bytes[100..104].try_into().unwrap()),
            inode_bytes
        )
        .unwrap()
    );

    let superblock = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::Superblock)
        .unwrap();
    let bytes = &superblock.after[1024..2048];
    assert_eq!(
        u32::from_le_bytes(bytes[1020..1024].try_into().unwrap()),
        superblock_csum32(bytes).unwrap()
    );
}

#[test]
fn setattr_plan_preserves_unknown_inode_bytes_and_updates_checksum() {
    let mut image = mock_image();
    let mut superblock = Superblock::parse(&image.block(0)[1024..2048]).unwrap();
    superblock.feature_ro_compat |= Superblock::FEATURE_RO_COMPAT_METADATA_CSUM;
    superblock
        .encode(&mut image.block_mut(0)[1024..2048])
        .unwrap();
    let inode_offset = 11 * 256;
    image.block_mut(4)[inode_offset + 200] = 0xA5;
    let mut pager = Ext4Pager::open(image).unwrap();

    let plan = pager
        .plan_setattr_mode(InodeNo::new(12), 0o755, FsyncStamp::new(13))
        .unwrap();
    let inode_table = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::InodeTable)
        .unwrap();
    let inode_bytes = &inode_table.after[inode_offset..inode_offset + 256];

    assert_eq!(
        Inode::parse(inode_bytes).unwrap().mode,
        Inode::S_IFREG | 0o755
    );
    assert_eq!(inode_bytes[200], 0xA5);
    let stored_checksum = u32::from(u16::from_le_bytes(
        inode_bytes[124..126].try_into().unwrap(),
    )) | (u32::from(u16::from_le_bytes(
        inode_bytes[130..132].try_into().unwrap(),
    )) << 16);
    assert_eq!(
        stored_checksum,
        inode_csum32(
            crc32c_append(0xFFFF_FFFF, &superblock.uuid),
            12,
            u32::from_le_bytes(inode_bytes[100..104].try_into().unwrap()),
            inode_bytes
        )
        .unwrap()
    );
}

#[test]
fn setattr_plan_updates_owner_and_times_without_mutating_home_inode() {
    let image = mock_image();
    let before = *image.block(4);
    let mut pager = Ext4Pager::open(image).unwrap();

    let owner = pager
        .plan_setattr(
            InodeNo::new(12),
            SetAttr::Owner {
                uid: Some(0x1_0002),
                gid: Some(0x3_0004),
            },
            FsyncStamp::new(14),
        )
        .unwrap();
    let owner_inode = owner
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::InodeTable)
        .unwrap();
    let owner_inode = Inode::parse(&owner_inode.after[11 * 256..12 * 256]).unwrap();
    assert_eq!(owner_inode.uid, 0x1_0002);
    assert_eq!(owner_inode.gid, 0x3_0004);
    assert!(owner_inode.is_file());

    let times = pager
        .plan_setattr(
            InodeNo::new(12),
            SetAttr::Times {
                atime_ns: Some(5_000_000_123),
                mtime_ns: Some(8_000_000_999),
                ctime_ns: 13_000_000_001,
            },
            FsyncStamp::new(15),
        )
        .unwrap();
    let times_inode = times
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::InodeTable)
        .unwrap();
    let times_inode = Inode::parse(&times_inode.after[11 * 256..12 * 256]).unwrap();
    assert_eq!(
        (times_inode.atime, times_inode.mtime, times_inode.ctime),
        (5, 8, 13)
    );
    assert_eq!(pager.image().block(4), &before);
}

#[test]
fn truncate_plan_updates_size_without_mutating_home_inode() {
    let mut image = mock_image();
    let mut superblock = Superblock::parse(&image.block(0)[1024..2048]).unwrap();
    superblock.feature_ro_compat |= Superblock::FEATURE_RO_COMPAT_METADATA_CSUM;
    superblock
        .encode(&mut image.block_mut(0)[1024..2048])
        .unwrap();
    let before = *image.block(4);
    let mut pager = Ext4Pager::open(image).unwrap();

    let plan = pager
        .plan_truncate_size(
            InodeNo::new(12),
            4 * BLOCK_SIZE as u64 - 17,
            FsyncStamp::new(16),
        )
        .unwrap();
    assert_eq!(
        plan.origin,
        tx_ext4_format::mutation::MutationOrigin::Truncate
    );
    assert_eq!(plan.object, 12);
    assert!(plan.data.is_empty());
    assert!(plan.allocations.is_empty());
    assert!(plan.revokes.is_empty());
    assert!(plan.deferred_frees.is_empty());
    assert_eq!(plan.metadata.len(), 1);

    let inode_table = &plan.metadata[0];
    assert_eq!(
        inode_table.role,
        tx_ext4_format::mutation::MetaRole::InodeTable
    );
    assert_eq!(inode_table.home, 4);
    let inode_bytes = &inode_table.after[11 * 256..12 * 256];
    let after_inode = Inode::parse(inode_bytes).unwrap();
    assert_eq!(after_inode.size, 4 * BLOCK_SIZE as u64 - 17);
    assert_eq!(pager.image().block(4), &before);

    let stored_checksum = u32::from(u16::from_le_bytes(
        inode_bytes[124..126].try_into().unwrap(),
    )) | (u32::from(u16::from_le_bytes(
        inode_bytes[130..132].try_into().unwrap(),
    )) << 16);
    assert_eq!(
        stored_checksum,
        inode_csum32(
            crc32c_append(0xFFFF_FFFF, &superblock.uuid),
            12,
            u32::from_le_bytes(inode_bytes[100..104].try_into().unwrap()),
            inode_bytes
        )
        .unwrap()
    );
}

#[test]
fn truncate_plan_releases_complete_tail_blocks_with_revoke_claims() {
    let mut image = mock_image();
    BitmapMut::new(image.block_mut(2)).set(20).unwrap();
    BitmapMut::new(image.block_mut(2)).set(21).unwrap();
    BitmapMut::new(image.block_mut(2)).set(30).unwrap();
    let before = *image.block(4);
    let mut pager = Ext4Pager::open(image).unwrap();

    let plan = pager
        .plan_truncate_size(InodeNo::new(12), BLOCK_SIZE as u64, FsyncStamp::new(17))
        .unwrap();

    assert_eq!(plan.origin, MutationOrigin::Truncate);
    assert!(plan.data.is_empty());
    assert!(plan.allocations.is_empty());
    assert_eq!(
        plan.revokes,
        vec![
            tx_ext4_format::mutation::RevokeRecord { physical_block: 21 },
            tx_ext4_format::mutation::RevokeRecord { physical_block: 30 }
        ]
    );
    assert_eq!(
        plan.deferred_frees,
        vec![
            tx_ext4_format::mutation::DeferredFreeClaim { physical_block: 21 },
            tx_ext4_format::mutation::DeferredFreeClaim { physical_block: 30 }
        ]
    );
    assert_eq!(plan.metadata.len(), 4);

    let bitmap = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::BlockBitmap)
        .unwrap();
    assert_eq!(bitmap.home, 2);
    assert!(BitmapView::new(&bitmap.after).is_set(20));
    assert!(!BitmapView::new(&bitmap.after).is_set(21));
    assert!(!BitmapView::new(&bitmap.after).is_set(30));

    let group_desc = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::GroupDescriptor)
        .unwrap();
    let parsed_group = GroupDesc::parse(&group_desc.after[..64]).unwrap();
    assert_eq!(parsed_group.free_blocks_count, 34);

    let superblock = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::Superblock)
        .unwrap();
    let parsed_superblock = Superblock::parse(&superblock.after[1024..2048]).unwrap();
    assert_eq!(parsed_superblock.free_blocks_count, 34);

    let inode_table = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::InodeTable)
        .unwrap();
    let inode_bytes = &inode_table.after[11 * 256..12 * 256];
    let after_inode = Inode::parse(inode_bytes).unwrap();
    assert_eq!(after_inode.size, BLOCK_SIZE as u64);
    assert_eq!(after_inode.blocks_512, 8);
    assert_eq!(
        Extent::parse_all(after_inode.extent_root_bytes()).unwrap(),
        vec![Extent {
            logical_block: 0,
            len: 1,
            physical_start: 20
        }]
    );
    assert_eq!(pager.image().block(4), &before);
}

#[test]
fn truncate_plan_releases_inline_tail_blocks_across_groups() {
    let mut image = mock_image();
    let mut superblock = Superblock::parse(&image.block(0)[1024..2048]).unwrap();
    superblock.blocks_count = 128;
    superblock.blocks_per_group = 64;
    superblock.desc_size = 64;
    superblock.feature_incompat |= Superblock::FEATURE_INCOMPAT_64BIT;
    superblock
        .encode(&mut image.block_mut(0)[1024..2048])
        .unwrap();
    GroupDesc {
        block_bitmap: 66,
        inode_bitmap: 67,
        inode_table: 70,
        free_blocks_count: 10,
        free_inodes_count: 64,
        ..GroupDesc::default()
    }
    .encode(&mut image.block_mut(1)[64..128])
    .unwrap();
    BitmapMut::new(image.block_mut(2)).set(20).unwrap();
    BitmapMut::new(image.block_mut(2)).set(21).unwrap();
    BitmapMut::new(image.block_mut(66)).set(16).unwrap();

    let mut inode = Inode::default();
    inode.mode = Inode::S_IFREG | 0o600;
    inode.size = 3 * BLOCK_SIZE as u64;
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
                logical_block: 2,
                len: 1,
                physical_start: 80,
            },
        ])
        .unwrap();
    write_inode(&mut image, 12, &inode);
    let source_before = [
        *image.block(0),
        *image.block(1),
        *image.block(2),
        *image.block(4),
        *image.block(66),
    ];
    let mut pager = Ext4Pager::open(image).unwrap();

    let plan = pager
        .plan_truncate_size(InodeNo::new(12), BLOCK_SIZE as u64, FsyncStamp::new(18))
        .unwrap();

    assert_eq!(
        plan.revokes
            .iter()
            .map(|claim| claim.physical_block)
            .collect::<Vec<_>>(),
        vec![21, 80]
    );
    assert_eq!(
        plan.deferred_frees
            .iter()
            .map(|claim| claim.physical_block)
            .collect::<Vec<_>>(),
        vec![21, 80]
    );
    let bitmaps: Vec<_> = plan
        .metadata
        .iter()
        .filter(|block| block.role == tx_ext4_format::mutation::MetaRole::BlockBitmap)
        .collect();
    assert_eq!(bitmaps.len(), 2);
    assert!(
        !BitmapView::new(&bitmaps.iter().find(|block| block.home == 2).unwrap().after).is_set(21)
    );
    assert!(
        !BitmapView::new(&bitmaps.iter().find(|block| block.home == 66).unwrap().after).is_set(16)
    );
    let groups: Vec<_> = plan
        .metadata
        .iter()
        .filter(|block| block.role == tx_ext4_format::mutation::MetaRole::GroupDescriptor)
        .collect();
    assert_eq!(groups.len(), 1);
    assert_eq!(
        GroupDesc::parse_sized(&groups[0].after[..64], 64)
            .unwrap()
            .free_blocks_count,
        GroupDesc::parse_sized(&source_before[1][..64], 64)
            .unwrap()
            .free_blocks_count
            + 1
    );
    assert_eq!(
        GroupDesc::parse_sized(&groups[0].after[64..128], 64)
            .unwrap()
            .free_blocks_count,
        11
    );
    let superblock = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::Superblock)
        .unwrap();
    assert_eq!(
        Superblock::parse(&superblock.after[1024..2048])
            .unwrap()
            .free_blocks_count,
        Superblock::parse(&source_before[0][1024..2048])
            .unwrap()
            .free_blocks_count
            + 2
    );
    for (home, before) in [0, 1, 2, 4, 66].into_iter().zip(source_before) {
        assert_eq!(pager.image().block(home), &before);
    }
}

#[test]
fn pager_spills_inline_root_with_claims_from_two_groups() {
    let mut image = mock_image();
    let mut superblock = Superblock::parse(&image.block(0)[1024..2048]).unwrap();
    superblock.blocks_count = 128;
    superblock.free_blocks_count = 2;
    superblock.blocks_per_group = 64;
    superblock.desc_size = 64;
    superblock.feature_incompat |= Superblock::FEATURE_INCOMPAT_64BIT;
    superblock
        .encode(&mut image.block_mut(0)[1024..2048])
        .unwrap();

    let mut group0 = GroupDesc::parse_sized(&image.block(1)[..64], 64).unwrap();
    group0.free_blocks_count = 1;
    group0.encode(&mut image.block_mut(1)[..64]).unwrap();
    GroupDesc {
        block_bitmap: 66,
        inode_bitmap: 67,
        inode_table: 70,
        free_blocks_count: 1,
        free_inodes_count: 64,
        ..GroupDesc::default()
    }
    .encode(&mut image.block_mut(1)[64..128])
    .unwrap();
    for bitmap_home in [2, 66] {
        let mut bitmap = BitmapMut::new(image.block_mut(bitmap_home));
        for bit in 0..63 {
            bitmap.set(bit).unwrap();
        }
    }

    let mut inode = Inode::default();
    inode.mode = Inode::S_IFREG | 0o644;
    inode.size = 7 * BLOCK_SIZE as u64;
    inode.blocks_512 = 32;
    inode.links_count = 1;
    inode.flags = Inode::EXTENTS_FL;
    inode
        .set_extent_root(&[
            Extent {
                logical_block: 0,
                len: 1,
                physical_start: 20,
            },
            Extent {
                logical_block: 2,
                len: 1,
                physical_start: 22,
            },
            Extent {
                logical_block: 4,
                len: 1,
                physical_start: 24,
            },
            Extent {
                logical_block: 6,
                len: 1,
                physical_start: 26,
            },
        ])
        .unwrap();
    write_inode(&mut image, 12, &inode);
    let source_before = [
        *image.block(0),
        *image.block(1),
        *image.block(2),
        *image.block(4),
        *image.block(63),
        *image.block(66),
        *image.block(127),
    ];
    let mut pager = Ext4Pager::open(image).unwrap();

    let plan = pager
        .plan_write_page(InodeNo::new(12), 8, &filled_page(0xE9), FsyncStamp::new(49))
        .unwrap();

    assert_eq!(
        plan.allocations
            .iter()
            .map(|claim| claim.physical_block)
            .collect::<Vec<_>>(),
        vec![63, 127]
    );
    let bitmaps: Vec<_> = plan
        .metadata
        .iter()
        .filter(|block| block.role == tx_ext4_format::mutation::MetaRole::BlockBitmap)
        .collect();
    assert_eq!(bitmaps.len(), 2);
    assert!(
        BitmapView::new(&bitmaps.iter().find(|block| block.home == 2).unwrap().after).is_set(63)
    );
    assert!(
        BitmapView::new(&bitmaps.iter().find(|block| block.home == 66).unwrap().after).is_set(63)
    );
    let group_after: Vec<_> = plan
        .metadata
        .iter()
        .filter(|block| block.role == tx_ext4_format::mutation::MetaRole::GroupDescriptor)
        .collect();
    assert_eq!(group_after.len(), 1);
    assert_eq!(group_after[0].home, 1);
    assert_eq!(
        GroupDesc::parse_sized(&group_after[0].after[..64], 64)
            .unwrap()
            .free_blocks_count,
        0
    );
    assert_eq!(
        GroupDesc::parse_sized(&group_after[0].after[64..128], 64)
            .unwrap()
            .free_blocks_count,
        0
    );
    assert_eq!(
        plan.metadata
            .iter()
            .filter(|block| block.role == tx_ext4_format::mutation::MetaRole::Superblock)
            .count(),
        1
    );
    let extent_node = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::ExtentNode)
        .unwrap();
    assert_eq!(extent_node.home, 127);
    for (home, before) in [0, 1, 2, 4, 63, 66, 127].into_iter().zip(source_before) {
        assert_eq!(pager.image().block(home), &before);
    }
}

#[test]
fn pager_rejects_metadata_alias_and_exhausted_block_plans_without_home_write() {
    let mut alias_image = mock_image();
    mark_block_bitmap_used(&mut alias_image, 64);
    BitmapMut::new(alias_image.block_mut(2)).clear(2).unwrap();
    let alias_before = [
        *alias_image.block(0),
        *alias_image.block(1),
        *alias_image.block(2),
        *alias_image.block(4),
    ];
    let mut alias_pager = Ext4Pager::open(alias_image).unwrap();
    assert_eq!(
        alias_pager
            .plan_write_page(InodeNo::new(12), 2, &filled_page(0xCD), FsyncStamp::new(50))
            .unwrap_err(),
        Ext4FormatError::OutOfBounds
    );
    for (home, before) in [0, 1, 2, 4].into_iter().zip(alias_before) {
        assert_eq!(alias_pager.image().block(home), &before);
    }

    let mut exhausted_image = mock_image();
    mark_block_bitmap_used(&mut exhausted_image, 64);
    let exhausted_before = [
        *exhausted_image.block(0),
        *exhausted_image.block(1),
        *exhausted_image.block(2),
        *exhausted_image.block(4),
    ];
    let mut exhausted_pager = Ext4Pager::open(exhausted_image).unwrap();
    assert_eq!(
        exhausted_pager
            .plan_write_page(InodeNo::new(12), 2, &filled_page(0xCE), FsyncStamp::new(51))
            .unwrap_err(),
        Ext4FormatError::OutOfBounds
    );
    for (home, before) in [0, 1, 2, 4].into_iter().zip(exhausted_before) {
        assert_eq!(exhausted_pager.image().block(home), &before);
    }
}

#[test]
fn pager_rejects_incomplete_group_allocation_without_home_write() {
    let mut image = mock_image();
    let mut superblock = Superblock::parse(&image.block(0)[1024..2048]).unwrap();
    superblock.blocks_count = 128;
    superblock.blocks_per_group = 64;
    superblock.desc_size = 64;
    superblock.feature_incompat |= Superblock::FEATURE_INCOMPAT_64BIT;
    superblock
        .encode(&mut image.block_mut(0)[1024..2048])
        .unwrap();
    mark_block_bitmap_used(&mut image, 64);
    let source_before = [
        *image.block(0),
        *image.block(1),
        *image.block(2),
        *image.block(4),
    ];
    let mut pager = Ext4Pager::open(image).unwrap();

    assert_eq!(
        pager
            .plan_write_page(InodeNo::new(12), 2, &filled_page(0xCF), FsyncStamp::new(52))
            .unwrap_err(),
        Ext4FormatError::Corrupt
    );
    for (home, before) in [0, 1, 2, 4].into_iter().zip(source_before) {
        assert_eq!(pager.image().block(home), &before);
    }
}

#[test]
fn pager_initializes_one_inline_unwritten_block_without_new_claims() {
    let mut image = mock_image();
    let mut inode = Inode::default();
    inode.mode = Inode::S_IFREG | 0o600;
    inode.size = 3 * BLOCK_SIZE as u64;
    inode.blocks_512 = 24;
    inode.links_count = 1;
    inode.flags = Inode::EXTENTS_FL;
    inode
        .set_extent_root(&[Extent {
            logical_block: 0,
            len: Extent::UNINITIALIZED_MASK | 3,
            physical_start: 20,
        }])
        .unwrap();
    write_inode(&mut image, 12, &inode);
    let inode_table_before = *image.block(4);
    let data_before = *image.block(21);
    let mut pager = Ext4Pager::open(image).unwrap();

    let plan = pager
        .plan_write_page(InodeNo::new(12), 1, &filled_page(0xD0), FsyncStamp::new(53))
        .unwrap();

    assert!(plan.allocations.is_empty());
    assert_eq!(plan.data.len(), 1);
    assert_eq!(plan.data[0].physical_block, 21);
    let inode_table = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::InodeTable)
        .unwrap();
    let inode_after = Inode::parse(&inode_table.after[11 * 256..12 * 256]).unwrap();
    assert_eq!(
        Extent::parse_all(inode_after.extent_root_bytes()).unwrap(),
        vec![
            Extent {
                logical_block: 0,
                len: Extent::UNINITIALIZED_MASK | 1,
                physical_start: 20,
            },
            Extent {
                logical_block: 1,
                len: 1,
                physical_start: 21,
            },
            Extent {
                logical_block: 2,
                len: Extent::UNINITIALIZED_MASK | 1,
                physical_start: 22,
            },
        ]
    );
    assert_eq!(pager.image().block(4), &inode_table_before);
    assert_eq!(pager.image().block(21), &data_before);
}

#[test]
fn pager_initializes_one_depth_one_unwritten_block_without_new_claims() {
    let mut image = mock_image();
    let mut inode = Inode::default();
    inode.mode = Inode::S_IFREG | 0o600;
    inode.size = 5 * BLOCK_SIZE as u64;
    inode.blocks_512 = 24;
    inode.links_count = 1;
    inode.flags = Inode::EXTENTS_FL;
    inode
        .set_extent_index_root(
            &[ExtentIdx {
                logical_block: 4,
                child: 60,
            }],
            1,
        )
        .unwrap();
    write_inode(&mut image, 12, &inode);
    ExtentNode::encode_leaf(
        &[Extent {
            logical_block: 4,
            len: Extent::UNINITIALIZED_MASK | 3,
            physical_start: 40,
        }],
        image.block_mut(60),
    )
    .unwrap();
    let inode_table_before = *image.block(4);
    let leaf_before = *image.block(60);
    let data_before = *image.block(41);
    let mut pager = Ext4Pager::open(image).unwrap();

    let plan = pager
        .plan_write_page(InodeNo::new(12), 5, &filled_page(0xD1), FsyncStamp::new(54))
        .unwrap();

    assert!(plan.allocations.is_empty());
    assert_eq!(plan.data.len(), 1);
    assert_eq!(plan.data[0].physical_block, 41);
    let inode_table = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::InodeTable)
        .unwrap();
    let inode_after = Inode::parse(&inode_table.after[11 * 256..12 * 256]).unwrap();
    assert_eq!(inode_after.size, 6 * BLOCK_SIZE as u64);
    assert_eq!(inode_after.blocks_512, 24);
    let leaf = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::ExtentNode)
        .unwrap();
    assert_eq!(leaf.home, 60);
    assert_eq!(
        Extent::parse_all(&leaf.after).unwrap(),
        vec![
            Extent {
                logical_block: 4,
                len: Extent::UNINITIALIZED_MASK | 1,
                physical_start: 40,
            },
            Extent {
                logical_block: 5,
                len: 1,
                physical_start: 41,
            },
            Extent {
                logical_block: 6,
                len: Extent::UNINITIALIZED_MASK | 1,
                physical_start: 42,
            },
        ]
    );
    assert_eq!(pager.image().block(4), &inode_table_before);
    assert_eq!(pager.image().block(60), &leaf_before);
    assert_eq!(pager.image().block(41), &data_before);
}

#[test]
fn pager_keeps_depth_two_initialized_writeback_on_the_existing_mapping_path() {
    let mut image = mock_image();
    let mut inode = Inode::default();
    inode.mode = Inode::S_IFREG | 0o600;
    inode.size = 7 * BLOCK_SIZE as u64;
    inode.blocks_512 = 24;
    inode.links_count = 1;
    inode.flags = Inode::EXTENTS_FL;
    inode
        .set_extent_index_root(
            &[ExtentIdx {
                logical_block: 4,
                child: 60,
            }],
            2,
        )
        .unwrap();
    write_inode(&mut image, 12, &inode);
    ExtentNode::encode_index(
        1,
        &[ExtentIdx {
            logical_block: 4,
            child: 61,
        }],
        image.block_mut(60),
    )
    .unwrap();
    ExtentNode::encode_leaf(
        &[Extent {
            logical_block: 4,
            len: 3,
            physical_start: 40,
        }],
        image.block_mut(61),
    )
    .unwrap();
    let inode_table_before = *image.block(4);
    let parent_before = *image.block(60);
    let leaf_before = *image.block(61);
    let data_before = *image.block(41);
    let mut pager = Ext4Pager::open(image).unwrap();

    let plan = pager
        .plan_write_page(InodeNo::new(12), 5, &filled_page(0xD2), FsyncStamp::new(55))
        .unwrap();

    assert!(plan.allocations.is_empty());
    assert_eq!(plan.data[0].physical_block, 41);
    assert_eq!(
        plan.metadata
            .iter()
            .map(|block| block.role)
            .collect::<Vec<_>>(),
        vec![tx_ext4_format::mutation::MetaRole::InodeTable]
    );
    assert_eq!(pager.image().block(4), &inode_table_before);
    assert_eq!(pager.image().block(60), &parent_before);
    assert_eq!(pager.image().block(61), &leaf_before);
    assert_eq!(pager.image().block(41), &data_before);
}

#[test]
fn pager_splits_full_depth_one_unwritten_leaf_without_data_claim() {
    let mut image = mock_image();
    mark_block_bitmap_used(&mut image, 32);
    let mut extents: Vec<_> = (0..339u32)
        .map(|index| Extent {
            logical_block: index * 4,
            len: 1,
            physical_start: 1000 + u64::from(index) * 4,
        })
        .collect();
    extents.push(Extent {
        logical_block: 1356,
        len: Extent::UNINITIALIZED_MASK | 3,
        physical_start: 2356,
    });
    ExtentNode::encode_leaf(&extents, image.block_mut(31)).unwrap();
    let mut inode = Inode::default();
    inode.mode = Inode::S_IFREG | 0o644;
    inode.size = 1357 * BLOCK_SIZE as u64;
    inode.blocks_512 = 8;
    inode.links_count = 1;
    inode.flags = Inode::EXTENTS_FL;
    inode
        .set_extent_index_root(
            &[ExtentIdx {
                logical_block: 0,
                child: 31,
            }],
            1,
        )
        .unwrap();
    write_inode(&mut image, 12, &inode);
    let inode_table_before = *image.block(4);
    let leaf_before = *image.block(31);
    let bitmap_before = *image.block(2);
    let mut pager = Ext4Pager::open(image).unwrap();

    let plan = pager
        .plan_write_page(
            InodeNo::new(12),
            1357,
            &filled_page(0xD3),
            FsyncStamp::new(56),
        )
        .unwrap();

    assert_eq!(plan.data[0].physical_block, 2357);
    assert_eq!(plan.allocations.len(), 1);
    let right_home = plan.allocations[0].physical_block;
    assert_ne!(right_home, plan.data[0].physical_block);
    let extent_nodes: Vec<_> = plan
        .metadata
        .iter()
        .filter(|block| block.role == tx_ext4_format::mutation::MetaRole::ExtentNode)
        .collect();
    assert_eq!(extent_nodes.len(), 2);
    assert_eq!(extent_nodes[0].home, 31);
    assert_eq!(extent_nodes[1].home, right_home);
    assert_eq!(
        Extent::parse_all(&extent_nodes[0].after).unwrap().len()
            + Extent::parse_all(&extent_nodes[1].after).unwrap().len(),
        342
    );
    let inode_after = Inode::parse(
        &plan
            .metadata
            .iter()
            .find(|block| block.role == tx_ext4_format::mutation::MetaRole::InodeTable)
            .unwrap()
            .after[11 * 256..12 * 256],
    )
    .unwrap();
    assert_eq!(inode_after.blocks_512, 16);
    assert_eq!(inode_after.size, 1358 * BLOCK_SIZE as u64);
    let root = match ExtentNode::parse(inode_after.extent_root_bytes()).unwrap() {
        ExtentNode::Index(indexes) => indexes,
        ExtentNode::Leaf(_) => panic!("split leaf must retain an indexed root"),
    };
    assert_eq!(root.len(), 2);
    assert_eq!(root[0].child, 31);
    assert_eq!(root[1].child, right_home);
    assert_eq!(root[1].logical_block, 684);
    assert_eq!(pager.image().block(4), &inode_table_before);
    assert_eq!(pager.image().block(31), &leaf_before);
    assert_eq!(pager.image().block(2), &bitmap_before);
}

#[test]
fn pager_grows_full_depth_one_root_for_unwritten_conversion_without_data_claim() {
    let mut image = mock_image();
    mark_block_bitmap_used(&mut image, 35);
    let mut root_indexes = Vec::new();
    for child in 0..4u64 {
        let logical_base = child as u32 * 2000;
        let mut extents: Vec<_> = (0..339u32)
            .map(|index| Extent {
                logical_block: logical_base + index * 4,
                len: 1,
                physical_start: 10_000 + child * 2_000 + u64::from(index) * 4,
            })
            .collect();
        extents.push(Extent {
            logical_block: logical_base + 1356,
            len: if child == 3 {
                Extent::UNINITIALIZED_MASK | 3
            } else {
                1
            },
            physical_start: 10_000 + child * 2_000 + 1356,
        });
        ExtentNode::encode_leaf(&extents, image.block_mut(31 + child)).unwrap();
        root_indexes.push(ExtentIdx {
            logical_block: logical_base,
            child: 31 + child,
        });
    }
    let mut inode = Inode::default();
    inode.mode = Inode::S_IFREG | 0o644;
    inode.size = (6000 + 1357) * BLOCK_SIZE as u64;
    inode.blocks_512 = 32;
    inode.links_count = 1;
    inode.flags = Inode::EXTENTS_FL;
    inode.set_extent_index_root(&root_indexes, 1).unwrap();
    write_inode(&mut image, 12, &inode);
    let inode_table_before = *image.block(4);
    let leaf_before = *image.block(34);
    let bitmap_before = *image.block(2);
    let mut pager = Ext4Pager::open(image).unwrap();

    let plan = pager
        .plan_write_page(
            InodeNo::new(12),
            7357,
            &filled_page(0xD4),
            FsyncStamp::new(57),
        )
        .unwrap();

    assert_eq!(plan.data[0].physical_block, 17_357);
    assert_eq!(plan.allocations.len(), 3);
    assert!(
        plan.allocations
            .iter()
            .all(|claim| claim.physical_block != plan.data[0].physical_block)
    );
    assert_eq!(
        plan.metadata
            .iter()
            .filter(|block| block.role == tx_ext4_format::mutation::MetaRole::ExtentNode)
            .count(),
        4
    );
    let inode_after = Inode::parse(
        &plan
            .metadata
            .iter()
            .find(|block| block.role == tx_ext4_format::mutation::MetaRole::InodeTable)
            .unwrap()
            .after[11 * 256..12 * 256],
    )
    .unwrap();
    assert_eq!(inode_after.blocks_512, 56);
    assert_eq!(inode_after.size, 7358 * BLOCK_SIZE as u64);
    assert_eq!(
        ExtentHeader::parse(inode_after.extent_root_bytes())
            .unwrap()
            .depth,
        2
    );
    let root = match ExtentNode::parse(inode_after.extent_root_bytes()).unwrap() {
        ExtentNode::Index(indexes) => indexes,
        ExtentNode::Leaf(_) => panic!("root growth must keep an indexed root"),
    };
    assert_eq!(root.len(), 2);
    assert_eq!(root[0].child, plan.allocations[1].physical_block);
    assert_eq!(root[1].child, plan.allocations[2].physical_block);
    assert_eq!(pager.image().block(4), &inode_table_before);
    assert_eq!(pager.image().block(34), &leaf_before);
    assert_eq!(pager.image().block(2), &bitmap_before);
}

#[test]
fn pager_splits_full_depth_two_unwritten_leaf_into_nonfull_parent_without_data_claim() {
    let mut image = mock_image();
    mark_block_bitmap_used(&mut image, 62);
    let mut extents: Vec<_> = (0..339u32)
        .map(|index| Extent {
            logical_block: index * 4,
            len: 1,
            physical_start: 1000 + u64::from(index) * 4,
        })
        .collect();
    extents.push(Extent {
        logical_block: 1356,
        len: Extent::UNINITIALIZED_MASK | 3,
        physical_start: 2356,
    });
    ExtentNode::encode_leaf(&extents, image.block_mut(61)).unwrap();
    ExtentNode::encode_index(
        1,
        &[ExtentIdx {
            logical_block: 0,
            child: 61,
        }],
        image.block_mut(60),
    )
    .unwrap();
    let mut inode = Inode::default();
    inode.mode = Inode::S_IFREG | 0o644;
    inode.size = 1357 * BLOCK_SIZE as u64;
    inode.blocks_512 = 16;
    inode.links_count = 1;
    inode.flags = Inode::EXTENTS_FL;
    inode
        .set_extent_index_root(
            &[ExtentIdx {
                logical_block: 0,
                child: 60,
            }],
            2,
        )
        .unwrap();
    write_inode(&mut image, 12, &inode);
    let inode_table_before = *image.block(4);
    let parent_before = *image.block(60);
    let leaf_before = *image.block(61);
    let bitmap_before = *image.block(2);
    let mut pager = Ext4Pager::open(image).unwrap();

    let plan = pager
        .plan_write_page(
            InodeNo::new(12),
            1357,
            &filled_page(0xD5),
            FsyncStamp::new(58),
        )
        .unwrap();

    assert_eq!(plan.data[0].physical_block, 2357);
    assert_eq!(plan.allocations.len(), 1);
    let right_home = plan.allocations[0].physical_block;
    let parent = plan.metadata.iter().find(|block| block.home == 60).unwrap();
    let parent_indexes = match ExtentNode::parse(&parent.after).unwrap() {
        ExtentNode::Index(indexes) => indexes,
        ExtentNode::Leaf(_) => panic!("parent must remain indexed"),
    };
    assert_eq!(parent_indexes.len(), 2);
    assert_eq!(parent_indexes[0].child, 61);
    assert_eq!(parent_indexes[1].child, right_home);
    assert_eq!(parent_indexes[1].logical_block, 684);
    let inode_after = Inode::parse(
        &plan
            .metadata
            .iter()
            .find(|block| block.role == tx_ext4_format::mutation::MetaRole::InodeTable)
            .unwrap()
            .after[11 * 256..12 * 256],
    )
    .unwrap();
    assert_eq!(inode_after.blocks_512, 24);
    assert_eq!(inode_after.size, 1358 * BLOCK_SIZE as u64);
    assert_eq!(
        ExtentHeader::parse(inode_after.extent_root_bytes())
            .unwrap()
            .depth,
        2
    );
    assert_eq!(pager.image().block(4), &inode_table_before);
    assert_eq!(pager.image().block(60), &parent_before);
    assert_eq!(pager.image().block(61), &leaf_before);
    assert_eq!(pager.image().block(2), &bitmap_before);
}

#[test]
fn pager_carries_full_depth_two_parent_for_unwritten_leaf_without_data_claim() {
    let mut image = mock_image();
    mark_block_bitmap_used(&mut image, 62);
    let mut extents: Vec<_> = (0..339u32)
        .map(|index| Extent {
            logical_block: index * 4,
            len: 1,
            physical_start: 1000 + u64::from(index) * 4,
        })
        .collect();
    extents.push(Extent {
        logical_block: 1356,
        len: Extent::UNINITIALIZED_MASK | 3,
        physical_start: 2356,
    });
    ExtentNode::encode_leaf(&extents, image.block_mut(61)).unwrap();
    let parent_indexes: Vec<_> = (0..340u32)
        .map(|index| ExtentIdx {
            logical_block: index * 2000,
            child: 61 + u64::from(index),
        })
        .collect();
    ExtentNode::encode_index(1, &parent_indexes, image.block_mut(60)).unwrap();
    let mut inode = Inode::default();
    inode.mode = Inode::S_IFREG | 0o644;
    inode.size = 1357 * BLOCK_SIZE as u64;
    inode.blocks_512 = 16;
    inode.links_count = 1;
    inode.flags = Inode::EXTENTS_FL;
    inode
        .set_extent_index_root(
            &[ExtentIdx {
                logical_block: 0,
                child: 60,
            }],
            2,
        )
        .unwrap();
    write_inode(&mut image, 12, &inode);
    let inode_table_before = *image.block(4);
    let parent_before = *image.block(60);
    let leaf_before = *image.block(61);
    let bitmap_before = *image.block(2);
    let mut pager = Ext4Pager::open(image).unwrap();

    let plan = pager
        .plan_write_page(
            InodeNo::new(12),
            1357,
            &filled_page(0xD6),
            FsyncStamp::new(59),
        )
        .unwrap();

    assert_eq!(plan.data[0].physical_block, 2357);
    assert_eq!(plan.allocations.len(), 2);
    let right_parent = plan.allocations[1].physical_block;
    let parent_left = plan.metadata.iter().find(|block| block.home == 60).unwrap();
    let parent_right = plan
        .metadata
        .iter()
        .find(|block| block.home == right_parent)
        .unwrap();
    let left_indexes = match ExtentNode::parse(&parent_left.after).unwrap() {
        ExtentNode::Index(indexes) => indexes,
        ExtentNode::Leaf(_) => panic!("left parent must remain indexed"),
    };
    let right_indexes = match ExtentNode::parse(&parent_right.after).unwrap() {
        ExtentNode::Index(indexes) => indexes,
        ExtentNode::Leaf(_) => panic!("right parent must remain indexed"),
    };
    assert_eq!(left_indexes.len() + right_indexes.len(), 341);
    let inode_after = Inode::parse(
        &plan
            .metadata
            .iter()
            .find(|block| block.role == tx_ext4_format::mutation::MetaRole::InodeTable)
            .unwrap()
            .after[11 * 256..12 * 256],
    )
    .unwrap();
    assert_eq!(inode_after.blocks_512, 32);
    let root = match ExtentNode::parse(inode_after.extent_root_bytes()).unwrap() {
        ExtentNode::Index(indexes) => indexes,
        ExtentNode::Leaf(_) => panic!("root must remain indexed"),
    };
    assert_eq!(root.len(), 2);
    assert_eq!(root[0].child, 60);
    assert_eq!(root[1].child, right_parent);
    assert_eq!(root[1].logical_block, right_indexes[0].logical_block);
    assert_eq!(pager.image().block(4), &inode_table_before);
    assert_eq!(pager.image().block(60), &parent_before);
    assert_eq!(pager.image().block(61), &leaf_before);
    assert_eq!(pager.image().block(2), &bitmap_before);
}

#[test]
fn pager_grows_full_depth_two_root_for_unwritten_leaf_without_data_claim() {
    let mut image = mock_image();
    mark_block_bitmap_used(&mut image, 55);
    let mut extents: Vec<_> = (0..339u32)
        .map(|index| Extent {
            logical_block: index * 4,
            len: 1,
            physical_start: 1000 + u64::from(index) * 4,
        })
        .collect();
    extents.push(Extent {
        logical_block: 1356,
        len: Extent::UNINITIALIZED_MASK | 3,
        physical_start: 2356,
    });
    ExtentNode::encode_leaf(&extents, image.block_mut(51)).unwrap();
    let parent_indexes: Vec<_> = (0..340u32)
        .map(|index| ExtentIdx {
            logical_block: index * 2000,
            child: 51 + u64::from(index),
        })
        .collect();
    ExtentNode::encode_index(1, &parent_indexes, image.block_mut(50)).unwrap();
    let mut inode = Inode::default();
    inode.mode = Inode::S_IFREG | 0o644;
    inode.size = 1357 * BLOCK_SIZE as u64;
    inode.blocks_512 = 16;
    inode.links_count = 1;
    inode.flags = Inode::EXTENTS_FL;
    inode
        .set_extent_index_root(
            &[
                ExtentIdx {
                    logical_block: 0,
                    child: 50,
                },
                ExtentIdx {
                    logical_block: 10_000,
                    child: 52,
                },
                ExtentIdx {
                    logical_block: 20_000,
                    child: 53,
                },
                ExtentIdx {
                    logical_block: 30_000,
                    child: 54,
                },
            ],
            2,
        )
        .unwrap();
    write_inode(&mut image, 12, &inode);
    let inode_table_before = *image.block(4);
    let parent_before = *image.block(50);
    let leaf_before = *image.block(51);
    let bitmap_before = *image.block(2);
    let mut pager = Ext4Pager::open(image).unwrap();

    let plan = pager
        .plan_write_page(
            InodeNo::new(12),
            1357,
            &filled_page(0xD7),
            FsyncStamp::new(60),
        )
        .unwrap();

    assert_eq!(plan.data[0].physical_block, 2357);
    assert_eq!(plan.allocations.len(), 4);
    let left_root_home = plan.allocations[2].physical_block;
    let right_root_home = plan.allocations[3].physical_block;
    let inode_after = Inode::parse(
        &plan
            .metadata
            .iter()
            .find(|block| block.role == tx_ext4_format::mutation::MetaRole::InodeTable)
            .unwrap()
            .after[11 * 256..12 * 256],
    )
    .unwrap();
    assert_eq!(inode_after.blocks_512, 48);
    assert_eq!(
        ExtentHeader::parse(inode_after.extent_root_bytes())
            .unwrap()
            .depth,
        3
    );
    let root = match ExtentNode::parse(inode_after.extent_root_bytes()).unwrap() {
        ExtentNode::Index(indexes) => indexes,
        ExtentNode::Leaf(_) => panic!("root growth must keep an indexed root"),
    };
    assert_eq!(root.len(), 2);
    assert_eq!(root[0].child, left_root_home);
    assert_eq!(root[1].child, right_root_home);
    let left_root = plan
        .metadata
        .iter()
        .find(|block| block.home == left_root_home)
        .unwrap();
    let right_root = plan
        .metadata
        .iter()
        .find(|block| block.home == right_root_home)
        .unwrap();
    let left_indexes = match ExtentNode::parse(&left_root.after).unwrap() {
        ExtentNode::Index(indexes) => indexes,
        ExtentNode::Leaf(_) => panic!("left grown root must be indexed"),
    };
    let right_indexes = match ExtentNode::parse(&right_root.after).unwrap() {
        ExtentNode::Index(indexes) => indexes,
        ExtentNode::Leaf(_) => panic!("right grown root must be indexed"),
    };
    assert_eq!(left_indexes.len() + right_indexes.len(), 5);
    assert_eq!(left_indexes[0].child, 50);
    assert_eq!(right_indexes[0].logical_block, 10_000);
    assert_eq!(pager.image().block(4), &inode_table_before);
    assert_eq!(pager.image().block(50), &parent_before);
    assert_eq!(pager.image().block(51), &leaf_before);
    assert_eq!(pager.image().block(2), &bitmap_before);
}

#[test]
fn pager_converts_depth_three_unwritten_leaf_without_new_claims() {
    let mut image = mock_image();
    mark_block_bitmap_used(&mut image, 53);
    let leaf_before = {
        let leaf = image.block_mut(52);
        ExtentNode::encode_leaf(
            &[Extent {
                logical_block: 0,
                len: Extent::UNINITIALIZED_MASK | 3,
                physical_start: 1000,
            }],
            leaf,
        )
        .unwrap();
        *leaf
    };
    ExtentNode::encode_index(
        1,
        &[ExtentIdx {
            logical_block: 0,
            child: 52,
        }],
        image.block_mut(51),
    )
    .unwrap();
    ExtentNode::encode_index(
        2,
        &[ExtentIdx {
            logical_block: 0,
            child: 51,
        }],
        image.block_mut(50),
    )
    .unwrap();
    let mut inode = Inode::default();
    inode.mode = Inode::S_IFREG | 0o644;
    inode.size = 4 * BLOCK_SIZE as u64;
    inode.blocks_512 = 24;
    inode.links_count = 1;
    inode.flags = Inode::EXTENTS_FL;
    inode
        .set_extent_index_root(
            &[ExtentIdx {
                logical_block: 0,
                child: 50,
            }],
            3,
        )
        .unwrap();
    write_inode(&mut image, 12, &inode);
    let inode_before = *image.block(4);
    let parent_before = *image.block(50);
    let intermediate_before = *image.block(51);
    let bitmap_before = *image.block(2);
    let mut pager = Ext4Pager::open(image).unwrap();

    let plan = pager
        .plan_write_page(InodeNo::new(12), 1, &filled_page(0xD8), FsyncStamp::new(61))
        .unwrap();

    assert_eq!(plan.data[0].physical_block, 1001);
    assert!(plan.allocations.is_empty());
    assert_eq!(plan.metadata.len(), 1);
    let after = &plan.metadata[0];
    assert_eq!(after.home, 52);
    let converted = match ExtentNode::parse(&after.after).unwrap() {
        ExtentNode::Leaf(extents) => extents,
        ExtentNode::Index(_) => panic!("depth-three conversion must retain a leaf"),
    };
    assert_eq!(converted[0].logical_block, 0);
    assert_eq!(converted[0].len, Extent::UNINITIALIZED_MASK | 1);
    assert_eq!(converted[1].logical_block, 1);
    assert_eq!(converted[1].len, 1);
    assert_eq!(converted[1].physical_start, 1001);
    assert_eq!(converted[2].logical_block, 2);
    assert_eq!(converted[2].len, Extent::UNINITIALIZED_MASK | 1);
    assert_eq!(pager.image().block(4), &inode_before);
    assert_eq!(pager.image().block(50), &parent_before);
    assert_eq!(pager.image().block(51), &intermediate_before);
    assert_eq!(pager.image().block(52), &leaf_before);
    assert_eq!(pager.image().block(2), &bitmap_before);
}

#[test]
fn destroy_plan_frees_zero_link_regular_inode_without_home_write() {
    let mut image = mock_image();
    mark_block_bitmap_used(&mut image, 31);
    mark_inode_bitmap_used(&mut image, 13);
    let mut victim = Inode::parse(&image.block(4)[11 * 256..12 * 256]).unwrap();
    victim.links_count = 0;
    victim.dtime = 44;
    write_inode(&mut image, 12, &victim);

    let block_bitmap_before = *image.block(2);
    let inode_bitmap_before = *image.block(3);
    let inode_table_before = *image.block(4);
    let superblock_before = *image.block(0);
    let mut pager = Ext4Pager::open(image).unwrap();

    let plan = pager
        .plan_destroy_inode(InodeNo::new(12), FsyncStamp::new(44))
        .unwrap();

    assert_eq!(plan.origin, MutationOrigin::Destroy);
    assert_eq!(plan.object, 12);
    assert!(plan.data.is_empty());
    assert!(plan.allocations.is_empty());
    assert_eq!(
        plan.revokes,
        vec![
            tx_ext4_format::mutation::RevokeRecord { physical_block: 20 },
            tx_ext4_format::mutation::RevokeRecord { physical_block: 21 },
            tx_ext4_format::mutation::RevokeRecord { physical_block: 30 }
        ]
    );
    assert_eq!(
        plan.deferred_frees,
        vec![
            tx_ext4_format::mutation::DeferredFreeClaim { physical_block: 20 },
            tx_ext4_format::mutation::DeferredFreeClaim { physical_block: 21 },
            tx_ext4_format::mutation::DeferredFreeClaim { physical_block: 30 }
        ]
    );

    let block_bitmap = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::BlockBitmap)
        .unwrap();
    assert_eq!(block_bitmap.home, 2);
    assert!(!BitmapView::new(&block_bitmap.after).is_set(20));
    assert!(!BitmapView::new(&block_bitmap.after).is_set(21));
    assert!(!BitmapView::new(&block_bitmap.after).is_set(30));

    let inode_bitmap = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::InodeBitmap)
        .unwrap();
    assert_eq!(inode_bitmap.home, 3);
    assert!(!BitmapView::new(&inode_bitmap.after).is_set(11));

    let group_desc = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::GroupDescriptor)
        .unwrap();
    let parsed_group = GroupDesc::parse(&group_desc.after[..64]).unwrap();
    assert_eq!(parsed_group.free_blocks_count, 35);
    assert_eq!(parsed_group.free_inodes_count, 53);

    let superblock = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::Superblock)
        .unwrap();
    let parsed_superblock = Superblock::parse(&superblock.after[1024..2048]).unwrap();
    assert_eq!(parsed_superblock.free_blocks_count, 35);
    assert_eq!(parsed_superblock.free_inodes_count, 53);

    let inode_table = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::InodeTable)
        .unwrap();
    let deleted_inode = Inode::parse(&inode_table.after[11 * 256..12 * 256]).unwrap();
    assert_eq!(deleted_inode.links_count, 0);
    assert_eq!(deleted_inode.dtime, 0);

    assert_eq!(pager.image().block(2), &block_bitmap_before);
    assert_eq!(pager.image().block(3), &inode_bitmap_before);
    assert_eq!(pager.image().block(4), &inode_table_before);
    assert_eq!(pager.image().block(0), &superblock_before);
}

#[test]
fn destroy_plan_releases_inline_data_blocks_across_groups() {
    let mut image = mock_image();
    let mut superblock = Superblock::parse(&image.block(0)[1024..2048]).unwrap();
    superblock.blocks_count = 128;
    superblock.blocks_per_group = 64;
    superblock.desc_size = 64;
    superblock.feature_incompat |= Superblock::FEATURE_INCOMPAT_64BIT;
    superblock
        .encode(&mut image.block_mut(0)[1024..2048])
        .unwrap();
    GroupDesc {
        block_bitmap: 66,
        inode_bitmap: 67,
        inode_table: 70,
        free_blocks_count: 10,
        free_inodes_count: 64,
        ..GroupDesc::default()
    }
    .encode(&mut image.block_mut(1)[64..128])
    .unwrap();
    BitmapMut::new(image.block_mut(2)).set(20).unwrap();
    BitmapMut::new(image.block_mut(66)).set(16).unwrap();
    mark_inode_bitmap_used(&mut image, 12);

    let mut inode = Inode::default();
    inode.mode = Inode::S_IFREG | 0o600;
    inode.size = 2 * BLOCK_SIZE as u64;
    inode.blocks_512 = 16;
    inode.links_count = 0;
    inode.flags = Inode::EXTENTS_FL;
    inode
        .set_extent_root(&[
            Extent {
                logical_block: 0,
                len: 1,
                physical_start: 20,
            },
            Extent {
                logical_block: 1,
                len: 1,
                physical_start: 80,
            },
        ])
        .unwrap();
    write_inode(&mut image, 12, &inode);
    let source_before = [
        *image.block(0),
        *image.block(1),
        *image.block(2),
        *image.block(3),
        *image.block(4),
        *image.block(66),
    ];
    let mut pager = Ext4Pager::open(image).unwrap();

    let plan = pager
        .plan_destroy_inode(InodeNo::new(12), FsyncStamp::new(15))
        .unwrap();

    assert_eq!(
        plan.revokes
            .iter()
            .map(|claim| claim.physical_block)
            .collect::<Vec<_>>(),
        vec![20, 80]
    );
    let bitmaps: Vec<_> = plan
        .metadata
        .iter()
        .filter(|block| block.role == tx_ext4_format::mutation::MetaRole::BlockBitmap)
        .collect();
    assert_eq!(bitmaps.len(), 2);
    assert!(
        !BitmapView::new(&bitmaps.iter().find(|block| block.home == 2).unwrap().after).is_set(20)
    );
    assert!(
        !BitmapView::new(&bitmaps.iter().find(|block| block.home == 66).unwrap().after).is_set(16)
    );
    let groups: Vec<_> = plan
        .metadata
        .iter()
        .filter(|block| block.role == tx_ext4_format::mutation::MetaRole::GroupDescriptor)
        .collect();
    assert_eq!(groups.len(), 1);
    let group0_before = GroupDesc::parse_sized(&source_before[1][..64], 64).unwrap();
    let group0_after = GroupDesc::parse_sized(&groups[0].after[..64], 64).unwrap();
    assert_eq!(
        group0_after.free_blocks_count,
        group0_before.free_blocks_count + 1
    );
    assert_eq!(
        group0_after.free_inodes_count,
        group0_before.free_inodes_count + 1
    );
    assert_eq!(
        GroupDesc::parse_sized(&groups[0].after[64..128], 64)
            .unwrap()
            .free_blocks_count,
        11
    );
    let superblock = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::Superblock)
        .unwrap();
    let parsed_superblock = Superblock::parse(&superblock.after[1024..2048]).unwrap();
    assert_eq!(parsed_superblock.free_blocks_count, 34);
    assert_eq!(parsed_superblock.free_inodes_count, 53);
    for (home, before) in [0, 1, 2, 3, 4, 66].into_iter().zip(source_before) {
        assert_eq!(pager.image().block(home), &before);
    }
}

#[test]
fn destroy_plan_removes_head_orphan_from_superblock_chain() {
    let mut image = mock_image();
    let mut superblock = Superblock::parse(&image.block(0)[1024..2048]).unwrap();
    superblock.last_orphan = 12;
    superblock
        .encode(&mut image.block_mut(0)[1024..2048])
        .unwrap();
    mark_block_bitmap_used(&mut image, 31);
    mark_inode_bitmap_used(&mut image, 13);
    let mut victim = Inode::parse(&image.block(4)[11 * 256..12 * 256]).unwrap();
    victim.links_count = 0;
    victim.dtime = 14;
    write_inode(&mut image, 12, &victim);
    let mut pager = Ext4Pager::open(image).unwrap();

    let plan = pager
        .plan_destroy_inode(InodeNo::new(12), FsyncStamp::new(44))
        .unwrap();

    let superblock = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::Superblock)
        .unwrap();
    let parsed_superblock = Superblock::parse(&superblock.after[1024..2048]).unwrap();
    assert_eq!(parsed_superblock.last_orphan, 14);
    assert_eq!(parsed_superblock.free_blocks_count, 35);
    assert_eq!(parsed_superblock.free_inodes_count, 53);
}

#[test]
fn destroy_plan_removes_singleton_orphan_head_from_superblock_chain() {
    let mut image = mock_image();
    let mut superblock = Superblock::parse(&image.block(0)[1024..2048]).unwrap();
    superblock.last_orphan = 12;
    superblock
        .encode(&mut image.block_mut(0)[1024..2048])
        .unwrap();
    mark_block_bitmap_used(&mut image, 31);
    mark_inode_bitmap_used(&mut image, 13);
    let mut victim = Inode::parse(&image.block(4)[11 * 256..12 * 256]).unwrap();
    victim.links_count = 0;
    victim.dtime = 0;
    write_inode(&mut image, 12, &victim);
    let mut pager = Ext4Pager::open(image).unwrap();

    let plan = pager
        .plan_destroy_inode(InodeNo::new(12), FsyncStamp::new(44))
        .unwrap();

    let superblock = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::Superblock)
        .unwrap();
    let parsed_superblock = Superblock::parse(&superblock.after[1024..2048]).unwrap();
    assert_eq!(parsed_superblock.last_orphan, 0);
    assert_eq!(parsed_superblock.free_blocks_count, 35);
    assert_eq!(parsed_superblock.free_inodes_count, 53);
}

#[test]
fn recovery_cleans_singleton_classic_orphan_head() {
    let mut image = mock_image();
    let mut superblock = Superblock::parse(&image.block(0)[1024..2048]).unwrap();
    superblock.last_orphan = 12;
    superblock
        .encode(&mut image.block_mut(0)[1024..2048])
        .unwrap();
    mark_block_bitmap_used(&mut image, 31);
    mark_inode_bitmap_used(&mut image, 13);
    let mut victim = Inode::parse(&image.block(4)[11 * 256..12 * 256]).unwrap();
    victim.links_count = 0;
    victim.dtime = 0;
    write_inode(&mut image, 12, &victim);
    let mut pager = Ext4Pager::open(image).unwrap();

    assert_eq!(pager.recover_classic_orphan_chain().unwrap(), 1);

    let image = pager.image();
    let parsed_superblock = Superblock::parse(&image.block(0)[1024..2048]).unwrap();
    assert_eq!(parsed_superblock.last_orphan, 0);
    assert_eq!(parsed_superblock.free_blocks_count, 35);
    assert_eq!(parsed_superblock.free_inodes_count, 53);
    assert!(!BitmapView::new(image.block(2)).is_set(20));
    assert!(!BitmapView::new(image.block(2)).is_set(21));
    assert!(!BitmapView::new(image.block(2)).is_set(30));
    assert!(!BitmapView::new(image.block(3)).is_set(11));
    let deleted_inode = Inode::parse(&image.block(4)[11 * 256..12 * 256]).unwrap();
    assert_eq!(deleted_inode.links_count, 0);
    assert_eq!(deleted_inode.dtime, 0);
}

#[test]
fn destroy_plan_frees_zero_link_empty_directory_and_decrements_used_dirs() {
    let mut image = mock_image();
    mark_block_bitmap_used(&mut image, 18);
    mark_inode_bitmap_used(&mut image, 13);
    GroupDesc {
        block_bitmap: 2,
        inode_bitmap: 3,
        inode_table: 4,
        free_blocks_count: 32,
        free_inodes_count: 52,
        used_dirs_count: 2,
        ..GroupDesc::default()
    }
    .encode(&mut image.block_mut(1)[..64])
    .unwrap();
    let mut victim = Inode::parse(&image.block(4)[12 * 256..13 * 256]).unwrap();
    victim.links_count = 0;
    victim.dtime = 55;
    write_inode(&mut image, 13, &victim);
    encode_directory(
        image.block_mut(17),
        &[(13, 2, b".".as_slice()), (2, 2, b"..".as_slice())],
    );

    let mut pager = Ext4Pager::open(image).unwrap();
    let plan = pager
        .plan_destroy_inode(InodeNo::new(13), FsyncStamp::new(55))
        .unwrap();

    let block_bitmap = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::BlockBitmap)
        .unwrap();
    assert!(!BitmapView::new(&block_bitmap.after).is_set(17));
    assert_eq!(
        plan.deferred_frees,
        vec![tx_ext4_format::mutation::DeferredFreeClaim { physical_block: 17 }]
    );

    let inode_bitmap = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::InodeBitmap)
        .unwrap();
    assert!(!BitmapView::new(&inode_bitmap.after).is_set(12));

    let group_desc = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::GroupDescriptor)
        .unwrap();
    let parsed_group = GroupDesc::parse(&group_desc.after[..64]).unwrap();
    assert_eq!(parsed_group.free_blocks_count, 33);
    assert_eq!(parsed_group.free_inodes_count, 53);
    assert_eq!(parsed_group.used_dirs_count, 1);

    let inode_table = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::InodeTable)
        .unwrap();
    let deleted_inode = Inode::parse(&inode_table.after[12 * 256..13 * 256]).unwrap();
    assert_eq!(deleted_inode.links_count, 0);
    assert_eq!(deleted_inode.dtime, 0);
}

#[test]
fn destroy_plan_frees_zero_link_fast_symlink_without_data_blocks() {
    let mut image = mock_image();
    mark_inode_bitmap_used(&mut image, 14);
    let mut victim = Inode::default();
    victim.mode = 0xA000 | 0o777;
    victim.links_count = 0;
    victim.dtime = 66;
    victim.set_inline_symlink_target(b"target").unwrap();
    write_inode(&mut image, 14, &victim);

    let mut pager = Ext4Pager::open(image).unwrap();
    let plan = pager
        .plan_destroy_inode(InodeNo::new(14), FsyncStamp::new(66))
        .unwrap();

    assert!(plan.revokes.is_empty());
    assert!(plan.deferred_frees.is_empty());
    assert!(
        plan.metadata
            .iter()
            .all(|block| block.role != tx_ext4_format::mutation::MetaRole::BlockBitmap)
    );
    let inode_bitmap = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::InodeBitmap)
        .unwrap();
    assert!(!BitmapView::new(&inode_bitmap.after).is_set(13));
    let inode_table = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::InodeTable)
        .unwrap();
    let deleted_inode = Inode::parse(&inode_table.after[13 * 256..14 * 256]).unwrap();
    assert_eq!(deleted_inode.links_count, 0);
    assert_eq!(deleted_inode.dtime, 0);
}

#[test]
fn destroy_plan_rejects_nonzero_link_inode() {
    let mut image = mock_image();
    mark_inode_bitmap_used(&mut image, 13);
    let mut pager = Ext4Pager::open(image).unwrap();

    assert_eq!(
        pager.plan_destroy_inode(InodeNo::new(12), FsyncStamp::new(45)),
        Err(Ext4FormatError::Unsupported)
    );
}

#[test]
fn namespace_plan_unlinks_dir_entry_and_decrements_nlink_without_home_write() {
    let image = mock_image();
    let dir_before = *image.block(16);
    let inode_before = *image.block(4);
    let mut pager = Ext4Pager::open(image).unwrap();

    let plan = pager
        .plan_unlink_dir_entry(
            InodeNo::new(2),
            b"hello",
            InodeNo::new(12),
            FsyncStamp::new(20),
        )
        .unwrap();

    assert_eq!(plan.origin, MutationOrigin::Unlink);
    assert_eq!(plan.object, 12);
    assert!(plan.data.is_empty());
    assert!(plan.allocations.is_empty());
    assert!(plan.revokes.is_empty());
    assert!(plan.deferred_frees.is_empty());
    assert_eq!(plan.metadata.len(), 3);

    let dir_block = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::DirectoryBlock)
        .unwrap();
    assert_eq!(dir_block.home, 16);
    let names: Vec<_> = DirEntryIter::new(&dir_block.after)
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
        .into_iter()
        .map(|entry| entry.name)
        .collect();
    assert!(!names.iter().any(|name| *name == b"hello"));

    let inode_table = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::InodeTable)
        .unwrap();
    assert_eq!(inode_table.home, 4);
    let inode = Inode::parse(&inode_table.after[11 * 256..12 * 256]).unwrap();
    assert_eq!(inode.links_count, 0);
    assert_eq!(inode.ctime, 20);
    assert_eq!(inode.dtime, 0);

    let superblock = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::Superblock)
        .unwrap();
    assert_eq!(superblock.home, 0);
    let parsed_superblock = Superblock::parse(&superblock.after[1024..2048]).unwrap();
    assert_eq!(parsed_superblock.last_orphan, 12);

    assert_eq!(pager.image().block(16), &dir_before);
    assert_eq!(pager.image().block(4), &inode_before);
}

#[test]
fn namespace_plan_chains_zero_link_unlink_after_existing_orphan_head() {
    let mut image = mock_image();
    let mut superblock = Superblock::parse(&image.block(0)[1024..2048]).unwrap();
    superblock.last_orphan = 14;
    superblock
        .encode(&mut image.block_mut(0)[1024..2048])
        .unwrap();
    let mut pager = Ext4Pager::open(image).unwrap();

    let plan = pager
        .plan_unlink_dir_entry(
            InodeNo::new(2),
            b"hello",
            InodeNo::new(12),
            FsyncStamp::new(20),
        )
        .unwrap();

    let inode_table = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::InodeTable)
        .unwrap();
    let inode = Inode::parse(&inode_table.after[11 * 256..12 * 256]).unwrap();
    assert_eq!(inode.links_count, 0);
    assert_eq!(inode.dtime, 14);

    let superblock = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::Superblock)
        .unwrap();
    let parsed_superblock = Superblock::parse(&superblock.after[1024..2048]).unwrap();
    assert_eq!(parsed_superblock.last_orphan, 12);
}

#[test]
fn namespace_plan_links_regular_file_and_increments_nlink_without_home_write() {
    let image = mock_image();
    let dir_before = *image.block(16);
    let inode_before = *image.block(4);
    let mut pager = Ext4Pager::open(image).unwrap();

    let plan = pager
        .plan_link_dir_entry(
            InodeNo::new(2),
            b"alias",
            InodeNo::new(12),
            FsyncStamp::new(23),
        )
        .unwrap();

    assert_eq!(plan.origin, MutationOrigin::Link);
    assert_eq!(plan.object, 12);
    assert!(plan.data.is_empty());
    assert!(plan.allocations.is_empty());
    assert!(plan.revokes.is_empty());
    assert!(plan.deferred_frees.is_empty());
    assert_eq!(plan.metadata.len(), 2);

    let dir_block = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::DirectoryBlock)
        .unwrap();
    assert_eq!(dir_block.home, 16);
    let entries: Vec<_> = DirEntryIter::new(&dir_block.after)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(
        entries
            .iter()
            .any(|entry| entry.name == b"alias" && entry.inode == 12)
    );

    let inode_table = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::InodeTable)
        .unwrap();
    assert_eq!(inode_table.home, 4);
    let inode = Inode::parse(&inode_table.after[11 * 256..12 * 256]).unwrap();
    assert_eq!(inode.links_count, 2);
    assert_eq!(inode.ctime, 23);

    assert_eq!(pager.image().block(16), &dir_before);
    assert_eq!(pager.image().block(4), &inode_before);
}

#[test]
fn namespace_plan_creates_regular_file_without_home_write() {
    let mut image = mock_image();
    let mut superblock = Superblock::parse(&image.block(0)[1024..2048]).unwrap();
    superblock.feature_ro_compat |= Superblock::FEATURE_RO_COMPAT_METADATA_CSUM;
    superblock
        .encode(&mut image.block_mut(0)[1024..2048])
        .unwrap();
    mark_inode_bitmap_used(&mut image, 13);
    let inode_bitmap_before = *image.block(3);
    let group_desc_before = *image.block(1);
    let superblock_before = *image.block(0);
    let inode_table_before = *image.block(4);
    let dir_before = *image.block(16);
    let mut pager = Ext4Pager::open(image).unwrap();

    let (new_ino, plan) = pager
        .plan_create_regular_file(
            InodeNo::new(2),
            b"created",
            0o100640,
            1001,
            1002,
            FsyncStamp::new(26),
        )
        .unwrap();

    assert_eq!(new_ino, InodeNo::new(14));
    assert_eq!(plan.origin, MutationOrigin::Create);
    assert_eq!(plan.object, 14);
    assert!(plan.data.is_empty());
    assert!(plan.allocations.is_empty());
    assert!(plan.revokes.is_empty());
    assert!(plan.deferred_frees.is_empty());
    assert_eq!(plan.metadata.len(), 5);

    let inode_bitmap = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::InodeBitmap)
        .unwrap();
    assert_eq!(inode_bitmap.home, 3);
    assert!(!BitmapView::new(&inode_bitmap_before).is_set(13));
    assert!(BitmapView::new(&inode_bitmap.after).is_set(13));

    let group_desc = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::GroupDescriptor)
        .unwrap();
    let parsed_group = GroupDesc::parse(&group_desc.after[..64]).unwrap();
    assert_eq!(parsed_group.free_inodes_count, 51);
    let seed = Superblock::parse(&superblock_before[1024..2048])
        .unwrap()
        .metadata_csum_seed();
    assert_eq!(
        u16::from_le_bytes(group_desc.after[26..28].try_into().unwrap()),
        inode_bitmap_csum32(seed, &inode_bitmap.after, 64).unwrap() as u16
    );

    let superblock = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::Superblock)
        .unwrap();
    assert_eq!(
        Superblock::parse(&superblock.after[1024..2048])
            .unwrap()
            .free_inodes_count,
        51
    );

    let inode_table = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::InodeTable)
        .unwrap();
    let inode = Inode::parse(&inode_table.after[13 * 256..14 * 256]).unwrap();
    assert_eq!(inode.mode, Inode::S_IFREG | 0o640);
    assert_eq!(inode.uid, 1001);
    assert_eq!(inode.gid, 1002);
    assert_eq!(inode.links_count, 1);
    assert_eq!(inode.ctime, 26);
    assert_eq!(inode.mtime, 26);
    assert_eq!(inode.atime, 26);

    let dir = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::DirectoryBlock)
        .unwrap();
    assert_eq!(dir.home, 16);
    let entries: Vec<_> = DirEntryIter::new(&dir.after)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(
        entries
            .iter()
            .any(|entry| entry.name == b"created" && entry.inode == 14)
    );

    assert_eq!(pager.image().block(3), &inode_bitmap_before);
    assert_eq!(pager.image().block(1), &group_desc_before);
    assert_eq!(pager.image().block(0), &superblock_before);
    assert_eq!(pager.image().block(4), &inode_table_before);
    assert_eq!(pager.image().block(16), &dir_before);
}

#[test]
fn namespace_plan_creates_directory_without_home_write() {
    let mut image = mock_image();
    let mut superblock = Superblock::parse(&image.block(0)[1024..2048]).unwrap();
    superblock.feature_ro_compat |= Superblock::FEATURE_RO_COMPAT_METADATA_CSUM;
    superblock.required_extra_isize = 32;
    superblock.desired_extra_isize = 32;
    superblock
        .encode(&mut image.block_mut(0)[1024..2048])
        .unwrap();
    mark_inode_bitmap_used(&mut image, 13);
    mark_block_bitmap_used(&mut image, 48);
    let block_bitmap_before = *image.block(2);
    let inode_bitmap_before = *image.block(3);
    let group_desc_before = *image.block(1);
    let superblock_before = *image.block(0);
    let inode_table_before = *image.block(4);
    let parent_dir_before = *image.block(16);
    let data_before = *image.block(48);
    let mut pager = Ext4Pager::open(image).unwrap();

    let (new_ino, data_block, plan) = pager
        .plan_create_directory(
            InodeNo::new(2),
            b"newdir",
            0o755,
            1001,
            1002,
            FsyncStamp::new(27),
        )
        .unwrap();

    assert_eq!(new_ino, InodeNo::new(14));
    assert_eq!(data_block, 48);
    assert_eq!(plan.origin, MutationOrigin::Create);
    assert_eq!(plan.object, 14);
    assert!(plan.data.is_empty());
    assert_eq!(plan.allocations.len(), 1);
    assert_eq!(plan.allocations[0].physical_block, 48);
    assert!(plan.revokes.is_empty());
    assert!(plan.deferred_frees.is_empty());
    assert_eq!(plan.metadata.len(), 7);

    let block_bitmap = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::BlockBitmap)
        .unwrap();
    assert_eq!(block_bitmap.home, 2);
    assert!(BitmapView::new(&block_bitmap.after).is_set(48));

    let inode_bitmap = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::InodeBitmap)
        .unwrap();
    assert_eq!(inode_bitmap.home, 3);
    assert!(!BitmapView::new(&inode_bitmap_before).is_set(13));
    assert!(BitmapView::new(&inode_bitmap.after).is_set(13));

    let group_desc = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::GroupDescriptor)
        .unwrap();
    let parsed_group = GroupDesc::parse(&group_desc.after[..64]).unwrap();
    assert_eq!(parsed_group.free_blocks_count, 31);
    assert_eq!(parsed_group.free_inodes_count, 51);
    assert_eq!(parsed_group.used_dirs_count, 2);
    let seed = Superblock::parse(&superblock_before[1024..2048])
        .unwrap()
        .metadata_csum_seed();
    assert_eq!(
        u16::from_le_bytes(group_desc.after[24..26].try_into().unwrap()),
        block_bitmap_csum32(seed, &block_bitmap.after, 64).unwrap() as u16
    );
    assert_eq!(
        u16::from_le_bytes(group_desc.after[26..28].try_into().unwrap()),
        inode_bitmap_csum32(seed, &inode_bitmap.after, 64).unwrap() as u16
    );

    let superblock = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::Superblock)
        .unwrap();
    let parsed_superblock = Superblock::parse(&superblock.after[1024..2048]).unwrap();
    assert_eq!(parsed_superblock.free_blocks_count, 31);
    assert_eq!(parsed_superblock.free_inodes_count, 51);

    let inode_table = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::InodeTable)
        .unwrap();
    let parent_inode = Inode::parse(&inode_table.after[256..512]).unwrap();
    assert_eq!(parent_inode.links_count, 4);
    assert_eq!(parent_inode.ctime, 27);
    let inode = Inode::parse(&inode_table.after[13 * 256..14 * 256]).unwrap();
    assert_eq!(inode.mode, Inode::S_IFDIR | 0o755);
    assert_eq!(inode.uid, 1001);
    assert_eq!(inode.gid, 1002);
    assert_eq!(inode.size, BLOCK_SIZE as u64);
    assert_eq!(inode.links_count, 2);
    assert_eq!(inode.blocks_512, 8);
    assert_eq!(inode.extra_isize, 32);
    assert_eq!(inode.ctime, 27);
    assert_eq!(inode.mtime, 27);
    assert_eq!(inode.atime, 27);
    assert_eq!(
        inode.map_extent_block(0).unwrap(),
        tx_ext4_format::ondisk::BlockMapping::Data(48)
    );

    let parent_dir = plan.metadata.iter().find(|block| block.home == 16).unwrap();
    assert_eq!(
        parent_dir.role,
        tx_ext4_format::mutation::MetaRole::DirectoryBlock
    );
    let parent_entries: Vec<_> = DirEntryIter::new(&parent_dir.after)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(
        parent_entries
            .iter()
            .any(|entry| entry.name == b"newdir" && entry.inode == 14 && entry.file_type == 2)
    );

    let child_dir = plan.metadata.iter().find(|block| block.home == 48).unwrap();
    assert_eq!(
        child_dir.role,
        tx_ext4_format::mutation::MetaRole::DirectoryBlock
    );
    let child_entries: Vec<_> = DirEntryIter::new(&child_dir.after)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(child_entries.len(), 2);
    assert!(
        child_entries
            .iter()
            .any(|entry| entry.name == b"." && entry.inode == 14)
    );
    assert!(
        child_entries
            .iter()
            .any(|entry| entry.name == b".." && entry.inode == 2)
    );
    let tail = &child_dir.after[BLOCK_SIZE - 12..];
    assert_eq!(&tail[0..4], &[0, 0, 0, 0]);
    assert_eq!(u16::from_le_bytes(tail[4..6].try_into().unwrap()), 12);
    assert_eq!(tail[6], 0);
    assert_eq!(tail[7], 0xDE);
    let stored_dir_checksum = u32::from_le_bytes(tail[8..12].try_into().unwrap());
    let mut child_dir_without_checksum = child_dir.after;
    child_dir_without_checksum[BLOCK_SIZE - 4..].fill(0);
    assert_eq!(
        stored_dir_checksum,
        dirblock_csum32(
            seed,
            new_ino.get(),
            inode.generation,
            &child_dir_without_checksum[..BLOCK_SIZE - 12]
        )
    );

    assert_eq!(pager.image().block(2), &block_bitmap_before);
    assert_eq!(pager.image().block(3), &inode_bitmap_before);
    assert_eq!(pager.image().block(1), &group_desc_before);
    assert_eq!(pager.image().block(0), &superblock_before);
    assert_eq!(pager.image().block(4), &inode_table_before);
    assert_eq!(pager.image().block(16), &parent_dir_before);
    assert_eq!(pager.image().block(48), &data_before);
}

#[test]
fn namespace_plan_create_directory_decrements_group_itable_unused_tail() {
    let mut image = mock_image();
    let mut superblock = Superblock::parse(&image.block(0)[1024..2048]).unwrap();
    superblock.feature_ro_compat |= Superblock::FEATURE_RO_COMPAT_METADATA_CSUM;
    superblock
        .encode(&mut image.block_mut(0)[1024..2048])
        .unwrap();
    mark_inode_bitmap_used(&mut image, 13);
    mark_block_bitmap_used(&mut image, 48);
    image.block_mut(1)[28..30].copy_from_slice(&51u16.to_le_bytes());
    let mut pager = Ext4Pager::open(image).unwrap();

    let (_, _, plan) = pager
        .plan_create_directory(
            InodeNo::new(2),
            b"newdir",
            0o755,
            1001,
            1002,
            FsyncStamp::new(27),
        )
        .unwrap();

    let group_desc = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::GroupDescriptor)
        .unwrap();
    assert_eq!(
        u16::from_le_bytes(group_desc.after[28..30].try_into().unwrap()),
        50
    );
}

#[test]
fn namespace_plan_create_directory_preserves_pending_inode_table_after_images() {
    let mut image = mock_image();
    let mut superblock = Superblock::parse(&image.block(0)[1024..2048]).unwrap();
    superblock.feature_ro_compat |= Superblock::FEATURE_RO_COMPAT_METADATA_CSUM;
    superblock
        .encode(&mut image.block_mut(0)[1024..2048])
        .unwrap();
    mark_inode_bitmap_used(&mut image, 13);
    mark_block_bitmap_used(&mut image, 48);
    let inode_table_home_before = *image.block(4);
    let mut pager = Ext4Pager::open(image).unwrap();

    let (parent_ino, _, parent_plan) = pager
        .plan_create_directory(
            InodeNo::new(2),
            b"parent",
            0o755,
            1001,
            1002,
            FsyncStamp::new(27),
        )
        .unwrap();
    assert_eq!(parent_ino, InodeNo::new(14));
    pager.stage_mutation_after_images(&parent_plan);

    let (_, _, child_plan) = pager
        .plan_create_directory(parent_ino, b"child", 0o755, 1001, 1002, FsyncStamp::new(28))
        .unwrap();

    let inode_table = child_plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::InodeTable)
        .unwrap();
    assert_ne!(&inode_table.after, &inode_table_home_before);
    let parent_inode = Inode::parse(&inode_table.after[13 * 256..14 * 256]).unwrap();
    assert_eq!(parent_inode.mode, Inode::S_IFDIR | 0o755);
    assert_eq!(parent_inode.links_count, 3);
    assert_eq!(parent_inode.ctime, 28);
}

#[test]
fn namespace_plan_creates_fast_symlink_without_home_write() {
    let mut image = mock_image();
    let mut superblock = Superblock::parse(&image.block(0)[1024..2048]).unwrap();
    superblock.feature_ro_compat |= Superblock::FEATURE_RO_COMPAT_METADATA_CSUM;
    superblock
        .encode(&mut image.block_mut(0)[1024..2048])
        .unwrap();
    mark_inode_bitmap_used(&mut image, 13);
    let inode_bitmap_before = *image.block(3);
    let group_desc_before = *image.block(1);
    let superblock_before = *image.block(0);
    let inode_table_before = *image.block(4);
    let dir_before = *image.block(16);
    let mut pager = Ext4Pager::open(image).unwrap();

    let (new_ino, plan) = pager
        .plan_create_fast_symlink(
            InodeNo::new(2),
            b"alink",
            b"nested/child",
            1001,
            1002,
            FsyncStamp::new(28),
        )
        .unwrap();

    assert_eq!(new_ino, InodeNo::new(14));
    assert_eq!(plan.origin, MutationOrigin::Create);
    assert_eq!(plan.object, 14);
    assert!(plan.data.is_empty());
    assert!(plan.allocations.is_empty());
    assert!(plan.revokes.is_empty());
    assert!(plan.deferred_frees.is_empty());
    assert_eq!(plan.metadata.len(), 5);

    let inode_bitmap = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::InodeBitmap)
        .unwrap();
    assert_eq!(inode_bitmap.home, 3);
    assert!(!BitmapView::new(&inode_bitmap_before).is_set(13));
    assert!(BitmapView::new(&inode_bitmap.after).is_set(13));

    let group_desc = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::GroupDescriptor)
        .unwrap();
    let parsed_group = GroupDesc::parse(&group_desc.after[..64]).unwrap();
    assert_eq!(parsed_group.free_inodes_count, 51);
    let seed = Superblock::parse(&superblock_before[1024..2048])
        .unwrap()
        .metadata_csum_seed();
    assert_eq!(
        u16::from_le_bytes(group_desc.after[26..28].try_into().unwrap()),
        inode_bitmap_csum32(seed, &inode_bitmap.after, 64).unwrap() as u16
    );

    let superblock = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::Superblock)
        .unwrap();
    assert_eq!(
        Superblock::parse(&superblock.after[1024..2048])
            .unwrap()
            .free_inodes_count,
        51
    );

    let inode_table = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::InodeTable)
        .unwrap();
    let inode = Inode::parse(&inode_table.after[13 * 256..14 * 256]).unwrap();
    assert_eq!(inode.mode, Inode::S_IFLNK | 0o777);
    assert_eq!(inode.uid, 1001);
    assert_eq!(inode.gid, 1002);
    assert_eq!(inode.size, b"nested/child".len() as u64);
    assert_eq!(inode.links_count, 1);
    assert_eq!(inode.blocks_512, 0);
    assert_eq!(inode.flags, 0);
    assert_eq!(inode.ctime, 28);
    assert_eq!(inode.mtime, 28);
    assert_eq!(inode.atime, 28);
    assert_eq!(
        inode.inline_symlink_target().unwrap(),
        Some(b"nested/child".as_slice())
    );

    let dir = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::DirectoryBlock)
        .unwrap();
    assert_eq!(dir.home, 16);
    let entries: Vec<_> = DirEntryIter::new(&dir.after)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(
        entries
            .iter()
            .any(|entry| entry.name == b"alink" && entry.inode == 14 && entry.file_type == 7)
    );

    assert_eq!(pager.image().block(3), &inode_bitmap_before);
    assert_eq!(pager.image().block(1), &group_desc_before);
    assert_eq!(pager.image().block(0), &superblock_before);
    assert_eq!(pager.image().block(4), &inode_table_before);
    assert_eq!(pager.image().block(16), &dir_before);
}

#[test]
fn namespace_plan_renames_regular_file_in_place_without_home_write() {
    let image = mock_image();
    let dir_before = *image.block(16);
    let mut pager = Ext4Pager::open(image).unwrap();

    let plan = pager
        .plan_rename_dir_entry(
            InodeNo::new(2),
            b"hello",
            b"moved",
            InodeNo::new(12),
            FsyncStamp::new(21),
        )
        .unwrap();

    assert_eq!(plan.origin, MutationOrigin::Rename);
    assert_eq!(plan.object, 12);
    assert!(plan.data.is_empty());
    assert!(plan.allocations.is_empty());
    assert!(plan.revokes.is_empty());
    assert!(plan.deferred_frees.is_empty());
    assert_eq!(plan.metadata.len(), 1);
    assert_eq!(
        plan.metadata[0].role,
        tx_ext4_format::mutation::MetaRole::DirectoryBlock
    );
    assert_eq!(plan.metadata[0].home, 16);

    let entries: Vec<_> = DirEntryIter::new(&plan.metadata[0].after)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(!entries.iter().any(|entry| entry.name == b"hello"));
    assert!(
        entries
            .iter()
            .any(|entry| entry.name == b"moved" && entry.inode == 12)
    );
    assert_eq!(pager.image().block(16), &dir_before);
}

#[test]
fn namespace_plan_cross_dir_renames_regular_file_without_home_write() {
    let image = mock_image();
    let old_dir_before = *image.block(16);
    let new_dir_before = *image.block(17);
    let mut pager = Ext4Pager::open(image).unwrap();

    let plan = pager
        .plan_cross_dir_rename_dir_entry(
            InodeNo::new(2),
            b"hello",
            InodeNo::new(13),
            b"moved",
            InodeNo::new(12),
            FsyncStamp::new(25),
        )
        .unwrap();

    assert_eq!(plan.origin, MutationOrigin::Rename);
    assert_eq!(plan.object, 12);
    assert!(plan.data.is_empty());
    assert!(plan.allocations.is_empty());
    assert!(plan.revokes.is_empty());
    assert!(plan.deferred_frees.is_empty());
    assert_eq!(plan.metadata.len(), 2);

    let old_dir = plan.metadata.iter().find(|block| block.home == 16).unwrap();
    assert_eq!(
        old_dir.role,
        tx_ext4_format::mutation::MetaRole::DirectoryBlock
    );
    let old_entries: Vec<_> = DirEntryIter::new(&old_dir.after)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(!old_entries.iter().any(|entry| entry.name == b"hello"));

    let new_dir = plan.metadata.iter().find(|block| block.home == 17).unwrap();
    assert_eq!(
        new_dir.role,
        tx_ext4_format::mutation::MetaRole::DirectoryBlock
    );
    let new_entries: Vec<_> = DirEntryIter::new(&new_dir.after)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(
        new_entries
            .iter()
            .any(|entry| entry.name == b"moved" && entry.inode == 12)
    );
    assert!(!new_entries.iter().any(|entry| entry.name == b"hello"));

    assert_eq!(pager.image().block(16), &old_dir_before);
    assert_eq!(pager.image().block(17), &new_dir_before);
}

#[test]
fn namespace_plan_rename_overwrites_regular_file_without_home_write() {
    let mut image = mock_image();
    let mut other_inode = Inode::default();
    other_inode.mode = 0x8000 | 0o644;
    other_inode.size = BLOCK_SIZE as u64;
    other_inode.links_count = 1;
    other_inode.blocks_512 = 8;
    other_inode.flags = Inode::EXTENTS_FL;
    other_inode
        .set_extent_root(&[Extent {
            logical_block: 0,
            len: 1,
            physical_start: 31,
        }])
        .unwrap();
    write_inode(&mut image, 14, &other_inode);
    encode_directory(
        image.block_mut(16),
        &[
            (2, 2, b".".as_slice()),
            (2, 2, b"..".as_slice()),
            (12, 1, b"hello".as_slice()),
            (14, 1, b"other".as_slice()),
            (13, 2, b"nested".as_slice()),
        ],
    );
    let dir_before = *image.block(16);
    let inode_before = *image.block(4);
    let mut pager = Ext4Pager::open(image).unwrap();

    let plan = pager
        .plan_rename_overwrite_dir_entry(
            InodeNo::new(2),
            b"hello",
            b"other",
            InodeNo::new(12),
            InodeNo::new(14),
            FsyncStamp::new(24),
        )
        .unwrap();

    assert_eq!(plan.origin, MutationOrigin::Rename);
    assert_eq!(plan.object, 12);
    assert!(plan.data.is_empty());
    assert!(plan.allocations.is_empty());
    assert!(plan.revokes.is_empty());
    assert!(plan.deferred_frees.is_empty());
    assert_eq!(plan.metadata.len(), 2);

    let dir_block = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::DirectoryBlock)
        .unwrap();
    assert_eq!(dir_block.home, 16);
    let entries: Vec<_> = DirEntryIter::new(&dir_block.after)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(!entries.iter().any(|entry| entry.name == b"hello"));
    assert!(
        entries
            .iter()
            .any(|entry| entry.name == b"other" && entry.inode == 12)
    );
    assert!(
        !entries
            .iter()
            .any(|entry| entry.name == b"other" && entry.inode == 14)
    );

    let inode_table = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::InodeTable)
        .unwrap();
    assert_eq!(inode_table.home, 4);
    let old_inode = Inode::parse(&inode_table.after[11 * 256..12 * 256]).unwrap();
    let overwritten_inode = Inode::parse(&inode_table.after[13 * 256..14 * 256]).unwrap();
    assert_eq!(old_inode.links_count, 1);
    assert_eq!(overwritten_inode.links_count, 0);
    assert_eq!(overwritten_inode.ctime, 24);

    assert_eq!(pager.image().block(16), &dir_before);
    assert_eq!(pager.image().block(4), &inode_before);
}

#[test]
fn namespace_plan_rmdirs_empty_directory_without_home_write() {
    let mut image = mock_image();
    encode_directory(
        image.block_mut(17),
        &[(13, 2, b".".as_slice()), (2, 2, b"..".as_slice())],
    );
    let dir_before = *image.block(16);
    let nested_before = *image.block(17);
    let inode_before = *image.block(4);
    let mut pager = Ext4Pager::open(image).unwrap();

    let plan = pager
        .plan_rmdir_dir_entry(
            InodeNo::new(2),
            b"nested",
            InodeNo::new(13),
            FsyncStamp::new(22),
        )
        .unwrap();

    assert_eq!(plan.origin, MutationOrigin::Unlink);
    assert_eq!(plan.object, 13);
    assert!(plan.data.is_empty());
    assert!(plan.allocations.is_empty());
    assert!(plan.revokes.is_empty());
    assert!(plan.deferred_frees.is_empty());
    assert_eq!(plan.metadata.len(), 2);

    let dir_block = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::DirectoryBlock)
        .unwrap();
    assert_eq!(dir_block.home, 16);
    let names: Vec<_> = DirEntryIter::new(&dir_block.after)
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
        .into_iter()
        .map(|entry| entry.name)
        .collect();
    assert!(!names.iter().any(|name| *name == b"nested"));

    let inode_table = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::InodeTable)
        .unwrap();
    assert_eq!(inode_table.home, 4);
    let root_inode = Inode::parse(&inode_table.after[1 * 256..2 * 256]).unwrap();
    let nested_inode = Inode::parse(&inode_table.after[12 * 256..13 * 256]).unwrap();
    assert_eq!(root_inode.links_count, 2);
    assert_eq!(root_inode.ctime, 22);
    assert_eq!(nested_inode.links_count, 0);
    assert_eq!(nested_inode.ctime, 22);

    assert_eq!(pager.image().block(16), &dir_before);
    assert_eq!(pager.image().block(17), &nested_before);
    assert_eq!(pager.image().block(4), &inode_before);
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

#[test]
fn pager_recursively_truncates_depth_two_tree_without_home_writes() {
    let mut image = mock_image();
    for block in [20, 21, 30, 32, 33, 34, 35] {
        BitmapMut::new(image.block_mut(2)).set(block).unwrap();
    }

    ExtentNode::encode_leaf(
        &[Extent {
            logical_block: 0,
            len: 2,
            physical_start: 20,
        }],
        image.block_mut(34),
    )
    .unwrap();
    ExtentNode::encode_leaf(
        &[Extent {
            logical_block: 2,
            len: 1,
            physical_start: 30,
        }],
        image.block_mut(35),
    )
    .unwrap();
    ExtentNode::encode_index(
        1,
        &[ExtentIdx {
            logical_block: 0,
            child: 34,
        }],
        image.block_mut(32),
    )
    .unwrap();
    ExtentNode::encode_index(
        1,
        &[ExtentIdx {
            logical_block: 2,
            child: 35,
        }],
        image.block_mut(33),
    )
    .unwrap();

    let mut inode = Inode::default();
    inode.mode = Inode::S_IFREG | 0o600;
    inode.size = 3 * BLOCK_SIZE as u64;
    inode.blocks_512 = 56;
    inode.links_count = 1;
    inode.flags = Inode::EXTENTS_FL;
    inode
        .set_extent_index_root(
            &[
                ExtentIdx {
                    logical_block: 0,
                    child: 32,
                },
                ExtentIdx {
                    logical_block: 2,
                    child: 33,
                },
            ],
            2,
        )
        .unwrap();
    write_inode(&mut image, 12, &inode);
    let source_before = [
        *image.block(0),
        *image.block(2),
        *image.block(4),
        *image.block(32),
        *image.block(33),
        *image.block(34),
        *image.block(35),
    ];
    let mut pager = Ext4Pager::open(image).unwrap();

    let plan = pager
        .plan_truncate_size(InodeNo::new(12), BLOCK_SIZE as u64, FsyncStamp::new(91))
        .unwrap();

    assert_eq!(
        plan.revokes
            .iter()
            .map(|claim| claim.physical_block)
            .collect::<Vec<_>>(),
        vec![21, 30, 33, 35]
    );
    let extent_node = plan.metadata.iter().find(|block| block.home == 34).unwrap();
    assert_eq!(Extent::parse_all(&extent_node.after).unwrap()[0].len, 1);
    let inode_table = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::InodeTable)
        .unwrap();
    let inode_after = Inode::parse(&inode_table.after[11 * 256..12 * 256]).unwrap();
    assert_eq!(inode_after.size, BLOCK_SIZE as u64);
    assert_eq!(inode_after.blocks_512, 24);
    let root = match ExtentNode::parse(inode_after.extent_root_bytes()).unwrap() {
        ExtentNode::Index(indexes) => indexes,
        ExtentNode::Leaf(_) => panic!("depth-two root must remain indexed"),
    };
    assert_eq!(
        root,
        vec![ExtentIdx {
            logical_block: 0,
            child: 32
        }]
    );
    for (home, before) in [0, 2, 4, 32, 33, 34, 35].into_iter().zip(source_before) {
        assert_eq!(pager.image().block(home), &before);
    }
}

#[test]
fn pager_recursively_truncates_depth_three_tree_without_home_writes() {
    let mut image = mock_image();
    for block in [20, 30, 31, 32] {
        BitmapMut::new(image.block_mut(2)).set(block).unwrap();
    }
    ExtentNode::encode_leaf(
        &[Extent {
            logical_block: 0,
            len: 1,
            physical_start: 20,
        }],
        image.block_mut(30),
    )
    .unwrap();
    ExtentNode::encode_index(
        1,
        &[ExtentIdx {
            logical_block: 0,
            child: 30,
        }],
        image.block_mut(31),
    )
    .unwrap();
    ExtentNode::encode_index(
        2,
        &[ExtentIdx {
            logical_block: 0,
            child: 31,
        }],
        image.block_mut(32),
    )
    .unwrap();

    let mut inode = Inode::default();
    inode.mode = Inode::S_IFREG | 0o600;
    inode.size = BLOCK_SIZE as u64;
    inode.blocks_512 = 32;
    inode.links_count = 1;
    inode.flags = Inode::EXTENTS_FL;
    inode
        .set_extent_index_root(
            &[ExtentIdx {
                logical_block: 0,
                child: 32,
            }],
            3,
        )
        .unwrap();
    write_inode(&mut image, 12, &inode);
    let source_before = [
        *image.block(0),
        *image.block(2),
        *image.block(4),
        *image.block(20),
        *image.block(30),
        *image.block(31),
        *image.block(32),
    ];
    let mut pager = Ext4Pager::open(image).unwrap();

    let plan = pager
        .plan_truncate_size(InodeNo::new(12), 0, FsyncStamp::new(93))
        .unwrap();

    assert_eq!(
        plan.revokes
            .iter()
            .map(|claim| claim.physical_block)
            .collect::<Vec<_>>(),
        vec![20, 30, 31, 32]
    );
    assert_eq!(plan.revokes.len(), plan.deferred_frees.len());
    for (home, before) in [0, 2, 4, 20, 30, 31, 32].into_iter().zip(source_before) {
        assert_eq!(pager.image().block(home), &before);
    }
}

#[test]
fn pager_destroys_depth_two_indexed_inode_without_home_writes() {
    let mut image = mock_image();
    let mut superblock = Superblock::parse(&image.block(0)[1024..2048]).unwrap();
    superblock.blocks_count = 128;
    superblock.blocks_per_group = 64;
    superblock.desc_size = 64;
    superblock.feature_incompat |= Superblock::FEATURE_INCOMPAT_64BIT;
    superblock
        .encode(&mut image.block_mut(0)[1024..2048])
        .unwrap();
    GroupDesc {
        block_bitmap: 66,
        inode_bitmap: 67,
        inode_table: 70,
        free_blocks_count: 10,
        free_inodes_count: 64,
        ..GroupDesc::default()
    }
    .encode(&mut image.block_mut(1)[64..128])
    .unwrap();
    for block in [60, 61, 80] {
        let (home, bit) = if block < 64 {
            (2, block)
        } else {
            (66, block - 64)
        };
        BitmapMut::new(image.block_mut(home)).set(bit).unwrap();
    }
    BitmapMut::new(image.block_mut(3)).set(11).unwrap();
    ExtentNode::encode_leaf(
        &[Extent {
            logical_block: 0,
            len: 1,
            physical_start: 80,
        }],
        image.block_mut(61),
    )
    .unwrap();
    ExtentNode::encode_index(
        1,
        &[ExtentIdx {
            logical_block: 0,
            child: 61,
        }],
        image.block_mut(60),
    )
    .unwrap();
    let mut inode = Inode::default();
    inode.mode = Inode::S_IFREG | 0o600;
    inode.size = BLOCK_SIZE as u64;
    inode.blocks_512 = 24;
    inode.links_count = 0;
    inode.flags = Inode::EXTENTS_FL;
    inode
        .set_extent_index_root(
            &[ExtentIdx {
                logical_block: 0,
                child: 60,
            }],
            2,
        )
        .unwrap();
    write_inode(&mut image, 12, &inode);
    let source_before = [
        *image.block(0),
        *image.block(2),
        *image.block(3),
        *image.block(4),
        *image.block(60),
        *image.block(61),
        *image.block(80),
    ];
    let mut pager = Ext4Pager::open(image).unwrap();

    let plan = pager
        .plan_destroy_inode(InodeNo::new(12), FsyncStamp::new(92))
        .unwrap();

    assert_eq!(
        plan.revokes
            .iter()
            .map(|claim| claim.physical_block)
            .collect::<Vec<_>>(),
        vec![60, 61, 80]
    );
    let inode_table = plan
        .metadata
        .iter()
        .find(|block| block.role == tx_ext4_format::mutation::MetaRole::InodeTable)
        .unwrap();
    let deleted_inode = Inode::parse(&inode_table.after[11 * 256..12 * 256]).unwrap();
    assert_eq!(deleted_inode.links_count, 0);
    assert_eq!(deleted_inode.dtime, 0);
    for (home, before) in [0, 2, 3, 4, 60, 61, 80].into_iter().zip(source_before) {
        assert_eq!(pager.image().block(home), &before);
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
        free_blocks_count: 32,
        free_inodes_count: 52,
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

#[test]
fn pager_derives_journal_ring_from_journal_inode_mapping() {
    let mut image = mock_image();
    let mut journal_inode = Inode::default();
    journal_inode.mode = 0x8000 | 0o600;
    journal_inode.size = 8 * BLOCK_SIZE as u64;
    journal_inode.blocks_512 = 64;
    journal_inode.links_count = 1;
    journal_inode.flags = Inode::EXTENTS_FL;
    journal_inode
        .set_extent_root(&[
            Extent {
                logical_block: 0,
                len: 3,
                physical_start: 40,
            },
            Extent {
                logical_block: 3,
                len: 5,
                physical_start: 50,
            },
        ])
        .unwrap();
    write_inode(&mut image, 8, &journal_inode);
    let journal = image.block_mut(40);
    journal[..4].copy_from_slice(&JBD2_MAGIC.to_be_bytes());
    journal[4..8].copy_from_slice(&4u32.to_be_bytes());
    journal[8..12].copy_from_slice(&23u32.to_be_bytes());
    journal[12..16].copy_from_slice(&(BLOCK_SIZE as u32).to_be_bytes());
    journal[16..20].copy_from_slice(&8u32.to_be_bytes());
    journal[20..24].copy_from_slice(&1u32.to_be_bytes());
    journal[24..28].copy_from_slice(&24u32.to_be_bytes());
    journal[28..32].copy_from_slice(&3u32.to_be_bytes());
    journal[48..64].copy_from_slice(&[0x6b; 16]);
    let expected_journal_superblock_page = *image.block(40);

    let mut pager = Ext4Pager::open(image).expect("open journal image");
    let geometry = pager.journal_geometry().expect("derive journal geometry");

    assert_eq!(geometry.superblock.max_len, 8);
    assert_eq!(geometry.superblock.first, 1);
    assert_eq!(geometry.superblock.sequence, 24);
    assert_eq!(geometry.superblock.start, 3);
    assert_eq!(
        geometry.blocks.as_slice(),
        &[40, 41, 42, 50, 51, 52, 53, 54]
    );
    assert_eq!(
        geometry.superblock_page.as_ref(),
        Some(&expected_journal_superblock_page)
    );
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
