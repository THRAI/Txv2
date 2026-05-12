use super::*;
use crate::vm::AddressSpace;
use crate::page_backed::adapter::step_engine::{self as step_engine, ByteProgress, ScriptCtx, StepOp, StepOutcome, SubjectIdentity};

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
    use crate::page_backed::adapter::step_engine::StepOutcome as V3;
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

        // `materialize_page` is now v3
        // `StepOutcome<MaterializedPage, NoProgress>`. Map per variant:
        // - `Done` / `Continue { .. }` → run the user-side copy, then
        //   either continue the loop (on success) or terminate.
        // - `Yield { OnWaitSource .. }` with `advanced == 0` → v3 `Yield`
        //   with `ByteProgress::EMPTY`. Otherwise propagate the yield
        //   carrying the accumulated bytes.
        // - `Yield { .. }` (OnAgent) → unsupported; surface `Err(EIO)`
        //   if no progress yet, else partial `Done`.
        // - `Err(errno)` with `advanced == 0` → v3 `Err(errno)`.
        //   Otherwise return v3 `Done(advanced)` (partial-success).
use crate::page_backed::adapter::step_engine::YieldShape;
        match pc.materialize_page(page_index, access, guard) {
            StepOutcome::Done(materialized) => {
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
            StepOutcome::Continue { .. } => {
                // NoProgress wait source: no materialized frame; treat as
                // EAGAIN-like and surface partial progress (or EIO if
                // none) — page allocation rarely emits this.
                if advanced == 0 {
                    return V3::err(step_engine::Errno::EAGAIN);
                }
                of.set_offset(offset);
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
                of.set_offset(offset);
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
                of.set_offset(offset);
                return V3::done(advanced);
            }
            StepOutcome::Err(errno) => {
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
            use crate::page_backed::adapter::step_engine::StepOutcome as V3;
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
            use crate::page_backed::adapter::step_engine::StepOutcome as V3;
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
    use crate::page_backed::adapter::step_engine::{ByteProgress, StepOutcome as V3, YieldShape};
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
                if advanced == 0 {
                    return V3::err(step_engine::Errno::EAGAIN);
                }
                of.set_offset(offset);
                return V3::done(advanced);
            }
            V3::Yield {
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
                of.set_offset(offset);
                return V3::yield_on_wait_source(
                    ByteProgress::new(advanced),
                    carrier.raw(),
                    interests.raw(),
                );
            }
            V3::Yield { .. } => {
                if advanced == 0 {
                    return V3::err(step_engine::Errno::EIO);
                }
                of.set_offset(offset);
                return V3::done(advanced);
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
    pub guard: &'a Guard<'a>,
}

impl<'a, I: SubjectIdentity> StepOp<I>
    for ReadToUserOp<'a>
{
    type Output = usize;
    type Progress = ByteProgress;
    fn step(
        &mut self,
        _ctx: &mut ScriptCtx<I>,
    ) -> StepOutcome<Self::Output, Self::Progress> {
        step_read_to_user(
            self.pc,
            self.of,
            self.aspace,
            self.dst,
            self.len,
            self.guard,
        )
    }
}

/// `StepOp` wrap of [`step_write_from_user`].
pub struct WriteFromUserOp<'a> {
    pub pc: &'a PageContainer,
    pub of: &'a OpenFile,
    pub aspace: &'a AddressSpace,
    pub src: UserPtr<u8>,
    pub len: usize,
    pub guard: &'a Guard<'a>,
}

impl<'a, I: SubjectIdentity> StepOp<I>
    for WriteFromUserOp<'a>
{
    type Output = usize;
    type Progress = ByteProgress;
    fn step(
        &mut self,
        _ctx: &mut ScriptCtx<I>,
    ) -> StepOutcome<Self::Output, Self::Progress> {
        step_write_from_user(
            self.pc,
            self.of,
            self.aspace,
            self.src,
            self.len,
            self.guard,
        )
    }
}
