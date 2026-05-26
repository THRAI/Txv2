use tx_hal::{
    CpuId, FaultInfo, IpiKind, IrqHandled, KernelTrapSink, PercpuIf, SmpIf, TrapAction,
    TrapFrameMut, TxPlatform,
};

use crate::{adapter::boot_runtime, trap_handoff};

pub struct KernelTrapDispatcher;

impl<P: TxPlatform> KernelTrapSink<P> for KernelTrapDispatcher {
    fn on_page_fault(view: TrapFrameMut<'_>, fault: FaultInfo) -> TrapAction {
        // Kernel-mode page faults: keep the existing terminate
        // policy; only user-mode faults can be handed off to a
        // userspace-run wait.
        if !fault.from_user {
            if fault.instruction && fault.address.0 == 0 && <P as SmpIf>::online_cpu_count() > 1 {
                <P as SmpIf>::park_this_cpu();
            }
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

    fn on_syscall(view: TrapFrameMut<'_>) -> TrapAction {
        // Phase 1: translate, snapshot context into the active
        // payload, resolve the userspace-run wait, and reschedule.
        // No trap-frame writeback (Plan B); the userspace-entry
        // shim drains `pending_syscall_return` and writes
        // `set_syscall_return` / `set_syscall_error` into the
        // *fresh* trap frame before `enter_userspace`.
        let req = trap_handoff::translate_syscall::<P>(&view.view());
        let hart = <P as PercpuIf>::current_cpu_id().0;
        let outcome = trap_handoff::hand_off_syscall(hart, &view, req);
        trap_handoff::outcome_to_trap_action(&outcome)
    }

    fn on_timer_interrupt(cpu: CpuId, view: TrapFrameMut<'_>) -> TrapAction {
        P::cancel_deadline();
        // Update the global VVAR page with current time, but only if the
        // vDSO image was successfully mapped during boot. Without this
        // guard a timer fires before `init_vdso()` runs (or after it
        // failed silently) and `vvar_page()` dereferences the still-null
        // `VVAR_PTR`, panicking with a store/AMO page fault at scause=15.
        if tx_subsystems::vdso::vdso_available() {
            let mono_ns = P::read_ns();
            let sec = mono_ns / 1_000_000_000;
            let nsec = mono_ns % 1_000_000_000;
            tx_subsystems::vdso::vvar_page().update((sec, nsec), (sec, nsec));
        }

        if view.view().previous_mode == tx_hal::TrapPreviousMode::User {
            let hart = <P as PercpuIf>::current_cpu_id().0;
            let outcome = trap_handoff::hand_off_timer_preempt(hart, &view);
            if matches!(outcome, trap_handoff::TimerPreemptOutcome::Preempted) {
                crate::init::mark_boot_reactor_userspace_preempt(cpu);
            }
            return trap_handoff::timer_preempt_outcome_to_trap_action(&outcome);
        }

        TrapAction::Resume
    }

    fn on_external_irq(_cpu: CpuId) -> TrapAction {
        let irq = P::claim();
        if irq == 0 {
            return TrapAction::Resume;
        }

        let handled = P::dispatch_irq(irq);
        P::complete(irq);

        match handled {
            IrqHandled::Wake => TrapAction::Reschedule,
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

    fn on_illegal_or_sync_fault(_view: TrapFrameMut<'_>, _fault: FaultInfo) -> TrapAction {
        TrapAction::Terminate
    }
}

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
