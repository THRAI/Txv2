//! Process subsystem execution: fork, exit-group, setpgid, setsid, and
//! the internal `step_zombie` helper.

use alloc::vec;
use alloc::vec::Vec;

use tx_hal::PmapIf;
use tx_substrate::zone::{self, Cap, ZoneError};

use crate::cred::Cred;
use crate::process::structure::{
    allocate_pid, Pgid, Pid, ProcessGroup, ProcessIdentity, ProcessPayload, Session, Sid,
};
use crate::signal::{PendingSignalQueue, SigActionTable};
use crate::sync::SpinMutex;
use crate::thread_runtime::execution::set_thread_zombie;
use crate::thread_runtime::structure::{allocate_tid, ThreadIdentity, ThreadPayload};
use crate::vm::{AddressSpace, VmMapError};

use core::sync::atomic::{AtomicU64, AtomicU8};

/// Errors from `step_fork`.
#[derive(Debug)]
pub enum ForkError {
    /// Parent process has no payload (zombie).
    ParentZombie,
    /// VM-side fork failed.
    Vm(VmMapError),
    /// Zone allocator could not satisfy the reservation.
    Zone(ZoneError),
}

impl From<VmMapError> for ForkError {
    fn from(e: VmMapError) -> Self {
        Self::Vm(e)
    }
}

impl From<ZoneError> for ForkError {
    fn from(e: ZoneError) -> Self {
        Self::Zone(e)
    }
}

/// Errors from `step_setpgid`.
#[derive(Debug)]
pub enum SetpgidError {
    /// Day-1 only supports `pgid == target.pid` (create new group).
    /// Joining an existing group requires session-walk, which is a
    /// follow-up.
    Unimplemented,
    Zone(ZoneError),
}

impl From<ZoneError> for SetpgidError {
    fn from(e: ZoneError) -> Self {
        Self::Zone(e)
    }
}

/// Errors from `step_setsid`.
#[derive(Debug)]
pub enum SetsidError {
    Zone(ZoneError),
}

impl From<ZoneError> for SetsidError {
    fn from(e: ZoneError) -> Self {
        Self::Zone(e)
    }
}

/// Bootstrap an init-style root process: pid=1, no parent, fresh
/// `Session` and `ProcessGroup`, supplied address space, single leader
/// thread. Used at boot and in tests.
pub fn bootstrap_init_process(
    aspace: Cap<AddressSpace>,
) -> Result<Cap<ProcessIdentity>, ZoneError> {
    let pid = Pid::INIT;
    let session = sign_session(Sid(pid.0))?;
    let pgrp = sign_process_group(Pgid(pid.0), session)?;

    let proc_cap = sign_process_identity(pid, Pid::RESERVED, pgrp.clone())?;

    pgrp.members.lock().push(proc_cap.downgrade());
    pgrp.session.groups.lock().push(pgrp.downgrade());

    let leader = sign_thread(proc_cap.downgrade())?;
    let payload = sign_process_payload(aspace, vec![leader], Cred::root())?;
    *proc_cap.payload.lock() = Some(payload);

    Ok(proc_cap)
}

/// Fork a process: clones the parent's address space, allocates a new
/// pid + leader tid, inherits the parent's pgrp/session, returns the
/// child identity.
pub fn step_fork<P: PmapIf>(
    parent: &Cap<ProcessIdentity>,
) -> Result<Cap<ProcessIdentity>, ForkError> {
    // Snapshot parent state under its payload lock.
    let (parent_aspace, parent_cred) = {
        let payload_guard = parent.payload.lock();
        let payload = payload_guard.as_ref().ok_or(ForkError::ParentZombie)?;
        (payload.aspace.clone(), payload.cred())
    };
    let parent_pgrp = parent.pgrp.lock().clone();

    // Fork the address space, then publish into the AddressSpace zone.
    let child_aspace = AddressSpace::fork_aspace::<P>(&parent_aspace)?;
    let aspace_res = zone::reserve_for::<AddressSpace>()?;
    let child_aspace_cap = zone::sign_for(aspace_res, child_aspace);

    // Identity first (payload=None) so the leader thread can hold a
    // Weak<ProcessIdentity> back-reference.
    let child_pid = allocate_pid();
    let child_proc = sign_process_identity(child_pid, parent.pid, parent_pgrp.clone())
        .map_err(ForkError::Zone)?;

    // Leader thread.
    let leader = sign_thread(child_proc.downgrade()).map_err(ForkError::Zone)?;

    // Wire up payload — child inherits parent credentials.
    let payload = sign_process_payload(child_aspace_cap, vec![leader], parent_cred)
        .map_err(ForkError::Zone)?;
    *child_proc.payload.lock() = Some(payload);

    // Register child in parent's pgrp.
    parent_pgrp.members.lock().push(child_proc.downgrade());

    Ok(child_proc)
}

/// Exit the entire thread group: zombify every thread, drop the
/// process payload, set the process exit status. Identity persists.
pub fn step_exit_group(process: &Cap<ProcessIdentity>, status: i32) {
    let mut payload_guard = process.payload.lock();
    if let Some(payload) = payload_guard.as_ref() {
        let drained: Vec<Cap<ThreadIdentity>> = core::mem::take(&mut *payload.threads.lock());
        for thread in &drained {
            set_thread_zombie(thread, status);
        }
        // `drained` drops here, releasing the strong refs on each thread.
    }
    *payload_guard = None;
    drop(payload_guard);
    *process.exit_status.lock() = Some(status);
}

/// Internal helper called by `thread_runtime::step_thread_exit` when
/// the last thread exits. Sets the process exit status and drops the
/// payload (zombification).
pub(crate) fn step_zombie(process: &Cap<ProcessIdentity>, status: i32) {
    *process.exit_status.lock() = Some(status);
    *process.payload.lock() = None;
}

/// Day-1 status convention for "killed by signal" exits. POSIX `wait(2)`
/// encodes this as `(sig & 0x7f)`; shells expose it as `128 + sig`.
/// Day-1 picks the shell convention so the value is unambiguously
/// "fatal signal X" without needing `WIFSIGNALED`/`WTERMSIG` decoders.
/// Migrates to Linux encoding when `wait(2)` lands and the encoder is
/// authoritative.
const fn signal_exit_status(sig: crate::signal::Signum) -> i32 {
    128 + sig.raw() as i32
}

/// Exit the entire thread group due to a fatal signal. Like
/// `step_exit_group` but additionally records the terminating signum
/// and encodes the exit status from it.
///
/// Materialises `route_sigkill`'s `invoke_group_exit_with_signal`
/// per `SIGNAL_v1` §12.3. Any caller of `route_gewalt(SIGKILL)`,
/// `ast_check`'s `DefaultTerminate` outcome, or a (future) fatal
/// synchronous-fault path goes through this to actually take the
/// process down.
pub fn step_exit_group_with_signal(process: &Cap<ProcessIdentity>, sig: crate::signal::Signum) {
    *process.terminating_signal.lock() = Some(sig);
    step_exit_group(process, signal_exit_status(sig));
}

/// Day-1 setpgid: only supports `new_pgid == target.pid`, which
/// creates a fresh process group inside the target's current session
/// and rebinds the target into it. Joining an existing group requires
/// walking the session for an existing pgid match — a follow-up.
pub fn step_setpgid(target: &Cap<ProcessIdentity>, new_pgid: Pgid) -> Result<(), SetpgidError> {
    if new_pgid.0 != target.pid.0 {
        return Err(SetpgidError::Unimplemented);
    }

    // Clone session out of the current pgrp; we'll keep the same
    // session and create a new pgrp inside it.
    let old_pgrp = target.pgrp.lock().clone();
    let session = old_pgrp.session.clone();

    let new_pgrp = sign_process_group(new_pgid, session)?;
    new_pgrp.session.groups.lock().push(new_pgrp.downgrade());
    new_pgrp.members.lock().push(target.downgrade());

    // Drop target from old pgrp.
    drop_member(&old_pgrp, target);

    *target.pgrp.lock() = new_pgrp;
    Ok(())
}

/// Day-1 setsid: creates a fresh `Session` + leader `ProcessGroup`
/// rooted at the target's pid, severs any controlling-tty link the
/// new session might have inherited (it can't have one yet), and
/// rebinds the target.
pub fn step_setsid(target: &Cap<ProcessIdentity>) -> Result<Sid, SetsidError> {
    let new_sid = Sid(target.pid.0);
    let new_pgid = Pgid(target.pid.0);

    let new_session = sign_session(new_sid)?;
    let new_pgrp = sign_process_group(new_pgid, new_session.clone())?;

    new_session.groups.lock().push(new_pgrp.downgrade());
    new_pgrp.members.lock().push(target.downgrade());

    let old_pgrp = target.pgrp.lock().clone();
    drop_member(&old_pgrp, target);

    *target.pgrp.lock() = new_pgrp;
    Ok(new_sid)
}

// --- internal sign-and-publish helpers ---

fn sign_session(sid: Sid) -> Result<Cap<Session>, ZoneError> {
    let res = zone::reserve_for::<Session>()?;
    Ok(zone::sign_for(
        res,
        Session {
            sid,
            controlling_tty: SpinMutex::new(None),
            groups: SpinMutex::new(Vec::new()),
        },
    ))
}

fn sign_process_group(pgid: Pgid, session: Cap<Session>) -> Result<Cap<ProcessGroup>, ZoneError> {
    let res = zone::reserve_for::<ProcessGroup>()?;
    Ok(zone::sign_for(
        res,
        ProcessGroup {
            pgid,
            session,
            members: SpinMutex::new(Vec::new()),
        },
    ))
}

fn sign_process_identity(
    pid: Pid,
    parent_pid: Pid,
    pgrp: Cap<ProcessGroup>,
) -> Result<Cap<ProcessIdentity>, ZoneError> {
    let res = zone::reserve_for::<ProcessIdentity>()?;
    Ok(zone::sign_for(
        res,
        ProcessIdentity {
            pid,
            parent_pid,
            pgrp: SpinMutex::new(pgrp),
            exit_status: SpinMutex::new(None),
            terminating_signal: SpinMutex::new(None),
            payload: SpinMutex::new(None),
        },
    ))
}

fn sign_process_payload(
    aspace: Cap<AddressSpace>,
    threads: Vec<Cap<ThreadIdentity>>,
    cred: Cred,
) -> Result<tx_substrate::zone::PayloadCap<ProcessPayload>, ZoneError> {
    let res = zone::reserve_for::<ProcessPayload>()?;
    let cap = zone::sign_for(
        res,
        ProcessPayload {
            aspace,
            threads: SpinMutex::new(threads),
            sig_actions: SigActionTable::new(),
            group_pending: PendingSignalQueue::new(),
            cred: SpinMutex::new(cred),
        },
    );
    Ok(tx_substrate::zone::PayloadCap::from_cap(cap))
}

fn sign_thread(
    owner_proc: tx_substrate::zone::Weak<ProcessIdentity>,
) -> Result<Cap<ThreadIdentity>, ZoneError> {
    let tid = allocate_tid();

    let payload_res = zone::reserve_for::<ThreadPayload>()?;
    let payload_cap = zone::sign_for(
        payload_res,
        ThreadPayload {
            task: SpinMutex::new(None),
            signal_mask: AtomicU64::new(0),
            thread_pending: PendingSignalQueue::new(),
            signal_summary: AtomicU8::new(0),
        },
    );
    let payload = tx_substrate::zone::PayloadCap::from_cap(payload_cap);

    let identity_res = zone::reserve_for::<ThreadIdentity>()?;
    let cap = zone::sign_for(
        identity_res,
        ThreadIdentity {
            tid,
            owner_proc,
            exit_status: SpinMutex::new(None),
            payload: SpinMutex::new(Some(payload)),
        },
    );
    Ok(cap)
}

fn drop_member(pgrp: &Cap<ProcessGroup>, target: &Cap<ProcessIdentity>) {
    let target_key = target.key();
    pgrp.members.lock().retain(|weak| {
        weak.observe_with_guard(|ident| ident.key() != target_key)
            .unwrap_or(true)
    });
}

// Helper trait alias — Weak observation under a guard is verbose; this
// wraps it in a closure.
trait WeakObserveExt<T: 'static> {
    fn observe_with_guard<F, R>(&self, f: F) -> Option<R>
    where
        F: FnOnce(tx_substrate::zone::IdentRef<'_, T>) -> R;
}

impl<T: 'static> WeakObserveExt<T> for tx_substrate::zone::Weak<T> {
    fn observe_with_guard<F, R>(&self, f: F) -> Option<R>
    where
        F: FnOnce(tx_substrate::zone::IdentRef<'_, T>) -> R,
    {
        let guard = tx_substrate::epoch::guard();
        self.observe(&guard).map(f)
    }
}
