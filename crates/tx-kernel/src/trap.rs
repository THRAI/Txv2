use tx_hal::{
    CpuId, FaultInfo, IpiKind, IrqHandled, KernelTrapSink, PercpuIf, TrapAction, TrapFrameMut,
    TxPlatform, VirtAddr,
};
use tx_shims::linux_syscall::numbers::{NR_RT_SIGPROCMASK, NR_SET_TID_ADDRESS};
use tx_shims::linux_syscall::SyscallResult;

use crate::{adapter::boot_runtime, trap_handoff};

pub struct KernelTrapDispatcher;

impl<P: TxPlatform> KernelTrapSink<P> for KernelTrapDispatcher {
    fn on_page_fault(view: TrapFrameMut<'_>, fault: FaultInfo) -> TrapAction {
        // Kernel-mode page faults: keep the existing terminate
        // policy; only user-mode faults can be handed off to a
        // userspace-run wait.
        if !fault.from_user {
            return TrapAction::Terminate;
        }

        // Phase 1: snapshot context, resolve the userspace-run
        // wait, and reschedule. Per the Trio plan's "Cross-cutting
        // risks #1" Plan B writeback discipline, no trap-frame
        // mutation happens here.
        let info = trap_handoff::translate_user_pf::<P>(&view.view(), &fault);
        let hart = <P as PercpuIf>::current_cpu_id().0;
        let outcome = trap_handoff::hand_off_user_pf(hart, &view, info);
        let action = trap_handoff::outcome_to_trap_action(&outcome);
        if matches!(action, TrapAction::Terminate) {
            log_page_fault_handoff_failure::<P>(hart, &outcome);
        }
        action
    }

    fn on_syscall(mut view: TrapFrameMut<'_>) -> TrapAction {
        // Phase 1: translate, snapshot context into the active
        // payload, resolve the userspace-run wait, and reschedule.
        // No trap-frame writeback (Plan B); the userspace-entry
        // shim drains `pending_syscall_return` and writes
        // `set_syscall_return` / `set_syscall_error` into the
        // *fresh* trap frame before `enter_userspace`.
        let req = trap_handoff::translate_syscall::<P>(&view.view());
        if let Some(action) = try_direct_trap_syscall::<P>(&mut view, &req) {
            return action;
        }
        emit_debug_counter(b"debug.trap.syscall", req.nr as i64);
        let hart = <P as PercpuIf>::current_cpu_id().0;
        let outcome = trap_handoff::hand_off_syscall(hart, &view, req);
        trap_handoff::outcome_to_trap_action(&outcome)
    }

    fn on_timer_interrupt(cpu: CpuId, view: TrapFrameMut<'_>) -> TrapAction {
        P::cancel_deadline();
        if view.view().previous_mode == tx_hal::TrapPreviousMode::User {
            emit_debug_counter(b"debug.trap.timer_user", view.view().pc.0 as i64);
            let hart = <P as PercpuIf>::current_cpu_id().0;
            let outcome = trap_handoff::hand_off_timer_preempt(hart, &view);
            if matches!(outcome, trap_handoff::TimerPreemptOutcome::Preempted) {
                crate::init::mark_boot_reactor_userspace_preempt(cpu);
            }
            return trap_handoff::timer_preempt_outcome_to_trap_action(&outcome);
        }

        TrapAction::Resume
    }

    fn on_external_irq(cpu: CpuId, view: TrapFrameMut<'_>) -> TrapAction {
        let irq = P::claim();
        if irq == 0 {
            return TrapAction::Resume;
        }

        let handled = P::dispatch_irq(irq);
        P::complete(irq);

        match handled {
            IrqHandled::Wake => {
                // An external interrupt that lands while USER code is
                // running must park the interrupted thread exactly like
                // a timer preempt: save the user context and resolve
                // the thread's userspace wait so the reactor can resume
                // it later. Returning a bare `Reschedule` here (the
                // pre-2026-07-06 behaviour) longjmps to the reactor
                // with the user context unsaved and the wait
                // unresolved — the thread is parked forever and the
                // session appears to freeze. Trigger: serial RX
                // arriving in the window where the process is
                // executing userspace (vim startup racing the
                // terminal's query responses; bursty paste input);
                // also the P2 external-connect resume hang (2026-07-03).
                // Kernel-mode interrupts (WFI wake) keep the plain
                // Reschedule path.
                if view.view().previous_mode == tx_hal::TrapPreviousMode::User {
                    let hart = <P as PercpuIf>::current_cpu_id().0;
                    let outcome = trap_handoff::hand_off_timer_preempt(hart, &view);
                    if matches!(outcome, trap_handoff::TimerPreemptOutcome::Preempted) {
                        crate::init::mark_boot_reactor_userspace_preempt(cpu);
                    }
                    return trap_handoff::timer_preempt_outcome_to_trap_action(&outcome);
                }
                TrapAction::Reschedule
            }
            IrqHandled::Done | IrqHandled::NotMine => TrapAction::Resume,
        }
    }

    fn on_ipi(_cpu: CpuId) -> TrapAction {
        if P::pending_ipi(IpiKind::Membarrier) {
            core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
            P::ack_ipi(IpiKind::Membarrier);
        }
        if P::pending_ipi(IpiKind::Reschedule) {
            P::ack_ipi(IpiKind::Reschedule);
        }
        if P::pending_ipi(IpiKind::TlbShootdown) {
            P::ack_ipi(IpiKind::TlbShootdown);
        }
        TrapAction::Resume
    }

    fn on_illegal_or_sync_fault(view: TrapFrameMut<'_>, fault: FaultInfo) -> TrapAction {
        // Kernel-mode illegal/sync faults are genuinely fatal: terminate
        // (the board maps this to a kernel panic, which is correct — a
        // kernel bug should stop the world).
        if !fault.from_user {
            return TrapAction::Terminate;
        }

        // User-mode illegal instruction / unrecoverable sync fault: deliver
        // a fatal signal to the offending thread and reschedule, exactly
        // like an unrecoverable user page fault (see `on_page_fault`). A
        // crashing *user* process must never panic the *kernel*; otherwise
        // a single bad user instruction (e.g. a test whose context is
        // corrupted on signal return) tears down the whole run instead of
        // just that one process.
        let hart = <P as PercpuIf>::current_cpu_id().0;
        let outcome = trap_handoff::hand_off_user_fatal(hart, &view, 0, fault.address.0 as u64);
        let action = trap_handoff::outcome_to_trap_action(&outcome);
        if matches!(action, TrapAction::Terminate) {
            log_page_fault_handoff_failure::<P>(hart, &outcome);
        }
        action
    }
}

fn try_direct_trap_syscall<P: TxPlatform>(
    view: &mut TrapFrameMut<'_>,
    req: &trap_handoff::SyscallRequest,
) -> Option<TrapAction> {
    let direct_total_start = direct_sigprocmask_detail_now(req.nr);
    let hart = <P as PercpuIf>::current_cpu_id().0;
    let payload_start = direct_sigprocmask_detail_now(req.nr);
    let Some(payload) = tx_subsystems::thread_runtime::current_userspace_payload(hart) else {
        emit_direct_sigprocmask_detail_value(
            req.nr,
            b"debug.trap.direct_sigprocmask.no_payload",
            1,
        );
        return None;
    };
    if payload.active_userspace_request().is_none() {
        emit_direct_sigprocmask_detail_value(
            req.nr,
            b"debug.trap.direct_sigprocmask.no_active_request",
            1,
        );
        return None;
    }
    emit_direct_sigprocmask_detail_duration(
        req.nr,
        b"debug.trap.direct_sigprocmask.payload_ns",
        payload_start,
    );

    let context_start = direct_sigprocmask_detail_now(req.nr);
    let thread_lookup_start = direct_sigprocmask_detail_now(req.nr);
    // The trap runs inside `PerHartSlotted::poll`, which keeps the current
    // thread identity installed until the userspace round-trip longjmps back
    // and the wrapped poll returns.  Use that poll-scoped authority instead of
    // maintaining a second userspace identity cache with a wider lifetime.
    let Some(thread) = tx_subsystems::thread_runtime::current_thread_identity(hart) else {
        emit_direct_sigprocmask_detail_value(req.nr, b"debug.trap.direct_sigprocmask.no_thread", 1);
        return None;
    };
    emit_direct_sigprocmask_detail_duration(
        req.nr,
        b"debug.trap.direct_sigprocmask.context_thread_ns",
        thread_lookup_start,
    );
    let owner_upgrade_start = direct_sigprocmask_detail_now(req.nr);
    let Some(process) = thread.upgrade_owner_proc() else {
        emit_direct_sigprocmask_detail_value(
            req.nr,
            b"debug.trap.direct_sigprocmask.no_process",
            1,
        );
        return None;
    };
    emit_direct_sigprocmask_detail_duration(
        req.nr,
        b"debug.trap.direct_sigprocmask.context_owner_ns",
        owner_upgrade_start,
    );
    let aspace_start = direct_sigprocmask_detail_now(req.nr);
    let Some(aspace) = process.aspace_cap() else {
        emit_direct_sigprocmask_detail_value(req.nr, b"debug.trap.direct_sigprocmask.no_aspace", 1);
        return None;
    };
    emit_direct_sigprocmask_detail_duration(
        req.nr,
        b"debug.trap.direct_sigprocmask.context_aspace_ns",
        aspace_start,
    );
    emit_direct_sigprocmask_detail_duration(
        req.nr,
        b"debug.trap.direct_sigprocmask.context_ns",
        context_start,
    );

    let precondition_start = direct_sigprocmask_detail_now(req.nr);
    if !direct_syscall_preconditions(req.nr, &payload, &process) {
        emit_direct_sigprocmask_detail_duration(
            req.nr,
            b"debug.trap.direct_sigprocmask.precondition_ns",
            precondition_start,
        );
        emit_direct_sigprocmask_detail_value(
            req.nr,
            b"debug.trap.direct_sigprocmask.precondition_failed",
            1,
        );
        return None;
    }
    emit_direct_sigprocmask_detail_duration(
        req.nr,
        b"debug.trap.direct_sigprocmask.precondition_ns",
        precondition_start,
    );

    let dispatch_start = direct_sigprocmask_detail_now(req.nr);
    let Some(result) = tx_shims::linux_syscall::dispatch_direct_trap_payload_oneshot::<P>(
        req, &process, &thread, &payload, &aspace,
    ) else {
        emit_direct_sigprocmask_detail_duration(
            req.nr,
            b"debug.trap.direct_sigprocmask.dispatch_ns",
            dispatch_start,
        );
        emit_direct_sigprocmask_detail_value(
            req.nr,
            b"debug.trap.direct_sigprocmask.unsupported",
            1,
        );
        return None;
    };
    emit_direct_sigprocmask_detail_duration(
        req.nr,
        b"debug.trap.direct_sigprocmask.dispatch_ns",
        dispatch_start,
    );

    let post_start = direct_sigprocmask_detail_now(req.nr);
    let needs_reschedule = direct_trap_syscall_needs_wake_handoff(req, &result);

    match result {
        SyscallResult::Return(value) => view.set_syscall_return(value),
        SyscallResult::CloneReturn { value, .. } => view.set_syscall_return(value),
        SyscallResult::Error(errno) => view.set_syscall_error(errno),
        SyscallResult::NoReturn
        | SyscallResult::ExecCommitted
        | SyscallResult::SigreturnRestored
        | SyscallResult::SigreturnContextRestored => return None,
    }

    const RV64_ECALL_INSN_BYTES: usize = 4;
    view.set_pc(VirtAddr(
        view.view().pc.0.wrapping_add(RV64_ECALL_INSN_BYTES),
    ));
    emit_direct_sigprocmask_detail_duration(
        req.nr,
        b"debug.trap.direct_sigprocmask.writeback_ns",
        post_start,
    );
    emit_debug_counter(b"debug.trap.direct_syscall", req.nr as i64);
    if needs_reschedule {
        emit_debug_counter(b"debug.trap.direct_wake_handoff", req.nr as i64);
        let handoff_start = direct_sigprocmask_detail_now(req.nr);
        let outcome = trap_handoff::hand_off_timer_preempt(hart, view);
        emit_direct_sigprocmask_detail_duration(
            req.nr,
            b"debug.trap.direct_sigprocmask.wake_handoff_ns",
            handoff_start,
        );
        emit_direct_sigprocmask_detail_duration(
            req.nr,
            b"debug.trap.direct_sigprocmask.total_ns",
            direct_total_start,
        );
        return Some(trap_handoff::timer_preempt_outcome_to_trap_action(&outcome));
    }
    emit_direct_sigprocmask_detail_duration(
        req.nr,
        b"debug.trap.direct_sigprocmask.total_ns",
        direct_total_start,
    );
    Some(TrapAction::Resume)
}

pub(crate) fn direct_trap_syscall_needs_wake_handoff(
    req: &trap_handoff::SyscallRequest,
    result: &SyscallResult,
) -> bool {
    crate::thread_future::syscall_return_may_publish_wake_handoff(req, result)
}

fn direct_syscall_preconditions(
    nr: u64,
    payload: &tx_subsystems::thread_runtime::ThreadPayload,
    process: &tx_subsystems::process::ProcessIdentity,
) -> bool {
    use tx_shims::linux_syscall::numbers::{
        NR_CLOCK_GETTIME, NR_GETEGID, NR_GETEUID, NR_GETGID, NR_GETPID, NR_GETTID, NR_GETTIMEOFDAY,
        NR_GETUID,
    };
    match nr {
        NR_RT_SIGPROCMASK => {
            payload.pending().snapshot() == 0
                && process.group_pending_snapshot() == 0
                && payload.interrupt_summary() == tx_subsystems::signal::InterruptSummary::EMPTY
        }
        NR_SET_TID_ADDRESS => {
            payload.interrupt_summary() == tx_subsystems::signal::InterruptSummary::EMPTY
        }
        // Direct query lane (getpid-class + clock reads): only bypass the
        // run_thread AST checkpoint when no signal work is pending, so
        // delivery timing is identical to the slow path.
        NR_GETPID | NR_GETTID | NR_GETUID | NR_GETEUID | NR_GETGID | NR_GETEGID
        | NR_CLOCK_GETTIME | NR_GETTIMEOFDAY => {
            payload.pending().snapshot() == 0
                && process.group_pending_snapshot() == 0
                && payload.interrupt_summary() == tx_subsystems::signal::InterruptSummary::EMPTY
        }
        _ => true,
    }
}

fn emit_debug_counter(name: &[u8], value: i64) {
    if !cfg!(tx_thread_roundtrip_metrics) {
        return;
    }
    if let Some(observer) = tx_observe::current() {
        observer.counter(
            tx_observe::EventNameId::from_raw(tx_observe::fnv1a32(name)),
            value,
        );
        tx_observe::dump_registered_if_requested();
    }
}

#[cfg(tx_sigprocmask_detail_metrics)]
fn direct_sigprocmask_detail_now(nr: u64) -> u64 {
    if nr == NR_RT_SIGPROCMASK {
        tx_observe::clock_now_ns()
    } else {
        0
    }
}

#[cfg(not(tx_sigprocmask_detail_metrics))]
fn direct_sigprocmask_detail_now(_nr: u64) -> u64 {
    0
}

#[cfg(tx_sigprocmask_detail_metrics)]
fn emit_direct_sigprocmask_detail_duration(nr: u64, name: &[u8], start_ns: u64) {
    if nr != NR_RT_SIGPROCMASK {
        return;
    }
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
fn emit_direct_sigprocmask_detail_duration(_nr: u64, _name: &[u8], _start_ns: u64) {}

#[cfg(tx_sigprocmask_detail_metrics)]
fn emit_direct_sigprocmask_detail_value(nr: u64, name: &[u8], value: i64) {
    if nr != NR_RT_SIGPROCMASK {
        return;
    }
    if let Some(observer) = tx_observe::current() {
        observer.counter(
            tx_observe::EventNameId::from_raw(tx_observe::fnv1a32(name)),
            value,
        );
        tx_observe::dump_registered_if_requested();
    }
}

#[cfg(not(tx_sigprocmask_detail_metrics))]
fn emit_direct_sigprocmask_detail_value(_nr: u64, _name: &[u8], _value: i64) {}

fn log_page_fault_handoff_failure<P: TxPlatform>(
    hart: usize,
    outcome: &trap_handoff::HandoffOutcome,
) {
    tx_hal::console_write_str::<P>("txkernel:");
    tx_hal::console_write_str::<P>(P::BOARD);
    tx_hal::console_write_str::<P>(":trap-handoff:pf:");
    match outcome {
        trap_handoff::HandoffOutcome::Resolved => {
            tx_hal::console_write_str::<P>("resolved");
        }
        trap_handoff::HandoffOutcome::NoActivePayload => {
            tx_hal::console_write_str::<P>("no-active-payload");
        }
        trap_handoff::HandoffOutcome::NoActiveRequest => {
            tx_hal::console_write_str::<P>("no-active-request");
        }
        trap_handoff::HandoffOutcome::SlotError(err) => {
            tx_hal::console_write_str::<P>("slot-error:");
            match err {
                boot_runtime::userspace::UserspaceRunError::Busy(_) => {
                    tx_hal::console_write_str::<P>("busy");
                }
                boot_runtime::userspace::UserspaceRunError::NoActiveRequest => {
                    tx_hal::console_write_str::<P>("no-active-request");
                }
                boot_runtime::userspace::UserspaceRunError::StaleRequest { .. } => {
                    tx_hal::console_write_str::<P>("stale-request");
                }
                boot_runtime::userspace::UserspaceRunError::AlreadyResolved(_) => {
                    tx_hal::console_write_str::<P>("already-resolved");
                }
                boot_runtime::userspace::UserspaceRunError::NotRunning(_) => {
                    tx_hal::console_write_str::<P>("not-running");
                }
                boot_runtime::userspace::UserspaceRunError::RequestIdExhausted => {
                    tx_hal::console_write_str::<P>("request-id-exhausted");
                }
            }
        }
    }
    tx_hal::console_write_str::<P>(":hart=");
    write_usize::<P>(hart);
    let (last_set, last_clear, set_count, clear_count) =
        tx_subsystems::thread_runtime::userspace_payload_trace_counters();
    tx_hal::console_write_str::<P>(":last-set=");
    write_u64::<P>(last_set);
    tx_hal::console_write_str::<P>(":last-clear=");
    write_u64::<P>(last_clear);
    tx_hal::console_write_str::<P>(":sets=");
    write_u64::<P>(set_count);
    tx_hal::console_write_str::<P>(":clears=");
    write_u64::<P>(clear_count);
    tx_hal::console_write_str::<P>("\n");
}

fn write_usize<P: TxPlatform>(value: usize) {
    write_u64::<P>(value as u64);
}

fn write_u64<P: TxPlatform>(value: u64) {
    if value == 0 {
        tx_hal::console_write_str::<P>("0");
        return;
    }

    let mut digits = [0u8; 20];
    let mut n = value;
    let mut idx = digits.len();
    while n > 0 {
        idx -= 1;
        digits[idx] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    let s = core::str::from_utf8(&digits[idx..]).unwrap_or("");
    tx_hal::console_write_str::<P>(s);
}
