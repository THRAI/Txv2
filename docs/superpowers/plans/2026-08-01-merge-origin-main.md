# Merge `origin/main` into current branch Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Merge `origin/main` into `codex/test-remote-network` without losing the current ext4/page-backed work and without leaving unresolved conflict debt.

**Architecture:** Treat this as a staged merge, not a single merge commit. First preserve the dirty branch state, then perform the merge in an isolated worktree, then resolve conflict clusters in dependency order: workspace metadata, ext4 format/core, fs bridge, subsystems/page-backed, time/xtask/docs, and finally verification. The current checkout stays as the recovery source until the merge result is proven.

**Tech Stack:** git, Rust workspace, `cargo xtask`, progress JSON, markdown docs.

---

### Task 1: Preserve the current dirty branch state

**Files:**
- Modify: none yet

- [ ] **Step 1: Record the exact dirty state**

Run:
```bash
git status --short --branch
git diff --stat
git diff --name-only
```

Expected: a stable snapshot of the current `codex/test-remote-network` worktree, including the ext4/lifecycle files already modified locally.

- [ ] **Step 2: Save the current state without mixing it into the merge**

Use one of:
```bash
git stash push -u -m "pre-merge origin-main preserve dirty ext4 lifecycle state"
```
or a WIP commit on the current branch if the user prefers permanent preservation.

Expected: the merge work begins from a clean tree while the dirty state remains recoverable.

- [ ] **Step 3: Verify the recovery point**

Run:
```bash
git stash list --max-count=3
git status --short --branch
```

Expected: clean merge starting point, with a visible stash entry or WIP commit containing the prior dirty state.

### Task 2: Isolate the merge work

**Files:**
- Create: a dedicated merge worktree or branch only if needed

- [ ] **Step 1: Create an isolated merge workspace**

Run from the clean starting point:
```bash
git worktree add /Users/3y/.config/superpowers/worktrees/Tx/merge-origin-main -b codex/merge-origin-main
```

Expected: a separate workspace that does not disturb the preserved dirty checkout.

- [ ] **Step 2: Confirm the merge base and branch direction**

Run:
```bash
git branch --show-current
git rev-list --left-right --count origin/main...HEAD
git merge-base HEAD origin/main
```

Expected: the merge target remains `origin/main`, and the current branch still shows the local feature branch ancestry.

### Task 3: Merge and enumerate conflicts

**Files:**
- Modify: none yet

- [ ] **Step 1: Perform a no-commit merge**

Run:
```bash
git merge --no-commit --no-ff origin/main
```

Expected: merge stops at conflicts, but does not create a commit.

- [ ] **Step 2: Enumerate the actual conflict set**

Run:
```bash
git diff --name-only --diff-filter=U
```

Expected: the active conflict list, which should be smaller than the full overlap set and can be resolved in batches.

- [ ] **Step 3: Group conflicts by resolution domain**

Use this grouping:
```text
Group A: Cargo.lock, xtask/Cargo.toml, xtask/src/image.rs, xtask lint helpers
Group B: crates/tx-ext4-format/*
Group C: crates/tx-ext4/*
Group D: crates/tx-fs/src/devfs/*, procfs/*, fat_bridge.rs
Group E: crates/tx-subsystems/src/page_backed/*, vfs/execution.rs, fs_iface/plan.rs
Group F: crates/tx-time/src/*
Group G: docs/Txv3/*, docs/design/*, docs/progress/*
```

Expected: each cluster gets a single owner decision, not file-by-file improvisation.

### Task 4: Resolve the ext4 format/core cluster first

**Files:**
- Modify: `crates/tx-ext4-format/src/journal.rs`
- Modify: `crates/tx-ext4-format/src/journal_replay.rs`
- Modify: `crates/tx-ext4-format/src/lib.rs`
- Modify: `crates/tx-ext4-format/src/mutation.rs`
- Modify: `crates/tx-ext4-format/src/pager.rs`
- Modify: `crates/tx-ext4-format/tests/jbd2_recovery.rs`
- Modify: `crates/tx-ext4-format/tests/pager_mock.rs`
- Modify: `crates/tx-ext4/src/journal.rs`
- Modify: `crates/tx-ext4/src/mount.rs`
- Modify: `crates/tx-ext4/src/namespace.rs`
- Modify: `crates/tx-ext4/src/pager.rs`
- Modify: `crates/tx-ext4/src/planner.rs`
- Modify: `crates/tx-ext4/src/read_backend.rs`
- Modify: `crates/tx-ext4/src/tests_v3.rs`
- Modify: `crates/tx-ext4/tests/journal_prepared_transaction.rs`

- [ ] **Step 1: Favor the newer lifecycle/settlement model when it is already the local branch direction**

Resolution rule:
```text
- keep the ext4 lifecycle and settlement additions that are local to this branch
- preserve remote fixes only when they do not regress those ownership boundaries
- if both sides changed the same API surface, prefer the version that still composes with MutationHandle / settlement ownership
```

- [ ] **Step 2: Resolve format-layer API drift**

Run after each file:
```bash
cargo test -p tx-ext4-format --lib --tests
```

Expected: no remaining syntax conflicts and the codec/recovery tests still describe the same JBD2 geometry.

- [ ] **Step 3: Resolve ext4 planner and mount ownership**

Use the branch-local policy:
```text
planner owns transaction assembly
mount owns publication and backend wiring
read_backend stays read-only and should not absorb writeback ownership
```

Expected: no fallback to legacy direct writeback ownership.

### Task 5: Resolve fs bridge and subsystems

**Files:**
- Modify: `crates/tx-fs/src/devfs/mod.rs`
- Modify: `crates/tx-fs/src/devfs/tests.rs`
- Modify: `crates/tx-fs/src/fat_bridge.rs`
- Modify: `crates/tx-fs/src/procfs/mod.rs`
- Modify: `crates/tx-fs/src/tx_ext4_bridge.rs`
- Modify: `crates/tx-subsystems/src/fs_iface/plan.rs`
- Modify: `crates/tx-subsystems/src/page_backed/core_tests.rs`
- Modify: `crates/tx-subsystems/src/page_backed/lifecycle.rs`
- Modify: `crates/tx-subsystems/src/page_backed/mod.rs`
- Modify: `crates/tx-subsystems/src/vfs/execution.rs`

- [ ] **Step 1: Keep the page-backed lifecycle contract aligned with the ext4 side**

Resolution rule:
```text
- page-backed owns lifecycle and terminalization
- VFS should project the request, not duplicate ownership
- bridge code should stay neutral and not reintroduce direct mutation control
```

- [ ] **Step 2: Resolve fs bridge adapters against the new lifecycle shape**

Expected: no adapter calls that bypass the current ownership split.

- [ ] **Step 3: Re-run subsystem tests**

Run:
```bash
cargo test -p tx-subsystems page_backed -- --test-threads=1
cargo test -p tx-fs --lib
```

Expected: page-backed and fs bridge tests still pass with the merged ownership model.

### Task 6: Resolve time, xtask, and docs

**Files:**
- Modify: `crates/tx-time/src/hal.rs`
- Modify: `crates/tx-time/src/keeper.rs`
- Modify: `crates/tx-time/src/timer/min_heap.rs`
- Modify: `xtask/Cargo.toml`
- Modify: `xtask/src/image.rs`
- Modify: `xtask/src/lint_invariants_api_language.rs`
- Modify: `xtask/src/lint_invariants_time_layering.rs`
- Modify: `xtask/src/lint_invariants_time_wake.rs`
- Modify: `xtask/src/observe_schema.rs`
- Modify: `xtask/src/oscomp.rs`
- Modify: `xtask/src/qemu.rs`
- Modify: `xtask/src/test.rs`
- Modify: `docs/Txv3/01_CONCEPTS_v5.md`
- Modify: `docs/Txv3/02_INVARIANTS_v5.md`
- Modify: `docs/design/03_memory-vm/PAGE_BACKED_v1.md`
- Modify: `docs/design/05_filesystem/IO_MANAGER_v1.md`
- Modify: `docs/design/INDEX.md`
- Modify: `docs/progress/STATUS.md`
- Modify: `docs/progress/decisions/2026-07-16-file-io-runtime-spawner-gate.md`
- Modify: `docs/progress/plans/2026-07-13-ext4-io-manager-write-path.json`
- Modify: `docs/progress/plans/2026-07-14-elf-exec-loader.json`

- [ ] **Step 1: Treat docs as architecture evidence, not commentary**

Resolution rule:
```text
- keep active docs aligned with the merged code
- update progress notes to reflect the merged direction and the verification run
- do not resurrect retired vocabulary or duplicate subsystem ownership
```

- [ ] **Step 2: Reconcile xtask changes with the merged commands**

Expected: build/test/progress commands still route to the right binaries and lints.

- [ ] **Step 3: Update docs/progress to match the merge state**

Minimum required content:
```text
- what changed
- what was verified
- next step
- blockers, if any
```

### Task 7: Finish merge verification

**Files:**
- Modify: none, unless verification exposes a real fix

- [ ] **Step 1: Run repository checks**

Run:
```bash
cargo -q xtask unit
cargo xtask progress validate
git diff --check
```

Expected: unit tests pass, progress JSON is valid, and no whitespace or patch-format issues remain.

- [ ] **Step 2: Run a broader check if merge touched shared paths**

Run:
```bash
cargo xtask check
```

Expected: repository-level checks pass or any failure is traced to a pre-existing unrelated issue.

- [ ] **Step 3: Record the merge result**

If the merge is clean, create the merge commit with a message that identifies the remote sync and the preserved ext4 lifecycle work. If not, stop at the exact unresolved files and keep the worktree open for follow-up.

## Self-Review

1. Spec coverage:
   - preservation of current dirty state: Task 1
   - isolated merge workspace: Task 2
   - actual merge and conflict enumeration: Task 3
   - ext4-format/ext4 resolution: Task 4
   - fs bridge and subsystem resolution: Task 5
   - time/xtask/docs resolution: Task 6
   - verification and merge closeout: Task 7

2. Placeholder scan:
   - no TBD/TODO placeholders
   - every task names concrete files or commands
   - every verification step has an exact command

3. Type consistency:
   - file paths are consistent with the current checkout
   - ownership terms match current branch vocabulary: PageBacked, MutationHandle, settlement, fs bridge, xtask, progress
