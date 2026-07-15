//! Immutable ext4 write-mutation planning values.
//!
//! Format code uses these values to describe a complete before-home-write
//! transaction. Execution, journal ownership, and I/O scheduling remain in
//! `tx-ext4` and `io_manager`.

use alloc::vec::Vec;

use crate::pager::{Page4K, BLOCK_SIZE};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MutationOrigin {
    FlushPage,
    Create,
    Rename,
    Truncate,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct FsyncStamp(u64);

impl FsyncStamp {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetaRole {
    InodeTable,
    ExtentNode,
    BlockBitmap,
    InodeBitmap,
    GroupDescriptor,
    Superblock,
    DirectoryBlock,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SealedDataWrite {
    pub logical_page: u64,
    pub physical_block: u64,
    pub bytes: Page4K,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetadataBlock {
    pub home: u64,
    pub role: MetaRole,
    pub before_version: u64,
    pub after: Page4K,
    pub depends_on: Vec<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlockClaim {
    pub physical_block: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Ext4MutationPlan {
    pub origin: MutationOrigin,
    pub object: u64,
    pub fsync_stamp: FsyncStamp,
    pub data: Vec<SealedDataWrite>,
    pub metadata: Vec<MetadataBlock>,
    pub allocations: Vec<BlockClaim>,
}

impl Ext4MutationPlan {
    pub fn new(origin: MutationOrigin, object: u64, fsync_stamp: FsyncStamp) -> Self {
        Self {
            origin,
            object,
            fsync_stamp,
            data: Vec::new(),
            metadata: Vec::new(),
            allocations: Vec::new(),
        }
    }

    pub fn push_metadata(&mut self, block: MetadataBlock) -> Result<(), MutationPlanError> {
        if self.metadata.iter().any(|existing| existing.home == block.home) {
            return Err(MutationPlanError::DuplicateMetadataHome);
        }
        self.metadata.push(block);
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MutationPlanError {
    DuplicateMetadataHome,
    UnsupportedBlockSize,
}

pub const fn supports_mutation_plans() -> Result<(), MutationPlanError> {
    if BLOCK_SIZE == 4096 {
        Ok(())
    } else {
        Err(MutationPlanError::UnsupportedBlockSize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mutation_plan_rejects_duplicate_metadata_home_blocks() {
        let mut plan = Ext4MutationPlan::new(MutationOrigin::FlushPage, 7, FsyncStamp::new(3));
        let block = MetadataBlock {
            home: 12,
            role: MetaRole::InodeTable,
            before_version: 1,
            after: [0; BLOCK_SIZE],
            depends_on: Vec::new(),
        };
        assert_eq!(plan.push_metadata(block.clone()), Ok(()));
        assert_eq!(
            plan.push_metadata(block),
            Err(MutationPlanError::DuplicateMetadataHome)
        );
    }
}
