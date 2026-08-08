# txKernel Agent Guide

txKernel is a Rust kernel architecture/spec workspace. The active docs define a factored kernel model: semantic subsystems own entities and transitions; substrate provides zone, index, epoch, mutation, bus, page, and reservation primitives; the reactor schedules tasks and waits; HAL is axHal-style static platform selection.

## Read Order

1. `docs/design/INDEX.md`
2. `docs/design/00_meta-framework/CONCEPTS_v4.md`
3. `docs/design/00_meta-framework/INVARIANTS_v4.md`
4. `docs/design/00_meta-framework/object_model_v2.md`
5. `docs/design/00_meta-framework/SUBSYSTEM_ANATOMY_v2_1.md`

## Skills

Use `.agents/skills/` for task-specific guidance. Skills are generated or maintained from canonical docs and are the preferred operational entry points for edits, audits, and implementation planning.

Start with:

- `tx-agentic-development` when planning or running multi-agent work, especially
  HumanLayer-style locator/analyzer/pattern workflows.
- `tx-design-reference` when gathering the relevant active docs for a task.
- `tx-docs-cleanup` for broad cleanup walks.
- `tx-docs-skill-maintenance` when creating or refreshing skills.
- `tx-progress-memory` when recording or resuming decisions, plans, handoffs,
  research, or status.
- `tx-meta-alignment` when editing `docs/design/00_meta-framework/`.
- `tx-ebr-zone` when editing object model, EBR, zone, cap, weak, witness, or projection docs.
- `tx-hal-axhal` when editing HAL, boot, page substrate, traps, pmap, or platform docs.
- `tx-subsystem-manifest` when editing subsystem specs.
- `tx-implementation-readiness` when deciding if docs are ready to code from.
- `tx-xtask` when running, building, imaging, decoding traps, linting, or any
  other `cargo xtask` invocation — full reference for every subcommand.

## Rules

- Active docs override archived and source-trace docs.
- Do not resurrect the retired architecture draft, legacy dynamic HAL manager,
  runtime HAL-manager type, or upper-layer raw `Zone<T, Policy>`.
- Upper subsystems expose role-shaped types: `Cap<T>`, `PayloadCap<T>`, `Weak<T>`, `IdentRef<'g, T>`, witnesses, identity slots, and projection rows.
- Active design docs carry grep-stable `txdoc:` tags. Use those tags in CI,
  review, and implementation-plan references.
- For fast local feedback (build + host unit tests), run `cargo -q xtask unit`.
  One line per step on pass; failures show only the failing test name, panic
  message, and failure list — no passing lines or cargo build headers.
- To build everything needed before a QEMU run (environment check, kernel ELF,
  and initramfs/disk image), run `cargo xtask full-build [--target TARGET]
  [--skip-doctor] [--no-image]`. This is the prepare step before
  `cargo xtask qemu`; `cargo xtask test [busybox-boot|smoke]` chains all three.
- For RV64 QEMU trap or fault logs, prefer `cargo xtask fault-decode
  --target rv64-qemu` before hand-decoding `scause`/`sepc`/`stval`. The tool
  handles low-linked and high-VMA ELF layouts, direct-map classification,
  demangling, conservative data code-pointer candidate tracing, full RV64C
  compressed instruction decode, stack dump with heuristic code-pointer
  scanning, and kernel panics (the panic handler emits a synthetic
  `scause=3 sepc=<ra> stval=0` line so panics parse identically to hardware
  traps). Key flags: `--serial <log>` (parse a full QEMU log, `--all` for
  every trap); `--brief` (one line per trap); `--json` (structured output);
  `--summary` (aligned table + histogram); `--color`/`--no-color`; `--user-elf`
  (annotate user-space addresses).
- Before declaring any task complete, do a progress catch-up in
  `docs/progress/`: update `STATUS.md` and, when useful, close or update the
  relevant JSON plan/worktree/handoff or add a dated decision/research note.
  The catch-up must name what changed, verification run, next step, and any
  blocker. Validate changed JSON records with `cargo xtask progress validate`.
- After doc edits, check active Markdown links and stale vocabulary before declaring alignment.
- **Do not** add yourself as coauthor when creating commits.

<!-- CODEGRAPH_START -->
## CodeGraph

In repositories indexed by CodeGraph (a `.codegraph/` directory exists at the repo root), reach for it BEFORE grep/find or reading files when you need to understand or locate code:

- **MCP tool** (when available): `codegraph_explore` answers most code questions in one call — the relevant symbols' verbatim source plus the call paths between them, including dynamic-dispatch hops grep can't follow. Name a file or symbol in the query to read its current line-numbered source. If it's listed but deferred, load it by name via tool search.
- **Shell** (always works): `codegraph explore "<symbol names or question>"` prints the same output.

If there is no `.codegraph/` directory, skip CodeGraph entirely — indexing is the user's decision.
<!-- CODEGRAPH_END -->

## Toolchain inventory

Last verified locally: 2026-08-01 (macOS, `/Users/3y/Downloads/Tx`). Prefer
the repository entry points below when operating on Tx:

- Rust: `rustc`/`cargo` 1.89.0-nightly, `rustup` 1.28.2; targets
  `riscv64gc-unknown-none-elf` and `loongarch64-unknown-none-softfloat`, with
  `rustfmt`, `clippy`, `rust-src`, and `llvm-tools` installed.
- Tx workflow: `cargo xtask doctor`, `unit`, `check`, `full-build`, `image`,
  `qemu`, `test`, `shell-test`, `fault-decode`, `trap-trace`, `progress`, and
  `observe` (see `.agents/skills/tx-xtask/SKILL.md`).
- Emulation and indexing: QEMU 10.2.2 (`qemu-system-riscv64`,
  `qemu-system-loongarch64`) and CodeGraph 1.4.1. If `.codegraph/` exists,
  use CodeGraph before broad text searches.
- Containers: Docker CLI 28.3.2 with a reachable Docker Engine 29.5.2;
  Docker Compose v2.38.2-desktop.1 and Buildx v0.25.0 are available. Podman
  is not installed. Use Docker for containerized build/test workflows when a
  repository script or task requires it; do not assume Podman compatibility.
- Host utilities available: Python 3.14.5, Node.js v26.0.0/npm, Git 2.50.1,
  GitHub CLI 2.92.0, `rg`, `jq`, CMake, Ninja, Make, Clang, GCC, Typst, and
  Pandoc.
- Tx doctor gaps: `mkfs.ext4`, `debugfs`, `e2fsck`, and `mcopy` are currently
  unavailable. Ext4 image creation/checking and FAT image workflows that need
  these tools may therefore fail or omit their corresponding steps. The
  vendored RV64 BusyBox and OSComp autotest submodule are present; the
  LoongArch BusyBox binary and `TX_MUSL_LIBC` are not configured.

When this inventory changes, rerun `cargo xtask doctor` plus the relevant
version/runtime checks and update this section rather than relying on stale
tool assumptions.

## Discover and output discipline

- confirm discovery files first: use `rg -l <keyword>' <paths>` to search for files, then `rg -n` on selected candidate files.
- rangy, long queries goes in `/tmp` first, cope large files with `wc -l` and `sed` for selective read.
- do not read large files in one shot: after determining the lines, use `sed` to read the code snippets.
- On exploring files: use `head` and `tail` plus pipe with `rg` to check sample shard, then decide whether to read more.
- User allows subagent/delegation in this repo, do not ask for confirmation before parallel agent worl.
- Explorer should return conclusion and evidence table ONLY (claim | file:line | confidence), DO NOT return original output, long diff or irrevelent logs.
- Main thread should use long timeout `wait_agent` for results. Do not read repo and files in main thread while waiting for agents. Syncthesize the results after explorers returned. Spot checwith key suspects.

## Worktree Identity Checks

- Do not calculate or fingerprint SHA values when comparing or recording worktrees. Use paths, branch names, file status, timestamps, and measured disk usage instead.
