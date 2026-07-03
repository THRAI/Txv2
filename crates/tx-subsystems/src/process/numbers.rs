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

use crate::process::adapter::step_engine::{process_spin_mutex, Cap, ProcessSpinMutex};
use crate::process::structure::{Pgid, Pid, ProcessGroup, ProcessIdentity, Session, Sid};
use crate::thread_runtime::structure::{ThreadIdentity, Tid};
// allocate_tid re-exported via thread_runtime

macro_rules! measure_process_ds {
    ($method_name:expr, $body:block) => {{
        #[cfg(all(tx_ds_metrics, tx_ds_metrics_process))]
        {
            crate::process::ds_metrics::measure($method_name, || $body)
        }
        #[cfg(not(all(tx_ds_metrics, tx_ds_metrics_process)))]
        {
            $body
        }
    }};
}

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

static PID_NS: ProcessSpinMutex<BTreeMap<(u64, PidNameKind), PidName>> =
    process_spin_mutex(BTreeMap::new(), b"debug.lock.process.pid_namespace");

/// Register a process-level pid.
pub fn register_pid(pid: Pid, cap: Cap<ProcessIdentity>) {
    measure_process_ds!(b"debug.ds.process.pid_namespace.register_pid", {
        PID_NS
            .lock()
            .insert((pid.0 as u64, PidNameKind::Process), PidName::Process(cap));
    });
}

/// Register a thread-level tid.
pub fn register_tid(tid: Tid, cap: Cap<ThreadIdentity>) {
    measure_process_ds!(b"debug.ds.process.pid_namespace.register_tid", {
        PID_NS
            .lock()
            .insert((tid.0 as u64, PidNameKind::Thread), PidName::Thread(cap));
    });
}

/// Register a process-group-level pgid.
pub fn register_pgrp(pgid: Pgid, cap: Cap<ProcessGroup>) {
    measure_process_ds!(b"debug.ds.process.pid_namespace.register_pgrp", {
        PID_NS.lock().insert(
            (pgid.0 as u64, PidNameKind::ProcessGroup),
            PidName::ProcessGroup(cap),
        );
    });
}

/// Register a session-level sid.
pub fn register_session(sid: Sid, cap: Cap<Session>) {
    measure_process_ds!(b"debug.ds.process.pid_namespace.register_session", {
        PID_NS
            .lock()
            .insert((sid.0 as u64, PidNameKind::Session), PidName::Session(cap));
    });
}

/// Unregister every role currently bound to a raw number.
pub fn unregister_pid_number(number: u64) {
    measure_process_ds!(b"debug.ds.process.pid_namespace.unregister_pid_number", {
        PID_NS.lock().retain(|(key, _), _| *key != number);
    });
}

/// Unregister only the thread-level TID binding for a raw number.
pub fn unregister_tid_number(number: u64) {
    measure_process_ds!(b"debug.ds.process.pid_namespace.unregister_tid_number", {
        PID_NS.lock().remove(&(number, PidNameKind::Thread));
    });
}

/// Resolve a number to its preferred `PidName`.
///
/// Compatibility helper for older call sites. New code should prefer
/// [`resolve_pid_number_as`] so pid, tid, pgid, and sid lookups do not
/// depend on role priority.
pub fn resolve_pid_number(number: u64) -> Option<PidName> {
    measure_process_ds!(b"debug.ds.process.pid_namespace.resolve_pid_number", {
        let ns = PID_NS.lock();
        [
            PidNameKind::Process,
            PidNameKind::Thread,
            PidNameKind::ProcessGroup,
            PidNameKind::Session,
        ]
        .into_iter()
        .find_map(|kind| ns.get(&(number, kind)).cloned())
    })
}

/// Resolve a number to a specific pid-namespace role.
pub fn resolve_pid_number_as(number: u64, kind: PidNameKind) -> Option<PidName> {
    measure_process_ds!(b"debug.ds.process.pid_namespace.resolve_pid_number_as", {
        PID_NS.lock().get(&(number, kind)).cloned()
    })
}

/// Iterate all entries (for procfs).
pub fn with_namespace<T>(f: impl FnOnce(&BTreeMap<(u64, PidNameKind), PidName>) -> T) -> T {
    measure_process_ds!(b"debug.ds.process.pid_namespace.with_namespace", {
        f(&PID_NS.lock())
    })
}

// ---------------------------------------------------------------------------
// Allocation
// ---------------------------------------------------------------------------

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

static NEXT_PID: AtomicU32 = AtomicU32::new(2);

/// One-shot tripwire well below procfs' pid-id window
/// (`PROCFS_PID_LIMIT` = 0x40_0000): this counter is a monotone
/// scaffold shared by pids AND tids and never recycles (PROCESS_v1
/// specifies a bitmap `reserve_pid_name` allocator instead — unbuilt).
/// A boot that outgrows the window makes `/proc/<pid>` stop resolving,
/// which wedged the 2026-07-02 ltp-glibc lane at fs_bind_move01. Fire
/// a loud marker at 3M (75% of the window) so the ceiling announces
/// itself before it hurts. See ljs/08-pid分配与procfs窗口事故.md.
const PID_TRIPWIRE: u32 = 0x30_0000;
static PID_TRIPWIRE_SINK: AtomicUsize = AtomicUsize::new(0);

/// Register the kernel-side console warning hook (this crate is
/// platform-independent and cannot emit console bytes itself).
pub fn install_pid_tripwire_sink(sink: fn()) {
    PID_TRIPWIRE_SINK.store(sink as usize, Ordering::Release);
}

fn next_number() -> u32 {
    let number = NEXT_PID.fetch_add(1, Ordering::Relaxed);
    if number == PID_TRIPWIRE {
        let sink = PID_TRIPWIRE_SINK.load(Ordering::Acquire);
        if sink != 0 {
            // SAFETY: only ever stored from `install_pid_tripwire_sink`
            // with a valid `fn()`.
            let sink: fn() = unsafe { core::mem::transmute(sink) };
            sink();
        }
    }
    number
}

pub fn allocate_pid() -> Pid {
    Pid(next_number())
}

/// Allocate a unique TID from the shared PID/TID space.
pub fn allocate_tid() -> Tid {
    Tid(next_number())
}

pub fn reset_pid_counter_for_test() {
    NEXT_PID.store(2, Ordering::Relaxed);
    PID_NS.lock().clear();
}
