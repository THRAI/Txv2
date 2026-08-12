# RTC Persistent-Clock Foundation

Date: 2026-07-06

## Summary

Implemented the Package F foundation from
[`TIME_WAKE_v1.md`](../../design/02_execution/TIME_WAKE_v1.md): HAL now has a
fallible persistent-clock capability, the timekeeper has realtime seed hooks,
the device layer has typed RTC operations, and `/dev/misc/rtc`
`RTC_RD_TIME` / `RTC_SET_TIME` / `RTC_ALM_READ` / `RTC_ALM_SET` route through
typed char-device ops instead of syscall-local RTC semantics. System realtime
setters now also have the Package F persistent-writeback policy hook.

## What Changed

- Added `tx_hal::PersistentClockIf` and `PersistentClockError`.
- Added explicit `PersistentClockIf` impls for current board platforms:
  - `boards/tx-hal-riscv64-qemu-virt` now implements the RV64 QEMU
    `google,goldfish-rtc` MMIO persistent-clock backend.
  - `boards/tx-hal-loongarch64-qemu-virt` now implements the LA64 QEMU
    LS7A RTC backend over the `ls7a_rtc` MMIO block.
  - `boards/tx-hal-riscv64-m1dock-mock` remains an explicit unsupported
    backend.
- Added `TimekeeperIf::seed_realtime_ns` and
  `TimekeeperIf::seed_realtime_from_persistent`.
- Added typed RTC device data and ops in `tx_subsystems::device`:
  `RtcTime`, `RtcAlarm`, `RtcEventMask`, `RtcError`, and `RtcDeviceOps`.
- Extended `CharDeviceOps` with a default `rtc_ops()` typed adapter.
- Changed devfs `RtcCharOps` to expose `RtcDeviceOps`.
- Changed `sys_ioctl(RTC_RD_TIME)` to call `binding.ops.rtc_ops().read_time()`
  and removed the syscall-local `RtcTime::fixed_oscomp_time` stub.
- Changed `sys_ioctl(RTC_SET_TIME)` to call `binding.ops.rtc_ops().set_time()`;
  the current devfs fallback backend returns `EOPNOTSUPP` until real persistent
  set-time support exists.
- Added `PersistentClockIf` to the `TxPlatform` supertrait boundary so
  persistent realtime is an explicit static platform capability, not an
  optional ad-hoc side channel.
- Wired kernel vDSO bootstrap to seed `TimekeeperIf` from
  `PersistentClockIf` before publishing the initial vvar snapshot. Unsupported
  board defaults are ignored and keep the fallback realtime epoch.
- Added a focused kernel unit test for the boot seed helper and updated
  full-`TxPlatform` host test stubs to implement the unsupported
  `PersistentClockIf` default.
- Retired the devfs RTC fixed-time fallback: `/dev/misc/rtc` now requires an
  installed typed persistent-clock backend and returns `Unsupported` otherwise.
- Added `RtcTime::to_unix_ns()` with calendar validation so `RTC_SET_TIME`
  can call persistent set-time through typed ops rather than accepting an
  opaque struct.
- Added a devfs RTC backend installer that binds the statically selected
  platform's `PersistentClockIf` callbacks to `RtcDeviceOps`; `CoreInit`
  installs it during boot before devfs is mounted.
- Updated RTC ioctl tests so `RTC_RD_TIME` and `RTC_SET_TIME` prove the path
  reaches a fake `PersistentClockIf` backend instead of a syscall-local or
  devfs-local stub.
- Added typed alarm routing for `RTC_ALM_READ` and `RTC_ALM_SET`: the shim
  only decodes the Linux ioctl ABI, while devfs `RtcDeviceOps` stores alarm
  state and calls `PersistentClockIf::set_wake_alarm_ns`.
- Added devfs and shim tests proving alarm set/read reach the fake persistent
  backend. Blocking RTC `read(2)` and poll/epoll wake semantics remain
  deferred to a device wait-source slice.
- Added `RealtimeWritebackPolicy` and `RealtimeSetReport` in
  `tx_subsystems::wall_clock` so system realtime mutation can report
  timekeeper generation separately from optional persistent writeback.
- Routed `clock_settime(CLOCK_REALTIME)` and `settimeofday` through best-effort
  `PersistentClockIf::set_realtime_ns` after accepted timekeeper mutation. A
  persistent-clock writeback failure is observable in the helper report but
  does not roll back kernel realtime or fail those system-clock syscalls.
- Updated syscall fake platforms and integration-test `StubPmap`s to implement
  the explicit `PersistentClockIf` boundary, usually as unsupported default.
- Updated `docs/design/02_execution/TIME_WAKE_v1.md` current-status and
  migration notes, and aligned `docs/design/01_substrate/HAL_v1.md` with the
  three time hardware capability traits.
- Added the RTC event foundation: devfs owns pending RTC event bits plus a
  registered RTC event wait token, `RtcDeviceOps::poll_events` reports pending
  state, RTC char-device `read(2)` consumes Linux-shaped 8-byte event records,
  and `ppoll`/`epoll` use typed `rtc_ops()` readiness instead of name-based RTC
  dispatch.
- Added true blocking RTC `read(2)` waits at the syscall/open-file boundary:
  the typed RTC char-device branch retries `RtcCharOps::read`, parks blocking
  fds on `tx_fs::devfs::rtc_event_wait_token()` when no event is pending, keeps
  nonblocking fds returning `EAGAIN`, and resumes by re-reading device pending
  state after RTC event publication.
- Added emulated RTC alarm publication: `TimerWheel` now supports a generic
  `DeviceEvent` callback registration that publishes into device-owned state
  without task-mailbox routing, `RtcDeviceOps::set_alarm_with_emulation` lets
  the RTC char device fall back when `PersistentClockIf::set_wake_alarm_ns`
  returns unsupported, and `RTC_ALM_SET` installs an emulated timer through
  `SyscallCtx.timer_registrar`. When the timer fires, it publishes
  `RtcEventMask::ALARM` into the RTC pending event path used by read/poll.
- Added the first real board backend: RV64 QEMU virt maps the DTB-proven
  `google,goldfish-rtc` MMIO block at `0x0010_1000`, reads realtime by loading
  TIME_LOW then TIME_HIGH, writes realtime high then low, programs alarms high
  then low then IRQ enable, and clears alarms/interrupts through board-local
  helpers. Focused host tests prove the MMIO region, register ordering, alarm
  programming, and PLIC IRQ 11 mask/unmask behavior.
- Added hardware RTC IRQ publication for the first board path:
  `tx_hal::IrqIf` now has optional `RTC_IRQ`, RV64 QEMU virt exposes PLIC IRQ
  11, `PersistentClockIf` has an IRQ acknowledgement hook,
  `tx-kernel::install_irq_handlers` pre-initializes the RTC event source before
  unmasking the RTC IRQ, and `rtc_alarm_irq_handler` acknowledges the hardware
  source before publishing `RtcEventMask::ALARM` into the existing devfs RTC
  pending-event/read/poll path.
- Added the LA64 QEMU virt RTC backend: the board now publishes an `ls7a-rtc`
  MMIO region at `0x100d_0100`, exposes `IrqIf::RTC_IRQ` as GSI 67,
  normalizes LS7A TOY calendar registers to Unix nanoseconds for
  `PersistentClockIf::read_realtime_ns` / `set_realtime_ns`, programs
  `TOYMATCH0` for wake alarms, and masks the external IRQ on alarm clear
  without disabling the TOY clock. Focused host tests prove MMIO publication,
  TOY read/set ordering, alarm programming/unmask, alarm clear/mask, and the
  RTC IRQ constant.

## Verification

- `cargo fmt --check` passed.
- `cargo check -p tx-hal -q` passed.
- `cargo check -p tx-hal-riscv64-qemu-virt -q` passed.
- `cargo check -p tx-hal-riscv64-m1dock-mock -q` passed.
- `cargo check -p tx-hal-loongarch64-qemu-virt -q` passed.
- `cargo check -p tx-subsystems -q` passed.
- `cargo check -p tx-fs -q` passed.
- `cargo check -p tx-shims -q` passed.
- `cargo check -p tx-kernel -q` passed.
- `cargo check -p tx-hal-riscv64-qemu-virt -q` passed.
- `cargo check -p tx-hal-riscv64-m1dock-mock -q` passed.
- `cargo check -p tx-hal-loongarch64-qemu-virt -q` passed.
- `cargo check -p tx-subsystems -q` passed.
- `cargo check -p tx-shims -q` passed.
- `cargo test -p tx-kernel boot_seed_helper_uses_persistent_clock_before_publish_path -- --nocapture`
  passed.
- `cargo test -p tx-kernel vdso --no-run` passed.
- `cargo test -p tx-substrate --no-run` passed.
- `cargo xtask progress validate` passed with `progress records: ok (29
  file(s))`.
- `cargo xtask lint docs` passed with the expected retired-term warning class
  and `docs lint: ok`.
- `cargo test -p tx-subsystems rtc_time_converts_default_realtime_epoch -- --nocapture`
  passed.
- `cargo test -p tx-subsystems seed_realtime_uses_persistent_clock_value_without_timer_notification_path -- --nocapture`
  passed.
- `cargo test -p tx-subsystems rtc_time -- --nocapture` passed.
- `cargo test -p tx-fs devfs_rtc_ops -- --nocapture` passed.
- `cargo test -p tx-shims dispatch_ioctl_rtc -- --nocapture` passed with
  `RTC_RD_TIME`, `RTC_SET_TIME`, and `RTC_ALM_*` coverage.
- `cargo test -p tx-shims dispatch_ioctl_rtc_rd_time_on_rtc_char_device_writes_rtc_time -- --nocapture`
  passed.
- `cargo test -p tx-shims dispatch_ioctl_rtc_set_time_on_rtc_char_device_reaches_typed_ops -- --nocapture`
  passed.
- `cargo check -p tx-fs -q` passed.
- `cargo test -p tx-subsystems persistent_writeback -- --nocapture` passed.
- `cargo test -p tx-shims dispatch_clock_settime -- --nocapture` passed.
- `cargo test -p tx-shims dispatch_settimeofday -- --nocapture` passed.
- `cargo check -p tx-kernel -q` passed with the existing unrelated warnings.
- `cargo test -p tx-fs devfs_rtc -- --nocapture` passed with RTC backend and
  event-readiness coverage.
- `cargo test -p tx-shims dispatch_ppoll_rtc_uses_typed_pending_event_readiness -- --nocapture`
  passed.
- `cargo test -p tx-shims epoll_dispatch -- --nocapture` passed after adding
  RTC typed readiness/source support to epoll.
- `cargo test -p tx-shims dispatch_read_rtc_blocks_until_event_then_returns_record -- --nocapture`
  first failed with immediate `Ready(Error(11))`, then passed after wiring the
  blocking wait path.
- `cargo test -p tx-shims dispatch_read_rtc -- --nocapture` passed with both
  blocking and nonblocking RTC read coverage.
- `cargo check -p tx-shims -q` passed.
- `cargo test -p tx-reactor timer_registrar_installs_device_callback_and_registry_fires_it_without_mailbox -- --nocapture`
  first failed because `DeviceTimerCallback`, `TimerGuardRole::DeviceEvent`,
  and `TimerRegistrarHandle::install_device_callback` did not exist, then
  passed after adding the generic device callback timer path.
- `cargo test -p tx-reactor dropping_device_callback_guard_cancels_before_fire -- --nocapture`
  passed.
- `cargo test -p tx-shims dispatch_ioctl_rtc_alarm_set_emulates_event_when_hardware_alarm_is_unsupported -- --nocapture`
  passed after proving `RTC_ALM_SET` falls back to an emulated timer and
  publishes `RtcEventMask::ALARM` only after the timer fire walk.
- `cargo fmt --check -p tx-hal-riscv64-qemu-virt` passed after the RV64
  goldfish backend.
- `cargo check -p tx-hal-riscv64-qemu-virt -q` passed after the RV64 goldfish
  backend.
- `cargo test -p tx-hal-riscv64-qemu-virt goldfish -- --nocapture` passed with
  6 focused tests covering the MMIO region, read/write ordering, alarm
  programming, alarm clear behavior, and interrupt acknowledgement.
- `cargo test -p tx-hal-riscv64-qemu-virt rtc -- --nocapture` passed for the
  RTC-related board-region witness.
- `cargo check -p tx-kernel -q` passed with the existing unrelated warnings.
- `cargo fmt --check -p tx-hal -p tx-kernel -p tx-hal-riscv64-qemu-virt`
  passed after the IRQ publication slice.
- `cargo check -p tx-hal -q` passed.
- `cargo test -p tx-kernel rtc_irq_handler_publishes_alarm_event_to_devfs_rtc_state -- --nocapture`
  passed.
- `cargo test -p tx-kernel install_irq_handlers_publishes_table_to_platform -- --nocapture`
  passed with RTC handler registration coverage.
- `cargo fmt --check -p tx-hal-loongarch64-qemu-virt` passed after the LA64
  LS7A RTC backend.
- `cargo check -p tx-hal-loongarch64-qemu-virt -q` passed.
- `cargo test -p tx-hal-loongarch64-qemu-virt ls7a_persistent_clock -- --nocapture`
  passed with 4 focused LS7A RTC tests.
- `cargo test -p tx-hal-loongarch64-qemu-virt qemu_la64_mmio_regions_include_ls7a_rtc -- --nocapture`
  passed.
- `cargo test -p tx-hal-loongarch64-qemu-virt la64_platform_overrides_rtc_irq_constant -- --nocapture`
  passed.
- `cargo test -p tx-hal-loongarch64-qemu-virt irq -- --nocapture` passed,
  proving the new RTC IRQ constant and LS7A alarm paths still compose with the
  host IRQ controller tests.
- `cargo check -p tx-kernel-loongarch64-qemu-virt -q` passed with existing
  unrelated warnings.
- `cargo check -p tx-kernel -q` passed with existing unrelated warnings.
- Final slice validation passed: `cargo fmt --check`, scoped
  `git diff --check`, `cargo xtask progress validate`, and
  `cargo xtask lint docs` passed. Docs lint reported the expected
  retired-term warning class and ended with `docs lint: ok`.
- Old-interface/stub audit passed with no active Rust hits:
  `rg -n '\bTimeIf\b|TimerQueue|DeadlineFuture|timer_sleep|install_timer_queue|sleep_until_ns|DirectMailboxTimerWakeRouter|\.fire_due\(|timer_queue|fixed_oscomp_time|binding\.name == "rtc"' crates boards --glob '*.rs'`.

Existing unrelated warnings remain: unused `guard` in
`tx-subsystems/src/net/execution/step_connect.rs`, unused `alloc::vec::Vec` in
`tx-fs/src/tx_ext4_bridge.rs`, and dead
`unregister_thread_reactor_task` in `tx-kernel`.

## Next Step

Finish Package F by adding real-board RTC or firmware backends, or by recording
explicit unsupported hardware witnesses for boards without reliable RTC
hardware. Then continue Package G wake-class convergence through the
owner-aware router.

## Blockers

No blocker for this foundation slice. Full time/wake refactor remains
incomplete until real-board RTC/firmware witnesses and Package G are finished.
