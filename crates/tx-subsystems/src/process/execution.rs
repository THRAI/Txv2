//! Process subsystem execution: fork, exit-group, setpgid, setsid, and
//! the internal `step_process_exit` last-thread cascade.

use alloc::vec;
use alloc::vec::Vec;

use tx_hal::PmapIf;
use tx_substrate::zone::{self, Cap, ZoneError};

use crate::cred::Cred;
use crate::process::structure::{
    allocate_pid, ExitStatus, Pgid, Pid, ProcessGroup, ProcessIdentity, ProcessPayload, Session,
    Sid, FD_TABLE_SIZE,
};
use crate::signal::{PendingSignalQueue, SigActionTable};
use crate::sync::SpinMutex;
use crate::thread_runtime::execution::set_thread_zombie;
use crate::thread_runtime::structure::{allocate_tid, ThreadIdentity, ThreadPayload};
use crate::vfs::OpenFile;
use crate::vm::{AddressSpace, VmMapError};

/// Global init (`pid=1`) process handle. `None` until
/// `bootstrap_init_process` runs, after which it holds a strong `Cap`
/// retainer for the entire process lifetime. Per `PROCESS_v1` §8.1
/// (children reparenting), `sever_children` consults this on every
/// process exit to decide whether to reparent to init or sever-only.
///
/// The `SpinMutex<Option<Cap>>` shape matches every other day-1 slot
/// in the process subsystem; migration to a future
/// `tx_substrate::AtomicSlot<T>` is a subsystem-internal change.
static INIT_PROCESS: SpinMutex<Option<Cap<ProcessIdentity>>> = SpinMutex::new(None);

/// Snapshot the global init handle. Returns `None` before
/// `bootstrap_init_process` has run (test pre-bootstrap; boot-time
/// pre-process-init). The returned `Cap` is a clone — caller drops
/// freely without touching the global slot.
pub fn init_process() -> Option<Cap<ProcessIdentity>> {
    INIT_PROCESS.lock().clone()
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn reset_init_process_for_test() {
    *INIT_PROCESS.lock() = None;
}

/// Errors from `bootstrap_init_process`.
#[derive(Debug)]
pub enum BootstrapError {
    /// Zone allocator could not satisfy the reservation.
    Zone(ZoneError),
    /// `bootstrap_init_process` has already run; the `INIT_PROCESS`
    /// slot is occupied. Production boots once; tests must call
    /// `reset_init_process_for_test()` between runs.
    AlreadyBootstrapped,
}

impl From<ZoneError> for BootstrapError {
    fn from(e: ZoneError) -> Self {
        Self::Zone(e)
    }
}

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

/// `wait(2)` selector — full POSIX `pid` argument coverage.
///
/// POSIX maps the syscall's signed `pid_t` argument to:
/// - `pid > 0` → `Pid(p)` — exact pid match.
/// - `pid == 0` → `CallerPgrp` — any child in caller's pgroup.
/// - `pid == -1` → `Any` — any child.
/// - `pid < -1` → `Pgrp(Pgid(-pid))` — any child in pgroup `-pid`.
///
/// `CallerPgrp` is resolved to a concrete `Pgid` at the start of
/// `step_waitpid_nohang` (taken from the caller's `pgrp_cap`), so
/// the walk only ever sees `Pid` / `Pgrp` / `Any` selectors.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WaitTarget {
    /// `waitpid(-1, ...)` — any child.
    Any,
    /// `waitpid(pid > 0, ...)` — child with this exact pid.
    Pid(Pid),
    /// `waitpid(0, ...)` — any child in the caller's process group.
    CallerPgrp,
    /// `waitpid(-pgid, ...)` — any child whose pgrp matches this pgid.
    Pgrp(Pgid),
}

impl WaitTarget {
    fn matches(&self, child: &ProcessIdentity) -> bool {
        match self {
            WaitTarget::Any => true,
            WaitTarget::Pid(p) => child.pid == *p,
            WaitTarget::Pgrp(target_pgid) => child.pgrp_cap().pgid == *target_pgid,
            // `CallerPgrp` is resolved to `Pgrp(caller_pgid)` before
            // the walk; reaching it here is a programmer error.
            WaitTarget::CallerPgrp => false,
        }
    }
}

/// Failure modes for `step_waitpid_nohang`. POSIX-wise:
///
/// - `NoChildren` corresponds to `ECHILD` (no matching children at
///   all — caller has nothing to wait for).
/// - `NoneReady` corresponds to the WNOHANG "no zombie ready" return
///   (POSIX returns `0` rather than an error; the syscall driver
///   maps it).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WaitError {
    /// No matching children — caller has no live or zombie child
    /// satisfying `target` selector.
    NoChildren,
    /// Matching children exist but none are zombies right now.
    /// WNOHANG semantic — caller would block under POSIX `waitpid`.
    NoneReady,
}

/// Bootstrap an init-style root process: pid=1, no parent, fresh
/// `Session` and `ProcessGroup`, supplied address space, single leader
/// thread. Registers the resulting `Cap` in the global `INIT_PROCESS`
/// slot so `sever_children` (and future `step_process_exit`
/// orphan-routing) can resolve "the kernel's init process."
///
/// Production callers run this once from the kernel boot path; tests
/// reset between runs via `reset_init_process_for_test()`.
pub fn bootstrap_init_process(
    aspace: Cap<AddressSpace>,
) -> Result<Cap<ProcessIdentity>, BootstrapError> {
    // Reject second bootstrap before doing any allocation work.
    {
        let slot = INIT_PROCESS.lock();
        if slot.is_some() {
            return Err(BootstrapError::AlreadyBootstrapped);
        }
    }

    let pid = Pid::INIT;
    let session = sign_session(Sid(pid.0))?;
    let pgrp = sign_process_group(Pgid(pid.0), session)?;

    let proc_cap = sign_process_identity(pid, None, pgrp.clone())?;

    pgrp.members.lock().push(proc_cap.downgrade());
    pgrp.session.members.lock().push(pgrp.downgrade());

    let leader = sign_thread(proc_cap.downgrade())?;
    // Day-1: init has no cwd until a rootfs is mounted and an
    // initial chdir runs. Future EXEC_v1 / first-userspace lands
    // a synthesized "/" DEntry and threads it through here.
    // Bootstrap-time fd table is empty: the devfs `console` alias is
    // not yet registered when the kernel reaches process bootstrap.
    // Phase 3b's `init.rs` calls `tx_fs::devfs::open_console_for_init()`
    // *after* registering the console hardware and stuffs the result
    // into fds 0/1/2 via `payload.set_fd`.
    let payload = sign_process_payload(aspace, vec![leader], Cred::root(), None, empty_fd_table())?;
    *proc_cap.payload.lock() = Some(payload);

    // Register globally. The slot retains a strong Cap so init
    // outlives every other reference (matches POSIX init lifetime).
    *INIT_PROCESS.lock() = Some(proc_cap.clone());

    Ok(proc_cap)
}

/// Fork a process: clones the parent's address space, allocates a new
/// pid + leader tid, inherits the parent's pgrp/session, returns the
/// child identity.
pub fn step_fork<P: PmapIf>(
    parent: &Cap<ProcessIdentity>,
) -> Result<Cap<ProcessIdentity>, ForkError> {
    // Snapshot parent state under its payload lock. Fd table is
    // cloned slot-by-slot so parent and child share the same
    // `Cap<OpenFile>` per slot, matching the Trio plan §"Cross-cutting
    // risks #6" (full `dup`-shape sharing is deferred).
    let (parent_aspace, parent_cred, parent_cwd, parent_fds) = {
        let payload_guard = parent.payload.lock();
        let payload = payload_guard.as_ref().ok_or(ForkError::ParentZombie)?;
        (
            payload.aspace.clone(),
            payload.cred(),
            payload.cwd(),
            payload.snapshot_fds(),
        )
    };
    let parent_pgrp = parent.pgrp.lock().clone();

    // Fork the address space, then publish into the AddressSpace zone.
    let child_aspace = AddressSpace::fork_aspace::<P>(&parent_aspace)?;
    let aspace_res = zone::reserve_for::<AddressSpace>()?;
    let child_aspace_cap = zone::sign_for(aspace_res, child_aspace);

    // Identity first (payload=None) so the leader thread can hold a
    // Weak<ProcessIdentity> back-reference.
    let child_pid = allocate_pid();
    let child_proc =
        sign_process_identity(child_pid, Some(parent.downgrade()), parent_pgrp.clone())
            .map_err(ForkError::Zone)?;

    // Leader thread.
    let leader = sign_thread(child_proc.downgrade()).map_err(ForkError::Zone)?;

    // Wire up payload — child inherits parent credentials, cwd, and
    // a per-slot clone of the parent's fd table. POSIX: fork copies
    // the cwd reference (same DEntry); CLONE_FS (sharing) is a
    // Phase-2 concern.
    let payload = sign_process_payload(
        child_aspace_cap,
        vec![leader],
        parent_cred,
        parent_cwd,
        parent_fds,
    )
    .map_err(ForkError::Zone)?;
    *child_proc.payload.lock() = Some(payload);

    // Register child in parent's pgrp.
    parent_pgrp.members.lock().push(child_proc.downgrade());

    // Register child in parent's children list. Materialization of the
    // upward `parent` binding per `PROCESS_v1` §2.1. The list holds
    // strong `Cap` refs — children stay observable here until reaped
    // (§8.5).
    parent.children.lock().push(child_proc.clone());

    Ok(child_proc)
}

/// Exit the entire thread group: zombify every thread, drop the
/// process payload, set the process exit status. Identity persists.
///
/// Threads zombify with the `wait_status_word` projection of `status`
/// — day-1's `128 + sig` shell encoding for `Signaled`, the raw int
/// for `Exited` — preserving the "thread-side exit_status is an int"
/// shape that `THREAD_RUNTIME_v1` §7.2 carries.
pub fn step_exit_group(process: &Cap<ProcessIdentity>, status: ExitStatus) {
    session_leader_hangup_cascade(process);
    sever_children(process);

    let mut payload_guard = process.payload.lock();
    if let Some(payload) = payload_guard.as_ref() {
        let drained: Vec<Cap<ThreadIdentity>> = core::mem::take(&mut *payload.threads.lock());
        for thread in &drained {
            set_thread_zombie(thread, status.wait_status_word());
        }
        // `drained` drops here, releasing the strong refs on each thread.
    }
    *payload_guard = None;
    drop(payload_guard);
    *process.exit_status.lock() = Some(status);

    // §7.3.3 phase 5: notify the parent. Posted after zombification so
    // the parent observes a complete zombie when it acts on SIGCHLD.
    post_sigchld_to_parent(process);
}

/// Last-thread cascade: called by `thread_runtime::step_thread_exit`
/// when the final thread of a process exits. Materialises
/// `PROCESS_v1` §7.3.3 `step_process_exit`'s commit phase.
///
/// Day-1 scope: severs children's parent slots (§8.1 stub), writes
/// the process-visible exit status, drops the `ProcessPayload`. The
/// remaining §7.3.3 phase-5 cascade (SIGCHLD to parent, `exit_port`
/// wake, full reparenting-into-init, orphan-pgrp SIGHUP,
/// session-leader controlling-tty hangup per §8.3) lands when an
/// init handle is globally addressable and `exit_port` machinery
/// arrives.
pub(crate) fn step_process_exit(process: &Cap<ProcessIdentity>, status: ExitStatus) {
    session_leader_hangup_cascade(process);
    sever_children(process);
    *process.exit_status.lock() = Some(status);
    *process.payload.lock() = None;
    post_sigchld_to_parent(process);
}

/// Children reparenting per `PROCESS_v1` §8.1. Three cases:
///
/// 1. Init handle registered AND `init != process` — move each child
///    `Cap` from `process.children` into `init.children`, update each
///    child's `parent` slot to `Weak<init>`. Real reparent semantics.
///
/// 2. Init handle is the exiting process (init itself is dying) —
///    sever-only. No higher-level reaper exists; children become
///    orphans with `parent = None`.
///
/// 3. Init handle absent (test pre-bootstrap; pre-process-init at
///    boot) — sever-only. Same observable shape as case 2.
///
/// In every case the exiting process's `children` list is drained,
/// so post-sever the dying process owns no child Caps.
fn sever_children(process: &Cap<ProcessIdentity>) {
    let children: Vec<Cap<ProcessIdentity>> = core::mem::take(&mut *process.children.lock());

    let init = init_process();
    let target = init.as_ref().filter(|i| i.key() != process.key());

    if let Some(init) = target {
        let init_weak = init.downgrade();
        let mut init_children = init.children.lock();
        for child in children {
            *child.parent.lock() = Some(init_weak);
            init_children.push(child);
        }
    } else {
        for child in children {
            *child.parent.lock() = None;
        }
    }
}

/// Session-leader-death hangup cascade per `PROCESS_v1` §8.3.
///
/// Detects whether the exiting process is a session leader (`pid ==
/// session.sid`) and, if so, severs the session ↔ TTY relationship:
///
/// 1. Resolve the foreground pgrp via `session.foreground_pgrp_cap()`
///    — the two-hop weak dereference (`session.controlling_tty` →
///    `tty.session_pgrp.foreground_pgrp`). Either hop may return
///    `None` (TIOCNOTTY already ran; tty already reclaimed; tty has
///    no fg pgrp; pgrp reclaimed) — handled silently.
/// 2. Post `SIGHUP` to the foreground pgrp, then `SIGCONT` (POSIX
///    §11.1.3 requires SIGCONT to wake any stopped processes in the
///    fg pgrp so they can receive the SIGHUP). Posted via
///    `step_kill_pgrp` — SIGHUP routes through the catchable shim;
///    SIGCONT through the Gewalt path.
/// 3. Clear the tty's `session_pgrp` slot (authoritative side) —
///    subsequent readers see "no controlling session." Per OPA-3,
///    this is the slot that needs the explicit clear.
/// 4. Clear `session.controlling_tty` (the mirror).
///
/// Steps 3 and 4 run even if step 1's resolution returned `None` for
/// the fg pgrp — the session-tty linkage must be severed regardless.
/// If the session has no controlling tty at all (step 1 first hop
/// fails), the cascade is a no-op.
///
/// Atomicity note: the four substeps are class-3 compositional per
/// `BINDING_v1` — a concurrent reader on another hart can observe
/// the SIGHUP delivered while the tty's `session_pgrp` is still
/// set, or vice versa. POSIX doesn't constrain intermediate-state
/// visibility for session-leader death.
fn session_leader_hangup_cascade(process: &Cap<ProcessIdentity>) {
    let pgrp = process.pgrp_cap();
    let session = pgrp.session_cap();

    // Day-1 single-namespace: session leader is the process whose
    // pid matches the session's sid (set at setsid time).
    if process.pid.0 != session.sid.0 {
        return;
    }

    // Phase 1: resolve via Session::controlling_tty_cap (first hop).
    let Some(tty) = session.controlling_tty_cap() else {
        // No controlling tty — entire cascade is a no-op.
        return;
    };

    // Phase 1 (second hop): foreground pgrp.
    let fg_pgrp = tty.foreground_pgrp_cap();

    // Phase 2: SIGHUP + SIGCONT to fg pgrp, if both hops succeeded.
    if let Some(fg_pgrp) = fg_pgrp {
        let _ = crate::signal::step_kill_pgrp(&fg_pgrp, crate::signal::Signum::SIGHUP);
        let _ = crate::signal::step_kill_pgrp(&fg_pgrp, crate::signal::Signum::SIGCONT);
    }

    // Phase 3: clear tty's session_pgrp (authoritative).
    let _ = tty.clear_session_pgrp();

    // Phase 4: clear session.controlling_tty (mirror).
    *session.controlling_tty.lock() = None;
}

/// SIGCHLD producer for `PROCESS_v1` §7.3.3 phase 5. When a process
/// zombifies, its parent should receive `SIGCHLD` so it can `wait(2)`
/// the corpse. Day-1 scope: catchable-shim post (default action is
/// `Ignore`, so without a handler the bit just accumulates in the
/// parent's leader-thread `thread_pending` until reaped or masked).
///
/// No-op for processes with no parent: bootstrap init (never had one)
/// and orphans whose parent has already exited and severed them.
/// Also no-op if the parent is itself a zombie — `step_kill_process`
/// returns `NoLiveThread` and we discard.
///
/// `siginfo` is not yet wired (no `SigInfo` type in day-1 signal
/// surface); spec §7.3.3 phase 5 will populate `si_pid`, `si_uid`,
/// `si_code` (`CLD_EXITED` / `CLD_KILLED` / `CLD_DUMPED`), and
/// `si_status` when the siginfo carrier lands.
fn post_sigchld_to_parent(process: &Cap<ProcessIdentity>) {
    if let Some(parent) = process.parent_cap() {
        let _ = crate::signal::step_kill_process(&parent, crate::signal::Signum::SIGCHLD);
    }
}

/// Exit the entire thread group due to a fatal signal. Records
/// `ExitStatus::Signaled(sig)` and runs the same payload teardown as
/// `step_exit_group`.
///
/// Materialises `route_sigkill`'s `invoke_group_exit_with_signal`
/// per `SIGNAL_v1` §12.3. Any caller of `route_gewalt(SIGKILL)`,
/// `ast_check`'s `DefaultTerminate` outcome, or a (future) fatal
/// synchronous-fault path goes through this to actually take the
/// process down.
pub fn step_exit_group_with_signal(process: &Cap<ProcessIdentity>, sig: crate::signal::Signum) {
    step_exit_group(process, ExitStatus::Signaled(sig));
}

/// Synchronous `waitpid(2)` with WNOHANG semantics. Walks the
/// caller's children list, finds a zombie matching `target`, reaps
/// it (withdraws from `parent.children` and from the child's
/// `pgrp.members`), and returns `(pid, exit_status)`.
///
/// Materialises `PROCESS_v1` §7.4 `script_waitpid` minus the
/// reactor-blocking variant — the loop's "block until child state
/// changes" arm needs reactor channel integration that isn't wired
/// yet. Day-1 callers get the WNOHANG poll only.
///
/// Spec deferrals carried forward:
/// - No PidName withdrawal from `PidNamespace.numbers` (single-ns).
/// - No RLIMIT_NPROC release (no rlimits).
/// - No `siginfo` return (caller gets `ExitStatus`; the syscall
///   driver formats `WIFEXITED` / `WIFSIGNALED` from it).
/// - Selectors limited to `Any` / `Pid`; pgrp selectors are a small
///   filter-only follow-up.
pub fn step_waitpid_nohang(
    parent: &Cap<ProcessIdentity>,
    target: WaitTarget,
) -> Result<(Pid, ExitStatus), WaitError> {
    // Resolve `CallerPgrp` to a concrete `Pgrp(caller_pgid)` before
    // the walk so `WaitTarget::matches` only handles concrete
    // selectors. The caller's pgrp can change between syscalls, but
    // for a single waitpid call we snapshot once and use it for the
    // entire walk.
    let target = match target {
        WaitTarget::CallerPgrp => WaitTarget::Pgrp(parent.pgrp_cap().pgid),
        other => other,
    };

    // Phase 1: walk children once. Track whether any child matches the
    // selector at all (regardless of zombie state) so we can
    // distinguish ECHILD (no match) from NoneReady (match but live).
    let snapshot: Vec<Cap<ProcessIdentity>> = parent.children.lock().clone();

    let mut any_match = false;
    let mut reapable: Option<Cap<ProcessIdentity>> = None;
    for child in &snapshot {
        if !target.matches(child) {
            continue;
        }
        any_match = true;
        if child.is_zombie() {
            reapable = Some(child.clone());
            break;
        }
    }
    drop(snapshot);

    let Some(child) = reapable else {
        return Err(if any_match {
            WaitError::NoneReady
        } else {
            WaitError::NoChildren
        });
    };

    // Phase 2: reap. Read status, withdraw from parent.children and
    // the child's pgrp.members. After both withdrawals plus dropping
    // the local Cap, no strong retainer remains; the identity becomes
    // eligible for reclamation after epoch drain.
    let pid = child.pid;
    let status = child.exit_status().expect("zombie has exit_status");
    let key = child.key();

    parent.children.lock().retain(|c| c.key() != key);

    let pgrp = child.pgrp_cap();
    pgrp.members.lock().retain(|weak| {
        weak.observe_with_guard(|ident| ident.key() != key)
            .unwrap_or(true)
    });

    drop(child);

    Ok((pid, status))
}

/// Outcome of `step_chdir`. `Replaced` is the normal path, carrying
/// the previous cwd `Cap` (if any) so callers can release retention
/// or audit. `ZombieIgnored` means the target had no payload —
/// chdir on a zombie is a no-op.
#[derive(Clone, Debug)]
pub enum ChdirOutcome {
    Replaced {
        prev: Option<Cap<crate::vfs::DEntry>>,
    },
    ZombieIgnored,
}

/// Replace `target.cwd` with `new_cwd`. The new dentry's identity is
/// retained by the process payload until the next chdir or process
/// exit. Returns `Replaced { prev }` on success; `prev` is the
/// outgoing cwd `Cap` (`None` for processes that had no cwd yet).
///
/// Path resolution is the syscall-driver's job: this step takes a
/// pre-resolved `Cap<DEntry>`. POSIX `chdir(2)` / `fchdir(2)` and
/// the `EACCES` / `ENOENT` resolution errors live above this layer.
pub fn step_chdir(target: &Cap<ProcessIdentity>, new_cwd: Cap<crate::vfs::DEntry>) -> ChdirOutcome {
    let payload_guard = target.payload.lock();
    let Some(payload) = payload_guard.as_ref() else {
        return ChdirOutcome::ZombieIgnored;
    };
    let prev = payload.cwd.lock().replace(new_cwd);
    ChdirOutcome::Replaced { prev }
}

/// Render `target.cwd` as an absolute POSIX path. Returns the bytes
/// produced by walking the cwd dentry's `parent_hint` chain to the
/// root (per `vfs::render_dentry_path`).
///
/// Returns `None` if:
/// - `target` is a zombie (no payload).
/// - `target.cwd` is unset (init pre-rootfs, or never `step_chdir`'d).
/// - The dentry chain is broken (a `parent_hint` Weak failed to upgrade).
///   POSIX maps this to `ENOENT` ("the cwd has been unlinked"); the
///   syscall driver applies the errno.
pub fn step_getcwd(target: &Cap<ProcessIdentity>) -> Option<alloc::vec::Vec<u8>> {
    let payload_guard = target.payload.lock();
    let payload = payload_guard.as_ref()?;
    let cwd = payload.cwd.lock().clone()?;
    drop(payload_guard);
    crate::vfs::render_dentry_path(&cwd)
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
    new_pgrp.session.members.lock().push(new_pgrp.downgrade());
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

    new_session.members.lock().push(new_pgrp.downgrade());
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
            members: SpinMutex::new(Vec::new()),
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
    parent: Option<tx_substrate::zone::Weak<ProcessIdentity>>,
    pgrp: Cap<ProcessGroup>,
) -> Result<Cap<ProcessIdentity>, ZoneError> {
    let res = zone::reserve_for::<ProcessIdentity>()?;
    Ok(zone::sign_for(
        res,
        ProcessIdentity {
            pid,
            parent: SpinMutex::new(parent),
            children: SpinMutex::new(Vec::new()),
            pgrp: SpinMutex::new(pgrp),
            exit_status: SpinMutex::new(None),
            payload: SpinMutex::new(None),
        },
    ))
}

fn sign_process_payload(
    aspace: Cap<AddressSpace>,
    threads: Vec<Cap<ThreadIdentity>>,
    cred: Cred,
    cwd: Option<Cap<crate::vfs::DEntry>>,
    fds: [Option<Cap<OpenFile>>; FD_TABLE_SIZE],
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
            cwd: SpinMutex::new(cwd),
            fds: SpinMutex::new(fds),
        },
    );
    Ok(tx_substrate::zone::PayloadCap::from_cap(cap))
}

/// All-`None` initial fd table for processes that have no preopens at
/// payload-construction time (bootstrap init pre-Phase-3b; tests).
fn empty_fd_table() -> [Option<Cap<OpenFile>>; FD_TABLE_SIZE] {
    core::array::from_fn(|_| None)
}

fn sign_thread(
    owner_proc: tx_substrate::zone::Weak<ProcessIdentity>,
) -> Result<Cap<ThreadIdentity>, ZoneError> {
    let tid = allocate_tid();

    let payload_res = zone::reserve_for::<ThreadPayload>()?;
    let payload_cap = zone::sign_for(payload_res, ThreadPayload::fresh());
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
