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
//!     │  └──Weak (members)───────┘ └──Weak (members)──┘ │
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
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use tx_substrate::zone::{Cap, PayloadCap, Weak, Zone, ZoneAllocated};
use tx_substrate::SpinMutex;

use crate::cred::{Cred, Gid, Uid};
use crate::signal::{PendingSignalQueue, SigActionTable};
use crate::thread_runtime::ThreadIdentity;
use crate::tty::structure::identity::{AtomicSlot, TtyIdentity};
use crate::vfs::{DEntry, OpenFile};
use crate::vm::AddressSpace;

/// Number of slots in the day-1 fixed-size fd table on `ProcessPayload`.
///
/// Per Trio plan §"Cross-cutting risks #6" and §"Open questions #3": a
/// fixed 8-slot vec is sufficient for the bootstrap smoke path
/// (fds 0/1/2 preopened via `open_console_for_init`). Growable
/// `BTreeMap`-shaped fd tables (full POSIX `dup3`/`fcntl(F_DUPFD)`)
/// are a follow-up beyond the trio.
pub const FD_TABLE_SIZE: usize = 8;

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
    /// Day-1 numeric status word, shell-convention: raw int for
    /// explicit exits, `128 + signum` for signal exits. POSIX
    /// `wait(2)` will replace this with the proper
    /// WIFEXITED / WIFSIGNALED encoding when the decoder lands.
    pub fn wait_status_word(self) -> i32 {
        match self {
            ExitStatus::Exited(s) => s,
            ExitStatus::Signaled(sig) => 128 + sig.raw() as i32,
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
    pub(crate) children: SpinMutex<Vec<Cap<ProcessIdentity>>>,
    pub(crate) pgrp: SpinMutex<Cap<ProcessGroup>>,
    /// Process-visible exit disposition. `Some` once the process has
    /// run `step_exit_group` / `step_exit_group_with_signal` (or the
    /// last-thread cascade has fired); otherwise `None`. Discriminates
    /// explicit-int exits from signal-driven termination per
    /// `PROCESS_v1` §6.2.
    pub(crate) exit_status: SpinMutex<Option<ExitStatus>>,
    pub(crate) payload: SpinMutex<Option<PayloadCap<ProcessPayload>>>,
}

impl ProcessIdentity {
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
        let guard = tx_substrate::epoch::guard();
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
        self.children.lock().len()
    }

    /// Snapshot the children list as owned `Cap`s. The returned
    /// vec is independent of the locked container; the lock is
    /// released before return. Includes zombie children (callers
    /// that need to skip zombies should `is_zombie()`-filter).
    pub fn children(&self) -> alloc::vec::Vec<Cap<ProcessIdentity>> {
        self.children.lock().clone()
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
        payload.aspace.swap(Some(new))
    }

    /// Snapshot the `Cap<OpenFile>` registered at fd `idx` on this
    /// process's payload. Returns `None` if the process is a zombie
    /// (no payload), `idx` is out of range for the day-1 fixed-size
    /// fd table, or the slot is empty.
    ///
    /// Used by the syscall dispatcher (`tx-shims::linux_syscall`) to
    /// resolve fds without holding the payload lock across a step's
    /// `.await`.
    pub fn fd(&self, idx: usize) -> Option<Cap<crate::vfs::OpenFile>> {
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
    /// `None`) for zombies and for indices outside the fixed-size
    /// table.
    ///
    /// Used by the syscall dispatcher's tests and (Phase 3b)
    /// `init.rs` to preopen fds 0/1/2 against
    /// `tx_fs::devfs::open_console_for_init()`.
    pub fn set_fd(
        &self,
        idx: usize,
        file: Option<Cap<crate::vfs::OpenFile>>,
    ) -> Option<Cap<crate::vfs::OpenFile>> {
        self.payload
            .lock()
            .as_ref()
            .and_then(|p| p.set_fd(idx, file))
    }

    /// Read the close-on-exec bit for fd `fd` on this process's
    /// payload. Returns `false` for zombies (no payload), for fd
    /// indices outside the day-1 `AtomicU32` bitmap (≥ 32), and for
    /// unmarked fds.
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
    /// process's payload. No-op for zombies and for fd indices outside
    /// the day-1 `AtomicU32` bitmap (≥ 32).
    ///
    /// Used by `tx-shims::linux_syscall::sys_fcntl` to service
    /// `F_SETFD`, and by future `sys_open` plumbing once `O_CLOEXEC`
    /// is threaded through.
    pub fn set_fd_cloexec(&self, fd: u32, value: bool) {
        if let Some(payload) = self.payload.lock().as_ref() {
            payload.set_fd_cloexec(fd, value);
        }
    }

    /// Internal: snapshot the full close-on-exec bitmap word. Returns
    /// `0` for zombies. Used by
    /// [`crate::process::execution::step_close_cloexec_fds`] during
    /// exec phase 7 to walk every set bit.
    pub(crate) fn fd_cloexec_word(&self) -> u32 {
        self.payload
            .lock()
            .as_ref()
            .map(|p| p.fd_cloexec_word())
            .unwrap_or(0)
    }

    /// Snapshot the current `SigDisposition` for `sig` from this
    /// process's per-process action table. Returns `None` for zombies
    /// (no payload). Used by the `rt_sigaction(2)` syscall dispatcher
    /// to read the live disposition without going through
    /// `step_sigaction` (which would mutate). Per `SIGNAL_v1` §15.1.
    pub fn sig_disposition(
        &self,
        sig: crate::signal::Signum,
    ) -> Option<crate::signal::SigDisposition> {
        self.payload
            .lock()
            .as_ref()
            .map(|p| p.sig_actions().get(sig))
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

    /// Number of currently-live threads owned by this process.
    /// Returns `0` for zombies.
    pub fn live_thread_count(&self) -> usize {
        self.payload
            .lock()
            .as_ref()
            .map(|p| p.threads.lock().len())
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
        let result = payload.threads.lock().get(idx).cloned();
        result
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
/// (`rlimits`, `fd_table`) land in follow-up passes without changing
/// the existing surface.
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
    pub(crate) aspace: AtomicSlot<Cap<AddressSpace>>,
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
    /// Day-1 fixed-size fd table (`FD_TABLE_SIZE` slots).
    ///
    /// Per Trio plan §"Cross-cutting risks #6": a stub fd table on
    /// `ProcessPayload` is needed by the Phase 2a syscall dispatcher
    /// (`write` resolves `fd → Cap<OpenFile>` against this slice). Phase
    /// 3b's `init.rs` preopens fds 0/1/2 via
    /// `tx_fs::devfs::open_console_for_init()`; `bootstrap_init_process`
    /// itself leaves every slot `None` because the devfs alias is not
    /// yet registered when the kernel reaches process bootstrap.
    ///
    /// `step_fork` clones the slice; each `Cap<OpenFile>` is `.clone()`
    /// so parent and child share the same `OpenFile` (no full POSIX
    /// `dup`-shape sharing). Per the same risk note: full
    /// `dup3`/`fcntl(F_DUPFD)` semantics are a follow-up.
    pub(crate) fds: SpinMutex<[Option<Cap<OpenFile>>; FD_TABLE_SIZE]>,
    /// Per-fd close-on-exec bitmap. Bit `i` set means fd `i` will be
    /// closed by [`crate::process::execution::step_close_cloexec_fds`]
    /// during exec phase 7 (per `txdoc:EXEC-12-2-RESET-FDS-WITH-CLOEXEC`).
    ///
    /// Per Open Q #4 (DECIDED 2026-05-06) the storage is `AtomicU32`,
    /// covering up to fd 31 — well above today's `FD_TABLE_SIZE = 8`
    /// and leaving headroom for the next-phase fd-table grow without
    /// reshaping the field. Once the fd table grows beyond 32 the
    /// natural upgrade is `[AtomicU64; N]`.
    ///
    /// Default `0` (no fds CLOEXEC at process creation). `step_fork`
    /// clones the parent's bits per Linux semantics (CLOEXEC is per-fd,
    /// copied across fork). Mutated via [`ProcessPayload::set_fd_cloexec`]
    /// from the syscall dispatcher's `fcntl(F_SETFD)` arm and from
    /// `sys_open` once `O_CLOEXEC` plumbing lands (see Wave 2 scope
    /// reduction note in [`crate::process::execution::step_close_cloexec_fds`]).
    pub(crate) fd_cloexec: AtomicU32,
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
}

impl ProcessPayload {
    /// Snapshot the current address-space `Cap` out of the
    /// `AtomicSlot<Cap<AddressSpace>>` slot. Panics if the slot is
    /// empty — by construction the initial state is always populated
    /// (`bootstrap_init_process` / `step_fork`) and the only mutator
    /// is exec's phase 6 store, which atomically swaps to a fresh
    /// `Cap` and never leaves the slot empty.
    pub fn aspace_cap(&self) -> Cap<AddressSpace> {
        self.aspace
            .load()
            .expect("ProcessPayload.aspace slot is always populated")
    }

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

    /// Snapshot the current working-directory `Cap<DEntry>` if one
    /// is installed. `None` for processes that haven't had a cwd
    /// set (init pre-rootfs).
    pub fn cwd(&self) -> Option<Cap<DEntry>> {
        self.cwd.lock().clone()
    }

    /// Snapshot the `Cap<OpenFile>` registered at fd `idx`, if any.
    ///
    /// Returns `None` for fd indices outside the day-1 fixed-size
    /// table (≥ `FD_TABLE_SIZE`) and for empty slots. Per Trio plan
    /// §"Cross-cutting risks #6", the fd table is a stub: callers that
    /// need full POSIX semantics will land beyond the trio.
    pub fn fd(&self, idx: usize) -> Option<Cap<OpenFile>> {
        if idx >= FD_TABLE_SIZE {
            return None;
        }
        self.fds.lock()[idx].clone()
    }

    /// Install `file` at fd `idx`, returning the previously installed
    /// `Cap<OpenFile>` if any. Returns `None` (and ignores the install)
    /// for fd indices outside the day-1 fixed-size table.
    ///
    /// Used by Phase 2a tests to manually wire the console as fd 1
    /// before dispatching `NR_WRITE`. Phase 3b's `init.rs` will use the
    /// same accessor to preopen fds 0/1/2.
    pub fn set_fd(&self, idx: usize, file: Option<Cap<OpenFile>>) -> Option<Cap<OpenFile>> {
        if idx >= FD_TABLE_SIZE {
            return None;
        }
        let mut slot = self.fds.lock();
        core::mem::replace(&mut slot[idx], file)
    }

    /// Snapshot the entire fd table as a fresh array. Each populated
    /// slot's `Cap<OpenFile>` is `.clone()`'d so the snapshot does not
    /// borrow the lock; callers can drop the result freely without
    /// touching the payload's storage. Used by `step_fork` to clone
    /// the parent's fd table into the child.
    pub(crate) fn snapshot_fds(&self) -> [Option<Cap<OpenFile>>; FD_TABLE_SIZE] {
        let slot = self.fds.lock();
        core::array::from_fn(|i| slot[i].clone())
    }

    /// Read the close-on-exec bit for fd `idx`. Out-of-range indices
    /// (≥ 32) return `false` — they cannot be marked under the day-1
    /// `AtomicU32` bitmap.
    pub fn fd_cloexec_get(&self, idx: u32) -> bool {
        if idx >= 32 {
            return false;
        }
        (self.fd_cloexec.load(Ordering::Acquire) & (1u32 << idx)) != 0
    }

    /// Set or clear the close-on-exec bit for fd `idx`. Out-of-range
    /// indices (≥ 32) are silently ignored — the day-1 `AtomicU32`
    /// bitmap covers fds 0..31.
    pub fn set_fd_cloexec(&self, idx: u32, value: bool) {
        if idx >= 32 {
            return;
        }
        let mask = 1u32 << idx;
        if value {
            self.fd_cloexec.fetch_or(mask, Ordering::AcqRel);
        } else {
            self.fd_cloexec.fetch_and(!mask, Ordering::AcqRel);
        }
    }

    /// Snapshot the entire close-on-exec bitmap as a `u32`. Used by
    /// [`crate::process::execution::step_close_cloexec_fds`] to walk
    /// every set bit during exec phase 7.
    pub(crate) fn fd_cloexec_word(&self) -> u32 {
        self.fd_cloexec.load(Ordering::Acquire)
    }

    /// Replace the entire close-on-exec bitmap. Used by `step_fork`
    /// to inherit the parent's CLOEXEC bits into the child, and by
    /// [`crate::process::execution::step_close_cloexec_fds`] to clear
    /// the bitmap after the close sweep.
    pub(crate) fn store_fd_cloexec_word(&self, word: u32) {
        self.fd_cloexec.store(word, Ordering::Release);
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
    /// Process groups whose `session` binding names this session.
    /// Per `PROCESS_v1` §2.4 (the spec calls this DLL `members`); we
    /// hold weak refs because retention is held by each pgrp's
    /// session binding.
    pub(crate) members: SpinMutex<Vec<Weak<ProcessGroup>>>,
}

impl Session {
    /// Whether the session currently has a controlling TTY.
    pub fn has_controlling_tty(&self) -> bool {
        self.controlling_tty.lock().is_some()
    }

    /// Number of `Weak` member-pgrp slots currently held. Includes
    /// stale entries pointing to dropped groups.
    pub fn member_slot_count(&self) -> usize {
        self.members.lock().len()
    }

    /// Snapshot the controlling TTY's `Cap` if it's still live. `None`
    /// if the session has no controlling TTY (TIOCNOTTY / hangup ran)
    /// or the TTY identity has been reclaimed.
    pub fn controlling_tty_cap(&self) -> Option<Cap<TtyIdentity>> {
        let weak = (*self.controlling_tty.lock())?;
        let guard = tx_substrate::epoch::guard();
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

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn reset_pid_counter_for_test() {
    NEXT_PID.store(2, Ordering::Relaxed);
}
