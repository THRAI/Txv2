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

    fn on_timer_interrupt(_cpu: CpuId) -> TrapAction {
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
