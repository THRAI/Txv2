//! Poll-driven hardware RX ingest helper.
//!
//! This is the Phase F bridge before the full reactor/IRQ runtime lands:
//! board or driver code can call into this step after a UART readable event,
//! or from a polling loop, and the bytes will be fed through the tty line
//! discipline exactly like future IRQ-driven ingest.

use alloc::vec;

use tx_substrate::zone::Cap;

use crate::execution::{Errno, Guard, StepOutcome};
use crate::tty::checks::require_live_tty;
use crate::tty::execution::{step_ingest, IngestOutcome};
use crate::tty::structure::{TtyIdentity, TtyTransport};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HardwarePollOutcome {
    pub bytes_read: usize,
    pub ingest: IngestOutcome,
}

/// Read up to `max_bytes` from a hardware-backed tty transport and feed the
/// result into `step_ingest`.
pub fn step_poll_hardware_input(
    tty: &Cap<TtyIdentity>,
    max_bytes: usize,
    guard: &Guard<'_>,
) -> StepOutcome<HardwarePollOutcome> {
    if max_bytes == 0 {
        return StepOutcome::Done(HardwarePollOutcome::default());
    }

    let payload = match require_live_tty(tty, guard) {
        Ok(payload) => payload,
        Err(err) => return StepOutcome::Err(err),
    };

    let binding = match &payload.transport {
        TtyTransport::Hardware { binding } => *binding,
        TtyTransport::Pty { .. } => return StepOutcome::Err(Errno::EINVAL),
    };

    let mut bytes = vec![0u8; max_bytes];
    match binding.ops.read(&mut bytes, guard) {
        StepOutcome::Done(read) | StepOutcome::Advanced(read) => {
            let read = read.min(bytes.len());
            if read == 0 {
                return StepOutcome::Done(HardwarePollOutcome::default());
            }
            bytes.truncate(read);
            match step_ingest(tty, &bytes, guard) {
                StepOutcome::Done(ingest) | StepOutcome::Advanced(ingest) => {
                    StepOutcome::Done(HardwarePollOutcome {
                        bytes_read: read,
                        ingest,
                    })
                }
                StepOutcome::AdvancedThenBlocked(ingest, wait) => StepOutcome::AdvancedThenBlocked(
                    HardwarePollOutcome {
                        bytes_read: read,
                        ingest,
                    },
                    wait,
                ),
                StepOutcome::Blocked(wait) => StepOutcome::Blocked(wait),
                StepOutcome::Err(err) => StepOutcome::Err(err),
            }
        }
        StepOutcome::AdvancedThenBlocked(read, wait) => {
            let read = read.min(bytes.len());
            if read == 0 {
                return StepOutcome::Blocked(wait);
            }
            bytes.truncate(read);
            match step_ingest(tty, &bytes, guard) {
                StepOutcome::Done(ingest) | StepOutcome::Advanced(ingest) => {
                    StepOutcome::AdvancedThenBlocked(
                        HardwarePollOutcome {
                            bytes_read: read,
                            ingest,
                        },
                        wait,
                    )
                }
                StepOutcome::AdvancedThenBlocked(ingest, _) => StepOutcome::AdvancedThenBlocked(
                    HardwarePollOutcome {
                        bytes_read: read,
                        ingest,
                    },
                    wait,
                ),
                StepOutcome::Blocked(_) => StepOutcome::Blocked(wait),
                StepOutcome::Err(err) => StepOutcome::Err(err),
            }
        }
        StepOutcome::Blocked(wait) => StepOutcome::Blocked(wait),
        StepOutcome::Err(err) => StepOutcome::Err(err),
    }
}
