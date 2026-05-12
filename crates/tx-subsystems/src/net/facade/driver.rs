use crate::execution::{Errno, Guard, StepOutcome};
use crate::net::facade::{socket_connect_facade, SocketConnectCapability};
use tx_reactor::wait::WaitProtocol;
use tx_substrate::step_v3::YieldShape;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SocketFacadeDriveMode {
    Nonblocking,
    Waiting { protocol: WaitProtocol },
    Selectable,
}

pub fn drive_socket_nonblocking<T>(
    step: impl FnOnce(&Guard<'_>) -> StepOutcome<T>,
) -> StepOutcome<T> {
    let guard = tx_substrate::epoch::guard();
    match step(&guard) {
        StepOutcome::Done(value) => StepOutcome::Done(value),
        StepOutcome::Continue { .. } => StepOutcome::Err(Errno::EAGAIN),
        StepOutcome::Yield { .. } => StepOutcome::Err(Errno::EAGAIN),
        StepOutcome::Err(errno) => StepOutcome::Err(errno),
    }
}

pub async fn drive_socket_connect_waiting(
    cap: SocketConnectCapability,
    _protocol: WaitProtocol,
) -> StepOutcome<()> {
    loop {
        let outcome = {
            let guard = tx_substrate::epoch::guard();
            socket_connect_facade(cap.clone(), &guard)
        };

        match outcome {
            StepOutcome::Done(()) => return StepOutcome::Done(()),
            StepOutcome::Err(errno) => return StepOutcome::Err(errno),
            StepOutcome::Continue { .. } => {}
            StepOutcome::Yield { shape, .. } => {
                if let Some(future) = wait_on_yield_shape(shape) {
                    let _ = future.await;
                }
            }
        }
    }
}

fn wait_on_yield_shape(shape: YieldShape) -> Option<crate::wait_source::RegisteredWaitFuture> {
    match shape {
        YieldShape::OnWaitSource { source, interests } => {
            let token = crate::execution::WaitToken::new(source.raw(), interests.raw());
            crate::wait_source::wait_on_token(token)
        }
        YieldShape::OnAgent { .. } | YieldShape::OnTimer { .. } => None,
    }
}
