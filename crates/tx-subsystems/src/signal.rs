//! Signal state types and POSIX-shim entry points.
//!
//! Per `MODULE_MAP_v1` §11 and `SIGNAL_v1`, signal is a *shim* — not a
//! subsystem with its own zone-allocated entities. The data lives on
//! `ProcessPayload` (`sig_actions`, `group_pending`) and `ThreadPayload`
//! (`signal_mask`, `thread_pending`); the shim's job is POSIX-shaped
//! routing on top of those fields.
//!
//! Day-1 surface:
//!
//! - Types: [`Signum`], [`SignalMask`], [`PendingSignalQueue`],
//!   [`SigDisposition`], [`SigActionTable`].
//! - Shims: [`step_kill_process`], [`step_kill_pgrp`], [`step_sigaction`].
//! - Per-thread mutators (mask + pending) live in
//!   `thread_runtime::execution`.
//!
//! Deliberately deferred:
//!
//! - `SigInfo` payload (`si_code`, `si_value`, `si_pid`). Day-1 carries
//!   only the signum bit.
//! - Realtime queue (signums 32..=64). Day-1 is a 64-bit bitset; SIGRT*
//!   bits coexist with standard but do not queue per-occurrence yet.
//! - Default-disposition resolution (terminate / stop / continue /
//!   ignore). Disposition is recorded via `step_sigaction` but not yet
//!   acted on at delivery — the delivery step lands with the AST/scripts
//!   pass.
//! - `step_kill` taking a numeric `pid_t`. No pid → process registry
//!   yet; callers pass `Cap<ProcessIdentity>` directly. Numeric kill
//!   lands when the registry does.

use core::sync::atomic::{AtomicU64, Ordering};

use tx_substrate::zone::{Cap, OperationalCapExt};

use crate::execution::Errno;
use crate::process::structure::{ProcessGroup, ProcessIdentity};
use crate::thread_runtime::execution::post_signal;
use tx_substrate::SpinMutex;

/// POSIX signal number, 1..=64.
///
/// Standard signals occupy 1..=31; realtime signals occupy 32..=64.
/// The day-1 implementation treats them uniformly as bits in a 64-bit
/// bitset; per-occurrence queuing for realtime signals is a follow-up.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Hash)]
pub struct Signum(u8);

impl Signum {
    pub const SIGHUP: Self = Self(1);
    pub const SIGINT: Self = Self(2);
    pub const SIGQUIT: Self = Self(3);
    pub const SIGILL: Self = Self(4);
    pub const SIGABRT: Self = Self(6);
    pub const SIGKILL: Self = Self(9);
    pub const SIGSEGV: Self = Self(11);
    pub const SIGPIPE: Self = Self(13);
    pub const SIGTERM: Self = Self(15);
    pub const SIGCHLD: Self = Self(17);
    pub const SIGCONT: Self = Self(18);
    pub const SIGSTOP: Self = Self(19);
    pub const SIGTSTP: Self = Self(20);
    pub const SIGTTIN: Self = Self(21);
    pub const SIGTTOU: Self = Self(22);

    pub const MIN: u8 = 1;
    pub const MAX: u8 = 64;

    /// Construct a `Signum`. Returns `None` for values outside 1..=64.
    pub const fn new(value: u8) -> Option<Self> {
        if value >= Self::MIN && value <= Self::MAX {
            Some(Self(value))
        } else {
            None
        }
    }

    pub const fn raw(self) -> u8 {
        self.0
    }

    /// Bit position in a 64-bit bitset. `SIGHUP` (1) → bit 0, etc.
    pub const fn bit(self) -> u64 {
        1u64 << (self.0 - 1)
    }

    /// SIGKILL and SIGSTOP cannot be caught, blocked, or ignored per
    /// POSIX. Day-1 records this so `step_sigaction` and
    /// `step_sigprocmask` can refuse no-ops on these signals later.
    pub const fn is_uncatchable(self) -> bool {
        matches!(self.0, 9 | 19)
    }
}

/// Per-thread signal mask: bits set indicate signals that are *blocked*
/// (held pending; not delivered until unblocked). Stored as a plain
/// `u64` bitset; uncatchable signals (`SIGKILL`, `SIGSTOP`) are masked
/// off automatically by [`SignalMask::block`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SignalMask(u64);

impl SignalMask {
    pub const EMPTY: Self = Self(0);

    pub const fn new(bits: u64) -> Self {
        // Strip the uncatchable bits so SIGKILL/SIGSTOP can never be
        // blocked even if a caller passes them.
        Self(bits & !Self::uncatchable_bits())
    }

    pub const fn raw_bits(self) -> u64 {
        self.0
    }

    pub fn block(&mut self, sig: Signum) {
        if !sig.is_uncatchable() {
            self.0 |= sig.bit();
        }
    }

    pub fn unblock(&mut self, sig: Signum) {
        self.0 &= !sig.bit();
    }

    pub fn is_blocked(self, sig: Signum) -> bool {
        (self.0 & sig.bit()) != 0
    }

    const fn uncatchable_bits() -> u64 {
        Signum::SIGKILL.bit() | Signum::SIGSTOP.bit()
    }
}

/// Bitset-shaped pending-signal queue. `bits[i]` set means signal
/// `Signum(i+1)` is pending at least once. Day-1 collapses repeated
/// posts of the same standard signal (not a queue); realtime per-
/// occurrence queuing is a follow-up.
#[derive(Default)]
pub struct PendingSignalQueue {
    bits: AtomicU64,
}

impl PendingSignalQueue {
    pub const fn new() -> Self {
        Self {
            bits: AtomicU64::new(0),
        }
    }

    pub fn post(&self, sig: Signum) {
        self.bits.fetch_or(sig.bit(), Ordering::Release);
    }

    pub fn clear(&self, sig: Signum) {
        self.bits.fetch_and(!sig.bit(), Ordering::Release);
    }

    pub fn is_pending(&self, sig: Signum) -> bool {
        (self.bits.load(Ordering::Acquire) & sig.bit()) != 0
    }

    pub fn snapshot(&self) -> u64 {
        self.bits.load(Ordering::Acquire)
    }

    /// Bits that are pending and not blocked by `mask` — the set of
    /// signals that would be delivered if the thread polled now.
    pub fn deliverable_bits(&self, mask: SignalMask) -> u64 {
        self.snapshot() & !mask.raw_bits()
    }
}

/// Per-signal disposition installed via `step_sigaction`.
///
/// Day-1 records the choice; `Handler` carries an opaque `usize`
/// (typically a userspace function pointer) but the actual handler
/// invocation lives in the AST/script pass.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SigDisposition {
    /// POSIX default — terminate, stop, continue, or ignore depending
    /// on the signal. Resolution lands in the delivery pass.
    #[default]
    Default,
    /// Discard the signal without delivery.
    Ignore,
    /// User-defined handler at the recorded address.
    Handler(usize),
}

/// Per-process signal-action table. One [`SigDisposition`] slot per
/// signum. `SIGKILL` / `SIGSTOP` slots are ignored on writes to honor
/// the uncatchable invariant.
pub struct SigActionTable {
    entries: SpinMutex<[SigDisposition; Signum::MAX as usize]>,
}

impl Default for SigActionTable {
    fn default() -> Self {
        Self::new()
    }
}

impl SigActionTable {
    pub fn new() -> Self {
        Self {
            entries: SpinMutex::new([SigDisposition::Default; Signum::MAX as usize]),
        }
    }

    pub fn get(&self, sig: Signum) -> SigDisposition {
        self.entries.lock()[(sig.raw() - 1) as usize]
    }

    pub fn set(&self, sig: Signum, disposition: SigDisposition) {
        if sig.is_uncatchable() {
            return;
        }
        self.entries.lock()[(sig.raw() - 1) as usize] = disposition;
    }

    /// Reset every user-installed handler to `SigDisposition::Default`,
    /// preserving `Default` and `Ignore` slots.
    ///
    /// Per `txdoc:EXEC-12-3-RESET-SIGNAL-DISPOSITIONS`,
    /// `txdoc:EXEC-16-SIGNAL-RESET-SEMANTICS`, and `SIGNAL_v1` §15.2:
    /// exec resets only handlers — `SIG_IGN` dispositions survive
    /// across exec (POSIX), and pending signals are NOT cleared (a
    /// SIGTERM sent moments before exec is still delivered after the
    /// new image starts).
    ///
    /// `SigDisposition` today carries only the handler shape
    /// (`Default` / `Ignore` / `Handler(usize)`); when SA_FLAGS,
    /// SA_RESTORER, and per-handler SA_MASK are added, those fields
    /// will be zeroed in the same sweep (a `Default` slot has no
    /// handler frame storage by definition).
    ///
    /// Phase 7 — infallible. Called by the exec script after
    /// `txdoc:EXEC-11-PHASE-6-ADDRESS-SPACE-VISIBILITY-BOUNDARY`.
    pub fn step_reset_for_exec(&self) {
        let mut entries = self.entries.lock();
        for slot in entries.iter_mut() {
            if matches!(slot, SigDisposition::Handler(_)) {
                *slot = SigDisposition::Default;
            }
        }
    }
}

// ----- Delivery-side types (selection + AST) -----

/// Per-thread interrupt summary per `THREAD_RUNTIME_v1` §5.2.
///
/// Cheap atomic snapshot of "is there a deliverable signal / is the
/// thread being terminated / is a stop pending". The authoritative
/// state lives in `thread_pending`, `group_pending`, the current mask,
/// and `step_thread_exit`'s commit; `signal_summary` is a denormalised
/// view kept current by `post_signal`, `step_sigprocmask`, and the
/// SIGKILL routing path.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct InterruptSummary {
    pub deliverable_signal: bool,
    pub termination: bool,
    pub stop_requested: bool,
}

impl InterruptSummary {
    pub const EMPTY: Self = Self {
        deliverable_signal: false,
        termination: false,
        stop_requested: false,
    };

    const DELIVERABLE_BIT: u8 = 1 << 0;
    const TERMINATION_BIT: u8 = 1 << 1;
    const STOP_REQUESTED_BIT: u8 = 1 << 2;

    pub const fn pack(self) -> u8 {
        let mut bits = 0u8;
        if self.deliverable_signal {
            bits |= Self::DELIVERABLE_BIT;
        }
        if self.termination {
            bits |= Self::TERMINATION_BIT;
        }
        if self.stop_requested {
            bits |= Self::STOP_REQUESTED_BIT;
        }
        bits
    }

    pub const fn unpack(bits: u8) -> Self {
        Self {
            deliverable_signal: (bits & Self::DELIVERABLE_BIT) != 0,
            termination: (bits & Self::TERMINATION_BIT) != 0,
            stop_requested: (bits & Self::STOP_REQUESTED_BIT) != 0,
        }
    }
}

/// Default action for a signal whose disposition is `SIG_DFL` per
/// POSIX + Linux. `select_next_signal` + `ast_check` consult this when
/// they encounter `Disposition::Default`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DefaultAction {
    /// Terminate the process.
    Term,
    /// Terminate the process and dump core (for our purposes,
    /// indistinguishable from `Term`).
    Core,
    /// Drop the signal silently.
    Ignore,
    /// Stop the process (job-control).
    Stop,
    /// Continue a stopped process.
    Cont,
}

/// POSIX/Linux default-action table. Day-1 covers the subset used by
/// the existing tests + producer catalog; signums outside the table
/// default to `Term` (matches Linux's policy for unknown realtime
/// signals).
pub fn default_action(sig: Signum) -> DefaultAction {
    match sig.raw() {
        // Term: SIGHUP, SIGINT, SIGKILL, SIGPIPE, SIGTERM
        1 | 2 | 9 | 13 | 15 => DefaultAction::Term,
        // Core: SIGQUIT, SIGILL, SIGABRT, SIGSEGV
        3 | 4 | 6 | 11 => DefaultAction::Core,
        // Ignore: SIGCHLD, SIGWINCH (28), SIGURG (23, when wired)
        17 | 23 | 28 => DefaultAction::Ignore,
        // Stop: SIGSTOP, SIGTSTP, SIGTTIN, SIGTTOU
        19..=22 => DefaultAction::Stop,
        // Cont: SIGCONT
        18 => DefaultAction::Cont,
        // Realtime + unknown: terminate by default per Linux.
        _ => DefaultAction::Term,
    }
}

/// Source queue from which `select_next_signal` chose the signum.
/// Used by `ast_check` to dequeue from the right queue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PendingSource {
    Thread,
    Group,
}

/// Lowest deliverable signum on a thread per `SIGNAL_v1` §14.
///
/// Selection order: thread-directed pending first (lowest signum),
/// then group-directed pending (lowest signum). Both intersected with
/// `!signal_mask`. Returns the chosen signum + the source queue
/// without dequeuing — the caller (`ast_check`) dequeues only after
/// consulting `sig_actions`, since some dispositions (Ignore,
/// Default-Ignore) drop the signal and the loop re-selects.
pub fn select_next_signal(
    thread: &Cap<crate::thread_runtime::ThreadIdentity>,
) -> Option<(Signum, PendingSource)> {
    let payload_guard = thread.payload.lock();
    let payload = payload_guard.as_ref()?;
    let mask = payload.signal_mask();

    // Thread-directed pending first.
    let t_deliverable = payload.pending().deliverable_bits(mask);
    if let Some(sig) = lowest_signum_bit(t_deliverable) {
        return Some((sig, PendingSource::Thread));
    }
    drop(payload_guard);

    // Group-directed pending next.
    let guard = tx_substrate::epoch::guard();
    let proc = thread.owner_proc.upgrade(&guard)?;
    drop(guard);

    let proc_payload_guard = proc.payload.lock();
    let proc_payload = proc_payload_guard.as_ref()?;

    // Mask check uses the same per-thread mask (re-read in case it
    // changed between blocks; cheap).
    let mask = thread
        .payload
        .lock()
        .as_ref()
        .map(|p| p.signal_mask())
        .unwrap_or(SignalMask::EMPTY);

    let g_deliverable = proc_payload.group_pending().deliverable_bits(mask);
    lowest_signum_bit(g_deliverable).map(|sig| (sig, PendingSource::Group))
}

fn lowest_signum_bit(bits: u64) -> Option<Signum> {
    if bits == 0 {
        return None;
    }
    let pos = bits.trailing_zeros() as u8 + 1; // bit 0 = signum 1
    Signum::new(pos)
}

/// Outcome of `ast_check` per `SIGNAL_v1` §15.1, projected to the
/// day-1 surface that does not yet build signal frames or invoke
/// group-exit machinery. Each variant captures a *recognised intent*;
/// future work materialises it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AstOutcome {
    /// No deliverable signal; trap return proceeds to userspace.
    Continue,
    /// `summary.termination` is set (SIGKILL or fatal signal already
    /// escalated). Thread should exit; the trap return must not enter
    /// userspace.
    InitiateTermination,
    /// Default action for `sig` is `Term`/`Core` — process group
    /// should exit with this signal. Future work hooks this into
    /// `process::step_exit_group` with an exit-status that encodes
    /// "killed by sig".
    DefaultTerminate { sig: Signum },
    /// Default action is `Stop` (SIGSTOP-family). Thread should park
    /// on the stop channel; `THREAD_RUNTIME_v1` §6 owns the state
    /// machine. Day-1 just recognises the intent.
    DefaultStop { sig: Signum },
    /// Default action is `Cont` (SIGCONT). Continue control op.
    DefaultContinue { sig: Signum },
    /// User-installed handler. Day-1 records the intent + handler
    /// address; signal-frame construction lands with the AST trap-
    /// return wiring.
    DeliverHandler { sig: Signum, handler: usize },
}

/// Site-B delivery decision per `SIGNAL_v1` §15.1.
///
/// Selection priority:
/// 1. `summary.termination` → `InitiateTermination` (no signal pop).
/// 2. Loop: pick lowest deliverable signum, dequeue from its source
///    queue, consult `sig_actions`. `Ignore` and `Default::Ignore`
///    drop the signal and re-loop. Any other disposition returns
///    its corresponding outcome.
/// 3. If the loop exhausts deliverable signals, return `Continue`
///    (so a `stop_requested` summary bit is observed by
///    `thread_future`'s next poll, per the spec).
///
/// Thread must be live (`payload.is_some()`); calling on a zombie
/// thread returns `Continue`. Owner process must also be live; if
/// gone, returns `Continue` (no group_pending or sig_actions to
/// consult).
pub fn ast_check(thread: &Cap<crate::thread_runtime::ThreadIdentity>) -> AstOutcome {
    let thread_payload = match thread.upgrade_operational() {
        Ok(payload) => payload,
        Err(_) => return AstOutcome::Continue,
    };
    let summary = thread_payload.interrupt_summary();

    if summary.termination {
        return AstOutcome::InitiateTermination;
    }

    let guard = tx_substrate::epoch::guard();
    let Some(proc) = thread.owner_proc.upgrade(&guard) else {
        return AstOutcome::Continue;
    };
    drop(guard);

    loop {
        let Some((sig, source)) = select_next_signal(thread) else {
            return AstOutcome::Continue;
        };

        // Dequeue from the source queue before consulting disposition,
        // so an `Ignore` drops the signal cleanly and the loop re-selects.
        match source {
            PendingSource::Thread => {
                thread_payload.pending().clear(sig);
            }
            PendingSource::Group => {
                let Ok(proc_payload) = proc.upgrade_operational() else {
                    return AstOutcome::Continue;
                };
                proc_payload.group_pending().clear(sig);
            }
        }

        let disposition = match proc.upgrade_operational() {
            Ok(payload) => payload.sig_actions().get(sig),
            Err(_) => return AstOutcome::Continue,
        };

        match disposition {
            SigDisposition::Ignore => continue,
            SigDisposition::Default => match default_action(sig) {
                DefaultAction::Ignore => continue,
                DefaultAction::Term | DefaultAction::Core => {
                    return AstOutcome::DefaultTerminate { sig };
                }
                DefaultAction::Stop => return AstOutcome::DefaultStop { sig },
                DefaultAction::Cont => return AstOutcome::DefaultContinue { sig },
            },
            SigDisposition::Handler(handler) => return AstOutcome::DeliverHandler { sig, handler },
        }
    }
}

/// Run `ast_check` and materialise its outcome with the day-1
/// side-effects we have wired:
///
/// - `DefaultTerminate { sig }` invokes
///   [`process::step_exit_group_with_signal`] on the thread's owner
///   process. The process zombifies with `terminating_signal =
///   Some(sig)` and the day-1 status encoding.
/// - All other variants are returned unchanged. `DefaultStop`,
///   `DefaultContinue`, `DeliverHandler` are recognised intents;
///   their materialisation needs machinery (stop-state, continue
///   control op, signal-frame construction) that doesn't exist yet.
///   `InitiateTermination` is observed at the future `thread_future`'s
///   poll boundary (per `SIGNAL_v1` §15.1); the caller of this
///   helper is the place to take the future-exit hint.
///
/// Returns the same `AstOutcome` `ast_check` produced, so callers
/// can dispatch on the variant after the side-effect (if any) has
/// run.
pub fn ast_dispatch(thread: &Cap<crate::thread_runtime::ThreadIdentity>) -> AstOutcome {
    let outcome = ast_check(thread);
    if let AstOutcome::DefaultTerminate { sig } = outcome {
        let guard = tx_substrate::epoch::guard();
        if let Some(proc) = thread.owner_proc.upgrade(&guard) {
            drop(guard);
            crate::process::execution::step_exit_group_with_signal(&proc, sig);
        }
    }
    outcome
}

/// Result of a kill-style shim. `Delivered` if at least one thread
/// received the post; `NoLiveThread` if the target is a zombie or its
/// thread list is empty.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KillOutcome {
    Delivered,
    NoLiveThread,
}

/// Deliver `sig` to a single process. Per `SIGNAL_v1` §1, §12.1
/// dispatches by category:
///
/// - **Gewalt** signums (SIGKILL, SIGSTOP, SIGCONT) bypass the
///   pending queue entirely and route through [`route_gewalt`],
///   which updates each thread's `signal_summary` in place.
/// - **Event** signums (catchable) route to the first live thread's
///   `thread_pending` via [`post_signal`].
///
/// Zombies are skipped.
pub fn step_kill_process(target: &Cap<ProcessIdentity>, sig: Signum) -> KillOutcome {
    if is_gewalt(sig) {
        return route_gewalt(target, sig);
    }

    let Ok(payload) = target.upgrade_operational() else {
        return KillOutcome::NoLiveThread;
    };
    let threads = payload.threads.lock();
    let Some(leader) = threads.iter().find(|t| !t.is_zombie()).cloned() else {
        return KillOutcome::NoLiveThread;
    };
    drop(threads);

    post_signal(&leader, sig);
    KillOutcome::Delivered
}

/// `true` for the three Gewalt signums per `SIGNAL_v1` §1: SIGKILL,
/// SIGSTOP, SIGCONT. These bypass the catchable pending queues and
/// are routed by [`route_gewalt`].
pub const fn is_gewalt(sig: Signum) -> bool {
    matches!(
        sig.raw(),
        9  /* SIGKILL */ | 19 /* SIGSTOP */ | 18 /* SIGCONT */
    )
}

/// Apply a Gewalt signal to `target`. Per `SIGNAL_v1` §12.3, Gewalt
/// signals do not enter pending queues — they directly invoke control
/// ops (group exit, group stop, group continue):
///
/// - **SIGKILL** → invokes [`process::step_exit_group_with_signal`]
///   immediately. The target becomes a zombie with
///   `terminating_signal = Some(SIGKILL)` and
///   `exit_status = Some(128 + SIGKILL)`. Per spec
///   "exit_status encodes 'killed by SIGKILL'".
/// - **SIGSTOP** → sets `summary.stop_requested` on every live
///   thread. Day-1 has no stop-state machine; the bit is observable
///   for a future `thread_future` to park on.
/// - **SIGCONT** → clears `summary.stop_requested` on every live
///   thread. The (also-future) handler-half of `route_sigcont` —
///   enqueueing on `group_pending` if a SIGCONT handler is installed
///   — is deferred.
///
/// Returns `Delivered` if the target was live and at least one
/// transition was applied, `NoLiveThread` otherwise.
pub fn route_gewalt(target: &Cap<ProcessIdentity>, sig: Signum) -> KillOutcome {
    debug_assert!(is_gewalt(sig), "route_gewalt called with non-Gewalt signum");

    if sig == Signum::SIGKILL {
        if target.is_zombie() {
            return KillOutcome::NoLiveThread;
        }
        crate::process::execution::step_exit_group_with_signal(target, sig);
        return KillOutcome::Delivered;
    }

    let Ok(payload) = target.upgrade_operational() else {
        return KillOutcome::NoLiveThread;
    };
    let threads: alloc::vec::Vec<Cap<crate::thread_runtime::ThreadIdentity>> =
        payload.threads.lock().iter().cloned().collect();

    let mut touched = false;
    for thread in &threads {
        let Ok(thread_payload) = thread.upgrade_operational() else {
            continue;
        };
        thread_payload.update_summary(|s| match sig.raw() {
            19 => s.stop_requested = true,
            18 => s.stop_requested = false,
            _ => unreachable!("SIGKILL handled above; is_gewalt guarantees the rest"),
        });
        touched = true;
    }

    if touched {
        KillOutcome::Delivered
    } else {
        KillOutcome::NoLiveThread
    }
}

/// Deliver `sig` to every live process in `pgrp`. Catchable signals
/// also get the bit set on each delivered member's `group_pending`
/// queue so the (future) delivery step can distinguish thread-
/// targeted from group-targeted posts. Gewalt signals bypass the
/// pending queues entirely per `SIGNAL_v1` §2 Consequence 2 — they
/// route through [`route_gewalt`] which directly updates each
/// thread's `signal_summary`.
///
/// Returns the count of processes that received the post.
pub fn step_kill_pgrp(pgrp: &Cap<ProcessGroup>, sig: Signum) -> usize {
    let guard = tx_substrate::epoch::guard();
    let mut delivered = 0usize;
    let catchable = !is_gewalt(sig);
    for weak in pgrp.members.lock().iter() {
        let Some(member) = weak.upgrade(&guard) else {
            continue;
        };
        if step_kill_process(&member, sig) == KillOutcome::Delivered {
            // Catchable only: mirror onto group_pending so the
            // delivery step can recognise group-targeted posts.
            // Gewalt bypasses pending queues entirely.
            if catchable {
                if let Ok(payload) = member.upgrade_operational() {
                    payload.group_pending().post(sig);
                }
            }
            delivered += 1;
        }
    }
    delivered
}

/// Install (or replace) the disposition of `sig` on `process`.
/// `Default` for `SIGKILL`/`SIGSTOP` is rejected silently.
/// Returns the previous disposition.
pub fn step_sigaction(
    process: &Cap<ProcessIdentity>,
    sig: Signum,
    disposition: SigDisposition,
) -> SigDispositionChange {
    let Ok(payload) = process.upgrade_operational() else {
        return SigDispositionChange::ZombieIgnored;
    };
    let prev = payload.sig_actions().get(sig);
    if sig.is_uncatchable() {
        return SigDispositionChange::Uncatchable(prev);
    }
    payload.sig_actions().set(sig, disposition);
    SigDispositionChange::Replaced { prev }
}

/// Outcome of `step_sigaction`. `Replaced` is the normal path;
/// `ZombieIgnored` means the target had no payload; `Uncatchable`
/// means SIGKILL/SIGSTOP were silently kept at default.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SigDispositionChange {
    Replaced { prev: SigDisposition },
    Uncatchable(SigDisposition),
    ZombieIgnored,
}

// ----- Script-level (permission-checked) entry points -----

/// Outcome of [`script_kill_process`]. `Delivered` is the normal path;
/// `NoLiveThread` means the target had no payload (zombie); permission
/// denial is reported as `Err(Errno::EPERM)` per POSIX `kill(2)`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KillScriptOutcome {
    Delivered,
    NoLiveThread,
    /// Posted permission was OK (e.g. signal 0 probe), no actual
    /// delivery happened.
    Probed,
}

/// POSIX-shaped `kill(target_pid, sig)` modulo pid lookup. Composes
/// the `cred::require_signal_send` permission check with
/// [`step_kill_process`] per `SIGNAL_v1` §32's `script_kill`. The
/// guard-bound witness is consumed in the same step; the underlying
/// post is the existing producer-internal mechanism.
///
/// Returns:
/// - `Ok(Delivered)` — permission granted, signal posted to a live thread.
/// - `Ok(NoLiveThread)` — permission granted, target has no live thread.
/// - `Ok(Probed)` — `sig` was the null signal (`Signum::new(0)` is not
///   constructible via the public API; this branch is reachable only
///   via the explicit signal-0 probe shape — see `script_kill_probe`).
/// - `Err(Errno::ESRCH)` — `source` is a zombie (no cred to consult).
/// - `Err(Errno::EPERM)` — cred check denied per
///   `cred::signal_permitted`.
pub fn script_kill_process(
    source: &Cap<ProcessIdentity>,
    target: &Cap<ProcessIdentity>,
    sig: Signum,
) -> Result<KillScriptOutcome, Errno> {
    let guard = tx_substrate::epoch::guard();

    let source_cred = source
        .upgrade_operational()
        .map_err(|_| Errno::ESRCH)?
        .cred();

    let Some(target_facts) = target.target_proc_cred_for(source) else {
        return Ok(KillScriptOutcome::NoLiveThread);
    };

    let _auth = crate::cred::require_signal_send(source_cred, &target_facts, sig, &guard)?;

    Ok(match step_kill_process(target, sig) {
        KillOutcome::Delivered => KillScriptOutcome::Delivered,
        KillOutcome::NoLiveThread => KillScriptOutcome::NoLiveThread,
    })
}

/// Permission probe equivalent to POSIX `kill(pid, 0)`. Runs the cred
/// check but does not deliver. Returns `Ok(Probed)` on permitted,
/// `Err(EPERM)` otherwise.
pub fn script_kill_probe(
    source: &Cap<ProcessIdentity>,
    target: &Cap<ProcessIdentity>,
) -> Result<KillScriptOutcome, Errno> {
    let guard = tx_substrate::epoch::guard();

    let source_cred = source
        .upgrade_operational()
        .map_err(|_| Errno::ESRCH)?
        .cred();

    let Some(target_facts) = target.target_proc_cred_for(source) else {
        return Ok(KillScriptOutcome::NoLiveThread);
    };

    // Use SIGTERM as the rule's signum input — the no-deliver probe
    // applies the same rule POSIX kill(pid, 0) does, which is
    // signum-independent except for SIGCONT-same-session. We pass a
    // signum that doesn't trigger the SIGCONT bypass to keep the
    // probe consistent with how userspace expects kill(pid, 0) to
    // behave.
    let _auth =
        crate::cred::require_signal_send(source_cred, &target_facts, Signum::SIGTERM, &guard)?;

    Ok(KillScriptOutcome::Probed)
}

/// Permission-checked process-group fanout per `SIGNAL_v1` §12.2.
/// Iterates `pgrp.members`, builds a `TargetProcCred` per live member,
/// runs the cred check, and posts on permitted members via
/// [`step_kill_process`]. Returns the count of members the call
/// successfully delivered to. Per-member denials and zombies are
/// independent — they don't fail the whole call.
///
/// Returns `Err(ESRCH)` if `source` is a zombie. Returns `Ok(0)` if
/// no member was permitted *and* live; userspace shims may translate
/// this to `Errno::EPERM` (POSIX `kill` to a pgrp returns EPERM iff
/// no member was reachable).
pub fn script_kill_pgrp(
    source: &Cap<ProcessIdentity>,
    pgrp: &Cap<ProcessGroup>,
    sig: Signum,
) -> Result<u32, Errno> {
    let guard = tx_substrate::epoch::guard();
    script_kill_pgrp_with_guard(source, pgrp, sig, &guard)
}

pub(crate) fn script_kill_pgrp_with_guard(
    source: &Cap<ProcessIdentity>,
    pgrp: &Cap<ProcessGroup>,
    sig: Signum,
    guard: &tx_substrate::epoch::Guard<'_>,
) -> Result<u32, Errno> {
    let source_cred = source
        .upgrade_operational()
        .map_err(|_| Errno::ESRCH)?
        .cred();

    let mut delivered = 0u32;
    let members: alloc::vec::Vec<Cap<ProcessIdentity>> = pgrp
        .members
        .lock()
        .iter()
        .filter_map(|w| w.upgrade(guard))
        .collect();

    for member in &members {
        let Some(facts) = member.target_proc_cred_for(source) else {
            continue;
        };
        if crate::cred::require_signal_send(source_cred, &facts, sig, guard).is_err() {
            continue;
        }
        if step_kill_process(member, sig) == KillOutcome::Delivered {
            // Catchable only: mirror onto group_pending. Gewalt
            // signals (SIGKILL/SIGSTOP/SIGCONT) bypass pending
            // queues entirely per SIGNAL_v1 §2 Consequence 2.
            if !is_gewalt(sig) {
                if let Ok(payload) = member.upgrade_operational() {
                    payload.group_pending().post(sig);
                }
            }
            delivered += 1;
        }
    }

    Ok(delivered)
}

// ----- TTY job-control bridge -----

/// Map a TTY [`JobControlSignal`](crate::tty::execution::JobControlSignal)
/// to the POSIX `Signum`. Centralised here so the signal shim is the
/// canonical authority on which numeric signal each control event
/// produces; TTY emits intent (`Int`/`Quit`/...) and signal materialises
/// it.
pub fn signum_for_job_control(sig: crate::tty::execution::JobControlSignal) -> Signum {
    use crate::tty::execution::JobControlSignal as J;
    match sig {
        J::Int => Signum::SIGINT,
        J::Quit => Signum::SIGQUIT,
        J::Tstp => Signum::SIGTSTP,
        J::Ttin => Signum::SIGTTIN,
        J::Ttou => Signum::SIGTTOU,
        J::Hup => Signum::SIGHUP,
        J::Cont => Signum::SIGCONT,
        // SIGWINCH is signum 28 on Linux. Day-1 Signum exposes a small
        // POSIX subset; we synthesise the value here. The downstream
        // route is the same as any other signal.
        J::Winch => Signum::new(28).expect("SIGWINCH"),
    }
}

/// Outcome of [`deliver_tty_dispatch`]. Mirrors `KillScriptOutcome`'s
/// shape but adds `NoTypedPgrp` for the legacy-binding case where the
/// dispatch carried no typed `Weak<ProcessGroup>` — the caller (the
/// TTY ioctl driver) has nothing to upgrade.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DispatchOutcome {
    /// Posted to N processes in the target pgrp.
    Delivered { count: u32 },
    /// The target pgrp's `Weak` upgraded to a dropped slot.
    PgrpDropped,
    /// The dispatch carried no typed pgrp ref — caller must fall back
    /// to a numeric-pgid lookup path.
    NoTypedPgrp,
}

/// Bridge from a TTY-emitted [`SignalDispatch`](crate::tty::execution::SignalDispatch)
/// to a real signal post via [`script_kill_pgrp`]. Upgrades the
/// dispatch's typed `Weak<ProcessGroup>` and runs the cred-checked
/// pgrp fanout against `source`'s cred.
///
/// Returns `Err(Errno::ESRCH)` if `source` is a zombie (per
/// `script_kill_pgrp`); other per-member denials are absorbed into
/// the count (the SIGNAL_v1 §12.2 "members independent" rule).
pub fn deliver_tty_dispatch(
    source: &Cap<ProcessIdentity>,
    dispatch: crate::tty::execution::SignalDispatch,
) -> Result<DispatchOutcome, Errno> {
    let guard = tx_substrate::epoch::guard();
    deliver_tty_dispatch_with_guard(source, dispatch, &guard)
}

pub fn deliver_tty_dispatch_with_guard(
    source: &Cap<ProcessIdentity>,
    dispatch: crate::tty::execution::SignalDispatch,
    guard: &tx_substrate::epoch::Guard<'_>,
) -> Result<DispatchOutcome, Errno> {
    let Some(weak) = dispatch.target.pgrp_weak() else {
        return Ok(DispatchOutcome::NoTypedPgrp);
    };

    let Some(pgrp_cap) = weak.upgrade(guard) else {
        return Ok(DispatchOutcome::PgrpDropped);
    };

    let signum = signum_for_job_control(dispatch.signal);
    let count = script_kill_pgrp_with_guard(source, &pgrp_cap, signum, guard)?;
    Ok(DispatchOutcome::Delivered { count })
}

#[cfg(test)]
mod tests;
