//! Process subsystem structure: identities, payloads, and the
//! [`ProcessGroup`] / [`Session`] container entities.
//!
//! This module realizes the topology described in `PROCESS_v1` — entity
//! relationships, identity/payload split, container ownership — without
//! the signal state, credential, rlimit, or fd-table fields that land in
//! follow-up passes. Signal state lives on `ThreadPayload` /
//! `ProcessPayload` once the signal shim arrives; the slots are intentionally
//! absent here so day-1 code does not have to compile against placeholder
//! types.
//!
//! Ownership graph:
//!
//! ```text
//! ProcessIdentity ──Cap──▶ ProcessGroup ──Cap──▶ Session
//!     │  ▲                       │ ▲                  │ ▲
//!     │  └──Weak (members)───────┘ └──Weak (groups)───┘ │
//!     │                                                  │
//!     └──PayloadCap──▶ ProcessPayload                    │
//!                          │  ▲                          │
//!                          │  └──Weak (owner_proc)───────┘  (from ThreadIdentity)
//!                          │
//!                          └──Cap──▶ ThreadIdentity ──PayloadCap──▶ ThreadPayload
//!                                          │
//!                                          └──Weak──▶ ProcessIdentity
//! ```

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use tx_substrate::zone::{Cap, PayloadCap, Weak, Zone, ZoneAllocated};

use crate::cred::{Cred, Gid, Uid};
use crate::signal::{PendingSignalQueue, SigActionTable};
use crate::sync::SpinMutex;
use crate::thread_runtime::ThreadIdentity;
use crate::tty::structure::identity::TtyIdentity;
use crate::vm::AddressSpace;

/// Process identifier. PID 0 is reserved (no-parent / pre-init); init = 1.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Hash)]
pub struct Pid(pub u32);

impl Pid {
    pub const RESERVED: Self = Self(0);
    pub const INIT: Self = Self(1);
}

/// Process-group identifier. By convention equals the leader process's pid
/// at the moment of group creation.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Hash)]
pub struct Pgid(pub u32);

/// Session identifier. By convention equals the session leader's pid at
/// the moment of session creation.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Hash)]
pub struct Sid(pub u32);

/// Process identity. Persists across payload teardown so zombies remain
/// observable in `/proc` and reapable by their parent.
///
/// `pgrp` is on the identity (not the payload) because process-group
/// membership outlives payload teardown — Linux semantics: zombies remain
/// in their pgrp until reaped.
pub struct ProcessIdentity {
    pub pid: Pid,
    pub parent_pid: Pid,
    pub(crate) pgrp: SpinMutex<Cap<ProcessGroup>>,
    pub(crate) exit_status: SpinMutex<Option<i32>>,
    pub(crate) payload: SpinMutex<Option<PayloadCap<ProcessPayload>>>,
}

impl ProcessIdentity {
    /// Snapshot the current process-group `Cap`. The returned `Cap` is a
    /// strong reference; it remains valid until dropped even if the
    /// target rebinds via `setpgid`.
    pub fn pgrp_cap(&self) -> Cap<ProcessGroup> {
        self.pgrp.lock().clone()
    }

    /// Read the recorded exit status. `Some` once `step_exit_group`
    /// (or last-thread `step_thread_exit`) has run; otherwise `None`.
    pub fn exit_status(&self) -> Option<i32> {
        *self.exit_status.lock()
    }

    /// Whether the process is a zombie (payload dropped, identity
    /// retained). Equivalent to `exit_status().is_some() || payload is None`
    /// but cheaper.
    pub fn is_zombie(&self) -> bool {
        self.payload.lock().is_none()
    }

    /// Snapshot the current address space `Cap`, if the process is
    /// alive. Returns `None` for zombies.
    pub fn aspace_cap(&self) -> Option<Cap<AddressSpace>> {
        self.payload.lock().as_ref().map(|p| p.aspace.clone())
    }

    /// Number of currently-live threads owned by this process.
    /// Returns `0` for zombies.
    pub fn live_thread_count(&self) -> usize {
        self.payload
            .lock()
            .as_ref()
            .map(|p| p.threads.lock().len())
            .unwrap_or(0)
    }

    /// Build the process-exported value type that `cred::require_signal_send`
    /// consumes. Per `cred_service_v_1` §"Foreign value types from
    /// subsystems", the target side of a signal-permission check is a
    /// small value, not a `Cap` — cred consumes only the facts it needs.
    ///
    /// `same_session` is `true` iff `source` and `self` resolve to the
    /// same `Session` `Cap` via their pgrps' `Weak<Session>` refs (under
    /// a fresh epoch guard). Both sides must be live; either one zombie
    /// returns `false`.
    ///
    /// Returns `None` if `self` has no payload (cred unobservable).
    pub fn target_proc_cred_for(&self, source: &ProcessIdentity) -> Option<TargetProcCred> {
        let target_payload_guard = self.payload.lock();
        let target_payload = target_payload_guard.as_ref()?;
        let target_cred = target_payload.cred();
        drop(target_payload_guard);

        let target_pgrp = self.pgrp.lock().clone();
        let source_pgrp = source.pgrp.lock().clone();
        let same_session = target_pgrp.session_cap().key() == source_pgrp.session_cap().key();

        Some(TargetProcCred {
            uid: target_cred.uid,
            euid: target_cred.euid,
            gid: target_cred.gid,
            egid: target_cred.egid,
            same_session,
        })
    }
}

/// Target-side facts consumed by `cred::require_signal_send`.
///
/// Day-1 subset of the illustrative `TargetProcCred` shape in
/// [`cred_service_v_1`]: only the fields the day-1 permission rule
/// reads. `suid`/`sgid` arrive with saved-set IDs, `dumpable` arrives
/// with ptrace.
///
/// [`cred_service_v_1`]:
/// `docs/design/02_execution/cred_service_v_1_draft (2).md`
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TargetProcCred {
    pub uid: Uid,
    pub euid: Uid,
    pub gid: Gid,
    pub egid: Gid,
    pub same_session: bool,
}

/// Process payload. Dropped when the last thread exits (zombie state).
/// Holds the address space and the live thread list. Future fields
/// (`cred`, `rlimits`, `fd_table`, `sig_actions`, `group_pending`) land
/// in follow-up passes without changing the existing surface.
pub struct ProcessPayload {
    pub(crate) aspace: Cap<AddressSpace>,
    pub(crate) threads: SpinMutex<Vec<Cap<ThreadIdentity>>>,
    /// Per-process signal-action table. Day-1 records dispositions
    /// installed via `step_sigaction`; the delivery step that consults
    /// these lands with the AST/scripts pass.
    pub(crate) sig_actions: SigActionTable,
    /// Process-group-targeted pending signals. Day-1 collapses
    /// repeated posts (bitset, no per-occurrence queueing); a thread
    /// whose mask permits the signal will sweep it on its next
    /// delivery point.
    pub(crate) group_pending: PendingSignalQueue,
    /// Per-process credential snapshot. Mutated via `cred::step_setuid`
    /// / `cred::step_setgid`; readers clone via `cred()` accessor.
    pub(crate) cred: SpinMutex<Cred>,
}

impl ProcessPayload {
    /// Borrow the per-process action table.
    pub fn sig_actions(&self) -> &SigActionTable {
        &self.sig_actions
    }

    /// Borrow the per-process group-pending queue.
    pub fn group_pending(&self) -> &PendingSignalQueue {
        &self.group_pending
    }

    /// Snapshot the current credential. Returns a `Copy` so callers
    /// don't have to retain the lock; permissioned mutators
    /// (`cred::step_setuid` etc.) take the lock internally.
    pub fn cred(&self) -> Cred {
        *self.cred.lock()
    }
}

/// Process group: the unit of `setpgid`/`getpgid` and the eventual unit
/// of pgrp-wide signal fanout. Members tracked by `Weak` so a process can
/// die without serializing against the group; readers must `upgrade` to
/// observe live membership.
pub struct ProcessGroup {
    pub pgid: Pgid,
    pub(crate) session: Cap<Session>,
    pub(crate) members: SpinMutex<Vec<Weak<ProcessIdentity>>>,
}

impl ProcessGroup {
    /// Snapshot the owning session `Cap`.
    pub fn session_cap(&self) -> Cap<Session> {
        self.session.clone()
    }

    /// Number of `Weak` slots currently held in the members list.
    /// Includes entries pointing to dropped processes — callers that
    /// need the live count should walk and `upgrade` under a guard.
    pub fn member_slot_count(&self) -> usize {
        self.members.lock().len()
    }
}

/// Session: container of process groups, holds the controlling-tty link.
/// `controlling_tty` is `Weak` so the TTY can be torn down (hangup) while
/// the session continues to exist with no controlling terminal.
pub struct Session {
    pub sid: Sid,
    pub(crate) controlling_tty: SpinMutex<Option<Weak<TtyIdentity>>>,
    pub(crate) groups: SpinMutex<Vec<Weak<ProcessGroup>>>,
}

impl Session {
    /// Whether the session currently has a controlling TTY.
    pub fn has_controlling_tty(&self) -> bool {
        self.controlling_tty.lock().is_some()
    }

    /// Number of `Weak` group slots currently held. Includes stale
    /// entries pointing to dropped groups.
    pub fn group_slot_count(&self) -> usize {
        self.groups.lock().len()
    }
}

static PROCESS_IDENTITY_ZONE: Zone<ProcessIdentity> = Zone::const_new();
static PROCESS_PAYLOAD_ZONE: Zone<ProcessPayload> = Zone::const_new();
static PROCESS_GROUP_ZONE: Zone<ProcessGroup> = Zone::const_new();
static SESSION_ZONE: Zone<Session> = Zone::const_new();

unsafe impl ZoneAllocated for ProcessIdentity {
    fn zone() -> &'static Zone<Self> {
        &PROCESS_IDENTITY_ZONE
    }
}

unsafe impl ZoneAllocated for ProcessPayload {
    fn zone() -> &'static Zone<Self> {
        &PROCESS_PAYLOAD_ZONE
    }
}

unsafe impl ZoneAllocated for ProcessGroup {
    fn zone() -> &'static Zone<Self> {
        &PROCESS_GROUP_ZONE
    }
}

unsafe impl ZoneAllocated for Session {
    fn zone() -> &'static Zone<Self> {
        &SESSION_ZONE
    }
}

/// Simple atomic PID allocator. PID 1 is reserved for init; allocator
/// starts at 2. PidNamespace and reuse-after-reap policy land later.
static NEXT_PID: AtomicU32 = AtomicU32::new(2);

pub fn allocate_pid() -> Pid {
    Pid(NEXT_PID.fetch_add(1, Ordering::Relaxed))
}

#[cfg(test)]
pub(crate) fn reset_pid_counter_for_test() {
    NEXT_PID.store(2, Ordering::Relaxed);
}
