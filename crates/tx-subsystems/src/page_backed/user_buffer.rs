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
) -> tx_substrate::step_v3::StepOutcome<usize, tx_substrate::step_v3::ByteProgress> {
    use tx_substrate::step_v3::StepOutcome as V3;
    if len == 0 {
        return V3::done(0);
    }
    let Some(capacity) = pc.byte_capacity() else {
        return V3::err(Errno::EINVAL.into());
    };
    let start = of.offset();
    let valid_end = core::cmp::min(pc.size_bytes(), capacity);
    if start >= valid_end {
        return V3::done(0);
    }
    let effective_len = core::cmp::min(len as u64, valid_end - start) as usize;
    step_range_with_user_buffer(
        pc,
        of,
        aspace,
        effective_len,
        UserBuffer::Read { dst },
        guard,
    )
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
) -> tx_substrate::step_v3::StepOutcome<usize, tx_substrate::step_v3::ByteProgress> {
    use tx_substrate::step_v3::StepOutcome as V3;
    if len == 0 {
        return V3::done(0);
    }
    if matches!(pc.kind(), PageContainerKind::Device { .. }) {
        return V3::err(Errno::EINVAL.into());
    }
    let Some(capacity) = pc.byte_capacity() else {
        return V3::err(Errno::EINVAL.into());
    };
    let Some(end) = of.offset().checked_add(len as u64) else {
        return V3::err(Errno::EINVAL.into());
    };
    if end > capacity {
        return V3::err(Errno::EINVAL.into());
    }
    let start = of.offset();
    let outcome =
        step_range_with_user_buffer(pc, of, aspace, len, UserBuffer::Write { src }, guard);
    let advanced_bytes = match &outcome {
        V3::Done(n) => *n,
        V3::Continue { progress } => progress.bytes(),
        V3::Yield { progress, .. } => progress.bytes(),
        V3::Err(_) => 0,
    };
    if advanced_bytes > 0 {
        pc.grow_size_to(start + advanced_bytes as u64);
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
) -> tx_substrate::step_v3::StepOutcome<usize, tx_substrate::step_v3::ByteProgress> {
    use tx_substrate::step_v3::{ByteProgress, StepOutcome as V3};
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

        // `materialize_page` is now v3
        // `StepOutcome<MaterializedPage, NoProgress>`. Map per variant:
        // - `Done` / `Continue { .. }` → run the user-side copy, then
        //   either continue the loop (on success) or terminate.
        // - `Yield { OnCarrier .. }` with `advanced == 0` → v3 `Yield`
        //   with `ByteProgress::EMPTY`. Otherwise propagate the yield
        //   carrying the accumulated bytes.
        // - `Yield { .. }` (OnAgent) → unsupported; surface `Err(EIO)`
        //   if no progress yet, else partial `Done`.
        // - `Err(errno)` with `advanced == 0` → v3 `Err(errno)`.
        //   Otherwise return v3 `Done(advanced)` (partial-success).
        use tx_substrate::step_v3::YieldShape;
        match pc.materialize_page(page_index, access, guard) {
            tx_substrate::step_v3::StepOutcome::Done(materialized) => {
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
                            return V3::err(errno.into());
                        }
                        of.set_offset(offset);
                        return V3::done(advanced);
                    }
                }
            }
            tx_substrate::step_v3::StepOutcome::Continue { .. } => {
                // NoProgress carrier: no materialized frame; treat as
                // EAGAIN-like and surface partial progress (or EIO if
                // none) — page allocation rarely emits this.
                if advanced == 0 {
                    return V3::err(tx_substrate::step_v3::Errno::EAGAIN);
                }
                of.set_offset(offset);
                return V3::done(advanced);
            }
            tx_substrate::step_v3::StepOutcome::Yield {
                shape: YieldShape::OnCarrier { carrier, interests },
                ..
            } => {
                if advanced == 0 {
                    return V3::yield_on_carrier(
                        ByteProgress::EMPTY,
                        carrier.raw(),
                        interests.raw(),
                    );
                }
                of.set_offset(offset);
                return V3::yield_on_carrier(
                    ByteProgress::new(advanced),
                    carrier.raw(),
                    interests.raw(),
                );
            }
            tx_substrate::step_v3::StepOutcome::Yield { .. } => {
                if advanced == 0 {
                    return V3::err(tx_substrate::step_v3::Errno::EIO);
                }
                of.set_offset(offset);
                return V3::done(advanced);
            }
            tx_substrate::step_v3::StepOutcome::Err(errno) => {
                if advanced == 0 {
                    return V3::err(errno);
                }
                of.set_offset(offset);
                return V3::done(advanced);
            }
        }
    }

    of.set_offset(offset);
    V3::done(advanced)
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
            use tx_substrate::step_v3::StepOutcome as V3;
            match aspace.copy_to_user(user_dst, kernel_slice, guard) {
                V3::Done(n) if n == chunk => Ok(()),
                V3::Continue { progress, .. } if progress.bytes() == chunk => Ok(()),
                V3::Done(_) | V3::Continue { .. } => Err(Errno::EFAULT),
                V3::Err(e) => Err(Errno::from(e)),
                // For per-chunk copies we treat any block as EFAULT
                // here — the outer step machinery already handles
                // PC-side blocks; user-side blocks would only happen
                // if a user-page backing itself blocks (not common
                // for the fast paths PC ↔ user-buf serves today).
                V3::Yield { .. } => Err(Errno::EFAULT),
            }
        }
        UserBuffer::Write { src } => {
            // Writing from user-space into PC: user buffer is the
            // source, PC is the destination.
            // SAFETY: kernel_byte is valid for `chunk` mutable bytes
            // (the materialised PC page); we expose it as a kernel-
            // side mutable slice for the copy_from_user output.
            let kernel_slice = unsafe { core::slice::from_raw_parts_mut(kernel_byte, chunk) };
            let user_src = UserPtr::<u8>::new(src.addr() + already_advanced);
            use tx_substrate::step_v3::StepOutcome as V3;
            match aspace.copy_from_user(kernel_slice, user_src, guard) {
                V3::Done(n) if n == chunk => Ok(()),
                V3::Continue { progress, .. } if progress.bytes() == chunk => Ok(()),
                V3::Done(_) | V3::Continue { .. } => Err(Errno::EFAULT),
                V3::Err(e) => Err(Errno::from(e)),
                V3::Yield { .. } => Err(Errno::EFAULT),
            }
        }
    }
}
