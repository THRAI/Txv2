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
// SessionPgrp
// ---------------------------------------------------------------------------

/// Session + foreground-pgrp binding for a TTY.
///
/// Carries both raw IDs (for fast comparison without an epoch guard)
/// and typed `Weak` references to the owning process subsystem
/// entities. Typed refs are populated by [`SessionPgrp::from_typed`]
/// (used once a real `Cap<Session>` / `Cap<ProcessGroup>` is in hand)
/// and left `None` for the legacy [`SessionPgrp::from_raw_ids`] path
/// that the TTY tests exercise.
///
/// Typed refs unlock pgrp-targeted signal delivery: callers can
/// upgrade via [`SessionPgrp::upgrade_foreground_pgrp`] and pass the
/// resulting `Cap<ProcessGroup>` to `signal::step_kill_pgrp`.
#[derive(Clone, Copy, Debug)]
pub struct SessionPgrp {
    pub session_id: u32,
    pub session_leader_pgid: u32,
    pub foreground_pgid: u32,
    pub session: Option<tx_substrate::zone::Weak<crate::process::structure::Session>>,
    pub foreground_pgrp: Option<tx_substrate::zone::Weak<crate::process::structure::ProcessGroup>>,
}

impl PartialEq for SessionPgrp {
    fn eq(&self, other: &Self) -> bool {
        // Typed Weak refs intentionally not compared: tests construct
        // bindings via from_raw_ids (no typed refs) and assert equality
        // on the id triplet. Two bindings with the same ids but
        // different (or missing) Weak refs are operationally
        // equivalent for the day-1 surface.
        self.session_id == other.session_id
            && self.session_leader_pgid == other.session_leader_pgid
            && self.foreground_pgid == other.foreground_pgid
    }
}

impl Eq for SessionPgrp {}

impl SessionPgrp {
    /// Construct a binding from raw POSIX ids only. Typed `Weak` refs
    /// are left `None`; callers that need pgrp-targeted signal
    /// delivery must use [`Self::from_typed`] or migrate later.
    pub const fn from_raw_ids(
        session_id: u32,
        session_leader_pgid: u32,
        foreground_pgid: u32,
    ) -> Self {
        Self {
            session_id,
            session_leader_pgid,
            foreground_pgid,
            session: None,
            foreground_pgrp: None,
        }
    }

    /// Construct a binding from real process-subsystem caps. Caches
    /// the ids out of `session.sid` and `foreground_pgrp.pgid`, then
    /// downgrades to `Weak` refs so the TTY does not retain the
    /// session or pgrp itself.
    pub fn from_typed(
        session: &tx_substrate::zone::Cap<crate::process::structure::Session>,
        foreground_pgrp: &tx_substrate::zone::Cap<crate::process::structure::ProcessGroup>,
    ) -> Self {
        Self {
            session_id: session.sid.0,
            session_leader_pgid: session.sid.0,
            foreground_pgid: foreground_pgrp.pgid.0,
            session: Some(session.downgrade()),
            foreground_pgrp: Some(foreground_pgrp.downgrade()),
        }
    }

    /// Upgrade the typed `Weak<Session>` to a strong `Cap<Session>` if
    /// the binding was constructed from a real session and that
    /// session is still alive. Returns `None` for legacy raw-id
    /// bindings or when the session has been dropped.
    pub fn upgrade_session(
        &self,
    ) -> Option<tx_substrate::zone::Cap<crate::process::structure::Session>> {
        let weak = self.session.as_ref()?;
        let guard = tx_substrate::epoch::guard();
        weak.upgrade(&guard)
    }

    /// Upgrade the typed `Weak<ProcessGroup>` for the foreground pgrp
    /// to a strong `Cap<ProcessGroup>` if alive. Returns `None` for
    /// legacy raw-id bindings or when the pgrp has been dropped.
    pub fn upgrade_foreground_pgrp(
        &self,
    ) -> Option<tx_substrate::zone::Cap<crate::process::structure::ProcessGroup>> {
        let weak = self.foreground_pgrp.as_ref()?;
        let guard = tx_substrate::epoch::guard();
        weak.upgrade(&guard)
    }
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

    /// Convenience: build a typed binding from real process-subsystem
    /// caps and install it. Equivalent to
    /// `bind_session_pgrp(SessionPgrp::from_typed(session, fg))`.
    pub fn bind_session_pgrp_typed(
        &self,
        session: &tx_substrate::zone::Cap<crate::process::structure::Session>,
        foreground_pgrp: &tx_substrate::zone::Cap<crate::process::structure::ProcessGroup>,
    ) -> Option<SessionPgrp> {
        self.bind_session_pgrp(SessionPgrp::from_typed(session, foreground_pgrp))
    }

    /// If the current binding carries a typed foreground-pgrp `Weak`,
    /// upgrade it to a `Cap<ProcessGroup>`. Returns `None` for legacy
    /// raw-id bindings, when no binding is installed, or when the
    /// pgrp has been dropped.
    ///
    /// Used by signal-fanout paths that want to call
    /// `signal::step_kill_pgrp` against the foreground pgrp.
    pub fn foreground_pgrp_cap(
        &self,
    ) -> Option<tx_substrate::zone::Cap<crate::process::structure::ProcessGroup>> {
        self.session_pgrp.snapshot()?.upgrade_foreground_pgrp()
    }

    pub fn clear_session_pgrp(&self) -> Option<SessionPgrp> {
        self.session_pgrp.swap(None)
    }
}
