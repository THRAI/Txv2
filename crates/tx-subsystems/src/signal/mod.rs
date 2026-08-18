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
//! - Shims: [`step_kill_process_with_post`], [`step_kill_pgrp_with_post`],
//!   [`step_sigaction`].
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

use alloc::sync::Weak as ArcWeak;
use core::sync::atomic::{AtomicU64, Ordering};

pub mod adapter;

use adapter::step_engine::{
    self, Cap, Guard, MailboxEvent, NoProgress, OneShotStepOp, OperationalCapExt, PayloadCap,
    ScriptCtx, SignalRouting, SpinMutex, StepOp, StepOutcome, SubjectIdentity, TaskMailbox,
};

use crate::execution::Errno;
use crate::process::structure::{ProcessGroup, ProcessIdentity, ProcessPayload, SIGNAL_GENERATED};
use crate::thread_runtime::execution::{post_signal_mailbox_with_post, post_signal_with_post};

fn direct_task_mailbox_post(weak: ArcWeak<TaskMailbox>, event: MailboxEvent) {
    let Some(mailbox) = weak.upgrade() else {
        return;
    };
    let _ = mailbox.post(event);
}

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
    pub const GLIBC_SIGCANCEL: Self = Self(32);
    pub const MUSL_SIGCANCEL: Self = Self(33);

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

    /// Linux libcs reserve internal realtime signals for pthread
    /// cancellation. These handlers inspect the delivered ucontext more
    /// tightly than ordinary application handlers.
    pub const fn is_libc_sigcancel(self) -> bool {
        matches!(self.0, 32 | 33)
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

    pub const fn union(self, other: Self) -> Self {
        Self::new(self.0 | other.0)
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

impl SigDisposition {
    pub const fn handler(addr: usize) -> Self {
        Self::Handler(addr)
    }
}

/// POSIX sigaction flags stored alongside each disposition.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SaFlags {
    bits: u64,
}

impl SaFlags {
    pub const EMPTY: Self = Self { bits: 0 };
    pub const NOCLDSTOP: Self = Self { bits: 1 };
    pub const NOCLDWAIT: Self = Self { bits: 2 };
    pub const SIGINFO: Self = Self { bits: 4 };
    pub const ONSTACK: Self = Self { bits: 0x0800_0000 };
    pub const RESTART: Self = Self { bits: 0x1000_0000 };
    pub const NODEFER: Self = Self { bits: 0x4000_0000 };
    pub const RESETHAND: Self = Self { bits: 0x8000_0000 };

    pub const fn new(bits: u64) -> Self {
        Self { bits }
    }

    pub const fn bits(self) -> u64 {
        self.bits
    }

    pub const fn contains(self, flag: Self) -> bool {
        (self.bits & flag.bits) == flag.bits
    }
}

impl core::ops::BitOr for SaFlags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self::new(self.bits | rhs.bits)
    }
}

impl core::ops::BitOrAssign for SaFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.bits |= rhs.bits;
    }
}

/// Full per-signal action entry required by POSIX `sigaction(2)`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SigActionEntry {
    pub disposition: SigDisposition,
    pub flags: SaFlags,
    pub sa_mask: SignalMask,
    /// ABI spare/restorer word. On RV64 musl this is the unused
    /// fourth word of `struct k_sigaction` because the architecture
    /// has no `SA_RESTORER`; other Linux ABIs may use the same storage
    /// as a userspace restorer pointer. txKernel writes its own stack
    /// trampoline during delivery either way.
    pub restorer: usize,
}

impl SigActionEntry {
    pub const DEFAULT: Self = Self {
        disposition: SigDisposition::Default,
        flags: SaFlags::EMPTY,
        sa_mask: SignalMask::EMPTY,
        restorer: 0,
    };

    pub const fn new(
        disposition: SigDisposition,
        flags: SaFlags,
        sa_mask: SignalMask,
        restorer: usize,
    ) -> Self {
        Self {
            disposition,
            flags,
            sa_mask,
            restorer,
        }
    }

    pub const fn handler(addr: usize) -> Self {
        Self::new(
            SigDisposition::Handler(addr),
            SaFlags::EMPTY,
            SignalMask::EMPTY,
            0,
        )
    }
}

impl From<SigDisposition> for SigActionEntry {
    fn from(disposition: SigDisposition) -> Self {
        Self::new(disposition, SaFlags::EMPTY, SignalMask::EMPTY, 0)
    }
}

/// Per-process signal-action table. One [`SigActionEntry`] slot per
/// signum. `SIGKILL` / `SIGSTOP` slots are ignored on writes to honor
/// the uncatchable invariant.
pub struct SigActionTable {
    entries: SpinMutex<[SigActionEntry; Signum::MAX as usize]>,
}

impl Clone for SigActionTable {
    fn clone(&self) -> Self {
        Self {
            entries: SpinMutex::new(*self.entries.lock()),
        }
    }
}

impl Default for SigActionTable {
    fn default() -> Self {
        Self::new()
    }
}

impl SigActionTable {
    pub fn new() -> Self {
        Self {
            entries: SpinMutex::new([SigActionEntry::DEFAULT; Signum::MAX as usize]),
        }
    }

    pub fn get_entry(&self, sig: Signum) -> SigActionEntry {
        self.entries.lock()[(sig.raw() - 1) as usize]
    }

    pub fn get(&self, sig: Signum) -> SigDisposition {
        self.get_entry(sig).disposition
    }

    pub fn set_entry(&self, sig: Signum, entry: SigActionEntry) {
        if sig.is_uncatchable() {
            return;
        }
        self.entries.lock()[(sig.raw() - 1) as usize] = entry;
    }

    pub fn set(&self, sig: Signum, disposition: SigDisposition) {
        self.set_entry(sig, SigActionEntry::from(disposition));
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
            if matches!(slot.disposition, SigDisposition::Handler(_)) {
                *slot = SigActionEntry::DEFAULT;
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
/// view kept current by catchable-signal posting, `step_sigprocmask`, and the
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

#[cfg(any(test, tx_signal_select_metrics))]
pub(crate) const SIGNAL_SELECT_TRACE_NAMES: &[&[u8]] = &[
    b"debug.signal.select.thread1.lock.request",
    b"debug.signal.select.thread1.lock.acquired",
    b"debug.signal.select.thread1.lock.release",
    b"debug.signal.select.owner.upgrade.request",
    b"debug.signal.select.owner.upgrade.done",
    b"debug.signal.select.owner.upgrade.miss",
    b"debug.signal.select.proc.lock.request",
    b"debug.signal.select.proc.lock.acquired",
    b"debug.signal.select.proc.lock.release",
    b"debug.signal.select.thread2.lock.request",
    b"debug.signal.select.thread2.lock.acquired",
    b"debug.signal.select.thread2.lock.release",
    b"debug.signal.select.thread_pending.hit",
    b"debug.signal.select.group_pending.hit",
    b"debug.signal.select.done",
];

fn emit_signal_select_trace(name: &[u8], value: i64) {
    #[cfg(tx_signal_select_metrics)]
    {
        let _ = value;
        if let Some(observer) = tx_observe::current() {
            debug_assert!(SIGNAL_SELECT_TRACE_NAMES.contains(&name));
            observer.debug_counter(name, value);
        }
    }
    #[cfg(not(tx_signal_select_metrics))]
    {
        let _ = name;
        let _ = value;
    }
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
    emit_signal_select_trace(b"debug.signal.select.thread1.lock.request", 1);
    let payload_guard = thread.payload.lock();
    emit_signal_select_trace(b"debug.signal.select.thread1.lock.acquired", 1);
    let Some(payload) = payload_guard.as_ref() else {
        emit_signal_select_trace(b"debug.signal.select.thread1.lock.release", 0);
        drop(payload_guard);
        emit_signal_select_trace(b"debug.signal.select.done", 0);
        return None;
    };
    let mask = payload.signal_mask();

    // Thread-directed pending first.
    let t_deliverable = payload.pending().deliverable_bits(mask);
    if let Some(sig) = lowest_signum_bit(t_deliverable) {
        emit_signal_select_trace(b"debug.signal.select.thread_pending.hit", sig.raw() as i64);
        emit_signal_select_trace(b"debug.signal.select.thread1.lock.release", 1);
        drop(payload_guard);
        emit_signal_select_trace(b"debug.signal.select.done", 1);
        return Some((sig, PendingSource::Thread));
    }
    emit_signal_select_trace(b"debug.signal.select.thread1.lock.release", 2);
    drop(payload_guard);

    // Group-directed pending next.
    let guard = step_engine::guard();
    emit_signal_select_trace(b"debug.signal.select.owner.upgrade.request", 1);
    let proc = thread.owner_proc.upgrade(&guard);
    emit_signal_select_trace(b"debug.signal.select.owner.upgrade.done", 1);
    let Some(proc) = proc else {
        emit_signal_select_trace(b"debug.signal.select.owner.upgrade.miss", 1);
        drop(guard);
        emit_signal_select_trace(b"debug.signal.select.done", 0);
        return None;
    };
    drop(guard);

    emit_signal_select_trace(b"debug.signal.select.proc.lock.request", 1);
    let proc_payload_guard = proc.payload.lock();
    emit_signal_select_trace(b"debug.signal.select.proc.lock.acquired", 1);
    let Some(proc_payload) = proc_payload_guard.as_ref() else {
        emit_signal_select_trace(b"debug.signal.select.proc.lock.release", 0);
        drop(proc_payload_guard);
        emit_signal_select_trace(b"debug.signal.select.done", 0);
        return None;
    };

    // Mask check uses the same per-thread mask (re-read in case it
    // changed between blocks; cheap).
    emit_signal_select_trace(b"debug.signal.select.thread2.lock.request", 1);
    let thread_payload_guard = thread.payload.lock();
    emit_signal_select_trace(b"debug.signal.select.thread2.lock.acquired", 1);
    let mask = thread_payload_guard
        .as_ref()
        .map(|p| p.signal_mask())
        .unwrap_or(SignalMask::EMPTY);
    emit_signal_select_trace(b"debug.signal.select.thread2.lock.release", 1);
    drop(thread_payload_guard);

    let g_deliverable = proc_payload.group_pending().deliverable_bits(mask);
    let result = lowest_signum_bit(g_deliverable).map(|sig| {
        emit_signal_select_trace(b"debug.signal.select.group_pending.hit", sig.raw() as i64);
        (sig, PendingSource::Group)
    });
    emit_signal_select_trace(b"debug.signal.select.proc.lock.release", 1);
    drop(proc_payload_guard);
    emit_signal_select_trace(
        b"debug.signal.select.done",
        if result.is_some() { 2 } else { 0 },
    );
    result
}

/// True iff the thread has a pending, deliverable signal that should interrupt
/// a blocking syscall with EINTR, per POSIX:
///   - ignored signal (explicit Ignore, or default action Ignore e.g. SIGCHLD)
///     → does NOT interrupt;
///   - default disposition with a non-ignore action (Term/Core/Stop/Cont, e.g.
///     SIGTERM) → interrupts (the AST checkpoint then terminates/stops);
///   - caught (handler) without SA_RESTART → interrupts (handler delivery);
///   - caught with SA_RESTART → does NOT interrupt (syscall restarts).
///
/// Blocking syscalls (`ppoll`/`pselect`/`recv`/`send`/`wait4`) use this instead
/// of a broad "any pending signal" test. The broad test spuriously interrupted
/// `select`/`ppoll` on benign signals (notably SIGCHLD, which netserver gets as
/// it reaps connection children) and broke netperf. This precise test still lets
/// `kill`'s SIGTERM tear down a process blocked in a syscall (hackbench workers
/// parked in ppoll) without disturbing programs that ignore or SA_RESTART-handle
/// signals while polling.
pub fn thread_pending_signal_interrupts(
    thread: &Cap<crate::thread_runtime::ThreadIdentity>,
) -> bool {
    let Ok(thread_payload) = thread.upgrade_operational() else {
        return false;
    };
    let Some(process) = thread.upgrade_owner_proc() else {
        return false;
    };
    let Ok(process_payload) = process.upgrade_operational() else {
        return false;
    };

    // This is a read-only predicate: inspect every currently deliverable bit
    // without dequeuing it. A lower-numbered ignored or restartable signal must
    // not hide a later signal that should abort the wait.
    let mask = thread_payload.signal_mask();
    let mut pending = thread_payload.pending().deliverable_bits(mask)
        | process_payload.group_pending().deliverable_bits(mask);
    while let Some(sig) = lowest_signum_bit(pending) {
        pending &= !sig.bit();
        let entry = process_payload.sig_actions().get_entry(sig);
        if signal_action_interrupts_wait(sig, entry) {
            return true;
        }
    }
    false
}

fn signal_action_interrupts_wait(sig: Signum, entry: SigActionEntry) -> bool {
    match entry.disposition {
        SigDisposition::Ignore => false,
        SigDisposition::Default => !matches!(default_action(sig), DefaultAction::Ignore),
        SigDisposition::Handler(_) => {
            // A pending handler interrupts a blocking syscall so the handler can
            // be delivered. Without SA_RESTART the syscall returns EINTR; with
            // SA_RESTART, Linux delivers the handler then RESTARTS the syscall.
            // We have no syscall-restart machinery, so for SA_RESTART handlers
            // we normally do NOT interrupt (returning EINTR would be a spurious,
            // un-restarted failure — it broke netperf on benign SIGCHLD, and
            // breaks LTP wait4/poll which rely on SA_RESTART, e.g. the tst
            // harness's parent waitpid taking SIGUSR1/SIGALRM cleanup signals).
            //
            // SIGINT is the interactive-teardown exception: cyclictest's
            // `kill -2 $hackbench` relies on hackbench's SIGINT handler running
            // to reap its workers even though glibc's signal() installs the
            // handler with SA_RESTART.
            //
            // Libc cancellation signals are the other narrow exception.
            // glibc/musl wrap cancelable syscalls such as futex waits in their
            // own restart/cancel logic; the kernel must wake the blocked wait so
            // the wrapper can observe pending cancellation instead of silently
            // re-parking under SA_RESTART.
            !entry.flags.contains(SaFlags::RESTART)
                || sig == Signum::SIGINT
                || sig.is_libc_sigcancel()
        }
    }
}

/// A caught/default-action signal ends `rt_sigsuspend` even when the handler
/// has `SA_RESTART`.  Inspect all deliverable bits so an ignored lower-numbered
/// signal cannot hide a caught higher-numbered one.
pub fn thread_pending_signal_ends_sigsuspend(
    thread: &Cap<crate::thread_runtime::ThreadIdentity>,
) -> bool {
    let Ok(thread_payload) = thread.upgrade_operational() else {
        return false;
    };
    let Some(process) = thread.upgrade_owner_proc() else {
        return false;
    };
    let Ok(process_payload) = process.upgrade_operational() else {
        return false;
    };

    let mask = thread_payload.signal_mask();
    let mut pending = thread_payload.pending().deliverable_bits(mask)
        | process_payload.group_pending().deliverable_bits(mask);
    while let Some(sig) = lowest_signum_bit(pending) {
        pending &= !sig.bit();
        match process_payload.sig_actions().get(sig) {
            SigDisposition::Ignore => {}
            SigDisposition::Default if matches!(default_action(sig), DefaultAction::Ignore) => {}
            SigDisposition::Default | SigDisposition::Handler(_) => return true,
        }
    }
    false
}

fn refresh_deliverable_signal_summary_fast(
    thread: &Cap<crate::thread_runtime::ThreadIdentity>,
) -> bool {
    let Some(payload) = thread.payload_cap() else {
        return false;
    };
    refresh_deliverable_signal_summary_with_payload(thread, &payload)
}

pub(crate) fn refresh_deliverable_signal_summary_with_payload(
    _thread: &Cap<crate::thread_runtime::ThreadIdentity>,
    payload: &PayloadCap<crate::thread_runtime::ThreadPayload>,
) -> bool {
    let mask = payload.signal_mask();
    let t_deliverable = payload.pending().deliverable_bits(mask);
    let g_hint_deliverable = payload.group_pending_summary() & !mask.raw_bits();
    let deliverable = t_deliverable != 0 || g_hint_deliverable != 0;
    payload.update_summary(|s| s.deliverable_signal = deliverable);
    deliverable
}

fn sync_group_pending_summaries(
    process_payload: &ProcessPayload,
    wake_sig: Option<Signum>,
) -> bool {
    let group_pending = process_payload.group_pending_snapshot();
    let mut touched = false;
    for thread in process_payload.threads.snapshot() {
        if let Some(thread_payload) = sync_thread_group_pending_summary(process_payload, &thread) {
            if let Some(sig) = wake_sig {
                if (group_pending & sig.bit()) != 0 && !thread_payload.signal_mask().is_blocked(sig)
                {
                    let _ = post_signal_mailbox_with_post(
                        &thread_payload,
                        sig,
                        SignalRouting::ProcessDirected,
                        |weak, event| {
                            let Some(mailbox) = weak.upgrade() else {
                                return;
                            };
                            let _ = mailbox.post(event);
                        },
                    );
                }
            }
            touched = true;
        }
    }
    touched
}

pub(crate) fn sync_thread_group_pending_summary(
    process_payload: &ProcessPayload,
    thread: &Cap<crate::thread_runtime::ThreadIdentity>,
) -> Option<
    crate::thread_runtime::adapter::step_engine::PayloadCap<crate::thread_runtime::ThreadPayload>,
> {
    let thread_payload = thread.upgrade_operational().ok()?;
    let mask = thread_payload.signal_mask();
    let group_pending = process_payload.group_pending_snapshot();
    let deliverable = thread_payload.pending().deliverable_bits(mask) != 0
        || (group_pending & !mask.raw_bits()) != 0;
    thread_payload.store_group_pending_summary(group_pending);
    thread_payload.update_summary(|s| s.deliverable_signal = deliverable);
    Some(thread_payload)
}

/// Refresh the denormalised interrupt-summary deliverability bit from
/// the authoritative pending queues and the thread's current mask.
///
/// Use this after consumers dequeue a signal or after a direct
/// signal-mask restore outside [`thread_runtime::execution::step_sigprocmask`].
/// Termination and stop bits are left untouched.
pub fn refresh_deliverable_signal_summary(
    thread: &Cap<crate::thread_runtime::ThreadIdentity>,
) -> bool {
    refresh_deliverable_signal_summary_fast(thread)
}

pub(crate) fn post_group_pending_signal(process: &Cap<ProcessIdentity>, sig: Signum) -> bool {
    let Ok(payload) = process.upgrade_operational() else {
        return false;
    };
    payload.group_pending().post(sig);
    sync_group_pending_summaries(&payload, Some(sig))
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
    /// the process group-exit transition with an exit status that encodes
    /// "killed by sig".
    DefaultTerminate { sig: Signum },
    /// Default termination was selected, but process lifecycle ownership
    /// prevented the exit transition. The signal has been requeued and the
    /// caller must yield before retrying the AST checkpoint.
    DefaultTerminateDeferred { sig: Signum },
    /// Default action is `Stop` (SIGSTOP-family). Thread should park
    /// on the stop channel; `THREAD_RUNTIME_v1` §6 owns the state
    /// machine. Day-1 just recognises the intent.
    DefaultStop { sig: Signum },
    /// Default action is `Cont` (SIGCONT). Continue control op.
    DefaultContinue { sig: Signum },
    /// User-installed handler and the sigaction metadata needed for
    /// frame construction.
    DeliverHandler { sig: Signum, action: SigActionEntry },
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
    if !summary.deliverable_signal && !summary.stop_requested {
        return AstOutcome::Continue;
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
                sync_group_pending_summaries(&proc_payload, None);
            }
        }
        refresh_deliverable_signal_summary(thread);

        let action = match proc.upgrade_operational() {
            Ok(payload) => payload.sig_actions().get_entry(sig),
            Err(_) => return AstOutcome::Continue,
        };

        match action.disposition {
            SigDisposition::Ignore => continue,
            SigDisposition::Default => match default_action(sig) {
                DefaultAction::Ignore => continue,
                DefaultAction::Term | DefaultAction::Core => {
                    return AstOutcome::DefaultTerminate { sig };
                }
                DefaultAction::Stop => return AstOutcome::DefaultStop { sig },
                DefaultAction::Cont => return AstOutcome::DefaultContinue { sig },
            },
            SigDisposition::Handler(_) => return AstOutcome::DeliverHandler { sig, action },
        }
    }
}

/// Run `ast_check` and materialise its outcome with the day-1
/// side-effects we have wired:
///
/// - `DefaultTerminate { sig }` invokes
///   the fatal group-exit transition on the thread's owner
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
            let exit = crate::process::execution::step_exit_group_with_signal_with_posts(
                &proc,
                sig,
                direct_task_mailbox_post,
                |mailbox, event| mailbox.post(event),
            );
            if exit == crate::process::ProcessExitOutcome::Retry {
                post_group_pending_signal(&proc, sig);
                return AstOutcome::DefaultTerminateDeferred { sig };
            }
        }
    }
    outcome
}

/// Entry-side AST checkpoint for a caller which already owns the live thread
/// payload driving the current userspace task.
///
/// The no-signal case is the normal syscall-return path.  Reading its packed
/// summary directly avoids taking `ThreadIdentity.payload` merely to clone the
/// same payload cap the caller already holds.  Interesting summaries retain
/// the canonical [`ast_dispatch`] path and its full lifecycle revalidation.
pub fn ast_dispatch_with_payload(
    thread: &Cap<crate::thread_runtime::ThreadIdentity>,
    payload: &PayloadCap<crate::thread_runtime::ThreadPayload>,
) -> AstOutcome {
    let summary = payload.interrupt_summary();
    if !summary.termination && !summary.deliverable_signal && !summary.stop_requested {
        AstOutcome::Continue
    } else {
        ast_dispatch(thread)
    }
}

/// Result of a kill-style shim. `Delivered` if at least one thread
/// received the post; `NoLiveThread` if the target is a zombie or its
/// thread list is empty.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KillOutcome {
    Delivered,
    NoLiveThread,
    Retry,
}

fn kill_outcome_from_exit(outcome: crate::process::ProcessExitOutcome) -> KillOutcome {
    match outcome {
        crate::process::ProcessExitOutcome::Completed => KillOutcome::Delivered,
        crate::process::ProcessExitOutcome::Retry => KillOutcome::Retry,
    }
}

/// Deliver a synchronous fault signal (SIGSEGV, SIGILL, SIGBUS, etc.)
/// to the current thread's owning process.
///
/// Per `SIGNAL_v1` §20, synchronous faults are delivered on the
/// trapping thread and **bypass the signal mask** — the fault
/// occurs regardless of `sigprocmask` state.  The delivery path
/// differs from ordinary POSIX signal delivery in two critical ways:
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
///    the fatal group-exit transition.
///
/// The `thread` argument supplies the trapping thread's `Cap` so
///    the function can resolve the owning process.  The faulting
///    signum must be one of the synchronous set (SIGSEGV, SIGILL,
///    SIGBUS, SIGFPE, SIGTRAP, SIGSYS); other signums panic.
///
/// See: `txdoc:SIGNAL-V1-S20-SYNCHRONOUS-FAULT`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SynchronousFaultOutcome {
    HandlerQueued,
    ProcessExit(crate::process::ProcessExitOutcome),
}

pub fn deliver_synchronous_fault(
    thread: &Cap<crate::thread_runtime::ThreadIdentity>,
    sig: Signum,
) -> SynchronousFaultOutcome {
    let guard = step_engine::guard();
    if let Some(process) = thread.owner_proc.upgrade(&guard) {
        drop(guard);
        let disposition = match process.upgrade_operational() {
            Ok(payload) => payload.sig_actions().get(sig),
            Err(_) => {
                return SynchronousFaultOutcome::ProcessExit(
                    crate::process::ProcessExitOutcome::Completed,
                );
            }
        };
        if matches!(disposition, SigDisposition::Handler(_)) {
            let info = SigInfo {
                si_signo: sig.raw() as u32,
                // Linux ILL_ILLOPC. Other synchronous signals currently do
                // not inspect this field, but SIGILL SA_SIGINFO handlers do.
                si_code: 1,
                si_pid: 0,
                si_uid: 0,
            };
            post_signal_with_post(
                thread,
                sig,
                SignalRouting::ThreadDirected {
                    tid: thread.tid.0 as u64,
                },
                Some(info),
                direct_task_mailbox_post,
            );
            return SynchronousFaultOutcome::HandlerQueued;
        }

        return SynchronousFaultOutcome::ProcessExit(
            crate::process::execution::step_exit_group_with_signal_with_posts(
                &process,
                sig,
                direct_task_mailbox_post,
                |mailbox, event| mailbox.post(event),
            ),
        );
    }
    SynchronousFaultOutcome::ProcessExit(crate::process::ProcessExitOutcome::Completed)
}

/// Deliver a signal via the canonical POSIX entry point.
///
/// This is the canonical POSIX signal delivery entry per `SIGNAL_v1`
/// §12. It accepts a `SignalTarget` and
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
pub fn deliver_posix_signal_with_post<F>(
    target: SignalTarget,
    sig: Signum,
    mut post: F,
) -> KillOutcome
where
    F: FnMut(ArcWeak<TaskMailbox>, MailboxEvent),
{
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
        return step_kill_process_with_post(&cap, sig, None, post);
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
                DefaultAction::Term | DefaultAction::Core => kill_outcome_from_exit(
                    crate::process::execution::step_exit_group_with_signal_with_posts(
                        &cap,
                        sig,
                        &mut post,
                        |mailbox, event| mailbox.post(event),
                    ),
                ),
                DefaultAction::Stop => {
                    route_gewalt_with_post(&cap, Signum::SIGSTOP, &mut post);
                    KillOutcome::Delivered
                }
                DefaultAction::Cont => {
                    route_gewalt_with_post(&cap, Signum::SIGCONT, &mut post);
                    KillOutcome::Delivered
                }
            }
        }
        SigDisposition::Handler(_handler) => {
            // Post to eligible thread's pending; AST delivers handler.
            step_kill_process_with_post(&cap, sig, None, post)
        }
    }
}

/// Direct-publication compatibility entry point.  The injected-post form is
/// the canonical implementation; this wrapper preserves callers from
/// final-smp while still using the main branch's mailbox publication rules.
pub fn deliver_posix_signal(target: SignalTarget, sig: Signum) -> KillOutcome {
    deliver_posix_signal_with_post(target, sig, direct_task_mailbox_post)
}

/// Deliver `sig` to `process` only when it has a user handler installed.
///
/// Returns `true` if a handler was present (signal posted; the AST runs it on
/// return to userspace), `false` otherwise (no-op). The ITIMER_REAL expiry path
/// uses this so an alarm-bounded blocking recv on a process *without* a SIGALRM
/// handler keeps its EINTR-only behaviour — delivering the default action would
/// terminate the process and regress e.g. LTP `recvfrom01` — while a process
/// that registered a handler (e.g. busybox `ping`'s interval-driven sender)
/// actually gets it run.
/// Whether a pending unblocked signal on `thread` would actually interrupt a
/// blocked slow syscall: a user handler is installed (EINTR + AST delivery on
/// return) or the default action terminates the process. Pending-but-ignored
/// signals (a shell's SIGCHLD churn) must NOT abort waits — Linux leaves the
/// task parked for those. Consulted by blocking socket waits before parking.
pub fn pending_signal_interrupts_wait(
    thread: &Cap<crate::thread_runtime::ThreadIdentity>,
    process: &Cap<ProcessIdentity>,
) -> bool {
    let Some(summary) = thread
        .payload_cap()
        .map(|payload| payload.interrupt_summary())
    else {
        return false;
    };
    if summary.termination {
        return true;
    }
    if !summary.deliverable_signal {
        return false;
    }
    // The summary bit is a denormalised hint; consult the real queues (the
    // bit can be momentarily stale after an AST delivery).
    let Some((sig, _source)) = select_next_signal(thread) else {
        return false;
    };
    let disposition = match process.upgrade_operational() {
        Ok(payload) => payload.sig_actions().get(sig),
        Err(_) => return false,
    };
    match disposition {
        SigDisposition::Handler(_) => true,
        SigDisposition::Default => matches!(
            default_action(sig),
            DefaultAction::Term | DefaultAction::Core
        ),
        SigDisposition::Ignore => false,
    }
}

pub fn deliver_signal_if_handler_with_post<F>(
    process: &Cap<ProcessIdentity>,
    sig: Signum,
    post: F,
) -> bool
where
    F: FnMut(ArcWeak<TaskMailbox>, MailboxEvent),
{
    let disposition = match process.upgrade_operational() {
        Ok(payload) => payload.sig_actions().get(sig),
        Err(_) => return false,
    };
    if matches!(disposition, SigDisposition::Handler(_)) {
        step_kill_process_with_post(process, sig, None, post);
        true
    } else {
        false
    }
}

pub fn deliver_signal_if_handler(process: &Cap<ProcessIdentity>, sig: Signum) -> bool {
    deliver_signal_if_handler_with_post(process, sig, direct_task_mailbox_post)
}

/// Target for POSIX signal delivery.
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
///   pending queue entirely and route through [`route_gewalt_with_post`],
///   which updates each thread's `signal_summary` in place.
/// - **Event** signums (catchable) route to a single eligible live
///   thread's `thread_pending` via `post_signal_with_post`.
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
///    deliverable_signal` stays `false` because the catchable-signal post mask
///    check filters the summary update.
///
/// The mailbox post happens via the catchable-signal post helper *after* the
/// thread-list lock is dropped — it does not reacquire
/// `payload.threads.inner.lock()`, so this order avoids any
/// post-while-holding-list-lock hazard. Zombies are skipped.
pub fn step_kill_process_with_post<F>(
    target: &Cap<ProcessIdentity>,
    sig: Signum,
    info: Option<SigInfo>,
    mut post: F,
) -> KillOutcome
where
    F: FnMut(ArcWeak<TaskMailbox>, MailboxEvent),
{
    step_kill_process_with_posts(target, sig, info, &mut post, |mailbox, event| {
        mailbox.post(event)
    })
}

pub fn step_kill_process(
    target: &Cap<ProcessIdentity>,
    sig: Signum,
    info: Option<SigInfo>,
) -> KillOutcome {
    step_kill_process_with_post(target, sig, info, direct_task_mailbox_post)
}

pub fn step_kill_process_with_posts<P, R>(
    target: &Cap<ProcessIdentity>,
    sig: Signum,
    info: Option<SigInfo>,
    mut post: P,
    mut source_post: R,
) -> KillOutcome
where
    P: FnMut(ArcWeak<TaskMailbox>, MailboxEvent),
    R: FnMut(&TaskMailbox, MailboxEvent) -> bool,
{
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    if is_gewalt(sig) {
        let outcome = route_gewalt_with_post(target, sig, &mut post);
        // D9-D: Gewalt routing does not currently fan out to signalfd
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
            crate::signalfd::notify_process_signal_with_post(
                target.key().raw(),
                sig,
                &mut source_post,
            );
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
        // atomic via `signal_mask()`, matching catchable-signal posting /
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
        // `threads` lock drops at end of scope before catchable-signal posting.
    };

    let Some(chosen) = chosen else {
        return KillOutcome::NoLiveThread;
    };

    // Store SigInfo in the process slots before posting.
    if let Some(ref sinfo) = info {
        target.siginfo_store(sig, *sinfo);
    }

    post_signal_with_post(
        &chosen,
        sig,
        SignalRouting::ProcessDirected,
        info,
        &mut post,
    );

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
    crate::signalfd::notify_process_signal_with_post(target.key().raw(), sig, &mut source_post);

    KillOutcome::Delivered
}

/// `true` for the three Gewalt signums per `SIGNAL_v1` §1: SIGKILL,
/// SIGSTOP, SIGCONT. These bypass the catchable pending queues and
/// are routed by [`route_gewalt_with_post`].
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
/// - **SIGKILL** -> invokes the fatal group-exit transition
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
pub fn route_gewalt_with_post<F>(
    target: &Cap<ProcessIdentity>,
    sig: Signum,
    mut post: F,
) -> KillOutcome
where
    F: FnMut(ArcWeak<TaskMailbox>, MailboxEvent),
{
    debug_assert!(
        is_gewalt(sig),
        "route_gewalt_with_post called with non-Gewalt signum"
    );

    if sig == Signum::SIGKILL {
        if target.is_zombie() {
            return KillOutcome::NoLiveThread;
        }
        return kill_outcome_from_exit(
            crate::process::execution::step_exit_group_with_signal_with_posts(
                target,
                sig,
                &mut post,
                |mailbox, event| mailbox.post(event),
            ),
        );
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
        let _ = post_signal_mailbox_with_post(
            &thread_payload,
            sig,
            SignalRouting::ProcessDirected,
            &mut post,
        );
        touched = true;
    }

    if touched {
        KillOutcome::Delivered
    } else {
        KillOutcome::NoLiveThread
    }
}

pub fn route_gewalt(target: &Cap<ProcessIdentity>, sig: Signum) -> KillOutcome {
    route_gewalt_with_post(target, sig, direct_task_mailbox_post)
}

/// Deliver `sig` to every live process in `pgrp`. Catchable signals
/// also get the bit set on each delivered member's `group_pending`
/// queue so the (future) delivery step can distinguish thread-
/// targeted from group-targeted posts. Gewalt signals bypass the
/// pending queues entirely per `SIGNAL_v1` §2 Consequence 2 — they
/// route through [`route_gewalt_with_post`] which directly updates each
/// thread's `signal_summary`.
///
/// Returns the count of processes that received the post.
pub fn step_kill_pgrp_with_post<F>(pgrp: &Cap<ProcessGroup>, sig: Signum, mut post: F) -> usize
where
    F: FnMut(ArcWeak<TaskMailbox>, MailboxEvent),
{
    let mut source_post = |mailbox: &TaskMailbox, event| mailbox.post(event);
    step_kill_pgrp_with_posts_dyn(pgrp, sig, &mut post, &mut source_post)
}

pub fn step_kill_pgrp(pgrp: &Cap<ProcessGroup>, sig: Signum) -> usize {
    step_kill_pgrp_with_post(pgrp, sig, direct_task_mailbox_post)
}

pub fn step_kill_pgrp_with_posts<P, R>(
    pgrp: &Cap<ProcessGroup>,
    sig: Signum,
    mut post: P,
    mut source_post: R,
) -> usize
where
    P: FnMut(ArcWeak<TaskMailbox>, MailboxEvent),
    R: FnMut(&TaskMailbox, MailboxEvent) -> bool,
{
    step_kill_pgrp_with_posts_dyn(pgrp, sig, &mut post, &mut source_post)
}

fn step_kill_pgrp_with_posts_dyn(
    pgrp: &Cap<ProcessGroup>,
    sig: Signum,
    post: &mut dyn FnMut(ArcWeak<TaskMailbox>, MailboxEvent),
    source_post: &mut dyn FnMut(&TaskMailbox, MailboxEvent) -> bool,
) -> usize {
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
        if step_kill_process_with_posts(member, sig, None, &mut *post, &mut *source_post)
            == KillOutcome::Delivered
        {
            // Catchable only: mirror onto group_pending so the
            // delivery step can recognise group-targeted posts.
            // Gewalt bypasses pending queues entirely.
            if catchable {
                post_group_pending_signal(member, sig);
            }
            delivered += 1;
        }
    }
    delivered
}

/// Install (or replace) the disposition of `sig` on `process`.
/// `Default` for `SIGKILL`/`SIGSTOP` is rejected silently.
/// Returns the previous disposition.
pub fn step_sigaction_entry(
    process: &Cap<ProcessIdentity>,
    sig: Signum,
    entry: SigActionEntry,
) -> SigDispositionChange {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    let Ok(payload) = process.upgrade_operational() else {
        return SigDispositionChange::ZombieIgnored;
    };
    let prev = payload.sig_actions().get_entry(sig);
    if sig.is_uncatchable() {
        return SigDispositionChange::Uncatchable(prev);
    }
    payload.sig_actions().set_entry(sig, entry);
    SigDispositionChange::Replaced { prev }
}

/// Install (or replace) only the disposition of `sig` on `process`.
/// Compatibility wrapper for older call sites that do not yet carry
/// flags or a handler mask.
pub fn step_sigaction(
    process: &Cap<ProcessIdentity>,
    sig: Signum,
    disposition: SigDisposition,
) -> SigDispositionChange {
    // observe: inspect current subsystem state and validate inputs.
    // upgrade: acquire capabilities/guards needed for mutation.
    // reserve: reserve namespace, memory, or wait-source effects.
    // commit: apply the state transition.
    // publish: emit readiness, signal, or observable outcome.
    step_sigaction_entry(process, sig, SigActionEntry::from(disposition))
}

/// Outcome of `step_sigaction`. `Replaced` is the normal path;
/// `ZombieIgnored` means the target had no payload; `Uncatchable`
/// means SIGKILL/SIGSTOP were silently kept at default.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SigDispositionChange {
    Replaced { prev: SigActionEntry },
    Uncatchable(SigActionEntry),
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
    Retry,
    /// Posted permission was OK (e.g. signal 0 probe), no actual
    /// delivery happened.
    Probed,
}

/// POSIX-shaped `kill(target_pid, sig)` modulo pid lookup. Composes
/// the `cred::require_signal_send` permission check with
/// [`step_kill_process_with_posts`] per `SIGNAL_v1` §32's `script_kill`. The
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
    script_kill_process_with_posts(
        source,
        target,
        sig,
        info,
        direct_task_mailbox_post,
        |mailbox, event| mailbox.post(event),
    )
}

pub fn script_kill_process_with_posts<P, R>(
    source: &Cap<ProcessIdentity>,
    target: &Cap<ProcessIdentity>,
    sig: Signum,
    info: Option<SigInfo>,
    mut post: P,
    source_post: R,
) -> Result<KillScriptOutcome, Errno>
where
    P: FnMut(ArcWeak<TaskMailbox>, MailboxEvent),
    R: FnMut(&TaskMailbox, MailboxEvent) -> bool,
{
    use crate::cred::checks::{authorize_signal_send, AuthOutcome};

    match authorize_signal_send(source, target, sig)? {
        AuthOutcome::NoLiveTarget => return Ok(KillScriptOutcome::NoLiveThread),
        AuthOutcome::Authorized => {}
    }
    // Commit. `step_kill_process_with_posts` is the primitive (no cred check);
    // authorize_signal_send dropped its guard before returning, so
    // The catchable-signal post helper's inner guard for SigInfo storage doesn't nest.
    Ok(
        match step_kill_process_with_posts(target, sig, info, &mut post, source_post) {
            KillOutcome::Delivered => KillScriptOutcome::Delivered,
            KillOutcome::NoLiveThread => KillScriptOutcome::NoLiveThread,
            KillOutcome::Retry => KillScriptOutcome::Retry,
        },
    )
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

/// Permission-only counterpart to [`script_kill_pgrp`]. Returns the
/// number of live group members the source may signal without posting
/// anything to pending queues or mailboxes.
pub fn script_kill_pgrp_probe(
    source: &Cap<ProcessIdentity>,
    pgrp: &Cap<ProcessGroup>,
) -> Result<u32, Errno> {
    use crate::cred::checks::{authorize_signal_send_under_guard, AuthOutcome};

    let guard = step_engine::guard();
    let source_snapshot = source.cred_snapshot().ok_or(Errno::ESRCH)?;
    let members: alloc::vec::Vec<Cap<ProcessIdentity>> = pgrp.members.snapshot_live(&guard);
    let mut permitted = 0u32;

    for member in &members {
        if matches!(
            authorize_signal_send_under_guard(
                &source_snapshot,
                source,
                member,
                Signum::SIGTERM,
                &guard,
            ),
            Ok(AuthOutcome::Authorized)
        ) {
            permitted += 1;
        }
    }

    Ok(permitted)
}

/// Permission-checked process-group fanout per `SIGNAL_v1` §12.2.
/// Iterates `pgrp.members`, builds a `TargetProcCred` per live member,
/// runs the cred check, and posts on permitted members via
/// [`step_kill_process_with_post`]. Returns the count of members the call
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
    script_kill_pgrp_with_posts(
        source,
        pgrp,
        sig,
        direct_task_mailbox_post,
        |mailbox, event| mailbox.post(event),
    )
}

pub fn script_kill_pgrp_with_posts<P, R>(
    source: &Cap<ProcessIdentity>,
    pgrp: &Cap<ProcessGroup>,
    sig: Signum,
    mut post: P,
    mut source_post: R,
) -> Result<u32, Errno>
where
    P: FnMut(ArcWeak<TaskMailbox>, MailboxEvent),
    R: FnMut(&TaskMailbox, MailboxEvent) -> bool,
{
    let guard = step_engine::guard();
    script_kill_pgrp_with_guard_and_posts_dyn(
        source,
        pgrp,
        sig,
        &guard,
        &mut post,
        &mut source_post,
    )
}

pub(crate) fn script_kill_pgrp_with_guard(
    source: &Cap<ProcessIdentity>,
    pgrp: &Cap<ProcessGroup>,
    sig: Signum,
    guard: &Guard<'_>,
) -> Result<u32, Errno> {
    script_kill_pgrp_with_guard_and_posts(
        source,
        pgrp,
        sig,
        guard,
        direct_task_mailbox_post,
        |mailbox, event| mailbox.post(event),
    )
}

pub(crate) fn script_kill_pgrp_with_guard_and_posts<P, R>(
    source: &Cap<ProcessIdentity>,
    pgrp: &Cap<ProcessGroup>,
    sig: Signum,
    guard: &Guard<'_>,
    mut post: P,
    mut source_post: R,
) -> Result<u32, Errno>
where
    P: FnMut(ArcWeak<TaskMailbox>, MailboxEvent),
    R: FnMut(&TaskMailbox, MailboxEvent) -> bool,
{
    script_kill_pgrp_with_guard_and_posts_dyn(source, pgrp, sig, guard, &mut post, &mut source_post)
}

fn script_kill_pgrp_with_guard_and_posts_dyn(
    source: &Cap<ProcessIdentity>,
    pgrp: &Cap<ProcessGroup>,
    sig: Signum,
    guard: &Guard<'_>,
    post: &mut dyn FnMut(ArcWeak<TaskMailbox>, MailboxEvent),
    source_post: &mut dyn FnMut(&TaskMailbox, MailboxEvent) -> bool,
) -> Result<u32, Errno> {
    use crate::cred::checks::{authorize_signal_send_under_guard, AuthOutcome};

    let source_snapshot = source.cred_snapshot().ok_or(Errno::ESRCH)?;

    let mut delivered = 0u32;
    let mut retry = false;
    let members: alloc::vec::Vec<Cap<ProcessIdentity>> = pgrp.members.snapshot_live(guard);

    for member in &members {
        // Per-member cred check using the snapshot captured once
        // outside the loop. Denials / no-live-target both fold into
        // "skip" — SIGNAL_v1 §12.2 members-independent rule.
        match authorize_signal_send_under_guard(&source_snapshot, source, member, sig, guard) {
            Ok(AuthOutcome::Authorized) => {}
            Ok(AuthOutcome::NoLiveTarget) | Err(_) => continue,
        }
        match step_kill_process_with_posts(member, sig, None, &mut *post, &mut *source_post) {
            KillOutcome::Delivered => {
                // Catchable only: mirror onto group_pending. Gewalt
                // signals (SIGKILL/SIGSTOP/SIGCONT) bypass pending
                // queues entirely per SIGNAL_v1 §2 Consequence 2.
                if !is_gewalt(sig) {
                    post_group_pending_signal(member, sig);
                }
                delivered += 1;
            }
            KillOutcome::Retry => retry = true,
            KillOutcome::NoLiveThread => {}
        }
    }

    if delivered == 0 && retry {
        Err(Errno::EAGAIN)
    } else {
        Ok(delivered)
    }
}

/// Resolve a signal target to its owning process and run send authorization.
fn authorize_signal_target(
    source: &Cap<ProcessIdentity>,
    target: &SignalTarget,
    sig: Signum,
) -> Result<Option<Cap<ProcessIdentity>>, Errno> {
    let target_proc = match target {
        SignalTarget::Process(cap) => cap.clone(),
        SignalTarget::Thread(thread) => match thread.upgrade_owner_proc() {
            Some(process) => process,
            None => return Ok(None),
        },
        SignalTarget::ProcessGroup(_) => return Ok(None),
    };

    use crate::cred::checks::{authorize_signal_send, AuthOutcome};
    match authorize_signal_send(source, &target_proc, sig)? {
        AuthOutcome::NoLiveTarget => Ok(None),
        AuthOutcome::Authorized => Ok(Some(target_proc)),
    }
}

/// Authorize a thread-targeted signal using the normal POSIX send rule, then
/// terminate the owning process with that signal. This is the LA64 SIGCANCEL
/// fail-fast script; keeping it architecture-neutral makes its auth and
/// lifecycle outcomes host-testable.
pub fn script_authorized_thread_exit_with_posts<P, R>(
    source: &Cap<ProcessIdentity>,
    thread: &Cap<crate::thread_runtime::ThreadIdentity>,
    sig: Signum,
    post: P,
    source_post: R,
) -> Result<KillOutcome, Errno>
where
    P: FnMut(ArcWeak<TaskMailbox>, MailboxEvent),
    R: FnMut(&TaskMailbox, MailboxEvent) -> bool,
{
    let target = SignalTarget::Thread(thread.clone());
    let Some(target_proc) = authorize_signal_target(source, &target, sig)? else {
        return Ok(KillOutcome::NoLiveThread);
    };

    Ok(kill_outcome_from_exit(
        crate::process::execution::step_exit_group_with_signal_with_posts(
            &target_proc,
            sig,
            post,
            source_post,
        ),
    ))
}

/// Cred-checked counterpart to disposition-aware POSIX signal delivery.
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
/// with `Ok(NoLiveThread)` - pgrp fanout flows through
/// [`script_kill_pgrp`] instead.
///
/// Returns:
/// - `Ok(Delivered)` / `Ok(NoLiveThread)` - outcome of the delivery
///   primitive after a successful cred check.
/// - `Err(Errno::ESRCH)` - `source` is a zombie.
/// - `Err(Errno::EPERM)` - cred check denied the post.
pub fn script_deliver_signal_with_post<F>(
    source: &Cap<ProcessIdentity>,
    target: SignalTarget,
    sig: Signum,
    info: Option<SigInfo>,
    mut post: F,
) -> Result<KillOutcome, Errno>
where
    F: FnMut(ArcWeak<TaskMailbox>, MailboxEvent),
{
    script_deliver_signal_with_posts(source, target, sig, info, &mut post, |mailbox, event| {
        mailbox.post(event)
    })
}

pub fn script_deliver_signal(
    source: &Cap<ProcessIdentity>,
    target: SignalTarget,
    sig: Signum,
    info: Option<SigInfo>,
) -> Result<KillOutcome, Errno> {
    script_deliver_signal_with_post(source, target, sig, info, direct_task_mailbox_post)
}

pub fn script_deliver_signal_with_posts<P, R>(
    source: &Cap<ProcessIdentity>,
    target: SignalTarget,
    sig: Signum,
    info: Option<SigInfo>,
    mut post: P,
    mut source_post: R,
) -> Result<KillOutcome, Errno>
where
    P: FnMut(ArcWeak<TaskMailbox>, MailboxEvent),
    R: FnMut(&TaskMailbox, MailboxEvent) -> bool,
{
    let Some(target_proc) = authorize_signal_target(source, &target, sig)? else {
        return Ok(KillOutcome::NoLiveThread);
    };

    // Phase 2 — commit. Process-targeted sends keep the
    // disposition-aware process selection path; thread-targeted
    // sends post to the requested TID. musl's pthread_cancel uses
    // pthread_kill -> SYS_tkill, so collapsing thread sends back to
    // process-directed routing can strand SIGCANCEL on a different
    // unblocked sibling.
    Ok(match target {
        SignalTarget::Process(_) => {
            step_kill_process_with_posts(&target_proc, sig, info, post, source_post)
        }
        SignalTarget::Thread(thread) => {
            if is_gewalt(sig) {
                return Ok(step_kill_process_with_posts(
                    &target_proc,
                    sig,
                    info,
                    post,
                    source_post,
                ));
            }
            let disposition = match target_proc.upgrade_operational() {
                Ok(payload) => payload.sig_actions().get(sig),
                Err(_) => return Ok(KillOutcome::NoLiveThread),
            };
            match disposition {
                SigDisposition::Ignore => return Ok(KillOutcome::Delivered),
                SigDisposition::Default => match default_action(sig) {
                    DefaultAction::Ignore => return Ok(KillOutcome::Delivered),
                    DefaultAction::Term | DefaultAction::Core => {
                        return Ok(kill_outcome_from_exit(
                            crate::process::execution::step_exit_group_with_signal_with_posts(
                                &target_proc,
                                sig,
                                &mut post,
                                &mut source_post,
                            ),
                        ));
                    }
                    DefaultAction::Stop => {
                        route_gewalt_with_post(&target_proc, Signum::SIGSTOP, &mut post);
                        return Ok(KillOutcome::Delivered);
                    }
                    DefaultAction::Cont => {
                        route_gewalt_with_post(&target_proc, Signum::SIGCONT, &mut post);
                        return Ok(KillOutcome::Delivered);
                    }
                },
                SigDisposition::Handler(_) => {}
            }
            if let Some(info) = info {
                target_proc.siginfo_store(sig, info);
            }
            post_signal_with_post(
                &thread,
                sig,
                SignalRouting::ThreadDirected {
                    tid: thread.tid.0 as u64,
                },
                info,
                post,
            );
            KillOutcome::Delivered
        }
        SignalTarget::ProcessGroup(_) => KillOutcome::NoLiveThread,
    })
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

/// `StepOp` wrap for process-directed signal delivery with caller-injected
/// mailbox publication.
pub struct KillProcessWithPostOp<F> {
    pub target: Cap<ProcessIdentity>,
    pub sig: Signum,
    pub info: Option<SigInfo>,
    pub post: F,
}

/// Direct-publication StepOp retained for final-smp syscall shims.
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

impl<I, F> StepOp<I> for KillProcessWithPostOp<F>
where
    I: SubjectIdentity,
    F: FnMut(ArcWeak<TaskMailbox>, MailboxEvent),
{
    type Output = KillOutcome;
    type Progress = NoProgress;
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        StepOutcome::Done(step_kill_process_with_post(
            &self.target,
            self.sig,
            self.info,
            &mut self.post,
        ))
    }
}

impl<I, F> OneShotStepOp<I> for KillProcessWithPostOp<F>
where
    I: SubjectIdentity,
    F: FnMut(ArcWeak<TaskMailbox>, MailboxEvent),
{
}

/// `StepOp` wrap for process-group-directed signal delivery with caller-
/// injected mailbox publication.
pub struct KillPgrpWithPostOp<F> {
    pub pgrp: Cap<ProcessGroup>,
    pub sig: Signum,
    pub post: F,
}

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

impl<I: SubjectIdentity> OneShotStepOp<I> for KillPgrpOp {}

impl<I, F> StepOp<I> for KillPgrpWithPostOp<F>
where
    I: SubjectIdentity,
    F: FnMut(ArcWeak<TaskMailbox>, MailboxEvent),
{
    type Output = usize;
    type Progress = NoProgress;
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        StepOutcome::Done(step_kill_pgrp_with_post(
            &self.pgrp,
            self.sig,
            &mut self.post,
        ))
    }
}

impl<I, F> OneShotStepOp<I> for KillPgrpWithPostOp<F>
where
    I: SubjectIdentity,
    F: FnMut(ArcWeak<TaskMailbox>, MailboxEvent),
{
}

/// `StepOp` wrap for disposition-aware POSIX signal delivery.
pub struct DeliverSignalWithPostOp<F> {
    pub target: SignalTarget,
    pub sig: Signum,
    pub post: F,
}

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

impl<I, F> StepOp<I> for DeliverSignalWithPostOp<F>
where
    I: SubjectIdentity,
    F: FnMut(ArcWeak<TaskMailbox>, MailboxEvent),
{
    type Output = KillOutcome;
    type Progress = NoProgress;
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        StepOutcome::Done(deliver_posix_signal_with_post(
            self.target.clone(),
            self.sig,
            &mut self.post,
        ))
    }
}

impl<I, F> OneShotStepOp<I> for DeliverSignalWithPostOp<F>
where
    I: SubjectIdentity,
    F: FnMut(ArcWeak<TaskMailbox>, MailboxEvent),
{
}

/// `StepOp` wrap for [`step_sigaction`]. PR-2 wave 2.
pub struct SigactionOp {
    pub process: Cap<ProcessIdentity>,
    pub sig: Signum,
    pub entry: SigActionEntry,
}

impl<I: SubjectIdentity> StepOp<I> for SigactionOp {
    type Output = SigDispositionChange;
    type Progress = NoProgress;
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        StepOutcome::Done(step_sigaction_entry(&self.process, self.sig, self.entry))
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
    use alloc::sync::Arc;

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
    fn kill_process_with_post_op_uses_injected_post() {
        let _g = setup();
        let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
        let leader = proc_cap.nth_thread(0).expect("leader thread");
        let mailbox = Arc::new(TaskMailbox::new());
        let leader_payload = leader.payload_cap().expect("live leader");
        leader_payload.bind_mailbox(Arc::downgrade(&mailbox));
        let mut posted = 0usize;
        let mut op = KillProcessWithPostOp {
            target: proc_cap.clone(),
            sig: Signum::SIGTERM,
            info: None,
            post: |weak: ArcWeak<TaskMailbox>, event: MailboxEvent| {
                if !matches!(event, MailboxEvent::SignalDelivered { .. }) {
                    return;
                }
                if let Some(mailbox) = weak.upgrade() {
                    let _ = mailbox.post(event);
                    posted += 1;
                }
            },
        };
        let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
        let outcome = op.step(&mut ctx);
        assert_eq!(outcome, StepOutcome::Done(KillOutcome::Delivered));
        assert_eq!(posted, 1);
        assert!(matches!(
            mailbox.poll(),
            Some(MailboxEvent::SignalDelivered { .. })
        ));
    }

    #[test]
    fn kill_pgrp_with_post_op_uses_injected_post() {
        let _g = setup();
        let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
        let leader = proc_cap.nth_thread(0).expect("leader thread");
        let mailbox = Arc::new(TaskMailbox::new());
        let leader_payload = leader.payload_cap().expect("live leader");
        leader_payload.bind_mailbox(Arc::downgrade(&mailbox));
        let pgrp = proc_cap.pgrp_cap();
        let mut posted = 0usize;
        let mut op = KillPgrpWithPostOp {
            pgrp,
            sig: Signum::SIGINT,
            post: |weak: ArcWeak<TaskMailbox>, event: MailboxEvent| {
                if !matches!(event, MailboxEvent::SignalDelivered { .. }) {
                    return;
                }
                if let Some(mailbox) = weak.upgrade() {
                    let _ = mailbox.post(event);
                    posted += 1;
                }
            },
        };
        let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
        let outcome = op.step(&mut ctx);
        // bootstrap_init_process gives a single-member pgrp.
        assert_eq!(outcome, StepOutcome::Done(1usize));
        assert_eq!(posted, 1);
        assert!(matches!(
            mailbox.poll(),
            Some(MailboxEvent::SignalDelivered { .. })
        ));
    }

    #[test]
    fn sigaction_op_delegates_to_step_sigaction() {
        let _g = setup();
        let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
        let mut op = SigactionOp {
            process: proc_cap.clone(),
            sig: Signum::SIGTERM,
            entry: SigActionEntry::from(SigDisposition::Ignore),
        };
        let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
        let outcome = op.step(&mut ctx);
        match outcome {
            StepOutcome::Done(SigDispositionChange::Replaced {
                prev:
                    SigActionEntry {
                        disposition: SigDisposition::Default,
                        ..
                    },
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
            entry: SigActionEntry::from(SigDisposition::Ignore),
        };
        let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
        let outcome = op.step(&mut ctx);
        assert_eq!(
            outcome,
            StepOutcome::Done(SigDispositionChange::Uncatchable(SigActionEntry::DEFAULT))
        );
    }
}

#[cfg(test)]
mod tests;
