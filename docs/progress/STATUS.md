# txKernel Status

**Updated:** 2026-05-02

## Current Shape

- 2026-05-02 VM checks/projections completion slice has landed on top of the
  subsystem-anatomy reorg. `vm::checks` now exposes staged observation helpers
  for fault recipe admission, fault-publication revalidation, map admission,
  and disjoint-remap shape checks; `execution.rs` still owns RangeLock
  acquisition, recipe mutation, and pmap publication. `vm::project` now exposes
  read-only `AddressSpaceProjection` / `VmMappingProjection` rows backed by a
  deterministic recipe snapshot, with page-backed mappings reduced to
  non-authoritative offsets rather than leaking `Cap<PageContainer>`. Focused
  red/green checks so far: `cargo test -p tx-kernel vm_checks --
  --test-threads=1` and `cargo test -p tx-kernel vm_project --
  --test-threads=1`. Verification: `cargo fmt --check`, `cargo test -p
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
  spellings. The finish pass also made two existing VM/PageBacked tests robust
  under parallel `cargo test`: the pmap-drop test now waits boundedly for the
  EBR-delayed destructor, and the PageBacked cap test no longer enters an
  incidental epoch guard. Verification: `cargo fmt --check`, `cargo test -p
  tx-kernel --lib`, `cargo xtask progress validate`, `cargo xtask lint docs`,
  and `git diff --check`. Next step: build real VFS/PageBacked file-device
  behavior on these shells; blockers remain trap/process/runtime integration
  and concrete backend implementations.
- 2026-05-02 VM/PageBacked implementation lane now lives on
  `codex/vm-pagebacked-impl`. It brought in the corrected AddressSpace
  range-index core, mmap-style gap placement, v1 disjoint-only `mremap`, and
  fault resolution over authoritative recipes. The lane now also has
  zone-backed `Cap<AddressSpace>` and `Cap<PageContainer>` constructors,
  registered VM/PageBacked zones, recipes carrying `VmBacking::Page { pc:
  Cap<PageContainer>, offset }`, PageBacked-owned `PageCacheIndex` entries
  backed by real PPN plus `CachePin`, and fault materialization that returns
  `MapPin` evidence for pmap publication. The pmap lane now replaces the
  remaining `PmapSeam` with a VM-owned `VmPmap`: `AddressSpace` construction
  captures a root-local `PmapIf` ops table, owns a real `PmapRoot`/ASID,
  publishes faults through HAL reserve/commit, keeps a VM shadow map solely for
  `MapPin` ownership, and tears down mappings through HAL unmap plus
  ASID-scoped shootdown before releasing map-count evidence. Verification so
  far: focused `tx-substrate` page-allocator tests, focused and full
  `tx-kernel` VM/PageBacked tests, full `tx-kernel --lib`, `cargo fmt
  --check`, `cargo xtask progress validate`, `cargo xtask lint docs`, and
  `git diff --check`. Next step: connect VFS `FsPageBacking` and later trap /
  Process / ThreadRuntime fault dispatch; blockers remain persistent epoch
  recipe snapshots, trap/process/runtime integration, and file/device backing.
- 2026-05-02 skill refresh: added subsystem-specific operational skills for
  VM/PageBacked, VFS/filesystem, and Process/ThreadRuntime work so future
  agents load the canonical subsystem docs before implementation and preserve
  the correct harness boundaries. Verification: `cargo xtask progress
  validate`, `cargo xtask lint docs`, and `git diff --check`. Next step: use
  these skills in future subsystem worktree prompts; no blocker.
- 2026-05-01 checkpoint: the current dirty workspace has been intentionally
  consolidated into a clean-start checkpoint covering reactor/bus/trap/SMP,
  progress memory, and docs alignment changes. Verification for the catch-up:
  `cargo xtask progress validate`. Next step: resume bounded subsystem fanout
  from isolated worktrees or a rendezvous commit; blocker at checkpoint time:
  overlapping uncommitted edits across multiple lanes.
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
  decoder. It parses legacy `scause`/`sepc`/`stval` logs and the richer RV64
  QEMU `trapframe:` dump emitted by the terminating trap path, detects
  low-linked versus high-VMA ELF layouts, classifies direct-map and
  firmware-gap addresses, symbolizes through Rust-native ELF/DWARF readers,
  and conservatively reports data code-pointer candidates without changing the
  kernel trap path.
  `AGENTS.md` and the HAL/trap skill now point future debugging sessions at
  this command before manual `nm`/`addr2line` work.
  Verification: `cargo fmt --check`, `cargo test -p xtask`,
  `cargo xtask build --target rv64-qemu`, and manual `fault-decode --addr` /
  `fault-decode --serial` smoke runs in the
  `codex/fault-decode-tool-impl` worktree. Next step: wire QEMU failure
  auto-annotation later if desired; no blocker. Post-merge high-VMA smoke
  coverage also fixed high-kernel alias classification and added regression
  coverage so those addresses are not reported as direct-map addresses. The
  2026-05-01 trapframe parser refresh keeps old one-line logs compatible while
  attaching saved `x0..x31`/CSR dumps to the decoded trap report. The QEMU
  sentinel runner now treats RV64 `scause`/`sepc`/`stval` trap summaries as an
  immediate trap failure and appends `fault-decode --serial` output to the
  error, avoiding a second manual decode step; the embedded annotation path has
  direct unit coverage for successful and failed decoder runs.
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
- Reactor readiness was audited against active design docs, existing
  `tx-reactor` code, tests, and progress memory. The design contract is mostly
  ready for subsystem authors to write step/script skeletons. The first
  follow-up worker wave has now closed several of the earlier implementation
  gaps: generation-checked task keys, cancel/drain lifecycle, bus-backed
  wait_event, and a HAL-shaped timer/idle adapter exist in host-testable form.
  Remaining blockers for a full subsystem runtime are interruptible/killable
  classification, userspace-run, AST return-to-user delivery, full cross-hart
  coordination, and CoreInit timer wiring. See
  `docs/progress/research/2026-04-30-reactor-readiness-for-subsystems.md`.
- Reactor first-wave parallel work is merged back into the coordinator tree.
  The accepted serial API decisions are recorded in
  `docs/progress/decisions/2026-04-30-reactor-serial-api-prework.md`; the
  worker merge and coordinator audit are recorded in
  `docs/progress/research/2026-04-30-reactor-first-wave-audit.md`. The merged
  surface includes the `tx-reactor` module facade, generation-safe `TaskKey`
  and `TaskTable`, `Reactor::submit_task` / `cancel_task` /
  `drain_completed` / `drain_cancelled`, scheduler direct tests and tightened
  `Yielded` behavior, `tx_substrate::bus::{RawQueue, RawPort, RawTrace}`,
  bus-backed `wait_event` with check/register/recheck/park, and
  `run_until_idle_with_clock` / `RunIdleReport` for HAL-shaped timer driving.
  Verification: `cargo fmt --check`, `cargo test -p tx-reactor` (43 tests),
  `cargo test -p tx-substrate`, RV64 kernel target check, `cargo xtask lint
  arch`, `cargo xtask lint unused`, `cargo xtask lint docs`,
  `cargo xtask progress validate`,
  `cargo xtask ci` (11 passed), and `git diff --check`. Next step: continue
  through the second-wave reactor audit and wire CoreInit once the `init.rs`
  lease clears. See
  `docs/progress/plans/2026-04-30-reactor-parallel-shards.json`.
- Reactor second-wave parallel work is merged back into the coordinator tree.
  The accepted surface adds the `InterruptSource` / `AtomicInterruptSummary`
  wait-classification seam, `wait_event_with_interrupts`, task-local AST
  marker batching, atomic preemption markers, counted `Completion`,
  `NonZeroU32` `CountdownCompletion`, and reactor-local `SyncRendezvous`
  acknowledgment coordination. Coordinator audit kept the work mechanism-only:
  no POSIX signal routing, ThreadPayload entities, HAL return path, real SMP
  shootdown protocol, or CoreInit loop wiring were added. Verification:
  `cargo fmt --check`, `cargo test -p tx-reactor --test wait_interrupt`,
  `cargo test -p tx-reactor --test completion`,
  `cargo test -p tx-reactor --test sync_coord`, and
  `cargo test -p tx-reactor` (65 tests), `cargo test -p tx-substrate`, RV64
  kernel target check, `cargo xtask lint arch`, `cargo xtask lint unused`,
  `cargo xtask lint docs`,
  `cargo xtask progress validate`, `cargo xtask ci` (11 passed), and
  `git diff --check`. Next step: move to userspace-run/CoreInit only after the
  active leases clear.
  See `docs/progress/research/2026-04-30-reactor-second-wave-audit.md`.
- Reactor third-wave parallel work is merged back into the coordinator tree.
  The stale `2026-04-29-zone-ebr-integration` worktree lease was marked
  `merged`, then four isolated workers landed cooperative `yield_now`, AST
  poll-boundary consumption, CoreInit HAL-clock smoke wiring, and RawQueue /
  RawPort terminal/subscriber hardening. Coordinator audit kept the work
  mechanism-only: no userspace-run, POSIX signal routing, `ThreadPayload`, real
  SMP shootdown protocol, epoll, or permanent WFI loop was added.
  Verification: `cargo test -p tx-reactor --test yield_now`,
  `cargo test -p tx-reactor --test ast_runtime`,
  `cargo test -p tx-reactor --test wait_bus`, `cargo test -p tx-substrate`,
  `cargo test -p tx-reactor` (71 tests), RV64 kernel target check,
  `cargo fmt --check`, `cargo xtask lint arch`, `cargo xtask lint unused`,
  `cargo xtask lint docs`,
  `cargo xtask progress validate`, `cargo xtask ci` (11 passed), and
  `git diff --check`. Boundary: this is enough for prototype reactor/mock
  device completions that update owner truth and fire a wake, but not for a
  production VFS/device/block runtime or real IRQ-driven block I/O across
  harts. Next dispatcher preference is the bus typed-declaration and
  wire-destruction hardening slice before treating VFS/block runtime as ready;
  trap/thread-runtime userspace-run remains a separate prerequisite. See
  `docs/progress/research/2026-04-30-reactor-third-wave-audit.md`.
- `SmpIf` is now the first real low-level SMP boundary, and RV64 QEMU now
  boots APs under `-smp 4`. `tx-hal` exposes `CpuMask`,
  `SecondaryEntry`, `IpiKind`, possible/online masks, AP online publication,
  AP boot, parking, and IPI send/ack hooks; `PlatformInfo` carries
  `possible_cpu_count`. RV64 QEMU parses DTB CPU nodes, uses SBI HSM to start
  APs through a retained low trampoline, allocates per-hart temporary boot
  stack slots for both BSP and APs, installs AP per-CPU state/trap vectors, and
  hands APs to a kernel-owned AP loop after initializing AP-local epoch/zone
  substrate state and marking them online. `tx_substrate::init_on_ap(cpu)` is
  now the generic AP-local substrate entrypoint, and RV64
  `install_early_percpu` no longer publishes online as a side effect.
  Reactor/bus wait storage was hardened at the raw
  storage layer by replacing remaining `Rc<RefCell<...>>` state in raw bus
  wires, timers, and sync rendezvous with `Arc` plus spin-locked storage. RV64
  QEMU pmap shootdown now also performs local `sfence.vma` plus SBI RFENCE to
  online remote harts, and CoreInit emits
  `txkernel:qemu-riscv64-virt:smp:shootdown:ok` after APs are online. APs now
  arm supervisor software wakeups, wait in `wfi`, poll and acknowledge
  pending SSIP as `IpiKind::Reschedule`, and CoreInit emits
  `txkernel:qemu-riscv64-virt:smp:ipi:ok` after online APs acknowledge the
  smoke IPI. `Phase1Scheduler` now also honors initial affinity masks for
  per-hart queues, keeps wakes on the last allowed hart when possible, and
  reports `RunnablePlacement { target_hart, wake_remote }`. `tx-reactor` now has
  `DispatchState`, `RescheduleSignal`, and `Reactor::drain_wakes_for_hart` so a
  wake can mark the target hart `need_resched` and request a remote reschedule
  IPI without making scheduler policy depend on HAL. `tx-kernel` wires that
  signal to `SmpIf::send_ipi(..., IpiKind::Reschedule)` through
  `SmpRescheduleSignal<P>`, and RV64 QEMU now prints
  `txkernel:qemu-riscv64-virt:reactor:dispatch:ipi:ok` after a wait-channel
  wake is dispatched to an AP, and
  `txkernel:qemu-riscv64-virt:reactor:ap-loop:ok` after that AP loop consumes
  the reschedule wake. `tx-reactor` now also requires submitted task futures to
  be `Send + 'static`, exposes `SharedReactor`, and has
  `Reactor::run_rescheduled_on_hart_with_reschedule()` to consume
  `need_resched` before draining a hart's runqueue. The boot smoke uses that
  shared reactor so the AP polls the remote-affinity task itself, then prints
  `txkernel:qemu-riscv64-virt:reactor:ap-runqueue:ok`.
  Verification: focused bus/reactor timer/rendezvous tests, scheduler affinity
  placement tests, RV64 HAL tests, RV64 kernel target check/build, ELF
  trampoline inspection, and RV64 QEMU smoke showing `Platform HART Count : 4`,
  `txkernel:qemu-riscv64-virt:smp:aps:online`,
  `txkernel:qemu-riscv64-virt:smp:shootdown:ok`,
  `txkernel:qemu-riscv64-virt:smp:ipi:ok`,
  `txkernel:qemu-riscv64-virt:reactor:dispatch:ipi:ok`,
  `txkernel:qemu-riscv64-virt:reactor:ap-loop:ok`,
  `txkernel:qemu-riscv64-virt:reactor:ap-runqueue:ok`,
  `txkernel:qemu-riscv64-virt:reactor:task:ok`, and
  `txkernel:qemu-riscv64-virt:boot:ok`. Boundary: this proves AP boot with
  AP-local substrate initialization and RFENCE-backed remote pmap invalidation
  plus low-level AP IPI acknowledgement on QEMU/OpenSBI, plus scheduler-owned
  wake placement and reactor-to-`SmpIf` reschedule IPI dispatch into a
  kernel-owned AP wake loop that drains real shared-reactor runqueue work. It
  is still serialized behind one boot-reactor lock, and it is not
  kernel-managed IPI/ack shootdown fallback, full trace/owner bus declaration
  macro coverage, concrete VFS/device owner implementations over embedded wire
  reclamation, final per-hart reactor sharding, or a production idle/timer
  loop. See
  `docs/progress/decisions/2026-04-30-smpif-parked-ap-boot.md` and
  `docs/progress/decisions/2026-04-30-rv64-smp-rfence-shootdown.md` and
  `docs/progress/decisions/2026-04-30-ap-substrate-before-online.md` and
  `docs/progress/decisions/2026-04-30-rv64-ipi-ack-smoke.md` and
  `docs/progress/decisions/2026-04-30-reactor-affinity-wake-placement.md` and
  `docs/progress/decisions/2026-04-30-reactor-reschedule-dispatch-bridge.md`
  and `docs/progress/decisions/2026-04-30-ap-reactor-loop-wfi-smoke.md` and
  `docs/progress/decisions/2026-04-30-ap-reactor-shared-runqueue-smoke.md`.
- `tx_substrate::bus` now has the first typed declaration surface over the
  raw queue/port carriers. `WireEventSet`, `WireDeclaration<E>`,
  `DeclaredQueue<E>`, and `DeclaredPort<E>` give subsystem-facing code a
  declared carrier name/kind/bit-set and validate fired bits plus subscription
  interests before delegating to the raw terminal/unsubscribed machinery.
  Verification: `cargo test -p tx-substrate --test bus`,
  `cargo test -p tx-substrate`, `cargo test -p tx-reactor --test wait_bus`,
  `cargo test -p tx-reactor`, RV64 kernel target check, fmt/progress/docs/
  arch/unused lints, `cargo xtask ci`, and `git diff --check`. Boundary:
  current reactor waits still use raw masks internally. Follow-up declaration
  macro work now covers queue/port bit-set newtypes, but production bus work
  still needs concrete zone/device owner manifests, final global epoll graph
  policy, and broader validation. A later trace slice added typed `RawTrace`
  payloads. See
  `docs/progress/decisions/2026-04-30-bus-typed-declaration-surface.md`.
- `tx_substrate::bus` now also has an epoch-fenced retire handshake for
  queue/port wire destruction. `RawQueue`, `RawPort`, `DeclaredQueue<E>`, and
  `DeclaredPort<E>` expose `retire(..., &epoch::Guard)` and
  `retire_silently(&epoch::Guard)`, terminate/drain subscribers, and return a
  `WireRetirement` record with carrier kind, terminal bits, wake count, guard
  epoch, guard CPU, and whether this call performed the terminal transition.
  Verification: `cargo test -p tx-substrate --test bus`,
  `cargo test -p tx-substrate`, `cargo test -p tx-reactor`, RV64 kernel target
  check, fmt/progress/docs/arch/unused lints, `cargo xtask ci`, and
  `git diff --check`. Follow-up owner-fence work now connects wire retirement
  records to EBR-delayed owner-storage reclaim. See
  `docs/progress/decisions/2026-04-30-bus-epoch-retire-handshake.md`.
- Static device/block wire backing storage now exists. `StaticRawQueue` and
  `StaticRawPort` are const-constructible storage objects; `raw()` plus
  `RawQueue::from_static` / `RawPort::from_static` create cloneable raw handles
  without allocating `Arc`, so device tables can be static-backed while keeping
  the hot raw fire/subscribe API unchanged. `DeclaredQueue<E>::from_static`
  and `DeclaredPort<E>::from_static` add typed declaration validation over the
  same static storage. `DEVICE.md` now describes this storage/handle split
  instead of an impossible zero-argument
  `RawQueue::new_const()` shape. Verification:
  `cargo test -p tx-substrate --test bus`, `cargo test -p tx-substrate`,
  `cargo test -p tx-reactor`, RV64 kernel target check, fmt/progress/docs/
  arch/unused lints, `cargo xtask ci`, and `git diff --check`. Boundary:
  concrete VFS/device owner implementations remain later. Later macro slices
  added typed `RawTrace` payloads and generated owner-manifest boilerplate. See
  `docs/progress/decisions/2026-04-30-bus-static-wire-storage.md`.
- The bus implementation is now split into smaller modules under
  `crates/tx-substrate/src/bus/`: `common.rs` for shared declaration/error/
  storage vocabulary, `queue.rs` for raw/static/declared queue carriers,
  `port.rs` for raw/static/declared port carriers, `graph.rs` for the bounded
  long-lived subscription owner, `trace.rs` for typed trace declarations, and
  `mod.rs` as the facade.
  This is a behavior-preserving reorganization so the next bus slices do not
  push one authored source file past the 1,500-line arch lint cap.
  Verification: `cargo test -p tx-substrate --test bus`,
  `cargo test -p tx-substrate`, `cargo test -p tx-reactor --test wait_bus`,
  `cargo test -p tx-reactor`, RV64 kernel target check, fmt/progress/docs/
  arch/unused lints, `cargo xtask ci`, and `git diff --check`. See
  `docs/progress/decisions/2026-04-30-bus-module-split.md`.
- `tx_substrate::bus` now has a bounded long-lived subscription graph slice.
  `SubscriptionGraph<N>` owns raw queue/port subscription tokens, returns
  generation-checked `SubscriptionGraphKey` handles, rejects stale/wrong-kind/
  empty-interest operations, supports update/remove/kind/state/take-ready, and
  is `Send + Sync` when shared behind an owner lock. This gives epoll-style
  code a durable subscription-token owner without making the raw carriers know
  about fd tables or tasks. Verification: `cargo test -p tx-substrate --test
  bus`, `cargo test -p tx-substrate`, `cargo test -p tx-reactor --test
  wait_bus`, `cargo test -p tx-reactor`, RV64 kernel target check, fmt/
  progress/docs/arch/unused lints, `cargo xtask ci`, and `git diff --check`.
  Boundary: final production work still needs target-fd reverse-index
  teardown, global epoll table integration, spill/fanout policy, and concrete
  VFS/device owner implementations. See
  `docs/progress/decisions/2026-04-30-bus-subscription-graph-slice.md`.
- `tx_substrate::bus` now has typed declared helpers over the bounded
  subscription graph. `DeclaredSubscriptionGraphKey<E>` wraps the raw
  generation-checked key, and `subscribe_declared_{queue,port}`,
  `update_declared_{queue,port}`, `remove_declared`, `kind_declared`,
  `state_declared`, and `take_declared_ready` validate interests against
  `DeclaredQueue<E>` / `DeclaredPort<E>` declarations before delegating to the
  raw graph. Focused tests cover declared queue/port registration, typed
  update, stale-key rejection after removal, undeclared-bit rejection, and
  `Send + Sync` for typed graph keys. Verification: `cargo fmt --check`,
  `cargo test -p tx-substrate --test bus`, `cargo test -p tx-substrate`,
  `cargo test -p tx-reactor --test wait_bus`, `cargo test -p tx-reactor`, RV64
  kernel target check, progress/docs/arch/unused lints, `cargo xtask ci`, and
  `git diff --check`. Boundary: this is not the final target-fd reverse-index
  teardown, global epoll table integration, spill/fanout policy, concrete
  VFS/device owner implementation, trace subscriber/nop-patching runtime, or
  production IRQ/timer integration. See
  `docs/progress/decisions/2026-05-01-bus-declared-subscription-graph-helpers.md`.
- `tx_substrate::bus` now has a first owner-storage EBR bridge for embedded
  wires. `WireOwnerRetireFence` is built from one or more `WireRetirement`
  records, requires every embedded wire retirement to be newly terminal and
  captured under the same guard epoch/CPU, aggregates wake/wire counts, and
  enqueues the containing owner storage through the epoch domain with an
  owner-provided reclaim callback. Verification: `cargo test -p tx-substrate
  --test bus`, `cargo test -p tx-substrate`, `cargo test -p tx-reactor --test
  wait_bus`, `cargo test -p tx-reactor`, RV64 kernel target check, fmt/
  progress/docs/arch/unused lints, `cargo xtask ci`, and `git diff --check`.
  A follow-up typed owner hook now adds `WireOwnerManifest` and
  `retire_wire_owner<T>()`, so a containing owner type supplies the complete
  embedded-wire retire sequence plus typed reclaim callback and call sites no
  longer pass erased reclaim functions directly. Boundary: concrete VFS/device
  owner implementations, trace subscriber/nop-patching runtime, target-fd
  reverse-index teardown, global epoll table integration, and production IRQ/
  timer lost-wake integration remain later. A later macro slice added generated
  owner-manifest boilerplate.
  See
  `docs/progress/decisions/2026-04-30-bus-owner-storage-ebr-fence.md`.
- `tx_substrate::bus` now has the generic typed owner-manifest hook over the
  owner-storage fence. `WireOwnerManifest` records the owner type's complete
  embedded-wire retire sequence and typed reclaim callback, and
  `retire_wire_owner<T>()` retires the owner by calling that manifest before
  queueing EBR-delayed storage reclaim. Verification: `cargo test -p
  tx-substrate --test bus`, `cargo test -p tx-substrate`, `cargo test -p
  tx-reactor --test wait_bus`, `cargo test -p tx-reactor`, RV64 kernel target
  check, fmt/progress/docs/arch/unused lints, `cargo xtask ci`, and
  `git diff --check`. Boundary: actual device/VFS owner types still need
  concrete manifests; target-fd reverse-index teardown, global epoll table
  integration, trace subscriber/nop-patching runtime, and production IRQ/timer
  integration remain later. Later macro slices added typed `RawTrace` payloads and generated
  owner-manifest boilerplate.
  See
  `docs/progress/decisions/2026-04-30-bus-typed-owner-manifest-hook.md`.
- `tx_substrate::bus` now has the first declaration macro slice.
  `bus_event_set!` generates typed `WireEventSet` bit newtypes with
  declared-bit unions, `from_bits`, `bits`, `contains`, and bitwise
  composition; `bus_readiness!` and `bus_lifecycle!` are queue/port-oriented
  aliases re-exported through `tx_substrate::bus`. Focused bus tests cover
  macro-generated `DeclaredQueue<E>` and `DeclaredPort<E>` integration plus
  undeclared-bit rejection. Verification: `cargo fmt --check`, `cargo test -p
  tx-substrate --test bus`, `cargo test -p tx-substrate`, `cargo test -p
  tx-reactor --test wait_bus`, `cargo test -p tx-reactor`, RV64 kernel target
  check, progress/docs/arch/unused lints, `cargo xtask ci`, and
  `git diff --check`. Boundary: this is not target-fd reverse-index teardown,
  global epoll table integration, or spill/fanout policy. Later macro slices
  added typed tracepoint payload structs and generated owner-manifest
  boilerplate. See
  `docs/progress/decisions/2026-04-30-bus-declaration-macro-slice.md`.
- `tx_substrate::bus` now has typed `RawTrace<P>` payload declarations.
  `TracePayload`, `TraceDeclaration<P>`, and `RawTrace<P>` preserve tracepoint
  payload types and declaration names at call sites; `bus_tracepoint!`
  generates copyable payload structs plus the marker implementation. Focused
  tests cover manual and macro-generated payloads, no-op emit, explicit empty
  payload trace emit, and `Send + Sync` for the trace declaration/carrier.
  Verification:
  `cargo fmt --check`, `cargo test -p tx-substrate --test bus`,
  `cargo test -p tx-substrate`, `cargo test -p tx-reactor --test wait_bus`,
  `cargo test -p tx-reactor`, RV64 kernel target check, progress/docs/arch/
  unused lints, `cargo xtask ci`, and `git diff --check`. Boundary: this is
  not trace subscriber registration, nop-patching, ftrace/perf/BPF delivery,
  target-fd reverse-index teardown, global epoll table integration, or
  spill/fanout policy. Later graph and macro slices added ready scans,
  explicit graph teardown, and generated owner-manifest boilerplate. See
  `docs/progress/decisions/2026-05-01-bus-typed-rawtrace-payloads.md`.
- `tx_substrate::bus` now has a first epoll-style readiness scan and explicit
  graph teardown API. `SubscriptionGraphReady` reports the generation-checked
  key, wire kind, and subscribed/terminal state for each entry returned by
  `SubscriptionGraph::collect_ready(&mut [..])`; the scan consumes ordinary
  readiness but keeps terminal subscriptions visible until policy code removes
  them. `SubscriptionGraph::clear()` explicitly drops every owned subscription
  token for epoll-fd close and stales the old keys. Focused tests cover bounded
  scans that leave unread ready entries intact, terminal wire destruction
  reporting, and full graph teardown unsubscribing raw queue/port carriers.
  Verification: `cargo fmt --check`, `cargo test -p tx-substrate --test bus`,
  `cargo test -p tx-substrate`, `cargo test -p tx-reactor --test wait_bus`,
  `cargo test -p tx-reactor`, RV64 kernel target check, progress/docs/arch/
  unused lints, `cargo xtask ci`, and `git diff --check`. Boundary: this is
  not the final global epoll table, target-fd reverse index, spill/fanout
  policy, trace subscriber/nop-patching runtime, or concrete VFS/device/fs/
  block owner implementation. See
  `docs/progress/decisions/2026-05-01-bus-graph-ready-scan-teardown.md`.
- `tx_substrate::bus` now has generated owner-manifest boilerplate for simple
  embedded-wire owners. `bus_wire_owner_manifest!` emits the unsafe
  `WireOwnerManifest` impl from a field-retire list, builds the retire fence
  under one epoch guard, and wires the typed reclaim callback through the
  existing `retire_wire_owner<T>()` path. Focused tests cover a declared
  queue/port owner, terminal wake delivery, fence accounting, and EBR-delayed
  reclaim. Verification: `cargo fmt --check`, `cargo test -p tx-substrate
  --test bus`, `cargo test -p tx-substrate`, `cargo test -p tx-reactor --test
  wait_bus`, `cargo test -p tx-reactor`, RV64 kernel target check,
  progress/docs/arch/unused lints, `cargo xtask ci`, and `git diff --check`.
  Boundary: concrete VFS/device/fs/block owner types still need to exist; this
  is not trace subscriber/nop-patching runtime, target-fd reverse-index
  teardown, global epoll table integration, spill/fanout policy, or production
  IRQ/timer integration. See
  `docs/progress/decisions/2026-05-01-bus-owner-manifest-macro.md`.
- `tx_reactor::wait` now has first-class typed declared-port wait consumption.
  `DeclaredChannel<E>` wraps an existing `DeclaredPort<E>` or creates one from
  `WireDeclaration<E>`, `DeclaredWaitFuture<E>` and
  `DeclaredWaitEventFuture<E, C, I>` subscribe with typed event interests, and
  `Reactor::declared_channel*` attaches declared channels to the reactor timer
  queue. The raw `Channel` / `Mask` path remains for existing completion and
  sync coordination internals, but subsystem-facing code can now wait on a
  declared bus port without erasing its event type to a raw mask. Verification:
  `cargo fmt --check`, `cargo test -p tx-reactor --test wait_bus`,
  `cargo test -p tx-reactor`, `cargo test -p tx-substrate --test bus`,
  `cargo test -p tx-substrate`, RV64 kernel target check, progress/docs/
  arch/unused lints, `cargo xtask ci`, and `git diff --check`. Boundary:
  typed queue/readiness wait adapters now live in the readiness-channel slice;
  concrete subsystem migrations, target-fd reverse-index teardown, global
  epoll table integration, owner manifests, and the production per-hart loop
  remain later. Later graph and trace slices added explicit graph teardown and
  typed `RawTrace` payloads. See
  `docs/progress/decisions/2026-04-30-reactor-declared-wait-channel.md`.
- `tx_reactor::wait` now also has typed declared-readiness waits over
  `DeclaredQueue<E>`. `DeclaredReadinessChannel<E>` wraps an existing
  `DeclaredQueue<E>` or creates one from `WireDeclaration<E>`, typed readiness
  wait futures subscribe with declared readiness interests, and
  `Reactor::declared_readiness_channel*` attaches them to the reactor timer
  queue. Focused tests cover existing declared-queue integration, undeclared
  interest rejection, raw-compatible empty direct waits, and timeout waits.
  Verification: `cargo fmt --check`, `cargo test -p tx-reactor --test
  wait_bus`, `cargo test -p tx-reactor`, `cargo test -p tx-substrate --test
  bus`, `cargo test -p tx-substrate`, RV64 kernel target check,
  progress/docs/arch/unused lints, `cargo xtask ci`, and `git diff --check`.
  Boundary: concrete subsystem migration to declared waits, target-fd
  reverse-index teardown, global epoll table integration, owner manifests, and
  the production per-hart loop remain later. Later graph and trace slices added
  explicit graph teardown and typed `RawTrace` payloads. See
  `docs/progress/decisions/2026-04-30-reactor-declared-readiness-channel.md`.
- Reactor/bus/substrate is now prototype-ready for subsystem development where
  mock devices or reactor tasks update owner truth and fire wakes, but it is
  not production-complete for the full VFS/device/block runtime. Remaining
  spine gaps are: real subsystem migration to declared waits is not done;
  concrete VFS/device/fs/block owners and owner manifests do not exist yet;
  target-fd reverse-index teardown and the global epoll/fd policy are not
  implemented; trace subscriber
  registration and nop-patching are not implemented; AP/shared-reactor behavior
  is still not the final per-hart production loop; and VM/user/trap integration
  is still mock or partial.
- `tx_reactor` now has the first runtime-dispatch seams from the 2026-05-01
  parallel pass: `userspace` provides a host-testable single-slot
  userspace-run wait that resolves only on syscall/page-fault/fatal trap
  outcomes, and `hart_loop` provides a platform-independent per-hart step
  report over timer advancement, wake dispatch, reschedule markers, ready-task
  polling, and deadline selection. The full CoreInit runtime loop and AST
  return-to-user integration remain adjacent-owner work: CoreInit/HAL binds
  WFI, timer, and IPI mechanics to the per-hart step, while
  ThreadRuntime/signal/trap owners define userspace-entry AST policy and real
  trap-frame return. Verification: `cargo fmt --check`, `cargo test -p
  tx-reactor --test userspace_run`, `cargo test -p tx-reactor --test
  hart_loop`, `cargo test -p tx-reactor`, RV64 kernel target check,
  progress/docs/arch/unused lints, `cargo xtask ci`, and `git diff --check`.
  Boundary: this is still a runtime-dispatch shell around the current
  single-slot userspace-run facade, not ThreadRuntime-backed per-task
  userspace state, saved-register trap shell, or permanent BSP/AP production
  loop. See
  `docs/progress/research/2026-05-01-reactor-runtime-dispatch-audit.md`.
- `tx_reactor::userspace` now has a public reactor facade for the single-slot
  userspace-run shell. `Reactor::request_userspace_run` starts the current
  wait, and `Reactor::{dispatch_userspace_run,record_userspace_timer_preemption,
  complete_userspace_run,userspace_run_status}` expose the reactor-owned driver
  side without pulling in `ThreadPayload` or HAL return mechanics.
  `UserspaceRunSlot::checkpoint_userspace_entry` validates the active
  userspace-run request, drains a caller-supplied task-local `AstSlot`, and
  applies a policy-neutral decision to enter userspace, preserve the task for
  re-poll, or resolve the wait with a caller-supplied interesting trap.
  `Reactor::checkpoint_task_userspace_entry` wires that checkpoint to the real
  task table behind a generation-checked `TaskKey`, validating the
  userspace-run request before draining task AST so stale requests do not
  consume pending markers.
  Verification: `cargo test -p tx-reactor --test userspace_run`. Boundary:
  this is not POSIX signal delivery, VM fault policy, `ThreadPayload`
  mutation, multi-thread userspace-run state, or final HAL `return_to_userspace`. See
  `docs/progress/decisions/2026-05-01-reactor-userspace-entry-ast-checkpoint.md`.
- `tx_kernel::CoreInit` now binds the boot reactor to
  `tx_reactor::hart_loop` through a private HAL adapter. The adapter supplies
  current hart identity, `TimeIf::read_ns()`, deadline programming through
  `TimeIf::{set_deadline_ns,cancel_deadline}`, and remote reschedule delivery
  through the existing `SmpRescheduleSignal<P>`. APs now step the shared boot
  reactor through the hart-loop API before entering WFI, and the BSP
  `:reactor:task:ok` smoke now runs on `BOOT_REACTOR` through the same bounded
  adapter, followed by `:reactor:runtime-loop:ok`. Verification: `cargo fmt
  --check`, RV64 kernel target check, `cargo xtask build --target rv64-qemu`,
  RV64 QEMU smoke sentinel with the new runtime-loop serial marker,
  `cargo xtask progress validate`, docs lint, `cargo xtask ci`, and
  `git diff --check`. Boundary: this is still a bounded boot/runtime smoke,
  not the permanent BSP root runtime loop, full timer-trap dispatch,
  external-IRQ/device completion loop, final per-hart reactor sharding, or
  userspace return path. See
  `docs/progress/decisions/2026-05-01-coreinit-hart-loop-adapter.md`.
- RV64 QEMU now has the first timer-trap return path needed by the reactor
  runtime loop. `TimeIf::enable_timer_wakeups()` prepares the current hart for
  timer wakeups, the RV64 direct-mode trap vector handles supervisor timer
  interrupts beside reschedule IPIs, and the timer handler cancels the expired
  SBI deadline before returning to kernel code. CoreInit adds a bounded
  deadline-driven idle smoke: a `BOOT_REACTOR` timeout waiter arms a platform
  deadline, the BSP waits with WFI, the timer trap returns, and the next
  hart-loop step observes the timer wake and emits
  `txkernel:qemu-riscv64-virt:reactor:timer-idle:ok`. Verification:
  `cargo fmt --check`, RV64 kernel target check, `cargo test -p
  tx-hal-riscv64-qemu-virt`, `cargo xtask build --target rv64-qemu`, RV64 QEMU
  smoke sentinel with the new timer-idle marker, progress/docs validation,
  `cargo xtask ci`, and `git diff --check`. Boundary: this is not the full
  saved-register `KernelTrapSink`, external IRQ/device dispatch, production
  scheduler tick/preemption policy, final per-hart sharding, or user-mode trap
  return. See
  `docs/progress/decisions/2026-05-01-rv64-timer-trap-idle-smoke.md`.
- RV64 QEMU now routes traps through a first saved-register dispatch spine.
  The trap vector saves all integer registers plus `scause`, `sepc`, `stval`,
  and `sstatus` into `Rv64TrapFrame`, calls the board binary's named
  `tx_kernel_riscv64_qemu_trap_dispatch` symbol, and restores the frame before
  `sret`. `tx-hal` now exposes the first executable `TrapAction`,
  `FaultInfo`, `TrapFrameView`, `TrapFrameMut`, and `KernelTrapSink<P>`
  contract; `tx-kernel::trap::KernelTrapDispatcher` handles timer and IPI
  traps by cancelling deadlines or acknowledging reschedule IPIs, resumes
  external IRQs as a stub, and terminates unimplemented sync/syscall/user
  traps. Verification: `cargo fmt --check`, `cargo test -p
  tx-hal-riscv64-qemu-virt` with host dispatcher routing tests, RV64 kernel
  target check, `cargo xtask build --target rv64-qemu`, RV64 QEMU smoke
  sentinel preserving AP IPI and timer-idle markers, progress/docs validation,
  `cargo xtask ci`, and `git diff --check`. Boundary: this is a trap dispatch
  spine, not mutable syscall return writeback, VM page-fault handling,
  external IRQ/device dispatch, signal policy, userspace trampoline, or
  `return_to_userspace`. See
  `docs/progress/decisions/2026-05-01-rv64-saved-trap-dispatch.md`.
- RV64 QEMU trap frames now support real saved-frame writeback. `TrapFrameMut`
  carries a platform vtable for PC, SP, syscall return/error, and user TLS
  writes; `Rv64TrapFrame::view_mut()` backs that handle with the live saved
  frame; syscall dispatch tests prove the sink can rewrite `a0`; and the trap
  vector writes saved `sepc`/`sstatus` back before `sret`. RV64 also has
  `prepare_user_return()` and an unsafe `return_to_userspace` register-restore
  skeleton for the future trampoline. Terminating traps now print the full
  saved frame after the scalar `scause`/`sepc`/`stval` summary, and
  `fault-decode --serial` parses that block without double-counting it as a
  second trap. Verification: `cargo test -p
  tx-hal-riscv64-qemu-virt`, RV64 kernel target check, RV64 build/QEMU smoke,
  progress/docs validation, `cargo xtask ci`, and `git diff --check`.
  Boundary: this is writeback/trampoline substrate, not syscall dispatch,
  VM page-fault handling, signal delivery, user trap stack switching,
  external IRQ/device dispatch, or ThreadRuntime userspace-run integration.
  See `docs/progress/decisions/2026-05-01-rv64-trap-frame-writeback.md`.
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
- Local `main` has been merged forward with remote `origin/main` after the
  reactor-task-aware and zone/EBR PRs landed, while preserving the local
  agent-runner commits. Conflict resolution kept the board MMIO facts with the
  new `PlatformInfo.timebase_frequency_hz` field, kept the `CoreInit` kernel
  entry path, and accepted the upstream merged progress records. Verification:
  `cargo fmt --check`, `cargo xtask progress validate`, `cargo xtask lint
  docs`, `cargo xtask ci`, `cargo xtask build --target rv64-qemu`, RV64 QEMU
  smoke sentinel, and serial grep for zone/reactor/boot sentinels. Next step
  is pushing the merge commit if this local agent-runner mainline should become
  the remote `main`. Blocker: none.
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

## Latest Research

- `docs/progress/research/2026-05-01-reactor-runtime-dispatch-audit.md`
- `docs/progress/research/2026-05-01-coreinit-runtime-loop-scout.md`
- `docs/progress/research/2026-05-01-ast-return-to-user-scout.md`

## Latest Decisions

- `docs/progress/decisions/2026-05-01-reactor-userspace-entry-ast-checkpoint.md`
- `docs/progress/decisions/2026-05-01-rv64-saved-trap-dispatch.md`
- `docs/progress/decisions/2026-05-01-rv64-timer-trap-idle-smoke.md`
- `docs/progress/decisions/2026-05-01-coreinit-hart-loop-adapter.md`
- `docs/progress/decisions/2026-05-01-bus-graph-ready-scan-teardown.md`
- `docs/progress/decisions/2026-05-01-bus-owner-manifest-macro.md`
- `docs/progress/decisions/2026-05-01-bus-typed-rawtrace-payloads.md`
- `docs/progress/decisions/2026-05-01-bus-declared-subscription-graph-helpers.md`
- `docs/progress/decisions/2026-04-30-reactor-declared-readiness-channel.md`
- `docs/progress/decisions/2026-04-30-reactor-declared-wait-channel.md`
- `docs/progress/decisions/2026-04-30-bus-declaration-macro-slice.md`
- `docs/progress/decisions/2026-04-30-bus-typed-owner-manifest-hook.md`
- `docs/progress/decisions/2026-04-30-bus-owner-storage-ebr-fence.md`
- `docs/progress/decisions/2026-04-30-bus-subscription-graph-slice.md`
- `docs/progress/decisions/2026-04-30-bus-module-split.md`
- `docs/progress/decisions/2026-04-30-bus-static-wire-storage.md`
- `docs/progress/decisions/2026-04-30-bus-epoch-retire-handshake.md`
- `docs/progress/decisions/2026-04-30-bus-typed-declaration-surface.md`
- `docs/progress/decisions/2026-04-30-ap-reactor-shared-runqueue-smoke.md`
- `docs/progress/decisions/2026-04-30-ap-reactor-loop-wfi-smoke.md`
- `docs/progress/decisions/2026-04-30-reactor-reschedule-dispatch-bridge.md`
- `docs/progress/decisions/2026-04-30-reactor-affinity-wake-placement.md`
- `docs/progress/decisions/2026-04-30-rv64-ipi-ack-smoke.md`
- `docs/progress/decisions/2026-04-30-ap-substrate-before-online.md`
- `docs/progress/decisions/2026-04-30-rv64-smp-rfence-shootdown.md`
- `docs/progress/decisions/2026-04-30-smpif-parked-ap-boot.md`
- `docs/progress/decisions/2026-04-29-m1dock-mock-high-half-boot.md`
- `docs/progress/decisions/2026-04-29-m1dock-mock-process-root-asid.md`
- `docs/progress/decisions/2026-04-29-m1dock-mock-pmap-module-split.md`
- `docs/progress/decisions/2026-04-29-m1dock-mock-kernel-unmap-protect.md`
- `docs/progress/decisions/2026-04-29-m1dock-mock-pt-node-kernel-mapping.md`
- `docs/progress/decisions/2026-04-29-la64-m1-memory-pmap-prep.md`
- `docs/progress/decisions/2026-04-29-pre-substrate-smoke-gate.md`
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

- `docs/progress/research/2026-04-27-humanlayer-progress-memory.md`
