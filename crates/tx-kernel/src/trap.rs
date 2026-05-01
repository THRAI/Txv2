use tx_hal::{CpuId, FaultInfo, IpiKind, KernelTrapSink, TrapAction, TrapFrameMut, TxPlatform};

pub struct KernelTrapDispatcher;

impl<P: TxPlatform> KernelTrapSink<P> for KernelTrapDispatcher {
    fn on_page_fault(_view: TrapFrameMut<'_>, _fault: FaultInfo) -> TrapAction {
        TrapAction::Terminate
    }

    fn on_syscall(_view: TrapFrameMut<'_>) -> TrapAction {
        TrapAction::Terminate
    }

    fn on_timer_interrupt(_cpu: CpuId) -> TrapAction {
        P::cancel_deadline();
        TrapAction::Resume
    }

    fn on_external_irq(_cpu: CpuId) -> TrapAction {
        TrapAction::Resume
    }

    fn on_ipi(_cpu: CpuId) -> TrapAction {
        P::ack_ipi(IpiKind::Reschedule);
        TrapAction::Resume
    }

    fn on_illegal_or_sync_fault(_view: TrapFrameMut<'_>, _fault: FaultInfo) -> TrapAction {
        TrapAction::Terminate
    }
}
