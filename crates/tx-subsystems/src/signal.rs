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

use tx_substrate::zone::Cap;

use crate::execution::Errno;
use crate::process::structure::{ProcessGroup, ProcessIdentity};
use crate::sync::SpinMutex;
use crate::thread_runtime::execution::post_signal;

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
}

// ----- POSIX-shim entry points -----

/// Result of a kill-style shim. `Delivered` if at least one thread
/// received the post; `NoLiveThread` if the target is a zombie or its
/// thread list is empty.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KillOutcome {
    Delivered,
    NoLiveThread,
}

/// Deliver `sig` to a single process by routing it to a live leader
/// thread. Zombies are skipped. Day-1 routes to *the first live
/// thread*; later passes select per process-shared/per-thread signal
/// semantics.
pub fn step_kill_process(target: &Cap<ProcessIdentity>, sig: Signum) -> KillOutcome {
    let payload_guard = target.payload.lock();
    let Some(payload) = payload_guard.as_ref() else {
        return KillOutcome::NoLiveThread;
    };
    let threads = payload.threads.lock();
    let Some(leader) = threads.iter().find(|t| !t.is_zombie()).cloned() else {
        return KillOutcome::NoLiveThread;
    };
    drop(threads);
    drop(payload_guard);

    post_signal(&leader, sig);
    KillOutcome::Delivered
}

/// Deliver `sig` to every live process in `pgrp`. Each delivered
/// member also gets the bit set on its `group_pending` queue so the
/// (future) delivery step can distinguish thread-targeted from
/// group-targeted signals when consulting per-process action tables.
/// Returns the count of processes that received the post.
pub fn step_kill_pgrp(pgrp: &Cap<ProcessGroup>, sig: Signum) -> usize {
    let guard = tx_substrate::epoch::guard();
    let mut delivered = 0usize;
    for weak in pgrp.members.lock().iter() {
        let Some(member) = weak.upgrade(&guard) else {
            continue;
        };
        if step_kill_process(&member, sig) == KillOutcome::Delivered {
            // Mirror onto the per-process group_pending queue so
            // delivery code can recognise group-targeted posts.
            if let Some(payload) = member.payload.lock().as_ref() {
                payload.group_pending().post(sig);
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
    let payload_guard = process.payload.lock();
    let Some(payload) = payload_guard.as_ref() else {
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

    let source_cred = {
        let payload_guard = source.payload.lock();
        let payload = payload_guard.as_ref().ok_or(Errno::ESRCH)?;
        payload.cred()
    };

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

    let source_cred = {
        let payload_guard = source.payload.lock();
        let payload = payload_guard.as_ref().ok_or(Errno::ESRCH)?;
        payload.cred()
    };

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
    let source_cred = {
        let payload_guard = source.payload.lock();
        let payload = payload_guard.as_ref().ok_or(Errno::ESRCH)?;
        payload.cred()
    };

    let guard = tx_substrate::epoch::guard();
    let mut delivered = 0u32;
    let members: alloc::vec::Vec<Cap<ProcessIdentity>> = pgrp
        .members
        .lock()
        .iter()
        .filter_map(|w| w.upgrade(&guard))
        .collect();

    for member in &members {
        let Some(facts) = member.target_proc_cred_for(source) else {
            continue;
        };
        if crate::cred::require_signal_send(source_cred, &facts, sig, &guard).is_err() {
            continue;
        }
        if step_kill_process(member, sig) == KillOutcome::Delivered {
            // Mirror onto the per-process group_pending so future
            // delivery code can recognise group-targeted posts.
            if let Some(payload) = member.payload.lock().as_ref() {
                payload.group_pending().post(sig);
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
    let Some(weak) = dispatch.target.pgrp_weak() else {
        return Ok(DispatchOutcome::NoTypedPgrp);
    };

    let guard = tx_substrate::epoch::guard();
    let Some(pgrp_cap) = weak.upgrade(&guard) else {
        return Ok(DispatchOutcome::PgrpDropped);
    };
    drop(guard);

    let signum = signum_for_job_control(dispatch.signal);
    let count = script_kill_pgrp(source, &pgrp_cap, signum)?;
    Ok(DispatchOutcome::Delivered { count })
}

#[cfg(test)]
mod tests;
