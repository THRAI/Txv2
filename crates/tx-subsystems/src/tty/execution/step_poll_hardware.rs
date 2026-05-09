//! Poll-driven hardware RX ingest helper.
//!
//! This is the Phase F bridge before the full reactor/IRQ runtime lands:
//! board or driver code can call into this step after a UART readable event,
//! or from a polling loop, and the bytes will be fed through the tty line
//! discipline exactly like future IRQ-driven ingest.

use alloc::vec;

use tx_substrate::zone::Cap;

use crate::execution::{Errno, Guard};
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
///
/// v3-shape: a one-shot `NoProgress` outcome. The pre-v3 flavour
/// surfaced `AdvancedThenBlocked(outcome, wait)` when the driver's
/// `read` returned partial bytes followed by a wait token; in v3 the
/// closed catalog only allows `Done | Yield | Err` for `NoProgress`
/// ops, so we collapse partial-then-blocked into `Done(outcome)` —
/// the caller observes the bytes already ingested and re-enters the
/// step the next time the carrier fires.
pub fn step_poll_hardware_input(
    tty: &Cap<TtyIdentity>,
    max_bytes: usize,
    guard: &Guard<'_>,
) -> tx_substrate::step_v3::StepOutcome<HardwarePollOutcome, tx_substrate::step_v3::NoProgress> {
    use tx_substrate::step_v3::{NoProgress, StepOutcome as V3, YieldShape};

    if max_bytes == 0 {
        return V3::Done(HardwarePollOutcome::default());
    }

    let payload = match require_live_tty(tty, guard) {
        Ok(payload) => payload,
        Err(err) => return V3::Err(err.into()),
    };

    let binding = match &payload.transport {
        TtyTransport::Hardware { binding } => *binding,
        TtyTransport::Pty { .. } => return V3::Err(Errno::EINVAL.into()),
    };

    // Helper: dispatch ingest's v3 outcome up into our v3 outcome.
    let drive_ingest = |bytes: &[u8],
                        read: usize,
                        guard: &Guard<'_>|
     -> tx_substrate::step_v3::StepOutcome<
        HardwarePollOutcome,
        tx_substrate::step_v3::NoProgress,
    > {
        match step_ingest(tty, bytes, guard) {
            V3::Done(ingest) => V3::Done(HardwarePollOutcome {
                bytes_read: read,
                ingest,
            }),
            V3::Continue { .. } => V3::Done(HardwarePollOutcome {
                bytes_read: read,
                ingest: IngestOutcome::default(),
            }),
            V3::Yield {
                progress: _,
                shape: YieldShape::OnCarrier { carrier, interests },
            } => V3::yield_on_carrier(NoProgress, carrier.raw(), interests.raw()),
            V3::Yield { shape, .. } => V3::Yield {
                progress: NoProgress,
                shape,
            },
            V3::Err(e) => V3::Err(e),
        }
    };

    let mut bytes = vec![0u8; max_bytes];
    // `binding.ops.read` is now a v3 `StepOutcome<usize, ByteProgress>`.
    // Map directly:
    // - `Done(n)` → ingest the bytes (or short-circuit at n=0).
    // - `Continue { progress }` → bytes were consumed but the op asked
    //   to continue without waiting; ingest those bytes (the caller's
    //   next poll re-enters the step).
    // - `Yield { progress, shape: OnCarrier }` with non-empty progress
    //   → ingest the bytes and surface the inner outcome (matches the
    //   pre-v3 `AdvancedThenBlocked` collapse). With empty progress the
    //   yield-on-carrier propagates as-is.
    // - `Yield { shape: OnAgent .. }` → unsupported; surface `Err(EIO)`.
    // - `Err(errno)` → propagate.
    match binding.ops.read(&mut bytes, guard) {
        V3::Done(read) => {
            let read = read.min(bytes.len());
            if read == 0 {
                return V3::Done(HardwarePollOutcome::default());
            }
            bytes.truncate(read);
            drive_ingest(&bytes, read, guard)
        }
        V3::Continue { progress } => {
            let read = progress.bytes().min(bytes.len());
            if read == 0 {
                return V3::Done(HardwarePollOutcome::default());
            }
            bytes.truncate(read);
            drive_ingest(&bytes, read, guard)
        }
        V3::Yield {
            progress,
            shape: YieldShape::OnCarrier { carrier, interests },
        } => {
            let read = progress.bytes().min(bytes.len());
            if read == 0 {
                return V3::yield_on_carrier(NoProgress, carrier.raw(), interests.raw());
            }
            bytes.truncate(read);
            drive_ingest(&bytes, read, guard)
        }
        V3::Yield { .. } => V3::Err(Errno::EIO.into()),
        V3::Err(err) => V3::Err(err),
    }
}
