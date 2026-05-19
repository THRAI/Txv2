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

pub mod adapter;

use adapter::step_engine::{
    self, Cap, Guard, NoProgress, OneShotStepOp, OperationalCapExt, ScriptCtx, SignalRouting,
    SpinMutex, StepOp, StepOutcome, SubjectIdentity,
};

use crate::execution::Errno;
use crate::process::structure::{ProcessGroup, ProcessIdentity, SIGNAL_GENERATED};
use crate::thread_runtime::execution::{post_signal, post_signal_mailbox};

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

/// Minimal POSIX `siginfo_t` payload carried with each signal
/// delivery.  Phase I carries `si_signo`, `si_code`, `si_pid`,
/// and `si_uid`; `si_addr`, `si_value`, and the status union
/// (`si_status`) are deferred.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SigInfo {
    pub si_signo: u32,
    pub si_code: i32,
    pub si_pid: u32,
    pub si_uid: u32,
}

/// SI_USER: signal sent by kill(2) / tkill(2) / tgkill(2).
pub const SI_USER: i32 = 0;

/// Per-process siginfo slots — one optional [`SigInfo`] record
/// per signum.  Lives on `ProcessPayload` alongside `group_pending`.
pub struct SigInfoSlots {
    slots: SpinMutex<[Option<SigInfo>; 64]>,
}

impl SigInfoSlots {
    pub fn new() -> Self {
        Self {
            slots: SpinMutex::new([None; 64]),
        }
    }

    pub fn store(&self, signum: Signum, info: SigInfo) {
        let idx = (signum.raw() - 1) as usize;
        self.slots.lock()[idx] = Some(info);
    }

    pub fn get(&self, signum: Signum) -> Option<SigInfo> {
        let idx = (signum.raw() - 1) as usize;
        self.slots.lock()[idx]
    }

    pub fn clear(&self, signum: Signum) {
        let idx = (signum.raw() - 1) as usize;
        self.slots.lock()[idx] = None;
    }
}

impl Default for SigInfoSlots {
    fn default() -> Self {
        Self::new()
    }
}

impl PendingSignalQueue {
    pub fn new() -> Self {
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
        // observe
        // upgrade
        // reserve
        // commit
        // publish
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
    let guard = step_engine::guard();
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

    let guard = step_engine::guard();
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
        let guard = step_engine::guard();
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

/// Deliver a synchronous fault signal (SIGSEGV, SIGILL, SIGBUS, etc.)
/// to the current thread's owning process.
///
/// Per `SIGNAL_v1` §20, synchronous faults are delivered on the
/// trapping thread and **bypass the signal mask** — the fault
/// occurs regardless of `sigprocmask` state.  The delivery path
/// differs from `deliver_posix_signal` in two critical ways:
///
/// 1. **No Gewalt routing.** Synchronous faults are always
///    catchable Event signals — they consult `sig_actions` for
///    disposition, never trigger control primitives.
/// 2. **No group-wide scan.** The signal is posted directly to the
///    trapping thread's `thread_pending` (not the process group
///    queue) because POSIX requires the fault to be delivered on
///    the thread that raised it.
///
/// If the disposition is `Default` (no handler installed), the
///    default action is taken immediately — for SIGSEGV/SIGILL/
///    SIGBUS/SIGFPE this is `Term` or `Core`, which maps to
///    `step_exit_group_with_signal`.
///
/// Phase B (first pass): always takes the default-action path
///    (calls `step_exit_group_with_signal`).  Handler routing
///    via `sig_actions` and signal-frame delivery land in
///    Phase D once `SignalFrameIf` integration is complete.
///
/// The `thread` argument supplies the trapping thread's `Cap` so
///    the function can resolve the owning process.  The faulting
///    signum must be one of the synchronous set (SIGSEGV, SIGILL,
///    SIGBUS, SIGFPE, SIGTRAP, SIGSYS); other signums panic.
///
/// See: `txdoc:SIGNAL-V1-S20-SYNCHRONOUS-FAULT`.
pub fn deliver_synchronous_fault(thread: &Cap<crate::thread_runtime::ThreadIdentity>, sig: Signum) {
    let guard = step_engine::guard();
    if let Some(process) = thread.owner_proc.upgrade(&guard) {
        drop(guard);
        // Phase B: always take default action.
        crate::process::execution::step_exit_group_with_signal(&process, sig);
    }
}

/// Deliver a signal via the canonical POSIX entry point.
///
/// This is the canonical POSIX signal delivery entry per `SIGNAL_v1`
/// §12 (`deliver_posix_signal`). It accepts a `SignalTarget` and
/// routes the signal through three stages:
///
/// 1. **Gewalt check** — `SIGKILL`/`SIGSTOP`/`SIGCONT` bypass the
///    routing table and call control primitives directly.
/// 2. **Event routing** — all other signums consult `sig_actions`
///    (disposition) to determine routing:
///    - `SIG_IGN` → drop immediately (no pending post).
///    - `SIG_DFL` → take default action immediately (Term/Core/
///      Stop/Cont/Ignore per POSIX).
///    - `Handler(addr)` → post to an eligible thread's pending
///      queue; the AST delivers the handler on next userspace
///      boundary (Phase D handler delivery).
/// 3. **Bus wire fire** — after thread-eligibility post and signalfd
///    notification, `signal_port.fire(SIGNAL_GENERATED)` wakes native
///    subscribers.
///
/// Phase D: consults `sig_actions` for routing; `ProcessGroup` and
/// `Thread` targets are TODO.
///
/// See: `txdoc:SIGNAL-V1-S12-DELIVER-POSIX-SIGNAL`.
pub fn deliver_posix_signal(target: SignalTarget, sig: Signum) -> KillOutcome {
    let cap = match target {
        SignalTarget::Process(cap) => cap,
        SignalTarget::ProcessGroup(_group) => {
            // Not yet implemented — surface as no-live-thread.
            return KillOutcome::NoLiveThread;
        }
        SignalTarget::Thread(thread_cap) => {
            // Upgrade to owning process cap for signal routing.
            let _guard = step_engine::guard();
            match thread_cap.upgrade_owner_proc() {
                Some(proc) => proc,
                None => return KillOutcome::NoLiveThread,
            }
        }
    };

    // Gewalt signums bypass the routing table entirely.
    if is_gewalt(sig) {
        return step_kill_process(&cap, sig, None);
    }

    // Event signums: consult sig_actions for disposition.
    let disposition = match cap.upgrade_operational() {
        Ok(payload) => payload.sig_actions().get(sig),
        Err(_) => return KillOutcome::NoLiveThread,
    };

    match disposition {
        SigDisposition::Ignore => KillOutcome::Delivered,
        SigDisposition::Default => {
            // Materialise the default action immediately.
            match default_action(sig) {
                DefaultAction::Ignore => KillOutcome::Delivered,
                DefaultAction::Term | DefaultAction::Core => {
                    crate::process::execution::step_exit_group_with_signal(&cap, sig);
                    KillOutcome::Delivered
                }
                DefaultAction::Stop => {
                    route_gewalt(&cap, Signum::SIGSTOP);
                    KillOutcome::Delivered
                }
                DefaultAction::Cont => {
                    route_gewalt(&cap, Signum::SIGCONT);
                    KillOutcome::Delivered
                }
            }
        }
        SigDisposition::Handler(_handler) => {
            // Post to eligible thread's pending; AST delivers handler.
            step_kill_process(&cap, sig, None)
        }
    }
}

/// Target for `deliver_posix_signal`.
///
/// Per `SIGNAL_v1` §12, a signal can be addressed to a process
/// (`pid > 0`), a process group (`pid == 0` or `pid == -pgid`), or a
/// specific thread (`tkill`/`tgkill`). Phase A supports only
/// `Process`.
#[derive(Clone, Debug)]
pub enum SignalTarget {
    Process(Cap<ProcessIdentity>),
    ProcessGroup(Cap<ProcessGroup>),
    Thread(Cap<crate::thread_runtime::structure::ThreadIdentity>),
}

/// Deliver `sig` to a single process. Per `SIGNAL_v1` §1, §12.1
/// dispatches by category:
///
/// - **Gewalt** signums (SIGKILL, SIGSTOP, SIGCONT) bypass the
///   pending queue entirely and route through [`route_gewalt`],
///   which updates each thread's `signal_summary` in place.
/// - **Event** signums (catchable) route to a single eligible live
///   thread's `thread_pending` via [`post_signal`].
///
/// **D9-B eligibility scan.** Per POSIX, a process-directed signal
/// must be delivered to a thread whose mask permits the signum if
/// one exists. The scan runs under `payload.threads.inner.lock()` so that
/// concurrent `kill(pid, sig)` calls serialise on the thread-list
/// lock; only one CAS into a thread's `thread_pending` succeeds in
/// *first* setting the bit (POSIX coalescence for standard signals).
///
/// Two-pass selection:
/// 1. **Eligible.** Prefer the first non-zombie thread that does
///    *not* have `sig` blocked in its sigmask.
/// 2. **Fallback.** If every non-zombie thread has `sig` blocked
///    (POSIX permits the signal to remain pending until the chosen
///    thread unblocks it), pick the first non-zombie thread anyway.
///    The bit is set in its `thread_pending`; `signal_summary.
///    deliverable_signal` stays `false` because `post_signal`'s mask
///    check filters the summary update.
///
/// The mailbox post happens via `post_signal` *after* the
/// thread-list lock is dropped — `post_signal` does not reacquire
/// `payload.threads.inner.lock()`, so this order avoids any
/// post-while-holding-list-lock hazard. Zombies are skipped.
pub fn step_kill_process(
    target: &Cap<ProcessIdentity>,
    sig: Signum,
    info: Option<SigInfo>,
) -> KillOutcome {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    if is_gewalt(sig) {
        let outcome = route_gewalt(target, sig);
        // D9-D: route_gewalt does not currently fan out to signalfd
        // subscriptions — SIGKILL/SIGSTOP/SIGCONT bypass pending
        // queues per SIGNAL_v1 §2 Consequence 2, and the canonical
        // signalfd Linux behaviour for SIGSTOP/SIGCONT is to deliver
        // them as well (handler-bypassing, but signalfd-visible).
        // We follow that here: notify any per-process subscription
        // whose mask covers the signum *after* the gewalt routing
        // completes its per-thread fan-out. SIGKILL routes to the
        // process-exit path which already zombifies every thread; the
        // signalfd post happens before zombify reaches the registry
        // unregister site (Drop for SignalFd runs only when the cap
        // drops, not at thread zombify).
        if outcome == KillOutcome::Delivered {
            // Phase A (bus wire alignment): Gewalt signums (SIGKILL,
            // SIGSTOP, SIGCONT) fire signal_port too — Linux delivers
            // them to signalfd per D9-D comment above. Bus subscribers
            // wake and re-read pending state after the gewalt routing
            // completes its per-thread fan-out.
            // Phase C: signal_port.fire is the canonical driver;
            // signalfd notification happens as a bus subscriber
            // reaction after the wire fires.
            if let Some(payload) = target.payload.lock().as_ref() {
                payload.signal_port.fire(SIGNAL_GENERATED);
            }
            crate::signalfd::notify_process_signal(target.key().raw(), sig);
        }
        return outcome;
    }

    let Ok(payload) = target.upgrade_operational() else {
        return KillOutcome::NoLiveThread;
    };

    let chosen = {
        let threads = payload.threads.snapshot();

        // Pass 1: first non-zombie thread with `sig` NOT blocked.
        // The sigmask read goes through the payload's `signal_mask`
        // atomic via `signal_mask()`, matching `post_signal` /
        // `step_sigprocmask`'s discipline.
        let mut eligible: Option<Cap<crate::thread_runtime::ThreadIdentity>> = None;
        let mut fallback: Option<Cap<crate::thread_runtime::ThreadIdentity>> = None;
        for thread in threads.iter() {
            let Some(thread_payload) = thread.payload_cap() else {
                // Zombie: skip.
                continue;
            };
            if fallback.is_none() {
                fallback = Some(thread.clone());
            }
            if !thread_payload.signal_mask().is_blocked(sig) {
                eligible = Some(thread.clone());
                break;
            }
        }
        eligible.or(fallback)
        // `threads` lock drops at end of scope before `post_signal`.
    };

    let Some(chosen) = chosen else {
        return KillOutcome::NoLiveThread;
    };

    // Store SigInfo in the process slots before posting.
    if let Some(ref sinfo) = info {
        target.siginfo_store(sig, *sinfo);
    }

    post_signal(&chosen, sig, info);

    // D9-D: fan out to every per-process signalfd subscription whose
    // mask covers `sig`. Runs *after* the thread-eligibility post —
    // the wake paths are additive (per D9 §6 / W-II prompt
    // constraint 1). The thread post still updates the truth-bearing
    // `InterruptSummary` and the bound mailbox; the signalfd post
    // routes signal-as-event to any agent draining via `read(2)`.

    // Phase C (bus-aligned delivery): fire the per-process
    // `signal_port` RawPort *first* — the bus wire is the canonical
    // driver. Signalfd notification follows as a bus subscriber
    // reaction. Native bus subscribers (future pidfd, timerfd) wake
    // on the edge and re-read their respective pending state.
    // Single-bit fire per SIGNAL_GENERATED — the signum is carried
    // in the pending queues, not in the bus event.
    //
    // See: `txdoc:SIGNAL-ATTACHMENTS-CATALOG-SCHEMA-1`,
    // `docs/design/04_process-signals/SIGNAL_ATTACHMENTS_v1.md`.
    if let Some(payload) = target.payload.lock().as_ref() {
        payload.signal_port.fire(SIGNAL_GENERATED);
    }

    // bus subscriber reaction: push siginfo to signalfd queues
    crate::signalfd::notify_process_signal(target.key().raw(), sig);

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
        payload.threads.snapshot();

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
        // Phase E (stop-state): update the per-thread `stopped` flag
        // in addition to the summary stop_requested bit. The AST
        // checkpoint in thread_future reads `stopped` (AtomicBool)
        // without acquiring the payload lock. SIGSTOP sets it to
        // prevent userspace re-entry; SIGCONT clears it to allow
        // resumption.
        match sig.raw() {
            19 => thread_payload.set_stopped(true),
            18 => thread_payload.set_stopped(false),
            _ => {}
        }
        // D9-A: post the wake-hint to each thread's mailbox after
        // the summary mutation so a parked future re-polls and
        // observes the new `stop_requested` bit. The routing tag is
        // `ProcessDirected` — SIGSTOP/SIGCONT are process-wide
        // control ops; per D9 §5 these "control ops, not
        // readiness transitions" still warrant a wake-hint because
        // the parked future has to notice the summary change.
        post_signal_mailbox(&thread_payload, sig, SignalRouting::ProcessDirected);
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
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    let guard = step_engine::guard();
    let mut delivered = 0usize;
    let catchable = !is_gewalt(sig);
    let members = pgrp.members.snapshot_live(&guard);
    for member in &members {
        if step_kill_process(member, sig, None) == KillOutcome::Delivered {
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
    // observe
    // upgrade
    // reserve
    // commit
    // publish
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
    info: Option<SigInfo>,
) -> Result<KillScriptOutcome, Errno> {
    use crate::cred::checks::{authorize_signal_send, AuthOutcome};

    match authorize_signal_send(source, target, sig)? {
        AuthOutcome::NoLiveTarget => return Ok(KillScriptOutcome::NoLiveThread),
        AuthOutcome::Authorized => {}
    }
    // Commit. `step_kill_process` is the primitive (no cred check);
    // authorize_signal_send dropped its guard before returning, so
    // post_signal's inner guard for SigInfo storage doesn't nest.
    Ok(match step_kill_process(target, sig, info) {
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
    use crate::cred::checks::{authorize_signal_send, AuthOutcome};

    // Use SIGTERM as the rule's signum input — the no-deliver probe
    // applies the same rule POSIX kill(pid, 0) does, which is
    // signum-independent except for SIGCONT-same-session. SIGTERM
    // doesn't trigger the SIGCONT bypass, matching how userspace
    // expects kill(pid, 0) to behave.
    match authorize_signal_send(source, target, Signum::SIGTERM)? {
        AuthOutcome::NoLiveTarget => Ok(KillScriptOutcome::NoLiveThread),
        AuthOutcome::Authorized => Ok(KillScriptOutcome::Probed),
    }
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
    let guard = step_engine::guard();
    script_kill_pgrp_with_guard(source, pgrp, sig, &guard)
}

pub(crate) fn script_kill_pgrp_with_guard(
    source: &Cap<ProcessIdentity>,
    pgrp: &Cap<ProcessGroup>,
    sig: Signum,
    guard: &Guard<'_>,
) -> Result<u32, Errno> {
    use crate::cred::checks::{authorize_signal_send_under_guard, AuthOutcome};

    let source_snapshot = source.cred_snapshot().ok_or(Errno::ESRCH)?;

    let mut delivered = 0u32;
    let members: alloc::vec::Vec<Cap<ProcessIdentity>> = pgrp.members.snapshot_live(guard);

    for member in &members {
        // Per-member cred check using the snapshot captured once
        // outside the loop. Denials / no-live-target both fold into
        // "skip" — SIGNAL_v1 §12.2 members-independent rule.
        match authorize_signal_send_under_guard(&source_snapshot, source, member, sig, guard) {
            Ok(AuthOutcome::Authorized) => {}
            Ok(AuthOutcome::NoLiveTarget) | Err(_) => continue,
        }
        if step_kill_process(member, sig, None) == KillOutcome::Delivered {
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

/// Cred-checked counterpart to [`deliver_posix_signal`].
///
/// Runs `cred::require_signal_send` against `source`'s syscall-entry
/// snapshot before invoking the disposition-aware delivery primitive.
/// Used by `sys_tkill` / `sys_tgkill` so a thread-targeted post is
/// authorised by the same POSIX rule as a process-targeted `kill`
/// (txKernel has no per-thread cred today; the rule resolves to the
/// owning process's cred).
///
/// `target` may be [`SignalTarget::Process`] or
/// [`SignalTarget::Thread`]. The `ProcessGroup` variant is rejected
/// with `Ok(NoLiveThread)` — pgrp fanout flows through
/// [`script_kill_pgrp`] instead.
///
/// Returns:
/// - `Ok(Delivered)` / `Ok(NoLiveThread)` — outcome of the delivery
///   primitive after a successful cred check.
/// - `Err(Errno::ESRCH)` — `source` is a zombie.
/// - `Err(Errno::EPERM)` — cred check denied the post.
pub fn script_deliver_signal(
    source: &Cap<ProcessIdentity>,
    target: SignalTarget,
    sig: Signum,
) -> Result<KillOutcome, Errno> {
    // Resolve the target's owning process *before* taking the
    // auth-phase guard — `upgrade_owner_proc()` takes its own guard
    // internally, and nesting would trip the no-nested-guard
    // invariant.
    let target_proc = match &target {
        SignalTarget::Process(cap) => cap.clone(),
        SignalTarget::Thread(t) => match t.upgrade_owner_proc() {
            Some(p) => p,
            None => return Ok(KillOutcome::NoLiveThread),
        },
        // Pgrp fanout has its own cred-checked script
        // (`script_kill_pgrp`). Reject here defensively rather than
        // delivering unchecked.
        SignalTarget::ProcessGroup(_) => return Ok(KillOutcome::NoLiveThread),
    };

    // Phase 1 — authorise. authorize_signal_send takes + drops its
    // own guard, so the commit phase can take its own guard for
    // SigInfo storage / weak upgrades without nesting.
    use crate::cred::checks::{authorize_signal_send, AuthOutcome};
    match authorize_signal_send(source, &target_proc, sig)? {
        AuthOutcome::NoLiveTarget => return Ok(KillOutcome::NoLiveThread),
        AuthOutcome::Authorized => {}
    }

    // Phase 2 — commit. We pass `SignalTarget::Process(target_proc)`
    // (the already-resolved owning process) rather than the original
    // `target`, even for thread-targeted calls. Reasons:
    //   • `deliver_posix_signal`'s `SignalTarget::Thread` branch
    //     calls `upgrade_owner_proc` under a held guard, which would
    //     nest a guard inside `Weak::upgrade`'s own guard and trip
    //     the no-nested-guard invariant.
    //   • txKernel currently delivers process-targeted even for
    //     `tkill`/`tgkill` (thread-specific delivery is a later
    //     phase); routing through Process matches that behaviour.
    // `deliver_posix_signal` honours the target's `sig_actions` for
    // catchable signals, default-action mapping for unhandled ones,
    // and gewalt routing for SIGKILL/SIGSTOP/SIGCONT.
    drop(target);
    Ok(deliver_posix_signal(
        SignalTarget::Process(target_proc),
        sig,
    ))
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
    let guard = step_engine::guard();
    deliver_tty_dispatch_with_guard(source, dispatch, &guard)
}

pub fn deliver_tty_dispatch_with_guard(
    source: &Cap<ProcessIdentity>,
    dispatch: crate::tty::execution::SignalDispatch,
    guard: &Guard<'_>,
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

// -- PR-2 StepOp wraps -------------------------------------------------
//
// Per `docs/Txv3/03_STEP_MODEL_v2.md` §2.1, PR-2 wraps each free
// `step_*` fn in an `impl StepOp for FooOp` shell. The signal-shim
// mutators here take `&Cap<_>` references; the wraps store the cap by
// value (`Cap` is `Clone`) per the PR-2 convention demonstrated by
// `cred::SetuidOp`. None of these fns take an epoch `Guard`, so the
// wraps need no lifetime parameter. Each `step()` body delegates to
// the free fn unchanged and lifts the return into `StepOutcome::Done`.

/// `StepOp` wrap for [`step_kill_process`]. PR-2 wave 2.
pub struct KillProcessOp {
    pub target: Cap<ProcessIdentity>,
    pub sig: Signum,
    pub info: Option<SigInfo>,
}

impl<I: SubjectIdentity> StepOp<I> for KillProcessOp {
    type Output = KillOutcome;
    type Progress = NoProgress;
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        StepOutcome::Done(step_kill_process(&self.target, self.sig, self.info))
    }
}

impl<I: SubjectIdentity> OneShotStepOp<I> for KillProcessOp {}

/// `StepOp` wrap for [`step_kill_pgrp`]. PR-2 wave 2.
pub struct KillPgrpOp {
    pub pgrp: Cap<ProcessGroup>,
    pub sig: Signum,
}

impl<I: SubjectIdentity> StepOp<I> for KillPgrpOp {
    type Output = usize;
    type Progress = NoProgress;
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        StepOutcome::Done(step_kill_pgrp(&self.pgrp, self.sig))
    }
}

impl OneShotStepOp<crate::process::ProcessIdentity> for KillPgrpOp {}

/// `StepOp` wrap for [`deliver_posix_signal`].
pub struct DeliverSignalOp {
    pub target: SignalTarget,
    pub sig: Signum,
}

impl<I: SubjectIdentity> StepOp<I> for DeliverSignalOp {
    type Output = KillOutcome;
    type Progress = NoProgress;
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        StepOutcome::Done(deliver_posix_signal(self.target.clone(), self.sig))
    }
}

impl<I: SubjectIdentity> OneShotStepOp<I> for DeliverSignalOp {}

/// `StepOp` wrap for [`step_sigaction`]. PR-2 wave 2.
pub struct SigactionOp {
    pub process: Cap<ProcessIdentity>,
    pub sig: Signum,
    pub disposition: SigDisposition,
}

impl<I: SubjectIdentity> StepOp<I> for SigactionOp {
    type Output = SigDispositionChange;
    type Progress = NoProgress;
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        StepOutcome::Done(step_sigaction(&self.process, self.sig, self.disposition))
    }
}

impl OneShotStepOp<crate::process::ProcessIdentity> for SigactionOp {}

#[cfg(test)]
mod step_op_wraps {
    //! PR-2 wave-2 `StepOp` wrap tests for the signal shim. Each test
    //! exercises one wrap against a `bootstrap_init_process`-minted
    //! cap, confirming the wrap delegates to the free fn and lifts
    //! the result into `StepOutcome::Done`. Permission-rule and
    //! routing semantics are covered by the existing free-fn tests
    //! in `signal::tests`.
    use super::*;
    use crate::process::bootstrap_init_process;
    use crate::process::structure::reset_pid_counter_for_test;
    use crate::signal::adapter::step_engine::{
        PlaceholderProcessSubject, ScriptCtx, StepOp, StepOutcome,
    };
    use crate::test_support::EPOCH_TEST_LOCK;
    use crate::thread_runtime::structure::reset_tid_counter_for_test;
    use crate::vm::{AddressSpace, TestPmap};
    use crate::zones;
    fn setup() -> std::sync::MutexGuard<'static, ()> {
        let guard = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        tx_test_support::init_host();
        let _ = zones::register_all();
        tx_test_support::drain_to_quiescence();
        reset_pid_counter_for_test();
        reset_tid_counter_for_test();
        crate::process::execution::reset_init_process_for_test();
        guard
    }

    fn fresh_aspace() -> Cap<crate::vm::AddressSpace> {
        AddressSpace::new_cap_for_platform::<TestPmap>().expect("fresh aspace")
    }

    #[test]
    fn kill_process_op_delegates_to_step_kill_process() {
        let _g = setup();
        let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
        let mut op = KillProcessOp {
            target: proc_cap.clone(),
            sig: Signum::SIGTERM,
            info: None,
        };
        let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
        let outcome = op.step(&mut ctx);
        assert_eq!(outcome, StepOutcome::Done(KillOutcome::Delivered));
    }

    #[test]
    fn kill_pgrp_op_delegates_to_step_kill_pgrp() {
        let _g = setup();
        let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
        let pgrp = proc_cap.pgrp_cap();
        let mut op = KillPgrpOp {
            pgrp,
            sig: Signum::SIGINT,
        };
        let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
        let outcome = op.step(&mut ctx);
        // bootstrap_init_process gives a single-member pgrp.
        assert_eq!(outcome, StepOutcome::Done(1usize));
    }

    #[test]
    fn sigaction_op_delegates_to_step_sigaction() {
        let _g = setup();
        let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
        let mut op = SigactionOp {
            process: proc_cap.clone(),
            sig: Signum::SIGTERM,
            disposition: SigDisposition::Ignore,
        };
        let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
        let outcome = op.step(&mut ctx);
        match outcome {
            StepOutcome::Done(SigDispositionChange::Replaced {
                prev: SigDisposition::Default,
            }) => {}
            other => panic!("expected Done(Replaced{{Default}}), got {other:?}"),
        }
    }

    #[test]
    fn sigaction_op_uncatchable_signum_returns_uncatchable() {
        let _g = setup();
        let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
        let mut op = SigactionOp {
            process: proc_cap.clone(),
            sig: Signum::SIGKILL,
            disposition: SigDisposition::Ignore,
        };
        let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
        let outcome = op.step(&mut ctx);
        assert_eq!(
            outcome,
            StepOutcome::Done(SigDispositionChange::Uncatchable(SigDisposition::Default))
        );
    }
}

#[cfg(test)]
mod tests;
