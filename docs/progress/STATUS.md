# txKernel Status

**Updated:** 2026-05-07

## Current Shape

- 2026-05-07 shell-prompt slice 8 (file-mutation syscalls) on branch
  `feat/file-mutation`. Per
  `docs/progress/plans/2026-05-07-shell-prompt-roadmap.md` Slice 8.
  Wires nine new arms — `NR_MKDIRAT = 34`, `NR_UNLINKAT = 35`,
  `NR_SYMLINKAT = 36`, `NR_LINKAT = 37`, `NR_TRUNCATE = 45`,
  `NR_FTRUNCATE = 46`, `NR_READLINKAT = 78`, `NR_UTIMENSAT = 88`
  (`-ENOSYS` carryover), `NR_RENAMEAT2 = 276`. Each path-relative arm
  walks the parent directory via `step_walk` (synchronous through
  `poll_walker_synchronously` for Send-future discipline) and
  dispatches through `FsOps::{mkdir,unlink,rmdir,symlink,link,rename,
  read_link}` plus `page_backed::step_truncate` for the truncate
  pair. `unlinkat` decodes `AT_REMOVEDIR` to choose `unlink` vs
  `rmdir`; `renameat2` honours `RENAME_NOREPLACE` via a pre-walk
  existence check; `RENAME_EXCHANGE` returns `-ENOSYS` and
  `RENAME_WHITEOUT` returns `-EINVAL`. `readlinkat` walks the
  parent dir and calls `FsOps::lookup` + `read_link` directly so the
  symlink itself (not its resolved target) is what gets read — the
  in-tree walker follows symlinks unconditionally so a standard
  `step_walk` to the link path would resolve through the link.
  Verification: `cargo build --workspace --lib --tests` clean (no
  warnings), `cargo test --workspace --lib --tests --
  --test-threads=1` 1111/1111 passed (1086 baseline + 25 new
  `file_mutation::*` dispatch tests covering each arm's success and
  canonical-error shapes). Carryovers: `utimensat` deferred under
  `TODO(phase-vfs-utimens)` (no `FsOps::set_times` hook); `linkat`
  surfaces tmpfs's existing `-ENOSYS` for `link` (Phase 3b
  carryover, hard-links not yet supported); `renameat2`
  cross-directory rename surfaces tmpfs's same-dir-only `-ENOSYS`
  (Phase 3b carryover). Next step: Slice 9 (user-VA migration) or
  shell-bringup integration smoke.
- 2026-05-07 fd-ops slice (Waves 1–4) + drift cleanup chore + CSPRNG
  prerequisite chore on branch `feat/fd-ops`. Per
  `docs/progress/plans/2026-05-07-fd-ops-and-drift-cleanup.md` +
  decision `docs/progress/decisions/2026-05-07-fd-ops-and-drift-cleanup.md`.
  Closes the biggest day-1 blocker before booting a real shell. LTP
  unlock estimate ~30–50 tests across `open*` / `close*` / `dup*` /
  `pipe*` / `lseek*` plus shell-style fd-redirect tests scattered
  across `fs/` and `pty/`. 7 commits on top of dac-and-setuid:
  `f2d8a67` (CSPRNG via HAL `EntropyIf` trait + per-exec `AT_RANDOM`
  fill — audit Tier-1 #3), `b7a15fb` (interface drift audit + slice
  plan; Q1/Q2/Q3 defaults accepted), `0516911` (drift cleanup batch
  — `AtomicSlot` move to `tx_substrate::slot`, `AT_ENTRY`/`AT_BASE`
  added to `AuxvFacts`, 3 doc amendments closing audit Tier-1
  #3/#4/#5/#6 + Tier-2 #1/#6), `203e0fe` (Wave 1: fd-table
  `BTreeMap<u32, Cap<OpenFile>>` migration + sparse `BTreeSet<u32>`
  cloexec replacing the fd-31-ceiling `AtomicU32` bitmap; new
  `allocate_fd` / `install_fd` accessors), `302bab9` (Wave 2:
  `NR_OPENAT = 56` + `NR_CLOSE = 57` + `NR_DUP = 23` + `NR_DUP3 = 24`
  — bundled because they share helpers; `O_CREAT + O_EXCL` via
  syscall-arm `create_then_walk` helper since `step_open` is
  resolve-only; `dup3` same-fd `-EINVAL`; `NR_DUP2` absent on RV64
  generic — musl emits `dup3(_, _, 0)`), `28b21f2` (Wave 3:
  `NR_PIPE2 = 59` + new `tx_subsystems::pipe` module — 4 KiB ring
  with reader-side and writer-side wait carriers; matches Linux
  blocking semantics exactly per Q2; `Errno::EAGAIN`/`EBADF`/`EPIPE`
  added; `OpenFileFlags.nonblocking` field; SIGPIPE-on-EPIPE
  delivered from `sys_write` arm), `bd0e9ea` (Wave 4: `NR_LSEEK = 62`
  + per-fd `OpenFile.offset: AtomicU64` — replaces `u64` so `step_*`
  can run against `&Cap<OpenFile>` without `&mut`; `Errno::ESPIPE`;
  TTY/CharDevice/Pipe → ESPIPE; PageBacked uses
  `PageContainer::size_bytes()` for SEEK_END; sibling
  `init_lseek_fixture.rs` for `openat → write → lseek → read →
  close → exit_group` Layer A byte-pin smoke). Plan Part 8
  deviation: sibling fixture (matches DAC slice precedent) instead
  of extending `init_fixture.rs` — preserves the existing
  fork+wait+exit byte pins. **Q1 DECIDED 2026-05-07:** fd-table is
  BTreeMap (sparse-fd case is real). **Q2 DECIDED 2026-05-07:**
  pipe blocking matches Linux exactly (writer-side carrier).
  **Q3 DECIDED 2026-05-07:** NR_GETDENTS64 deferred to a sibling
  directory-ops mini-slice. Verification: `cargo build --workspace
  --lib --tests` clean (no warnings); `cargo test --workspace --lib
  --tests -- --test-threads=1` 984/984 passed; per-crate deltas:
  tx-subsystems 405 → 420, tx-shims 78 → 109, tx-kernel 37 → 43,
  tx-scripts 39 → 42; tx-substrate sync + integration preserved;
  tx-fs unchanged. Pre-existing conditions (verified Wave 2 vs
  baseline before any Wave 3 change): cross-compiled board
  binaries (`tx-kernel-*-qemu-virt`) fail to link on host without
  cross-toolchains; tx-subsystems lib tests need
  `--test-threads=1` for green. Carryovers: pipe lifecycle Drop
  hook (`Cap<OpenFile>` Drop → `decr_reader` / `decr_writer` so
  `close(reader_fd)` flips reader_count); reactor-driven Layer B
  end-to-end execution of `init_lseek_fixture` (deferred per slice
  norm). Next step: directory-ops mini-slice (NR_GETDENTS64) or
  pipe lifecycle hook — both are small isolated follow-ups.
- 2026-05-07 drift cleanup chore (5 items, ~200 LOC) on branch
  `chore/drift-cleanup`. Per
  `docs/progress/plans/2026-05-07-fd-ops-and-drift-cleanup.md`
  §"Drift cleanup batch" + audit
  `docs/progress/research/2026-05-07-interface-drift-audit.md`. Lands
  before fd-ops Wave 1 because the `AtomicSlot` move makes the
  upcoming fd-table BTreeMap migration cleaner, and `AT_ENTRY` /
  `AT_BASE` should be added once not again. Item-by-item:
  (1) `AtomicSlot<T>` moved from
  `crates/tx-subsystems/src/tty/structure/identity.rs:72-115` to
  `crates/tx-substrate/src/slot.rs` and re-exported at
  `tx_substrate::AtomicSlot`. Audit Tier-2 #1 closed. Touched 5
  call sites (process/structure.rs, process/execution.rs (test
  builder), tty/structure/mod.rs, tty/structure/payload.rs,
  identity.rs). (2) `AT_ENTRY = 9` and `AT_BASE = 7` added to
  `AuxvFacts` and the stack builder; `AUXV_PAIR_COUNT` 11 → 13;
  total auxv contribution 176 → 208 bytes. New tests pin
  `AT_ENTRY` carries `image_plan.entry`, `AT_BASE = 0` for
  static-EXEC; existing eleven-entries test renamed to
  thirteen-entries; AT_UID/AT_SECURE/AT_RANDOM index pins shifted.
  Audit Tier-1 #5 (partial) closed. (3) `HAL_v1.md` §13 amended:
  added `IrqIf::UART_IRQ` to the trait surface, replaced the
  linkme-distributed-slice example with the explicit
  `register_irq_handler` shape, added §13.2.1 documenting the
  seven-point case against linkme, removed `IRQ_HANDLERS` from
  §21.2 approved slices. Audit Tier-2 #6 + Tier-1 #4 closed.
  (4) `HAL_v1.md` §13A added: new section documenting `EntropyIf`
  trait surface, default xorshift impl, RV64 rdtime impl, trust
  model, and upgrade path. Audit Tier-1 #3 closed. (5)
  `PROCESS_v1.md` §2.2.1 v2 amendment added: ratifies the flat
  `ProcessPayload` shape that 7 commits built (trio Phase 2a/b,
  pre-ELF Wave 3, ELF loader Wave 1, fork/clone/wait4 Wave 1, DAC
  Wave 2, DAC Wave 4); `Frame { Shared<T> }` / `ProcessPolicy` /
  `nsproxy` / `group_exit` / `leader_exit_status` deferred to v3
  with rationale. Audit Tier-1 #6 closed. Verification:
  `cargo check --workspace` clean; `cargo check --workspace --tests`
  clean; `cargo check -p tx-kernel-riscv64-qemu-virt --target
  riscv64gc-unknown-none-elf` clean; tx-substrate sync 2/2 +
  integration suites preserved; tx-subsystems 405/405 serial;
  tx-scripts 42/42 (40 baseline + 2 new auxv tests); tx-kernel
  37/37; tx-fs 24/24 serial; tx-shims 78/78; `cargo fmt --check`
  clean; `cargo xtask progress validate` ok. Next step: ship as
  separate PR; fd-ops Wave 1 follows with the cleaner seam.
- 2026-05-07 DAC + setuid Wave 5 (Part 8 end-to-end smoke + sibling
  setuid fixture) on branch `feat/dac-and-setuid`. Per
  `docs/progress/plans/2026-05-06-dac-and-setuid.md` Part 8.
  Climax wave — proves the DAC + setuid pipeline end-to-end.
  Deliverables: (1) `crates/tx-kernel/src/init/init_setuid_fixture.rs`
  — 204-byte hand-encoded RV64 ET_EXEC ELF fixture; 7-instruction
  body (`li a7, 174` (NR_GETUID) → ecall → `li a7, 175` (NR_GETEUID)
  → ecall → `li a7, 94` (NR_EXIT_GROUP) → `li a0, 0` → ecall);
  same `LOAD_VADDR = 0x10000` as the fork+wait fixture (the two
  fixtures are not co-resident in any single AddressSpace);
  entry-vaddr `0x100B0`. **Plan Q4 deviation:** Q4 was authored
  before the fork/clone/wait4 slice rewrote `init_fixture.rs` into
  a 317-byte fork+wait+exit binary with ~7 pinned byte tests;
  extending it again into a third behaviour would invalidate the
  existing pin tests. Sibling fixture matches the plan's Part 8
  section heading and keeps both smokes independently pinned.
  (2) 5 pin tests in `init_setuid_fixture/tests`:
  size-matches-constant (204 bytes), elf-magic, e_machine=EM_RISCV,
  e_entry-matches-constant, first-instruction-is-li-a7-174.
  (3) End-to-end Layer A smoke
  `boot_smoke_setuid_exec_seeds_post_setuid_euid_and_at_secure`
  in `crates/tx-kernel/src/init/tests.rs`. Drives boot wiring,
  registers `/setuid-target` with mode `S_ISUID | 0o755` owned
  by uid=1000/gid=1000 (via the new `register_setuid_fixture_into_tmpfs`
  helper that uses production `step_chown` + `step_chmod` under
  CAP_FOWNER root cred to avoid the silent-clear-S_ISUID rule),
  drops init's cred to uid=euid=suid=1001 + clears caps via
  `cross_crate_test_support::clear_caps_for_test` +
  `set_cred_ids_for_test`, then `block_on(exec_script::<TestPlatform>)`.
  Post-exec assertions: `init.cred().uid == 1001` (real uid
  preserved), `init.cred().euid == 1000` (S_ISUID recompute set
  effective uid to file owner), `init.cred().suid == 1000`
  (saved-set tracks new euid), `init.cred().gid/egid/sgid == 1001`
  (no S_ISGID on fixture so gid family unchanged),
  `saved_user_context.pc == INIT_SETUID_FIXTURE_ENTRY_VADDR`
  (Phase 6 still seeded entry-point with new cred), AddressSpace
  Cap key changed (PoNR boundary crossed). (4) Same
  `register_setuid_fixture_into_tmpfs` helper added inline to
  the test module — mirrors `register_init_fixture_into_tmpfs`'s
  shape (create_inode → materialise_rnode → page-by-page memcpy
  → truncate) plus post-creation `step_chown` + `step_chmod`
  under root cred. **Layer A choice:** matches the fork/clone/wait4
  Wave 4 smoke's choice (production-paths-up-to-divergence;
  reactor-driven instruction-level execution deferred to a
  future integration smoke). Verification: tx-kernel 37/37
  (31 baseline + 6 new); tx-substrate sync 2/2 + integration 2/2;
  tx-fs 24/24 serial; tx-shims 78/78; tx-scripts 39/39;
  tx-subsystems 405/405 serial; `cargo check --workspace` clean;
  `cargo check -p tx-kernel-riscv64-qemu-virt --target
  riscv64gc-unknown-none-elf` clean; `cargo fmt --check` clean;
  `cargo xtask progress validate` ok (24 file(s)). Closes the
  DAC + setuid slice (Waves 1–5 all landed). Next step: progress
  catch-up + decision note for the slice.
- 2026-05-06 fork/clone/wait4 Wave 3 (NR_WAIT4 syscall arm with
  blocking-wait) on branch `feat/fork-clone-wait4`. Per
  `docs/progress/plans/2026-05-06-fork-clone-wait4.md` Part 3.
  Adds the Linux RV64 `wait4(2)` syscall arm against the
  `step_waitpid_nohang` walker (synchronous, no guard parameter —
  takes a fresh internal snapshot of `parent.children`) plus the
  blocking variant via `wait_carrier::wait_on_token` over the
  parent's per-process `exit_port` carrier (registered at
  payload-sign time per Wave 1; fired from
  `post_sigchld_to_parent` when any child zombifies).
  Deliverables: (1) `crates/tx-shims/src/linux_syscall/numbers.rs`
  adds `NR_WAIT4 = 260` and `WNOHANG = 0x1` constants. (2)
  `crates/tx-shims/src/linux_syscall/mod.rs` adds `ECHILD_VALUE = 10`
  errno + `sys_wait4` async function — full POSIX `pid` selector
  coverage (`pid > 0` → `Pid`, `pid == 0` → `CallerPgrp`,
  `pid == -1` → `Any`, `pid < -1` → `Pgrp(Pgid(-pid))`,
  `pid == i32::MIN` → `-EINVAL` per LTP `wait403`); WNOHANG-only
  options bit acted on (WUNTRACED/WCONTINUED accepted but ignored
  per Linux's silent-unknown-bits behaviour); non-NULL `rusage`
  rejected with `-EINVAL` (`TODO(phase-rusage)` — txKernel
  doesn't track rusage today). On `Done(child_pid, status)` the
  arm encodes the wait-status word via the existing POSIX
  `ExitStatus::wait_status_word` (Wave 1's migration from the
  shell `128+sig` shape) and writes a 4-byte little-endian `i32`
  to `wstatus_uaddr` if non-zero, mirroring the `sys_write`
  bootstrap-buffer exemption (`core::ptr::write_volatile` with
  `TODO(phase-userva)`). On `Err(NoneReady)` without WNOHANG,
  the arm builds a `WaitToken` from
  `ctx.process.exit_port_wait_token()` (returns `None` for
  zombies, surfaced as `-ECHILD`) and awaits
  `wait_carrier::wait_on_token`, looping post-wake (standard
  double-check pattern: a third party may have reaped first).
  Dispatch arm wires under the existing `nr if nr == NR_*`
  guard pattern alongside `NR_CLONE`, `NR_EXECVE`. (3) 10 new
  tests in `crates/tx-shims/src/linux_syscall/tests.rs`'s
  `fork_clone_wait4_wave3` mod: ECHILD on no-children,
  WNOHANG-no-zombies returns 0 (child preserved alive),
  WNOHANG-zombie reaps and returns pid, WNOHANG writes wstatus
  word (Exited(42) → 0x2a00), specific-pid skips other-pgrp
  zombies, blocking-wait load-bearing test (manually polls the
  future to Pending, calls `step_exit_group` on the child to
  fire the parent's exit_port via `post_sigchld_to_parent`,
  re-polls to Ready), rusage-non-NULL → -EINVAL,
  WUNTRACED/WCONTINUED bits silently ignored, i32::MIN pid →
  -EINVAL, pgid selector picks grouped zombie. Verification:
  tx-shims 48/48 (38 baseline + 10 new); tx-substrate sync
  2/2 + integration 2/2 ; tx-fs 16/16; tx-scripts 29/29;
  tx-subsystems 377/377; tx-kernel 29/29; `cargo check
  --workspace` clean; `cargo check -p
  tx-kernel-riscv64-qemu-virt --target
  riscv64gc-unknown-none-elf` clean; `cargo fmt --check`
  clean; `cargo xtask progress validate` ok (24 file(s)).
  Wave 4 (RV64 fixture v2 + end-to-end smoke) is the next
  step.
- 2026-05-06 ELF loader Phase 6 (NR_EXECVE syscall arm) on branch
  `feat/elf-loader-and-execve`. Per
  `docs/progress/plans/2026-05-06-elf-loader-and-execve.md` Part 6.
  Wires the userspace-visible entry point to Phase 5's `exec_script`.
  Deliverables: (1) `tx-shims/Cargo.toml` adds `tx-scripts` + `tx-hal`
  as runtime deps (one-directional — tx-scripts does NOT pull
  tx-shims, no cycle). (2) `SyscallResult::ExecCommitted` enum
  variant on `tx_shims::linux_syscall::SyscallResult` — the
  thread future treats this as "do NOT drain
  `pending_syscall_return` for this iteration" (cite
  `txdoc:EXEC-12-1-INSTALL-USER-TRAP-CONTEXT`). (3) `NR_EXECVE = 221`
  in `numbers.rs` (Linux RV64 generic ABI). (4) `dispatch::<P: PmapIf>`
  signature change — generic over the platform's `PmapIf` so
  `sys_execve::<P>` can call `exec_script::<P>`. All existing
  callers updated (3 in tx-shims tests, 2 in tx-kernel
  thread_future + tests). (5) `sys_execve` arm: bounded
  user-buffer copies via `read_user_cstr` / `read_user_cstr_vec`
  helpers (kernel-side `read_volatile` per the Phase 2a bootstrap
  exemption); caps `EXECVE_PATH_MAX = 4096` (NUL-terminator-or-
  ENAMETOOLONG), `EXECVE_ARG_MAX_INLINE = 8192` (shared argv +
  envp byte budget — overflow → E2BIG), `EXECVE_VEC_MAX = 256`
  pointer slots. (6) `ExecError::to_errno_i32` impl on tx-scripts'
  `ExecError`: returns negative magnitudes (`PathNotFound = -2`,
  `NotExecutable = -8`, `PathTooLong = -36`, `OutOfMemory = -12`,
  ...) consistent with `tx-shims::linux_syscall::errno_to_i32`.
  Helper `execve_errno_magnitude` flips sign so
  `SyscallResult::Error(positive)` is preserved. (7) Thread future
  match-arm refactor in `crates/tx-kernel/src/thread_future.rs`:
  the `UserspaceTrapInfo::Syscall` arm now matches all four
  `SyscallResult` variants explicitly; on `ExecCommitted` it
  falls through (no early return, no pending-return write) so the
  AST drain + `prepare_userspace_entry_payload` +
  `enter_userspace_with_context` tail re-uses the
  freshly-seeded `saved_user_context` from the Phase-6 swap. (8)
  Send-fix in `tx-scripts::process::exec::script::exec_script` —
  the walker `step_open(...).await` was capturing `&Guard` across
  the suspension point, making the resulting future `!Send` (Guard
  is deliberately `!Send + !Sync`). Replaced with a synchronous
  poll via a noop-waker helper `poll_walker_synchronously`; the
  in-tree walker backends never `.await` today (per
  `vfs::walker` module docs), so a single `poll` returns Ready
  every time. When real-await backends land, the `Pending` arm's
  panic message points to the canonical fresh-guard-inside-await_*
  shape. Tests: 5 new in `tx-shims` (path-not-found-returns-
  neg-enoent; invalid-elf-returns-neg-enoexec; too-long-path-
  returns-neg-enametoolong; argv-overflow-returns-neg-e2big;
  success-returns-exec-committed) + 1 new in `tx-kernel`
  (execve-continues-loop-without-writing-pending-return — scripts
  the dispatcher's outcome with `ExecCommitted` and asserts the
  match-arm semantics directly, mirroring the existing
  PageFault-Ok test pattern). The new `execve` tests reuse the
  tx-scripts `ExecTestFs` shape inline (FsOps + FsPageBacking
  fixture with `materialise_rnode` over a hand-crafted RV64 ET_EXEC
  binary). Verification: `cargo check --workspace` clean; `cargo
  check -p tx-kernel-riscv64-qemu-virt --target
  riscv64gc-unknown-none-elf` clean; `cargo test -p tx-shims --lib`
  17 → 22; `cargo test -p tx-kernel --lib` 19 → 20; `cargo test
  -p tx-scripts --lib` 29/29 unchanged; `cargo test -p
  tx-subsystems --lib -- --test-threads=1` 364/364 unchanged;
  `cargo test -p tx-fs --lib -- --test-threads=1` 13/13 unchanged;
  `cargo fmt --check` + `cargo xtask progress validate` clean.
  Next: Phase 7 (bootstrap exec of `/init` from `init.rs`). Note
  for Phase 7: `sys_execve` reads `Credential::default()` (uid=0,
  gid=0) — once `SyscallCtx` grows a `cred` field plumbed from
  the per-thread payload, switch the arm to `ctx.cred()`. Also,
  `dispatch` is now `dispatch::<P: PmapIf>` so any new caller
  must thread the platform type. Blocker: none.

- 2026-05-06 ELF loader Phase 5 (`exec_script` orchestration) on branch
  `feat/elf-loader-and-execve`. Per
  `docs/progress/plans/2026-05-06-elf-loader-and-execve.md` Part 5.
  Realises the EXEC_v1 eight-phase protocol against the seams shipped
  by Wave 1 (build_aspace_from_image / populate_detached_user_range /
  read_exact_at) and Wave 2 (step_close_cloexec_fds /
  step_reset_signal_dispositions_for_exec / step_install_brk_for_exec).
  Deliverables: (1) `tx-scripts::process::exec::script::exec_script::<P>(
  process, thread, path, argv, envp, cred)` returning
  `Result<(), ExecError>` (note: shape differs from the plan's
  `Result<Infallible, ExecError>` sketch — the syscall-arm "do not
  write a return value" decision is structural, made by Phase 6 of
  the loader plan rather than encoded in the type). Eight-phase body:
  walker `step_open` → snapshot `Cap<PageContainer>` from
  `RNodeBacking::PageBacked` → 4 KiB `read_exact_at` → goblin parse →
  bridge to `vm::scripts::ImagePlan` (every LOAD shares the file's
  Cap<PC>) → V1 `build_aspace_from_image::<P>` → V2
  `populate_detached_user_range` with stack image from
  `build_initial_user_stack` → Phase-6 atomic
  `replace_aspace` + `store_saved_user_context(UserTrapContext{ pc:
  e_entry, regs[2]: initial_sp, .. })` → Phase-7 infallible commits
  (CLOEXEC sweep, sig disposition reset, brk install at
  `page_round_up(highest_load.vaddr + memsz)`). PoNR enforced
  structurally: phases 1-5 use `?` and `.await` freely; phases 6-7 are
  a straight-line synchronous block of atomic stores + Wave 2
  helpers. (2) Cross-doc edit P-SIG-RESET: new
  `tx_subsystems::process::execution::step_reset_signal_dispositions_for_exec`
  thin wrapper around Wave 2's `SigActionTable::step_reset_for_exec`
  so `tx-scripts` doesn't need to reach into the `pub(crate)` payload
  field. (3) `tx-scripts/Cargo.toml` gains `tx-hal`, `tx-substrate`,
  `tx-subsystems` deps + dev-dep on `tx-subsystems` with `test-support`.
  Tests: 6 new (loads-minimal-elf-seeds-saved-user-context;
  resets-brk-base-from-image-plan; invalid-elf-returns-not-executable;
  path-not-found-returns-path-not-found;
  resets-signal-dispositions-to-sig-dfl; closes-cloexec-fds-keeps-others)
  driven via host block_on against an in-test `ExecTestFs` that
  overrides `materialise_rnode` to produce
  `RNodeBacking::PageBacked { pc }` over a kernel-built PageContainer
  pre-populated with hand-crafted RV64 ET_EXEC fixture bytes via
  `materialize_anon` + direct map. Verification: `cargo check
  --workspace` clean; `cargo check -p tx-kernel-riscv64-qemu-virt
  --target riscv64gc-unknown-none-elf` clean; `cargo test -p tx-scripts
  --lib` 23 → 29; `cargo test -p tx-subsystems --lib --
  --test-threads=1` 364/364 preserved; `cargo test -p tx-shims --lib`
  17/17; `cargo test -p tx-fs --lib -- --test-threads=1` 13/13;
  `cargo test -p tx-kernel --lib` 19/19; `cargo fmt --check` +
  `cargo xtask progress validate` clean. Next: Phase 6 (NR_EXECVE
  syscall arm in `tx-shims::linux_syscall`) which decodes user
  argv/envp pointers, calls `exec_script::<P>`, and emits a new
  `SyscallResult::ExecCommitted` shape so the thread future skips
  the syscall-return writeback. Note for Phase 6: tmpfs's production
  surface today does NOT override `materialise_rnode`, so a real
  `step_open` against a regular file in tmpfs returns ENOSYS — the
  test fixture works around it with an in-test FsOps override; tmpfs
  needs a small `materialise_rnode` impl (cited
  `bringup_fs_specs_v_1` §"tmpfs `Regular` →
  `RNodeBacking::PageBacked { pc }` over the inode's
  `Cap<PageContainer>`") before Phase 7's bootstrap exec path is
  end-to-end-runnable from real init. Blocker: none.

- 2026-05-06 ELF loader Wave 2 (Phase 2 CLOEXEC plumbing + Phase 1B
  P1/P2/P3 process-side helpers) on branch `feat/elf-loader-and-execve`.
  Per `docs/progress/plans/2026-05-06-elf-loader-and-execve.md` Part 2
  + Part 1 sub-items P1/P2/P3. Open Q #4 DECIDED 2026-05-06: per-fd
  CLOEXEC bitmap stored as `AtomicU32` on `ProcessPayload` (covers fds
  0..31). Deliverables: (1) `ProcessPayload.fd_cloexec: AtomicU32`
  next to existing `fds`; `step_fork` clones the parent's word; init
  defaults to `0` per Linux convention (stdio NOT close-on-exec).
  Public accessors `ProcessIdentity::fd_cloexec(fd)` /
  `set_fd_cloexec(fd, value)` + crate-internal `fd_cloexec_word`.
  (2) `OpenFileFlags.cloexec: bool` flag added (sibling to `read` /
  `write` / `append`); all in-tree call sites updated for source-compat
  (`tx-fs::devfs`, `tx-subsystems::tty::project`, vfs/page_backed
  tests). (3) `NR_FCNTL = 25` arm with `F_GETFD` / `F_SETFD` /
  `FD_CLOEXEC = 1` + `O_CLOEXEC = 0o2000000` in
  `tx-shims::linux_syscall::numbers`; `sys_fcntl` validates
  `fd < FD_TABLE_SIZE` else `-EBADF`, returns `-ENOSYS` for unknown
  cmd (`TODO(phase-fcntl-extension)`). (4) Exec phase-7 helpers in
  `tx-subsystems::process::execution` (per
  `txdoc:EXEC-12-2-RESET-FDS-WITH-CLOEXEC` /
  `txdoc:EXEC-12-4-INSTALL-BRK`): `step_close_cloexec_fds(process)`
  closes every marked fd then clears the bitmap; both NOT async — the
  drop runs through EBR-deferred `Cap<OpenFile>::Drop`, no flush
  await. `step_install_brk_for_exec(process, new_brk_base)` overwrites
  both `brk_base` and `current_brk` atomically. (5)
  `SigActionTable::step_reset_for_exec(&self)` on the existing
  per-payload table (per `txdoc:EXEC-12-3-RESET-SIGNAL-DISPOSITIONS` +
  `SIGNAL_v1` §15.2): walks 1..=64, replaces every `Handler(_)` slot
  with `Default`, preserves `Default` and `Ignore`; pending signals
  NOT cleared (POSIX). Scope reduction: `sys_open` is not in the
  trio's syscall surface, so Wave 2's `O_CLOEXEC` plumbing is the
  bitmap + fcntl arm only — when `sys_open` lands (post-ELF-loader
  slice), threading `O_CLOEXEC` through it is mechanical (decode
  `args[1] & 0o2000000`, set the matching bit on
  `ProcessPayload.fd_cloexec` after `set_fd`). Verification:
  `cargo check --workspace` + `cargo check -p
  tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf`
  clean; `cargo test -p tx-subsystems --lib -- --test-threads=1`
  355 → 364 (+6 process tests covering default/round-trip/fork-clone
  + close-cloexec marked-only / bitmap-clear / install-brk; +3 signal
  tests covering reset to `SIG_DFL` / preserves `SIG_IGN` / preserves
  pending); `cargo test -p tx-shims --lib` 12 → 17 (+5 fcntl tests:
  getfd-zero / setfd-then-getfd / setfd-no-spillover / unknown-cmd
  ENOSYS / invalid-fd EBADF); `tx-scripts` 23/23, `tx-kernel` 19/19,
  `tx-fs` 13/13 unchanged; `cargo fmt --check` + `cargo xtask
  progress validate` clean. Next: Phase 5 (the exec script itself in
  `tx-scripts/src/process/exec/`) which composes the V1 (build aspace),
  V2 (populate stack), Phase-3 stack builder, and these Phase-7
  helpers into the eight-phase script per
  `txdoc:EXEC-4-THE-EIGHT-PHASES`. Note for Phase 5: the three
  phase-7 helpers (`step_close_cloexec_fds`,
  `step_install_brk_for_exec`, `SigActionTable::step_reset_for_exec`)
  are NOT async — phase 7 is a synchronous block per EXEC-PONR;
  earlier exec phases that touch I/O (open binary; populate stack)
  are async. Blocker: none.

- 2026-05-06 Pre-ELF Phase 7 (end-to-end production smoke +
  `kernel_main` reactor-loop wiring) on branch `feat/pre-elf-runtime`.
  Per `docs/progress/plans/2026-05-06-pre-elf-runtime-completion.md`
  §"Phasing" item 7 ("End-to-end host smoke. A single tx-kernel test
  that exercises trap shell → reactor task wrapper → thread future →
  linux_syscall::dispatch → tty + devfs through real walker → real RX
  path, with no synthesised driver."). Three production-code
  deliverables landed: (1) `CoreInit::boot` now calls a new
  `run_userspace_reactor_loop` after `boot_sentinel` — fetches init's
  leader thread + payload, builds
  `PerHartSlotted<P, run_thread::<P>(thread, payload)>`, submits via
  `BOOT_REACTOR.with(|r| r.submit_task(...))`, then drives the BSP
  hart loop using the same `step_hart_loop_at` shape the secondary
  CPUs already use; loop exits on `init.is_zombie()` and emits
  `:userspace:exited:N` (where N = `ExitStatus::wait_status_word()`)
  before `system_off`. (2) `ThreadIdentity::payload_cap_for_test` was
  promoted to a production `payload_cap()` accessor (kept as alias for
  test-support) so `kernel_main` can reach the leader's payload
  without the `pub(crate)` field. (3) `run_thread`'s loop body
  restructured: AST checkpoint runs on a *fresh* `start_request`
  (entry_token) instead of the just-resolved one
  (`req_token`), fixing the `NoActiveRequest` failure surfaced by the
  end-to-end drive. The trio's Phase 6 fake-driver smoke
  (`boot_smoke_userspace_round_trip_writes_console_then_exits`) is
  deleted; replaced by `boot_smoke_production_userspace_loop_writes_
  console_then_exits` in `crates/tx-kernel/src/init/tests.rs`. The new
  smoke uses Option C (pragmatic limit-to-divergence per the Phase 7
  brief): a `TestPlatform`'s `TrapIf::enter_userspace_with_context`
  override captures the merged `UserTrapContext` into a static and
  panics with `SMOKE_YIELD_PANIC`; the smoke wraps each `Future::poll`
  in `std::panic::catch_unwind` and runs a fresh `run_thread` per
  scripted syscall. Two iterations: (i) `write(1, "hi\n", 3)` drives
  the production walker → tty → ConsoleIf::write_bytes path, asserts
  `b"hi\r\n"` post-OPOST capture and `regs[10] == 3` (Plan B writeback
  discipline); (ii) `exit_group(0)` returns `SyscallResult::NoReturn`,
  the future resolves Ready cleanly, init zombifies with
  `ExitStatus::Exited(0)`. tx-kernel 19 → 19 (one deleted, one new;
  test_count unchanged); tx-subsystems 344/344 serial, tx-shims 12/12,
  tx-fs 13/13 serial, tx-substrate sync 2/2. `cargo check --workspace`,
  `cargo check -p tx-kernel-riscv64-qemu-virt --target
  riscv64gc-unknown-none-elf`, `cargo fmt --check`,
  `cargo xtask progress validate` all clean. Next: ELF loader (out of
  scope for pre-ELF wave); the production reactor loop is wired and
  ready to drive a real userspace binary as soon as one can be
  loaded. Blocker: none; the smoke's "limit-to-divergence" choice
  matches the brief's recommendation, and the
  `enter_userspace_with_context` divergent path is exercised
  end-to-end on the RV64 board target (verified by `cargo check`).

- 2026-05-06 Pre-ELF Phase 2 (reactor task wrapper + userspace-entry
  shim) on branch `feat/pre-elf-runtime`. Per
  `docs/progress/plans/2026-05-06-pre-elf-runtime-completion.md` Part 1.
  HAL grows a default-panic `TrapIf::enter_userspace_with_context(ctx:
  UserTrapContext) -> !` (`crates/tx-hal/src/trap.rs`); RV64 board
  override builds an `Rv64TrapFrame`, calls the existing
  `restore_user_context`, then `return_to_userspace`
  (`boards/tx-hal-riscv64-qemu-virt/src/trap.rs`). New userspace-entry
  shim `pub fn prepare_userspace_entry_payload(payload:
  &PayloadCap<ThreadPayload>) -> UserTrapContext` in
  `crates/tx-subsystems/src/thread_runtime/execution.rs`: snapshots
  `saved_user_context`, drains `pending_syscall_return` (Ok(v) → a0 =
  v as u64; Err(errno) → a0 = -errno as i64 as u64) into
  `regs[10]`, clears `active_userspace_request`. Plan B writeback
  discipline pinned by `txdoc:THREAD-5-4-THE-TWO-SITE-DISCIPLINE` —
  this is the only site that drains pending_syscall_return. New
  `crates/tx-kernel/src/thread_future.rs` ships `PerHartSlotted<P, F>`
  (sets/clears the per-hart slot around each `Future::poll`,
  unconditional clear on `Pending` exit too) and the production
  `pub async fn run_thread<P>(thread, payload)` driver. Sequencing:
  `start_request` → `wait.await` (yields Pending) →
  `linux_syscall::dispatch` for Syscall arms /
  `step_exit_group_with_signal(SIGSEGV)` for PageFault (Phase 3
  placeholder) → `checkpoint_userspace_entry_batch(req,
  AstBatch::default(), |_| EnterUserspace)` (AST drain ordering before
  prepare, per Cross-cutting risk #3) →
  `prepare_userspace_entry_payload` →
  `<P as TrapIf>::enter_userspace_with_context` (divergent). Init
  wiring deferred to Phase 7 per the brief — `run_thread` is exported
  but `kernel_main` still uses the trio's shutdown path; an end-to-end
  smoke that demonstrates the reactor loop replaces the Phase 6 fake
  driver in Phase 7. Tx-kernel grew a shared
  `crate::test_serialise::KERNEL_TEST_LOCK` so the new thread_future
  tests serialise against the existing init tests (both bootstrap
  INIT_PROCESS). tx-subsystems 341 → 344 (3 new shim tests:
  `prepare_userspace_entry_payload_drains_pending_return_into_a0`,
  `prepare_userspace_entry_payload_negative_errno_encodes_as_minus_errno`,
  `prepare_userspace_entry_payload_no_pending_preserves_saved_a0`);
  tx-kernel 9 → 13 (4 new: `per_hart_slotted_sets_and_clears_slot_around_poll`,
  `per_hart_slotted_clears_slot_on_pending_exit`,
  `thread_future_dispatches_syscall_then_yields_for_userspace_entry`,
  `thread_future_terminates_on_exit_group`); tx-fs 13/13, tx-shims 12/12,
  tx-substrate sync 2/2. `cargo check --workspace`,
  `cargo check -p tx-kernel-riscv64-qemu-virt --target
  riscv64gc-unknown-none-elf`, `cargo fmt --check`,
  `cargo xtask progress validate` all clean. Next: Phase 3
  (page-fault async dispatch via `aspace.fault_script`) → Phase 5 (IRQ
  dispatch + UART RX) → Phase 7 (end-to-end smoke + retire trio fake
  driver). Blocker: none; the page-fault arm currently routes SIGSEGV
  unconditionally as a Phase 3 placeholder, and the AST checkpoint
  uses an empty batch because the reactor's per-task `AstSlot` is not
  yet exposed as a public surface to the thread future.

- 2026-05-06 Pre-ELF Phase 6 (mount/dev id allocators + Phase-4
  deferred `register_mount` wire-up) on branch `feat/pre-elf-runtime`.
  Per `docs/progress/plans/2026-05-06-pre-elf-runtime-completion.md`
  Part 5 §"MountId / DevId allocators (item 10)" + Phasing item 6.
  `crates/tx-subsystems/src/mount.rs` grows `static NEXT_MOUNT_ID:
  AtomicU64 = AtomicU64::new(1)`, `static NEXT_DEV_ID: AtomicU32 =
  AtomicU32::new(1)`, `pub fn allocate_mount_id() -> MountId`, `pub
  fn allocate_dev_id() -> DevId`, plus
  `reset_mount_id_counter_for_test` / `reset_dev_id_counter_for_test`
  test-only helpers (gated on `cfg(any(test, feature =
  "test-support"))`). The allocators are deterministic from cold
  start: rootfs's first call returns `MountId(1)` / `DevId(1)`,
  devfs's second call returns `(2)`/`(2)`, so existing trio
  boot-smoke assertions on the literal ids stay valid. New
  cross-crate test-support shims `reset_mount_table`,
  `reset_mount_id_counter`, `reset_dev_id_counter` in
  `tx-subsystems/src/lib.rs::cross_crate_test_support`; tx-kernel's
  `init/tests.rs::setup` calls them between runs. `init.rs`
  `mount_rootfs_tmpfs` and `mount_devfs_at_dev` flipped from
  `MountId::new(N)` / `DevId::new(N)` to the allocator helpers; the
  rootfs and devfs root rnodes now also carry
  `with_containing_mount` pointers (without these the walker emitted
  `ENODEV` because `fs_ops_for` returned `None`). After building the
  dev mount cap but before publishing to the `DEV_MOUNT` slot,
  init.rs calls `mount::register_mount(&rootfs_payload,
  dev_object_id, dev_mount.clone())` so the Phase 4 walker resolves
  `/dev/console` end-to-end without the legacy direct-RNode
  fallback. `tx-fs` devfs grew an `FsOps::materialise_rnode`
  override: the walker's terminal `CharDevice` arm now wraps the
  alias's TTY as `RNodeBacking::StructBacked { Tty }` instead of
  returning `ENOSYS`. New tx-kernel boot smoke
  `boot_smoke_walker_resolves_dev_console_after_mount_registration`
  asserts the walker's terminal DEntry's RNode is a `StructBacked
  Tty` matching the registered console. tx-kernel 8 → 9 (9/9);
  tx-subsystems 341/341, tx-fs 13/13, tx-shims 12/12, tx-substrate
  sync 2/2. `cargo check --workspace`, `cargo fmt --check`, `cargo
  xtask progress validate` all clean. Next: Phase 2 (reactor task
  wrapper + userspace-entry shim) → Phase 3 (page-fault async
  dispatch) → Phase 5 (IRQ dispatch + UART RX). Blocker: none.

- 2026-05-06 Pre-ELF Wave 1 (Phase 1 minor cleanups + Phase 4 VFS
  walker) landed on branch `feat/pre-elf-runtime` (worktree
  `funny-hugle-06199b`). Per
  `docs/progress/plans/2026-05-06-pre-elf-runtime-completion.md`.
  Phase 1 covers Part 5 §"tx-substrate public SpinMutex / Errno
  EEXIST+ENOTEMPTY / InlineName: Ord"; Phase 4 covers Part 3
  ("VFS walker / step_open"). Substrate-vs-subsystems layering fix per Open
  Q #5: `tx_subsystems::sync::SpinMutex` (`pub(crate)`) relocated to
  `tx_substrate::SpinMutex` (`pub`, re-exported at crate root). The
  two duplicate inline TAS shims retired:
  `crates/tx-kernel/src/init.rs::BootSpinMutex` and
  `crates/tx-fs/src/tmpfs.rs::SpinMutex` both deleted; their slots
  (`ROOT_MOUNT`, `DEV_MOUNT`, `CONSOLE_TTY`, `CONSOLE_OPS` serial,
  tmpfs `state` lock) re-pointed to `tx_substrate::SpinMutex`. 13
  in-tree `use crate::sync::SpinMutex` import sites in tx-subsystems
  (`signal`, `page_backed`, `wait_carrier`, `tty/structure/{registry,
  identity, payload}`, `vm/pmap`, `vm/structure/{recipe, range_lock}`,
  `thread_runtime/structure`, `process/{structure, execution}`)
  rewritten to `use tx_substrate::SpinMutex`. `Errno` extended with
  `EEXIST` (POSIX 17) and `ENOTEMPTY` (POSIX 39); `errno_to_i32`
  in `tx-shims/src/linux_syscall/mod.rs` extended in lockstep.
  tmpfs `create_inode`/`mkdir`/`symlink` flipped from `EINVAL` to
  `EEXIST` for name collisions; `rmdir` flipped from `EBUSY` to
  `ENOTEMPTY`; their `TODO(phase-vfs-errno-{eexist,enotempty})`
  comments deleted. `InlineName` grew a manual `Ord`/`PartialOrd`
  impl that compares `as_bytes()` (a derive would compare `len`
  first then include the trailing zero pad — wrong); tmpfs's
  per-directory `BTreeMap<TmpfsName=Vec<u8>, FsObjectId>` flipped
  to `BTreeMap<InlineName, FsObjectId>` and the `TmpfsName` newtype
  dropped. New tx-substrate integration test
  `tests/sync.rs::{spinmutex_lock_unlock_round_trip, spinmutex_holds_send_payload}`.
  Two new tx-fs tests: `tmpfs_create_existing_returns_eexist` and
  `tmpfs_rmdir_nonempty_returns_enotempty`. Phase 4 ships
  `crates/tx-subsystems/src/vfs/walker.rs` with `pub async
  step_walk` + `step_open`; mount-point crossing via the new
  `mount::register_mount` + `mount_for` registry (Phase 6 sibling
  will wire the registry calls into init.rs's `mount_devfs_at_dev`
  so production resolves `/dev/console` end-to-end without the
  walker fallback path); `RNodeBacking::Symlink { target:
  Box<[u8]> }` (changed from `Box<InlineName>` because InlineName
  rejects `/` in multi-component targets); new
  `FsOps::read_link` trait method default-`ENOSYS` with tmpfs
  override; `Errno::ELOOP` (POSIX 40) added; symlink chasing
  implements absolute-target restart vs relative-target splice
  with hop-budget `SYMLOOP_MAX = 40`. `open_console_for_init`
  redirected through `step_open(b"/dev/console", RDWR, 0, ...)`
  with a synchronous `block_on` shim and a fallback to the
  legacy direct-RNode path until init.rs registers the mount in
  the new registry. Two phases bundled in one commit because they
  share files (vfs/structure.rs, execution.rs::Errno,
  process/structure.rs, tmpfs.rs, linux_syscall/mod.rs). 10 new
  walker tests pass (relative path, absolute path, ENOENT,
  ENOTDIR-trailing-slash, ENOTDIR-mid-path, relative symlink,
  absolute symlink, ELOOP at 41 hops, mount crossing, step_open
  round-trip). tx-fs suite 10 → 13 (13/13); tx-kernel 8/8,
  tx-shims 12/12, tx-subsystems 331 → 341 (341/341), all
  tx-substrate integration tests green (incl. 2 new sync tests).
  `cargo check --workspace`, `cargo fmt --check`, `cargo xtask
  progress validate` all clean. Next: Phase 6 (mount/dev id
  allocators + register_mount wire-up in init.rs); then Phase 2
  reactor task wrapper + userspace-entry shim. Blocker: none.

- 2026-05-05 Trio Phase 6 (end-to-end userspace round-trip smoke) on
  branch `feat/trio-trap-syscall-tmpfs-devfs`. New host test
  `tx_kernel::init::tests::boot_smoke_userspace_round_trip_writes_console_then_exits`
  stitches the full chain together: fake userspace queue
  (`Syscall(write(1, "hi\n", 3))` then `Syscall(exit_group(0))`) →
  `UserspaceRunSlot::start_request` + `complete_interesting_trap` →
  `linux_syscall::dispatch` → `OpenFile::step_write` → TTY ldisc OPOST
  → `ConsoleIf::write_bytes` capture (asserts `b"hi\r\n"` post-OPOST)
  → Plan B writeback into `pending_syscall_return` → fake
  userspace-entry shim drains the slot into a test-local log
  (`Some(Ok(3))`, then `None` for the no-return exit). Loop terminates
  on `SyscallResult::NoReturn`; init transitions to zombie with
  `ExitStatus::Exited(0)`. The synthesised driver replaces the not-yet-
  existent reactor task wrapper + userspace-entry shim called out in
  the plan's "Cross-cutting risks #1": per-hart slot is staged
  manually via `set_current_thread_payload(0, _)` /
  `clear_current_thread_payload(0)` and the writeback path is host-
  side only (no `TrapFrameMut`). One additive seam introduced —
  `ThreadIdentity::payload_cap_for_test()` gated on `cfg(any(test,
  feature = "test-support"))`, mirroring the existing
  `cross_crate_test_support` pattern. tx-kernel suite at 8 (7 prior +
  1 new); tx-fs / tx-shims / tx-subsystems unchanged (10 / 12 / 331).
  Verified: `cargo check -p tx-kernel`, `cargo test -p tx-kernel
  --lib`, `cargo test -p tx-fs --lib -- --test-threads=1`,
  `cargo test -p tx-shims --lib`, `cargo test -p tx-subsystems --lib
  -- --test-threads=1`, `cargo check --workspace`, `cargo fmt
  --check`, `cargo xtask progress validate` all green. Next: lay down
  the production reactor task wrapper + real userspace-entry shim so
  the Phase 6 fake driver can be deleted. Blocker: none for the
  follow-up; ELF loading + first userspace binary remain explicitly
  out of scope per the trio plan.
- 2026-05-05 cwd / chdir / getcwd VFS integration on branch
  `process-topology`. First VFS↔process seam: processes now carry a
  `Cap<DEntry>` cwd. New `step_chdir(target, new_cwd)` (returns
  `ChdirOutcome::Replaced { prev } | ZombieIgnored`) and
  `step_getcwd(target) -> Option<Vec<u8>>` (renders absolute path
  by walking DEntry parent_hint chain to root). Path-render helper
  lives in vfs (`vfs::render_dentry_path`); process delegates. New
  `InlineName::ROOT` constant (empty-name marker for root dentry,
  bypasses `InlineName::new`'s empty-rejection); new
  `DEntry::parent_hint()` accessor. `cwd: SpinMutex<Option<Cap<DEntry>>>`
  field on `ProcessPayload`; bootstrap leaves it None until rootfs
  lands. `step_fork` snapshots parent.cwd alongside aspace + cred
  and threads through to the new payload — POSIX semantics: child
  inherits the cwd Cap; subsequent parent chdir doesn't affect child.
  PROCESS_v1 §3 amended (v1.2 note): `cwd: Cap<RNode>` →
  `cwd: Cap<DEntry>` with `root: Cap<RNode>` → `root: Cap<DEntry>`,
  matching VFS's ResolveCtx shape and enabling getcwd path-render
  via the named-path edge that DEntry carries. Send/Sync chain
  fix: Cap<DEntry> → Cap<RNode> → Cap<PageContainer> → BTreeMap
  with `*const ()` cache pin broke INIT_PROCESS static; resolved
  with `unsafe impl Send + Sync for PageContainer` mirroring the
  AddressSpace / TtyPayload precedent. 7 new tests using synthetic
  DEntry chains: getcwd-on-no-cwd → None, chdir-then-getcwd-root,
  chdir-then-getcwd-nested (`/usr/bin`), chdir-returns-prev,
  chdir-on-zombie, fork-inherits-cwd, parent-chdir-after-fork-doesnt-
  affect-child. Deliberately deferred: full Frame container (needs
  Shared<T> for CLONE_FS/VM/FILES/SIGHAND), Frame.root chroot
  boundary, path-string resolution (syscall driver), bootstrapped
  rootfs, fchdir, symlink-aware path render, CLONE_FS sharing. Suite
  at 329 (322 + 7 new); full `cargo xtask ci` green (11/11 gates).
- 2026-05-05 Pgrp selectors for `step_waitpid_nohang` (branch
  `process-topology`). Completes POSIX `waitpid(2)`'s pid-argument
  coverage: signed `pid_t` now maps fully to (`pid > 0` → `Pid`,
  `pid == 0` → `CallerPgrp`, `pid == -1` → `Any`, `pid < -1` →
  `Pgrp(-pid)`). Two new `WaitTarget` variants: `Pgrp(Pgid)`
  matches children whose `pgrp_cap().pgid` equals the target;
  `CallerPgrp` is resolved to a concrete `Pgrp(parent.pgrp_cap().pgid)`
  at the start of `step_waitpid_nohang` so the children walk only
  ever sees concrete selectors. The `WaitTarget::matches` helper
  panics-by-default on `CallerPgrp` (returns false) — programmer
  error if it reaches the walk. 5 new tests cover: caller-pgrp reaps
  same-pgrp zombie, caller-pgrp skips a child that has setpgid'd
  out, pgrp selector reaps child in specific pgrp, pgrp selector with
  no matching pgid → NoChildren, pgrp selector with live match →
  NoneReady. The day-1 `step_waitpid_nohang` surface is now
  fully POSIX-pid-coverage complete; only blocking variant remains
  deferred (needs reactor wait integration). Suite at 322 (317 + 5
  new); full `cargo xtask ci` green (11/11 gates).
- 2026-05-05 Session-leader-tty hangup cascade per `PROCESS_v1` §8.3
  (branch `process-topology`). Materialises the cascade doc-spelled
  in the P3 ratification pass, now buildable on top of
  `Session::foreground_pgrp_cap()` (added in P3) and the kernel-static
  init handle (added in boot wiring). New
  `session_leader_hangup_cascade(process)` helper detects "exiting
  process is session leader" via `process.pid.0 == session.sid.0`,
  then runs the four-step cascade: (1) two-hop weak deref to resolve
  fg pgrp via `session.controlling_tty_cap()` →
  `tty.foreground_pgrp_cap()`; (2) SIGHUP + SIGCONT to fg pgrp via
  `signal::step_kill_pgrp` (POSIX §11.1.3 — SIGCONT wakes any stopped
  members so they observe SIGHUP); (3) clear tty's `session_pgrp`
  slot (authoritative side per OPA-3); (4) clear
  `session.controlling_tty` mirror. Steps 3+4 fire even when step 1's
  fg-pgrp resolution returns None — the tty/session linkage must be
  severed regardless of pgrp upgrade success. Five short-circuit
  cases: non-leader exit (skip), no controlling tty (skip), full
  cascade, no fg pgrp (skip SIGHUP, still clear), init exit (full
  cascade fires when applicable). Wired into both
  `step_exit_group` and `step_process_exit` *before*
  `sever_children` and payload drop so signal-state infrastructure
  on the exiting process is still observable. 5 new cascade tests
  in `tty/tests/typed_session_pgrp.rs` (natural home — needs both
  TTY constructors and process surfaces): full happy path verifying
  binding clears; SIGHUP delivery to a *surviving* fg-pgrp member
  (forked child; init zombifies before assertion); non-leader exit
  preserves bindings; no-controlling-tty no-op; tty-with-no-fg-pgrp
  still clears tty.session_pgrp per spec. All 27 pre-existing process
  tests stay green: bootstrap sessions have no controlling tty, so
  the cascade short-circuits at the first hop in every existing
  topology test. Deliberately deferred: SigInfo carrier for
  SIGHUP/SIGCONT, atomic batching of the four substeps (POSIX
  permits class-3 compositional), §8.2 orphan-pgrp SIGHUP cascade
  (needs stop-state machinery). Suite at 317 (312 + 5 net); full
  `cargo xtask ci` green (11/11 gates).
- 2026-05-05 Boot wiring — pid=1 globally addressable + kernel boot
  path creates init (branch `process-topology`). Closes the largest
  remaining process-subsystem gap: `PROCESS_v1` §8.1's reparent-to-init
  arm, deferred since the children-container pass landed as
  sever-only because no globally-addressable init handle existed.
  New `static INIT_PROCESS: SpinMutex<Option<Cap<ProcessIdentity>>>`
  in `process/execution.rs` with `init_process()` accessor and
  `reset_init_process_for_test()` reset gate. New `BootstrapError`
  enum (`Zone(ZoneError)` / `AlreadyBootstrapped`); `bootstrap_init_process`
  return type changes from `Result<.., ZoneError>` to
  `Result<.., BootstrapError>` and now registers the resulting Cap
  in `INIT_PROCESS` (rejects second-bootstrap to prevent test
  leakage). `sever_children` upgraded from sever-only stub to
  three-case logic per §8.1: (1) init handle present and `init !=
  exiting process` → move children Caps from `process.children` into
  `init.children`, set each child's `parent` slot to `Weak<init>`;
  (2) init is the one exiting → sever-only (no higher-level reaper);
  (3) no init handle (test pre-bootstrap, pre-process-init at boot)
  → sever-only. `mem::take` drains `process.children` in all cases.
  tx-kernel boot path: new `init_process_subsystem()` step in
  `init_substrate_if_ready` after the reactor smoke tests; allocates
  init's `AddressSpace` via `new_cap_for_platform::<P>()`, calls
  `bootstrap_init_process`, drops the local Cap (INIT_PROCESS
  retains for kernel lifetime), writes
  `txkernel:<board>:process:init:ok` sentinel. `P: TxPlatform`
  already implies `PmapIf` per the supertrait chain so no new
  bound. Test isolation: `reset_init_process_for_test()` wired into
  setup() across 5 test files (process / signal — 4 inner-module
  setup() sites — / cred / thread_runtime / tty/typed_session_pgrp);
  one test (`weak_owner_proc_flips_dead_after_identity_drop`)
  manually releases INIT_PROCESS mid-test before drain. 5 new
  process tests cover bootstrap registration semantics, double-bootstrap
  rejection, init reparenting (init→middle→leaf chain, exit middle,
  assert leaf reparents to init + init.child_count grows + middle
  drained), and init-exit-without-reparent-target. Existing
  parent-exit tests stay green: every one uses `parent = bootstrap()`
  so parent IS init, hitting the "init is the exiting process"
  branch ⇒ same observable behavior. Suite at 312 (307 + 5 net);
  full `cargo xtask ci` green (11/11 gates) including the rv64
  qemu / m1dock mock / la64 qemu board targets compiling against
  the new boot path.
- 2026-05-05 `step_waitpid_nohang` reaps zombie children + retention
  fix for parent.children (branch `process-topology`). Materialises
  the WNOHANG path of `PROCESS_v1` §7.4 `script_waitpid`. New
  `step_waitpid_nohang(parent, target) -> Result<(Pid, ExitStatus),
  WaitError>` walks parent's children, finds a zombie matching
  `WaitTarget::Any` or `WaitTarget::Pid(p)`, reaps by withdrawing
  from `parent.children` and `child.pgrp.members`, drops the local
  Cap so identity reclaims after epoch drain. `WaitError` distinguishes
  `NoChildren` (POSIX ECHILD — no matching children) from `NoneReady`
  (WNOHANG no-zombie — POSIX returns 0). Implementing waitpid
  surfaced a real bug: `parent.children` was `Vec<Weak>`, so zombie
  children whose only retainer was the parent reclaimed before reap
  per §8.5's "zombies stay until reap" invariant. Switched to
  `Vec<Cap<ProcessIdentity>>` — children container now retains, only
  releasing at reap or parent reclaim. Asymmetry preserved with
  `pgrp.members` which stays `Vec<Weak>` (per §2.3, pgrp's retention
  is via `session.members` and `member.pgrp`, not via
  `pgrp.members`). Accessor renames: `child_slot_count` → `child_count`,
  `live_children` → `children` (no stale entries to filter under
  Cap retention). 10 new waitpid tests cover all four selector ×
  state combinations (any/specific × no-children/live/zombie),
  reap withdrawals on both sides (parent.children + pgrp.members),
  Signaled exit status round-trip, second-reap-after-exhaustion.
  Two pre-existing tests rewritten to match new retention model:
  `live_children_drops_stale_weak` → asserts test-Cap-drop is *not*
  enough to reclaim (parent retains); `pgrp_member_weak_observation`
  uses waitpid reap to fully release the child before asserting
  Weak goes stale. The day-1 process subsystem now closes the full
  reap cycle: fork → exit → SIGCHLD-to-parent → waitpid → reap.
  Deferred: blocking `waitpid` (reactor wait integration), pgrp
  selectors, WCONTINUED/WUNTRACED, siginfo carrier, auto-reap on
  SIGCHLD-Ignore. Suite at 307 (297 + 10 net); full `cargo xtask ci`
  green (11/11 gates).
- 2026-05-05 SIGCHLD edge in `step_process_exit` / `step_exit_group`
  on branch `process-topology`. Materialises the catchable producer
  half of `PROCESS_v1` §7.3.3 phase 5 — when a process zombifies,
  its parent's leader thread now receives a `SIGCHLD` post via
  `signal::step_kill_process`. New `post_sigchld_to_parent(process)`
  helper resolves the parent via `process.parent_cap()` (added in
  the prior drift cleanup); short-circuits if `None` (init / orphan
  / reclaimed parent), discards `KillOutcome::NoLiveThread` if the
  parent is itself a zombie. Wired into both exit paths after the
  zombification commit (post-`sever_children`, post-payload-drop,
  post-exit_status-write) so the parent observes a complete zombie
  when it acts on the SIGCHLD. Default mask is empty so the post
  populates the parent leader's `thread_pending` and updates
  `signal_summary.deliverable_signal`. Default action is Ignore so
  AST consult is a no-op; bits accumulate until the parent installs
  a handler or `wait(2)` reaps. 5 new tests cover happy path on both
  exit routes (step_exit_group + last-thread cascade), bootstrap-init
  no-parent skip, orphaned-child no-parent skip, and pathological
  zombie-parent NoLiveThread discard. Existing 292 tests verified
  green pre-add (no regression from the new producer). Deliberately
  deferred: `SigInfo` carrier (`si_pid` / `si_code` / `si_status`)
  pending the day-1 signal-surface extension; `exit_port` wake
  pending port machinery; SIGCHLD↔wait(2) auto-reap pending
  `script_waitpid`. Suite at 297 (292 + 5 new); full `cargo xtask
  ci` green (11/11 gates).
- 2026-05-05 `children` container on `ProcessIdentity` (branch
  `process-topology`). Pairs the `parent: Weak<ProcessIdentity>`
  field added in the prior drift-cleanup pass with the matching
  downward materialization per `PROCESS_v1` §2.1. New
  `children: SpinMutex<Vec<Weak<ProcessIdentity>>>` slot (same shape
  as `pgrp.members` and `session.members`); members held weakly so
  the parent does not pin children. Two new accessors:
  `child_slot_count()` (raw count incl. stale entries) and
  `live_children()` (snapshot + epoch-guarded upgrade + stale
  filter, returns owned Caps). `step_fork` now pushes the child's
  Weak into `parent.children` alongside the pgrp registration —
  bidirectional binding wired at fork time. New
  `sever_children(process)` helper materialises §8.1's day-1 stub:
  walks the children list under an epoch guard and clears each
  live child's `parent` slot to None. Wired into both
  `step_exit_group` (explicit group-exit) and `step_process_exit`
  (last-thread cascade) before payload drop; sever is shallow
  (direct children only — grandchildren keep their parent).
  Reparent-to-init lands with boot wiring (no globally-addressable
  init handle yet); after sever, a child's `parent_pid()` returns
  `Pid::RESERVED` (same shape as init itself). 7 new tests cover
  bootstrap-empty, single/multi fork accumulation, stale-Weak
  filtering after child drop, sever via both exit paths, and the
  shallow-sever invariant. Structurally unblocks `script_waitpid`,
  §7.3.3 phase-5 SIGCHLD edge, §8.2 orphan-pgrp SIGHUP detection,
  and §8.3 session-leader-tty hangup cascade. Suite at 292 (285 +
  7 new); full `cargo xtask ci` green (11/11 gates).
- 2026-05-05 Foreground-pgrp homing ratified TTY-owned (P3) on branch
  `process-topology`. Process audit had surfaced a spec/spec
  contradiction: `PROCESS_v1` §2.4 declared `Session.foreground_pgrp`
  while `OBJECT_PATTERN_FIXES_v1.md` OPA-3 (recommended) and the impl
  put the slot on `TtyIdentity.session_pgrp`. Architectural analysis
  picked TTY-owned for four reasons (subsystem encapsulation, hangup
  atomicity, bundled-pair invariant, lifecycle alignment). This pass
  closes the loop in code, docs, and lint. (1) New
  `Session::foreground_pgrp_cap()` + `controlling_tty_cap()` helpers
  hide the two-hop weak dereference (`Session.controlling_tty` →
  `TtyIdentity` → `tty.foreground_pgrp_cap()`); 3 new tests cover the
  happy path and both `None` failure modes. (2) `PROCESS_v1` §2.4
  removes the stale `foreground_pgrp` field from `Session`, restates
  `controlling_tty` as the mirror of the authoritative
  `TtyIdentity.session_pgrp`, and adds an explicit "fg pgrp not stored
  on Session" paragraph with the two-hop diagram. (3) `PROCESS_v1`
  §8.3 rewrites the session-leader-death cascade as 4 None-tolerant
  steps (resolve via helper → SIGHUP cascade → clear tty's
  session_pgrp → clear session's mirror) with an explicit class-3
  compositional atomicity note. (4) `OPA-3` flips "Recommended" →
  "Decided", drops the "two valid choices" preamble, expands the
  rationale into the four-leg argument, and adds new invariant
  **TTY-CTL-1a** ("no `foreground_pgrp` field on Session"). (5) Two
  new `cargo xtask lint arch` rules enforce TTY-CTL-1 (rejects
  `Cap<ProcessIdentity>` in `tty/structure/identity.rs`) and
  TTY-CTL-1a (rejects `foreground_pgrp:` field decl in
  `process/structure.rs`, with comment-line escapes); 6 new xtask
  unit tests cover rejection + allowance cases. Suite at 285 (282 +
  3 new); xtask at 43 (37 + 6 new); full `cargo xtask ci` green
  (11/11 gates).
- 2026-05-05 Process subsystem drift cleanup against `PROCESS_v1`
  (branch `process-topology`). Doc/impl coherence audit identified
  four mechanism-level drift items the spec already pins; this pass
  closes all four without touching deferred features. (1) Renamed
  `step_zombie` → `step_process_exit` per §7.3.3 (last-thread cascade
  named for the verb, not the side-effect; full §7.3.3 phase-5
  cascade still future). (2) Renamed `Session.groups` →
  `Session.members` per §2.4 (matches `ProcessGroup.members` already-
  correct shape; accessor `group_slot_count` →
  `member_slot_count`). (3) Unified the parallel
  `exit_status: SpinMutex<Option<i32>>` and
  `terminating_signal: SpinMutex<Option<Signum>>` slots into single
  `exit_status: SpinMutex<Option<ExitStatus>>` with
  `enum ExitStatus { Exited(i32), Signaled(Signum) }` per §6.2. The
  `128 + sig` shell-convention encoding moves into
  `ExitStatus::wait_status_word()`; `terminating_signal()` accessor
  derives from the enum. `step_exit_group` signature now takes
  `ExitStatus`; `step_exit_group_with_signal` is the thin
  `Signaled(sig)` wrapper. (4) Replaced bare `parent_pid: Pid` with
  `parent: SpinMutex<Option<Weak<ProcessIdentity>>>` per §2.1 — same
  retention story as spec's `Binding<ProcessIdentity>` (no retention),
  uses the substrate primitives we have today. New `parent_cap()` /
  `parent_pid()` accessors; init has `parent = None`, fork sets
  `Some(parent.downgrade())`. Unblocks the future children-DLL pass.
  Out of scope: children container, `step_process_exit` SIGCHLD/
  exit_port/reparent, GroupExit, leader_exit_status, Frame, nsproxy,
  Session.foreground_pgrp homing — all roadmap items the audit
  flagged separately. Suite at 282; full `cargo xtask ci` green
  (11/11 gates).
- 2026-05-05 `signal::ast_dispatch` closes the AstOutcome →
  step_exit_group_with_signal loop on branch `process-topology`.
  Thin wrapper over `ast_check` that materialises the day-1
  side-effects we have wired: `AstOutcome::DefaultTerminate { sig }`
  invokes `step_exit_group_with_signal(owner_proc, sig)` so the
  catchable-fatal-default path now actually terminates the process
  instead of just being a recognised intent. Other variants
  (`Continue`, `InitiateTermination`, `DefaultStop`, `DefaultContinue`,
  `DeliverHandler`) flow through unchanged — their materialisation
  still needs the future thread_future poll, stop/continue
  control ops, and signal-frame construction. 4 new tests cover
  default-terminate-zombifies-with-signum, continue no-op,
  recognised-but-unrealised stop and handler. Full end-to-end
  testable: `post_signal(SIGTERM)` → `ast_dispatch` →
  `is_zombie() && terminating_signal == Some(SIGTERM)`. Suite at
  284; full `cargo xtask ci` green (11/11 gates).
- 2026-05-05 `step_exit_group_with_signal` lands on top of Gewalt/event
  factoring (branch `process-topology`). Materialises the SIGKILL
  control-op invocation that `SIGNAL_v1` §12.3 `route_sigkill`
  prescribes. New `ProcessIdentity.terminating_signal: SpinMutex<Option<Signum>>`
  field with `terminating_signal()` accessor; new
  `process::step_exit_group_with_signal(proc, sig)` sets the slot
  and calls `step_exit_group(proc, 128 + sig.raw())` (shell-
  convention status until `wait(2)` lands and switches to Linux
  encoding). `signal::route_gewalt(SIGKILL)` now invokes
  `step_exit_group_with_signal` directly instead of setting
  `summary.termination` — the target zombifies on the spot per spec
  ("exit_status encodes 'killed by SIGKILL'"). The
  `summary.termination` AST priority-1 path remains for the future
  fatal-synchronous-fault and ptrace-fatal producers; the matching
  test now sets the bit explicitly via `update_summary`. SIGSTOP /
  SIGCONT routes unchanged: still update `stop_requested` since
  there's no stop-state machine yet. 3 new tests + 2 reshaped tests;
  suite at 280; full `cargo xtask ci` green (11/11 gates).
- 2026-05-05 Gewalt/event factoring restored on top of signal delivery
  sweep (branch `process-topology`). Audit found day-1 collapsed the
  spec's two signal categories into one `post_signal` pipeline:
  SIGKILL/SIGSTOP/SIGCONT entered `thread_pending` alongside
  catchable signals, with summary special-cases on top. Per
  `SIGNAL_v1` §1 + §2 Consequence 2 the Gewalt signums must bypass
  pending queues entirely. Refactor: new `signal::route_gewalt(target,
  sig)` walks every live thread of the target process and updates
  `signal_summary` directly (SIGKILL → termination, SIGSTOP →
  stop_requested, SIGCONT → clear stop_requested) without touching
  pending queues. `signal::step_kill_process` dispatches by signum:
  Gewalt → `route_gewalt`, catchable → `post_signal` to leader
  thread. `signal::is_gewalt(sig)` is the public predicate. Pgrp
  shims (`step_kill_pgrp`, `script_kill_pgrp`) skip the
  `group_pending` mirror for Gewalt members. `post_signal` contract
  tightens with a `debug_assert!` rejecting Gewalt signums; its
  body strips the SIGKILL/SIGSTOP/SIGCONT special-cases and only
  handles catchable signals (sets `summary.deliverable_signal` when
  unmasked). Existing 2 SIGSTOP/SIGCONT tests ported to
  `step_kill_process` route; 5 new tests cover pending-queue bypass
  per Gewalt signum and pgrp non-mirroring; `ast_check_default_continue_for_sigcont`
  removed (SIGCONT is Gewalt → never visits AST in day-1; its
  enqueue-for-handler half lands when SIGCONT-with-handler is wired).
  Suite at 277; full `cargo xtask ci` green (11/11 gates).
- 2026-05-05 Signal delivery sweep day-1 lands on top of TTY → signal
  end-to-end (branch `process-topology`). Realises the day-1 subset
  of `SIGNAL_v1` §14 (selection algorithm) and §15.1 (ast_check) plus
  `THREAD_RUNTIME_v1` §5.2 (interrupt summary). New types in
  `signal.rs`: `InterruptSummary { deliverable_signal, termination,
  stop_requested }` with atomic-packing helpers; `DefaultAction
  { Term, Core, Ignore, Stop, Cont }` + `default_action(sig)`
  table; `PendingSource { Thread, Group }`; `AstOutcome` with 6
  variants (Continue, InitiateTermination, DefaultTerminate,
  DefaultStop, DefaultContinue, DeliverHandler). New
  `signal_summary: AtomicU8` on `ThreadPayload` with `interrupt_summary()`
  accessor + crate-internal `update_summary` CAS-loop helper.
  `post_signal` now keeps the summary current: unmasked posts set
  `deliverable_signal`; SIGKILL sets `termination` and
  `deliverable_signal` (uncatchable, bypasses mask); SIGSTOP-family
  sets `stop_requested`; SIGCONT clears `stop_requested`.
  `step_sigprocmask` recomputes `deliverable_signal` against the new
  mask. `select_next_signal(thread)` returns the lowest deliverable
  signum + source-queue tag, scanning `thread_pending` first then
  `group_pending`. `ast_check(thread)` runs the SIGNAL_v1 §15.1 loop:
  termination → InitiateTermination, else dequeue + consult
  `sig_actions` + map `Default` via `default_action`, with Ignore /
  Default-Ignore re-looping. 21 new tests bring suite to 272; full
  `cargo xtask ci` green (11/11 gates). Site-A wait-adapt
  integration, signal-frame construction, and group-exit-with-signal
  invocation remain deferred (need reactor / scripts / HAL trap-
  return wiring).
- 2026-05-05 TTY → signal end-to-end typed dispatch lands on top of
  the kill-permission check (branch `process-topology`). Closes the
  last raw-id seam in TTY's job-control flow: `SignalTarget`
  variants become struct-shaped `{ pgid: u32, pgrp:
  Option<Weak<ProcessGroup>> }`, `IoctlCaller` gains a `pgrp:
  Option<Weak<ProcessGroup>>` field with a `with_pgrp_weak()`
  builder, and the ioctl/hangup steps populate the typed Weak from
  `tty.session_pgrp().foreground_pgrp` (already typed since the TTY
  pgrp rebinding pass). New `signal::deliver_tty_dispatch(source,
  dispatch)` upgrades the Weak under one epoch guard, maps
  `JobControlSignal` to `Signum`, and calls the cred-checked
  `script_kill_pgrp`. End-to-end test demonstrates VINTR-style
  dispatch posting SIGINT to every member of the typed foreground
  pgrp; partial-permission and zombie-source cases covered. Hybrid
  preserved: legacy raw-id binders still work (typed slot stays
  `None` and the bridge returns `DispatchOutcome::NoTypedPgrp`). 5
  new tests bring suite to 251; full `cargo xtask ci` green
  (11/11 gates).
- 2026-05-05 Kill permission check lands on top of TTY pgrp typed
  rebind (branch `process-topology`). Wires `cred` into the `signal`
  shim per `SIGNAL_v1` §32 and `cred_service_v_1`. New
  `cred::require_signal_send(source: Cred, target: &TargetProcCred,
  sig, &Guard) -> Result<SignalAuthorized<'g>, Errno>` runs the
  permission rule and emits a zero-sized witness. New
  `process::structure::TargetProcCred` is the day-1 subset of the
  illustrative `{ruid, euid, suid, ..., same_session, dumpable}`
  shape from the cred doc — `{uid, euid, gid, egid, same_session}`.
  `ProcessIdentity::target_proc_cred_for(&source)` builds it,
  computing `same_session` by comparing the source's and target's
  pgrp `Cap<Session>` keys. New `signal::script_kill_process`,
  `signal::script_kill_pgrp`, and `signal::script_kill_probe` compose
  the cred check with the existing `step_kill_*` posters; the latter
  is the POSIX `kill(pid, 0)` permission probe. Day-1 rule:
  `(source.uid, source.euid) × (target.uid, target.euid)` match,
  `CAP_KILL`/root bypass, SIGCONT-same-session bypass — Linux's full
  4-way `(uid,euid) × (uid,suid,ruid)` is the saved-set extension
  that lands when Cred grows `suid`/`ruid`. `Errno` gains `EPERM` and
  `ESRCH` (POSIX kill returns EPERM on permission deny, ESRCH on
  zombie source). 12 new tests bring the suite to 246; full
  `cargo xtask ci` green (11/11 gates).
- 2026-05-05 TTY pgrp typed-rebinding lands on top of cred day-1
  (branch `process-topology`). `TtyIdentity.session_pgrp` now carries
  both raw POSIX IDs (legacy fast path) and typed
  `Weak<Session>` / `Weak<ProcessGroup>` references. New constructors:
  `SessionPgrp::from_raw_ids(...)` (no typed refs, used by all
  existing TTY tests) and `SessionPgrp::from_typed(&session, &pgrp)`
  which caches the IDs from the caps and downgrades to Weak refs.
  `TtyIdentity` gains `bind_session_pgrp_typed(...)` and
  `foreground_pgrp_cap()` so signal-fanout callers can hand the
  foreground pgrp Cap directly to `signal::step_kill_pgrp`. Required
  bumps: `tx-substrate::zone::Weak<T>` Clone/Copy made unconditional
  (manual impls — derive was emitting spurious `T: Clone` bounds);
  `unsafe impl Send + Sync for AddressSpace` to lift the
  page-allocator MapPin's intentionally-!Send into the
  shared-by-discipline shape that lets `Cap<AddressSpace>` flow
  through `ProcessPayload` and transitively through `Weak<Session>`
  inside `SessionPgrp`. tty/tests.rs at 1500-line ceiling so split
  into `tty/tests/legacy_phase_a.rs` + `tty/tests/typed_session_pgrp.rs`.
  7 new tests bring suite to 234; full `cargo xtask ci` green
  (11/11 gates).
- 2026-05-05 Cred service stub layered on top of signal day-1 (branch
  `process-topology`). Adds `crates/tx-subsystems/src/cred.rs` with
  POSIX cred types (`Uid`, `Gid`, `Capability`, `CapabilitySet`,
  `Cred`) and the `step_setuid` / `step_setgid` shims. `ProcessPayload`
  gains `cred: SpinMutex<Cred>`; `bootstrap_init_process` initializes
  with `Cred::root()`; `step_fork` inherits the parent's cred unchanged.
  Privilege model: root or `CAP_SETUID`/`CAP_SETGID` allows arbitrary
  id changes; non-privileged callers may only swap among existing
  `(uid, euid)` / `(gid, egid)` pairs. Saved-set IDs, `fsuid`/`fsgid`,
  supplementary groups, capability bounding/inheritable/ambient sets
  all deferred — extend rather than reshape. 11 new tests; total
  suite 227. Full `cargo xtask ci` green (11/11 gates).
- 2026-05-05 Signal day-1 layered on top of the topology branch
  `process-topology`. Adds `crates/tx-subsystems/src/signal.rs` with
  the POSIX-shim types (`Signum`, `SignalMask`, `PendingSignalQueue`,
  `SigDisposition`, `SigActionTable`) and the kill / sigaction shim
  entry points (`step_kill_process`, `step_kill_pgrp`,
  `step_sigaction`). `ProcessPayload` now carries `sig_actions` and
  `group_pending`; `ThreadPayload` carries `signal_mask` and
  `thread_pending`. `thread_runtime::execution` gains `post_signal`
  and `step_sigprocmask` (with `SigmaskHow::SetMask/Block/Unblock`).
  Day-1 deliberately stops at "post + observe": no SigInfo payload,
  no realtime per-occurrence queue, no default-disposition resolution
  (`Default → terminate / stop / continue / ignore`), no AST
  delivery. SIGKILL/SIGSTOP are uncatchable at the type layer
  (`SignalMask::block` strips them; `step_sigaction` returns
  `Uncatchable`). 13 new tests bring the suite to 216; full
  `cargo xtask ci` green (11/11 gates).
- 2026-05-05 Process / Thread topology pass started on branch
  `process-topology`. Lands the entity graph for the upcoming β bundle
  without signal state, credentials, rlimits, or fd-table coupling —
  topology first, signals second so the entity shapes do not have to
  compromise for signal semantics later. Four new zone-allocated
  entities: `ProcessIdentity` ↔ `ProcessPayload` (identity-payload
  split, zombies retain identity), `ThreadIdentity` ↔ `ThreadPayload`
  (same), `ProcessGroup`, `Session`. Step set: `bootstrap_init_process`,
  `step_fork` (clones aspace via `AddressSpace::fork_aspace`, creates
  leader thread, inherits parent pgrp), `step_exit_group`,
  `step_thread_exit` (last-thread zombifies parent), `step_setpgid`
  (day-1 only supports `pgid == target.pid`; existing-group join is a
  follow-up), `step_setsid`. 18 topology tests pass; full
  `cargo xtask ci` green (11/11 gates). `tx-subsystems::lib.rs` empty
  stubs `pub mod process {}` / `pub mod thread_runtime {}` removed.
  Reactor `TaskKey` slot on `ThreadPayload` is `None` until β4 wires
  the runtime; signal mask / summary / pending queues land in the
  signal pass. TTY `session_pgrp` triplet still holds raw IDs — typed
  `Weak<Session>` / `Weak<ProcessGroup>` rebinding is a small follow-up
  before β3.
- 2026-05-04 VM compliance fixup landed on branch `vm-compliance-fixup`.
  Closes the drift items identified in the post-merge VM audit against
  `VM_v1_2.md`: (1) renamed VM scripts to spec names — `mmap_script` /
  `munmap_script` / `mprotect_script` / `mremap_script` / `fault_script`
  / `brk_script`; sync helpers became `try_mmap` / `try_munmap` /
  `try_mprotect` / `try_mremap`. (2) Doc reconciled to match impl —
  `VAddrRange` → `UserRange`, `UserRange::full_user_v1` / `new_aligned`
  constructor names; mincore signature clarified as per-page
  `Vec<bool>` (matches POSIX); §2 recipes carry an implementation note
  for the COW-`BTreeMap` shape. (3) `MADV_DONTNEED` and `MADV_FREE`
  implemented per §5.9 (range-scoped pmap teardown + shootdown, recipes
  preserved); was a no-op. (4) `RangeLock::acquire_step` /
  `acquire_pair_step` now return canonical `StepOutcome<RangeGuard>` as
  the spec specifies; the rich `AcquireResult` is retained as
  `acquire_step_rich` for writer-preference tests. Production scripts,
  `reserve_map`, `acquire_writer`, and `MapReserveResult::Blocked` are
  all on the canonical surface. Two new behavior tests for
  DONTNEED/Free; 79/79 vm:: tests pass; all 11 CI gates green.
  `exec_aspace` rebuild half and the `BTreeMap` → persistent-BTree
  optimization remain deferred (need `ExecImage` / process subsystem;
  tracked outside the fixup).
- 2026-05-04 VFS spec-reconciliation Phases 1-4 complete on branch
  `vfs-spec-reconciliation`. The four-phase plan that started after
  the deferred-move investigation is now fully landed: Phase 1 brought
  `tx-kernel/src/vfs.rs` into doc-canonical shape (full POSIX
  `InodeMeta`, opaque `[u8; 16]` `DirCursor`, `Timespec`, `MountOutput`,
  four-module `vfs/{structure,checks,execution}/` layout); Phase 2
  brought the tx-subsystems skeleton into spec (POSIX `Errno`
  spelling, substrate-owned PPN-handle `Frame`); Phase 3 ported
  tx-ext4 onto the canonical surface (rewrote `pager::fetch_page` to
  allocate via `page_allocator::reserve_frame` + permanent-frame token,
  copy bytes through the test direct-map; deleted the byte-buffer-
  Frame-dependent `vfs_full_read` and `kernel_read_backend` test
  files); Phase 4 deleted the now-redundant skeleton and `git mv`-ed
  the working subsystems from tx-kernel to tx-subsystems. tx-kernel
  collapsed to `init.rs + trap.rs + lib.rs`. Verification: workspace
  builds clean, 522 tests pass with `--test-threads=1`,
  `cargo xtask lint arch/unused/docs` ok, `cargo xtask progress
  validate` 24 records ok. The vm/tty move blocker recorded in
  `2026-05-04-vm-tty-subsystems-move-deferred.md` is resolved.
- 2026-05-04 VFS spec-reconciliation Phase 1 complete on branch
  `vfs-spec-reconciliation`. Brings `tx-kernel/src/vfs.rs` into
  doc-canonical shape per `TX_EXT4_PLAN_v1_2.md`,
  `bringup_fs_specs_v_1`, and `SUBSYSTEM_ANATOMY_v2_1.md`. Five sub-
  steps landed: (1) `InodeMeta` extended to full POSIX layout with
  `atime/mtime/ctime: Timespec`, `nlinks/blocks/flags`; doc-absent
  `kind`/`rdev` removed; (2) `DirCursor` reshaped from `u64` to
  opaque `[u8; 16]` per spec, with `from_u64`/`as_u64` helpers for the
  common case; (3) `MountOutput` type added; (4) workspace cascade
  verified — TTY, Mount, page_backed adapt cleanly; (5) flat 753-line
  `vfs.rs` decomposed into the four-module layout `vfs/{mod,structure,
  checks,execution,tests}.rs`. Verification: cargo fmt clean, all 183
  tx-kernel tests pass with `--test-threads=1`, workspace test gates
  green (215 tests total across crates), `cargo clippy -p tx-kernel
  -- -D warnings` clean, `cargo xtask lint arch/unused/docs` ok,
  `cargo xtask progress validate` 24 records ok. Reconciles five drift
  axes flagged in the deferred-move decision note. Next: Phase 2
  (skeleton in tx-subsystems → spec — `Frame` PPN model, `Errno`
  POSIX spelling), Phase 3 (tx-ext4 to consume canonical surface),
  Phase 4 (delete skeleton + move vm/tty into tx-subsystems).
- 2026-05-04 Final ledger revised post-audit. The
  `2026-05-04-vm-pagebacked-final-ledger.md` and the closure decision
  note now reflect 20 plan steps complete (17 original + 3 audit
  follow-ups), revised completion ~92% structure / ~88% behavior, and
  fix the prior mis-classification of fork_aspace / exec_aspace as
  Process-blocked. Both are landed VM-side primitives. Residual gaps
  recorded as stylistic / optimization (StepOutcome return type, true
  persistent BTree, hidden rewrite_range primitive) and out-of-scope
  (concrete VFS backends, ThreadRuntime trap dispatch, PageBacked
  PC-side wait channels).
- 2026-05-04 VM doc-spelling polish + fork full-user serialization (plan-
  extension step vm-doc-polish-and-full-user-range). `RangeLock::acquire`
  and `acquire_pair` renamed to `acquire_step` / `acquire_pair_step` to
  match VM_v1_2 §3.1; `AcquireResult` / `AcquirePairResult` retained as
  the 2-variant Result shape because StepOutcome integration is a
  separate concern. New `types::FULL_USER_V1_TOP = 1 << 38` constant and
  `UserRange::full_user_v1()` method (sized for Sv39 and Sv48 user
  halves). `AddressSpace::fork_aspace` now acquires ExclusiveWriter on
  full_user_v1 before snapshotting parent recipes per VM_v1_2 §9.5;
  WouldBlock surfaces as `VmMapError::WouldBlock`. One new test confirms
  the fork lock fires. Verification: `cargo fmt --check` clean, vm 77 ok
  (was 76, +1), page_backed 49 ok, lib 138 ok, workspace clippy clean,
  `cargo xtask lint arch/unused/docs` ok, `cargo xtask progress validate`
  24 ok.
- 2026-05-04 fork_aspace + exec_aspace landed (plan-extension steps
  fork-aspace and exec-aspace, closing VM_v1_2 §5.6 / §5.7 gaps the
  audit caught after the plan's first closure). `AddressSpace::fork_aspace::<P>(parent)`
  snapshots parent recipes, builds a fresh child AddressSpace, commits each
  recipe into the child (Cap refcount bumps share PageContainers), and
  tears down parent's pmap on MAP_PRIVATE entries so subsequent writes
  refault and CoW. MAP_SHARED PTEs in parent stay intact; child's pmap
  starts empty and rebuilds via refault. `AddressSpace::exec_aspace(old)`
  tears down every materialized PTE across all current recipes via the
  new `AddressSpace::teardown_all_pmap` helper; recipe tree management
  remains caller-side because the new image's shape comes from the
  Process-side exec image loader. Three new tests cover recipe-clone
  shape, MAP_PRIVATE-only PTE demotion, and exec teardown of all PTEs.
  Verification: `cargo fmt --check` clean, vm 76 ok (was 73, +3),
  page_backed 49 ok, lib 137 ok, workspace clippy clean, `cargo xtask
  lint arch/unused/docs` ok, `cargo xtask progress validate` 24 ok.
- 2026-05-04 VM/PageBacked v1 completion plan closed. All 17 plan steps
  (16 original + 1 plan-extension prerequisite) complete; plan status
  flipped from active to complete. VM/PageBacked has moved from the
  post-resync ~45% structure / ~30% behavior to roughly ~85% structure /
  ~80% behavior against VM_v1_2 / PAGE_BACKED_v1. Final ledger:
  `docs/progress/research/2026-05-04-vm-pagebacked-final-ledger.md`.
  Closure decision:
  `docs/progress/decisions/2026-05-04-vm-pagebacked-v1-plan-closure.md`.
  Remaining 15-20% of contract surface is exactly what the active design
  docs already mark deferred-by-v1 or what depends on a Process subsystem
  that does not yet exist (fork_aspace, exec_aspace, trap page-fault
  dispatch). Recommended next milestones: Process / ThreadRuntime
  integration (unblocks fork/exec/trap dispatch), concrete VFS backends
  (ext4 / devfs / bdev-fs replace the FsPageBacking mocks), per-
  PageContainer wait channels so fault_script_async honors File-variant
  PC-side blocking. Final verification: cargo fmt --check clean, vm 73
  ok, page_backed 49 ok, lib 134 ok, substrate page_allocator 18 ok,
  workspace clippy clean, cargo xtask lint arch/unused/docs ok, cargo
  xtask progress validate 24 ok.
- 2026-05-04 fault_script_async landed (plan step fault-script-async).
  Loops the three-step fault sequence: acquire Materializer + observe
  recipe + drop, materialize, re-acquire Materializer + publish.
  RangeLock WouldBlock at step 1 or step 3 drops the guard (and any held
  materialization), awaits `wait_carrier::wait_on_token`, retries from
  step 1 with fresh recipe observation. Inner sync helpers preserved.
  PC-side blocking on File-variant `materialize_page` (FsPageBacking
  fetch/flush) is not yet routed through this script — that remains a
  follow-up requiring per-PageContainer wait channels analogous to the
  RangeLock channel. Two new tests in `vm/tests/script_async.rs` cover
  uncontended-one-poll-success and writer-conflict-yields-and-completes-
  after-release. Verification: `cargo fmt --check` clean, vm 73 ok (was
  71, +2), page_backed 49 ok, lib 134 ok, workspace clippy clean,
  `cargo xtask lint arch/unused/docs` ok, `cargo xtask progress
  validate` 24 ok.
- 2026-05-04 brk_script_async landed (plan step brk-script). Models the
  program break as an Anon PrivateAnon mapping covering
  `[brk_base, current_brk)`. Grow calls `map_script_async` on the new
  range; shrink calls `unmap_async`; equal returns current; below
  `brk_base` rejects `InvalidRange`. Process-level brk tracking is out
  of scope for VM. Four new tests in `vm/tests/script_async.rs`.
  Verification: `cargo fmt --check` clean, vm 71 ok (was 67, +4),
  page_backed 49 ok, lib 132 ok, workspace clippy clean, `cargo xtask
  lint arch/unused/docs` ok, `cargo xtask progress validate` 24 ok.
- 2026-05-04 unmap/protect/remap async wrappers landed (plan step
  munmap-mprotect-mremap-scripts), each following the
  `map_script_async` template. `AddressSpace::unmap_async`,
  `AddressSpace::protect_async`, `AddressSpace::remap_async` loop on
  their inner sync helper, drop the blocked guard on WouldBlock, await
  `wait_carrier::wait_on_token`, retry. Inner sync helpers preserved.
  `remap_async` stays in disjoint-only mode for v1. Three new tests
  exercise blocked-then-release-wakes-and-completes for each wrapper.
  Verification: `cargo fmt --check` clean, vm 67 ok (was 64, +3
  script_async), page_backed 49 ok, lib 128 ok, workspace clippy clean,
  `cargo xtask lint arch/unused/docs` ok, `cargo xtask progress
  validate` 24 ok.
- 2026-05-04 mmap-script-async landed end-to-end as the working template
  for the async script wave (plan step mmap-script-async). RangeLock now
  owns a `tx_reactor::wait::Channel` registered with `wait_carrier`;
  release fires `RANGE_LOCK_RELEASE_MASK` so blocked acquirers can wake.
  `WouldBlock<'a>` carries a `&'a RangeLock` and exposes
  `wait_token() -> WaitToken`. `Drop for RangeLock` releases the carrier
  registration. `AddressSpace::map_script_async` loops calling
  `reserve_map`, drops the blocked guard, awaits
  `wait_carrier::wait_on_token`, and retries — honoring VM_v1_2 §3.6
  cross-async-wait discipline. Four tests in `vm/tests/script_async.rs`
  cover wait_token shape, uncontended one-poll success, blocked-then-
  release wakes the future, and external Channel subscribers see the
  release fire. Verification: `cargo fmt --check` clean, vm 64 ok (was
  60, +4 script_async), page_backed 49 ok, lib 125 ok, workspace clippy
  clean, `cargo xtask lint arch/unused/docs` ok, `cargo xtask progress
  validate` 24 ok.
- 2026-05-04 WaitToken → Channel resolver landed (plan-extension step
  waittoken-channel-resolver, prerequisite for the four async script
  wrappers). New `tx_kernel::wait_carrier` module holds a
  `SpinMutex<BTreeMap<u64, tx_reactor::wait::Channel>>` registry plus an
  `AtomicU64` carrier id allocator. `register_wait_channel(channel)`,
  `release_wait_channel(id)`, `lookup_wait_channel(id)`, and
  `wait_on_token(token)` round-trip a `WaitToken` whose carrier is a
  registered id into a `WaitFuture`. Test placeholder tokens (e.g.
  `BlockingFs`/`LifecycleFs` returning `WaitToken::new(13, 0x55)`)
  unregistered carriers return `None` from `wait_on_token` rather than
  panicking, so existing test mocks keep working. Six tests cover the
  register/lookup/release shape and the placeholder-token case.
  Verification: `cargo fmt --check` clean, page_backed 49 ok, vm 60 ok,
  lib 121 ok (was 115, +6), workspace clippy clean, `cargo xtask lint
  arch/unused/docs` ok, `cargo xtask progress validate` 24 ok.
- 2026-05-04 Reflink + CoW-on-write scaffolding landed (plan step
  reflink-cow-scaffold). New `page_backed::install_shared_page(pc, page,
  source_ppn)` and `page_backed::cow_replace_into_private(pc, page)` in a
  new sibling `page_backed/reflink.rs` module. `install_shared_page`
  acquires a fresh `CachePin` on the source PPN and inserts via
  `install_if_absent` (cache_ref bumps so source stays live); rejects
  Device backings and pre-existing entries. `cow_replace_into_private`
  allocates a fresh zeroed frame, copies bytes through the substrate
  `FrameCopier`, and swaps the page-cache entry via `install_if_match`
  (now production code so concurrent CoW linearizes); old `CachePin`
  drops on success, decrementing the source's cache_ref. Six tests in
  `reflink_tests.rs`. Real reflink across two RNodes (filesystem-side
  refcount accounting) and the §12.4 reflink-vs-truncate race remain
  deferred. Verification: `cargo fmt --check` clean, vm 60 ok,
  page_backed 49 ok (43 + 6 reflink), lib 115 ok, workspace clippy clean,
  `cargo xtask lint arch/unused/docs` ok, `cargo xtask progress validate`
  24 ok.
- 2026-05-04 Persistent EBR-backed recipe publication landed (plan step
  persistent-epoch-recipes). `RecipeIndex` now publishes via
  `AtomicPtr<RecipeTree>` for lock-free reads under `epoch::Guard`, with a
  separate writer mutation `SpinMutex` serializing mutators. Writers
  atomically swap and retire the old tree through
  `tx_substrate::epoch::retire_raw`, which is now public so upper-layer
  publication paths can opt into EBR-managed reclamation. Internal read
  methods take `&Guard<'_>` and load via a `pinned()` helper that performs
  an Acquire load and relies on the caller's guard for soundness.
  `AddressSpace` public read methods keep their existing signatures by
  creating a short-lived internal `epoch::guard()`; `msync` threads its
  caller-supplied guard directly. `RecipeSnapshot` is now `cfg(test)`
  (only the publication-rule test still consumes it). `Drop` on
  `RecipeIndex` frees the final tree. Satisfies VM_v1_2 §1.2 publication
  rule with guard-scoped reader lifetimes. Verification: `cargo fmt
  --check` clean, vm 60 ok, page_backed 43 ok, lib 109 ok, substrate
  page_allocator 18 ok, workspace clippy clean, `cargo xtask lint
  arch/unused/docs` ok, `cargo xtask progress validate` 24 ok.
- 2026-05-04 Midway checkpoint: 10 of 17
  vm-pagebacked-v1-completion plan steps complete (~60% structure / ~55%
  behavior against VM_v1_2 / PAGE_BACKED_v1). Catch-up note at
  `docs/progress/research/2026-05-04-vm-pagebacked-midway-checkpoint.md`.
  Remaining seven slices: `persistent-epoch-recipes` is a lock-free
  architecture upgrade (correctness-equivalent to today; multi-session
  rewrite warranting its own sub-plan); the four async script wrappers
  (`mmap-script-async`, `munmap-mprotect-mremap-scripts`, `brk-script`,
  `fault-script-async`) need a tx-reactor `WaitToken → Channel` resolver
  plus `RangeLock` async-wait integration before they can honor
  VM_v1_2 §3.6 cross-async-wait discipline; `reflink-cow-scaffold`
  depends on `persistent-epoch-recipes`; `ledger-and-status-final` closes
  the plan once those land. Recommended next moves: push branch, spawn a
  focused resolver slice, then a dedicated `persistent-epoch-recipes`
  slice. Verification for the checkpoint: `cargo xtask progress validate`
  24 ok, `cargo xtask lint docs` ok.
- 2026-05-04 Cross-variant copy_file_range slice landed (plan steps
  cross-variant-scripts and mock-fs-pagebacking). New
  `page_backed::step_copy_file_range(in_pc, in_offset, out_pc, out_offset,
  len, guard)` lives in a new sibling module
  `page_backed/cross_variant.rs`. Page-by-page copy via `materialize_page`
  on each side and the substrate `FrameKernelAddr` hook for direct-map
  byte movement. Output Device rejects `EINVAL`; out offset+len beyond
  capacity rejects `EINVAL`; in offset at or past source EOF returns
  `Done(0)`; copy clamps to `in_pc.size_bytes() - in_offset`;
  `pc.size_bytes` is bumped on dst only after byte progress; dirty marking
  handled by `materialize_page(Write)` for Anon/File output. Six tests in
  `cross_variant_tests.rs` cover within-page copy, page-boundary-crossing
  at different alignments, source-EOF clamping, source-at-EOF returns
  `Done(0)`, Device-destination `EINVAL`, and dst-capacity-overflow
  `EINVAL`. Splice (§9.1) and sendfile (§9.2) are explicitly deferred (no
  Pipe StructBacked yet); reflink path is deferred to the reflink slice.
  `mock-fs-pagebacking` plan step is closed-as-redundant: existing
  RecordingFs / BlockingFs / LifecycleFs cover File-variant
  `materialize_page` end-to-end. Verification: `cargo fmt --check` clean,
  `cargo test -p tx-kernel page_backed -- --test-threads=1` (43 ok),
  `cargo test -p tx-kernel vm -- --test-threads=1` (60 ok), `cargo test -p
  tx-kernel --lib -- --test-threads=1` (109 ok), workspace clippy clean,
  `cargo xtask lint arch/unused/docs` ok, `cargo xtask progress validate`
  24 ok.
- 2026-05-04 Partial-page byte fidelity slice landed (plan step
  partial-page-byte-fidelity). `step_truncate` now zeroes the cached
  partial-EOF page tail (bytes `[new_size mod PAGE, PAGE_END)`) after
  withdrawing higher pages, so a subsequent truncate-grow exposes zeros for
  the previously-stale region. The new `zero_partial_eof_tail` helper uses
  the substrate `FrameKernelAddr` hook and is no-op when `new_size` is
  page-aligned, when the EOF page is not cached, or when the hook is
  missing. Three tests added: shrink-past-mid-page zeros the tail and
  preserves the head, page-aligned shrink does not touch the surviving
  page, and end-to-end shrink-then-grow round-trip via `step_read_to_user`
  reads zeros for the post-EOF region. Verification: `cargo fmt --check`
  clean, `cargo test -p tx-kernel page_backed -- --test-threads=1`
  (37 ok), `cargo test -p tx-kernel vm -- --test-threads=1` (60 ok),
  `cargo test -p tx-kernel --lib -- --test-threads=1` (103 ok), workspace
  clippy clean, `cargo xtask lint arch/unused/docs` ok, `cargo xtask
  progress validate` 24 ok.
- 2026-05-04 PageBacked fallocate slice landed (plan step
  pagebacked-fallocate). `FsPageBacking` gained a default-impl
  `fallocate(fs_object_id, new_size, guard)` so existing backings keep
  compiling. `page_backed::step_fallocate` rejects Device with `EINVAL`,
  rejects `new_size` beyond fixed `page_count` capacity with `EINVAL`,
  treats `new_size <= pc.size_bytes()` as `Done(())` no-op, calls
  `FsPageBacking::fallocate` first for File backings and only publishes
  `pc.size_bytes` on backing success, and bumps `pc.size_bytes` for Anon
  backings without materializing pages. Five new tests in
  `page_backed/lifecycle_tests.rs`; `LifecycleFs` extended with
  `fallocates`/`last_fallocate_size` counters and a `failing_fallocate`
  constructor. Verification: `cargo fmt --check` clean, `cargo test -p
  tx-kernel page_backed -- --test-threads=1` (34 ok), `cargo test -p
  tx-kernel vm -- --test-threads=1` (60 ok), `cargo test -p tx-kernel --lib
  -- --test-threads=1` (100 ok), workspace clippy clean, `cargo xtask lint
  arch/unused/docs` ok, `cargo xtask progress validate` 24 ok. The
  `mock-fs-pagebacking` dependency was retired in this slice: existing
  `LifecycleFs` was sufficient.
- 2026-05-04 madvise / msync / mincore observation surface landed (plan
  step madvise-msync-mincore). `AddressSpace::mincore(range)` returns
  range-page-count booleans against the new `VmPmap::walk_range`;
  `AddressSpace::madvise(range, MadviseAdvice)` is no-op per VM §9.7 with the
  documented enum so callers and future syscall wrappers can compile against
  the spelling; `AddressSpace::msync(range, guard)` iterates recipes
  overlapping `range`, deduplicates File-backed `PageContainer`s by Cap key,
  and calls `page_backed::step_fsync` per unique PC. Anon/PrivateAnon/Device
  backings are no-op for `msync`. Eight new tests in
  `vm/tests/observation.rs`; `vm/tests.rs` split to keep the parent under
  the 1500-line guard. Verification: `cargo test -p tx-kernel vm --
  --test-threads=1` (60 ok), `cargo test -p tx-kernel page_backed --
  --test-threads=1` (29 ok), `cargo test -p tx-kernel --lib --
  --test-threads=1` (95 ok), `cargo fmt --check` clean, workspace clippy
  clean, `cargo xtask lint arch/unused/docs` ok, `cargo xtask progress
  validate` 24 ok.
- 2026-05-04 VmPmap walk surface and wait-aware StepOutcome audit landed
  (plan steps vm-pmap-walk-protect-surface and wait-aware-step-outcome).
  `VmPmap::walk_range(range)` returns ascending-order `(UserPage,
  PmapMappingSnapshot)` tuples for mincore-style enumeration and for future
  fork CoW demotion to discover affected pages. `teardown_range` rustdoc now
  documents its dual role as the protect-via-refault path per VM_v1_2 §9.8
  (in-place PTE permission patching deferred). Three new vm tests cover
  ascending order, range exclusion, and empty results. Wait-aware
  `StepOutcome` audit confirms the existing five-variant algebra and
  `WaitToken(carrier, interest)` shape already match STEP_MODEL_v1 §2/§2.3 —
  no code change needed; downstream script wrappers can call the existing
  variants directly. Verification: `cargo test -p tx-kernel vm --
  --test-threads=1` (55 ok), `cargo test -p tx-kernel page_backed --
  --test-threads=1` (29 ok), `cargo test -p tx-kernel --lib --
  --test-threads=1` (90 ok), `cargo fmt --check` clean, workspace clippy
  clean, `cargo xtask lint arch/unused/docs` ok, `cargo xtask progress
  validate` 24 ok.
- 2026-05-04 VM fault PC.size SIGBUS check slice landed (plan step
  vm-fault-pc-size-checks). `VmFaultError` gained `PageBeyondSize`.
  `VmFaultOutcome::materialize_page_recipe` rejects faults whose
  `page_index * USER_PAGE_SIZE >= pc.size_bytes()` before calling
  `materialize_anon`, leaving `BackingOffsetOverflow` for capacity violations.
  Three new tests in `vm/tests/fault_materialization.rs` cover SHARED past-EOF
  read rejection, MAP_PRIVATE past-EOF write rejection (before CoW
  replacement), and admission of a page whose first byte is just below
  `PC.size`. Verification: `cargo test -p tx-kernel vm -- --test-threads=1`
  (52 ok), `cargo test -p tx-kernel page_backed -- --test-threads=1` (29 ok),
  `cargo test -p tx-kernel --lib -- --test-threads=1` (87 ok), `cargo fmt
  --check` clean, workspace clippy clean, `cargo xtask lint arch/unused/docs`
  ok, `cargo xtask progress validate` 24 ok. Closes the prior STATUS "next
  step: connect PC.size to VM fault SIGBUS-style checks for page-backed
  mappings" item.
- 2026-05-04 PageBacked user-buffer byte copy slice landed (plan step
  user-buffer-byte-copy). Substrate gained a `FrameKernelAddr` hook installed
  at boot (direct-map) and in host tests (test direct map). `Errno` gained
  `EFAULT`. PageBacked now exposes `step_read_to_user<H: UserAccessIf>` and
  `step_write_from_user<H: UserAccessIf>` in a new sibling module
  `page_backed/user_buffer.rs`; copyless `step_read`/`step_write` remain as
  the in-kernel staging surface. Four host tests cover single-page round trip,
  cross-page round trip, and EFAULT propagation in both directions.
  Verification: `cargo fmt --check`, `cargo test -p tx-kernel page_backed --
  --test-threads=1` (29 ok), `cargo test -p tx-kernel vm -- --test-threads=1`
  (49 ok), `cargo test -p tx-kernel --lib -- --test-threads=1` (84 ok),
  `cargo clippy --workspace --all-targets ...` clean, `cargo xtask lint arch`
  ok, `cargo xtask lint unused` ok, `cargo xtask lint docs` ok,
  `cargo xtask progress validate` 24 ok.
- 2026-05-04 VM/PageBacked v1 completion plan activated. Active roadmap is
  `docs/progress/plans/2026-05-04-vm-pagebacked-v1-completion.json` (17 steps),
  bridging VM/PageBacked from ~45% structure / ~30% behavior toward ~85% on
  both, leaving only items the active design docs explicitly defer or items
  that depend on Process/ThreadRuntime ownership. Prior worktree
  `2026-05-02-vm-pagebacked-impl` closed as merged (PR #14 on main); follow-on
  work continues on this branch.
- 2026-05-04 Claude harness parallel and main resync. `CLAUDE.md` symlinked to
  `AGENTS.md` and `.claude/settings.json` SessionStart hook wired to inject
  `AGENTS.md` as additionalContext at session start; misleading
  `.claude/skills`/`.claude/commands` symlinks dropped after probes confirmed
  the harness does not scan them. Branch resynced onto `origin/main` (HAL +
  useraccessif + irqif work) by `git reset --hard origin/main` then
  `git cherry-pick origin/main..backup/pre-main-resync-2026-05-04`; all ten
  PageBacked/VM commits replayed clean with zero conflicts. Verification:
  `cargo xtask progress validate` 23 ok, `cargo check --workspace
  --all-targets --exclude tx-kernel-riscv64-qemu-virt --exclude
  tx-kernel-riscv64-m1dock-mock --exclude tx-kernel-loongarch64-qemu-virt`
  green, `cargo test -p tx-kernel vm -- --test-threads=1` 49 ok, `cargo test
  -p tx-kernel page_backed -- --test-threads=1` 25 ok. Decision note:
  `docs/progress/decisions/2026-05-04-claude-harness-parallel-and-main-resync.md`.
  Backup ref `backup/pre-main-resync-2026-05-04` retains pre-resync history.
  Next step: connect `PC.size` to VM fault SIGBUS-style checks for page-backed
  mappings and add byte-accurate user-buffer read/write once copyin/copyout
  gates exist; subagents still need txKernel rules pasted into spawn prompts
  because no harness-level pass-through exists. Blockers: async fault-script
  retry/yield behavior, Process/ThreadRuntime/trap authority wiring, concrete
  VFS/backend implementations, final user-buffer copy plumbing.
- 2026-05-04 VFS/ext4 CI fix landed after GitHub `check` failed on
  `merge vfs work`. The fix boxes large VFS resolution/read-boundary enum
  payloads, keeps VFS cold-read test-support code behind real cfg boundaries
  instead of dead-code allowances, splits the oversized
  `vfs/execution/tests.rs` into responsibility modules, and serializes the
  ext4 kernel-read backend tests so host epoch guards are not nested by
  parallel tests. It also updates the ext4 page offset check for the current
  nightly clippy lint. Verification: `cargo test -p tx-subsystems
  vfs::execution::tests -- --test-threads=1`, `cargo test -p tx-ext4 --test
  kernel_read_backend`, `cargo test -p tx-ext4 --test vfs_full_read`, `cargo
  xtask lint unused`, `cargo xtask lint arch`, and `cargo xtask ci` with 10
  passed, 1 skipped la64 target, 0 failed. Next step: commit and push this CI
  repair so GitHub Actions reruns green; no blocker.
- 2026-05-04 TTY progress memory now has a durable status note at
  `docs/progress/research/2026-05-04-tty-implementation-status.md`, and
  `.agents/skills/tx-tty-subsystem/SKILL.md` now points future work at the
  canonical TTY docs, current code map, and known staging seams. This records
  that the tty-only implementation slice is present under
  `crates/tx-kernel/src/tty/`, while final Process/Signal/VFS convergence still
  needs follow-up replacements for staged session/pgrp ids, signal delivery
  wiring, termios publication shape, and full controlling-tty lifecycle hooks.
  Verification for the code slice referenced by the note had already covered
  `cargo test -p tx-kernel tty -- --test-threads=1`, `cargo test -p tx-kernel
  --lib -- --test-threads=1`, `cargo fmt --check`, `git diff --check`, and the
  user's RV64 boot smoke. Next step: when Process, Signal, or VFS work reaches
  tty integration, start from the new skill and progress note before widening
  TTY changes; no blocker.
- 2026-05-03 PageBacked dynamic `PC.size` slice added a visible byte-size
  field to `PageContainer` while preserving the existing fixed `page_count`
  capacity as the upper bound. `PageContainer::size_bytes()` is now the compact
  observation helper; `step_read` clamps EOF to visible size rather than
  capacity; `step_write` rejects growth beyond capacity but grows visible size
  after byte progress for Anon/File; and `step_truncate` publishes the new size
  only after File `FsPageBacking::truncate` succeeds, withdrawing cached pages
  on shrink and materializing nothing on grow. Size-focused tests moved into
  `crates/tx-kernel/src/page_backed/size_tests.rs` so the PageBacked facade
  stays under the 1500-line architecture guard. Verification: initial red
  compile check for missing `size_bytes`, then `cargo test -p tx-kernel
  pagebacked_step_write_extends_visible_size_within_capacity --
  --test-threads=1`, `cargo test -p tx-kernel
  page_container_size_starts_at_fixed_capacity -- --test-threads=1`, `cargo
  test -p tx-kernel pagebacked_step_read_uses_visible_size_not_capacity --
  --test-threads=1`, `cargo test -p tx-kernel pagebacked_step_truncate --
  --test-threads=1`, `cargo test -p tx-kernel page_backed --
  --test-threads=1`; regression gates with `cargo fmt --check`, `cargo test -p
  tx-kernel vm -- --test-threads=1`, `cargo test -p tx-kernel --lib`, `cargo
  clippy --workspace --all-targets --exclude tx-kernel-riscv64-qemu-virt
  --exclude tx-kernel-riscv64-m1dock-mock --exclude
  tx-kernel-loongarch64-qemu-virt -- -D warnings`, `cargo xtask lint unused`,
  `cargo xtask lint arch`, `cargo xtask progress validate`, `cargo xtask lint
  docs`, `git diff --check`; and `cargo xtask ci` with 11 passed, 0 skipped, 0
  failed. Next step: connect `PC.size` to VM fault SIGBUS-style checks for
  page-backed mappings and then add byte-accurate user-buffer read/write once
  copyin/copyout gates exist. Blockers remain async fault-script retry/yield
  behavior, Process/ThreadRuntime/trap authority wiring, concrete VFS/backend
  implementations, and final user-buffer copy plumbing.
- 2026-05-03 source-frame byte-copy slice added a substrate-owned
  `FrameCopier` hook, `install_frame_copier`, and
  `page_allocator::copy_frame_contents(source, dest)` as the direct-map
  full-frame copy primitive for VM CoW and future PageBacked byte movement. Boot
  now installs the hook beside the existing direct-map zeroer, host tests use
  the test direct-map backing for byte-level assertions, and repeated
  `claim_zero_frame` calls no longer leak extra permanent frames after the zero
  frame is already installed. MAP_PRIVATE PageBacked write faults now
  materialize the shared source page and copy its bytes into the private frame
  before publishing the writable replacement; the source `PageContainer` page
  remains cached and unchanged. Full user-buffer `step_read`/`step_write`
  byte copying is still deferred because `UserAccessIf`/copyin-copyout is not
  wired. Parallel `tx-kernel --lib` also exposed that mount tests allocate
  zone-backed payloads on the shared host pseudo-CPU, so they now share the
  crate-level test serializer with PageBacked/Device epoch tests. Focused
  verification so far: red compile checks for missing copy/test helpers, then
  `cargo test -p tx-substrate --test page_allocator
  installed_frame_copy_hook_copies_test_direct_map_bytes`, `cargo test -p
  tx-kernel vm_fault_map_private_write_copies_source_page_contents --
  --test-threads=1`, `cargo test -p tx-substrate --test page_allocator`, `cargo
  test -p tx-kernel vm -- --test-threads=1`, `cargo test -p tx-kernel --lib`,
  `cargo test -p tx-substrate --lib`, and `cargo clippy --workspace
  --all-targets --exclude tx-kernel-riscv64-qemu-virt --exclude
  tx-kernel-riscv64-m1dock-mock --exclude tx-kernel-loongarch64-qemu-virt --
  -D warnings`, followed by `cargo xtask lint unused`, `cargo xtask lint arch`,
  `cargo xtask progress validate`, `cargo xtask lint docs`, `git diff --check`,
  and `cargo xtask ci` with 11 passed, 0 skipped, 0 failed. Next step: dynamic
  `PC.size` growth/truncate semantics and byte-accurate PageBacked range I/O
  once user-buffer copy gates exist.
  Blockers remain async fault-script retry/yield behavior,
  Process/ThreadRuntime/trap authority wiring, and concrete VFS/backend
  implementations.
- 2026-05-03 PageBacked lifecycle-script slice added
  `page_backed::step_truncate` and `page_backed::step_fsync` in a new
  `crates/tx-kernel/src/page_backed/` submodule so the main PageBacked file
  stays below the 1500-line guard. `step_truncate` rejects Device backing,
  asks File `FsPageBacking::truncate` before mutating cache state, and withdraws
  cached pages at or beyond the new staged size boundary. Because
  `PageContainer` still stores fixed `page_count` capacity rather than final
  dynamic `PC.size`, truncate-up beyond current capacity remains `EINVAL` and
  read/write EOF still uses page capacity. `step_fsync` is a no-op for Anon and
  Device, flushes dirty File pages through `FsPageBacking::flush_page` in
  deterministic page-index order, clears dirty marks after successful flushes,
  propagates waits with `AdvancedThenBlocked` after flush progress, then calls
  filesystem `fsync` for metadata. Focused verification so far: red compile
  check for missing lifecycle surface, then `cargo fmt --check`, `cargo test -p
  tx-kernel pagebacked_step_truncate -- --test-threads=1`, `cargo test -p
  tx-kernel pagebacked_step_fsync -- --test-threads=1`, and `cargo test -p
  tx-kernel page_backed -- --test-threads=1`. Full lib verification initially
  exposed that Device and PageBacked tests shared the host epoch guard without a
  common serializer; the slice added a crate-level test-only `EPOCH_TEST_LOCK`
  and reran `cargo test -p tx-kernel --lib` with 73 passed, 0 failed, plus
  `cargo test -p tx-kernel vm -- --test-threads=1` with 48 passed. Full
  verification completed with `cargo clippy --workspace --all-targets --exclude
  tx-kernel-riscv64-qemu-virt --exclude tx-kernel-riscv64-m1dock-mock
  --exclude tx-kernel-loongarch64-qemu-virt -- -D warnings`, `cargo xtask lint
  unused`, `cargo xtask lint arch`, `cargo xtask progress validate`, `cargo
  xtask lint docs`, and `cargo xtask ci` with 11 passed, 0 skipped, 0 failed.
  Next step: source-frame /
  direct-map byte-copy helper for full CoW and real read/write contents, then
  dynamic `PC.size` growth/truncate semantics. Blockers remain byte-copy
  fidelity, async fault-script retry/yield behavior, Process/ThreadRuntime/trap
  authority wiring, and concrete VFS/backend implementations.
- 2026-05-03 PageBacked range-script slice added copyless staged
  `page_backed::step_read` and `page_backed::step_write` helpers over
  `PageContainer::materialize_page` plus an `OpenFile::set_offset` compatibility
  hook. The scripts materialize page ranges, advance offsets only after
  progress, return EOF at the current page-capacity boundary, propagate
  `Blocked` / `AdvancedThenBlocked` for file fetch waits, mark written pages
  dirty for Anon/File, and reject Device writes with `EINVAL`. Actual byte
  movement through direct-map/user-buffer helpers, dynamic `PC.size` growth,
  truncate, fsync, writeback, and withdrawal remain deferred. Focused
  verification so far: red check for missing `set_offset` / `step_read` /
  `step_write`, then `cargo test -p tx-kernel pagebacked_step_ --
  --test-threads=1`, `cargo test -p tx-kernel page_backed --
  --test-threads=1`, `cargo test -p tx-kernel --lib`, `cargo clippy
  --workspace --all-targets --exclude tx-kernel-riscv64-qemu-virt --exclude
  tx-kernel-riscv64-m1dock-mock --exclude tx-kernel-loongarch64-qemu-virt --
  -D warnings`, `cargo xtask lint unused`, `cargo xtask lint arch`, `cargo
  xtask progress validate`, `cargo xtask lint docs`, `git diff --check`, and
  `cargo xtask ci` with 11 passed, 0 skipped, 0 failed. Next step:
  `step_truncate` / `step_fsync` with mock backends, then real byte-copy
  helpers. Blockers remain source-frame/direct-map byte-copy fidelity, dynamic
  size/truncate semantics, async fault-script retry/yield behavior,
  Process/ThreadRuntime/trap authority wiring, and concrete VFS/backend
  implementations.
- 2026-05-03 PageBacked v1 core materialization slice added
  `PageContainer::materialize_page` as the uniform PageBacked dispatcher over
  Anon, File, and Device variants. Anon keeps the existing zeroed-frame
  behavior, File calls the mounted `FsPageBacking::fetch_page` and propagates
  blocked/errored `StepOutcome` results, and Device wraps stable PPNs without
  allocator ownership. `MaterializedPage` now carries allocator-backed or
  device-backed publication evidence, and VM pmap tracking can retain either
  kind while preserving allocator shootdown release for normal RAM pages.
  Focused verification so far: red check for the missing
  `materialize_page`, then `cargo test -p tx-kernel
  page_container_materialize_page -- --test-threads=1`, `cargo test -p
  tx-kernel page_backed -- --test-threads=1`, `cargo test -p tx-kernel vm
  -- --test-threads=1`, `cargo test -p tx-kernel --lib`, `cargo clippy
  --workspace --all-targets --exclude tx-kernel-riscv64-qemu-virt --exclude
  tx-kernel-riscv64-m1dock-mock --exclude tx-kernel-loongarch64-qemu-virt --
  -D warnings`, `cargo xtask lint unused`, `cargo xtask progress validate`,
  `cargo xtask lint docs`, `git diff --check`, and `cargo xtask ci` with 11
  passed, 0 skipped, 0 failed. Next step: minimal PageBacked read/write scripts
  with mock backends. Blockers remain source-frame byte-copy fidelity for full
  CoW, async fault-script retry/yield behavior, Process/ThreadRuntime/trap
  authority wiring, and concrete VFS/backend implementations.
- 2026-05-03 local CI catch-up for PR #15 split VM fault
  materialization tests out of `crates/tx-kernel/src/vm/tests.rs` into
  `crates/tx-kernel/src/vm/tests/fault_materialization.rs` after GitHub
  Actions reported `cargo xtask lint arch` failing on the 1500-line authored
  Rust guardrail. The parent VM test harness is now 1340 lines and the new
  focused fault-materialization test module is 267 lines. Verification so far:
  `cargo fmt --check`, `cargo xtask lint arch`, and `cargo test -p tx-kernel
  vm -- --test-threads=1`, plus `cargo xtask ci` with 11 passed, 0 skipped, 0
  failed. This fix is intentionally local-only until the next requested push.
- 2026-05-03 VM generalized fault-materialization slice added the
  `VmFaultOutcome::materialize_pagebacked` path and kept
  `materialize_pagebacked_anon` as a compatibility wrapper. PrivateAnon read
  faults now materialize the permanent zero frame read-only; PrivateAnon write
  faults allocate fresh zeroed private frames and replace an existing zero-frame
  PTE when present. MAP_PRIVATE PageBacked read faults install the shared source
  page read-only, and write faults allocate a private frame and replace the
  read-only mapping without inserting the private frame into the source
  `PageContainer`. `VmPmap` now has replacement publication for these staged
  CoW faults. Tests added zero-frame read, PrivateAnon write replacement, and
  MAP_PRIVATE read/write CoW coverage. Verification: `cargo fmt --check`,
  `cargo test -p tx-kernel vm -- --test-threads=1`, `cargo test -p tx-kernel
  --lib`, `cargo clippy --workspace --all-targets --exclude
  tx-kernel-riscv64-qemu-virt --exclude tx-kernel-riscv64-m1dock-mock
  --exclude tx-kernel-loongarch64-qemu-virt -- -D warnings`, and `cargo xtask
  lint unused`, `cargo xtask progress validate`, `cargo xtask lint docs`, and
  `git diff --check`.
  Next step: PageBacked v1 core `PageContainer::materialize_page` with mock
  File/Device dispatch. Blockers remain byte-copying source frame contents for
  full CoW fidelity, async fault-script retry/yield behavior,
  Process/ThreadRuntime/trap authority wiring, and concrete VFS/backend
  implementations.
- 2026-05-03 VM recipe snapshot slice replaced the recipe index's mutable
  in-place map publication with owned whole-tree `RecipeSnapshot` clones.
  Public helpers such as `lookup`, `recipes_overlapping`, `recipes_snapshot`,
  map/unmap/protect, fixed replace, and disjoint remap keep their existing
  behavior, while readers can now hold an owned pre-mutation recipe view
  across split/rewrite publication. Tests added
  `vm_recipe_snapshot_reader_survives_split_rewrite_publication`. Verification:
  red check for the new test, then `cargo fmt --check`, `cargo test -p
  tx-kernel vm -- --test-threads=1`, `cargo test -p tx-kernel --lib`,
  `cargo clippy --workspace --all-targets --exclude
  tx-kernel-riscv64-qemu-virt --exclude tx-kernel-riscv64-m1dock-mock
  --exclude tx-kernel-loongarch64-qemu-virt -- -D warnings`, `cargo xtask
  lint unused`, `cargo xtask progress validate`, `cargo xtask lint docs`, and
  `git diff --check`. Next step: generalize fault
  materialization for PrivateAnon zero-frame reads, private writes, and
  MAP_PRIVATE CoW. Blockers remain final epoch/guard-shaped recipe witnesses,
  Process/ThreadRuntime/trap authority wiring, and concrete VFS/backend
  implementations.
- 2026-05-03 VM doc gap ledger recorded the current
  `codex/vm-pagebacked-impl` delta against `VM_v1_2` and `PAGE_BACKED_v1` in
  `docs/progress/research/2026-05-03-vm-doc-gap-ledger.md`. The ledger
  classifies obligations as implemented, staged, blocked/not implemented, or
  deferred by active docs, and fixes the next mitigation order: snapshot-stable
  recipes, generalized fault materialization and CoW, PageBacked core with mock
  File/Device backing, syscall-script surfaces, then Process/ThreadRuntime/trap
  integration. Verification: `cargo fmt --check`, `cargo xtask progress
  validate`, `cargo xtask lint docs`, and `git diff --check`. Next step:
  start the recipe snapshot slice while preserving current helper names and
  error behavior. Blockers
  remain Process/ThreadRuntime/trap authority wiring and concrete VFS/backend
  implementations.
- 2026-05-03 PR #14 CI check fix cleared the GitHub `check` failures after
  inspecting Actions logs. The patch removes clippy warnings from the
  VM/PageBacked/VFS interface lane by eliding needless guard lifetimes,
  shrinking `RNodeBacking::Symlink` through boxed inline names, factoring VM
  pmap operation function-pointer types, collapsing a RangeLock predicate, and
  cloning recipe overlap rows only after filtering. It also removes the
  forbidden pmap dead-code allowance by dropping the unused staged rollback
  op slot, and splits VM execution-script tests into
  `vm/tests/execution_scripts.rs` so `vm/tests.rs` stays below the 1,500-line
  arch-lint cap. Verification: `cargo clippy --workspace --all-targets
  --exclude tx-kernel-riscv64-qemu-virt --exclude
  tx-kernel-riscv64-m1dock-mock --exclude tx-kernel-loongarch64-qemu-virt --
  -D warnings`, `cargo xtask lint arch`, and `cargo xtask ci` with 11 passed,
  0 skipped, 0 failed. Next step: push and let PR #14's GitHub check rerun; no
  blocker.
- 2026-05-03 PR #14 unused-lint fix kept the PageBacked production surface
  thin by gating the private `PageCacheIndex::install_if_match` replacement /
  withdrawal helper to tests. The helper was only exercised by unit tests, so a
  normal test build hid the warning while `RUSTFLAGS=-Dunused cargo check -p
  tx-kernel` and GitHub's lint path rejected the non-test library build.
  Verification: `cargo fmt --check`, `RUSTFLAGS=-Dunused cargo check -p
  tx-kernel`, `cargo test -p tx-kernel --lib`, `cargo xtask progress
  validate`, `cargo xtask lint unused`, `cargo xtask lint docs`, and `git diff
  --check`. Next step: reintroduce production replacement / withdrawal only
  when a real file-backed truncation, writeback, or eviction path consumes it;
  no blocker.
- 2026-05-03 PR #14 conflict resolution merged remote `origin/main` into
  `codex/vm-pagebacked-impl`. Resolution kept the base branch's current
  reactor/trap/core progress notes, kept the VM/PageBacked branch's kernel
  module exports and epoch test bootstrap hook, and restored the VM/VFS
  interface catch-up below. Verification: `cargo fmt --check`, `cargo test -p
  tx-kernel vm -- --test-threads=1`, `cargo test -p tx-kernel --lib`,
  `cargo test -p tx-substrate epoch`, `cargo xtask progress validate`, `cargo
  xtask lint docs`, `git diff --check`, and an anchored conflict-marker scan.
  The merge resolution was pushed; GitHub now reports PR #14 as `UNSTABLE`
  while the `check` workflow runs, instead of the prior `DIRTY` conflict state.
  Next step: wait for CI to finish and address any check failure if it appears.
- 2026-05-02 VM checks/projections completion slice has landed on top of the
  subsystem-anatomy reorg. `vm::checks` now exposes staged observation helpers
  for fault recipe admission, fault-publication revalidation, map admission,
  and disjoint-remap shape checks; `execution.rs` still owns RangeLock
  acquisition, recipe mutation, and pmap publication. `vm::project` now exposes
  read-only `AddressSpaceProjection` / `VmMappingProjection` rows backed by a
  deterministic recipe snapshot, with page-backed mappings reduced to
  non-authoritative offsets rather than leaking `Cap<PageContainer>`.
  Verification before PR publication: `cargo fmt --check`, `cargo test -p
  tx-kernel vm -- --test-threads=1`, `cargo test -p tx-kernel --lib`,
  `cargo xtask progress validate`, `cargo xtask lint docs`, and `git diff
  --check`. Next step: have future syscall/trap-facing VM scripts consume
  these check and projection surfaces instead of direct helper calls. Blockers
  remain persistent/epoch recipe snapshots and trap/process/runtime
  integration.
- 2026-05-02 VFS/Mount/PageBacked interface seam landed from the
  `codex/vfs-interface-scout` readiness note. `tx-kernel` now exposes shared
  interface shells for `Errno` / `StepOutcome`, device and block handles,
  VFS live-node names (`DEntry`, `RNode`, `RNodeBacking`, `OpenFile`,
  `ResolveCtx` / `RootCtx`, witnesses), mount names (`MountIdentity`,
  `MountPayload`, `MountNamespace`, `MountPayloadPin`, `MountInitContext`,
  `MetadataPcFactory`, `MountOutput`), and the backend traits
  `FsOps` / `FsPageBacking`. `PageContainerKind::File` now carries the
  canonical `Cap<MountPayload>` plus `FsObjectId` boundary so future VFS,
  PageBacked, bdev-fs, devfs, and kernel-facing ext4 lanes do not invent local
  spellings. Next step: build real VFS/PageBacked file-device behavior on
  these shells; blockers remain trap/process/runtime integration and concrete
  backend implementations.
- 2026-05-02 VM/PageBacked implementation lane now lives on
  `codex/vm-pagebacked-impl`. It brought in the corrected AddressSpace
  range-index core, mmap-style gap placement, v1 disjoint-only `mremap`, and
  fault resolution over authoritative recipes. The lane now also has
  zone-backed `Cap<AddressSpace>` and `Cap<PageContainer>` constructors,
  registered VM/PageBacked zones, recipes carrying `VmBacking::Page { pc:
  Cap<PageContainer>, offset }`, PageBacked-owned `PageCacheIndex` entries
  backed by real PPN plus `CachePin`, fault materialization that returns
  `MapPin` evidence for pmap publication, and a VM-owned `VmPmap` over HAL
  `PmapIf` roots. Next step: connect VFS `FsPageBacking` and later trap /
  Process / ThreadRuntime fault dispatch; blockers remain persistent epoch
  recipe snapshots, trap/process/runtime integration, and file/device backing.
- Rust workspace skeleton exists with `cargo xtask` as the developer command
  surface.
- QEMU RV64, LA64, and RV64 M1 Dock mock target wiring exists as compile-first
  stubs.
- OSComp autotest is present as the `external/oscomp-autotest` submodule.
- BusyBox cpio, BusyBox ext4, and M1 Dock SD-image builder contracts exist.
- Clean K210 submit-tree generation is available through `cargo xtask submit k210`.
- Active design docs are collected under `docs/design/`.
- Imported EBR/Zone mechanics references are collected under `docs/ebr-zone/`.
- Durable progress memory lives under `docs/progress/`.
- Plans, handoffs, and worktree records use schema-tagged JSON for
  agent-friendly queries.
- `cargo xtask progress` can validate, list, create, claim, and close
  operational JSON records.
- `xtask` is split by command family under `xtask/src/`, with a local module
  map in `xtask/README.md`.
- `cargo xtask fault-decode` now exists as a host-side RV64 trap/address
  decoder. It parses `scause`/`sepc`/`stval` logs, detects low-linked versus
  high-VMA ELF layouts, classifies direct-map and firmware-gap addresses,
  symbolizes through Rust-native ELF/DWARF readers, and conservatively reports
  data code-pointer candidates without changing the kernel trap path.
  `AGENTS.md` and the HAL/trap skill now point future debugging sessions at
  this command before manual `nm`/`addr2line` work.
  Verification: `cargo fmt --check`, `cargo test -p xtask`,
  `cargo xtask build --target rv64-qemu`, and manual `fault-decode --addr` /
  `fault-decode --serial` smoke runs in the
  `codex/fault-decode-tool-impl` worktree. Next step: wire QEMU failure
  auto-annotation later if desired; no blocker. Post-merge high-VMA smoke
  coverage also fixed high-kernel alias classification and added regression
  coverage so those addresses are not reported as direct-map addresses.
- HumanLayer `.claude` workflow references are available as a sparse submodule
  at `external/humanlayer-reference`.
- `cargo xtask ci` provides concise CI reporting with `txdoc:` references into
  the active design docs.
- Active design docs now carry fine-grained `txdoc:` anchors; docs lint rejects
  top-only anchoring.
- RV64 QEMU now has an ArceOS-style platform-owned boot path: linker script,
  `_start`, BSS clearing, SBI console output, typed `BootHandoff`, and the
  smoke sentinel `txkernel:qemu-riscv64-virt:boot:ok`.
- RV64 QEMU publishes BootInfo v1 from the OpenSBI-provided DTB: usable memory
  regions, kernel image linker bounds, chosen bootargs, and initrd bounds.
- RV64 QEMU DTB parsing now delegates flattened-devicetree traversal to the
  `fdt` crate while keeping board-owned BootInfo normalization for memory
  regions, chosen bootargs, and Linux initrd bounds; verification covered host
  tests, RV64 no-std check/build, arch/docs/progress lints, and the RV64 QEMU
  smoke sentinel, with no parser blocker and the next step still the
  page-substrate boot handoff.
- RV64 QEMU now uses a high-VMA/low-LMA linker layout. Firmware enters only the
  low `.text.trampoline` at `0x8020_0000`; that assembly uses suffixed `_load`
  symbols to clear BSS, build identity/direct-map/high-kernel page tables,
  enable Sv39, rewrite `sp`/`gp`, and jump to high `rust_entry`. High Rust then
  captures boot statics, publishes pmap facts, proves high PC/SP/GP, drops the
  low identity leaf, and still reserves `[0x8000_0000, 0x8020_0000)` so
  allocator metadata is not carved over OpenSBI/kernel-loader RAM. Verification:
  `cargo test -p tx-hal-riscv64-qemu-virt`, ELF layout inspection, and
  `cargo xtask qemu --target rv64-qemu --profile smoke --expect-sentinel`.
- RV64 QEMU centralizes Rust boot-static/linker-symbol address capture in a
  single `BootStaticBag` authority; high Rust constructs
  `BootStaticBag<IdentityLive>` once with the firmware DTB and boot/static
  facts, then the post-entry pipeline consumes it into the post-entry bag
  typestate after identity teardown. BootInfo, PlatformInfo, bootstrap pmap
  roots, the kernel alias L1, and the PT-node pool flow through named bag
  accessors. `cargo xtask lint arch` enforces that other board files do not
  recreate static address facts.
- RV64 QEMU pmap host tests now avoid manufacturing direct-map aliases from
  host static pointers. `BootStaticBag::pt_node_direct_va()` is target-only, and
  the boot PT-node pool test checks pool bookkeeping instead of adding the high
  direct-map base to a host pointer. Verification: `cargo test -p
  tx-hal-riscv64-qemu-virt`; no blocker.
- `cargo xtask lint unused` now runs Rust unused/dead-code checks as hard
  errors for the host workspace and installed board targets, and `lint arch`
  rejects `#[allow(dead_code)]` / `#[allow(unused...)]` escape hatches in
  normal code. The RV64 high sentinel remains live target code because it is the
  final proof before substrate relies on the high alias; explicit identity
  teardown stays test-only until the high-linker/relocation slice.
- `tx-substrate` now has the v1 typed page allocator interface:
  `PageAllocator`, `BitmapPageAllocator`, `FrameMeta`, reservation/owned-frame
  tokens, role pins, permanent/device/page-table frame classes, installed
  bitmap-backend delegation, allocator interface tests, rustdoc covering map
  topology/function usage, no-alloc run splitting, and module files grouped by
  state-machine topic.
- `tx_substrate::init::<P>()` now performs the first real page-substrate boot
  handoff: it normalizes HAL `BootInfo` memory regions, consumes
  `BootstrapPmapInfo.reserved_page_tables`, carves direct-mapped `FrameMeta[]`
  and bitmap storage, installs a dense `base_ppn` bitmap allocator, and wires
  `ZeroPolicy::Zeroed` to the direct-map scrubber. Verification:
  `cargo fmt --check`, `cargo test -p tx-substrate`, `cargo test -p
  tx-hal-riscv64-qemu-virt`, `cargo xtask lint unused`, `cargo xtask lint
  docs`, `cargo xtask progress validate`, `cargo xtask ci`, RV64 QEMU smoke
  sentinel, and `git diff --check`.
- RV64 QEMU now exposes the first executable pmap reserve/commit mutation:
  idempotent 1 GiB kernel direct-map leaf reservation/commit plus
  `PmapIf::extend_direct_map()`. `tx_substrate::init::<P>()` calls it before
  allocator metadata placement when `BootInfo` reports RAM beyond the bootstrap
  direct-map window.
- RV64 QEMU now maps platform MMIO during `tx_substrate::init::<P>()` through
  `PlatformInfo.mmio_regions` and `PmapIf::reserve_kernel_mapping()` /
  `commit_kernel_mapping()`, using 2 MiB leaves when aligned and 4 KiB leaves
  for small or tail regions.
- The pmap lifecycle surface now includes abandoned-reservation rollback,
  kernel mapping unmap, and explicit invalidation tokens. RV64 QEMU rollback
  releases `PT_NODE_POOL` intermediates allocated during 2 MiB / 4 KiB
  reservation, and kernel unmap clears 2 MiB / 4 KiB leaves before a local
  shootdown.
- After the frame allocator is installed, `tx_substrate::init::<P>()` now
  installs a typed pmap PT-node source with `PmapIf::install_pt_node_allocator`.
  RV64 QEMU uses typed `PtFrame` pages for new intermediates first and retains
  `PT_NODE_POOL` as the exhaustion fallback.
- Substrate now has a no-alloc `KernelShootdownBatch` for page-sized kernel
  unmaps. It owns the `MapPin` for the cleared mapping, issues
  `P::shootdown_kernel_mapping()` first, and only then drops the pin so
  `map_count` cannot reach zero before invalidation.
- `tx_substrate::init::<P>()` now brings up the first no-std slab heap after the
  frame allocator is installed: small classes up to 2 KiB, page-run backing for
  page-sized and larger allocations, empty slab-page return, a kernel-target
  `GlobalAlloc`, a boot-time allocation smoke, and a permanent zero-frame
  anchor via `OwnedFrame::into_permanent_frame()`. `TrapIf` now exposes
  `install_kernel_trap_vector()` and generic `tx_kernel::kernel_main::<P>()`
  calls it after `P::init_later()`.
- `tx-substrate` now has the first executable EBR/Zone substrate slice:
  `epoch::guard`, per-CPU retired-node slices, bounded drain, `Zone<T>` static
  registration, frame-backed bitmap slabs, compact `Cap<T>` / `Weak<T>` keys,
  `ZoneReservation<T>` reserve/sign publication, `Weak -> IdentRef -> Cap`
  upgrade, and EBR-delayed slot/slab reclamation. `Cap<T>` is 4 bytes and
  `Weak<T>` is 8 bytes by compile-time assertion. RV64 QEMU smoke can run the
  kernel-side zone smoke path and prints `txkernel:zone:smoke:ok` before the
  boot sentinel. Remaining gaps are linker-section auto-registration of all
  static zones, full upper-subsystem zone manifests, SMP stress coverage, and
  the still-pending bus/index/mutation substrate pieces.
- The remote `origin/zone` EBR/Zone branch (`e53956e`) has been audited and
  conflict-resolved on `codex/zone-ebr-integration` against
  `codex/reactor-task-aware`. The merge keeps the newer HAL trap/pmap/TimeIf
  surface, adopts the directory-based EBR/Zone implementation, removes the old
  flat placeholder modules, wires CoreInit to run the kernel zone smoke, and
  updates stale host tests to the new static-zone API. The integration also
  removed imported clippy blockers, repaired the HumanLayer README link target
  for docs lint, and made the reactor smoke test counters per-test so full
  workspace CI is deterministic. Verification: `cargo fmt --check`, `cargo
  test -p tx-substrate`, `cargo test -p tx-hal-riscv64-qemu-virt`, `cargo
  check -p tx-kernel`, `cargo xtask lint unused`, `cargo xtask lint docs`,
  `cargo xtask progress validate`, `cargo xtask ci`, RV64 QEMU smoke sentinel
  with `txkernel:zone:smoke:ok`, and `git diff --check`. Blocker: none found
  in the conflict audit.
- RV64 QEMU now implements safe in-place kernel pmap permission updates through
  `PmapIf::protect_kernel_mapping()`. It rewrites existing same-granularity
  leaves, returns a `PmapInvalidation`, treats absent mappings as no mutation,
  and rejects unsafe split/rematerialization cases for VM to handle later.
- RV64 QEMU committed kernel pmap intermediates now have teardown ownership:
  commit registers new branch-table `PtNode`s, unmap prunes empty L0/L1 tables,
  and release returns typed page-table frames or static PT-node pool entries
  through the pmap path instead of losing authority in the branch PTE.
- RV64 QEMU high-kernel alias now uses reserved 4 KiB L0 tables with final
  permissions: text RX, rodata R, data/bss/boot stack RW, direct map/MMIO RW
  and NX. The alias table range is published through
  `BootstrapPmapInfo.reserved_page_tables`.
- RV64 QEMU now has concrete `PmapRoot`/`Asid` process-root handoff: roots copy
  the shared kernel half, ASIDs are allocated/reused from a fixed bitmap, user
  mappings can reserve/commit/protect/unmap, and root teardown recursively
  releases committed user page-table intermediates.
- Substrate shootdown now has both kernel-global and ASID-scoped page batches;
  both hold `MapPin`s until after the HAL invalidation call. Boot also anchors
  allocator metadata, kernel-image pages, and bootstrap pmap pages as permanent
  frames after allocator installation.
- `tx_hal::pmap` now has no-alloc page-range surface helpers:
  `PmapRangeReservation<P, N>` rolls back uncommitted reserved prefixes on drop,
  `commit()` publishes the range, and range unmap/protect collect per-page
  results into caller-provided slices for later shootdown batching. Substrate
  re-exports the helpers, but the implementation now lives at the HAL surface.
- The pmap implementation is now split by responsibility: generic range
  orchestration lives in `crates/tx-hal/src/pmap.rs`, while RV64 QEMU separates
  process-root/ASID orchestration (`pmap/address_space.rs`), PT-node
  pool/typed-node ownership (`pmap/pt_node.rs`), and kernel direct-map/MMIO
  mapping mutations (`pmap/kernel_space.rs`). RV64 QEMU also separates PTE
  encoding/inspection (`pmap/pte.rs`) from Sv39/QEMU topology and big-page
  sizing/index helpers (`pmap/topology.rs`). The board facade is now
  `pmap/mod.rs`; it keeps bootstrap/high-half flow and shared table
  orchestration, with data structures first, lifecycle/data-flow functions
  next, and helper machinery after. Pmap unit tests live in `pmap/tests.rs`;
  non-pmap board code uses `pmap::topology` for constants instead of the pmap
  operation facade.
- Agent skills now include `tx-code-reorganization`, a reusable workflow for
  behavior-preserving module splits, `foo.rs` to `foo/mod.rs` facade moves,
  state/lifecycle/helper function ordering, group-level comments, and
  verification. `cargo xtask lint arch` also rejects authored Rust source files
  above 1,500 lines outside `target/` and `external/`.
- The long-term `kernel_main` roadmap is recorded as
  `docs/progress/plans/2026-04-29-kernel-main-long-term-checklist.json`,
  covering the path from the current H3 sentinel/shutdown endpoint through
  CoreInit, zone/epoch/bus, trap shell, reactor/scheduler, VM, process/thread
  runtime, exec, first userspace, SMP coordination, and runtime boot tests.
- The earlier substrate/kernel-main integration branch is closed and
  superseded by `codex/reactor-task-aware`; see
  `docs/progress/worktrees/2026-04-29-substrate-parallel-integration.json`.
  The trap vocabulary/RV64 extraction, pmap root/ASID/shootdown hardening,
  bounded zone/index/mutation primitives, and initial reactor smoke work are
  now part of the later reactor-task-aware line.
- `tx_kernel::kernel_main` now delegates to `init::CoreInit<P>::boot`, which
  names the current H3 order explicitly while preserving the
  `txkernel:<board>:reactor:task:ok` and `txkernel:<board>:boot:ok`
  sentinels. The H4 slots for post-substrate hooks, VFS/device ordering,
  scheduler/process init, and userspace entry remain deferred placeholders; see
  `docs/progress/worktrees/2026-04-29-coreinit-spine.json`.
- `tx-reactor` now has task-aware wake and first wait-channel mechanics:
  tasks carry explicit `Runnable`/`Polling`/`Parked`/`Completed` status, enter a
  runnable queue on submit or task-local wake, and repeated wake calls coalesce
  before the next poll. `wait::Channel`, `Mask`, `WaitFuture`,
  `WaitProtocol`, `WaitOutcome`, and `wait_event` now let a task park on a mask
  and let another task fire the channel; matching waiter readiness is
  token-backed so later nonmatching fires cannot erase a wake before the waiter
  is repolled. `wait_event` rechecks its condition after each wake, preserving
  the REACTOR_v0 rule that wake is not truth. Focused tests cover per-task wake
  isolation, pending wake idleness, duplicate wake coalescing, task-to-task
  channel wake, matching-wake preservation, and spurious wake re-parking.
  Verification:
  `cargo fmt --check`, `cargo test -p tx-reactor`, `cargo xtask lint unused`,
  `cargo xtask progress validate`, `cargo xtask ci`, `cargo xtask ci-slow`, and
  `git diff --check`; next step is timer/signal classification hooks plus the
  scheduler/idle loop boundary.
- `TimeIf` is now a concrete HAL deadline surface for reactor/scheduler use:
  `tx-hal` exposes `read_ns`, `set_deadline_ns`, `cancel_deadline`, and
  `frequency_hz`, plus saturating ns/tick conversion helpers. RV64 QEMU parses
  root DTB `timebase-frequency` through the existing DTB reader, publishes it
  as `PlatformInfo.timebase_frequency_hz`, reads `rdtime`, and programs
  absolute deadlines with legacy SBI `set_timer`; the qemu virt 10 MHz
  fallback is documented for absent/zero/invalid firmware data. LA64 and M1
  Dock mock boards have explicit compile stubs only. Verification:
  `cargo fmt --check`, `cargo test -p tx-hal-riscv64-qemu-virt`,
  `cargo check -p tx-kernel-riscv64-qemu-virt --target
  riscv64gc-unknown-none-elf`, `cargo xtask lint unused`, and
  `cargo xtask progress validate`; next step is for reactor/scheduler code to
  consume `TimeIf` without adding a runtime HAL manager. No blocker.
- `tx-reactor` now has host-driven timeout waits on top of the task-aware wait
  channel: `Reactor::channel()` creates timer-aware channels, timeout
  `WaitProtocol` variants carry absolute nanosecond deadlines, and
  `Reactor::advance_time_to(now_ns)` wakes expired deadlines so
  `wait_event` can return `TimedOut` while still rechecking semantic readiness
  after every event wake. Focused tests cover no early timeout,
  ready-before-timeout unregister, and spurious event wake re-parking before
  timeout. Verification: `cargo fmt --check`, `cargo test -p tx-reactor`,
  `cargo xtask lint unused`, `cargo xtask lint docs`,
  `cargo xtask progress validate`, `cargo xtask ci`, `cargo xtask ci-slow`,
  and `git diff --check`. Next step: scheduler shell boundary types and, after
  the saved-register trap shell exists, a narrow trap-to-kernel timer delivery
  hook; no EBR/zone work was touched.
- `tx-reactor` now also has the first scheduler shell:
  scheduler-facing task/hart/slice/stop/wake/meta types, `SchedulerPolicy`,
  `Phase1Scheduler`, policy-backed submit/wake/pick paths, stop-reason
  reporting for tests, and `Reactor::next_deadline_ns()`. Plain
  `Reactor::submit` futures remain kernel-only cooperative tasks; trap-driven
  timer delivery and userspace-run dispatch remain later slices. See
  `docs/progress/worktrees/2026-04-29-reactor-scheduler-shell.json`.
- `tx-reactor` is now split into focused modules and has the full prototype
  reactor shell for subsystem development: generation-checked `TaskKey`
  lifecycle, task-local wakers, wake-as-hint re-observation, timer-backed
  waits, interruptible/killable wait classification, completion/rendezvous
  helpers, AST marker queues, scheduler stop reasons, affinity wake placement,
  remote reschedule dispatch markers, typed declared wait/readiness channels
  over substrate bus declarations, a public single-slot
  `Reactor::request_userspace_run` facade, userspace-entry AST checkpointing,
  and a platform-neutral per-hart `hart_loop` step. The same PR slice carries
  the reactor runtime dependency spine: HAL `TimeIf::enable_timer_wakeups`,
  `SmpIf` parked-AP/IPI hooks, AP-local `tx_substrate::init_on_ap`, the
  CoreInit shared `BOOT_REACTOR` hart-loop adapter, RV64 saved-trap dispatch
  and trap-frame writeback, and timer/IPI interrupt paths that can drive the
  reactor loop on real harts. Verification: `cargo fmt --check`, `cargo test
  -p tx-reactor --test userspace_run`, `cargo test -p tx-reactor --test
  hart_loop`, `cargo test -p tx-reactor`, `cargo test -p tx-substrate --test
  bus`, `cargo test -p tx-substrate --test ap_init`, `cargo test -p
  tx-hal-riscv64-qemu-virt`, `cargo check -p tx-kernel-riscv64-qemu-virt
  --target riscv64gc-unknown-none-elf`, `cargo xtask lint docs`, `cargo xtask
  progress validate`, `cargo xtask ci`, and `git diff --check`. Boundary: this
  is enough for prototype reactor tasks, mock device completions, timer
  preemption, and AP wake/reschedule smokes to update owner truth and fire
  wakes, but it is not production-complete VFS/device/block runtime:
  ThreadRuntime-backed per-task userspace state, VM fault policy, signal
  delivery, syscall dispatch, external IRQ/device completion dispatch, and the
  final production sharding policy remain later. See
  `docs/progress/research/2026-05-01-reactor-runtime-dispatch-audit.md`,
  `docs/progress/decisions/2026-05-01-coreinit-hart-loop-adapter.md`, and
  `docs/progress/decisions/2026-05-01-reactor-userspace-entry-ast-checkpoint.md`.
- `tx_kernel::vm` now has a pure/mock VM foundation: `UserVirtAddr`,
  `UserPage`, `UserRange`, protection/access flags, draft `VmEntry`
  split/rewrite helpers for `munmap`/`mprotect`-like value behavior, and a
  bounded no-alloc `RangeLock` with materializer/writer modes. It does not
  publish a real zone-owned `AddressSpace`, `PageContainer`, pmap
  materialization, shootdown retention, or user-access path yet; see
  `docs/progress/worktrees/2026-04-29-vm-range-foundation.json`.
  Integrated verification on `codex/reactor-task-aware`: `cargo fmt --check`,
  `cargo test -p tx-reactor`, `cargo test -p tx-kernel vm`, `cargo test -p
  tx-kernel`, `cargo check -p tx-kernel-riscv64-qemu-virt --target
  riscv64gc-unknown-none-elf`, `cargo xtask lint unused`, `cargo xtask lint
  docs`, `cargo xtask progress validate`, `cargo xtask ci`, `cargo xtask
  ci-slow`, and `git diff --check`. Next step: either wire a kernel
  scheduler/CoreInit adapter or start the timer-trap delivery shell; blockers
  remain the full saved-register trap shell and real zone-owned VM entities.
- `TrapIf` now includes typed trap snapshots, classification, mutable trap-frame
  views, and a `KernelTrapSink` dispatch boundary. RV64 QEMU decodes common
  synchronous faults and supervisor interrupts from `scause`, saves full trap
  frames for the Rust dispatcher, routes timer/external/IPI/syscall/fault cases
  through the kernel sink, and can write back trap-frame mutations before
  resume. Full VM/syscall/user-return policy remains a later kernel slice.
- RV64 QEMU still keeps the minimal direct-mode panic vector for early boot, but
  post-init trap handling now switches to the saved-register dispatch vector.
  Fault logs include trap-frame context for `sepc` failures, and the host
  decoder/QEMU runner can annotate those logs through `cargo xtask
  fault-decode`.
- `cargo xtask ci-slow` runs the RV64 QEMU smoke sentinel lane separately from
  fast compile/lint CI.
- The active HAL, page-substrate, module-map, and invariant docs now state the
  portable boot contract later platforms must follow.
- HAL and memory/VM docs now state the address boundary policy: address typing
  belongs to pmap/boot/page-substrate/VM/user-access gates, while ordinary
  kernel subsystems speak caps, weak refs, IdentRefs, witnesses, reservations,
  recipes, and role-shaped Frame tokens.
- Agent workflow now requires a finish catch-up in `docs/progress/` before any
  completed task is declared done; see
  `docs/progress/decisions/2026-04-28-finish-catchup-progress-memory.md`.
- Step model terminology now names the STEP-4 order as a five-stage in-step
  discipline (`observe`, `upgrade`, `reserve`, `commit`, `publish`) in
  `docs/design/02_execution/STEP_MODEL_v1.md`, with INDEX/CONCEPTS summaries
  aligned. Verification: `git diff --check`, `cargo xtask lint docs`, and
  `cargo xtask progress validate`. Next step: continue using stage vocabulary
  when touching step examples; no blocker.
- This foundational workspace snapshot is ready to publish to the Txv2 remote:
  it captures the Rust skeleton, xtask tooling, docs/progress memory, OSComp and
  HumanLayer references, RV64 QEMU smoke boot, BootInfo v1, and bootstrap pmap.

## Open Blockers

- Real K210 boot, linker, and hardware path are not implemented yet.
- OSComp FAT32 image/test runner integration is not yet a passing boot test.
- LA64 target availability depends on local rustup support.
- LA64 and M1 Dock mock boot protocols are compile-first only.
- RV64 QEMU still needs superpage/multi-frame map-count batching, production
  remote-hart shootdown coordination, and VM/syscall/user-return trap policy
  before the page substrate is user/VM-ready.
- ext4 image creation requires host `mkfs.ext4`.
- BusyBox images require `TX_BUSYBOX`; dynamic musl layouts also require
  `TX_MUSL_LIBC`.

## Latest Decisions

- `docs/progress/decisions/2026-05-07-fd-ops-and-drift-cleanup.md`
- `docs/progress/decisions/2026-05-06-dac-and-setuid.md`
- `docs/progress/decisions/2026-05-06-elf-loader-and-execve.md`
- `docs/progress/decisions/2026-05-06-fork-clone-wait4.md`
- `docs/progress/decisions/2026-05-06-pre-elf-runtime-completion.md`
- `docs/progress/decisions/2026-05-06-trio-trap-syscall-tmpfs-devfs.md`
- `docs/progress/decisions/2026-05-05-tty-signal-end-to-end-typed-dispatch.md`
- `docs/progress/decisions/2026-05-05-tty-pgrp-typed-rebinding.md`
- `docs/progress/decisions/2026-05-05-step-waitpid-nohang.md`
- `docs/progress/decisions/2026-05-05-step-exit-group-with-signal.md`
- `docs/progress/decisions/2026-05-05-signal-gewalt-event-factoring.md`
- `docs/progress/decisions/2026-05-05-signal-delivery-sweep-day1.md`
- `docs/progress/decisions/2026-05-05-signal-day1.md`
- `docs/progress/decisions/2026-05-05-sigchld-edge.md`
- `docs/progress/decisions/2026-05-05-session-leader-hangup.md`
- `docs/progress/decisions/2026-05-05-process-topology-day1.md`

## Latest Research

- `docs/progress/research/2026-05-07-interface-drift-audit.md`
- `docs/progress/research/2026-05-04-vm-pagebacked-midway-checkpoint.md`
- `docs/progress/research/2026-05-04-vm-pagebacked-gap-update.md`
- `docs/progress/research/2026-05-04-vm-pagebacked-final-ledger.md`
- `docs/progress/research/2026-05-04-tty-implementation-status.md`
- `docs/progress/research/2026-05-03-vm-doc-gap-ledger.md`
- `docs/progress/research/2026-05-01-reactor-runtime-dispatch-audit.md`
- `docs/progress/research/2026-05-01-coreinit-runtime-loop-scout.md`
- `docs/progress/research/2026-05-01-ast-return-to-user-scout.md`
- `docs/progress/research/2026-04-30-reactor-third-wave-scout.md`
- `docs/progress/research/2026-04-30-reactor-third-wave-audit.md`
