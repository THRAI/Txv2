//! Auto-extracted from `mod.rs` during the 2026-05-08 jumbo-mod
//! split. The implementations here are unchanged; only the
//! enclosing module changed. Helpers and constants used here live
//! either in this submodule or in the shared parent (`super::*`).

use super::*;
#[cfg(tx_sigprocmask_detail_metrics)]
use core::sync::atomic::{AtomicU64, Ordering};
use tx_services::time::{
    timekeeper_clock, ClockRead, DeadlineRegistrar, DeadlineRegistrarHandle, TimekeeperClock,
};
use tx_substrate::verbs::OperationalCapExt;
use tx_subsystems::process::numbers::{resolve_pid_number_as, PidName, PidNameKind};
use tx_subsystems::signal::{KillOutcome, SignalTarget};
use tx_subsystems::signal::{SigInfo, SI_USER};

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
        observer.debug_counter(name, value);
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
        observer.debug_counter(name, dur.min(i64::MAX as u64) as i64);
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
/// `sigsetsize` is rejected with `-EINVAL` for any value other than
/// `8` (the kernel's only supported sigset width on RV64 — a single
/// `u64` bitset). `set_ptr == 0` means "query only"; `oldset_ptr == 0`
/// means "don't return the previous mask".
pub(super) fn sys_rt_sigprocmask<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    sys_rt_sigprocmask_thread_aspace(args, &ctx.thread, &ctx.aspace)
}

pub(super) fn sys_rt_sigprocmask_thread_aspace(
    args: [u64; 6],
    thread: &Cap<ThreadIdentity>,
    aspace: &Cap<AddressSpace>,
) -> SyscallResult {
    sys_rt_sigprocmask_impl(args, thread, None, aspace)
}

pub(super) fn sys_rt_sigprocmask_thread_payload_aspace(
    args: [u64; 6],
    thread: &Cap<ThreadIdentity>,
    payload: &PayloadCap<ThreadPayload>,
    aspace: &Cap<AddressSpace>,
) -> SyscallResult {
    sys_rt_sigprocmask_impl(args, thread, Some(payload), aspace)
}

fn sys_rt_sigprocmask_impl(
    args: [u64; 6],
    thread: &Cap<ThreadIdentity>,
    payload: Option<&PayloadCap<ThreadPayload>>,
    aspace: &Cap<AddressSpace>,
) -> SyscallResult {
    let total_start = sigprocmask_detail_now();
    let decode_start = sigprocmask_detail_now();
    #[cfg(not(tx_sigprocmask_detail_metrics))]
    let _ = decode_start;
    let how_raw = args[0] as i32;
    let set_ptr = args[1] as usize;
    let oldset_ptr = args[2] as usize;
    let sigsetsize = args[3];
    #[cfg(tx_sigprocmask_detail_metrics)]
    {
        emit_sigprocmask_detail_duration(b"debug.sigprocmask.detail.decode_ns", decode_start);
        emit_sigprocmask_detail_value(
            b"debug.sigprocmask.detail.route",
            i64::from(payload.is_some())
                | (i64::from(set_ptr != 0) << 1)
                | (i64::from(oldset_ptr != 0) << 2),
        );
    }
    let trace_seq = sigprocmask_trace_sample();
    if let Some(seq) = trace_seq {
        emit_sigprocmask_debug(b"debug.sigprocmask.enter", seq);
        emit_sigprocmask_debug(
            b"debug.sigprocmask.args",
            (how_raw as i64 & 0xff)
                | (i64::from(set_ptr != 0) << 8)
                | (i64::from(oldset_ptr != 0) << 9),
        );
    }

    if sigsetsize != SIGSETSIZE_BYTES {
        if let Some(seq) = trace_seq {
            emit_sigprocmask_debug(b"debug.sigprocmask.bad_size", seq);
        }
        #[cfg(tx_sigprocmask_detail_metrics)]
        {
            emit_sigprocmask_detail_value(b"debug.sigprocmask.detail.bad_size", 1);
            emit_sigprocmask_detail_duration(b"debug.sigprocmask.detail.total_ns", total_start);
        }
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
        _ => {
            #[cfg(tx_sigprocmask_detail_metrics)]
            {
                emit_sigprocmask_detail_value(b"debug.sigprocmask.detail.bad_how", 1);
                emit_sigprocmask_detail_duration(b"debug.sigprocmask.detail.total_ns", total_start);
            }
            return SyscallResult::Error(EINVAL_VALUE);
        }
    };

    // Read the user-supplied set bitset through the canonical
    // user-VA lane (`bootstrap_read_user` bridges via
    // `aspace.read_user`, falling back to a kernel-pointer read on
    // EFAULT).
    let next_mask = if set_ptr == 0 {
        SignalMask::EMPTY
    } else {
        let read_start = sigprocmask_detail_now();
        match bootstrap_read_user::<u64>(aspace, set_ptr as u64) {
            Ok(bits) => {
                emit_sigprocmask_detail_duration(
                    b"debug.sigprocmask.detail.read_user_ns",
                    read_start,
                );
                SignalMask::new(bits)
            }
            Err(errno) => {
                if let Some(seq) = trace_seq {
                    emit_sigprocmask_debug(b"debug.sigprocmask.read.err", seq);
                }
                #[cfg(tx_sigprocmask_detail_metrics)]
                {
                    emit_sigprocmask_detail_duration(
                        b"debug.sigprocmask.detail.read_user_ns",
                        read_start,
                    );
                    emit_sigprocmask_detail_value(b"debug.sigprocmask.detail.read_err", 1);
                    emit_sigprocmask_detail_duration(
                        b"debug.sigprocmask.detail.total_ns",
                        total_start,
                    );
                }
                return SyscallResult::error_from(errno);
            }
        }
    };
    if let Some(seq) = trace_seq {
        emit_sigprocmask_debug(b"debug.sigprocmask.read.after", seq);
    }

    // If `set` is null, this is query-only. Read the current mask directly
    // instead of driving a no-op SigprocmaskOp: the no-op path still refreshes
    // signal deliverability, which is unnecessary work on pthread lifecycle
    // probes.
    let prev_mask: SignalMask = match how {
        Some(how) => {
            let step_start = sigprocmask_detail_now();
            match payload.map_or_else(
                || step_sigprocmask(thread, how, next_mask),
                |payload| step_sigprocmask_with_payload(thread, payload, how, next_mask),
            ) {
                SigprocmaskChange::Replaced { prev, .. } => {
                    emit_sigprocmask_detail_duration(
                        b"debug.sigprocmask.detail.step_ns",
                        step_start,
                    );
                    if let Some(seq) = trace_seq {
                        emit_sigprocmask_debug(b"debug.sigprocmask.step.after", seq);
                    }
                    prev
                }
                SigprocmaskChange::ZombieIgnored => {
                    #[cfg(tx_sigprocmask_detail_metrics)]
                    {
                        emit_sigprocmask_detail_duration(
                            b"debug.sigprocmask.detail.step_ns",
                            step_start,
                        );
                        emit_sigprocmask_detail_value(b"debug.sigprocmask.detail.step_zombie", 1);
                        emit_sigprocmask_detail_duration(
                            b"debug.sigprocmask.detail.total_ns",
                            total_start,
                        );
                    }
                    if let Some(seq) = trace_seq {
                        emit_sigprocmask_debug(b"debug.sigprocmask.step.zombie", seq);
                    }
                    return SyscallResult::Error(ESRCH_VALUE);
                }
            }
        }
        None => {
            let query_start = sigprocmask_detail_now();
            let mask = if let Some(payload) = payload {
                payload.signal_mask()
            } else {
                let Some(payload) = thread.payload_cap() else {
                    if let Some(seq) = trace_seq {
                        emit_sigprocmask_debug(b"debug.sigprocmask.mask.zombie", seq);
                    }
                    #[cfg(tx_sigprocmask_detail_metrics)]
                    {
                        emit_sigprocmask_detail_duration(
                            b"debug.sigprocmask.detail.query_mask_ns",
                            query_start,
                        );
                        emit_sigprocmask_detail_value(b"debug.sigprocmask.detail.mask_zombie", 1);
                        emit_sigprocmask_detail_duration(
                            b"debug.sigprocmask.detail.total_ns",
                            total_start,
                        );
                    }
                    return SyscallResult::Error(ESRCH_VALUE);
                };
                payload.signal_mask()
            };
            emit_sigprocmask_detail_duration(
                b"debug.sigprocmask.detail.query_mask_ns",
                query_start,
            );
            if let Some(seq) = trace_seq {
                emit_sigprocmask_debug(b"debug.sigprocmask.mask.after", seq);
            }
            mask
        }
    };

    if oldset_ptr != 0 {
        let write_start = sigprocmask_detail_now();
        if let Err(errno) =
            bootstrap_write_user::<u64>(aspace, oldset_ptr as u64, prev_mask.raw_bits())
        {
            if let Some(seq) = trace_seq {
                emit_sigprocmask_debug(b"debug.sigprocmask.write.err", seq);
            }
            #[cfg(tx_sigprocmask_detail_metrics)]
            {
                emit_sigprocmask_detail_duration(
                    b"debug.sigprocmask.detail.write_user_ns",
                    write_start,
                );
                emit_sigprocmask_detail_value(b"debug.sigprocmask.detail.write_err", 1);
                emit_sigprocmask_detail_duration(b"debug.sigprocmask.detail.total_ns", total_start);
            }
            return SyscallResult::error_from(errno);
        }
        emit_sigprocmask_detail_duration(b"debug.sigprocmask.detail.write_user_ns", write_start);
        if let Some(seq) = trace_seq {
            emit_sigprocmask_debug(b"debug.sigprocmask.write.after", seq);
        }
    }

    if let Some(seq) = trace_seq {
        emit_sigprocmask_debug(b"debug.sigprocmask.return", seq);
    }
    emit_sigprocmask_detail_duration(b"debug.sigprocmask.detail.total_ns", total_start);
    SyscallResult::Return(0)
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
    // by the delivered handler's `sigreturn` frame / the caller's own SETMASK
    // after its wait loop.
    let restore_and_eintr = |ctx: &SyscallCtx<'_>, old_mask: SignalMask| {
        let _ = set_thread_signal_mask(ctx, old_mask);
        SyscallResult::Error(EINTR_VALUE)
    };

    const CHUNK_NS: u64 = 5_000_000;
    loop {
        let timer_registrar = ctx.timer_registrar.as_ref().cloned();
        let timer_mailbox = ctx.mailbox.as_ref().map(alloc::sync::Arc::downgrade);
        let itimer_deadline_ns = poll_due_itimers_with_post::<P, _>(
            &ctx.process,
            timer_registrar
                .as_ref()
                .map(|registrar| registrar as &dyn DeadlineRegistrar),
            timer_mailbox,
            |mailbox, event| {
                ctx.post_mailbox_event(mailbox, event);
            },
        );
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
        if tx_subsystems::signal::thread_pending_signal_interrupts(&ctx.thread) {
            return restore_and_eintr(ctx, old_mask);
        }

        use crate::adapter::step_engine::DriveMode;
        use tx_scripts::drive;
        let now_ns = timekeeper_clock::<P>().monotonic_now_ns();
        let chunk_deadline_ns = now_ns.saturating_add(CHUNK_NS);
        let park_deadline_ns = itimer_deadline_ns
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

async fn park_sigtimedwait_tick<P>(ctx: &SyscallCtx<'_>, wait_bits: u64, deadline_ns: Option<u64>)
where
    TimekeeperClock<P>: ClockRead,
{
    const SIGTIMEDWAIT_POLL_NS: u64 = 1_000_000;

    if wait_bits & Signum::SIGCHLD.bit() != 0 && deadline_ns.is_none() {
        if let Some(endpoint) = ctx.process.exit_endpoint() {
            tx_subsystems::wait_source::wait_on_endpoint(
                &endpoint,
                tx_subsystems::process::EXIT_SOURCE_CHILD_ZOMBIFIED,
            )
            .await;
            return;
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

/// `rt_sigtimedwait(set, info, timeout, sigsetsize)` — Linux RV64
/// ABI `__NR_rt_sigtimedwait = 137`.
///
/// Consumes a pending signal named by `set`, optionally waits until
/// `timeout` expires, and writes the leading Linux `siginfo_t` fields.
/// Signal-mask interaction is intentionally different from normal
/// delivery: `sigtimedwait(2)` observes pending signals in `set` even
/// when they are blocked, which is how runtest waits for child SIGCHLD.
pub(super) async fn sys_rt_sigtimedwait<'a, P>(
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
            if let Err(errno) = write_sigtimedwait_siginfo(ctx, info_ptr, sig) {
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
        nonblocking: (flags & O_NONBLOCK) != 0,
        packet: false,
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

/// `rt_sigaction(signum, act, oldact, sigsetsize)` per `SIGNAL_v1`
/// §15.1.
///
/// Decodes a 32-byte kernel `struct sigaction` (see `SIGACTION_BYTES`
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
        let restorer = read_u64_le(&bytes[24..32]);

        // SIG_DFL == 0, SIG_IGN == 1 per Linux generic ABI; everything
        // else is a userspace function-pointer handler.
        let disp = match handler {
            0 => SigDisposition::Default,
            1 => SigDisposition::Ignore,
            other => SigDisposition::Handler(other as usize),
        };
        Some(SigActionEntry::new(disp, flags, mask, restorer as usize))
    };

    // If the caller wants the previous disposition, snapshot it
    // *before* installing the new one. `step_sigaction` returns the
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
                Ok(SigDispositionChange::Replaced { prev }) => prev,
                Ok(SigDispositionChange::Uncatchable(prev)) => prev,
                Ok(SigDispositionChange::ZombieIgnored) => {
                    return SyscallResult::Error(ESRCH_VALUE);
                }
                Err(v3errno) => {
                    return SyscallResult::error_from(Errno::from(v3errno));
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
        // Build a 32-byte image and copy out through the canonical
        // user-VA lane. RV64 musl layout: 4×u64 little-endian
        // (handler, flags, mask, unused/restorer).
        let mut image = [0u8; SIGACTION_BYTES];
        image[0..8].copy_from_slice(&handler_value.to_le_bytes());
        image[8..16].copy_from_slice(&prev_entry.flags.bits().to_le_bytes());
        image[16..24].copy_from_slice(&prev_entry.sa_mask.raw_bits().to_le_bytes());
        image[24..32].copy_from_slice(&(prev_entry.restorer as u64).to_le_bytes());
        if let Err(errno) = bootstrap_copy_to_user(&ctx.aspace, oldact_ptr as u64, &image) {
            return SyscallResult::error_from(errno);
        }
    }

    SyscallResult::Return(0)
}

fn signal_probe_result(
    result: Result<tx_subsystems::signal::KillScriptOutcome, Errno>,
) -> SyscallResult {
    match result {
        Ok(tx_subsystems::signal::KillScriptOutcome::Probed) => SyscallResult::Return(0),
        Ok(tx_subsystems::signal::KillScriptOutcome::NoLiveThread) => {
            SyscallResult::Error(ESRCH_VALUE)
        }
        Ok(tx_subsystems::signal::KillScriptOutcome::Retry) => SyscallResult::Error(EAGAIN_VALUE),
        Ok(tx_subsystems::signal::KillScriptOutcome::Delivered) => SyscallResult::Return(0),
        Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
    }
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
/// Supported target modes:
/// - `pid > 0`: the matching process.
/// - `pid == 0`: every permitted live member of the caller's process group.
/// - `pid == -1`: every permitted live process except init and the caller.
/// - `pid < -1`: every permitted live member of process group `|pid|`.
/// - `sig == 0`: permission probe with the same target selection and no delivery.
/// - Unknown signum (outside 1..=64): `-EINVAL`.
/// - Target zombie / no live thread: `-ESRCH` (matches Linux's
///   "kill returns ESRCH if no signal could be delivered").
pub(super) fn sys_kill(args: [u64; 6], ctx: &SyscallCtx) -> SyscallResult {
    let pid = args[0] as i32;
    let sig = args[1] as u32;

    // Phase F: pid == 0 routes to caller's process group.
    if pid == 0 {
        if sig == 0 {
            let pgrp = ctx.process.pgrp_cap();
            return dispatch_errno(
                tx_subsystems::signal::script_kill_pgrp_probe(&ctx.process, &pgrp),
                |permitted| {
                    if permitted > 0 {
                        SyscallResult::Return(0)
                    } else {
                        SyscallResult::Error(EPERM_VALUE)
                    }
                },
            );
        }
        let signum = match u8::try_from(sig).ok().and_then(Signum::new) {
            Some(s) => s,
            None => return SyscallResult::Error(EINVAL_VALUE),
        };
        let pgrp = ctx.process.pgrp_cap();
        // Route through script_kill_pgrp (cred-checked per-member fanout)
        // rather than the primitive process-group delivery helper, which would deliver
        // without consulting cred::require_signal_send.
        // POSIX: kill(0, sig) returns -EPERM when no member was both
        // live AND permitted; script_kill_pgrp folds zombies and
        // permission denials into the same 0-count return.
        return dispatch_errno(
            tx_subsystems::signal::script_kill_pgrp_with_posts(
                &ctx.process,
                &pgrp,
                signum,
                |mailbox, event| ctx.post_mailbox_event(mailbox, event),
                |mailbox, event| ctx.post_mailbox_ref_event(mailbox, event),
            ),
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
        let mut probed = 0u32;
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
                if matches!(
                    tx_subsystems::signal::script_kill_probe(&ctx.process, &proc),
                    Ok(tx_subsystems::signal::KillScriptOutcome::Probed)
                ) {
                    probed += 1;
                }
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
            return if probed > 0 {
                SyscallResult::Return(0)
            } else {
                SyscallResult::Error(EPERM_VALUE)
            };
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
        return signal_probe_result(tx_subsystems::signal::script_kill_probe(
            &ctx.process,
            &target,
        ));
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
    // surface") and only then commits the post via the process-directed
    // signal primitive. Going through a process-kill StepOp directly
    // would bypass the cred check.
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
            let Some(target_process) = thread_cap.upgrade_owner_proc() else {
                return SyscallResult::Error(ESRCH_VALUE);
            };
            return signal_probe_result(tx_subsystems::signal::script_kill_probe(
                &ctx.process,
                &target_process,
            ));
        }
        let signum = match u8::try_from(sig).ok().and_then(Signum::new) {
            Some(s) => s,
            None => return SyscallResult::Error(EINVAL_VALUE),
        };

        #[cfg(target_arch = "loongarch64")]
        if signum.raw() == MUSL_SIGCANCEL {
            // LA64 signal-frame delivery for musl's private SIGCANCEL path is
            // not ABI-complete yet. Authorize exactly like normal tkill before
            // terminating the target's process as a bounded fail-fast.
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
        return signal_delivery_result(tx_subsystems::signal::script_deliver_signal_with_posts(
            &ctx.process,
            SignalTarget::Thread(thread_cap),
            signum,
            siginfo,
            |mailbox, event| ctx.post_mailbox_event(mailbox, event),
            |mailbox, event| ctx.post_mailbox_ref_event(mailbox, event),
        ));
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
    // `script_deliver_signal_with_posts` (cred-checked) the way sys_tkill does.
    if tgid != ctx.process.pid.0 {
        return SyscallResult::Error(ESRCH_VALUE);
    }
    if sig == 0 {
        let Some(thread) = ctx.process.thread_by_tid(tid) else {
            return SyscallResult::Error(ESRCH_VALUE);
        };
        let Some(target_process) = thread.upgrade_owner_proc() else {
            return SyscallResult::Error(ESRCH_VALUE);
        };
        return signal_probe_result(tx_subsystems::signal::script_kill_probe(
            &ctx.process,
            &target_process,
        ));
    }
    let signum = match u8::try_from(sig).ok().and_then(Signum::new) {
        Some(s) => s,
        None => return SyscallResult::Error(EINVAL_VALUE),
    };
    if let Some(thread) = ctx.process.thread_by_tid(tid) {
        #[cfg(target_arch = "loongarch64")]
        if signum.raw() == MUSL_SIGCANCEL {
            // Keep the caller-tgid restriction above, then run the same target
            // lifecycle and credential authorization as normal tgkill.
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
        return signal_delivery_result(tx_subsystems::signal::script_deliver_signal_with_posts(
            &ctx.process,
            SignalTarget::Thread(thread),
            signum,
            siginfo,
            |mailbox, event| ctx.post_mailbox_event(mailbox, event),
            |mailbox, event| ctx.post_mailbox_ref_event(mailbox, event),
        ));
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
/// If no signal frame is in flight, the kernel has no parked
/// context to restore. POSIX leaves this case undefined; we return
/// `-EFAULT` defensively rather than corrupt the current context.
pub(super) fn sys_rt_sigreturn(ctx: &SyscallCtx) -> SyscallResult {
    let Some(payload) = ctx.thread.payload_cap() else {
        return SyscallResult::Error(EFAULT_VALUE);
    };

    let current = payload.saved_user_context();
    if let Some(current_ctx) = current {
        if let Ok(frame) = read_compat_signal_frame(&ctx.aspace, current_ctx.regs[2] as u64) {
            let mut script_ctx = build_subject_script_ctx(ctx);
            let mut op = SigprocmaskOp {
                thread: ctx.thread.clone(),
                how: SigmaskHow::SetMask,
                next: SignalMask::new(frame.saved_mask.bits),
            };
            let _ = step_engine::drive_oneshot(&mut op, &mut script_ctx);
            let _ = payload.take_saved_signal_context();
            let _ = payload.take_saved_signal_mask();
            payload.store_saved_user_context(Some(frame.user_context));
            return SyscallResult::SigreturnContextRestored;
        }
    }

    if let Some(saved) = payload.take_saved_signal_context() {
        if let Some(mask) = payload.take_saved_signal_mask() {
            payload.store_signal_mask(mask);
        }
        payload.store_saved_user_context(Some(saved));
        return SyscallResult::SigreturnRestored;
    }

    let Some(current) = current else {
        return SyscallResult::Error(EFAULT_VALUE);
    };
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

    let Some(payload) = ctx.thread.payload_cap() else {
        return SyscallResult::Error(ESRCH_VALUE);
    };
    let pending = payload.pending().snapshot() | ctx.process.group_pending_snapshot();
    if let Err(errno) = bootstrap_write_user::<u64>(&ctx.aspace, set_ptr as u64, pending) {
        return SyscallResult::error_from(errno);
    }

    SyscallResult::Return(0)
}
