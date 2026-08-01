//! Process subsystem execution: fork, exit-group, setpgid, setsid, and
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::sync::{Arc, Weak as ArcWeak};
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use tx_hal::{PmapIf, UserTrapContext};

use crate::cred::Cred;
use crate::page_backed::PageContainer;
use crate::process::adapter::step_engine::{
    self, process_spin_mutex, Cap, IdentRef, NoProgress, OneShotStepOp, OperationalCapExt,
    PayloadCap, ProcessSpinMutex, ScriptCtx, SpinMutex, StepOp, StepOutcome, SubjectIdentity, Weak,
    YieldShape, ZoneError,
};
use crate::process::adapter::wait_routing::{MailboxEvent, TaskMailbox};
use crate::process::structure::{
    CwdBinding, ExitStatus, Frame, Pgid, Pid, ProcessCwdState, ProcessGroup, ProcessIdentity,
    ProcessPayload, Session, Sid,
};
use crate::process::topology::{
    ProcessChildren, ProcessGroupMembers, ProcessThreads, SessionMembers,
};
use crate::signal::{
    sync_thread_group_pending_summary, PendingSignalQueue, SigActionTable, SignalMask,
};
use crate::thread_runtime::execution::post_signal_mailbox_with_post;
use crate::thread_runtime::structure::{allocate_tid, ThreadIdentity, ThreadPayload, Tid};
use crate::vfs::OpenFile;
use crate::vm::{AddressSpace, VmMapError};

pub(crate) const PROCESS_LOCK_SERVICE_TRACE_NAMES: &[&[u8]] = &[
    b"debug.lock_service.process.payload.exit_group.shm_detach.duration_ns",
    b"debug.lock_service.process.payload.exit_group.drain_fds.duration_ns",
    b"debug.lock_service.process.payload.exit_group.threads_drain.duration_ns",
    b"debug.lock_service.process.payload.exit_group.zombify_threads.duration_ns",
    b"debug.lock_service.process.payload.exit_group.drop_drained.duration_ns",
    b"debug.lock_service.process.payload.exit_group.payload_drop.duration_ns",
    b"debug.lock_service.process.payload.process_exit.shm_detach.duration_ns",
    b"debug.lock_service.process.payload.process_exit.drain_fds.duration_ns",
    b"debug.lock_service.process.payload.process_exit.drop_closed_fds.duration_ns",
    b"debug.lock_service.process.payload.process_exit.payload_drop.duration_ns",
    b"debug.lock_service.process.payload.thread_exit.threads_detach.duration_ns",
    b"debug.lock_service.process.payload.thread_exit.thread_count.duration_ns",
    b"debug.lock_service.process.payload.thread_exit.group_exit.duration_ns",
    b"debug.lock_service.process.payload.robust.head_reads.duration_ns",
    b"debug.lock_service.process.payload.robust.entries.duration_ns",
    b"debug.lock_service.process.payload.robust.pending.duration_ns",
    b"debug.lock_service.process.payload.robust.entry_count",
];

/// Dirty file containers retained after a close-time synchronous flush could
/// not finish.  Descriptor removal is already committed at that point, so the
/// cache object itself must stay alive until a later close/exit retries it.
/// The lock only protects this bounded-work retry list and is never held while
/// filesystem or block-I/O code runs.
static DEFERRED_PAGE_WRITEBACKS: SpinMutex<Vec<Cap<PageContainer>>> = SpinMutex::new(Vec::new());

#[inline(always)]
pub(crate) fn measure_process_lock_service<R>(name: &'static [u8], f: impl FnOnce() -> R) -> R {
    let known_names = PROCESS_LOCK_SERVICE_TRACE_NAMES;
    debug_assert!(known_names.contains(&name));
    #[cfg(tx_lock_metrics_process)]
    {
        let start = tx_observe::clock_now_ns();
        let result = f();
        let duration = tx_observe::clock_now_ns().saturating_sub(start);
        emit_process_lock_service_trace(name, duration.min(i64::MAX as u64) as i64);
        result
    }
    #[cfg(not(tx_lock_metrics_process))]
    {
        f()
    }
}

#[inline(always)]
pub(crate) fn emit_process_lock_service_trace(name: &'static [u8], value: i64) {
    let known_names = PROCESS_LOCK_SERVICE_TRACE_NAMES;
    debug_assert!(known_names.contains(&name));
    #[cfg(tx_lock_metrics_process)]
    {
        if let Some(observer) = tx_observe::current() {
            observer.debug_counter(name, value);
        }
    }
    #[cfg(not(tx_lock_metrics_process))]
    {
        let _ = value;
    }
}

// ---------------------------------------------------------------------------
// PID namespacing — delegates to `crate::process::numbers`
// ---------------------------------------------------------------------------

use crate::process::numbers::{
    allocate_pid, register_pgrp, register_pid as ns_register_pid, register_session, register_tid,
    resolve_pid_number_as, unregister_pid_number, with_namespace, PidName, PidNameKind,
};

/// Register a process pid → Cap binding. The Cap must be fully
/// constructed before this call (call it AFTER `step_engine::sign`).
pub(crate) fn register_pid(pid: Pid, cap: Cap<ProcessIdentity>) {
    ns_register_pid(pid, cap);
}

/// Remove a pid/tid from the namespace.
pub(crate) fn unregister_pid(pid: Pid) {
    unregister_pid_number(pid.0 as u64);
}

/// Look up a process by PID.
pub fn process_by_pid(pid: Pid) -> Option<Cap<ProcessIdentity>> {
    match resolve_pid_number_as(pid.0 as u64, PidNameKind::Process) {
        Some(PidName::Process(cap)) => Some(cap),
        _ => None,
    }
}

/// Look up a process group by PGID. Restored alongside the net subsystem
/// re-home; used by signal delivery to a process group (`kill(-pgid, ...)`).
pub fn process_group_by_pgid(pgid: Pgid) -> Option<Cap<ProcessGroup>> {
    match resolve_pid_number_as(pgid.0 as u64, PidNameKind::ProcessGroup) {
        Some(PidName::ProcessGroup(cap)) => Some(cap),
        _ => None,
    }
}

/// Return all registered PIDs with alive status (for procfs).
pub fn all_pids() -> alloc::vec::Vec<(Pid, bool)> {
    let mut out = alloc::vec::Vec::new();
    with_namespace(|ns| {
        for (&(num, _), name) in ns.iter() {
            if matches!(name.kind(), PidNameKind::Process) {
                out.push((Pid(num as u32), true));
            }
        }
    });
    out
}

#[cfg(any(test, feature = "test-support"))]
#[allow(dead_code)] // txdoc:vfs-full-bringup-scaffold
pub(crate) fn reset_pid_counter_for_test() {
    crate::process::numbers::reset_pid_counter_for_test();
}

/// Bootstrap value for the program-break base, per the Trio plan
/// §"Cross-cutting risks #7". Used by `bootstrap_init_process` to
/// seed init's brk region before the ELF loader lands; the syscall
/// dispatcher's `brk(2)` arm then operates against this base.
///
/// TODO(phase-elf-loader): replace with binary-derived value once
/// `step_exec` materialises an image.
pub const BOOTSTRAP_BRK_BASE: u64 = 0x6000_0000;

/// Global init (`pid=1`) process handle. `None` until
/// `bootstrap_init_process` runs, after which it holds a strong `Cap`
/// retainer for the entire process lifetime. Per `PROCESS_v1` §8.1
/// (children reparenting), `sever_children` consults this on every
/// process exit to decide whether to reparent to init or sever-only.
///
/// The `SpinMutex<Option<Cap>>` shape matches every other day-1 slot
/// in the process subsystem; migration to a future
/// `adapter::step_engine::AtomicSlot<T>` is a subsystem-internal change.
static INIT_PROCESS: ProcessSpinMutex<Option<Cap<ProcessIdentity>>> =
    process_spin_mutex(None, b"debug.lock.process.init_process");

/// Snapshot the global init handle. Returns `None` before
/// `bootstrap_init_process` has run (test pre-bootstrap; boot-time
/// pre-process-init). The returned `Cap` is a clone — caller drops
/// freely without touching the global slot.
pub fn init_process() -> Option<Cap<ProcessIdentity>> {
    INIT_PROCESS.lock().clone()
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn reset_init_process_for_test() {
    if let Some(prev) = INIT_PROCESS.lock().take() {
        unregister_pid(prev.pid);
    }
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
    /// Transient failure — resource contention (GroupExit in progress,
    /// clone-thread during shutdown, etc.)
    Busy,
    /// Tid allocation or pid-namespace registration failed.
    PidNamespace,
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
    /// Target is no longer operational.
    Zombie,
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
    /// POSIX: process-group leaders cannot create a new session.
    ProcessGroupLeader,
    /// Target is no longer operational.
    Zombie,
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
    let pgrp = sign_process_group(Pgid(pid.0), session.clone())?;

    let proc_cap = sign_process_identity(pid, None, pgrp.clone())?;

    pgrp.members.attach(proc_cap.downgrade());
    pgrp.session.members.attach(pgrp.downgrade());

    let leader = sign_thread(proc_cap.downgrade(), Tid(pid.0))?;
    // Day-1: init has no cwd until a rootfs is mounted and an
    // initial chdir runs. Future EXEC_v1 / first-userspace lands
    // a synthesized "/" DEntry and threads it through here.
    // Bootstrap-time fd table is empty: the devfs `console` alias is
    // not yet registered when the kernel reaches process bootstrap.
    // Phase 3b's `init.rs` calls `tx_fs::devfs::open_console_for_init()`
    // *after* registering the console hardware and stuffs the result
    // into fds 0/1/2 via `payload.set_fd`.
    //
    // brk: Trio plan §"Cross-cutting risks #7" pins a temporary
    // bootstrap base of `0x6000_0000` until the ELF loader lands and
    // can derive the real `brk_base` from the executable's `_end`
    // symbol (or `PT_LOAD` segment max). `current_brk == brk_base`
    // at exec time per VM_v1_2 §5.8.
    // TODO(phase-elf-loader): replace bootstrap brk_base with
    // binary-derived value once `step_exec` materialises an image.
    // Bootstrap-time CLOEXEC bitmap is `0`: init's stdio (fds 0/1/2,
    // installed by Phase 3b's `init.rs::bind_init_cwd_and_root`) is
    // NOT close-on-exec by Linux convention. Per the Wave 2 plan,
    // until `sys_open` exists in the trio's syscall surface, the
    // bitmap is mutated only by `fcntl(F_SETFD)`.
    // Create the init namespace proxy. Day-1: all namespace caps
    // point at init-namespace stubs; mnt_ns is deferred.
    let nsproxy = crate::process::nsproxy::sign_init_nsproxy()?;
    let init_net_namespace =
        crate::net::initial_net_namespace_payload_with_owner(nsproxy.user_ns.clone());
    let payload = sign_process_payload(
        aspace,
        vec![leader.clone()],
        nsproxy,
        Cred::root(),
        None,
        BTreeMap::new(),
        BTreeSet::new(),
        (1024, 4096),
        (u64::MAX, u64::MAX),
        init_net_namespace,
        BOOTSTRAP_BRK_BASE,
        BOOTSTRAP_BRK_BASE,
        // Slice 6 of the shell-prompt roadmap. init's file-creation
        // mask defaults to `0o022` per Linux convention; children
        // inherit through `step_fork`'s umask thread-through.
        0o022,
        0,
        Arc::new(SigActionTable::new()),
    )?;
    *proc_cap.payload.lock() = Some(payload);

    // Register globally. The slot retains a strong Cap so init
    // outlives every other reference (matches POSIX init lifetime).
    *INIT_PROCESS.lock() = Some(proc_cap.clone());

    // Register init in the pid namespace so `process_by_pid(Pid::INIT)`
    // resolves: kill/tkill/tgkill, /proc/<pid>/, waitpid, and the
    // SIGCHLD reparenting path all use this lookup. `step_fork`
    // registers child pids; init has no fork parent, so the bootstrap
    // path must register itself.
    register_pid(pid, proc_cap.clone());
    register_tid(Tid(pid.0), leader);
    register_pgrp(pgrp.pgid, pgrp.clone());
    register_session(session.sid, session);

    Ok(proc_cap)
}

/// Clone a new thread into an existing process (CLONE_THREAD).
/// Fork a process: clones the parent's address space, allocates a new
/// pid + leader tid, inherits the parent's pgrp/session, returns the
/// child identity.
pub fn step_fork<P: PmapIf>(
    parent: &Cap<ProcessIdentity>,
    clone_vm: bool,
    clone_sighand: bool,
) -> Result<Cap<ProcessIdentity>, ForkError> {
    // observe: inspect current subsystem state and validate inputs.
    // upgrade: acquire capabilities/guards needed for mutation.
    // reserve: reserve namespace, memory, or wait-source effects.
    // commit: apply the state transition.
    // publish: emit readiness, signal, or observable outcome.
    step_fork_with_options::<P>(
        parent,
        ForkOptions {
            clone_vm,
            clone_sighand,
            clone_newipc: false,
            clone_newnet: false,
            clone_newns: false,
        },
    )
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ForkOptions {
    pub clone_vm: bool,
    pub clone_sighand: bool,
    pub clone_newipc: bool,
    /// `CLONE_NEWNET` — give the child a fresh, isolated network namespace
    /// instead of inheriting the parent's. Used by `tst_ns_create net,mnt`.
    pub clone_newnet: bool,
    /// `CLONE_NEWNS` — accepted so `clone(CLONE_NEWNS)` succeeds; full mount-
    /// namespace isolation is deferred (child currently shares parent mnt_ns).
    pub clone_newns: bool,
}

pub fn step_fork_with_options<P: PmapIf>(
    parent: &Cap<ProcessIdentity>,
    options: ForkOptions,
) -> Result<Cap<ProcessIdentity>, ForkError> {
    step_fork_with_prepared_aspace::<P>(parent, options, None)
}

/// Wait-capable process-fork path used by Linux clone/fork syscalls.
///
/// Only the detached address-space preparation may suspend. PID allocation,
/// fd-reference accounting, and parent/child publication remain in the
/// one-shot commit below, so a RangeLock retry cannot publish a partial child.
pub async fn fork_with_options_wait<P: PmapIf>(
    parent: &Cap<ProcessIdentity>,
    options: ForkOptions,
) -> Result<Cap<ProcessIdentity>, ForkError> {
    if options.clone_vm {
        return step_fork_with_prepared_aspace::<P>(parent, options, None);
    }

    loop {
        let parent_aspace = {
            let payload_guard = parent.payload.lock();
            let payload = payload_guard.as_ref().ok_or(ForkError::ParentZombie)?;
            payload.aspace_cap()
        };
        let child_aspace = AddressSpace::fork_aspace_wait::<P>(&parent_aspace).await?;
        let child_aspace = step_engine::sign(child_aspace)?;

        match step_fork_with_prepared_aspace::<P>(
            parent,
            options,
            Some((parent_aspace, child_aspace)),
        ) {
            // Exec replaced the authoritative parent aspace while the
            // detached child clone was being prepared. Nothing was published,
            // so discard it and restart from the new parent state.
            Err(ForkError::Busy) => continue,
            other => return other,
        }
    }
}

fn step_fork_with_prepared_aspace<P: PmapIf>(
    parent: &Cap<ProcessIdentity>,
    options: ForkOptions,
    prepared_aspace: Option<(Cap<AddressSpace>, Cap<AddressSpace>)>,
) -> Result<Cap<ProcessIdentity>, ForkError> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    // Snapshot parent state under its payload lock. Fd table is
    // cloned entry-by-entry so parent and child share the same
    // `Cap<OpenFile>` per fd, matching the Trio plan §"Cross-cutting
    // risks #6" (full `dup`-shape sharing — separate file description
    // per fd — is deferred). The CLOEXEC set is cloned wholesale so
    // the child inherits parent's exec-time bits per Linux semantics.
    // brk values are cloned per the Trio plan §"Cross-cutting risks
    // #7": each child gets its own brk_base/current_brk pair, while
    // the underlying VM mappings are cloned through
    // `AddressSpace::fork_aspace` below.
    let (
        parent_aspace,
        parent_nsproxy,
        parent_cred,
        parent_cwd,
        parent_fds,
        parent_fd_cloexec,
        parent_rlimit_nofile,
        parent_rlimit_memlock,
        parent_net_namespace,
        parent_brk_base,
        parent_current_brk,
        parent_umask,
        parent_personality,
    ) = {
        let payload_guard = parent.payload.lock();
        let payload = payload_guard.as_ref().ok_or(ForkError::ParentZombie)?;
        let parent_aspace = payload.aspace_cap();
        if prepared_aspace
            .as_ref()
            .is_some_and(|(expected_parent, _)| expected_parent != &parent_aspace)
        {
            return Err(ForkError::Busy);
        }
        let (parent_fds, parent_fd_cloexec) = payload.clone_fd_state_for_fork();
        (
            parent_aspace,
            payload.nsproxy_cap(),
            payload.cred(),
            payload.cwd_state(),
            parent_fds,
            parent_fd_cloexec,
            payload.rlimit_nofile(),
            payload.rlimit_memlock(),
            payload.net_namespace(),
            payload.brk_base(),
            payload.current_brk(),
            payload.umask(),
            payload.personality(),
        )
    };
    let parent_pgrp = parent.pgrp.lock().clone();

    // Signal-action table: share via Arc for CLONE_SIGHAND; otherwise
    // fork snapshots the parent's dispositions.
    let child_sig_actions = if options.clone_sighand {
        let payload_guard = parent.payload.lock();
        let payload = payload_guard.as_ref().ok_or(ForkError::ParentZombie)?;
        Arc::clone(&payload.frame.sig_actions)
    } else {
        let payload_guard = parent.payload.lock();
        let payload = payload_guard.as_ref().ok_or(ForkError::ParentZombie)?;
        Arc::new((*payload.frame.sig_actions).clone())
    };

    // Address space: fork (CoW clone) or share (CLONE_VM).
    let child_aspace_cap = if options.clone_vm {
        parent_aspace.clone()
    } else if let Some((_expected_parent, child_aspace)) = prepared_aspace {
        child_aspace
    } else {
        let child_aspace = AddressSpace::fork_aspace::<P>(&parent_aspace)?;
        step_engine::sign(child_aspace)?
    };

    // Identity first (payload=None) so the leader thread can hold a
    // Weak<ProcessIdentity> back-reference.
    let child_pid = allocate_pid();
    let child_proc =
        sign_process_identity(child_pid, Some(parent.downgrade()), parent_pgrp.clone())
            .map_err(ForkError::Zone)?;

    // Leader thread.
    let leader = sign_thread(child_proc.downgrade(), Tid(child_pid.0)).map_err(ForkError::Zone)?;

    // Wire up payload — child inherits parent credentials, cwd, and
    // a per-slot clone of the parent's fd table. POSIX: fork copies
    // the cwd reference (same DEntry); CLONE_FS (sharing) is a
    // Phase-2 concern. Per Linux semantics CLOEXEC is per-fd and
    // copied across fork — the child sees the parent's snapshot at
    // fork time; subsequent `fcntl(F_SETFD)` calls in either parent
    // or child do not affect the other.
    // Clone the parent's nsproxy. POSIX: fork inherits the parent's
    // namespace bundle. CLONE_NEWIPC replaces only the IPC namespace.
    // CLONE_NEWNS publishes a fresh mount namespace that starts with the
    // parent's mount table snapshot, matching Linux's copy-on-unshare shape.
    let mut child_nsproxy =
        crate::process::nsproxy::clone_nsproxy_for_fork(&parent_nsproxy, options.clone_newipc)
            .map_err(ForkError::Zone)?;
    if options.clone_newns {
        let parent_mnt_ns = parent_nsproxy
            .mnt_ns
            .as_ref()
            .ok_or(ForkError::Zone(ZoneError::InvalidState))?;
        let child_mnt_ns = parent_mnt_ns.clone_ns().map_err(ForkError::Zone)?;
        child_nsproxy = crate::process::nsproxy::clone_nsproxy_with_mount_namespace(
            &child_nsproxy,
            child_mnt_ns,
        )
        .map_err(ForkError::Zone)?;
    }
    // CLONE_NEWNET: publish a fresh isolated network namespace for the child
    // (owned by the parent's user namespace) instead of inheriting the parent's.
    // Required by LTP's `tst_ns_create net,mnt` (the shell net command harness).
    let child_net_namespace = if options.clone_newnet {
        let namespace = crate::net::create_isolated_net_namespace_with_owner(
            "clone",
            Some(parent_nsproxy.user_ns.clone()),
        )
        .map_err(ForkError::Zone)?;
        namespace
            .payload_cap()
            .ok_or(ForkError::Zone(ZoneError::InvalidState))?
    } else {
        parent_net_namespace
    };
    let payload = sign_process_payload(
        child_aspace_cap,
        vec![leader],
        child_nsproxy,
        parent_cred,
        parent_cwd,
        parent_fds,
        parent_fd_cloexec,
        parent_rlimit_nofile,
        parent_rlimit_memlock,
        child_net_namespace,
        parent_brk_base,
        parent_current_brk,
        parent_umask,
        parent_personality,
        child_sig_actions,
    )
    .map_err(ForkError::Zone)?;
    *child_proc.payload.lock() = Some(payload);

    register_pid(child_pid, child_proc.clone());

    // Register child in parent's pgrp.
    parent_pgrp.members.attach(child_proc.downgrade());

    // Register child in parent's children list. Materialization of the
    // upward `parent` binding per `PROCESS_v1` §2.1. The list holds
    // strong `Cap` refs — children stay observable here until reaped
    // (§8.5).
    parent.children.attach(child_proc.clone());

    Ok(child_proc)
}

/// Publish a concrete mount namespace into a process's namespace bundle.
///
/// Boot uses this after rootfs mount creation because the initial process is
/// constructed before any `MountNamespace` exists in the current ordering.
/// The operation follows the immutable-NsProxy rule: clone the existing bundle
/// with the new mount namespace cap, then atomically replace the payload slot.
pub fn step_set_mount_namespace(
    process: &Cap<ProcessIdentity>,
    mnt_ns: Cap<crate::mount::MountNamespace>,
) -> Result<(), ForkError> {
    // observe: inspect current subsystem state and validate inputs.
    // upgrade: acquire capabilities/guards needed for mutation.
    // reserve: reserve namespace, memory, or wait-source effects.
    // commit: apply the state transition.
    // publish: emit readiness, signal, or observable outcome.
    let payload_guard = process.payload.lock();
    let payload = payload_guard.as_ref().ok_or(ForkError::ParentZombie)?;
    let current = payload.nsproxy_cap();
    let replacement = crate::process::nsproxy::clone_nsproxy_with_mount_namespace(&current, mnt_ns)
        .map_err(ForkError::Zone)?;
    let _old = payload.replace_nsproxy(replacement);
    Ok(())
}

/// Seed the child leader thread's `saved_user_context` from the
/// parent's snapshot at clone time.
///
/// Linux fork/clone ABI: the child returns from the clone syscall
/// with the parent's GPRs *except* `a0 = 0`, and resumes at the
/// instruction *after* the trapping syscall instruction:
///
/// - `a0` lives in an architecture-specific GPR slot (`regs[10]` on
///   RV64, `regs[4]` on LoongArch64).
/// - the syscall instruction is exactly 4 bytes on the supported
///   ports, and `hand_off_syscall` has already advanced the saved pc
///   by that width before this helper runs.
///
/// The parent thread is intentionally **not** modified here: the
/// syscall arm encodes the child's pid into the parent's
/// `pending_syscall_return` separately, and the trap-shell writeback
/// discipline drains that into the parent's fresh trap frame's `a0`
/// before re-entry per `txdoc:THREAD-5-4-THE-TWO-SITE-DISCIPLINE`.
///
/// Behaviour:
///
/// 1. Clone `parent_user_ctx` into a fresh `UserTrapContext`.
/// 2. Overwrite the child-return register (`a0`) with 0.
/// 3. If the clone syscall supplied a non-zero child stack, overwrite
///    the architecture-specific stack-pointer register with it.
/// 4. `store_saved_user_context(Some(child_ctx))` on the child
///    thread's payload.
///
/// `payload_cap()` returning `None` is a kernel-invariant
/// violation: a fresh child thread minted by [`step_fork`] is
/// always live-with-payload at the moment the syscall arm calls
/// this helper. We panic loudly with a stable sentinel string
/// (matches the precedent set by the ELF loader's
/// `:bootstrap-exec:fail`).
///
/// Sibling helper, not a method on [`ProcessIdentity`]: keeps the
/// ABI knowledge (the syscall skip and the `a0` register index)
/// local to a single grep target so a future ARM64 / x86_64 port
/// has one place to extract per-arch constants.
///
/// Cites:
/// - `txdoc:PROCESS-FORK-FAMILY` /
///   `txdoc:PROCESS-STEP-CLONE-PROCESS-NEW-PROCESS-PATH-CLONE-THREAD-1`
///   (`docs/design/04_process-signals/PROCESS_v1.md` §7.1).
/// - `txdoc:THREAD-5-1-STATE-PLACEMENT`
///   (`docs/design/02_execution/THREAD_RUNTIME_v1.md`) — the
///   child's `saved_user_context` lives on the child's leader
///   thread payload.
#[cfg(target_arch = "loongarch64")]
const CLONE_CHILD_RETURN_REG_INDEX: usize = 4;
#[cfg(not(target_arch = "loongarch64"))]
const CLONE_CHILD_RETURN_REG_INDEX: usize = 10;

/// Register index for the thread pointer (`tp` on RV64 = x4,
/// r2 on LoongArch64). Used to seed the child's TLS pointer when
/// `clone()` passes `CLONE_SETTLS`.
#[cfg(target_arch = "loongarch64")]
const TLS_REG_INDEX: usize = 2;
#[cfg(not(target_arch = "loongarch64"))]
const TLS_REG_INDEX: usize = 4;

/// Register index for the stack pointer (`sp` on RV64 = x2,
/// r3 on LoongArch64). Used to seed the child's sp when the
/// userspace `clone(2)` `newsp` argument is non-zero (libc-style
/// `clone(fn, stack, flags, arg, ...)` wraps push the fn/arg pair
/// on the new stack and pass it through).
#[cfg(target_arch = "loongarch64")]
const STACK_REG_INDEX: usize = 3;
#[cfg(not(target_arch = "loongarch64"))]
const STACK_REG_INDEX: usize = 2;

/// Register index for the frame pointer. The clone child starts on a
/// freshly supplied stack; keeping the parent's frame pointer can make
/// the post-syscall libc wrapper spill through the parent's stack.
#[cfg(target_arch = "loongarch64")]
const FRAME_REG_INDEX: usize = 22;
#[cfg(not(target_arch = "loongarch64"))]
const FRAME_REG_INDEX: usize = 8;

pub fn seed_child_leader_context(
    child_thread: &Cap<ThreadIdentity>,
    parent_user_ctx: &UserTrapContext,
    tls: usize,
    stack: usize,
) {
    // (1) Clone the parent context.
    let mut child_ctx = *parent_user_ctx;
    // (2) a0 = 0: child's clone-syscall return value.
    child_ctx.regs[CLONE_CHILD_RETURN_REG_INDEX] = 0;
    // (3) tp = tls: seed the thread pointer for TLS access.
    //     When CLONE_SETTLS is not set, the caller passes 0 and tp
    //     inherits the parent's value (preserved from the clone).
    if tls != 0 {
        child_ctx.regs[TLS_REG_INDEX] = tls;
    }
    // (3b) sp = stack: seed the stack pointer when the syscall's
    //     `newsp` argument is non-zero. Linux `clone(2)`: a non-zero
    //     `newsp` means "the child enters userspace with sp = newsp"
    //     (the libc clone wrapper has already pushed `fn`/`arg` to
    //     that stack); a zero `newsp` means "the child shares the
    //     parent's sp" (bare fork convention).
    //
    //     On LoongArch64, musl's clone child path explicitly clears
    //     `$fp` before it starts consuming the new stack. Mirroring
    //     that shape avoids inheriting a parent frame chain into the
    //     child. RV64 keeps the old "fp follows sp" workaround.
    if stack != 0 {
        child_ctx.regs[STACK_REG_INDEX] = stack;
        #[cfg(target_arch = "loongarch64")]
        {
            child_ctx.regs[FRAME_REG_INDEX] = 0;
        }
        #[cfg(not(target_arch = "loongarch64"))]
        {
            child_ctx.regs[FRAME_REG_INDEX] = stack;
        }
    }
    // (4) PC already points past `ecall`: the trap shell
    // (`tx-kernel::trap_handoff::hand_off_syscall`) added the 4-byte
    // RV64 `ecall` insn width to `pc` at trap-capture time before
    // storing into `saved_user_context`. Adding another 4 here would
    // skip an extra instruction in the child (off-by-one), which
    // shows up as a crash on the first userspace instruction the
    // child should not be executing.
    //
    // The init fixture works around this by luck: the instruction
    // skipped is the `bnez a0, parent_path` branch that for `a0=0`
    // falls through anyway, so the extra +4 lands at the start of
    // the child path. Real musl-built binaries (busybox sh forking
    // for an applet) place a `mv` or load between the ecall and the
    // branch, and the +4 turns into a wild PC.

    // (6) Store on the leader thread's payload. payload_cap == None
    // here means a freshly forked thread already lost its payload,
    // which is a kernel-invariant violation: step_fork's post-condition
    // is exactly that the child leader is live-with-payload.
    let payload = child_thread
        .payload_cap()
        .expect("seed_child_leader_context: fresh child thread missing payload");
    payload.store_saved_user_context(Some(child_ctx));
}

/// Create a new thread within an existing process (clone(2) with
/// CLONE_THREAD).
pub fn step_clone_thread(
    process: &Cap<ProcessIdentity>,
    parent_user_ctx: &UserTrapContext,
    parent_signal_mask: SignalMask,
    stack: usize,
    tls: usize,
    ctid_ptr: u64,
) -> Result<Cap<ThreadIdentity>, ForkError> {
    step_clone_thread_before_attach(
        process,
        parent_user_ctx,
        parent_signal_mask,
        stack,
        tls,
        ctid_ptr,
        |_| {},
    )
}

fn step_clone_thread_before_attach<H>(
    process: &Cap<ProcessIdentity>,
    parent_user_ctx: &UserTrapContext,
    parent_signal_mask: SignalMask,
    stack: usize,
    tls: usize,
    ctid_ptr: u64,
    before_attach: H,
) -> Result<Cap<ThreadIdentity>, ForkError>
where
    H: FnOnce(Tid),
{
    // observe: inspect current subsystem state and validate inputs.
    // upgrade: acquire capabilities/guards needed for mutation.
    // reserve: reserve namespace, memory, or wait-source effects.
    // commit: apply the state transition.
    // publish: emit readiness, signal, or observable outcome.
    let total_start = clone_path_clock_now();
    emit_clone_thread_marker(b"debug.clone_thread.enter", process.pid.0 as i64);
    let allocate_tid_start = clone_path_clock_now();
    let tid = allocate_tid();
    emit_clone_path_duration(
        b"debug.clone_path.step_clone_thread.allocate_tid_ns",
        allocate_tid_start,
    );
    emit_clone_thread_marker(b"debug.clone_thread.allocate_tid.after", tid.0 as i64);
    let sign_thread_start = clone_path_clock_now();
    let child = sign_thread(process.downgrade(), tid)?;
    emit_clone_path_duration(
        b"debug.clone_path.step_clone_thread.sign_thread_ns",
        sign_thread_start,
    );
    emit_clone_thread_marker(b"debug.clone_thread.sign_thread.after", tid.0 as i64);
    let seed_context_start = clone_path_clock_now();
    seed_child_leader_context(&child, parent_user_ctx, tls, stack);
    emit_clone_path_duration(
        b"debug.clone_path.step_clone_thread.seed_context_ns",
        seed_context_start,
    );
    emit_clone_thread_marker(b"debug.clone_thread.seed_context.after", tid.0 as i64);
    let clear_ctid_start = clone_path_clock_now();
    if let Some(payload) = child.payload_cap() {
        payload
            .signal_mask
            .store(parent_signal_mask.raw_bits(), Ordering::Release);
    }
    if ctid_ptr != 0 {
        let payload = child
            .payload_cap()
            .expect("step_clone_thread: fresh child missing payload");
        *payload.clear_child_tid.lock() = Some(ctid_ptr);
    }
    emit_clone_path_duration(
        b"debug.clone_path.step_clone_thread.clear_ctid_ns",
        clear_ctid_start,
    );
    emit_clone_thread_marker(b"debug.clone_thread.clear_ctid.after", tid.0 as i64);
    before_attach(child.tid);
    let attach_start = clone_path_clock_now();
    let mut register_tid_start = None;
    let attach_result = {
        let payload_guard = process.payload.lock();
        match payload_guard.as_ref() {
            None => Err(ForkError::ParentZombie),
            Some(proc_payload) if !proc_payload.attach_thread_if_lifecycle_idle(child.clone()) => {
                Err(ForkError::Busy)
            }
            Some(proc_payload) => {
                register_tid_start = clone_path_clock_now();
                register_tid(child.tid, child.clone());
                sync_thread_group_pending_summary(proc_payload, &child);
                Ok(())
            }
        }
    };
    if let Err(error) = attach_result {
        return Err(error);
    }
    emit_clone_path_duration(
        b"debug.clone_path.step_clone_thread.register_tid_ns",
        register_tid_start,
    );
    emit_clone_thread_marker(b"debug.clone_thread.register_tid.after", tid.0 as i64);
    emit_clone_path_duration(
        b"debug.clone_path.step_clone_thread.attach_ns",
        attach_start,
    );
    emit_clone_thread_marker(b"debug.clone_thread.attach.after", tid.0 as i64);
    emit_clone_path_count(b"debug.clone_path.step_clone_thread.count", 1);
    emit_clone_path_duration(b"debug.clone_path.step_clone_thread.total_ns", total_start);
    Ok(child)
}

#[cfg(test)]
pub(crate) fn step_clone_thread_before_attach_for_test<H>(
    process: &Cap<ProcessIdentity>,
    parent_user_ctx: &UserTrapContext,
    parent_signal_mask: SignalMask,
    stack: usize,
    tls: usize,
    ctid_ptr: u64,
    before_attach: H,
) -> Result<Cap<ThreadIdentity>, ForkError>
where
    H: FnOnce(Tid),
{
    step_clone_thread_before_attach(
        process,
        parent_user_ctx,
        parent_signal_mask,
        stack,
        tls,
        ctid_ptr,
        before_attach,
    )
}

fn emit_clone_thread_marker(name: &[u8], value: i64) {
    if !cfg!(tx_thread_lifecycle_metrics) {
        return;
    }
    if let Some(observer) = tx_observe::current() {
        observer.debug_counter(name, value);
        tx_observe::dump_registered_if_requested();
    }
}

#[inline(always)]
fn clone_path_metrics_enabled() -> bool {
    cfg!(tx_clone_path_metrics) && tx_observe::current().is_some()
}

#[inline(always)]
fn clone_path_clock_now() -> Option<u64> {
    if clone_path_metrics_enabled() {
        Some(tx_observe::clock_now_ns())
    } else {
        None
    }
}

fn emit_clone_path_duration(name: &[u8], start: Option<u64>) {
    let Some(start) = start else {
        return;
    };
    let duration = tx_observe::clock_now_ns().saturating_sub(start);
    emit_clone_path_count(name, duration);
}

fn emit_clone_path_count(name: &[u8], value: u64) {
    if !clone_path_metrics_enabled() {
        return;
    }
    if let Some(observer) = tx_observe::current() {
        observer.debug_counter(name, value as i64);
    }
}

/// Exit the entire thread group: zombify every thread, drop the
/// process payload, set the process exit status. Identity persists.
///
/// Threads zombify with the `wait_status_word` projection of `status`
/// — POSIX `<sys/wait.h>` encoding (`(code & 0xff) << 8` for explicit
/// exits, `sig & 0x7f` for signal exits) — preserving the
/// "thread-side exit_status is an int" shape that `THREAD_RUNTIME_v1`
/// §7.2 carries. (Migrated to POSIX from the day-1 shell-convention
/// `128 + sig` encoding by Wave 1 of the fork/clone/wait4 slice;
/// Open Q #3 DECIDED 2026-05-06.)
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessExitOutcome {
    Completed,
    Retry,
}

pub fn group_exit_status(process: &Cap<ProcessIdentity>) -> Option<ExitStatus> {
    let payload = process.payload.lock().as_ref().cloned()?;
    let status = payload.group_exit.lock().as_ref().map(|state| state.status);
    status
}

/// Complete the current thread's side of a previously published group exit.
/// The process episode owns the status; the thread path only tears down this
/// participant and lets the last-thread cascade finish shared resources.
pub fn step_current_thread_group_exit(thread: &Cap<ThreadIdentity>) {
    if thread.payload_cap().is_none() {
        return;
    }
    let Some(process) = thread.upgrade_owner_proc() else {
        return;
    };
    let status = group_exit_status(&process).unwrap_or(ExitStatus::Exited(0));
    crate::thread_runtime::execution::step_thread_exit_with_status(thread.clone(), status);
}

pub fn step_exit_group_with_posts<F, G>(
    process: &Cap<ProcessIdentity>,
    status: ExitStatus,
    signal_post: F,
    wake_post: G,
) -> ProcessExitOutcome
where
    F: FnMut(ArcWeak<TaskMailbox>, MailboxEvent),
    G: FnMut(&TaskMailbox, MailboxEvent) -> bool,
{
    step_exit_group_with_posts_inner(process, status, signal_post, wake_post, || {})
}

fn step_exit_group_with_posts_inner<F, G, H>(
    process: &Cap<ProcessIdentity>,
    status: ExitStatus,
    mut signal_post: F,
    mut wake_post: G,
    after_reserve: H,
) -> ProcessExitOutcome
where
    F: FnMut(ArcWeak<TaskMailbox>, MailboxEvent),
    G: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    H: FnOnce(),
{
    let payload = match process.payload.lock().as_ref().cloned() {
        Some(payload) => payload,
        None => return ProcessExitOutcome::Completed,
    };
    if !payload.reserve_group_exit(status) {
        return ProcessExitOutcome::Retry;
    }
    after_reserve();
    payload.notify_vfork_done();
    // Installing GroupExit and attaching a CLONE_THREAD participant share
    // `payload.group_exit`, so this snapshot is closed: no new thread can be
    // published after the episode starts.  Do not tear down the address space,
    // fd table, or thread payloads from this hart.  Each thread observes the
    // termination summary and commits its own exit; the last participant alone
    // runs `step_process_exit` and releases process-owned resources.
    let threads = payload.threads.snapshot();
    for thread in &threads {
        if let Some(thread_payload) = thread.payload_cap() {
            thread_payload.update_summary(|summary| summary.termination = true);
            thread_payload.wake_lifecycle_task();
            let _ = post_signal_mailbox_with_post(
                &thread_payload,
                crate::signal::Signum::SIGKILL,
                tx_substrate::wake::SignalRouting::ProcessDirected,
                &mut signal_post,
            );
        }
    }

    // Host tests have no reactor tasks to consume lifecycle wakeups. Drive the
    // same per-thread exit path synchronously so existing process tests still
    // observe a completed zombie rather than a half-published episode.
    #[cfg(any(test, feature = "test-support"))]
    for thread in threads {
        let _ = crate::thread_runtime::execution::step_thread_exit_with_status(thread, status);
    }
    #[cfg(not(any(test, feature = "test-support")))]
    let _ = threads;

    // Keep the injected wake operation part of this API: the last-thread
    // cascade owns child-exit wake publication, while callers that test this
    // reserve/publish boundary still provide the same capability surface.
    let _ = &mut wake_post;
    ProcessExitOutcome::Completed
}

#[cfg(test)]
pub(crate) fn step_exit_group_with_posts_after_reserve_for_test<F, G, H>(
    process: &Cap<ProcessIdentity>,
    status: ExitStatus,
    signal_post: F,
    wake_post: G,
    after_reserve: H,
) -> ProcessExitOutcome
where
    F: FnMut(ArcWeak<TaskMailbox>, MailboxEvent),
    G: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    H: FnOnce(),
{
    step_exit_group_with_posts_inner(process, status, signal_post, wake_post, after_reserve)
}

fn step_process_exit_inner<F, G>(
    process: &Cap<ProcessIdentity>,
    status: ExitStatus,
    mut signal_post: F,
    mut wake_post: G,
) where
    F: FnMut(ArcWeak<TaskMailbox>, MailboxEvent),
    G: FnMut(&TaskMailbox, MailboxEvent) -> bool,
{
    // observe
    if let Some(payload) = process.payload.lock().as_ref() {
        payload.notify_vfork_done();
    }
    // upgrade
    // reserve
    // commit
    // publish
    session_leader_hangup_cascade(process, &mut signal_post, &mut wake_post);
    sever_children(process);
    *process.exit_status.lock() = Some(status);
    crate::ipc::sysv_sem::execution::step_sem_undo_with_post(process, |mailbox, event| {
        wake_post(mailbox, event)
    });
    let mut exit_aspace = None;
    let mut closed_fds = BTreeMap::new();
    {
        let payload_guard = process.payload.lock();
        if let Some(payload) = payload_guard.as_ref() {
            exit_aspace = Some(payload.aspace_cap());
            closed_fds = measure_process_lock_service(
                b"debug.lock_service.process.payload.process_exit.drain_fds.duration_ns",
                || payload.drain_fds(),
            );
        }
    }

    if let Some(aspace) = exit_aspace {
        let _shm_detach = measure_process_lock_service(
            b"debug.lock_service.process.payload.process_exit.shm_detach.duration_ns",
            || crate::ipc::sysv_shm::execution::detach_all_for_aspace(&aspace),
        );
        finalize_open_files_after_fd_drain(&closed_fds);
        measure_process_lock_service(
            b"debug.lock_service.process.payload.process_exit.drop_closed_fds.duration_ns",
            || drop(closed_fds),
        );
    }
    {
        let mut payload_guard = process.payload.lock();
        measure_process_lock_service(
            b"debug.lock_service.process.payload.process_exit.payload_drop.duration_ns",
            || *payload_guard = None,
        );
    }
    if try_auto_reap_adopted_by_init(process) {
        return;
    }
    post_sigchld_to_parent_with_posts(process, &mut signal_post, &mut wake_post);
}

/// Last-thread cascade: called by `thread_runtime::step_thread_exit`
/// when the final thread of a process exits. Materialises
/// `PROCESS_v1` §7.3.3 `step_process_exit`'s commit phase.
///
/// Day-1 scope: severs children's parent slots (§8.1 stub), writes
/// the process-visible exit status, drops the `ProcessPayload`. The
/// remaining §7.3.3 phase-5 cascade (SIGCHLD to parent, `exit_source`
/// wake, full reparenting-into-init, orphan-pgrp SIGHUP,
/// session-leader controlling-tty hangup per §8.3) lands incrementally:
/// SIGCHLD post is wired via `post_sigchld_to_parent`, and the Wave 1
/// fork/clone/wait4 slice (2026-05-06) added the `exit_source` fire
/// alongside the SIGCHLD post for parent-side wake.
pub(crate) fn step_process_exit(process: &Cap<ProcessIdentity>, status: ExitStatus) {
    step_process_exit_inner(
        process,
        status,
        |weak, event| {
            let Some(mailbox) = weak.upgrade() else {
                return;
            };
            let _ = mailbox.post(event);
        },
        |mailbox, event| mailbox.post(event),
    );
}

/// Complete the close protocol after a process fd table has been detached.
///
/// `ProcessPayload::drain_fds` already decrements the explicit pipe/socket
/// descriptor counts while holding the fd-table lock.  This second phase is
/// deliberately outside that lock: file writeback admission and network
/// teardown can perform substantial work and may publish wakeups.
fn finalize_open_files_after_fd_drain(fds: &BTreeMap<u32, Cap<OpenFile>>) {
    let guard = step_engine::borrow_current_guard().unwrap_or_else(step_engine::guard);
    let _ = finalize_detached_open_files(fds.values(), &guard);
}

/// Complete the post-fd-table-removal protocol for every detached open-file
/// description.  final-smp deliberately centralises this operation so close,
/// close_range, dup replacement, exec CLOEXEC and process exit cannot drift
/// into different visibility/last-close semantics.
pub fn finalize_detached_open_files<'a>(
    files: impl IntoIterator<Item = &'a Cap<OpenFile>>,
    guard: &step_engine::Guard<'_>,
) -> Result<(), step_engine::Errno> {
    retry_deferred_page_writebacks(guard, 16);

    let mut seen_files = Vec::new();
    let mut first_error = None;
    for file in files {
        let raw_file = file.raw();
        if seen_files.contains(&raw_file) {
            continue;
        }
        seen_files.push(raw_file);

        if let Err(errno) = finalize_detached_open_file_without_retry(file, guard) {
            first_error.get_or_insert(errno);
        }
    }
    first_error.map_or(Ok(()), Err)
}

/// Allocation-free half of [`finalize_detached_open_files`].
///
/// Exec calls this after its point of no return using the descriptor vector it
/// prepared beforehand.  It must not grow the deferred-retry queue or allocate
/// a temporary deduplication set there.
pub(crate) fn finalize_detached_open_file_without_retry(
    file: &Cap<OpenFile>,
    guard: &step_engine::Guard<'_>,
) -> Result<(), step_engine::Errno> {
    let flush_result = flush_page_backed_open_file_without_retry(file, guard);
    if !file
        .socket_identity()
        .is_some_and(|socket| socket.fd_ref_count() != 0)
    {
        if let Some(ops) = file.file_ops() {
            ops.on_last_close(guard);
        }
    }
    flush_result
}

/// Make dirty regular-file data and its logical EOF visible before a detached
/// file description can disappear.  The background service from main remains
/// active, but it is a durability/performance mechanism rather than a
/// substitute for final-smp's close-to-reopen visibility guarantee.
pub fn flush_page_backed_open_file(
    file: &Cap<OpenFile>,
    guard: &step_engine::Guard<'_>,
) -> Result<(), step_engine::Errno> {
    let result = flush_page_backed_open_file_without_retry(file, guard);
    if result.is_err() {
        use crate::vfs::structure::RNodeBacking;
        if let RNodeBacking::PageBacked { pc } = file.rnode().backing() {
            retain_deferred_page_writeback(pc);
        }
    }
    result
}

fn flush_page_backed_open_file_without_retry(
    file: &Cap<OpenFile>,
    guard: &step_engine::Guard<'_>,
) -> Result<(), step_engine::Errno> {
    use crate::vfs::structure::{OpenFileBacking, RNodeBacking};
    if !matches!(file.backing(), OpenFileBacking::Rnode { .. }) {
        return Ok(());
    }
    let RNodeBacking::PageBacked { pc } = file.rnode().backing() else {
        return Ok(());
    };

    // Main's IO-manager/JBD2 mount deliberately disables the legacy
    // synchronous FsPageBacking hooks.  Close only admits background
    // writeback there; explicit fsync owns and waits for the durability
    // frontier through FsyncOp. Treating the expected ENOSYS from the retired
    // hook as a failure would retain every closed file forever.
    let _ = pc.queue_dirty_file_writeback();
    if pc
        .file_backend_context()
        .is_some_and(|context| context.payload().backend_planner().is_some())
    {
        return Ok(());
    }

    // Legacy mounts have no owned background backend. Preserve final-smp's
    // close-to-reopen visibility and retry failed synchronous writeback.
    match crate::page_backed::step_fsync(pc, guard) {
        StepOutcome::Done(()) => Ok(()),
        StepOutcome::Err(errno) => Err(errno),
        StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => Err(step_engine::Errno::EIO),
    }
}

fn retain_deferred_page_writeback(pc: &Cap<PageContainer>) {
    let mut deferred = DEFERRED_PAGE_WRITEBACKS.lock();
    if !deferred.iter().any(|queued| queued == pc) {
        deferred.push(pc.clone());
    }
}

fn retry_deferred_page_writebacks(guard: &step_engine::Guard<'_>, budget: usize) {
    let batch = {
        let mut deferred = DEFERRED_PAGE_WRITEBACKS.lock();
        let count = budget.min(deferred.len());
        let split_at = deferred.len() - count;
        deferred.split_off(split_at)
    };

    for pc in batch {
        let _ = pc.queue_dirty_file_writeback();
        if pc
            .file_backend_context()
            .is_some_and(|context| context.payload().backend_planner().is_some())
        {
            continue;
        }
        match crate::page_backed::step_fsync(&pc, guard) {
            StepOutcome::Done(()) => {}
            StepOutcome::Err(_) | StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {
                retain_deferred_page_writeback(&pc);
            }
        }
    }
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
    let children: Vec<Cap<ProcessIdentity>> = process.children.drain();

    let init = init_process();
    let target = init.as_ref().filter(|i| i.key() != process.key());

    if let Some(init) = target {
        let init_weak = init.downgrade();
        for child in children {
            *child.parent.lock() = Some(init_weak);
            child.adopted_by_init.store(true, Ordering::Release);
            if child.is_zombie() {
                reap_child_from_parent(init, &child);
            } else {
                init.children.attach(child);
            }
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
///    fg pgrp so they can receive the SIGHUP). Posted through the caller-
///    injected process-group signal route — SIGHUP routes through the
///    catchable shim; SIGCONT through the Gewalt path.
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
fn session_leader_hangup_cascade<F, G>(
    process: &Cap<ProcessIdentity>,
    signal_post: &mut F,
    wake_post: &mut G,
) where
    F: FnMut(ArcWeak<TaskMailbox>, MailboxEvent),
    G: FnMut(&TaskMailbox, MailboxEvent) -> bool,
{
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
        let _ = crate::signal::step_kill_pgrp_with_posts(
            &fg_pgrp,
            crate::signal::Signum::SIGHUP,
            &mut *signal_post,
            &mut *wake_post,
        );
        let _ = crate::signal::step_kill_pgrp_with_posts(
            &fg_pgrp,
            crate::signal::Signum::SIGCONT,
            &mut *signal_post,
            &mut *wake_post,
        );

        // Phase E (orphan-pgrp SIGHUP, PROCESS_v1 §8.3):
        // iterate all process groups in the session; if a pgrp is
        // orphaned (no member has a parent in a different pgrp of
        // this session), send SIGHUP + SIGCONT.  Phase E first pass:
        // iterates all pgrps in `session.members` regardless of
        // orphan status.  TODO: add parent-in-session check for
        // true orphan detection.
        let members: alloc::vec::Vec<Cap<ProcessGroup>> = {
            let guard = step_engine::guard();
            session
                .members
                .snapshot_live(&guard)
                .into_iter()
                .filter(|pgrp| pgrp.key() != fg_pgrp.key())
                .collect()
        };
        for pgrp in &members {
            let _ = crate::signal::step_kill_pgrp_with_posts(
                pgrp,
                crate::signal::Signum::SIGHUP,
                &mut *signal_post,
                &mut *wake_post,
            );
            let _ = crate::signal::step_kill_pgrp_with_posts(
                pgrp,
                crate::signal::Signum::SIGCONT,
                &mut *signal_post,
                &mut *wake_post,
            );
        }
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
/// Wave 1 of the fork/clone/wait4 slice (2026-05-06): also fires the
/// parent's per-process `exit_source` wait channel
/// (`EXIT_SOURCE_CHILD_ZOMBIFIED` bit) so a parent parked on
/// `sys_wait4` (Wave 2) wakes when any child zombifies. Per
/// `txdoc:PROCESS-WAIT-FAMILY-1` and the spec's
/// "Block until a child's state changes" arm.
///
/// No-op for processes with no parent: bootstrap init (never had one)
/// and orphans whose parent has already exited and severed them.
/// Also no-op if the parent is itself a zombie: process-directed kill returns
/// `NoLiveThread`, and `fire_exit_source_with_post` returns `0` (no payload to
/// fire through). The `exit_source` notify is harmless when
/// no awaiter is parked.
///
/// `siginfo` is not yet wired (no `SigInfo` type in day-1 signal
/// surface); spec §7.3.3 phase 5 will populate `si_pid`, `si_uid`,
/// `si_code` (`CLD_EXITED` / `CLD_KILLED` / `CLD_DUMPED`), and
/// `si_status` when the siginfo carrier lands.
fn post_sigchld_to_parent_with_posts(
    process: &Cap<ProcessIdentity>,
    signal_post: &mut dyn FnMut(ArcWeak<TaskMailbox>, MailboxEvent),
    wake_post: &mut dyn FnMut(&TaskMailbox, MailboxEvent) -> bool,
) {
    if let Some(parent) = process.parent_cap() {
        let _ = crate::signal::step_kill_process_with_post(
            &parent,
            crate::signal::Signum::SIGCHLD,
            None,
            signal_post,
        );
        // Fire the parent's exit_source. A zombie parent has no payload
        // and `fire_exit_source_with_post` returns 0 — no panic, no double-fire.
        let _ = crate::process::notification::notify_child_zombified_with_post(&parent, wake_post);
    }
}

fn reap_child_from_parent(parent: &Cap<ProcessIdentity>, child: &Cap<ProcessIdentity>) {
    let key = child.key();

    parent.children.retain(|c| c.key() != key);

    let pgrp = child.pgrp_cap();
    pgrp.members.retain(|weak| {
        weak.observe_with_guard(|ident| ident.key() != key)
            .unwrap_or(true)
    });

    unregister_pid(child.pid);
}

fn maintenance_after_process_reap() {
    if step_engine::borrow_current_guard().is_some() {
        return;
    }

    let _ = step_engine::drain_with_budget(128);
    let _ = step_engine::drain_with_budget(128);
}

fn try_auto_reap_adopted_by_init(process: &Cap<ProcessIdentity>) -> bool {
    if !process.adopted_by_init.load(Ordering::Acquire) {
        return false;
    }
    let Some(parent) = process.parent_cap() else {
        return false;
    };
    let Some(init) = init_process() else {
        return false;
    };
    if parent.key() != init.key() {
        return false;
    }

    reap_child_from_parent(&parent, process);
    maintenance_after_process_reap();
    true
}

/// Exit the entire thread group due to a fatal signal. Records
/// `ExitStatus::Signaled(sig)` and runs the same payload teardown as
/// the normal group-exit transition.
///
/// Materialises `route_sigkill`'s `invoke_group_exit_with_signal`
/// per `SIGNAL_v1` §12.3. Any caller of Gewalt SIGKILL routing,
/// `ast_check`'s `DefaultTerminate` outcome, or a (future) fatal
/// synchronous-fault path goes through this to actually take the
/// process down.
pub fn step_exit_group_with_signal_with_posts<F, G>(
    process: &Cap<ProcessIdentity>,
    sig: crate::signal::Signum,
    signal_post: F,
    wake_post: G,
) -> ProcessExitOutcome
where
    F: FnMut(ArcWeak<TaskMailbox>, MailboxEvent),
    G: FnMut(&TaskMailbox, MailboxEvent) -> bool,
{
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    step_exit_group_with_posts(process, ExitStatus::Signaled(sig), signal_post, wake_post)
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
    // observe
    // upgrade
    // reserve
    // commit
    // publish
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
    let snapshot: Vec<Cap<ProcessIdentity>> = parent.children.snapshot();

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
    reap_child_from_parent(parent, &child);
    drop(child);
    maintenance_after_process_reap();

    Ok((pid, status))
}

/// Return whether a blocking wait for `target` still has a matching live
/// child and no matching zombie. This is a read-only predicate used to close
/// the observe/register/recheck window in the async wait driver.
pub fn waitpid_would_block(parent: &Cap<ProcessIdentity>, target: WaitTarget) -> bool {
    let target = match target {
        WaitTarget::CallerPgrp => WaitTarget::Pgrp(parent.pgrp_cap().pgid),
        other => other,
    };
    let snapshot: Vec<Cap<ProcessIdentity>> = parent.children.snapshot();
    let mut any_match = false;
    for child in &snapshot {
        if !target.matches(child) {
            continue;
        }
        any_match = true;
        if child.is_zombie() {
            return false;
        }
    }
    any_match
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
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    let Ok(payload) = target.upgrade_operational() else {
        return ChdirOutcome::ZombieIgnored;
    };
    let prev = payload
        .cwd
        .lock()
        .replace(ProcessCwdState::Legacy(new_cwd))
        .map(|state| state.dentry());
    ChdirOutcome::Replaced { prev }
}

pub fn step_chdir_with_mount(
    target: &Cap<ProcessIdentity>,
    new_cwd: Cap<crate::vfs::DEntry>,
    new_mount: Cap<crate::mount::MountIdentity>,
) -> ChdirOutcome {
    let Ok(payload) = target.upgrade_operational() else {
        return ChdirOutcome::ZombieIgnored;
    };
    let prev = payload
        .cwd
        .lock()
        .replace(ProcessCwdState::Mounted(CwdBinding {
            dentry: new_cwd,
            mount: new_mount,
        }))
        .map(|state| state.dentry());
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
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    let payload = target.upgrade_operational().ok()?;
    let cwd = payload.cwd()?;
    // Namespace-aware: a cwd inside a mounted fs (e.g. the sdcard ext4 at
    // /musl) must render with its mountpoint prefix, or callers that
    // round-trip getcwd() through an absolute walk resolve a directory
    // that does not exist.
    let ns = target.mount_namespace_cap();
    crate::vfs::render_dentry_path_in_namespace(&cwd, ns.as_ref())
}

/// Day-1 setpgid: supports creating a fresh process group rooted at
/// `target.pid` inside the target's current session. Joining an
/// existing group requires session-walk, which is a follow-up.
pub fn step_setpgid(target: &Cap<ProcessIdentity>, new_pgid: Pgid) -> Result<(), SetpgidError> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    if target.is_zombie() {
        return Err(SetpgidError::Zombie);
    }
    if new_pgid.0 != target.pid.0 {
        return Err(SetpgidError::Unimplemented);
    }

    // Clone session out of the current pgrp; we'll keep the same
    // session and create a new pgrp inside it.
    let old_pgrp = target.pgrp.lock().clone();
    if old_pgrp.pgid == new_pgid {
        return Ok(());
    }
    let session = old_pgrp.session.clone();

    let new_pgrp = sign_process_group(new_pgid, session.clone())?;
    new_pgrp.session.members.attach(new_pgrp.downgrade());
    new_pgrp.members.attach(target.downgrade());

    // Drop target from old pgrp.
    drop_member(&old_pgrp, target);

    *target.pgrp.lock() = new_pgrp;
    register_pgrp(new_pgid, target.pgrp_cap());
    Ok(())
}

/// Day-1 setsid: creates a fresh `Session` + leader `ProcessGroup`
/// rooted at the target's pid, severs any controlling-tty link the
/// new session might have inherited (it can't have one yet), and
/// rebinds the target.
pub fn step_setsid(target: &Cap<ProcessIdentity>) -> Result<Sid, SetsidError> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    let new_sid = Sid(target.pid.0);
    let new_pgid = Pgid(target.pid.0);
    if target.is_zombie() {
        return Err(SetsidError::Zombie);
    }
    if target.pgrp_cap().pgid == new_pgid {
        return Err(SetsidError::ProcessGroupLeader);
    }

    let new_session = sign_session(new_sid)?;
    let new_pgrp = sign_process_group(new_pgid, new_session.clone())?;

    new_session.members.attach(new_pgrp.downgrade());
    new_pgrp.members.attach(target.downgrade());

    let old_pgrp = target.pgrp.lock().clone();
    drop_member(&old_pgrp, target);

    *target.pgrp.lock() = new_pgrp;
    register_session(new_sid, new_session);
    register_pgrp(new_pgid, target.pgrp_cap());
    Ok(new_sid)
}

// --- exec phase-7 commit helpers (post-PoNR, infallible) -------------
//
// The steps below materialise the remaining per-process commits exec phase 7
// applies after `txdoc:EXEC-11-PHASE-6-ADDRESS-SPACE-VISIBILITY-BOUNDARY`
// has stored the new aspace. Each is **infallible** and **synchronous**
// per `txdoc:EXEC-15-THE-EXEC-PONR-INVARIANT` — no allocation, no I/O,
// no fallible computation past phase 6. The exec script (Part 5) calls
// these in sequence; they are also unit-testable in isolation.
//
// Note for Phase 5 (the exec script itself): these are NOT async. Other
// `step_*` functions in this crate that perform I/O (`step_open`,
// `vm::scripts::populate_detached_user_range`) are async, but the
// phase-7 commits act on already-resolved Caps and atomic words —
// nothing to await. The script's phase 7 is therefore a sequential
// block of synchronous calls.

// CLOEXEC closure is prepared and committed with the AS swap by
// `ProcessExecPrep`. `ResetSignalDispositionsForExecOp` and
// `InstallBrkForExecOp` delegate into `process/exec_prep.rs` to keep
// this file under the 1500-line authored-source cap while preserving
// the public StepOp surface.

// --- internal sign-and-publish helpers ---

fn sign_session(sid: Sid) -> Result<Cap<Session>, ZoneError> {
    step_engine::sign(Session {
        sid,
        controlling_tty: process_spin_mutex(None, b"debug.lock.process.session.controlling_tty"),
        members: SessionMembers::new(),
    })
}

fn sign_process_group(pgid: Pgid, session: Cap<Session>) -> Result<Cap<ProcessGroup>, ZoneError> {
    step_engine::sign(ProcessGroup {
        pgid,
        session,
        members: ProcessGroupMembers::new(),
    })
}

fn sign_process_identity(
    pid: Pid,
    parent: Option<Weak<ProcessIdentity>>,
    pgrp: Cap<ProcessGroup>,
) -> Result<Cap<ProcessIdentity>, ZoneError> {
    step_engine::sign(ProcessIdentity {
        pid,
        parent: process_spin_mutex(parent, b"debug.lock.process.identity.parent"),
        children: ProcessChildren::new(),
        pgrp: process_spin_mutex(pgrp, b"debug.lock.process.identity.pgrp"),
        exit_status: process_spin_mutex(None, b"debug.lock.process.identity.exit_status"),
        adopted_by_init: AtomicBool::new(false),
        payload: process_spin_mutex(None, b"debug.lock.process.identity.payload"),
    })
}

#[allow(clippy::too_many_arguments)]
fn sign_process_payload(
    aspace: Cap<AddressSpace>,
    threads: Vec<Cap<ThreadIdentity>>,
    nsproxy: Cap<crate::process::nsproxy::NsProxy>,
    cred: Cred,
    cwd: Option<ProcessCwdState>,
    fds: BTreeMap<u32, Cap<OpenFile>>,
    fd_cloexec: BTreeSet<u32>,
    rlimit_nofile: (u32, u32),
    rlimit_memlock: (u64, u64),
    net_namespace: PayloadCap<crate::net::NetNamespacePayload>,
    brk_base: u64,
    current_brk: u64,
    umask: u16,
    personality: u32,
    sig_actions: Arc<SigActionTable>,
) -> Result<PayloadCap<ProcessPayload>, ZoneError> {
    use crate::process::adapter::step_engine::AtomicSlot;
    use crate::process::adapter::step_engine::{RawPort, RawQueue};
    let aspace_slot: AtomicSlot<Cap<AddressSpace>> = AtomicSlot::empty();
    aspace_slot.store(Some(aspace));

    // PR-9 phase 5 (D5 Path A): mint a fresh `Cap<Cred>` for this
    // process's cred slot. `bootstrap_init_process` passes `Cred::root()`;
    // `step_fork` passes a value-copy of the parent's current `Cred`
    // (the cred snapshot is read via `payload.cred()` at fork entry —
    // which itself dereferences the parent's `Cap<Cred>` — so the
    // child receives a fresh cap pointing at an independent slab
    // entry, not a clone of the parent's cap key). Per D5 constraint
    // #3: don't share the cap; that would couple parent and child
    // cred lifetimes incorrectly.
    let cred_cap = crate::cred::sign_cred(cred)?;
    let cred_slot: AtomicSlot<Cap<crate::cred::Cred>> = AtomicSlot::empty();
    cred_slot.store(Some(cred_cap));

    // Namespace proxy slot — the caller provides the immutable
    // namespace bundle (init nsproxy for bootstrap, parent clone for
    // fork, or a new bundle for unshare).
    let nsproxy_slot: AtomicSlot<Cap<crate::process::nsproxy::NsProxy>> = AtomicSlot::empty();
    nsproxy_slot.store(Some(nsproxy));
    let net_namespace_slot: AtomicSlot<PayloadCap<crate::net::NetNamespacePayload>> =
        AtomicSlot::empty();
    net_namespace_slot.store(Some(net_namespace));

    // Allocate a fresh exit WaitSource per ProcessPayload and register it with
    // the global wait-source resolver so async awaiters can park without
    // holding a Cap<ProcessIdentity>.
    let exit_wait_point = crate::process::notification::new_exit_wait_point();

    let cap = step_engine::sign(ProcessPayload {
        frame: Frame {
            vm: aspace_slot,
            sig_actions,
        },
        threads: ProcessThreads::from_vec(threads),
        group_pending: PendingSignalQueue::new(),
        siginfo_slots: crate::signal::SigInfoSlots::new(),
        signal_port: RawPort::new(),
        cred: cred_slot,
        cred_mutation: process_spin_mutex(
            crate::process::structure::CredMutationState::new(),
            b"debug.lock.process.payload.cred_mutation",
        ),
        nsproxy: nsproxy_slot,
        net_namespace: net_namespace_slot,
        cwd: process_spin_mutex(cwd, b"debug.lock.process.payload.cwd"),
        fds: process_spin_mutex(fds, b"debug.lock.process.payload.fds"),
        fd_cloexec: process_spin_mutex(fd_cloexec, b"debug.lock.process.payload.fd_cloexec"),
        rlimit_nofile_cur: AtomicU32::new(rlimit_nofile.0),
        rlimit_nofile_max: AtomicU32::new(rlimit_nofile.1),
        rlimit_memlock_cur: AtomicU64::new(rlimit_memlock.0),
        rlimit_memlock_max: AtomicU64::new(rlimit_memlock.1),
        brk_base: AtomicU64::new(brk_base),
        current_brk: AtomicU64::new(current_brk),
        // Slice 6 of the shell-prompt roadmap. Per-process
        // file-creation mask. `bootstrap_init_process` seeds with
        // the Linux default `0o022` (owner keeps full perms,
        // group/other lose write); `step_fork` propagates the
        // parent's umask through this argument (umask is
        // per-process, copied across fork). `step_exec` preserves
        // the umask (umask survives `exec` per POSIX).
        umask: core::sync::atomic::AtomicU16::new(umask & 0o777),
        personality: AtomicU32::new(personality),
        sem_undos: process_spin_mutex(BTreeMap::new(), b"debug.lock.process.payload.sem_undos"),
        exit_source_id: exit_wait_point.source_id,
        exit_wait_source: exit_wait_point.source,
        exit_source_bus: RawQueue::new(),
        _cmdline: process_spin_mutex(None, b"debug.lock.process.payload.cmdline"),
        _exe_file: process_spin_mutex(None, b"debug.lock.process.payload.exe_file"),
        _comm: process_spin_mutex([0u8; 16], b"debug.lock.process.payload.comm"),
        thread_count: AtomicU32::new(1), // leader thread
        group_exit: process_spin_mutex(None, b"debug.lock.process.payload.group_exit"),
        next_group_exit_generation: AtomicU64::new(0),
        vfork_done: AtomicBool::new(false),
        vfork_waiter: process_spin_mutex(None, b"debug.lock.process.payload.vfork_waiter"),
    })?;
    Ok(PayloadCap::from_cap(cap))
}

fn sign_thread(
    owner_proc: Weak<ProcessIdentity>,
    tid: Tid,
) -> Result<Cap<ThreadIdentity>, ZoneError> {
    let total_start = clone_path_clock_now();
    let payload_fresh_start = clone_path_clock_now();
    let payload_value = ThreadPayload::fresh();
    emit_clone_path_duration(
        b"debug.clone_path.sign_thread.payload_fresh_ns",
        payload_fresh_start,
    );
    emit_clone_thread_marker(b"debug.clone_thread.payload_fresh.after", tid.0 as i64);
    let payload_sign_start = clone_path_clock_now();
    let payload_cap = step_engine::sign(payload_value)?;
    emit_clone_path_duration(
        b"debug.clone_path.sign_thread.payload_sign_ns",
        payload_sign_start,
    );
    emit_clone_thread_marker(b"debug.clone_thread.payload_sign.after", tid.0 as i64);
    let payload_cap_start = clone_path_clock_now();
    let payload = PayloadCap::from_cap(payload_cap);
    emit_clone_path_duration(
        b"debug.clone_path.sign_thread.payload_cap_ns",
        payload_cap_start,
    );
    emit_clone_thread_marker(b"debug.clone_thread.payload_cap.after", tid.0 as i64);
    let identity_sign_start = clone_path_clock_now();
    let identity = step_engine::sign(ThreadIdentity {
        tid,
        owner_proc,
        exit_status: SpinMutex::new(None),
        payload: SpinMutex::new(Some(payload)),
    })?;
    emit_clone_path_duration(
        b"debug.clone_path.sign_thread.identity_sign_ns",
        identity_sign_start,
    );
    emit_clone_thread_marker(b"debug.clone_thread.identity_sign.after", tid.0 as i64);
    emit_clone_path_count(b"debug.clone_path.sign_thread.count", 1);
    emit_clone_path_duration(b"debug.clone_path.sign_thread.total_ns", total_start);
    Ok(identity)
}

/// Test-only: attach a fresh sibling thread to `target`'s thread
/// list. Used by D9-B's eligibility-scan pins and the D9-C
/// interrupt-wake test to construct multi-thread processes without
/// going through the (not-yet-shipped) `clone(CLONE_THREAD)` path.
///
/// Returns the new thread's `Cap<ThreadIdentity>`. The thread's
/// `signal_mask` starts empty; tests block signals via
/// `step_sigprocmask` after spawning.
///
/// Hidden behind `cfg(any(test, feature = "test-support"))` so it
/// never reaches release builds.
#[cfg(any(test, feature = "test-support"))]
pub fn spawn_sibling_thread_for_test(
    target: &Cap<ProcessIdentity>,
) -> Result<Cap<ThreadIdentity>, ZoneError> {
    let tid = allocate_tid();
    let sibling = sign_thread(target.downgrade(), tid)?;
    register_tid(sibling.tid, sibling.clone());
    if let Some(payload) = target.payload.lock().as_ref() {
        payload.threads.attach(sibling.clone());
        sync_thread_group_pending_summary(payload, &sibling);
    }
    Ok(sibling)
}

fn drop_member(pgrp: &Cap<ProcessGroup>, target: &Cap<ProcessIdentity>) {
    let target_key = target.key();
    pgrp.members.retain(|weak| {
        weak.observe_with_guard(|ident| ident.key() != target_key)
            .unwrap_or(true)
    });
}

// Helper trait alias — Weak observation under a guard is verbose; this
// wraps it in a closure.
trait WeakObserveExt<T: 'static> {
    fn observe_with_guard<F, R>(&self, f: F) -> Option<R>
    where
        F: FnOnce(IdentRef<'_, T>) -> R;
}

impl<T: 'static> WeakObserveExt<T> for Weak<T> {
    fn observe_with_guard<F, R>(&self, f: F) -> Option<R>
    where
        F: FnOnce(IdentRef<'_, T>) -> R,
    {
        // Process-group / children cleanup runs this helper from the
        // process-exit path, which a fatal-signal teardown can reach while an
        // epoch guard is already active. Borrow that guard instead of opening a
        // nested one (the EBR domain asserts guards never nest).
        let guard = step_engine::borrow_current_guard().unwrap_or_else(step_engine::guard);
        self.observe(&guard).map(f)
    }
}

// ---------------------------------------------------------------------------
// PR-2 StepOp wraps
// ---------------------------------------------------------------------------
//
// Additive `impl StepOp` adapters per `docs/Txv3/03_STEP_MODEL_v2.md` §2.1.
// Each wrap stores its inputs (Cap by value — Cap is Clone+Send+Sync;
// scalars/Copy by value; `&T` under a single lifetime `'a`) and delegates
// `step()` to the corresponding free fn above. Synchronous free fns whose
// return type is not already a `StepOutcome` are lifted via
// `StepOutcome::Done(...)` (or `StepOutcome::Err(_)` when the free fn
// returns a `Result<_, E>` that maps onto `StepOutcome::Err`).
//
// The free fns remain the source of truth; callers can migrate to the
// `*Op` types incrementally.

/// `StepOp` wrap of [`step_fork`]. The pmap generic `P: PmapIf` is
/// carried as a `PhantomData` so the wrap type fixes the platform at
/// construction time without runtime cost.
///
/// `Output = Result<Cap<ProcessIdentity>, ForkError>` — the rich error
/// type is preserved inside `StepOutcome::Done` so callers can match on
/// `Vm` / `Zone` variants without an Errno crush.
pub struct ForkOp<'a, P: PmapIf> {
    pub parent: &'a Cap<ProcessIdentity>,
    pub clone_vm: bool,
    pub clone_sighand: bool,
    pub clone_newipc: bool,
    pub clone_newnet: bool,
    pub clone_newns: bool,
    pub _pmap: core::marker::PhantomData<P>,
}

impl<'a, P: PmapIf, I: SubjectIdentity> StepOp<I> for ForkOp<'a, P> {
    type Output = Result<Cap<ProcessIdentity>, ForkError>;
    type Progress = NoProgress;
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        StepOutcome::Done(step_fork_with_options::<P>(
            self.parent,
            ForkOptions {
                clone_vm: self.clone_vm,
                clone_sighand: self.clone_sighand,
                clone_newipc: self.clone_newipc,
                clone_newnet: self.clone_newnet,
                clone_newns: self.clone_newns,
            },
        ))
    }
}

impl<P: PmapIf, I: SubjectIdentity> OneShotStepOp<I> for ForkOp<'_, P> {}

/// `StepOp` wrap of [`step_clone_thread`] for `clone(CLONE_THREAD)`.
///
/// This keeps thread clone in the same one-shot step vocabulary as fork:
/// the operation is synchronous, does not yield, and preserves the underlying
/// `ForkError` so syscall translation can distinguish lifecycle contention
/// (`EAGAIN`) from allocation failure (`ENOMEM`).
pub struct CloneThreadOp<'a> {
    pub process: &'a Cap<ProcessIdentity>,
    pub parent_user_ctx: &'a UserTrapContext,
    pub parent_signal_mask: SignalMask,
    pub stack: usize,
    pub tls: usize,
    pub ctid_ptr: u64,
}

impl<'a, I: SubjectIdentity> StepOp<I> for CloneThreadOp<'a> {
    type Output = Result<Cap<ThreadIdentity>, ForkError>;
    type Progress = NoProgress;

    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        StepOutcome::Done(step_clone_thread(
            self.process,
            self.parent_user_ctx,
            self.parent_signal_mask,
            self.stack,
            self.tls,
            self.ctid_ptr,
        ))
    }
}

impl<I: SubjectIdentity> OneShotStepOp<I> for CloneThreadOp<'_> {}

/// `StepOp` wrap of [`step_waitpid_nohang`]. Returns the result type
/// `Result<(Pid, ExitStatus), WaitError>` via `StepOutcome::Done` so
/// callers can inspect both the success tuple and WNOHANG-empty signal.
pub struct WaitpidNohangOp<'a> {
    pub parent: &'a Cap<ProcessIdentity>,
    pub target: WaitTarget,
}

impl<'a, I: SubjectIdentity> StepOp<I> for WaitpidNohangOp<'a> {
    type Output = Result<(Pid, ExitStatus), WaitError>;
    type Progress = NoProgress;
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        let result = step_waitpid_nohang(self.parent, self.target);
        match result {
            Ok(outcome) => StepOutcome::Done(Ok(outcome)),
            Err(WaitError::NoneReady) => {
                // No child has exited yet — yield on the process exit
                // endpoint. The drive loop parks the task; when a child exits,
                // step_process_exit fires exit_source, and drive() re-calls
                // step().
                if let Some(endpoint) = self.parent.exit_endpoint() {
                    let exit_id = tx_substrate::wake::WaitEndpoint::source_id(&endpoint).raw();
                    return StepOutcome::Yield {
                        progress: NoProgress,
                        shape: YieldShape::on_wait_source(exit_id, 1),
                    };
                }
                // No exit source registered — would spin forever.
                StepOutcome::Done(Err(WaitError::NoneReady))
            }
            Err(e) => StepOutcome::Done(Err(e)),
        }
    }
}

/// `StepOp` wrap of [`step_chdir`].
pub struct ChdirOp<'a> {
    pub target: &'a Cap<ProcessIdentity>,
    pub new_cwd: Cap<crate::vfs::DEntry>,
    pub new_mount: Option<Cap<crate::mount::MountIdentity>>,
}

impl<'a, I: SubjectIdentity> StepOp<I> for ChdirOp<'a> {
    type Output = ChdirOutcome;
    type Progress = NoProgress;
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        StepOutcome::Done(match &self.new_mount {
            Some(mount) => step_chdir_with_mount(self.target, self.new_cwd.clone(), mount.clone()),
            None => step_chdir(self.target, self.new_cwd.clone()),
        })
    }
}

impl OneShotStepOp<crate::process::ProcessIdentity> for ChdirOp<'_> {}

/// `StepOp` wrap of [`step_getcwd`].
pub struct GetcwdOp<'a> {
    pub target: &'a Cap<ProcessIdentity>,
}

impl<'a, I: SubjectIdentity> StepOp<I> for GetcwdOp<'a> {
    type Output = Option<alloc::vec::Vec<u8>>;
    type Progress = NoProgress;
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        StepOutcome::Done(step_getcwd(self.target))
    }
}

impl OneShotStepOp<crate::process::ProcessIdentity> for GetcwdOp<'_> {}

/// `StepOp` wrap of [`step_setpgid`].
pub struct SetpgidOp<'a> {
    pub target: &'a Cap<ProcessIdentity>,
    pub new_pgid: Pgid,
}

impl<'a, I: SubjectIdentity> StepOp<I> for SetpgidOp<'a> {
    type Output = ();
    type Progress = NoProgress;
    /// PR-3 refactored: splits `Done(Ok(()))` / `Err(domain_error)`
    /// at the `StepOutcome` level so `drive_oneshot` can translate
    /// domain errors into `Result<(), Errno>`.
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<(), NoProgress> {
        match step_setpgid(self.target, self.new_pgid) {
            Ok(()) => StepOutcome::Done(()),
            Err(SetpgidError::Zombie) => {
                StepOutcome::Err(crate::process::adapter::step_engine::Errno::ESRCH)
            }
            Err(SetpgidError::Unimplemented) => {
                StepOutcome::Err(crate::process::adapter::step_engine::Errno::ENOSYS)
            }
            Err(SetpgidError::Zone(_)) => {
                StepOutcome::Err(crate::process::adapter::step_engine::Errno::ENOMEM)
            }
        }
    }
}

impl OneShotStepOp<crate::process::ProcessIdentity> for SetpgidOp<'_> {}

/// `StepOp` wrap of [`step_setsid`].
pub struct SetsidOp<'a> {
    pub target: &'a Cap<ProcessIdentity>,
}

impl<'a, I: SubjectIdentity> StepOp<I> for SetsidOp<'a> {
    type Output = Sid;
    type Progress = NoProgress;
    /// PR-3 refactored: splits `Done(Ok(sid))` / `Err(Zone)` at
    /// the `StepOutcome` level.
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Sid, NoProgress> {
        match step_setsid(self.target) {
            Ok(sid) => StepOutcome::Done(sid),
            Err(SetsidError::ProcessGroupLeader) => {
                StepOutcome::Err(crate::process::adapter::step_engine::Errno::EPERM)
            }
            Err(SetsidError::Zombie) => {
                StepOutcome::Err(crate::process::adapter::step_engine::Errno::ESRCH)
            }
            Err(SetsidError::Zone(_)) => {
                StepOutcome::Err(crate::process::adapter::step_engine::Errno::ENOMEM)
            }
        }
    }
}

impl OneShotStepOp<crate::process::ProcessIdentity> for SetsidOp<'_> {}

/// `StepOp` wrap of the exec signal-disposition reset helper.
pub struct ResetSignalDispositionsForExecOp<'a> {
    pub process: &'a Cap<ProcessIdentity>,
}

impl<'a, I: SubjectIdentity> StepOp<I> for ResetSignalDispositionsForExecOp<'a> {
    type Output = ();
    type Progress = NoProgress;
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        super::exec_prep::reset_signal_dispositions_for_exec(self.process);
        StepOutcome::Done(())
    }
}

impl OneShotStepOp<crate::process::ProcessIdentity> for ResetSignalDispositionsForExecOp<'_> {}

/// `StepOp` wrap of the exec brk installation helper.
pub struct InstallBrkForExecOp<'a> {
    pub process: &'a Cap<ProcessIdentity>,
    pub new_brk_base: u64,
}

impl<'a, I: SubjectIdentity> StepOp<I> for InstallBrkForExecOp<'a> {
    type Output = ();
    type Progress = NoProgress;
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        super::exec_prep::install_brk_for_exec(self.process, self.new_brk_base);
        StepOutcome::Done(())
    }
}

impl OneShotStepOp<crate::process::ProcessIdentity> for InstallBrkForExecOp<'_> {}

// ---------------------------------------------------------------------------
// PR-3 fd-table StepOp wraps (close, dup, dup3, fcntl)
// ---------------------------------------------------------------------------

/// `StepOp` wrap for `close(fd)`. PR-3 fd-table migration.
///
/// Concrete `ProcessIdentity` type parameter so `drive_oneshot` works
/// with the kernel's `ScriptCtx<ProcessIdentity>`.
pub struct CloseOp {
    pub process: Cap<ProcessIdentity>,
    pub fd: u32,
}

impl StepOp<crate::process::ProcessIdentity> for CloseOp {
    type Output = Cap<crate::vfs::OpenFile>;
    type Progress = NoProgress;
    fn step(
        &mut self,
        _ctx: &mut ScriptCtx<crate::process::ProcessIdentity>,
    ) -> StepOutcome<Cap<crate::vfs::OpenFile>, NoProgress> {
        let file = match self.process.take_fd_for_close(self.fd) {
            Some(file) => file,
            None => return StepOutcome::Err(crate::process::adapter::step_engine::Errno::EBADF),
        };
        StepOutcome::Done(file)
    }
}

impl OneShotStepOp<crate::process::ProcessIdentity> for CloseOp {}

/// `StepOp` wrap for `dup(oldfd)`. PR-3 fd-table migration.
pub struct DupOp {
    pub process: Cap<ProcessIdentity>,
    pub oldfd: u32,
}

impl StepOp<crate::process::ProcessIdentity> for DupOp {
    type Output = u32;
    type Progress = NoProgress;
    fn step(
        &mut self,
        _ctx: &mut ScriptCtx<crate::process::ProcessIdentity>,
    ) -> StepOutcome<u32, NoProgress> {
        let file = match self.process.fd(self.oldfd) {
            Some(f) => f,
            None => return StepOutcome::Err(crate::process::adapter::step_engine::Errno::EBADF),
        };
        super::structure::incr_pipe_fd_ref(&file);
        match self.process.install_new_fd(file.clone(), false) {
            Some(newfd) => StepOutcome::Done(newfd),
            None => {
                super::structure::decr_pipe_fd_ref(&file);
                StepOutcome::Err(crate::process::adapter::step_engine::Errno::EAGAIN)
            }
        }
    }
}

impl OneShotStepOp<crate::process::ProcessIdentity> for DupOp {}

/// `StepOp` wrap for `dup3(oldfd, newfd, flags)`. PR-3 fd-table migration.
pub struct Dup3Op {
    pub process: Cap<ProcessIdentity>,
    pub oldfd: u32,
    pub newfd: u32,
    pub flags: u32,
}

impl StepOp<crate::process::ProcessIdentity> for Dup3Op {
    type Output = (u32, Option<Cap<OpenFile>>);
    type Progress = NoProgress;
    fn step(
        &mut self,
        _ctx: &mut ScriptCtx<crate::process::ProcessIdentity>,
    ) -> StepOutcome<(u32, Option<Cap<OpenFile>>), NoProgress> {
        if self.oldfd == self.newfd {
            return StepOutcome::Err(crate::process::adapter::step_engine::Errno::EINVAL);
        }
        const O_CLOEXEC: u32 = 0o2000000; // Linux generic ABI
        if self.flags & !O_CLOEXEC != 0 {
            return StepOutcome::Err(crate::process::adapter::step_engine::Errno::EINVAL);
        }
        let file = match self.process.fd(self.oldfd) {
            Some(f) => f,
            None => return StepOutcome::Err(crate::process::adapter::step_engine::Errno::EBADF),
        };
        super::structure::incr_pipe_fd_ref(&file);
        let want_cloexec = self.flags & O_CLOEXEC != 0;
        let previous = self
            .process
            .install_fd_with_cloexec(self.newfd, file, want_cloexec);
        StepOutcome::Done((self.newfd, previous))
    }
}

impl OneShotStepOp<crate::process::ProcessIdentity> for Dup3Op {}

/// `StepOp` wrap for `fcntl(F_GETFD)` and `fcntl(F_SETFD)`.
/// PR-3 fd-table migration.  Returns `(is_cloexec: bool)` on GET,
/// `()` on SET.  EBADF if the fd is not open.
pub struct FcntlFdOp {
    pub process: Cap<ProcessIdentity>,
    pub fd: u32,
    pub set_on: Option<bool>, // None = GET, Some(bit) = SET
}

impl StepOp<crate::process::ProcessIdentity> for FcntlFdOp {
    type Output = Option<bool>; // None on SET, Some(bit) on GET
    type Progress = NoProgress;
    fn step(
        &mut self,
        _ctx: &mut ScriptCtx<crate::process::ProcessIdentity>,
    ) -> StepOutcome<Option<bool>, NoProgress> {
        if self.process.fd(self.fd).is_none() {
            return StepOutcome::Err(crate::process::adapter::step_engine::Errno::EBADF);
        }
        match self.set_on {
            Some(on) => {
                self.process.set_fd_cloexec(self.fd, on);
                StepOutcome::Done(None)
            }
            None => StepOutcome::Done(Some(self.process.fd_cloexec(self.fd))),
        }
    }
}

impl OneShotStepOp<crate::process::ProcessIdentity> for FcntlFdOp {}

/// `StepOp` wrap for `fcntl(F_DUPFD)` / `fcntl(F_DUPFD_CLOEXEC)`.
pub struct FcntlDupFdOp {
    pub process: Cap<ProcessIdentity>,
    pub fd: u32,
    pub min: u32,
    pub cloexec: bool,
}

impl StepOp<crate::process::ProcessIdentity> for FcntlDupFdOp {
    type Output = u32;
    type Progress = NoProgress;
    fn step(
        &mut self,
        _ctx: &mut ScriptCtx<crate::process::ProcessIdentity>,
    ) -> StepOutcome<u32, NoProgress> {
        if self.process.fd(self.fd).is_none() {
            return StepOutcome::Err(crate::process::adapter::step_engine::Errno::EBADF);
        }
        let file = self.process.fd(self.fd).unwrap();
        super::structure::incr_pipe_fd_ref(&file);
        match self
            .process
            .install_new_fd_at_least(self.min, file.clone(), self.cloexec)
        {
            Some(new_fd) => StepOutcome::Done(new_fd),
            None => {
                super::structure::decr_pipe_fd_ref(&file);
                StepOutcome::Err(crate::process::adapter::step_engine::Errno::EAGAIN)
            }
        }
    }
}

impl OneShotStepOp<crate::process::ProcessIdentity> for FcntlDupFdOp {}

#[cfg(test)]
#[path = "step_op_wraps.rs"]
mod step_op_wraps;
