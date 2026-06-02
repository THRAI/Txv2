//! Auto-extracted from `mod.rs` during the 2026-05-08 jumbo-mod
//! split. The implementations here are unchanged; only the
//! enclosing module changed. Helpers and constants used here live
//! either in this submodule or in the shared parent (`super::*`).

use super::*;
use alloc::collections::BTreeMap;

use crate::adapter::step_engine::SpinMutex;
use tx_substrate::verbs::OperationalCapExt;
use tx_subsystems::process::numbers::{resolve_pid_number_as, PidName, PidNameKind};
use tx_subsystems::signal::{step_kill_pgrp, SigInfo, SI_USER};
use tx_subsystems::signal::{KillOutcome, SignalTarget};
use tx_subsystems::thread_runtime::execution::step_sigprocmask;
use tx_subsystems::vfs::structure::{OpenFile, OpenFileFlags};

#[cfg(target_arch = "loongarch64")]
const MUSL_SIGCANCEL: u8 = 33;
const SI_TKILL: i32 = -6;

#[repr(C)]
#[derive(Clone, Copy, Default, Eq, PartialEq)]
struct SigaltstackLayout {
    ss_sp: u64,
    ss_flags: i32,
    _pad: u32,
    ss_size: u64,
}

const _: () = assert!(core::mem::size_of::<SigaltstackLayout>() == 24);
const SS_ONSTACK: i32 = 1;
const SS_DISABLE: i32 = 2;
const SS_AUTODISARM: i32 = 1 << 31;

pub(super) mod layout_descriptors {
    use core::mem::{align_of, offset_of, size_of};

    pub(super) use super::SigaltstackLayout;
    use crate::linux_syscall::{KernelToUserLayout, KernelUserField, KernelUserLayout};

    impl KernelToUserLayout for SigaltstackLayout {
        const LAYOUT: KernelUserLayout = KernelUserLayout {
            rust_type: "SigaltstackLayout",
            musl_header: "signal.h",
            musl_type: "struct sigaltstack",
            size: size_of::<SigaltstackLayout>(),
            align: align_of::<SigaltstackLayout>(),
            fields: &[
                KernelUserField {
                    rust: "ss_sp",
                    musl: "ss_sp",
                    offset: offset_of!(SigaltstackLayout, ss_sp),
                },
                KernelUserField {
                    rust: "ss_flags",
                    musl: "ss_flags",
                    offset: offset_of!(SigaltstackLayout, ss_flags),
                },
                KernelUserField {
                    rust: "ss_size",
                    musl: "ss_size",
                    offset: offset_of!(SigaltstackLayout, ss_size),
                },
            ],
        };
    }
    pub(in crate::linux_syscall) const SIGALTSTACK_LAYOUT: KernelUserLayout =
        <SigaltstackLayout as KernelToUserLayout>::LAYOUT;
}

/// Drain `SignalDelivered` wake hints from a thread mailbox.
///
/// `sys_rt_sigtimedwait` consumes truth from the pending-signal queues,
/// not from mailbox events. A stale signal wake hint can otherwise make
/// the next park return immediately even though the relevant pending bit
/// has already been consumed.
fn drain_stale_signal_events(mailbox: &crate::adapter::reactor_entry::TaskMailbox) {
    use crate::adapter::reactor_entry::MailboxEvent;
    let mut keep = alloc::vec::Vec::new();
    while let Some(event) = mailbox.poll() {
        if !matches!(event, MailboxEvent::SignalDelivered { .. }) {
            keep.push(event);
        }
    }
    for event in keep {
        let _ = mailbox.post(event);
    }
}

static SIGACTION_RESTORERS: SpinMutex<BTreeMap<(u32, u8), usize>> = SpinMutex::new(BTreeMap::new());

fn remember_sigaction_restorer(pid: u32, sig: Signum, restorer: u64) {
    let mut restorers = SIGACTION_RESTORERS.lock();
    let key = (pid, sig.raw());
    if restorer == 0 {
        restorers.remove(&key);
    } else {
        restorers.insert(key, restorer as usize);
    }
}

pub(super) fn sigaction_restorer(pid: u32, sig: Signum) -> Option<usize> {
    SIGACTION_RESTORERS.lock().get(&(pid, sig.raw())).copied()
}

/// `rt_sigprocmask(how, set, oldset, sigsetsize)` per `SIGNAL_v1` §3.
///
/// `sigsetsize` is rejected with `-EINVAL` for any value other than
/// `8` (the kernel's only supported sigset width on RV64 — a single
/// `u64` bitset). `set_ptr == 0` means "query only"; `oldset_ptr == 0`
/// means "don't return the previous mask".
pub(super) fn sys_rt_sigprocmask<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let how_raw = args[0] as i32;
    let set_ptr = args[1] as usize;
    let oldset_ptr = args[2] as usize;
    let sigsetsize = args[3];

    if sigsetsize != SIGSETSIZE_BYTES {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    // Decode `how` per Linux generic ABI: 0 = SIG_BLOCK, 1 = SIG_UNBLOCK,
    // 2 = SIG_SETMASK. `set_ptr == 0` short-circuits to a query-only
    // path — `step_sigprocmask` doesn't need to run because the mask
    // doesn't change; we only need to read the current value out for
    // `oldset_ptr` writeback.
    let how = match (how_raw, set_ptr) {
        (_, 0) => None,
        (0, _) => Some(SigmaskHow::Block),
        (1, _) => Some(SigmaskHow::Unblock),
        (2, _) => Some(SigmaskHow::SetMask),
        _ => return SyscallResult::Error(EINVAL_VALUE),
    };

    // Read the user-supplied set bitset through the canonical
    // user-VA lane (`bootstrap_read_user` bridges via
    // `aspace.read_user`, falling back to a kernel-pointer read on
    // EFAULT).
    let next_mask = if set_ptr == 0 {
        SignalMask::EMPTY
    } else {
        match bootstrap_read_user::<u64>(&ctx.aspace, set_ptr as u64) {
            Ok(bits) => SignalMask::new(bits),
            Err(errno) => return SyscallResult::error_from(errno),
        }
    };

    // If `set` is null we still need the previous mask to satisfy
    // `oldset_ptr`. `step_sigprocmask` returns `prev` from the
    // change record, so call it with `SetMask` of the *current* bits
    // (a no-op, plus it uniformly produces a `Replaced` record). The
    // simpler approach: skip the call and read the mask directly via
    // `step_sigprocmask` invoked with a no-op `SetMask` of `prev`...
    // but the cleanest shape is to call `step_sigprocmask` always
    // when `how` is Some, and for the query-only branch bypass it.
    let prev_mask: SignalMask = match how {
        Some(how) => {
            let mut script_ctx = build_subject_script_ctx(ctx);
            let mut op = SigprocmaskOp {
                thread: ctx.thread.clone(),
                how,
                next: next_mask,
            };
            match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
                Ok(SigprocmaskChange::Replaced { prev, .. }) => prev,
                Ok(SigprocmaskChange::ZombieIgnored) => {
                    return SyscallResult::Error(ESRCH_VALUE);
                }
                Err(v3errno) => {
                    return SyscallResult::error_from(v3errno);
                }
            }
        }
        None => {
            let mut script_ctx = build_subject_script_ctx(ctx);
            let mut op = SigprocmaskOp {
                thread: ctx.thread.clone(),
                how: SigmaskHow::Block,
                next: SignalMask::EMPTY,
            };
            match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
                Ok(SigprocmaskChange::Replaced { prev, .. }) => prev,
                Ok(SigprocmaskChange::ZombieIgnored) => {
                    return SyscallResult::Error(ESRCH_VALUE);
                }
                Err(v3errno) => {
                    return SyscallResult::error_from(v3errno);
                }
            }
        }
    };

    if oldset_ptr != 0 {
        if let Err(errno) =
            bootstrap_write_user::<u64>(&ctx.aspace, oldset_ptr as u64, prev_mask.raw_bits())
        {
            return SyscallResult::error_from(errno);
        }
    }

    SyscallResult::Return(0)
}

// --- Stub syscalls (deferred to post-bringup) -------------------------

fn set_thread_signal_mask(ctx: &SyscallCtx, next: SignalMask) -> Result<SignalMask, SyscallResult> {
    let mut script_ctx = build_subject_script_ctx(ctx);
    let mut op = SigprocmaskOp {
        thread: ctx.thread.clone(),
        how: SigmaskHow::SetMask,
        next,
    };
    match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
        Ok(SigprocmaskChange::Replaced { prev, .. }) => Ok(prev),
        Ok(SigprocmaskChange::ZombieIgnored) => Err(SyscallResult::Error(ESRCH_VALUE)),
        Err(v3errno) => Err(SyscallResult::error_from(Errno::from(v3errno))),
    }
}

pub(super) async fn sys_rt_sigsuspend<P: tx_hal::TimeIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'_>,
) -> SyscallResult {
    let mask_ptr = args[0];
    let sigset_size = args[1];
    if mask_ptr == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }
    if sigset_size != core::mem::size_of::<u64>() as u64 {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    let mask_bits = match bootstrap_read_user::<u64>(&ctx.aspace, mask_ptr) {
        Ok(bits) => bits,
        Err(errno) => return SyscallResult::error_from(errno),
    };
    let old_mask = match set_thread_signal_mask(ctx, SignalMask::new(mask_bits)) {
        Ok(mask) => mask,
        Err(result) => return result,
    };

    let restore_and_eintr = |ctx: &SyscallCtx<'_>, old_mask: SignalMask| {
        let _ = set_thread_signal_mask(ctx, old_mask);
        SyscallResult::Error(EINTR_VALUE)
    };

    const CHUNK_NS: u64 = 5_000_000;
    loop {
        if let Some(deadline_ns) = poll_due_itimers::<P>(&ctx.process) {
            P::set_deadline_ns(deadline_ns);
        }
        if tx_subsystems::signal::select_next_signal(&ctx.thread).is_some() {
            return restore_and_eintr(ctx, old_mask);
        }

        use crate::adapter::step_engine::DriveMode;
        use tx_scripts::drive;
        let now_ns = <P as tx_hal::TimeIf>::read_ns();
        let mut script_ctx = build_subject_script_ctx(ctx);
        let timer_wheel_arc = script_ctx.timer_wheel().cloned();
        let mailbox = ctx.mailbox.clone();
        let op = super::NanosleepOp {
            nanos: CHUNK_NS,
            deadline_ns: now_ns.saturating_add(CHUNK_NS),
            started: false,
        };
        match drive(
            op,
            &mut script_ctx,
            DriveMode::Waiting,
            mailbox.as_ref(),
            None,
            timer_wheel_arc.as_ref(),
        )
        .await
        {
            Ok(()) => {}
            Err(v3errno) => {
                let errno: Errno = v3errno.into();
                if errno == Errno::EINTR {
                    return restore_and_eintr(ctx, old_mask);
                }
                let _ = set_thread_signal_mask(ctx, old_mask);
                return SyscallResult::error_from(errno);
            }
        }
    }
}

/// `sigaltstack(ss, old_ss)` — Linux LP64 `stack_t` query/update.
///
/// This records the registered alternate stack in
/// `ThreadPayload.alt_stack` and reports it back through `old_ss`.
/// Signal-frame delivery still uses the current user stack today, so
/// `SA_ONSTACK` delivery semantics remain deferred; the syscall
/// pointer contract itself is Linux/musl-shaped.
pub(super) fn sys_sigaltstack(args: [u64; 6], ctx: &SyscallCtx) -> SyscallResult {
    let ss_ptr = args[0];
    let old_ss_ptr = args[1];
    let Some(payload) = ctx.thread.payload_cap() else {
        return SyscallResult::Error(ESRCH_VALUE);
    };

    if old_ss_ptr != 0 {
        let (ss_sp, ss_flags, ss_size) = match payload.alt_stack() {
            Some((base, size)) => (base as u64, 0, size as u64),
            None => (0, SS_DISABLE, 0),
        };
        let old = SigaltstackLayout {
            ss_sp,
            ss_flags,
            _pad: 0,
            ss_size,
        };
        if let Err(errno) = bootstrap_write_user::<SigaltstackLayout>(&ctx.aspace, old_ss_ptr, old)
        {
            return SyscallResult::error_from(errno);
        }
    }

    if ss_ptr != 0 {
        let new = match bootstrap_read_user::<SigaltstackLayout>(&ctx.aspace, ss_ptr) {
            Ok(value) => value,
            Err(errno) => return SyscallResult::error_from(errno),
        };
        let allowed = SS_DISABLE | SS_AUTODISARM;
        if new.ss_flags & !allowed != 0 {
            return SyscallResult::Error(EINVAL_VALUE);
        }
        if new.ss_flags & SS_DISABLE != 0 {
            payload.set_alt_stack(None);
        } else {
            if new.ss_size < MINSIGSTKSZ {
                return SyscallResult::Error(ENOMEM_VALUE);
            }
            payload.set_alt_stack(Some((new.ss_sp as usize, new.ss_size as usize)));
        }
    }

    SyscallResult::Return(0)
}

pub(super) fn sys_rt_sigqueueinfo(_args: [u64; 6], _ctx: &SyscallCtx) -> SyscallResult {
    SyscallResult::Error(ENOSYS_VALUE)
}

fn lowest_sigtimedwait_bit(bits: u64) -> Option<Signum> {
    if bits == 0 {
        return None;
    }
    let raw = bits.trailing_zeros() as u8 + 1;
    Signum::new(raw)
}

fn take_matching_pending_signal(ctx: &SyscallCtx<'_>, wait_bits: u64) -> Option<Signum> {
    let thread_payload = ctx.thread.payload_cap()?;
    let thread_match = thread_payload.pending().snapshot() & wait_bits;
    if let Some(sig) = lowest_sigtimedwait_bit(thread_match) {
        thread_payload.pending().clear(sig);
        tx_subsystems::signal::refresh_deliverable_signal_summary(&ctx.thread);
        return Some(sig);
    }

    let proc_payload = ctx.process.upgrade_operational().ok()?;
    let group_match = proc_payload.group_pending().snapshot() & wait_bits;
    let sig = lowest_sigtimedwait_bit(group_match)?;
    proc_payload.group_pending().clear(sig);
    tx_subsystems::signal::refresh_deliverable_signal_summary(&ctx.thread);
    Some(sig)
}

fn write_sigtimedwait_siginfo(ctx: &SyscallCtx<'_>, info_ptr: u64, sig: Signum) -> Result<(), i32> {
    if info_ptr == 0 {
        return Ok(());
    }

    let mut image = [0u8; 128];
    image[0..4].copy_from_slice(&(sig.raw() as u32).to_le_bytes());
    image[8..12].copy_from_slice(&0i32.to_le_bytes());
    bootstrap_copy_to_user(&ctx.aspace, info_ptr, &image).map_err(errno_to_i32)
}

async fn park_sigtimedwait_tick<P: tx_hal::TimeIf>(
    ctx: &SyscallCtx<'_>,
    wait_bits: u64,
    deadline_ns: Option<u64>,
) {
    const SIGTIMEDWAIT_POLL_NS: u64 = 1_000_000;

    if wait_bits & Signum::SIGCHLD.bit() != 0 && deadline_ns.is_none() {
        if let Some(token) = ctx.process.exit_source_wait_token() {
            if let Some(future) = tx_subsystems::wait_source::wait_on_token(token) {
                future.await;
                return;
            }
        }
    }

    let now = <P as tx_hal::TimeIf>::read_ns();
    let next = match deadline_ns {
        Some(deadline) => core::cmp::min(deadline, now.saturating_add(SIGTIMEDWAIT_POLL_NS)),
        None => now.saturating_add(SIGTIMEDWAIT_POLL_NS),
    };
    if let Some(future) = tx_subsystems::timer_sleep::sleep_until_ns(next) {
        future.await;
    } else {
        tx_reactor::yield_now().await;
    }
}

/// `rt_sigtimedwait(set, info, timeout, sigsetsize)`.
///
/// This is the small POSIX wait surface needed by the libctest
/// harness: consume a pending signal named by `set`, optionally wait
/// until `timeout` expires, and write the leading Linux `siginfo_t`
/// fields. Signal-mask interaction is intentionally different from
/// normal delivery: `sigtimedwait(2)` observes pending signals in
/// `set` even when they are blocked, which is exactly how runtest waits
/// for a child `SIGCHLD`.
pub(super) async fn sys_rt_sigtimedwait<'a, P: tx_hal::TimeIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    let set_ptr = args[0];
    let info_ptr = args[1];
    let timeout_ptr = args[2];
    let sigsetsize = args[3];

    // The slice only supports the canonical 8-byte sigset_t on RV64;
    // mirrors the `sys_rt_sigprocmask` precedent (`SIGSETSIZE_BYTES`).
    if sigsetsize != SIGSETSIZE_BYTES {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if set_ptr == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }

    let wait_bits = match bootstrap_read_user::<u64>(&ctx.aspace, set_ptr) {
        Ok(bits) => bits,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };
    let timeout_ns = if timeout_ptr == 0 {
        None
    } else {
        match read_timespec_at(&ctx.aspace, timeout_ptr) {
            Some(ns) => Some(ns),
            None => return SyscallResult::Error(EINVAL_VALUE),
        }
    };
    let deadline_ns = timeout_ns.map(|ns| <P as tx_hal::TimeIf>::read_ns().saturating_add(ns));

    // `await_mailbox_event` (in `tx-scripts::drive::resolve_on_timer`)
    // RE-POSTS any `SignalDelivered` event it consumes back to the
    // mailbox so the caller's main loop can observe it. For
    // sigtimedwait, the "main loop" IS this loop, and we consume
    // signals directly from `payload.pending()` — so the re-posted
    // event is stale information. If left in the queue it traps every
    // subsequent `drive(NanosleepOp)` call into returning Ready(true)
    // immediately, creating a no-actual-sleep tight loop. Drain any
    // pre-existing `SignalDelivered` events left over from a previous
    // sigtimedwait call on this same thread before the poll loop
    // starts; the per-iteration drain below handles new arrivals.
    let mailbox_for_drain = ctx.mailbox.clone();
    if let Some(ref mbox) = mailbox_for_drain {
        drain_stale_signal_events(mbox);
    }
    loop {
        if let Some(sig) = take_matching_pending_signal(ctx, wait_bits) {
            if let Err(errno) = write_sigtimedwait_siginfo(ctx, info_ptr, sig) {
                return SyscallResult::Error(errno);
            }
            return SyscallResult::Return(sig.raw() as i64);
        }

        match deadline_ns {
            Some(deadline) if <P as tx_hal::TimeIf>::read_ns() >= deadline => {
                return SyscallResult::Error(EAGAIN_VALUE);
            }
            Some(_) | None => {
                park_sigtimedwait_tick::<P>(ctx, wait_bits, deadline_ns).await;
                if let Some(ref mbox) = ctx.mailbox {
                    drain_stale_signal_events(mbox);
                }
            }
        }
    }
}

pub(super) fn sys_pidfd_open(args: [u64; 6], ctx: &SyscallCtx) -> SyscallResult {
    let pid_raw = args[0];
    let flags = args[1] as u32;

    if pid_raw == 0 || pid_raw > i32::MAX as u64 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if flags & !PIDFD_NONBLOCK != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    let target = match resolve_pid_number_as(pid_raw, PidNameKind::Process) {
        Some(PidName::Process(process)) => process,
        _ => return SyscallResult::Error(ESRCH_VALUE),
    };

    let fd = ctx.process.allocate_fd();
    let (soft_limit, _) = ctx.process.rlimit_nofile();
    if fd >= soft_limit {
        return SyscallResult::Error(EMFILE_VALUE);
    }

    let open_flags = OpenFileFlags {
        read: true,
        write: false,
        append: false,
        cloexec: true,
        nonblocking: (flags & PIDFD_NONBLOCK) != 0,
    };
    let open_cap = match OpenFile::new_pidfd_cap(target, open_flags) {
        Ok(cap) => cap,
        Err(_) => return SyscallResult::Error(ENOMEM_VALUE),
    };
    let _ = ctx.process.install_fd(fd, open_cap);
    ctx.process.set_fd_cloexec(fd, true);
    SyscallResult::Return(fd as i64)
}

fn process_from_pidfd_like_file(file: &OpenFile) -> Option<Cap<ProcessIdentity>> {
    if let Some(process) = file.pidfd_process() {
        return Some(process.clone());
    }
    match file.backing() {
        OpenFileBacking::Rnode { rnode } => {
            tx_fs::procfs::pid_from_dir(rnode.fs_object_id()).and_then(process_by_pid)
        }
        _ => None,
    }
}

pub(super) fn sys_pidfd_send_signal(args: [u64; 6], ctx: &SyscallCtx) -> SyscallResult {
    let fd = args[0] as u32;
    let sig = args[1] as u32;
    let info_ptr = args[2];
    let flags = args[3] as u32;

    if flags != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    let file = match ctx.process.fd(fd) {
        Some(file) => file,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    let target = match process_from_pidfd_like_file(&file) {
        Some(target) => target,
        None => return SyscallResult::Error(EBADF_VALUE),
    };

    if sig > 64 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if info_ptr != 0 {
        let mut signo_bytes = [0u8; 4];
        if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut signo_bytes, info_ptr) {
            return SyscallResult::error_from(errno);
        }
        let info_signo = u32::from_le_bytes(signo_bytes);
        if info_signo != sig {
            return SyscallResult::Error(EINVAL_VALUE);
        }
    }
    if sig == 0 {
        return SyscallResult::Return(0);
    }
    let signum = match u8::try_from(sig).ok().and_then(Signum::new) {
        Some(signum) => signum,
        None => return SyscallResult::Error(EINVAL_VALUE),
    };
    let siginfo = Some(SigInfo {
        si_signo: signum.raw() as u32,
        si_code: SI_USER,
        si_pid: ctx.process.pid.0,
        si_uid: 0,
    });

    dispatch_errno(
        tx_subsystems::signal::script_deliver_signal(
            &ctx.process,
            SignalTarget::Process(target),
            signum,
            siginfo,
        ),
        |outcome| match outcome {
            KillOutcome::Delivered => SyscallResult::Return(0),
            KillOutcome::NoLiveThread => SyscallResult::Error(ESRCH_VALUE),
        },
    )
}

/// `rt_sigaction(signum, act, oldact, sigsetsize)` per `SIGNAL_v1`
/// §15.1.
///
/// Decodes the RV64 kernel `struct sigaction` (see `SIGACTION_BYTES`
/// for the layout citation). `act_ptr == 0` queries the current
/// disposition without changing it; `oldact_ptr == 0` discards the
/// previous disposition.
pub(super) fn sys_rt_sigaction<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let signum_raw = args[0] as u32;
    let act_ptr = args[1] as usize;
    let oldact_ptr = args[2] as usize;
    let sigsetsize = args[3];

    if sigsetsize != SIGSETSIZE_BYTES {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    let Some(sig) = (if signum_raw <= u8::MAX as u32 {
        Signum::new(signum_raw as u8)
    } else {
        None
    }) else {
        return SyscallResult::Error(EINVAL_VALUE);
    };

    if act_ptr != 0 && sig.is_uncatchable() {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    // Decode the new action (if any) through the canonical user-VA
    // lane (`bootstrap_copy_from_user` bridges via
    // `aspace.copy_from_user`).
    let new_entry: Option<SigActionEntry> = if act_ptr == 0 {
        None
    } else {
        let mut bytes = [0u8; SIGACTION_BYTES];
        if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut bytes, act_ptr as u64) {
            return SyscallResult::error_from(errno);
        }
        let handler = read_u64_le(&bytes[0..8]);
        let flags = SaFlags::new(read_u64_le(&bytes[8..16]));
        let mask = SignalMask::new(read_u64_le(&bytes[16..24]));

        // SIG_DFL == 0, SIG_IGN == 1 per Linux generic ABI; everything
        // else is a userspace function-pointer handler.
        let disp = match handler {
            0 => SigDisposition::Default,
            1 => SigDisposition::Ignore,
            other => SigDisposition::Handler(other as usize),
        };
        Some(SigActionEntry::new(disp, flags, mask, 0))
    };

    // If the caller wants the previous disposition, snapshot it
    // *before* installing the new one. `SigactionOp` returns the
    // prev as part of `SigDispositionChange`, so a single call suffices
    // for both install and query — but `act_ptr == 0` is "query only",
    // and we must not mutate. Read the live disposition through the
    // process's `sig_actions` table accessor in that case.
    let prev_entry: SigActionEntry = match new_entry {
        Some(entry) => {
            let mut script_ctx = build_subject_script_ctx(ctx);
            let mut op = SigactionOp {
                process: ctx.process.clone(),
                sig,
                entry,
            };
            match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
                Ok(SigDispositionChange::Replaced { prev }) => {
                    remember_sigaction_restorer(ctx.process.pid.0, sig, entry.restorer as u64);
                    prev
                }
                Ok(SigDispositionChange::Uncatchable(prev)) => prev,
                Ok(SigDispositionChange::ZombieIgnored) => {
                    return SyscallResult::Error(ESRCH_VALUE);
                }
                Err(v3errno) => {
                    return SyscallResult::error_from(v3errno);
                }
            }
        }
        None => {
            // Query-only: read directly via the process's
            // `sig_action_entry` accessor. Returns `None` for zombies
            // — surface as `-ESRCH`.
            match ctx.process.sig_action_entry(sig) {
                Some(entry) => entry,
                None => {
                    return SyscallResult::Error(ESRCH_VALUE);
                }
            }
        }
    };

    if oldact_ptr != 0 {
        let handler_value: u64 = match prev_entry.disposition {
            SigDisposition::Default => 0, // SIG_DFL
            SigDisposition::Ignore => 1,  // SIG_IGN
            SigDisposition::Handler(addr) => addr as u64,
        };
        // Build an RV64 image and copy out through the canonical
        // user-VA lane. RV64 layout: 3×u64 little-endian
        // (handler, flags, mask); the architecture does not carry an
        // in-struct userspace restorer.
        let mut image = [0u8; SIGACTION_BYTES];
        image[0..8].copy_from_slice(&handler_value.to_le_bytes());
        image[8..16].copy_from_slice(&prev_entry.flags.bits().to_le_bytes());
        image[16..24].copy_from_slice(&prev_entry.sa_mask.raw_bits().to_le_bytes());
        if let Err(errno) = bootstrap_copy_to_user(&ctx.aspace, oldact_ptr as u64, &image) {
            return SyscallResult::error_from(errno);
        }
    }

    SyscallResult::Return(0)
}

#[cfg(test)]
pub(super) fn reset_sigaction_restorers_for_test() {
    SIGACTION_RESTORERS.lock().clear();
}

/// `kill(pid, sig)` — Linux RV64 generic ABI `__NR_kill = 129`.
///
/// Slice 7 v1 surface:
/// - `pid > 0`: deliver `sig` to the matching process via
///   `tx_subsystems::signal::step_kill_process`. Resolved through
///   `process_by_pid`'s init-rooted tree walk.
/// - `pid == 0`: deliver to the caller's process group.
/// - `pid < -1`: deliver to process group `-pid`.
/// - `pid == -1`: all-processes target — out of scope for v1
///   (`-ENOSYS`).
/// - `sig == 0`: existence probe — return `0` if the target exists
///   (live or zombie), `-ESRCH` otherwise. Linux semantic.
/// - Unknown signum (outside 1..=64): `-EINVAL`.
/// - Target zombie / no live thread: `-ESRCH` (matches Linux's
///   "kill returns ESRCH if no signal could be delivered").
pub(super) fn sys_kill(args: [u64; 6], ctx: &SyscallCtx) -> SyscallResult {
    let pid = args[0] as i32;
    let sig = args[1] as u32;

    // Phase F: pid == 0 routes to caller's process group.
    if pid == 0 {
        if sig == 0 {
            return SyscallResult::Return(0); // existence probe
        }
        let signum = match u8::try_from(sig).ok().and_then(Signum::new) {
            Some(s) => s,
            None => return SyscallResult::Error(EINVAL_VALUE),
        };
        let pgrp = ctx.process.pgrp_cap();
        // Route through script_kill_pgrp (cred-checked per-member fanout)
        // rather than the primitive step_kill_pgrp, which would deliver
        // without consulting cred::require_signal_send.
        // POSIX: kill(0, sig) returns -EPERM when no member was both
        // live AND permitted; script_kill_pgrp folds zombies and
        // permission denials into the same 0-count return.
        return dispatch_errno(
            tx_subsystems::signal::script_kill_pgrp(&ctx.process, &pgrp, signum),
            |n| {
                if n > 0 {
                    SyscallResult::Return(0)
                } else {
                    SyscallResult::Error(EPERM_VALUE)
                }
            },
        );
    }
    if pid == -1 {
        // TODO(phase-all-processes-kill): Linux `kill(-1, sig)` sends
        // to every permitted process except implementation-specific
        // exclusions. Keep it explicit instead of confusing it with
        // process-group dispatch.
        return SyscallResult::Error(ENOSYS_VALUE);
    }
    if pid < -1 {
        let pgid = match pid.checked_neg() {
            Some(pgid) => pgid as u64,
            None => return SyscallResult::Error(ESRCH_VALUE),
        };
        let pgrp = match resolve_pid_number_as(pgid, PidNameKind::ProcessGroup) {
            Some(PidName::ProcessGroup(pgrp)) => pgrp,
            _ => return SyscallResult::Error(ESRCH_VALUE),
        };
        if sig == 0 {
            return SyscallResult::Return(0);
        }
        let signum = match u8::try_from(sig).ok().and_then(Signum::new) {
            Some(s) => s,
            None => return SyscallResult::Error(EINVAL_VALUE),
        };
        return dispatch_errno(
            tx_subsystems::signal::script_kill_pgrp(&ctx.process, &pgrp, signum),
            |n| {
                if n > 0 {
                    SyscallResult::Return(0)
                } else {
                    SyscallResult::Error(EPERM_VALUE)
                }
            },
        );
    }

    let target = match process_by_pid(Pid(pid as u32)) {
        Some(t) => t,
        None => return SyscallResult::Error(ESRCH_VALUE),
    };

    if sig == 0 {
        // Existence probe: 0 for live or zombie targets, -ESRCH for
        // missing (handled above by the `process_by_pid` None branch).
        return SyscallResult::Return(0);
    }

    // Bound check: Linux signums are 1..=64 (the realtime range
    // shares the same encoding as `Signum`). Anything outside that
    // is `-EINVAL`.
    let signum = match u8::try_from(sig).ok().and_then(Signum::new) {
        Some(s) => s,
        None => return SyscallResult::Error(EINVAL_VALUE),
    };

    // Route through the cred-checked, disposition-aware script entry
    // point. Default-terminate signals such as SIGTERM must take
    // effect even if the target is blocked inside a syscall (for
    // example a server waiting in accept(2)); merely posting the bit
    // and waiting for a later AST checkpoint leaves such daemons alive.
    let siginfo = Some(SigInfo {
        si_signo: signum.raw() as u32,
        si_code: SI_USER,
        si_pid: ctx.process.pid.0,
        si_uid: 0,
    });
    dispatch_errno(
        tx_subsystems::signal::script_deliver_signal(
            &ctx.process,
            SignalTarget::Process(target),
            signum,
            siginfo,
        ),
        |outcome| match outcome {
            KillOutcome::Delivered => SyscallResult::Return(0),
            KillOutcome::NoLiveThread => SyscallResult::Error(ESRCH_VALUE),
        },
    )
}

/// `tkill(tid, sig)` — Linux RV64 generic ABI `__NR_tkill = 130`.
///
/// Delivers directly to the named thread. This matters for musl
/// pthread cancellation: the cancel handler may re-send SIGCANCEL to
/// `self->tid`, and process-directed routing can choose the wrong
/// thread and leave the target blocked.
pub(super) fn sys_tkill(args: [u64; 6], ctx: &SyscallCtx) -> SyscallResult {
    let tid = args[0];
    let sig = args[1] as u32;

    if sig > 64 {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    // Resolve tid → ThreadIdentity via PidName namespace
    if let Some(PidName::Thread(thread_cap)) = resolve_pid_number_as(tid, PidNameKind::Thread) {
        if sig == 0 {
            return SyscallResult::Return(0);
        }
        let signum = match u8::try_from(sig).ok().and_then(Signum::new) {
            Some(s) => s,
            None => return SyscallResult::Error(EINVAL_VALUE),
        };

        #[cfg(target_arch = "loongarch64")]
        if signum.raw() == MUSL_SIGCANCEL {
            if let Some(proc_cap) = thread_cap.upgrade_owner_proc() {
                // LA64 signal-frame delivery for musl's private
                // SIGCANCEL path is not ABI-complete yet. Until the
                // handler path is fixed, fail the current libctest
                // child fast instead of leaving pthread_join blocked
                // forever on the target thread's clear_child_tid futex.
                tx_subsystems::process::execution::step_exit_group_with_signal(&proc_cap, signum);
                return SyscallResult::Return(0);
            }
            return SyscallResult::Error(ESRCH_VALUE);
        }

        let siginfo = Some(SigInfo {
            si_signo: signum.raw() as u32,
            si_code: SI_TKILL,
            si_pid: ctx.process.pid.0,
            si_uid: 0,
        });
        return match tx_subsystems::signal::script_deliver_signal(
            &ctx.process,
            SignalTarget::Thread(thread_cap),
            signum,
            siginfo,
        ) {
            Ok(tx_subsystems::signal::KillOutcome::Delivered) => SyscallResult::Return(0),
            Ok(tx_subsystems::signal::KillOutcome::NoLiveThread) => {
                SyscallResult::Error(ESRCH_VALUE)
            }
            Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
        };
    }

    SyscallResult::Error(ESRCH_VALUE)
}

/// `tgkill(tgid, tid, sig)` — Linux RV64 generic ABI
/// `__NR_tgkill = 131`.
///
/// Phase 4: validates tgid matches the caller's pid, then posts
/// directly to the target thread. Falls back to `sys_kill` for
/// single-threaded compatibility.
pub(super) fn sys_tgkill(args: [u64; 6], ctx: &SyscallCtx) -> SyscallResult {
    let tgid = args[0] as u32;
    let tid = args[1] as u32;
    let sig = args[2] as u32;
    // Validate tgid. v1 limitation: tgid must match the caller's pid
    // (caller may only tgkill threads in its own thread group). This
    // is what makes the cred check below trivially self-permitted —
    // when cross-process tgkill lands the dispatch must route through
    // `script_deliver_signal` (cred-checked) the way sys_tkill does.
    if tgid != ctx.process.pid.0 {
        return SyscallResult::Error(ESRCH_VALUE);
    }
    if sig == 0 {
        return SyscallResult::Return(0);
    }
    let signum = match u8::try_from(sig).ok().and_then(Signum::new) {
        Some(s) => s,
        None => return SyscallResult::Error(EINVAL_VALUE),
    };
    if let Some(thread) = ctx.process.thread_by_tid(tid) {
        #[cfg(target_arch = "loongarch64")]
        if signum.raw() == MUSL_SIGCANCEL {
            // Same LA64 SIGCANCEL fail-fast as sys_tkill above. The
            // tgid check already proved this targets the caller's
            // thread group, so terminate that test child process.
            tx_subsystems::process::execution::step_exit_group_with_signal(&ctx.process, signum);
            return SyscallResult::Return(0);
        }

        let siginfo = Some(SigInfo {
            si_signo: signum.raw() as u32,
            si_code: SI_TKILL,
            si_pid: ctx.process.pid.0,
            si_uid: 0,
        });
        let mut script_ctx = build_subject_script_ctx(ctx);
        let mut op = ThreadKillOp {
            thread,
            sig: signum,
            info: siginfo,
        };
        match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
            Ok(()) => return SyscallResult::Return(0),
            Err(v3errno) => return SyscallResult::error_from(v3errno),
        }
    }
    SyscallResult::Error(ESRCH_VALUE)
}

/// `rt_sigreturn(...)` — Linux RV64 generic ABI
/// `__NR_rt_sigreturn = 139`.
///
/// Restore the pre-handler trap context that signal delivery parked
/// in `payload.saved_signal_context` before flipping
/// `saved_user_context` to the handler-entry context. After the
/// handler `ret`s through the stack trampoline (`addi a7, 0, 139;
/// ecall`), this syscall fires; we move the parked context back into
/// `saved_user_context` so the thread re-enters userspace exactly
/// where the signal interrupted it.
///
/// `SigreturnRestored` tells the syscall-return path in
/// `thread_future` to decode the platform signal frame before
/// re-entering userspace. `SigreturnContextRestored` is reserved for
/// the small compatibility frame emitted by `maybe_deliver_itimer_signal`,
/// where this syscall arm has already restored the context.
///
/// Without this restore, the handler's "post-`ret`" path stays in
/// the trampoline / signal-frame memory and the thread reads garbage
/// off the signal stack — observed end-to-end as the busybox-sh
/// SIGCHLD-handler crash on 2026-05-18 (root cause traced through
/// the trap-trace serial log).
///
/// If no signal frame is in flight, the kernel has no parked
/// context to restore. POSIX leaves this case undefined; we return
/// `-EFAULT` defensively rather than corrupt the current context.
///
/// N69b also keeps the older itimer compatibility frame path alive:
/// `maybe_deliver_itimer_signal` writes a small RV64 frame directly
/// on the user stack, so when no parked context exists we restore from
/// that frame as a fallback. This keeps netperf's SIGALRM completion
/// path working while the generic `SignalFrameIf` delivery path is
/// the primary signal route.
pub(super) fn sys_rt_sigreturn(ctx: &SyscallCtx) -> SyscallResult {
    let Some(payload) = ctx.thread.payload_cap() else {
        return SyscallResult::Error(EFAULT_VALUE);
    };

    let Some(current) = payload.saved_user_context() else {
        return SyscallResult::Error(EFAULT_VALUE);
    };
    if let Ok(frame) = read_compat_signal_frame(&ctx.aspace, current.regs[2] as u64) {
        let mut script_ctx = build_subject_script_ctx(ctx);
        let mut op = SigprocmaskOp {
            thread: ctx.thread.clone(),
            how: SigmaskHow::SetMask,
            next: SignalMask::new(frame.saved_mask.bits),
        };
        let _ = step_engine::drive_oneshot(&mut op, &mut script_ctx);
        let _ = payload.take_saved_signal_context();
        payload.store_saved_user_context(Some(frame.user_context));
        return SyscallResult::SigreturnContextRestored;
    }

    if let Some(saved) = payload.take_saved_signal_context() {
        if let Some(mask) = payload.take_saved_signal_mask() {
            payload.store_signal_mask(mask);
        }
        payload.store_saved_user_context(Some(saved));
        return SyscallResult::SigreturnRestored;
    }

    let frame = match read_compat_signal_frame(&ctx.aspace, current.regs[2] as u64) {
        Ok(frame) => frame,
        Err(errno) => return SyscallResult::Error(errno),
    };
    let mut script_ctx = build_subject_script_ctx(ctx);
    let mut op = SigprocmaskOp {
        thread: ctx.thread.clone(),
        how: SigmaskHow::SetMask,
        next: SignalMask::new(frame.saved_mask.bits),
    };
    let _ = step_engine::drive_oneshot(&mut op, &mut script_ctx);
    payload.store_saved_user_context(Some(frame.user_context));
    SyscallResult::SigreturnContextRestored
}

/// `rt_sigpending(set, sigsetsize)` — Linux RV64 generic ABI
/// `__NR_rt_sigpending = 136`.
///
/// Phase F: reads the calling thread's pending signal bitset
/// (thread_pending merged with the owning process's group_pending)
/// and writes it to `set`.
pub(super) fn sys_rt_sigpending(args: [u64; 6], ctx: &SyscallCtx) -> SyscallResult {
    let set_ptr = args[0] as usize;
    let sigsetsize = args[1];

    if sigsetsize != SIGSETSIZE_BYTES {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    // TODO(merge-fixup): upgrade_operational removed from Cap<ThreadIdentity>;
    // stub with empty pending set until thread payload accessor lands.
    let pending: u64 = 0;
    if let Err(errno) = bootstrap_write_user::<u64>(&ctx.aspace, set_ptr as u64, pending) {
        return SyscallResult::error_from(errno);
    }

    SyscallResult::Return(0)
}
