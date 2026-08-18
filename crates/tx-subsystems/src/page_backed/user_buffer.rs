use super::*;
use crate::page_backed::adapter::step_engine::{
    self as step_engine, ByteProgress, ScriptCtx, StepOp, StepOutcome, SubjectIdentity,
};
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
) -> StepOutcome<usize, ByteProgress> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    use crate::page_backed::adapter::step_engine::StepOutcome as V3;
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
        start,
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
) -> StepOutcome<usize, ByteProgress> {
    step_write_from_user_at(pc, of, aspace, src, len, of.offset(), guard)
}

/// Variant of [`step_write_from_user`] whose first byte is written at
/// `start`. The shared file offset is published only after bytes move, so an
/// append attempt that faults or waits with zero progress leaves it unchanged.
pub fn step_write_from_user_at(
    pc: &PageContainer,
    of: &OpenFile,
    aspace: &AddressSpace,
    src: UserPtr<u8>,
    len: usize,
    start: u64,
    guard: &Guard<'_>,
) -> StepOutcome<usize, ByteProgress> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    use crate::page_backed::adapter::step_engine::StepOutcome as V3;
    emit_pagebacked_trace(b"debug.pagebacked.write_user.len", len as i64);
    emit_pagebacked_trace(b"debug.pagebacked.write_user.offset", start as i64);
    emit_pagebacked_trace(b"debug.pagebacked.write_user.phase", 0);
    if len == 0 {
        return V3::done(0);
    }
    if matches!(pc.kind(), PageContainerKind::Device { .. }) {
        emit_pagebacked_trace(b"debug.pagebacked.write_user.err", 1);
        return V3::err(Errno::EINVAL.into());
    }
    let Some(capacity) = pc.byte_capacity() else {
        emit_pagebacked_trace(b"debug.pagebacked.write_user.err", 2);
        return V3::err(Errno::EINVAL.into());
    };
    let Some(end) = start.checked_add(len as u64) else {
        emit_pagebacked_trace(b"debug.pagebacked.write_user.err", 3);
        return V3::err(Errno::EINVAL.into());
    };
    if end > capacity {
        emit_pagebacked_trace(b"debug.pagebacked.write_user.err", 4);
        return V3::err(Errno::EINVAL.into());
    }
    emit_pagebacked_trace(b"debug.pagebacked.write_user.phase", 1);
    let outcome =
        step_range_with_user_buffer(pc, of, aspace, len, UserBuffer::Write { src }, start, guard);
    emit_pagebacked_trace(b"debug.pagebacked.write_user.phase", 2);
    let advanced_bytes = match &outcome {
        V3::Done(n) => *n,
        V3::Continue { progress } => progress.bytes(),
        V3::Yield { progress, .. } => progress.bytes(),
        V3::Err(_) => 0,
    };
    emit_pagebacked_trace(
        b"debug.pagebacked.write_user.advanced",
        advanced_bytes as i64,
    );
    if advanced_bytes > 0 {
        emit_pagebacked_trace(b"debug.pagebacked.write_user.phase", 3);
        pc.grow_size_to(start + advanced_bytes as u64);
        emit_pagebacked_trace(b"debug.pagebacked.write_user.phase", 4);
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

/// Outcome of a single chunk copy between a materialised PC frame and
/// user-space memory via `AddressSpace::copy_*_user`.
enum UserChunkOutcome {
    /// Chunk copied in full.
    Copied,
    /// Fatal error (EFAULT, EIO, etc.).
    Fault(Errno),
    /// User-space materialisation asked the enclosing step to be polled
    /// again. `bytes` is the prefix already copied by this chunk.
    Continue { bytes: usize },
    /// User-space page needs async materialisation; yield so the
    /// reactor can re-poll after wake.
    Blocked {
        bytes: usize,
        source: u64,
        interests: u64,
    },
}

fn step_range_with_user_buffer(
    pc: &PageContainer,
    of: &OpenFile,
    aspace: &AddressSpace,
    len: usize,
    buffer: UserBuffer,
    start_offset: u64,
    guard: &Guard<'_>,
) -> StepOutcome<usize, ByteProgress> {
    use crate::page_backed::adapter::step_engine::{ByteProgress, StepOutcome as V3};
    let mut advanced = 0usize;
    let mut offset = start_offset;
    emit_pagebacked_trace(b"debug.pagebacked.user_range.len", len as i64);
    emit_pagebacked_trace(b"debug.pagebacked.user_range.phase", 0);
    while advanced < len {
        let page_index = PageIndex::new(offset / crate::vm::USER_PAGE_SIZE as u64);
        let within_page = (offset % crate::vm::USER_PAGE_SIZE as u64) as usize;
        let chunk = core::cmp::min(len - advanced, crate::vm::USER_PAGE_SIZE - within_page);
        emit_pagebacked_trace(b"debug.pagebacked.user_range.chunk", chunk as i64);
        emit_pagebacked_trace(b"debug.pagebacked.user_range.advanced", advanced as i64);
        let access = match buffer.io_kind() {
            PageBackedIoKind::Read => MaterializeAccess::Read,
            PageBackedIoKind::Write => MaterializeAccess::Write,
        };

        // `materialize_page` is now v3
        // `StepOutcome<MaterializedPage, NoProgress>`. Map per variant:
        // - `Done` → run the user-side copy, then either continue the loop
        //   (on success) or terminate.
        // - `Continue { .. }` → preserve the internal page-cache retry and
        //   any bytes already copied; it must not become userspace EAGAIN.
        // - `Yield { OnWaitSource .. }` with `advanced == 0` → v3 `Yield`
        //   with `ByteProgress::EMPTY`. Otherwise propagate the yield
        //   carrying the accumulated bytes.
        // - `Yield { .. }` (OnAgent) → unsupported; surface `Err(EIO)`
        //   if no progress yet, else partial `Done`.
        // - `Err(errno)` with `advanced == 0` → v3 `Err(errno)`.
        //   Otherwise return v3 `Done(advanced)` (partial-success).
        emit_pagebacked_trace(b"debug.pagebacked.user_range.phase", 1);
        match pc.materialize_page(page_index, access, guard) {
            StepOutcome::Done(materialized) => {
                emit_pagebacked_trace(b"debug.pagebacked.user_range.phase", 2);
                match copy_chunk_user(
                    materialized.ppn,
                    within_page,
                    chunk,
                    buffer,
                    advanced,
                    aspace,
                    guard,
                ) {
                    UserChunkOutcome::Copied => {
                        emit_pagebacked_trace(b"debug.pagebacked.user_range.phase", 3);
                        advanced += chunk;
                        offset += chunk as u64;
                    }
                    UserChunkOutcome::Fault(errno) => {
                        emit_pagebacked_trace(b"debug.pagebacked.user_range.err", 1);
                        if advanced == 0 {
                            return V3::err(errno.into());
                        }
                        emit_pagebacked_trace(b"debug.pagebacked.user_range.phase", 4);
                        of.set_offset(offset);
                        return V3::done(advanced);
                    }
                    UserChunkOutcome::Continue { bytes } => {
                        let copied = bytes.min(chunk);
                        advanced += copied;
                        offset += copied as u64;
                        if advanced > 0 {
                            of.set_offset(offset);
                        }
                        return V3::continue_with(ByteProgress::new(advanced));
                    }
                    UserChunkOutcome::Blocked {
                        bytes,
                        source,
                        interests,
                    } => {
                        emit_pagebacked_trace(
                            b"debug.pagebacked.user_range.blocked",
                            source as i64,
                        );
                        let copied = bytes.min(chunk);
                        advanced += copied;
                        offset += copied as u64;
                        if advanced > 0 {
                            emit_pagebacked_trace(b"debug.pagebacked.user_range.phase", 4);
                            of.set_offset(offset);
                        }
                        return crate::page_backed::notification::yield_on_wait_source(
                            ByteProgress::new(advanced),
                            source,
                            interests,
                        );
                    }
                }
            }
            StepOutcome::Continue { .. } => {
                // A concurrent fetch owner or resident-root publisher may
                // leave a short retry window with no wait source to join yet.
                // Preserve that internal retry as `Continue`: the waiting
                // syscall driver inserts a scheduler boundary for empty
                // progress and re-enters with a fresh guard.  Returning
                // EAGAIN here leaks page-cache coordination to a blocking
                // read(2), which is observable under parallel compiler I/O.
                emit_pagebacked_trace(b"debug.pagebacked.user_range.err", 2);
                emit_pagebacked_trace(b"debug.pagebacked.user_range.phase", 4);
                return continue_after_materialize_retry(of, offset, advanced);
            }
            StepOutcome::Yield { shape, .. } => {
                emit_pagebacked_trace(b"debug.pagebacked.user_range.err", 3);
                let Some((carrier, interests)) =
                    crate::page_backed::notification::wait_source_parts(&shape)
                else {
                    if advanced == 0 {
                        return V3::err(step_engine::Errno::EIO);
                    }
                    emit_pagebacked_trace(b"debug.pagebacked.user_range.phase", 4);
                    of.set_offset(offset);
                    return V3::done(advanced);
                };
                if advanced == 0 {
                    return crate::page_backed::notification::yield_on_wait_source(
                        ByteProgress::EMPTY,
                        carrier,
                        interests,
                    );
                }
                emit_pagebacked_trace(b"debug.pagebacked.user_range.phase", 4);
                of.set_offset(offset);
                return crate::page_backed::notification::yield_on_wait_source(
                    ByteProgress::new(advanced),
                    carrier,
                    interests,
                );
            }
            StepOutcome::Err(errno) => {
                emit_pagebacked_trace(b"debug.pagebacked.user_range.err", 4);
                if advanced == 0 {
                    return V3::err(errno);
                }
                emit_pagebacked_trace(b"debug.pagebacked.user_range.phase", 4);
                of.set_offset(offset);
                return V3::done(advanced);
            }
        }
    }

    emit_pagebacked_trace(b"debug.pagebacked.user_range.phase", 5);
    of.set_offset(offset);
    V3::done(advanced)
}

/// Preserve a page-cache retry inside the step engine instead of exposing it
/// as a Linux `EAGAIN`.  When earlier chunks completed, their file-position
/// advance must be committed exactly once before the driver re-enters.
pub(super) fn continue_after_materialize_retry(
    of: &OpenFile,
    offset: u64,
    advanced: usize,
) -> StepOutcome<usize, ByteProgress> {
    if advanced != 0 {
        of.set_offset(offset);
    }
    StepOutcome::Continue {
        progress: ByteProgress::new(advanced),
    }
}

fn copy_chunk_user(
    ppn: Ppn,
    within_page: usize,
    chunk: usize,
    buffer: UserBuffer,
    already_advanced: usize,
    aspace: &AddressSpace,
    guard: &Guard<'_>,
) -> UserChunkOutcome {
    emit_pagebacked_trace(b"debug.pagebacked.user_copy.chunk", chunk as i64);
    emit_pagebacked_trace(b"debug.pagebacked.user_copy.phase", 0);
    let frame_base = match page_allocator::frame_kernel_addr(ppn) {
        Ok(p) => p,
        Err(_) => {
            emit_pagebacked_trace(b"debug.pagebacked.user_copy.err", 1);
            return UserChunkOutcome::Fault(Errno::EIO);
        }
    };
    emit_pagebacked_trace(b"debug.pagebacked.user_copy.phase", 1);
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
            use crate::page_backed::adapter::step_engine::StepOutcome as V3;
            emit_pagebacked_trace(b"debug.pagebacked.user_copy.kind", 0);
            emit_pagebacked_trace(b"debug.pagebacked.user_copy.phase", 2);
            match aspace.copy_to_user(user_dst, kernel_slice, guard) {
                V3::Done(n) if n == chunk => {
                    emit_pagebacked_trace(b"debug.pagebacked.user_copy.phase", 3);
                    UserChunkOutcome::Copied
                }
                V3::Done(_) => {
                    emit_pagebacked_trace(b"debug.pagebacked.user_copy.err", 2);
                    UserChunkOutcome::Fault(Errno::EFAULT)
                }
                V3::Continue { progress } => UserChunkOutcome::Continue {
                    bytes: progress.bytes(),
                },
                V3::Err(e) => {
                    emit_pagebacked_trace(b"debug.pagebacked.user_copy.err", 3);
                    UserChunkOutcome::Fault(Errno::from(e))
                }
                V3::Yield { progress, shape } => {
                    if let Some((source, interests)) =
                        crate::page_backed::notification::wait_source_parts(&shape)
                    {
                        emit_pagebacked_trace(b"debug.pagebacked.user_copy.blocked", source as i64);
                        UserChunkOutcome::Blocked {
                            bytes: progress.bytes(),
                            source,
                            interests,
                        }
                    } else {
                        emit_pagebacked_trace(b"debug.pagebacked.user_copy.err", 4);
                        UserChunkOutcome::Fault(Errno::EFAULT)
                    }
                }
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
            use crate::page_backed::adapter::step_engine::StepOutcome as V3;
            emit_pagebacked_trace(b"debug.pagebacked.user_copy.kind", 1);
            emit_pagebacked_trace(b"debug.pagebacked.user_copy.phase", 2);
            match aspace.copy_from_user(kernel_slice, user_src, guard) {
                V3::Done(n) if n == chunk => {
                    emit_pagebacked_trace(b"debug.pagebacked.user_copy.phase", 3);
                    UserChunkOutcome::Copied
                }
                V3::Done(_) => {
                    emit_pagebacked_trace(b"debug.pagebacked.user_copy.err", 5);
                    UserChunkOutcome::Fault(Errno::EFAULT)
                }
                V3::Continue { progress } => UserChunkOutcome::Continue {
                    bytes: progress.bytes(),
                },
                V3::Err(e) => {
                    emit_pagebacked_trace(b"debug.pagebacked.user_copy.err", 6);
                    UserChunkOutcome::Fault(Errno::from(e))
                }
                V3::Yield { progress, shape } => {
                    if let Some((source, interests)) =
                        crate::page_backed::notification::wait_source_parts(&shape)
                    {
                        emit_pagebacked_trace(b"debug.pagebacked.user_copy.blocked", source as i64);
                        UserChunkOutcome::Blocked {
                            bytes: progress.bytes(),
                            source,
                            interests,
                        }
                    } else {
                        emit_pagebacked_trace(b"debug.pagebacked.user_copy.err", 7);
                        UserChunkOutcome::Fault(Errno::EFAULT)
                    }
                }
            }
        }
    }
}

fn emit_pagebacked_trace(name: &[u8], value: i64) {
    if let Some(observer) = tx_observe::current() {
        observer.debug_counter(name, value);
    }
}

// ---------------------------------------------------------------------------
// Kernel-buffer variants (PR-11 follow-up: OpenFile::step_read PageBacked arm)
// ---------------------------------------------------------------------------
//
// Per W-JJ's gap analysis (2026-05-12 STATUS catchup), `OpenFile::step_read`
// for `RNodeBacking::PageBacked` previously returned `ENOSYS` — neither
// `step_read` (copyless staging) nor `step_read_to_user` (needs an
// `AddressSpace`) fits the byte-moving `&mut [u8]` shape that
// `OpenFile::step_read` and `OpenFile::step_write` expose. These helpers are
// the kernel-buffer cousin to `step_read_to_user` / `step_write_from_user`:
// they materialise pages on demand and copy bytes through the substrate's
// `frame_kernel_addr` direct-map view directly into / out of a kernel slice,
// advancing `of.offset()` as bytes move.
//
// AIO PREAD / PWRITE and direct `sys_read` / `sys_write` against a
// page-backed file both reach these through `OpenFile::step_read` /
// `OpenFile::step_write`.

/// Read up to `dst.len()` bytes from `pc` at `of.offset()` into the kernel
/// buffer `dst`, returning the number of bytes actually copied. Mirrors
/// [`step_read_to_user`] but the destination is a kernel slice (no
/// `AddressSpace` traversal).
///
/// EOF before the buffer is full short-reads: returns `Done(advanced)` with
/// `advanced < dst.len()`. A read starting at or past EOF returns `Done(0)`.
pub fn step_read_to_kernel(
    pc: &PageContainer,
    of: &OpenFile,
    dst: &mut [u8],
    guard: &Guard<'_>,
) -> StepOutcome<usize, ByteProgress> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    use crate::page_backed::adapter::step_engine::StepOutcome as V3;
    let len = dst.len();
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
    step_range_with_kernel_buffer(pc, of, effective_len, KernelBuffer::Read { dst }, guard)
}

/// Write up to `src.len()` bytes from the kernel buffer `src` into `pc` at
/// `of.offset()`, returning the number of bytes actually copied. Mirrors
/// [`step_write_from_user`] but the source is a kernel slice.
///
/// Advances `of.offset()` and grows the visible `PC.size` only after a chunk
/// has been both materialised and copied.
pub fn step_write_from_kernel(
    pc: &PageContainer,
    of: &OpenFile,
    src: &[u8],
    guard: &Guard<'_>,
) -> StepOutcome<usize, ByteProgress> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    use crate::page_backed::adapter::step_engine::StepOutcome as V3;
    let len = src.len();
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
    let outcome = step_range_with_kernel_buffer(pc, of, len, KernelBuffer::Write { src }, guard);
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

enum KernelBuffer<'a> {
    Read { dst: &'a mut [u8] },
    Write { src: &'a [u8] },
}

impl<'a> KernelBuffer<'a> {
    fn io_kind(&self) -> PageBackedIoKind {
        match self {
            KernelBuffer::Read { .. } => PageBackedIoKind::Read,
            KernelBuffer::Write { .. } => PageBackedIoKind::Write,
        }
    }
}

fn step_range_with_kernel_buffer(
    pc: &PageContainer,
    of: &OpenFile,
    len: usize,
    mut buffer: KernelBuffer<'_>,
    guard: &Guard<'_>,
) -> StepOutcome<usize, ByteProgress> {
    use crate::page_backed::adapter::step_engine::{ByteProgress, StepOutcome as V3};
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
            V3::Done(materialized) => {
                match copy_chunk_kernel(materialized.ppn, within_page, chunk, &mut buffer, advanced)
                {
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
            V3::Continue { .. } => {
                // Match the direct user-buffer lane: this is an internal
                // page-cache retry, not a Linux-visible nonblocking result.
                return continue_after_materialize_retry(of, offset, advanced);
            }
            V3::Yield { shape, .. } => {
                let Some((carrier, interests)) =
                    crate::page_backed::notification::wait_source_parts(&shape)
                else {
                    if advanced == 0 {
                        return V3::err(step_engine::Errno::EIO);
                    }
                    of.set_offset(offset);
                    return V3::done(advanced);
                };
                if advanced == 0 {
                    return crate::page_backed::notification::yield_on_wait_source(
                        ByteProgress::EMPTY,
                        carrier,
                        interests,
                    );
                }
                of.set_offset(offset);
                return crate::page_backed::notification::yield_on_wait_source(
                    ByteProgress::new(advanced),
                    carrier,
                    interests,
                );
            }
            V3::Err(errno) => {
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

fn copy_chunk_kernel(
    ppn: Ppn,
    within_page: usize,
    chunk: usize,
    buffer: &mut KernelBuffer<'_>,
    already_advanced: usize,
) -> Result<(), Errno> {
    let frame_base = page_allocator::frame_kernel_addr(ppn).map_err(|_| Errno::EIO)?;
    // SAFETY: frame_base is the kernel direct-map view of the
    // materialised PC frame. We hold the materialisation pin via the
    // caller's `MaterializedPage`. `within_page + chunk <= USER_PAGE_SIZE`
    // by construction in `step_range_with_kernel_buffer`.
    let frame_byte = unsafe { frame_base.add(within_page) };
    match buffer {
        KernelBuffer::Read { dst } => {
            // PC → kernel buffer: source is the materialised page, dest
            // is `dst[already_advanced..][..chunk]`.
            // SAFETY: `frame_byte` is valid for `chunk` bytes; the
            // destination slice has at least `chunk` bytes remaining
            // (the caller's `step_range_with_kernel_buffer` loop only
            // requests chunks that fit in `len - advanced`).
            unsafe {
                core::ptr::copy_nonoverlapping(
                    frame_byte,
                    dst.as_mut_ptr().add(already_advanced),
                    chunk,
                );
            }
            Ok(())
        }
        KernelBuffer::Write { src } => {
            // Kernel buffer → PC: source is `src[already_advanced..][..chunk]`,
            // dest is the materialised page.
            // SAFETY: same as the Read arm; we additionally need the page
            // to be writable, which the caller ensures via
            // `MaterializeAccess::Write`.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    src.as_ptr().add(already_advanced),
                    frame_byte,
                    chunk,
                );
            }
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// StepOp wraps (PR-2 wave 3)
// ---------------------------------------------------------------------------
//
// Additive `impl StepOp` adapters per `docs/Txv3/03_STEP_MODEL_v2.md` §2.1.
// Each wrap stores its inputs by reference under a single lifetime `'a` and
// delegates from `step()` to the corresponding free fn above — semantics
// are unchanged. The free fns remain the source of truth; callers can
// migrate to the `*Op` types incrementally.

/// `StepOp` wrap of [`step_read_to_user`].
pub struct ReadToUserOp<'a> {
    pub pc: &'a PageContainer,
    pub of: &'a OpenFile,
    pub aspace: &'a AddressSpace,
    pub dst: UserPtr<u8>,
    pub len: usize,
}

impl<'a, I: SubjectIdentity> StepOp<I> for ReadToUserOp<'a> {
    type Output = usize;
    type Progress = ByteProgress;
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        let __guard = step_engine::guard();
        step_read_to_user(self.pc, self.of, self.aspace, self.dst, self.len, &__guard)
    }
}

/// `StepOp` wrap of [`step_write_from_user`].
pub struct WriteFromUserOp<'a> {
    pub pc: &'a PageContainer,
    pub of: &'a OpenFile,
    pub aspace: &'a AddressSpace,
    pub src: UserPtr<u8>,
    pub len: usize,
}

impl<'a, I: SubjectIdentity> StepOp<I> for WriteFromUserOp<'a> {
    type Output = usize;
    type Progress = ByteProgress;
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        let __guard = step_engine::guard();
        step_write_from_user(self.pc, self.of, self.aspace, self.src, self.len, &__guard)
    }
}
