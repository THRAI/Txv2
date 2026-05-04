//! TTY identity entity: persists through hangup.
//!
//! Phase B skeleton — behaviorally inert. Wires and session slot are declared
//! here per TTY.md §2 (BIF-5: wires live on identity, not payload) but the
//! reactor integration is Phase C+.
//!
//! # Staging types
//!
//! Two types below are staging replacements for final interfaces:
//!
//! * [`FixedName`] — replace with the global `FixedName<N>` type once it lands
//!   in `tx-substrate` or a shared utility crate.
//! * [`AtomicSlot`] — replace with the real `AtomicSlot<T>` once the session/
//!   pgrp subsystem exports one.  The current implementation is a
//!   `SpinMutex<Option<T>>` which has identical observable semantics but worse
//!   scalability under high contention.

use tx_substrate::bus::{RawPort, RawQueue};
use tx_substrate::zone::PayloadCap;

use crate::sync::SpinMutex;

use super::payload::TtyPayload;

// ---------------------------------------------------------------------------
// Staging: FixedName<N>
// ---------------------------------------------------------------------------

/// Fixed-capacity ASCII name stored inline without heap allocation.
///
/// TODO(Phase G): replace with the shared `FixedName<N>` type once available.
#[derive(Clone)]
pub struct FixedName<const N: usize> {
    buf: [u8; N],
    len: u8,
}

impl<const N: usize> FixedName<N> {
    /// Create from a byte slice, truncating if longer than `N`.
    pub fn from_bytes(src: &[u8]) -> Self {
        let mut buf = [0u8; N];
        let len = src.len().min(N);
        buf[..len].copy_from_slice(&src[..len]);
        Self {
            buf,
            len: len as u8,
        }
    }

    /// Create from a `&str`, truncating to `N` bytes.
    pub fn from_name(s: &str) -> Self {
        Self::from_bytes(s.as_bytes())
    }

    /// Return the stored bytes as a slice.
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len as usize]
    }
}

// ---------------------------------------------------------------------------
// Staging: AtomicSlot<T>
// ---------------------------------------------------------------------------

/// Single-slot atomic container for an optional value.
///
/// TODO(Phase G): replace with the real `AtomicSlot<T>` once the session/pgrp
/// subsystem defines one.  Current implementation wraps `SpinMutex<Option<T>>`
/// which is correct but not lock-free.
pub struct AtomicSlot<T> {
    inner: SpinMutex<Option<T>>,
}

impl<T> AtomicSlot<T> {
    pub const fn empty() -> Self {
        Self {
            inner: SpinMutex::new(None),
        }
    }

    pub fn store(&self, value: Option<T>) {
        *self.inner.lock() = value;
    }

    pub fn swap(&self, value: Option<T>) -> Option<T> {
        let mut slot = self.inner.lock();
        let old = slot.take();
        *slot = value;
        old
    }

    pub fn with<R, F: FnOnce(Option<&T>) -> R>(&self, f: F) -> R {
        f(self.inner.lock().as_ref())
    }

    pub fn snapshot(&self) -> Option<T>
    where
        T: Clone,
    {
        self.inner.lock().clone()
    }
}

// ---------------------------------------------------------------------------
// SessionPgrp staging
// ---------------------------------------------------------------------------

/// Staging placeholder for the session + foreground-pgrp binding.
///
/// TODO(Phase G): replace with `SessionPgrp { session: Cap<Session>,
/// foreground_pgrp: Cap<ProcessGroup> }` once Process/Session land.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionPgrp {
    pub session_id: u32,
    pub session_leader_pgid: u32,
    pub foreground_pgid: u32,
}

// ---------------------------------------------------------------------------
// TtyKind
// ---------------------------------------------------------------------------

/// Discriminates hardware vs. pty TTY instances.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TtyKind {
    /// Backed by a `CharDeviceBinding` (UART, virtual console, …).
    SerialHardware,
    /// pty master — the controlling end opened via `/dev/ptmx`.
    PtyMaster,
    /// pty slave — the terminal end visible at `/dev/pts/<N>`.
    PtySlave,
}

// ---------------------------------------------------------------------------
// TtyIdentity
// ---------------------------------------------------------------------------

/// Long-lived identity for a TTY instance.
///
/// Outlives hangup: after `TtyPayload` is dropped the identity remains
/// observable so poll/epoll waiters can see `POLLHUP` and session leaders can
/// receive `SIGHUP`.
///
/// # Field notes
///
/// * `session_pgrp` — staging `AtomicSlot<SessionPgrp>` (see module doc).
/// * `input_readable` / `output_writable` — level-triggered readiness wires
///   (BIF-5 from TTY.md §3).
/// * `hangup_port` / `session_ctl_port` — edge-triggered event wires.
/// * `payload` — `None` after hangup.
pub struct TtyIdentity {
    pub kind: TtyKind,
    pub index: u32,
    /// Short human-readable name, e.g. `"ttyS0"` or `"pts/3"`.
    ///
    /// TODO(Phase G): replace `FixedName<16>` with the shared `FixedName<N>`
    /// once available.
    pub name: FixedName<16>,
    /// Current session + foreground pgrp binding.
    ///
    /// TODO(Phase G): replace staging `AtomicSlot<SessionPgrp>` with real type.
    pub session_pgrp: AtomicSlot<SessionPgrp>,
    /// Level-triggered wire: set when `input_queue` is non-empty (or a full
    /// line is available in cooked mode).
    pub input_readable: RawQueue,
    /// Level-triggered wire: set when `output_queue` has space.
    pub output_writable: RawQueue,
    /// Edge-triggered wire: fired once on hangup.
    pub hangup_port: RawPort,
    /// Edge-triggered wire: fired on TIOCSCTTY / TIOCNOTTY / hangup.
    pub session_ctl_port: RawPort,
    /// Live payload. `None` after hangup drops the `PayloadCap`.
    ///
    /// This is a staging slot implemented with a spin mutex. The design wants
    /// an identity-level published payload slot: pty creation publishes both
    /// identities first, then installs peer-linked payloads as commit points;
    /// hangup later clears this slot while identity wires remain observable.
    pub(crate) payload: SpinMutex<Option<PayloadCap<TtyPayload>>>,
}

impl TtyIdentity {
    /// Construct a new identity with sensible defaults.
    ///
    /// `payload` starts as `None`; the caller must assign it after allocating
    /// the payload zone slot.
    pub fn new(kind: TtyKind, index: u32, name: &str) -> Self {
        Self {
            kind,
            index,
            name: FixedName::from_name(name),
            session_pgrp: AtomicSlot::empty(),
            input_readable: RawQueue::new(),
            output_writable: RawQueue::new(),
            hangup_port: RawPort::new(),
            session_ctl_port: RawPort::new(),
            payload: SpinMutex::new(None),
        }
    }

    /// Install or replace the live payload slot.
    pub fn install_payload(&self, payload: PayloadCap<TtyPayload>) {
        *self.payload.lock() = Some(payload);
    }

    /// Clear the live payload slot and return the old payload, if any.
    pub fn take_payload(&self) -> Option<PayloadCap<TtyPayload>> {
        self.payload.lock().take()
    }

    /// Snapshot the live payload slot.
    pub fn live_payload(&self) -> Option<PayloadCap<TtyPayload>> {
        self.payload.lock().clone()
    }

    /// Returns `true` if the payload is still live (no hangup yet).
    pub fn is_live(&self) -> bool {
        self.live_payload().is_some()
    }

    pub fn session_pgrp(&self) -> Option<SessionPgrp> {
        self.session_pgrp.snapshot()
    }

    pub fn bind_session_pgrp(&self, binding: SessionPgrp) -> Option<SessionPgrp> {
        self.session_pgrp.swap(Some(binding))
    }

    pub fn clear_session_pgrp(&self) -> Option<SessionPgrp> {
        self.session_pgrp.swap(None)
    }
}
