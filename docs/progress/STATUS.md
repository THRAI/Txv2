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
- Agent cooperation framework research now records both the local HumanLayer
  command/specialist-agent adaptation and a follow-up mature-framework scan for
  Codex-without-native-subagents. The current recommendation is a Tx-owned
  cooperation protocol plus an optional external LangGraph-style runner, MCP
  tool surface, A2A service boundary only if needed, and Temporal only for
  durable long-running execution. A local Codex 0.125.0 probe confirmed both
  `codex exec` subprocess execution and `codex mcp-server` tools (`codex` and
  `codex-reply`) as practical runner integration surfaces. See
  `docs/progress/research/2026-04-29-agent-cooperation-framework.md`.
  The same note now clarifies that LangGraph should live in an external Tx
  runner: Tx publishes plans, the runner instantiates the graph, Codex executes
  bounded nodes, and progress artifacts advance the graph.
  A follow-up section now states that LangGraph is sufficient as the
  orchestration core but not the whole command system; Tx still needs a runner
  with plan compilation, Codex worker adapters, worktree/scope leases, artifact
  protocol, verifier/reviewer gates, human control, budget policy, and recovery.
- The Tx Parallel Agent Runner MVP now exists under `tools/agent-runner` as a
  standalone `uv`/LangGraph project. It loads active worktree records, validates
  path/branch/write-scope leases, compiles bounded Codex worker prompts, runs
  dry-run or `codex exec` fanout with a configurable worker cap, captures
  prompt/event/final/summary artifacts under `target/tx-agent-runs/`, and runs
  `cargo xtask progress validate` after real worker completion. Verification:
  the agent-runner pytest suite, active-worktree inspect, active status dry-run
  with `--max-workers 2`, safe non-dry smoke through the local fake Codex
  adapter, `cargo xtask progress validate`, `cargo xtask lint docs`, and
  `git diff --check`. Next step: run an optional real one-worktree status smoke
  only with explicit external-service approval; no implementation blocker.
- Architecture-parallelism research now records that Zone/EBR is the main
  semantic-entity unlock, but safe subsystem fanout also needs index/mutation,
  bus, credit, and a minimal reactor/waker shell. The current coarse
  `kernel_main` plan reaches width 3; finer subsystem/module splits can
  plausibly support 3-5 implementation lanes after substrate, with wider
  read-only research and narrower final integration. See
  `docs/progress/research/2026-04-29-architecture-parallelism-after-zone-ebr.md`.
- `rsext4` is available as the `external/rsext4` submodule, pinned to upstream
  `Starry-OS/rsext4` commit `984201f`. It is a source baseline for future
  async, uncached `tx-ext4` adaptation work and is not wired as a workspace
  dependency yet. Verification: `git submodule status`,
  `git -C external/rsext4 log -1 --oneline`, and
  `cargo xtask progress validate`. Next step: reshape the library around the
  active filesystem docs before integrating it into `crates/tx-fs`; no blocker.
- `tx-ext4-format` now exists as a host-testable, no-kernel-interface workspace
  crate. It provides reusable on-disk ext4/JBD2 parsers, CRC32C plus ext4
  metadata-checksum chaining helpers, 32/64-bit group descriptor parsing,
  multi-group inode-table location, recursive extent-index resolution,
  directory entry and HTree/DX record parsing, bitmap allocation primitives,
  and a thin `BlockImage` pager that can return inode metadata, read 4 KiB
  pages with hole/EOF zero-fill, write back existing mapped pages, list and
  look up directory entries, and journal/replay inode metadata blocks on mock
  images.
  Optional host-tool verification creates a real ext4 image with `mkfs.ext4`,
  populates `/folder/hello.txt` with `debugfs`, verifies it with
  `dumpe2fs`/`e2fsck`, and compares pager folder/file observations against
  `debugfs` when those tools are installed; the current host lacks those tools,
  so that path compiles and skips locally. Verification:
  `cargo test -p tx-ext4-format`,
  `cargo test -p tx-ext4-format --test host_tools -- --nocapture`,
  `cargo fmt -p tx-ext4-format -- --check`,
  `cargo tree -p tx-ext4-format`, static grep for rsext4/kernel-interface
  imports, scoped `git diff --check`, `cargo xtask progress validate`, and
  `cargo xtask lint docs`. Next step: add host-generated fixtures for htree,
  sparse, and multi-level extent images when e2fsprogs is available, then add
  the thin txKernel async adapter; no blocker.
- `tx-ext4` now exists as the host-first async adapter crate above
  `tx-ext4-format`. Its `host_async` layer defines an `AsyncBlockDevice`
  Future-returning trait, `FsObjectId`, an `Ext4Async` adapter, a small
  file-page cache, async inode metadata loading, page fetch with cache-hit
  short-circuiting, directory lookup, existing-page writeback, and ordered
  descriptor/payload/commit metadata journaling with test replay. Tokio is
  test-only and used as the mock reactor; production code has no Tokio,
  rsext4, VFS, VM, or kernel-interface imports. Verification:
  `cargo test -p tx-ext4`, `cargo test -p tx-ext4-format`,
  `cargo fmt -p tx-ext4 -p tx-ext4-format -- --check`,
  `cargo tree -p tx-ext4`, `cargo tree -p tx-ext4-format`, static boundary
  greps, scoped `git diff --check`, `cargo xtask progress validate`, and
  `cargo xtask lint docs`. Next step: swap the host Future trait for the real
  `StepOutcome`/reactor wrapper once the VM/VFS traits land; no blocker.
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
- Pre-substrate board smoke boot is now gated by
  `PlatformConfig::SUBSTRATE_BOOT_READY`. RV64 QEMU opts in and still runs
  substrate init, `init_later()`, and the kernel trap-vector install before its
  sentinel. The first LA64 QEMU and RV64 M1 Dock mock wave kept the default
  false gate, moved `_start` into platform crates, preserved firmware registers
  into `rust_entry`, and reached smoke sentinels through board early consoles.
  Verification: `cargo fmt --check`, `cargo xtask check`, all three
  `cargo xtask build --target ...` lanes, all three QEMU smoke sentinel lanes,
  `cargo xtask lint docs`, `cargo xtask progress validate`, and
  `git diff --check`. Follow-up board-worker validation re-ran LA64 and M1
  mock builds/smoke boots and confirmed the sentinels are emitted from the
  generic `tx_kernel::kernel_main` path, not from board-local stubs.
- LA64 QEMU and RV64 M1 Dock mock now publish board-owned memory and
  bootstrap-pmap prep facts as the substrate-smoke prerequisite.
  LA64 describes QEMU `virt` RAM `0x0..0x1000_0000`, reserves the low/kernel
  loaded range, derives `kernel_image` from linker symbols, and exposes an
  identity/direct bootstrap pmap description. M1 mock describes QEMU/OpenSBI
  RAM `0x8000_0000..0x9000_0000`, the firmware loader gap
  `0x8000_0000..0x8020_0000`, linker-derived `kernel_image`, and the current
  identity/direct bootstrap pmap description. The first prep pass deliberately
  left platform MMIO unclaimed until the boards had a clear pmap story.
  Verification: board HAL unit tests, `cargo xtask check`, LA64/M1 builds and
  smoke sentinels, plus RV64 QEMU substrate-ready smoke as a guardrail. The
  follow-ups below record the PT-node allocator handoff and identity-MMIO
  coverage that made substrate smoke stronger.
- LA64 QEMU and RV64 M1 Dock mock now opt into substrate smoke after adding the
  board-local typed PT-node allocator handoff required by
  `tx_substrate::init::<P>()`. `SUBSTRATE_BOOT_READY=true` is now limited to
  RV64 QEMU, LA64 QEMU, and the QEMU/OpenSBI M1 mock; LA64/M1 still expose no
  fake direct-map extension, process-root, or general kernel pmap mutation.
  Verification: board HAL unit tests, `cargo xtask check`, all three board
  build lanes, all three QEMU smoke sentinel lanes, `cargo xtask lint docs`,
  `cargo xtask progress validate`, and `git diff --check`. Next step:
  implement real mapping/MMIO and process-root surfaces before BusyBox or real
  K210 boot claims.
- LA64 QEMU and RV64 M1 Dock mock now publish one UART MMIO region each and
  implement the minimal pmap phase-3 response for the exact page-aligned
  identity mapping already covered by their early boot execution model.
  `PmapIf::reserve_kernel_mapping()` returns `Ok(None)` only for those
  precovered identity UART pages; all other kernel mapping requests remain
  `Unsupported`. Verification: board HAL unit tests plus LA64 and M1 QEMU
  smoke sentinel lanes with nonempty `PlatformInfo.mmio_regions`. Next step:
  replace this identity-precovered bridge with real LA64 DMW/MMU and M1 Sv39
  mutation before enabling drivers or process roots on these boards.
- The RV64 M1 Dock mock now has a high-VMA/low-LMA Sv39 bootstrap pmap. H1
  stays in assembly until `satp` is live, maps QEMU RAM both through a temporary
  low identity leaf and through the high direct map, maps the kernel at
  `0xffff_ffff_8020_0000`, pre-covers the QEMU UART at its high direct-map
  alias, rewrites `sp`/`gp`, and jumps to high `rust_entry`. Verification: M1
  HAL unit tests, target build, `RUSTFLAGS=-Dunused` target check, and M1 QEMU
  smoke. Still no real K210 hardware boot claim.
- The RV64 M1 Dock mock pmap can now grow beyond the preinstalled UART L0:
  high direct-map low-MMIO 4 KiB kernel mappings walk root/L1/L0 tables,
  allocate missing L1/L0 tables through the installed `PtNodeAllocator`, carry
  fresh nodes in `PmapReservationIntermediates`, roll back uncommitted
  intermediates, and commit final leaf PTEs. Verification used a red-first unit
  test for the next UART 2 MiB window, M1 HAL tests, target build,
  `RUSTFLAGS=-Dunused` target check, and M1 QEMU smoke.
- The RV64 M1 Dock mock now has committed low-MMIO kernel pmap lifecycle
  coverage: commit records newly allocated PT-node intermediates, unmap clears
  high direct-map 4 KiB low-MMIO leaves and prunes empty committed L0/L1 tables
  back through their typed frame releasers, protect rewrites same-granularity
  kernel leaves in place, and `shootdown_kernel_mapping()` issues the local
  Sv39 fence on target. Verification: M1 HAL unit tests, scoped fmt check,
  target build, `RUSTFLAGS=-Dunused` target check, M1 QEMU smoke sentinel,
  docs lint, progress validation, and `git diff --check`. Next step: keep LA64
  on its own DMW/MMU path and grow M1 process-root/ASID support without
  claiming real K210 hardware boot.
- The RV64 M1 Dock mock HAL crate is now split so `lib.rs` is a small platform
  facade and `_start`, while `pmap.rs` owns topology constants, boot pmap
  tables, BootInfo/BootstrapPmapInfo publication, the PT-node registry, kernel
  mapping lifecycle hooks. The private pmap tests now live in
  `src/pmap_tests.rs`, keeping current file sizes under the arch-lint cap:
  `lib.rs` 375 lines, `pmap.rs` 1352 lines, and `pmap_tests.rs` 462 lines.
  Verification: M1 HAL tests, target build, `RUSTFLAGS=-Dunused` target check,
  M1 QEMU smoke sentinel, scoped fmt check, and arch lint. Next step: grow
  VM/trap-facing behavior without bloating the platform facade.
- The RV64 M1 Dock mock now implements the `PmapIf` process-root and ASID
  surface for the QEMU/OpenSBI smoke lane. Process roots allocate a typed
  page-table root, copy the bootstrap root's kernel high half, receive/reuse an
  ASID from a fixed bitmap, materialize user 1 GiB / 2 MiB / 4 KiB mappings,
  protect and unmap same-granularity leaves, prune committed user L0/L1 tables,
  and issue local Sv39 fences for ASID-shaped shootdown. The private pmap tests
  moved to `src/pmap_tests.rs`, leaving `lib.rs`, `pmap.rs`, and the test module
  below the 1,500-line arch-lint cap. Verification: red-first process-root
  tests, full M1 HAL tests, scoped fmt check, clippy, target build,
  `RUSTFLAGS=-Dunused` target check, M1 QEMU smoke sentinel, arch/docs lints,
  progress validation, and `git diff --check`. Aggregate `cargo xtask check`
  is currently blocked outside this slice by `tx-ext4` Clippy
  `manual_is_multiple_of` in `crates/tx-ext4/src/host_async.rs`. Next step:
  keep VM/user runtime behind the later AddressSpace and trap-shell slices;
  still no real K210 hardware boot claim.
- Four parallel substrate/kernel-main follow-up worktrees are prepared from
  `origin/main` commit `0857ab9`: reactor smoke
  (`codex/reactor-smoke`), zone/index/mutation
  (`codex/zone-index-mutation`), pmap root/ASID/shootdown
  (`codex/pmap-root-asid-shootdown`), and trap/core-init
  (`codex/trap-coreinit`). Their ownership, paths, and verification commands
  are recorded under `docs/progress/worktrees/`; next step is per-lane planning
  before implementation. No blocker.
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
- LA64 QEMU still needs process-root/ASID-equivalent pmap work. M1 Dock mock
  now has local ASID-shaped process roots for QEMU/OpenSBI, but both LA64 and
  M1 still need full VM/trap integration before they are user/VM-ready. Real
  K210 boot is still out of scope.
- RV64 QEMU still needs superpage/multi-frame map-count batching, remote-hart
  shootdown coordination, and the full trap shell before the page substrate is
  user/VM-ready.
- ext4 image creation requires host `mkfs.ext4`.
- `cargo xtask check` is currently blocked by an unrelated `tx-ext4` Clippy
  `manual_is_multiple_of` finding in `crates/tx-ext4/src/host_async.rs`.
- BusyBox images require `TX_BUSYBOX`; dynamic musl layouts also require
  `TX_MUSL_LIBC`.

## Latest Decisions

- `docs/progress/decisions/2026-04-29-m1dock-mock-high-half-boot.md`
- `docs/progress/decisions/2026-04-29-m1dock-mock-process-root-asid.md`
- `docs/progress/decisions/2026-04-29-m1dock-mock-pmap-module-split.md`
- `docs/progress/decisions/2026-04-29-m1dock-mock-kernel-unmap-protect.md`
- `docs/progress/decisions/2026-04-29-m1dock-mock-pt-node-kernel-mapping.md`
- `docs/progress/decisions/2026-04-29-la64-m1-memory-pmap-prep.md`
- `docs/progress/decisions/2026-04-29-pre-substrate-smoke-gate.md`
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
