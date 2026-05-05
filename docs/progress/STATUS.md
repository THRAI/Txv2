# txKernel Status

**Updated:** 2026-05-05

## Current Shape

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

- `docs/progress/decisions/2026-05-01-reactor-userspace-entry-ast-checkpoint.md`
- `docs/progress/decisions/2026-05-01-rv64-trapframe-fault-decode.md`
- `docs/progress/decisions/2026-05-01-rv64-trap-frame-writeback.md`
- `docs/progress/decisions/2026-05-01-rv64-timer-trap-idle-smoke.md`
- `docs/progress/decisions/2026-05-01-rv64-saved-trap-dispatch.md`
- `docs/progress/decisions/2026-05-01-coreinit-hart-loop-adapter.md`
- `docs/progress/decisions/2026-04-30-ap-reactor-shared-runqueue-smoke.md`
- `docs/progress/decisions/2026-04-30-ap-reactor-loop-wfi-smoke.md`
- `docs/progress/decisions/2026-04-30-rv64-ipi-ack-smoke.md`
- `docs/progress/decisions/2026-04-30-ap-substrate-before-online.md`
- `docs/progress/decisions/2026-04-30-rv64-smp-rfence-shootdown.md`
- `docs/progress/decisions/2026-04-30-smpif-parked-ap-boot.md`
- `docs/progress/decisions/2026-04-30-reactor-reschedule-dispatch-bridge.md`
- `docs/progress/decisions/2026-04-30-reactor-affinity-wake-placement.md`
- `docs/progress/decisions/2026-04-30-reactor-declared-readiness-channel.md`
- `docs/progress/decisions/2026-04-30-reactor-declared-wait-channel.md`
- `docs/progress/decisions/2026-04-29-ebr-zone-first-executable-slice.md`
- `docs/progress/decisions/2026-04-29-rv64-high-vma-low-lma-linker.md`
- `docs/progress/decisions/2026-04-29-rv64-low-linked-identity-retention.md`
- `docs/progress/decisions/2026-04-28-pageallocator-token-interface.md`
- `docs/progress/decisions/2026-04-29-code-reorganization-skill-and-line-limit.md`
- `docs/progress/decisions/2026-04-29-substrate-slab-heap-zero-frame.md`
- `docs/progress/decisions/2026-04-29-rv64-pmap-helper-extraction.md`
- `docs/progress/decisions/2026-04-29-rv64-pmap-module-extraction.md`
- `docs/progress/decisions/2026-04-29-pmap-kernel-protect-in-place.md`
- `docs/progress/decisions/2026-04-29-rv64-minimal-trap-vector.md`
- `docs/progress/decisions/2026-04-28-substrate-init-frameallocator-handoff.md`
- `docs/progress/decisions/2026-04-29-kernel-shootdown-map-accounting.md`
- `docs/progress/decisions/2026-04-29-pmap-typed-intermediate-source.md`
- `docs/progress/decisions/2026-04-29-pmap-rollback-unmap-vocabulary.md`
- `docs/progress/decisions/2026-04-28-rv64-mmio-pmap-reserve-commit.md`
- `docs/progress/decisions/2026-04-28-rv64-direct-map-extension.md`
- `docs/progress/decisions/2026-04-28-unused-lint-gate.md`
- `docs/progress/decisions/2026-04-28-rv64-identity-teardown-sentinel.md`
- `docs/progress/decisions/2026-04-28-rv64-high-half-entry.md`
- `docs/progress/decisions/2026-04-28-rv64-boot-static-bag.md`
- `docs/progress/decisions/2026-04-28-address-boundary-policy.md`
- `docs/progress/decisions/2026-04-28-rv64-high-half-alias-bootstrap.md`
- `docs/progress/decisions/2026-04-28-finish-catchup-progress-memory.md`
- `docs/progress/decisions/2026-04-28-arceos-aligned-portable-boot.md`
- `docs/progress/decisions/2026-04-27-fine-grained-txdoc-anchors.md`
- `docs/progress/decisions/2026-04-27-xtask-module-split.md`
- `docs/progress/decisions/2026-04-27-xtask-progress-command-surface.md`
- `docs/progress/decisions/2026-04-27-ci-reporting-and-txdoc-tags.md`
- `docs/progress/decisions/2026-04-27-json-agent-operational-records.md`
- `docs/progress/decisions/2026-04-27-humanlayer-reference-and-agentic-workflow.md`
- `docs/progress/decisions/2026-04-27-doc-layout-and-progress-memory.md`

## Latest Research

- `docs/progress/research/2026-05-01-reactor-runtime-dispatch-audit.md`
- `docs/progress/research/2026-05-01-ast-return-to-user-scout.md`
- `docs/progress/research/2026-05-01-coreinit-runtime-loop-scout.md`
- `docs/progress/research/2026-04-30-reactor-readiness-for-subsystems.md`
- `docs/progress/research/2026-04-27-humanlayer-progress-memory.md`
