//! Raw Linux AIO syscall arms.
//!
//! Spec:
//! - `docs/progress/decisions/2026-05-26-raw-linux-aio-abi.md`
//! - `docs/Txv3/06_EXECUTION_SCOPE_v1.md` (`OnBehalfOf<P>` execution
//!   scope — the AIO worker runs under `with_on_behalf_of`)
//! - Linux `include/uapi/linux/aio_abi.h`, `fs/aio.c`
//!
//! # What lands here
//!
//! `io_setup(nr_events, ctxp)` writes a Linux-shaped user `aio_context_t`
//! value that identifies the mapped ring. `io_submit` validates and admits
//! IOCBs only; a long-lived AIO worker reactor task drains the queue under
//! `OnBehalfOf<P>`, writes `struct io_event` records into the user-visible
//! ring, signals optional eventfd completions, and wakes `io_getevents`.
//! Host tests without the boot-reactor hook use the test worker registry to
//! poll that same worker future explicitly.
//!
//! [`AioContext`]: tx_subsystems::aio::AioContext
//! [`AioWorkerFuture`]: tx_subsystems::aio::AioWorkerFuture

extern crate alloc;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

use tx_subsystems::aio::{
    is_valid_iocb_opcode, spawn_worker_for_context, AioContext, AioWorkerFuture, CompletionPublish,
    CompletionPublisher, IoEvent, Iocb, IocbDispatcher, EVENTS_AVAILABLE_MASK, IOCB_CMD_FDSYNC,
    IOCB_CMD_FSYNC, IOCB_CMD_NOOP, IOCB_CMD_POLL, IOCB_CMD_PREAD, IOCB_CMD_PREADV, IOCB_CMD_PWRITE,
    IOCB_CMD_PWRITEV, IO_EVENT_BYTES,
};
use tx_subsystems::eventfd::step_eventfd_write;
use tx_subsystems::page_backed::{AnonSwapPolicy, PageContainer, PageContainerKind};
use tx_subsystems::process::ProcessIdentity;
use tx_subsystems::signal::SignalMask;
use tx_subsystems::thread_runtime::execution::{step_sigprocmask, SigmaskHow, SigprocmaskChange};
use tx_subsystems::vfs::execution::{OpenFileLseekOp, OpenFileReadOp, OpenFileWriteOp};
use tx_subsystems::vfs::OpenFile;
use tx_subsystems::vm::{
    AddressSpace, MapPlacement, Prot, UserRange, UserVirtAddr, VmBacking, VmEntryFlags,
    VmMapRequest, USER_PAGE_SIZE,
};

use super::{bootstrap_copy_from_user, bootstrap_copy_to_user, SyscallCtx, SyscallResult};
use super::{
    EAGAIN_VALUE, EBADF_VALUE, EFAULT_VALUE, EINTR_VALUE, EINVAL_VALUE, ENOMEM_VALUE, ENOSYS_VALUE,
};
use crate::adapter::step_engine::StepOutcome as V3Out;
use crate::adapter::step_engine::{
    self as step_engine, Cap, InterestMask, SpinMutex, StepOp, WaitSourceId,
};

// === Linux negative-errno values used by the dispatcher =============
//
// The dispatcher returns negative errno values per Linux's
// `struct io_event.res` convention. We keep these inline (rather than
// importing from `super::*`) because the dispatcher closure is
// `'static` — it cannot reference `super::EBADF_VALUE` directly unless
// it's a `const`, which it already is.

const NEG_EBADF: i64 = -(EBADF_VALUE as i64);
const NEG_EINVAL: i64 = -(EINVAL_VALUE as i64);
const NEG_EIO: i64 = -5;
const NEG_EFAULT: i64 = -(EFAULT_VALUE as i64);
const NEG_ENOSYS: i64 = -(ENOSYS_VALUE as i64);
const NEG_ECANCELED: i64 = -125;

const AIO_RING_MAGIC: u32 = 0xa10a10a1;
const AIO_RING_COMPAT_FEATURES: u32 = 1;
const AIO_RING_INCOMPAT_FEATURES: u32 = 0;
const AIO_RING_HEADER_BYTES: usize = 128;
const IOCB_FLAG_RESFD: u32 = 1 << 0;
const IOCB_FLAG_IOPRIO: u32 = 1 << 1;
const RAW_AIO_RING_BASE_START: u64 = 0x6200_0000;
const RAW_AIO_RING_STRIDE: u64 = 0x0010_0000;

#[derive(Clone)]
struct RawAioContext {
    aio_cap: Cap<AioContext>,
    ring_base: u64,
    nr_events: u32,
}

static RAW_AIO_CONTEXTS: SpinMutex<BTreeMap<u64, RawAioContext>> = SpinMutex::new(BTreeMap::new());
static NEXT_RAW_AIO_RING_BASE: AtomicU64 = AtomicU64::new(RAW_AIO_RING_BASE_START);

/// Construct the per-context iocb dispatcher.
///
/// Per D8 §7 / phase 3: for each iocb the worker pops off the submit
/// queue, the dispatcher is invoked under the `with_on_behalf_of`
/// borrow. The closure resolves `aio_fildes` against the principal's
/// fd table (`process.fd(...)`), `aio_buf` against the principal's
/// address space, and dispatches PREAD/PWRITE through the existing
/// VFS step ops (`OpenFileLseekOp` + `OpenFileReadOp`/`OpenFileWriteOp`).
/// Other opcodes (FSYNC/FDSYNC/NOOP/PREADV/PWRITEV) return `-EINVAL`
/// for now — the canary phase ships buffered `PREAD`+`PWRITE` only.
///
/// The closure is `Send + Sync + 'static` — see [`IocbDispatcher`]
/// for the rationale.
fn build_iocb_dispatcher(
    process: Cap<ProcessIdentity>,
    aspace: Cap<AddressSpace>,
) -> IocbDispatcher {
    Arc::new(move |iocb: &Iocb| -> IoEvent { dispatch_one_iocb(&process, &aspace, iocb) })
}

fn build_completion_publisher(
    raw: RawAioContext,
    aspace: Cap<AddressSpace>,
    process: Cap<ProcessIdentity>,
) -> CompletionPublisher {
    Arc::new(move |iocb: &Iocb, event: IoEvent| -> CompletionPublish {
        if raw_push_completion(&raw, &aspace, event).is_err() {
            return CompletionPublish::WouldBlock;
        }
        if (iocb.aio_flags & IOCB_FLAG_RESFD) != 0 {
            if let Some(efd) = process
                .fd(iocb.aio_resfd)
                .and_then(|file| file.eventfd().cloned())
            {
                let _ = step_eventfd_write(&efd, 1, true);
            }
        }
        CompletionPublish::Published
    })
}

/// Per-iocb dispatch body. Called synchronously by the worker for each
/// iocb. Returns the [`IoEvent`] to post into the completion queue.
fn dispatch_one_iocb(
    process: &Cap<ProcessIdentity>,
    aspace: &Cap<AddressSpace>,
    iocb: &Iocb,
) -> IoEvent {
    let cookie = iocb.aio_data;
    let obj = iocb.aio_user_ptr;
    if iocb.aio_rw_flags != 0 {
        return IoEvent::new(cookie, obj, NEG_ENOSYS, 0);
    }
    match iocb.aio_lio_opcode {
        IOCB_CMD_PREAD => dispatch_pread(process, aspace, iocb, cookie),
        IOCB_CMD_PWRITE => dispatch_pwrite(process, aspace, iocb, cookie),
        IOCB_CMD_PREADV => dispatch_preadv(process, aspace, iocb, cookie),
        IOCB_CMD_PWRITEV => dispatch_pwritev(process, aspace, iocb, cookie),
        IOCB_CMD_NOOP | IOCB_CMD_FSYNC | IOCB_CMD_FDSYNC => IoEvent::new(cookie, obj, 0, 0),
        IOCB_CMD_POLL => IoEvent::new(cookie, obj, NEG_ENOSYS, 0),
        _ => IoEvent::new(cookie, obj, NEG_EINVAL, 0),
    }
}

/// Dispatch one `IOCB_CMD_PREAD`. Resolves `aio_fildes` against the
/// principal's fd table, seeks to `aio_offset`, reads up to
/// `aio_nbytes` into a kernel-staging buffer, then copies the result
/// into P's address space at `aio_buf`. Returns an `IoEvent` whose
/// `res` is the bytes read on success or `-errno` on failure.
fn dispatch_pread(
    process: &Cap<ProcessIdentity>,
    aspace: &Cap<AddressSpace>,
    iocb: &Iocb,
    cookie: u64,
) -> IoEvent {
    let file = match process.fd(iocb.aio_fildes) {
        Some(f) => f,
        None => return IoEvent::new(cookie, iocb.aio_user_ptr, NEG_EBADF, 0),
    };
    let len = iocb.aio_nbytes as usize;
    if len == 0 {
        return IoEvent::new(cookie, iocb.aio_user_ptr, 0, 0);
    }
    let saved_offset = file.offset();
    // Seek to the requested offset. SEEK_SET = 0. step_lseek returns
    // ESPIPE for non-seekable backings; surface as -EINVAL since
    // PREAD against a non-seekable backing is not meaningful.
    if !run_lseek_set(&file, iocb.aio_offset) {
        return IoEvent::new(cookie, iocb.aio_user_ptr, NEG_EINVAL, 0);
    }
    let mut staging: alloc::vec::Vec<u8> = alloc::vec![0u8; len];
    let read_result = run_read(&file, &mut staging);
    file.set_offset(saved_offset);
    match read_result {
        Ok(bytes) => {
            if bytes > 0 {
                if let Err(errno) = bootstrap_copy_to_user(aspace, iocb.aio_buf, &staging[..bytes])
                {
                    let _ = errno;
                    return IoEvent::new(cookie, iocb.aio_user_ptr, NEG_EFAULT, 0);
                }
            }
            IoEvent::new(cookie, iocb.aio_user_ptr, bytes as i64, 0)
        }
        Err(neg) => IoEvent::new(cookie, iocb.aio_user_ptr, neg, 0),
    }
}

/// Dispatch one `IOCB_CMD_PWRITE`. Resolves `aio_fildes`, seeks to
/// `aio_offset`, copies the user buffer into a kernel-staging buffer,
/// then writes through `step_write`. Returns bytes written or `-errno`.
fn dispatch_pwrite(
    process: &Cap<ProcessIdentity>,
    aspace: &Cap<AddressSpace>,
    iocb: &Iocb,
    cookie: u64,
) -> IoEvent {
    let file = match process.fd(iocb.aio_fildes) {
        Some(f) => f,
        None => return IoEvent::new(cookie, iocb.aio_user_ptr, NEG_EBADF, 0),
    };
    let len = iocb.aio_nbytes as usize;
    if len == 0 {
        return IoEvent::new(cookie, iocb.aio_user_ptr, 0, 0);
    }
    let mut staging: alloc::vec::Vec<u8> = alloc::vec![0u8; len];
    if let Err(_errno) = bootstrap_copy_from_user(aspace, &mut staging, iocb.aio_buf) {
        return IoEvent::new(cookie, iocb.aio_user_ptr, NEG_EFAULT, 0);
    }
    let saved_offset = file.offset();
    if !run_lseek_set(&file, iocb.aio_offset) {
        return IoEvent::new(cookie, iocb.aio_user_ptr, NEG_EINVAL, 0);
    }
    let result = match run_write(&file, &staging) {
        Ok(bytes) => IoEvent::new(cookie, iocb.aio_user_ptr, bytes as i64, 0),
        Err(neg) => IoEvent::new(cookie, iocb.aio_user_ptr, neg, 0),
    };
    file.set_offset(saved_offset);
    result
}

fn dispatch_preadv(
    process: &Cap<ProcessIdentity>,
    aspace: &Cap<AddressSpace>,
    iocb: &Iocb,
    cookie: u64,
) -> IoEvent {
    let file = match process.fd(iocb.aio_fildes) {
        Some(f) => f,
        None => return IoEvent::new(cookie, iocb.aio_user_ptr, NEG_EBADF, 0),
    };
    let saved_offset = file.offset();
    if !run_lseek_set(&file, iocb.aio_offset) {
        return IoEvent::new(cookie, iocb.aio_user_ptr, NEG_EINVAL, 0);
    }
    let result = run_iovec_read(&file, aspace, iocb.aio_buf, iocb.aio_nbytes as usize);
    file.set_offset(saved_offset);
    match result {
        Ok(bytes) => IoEvent::new(cookie, iocb.aio_user_ptr, bytes as i64, 0),
        Err(neg) => IoEvent::new(cookie, iocb.aio_user_ptr, neg, 0),
    }
}

fn dispatch_pwritev(
    process: &Cap<ProcessIdentity>,
    aspace: &Cap<AddressSpace>,
    iocb: &Iocb,
    cookie: u64,
) -> IoEvent {
    let file = match process.fd(iocb.aio_fildes) {
        Some(f) => f,
        None => return IoEvent::new(cookie, iocb.aio_user_ptr, NEG_EBADF, 0),
    };
    let saved_offset = file.offset();
    if !run_lseek_set(&file, iocb.aio_offset) {
        return IoEvent::new(cookie, iocb.aio_user_ptr, NEG_EINVAL, 0);
    }
    let result = run_iovec_write(&file, aspace, iocb.aio_buf, iocb.aio_nbytes as usize);
    file.set_offset(saved_offset);
    match result {
        Ok(bytes) => IoEvent::new(cookie, iocb.aio_user_ptr, bytes as i64, 0),
        Err(neg) => IoEvent::new(cookie, iocb.aio_user_ptr, neg, 0),
    }
}

/// Run `OpenFileLseekOp` with `whence = SEEK_SET (0)` synchronously.
/// Returns `true` on success, `false` on any non-`Done`/`Continue`
/// outcome. The seek is synchronous in this kernel; the loop is
/// defensive against unexpected Yield/Err shapes.
fn run_lseek_set(file: &Cap<OpenFile>, offset: i64) -> bool {
    let mut script_ctx = crate::KernelScriptCtx::new();
    let mut op = OpenFileLseekOp {
        file,
        offset,
        whence: 0, // SEEK_SET
    };
    matches!(
        op.step(&mut script_ctx),
        V3Out::Done(_) | V3Out::Continue { .. }
    )
}

/// Run `OpenFileReadOp` synchronously, looping on `Continue { progress }`
/// outcomes. Returns `Ok(bytes_read)` or `Err(-errno)`. `Yield` outcomes
/// surface `-EIO` for the canary — production wiring will park on the
/// returned `WaitSource`, but the dispatcher closure runs synchronously
/// inside the worker body and cannot `.await`.
fn run_read(file: &Cap<OpenFile>, out: &mut [u8]) -> Result<usize, i64> {
    let mut script_ctx = crate::KernelScriptCtx::new();
    let mut total: usize = 0;
    let total_len = out.len();
    loop {
        let outcome = {
            // Op acquires its own guard inside `step()` (STEP_MODEL_v2 §1).
            let mut op = OpenFileReadOp {
                file,
                out: &mut out[total..],
                caller_netns: None,
                cursor: 0,
            };
            op.step(&mut script_ctx)
        };
        match outcome {
            V3Out::Done(bytes) => return Ok(total + bytes),
            V3Out::Continue { progress } => {
                let bytes = progress.bytes();
                total += bytes;
                if bytes == 0 || total >= total_len {
                    return Ok(total);
                }
            }
            V3Out::Yield { progress, .. } => {
                // Canary: any Yield (OnWaitSource / OnTimer / OnAgent)
                // surfaces as a short-read of whatever progress we
                // already have, or -EIO if nothing yet. Production
                // wiring would park; the dispatcher closure cannot
                // `.await` so we degrade.
                let bytes = progress.bytes();
                total += bytes;
                if total > 0 {
                    return Ok(total);
                }
                return Err(NEG_EIO);
            }
            V3Out::Err(v3errno) => {
                if total > 0 {
                    return Ok(total);
                }
                return Err(-(super::errno_to_i32(v3errno) as i64));
            }
        }
    }
}

/// Run `OpenFileWriteOp` synchronously, mirroring [`run_read`].
fn run_write(file: &Cap<OpenFile>, bytes: &[u8]) -> Result<usize, i64> {
    let mut script_ctx = crate::KernelScriptCtx::new();
    let mut total: usize = 0;
    let mut remaining = bytes;
    loop {
        let outcome = {
            let mut op = OpenFileWriteOp {
                file,
                bytes: remaining,
                caller_netns: None,
                writer_cred: None,
                writer_user_ns: None,
                cursor: 0,
            };
            op.step(&mut script_ctx)
        };
        match outcome {
            V3Out::Done(written) => return Ok(total + written),
            V3Out::Continue { progress } => {
                let written = progress.bytes();
                total += written;
                if written == 0 || written >= remaining.len() {
                    return Ok(total);
                }
                remaining = &remaining[written..];
            }
            V3Out::Yield { progress, .. } => {
                let written = progress.bytes();
                total += written;
                if total > 0 {
                    return Ok(total);
                }
                return Err(NEG_EIO);
            }
            V3Out::Err(v3errno) => {
                if total > 0 {
                    return Ok(total);
                }
                return Err(-(super::errno_to_i32(v3errno) as i64));
            }
        }
    }
}

fn run_iovec_read(
    file: &Cap<OpenFile>,
    aspace: &Cap<AddressSpace>,
    iov_ptr: u64,
    iovcnt: usize,
) -> Result<usize, i64> {
    let mut total = 0usize;
    for i in 0..iovcnt {
        let mut raw = [0u8; 16];
        let slot = iov_ptr.wrapping_add((i * 16) as u64);
        if bootstrap_copy_from_user(aspace, &mut raw, slot).is_err() {
            return if total > 0 {
                Ok(total)
            } else {
                Err(NEG_EFAULT)
            };
        }
        let base = read_u64(&raw, 0);
        let len = read_u64(&raw, 8) as usize;
        if len == 0 {
            continue;
        }
        let mut staging = alloc::vec![0u8; len];
        let bytes = match run_read(file, &mut staging) {
            Ok(bytes) => bytes,
            Err(neg) => return if total > 0 { Ok(total) } else { Err(neg) },
        };
        if bytes > 0 && bootstrap_copy_to_user(aspace, base, &staging[..bytes]).is_err() {
            return if total > 0 {
                Ok(total)
            } else {
                Err(NEG_EFAULT)
            };
        }
        total += bytes;
        if bytes < len {
            break;
        }
    }
    Ok(total)
}

fn run_iovec_write(
    file: &Cap<OpenFile>,
    aspace: &Cap<AddressSpace>,
    iov_ptr: u64,
    iovcnt: usize,
) -> Result<usize, i64> {
    let mut total = 0usize;
    for i in 0..iovcnt {
        let mut raw = [0u8; 16];
        let slot = iov_ptr.wrapping_add((i * 16) as u64);
        if bootstrap_copy_from_user(aspace, &mut raw, slot).is_err() {
            return if total > 0 {
                Ok(total)
            } else {
                Err(NEG_EFAULT)
            };
        }
        let base = read_u64(&raw, 0);
        let len = read_u64(&raw, 8) as usize;
        if len == 0 {
            continue;
        }
        let mut staging = alloc::vec![0u8; len];
        if bootstrap_copy_from_user(aspace, &mut staging, base).is_err() {
            return if total > 0 {
                Ok(total)
            } else {
                Err(NEG_EFAULT)
            };
        }
        match run_write(file, &staging) {
            Ok(bytes) => {
                total += bytes;
                if bytes < len {
                    break;
                }
            }
            Err(neg) => return if total > 0 { Ok(total) } else { Err(neg) },
        }
    }
    Ok(total)
}

/// Build the owner's `SubjectContext` from the syscall ctx. Mirrors
/// `super::build_subject_script_ctx` but returns the raw subject (the
/// PR-11 phase 2 worker spawn needs a by-value subject so the borrow
/// scope can outlive the syscall arm).
fn build_owner_subject(ctx: &SyscallCtx<'_>) -> crate::KernelSubjectContext {
    let cred_cap = ctx.cred_cap();
    let restrictions_cap = tx_subsystems::cred::placeholder_restrictions_cap()
        .expect("placeholder restrictions zone has capacity per syscall entry");
    let authority = crate::KernelSubjectAuthority::new(cred_cap, restrictions_cap);
    crate::KernelSubjectContext::from_thread(ctx.process.clone(), ctx.thread.clone(), authority)
}

fn align_up(value: usize, align: usize) -> Option<usize> {
    value.checked_add(align - 1).map(|v| v & !(align - 1))
}

fn raw_ring_event_capacity(requested: u32) -> Option<(usize, u32)> {
    let requested = requested.checked_add(2)?;
    let bytes =
        AIO_RING_HEADER_BYTES.checked_add((requested as usize).checked_mul(IO_EVENT_BYTES)?)?;
    let ring_bytes = align_up(bytes, USER_PAGE_SIZE)?;
    let events = (ring_bytes.checked_sub(AIO_RING_HEADER_BYTES)? / IO_EVENT_BYTES) as u32;
    Some((ring_bytes, events))
}

fn write_u32_field(aspace: &Cap<AddressSpace>, addr: u64, value: u32) -> Result<(), ()> {
    bootstrap_copy_to_user(aspace, addr, &value.to_le_bytes()).map_err(|_| ())
}

fn read_user_u32_field(aspace: &Cap<AddressSpace>, addr: u64) -> Result<u32, ()> {
    let mut bytes = [0u8; 4];
    bootstrap_copy_from_user(aspace, &mut bytes, addr).map_err(|_| ())?;
    Ok(u32::from_le_bytes(bytes))
}

fn write_aio_ring_header(
    aspace: &Cap<AddressSpace>,
    ring_base: u64,
    id: u32,
    nr_events: u32,
) -> Result<(), ()> {
    write_u32_field(aspace, ring_base, id)?;
    write_u32_field(aspace, ring_base + 4, nr_events)?;
    write_u32_field(aspace, ring_base + 8, 0)?;
    write_u32_field(aspace, ring_base + 12, 0)?;
    write_u32_field(aspace, ring_base + 16, AIO_RING_MAGIC)?;
    write_u32_field(aspace, ring_base + 20, AIO_RING_COMPAT_FEATURES)?;
    write_u32_field(aspace, ring_base + 24, AIO_RING_INCOMPAT_FEATURES)?;
    write_u32_field(aspace, ring_base + 28, AIO_RING_HEADER_BYTES as u32)?;
    Ok(())
}

fn install_raw_ring(
    aspace: &Cap<AddressSpace>,
    nr_events: u32,
    id: u32,
) -> Result<(u64, u32), SyscallResult> {
    let (ring_bytes, actual_events) =
        raw_ring_event_capacity(nr_events).ok_or(SyscallResult::Error(EINVAL_VALUE))?;
    let ring_base = NEXT_RAW_AIO_RING_BASE.fetch_add(RAW_AIO_RING_STRIDE, Ordering::AcqRel);
    let range = UserRange::new_aligned(UserVirtAddr(ring_base as usize), ring_bytes)
        .map_err(|_| SyscallResult::Error(EINVAL_VALUE))?;
    let ring_pages = (ring_bytes / USER_PAGE_SIZE) as u64;
    let ring_pc = PageContainer::new_cap(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        ring_pages,
    )
    .map_err(|_| SyscallResult::Error(ENOMEM_VALUE))?;
    let request = VmMapRequest::fixed(
        range,
        MapPlacement::FixedReplace,
        Prot::READ_WRITE,
        VmEntryFlags::SHARED,
        VmBacking::Page {
            pc: ring_pc,
            offset: 0,
        },
    );
    aspace
        .try_mmap(request)
        .map_err(|_| SyscallResult::Error(ENOMEM_VALUE))?;
    write_aio_ring_header(aspace, ring_base, id, actual_events)
        .map_err(|_| SyscallResult::Error(EFAULT_VALUE))?;
    Ok((ring_base, actual_events))
}

fn lookup_raw_aio(ctx_id: u64) -> Option<RawAioContext> {
    RAW_AIO_CONTEXTS.lock().get(&ctx_id).cloned()
}

fn raw_ring_event_addr(raw: &RawAioContext, index: u32) -> u64 {
    let events_per_page = (USER_PAGE_SIZE / IO_EVENT_BYTES) as u32;
    let first_page_events = ((USER_PAGE_SIZE - AIO_RING_HEADER_BYTES) / IO_EVENT_BYTES) as u32;
    let pos = index + (events_per_page - first_page_events);
    raw.ring_base + (pos as u64) * IO_EVENT_BYTES as u64
}

fn raw_push_completion(
    raw: &RawAioContext,
    aspace: &Cap<AddressSpace>,
    event: IoEvent,
) -> Result<(), ()> {
    let head = read_user_u32_field(aspace, raw.ring_base + 8)?;
    let tail = read_user_u32_field(aspace, raw.ring_base + 12)?;
    let next_tail = if tail + 1 >= raw.nr_events {
        0
    } else {
        tail + 1
    };
    if next_tail == head {
        return Err(());
    }
    let slot = raw_ring_event_addr(raw, tail);
    bootstrap_copy_to_user(aspace, slot, &event.to_le_bytes()).map_err(|_| ())?;
    write_u32_field(aspace, raw.ring_base + 12, next_tail)?;
    Ok(())
}

fn raw_drain_events(
    raw: &RawAioContext,
    aspace: &Cap<AddressSpace>,
    events_ptr: u64,
    max: usize,
) -> Result<usize, SyscallResult> {
    let mut head = read_user_u32_field(aspace, raw.ring_base + 8)
        .map_err(|_| SyscallResult::Error(EFAULT_VALUE))?;
    let tail = read_user_u32_field(aspace, raw.ring_base + 12)
        .map_err(|_| SyscallResult::Error(EFAULT_VALUE))?;
    let mut copied = 0usize;
    while copied < max && head != tail {
        let slot = raw_ring_event_addr(raw, head);
        let mut event = [0u8; IO_EVENT_BYTES];
        bootstrap_copy_from_user(aspace, &mut event, slot)
            .map_err(|_| SyscallResult::Error(EFAULT_VALUE))?;
        let out = events_ptr.wrapping_add((copied * IO_EVENT_BYTES) as u64);
        if bootstrap_copy_to_user(aspace, out, &event).is_err() {
            if copied == 0 {
                return Err(SyscallResult::Error(EFAULT_VALUE));
            }
            break;
        }
        copied += 1;
        head += 1;
        if head >= raw.nr_events {
            head = 0;
        }
    }
    if copied > 0 {
        write_u32_field(aspace, raw.ring_base + 8, head)
            .map_err(|_| SyscallResult::Error(EFAULT_VALUE))?;
        raw.aio_cap.notify_ring_space_available();
    }
    Ok(copied)
}

/// `io_setup(nr_events, ctx_idp)` syscall arm.
///
/// Per `man 2 io_setup`:
/// - `nr_events`: the maximum number of in-flight events the user
///   intends to submit against this context. Linux caches this for
///   submission-queue sizing; we stash it on the [`AioContext`] for
///   phase 3 to consume.
/// - `ctx_idp`: a user pointer to an `aio_context_t` out-parameter.
///   **We ignore this argument** per the Linux-divergence policy in
///   the module docs — the fd is returned as the syscall result
///   directly. Userspace bridges the fd value back into the
///   `aio_context_t *` slot.
///
/// Returns the new fd on success, `-ENOMEM` if the zone allocation
/// fails (or the fd-table is exhausted — the substrate does not yet
/// expose `RLIMIT_NOFILE`, so the `ENOMEM` magnitude covers both
/// shapes).
///
/// **Why no flags validation:** Linux's `io_setup` has no `flags`
/// argument; the syscall surface is `(unsigned nr_events,
/// aio_context_t *ctx_idp)`. The companion `O_CLOEXEC`-style bit is
/// not part of this syscall's contract in Linux either — `fcntl(F_SETFD)`
/// is the documented post-hoc path, mirrored here.
///
/// [`AioContext`]: tx_subsystems::aio::AioContext
pub(super) fn sys_io_setup(nr_events: u32, ctx_idp: u64, ctx: &SyscallCtx<'_>) -> SyscallResult {
    if nr_events == 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let mut old_ctx = [0u8; 8];
    if bootstrap_copy_from_user(&ctx.aspace, &mut old_ctx, ctx_idp).is_err() {
        return SyscallResult::Error(EFAULT_VALUE);
    }
    if u64::from_le_bytes(old_ctx) != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    let aio_cap = match AioContext::new_with_nr_events_cap(nr_events) {
        Ok(cap) => cap,
        Err(_) => return SyscallResult::Error(ENOMEM_VALUE),
    };
    let context_id = aio_cap.context_id();
    let (ring_base, actual_events) =
        match install_raw_ring(&ctx.aspace, nr_events, context_id as u32) {
            Ok(parts) => parts,
            Err(err) => return err,
        };

    let raw = RawAioContext {
        aio_cap: aio_cap.clone(),
        ring_base,
        nr_events: actual_events,
    };
    RAW_AIO_CONTEXTS.lock().insert(ring_base, raw.clone());

    if bootstrap_copy_to_user(&ctx.aspace, ctx_idp, &ring_base.to_le_bytes()).is_err() {
        RAW_AIO_CONTEXTS.lock().remove(&ring_base);
        return SyscallResult::Error(EFAULT_VALUE);
    }

    // Keep the worker future available for existing host canaries, but
    // raw ABI submissions complete synchronously into the Linux-shaped
    // user ring below. Once the kernel reactor spawn seam lands, this
    // future becomes the production async path again.
    let owner_subject = build_owner_subject(ctx);
    let dispatcher = build_iocb_dispatcher(ctx.process.clone(), ctx.aspace.clone());
    let publisher =
        build_completion_publisher(raw.clone(), ctx.aspace.clone(), ctx.process.clone());
    let worker = spawn_worker_for_context(
        aio_cap.clone(),
        ctx.process.clone(),
        owner_subject,
        dispatcher,
        publisher,
    );
    if tx_subsystems::reactor_submit::submit_aio_worker_fn().is_some() {
        tx_subsystems::reactor_submit::submit_aio_worker(worker);
    } else {
        install_worker_future_for_test(context_id, worker);
    }
    SyscallResult::Return(0)
}

/// Linux `struct iocb` size on 64-bit. Linux's UAPI lays the iocb out
/// with 64 bytes including reserved / version words; the phase-2 arm
/// reads only the fields it needs and ignores the rest, but the size
/// constant pins the read window so future ABI bumps land here.
const IOCB_BYTES: usize = 64;

/// Phase-2 cap on the number of iocbs `sys_io_submit` will process in
/// a single batch. Linux caps via `aio-max-nr`; we cap structurally
/// for now at the size of one page worth of `u64` pointers (512). The
/// per-context `nr_events` further constrains admission.
const IOCB_BATCH_MAX: u32 = 512;

/// `io_submit(ctx_id, nr, iocbpp)` syscall arm.
///
/// Per `man 2 io_submit`:
/// - `ctx_id`: the raw Linux `aio_context_t` value written by [`sys_io_setup`].
/// - `nr`: number of iocb pointers in `iocbpp`.
/// - `iocbpp`: user pointer to `nr` `*struct iocb` user pointers.
///
/// **Flow** (5 lines):
/// 1. Resolve `ctx_id` to the private [`AioContext`] state.
/// 2. For each pointer in `iocbpp`: copy in the user pointer (one
///    `u64`), then copy in the `struct iocb` it addresses.
/// 3. Parse the iocb fields (opcode, fd, buf, nbytes, offset, data).
/// 4. Validate (opcode known); push onto the AIO context's submit
///    queue; the push notifies the worker's `iocb_arrived` source.
/// 5. Return the number admitted; short-circuit the rest on the
///    first per-iocb validation / capacity failure.
///
/// **Linux compatibility note.** Linux returns at-least-1 OR
/// `-EAGAIN` if the first iocb fails. Our phase-2 implementation
/// matches that shape: an empty admit returns `-EAGAIN`; otherwise
/// the positive count is returned even if later iocbs were
/// rejected. See the report's last question for the policy
/// discussion.
pub(super) fn sys_io_submit(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let ctx_id = args[0];
    let nr = args[1] as u32;
    let iocbpp = args[2];

    if nr == 0 {
        return SyscallResult::Return(0);
    }
    if nr > IOCB_BATCH_MAX {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    let raw = match lookup_raw_aio(ctx_id) {
        Some(raw) => raw,
        None => return SyscallResult::Error(EINVAL_VALUE),
    };

    // Loop over the `iocbpp` array. Each slot is a `u64` user
    // pointer to a `struct iocb`.
    let mut admitted: i64 = 0;
    for i in 0..nr {
        // 1. Copy in the user pointer at `iocbpp + i*8`.
        let slot_ptr = iocbpp.wrapping_add((i as u64).wrapping_mul(8));
        let mut slot_bytes = [0u8; 8];
        if bootstrap_copy_from_user(&ctx.aspace, &mut slot_bytes, slot_ptr).is_err() {
            // Short-circuit on per-slot copy failure. Per Linux:
            // return what was admitted, else -EFAULT for first.
            if admitted == 0 {
                return SyscallResult::Error(super::EFAULT_VALUE);
            }
            return SyscallResult::Return(admitted);
        }
        let iocb_ptr = u64::from_le_bytes(slot_bytes);

        // 2. Copy in the `struct iocb` itself.
        let mut iocb_bytes = [0u8; IOCB_BYTES];
        if bootstrap_copy_from_user(&ctx.aspace, &mut iocb_bytes, iocb_ptr).is_err() {
            if admitted == 0 {
                return SyscallResult::Error(super::EFAULT_VALUE);
            }
            return SyscallResult::Return(admitted);
        }

        // 3. Parse the fields. The Linux UAPI layout is:
        //
        //   aio_data    : u64 @ 0
        //   aio_key/v   : u64 @ 8     (reserved/version — ignored)
        //   aio_lio_opcode: u16 @ 16
        //   aio_reqprio : u16 @ 18    (ignored)
        //   aio_fildes  : u32 @ 20
        //   aio_buf     : u64 @ 24
        //   aio_nbytes  : u64 @ 32
        //   aio_offset  : i64 @ 40
        //   ... rest reserved/flags ...
        let aio_data = read_u64(&iocb_bytes, 0);
        let aio_rw_flags = read_u32(&iocb_bytes, 12);
        let aio_lio_opcode = read_u16(&iocb_bytes, 16);
        let aio_fildes = read_u32(&iocb_bytes, 20);
        let aio_buf = read_u64(&iocb_bytes, 24);
        let aio_nbytes = read_u64(&iocb_bytes, 32);
        let aio_offset = read_u64(&iocb_bytes, 40) as i64;
        let aio_reserved2 = read_u64(&iocb_bytes, 48);
        let aio_flags = read_u32(&iocb_bytes, 56);
        let aio_resfd = read_u32(&iocb_bytes, 60);

        // 4. Validate the opcode. Other validation (fd / buf in
        //    user-VA range) defers to the worker's borrow body in
        //    phase 3 — at dispatch time the borrow's `SubjectContext`
        //    is the right principal to check against.
        if aio_reserved2 != 0 || !is_valid_iocb_opcode(aio_lio_opcode) {
            if admitted == 0 {
                return SyscallResult::Error(EINVAL_VALUE);
            }
            return SyscallResult::Return(admitted);
        }
        let recognized_flags = IOCB_FLAG_RESFD | IOCB_FLAG_IOPRIO;
        if (aio_flags & !recognized_flags) != 0 {
            if admitted == 0 {
                return SyscallResult::Error(EINVAL_VALUE);
            }
            return SyscallResult::Return(admitted);
        }
        if aio_rw_flags != 0 {
            if admitted == 0 {
                return SyscallResult::Error(ENOSYS_VALUE);
            }
            return SyscallResult::Return(admitted);
        }
        if (aio_flags & IOCB_FLAG_RESFD) != 0 {
            match ctx
                .process
                .fd(aio_resfd)
                .and_then(|file| file.eventfd().cloned())
            {
                Some(_) => {}
                None => {
                    if admitted == 0 {
                        return SyscallResult::Error(EINVAL_VALUE);
                    }
                    return SyscallResult::Return(admitted);
                }
            }
        }
        let key_zero = 0u32.to_le_bytes();
        if bootstrap_copy_to_user(&ctx.aspace, iocb_ptr + 8, &key_zero).is_err() {
            if admitted == 0 {
                return SyscallResult::Error(EFAULT_VALUE);
            }
            return SyscallResult::Return(admitted);
        }

        // 5. Push onto the AIO context's submit queue. Rejection
        //    returns the iocb back (queue at `nr_events` capacity);
        //    we surface that as a count-short-circuit too.
        let iocb = Iocb::new(
            aio_lio_opcode,
            aio_fildes,
            aio_buf,
            aio_nbytes,
            aio_offset,
            aio_data,
        )
        .with_linux_tail(iocb_ptr, aio_rw_flags, aio_flags, aio_resfd);
        if raw.aio_cap.push_iocb(iocb).is_err() {
            if admitted == 0 {
                return SyscallResult::Error(EAGAIN_VALUE);
            }
            return SyscallResult::Return(admitted);
        }
        admitted += 1;
    }

    SyscallResult::Return(admitted)
}

// === little-endian readers (no_std-friendly) ============================

fn read_u16(bytes: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([bytes[off], bytes[off + 1]])
}

fn read_u32(bytes: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]])
}

fn read_u64(bytes: &[u8], off: usize) -> u64 {
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&bytes[off..off + 8]);
    u64::from_le_bytes(buf)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AioWakeReason {
    WaitSource,
    ProcessTimer,
}

async fn await_aio_events_or_process_timer(
    ctx: &SyscallCtx<'_>,
    source: WaitSourceId,
    interests: InterestMask,
) -> AioWakeReason {
    let Some(process_timer_deadline) = ctx.process.next_process_timer_deadline_ns() else {
        super::await_wait_source(ctx, source, interests).await;
        return AioWakeReason::WaitSource;
    };
    let Some(timer_future) = tx_subsystems::timer_sleep::sleep_until_ns(process_timer_deadline)
    else {
        super::await_wait_source(ctx, source, interests).await;
        return AioWakeReason::WaitSource;
    };

    let source_future = super::await_wait_source(ctx, source, interests);
    let mut source_future = core::pin::pin!(source_future);
    let mut timer_future = core::pin::pin!(timer_future);

    use core::future::{poll_fn, Future};
    use core::task::Poll;

    poll_fn(|cx| {
        if source_future.as_mut().poll(cx).is_ready() {
            Poll::Ready(AioWakeReason::WaitSource)
        } else if timer_future.as_mut().poll(cx).is_ready() {
            super::time::poll_expired_process_timers_at(ctx, process_timer_deadline);
            Poll::Ready(AioWakeReason::ProcessTimer)
        } else {
            Poll::Pending
        }
    })
    .await
}

/// `io_getevents(ctx_id, min_nr, nr, events, timeout)` syscall arm.
///
/// Per `man 2 io_getevents`:
/// - `ctx_id`: the raw Linux `aio_context_t` value written by [`sys_io_setup`].
/// - `min_nr`: minimum number of events to return before unblocking
///   (0 → return whatever is already available).
/// - `nr`: maximum number of events to drain in this call.
/// - `events`: user pointer to `nr` `struct io_event` slots.
/// - `timeout`: user pointer to a `struct timespec` (`{ tv_sec, tv_nsec }`,
///   16 bytes). NULL → block indefinitely; non-NULL keeps the current
///   v1 nonblocking behavior.
///
/// **Flow** (5 lines):
/// 1. Resolve `ctx_id` → raw AIO state; reject unknown contexts with `EINVAL`.
/// 2. Drain up to `nr` events from the user-visible ring and advance head.
/// 3. If `drained.len() < min_nr` and `timeout != NULL_TIMEOUT_NEVER`,
///    park on the `events_available` carrier and re-drain (loop).
/// 4. Serialise each drained event into a 32-byte `struct io_event`
///    and copy through the principal's address space at
///    `events + i * 32`.
/// 5. Return the count of events written.
///
/// **Timeout semantics.** Phase 5 honors two shapes: `timeout == NULL`
/// (block until `min_nr` events are available — the canonical Linux
/// shape) and `timeout != NULL` (return immediately after the first
/// drain regardless of `min_nr`). A real `struct timespec` parse +
/// `OnTimer` yield is phase 6 territory. Tests use `timeout = NULL`
/// for blocking and `timeout = 1` (any non-zero pointer) for the
/// non-blocking variant.
pub(super) async fn sys_io_getevents<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let ctx_id = args[0];
    let min_nr = args[1];
    let nr = args[2];
    let events_ptr = args[3];
    let timeout_ptr = args[4];

    let raw = match lookup_raw_aio(ctx_id) {
        Some(raw) => raw,
        None => return SyscallResult::Error(EINVAL_VALUE),
    };

    if nr == 0 {
        return SyscallResult::Return(0);
    }
    if min_nr > nr {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    let blocking = timeout_ptr == 0;
    let nr_usize = core::cmp::min(nr as usize, IOCB_BATCH_MAX as usize);
    let min_nr_usize = core::cmp::min(min_nr as usize, nr_usize);

    // Bounded drain-then-wait loop. Each iteration drains the
    // completion queue; if fewer than `min_nr` events have been
    // collected and we're allowed to block, park on the
    // `events_available` carrier and re-drain.
    let mut drained: alloc::vec::Vec<IoEvent> = alloc::vec::Vec::with_capacity(nr_usize);
    let mut iter_budget: u32 = 1024;
    loop {
        let remaining = nr_usize - drained.len();
        if remaining > 0 {
            let before = drained.len();
            match raw_drain_events(
                &raw,
                &ctx.aspace,
                events_ptr.wrapping_add((before * IO_EVENT_BYTES) as u64),
                remaining,
            ) {
                Ok(copied) => {
                    for _ in 0..copied {
                        drained.push(IoEvent::new(0, 0, 0, 0));
                    }
                }
                Err(err) => return err,
            }
        }
        if drained.len() >= min_nr_usize || drained.len() >= nr_usize {
            break;
        }
        if !blocking {
            break;
        }
        // Park on the events_available carrier and re-drain. Process
        // timers are signal delivery deadlines; they interrupt the
        // blocking wait through the same wake/recheck shape as the
        // other wait-source syscalls.
        if await_aio_events_or_process_timer(
            ctx,
            WaitSourceId::new(raw.aio_cap.events_available_id()),
            InterestMask::new(EVENTS_AVAILABLE_MASK),
        )
        .await
            == AioWakeReason::ProcessTimer
        {
            return SyscallResult::Error(EINTR_VALUE);
        }
        iter_budget = iter_budget.saturating_sub(1);
        if iter_budget == 0 {
            // Defensive break: never block forever in the canary even
            // if the wait carrier never wakes. Surface what we have.
            break;
        }
    }

    SyscallResult::Return(drained.len() as i64)
}

/// Linux `struct __aio_sigset` used by `io_pgetevents(2)`.
///
/// Linux stores a user sigset pointer plus its byte size here, then
/// applies it as a temporary signal mask around `do_io_getevents`.
/// Tx v1 validates the wrapper and pointed-to mask when present, but
/// keeps the same deferred temporary-mask policy as `ppoll` and
/// `epoll_pwait`.
const AIO_SIGSET_BYTES: usize = 16;

/// `io_pgetevents(ctx_id, min_nr, nr, events, timeout, sig)`.
///
/// This is the signal-mask variant of `io_getevents`. The AIO data
/// path is identical; the extra `sig` argument points at Linux's
/// `struct __aio_sigset { const sigset_t *sigmask; size_t sigsetsize; }`.
/// For v1, a non-NULL `sigmask` must use the existing 8-byte sigset
/// size and must be readable, but the temporary signal-mask swap is
/// deliberately deferred alongside `ppoll`/`epoll_pwait`.
pub(super) async fn sys_io_pgetevents<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let sigset_ptr = args[5];
    let mut restore_mask = None;
    if sigset_ptr != 0 {
        let mut sigset = [0u8; AIO_SIGSET_BYTES];
        if bootstrap_copy_from_user(&ctx.aspace, &mut sigset, sigset_ptr).is_err() {
            return SyscallResult::Error(EFAULT_VALUE);
        }
        let sigmask_ptr = read_u64(&sigset, 0);
        let sigset_size = read_u64(&sigset, 8);
        if sigmask_ptr != 0 {
            if sigset_size != 8 {
                return SyscallResult::Error(EINVAL_VALUE);
            }
            let mut mask = [0u8; 8];
            if bootstrap_copy_from_user(&ctx.aspace, &mut mask, sigmask_ptr).is_err() {
                return SyscallResult::Error(EFAULT_VALUE);
            }
            let next = SignalMask::new(u64::from_le_bytes(mask));
            match step_sigprocmask(&ctx.thread, SigmaskHow::SetMask, next) {
                SigprocmaskChange::Replaced { prev, .. } => restore_mask = Some(prev),
                SigprocmaskChange::ZombieIgnored => {}
            }
        }
    }

    let result = sys_io_getevents(args, ctx).await;
    if let Some(prev) = restore_mask {
        let _ = step_sigprocmask(&ctx.thread, SigmaskHow::SetMask, prev);
    }
    result
}

/// `io_destroy(ctx_id)` syscall arm.
///
/// Per `man 2 io_destroy`:
/// - `ctx_id`: the raw Linux `aio_context_t` value written by [`sys_io_setup`].
///
/// Semantics:
/// 1. Remove the raw context mapping; later AIO syscalls see `EINVAL`.
/// 2. Trip the worker abort signal and wake AIO wait sources so parked
///    workers/getevents calls re-check state.
/// 3. Drop any host-test worker future if the boot reactor hook was absent.
///
/// Returns `0` on success. Linux's `io_destroy` is documented as
/// synchronous w.r.t. in-flight ops; Tx v1 aborts the worker cooperatively.
pub(super) fn sys_io_destroy(ctx_id: u64, _ctx: &SyscallCtx<'_>) -> SyscallResult {
    let raw = match RAW_AIO_CONTEXTS.lock().remove(&ctx_id) {
        Some(raw) => raw,
        None => return SyscallResult::Error(EINVAL_VALUE),
    };

    // Trip the worker abort signal so any future poll of the worker future
    // terminates with `CooperativeCancel(OwnerRequested)`.
    raw.aio_cap.cancel_worker();
    raw.aio_cap.notify_ring_space_available();
    raw.aio_cap.push_completion(IoEvent::new(0, 0, 0, 0));

    // Drop the stashed worker future (idempotent — None if a test
    // already took it). This causes the future to drop synchronously
    // even if it was never pumped to completion.
    let _ = WORKER_REGISTRY.lock().remove(&raw.aio_cap.context_id());

    SyscallResult::Return(0)
}

pub(super) fn sys_io_cancel(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let ctx_id = args[0];
    let iocb_ptr = args[1];
    let _result_ptr = args[2];
    let raw = match lookup_raw_aio(ctx_id) {
        Some(raw) => raw,
        None => return SyscallResult::Error(EINVAL_VALUE),
    };
    let mut key = [0u8; 4];
    if bootstrap_copy_from_user(&ctx.aspace, &mut key, iocb_ptr + 8).is_err() {
        return SyscallResult::Error(EFAULT_VALUE);
    }
    if u32::from_le_bytes(key) != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if let Some(iocb) = raw.aio_cap.cancel_queued(iocb_ptr) {
        let event = IoEvent::new(iocb.aio_data, iocb.aio_user_ptr, NEG_ECANCELED, 0);
        if raw_push_completion(&raw, &ctx.aspace, event).is_err() {
            return SyscallResult::Error(EAGAIN_VALUE);
        }
        raw.aio_cap.push_completion(event);
        if args[2] != 0
            && bootstrap_copy_to_user(&ctx.aspace, args[2], &event.to_le_bytes()).is_err()
        {
            return SyscallResult::Error(EFAULT_VALUE);
        }
        return SyscallResult::Return(0);
    }
    if raw.aio_cap.mark_cancel_requested(iocb_ptr) {
        return SyscallResult::Error(EAGAIN_VALUE);
    }
    SyscallResult::Error(EAGAIN_VALUE)
}

// === host-test worker-future registry ====================================
//
// Production `io_setup` submits the worker through the tx-kernel reactor hook.
// Host syscall tests do not boot that hook, so they stash the same worker
// future here keyed by `context_id` and pump it manually.
//
// **Why a global registry rather than stashing on the `AioContext`
// itself.** The future's concrete type depends on the
// `SubjectIdentity` parameter `I`; storing it on the cap would force
// the cap to be parameterised too, which would ripple through every
// fd-table call site. The registry is keyed by `context_id`, which
// is the same addressing key phase 3+ uses for completion routing.
//
// **Why not a SpinMutex<Vec<...>>**: the per-id key shape makes
// targeted lookup cheap; the BTreeMap stays small (one entry per
// live AIO context).

static WORKER_REGISTRY: SpinMutex<BTreeMap<u64, AioWorkerFuture>> = SpinMutex::new(BTreeMap::new());

/// Monotonic counter of worker-future installs; tests use it to
/// confirm a worker was spawned per `io_setup` call. Production code
/// has no use for this counter.
static WORKER_INSTALL_COUNT: AtomicU64 = AtomicU64::new(0);

fn install_worker_future_for_test(context_id: u64, fut: AioWorkerFuture) {
    WORKER_REGISTRY.lock().insert(context_id, fut);
    WORKER_INSTALL_COUNT.fetch_add(1, Ordering::AcqRel);
}

/// Test-only: remove and return the worker future for `context_id`,
/// if any. Tests pump the returned future manually to confirm the
/// borrow-scope body observes pushed iocbs and the abort signal.
pub fn take_worker_future_for_test(context_id: u64) -> Option<AioWorkerFuture> {
    WORKER_REGISTRY.lock().remove(&context_id)
}

/// Test-only: clear every stashed worker future. Used by per-test
/// setup so a previous test's residual futures don't surface as
/// false positives in the install counter.
pub fn reset_worker_registry_for_test() {
    WORKER_REGISTRY.lock().clear();
    RAW_AIO_CONTEXTS.lock().clear();
    WORKER_INSTALL_COUNT.store(0, Ordering::Release);
    NEXT_RAW_AIO_RING_BASE.store(RAW_AIO_RING_BASE_START, Ordering::Release);
}

/// Test-only: monotonic install count.
pub fn worker_install_count_for_test() -> u64 {
    WORKER_INSTALL_COUNT.load(Ordering::Acquire)
}
