//! PR-8 public timer surface: [`TimerWheel`], [`TimerGuard`],
//! [`TimerToken`], [`TimerGuardRole`].
//!
//! Per [`docs/progress/decisions/2026-05-11-d6-timerwheel-layering.md`],
//! the role-tagged timer registry lives **below** `tx-reactor` next
//! to the rest of the wake substrate ([`super::mailbox::TaskMailbox`],
//! [`super::wait_source::WaitSource`]). The wheel registers tokens
//! with roles; [`TimerGuard`] is the RAII handle that cancels the
//! registration on drop.
//!
//! The reactor's internal `timer::TimerQueue` — which
//! holds the actual `Waker`s and drives `WaitProtocol::*Timeout`
//! paths — continues to live in `tx-reactor` and is unrelated to
//! this surface.
//!
//! PR-7B's timer-tick → substrate-state-machine glue
//! ([`TimerWheel::install_delegate_timeout`] /
//! [`TimerWheel::fire_due_delegate_timeouts`]) lives here too: both
//! the wheel and [`crate::step::DelegateRegistry`] are substrate
//! types, so the routing call is a same-crate edge.

use alloc::{sync::Arc, vec::Vec};

use crate::step::{Deadline, DelegateRegistry, DelegateTokenId};
use crate::sync::SpinMutex;

/// Opaque identifier for a timer registered on a [`TimerWheel`].
///
/// Issued by [`TimerWheel::install`]. Monotonic per-wheel; never
/// reused — an already-cancelled token id stays cancelled even if
/// later registrations occupy adjacent slots.
///
/// Per `docs/Txv3/03_STEP_MODEL_v2.md` §2.3 the eventual
/// `YieldShape::OnTimer` runtime will resume an op with
/// `ResumeOutcome::TimerExpired(token)` carrying this id; PR-7
/// connects that path. For PR-8 `TimerToken` is published as the
/// stable handle returned by `install`/embedded in `TimerGuard`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct TimerToken(u64);

impl TimerToken {
    /// Construct a `TimerToken` from a raw `u64`. Provided so tests
    /// and PR-7's eventual reconciliation with
    /// [`crate::step::TimerId`] can round-trip the id
    /// without going through [`TimerWheel::install`].
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// The underlying raw id. Useful for log/trace lines and for
    /// bridging to `crate::step::TimerId` until PR-7
    /// collapses the two.
    pub const fn raw(self) -> u64 {
        self.0
    }
}

// PR-7/8: TimerId (agent.rs) and TimerToken (here) are both u64
// wrappers. Once the types are unified (PR-8), these From impls
// become identity conversions or are removed.
impl From<crate::step::TimerId> for TimerToken {
    fn from(id: crate::step::TimerId) -> Self {
        TimerToken(id.raw())
    }
}

impl From<TimerToken> for crate::step::TimerId {
    fn from(token: TimerToken) -> Self {
        crate::step::TimerId::new(token.raw())
    }
}

/// Role of a [`TimerGuard`] registration. Set at install time and
/// immutable for the guard's lifetime. Per
/// `docs/Txv3/07_BLAST_RADIUS.md` §4 row H, the three roles are
/// distinguished so the runtime can route fires and abandonments
/// correctly:
///
/// - [`PrimarySleep`](Self::PrimarySleep): the wait *is* a timer
///   (e.g. `nanosleep`, `clock_nanosleep`). Maps to
///   `YieldShape::OnTimer` (`03_STEP_MODEL_v2.md` §2.3). Firing the
///   timer resumes the op with `ResumeOutcome::TimerExpired`.
/// - [`DeadlineAbort`](Self::DeadlineAbort): a protocol-level
///   timeout attached via `WaitProtocol.deadline`
///   (`03_STEP_MODEL_v2.md` §2.3 last paragraph). Composes
///   uniformly on top of any primary `YieldShape`. Firing aborts
///   the primary wait with `AbortReason::TimedOut`.
/// - [`DelegateTimeout`](Self::DelegateTimeout): an `OnAgent`
///   token's reply-deadline (`05_DELEGATE_v1.md`). PR-7's
///   `DelegateState` consumes this role to mark a token
///   `SENTINEL_TIMED_OUT` when fired.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimerGuardRole {
    /// Primary timer wait — the wait subject itself.
    PrimarySleep,
    /// Protocol-deadline timeout attached over a non-timer primary
    /// wait shape.
    DeadlineAbort,
    /// `OnAgent` token reply deadline.
    DelegateTimeout,
}

/// Internal bookkeeping for a single wheel registration. Held in
/// the wheel's `Vec<Entry>` and looked up by `TimerToken`.
///
/// PR-7B extension: for [`TimerGuardRole::DelegateTimeout`]
/// entries installed via [`TimerWheel::install_delegate_timeout`],
/// the entry also carries the [`DelegateTokenId`] the wheel will
/// pass to [`DelegateRegistry::mark_timed_out`] on fire. Entries
/// installed via the generic [`TimerWheel::install`] keep
/// `delegate_token = None` because the runtime has no
/// per-entry routing target for `PrimarySleep` / `DeadlineAbort`
/// (those wake their bound waiter via a different path).
#[derive(Clone, Copy, Debug)]
struct Entry {
    token: TimerToken,
    deadline: Deadline,
    role: TimerGuardRole,
    /// `Some(id)` only for `DelegateTimeout` entries installed via
    /// [`TimerWheel::install_delegate_timeout`].
    delegate_token: Option<DelegateTokenId>,
}

struct TimerWheelState {
    next_token: u64,
    entries: Vec<Entry>,
}

/// Public timer registry. Subsystems (well, PR-7's `OnAgent`
/// runtime and PR-9+ protocol-deadline plumbing) call
/// [`Self::install`] to schedule a timer with a role, receiving a
/// [`TimerGuard`] whose drop cancels the registration.
///
/// **PR-8 stub mechanics.** The wheel tracks registrations in a
/// `Vec<Entry>` keyed by `TimerToken`. It does *not* yet fire
/// wakeups — PR-7 wires the fire path to a `TaskMailbox`. The
/// surface is intentionally additive so PR-7 can fill in
/// `arm_fire(...)` / `expire_due(...)` without further reshuffling
/// the public API.
///
/// **Why a separate type from the reactor's internal `TimerQueue`.**
/// `TimerQueue` owns `Waker`s and is advanced by the host clock
/// callback; it backs the existing `WaitProtocol::*Timeout` path
/// inside `tx-reactor`. The `TimerWheel` is the *v3 surface*:
/// role-tagged registrations the step model reasons about. PR-7+
/// may consolidate the two; PR-8 publishes the role-shaped surface
/// without touching the wake path.
#[derive(Clone)]
pub struct TimerWheel {
    state: Arc<SpinMutex<TimerWheelState>>,
}

impl TimerWheel {
    /// Construct an empty wheel. Tokens are issued starting at `1`
    /// (`TimerToken::new(0)` is a never-issued sentinel, matching
    /// the convention used by [`super::wait_source::SubscriberId`]).
    pub fn new() -> Self {
        Self {
            state: Arc::new(SpinMutex::new(TimerWheelState {
                next_token: 1,
                entries: Vec::new(),
            })),
        }
    }

    /// Number of currently-armed timer registrations. Includes
    /// timers whose deadline has passed but have not yet been
    /// retired by the (PR-7) fire path.
    pub fn armed_count(&self) -> usize {
        self.state.lock().entries.len()
    }

    /// Install a timer firing at `deadline` with role `role`.
    /// Returns a [`TimerGuard`] whose drop cancels the
    /// registration; the guard carries the issued [`TimerToken`].
    ///
    /// PR-8 publishes the install/cancel handshake; PR-7's
    /// `OnAgent` runtime is the first real user (role
    /// `DelegateTimeout`).
    ///
    /// For the `DelegateTimeout` role with a known token id, prefer
    /// [`Self::install_delegate_timeout`] — that variant tags the
    /// entry with the `DelegateTokenId` so
    /// [`Self::fire_due_delegate_timeouts`] can route fires to the
    /// right `DelegateRegistry::mark_timed_out` callback.
    pub fn install(&self, deadline: Deadline, role: TimerGuardRole) -> TimerGuard {
        let token = {
            let mut state = self.state.lock();
            let raw = state.next_token;
            state.next_token = state.next_token.wrapping_add(1);
            let token = TimerToken(raw);
            state.entries.push(Entry {
                token,
                deadline,
                role,
                delegate_token: None,
            });
            token
        };
        TimerGuard {
            wheel: Some(self.clone_handle()),
            token,
            deadline,
            role,
        }
    }

    /// Install a `DelegateTimeout`-role timer tagged with the
    /// `DelegateTokenId` it should retire on fire. Returns a
    /// [`TimerGuard`] whose drop cancels the registration; the
    /// substrate-side `AgentTokenGuard` is paired with this guard
    /// at the call site (per PR-7B option (b): primitives migrate
    /// down one at a time, so the substrate guard does **not**
    /// own the timer guard directly).
    ///
    /// Tagging the entry lets
    /// [`Self::fire_due_delegate_timeouts`] walk expired
    /// `DelegateTimeout` entries and call
    /// `registry.mark_timed_out(delegate_token)` for each — the
    /// timer-tick → substrate-state-machine glue.
    pub fn install_delegate_timeout(
        &self,
        deadline: Deadline,
        delegate_token: DelegateTokenId,
    ) -> TimerGuard {
        let token = {
            let mut state = self.state.lock();
            let raw = state.next_token;
            state.next_token = state.next_token.wrapping_add(1);
            let token = TimerToken(raw);
            state.entries.push(Entry {
                token,
                deadline,
                role: TimerGuardRole::DelegateTimeout,
                delegate_token: Some(delegate_token),
            });
            token
        };
        TimerGuard {
            wheel: Some(self.clone_handle()),
            token,
            deadline,
            role: TimerGuardRole::DelegateTimeout,
        }
    }

    /// Walk every armed `DelegateTimeout` entry whose deadline is
    /// `<= now` and call `registry.mark_timed_out(delegate_token)`
    /// on each. Retires the matching entries from the wheel
    /// regardless of the transition outcome (the substrate state
    /// machine handles `LateNoOp` correctly per DTOK-3).
    ///
    /// Post-D6 (wheel relocated into `tx-substrate::wake`) the
    /// registry and wheel live in the same crate, so the call to
    /// [`DelegateRegistry::mark_timed_out`] is a direct same-crate
    /// edge — no callback laundering required. **PR-7B stub fire
    /// path**: PR-8B (the eventual wheel-mechanics PR) will fold
    /// this walk into the wheel's primary fire path. Until then,
    /// callers drive it explicitly from the tick handler (today:
    /// tests and any PR-7B integration smoke).
    ///
    /// Returns the number of entries that were retired (regardless
    /// of `TransitionOutcome`).
    pub fn fire_due_delegate_timeouts(&self, now: Deadline, registry: &DelegateRegistry) -> usize {
        let due: Vec<(TimerToken, DelegateTokenId)> = {
            let state = self.state.lock();
            state
                .entries
                .iter()
                .filter_map(|e| match (e.role, e.delegate_token) {
                    (TimerGuardRole::DelegateTimeout, Some(id))
                        if e.deadline.raw() <= now.raw() =>
                    {
                        Some((e.token, id))
                    }
                    _ => None,
                })
                .collect()
        };
        for (timer_token, delegate_token) in &due {
            // Retire the entry first so a re-entrant
            // mark_timed_out callback observing the wheel sees
            // a consistent view.
            self.cancel(*timer_token);
            let _ = registry.mark_timed_out(*delegate_token);
        }
        due.len()
    }

    /// Look up a previously-installed timer by token. Returns
    /// `None` if the token has been cancelled (guard dropped) or
    /// never existed. Diagnostic helper for tests; production
    /// callers should rely on the [`TimerGuard`] they were handed.
    pub fn lookup(&self, token: TimerToken) -> Option<(Deadline, TimerGuardRole)> {
        self.state
            .lock()
            .entries
            .iter()
            .find(|e| e.token == token)
            .map(|e| (e.deadline, e.role))
    }

    fn clone_handle(&self) -> Self {
        Self {
            state: Arc::clone(&self.state),
        }
    }

    fn cancel(&self, token: TimerToken) {
        let mut state = self.state.lock();
        if let Some(idx) = state.entries.iter().position(|e| e.token == token) {
            state.entries.swap_remove(idx);
        }
    }
}

impl Default for TimerWheel {
    fn default() -> Self {
        Self::new()
    }
}

/// RAII handle for a [`TimerWheel`] registration. On drop, cancels
/// the registration on its parent wheel — the standard pattern for
/// "wait-with-timeout" call sites: the driver installs the guard
/// before parking, and the guard's drop on resume retires the
/// timer regardless of whether the wait fired or completed via the
/// primary path.
///
/// Three roles per `07_BLAST_RADIUS.md` §4 row H — see
/// [`TimerGuardRole`]. The role is captured at install time and
/// read back via [`Self::role`] for diagnostic asserts.
///
/// **Drop semantics.** `Drop` always cancels. To deliberately
/// transfer ownership of the registration (e.g. to a longer-lived
/// state machine) call [`Self::forget`]; the caller is then
/// responsible for retiring the token explicitly. This mirrors
/// [`super::wait_source::WaitRegistrationGuard::forget`].
#[must_use = "drop the guard to cancel the timer; binding to _ cancels immediately"]
pub struct TimerGuard {
    /// `None` after [`Self::forget`]; drop becomes a no-op.
    wheel: Option<TimerWheel>,
    token: TimerToken,
    deadline: Deadline,
    role: TimerGuardRole,
}

impl TimerGuard {
    /// The token issued for this registration.
    pub fn token(&self) -> TimerToken {
        self.token
    }

    /// The deadline this timer was installed against.
    pub fn deadline(&self) -> Deadline {
        self.deadline
    }

    /// The role this timer plays in the wait — set at install
    /// time, immutable for the guard's lifetime.
    pub fn role(&self) -> TimerGuardRole {
        self.role
    }

    /// Suppress the drop-cancel and return the raw `TimerToken`.
    /// The caller takes responsibility for retiring the token via
    /// future wheel APIs (PR-7).
    pub fn forget(mut self) -> TimerToken {
        self.wheel = None;
        self.token
    }
}

impl Drop for TimerGuard {
    fn drop(&mut self) {
        if let Some(wheel) = self.wheel.take() {
            wheel.cancel(self.token);
        }
    }
}
