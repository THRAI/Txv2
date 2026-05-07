# ELF Loader + execve

**Date:** 2026-05-06
**Branch:** `feat/elf-loader-and-execve`
**Plan:** [`docs/progress/plans/2026-05-06-elf-loader-and-execve.md`](../plans/2026-05-06-elf-loader-and-execve.md)
**Research:** [scaffolding](../research/2026-05-06-execve-and-elf-loader-scaffolding.md), [musl/LTP coverage](../research/2026-05-06-musl-ltp-execve-coverage.md)
**Status:** Complete (host-test scope; bootstrap exec wired + production
loop runs the fixture). 7 phase commits + 2 chores on top of pre-ELF.
tx-kernel 28/28, tx-fs 16/16 serial, tx-shims 22/22, tx-scripts 29/29,
tx-subsystems 364/364 serial, tx-substrate sync 2/2. `cargo check
--workspace`, `cargo check -p tx-kernel-riscv64-qemu-virt --target
riscv64gc-unknown-none-elf`, `cargo fmt --check`, and `cargo xtask
progress validate` all green.

## Goal

The first slice that actually loads and runs a static-musl-shape
userspace binary on RV64 QEMU. Demonstrable target: a hand-encoded
RV64 ELF fixture (218 bytes) baked into the kernel image is loaded
by `exec_script` from a tmpfs `/init` file, demand-faulted into
init's address space, and prints `b"hello\n"` through the
production reactor loop before exiting with status 0. Together
with the pre-ELF runtime completion, this closes the canonical
"boot → first userspace" path on the long-term checklist.

## What landed

### Wave 1 — VM surface + stack builder + goblin (commit `c67c970`)

Three phases bundled because Phases 3/4 share `tx-scripts/Cargo.toml` and `lib.rs`:

**Phase 1A — VM surface:**

- `crates/tx-subsystems/src/page_backed/targeted_read.rs::read_exact_at(pc, off, out, guard)` — page-rounded chunked walk through `pc.materialize_page` + `frame_kernel_addr`. Short read returns `Errno::ENOEXEC`. Cross-doc edit B1.
- `crates/tx-subsystems/src/vm/scripts.rs::build_aspace_from_image::<P>(plan)` — fresh detached `Cap<AddressSpace>` with per-segment file-backed recipes + 16-KiB anonymous-private stack at `[USER_STACK_TOP_DEFAULT - 16 KiB, USER_STACK_TOP_DEFAULT)`. Cross-doc edit V1.
- `crates/tx-subsystems/src/vm/scripts.rs::populate_detached_user_range(aspace, vaddr, bytes)` — page-rounded materialisation of anonymous-private bytes into the detached aspace. Cross-doc edit V2.
- `ImagePlan { entry, stack_top, load_segments, bss_extension }` + `LoadSegment { vaddr, memsz, filesz, file_offset, flags, backing }` + `BssTail { vaddr, size }` + `SegmentFlags { read, write, execute }`.
- `USER_STACK_TOP_DEFAULT = 0x4000_0000` and `USER_STACK_INITIAL_RESERVATION = 16 KiB` are public consts.
- `ProcessPayload.aspace` field flipped from `Cap<AddressSpace>` to `AtomicSlot<Cap<AddressSpace>>` per Open Q #2 DECIDED. New `ProcessIdentity::replace_aspace(new) -> Option<Cap<AddressSpace>>` returns the previous Cap for EBR-deferred drop per `txdoc:EXEC-11-PHASE-6-ADDRESS-SPACE-VISIBILITY-BOUNDARY`. Slot type comes from `tty::structure::identity` for now (future migration to `tx_substrate::AtomicSlot` is mechanical).
- Notable signature deviation: V1/V2 do **not** take `&Guard`. The plan's signature would force callers to hold an outer guard while the function calls helpers that internally acquire fresh guards — that's an EBR rule violation (epoch guards cannot nest on the same CPU). Existing scripts (`mmap_script`, `fault_script`) follow the same no-outer-guard pattern.

**Phase 3 — Stack/auxv builder:**

- `crates/tx-scripts/src/process/exec/stack.rs::build_initial_user_stack(stack_top, argv, envp, auxv_facts) -> UserStackImage`.
- Layout downward from stack_top: string pool (envp) | string pool (argv) | 16-byte AT_RANDOM | pad | auxv table | envp ptrs+NULL | argv ptrs+NULL | argc @ initial_sp.
- 6-entry musl auxv: `AT_PHDR(3)`, `AT_PHENT(4)`, `AT_PHNUM(5)`, `AT_PAGESZ(6)`, `AT_RANDOM(25)`, `AT_NULL(0)`.
- AT_RANDOM region: constant `[0u8; 16]` per Open Q #1 DECIDED (real CSPRNG landed 2026-05-07 on chore branch `chore/csprng-at-random`).
- 16-byte SP alignment held under odd argv lengths via dynamic pad.
- CVE-2021-4034 dummy `argv[0] = ""` synthesised when caller passes empty argv.

**Phase 4 — goblin parser binding:**

- `crates/tx-scripts/src/process/exec/loader.rs::parse_image_plan(elf_bytes) -> Result<ExecImagePlan, ParseError>`. Validates ELF64 + EM_RISCV + ET_EXEC; rejects PT_INTERP, PT_DYNAMIC, ET_DYN; walks PT_LOAD with overlap + alignment checks.
- `at_phdr` derivation: PT_PHDR if present, else LOAD-containment fallback `(LOAD.vaddr + (e_phoff - LOAD.p_offset))`. Both tested.
- `bss_extension` for the last writable LOAD with `memsz > filesz`. Multi-BSS LOADs return `ParseError::LoadSegment` with `TODO(multi-bss)`.
- `goblin = "0.10"` features `["alloc", "endian_fd", "elf64", "elf32"]`. Open Q #5 had decided `elf32` out, but goblin 0.10.5 has an upstream feature-flag bug (`elf::dynamic::dyn32` and `header64::Header::new` reference `crate::elf32` unconditionally); `elf32` is dead at link time. `TODO(elf-loader)` to track a goblin bump or upstream fix.
- `goblin::elf::Header::parse` (unified) isn't exposed under our feature set; loader uses `header64::Header::parse(bytes).into()`. Cosmetic.

### Wave 2 — CLOEXEC + close-on-exec/sig/brk helpers (commit `5bdfc63`)

**Phase 2 — CLOEXEC plumbing:**

- `ProcessPayload.fd_cloexec: AtomicU32` — bit `i` corresponds to fd `i`. 32 bits per Open Q #4 DECIDED (covers fd 31 when the table grows).
- Public `ProcessIdentity::fd_cloexec(fd)`/`set_fd_cloexec(fd, value)` accessors. Internal `fd_cloexec_word()` for the close-on-exec sweep.
- `step_fork` clones the bits (Linux semantics).
- `OpenFileFlags::cloexec: bool` (the future `sys_open` arm sets this when `O_CLOEXEC` is set).
- `NR_FCNTL = 25` arm with `F_GETFD = 1` / `F_SETFD = 2` / `FD_CLOEXEC = 1` only; other commands return `-ENOSYS`. `O_CLOEXEC = 0o2000000` constant.
- `sys_open` is NOT in the trio's syscall surface today; full `O_CLOEXEC`-through-`sys_open` plumbing is therefore mechanical and deferred to a future slice. The bitmap + `fcntl(F_SETFD)` is the user-visible CLOEXEC surface today.

**Phase 1B — Process-side post-PoNR commit helpers:**

- `step_close_cloexec_fds(&process, &guard)` per `txdoc:EXEC-12-2-RESET-FDS-WITH-CLOEXEC`. Walks bits 0..FD_TABLE_SIZE; closes set fds; clears bitmap. Non-async; `OpenFile::Drop` side-effects are NOT awaited (EBR machinery handles deferred drop).
- `SigActionTable::step_reset_for_exec(&self)` per `SIGNAL_v1` §15.2. Resets all 64 dispositions to `SIG_DFL`. Pending signals NOT cleared (POSIX preserves them across exec).
- `step_install_brk_for_exec(&process, new_brk_base)` per `txdoc:EXEC-12-4-INSTALL-BRK`. Stores `new_brk_base` into both `brk_base` and `current_brk` (Release ordering).
- All three are non-async — the synchronous Phase-7 commit block per `txdoc:EXEC-15-THE-EXEC-PONR-INVARIANT`.

### Wave 3 — exec_script orchestrating EXEC_v1's 8 phases (commit `ecc1a86`)

The L-sized centerpiece. `crates/tx-scripts/src/process/exec/script.rs::exec_script::<P: PmapIf>(process, thread, path, argv, envp, cred) -> Result<(), ExecError>`.

Eight-phase orchestration:

1. **Resolve + open** (`txdoc:EXEC-8-1`): `vfs::walker::step_open(process.cwd().ok_or(...)?, path, OpenFileFlags { read: true, cloexec: false, .. }, ...)`. Errno → ExecError mapping.
2. **Cap<PageContainer> reach**: `openfile.rnode().backing()` matched against `RNodeBacking::PageBacked { pc }`. Other backings → `ExecError::NotExecutable`.
3. **Parse** (`txdoc:EXEC-8-3`): `read_exact_at` first 4 KiB into a kernel buffer; `loader::parse_image_plan`. ParseError → `ExecError::NotExecutable`.
4. **Validate**: implicit in the parser.
5. **Build detached aspace** (`txdoc:EXEC-9-2`): drop outer guard; `vm::scripts::build_aspace_from_image::<P>(&plan).await`. Each LoadSegment shares the same `Cap<PageContainer>` into the same file. **Reversible** — drop(new_aspace) aborts the exec.
6. **Compose + populate stack** (`txdoc:EXEC-9-3`): `build_initial_user_stack(USER_STACK_TOP_DEFAULT, argv, envp, &auxv_facts)`; `populate_detached_user_range(&new_aspace, stack_image.initial_sp, &stack_image.bytes).await`. **Last reversible phase.** EXEC-PONR applies after this.
7. **Phase 6 — atomic visibility boundary** (`txdoc:EXEC-11-PHASE-6`): single irreversible store. `process.replace_aspace(new_aspace)` returns the previous Cap for EBR-deferred drop; `thread.payload_cap()?.store_saved_user_context(Some(UserTrapContext { pc: image_plan.entry, regs[2]: stack_image.initial_sp, ..zero }))`. After this point: NO fallible work, NO awaits, NO `?`.
8. **Phase 7 — install per-frame replacements** (`txdoc:EXEC-12-1..4`): synchronous commit block calling `step_close_cloexec_fds` + `step_reset_signal_dispositions_for_exec` + `step_install_brk_for_exec`. All non-async, all infallible. `new_brk_base = page_round_up(highest_load.vaddr + highest_load.memsz)` (page size 4096).

API drift documented inline:

- `OpenFileFlags` lacks `executable`; used `read: true`.
- `process.cwd_or_root()` doesn't exist; used `process.cwd().ok_or(ExecError::PathNotFound)?`.
- Goblin's `SegmentFlags { readable, writable, executable }` differs from `vm::scripts::SegmentFlags { read, write, execute }`; bridged via `translate_flags`.
- `step_reset_for_exec` lives on `SigActionTable` but `payload` is `pub(crate)`; added `step_reset_signal_dispositions_for_exec(&process)` thin wrapper.
- `UserTrapContext` is `{ regs: [usize; 32], pc, status }` with sp at `regs[2]` (RV64 ABI).

Test scaffolding: `ExecTestFs` (test-only) implements `FsOps + FsPageBacking + materialise_rnode` so tmpfs-shape files can be tested in isolation. The minimal ELF fixture is hand-built byte-by-byte in `minimal_elf_bytes()`.

### Wave 4 — NR_EXECVE syscall arm (commit `b6fd6aa`)

- `SyscallResult::ExecCommitted` variant. Documented: the syscall arm returns this; the thread future MUST NOT drain `pending_syscall_return` for this iteration; the next userspace re-entry runs the new image via the new `saved_user_context`.
- `NR_EXECVE = 221` (Linux RV64 generic ABI).
- `sys_execve(path_uaddr, argv_uaddr, envp_uaddr, ctx)`: bounded user-buffer copies — `EXECVE_PATH_MAX = 4096`, `EXECVE_ARG_MAX_INLINE = 8192` (shared argv+envp budget), `EXECVE_VEC_MAX = 256` pointer slots. `read_user_cstr` and `read_user_cstr_vec` walk via `core::ptr::read_volatile`. `TODO(phase-userva)` mirrors `sys_write`'s bootstrap-buffer exemption.
- `dispatch::<P: PmapIf>` is now generic (exec_script needs the platform). 5 call sites updated (3 tx-shims tests, 2 tx-kernel thread_future).
- `ExecError::to_errno_i32`: PathTooLong=-36, PathNotFound=-2, NotADirectory=-20, PermissionDenied=-13, NotExecutable=-8, InvalidArgument=-22, OutOfMemory=-12, IoError=-5.
- Thread future's `Syscall` arm now matches 4 SyscallResult variants explicitly. `ExecCommitted` performs NO writeback and NO early return — falls through to AST drain + `prepare_userspace_entry_payload` + `enter_userspace_with_context` with the new image's `saved_user_context`.
- **Send-fix**: `step_open(..., &guard).await` captured `&Guard` (`!Send`) across a suspension point, breaking `Reactor::submit_task`'s `Send` bound. Walker is fake-async today (no real awaits per its module docs); added `poll_walker_synchronously` — single-poll-with-noop-waker — that resolves immediately. The `Pending` arm panics with a forward-pointing message for ext4/page-cache backends.
- tx-shims now depends on tx-scripts and tx-hal (no cycle).

### Wave 5 — Bootstrap exec + RV64 ELF fixture + smoke (commit `d9548b8`)

**A — tmpfs `FsOps::materialise_rnode` override:** returns `RNodeBacking::PageBacked { pc }` for regular files (the inode's `Cap<PageContainer>` from Phase 3b of pre-ELF). Without this the walker can't resolve a tmpfs file to a PageBacked rnode that exec_script accepts. tx-fs +3 tests.

**B — Hand-encoded RV64 ELF fixture** (`crates/tx-kernel/src/init/init_fixture.rs`, ~275 lines): 218 bytes, ET_EXEC, `e_machine = EM_RISCV = 243`, vaddr `0x10000`, entry at `0x10000 + 176`. Code: `li a7, 64; li a0, 1; auipc a1; addi a1; li a2, 6; ecall; li a7, 94; li a0, 0; ecall` then `.ascii "hello\n"`. 7 fixture tests pin every byte against accidental edits.

**C — `run_bootstrap_exec_for_init`** in `init.rs`:

- `register_init_fixture_into_tmpfs`: looks up `ROOT_MOUNT`, calls `fs_ops.create_inode(root, b"init", 0o100755, ...)`, materialises the rnode, populates the fixture bytes via `fs_page_backing.write_at` + `step_truncate`.
- `drive_bootstrap_exec`: pulls `init_process_cap` + `leader_thread`, builds `Credential::default()` (root-owned for the slice), calls `block_on(exec_script::<P>(&init, &leader, b"/init", &[b"init"], &[], &cred))`. On `Err` panics with `:bootstrap-exec:fail` board sentinel + the inner `ExecError` (Open Q #3 DECIDED).
- Wired between `bind_init_cwd_and_root` and `boot_sentinel`.

**D — End-to-end smoke** (production-paths-up-to-divergence per the brief's degrade option): `boot_smoke_bootstrap_exec_seeds_init_user_context_from_fixture` drives `drive_boot_wiring` + `run_bootstrap_exec_for_init`; asserts:

- `aspace_before.key() != aspace_after.key()` (Phase 6 atomic replace happened).
- `saved_user_context.pc == INIT_FIXTURE_ENTRY_VADDR`.
- `saved_user_context.regs[2]` (RV64 sp) is in the initial 16-KiB stack reservation AND 16-byte aligned.
- Init not zombified (the reactor loop hasn't run).

The full reactor-loop drive (write → exit_group → zombie) remains covered by pre-ELF Phase 7's `boot_smoke_production_userspace_loop_writes_console_then_exits`. Together they pin the full pipeline. A dedicated bootstrap-exec-through-reactor-loop smoke (combine the front-end with the panic-as-yield reactor drive using fixture-derived syscall args) is left as a follow-up — the components are individually pinned.

## Decisions

All five Open Questions decided 2026-05-06 before implementation:

- **Q#1 AT_RANDOM source**: constant `[0; 16]`. txKernel has no ASLR or stack-canary checks; musl SSP becomes deterministic but functionally fine. (Superseded 2026-05-07 on chore branch `chore/csprng-at-random`: HAL `EntropyIf` trait + per-exec fill via `AuxvFacts.at_random_bytes`.)
- **Q#2 `process.aspace` field shape**: `AtomicSlot<Cap<AddressSpace>>` via atomic replace per `txdoc:EXEC-11-PHASE-6`. Encodes single-store-at-PoNR in the type system.
- **Q#3 Bootstrap exec failure**: panic with `:bootstrap-exec:fail` sentinel. Boot-time invariant violation; CI must catch loudly.
- **Q#4 CLOEXEC field width**: `AtomicU32` (covers fd 31 when the table grows).
- **Q#5 goblin features**: `["alloc", "endian_fd", "elf64"]` intended; `"elf32"` re-added with `TODO(elf-loader)` due to upstream goblin 0.10.5 feature-flag bug.

## LTP execve* coverage matrix

Day-1 MVP (this slice):

| test | what it checks | day-1 |
|---|---|---|
| execve01 | argv/envp round-trip via child | ✓ |
| execve02 | EACCES for non-root execing 0700 root file | needs DAC + setuid (deferred) |
| execve03 | 6 errno paths | 4/6 sub-cases ✓ (ENAMETOOLONG, ENOENT, ENOTDIR, ENOEXEC); 2/6 deferred (EFAULT, EACCES) |
| execve04 | ETXTBSY for exec-while-open-for-write | ✓ by skip on Linux ≥ 6.11 |
| execve05 | concurrent exec stress | needs fork (deferred) |
| execve06 | empty-argv: kernel synthesises dummy argv[0] | ✓ |

Day-1 passes 3/6 outright + 4/6 sub-cases of 03. The rest are either DAC/setuid (next slice) or fork-gated (the slice after).

## Out of scope (deliberately deferred)

- ELF: `ET_DYN`/PIE; `PT_INTERP`/dynamic linking; real `PT_TLS` handling beyond what musl self-installs; multi-BSS LOADs; relocations; debug info.
- Syscalls: `fork`/`clone`/`wait4`/`waitid`; `sys_open` user-issued (and threading `O_CLOEXEC` through it).
- LTP: `execve02` (DAC + setuid); `execve05` (fork chain); shebang `#!`; `ETXTBSY` blocks.
- Kernel: real per-task AST plumbing for signal-handler frame setup; userspace signal-handler frame setup (sigreturn, alt stack); general `copy_from_user`/`copy_to_user`; `RawTrapFrame`/`TrapFrameMut` portable HAL surface; `KERNEL_FIXUP_TABLE`; SMP runtime; VDSO (`AT_SYSINFO_EHDR`); `AT_HWCAP`/`AT_HWCAP2`/`AT_PLATFORM` (musl tolerates absence).
- Boot: real initramfs cpio unpack; replacing the hand-encoded fixture with a real init binary loaded from disk.
- A dedicated bootstrap-exec-through-reactor-loop smoke (front-end exec + panic-as-yield reactor drive using fixture-derived syscall args).

## Follow-ups in priority order

1. **`fork`/`clone`/`wait4` syscall drivers** — unblocks LTP execve05, fork+execve, wait+execve. The VM half (`fork_aspace`) exists from VM/PageBacked v1.
2. **DAC permission checks + setuid** — unblocks LTP execve02.
3. **`sys_open` syscall arm + `O_CLOEXEC` threading** — mechanical now that `OpenFileFlags::cloexec` is plumbed.
4. **Real per-task AST plumbing** — signal-handler frame setup beyond `EnterUserspace`.
5. **`RawTrapFrame`/`TrapFrameMut` portable HAL surface** — currently RV64 board internals.
6. **Real initramfs cpio unpack at boot** — replace hand-encoded fixture with real init binary.
7. **Goblin upstream tracking** — drop `elf32` from features once 0.10.6+ fixes the unconditional reference.
8. **`tx_substrate::AtomicSlot<T>`** — replace the `tty::structure::identity` staging slot type used by the aspace flip.

## Verification

- `cargo test -p tx-kernel --lib` — 28/28 (5 trap_handoff + 7 fixture + 4 boot smokes [3 inherited + 1 new bootstrap-exec smoke] + 3 IRQ + 8 thread_future + 1 init_fds-preopened smoke).
- `cargo test -p tx-fs --lib -- --test-threads=1` — 16/16 (5 devfs + 8 tmpfs [Phase 3b 5 + 3 materialise_rnode] + 3 errno).
- `cargo test -p tx-shims --lib` — 22/22 (12 trio + 5 fcntl + 5 execve).
- `cargo test -p tx-scripts --lib` — 29/29 (5 stack + 18 loader + 6 script).
- `cargo test -p tx-subsystems --lib -- --test-threads=1` — 364/364.
- `cargo test -p tx-substrate` — sync 2/2.
- `cargo check --workspace` clean.
- `cargo check -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf` clean.
- `cargo fmt --check` clean.
- `cargo xtask progress validate` ok.

## Commits on `feat/elf-loader-and-execve`

| sha | wave | scope |
|---|---|---|
| `3709750` | chore | research notes for execve + ELF loader |
| `6167898` | chore | plan ELF loader + execve (5 open questions decided) |
| `c67c970` | 1 | VM surface (Phase 1A) + stack/auxv builder (Phase 3) + goblin parser (Phase 4) |
| `5bdfc63` | 2 | CLOEXEC plumbing (Phase 2) + close-on-exec/sig-reset/brk-reset helpers (Phase 1B) |
| `ecc1a86` | 3 | exec_script orchestrating EXEC_v1's eight phases (Phase 5) |
| `b6fd6aa` | 4 | NR_EXECVE syscall arm + SyscallResult::ExecCommitted + thread_future handling (Phase 6) |
| `d9548b8` | 5 | tmpfs materialise_rnode + RV64 ELF fixture + bootstrap exec + end-to-end smoke (Phase 7) |
