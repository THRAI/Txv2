use tx_ext4::mount::mount_ext4_read_only;
use tx_ext4_format::ondisk::{Extent, GroupDesc, Inode, Superblock};
use tx_ext4_format::pager::{BlockImage, Page4K, BLOCK_SIZE};
use tx_ext4_format::Ext4FormatError;
use tx_subsystems::page_backed::FRAME_CAPACITY;
use tx_subsystems::step::StepOutcome;
use tx_subsystems::vfs::fs_ops::DirCursor;
use tx_subsystems::vfs::structure::FsObjectId;

#[derive(Clone)]
struct MockImage {
    blocks: Vec<Page4K>,
}

impl MockImage {
    fn new(blocks: usize) -> Self {
        Self {
            blocks: vec![[0; BLOCK_SIZE]; blocks],
        }
    }

    fn set_block(&mut self, block: u64, data: Page4K) {
        self.blocks[block as usize] = data;
    }
}

impl BlockImage for MockImage {
    fn total_blocks(&self) -> u64 {
        self.blocks.len() as u64
    }

    fn read_block(&self, block: u64, out: &mut Page4K) -> tx_ext4_format::Result<()> {
        *out = *self
            .blocks
            .get(block as usize)
            .ok_or(Ext4FormatError::OutOfBounds)?;
        Ok(())
    }

    fn write_block(&mut self, block: u64, data: &Page4K) -> tx_ext4_format::Result<()> {
        let dst = self
            .blocks
            .get_mut(block as usize)
            .ok_or(Ext4FormatError::OutOfBounds)?;
        *dst = *data;
        Ok(())
    }
}

#[test]
fn read_only_mount_output_reports_ext4_root() {
    setup();

    let output = mount_ext4_read_only(mock_image()).expect("mount ext4");

    assert_eq!(output.root_fs_object_id, FsObjectId(2));
    assert!(output.root_inode_meta.is_directory());
    assert_eq!(output.root_inode_meta.nlinks, 3);
}

#[test]
fn lookup_readdir_and_metadata_use_kernel_facing_backend() {
    setup();
    let output = mount_ext4_read_only(mock_image()).expect("mount ext4");
    let guard = tx_substrate::epoch::guard();

    let nested = match output.fs_ops.lookup(FsObjectId(2), b"nested", &guard) {
        StepOutcome::Done(id) => id,
        other => panic_step("lookup nested", other),
    };
    assert_eq!(nested, FsObjectId(13));
    let child = match output.fs_ops.lookup(nested, b"child", &guard) {
        StepOutcome::Done(id) => id,
        other => panic_step("lookup child", other),
    };
    assert_eq!(child, FsObjectId(12));

    let root_names = collect_dir_names(output.fs_ops.as_ref(), FsObjectId(2), &guard);
    assert!(root_names.iter().any(|name| name == b"nested"));
    let nested_names = collect_dir_names(output.fs_ops.as_ref(), nested, &guard);
    assert!(nested_names.iter().any(|name| name == b"child"));

    let meta = match output.fs_ops.load_inode_meta(child, &guard) {
        StepOutcome::Done(meta) => meta,
        other => panic_step("load child metadata", other),
    };
    assert!(meta.is_regular_file());
    assert_eq!(meta.size, 4 * BLOCK_SIZE as u64);
    assert_eq!(meta.blocks, 24);
}

#[test]
fn fetch_page_reads_ext4_data_blocks_without_tokio_adapter() {
    setup();
    let output = mount_ext4_read_only(mock_image()).expect("mount ext4");
    let guard = tx_substrate::epoch::guard();

    let first = match output.fs_page_backing.fetch_page(FsObjectId(12), 0, &guard) {
        StepOutcome::Done(frame) => frame,
        other => panic_step("fetch first page", other),
    };
    assert_eq!(first.len(), FRAME_CAPACITY);
    assert_eq!(first.as_bytes(), &filled_page(0x20));

    let second = match output
        .fs_page_backing
        .fetch_page(FsObjectId(12), BLOCK_SIZE as u64, &guard)
    {
        StepOutcome::Done(frame) => frame,
        other => panic_step("fetch second page", other),
    };
    assert_eq!(second.as_bytes(), &filled_page(0x21));
}

fn setup() {
    tx_substrate::testing::init_host_for_test_once();
}

fn collect_dir_names<'g>(
    fs_ops: &dyn tx_subsystems::vfs::fs_ops::FsOps,
    fs_object_id: FsObjectId,
    guard: &'g tx_substrate::epoch::Guard<'g>,
) -> Vec<Vec<u8>> {
    let mut cursor = DirCursor([0; 16]);
    let mut names = Vec::new();
    loop {
        match fs_ops.readdir(fs_object_id, cursor, guard) {
            StepOutcome::Done(Some((entry, next))) => {
                names.push(entry.name.as_bytes().to_vec());
                cursor = next;
            }
            StepOutcome::Done(None) => return names,
            other => panic_step("readdir", other),
        }
    }
}

fn panic_step<T>(label: &str, outcome: StepOutcome<T>) -> ! {
    match outcome {
        StepOutcome::Advanced(_) => panic!("{label}: unexpected progress"),
        StepOutcome::Blocked(_, _) => panic!("{label}: unexpected block"),
        StepOutcome::AdvancedThenBlocked(_, _, _) => {
            panic!("{label}: unexpected progress then block")
        }
        StepOutcome::Done(_) => panic!("{label}: unexpected done shape"),
        StepOutcome::Err(err) => panic!("{label}: {err:?}"),
    }
}

fn mock_image() -> MockImage {
    let mut image = MockImage::new(128);

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
    let mut block0 = [0u8; BLOCK_SIZE];
    sb.encode(&mut block0[1024..2048]).unwrap();
    image.set_block(0, block0);

    let mut block1 = [0u8; BLOCK_SIZE];
    GroupDesc {
        block_bitmap: 2,
        inode_bitmap: 3,
        inode_table: 4,
        free_blocks_count: 32,
        free_inodes_count: 52,
        used_dirs_count: 1,
        ..GroupDesc::default()
    }
    .encode(&mut block1[..64])
    .unwrap();
    image.set_block(1, block1);

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
    file_inode.size = 4 * BLOCK_SIZE as u64;
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

    image.set_block(
        16,
        directory_block(&[
            (2, 2, b".".as_slice()),
            (2, 2, b"..".as_slice()),
            (12, 1, b"hello".as_slice()),
            (13, 2, b"nested".as_slice()),
        ]),
    );
    image.set_block(
        17,
        directory_block(&[
            (13, 2, b".".as_slice()),
            (2, 2, b"..".as_slice()),
            (12, 1, b"child".as_slice()),
        ]),
    );
    image.set_block(20, filled_page(0x20));
    image.set_block(21, filled_page(0x21));
    image.set_block(30, filled_page(0x30));

    image
}

fn write_inode(image: &mut MockImage, ino: u32, inode: &Inode) {
    let index = (ino - 1) as usize;
    let offset = index * 256;
    let block = 4 + offset / BLOCK_SIZE;
    let in_block = offset % BLOCK_SIZE;
    inode
        .encode(&mut image.blocks[block][in_block..in_block + 256])
        .unwrap();
}

fn directory_block(entries: &[(u32, u8, &[u8])]) -> Page4K {
    let mut block = [0u8; BLOCK_SIZE];
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
    block
}

fn filled_page(byte: u8) -> Page4K {
    [byte; BLOCK_SIZE]
}
