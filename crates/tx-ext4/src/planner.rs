//! Ext4-to-L6 block-plan helpers.

use alloc::vec;

use tx_subsystems::fs_iface::{
    BackendPageRequest, BackendPlan, BackendPlanner, BioPlanList, IoDataTarget, PageCompletion,
    PageCompletionList, PageFrameRef, PagerResumeToken,
};
use tx_subsystems::io_manager::block::{BioPlan, BioVec, BlockFlags, BlockOp, DeviceKey, LbaRange};
use tx_subsystems::execution::Errno;

use tx_ext4_format::pager::BLOCK_SIZE;

/// Block geometry supplied by the concrete device bridge, not by the format pager.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Ext4BlockGeometry {
    pub device: DeviceKey,
    pub sectors_per_block: u64,
}

/// Metadata-free result of resolving one logical ext4 page.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Ext4ReadMapping {
    Hole,
    Data { physical_block: u64 },
    MetadataFirst { resume: PagerResumeToken },
}

/// Mapping lookup supplied by the ext4 metadata owner.
///
/// The provider may return `MetadataFirst` when an extent index block must be
/// fetched. It never performs block I/O inside this callback.
pub trait Ext4ReadMappingSource: Send + Sync + 'static {
    fn map_page(&self, request: &BackendPageRequest) -> Ext4ReadMapping;
}

pub struct Ext4ReadPlanner<S> {
    geometry: Ext4BlockGeometry,
    mapping: S,
}

impl<S> Ext4ReadPlanner<S> {
    pub const fn new(geometry: Ext4BlockGeometry, mapping: S) -> Self {
        Self { geometry, mapping }
    }
}

impl<S: Ext4ReadMappingSource> BackendPlanner for Ext4ReadPlanner<S> {
    fn plan_page_io(&self, request: BackendPageRequest) -> BackendPlan {
        match request.op {
            tx_subsystems::io_manager::page::PageIoOp::Read
            | tx_subsystems::io_manager::page::PageIoOp::Readahead => {
                plan_read_request(self.geometry, &request, self.mapping.map_page(&request))
            }
            _ => BackendPlan::Err(Errno::ENOSYS),
        }
    }
}

/// Translate an ext4 mapping result into the neutral L4/L6 plan IR.
pub fn plan_read_request(
    geometry: Ext4BlockGeometry,
    request: &BackendPageRequest,
    mapping: Ext4ReadMapping,
) -> BackendPlan {
    let Some(generation) = request.generation_hint else {
        return BackendPlan::Err(Errno::EINVAL);
    };
    match mapping {
        Ext4ReadMapping::Hole => match request.target {
            IoDataTarget::PageCache { frame, .. } => BackendPlan::Complete(
                PageCompletionList::from_vec(vec![
                    PageCompletion::new(
                        request.id,
                        request.range,
                        tx_subsystems::io_manager::page::PageIoResult::Done,
                        generation,
                        tx_subsystems::io_manager::page::PageIoCompletionKind::ReadInstalled,
                    )
                    .with_frame_ref(frame),
                ]),
            ),
            IoDataTarget::None => BackendPlan::Err(Errno::EINVAL),
        },
        Ext4ReadMapping::Data { physical_block } => match geometry.plan_read(physical_block, &request.target) {
            Ok(bio) => BackendPlan::SubmitBios(BioPlanList::from_vec(vec![bio])),
            Err(errno) => BackendPlan::Err(errno),
        },
        Ext4ReadMapping::MetadataFirst { resume } => BackendPlan::MetadataFirst {
            bios: BioPlanList::default(),
            resume,
        },
    }
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

    #[test]
    fn hole_read_completes_into_l4_zeroed_target() {
        let target = IoDataTarget::page_cache(
            IoDataLeaseId::new(2),
            PageFrameRef::new(Ppn(10)),
            0,
            BLOCK_SIZE as u32,
        );
        let request = BackendPageRequest::new_with_source_and_target(
            tx_subsystems::fs_iface::FsObjectKey::new(3),
            tx_subsystems::io_manager::page::PageIoRequestId::new(4),
            tx_subsystems::io_manager::page::PageIoRange::new(5, 1),
            tx_subsystems::io_manager::page::PageIoOp::Read,
            tx_subsystems::io_manager::page::PageIoFlags::DEMAND,
            Some(tx_subsystems::io_manager::page::PageGeneration::new(6)),
            tx_subsystems::fs_iface::IoDataSource::None,
            target,
        );
        let BackendPlan::Complete(completions) = plan_read_request(
            Ext4BlockGeometry::new(DeviceKey::new(7), 8),
            &request,
            Ext4ReadMapping::Hole,
        ) else {
            panic!("hole must complete without a device bio");
        };
        assert_eq!(completions.as_slice()[0].frame, Some(PageFrameRef::new(Ppn(10))));
    }

    struct FixedMapping {
        mapping: Ext4ReadMapping,
    }

    impl Ext4ReadMappingSource for FixedMapping {
        fn map_page(&self, _request: &BackendPageRequest) -> Ext4ReadMapping {
            self.mapping
        }
    }

    #[test]
    fn ext4_read_planner_delegates_mapping_without_owning_io() {
        let planner = Ext4ReadPlanner::new(
            Ext4BlockGeometry::new(DeviceKey::new(7), 8),
            FixedMapping {
                mapping: Ext4ReadMapping::Hole,
            },
        );
        let request = BackendPageRequest::new_with_source_and_target(
            tx_subsystems::fs_iface::FsObjectKey::new(3),
            tx_subsystems::io_manager::page::PageIoRequestId::new(4),
            tx_subsystems::io_manager::page::PageIoRange::new(5, 1),
            tx_subsystems::io_manager::page::PageIoOp::Read,
            tx_subsystems::io_manager::page::PageIoFlags::DEMAND,
            Some(tx_subsystems::io_manager::page::PageGeneration::new(6)),
            tx_subsystems::fs_iface::IoDataSource::None,
            IoDataTarget::page_cache(
                IoDataLeaseId::new(2),
                PageFrameRef::new(Ppn(10)),
                0,
                BLOCK_SIZE as u32,
            ),
        );
        let plan = planner.plan_page_io(request);
        assert!(matches!(plan, BackendPlan::Complete(_)));
    }
}
