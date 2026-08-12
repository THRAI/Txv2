# Time/Timer Topology Audit

Date: 2026-07-12

Scope: current worktree alignment against
`docs/design/02_execution/TIME_WAKE_v1.md` and
`docs/stage2-documents/time_infra/TX_TIMER_SUBSYSTEM_DESIGN_CN.md`.

## Evidence Snapshot

- `cargo test -p xtask time_layering -- --nocapture`: passed 24/24 with
  `CARGO_TARGET_DIR=target/codex-time-topology`.
- `cargo xtask lint invariants time-layering`: 0 findings.
- `cargo xtask lint invariants time-wake-retired`: 0 retired sites.
- `cargo xtask lint docs`: ok, with the existing 7 stale-vocabulary warnings.
- `cargo xtask progress validate`: ok.
- `git diff --check`: ok.
- `cargo test -p tx-reactor --test reactor_smoke timer_wheel_expiry --
  --nocapture`: passed 2/2 with `CARGO_TARGET_DIR=target/codex-time-topology`.
- `cargo test -p tx-reactor --test reactor_smoke
  mixed_producer_wakes_repeatedly_route_current_owner -- --nocapture`: passed
  1/1 with `CARGO_TARGET_DIR=target/codex-time-topology`.
- `cargo test -p tx-reactor --test reactor_smoke
  broad_owner_aware_producer_stress_routes_remote_wakes -- --nocapture`:
  passed 1/1 with `CARGO_TARGET_DIR=target/codex-time-topology`.
- `cargo test -p xtask qemu_args_require_owner_wake_marker -- --nocapture`:
  passed 2/2 with `CARGO_TARGET_DIR=target/codex-time-topology`.
- `cargo xtask test smoke --target rv64-qemu --timeout-ms 60000`: passed with
  `CARGO_TARGET_DIR=target/codex-time-topology`; QEMU observed
  `txkernel:qemu-riscv64-virt:boot:ok` plus
  `txkernel:qemu-riscv64-virt:reactor:owner-wake:smp:ok`.
- `cargo xtask test busybox-boot --target rv64-qemu --timeout-ms 60000`:
  passed with `CARGO_TARGET_DIR=target/codex-time-topology`; QEMU observed
  `txkernel:qemu-riscv64-virt:boot:ok` plus
  `txkernel:qemu-riscv64-virt:reactor:owner-wake:smp:ok`.

## Requirement Audit

Sources audited:

- `TIME_WAKE_v1.md` Requirements, Ownership Matrix, Interface Catalog,
  Implementation Readiness Checklist, Interface Retirement Gate, Module
  Acceptance Tests, and Package F/G status callouts.
- `TX_TIMER_SUBSYSTEM_DESIGN_CN.md` design-complete/implementation-complete
  criteria, module-boundary table, linter/file-boundary table, producer
  contract, and verification plan.

Scope decision:

- The topology/import/API consolidation target covers Packages A-E, the
  non-network Package G wake-post boundary, and the typed RTC route foundation
  through QEMU/no-RTC profiles.
- Real-board or firmware-backed RTC evidence beyond QEMU is Package F external
  board-backend work. It remains required for full hardware coverage, but it is
  not a blocker for declaring the current timer topology and upper/lower
  interface boundary closed.
- Broad dirty-tree `--tests` coverage is still unavailable in this checkout due
  to unrelated integration-test blockers. The current claim is therefore
  scoped to the focused gates and lints listed above, not to whole-tree tests.

| Requirement | Current evidence | Status |
|---|---|---|
| Upper producers use `tx_services::time` facades instead of raw HAL calls. | `time-layering` forbids raw HAL time traits/calls outside HAL, boards, observe, and service time adapter homes; focused time syscall/timer tests were already passing in this checkpoint series. | Proven for current production scan. |
| Concrete `TimerWheel` / current-wheel bridge is not public API. | `time-layering` forbids raw `TimerWheel`, `current_timer_wheel`, `set_current_timer_wheel`, and `registrar_handle()` outside substrate/service/reactor private homes. `time-wake-retired` remains 0. | Proven for current production scan. |
| `tx_services::time` is the facade home for deadline producers. | `DeadlineRegistrar`, `DeadlineRegistrarHandle`, `TimerRole`, `TimerTarget`, service-owned `DeviceTimerCallback`, and service-owned `TimerGuard` live under `crates/tx-services/src/time/`. | Implemented. |
| Service facade does not directly re-export substrate callback/guard implementation types. | `time-layering` rejects substrate `DeviceTimerCallback` and `TimerGuard` re-exports from `deadline.rs`; `TimerGuard` exposes only `token()` / `forget()`. | Proven by lint fixtures and real-file witness. |
| Direct `tx_substrate::wake::timer` imports stay in lower/driver homes. | New `upper production direct substrate timer import` rule confines production imports to `tx-substrate`, `tx-scripts`, reactor private driver/runtime files, and `tx-services::time` adapters. | Proven by lint gate. |
| Semantic objects do not store registrar handles or private wheels. | `time-layering` rejects `DeadlineRegistrarHandle`, `TimerRegistrarHandle`, and `TimerWheel` fields under `tx-subsystems/src/`; semantic objects store `TimerGuard` / `TimerToken` / pending state. | Proven for current production scan. |
| `SyscallCtx` stores service facade handle, while `ScriptCtx` remains the lower bridge. | `SyscallCtx.timer_registrar` stores `DeadlineRegistrarHandle`; only `build_subject_script_ctx()` calls `into_substrate_registrar_for_script_bridge()`. | Proven by lint fixture and real-file witness. |
| Public adapters do not leak substrate timer implementation surface. | `time-layering` covers `tx-reactor`, `tx-shims`, and `tx-kernel` adapter public files for timer module/export names; `tx-subsystems::adapter` cannot re-export `tx_substrate::wake` wholesale. | Proven by lint fixtures. |
| Reactor owns due driving and owner-aware wake placement, not timerfd/POSIX/RTC semantics. | `TimerWakeRouter` production implementation is in `tx-reactor/src/runtime.rs`; semantic producers store state and recheck on wake. Existing focused witnesses cover timerfd, POSIX timer, ITIMER, and devfs RTC paths. `reactor_smoke::timer_wheel_expiry_from_remote_hart_requests_remote_ipi_and_requeues_owner` proves a due timer fired from a non-owner hart requeues on the owner hart and requests one remote IPI. `mixed_producer_wakes_repeatedly_route_current_owner` and `broad_owner_aware_producer_stress_routes_remote_wakes` prove wait-source, timer, delegate, signal, channel, device wait-source callback, and device RawQueue callback rows enter the owner-aware route under host SMP-shaped non-owner wake. Current RV64 QEMU smoke and busybox lanes both require and observed the `reactor:owner-wake:smp:ok` marker. | Implemented for focused producers, host mixed-producer owner-aware wake stress, and current RV64 QEMU marker lanes. |
| `TimerToken` handling is deliberate and documented. | `TimerToken` remains a shared substrate event token because mailbox events and `TimerWakeRouter` carry it for stale-wake filtering. The timer design doc names `TimerToken` as a semantic-object stale-wake identity, while service-owned `TimerGuard` hides substrate guard inspection. Wrapping `TimerToken` later would require a mailbox conversion seam and is a separate design choice, not the current topology blocker. | Accepted current boundary. |

## Implementation Readiness Matrix

| Area | Required evidence from design docs | Current evidence | Status |
|---|---|---|---|
| HAL split | Boards expose separate monotonic counter and deadline timer capabilities; no aggregate `TimeIf` active interface. | `time-wake-retired` is 0; `time-layering` raw HAL production scan is 0; `TIME_WAKE_v1.md` status records `TimeIf` retired from active Rust code and `TxPlatform` naming split traits directly. | Closed for topology scan. |
| Timekeeper | Clock syscalls, VFS/stat timestamps, vDSO/VVAR, and realtime setters use `TimekeeperIf`/service facade. | `tx_services::time::wall_clock` is the public facade home; old `tx_subsystems::wall_clock` path is retired by docs and lint; focused time syscall/stat/vDSO evidence is in the checkpoint series. | Closed for current focused scope. |
| Persistent writeback | Realtime mutation updates timekeeper generation first; RTC writeback is optional policy effect. | `TIME_WAKE_v1.md` records `clock_settime`/`settimeofday` writeback policy helper; `RealtimeControl::set_realtime_ns_with_timerfd_post` is the syscall-facing seam. | Closed for topology; hardware success still board-dependent. |
| Timer registry | `OnTimer`, protocol deadlines, delegate deadlines, timerfd, POSIX timer, itimer, wait timeouts, and RTC alarm fallback use one registrar surface. | `DeadlineRegistrar` facade exists; `SyscallCtx` carries the service handle; `ScriptCtx` lowering is confined; focused witnesses cover timerfd, POSIX timer, `ITIMER_REAL`, sleep, futex, ppoll/pselect/epoll, delegate/device callback, and RTC alarm fallback. | Closed for non-network focused producers. |
| Reactor driver | Hardware deadline programming and due firing stay behind reactor driver boundary. | `ReactorTimeDriver` and `CurrentHartDeadlineTimer` are in `tx_services::time`; raw wheel/current-wheel bridge findings are 0; reactor private code owns concrete due driving. | Closed for current production scan. |
| Wake routing | Timer expiry goes through owner-aware post, not captured local waker state. | `ReactorOwnerWakePost` route is documented; host mixed-producer and broad producer stress pass; QEMU smoke/busybox both require the owner-wake SMP marker. | Closed for focused host + RV64 QEMU lanes. |
| SMP/post-steal | Wake-time owner is re-resolved; timer registry does not cache target hart as placement truth. | Remote-owner timer expiry witness passes; mixed-producer host witnesses cover non-owner wake rows; RV64 QEMU marker covers a real SMP boot lane. | Closed for RV64 QEMU plus host SMP-shaped tests. |
| RTC typed route | `/dev/rtc` routes through devfs `CharDeviceOps`/`RtcDeviceOps`; timekeeper RTC use is seed/writeback only; RTC read/poll events use device wait source. | `RtcDeviceOps` exists; QEMU RV64 goldfish and LA64 LS7A paths are documented; no-RTC unsupported paths are typed; RTC alarm/read/poll/IRQ focused witnesses are recorded. | Closed for typed route foundation; real-board backend evidence deferred. |
| Retirement | Active runtime code has no usable old `TimeIf`, private timer queue/future, router-free fire shortcut, raw RTC string dispatch, or broad public timer re-export. | `time-wake-retired` is 0; `time-layering` is 0 and includes adapter/public-export/direct-import/lowering rules. | Closed by hard gates. |
| Module/file boundary | Upper layers import facade traits; substrate owns registry data; reactor owns placement; HAL owns hardware; semantic objects own Linux state. | `time-layering` covers raw HAL calls, raw wheel/current-wheel, adapter leakage, direct substrate timer imports, `SyscallCtx` lower handle fields, and `tx-subsystems` semantic object handle fields. | Closed by hard gate for current production scan. |

## Acceptance Decision

The current tree is aligned with the timer-topology design for the scoped
non-network timer consolidation:

- Design-complete criteria are satisfied: every current time/timer path maps to
  one of `ClockRead`, `RealtimeControl`, `DeadlineRegistrar`,
  `ReactorTimeDriver`, or `RtcDeviceOps`; each row has an owner, lower
  dependency, forbidden dependency, and lint/review control.
- Implementation-complete criteria for topology/import/API cleanup are
  satisfied in the focused scope: `time-layering` production findings are 0,
  `time-wake-retired` is 0, the named producer witnesses exist, and the RV64
  QEMU owner-aware marker is enforced by smoke and busybox lanes.
- The claim does not include whole-tree `--tests`, network-stack semantic
  timer/readiness behavior, or real-board/firmware RTC backend validation.

## Remaining Work After Topology Closure

1. Re-enable broad dirty-tree `--tests` coverage after unrelated integration
   blockers are fixed, then rerun the same topology gates as a regression net.
2. Track real-board or firmware-backed RTC support as Package F board-backend
   work beyond topology cleanup; QEMU RTC and no-RTC typed paths do not prove
   real-board RTC behavior.
3. Keep network-stack timer/readiness as its own lane. It should consume these
   time interfaces later, but its semantics are outside this closure.
