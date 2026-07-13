//! Ext4-to-L6 block-plan helpers.

use alloc::vec;

use tx_subsystems::fs_iface::{IoDataTarget, PageFrameRef};
use tx_subsystems::io_manager::block::{BioPlan, BioVec, BlockFlags, BlockOp, DeviceKey, LbaRange};
use tx_subsystems::execution::Errno;

use tx_ext4_format::pager::BLOCK_SIZE;

/// Block geometry supplied by the concrete device bridge, not by the format pager.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Ext4BlockGeometry {
    pub device: DeviceKey,
    pub sectors_per_block: u64,
}

impl Ext4BlockGeometry {
    pub const fn new(device: DeviceKey, sectors_per_block: u64) -> Self {
        Self { device, sectors_per_block }
    }

    /// Build one L6 read bio for a mapped ext4 data block and an L4-owned target.
    pub fn plan_read(self, physical_block: u64, target: &IoDataTarget) -> Result<BioPlan, Errno> {
        let IoDataTarget::PageCache { frame, offset, len, .. } = target else {
            return Err(Errno::EINVAL);
        };
        if self.sectors_per_block == 0 || *len != BLOCK_SIZE as u32 {
            return Err(Errno::EINVAL);
        }
        let lba = physical_block
            .checked_mul(self.sectors_per_block)
            .ok_or(Errno::EINVAL)?;
        Ok(BioPlan::new(
            self.device,
            BlockOp::Read,
            LbaRange::new(lba, self.sectors_per_block),
            vec![bio_vec(*frame, *offset, *len)],
            BlockFlags::EMPTY,
        ))
    }
}

fn bio_vec(frame: PageFrameRef, offset: u32, len: u32) -> BioVec {
    BioVec::new(frame.ppn().0 as u64, offset, len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tx_hal::Ppn;
    use tx_subsystems::fs_iface::IoDataLeaseId;

    #[test]
    fn mapped_ext4_block_plans_l6_read_into_page_cache_target() {
        let target = IoDataTarget::page_cache(
            IoDataLeaseId::new(1),
            PageFrameRef::new(Ppn(9)),
            0,
            BLOCK_SIZE as u32,
        );
        let plan = Ext4BlockGeometry::new(DeviceKey::new(7), 8)
            .plan_read(11, &target)
            .expect("mapped block plan");
        assert_eq!(plan.device, DeviceKey::new(7));
        assert_eq!(plan.lba, LbaRange::new(88, 8));
        assert_eq!(plan.vecs, vec![BioVec::new(9, 0, BLOCK_SIZE as u32)]);
    }
}
