# ELF Loader + execve

Status: proposed (planning only). Companion to
`docs/progress/plans/2026-04-29-kernel-main-long-term-checklist.json`
(`exec-loader-and-detached-aspace` step + `first-userspace-entry`
step both move toward complete after this slice). The first slice
that actually loads and runs a static-musl userspace binary on
RV64 QEMU. Builds on the pre-ELF runtime completion
(`docs/progress/plans/2026-05-06-pre-elf-runtime-completion.md`):
the production reactor loop in
`crates/tx-kernel/src/init.rs::run_userspace_reactor_loop` (line
911) already drives `crate::thread_future::run_thread::<P>` to
zombification, the trap shell already routes syscalls and page
faults through `prepare_userspace_entry_payload` /
`enter_userspace_with_context`, and Phase 3's wiring of
`AddressSpace::fault_script` from inside the thread future means
demand-faulting is fully load-bearing for the loader. Inputs:
`docs/progress/research/2026-05-06-execve-and-elf-loader-scaffolding.md`,
`docs/progress/research/2026-05-06-musl-ltp-execve-coverage.md`,
the seven `EXEC_v1` decisions locked in 2026-05-06 (demand-faulting,
per-fd CLOEXEC bitmap option (b), `goblin` for parsing, static-musl
target, 6-entry musl auxv, kernel does not seed `tp` from
`PT_TLS`).

## Goal

Demonstrate that a static-musl-linked `hello-world` binary,
embedded into the kernel image as a built-in `&[u8]` and
registered into tmpfs at `/init`, is loaded by the new exec
script, demand-faulted into pid=1's `Cap<AddressSpace>`, and
prints `hello\n` plus `exit_group(0)` through the production
reactor loop. End state: the trio's `run_userspace_reactor_loop`
sees pid=1's `payload.saved_user_context` populated (entry +
sp + zeroed gprs) by the loader before its first BSP-loop
iteration, drives `enter_userspace_with_context`, takes the
binary's first `write(1, "hello\n", 6)` syscall through the trap
shell into the thread future's syscall arm, sees the
`exit_group(0)` arm fire, and emits `:userspace:exited:0`.

Smoke target — pick **(a) host test** for the slice: a single
host test in `crates/tx-kernel/src/init/tests.rs` extending the
trio's existing `boot_smoke_userspace_round_trip_writes_console_then_exits`
shape to register the embedded fixture binary into tmpfs at
`/init`, call `exec_script(init_process, init_thread, b"/init",
&[], &[])`, drive the existing `Reactor::submit_task(thread_future)`
loop, and assert the console captures `b"hello\n"` plus
`init.is_zombie()` with `ExitStatus::Exited(0)`. The host test
uses the same panic-as-yield pattern that pre-ELF Phase 7 used
for its end-to-end smoke. **Deferred (b)**: real RV64 QEMU boot
that emits `:userspace:exited:0` from the platform console.
That requires extending `cargo xtask qemu --target rv64-qemu`
with an `--image PATH` flag and a board-side fixture-load step;
documented under Out of scope, picked up after the host smoke
proves the loader.

## Doc anchors

- `txdoc:EXEC-WHAT-THIS-DOCUMENT-PINS`,
  `txdoc:EXEC-2-WHERE-EXEC-LIVES`,
  `txdoc:EXEC-2-1-MODULE-PLACEMENT`
  (`docs/design/02_execution/EXEC_v1.md`) — exec is a script,
  not a subsystem; module placement is `tx-scripts/src/process/exec/`.
- `txdoc:EXEC-4-THE-EIGHT-PHASES`,
  `txdoc:EXEC-4-2-PHASE-SUMMARY-TABLE` — the eight phases the
  exec script runs.
- `txdoc:EXEC-15-THE-EXEC-PONR-INVARIANT` — no allocation, no
  user-memory access, no I/O, no fallible computation past
  phase 6.
- `txdoc:EXEC-8-1-LOADER-PUBLIC-TYPES`,
  `txdoc:EXEC-8-2-THE-BOUNDED-TARGETED-READ-MODEL`,
  `txdoc:EXEC-8-3-ENTRY-LOAD-EXEC-IMAGE`,
  `txdoc:EXEC-8-4-HEADER-VALIDATION`,
  `txdoc:EXEC-8-5-PROGRAM-HEADER-VALIDATION`,
  `txdoc:EXEC-8-6-AT-PHDR-COMPUTATION`,
  `txdoc:EXEC-8-7-STATIC-PIE-HANDLING-AND-LOAD-BIAS`,
  `txdoc:EXEC-8-8-PT-TLS`,
  `txdoc:EXEC-8-9-ERRNO-MAPPING`,
  `txdoc:EXEC-8-10-IMPLEMENTATION-NOTE-NON-NORMATIVE` — the
  loader's parser contract.
- `txdoc:EXEC-9-2-BUILD-THE-DETACHED-ADDRESSSPACE`,
  `txdoc:EXEC-9-3-POPULATE-THE-INITIAL-USER-STACK`,
  `txdoc:EXEC-9-4-AUXV-CONSTRUCTION`,
  `txdoc:EXEC-9-5-PREPARE-PRIVATE-FD-TABLE`,
  `txdoc:EXEC-9-6-PREPARE-PRIVATE-SIG-ACTIONS` — phase 4's
  reversible work.
- `txdoc:EXEC-11-PHASE-6-ADDRESS-SPACE-VISIBILITY-BOUNDARY`,
  `txdoc:EXEC-12-5-CLEAR-GROUPEXIT-EPISODE`,
  `txdoc:EXEC-12-6-INSTALL-USER-TRAP-CONTEXT`,
  `txdoc:EXEC-16-SIGNAL-RESET-SEMANTICS` — the irreversible
  phase 6 store and phase 7's infallible commits.
- `txdoc:STEP-MODEL` (`docs/design/02_execution/STEP_MODEL_v1.md`)
  — the observe / upgrade / reserve / commit / publish cadence
  the exec script and its peer steps obey.
- `txdoc:VM-5-1-FAULT-HANDLER`,
  `txdoc:VM-3-6-CROSS-ASYNC-WAIT-DISCIPLINE`
  (`docs/design/03_memory-vm/VM_v1_2.md`) — the page-fault
  path the loader's PageContainer-backed `VmEntry`s rely on
  post-swap. Already wired by pre-ELF Phase 3.
- `txdoc:VM-5-8-BRK` — `brk_base`/`current_brk` reset semantics
  the loader honours.
- `txdoc:PAGE-BACKED-2-RNODEBACKING`,
  `txdoc:PAGE-BACKED-3-PAGECONTAINER`
  (`docs/design/03_memory-vm/PAGE_BACKED_v1.md`) — the
  PageContainer recipe the loader binds segments to. Existing
  surface: `step_truncate`, `step_read_to_user`,
  `step_write_from_user` in
  `crates/tx-subsystems/src/page_backed/{lifecycle,user_buffer}.rs`.
- `txdoc:PROCESS-WHAT-THIS-DOCUMENT-PINS-1`
  (`docs/design/04_process-signals/PROCESS_v1.md`) — exec's
  interaction with signal disposition reset, fd close-on-exec,
  brk_base reset.
- `txdoc:VFS-CHECKS-RUN-WALKER-LOOP-1`
  (`docs/design/05_filesystem/VFS_CHECKS_V2.1.md`) — the
  walker the loader uses to resolve `/init`.

## Part 1 — Cross-doc supporting edits (B1, V1, V2, P1, P2, P3)

Pre-loader spec primitives. Each is named in
`txdoc:EXEC-WHAT-THIS-DOCUMENT-PINS` as a cross-doc obligation
the spec demands before exec can be implemented. The MVP
research note (2026-05-06-musl-ltp-execve-coverage) labels them
B1, V1, V2, P1, P2, P3 — same labels used here.

### B1 — `page_backed::read_exact_at`

The bounded targeted read primitive. The loader needs it to
read the 64-byte ELF header at offset 0 and the ≤ 3584-byte
program-header table at `e_phoff` *before* an AddressSpace
exists for the new image (so demand-faulting is not yet
available for these reads — they go through `PageContainer` /
`FsPageBacking` directly).

#### Surface

- New module-level item in
  `crates/tx-subsystems/src/page_backed/user_buffer.rs` (or a
  sibling `targeted_read.rs` if `user_buffer.rs` is reserved
  for `UserAccessIf`-flavored copies):
  ```text
  pub async fn read_exact_at(
      pc: &PageContainer,
      offset: u64,
      out: &mut [u8],
      guard: &Guard<'_>,
  ) -> Result<(), Errno>
  ```
- Returns `Err(Errno::ENOEXEC)` on short read (request bytes
  past `step_size`'s reported visible size). Returns
  `Err(Errno::EIO)` on `FsPageBacking` materialisation failure
  per `txdoc:PAGE-BACKED-3-PAGECONTAINER`. May `.await` an
  internal `materialize_page` if the target page is not yet
  resident.
- Implementation note: the call walks page-rounded chunks of
  `out`, each chunk goes through `PageContainer::install_if_absent`
  or its `materialize_page` cousin, and copies bytes from the
  resident page's direct-map view into `out`. No `UserAccessIf`
  involvement (the destination is a kernel slice, not a user
  pointer).

#### Tests

- `read_exact_at_round_trips_aligned_chunks` — write a known
  pattern via `step_write_from_user`, then read via
  `read_exact_at`; assert byte-equal.
- `read_exact_at_returns_enoexec_on_short_read` — read past
  the PageContainer's recorded size; expect
  `Err(Errno::ENOEXEC)`.
- `read_exact_at_crosses_page_boundary` — request a 5000-byte
  read at offset 1024 against a 2-page container.
- `read_exact_at_returns_eio_when_fs_backing_fails` — use a
  `FsPageBacking` test stub whose `read_page` returns `EIO`;
  assert the error propagates unchanged.

### V1 — `vm::scripts::build_aspace_from_image`

The new VM script that produces a fresh detached `Cap<AddressSpace>`
from a parsed `ExecImagePlan`. Replaces the v1.2-shaped
`vm::execution::exec_aspace` (still in tree at
`crates/tx-subsystems/src/vm/execution.rs:130`, kept for now —
its caller is the loader, which the script supersedes). The
existing `exec_aspace` will be deleted once V1 lands and the
loader uses the new primitive end-to-end.

#### Surface

- New module `crates/tx-subsystems/src/vm/scripts.rs` (or
  `vm/execution_scripts.rs` to match the test naming
  convention at `crates/tx-subsystems/src/vm/tests.rs:10`):
  ```text
  pub async fn build_aspace_from_image<P: PmapIf>(
      file_pc: Option<Cap<PageContainer>>,
      image: &ExecImagePlan,
      guard: &Guard<'_>,
  ) -> Result<Cap<AddressSpace>, Errno>
  ```
- Allocates a fresh `AddressSpace` (existing
  `AddressSpace::new` / construction surface), then for each
  `LoadSegment` in `image.segments` registers a recipe row:
  - **File-backed prefix** (length = `file_size_rounded_up`):
    `VmEntry { range: [map_start, map_start + file_size_rounded_up), backing: VmBacking::Page { pc: file_pc.clone(), offset: file_page_offset }, prot, shared: false }`.
  - **BSS tail** (length = `mem_size_rounded_up - file_size_rounded_up` when `mem_size > file_size`):
    `VmEntry { range: [..., map_start + mem_size_rounded_up), backing: VmBacking::PrivateAnon, prot, shared: false }`.
- Allocates a stack `VmEntry` (anonymous private, top-down,
  16 KiB initial reservation, growable per a future
  `expand_stack` script — out of scope).
- Sets `aspace.brk_start` / `aspace.brk_current` to the
  page-rounded end of the highest data segment (per
  `txdoc:VM-5-8-BRK`).
- Shares the kernel high half (the `kernel_pmap` clone path
  already used by `AddressSpace::new`).
- Returns `Cap<AddressSpace>` held only by the loader's
  detached-build local until phase 6's `frame.vm.replace`.
  No live PTEs at this point — every page is recipe-only.

#### Tests

- `build_aspace_from_image_emits_recipes_per_load_segment` —
  feed a synthetic `ExecImagePlan` with two LOAD segments;
  assert `aspace.recipes.iter().count() == 2 + bss_count + 1 (stack)`.
- `build_aspace_from_image_zero_fills_bss_tail` — synthesise a
  segment with `mem_size > file_size`; faulting any page in
  the bss tail materialises zero-filled pages (the existing
  `fault_script` path).
- `build_aspace_from_image_sets_brk_to_highest_data_end` —
  load with two data segments at different vaddrs; assert
  `aspace.brk_start` equals the higher one's
  page-rounded `map_end`.
- `build_aspace_from_image_rejects_overlapping_load_segments`
  — synthesise an `ExecImagePlan` whose two segments overlap;
  expect `Err(Errno::ENOEXEC)`. (Validation lives in Part 4's
  parser, but defence-in-depth in V1 is cheap.)

### V2 — `vm::populate_detached_user_range`

The only surface that can write into an AddressSpace no thread
is currently running in. `copy_to_user` (the existing
`UserAccessIf::copy_to_user_*` shape) walks the **current**
hart's pmap; the loader has a `Cap<AddressSpace>` not yet
swapped in. V2 walks the recipes BTree itself, anonymously
allocates or fault-pulls the target page, and copies bytes
through the kernel's direct-map view of the target frame's
PPN.

#### Surface

- Sibling to V1 in `vm/scripts.rs`:
  ```text
  pub async fn populate_detached_user_range<P: PmapIf>(
      aspace: &Cap<AddressSpace>,
      dst: UserAddr,
      src: &[u8],
      guard: &Guard<'_>,
  ) -> Result<(), Errno>
  ```
- Errno surface: `EFAULT` if `dst` is outside any recipe range;
  `EINVAL` if the recipe is read-only; `ENOMEM` if a needed
  anonymous page cannot be allocated; `EIO` if a file-backed
  page-pull fails.
- Implementation walks page-rounded steps of `src`; for each
  page, looks up the covering `VmEntry` via
  `aspace.recipes.lookup`, materialises the page (anonymous:
  fresh `Frame`; file-backed: `PageContainer::install_if_absent`
  the same way `fault_script` does), copies via the direct-map
  view, and registers the page in the AS's pmap so a later
  fault on the same vaddr finds it.
- Used twice by the loader: (1) populate the initial userspace
  stack with the argv/envp/auxv image (Part 3), and (2) the
  bootstrap path in Part 7 (no separate use). All other image
  bytes (text, rodata, data) materialise lazily through
  `fault_script` post-swap — V2 is *not* on that path.

#### Tests

- `populate_detached_user_range_writes_into_anonymous_stack` —
  build an aspace with a single anon stack segment via V1;
  populate a 4 KiB region; verify by reading the page back via
  the kernel direct map.
- `populate_detached_user_range_crosses_pages_in_anon_recipe` —
  populate a 9 KiB region spanning three pages.
- `populate_detached_user_range_returns_efault_outside_recipe`
  — write to an address outside any registered recipe; expect
  `Err(Errno::EFAULT)`.
- `populate_detached_user_range_propagates_alloc_failure` —
  use a test allocator that fails after N pages; expect
  `Err(Errno::ENOMEM)` and partial-write rollback (page
  registrations rolled back; the script returns the address
  range to its pre-call state).

### P1 — Per-fd CLOEXEC sweep helper

The Process-side helper that, given the new fd table state,
clears any fds whose CLOEXEC bit is set. The CLOEXEC bitmap
itself lands in Part 2; this helper is the call site exec uses.

#### Surface

- Add to `crates/tx-subsystems/src/process/execution.rs`:
  ```text
  pub fn step_close_cloexec_fds(
      process: &Cap<ProcessIdentity>,
      guard: &Guard<'_>,
  )
  ```
- Walks `payload.fds` from index 0 to `FD_TABLE_SIZE - 1`;
  for each `Some(file)` whose CLOEXEC bit is set in the new
  `payload.fd_cloexec` field (Part 2), drops the entry and
  clears the bit. The drop runs the existing
  `Cap<OpenFile>::Drop` which closes the open description per
  `txdoc:VFS-CHECKS-WALKER-MODES-1`. Phase 7 — infallible by
  EXEC-PONR.
- Side effect: leaves `payload.fds[i] == None` and
  `payload.fd_cloexec` cleared for every closed slot.

#### Tests

- `step_close_cloexec_fds_drops_marked_fds_only` — preopen
  three fds, set CLOEXEC on one, run; assert only that slot
  is `None`.
- `step_close_cloexec_fds_is_noop_when_no_cloexec_set`.

### P2 — Signal disposition reset

Per `txdoc:EXEC-16-SIGNAL-RESET-SEMANTICS`: every user-installed
handler (that is, anything other than SIG_DFL or SIG_IGN)
resets to SIG_DFL. SIG_IGN dispositions are preserved. Mask is
preserved. Pending is preserved. Altstack is cleared.

#### Surface

- Add to `crates/tx-subsystems/src/signal.rs` (or wherever the
  `SigActionTable` lives):
  ```text
  impl SigActionTable {
      pub fn step_reset_for_exec(&self, guard: &Guard<'_>);
  }
  ```
- Walks all 31 entries; for each entry whose disposition is
  `SigDisposition::Handler(_)`, replaces with
  `SigDisposition::Default`. Preserves `SigDisposition::Default`
  and `SigDisposition::Ignore`. Phase 7 — infallible.
- Companion: `ThreadPayload::step_clear_altstack(&self)` for
  the per-thread alt-stack slot. (The pending queue is *not*
  touched per the spec.)

#### Tests

- `sig_action_table_step_reset_for_exec_clears_handlers` —
  install handlers for SIGUSR1, SIGUSR2, SIGINT; reset; assert
  all three are `Default`.
- `sig_action_table_step_reset_for_exec_preserves_sig_ign` —
  set SIGPIPE to `Ignore`; reset; assert it stays `Ignore`.
- `sig_action_table_step_reset_for_exec_preserves_mask_and_pending`
  — populate the per-thread mask + queue; reset; assert
  unchanged.

### P3 — `brk_base` reset

Phase 7's per-process commit that re-seeds `brk_base` /
`current_brk` from the new image's `brk_start`.

#### Surface

- Add to `crates/tx-subsystems/src/process/execution.rs`:
  ```text
  pub fn step_install_brk_for_exec(
      process: &Cap<ProcessIdentity>,
      brk_start: u64,
      guard: &Guard<'_>,
  )
  ```
- Sets `payload.brk_base = brk_start` and
  `payload.current_brk = brk_start` (both `AtomicU64::store`,
  Release ordering). Phase 7 — infallible.

#### Tests

- `step_install_brk_for_exec_overwrites_prior_values` —
  preset both to non-zero; install a fresh value; assert
  observable through `payload.brk_base()` /
  `payload.current_brk()`.

## Part 2 — Per-fd CLOEXEC bitmap + fcntl(F_SETFD) + O_CLOEXEC

Per Open Q decided 2026-05-06 (option b). The `ProcessPayload`
fd table today is `[Option<Cap<OpenFile>>; FD_TABLE_SIZE]` with
no CLOEXEC bit. Option (b) ships the right end-state: a parallel
bitmap, the `F_SETFD/F_GETFD` fcntl arm, and `O_CLOEXEC` flag
plumbing through `open` and `step_open`. Unblocks the LTP
fcntl+exec test cluster cleanly.

### Surface

- New field on `ProcessPayload` (in
  `crates/tx-subsystems/src/process/structure.rs:425` next to
  `fds`):
  ```text
  pub(crate) fd_cloexec: AtomicU32,  // bit i = CLOEXEC for fd i
  ```
  `AtomicU32` because `FD_TABLE_SIZE = 8` today; the field
  reserves room for growth to 32. (Once the fd table grows
  beyond 32, this becomes a `[AtomicU64; N]` per the
  conventional bitmap shape.)
- `ProcessPayload::set_fd_cloexec(&self, fd: u8, on: bool)` and
  `ProcessPayload::fd_cloexec_get(&self, fd: u8) -> bool` —
  trivial bit-set/test wrappers via `fetch_or` / `fetch_and`
  with `Acquire/Release`.
- `ProcessPayload::set_fd` (already at `:498`) does **not**
  set the bit — that is the caller's job, mirroring the
  Linux open / fcntl split.
- New `OpenFileFlags` field:
  `crates/tx-subsystems/src/vfs/structure.rs:313` already
  contains `pub struct OpenFileFlags { ... }`. Add a `cloexec:
  bool` field; thread it through
  `OpenFileFlags::new` / `Default` so existing callers stay
  source-compat.
- Update `step_open` (in
  `crates/tx-subsystems/src/vfs/walker.rs:108`) to honour the
  new `cloexec` bit on the `OpenFileFlags` argument: the
  walker itself doesn't touch `ProcessPayload.fd_cloexec` (it
  doesn't see it); the *caller* of `step_open` reads the
  returned `OpenFileFlags` and sets the bit on its own
  `ProcessPayload`. The pattern matches the kernel-side flag
  flow Linux uses.
- Linux ABI bit: `O_CLOEXEC = 0o2000000`. Plumb through:
  - `crates/tx-shims/src/linux_syscall/numbers.rs` — define
    `pub const O_CLOEXEC: u32 = 0o2000000;` next to the other
    open flags (presently absent — `open` syscall arm not yet
    in tree, lands as part of Part 6's syscalls if needed).
  - The exec script's path-resolution call (Part 5) opens
    with `OpenFileFlags { rdonly: true, cloexec: false, ... }`
    — the binary's fd is held only inside the script and
    dropped before phase 7; no exposure to the new
    `fd_cloexec` field.
- New syscall arm `NR_FCNTL = 25`:
  - `crates/tx-shims/src/linux_syscall/numbers.rs` — add the
    constant.
  - `crates/tx-shims/src/linux_syscall/mod.rs` — add the
    dispatch arm at line 200ish, calling
    `sys_fcntl(args, ctx).await`.
  - Subset for v1: `F_GETFD = 1` returns the CLOEXEC bit;
    `F_SETFD = 2` sets it from `arg & FD_CLOEXEC`. Other
    `cmd`s return `Err(Errno::EINVAL)` — full fcntl is a
    follow-up.
- `bind_init_cwd_and_root` (`crates/tx-kernel/src/init.rs:554`)
  preopens fds 0/1/2 via `open_console_for_init`; thread the
  new flag through so all three default to `cloexec: false`.

### Tests

- `process_payload_set_fd_cloexec_round_trip` — set on fd 3,
  read back, clear, read back.
- `step_open_records_cloexec_in_open_file_flags` — invoke
  `step_open(/dev/console, OpenFileFlags { cloexec: true, ... })`;
  the returned `Cap<OpenFile>::flags().cloexec` is `true`.
- `sys_fcntl_F_SETFD_FD_CLOEXEC_sets_bitmap_bit`.
- `sys_fcntl_F_GETFD_returns_FD_CLOEXEC_after_setfd`.
- `sys_fcntl_unsupported_cmd_returns_einval`.
- `step_close_cloexec_fds_clears_marked_only` — combined
  smoke that the fcntl + the P1 sweep agree.

## Part 3 — Stack layout + auxv builder

Bag of helpers that build the initial userspace stack image —
argc, argv*, envp*, auxv*, then the string pool — into a kernel
scratch buffer ready to be written into the detached AS via V2
(Part 1). Only the 6 musl-required auxv entries
(`AT_PHDR`, `AT_PHENT`, `AT_PHNUM`, `AT_PAGESZ`, `AT_RANDOM`,
`AT_NULL`) per the LTP coverage research note.

### Surface

- New module `crates/tx-scripts/src/process/exec/stack.rs`:
  ```text
  pub struct StackImage {
      pub bytes: Vec<u8>,        // the contiguous bytes to write to (sp..sp+len)
      pub initial_sp: UserAddr,  // 16-byte aligned, post-write sp
  }

  pub struct AuxvFacts {
      pub phdr_vaddr: u64,
      pub phent: u16,
      pub phnum: u16,
      pub pagesz: u64,    // = USER_PAGE_SIZE
      pub random_seed: [u8; 16],
  }

  pub fn build_initial_user_stack(
      stack_top: UserAddr,
      argv: &[&[u8]],
      envp: &[&[u8]],
      auxv: &AuxvFacts,
  ) -> Result<StackImage, Errno>
  ```
- Layout (from low → high, growing down from `stack_top`),
  matching `txdoc:EXEC-9-3-POPULATE-THE-INITIAL-USER-STACK`
  and verified against musl's `_start` reads:
  1. argc (8 B, signed long)
  2. argv[0..argc] (8 B each, pointers into string pool)
  3. NULL terminator (8 B)
  4. envp[0..envc] (8 B each)
  5. NULL terminator (8 B)
  6. auxv pairs (16 B each: a_type + a_un.a_val); pairs in
     fixed order `AT_PHDR, AT_PHENT, AT_PHNUM, AT_PAGESZ,
     AT_RANDOM, AT_NULL`. AT_RANDOM's `a_val` is a pointer
     into the string pool's 16-byte AT_RANDOM region.
  7. Padding to 16-byte align.
  8. String pool: argv strings (NUL-terminated), envp strings
     (NUL-terminated), 16-byte AT_RANDOM region.
  9. Stack-top page boundary.
- `initial_sp` is `stack_top - len(bytes)`, then rounded down
  to 16-byte alignment (RV64 psABI hard contract; musl's
  `_start` re-aligns defensively, but a misaligned sp leaves
  the value at `[sp]` not equal to `argc`).
- Empty-argv path (CVE-2021-4034): if `argv.is_empty()`,
  synthesise a single zero-byte string `b""` so argc=1 and
  argv[0] is a valid pointer. The spec hook is
  `txdoc:EXEC-WHAT-THIS-DOCUMENT-PINS` "kernel synthesises a
  dummy argv[0] when caller passes empty argv."
- Argv-byte-cap: each individual string ≤ `ARGV_STRING_MAX =
  4096`; total `argv + envp + pool` size ≤ `STACK_IMAGE_MAX
  = 8192`. Both caps enforced inside the helper, returning
  `E2BIG` on overflow. The ARG_MAX (128 KiB on Linux) is the
  long-term target; the v1 cap is the same `TTY_WRITE_MAX_INLINE
  = 4096` discipline pre-ELF Phase 5 introduced. Documented
  under Out of scope.

### Tests

- `build_initial_user_stack_layout_matches_musl_expectations`
  — run with one argv and one envp; manually decode the
  bytes; assert argc-at-sp, argv*-after-argc, envp*-after-NULL,
  auxv-pairs-after-NULL, AT_NULL terminator, AT_RANDOM points
  into the pool's 16-byte tail, sp is 16-byte aligned.
- `build_initial_user_stack_synthesises_dummy_argv_on_empty`
  — empty argv → assert argc == 1 and argv[0] points at a
  NUL-byte.
- `build_initial_user_stack_returns_e2big_on_oversized_argv`.
- `build_initial_user_stack_aligns_sp_to_16` — fuzzing the
  argv/envp byte counts produces sp values all ending in 4
  zero bits.
- `build_initial_user_stack_emits_six_auxv_entries` — assert
  exactly six AT pairs (5 facts + AT_NULL terminator); no
  AT_HWCAP, no AT_PLATFORM, no AT_BASE.

## Part 4 — ELF parser binding (goblin)

Wrapper module that turns a parsed `goblin::elf::Elf` into a
kernel-owned `ExecImagePlan`. Slice only needs PHDR walk + LOAD
segments + entry point. No dynamic linker, no relocations, no
debug info. `goblin = { version = "0.10", default-features =
false, features = ["alloc", "elf64", "endian_fd"] }` per the
research note's pinned line; add to
`crates/tx-scripts/Cargo.toml`. Static-`ET_EXEC` only for the
slice; static-`ET_DYN`/PIE deferred (see Out of scope).

### Surface

- New module `crates/tx-scripts/src/process/exec/loader.rs`:
  ```text
  pub struct ExecImagePlan {
      pub entry: UserAddr,
      pub phdr_vaddr: u64,
      pub phent: u16,
      pub phnum: u16,
      pub segments: Vec<LoadSegment>,
      pub flags: ImageFlags,
  }

  pub struct LoadSegment {
      pub map_start: UserAddr,        // page-rounded down
      pub map_end: UserAddr,          // page-rounded up to mem_size
      pub file_size: u64,             // bytes from file (≤ mem_size)
      pub mem_size: u64,
      pub file_page_offset: u64,      // page-rounded
      pub page_delta: u64,            // map_start ≡ p_vaddr - page_delta
      pub prot: VmProt,
  }

  pub struct ImageFlags {
      pub is_pie: bool,               // false for the slice
      pub has_pt_tls: bool,
      pub has_pt_interp: bool,        // must be false post-validation
      pub stack_executable: bool,     // must be false post-validation
  }
  ```
- Two parse entry points:
  ```text
  pub async fn read_and_parse_image_plan(
      pc: &PageContainer,
      guard: &Guard<'_>,
  ) -> Result<(ExecImagePlan, ImageFlags), Errno>
  ```
  Body:
  1. `read_exact_at(pc, 0, &mut header_buf[..64], guard).await`.
  2. `goblin::elf::header::Header::parse(&header_buf)` →
     `goblin::error::Result<Header>`. Map errors to
     `Errno::ENOEXEC`.
  3. Validate (`txdoc:EXEC-8-4-HEADER-VALIDATION`):
     `e_ident.class == ELFCLASS64`, `e_ident.data == ELFDATA2LSB`,
     `e_machine == EM_RISCV`, `e_type == ET_EXEC` (slice
     scope), `e_phoff > 0`, `e_phentsize == 56`, `e_phnum > 0`
     and `≤ 64` (a hard cap to keep the targeted-read bounded).
  4. `read_exact_at(pc, header.e_phoff, &mut phdrs_buf[..N], guard).await`
     where `N = e_phentsize * e_phnum ≤ 3584`.
  5. For each phdr (`goblin::elf::program_header::ProgramHeader::parse`),
     dispatch by `p_type`:
     - `PT_LOAD` → produce a `LoadSegment` per the validation
       table (`txdoc:EXEC-8-5-PROGRAM-HEADER-VALIDATION`):
       file/mem-size ordering, ELF congruence
       (`p_vaddr ≡ p_offset (mod page_size)`), 4 KiB
       alignment, no W+X prot, prot != 0.
     - `PT_TLS` → record `has_pt_tls = true`. (The slice
       does *not* materialise per-module TLS; musl
       self-installs tp from `AT_PHDR` at runtime per the LTP
       coverage research note.)
     - `PT_INTERP` → reject (`Errno::ENOEXEC`); slice is
       static-only.
     - `PT_GNU_STACK` with `PROT_EXEC` → reject
       (`Errno::ENOEXEC`).
     - `PT_PHDR` → record `phdr_vaddr` (overrides the
       AT_PHDR computation when present per
       `txdoc:EXEC-8-6-AT-PHDR-COMPUTATION`).
     - All other types ignored.
  6. AT_PHDR fallback: if no `PT_PHDR` was seen,
     `phdr_vaddr` = the first LOAD segment's `map_start +
     header.e_phoff - first_load.file_page_offset`.
  7. Reject overlapping page-rounded LOAD ranges.
  8. Return the assembled `ExecImagePlan`.
- Goblin types stay entirely behind `loader.rs`. Translation
  into `ExecImagePlan` happens inline; nothing past this
  module knows what crate parsed the ELF (a future swap to
  the `elf` crate per the spec's footnote leaves the contract
  unchanged).

### Tests

- `loader_parses_simple_static_elf_into_image_plan` — feed a
  fixed byte slice (a known minimal RV64 static ELF, checked
  in as a test fixture under
  `crates/tx-scripts/tests/fixtures/hello-static-rv64.elf` —
  see Part 7 for the fixture-build process); assert
  segment count, entry, phnum.
- `loader_rejects_pt_interp_with_enoexec`.
- `loader_rejects_w_x_segment_with_enoexec`.
- `loader_rejects_overlapping_loads_with_enoexec`.
- `loader_rejects_e_machine_x86_with_enoexec`.
- `loader_rejects_e_type_dyn_with_enoexec` — slice is
  static-`ET_EXEC` only.
- `loader_uses_pt_phdr_when_present_for_phdr_vaddr`.
- `loader_falls_back_when_pt_phdr_absent`.
- `loader_records_pt_tls_without_consuming_it`.

## Part 5 — exec script

The `exec_script` async function in `tx-scripts` that
orchestrates the eight phases of `EXEC_v1` for v1 (static-only,
leader-only, no setuid, no PT_INTERP). Lives at
`crates/tx-scripts/src/process/exec/mod.rs` per
`txdoc:EXEC-2-1-MODULE-PLACEMENT`.

### Surface

```text
pub async fn exec_script<P: TxPlatform>(
    process: Cap<ProcessIdentity>,
    thread:  Cap<ThreadIdentity>,
    path:    &[u8],
    argv:    &[&[u8]],
    envp:    &[&[u8]],
    cred:    &Credential,
    guard:   &Guard<'_>,
) -> Result<core::convert::Infallible, Errno>
```

Returns `Result<!, Errno>`: pre-PoNR errors map back through
the syscall dispatch as a normal `Err(errno)`; post-PoNR the
function never returns (control resolves to the next userspace
re-entry via the production reactor loop). The
`Result<!, Errno>` shape requires either a stable `!` (not yet
on stable Rust) or `core::convert::Infallible`; use
`Infallible` and document that `Ok(_)` is uninhabited.

Body, phase-by-phase, citing `txdoc:EXEC-4-2-PHASE-SUMMARY-TABLE`:

1. **Phase 0 — prelude** (`txdoc:EXEC-5-PHASE-0-PRELUDE`).
   Snapshot the calling thread's `payload`, `aspace_cap`,
   `cred`. Confirm leader-only (slice constraint): assert the
   calling thread is `process.threads()[0]`.
2. **Phase 1 — resolve and mount-policy**
   (`txdoc:EXEC-6-PHASE-1-RESOLVE-AND-MOUNT-POLICY`). Walk
   `path` via
   `vfs::walker::step_open(rooted_at, path, OpenFileFlags { rdonly: true, cloexec: false, ... }, 0, cred, guard).await`.
   `rooted_at` is `process.cwd_or_root()` (the existing
   accessor on `ProcessPayload`); leading `/` already means
   "absolute" per the walker's contract. The returned
   `Cap<OpenFile>` is held in the script's local until
   phase 4 finishes; after that it lives only as the `exe_file`
   reference (phase 7) — not a `ProcessPayload` fd.
3. **Phase 2 — authorization and credential plan**
   (`txdoc:EXEC-7-PHASE-2-AUTHORIZATION-AND-CREDENTIAL-PLAN`).
   Slice is no-setuid; `compute_exec_credentials(file, cred)`
   returns `cred` unchanged. Just record the pass-through.
4. **Phase 3 — load executable image plan**
   (`txdoc:EXEC-8-PHASE-3-LOAD-EXECUTABLE-IMAGE-PLAN`). Call
   `read_and_parse_image_plan(file_pc, guard).await` from
   Part 4. `file_pc` comes from the open file's RNode's
   `RNodeBacking::PageBacked { pc }`.
5. **Phase 4 — prepare detached replacement**
   (`txdoc:EXEC-9-PHASE-4-PREPARE-DETACHED-REPLACEMENT`).
   - `let new_aspace = vm::scripts::build_aspace_from_image::<P>(Some(file_pc.clone()), &image_plan, guard).await?;`
   - Build the stack image via Part 3:
     `let stack_image = build_initial_user_stack(stack_top, argv, envp, auxv_facts)?;`
     where `stack_top = USER_STACK_TOP_DEFAULT` (a new const
     in `tx-subsystems::vm`, doc-target
     `txdoc:EXEC-9-3-POPULATE-THE-INITIAL-USER-STACK`) and
     `auxv_facts` is built from the `image_plan` plus a 16-byte
     `AT_RANDOM` region of constant `[0; 16]` (Open Q #1
     DECIDED 2026-05-06; CSPRNG landed 2026-05-07 on chore
     branch `chore/csprng-at-random`).
   - Write the stack into the detached AS:
     `vm::scripts::populate_detached_user_range::<P>(&new_aspace, stack_image.initial_sp, &stack_image.bytes, guard).await?;`
   - Last reversible point. Past here the EXEC-PONR invariant
     applies.
6. **Phase 5 — collapse old-AS work** (no-op for v1, since
   slice has no `CLONE_FILES` / `CLONE_SIGHAND` and the fd
   table / sig_actions are in-place owned by the calling
   process). Documented as a comment that names the spec
   anchor.
7. **Phase 6 — address-space visibility boundary**
   (`txdoc:EXEC-11-PHASE-6-ADDRESS-SPACE-VISIBILITY-BOUNDARY`).
   The single irreversible store. Two writes, both atomic,
   both infallible:
   - `process.replace_aspace(new_aspace)` — a new method on
     `ProcessIdentity` (not `ProcessPayload`) that does an
     `AtomicSlot::store(Some(new_aspace))` on the existing
     `aspace` field, returning the previous `Cap<AddressSpace>`
     for EBR-deferred drop. Today the field is
     `pub(crate) aspace: Cap<AddressSpace>` at
     `crates/tx-subsystems/src/process/structure.rs:383`,
     written exactly once by `bootstrap_init_process`. **The
     slice flips the field type from `Cap<AddressSpace>` to
     `tx_substrate::AtomicSlot<Cap<AddressSpace>>`** (Open Q
     #2 DECIDED 2026-05-06 — same atomic-replace surface
     `cwd` and `current_payload` use). The conversion lands in
     Part 1A; every reader of `process.aspace_cap()` switches
     from `&Cap` to `slot.load()`-shaped access. Old aspace's
     `Drop` runs after the swap (the EBR guard defers
     reclamation per ZONE/EBR rules).
   - `thread.payload().store_saved_user_context(Some(UserTrapContext { pc: image_plan.entry, sp: stack_image.initial_sp, gprs: [0; 31], tp: 0, .. }))` —
     the existing setter at
     `crates/tx-subsystems/src/thread_runtime/structure.rs:192`.
8. **Phase 7 — install per-frame replacements**
   (`txdoc:EXEC-12-6-INSTALL-USER-TRAP-CONTEXT` covers the
   trap-context part; the full phase 7 includes the
   following infallible commits, all running under
   EXEC-PONR):
   - `step_close_cloexec_fds(&process, guard)` (Part 1, P1).
   - `process.payload().sig_actions().step_reset_for_exec(guard)`
     (Part 1, P2).
   - `step_install_brk_for_exec(&process, image_plan.brk_start, guard)`
     (Part 1, P3) where `image_plan.brk_start` is the
     page-rounded end of the highest data segment, computed
     during phase 4 from `image_plan.segments`.
   - Clear `process.payload().group_pending` of any pending
     `group_exit` indication (cross-doc edit P1 in research
     note's labelling — the `group_exit` clear, separate from
     the CLOEXEC sweep). Note this is *not* the same P1 as
     the close-on-exec sweep; the research note's labelling
     overlaps. We disambiguate by calling this **P-GROUPEXIT**
     internally and folding the actual call into the same
     phase-7 commit list.
   - exe_file / cmdline writes (P2/P3 in the research note's
     scaffolding labelling): **deferred for the slice** —
     procfs is not in scope, no consumer exists, the slots
     `exe_file: AtomicSlot<Option<ExecutableImageRef>>` and
     `cmdline: ExecCmdlineSnapshot` are added but left empty
     for v1. Their absence is harmless; they land for free
     when procfs ships. Documented under Out of scope.
9. **Phase 8 — userspace re-entry**
   (`txdoc:EXEC-12-6-INSTALL-USER-TRAP-CONTEXT`). The script
   does not directly re-enter userspace — the production
   reactor loop's next iteration's
   `prepare_userspace_entry_payload` reads the freshly-stored
   `saved_user_context` and the platform's
   `enter_userspace_with_context` resumes with pc=entry,
   sp=stack-top, gprs=zeroed. The script's job is done; it
   returns `Result<!, Errno>` such that the syscall arm sees
   an "exec committed" outcome.

The function is `pub`. No `IdentRef<'g, _>` crosses an `.await`
inside it — every async call site takes a fresh guard via
`tx_substrate::epoch::guard()` per the cross-await discipline
already established by `vm::execution::fault_script`.

### Wiring with the production reactor loop

The first iteration of `run_thread::<P>` for init will see
`saved_user_context` set up by exactly one of two paths:

**(a) Bootstrap path.** `init.rs` runs
`exec_script::<P>(init_process, init_thread, b"/init", &[], &[], &cred, &guard).await`
synchronously after `bind_init_cwd_and_root`, before
`run_userspace_reactor_loop`. The bootstrap helper drives the
exec future to completion via a single-poll synchronous adapter
(no Reactor needed — there is no other task). Once the future
returns, `init_thread.payload().saved_user_context()` is
populated; `run_userspace_reactor_loop` then submits the
thread future as before. **The plan picks (a).** It reuses the
existing reactor loop unchanged; the only init.rs change is
inserting one synchronous poll-to-completion call. See Part 7.

**(b) Deferred path.** init.rs uses `run_thread` with a
"kernel-only" initial state; an early-init kernel task issues
`NR_EXECVE` synthetically before handing back. More complex
(needs a kernel-side syscall-issue mechanism that doesn't
trap); skip.

### PoNR discipline

Per `txdoc:EXEC-15-THE-EXEC-PONR-INVARIANT`, no allocation, no
user-memory access, no I/O, no fallible computation past the
phase-6 store. The script enforces this by: (1) all `?`-bearing
`.await` and Result-returning calls live in phases 0-4; (2)
phase 6 is two atomic stores; (3) phase 7 calls only the P1 /
P2 / P3 / P-GROUPEXIT commit primitives, each marked infallible
in their docstring; (4) the script returns
`Result<Infallible, Errno>` so the type system witnesses the
post-PoNR irrevocability — the only path past phase 6 is an
infinite-loop branch into the phase-7 commits and then a
`unreachable!()` via the `Infallible` ok-arm.

A property-test: `exec_script_phase6_post_swap_no_alloc` runs
the script under a test allocator that panics on every alloc
call after phase 6, scripted via a phase-tracker helper. The
test must run to completion without a panic.

### Tests

- `exec_script_loads_fixture_binary_seeds_saved_user_context`
  — register the fixture binary into tmpfs at `/init`; run
  `exec_script(init, init_thread, b"/init", &[], &[])`; assert
  `init_thread.payload().saved_user_context()` is `Some(_)`
  with the binary's `e_entry` and a 16-byte-aligned sp.
- `exec_script_replaces_aspace_atomically` — observe `process.aspace_cap()`
  before and after; assert distinct `Cap`s.
- `exec_script_resets_signals_to_default` — pre-install a
  user handler for SIGUSR1; run; assert `Default`.
- `exec_script_resets_brk_from_image_plan` — assert
  `process.payload().brk_base()` equals the highest data
  segment's page-rounded end.
- `exec_script_closes_cloexec_fds_only` — preopen fd 0 (no
  CLOEXEC) and fd 3 (CLOEXEC); run; assert fd 0 still set,
  fd 3 cleared.
- `exec_script_returns_enoent_for_missing_path`.
- `exec_script_returns_enoexec_for_non_elf_bytes` — register
  a file with `b"#!/bin/sh"` (or any non-ELF magic); assert
  `Errno::ENOEXEC` (shebang handling deferred — the file is
  rejected at header validation).
- `exec_script_phase6_post_swap_no_alloc` — see PoNR
  discipline.
- `exec_script_pre_swap_error_leaves_old_aspace_intact` —
  trigger an `ENOEXEC` in phase 4; assert
  `process.aspace_cap()` is unchanged.

## Part 6 — NR_EXECVE syscall arm

The userspace-visible entry point. Calls Part 5's `exec_script`.

### Surface

- `crates/tx-shims/src/linux_syscall/numbers.rs` — add
  `pub const NR_EXECVE: u64 = 221;` (Linux RV64 generic ABI;
  the current set tops out at `NR_RT_SIGPROCMASK = 135`).
- `crates/tx-shims/src/linux_syscall/mod.rs` — add the
  dispatch arm at the existing match block (line 200ish):
  ```text
  NR_EXECVE => sys_execve(req.args, ctx).await,
  ```
- New function in the same file:
  ```text
  async fn sys_execve<'a>(
      args: SyscallArgs,
      ctx: &SyscallCtx<'a>,
  ) -> SyscallResult
  ```
  Body:
  1. Argument parse: `path = UserAddr(args.0)`,
     `argv = UserAddr(args.1)`, `envp = UserAddr(args.2)`.
  2. Bounded user-buffer copy of `path` into a kernel scratch
     buffer (max length `EXEC_PATH_MAX = 4096`, returns
     `ENAMETOOLONG` on overflow). Reuses the existing
     `UserAccessIf::copy_from_user_cstring` shape (the `write`
     syscall's bounded-copy pattern).
  3. Bounded user-buffer copy of `argv` and `envp` arrays:
     each is a NULL-terminated array of `UserAddr` pointers,
     each pointer points at a NUL-terminated string. Caps:
     `ARGV_VEC_MAX = 64` pointers (per side); per-string
     `ARGV_STRING_MAX = 4096`; total `argv + envp` byte
     budget = `EXEC_ARG_BYTE_MAX = 8192`. Overflow at any
     bound returns `E2BIG`.
  4. Call `exec_script::<P>(ctx.process(), ctx.thread(), &path_buf, &argv_buf, &envp_buf, &ctx.cred(), &guard).await`.
  5. The script's `Result<Infallible, Errno>` translates: on
     `Err(errno)` return `SyscallResult::Error(errno_to_i32(errno))`;
     on the (uninhabited) `Ok` arm,
     `SyscallResult::ExecCommitted` — a new variant on the
     enum at `crates/tx-shims/src/linux_syscall/mod.rs:170-185`
     that the dispatch caller (the thread future's syscall
     arm in `crates/tx-kernel/src/thread_future.rs`) treats
     as "do not drain `pending_syscall_return`; the saved
     user context already encodes the new program's
     entry/sp."
- The thread future's syscall arm needs one new branch in
  `crates/tx-kernel/src/thread_future.rs`:
  ```text
  SyscallResult::ExecCommitted => {
      // payload.saved_user_context already updated by the
      // exec script; no `pending_syscall_return` to drain.
      // Loop to userspace re-entry directly.
      continue;
  }
  ```

### Argument byte-cap

- Linux ARG_MAX is 128 KiB; v1 caps at `EXEC_ARG_BYTE_MAX =
  8192` to keep the inline-buffer discipline matching
  `TTY_WRITE_MAX_INLINE = 4096`. TODO marker
  `// TODO(phase-userva): lift to 128 KiB once general
  copy_from_user lands.` in the dispatch site. Documented in
  the open-questions list.

### Tests

- `sys_execve_argv_envp_round_trip_lands_on_stack` — issue
  `execve("/init", &["arg0", "arg1"], &["FOO=bar"])`; the
  loaded binary's stack image (read back from the new AS)
  contains argc=2, argv=["arg0", "arg1"], envp=["FOO=bar"].
- `sys_execve_returns_enoent_for_missing_path`.
- `sys_execve_returns_enoexec_for_bad_magic`.
- `sys_execve_returns_enametoolong_for_oversized_path`.
- `sys_execve_returns_e2big_for_oversized_argv_byte_count`.
- `sys_execve_returns_efault_for_invalid_user_pointer` — the
  classic `path = UserAddr(0)` test.
- `sys_execve_synthesises_dummy_argv_when_caller_passes_empty`
  — empty user argv pointer (NULL); assert the loaded
  binary sees argc=1, argv[0]=b"".

## Part 7 — Bootstrap exec of /init in init.rs

The smallest `init.rs` change: between `bind_init_cwd_and_root`
and `run_userspace_reactor_loop`, register the embedded fixture
binary into tmpfs at `/init`, call
`exec_script(init_process, init_thread, b"/init", &[], &[])`,
drive it to completion synchronously, and proceed. The fixture
binary is a built-in static-musl ELF compiled into the kernel
image as `&[u8]` — choice (a) per the brief; the initramfs cpio
path is deferred (choice (b)).

### Where /init comes from

A static-musl-linked RV64 hello-world binary, compiled
externally (the txKernel workspace itself is `no_std` and
cannot host a musl link), checked into the repo as
`crates/tx-kernel/fixtures/init.elf`. The build process:

```text
$ riscv64-linux-musl-gcc -static -Os -o init.elf init.c
$ cp init.elf /Users/3y/Downloads/Tx/.claude/worktrees/funny-hugle-06199b/crates/tx-kernel/fixtures/
```

`init.c`:

```text
#include <unistd.h>
int main(void) {
    write(1, "hello\n", 6);
    return 0;
}
```

The cross toolchain is the `riscv64-linux-musl-cross` tarball
from `<https://musl.cc/>` (or the upstream
`riscv-collab/riscv-gnu-toolchain` `make musl` target). Build
process documented in
`crates/tx-kernel/fixtures/README.md` (one-time setup). The
binary lands ~25 KiB; checked in as a binary blob with a
version-pinned hash in the README.

`crates/tx-kernel/src/init.rs` includes via:
```text
static INIT_FIXTURE_BYTES: &[u8] =
    include_bytes!("../fixtures/init.elf");
```

### Bootstrap insertion

Add to `CoreInit::init_substrate_if_ready` (or the existing
chain after `bind_init_cwd_and_root`):

```text
Self::register_init_fixture_into_tmpfs();
Self::run_bootstrap_exec_for_init();
Self::run_userspace_reactor_loop();
```

- `register_init_fixture_into_tmpfs` creates a tmpfs RNode at
  `/init` with `FsPageBacking` backed by the embedded byte
  slice. The tmpfs surface already supports
  `FsObjectId`-keyed file creation (per pre-ELF Phase 4).
  The fixture is read-only.
- `run_bootstrap_exec_for_init` builds a short-lived guard
  and credential, polls the `exec_script` future to
  completion via a `core::future::poll_fn` adapter (no
  Reactor; the only `.await` points are page-pulls through
  `read_exact_at`, which under tmpfs are immediate), and
  **panics with a `:bootstrap-exec:fail` board sentinel on
  `Err`** (Open Q #3 DECIDED 2026-05-06 — boot-time invariant
  violation; the fixture is built-in and a load failure means
  the kernel image is broken; CI must catch loudly. No
  fallback to the trio's hand-built path — that path was
  retired by Phase 7 of pre-ELF and reintroducing it would
  re-create the dual code path the trio just collapsed).

### Tests

- `bootstrap_exec_seeds_init_saved_user_context_before_reactor_loop`
  — extends the trio's host smoke. Assertion sequence:
  1. `register_init_fixture_into_tmpfs` populates `/init`.
  2. `run_bootstrap_exec_for_init` returns `Ok`.
  3. Inspect `init_thread.payload().saved_user_context()`
     and assert it is `Some(_)`.
  4. `run_userspace_reactor_loop` runs.
  5. Console captures `b"hello\n"`.
  6. `init.is_zombie()` with `ExitStatus::Exited(0)`.
- `bootstrap_exec_panics_on_fixture_load_failure` —
  fixture-load mock returns `Err`; assert panic message
  contains "fixture".

## Cross-cutting risks

1. **PoNR violations under demand-faulting.** Phase 4 publishes
   recipes; the recipes are consumed lazily on faults that
   happen *after* the phase-6 swap. Verify that recipe
   publication itself is *not* a fallible step in a post-swap
   commit phase. The plan handles this by registering all
   recipes inside `build_aspace_from_image` (V1, phase 4 —
   pre-PoNR); the phase-6 swap only stores the already-built
   `Cap<AddressSpace>` into `process.aspace`. Every fault
   post-swap goes through the existing `fault_script` path,
   which has its own retry/`WouldBlock` discipline. The
   property-test `exec_script_phase6_post_swap_no_alloc`
   guards this empirically.

2. **Walker-vs-cwd ordering.** `exec_script` resolves the path
   via `step_open` *before* the address space is replaced. By
   the time phase 6 swaps the AS, the binary's `Cap<OpenFile>`
   is held in the script's local stack frame — that Cap holds
   a `Cap<RNode>` whose `RNodeBacking::PageBacked { pc }` is
   the `PageContainer` the new aspace's recipes point at. The
   Cap's lifetime extends through phase 7 (where it is dropped
   after the recipe-build is done). Init's cwd is `/` post
   pre-ELF Phase 6; the resolution still works after the AS
   swap because the resolved Cap survives the swap. The
   subtle case is that `process.cwd()` itself is a
   `Cap<DEntry>` on `ProcessPayload`, which is *not* swapped
   by exec — it survives. Confirmed by `txdoc:EXEC-3-6-PROCESS-COORDINATES-COLLAPSE-AND-PRESERVES-THE-SHELL`.

3. **ARG_MAX caps.** Static-musl test binaries with realistic
   argv/envp may exceed the slice's 8 KiB inline cap — for
   the smoke binary (`hello`, no args) this is fine; for the
   LTP `execve01_child` helper (which receives a multi-arg
   canary) the cap may need lifting before that test runs.
   Flagged for the smoke binary as: *the slice's smoke uses
   empty argv/envp so the cap is moot; LTP coverage is on
   the next slice and will require lifting EXEC_ARG_BYTE_MAX*.

4. **Symbol collision with tx-scripts placeholders.** The
   current `crates/tx-scripts/src/lib.rs` is exactly:
   ```text
   pub mod file_io {} pub mod mount {} pub mod postlude {}
   pub mod prelude {} pub mod process {} pub mod route {}
   ```
   The plan replaces `pub mod process {}` with the real
   module hierarchy `process::exec::{loader, stack, mod}`.
   The empty placeholders for `file_io`, `mount`, `postlude`,
   `prelude`, `route` are kept (they belong to future scripts
   per `txdoc:EXEC-2-1-MODULE-PLACEMENT`); only `process` is
   filled in.

5. **goblin no_std + alloc compatibility on RV64 board.** The
   research note pins `goblin = "0.10"` with
   `default-features = false, features = ["alloc",
   "endian_fd", "elf64", "elf32"]`. Verification: build the
   `tx-kernel-riscv64-qemu-virt` board target with the
   updated `tx-scripts` Cargo.toml and confirm `cargo check
   --target riscv64gc-unknown-none-elf` succeeds. If
   `endian_fd` pulls in `std`-only code, drop to a
   smaller feature set. The implementer should confirm
   `goblin::elf::header::Header::parse` and
   `goblin::elf::program_header::ProgramHeader::parse` are
   both reachable under the chosen feature flags before
   committing the dep — checking
   `crates/tx-scripts/Cargo.toml` is the first action of
   Phase 4 (parse).

6. **`AT_RANDOM` weak-entropy R1 risk — accepted.** v1 sources
   the 16-byte AT_RANDOM region as constant `[0; 16]` (Open Q
   #1 DECIDED 2026-05-06). Any binary that links libssp uses
   this for stack-canary seeding; a constant trivially weakens
   the canary. v1 explicitly accepted the weakness — txKernel
   has no ASLR or stack-canary checks at this stage and the
   binaries the slice runs have no untrusted input. Real
   CSPRNG was a follow-up slice (landed
   2026-05-07 on chore branch `chore/csprng-at-random`:
   `EntropyIf` HAL trait + per-exec fill via
   `AuxvFacts.at_random_bytes`).

7. **`process.aspace` field-shape change — committed.** Open Q
   #2 DECIDED 2026-05-06: flip `ProcessPayload.aspace` from
   `Cap<AddressSpace>` to
   `tx_substrate::AtomicSlot<Cap<AddressSpace>>` (matching how
   `cwd` and `current_payload` are spelled). Adds a
   `replace_aspace(&self, new: Cap<AddressSpace>) ->
   Cap<AddressSpace>` method that returns the previous Cap for
   EBR-deferred drop. Affects every reader of
   `process.aspace_cap()` (already a snapshot, so no
   correctness break — only the writeback discipline changes
   from "set once" to "set-and-replace"). The Part 1A
   conversion includes this; do not skip it for diff size.

8. **Doc-vs-code anchor mismatch on
   `txdoc:EXEC-WHAT-THIS-DOCUMENT-PINS` cross-doc edit
   labelling.** The research note labels six edits B1, V1,
   V2, P1, P2, P3 — but the *spec's* in-document language
   uses different abbreviations: P1 in the spec is the
   `group_exit` clear (not the close-on-exec sweep). This
   plan disambiguates by using research-note labels (Part 1
   sections B1 / V1 / V2 / P1 / P2 / P3) and naming the
   group_exit clear "P-GROUPEXIT" in Part 5 phase 7.
   Implementer should be aware when reading EXEC_v1.md
   directly that the labels do not match 1:1; the doc-anchor
   list above cites the spec's own anchors which are stable.

## Out of scope (deliberately deferred)

- **ET_DYN / PIE binaries.** Static-`ET_EXEC` only for the
  slice. The `load_bias` plumbing lands when the first PIE
  test artefact does (likely the LTP execve01 test phase).
- **PT_INTERP / dynamic linking.** Static binaries only.
- **TLS support beyond what musl self-installs.** The kernel
  records `has_pt_tls` in `ImageFlags` but does not consume
  it; userspace `__init_tls` reads `AT_PHDR` and copies bytes
  itself.
- **Setuid / setgid bits + DAC permission checks.** LTP
  `execve02` needs DAC; deferred. The slice's
  `compute_exec_credentials` returns the snapshot unchanged.
- **Shebang `#!` handling.** LTP `execve` has a shebang case;
  deferred. The slice rejects non-ELF magic at header
  validation with `ENOEXEC`.
- **ETXTBSY (exec-while-open-for-write) blocks.** LTP
  `execve04` self-skips on Linux ≥ 6.11 so absence is OK
  for v1.
- **fork / clone / wait4 syscalls.** Pre-fork slice; the LTP
  `execve05` concurrent-exec test depends on these.
- **Real initramfs cpio unpack.** v1 uses a built-in `&[u8]`
  fixture; cpio is the next-step beachhead once a second
  test binary exists.
- **VDSO (`AT_SYSINFO_EHDR`).** No VDSO yet; static-musl
  tolerates absence.
- **`AT_HWCAP` / `AT_HWCAP2` / `AT_PLATFORM`.** musl
  tolerates absence per the LTP coverage research note's
  per-AT table; v1 omits them.
- **`exe_file` / `cmdline` ProcessPayload slots.** Procfs is
  not a peer; the slots are added (P2/P3 in research-note
  labelling) but left empty for v1.
- **Real RV64 QEMU smoke (`cargo xtask qemu --image`).**
  Picks up after the host smoke proves the loader; needs an
  `--image` flag and a board-side fixture-load shim.
- **`SYS_set_tid_address` / `SYS_set_robust_list` syscall
  arms.** musl ignores ENOSYS for these informational
  syscalls in the static path; the dispatcher's existing
  `_ => Error(ENOSYS_VALUE)` default suffices for the smoke.
  Real arms land when a binary that *needs* a real tid
  arrives (post-pthread).

## Phasing

Each step is a self-contained PR. Land in this order; later
parts depend on earlier ones (Part 5 needs Parts 1+3+4; Part 6
needs Part 5; Part 7 ties together).

1. **M — Part 1 V1 + V2 + B1.** Foundational VM/page_backed
   surface. `read_exact_at`, `build_aspace_from_image`,
   `populate_detached_user_range`. Touches
   `crates/tx-subsystems/src/page_backed/` and
   `crates/tx-subsystems/src/vm/`. Estimated 1 PR, ~600 LOC
   incl. tests.
2. **S — Part 1 P1 + P2 + P3.** ProcessPayload close-on-exec
   sweep helper, signal disposition reset, brk reset.
   Touches `crates/tx-subsystems/src/process/execution.rs`
   and `crates/tx-subsystems/src/signal.rs`. Estimated 1 PR,
   ~200 LOC.
3. **S — Part 2 CLOEXEC bitmap + fcntl + O_CLOEXEC plumbing.**
   `ProcessPayload.fd_cloexec`, `OpenFileFlags.cloexec`,
   `NR_FCNTL` arm with `F_GETFD/F_SETFD`. Touches
   `crates/tx-subsystems/src/process/structure.rs`,
   `crates/tx-subsystems/src/vfs/structure.rs`,
   `crates/tx-shims/src/linux_syscall/`. Estimated 1 PR,
   ~250 LOC.
4. **M — Part 3 stack/auxv builder + Part 4 goblin parser
   binding.** Two modules in `tx-scripts/src/process/exec/`:
   `stack.rs` (no deps) and `loader.rs` (depends on goblin
   + Part 1's `read_exact_at`). Tests share a fixture-load
   helper. Estimated 1 PR, ~700 LOC.
5. **L — Part 5 `exec_script` orchestration + PoNR commit
   phase.** The big one. `tx-scripts/src/process/exec/mod.rs`,
   the `process.replace_aspace` plumbing, the
   phase-tracker test harness for the post-PoNR no-alloc
   property test. Estimated 1 PR, ~900 LOC incl. tests.
6. **M — Part 6 NR_EXECVE syscall arm + tests.**
   `linux_syscall/mod.rs::sys_execve`, the
   `SyscallResult::ExecCommitted` variant, the thread future
   branch, bounded user-buffer copies. Estimated 1 PR,
   ~400 LOC incl. tests.
7. **S — Part 7 bootstrap exec of /init + end-to-end smoke
   + fixture-binary build process.** init.rs hook, the
   fixture binary commit, `crates/tx-kernel/fixtures/README.md`.
   The big payoff: the host-side end-to-end smoke
   (`bootstrap_exec_seeds_init_saved_user_context_before_reactor_loop`)
   demonstrates a real userspace binary printing `hello\n`
   through the production reactor loop. Estimated 1 PR,
   ~150 LOC + the fixture binary blob.

## Open questions

1. **AT_RANDOM source for v1 — DECIDED 2026-05-06: constant
   `[0; 16]`.** The 16-byte AT_RANDOM region was seeded with a
   constant; musl SSP became deterministic but functionally fine
   for static smoke binaries (txKernel has no ASLR or
   stack-canary checks at this stage). Real CSPRNG landed
   2026-05-07 on chore branch `chore/csprng-at-random`: HAL
   `EntropyIf` trait fills `AuxvFacts.at_random_bytes` per exec
   (RV64 mixes `rdtime` + xorshift counter; other boards use the
   deterministic counter default).
2. **`process.aspace` field-shape change — DECIDED 2026-05-06:
   atomic replace via `AtomicSlot<Cap<AddressSpace>>`.** Flip
   the field type from `Cap<AddressSpace>` to
   `tx_substrate::AtomicSlot<Cap<AddressSpace>>` per
   `txdoc:EXEC-11-PHASE-6`. The spec shape and the right end
   state. Larger diff (every reader changes from `&Cap` to
   `slot.load()`-shaped access) but encodes the
   single-store-at-PoNR discipline in the type system instead
   of relying on convention. Affects every reader of
   `process.aspace_cap()`; the slice's Part 1A includes the
   conversion.
3. **Bootstrap exec failure behaviour — DECIDED 2026-05-06:
   panic.** If `run_bootstrap_exec_for_init` fails (malformed
   fixture, `read_exact_at` error, etc.), the kernel panics
   with a board-sentinel `:bootstrap-exec:fail` line. Boot-time
   invariant violation, not a runtime condition; panic loudly
   so CI catches it. Fallback to the trio's hand-built
   userspace path is rejected because that path is being
   retired anyway (Phase 7 of pre-ELF deleted its smoke);
   keeping it as a fallback would re-introduce dual code paths
   the trio just collapsed.
4. **CLOEXEC field-width — accepted default 2026-05-06:
   `AtomicU32`.** Reserves 32 bits for `FD_TABLE_SIZE = 8`
   today; covers up to fd 31 when the table grows. `AtomicU8`
   would match today's table exactly but burns a PR when the
   table grows. Implementation uses `AtomicU32`.
5. **goblin feature-flag — accepted default 2026-05-06:
   `["alloc", "endian_fd", "elf64"]`.** No `elf32` (slice is
   RV64-only). Brief's recommendation kept; research note's
   `elf32` inclusion was conservative and dropped.
