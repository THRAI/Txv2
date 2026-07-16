//! Ext4-to-L6 block-plan helpers.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec;

use tx_subsystems::execution::{Errno, Guard};
use tx_subsystems::fs_iface::{
    BackendBioGraph, BackendPageCompletion, BackendPageRequest, BackendPlan, BackendPlanner, BioPlanList,
    IoDataSource, IoDataTarget, PageCompletion, PageCompletionList, PageFrameRef, PagerResumeToken,
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

/// Journal-owned fsync planning for one mounted ext4 instance.
///
/// The source retains prepared JBD2 records and their page leases while L6
/// executes the graph. The generic page mapper only asks for the graph; it
/// never owns journal state or performs I/O under its metadata lock.
pub trait Ext4FsyncPlanSource: Send + Sync + 'static {
    fn plan_fsync(&self, request: &BackendPageRequest) -> BackendPlan;

    fn complete_fsync(&self, _completion: BackendPageCompletion) {}

    fn take_background_graph(&self) -> Result<Option<BackendBioGraph>, Errno> {
        Ok(None)
    }

    fn complete_background_graph(&self, _result: Result<(), Errno>) {}
}

/// Ext4-owned writeback planning for one L4-retained data source.
///
/// The default covers existing mapped blocks. A mount-owned implementation
/// may atomically stage bitmap/inode/extent metadata for holes without giving
/// the generic planner access to live pager state.
pub trait Ext4WritePlanSource: Send + Sync + 'static {
    /// Stage backend-private writeback state while L4 still owns the source
    /// lease and its epoch guard. Implementations must not retain `guard`.
    fn prepare_writeback(
        &self,
        _request: &BackendPageRequest,
        _guard: &Guard<'_>,
    ) -> Result<(), Errno> {
        Ok(())
    }

    fn plan_writeback(
        &self,
        geometry: Ext4BlockGeometry,
        request: &BackendPageRequest,
        mapping: Ext4ReadMapping,
    ) -> BackendPlan;

    fn complete_writeback(&self, _completion: BackendPageCompletion) {}
}

#[derive(Clone, Copy, Debug, Default)]
pub struct MappedExt4WritePlan;

impl Ext4WritePlanSource for MappedExt4WritePlan {
    fn plan_writeback(
        &self,
        geometry: Ext4BlockGeometry,
        request: &BackendPageRequest,
        mapping: Ext4ReadMapping,
    ) -> BackendPlan {
        plan_writeback_request(geometry, request, mapping)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct UnsupportedExt4FsyncPlan;

impl Ext4FsyncPlanSource for UnsupportedExt4FsyncPlan {
    fn plan_fsync(&self, _request: &BackendPageRequest) -> BackendPlan {
        BackendPlan::Err(Errno::ENOSYS)
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

pub struct Ext4ReadPlanner<S, J = UnsupportedExt4FsyncPlan, W = MappedExt4WritePlan> {
    geometry: Ext4BlockGeometry,
    mapping: S,
    fsync: J,
    writeback: W,
}

/// Shared ext4 L5 planner state for one mounted filesystem.
///
/// The mapping table is held independently from the trait object so the
/// metadata owner can seed inode roots while L4 holds only `BackendPlanner`.
#[derive(Clone)]
pub struct Ext4PlannerBinding {
    planner: Arc<dyn BackendPlanner>,
    mapping: Arc<Ext4MappingTable>,
}

impl Ext4PlannerBinding {
    pub fn new(geometry: Ext4BlockGeometry) -> Self {
        let mapping = Arc::new(Ext4MappingTable::new());
        let planner: Arc<dyn BackendPlanner> =
            Arc::new(Ext4ReadPlanner::new(geometry, Arc::clone(&mapping)));
        Self { planner, mapping }
    }

    pub fn with_fsync_plan_source<J>(geometry: Ext4BlockGeometry, fsync: J) -> Self
    where
        J: Ext4FsyncPlanSource,
    {
        let mapping = Arc::new(Ext4MappingTable::new());
        let planner: Arc<dyn BackendPlanner> = Arc::new(Ext4ReadPlanner::with_fsync_plan_source(
            geometry,
            Arc::clone(&mapping),
            fsync,
        ));
        Self { planner, mapping }
    }

    pub fn with_plan_sources<J, W>(geometry: Ext4BlockGeometry, fsync: J, writeback: W) -> Self
    where
        J: Ext4FsyncPlanSource,
        W: Ext4WritePlanSource,
    {
        let mapping = Arc::new(Ext4MappingTable::new());
        let planner: Arc<dyn BackendPlanner> = Arc::new(Ext4ReadPlanner::with_plan_sources(
            geometry,
            Arc::clone(&mapping),
            fsync,
            writeback,
        ));
        Self { planner, mapping }
    }

    pub fn planner(&self) -> Arc<dyn BackendPlanner> {
        Arc::clone(&self.planner)
    }

    pub fn mapping(&self) -> Arc<Ext4MappingTable> {
        Arc::clone(&self.mapping)
    }
}

impl<S> Ext4ReadPlanner<S> {
    pub const fn new(geometry: Ext4BlockGeometry, mapping: S) -> Self {
        Self {
            geometry,
            mapping,
            fsync: UnsupportedExt4FsyncPlan,
            writeback: MappedExt4WritePlan,
        }
    }
}

impl<S, J> Ext4ReadPlanner<S, J> {
    pub const fn with_fsync_plan_source(geometry: Ext4BlockGeometry, mapping: S, fsync: J) -> Self {
        Self {
            geometry,
            mapping,
            fsync,
            writeback: MappedExt4WritePlan,
        }
    }
}

impl<S, J, W> Ext4ReadPlanner<S, J, W> {
    pub const fn with_plan_sources(
        geometry: Ext4BlockGeometry,
        mapping: S,
        fsync: J,
        writeback: W,
    ) -> Self {
        Self {
            geometry,
            mapping,
            fsync,
            writeback,
        }
    }
}

impl Ext4ReadPlanner<Ext4MappingTable> {
    pub fn with_mapping_table(geometry: Ext4BlockGeometry, mapping: Ext4MappingTable) -> Self {
        Self::new(geometry, mapping)
    }
}

impl Ext4ReadMappingSource for Arc<Ext4MappingTable> {
    fn map_page(&self, request: &BackendPageRequest) -> Ext4ReadMapping {
        self.as_ref().map_page(request)
    }

    fn resume_metadata(&self, request: &BackendPageRequest) -> Result<(), Errno> {
        self.as_ref().resume_metadata(request)
    }
}

impl<S: Ext4ReadMappingSource, J: Ext4FsyncPlanSource, W: Ext4WritePlanSource> BackendPlanner
    for Ext4ReadPlanner<S, J, W>
{
    fn prepare_page_io(
        &self,
        request: &BackendPageRequest,
        guard: &Guard<'_>,
    ) -> Result<(), Errno> {
        if request.op == tx_subsystems::io_manager::page::PageIoOp::Writeback {
            self.writeback.prepare_writeback(request, guard)
        } else {
            Ok(())
        }
    }

    fn plan_page_io(&self, request: BackendPageRequest) -> BackendPlan {
        match request.op {
            tx_subsystems::io_manager::page::PageIoOp::Read
            | tx_subsystems::io_manager::page::PageIoOp::Readahead => {
                plan_read_request(self.geometry, &request, self.mapping.map_page(&request))
            }
            tx_subsystems::io_manager::page::PageIoOp::Writeback => self.writeback.plan_writeback(
                self.geometry,
                &request,
                self.mapping.map_page(&request),
            ),
            tx_subsystems::io_manager::page::PageIoOp::Fsync => self.fsync.plan_fsync(&request),
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

    fn complete_page_io(&self, completion: BackendPageCompletion) {
        match completion.op {
            tx_subsystems::io_manager::page::PageIoOp::Writeback => {
                self.writeback.complete_writeback(completion);
            }
            tx_subsystems::io_manager::page::PageIoOp::Fsync => {
                self.fsync.complete_fsync(completion);
            }
            _ => {}
        }
    }

    fn take_background_graph(
        &self,
        _object: tx_subsystems::fs_iface::FsObjectKey,
    ) -> Result<Option<BackendBioGraph>, Errno> {
        self.fsync.take_background_graph()
    }

    fn complete_background_graph(
        &self,
        _object: tx_subsystems::fs_iface::FsObjectKey,
        result: Result<(), Errno>,
    ) {
        self.fsync.complete_background_graph(result);
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
                BackendPlan::Complete(PageCompletionList::from_vec(vec![PageCompletion::new(
                    request.id,
                    request.range,
                    tx_subsystems::io_manager::page::PageIoResult::Done,
                    generation,
                    tx_subsystems::io_manager::page::PageIoCompletionKind::ReadInstalled,
                )
                .with_frame_ref(frame)]))
            }
            IoDataTarget::Direct { .. } => BackendPlan::Err(Errno::ENOSYS),
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

/// Translate an already-mapped dirty page into one L6 data write.
///
/// L4 retains the source lease and releases it only after the matching
/// generation-checked writeback completion. Allocation, hole conversion, and
/// journal ordering deliberately stay out of this data-only plan until the
/// Phase 6D metadata graph and Phase 6E durability fence exist.
pub fn plan_writeback_request(
    geometry: Ext4BlockGeometry,
    request: &BackendPageRequest,
    mapping: Ext4ReadMapping,
) -> BackendPlan {
    match mapping {
        Ext4ReadMapping::Data { physical_block } => {
            match geometry.plan_write(physical_block, &request.source) {
                Ok(bio) => BackendPlan::SubmitBios(BioPlanList::from_vec(vec![bio])),
                Err(errno) => BackendPlan::Err(errno),
            }
        }
        Ext4ReadMapping::Hole | Ext4ReadMapping::MetadataFirst { .. } => {
            BackendPlan::Err(Errno::ENOSYS)
        }
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
        let vecs = match target {
            IoDataTarget::PageCache {
                frame, offset, len, ..
            } => {
                if *len != BLOCK_SIZE as u32 {
                    return Err(Errno::EINVAL);
                }
                vec![bio_vec(*frame, *offset, *len)]
            }
            IoDataTarget::Direct { vecs, .. } => direct_bio_vecs(vecs)?,
            IoDataTarget::None => return Err(Errno::EINVAL),
        };
        if self.sectors_per_block == 0 {
            return Err(Errno::EINVAL);
        }
        let lba = physical_block
            .checked_mul(self.sectors_per_block)
            .ok_or(Errno::EINVAL)?;
        Ok(BioPlan::new(
            self.device,
            BlockOp::Read,
            LbaRange::new(lba, self.sectors_per_block),
            vecs,
            BlockFlags::EMPTY,
        ))
    }

    /// Build one L6 write bio for an L4-owned dirty page-cache source.
    pub fn plan_write(self, physical_block: u64, source: &IoDataSource) -> Result<BioPlan, Errno> {
        let vecs = match source {
            IoDataSource::PageCache {
                frame, offset, len, ..
            } => {
                if *len != BLOCK_SIZE as u32 {
                    return Err(Errno::EINVAL);
                }
                vec![bio_vec(*frame, *offset, *len)]
            }
            IoDataSource::Direct { vecs, .. } => direct_bio_vecs(vecs)?,
            IoDataSource::None => return Err(Errno::EINVAL),
        };
        if self.sectors_per_block == 0 {
            return Err(Errno::EINVAL);
        }
        let lba = physical_block
            .checked_mul(self.sectors_per_block)
            .ok_or(Errno::EINVAL)?;
        Ok(BioPlan::new(
            self.device,
            BlockOp::Write,
            LbaRange::new(lba, self.sectors_per_block),
            vecs,
            BlockFlags::EMPTY,
        ))
    }
}

fn bio_vec(frame: PageFrameRef, offset: u32, len: u32) -> BioVec {
    BioVec::new(frame.ppn().0 as u64, offset, len)
}

fn direct_bio_vecs(vecs: &[BioVec]) -> Result<alloc::vec::Vec<BioVec>, Errno> {
    if vecs.is_empty() {
        return Err(Errno::EINVAL);
    }
    let mut total = 0u32;
    for vec in vecs {
        if vec.len == 0 {
            return Err(Errno::EINVAL);
        }
        total = total.checked_add(vec.len).ok_or(Errno::EINVAL)?;
    }
    if total != BLOCK_SIZE as u32 {
        return Err(Errno::EINVAL);
    }
    Ok(vecs.to_vec())
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
    fn mapped_ext4_block_plans_l6_read_into_direct_iovecs() {
        let target = IoDataTarget::direct(
            IoDataLeaseId::new(11),
            vec![BioVec::new(41, 128, 2048), BioVec::new(42, 0, 2048)],
        );
        let plan = Ext4BlockGeometry::new(DeviceKey::new(7), 8)
            .plan_read(11, &target)
            .expect("mapped direct block plan");

        assert_eq!(plan.device, DeviceKey::new(7));
        assert_eq!(plan.lba, LbaRange::new(88, 8));
        assert_eq!(
            plan.vecs,
            vec![BioVec::new(41, 128, 2048), BioVec::new(42, 0, 2048)]
        );
    }

    #[test]
    fn mapped_ext4_direct_read_rejects_empty_or_partial_iovecs() {
        let geometry = Ext4BlockGeometry::new(DeviceKey::new(7), 8);
        for vecs in [
            alloc::vec![],
            alloc::vec![BioVec::new(41, 0, (BLOCK_SIZE as u32) - 1)],
        ] {
            let target = IoDataTarget::direct(IoDataLeaseId::new(12), vecs);
            assert_eq!(geometry.plan_read(11, &target), Err(Errno::EINVAL));
        }
    }

    #[test]
    fn mapped_ext4_block_plans_l6_write_from_page_cache_source() {
        let source = IoDataSource::page_cache(
            IoDataLeaseId::new(1),
            PageFrameRef::new(Ppn(9)),
            0,
            BLOCK_SIZE as u32,
        );
        let plan = Ext4BlockGeometry::new(DeviceKey::new(7), 8)
            .plan_write(11, &source)
            .expect("mapped block write plan");
        assert_eq!(plan.device, DeviceKey::new(7));
        assert_eq!(plan.op, BlockOp::Write);
        assert_eq!(plan.lba, LbaRange::new(88, 8));
        assert_eq!(plan.vecs, vec![BioVec::new(9, 0, BLOCK_SIZE as u32)]);
    }

    #[test]
    fn mapped_ext4_block_plans_l6_write_from_direct_iovecs() {
        let source = IoDataSource::direct(
            IoDataLeaseId::new(13),
            vec![BioVec::new(51, 64, 2048), BioVec::new(52, 0, 2048)],
        );
        let plan = Ext4BlockGeometry::new(DeviceKey::new(7), 8)
            .plan_write(11, &source)
            .expect("mapped direct block write plan");

        assert_eq!(plan.device, DeviceKey::new(7));
        assert_eq!(plan.op, BlockOp::Write);
        assert_eq!(plan.lba, LbaRange::new(88, 8));
        assert_eq!(
            plan.vecs,
            vec![BioVec::new(51, 64, 2048), BioVec::new(52, 0, 2048)]
        );
    }

    #[test]
    fn mapped_ext4_direct_write_rejects_empty_or_partial_iovecs() {
        let geometry = Ext4BlockGeometry::new(DeviceKey::new(7), 8);
        for vecs in [
            alloc::vec![],
            alloc::vec![BioVec::new(51, 0, (BLOCK_SIZE as u32) - 1)],
        ] {
            let source = IoDataSource::direct(IoDataLeaseId::new(14), vecs);
            assert_eq!(geometry.plan_write(11, &source), Err(Errno::EINVAL));
        }
    }

    #[test]
    fn writeback_plan_rejects_hole_until_metadata_graph_exists() {
        let request = BackendPageRequest::new_with_source_and_target(
            tx_subsystems::fs_iface::FsObjectKey::new(3),
            tx_subsystems::io_manager::page::PageIoRequestId::new(4),
            tx_subsystems::io_manager::page::PageIoRange::new(5, 1),
            tx_subsystems::io_manager::page::PageIoOp::Writeback,
            tx_subsystems::io_manager::page::PageIoFlags::WRITEBACK,
            Some(tx_subsystems::io_manager::page::PageGeneration::new(6)),
            IoDataSource::page_cache(
                IoDataLeaseId::new(2),
                PageFrameRef::new(Ppn(10)),
                0,
                BLOCK_SIZE as u32,
            ),
            IoDataTarget::None,
        );
        assert_eq!(
            plan_writeback_request(
                Ext4BlockGeometry::new(DeviceKey::new(7), 8),
                &request,
                Ext4ReadMapping::Hole,
            ),
            BackendPlan::Err(Errno::ENOSYS)
        );
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

    #[test]
    fn hole_direct_read_rejects_until_l4_installs_a_zero_fill_adapter() {
        let target = IoDataTarget::direct(
            IoDataLeaseId::new(2),
            vec![BioVec::new(10, 0, BLOCK_SIZE as u32)],
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

        assert_eq!(
            plan_read_request(
                Ext4BlockGeometry::new(DeviceKey::new(7), 8),
                &request,
                Ext4ReadMapping::Hole,
            ),
            BackendPlan::Err(Errno::ENOSYS)
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

    struct GraphFsyncSource;

    impl Ext4FsyncPlanSource for GraphFsyncSource {
        fn plan_fsync(&self, _request: &BackendPageRequest) -> BackendPlan {
            let graph = tx_subsystems::fs_iface::BackendBioGraph::new(vec![], vec![])
                .expect("empty graph is a valid already-complete transaction");
            BackendPlan::SubmitGraph(graph)
        }
    }

    struct RejectingWriteSource;

    impl Ext4WritePlanSource for RejectingWriteSource {
        fn plan_writeback(
            &self,
            _geometry: Ext4BlockGeometry,
            _request: &BackendPageRequest,
            _mapping: Ext4ReadMapping,
        ) -> BackendPlan {
            BackendPlan::Err(Errno::EIO)
        }
    }

    #[test]
    fn ext4_writeback_planner_delegates_to_mount_owned_write_source() {
        let planner = Ext4ReadPlanner::with_plan_sources(
            Ext4BlockGeometry::new(DeviceKey::new(7), 8),
            FixedMapping {
                mapping: Ext4ReadMapping::Hole,
            },
            UnsupportedExt4FsyncPlan,
            RejectingWriteSource,
        );
        let request = BackendPageRequest::new_with_source_and_target(
            tx_subsystems::fs_iface::FsObjectKey::new(3),
            tx_subsystems::io_manager::page::PageIoRequestId::new(4),
            tx_subsystems::io_manager::page::PageIoRange::new(5, 1),
            tx_subsystems::io_manager::page::PageIoOp::Writeback,
            tx_subsystems::io_manager::page::PageIoFlags::WRITEBACK,
            Some(tx_subsystems::io_manager::page::PageGeneration::new(6)),
            IoDataSource::page_cache(
                IoDataLeaseId::new(2),
                PageFrameRef::new(Ppn(10)),
                0,
                BLOCK_SIZE as u32,
            ),
            IoDataTarget::None,
        );
        assert_eq!(planner.plan_page_io(request), BackendPlan::Err(Errno::EIO));
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
    fn ext4_fsync_planner_delegates_prepared_journal_graph_to_l4() {
        let planner = Ext4ReadPlanner::with_fsync_plan_source(
            Ext4BlockGeometry::new(DeviceKey::new(7), 8),
            FixedMapping {
                mapping: Ext4ReadMapping::Hole,
            },
            GraphFsyncSource,
        );
        let request = BackendPageRequest::new_with_source_and_target(
            tx_subsystems::fs_iface::FsObjectKey::new(3),
            tx_subsystems::io_manager::page::PageIoRequestId::new(4),
            tx_subsystems::io_manager::page::PageIoRange::new(0, 1),
            tx_subsystems::io_manager::page::PageIoOp::Fsync,
            tx_subsystems::io_manager::page::PageIoFlags::BARRIER,
            None,
            IoDataSource::None,
            IoDataTarget::None,
        );

        assert!(matches!(
            planner.plan_page_io(request),
            BackendPlan::SubmitGraph(_)
        ));
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
