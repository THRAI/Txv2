use alloc::vec;

use tx_ext4_format::pager::{BlockImage, Page4K, BLOCK_SIZE};
use tx_subsystems::execution::Errno;
use tx_subsystems::fs_iface::{
    BackendBioGraph, BackendBioNode, BackendBioNodeId, BackendPageCompletion, BackendPageRequest,
    BackendPlan, FsObjectKey, IoDataSource,
};
use tx_subsystems::io_manager::block::BlockOp;
use tx_subsystems::io_manager::page::{
    PageIoFlags, PageIoOp, PageIoRange, PageIoRequestId, PageIoResult,
};

use crate::adapter::step_engine::page_allocator;
use crate::journal::JournalMutationRuntime;
use crate::planner::Ext4FsyncPlanSource;
use crate::read_backend::Ext4FsInstance;

impl<I> Ext4FsInstance<I>
where
    I: BlockImage + Send + 'static,
{
    pub(crate) fn settle_metadata_mutation(
        &self,
        runtime: &JournalMutationRuntime,
    ) -> Result<(), Errno> {
        let source = runtime.source();
        let request = metadata_fsync_request();
        let commit = match source.plan_fsync(&request) {
            BackendPlan::SubmitGraph(graph) => graph,
            BackendPlan::Err(Errno::EAGAIN) => {
                self.settle_ordered_data(source.as_ref())?;
                match source.plan_fsync(&request) {
                    BackendPlan::SubmitGraph(graph) => graph,
                    BackendPlan::Err(errno) => return Err(errno),
                    _ => return Err(Errno::EIO),
                }
            }
            BackendPlan::Err(errno) => return Err(errno),
            _ => return Err(Errno::EIO),
        };
        if let Err(errno) = self.execute_bio_graph(commit) {
            source.commit_unknown(request.id, errno);
            return Err(errno);
        }
        source.complete_fsync(BackendPageCompletion::new(
            request.object,
            request.id,
            request.op,
            PageIoResult::Done,
        ));

        let checkpoint = source
            .take_checkpoint_graph()
            .map_err(|error| match error {
                crate::journal::JournalTransactionStateError::Busy => Errno::EBUSY,
                _ => Errno::EIO,
            })?;
        let Some(checkpoint) = checkpoint else {
            return Err(Errno::EIO);
        };
        if let Err(errno) = self.execute_bio_graph(checkpoint) {
            source.complete_checkpoint_result(Err(errno)).ok();
            return Err(errno);
        }
        source
            .complete_checkpoint_result(Ok(()))
            .map_err(|_| Errno::EIO)
    }

    fn settle_ordered_data(
        &self,
        source: &crate::journal::JournalFsyncSource,
    ) -> Result<(), Errno> {
        let request = metadata_writeback_request();
        let graph = match source.plan_data(&request) {
            BackendPlan::SubmitGraph(graph) => graph,
            BackendPlan::Err(errno) => return Err(errno),
            _ => return Err(Errno::EIO),
        };
        if let Err(errno) = self.execute_bio_graph(graph) {
            source.complete_data(BackendPageCompletion::new(
                request.object,
                request.id,
                request.op,
                PageIoResult::Err(errno),
            ));
            return Err(errno);
        }
        source.complete_data(BackendPageCompletion::new(
            request.object,
            request.id,
            request.op,
            PageIoResult::Done,
        ));
        Ok(())
    }

    pub(crate) fn shutdown_mount(&self) -> Result<(), Errno> {
        self.settle_metadata_caches();
        *self.mount_pin.lock() = None;
        Ok(())
    }

    fn execute_bio_graph(&self, graph: BackendBioGraph) -> Result<(), Errno> {
        let mut done = vec![false; graph.nodes().len()];
        let mut completed = 0usize;
        while completed < graph.nodes().len() {
            let mut progressed = false;
            for (index, node) in graph.nodes().iter().enumerate() {
                if done[index] || !dependencies_satisfied(&graph, &done, node.id) {
                    continue;
                }
                self.execute_bio_node(node)?;
                done[index] = true;
                completed += 1;
                progressed = true;
            }
            if !progressed {
                return Err(Errno::EIO);
            }
        }
        Ok(())
    }

    fn execute_bio_node(&self, node: &BackendBioNode) -> Result<(), Errno> {
        match node.bio.op {
            BlockOp::Write => {
                if node.bio.vecs.len() != 1 {
                    return Err(Errno::EIO);
                }
                let sectors_per_block = sectors_per_ext4_block()?;
                if node.bio.lba.block_count() != sectors_per_block {
                    return Err(Errno::EIO);
                }
                let mut page = [0; BLOCK_SIZE];
                read_page_cache_source(&node.source, &mut page)?;
                self.with_pager(|pager| {
                    pager.apply_l6_write_page(node.bio.lba.start_lba(), sectors_per_block, &page)
                })
            }
            BlockOp::Flush | BlockOp::Barrier => self.with_pager(|pager| pager.apply_l6_barrier()),
            BlockOp::Read => Err(Errno::EIO),
        }
    }
}

fn metadata_fsync_request() -> BackendPageRequest {
    BackendPageRequest::new(
        FsObjectKey::new(0),
        PageIoRequestId::new(1),
        PageIoRange::new(0, 1),
        PageIoOp::Fsync,
        PageIoFlags::BARRIER,
        None,
    )
}

fn metadata_writeback_request() -> BackendPageRequest {
    BackendPageRequest::new(
        FsObjectKey::new(0),
        PageIoRequestId::new(2),
        PageIoRange::new(0, 1),
        PageIoOp::Writeback,
        PageIoFlags::WRITEBACK,
        None,
    )
}

fn dependencies_satisfied(graph: &BackendBioGraph, done: &[bool], node: BackendBioNodeId) -> bool {
    graph
        .dependencies()
        .iter()
        .all(|dependency| dependency.after != node || node_done(graph, done, dependency.before))
}

fn node_done(graph: &BackendBioGraph, done: &[bool], node: BackendBioNodeId) -> bool {
    graph
        .nodes()
        .iter()
        .position(|candidate| candidate.id == node)
        .is_some_and(|index| done[index])
}

fn sectors_per_ext4_block() -> Result<u64, Errno> {
    u64::try_from(BLOCK_SIZE)
        .ok()
        .and_then(|bytes| bytes.checked_div(512))
        .filter(|sectors| *sectors > 0)
        .ok_or(Errno::EIO)
}

fn read_page_cache_source(source: &IoDataSource, page: &mut Page4K) -> Result<(), Errno> {
    let IoDataSource::PageCache {
        frame, offset, len, ..
    } = source
    else {
        return Err(Errno::EIO);
    };
    if *offset != 0 || *len != BLOCK_SIZE as u32 {
        return Err(Errno::EIO);
    }
    let ppn = frame.ppn();
    #[cfg(test)]
    {
        page_allocator::testing::read_frame_bytes_for_test(ppn, 0, page);
    }
    #[cfg(not(test))]
    {
        let src = page_allocator::frame_kernel_addr(ppn).map_err(|_| Errno::EIO)?;
        // SAFETY: the PageCache source exports a live frame lease for exactly
        // one ext4 block; `page` is a disjoint stack buffer of the same size.
        unsafe {
            core::ptr::copy_nonoverlapping(src as *const u8, page.as_mut_ptr(), BLOCK_SIZE);
        }
    }
    Ok(())
}
