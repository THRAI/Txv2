# Reactor Baseline Closure Phase A Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the snapshot coherent enough that formatting, Clippy, host compilation/tests, documentation, progress, syscall synchronization, and required target checks pass without weakening architecture gates.

**Architecture:** Repair stale callers at the owner-approved public interface, group high-arity wire data into typed values, restore exact canonical documents omitted by the snapshot, and generate external artifacts through repository-owned build scripts. Architecture, boundary, and invariants debt remain separate Phase B/C work and are not hidden.

**Tech Stack:** Rust `no_std` + host tests, Cargo/rustfmt/Clippy, txKernel `cargo xtask`, Markdown/JSON progress records, RV64 cross compilation.

---

## File Map

- `xtask/src/test.rs`: rustfmt-only repair.
- `crates/tx-observe/src/l2_producer/runtime.rs`: typed step-end input.
- `crates/tx-observe/tests/smoke.rs`: step-end API witness.
- `crates/tx-substrate/tests/v3_{pr7_delegate_runtime,pr7b_mailbox_integration,endpoint_scope_abandonment}.rs`: remove retired deadline argument.
- `crates/tx-shims/src/linux_syscall/signal.rs`: complete dual-publication exit call.
- `crates/tx-subsystems/src/process/execution.rs`: authoritative plural exit API, unchanged unless a regression test requires it.
- `crates/tx-time/src/{hal,keeper,timer/min_heap}.rs`: direct Clippy repairs.
- `crates/tx-reactor/src/{hart_loop,runtime/publication,scheduler/placement,hart/local,runtime/delivery,runtime/drive_facade,runtime/hart_runtime}.rs`: typed high-arity inputs and direct Clippy repairs.
- `crates/tx-subsystems/src/net/execution/step_connect.rs`, `crates/tx-ext4/src/read_backend.rs`, `crates/tx-fs/src/tx_ext4_bridge.rs`, `crates/tx-kernel/src/init.rs`, `crates/tx-kernel/src/init/reactor_submit.rs`: unused cleanup.
- `crates/tx-subsystems/src/{signal/adapter.rs,pipe,tty,userfaultfd,io_uring}` and affected tests: public endpoint migration.
- `xtask/src/{syscall,syscall_status}.rs`: agree on compound guarded syscall arms.
- `docs/progress/SYSCALL_STATUS.md`: generated sections plus manual summary reconciliation.
- Seven exact canonical documents copied from `/Users/3y/Downloads/Tx/docs/` into matching paths in this worktree.
- `tools/netfast/build.sh`: existing artifact generator; modify only if its documented compiler fallback is broken.

### Task 1: Restore Format And Typed Observe Step-End API

**Files:**
- Modify: `xtask/src/test.rs`
- Modify: `crates/tx-observe/src/l2_producer/runtime.rs`
- Modify: `crates/tx-observe/tests/smoke.rs`

- [ ] **Step 1: Pin the high-arity failure**

Run:

```sh
cargo clippy -p tx-observe --all-targets -- -D warnings
```

Expected: fail at `L2Producer::step_end` with `too_many_arguments`.

- [ ] **Step 2: Add a typed API witness**

Update the smoke test to construct one step-end outcome value and pass it with
the span. The value must retain variant, progress kind/value, errno, yielded,
and cancelled fields.

- [ ] **Step 3: Verify the new witness fails before implementation**

Run the focused smoke test. Expected: compile failure because the typed input
does not exist yet.

- [ ] **Step 4: Implement the minimal typed input**

Add a producer-facing value type and change `step_end` to:

```rust
pub fn step_end(&self, step_span: SpanId, outcome: StepEndOutcome)
```

Keep the existing `PayloadStepOutcome` encoding and bool-to-wire conversion.

- [ ] **Step 5: Format and verify**

Run:

```sh
cargo fmt --all
cargo fmt --check
cargo test -p tx-observe --test smoke
cargo clippy -p tx-observe --all-targets -- -D warnings
```

- [ ] **Step 6: Commit**

```sh
git add xtask/src/test.rs crates/tx-observe/src/l2_producer/runtime.rs crates/tx-observe/tests/smoke.rs
git commit -m "fix(ci): type observe step completion input"
```

### Task 2: Repair Stale Delegate Registry Test Calls

**Files:**
- Modify: `crates/tx-substrate/tests/v3_pr7_delegate_runtime.rs`
- Modify: `crates/tx-substrate/tests/v3_pr7b_mailbox_integration.rs`
- Modify: `crates/tx-substrate/tests/v3_endpoint_scope_abandonment.rs`

- [ ] **Step 1: Reproduce the stale arity**

Run each integration test with `--no-run`; expected: calls to
`install_request` supply one retired trailing `None`.

- [ ] **Step 2: Add a mechanical inventory assertion**

Record the exact remaining six-argument call count with:

```sh
rg -n -U 'install_request\([\s\S]{0,500}?Arc::downgrade\([^\n]+\),\n\s+None,' crates/tx-substrate/tests
```

Expected before fix: one or more matches. Expected after fix: zero.

- [ ] **Step 3: Remove only the retired deadline argument**

Do not change `DelegateRegistry` or restore timer ownership.

- [ ] **Step 4: Verify**

```sh
cargo test -p tx-substrate --test v3_pr7_delegate_runtime
cargo test -p tx-substrate --test v3_pr7b_mailbox_integration
cargo test -p tx-substrate --test v3_endpoint_scope_abandonment
```

- [ ] **Step 5: Commit**

```sh
git add crates/tx-substrate/tests/v3_pr7_delegate_runtime.rs crates/tx-substrate/tests/v3_pr7b_mailbox_integration.rs crates/tx-substrate/tests/v3_endpoint_scope_abandonment.rs
git commit -m "test(substrate): follow delegate request timer boundary"
```

### Task 3: Complete Signal Exit Dual Publication

**Files:**
- Modify: `crates/tx-shims/src/linux_syscall/signal.rs`
- Test: closest existing signal syscall test module

- [ ] **Step 1: Add a compile/runtime regression witness**

Pin both LA64 SIGCANCEL paths to the owner-aware dual-publication API. The test
must observe one weak-mailbox signal post and one borrowed-mailbox wake post.

- [ ] **Step 2: Verify RED**

Run the focused signal test or LA64 package check. Expected: missing singular
function or missing wake callback.

- [ ] **Step 3: Implement the minimal repair**

Use:

```rust
step_exit_group_with_signal_with_posts(
    process,
    signum,
    |mailbox, event| ctx.post_mailbox_event(mailbox, event),
    |mailbox, event| ctx.post_mailbox_ref_event(mailbox, event),
)
```

at both callsites.

- [ ] **Step 4: Verify and commit**

Run the focused test and LA64 target check, then commit only the signal paths
and test.

### Task 4: Burn Down Clippy And Unused Failures

**Files:**
- Modify the files listed in the File Map for `tx-time`, `tx-reactor`, network,
  ext4, tx-fs, and tx-kernel.

- [ ] **Step 1: Capture the complete warning inventory**

Run the exact CI Clippy command into `/tmp/tx-baseline-clippy.log`. Group by
crate and lint name.

- [ ] **Step 2: Write focused tests for typed Reactor transaction inputs**

Add compile-time or unit witnesses proving publication and hart-loop deadline
data are passed as coherent values rather than independent scalar arguments.

- [ ] **Step 3: Verify RED**

Run focused Reactor tests; expected compile failure against the desired typed
constructors.

- [ ] **Step 4: Implement typed request/options values**

Group publication enqueue state and lifecycle hooks without changing
transaction ordering. Group `deadline_changed` and `next_deadline_ns` into one
deadline-update value.

- [ ] **Step 5: Apply direct warning repairs**

Use `Default`, typed `transmute`, `is_empty`, `contains`, `?`, function items,
test gating, deletion, or parameter removal as indicated by each diagnostic.
Do not add allowances.

- [ ] **Step 6: Verify**

```sh
cargo xtask lint unused
cargo clippy --no-deps --workspace --all-targets \
  --exclude tx-kernel-riscv64-qemu-virt \
  --exclude tx-kernel-riscv64-m1dock-mock \
  --exclude tx-kernel-loongarch64-qemu-virt -- -D warnings
```

- [ ] **Step 7: Commit by owner**

Use separate commits for tx-time, Reactor, and cross-crate unused cleanup.

### Task 5: Repair Broad Host Integration API Drift

**Files:**
- Modify stale tests under `crates/tx-subsystems`, `crates/tx-shims/tests`, and
  signal adapter imports.

- [ ] **Step 1: Run the exact host test gate**

Expected: compile failures identify private wait-source helpers or missing
signal adapter vocabulary.

- [ ] **Step 2: Add public-endpoint regression assertions**

Tests must obtain `Arc<WaitSource>` through the existing endpoint accessor and
must not require visibility widening.

- [ ] **Step 3: Migrate stale tests and complete the signal adapter**

Use public `*_endpoint()` methods and owner-local wait-routing imports.

- [ ] **Step 4: Verify**

Run focused packages, then the exact serialized workspace test gate.

- [ ] **Step 5: Commit by owning crate**

### Task 6: Restore Canonical Documents And Progress References

**Files:**
- Create exact missing paths under `docs/design` and `docs/stage2-documents` by
  copying audited files from the main checkout.
- Modify: `docs/progress/plans/2026-07-13-ext4-io-manager-write-path.json` only
  if a second referenced progress file is still absent after the audited copy.

- [ ] **Step 1: Verify source identity**

For each missing destination, confirm the same relative source path exists in
`/Users/3y/Downloads/Tx`, contains non-placeholder content, and is referenced
by the current index or progress record.

- [ ] **Step 2: Copy with `apply_patch`**

Add exact source content; do not retarget links to weaker documents.

- [ ] **Step 3: Verify**

```sh
cargo xtask lint docs
cargo xtask progress validate
```

- [ ] **Step 4: Commit**

Commit the restored canonical documents and corrected progress references as
one snapshot-consistency change.

### Task 7: Unify Syscall Status Parsers And Regenerate

**Files:**
- Modify: `xtask/src/syscall.rs`
- Modify: tests in the same module
- Modify generated sections: `docs/progress/SYSCALL_STATUS.md`

- [ ] **Step 1: Add a failing compound-guard parser test**

The fixture must contain both `NR_FADVISE64_64` and alias `NR_FADVISE64` on one
guard and assert the numeric canonical arm is retained.

- [ ] **Step 2: Verify RED**

Run the focused xtask test; expected: parser selects the last alias or drops
the arm.

- [ ] **Step 3: Implement the minimal parser correction**

Make both generators choose the same canonical numeric token without changing
the syscall table.

- [ ] **Step 4: Regenerate and reconcile manual text**

```sh
cargo xtask syscall-status --regen
cargo xtask syscall sync
```

Update the manual refresh date and headline counts to match generated output.

- [ ] **Step 5: Verify and commit**

```sh
cargo xtask syscall-status --check
cargo xtask lint syscall-status
```

### Task 8: Generate Vendor Artifact And Close Required Target Checks

**Files:**
- Generated, gitignored: `tools/images/vendor/tx-netfast-riscv64`
- Modify `tools/netfast/build.sh` only if the existing supported compiler path
  fails for a repository reason.

- [ ] **Step 1: Run the existing generator**

```sh
tools/netfast/build.sh
```

Use the documented GNU cross compiler or `zig cc -target riscv64-linux-musl`.

- [ ] **Step 2: Verify artifact properties**

Confirm it is non-empty, executable-format RV64 ELF, and ignored by git.

- [ ] **Step 3: Run required target checks**

```sh
cargo check -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf
cargo check -p tx-kernel-riscv64-m1dock-mock --target riscv64gc-unknown-none-elf
cargo check -p tx-kernel-loongarch64-qemu-virt --target loongarch64-unknown-none-softfloat
```

- [ ] **Step 4: Fix newly exposed compile errors with TDD**

For each error, add the nearest compile or unit witness before changing
production code. Do not check in the vendor artifact unless repository policy
explicitly changes in a separate approved decision.

### Task 9: Phase A Integration And Progress Catch-Up

**Files:**
- Modify: `docs/progress/STATUS.md`
- Modify: `docs/progress/plans/2026-07-19-reactor-baseline-closure.json`
- Modify: `docs/progress/worktrees/2026-07-19-reactor-baseline-closure.json`

- [ ] **Step 1: Run Phase A gates fresh**

Run format, Clippy, host check/tests, docs, unused, syscall, progress, and target
checks from the design verification ladder.

- [ ] **Step 2: Record exact residual gates**

Run architecture, boundary, and invariants reports and record exact counts for
Phase B/C. Do not call Phase A complete if any non-ratchet gate remains red.

- [ ] **Step 3: Update progress records and validate JSON**

```sh
cargo xtask progress validate
git diff --check
```

- [ ] **Step 4: Commit**

```sh
git add docs/progress/STATUS.md docs/progress/plans/2026-07-19-reactor-baseline-closure.json docs/progress/worktrees/2026-07-19-reactor-baseline-closure.json
git commit -m "docs(progress): record baseline closure phase A"
```
