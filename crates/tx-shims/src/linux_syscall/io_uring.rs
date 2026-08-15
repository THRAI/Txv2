//! `sys_io_uring_setup(2)` / `sys_io_uring_enter(2)` scaffold
//! (future PR-12 phase 0).
//!
//! Spec:
//! - `docs/Txv3/06_EXECUTION_SCOPE_v1.md` §8.1 (SQPOLL design)
//! - `docs/progress/decisions/2026-05-11-d8-pr-11-aio-plan.md` §13
//!   (future canary section)
//!
//! # What lands here
//!
//! 1. [`sys_io_uring_setup`] — the syscall dispatcher for
//!    `__NR_io_uring_setup = 425`. Mints a fresh [`IoUring`] cap (W-LL
//!    phase 0 zone), wraps it in an `OpenFile` whose backing is
//!    `OpenFileBacking::IoUring`, installs at the lowest free fd via
//!    [`tx_subsystems::process::ProcessIdentity::install_fd`], and
//!    returns the fd.
//! 2. [`sys_io_uring_enter`] — the syscall dispatcher for
//!    `__NR_io_uring_enter = 426`. Resolves the ring fd, drains the
//!    in-kernel SQ scaffold into CQEs, and returns the submitted count.
//! 3. Setup also spawns the SQPOLL kthread for the new ring by
//!    constructing a [`SqpollWorkerFuture`] via
//!    [`tx_subsystems::io_uring::spawn_sqpoll_worker_with_completion_post`]
//!    and stashing it in a per-ring registry
//!    [`take_io_uring_worker_for_test`]. This mirrors W-CC's PR-11 phase 2
//!    deferred-pump model verbatim.
//!
//! # Linux divergence
//!
//! Linux's `io_uring_setup(u32 entries, struct io_uring_params *p)`
//! returns the ring fd as the syscall result AND writes ring offsets +
//! sizes into `*p` so userspace can `mmap(2)` the SQ/CQ rings. Phase 0
//! ignores `*p` entirely (the in-kernel `VecDeque` ring shape doesn't
//! need user-mmaps yet). The future PR-12 phase 1 wires the
//! `io_uring_params` out-parameter alongside the real user-mmapped
//! rings.
//!
//! # Constraints (phase 0 / scaffold)
//!
//! - **SQE dispatch is a counter.** The kthread body increments
//!   [`tx_subsystems::io_uring::IoUring::dispatched`]; phase 1 wires
//!   the real per-SQE dispatcher (closure shape mirrors
//!   `IocbDispatcher`).
//! - **CQE ring is in-kernel `VecDeque<CqeStub>`.** Phase 1 lands the
//!   user-mmapped ring.
//! - **Minimal enter flag handling.** Basic enter flags are recognised;
//!   signal-mask enter and user-mmapped SQ/CQ parsing are deferred.
//! - **Kthread spawn is deferred-pump.** Same model as AIO phase 2 —
//!   the test pulls the stashed worker future and pumps it.
//!
//! [`IoUring`]: tx_subsystems::io_uring::IoUring
//! [`SqpollWorkerFuture`]: tx_subsystems::io_uring::SqpollWorkerFuture

extern crate alloc;
use alloc::collections::BTreeMap;
use core::sync::atomic::{AtomicU64, Ordering};

use tx_subsystems::io_uring::{
    spawn_sqpoll_worker_with_completion_post, CqeStub, IoUring, SqpollCompletionPost,
    SqpollWorkerFuture,
};
use tx_subsystems::vfs::structure::OpenFileFlags;
use tx_subsystems::vfs::OpenFile;

use super::{SyscallCtx, SyscallResult};
use super::{EBADF_VALUE, EINVAL_VALUE, EMFILE_VALUE, ENOMEM_VALUE};
use crate::adapter::step_engine::SpinMutex;

const IORING_ENTER_GETEVENTS: u32 = 1 << 0;
const IORING_ENTER_SQ_WAKEUP: u32 = 1 << 1;
const IORING_ENTER_SQ_WAIT: u32 = 1 << 2;

/// Build the owner's `SubjectContext` from the syscall ctx. Mirrors
/// the helper used by `linux_syscall::aio::sys_io_setup` — see that
/// function for the rationale.
fn build_owner_subject(ctx: &SyscallCtx<'_>) -> crate::KernelSubjectContext {
    let cred_cap = ctx.cred_cap();
    let restrictions_cap = ctx.restrictions_cap();
    let authority = crate::KernelSubjectAuthority::new(cred_cap, restrictions_cap);
    crate::KernelSubjectContext::from_thread(ctx.process.clone(), ctx.thread.clone(), authority)
}

fn build_sqpoll_completion_post(ctx: &SyscallCtx<'_>) -> SqpollCompletionPost {
    let with_hint = ctx.mailbox_ref_post_with_hint;
    let plain = ctx.mailbox_ref_post;
    alloc::sync::Arc::new(move |mailbox, event| {
        if let Some(post) = with_hint {
            return post(
                mailbox,
                event,
                tx_substrate::wake::MailboxSchedulerHint::Normal,
            );
        }
        if let Some(post) = plain {
            return post(mailbox, event);
        }
        mailbox.post(event)
    })
}

/// `io_uring_setup(entries, params)` syscall arm — second
/// `OnBehalfOf<P>` canary.
///
/// Per `man 2 io_uring_setup`:
/// - `entries`: the requested SQ ring depth (rounded up to a power of
///   two in production; phase 0 stashes verbatim).
/// - `params`: a user pointer to a `struct io_uring_params`. Phase 0
///   ignores this argument — the in-kernel `VecDeque` ring shape
///   doesn't yet expose offsets/sizes to userspace. Phase 1 will write
///   the SQ/CQ-ring layout back through it.
///
/// **Flow** (mirrors `sys_io_setup` row-for-row — the framework
/// reusability claim):
/// 1. Mint a fresh `Cap<IoUring>` via the W-LL phase 0 zone.
/// 2. Build the owner's `SubjectContext` from the syscall ctx.
/// 3. Construct the SQPOLL kthread future via
///    [`spawn_sqpoll_worker_with_completion_post`]. The kthread enters
///    `with_on_behalf_of(owner, body)` at startup; the borrow holds
///    for the entire ring lifetime (per D8 §13).
/// 4. Stash the kthread future in the test registry keyed by
///    `ring_id` (deferred-pump model).
/// 5. Wrap the cap in an `OpenFile` with `OpenFileBacking::IoUring`,
///    install at the lowest free fd, return the fd.
///
/// Returns the new fd on success, `-ENOMEM` if zone allocation fails.
pub(super) fn sys_io_uring_setup(
    entries: u32,
    _params_ptr: u64,
    ctx: &SyscallCtx<'_>,
) -> SyscallResult {
    // 1. Mint a fresh `Cap<IoUring>`. Linux defaults `cq_entries` to
    //    `2 * sq_entries`; phase 0 captures the user's request shape
    //    by defaulting to that ratio.
    let cq_entries = entries.saturating_mul(2);
    let ring_cap = match IoUring::new_with_entries_cap(entries, cq_entries) {
        Ok(cap) => cap,
        Err(_) => return SyscallResult::Error(ENOMEM_VALUE),
    };

    // 2 + 3 + 4. Spawn the SQPOLL kthread, stash the future for the
    // deferred-pump model. Mirrors W-CC's AIO phase-2 pattern; the
    // production wiring will submit to the boot reactor through a
    // function-pointer seam (a future PR-12 phase 2b follow-up).
    let ring_id = ring_cap.ring_id();
    let owner_subject = build_owner_subject(ctx);
    let completion_post = build_sqpoll_completion_post(ctx);
    let worker = spawn_sqpoll_worker_with_completion_post(
        ring_cap.clone(),
        ctx.process.clone(),
        owner_subject,
        completion_post,
    );
    install_io_uring_worker_for_test(ring_id, worker);

    // 5. Wrap in an `OpenFile`. Phase 0 leaves every `OpenFileFlags`
    //    bit at its default — the io_uring fd is not a VFS-readable /
    //    writable object directly (userspace must use `io_uring_enter`
    //    or read the user-mmapped rings). Userspace that wants
    //    `O_CLOEXEC` calls `fcntl(fd, F_SETFD, FD_CLOEXEC)` after
    //    setup returns.
    let open_flags = OpenFileFlags::default();
    let open_cap = match OpenFile::new_io_uring_cap(ring_cap, open_flags) {
        Ok(cap) => cap,
        Err(_) => return SyscallResult::Error(ENOMEM_VALUE),
    };

    // Install at the lowest free fd.
    let Some(fd) = ctx.process.install_new_fd(open_cap, false) else {
        return SyscallResult::Error(EMFILE_VALUE);
    };

    SyscallResult::Return(fd as i64)
}

/// `io_uring_enter(fd, to_submit, min_complete, flags, sig, sigsz)`.
///
/// Tx's current io_uring scaffold has an in-kernel SQ/CQ pair rather
/// than Linux's user-mmapped rings. This arm still follows the same
/// syscall contract shape: resolve the ring fd, accept the basic enter
/// flags used by SQPOLL/GETEVENTS callers, drain up to `to_submit`
/// SQEs from the ring, publish one zero-result CQE per submitted SQE,
/// and return the submitted count. Waiting for `min_complete` is a
/// later mailbox-backed sleep extension; today the call is nonblocking.
pub(super) fn sys_io_uring_enter(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let fd = args[0] as u32;
    let to_submit = args[1] as u32;
    let _min_complete = args[2] as u32;
    let flags = args[3] as u32;
    let sig = args[4];
    let sigsz = args[5];

    let recognised_flags = IORING_ENTER_GETEVENTS | IORING_ENTER_SQ_WAKEUP | IORING_ENTER_SQ_WAIT;
    if flags & !recognised_flags != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if sig != 0 || sigsz != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    let open_file = match super::resolve_fd(&ctx.process, fd) {
        Some(file) => file,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    let ring = match open_file.io_uring() {
        Some(ring) => ring.clone(),
        None => return SyscallResult::Error(EINVAL_VALUE),
    };

    let mut submitted = 0u32;
    while submitted < to_submit {
        let Some(sqe) = ring.pop_sqe() else {
            break;
        };
        ring.push_cqe_with_post(CqeStub::new(sqe.user_data, 0, 0), |mailbox, event| {
            ctx.post_mailbox_ref_event(mailbox, event)
        });
        submitted += 1;
    }

    SyscallResult::Return(submitted as i64)
}

// === phase-0 worker-future registry =====================================
//
// Same deferred-pump model as W-CC's AIO phase-2 worker registry.
// `sys_io_uring_setup` stashes the SQPOLL kthread future here keyed by
// `ring_id`; tests pull it out via [`take_io_uring_worker_for_test`]
// and pump it manually. The phase-2b follow-up (future PR-12) replaces
// this with a `tx-kernel`-side seam that submits the future to the
// boot reactor at install time.

static IO_URING_WORKER_REGISTRY: SpinMutex<BTreeMap<u64, SqpollWorkerFuture>> =
    SpinMutex::new(BTreeMap::new());

/// Monotonic counter of SQPOLL kthread installs; tests use it to
/// confirm a kthread was spawned per `io_uring_setup` call.
static IO_URING_WORKER_INSTALL_COUNT: AtomicU64 = AtomicU64::new(0);

fn install_io_uring_worker_for_test(ring_id: u64, fut: SqpollWorkerFuture) {
    IO_URING_WORKER_REGISTRY.lock().insert(ring_id, fut);
    IO_URING_WORKER_INSTALL_COUNT.fetch_add(1, Ordering::AcqRel);
}

/// Test-only: remove and return the SQPOLL kthread future for
/// `ring_id`, if any. Tests pump the returned future manually to
/// confirm the borrow-scope body observes pushed SQEs and the abort
/// signal.
pub fn take_io_uring_worker_for_test(ring_id: u64) -> Option<SqpollWorkerFuture> {
    IO_URING_WORKER_REGISTRY.lock().remove(&ring_id)
}

/// Test-only: clear every stashed SQPOLL kthread future. Used by
/// per-test setup so a previous test's residual futures don't surface
/// as false positives in the install counter.
pub fn reset_io_uring_worker_registry_for_test() {
    IO_URING_WORKER_REGISTRY.lock().clear();
    IO_URING_WORKER_INSTALL_COUNT.store(0, Ordering::Release);
}

/// Test-only: monotonic install count.
pub fn io_uring_worker_install_count_for_test() -> u64 {
    IO_URING_WORKER_INSTALL_COUNT.load(Ordering::Acquire)
}
