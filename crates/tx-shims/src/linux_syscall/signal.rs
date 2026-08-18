//! Auto-extracted from `mod.rs` during the 2026-05-08 jumbo-mod
//! split. The implementations here are unchanged; only the
//! enclosing module changed. Helpers and constants used here live
//! either in this submodule or in the shared parent (`super::*`).

use super::*;
#[cfg(tx_sigprocmask_detail_metrics)]
use core::sync::atomic::{AtomicU64, Ordering};
use tx_services::time::{timekeeper_clock, ClockRead, DeadlineRegistrar, TimekeeperClock};
use tx_substrate::verbs::OperationalCapExt;
use tx_subsystems::process::numbers::{resolve_pid_number_as, PidName, PidNameKind};
use tx_subsystems::signal::{step_kill_pgrp, SigInfo, SI_USER};
use tx_subsystems::signal::{KillOutcome, SignalTarget};

#[cfg(target_arch = "loongarch64")]
const MUSL_SIGCANCEL: u8 = 33;
const SI_TKILL: i32 = -6;
#[cfg(tx_sigprocmask_detail_metrics)]
static SIGPROCMASK_TRACE_SAMPLE: AtomicU64 = AtomicU64::new(0);

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

/// Drain `SignalDelivered` events from a thread mailbox.
///
/// The `tx-scripts::drive` inner mailbox-await re-posts any
/// `SignalDelivered` event it consumes so the calling syscall's
/// outer loop can re-observe the signal. For `sys_rt_sigtimedwait`,
/// we consume signals directly from `payload.pending()` — the
/// re-posted mailbox event is stale information that would otherwise
/// trap every subsequent `drive(NanosleepOp)` call into returning
/// immediately, degenerating the 5 ms poll cadence to a no-op spin.
/// This helper drains every queued `SignalDelivered` (preserving any
/// other events like `TimerFired` or `SourceFired` that are still
/// genuinely informative for the next park).
fn drain_stale_signal_events(mailbox: &crate::adapter::reactor_entry::TaskMailbox) {
    use crate::adapter::reactor_entry::MailboxEvent;
    let mut keep = alloc::vec::Vec::new();
    while let Some(event) = mailbox.poll() {
        if !matches!(event, MailboxEvent::SignalDelivered { .. }) {
            keep.push(event);
        }
    }
    // Any dropped event has already published its authoritative signal/source
    // state. Clear the hint latch so a later unrelated wait does not spin; the
    // syscall paths re-observe the authoritative state before parking.
    let _ = mailbox.take_overflow();
    for event in keep {
        let _ = mailbox.post(event);
    }
}

#[cfg(tx_sigprocmask_detail_metrics)]
fn sigprocmask_trace_sample() -> Option<i64> {
    let seq = SIGPROCMASK_TRACE_SAMPLE.fetch_add(1, Ordering::Relaxed);
    (seq < 64 || seq.is_power_of_two()).then_some(seq as i64)
}

#[cfg(not(tx_sigprocmask_detail_metrics))]
fn sigprocmask_trace_sample() -> Option<i64> {
    None
}

#[cfg(tx_sigprocmask_detail_metrics)]
fn emit_sigprocmask_debug(name: &[u8], value: i64) {
    if let Some(observer) = tx_observe::current() {
        observer.counter(
            tx_observe::EventNameId::from_raw(tx_observe::fnv1a32(name)),
            value,
        );
        tx_observe::dump_registered_if_requested();
    }
}

#[cfg(not(tx_sigprocmask_detail_metrics))]
fn emit_sigprocmask_debug(_name: &[u8], _value: i64) {}

#[cfg(tx_sigprocmask_detail_metrics)]
fn sigprocmask_detail_now() -> u64 {
    tx_observe::clock_now_ns()
}

#[cfg(not(tx_sigprocmask_detail_metrics))]
fn sigprocmask_detail_now() -> u64 {
    0
}

#[cfg(tx_sigprocmask_detail_metrics)]
fn emit_sigprocmask_detail_duration(name: &[u8], start_ns: u64) {
    if let Some(observer) = tx_observe::current() {
        let dur = tx_observe::clock_now_ns().saturating_sub(start_ns);
        observer.counter(
            tx_observe::EventNameId::from_raw(tx_observe::fnv1a32(name)),
            dur.min(i64::MAX as u64) as i64,
        );
        tx_observe::dump_registered_if_requested();
    }
}

#[cfg(not(tx_sigprocmask_detail_metrics))]
fn emit_sigprocmask_detail_duration(_name: &[u8], _start_ns: u64) {}

#[cfg(tx_sigprocmask_detail_metrics)]
fn emit_sigprocmask_detail_value(name: &[u8], value: i64) {
    emit_sigprocmask_debug(name, value);
}

#[cfg(not(tx_sigprocmask_detail_metrics))]
fn emit_sigprocmask_detail_value(_name: &[u8], _value: i64) {}

/// `rt_sigprocmask(how, set, oldset, sigsetsize)` per `SIGNAL_v1` §3.
///
/// The generic ABI uses an 8-byte signal set. LoongArch old-world userspace
/// uses a 16-byte set; txKernel consumes the full image while retaining only
/// its supported low 64 signals. `set_ptr == 0` means "query only";
/// `oldset_ptr == 0` means "don't return the previous mask".
pub(super) async fn sys_rt_sigprocmask(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    sys_rt_sigprocmask_wait_impl(args, &ctx.thread, None, &ctx.aspace).await
}

pub(super) fn sys_rt_sigprocmask_oneshot(
    args: [u64; 6],
    ctx: &SyscallCtx<'_>,
) -> Option<SyscallResult> {
    sys_rt_sigprocmask_oneshot_impl(args, &ctx.thread, None, &ctx.aspace)
}

pub(super) fn sys_rt_sigprocmask_thread_aspace(
    args: [u64; 6],
    thread: &Cap<ThreadIdentity>,
    aspace: &Cap<AddressSpace>,
) -> Option<SyscallResult> {
    sys_rt_sigprocmask_oneshot_impl(args, thread, None, aspace)
}

pub(super) fn sys_rt_sigprocmask_thread_payload_aspace(
    args: [u64; 6],
    thread: &Cap<ThreadIdentity>,
    payload: &PayloadCap<ThreadPayload>,
    aspace: &Cap<AddressSpace>,
) -> Option<SyscallResult> {
    sys_rt_sigprocmask_oneshot_impl(args, thread, Some(payload), aspace)
}

fn decode_rt_sigprocmask_args(
    args: [u64; 6],
) -> Result<(RtSignalSetAbi, Option<SigmaskHow>, u64, u64), SyscallResult> {
    let set_ptr = args[1];
    let oldset_ptr = args[2];
    let wire_abi = classify_rt_signal_set_abi(args[3], CURRENT_RT_SIGNAL_TARGET_ABI)
        .ok_or(SyscallResult::Error(EINVAL_VALUE))?;
    let how = match (args[0] as i32, set_ptr) {
        (_, 0) => None,
        (0, _) => Some(SigmaskHow::Block),
        (1, _) => Some(SigmaskHow::Unblock),
        (2, _) => Some(SigmaskHow::SetMask),
        _ => return Err(SyscallResult::Error(EINVAL_VALUE)),
    };
    Ok((wire_abi, how, set_ptr, oldset_ptr))
}

fn current_thread_signal_mask(
    thread: &Cap<ThreadIdentity>,
    payload: Option<&PayloadCap<ThreadPayload>>,
) -> Result<SignalMask, SyscallResult> {
    if let Some(payload) = payload {
        return Ok(payload.signal_mask());
    }
    thread
        .payload_cap()
        .map(|payload| payload.signal_mask())
        .ok_or(SyscallResult::Error(ESRCH_VALUE))
}

fn commit_rt_sigprocmask(
    thread: &Cap<ThreadIdentity>,
    payload: Option<&PayloadCap<ThreadPayload>>,
    how: Option<SigmaskHow>,
    next_mask: SignalMask,
) -> SyscallResult {
    let Some(how) = how else {
        return SyscallResult::Return(0);
    };
    match payload.map_or_else(
        || step_sigprocmask(thread, how, next_mask),
        |payload| step_sigprocmask_with_payload(thread, payload, how, next_mask),
    ) {
        SigprocmaskChange::Replaced { .. } => SyscallResult::Return(0),
        SigprocmaskChange::ZombieIgnored => SyscallResult::Error(ESRCH_VALUE),
    }
}

fn sys_rt_sigprocmask_oneshot_impl(
    args: [u64; 6],
    thread: &Cap<ThreadIdentity>,
    payload: Option<&PayloadCap<ThreadPayload>>,
    aspace: &Cap<AddressSpace>,
) -> Option<SyscallResult> {
    let (wire_abi, how, set_ptr, oldset_ptr) = match decode_rt_sigprocmask_args(args) {
        Ok(decoded) => decoded,
        Err(result) => return Some(result),
    };
    let next_mask = if set_ptr == 0 {
        SignalMask::EMPTY
    } else {
        match read_rt_signal_set_oneshot(aspace, set_ptr, wire_abi)? {
            Ok(bits) => SignalMask::new(bits),
            Err(errno) => return Some(SyscallResult::error_from(errno)),
        }
    };
    let prev_mask = match current_thread_signal_mask(thread, payload) {
        Ok(mask) => mask,
        Err(result) => return Some(result),
    };

    // Complete every fallible userspace operation before changing the mask.
    // A RangeLock collision can therefore defer the syscall without leaving
    // a new mask installed or exposing EIO to pthread_create/join.
    if oldset_ptr != 0 {
        match write_rt_signal_set_oneshot(aspace, oldset_ptr, wire_abi, prev_mask.raw_bits())? {
            Ok(()) => {}
            Err(errno) => return Some(SyscallResult::error_from(errno)),
        }
    }
    Some(commit_rt_sigprocmask(thread, payload, how, next_mask))
}

async fn sys_rt_sigprocmask_wait_impl(
    args: [u64; 6],
    thread: &Cap<ThreadIdentity>,
    payload: Option<&PayloadCap<ThreadPayload>>,
    aspace: &Cap<AddressSpace>,
) -> SyscallResult {
    let (wire_abi, how, set_ptr, oldset_ptr) = match decode_rt_sigprocmask_args(args) {
        Ok(decoded) => decoded,
        Err(result) => return result,
    };
    let next_mask = if set_ptr == 0 {
        SignalMask::EMPTY
    } else {
        match read_rt_signal_set_wait(aspace, set_ptr, wire_abi).await {
            Ok(bits) => SignalMask::new(bits),
            Err(errno) => return SyscallResult::error_from(errno),
        }
    };
    let prev_mask = match current_thread_signal_mask(thread, payload) {
        Ok(mask) => mask,
        Err(result) => return result,
    };
    if oldset_ptr != 0 {
        if let Err(errno) =
            write_rt_signal_set_wait(aspace, oldset_ptr, wire_abi, prev_mask.raw_bits()).await
        {
            return SyscallResult::error_from(errno);
        }
    }
    commit_rt_sigprocmask(thread, payload, how, next_mask)
}

fn set_thread_signal_mask(
    ctx: &SyscallCtx<'_>,
    next: SignalMask,
) -> Result<SignalMask, SyscallResult> {
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

pub(super) async fn sys_rt_sigsuspend<P>(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult
where
    TimekeeperClock<P>: ClockRead,
{
    let mask_ptr = args[0];
    let sigset_size = args[1];
    if mask_ptr == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }
    let Some(wire_abi) = classify_rt_signal_set_abi(sigset_size, CURRENT_RT_SIGNAL_TARGET_ABI)
    else {
        return SyscallResult::Error(EINVAL_VALUE);
    };

    let mask_bits = match read_rt_signal_set_wait(&ctx.aspace, mask_ptr, wire_abi).await {
        Ok(bits) => bits,
        Err(errno) => return SyscallResult::error_from(errno),
    };
    let old_mask = match set_thread_signal_mask(ctx, SignalMask::new(mask_bits)) {
        Ok(mask) => mask,
        Err(result) => return result,
    };
    let Some(thread_payload) = ctx.thread.payload_cap() else {
        let _ = set_thread_signal_mask(ctx, old_mask);
        return SyscallResult::Error(ESRCH_VALUE);
    };
    thread_payload.store_sigsuspend_restore_mask(Some(old_mask));

    // Return EINTR for an interrupting signal WITHOUT restoring the pre-suspend
    // mask first. POSIX sigsuspend runs the signal's handler *while the suspend
    // mask is active* (the suspend mask is what unblocks the awaited signal; the
    // caller's normal mask typically blocks it, e.g. a shell blocks SIGCHLD then
    // sigsuspends with it unblocked to wait for a child). run_thread's AST
    // checkpoint delivers the pending handler immediately after this syscall
    // returns — so the mask in effect at that point must still unblock the
    // signal. Restoring the (blocking) mask here left the handler permanently
    // undeliverable: the signal stayed pending, every re-`sigsuspend` saw it
    // again, and the caller span forever (the flaky fs_bind* hang, root-caused
    // by gdb to a busy rt_sigsuspend loop). The pre-suspend mask is re-applied
    // by the delivered handler's `sigreturn` frame. The thread payload carries
    // `old_mask` separately so the handler itself still runs under the
    // temporary suspend mask.
    let return_eintr = || SyscallResult::Error(EINTR_VALUE);

    const CHUNK_NS: u64 = 5_000_000;
    loop {
        // Both POSIX timers and setitimer timers may terminate sigsuspend.
        // Register their deadlines through the shared service and route timer
        // wakeups through the task's owner-aware mailbox.
        let timer_registrar = ctx.timer_registrar.as_ref().cloned();
        let timer_mailbox = ctx.mailbox.as_ref().map(alloc::sync::Arc::downgrade);
        let registrar = timer_registrar
            .as_ref()
            .map(|registrar| registrar as &dyn DeadlineRegistrar);
        let posix_deadline = poll_due_posix_timers_with_post::<P, _>(
            &ctx.process,
            registrar,
            timer_mailbox.clone(),
            |mailbox, event| ctx.post_mailbox_event(mailbox, event),
        );
        let itimer_deadline = poll_due_itimers_with_post::<P, _>(
            &ctx.process,
            registrar,
            timer_mailbox,
            |mailbox, event| ctx.post_mailbox_event(mailbox, event),
        );
        let timer_deadline = match (posix_deadline, itimer_deadline) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (Some(deadline), None) | (None, Some(deadline)) => Some(deadline),
            (None, None) => None,
        };
        // Interrupt only on a signal that POSIX says should break a blocking
        // syscall — NOT on benign pending signals such as SIGCHLD (default
        // action Ignore), which the shell accrues while reaping the children
        // it forks (e.g. the `$(sed | awk)` in fs_bind's cleanup loop). Using
        // the broad `select_next_signal(...).is_some()` here made every such
        // SIGCHLD spuriously return EINTR; the shell then re-`sigsuspend`ed,
        // saw the same still-pending SIGCHLD, and span forever — the flaky
        // fs_bind* hang. This is the rt_sigsuspend counterpart of the
        // ppoll/pselect/recv/send/wait4 fix that already switched to
        // `thread_pending_signal_interrupts` (it broke netperf identically).
        if tx_subsystems::signal::thread_pending_signal_ends_sigsuspend(&ctx.thread) {
            return return_eintr();
        }

        use crate::adapter::step_engine::DriveMode;
        use tx_scripts::drive;
        let now_ns = timekeeper_clock::<P>().monotonic_now_ns();
        let chunk_deadline_ns = now_ns.saturating_add(CHUNK_NS);
        let park_deadline_ns = timer_deadline
            .map(|deadline_ns| deadline_ns.min(chunk_deadline_ns))
            .unwrap_or(chunk_deadline_ns);
        let mut script_ctx = build_subject_script_ctx(ctx);
        let timer_registrar_handle = script_ctx.timer_registrar().cloned();
        let mailbox = ctx.mailbox.clone();
        let op = NanosleepOp {
            nanos: park_deadline_ns.saturating_sub(now_ns),
            deadline_ns: park_deadline_ns,
            started: false,
        };
        match drive(
            op,
            &mut script_ctx,
            DriveMode::Waiting,
            mailbox.as_ref(),
            None,
            timer_registrar_handle.as_ref(),
        )
        .await
        {
            Ok(()) => {}
            Err(v3errno) => {
                let errno: Errno = v3errno.into();
                if errno == Errno::EINTR {
                    if tx_subsystems::signal::thread_pending_signal_ends_sigsuspend(&ctx.thread) {
                        return return_eintr();
                    }
                }
                let _ = set_thread_signal_mask(ctx, old_mask);
                thread_payload.store_sigsuspend_restore_mask(None);
                return SyscallResult::error_from(errno);
            }
        }
    }
}

/// `sigaltstack(ss, old_ss)` — Linux LP64 `stack_t` query/update.
///
/// This records the registered alternate stack in
/// `ThreadPayload.alt_stack`. Signal delivery uses it for `SA_ONSTACK` and
/// nested delivery continues below the current SP instead of overwriting the
/// outer frame.
pub(super) async fn sys_sigaltstack(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let ss_ptr = args[0];
    let old_ss_ptr = args[1];
    let Some(payload) = ctx.thread.payload_cap() else {
        return SyscallResult::Error(ESRCH_VALUE);
    };

    let current_sp = payload
        .saved_user_context()
        .map(|saved| {
            #[cfg(target_arch = "loongarch64")]
            {
                saved.regs[3]
            }
            #[cfg(not(target_arch = "loongarch64"))]
            {
                saved.regs[2]
            }
        })
        .unwrap_or(0);
    let on_altstack = payload.alt_stack().is_some_and(|(base, size)| {
        base.checked_add(size)
            .is_some_and(|end| (base..end).contains(&current_sp))
    });

    let new_stack = if ss_ptr != 0 {
        // Linux forbids replacing/disabling the alternate stack while the
        // caller is executing on it.
        if on_altstack {
            return SyscallResult::Error(EPERM_VALUE);
        }
        let new = match bootstrap_read_user_wait::<SigaltstackLayout>(&ctx.aspace, ss_ptr).await {
            Ok(value) => value,
            Err(errno) => return SyscallResult::error_from(errno),
        };
        let allowed = SS_DISABLE | SS_AUTODISARM;
        if new.ss_flags & !allowed != 0 {
            return SyscallResult::Error(EINVAL_VALUE);
        }
        if new.ss_flags & SS_DISABLE == 0 && new.ss_size < MINSIGSTKSZ {
            return SyscallResult::Error(ENOMEM_VALUE);
        }
        Some(new)
    } else {
        None
    };

    if old_ss_ptr != 0 {
        let (ss_sp, ss_flags, ss_size) = match payload.alt_stack() {
            Some((base, size)) => (
                base as u64,
                if on_altstack { SS_ONSTACK } else { 0 },
                size as u64,
            ),
            None => (0, SS_DISABLE, 0),
        };
        let old = SigaltstackLayout {
            ss_sp,
            ss_flags,
            _pad: 0,
            ss_size,
        };
        if let Err(errno) =
            bootstrap_write_user_wait::<SigaltstackLayout>(&ctx.aspace, old_ss_ptr, old).await
        {
            return SyscallResult::error_from(errno);
        }
    }

    // All fallible user copies are complete before the per-thread state is
    // changed, so a VM wait or bad output pointer cannot leave a partial
    // sigaltstack update behind.
    if let Some(new) = new_stack {
        if new.ss_flags & SS_DISABLE != 0 {
            payload.set_alt_stack(None);
        } else {
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

fn has_matching_pending_signal(ctx: &SyscallCtx<'_>, wait_bits: u64) -> bool {
    let thread_match = ctx
        .thread
        .payload_cap()
        .map(|payload| payload.pending().snapshot() & wait_bits)
        .unwrap_or(0);
    if thread_match != 0 {
        return true;
    }

    ctx.process
        .upgrade_operational()
        .map(|payload| payload.group_pending().snapshot() & wait_bits != 0)
        .unwrap_or(false)
}

async fn write_sigtimedwait_siginfo(
    ctx: &SyscallCtx<'_>,
    info_ptr: u64,
    sig: Signum,
) -> Result<(), i32> {
    if info_ptr == 0 {
        return Ok(());
    }

    let mut image = [0u8; 128];
    image[0..4].copy_from_slice(&(sig.raw() as u32).to_le_bytes());
    image[8..12].copy_from_slice(&0i32.to_le_bytes());
    bootstrap_copy_to_user_wait(&ctx.aspace, info_ptr, &image)
        .await
        .map_err(errno_to_i32)
}

async fn park_sigtimedwait_tick<P: tx_hal::TimeIf>(
    ctx: &SyscallCtx<'_>,
    wait_bits: u64,
    deadline_ns: Option<u64>,
) where
    TimekeeperClock<P>: ClockRead,
{
    const SIGTIMEDWAIT_POLL_NS: u64 = 1_000_000;

    if wait_bits & Signum::SIGCHLD.bit() != 0 && deadline_ns.is_none() {
        if let Some(source) = ctx.process.exit_wait_source() {
            // The legacy exit Channel is edge-triggered: a child can exit
            // after the pending-signal check above but before the Channel
            // future installs its subscription, permanently losing the only
            // wake.  Install against the process's v3 WaitSource and recheck
            // the authoritative pending queues under the same publication
            // lock used by child-exit notification.
            let parked = super::await_wait_source_if(
                ctx,
                source.id(),
                crate::adapter::step_engine::InterestMask::new(
                    tx_subsystems::process::EXIT_SOURCE_CHILD_ZOMBIFIED,
                ),
                || !has_matching_pending_signal(ctx, wait_bits),
            )
            .await;
            if parked || has_matching_pending_signal(ctx, wait_bits) {
                return;
            }
        }
    }

    let now = timekeeper_clock::<P>().monotonic_now_ns();
    let next = match deadline_ns {
        Some(deadline) => core::cmp::min(deadline, now.saturating_add(SIGTIMEDWAIT_POLL_NS)),
        None => now.saturating_add(SIGTIMEDWAIT_POLL_NS),
    };
    if let Some(future) = super::deadline_timer(ctx, next) {
        future.await;
    } else {
        tx_reactor::yield_now().await;
    }
}

/// `rt_sigtimedwait(set, info, timeout, sigsetsize)` — Linux LP64 signal ABI.
///
/// Consumes a pending signal named by `set`, optionally waits until
/// `timeout` expires, and writes the leading Linux `siginfo_t` fields.
/// Signal-mask interaction is intentionally different from normal
/// delivery: `sigtimedwait(2)` observes pending signals in `set` even
/// when they are blocked, which is how runtest waits for child SIGCHLD.
pub(super) async fn sys_rt_sigtimedwait<'a, P: tx_hal::TimeIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'a>,
) -> SyscallResult
where
    TimekeeperClock<P>: ClockRead,
{
    let set_ptr = args[0];
    let info_ptr = args[1];
    let timeout_ptr = args[2];
    let sigsetsize = args[3];

    let Some(wire_abi) = classify_rt_signal_set_abi(sigsetsize, CURRENT_RT_SIGNAL_TARGET_ABI)
    else {
        return SyscallResult::Error(EINVAL_VALUE);
    };
    if set_ptr == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }

    let wait_bits = match read_rt_signal_set_wait(&ctx.aspace, set_ptr, wire_abi).await {
        Ok(bits) => bits,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };
    let timeout_ns = if timeout_ptr == 0 {
        None
    } else {
        match read_timespec_at_wait(&ctx.aspace, timeout_ptr).await {
            Ok(ns) => Some(ns),
            Err(errno) => return SyscallResult::error_from(errno),
        }
    };
    let deadline_ns = timeout_ns.map(|ns| {
        timekeeper_clock::<P>()
            .monotonic_now_ns()
            .saturating_add(ns)
    });

    // `await_mailbox_event` (in `tx-scripts::drive::resolve_on_timer`)
    // re-posts any SignalDelivered event it consumes. sigtimedwait
    // consumes pending bits directly, so stale signal events would make
    // the timer wait return immediately and spin; drain them around
    // the polling loop.
    let mailbox_for_drain = ctx.mailbox.clone();
    if let Some(ref mbox) = mailbox_for_drain {
        drain_stale_signal_events(mbox);
    }
    loop {
        if let Some(sig) = take_matching_pending_signal(ctx, wait_bits) {
            if let Err(errno) = write_sigtimedwait_siginfo(ctx, info_ptr, sig).await {
                return SyscallResult::Error(errno);
            }
            return SyscallResult::Return(sig.raw() as i64);
        }

        match deadline_ns {
            Some(deadline) if timekeeper_clock::<P>().monotonic_now_ns() >= deadline => {
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
    if flags & !O_NONBLOCK != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    let target = match resolve_pid_number_as(pid_raw, PidNameKind::Process) {
        Some(PidName::Process(process)) => process,
        _ => return SyscallResult::Error(ESRCH_VALUE),
    };

    let open_flags = OpenFileFlags {
        read: true,
        write: false,
        append: false,
        cloexec: true,
        nonblocking: (flags & O_NONBLOCK) != 0,
        packet: false,
    };
    let open_cap = match OpenFile::new_pidfd_cap(target, open_flags) {
        Ok(cap) => cap,
        Err(_) => return SyscallResult::Error(ENOMEM_VALUE),
    };
    let Some(fd) = ctx.process.install_new_fd(open_cap, true) else {
        return SyscallResult::Error(EMFILE_VALUE);
    };
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

pub(super) async fn sys_pidfd_send_signal(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
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
        if let Err(errno) =
            bootstrap_copy_from_user_wait(&ctx.aspace, &mut signo_bytes, info_ptr).await
        {
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
        tx_subsystems::signal::script_deliver_signal_with_posts(
            &ctx.process,
            SignalTarget::Process(target),
            signum,
            siginfo,
            |mailbox, event| ctx.post_mailbox_event(mailbox, event),
            |mailbox, event| ctx.post_mailbox_ref_event(mailbox, event),
        ),
        |outcome| match outcome {
            KillOutcome::Delivered => SyscallResult::Return(0),
            KillOutcome::NoLiveThread => SyscallResult::Error(ESRCH_VALUE),
            KillOutcome::Retry => SyscallResult::Error(EAGAIN_VALUE),
        },
    )
}

const LA64_OLDWORLD_SIGSETSIZE_BYTES: u64 = 16;
const LA64_OLDWORLD_SIGACTION_BYTES: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RtSignalTargetAbi {
    Generic64,
    LoongArch64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RtSignalSetAbi {
    Generic64,
    LoongArchOldWorld,
}

impl RtSignalSetAbi {
    const fn sigset_bytes(self) -> usize {
        match self {
            Self::Generic64 => SIGSETSIZE_BYTES as usize,
            Self::LoongArchOldWorld => LA64_OLDWORLD_SIGSETSIZE_BYTES as usize,
        }
    }

    const fn action_bytes(self) -> usize {
        match self {
            Self::Generic64 => SIGACTION_BYTES,
            Self::LoongArchOldWorld => 16 + self.sigset_bytes(),
        }
    }
}

const CURRENT_RT_SIGNAL_TARGET_ABI: RtSignalTargetAbi = if cfg!(target_arch = "loongarch64") {
    RtSignalTargetAbi::LoongArch64
} else {
    RtSignalTargetAbi::Generic64
};

const fn classify_rt_signal_set_abi(
    sigsetsize: u64,
    target: RtSignalTargetAbi,
) -> Option<RtSignalSetAbi> {
    match (target, sigsetsize) {
        (_, SIGSETSIZE_BYTES) => Some(RtSignalSetAbi::Generic64),
        (RtSignalTargetAbi::LoongArch64, LA64_OLDWORLD_SIGSETSIZE_BYTES) => {
            Some(RtSignalSetAbi::LoongArchOldWorld)
        }
        _ => None,
    }
}

fn decode_rt_signal_set(wire_abi: RtSignalSetAbi, image: &[u8]) -> u64 {
    debug_assert!(image.len() >= wire_abi.sigset_bytes());
    let low = read_u64_le(&image[0..8]);
    if wire_abi == RtSignalSetAbi::LoongArchOldWorld {
        // Old-world LA64 defines signals 1..=128. Consume the second word at
        // the ABI boundary even though txKernel currently models only 1..=64.
        let _unsupported_high = read_u64_le(&image[8..16]);
    }
    low
}

fn encode_rt_signal_set(wire_abi: RtSignalSetAbi, low: u64, image: &mut [u8]) {
    debug_assert!(image.len() >= wire_abi.sigset_bytes());
    image[..wire_abi.sigset_bytes()].fill(0);
    image[0..8].copy_from_slice(&low.to_le_bytes());
}

fn read_rt_signal_set(
    aspace: &AddressSpace,
    user_ptr: u64,
    wire_abi: RtSignalSetAbi,
) -> Result<u64, Errno> {
    let mut image = [0u8; LA64_OLDWORLD_SIGSETSIZE_BYTES as usize];
    bootstrap_copy_from_user(aspace, &mut image[..wire_abi.sigset_bytes()], user_ptr)?;
    Ok(decode_rt_signal_set(wire_abi, &image))
}

fn read_rt_signal_set_oneshot(
    aspace: &AddressSpace,
    user_ptr: u64,
    wire_abi: RtSignalSetAbi,
) -> Option<Result<u64, Errno>> {
    match wire_abi {
        RtSignalSetAbi::Generic64 => bootstrap_read_user_oneshot::<[u8; 8]>(aspace, user_ptr)
            .map(|result| result.map(|image| u64::from_le_bytes(image))),
        RtSignalSetAbi::LoongArchOldWorld => {
            bootstrap_read_user_oneshot::<[u8; 16]>(aspace, user_ptr).map(|result| {
                result.map(|image| {
                    let mut low = [0u8; 8];
                    low.copy_from_slice(&image[..8]);
                    u64::from_le_bytes(low)
                })
            })
        }
    }
}

async fn read_rt_signal_set_wait(
    aspace: &AddressSpace,
    user_ptr: u64,
    wire_abi: RtSignalSetAbi,
) -> Result<u64, Errno> {
    let mut image = [0u8; LA64_OLDWORLD_SIGSETSIZE_BYTES as usize];
    bootstrap_copy_from_user_wait(aspace, &mut image[..wire_abi.sigset_bytes()], user_ptr).await?;
    Ok(decode_rt_signal_set(wire_abi, &image))
}

fn write_rt_signal_set(
    aspace: &AddressSpace,
    user_ptr: u64,
    wire_abi: RtSignalSetAbi,
    low: u64,
) -> Result<(), Errno> {
    let mut image = [0u8; LA64_OLDWORLD_SIGSETSIZE_BYTES as usize];
    encode_rt_signal_set(wire_abi, low, &mut image);
    bootstrap_copy_to_user(aspace, user_ptr, &image[..wire_abi.sigset_bytes()])
}

fn write_rt_signal_set_oneshot(
    aspace: &AddressSpace,
    user_ptr: u64,
    wire_abi: RtSignalSetAbi,
    low: u64,
) -> Option<Result<(), Errno>> {
    match wire_abi {
        RtSignalSetAbi::Generic64 => {
            bootstrap_write_user_oneshot(aspace, user_ptr, low.to_le_bytes())
        }
        RtSignalSetAbi::LoongArchOldWorld => {
            let mut image = [0u8; 16];
            image[..8].copy_from_slice(&low.to_le_bytes());
            bootstrap_write_user_oneshot(aspace, user_ptr, image)
        }
    }
}

async fn write_rt_signal_set_wait(
    aspace: &AddressSpace,
    user_ptr: u64,
    wire_abi: RtSignalSetAbi,
    low: u64,
) -> Result<(), Errno> {
    let mut image = [0u8; LA64_OLDWORLD_SIGSETSIZE_BYTES as usize];
    encode_rt_signal_set(wire_abi, low, &mut image);
    bootstrap_copy_to_user_wait(aspace, user_ptr, &image[..wire_abi.sigset_bytes()]).await
}

fn decode_rt_sigaction_entry(wire_abi: RtSignalSetAbi, image: &[u8]) -> SigActionEntry {
    debug_assert!(image.len() >= wire_abi.action_bytes());
    let handler = read_u64_le(&image[0..8]);
    let flags = SaFlags::new(read_u64_le(&image[8..16]));
    let mask = SignalMask::new(decode_rt_signal_set(
        wire_abi,
        &image[16..wire_abi.action_bytes()],
    ));
    let disposition = match handler {
        0 => SigDisposition::Default,
        1 => SigDisposition::Ignore,
        other => SigDisposition::Handler(other as usize),
    };
    SigActionEntry::new(disposition, flags, mask, 0)
}

fn encode_rt_sigaction_entry(wire_abi: RtSignalSetAbi, entry: SigActionEntry, image: &mut [u8]) {
    debug_assert!(image.len() >= wire_abi.action_bytes());
    image[..wire_abi.action_bytes()].fill(0);
    let handler = match entry.disposition {
        SigDisposition::Default => 0,
        SigDisposition::Ignore => 1,
        SigDisposition::Handler(addr) => addr as u64,
    };
    image[0..8].copy_from_slice(&handler.to_le_bytes());
    image[8..16].copy_from_slice(&entry.flags.bits().to_le_bytes());
    encode_rt_signal_set(
        wire_abi,
        entry.sa_mask.raw_bits(),
        &mut image[16..wire_abi.action_bytes()],
    );
}

/// `rt_sigaction(signum, act, oldact, sigsetsize)` per `SIGNAL_v1`
/// §15.1.
///
/// The new-world generic ABI uses an 8-byte signal set and a 24-byte action
/// image (`handler`, `flags`, one mask word). LoongArch old-world userspace
/// uses a 16-byte signal set and a 32-byte action image with a second mask
/// word. `act_ptr == 0` queries the current disposition without changing it;
/// `oldact_ptr == 0` discards the previous disposition.
pub(super) async fn sys_rt_sigaction(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let signum_raw = args[0] as u32;
    let act_ptr = args[1] as usize;
    let oldact_ptr = args[2] as usize;
    let sigsetsize = args[3];

    let Some(wire_abi) = classify_rt_signal_set_abi(sigsetsize, CURRENT_RT_SIGNAL_TARGET_ABI)
    else {
        return SyscallResult::Error(EINVAL_VALUE);
    };

    let Some(sig) = (if signum_raw <= u8::MAX as u32 {
        Signum::new(signum_raw as u8)
    } else {
        None
    }) else {
        return SyscallResult::Error(EINVAL_VALUE);
    };

    // Decode the new action (if any) through the canonical user-VA
    // lane (`bootstrap_copy_from_user` bridges via
    // `aspace.copy_from_user`).
    let new_entry: Option<SigActionEntry> = if act_ptr == 0 {
        None
    } else {
        let mut bytes = [0u8; LA64_OLDWORLD_SIGACTION_BYTES];
        if let Err(errno) = bootstrap_copy_from_user_wait(
            &ctx.aspace,
            &mut bytes[..wire_abi.action_bytes()],
            act_ptr as u64,
        )
        .await
        {
            return SyscallResult::error_from(errno);
        }
        Some(decode_rt_sigaction_entry(wire_abi, &bytes))
    };

    // Snapshot and copy out the previous action before installing a new one.
    // The current thread is the syscall's sole continuation, so this ordering
    // avoids a partial state change when the output page has to wait.
    let prev_entry = match ctx.process.sig_action_entry(sig) {
        Some(entry) => entry,
        None => return SyscallResult::Error(ESRCH_VALUE),
    };

    if oldact_ptr != 0 {
        let mut image = [0u8; LA64_OLDWORLD_SIGACTION_BYTES];
        encode_rt_sigaction_entry(wire_abi, prev_entry, &mut image);
        if let Err(errno) = bootstrap_copy_to_user_wait(
            &ctx.aspace,
            oldact_ptr as u64,
            &image[..wire_abi.action_bytes()],
        )
        .await
        {
            return SyscallResult::error_from(errno);
        }
    }

    if let Some(entry) = new_entry {
        let mut script_ctx = build_subject_script_ctx(ctx);
        let mut op = SigactionOp {
            process: ctx.process.clone(),
            sig,
            entry,
        };
        match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
            Ok(SigDispositionChange::Replaced { .. })
            | Ok(SigDispositionChange::Uncatchable(_)) => {}
            Ok(SigDispositionChange::ZombieIgnored) => {
                return SyscallResult::Error(ESRCH_VALUE);
            }
            Err(v3errno) => return SyscallResult::error_from(Errno::from(v3errno)),
        }
    }

    SyscallResult::Return(0)
}

pub(super) fn signal_delivery_result(
    result: Result<tx_subsystems::signal::KillOutcome, Errno>,
) -> SyscallResult {
    match result {
        Ok(tx_subsystems::signal::KillOutcome::Delivered) => SyscallResult::Return(0),
        Ok(tx_subsystems::signal::KillOutcome::NoLiveThread) => SyscallResult::Error(ESRCH_VALUE),
        Ok(tx_subsystems::signal::KillOutcome::Retry) => SyscallResult::Error(EAGAIN_VALUE),
        Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
    }
}

/// `kill(pid, sig)` — Linux RV64 generic ABI `__NR_kill = 129`.
///
/// Slice 7 v1 surface:
/// - `pid > 0`: deliver `sig` to the matching process via
///   `tx_subsystems::signal::step_kill_process`. Resolved through
///   `process_by_pid`'s init-rooted tree walk.
/// - `pid <= 0`: pgrp / all-processes targets — out of scope for v1
///   (`-ENOSYS`; needs a global pid-to-pgrp lookup the slice does
///   not yet wire).
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
    if pid < 0 {
        // `pid < -1`: signal process group `|pid|`. `pid == -1`:
        // broadcast to every process the caller may signal (POSIX),
        // excluding init (pid 1) and self.
        //
        // txKernel has no standalone pgid→group index, so we resolve
        // membership from the global pid registry (`all_pids`) plus each
        // live process's `pgrp_cap().pgid`. This is an authoritative live
        // snapshot that does not depend on the group leader still being
        // alive — important for harnesses (LTP) that `kill(-pgid)` after
        // the leader has reaped but children remain.
        let broadcast = pid == -1;
        let target_pgid = pid.unsigned_abs();

        let signum_opt = if sig == 0 {
            None
        } else {
            match u8::try_from(sig).ok().and_then(Signum::new) {
                Some(s) => Some(s),
                None => return SyscallResult::Error(EINVAL_VALUE),
            }
        };

        let self_pid = ctx.process.pid.0;
        let mut matched = 0u32;
        let mut delivered = 0u32;
        let mut retry = false;
        for (member_pid, _) in tx_subsystems::process::all_pids() {
            let proc = match tx_subsystems::process::process_by_pid(member_pid) {
                Some(p) => p,
                None => continue,
            };
            if broadcast {
                if member_pid.0 == 1 || member_pid.0 == self_pid {
                    continue;
                }
            } else if proc.pgrp_cap().pgid.0 != target_pgid {
                continue;
            }
            matched += 1;
            let Some(signum) = signum_opt else {
                continue;
            };
            let siginfo = Some(SigInfo {
                si_signo: signum.raw() as u32,
                si_code: SI_USER,
                si_pid: self_pid,
                si_uid: 0,
            });
            match tx_subsystems::signal::script_kill_process_with_posts(
                &ctx.process,
                &proc,
                signum,
                siginfo,
                |mailbox, event| ctx.post_mailbox_event(mailbox, event),
                |mailbox, event| ctx.post_mailbox_ref_event(mailbox, event),
            ) {
                Ok(tx_subsystems::signal::KillScriptOutcome::Delivered) => delivered += 1,
                Ok(tx_subsystems::signal::KillScriptOutcome::Retry) => retry = true,
                Ok(
                    tx_subsystems::signal::KillScriptOutcome::NoLiveThread
                    | tx_subsystems::signal::KillScriptOutcome::Probed,
                )
                | Err(_) => {}
            }
        }

        if matched == 0 {
            return SyscallResult::Error(ESRCH_VALUE);
        }
        if sig == 0 {
            // Existence probe (`kill(-pgid, 0)`): the group has at least
            // one live member.
            return SyscallResult::Return(0);
        }
        return if delivered > 0 {
            SyscallResult::Return(0)
        } else if retry {
            SyscallResult::Error(EAGAIN_VALUE)
        } else {
            // Members existed but none accepted the signal (cred denial
            // or raced exit) → EPERM, matching the `pid == 0` path.
            SyscallResult::Error(EPERM_VALUE)
        };
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

    let siginfo = Some(SigInfo {
        si_signo: signum.raw() as u32,
        si_code: SI_USER,
        si_pid: ctx.process.pid.0,
        si_uid: 0, // TODO: populate from cred when available
    });

    // Route through the cred-checked script entry point. Drives
    // `cred::require_signal_send` against the caller's syscall-entry
    // snapshot (per cred_service_v_1 §"In flight" + §"Checks
    // surface") and only then commits the post via `step_kill_process`.
    // Going through `KillProcessOp::drive_oneshot` directly would
    // bypass the cred check, since `KillProcessOp::step` calls the
    // primitive `step_kill_process` without authorization.
    use tx_subsystems::signal::KillScriptOutcome;
    dispatch_errno(
        tx_subsystems::signal::script_kill_process_with_posts(
            &ctx.process,
            &target,
            signum,
            siginfo,
            |mailbox, event| ctx.post_mailbox_event(mailbox, event),
            |mailbox, event| ctx.post_mailbox_ref_event(mailbox, event),
        ),
        |outcome| match outcome {
            KillScriptOutcome::Delivered | KillScriptOutcome::Probed => SyscallResult::Return(0),
            KillScriptOutcome::NoLiveThread => SyscallResult::Error(ESRCH_VALUE),
            KillScriptOutcome::Retry => SyscallResult::Error(EAGAIN_VALUE),
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
            return signal_delivery_result(
                tx_subsystems::signal::script_authorized_thread_exit_with_posts(
                    &ctx.process,
                    &thread_cap,
                    signum,
                    |mailbox, event| ctx.post_mailbox_event(mailbox, event),
                    |mailbox, event| ctx.post_mailbox_ref_event(mailbox, event),
                ),
            );
        }

        let siginfo = Some(SigInfo {
            si_signo: signum.raw() as u32,
            si_code: SI_TKILL,
            si_pid: ctx.process.pid.0,
            si_uid: 0,
        });
        return match tx_subsystems::signal::script_deliver_signal_with_posts(
            &ctx.process,
            SignalTarget::Thread(thread_cap),
            signum,
            siginfo,
            |mailbox, event| ctx.post_mailbox_event(mailbox, event),
            |mailbox, event| ctx.post_mailbox_ref_event(mailbox, event),
        ) {
            Ok(tx_subsystems::signal::KillOutcome::Delivered) => SyscallResult::Return(0),
            Ok(tx_subsystems::signal::KillOutcome::NoLiveThread) => {
                SyscallResult::Error(ESRCH_VALUE)
            }
            Ok(tx_subsystems::signal::KillOutcome::Retry) => SyscallResult::Error(EAGAIN_VALUE),
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
            // Keep the caller-tgid restriction above, then use the same
            // owner-aware mailbox/wake publication path as normal signal
            // delivery.  This preserves the merged process-exit protocol
            // instead of bypassing it through the removed legacy helper.
            return signal_delivery_result(
                tx_subsystems::signal::script_authorized_thread_exit_with_posts(
                    &ctx.process,
                    &thread,
                    signum,
                    |mailbox, event| ctx.post_mailbox_event(mailbox, event),
                    |mailbox, event| ctx.post_mailbox_ref_event(mailbox, event),
                ),
            );
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
            Err(v3errno) => return SyscallResult::error_from(Errno::from(v3errno)),
        }
    }
    SyscallResult::Error(ESRCH_VALUE)
}

/// `rt_sigreturn(...)` — Linux RV64 generic ABI
/// `__NR_rt_sigreturn = 139`.
///
/// Signal delivery stores the interrupted context and mask in the
/// architecture-defined userspace frame. After the handler returns through
/// the trampoline, the kernel return path decodes the frame at the caller's
/// current SP, validates it, restores the mask/context, and resumes userspace.
///
/// `SigreturnRestored` tells the syscall-return path in
/// `thread_future` to skip its normal `pending_syscall_return` drain
/// — the merged context's `a0` and `pc` come from the restored
/// pre-signal snapshot, not the syscall's nominal return value.
///
/// Without this restore, the handler's "post-`ret`" path stays in
/// the trampoline / signal-frame memory and the thread reads garbage
/// off the signal stack — observed end-to-end as the busybox-sh
/// SIGCHLD-handler crash on 2026-05-18 (root cause traced through
/// the trap-trace serial log).
///
/// There is deliberately no kernel-side "active frame" depth or shadow
/// context. Nested handlers, fork from a handler, and user edits to ucontext
/// all work because the userspace frame is authoritative.
pub(super) fn sys_rt_sigreturn(ctx: &SyscallCtx) -> SyscallResult {
    let Some(payload) = ctx.thread.payload_cap() else {
        return SyscallResult::Error(EFAULT_VALUE);
    };
    if payload.saved_user_context().is_none() {
        return SyscallResult::Error(EFAULT_VALUE);
    }
    SyscallResult::SigreturnRestored
}

/// `rt_sigpending(set, sigsetsize)` — Linux LP64 signal ABI.
///
/// Phase F: reads the calling thread's pending signal bitset
/// (thread_pending merged with the owning process's group_pending)
/// and writes it to `set`.
pub(super) async fn sys_rt_sigpending(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let set_ptr = args[0] as usize;
    let sigsetsize = args[1];

    let Some(wire_abi) = classify_rt_signal_set_abi(sigsetsize, CURRENT_RT_SIGNAL_TARGET_ABI)
    else {
        return SyscallResult::Error(EINVAL_VALUE);
    };

    let Some(payload) = ctx.thread.payload_cap() else {
        return SyscallResult::Error(ESRCH_VALUE);
    };
    let pending = payload.pending().snapshot() | ctx.process.group_pending_snapshot();
    if let Err(errno) =
        write_rt_signal_set_wait(&ctx.aspace, set_ptr as u64, wire_abi, pending).await
    {
        return SyscallResult::error_from(errno);
    }

    SyscallResult::Return(0)
}

#[cfg(test)]
mod rt_sigaction_wire_abi_tests {
    use super::*;

    #[test]
    fn loongarch_old_world_classifies_16_byte_sigsets() {
        assert_eq!(
            classify_rt_signal_set_abi(16, RtSignalTargetAbi::LoongArch64),
            Some(RtSignalSetAbi::LoongArchOldWorld)
        );
    }

    #[test]
    fn generic_64_bit_targets_keep_rejecting_16_byte_sigsets() {
        assert_eq!(
            classify_rt_signal_set_abi(8, RtSignalTargetAbi::Generic64),
            Some(RtSignalSetAbi::Generic64)
        );
        assert_eq!(
            classify_rt_signal_set_abi(16, RtSignalTargetAbi::Generic64),
            None
        );
    }

    #[test]
    fn loongarch_old_world_action_ignores_high_mask_on_input_and_zeros_it_on_output() {
        const FLAGS: u64 = 0x1000_0004;
        const LOW_MASK: u64 = 0xa5a5_5a5a_ffff_0000;
        const HIGH_MASK: u64 = 0xfeed_face_dead_beef;

        let mut input = [0u8; LA64_OLDWORLD_SIGACTION_BYTES];
        input[0..8].copy_from_slice(&1u64.to_le_bytes());
        input[8..16].copy_from_slice(&FLAGS.to_le_bytes());
        input[16..24].copy_from_slice(&LOW_MASK.to_le_bytes());
        input[24..32].copy_from_slice(&HIGH_MASK.to_le_bytes());

        let entry = decode_rt_sigaction_entry(RtSignalSetAbi::LoongArchOldWorld, &input);
        assert_eq!(entry.disposition, SigDisposition::Ignore);
        assert_eq!(entry.flags.bits(), FLAGS);
        assert_eq!(
            entry.sa_mask.raw_bits(),
            SignalMask::new(LOW_MASK).raw_bits()
        );

        let mut output = [0xaau8; LA64_OLDWORLD_SIGACTION_BYTES];
        encode_rt_sigaction_entry(RtSignalSetAbi::LoongArchOldWorld, entry, &mut output);
        assert_eq!(read_u64_le(&output[0..8]), 1);
        assert_eq!(read_u64_le(&output[8..16]), FLAGS);
        assert_eq!(read_u64_le(&output[16..24]), entry.sa_mask.raw_bits());
        assert_eq!(read_u64_le(&output[24..32]), 0);
    }

    #[test]
    fn loongarch_old_world_sigset_codec_reads_16_bytes_and_zero_extends_output() {
        const LOW: u64 = 0x0123_4567_89ab_cdef;
        const UNSUPPORTED_HIGH: u64 = 0xfedc_ba98_7654_3210;

        let mut input = [0u8; LA64_OLDWORLD_SIGSETSIZE_BYTES as usize];
        input[0..8].copy_from_slice(&LOW.to_le_bytes());
        input[8..16].copy_from_slice(&UNSUPPORTED_HIGH.to_le_bytes());
        assert_eq!(
            decode_rt_signal_set(RtSignalSetAbi::LoongArchOldWorld, &input),
            LOW
        );

        let mut output = [0xaau8; LA64_OLDWORLD_SIGSETSIZE_BYTES as usize];
        encode_rt_signal_set(RtSignalSetAbi::LoongArchOldWorld, LOW, &mut output);
        assert_eq!(read_u64_le(&output[0..8]), LOW);
        assert_eq!(read_u64_le(&output[8..16]), 0);
    }

    #[test]
    fn generic_sigset_codec_keeps_the_8_byte_wire_width() {
        const LOW: u64 = 0x0123_4567_89ab_cdef;
        let mut output = [0xaau8; LA64_OLDWORLD_SIGSETSIZE_BYTES as usize];

        encode_rt_signal_set(RtSignalSetAbi::Generic64, LOW, &mut output);

        assert_eq!(read_u64_le(&output[0..8]), LOW);
        assert_eq!(read_u64_le(&output[8..16]), u64::from_le_bytes([0xaa; 8]));
    }

    #[test]
    fn rt_sigaction_wire_classifier_rejects_zero_and_other_sizes() {
        for target in [RtSignalTargetAbi::Generic64, RtSignalTargetAbi::LoongArch64] {
            assert_eq!(classify_rt_signal_set_abi(0, target), None);
            assert_eq!(classify_rt_signal_set_abi(1, target), None);
            assert_eq!(classify_rt_signal_set_abi(24, target), None);
            assert_eq!(classify_rt_signal_set_abi(128, target), None);
        }
    }
}
