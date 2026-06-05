# Notification Convergence Migration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Migrate wait/notification code so raw notification primitives are available only through marked convergence modules, while each subsystem owns semantic notification wrappers in `notification.rs`.

**Architecture:** Follow the existing `adapter.rs` pattern: marker macros document and expose the legitimate boundary, and `xtask` lints enforce the boundary mechanically. The first phase adds the marker/lint and migrates live users without retiring compatibility paths. Retirement of stale interfaces happens only after the migrated surface is stable and the lint baseline is low enough to tighten to a ban.

**Tech Stack:** Rust workspace, `tx-platform-adapter` proc macros, `cargo xtask lint invariants`, subsystem-local `adapter.rs` / `notification.rs`, `WaitSource`, `YieldShape::OnWaitSource`, existing legacy `wait_source` compatibility bridge.

---

## File Structure

- Modify: `crates/tx-platform-adapter/src/lib.rs`
  - Add a `#[notification_adapter(...)]` marker macro modeled on `#[platform_adapter(...)]`.
  - Emit a manifest constant so the marker exists in compiled code and docs.
- Modify: `crates/tx-platform-adapter/tests/expansion.rs`
  - Add expansion tests for stacked `#[platform_adapter]` plus `#[notification_adapter]` use.
- Create: `xtask/src/lint_invariants_notification.rs`
  - Add the lint that recognizes notification marker scopes and file-level migration allowlists.
- Modify: `xtask/src/lib.rs`
  - Register the new lint module.
- Modify: `xtask/src/lint.rs`
  - Add `cargo xtask lint invariants notification-boundary` and include it in `all`.
- Modify: `.agents/skills/tx-xtask/SKILL.md`
  - Document the new subrule.
- Create, per subsystem as migration lands:
  - `crates/tx-subsystems/src/<subsystem>/notification.rs`
  - Add `pub mod notification;` in the subsystem `mod.rs`.
- Modify, per subsystem:
  - `crates/tx-subsystems/src/<subsystem>/adapter.rs`
  - Keep raw primitive access behind `#[platform_adapter(... domain = "wait_routing" ...)]`.
  - Add a `#[notification_adapter(...)]` marked inline module only when the file defines notification code helpers directly.
- Modify, per subsystem:
  - `structure.rs` owns `Arc<WaitSource>` / wait-source ids as object truth-adjacent publication points.
  - `execution.rs` calls semantic notification verbs, not raw masks or raw primitive functions.
- Modify: `docs/progress/STATUS.md`
  - Record each phase and verification.

## Migration Rules

- Do not retire `crates/tx-subsystems/src/wait_source.rs` in the migration phase.
- Do not remove legacy `Channel`/`wait_on_token` paths until the equivalent semantic `notification.rs` surface has landed and tests are green.
- Do not introduce a global notification subsystem that owns truth. Object truth remains in subsystem structure/payload objects.
- `adapter.rs` is the primitive boundary. `notification.rs` is the semantic notification boundary.
- `execution.rs` may publish with verbs like `notify_readable`, `notify_space_available`, or `abort_removed`; it may not construct `Mask`, call `notify_v3_source`, or call `fire_legacy_channel` directly after its subsystem is migrated.

## Task 1: Add Notification Marker Macro

**Files:**
- Modify: `crates/tx-platform-adapter/src/lib.rs`
- Modify: `crates/tx-platform-adapter/tests/expansion.rs`

- [ ] **Step 1: Add failing macro expansion tests**

Add tests beside the existing platform-adapter expansion tests:

```rust
use tx_platform_adapter::{notification_adapter, platform_adapter};

#[notification_adapter(
    subsystem = "pipe",
    domain = "readiness",
    reason = "pipe notification.rs owns readable/writable wait masks and wake verbs"
)]
pub mod pipe_notification_boundary {
    pub fn readable_mask() -> u64 {
        1
    }
}

#[platform_adapter(
    platform = "substrate",
    domain = "wait_routing",
    apis = ["wake"],
    reason = "pipe adapter exposes raw WaitSource primitives"
)]
#[notification_adapter(
    subsystem = "pipe",
    domain = "primitive_codes",
    reason = "pipe adapter is allowed to bind primitive wait-code helpers"
)]
pub mod stacked_wait_boundary {
    pub fn writable_mask() -> u64 {
        2
    }
}

#[test]
fn notification_adapter_injects_manifest() {
    assert!(pipe_notification_boundary::__NOTIFICATION_ADAPTER.contains("subsystem=pipe"));
    assert!(pipe_notification_boundary::__NOTIFICATION_ADAPTER.contains("domain=readiness"));
}

#[test]
fn notification_adapter_can_stack_with_platform_adapter() {
    assert!(stacked_wait_boundary::__PLATFORM_ADAPTER_SUBSTRATE.contains("platform=substrate"));
    assert!(stacked_wait_boundary::__NOTIFICATION_ADAPTER.contains("subsystem=pipe"));
}
```

- [ ] **Step 2: Run the failing tests**

Run:

```bash
cargo test -p tx-platform-adapter --test expansion notification_adapter -- --nocapture
```

Expected: compile failure because `notification_adapter` does not exist.

- [ ] **Step 3: Implement the marker macro**

In `crates/tx-platform-adapter/src/lib.rs`, mirror `AdapterArgs` with:

```rust
struct NotificationAdapterArgs {
    subsystem: LitStr,
    domain: LitStr,
    reason: LitStr,
}
```

Validation rules:

```rust
fn validate_subsystem(s: &LitStr) -> syn::Result<()> {
    let value = s.value();
    if value.is_empty() {
        return Err(syn::Error::new(s.span(), "`subsystem` must be non-empty"));
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return Err(syn::Error::new(
            s.span(),
            format!("`subsystem = \"{value}\"` must be snake_case"),
        ));
    }
    Ok(())
}
```

The proc macro should require an inline module and inject:

```rust
#[doc(hidden)]
pub const __NOTIFICATION_ADAPTER: &str = "subsystem=<name>;domain=<domain>;reason=<reason>";
```

- [ ] **Step 4: Verify macro tests pass**

Run:

```bash
cargo test -p tx-platform-adapter --test expansion notification_adapter -- --nocapture
```

Expected: tests pass.

## Task 2: Add Report-First Notification Boundary Lint

**Files:**
- Create: `xtask/src/lint_invariants_notification.rs`
- Modify: `xtask/src/lib.rs`
- Modify: `xtask/src/lint.rs`
- Modify: `.agents/skills/tx-xtask/SKILL.md`

- [ ] **Step 1: Add a failing xtask compile check**

Run:

```bash
cargo check -p xtask
```

Expected before implementation: no failure yet. This is the baseline.

- [ ] **Step 2: Create the lint module**

Create `xtask/src/lint_invariants_notification.rs` with a scanner that:

- walks production Rust files under `crates/tx-subsystems/src`;
- skips tests, `*/tests.rs`, and `*/tests/*`;
- treats `crates/tx-subsystems/src/wait_source.rs` as an explicit compatibility exception during migration;
- records whether a file contains `#[notification_adapter(`;
- recognizes subsystem-local `notification.rs` files as intended semantic homes;
- counts raw notification primitive uses outside allowed files.

Start with these raw patterns:

```rust
const RAW_NOTIFICATION_PATTERNS: &[&str] = &[
    "wait_source::register_wait_channel",
    "wait_source::release_wait_channel",
    "wait_source::wait_on_token",
    "crate::wait_source::register_wait_channel",
    "crate::wait_source::release_wait_channel",
    "crate::wait_source::wait_on_token",
    "tx_substrate::wake::notify",
    "tx_substrate::wake::new_source",
    "tx_substrate::wake::register_source",
    "tx_substrate::wake::unregister_source",
    "tx_reactor::wait::fire_legacy",
    "Mask::from_bits",
    "YieldShape::OnWaitSource",
    "StepOutcome::yield_on_wait_source",
];
```

Use a ratchet ceiling named:

```rust
const MAX_NOTIFICATION_BOUNDARY_VIOLATIONS: usize = <measured baseline>;
```

Do not guess the value. Temporarily set it to `0`, run the lint, then replace it with the exact live baseline.

- [ ] **Step 3: Wire the lint**

Add to `xtask/src/lib.rs`:

```rust
mod lint_invariants_notification;
```

Add to `xtask/src/lint.rs`:

```rust
"notification-boundary" => crate::lint_invariants_notification::lint_invariants_notification_boundary(root),
```

Add it to the `all` rule list after `legacy-wait-channel`, because it is a stricter follow-up boundary over the same migration surface.

- [ ] **Step 4: Measure the baseline**

Run:

```bash
cargo xtask lint invariants notification-boundary
```

Expected: failure with the exact number of current production violations.

Set `MAX_NOTIFICATION_BOUNDARY_VIOLATIONS` to that number and add a comment:

```rust
// Baseline captured YYYY-MM-DD before subsystem notification.rs migration.
// Lower this after each migrated subsystem.
```

- [ ] **Step 5: Verify the report-first lint**

Run:

```bash
cargo xtask lint invariants notification-boundary
cargo xtask lint invariants all
cargo check -p xtask
```

Expected: all pass at the measured baseline.

## Task 3: Pilot SysV Msg/Sem Notification Wrappers

**Files:**
- Create: `crates/tx-subsystems/src/ipc/sysv_msg/notification.rs`
- Create: `crates/tx-subsystems/src/ipc/sysv_sem/notification.rs`
- Modify: `crates/tx-subsystems/src/ipc/sysv_msg/mod.rs`
- Modify: `crates/tx-subsystems/src/ipc/sysv_sem/mod.rs`
- Modify: `crates/tx-subsystems/src/ipc/sysv_msg/structure.rs`
- Modify: `crates/tx-subsystems/src/ipc/sysv_sem/structure.rs`
- Modify: `crates/tx-subsystems/src/ipc/sysv_msg/execution.rs`
- Modify: `crates/tx-subsystems/src/ipc/sysv_sem/execution.rs`
- Modify: tests in `crates/tx-subsystems/src/ipc/sysv_msg/tests.rs`
- Modify: tests in `crates/tx-subsystems/src/ipc/sysv_sem/tests.rs`

- [ ] **Step 1: Add semantic notification files**

For SysV msg, create a wrapper with named meanings:

```rust
use crate::process::adapter::wait_routing::Mask;

pub const MSG_CAN_SEND: u64 = 1;
pub const MSG_CAN_RECV: u64 = 1;

pub fn send_mask() -> Mask {
    Mask::from_bits(MSG_CAN_SEND)
}

pub fn recv_mask() -> Mask {
    Mask::from_bits(MSG_CAN_RECV)
}
```

For SysV sem:

```rust
use crate::process::adapter::wait_routing::Mask;

pub const SEM_CHANGED: u64 = 1;

pub fn changed_mask() -> Mask {
    Mask::from_bits(SEM_CHANGED)
}
```

These initial files may still call through existing adapter types. The point of Task 3 is to remove magic masks and raw mask construction from structure/execution first.

- [ ] **Step 2: Route structure through notification names**

Replace direct `Mask::from_bits(1)` in SysV msg/sem structure/tests with `notification::send_mask()`, `notification::recv_mask()`, or `notification::changed_mask()`.

- [ ] **Step 3: Route execution wake names through notification verbs**

Add semantic verbs:

```rust
pub fn notify_message_available(channel: &crate::process::adapter::wait_routing::Channel) {
    crate::process::adapter::wait_routing::fire_legacy_channel(channel, MSG_CAN_RECV);
}

pub fn notify_space_available(channel: &crate::process::adapter::wait_routing::Channel) {
    crate::process::adapter::wait_routing::fire_legacy_channel(channel, MSG_CAN_SEND);
}
```

For SysV sem:

```rust
pub fn notify_changed(channel: &crate::process::adapter::wait_routing::Channel) {
    crate::process::adapter::wait_routing::fire_legacy_channel(channel, SEM_CHANGED);
}
```

Then call those verbs from execution. Do not remove existing legacy channels in this task.

- [ ] **Step 4: Verify SysV behavior**

Run:

```bash
cargo test -p tx-subsystems sysv_msg -- --nocapture
cargo test -p tx-subsystems sysv_sem -- --nocapture
cargo xtask lint invariants notification-boundary
cargo xtask lint invariants legacy-wait-channel
```

Expected: SysV tests pass. Boundary counts should stay the same or drop; if they drop, lower the ratchet to the exact new count.

## Task 4: Migrate Waitable Subsystems in Slices

**Files, by slice:**
- `crates/tx-subsystems/src/pipe/{adapter.rs,notification.rs,mod.rs}`
- `crates/tx-subsystems/src/futex/{adapter.rs,notification.rs,mod.rs}`
- `crates/tx-subsystems/src/eventfd/{adapter.rs,notification.rs,mod.rs}`
- `crates/tx-subsystems/src/signalfd/{adapter.rs,notification.rs,mod.rs}`
- `crates/tx-subsystems/src/timerfd/{adapter.rs,notification.rs,mod.rs}`
- `crates/tx-subsystems/src/userfaultfd/{adapter.rs,notification.rs,mod.rs}`
- `crates/tx-subsystems/src/aio/{adapter.rs,notification.rs,mod.rs}`
- `crates/tx-subsystems/src/io_uring/{adapter.rs,notification.rs,mod.rs}`
- `crates/tx-subsystems/src/vfs/{adapter.rs,notification.rs,mod.rs}`
- `crates/tx-subsystems/src/vm/{adapter.rs,notification.rs,mod.rs}`
- `crates/tx-subsystems/src/tty/{adapter.rs,notification.rs,mod.rs}`
- `crates/tx-subsystems/src/process/{adapter.rs,notification.rs,mod.rs}`

- [ ] **Step 1: Migrate one subsystem per commit**

For each subsystem, create `notification.rs` with domain names. Example for pipe:

```rust
pub const PIPE_READABLE: u64 = 1;
pub const PIPE_WRITABLE: u64 = 1;

pub fn notify_readable(wait: &crate::pipe::structure::PipeWaitState) {
    crate::pipe::adapter::wait_routing::fire_legacy_channel(&wait.reader_wait_channel, PIPE_READABLE);
    crate::pipe::adapter::wait_routing::notify_v3_source(&wait.reader_wait_source, PIPE_READABLE);
}

pub fn notify_writable(wait: &crate::pipe::structure::PipeWaitState) {
    crate::pipe::adapter::wait_routing::fire_legacy_channel(&wait.writer_wait_channel, PIPE_WRITABLE);
    crate::pipe::adapter::wait_routing::notify_v3_source(&wait.writer_wait_source, PIPE_WRITABLE);
}
```

Adjust exact field names to the subsystem being migrated. Keep this file semantic: no generic `notify(mask)` API.

- [ ] **Step 2: Remove raw primitive calls from migrated execution files**

After each subsystem migration, no `execution.rs` or `structure.rs` in that subsystem should call:

```rust
Mask::from_bits(...)
wait_routing::notify_v3_source(...)
wait_routing::fire_legacy_channel(...)
StepOutcome::yield_on_wait_source(...)
YieldShape::OnWaitSource { ... }
```

If a step needs to yield, expose a semantic wrapper in `notification.rs` or a named step verb in `adapter.rs::step_engine`, for example:

```rust
pub fn yield_until_readable(source_id: u64) -> StepOutcome<usize, ByteProgress> {
    crate::pipe::adapter::step_engine::yield_until_readable(source_id, PIPE_READABLE)
}
```

- [ ] **Step 3: Run slice-local tests**

Use the narrowest host tests for each slice first. Examples:

```bash
cargo test -p tx-subsystems pipe -- --nocapture
cargo test -p tx-subsystems futex -- --nocapture
cargo test -p tx-subsystems eventfd -- --nocapture
cargo test -p tx-subsystems signalfd -- --nocapture
cargo test -p tx-subsystems timerfd -- --nocapture
cargo test -p tx-subsystems userfaultfd -- --nocapture
cargo test -p tx-subsystems aio -- --nocapture
cargo test -p tx-subsystems io_uring -- --nocapture
cargo test -p tx-subsystems vfs -- --nocapture
cargo test -p tx-subsystems vm -- --nocapture
cargo test -p tx-subsystems tty -- --nocapture
cargo test -p tx-subsystems process -- --nocapture
```

If a filter has no tests, run:

```bash
cargo check -p tx-subsystems
```

- [ ] **Step 4: Lower ratchets after each slice**

Run:

```bash
cargo xtask lint invariants notification-boundary
cargo xtask lint invariants legacy-wait-channel
```

If either count drops, lower the corresponding `MAX_*` constant to the exact observed count. Do not lower a ratchet based on expected future cleanup.

## Task 5: Tighten Lint From Ratchet to Ban

**Files:**
- Modify: `xtask/src/lint_invariants_notification.rs`
- Modify: `xtask/src/lint_invariants_wait.rs`
- Modify: `docs/progress/decisions/YYYY-MM-DD-notification-boundary-tightening.md`
- Modify: `docs/progress/STATUS.md`

- [ ] **Step 1: Add a decision note**

Create a decision note that records:

- raw notification primitives are allowed only in marked `adapter.rs` modules and subsystem `notification.rs`;
- subsystem logic must call semantic notification verbs;
- compatibility `wait_source.rs` remains allowed only until the legacy bridge count reaches zero.

- [ ] **Step 2: Change the notification lint ceiling to zero**

After all migrated files pass:

```rust
const MAX_NOTIFICATION_BOUNDARY_VIOLATIONS: usize = 0;
```

Keep explicit exceptions only for:

- `crates/tx-subsystems/src/wait_source.rs` while legacy compatibility exists;
- tests;
- macro crate tests.

- [ ] **Step 3: Verify the ban**

Run:

```bash
cargo xtask lint invariants notification-boundary
cargo xtask lint invariants all
cargo xtask lint boundary
cargo xtask progress validate
```

Expected: all pass.

## Task 6: Retire Legacy Wait Interfaces After Migration

**Files:**
- Modify or delete: `crates/tx-subsystems/src/wait_source.rs`
- Modify: `crates/tx-shims/src/linux_syscall/*.rs` that still call `wait_on_token`
- Modify: `crates/tx-scripts/src/drive.rs`
- Modify: `crates/tx-subsystems/src/vm/execution.rs`
- Modify: `xtask/src/lint_invariants_wait.rs`
- Modify: active design/progress docs that mention D2 coexistence as live.

- [ ] **Step 1: Require zero legacy bridge sites**

Run:

```bash
cargo xtask lint invariants legacy-wait-channel
```

Expected before retirement: count is `0`. If count is not zero, stop and migrate the listed sites first.

- [ ] **Step 2: Remove or seal the compatibility registry**

Only after the count is zero, remove public access to:

```rust
register_wait_channel
release_wait_channel
wait_on_token
lookup_wait_channel
```

If the file still has an internal role, make the public API private and update the lint allowlist accordingly.

- [ ] **Step 3: Update interfaces and docs**

Update active docs and progress notes so they describe:

- object-owned `Arc<WaitSource>` publication points;
- per-subsystem `notification.rs` semantic wrappers;
- no production D2 coexistence path.

- [ ] **Step 4: Run final verification**

Run:

```bash
cargo fmt --check
cargo check -p tx-platform-adapter
cargo check -p xtask
cargo check -p tx-subsystems
cargo xtask lint invariants all
cargo xtask lint boundary
cargo xtask lint docs
cargo xtask progress validate
```

Expected: all pass. If `cargo xtask lint arch` is still blocked by the known `crates/tx-kernel/src/init.rs` file-size ratchet, report it separately and do not mix that blocker with notification migration.

## Self-Review

- Spec coverage: The plan covers marker macro creation, lint enforcement, migration-first sequencing, per-subsystem semantic wrappers, ratchet lowering, and retirement/interface update only after migration.
- Placeholder scan: No placeholder markers are used as implementation instructions.
- Type consistency: The marker macro is consistently named `notification_adapter`; the lint is consistently named `notification-boundary`; migration files are consistently named `notification.rs`.
- Scope check: The full migration spans many subsystems. Execute it slice-by-slice; Task 3 is the pilot and Task 4 is intentionally one subsystem per commit.
