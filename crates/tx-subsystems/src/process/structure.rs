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
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering};

use crate::process::adapter::step_engine::{
    self, AtomicSlot, Cap, Dead, Entity, PayloadCap, PayloadPolicy, RawPort, RawQueue, SpinMutex,
    Weak, Zone, ZoneAllocated,
};
use crate::process::adapter::wait_routing::{self, Channel, Mask, WaitSource};

use crate::cred::{Cred, CredSnapshot, Gid, Uid};
use crate::execution::WaitToken;
use crate::ipc::sysv_sem::structure::SemUndo;
use crate::process::topology::{
    ProcessChildren, ProcessGroupMembers, ProcessThreads, SessionMembers,
};
use crate::signal::{PendingSignalQueue, SigActionTable};
use crate::thread_runtime::ThreadIdentity;
use crate::tty::structure::identity::TtyIdentity;
use crate::vfs::{DEntry, OpenFile};
use crate::vm::AddressSpace;

/// Bit-mask for the "child has zombified" event on the per-process
/// `exit_source`. Future events (stop, continue) get their own bits
/// alongside their wakers; the slice carves out only this single bit.
///
/// Cites: `txdoc:PROCESS-WAIT-FAMILY-1`
/// (`docs/design/04_process-signals/PROCESS_v1.md` §7.4); plan
/// `docs/progress/plans/2026-05-06-fork-clone-wait4.md` Open Q #1.
pub const EXIT_SOURCE_CHILD_ZOMBIFIED: u64 = 0x1;

/// Event bit for `signal_port` (RawPort) fire.
///
/// Fired each time a catchable signal is delivered to this process.
/// All bus subscribers (signalfd, future pidfd/timerfd) wake on this
/// single bit; the subscribers then re-read their respective pending
/// state to determine which signal(s) arrived. Single-bit design per
/// `SIGNAL_ATTACHMENTS_v1` §2 (Carrier = RawPort, Polarity = fire).
///
/// See: `txdoc:SIGNAL-ATTACHMENTS-CATALOG-SCHEMA-1`.
pub const SIGNAL_GENERATED: u64 = 0x1;

/// Per-process exit disposition, populated by `step_exit_group` /
/// `step_exit_group_with_signal` / the last-thread cascade. Spec
/// counterpart in `PROCESS_v1` §6.2 ("priority of process exit
/// status"): both shapes feed into the future `wait(2)` status word.
///
/// Day-1 deliberately omits the core-dump bit — `Signaled` carries
/// only the signum until the fatal-Core action lands.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExitStatus {
    /// Explicit `step_exit_group(int)` exit.
    Exited(i32),
    /// Terminated by a fatal signal (default `Term` action or SIGKILL).
    Signaled(crate::signal::Signum),
}

impl ExitStatus {
    /// POSIX `<sys/wait.h>` status word as `wait4(2)` returns it via
    /// `wstatus`. The encoding matches Linux's generic ABI:
    ///
    /// - `Exited(code)` → `(code & 0xff) << 8` — `WIFEXITED(s)` is
    ///   `(s & 0x7f) == 0` and `WEXITSTATUS(s) == (s >> 8) & 0xff`.
    /// - `Signaled(sig)` → `sig.raw() & 0x7f` — `WIFSIGNALED(s)` is
    ///   `(((s & 0x7f) + 1) >> 1) > 0` and `WTERMSIG(s) == s & 0x7f`.
    ///
    /// Out of slice: the core-dump bit (`s & 0x80`) is not computed —
    /// txKernel doesn't track core-dump state. Stop/continue encoding
    /// (`(sig << 8) | 0x7f` / `0xffff`) lands with the stop/cont
    /// signal infrastructure slice.
    ///
    /// **Migration note (fork/clone/wait4 slice, 2026-05-06).**
    /// Previously this returned the day-1 shell-convention `128 + sig`
    /// shape. The shell encoding is the *userspace shell* (bash-style
    /// program-exit-code) convention; the kernel↔userspace `wait4` ABI
    /// uses the POSIX encoding above. Migrated to POSIX in tree per
    /// the slice plan's Open Q #3 (DECIDED 2026-05-06: replace and
    /// migrate the trio's smoke assertions).
    pub fn wait_status_word(self) -> i32 {
        match self {
            ExitStatus::Exited(code) => (code & 0xff) << 8,
            ExitStatus::Signaled(sig) => (sig.raw() as i32) & 0x7f,
        }
    }

    /// `Some(sig)` iff this is a `Signaled` exit. Convenience for
    /// callers that only care about the signum (kept-shape accessor
    /// from before the unification).
    pub fn terminating_signal(self) -> Option<crate::signal::Signum> {
        match self {
            ExitStatus::Exited(_) => None,
            ExitStatus::Signaled(sig) => Some(sig),
        }
    }
}

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
    /// Authoritative upward binding to the parent process. `None` only
    /// for `pid=1` init (no parent) and for processes whose parent has
    /// exited (severed at parent's `step_process_exit` per §8.1).
    /// Held weakly so a parent's exit does not pin children — children
    /// may outlive their parent. Per `PROCESS_v1` §2.1 the spec wants
    /// `Binding<ProcessIdentity>`; day-1 uses `SpinMutex<Option<Weak<...>>>`
    /// (same retention story; CAS-rebind arrives with the substrate
    /// `Binding<T>` primitive).
    pub(crate) parent: SpinMutex<Option<Weak<ProcessIdentity>>>,
    /// Downward materialization of children: processes whose `parent`
    /// binding names this process. Per `PROCESS_v1` §2.1 the spec
    /// shape is `DllContainer<ProcessIdentity>`; day-1 uses
    /// `SpinMutex<Vec<Cap<ProcessIdentity>>>` because children must
    /// be observable here until reaped (§8.5: "zombies stay in
    /// pgrp.members and session.members until reap. Withdrawn at
    /// reap, not at exit." — same retention story for parent.children:
    /// the children container is the **retainer** that keeps zombie
    /// children alive for `waitpid` even after every other strong
    /// reference has dropped).
    ///
    /// Pushed by `step_fork`; walked by `step_process_exit` (sever
    /// child's parent slot) and `step_waitpid_nohang` (reap —
    /// withdraws the Cap, releasing retention).
    pub(crate) children: ProcessChildren,
    pub(crate) pgrp: SpinMutex<Cap<ProcessGroup>>,
    /// Process-visible exit disposition. `Some` once the process has
    /// run `step_exit_group` / `step_exit_group_with_signal` (or the
    /// last-thread cascade has fired); otherwise `None`. Discriminates
    /// explicit-int exits from signal-driven termination per
    /// `PROCESS_v1` §6.2.
    pub(crate) exit_status: SpinMutex<Option<ExitStatus>>,
    pub(crate) payload: SpinMutex<Option<PayloadCap<ProcessPayload>>>,
}

/// `ProcessIdentity` is the production [`SubjectIdentity`]
/// per [D1](../../../../docs/progress/decisions/2026-05-11-d1-scriptctx-trait-bound-identity.md).
///
/// The trait declares what `step_v3` algebra needs to know about a
/// subject (its credential view, restriction-stack view, optional
/// thread-identity view, and exit-source id). Concrete types stay
/// here in the subsystem layer.
///
/// `Restrictions` currently uses the `step_v3::RestrictionStackHandle`
/// placeholder because the real append-only stack lives in
/// `tx-policy`, which is still skeleton. PR-K wires the real type
/// alongside the seccomp/landlock landing. The trait's
/// `Restrictions` associated type will swap to the real type then;
/// callers using `<I as SubjectIdentity>::Restrictions` will not
/// need to change.
impl step_engine::SubjectIdentity for ProcessIdentity {
    type Credential = crate::cred::Cred;
    type Restrictions = step_engine::RestrictionStackHandle;
    type ThreadIdentity = crate::thread_runtime::ThreadIdentity;

    fn exit_source(&self) -> Option<step_engine::WaitSourceId> {
        self.exit_source_id().map(step_engine::WaitSourceId::new)
    }

    /// Returns the process's PID low 32 bits as the task trace identity.
    ///
    /// For `drive<O, ProcessIdentity>` call sites the subject is a
    /// process; the PID is a stable, unique-enough discriminant for
    /// flow-ID hashing. Per-thread TID resolution is deferred to when
    /// `ScriptCtx` carries a thread cap; for now PID is sufficient to
    /// distinguish flows across processes. Kernel actors that use the
    /// placeholder `ProcessIdentity` inherit the default `0`.
    fn task_id_low(&self) -> u32 {
        self.pid.0
    }

    fn thread_deliverable_signal_pending(thread: &Cap<Self::ThreadIdentity>) -> bool {
        thread
            .payload_cap()
            .map(|payload| payload.interrupt_summary().deliverable_signal)
            .unwrap_or(false)
    }

    fn thread_termination_in_force(thread: &Cap<Self::ThreadIdentity>) -> bool {
        thread
            .payload_cap()
            .map(|payload| payload.interrupt_summary().termination)
            .unwrap_or(true)
    }

    fn thread_stop_requested(thread: &Cap<Self::ThreadIdentity>) -> bool {
        thread
            .payload_cap()
            .map(|payload| payload.interrupt_summary().stop_requested)
            .unwrap_or(false)
    }
}

impl ProcessIdentity {
    /// Access the process payload slot.  Returns `None` when the
    /// process is a zombie (payload dropped).
    pub fn payload_slot(&self) -> &SpinMutex<Option<PayloadCap<ProcessPayload>>> {
        &self.payload
    }

    /// Snapshot the current process-group `Cap`. The returned `Cap` is a
    /// strong reference; it remains valid until dropped even if the
    /// target rebinds via `setpgid`.
    pub fn pgrp_cap(&self) -> Cap<ProcessGroup> {
        self.pgrp.lock().clone()
    }

    /// Snapshot the parent's `Cap` if the parent identity is still
    /// retained somewhere. Returns `None` for `pid=1` init (no parent)
    /// and after the parent identity has been fully reclaimed.
    pub fn parent_cap(&self) -> Option<Cap<ProcessIdentity>> {
        let weak = (*self.parent.lock())?;
        let guard = step_engine::guard();
        weak.upgrade(&guard)
    }

    /// Render the parent's pid for `getppid` and tracing. Returns
    /// `Pid::RESERVED` when there's no parent (init) or the parent
    /// has been reclaimed. Day-1 single-namespace; namespace-aware
    /// rendering arrives with `PidName` / `nsproxy`.
    pub fn parent_pid(&self) -> Pid {
        self.parent_cap().map(|p| p.pid).unwrap_or(Pid::RESERVED)
    }

    /// Number of children currently retained by this process. All
    /// entries are valid `Cap`s — zombies and live children alike.
    /// Children leave this list only via `step_waitpid_nohang`'s
    /// reap step or when this process itself reclaims.
    pub fn child_count(&self) -> usize {
        self.children.len()
    }

    /// Snapshot the children list as owned `Cap`s. The returned
    /// vec is independent of the locked container; the lock is
    /// released before return. Includes zombie children (callers
    /// that need to skip zombies should `is_zombie()`-filter).
    pub fn children(&self) -> alloc::vec::Vec<Cap<ProcessIdentity>> {
        self.children.snapshot()
    }

    /// Read the recorded exit status. `Some` once `step_exit_group`
    /// (or last-thread `step_thread_exit`) has run; otherwise `None`.
    pub fn exit_status(&self) -> Option<ExitStatus> {
        *self.exit_status.lock()
    }

    /// Read the terminating signal, if any. `Some(sig)` if the recorded
    /// exit status is `ExitStatus::Signaled`; `None` for explicit-int
    /// exits and live processes.
    pub fn terminating_signal(&self) -> Option<crate::signal::Signum> {
        self.exit_status.lock().and_then(|s| s.terminating_signal())
    }

    /// Whether the process is a zombie (payload dropped, identity
    /// retained). Equivalent to `exit_status().is_some() || payload is None`
    /// but cheaper.
    pub fn is_zombie(&self) -> bool {
        self.payload.lock().is_none()
    }

    /// Process state char for /proc/<pid>/stat.
    pub fn state_char(&self) -> u8 {
        if self.is_zombie() {
            b'Z'
        } else {
            b'R'
        }
    }

    /// Process command-line (delegates to payload).
    pub fn ident_cmdline(&self) -> Option<alloc::vec::Vec<u8>> {
        self.payload.lock().as_ref()?.cmdline()
    }

    /// Process short name comm (delegates to payload).
    pub fn ident_comm(&self) -> Option<[u8; 16]> {
        Some(self.payload.lock().as_ref()?.comm())
    }

    /// Store siginfo for a delivered signal (delegates to payload).
    pub fn siginfo_store(&self, sig: crate::signal::Signum, info: crate::signal::SigInfo) {
        if let Some(payload) = self.payload.lock().as_ref() {
            payload.siginfo_slots.store(sig, info);
        }
    }

    /// Take siginfo for a signal being delivered to a userspace handler.
    pub fn siginfo_take(&self, sig: crate::signal::Signum) -> Option<crate::signal::SigInfo> {
        let payload_guard = self.payload.lock();
        let payload = payload_guard.as_ref()?;
        let info = payload.siginfo_slots.get(sig);
        payload.siginfo_slots.clear(sig);
        info
    }

    /// Find a thread by its tid within this process.
    pub fn thread_by_tid(&self, tid: u32) -> Option<Cap<ThreadIdentity>> {
        self.payload.lock().as_ref()?.threads.find_by_tid(tid)
    }

    /// Collapse all sibling threads for exec, leaving `initiator`
    /// as the sole live thread. Returns the number of siblings
    /// removed, or `None` for zombies.
    pub fn collapse_threads_for_exec(&self, initiator: &Cap<ThreadIdentity>) -> Option<usize> {
        let payload = {
            let payload_guard = self.payload.lock();
            payload_guard.as_ref().cloned()?
        };
        payload.collapse_threads_for_exec(initiator)
    }

    /// Snapshot the current address space `Cap`, if the process is
    /// alive. Returns `None` for zombies.
    ///
    /// Loads from the per-payload `AtomicSlot<Cap<AddressSpace>>`
    /// (`txdoc:EXEC-11-PHASE-6-ADDRESS-SPACE-VISIBILITY-BOUNDARY`). The
    /// slot is always populated for live payloads — initial state is
    /// installed by `bootstrap_init_process` / `step_fork`, and exec's
    /// phase 6 store keeps the slot inhabited at all times. A `None`
    /// here therefore strictly means "the payload itself is gone"
    /// (zombie), never "the slot is empty on a live payload".
    pub fn aspace_cap(&self) -> Option<Cap<AddressSpace>> {
        self.payload.lock().as_ref().map(|p| p.aspace_cap())
    }

    /// Snapshot the per-process credential. Returns `None` for zombies
    /// (payload dropped — cred is unobservable).
    ///
    /// `Cred` is `Copy`; the snapshot is independent of the lock and
    /// safe to hold across `.await` points. Mutators
    /// (`cred::step_setuid` / `step_setgid` / `step_setres{u,g}id` /
    /// `step_setre{u,g}id`) acquire the lock internally; readers that
    /// only need a coherent point-in-time view should use this
    /// accessor rather than reaching into the `pub(crate)` payload
    /// field directly.
    ///
    /// Used by the DAC + setuid slice's `SyscallCtx::cred()` accessor
    /// (`tx-shims::linux_syscall`) so every cred-mutation /
    /// cred-reading syscall arm can read through one snapshot under
    /// one lock acquisition.
    pub fn cred(&self) -> Option<Cred> {
        self.payload.lock().as_ref().map(|p| p.cred())
    }

    /// Snapshot the current `Cap<Cred>` for this process, if alive.
    /// Returns `None` for zombies (payload dropped — cred unobservable).
    ///
    /// PR-9 phase 5 (D5 Path A): companion to [`Self::cred`]. Returns
    /// the cap, not the value, so callers building a
    /// `SubjectAuthority` can pass it directly without re-signing.
    /// Used by `tx_shims::linux_syscall::SyscallCtx::cred_cap()` and
    /// the v3 subject-population helpers.
    pub fn cred_cap(&self) -> Option<Cap<Cred>> {
        self.payload.lock().as_ref().map(|p| p.cred_cap())
    }

    /// Capture a syscall-entry credential snapshot for this process.
    ///
    /// Per `cred_service_v_1` §"In flight": the script holds a by-value
    /// metadata copy of the credential, taken once at syscall entry and
    /// threaded through prelude → checks → commit. This is the
    /// `Cap<ProcessIdentity>`-side entry point producing that snapshot.
    ///
    /// Returns `None` for zombies (payload dropped — credential
    /// unobservable). Live syscall arms never reach `None` by
    /// construction; `SyscallCtx` falls back to `CredSnapshot::root()`
    /// defensively at construction time.
    pub fn cred_snapshot(&self) -> Option<CredSnapshot> {
        self.payload.lock().as_ref().map(|p| p.cred_snapshot())
    }

    /// Snapshot the per-process namespace proxy bundle.
    /// Returns `None` for zombies (no payload).
    pub fn nsproxy_cap(&self) -> Option<Cap<crate::process::nsproxy::NsProxy>> {
        self.payload.lock().as_ref().map(|p| p.nsproxy_cap())
    }

    pub fn mount_namespace_cap(&self) -> Option<Cap<crate::mount::MountNamespace>> {
        self.nsproxy_cap()?.mnt_ns.clone()
    }

    /// Process short name (for `/proc/<pid>/stat`). Returns `"?"` for
    /// zombies (no payload).
    pub fn comm(&self) -> [u8; 16] {
        self.payload
            .lock()
            .as_ref()
            .map(|p| p.comm())
            .unwrap_or([b'?', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])
    }

    /// Process command-line (for `/proc/<pid>/cmdline`). Returns
    /// `None` for zombies (no payload) or when no cmdline was set.
    pub fn cmdline(&self) -> Option<alloc::vec::Vec<u8>> {
        self.payload.lock().as_ref().and_then(|p| p.cmdline())
    }

    /// Process executable file DEntry (for `/proc/<pid>/exe`).
    /// Returns `None` for zombies (no payload).
    pub fn exe_file(&self) -> Option<Cap<DEntry>> {
        self.payload.lock().as_ref().and_then(|p| p.exe_file())
    }

    /// Replace this process's address space with `new` and return the
    /// previous `Cap` so the caller can defer-drop it via EBR. Per
    /// `txdoc:EXEC-11-PHASE-6-ADDRESS-SPACE-VISIBILITY-BOUNDARY` this
    /// is the single irreversible exec store: the new aspace becomes
    /// observable to every concurrent fault / VM lookup the moment
    /// this returns. Returns `None` for zombies (no payload — caller
    /// drops `new` itself).
    pub fn replace_aspace(&self, new: Cap<AddressSpace>) -> Option<Cap<AddressSpace>> {
        let payload_guard = self.payload.lock();
        let payload = payload_guard.as_ref()?;
        payload.frame.vm.swap(Some(new))
    }

    /// Snapshot the `Cap<OpenFile>` registered at fd `idx` on this
    /// process's payload. Returns `None` if the process is a zombie
    /// (no payload) or the slot is empty.
    ///
    /// Per fd-ops Wave 1 the underlying storage is a sparse
    /// `BTreeMap<u32, Cap<OpenFile>>`; any `u32` fd value is valid as
    /// a key and an absent key reads back as `None`.
    ///
    /// Used by the syscall dispatcher (`tx-shims::linux_syscall`) to
    /// resolve fds without holding the payload lock across a step's
    /// `.await`.
    pub fn fd(&self, idx: u32) -> Option<Cap<crate::vfs::OpenFile>> {
        self.payload.lock().as_ref().and_then(|p| p.fd(idx))
    }

    /// Snapshot the current working-directory `Cap<DEntry>` if one is
    /// installed on the payload. Returns `None` for zombies or
    /// processes whose cwd has never been bound (init pre-rootfs).
    ///
    /// Used by `tx_fs::devfs::open_console_for_init` (the walker
    /// redirect) to pick the search root for `step_open(/dev/console)`.
    pub fn cwd(&self) -> Option<Cap<DEntry>> {
        self.payload.lock().as_ref().and_then(|p| p.cwd())
    }

    /// Install `file` at fd `idx` on this process's payload, returning
    /// the previously installed `Cap<OpenFile>` if any. No-op (returns
    /// `None`) for zombies. Passing `file = None` removes the fd from
    /// the table.
    ///
    /// Per fd-ops Wave 1 the underlying storage is a sparse
    /// `BTreeMap<u32, Cap<OpenFile>>`; any `u32` fd value is valid as
    /// a key.
    ///
    /// Used by the syscall dispatcher's tests and (Phase 3b)
    /// `init.rs` to preopen fds 0/1/2 against
    /// `tx_fs::devfs::open_console_for_init()`.
    pub fn set_fd(
        &self,
        idx: u32,
        file: Option<Cap<crate::vfs::OpenFile>>,
    ) -> Option<Cap<crate::vfs::OpenFile>> {
        self.payload
            .lock()
            .as_ref()
            .and_then(|p| p.set_fd(idx, file))
    }

    /// Allocate the lowest unused fd ≥ 0 without installing anything.
    /// Returns `0` for zombies (no payload) — the caller must not
    /// install against a zombie regardless.
    ///
    /// Per fd-ops Wave 1 plan §C: needed by `sys_openat` (Wave 2),
    /// `sys_dup` (Wave 4), and `sys_pipe2` (Wave 5) to mimic Linux's
    /// "lowest unused fd" semantic.
    pub fn allocate_fd(&self) -> u32 {
        self.payload
            .lock()
            .as_ref()
            .map(|p| p.allocate_fd_at_least(0))
            .unwrap_or(0)
    }

    /// Allocate the lowest unused fd ≥ `min`. Returns `min` for
    /// zombies. Used by the future `F_DUPFD`-shape arms (lowest fd
    /// ≥ N) and shells doing `>&5`-style redirection — the
    /// `dup3(oldfd, newfd, 0)` arm with a specific `newfd` target
    /// uses [`Self::install_fd`] instead.
    pub fn allocate_fd_at_least(&self, min: u32) -> u32 {
        self.payload
            .lock()
            .as_ref()
            .map(|p| p.allocate_fd_at_least(min))
            .unwrap_or(min)
    }

    /// Return the lowest unused fd ≥ `min` without installing
    /// anything. Returns `min` for zombies. Differs from
    /// [`Self::allocate_fd_at_least`] only by intent: callers that
    /// want to peek at the next free fd without committing to install
    /// use this; both methods share the same underlying scan.
    pub fn next_fd_above(&self, min: u32) -> u32 {
        self.allocate_fd_at_least(min)
    }

    pub fn rlimit_nofile(&self) -> (u32, u32) {
        self.payload
            .lock()
            .as_ref()
            .map(|p| p.rlimit_nofile())
            .unwrap_or((1024, 4096))
    }

    pub fn set_rlimit_nofile(&self, cur: u32, max: u32) {
        if let Some(payload) = self.payload.lock().as_ref() {
            payload.set_rlimit_nofile(cur, max);
        }
    }

    /// Install `file` at the specific fd `fd`, returning the
    /// previously installed `Cap<OpenFile>` if any so the caller can
    /// drop it under their own EBR guard. Returns `None` for zombies
    /// (the install is a no-op).
    ///
    /// Convenience over [`Self::set_fd`] when callers always want to
    /// install a `Some(file)` (matches the `dup2`/`dup3` shape: if
    /// the target slot was occupied the previous occupant must be
    /// closed). Identical effect to
    /// `set_fd(fd, Some(file))` modulo the explicit `Cap` parameter.
    pub fn install_fd(
        &self,
        fd: u32,
        file: Cap<crate::vfs::OpenFile>,
    ) -> Option<Cap<crate::vfs::OpenFile>> {
        self.set_fd(fd, Some(file))
    }

    /// Read the close-on-exec bit for fd `fd` on this process's
    /// payload. Returns `false` for zombies (no payload) and for
    /// unmarked fds.
    ///
    /// Per fd-ops Wave 1 the underlying storage is a sparse
    /// `BTreeSet<u32>`; any `u32` fd value is valid (no fd-31
    /// ceiling).
    ///
    /// Used by `tx-shims::linux_syscall::sys_fcntl` to service
    /// `F_GETFD`, and by future `sys_open` plumbing once `O_CLOEXEC`
    /// is threaded through the open arm.
    pub fn fd_cloexec(&self, fd: u32) -> bool {
        self.payload
            .lock()
            .as_ref()
            .map(|p| p.fd_cloexec_get(fd))
            .unwrap_or(false)
    }

    /// Set or clear the close-on-exec bit for fd `fd` on this
    /// process's payload. No-op for zombies.
    ///
    /// Per fd-ops Wave 1 the underlying storage is a sparse
    /// `BTreeSet<u32>`; any `u32` fd value is valid (no fd-31
    /// ceiling).
    ///
    /// Used by `tx-shims::linux_syscall::sys_fcntl` to service
    /// `F_SETFD`, and by future `sys_open` plumbing once `O_CLOEXEC`
    /// is threaded through.
    pub fn set_fd_cloexec(&self, fd: u32, value: bool) {
        if let Some(payload) = self.payload.lock().as_ref() {
            payload.set_fd_cloexec(fd, value);
        }
    }

    /// Internal: snapshot the full close-on-exec set as an owned
    /// `BTreeSet<u32>`. Returns an empty set for zombies. Used by
    /// [`crate::process::execution::step_close_cloexec_fds`] during
    /// exec phase 7 to walk every marked fd.
    pub(crate) fn fd_cloexec_snapshot(&self) -> BTreeSet<u32> {
        self.payload
            .lock()
            .as_ref()
            .map(|p| p.fd_cloexec_snapshot())
            .unwrap_or_default()
    }

    /// Internal: clear the entire close-on-exec set. Used by
    /// [`crate::process::execution::step_close_cloexec_fds`] after the
    /// sweep so future `fcntl(F_SETFD)` calls start from a clean
    /// state.
    pub(crate) fn clear_fd_cloexec(&self) {
        if let Some(payload) = self.payload.lock().as_ref() {
            payload.clear_fd_cloexec();
        }
    }

    /// Snapshot the current `SigActionEntry` for `sig` from this
    /// process's per-process action table. Returns `None` for zombies
    /// (no payload). Used by the `rt_sigaction(2)` syscall dispatcher
    /// to read the live entry without going through
    /// `step_sigaction` (which would mutate). Per `SIGNAL_v1` §15.1.
    pub fn sig_action_entry(
        &self,
        sig: crate::signal::Signum,
    ) -> Option<crate::signal::SigActionEntry> {
        self.payload
            .lock()
            .as_ref()
            .map(|p| p.sig_actions().get_entry(sig))
    }

    /// Compatibility snapshot for call sites that only care about
    /// the disposition arm.
    pub fn sig_disposition(
        &self,
        sig: crate::signal::Signum,
    ) -> Option<crate::signal::SigDisposition> {
        self.sig_action_entry(sig).map(|entry| entry.disposition)
    }

    /// Snapshot siginfo stored for a delivered signal.
    pub fn siginfo_get(&self, sig: crate::signal::Signum) -> Option<crate::signal::SigInfo> {
        self.payload
            .lock()
            .as_ref()
            .and_then(|p| p.siginfo_slots.get(sig))
    }

    /// Clear siginfo stored for a delivered signal.
    pub fn siginfo_clear(&self, sig: crate::signal::Signum) {
        if let Some(payload) = self.payload.lock().as_ref() {
            payload.siginfo_slots.clear(sig);
        }
    }

    /// Snapshot the program-break base for this process. Returns `0`
    /// for zombies and for processes with no brk region configured.
    /// Used by the `brk(2)` syscall dispatcher to compute the
    /// `brk_script` arguments per `txdoc:VM-5-8-BRK`.
    pub fn brk_base(&self) -> u64 {
        self.payload
            .lock()
            .as_ref()
            .map(|p| p.brk_base())
            .unwrap_or(0)
    }

    /// Snapshot the current program break for this process. Returns
    /// `0` for zombies and for processes with no brk region.
    pub fn current_brk(&self) -> u64 {
        self.payload
            .lock()
            .as_ref()
            .map(|p| p.current_brk())
            .unwrap_or(0)
    }

    /// Update the current program break. No-op for zombies. Used by
    /// the `brk(2)` syscall dispatcher.
    pub fn set_current_brk(&self, value: u64) {
        if let Some(payload) = self.payload.lock().as_ref() {
            payload.set_current_brk(value);
        }
    }

    /// Snapshot the per-process file-creation mask. Returns `0` for
    /// zombies (no payload — defensively, the alive caller of
    /// `umask(2)` always has a payload). Slice 6 of the shell-prompt
    /// roadmap.
    pub fn umask(&self) -> u16 {
        self.payload.lock().as_ref().map(|p| p.umask()).unwrap_or(0)
    }

    /// Atomically replace the per-process file-creation mask, returning
    /// the previous value. No-op (returns `0`) for zombies. The
    /// argument is silently truncated to `0o777` per Linux semantics
    /// (`umask(2)` ignores bits above the `rwxrwxrwx` triplets).
    /// Used by the Slice 6 `sys_umask` arm.
    pub fn swap_umask(&self, new: u16) -> u16 {
        self.payload
            .lock()
            .as_ref()
            .map(|p| p.swap_umask(new))
            .unwrap_or(0)
    }

    /// Number of currently-live threads owned by this process.
    /// Returns `0` for zombies.
    pub fn live_thread_count(&self) -> usize {
        self.payload
            .lock()
            .as_ref()
            .map(|p| p.thread_count.load(Ordering::Acquire) as usize)
            .unwrap_or(0)
    }

    /// Snapshot the `Cap<ThreadIdentity>` of the thread at slot `idx`
    /// in this process's thread list. Returns `None` for zombies and
    /// for indices outside the live thread count.
    ///
    /// Used by the syscall dispatcher's tests (and the future Phase 6
    /// userspace-entry shim) to grab the leader thread without
    /// reaching into the `pub(crate)` payload field directly. Index 0
    /// is always the leader for processes constructed by
    /// `bootstrap_init_process` / `step_fork`; multi-threaded
    /// processes (post-`clone`) extend the list.
    pub fn nth_thread(&self, idx: usize) -> Option<Cap<ThreadIdentity>> {
        let payload_guard = self.payload.lock();
        let payload = payload_guard.as_ref()?;

        payload.threads.nth(idx)
    }

    /// Carrier id under which this process's `exit_source` channel is
    /// registered with the global wait-source resolver. Returns
    /// `None` for zombies (no payload — the channel is unreachable
    /// once the payload has been dropped).
    ///
    /// Wave 2's `sys_wait4` blocking arm pairs this with
    /// [`EXIT_SOURCE_CHILD_ZOMBIFIED`] to build the `WaitToken` it
    /// awaits via [`crate::wait_source::wait_on_token`].
    pub fn exit_source_id(&self) -> Option<u64> {
        self.payload.lock().as_ref().map(|p| p.exit_source_id())
    }

    /// Notify a vfork-parent that this child process has exec'd or
    /// exited.  Forwards to [`ProcessPayload::notify_vfork_done`].
    /// No-op for zombie processes (no payload).
    pub fn notify_vfork_done(&self) {
        if let Some(payload) = self.payload.lock().as_ref() {
            payload.notify_vfork_done();
        }
    }

    /// Check whether the child has exec'd or exited (vfork_done).
    /// Returns `true` for zombie processes (payload dropped → process
    /// has exited).
    pub fn is_vfork_done(&self) -> bool {
        self.payload
            .lock()
            .as_ref()
            .is_none_or(|p| p.vfork_done.load(Ordering::Acquire))
    }

    /// Build the `WaitToken` an awaiter parks on while waiting for any
    /// child of this process to zombify. Returns `None` for zombies
    /// (no payload).
    ///
    /// The returned token's interest mask is
    /// [`EXIT_SOURCE_CHILD_ZOMBIFIED`] — Wave 1 carves out only the
    /// child-zombified bit; future stop/cont events get separate bits
    /// alongside their own wakers.
    pub fn exit_source_wait_token(&self) -> Option<WaitToken> {
        self.exit_source_id()
            .map(|id| WaitToken::new(id, EXIT_SOURCE_CHILD_ZOMBIFIED))
    }

    /// Fire the `exit_source` channel with `mask`, returning the number
    /// of awaiters released by [`Channel::fire`]. No-op (returns `0`)
    /// for zombies.
    ///
    /// PR-3D-3 (D2/D4 coexistence): also fires the parallel
    /// [`Self::exit_wait_source`] (the new `Arc<WaitSource>` mailbox
    /// path) with the same mask reinterpreted as
    /// [`InterestMask`]. Both fires happen under the same
    /// payload-lock observation, so the zombie/live edge is
    /// idempotent on both paths — once the payload drops, neither
    /// fires (returns 0 / posts 0 events). Double-call on a
    /// still-live payload re-fires both: that's a property of
    /// `Channel::fire` (subsequent callers see the latch) and of
    /// `WaitSource::notify` (re-posts to any subscribers still
    /// registered). Production callers only invoke this once per
    /// transition (`post_sigchld_to_parent` runs once per zombify),
    /// so re-fire is not a concern in practice.
    pub fn fire_exit_source(&self, mask: Mask) -> usize {
        self.payload
            .lock()
            .as_ref()
            .map(|p| {
                let released = p.exit_source().fire(mask);
                // PR-3D-3 new path: post `MailboxEvent::SourceFired`
                // to any v3 caller that registered a `TaskMailbox`
                // against this process's exit_wait_source. Same
                // mask bits — the legacy Channel and the new
                // WaitSource share the bit-namespace
                // (`EXIT_SOURCE_CHILD_ZOMBIFIED` and future stop/cont
                // bits land in both).
                wait_routing::notify_v3_source(p.exit_wait_source(), mask.bits());
                // Phase A (bus wire alignment): set the same mask on
                // the RawQueue readiness wire so native bus subscribers
                // (future signalfd, pidfd) wake without going through
                // the legacy Channel. Level-triggered semantics
                // (`set` is idempotent); consumers re-read the queue
                // state via `subscribe` + `try_take_ready`.
                // See: `txdoc:SIGNAL-ATTACHMENTS-CATALOG-SCHEMA-2`.
                p.exit_source_bus.fire(mask.bits());
                released
            })
            .unwrap_or(0)
    }

    /// PR-3D-3: per-process `WaitSource` for the new mailbox-based
    /// wake path. Returns `None` for zombies (no payload — the
    /// source is unreachable once the payload has been dropped).
    /// `WaitSource::id()` matches the `u64` returned by
    /// [`Self::exit_source_id`].
    pub fn exit_wait_source(&self) -> Option<Arc<WaitSource>> {
        self.payload
            .lock()
            .as_ref()
            .map(|p| p.exit_wait_source().clone())
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

impl Entity for ProcessIdentity {
    type OperationalEvidence = PayloadCap<ProcessPayload>;

    fn upgrade_operational(identity: &Cap<Self>) -> Result<Self::OperationalEvidence, Dead> {
        identity.payload.lock().as_ref().cloned().ok_or(Dead)
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
/// (`rlimits`, `fd_table`) land in follow-up passes without changing
/// the existing surface.
/// Group-exit coordination state (PROCESS_v1 §5).
#[derive(Debug)]
pub(crate) struct GroupExitState {
    #[allow(dead_code)] // txdoc:vfs-full-bringup-scaffold
    pub status: ExitStatus,
    #[allow(dead_code)] // txdoc:vfs-full-bringup-scaffold
    pub is_exec: bool,
    pub remaining_threads: AtomicU32,
}

/// Per-process resource frame — the set of resources governed by
/// clone(2) sharing flags.
///
/// `Frame` groups the fields that can optionally be shared across
/// processes via `Shared<Frame>`. v1 stores it inline on
/// `ProcessPayload`; the cross-process sharing path is deferred
/// to the `Shared<T>` wiring slice.
pub struct Frame {
    /// Virtual address space (CLONE_VM). Atomic slot so exec can
    /// swap the address space atomically without &mut access.
    pub vm: AtomicSlot<Cap<AddressSpace>>,
    /// Signal-action table (CLONE_SIGHAND). `Arc` provides
    /// shared ownership across processes; the inner `SpinMutex`
    /// on `SigActionTable` handles per-entry concurrency.
    pub sig_actions: Arc<SigActionTable>,
}

pub struct ProcessPayload {
    /// Authoritative address-space slot for this process. Per Open Q #2
    /// (DECIDED 2026-05-06, `txdoc:EXEC-11-PHASE-6-ADDRESS-SPACE-VISIBILITY-BOUNDARY`),
    /// exec swaps this slot atomically: phase 6 stores the freshly built
    /// detached `Cap<AddressSpace>` while every other thread of the
    /// process group has already been zombified, and the previous
    /// `Cap` is returned for EBR-deferred drop. The `AtomicSlot`
    /// shape mirrors `cwd` and `current_payload` (the staging slot
    /// idiom). Initial state is always populated by
    /// `bootstrap_init_process` / `step_fork`; readers (the syscall
    /// dispatcher, the trap-shell aspace resolution, the page-fault
    /// driver) snapshot via `process.aspace_cap()` which clones the
    /// inner `Cap` out of the slot.
    /// Per-process resource frame. Holds the shared-ownership
    /// resources governed by clone(2) flags: the address space
    /// (CLONE_VM) and the signal-action table (CLONE_SIGHAND).
    /// v1: fields are inline on `ProcessPayload`. When cross-process
    /// sharing through `Shared<Frame>` lands, the frame will move
    /// to `Shared<Frame>` and the `share()` path will be wired
    /// in `step_fork`.
    pub(crate) frame: Frame,
    pub(crate) threads: ProcessThreads,
    /// Process-group-targeted pending signals. Day-1 collapses
    /// repeated posts (bitset, no per-occurrence queueing); a thread
    /// whose mask permits the signal will sweep it on its next
    /// delivery point.
    pub(crate) group_pending: PendingSignalQueue,
    /// Per-signum siginfo slots. Written by signal producers
    /// (post_signal via step_kill_process), read by signalfd
    /// and the future handler-delivery path. Phase I.
    pub(crate) siginfo_slots: crate::signal::SigInfoSlots,
    /// Per-process bus wire for signal-generated lifecycle events.
    ///
    /// `RawPort` (edge-triggered) per `SIGNAL_ATTACHMENTS_v1`:
    /// fires `SIGNAL_GENERATED` each time a catchable signal is
    /// delivered to this process. Native consumers (signalfd, pidfd,
    /// future timerfd) subscribe here; the signal shim also consumes
    /// this wire internally for handler-vs-default routing.
    ///
    /// Phase A (bus wire alignment): declared on ProcessPayload per
    /// spec; fired in `step_kill_process` after thread-eligibility
    /// post and signalfd notification. Signalfd migration from
    /// private subscription table to bus-subscriber is TODO.
    ///
    /// See: `txdoc:SIGNAL-ATTACHMENTS-CATALOG-SCHEMA-1`,
    /// `docs/design/04_process-signals/SIGNAL_ATTACHMENTS_v1.md`.
    pub(crate) signal_port: RawPort,
    /// Per-process credential **cap slot**. Mutated via
    /// `cred::step_setuid` / `cred::step_setgid` / ...; readers clone
    /// the cap via [`Self::cred_cap`] or snapshot the value via
    /// [`Self::cred`].
    ///
    /// PR-9 phase 5 (D5 Path A, 2026-05-11) — was `SpinMutex<Cred>`.
    /// `Cred` is now `ZoneAllocated`; each cred-mutator
    /// (`step_setuid`, ...) reads the current cap, derives a new
    /// `Cred` value, signs a fresh `Cap<Cred>`, and publishes via
    /// `AtomicSlot::swap`. The previous cap drops at the end of the
    /// mutator and the slab entry is EBR-retired once concurrent
    /// readers' guards complete. Atomicity becomes a single slot
    /// store (release/acquire); torn reads of partial cred state are
    /// structurally impossible.
    ///
    /// Initial state is always populated by `sign_process_payload`
    /// (`bootstrap_init_process` and `step_fork` both seed a fresh
    /// `Cap<Cred>`); the slot is never empty for a live payload.
    /// Field shape mirrors the existing
    /// `aspace: AtomicSlot<Cap<AddressSpace>>` precedent above.
    pub(crate) cred: AtomicSlot<Cap<Cred>>,
    /// Per-process namespace proxy. Immutable-after-publication bundle
    /// of namespace references per `NAMESPACE_VIEW_v1.md` §1.
    /// `AtomicSlot` allows clone/unshare/setns to publish a replacement
    /// bundle. Initial state is populated by `sign_process_payload`.
    ///
    /// Day-1: all namespace caps point at the init namespace.
    /// `mnt_ns` is deferred (`MountNamespace` bootstrap not yet wired).
    pub(crate) nsproxy: AtomicSlot<Cap<crate::process::nsproxy::NsProxy>>,
    /// Current working directory as a `DEntry` `Cap`.
    ///
    /// Spec note: `PROCESS_v1` §3 declares this as `Cap<RNode>` on a
    /// `Frame` container. The impl uses `Cap<DEntry>` because VFS's
    /// `ResolveCtx` already takes a `Cap<DEntry>` for cwd-bound
    /// path resolution, and `DEntry` carries the named-path edge
    /// needed to render `getcwd(2)` (walk `parent_hint` chain). The
    /// spec is amended in this pass to match the impl.
    ///
    /// `None` until a rootfs is mounted and `step_chdir` installs an
    /// initial cwd. Day-1 `bootstrap_init_process` leaves this slot
    /// empty; `step_getcwd` returns `None` for a process with no
    /// cwd installed.
    pub(crate) cwd: SpinMutex<Option<Cap<DEntry>>>,
    /// Sparse fd table keyed by `u32` fd value.
    ///
    /// **fd-ops Wave 1 (2026-05-07):** flipped from a fixed
    /// `[Option<Cap<OpenFile>>; FD_TABLE_SIZE]` array to a
    /// `BTreeMap<u32, Cap<OpenFile>>`. Closes the `FD_TABLE_SIZE = 8`
    /// ceiling that prevented shells from doing `>&100`-style fd
    /// redirection and lifts the artificial fd-31 limit on the
    /// CLOEXEC bitmap. The `BTreeMap` makes "lowest unused fd"
    /// lookup straightforward (walk keys looking for the first gap),
    /// matches `dup3(oldfd, newfd, _)`'s sparse-newfd semantic
    /// directly, and tracks exact memory rather than worst-case fd
    /// count. See plan §A and §"Open questions #1" (DECIDED).
    ///
    /// `step_fork` clones the entire map; each `Cap<OpenFile>` is
    /// `.clone()` so parent and child share the same `OpenFile` —
    /// `dup`-shape sharing (separate file description per fd) is a
    /// deferred follow-up.
    pub(crate) fds: SpinMutex<BTreeMap<u32, Cap<OpenFile>>>,
    /// Per-fd close-on-exec set. fd `i` is marked CLOEXEC iff
    /// `fd_cloexec.contains(&i)`; marked fds are closed by
    /// [`crate::process::execution::step_close_cloexec_fds`] during
    /// exec phase 7 (per `txdoc:EXEC-12-2-RESET-FDS-WITH-CLOEXEC`).
    ///
    /// **fd-ops Wave 1 (2026-05-07):** flipped from `AtomicU32` (which
    /// capped CLOEXEC tracking at fd 31) to `BTreeSet<u32>`. Now any
    /// `u32` fd may be CLOEXEC-marked; per fd-ops Wave 1 §B and
    /// §"Open questions #1" (DECIDED). The bitmap-vs-fd-table
    /// consistency burden is unchanged in shape (two separate
    /// containers for fd and cloexec); future migration to a single
    /// `OpenFile.flags.cloexec` source-of-truth is tracked as Open
    /// question #4 (deferred — `dup2`/`dup3` semantics are easier with
    /// a per-fd bit).
    ///
    /// Default empty (no fds CLOEXEC at process creation). `step_fork`
    /// clones the parent's set per Linux semantics (CLOEXEC is per-fd,
    /// copied across fork).
    pub(crate) fd_cloexec: SpinMutex<BTreeSet<u32>>,
    /// Per-process open-fd resource limit. Linux exposes this as
    /// `RLIMIT_NOFILE`; fork copies it and exec preserves it.
    pub(crate) rlimit_nofile_cur: AtomicU32,
    pub(crate) rlimit_nofile_max: AtomicU32,
    /// Base of the program-break (heap) region for this process.
    ///
    /// Set once at exec time (per `txdoc:VM-5-8-BRK`); never changes
    /// after that — `brk(2)` only moves `current_brk`. Default `0`
    /// indicates an unconfigured brk region (the syscall dispatcher
    /// treats this the same as "no brk available"). Bootstrap init's
    /// brk base is materialised by `bootstrap_init_process` per the
    /// Trio plan §"Cross-cutting risks #7".
    ///
    /// Stored as `AtomicU64` (not `SpinMutex<u64>`) because the field
    /// is effectively immutable after construction: only `bootstrap_
    /// init_process` and (eventually) `step_exec` assign it, and only
    /// once each. Readers (`brk` syscall) snapshot it under no
    /// coupling against `current_brk`.
    pub(crate) brk_base: AtomicU64,
    /// Current program break for this process.
    ///
    /// Mutated by every successful `brk(2)` syscall via
    /// `AddressSpace::brk_script` returning the new break. Bootstrap
    /// init starts at `current_brk == brk_base`. `step_fork` clones
    /// the value (each child has its own break point); the underlying
    /// VM mappings are cloned by the existing `AddressSpace::fork_aspace`
    /// path, so the child's brk region is materialised without
    /// re-running `brk_script`.
    pub(crate) current_brk: AtomicU64,
    /// Per-process file-creation mask (`umask(2)`).
    ///
    /// Slice 6 of the shell-prompt roadmap. Bits set in `umask` are
    /// **cleared** from the mode of newly created files / directories
    /// (POSIX `(mode & ~umask)`). Default `0o022` (matches Linux's
    /// `init`-inherited default — owner keeps full perms, group/other
    /// lose write). The kernel only honours the bottom 9 bits
    /// (`rwxrwxrwx`); `umask(2)` silently truncates the argument.
    ///
    /// Stored as `AtomicU16` because the value is mutated on every
    /// `umask(2)` syscall and read on every file-create path; a
    /// `SpinMutex<u16>` would be heavier than necessary for a 16-bit
    /// scalar with swap semantics.
    pub(crate) umask: AtomicU16,
    /// SysV semaphore undo records owned by this process.
    ///
    /// Per `docs/Txv3/08_SYSV_IPC_v1.md` §4.4 / IPC-3, `SEM_UNDO`
    /// state is process-local and process exit drains only this list.
    pub(crate) sem_undos: SpinMutex<BTreeMap<u32, SemUndo>>,
    /// Reactor wait source that fires when **any** child of this
    /// process zombifies (per `txdoc:PROCESS-WAIT-FAMILY-1`'s
    /// `children_state_channel` notion). Created at payload-sign time
    /// and registered with [`crate::wait_source::register_wait_channel`]
    /// so async script wrappers can `wait_on_token` against the
    /// returned id without holding a `Cap<ProcessIdentity>`.
    ///
    /// Pattern mirrors `TtyIdentity.wait_channel` /
    /// `wait_source_id` (see
    /// `crates/tx-subsystems/src/tty/structure/identity.rs`'s
    /// `TtyIdentity::new`) — the only other in-tree wait source
    /// today.
    ///
    /// Fire site: [`crate::process::execution::post_sigchld_to_parent`]
    /// fires this immediately after the SIGCHLD post once a child
    /// zombifies. Wave 1 of the fork/clone/wait4 slice wires the
    /// fire; the matching `sys_wait4` blocking-wait await arrives in
    /// Wave 2.
    ///
    /// Bit allocation: see [`EXIT_SOURCE_CHILD_ZOMBIFIED`].
    pub(crate) exit_source: Channel,
    /// Carrier id under which `exit_source` is registered with the
    /// global [`crate::wait_source`] resolver. Embedded in the
    /// `WaitToken` returned by
    /// [`ProcessIdentity::exit_source_wait_token`] so the syscall arm
    /// can park on the carrier without reaching the channel directly.
    ///
    /// Carrier-lifetime cleanup (release on payload drop) is tracked
    /// as Cross-cutting Risk #1 in the slice plan and not addressed
    /// in Wave 1; see plan §"Cross-cutting risks #1" for the
    /// follow-up.
    pub(crate) exit_source_id: u64,
    /// PR-3D-3 (D2/D4 coexistence). Per-process `WaitSource` minted at
    /// `sign_process_payload` time alongside the legacy
    /// `exit_source` Channel. Shares the same `WaitSourceId` (raw
    /// `u64` matches `exit_source_id`) so a v3 caller's
    /// `YieldShape::OnWaitSource { source: WaitSourceId(exit_source_id),
    /// .. }` round-trips cleanly to this source. Fired in parallel
    /// with the legacy Channel by `post_sigchld_to_parent` (the sole
    /// fire site). `notify` is idempotent in the "no subscribers,
    /// already-zombie process" sense — once the payload drops the
    /// `Arc<WaitSource>` is unreachable through this slot and any
    /// last fire was already issued under the live-payload guard
    /// inside `fire_exit_source`.
    pub(crate) exit_wait_source: Arc<WaitSource>,
    /// Bus-aligned readiness wire for process lifecycle events.
    ///
    /// `RawQueue` (level-triggered) per `SIGNAL_ATTACHMENTS_v1`:
    /// sets bits for zombie-children (`EXIT_SOURCE_CHILD_ZOMBIFIED`),
    /// future thread-stop events, and group-continue events. Native
    /// consumers (future signalfd, pidfd) subscribe to this wire;
    /// `wait4`/`waitid` consume the legacy `exit_source` Channel
    /// until the Channel → RawQueue migration completes.
    ///
    /// Phase A: declared alongside the legacy `exit_source` Channel.
    /// `fire_exit_source` sets bits on both mechanisms; native
    /// subscribers poll this wire, while the wait4 path still drains
    /// the Channel. Once all consumers migrate, the Channel is
    /// removed.
    ///
    /// Event bits use the same constants as the Channel
    /// (`EXIT_SOURCE_CHILD_ZOMBIFIED`).  See the top-level
    /// constant doc for how the wait4 arm builds its `WaitToken`.
    /// See: `txdoc:SIGNAL-ATTACHMENTS-CATALOG-SCHEMA-2`.
    pub(crate) exit_source_bus: RawQueue,
    /// Process command-line snapshot. Populated by `execve` at the
    /// point-of-no-return commit; read by procfs `/proc/<pid>/cmdline`.
    /// `None` for kernel threads and pre-exec processes.
    pub _cmdline: SpinMutex<Option<alloc::vec::Vec<u8>>>,
    /// Canonical executable DEntry. Set by `execve` to the resolved
    /// path of the loaded binary. Read by procfs `/proc/<pid>/exe`
    /// (symlink target) and `/proc/<pid>/stat`.
    /// `None` for kernel threads and pre-exec processes.
    pub _exe_file: SpinMutex<Option<Cap<DEntry>>>,
    /// Process short name (comm). Up to 15 bytes + NUL. Initialised
    /// from the executable basename at `execve`; can be changed via
    /// `prctl(PR_SET_NAME)`. Read by procfs `/proc/<pid>/stat`.
    pub _comm: SpinMutex<[u8; 16]>,
    pub(crate) thread_count: AtomicU32,
    pub(crate) group_exit: SpinMutex<Option<GroupExitState>>,
    pub vfork_done: AtomicBool,
    /// Waker for a vfork-parent that is parked in `sys_clone` waiting
    /// for this process to exec or exit.  Set by the parent before
    /// parking; taken and fired by the child's exec and exit paths.
    pub(crate) vfork_waiter: SpinMutex<Option<core::task::Waker>>,
}

impl ProcessPayload {
    /// Snapshot the current address-space `Cap` out of the
    /// `AtomicSlot<Cap<AddressSpace>>` slot. Panics if the slot is
    /// empty — by construction the initial state is always populated
    /// (`bootstrap_init_process` / `step_fork`) and the only mutator
    /// is exec's phase 6 store, which atomically swaps to a fresh
    /// `Cap` and never leaves the slot empty.
    /// Notify a vfork-parent that this child process has exec'd or
    /// exited.  Sets `vfork_done` and fires the stored waker (if any).
    pub fn notify_vfork_done(&self) {
        self.vfork_done.store(true, Ordering::Release);
        if let Some(waker) = self.vfork_waiter.lock().take() {
            waker.wake();
        }
    }

    /// Register a [`core::task::Waker`] to be fired when this process
    /// next reaches `notify_vfork_done`. Overwrites any prior waker.
    /// Used by the `CLONE_VFORK` parent-park loop in the syscall arm.
    pub fn store_vfork_waiter(&self, waker: core::task::Waker) {
        *self.vfork_waiter.lock() = Some(waker);
    }

    /// Snapshot the current address-space `Cap` out of the
    /// `AtomicSlot<Cap<AddressSpace>>` slot. Panics if the slot is
    /// empty — by construction the initial state is always populated
    /// (`bootstrap_init_process` / `step_fork`) and the only mutator
    /// is exec's phase 6 store, which atomically swaps to a fresh
    /// `Cap` and never leaves the slot empty.
    pub fn aspace_cap(&self) -> Cap<AddressSpace> {
        self.frame
            .vm
            .load()
            .expect("ProcessPayload.frame.vm slot is always populated")
    }

    /// Borrow the per-process action table.
    pub fn sig_actions(&self) -> &SigActionTable {
        &self.frame.sig_actions
    }

    /// Borrow the per-process group-pending queue.
    pub fn group_pending(&self) -> &PendingSignalQueue {
        &self.group_pending
    }

    /// Snapshot the current credential value. Returns a `Copy` so
    /// callers don't have to retain the cap; permissioned mutators
    /// (`cred::step_setuid` etc.) reserve+sign a fresh cap internally.
    ///
    /// PR-9 phase 5: under the new `AtomicSlot<Cap<Cred>>` shape,
    /// this loads the current cap, derefs through it to read the
    /// `Cred` value, and drops the cap clone on return — so the
    /// returned `Cred` is independent of the slot and safe to hold
    /// across `.await`. Equivalent semantics to the previous
    /// `SpinMutex<Cred>` snapshot.
    pub fn cred(&self) -> Cred {
        *self.cred_cap()
    }

    /// Capture a syscall-entry [`CredSnapshot`].
    ///
    /// Per `cred_service_v_1` §"In flight": scripts hold a by-value
    /// metadata copy of the credential, captured once at syscall entry.
    /// This is the `ProcessPayload`-side entry point producing that
    /// snapshot — one `AtomicSlot` load + cap deref + value-copy +
    /// drop, identical cost to [`Self::cred`] but typed as the
    /// architectural snapshot rather than a raw `Cred`.
    pub fn cred_snapshot(&self) -> CredSnapshot {
        CredSnapshot::from_cred(self.cred())
    }

    /// Snapshot the current `Cap<Cred>` out of the
    /// `AtomicSlot<Cap<Cred>>` slot. Panics if the slot is empty —
    /// by construction the initial state is always populated
    /// (`sign_process_payload`), and the only mutators are the
    /// cred-service `step_*` family which atomic-swap to a fresh
    /// `Cap` and never leave the slot empty.
    ///
    /// PR-9 phase 5: this is the cap-shaped accessor consumed by
    /// `SyscallCtx`-side `SubjectAuthority` construction. Mirrors
    /// `aspace_cap()` (already present for the `AddressSpace` slot
    /// per `txdoc:EXEC-11-PHASE-6-ADDRESS-SPACE-VISIBILITY-BOUNDARY`).
    pub fn cred_cap(&self) -> Cap<Cred> {
        self.cred
            .load()
            .expect("ProcessPayload.cred slot is always populated")
    }

    /// Snapshot the per-process namespace proxy. The returned `Cap`
    /// shares the same `NsProxy` bundle; clone/unshare/setns publish
    /// a replacement via `replace_nsproxy`.
    ///
    /// Day-1: most namespace caps point at init-namespace stubs. `mnt_ns`
    /// is `None` until rootfs mount bootstrap publishes a concrete mount
    /// namespace bundle.
    pub fn nsproxy_cap(&self) -> Cap<crate::process::nsproxy::NsProxy> {
        self.nsproxy
            .load()
            .expect("ProcessPayload.nsproxy slot is always populated")
    }

    /// Atomically install `new` as the current nsproxy cap and return
    /// the previously installed cap. Used by `step_clone_newipc` /
    /// `step_setns` to publish a replacement bundle.
    pub(crate) fn replace_nsproxy(
        &self,
        new: Cap<crate::process::nsproxy::NsProxy>,
    ) -> Cap<crate::process::nsproxy::NsProxy> {
        self.nsproxy
            .swap(Some(new))
            .expect("ProcessPayload.nsproxy slot is always populated")
    }

    /// Atomically install `new` as the current cred-cap and return
    /// the previously installed cap. Used by the 7 cred-mutator
    /// `step_*` functions and their test-only siblings.
    ///
    /// Per D5 Path A, the previous cap is returned (not dropped here)
    /// so the caller controls drop timing — drop typically happens
    /// at the end of the mutator's stack frame, releasing the
    /// retain-count on the prior slab entry; EBR reclaims when
    /// concurrent readers' guards exit.
    pub(crate) fn replace_cred(&self, new: Cap<Cred>) -> Cap<Cred> {
        self.cred
            .swap(Some(new))
            .expect("ProcessPayload.cred slot is always populated")
    }

    /// Snapshot the current working-directory `Cap<DEntry>` if one
    /// is installed. `None` for processes that haven't had a cwd
    /// set (init pre-rootfs).
    pub fn cwd(&self) -> Option<Cap<DEntry>> {
        self.cwd.lock().clone()
    }

    /// Snapshot the `Cap<OpenFile>` registered at fd `idx`, if any.
    ///
    /// Returns `None` for empty slots. Per fd-ops Wave 1 the underlying
    /// storage is a sparse `BTreeMap<u32, Cap<OpenFile>>`; any `u32`
    /// fd value is valid as a key.
    pub fn fd(&self, idx: u32) -> Option<Cap<OpenFile>> {
        self.fds.lock().get(&idx).cloned()
    }

    /// Install `file` at fd `idx`, returning the previously installed
    /// `Cap<OpenFile>` if any. Passing `file = None` removes the fd
    /// from the table (returns the previous occupant, if any).
    ///
    /// Per fd-ops Wave 1 the underlying storage is a sparse
    /// `BTreeMap<u32, Cap<OpenFile>>`; any `u32` fd value is valid as
    /// a key.
    ///
    /// Used by Phase 2a tests to manually wire the console as fd 1
    /// before dispatching `NR_WRITE`. Phase 3b's `init.rs` uses the
    /// same accessor to preopen fds 0/1/2.
    pub fn set_fd(&self, idx: u32, file: Option<Cap<OpenFile>>) -> Option<Cap<OpenFile>> {
        let mut slot = self.fds.lock();
        let previous = match file {
            Some(f) => slot.insert(idx, f),
            None => slot.remove(&idx),
        };
        drop(slot);
        if let Some(file) = &previous {
            decr_pipe_fd_ref(file);
        }
        previous
    }

    /// Remove every fd from this payload and return the detached table.
    ///
    /// Process exit must close all open fds before the payload itself
    /// becomes unreachable. In particular, pipe EOF/EPIPE publication is
    /// driven from fd-table accounting, so exit paths need a concrete
    /// drain rather than waiting for the whole payload to disappear
    /// later through EBR.
    pub(crate) fn drain_fds(&self) -> BTreeMap<u32, Cap<OpenFile>> {
        let drained = core::mem::take(&mut *self.fds.lock());
        for file in drained.values() {
            decr_pipe_fd_ref(file);
        }
        drained
    }

    /// Merge one SysV `SEM_UNDO` adjustment vector into this process's
    /// per-process undo list.
    pub(crate) fn record_sem_undo(&self, semid: u32, adjustments: Vec<i16>) {
        let mut undos = self.sem_undos.lock();
        undos
            .entry(semid)
            .and_modify(|u| {
                for (i, adj) in adjustments.iter().enumerate() {
                    if *adj != 0 && i < u.adjustments.len() {
                        u.adjustments[i] = u.adjustments[i].wrapping_add(*adj);
                    }
                }
            })
            .or_insert_with(|| SemUndo { semid, adjustments });
    }

    /// Drain this process's pending SysV `SEM_UNDO` records.
    pub(crate) fn drain_sem_undos(&self) -> BTreeMap<u32, SemUndo> {
        core::mem::take(&mut *self.sem_undos.lock())
    }

    /// Snapshot the entire fd table as a fresh `BTreeMap`. Each
    /// populated entry's `Cap<OpenFile>` is `.clone()`'d so the
    /// snapshot does not borrow the lock; callers can drop the result
    /// freely without touching the payload's storage. Used by
    /// `step_fork` to clone the parent's fd table into the child.
    pub(crate) fn snapshot_fds(&self) -> BTreeMap<u32, Cap<OpenFile>> {
        self.fds.lock().clone()
    }

    /// Clone the fd table for `fork`, accounting each inherited pipe
    /// endpoint as a new fd reference. Unlike [`Self::snapshot_fds`],
    /// this result is intended to be installed into another live
    /// `ProcessPayload`.
    pub(crate) fn clone_fds_for_fork(&self) -> BTreeMap<u32, Cap<OpenFile>> {
        let cloned = self.fds.lock().clone();
        for file in cloned.values() {
            incr_pipe_fd_ref(file);
        }
        cloned
    }

    /// Public fd-table snapshot (for procfs `/proc/<pid>/fd/`).
    pub fn open_fds(&self) -> BTreeMap<u32, Cap<OpenFile>> {
        self.snapshot_fds()
    }

    /// Process command-line (for `/proc/<pid>/cmdline`).
    pub fn cmdline(&self) -> Option<alloc::vec::Vec<u8>> {
        self._cmdline.lock().clone()
    }

    /// Executable DEntry (for `/proc/<pid>/exe` symlink target).
    pub fn exe_file(&self) -> Option<Cap<DEntry>> {
        self._exe_file.lock().clone()
    }

    /// EXEC Phase 5 — install the group-exit state that collapses every
    /// sibling thread of the calling process. Returns `true` if more
    /// than one thread was live and the group_exit slot was populated;
    /// `false` if the process was single-threaded and no collapse is
    /// needed. `txdoc:EXEC-10-COLLAPSE-OLD-AS-WORK`.
    pub fn install_exec_group_exit(&self) -> bool {
        let n = self
            .thread_count
            .load(core::sync::atomic::Ordering::Acquire);
        if n > 1 {
            *self.group_exit.lock() = Some(GroupExitState {
                status: ExitStatus::Exited(0),
                is_exec: true,
                remaining_threads: AtomicU32::new(n - 1),
            });
            true
        } else {
            false
        }
    }

    /// Collapse the thread group for exec. Marks all non-initiator
    /// threads zombie, clears the roster down to the initiator, and
    /// arms the exec GroupExit episode for the remaining exit path.
    ///
    /// Returns the number of threads removed from the live roster.
    /// No-op for zombies; returns `0` if the process is already
    /// single-threaded.
    pub fn collapse_threads_for_exec(&self, initiator: &Cap<ThreadIdentity>) -> Option<usize> {
        let siblings: Vec<Cap<ThreadIdentity>> = self
            .threads
            .snapshot()
            .into_iter()
            .filter(|thread| thread.key() != initiator.key())
            .collect();

        if siblings.is_empty() {
            return Some(0);
        }

        {
            let mut group_exit = self.group_exit.lock();
            *group_exit = Some(GroupExitState {
                status: ExitStatus::Exited(0),
                is_exec: true,
                remaining_threads: AtomicU32::new(siblings.len() as u32),
            });
        }

        for sibling in siblings {
            crate::thread_runtime::step_thread_exit(sibling, 0);
        }

        *self.group_exit.lock() = None;
        Some(self.threads.count())
    }

    /// Process short name comm (for `/proc/<pid>/stat`).
    pub fn comm(&self) -> [u8; 16] {
        *self._comm.lock()
    }

    /// Allocate the lowest unused fd ≥ `min` without installing
    /// anything. Walks the BTreeMap's sorted keys looking for the
    /// first gap at or above `min`.
    ///
    /// Per fd-ops Wave 1 §C: the new accessor surface needed by
    /// `sys_openat` (Wave 2), `sys_dup` / `sys_dup3` (Wave 4), and
    /// `sys_pipe2` (Wave 5).
    pub fn allocate_fd_at_least(&self, min: u32) -> u32 {
        let slot = self.fds.lock();
        let mut next = min;
        for &existing in slot.keys() {
            if existing < next {
                continue;
            }
            if existing == next {
                next = next.saturating_add(1);
            } else {
                break;
            }
        }
        next
    }

    pub fn rlimit_nofile(&self) -> (u32, u32) {
        (
            self.rlimit_nofile_cur.load(Ordering::Acquire),
            self.rlimit_nofile_max.load(Ordering::Acquire),
        )
    }

    pub fn set_rlimit_nofile(&self, cur: u32, max: u32) {
        self.rlimit_nofile_cur.store(cur, Ordering::Release);
        self.rlimit_nofile_max.store(max, Ordering::Release);
    }

    /// Read the close-on-exec bit for fd `idx`.
    ///
    /// Per fd-ops Wave 1 the underlying storage is a sparse
    /// `BTreeSet<u32>`; any `u32` fd value is valid (no fd-31
    /// ceiling).
    pub fn fd_cloexec_get(&self, idx: u32) -> bool {
        self.fd_cloexec.lock().contains(&idx)
    }

    /// Set or clear the close-on-exec bit for fd `idx`.
    ///
    /// Per fd-ops Wave 1 the underlying storage is a sparse
    /// `BTreeSet<u32>`; any `u32` fd value is valid (no fd-31
    /// ceiling).
    pub fn set_fd_cloexec(&self, idx: u32, value: bool) {
        let mut set = self.fd_cloexec.lock();
        if value {
            set.insert(idx);
        } else {
            set.remove(&idx);
        }
    }

    /// Snapshot the entire close-on-exec set as an owned
    /// `BTreeSet<u32>`. Used by
    /// [`crate::process::execution::step_close_cloexec_fds`] to walk
    /// every marked fd during exec phase 7.
    pub(crate) fn fd_cloexec_snapshot(&self) -> BTreeSet<u32> {
        self.fd_cloexec.lock().clone()
    }

    /// Clear the entire close-on-exec set. Used by
    /// [`crate::process::execution::step_close_cloexec_fds`] to clear
    /// the set after the close sweep so future `fcntl(F_SETFD)` calls
    /// start from a clean state.
    pub(crate) fn clear_fd_cloexec(&self) {
        self.fd_cloexec.lock().clear();
    }

    /// Read the program-break base address for this process. Returns
    /// `0` when no brk region is configured; bootstrap init seeds this
    /// per the Trio plan §"Cross-cutting risks #7".
    pub fn brk_base(&self) -> u64 {
        self.brk_base.load(Ordering::Acquire)
    }

    /// Read the current program break for this process. Returns `0`
    /// when no brk region is configured.
    pub fn current_brk(&self) -> u64 {
        self.current_brk.load(Ordering::Acquire)
    }

    /// Update the current program break. Called by the `brk(2)` syscall
    /// dispatcher after `AddressSpace::brk_script` returns the new
    /// break. The underlying mapping has already been materialised by
    /// `brk_script`; this just records the new top-of-heap.
    pub fn set_current_brk(&self, value: u64) {
        self.current_brk.store(value, Ordering::Release);
    }

    /// Read the per-process file-creation mask. Slice 6 of the
    /// shell-prompt roadmap.
    pub fn umask(&self) -> u16 {
        self.umask.load(Ordering::Acquire)
    }

    /// Atomically replace the per-process file-creation mask, returning
    /// the previous value. Argument is silently truncated to `0o777`
    /// (the bottom 9 bits — `rwxrwxrwx`); `umask(2)` ignores the
    /// kind / setuid / setgid / sticky bits per Linux semantics.
    pub fn swap_umask(&self, new: u16) -> u16 {
        self.umask.swap(new & 0o777, Ordering::AcqRel)
    }

    /// Borrow the per-process `exit_source` wait channel.
    ///
    /// Fired by [`crate::process::execution::post_sigchld_to_parent`]
    /// after the SIGCHLD producer post when a child zombifies. The
    /// matching syscall-side awaiter (Wave 2's `sys_wait4` blocking
    /// arm) parks on
    /// [`ProcessIdentity::exit_source_wait_token`] via
    /// [`crate::wait_source::wait_on_token`] rather than borrowing
    /// the channel directly.
    pub fn exit_source(&self) -> &Channel {
        &self.exit_source
    }

    /// Carrier id under which [`Self::exit_source`] is registered with
    /// the global wait-source resolver. Embedded in the
    /// `WaitToken` callers use to park.
    pub fn exit_source_id(&self) -> u64 {
        self.exit_source_id
    }

    /// PR-3D-3: per-process `WaitSource` for the new mailbox-based
    /// wake path. Returned as `&Arc<WaitSource>` so callers can clone
    /// the strong reference, register a `TaskMailbox` via
    /// `WaitSource::prepare(..).install_if(..)`, and hold the source
    /// alive across the wait window independent of payload lifetime.
    ///
    /// `WaitSource::id()` matches [`Self::exit_source_id`]: a v3
    /// caller's `YieldShape::OnWaitSource { source: WaitSourceId(id),
    /// .. }` where `id == exit_source_id()` resolves to this same
    /// source without going through the legacy `wait_source`
    /// resolver.
    pub fn exit_wait_source(&self) -> &Arc<WaitSource> {
        &self.exit_wait_source
    }
}

/// Process group: the unit of `setpgid`/`getpgid` and the eventual unit
/// of pgrp-wide signal fanout. Members tracked by `Weak` so a process can
/// die without serializing against the group; readers must `upgrade` to
/// observe live membership.
pub struct ProcessGroup {
    pub pgid: Pgid,
    pub(crate) session: Cap<Session>,
    pub(crate) members: ProcessGroupMembers,
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
        self.members.len()
    }
}

/// Session: container of process groups, holds the controlling-tty link.
/// `controlling_tty` is `Weak` so the TTY can be torn down (hangup) while
/// the session continues to exist with no controlling terminal.
pub struct Session {
    pub sid: Sid,
    pub(crate) controlling_tty: SpinMutex<Option<Weak<TtyIdentity>>>,
    /// Process groups whose `session` binding names this session.
    /// Per `PROCESS_v1` §2.4 (the spec calls this DLL `members`); we
    /// hold weak refs because retention is held by each pgrp's
    /// session binding.
    pub(crate) members: SessionMembers,
}

impl Session {
    /// Whether the session currently has a controlling TTY.
    pub fn has_controlling_tty(&self) -> bool {
        self.controlling_tty.lock().is_some()
    }

    /// Number of `Weak` member-pgrp slots currently held. Includes
    /// stale entries pointing to dropped groups.
    pub fn member_slot_count(&self) -> usize {
        self.members.len()
    }

    /// Snapshot the controlling TTY's `Cap` if it's still live. `None`
    /// if the session has no controlling TTY (TIOCNOTTY / hangup ran)
    /// or the TTY identity has been reclaimed.
    pub fn controlling_tty_cap(&self) -> Option<Cap<TtyIdentity>> {
        let weak = (*self.controlling_tty.lock())?;
        let guard = step_engine::guard();
        weak.upgrade(&guard)
    }

    /// Snapshot the foreground process group for this session.
    ///
    /// Per `OBJECT_PATTERN_FIXES_v1.md` OPA-3 (TTY-CTL-1) the
    /// authoritative foreground-pgrp slot lives on `TtyIdentity`'s
    /// `SessionPgrp`, not on `Session`. This helper hides the two-hop
    /// dereference (`Session.controlling_tty` Weak → `TtyIdentity` →
    /// `tty.foreground_pgrp_cap()` Weak) so process-side callers
    /// (session-leader-death SIGHUP cascade, orphan-pgrp detection,
    /// `tcgetpgrp`-style queries that come in via the process side)
    /// don't open-code the chain.
    ///
    /// Returns `None` if the session has no controlling tty, or the
    /// tty has no foreground pgrp installed, or either weak ref has
    /// been reclaimed.
    pub fn foreground_pgrp_cap(&self) -> Option<Cap<ProcessGroup>> {
        self.controlling_tty_cap()?.foreground_pgrp_cap()
    }

    /// Snapshot the session leader's process group, if still live.
    ///
    /// Day-1 `setsid` creates the leader pgrp with `pgid == sid`; this
    /// helper walks `session.members` to resolve that canonical group.
    /// Used by TTY hangup producers that need a typed session-leader pgrp
    /// target for SIGHUP fanout.
    pub fn leader_pgrp_cap(&self) -> Option<Cap<ProcessGroup>> {
        let guard = step_engine::guard();
        self.leader_pgrp_cap_with_guard(&guard)
    }

    pub(crate) fn leader_pgrp_cap_with_guard(
        &self,
        guard: &step_engine::Guard<'_>,
    ) -> Option<Cap<ProcessGroup>> {
        let leader_pgid = Pgid(self.sid.0);
        self.members
            .snapshot_live(guard)
            .into_iter()
            .find(|pgrp| pgrp.pgid == leader_pgid)
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
    type Policy = PayloadPolicy<Self>;
    fn zone() -> &'static Zone<Self> {
        &PROCESS_PAYLOAD_ZONE
    }
}

pub(crate) fn incr_pipe_fd_ref(file: &Cap<OpenFile>) {
    adjust_pipe_fd_ref(file, true);
}

fn decr_pipe_fd_ref(file: &Cap<OpenFile>) {
    adjust_pipe_fd_ref(file, false);
}

fn adjust_pipe_fd_ref(file: &Cap<OpenFile>, increment: bool) {
    if let Some((payload, side)) = file.pipe_endpoint() {
        match (side, increment) {
            (crate::pipe::PipeSide::Reader, true) => payload.incr_reader(),
            (crate::pipe::PipeSide::Writer, true) => payload.incr_writer(),
            (crate::pipe::PipeSide::Reader, false) => payload.decr_reader(),
            (crate::pipe::PipeSide::Writer, false) => payload.decr_writer(),
        }
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

// Simple atomic PID allocator. PID 1 is reserved for init; allocator
// PID/TID allocation moved to `crate::process::numbers`.

#[cfg(any(test, feature = "test-support"))]
pub use crate::process::numbers::reset_pid_counter_for_test;

#[cfg(test)]
mod subject_identity_tests;
