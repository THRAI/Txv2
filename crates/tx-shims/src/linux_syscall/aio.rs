//! `sys_io_setup(2)` + `sys_io_submit(2)` + `sys_io_getevents(2)` +
//! `sys_io_destroy(2)` — AIO syscall arms (PR-11 phases 1–5).
//!
//! Spec:
//! - `docs/progress/decisions/2026-05-11-d8-pr-11-aio-plan.md` §7
//!   (phase plan rows P-11.3 + P-11.4) and §4.1 (fd-shape decision)
//! - `docs/Txv3/06_EXECUTION_SCOPE_v1.md` (`OnBehalfOf<P>` execution
//!   scope — phase 2 wires the worker via `with_on_behalf_of`)
//! - `man 2 io_setup`, `man 2 io_submit`
//!
//! # What lands here
//!
//! 1. [`sys_io_setup`] — the syscall dispatcher for `__NR_io_setup =
//!    206`. Allocates a fresh [`AioContext`] cap (W-Z phase 1 zone),
//!    wraps it in an `OpenFile` whose backing is
//!    `OpenFileBacking::AioContext`, installs at the lowest free fd
//!    via [`tx_subsystems::process::ProcessIdentity::install_fd`], and
//!    returns the fd.
//! 2. **Phase 2.** `sys_io_setup` also spawns a worker for the new
//!    context: it constructs an [`AioWorkerFuture`] via
//!    [`tx_subsystems::aio::spawn_worker_for_context`] and stashes it
//!    in a per-context registry [`take_worker_future_for_test`]
//!    so a test (or a future reactor-seam installer) can drive it.
//!    Per D8 §7 the production wiring submits the future to the boot
//!    reactor; phase 2 leaves that wiring as a deferred-pump model
//!    (see the `TODO PR-11 phase 2b: spawn deferred` comment below).
//! 3. [`sys_io_submit`] — the phase 2 dispatcher for `__NR_io_submit
//!    = 209`. Looks up the context fd, copies each iocb pointer in,
//!    parses + validates each iocb (opcode check + non-zero
//!    bookkeeping), pushes onto the context's submission queue, and
//!    returns the number admitted (matches Linux's "at-most-1 OR
//!    -EAGAIN if first iocb fails" semantic — see report).
//!
//! # Linux divergence
//!
//! Linux's `io_setup(nr_events, ctx_idp)` writes back an opaque
//! pointer-shape value into the user-supplied
//! `aio_context_t *ctx_idp`. **We diverge intentionally** per D8 §4.1:
//! the kernel returns a real fd as the syscall result (no out-pointer
//! is written). This joins the Rnode/Ufd pattern W-Q established for
//! `userfaultfd(2)` in PR-10 phase 0 — userspace observes a fd
//! everywhere a non-VFS kernel object would otherwise need its own
//! handle namespace.
//!
//! For `sys_io_submit`, the `ctx_id` argument is a fd (not an opaque
//! `aio_context_t` value). The kernel resolves it through the same
//! fd-table lookup `read(2)`/`write(2)` use, then unwraps the inner
//! `Cap<AioContext>` via [`tx_subsystems::vfs::OpenFile::aio_context`].
//!
//! # Constraints (phase 2)
//!
//! - **Iocb dispatch is a stub.** The worker body increments a
//!   counter ([`tx_subsystems::aio::AioContext::dispatched`]); phase
//!   3 wires the real `step_pread` / `step_pwrite` under the borrow.
//! - **Completion queue is not yet allocated.** Phase 3 lands the
//!   user-mmapped ring; phase 2 leaves the worker as a structural
//!   pin: "the iocb was observed by the body within bounded reactor
//!   ticks".
//! - **Worker spawn is deferred-pump.** The boot reactor is owned by
//!   `tx-kernel`; reaching it from a syscall requires a function-
//!   pointer seam (like `reactor_submit::install_submit_child_thread`).
//!   Phase 2 stashes the future in a test-visible registry rather
//!   than wiring the seam; the seam install is the phase-2b follow-up.
//!
//! [`AioContext`]: tx_subsystems::aio::AioContext
//! [`AioWorkerFuture`]: tx_subsystems::aio::AioWorkerFuture

extern crate alloc;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

use tx_subsystems::aio::{
    is_valid_iocb_opcode, spawn_worker_for_context, AioContext, AioWorkerFuture, IoEvent, Iocb,
    IocbDispatcher, EVENTS_AVAILABLE_MASK, IOCB_CMD_PREAD, IOCB_CMD_PWRITE, IO_EVENT_BYTES,
};
use tx_subsystems::execution::WaitToken;
use tx_subsystems::process::ProcessIdentity;
use tx_subsystems::vfs::execution::{OpenFileLseekOp, OpenFileReadOp, OpenFileWriteOp};
use tx_subsystems::vfs::structure::OpenFileFlags;
use tx_subsystems::vfs::OpenFile;
use tx_subsystems::vm::AddressSpace;
use tx_subsystems::wait_source;

use super::{bootstrap_copy_from_user, bootstrap_copy_to_user, SyscallCtx, SyscallResult};
use super::{EBADF_VALUE, EFAULT_VALUE, EINVAL_VALUE, ENOMEM_VALUE};
use crate::adapter::step_engine::{self as step_engine, ByteProgress, Cap, NoProgress, ScriptCtx, SpinMutex, StepOp, StepOutcome, SubjectIdentity};
use crate::adapter::step_engine::StepOutcome as V3Out;

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

/// Per-iocb dispatch body. Called synchronously by the worker for each
/// iocb. Returns the [`IoEvent`] to post into the completion queue.
fn dispatch_one_iocb(
    process: &Cap<ProcessIdentity>,
    aspace: &Cap<AddressSpace>,
    iocb: &Iocb,
) -> IoEvent {
    // The `obj` placeholder echoes the user's cookie until we have a
    // real per-iocb kernel pointer (phase 6 territory).
    let cookie = iocb.aio_data;
    match iocb.aio_lio_opcode {
        IOCB_CMD_PREAD => dispatch_pread(process, aspace, iocb, cookie),
        IOCB_CMD_PWRITE => dispatch_pwrite(process, aspace, iocb, cookie),
        // FSYNC / FDSYNC / NOOP / PREADV / PWRITEV — out of canary
        // scope. Surface -EINVAL so userspace sees a clean rejection.
        _ => IoEvent::new(cookie, cookie, NEG_EINVAL, 0),
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
        None => return IoEvent::new(cookie, cookie, NEG_EBADF, 0),
    };
    let len = iocb.aio_nbytes as usize;
    if len == 0 {
        return IoEvent::new(cookie, cookie, 0, 0);
    }
    // Seek to the requested offset. SEEK_SET = 0. step_lseek returns
    // ESPIPE for non-seekable backings; surface as -EINVAL since
    // PREAD against a non-seekable backing is not meaningful.
    if !run_lseek_set(&file, iocb.aio_offset) {
        return IoEvent::new(cookie, cookie, NEG_EINVAL, 0);
    }
    let mut staging: alloc::vec::Vec<u8> = alloc::vec![0u8; len];
    let read_result = run_read(&file, &mut staging);
    match read_result {
        Ok(bytes) => {
            if bytes > 0 {
                if let Err(errno) = bootstrap_copy_to_user(aspace, iocb.aio_buf, &staging[..bytes])
                {
                    let _ = errno;
                    return IoEvent::new(cookie, cookie, NEG_EFAULT, 0);
                }
            }
            IoEvent::new(cookie, cookie, bytes as i64, 0)
        }
        Err(neg) => IoEvent::new(cookie, cookie, neg, 0),
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
        None => return IoEvent::new(cookie, cookie, NEG_EBADF, 0),
    };
    let len = iocb.aio_nbytes as usize;
    if len == 0 {
        return IoEvent::new(cookie, cookie, 0, 0);
    }
    let mut staging: alloc::vec::Vec<u8> = alloc::vec![0u8; len];
    if let Err(_errno) = bootstrap_copy_from_user(aspace, &mut staging, iocb.aio_buf) {
        return IoEvent::new(cookie, cookie, NEG_EFAULT, 0);
    }
    if !run_lseek_set(&file, iocb.aio_offset) {
        return IoEvent::new(cookie, cookie, NEG_EINVAL, 0);
    }
    match run_write(&file, &staging) {
        Ok(bytes) => IoEvent::new(cookie, cookie, bytes as i64, 0),
        Err(neg) => IoEvent::new(cookie, cookie, neg, 0),
    }
}

/// Run `OpenFileLseekOp` with `whence = SEEK_SET (0)` synchronously.
/// Returns `true` on success, `false` on any non-`Done`/`Continue`
/// outcome. The seek is synchronous in this kernel; the loop is
/// defensive against unexpected Yield/Err shapes.
fn run_lseek_set(file: &Cap<OpenFile>, offset: i64) -> bool {
    let mut script_ctx = crate::KernelScriptCtx::new();
    let guard = step_engine::guard();
    let mut op = OpenFileLseekOp {
        file,
        offset,
        whence: 0, // SEEK_SET
        guard: &guard,
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
            let guard = step_engine::guard();
            let mut op = OpenFileReadOp {
                file,
                out: &mut out[total..],
                guard: &guard,
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
                return Err(-(super::errno_to_i32(v3errno.into()) as i64));
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
            let guard = step_engine::guard();
            let mut op = OpenFileWriteOp {
                file,
                bytes: remaining,
                guard: &guard,
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
                return Err(-(super::errno_to_i32(v3errno.into()) as i64));
            }
        }
    }
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
pub(super) fn sys_io_setup(nr_events: u32, _ctx_idp: u64, ctx: &SyscallCtx<'_>) -> SyscallResult {
    // Mint a fresh `Cap<AioContext>` via the W-Z phase 1 zone. The
    // `nr_events` argument is stashed on the payload so phase 3's
    // submission-queue sizing can read it without rederiving from the
    // syscall args.
    let aio_cap = match AioContext::new_with_nr_events_cap(nr_events) {
        Ok(cap) => cap,
        Err(_) => return SyscallResult::Error(ENOMEM_VALUE),
    };

    // PR-11 phase 2 — spawn the per-context worker.
    //
    // The worker future enters `with_on_behalf_of(owner, body)` once
    // at startup and the borrow holds for the entire context
    // lifetime (per D8 §7 / W-Z's flag). The body loops draining
    // iocbs from the submit queue under the borrow's
    // `SubjectContext`.
    //
    // TODO PR-11 phase 2b: spawn deferred. Today's syscall context
    // does not carry a `&mut Reactor` handle (the boot reactor is
    // owned by `tx-kernel`); the production spawn must thread through
    // a function-pointer seam mirroring
    // `tx_subsystems::reactor_submit::install_submit_child_thread`.
    // For phase 2 we construct the worker future, stash it in a
    // test-visible registry keyed by `context_id`, and let tests pump
    // it manually. The phase-2b follow-up wires the seam.
    let context_id = aio_cap.context_id();
    // Build the owner's `SubjectContext` directly from the syscall
    // ctx. This is the same shape `build_subject_script_ctx` builds
    // (PR-9 phase 5 / D5 Path A) — see the comment on that helper for
    // why `placeholder_restrictions_cap` is the right stand-in until
    // PR-K lands the real restriction stack.
    let owner_subject = build_owner_subject(ctx);
    // PR-11 phase 3 — construct the real iocb dispatcher. The
    // closure captures the principal's `Cap<ProcessIdentity>` and
    // `Cap<AddressSpace>` clones so the worker can resolve
    // `aio_fildes` against P's fd table and copy bytes through P's
    // address space — all under the borrow's identity.
    let dispatcher = build_iocb_dispatcher(ctx.process.clone(), ctx.aspace.clone());
    let worker = spawn_worker_for_context(
        aio_cap.clone(),
        ctx.process.clone(),
        owner_subject,
        dispatcher,
    );
    install_worker_future_for_test(context_id, worker);

    // Wrap in an `OpenFile`. Phase 1 leaves every `OpenFileFlags` bit
    // at its default (`read = false, write = false, append = false,
    // cloexec = false, nonblocking = false`) — the AIO fd is not a
    // VFS-readable / writable object (the fd carries the AIO context
    // state, not a file's byte stream), so the read/write bits stay
    // off. Userspace that wants `O_CLOEXEC` on the AIO fd should call
    // `fcntl(fd, F_SETFD, FD_CLOEXEC)` after `io_setup` returns; that
    // path is already supported uniformly across `OpenFileBacking`
    // variants.
    let open_flags = OpenFileFlags::default();
    let open_cap = match OpenFile::new_aio_context_cap(aio_cap, open_flags) {
        Ok(cap) => cap,
        Err(_) => return SyscallResult::Error(ENOMEM_VALUE),
    };

    // Install at the lowest free fd. The fd-table machinery is
    // uniform across `OpenFileBacking` shapes; no AIO-specific
    // bookkeeping happens at install time in phase 1.
    let fd = ctx.process.allocate_fd();
    let _ = ctx.process.install_fd(fd, open_cap);

    SyscallResult::Return(fd as i64)
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

/// `io_submit(ctx_fd, nr, iocbpp)` syscall arm.
///
/// Per `man 2 io_submit`:
/// - `ctx_fd`: the AIO-context fd minted by [`sys_io_setup`] (our
///   fd-shape divergence from Linux's `aio_context_t` pointer).
/// - `nr`: number of iocb pointers in `iocbpp`.
/// - `iocbpp`: user pointer to `nr` `*struct iocb` user pointers.
///
/// **Flow** (5 lines):
/// 1. Resolve `ctx_fd` to a `Cap<AioContext>` via the fd table.
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
    let ctx_fd = args[0] as u32;
    let nr = args[1] as u32;
    let iocbpp = args[2];

    if nr == 0 {
        return SyscallResult::Return(0);
    }
    if nr > IOCB_BATCH_MAX {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    // Resolve the AIO-context fd. `OpenFile::aio_context` returns
    // `None` for non-AIO backings; we surface `-EINVAL` for that
    // (Linux uses EINVAL for an invalid context handle) and `-EBADF`
    // for a missing fd.
    let open_file = match ctx.process.fd(ctx_fd) {
        Some(f) => f,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    let aio_cap = match open_file.aio_context() {
        Some(c) => c.clone(),
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
        let aio_lio_opcode = read_u16(&iocb_bytes, 16);
        let aio_fildes = read_u32(&iocb_bytes, 20);
        let aio_buf = read_u64(&iocb_bytes, 24);
        let aio_nbytes = read_u64(&iocb_bytes, 32);
        let aio_offset = read_u64(&iocb_bytes, 40) as i64;

        // 4. Validate the opcode. Other validation (fd / buf in
        //    user-VA range) defers to the worker's borrow body in
        //    phase 3 — at dispatch time the borrow's `SubjectContext`
        //    is the right principal to check against.
        if !is_valid_iocb_opcode(aio_lio_opcode) {
            if admitted == 0 {
                return SyscallResult::Error(EINVAL_VALUE);
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
        );
        if aio_cap.push_iocb(iocb).is_err() {
            if admitted == 0 {
                // Linux returns -EAGAIN if the queue is full before
                // the first iocb is admitted.
                return SyscallResult::Error(super::EAGAIN_VALUE);
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

/// `io_getevents(ctx_fd, min_nr, nr, events, timeout)` syscall arm.
///
/// Per `man 2 io_getevents`:
/// - `ctx_fd`: the AIO-context fd minted by [`sys_io_setup`] (our
///   fd-shape divergence from Linux's `aio_context_t` pointer).
/// - `min_nr`: minimum number of events to return before unblocking
///   (0 → return whatever is already available).
/// - `nr`: maximum number of events to drain in this call.
/// - `events`: user pointer to `nr` `struct io_event` slots.
/// - `timeout`: user pointer to a `struct timespec` (`{ tv_sec, tv_nsec }`,
///   16 bytes). NULL → block indefinitely. Phase 5 honors NULL and any
///   non-NULL value by treating any non-NULL timeout as "non-blocking"
///   (the canary doesn't yet drive a real timer here — see body).
///
/// **Flow** (5 lines):
/// 1. Resolve `ctx_fd` → `Cap<AioContext>`; reject non-AIO fds with
///    `-EINVAL`.
/// 2. Drain up to `nr` events from the context's completion queue
///    (via [`AioContext::drain_completions`]).
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
    let ctx_fd = args[0] as u32;
    let min_nr = args[1];
    let nr = args[2];
    let events_ptr = args[3];
    let timeout_ptr = args[4];

    // Resolve the AIO-context fd.
    let open_file = match ctx.process.fd(ctx_fd) {
        Some(f) => f,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    let aio_cap = match open_file.aio_context() {
        Some(c) => c.clone(),
        None => return SyscallResult::Error(EINVAL_VALUE),
    };

    if nr == 0 {
        return SyscallResult::Return(0);
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
            let mut batch = aio_cap.drain_completions(remaining);
            drained.append(&mut batch);
        }
        if drained.len() >= min_nr_usize || drained.len() >= nr_usize {
            break;
        }
        if !blocking {
            break;
        }
        // Park on the events_available carrier and re-drain.
        let token = WaitToken::new(aio_cap.events_available_id(), EVENTS_AVAILABLE_MASK);
        if let Some(future) = wait_source::wait_on_token(token) {
            let _ = future.await;
        }
        iter_budget = iter_budget.saturating_sub(1);
        if iter_budget == 0 {
            // Defensive break: never block forever in the canary even
            // if the wait carrier never wakes. Surface what we have.
            break;
        }
    }

    // Serialise drained events into the user-events array.
    if drained.is_empty() {
        return SyscallResult::Return(0);
    }
    for (i, event) in drained.iter().enumerate() {
        let bytes = event.to_le_bytes();
        let slot_addr = events_ptr.wrapping_add((i * IO_EVENT_BYTES) as u64);
        if bootstrap_copy_to_user(&ctx.aspace, slot_addr, &bytes).is_err() {
            // Partial: return how many we already wrote. If none
            // written, surface -EFAULT.
            if i == 0 {
                return SyscallResult::Error(EFAULT_VALUE);
            }
            return SyscallResult::Return(i as i64);
        }
    }
    SyscallResult::Return(drained.len() as i64)
}

/// `io_destroy(ctx_fd)` syscall arm.
///
/// Per `man 2 io_destroy`:
/// - `ctx_fd`: the AIO-context fd minted by [`sys_io_setup`].
///
/// Semantics (phase 5):
/// 1. Resolve `ctx_fd` → `Cap<AioContext>`. `-EINVAL` for non-AIO fds;
///    `-EBADF` for unknown fds.
/// 2. Trip the worker's abort signal via [`AioContext::cancel_worker`]
///    — the worker's `with_on_behalf_of` racer observes the trip and
///    returns `Err(CooperativeCancel(OwnerRequested))` on its next
///    poll. In-flight iocbs (still in the submit queue) surface as
///    cancelled — the worker terminates before draining them.
/// 3. Drop the worker future stashed in the registry (idempotent;
///    `None` if `take_worker_future_for_test` was already called).
///    The future drops here even if a test never pumped it.
/// 4. Remove the fd-table entry (mirrors `sys_close(2)`). The
///    `Cap<OpenFile>` retires through EBR; when its last clone drops,
///    `OpenFile::Drop` retires the inner `Cap<AioContext>` and the
///    context payload's `Drop` fires (closing wait sources, etc.).
///
/// Returns `0` on success. Linux's `io_destroy` is documented as
/// synchronous w.r.t. in-flight ops — our canary trips the abort and
/// drops the fd; any further pumps of the worker future (if still
/// reachable) observe the abort and terminate.
pub(super) fn sys_io_destroy(ctx_fd: u32, ctx: &SyscallCtx<'_>) -> SyscallResult {
    let open_file = match ctx.process.fd(ctx_fd) {
        Some(f) => f,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    let aio_cap = match open_file.aio_context() {
        Some(c) => c.clone(),
        None => return SyscallResult::Error(EINVAL_VALUE),
    };

    // Phase 5: trip the worker abort signal so any future poll of
    // the worker future terminates with `CooperativeCancel(OwnerRequested)`.
    aio_cap.cancel_worker();

    // Drop the stashed worker future (idempotent — None if a test
    // already took it). This causes the future to drop synchronously
    // even if it was never pumped to completion.
    let _ = WORKER_REGISTRY.lock().remove(&aio_cap.context_id());

    // Remove the fd-table entry. Mirrors `sys_close(2)`: the
    // `Cap<OpenFile>` retires through EBR; the cap clone we hold
    // above (`aio_cap`) keeps the AioContext payload alive for the
    // duration of this syscall, then drops as we return.
    let _previous = ctx.process.set_fd(ctx_fd, None);
    ctx.process.set_fd_cloexec(ctx_fd, false);

    SyscallResult::Return(0)
}

// === phase-2 worker-future registry =====================================
//
// Phase 2 deferred-pump model. `sys_io_setup` stashes the worker
// future here keyed by `context_id`; tests pull it out via
// [`take_worker_future_for_test`] and pump it manually. Phase 2b
// replaces this with a `tx-kernel`-side seam that submits the future
// to the boot reactor at install time (mirroring
// `tx_subsystems::reactor_submit::install_submit_child_thread`).
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
    WORKER_INSTALL_COUNT.store(0, Ordering::Release);
}

/// Test-only: monotonic install count.
pub fn worker_install_count_for_test() -> u64 {
    WORKER_INSTALL_COUNT.load(Ordering::Acquire)
}
