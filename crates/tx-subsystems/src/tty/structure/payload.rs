//! TTY payload entity: dropped on hangup.
//!
//! Phase B skeleton. `TtyPayload` holds the live I/O state. It is wrapped in a
//! `PayloadCap` stored on `TtyIdentity::payload`; when the master closes or
//! carrier drops, the `PayloadCap` is cleared and the payload is reclaimed.
//!
//! # Staging types
//!
//! * `termios` field uses a staging `AtomicSlot<Termios>` instead of the final
//!   `AtomicSlot<Arc<Termios>>`. This preserves the publish-then-observe shape
//!   from the design without depending on a shared Arc-based primitive yet.

use core::cell::UnsafeCell;
use core::sync::atomic::AtomicBool;
use core::sync::atomic::AtomicU64;

use tx_substrate::zone::Cap;
use tx_substrate::SpinMutex;

use crate::device::CharDeviceBinding;
use crate::tty::ldisc::on_termios_changed;
use crate::tty::ldisc::state::LdiscState;
use crate::tty::structure::ring::TtyRing;
use crate::tty::structure::termios::Termios;

use super::identity::TtyIdentity;
use tx_substrate::AtomicSlot;

// ---------------------------------------------------------------------------
// Ring capacities
// ---------------------------------------------------------------------------

/// Bytes buffered in the input path (cooked or raw reads from the process side).
///
/// Kept below one zone slab page in Phase C because `TtyPayload` is zone
/// allocated inline. Larger buffers should move to separately allocated ring
/// pages once the backing allocator shape is finalized.
pub const INPUT_CAP: usize = 1024;

/// Bytes buffered in the output path (writes from the process side, or
/// bytes coming in from a pty master).
pub const OUTPUT_CAP: usize = 1024;

const INGEST_LINEARIZER_CAP: usize = 4;

// ---------------------------------------------------------------------------
// TtyTransport
// ---------------------------------------------------------------------------

/// How bytes leave / enter the TTY at the hardware or peer level.
///
/// Phase B: variants are declared; actual byte movement is Phase C+.
pub enum TtyTransport {
    /// Backed by a statically registered character device (UART, virtio-console,
    /// …).  The binding is `'static` because hardware tables live for the whole
    /// kernel lifetime.
    Hardware { binding: &'static CharDeviceBinding },
    /// pty: `peer` is the other side of the pair (master ↔ slave).
    ///
    /// Phase E will fill in the pty registry and openpty step.
    Pty {
        /// The peer identity cap.  Master holds a `Cap<TtyIdentity>` pointing
        /// at the slave, and vice-versa.
        peer: Cap<TtyIdentity>,
    },
}

// ---------------------------------------------------------------------------
// TtyPayload
// ---------------------------------------------------------------------------

/// Live state for an open TTY.  Dropped on hangup.
///
/// # Concurrency notes (TTY.md §3.5)
///
/// * `ldisc_state` is accessed only from the *ingest linearizer* (Phase C).
///   `UnsafeCell` is used because the linearizer guarantees single-writer.
/// * `input_queue` and `output_queue` are behind `SpinMutex` for Phase B.
///   Phase C may relax this with a lock-free ring once the ingest design is
///   finalised.
/// * `termios` uses a staging published slot. Readers snapshot it without
///   holding queue locks; future work can swap the slot payload to `Arc<Termios>`
///   without changing the execution-step shape.
/// * `window_size` is a packed `u64` (`ws_row:ws_col:ws_xpixel:ws_ypixel`)
///   atomically read/written using `Winsize::to_u64()` / `Winsize::from_u64()`.
pub struct TtyPayload {
    /// Current terminal settings.
    ///
    /// TODO(Phase G): replace `AtomicSlot<Termios>` with
    /// `AtomicSlot<Arc<Termios>>` once the shared Arc-based primitive exists.
    pub(crate) termios: AtomicSlot<Termios>,

    /// Packed window size: `(ws_row as u64) << 48 | (ws_col as u64) << 32
    /// | (ws_xpixel as u64) << 16 | ws_ypixel as u64`.
    ///
    /// Updated by TIOCSWINSZ, read by TIOCGWINSZ.  See `Winsize::to_u64()`.
    pub window_size: AtomicU64,

    /// N_TTY line discipline state.
    ///
    /// `UnsafeCell` because the ingest linearizer owns exclusive access
    /// (Phase C).  In Phase B tests this is accessed with `&mut` directly.
    pub ldisc_state: UnsafeCell<LdiscState>,

    /// Bytes available for `read(2)` from the process side.
    pub(crate) input_queue: SpinMutex<TtyRing<INPUT_CAP>>,

    /// Canonical VEOF on an empty line commits a zero-length read.
    ///
    /// The input queue cannot represent that event directly, so Phase C keeps
    /// one coalesced pending EOF marker next to the queue. `step_read` consumes
    /// it as `Done(0)`.
    pub(crate) eof_pending: AtomicBool,

    /// Bytes destined for the transport (UART TX / pty peer ingest).
    pub(crate) output_queue: SpinMutex<TtyRing<OUTPUT_CAP>>,

    /// External-mutator linearizer for ldisc side effects that must be applied
    /// by the ingest-side single writer after a termios publish.
    ingest_linearizer: SpinMutex<IngestLinearizer>,

    /// How bytes flow to/from the physical or virtual device.
    pub transport: TtyTransport,
}

// SAFETY: All interior mutability is either behind SpinMutex<T: Send> or
// behind UnsafeCell where the ingest linearizer enforces single-writer access.
unsafe impl Send for TtyPayload {}
unsafe impl Sync for TtyPayload {}

#[derive(Clone, Copy, Debug)]
enum IngestLinearizerOp {
    TermiosChange { old: Termios, new: Termios },
}

struct IngestLinearizer {
    ops: [Option<IngestLinearizerOp>; INGEST_LINEARIZER_CAP],
    len: usize,
}

impl IngestLinearizer {
    const fn new() -> Self {
        Self {
            ops: [const { None }; INGEST_LINEARIZER_CAP],
            len: 0,
        }
    }

    fn push(&mut self, op: IngestLinearizerOp) {
        if self.len < self.ops.len() {
            self.ops[self.len] = Some(op);
            self.len += 1;
            return;
        }

        // Staging overflow fallback: keep the oldest pre-change state from the
        // final queued op and publish the newest post-change state.
        if let (
            Some(IngestLinearizerOp::TermiosChange {
                old: previous_old, ..
            }),
            IngestLinearizerOp::TermiosChange { new, .. },
        ) = (self.ops[self.len - 1], op)
        {
            self.ops[self.len - 1] = Some(IngestLinearizerOp::TermiosChange {
                old: previous_old,
                new,
            });
        }
    }

    fn take_all(&mut self) -> ([Option<IngestLinearizerOp>; INGEST_LINEARIZER_CAP], usize) {
        let ops = self.ops;
        let len = self.len;
        *self = Self::new();
        (ops, len)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LinearizerApplyOutcome {
    pub readable_fired: bool,
    pub writable_fired: bool,
}

impl TtyPayload {
    // -----------------------------------------------------------------------
    // Accessors (used by Phase C execution steps: step_read, step_write,
    // step_ingest, step_ioctl_tcsetattr_commit).
    //
    // Closure-based API avoids exposing SpinMutexGuard in public signatures.
    // -----------------------------------------------------------------------

    /// Run `f` with a shared reference to the current termios.
    pub fn with_termios<R>(&self, f: impl FnOnce(&Termios) -> R) -> R {
        let termios = self.termios.snapshot().unwrap_or(Termios::zeroed());
        f(&termios)
    }

    pub fn publish_termios(&self, new_termios: Termios) -> Termios {
        self.termios
            .swap(Some(new_termios))
            .unwrap_or(Termios::zeroed())
    }

    pub fn queue_termios_change(&self, old: Termios, new: Termios) {
        self.ingest_linearizer
            .lock()
            .push(IngestLinearizerOp::TermiosChange { old, new });
    }

    pub fn apply_ingest_linearizer(&self) -> LinearizerApplyOutcome {
        let (ops, len) = self.ingest_linearizer.lock().take_all();
        if len == 0 {
            return LinearizerApplyOutcome::default();
        }

        let mut outcome = LinearizerApplyOutcome::default();
        self.with_input_queue(|input_queue| {
            self.with_output_queue(|output_queue| {
                for op in ops.into_iter().take(len).flatten() {
                    match op {
                        IngestLinearizerOp::TermiosChange { old, new } => {
                            let input_len_before = input_queue.len();
                            let output_space_before = output_queue.space();
                            // SAFETY: the ingest linearizer owns these deferred
                            // side effects; callers must serialize tty steps.
                            let state = unsafe { &mut *self.ldisc_state.get() };
                            on_termios_changed(state, &old, &new, input_queue, output_queue);
                            if input_queue.len() > input_len_before {
                                outcome.readable_fired = true;
                            }
                            if output_queue.space() != output_space_before {
                                outcome.writable_fired = true;
                            }
                        }
                    }
                }
            });
        });
        outcome
    }

    /// Run `f` with exclusive access to the input queue.
    pub fn with_input_queue<R>(&self, f: impl FnOnce(&mut TtyRing<INPUT_CAP>) -> R) -> R {
        f(&mut self.input_queue.lock())
    }

    /// Run `f` with exclusive access to the output queue.
    pub fn with_output_queue<R>(&self, f: impl FnOnce(&mut TtyRing<OUTPUT_CAP>) -> R) -> R {
        f(&mut self.output_queue.lock())
    }

    // -----------------------------------------------------------------------
    // Constructors
    // -----------------------------------------------------------------------

    /// Construct a payload for a hardware-backed TTY with cooked defaults.
    pub fn new_hardware(binding: &'static CharDeviceBinding) -> Self {
        Self {
            termios: {
                let slot = AtomicSlot::empty();
                slot.store(Some(Termios::default_cooked()));
                slot
            },
            window_size: AtomicU64::new(0),
            ldisc_state: UnsafeCell::new(LdiscState::new()),
            input_queue: SpinMutex::new(TtyRing::new()),
            eof_pending: AtomicBool::new(false),
            output_queue: SpinMutex::new(TtyRing::new()),
            ingest_linearizer: SpinMutex::new(IngestLinearizer::new()),
            transport: TtyTransport::Hardware { binding },
        }
    }

    /// Construct a payload for a pty slave, pointing back at the master identity.
    pub fn new_pty(peer: Cap<TtyIdentity>) -> Self {
        Self {
            termios: {
                let slot = AtomicSlot::empty();
                slot.store(Some(Termios::default_cooked()));
                slot
            },
            window_size: AtomicU64::new(0),
            ldisc_state: UnsafeCell::new(LdiscState::new()),
            input_queue: SpinMutex::new(TtyRing::new()),
            eof_pending: AtomicBool::new(false),
            output_queue: SpinMutex::new(TtyRing::new()),
            ingest_linearizer: SpinMutex::new(IngestLinearizer::new()),
            transport: TtyTransport::Pty { peer },
        }
    }

    /// Construct a payload for a pty master, pointing at the slave identity.
    ///
    /// Masters use `default_pty_master()` termios (raw, no echo).
    pub fn new_pty_master(peer: Cap<TtyIdentity>) -> Self {
        Self {
            termios: {
                let slot = AtomicSlot::empty();
                slot.store(Some(Termios::default_pty_master()));
                slot
            },
            window_size: AtomicU64::new(0),
            ldisc_state: UnsafeCell::new(LdiscState::new()),
            input_queue: SpinMutex::new(TtyRing::new()),
            eof_pending: AtomicBool::new(false),
            output_queue: SpinMutex::new(TtyRing::new()),
            ingest_linearizer: SpinMutex::new(IngestLinearizer::new()),
            transport: TtyTransport::Pty { peer },
        }
    }
}

// ---------------------------------------------------------------------------
// Phase B verification tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::{CharDeviceBinding, CharDeviceOps, DevT};
    use crate::execution::{Guard, StepOutcome};
    use crate::tty::structure::termios::{ICANON, ISIG};

    struct NoopOps;

    impl CharDeviceOps for NoopOps {
        fn read(&self, _out: &mut [u8], _guard: &Guard<'_>) -> StepOutcome<usize> {
            StepOutcome::Done(0)
        }

        fn write(&self, _bytes: &[u8], _guard: &Guard<'_>) -> StepOutcome<usize> {
            StepOutcome::Done(0)
        }
    }

    static NOOP_OPS: NoopOps = NoopOps;
    static NOOP_BINDING: CharDeviceBinding = CharDeviceBinding {
        devt: DevT::new(4, 0),
        name: "ttyS0",
        ops: &NOOP_OPS,
    };

    #[test]
    fn hardware_payload_starts_with_empty_queues() {
        let payload = TtyPayload::new_hardware(&NOOP_BINDING);
        assert!(payload.with_input_queue(|q| q.is_empty()));
        assert!(payload.with_output_queue(|q| q.is_empty()));
    }

    #[test]
    fn hardware_payload_starts_with_cooked_termios() {
        let payload = TtyPayload::new_hardware(&NOOP_BINDING);
        // default_cooked() sets ICANON and ISIG
        assert!(
            payload.with_termios(|t| t.c_lflag & ICANON != 0),
            "default hardware TTY must be canonical"
        );
        assert!(
            payload.with_termios(|t| t.c_lflag & ISIG != 0),
            "default hardware TTY must have ISIG"
        );
    }
}
