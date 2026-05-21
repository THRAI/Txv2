//! Pid-name resolution — maps pid/tid numbers to identity Caps.
//!
//! ## Architecture (PROCESS_v1 §3.1.1)
//!
//! `PidName` is the authoritative binding between a numeric pid/tid
//! and its identity.  It lives in a global `BTreeMap` keyed by raw
//! number.  `PidNameKind` discriminates process-level vs thread-level
//! bindings.
//!
//! ## v1 scope — single-root namespace
//!
//! Multi-level pid namespaces are deferred.  All pid/tid numbers
//! share a single global registry.

use alloc::collections::BTreeMap;

use crate::adapter::step_engine::{Cap, SpinMutex};
use crate::process::structure::{Pid, ProcessIdentity};
use crate::thread_runtime::structure::{ThreadIdentity, Tid};
// allocate_tid re-exported via thread_runtime

// ---------------------------------------------------------------------------
// PidName
// ---------------------------------------------------------------------------

/// Authoritative pid/tid → identity binding.
#[derive(Clone)]
pub enum PidName {
    Process(Cap<ProcessIdentity>),
    Thread(Cap<ThreadIdentity>),
}

/// Discriminant for `PidName`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PidNameKind {
    Process,
    Thread,
}

impl PidName {
    pub fn kind(&self) -> PidNameKind {
        match self {
            PidName::Process(_) => PidNameKind::Process,
            PidName::Thread(_) => PidNameKind::Thread,
        }
    }
}

// ---------------------------------------------------------------------------
// Global registry
// ---------------------------------------------------------------------------

static PID_NS: SpinMutex<BTreeMap<u64, PidName>> = SpinMutex::new(BTreeMap::new());

/// Register a process-level pid.
pub fn register_pid(pid: Pid, cap: Cap<ProcessIdentity>) {
    PID_NS.lock().insert(pid.0 as u64, PidName::Process(cap));
}

/// Register a thread-level tid.
pub fn register_tid(tid: Tid, cap: Cap<ThreadIdentity>) {
    PID_NS.lock().insert(tid.0 as u64, PidName::Thread(cap));
}

/// Unregister a number.
pub fn unregister_pid_number(number: u64) {
    PID_NS.lock().remove(&number);
}

/// Resolve a number to its PidName.
pub fn resolve_pid_number(number: u64) -> Option<PidName> {
    PID_NS.lock().get(&number).cloned()
}

/// Iterate all entries (for procfs).
pub fn with_namespace<T>(f: impl FnOnce(&BTreeMap<u64, PidName>) -> T) -> T {
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
