//! Pid-name resolution — maps pid/tid numbers to identity Caps.
//!
//! ## Architecture (PROCESS_v1 §3.1.1)
//!
//! `PidName` is the authoritative binding between a numeric pid/tid/
//! pgid/sid and its identity. It lives in a global `BTreeMap` keyed by
//! raw number plus role so process leaders can use the same numeric id
//! as their pid, tid, process group, and session without overwriting
//! one another.
//!
//! ## v1 scope — single-root namespace
//!
//! Multi-level pid namespaces are deferred.  All pid/tid numbers
//! share a single global registry.

use alloc::collections::BTreeMap;

use crate::adapter::step_engine::{Cap, SpinMutex};
use crate::process::structure::{Pgid, Pid, ProcessGroup, ProcessIdentity, Session, Sid};
use crate::thread_runtime::structure::{ThreadIdentity, Tid};
// allocate_tid re-exported via thread_runtime

// ---------------------------------------------------------------------------
// PidName
// ---------------------------------------------------------------------------

/// Authoritative pid/tid/pgid/sid → identity binding.
#[derive(Clone, Debug)]
pub enum PidName {
    Process(Cap<ProcessIdentity>),
    Thread(Cap<ThreadIdentity>),
    ProcessGroup(Cap<ProcessGroup>),
    Session(Cap<Session>),
}

/// Discriminant for `PidName`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PidNameKind {
    Process,
    Thread,
    ProcessGroup,
    Session,
}

impl PidName {
    pub fn kind(&self) -> PidNameKind {
        match self {
            PidName::Process(_) => PidNameKind::Process,
            PidName::Thread(_) => PidNameKind::Thread,
            PidName::ProcessGroup(_) => PidNameKind::ProcessGroup,
            PidName::Session(_) => PidNameKind::Session,
        }
    }
}

// ---------------------------------------------------------------------------
// Global registry
// ---------------------------------------------------------------------------

static PID_NS: SpinMutex<BTreeMap<(u64, PidNameKind), PidName>> = SpinMutex::new(BTreeMap::new());

/// Register a process-level pid.
pub fn register_pid(pid: Pid, cap: Cap<ProcessIdentity>) {
    PID_NS
        .lock()
        .insert((pid.0 as u64, PidNameKind::Process), PidName::Process(cap));
}

/// Register a thread-level tid.
pub fn register_tid(tid: Tid, cap: Cap<ThreadIdentity>) {
    PID_NS
        .lock()
        .insert((tid.0 as u64, PidNameKind::Thread), PidName::Thread(cap));
}

/// Register a process-group-level pgid.
pub fn register_pgrp(pgid: Pgid, cap: Cap<ProcessGroup>) {
    PID_NS.lock().insert(
        (pgid.0 as u64, PidNameKind::ProcessGroup),
        PidName::ProcessGroup(cap),
    );
}

/// Register a session-level sid.
pub fn register_session(sid: Sid, cap: Cap<Session>) {
    PID_NS
        .lock()
        .insert((sid.0 as u64, PidNameKind::Session), PidName::Session(cap));
}

/// Unregister every role currently bound to a raw number.
pub fn unregister_pid_number(number: u64) {
    PID_NS.lock().retain(|(key, _), _| *key != number);
}

/// Unregister only the thread-level TID binding for a raw number.
pub fn unregister_tid_number(number: u64) {
    PID_NS.lock().remove(&(number, PidNameKind::Thread));
}

/// Resolve a number to its preferred `PidName`.
///
/// Compatibility helper for older call sites. New code should prefer
/// [`resolve_pid_number_as`] so pid, tid, pgid, and sid lookups do not
/// depend on role priority.
pub fn resolve_pid_number(number: u64) -> Option<PidName> {
    let ns = PID_NS.lock();
    [
        PidNameKind::Process,
        PidNameKind::Thread,
        PidNameKind::ProcessGroup,
        PidNameKind::Session,
    ]
    .into_iter()
    .find_map(|kind| ns.get(&(number, kind)).cloned())
}

/// Resolve a number to a specific pid-namespace role.
pub fn resolve_pid_number_as(number: u64, kind: PidNameKind) -> Option<PidName> {
    PID_NS.lock().get(&(number, kind)).cloned()
}

/// Iterate all entries (for procfs).
pub fn with_namespace<T>(f: impl FnOnce(&BTreeMap<(u64, PidNameKind), PidName>) -> T) -> T {
    f(&PID_NS.lock())
}

// ---------------------------------------------------------------------------
// Allocation
// ---------------------------------------------------------------------------

use core::sync::atomic::{AtomicU32, Ordering};

static NEXT_PID: AtomicU32 = AtomicU32::new(2);

pub fn allocate_pid() -> Pid {
    Pid(NEXT_PID.fetch_add(1, Ordering::Relaxed))
}

/// Allocate a unique TID from the shared PID/TID space.
pub fn allocate_tid() -> Tid {
    Tid(NEXT_PID.fetch_add(1, Ordering::Relaxed))
}

pub fn reset_pid_counter_for_test() {
    NEXT_PID.store(2, Ordering::Relaxed);
    PID_NS.lock().clear();
}
