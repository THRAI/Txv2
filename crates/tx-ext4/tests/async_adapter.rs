use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use tx_ext4::host_async::{
    AsyncBlockDevice, Ext4Async, FetchSource, FsObjectId, JournalReceipt, PageFetch, ReplayReport,
    WritebackReceipt,
};
use tx_ext4_format::ondisk::{
    CommitHeader, Ext4FormatError, Extent, GroupDesc, Inode, JournalBlockTag, JournalHeader,
    Superblock, JBD2_BLOCK_DESCRIPTOR, JBD2_MAGIC,
};
use tx_ext4_format::pager::{Page4K, BLOCK_SIZE};

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Clone)]
struct MockAsyncImage {
    inner: Arc<Mutex<MockState>>,
}

struct MockState {
    blocks: Vec<Page4K>,
    reads: usize,
    writes: usize,
    barriers: usize,
}

impl MockAsyncImage {
    fn new(blocks: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(MockState {
                blocks: vec![[0; BLOCK_SIZE]; blocks],
                reads: 0,
                writes: 0,
                barriers: 0,
            })),
        }
    }

    fn block(&self, block: u64) -> Page4K {
        self.inner.lock().unwrap().blocks[block as usize]
    }

    fn set_block(&self, block: u64, data: Page4K) {
        self.inner.lock().unwrap().blocks[block as usize] = data;
    }

    fn counts(&self) -> (usize, usize, usize) {
        let state = self.inner.lock().unwrap();
        (state.reads, state.writes, state.barriers)
    }
}

impl AsyncBlockDevice for MockAsyncImage {
    fn total_blocks(&self) -> u64 {
        self.inner.lock().unwrap().blocks.len() as u64
    }

    fn read_block<'a>(
        &'a self,
        block: u64,
        out: &'a mut Page4K,
    ) -> BoxFuture<'a, tx_ext4_format::Result<()>> {
        Box::pin(async move {
            tokio::task::yield_now().await;
            let mut state = self.inner.lock().unwrap();
            let src = *state
                .blocks
                .get(block as usize)
                .ok_or(Ext4FormatError::OutOfBounds)?;
            state.reads += 1;
            *out = src;
            Ok(())
        })
    }

    fn write_block<'a>(
        &'a self,
        block: u64,
        data: &'a Page4K,
    ) -> BoxFuture<'a, tx_ext4_format::Result<()>> {
        Box::pin(async move {
            tokio::task::yield_now().await;
            let mut state = self.inner.lock().unwrap();
            let dst = state
                .blocks
                .get_mut(block as usize)
                .ok_or(Ext4FormatError::OutOfBounds)?;
            *dst = *data;
            state.writes += 1;
            Ok(())
        })
    }

    fn barrier<'a>(&'a self) -> BoxFuture<'a, tx_ext4_format::Result<()>> {
        Box::pin(async move {
            tokio::task::yield_now().await;
            self.inner.lock().unwrap().barriers += 1;
            Ok(())
        })
    }
}

#[tokio::test]
async fn fetch_page_awaits_disk_once_then_returns_cache_hit() {
    let image = mock_image();
    let fs = Ext4Async::open(image.clone()).await.unwrap();
    let reads_after_open = image.counts().0;

    let first = fs.fetch_page(FsObjectId::new(12), 0).await.unwrap();
    assert_eq!(
        first,
        PageFetch {
            source: FetchSource::Disk { block: 20 },
            page: filled_page(0x20),
        }
    );
    let reads_after_miss = image.counts().0;
    assert!(reads_after_miss > reads_after_open);

    let second = fs.fetch_page(FsObjectId::new(12), 0).await.unwrap();
    assert_eq!(second.source, FetchSource::Cache);
    assert_eq!(second.page, filled_page(0x20));
    assert_eq!(image.counts().0, reads_after_miss);
}

#[tokio::test]
async fn lookup_and_existing_page_writeback_use_async_device() {
    let image = mock_image();
    let fs = Ext4Async::open(image.clone()).await.unwrap();

    assert_eq!(
        fs.lookup(FsObjectId::new(2), b"nested").await.unwrap(),
        Some(FsObjectId::new(13))
    );
    assert_eq!(
        fs.lookup(FsObjectId::new(13), b"child").await.unwrap(),
        Some(FsObjectId::new(12))
    );

    let page = filled_page(0xEE);
    assert_eq!(
        fs.writeback_existing_page(FsObjectId::new(12), BLOCK_SIZE as u64, &page)
            .await
            .unwrap(),
        WritebackReceipt {
            fs_object_id: FsObjectId::new(12),
            offset: BLOCK_SIZE as u64,
            physical_block: 21,
        }
    );
    assert_eq!(image.block(21), page);
    assert_eq!(image.counts().1, 1);

    let reread = fs
        .fetch_page(FsObjectId::new(12), BLOCK_SIZE as u64)
        .await
        .unwrap();
    assert_eq!(reread.source, FetchSource::Cache);
    assert_eq!(reread.page, page);
}

#[tokio::test]
async fn journaled_inode_meta_write_awaits_ordered_descriptor_payload_commit() {
    let image = mock_image();
    let fs = Ext4Async::open(image.clone()).await.unwrap();

    let mut meta = fs.inode_meta(FsObjectId::new(12)).await.unwrap();
    meta.size = 3 * BLOCK_SIZE as u64;
    meta.mtime = 99;

    assert_eq!(
        fs.write_inode_meta_journaled(FsObjectId::new(12), meta)
            .await
            .unwrap(),
        JournalReceipt {
            sequence: 1,
            target_block: 4,
            descriptor_block: 41,
            payload_block: 42,
            commit_block: 43,
        }
    );
    assert_eq!(image.counts().2, 2);

    let descriptor = image.block(41);
    assert_eq!(
        JournalHeader::parse(&descriptor[..12]).unwrap(),
        JournalHeader {
            magic: JBD2_MAGIC,
            block_type: JBD2_BLOCK_DESCRIPTOR,
            sequence: 1,
        }
    );
    assert_eq!(
        JournalBlockTag::parse(&descriptor[12..20]).unwrap().block,
        4
    );
    assert_ne!(
        fs.inode_meta(FsObjectId::new(12)).await.unwrap().size,
        meta.size
    );

    assert_eq!(
        fs.replay_journal_for_test().await.unwrap(),
        ReplayReport {
            transactions: 1,
            blocks_replayed: 1,
        }
    );
    assert_eq!(
        fs.inode_meta(FsObjectId::new(12)).await.unwrap().size,
        meta.size
    );
    assert_eq!(fs.inode_meta(FsObjectId::new(12)).await.unwrap().mtime, 99);

    let commit = image.block(43);
    assert_eq!(CommitHeader::parse(&commit).unwrap().sequence, 1);
}

fn mock_image() -> MockAsyncImage {
    let image = MockAsyncImage::new(128);

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
    write_inode(&image, 8, &journal_inode);

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
    write_inode(&image, 2, &root_inode);

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
    write_inode(&image, 12, &file_inode);

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
    write_inode(&image, 13, &nested_inode);

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

fn write_inode(image: &MockAsyncImage, ino: u32, inode: &Inode) {
    let mut state = image.inner.lock().unwrap();
    let index = (ino - 1) as usize;
    let offset = index * 256;
    let block = 4 + offset / BLOCK_SIZE;
    let in_block = offset % BLOCK_SIZE;
    inode
        .encode(&mut state.blocks[block][in_block..in_block + 256])
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
