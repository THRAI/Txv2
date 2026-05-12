//! `signalfd(2)` — consume signals via a file descriptor (D9-D).
//!
//! Spec: `docs/progress/decisions/2026-05-11-d9-signal-wake-migration.md`
//! §6 — "Option C add-on path." Linux's `signalfd(2)` lets a process
//! consume a configurable subset of signals via a read-side file
//! descriptor instead of (or in addition to) signal handlers. The fd
//! is readable when a signal in its `mask` has been delivered to the
//! process; `read(2)` drains one or more 128-byte
//! `struct signalfd_siginfo` records.
//!
//! # Phase-D9-D surface
//!
//! 1. The zone-allocated [`SignalFd`] payload.
//! 2. A stable `sfd_id` minted at construction (mirrors W-Q's `ufd_id`
//!    and W-Z's `context_id` discipline).
//! 3. A per-process subscription registry keyed by the process's
//!    `Cap<ProcessIdentity>` slot key. `register_subscription` /
//!    `unregister_subscription` are driven by the syscall arm at
//!    open / close (`Drop for SignalFd`) time;
//!    [`notify_process_signal`] is driven by
//!    [`crate::signal::step_kill_process`] *after* the existing
//!    thread-eligibility post completes.
//! 4. A per-fd pending-signum queue with a paired
//!    [`Arc<WaitSource>`] for read-readiness. The queue is a
//!    `VecDeque<u8>` (raw signum bytes) — phase D9-D treats signals
//!    as the bitset their `Signum` shape allows; realtime
//!    per-occurrence queuing is a follow-up. **Coalescence:** a
//!    pending bit covers any number of matching deliveries.
//! 5. The [`step_signalfd_read`] step body — mirrors the userfaultfd
//!    `step_ufd_read` shape (one-siginfo-per-read, EAGAIN on
//!    empty + nonblock, `Yield { OnWaitSource }` on empty + blocking).
//!
//! # signalfd_siginfo wire layout
//!
//! `struct signalfd_siginfo` is 128 bytes on Linux. Phase D9-D
//! emits a minimal zero-filled record with the `ssi_signo` field
//! populated; future passes extend with `ssi_pid` / `ssi_uid` /
//! `ssi_code` once siginfo plumbing lands.
//!
//! ```text
//!   off  size  field
//!   0    4     ssi_signo  (u32) — the signum delivered
//!   4    4     ssi_errno  (i32, zero)
//!   8    4     ssi_code   (i32, zero)
//!   12   4     ssi_pid    (u32, zero)
//!   16   4     ssi_uid    (u32, zero)
//!   ... 108 bytes reserved / zero
//! ```
//!
//! # Routing decision
//!
//! Per D9 §6 the recommended Option A path extends to Option C with
//! a per-process `signal_event_source` and a
//! `SigDisposition::ConsumedBy(SignalSubscriptionId)` variant. Phase
//! D9-D keeps the disposition table untouched: the subscription is
//! invoked **additively** after the existing thread-eligibility
//! post. This preserves Linux's "signal handler runs *and* signalfd
//! receives" semantic by default. Once `signalfd_create4` lands an
//! explicit "mask out of handler delivery" variant the disposition
//! plumbing will follow; D9-D pins the wake/dispatch shape.

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

pub mod adapter;

use adapter::step_engine::{
    guard, sign, ByteProgress, Cap, InterestMask, SpinMutex, StepOutcome, V3Errno,
    WaitSource, WaitSourceId, Weak, Zone, ZoneAllocated, ZoneError,
};
use adapter::wait_routing::{Channel, Mask};

use crate::process::structure::ProcessIdentity;
use crate::signal::Signum;
use crate::wait_source;

/// Interest-mask bit fired on the SignalFd's per-fd wait source when a
/// new signum lands in the pending queue. Mirrors pipe.rs's
/// `PIPE_READABLE` / userfaultfd's `UFD_READABLE`.
pub const SIGNALFD_READABLE: u64 = 0x1;

/// Wire size of one `struct signalfd_siginfo` record, per Linux's
/// generic uapi (`<sys/signalfd.h>`). 128 bytes — phase D9-D emits a
/// zero-filled record with only `ssi_signo` populated.
pub const SIGNALFD_SIGINFO_SIZE: usize = 128;

// === sfd_id minting ===================================================

/// Monotonic counter for [`SignalFd::sfd_id`]. Mirrors W-Q's
/// `NEXT_UFD_ID` / W-Z's `NEXT_CONTEXT_ID` discipline.
static NEXT_SFD_ID: AtomicU64 = AtomicU64::new(1);

fn allocate_sfd_id() -> u64 {
    NEXT_SFD_ID.fetch_add(1, Ordering::AcqRel)
}

// === payload ==========================================================

/// `signalfd(2)` payload — zone-allocated per `Cap<SignalFd>`.
///
/// Each cap is owned by exactly one [`crate::vfs::structure::OpenFile`]
/// shape (`OpenFileBacking::SignalFd { sfd }`) and registers itself
/// against the owning process's subscription list at construction time
/// (via [`Self::register_with_process`]). The
/// [`Drop`] impl removes the registration so a closed signalfd no
/// longer receives posts.
pub struct SignalFd {
    /// Monotonic per-fd id. Stable for the lifetime of the
    /// `Cap<SignalFd>` — minted once at construction and never
    /// reassigned. Used by test pins for identity assertions.
    sfd_id: u64,
    /// Owning process's slot-key raw value. The signalfd is bound to
    /// the process at construction; the bind-key is what the
    /// per-process registry indexes on. `step_kill_process` resolves
    /// "which signalfds does this process own?" via the registry.
    owner_proc_key: u32,
    /// Bitmask of signums the subscription cares about — bit `i` set
    /// means the subscription accepts `Signum(i+1)`. Updated via
    /// `signalfd(fd, &mask, flags)` (the "modify existing fd" call
    /// shape). Phase D9-D stores the mask; the update arm lands with
    /// the syscall.
    ///
    /// The mask uses the same `Signum::bit` encoding as
    /// [`crate::signal::SignalMask`] — `1u64 << (signum - 1)`.
    mask: AtomicU64,
    /// Per-fd pending-signum queue. `push_back(signum.raw())` on each
    /// matching post; `pop_front()` on each `read(2)`. FIFO matches
    /// Linux's signalfd queue ordering.
    ///
    /// Coalescence: phase D9-D queues every matching post — multiple
    /// `kill(pid, SIGUSR1)` calls each push one entry. A future
    /// pass may collapse by signum if siginfo carries no
    /// per-occurrence payload; the current shape matches Linux's
    /// rt-signal queueing without needing extra state.
    pending: SpinMutex<VecDeque<u8>>,
    /// Per-fd `WaitSource`. Fired whenever a matching signal is
    /// delivered to the owning process. Pattern mirrors pipe /
    /// userfaultfd: an `Arc<WaitSource>` whose id is paired with a
    /// legacy `Channel` for D2/D4 coexistence.
    wait_source: Arc<WaitSource>,
    /// Legacy `Channel` companion to [`Self::wait_source`].
    wait_channel: Channel,
    /// Carrier id paired with [`Self::wait_channel`] and
    /// [`Self::wait_source`]. Stable for the lifetime of the
    /// `Cap<SignalFd>`.
    wait_source_id: u64,
}

impl core::fmt::Debug for SignalFd {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SignalFd")
            .field("sfd_id", &self.sfd_id)
            .field("owner_proc_key", &self.owner_proc_key)
            .field("mask", &self.mask.load(Ordering::Acquire))
            .field("pending_count", &self.pending_count())
            .finish()
    }
}

impl SignalFd {
    /// Construct a fresh signalfd payload bound to `owner_proc_key`
    /// with the given `mask`. Tests and the `sys_signalfd4(2)` arm
    /// should prefer [`Self::new_cap_for_process`] which zone-signs
    /// and registers in one step.
    pub fn new(owner_proc_key: u32, mask: u64) -> Self {
        let wait_channel = Channel::new();
        let wait_source_id = wait_source::register_wait_channel(wait_channel.clone());
        let wait_source = Arc::new(WaitSource::new(WaitSourceId::new(wait_source_id)));
        Self {
            sfd_id: allocate_sfd_id(),
            owner_proc_key,
            mask: AtomicU64::new(mask),
            pending: SpinMutex::new(VecDeque::new()),
            wait_source,
            wait_channel,
            wait_source_id,
        }
    }

    /// Zone-sign a fresh signalfd cap for `owner_proc` with the given
    /// `mask`, and register it with the per-process subscription
    /// list. Returns the cap the caller installs into an
    /// `OpenFile { backing: OpenFileBacking::SignalFd { sfd } }`.
    pub fn new_cap_for_process(
        owner_proc: &Cap<ProcessIdentity>,
        mask: u64,
    ) -> Result<Cap<Self>, ZoneError> {
        let payload = Self::new(owner_proc.key().raw(), mask);
        let cap = sign(payload)?;
        register_subscription(owner_proc.key().raw(), cap.downgrade());
        Ok(cap)
    }

    /// Snapshot the stable per-fd id.
    pub const fn sfd_id(&self) -> u64 {
        self.sfd_id
    }

    /// Snapshot the owning process's slot-key raw value.
    pub const fn owner_proc_key(&self) -> u32 {
        self.owner_proc_key
    }

    /// Snapshot the current mask.
    pub fn mask(&self) -> u64 {
        self.mask.load(Ordering::Acquire)
    }

    /// Replace the mask (used by `signalfd(existing_fd, &mask, flags)`).
    /// Returns the previous mask.
    pub fn set_mask(&self, new_mask: u64) -> u64 {
        self.mask.swap(new_mask, Ordering::AcqRel)
    }

    /// `true` iff `signum` is covered by the current mask.
    pub fn covers(&self, signum: Signum) -> bool {
        (self.mask() & signum.bit()) != 0
    }

    /// Current pending-queue depth.
    pub fn pending_count(&self) -> usize {
        self.pending.lock().len()
    }

    /// Borrow the per-fd wait source. The agent's `read(2)` arm parks
    /// on this carrier when the queue is empty (blocking mode).
    pub fn wait_source(&self) -> &Arc<WaitSource> {
        &self.wait_source
    }

    /// Carrier id paired with [`Self::wait_source`].
    pub fn wait_source_id(&self) -> u64 {
        self.wait_source_id
    }

    /// Notify this subscription of a delivered signal. Filters against
    /// the current mask: posts only if `signum` is covered. Fires
    /// both wake paths (D2/D4 coexistence) on success.
    pub fn notify(&self, signum: Signum) -> bool {
        if !self.covers(signum) {
            return false;
        }
        self.pending.lock().push_back(signum.raw());
        // Fire both wake paths (D2/D4 coexistence, mirroring pipe.rs /
        // userfaultfd.rs):
        self.wait_channel.fire(Mask::from_bits(SIGNALFD_READABLE));
        self.wait_source
            .notify(InterestMask::new(SIGNALFD_READABLE));
        true
    }

    /// Pop one signum off the pending queue. Returns `None` if empty.
    pub fn pop_pending(&self) -> Option<Signum> {
        let raw = self.pending.lock().pop_front()?;
        Signum::new(raw)
    }
}

impl Drop for SignalFd {
    fn drop(&mut self) {
        // Release the registry slot so a dropped signalfd no longer
        // receives posts from `step_kill_process`. Also release the
        // legacy carrier id so the wait_source registry does not leak.
        unregister_subscription(self.owner_proc_key, self.sfd_id);
        wait_source::release_wait_channel(self.wait_source_id);
    }
}

// === per-process subscription registry ================================
//
// Keyed by `Cap<ProcessIdentity>::key().raw()`. A signalfd registers
// itself at construction (`new_cap_for_process`) and unregisters in
// `Drop`. `step_kill_process` calls `notify_process_signal` to walk
// the list and fan out to every matching subscription *after* the
// existing thread-eligibility post (the additive shape per D9 §6).

type SubscriptionList = Vec<Weak<SignalFd>>;

static SUBSCRIPTIONS: SpinMutex<alloc::collections::BTreeMap<u32, SubscriptionList>> =
    SpinMutex::new(alloc::collections::BTreeMap::new());

fn register_subscription(proc_key: u32, sfd_weak: Weak<SignalFd>) {
    let mut map = SUBSCRIPTIONS.lock();
    map.entry(proc_key).or_default().push(sfd_weak);
}

fn unregister_subscription(proc_key: u32, sfd_id: u64) {
    let mut map = SUBSCRIPTIONS.lock();
    if let Some(list) = map.get_mut(&proc_key) {
        // Retain entries that either fail to upgrade (already gone) or
        // upgrade to a different `sfd_id`. The matching entry drops out
        // of the list.
        let guard = guard();
        list.retain(|w| match w.upgrade(&guard) {
            Some(cap) => cap.sfd_id() != sfd_id,
            None => false,
        });
        drop(guard);
        if list.is_empty() {
            map.remove(&proc_key);
        }
    }
}

/// Notify every signalfd subscription registered against `proc_key`
/// of a delivered signal. Each subscription checks its mask; the
/// match-and-post is per-subscription. Returns the number of
/// subscriptions that accepted the signal.
///
/// Called by [`crate::signal::step_kill_process`] *after* the
/// thread-eligibility post completes — the wake paths are additive
/// (per D9 §6 / W-II prompt constraint 1).
pub fn notify_process_signal(proc_key: u32, signum: Signum) -> usize {
    // Snapshot the subscription list under the lock so we don't hold
    // the registry spinlock across the per-subscription `notify`
    // calls (which take their own per-fd locks).
    let snapshot: Vec<Weak<SignalFd>> = {
        let map = SUBSCRIPTIONS.lock();
        map.get(&proc_key).cloned().unwrap_or_default()
    };
    if snapshot.is_empty() {
        return 0;
    }
    let guard = guard();
    let mut delivered = 0usize;
    for weak in &snapshot {
        let Some(cap) = weak.upgrade(&guard) else {
            continue;
        };
        if cap.notify(signum) {
            delivered += 1;
        }
    }
    drop(guard);
    delivered
}

/// `signalfd_create` — mint a fresh `Cap<SignalFd>` bound to
/// `owner_proc` with the given `mask`. Wraps
/// [`SignalFd::new_cap_for_process`] for the canonical syscall flow.
pub fn signalfd_create(
    owner_proc: &Cap<ProcessIdentity>,
    mask: u64,
) -> Result<Cap<SignalFd>, ZoneError> {
    SignalFd::new_cap_for_process(owner_proc, mask)
}

/// `signalfd_read` — pop one [`struct signalfd_siginfo`]-shaped record
/// off the pending queue and serialize it into `out`. Mirrors
/// `step_ufd_read`'s shape:
///
/// - `out.len() < SIGNALFD_SIGINFO_SIZE` → `Err(EINVAL)`.
/// - queue non-empty → `Done(SIGNALFD_SIGINFO_SIZE)` after
///   serializing one zero-filled record with the popped `ssi_signo`.
/// - queue empty + `nonblocking` → `Err(EAGAIN)`.
/// - queue empty + blocking → `Yield { OnWaitSource }` on the per-fd
///   wait source; the dispatcher parks on
///   `wait_source::wait_on_token` and re-polls when
///   [`SignalFd::notify`] fires.
pub fn signalfd_read(
    sfd: &SignalFd,
    out: &mut [u8],
    nonblocking: bool,
) -> StepOutcome<usize, ByteProgress> {
    if out.is_empty() {
        return StepOutcome::done(0);
    }
    if out.len() < SIGNALFD_SIGINFO_SIZE {
        return StepOutcome::err(V3Errno::EINVAL);
    }

    if let Some(signum) = sfd.pop_pending() {
        let bytes = serialize_signalfd_siginfo(signum);
        out[..SIGNALFD_SIGINFO_SIZE].copy_from_slice(&bytes);
        return StepOutcome::done(SIGNALFD_SIGINFO_SIZE);
    }

    if nonblocking {
        return StepOutcome::err(V3Errno::EAGAIN);
    }
    StepOutcome::yield_on_wait_source(ByteProgress::EMPTY, sfd.wait_source_id(), SIGNALFD_READABLE)
}

/// Serialize a single `Signum` into a 128-byte
/// `struct signalfd_siginfo` record. Phase D9-D zero-fills every
/// field except `ssi_signo` — siginfo plumbing (`ssi_pid`,
/// `ssi_uid`, `ssi_code`) lands once the real siginfo payload exists
/// (see D9 §11 follow-ups).
fn serialize_signalfd_siginfo(signum: Signum) -> [u8; SIGNALFD_SIGINFO_SIZE] {
    let mut out = [0u8; SIGNALFD_SIGINFO_SIZE];
    let ssi_signo = signum.raw() as u32;
    out[0..4].copy_from_slice(&ssi_signo.to_le_bytes());
    // bytes [4..8] = ssi_errno (i32, zero — phase D9-D)
    // bytes [8..12] = ssi_code (i32, zero — phase D9-D)
    // bytes [12..16] = ssi_pid (u32, zero — phase D9-D)
    // bytes [16..20] = ssi_uid (u32, zero — phase D9-D)
    // bytes [20..128] reserved / zero
    out
}

// === zone wiring ======================================================

static SIGNALFD_ZONE: Zone<SignalFd> = Zone::const_new();

unsafe impl ZoneAllocated for SignalFd {
    fn zone() -> &'static Zone<Self> {
        &SIGNALFD_ZONE
    }
}

pub(crate) fn register_zones() -> Result<(), ZoneError> {
    adapter::step_engine::register_zone_for::<SignalFd>()?;
    Ok(())
}

// === test-only counter reset =========================================

#[cfg(any(test, feature = "test-support"))]
pub fn reset_sfd_id_counter_for_test() {
    NEXT_SFD_ID.store(1, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::EPOCH_TEST_LOCK;
    use crate::zones;

    fn setup() -> std::sync::MutexGuard<'static, ()> {
        let guard = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        tx_test_support::init_host();
        let _ = zones::register_all();
        tx_test_support::drain_to_quiescence();
        guard
    }

    #[test]
    fn distinct_signalfds_have_distinct_ids() {
        let _g = setup();
        // Use a faked owner_proc_key = 0; for the no-process raw path
        // we sign directly via the zone (skipping the registry).
        let a = {
            sign(SignalFd::new(0, 0)).expect("reserve a")
        };
        let b = {
            sign(SignalFd::new(0, 0)).expect("reserve b")
        };
        assert_ne!(
            a.sfd_id(),
            b.sfd_id(),
            "each SignalFd must mint a fresh sfd_id"
        );
    }

    #[test]
    fn notify_filters_against_mask() {
        let _g = setup();
        // Subscribe to SIGUSR1 only (signum 10 on Linux generic).
        let sigusr1 = Signum::new(10).expect("SIGUSR1");
        let sigusr2 = Signum::new(12).expect("SIGUSR2");

        let cap = {
            sign(SignalFd::new(0, sigusr1.bit())).expect("reserve")
        };

        // SIGUSR2 is not in the mask — drop on the floor.
        assert!(!cap.notify(sigusr2));
        assert_eq!(cap.pending_count(), 0);

        // SIGUSR1 is — pushes onto the queue.
        assert!(cap.notify(sigusr1));
        assert_eq!(cap.pending_count(), 1);

        // Pop returns the signum.
        let popped = cap.pop_pending().expect("queue had one entry");
        assert_eq!(popped.raw(), sigusr1.raw());
        assert_eq!(cap.pending_count(), 0);
    }

    #[test]
    fn signalfd_read_returns_eagain_when_empty_and_nonblocking() {
        let _g = setup();
        let cap = {
            sign(SignalFd::new(0, !0u64)).expect("reserve")
        };
        let mut buf = [0u8; SIGNALFD_SIGINFO_SIZE];
        let outcome = signalfd_read(&cap, &mut buf, /* nonblocking = */ true);
        match outcome {
            StepOutcome::Err(V3Errno::EAGAIN) => {}
            other => panic!("expected EAGAIN, got {other:?}"),
        }
    }

    #[test]
    fn signalfd_read_serializes_popped_siginfo() {
        let _g = setup();
        let sigusr1 = Signum::new(10).expect("SIGUSR1");
        let cap = {
            sign(SignalFd::new(0, sigusr1.bit())).expect("reserve")
        };
        assert!(cap.notify(sigusr1));
        let mut buf = [0xFFu8; SIGNALFD_SIGINFO_SIZE];
        let outcome = signalfd_read(&cap, &mut buf, false);
        match outcome {
            StepOutcome::Done(n) => assert_eq!(n, SIGNALFD_SIGINFO_SIZE),
            other => panic!("expected Done(128), got {other:?}"),
        }
        // ssi_signo is the first 4 bytes.
        let signo = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        assert_eq!(signo, sigusr1.raw() as u32);
        // All other bytes are zero (phase D9-D zero-fills).
        assert!(buf[4..].iter().all(|&b| b == 0), "phase D9-D zero-fills");
    }

    #[test]
    fn signalfd_read_short_buf_returns_einval() {
        let _g = setup();
        let cap = {
            sign(SignalFd::new(0, !0u64)).expect("reserve")
        };
        let mut short_buf = [0u8; SIGNALFD_SIGINFO_SIZE - 1];
        let outcome = signalfd_read(&cap, &mut short_buf, false);
        match outcome {
            StepOutcome::Err(V3Errno::EINVAL) => {}
            other => panic!("expected EINVAL on short buf, got {other:?}"),
        }
    }
}
