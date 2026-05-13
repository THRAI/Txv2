# D50 — tx-reactor adapter: substrate refs routed through `#[platform_adapter]`

**Date:** 2026-05-13
**Scope:** `crates/tx-reactor/src/{adapter.rs, hart_loop.rs, agent_reply.rs, mailbox.rs, wait_source.rs, timer.rs, wait.rs, runtime.rs}`, `crates/tx-reactor/Cargo.toml`, `xtask/src/lint.rs`
**Depends on:** D49 (Phase 8 boundary ratchet CI gate at 208)

---

## 1. Context

Phase 7 (D17–D48) routed `tx_substrate::*` outside-adapter lines from 1446 to
208 across all consumer crates. tx-reactor was intentionally deferred because
its `tx_reactor::*` refs don't count against its own boundary (home-crate
exclusion), but its `tx_substrate::*` refs DO count as outside-adapter. The
top offenders were `hart_loop.rs` (step_v3 StepOp impls), `agent_reply.rs`
(step_v3 + wake types), plus back-compat re-export shims in `mailbox.rs`,
`wait_source.rs`, `timer.rs` and the bus wire consumers in `wait.rs` and
`runtime.rs`.

---

## 2. What landed

- **`crates/tx-reactor/Cargo.toml`**: added `tx-platform-adapter` dependency.

- **`crates/tx-reactor/src/adapter.rs`** (new): two `#[platform_adapter]` domains:
  - `step_engine` (platform = "substrate"): re-exports `step_v3::{StepOp,
    StepOutcome, NoProgress, ScriptCtx, SubjectIdentity, AbortReason,
    DelegateRegistry, DelegateReply, DelegateTokenId}` plus
    `ProcessIdentity as PlaceholderProcessSubject`.
  - `bus_wire` (platform = "substrate"): re-exports `bus::{DeclaredPort,
    DeclaredQueue, WireDeclaration, WireDeclarationError, WireEventSet, ...}`,
    `wake::{agent_event_matches, MailboxEvent, TaskMailbox}`,
    `wake::mailbox`, `wake::wait_source`, and
    `wake::timer::{TimerGuard, TimerGuardRole, TimerToken, TimerWheel}`.

- **`crates/tx-reactor/src/lib.rs`**: added `pub mod adapter;`.

- **`crates/tx-reactor/src/hart_loop.rs`**: migrated two `StepOp` impls
  (`HartLoopOp`, `HartLoopAtOp`) and the test module imports from
  `tx_substrate::step_v3::*` → `crate::adapter::step_engine::*`.
  `ProcessIdentity` consumed as `PlaceholderProcessSubject`.

- **`crates/tx-reactor/src/agent_reply.rs`**: migrated step_v3 and wake imports
  to `crate::adapter::step_engine::*` and `crate::adapter::bus_wire::*`.

- **`crates/tx-reactor/src/mailbox.rs`**: changed `pub use tx_substrate::wake::mailbox::*`
  → `pub use crate::adapter::bus_wire::mailbox::*`.

- **`crates/tx-reactor/src/wait_source.rs`**: changed `pub use tx_substrate::wake::wait_source::*`
  → `pub use crate::adapter::bus_wire::wait_source::*`.

- **`crates/tx-reactor/src/timer.rs`**: changed `pub use tx_substrate::wake::timer::{...}`
  → `pub use crate::adapter::bus_wire::{TimerGuard, ...}`.

- **`crates/tx-reactor/src/wait.rs`**: changed bus import from `tx_substrate::bus::{...}`
  → `crate::adapter::bus_wire::{...}`.

- **`crates/tx-reactor/src/runtime.rs`**: changed bus import from `tx_substrate::bus::{...}`
  → `crate::adapter::bus_wire::{...}`.

- **`xtask/src/lint.rs`**: lowered `MAX_SUBSTRATE_OUTSIDE_ADAPTER` from 208 to 192.

---

## 3. Key substitution patterns

| Before | After |
|--------|-------|
| `tx_substrate::step_v3::StepOp<I>` | `crate::adapter::step_engine::StepOp<I>` |
| `tx_substrate::step_v3::StepOutcome<...>` | `crate::adapter::step_engine::StepOutcome<...>` |
| `tx_substrate::step_v3::ProcessIdentity` | `crate::adapter::step_engine::PlaceholderProcessSubject` |
| `tx_substrate::step_v3::{AbortReason, ...}` | `crate::adapter::step_engine::{AbortReason, ...}` |
| `tx_substrate::wake::{agent_event_matches, ...}` | `crate::adapter::bus_wire::{agent_event_matches, ...}` |
| `tx_substrate::wake::mailbox::*` | `crate::adapter::bus_wire::mailbox::*` |
| `tx_substrate::wake::wait_source::*` | `crate::adapter::bus_wire::wait_source::*` |
| `tx_substrate::wake::timer::{TimerGuard, ...}` | `crate::adapter::bus_wire::{TimerGuard, ...}` |
| `tx_substrate::bus::{DeclaredPort, ...}` | `crate::adapter::bus_wire::{DeclaredPort, ...}` |

Allowed residue (doc-comment `tx_substrate::*` paths in module-level comments)
remains unchanged per the Phase 7 policy.

---

## 4. Ratchet adjustment

| Metric | Before (D49) | After (D50) |
|--------|-------------|-------------|
| substrate outside adapters | 208 lines | 192 lines |
| substrate inside adapters | 153 lines | 155 lines |
| reactor outside adapters | 4 lines | 4 lines |
| platform adapters declared | 45 | 47 |
| `MAX_SUBSTRATE_OUTSIDE_ADAPTER` | 208 | 192 |

---

## 5. Verification

```
$ cargo build -p tx-reactor
Finished `dev` profile [unoptimized + debuginfo] target(s)

$ cargo test -p tx-reactor --lib -- --test-threads=1
running 2 tests
test hart_loop::step_op_wraps::hart_loop_at_op_delegates_to_step_hart_loop_at ... ok
test hart_loop::step_op_wraps::hart_loop_op_delegates_to_step_hart_loop_via_clock ... ok
test result: ok. 2 passed; 0 failed; 0 ignored

$ cargo xtask lint boundary
Architecture Boundary Ratchet
=============================
substrate outside adapters:  192 lines  (ceiling 192)  ok
reactor   outside adapters:    4 lines  (ceiling 4)  ok
```

---

## 6. Acceptance

- [x] `crates/tx-reactor/src/adapter.rs` created with two `#[platform_adapter]` domains.
- [x] All `tx_substrate::*` production refs in tx-reactor routed through adapter.
- [x] `cargo build -p tx-reactor` clean.
- [x] `cargo test -p tx-reactor --lib` 2/2 pass.
- [x] `cargo xtask lint boundary` passes at new ceiling of 192.
- [x] `MAX_SUBSTRATE_OUTSIDE_ADAPTER` lowered from 208 → 192.
