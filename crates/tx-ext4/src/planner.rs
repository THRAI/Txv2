//! Ext4-to-L6 block-plan helpers.

use alloc::collections::BTreeMap;
use alloc::vec;

use tx_subsystems::execution::Errno;
use tx_subsystems::fs_iface::{
    BackendPageRequest, BackendPlan, BackendPlanner, BioPlanList, IoDataTarget, PageCompletion,
    PageCompletionList, PageFrameRef, PagerResumeToken,
};
use tx_subsystems::io_manager::block::{BioPlan, BioVec, BlockFlags, BlockOp, DeviceKey, LbaRange};

use tx_ext4_format::mapping::map_extent_root;
use tx_ext4_format::ondisk::BlockMapping;
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
    Data {
        physical_block: u64,
    },
    MetadataFirst {
        physical_block: u64,
        resume: PagerResumeToken,
    },
    Err(Errno),
}

/// Mapping lookup supplied by the ext4 metadata owner.
///
/// The provider may return `MetadataFirst` when an extent index block must be
/// fetched. It never performs block I/O inside this callback.
pub trait Ext4ReadMappingSource: Send + Sync + 'static {
    fn map_page(&self, request: &BackendPageRequest) -> Ext4ReadMapping;

    /// Consume a completed metadata read before replanning its original page.
    ///
    /// The default is suitable for sources that own their metadata elsewhere.
    /// `Ext4MappingTable` reads the completed child extent node from the
    /// L4-owned target frame and caches it before the next lookup.
    fn resume_metadata(&self, _request: &BackendPageRequest) -> Result<(), Errno> {
        Ok(())
    }
}

/// Concurrent extent roots and child-node cache owned by the ext4 metadata path.
///
/// An uncached extent child becomes `MetadataFirst`; its L6 completion fills
/// the child-node cache from the L4-owned target frame, then retries the
/// original page request without a synchronous pager call.
pub struct Ext4MappingTable {
    state: crate::sync::SpinMutex<Ext4MappingTableState>,
}

#[derive(Default)]
struct Ext4MappingTableState {
    rows: BTreeMap<(u64, u64), Ext4ReadMapping>,
    roots: BTreeMap<u64, alloc::vec::Vec<u8>>,
    nodes: BTreeMap<(u64, u64), alloc::vec::Vec<u8>>,
}

impl Ext4MappingTable {
    pub fn new() -> Self {
        Self {
            state: crate::sync::SpinMutex::new(Ext4MappingTableState::default()),
        }
    }

    pub fn insert(&self, object: u64, page: u64, mapping: Ext4ReadMapping) {
        self.state.lock().rows.insert((object, page), mapping);
    }

    /// Install the inline extent root acquired while resolving an inode.
    ///
    /// The caller may obtain this metadata through the compatibility pager
    /// during mount/lookup today. Once installed, page reads below this root
    /// do not call that pager again.
    pub fn insert_extent_root(&self, object: u64, root: &[u8]) -> Result<(), Errno> {
        map_extent_root(root, 0).map_err(crate::read_backend::map_format_error)?;
        self.state.lock().roots.insert(object, root.to_vec());
        Ok(())
    }

    /// Insert a completed child extent node. This accepts an ordinary byte
    /// slice so mount-time metadata seeding and L6 completion share validation.
    pub fn insert_extent_node(
        &self,
        object: u64,
        physical_block: u64,
        node: &[u8],
    ) -> Result<(), Errno> {
        if node.len() < BLOCK_SIZE {
            return Err(Errno::EINVAL);
        }
        map_extent_root(&node[..BLOCK_SIZE], 0).map_err(crate::read_backend::map_format_error)?;
        self.state
            .lock()
            .nodes
            .insert((object, physical_block), node[..BLOCK_SIZE].to_vec());
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.state.lock().rows.len()
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
        let object = request.object.raw();
        let page = request.range.start_page();
        let state = self.state.lock();
        if let Some(mapping) = state.rows.get(&(object, page)).copied() {
            return mapping;
        }
        let Some(root) = state.roots.get(&object) else {
            return Ext4ReadMapping::Err(Errno::ENOSYS);
        };
        let Ok(logical_block) = u32::try_from(page) else {
            return Ext4ReadMapping::Err(Errno::EINVAL);
        };
        let mut node = root.as_slice();
        for _ in 0..8 {
            match map_extent_root(node, logical_block) {
                Ok(BlockMapping::Data(physical_block)) => {
                    return Ext4ReadMapping::Data { physical_block };
                }
                Ok(BlockMapping::Hole) => return Ext4ReadMapping::Hole,
                Ok(BlockMapping::NeedNode(physical_block)) => {
                    let Some(cached) = state.nodes.get(&(object, physical_block)) else {
                        return Ext4ReadMapping::MetadataFirst {
                            physical_block,
                            resume: Self::resume_token(object, page),
                        };
                    };
                    node = cached.as_slice();
                }
                Err(error) => {
                    return Ext4ReadMapping::Err(crate::read_backend::map_format_error(error));
                }
            }
        }
        Ext4ReadMapping::Err(Errno::EINVAL)
    }

    fn resume_metadata(&self, request: &BackendPageRequest) -> Result<(), Errno> {
        let Ext4ReadMapping::MetadataFirst { physical_block, .. } = self.map_page(request) else {
            return Ok(());
        };
        let IoDataTarget::PageCache {
            frame, offset, len, ..
        } = request.target
        else {
            return Err(Errno::EINVAL);
        };
        if offset != 0 || len != BLOCK_SIZE as u32 {
            return Err(Errno::EINVAL);
        }
        let source =
            tx_substrate::page_allocator::frame_kernel_addr(frame.ppn()).map_err(|_| Errno::EIO)?;
        // The L4 lease keeps the target frame alive until this continuation
        // has parsed the metadata block and returned a replacement plan.
        let bytes = unsafe { core::slice::from_raw_parts(source, BLOCK_SIZE) };
        self.insert_extent_node(request.object.raw(), physical_block, bytes)
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

    fn resume_page_io(&self, resume: tx_subsystems::fs_iface::BackendPlanResume) -> BackendPlan {
        if let Some(errno) = resume
            .completions
            .iter()
            .find_map(|completion| completion.result.err())
        {
            return BackendPlan::Err(errno);
        }
        let Some(request) = resume.request else {
            return BackendPlan::Err(Errno::EINVAL);
        };
        if let Err(errno) = self.mapping.resume_metadata(&request) {
            return BackendPlan::Err(errno);
        }
        self.plan_page_io(request)
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
        Ext4ReadMapping::MetadataFirst {
            physical_block,
            resume,
        } => match geometry.plan_read(physical_block, &request.target) {
            Ok(bio) => BackendPlan::MetadataFirst {
                request: request.clone(),
                bios: BioPlanList::from_vec(vec![bio]),
                resume,
            },
            Err(errno) => BackendPlan::Err(errno),
        },
        Ext4ReadMapping::Err(errno) => BackendPlan::Err(errno),
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
    use tx_ext4_format::ondisk::{Extent, ExtentIdx, ExtentNode};
    use tx_hal::Ppn;
    use tx_subsystems::fs_iface::IoDataLeaseId;

    fn request(target: IoDataTarget) -> BackendPageRequest {
        BackendPageRequest::new_with_source_and_target(
            tx_subsystems::fs_iface::FsObjectKey::new(3),
            tx_subsystems::io_manager::page::PageIoRequestId::new(4),
            tx_subsystems::io_manager::page::PageIoRange::new(5, 1),
            tx_subsystems::io_manager::page::PageIoOp::Read,
            tx_subsystems::io_manager::page::PageIoFlags::DEMAND,
            Some(tx_subsystems::io_manager::page::PageGeneration::new(6)),
            tx_subsystems::fs_iface::IoDataSource::None,
            target,
        )
    }

    fn indexed_root(child: u64) -> [u8; 60] {
        let mut root = [0; 60];
        ExtentNode::encode_index(
            1,
            &[ExtentIdx {
                logical_block: 0,
                child,
            }],
            &mut root,
        )
        .expect("extent index root");
        root
    }

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
        planner
            .mapping
            .insert_extent_root(3, &indexed_root(12))
            .expect("install inline extent root");
        let request = request(IoDataTarget::page_cache(
            IoDataLeaseId::new(2),
            PageFrameRef::new(Ppn(10)),
            0,
            BLOCK_SIZE as u32,
        ));
        let BackendPlan::MetadataFirst { bios, .. } = planner.plan_page_io(request.clone()) else {
            panic!("uncached child extent node must submit metadata bio");
        };
        assert_eq!(bios.as_slice()[0].lba, LbaRange::new(96, 8));

        let mut leaf = [0; BLOCK_SIZE];
        ExtentNode::encode_leaf(
            &[Extent {
                logical_block: 5,
                len: 1,
                physical_start: 100,
            }],
            &mut leaf,
        )
        .expect("extent leaf");
        planner
            .mapping
            .insert_extent_node(3, 12, &leaf)
            .expect("cache extent child");
        let BackendPlan::SubmitBios(bios) = planner.plan_page_io(request) else {
            panic!("cached child extent node must replan file-data bio");
        };
        assert_eq!(bios.as_slice()[0].lba, LbaRange::new(800, 8));
    }

    #[test]
    fn metadata_resume_replans_after_mapping_owner_fills_row() {
        tx_test_support::init_host();
        let table = Ext4MappingTable::new();
        let planner = Ext4ReadPlanner::with_mapping_table(
            Ext4BlockGeometry::new(DeviceKey::new(7), 8),
            table,
        );
        planner
            .mapping
            .insert_extent_root(3, &indexed_root(12))
            .expect("install inline extent root");
        let owned = tx_substrate::page_allocator::reserve_frame(
            tx_substrate::page_allocator::ZeroPolicy::Zeroed,
        )
        .expect("metadata target frame")
        .commit();
        let ppn = owned.ppn();
        let request = request(IoDataTarget::page_cache(
            IoDataLeaseId::new(2),
            PageFrameRef::new(ppn),
            0,
            BLOCK_SIZE as u32,
        ));
        let BackendPlan::MetadataFirst { resume, .. } = planner.plan_page_io(request.clone())
        else {
            panic!("uncached child extent node must defer through metadata");
        };

        let mut leaf = [0; BLOCK_SIZE];
        ExtentNode::encode_leaf(
            &[Extent {
                logical_block: 5,
                len: 1,
                physical_start: 100,
            }],
            &mut leaf,
        )
        .expect("extent leaf");
        let target = tx_substrate::page_allocator::frame_kernel_addr(ppn)
            .expect("metadata target direct map");
        unsafe {
            core::ptr::copy_nonoverlapping(leaf.as_ptr(), target, BLOCK_SIZE);
        }
        let resumed =
            planner.resume_page_io(tx_subsystems::fs_iface::BackendPlanResume::with_request(
                resume,
                alloc::vec![tx_subsystems::fs_iface::BackendBioCompletion::new(
                    tx_subsystems::fs_iface::BackendBioNodeId::new(1),
                    Ok(()),
                )],
                request,
            ));

        let BackendPlan::SubmitBios(bios) = resumed else {
            panic!("metadata completion must replan the file-data bio");
        };
        assert_eq!(bios.as_slice()[0].lba, LbaRange::new(800, 8));
        drop(owned);
    }
}
