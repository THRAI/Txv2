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
    ByteProgress, Cap, InterestMask, NoProgress, SpinMutex, StepOutcome, V3Errno, WaitSource,
    WaitSourceId, YieldShape, Zone, ZoneAllocated, ZoneError,
};

// ---------------------------------------------------------------------------
// Epoll
// ---------------------------------------------------------------------------

/// Monitored fd entry.
#[derive(Clone, Debug)]
pub struct EpollEntry {
    /// Userspace fd number.
    pub fd: u32,
    /// `epoll_event.events` mask set by `EPOLL_CTL_ADD` / `EPOLL_CTL_MOD`.
    pub interests: u32,
    /// Opaque `epoll_event.data` payload copied from userspace.
    pub data: u64,
    /// The monitored fd's [`WaitSourceId`] (the bus wire that fires
    /// when the fd becomes readable/writable). `WaitSourceId::new(0)`
    /// is a sentinel meaning "source not yet known" — the entry is
    /// tracked but does not contribute to readiness.
    pub source: WaitSourceId,
    /// Target epoll, when the monitored fd is itself an epoll fd.
    pub target_epoll: Option<Cap<Epoll>>,
    /// Last readiness mask observed by `epoll_wait`. Used by the
    /// syscall shim to implement edge-triggered delivery without
    /// re-reporting a level that has not transitioned.
    pub last_ready: u32,
    /// `EPOLLONESHOT` disables the entry after one delivered event
    /// until userspace re-enables it with `EPOLL_CTL_MOD`.
    pub disabled: bool,
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
const EPOLL_MAX_NEST_DEPTH: usize = 5;

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

    pub fn entries_snapshot(&self) -> alloc::vec::Vec<EpollEntry> {
        self.fds.lock().values().cloned().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.fds.lock().is_empty()
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
    data: u64,
    source: WaitSourceId,
    target_epoll: Option<Cap<Epoll>>,
) -> StepOutcome<(), NoProgress> {
    // observe — N/A: ep is &Epoll (always alive)
    // upgrade — N/A: fd is u32, not a Cap
    // reserve — BTreeMap insert under SpinMutex
    // commit — entry inserted atomically with respect to the lock
    // publish — N/A: no signal attachments
    {
        let fds = ep.fds.lock();
        if fds.contains_key(&fd) {
            return StepOutcome::Err(V3Errno::EEXIST);
        }
    }

    if let Err(errno) = validate_epoll_target(ep, target_epoll.as_ref()) {
        return StepOutcome::Err(errno);
    }

    let mut fds = ep.fds.lock();
    if fds.contains_key(&fd) {
        return StepOutcome::Err(V3Errno::EEXIST);
    }
    fds.insert(
        fd,
        EpollEntry {
            fd,
            interests,
            data,
            source,
            target_epoll,
            last_ready: 0,
            disabled: false,
        },
    );
    StepOutcome::Done(())
}

/// `epoll_ctl(MOD)`: update an existing monitored fd.
pub fn step_epoll_ctl_mod(
    ep: &Epoll,
    fd: u32,
    interests: u32,
    data: u64,
    source: WaitSourceId,
    target_epoll: Option<Cap<Epoll>>,
) -> StepOutcome<(), NoProgress> {
    {
        let fds = ep.fds.lock();
        if !fds.contains_key(&fd) {
            return StepOutcome::Err(V3Errno::ENOENT);
        }
    }

    if let Err(errno) = validate_epoll_target(ep, target_epoll.as_ref()) {
        return StepOutcome::Err(errno);
    }

    let mut fds = ep.fds.lock();
    let Some(entry) = fds.get_mut(&fd) else {
        return StepOutcome::Err(V3Errno::ENOENT);
    };
    *entry = EpollEntry {
        fd,
        interests,
        data,
        source,
        target_epoll,
        last_ready: 0,
        disabled: false,
    };
    StepOutcome::Done(())
}

pub fn step_epoll_note_ready(
    ep: &Epoll,
    fd: u32,
    ready: u32,
    disable_after_delivery: bool,
) -> StepOutcome<(), NoProgress> {
    let mut fds = ep.fds.lock();
    let Some(entry) = fds.get_mut(&fd) else {
        return StepOutcome::Err(V3Errno::ENOENT);
    };
    entry.last_ready = ready;
    if disable_after_delivery {
        entry.disabled = true;
    }
    StepOutcome::Done(())
}

fn validate_epoll_target(ep: &Epoll, target_epoll: Option<&Cap<Epoll>>) -> Result<(), V3Errno> {
    let Some(target) = target_epoll else {
        return Ok(());
    };

    if epoll_reaches(target, ep.epoll_id(), EPOLL_MAX_NEST_DEPTH) {
        return Err(V3Errno::ELOOP);
    }

    let resulting_depth = 1 + epoll_max_depth(target, EPOLL_MAX_NEST_DEPTH);
    if resulting_depth > EPOLL_MAX_NEST_DEPTH {
        return Err(V3Errno::EINVAL);
    }

    Ok(())
}

fn epoll_reaches(ep: &Epoll, target_id: u64, budget: usize) -> bool {
    if ep.epoll_id() == target_id {
        return true;
    }
    if budget == 0 {
        return false;
    }

    ep.entries_snapshot()
        .iter()
        .filter_map(|entry| entry.target_epoll.as_ref())
        .any(|child| epoll_reaches(child, target_id, budget - 1))
}

fn epoll_max_depth(ep: &Epoll, budget: usize) -> usize {
    if budget == 0 {
        return 1;
    }

    let max_child_depth = ep
        .entries_snapshot()
        .iter()
        .filter_map(|entry| entry.target_epoll.as_ref())
        .map(|child| epoll_max_depth(child, budget - 1))
        .max()
        .unwrap_or(0);
    1 + max_child_depth
}

/// `epoll_ctl(DEL)`: remove a monitored fd.
pub fn step_epoll_ctl_del(ep: &Epoll, fd: u32) -> StepOutcome<(), NoProgress> {
    // observe — N/A
    // upgrade — N/A
    // reserve — BTreeMap remove under SpinMutex
    // commit — entry removed
    // publish — N/A
    let mut fds = ep.fds.lock();
    if fds.remove(&fd).is_some() {
        StepOutcome::Done(())
    } else {
        StepOutcome::Err(V3Errno::ENOENT)
    }
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
