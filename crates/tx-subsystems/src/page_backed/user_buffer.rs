use super::*;
use crate::vm::AddressSpace;

/// Read up to `len` bytes from `pc` at `of.offset()` into the user buffer at
/// `dst`, returning the number of bytes actually copied.
///
/// Materializes pages on demand and copies bytes through the eager-walk
/// `AddressSpace::copy_to_user` primitive. Advances `of.offset()` only after
/// a chunk has been both materialized and copied. EFAULT propagates as an
/// `Err` outcome on the very first chunk, or as `Done(advanced)` when prior
/// chunks succeeded.
pub fn step_read_to_user(
    pc: &PageContainer,
    of: &OpenFile,
    aspace: &AddressSpace,
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
    step_range_with_user_buffer(pc, of, aspace, effective_len, UserBuffer::Read { dst }, guard)
}

/// Write up to `len` bytes from the user buffer at `src` into `pc` at
/// `of.offset()`, returning the number of bytes actually copied.
///
/// Materializes pages on demand and copies bytes through the eager-walk
/// `AddressSpace::copy_from_user` primitive. Advances `of.offset()` and
/// grows the visible `PC.size` only after a chunk has been both materialized
/// and copied. EFAULT propagates as an `Err` outcome on the very first
/// chunk, or as `Done(advanced)` when prior chunks succeeded.
pub fn step_write_from_user(
    pc: &PageContainer,
    of: &OpenFile,
    aspace: &AddressSpace,
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
    let outcome =
        step_range_with_user_buffer(pc, of, aspace, len, UserBuffer::Write { src }, guard);
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

fn step_range_with_user_buffer(
    pc: &PageContainer,
    of: &OpenFile,
    aspace: &AddressSpace,
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
                match copy_chunk_user(
                    materialized.ppn,
                    within_page,
                    chunk,
                    buffer,
                    advanced,
                    aspace,
                    guard,
                ) {
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

fn copy_chunk_user(
    ppn: Ppn,
    within_page: usize,
    chunk: usize,
    buffer: UserBuffer,
    already_advanced: usize,
    aspace: &AddressSpace,
    guard: &Guard<'_>,
) -> Result<(), Errno> {
    let frame_base = page_allocator::frame_kernel_addr(ppn).map_err(|_| Errno::EIO)?;
    // SAFETY: frame_base is the kernel direct-map view of the
    // materialised PC frame. We hold the materialisation pin via the
    // caller's `MaterializedPage`. `within_page + chunk <= USER_PAGE_SIZE`
    // by construction in `step_range_with_user_buffer`.
    let kernel_byte = unsafe { frame_base.add(within_page) };
    match buffer {
        UserBuffer::Read { dst } => {
            // Reading from PC into user-space: PC is the kernel-side
            // source, user buffer is the destination.
            // SAFETY: kernel_byte is valid for `chunk` bytes (see
            // above); we expose it as a kernel-side slice for the
            // copy_to_user input.
            let kernel_slice = unsafe { core::slice::from_raw_parts(kernel_byte, chunk) };
            let user_dst = UserPtr::<u8>::new(dst.addr() + already_advanced);
            match aspace.copy_to_user(user_dst, kernel_slice, guard) {
                StepOutcome::Done(n) | StepOutcome::Advanced(n) if n == chunk => Ok(()),
                StepOutcome::Done(_) | StepOutcome::Advanced(_) => Err(Errno::EFAULT),
                StepOutcome::Err(e) => Err(e),
                // For per-chunk copies we treat any block as EFAULT
                // here — the outer step machinery already handles
                // PC-side blocks; user-side blocks would only happen
                // if a user-page backing itself blocks (not common
                // for the fast paths PC ↔ user-buf serves today).
                StepOutcome::Blocked(_) | StepOutcome::AdvancedThenBlocked(_, _) => {
                    Err(Errno::EFAULT)
                }
            }
        }
        UserBuffer::Write { src } => {
            // Writing from user-space into PC: user buffer is the
            // source, PC is the destination.
            // SAFETY: kernel_byte is valid for `chunk` mutable bytes
            // (the materialised PC page); we expose it as a kernel-
            // side mutable slice for the copy_from_user output.
            let kernel_slice =
                unsafe { core::slice::from_raw_parts_mut(kernel_byte, chunk) };
            let user_src = UserPtr::<u8>::new(src.addr() + already_advanced);
            match aspace.copy_from_user(kernel_slice, user_src, guard) {
                StepOutcome::Done(n) | StepOutcome::Advanced(n) if n == chunk => Ok(()),
                StepOutcome::Done(_) | StepOutcome::Advanced(_) => Err(Errno::EFAULT),
                StepOutcome::Err(e) => Err(e),
                StepOutcome::Blocked(_) | StepOutcome::AdvancedThenBlocked(_, _) => {
                    Err(Errno::EFAULT)
                }
            }
        }
    }
}
