use super::*;

/// Read up to `len` bytes from `pc` at `of.offset()` into the user buffer at
/// `dst`, returning the number of bytes actually copied.
///
/// Materializes pages on demand and copies bytes through the platform's
/// `UserAccessIf`. Advances `of.offset()` only after a chunk has been both
/// materialized and copied. EFAULT propagates as an `Err` outcome on the very
/// first chunk, or as `Done(advanced)` when prior chunks succeeded.
pub fn step_read_to_user<H: UserAccessIf>(
    pc: &PageContainer,
    of: &mut OpenFile,
    dst: UserPtr<u8>,
    len: usize,
    guard: &Guard<'_>,
) -> StepOutcome<usize> {
    if len == 0 {
        return StepOutcome::Done(0);
    }
    let Some(capacity) = pc.byte_capacity() else {
        return StepOutcome::Err(Errno::EINVAL);
    };
    let start = of.offset();
    let valid_end = core::cmp::min(pc.size_bytes(), capacity);
    if start >= valid_end {
        return StepOutcome::Done(0);
    }
    let effective_len = core::cmp::min(len as u64, valid_end - start) as usize;
    step_range_with_user_buffer::<H>(pc, of, effective_len, UserBuffer::Read { dst }, guard)
}

/// Write up to `len` bytes from the user buffer at `src` into `pc` at
/// `of.offset()`, returning the number of bytes actually copied.
///
/// Materializes pages on demand and copies bytes through the platform's
/// `UserAccessIf`. Advances `of.offset()` and grows the visible `PC.size`
/// only after a chunk has been both materialized and copied. EFAULT
/// propagates as an `Err` outcome on the very first chunk, or as
/// `Done(advanced)` when prior chunks succeeded.
pub fn step_write_from_user<H: UserAccessIf>(
    pc: &PageContainer,
    of: &mut OpenFile,
    src: UserPtr<u8>,
    len: usize,
    guard: &Guard<'_>,
) -> StepOutcome<usize> {
    if len == 0 {
        return StepOutcome::Done(0);
    }
    if matches!(pc.kind(), PageContainerKind::Device { .. }) {
        return StepOutcome::Err(Errno::EINVAL);
    }
    let Some(capacity) = pc.byte_capacity() else {
        return StepOutcome::Err(Errno::EINVAL);
    };
    let Some(end) = of.offset().checked_add(len as u64) else {
        return StepOutcome::Err(Errno::EINVAL);
    };
    if end > capacity {
        return StepOutcome::Err(Errno::EINVAL);
    }
    let start = of.offset();
    let outcome = step_range_with_user_buffer::<H>(pc, of, len, UserBuffer::Write { src }, guard);
    match &outcome {
        StepOutcome::Done(advanced)
        | StepOutcome::Advanced(advanced)
        | StepOutcome::AdvancedThenBlocked(advanced, _)
            if *advanced > 0 =>
        {
            pc.grow_size_to(start + *advanced as u64);
        }
        StepOutcome::Done(_)
        | StepOutcome::Advanced(_)
        | StepOutcome::AdvancedThenBlocked(_, _)
        | StepOutcome::Blocked(_)
        | StepOutcome::Err(_) => {}
    }
    outcome
}

#[derive(Clone, Copy)]
enum UserBuffer {
    Read { dst: UserPtr<u8> },
    Write { src: UserPtr<u8> },
}

impl UserBuffer {
    fn io_kind(self) -> PageBackedIoKind {
        match self {
            UserBuffer::Read { .. } => PageBackedIoKind::Read,
            UserBuffer::Write { .. } => PageBackedIoKind::Write,
        }
    }
}

fn step_range_with_user_buffer<H: UserAccessIf>(
    pc: &PageContainer,
    of: &mut OpenFile,
    len: usize,
    buffer: UserBuffer,
    guard: &Guard<'_>,
) -> StepOutcome<usize> {
    let mut advanced = 0usize;
    let mut offset = of.offset();
    while advanced < len {
        let page_index = PageIndex::new(offset / crate::vm::USER_PAGE_SIZE as u64);
        let within_page = (offset % crate::vm::USER_PAGE_SIZE as u64) as usize;
        let chunk = core::cmp::min(len - advanced, crate::vm::USER_PAGE_SIZE - within_page);
        let access = match buffer.io_kind() {
            PageBackedIoKind::Read => MaterializeAccess::Read,
            PageBackedIoKind::Write => MaterializeAccess::Write,
        };

        match pc.materialize_page(page_index, access, guard) {
            StepOutcome::Done(materialized) | StepOutcome::Advanced(materialized) => {
                match copy_chunk_user::<H>(materialized.ppn, within_page, chunk, buffer, advanced) {
                    Ok(()) => {
                        advanced += chunk;
                        offset += chunk as u64;
                    }
                    Err(errno) => {
                        if advanced == 0 {
                            return StepOutcome::Err(errno);
                        }
                        of.set_offset(offset);
                        return StepOutcome::Done(advanced);
                    }
                }
            }
            StepOutcome::AdvancedThenBlocked(_, token) => {
                of.set_offset(offset);
                return StepOutcome::AdvancedThenBlocked(advanced, token);
            }
            StepOutcome::Blocked(token) => {
                if advanced == 0 {
                    return StepOutcome::Blocked(token);
                }
                of.set_offset(offset);
                return StepOutcome::AdvancedThenBlocked(advanced, token);
            }
            StepOutcome::Err(errno) => {
                if advanced == 0 {
                    return StepOutcome::Err(errno);
                }
                of.set_offset(offset);
                return StepOutcome::Done(advanced);
            }
        }
    }

    of.set_offset(offset);
    StepOutcome::Done(advanced)
}

fn copy_chunk_user<H: UserAccessIf>(
    ppn: Ppn,
    within_page: usize,
    chunk: usize,
    buffer: UserBuffer,
    already_advanced: usize,
) -> Result<(), Errno> {
    let frame_base = page_allocator::frame_kernel_addr(ppn).map_err(|_| Errno::EIO)?;
    let kernel_byte = unsafe { frame_base.add(within_page) };
    match buffer {
        UserBuffer::Read { dst } => {
            let user_dst = UserPtr::<u8>::new(dst.addr() + already_advanced);
            unsafe {
                H::copy_to_user(user_dst, KernelPtr::new(kernel_byte), chunk)
                    .map_err(|_| Errno::EFAULT)
            }
        }
        UserBuffer::Write { src } => {
            let user_src = UserPtr::<u8>::new(src.addr() + already_advanced);
            unsafe {
                H::copy_from_user(KernelPtr::new(kernel_byte), user_src, chunk)
                    .map_err(|_| Errno::EFAULT)
            }
        }
    }
}
