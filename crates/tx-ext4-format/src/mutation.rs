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
    SetAttr,
    Create,
    Link,
    Unlink,
    Rename,
    Truncate,
}

/// The bounded inode metadata updates supported by the Tier 1 planner.
///
/// The pager applies one value to a complete inode after-image; admission and
/// durable publication remain owned by the mounted ext4 runtime.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SetAttr {
    Mode(u16),
    Owner {
        uid: Option<u32>,
        gid: Option<u32>,
    },
    Times {
        atime_ns: Option<u64>,
        mtime_ns: Option<u64>,
        ctime_ns: u64,
    },
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

/// One home block which an older replayable transaction must no longer write.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct RevokeRecord {
    pub physical_block: u64,
}

/// A block release that remains unavailable until journal-tail reclamation.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct DeferredFreeClaim {
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
    pub revokes: Vec<RevokeRecord>,
    pub deferred_frees: Vec<DeferredFreeClaim>,
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
            revokes: Vec::new(),
            deferred_frees: Vec::new(),
        }
    }

    pub fn push_metadata(&mut self, block: MetadataBlock) -> Result<(), MutationPlanError> {
        if self
            .metadata
            .iter()
            .any(|existing| existing.home == block.home)
        {
            return Err(MutationPlanError::DuplicateMetadataHome);
        }
        self.metadata.push(block);
        Ok(())
    }

    /// Record a block release. The immutable plan canonicalizes the pair so
    /// JBD2 encoding and the mount-owned deferred-free token agree exactly.
    pub fn defer_free(&mut self, physical_block: u64) {
        self.revokes.push(RevokeRecord { physical_block });
        self.deferred_frees
            .push(DeferredFreeClaim { physical_block });
        self.revokes.sort_unstable();
        self.revokes.dedup();
        self.deferred_frees.sort_unstable();
        self.deferred_frees.dedup();
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
    use alloc::vec;

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

    #[test]
    fn freeing_plan_emits_sorted_deduplicated_revoke_and_deferred_free_claims() {
        let mut plan = Ext4MutationPlan::new(MutationOrigin::Truncate, 7, FsyncStamp::new(3));

        plan.defer_free(33);
        plan.defer_free(8);
        plan.defer_free(33);

        assert_eq!(
            plan.revokes,
            vec![
                RevokeRecord { physical_block: 8 },
                RevokeRecord { physical_block: 33 }
            ]
        );
        assert_eq!(
            plan.deferred_frees,
            vec![
                DeferredFreeClaim { physical_block: 8 },
                DeferredFreeClaim { physical_block: 33 }
            ]
        );
    }
}
