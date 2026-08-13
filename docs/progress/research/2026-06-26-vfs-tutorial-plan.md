# VFS Tutorial Series — Authoring Note

**Date:** 2026-06-26
**Status:** Written (11 chapters + README + consolidated report); expanded with
3 functional deep-dives after review feedback ("too thin on kernel logics").
**Location:** `docs/tutorials/vfs/`
**Sibling precedent:** `docs/tutorials/reactor-and-threads/` (same house style)

## Expansion (review round 2)

Reviewer flagged the first cut as too architecture-heavy / thin on functional
kernel logic. Added three deep-dive chapters between the concept arc (0–6) and
the capstone, and renumbered the capstone 07→10:

- `07_page-cache-and-mmap.md` — `PageContainer`/`PageContainerKind`, demand
  paging via `materialize_page`, owner/joiner fetch coalescing, the
  `step_range` byte loop + `ByteProgress` partial-progress, `grow_size_to`,
  user-buffer copy seam, `mmap` sharing cache frames with `VmEntryBacking::Page`,
  COW, `step_fsync` writeback + `PageProgress`.
- `08_special-files-and-fd-zoo.md` — the two non-regular mechanisms
  (`StructPayload` vs non-`Rnode` `OpenFileBacking`); pipes (ring, EOF/EPIPE,
  `PIPE_BUF`), ttys (line discipline + typed `OpenFileIoctl`), char devices
  (`CharDeviceOps`, devfs `/dev/null|zero|urandom`), synthetic fds
  (eventfd/timerfd/signalfd/epoll/pidfd/socketpair), `fcntl(F_SETFL)` overrides.
- `09_blocking-and-mount-internals.md` — `WaitSource`/`RNodeWaitPoints`,
  park/wake walkthrough, `poll`/`epoll`, EINTR dual-subscription; mount
  propagation, bind, remount, `MountNamespace`/`CLONE_NEWNS`/`setns`, the new
  mount API (`MountApiFile`), boot bringup (`mount_rootfs_tmpfs` → devfs →
  procfs) + initramfs cpio unpack.

Renumber bookkeeping: `07_capstone.md` → `10_capstone.md`; README table +
two-arc framing rewritten; mapping table extended with page-cache/mmap/pipe/
tty/device/synthetic-fd/wait-queue/propagation/rootfs rows; all `Chapter 7`
forward-refs that meant the capstone updated to `Chapter 10`; Chapter-0 roadmap
extended with the 7–9 arc. `TECHNICAL_REPORT.md` grew sections 8/9/10 (page
cache, special files, blocking+mount) and renumbered case-study→11,
reference→12; type/source index extended. Material sourced from four parallel
code-exploration sweeps; anchors spot-checked (see below).

NOT added: permissions/credentials chapter (DAC, setuid/sticky, witnesses) —
reviewer deselected it. The witness model is still summarized where the split
demands it; a dedicated chapter remains an open follow-up if wanted.

## Original first cut

**Status:** Written (8 chapters + README + consolidated report)

## What was produced

A numbered tutorial series explaining the txKernel VFS to readers who know the
traditional Unix/Linux VFS (inode/dentry/file/superblock, the operations
vtables, `namei`, the dcache, mounts). Files:

- `README.md` — thesis, mapping table, how-to-read-code, "what's genuinely the
  same as a normal VFS" caveat.
- `00_what-is-a-vfs.md` — traditional four objects + vtables, the classic read
  flow, the `struct inode` fusion problem, the two motivating bugs.
- `01_entity-model.md` — `RNode` / `DEntry` / `OpenFile` with near-verbatim
  struct defs; `Cap<T>` introduced; parent-strong/child-weak dcache rationale;
  the two-chains-to-one-RNode object graph.
- `02_payload-identity-split.md` — **headline.** identity/capability/payload
  decomposition; the `Weak → IdentRef → Cap → OperationalEvidence` ladder with
  per-rung VFS uses; four motivations (unlink-open, force-umount, refcount-free
  traversal, monotone race degradation); three factoring strengths.
- `03_fsops-and-backends.md` — `FsOps` + `FsPageBacking`; the `StepOutcome`
  four-variant step model and the `drive` sync→async bridge; `MountOutput`; the
  backing gallery (tmpfs/procfs/devfs/ext4-FAT mapped to `RNodeBacking`).
- `04_path-resolution.md` — `WalkingState`/`kernel_step`/driver loop; symlinks +
  ELOOP; DAC descent; dcache hit path; mount crossing; `NeedIO`/`ResumeToken`
  yield; refcount-free traversal made concrete; `step_open`.
- `05_mount-and-composite.md` — `MountIdentity`/`MountPayload` fully split;
  `payload_cap() -> Result<_, Dead>` as the force-umount mechanism; the
  composite-op table.
- `06_open-read-write.md` — fd table; `sys_openat`/`sys_read`/`sys_write`;
  `OpenFile::step_read` dispatch-by-backing table; offset/`lseek`/shared
  description; `close`.
- `07_capstone.md` — `open`→`read`→`unlink`-while-open→`read`→`close` on tmpfs
  `/tmp/foo`, tracing identity and payload parting at unlink and reclaiming at
  close; mapping table re-populated.
- `TECHNICAL_REPORT.md` — consolidated single-file report (abstract, 9 numbered
  sections, type/source reference), matching the reactor report's form.

## Authoring decisions

- **Split-forward emphasis** (per user): traditional VFS taught briskly;
  `02_payload-identity-split.md` is the center of gravity, grounding the split
  in real VFS pain rather than abstract object-model theory.
- **All four filesystems** used as a "backing gallery" in ch.3 (one per
  `RNodeBacking` style) rather than four redundant walkthroughs.
- **Running example:** tmpfs `/tmp/foo`, reused ch.3→7.
- **Pseudocode bodies, accurate type/field names**, `file:line` anchors per
  chapter — same contract as the reactor series.
- `OpenPin`/`LinkPin`/`OperationalEvidence` are presented as *object-model
  vocabulary* (they are design-doc terms; the VFS code expresses the same
  predicate through `nlink` + open-edge `Cap`s, not literal structs named
  `OpenPin`). This is stated where it matters.

## Verification

- Documentation only; no code changed; no build/test impact.
- All internal cross-links resolve (checked).
- Source anchors spot-checked against the tree: `sys_openat`/`sys_close`/
  `sys_lseek` (`fs_basic.rs:718,1176,1374`), `sys_read`/`sys_write`
  (`io.rs:2179,1913`), `OpenFile::step_read`/`step_write`/`step_lseek`
  (`execution.rs:334,591,469`), `FsOps` (`execution.rs:66`), `FsPageBacking`
  (`fs_page_backing.rs:34`), `StepOutcome` (`step/mod.rs:272`), `kernel_step`
  (`resolution/step.rs:51`), driver (`driver.rs:34`), `MountIdentity`/
  `MountPayload`/`payload_cap` (`mount/mod.rs:413,292,481`), entity defs
  (`structure.rs:95,227,430,480,546,576,866,1016,1154`), tmpfs
  (`tmpfs/mod.rs:200,224,295`) — all confirmed on the lines cited.

## Possible follow-ups

- Add the three diagrams as committed assets (reference hierarchy on a file;
  the walk state machine; the unlink-while-open / force-umount lifetime
  timelines). Currently rendered as ASCII inline.
- If the series graduates into `docs/design/05_filesystem/`, wire `txdoc:` tags
  for CI grep-stability.
