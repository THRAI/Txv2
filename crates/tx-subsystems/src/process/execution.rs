//! Process subsystem execution: fork, exit-group, setpgid, setsid, and
//! the internal `step_process_exit` last-thread cascade.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec;
use alloc::vec::Vec;

use tx_hal::{PmapIf, UserTrapContext};
use tx_substrate::zone::{self, Cap, OperationalCapExt, ZoneError};
use tx_substrate::SpinMutex;

use crate::cred::Cred;
use crate::process::structure::{
    allocate_pid, ExitStatus, Pgid, Pid, ProcessGroup, ProcessIdentity, ProcessPayload, Session,
    Sid,
};
use crate::signal::{PendingSignalQueue, SigActionTable};
use crate::thread_runtime::execution::set_thread_zombie;
use crate::thread_runtime::structure::{allocate_tid, ThreadIdentity, ThreadPayload};
use crate::vfs::OpenFile;
use crate::vm::{AddressSpace, VmMapError};

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
/// `tx_substrate::AtomicSlot<T>` is a subsystem-internal change.
static INIT_PROCESS: SpinMutex<Option<Cap<ProcessIdentity>>> = SpinMutex::new(None);

/// Snapshot the global init handle. Returns `None` before
/// `bootstrap_init_process` has run (test pre-bootstrap; boot-time
/// pre-process-init). The returned `Cap` is a clone — caller drops
/// freely without touching the global slot.
pub fn init_process() -> Option<Cap<ProcessIdentity>> {
    INIT_PROCESS.lock().clone()
}

/// Resolve `pid` to a `Cap<ProcessIdentity>` by walking the
/// process tree rooted at init.
///
/// Slice 7 of the shell-prompt roadmap (2026-05-07) introduces this
/// resolver to back `kill(pid, sig)`. Day-1 has no global pid →
/// Cap<ProcessIdentity> registry — every process is reachable from
/// init via the `children` vectors that `step_fork` pushes onto the
/// parent. The walk visits the init root then recurses through each
/// `payload.children` snapshot until either the matching pid is found
/// or every node has been visited.
///
/// Live and zombie processes alike are visited (zombies remain in
/// `parent.children` until reaped per §8.5). Returns `None` if no
/// process in the tree carries `pid`. Returns `None` before
/// `bootstrap_init_process` has run.
///
/// The walk takes per-process `payload.children` snapshots (`.clone()`
/// of the `Vec<Cap<ProcessIdentity>>` under the `SpinMutex`), so the
/// children lock is released before recursion and never held across
/// callees.
///
/// O(n) in the number of live + zombie processes — acceptable for
/// day-1 where process counts stay small. A future global pid table
/// (`TODO(phase-pid-resolver)`) would replace this with O(1) lookup.
pub fn process_by_pid(pid: Pid) -> Option<Cap<ProcessIdentity>> {
    let init = init_process()?;
    walk_process_tree(&init, pid)
}

fn walk_process_tree(node: &Cap<ProcessIdentity>, pid: Pid) -> Option<Cap<ProcessIdentity>> {
    if node.pid == pid {
        return Some(node.clone());
    }
    let children = node.children.lock().clone();
    for child in children {
        if let Some(found) = walk_process_tree(&child, pid) {
            return Some(found);
        }
    }
    None
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
    let payload = sign_process_payload(
        aspace,
        vec![leader],
        Cred::root(),
        None,
        BTreeMap::new(),
        BTreeSet::new(),
        BOOTSTRAP_BRK_BASE,
        BOOTSTRAP_BRK_BASE,
        // Slice 6 of the shell-prompt roadmap. init's file-creation
        // mask defaults to `0o022` per Linux convention; children
        // inherit through `step_fork`'s umask thread-through.
        0o022,
    )?;
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
        parent_cred,
        parent_cwd,
        parent_fds,
        parent_fd_cloexec,
        parent_brk_base,
        parent_current_brk,
        parent_umask,
    ) = {
        let payload_guard = parent.payload.lock();
        let payload = payload_guard.as_ref().ok_or(ForkError::ParentZombie)?;
        (
            payload.aspace_cap(),
            payload.cred(),
            payload.cwd(),
            payload.snapshot_fds(),
            payload.fd_cloexec_snapshot(),
            payload.brk_base(),
            payload.current_brk(),
            payload.umask(),
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
    // Phase-2 concern. Per Linux semantics CLOEXEC is per-fd and
    // copied across fork — the child sees the parent's snapshot at
    // fork time; subsequent `fcntl(F_SETFD)` calls in either parent
    // or child do not affect the other.
    let payload = sign_process_payload(
        child_aspace_cap,
        vec![leader],
        parent_cred,
        parent_cwd,
        parent_fds,
        parent_fd_cloexec,
        parent_brk_base,
        parent_current_brk,
        parent_umask,
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

/// Seed the child leader thread's `saved_user_context` from the
/// parent's snapshot at clone time.
///
/// Linux fork/clone ABI: the child returns from the clone syscall
/// with the parent's GPRs *except* `a0 = 0`, and resumes at the
/// instruction *after* the trapping `ecall`. RV64-specific:
///
/// - `a0` lives in `regs[10]` (RV64 ABI),
/// - `ecall` is exactly 4 bytes (RV32I/RV64I base ISA — there is no
///   `c.ecall` compressed form), so the child's resume address is
///   `parent_user_ctx.pc + 4`.
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
/// 2. Overwrite `regs[10] = 0` (RV64 a0).
/// 3. Overwrite `pc = parent_user_ctx.pc + 4` (skip past `ecall`).
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
/// RV64-ABI knowledge (the `+ 4` skip and the `regs[10]` index)
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
pub fn seed_child_leader_context(
    child_thread: &Cap<ThreadIdentity>,
    parent_user_ctx: &UserTrapContext,
) {
    // (1) Clone the parent context.
    let mut child_ctx = *parent_user_ctx;
    // (2) RV64 a0 = 0: child's clone-syscall return value.
    child_ctx.regs[10] = 0;
    // (3) PC already points past `ecall`: the trap shell
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

    // (4) Store on the leader thread's payload. payload_cap == None
    // here means a freshly forked thread already lost its payload,
    // which is a kernel-invariant violation: step_fork's post-condition
    // is exactly that the child leader is live-with-payload.
    let payload = child_thread
        .payload_cap()
        .expect("seed_child_leader_context: fresh child thread missing payload");
    payload.store_saved_user_context(Some(child_ctx));
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
/// remaining §7.3.3 phase-5 cascade (SIGCHLD to parent, `exit_source`
/// wake, full reparenting-into-init, orphan-pgrp SIGHUP,
/// session-leader controlling-tty hangup per §8.3) lands incrementally:
/// SIGCHLD post is wired via `post_sigchld_to_parent`, and the Wave 1
/// fork/clone/wait4 slice (2026-05-06) added the `exit_source` fire
/// alongside the SIGCHLD post for parent-side wake.
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
/// Wave 1 of the fork/clone/wait4 slice (2026-05-06): also fires the
/// parent's per-process `exit_source` wait channel
/// (`EXIT_SOURCE_CHILD_ZOMBIFIED` bit) so a parent parked on
/// `sys_wait4` (Wave 2) wakes when any child zombifies. Per
/// `txdoc:PROCESS-WAIT-FAMILY-1` and the spec's
/// "Block until a child's state changes" arm.
///
/// No-op for processes with no parent: bootstrap init (never had one)
/// and orphans whose parent has already exited and severed them.
/// Also no-op if the parent is itself a zombie — `step_kill_process`
/// returns `NoLiveThread` and `fire_exit_source` returns `0` (no
/// payload to fire through). The `exit_source` fire is harmless when
/// no awaiter is parked (Channel::fire returns 0).
///
/// `siginfo` is not yet wired (no `SigInfo` type in day-1 signal
/// surface); spec §7.3.3 phase 5 will populate `si_pid`, `si_uid`,
/// `si_code` (`CLD_EXITED` / `CLD_KILLED` / `CLD_DUMPED`), and
/// `si_status` when the siginfo carrier lands.
fn post_sigchld_to_parent(process: &Cap<ProcessIdentity>) {
    if let Some(parent) = process.parent_cap() {
        let _ = crate::signal::step_kill_process(&parent, crate::signal::Signum::SIGCHLD);
        // Fire the parent's exit_source. A zombie parent has no payload
        // and `fire_exit_source` returns 0 — no panic, no double-fire.
        let _ = parent.fire_exit_source(tx_reactor::wait::Mask::from_bits(
            crate::process::structure::EXIT_SOURCE_CHILD_ZOMBIFIED,
        ));
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
    let Ok(payload) = target.upgrade_operational() else {
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
    let payload = target.upgrade_operational().ok()?;
    let cwd = payload.cwd.lock().clone()?;
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

// --- exec phase-7 commit helpers (post-PoNR, infallible) -------------
//
// The three steps below materialise the per-process commits exec phase 7
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

/// Close every fd whose CLOEXEC bit is set on `process`, then clear
/// the close-on-exec set.
///
/// Per `txdoc:EXEC-12-2-RESET-FDS-WITH-CLOEXEC` and the Wave 2 plan's
/// Part 1 P1 sub-item. Walks the per-process CLOEXEC `BTreeSet<u32>`
/// snapshot; for each marked fd, drops the fd via the existing
/// `set_fd(i, None)` accessor — the resulting `Cap<OpenFile>` `Drop`
/// runs the close per `txdoc:VFS-CHECKS-WALKER-MODES-1`. Any
/// `OpenFile::Drop` side-effects (eventual file-flush etc.) are NOT
/// awaited here: the EBR machinery handles deferred drop, and exec's
/// post-PoNR commit cannot await.
///
/// Infallible — by EXEC-PONR. No-op for zombies (no payload to sweep).
/// fd-ops Wave 1 (2026-05-07): the CLOEXEC set's fd-31 ceiling has been
/// removed alongside the fd table's 8-slot ceiling; any `u32` fd may
/// be marked.
pub fn step_close_cloexec_fds(process: &Cap<ProcessIdentity>) {
    let cloexec = process.fd_cloexec_snapshot();
    if cloexec.is_empty() {
        return;
    }
    for fd in cloexec {
        // Drop via the existing accessor; the previous `Cap` (if any)
        // is returned for EBR-deferred drop. We discard it here — the
        // slot is now empty, the fd is closed.
        let _ = process.set_fd(fd, None);
    }
    // Clear the set wholesale: every previously-marked fd is now
    // closed; future fcntl(F_SETFD) calls start from a clean state.
    process.clear_fd_cloexec();
}

/// Reset every user-installed signal disposition on `process` to
/// `SigDisposition::Default`, preserving `Default` and `Ignore` slots.
///
/// Thin Phase-5 wrapper around
/// [`crate::signal::SigActionTable::step_reset_for_exec`] (Wave 2 P2)
/// that lets the exec script (`tx-scripts::process::exec`) reach the
/// per-process action table without touching the `pub(crate)` payload
/// field on [`ProcessIdentity`]. Per
/// `txdoc:EXEC-12-3-RESET-SIGNAL-DISPOSITIONS` and `SIGNAL_v1` §15.2:
/// exec resets handlers but does NOT clear pending signals or
/// SIG_IGN dispositions.
///
/// Infallible — by EXEC-PONR. No-op for zombies (no payload).
pub fn step_reset_signal_dispositions_for_exec(process: &Cap<ProcessIdentity>) {
    if let Some(payload) = process.payload.lock().as_ref() {
        payload.sig_actions().step_reset_for_exec();
    }
}

/// Install `new_brk_base` as both the brk base and the current brk
/// for `process`. Per `txdoc:EXEC-12-4-INSTALL-BRK` and the Wave 2
/// plan's Part 1 P3 sub-item.
///
/// The exec script (Part 5) computes `new_brk_base` from the image
/// plan: typically the highest LOAD segment's `vaddr + memsz`,
/// page-rounded up. Storing the same value into both fields seeds the
/// process at "no heap allocated yet" — `brk(2)` with a request above
/// `current_brk` then grows the heap on demand.
///
/// Infallible — by EXEC-PONR. No-op for zombies (no payload to seed).
pub fn step_install_brk_for_exec(process: &Cap<ProcessIdentity>, new_brk_base: u64) {
    if let Some(payload) = process.payload.lock().as_ref() {
        payload
            .brk_base
            .store(new_brk_base, core::sync::atomic::Ordering::Release);
        payload
            .current_brk
            .store(new_brk_base, core::sync::atomic::Ordering::Release);
    }
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

#[allow(clippy::too_many_arguments)]
fn sign_process_payload(
    aspace: Cap<AddressSpace>,
    threads: Vec<Cap<ThreadIdentity>>,
    cred: Cred,
    cwd: Option<Cap<crate::vfs::DEntry>>,
    fds: BTreeMap<u32, Cap<OpenFile>>,
    fd_cloexec: BTreeSet<u32>,
    brk_base: u64,
    current_brk: u64,
    umask: u16,
) -> Result<tx_substrate::zone::PayloadCap<ProcessPayload>, ZoneError> {
    use tx_reactor::wait::Channel;
    use tx_substrate::AtomicSlot;
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

    // Allocate a fresh `exit_source` Channel per `ProcessPayload` and
    // register it with the global wait-source resolver so async
    // awaiters can `wait_on_token` against the returned id without
    // holding a `Cap<ProcessIdentity>`. Pattern mirrors
    // `TtyIdentity::new` — the only other in-tree wait source today
    // (`crates/tx-subsystems/src/tty/structure/identity.rs`).
    //
    // Carrier-lifetime cleanup (release on payload drop) is tracked
    // as Cross-cutting Risk #1 in the slice plan and is deferred
    // beyond Wave 1.
    let exit_source = Channel::new();
    let exit_source_id = crate::wait_source::register_wait_channel(exit_source.clone());
    // PR-3D-3 (D2/D4 coexistence). Per-process `WaitSource` shares the
    // same `u64` namespace as the legacy `exit_source` channel so a
    // `WaitSourceId` stamped into `YieldShape::OnWaitSource` lands at
    // both ends (legacy resolver + new `Arc<WaitSource>` slot).
    let exit_wait_source = alloc::sync::Arc::new(tx_substrate::wake::WaitSource::new(
        tx_substrate::step_v3::WaitSourceId::new(exit_source_id),
    ));

    let res = zone::reserve_for::<ProcessPayload>()?;
    let cap = zone::sign_for(
        res,
        ProcessPayload {
            aspace: aspace_slot,
            threads: SpinMutex::new(threads),
            sig_actions: SigActionTable::new(),
            group_pending: PendingSignalQueue::new(),
            cred: cred_slot,
            cwd: SpinMutex::new(cwd),
            fds: SpinMutex::new(fds),
            fd_cloexec: SpinMutex::new(fd_cloexec),
            brk_base: core::sync::atomic::AtomicU64::new(brk_base),
            current_brk: core::sync::atomic::AtomicU64::new(current_brk),
            // Slice 6 of the shell-prompt roadmap. Per-process
            // file-creation mask. `bootstrap_init_process` seeds with
            // the Linux default `0o022` (owner keeps full perms,
            // group/other lose write); `step_fork` propagates the
            // parent's umask through this argument (umask is
            // per-process, copied across fork). `step_exec` preserves
            // the umask (umask survives `exec` per POSIX).
            umask: core::sync::atomic::AtomicU16::new(umask & 0o777),
            exit_source,
            exit_source_id,
            exit_wait_source,
        },
    );
    Ok(tx_substrate::zone::PayloadCap::from_cap(cap))
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
    let sibling = sign_thread(target.downgrade())?;
    if let Some(payload) = target.payload.lock().as_ref() {
        payload.threads.lock().push(sibling.clone());
    }
    Ok(sibling)
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
    pub _pmap: core::marker::PhantomData<P>,
}

impl<'a, P: PmapIf, I: tx_substrate::step_v3::SubjectIdentity>
    tx_substrate::step_v3::StepOp<I> for ForkOp<'a, P>
{
    type Output = Result<Cap<ProcessIdentity>, ForkError>;
    type Progress = tx_substrate::step_v3::NoProgress;
    fn step(
        &mut self,
        _ctx: &mut tx_substrate::step_v3::ScriptCtx<I>,
    ) -> tx_substrate::step_v3::StepOutcome<Self::Output, Self::Progress> {
        tx_substrate::step_v3::StepOutcome::Done(step_fork::<P>(self.parent))
    }
}

/// `StepOp` wrap of [`step_exit_group`].
pub struct ExitGroupOp<'a> {
    pub process: &'a Cap<ProcessIdentity>,
    pub status: ExitStatus,
}

impl<'a, I: tx_substrate::step_v3::SubjectIdentity>
    tx_substrate::step_v3::StepOp<I> for ExitGroupOp<'a>
{
    type Output = ();
    type Progress = tx_substrate::step_v3::NoProgress;
    fn step(
        &mut self,
        _ctx: &mut tx_substrate::step_v3::ScriptCtx<I>,
    ) -> tx_substrate::step_v3::StepOutcome<Self::Output, Self::Progress> {
        step_exit_group(self.process, self.status);
        tx_substrate::step_v3::StepOutcome::Done(())
    }
}

/// `StepOp` wrap of [`step_exit_group_with_signal`].
pub struct ExitGroupWithSignalOp<'a> {
    pub process: &'a Cap<ProcessIdentity>,
    pub sig: crate::signal::Signum,
}

impl<'a, I: tx_substrate::step_v3::SubjectIdentity>
    tx_substrate::step_v3::StepOp<I> for ExitGroupWithSignalOp<'a>
{
    type Output = ();
    type Progress = tx_substrate::step_v3::NoProgress;
    fn step(
        &mut self,
        _ctx: &mut tx_substrate::step_v3::ScriptCtx<I>,
    ) -> tx_substrate::step_v3::StepOutcome<Self::Output, Self::Progress> {
        step_exit_group_with_signal(self.process, self.sig);
        tx_substrate::step_v3::StepOutcome::Done(())
    }
}

/// `StepOp` wrap of [`step_waitpid_nohang`]. Returns the result type
/// `Result<(Pid, ExitStatus), WaitError>` via `StepOutcome::Done` so
/// callers can inspect both the success tuple and WNOHANG-empty signal.
pub struct WaitpidNohangOp<'a> {
    pub parent: &'a Cap<ProcessIdentity>,
    pub target: WaitTarget,
}

impl<'a, I: tx_substrate::step_v3::SubjectIdentity>
    tx_substrate::step_v3::StepOp<I> for WaitpidNohangOp<'a>
{
    type Output = Result<(Pid, ExitStatus), WaitError>;
    type Progress = tx_substrate::step_v3::NoProgress;
    fn step(
        &mut self,
        _ctx: &mut tx_substrate::step_v3::ScriptCtx<I>,
    ) -> tx_substrate::step_v3::StepOutcome<Self::Output, Self::Progress> {
        tx_substrate::step_v3::StepOutcome::Done(step_waitpid_nohang(self.parent, self.target))
    }
}

/// `StepOp` wrap of [`step_chdir`].
pub struct ChdirOp<'a> {
    pub target: &'a Cap<ProcessIdentity>,
    pub new_cwd: Cap<crate::vfs::DEntry>,
}

impl<'a, I: tx_substrate::step_v3::SubjectIdentity>
    tx_substrate::step_v3::StepOp<I> for ChdirOp<'a>
{
    type Output = ChdirOutcome;
    type Progress = tx_substrate::step_v3::NoProgress;
    fn step(
        &mut self,
        _ctx: &mut tx_substrate::step_v3::ScriptCtx<I>,
    ) -> tx_substrate::step_v3::StepOutcome<Self::Output, Self::Progress> {
        tx_substrate::step_v3::StepOutcome::Done(step_chdir(self.target, self.new_cwd.clone()))
    }
}

/// `StepOp` wrap of [`step_getcwd`].
pub struct GetcwdOp<'a> {
    pub target: &'a Cap<ProcessIdentity>,
}

impl<'a, I: tx_substrate::step_v3::SubjectIdentity>
    tx_substrate::step_v3::StepOp<I> for GetcwdOp<'a>
{
    type Output = Option<alloc::vec::Vec<u8>>;
    type Progress = tx_substrate::step_v3::NoProgress;
    fn step(
        &mut self,
        _ctx: &mut tx_substrate::step_v3::ScriptCtx<I>,
    ) -> tx_substrate::step_v3::StepOutcome<Self::Output, Self::Progress> {
        tx_substrate::step_v3::StepOutcome::Done(step_getcwd(self.target))
    }
}

/// `StepOp` wrap of [`step_setpgid`].
pub struct SetpgidOp<'a> {
    pub target: &'a Cap<ProcessIdentity>,
    pub new_pgid: Pgid,
}

impl<'a, I: tx_substrate::step_v3::SubjectIdentity>
    tx_substrate::step_v3::StepOp<I> for SetpgidOp<'a>
{
    type Output = Result<(), SetpgidError>;
    type Progress = tx_substrate::step_v3::NoProgress;
    fn step(
        &mut self,
        _ctx: &mut tx_substrate::step_v3::ScriptCtx<I>,
    ) -> tx_substrate::step_v3::StepOutcome<Self::Output, Self::Progress> {
        tx_substrate::step_v3::StepOutcome::Done(step_setpgid(self.target, self.new_pgid))
    }
}

/// `StepOp` wrap of [`step_setsid`].
pub struct SetsidOp<'a> {
    pub target: &'a Cap<ProcessIdentity>,
}

impl<'a, I: tx_substrate::step_v3::SubjectIdentity>
    tx_substrate::step_v3::StepOp<I> for SetsidOp<'a>
{
    type Output = Result<Sid, SetsidError>;
    type Progress = tx_substrate::step_v3::NoProgress;
    fn step(
        &mut self,
        _ctx: &mut tx_substrate::step_v3::ScriptCtx<I>,
    ) -> tx_substrate::step_v3::StepOutcome<Self::Output, Self::Progress> {
        tx_substrate::step_v3::StepOutcome::Done(step_setsid(self.target))
    }
}

/// `StepOp` wrap of [`step_close_cloexec_fds`].
pub struct CloseCloexecFdsOp<'a> {
    pub process: &'a Cap<ProcessIdentity>,
}

impl<'a, I: tx_substrate::step_v3::SubjectIdentity>
    tx_substrate::step_v3::StepOp<I> for CloseCloexecFdsOp<'a>
{
    type Output = ();
    type Progress = tx_substrate::step_v3::NoProgress;
    fn step(
        &mut self,
        _ctx: &mut tx_substrate::step_v3::ScriptCtx<I>,
    ) -> tx_substrate::step_v3::StepOutcome<Self::Output, Self::Progress> {
        step_close_cloexec_fds(self.process);
        tx_substrate::step_v3::StepOutcome::Done(())
    }
}

/// `StepOp` wrap of [`step_reset_signal_dispositions_for_exec`].
pub struct ResetSignalDispositionsForExecOp<'a> {
    pub process: &'a Cap<ProcessIdentity>,
}

impl<'a, I: tx_substrate::step_v3::SubjectIdentity>
    tx_substrate::step_v3::StepOp<I> for ResetSignalDispositionsForExecOp<'a>
{
    type Output = ();
    type Progress = tx_substrate::step_v3::NoProgress;
    fn step(
        &mut self,
        _ctx: &mut tx_substrate::step_v3::ScriptCtx<I>,
    ) -> tx_substrate::step_v3::StepOutcome<Self::Output, Self::Progress> {
        step_reset_signal_dispositions_for_exec(self.process);
        tx_substrate::step_v3::StepOutcome::Done(())
    }
}

/// `StepOp` wrap of [`step_install_brk_for_exec`].
pub struct InstallBrkForExecOp<'a> {
    pub process: &'a Cap<ProcessIdentity>,
    pub new_brk_base: u64,
}

impl<'a, I: tx_substrate::step_v3::SubjectIdentity>
    tx_substrate::step_v3::StepOp<I> for InstallBrkForExecOp<'a>
{
    type Output = ();
    type Progress = tx_substrate::step_v3::NoProgress;
    fn step(
        &mut self,
        _ctx: &mut tx_substrate::step_v3::ScriptCtx<I>,
    ) -> tx_substrate::step_v3::StepOutcome<Self::Output, Self::Progress> {
        step_install_brk_for_exec(self.process, self.new_brk_base);
        tx_substrate::step_v3::StepOutcome::Done(())
    }
}

#[cfg(test)]
mod step_op_wraps {
    //! PR-2 StepOp wrap tests (process::execution scope).
    //!
    //! Each test exercises one `*Op` wrap end-to-end: build the op with
    //! a fixture, drive `.step(&mut ScriptCtx)`, assert the outcome
    //! variant shape. Coverage of the underlying step-fn semantics
    //! lives in `process::tests`; the value here is the compile-check
    //! plus a smoke that the wrap delegates with the expected arg
    //! plumbing.
    use super::*;
    use crate::process::structure::reset_pid_counter_for_test;
    use crate::signal::Signum;
    use crate::test_support::EPOCH_TEST_LOCK;
    use crate::thread_runtime::structure::reset_tid_counter_for_test;
    use crate::vm::{AddressSpace, TestPmap};
    use crate::zones;
    use tx_substrate::step_v3::{ScriptCtx, StepOp, StepOutcome};
    use tx_substrate::testing::init_host_for_test_once;

    fn setup() -> std::sync::MutexGuard<'static, ()> {
        let guard = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        init_host_for_test_once();
        let _ = zones::register_all();
        let _ = tx_substrate::epoch::drain_with_budget(usize::MAX);
        let _ = tx_substrate::epoch::drain_with_budget(usize::MAX);
        reset_pid_counter_for_test();
        reset_tid_counter_for_test();
        reset_init_process_for_test();
        guard
    }

    fn fresh_aspace() -> Cap<AddressSpace> {
        AddressSpace::new_cap_for_platform::<TestPmap>().expect("fresh aspace")
    }

    fn bootstrap() -> Cap<ProcessIdentity> {
        bootstrap_init_process(fresh_aspace()).expect("bootstrap init")
    }

    #[test]
    fn fork_op_delegates_to_step_fork() {
        let _g = setup();
        let parent = bootstrap();
        let mut op = ForkOp::<TestPmap> {
            parent: &parent,
            _pmap: core::marker::PhantomData,
        };
        let mut ctx = ScriptCtx::<tx_substrate::step_v3::ProcessIdentity>::new();
        let outcome = op.step(&mut ctx);
        match outcome {
            StepOutcome::Done(result) => {
                let child = result.expect("step_fork should succeed");
                assert_ne!(child.pid, parent.pid);
            }
            _ => panic!("expected Done(_), got non-Done outcome"),
        }
    }

    #[test]
    fn exit_group_op_delegates_to_step_exit_group() {
        let _g = setup();
        let parent = bootstrap();
        let child = step_fork::<TestPmap>(&parent).expect("fork");
        let mut op = ExitGroupOp {
            process: &child,
            status: ExitStatus::Exited(0),
        };
        let mut ctx = ScriptCtx::<tx_substrate::step_v3::ProcessIdentity>::new();
        let outcome = op.step(&mut ctx);
        assert_eq!(outcome, StepOutcome::Done(()));
        assert!(child.is_zombie());
        assert_eq!(child.exit_status(), Some(ExitStatus::Exited(0)));
    }

    #[test]
    fn exit_group_with_signal_op_delegates_to_step_exit_group_with_signal() {
        let _g = setup();
        let parent = bootstrap();
        let child = step_fork::<TestPmap>(&parent).expect("fork");
        let mut op = ExitGroupWithSignalOp {
            process: &child,
            sig: Signum::SIGKILL,
        };
        let mut ctx = ScriptCtx::<tx_substrate::step_v3::ProcessIdentity>::new();
        let outcome = op.step(&mut ctx);
        assert_eq!(outcome, StepOutcome::Done(()));
        assert!(child.is_zombie());
        assert_eq!(
            child.exit_status(),
            Some(ExitStatus::Signaled(Signum::SIGKILL))
        );
    }

    #[test]
    fn waitpid_nohang_op_no_children_returns_echild() {
        let _g = setup();
        let parent = bootstrap();
        let mut op = WaitpidNohangOp {
            parent: &parent,
            target: WaitTarget::Any,
        };
        let mut ctx = ScriptCtx::<tx_substrate::step_v3::ProcessIdentity>::new();
        let outcome = op.step(&mut ctx);
        match outcome {
            StepOutcome::Done(Err(WaitError::NoChildren)) => {}
            other => panic!("expected Done(Err(NoChildren)), got {other:?}"),
        }
    }

    #[test]
    fn getcwd_op_returns_none_for_init_without_cwd() {
        let _g = setup();
        let parent = bootstrap();
        let mut op = GetcwdOp { target: &parent };
        let mut ctx = ScriptCtx::<tx_substrate::step_v3::ProcessIdentity>::new();
        let outcome = op.step(&mut ctx);
        // init bootstrap leaves cwd unset, so step_getcwd returns None.
        assert_eq!(outcome, StepOutcome::Done(None));
    }

    #[test]
    fn setpgid_op_unimplemented_for_non_self_pgid() {
        let _g = setup();
        let parent = bootstrap();
        // Day-1 only supports new_pgid == target.pid; anything else
        // returns SetpgidError::Unimplemented. Use a value that is
        // not the target's pid to exercise that arm deterministically.
        let bogus = Pgid(parent.pid.0 + 999);
        let mut op = SetpgidOp {
            target: &parent,
            new_pgid: bogus,
        };
        let mut ctx = ScriptCtx::<tx_substrate::step_v3::ProcessIdentity>::new();
        let outcome = op.step(&mut ctx);
        match outcome {
            StepOutcome::Done(Err(SetpgidError::Unimplemented)) => {}
            other => panic!("expected Done(Err(Unimplemented)), got {other:?}"),
        }
    }

    #[test]
    fn setsid_op_delegates_to_step_setsid() {
        let _g = setup();
        let parent = bootstrap();
        let child = step_fork::<TestPmap>(&parent).expect("fork");
        let mut op = SetsidOp { target: &child };
        let mut ctx = ScriptCtx::<tx_substrate::step_v3::ProcessIdentity>::new();
        let outcome = op.step(&mut ctx);
        match outcome {
            StepOutcome::Done(Ok(sid)) => {
                assert_eq!(sid.0, child.pid.0);
            }
            other => panic!("expected Done(Ok(sid)), got {other:?}"),
        }
    }

    #[test]
    fn close_cloexec_fds_op_is_noop_with_empty_set() {
        let _g = setup();
        let parent = bootstrap();
        let mut op = CloseCloexecFdsOp { process: &parent };
        let mut ctx = ScriptCtx::<tx_substrate::step_v3::ProcessIdentity>::new();
        let outcome = op.step(&mut ctx);
        assert_eq!(outcome, StepOutcome::Done(()));
        // Bootstrap leaves the CLOEXEC set empty; post-call it stays empty.
        assert!(parent.fd_cloexec_snapshot().is_empty());
    }

    #[test]
    fn reset_signal_dispositions_for_exec_op_runs_on_live_process() {
        let _g = setup();
        let parent = bootstrap();
        let mut op = ResetSignalDispositionsForExecOp { process: &parent };
        let mut ctx = ScriptCtx::<tx_substrate::step_v3::ProcessIdentity>::new();
        let outcome = op.step(&mut ctx);
        assert_eq!(outcome, StepOutcome::Done(()));
    }

    #[test]
    fn install_brk_for_exec_op_seeds_brk_base_and_current() {
        let _g = setup();
        let parent = bootstrap();
        let new_base: u64 = 0x4000_0000;
        let mut op = InstallBrkForExecOp {
            process: &parent,
            new_brk_base: new_base,
        };
        let mut ctx = ScriptCtx::<tx_substrate::step_v3::ProcessIdentity>::new();
        let outcome = op.step(&mut ctx);
        assert_eq!(outcome, StepOutcome::Done(()));
        let payload_guard = parent.payload.lock();
        let payload = payload_guard.as_ref().expect("init payload live");
        assert_eq!(
            payload
                .brk_base
                .load(core::sync::atomic::Ordering::Acquire),
            new_base
        );
        assert_eq!(
            payload
                .current_brk
                .load(core::sync::atomic::Ordering::Acquire),
            new_base
        );
    }
}
