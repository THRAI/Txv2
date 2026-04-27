# Exec — v1

<!-- txdoc:02-EXECUTION-EXEC-V1 -->

## Status
<!-- txdoc:EXEC-STATUS -->

Draft v1 (2026-04-26).

This document specifies **execve**: the cross-subsystem operation that replaces a process's address space, file-descriptor table, and signal-handler state with a new image loaded from a path-resolved binary, while preserving its identity (pid, parent, pgrp, session). Exec is the canonical compositional script — it spans VFS, Mount, Cred, VM, Process, FD, Signal, and ThreadRuntime — and this document fixes the composition. The ELF format parser is a vendored implementation detail of the loader (§8.10), not part of the architectural contract.

Exec has no entities of its own. It owns no `structure/`, no `checks/`, no `execution/`, no projections. It lives entirely as a script under `scripts/process/exec.rs`, drawing on every subsystem it composes. This document is therefore not a subsystem spec in the [`SUBSYSTEM_ANATOMY`](../00_meta-framework/SUBSYSTEM_ANATOMY_v2_1.md) sense; it is a **script spec**, closer in shape to the cross-subsystem fork example in SUBSYSTEM_ANATOMY §9 than to PROCESS or VM.

Companion documents:

- [`CONCEPTS_v4.md`](../00_meta-framework/CONCEPTS_v4.md) — basis claims, scripts, waits, publication, and carve-outs.
- [`SUBSYSTEM_ANATOMY_v2_1.md`](../00_meta-framework/SUBSYSTEM_ANATOMY_v2_1.md) §3.6 (compositional scripts), §3.7 (point of no return), §9 (cross-subsystem scripts).
- [`STEP_MODEL_v1.md`](STEP_MODEL_v1.md) — five-phase discipline; each step inside exec follows it.
- [`VM_v1_2.md`](../03_memory-vm/VM_v1_2.md) §5.7 — detached-build-then-swap address-space construction consumed here.
- [`PROCESS_v1.md`](../04_process-signals/PROCESS_v1.md) §5 (group-exit coordination), §7.2 (the `script_execve` skeleton this document elaborates), and §3 (`Frame` shared slots).
- [`PAGE_BACKED_v1.md`](../03_memory-vm/PAGE_BACKED_v1.md) — `step_read` and the `read_exact_at` helper.
- [`VFS_CHECKS_V2.1.md`](../05_filesystem/VFS_CHECKS_V2.1.md) — path resolution; mount-witness production.
- [`SIGNAL_v1.md`](../04_process-signals/SIGNAL_v1.md) — disposition reset semantics consumed by phase 7.
- [`SIGNAL_ATTACHMENTS_v1.md`](../04_process-signals/SIGNAL_ATTACHMENTS_v1.md) — the `process_execd` tracepoint declared by this document.
- [`INVARIANTS_v4.md`](../00_meta-framework/INVARIANTS_v4.md) — `STEP-*`, `SCRIPT-*`, `EXEC-*`, and publication rules.

### What this document pins
<!-- txdoc:EXEC-WHAT-THIS-DOCUMENT-PINS -->

- The seven-peer factoring of exec (§3): VFS, Mount, Cred, Loader, VM, Process, FD/Signal/ThreadRuntime, with Procfs as a post-commit consumer.
- The eight-phase structure (§4) with a single point-of-no-return boundary (§15) separating reversible work from infallible commits.
- The `ExecutableFile` reference type (§8.1) carried by the loader and persisted on `ProcessPayload.exe_file` for `/proc/<pid>/exe`.
- The `ExecImagePlan` / `LoadSegment` types (§8.1) — the loader's parser-agnostic output.
- The bounded targeted-read model for the loader (§8.2): no whole-file slurp.
- The process-frame shared-slot COW preparation pattern for `fd_table` and `sig_actions` (§9.5, §9.6).
- The detached-AddressSpace construction model (§9.2): build fully, populate stack via `vm::populate_detached_user_range`, then swap.
- The `EXEC-PONR` invariant (§15): no allocation, no user memory access, no I/O, no fallible work past phase 6.
- The `process_execd` tracepoint (§18): a script-level RawTrace publication, fired from the script's publish phase.
- Static PIE support (§8.7); rejection of `PT_INTERP` (§8.4); rejection of executable stack (§8.4).

### Zone-derived type policy
<!-- txdoc:EXEC-ZONE-DERIVED-TYPE-POLICY -->

EXEC is a script, not a subsystem with its own zones. It composes
role-shaped evidence from other owners:

| Exec use | Public handle | Owner |
|---|---|---|
| caller process | `Cap<ProcessIdentity>` plus `PayloadCap<ProcessPayload>` when replacing Frame state | PROCESS |
| old/new address space | `Cap<AddressSpace>` | VM |
| executable path result | `Cap<RNode>`, `Cap<DEntry>`, `Cap<MountIdentity>` as supplied by VFS/MOUNT | VFS/MOUNT |
| mount policy checks | witness carrying `IdentRef<'g, MountIdentity>` | MOUNT |
| procfs-visible executable reference | role-shaped caps stored on `ProcessPayload.exe_file` | PROCESS/VFS/MOUNT |

Exec never chooses `Zone<T, Policy>`. Its phase boundary is where witnesses are
upgraded into caps or payload evidence before reservations and publication.

### What this document defers
<!-- txdoc:EXEC-WHAT-THIS-DOCUMENT-DEFERS -->

- **Non-leader execve.** Tid-rename semantics deferred to Phase 2 per [`PROCESS_v1.md §11.2`](../04_process-signals/PROCESS_v1.md). Non-leader exec returns ENOSYS in v1 (rejected in phase 0).
- **Dynamic linking (PT_INTERP).** Static binaries only in v1; the loader rejects `PT_INTERP` with ENOEXEC. Phase 2 adds interpreter resolution, second-image loading, `AT_BASE`.
- **suid / sgid / file capabilities.** Cred is preserved unchanged across exec in v1. The cred mutation reservation (§7) is taken to keep the Phase 2 patch local.
- **ASLR.** Static-PIE `load_bias` is a deterministic per-arch constant in v1. Random bias added when entropy and address-space randomization land.
- **AT_SECURE.** Always 0 in v1 (no privilege transition).
- **Personality flags.** `personality(2)` not implemented.
- **MAP_DENYWRITE.** Mount-level write inhibition during exec deferred. The binary's PageContainer is COW-shared via the recipes BTree; concurrent writes to the underlying file are visible to the exec'd process via the shared backing — a known POSIX divergence Linux mitigates with MAP_DENYWRITE on text segments. Not addressed in v1.
- **PT_TLS kernel-side initialization.** §8.8 records `PT_TLS` into `ExecImagePlan.tls` but does not act on it. If the chosen userland requires kernel-initialized TLS before entry, this is promoted to a v1 requirement (§20 risk).
- **`PTRACE_EVENT_EXEC`.** Flagged at §18; landing with the observation subsystem.

### Integration requirements
<!-- txdoc:EXEC-INTEGRATION-REQUIREMENTS -->

The following peer surfaces must exist for this spec to compile. They are implementation requirements, not a separate architecture model.

1. **[`PROCESS_v1.md`](../04_process-signals/PROCESS_v1.md).**
   - **(P1)** GroupExit is one-shot per *episode*, not per process lifetime. `group_exit.state` is cleared after exec collapse completes (§12.5). PROCESS_v1 §5 currently leaves this implicit; make it explicit.
   - **(P2)** New field on `ProcessPayload`: `exe_file: AtomicSlot<Option<ExecutableImageRef>>` (see §3.8 for the type). Set during phase 7 commit; read by procfs.
   - **(P3)** New field on `ProcessPayload`: `cmdline: ExecCmdlineSnapshot` — a copy of argv strings captured at exec time, projected by `/proc/<pid>/cmdline`.

2. **[`VM_v1_2.md`](../03_memory-vm/VM_v1_2.md).**
   - **(V1)** Use the detached-build-then-swap shape (§9.2, §11). The primitive `vm::scripts::build_aspace_from_image(plan)` returns a fresh `Cap<AddressSpace>` not yet bound to any process. The actual swap is `frame.vm.replace(new_as)` — a single process-frame shared-slot store, not a RangeLock-mediated mutation.
   - **(V2)** New API `vm::populate_detached_user_range(new_as, dst_user_va, src_kernel)` for writing into an AddressSpace that no thread is currently running in (§9.3). This is *not* `copy_to_user` — there is no current AS to fault against.

3. **[`PAGE_BACKED_v1.md`](../03_memory-vm/PAGE_BACKED_v1.md).**
   - **(B1)** New helper `read_exact_at(rnode, offset, dst) -> Result<(), Errno>` (§8.2). A targeted read at an explicit file offset that does not touch any `OpenFile.offset` and may block on materialization. Used by the loader; generally useful for any kernel-side targeted read of file content.

4. **[`SIGNAL_ATTACHMENTS_v1.md`](../04_process-signals/SIGNAL_ATTACHMENTS_v1.md).**
   - **(SA1)** New entry for the `process_execd` tracepoint, fired from `scripts/process/exec.rs::publish` (§18). The catalog gains either a "Scripts" subsection or a row under §3.x Process tracepoints.

5. **[`cred_service_v_1_draft (2).md`](<cred_service_v_1_draft (2).md>).**
   - **(C1)** Cred exposes a credential-mutation reservation lane. Exec takes the reservation in phase 2 even though v1 does not change credentials, so the later suid/cap patch is local.

---

## 1. Motivation and shape
<!-- txdoc:EXEC-1-MOTIVATION-AND-SHAPE -->

Exec is unique among POSIX operations: it preserves the calling process's identity but replaces nearly everything else. The pid stays. The parent, pgrp, session, controlling tty stay. Pending signals stay (POSIX). The signal mask stays. But the address space, the descriptor table's cloexec-marked entries, the registered signal *handlers*, the alt-stack, the program counter, the stack pointer, the entire userspace text/data/bss — gone, replaced by content drawn from a binary file.

This straddles every architectural axis the kernel cares about:

- **Resolution** ([CONCEPTS §2.1](../00_meta-framework/CONCEPTS_v4.md)). Exec resolves a path to an executable RNode, and resolves cred against the binary's permissions and mount policy.
- **Lifecycle.** The AddressSpace is destroyed and a new one is constructed. The fd-table is partially evicted. sig_actions is reset. Other threads in the group are killed.
- **Publication.** The `process_execd` tracepoint fires; ptrace observes; closing FD_CLOEXEC fds publishes fsnotify events to outside watchers.

Two structural properties make exec different from every other syscall:

**(a) The point of no return.** Exec cannot fail cleanly past a certain point — the AS is gone, there is nothing to return to. Up to a fixed boundary, every error returns control to the caller's old AS with `errno` set; past the boundary, every error terminates the process with a fatal signal. This boundary is a property of the **script**, not of any single instruction. The address-space replacement is the visibility boundary; everything after it must be infallible.

**(b) The composition is the work.** Exec does not own a subsystem. It does not maintain an index. It does not have predicates of its own. Its specification *is* the sequencing of other subsystems' steps in the correct order with the correct rollback semantics. Every interesting design question for exec is a question about the boundaries between peers.

This document specifies that composition. The seven peers in §3 are first-class, not implementation details; the eight phases in §4 are mandatory, not suggestions; and the EXEC-PONR invariant in §15 makes the irreversibility boundary checkable.

---

## 2. Where exec lives
<!-- txdoc:EXEC-2-WHERE-EXEC-LIVES -->

Per [`MODULE_MAP_v1.md`](../00_meta-framework/MODULE_MAP_v1.md) and [`SUBSYSTEM_ANATOMY_v2_1.md §3.6`](../00_meta-framework/SUBSYSTEM_ANATOMY_v2_1.md), exec is a **script**, not a subsystem.

### 2.1 Module placement
<!-- txdoc:EXEC-2-1-MODULE-PLACEMENT -->

```
scripts/process/exec.rs              the script_execve sequencer
scripts/process/exec/loader.rs       the image loader (§8); the only file
                                     that touches the ELF parser library
scripts/process/exec/stack.rs        argv/envp/auxv stack construction (§9.3, §9.4)
scripts/process/exec/cow_prepare.rs  fd_table and sig_actions COW preparation (§9.5, §9.6)
```

`scripts/process/exec.rs` is the script entry. The other three files are private helpers under it. They have no public API beyond what `exec.rs` calls; nothing outside `scripts/process/exec/*` should import them.

### 2.2 What exec imports
<!-- txdoc:EXEC-2-2-WHAT-EXEC-IMPORTS -->

Following SCRIPT-2 (scripts may import from any peer subsystem's `checks/` and `execution/`, plus from substrate primitives) and SCRIPT-6 (scripts may not import from another subsystem's `structure/`):

- `vfs::checks::*` — path resolution, executability witness.
- `vfs::execution::*` — none in v1; the loader does not open an OpenFile (no fd is allocated for the binary).
- `mount::checks::*` — `require_exec_permitted`, `observe_suid_policy`.
- `cred::checks::*` — `require_executable`; `cred::execution::compute_exec_credentials`.
- `vm::scripts::build_aspace_from_image`, `vm::populate_detached_user_range`.
- `proc::execution::initiate_group_exit_for_exec`, `proc::checks::is_thread_group_leader`.
- `fd_table::execution::prepare_exec_close_cloexec` (added by this document; see §9.5).
- `sig_actions::execution::prepare_exec_reset` (added by this document; see §9.6).
- `thread_runtime::execution::commit_exec_context`.
- `page_backed::read_exact_at` (cross-doc edit B1).
- `random::get_bytes` for `AT_RANDOM` (or boot-seeded fallback).
- `AuxvIf::arch_auxv_facts` through the selected platform `P` for `AT_HWCAP` and friends.
- `trace::process_execd` for the tracepoint.

### 2.3 What exec does not own
<!-- txdoc:EXEC-2-3-WHAT-EXEC-DOES-NOT-OWN -->

- **No entities.** No zone, no SlotMeta, no projection.
- **No structure/**. No index, no DLL.
- **No checks/**. No predicate that says "this exec is permitted" — that is composed from `vfs::checks::require_resolved`, `mount::checks::require_exec_permitted`, and `cred::checks::require_executable`.
- **No execution/**. No `step_*` function. The script is composed of step calls from peers.
- **No projection.** Procfs reads from `ProcessPayload.exe_file`, `ProcessPayload.cmdline`, the AddressSpace's recipes BTree — all owned by their respective subsystems.

The only exec-specific persistent state added to the system is the two new fields on `ProcessPayload` (corrections P2 and P3). Both are owned by Process; exec just writes them.

---

## 3. Peers and their roles
<!-- txdoc:EXEC-3-PEERS-AND-THEIR-ROLES -->

The seven peers, in the order they enter the script. Each peer has a focused role; the script is the only place these roles compose.

### 3.1 VFS — resolves the executable object
<!-- txdoc:EXEC-3-1-VFS-RESOLVES-THE-EXECUTABLE-OBJECT -->

Inputs: a userspace path (or `dirfd + path` for `execveat`).
Outputs: an `RNode` witness, a `DEntry` witness, a `Mount` witness, all under a single epoch guard.

Role: standard VFS resolution. Failures are the standard set: ENOENT, ENOTDIR, ELOOP, ENAMETOOLONG, EACCES on traversal.

VFS does **not** check executability. The mode bits and ownership are read here for the cred peer to consume; VFS's job ends at "the path resolves to this RNode in this mount."

### 3.2 Mount — supplies NOEXEC and NOSUID policy
<!-- txdoc:EXEC-3-2-MOUNT-SUPPLIES-NOEXEC-AND-NOSUID-POLICY -->

Inputs: the mount witness from VFS.
Outputs: an `ExecMountWitness` (NOEXEC denial check) and a `SuidWitness` (NOSUID state for cred consumption).

Role: mount flags are mount semantics, not inode semantics. NOEXEC denies exec; NOSUID modifies credential computation but does not deny exec.

```rust
mount::checks::require_exec_permitted(mount_w) -> Result<ExecMountWitness, Errno>
    // EACCES if MS_NOEXEC is set on the mount.

mount::checks::observe_suid_policy(mount_w) -> SuidWitness
    // Records whether MS_NOSUID is in effect.
    // Infallible: SuidWitness is informational, not authorizing.
```

Other mount flags (MS_NODEV, MS_RDONLY, MS_NOATIME) are inert here.

### 3.3 Cred — authorizes execute, computes new credentials
<!-- txdoc:EXEC-3-3-CRED-AUTHORIZES-EXECUTE-COMPUTES-NEW-CREDENTIALS -->

Inputs: RNode witness (mode + uid/gid), ExecMountWitness, SuidWitness, caller's credential.
Outputs: an `ExecAuthWitness` and a `NewCredential` value.

Role split:

```rust
cred::checks::require_executable(rnode_w, exec_mount_w, caller_cred)
    -> Result<ExecAuthWitness, Errno>
    // Mode bits + ownership. Returns EACCES (no execute permission) or
    // EPERM (caller cannot exec this binary, e.g., capability check).

cred::execution::compute_exec_credentials(auth_w, suid_w, caller_cred)
    -> NewCredential
    // v1: returns caller_cred unchanged.
    // Phase 2: applies setuid/setgid file mode bits subject to SuidWitness;
    //          applies file capabilities; computes the post-exec credential.
```

`compute_exec_credentials` runs under a credential mutation reservation (§7). The reservation prevents another thread of this process from racing a `setuid()` between authorization and exec commit. v1 still takes the reservation.

### 3.4 Loader — parses the executable into an image plan
<!-- txdoc:EXEC-3-4-LOADER-PARSES-THE-EXECUTABLE-INTO-AN-IMAGE-PLAN -->

Inputs: an `ExecutableFile` (RNode + mount + dentry, with retention promoted from witnesses).
Outputs: an `ExecImagePlan` (entry, segments, TLS template, auxv-relevant facts).

Role: read the ELF header and program-header table via targeted file reads, validate, translate into txKernel-owned types. The loader does not read segment data; segments materialize lazily after the AS is built and faulted.

The parser library is a v1 implementation choice (see §8.10), not part of the spec contract. Parser types do not escape `loader.rs`.

Full specification in §8.

### 3.5 VM — builds the replacement AddressSpace
<!-- txdoc:EXEC-3-5-VM-BUILDS-THE-REPLACEMENT-ADDRESSSPACE -->

Inputs: `ExecImagePlan`, `Cap<RNode>` for segment backing.
Outputs: a fresh `Cap<AddressSpace>` not yet bound to any process or thread.

Role: per-segment VmEntry construction; stack VmEntry; brk; populate the initial stack image. The new AS holds its own retention on the binary's PageContainer through the segment VmEntries — the loader's transient `Cap<RNode>` is not load-bearing for fault-time materialization.

The new primitive `vm::scripts::build_aspace_from_image` (cross-doc edit V1) replaces VM_v1.2 §5.7's teardown-then-rebuild model. The actual replacement of the process's AS happens later, in phase 6 (§11), via a single `Shared<AddressSpace>` store.

### 3.6 Process — coordinates collapse and preserves the shell
<!-- txdoc:EXEC-3-6-PROCESS-COORDINATES-COLLAPSE-AND-PRESERVES-THE-SHELL -->

Inputs: caller's `ProcessIdentity` and `ProcessPayload`, multi-thread state.
Outputs: a single-threaded process state, with `group_exit` armed and discharged.

Role: detect multi-threaded; if so, initiate `GroupExit { is_exec: true }` per [`PROCESS_v1.md §5`](../04_process-signals/PROCESS_v1.md); wait for sibling threads to terminate; clear the episode after exec commits.

Process owns the *outer* identity of the operation. Pid, parent binding, pgrp/session bindings, children DLL, exit status — all of `ProcessIdentity` — survive intact. Within `ProcessPayload`, the `Frame` fields (`vm`, `fd_table`, `sig_actions`) are replaced; the threads DLL collapses to one entry; `group_exit` arms and clears; the new `exe_file` and `cmdline` are written.

### 3.7 FD / Signal / ThreadRuntime — apply per-Frame replacements
<!-- txdoc:EXEC-3-7-FD-SIGNAL-THREADRUNTIME-APPLY-PER-FRAME-REPLACEMENTS -->

These three peers receive the prepared replacements built in phase 4 and install them post-PONR:

- **FD (fd_table).** `prepare_exec_close_cloexec` (phase 4) produces a private fd table with FD_CLOEXEC entries already absent (§9.5). Phase 7 swaps the Shared<FdTable> slot. The CLOEXEC entries' `Cap<OpenFile>` drops are the publication points (fsnotify on each underlying RNode where applicable).

- **Signal (sig_actions).** `prepare_exec_reset` (phase 4) produces a private SigActionTable with non-ignored handlers reset to SIG_DFL, ignored handlers preserved (§9.6). Phase 7 swaps the Shared<SigActionTable>. The thread's altstack is cleared inline.

- **ThreadRuntime.** `commit_exec_context(thread, entry, sp)` writes the new user pc/sp, zeros the gprs, sets the arch TLS register to zero, via the HAL's `TrapFrameMut` (§12.6). Exec does not touch raw trap frames.

### 3.8 ExecutableImageRef — the procfs handoff
<!-- txdoc:EXEC-3-8-EXECUTABLEIMAGEREF-THE-PROCFS-HANDOFF -->

Procfs is not a peer in the sequencing sense; it is a **post-commit consumer** that reads projections of state set by exec. The only active record exec keeps for procfs is the executable reference, persisted on `ProcessPayload.exe_file`:

```rust
pub struct ExecutableImageRef {
    pub mount:  Cap<MountIdentity>,
    pub dentry: Cap<DEntry>,
    pub rnode:  Cap<RNode>,
}
```

Stored in `AtomicSlot<Option<ExecutableImageRef>>` on `ProcessPayload`. The mount context allows `/proc/<pid>/exe` to render the current path correctly across mount-namespace transitions; the dentry provides the path; the RNode keeps the executable object's identity stable even if the dentry is unlinked or renamed (`/proc/<pid>/exe` then renders as `<path> (deleted)` per Linux convention).

The same struct shape appears as the loader's input — see §8.1's `ExecutableFile`. The symmetry is intentional: what flows into the loader as input is what we install on Process as output. One value, two uses.

---

## 4. The eight phases
<!-- txdoc:EXEC-4-THE-EIGHT-PHASES -->

```
Phase 0 — Prelude                                       reversible
Phase 1 — Resolve and mount policy                      reversible
Phase 2 — Authorization and credential plan             reversible
Phase 3 — Load executable image plan                    reversible
Phase 4 — Prepare detached replacement                  reversible
Phase 5 — Collapse thread group                         reversible (collapse infallible in v1)
================================================================================
Phase 6 — Address-space visibility boundary             POINT OF NO RETURN
================================================================================
Phase 7 — Infallible post-swap commits                  irreversible, infallible
Phase 8 — Publish and enter userspace                   irreversible, infallible
```

Phases 0–5 are **reversible**: any failure drops the prepared resources and returns errno to the caller in the *old* AS. Phase 6 is a single store. Phases 7–8 are **infallible**: the EXEC-PONR invariant (§15) constrains them to perform no allocation, no user memory access, no I/O, no fallible computation.

The boundary between phases 5 and 6 is the point of no return. The boundary between phases 6 and 7 is the address-space visibility boundary. They are different boundaries:

- After phase 5, the process is irreversibly committed to either exec'ing or dying. (In v1, collapse cannot fail, so this is automatic.)
- After phase 6, the new AS is visible. Other observers — procfs readers, ptrace tracers — may see the new AS while phase 7 is still running. v1 does not promise external atomicity of exec across the multi-commit phase 7 sequence.

POSIX permits this. Linux behaves the same way: a `/proc/<pid>/maps` reader concurrent with another process's exec can observe the new maps with the old fd table briefly. POSIX does not require exec to be atomic to external observers.

### 4.1 Witness flow across phases
<!-- txdoc:EXEC-4-1-WITNESS-FLOW-ACROSS-PHASES -->

Witnesses cannot cross step boundaries (WIT-3). Phases 1 and 2 collect *all* needed witnesses under *one* epoch guard, then promote to retention (`Cap<T>`) before phase 3 begins. Phase 3 onward holds only retention.

```
Phase 1:  rnode_w, dentry_w, mount_w        (epoch guard active)
Phase 2:  + auth_w, suid_w                  (same guard)
          → upgrade to:
            rnode_cap, dentry_cap, mount_cap, NewCredential
                                            (guard released)
Phase 3:  loader uses rnode_cap             (no witness)
Phase 4:  vm builder uses rnode_cap         (no witness)
...
```

This matches the cross-subsystem fork pattern in [`SUBSYSTEM_ANATOMY_v2_1.md §9`](../00_meta-framework/SUBSYSTEM_ANATOMY_v2_1.md). Enforcement is by SCRIPT-2/SCRIPT-6.

### 4.2 Phase summary table
<!-- txdoc:EXEC-4-2-PHASE-SUMMARY-TABLE -->

| Phase | Peer(s) | Action | Failure mode |
|---|---|---|---|
| 0 | — | Decode args; capture cred snapshot; reject non-leader | EFAULT, ENOSYS |
| 1 | VFS, Mount | Resolve path; check NOEXEC; observe NOSUID | ENOENT, ENOTDIR, ELOOP, EACCES |
| 2 | Cred | Authorize execute; reserve cred lane; compute new cred | EACCES, EPERM |
| 3 | Loader | Read ELF header and phdrs; validate; translate | ENOEXEC, ENOMEM, EIO |
| 4 | VM, FD, Signal | Build detached AS; copy argv/envp; populate stack; COW-prepare fd_table and sig_actions | ENOMEM, E2BIG, EFAULT |
| 5 | Process | If multi-threaded: collapse via GroupExit | (infallible in v1) |
| 6 | VM | `frame.vm.replace(new_as)` | (infallible) |
| 7 | FD, Signal, Cred, ThreadRuntime, Process | Install fd_table; close FD_CLOEXEC; install sig_actions; reset; clear altstack; install cred; record exe_file/cmdline; clear group_exit; install trap context | (infallible) |
| 8 | — | `process_execd` tracepoint; return to userspace at new entry | (infallible) |

---

## 5. Phase 0 — Prelude
<!-- txdoc:EXEC-5-PHASE-0-PRELUDE -->

```rust
async fn script_execve(path: UserAddr, argv: UserAddr, envp: UserAddr)
    -> Result<!, Errno>
{
    let caller_thread = current_thread();
    let caller_proc = caller_thread.process();

    // 0.1 — leader-only check (v1 limitation).
    if !proc::checks::is_thread_group_leader(caller_thread) {
        return Err(Errno::ENOSYS);  // non-leader exec deferred to Phase 2
    }

    // 0.2 — capture credential snapshot for racing setuid.
    let cred_snapshot = caller_proc.payload.policy.cred.snapshot();

    // 0.3 — defer pointer dereference of argv/envp to phase 4.
    //         here, we just record the userspace pointers.
    //         no copy_from_user yet.
    let argv_ptr = argv;
    let envp_ptr = envp;

    // 0.4 — copy path string out of userspace.
    //         path is short (PATH_MAX = 4096); bounded read.
    let path_buf = copy_path_from_user(path)?;

    // ... continues to phase 1
}
```

The non-leader check happens *first*, before any expensive work. Failing here costs only the path string copy.

The credential snapshot is captured early but consumed in phase 2. Between snapshot and phase 2's reservation, another thread may have racing `setuid` calls; the reservation in phase 2 either acquires after the racing change (and authorizes against the new cred) or fails to acquire and we retry with a fresh snapshot. v1 does not yet have racing cred mutation, but the snapshot/reservation pattern is wired now.

Argv/envp pointer arrays are *not* dereferenced in phase 0. The userspace strings they point to live in the old AS, which is still valid until phase 6. Reading them is deferred to phase 4 where it groups naturally with the other "last reads of old AS" work.

---

## 6. Phase 1 — Resolve and mount policy
<!-- txdoc:EXEC-6-PHASE-1-RESOLVE-AND-MOUNT-POLICY -->

```rust
    // All witnesses gathered under a single epoch guard.
    let guard = epoch::guard();

    // 1.1 — VFS path resolution.
    let resolved = vfs::checks::require_resolved(&path_buf, &caller_proc.payload.frame, &guard)?;
    //  resolved = ResolvedRNode { rnode_w, dentry_w, mount_w }

    // 1.2 — mount NOEXEC check.
    let exec_mount_w = mount::checks::require_exec_permitted(resolved.mount_w, &guard)?;
    //  EACCES if MS_NOEXEC.

    // 1.3 — observe NOSUID state for cred consumption.
    let suid_w = mount::checks::observe_suid_policy(resolved.mount_w, &guard);
    //  Infallible. Records whether MS_NOSUID is set.

    // ... continues into phase 2 under the same guard
```

**Failure modes.**

| Errno | Cause |
|---|---|
| ENOENT | Path component missing |
| ENOTDIR | Non-directory in path prefix |
| ELOOP | Symlink chain too deep |
| ENAMETOOLONG | Path or component too long |
| EACCES | Search permission denied on directory; **or** MS_NOEXEC mount |

Note that EACCES is overloaded: VFS produces it for traversal; Mount produces it for NOEXEC. Userspace cannot distinguish; this matches Linux.

`ENOEXEC` is **not** produced here. Reserved for image-format failures in phase 3.

---

## 7. Phase 2 — Authorization and credential plan
<!-- txdoc:EXEC-7-PHASE-2-AUTHORIZATION-AND-CREDENTIAL-PLAN -->

```rust
    // 2.1 — credential mutation reservation.
    //         Prevents concurrent setuid/setgid/capset until exec commits or aborts.
    let cred_reservation = cred::execution::reserve_exec_credential_lane(
        &caller_proc.payload.policy,
        cred_snapshot,
    )?;
    //  Errno::EAGAIN if another exec is concurrently in flight on this process
    //  (cannot happen in v1 — single-threaded post-collapse — but the
    //   reservation is taken before collapse to keep the wiring uniform).

    // 2.2 — execute authorization.
    let auth_w = cred::checks::require_executable(
        resolved.rnode_w, exec_mount_w, &cred_reservation.snapshot, &guard,
    )?;
    //  EACCES (no x bit), EPERM (capability check), or specific cred errors.

    // 2.3 — compute post-exec credential.
    //         v1: returns cred_snapshot unchanged.
    //         Phase 2 of cred work: applies setuid/sgid/file caps subject to suid_w.
    let new_credential = cred::execution::compute_exec_credentials(
        auth_w, suid_w, &cred_reservation,
    );

    // 2.4 — promote witnesses to retention.
    //         Witnesses cannot cross step boundaries; phase 3 needs Caps.
    let rnode_cap = resolved.rnode_w.upgrade()?;
    let dentry_cap = resolved.dentry_w.upgrade()?;
    let mount_cap = resolved.mount_w.upgrade()?;
    drop(guard);

    let exe_file = ExecutableFile {
        rnode: rnode_cap,
        mount: mount_cap,
        dentry: dentry_cap,
    };

    // ... continues into phase 3
```

**Reservation semantics.** The cred reservation is a substrate-style linear token (per [`SUBSYSTEM_ANATOMY_v2_1.md §4`](../00_meta-framework/SUBSYSTEM_ANATOMY_v2_1.md)). It holds until either:

- Phase 7 commits the new credential (consume), or
- The script returns Err before phase 6 (drop, rollback).

The reservation is *not* a lock in the traditional sense; concurrent reads of cred go through `cred::checks::*` and produce witnesses that are valid against the reservation's snapshot. Concurrent *mutations* (setuid, capset) are blocked by the reservation until exec commits or aborts.

**Failure modes.**

| Errno | Cause |
|---|---|
| EACCES | No execute permission (mode bits, caller's effective uid/gid) |
| EPERM | Privileged check failed (capability not held, etc.) |
| EAGAIN | Cred mutation reservation contended (cannot happen in v1) |

---

## 8. Phase 3 — Load executable image plan
<!-- txdoc:EXEC-8-PHASE-3-LOAD-EXECUTABLE-IMAGE-PLAN -->

This phase reads the binary's headers, validates them, and translates them into txKernel-owned image-plan types. It performs targeted I/O (which may block), bounded synchronous parsing, and validation. No segment data is read here; segments materialize lazily after the AS is built.

The phase is itself a small script: read header → validate → read phdrs → validate → translate. Each read may yield. Total I/O is bounded by `64 + MAX_PHDRS * 56` bytes (~3.6 KB).

### 8.1 Loader public types
<!-- txdoc:EXEC-8-1-LOADER-PUBLIC-TYPES -->

```rust
// Input.
pub struct ExecutableFile {
    pub rnode:  Cap<RNode>,
    pub mount:  Cap<MountIdentity>,
    pub dentry: Cap<DEntry>,
}

// Output.
pub struct ExecImagePlan {
    pub arch:       ImageArch,            // RV64 | LA64
    pub image_type: ImageType,            // Exec | Pie
    pub load_bias:  UserAddr,             // 0 for ET_EXEC; deterministic constant for ET_DYN
    pub entry:      UserAddr,             // load_bias + e_entry
    pub segments:   Vec<LoadSegment>,     // PT_LOAD segments, page-rounded
    pub interp:     Option<InterpRef>,    // v1: always None; Some → ENOEXEC
    pub tls:        Option<TlsTemplate>,  // PT_TLS, recorded but not applied in v1
    pub phdr_va:    UserAddr,             // AT_PHDR (computed; ENOEXEC if uncomputable)
    pub phent:      u16,                  // AT_PHENT
    pub phnum:      u16,                  // AT_PHNUM
    pub flags:      ImageFlags,           // exec_stack request, etc.
}

pub struct LoadSegment {
    pub map_start:        UserAddr,   // floor(load_bias + p_vaddr, PAGE_SIZE)
    pub map_end:          UserAddr,   // ceil(load_bias + p_vaddr + p_memsz, PAGE_SIZE)
    pub file_page_offset: u64,        // floor(p_offset, PAGE_SIZE)
    pub page_delta:       usize,      // (load_bias + p_vaddr) - map_start
    pub file_size:        u64,        // p_filesz (from start of segment, not from map_start)
    pub mem_size:         u64,        // p_memsz
    pub prot:             Prot,       // from PF_R | PF_W | PF_X
    pub align:            u64,        // p_align (validated to be page-aligned)
}

pub struct TlsTemplate {
    pub file_page_offset: u64,
    pub file_size:        u64,
    pub mem_size:         u64,
    pub align:            u64,
    pub vaddr:            UserAddr,    // load_bias + PT_TLS.p_vaddr
}

pub enum ImageArch { Rv64, La64 }
pub enum ImageType { Exec, Pie }

pub struct ImageFlags {
    pub exec_stack: bool,   // PT_GNU_STACK has PF_X — v1 rejects
}
```

### 8.2 The bounded targeted-read model
<!-- txdoc:EXEC-8-2-THE-BOUNDED-TARGETED-READ-MODEL -->

The loader does not read the whole binary into memory. It reads at most:

- 64 bytes for the ELF64 header at offset 0.
- `phnum * phentsize` bytes for the program-header table at `e_phoff`. Bounded by `MAX_PHDRS = 64` and `phentsize = 56`, so ≤ 3584 bytes.

These reads use a new helper added to PAGE_BACKED (cross-doc edit B1):

```rust
/// Read exactly `dst.len()` bytes from `rnode` at file offset `offset` into `dst`.
/// Does not touch any OpenFile.offset. May block on page materialization.
/// Returns ENOEXEC if the requested range extends past file size.
/// Returns EIO on underlying read error.
pub async fn read_exact_at(
    rnode: &Cap<RNode>,
    offset: u64,
    dst: &mut [u8],
) -> Result<(), Errno>;
```

The contract differs from the read syscall path:

- No fd, no OpenFile, no offset mutation.
- Returns ENOEXEC (not 0 or EOF) on short read of the requested range. This matches the caller's intent: "I need exactly these bytes; if the file is too short, the binary is malformed."
- Generally useful for any kernel-side targeted read. Other consumers (boot-time loader for kernel modules, future) can use the same helper.

### 8.3 Entry: `load_exec_image`
<!-- txdoc:EXEC-8-3-ENTRY-LOAD-EXEC-IMAGE -->

```rust
pub async fn load_exec_image(exe: &ExecutableFile) -> Result<ExecImagePlan, Errno> {
    // 8.3.1 — read and parse the ELF header.
    let mut hdr_buf = [0u8; ELF64_HEADER_SIZE];  // 64 bytes
    page_backed::read_exact_at(&exe.rnode, 0, &mut hdr_buf).await?;
    let header = parse_elf_header(&hdr_buf)?;     // see §8.4

    // 8.3.2 — determine load_bias from image type.
    let (image_type, load_bias) = compute_load_bias(&header)?;

    // 8.3.3 — read and parse the program-header table.
    let phdr_size = header.phnum as usize * header.phentsize as usize;
    let mut phdr_buf = alloc_kernel_buf(phdr_size).map_err(|_| Errno::ENOMEM)?;
    page_backed::read_exact_at(&exe.rnode, header.phoff, &mut phdr_buf).await?;
    let phdrs = parse_program_headers(&phdr_buf, header.phnum, header.phentsize)?;

    // 8.3.4 — validate and translate.
    validate_phdrs(&phdrs, load_bias)?;
    let segments = translate_pt_loads(&phdrs, load_bias)?;
    let tls = translate_pt_tls(&phdrs, load_bias);
    let phdr_va = compute_at_phdr(&header, &phdrs, load_bias)?;
    let flags = derive_image_flags(&phdrs)?;

    Ok(ExecImagePlan {
        arch: header.arch,
        image_type,
        load_bias,
        entry: UserAddr(load_bias.0 + header.entry),
        segments,
        interp: None,    // v1 rejects PT_INTERP earlier
        tls,
        phdr_va,
        phent: header.phentsize,
        phnum: header.phnum,
        flags,
    })
}
```

### 8.4 Header validation
<!-- txdoc:EXEC-8-4-HEADER-VALIDATION -->

Per the canonical ELF64 layout (Elf64_Ehdr), `parse_elf_header` validates:

| Check | Error |
|---|---|
| `e_ident[EI_MAG0..4] == "\x7fELF"` | ENOEXEC |
| `e_ident[EI_CLASS] == ELFCLASS64` | ENOEXEC |
| `e_ident[EI_DATA] == ELFDATA2LSB` | ENOEXEC |
| `e_ident[EI_VERSION] == EV_CURRENT` | ENOEXEC |
| `e_machine ∈ {EM_RISCV, EM_LOONGARCH}` and matches build target | ENOEXEC |
| `e_type ∈ {ET_EXEC, ET_DYN}` | ENOEXEC |
| `e_phentsize == sizeof(Elf64_Phdr)` (= 56) | ENOEXEC |
| `e_phnum <= MAX_PHDRS` (= 64) | ENOEXEC |
| `e_phoff + e_phnum * e_phentsize` does not overflow u64 | ENOEXEC |

`MAX_PHDRS = 64` is a hard parsing bound. Real-world static binaries have ≤ 16 program headers; 64 is comfortable headroom.

After header validation, but before phdr-table read: any `PT_INTERP` discovered during phdr translation will be rejected with ENOEXEC (§8.6). v1 does not pre-screen for it at the header level since the header doesn't expose it directly; the rejection happens in `validate_phdrs`.

### 8.5 Program-header validation
<!-- txdoc:EXEC-8-5-PROGRAM-HEADER-VALIDATION -->

For each `PT_LOAD` segment:

| Check | Error |
|---|---|
| `p_filesz <= p_memsz` | ENOEXEC |
| `p_offset + p_filesz` does not overflow | ENOEXEC |
| `p_vaddr + p_memsz` does not overflow | ENOEXEC |
| `load_bias + p_vaddr + p_memsz <= USER_TOP` | ENOEXEC |
| `p_align` is 0, 1, or a power of two | ENOEXEC |
| `p_align <= PAGE_SIZE` (we don't support huge pages in v1) | ENOEXEC |
| `p_vaddr % PAGE_SIZE == p_offset % PAGE_SIZE` (ELF congruence) | ENOEXEC |
| Page-rounded ranges of distinct PT_LOADs do not overlap | ENOEXEC |
| `p_flags & ~(PF_R \| PF_W \| PF_X) == 0` (no unknown bits) | (ignored, not fatal) |

Encountering `PT_INTERP`: `Errno::ENOEXEC` (v1 rejects dynamic linking).

Encountering `PT_GNU_STACK` with `PF_X`: `Errno::ENOEXEC` (v1 conservative policy; see §8.4 below).

`PT_TLS`: recorded into `ExecImagePlan.tls`. Not validated for executability of TLS init image.

`PT_PHDR`, `PT_NOTE`, `PT_GNU_RELRO`, `PT_GNU_EH_FRAME`: noted for future use; `PT_PHDR` consumed for `AT_PHDR` computation (§8.6).

### 8.6 AT_PHDR computation
<!-- txdoc:EXEC-8-6-AT-PHDR-COMPUTATION -->

`AT_PHDR` must be computed and is mandatory in `ExecImagePlan`. The runtime needs it for `dl_iterate_phdr`, stack unwinding via `eh_frame`, and (when interp lands) dynamic-linker bootstrap.

```
1. If a PT_PHDR program header exists:
       phdr_va = load_bias + PT_PHDR.p_vaddr
2. Else, find the PT_LOAD whose file range covers e_phoff:
       segment ∈ PT_LOADs where
           segment.p_offset <= e_phoff
           e_phoff + e_phnum * e_phentsize <= segment.p_offset + segment.p_filesz
       phdr_va = load_bias + segment.p_vaddr + (e_phoff - segment.p_offset)
3. Else: return Errno::ENOEXEC.
```

The third case — a binary whose phdr table is not covered by any loaded segment and lacks `PT_PHDR` — is technically permitted by the ELF spec but unusual. Modern toolchains always produce binaries satisfying case 1 or case 2; rejecting case 3 is reasonable and matches Linux's `load_elf_phdrs` behavior in practice.

### 8.7 Static PIE handling and load_bias
<!-- txdoc:EXEC-8-7-STATIC-PIE-HANDLING-AND-LOAD-BIAS -->

```
ET_EXEC:                            load_bias = 0
ET_DYN with no PT_INTERP:           load_bias = ELF_ET_DYN_BASE
ET_DYN with PT_INTERP:              ENOEXEC (dynamic linking deferred)
```

`ELF_ET_DYN_BASE` is a per-architecture deterministic constant in v1:

| Arch | ELF_ET_DYN_BASE |
|---|---|
| RV64 (Sv39 / Sv48) | `0x2AAAAAAA000` (≈ 2/3 of Sv48 user range) |
| LA64 | `0x2AAAAAAA000` |

These are nominal; the actual constants live in HAL per-arch headers and may change with the Sv-mode decision. The spec only requires that the value is page-aligned, deterministic, leaves room above for the heap and below for the stack, and is the same across all execs on a given build.

ASLR (random `load_bias`) is deferred. When the entropy subsystem and address-space randomization land, `load_bias` becomes `ELF_ET_DYN_BASE + (random & ASLR_MASK)`. The interface to phase 4 does not change.

### 8.8 PT_TLS
<!-- txdoc:EXEC-8-8-PT-TLS -->

The loader records PT_TLS into `ExecImagePlan.tls` if present. v1 does **not** instantiate the initial TLS image; the user TLS register is initialized to zero in phase 7 (`commit_exec_context`).

This is acceptable for static userlands that initialize TLS in their startup code from auxv (`AT_PHDR` + parsing PT_TLS at runtime is the standard musl approach for static-pie). It is **not** acceptable for userlands that require TLS to be live before entering `_start` — such userlands will crash on first TLS access in startup.

If the chosen target userland requires kernel-side TLS instantiation, this is promoted from deferred to v1-required. See §20 for risk tracking.

### 8.9 Errno mapping
<!-- txdoc:EXEC-8-9-ERRNO-MAPPING -->

```
load_exec_image errors:
    ENOEXEC — bad magic, wrong class, wrong endian, wrong machine,
              malformed phdr, e_phnum > MAX_PHDRS, e_phentsize wrong,
              header/phdr range overflows file, PT_INTERP present,
              PT_GNU_STACK requests executable stack, segment overlap,
              segment overflow, AT_PHDR uncomputable, ET_DYN with PT_INTERP
    ENOMEM  — phdr buffer or segment Vec allocation failed
    EIO     — underlying read error from PAGE_BACKED
```

`EACCES` is **not** produced by the loader. NOEXEC mount denial is phase 1 / 2; the loader never sees a NOEXEC binary.

### 8.10 Implementation note (non-normative)
<!-- txdoc:EXEC-8-10-IMPLEMENTATION-NOTE-NON-NORMATIVE -->

> The v1 implementation uses the `goblin` crate for ELF parsing, configured for `no_std` with the `alloc` feature:
>
> ```toml
> goblin = { version = "0.10", default-features = false,
>            features = ["alloc", "endian_fd", "elf64", "elf32"] }
> ```
>
> Only `goblin::elf::header::Header::parse` and `goblin::elf::program_header::ProgramHeader::parse` are called. The unified `goblin::elf::Elf::parse` is *not* used because it requires a contiguous full-file `&[u8]`, which is incompatible with our targeted-read model.
>
> `goblin` types do not escape `loader.rs`. Translation to txKernel-owned types (`ExecImagePlan`, `LoadSegment`, `TlsTemplate`) happens inline.
>
> This is an implementation choice. The architectural contract is the public `load_exec_image` function and the `ExecImagePlan` output type. A future implementation may swap parsers (the `elf` crate is no_std-clean and a viable alternative) without changing the spec.

---


## 9. Phase 4 — Prepare detached replacement
<!-- txdoc:EXEC-9-PHASE-4-PREPARE-DETACHED-REPLACEMENT -->

This is the largest reversible phase. It performs all remaining allocation and all remaining user-memory access. Every step that can fail must complete here. By the end of phase 4, every post-PoNR commit is reduced to a sequence of infallible mutations.

The "detached" in the section title refers to the new AddressSpace: it is built fully — segments, stack, brk, initial stack contents — but is held only by a `Cap<AddressSpace>` local to this script. No process or thread points to it. If phase 4 fails, the Cap drops; the AS reclaims; nothing observable changed.

### 9.1 Argv and envp copy from the old AS
<!-- txdoc:EXEC-9-1-ARGV-AND-ENVP-COPY-FROM-THE-OLD-AS -->

The argv and envp pointer arrays (and the strings they point to) live in the caller's old AS. They are read out via standard `copy_from_user` against the *current* AS, before any AS replacement.

```rust
    // 9.1.1 — copy argv (NULL-terminated array of pointers to strings).
    let argv_strings = copy_user_string_array(argv_ptr, ARG_MAX)
        .map_err(map_argv_error)?;
    //  ARG_MAX = 128 KB total argv + envp content (POSIX guidance: ≥ 4 KB).
    //  Returns E2BIG if total exceeds ARG_MAX, EFAULT on bad pointer.

    // 9.1.2 — copy envp.
    let envp_strings = copy_user_string_array(envp_ptr, ARG_MAX - argv_total)
        .map_err(map_envp_error)?;

    // 9.1.3 — derive cmdline snapshot.
    //          /proc/<pid>/cmdline shows argv strings separated by NULs.
    let cmdline = ExecCmdlineSnapshot::from_argv(&argv_strings);
```

`ARG_MAX` is the combined argv+envp byte budget. Linux uses `1/4 * RLIMIT_STACK` capped at `MAX_ARG_STRLEN`. v1 uses a fixed 128 KB; rlimit-derived sizing is deferred.

**Failure modes.**

| Errno | Cause |
|---|---|
| E2BIG | Combined argv+envp size exceeds ARG_MAX |
| EFAULT | argv or envp pointer is unmapped or unreadable |
| ENOMEM | Kernel-side string buffer allocation failed |

After this point, no further reads from the caller's old AS are needed. The path string was already copied in phase 0. Everything from here writes into kernel-heap buffers and the new (detached) AS.

### 9.2 Build the detached AddressSpace
<!-- txdoc:EXEC-9-2-BUILD-THE-DETACHED-ADDRESSSPACE -->

```rust
    // 9.2.1 — VM script: build the new AS from the image plan.
    let new_as: Cap<AddressSpace> = vm::scripts::build_aspace_from_image(
        &exe_file.rnode,    // for segment backing
        &image_plan,
    )?;
```

`build_aspace_from_image` is a new VM script (cross-doc edit V1) that performs:

1. Allocate a fresh `AddressSpace` (zone allocation; Cap returned).
2. For each `LoadSegment` in `image_plan.segments`:
   - If `mem_size > file_size`: emit two VmEntries.
     - File-backed: `[map_start, map_start + page_delta + file_size)`, `MAP_PRIVATE` against `exe_file.rnode`'s PageContainer at `file_page_offset`, prot from segment.
     - Anonymous BSS: `[ceil(map_start + page_delta + file_size, PAGE), map_end)`, RW (or whatever the segment prot says — usually RW for data BSS).
     - Note: the *partial* page where file content ends is part of the file-backed VmEntry. The COW machinery zeros the tail past `file_size` on first write to that page (standard semantics).
   - Else (`mem_size == file_size`): single file-backed VmEntry over `[map_start, map_end)`.
3. Allocate a stack VmEntry: anonymous, RW, `STACK_SIZE_DEFAULT` (8 MB minus a guard page).
4. Set `brk_current` and `brk_start` past the highest data segment's `map_end`, page-aligned.
5. Initialize the kernel high-half: shared by reference with all AddressSpaces.

The new AS holds its own `Cap<RNode>` references through each file-backed VmEntry's backing. The script's `exe_file.rnode` is not load-bearing for fault-time materialization; it is only used as a convenience to pass the same Cap into VM. After phase 4, the new AS's segment backings are independent.

**Failure modes.**

| Errno | Cause |
|---|---|
| ENOMEM | AddressSpace allocation, recipes BTree allocation, or VmEntry allocation failed |

### 9.3 Populate the initial user stack
<!-- txdoc:EXEC-9-3-POPULATE-THE-INITIAL-USER-STACK -->

The initial stack contains, at the new SP and growing downward:

```
high addresses
+----------------------------+
|  (string pool)             |   argv strings, envp strings, AT_RANDOM bytes
+----------------------------+
|  AT_NULL (auxv terminator) |
+----------------------------+
|  auxv entries              |
+----------------------------+
|  NULL                      |
+----------------------------+
|  envp pointers             |
+----------------------------+
|  NULL                      |
+----------------------------+
|  argv pointers             |
+----------------------------+
|  argc                      |   ← new SP (16-byte aligned per psABI)
+----------------------------+
low addresses
```

This layout is built in a kernel-side scratch buffer first, then written into the new AS via `vm::populate_detached_user_range`:

```rust
    // 9.3.1 — assemble auxv.
    let auxv = stack::build_auxv::<P>(&image_plan, &new_credential)?;
    //  See §9.4 for auxv content.

    // 9.3.2 — compute layout in a scratch buffer.
    let stack_image = stack::layout_initial_stack(
        &argv_strings, &envp_strings, &auxv,
    )?;
    //  Returns StackImage { bytes: Vec<u8>, sp_offset_from_top: usize }.

    // 9.3.3 — compute the user-VA base of the stack.
    let stack_top = STACK_TOP_DEFAULT;  // page-aligned per arch
    let new_sp = UserAddr(stack_top.0 - stack_image.sp_offset_from_top);
    debug_assert!(new_sp.0 % 16 == 0);   // psABI 16-byte alignment

    // 9.3.4 — write into the detached AS.
    //          NOT copy_to_user — we are not running in the new AS.
    let dst_base = UserAddr(stack_top.0 - stack_image.bytes.len());
    vm::populate_detached_user_range(
        &new_as, dst_base, &stack_image.bytes,
    )?;
```

`vm::populate_detached_user_range` is a new VM API (cross-doc edit V2):

```rust
/// Write `src` to the user-VA range [dst, dst + src.len()) in the
/// detached AddressSpace `new_as`. The current thread is NOT executing
/// in `new_as`; this resolves dst against new_as's recipes, materializes
/// the page (zero-filling for anonymous, since the stack is anon RW),
/// and copies via direct-map.
///
/// Distinct from copy_to_user: copy_to_user resolves against the
/// *current* thread's AS. populate_detached_user_range resolves against
/// the explicitly-provided AS. The new AS has no live PTEs yet, so this
/// is effectively "fault, then copy" without a fault path.
///
/// Errors: ENOMEM if frame allocation fails; EFAULT if dst+len falls
/// outside any VmEntry.
pub fn populate_detached_user_range(
    new_as: &Cap<AddressSpace>,
    dst: UserAddr,
    src: &[u8],
) -> Result<(), Errno>;
```

**Failure modes.**

| Errno | Cause |
|---|---|
| ENOMEM | Stack page allocation failed; auxv or stack image buffer allocation failed |
| EFAULT | Stack image too large for stack VmEntry (should not occur if STACK_SIZE_DEFAULT is chosen sanely) |

### 9.4 Auxv construction
<!-- txdoc:EXEC-9-4-AUXV-CONSTRUCTION -->

The auxiliary vector communicates per-image and per-system facts to userspace startup. Entries used in v1:

| Type | Source | Value |
|---|---|---|
| AT_PHDR | Loader | `image_plan.phdr_va` |
| AT_PHENT | Loader | `image_plan.phent` |
| AT_PHNUM | Loader | `image_plan.phnum` |
| AT_ENTRY | Loader | `image_plan.entry` |
| AT_BASE | Static | 0 (no interpreter in v1) |
| AT_PAGESZ | HAL | 4096 |
| AT_HWCAP | HAL | `P::arch_auxv_facts().hwcap` |
| AT_HWCAP2 | HAL | `P::arch_auxv_facts().hwcap2` (0 on RV64 unless the ABI says otherwise; relevant on LA64) |
| AT_PLATFORM | HAL | per-arch string ("riscv64" / "loongarch64") |
| AT_RANDOM | Entropy | 16 bytes via `random::get_bytes(&mut buf)` |
| AT_FLAGS | Static | 0 |
| AT_UID, AT_EUID, AT_GID, AT_EGID | Cred | from `new_credential` |
| AT_SECURE | Static | 0 (no privilege transition in v1) |
| AT_EXECFN | Stack | pointer to a copy of the path string in the stack string pool |
| AT_NULL | Static | 0 (terminator) |

Ownership split (per the reviewer's correction):

- **HAL** owns architecture/platform facts (`AT_HWCAP`, `AT_HWCAP2`, `AT_PAGESZ`, `AT_PLATFORM`).
- **Entropy subsystem** owns `AT_RANDOM`. v1 fallback: if the entropy subsystem is not yet seeded, use boot-time entropy if available; if not, log a warning and use a weak fixed pattern (this is a known v1 limitation, not a permanent design).
- **Loader** owns image-derived facts (`AT_PHDR`, `AT_PHENT`, `AT_PHNUM`, `AT_ENTRY`).
- **Cred** owns credential facts (`AT_UID`, etc.).
- **Stack builder** owns layout-derived facts (`AT_EXECFN`).

```rust
pub fn build_auxv<P: AuxvIf>(
    image_plan: &ExecImagePlan,
    new_credential: &NewCredential,
) -> Result<AuxvVec, Errno> {
    let mut auxv = AuxvVec::new();
    let arch_facts = P::arch_auxv_facts();
    let mut at_random = [0u8; 16];
    random::get_bytes(&mut at_random).unwrap_or_else(|_| {
        // v1 fallback: weak boot-seeded pattern. Logged.
        log::warn!("AT_RANDOM using weak fallback");
        boot_seed_random(&mut at_random);
    });

    auxv.push(AT_PHDR,     image_plan.phdr_va.0);
    auxv.push(AT_PHENT,    image_plan.phent as u64);
    auxv.push(AT_PHNUM,    image_plan.phnum as u64);
    auxv.push(AT_ENTRY,    image_plan.entry.0);
    auxv.push(AT_BASE,     0);
    auxv.push(AT_PAGESZ,   arch_facts.page_size);
    auxv.push(AT_HWCAP,    arch_facts.hwcap);
    auxv.push(AT_HWCAP2,   arch_facts.hwcap2);
    auxv.push(AT_FLAGS,    0);
    auxv.push(AT_UID,      new_credential.uid as u64);
    auxv.push(AT_EUID,     new_credential.euid as u64);
    auxv.push(AT_GID,      new_credential.gid as u64);
    auxv.push(AT_EGID,     new_credential.egid as u64);
    auxv.push(AT_SECURE,   0);
    auxv.push_random(at_random);   // 16 bytes embedded later as a string-pool ref
    auxv.push_platform(arch_facts.platform);
    auxv.push_execfn(/* string-pool ref to path */);
    auxv.push(AT_NULL,     0);

    Ok(auxv)
}
```

### 9.5 Prepare private fd_table
<!-- txdoc:EXEC-9-5-PREPARE-PRIVATE-FD-TABLE -->

```rust
    // 9.5.1 — prepare private fd_table.
    //          If frame.fd_table.shared_count() > 1 (CLONE_FILES sharing),
    //          allocate a fresh FdTable with the cloexec entries already
    //          absent. If shared_count() == 1, no copy needed; return a
    //          plan that closes cloexec entries in place at commit.
    let prepared_fd_table = fd_table::execution::prepare_exec_close_cloexec(
        &caller_proc.payload.frame.fd_table,
    )?;
    //  Errors: ENOMEM on allocation.
```

`prepare_exec_close_cloexec` is a new fd-table API. Its semantics:

```rust
pub fn prepare_exec_close_cloexec(
    fd_table: &Shared<FdTable>,
) -> Result<PreparedFdTable, Errno>;

pub enum PreparedFdTable {
    /// CLONE_FILES sharing: a fresh FdTable was allocated, with non-cloexec
    /// entries cloned (Cap<OpenFile> refcount bumped) and cloexec entries
    /// dropped. Commit replaces the Shared slot atomically.
    CowReplaced(Cap<FdTable>),

    /// No sharing: the existing FdTable is to be mutated in place at commit.
    /// Records which fds need closing (their indices and the Caps to drop).
    InPlaceClosePlan(CloseOnExecPlan),
}
```

**Why this shape.** The process `Frame` shared-slot invariant from [`PROCESS_v1.md §3`](../04_process-signals/PROCESS_v1.md) is that mutation through a shared process-frame slot with `strong_count > 1` requires COW. Exec must respect this: if another process or thread group shares the fd table via `CLONE_FILES`, we cannot remove cloexec entries from *their* view. We allocate a fresh table for our process; the other holders keep theirs unchanged.

For the common case (`strong_count == 1`), no copy is needed; we record the close plan and apply it in place at commit. The cost is `prepared_fd_table` carries a small list of (fd_index, Cap<OpenFile>) pairs.

Either variant of `PreparedFdTable` produces an infallible commit (§12.1). The fallible work — allocation, scanning, Cap cloning — happens here in phase 4.

### 9.6 Prepare private sig_actions
<!-- txdoc:EXEC-9-6-PREPARE-PRIVATE-SIG-ACTIONS -->

```rust
    // 9.6.1 — prepare private sig_actions.
    //          If frame.sig_actions.shared_count() > 1 (CLONE_SIGHAND sharing),
    //          allocate a fresh SigActionTable. Apply exec-reset semantics:
    //              - non-ignored handlers → SIG_DFL
    //              - SIG_IGN preserved
    //              - SIG_DFL preserved (already default)
    //          If shared_count() == 1, prepare an in-place reset plan.
    let prepared_sig_actions = sig_actions::execution::prepare_exec_reset(
        &caller_proc.payload.frame.sig_actions,
    )?;
    //  Errors: ENOMEM on allocation.
```

Same shape as fd_table:

```rust
pub fn prepare_exec_reset(
    sig_actions: &Shared<SigActionTable>,
) -> Result<PreparedSigActions, Errno>;

pub enum PreparedSigActions {
    CowReplaced(Cap<SigActionTable>),
    InPlaceResetPlan(SigResetPlan),
}
```

The reset semantics, fully:

| Pre-exec disposition | Post-exec disposition |
|---|---|
| User handler (`sa_handler != SIG_DFL && sa_handler != SIG_IGN`) | `SIG_DFL` |
| `SIG_IGN` | `SIG_IGN` (preserved) |
| `SIG_DFL` | `SIG_DFL` |

Signal *mask* is preserved. Signal *pending sets* (process-pending and surviving-thread-pending) are preserved. The thread's altstack is cleared inline in phase 7.2, not as part of the sig_actions prepare.

Other surviving-thread state: in v1, post-collapse there is exactly one thread (the leader). Its pending signals are preserved. The killed siblings' pending signals are gone with them.

After disposition reset, the signal **summary** (the bitmask of currently-non-default-non-ignored handlers) is recomputed. This is automatic if the summary is derived from the table on each commit.

### 9.7 Reserve credential mutation lane
<!-- txdoc:EXEC-9-7-RESERVE-CREDENTIAL-MUTATION-LANE -->

The reservation was acquired in phase 2 (§7.2.1). It is held through phase 4 to phase 7.3, where the new credential is committed. Nothing further is required here.

### 9.8 Reserve exe_file and cmdline slots
<!-- txdoc:EXEC-9-8-RESERVE-EXE-FILE-AND-CMDLINE-SLOTS -->

`ProcessPayload.exe_file` and `ProcessPayload.cmdline` are written in phase 7.4. The values to be installed are constructed here:

```rust
    // 9.8.1 — assemble the executable image reference.
    let exe_image_ref = ExecutableImageRef {
        mount: exe_file.mount.clone(),
        dentry: exe_file.dentry.clone(),
        rnode: exe_file.rnode.clone(),
    };
    //  These Cap clones bump refcounts; commit installs by simple pointer
    //  store. The phase-4 work is the Cap clone (could fail if SENTINEL_DEAD
    //  raced — but exe_file's targets cannot become dead since we hold them).
```

`AtomicSlot<Option<ExecutableImageRef>>` allows a single atomic swap at commit. The previous `exe_file` (if any — set by a prior exec on this process) drops naturally.

### 9.9 Last reversible point
<!-- txdoc:EXEC-9-9-LAST-REVERSIBLE-POINT -->

At the end of phase 4, the script holds:

```
new_as:               Cap<AddressSpace>            // new mappings, populated stack
new_credential:       NewCredential                // computed (v1: same as old)
prepared_fd_table:    PreparedFdTable              // COW or in-place close plan
prepared_sig_actions: PreparedSigActions           // COW or in-place reset plan
exe_image_ref:        ExecutableImageRef           // for /proc/<pid>/exe
cmdline:              ExecCmdlineSnapshot          // for /proc/<pid>/cmdline
new_sp, new_entry:    UserAddr                     // for trap context install
```

If any error occurs from here back to phase 0, the script returns Err. All the above drop on stack unwind:

- `new_as` drops; recipes BTree drops; segment Caps on PageContainers drop; AS reclaims.
- `new_credential`: by-value, dropped trivially.
- `prepared_fd_table`: if `CowReplaced(cap)`, the new FdTable Cap drops, reclaiming. If `InPlaceClosePlan`, the close plan drops; nothing was applied.
- `prepared_sig_actions`: same as fd_table.
- `exe_image_ref`: three Caps drop, refcounts decrement, no semantic change.
- `cmdline`: kernel-heap bytes free.
- `cred_reservation`: drop releases the lane; concurrent setuid (post-v1) resumes.

The caller's old AS, fd table, sig_actions, credential, exe_file are all unchanged.

After phase 5 (collapse) completes, this stack-unwind rollback is no longer available — collapse is irreversible. But in v1 collapse cannot fail, so the irreversibility is benign.

---

## 10. Phase 5 — Collapse thread group
<!-- txdoc:EXEC-10-PHASE-5-COLLAPSE-THREAD-GROUP -->

```rust
    // 10.1 — multi-thread check.
    if caller_proc.payload.thread_count.load(Ordering::Acquire) > 1 {
        // 10.2 — initiate group exit with is_exec=true.
        //          Wakes all sibling threads; they perform step_thread_exit.
        //          Returns when remaining_threads reaches 0 (caller is sole survivor).
        proc::execution::initiate_group_exit_for_exec(caller_proc).await;
        //  Infallible in v1. May yield through reactor while siblings exit.
    }
    //  After this point: thread_count == 1 (this thread).
```

Per [`PROCESS_v1.md §5`](../04_process-signals/PROCESS_v1.md), `GroupExitState { is_exec: true }` is installed on `payload.group_exit.state`. Sibling threads observe this on next AST or syscall return and call `step_thread_exit`. The initiator awaits the completion channel; when `remaining_threads` decrements to zero, the wait wakes, and the script proceeds with phase 6.

Group exit is one-shot per *episode* (cross-doc edit P1). After exec collapse completes and exec commits, the episode is cleared in phase 7.5. The process can later clone new threads, exec again, or call `exit_group` for a fresh episode.

In v1, non-leader exec was rejected in phase 0, so the surviving thread is always the thread-group leader. The `thread_count` after collapse is exactly 1.

**Failure modes.** None. Collapse is infallible. (When non-leader exec lands in Phase 2, the tid-rename step may introduce its own failures; those will be handled in their own phase.)

---

## 11. Phase 6 — Address-space visibility boundary
<!-- txdoc:EXEC-11-PHASE-6-ADDRESS-SPACE-VISIBILITY-BOUNDARY -->

This is the point of no return.

```rust
    // 11.1 — single-store atomic AS replacement.
    caller_proc.payload.frame.vm.replace(new_as);
```

`Frame.vm` is a process-frame shared slot whose value is identity-retaining `Cap<AddressSpace>` evidence (see [`PROCESS_v1.md §3`](../04_process-signals/PROCESS_v1.md)). `replace` is a single atomic store, decrementing the old AS's refcount and storing the new AS's Cap. The old AS reaches `strong_count == 0` (since v1's collapse guarantees no other threads of this process exist, and cross-process sharing of the AS is rare) and reclaims.

After this single store:

- The thread is still executing on the kernel stack of the syscall entry. Its trap frame still encodes the *old* user PC and SP; we will rewrite those in phase 7.6.
- Any external observer (procfs reader, ptrace tracer with this pid) that consults `frame.vm` from this point sees the new AS.
- The caller's userspace state is gone. There is no path back.
- However, *this thread* is still in kernel mode and has not yet returned to userspace. The stale trap frame is fine; we update it before returning.

The store itself cannot fail. `Shared::replace` is a typed atomic operation; the old AS's drop happens via refcount and may take time (PTE teardown, recipes drop) but does not affect the script's progress — it's deferred reclamation.

**This is the only step in exec that mutates `frame.vm`.** Phase 7 mutates other Frame fields (`fd_table`, `sig_actions`), but `vm` is touched exactly once.

### 11.1 Why this is the visibility boundary, not the linearization point
<!-- txdoc:EXEC-11-1-WHY-THIS-IS-THE-VISIBILITY-BOUNDARY-NOT-THE-LINEARIZATION-POINT -->

The full exec is a sequence of commits:

```
phase 6:  frame.vm replaced
phase 7.1: frame.fd_table replaced (+ cloexec closes)
phase 7.2: frame.sig_actions replaced (+ disposition reset)
           thread.alt_stack cleared
phase 7.3: payload.policy.cred replaced
phase 7.4: payload.exe_file installed; payload.cmdline installed
phase 7.5: payload.group_exit.state cleared
phase 7.6: thread trap context written (entry, sp, gprs, tp)
phase 8:   process_execd tracepoint emitted
```

Each is a separate commit point. There is no compound atomic primitive that swaps all of them at once.

**Same-process observers.** Cannot exist — collapse killed all sibling threads, and the calling thread has not returned to userspace. No same-process userspace code observes the multi-commit window.

**External observers.** May observe transient mixtures. For example, a parent's `/proc/<child>/maps` read concurrent with phase 7 may see the new AS but the old `cmdline`. This matches Linux. POSIX does not require atomic visibility of exec to external observers.

**ptrace.** When ptrace lands, `PTRACE_EVENT_EXEC` will be raised at phase 8. The tracer may inspect any of the new state. Mixed observation during phases 6–7 is bounded by ptrace not yet having received the event — tracers should consult only after the event.

The single canonical "exec happened" moment, for tooling purposes, is the `process_execd` tracepoint in phase 8.

---

## 12. Phase 7 — Infallible post-swap commits
<!-- txdoc:EXEC-12-PHASE-7-INFALLIBLE-POST-SWAP-COMMITS -->

Each subsection is one commit point. Order matters for observability (external observers see the sequence in this order) but not correctness — any prefix of these commits, observed by an external reader, is a self-consistent state.

The EXEC-PONR invariant (§15) constrains every action in this phase: no allocation, no user memory access, no I/O, no fallible computation. Each commit is a substrate primitive call or a single field store.

### 12.1 Install fd_table; close FD_CLOEXEC entries
<!-- txdoc:EXEC-12-1-INSTALL-FD-TABLE-CLOSE-FD-CLOEXEC-ENTRIES -->

```rust
    // 12.1.1 — install the prepared fd_table.
    match prepared_fd_table {
        PreparedFdTable::CowReplaced(new_fd_table) => {
            caller_proc.payload.frame.fd_table.replace(new_fd_table);
            //  Old fd_table refcount-decrements. If shared, peers keep theirs.
            //  Cloexec'd OpenFile Caps already absent from new_fd_table; their
            //  drops happened during prepare_exec_close_cloexec (phase 9.5).
        }
        PreparedFdTable::InPlaceClosePlan(plan) => {
            //  Apply the close plan to the existing fd_table.
            //  No allocation; just clearing slots and dropping Caps.
            fd_table::execution::apply_close_plan_infallible(
                &caller_proc.payload.frame.fd_table,
                plan,
            );
        }
    }
```

Either arm fires the same publication: dropping a `Cap<OpenFile>` for a cloexec'd file may decrement the fd's refcount to zero, which triggers the file's close path. The close path publishes via existing VFS attachments (fsnotify on the parent dentry, `close_port` on the OpenFile per [`SIGNAL_ATTACHMENTS_v1.md §3.2`](../04_process-signals/SIGNAL_ATTACHMENTS_v1.md)).

The publications are happening *post-PoNR*, but they are not exec-introduced; they are the existing close-path publications, already specified by VFS. Their timing relative to the AS swap is "after."

### 12.2 Install sig_actions; reset; clear altstack
<!-- txdoc:EXEC-12-2-INSTALL-SIG-ACTIONS-RESET-CLEAR-ALTSTACK -->

```rust
    // 12.2.1 — install the prepared sig_actions.
    match prepared_sig_actions {
        PreparedSigActions::CowReplaced(new_sig_actions) => {
            caller_proc.payload.frame.sig_actions.replace(new_sig_actions);
        }
        PreparedSigActions::InPlaceResetPlan(plan) => {
            sig_actions::execution::apply_reset_plan_infallible(
                &caller_proc.payload.frame.sig_actions,
                plan,
            );
        }
    }

    // 12.2.2 — clear the surviving thread's alt signal stack.
    caller_thread.payload.alt_stack.store(None, Ordering::Release);
```

No publication fires from sig_actions reset. Signal disposition is consulted at delivery time, not at registration; existing pending-signal delivery (if any) continues from the new disposition.

### 12.3 Install new credential
<!-- txdoc:EXEC-12-3-INSTALL-NEW-CREDENTIAL -->

```rust
    // 12.3.1 — install the new credential, consuming the reservation.
    cred::execution::commit_exec_credential_infallible(
        &caller_proc.payload.policy,
        cred_reservation,
        new_credential,
    );
```

In v1, this stores the same credential value back. The reservation drops, releasing the lane for future setuid/capset calls.

If Phase 2 cred work introduces credential publications (e.g., audit-style "credential changed" events), they fire here. v1 has no such publication.

### 12.4 Update exe_file and cmdline
<!-- txdoc:EXEC-12-4-UPDATE-EXE-FILE-AND-CMDLINE -->

```rust
    // 12.4.1 — install the executable reference (for /proc/<pid>/exe).
    caller_proc.payload.exe_file.store(Some(exe_image_ref), Ordering::Release);

    // 12.4.2 — install the cmdline snapshot (for /proc/<pid>/cmdline).
    caller_proc.payload.cmdline.store(cmdline);
```

Both are simple atomic stores. The previous `exe_file` (if any — there might be one from a prior exec on this process) drops, refcount-decrementing the previous mount/dentry/rnode Caps.

Procfs projections read these slots on demand; readers see the new values from the next read after this commit.

### 12.5 Clear GroupExit episode
<!-- txdoc:EXEC-12-5-CLEAR-GROUPEXIT-EPISODE -->

```rust
    // 12.5.1 — clear the episode.
    //          The episode armed in phase 5 is now discharged.
    caller_proc.payload.group_exit.state.store(None, Ordering::Release);
```

Per cross-doc edit P1, GroupExit is one-shot per episode. After this clear, the process can later clone new threads, exec again, or call exit_group with a fresh episode.

If `payload.thread_count.load() == 1` (which is the case post-collapse), this clear is unobservable in v1. When non-leader exec lands and threads can clone post-exec, this clear matters.

### 12.6 Install user trap context
<!-- txdoc:EXEC-12-6-INSTALL-USER-TRAP-CONTEXT -->

```rust
    // 12.6.1 — write the new user pc/sp/gprs into the thread's trap frame.
    let exec_ctx = ExecUserContext {
        entry: image_plan.entry,    // load_bias + e_entry
        sp:    new_sp,              // computed in phase 9.3
        // arch-specific: zero gprs except a0, set tp=0, etc.
        arch: ExecArchContext::initial(image_plan.arch),
    };
    thread_runtime::execution::commit_exec_context(caller_thread, exec_ctx);
```

`commit_exec_context` is a ThreadRuntime helper. It writes the saved trap frame through the HAL `TrapFrameMut` view for the current thread:

- **RV64.** Zero `x1`–`x31` except as needed for ABI; set `pc` (sepc) = entry; set `sp` (`x2`) = new_sp; set `tp` (`x4`) = 0. Floating-point state initialized to disabled.
- **LA64.** Zero `$r1`–`$r31` except as needed; set `era` = entry; set `$sp` (`$r3`) = new_sp; set TLS register (`$r2`) = 0. Float state likewise.

ABI-specific argument passing: SysV expects the *userspace startup* (libc's `_start`) to read argc/argv/envp from the stack pointer, so we don't need to set up a0 with anything special — the stack layout (§9.3) is the contract.

Exec does not directly mutate raw trap frames; the HAL/ThreadRuntime layer owns that detail. This keeps exec arch-independent at the script level.

**This is the last write before publish.** After this, the thread's saved user context represents the new program's initial state; the standard syscall return path will load these registers and resume userspace at the new entry point.

---

## 13. Phase 8 — Publish and enter userspace
<!-- txdoc:EXEC-13-PHASE-8-PUBLISH-AND-ENTER-USERSPACE -->

```rust
    // 13.1 — emit the script-level tracepoint.
    trace::process_execd(
        caller_proc.pid(),
        &exe_image_ref,
        argv_strings.argc(),
    );

    // 13.2 — return to userspace.
    //         The script returns ! — the syscall return path loads the new
    //         trap context (entry, sp, gprs) and resumes userspace at the
    //         new entry point. There is no syscall-return value because
    //         exec succeeded.
    //
    //         Pending signals (preserved per POSIX) are checked at AST in
    //         the standard return-to-userspace path; nothing exec-specific
    //         is required.
    unreachable!()
}
```

`trace::process_execd` is a RawTrace publication. It is fire-and-forget; subscribers (ftrace, perf, audit) consume asynchronously. If no subscribers are attached, the call is nop-patched per [`BUS_v1.md`](../01_substrate/BUS_v1.md). It cannot fail.

When ptrace is implemented, `PTRACE_EVENT_EXEC` will be raised here as a separate intercept (see §18). The intercept may park the thread until the tracer continues; this is the ptrace observation subsystem's responsibility, not exec's.

**The script returns `!`.** There is no successful return value to userspace because exec replaced the entire program; the next instruction after exec is the new program's entry point, not the syscall-exit handler's "set a0 to retval" path. The trap-return path takes the new pc and sp from the trap frame and dispatches; userspace resumes at `image_plan.entry` with the constructed stack at `new_sp`.

---

## 14. Failure modes and the rollback boundary
<!-- txdoc:EXEC-14-FAILURE-MODES-AND-THE-ROLLBACK-BOUNDARY -->

### 14.1 Pre-PoNR errno table
<!-- txdoc:EXEC-14-1-PRE-PONR-ERRNO-TABLE -->

The complete error map for phases 0–5:

| Phase | Errno | Cause |
|---|---|---|
| 0 | ENOSYS | Non-leader exec (v1) |
| 0 | EFAULT | Path pointer unmapped |
| 0 | ENAMETOOLONG | Path > PATH_MAX |
| 1 | ENOENT | Path component missing |
| 1 | ENOTDIR | Non-directory in path prefix |
| 1 | ELOOP | Symlink chain exceeded |
| 1 | EACCES | Search permission denied; **or** MS_NOEXEC mount |
| 2 | EACCES | No execute permission (mode bits, ownership) |
| 2 | EPERM | Privileged check failed (capability missing) |
| 2 | EAGAIN | Cred mutation reservation contended (cannot occur in v1) |
| 3 | ENOEXEC | Image format invalid (full set in §8.9) |
| 3 | ENOMEM | Loader buffer allocation failed |
| 3 | EIO | Underlying read error from binary's PageContainer |
| 4 | E2BIG | argv + envp exceeds ARG_MAX |
| 4 | EFAULT | argv/envp pointer unmapped or unreadable |
| 4 | ENOMEM | New AS, stack, fd_table COW, sig_actions COW, or auxv allocation failed |
| 5 | — | Collapse infallible in v1 |

Every pre-PoNR error returns to the caller in the *old* AS with `errno` set. The caller's userspace state is unchanged — pid, fds, mappings, signal state, stack pointer all as they were.

### 14.2 Post-PoNR: no errors permitted
<!-- txdoc:EXEC-14-2-POST-PONR-NO-ERRORS-PERMITTED -->

Phases 6–8 must not produce errors. Per the EXEC-PONR invariant (§15), they perform no allocation, no user memory access, no filesystem I/O, no fallible computation. All work that could have failed has completed in phase 4.

If a bug causes a kernel-detected fault past PoNR (e.g., a Cap upgrade fails because a SENTINEL_DEAD raced — which it shouldn't in v1 because we hold retention on everything we touch), the action is **fatal process termination via SIGKILL-equivalent**. The process cannot return to its old AS (it has been replaced) and cannot continue with corrupt new state.

In practice, no v1 path crosses PoNR with a possibility of failure. The fatal-termination path is a defensive backstop, not an expected outcome.

### 14.3 Rollback semantics
<!-- txdoc:EXEC-14-3-ROLLBACK-SEMANTICS -->

For a pre-PoNR failure at phase N, the resources held are exactly those constructed in phases 0..N. Rollback is by stack unwind: each Rust binding drops, releasing its retention. No explicit cleanup code is needed.

| Resource | Drop effect |
|---|---|
| `path_buf` | Heap free |
| `cred_snapshot` | Trivial |
| Witnesses (phase 1, 2) | Released with epoch guard |
| `cred_reservation` | Lane released |
| `rnode_cap`, `dentry_cap`, `mount_cap` | Refcount decrement; possible reclamation |
| `image_plan` | Vec<LoadSegment> freed; Cap refs in `interp` (none in v1) decrement |
| `argv_strings`, `envp_strings` | Heap free |
| `new_as` | AddressSpace reclaims; segment Caps on PageContainers decrement; recipes BTree drops; pmap entries drop |
| `prepared_fd_table` | If COW: new FdTable Cap reclaims, OpenFile Cap clones decrement. If in-place plan: plan drops, no application |
| `prepared_sig_actions` | Symmetric to fd_table |
| `exe_image_ref` | Three Caps decrement |
| `cmdline` | Heap free |

The script's Future resolves with `Err(errno)`; the syscall return path stores `errno` in the caller's register set and resumes userspace. The caller's AS, fd table, sig_actions, credential, exe_file are unchanged from before exec was called.

### 14.4 Post-collapse-pre-swap: the unreachable corner
<!-- txdoc:EXEC-14-4-POST-COLLAPSE-PRE-SWAP-THE-UNREACHABLE-CORNER -->

Between phase 5 (collapse complete) and phase 6 (AS swap), the process is single-threaded but the AS has not been replaced. If a failure could occur here, the process would be in an awkward state: siblings dead, but the leader still runs the old binary.

In v1, this corner is **unreachable**: phase 5 is infallible and phase 6 is a single store. There is no instruction between them that can fail.

Future-proofing: if non-leader exec adds tid-rename steps between collapse and swap, those steps must be infallible (or any failure must abort the whole process via fatal signal, since rolling back collapse is impossible — siblings cannot be resurrected). Phase 2 of cred work that introduces credential install failures must place those failures *before* collapse.

---

## 15. The EXEC-PONR invariant
<!-- txdoc:EXEC-15-THE-EXEC-PONR-INVARIANT -->

Stated formally:

> **EXEC-PONR.** After phase 6 (the address-space visibility boundary), the script performs no allocation, no user memory access, no filesystem I/O, and no fallible computation. Every action in phases 7 and 8 is a substrate primitive call, an infallible field store, or a previously-prepared infallible plan application. Any kernel-detected failure past this boundary is fatal process termination, not error recovery.

This invariant is the load-bearing rule that makes the exec script auditable. Without it, the multi-commit phase 7 sequence would have no defined behavior on partial failure; with it, partial failure is impossible.

### 15.1 What the invariant rules out in phase 7+
<!-- txdoc:EXEC-15-1-WHAT-THE-INVARIANT-RULES-OUT-IN-PHASE-7 -->

- **Heap allocation.** No `Vec::push`, no `Box::new`, no `alloc_kernel_buf`. All buffers needed in phase 7 are constructed in phase 4 and carried forward.
- **`copy_from_user` and `copy_to_user` on the old AS.** The old AS is gone after phase 6.
- **`copy_to_user` on the new AS.** Stack contents are written in phase 4 via `populate_detached_user_range`. No further user-memory writes are needed in phase 7.
- **Filesystem I/O.** The binary's PageContainer is now wired into the new AS's segment VmEntries; no further reads needed for exec proper. (Subsequent fault-time materialization is normal page-fault handling, not exec.)
- **Cap upgrades that can fail.** All upgrades happened in phase 2.
- **BTree operations that can fail.** Recipes BTree allocations happened in `build_aspace_from_image` (phase 4).

### 15.2 What the invariant permits in phase 7+
<!-- txdoc:EXEC-15-2-WHAT-THE-INVARIANT-PERMITS-IN-PHASE-7 -->

- **Atomic stores.** `Shared<T>::replace`, `AtomicSlot::store`, `AtomicU32::store`.
- **Refcount operations.** `Cap` clone bumps a refcount, and `Cap` drop decrements one. Both are infallible.
- **Substrate primitive calls.** `index::commit`, `index::withdraw_commit`, etc., on prepared inputs.
- **Plan application.** Applying a prepared `CloseOnExecPlan` or `SigResetPlan` — bounded loops over precomputed indices.
- **Trap-frame writes.** `commit_exec_context` writes to the thread's saved trap frame (kernel memory, not user memory).
- **Tracepoint emission.** `trace::process_execd` is fire-and-forget on a RawTrace.

### 15.3 Enforcement
<!-- txdoc:EXEC-15-3-ENFORCEMENT -->

The invariant is enforced by:

- **Code review.** `scripts/process/exec.rs` phase 7 and phase 8 sections are auditable in isolation; they should be ~30 lines of straight-line code with no error-returning calls.
- **Type system.** The prepared-plan types (`PreparedFdTable`, `PreparedSigActions`) are constructed only in phase 4 helpers; their `apply_*_infallible` consumers cannot return Result.
- **Lints.** A future lint can verify that phase 7+ functions called from `script_execve` past the swap point have signatures returning `()` or `!`, never `Result`.

There is no runtime check; the invariant is structural.

---

## 16. Signal-reset semantics
<!-- txdoc:EXEC-16-SIGNAL-RESET-SEMANTICS -->

The complete reset rules, applied in phase 7.2:

```
On successful exec:
    - Dispositions set to user handlers (sa_handler != SIG_DFL && != SIG_IGN)
        → reset to SIG_DFL.
    - Dispositions set to SIG_IGN → preserved as SIG_IGN.
    - Dispositions already at SIG_DFL → preserved.
    - sa_flags fields → preserved on the disposition entries that survive
        (i.e., on SIG_IGN entries). Reset entries (SIG_DFL) get default flags.
    - sa_mask fields → reset to empty per POSIX (the per-handler mask is
        meaningless for SIG_DFL and SIG_IGN dispositions).

    - Process-pending signal queue → preserved.
        (POSIX: signals pending before exec remain pending after.)
    - Thread-pending queue of the surviving thread → preserved.
    - Thread-pending queues of killed siblings → discarded with the threads.

    - Signal mask → preserved.
    - Alt signal stack (SS_ONSTACK / SS_DISABLE flags) → cleared (set to
        the disabled / no-altstack default).

    - Real-time signal queues (rt_sigqueue contents) → preserved per POSIX.
        (v1 may not implement real-time signal queuing per
        PROCESS_v1 §11.2 deferrals.)

    - Restart-flag for system calls → not applicable; exec replaces the
        program, no in-progress syscalls survive.

After disposition reset, the signal summary (the bitmask of dispositions
that are neither SIG_DFL nor SIG_IGN — i.e., signals with caught handlers)
is recomputed. With user handlers reset, the summary collapses to the
SIG_IGN entries, allowing the signal delivery path to short-circuit
delivery for any signal whose disposition becomes default.
```

The key POSIX subtleties:

- **SIG_IGN preservation.** A program that explicitly ignored SIGPIPE before exec'ing wants the new program to see SIGPIPE ignored too. POSIX requires this.
- **Pending preservation.** A program that received SIGTERM just before exec'ing should still receive it; the new program's `_start` will deliver the pending signal to whatever the (now-default) disposition is — typically termination.
- **Mask preservation.** A program that masked SIGCHLD before exec'ing wants the new program to start with SIGCHLD masked. The new program may immediately unmask if it chooses, but the kernel does not unmask on its behalf.
- **Altstack clear.** The altstack VA was in the old AS; preserving the altstack pointer would be a pointer into now-invalid memory. Clearing is correct.

---

## 17. Mount-witness role
<!-- txdoc:EXEC-17-MOUNT-WITNESS-ROLE -->

The mount-witness split (§3.2) carries two distinct concerns: NOEXEC denies, NOSUID modifies.

### 17.1 NOEXEC
<!-- txdoc:EXEC-17-1-NOEXEC -->

```rust
mount::checks::require_exec_permitted(mount_w: MountWitness)
    -> Result<ExecMountWitness, Errno>;
//  EACCES if the mount has MS_NOEXEC.
//  Witness consumed (linear); on success, ExecMountWitness encodes
//  "this mount permits execute."
```

A mount with `MS_NOEXEC` denies exec across all paths in that mount. The check is **mount-level**, not file-level: even a file with executable mode bits cannot be executed if it lives under a NOEXEC mount.

This matches Linux. POSIX does not require it; it's a security feature for mounts of untrusted content (`/tmp` mounted noexec, removable media, etc.).

The check is a check, not a refinement: if the witness is refused, the operation fails. There is no fallback path.

### 17.2 NOSUID
<!-- txdoc:EXEC-17-2-NOSUID -->

```rust
mount::checks::observe_suid_policy(mount_w: MountWitness) -> SuidWitness;
//  Infallible. SuidWitness records whether MS_NOSUID is set.
//  Consumed by cred::execution::compute_exec_credentials.
```

NOSUID does not deny exec. It modifies credential computation:

- If the binary has `S_ISUID` mode bit and the mount is NOT NOSUID: setuid takes effect; effective uid becomes the file's owner.
- If the binary has `S_ISUID` mode bit and the mount IS NOSUID: setuid is suppressed; effective uid is preserved.
- Same for `S_ISGID` and effective gid.
- File capabilities (Linux extended attribute) similarly suppressed under NOSUID.

In v1, since `compute_exec_credentials` returns the unchanged credential, `SuidWitness` is observed but has no effect. The wiring is in place for Phase 2.

### 17.3 Other mount flags
<!-- txdoc:EXEC-17-3-OTHER-MOUNT-FLAGS -->

- **MS_NODEV.** Affects whether device nodes can be opened on the mount. Inert during exec; if the binary opens a device, MS_NODEV applies at open time.
- **MS_RDONLY.** Read-only mount. Inert during exec; affects writes only.
- **MS_NOATIME.** Suppresses atime updates. Inert during exec; affects atime semantics on subsequent reads of the binary.

The mount-witness checks are minimal: NOEXEC denial and NOSUID observation. Adding more mount-driven exec semantics (e.g., MAP_DENYWRITE-style write inhibition during exec) is deferred.

---

## 18. Tracepoint catalog row
<!-- txdoc:EXEC-18-TRACEPOINT-CATALOG-ROW -->

The `process_execd` tracepoint is a script-level publication, fired from the exec script's publish phase rather than from any subsystem's step. It announces the completion of an exec with the new exe-file reference and argc.

### 18.1 Catalog entry
<!-- txdoc:EXEC-18-1-CATALOG-ENTRY -->

For [`SIGNAL_ATTACHMENTS_v1.md`](../04_process-signals/SIGNAL_ATTACHMENTS_v1.md) §3 (Process or new "Scripts" section):

| Entity | Carrier | Wire | Transition | Polarity | Fired from | Subscribers | Projection link |
|---|---|---|---|---|---|---|---|
| ProcessIdentity | RawTrace | `process_trace` | Exec commit completed | `emit({pid, exe_file, argc})` | `scripts/process/exec.rs::publish` (phase 8) | ftrace, perf, audit, ptrace (when implemented) | — |

The wire `process_trace` is shared across other Process tracepoints (process_forked, process_exited). `process_execd` is one event variant on this wire.

### 18.2 Why script-level publication is appropriate here
<!-- txdoc:EXEC-18-2-WHY-SCRIPT-LEVEL-PUBLICATION-IS-APPROPRIATE-HERE -->

Per [`SIGNAL_ATTACHMENTS_v1.md §1`](../04_process-signals/SIGNAL_ATTACHMENTS_v1.md), most attachments fire from a subsystem's `execution/step_*` commit point. Script-level publications are the exception, not the rule. They are appropriate when:

- The publication marks a cross-subsystem transition that does not correspond to any single subsystem's commit.
- The set of subsystem commits that comprise the transition is too granular to attach to (firing from each would produce a noisy stream of correlated events).
- The audience is interested in the script-as-a-whole, not in the per-peer commits.

All three apply to `process_execd`. ftrace and audit consumers want one event per exec, not seven.

### 18.3 Tracepoint payload
<!-- txdoc:EXEC-18-3-TRACEPOINT-PAYLOAD -->

```rust
pub struct ProcessExecdTrace {
    pub pid: Pid,
    pub exe_file: ExecutableImageRef,    // for path reconstruction
    pub argc: u32,
    // argv strings not in payload — too large; subscribers can read
    // /proc/<pid>/cmdline if they want them, with the standard
    // post-commit caveats.
}
```

The trace is **not authoritative**. Per SIG-1 and SIG-2, signals are not truth and wires are not state. The authoritative state is on `ProcessPayload.exe_file` and the AddressSpace's recipes BTree. Tracers that need authoritative state must consult the projections (procfs) or the structures directly.

### 18.4 PTRACE_EVENT_EXEC (deferred)
<!-- txdoc:EXEC-18-4-PTRACE-EVENT-EXEC-DEFERRED -->

When ptrace lands, exec will additionally raise `PTRACE_EVENT_EXEC` here as an intercept (per [`CONCEPTS_v4.md`](../00_meta-framework/CONCEPTS_v4.md)'s script/intercept vocabulary). The intercept may park the thread until the tracer continues. Unlike the tracepoint, the ptrace event is consumed by exactly one tracer and is part of the operation's control flow.

The ptrace intercept fires *after* the tracepoint, ensuring tracepoint subscribers see the event before the thread potentially blocks for tracer interaction.

---

## 19. Cross-subsystem composition
<!-- txdoc:EXEC-19-CROSS-SUBSYSTEM-COMPOSITION -->

The full sequence, with witness and Cap flow shown as in [`SUBSYSTEM_ANATOMY_v2_1.md §9`](../00_meta-framework/SUBSYSTEM_ANATOMY_v2_1.md)'s fork example:

```rust
async fn script_execve(path: UserAddr, argv: UserAddr, envp: UserAddr)
    -> Result<!, Errno>
{
    let caller_thread = current_thread();
    let caller_proc = caller_thread.process();

    // ─── Phase 0: Prelude ─────────────────────────────────────────────
    if !proc::checks::is_thread_group_leader(caller_thread) {
        return Err(Errno::ENOSYS);
    }
    let cred_snapshot = caller_proc.payload.policy.cred.snapshot();
    let path_buf = copy_path_from_user(path)?;

    // ─── Phase 1: Resolve and mount policy (under one guard) ─────────
    let guard = epoch::guard();
    let resolved      = vfs::checks::require_resolved(&path_buf, &caller_proc.payload.frame, &guard)?;
    let exec_mount_w  = mount::checks::require_exec_permitted(resolved.mount_w, &guard)?;
    let suid_w        = mount::checks::observe_suid_policy(resolved.mount_w, &guard);

    // ─── Phase 2: Authorization and credential plan (same guard) ─────
    let cred_reservation = cred::execution::reserve_exec_credential_lane(
        &caller_proc.payload.policy, cred_snapshot,
    )?;
    let auth_w = cred::checks::require_executable(
        resolved.rnode_w, exec_mount_w, &cred_reservation.snapshot, &guard,
    )?;
    let new_credential = cred::execution::compute_exec_credentials(
        auth_w, suid_w, &cred_reservation,
    );

    // Promote witnesses to retention; release guard.
    let exe_file = ExecutableFile {
        rnode:  resolved.rnode_w.upgrade()?,
        mount:  resolved.mount_w.upgrade()?,
        dentry: resolved.dentry_w.upgrade()?,
    };
    drop(guard);

    // ─── Phase 3: Load executable image plan (may yield on file I/O)─
    let image_plan = loader::load_exec_image(&exe_file).await?;

    // ─── Phase 4: Prepare detached replacement ───────────────────────
    let argv_strings = copy_user_string_array(argv, ARG_MAX)?;
    let envp_strings = copy_user_string_array(envp, ARG_MAX - argv_strings.size())?;
    let cmdline      = ExecCmdlineSnapshot::from_argv(&argv_strings);

    let new_as = vm::scripts::build_aspace_from_image(&exe_file.rnode, &image_plan)?;

    let auxv = stack::build_auxv::<P>(&image_plan, &new_credential)?;
    let stack_image = stack::layout_initial_stack(&argv_strings, &envp_strings, &auxv)?;
    let new_sp = UserAddr(STACK_TOP_DEFAULT.0 - stack_image.sp_offset_from_top);
    vm::populate_detached_user_range(
        &new_as, UserAddr(STACK_TOP_DEFAULT.0 - stack_image.bytes.len()), &stack_image.bytes,
    )?;

    let prepared_fd_table    = fd_table::execution::prepare_exec_close_cloexec(&caller_proc.payload.frame.fd_table)?;
    let prepared_sig_actions = sig_actions::execution::prepare_exec_reset(&caller_proc.payload.frame.sig_actions)?;

    let exe_image_ref = ExecutableImageRef {
        mount:  exe_file.mount.clone(),
        dentry: exe_file.dentry.clone(),
        rnode:  exe_file.rnode.clone(),
    };

    // ── Last reversible point. Past here, EXEC-PONR applies. ────────

    // ─── Phase 5: Collapse thread group ──────────────────────────────
    if caller_proc.payload.thread_count.load(Ordering::Acquire) > 1 {
        proc::execution::initiate_group_exit_for_exec(caller_proc).await;
    }

    // ─── Phase 6: Address-space visibility boundary (PoNR) ───────────
    caller_proc.payload.frame.vm.replace(new_as);

    // ─── Phase 7: Infallible post-swap commits ───────────────────────
    install_prepared_fd_table(&caller_proc.payload.frame.fd_table, prepared_fd_table);
    install_prepared_sig_actions(&caller_proc.payload.frame.sig_actions, prepared_sig_actions);
    caller_thread.payload.alt_stack.store(None, Ordering::Release);
    cred::execution::commit_exec_credential_infallible(
        &caller_proc.payload.policy, cred_reservation, new_credential,
    );
    caller_proc.payload.exe_file.store(Some(exe_image_ref), Ordering::Release);
    caller_proc.payload.cmdline.store(cmdline);
    caller_proc.payload.group_exit.state.store(None, Ordering::Release);
    thread_runtime::execution::commit_exec_context(
        caller_thread,
        ExecUserContext {
            entry: image_plan.entry,
            sp:    new_sp,
            arch:  ExecArchContext::initial(image_plan.arch),
        },
    );

    // ─── Phase 8: Publish and enter userspace ────────────────────────
    trace::process_execd(caller_proc.pid(), &exe_image_ref, argv_strings.argc());
    unreachable!()
}
```

The script is roughly 80 lines of substantive code, plus comments. Most of the *behavior* lives in the peer functions; the script's job is to call them in the right order with the right rollback semantics.

Compare the structure to fork in [`SUBSYSTEM_ANATOMY_v2_1.md §9`](../00_meta-framework/SUBSYSTEM_ANATOMY_v2_1.md):

- **Fork** is one composite step: phase 1 collects witnesses, phase 2 upgrades, phase 3 reserves per-subsystem, phase 4 commits per-subsystem in order, phase 5 publishes per-subsystem.
- **Exec** is a script over many steps: phases 1–4 do the witness/upgrade/reserve work, but the actual mutations span phases 6–8 with the PoNR boundary in the middle. The fork pattern doesn't apply directly because exec has a halfway-irreversible commit (phase 6) that the fork pattern does not.

Both share the witness-then-Cap flow and the cross-subsystem reservation+commit shape; they differ in the irreversibility profile.

---

## 20. v1 scope and tech debt
<!-- txdoc:EXEC-20-V1-SCOPE-AND-TECH-DEBT -->

### 20.1 In scope
<!-- txdoc:EXEC-20-1-IN-SCOPE -->

- Static ELF binaries (ET_EXEC).
- Static PIE (ET_DYN without PT_INTERP).
- Leader-only exec.
- argv, envp, full auxv (AT_PHDR, AT_PHENT, AT_PHNUM, AT_ENTRY, AT_BASE, AT_PAGESZ, AT_HWCAP, AT_HWCAP2, AT_PLATFORM, AT_RANDOM, AT_FLAGS, AT_UID/EUID/GID/EGID, AT_SECURE, AT_EXECFN).
- FD_CLOEXEC closure with Shared<FdTable> COW.
- Signal disposition reset with Shared<SigActionTable> COW.
- Altstack clear.
- Mount NOEXEC denial.
- Mount NOSUID observation (no effect in v1; wiring for Phase 2).
- Thread-group collapse via GroupExit.
- exe_file and cmdline persistence for procfs.
- process_execd tracepoint.

### 20.2 Deferred
<!-- txdoc:EXEC-20-2-DEFERRED -->

| Feature | Defer reason | Phase |
|---|---|---|
| Non-leader execve | Tid-rename design needed | Phase 2 |
| PT_INTERP / dynamic linking | Second image load + AT_BASE machinery | Phase 2 |
| setuid / setgid (S_ISUID / S_ISGID) | Cred subsystem Phase 2 | Phase 2 |
| File capabilities | xattr support + cred Phase 2 | Phase 2 |
| AT_SECURE | Driven by suid/caps; always 0 in v1 | Phase 2 |
| ASLR | Entropy subsystem maturity | Phase 2 |
| Personality flags | Linux compat tail | Phase 3+ |
| MAP_DENYWRITE on text segments | Tracking writers vs. mappers | Phase 3+ |
| PT_TLS kernel-side initialization | Risk; see §20.3 | v1-promotable |
| `execveat(AT_EMPTY_PATH)` | Trivial extension; deferred for scope | Phase 2 |
| `PTRACE_EVENT_EXEC` | Observation subsystem | Observation |
| Real-time signal queue preservation | `PROCESS_v1 §11.2` defers RT queues | Phase 2 |

### 20.3 v1 risks
<!-- txdoc:EXEC-20-3-V1-RISKS -->

**(R1) PT_TLS may need kernel-side setup.** The chosen userland (musl-static or similar) may require TLS to be live before `_start`. If so, this is promoted from §8.8 deferred to v1-required: phase 4 allocates a TLS init region, copies the TLS template, sets the arch TLS register accordingly. This is a self-contained extension; the loader already records the TlsTemplate.

**(R2) AT_RANDOM weak fallback.** If the entropy subsystem is not seeded by exec time, AT_RANDOM uses boot-seeded weak entropy. Userlands using AT_RANDOM for stack canary init will get predictable canaries. Mitigation: ensure entropy is seeded before init runs exec; failing that, log the weakness so it's visible.

**(R3) Phase 4's allocation footprint.** `build_aspace_from_image`, the stack image, the argv/envp buffers, and the COW-prepared fd_table/sig_actions all allocate. A process exec'ing with 256 fds (most cloexec) under tight memory may OOM. v1 accepts this; the EXEC-PONR invariant ensures any OOM is pre-PoNR and recoverable.

**(R4) MAP_DENYWRITE absence.** A concurrent writer to the binary file can modify text the executing process is running. This is a known Linux behavior pre-`MAP_DENYWRITE`; in our model the writes go through the binary's PageContainer, and the COW machinery means the writer's pages are not the executor's pages — but if the writer extends the file or modifies pages before the executor faults them in, there is a window. Acceptable for v1; a real concern only if the userland self-modifies running binaries (rare).

---

## 21. Summary
<!-- txdoc:EXEC-21-SUMMARY -->

Exec is a script — not a subsystem — that composes seven peers across eight phases, with one reversibility boundary (phase 5 → 6, the point of no return) and one visibility boundary (phase 6 → 7, the address-space replacement). Phases 0–5 are reversible: any failure drops prepared resources and returns errno. Phase 6 is a single Shared<AddressSpace> store. Phases 7–8 are infallible by the EXEC-PONR invariant.

The seven peers each have a focused role:

```
VFS       resolves the executable
Mount     supplies NOEXEC / NOSUID policy through witnesses
Cred      authorizes execute, computes new credentials, holds a mutation reservation
Loader    parses ELF into a parser-agnostic ExecImagePlan (parser library is an implementation choice; see §8.10)
VM        builds a detached AddressSpace, populates initial stack, swaps at PoNR
Process   coordinates GroupExit, preserves identity, holds exe_file/cmdline
FD/Signal/ThreadRuntime
          install per-Frame replacements: fd_table, sig_actions, trap context
```

Procfs is not a peer in the sequencing sense; it consumes projections of state set by exec.

The Shared<T> COW preparation pattern (§9.5, §9.6) is the load-bearing mechanism for fd_table and sig_actions: all allocation happens in phase 4, and phase 7's installation is reduced to atomic-store + bounded-loop plan application. The credential mutation reservation (§7) prevents racing setuid; v1 takes the reservation even though it doesn't yet change credentials, keeping the Phase 2 patch local.

The detached-AddressSpace model (§9.2, §11) is the central architectural improvement over the obvious "teardown + rebuild" implementation. Building the new AS fully off to the side, then swapping at PoNR, makes phase 6 atomic and lets all fallible work happen reversibly in phase 4. `vm::populate_detached_user_range` (cross-doc edit V2) is the new VM API that makes this possible.

The loader (§8) reads only the ELF header and program-header table — bounded targeted reads via `read_exact_at` (cross-doc edit B1) — and outputs a parser-agnostic `ExecImagePlan` carrying page-rounded `LoadSegment` data. The choice of parser library is an implementation detail per §8.10, not part of the architectural contract.

About 1500 lines of spec covering ~80 lines of script. The high spec-to-code ratio reflects exec's nature: most of the design work is in the boundaries between peers, not in the script's own logic.

---

## 22. References
<!-- txdoc:EXEC-22-REFERENCES -->

- [`CONCEPTS_v4.md`](../00_meta-framework/CONCEPTS_v4.md) — basis claims, script/wait vocabulary, publication, and carve-outs.
- [`INVARIANTS_v4.md`](../00_meta-framework/INVARIANTS_v4.md) — `STEP-*`, `SCRIPT-*`, `EXEC-*`, and publication rules.
- [`SUBSYSTEM_ANATOMY_v2_1.md`](../00_meta-framework/SUBSYSTEM_ANATOMY_v2_1.md) §3.6 (compositional scripts), §3.7 (point of no return), §9 (cross-subsystem scripts).
- [`STEP_MODEL_v1.md`](./STEP_MODEL_v1.md) — five-phase discipline.
- [`PROCESS_v1.md`](../04_process-signals/PROCESS_v1.md) §5 (group exit), §7.2 (script_execve skeleton), §11.2 (deferrals), §3 (Frame shared slots).
- [`VM_v1_2.md`](../03_memory-vm/VM_v1_2.md) §5.7 (detached exec address-space construction).
- [`PAGE_BACKED_v1.md`](../03_memory-vm/PAGE_BACKED_v1.md) — PageContainer, materialize_page, `read_exact_at`.
- [`VFS_CHECKS_V2.1.md`](../05_filesystem/VFS_CHECKS_V2.1.md) — path resolution, witnesses.
- [`SIGNAL_v1.md`](../04_process-signals/SIGNAL_v1.md) — disposition semantics.
- [`SIGNAL_ATTACHMENTS_v1.md`](../04_process-signals/SIGNAL_ATTACHMENTS_v1.md) — attachment catalog.
- [`BUS_v1.md`](../01_substrate/BUS_v1.md) — RawTrace primitive used by `process_execd`.
