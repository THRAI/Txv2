use super::*;

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
) -> StepOutcome<usize> {
    if len == 0 {
        return StepOutcome::Done(0);
    }
    if matches!(out_pc.kind(), PageContainerKind::Device { .. }) {
        return StepOutcome::Err(Errno::EINVAL);
    }

    let Some(in_capacity) = in_pc.byte_capacity() else {
        return StepOutcome::Err(Errno::EINVAL);
    };
    let Some(out_capacity) = out_pc.byte_capacity() else {
        return StepOutcome::Err(Errno::EINVAL);
    };

    let Some(out_end) = out_offset.checked_add(len as u64) else {
        return StepOutcome::Err(Errno::EINVAL);
    };
    if out_end > out_capacity {
        return StepOutcome::Err(Errno::EINVAL);
    }

    let in_valid_end = core::cmp::min(in_pc.size_bytes(), in_capacity);
    if in_offset >= in_valid_end {
        return StepOutcome::Done(0);
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

        let in_materialized = match in_pc.materialize_page(in_page, MaterializeAccess::Read, guard)
        {
            StepOutcome::Done(m) | StepOutcome::Advanced(m) => m,
            StepOutcome::Blocked(token) => {
                if advanced == 0 {
                    return StepOutcome::Blocked(token);
                }
                return advanced_with_partial(out_pc, out_offset, advanced, token);
            }
            StepOutcome::AdvancedThenBlocked(_, token) => {
                return advanced_with_partial(out_pc, out_offset, advanced, token);
            }
            StepOutcome::Err(errno) => {
                if advanced == 0 {
                    return StepOutcome::Err(errno);
                }
                publish_progress(out_pc, out_offset, advanced);
                return StepOutcome::Done(advanced);
            }
        };

        let out_materialized =
            match out_pc.materialize_page(out_page, MaterializeAccess::Write, guard) {
                StepOutcome::Done(m) | StepOutcome::Advanced(m) => m,
                StepOutcome::Blocked(token) => {
                    if advanced == 0 {
                        return StepOutcome::Blocked(token);
                    }
                    return advanced_with_partial(out_pc, out_offset, advanced, token);
                }
                StepOutcome::AdvancedThenBlocked(_, token) => {
                    return advanced_with_partial(out_pc, out_offset, advanced, token);
                }
                StepOutcome::Err(errno) => {
                    if advanced == 0 {
                        return StepOutcome::Err(errno);
                    }
                    publish_progress(out_pc, out_offset, advanced);
                    return StepOutcome::Done(advanced);
                }
            };

        let in_base = match page_allocator::frame_kernel_addr(in_materialized.ppn) {
            Ok(ptr) => ptr,
            Err(_) => {
                if advanced == 0 {
                    return StepOutcome::Err(Errno::EIO);
                }
                publish_progress(out_pc, out_offset, advanced);
                return StepOutcome::Done(advanced);
            }
        };
        let out_base = match page_allocator::frame_kernel_addr(out_materialized.ppn) {
            Ok(ptr) => ptr,
            Err(_) => {
                if advanced == 0 {
                    return StepOutcome::Err(Errno::EIO);
                }
                publish_progress(out_pc, out_offset, advanced);
                return StepOutcome::Done(advanced);
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
    StepOutcome::Done(advanced)
}

fn publish_progress(out_pc: &PageContainer, out_offset: u64, advanced: usize) {
    if advanced == 0 {
        return;
    }
    out_pc.grow_size_to(out_offset + advanced as u64);
}

fn advanced_with_partial(
    out_pc: &PageContainer,
    out_offset: u64,
    advanced: usize,
    token: crate::execution::WaitToken,
) -> StepOutcome<usize> {
    publish_progress(out_pc, out_offset, advanced);
    StepOutcome::AdvancedThenBlocked(advanced, token)
}
