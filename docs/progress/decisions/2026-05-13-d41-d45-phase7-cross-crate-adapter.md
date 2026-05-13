# D41-D45 — Phase 7 Cross-Crate Substrate Adapter Migration

**Date:** 2026-05-13
**Scope:** tx-ext4, tx-kernel, tx-scripts, tx-fs, tx-shims (five crates)
**Commits:** D41 f079e21, D42 876f9b5, D43 0f5d79c, D44 38a4607, D45 f0bfe61

---

## 1. Context

After Phase 7's D34-D40 wave completed the tx-subsystems migration (all
subsystem files now route substrate/reactor calls through per-subsystem
adapters), the remaining raw `tx_substrate::*` and `tx_reactor::*`
references lived in the five cross-layer consumer crates. This decision
covers the five commits that migrated those crates.

Baseline at start of D41: substrate outside-adapter 575 lines / 96 files,
reactor outside 22 lines / 17 files (from the D34-D40 boundary report).

---

## 2. Patterns established

### PlaceholderProcessSubject alias
`tx_substrate::step_v3::ProcessIdentity` is a collision-prone name at
call sites that also use the subsystem-layer `ProcessIdentity`. Each
crate that uses `ScriptCtx<ProcessIdentity>` defines:

```rust
pub use tx_substrate::step_v3::ProcessIdentity as PlaceholderProcessSubject;
```

Used in tx-kernel (adapter.rs) and tx-shims (adapter.rs → step_engine domain).

### ebr_guard alias for let-binding shadowing
Test files that contain multiple `let guard = guard()` bindings inside
a single function body shadow the imported `guard` function after the
first binding. Fix: import as `guard as ebr_guard` and write
`let guard = ebr_guard()` at each call site.

Files affected: dac_setuid_wave4.rs, fd_ops_wave2.rs (both in tx-shims).

### Two-domain adapter modules
tx-kernel and tx-shims both have two domains in their adapter:

- `step_engine` — substrate step_v3, zone, epoch, page_allocator, SpinMutex
- `boot_runtime` / `reactor_entry` — tx_reactor::userspace (SyscallRequest,
  UserspaceTrapInfo, etc.)

### Adapter granularity for tx-fs
tx-fs uses per-subdirectory adapters (tx-fs/src/tmpfs/adapter.rs,
tx-fs/src/devfs/adapter.rs) rather than a crate-root adapter, because
tmpfs and devfs are independently imported by external crates. The
initramfs_tests.rs file imports from `crate::tmpfs::adapter::step_engine`.

---

## 3. Files migrated per crate

### D41 — tx-ext4 (2 files)
- `crates/tx-ext4/src/pager.rs` — page_allocator/ZeroPolicy via adapter
- `crates/tx-ext4/src/tests_v3.rs` — epoch/step_v3/page_allocator via adapter

### D42 — tx-kernel (7 files)
- `crates/tx-kernel/src/adapter.rs` — extended: PayloadCap, page_allocator, init, init_on_ap
- `crates/tx-kernel/src/init.rs` — init/init_on_ap/SpinMutex/Cap via adapter
- `crates/tx-kernel/src/init/exec.rs` — page_allocator via adapter
- `crates/tx-kernel/src/init/tests.rs` — guard/page_allocator/StepOutcome/SyscallRequest via adapter
- `crates/tx-kernel/src/irq.rs` — SpinMutex via adapter
- `crates/tx-kernel/src/thread_future.rs` — PayloadCap via adapter
- `crates/tx-kernel/src/thread_future/tests.rs` — PayloadCap/userspace types via adapter
- `crates/tx-kernel/src/trap_handoff.rs` — PayloadCap via adapter
- `crates/tx-kernel/src/zones.rs` — ZoneError via adapter

### D43 — tx-scripts (3 files)
- `crates/tx-scripts/src/adapter.rs` — extended: page_allocator, SpinMutex
- `crates/tx-scripts/src/process/exec/script/tests.rs` — full adapter import
- `crates/tx-scripts/src/process/exec/script/tests/v3.rs` — adapter import with V3Errno/V3Outcome aliases

### D44 — tx-fs (5 files)
- `crates/tx-fs/src/tmpfs/adapter.rs` — extended: full step_v3 surface, page_allocator, SpinMutex
- `crates/tx-fs/src/tmpfs/mod.rs` — StepOutcome/YieldShape via adapter
- `crates/tx-fs/src/tmpfs/tests.rs` — guard/page_allocator/Errno/StepOutcome via adapter
- `crates/tx-fs/src/devfs/tests.rs` — guard/ByteProgress/Errno/StepOutcome via adapter
- `crates/tx-fs/src/initramfs_tests.rs` — full adapter import from tmpfs/adapter

### D45 — tx-shims (19 files)
- `crates/tx-shims/src/adapter.rs` — extended step_engine domain (full step_v3 surface,
  PlaceholderProcessSubject, reserve_for/sign_for, Cap, page_allocator, SpinMutex,
  guard/Guard) + reactor_entry domain (tx_reactor::userspace)
- `crates/tx-shims/src/lib.rs` — SubjectContext/SubjectAuthority/StepOp/StepOutcome/ScriptCtx via adapter
- 17 production + test files in `linux_syscall/` tree — all step_v3/epoch/zone/reactor usages via adapter

---

## 4. Boundary report (final)

Post-D45 `cargo xtask boundary-report`:
- Substrate outside adapters: **304 lines / 96 files**
- Substrate inside adapters: **150 lines / 23 files**
- Reactor outside adapters: **19 lines / 17 files**
- Reactor inside adapters: **18 lines / 14 files**
- Platform adapters declared: **43**

The remaining 304 outside-adapter substrate lines break down as:
- 109 epoch (drain_with_budget pattern + testing::init_host_for_test_once — allowed residue)
- 87 step_v3 (integration test files not in lib scope: crates/tx-shims/tests/, crates/tx-subsystems/tests/)
- 87 testing (init_host_for_test_once — allowed residue)
- 68 zone
- 63 wake
- 16 SpinMutex
- 12 page_allocator
- 6 bus
- 1 shootdown

The top offenders are all in integration test directories (`crates/tx-*/tests/`)
which are outside the lib migration scope and contain the allowed
`tx_substrate::testing::init_host_for_test_once` harness pattern.

---

## 5. Verification

Per-crate test runs (--test-threads=1):
- tx-ext4: 7/7 pass
- tx-kernel: 43/43 pass
- tx-scripts: 47/47 pass
- tx-fs: 39/39 pass
- tx-shims: 233/233 pass

`cargo build -p tx-shims` clean (19 unused-import warnings only —
pre-existing over-import in adapter; not migration regressions).

---

## 6. Deferred

Integration test files in `crates/tx-shims/tests/` and
`crates/tx-subsystems/tests/` still contain raw `tx_substrate::` /
`tx_reactor::` imports. These are outside the `--lib` boundary and use
the allowed `testing::init_host_for_test_once` harness. A follow-up
pass to route those through test-specific adapter shims is deferred
until integration test scaffolding stabilizes.
