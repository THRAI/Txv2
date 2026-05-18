//! `epoll(7)` — I/O event notification facility (Phase B.1).
//!
//! Spec: `docs/Txv3/03_STEP_MODEL_v2.md` §5 `YieldShape::OnEdge`.
//!
//! # Architecture
//!
//! Each [`Epoll`] instance carries a [`WaitSource`] that fires when
//! any monitored fd becomes ready.  The `epoll_wait` step yields
//! `YieldShape::OnEdge { source, interests }` to park the callee
//! until readiness is signalled.  On wake, monitored fds are scanned
//! and ready events collected into a userspace event buffer.
//!
//! # Phase B.1 surface
//!
//! 1. Zone-allocated [`Epoll`] identity.
//! 2. `epoll_create1(2)` — allocates an Epoll, installs as an fd.
//! 3. `epoll_ctl(2)` — ADD/MOD/DEL a monitored fd.
//! 4. `epoll_wait(2)` — block until ready, collect events.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

pub mod adapter;

use adapter::step_engine::{
    ByteProgress, InterestMask, NoProgress, SpinMutex, StepOutcome, WaitSource, WaitSourceId,
    YieldShape, Zone, ZoneAllocated, ZoneError,
};

// ---------------------------------------------------------------------------
// Epoll
// ---------------------------------------------------------------------------

/// Monitored fd entry.
#[allow(dead_code)] // txdoc:vfs-full-bringup-scaffold
#[derive(Clone, Debug)]
struct EpollEntry {
    /// Userspace fd number.
    fd: u32,
    /// `epoll_event.events` mask set by `EPOLL_CTL_ADD` / `EPOLL_CTL_MOD`.
    interests: u32,
    /// The monitored fd's [`WaitSourceId`] (the bus wire that fires
    /// when the fd becomes readable/writable). `WaitSourceId::new(0)`
    /// is a sentinel meaning "source not yet known" — the entry is
    /// tracked but does not contribute to readiness.
    source: WaitSourceId,
}

/// Per-instance epoll state.
pub struct Epoll {
    /// Unique id for this epoll instance.
    epoll_id: u64,
    /// Monitored fds, keyed by userspace fd number.
    fds: SpinMutex<BTreeMap<u32, EpollEntry>>,
    /// Wait source fired when any monitored fd becomes ready.
    wait_source: Arc<WaitSource>,
}

// ---------------------------------------------------------------------------
// id minting
// ---------------------------------------------------------------------------

static NEXT_EPOLL_ID: AtomicU64 = AtomicU64::new(1);

fn allocate_epoll_id() -> u64 {
    NEXT_EPOLL_ID.fetch_add(1, Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// zone
// ---------------------------------------------------------------------------

static EPOLL_ZONE: Zone<Epoll> = Zone::const_new();

unsafe impl ZoneAllocated for Epoll {
    fn zone() -> &'static Zone<Self> {
        &EPOLL_ZONE
    }
}

#[allow(dead_code)] // txdoc:vfs-full-bringup-scaffold
pub(crate) fn register_zones() -> Result<(), ZoneError> {
    adapter::step_engine::register_zone_for::<Epoll>()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Epoll impl
// ---------------------------------------------------------------------------

impl Default for Epoll {
    fn default() -> Self {
        Self::new()
    }
}

impl Epoll {
    pub fn new() -> Self {
        let id = WaitSourceId::new(allocate_epoll_id());
        Self {
            epoll_id: id.raw(),
            fds: SpinMutex::new(BTreeMap::new()),
            wait_source: Arc::new(WaitSource::new(id)),
        }
    }

    pub const fn epoll_id(&self) -> u64 {
        self.epoll_id
    }

    pub fn wait_source_id(&self) -> WaitSourceId {
        self.wait_source.id()
    }
}

// ---------------------------------------------------------------------------
// step functions
// ---------------------------------------------------------------------------

/// `epoll_ctl(ADD)` / `MOD`: register or update a monitored fd.
///
/// `source` is the monitored fd's [`WaitSourceId`] — the bus wire
/// that fires when the fd becomes ready.  Callers that cannot
/// resolve a source may pass `WaitSourceId::new(0)` (sentinel),
/// which means the entry is tracked but does not contribute to
/// readiness until a source is wired (Phase B.3+).
pub fn step_epoll_ctl_add(
    ep: &Epoll,
    fd: u32,
    interests: u32,
    source: WaitSourceId,
) -> StepOutcome<(), NoProgress> {
    // observe — N/A: ep is &Epoll (always alive)
    // upgrade — N/A: fd is u32, not a Cap
    // reserve — BTreeMap insert under SpinMutex
    // commit — entry inserted atomically with respect to the lock
    // publish — N/A: no signal attachments
    let mut fds = ep.fds.lock();
    fds.insert(
        fd,
        EpollEntry {
            fd,
            interests,
            source,
        },
    );
    StepOutcome::Done(())
}

/// `epoll_ctl(DEL)`: remove a monitored fd.
pub fn step_epoll_ctl_del(ep: &Epoll, fd: u32) -> StepOutcome<(), NoProgress> {
    // observe — N/A
    // upgrade — N/A
    // reserve — BTreeMap remove under SpinMutex
    // commit — entry removed
    // publish — N/A
    let mut fds = ep.fds.lock();
    fds.remove(&fd);
    StepOutcome::Done(())
}

/// `epoll_wait`: block until ready events arrive, then return count.
///
/// Yields `YieldShape::OnEdge` to park until the epoll instance's
/// [`WaitSource`] fires.  On wake, scans monitored fds and collects
/// ready events into the caller-supplied buffer.
///
/// # Phase B.2 readiness scanning
///
/// Monitored fds whose `source` is a valid [`WaitSourceId`] (not
/// `WaitSourceId::new(0)`) are checked for readiness.  The count
/// of ready fds is returned.  If no fds are ready but some are
/// monitored, the step yields `OnEdge` to park until a source fires.
pub fn step_epoll_wait(
    ep: &Epoll,
    _guard: &crate::execution::Guard<'_>,
) -> StepOutcome<usize, ByteProgress> {
    // observe — N/A (epoll always live)
    // upgrade — N/A (&Epoll, no ident→cap)
    // reserve — read fds lock (shared)
    // commit — scan monitored fds; return ready count
    // publish — N/A: no signal attachments
    let fds = ep.fds.lock();
    if fds.is_empty() {
        // Park until a monitored fd is added and fires.
        return StepOutcome::Yield {
            progress: ByteProgress::new(0),
            shape: YieldShape::OnEdge {
                source: ep.wait_source_id(),
                interests: InterestMask::new(1), // "readable" epoll fd
            },
        };
    }

    // Phase B.2 PoC: count monitored fds with a valid source id.
    // Real EPOLLIN/EPOLLOUT detection requires bus subscription
    // per fd — Phase B.3.
    let ready: usize = fds.values().filter(|e| e.source.raw() != 0).count();

    if ready > 0 {
        StepOutcome::Done(ready)
    } else {
        // Fds are monitored but none have a wired source yet.
        // Yield to park until a source is connected.
        StepOutcome::Yield {
            progress: ByteProgress::new(0),
            shape: YieldShape::OnEdge {
                source: ep.wait_source_id(),
                interests: InterestMask::new(1),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// test support
// ---------------------------------------------------------------------------

#[cfg(any(test, feature = "test-support"))]
pub fn reset_epoll_id_counter_for_test() {
    NEXT_EPOLL_ID.store(1, Ordering::Release);
}
