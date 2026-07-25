use crate::execution::{Errno, Guard, StepOutcome};
use crate::net::facade::{socket_connect_facade, SocketConnectCapability};
use crate::net::notification::registered_wait_for_yield;
use tx_reactor::wait::WaitProtocol;

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
                if let Some(future) = registered_wait_for_yield(shape) {
                    let _ = future.await;
                }
            }
        }
    }
}
