# Tx Time Module Migration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the substrate-owned `TimerWheel` and the transitional `tx-services::time` implementation with a physical `tx-time` crate, while preserving reactor ownership of concrete deadline domains and semantic-owner time behavior.

**Architecture:** `tx-time` owns time capability APIs, timekeeper/VVAR payloads, typed HAL/RTC adapters, and a private indexed-min-heap timer engine that stores only `TimerKey + deadline`. `tx-reactor::ReactorTimerDomain` owns each engine instance, its key-to-delivery route table, one-driver claim, deadline-change wake, hardware arm/cancel, and owner-aware task routing. `tx-services` remains a compatibility re-export only until all callers have moved.

**Tech Stack:** Rust 2021 `no_std` kernel crates, `SpinMutex`, `Arc`/`Weak`, `tx-hal` static `P: TxPlatform`, workspace invariant lints, host cargo tests, RV64 QEMU smoke tests.

---

## Scope and File Topology

The target files are intentionally organized around ownership rather than the
old crate boundaries:

| Area | Create or modify | Responsibility |
|---|---|---|
| Workspace | `Cargo.toml`, `crates/tx-time/Cargo.toml` | Add a dependency-minimal `tx-time` crate. |
| Consumer facade | `crates/tx-services/src/time/{mod,clock,realtime,deadline,rtc,types,vvar,platform,driver}.rs` | Retain stable imports as re-exports during migration; remove concrete implementation. |
| Time core | `crates/tx-time/src/{api,keeper,vvar,rtc,timer,hal}.rs` | Own public time types, timekeeper/VVAR payloads, RTC adapter, and opaque-key engine. |
| Reactor integration | `crates/tx-reactor/src/{time_domain,time_route,hart_loop,runtime,lib}.rs` | Own engine instance, timer route table, expiry delivery, driver claim, and current-hart arm. |
| Kernel binding | `crates/tx-kernel/src/{init,thread_future,trap}.rs` | Use reactor domain arm surface, not `HalDeadlineTimer` directly. |
| Consumers | `crates/tx-{scripts,subsystems,shims,fs}/src/**` | Depend on `DeadlineRegistrar` and semantic target types; retain state in the semantic owner. |
| Retirement gates | `xtask/src/lint_invariants_{time_layering,time_wake}.rs` | Forbid old timer imports and new HAL/queue escapes. |

Out of scope: network-stack timer/readiness semantics, board-specific RTC
register drivers or firmware protocols, CPU-time/TAI completion, a hierarchical
wheel, and moving `tx-vdso` assembly or VM/exec VVAR mapping.

## Execution Rules

1. Execute in an isolated worktree because the current checkout is dirty.
2. Do not add `.superpowers/` or any browser/webview artifact.
3. Run every command with `CARGO_TARGET_DIR=target/codex-time-topology` while
   this migration is active.
4. Preserve semantic ownership: timerfd owns expiration/rebase state; POSIX and
   ITIMER own signal/overrun state; futex and epoll own wait/readiness state;
   RTC owns device/alarm state. A deadline expiry is a hint, not the semantic
   transition itself.
5. A queue lock protects only queue/slot state. It must be released before a
   callback, wait-source publication, mailbox post, scheduler enqueue, or IPI.

### Task 1: Add the `tx-time` crate and compatibility facade

**Files:**
- Modify: `Cargo.toml`
- Create: `crates/tx-time/Cargo.toml`
- Create: `crates/tx-time/src/lib.rs`
- Modify: `crates/tx-services/Cargo.toml`
- Modify: `crates/tx-services/src/time/mod.rs`
- Test: `crates/tx-time/src/lib.rs`

- [ ] **Step 1: Write compile-time facade coverage before moving any implementation.**

  Add this `tx-services` test, which fixes the public import contract:

  ```rust
  #[test]
  fn compatibility_time_facade_reexports_tx_time_types() {
      let _: tx_time::DeadlineNs = DeadlineNs::new(7);
      let _: Option<tx_time::TimerGuard> = None;
      assert_eq!(ClockId::Monotonic, tx_time::ClockId::Monotonic);
  }
  ```

- [ ] **Step 2: Run the focused facade test and confirm the crate is absent.**

  Run: `CARGO_TARGET_DIR=target/codex-time-topology cargo test -p tx-services --lib compatibility_time_facade_reexports_tx_time_types -- --exact`

  Expected: compile failure because package `tx-time` does not exist.

- [ ] **Step 3: Add the crate with only allowed lower dependencies.**

  Add `"crates/tx-time"` to workspace members and create:

  ```toml
  [package]
  name = "tx-time"
  version.workspace = true
  edition.workspace = true
  license.workspace = true
  repository.workspace = true

  [dependencies]
  tx-hal = { path = "../tx-hal" }
  tx-substrate = { path = "../tx-substrate" }

  [lib]
  path = "src/lib.rs"
  test = true
  doctest = false

  [lints]
  workspace = true
  ```

  Start `crates/tx-time/src/lib.rs` with only module declarations and public
  re-exports. It must not depend on `tx-reactor`, `tx-kernel`, `tx-shims`, or
  semantic subsystem crates.

- [ ] **Step 4: Turn `tx-services::time` into an explicit compatibility facade.**

  Add `tx-time = { path = "../tx-time" }` to `crates/tx-services/Cargo.toml`.
  Re-export the public surface without a wildcard:

  ```rust
  pub use tx_time::{
      ClockId, ClockRead, DeadlineNs, DeadlineRegistrar, DeadlineRegistrarHandle,
      RealtimeControl, RtcDeviceOps, TimeError, TimerGuard, TimerKey, TimerRole,
      TimerTarget, VvarPublisher,
  };
  pub mod platform {
      pub use tx_time::hal::{HalDeadlineTimer, HalMonotonicClock};
      pub use tx_time::rtc::HalRtcDevice;
  }
  ```

  Keep compatibility-only module comments that point new code to `tx_time`.
  Do not change any non-time call site in this task.

- [ ] **Step 5: Verify the crate edge and record a bounded checkpoint.**

  Run:

  ```sh
  CARGO_TARGET_DIR=target/codex-time-topology cargo test -p tx-time -p tx-services --lib
  CARGO_TARGET_DIR=target/codex-time-topology cargo check -p tx-reactor -p tx-kernel
  ```

  Expected: both crates compile; `tx-time` has no upward crate dependency.
  Commit only the owned scaffold in the isolated worktree:

  ```sh
  git add Cargo.toml crates/tx-time crates/tx-services/Cargo.toml crates/tx-services/src/time
  git commit -m "feat(time): add tx-time compatibility facade"
  ```

### Task 2: Move clock, timekeeper, VVAR payload, and typed RTC into `tx-time`

**Files:**
- Create: `crates/tx-time/src/api/{clock,realtime,deadline,rtc}.rs`
- Create: `crates/tx-time/src/keeper/{mod,state,convert,set}.rs`
- Create: `crates/tx-time/src/vvar/{mod,layout,calibrate,publish}.rs`
- Create: `crates/tx-time/src/rtc/{mod,device,alarm}.rs`
- Create: `crates/tx-time/src/hal.rs`
- Modify: `crates/tx-services/src/time/{clock,realtime,rtc,types,vvar,platform}.rs`
- Modify: `crates/tx-subsystems/src/{wall_clock,time_hooks}.rs`
- Test: `crates/tx-time/src/{keeper,vvar,rtc}/mod.rs`

- [ ] **Step 1: Add failing timekeeper/VVAR serialization tests.**

  Place focused tests next to `Timekeeper` and `VvarPublisher`:

  ```rust
  #[test]
  fn realtime_set_publishes_one_matching_generation_and_offset() {
      let keeper = Timekeeper::new_for_test(1_000, 10);
      let report = keeper.set_realtime_ns(2_000, 1_100).unwrap();
      let vvar = keeper.vvar_snapshot();
      assert_eq!(vvar.realtime_offset_ns, report.realtime_offset_ns);
      assert_eq!(vvar.realtime_generation, report.realtime_generation);
      assert_eq!(vvar.sequence & 1, 0);
  }
  ```

  The writer uses one serialization guard and an odd/even VVAR sequence
  publication around the copied snapshot; readers retry while it is odd or
  changes.

- [ ] **Step 2: Run the test to verify the current split cannot provide the new owner.**

  Run: `CARGO_TARGET_DIR=target/codex-time-topology cargo test -p tx-time --lib realtime_set_publishes_one_matching_generation_and_offset -- --exact`

  Expected: test or `Timekeeper` symbol is absent.

- [ ] **Step 3: Move the stable API types and implement the lower time core.**

  Define the public capability surface in `tx_time`:

  ```rust
  pub trait ClockRead {
      fn monotonic_now_ns(&self) -> u64;
      fn realtime_now_ns(&self) -> u64;
  }

  pub trait RealtimeControl: ClockRead {
      fn set_realtime_ns(&self, realtime_ns: u64) -> Result<RealtimeSetReport, TimeError>;
      fn realtime_generation(&self) -> u64;
  }

  pub trait RtcDeviceOps {
      fn read_time_ns(&self) -> Result<u64, TimeError>;
      fn set_time_ns(&self, ns: u64) -> Result<(), TimeError>;
      fn set_alarm_ns(&self, ns: u64) -> Result<(), TimeError>;
      fn clear_alarm(&self) -> Result<(), TimeError>;
      fn acknowledge_alarm_irq(&self) -> Result<(), TimeError>;
  }
  ```

  Move the existing wall-clock behavior into `keeper`: monotonic reads come
  from `MonotonicCounterIf::read_ns`, realtime is monotonic plus a protected
  offset, and persistent-clock writeback follows the current required/best
  effort policy. Move the VVAR `repr(C)` layout, calibration parameters,
  snapshot, and publisher into `tx_time::vvar`; leave address-space mapping
  and `AT_SYSINFO_EHDR` untouched.

- [ ] **Step 4: Move the static HAL adapters without adding a dynamic manager.**

  Keep the exact static shape:

  ```rust
  pub struct HalMonotonicClock<P>(core::marker::PhantomData<P>);
  pub struct HalDeadlineTimer<P>(core::marker::PhantomData<P>);
  pub struct HalRtcDevice<P>(core::marker::PhantomData<P>);
  ```

  `HalMonotonicClock<P>` adapts `MonotonicCounterIf`; `HalRtcDevice<P>` adapts
  `PersistentClockIf` and maps `PersistentClockError` to `TimeError`.
  `IrqIf::RTC_IRQ` remains owned by trap/IRQ binding. Make old service files
  re-export these definitions rather than duplicate them.

- [ ] **Step 5: Verify behavior and preserve VM/vDSO boundaries.**

  Run:

  ```sh
  CARGO_TARGET_DIR=target/codex-time-topology cargo test -p tx-time --lib
  CARGO_TARGET_DIR=target/codex-time-topology cargo test -p tx-services --lib time
  CARGO_TARGET_DIR=target/codex-time-topology cargo test -p tx-fs --lib devfs_rtc_ops_route_through_persistent_clock_backend -- --exact
  CARGO_TARGET_DIR=target/codex-time-topology cargo test -p tx-kernel --lib vdso
  ```

  Expected: writer sequence integrity, RTC adapter operations, and existing
  vDSO mapping tests pass. Commit the self-contained core move.

### Task 3: Implement the opaque-key indexed min-heap timer engine

**Files:**
- Create: `crates/tx-time/src/timer/{mod,key,guard,queue,min_heap,engine}.rs`
- Modify: `crates/tx-time/src/lib.rs`
- Test: `crates/tx-time/src/timer/{min_heap,engine}.rs`

- [ ] **Step 1: Add contract tests before writing the queue.**

  Add tests for earliest ordering, deterministic same-deadline ordering,
  idempotent cancel, rearm invalidating former expiry, and batch extraction:

  ```rust
  #[test]
  fn drain_due_returns_keys_without_delivery_payload() {
      let mut engine = TimerEngine::new();
      let late = engine.insert(DeadlineNs::new(20));
      let early = engine.insert(DeadlineNs::new(10));
      assert_eq!(engine.drain_due(10), vec![early]);
      assert_eq!(engine.next_deadline_ns(), Some(20));
      assert_ne!(early, late);
  }
  ```

- [ ] **Step 2: Run the new contract test and confirm it fails.**

  Run: `CARGO_TARGET_DIR=target/codex-time-topology cargo test -p tx-time --lib drain_due_returns_keys_without_delivery_payload -- --exact`

  Expected: compilation failure until `TimerEngine` exists.

- [ ] **Step 3: Define opaque keys, guards, and the private queue contract.**

  `TimerKey` is a nonzero opaque id plus generation. `TimerGuard` contains a
  cancellation handle and key only; drop cancellation is idempotent. Keep the
  algorithm contract private:

  ```rust
  trait TimerQueue {
      fn insert(&mut self, key: TimerKey, deadline_ns: u64);
      fn remove(&mut self, key: TimerKey) -> bool;
      fn rearm(&mut self, key: TimerKey, deadline_ns: u64) -> bool;
      fn drain_due(&mut self, now_ns: u64, out: &mut Vec<TimerKey>);
      fn next_deadline_ns(&self) -> Option<u64>;
  }
  ```

  Do not put `TaskMailbox`, `WaitSourceId`, raw queues, callbacks, harts, or
  signal state in `TimerKey`, heap entries, slots, or guards.

- [ ] **Step 4: Implement the .NET-style indexed binary min-heap.**

  Each live slot stores `key`, `deadline_ns`, and current heap index. The heap
  order is `(deadline_ns, key.raw())`; `insert`, `remove`, and `rearm` repair
  the heap in `O(log n)`. `drain_due` removes eligible entries under the engine
  lock and appends keys to caller-owned storage. The engine returns the batch
  after releasing its lock, so it has no routing callback API to misuse.

- [ ] **Step 5: Verify queue invariants.**

  Run:

  ```sh
  CARGO_TARGET_DIR=target/codex-time-topology cargo test -p tx-time --lib timer::
  CARGO_TARGET_DIR=target/codex-time-topology cargo test -p tx-time --lib -- --test-threads=1
  ```

  Expected: all queue tests pass, including stale former expiry after rearm.
  Commit the standalone algorithm without changing reactor behavior.

### Task 4: Introduce `ReactorTimerDomain` and reactor-private routing

**Files:**
- Create: `crates/tx-reactor/src/{time_domain,time_route,hart_loop}.rs`
- Modify: `crates/tx-reactor/src/{deadline_registry,runtime,task,lib}.rs`
- Modify: `crates/tx-reactor/Cargo.toml`
- Test: `crates/tx-reactor/tests/{v3_timer_surface,v3_pr7b_timer_routing,reactor_smoke}.rs`

- [ ] **Step 1: Add failing runtime-domain tests.**

  Cover the behavior that the queue deliberately does not own:

  ```rust
  #[test]
  fn due_batch_is_routed_after_engine_lock_is_released() {
      let domain = ReactorTimerDomain::new_for_test();
      let observed_unlocked = Arc::new(AtomicBool::new(false));
      domain.register_test_route(10, Arc::clone(&observed_unlocked));
      domain.drive_due(10, &mut TestRouter::default());
      assert!(observed_unlocked.load(Ordering::Acquire));
  }
  ```

  Add tests for first-driver claim, earlier-deadline change signal, empty-domain
  cancel, and remote owner wake producing exactly one reschedule IPI.

- [ ] **Step 2: Run the focused reactor tests and confirm missing domain APIs.**

  Run: `CARGO_TARGET_DIR=target/codex-time-topology cargo test -p tx-reactor --test v3_timer_surface due_batch_is_routed_after_engine_lock_is_released -- --exact`

  Expected: compilation failure until `ReactorTimerDomain` is available.

- [ ] **Step 3: Add the domain and route table with a strict data split.**

  Implement:

  ```rust
  pub struct ReactorTimerDomain {
      engine: tx_time::timer::TimerEngine,
      routes: SpinMutex<TimerRouteTable>,
      driver: AtomicUsize,
      deadline_changed: AtomicBool,
  }

  enum TimerRoute {
      TaskMailbox(Weak<TaskMailbox>),
      SignalMailbox(Weak<TaskMailbox>),
      WaitSource { source: WaitSourceId, interests: InterestMask },
      Delegate(DelegateTokenId),
      Device(DeviceDelivery),
  }
  ```

  Registration allocates a key, writes its route, inserts the deadline, and
  signals a driver only when the earliest live deadline changed. Cancellation
  removes both engine entry and route. A driver claim is acquired before a
  shared domain is drained and released after routing/arm is complete.

- [ ] **Step 4: Route a due batch outside every queue lock.**

  `drive_due(now, router)` must call `engine.drain_due(now)` first, then remove
  routes and dispatch each `TimerRoute`. Task routes use the existing
  owner-aware mailbox router; signal routes post `SignalTimerFired`; wait-source
  routes use source publication; delegate and device routes use their existing
  adapter paths. No route may retain registration-hart affinity as routing
  truth.

- [ ] **Step 5: Replace the reactor registry surface while retaining the public registrar.**

  Replace `ReactorDeadlineRegistry` fields with one `ReactorTimerDomain` and
  make `Reactor::deadline_registrar_handle()` return the `tx_time`
  compatibility handle. Keep `ReactorTimeDriver` behavior at this boundary,
  but make it call `domain.drive_due` and `domain.next_deadline_ns`.

- [ ] **Step 6: Verify routing and SMP witnesses.**

  Run:

  ```sh
  CARGO_TARGET_DIR=target/codex-time-topology cargo test -p tx-reactor --test v3_timer_surface -- --nocapture
  CARGO_TARGET_DIR=target/codex-time-topology cargo test -p tx-reactor --test v3_pr7b_timer_routing -- --nocapture
  CARGO_TARGET_DIR=target/codex-time-topology cargo test -p tx-reactor --test reactor_smoke -- --nocapture
  ```

  Expected: existing task, delegate, and device route tests pass plus the new
  no-callback-under-lock, driver-claim, and remote-IPI tests. Commit this
  reactor-only transition.

### Task 5: Bind hardware deadline programming to the domain driver

**Files:**
- Modify: `crates/tx-time/src/hal.rs`
- Modify: `crates/tx-reactor/src/{time_domain,hart_loop,runtime}.rs`
- Modify: `crates/tx-kernel/src/{init,thread_future,trap}.rs`
- Test: `crates/tx-kernel/src/{init,thread_future}/tests.rs`

- [ ] **Step 1: Add a failing earlier-deadline wake test at the hart-loop boundary.**

  ```rust
  #[test]
  fn earlier_remote_registration_reprograms_driver_before_idle() {
      let mut timer = RecordingDeadlineTimer::default();
      let domain = ReactorTimerDomain::new_for_test();
      domain.register_test_deadline(100);
      domain.arm_current_hart(&mut timer);
      domain.register_test_deadline(10);
      domain.consume_deadline_change_and_arm(&mut timer);
      assert_eq!(timer.last_programmed(), Some(10));
  }
  ```

- [ ] **Step 2: Run it and confirm direct kernel timer ownership has not been removed.**

  Run: `CARGO_TARGET_DIR=target/codex-time-topology cargo test -p tx-kernel --lib earlier_remote_registration_reprograms_driver_before_idle -- --exact`

  Expected: test cannot compile before the domain arm API exists.

- [ ] **Step 3: Make `HalDeadlineTimer<P>` a narrow current-hart adapter.**

  Keep its `DeadlineTimerIf` implementation in `tx_time::hal`, but expose it
  only through a reactor driver method:

  ```rust
  pub trait CurrentHartDeadlineTimer {
      fn set_current_hart_deadline_ns(&mut self, deadline_ns: u64);
      fn cancel_current_hart_deadline(&mut self);
  }

  impl ReactorTimerDomain {
      pub fn arm_current_hart<T: CurrentHartDeadlineTimer>(&self, timer: &mut T) { /* next or cancel */ }
  }
  ```

  The hart loop alone consumes `deadline_changed`, drives due work in normal
  reactor context, and calls `arm_current_hart`.

- [ ] **Step 4: Remove direct deadline programming from thread futures.**

  Replace `HalDeadlineTimer::<P>` use in `thread_future.rs` with a passed or
  retrieved reactor-domain driver surface. Trap IRQ code may acknowledge/cancel
  the hardware interrupt, but it must not fire keys or route tasks. Delete the
  obsolete direct arm helper after all current users use the domain.

- [ ] **Step 5: Verify current-hart arm/cancel and build platform bindings.**

  Run:

  ```sh
  CARGO_TARGET_DIR=target/codex-time-topology cargo test -p tx-kernel --lib thread_future -- --nocapture
  CARGO_TARGET_DIR=target/codex-time-topology cargo test -p tx-kernel --lib init -- --nocapture
  CARGO_TARGET_DIR=target/codex-time-topology cargo check -p tx-kernel -p tx-platform-adapter
  ```

  Expected: only reactor-domain code programs a deadline; timer IRQ still
  reaches normal reactor context. Commit this HAL-to-reactor boundary.

### Task 6: Migrate semantic consumers and retire the script bridge

**Files:**
- Modify: `crates/tx-subsystems/src/timerfd/mod.rs`
- Modify: `crates/tx-shims/src/linux_syscall/{timerfd,posix_timer,signal,time,epoll,wait,ctx}.rs`
- Modify: `crates/tx-subsystems/src/{futex,epoll}/mod.rs`
- Modify: `crates/tx-fs/src/devfs/{mod,tests}.rs`
- Modify: `crates/tx-scripts/src/drive.rs`
- Modify: `crates/tx-services/src/time/deadline.rs`
- Test: `crates/tx-shims/src/linux_syscall/tests/{time_syscalls,timerfd_dispatch,futex_dispatch}.rs`
- Test: `crates/tx-fs/src/devfs/tests.rs`

- [ ] **Step 1: Add semantic regression tests for target/role preservation.**

  Add or update coverage to require `DeadlineAbort` for temporary syscall wait
  deadlines and keep semantic state at the owner:

  ```rust
  #[test]
  fn futex_timeout_uses_deadline_abort_not_primary_sleep() {
      let request = build_positive_timeout_futex_wait();
      assert_eq!(request.timer_role(), TimerRole::DeadlineAbort);
  }
  ```

  Preserve timerfd periodic rearm, realtime rebase/cancel-on-set, POSIX
  overrun scanning, `ITIMER_REAL` signal hints, epoll wait timeout, and RTC
  device-event readiness tests.

- [ ] **Step 2: Run focused existing consumer tests to establish the baseline.**

  Run:

  ```sh
  CARGO_TARGET_DIR=target/codex-time-topology cargo test -p tx-shims --lib timerfd_dispatch -- --nocapture
  CARGO_TARGET_DIR=target/codex-time-topology cargo test -p tx-shims --lib futex_dispatch -- --nocapture
  CARGO_TARGET_DIR=target/codex-time-topology cargo test -p tx-fs --lib devfs_rtc_emulated_alarm_uses_timer_router_raw_queue_wake -- --exact
  ```

  Expected: baseline is recorded before swapping the registrar backend.

- [ ] **Step 3: Move producers to `DeadlineRegistrar` without giving them queue ownership.**

  Use `TimerTarget::WaitSource` for timerfd/readiness, `SignalTarget` for POSIX
  and interval timers, `TaskMailbox` for temporary futex/epoll waits,
  `DelegateToken` for delegate timeout, and device delivery for RTC emulation.
  Each producer stores only its own `TimerGuard` plus semantic state. No
  producer may import `TimerWheel`, `TimerEngine`, `ReactorTimerDomain`, or
  `HalDeadlineTimer`.

- [ ] **Step 4: Replace generic script `OnTimer` lowering with the facade.**

  Modify `tx-scripts::drive` and the syscall context bridge so `OnTimer`
  registers through `DeadlineRegistrar`. Remove
  `DeadlineRegistrarHandle::into_substrate_registrar_for_script_bridge` after
  the new path handles guard lifetime/cancellation. Retain `OnWaitSource` as a
  wait-source shape; a timeout installed beside it uses `DeadlineAbort`.

- [ ] **Step 5: Migrate RTC emulation to typed device delivery.**

  Preserve hardware RTC IRQ as `acknowledge_alarm_irq` then device event
  publication. For software alarm fallback, register a device route that
  publishes the RTC object's readiness source after expiry. Do not model an
  RTC alarm as a direct task timeout or introduce board details into devfs.

- [ ] **Step 6: Verify semantic behavior.**

  Run:

  ```sh
  CARGO_TARGET_DIR=target/codex-time-topology cargo test -p tx-shims --lib time_syscalls -- --nocapture
  CARGO_TARGET_DIR=target/codex-time-topology cargo test -p tx-shims --lib timerfd_dispatch -- --nocapture
  CARGO_TARGET_DIR=target/codex-time-topology cargo test -p tx-shims --lib futex_dispatch -- --nocapture
  CARGO_TARGET_DIR=target/codex-time-topology cargo test -p tx-fs --lib devfs -- --nocapture
  CARGO_TARGET_DIR=target/codex-time-topology cargo test -p tx-scripts --lib drive -- --nocapture
  ```

  Expected: all timer consumers retain their Linux-visible behavior without
  importing a concrete queue. Commit the consumer cutover.

### Task 7: Delete the old registry and make the lints enforce the topology

**Files:**
- Delete: `crates/tx-substrate/src/wake/timer.rs`
- Modify: `crates/tx-substrate/src/wake/mod.rs`
- Delete: `crates/tx-reactor/src/deadline_registry.rs`
- Modify: `crates/tx-reactor/src/{lib,runtime,task,wait}.rs`
- Modify: `xtask/src/lint_invariants_{time_layering,time_wake}.rs`
- Modify: `docs/stage2-documents/time_infra/TX_TIMER_SUBSYSTEM_DESIGN_CN.md`
- Test: `xtask/src/lint_invariants_{time_layering,time_wake}.rs`

- [ ] **Step 1: Add linter fixtures that fail on forbidden imports.**

  Add fixture cases for these prohibited forms outside their allowlisted owners:

  ```rust
  use tx_substrate::wake::timer::TimerWheel;
  use tx_time::timer::TimerEngine;
  use tx_time::hal::HalDeadlineTimer;
  ```

  The first must always fail after deletion. The second is allowed only in
  `tx-reactor::time_domain`; the third is allowed only in the reactor arm
  adapter/hart-loop and narrow trap handoff allowlist.

- [ ] **Step 2: Run the invariant fixture tests and confirm the new patterns are initially uncovered.**

  Run: `CARGO_TARGET_DIR=target/codex-time-topology cargo test -p xtask --lib lint_invariants_time -- --nocapture`

  Expected: fixture assertion fails until the lints recognize the new imports.

- [ ] **Step 3: Tighten `time-layering` and `time-wake-retired`.**

  Extend `time-layering` to make `tx-time` the only generic HAL-time adapter
  home, `tx-reactor::time_domain` the only engine owner, and semantic crates
  capability-only consumers. Extend `time-wake-retired` to reject all old
  `TimerWheel` and script-bridge imports. Change report-only ceilings to zero
  only after all production sites migrate.

- [ ] **Step 4: Delete obsolete implementation and compatibility escape hatches.**

  Remove `wake::timer`, `ReactorDeadlineRegistry`, the old substrate registrar
  types, and the script substrate lowerer. Delete their imports and re-exports
  rather than preserving type aliases; no source may be left with a path that
  makes a concrete queue look like a supported interface.

- [ ] **Step 5: Verify lints and source absence.**

  Run:

  ```sh
  CARGO_TARGET_DIR=target/codex-time-topology cargo xtask lint invariants time-layering
  CARGO_TARGET_DIR=target/codex-time-topology cargo xtask lint invariants time-wake-retired
  rg -n 'tx_substrate::wake::timer|TimerWheel|into_substrate_registrar_for_script_bridge' crates xtask
  ```

  Expected: both lints report zero production findings and `rg` has no source
  result except explicit retired-name linter fixture text. Commit the retirement
  with its lint ratchet.

### Task 8: Run end-to-end gates and close the migration record

**Files:**
- Modify: `docs/progress/plans/2026-07-13-tx-time-module-migration.json`
- Modify: `docs/progress/STATUS.md`
- Modify: `docs/superpowers/specs/2026-07-13-tx-time-module-design.md`
- Test: focused host and QEMU time/reaction paths

- [ ] **Step 1: Run the host time and reactor gate ladder.**

  ```sh
  CARGO_TARGET_DIR=target/codex-time-topology cargo test -p tx-time --lib -- --test-threads=1
  CARGO_TARGET_DIR=target/codex-time-topology cargo test -p tx-reactor --test v3_timer_surface -- --nocapture
  CARGO_TARGET_DIR=target/codex-time-topology cargo test -p tx-reactor --test v3_pr7b_timer_routing -- --nocapture
  CARGO_TARGET_DIR=target/codex-time-topology cargo test -p tx-shims --lib time_syscalls -- --nocapture
  CARGO_TARGET_DIR=target/codex-time-topology cargo test -p tx-shims --lib timerfd_dispatch -- --nocapture
  CARGO_TARGET_DIR=target/codex-time-topology cargo test -p tx-shims --lib futex_dispatch -- --nocapture
  CARGO_TARGET_DIR=target/codex-time-topology cargo test -p tx-fs --lib devfs -- --nocapture
  ```

  Expected: no queue algorithm callback runs under its lock; existing Linux
  time semantic tests retain their prior pass set.

- [ ] **Step 2: Run the target build and bounded QEMU witnesses.**

  ```sh
  CARGO_TARGET_DIR=target/codex-time-topology cargo xtask full-build --target rv64-qemu
  CARGO_TARGET_DIR=target/codex-time-topology cargo xtask test smoke --target rv64-qemu
  ```

  Expected: boot and timer interrupt handoff work. If a guest lane fails for an
  unrelated dirty-tree blocker, record the command, failure identity, and
  affected scope without weakening the migration lints.

- [ ] **Step 3: Run documentation and progress gates.**

  ```sh
  CARGO_TARGET_DIR=target/codex-time-topology cargo xtask progress validate
  CARGO_TARGET_DIR=target/codex-time-topology cargo xtask lint docs
  git diff --check
  ```

  Expected: valid progress JSON, no broken active-doc links/stale terminology,
  and no whitespace errors.

- [ ] **Step 4: Update durable status with actual evidence.**

  Mark every completed JSON step `complete`, replace each verification status
  with its command/result, and change the plan status to `complete` only after
  Task 7's zero-findings lints and Task 8's required gates pass. Update
  `STATUS.md` with changed ownership, exact verification, next step, and any
  blocker. Update the design status from implementation pending to the verified
  completion scope; leave network, real-board RTC, CPU/TAI, and vDSO mapping
  explicitly deferred.

- [ ] **Step 5: Commit only the fully verified owned migration.**

  ```sh
  git add Cargo.toml crates/tx-time crates/tx-services crates/tx-reactor \
    crates/tx-kernel crates/tx-subsystems crates/tx-shims crates/tx-fs \
    crates/tx-scripts xtask docs
  git commit -m "refactor(time): centralize timer and timekeeper subsystem"
  ```

  Expected: the commit contains no browser artifacts, unrelated dirty-tree
  changes, or board RTC implementation.

## Acceptance Matrix

| Contract | Evidence |
|---|---|
| `tx-time` has no upward subsystem dependency | `cargo tree -p tx-time` and crate manifest review |
| Heap stores opaque keys only | queue unit tests plus source boundary lint |
| Routing happens after engine lock release | reactor reentrant route test |
| Shared domain has one driver and reprogram signal | reactor single-driver and earlier-deadline tests |
| Cross-hart wake resolves owner at delivery time | reactor SMP route/one-IPI test |
| Time semantics remain owner-local | timerfd, POSIX/ITIMER, futex, epoll, RTC focused tests |
| VVAR writer publication is coherent | concurrent set/read snapshot test |
| Old substrate wheel has no surviving public path | zero-findings retired-interface lint |
| HAL remains static and typed | no runtime manager, `HalRtcDevice<P>` and arm-adapter tests |

## Risks and Rollback Boundaries

- A direct queue-to-callback port would reintroduce lock inversion and stale
  owner routing. Stop at Task 4 and fix the route split before consumer moves.
- Replacing a timerfd expiry with only a mailbox wake loses periodic/rebase
  semantics. Keep timerfd semantic tests green before deleting old adapters.
- Treating an RTC alarm as a task deadline breaks poll/epoll device readiness.
  Hardware and emulated RTC alarms both publish device-owned state.
- A global shared engine without driver claim can double-fire under SMP. Do not
  enable AP timer driving until the claim and remote reprogram tests pass.
- This plan does not certify CPU/TAI, networking, board RTC hardware, or full
  user VDSO mapping; they remain separately tracked work after this topology
  migration.
