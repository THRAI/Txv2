# Reactor Baseline Closure Design

**Status:** approved design, implementation in progress (2026-07-19)

## Purpose

Close the repository-wide CI blockers inherited by the completed Reactor
refactor without rewriting its thirteen commits, weakening lint rules, raising
ratchet ceilings, or importing unrelated dirty-main state.

The completed Reactor branch remains the reviewed architecture artifact. This
work is a stacked integration branch whose completion condition is the actual
CI contract: `cargo xtask ci` passes, followed by `cargo xtask ci-slow` where
the required local tools and images are available.

## Starting Point

- Base branch: `codex/reactor-refactor` at `df0e877e`.
- Closure branch: `codex/reactor-baseline-closure`.
- The Reactor range remains exactly thirteen commits.
- The base commit `b81a0327` was a working-tree snapshot, not a green CI
  baseline. It captured partially migrated tracked files while omitting
  untracked design documents and generated vendor artifacts.
- Fresh `cargo xtask ci` reported 15 failing checks.

The failures fall into four classes:

1. **Snapshot consistency defects:** stale call arities, incomplete API
   migrations, missing canonical documents, and stale generated status tables.
2. **Branch integration debt:** a small number of new raw boundary references,
   Clippy warnings, and growth in already-oversized kernel files.
3. **Repository ratchet debt:** architecture, boundary, and invariants counts
   that were already above their enforced ceilings in the snapshot.
4. **Environment artifacts:** the RV64 netfast binary is generated and
   gitignored; it must be built reproducibly rather than fabricated or checked
   in as opaque evidence.

## Non-Negotiable Rules

- Do not amend, reorder, squash, or append commits to
  `codex/reactor-refactor`.
- Do not raise `MAX_*` ceilings.
- Do not add `#[allow(...)]` to silence CI.
- Do not make private object internals public to repair stale tests; migrate
  tests to existing public endpoint or adapter APIs.
- Do not restore retired timer ownership to `DelegateRegistry`.
- Do not move semantic policy into Reactor, substrate, or HAL.
- Do not fabricate vendor binaries or QEMU sentinel output.
- Preserve the dirty main checkout and copy only explicitly audited source
  documents that the snapshot omitted.
- Keep authored Rust files at or below the architecture-lint limit, with a
  target of 1,500 lines for newly split modules.

## Completion Model

The closure is split into independently verifiable phases.

### Phase A: Coherent Build And Generated State

Repair deterministic blockers that do not require repository-wide ownership
changes:

- rustfmt and Clippy API-shape failures;
- stale substrate test arities;
- incomplete signal dual-publication migration;
- host integration tests using private wait-source helpers;
- unused imports, variables, constructors, and orphan helpers;
- missing canonical documents already present in the main checkout;
- syscall generator disagreement and regenerated status sections;
- reproducible `tx-netfast-riscv64` generation;
- target compilation fallout exposed after those repairs.

Phase A is complete when format, Clippy, host check/tests, docs, unused,
progress, syscall sync, and required target checks pass. Architecture,
boundary, and invariants may remain red only when their exact inventories are
recorded for later phases.

### Phase B: Reactor Integration Delta

Remove only debt introduced by `b81a0327..df0e877e`:

- route Reactor substrate imports through `adapter::bus_wire`;
- route kernel Reactor test imports through `boot_runtime`;
- group high-arity Reactor publication APIs into typed transaction inputs;
- move Reactor-specific kernel drive glue/tests out of oversized parent files;
- keep all Reactor correctness and module-layout witnesses green.

Phase B must not claim to solve inherited repository-wide boundary debt.

### Phase C: Repository Ratchet Burn-Down

Close the inherited architectural debt by ownership-aligned slices:

- architecture lint: split oversized files, remove stale allowance scaffolds,
  route locks through facades, replace BootArgs with typed BootHandoff-derived
  plans, and move target-specific rootfs selection to board/HAL ownership;
- boundary lint: route substrate and Reactor access through owner-local adapter
  verbs, prioritizing production callsites before tests;
- invariants: audit semantic `step_*` functions, consume `ScriptCtx`, remove
  stored guards, converge notification wrappers, authorize `openat` creation,
  and migrate at least thirty syscall awaits into the three-lane dispatch
  model.

Each subsystem slice gets its own plan, tests, commit, spec review, and code
quality review. Mechanical file movement and semantic corrections are separate
commits unless they cannot compile independently.

## Ownership Boundaries

| Area | Owner | Closure rule |
|---|---|---|
| Reactor task/wait mechanism | `tx-reactor` | substrate access only through Reactor adapters |
| Kernel boot integration | `tx-kernel` + HAL board boundary | generic kernel consumes typed handoff and adapter vocabulary |
| Semantic steps | owning `tx-subsystems` module | real five-stage transitions or non-step helper names |
| Syscall sequencing | `tx-shims` / scripts | no semantic truth; use Immediate, OneShot, or full drive lanes |
| Generated syscall docs | `xtask` generators | both parsers agree before regeneration |
| Canonical design docs | `docs/design` | restore exact audited source, never placeholders |
| Vendor netfast binary | `tools/netfast` | reproducible local build, never fabricated content |

## Verification Ladder

Every task starts with the narrow failing command and ends with the same
command passing. Phase gates are:

```sh
cargo fmt --check
cargo clippy --no-deps --workspace --all-targets \
  --exclude tx-kernel-riscv64-qemu-virt \
  --exclude tx-kernel-riscv64-m1dock-mock \
  --exclude tx-kernel-loongarch64-qemu-virt -- -D warnings
cargo check --workspace
cargo test --workspace \
  --exclude tx-kernel-riscv64-qemu-virt \
  --exclude tx-kernel-riscv64-m1dock-mock \
  --exclude tx-kernel-loongarch64-qemu-virt -- --test-threads=1
cargo xtask lint arch
cargo xtask lint docs
cargo xtask lint unused
cargo xtask lint boundary
cargo xtask lint invariants all
cargo xtask lint syscall-status
cargo xtask syscall-status --check
cargo xtask progress validate
cargo xtask ci
cargo xtask ci-slow
```

No phase is complete from cached output or an agent report. The responsible
agent must run the fresh command and record the exact result.

## Commit And Review Policy

- One coherent fix or subsystem slice per commit.
- Every implementation task receives a spec-compliance review followed by a
  code-quality review.
- Review findings are fixed and re-reviewed before the next task starts.
- Progress records name changed paths, exact verification, next step, and
  blockers.
- No coauthor trailers.
