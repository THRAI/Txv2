//! `splice(2)` / `tee(2)` / `vmsplice(2)` v1 syscall shims.
//!
//! Pipe is an ordered descriptor transport: anonymous pipe pages and
//! PageBacked leases can both travel through its slots. PageBacked
//! owns lease/gift export and install policy; VM owns user-page
//! gift eligibility and freeze/detach before a gift descriptor is
//! published to a pipe.

use super::*;
use crate::adapter::step_engine::StepOutcome;
use tx_subsystems::vfs::structure::{InodeKind, OpenFileBacking, RNodeBacking, StructPayload};

const SPLICE_F_MOVE: u32 = 0x01;
const SPLICE_F_NONBLOCK: u32 = 0x02;
const SPLICE_F_MORE: u32 = 0x04;
const SPLICE_F_GIFT: u32 = 0x08;
const SPLICE_KNOWN_FLAGS: u32 = SPLICE_F_MOVE | SPLICE_F_NONBLOCK | SPLICE_F_MORE | SPLICE_F_GIFT;
const IOVEC_BYTES: u64 = 16;
const IOV_MAX: u64 = 1024;
const ESPIPE_VALUE: i32 = 29;

#[derive(Clone)]
struct PipeEnd {
    payload: Cap<tx_subsystems::pipe::PipePayload>,
    side: tx_subsystems::pipe::PipeSide,
    nonblocking: bool,
    packet: bool,
}

fn pipe_end(file: &Cap<OpenFile>) -> Option<PipeEnd> {
    match file.backing() {
        tx_subsystems::vfs::structure::OpenFileBacking::Rnode { rnode } => match rnode.backing() {
            RNodeBacking::StructBacked {
                payload: StructPayload::Pipe { payload, side },
            } => Some(PipeEnd {
                payload: payload.clone(),
                side: *side,
                nonblocking: file.flags().nonblocking,
                packet: file.flags().packet,
            }),
            _ => None,
        },
        _ => None,
    }
}

fn splice_nonblocking(flags: u32, input: bool, output: bool) -> bool {
    flags & SPLICE_F_NONBLOCK != 0 || input || output
}

fn validate_flags(flags: u32) -> Option<SyscallResult> {
    if flags & !SPLICE_KNOWN_FLAGS != 0 {
        Some(SyscallResult::Error(EINVAL_VALUE))
    } else {
        None
    }
}

fn splice_validate_read_file(file: &Cap<OpenFile>) -> Result<(), i32> {
    let OpenFileBacking::Rnode { rnode } = file.backing() else {
        return Err(EINVAL_VALUE);
    };
    if !file.flags().read {
        return Err(EBADF_VALUE);
    }
    if rnode.meta().kind() == InodeKind::Directory {
        return Err(EINVAL_VALUE);
    }
    if super::socket::socket_identity_from_file(file).is_ok()
        || file.socketpair_endpoint().is_some()
    {
        return Err(EINVAL_VALUE);
    }
    Ok(())
}

fn splice_validate_write_file(file: &Cap<OpenFile>) -> Result<(), i32> {
    let OpenFileBacking::Rnode { rnode } = file.backing() else {
        return Err(EINVAL_VALUE);
    };
    if !file.flags().write {
        return Err(EBADF_VALUE);
    }
    if rnode.meta().kind() == InodeKind::Directory {
        return Err(EINVAL_VALUE);
    }
    if super::socket::socket_identity_from_file(file).is_ok()
        || file.socketpair_endpoint().is_some()
    {
        return Err(EINVAL_VALUE);
    }
    Ok(())
}

pub(super) async fn sys_vmsplice<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let flags = args[3] as u32;
    if let Some(result) = validate_flags(flags) {
        return result;
    }
    let fd = args[0] as i32;
    let iov_ptr = args[1];
    let nr_segs = args[2];
    if fd < 0 {
        return SyscallResult::Error(EBADF_VALUE);
    }
    if nr_segs > IOV_MAX {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let file = match resolve_fd(&ctx.process, fd as u32) {
        Some(file) => file,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    let out_pipe = match pipe_end(&file) {
        Some(end) if end.side == tx_subsystems::pipe::PipeSide::Writer => end,
        Some(_) => return SyscallResult::Error(EBADF_VALUE),
        None => return SyscallResult::Error(EINVAL_VALUE),
    };
    if nr_segs == 0 {
        return SyscallResult::Return(0);
    }

    let mut total = 0i64;
    for idx in 0..nr_segs {
        let ent_ptr = iov_ptr.wrapping_add(idx * IOVEC_BYTES);
        let mut ent_bytes = [0u8; IOVEC_BYTES as usize];
        if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut ent_bytes, ent_ptr) {
            if total > 0 {
                return SyscallResult::Return(total);
            }
            return SyscallResult::error_from(errno);
        }
        let base = u64::from_le_bytes(ent_bytes[0..8].try_into().unwrap());
        let len = u64::from_le_bytes(ent_bytes[8..16].try_into().unwrap());
        if len == 0 {
            continue;
        }

        if flags & SPLICE_F_GIFT != 0 {
            if let Some(result) = try_vmsplice_gift_to_pipe(&out_pipe, base, len, flags, ctx) {
                match result {
                    SyscallResult::Return(n) => {
                        total += n;
                        if (n as u64) < len {
                            return SyscallResult::Return(total);
                        }
                        continue;
                    }
                    SyscallResult::Error(e) => {
                        if total > 0 {
                            return SyscallResult::Return(total);
                        }
                        return SyscallResult::Error(e);
                    }
                    other => return other,
                }
            }
        }

        let result = sys_write([fd as u64, base, len, 0, 0, 0], ctx).await;
        match result {
            SyscallResult::Return(n) => {
                total += n;
                if (n as u64) < len {
                    return SyscallResult::Return(total);
                }
            }
            SyscallResult::Error(e) => {
                if total > 0 {
                    return SyscallResult::Return(total);
                }
                return SyscallResult::Error(e);
            }
            other => return other,
        }
    }
    SyscallResult::Return(total)
}

fn try_vmsplice_gift_to_pipe<'a>(
    out_pipe: &PipeEnd,
    base: u64,
    len: u64,
    flags: u32,
    ctx: &SyscallCtx<'a>,
) -> Option<SyscallResult> {
    let page_size = tx_subsystems::vm::USER_PAGE_SIZE;
    if len != page_size as u64 {
        return None;
    }
    let base = usize::try_from(base).ok()?;
    if !base.is_multiple_of(page_size) {
        return None;
    }
    let range = match UserRange::new_aligned(UserVirtAddr(base), page_size) {
        Ok(range) => range,
        Err(_) => return None,
    };
    let nonblocking = splice_nonblocking(flags, false, out_pipe.nonblocking);
    let slot = {
        let guard = step_engine::guard();
        match tx_subsystems::pipe::step_reserve_user_page_gift_slot(
            &out_pipe.payload,
            &guard,
            nonblocking,
            out_pipe.packet,
        ) {
            StepOutcome::Done(slot) => slot,
            StepOutcome::Err(e) => return Some(splice_outcome_to_result(StepOutcome::Err(e))),
            StepOutcome::Yield { .. } | StepOutcome::Continue { .. } => {
                return Some(SyscallResult::Error(EAGAIN_VALUE));
            }
        }
    };
    let batch = match ctx.aspace.gift_user_pages_step(ctx.aspace.clone(), range) {
        StepOutcome::Done(batch) => batch,
        StepOutcome::Err(e) => return Some(SyscallResult::error_from(e.into())),
        StepOutcome::Yield { .. } | StepOutcome::Continue { .. } => {
            return Some(SyscallResult::Error(EAGAIN_VALUE));
        }
    };
    if batch.is_empty() {
        return None;
    }
    if batch.bytes() != page_size || batch.gift_count() != 1 {
        return Some(SyscallResult::Error(EINVAL_VALUE));
    }
    let mut gifts = batch.into_gifts();
    let gift = gifts.pop().expect("gift_count checked");
    let outcome = {
        let guard = step_engine::guard();
        slot.commit(gift, &guard)
    };
    Some(splice_outcome_to_result(outcome))
}

pub(super) fn sys_tee<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let fd_in = args[0] as i32;
    let fd_out = args[1] as i32;
    let len = args[2] as usize;
    let flags = args[3] as u32;
    if let Some(result) = validate_flags(flags) {
        return result;
    }
    if fd_in < 0 || fd_out < 0 {
        return SyscallResult::Error(EBADF_VALUE);
    }
    if len == 0 {
        return SyscallResult::Return(0);
    }
    let in_file = match resolve_fd(&ctx.process, fd_in as u32) {
        Some(file) => file,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    let out_file = match resolve_fd(&ctx.process, fd_out as u32) {
        Some(file) => file,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    let in_pipe = match pipe_end(&in_file) {
        Some(end) if end.side == tx_subsystems::pipe::PipeSide::Reader => end,
        Some(_) => return SyscallResult::Error(EBADF_VALUE),
        None => return SyscallResult::Error(EINVAL_VALUE),
    };
    let out_pipe = match pipe_end(&out_file) {
        Some(end) if end.side == tx_subsystems::pipe::PipeSide::Writer => end,
        Some(_) => return SyscallResult::Error(EBADF_VALUE),
        None => return SyscallResult::Error(EINVAL_VALUE),
    };
    let nonblocking = splice_nonblocking(flags, in_pipe.nonblocking, out_pipe.nonblocking);
    let guard = step_engine::guard();
    let outcome = tx_subsystems::pipe::step_tee_to_pipe(
        &in_pipe.payload,
        &out_pipe.payload,
        len,
        &guard,
        nonblocking,
        out_pipe.packet,
    );
    drop(guard);
    splice_outcome_to_result(outcome)
}

pub(super) async fn sys_splice<'a, P: tx_hal::TimeIf + tx_hal::ConsoleIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'a>,
) -> SyscallResult
where
    TimekeeperClock<P>: ClockRead,
{
    let fd_in = args[0] as i32;
    let off_in_ptr = args[1];
    let fd_out = args[2] as i32;
    let off_out_ptr = args[3];
    let len = args[4] as usize;
    let flags = args[5] as u32;
    if let Some(result) = validate_flags(flags) {
        return result;
    }
    if fd_in < 0 || fd_out < 0 {
        return SyscallResult::Error(EBADF_VALUE);
    }
    if len == 0 {
        return SyscallResult::Return(0);
    }
    let in_file = match resolve_fd(&ctx.process, fd_in as u32) {
        Some(file) => file,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    let out_file = match resolve_fd(&ctx.process, fd_out as u32) {
        Some(file) => file,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    let in_pipe = pipe_end(&in_file);
    let out_pipe = pipe_end(&out_file);
    if in_pipe.is_none() && out_pipe.is_none() {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if in_pipe.is_some() && off_in_ptr != 0 {
        return SyscallResult::Error(ESPIPE_VALUE);
    }
    if out_pipe.is_some() && off_out_ptr != 0 {
        return SyscallResult::Error(ESPIPE_VALUE);
    }

    match (in_pipe, out_pipe) {
        (Some(input), Some(output)) => {
            if input.side != tx_subsystems::pipe::PipeSide::Reader
                || output.side != tx_subsystems::pipe::PipeSide::Writer
            {
                return SyscallResult::Error(EBADF_VALUE);
            }
            let nonblocking = splice_nonblocking(flags, input.nonblocking, output.nonblocking);
            let outcome = {
                let guard = step_engine::guard();
                tx_subsystems::pipe::step_splice_to_pipe(
                    &input.payload,
                    &output.payload,
                    len,
                    &guard,
                    nonblocking,
                    output.packet,
                )
            };
            splice_outcome_to_result(outcome)
        }
        (Some(input), None) => {
            if input.side != tx_subsystems::pipe::PipeSide::Reader {
                return SyscallResult::Error(EBADF_VALUE);
            }
            splice_pipe_to_file::<P>(fd_in, fd_out, off_out_ptr, len, ctx).await
        }
        (None, Some(output)) => {
            if output.side != tx_subsystems::pipe::PipeSide::Writer {
                return SyscallResult::Error(EBADF_VALUE);
            }
            splice_file_to_pipe::<P>(fd_in, off_in_ptr, fd_out, len, ctx).await
        }
        (None, None) => SyscallResult::Error(EINVAL_VALUE),
    }
}

async fn splice_pipe_to_file<'a, P: tx_hal::TimeIf + tx_hal::ConsoleIf>(
    fd_in: i32,
    fd_out: i32,
    off_out_ptr: u64,
    len: usize,
    ctx: &SyscallCtx<'a>,
) -> SyscallResult
where
    TimekeeperClock<P>: ClockRead,
{
    let in_file = match resolve_fd(&ctx.process, fd_in as u32) {
        Some(file) => file,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    let in_pipe = match pipe_end(&in_file) {
        Some(end) => end,
        None => return SyscallResult::Error(EINVAL_VALUE),
    };
    let out_file = match resolve_fd(&ctx.process, fd_out as u32) {
        Some(file) => file,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    if let Err(errno) = splice_validate_write_file(&out_file) {
        return SyscallResult::Error(errno);
    }
    if let Some(result) = try_splice_pipe_gift_to_file(fd_out, off_out_ptr, &in_pipe, len, ctx) {
        return result;
    }
    if let Some(result) = try_splice_pipe_lease_to_file(fd_out, off_out_ptr, &in_pipe, len, ctx) {
        return result;
    }
    let mut buf = alloc::vec::Vec::new();
    buf.resize(len, 0);
    let peek = {
        let guard = step_engine::guard();
        tx_subsystems::pipe::step_peek(&in_pipe.payload, &mut buf, &guard, in_pipe.nonblocking)
    };

    match splice_outcome_to_result(peek) {
        SyscallResult::Return(n) if n <= 0 => SyscallResult::Return(n),
        SyscallResult::Return(n) => {
            let n = n as usize;
            let saved = if off_out_ptr != 0 {
                let out_file = match resolve_fd(&ctx.process, fd_out as u32) {
                    Some(file) => file,
                    None => return SyscallResult::Error(EBADF_VALUE),
                };
                let offset = match bootstrap_read_user::<u64>(&ctx.aspace, off_out_ptr) {
                    Ok(offset) => offset,
                    Err(errno) => return SyscallResult::error_from(errno),
                };
                let saved = out_file.offset();
                out_file.set_offset(offset);
                Some((out_file, saved, offset))
            } else {
                None
            };
            let result = write_file_from_kernel(fd_out, &buf[..n], ctx).await;
            let mut written_len = None;
            if let Some((file, saved, offset)) = saved {
                if let SyscallResult::Return(written) = result {
                    let _ = bootstrap_write_user::<u64>(
                        &ctx.aspace,
                        off_out_ptr,
                        offset + written as u64,
                    );
                    written_len = Some(written as usize);
                }
                file.set_offset(saved);
            } else if let SyscallResult::Return(n) = result {
                written_len = Some(n as usize);
            }

            if let Some(n) = written_len {
                let mut discard = alloc::vec::Vec::new();
                discard.resize(n, 0);
                let _ = sys_read::<P>(
                    [fd_in as u64, discard.as_mut_ptr() as u64, n as u64, 0, 0, 0],
                    ctx,
                )
                .await;
            }
            result
        }
        other => other,
    }
}

fn try_splice_pipe_gift_to_file<'a>(
    fd_out: i32,
    off_out_ptr: u64,
    in_pipe: &PipeEnd,
    len: usize,
    ctx: &SyscallCtx<'a>,
) -> Option<SyscallResult> {
    let out_file = resolve_fd(&ctx.process, fd_out as u32)?;
    let OpenFileBacking::Rnode { rnode } = out_file.backing() else {
        return None;
    };
    let RNodeBacking::PageBacked { pc } = rnode.backing() else {
        return None;
    };
    let out_offset = if off_out_ptr != 0 {
        match bootstrap_read_user::<u64>(&ctx.aspace, off_out_ptr) {
            Ok(offset) => offset,
            Err(errno) => return Some(SyscallResult::error_from(errno)),
        }
    } else {
        out_file.offset()
    };
    let page_size = tx_subsystems::vm::USER_PAGE_SIZE as u64;
    if !out_offset.is_multiple_of(page_size) || len < tx_subsystems::vm::USER_PAGE_SIZE {
        return None;
    }
    let gift_outcome = {
        let guard = step_engine::guard();
        tx_subsystems::pipe::step_pop_user_page_gift(&in_pipe.payload, &guard, in_pipe.nonblocking)
    };
    let Some(gift) = (match gift_outcome {
        StepOutcome::Done(value) => value,
        StepOutcome::Err(e) => return Some(splice_outcome_to_result(StepOutcome::Err(e))),
        StepOutcome::Yield { .. } | StepOutcome::Continue { .. } => {
            return Some(SyscallResult::Error(EAGAIN_VALUE));
        }
    }) else {
        return None;
    };
    let gift_len = gift.len();
    if gift_len != tx_subsystems::vm::USER_PAGE_SIZE || gift_len > len {
        return Some(SyscallResult::Error(EINVAL_VALUE));
    }
    let page = tx_subsystems::page_backed::PageIndex::new(out_offset / page_size);
    match pc.install_user_gift_or_copy(page, gift) {
        Ok(_installed_or_copied) => {
            pc.set_size_bytes(out_offset + gift_len as u64);
            if off_out_ptr != 0 {
                let _ = bootstrap_write_user::<u64>(
                    &ctx.aspace,
                    off_out_ptr,
                    out_offset + gift_len as u64,
                );
            } else {
                out_file.set_offset(out_offset + gift_len as u64);
            }
            Some(SyscallResult::Return(gift_len as i64))
        }
        Err(_) => Some(SyscallResult::Error(EINVAL_VALUE)),
    }
}

fn try_splice_pipe_lease_to_file<'a>(
    fd_out: i32,
    off_out_ptr: u64,
    in_pipe: &PipeEnd,
    len: usize,
    ctx: &SyscallCtx<'a>,
) -> Option<SyscallResult> {
    let out_file = resolve_fd(&ctx.process, fd_out as u32)?;
    let OpenFileBacking::Rnode { rnode } = out_file.backing() else {
        return None;
    };
    let RNodeBacking::PageBacked { pc } = rnode.backing() else {
        return None;
    };
    let out_offset = if off_out_ptr != 0 {
        match bootstrap_read_user::<u64>(&ctx.aspace, off_out_ptr) {
            Ok(offset) => offset,
            Err(errno) => return Some(SyscallResult::error_from(errno)),
        }
    } else {
        out_file.offset()
    };
    let page_size = tx_subsystems::vm::USER_PAGE_SIZE as u64;
    if !out_offset.is_multiple_of(page_size) || len < tx_subsystems::vm::USER_PAGE_SIZE {
        return None;
    }
    let lease_outcome = {
        let guard = step_engine::guard();
        tx_subsystems::pipe::step_pop_page_lease(&in_pipe.payload, &guard, in_pipe.nonblocking)
    };
    let Some((lease, in_offset, lease_len)) = (match lease_outcome {
        StepOutcome::Done(value) => value,
        StepOutcome::Err(e) => return Some(splice_outcome_to_result(StepOutcome::Err(e))),
        StepOutcome::Yield { .. } | StepOutcome::Continue { .. } => {
            return Some(SyscallResult::Error(EAGAIN_VALUE));
        }
    }) else {
        return None;
    };
    if in_offset != 0 || lease_len != tx_subsystems::vm::USER_PAGE_SIZE || lease_len > len {
        return None;
    }
    let page = tx_subsystems::page_backed::PageIndex::new(out_offset / page_size);
    match pc.install_page_lease_or_copy(page, lease) {
        Ok(_shared_or_copied) => {
            pc.set_size_bytes(out_offset + lease_len as u64);
            if off_out_ptr != 0 {
                let _ = bootstrap_write_user::<u64>(
                    &ctx.aspace,
                    off_out_ptr,
                    out_offset + lease_len as u64,
                );
            } else {
                out_file.set_offset(out_offset + lease_len as u64);
            }
            Some(SyscallResult::Return(lease_len as i64))
        }
        Err(_) => None,
    }
}

async fn splice_file_to_pipe<'a, P: tx_hal::TimeIf + tx_hal::ConsoleIf>(
    fd_in: i32,
    off_in_ptr: u64,
    fd_out: i32,
    len: usize,
    ctx: &SyscallCtx<'a>,
) -> SyscallResult
where
    TimekeeperClock<P>: ClockRead,
{
    if let Some(result) = try_splice_file_lease_to_pipe(fd_in, off_in_ptr, fd_out, len, ctx) {
        return result;
    }
    let in_file = match resolve_fd(&ctx.process, fd_in as u32) {
        Some(file) => file,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    if let Err(errno) = splice_validate_read_file(&in_file) {
        return SyscallResult::Error(errno);
    }
    let mut buf = alloc::vec::Vec::new();
    buf.resize(len, 0);
    let saved = if off_in_ptr != 0 {
        let in_file = match resolve_fd(&ctx.process, fd_in as u32) {
            Some(file) => file,
            None => return SyscallResult::Error(EBADF_VALUE),
        };
        let offset = match bootstrap_read_user::<u64>(&ctx.aspace, off_in_ptr) {
            Ok(offset) => offset,
            Err(errno) => return SyscallResult::error_from(errno),
        };
        let saved = in_file.offset();
        in_file.set_offset(offset);
        Some((in_file, saved, offset))
    } else {
        None
    };
    let original_offset = if off_in_ptr == 0 {
        resolve_fd(&ctx.process, fd_in as u32).map(|file| (file.clone(), file.offset()))
    } else {
        None
    };
    let read_result = read_file_to_kernel::<P>(fd_in, &mut buf, ctx).await;
    if let Some((file, saved_offset, _)) = &saved {
        file.set_offset(*saved_offset);
    }
    let n = match read_result {
        SyscallResult::Return(n) if n <= 0 => return SyscallResult::Return(n),
        SyscallResult::Return(n) => n as usize,
        other => return other,
    };
    let write_result = write_file_from_kernel(fd_out, &buf[..n], ctx).await;
    if let (Some((_, _, offset)), SyscallResult::Return(written)) = (saved, write_result) {
        let _ = bootstrap_write_user::<u64>(&ctx.aspace, off_in_ptr, offset + written as u64);
        return SyscallResult::Return(written);
    }
    if let Some((file, original)) = original_offset {
        match write_result {
            SyscallResult::Return(written) => {
                file.set_offset(original + written as u64);
                return SyscallResult::Return(written);
            }
            SyscallResult::Error(errno) => {
                file.set_offset(original);
                return SyscallResult::Error(errno);
            }
            other => return other,
        }
    }
    write_result
}

fn try_splice_file_lease_to_pipe<'a>(
    fd_in: i32,
    off_in_ptr: u64,
    fd_out: i32,
    len: usize,
    ctx: &SyscallCtx<'a>,
) -> Option<SyscallResult> {
    if len < tx_subsystems::vm::USER_PAGE_SIZE {
        return None;
    }
    let in_file = resolve_fd(&ctx.process, fd_in as u32)?;
    let out_file = resolve_fd(&ctx.process, fd_out as u32)?;
    let out_pipe = pipe_end(&out_file)?;
    let OpenFileBacking::Rnode { rnode } = in_file.backing() else {
        return None;
    };
    let RNodeBacking::PageBacked { pc } = rnode.backing() else {
        return None;
    };
    let in_offset = if off_in_ptr != 0 {
        match bootstrap_read_user::<u64>(&ctx.aspace, off_in_ptr) {
            Ok(offset) => offset,
            Err(errno) => return Some(SyscallResult::error_from(errno)),
        }
    } else {
        in_file.offset()
    };
    let page_size = tx_subsystems::vm::USER_PAGE_SIZE as u64;
    if !in_offset.is_multiple_of(page_size) {
        return None;
    }
    let valid_end = pc.size_bytes();
    if in_offset >= valid_end {
        return Some(SyscallResult::Return(0));
    }
    let lease_len = core::cmp::min(
        tx_subsystems::vm::USER_PAGE_SIZE,
        core::cmp::min(len, (valid_end - in_offset) as usize),
    );
    if lease_len != tx_subsystems::vm::USER_PAGE_SIZE {
        return None;
    }
    let outcome = {
        let guard = step_engine::guard();
        let lease = match pc.export_page_lease(
            tx_subsystems::page_backed::PageIndex::new(in_offset / page_size),
            &guard,
        ) {
            StepOutcome::Done(lease) => lease,
            StepOutcome::Err(e) => return Some(splice_outcome_to_result(StepOutcome::Err(e))),
            StepOutcome::Yield { .. } | StepOutcome::Continue { .. } => {
                return Some(SyscallResult::Error(EAGAIN_VALUE));
            }
        };
        tx_subsystems::pipe::step_push_page_lease(
            &out_pipe.payload,
            lease,
            0,
            lease_len,
            &guard,
            out_pipe.nonblocking,
            out_pipe.packet,
        )
    };
    match splice_outcome_to_result(outcome) {
        SyscallResult::Return(n) if n > 0 => {
            if off_in_ptr != 0 {
                let _ = bootstrap_write_user::<u64>(&ctx.aspace, off_in_ptr, in_offset + n as u64);
            } else {
                in_file.set_offset(in_offset + n as u64);
            }
            Some(SyscallResult::Return(n))
        }
        other => Some(other),
    }
}

async fn read_file_to_kernel<'a, P: tx_hal::TimeIf + tx_hal::ConsoleIf>(
    fd: i32,
    buf: &mut [u8],
    ctx: &SyscallCtx<'a>,
) -> SyscallResult
where
    TimekeeperClock<P>: ClockRead,
{
    let file = match resolve_fd(&ctx.process, fd as u32) {
        Some(file) => file,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    let OpenFileBacking::Rnode { rnode } = file.backing() else {
        return SyscallResult::Error(EINVAL_VALUE);
    };
    if let Err(errno) = splice_validate_read_file(&file) {
        return SyscallResult::Error(errno);
    }
    if let RNodeBacking::PageBacked { pc } = rnode.backing() {
        let outcome = {
            let guard = step_engine::guard();
            tx_subsystems::page_backed::step_read_to_kernel(pc, &file, buf, &guard)
        };
        return splice_outcome_to_result(outcome);
    }
    sys_read::<P>(
        [
            fd as u64,
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
            0,
            0,
            0,
        ],
        ctx,
    )
    .await
}

async fn write_file_from_kernel<'a>(fd: i32, buf: &[u8], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let file = match resolve_fd(&ctx.process, fd as u32) {
        Some(file) => file,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    let OpenFileBacking::Rnode { rnode } = file.backing() else {
        return SyscallResult::Error(EINVAL_VALUE);
    };
    if let Err(errno) = splice_validate_write_file(&file) {
        return SyscallResult::Error(errno);
    }
    if let RNodeBacking::PageBacked { pc } = rnode.backing() {
        let outcome = {
            let guard = step_engine::guard();
            tx_subsystems::page_backed::step_write_from_kernel(pc, &file, buf, &guard)
        };
        return splice_outcome_to_result(outcome);
    }
    sys_write(
        [fd as u64, buf.as_ptr() as u64, buf.len() as u64, 0, 0, 0],
        ctx,
    )
    .await
}

fn splice_outcome_to_result(
    outcome: StepOutcome<usize, tx_substrate::step::ByteProgress>,
) -> SyscallResult {
    match outcome {
        StepOutcome::Done(n) => SyscallResult::Return(n as i64),
        StepOutcome::Err(e) => {
            let errno: tx_subsystems::execution::Errno = e.into();
            SyscallResult::error_from(errno)
        }
        StepOutcome::Yield { .. } | StepOutcome::Continue { .. } => {
            SyscallResult::Error(EAGAIN_VALUE)
        }
    }
}
