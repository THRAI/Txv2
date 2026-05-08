# Interface drift audit (2026-05-07)

## Scope + method

Read 14 active design docs (HAL_v1, EXEC_v1, PROCESS_v1, SIGNAL_v1,
THREAD_RUNTIME_v1, REACTOR_v0, SCHEDULER_v0, VM_v1_2, PAGE_BACKED_v1,
VFS_CHECKS_V2.1, MOUNT_v1, DEVICE, TTY, INDEX) and sampled ~30 code
files across `tx-hal`, `tx-substrate`, `tx-subsystems`, `tx-fs`,
`tx-scripts`, `tx-shims`, `tx-kernel`, and `tx-reactor`. Cross-
referenced against the four most recent decision notes
(`2026-05-06-pre-elf-runtime-completion`, `-elf-loader-and-execve`,
`-fork-clone-wait4`, `-dac-and-setuid`) which document per-slice drift
inline. Read-only audit; no code changes.

## Drift findings — by subsystem

### HAL (`crates/tx-hal/src/`)

- **`RawTrapFrame` not exposed at trait level.** Spec
  (`HAL_v1.md:1302-1313`) requires `TrapIf::type RawTrapFrame; fn
  classify(tf: &Self::RawTrapFrame); fn view(tf: &Self::RawTrapFrame);
  fn view_mut; unsafe fn return_to_userspace(tf: &Self::RawTrapFrame)`.
  Code (`tx-hal/src/trap.rs:281-315`) defines `TrapIf` with only
  `install_minimal_trap_vector`, `install_kernel_trap_vector`,
  `install_user_trap_vector`, `classify_trap(snapshot:
  TrapFrameSnapshot)`, `snapshot_trap`, and
  `enter_userspace_with_context(_ctx: UserTrapContext) -> !` (default
  panic). The portable `RawTrapFrame` associated type, the
  `TrapFrameView::view`/`view_mut` constructors, and
  `return_to_userspace(tf: &RawTrapFrame)` are board-internal on
  RV64 today. **Verdict: Tier-1 drift (fix code to match)** — flagged
  in every recent decision note's follow-up list (pre-ELF #7,
  elf-loader #5, fork/clone/wait4 #7, dac+setuid #7).

- **`TrapFrameView` / `TrapFrameMut` exist but are constructed via
  `from_raw_parts` only.** `tx-hal/src/trap.rs:174-185` exposes
  `unsafe fn from_raw_parts` rather than the spec's
  `TrapIf::view`/`view_mut` associated functions. RV64 board uses
  this directly; portable view/view_mut fns absent. **Verdict:
  Tier-1 drift (fix code to match)** — same root cause as the
  `RawTrapFrame` gap.

- **`TrapIf::enter_userspace_with_context(_ctx: UserTrapContext)` is
  the only return path.** Spec
  (`HAL_v1.md:1333`) names this `unsafe fn return_to_userspace(tf:
  &Self::RawTrapFrame) -> !`. Code (`tx-hal/src/trap.rs:312-314`)
  ships `enter_userspace_with_context` taking `UserTrapContext`.
  Plan B writeback discipline (pre-ELF Wave 3) chose this shape over
  the trap-frame-based one for AST drain ordering. **Verdict:
  Tier-1 drift (fix doc to match)** — the codebase's choice is
  correct per `txdoc:THREAD-5-4-THE-TWO-SITE-DISCIPLINE`; the spec
  predates the writeback decision.

- **`EntropyIf` trait exists in code, absent from spec.** Code
  (`tx-hal/src/lib.rs:1126-1149`) defines `pub trait EntropyIf { fn
  fill_random(out: &mut [u8]) }` with deterministic xorshift64
  default and is added to the `TxPlatform` supertrait
  (`tx-hal/src/lib.rs:1418`). Spec `HAL_v1.md` has no occurrence of
  `EntropyIf` or `fill_random`. EXEC_v1's `9.4 Auxv construction`
  delegates `AT_RANDOM` to "Entropy subsystem"
  (`EXEC_v1.md:923-924`); the impl now sources it through HAL.
  **Verdict: Tier-1 drift (fix doc to match)** — needs a HAL_v1 §
  for EntropyIf + an EXEC_v1 §9.4 amendment that names HAL as the
  AT_RANDOM source.

- **`IrqIf::UART_IRQ` associated const.** Code
  (`tx-hal/src/lib.rs:1163`) declares `const UART_IRQ: u32 = 0` on
  `IrqIf`. Spec `HAL_v1.md:1775-1804` defines `IrqIf` with only
  `MAX_IRQ`, `claim`, `complete`, `mask`, `unmask`, `set_priority`,
  `install_dispatch_table`. **Verdict: Tier-2 drift (fix doc to
  match)** — pre-ELF Open Q #6 documented the placement decision;
  spec needs the const.

- **IRQ registration: code uses explicit
  `register_irq_handler`; spec describes `linkme`.** Spec
  `HAL_v1.md:1832-1845` shows `#[linkme::distributed_slice]
  pub static IRQ_HANDLERS: [IrqHandlerRegistration]`. Code
  (`tx-kernel/src/irq.rs`) ships an explicit `pub fn
  register_irq_handler(irq, fn)` table installed in
  `install_irq_handlers::<P>` from `init.rs`. Pre-ELF Open Q #4
  decided the explicit-registration shape (7 reasons documented in
  the slice plan). **Verdict: Tier-1 drift (fix doc to match)** —
  the linkme path is rejected by the active design.

### Substrate (`crates/tx-substrate/src/`)

- **`AtomicSlot<T>` lives in `tty::structure::identity`, not
  `tx_substrate`.** `tx-subsystems/src/tty/structure/identity.rs:72-115`
  defines `pub struct AtomicSlot<T>` with `TODO(Phase G): replace
  with the real AtomicSlot<T>` (line 13, 69). `process/structure.rs:39`
  imports it from there: `use crate::tty::structure::identity::{AtomicSlot,
  TtyIdentity}`. Same slot used for `ProcessPayload.aspace:
  AtomicSlot<Cap<AddressSpace>>` (`process/structure.rs:547`).
  ELF-loader follow-up #8 already flags the migration. **Verdict:
  Tier-2 drift (fix code; mechanical cleanup)** — substrate is the
  natural home per `CONCEPTS_v4`.

- **`SpinMutex` location now correct.** `tx-substrate/src/sync.rs:15-70`
  exports `SpinMutex` / `SpinMutexGuard`; `tx-substrate/src/lib.rs:29`
  re-exports at crate root. Pre-ELF Phase 1 migration (commit
  `04edaa6`) deleted the duplicate inline shims. **Verdict: aligned;
  no drift.**

### VFS / filesystem

- **Walker DAC predicate landed at `vfs/walker.rs:190-218`** —
  `check_descend_perm` (interior X-bit) and `check_open_perm`
  (terminal R/W bits with `CAP_DAC_OVERRIDE` short-circuit). Spec
  `VFS_CHECKS_V2.1.md` covers the witness flow but does not define
  a permission-check shape. **Verdict: code more specific than
  spec; not drift, but spec gap.**

- **`FsOps` trait surface vs spec.** Code `vfs/execution.rs:20-210`
  defines 13 trait methods: `lookup`, `load_inode_meta`,
  `serialize_inode_meta`, `create_inode`, `unlink`, `rename`, `link`,
  `mkdir`, `rmdir`, `symlink`, `readdir`, `destroy_inode`,
  `read_link`, `materialise_rnode`, `step_chmod`, `step_chown`. Spec
  `VFS_CHECKS_V2.1.md` does not name `FsOps` directly — it specifies
  the walker contract and witness flow rather than a driver trait.
  **Verdict: spec gap (Tier-2)** — driver trait shape is in code
  but undocumented; doc owes a "filesystem-instance trait surface"
  section.

- **`FsOps::materialise_rnode` override convention.** Default returns
  `ENOSYS` (`vfs/execution.rs:147-155`). tmpfs override returns
  `RNodeBacking::PageBacked { pc }` for regular files; devfs returns
  `RNodeBacking::StructBacked { Tty }` for char devices. Pattern
  cited in elf-loader Wave 5A. **Verdict: aligned with the
  impl-extending convention; spec owes a description.**

- **Mount registry: `mount::register_mount` + `mount::mount_for`
  + `allocate_mount_id` + `allocate_dev_id`.** Code
  (`tx-subsystems/src/mount.rs:359-444`) ships the registry. Spec
  `MOUNT_v1.md` describes the mount tree but not the registry slot.
  Pre-ELF Phase 6 wired the allocators. **Verdict: spec gap
  (Tier-2).**

### Process / cred / signal / thread_runtime

- **`ProcessPayload` shape: ARCHITECTURAL drift from
  `PROCESS_v1.md`.** Spec `PROCESS_v1.md:218-241` declares:
  ```
  pub struct ProcessPayload {
      pub threads: DllContainer<ThreadIdentity>,
      pub thread_count: AtomicU32,
      pub frame: Frame,                 // ← Frame container with
                                        //   Shared<T> slots
      pub nsproxy: Cap<NsProxy>,
      pub policy: ProcessPolicy,
      pub group_pending: PendingSignalQueue,
      pub group_exit: GroupExit,
      pub leader_exit_status: AtomicOption<ExitStatus>,
      pub identity: Weak<ProcessIdentity>,
  }
  ```
  with `Frame { vm: Shared<AddressSpace>, fd_table: Shared<FdTable>,
  sig_actions: Shared<SigActionTable>, fs_context: Shared<FsContext>,
  cwd: Cap<DEntry>, root: Cap<DEntry>, umask: AtomicU16 }` per
  `PROCESS_v1.md:349-365`. Code `process/structure.rs:534-664`
  ships a flat `ProcessPayload` with no `Frame`, no `Shared<T>`,
  no `policy: ProcessPolicy`, no `group_exit`, no `nsproxy`, no
  `leader_exit_status`. Instead: `aspace: AtomicSlot<Cap<AddressSpace>>`
  (line 547), `threads: SpinMutex<Vec<Cap<ThreadIdentity>>>` (548),
  `sig_actions: SigActionTable` (552, inline not Shared), `cred:
  SpinMutex<Cred>` (560, no Shared), `cwd: SpinMutex<Option<Cap<DEntry>>>`
  (574), `fds: SpinMutex<[Option<Cap<OpenFile>>; 8]>` (589),
  `fd_cloexec: AtomicU32` (606), `brk_base: AtomicU64` /
  `current_brk: AtomicU64` (621/631), `exit_port: Channel` (652),
  `exit_port_carrier_id: u64` (663). **Verdict: Tier-1 drift
  (multi-slice).** Spec needs a v2 amendment that ratifies the flat
  shape (`Shared<T>` + `Frame` + `ProcessPolicy` deferred); or code
  needs a major refactor. The flat shape was the trio's pragmatic
  choice and has carried through 5 slices; ratify, don't refactor.

- **`Cred.suid` + `Cred.sgid` + `effective_caps` /
  `permitted_caps` extension.** Code `cred.rs:143-156` has the
  full saved-set fields. DAC+setuid Wave 1 landed these. Spec
  `cred_service_v_1_draft (2).md` is a draft. **Verdict: Tier-2
  drift (fix doc to match).**

- **`ProcessPayload.fds` fixed 8-slot array.** Code
  `process/structure.rs:589` has `fds: SpinMutex<[Option<Cap<OpenFile>>;
  FD_TABLE_SIZE]>` with `FD_TABLE_SIZE: usize = 8` (line 59).
  Spec implies a real `FdTable` Shared<T>. **Verdict: Tier-1
  drift (fix code in fd-ops slice).** Headlines the fd-ops plan.

- **`ProcessPayload.fd_cloexec: AtomicU32`** — bitmap covers fd
  0..31; tied to the 8-slot fd table. When fd table grows past 32,
  becomes `[AtomicU64; N]` per the field's docstring (line 597).
  **Verdict: Tier-3 (already documented as v1 simplification).**

- **`ProcessPayload.exit_port: Channel` + `exit_port_carrier_id:
  u64`.** Wait carrier on which `wait4` blocks. Spec
  `PROCESS_v1.md:144` mentions an `exit_port` concept; the impl is
  the carrier-with-registry-id pattern from fork/clone/wait4 Wave 1.
  **Verdict: Tier-2 drift (fix doc to match).**

- **`SigActionTable` lives inline on `ProcessPayload`, not in
  `Frame.sig_actions: Shared<SigActionTable>`.** `process/structure.rs:552`.
  No CLONE_SIGHAND sharing yet (clone slice is bare-SIGCHLD).
  **Verdict: Tier-1 drift (architectural, deferred).**

- **`saved_user_context` shape.** Stored on `ThreadPayload`
  (per pre-ELF Phase 2's `prepare_userspace_entry_payload` call
  shape). Spec `THREAD_RUNTIME_v1.md` describes the running-thread
  states; the payload field shape is implementation-defined.
  **Verdict: aligned.**

### VM / page_backed

- **`vm::scripts::build_aspace_from_image::<P>(plan)` and
  `populate_detached_user_range(aspace, vaddr, bytes)` (no `&Guard`).**
  `vm/scripts.rs:203` and `:292`. ELF-loader Wave 1 noted: signature
  deviates from plan because callers cannot hold an outer `Guard`
  while inner helpers acquire fresh guards (EBR rule). Existing
  scripts (`mmap_script`, `fault_script`) follow the same pattern.
  Spec `VM_v1_2.md` describes the script shape but doesn't pin the
  `&Guard` argument. **Verdict: aligned; no drift.**

- **`ImagePlan { entry, stack_top, load_segments, bss_extension }`
  + `LoadSegment { vaddr, memsz, filesz, file_offset, flags,
  backing }`.** `vm/scripts.rs`. Doc `VM_v1_2` doesn't define this
  shape — it lives at the `vm::scripts` module surface that exec
  consumes. **Verdict: spec gap (Tier-2).**

- **`page_backed::targeted_read::read_exact_at(pc, off, out, guard)`.**
  `page_backed/targeted_read.rs:33`. Cross-doc edit B1 from
  ELF-loader plan. Spec `PAGE_BACKED_v1.md` doesn't define this
  helper but the slice's plan amended it. **Verdict: spec gap
  (Tier-2).**

### tx-scripts (exec)

- **`exec_script` Phase 3.5 setuid recompute.** `script.rs:355-380`
  inserts `step_apply_suid_for_exec` between Phase 3 (parse) and
  Phase 4 (build_aspace). Spec EXEC_v1's `7. Phase 2 — Authorization
  and credential plan` (`EXEC_v1.md:425-466`) places setuid handling
  in Phase 2 (before image load). **Verdict: Tier-2 drift (fix doc
  to match — Q2 plan decision)** — Phase 3.5 in code is the cred
  recompute that follows the parse-confirms-the-binary discipline;
  spec's Phase 2 conflates authorization (X-bit auth) with cred
  recompute. The current placement matches Linux semantics.

- **`AuxvFacts` shape vs EXEC_v1 §9.4.** Spec
  `EXEC_v1.md:903-919` lists 18 entries: AT_PHDR, AT_PHENT,
  AT_PHNUM, **AT_ENTRY**, **AT_BASE**, AT_PAGESZ, **AT_HWCAP**,
  **AT_HWCAP2**, **AT_PLATFORM**, AT_RANDOM, **AT_FLAGS**, AT_UID,
  AT_EUID, AT_GID, AT_EGID, AT_SECURE, **AT_EXECFN**, AT_NULL.
  Code `tx-scripts/src/process/exec/stack.rs:74,42-52` ships
  `AUXV_PAIR_COUNT = 11` with: AT_NULL, AT_PHDR, AT_PHENT, AT_PHNUM,
  AT_PAGESZ, AT_UID, AT_EUID, AT_GID, AT_EGID, AT_SECURE, AT_RANDOM.
  **Missing 7 entries: AT_ENTRY, AT_BASE, AT_HWCAP, AT_HWCAP2,
  AT_PLATFORM, AT_FLAGS, AT_EXECFN.** **Verdict: Tier-1 drift.**
  musl tolerates absence of AT_HWCAP/AT_PLATFORM (per ELF-loader
  decision Q#5 deferral). AT_ENTRY missing is more concerning for
  static-PIE; static-EXEC tolerates it.

- **`check_exec_perm` lives in `script.rs`, not `walker.rs`.**
  `tx-scripts/src/process/exec/script.rs:683` (vs. the natural home
  in `vfs/walker.rs`). Pre-Phase-6 X-bit check; placed at the exec
  call site because the walker's `step_open` only validates R/W.
  DAC-setuid Wave 4 landed it. **Verdict: Tier-2 drift (refactor
  candidate).**

- **goblin `elf32` feature dead-but-required.**
  `tx-scripts/Cargo.toml:24` carries `features = ["alloc",
  "endian_fd", "elf64", "elf32"]`. ELF-loader Wave 1 documented
  goblin 0.10.5 has an upstream feature-flag bug: `elf::dynamic::dyn32`
  and `header64::Header::new` reference `crate::elf32`
  unconditionally. **Verdict: Tier-3 (documented; tracked via
  `TODO(elf-loader)`).**

### Reactor / scheduler

- **Per-task AST plumbing absent — `AstBatch::default()`
  shortcut.** `tx-kernel/src/thread_future.rs:349-363` calls
  `userspace_slot().checkpoint_userspace_entry_batch(req,
  AstBatch::default(), |_ast| EnterUserspace)`. The "empty-batch
  variant" comment cites the gap explicitly: "When the per-task
  AST plumbing lands the `AstBatch::default()` here is replaced by
  the drained ASTs". `tx-reactor/src/task.rs:112` also seeds
  `last_ast_batch: AstBatch::default()`. Flagged in pre-ELF
  follow-up #2, fork/clone/wait4 follow-up #2, dac+setuid follow-up
  #1. Spec `REACTOR_v0.md` describes the AST checkpoint shape; code
  doesn't yet implement per-task drain. **Verdict: Tier-1 drift
  (deferred but on the critical path for signal-handler frames).**

- **Reactor task wrapper `PerHartSlotted<F>` shape.** Pre-ELF
  Phase 2's keystone — `tx-kernel/src/thread_future.rs`. Spec
  `REACTOR_v0.md` doesn't define the per-hart slot bracketing
  shape. **Verdict: spec gap (Tier-2).**

- **`reactor_submit::SUBMIT_CHILD_THREAD` function-pointer slot.**
  `tx-subsystems/src/reactor_submit.rs`. Function-pointer-over-
  trait pattern chosen because tx-shims → tx-kernel would be a
  cyclic dep (fork/clone/wait4 Wave 1 rationale). Spec
  `REACTOR_v0.md` doesn't describe a child-thread submission seam.
  **Verdict: spec gap (Tier-2).**

### Syscall surface (tx-shims) — full catalog

`tx-shims/src/linux_syscall/numbers.rs` declares 36 NR constants;
`mod.rs:336-396` dispatches **33 handler arms** (the GETPGRP arm
returns `ENOSYS_VALUE` directly so it counts but is a no-op):

| nr | name | wave |
|---|---|---|
| 25 | NR_FCNTL | elf-loader 2 |
| 48 | NR_FACCESSAT | dac+setuid 4 |
| 53 | NR_FCHMODAT | dac+setuid 4 |
| 54 | NR_FCHOWNAT | dac+setuid 4 |
| 63 | NR_READ | trio |
| 64 | NR_WRITE | trio |
| 81 | NR_GETPGRP | fork/clone/wait4 2 (returns ENOSYS) |
| 93 | NR_EXIT | trio |
| 94 | NR_EXIT_GROUP | trio |
| 96 | NR_SET_TID_ADDRESS | fork/clone/wait4 2 (stub) |
| 99 | NR_SET_ROBUST_LIST | fork/clone/wait4 2 (stub) |
| 134 | NR_RT_SIGACTION | trio |
| 135 | NR_RT_SIGPROCMASK | trio |
| 143 | NR_SETREGID | dac+setuid 2 |
| 144 | NR_SETGID | dac+setuid 2 |
| 145 | NR_SETREUID | dac+setuid 2 |
| 146 | NR_SETUID | dac+setuid 2 |
| 147 | NR_SETRESUID | dac+setuid 2 |
| 148 | NR_GETRESUID | dac+setuid 2 |
| 149 | NR_SETRESGID | dac+setuid 2 |
| 150 | NR_GETRESGID | dac+setuid 2 |
| 154 | NR_SETPGID | fork/clone/wait4 2 |
| 155 | NR_GETPGID | fork/clone/wait4 2 |
| 156 | NR_GETSID | fork/clone/wait4 2 |
| 157 | NR_SETSID | fork/clone/wait4 2 |
| 172 | NR_GETPID | trio |
| 173 | NR_GETPPID | fork/clone/wait4 2 |
| 174 | NR_GETUID | dac+setuid 2 |
| 175 | NR_GETEUID | dac+setuid 2 |
| 176 | NR_GETGID | dac+setuid 2 |
| 177 | NR_GETEGID | dac+setuid 2 |
| 214 | NR_BRK | trio |
| 220 | NR_CLONE | fork/clone/wait4 2 |
| 221 | NR_EXECVE | elf-loader 4 |
| 260 | NR_WAIT4 | fork/clone/wait4 3 |
| 439 | NR_FACCESSAT2 | dac+setuid 4 |

No numbering inconsistencies vs Linux RV64 generic ABI. **Headline
missing arms: NR_OPENAT (56), NR_CLOSE (57), NR_LSEEK (62), NR_DUP
(23), NR_DUP3 (24), NR_PIPE2 (59), NR_GETDENTS64 (61), NR_OPEN
(absent on RV64 generic — only `openat`), NR_PIPE (absent on RV64
generic — only `pipe2`), NR_WAITID (95).** No `sys_open` because
RV64 generic ABI does not include `__NR_open`; the equivalent is
`__NR_openat` with `AT_FDCWD`. **Verdict: aligned numbering;
substantial coverage gap — fd-ops slice closes the file-I/O cluster.**

## Drift severity tiers

**Tier 1 (Architectural — fix soon).** Affect correctness or future
evolution.

1. `RawTrapFrame` / `TrapIf::view` / `TrapIf::view_mut` /
   `return_to_userspace(&RawTrapFrame)` not at trait surface.
   Cross-platform port (LA64) blocked.
2. `enter_userspace_with_context(UserTrapContext)` is the only
   return path — spec needs ratification (Plan B writeback was
   chosen, doc still describes Plan A shape).
3. `EntropyIf` trait absent from spec; landed in code.
4. IRQ explicit registration vs spec's `linkme::distributed_slice`.
5. `AuxvFacts` missing 7 spec-required entries (AT_ENTRY, AT_BASE,
   AT_HWCAP, AT_HWCAP2, AT_PLATFORM, AT_FLAGS, AT_EXECFN).
6. `ProcessPayload` flat shape vs spec's `Frame` + `Shared<T>` +
   `ProcessPolicy` decomposition.
7. `ProcessPayload.fds` fixed 8-slot array (closes in fd-ops
   slice).
8. `SigActionTable` inline on payload, not in `Frame.sig_actions:
   Shared<SigActionTable>`.
9. Per-task AST plumbing absent (`AstBatch::default()` shortcut
   in two sites).

**Tier 2 (Stylistic / spec-gap — batch-fix).** Doc and code do
the same thing differently, or doc owes a section.

1. `AtomicSlot<T>` lives in `tty::structure::identity`; should
   move to `tx_substrate`.
2. `Cred.suid` / `Cred.sgid` / `effective_caps` / `permitted_caps`
   not in cred service draft.
3. `exit_port: Channel` + `exit_port_carrier_id` not in PROCESS_v1.
4. `exec_script` Phase 3.5 setuid placement vs spec's Phase 2
   (need EXEC_v1 amendment).
5. `check_exec_perm` lives in `tx-scripts` rather than walker.
6. `IrqIf::UART_IRQ` const not in HAL spec.
7. `FsOps` driver trait shape undocumented.
8. `vm::scripts::ImagePlan` / `LoadSegment` / `BssTail` shapes
   undocumented.
9. `mount::register_mount` / `mount::mount_for` /
   `allocate_mount_id` / `allocate_dev_id` registry shape
   undocumented.
10. `read_exact_at` helper undocumented in PAGE_BACKED.
11. Reactor `PerHartSlotted<F>` wrapper shape undocumented.
12. `reactor_submit::SUBMIT_CHILD_THREAD` seam undocumented.

**Tier 3 (Documented — accepted as v1 simplifications).**

1. `fd_cloexec: AtomicU32` width (covers fd 31; field docstring
   names the upgrade path to `[AtomicU64; N]`).
2. goblin `elf32` feature carried as dead-but-required (upstream
   bug; tracked).
3. AT_RANDOM weak fallback (now CSPRNG via EntropyIf, but
   default impl is xorshift64; superseded on real boards).
4. Carrier-lifetime cleanup leak shape (TtyIdentity +
   ProcessPayload exit_port; pre-existing leak).
5. Walker symlink chase 40-hop limit (POSIX SYMLOOP_MAX).
6. Layer B end-to-end smokes deferred for setuid + fork/wait4
   (instruction-decoder simulator out of scope).
7. RV64-only platform; LA64 path blocked on Tier-1 #1.
8. Single-hart BSP loop (SMP runtime deferred).

## Verdict

Substantial drift accumulated across 5 slices. Most is **doc-needs-
to-catch-up-to-code** (Tiers 1.1–1.3, 1.6, 2.x): the slice decisions
were sound but the active design docs have not been amended to
ratify them. The genuinely architectural items are **Tier-1 #1
(`RawTrapFrame` portable surface)** which blocks LA64 and
**Tier-1 #6/7/8 (Frame / Shared<T> / fd-table)** which is the
fd-ops slice's headline scope.

The fd-ops slice is **clear** — Tier-1 #7 (fixed 8-slot fd table) is
exactly what fd-ops grows. Tier-1 #6/#8 (Frame + Shared<T>
sig_actions) are deeper refactors that the fd-ops slice can defer:
fd-ops sticks with the flat `ProcessPayload` and grows the table
slot in place. Tier-1 #5 (auxv missing 7 entries) is independent of
fd-ops and should be either folded into a small chore or deferred.

Recommended: ship fd-ops as planned; ship a parallel **drift-cleanup
chore** (~200 LOC) that lands AtomicSlot migration + Tier-2 doc
amendments + AT_ENTRY/AT_BASE auxv fields.
