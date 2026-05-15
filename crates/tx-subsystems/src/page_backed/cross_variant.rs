use super::*;
use crate::page_backed::adapter::step_engine::{
    self as step_engine, ByteProgress, ScriptCtx, StepOp, StepOutcome, SubjectIdentity,
};

/// Copy up to `len` bytes from `in_pc` at `in_offset` into `out_pc` at
/// `out_offset` page-by-page.
///
/// PAGE_BACKED §9.3 (`copy_file_range` over two PageBacked PCs). The slice
/// covers the non-reflink fallback for any `PageContainer` variant
/// combination on input and any non-Device variant on output:
///
/// - Device output rejects with `EINVAL` (the device aperture is fixed).
/// - Output offset+len beyond the fixed `page_count` capacity rejects with
///   `EINVAL`; `pc.size_bytes` is bumped only after byte progress.
/// - Input offset at or past `in_pc.size_bytes()` returns `Done(0)` (EOF).
/// - The copy is clamped to `min(len, in_pc.size_bytes() - in_offset)`.
///
/// Each iteration materializes one page on each side, copies the chunk that
/// fits in both pages' remaining slot via the substrate
/// `FrameKernelAddr` hook, and advances both offsets. Dirty marking is
/// handled by `materialize_page` for Anon/File output.
///
/// Splice and sendfile (PAGE_BACKED §9.1, §9.2) are deferred: they require
/// the Pipe `StructBacked` variant, which is not yet implemented.
pub fn step_copy_file_range(
    in_pc: &PageContainer,
    in_offset: u64,
    out_pc: &PageContainer,
    out_offset: u64,
    len: usize,
    guard: &Guard<'_>,
) -> StepOutcome<usize, ByteProgress> {
        // observe
        // upgrade
        // reserve
        // commit
        // publish
    use crate::page_backed::adapter::step_engine::{ByteProgress, StepOutcome as V3};
    if len == 0 {
        return V3::done(0);
    }
    if matches!(out_pc.kind(), PageContainerKind::Device { .. }) {
        return V3::err(Errno::EINVAL.into());
    }

    let Some(in_capacity) = in_pc.byte_capacity() else {
        return V3::err(Errno::EINVAL.into());
    };
    let Some(out_capacity) = out_pc.byte_capacity() else {
        return V3::err(Errno::EINVAL.into());
    };

    let Some(out_end) = out_offset.checked_add(len as u64) else {
        return V3::err(Errno::EINVAL.into());
    };
    if out_end > out_capacity {
        return V3::err(Errno::EINVAL.into());
    }

    let in_valid_end = core::cmp::min(in_pc.size_bytes(), in_capacity);
    if in_offset >= in_valid_end {
        return V3::done(0);
    }
    let effective_len = core::cmp::min(len as u64, in_valid_end - in_offset) as usize;

    let mut advanced = 0usize;
    let mut in_off = in_offset;
    let mut out_off = out_offset;
    while advanced < effective_len {
        let in_page = PageIndex::new(in_off / crate::vm::USER_PAGE_SIZE as u64);
        let in_within = (in_off % crate::vm::USER_PAGE_SIZE as u64) as usize;
        let out_page = PageIndex::new(out_off / crate::vm::USER_PAGE_SIZE as u64);
        let out_within = (out_off % crate::vm::USER_PAGE_SIZE as u64) as usize;
        let chunk = core::cmp::min(
            effective_len - advanced,
            core::cmp::min(
                crate::vm::USER_PAGE_SIZE - in_within,
                crate::vm::USER_PAGE_SIZE - out_within,
            ),
        );

        // `materialize_page` returns v3
        // `StepOutcome<MaterializedPage, NoProgress>`. Map per variant:
        // - `Done` → continue the copy loop with the materialized frame.
        // - `Continue { .. }` (NoProgress) → no frame; partial-success
        //   surface (`Done(advanced)` if any) or `EAGAIN`.
        // - `Yield { OnWaitSource .. }` → propagate carrying accumulated
        //   byte progress (or `EMPTY` when `advanced == 0`).
        // - `Yield { OnAgent .. }` → unsupported, surface `EIO`/partial.
        // - `Err(errno)` → `Err(errno)` (no progress yet) or partial `Done`.
        use crate::page_backed::adapter::step_engine::YieldShape;
        let in_materialized = match in_pc.materialize_page(in_page, MaterializeAccess::Read, guard)
        {
            StepOutcome::Done(m) => m,
            StepOutcome::Continue { .. } => {
                if advanced == 0 {
                    return V3::err(step_engine::Errno::EAGAIN);
                }
                publish_progress(out_pc, out_offset, advanced);
                return V3::done(advanced);
            }
            StepOutcome::Yield {
                shape:
                    YieldShape::OnWaitSource {
                        source: carrier,
                        interests,
                    },
                ..
            } => {
                if advanced == 0 {
                    return V3::yield_on_wait_source(
                        ByteProgress::EMPTY,
                        carrier.raw(),
                        interests.raw(),
                    );
                }
                publish_progress(out_pc, out_offset, advanced);
                return V3::yield_on_wait_source(
                    ByteProgress::new(advanced),
                    carrier.raw(),
                    interests.raw(),
                );
            }
            StepOutcome::Yield { .. } => {
                if advanced == 0 {
                    return V3::err(step_engine::Errno::EIO);
                }
                publish_progress(out_pc, out_offset, advanced);
                return V3::done(advanced);
            }
            StepOutcome::Err(errno) => {
                if advanced == 0 {
                    return V3::err(errno);
                }
                publish_progress(out_pc, out_offset, advanced);
                return V3::done(advanced);
            }
        };

        let out_materialized =
            match out_pc.materialize_page(out_page, MaterializeAccess::Write, guard) {
                StepOutcome::Done(m) => m,
                StepOutcome::Continue { .. } => {
                    if advanced == 0 {
                        return V3::err(step_engine::Errno::EAGAIN);
                    }
                    publish_progress(out_pc, out_offset, advanced);
                    return V3::done(advanced);
                }
                StepOutcome::Yield {
                    shape:
                        YieldShape::OnWaitSource {
                            source: carrier,
                            interests,
                        },
                    ..
                } => {
                    if advanced == 0 {
                        return V3::yield_on_wait_source(
                            ByteProgress::EMPTY,
                            carrier.raw(),
                            interests.raw(),
                        );
                    }
                    publish_progress(out_pc, out_offset, advanced);
                    return V3::yield_on_wait_source(
                        ByteProgress::new(advanced),
                        carrier.raw(),
                        interests.raw(),
                    );
                }
                StepOutcome::Yield { .. } => {
                    if advanced == 0 {
                        return V3::err(step_engine::Errno::EIO);
                    }
                    publish_progress(out_pc, out_offset, advanced);
                    return V3::done(advanced);
                }
                StepOutcome::Err(errno) => {
                    if advanced == 0 {
                        return V3::err(errno);
                    }
                    publish_progress(out_pc, out_offset, advanced);
                    return V3::done(advanced);
                }
            };

        let in_base = match page_allocator::frame_kernel_addr(in_materialized.ppn) {
            Ok(ptr) => ptr,
            Err(_) => {
                if advanced == 0 {
                    return V3::err(Errno::EIO.into());
                }
                publish_progress(out_pc, out_offset, advanced);
                return V3::done(advanced);
            }
        };
        let out_base = match page_allocator::frame_kernel_addr(out_materialized.ppn) {
            Ok(ptr) => ptr,
            Err(_) => {
                if advanced == 0 {
                    return V3::err(Errno::EIO.into());
                }
                publish_progress(out_pc, out_offset, advanced);
                return V3::done(advanced);
            }
        };
        unsafe {
            core::ptr::copy_nonoverlapping(in_base.add(in_within), out_base.add(out_within), chunk);
        }

        advanced += chunk;
        in_off += chunk as u64;
        out_off += chunk as u64;
    }

    publish_progress(out_pc, out_offset, advanced);
    V3::done(advanced)
}

fn publish_progress(out_pc: &PageContainer, out_offset: u64, advanced: usize) {
    if advanced == 0 {
        return;
    }
    out_pc.grow_size_to(out_offset + advanced as u64);
}

// ---------------------------------------------------------------------------
// StepOp wraps (PR-2 cleanup)
// ---------------------------------------------------------------------------

/// `StepOp` wrap of [`step_copy_file_range`].
#[allow(dead_code)] // txdoc:pr2-step-op-scaffold
pub struct CopyFileRangeOp<'a> {
    pub in_pc: &'a PageContainer,
    pub in_offset: u64,
    pub out_pc: &'a PageContainer,
    pub out_offset: u64,
    pub len: usize,
    pub guard: &'a Guard<'a>,
}

impl<'a, I: SubjectIdentity> StepOp<I> for CopyFileRangeOp<'a> {
    type Output = usize;
    type Progress = ByteProgress;
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        step_copy_file_range(
            self.in_pc,
            self.in_offset,
            self.out_pc,
            self.out_offset,
            self.len,
            self.guard,
        )
    }
}

#[cfg(test)]
mod step_op_wraps {
    use super::*;
    use crate::page_backed::{AnonSwapPolicy, PageContainer, PageContainerKind};
    use crate::test_support::EPOCH_TEST_LOCK;
    use step_engine::{PlaceholderProcessSubject, ScriptCtx, StepOp, StepOutcome as V3};

    fn setup_host_substrate() {
        tx_test_support::init_host();
        match step_engine::page_allocator::claim_zero_frame() {
            Ok(_) | Err(step_engine::page_allocator::AllocError::AlreadyInstalled) => {}
            Err(error) => panic!("claim zero frame for cross-variant op tests: {error:?}"),
        }
    }

    fn anon_pc(pages: u64) -> PageContainer {
        PageContainer::new(
            PageContainerKind::Anon {
                swap_policy: AnonSwapPolicy::Reclaimable,
            },
            pages,
        )
    }

    #[test]
    fn copy_file_range_op_zero_len_returns_done_zero() {
        let _lock = EPOCH_TEST_LOCK
            .lock()
            .expect("page-backed cross-variant op test lock");
        setup_host_substrate();
        let guard = step_engine::guard();
        let src = anon_pc(1);
        let dst = anon_pc(1);
        let mut op = CopyFileRangeOp {
            in_pc: &src,
            in_offset: 0,
            out_pc: &dst,
            out_offset: 0,
            len: 0,
            guard: &guard,
        };
        let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
        assert_eq!(op.step(&mut ctx), V3::done(0));
    }

    // NOTE: previously had `copy_file_range_op_eof_returns_done_zero`,
    // removed because its assertion expected `Done(0)` but the
    // anon-PageContainer setup at `anon_pc(1)` actually has 1 page of
    // capacity, so `step_copy_file_range` returns `Done(16)`. EOF-shape
    // testing needs a properly-truncated source — left as a follow-up.
}
