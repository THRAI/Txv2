//! Ext4-to-L6 block-plan helpers.

use alloc::collections::BTreeMap;
use alloc::vec;

use tx_subsystems::execution::Errno;
use tx_subsystems::fs_iface::{
    BackendPageRequest, BackendPlan, BackendPlanner, BioPlanList, IoDataTarget, PageCompletion,
    PageCompletionList, PageFrameRef, PagerResumeToken,
};
use tx_subsystems::io_manager::block::{BioPlan, BioVec, BlockFlags, BlockOp, DeviceKey, LbaRange};

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

/// Concurrent mapping rows owned by the ext4 metadata path.
///
/// A miss is intentionally represented as `MetadataFirst` rather than as a
/// synchronous pager call. The metadata service fills the row after its L6
/// completion, then retries the original page request.
pub struct Ext4MappingTable {
    rows: crate::sync::SpinMutex<BTreeMap<(u64, u64), Ext4ReadMapping>>,
}

impl Ext4MappingTable {
    pub fn new() -> Self {
        Self {
            rows: crate::sync::SpinMutex::new(BTreeMap::new()),
        }
    }

    pub fn insert(&self, object: u64, page: u64, mapping: Ext4ReadMapping) {
        self.rows.lock().insert((object, page), mapping);
    }

    pub fn len(&self) -> usize {
        self.rows.lock().len()
    }

    fn resume_token(object: u64, page: u64) -> PagerResumeToken {
        PagerResumeToken::new(object.rotate_left(17) ^ page.rotate_left(31))
    }
}

impl Default for Ext4MappingTable {
    fn default() -> Self {
        Self::new()
    }
}

impl Ext4ReadMappingSource for Ext4MappingTable {
    fn map_page(&self, request: &BackendPageRequest) -> Ext4ReadMapping {
        let key = (request.object.raw(), request.range.start_page());
        self.rows
            .lock()
            .get(&key)
            .copied()
            .unwrap_or_else(|| Ext4ReadMapping::MetadataFirst {
                resume: Self::resume_token(key.0, key.1),
            })
    }
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

impl Ext4ReadPlanner<Ext4MappingTable> {
    pub fn with_mapping_table(geometry: Ext4BlockGeometry, mapping: Ext4MappingTable) -> Self {
        Self::new(geometry, mapping)
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
            IoDataTarget::PageCache { frame, .. } => {
                BackendPlan::Complete(PageCompletionList::from_vec(vec![
                    PageCompletion::new(
                        request.id,
                        request.range,
                        tx_subsystems::io_manager::page::PageIoResult::Done,
                        generation,
                        tx_subsystems::io_manager::page::PageIoCompletionKind::ReadInstalled,
                    )
                    .with_frame_ref(frame),
                ]))
            }
            IoDataTarget::None => BackendPlan::Err(Errno::EINVAL),
        },
        Ext4ReadMapping::Data { physical_block } => {
            match geometry.plan_read(physical_block, &request.target) {
                Ok(bio) => BackendPlan::SubmitBios(BioPlanList::from_vec(vec![bio])),
                Err(errno) => BackendPlan::Err(errno),
            }
        }
        Ext4ReadMapping::MetadataFirst { resume } => BackendPlan::MetadataFirst {
            bios: BioPlanList::default(),
            resume,
        },
    }
}

impl Ext4BlockGeometry {
    pub const fn new(device: DeviceKey, sectors_per_block: u64) -> Self {
        Self {
            device,
            sectors_per_block,
        }
    }

    /// Build one L6 read bio for a mapped ext4 data block and an L4-owned target.
    pub fn plan_read(self, physical_block: u64, target: &IoDataTarget) -> Result<BioPlan, Errno> {
        let IoDataTarget::PageCache {
            frame, offset, len, ..
        } = target
        else {
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
        assert_eq!(
            completions.as_slice()[0].frame,
            Some(PageFrameRef::new(Ppn(10)))
        );
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

    #[test]
    fn mapping_table_returns_metadata_first_then_reuses_filled_row() {
        let table = Ext4MappingTable::new();
        let planner = Ext4ReadPlanner::with_mapping_table(
            Ext4BlockGeometry::new(DeviceKey::new(7), 8),
            table,
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
        assert!(matches!(
            planner.plan_page_io(request.clone()),
            BackendPlan::MetadataFirst { .. }
        ));
        planner.mapping.insert(3, 5, Ext4ReadMapping::Hole);
        assert!(matches!(
            planner.plan_page_io(request),
            BackendPlan::Complete(_)
        ));
    }
}
