use tx_hal::{
    CpuId, FaultInfo, IpiKind, IrqHandled, KernelTrapSink, PercpuIf, TrapAction, TrapFrameMut,
    TxPlatform,
};

use crate::trap_handoff;

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
        trap_handoff::outcome_to_trap_action(&outcome)
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

    fn on_timer_interrupt(_cpu: CpuId, view: TrapFrameMut<'_>) -> TrapAction {
        P::cancel_deadline();
        crate::zones::try_bounded_maintenance_tick();
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
        tx_hal::console_write_str::<P>(":diag:trap:timer mode=");
        tx_hal::console_write_str::<P>(
            if view.view().previous_mode == tx_hal::TrapPreviousMode::User {
                "user"
            } else {
                "kernel"
            },
        );
        tx_hal::console_write_str::<P>(" pc=");
        write_hex::<P>(view.view().pc.0 as u64);
        tx_hal::console_write_str::<P>("\n");
        if view.view().previous_mode == tx_hal::TrapPreviousMode::User {
            let hart = <P as PercpuIf>::current_cpu_id().0;
            tx_hal::console_write_str::<P>(":diag:trap:preempt-slot hart=");
            write_decimal::<P>(hart);
            tx_hal::console_write_str::<P>(" slot=");
            tx_hal::console_write_str::<P>(
                if trap_handoff::current_payload_for_hart(hart).is_some() {
                    "y"
                } else {
                    "n"
                },
            );
            tx_hal::console_write_str::<P>(" slot0=");
            tx_hal::console_write_str::<P>(
                if trap_handoff::current_payload_for_hart(0).is_some() {
                    "y"
                } else {
                    "n"
                },
            );
            tx_hal::console_write_str::<P>("\n");
            let outcome = trap_handoff::hand_off_user_preempt(hart, &view);
            tx_hal::console_write_str::<P>(":diag:trap:preempt outcome=");
            tx_hal::console_write_str::<P>(handoff_outcome_name(&outcome));
            tx_hal::console_write_str::<P>("\n");
            trap_handoff::outcome_to_trap_action(&outcome)
        } else {
            TrapAction::Resume
        }
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

fn handoff_outcome_name(outcome: &trap_handoff::HandoffOutcome) -> &'static str {
    match outcome {
        trap_handoff::HandoffOutcome::Resolved => "resolved",
        trap_handoff::HandoffOutcome::NoActivePayload => "no-payload",
        trap_handoff::HandoffOutcome::NoActiveRequest => "no-request",
        trap_handoff::HandoffOutcome::SlotError(_) => "slot-error",
    }
}

fn write_hex<P: TxPlatform>(value: u64) {
    tx_hal::console_write_str::<P>("0x");
    if value == 0 {
        tx_hal::console_write_str::<P>("0");
        return;
    }
    let mut digits = [0u8; 16];
    let mut n = value;
    let mut idx = digits.len();
    while n > 0 {
        idx -= 1;
        let nibble = (n & 0xf) as u8;
        digits[idx] = if nibble < 10 {
            b'0' + nibble
        } else {
            b'a' + (nibble - 10)
        };
        n >>= 4;
    }
    let s = core::str::from_utf8(&digits[idx..]).unwrap_or("");
    tx_hal::console_write_str::<P>(s);
}

fn write_decimal<P: TxPlatform>(value: usize) {
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
