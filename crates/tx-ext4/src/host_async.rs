use alloc::boxed::Box;
use alloc::vec::Vec;

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;

use tx_ext4_format::ondisk::{
    encode_journal_commit, encode_journal_descriptor, parse_journal_descriptor, BlockMapping,
    CommitHeader, DirEntryIter, ExtentNode, GroupDesc, Inode, InodeLocation, InodeTableLayout,
    Superblock,
};
use tx_ext4_format::pager::{Page4K, BLOCK_SIZE};
use tx_ext4_format::{Ext4FormatError, Result};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait AsyncBlockDevice: Send + Sync + 'static {
    fn total_blocks(&self) -> u64;
    fn read_block<'a>(&'a self, block: u64, out: &'a mut Page4K) -> BoxFuture<'a, Result<()>>;
    fn write_block<'a>(&'a self, block: u64, data: &'a Page4K) -> BoxFuture<'a, Result<()>>;
    fn barrier<'a>(&'a self) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FsObjectId(u64);

impl FsObjectId {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    fn inode_no(self) -> Result<u32> {
        self.0.try_into().map_err(|_| Ext4FormatError::OutOfBounds)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchSource {
    Cache,
    Disk { block: u64 },
    Hole,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageFetch {
    pub source: FetchSource,
    pub page: Page4K,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WritebackReceipt {
    pub fs_object_id: FsObjectId,
    pub offset: u64,
    pub physical_block: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InodeMeta {
    pub mode: u16,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub nlinks: u32,
    pub blocks_512: u64,
    pub flags: u32,
    pub atime: u32,
    pub ctime: u32,
    pub mtime: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalReceipt {
    pub sequence: u32,
    pub target_block: u64,
    pub descriptor_block: u64,
    pub payload_block: u64,
    pub commit_block: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplayReport {
    pub transactions: u32,
    pub blocks_replayed: u32,
}

pub struct Ext4Async<D> {
    device: D,
    superblock: Superblock,
    groups: Vec<GroupDesc>,
    journal_start: Option<u64>,
    next_sequence: Mutex<u32>,
    page_cache: Mutex<BTreeMap<(FsObjectId, u64), Page4K>>,
}

impl<D: AsyncBlockDevice> Ext4Async<D> {
    pub async fn open(device: D) -> Result<Self> {
        let mut block = [0u8; BLOCK_SIZE];
        device.read_block(0, &mut block).await?;
        let superblock = Superblock::parse(&block[1024..2048])?;
        if superblock.block_size() != BLOCK_SIZE as u32 {
            return Err(Ext4FormatError::Unsupported);
        }
        if superblock.feature_incompat & Superblock::FEATURE_INCOMPAT_EXTENTS == 0 {
            return Err(Ext4FormatError::Unsupported);
        }
        let groups = read_group_descs(&device, &superblock).await?;
        let mut fs = Self {
            device,
            superblock,
            groups,
            journal_start: None,
            next_sequence: Mutex::new(1),
            page_cache: Mutex::new(BTreeMap::new()),
        };
        fs.journal_start = match fs
            .read_inode(FsObjectId::new(fs.superblock.journal_inode as u64))
            .await
        {
            Ok(inode) => match fs.resolve_inode_block(&inode, 0).await? {
                BlockMapping::Data(block) => Some(block),
                BlockMapping::Hole | BlockMapping::Unwritten(_) | BlockMapping::NeedNode(_) => None,
            },
            Err(_) => None,
        };
        Ok(fs)
    }

    pub fn device(&self) -> &D {
        &self.device
    }

    pub async fn inode_meta(&self, fs_object_id: FsObjectId) -> Result<InodeMeta> {
        let inode = self.read_inode(fs_object_id).await?;
        Ok(InodeMeta {
            mode: inode.mode,
            uid: inode.uid,
            gid: inode.gid,
            size: inode.size,
            nlinks: inode.links_count as u32,
            blocks_512: inode.blocks_512,
            flags: inode.flags,
            atime: inode.atime,
            ctime: inode.ctime,
            mtime: inode.mtime,
        })
    }

    pub async fn fetch_page(&self, fs_object_id: FsObjectId, offset: u64) -> Result<PageFetch> {
        let page_index = page_index(offset)?;
        if let Some(page) = self.cache_get(fs_object_id, page_index) {
            return Ok(PageFetch {
                source: FetchSource::Cache,
                page,
            });
        }

        let inode = self.read_inode(fs_object_id).await?;
        let page_start = page_index
            .checked_mul(BLOCK_SIZE as u64)
            .ok_or(Ext4FormatError::OutOfBounds)?;
        if page_start >= inode.size {
            return Ok(PageFetch {
                source: FetchSource::Hole,
                page: [0; BLOCK_SIZE],
            });
        }

        match self
            .resolve_inode_block(&inode, logical_block(page_index)?)
            .await?
        {
            BlockMapping::Data(block) => {
                let mut page = [0u8; BLOCK_SIZE];
                self.device.read_block(block, &mut page).await?;
                let valid_len = core::cmp::min(BLOCK_SIZE as u64, inode.size - page_start);
                page[valid_len as usize..].fill(0);
                self.cache_insert(fs_object_id, page_index, page);
                Ok(PageFetch {
                    source: FetchSource::Disk { block },
                    page,
                })
            }
            BlockMapping::Hole | BlockMapping::Unwritten(_) => Ok(PageFetch {
                source: FetchSource::Hole,
                page: [0; BLOCK_SIZE],
            }),
            BlockMapping::NeedNode(_) => Err(Ext4FormatError::Unsupported),
        }
    }

    pub async fn writeback_existing_page(
        &self,
        fs_object_id: FsObjectId,
        offset: u64,
        page: &Page4K,
    ) -> Result<WritebackReceipt> {
        let page_index = page_index(offset)?;
        let inode = self.read_inode(fs_object_id).await?;
        let block = match self
            .resolve_inode_block(&inode, logical_block(page_index)?)
            .await?
        {
            BlockMapping::Data(block) => block,
            BlockMapping::Unwritten(_) | BlockMapping::Hole | BlockMapping::NeedNode(_) => {
                return Err(Ext4FormatError::Unsupported)
            }
        };
        self.device.write_block(block, page).await?;
        self.cache_insert(fs_object_id, page_index, *page);
        Ok(WritebackReceipt {
            fs_object_id,
            offset,
            physical_block: block,
        })
    }

    pub async fn lookup(&self, directory: FsObjectId, name: &[u8]) -> Result<Option<FsObjectId>> {
        let inode = self.read_inode(directory).await?;
        if !inode.is_dir() {
            return Err(Ext4FormatError::Unsupported);
        }
        let page_count = div_ceil_u64(inode.size, BLOCK_SIZE as u64);
        for page_index in 0..page_count {
            let block = match self
                .resolve_inode_block(&inode, logical_block(page_index)?)
                .await?
            {
                BlockMapping::Data(block) => block,
                BlockMapping::Hole | BlockMapping::Unwritten(_) => continue,
                BlockMapping::NeedNode(_) => return Err(Ext4FormatError::Unsupported),
            };
            let mut page = [0u8; BLOCK_SIZE];
            self.device.read_block(block, &mut page).await?;
            for entry in DirEntryIter::new(&page) {
                let entry = entry?;
                if entry.name == name {
                    return Ok(Some(FsObjectId::new(entry.inode as u64)));
                }
            }
        }
        Ok(None)
    }

    pub async fn write_inode_meta_journaled(
        &self,
        fs_object_id: FsObjectId,
        meta: InodeMeta,
    ) -> Result<JournalReceipt> {
        let journal_start = self.journal_start.ok_or(Ext4FormatError::Unsupported)?;
        let location = self.inode_location(fs_object_id)?;
        let mut home_block = [0u8; BLOCK_SIZE];
        self.device
            .read_block(location.block, &mut home_block)
            .await?;
        let mut inode = Inode::parse(&home_block[location.offset..location.offset + location.len])?;
        apply_meta(&mut inode, meta);
        inode.encode(&mut home_block[location.offset..location.offset + location.len])?;

        let sequence = {
            let mut guard = self.next_sequence.lock().unwrap();
            let sequence = *guard;
            *guard = sequence.saturating_add(1);
            sequence
        };
        let descriptor_block = journal_start + 1 + ((sequence - 1) as u64 * 3);
        let payload_block = descriptor_block + 1;
        let commit_block = descriptor_block + 2;

        let mut descriptor = [0u8; BLOCK_SIZE];
        encode_journal_descriptor(sequence, location.block as u32, &mut descriptor)?;
        let mut commit = [0u8; BLOCK_SIZE];
        encode_journal_commit(sequence, &mut commit)?;

        self.device
            .write_block(descriptor_block, &descriptor)
            .await?;
        self.device.write_block(payload_block, &home_block).await?;
        self.device.barrier().await?;
        self.device.write_block(commit_block, &commit).await?;
        self.device.barrier().await?;

        Ok(JournalReceipt {
            sequence,
            target_block: location.block,
            descriptor_block,
            payload_block,
            commit_block,
        })
    }

    pub async fn replay_journal_for_test(&self) -> Result<ReplayReport> {
        let journal_start = self.journal_start.ok_or(Ext4FormatError::Unsupported)?;
        let mut cursor = journal_start + 1;
        let mut transactions = 0u32;
        let mut blocks_replayed = 0u32;
        loop {
            if cursor + 2 >= self.device.total_blocks() {
                break;
            }
            let mut descriptor = [0u8; BLOCK_SIZE];
            self.device.read_block(cursor, &mut descriptor).await?;
            let (header, tag) = match parse_journal_descriptor(&descriptor) {
                Ok(parsed) => parsed,
                Err(_) => break,
            };
            let mut commit = [0u8; BLOCK_SIZE];
            self.device.read_block(cursor + 2, &mut commit).await?;
            let commit = CommitHeader::parse(&commit)?;
            if commit.sequence != header.sequence {
                break;
            }
            let mut payload = [0u8; BLOCK_SIZE];
            self.device.read_block(cursor + 1, &mut payload).await?;
            self.device.write_block(tag.block as u64, &payload).await?;
            transactions += 1;
            blocks_replayed += 1;
            cursor += 3;
        }
        Ok(ReplayReport {
            transactions,
            blocks_replayed,
        })
    }

    async fn read_inode(&self, fs_object_id: FsObjectId) -> Result<Inode> {
        let location = self.inode_location(fs_object_id)?;
        let mut block = [0u8; BLOCK_SIZE];
        self.device.read_block(location.block, &mut block).await?;
        Inode::parse(&block[location.offset..location.offset + location.len])
    }

    fn inode_location(&self, fs_object_id: FsObjectId) -> Result<InodeLocation> {
        let inode = fs_object_id.inode_no()?;
        if inode == 0 || self.superblock.inodes_per_group == 0 {
            return Err(Ext4FormatError::OutOfBounds);
        }
        let inode_index = inode - 1;
        let group_index = inode_index / self.superblock.inodes_per_group;
        let index_in_group = inode_index % self.superblock.inodes_per_group;
        let group = self
            .groups
            .get(group_index as usize)
            .ok_or(Ext4FormatError::OutOfBounds)?;
        InodeTableLayout {
            inode_size: self.superblock.inode_size as usize,
            inodes_per_group: self.superblock.inodes_per_group,
            first_inode_table_block: group.inode_table_block(),
        }
        .locate_index(index_in_group, BLOCK_SIZE)
    }

    async fn resolve_inode_block(&self, inode: &Inode, logical_block: u32) -> Result<BlockMapping> {
        let mut node = ExtentNode::parse(inode.extent_root_bytes())?;
        for _ in 0..8 {
            match node {
                ExtentNode::Leaf(extents) => {
                    for extent in extents {
                        if let Some(block) = extent.physical_for(logical_block) {
                            return Ok(BlockMapping::Data(block));
                        }
                    }
                    return Ok(BlockMapping::Hole);
                }
                ExtentNode::Index(indexes) => {
                    if indexes.is_empty() {
                        return Ok(BlockMapping::Hole);
                    }
                    let mut selected = None;
                    for index in &indexes {
                        if index.logical_block <= logical_block {
                            selected = Some(index.child);
                        } else {
                            break;
                        }
                    }
                    let child = selected.unwrap_or(indexes[0].child);
                    let mut block = [0u8; BLOCK_SIZE];
                    self.device.read_block(child, &mut block).await?;
                    node = ExtentNode::parse(&block)?;
                }
            }
        }
        Err(Ext4FormatError::Corrupt)
    }

    fn cache_get(&self, fs_object_id: FsObjectId, page_index: u64) -> Option<Page4K> {
        self.page_cache
            .lock()
            .unwrap()
            .get(&(fs_object_id, page_index))
            .copied()
    }

    fn cache_insert(&self, fs_object_id: FsObjectId, page_index: u64, page: Page4K) {
        self.page_cache
            .lock()
            .unwrap()
            .insert((fs_object_id, page_index), page);
    }
}

fn apply_meta(inode: &mut Inode, meta: InodeMeta) {
    inode.mode = meta.mode;
    inode.uid = meta.uid;
    inode.gid = meta.gid;
    inode.size = meta.size;
    inode.links_count = meta.nlinks as u16;
    inode.blocks_512 = meta.blocks_512;
    inode.flags = meta.flags;
    inode.atime = meta.atime;
    inode.ctime = meta.ctime;
    inode.mtime = meta.mtime;
}

async fn read_group_descs<D: AsyncBlockDevice>(
    device: &D,
    superblock: &Superblock,
) -> Result<Vec<GroupDesc>> {
    let group_count = superblock.group_count()? as usize;
    let desc_size = superblock.group_desc_size();
    let gdt_start = if superblock.block_size() == 1024 {
        2
    } else {
        1
    };
    let mut block = [0u8; BLOCK_SIZE];
    let mut groups = Vec::new();
    for idx in 0..group_count {
        let byte_offset = idx * desc_size;
        let block_id = gdt_start + (byte_offset / BLOCK_SIZE) as u64;
        let in_block = byte_offset % BLOCK_SIZE;
        device.read_block(block_id, &mut block).await?;
        groups.push(GroupDesc::parse_sized(
            &block[in_block..in_block + desc_size],
            desc_size,
        )?);
    }
    Ok(groups)
}

fn page_index(offset: u64) -> Result<u64> {
    if !offset.is_multiple_of(BLOCK_SIZE as u64) {
        return Err(Ext4FormatError::Unsupported);
    }
    Ok(offset / BLOCK_SIZE as u64)
}

fn logical_block(page_index: u64) -> Result<u32> {
    page_index
        .try_into()
        .map_err(|_| Ext4FormatError::OutOfBounds)
}

fn div_ceil_u64(value: u64, divisor: u64) -> u64 {
    if value == 0 {
        0
    } else {
        1 + (value - 1) / divisor
    }
}
