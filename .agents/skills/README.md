# txKernel Skills

These skills are operational lenses over the canonical architecture docs. They should not introduce new doctrine. If a skill disagrees with a canonical doc, the canonical doc wins and the skill should be regenerated or repaired.

Skill style follows a progressive-disclosure pattern:

- Keep `SKILL.md` short and task-specific.
- List canonical source docs up front.
- Give concrete rules and checks, not broad tutorials.
- Put longer rubrics or templates in `references/` only when needed.

Use `tx-docs-skill-maintenance` when adding or refreshing skills.

Current orientation skills:

- `tx-agentic-development` adapts HumanLayer-style subagent workflows for
  isolated research, planning, implementation, and progress memory.
- `tx-code-reorganization` guides behavior-preserving module splits, large-file
  reduction, group-level comments, and verification for code shape work.
- `tx-design-reference` gathers the relevant active docs for implementation,
  planning, and audits.
- `tx-docs-cleanup` guides broad active-doc cleanup, stale-reference scans, and
  link validation.
- `tx-docs-skill-maintenance` refreshes generated skills from canonical docs.
- `tx-ebr-zone` covers object model, EBR, zone, cap, weak, witness, identity
  slot, and projection-reference docs.
- `tx-hal-axhal` covers HAL, boot, page substrate, traps, pmap, platform docs,
  and host-side RV64 fault decoding with `cargo xtask fault-decode`.
- `tx-implementation-readiness` audits whether a doc area is ready to implement
  from.
- `tx-meta-alignment` covers edits under `docs/design/00_meta-framework/`.
- `tx-oscomp-musl-debug` captures the named-worktree OSComp/LTP musl debugging
  workflow, including tailored sdcard/QEMU runs, fault/trap tools, focused host
  reductions, merge catch-up, and progress recording.
- `tx-observe` covers kernel-side `tx-observe` emit points, txtrace ABI
  changes, trace-daemon replay/live host drain, Perfetto tracks, OSComp trace
  windows, page-fault/cold-cache probes, and allocation-track probes.
- `tx-process-threadruntime` covers Process, ThreadRuntime, signal, syscall,
  exec, first-userspace, and runtime integration seams.
- `tx-progress-memory` records or resumes durable decisions, plans, handoffs,
  research, status, and task-finish catch-ups.
- `tx-shell-syscall-fixup` tight observe→fix→verify loop for missing or
  broken syscalls when an interactive busybox/musl shell hangs, crashes, or
  diverges; pairs the trap-trace + fault-decode + shell-test xtask tools with
  the syscall-stub edit pattern in `tx-shims`.
- `tx-step-migration` wraps free-function `step_*` fns into `StepOp` trait
  impls, migrates ad-hoc step loops to the central `drive()` loop, and retires
  v4 vocabulary per the Txv3 migration plan and `07_BLAST_RADIUS.md`.
- `tx-subsystem-manifest` covers subsystem specs and zone-derived type policy
  tables.
- `tx-vfs-filesystem` covers VFS, Mount, PageBacked filesystem interfaces,
  bdev-fs, devfs, and kernel-facing filesystem backend work.
- `tx-vm-pagebacked` covers VM `AddressSpace`, recipes, `RangeLock`,
  PageBacked, page-cache, and VM fault-materialization work.
