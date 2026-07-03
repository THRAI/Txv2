//! P3-S2 (D13): socket implementation of the VFS [`FileOps`] trait.
//!
//! `read(fd)`/`write(fd)` on a socket are POSIX-equivalent to
//! `recv(fd, buf, len, 0)`/`send(fd, buf, len, 0)`, so the ops delegate
//! straight to the net byte steps (`step_recv_kernel_bytes` /
//! `step_send_kernel_bytes`) — NOT back through the syscall layer. The
//! file's O_NONBLOCK translates to `MSG_DONTWAIT`. Blocking behaviour
//! rides the returned `Yield` shapes exactly like the pipe arm; the
//! syscall generic await loop resolves them.

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
}
