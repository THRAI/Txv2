# txKernel Status

**Updated:** 2026-04-29

## Current Shape

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
- Four parallel substrate/kernel-main lanes have been merged into the
  controller integration branch `codex/substrate-parallel-integration`:
  reactor smoke, bounded zone/index/mutation primitives, pmap
  root/ASID/shootdown hardening with `MapPinRun`, and trap vocabulary/RV64
  trap module extraction. The branch is tracked by
  `docs/progress/worktrees/2026-04-29-substrate-parallel-integration.json`.
  Next step: integrated verification and publishing; remaining design decision
  is the monomorphic kernel sink bridge for a future saved-register trap shell.
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
- `TrapIf` now includes typed trap snapshots and classification. RV64 QEMU
  decodes common synchronous faults and supervisor interrupts from `scause`;
  the direct-mode vector still panics/spins until the full saved-register
  trap shell and user-return path exist.
- RV64 QEMU now installs a minimal direct-mode `stvec` panic vector before
  boot handoff and again after `init_later()`. The vector prints `scause`,
  `sepc`, and `stval` through the SBI console and spins; the full
  `RawTrapFrame`/`KernelTrapSink` user trap shell remains a later slice.
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
- RV64 QEMU still needs superpage/multi-frame map-count batching, remote-hart
  shootdown coordination, and the full trap shell before the page substrate is
  user/VM-ready.
- ext4 image creation requires host `mkfs.ext4`.
- BusyBox images require `TX_BUSYBOX`; dynamic musl layouts also require
  `TX_MUSL_LIBC`.

## Latest Decisions

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

- `docs/progress/research/2026-04-27-humanlayer-progress-memory.md`
