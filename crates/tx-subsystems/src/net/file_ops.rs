//! Socket implementation of the VFS [`FileOps`] interface.

use tx_substrate::zone::Cap;

use crate::adapter::step_engine::{ByteProgress, StepOutcome};
use crate::device::FileOps;
use crate::execution::{Errno, Guard, WaitToken};
use crate::net::execution::{
    step_poll_ready, step_poll_wait_token, step_recv_kernel_bytes, step_send_kernel_bytes,
};
use crate::net::structure::{SendRecvFlags, SocketIdentity};
use crate::net::PollMask;

fn flags_for(nonblocking: bool) -> SendRecvFlags {
    if nonblocking {
        SendRecvFlags::MSG_DONTWAIT
    } else {
        SendRecvFlags::empty()
    }
}

impl FileOps for Cap<SocketIdentity> {
    fn read(
        &self,
        out: &mut [u8],
        nonblocking: bool,
        guard: &Guard<'_>,
    ) -> StepOutcome<usize, ByteProgress> {
        match step_recv_kernel_bytes(self, out, flags_for(nonblocking), guard) {
            StepOutcome::Done(outcome) => StepOutcome::Done(outcome.bytes),
            StepOutcome::Continue { progress } => StepOutcome::Continue { progress },
            StepOutcome::Yield { progress, shape } => StepOutcome::Yield { progress, shape },
            StepOutcome::Err(errno) => StepOutcome::Err(errno),
        }
    }

    fn write(
        &self,
        bytes: &[u8],
        nonblocking: bool,
        guard: &Guard<'_>,
    ) -> StepOutcome<usize, ByteProgress> {
        step_send_kernel_bytes(self, bytes, flags_for(nonblocking), guard)
    }

    fn poll_mask(&self, guard: &Guard<'_>) -> Result<PollMask, Errno> {
        match step_poll_ready(self, guard) {
            StepOutcome::Done(mask) => Ok(mask),
            StepOutcome::Err(errno) => Err(errno),
            StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => Ok(PollMask::empty()),
        }
    }

    fn poll_wait_token(
        &self,
        interests: PollMask,
        guard: &Guard<'_>,
    ) -> Result<Option<WaitToken>, Errno> {
        match step_poll_wait_token(self, interests, guard) {
            StepOutcome::Done(token) => Ok(token),
            StepOutcome::Err(errno) => Err(errno),
            StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => Ok(None),
        }
    }

    fn on_set_fl_nonblock(&self) {
        self.readiness
            .fire_send(crate::net::structure::SendWireSet::SPACE);
    }

    fn on_last_close(&self, guard: &Guard<'_>) {
        let _ = crate::net::execution::step_socket_close(self, guard);
    }
}
