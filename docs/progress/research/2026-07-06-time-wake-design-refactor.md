# Time/Wake Design Refactor

Date: 2026-07-06

## Summary

Refactored [`docs/design/02_execution/TIME_WAKE_v1.md`](../../design/02_execution/TIME_WAKE_v1.md)
from a linear design sketch into an implementation-oriented architecture
document. The new structure separates ownership, canonical paths, and migration
packages so future implementation can proceed by clear layers.

Update: after the scheduler-aware timer expiry slice and direct mailbox router
retirement landed, the design document was aligned with the current code state:
`TimeIf`, `DirectMailboxTimerWakeRouter`, and `TimerWheel::fire_due` are now
recorded as retired active Rust interfaces, while `TimerQueue`,
`DeadlineFuture`, and `timer_sleep` remain the active legacy migration scope.

## What Changed

- 2026-07-09 non-network retired-interface audit: after the scope was narrowed
  to exclude the network stack, rechecked the time/wake retired-interface
  surface outside `crates/tx-subsystems/src/net/**` and syscall socket paths.
  `cargo xtask lint invariants time-wake-retired` passed with zero retired
  active Rust sites. A focused non-network grep for the retired producer names
  over `crates` and `boards` reported only `step_ingest` documentation/test
  text and the active module re-export of `step_ingest_with_post`; it did not
  find a callable non-network old wrapper. No network-stack implementation
  files were changed for this audit. Remaining blockers are the explicitly
  deferred network lane and the Package H external real-board or
  firmware-backed RTC witness.
- 2026-07-09 implementation-level design detail: expanded the formal Chinese
  design entry
  [`TX_TIME_WAKE_DESIGN_REVIEW_CN.md`](../../stage2-documents/time_infra/TX_TIME_WAKE_DESIGN_REVIEW_CN.md)
  with implementation-level module details. The new sections describe HAL
  three-capability logic, Timekeeper read/write and vvar paths, Timer Registry
  install/fire/cancel semantics, ActiveWait/StepOp timeout handling,
  WaitSource/TaskMailbox producer publication, owner-aware wake placement,
  reactor timer driver hardware-deadline programming, and RTC typed device
  routing. Each module now has a sub-architecture diagram, upper/lower
  interface notes, adjacent-module touchpoints, and a minimum implementation
  contract. This closes the documentation-detail gap only; producer
  implementation audit and Package H external RTC witness remain open.
- 2026-07-09 net device TX post injection: retired the old direct net device
  TX pending wrappers from active Rust. The active surface is now
  `step_process_device_tx_pending_with_post`,
  `step_process_device_tx_pending_at_with_post`, and
  `step_process_device_tx_pending_in_namespace_at_with_post`; UDP and raw-ICMP
  send-space readiness publication routes through the caller-provided post.
  `drive_net_namespace_runtime_at_with_post` and the net delegate runtime pass
  their existing owner-aware mailbox-ref post hooks into device TX processing,
  while no-context tests pass explicit direct closures. The `time-wake-retired`
  gate now rejects the retired device TX names. Verification: active-Rust
  exact-name grep over `crates/tx-subsystems/src`, `crates/tx-shims/src`,
  `crates/tx-kernel/src`, and `boards` returned no hits; touched-file rustfmt
  check passed; subsystem/kernel check passed with existing unrelated warnings;
  focused netdevice injected-post and busy-retry tests passed; veth and bridge
  UDP device-TX tests passed under their exact filters; the xtask linter
  self-test passed; and the retired-interface gate stayed at zero sites. A
  mistaken earlier veth filter ran zero tests and was not counted as evidence.
  Broader Package G producer convergence and Package H external real-board or
  firmware-backed RTC witness remain open.
- 2026-07-09 UDP/ICMP loopback post injection: retired the old direct
  UDP/ICMP loopback processing and inline UDP loopback send wrappers from
  active Rust. The active surface is now
  `step_process_loopback_udp_with_post`,
  `step_process_loopback_udp_on_iface_with_post`,
  `step_send_udp_loopback_kernel_bytes_with_post`,
  `step_send_udp_loopback_kernel_bytes_on_iface_with_post`,
  `step_process_loopback_icmp_with_post`, and
  `step_process_loopback_icmp_on_iface_with_post`. The loopback pending driver
  now threads its caller-provided post through TCP, UDP, and ICMP lower
  processing, while syscall UDP loopback send and post-send loopback driving
  inject `SyscallCtx::post_mailbox_ref_event`. The `time-wake-retired` gate now
  rejects the retired UDP/ICMP loopback direct wrapper names. Verification:
  active-Rust exact-name grep over `crates/tx-subsystems/src`,
  `crates/tx-shims/src`, `crates/tx-kernel/src`, and `boards` returned no hits;
  touched-file rustfmt check passed; subsystem/shims check passed with existing
  unrelated warnings; focused UDP loopback injected-post and direct-send tests
  passed; loopback pending UDP and raw ICMP tests passed; shims UDP
  sendmsg/recvmsg, netperf-style UDP RR, and raw ICMP ping socket witnesses
  passed; the xtask linter self-test passed; and the retired-interface gate
  stayed at zero sites. Broader Package G producer convergence and Package H
  external real-board or firmware-backed RTC witness remain open.
- 2026-07-09 netlink send post injection: retired the old direct
  route/xfrm/netfilter netlink send wrappers from active Rust. The active
  surface is now `netlink_route_send_with_post`,
  `netlink_route_send_with_netns_resolver_and_post`,
  `netlink_route_send_with_netns_resolvers_and_post`,
  `netlink_xfrm_send_with_post`, and `netlink_netfilter_send_with_post`;
  syscall dispatch injects `SyscallCtx::post_mailbox_ref_event` so queued
  netlink response readability uses the same mailbox-ref owner-aware seam as
  other socket readiness producers. The `time-wake-retired` gate now rejects
  the retired direct netlink send names. Verification: subsystem check passed
  with existing unrelated warnings, the new rtnetlink injected-post witness
  passed, the full rtnetlink lib filter passed 24/24, the focused shims
  route-send test passed, the full `netlink_packet` shims filter passed 23/23,
  the xtask linter self-test passed, and the retired-interface gate stayed at
  zero sites. Broader Package G producer convergence and Package H external
  real-board or firmware-backed RTC witness remain open.
- 2026-07-09 complete design document formalization: promoted
  [`TX_TIME_WAKE_DESIGN_REVIEW_CN.md`](../../stage2-documents/time_infra/TX_TIME_WAKE_DESIGN_REVIEW_CN.md)
  into the formal complete Chinese time/wake design document. The document now
  makes the architecture-complete vs implementation-complete distinction
  explicit; adds the owner/interface/proof contract; defines implementation
  slices, PR split order, old-interface retirement maintenance, and the final
  delivery checklist; and keeps Package H real-board or firmware-backed RTC
  witness as an open implementation evidence row. The time-infra README now
  names this document as the preferred implementation, review, regression, and
  handoff entry. Verification for this slice is docs/progress validation and
  docs lint only; no active Rust code changed.
- 2026-07-09 net device IRQ post injection: retired the net device / virtio
  IRQ direct delegate-kick wrappers from active Rust. Staged
  `VirtioNetDevice` now exposes `handle_irq_with_post`,
  `inject_rx_and_fire_poll_with_post_for_test_or_irq`, and
  `complete_tx_and_fire_poll_with_post_for_test_or_irq`; raw virtio drivers and
  `NetDeviceOps` now expose `poll_device_and_fire_with_post` /
  `ack_interrupt_and_fire_with_post`. Tests pass explicit direct closures, and
  `time-wake-retired` now rejects the old `handle_irq`,
  `poll_device_and_fire`, `ack_interrupt_and_fire`,
  `inject_rx_and_fire_poll_for_test_or_irq`, and
  `complete_tx_and_fire_poll_for_test_or_irq` names in active Rust. Verification:
  the `virtio_net_device_tests` lib filter passed 7/7, the thread-future fatal
  signal injected-post test passed after removing a stale old-name test import,
  the tx-subsystems/tx-drivers/tx-kernel check passed with existing unrelated
  warnings, the xtask linter self-test passed, and the retired-interface gate
  stayed at zero sites. The attempted tx-kernel boot-net test filter ran zero
  tests in the current harness, so it was not counted as evidence. The broader
  Package G audit and Package H external real-board or firmware-backed RTC
  witness remain open.
- 2026-07-09 net packet event post injection: moved the net delegate runtime's
  TCP/UDP/ICMP packet-dispatch readiness path from the no-context direct
  mailbox fallback onto the driver-injected mailbox-ref seam.
  `step_process_network_events_in_namespace_at_with_post` now carries the
  caller post closure through `NetworkPublish::publish_to_with_post`, and
  `drive_all_net_namespace_runtimes_at_with_post` gives namespace runtime
  packet processing the same route. `net_delegate_step_once` injects
  `NetDelegateDriver::post_net_mailbox_ref_event` for both direct packet-source
  processing and per-namespace device packet processing, while no-context
  wrappers keep explicit direct closures. Verification: the focused packet
  event injected-post test passed, the loopback-progress delegate test passed,
  the single-thread `net_delegate` lib filter passed 20/20, `cargo check -p
  tx-subsystems -p tx-kernel -q` passed with existing unrelated warnings, and
  `cargo xtask lint invariants time-wake-retired` stayed at zero retired sites.
  The broader Package G audit and Package H external real-board or
  firmware-backed RTC witness remain open.
- 2026-07-09 net delegate runtime post injection: moved the net delegate
  runtime loopback TCP readiness slice from lower direct mailbox posting to the
  driver-injected mailbox-ref seam. `NetDelegateDriver` now has a narrow post
  hook, the production `BootNetDelegateDriver<P>` injects
  `post_mailbox_ref_event_with_hint_from_current_hart`, and
  `step_process_loopback_pending_in_namespace_with_post` /
  `step_process_loopback_tcp_with_post` carry that hook through lower loopback
  publish targets. Tests and other no-context callers retain the default
  direct fallback explicitly through the trait. Verification: the focused
  `net_delegate_step_once_rekicks_after_loopback_progress` lib test passed,
  the single-thread `net_delegate` lib filter passed 20/20, `cargo check -p
  tx-subsystems -p tx-kernel -q` passed with existing unrelated warnings, and
  `cargo xtask lint invariants time-wake-retired` stayed at zero retired
  sites. The parallel `net_delegate` filter remains poor evidence because its
  shared global delegate queue and epoch lock can cause test interference.
- 2026-07-09 review design completeness pass: expanded the standalone Chinese
  review document
  [`TX_TIME_WAKE_DESIGN_REVIEW_CN.md`](../../stage2-documents/time_infra/TX_TIME_WAKE_DESIGN_REVIEW_CN.md)
  from a concise review entry into a full implementation-review design
  document. The new material adds Linux/POSIX requirement traceability, key
  timer/wait/task/RTC state machines, the target HAL/timekeeper/timer/RTC/
  owner-wake interface blueprint, a producer migration catalog, an end-to-end
  test matrix, and mandatory document/lint/progress synchronization rules.
  This closes the reader-facing design-document shape; it does not close
  implementation evidence, per-producer proof, or the Package H external
  real-board/firmware-backed RTC witness.
- 2026-07-09 standalone complete design review doc: added
  [`TX_TIME_WAKE_DESIGN_REVIEW_CN.md`](../../stage2-documents/time_infra/TX_TIME_WAKE_DESIGN_REVIEW_CN.md)
  as the preferred Chinese review entry for the time/wake architecture. The
  new document extracts the cumulative design into a single formal contract:
  problem statement, goals and non-goals, global architecture, Linux mapping,
  HAL/timekeeper/timer/wait-source/RTC module contracts, end-to-end control
  flows, SMP/future-stealing wake rules, data ownership, error semantics,
  implementation packages, acceptance gates, and the reusable VFS-to-HAL
  layering template. The stage2 README and cumulative Chinese handoff now
  point to this standalone review document. This closes the reader-facing
  document shape only; implementation completion still requires current
  per-producer proof and the Package H external real-board or firmware-backed
  RTC witness.
- 2026-07-09 complete design document closure: added a normative-anchor and
  synchronization pass to the Chinese time/wake design handoff. The front
  matter now maps each review concern to its active txdoc anchor, the relevant
  explanatory sections, and the mechanical proof gate: owner layering,
  Step/yield/timeout semantics, static HAL boundaries, timekeeper, timer
  registry, wait-source publication, SMP owner-aware wake, RTC/devfs routing,
  and old-interface retirement. It also records the mandatory sync order for
  future changes: update `TIME_WAKE_v1.md`, the Chinese design, the English
  handoff when applicable, the `time-wake-retired` lint, and progress evidence.
  This closes the document as a usable design-review entry point, but does not
  close implementation evidence rows such as per-producer proof or the external
  real-board/firmware RTC witness.
- 2026-07-09 process group-exit design sync: aligned the Chinese and English
  stage2 design handoffs with the current group-exit caller-posting surface.
  Process group-exit now appears in the producer catalog as a dual-post path:
  `step_exit_group_with_posts` / `step_exit_group_with_signal_with_posts`
  inject both the signal-task mailbox post and the exit-source mailbox-ref
  post. The no-context fallback is explicit direct closures for both post
  roles, and the complete-design grep tripwire now rejects the retired
  `step_exit_group` and `step_exit_group_with_signal` names with the rest of
  the signal/signalfd/process-wait retired interface group.
- 2026-07-09 process-group signal direct wrapper retirement: retired the bare
  `step_kill_pgrp` helper and old `KillPgrpOp` StepOp from active Rust. The
  process-group signal surface is now `step_kill_pgrp_with_post` for narrow
  weak-mailbox posting and `step_kill_pgrp_with_posts` for the full dual-post
  path that carries both signal-task mailbox publication and
  signalfd/readiness mailbox-ref publication. `KillPgrpWithPostOp` replaces
  the old direct StepOp wrapper. Session-leader hangup cascades and syscall
  `kill(0, sig)` now inject the dual-post route, and the retired-interface
  gate rejects `step_kill_pgrp` and `KillPgrpOp` as active-Rust residue.
- 2026-07-08 documentation closeout: completed the Chinese time/wake design
  handoff as a design-complete artifact without changing active Rust code.
  `TX_TIME_WAKE_DESIGN_CN.md` now has a final interface blueprint for the HAL
  time traits, `TimekeeperIf`, `TimerRegistrar` / `TimerRegistry`,
  `TimerWakeRouter`, wait-source caller-posting, `RtcDeviceOps`,
  `CharDeviceOps`, `SyscallCtx`, and worker injected-post seams. It also
  abstracts the reusable VFS-to-HAL layering pattern that should guide later
  device/VFS/HAL refactors: hardware capability -> typed subsystem ops ->
  semantic state -> devfs/RNode projection -> wait-source publication ->
  owner-aware scheduler placement. The new final review section states that
  the document is architecture-complete, while implementation completion still
  depends on the retired-interface gate, focused producer tests, QEMU or board
  witnesses, and progress closeout evidence.
- Retired the timerfd direct no-context wrappers
  `timerfd_settime_with_flags` and `timerfd_clock_was_set` from active Rust.
  The production syscall and wall-clock mutation paths already used
  `timerfd_settime_with_flags_and_post` and
  `timerfd_clock_was_set_with_post` with `SyscallCtx` mailbox-ref posting; the
  remaining subsystem tests now pass explicit direct closures to the
  `_with_post` helpers. The `time-wake-retired` lint and the manual
  design-document grep tripwires now reject the old timerfd names in active
  `crates` / `boards` Rust.
- Retired the socket readiness public direct fallback helper
  `direct_mailbox_post` from active Rust. The helper definition, network
  structure re-export, and residual imports in net execution, netlink, and
  tests were removed after the call sites had already been moved to explicit
  direct closures through `_with_post` seams. The `time-wake-retired` lint now
  treats `direct_mailbox_post` as a retired socket/network readiness name in
  both the scoped callable scan and the full active-Rust old-name residue scan.
  Verification: strict grep over `crates` and `boards` found zero hits,
  `cargo check -p tx-subsystems -q`, `cargo test -p xtask
  lint_invariants_time_wake -- --nocapture`, `cargo xtask lint invariants
  time-wake-retired`, and `cargo test -p tx-subsystems net -- --nocapture
  --test-threads=1` passed. A parallel `cargo test -p tx-subsystems net --
  --nocapture` run is not good evidence for this checkout because the net
  tests share an epoch lock; it failed with one `EADDRINUSE` followed by
  `PoisonError` cascades, while the single-thread rerun passed all 253
  net-filtered lib tests.
- Added the async-runtime/reference closure section to the Chinese complete
  design document. It explains how Linux-style hrtimer/wakeup paths,
  Tokio-like multi-thread executors, dispatcher-based async loops, and
  embedded executors all separate time/readiness production from scheduler
  placement. The section maps that shared split onto Tx's `TimerRegistrar`,
  `TaskMailbox`, `WaitSource`, `ReactorOwnerWakePost`, and scheduler
  current-owner re-resolution; records the post-steal wake linearization
  point; rejects stale-hart timer entries, future-owned timer wheels, and
  per-subsystem scheduler shortcuts; and gives future producer authors a
  checklist for proving timer and stealing safety.
- Retired the old public delegate-timeout manual driver
  `TimerWheel::fire_due_delegate_timeouts` from active Rust code.
  The substrate and reactor delegate-timeout pin tests now drive expiry through
  `TimerRegistry::fire_due_with` and a `TimerWakeRouter`, so the same
  registry/router shape is used for no-reactor tests and the production
  reactor path. `cargo xtask lint invariants time-wake-retired` now includes
  the old helper name in the active-Rust retired-name matrix.
- Restored the default RV64 QEMU smoke sentinel without reintroducing a
  kernel-owned `/init` fixture. `cargo xtask test smoke` now builds the
  minimal test-init initramfs, and QEMU smoke attaches it with explicit
  `init=/tx-test-init tx.test_init=1`. This keeps smoke setup in userspace and
  lets the Linux-like boot path skip rootfs shims while still providing a real
  first userspace image for bootstrap exec. Current RV64 QEMU evidence now has
  both smoke and busybox sentinels passing with SMP/AP reactor markers.
- Closed the RV64 QEMU busybox boot blocker that had been preventing current
  QEMU SMP evidence from reaching the boot sentinel. Investigation showed that
  `tx.boot.mode=busybox` still selected `LegacyKernelShims`, which created
  `/bin/busybox -> /musl/musl/busybox` before initramfs unpack. The busybox
  initramfs contains its own regular `./bin/busybox`, so unpack preserved the
  stale symlink and bootstrap exec followed the missing `/musl/musl/busybox`
  path. `BootMode::Busybox` now skips kernel rootfs shims like other
  Linux-like image-owned boot modes, while OSComp/LTP/Test remain on the
  legacy shim path. The decision note
  [`2026-07-06-boot-mode-shim-split.md`](../decisions/2026-07-06-boot-mode-shim-split.md)
  was synchronized with this boundary.
- Completed the Chinese complete-detail pass:
  [`TX_TIME_WAKE_DESIGN_CN.md`](../../stage2-documents/time_infra/TX_TIME_WAKE_DESIGN_CN.md)
  now includes board modeling profiles over `MonotonicCounterIf`,
  `DeadlineTimerIf`, and `PersistentClockIf`; timekeeper boot seed and runtime
  mutation flows; reactor-facing timer driver interfaces; timer/wait-source
  convergence; expanded data-structure ownership; Package A-H exit criteria;
  and an appendix that maps live interfaces to code homes, feature lookup
  paths, and retired-interface regression commands. This raises the Chinese
  document to the full implementation-handoff level while preserving
  [`TIME_WAKE_v1.md`](../../design/02_execution/TIME_WAKE_v1.md) as the
  txdoc-tagged normative contract.
- Completed the Chinese stage2 design-document closure:
  [`TX_TIME_WAKE_DESIGN_CN.md`](../../stage2-documents/time_infra/TX_TIME_WAKE_DESIGN_CN.md)
  now has the same implementation-review closure shape as the English stage2
  design. It adds a stable complete contents table, maintainer handoff section,
  remaining Package G producer detailed design, completeness-boundary table,
  and closed v1 design decisions, while keeping
  [`TIME_WAKE_v1.md`](../../design/02_execution/TIME_WAKE_v1.md) as the
  normative txdoc-tagged contract. This closes the reader-facing Chinese design
  gap only; real-board/firmware RTC witness evidence and QEMU/real-board SMP
  stress remain open implementation evidence rows.
- Added a broader host-level SMP/mixed-producer wake witness:
  `mixed_producer_wakes_repeatedly_route_current_owner` in
  `crates/tx-reactor/tests/reactor_smoke.rs` parks one task and wakes it
  sequentially through wait-source publication, timer expiry, and delegate
  reply from a non-owner hart. Each producer must route through owner-aware
  placement, emit the remote reschedule signal to the current owner hart, and
  let the task re-poll before the next producer fires. This strengthens the
  Package G completion evidence but does not replace the remaining real-board
  RTC and broader QEMU/board SMP stress rows.
- Added the mechanical retired-interface regression gate:
  `cargo xtask lint invariants time-wake-retired` now encodes the
  time/wake active-Rust retirement matrix from the design documents. It
  scope-checks old broad time/timer names, direct wake producer wrappers,
  raw RTC queue access, generic v3 wait-source direct adapters, and concrete
  TimerWheel adapter imports while preserving the intended `_with_post`
  replacement names. This turns the former manual grep checklist into an
  invariant gate that can be rerun before declaring a slice complete.
- Added the Chinese complete-design entry:
  [`TX_TIME_WAKE_DESIGN_CN.md`](../../stage2-documents/time_infra/TX_TIME_WAKE_DESIGN_CN.md)
  now provides the reader-facing Chinese design document for Tx time, RTC,
  timer registry, wait-source publication, owner-aware wake routing, SMP
  future stealing, producer migration, and acceptance gates. It does not
  replace `TIME_WAKE_v1.md`; it is the implementation-review and handoff
  companion for Chinese readers.
- Corrected the global architecture graph so the reactor timer driver, not
  `TimerWheel`, programs `DeadlineTimerIf`.
- Added an ownership matrix covering HAL counter, HAL deadline, persistent
  clock, timekeeper, timer registrar, reactor driver, wake router, scheduler,
  and RTC device facade.
- Added canonical sequence diagrams for reading time, registering a timeout,
  firing a timeout, and exposing RTC through devfs.
- Reworked the migration plan into six packages:
  HAL capability split, timekeeper facade, timer registry facades,
  ActiveWait/WakeRouter, `TimerQueue` retirement, and RTC device route.
- Added per-package exit evidence to make the design usable as an
  implementation plan seed.
- Expanded the document into the final target-state design: hardware capability
  sub-architecture, timekeeper state model, timer registry internals,
  ActiveWait driver logic, reactor timer loop, WakeRouter SMP routing,
  RTC/devfs semantics, invariants, and implementation-readiness checks.
- Completed the design reference with Linux function-group mapping, fixed
  dependency-direction rules, explicit clock-class semantics, timer
  fire/cancel race rules, authoritative mailbox owner binding, cross-hart
  timer registration and hardware reprogramming policy, and observation points.
- Corrected the current-implementation status after Package A/C progress:
  `TimeIf` is already retired from active Rust code, while
  `TimerQueue` / `DeadlineFuture` / `timer_sleep` remain active legacy
  retirement scope.
- Corrected the current-code alignment after the later timer wake slice:
  timer expiry now uses a reactor-owned scheduler-aware `TimerWakeRouter`, the
  direct mailbox compatibility route is gone, and Package D is marked as
  partially landed rather than fully complete.
- Aligned active HAL/device/observation docs with the target split by replacing
  `TimeIf` as a live interface with `MonotonicCounterIf` and
  `DeadlineTimerIf`.
- Added the final design-detail pass: target module map, hardware capability
  contracts, boot RTC seed and realtime mutation flow, timer guard lifecycle,
  reactor idle contract, wake-class convergence table, RTC ABI layering and
  current stub retirement path, Linux compatibility boundary, and package
  dependency graph.
- Grounded the RTC section in current code reality: `/dev/misc/rtc` is a
  static devfs char-device projection whose typed `RtcDeviceOps` are backed by
  the installed `PersistentClockIf` callback; RTC ioctl decoding remains in
  the shim, but `RTC_RD_TIME`, `RTC_SET_TIME`, `RTC_ALM_READ`, and
  `RTC_ALM_SET` semantics now route through typed device ops rather than a
  syscall-local fixed-time special case.
- Completed the missing design-document closure pass: added the explicit
  interface catalog, end-to-end scenario map, time/wake error model, SMP race
  matrix, and the previously missing Package G migration section for
  wake-class convergence.
- Updated the Package G scope so timer, wait-source, delegate, signal, and
  device-readiness wakes all converge through the owner-aware router before
  scheduler placement; the document now separates target architecture from the
  still-incomplete implementation status.
- Completed the full design-contract pass after RTC alarm typed routing:
  added board backend modeling for counter/deadline/persistent-clock roles,
  explicit persistent writeback policy, the shared owner-aware wake-post
  primitive, RTC event wait-source semantics, an RTC UAPI completion matrix,
  tightened Package F/G exit evidence, and replaced Open Questions with closed
  v1 decisions plus deferred v2 work.
- Added the final document-navigation pass: a top-level Document Map now tells
  readers how to move from architecture contract, to module contracts, to
  migration/proof evidence without treating time/wake as a single monolithic
  subsystem.
- Added the implementation blueprint closeout: global-to-local traceability,
  data ownership, required implementation order, strict old-interface
  retirement gate, and per-module acceptance tests now appear in one section
  that ties the global architecture to the package migration plan.
- Added the Package G wake-producer migration catalog to the stage2 design:
  timer, delegate, signal, itimer, process exit-source, futex, eventfd, pipe,
  VFS/RNode, TTY, socket/network, RTC, async fd/IPC, and reactor-local
  coordination producers are now classified by semantic owner, wake identity,
  scheduler-context seam, no-context fallback, migration priority, and
  acceptance proof. This closes the reader-facing design gap between the
  global owner-aware wake architecture and per-producer implementation slices.
- Finalized the stage2 complete design document pass: added the design thesis
  separating clock meaning, deadline mechanics, and runnable placement; added
  board modeling profiles for SiFive/RISC-V-like, LoongArch/2K-like, QEMU, and
  no-RTC platforms over the three hardware capability traits; added an
  interface contract table naming what each layer may and must not mutate; and
  extended the implementation plan with Package G owner-aware wake producer
  convergence and Package H board/Linux-parity evidence exits.
- Added the complete-design handoff entry to
  [`TX_TIME_WAKE_DESIGN.md`](../../stage2-documents/time_infra/TX_TIME_WAKE_DESIGN.md):
  the document now states up front that "complete" means each known time, RTC,
  timer, wait-source, device-readiness, and SMP wake feature has a named owner,
  legal lower and upper interfaces, a migration package, and acceptance proof.
  It also separates architecture complete, slice complete, and implementation
  complete so remaining implementation rows can be closed without reopening the
  broad `TimeIf`, private timer queue, HAL-to-RNode, or per-subsystem scheduler
  shortcut shapes.
- Advanced the Package G AIO/io_uring completion readiness slice:
  `AioContext::push_completion_with_post` and `IoUring::push_cqe_with_post`
  are now the only active completion/CQ publication verbs; AIO and SQPOLL
  worker setup receive narrow completion-post closures through
  `spawn_worker_for_context_with_completion_post` and
  `spawn_sqpoll_worker_with_completion_post`; `sys_io_setup`,
  `sys_io_uring_setup`, and `sys_io_uring_enter` inject the relevant
  `SyscallCtx` mailbox-ref post route; and the old direct
  `push_completion`, `push_cqe`, direct worker-spawn wrappers, direct
  completion-notify names, and direct-post helper names are absent from active
  Rust code. Focused AIO/io_uring tests, `cargo check -p tx-subsystems -q &&
  cargo check -p tx-shims -q`, `cargo fmt --check -p tx-subsystems -p
  tx-shims`, and the strict retired-interface grep audit passed with only the
  existing unrelated warnings.
- Retired the remaining direct no-context wrapper names from the already
  migrated signalfd and process exit-source readiness paths:
  `SignalFd::notify`, `notify_process_signal`, process
  `fire_exit_source`, and process-local `notify_v3_source` are absent from
  active Rust code. No-context tests now pass explicit direct mailbox-ref post
  closures through `SignalFd::notify_with_post`,
  `notify_process_signal_with_post`, and `fire_exit_source_with_post`.
  Verification: strict signalfd/process retired-interface grep audit passed;
  signalfd unit tests, `v3_signal_mailbox`, process exit-source unit tests,
  `v3_exit_wait_source`, `v3_signalfd`, and `cargo check -p tx-subsystems -q`
  passed with only the existing unrelated `step_connect.rs` warning.
- Aligned the design document with the first Package G owner-aware wake-post
  implementation slice: `ReactorOwnerWakePost` is now named as the concrete
  reactor helper, timer expiry and wake-inbox drain are recorded as using the
  shared route, and `Reactor::post_mailbox_event_from_hart` is documented as
  the approved entry for non-timer producers that already have reactor,
  current-hart, and reschedule-signal context. The document still marks
  Package G incomplete until delegate, signal, device readiness, and remaining
  wait-source producers converge where scheduler context exists.
- Removed stale implementation-status residue from the design text: RTC
  `read(2)` is now described as the implemented pending-event consume/blocking
  wait path rather than a future EOF-placeholder route, and persistent
  writeback is recorded as wired through `clock_settime(CLOCK_REALTIME)` and
  `settimeofday`.
- Completed the stage2 Linux reference navigation layer in
  [`docs/stage2-documents/time_infra/README.md`](../../stage2-documents/time_infra/README.md):
  added a global Linux module/interface matrix and end-to-end operation paths
  so the reference now connects each detailed module section back to the top
  architecture without mentioning the local target design.
- Completed the hardware RTC backend design closure in
  [`docs/design/02_execution/TIME_WAKE_v1.md`](../../design/02_execution/TIME_WAKE_v1.md):
  added board RTC backend profiles for RV64 QEMU goldfish RTC, LA64/LS7A-like
  RTCs, and real-board unsupported/fallback cases, plus the explicit hardware
  RTC IRQ publication boundary. The boundary keeps HAL on IRQ dispatch and
  register access, keeps board RTC register/ack logic in the board backend,
  keeps `tx-kernel` responsible for IRQ handler installation, and publishes
  alarm/update events into RTC device state before normal wait-source and
  owner-aware wake routing.
- Implemented the first board backend profile: `boards/tx-hal-riscv64-qemu-virt`
  now maps the QEMU `google,goldfish-rtc` MMIO block, implements
  `PersistentClockIf` with goldfish read/set/alarm/clear helpers, and has
  focused host tests for MMIO exposure, 64-bit split ordering, alarm
  programming, and PLIC IRQ 11 mask/unmask behavior.
- Implemented the first hardware RTC IRQ publication path: optional
  `IrqIf::RTC_IRQ`, RV64 QEMU PLIC IRQ 11, `PersistentClockIf` hardware IRQ
  acknowledgement, and `tx-kernel::rtc_alarm_irq_handler` now connect a
  hardware alarm interrupt into the existing RTC pending-event/read/poll state
  without HAL touching devfs/RNode state directly.
- Implemented the second QEMU board RTC backend:
  `boards/tx-hal-loongarch64-qemu-virt` now publishes the LS7A RTC MMIO
  region, exposes RTC GSI 67 through `IrqIf::RTC_IRQ`, converts LS7A TOY
  calendar registers to/from Unix nanoseconds for `PersistentClockIf`, and
  programs `TOYMATCH0` for wake alarms while keeping RTC event publication in
  the generic kernel/devfs path.
- Completed the future-stealing design closure in
  [`docs/design/02_execution/TIME_WAKE_v1.md`](../../design/02_execution/TIME_WAKE_v1.md):
  the document now explicitly separates future continuation state, stable
  `TaskMailbox` wake identity, and scheduler `current_hart` ownership. Timer,
  wait-source, delegate, signal, and device wake producers must keep mailbox
  identities rather than hart identities, and every post-steal wake must
  re-resolve the current scheduler owner before placement.
- Strengthened the stage2 handoff into a formal design specification:
  [`TX_TIME_WAKE_DESIGN.md`](../../stage2-documents/time_infra/TX_TIME_WAKE_DESIGN.md)
  now opens with a review-oriented specification table, the authoritative
  contract/reference mapping, the feature-family owner rows, and the hard
  retirement posture. This makes the document directly usable for future
  implementation review: every new time/wake feature must fit a named owner
  row or add a justified extension slot before code is written.
  re-resolve current owner through `OwnerAwareWakePost` / scheduler
  lock-and-recheck instead of treating a captured local `Waker` as placement
  authority.
- Advanced Package G delegate-timeout convergence: `TimerWakeRouter` now has a
  delegate-timeout callback, `TimerRegistry::fire_due_with` routes tagged
  `DelegateTimeout` entries through that callback, and `ReactorOwnerWakePost`
  marks the delegate token timed out with a caller-supplied post operation so
  the resulting `Abort(TimedOut)` mailbox event uses the same owner-aware
  scheduler placement path as ordinary timer expiry.
- Synchronized the complete design documents with the 2026-07-07
  thread-future fatal-signal slice: `TIME_WAKE_v1.md` and
  `TX_TIME_WAKE_DESIGN.md` now record that signal-frame reserve/copy failures,
  `prepare_signal_frame` failures, and bad or failed sigreturn restoration use
  `fatal_signal_teardown_from_current_hart` plus
  `post_mailbox_event_from_current_hart` to reach
  `Reactor::post_mailbox_event_from_hart` when reactor context exists. Package
  G remains explicitly incomplete for remaining higher-level signal/syscall
  callers and ordinary wait-source producers.
- Advanced the next Package G signal slice for syscall-context producers:
  `SyscallCtx` now carries an optional mailbox-post function, the real
  `thread_future` syscall path injects
  `post_mailbox_event_from_current_hart`, and `SyscallCtx::post_mailbox_event`
  falls back to direct posting only for host/no-reactor contexts.
  `script_deliver_signal_with_post` and `ThreadKillWithPostOp` let
  `pidfd_send_signal`, `tkill`, and `tgkill` reuse their existing signal
  routing/StepOp structure while publishing through the injected post. The
  socket, pipe, and page-backed `SIGPIPE` paths now call
  `step_kill_process_with_post` via the same `SyscallCtx` seam. The legacy
  itimer compatibility frame path in `time.rs` remains a no-context direct
  producer because it receives a `UserTrapContext`, not a `SyscallCtx` or
  reactor handle.
- Advanced the active `ITIMER_REAL` producer slice: syscall-boundary polling
  and socket wait expiry now call `fire_itimer_real_with_post(ctx)`, which
  delivers handler-gated `SIGALRM` through `deliver_signal_if_handler_with_post`
  and `SyscallCtx::post_mailbox_event`; the enter-userspace compatibility
  frame path is now `maybe_deliver_itimer_signal_with_post(..., post)` and the
  kernel injects `post_mailbox_event_from_current_hart`. The old direct
  `maybe_deliver_itimer_signal`, `fire_itimer_real`, and
  `deliver_signal_if_handler` active wrappers are retired.
- Advanced the first ordinary syscall-context wait-source producer:
  `SyscallCtx` now carries an optional mailbox-ref post function for producers
  that already upgraded a subscriber mailbox; `thread_future` injects
  `post_mailbox_ref_event_from_current_hart`; process `exit_source` gained
  `fire_exit_source_with_post` / `notify_child_zombified_with_post`; and
  `sys_exit_group` routes the parent `exit_source` wake through
  `SyscallCtx::post_mailbox_ref_event` while preserving the default direct
  fallback for no-reactor contexts.
- Completed the stage2 design document's implementation-entry pass:
  [`TX_TIME_WAKE_DESIGN.md`](../../stage2-documents/time_infra/TX_TIME_WAKE_DESIGN.md)
  now begins with a document map that routes readers from architecture, to
  module contracts, to implementation packages; Package G now has a mechanical
  producer-slice recipe for `_with_post`/caller-posting migration without a
  `tx-subsystems -> tx-reactor` dependency; and the document now includes
  end-to-end acceptance scenarios for clock reads, realtime mutation, sleep,
  futex/poll timeout, timerfd, signalfd, RTC, VFS timestamps, and post-steal
  wake routing.
- Added the maintainer-handoff closure to
  [`TX_TIME_WAKE_DESIGN.md`](../../stage2-documents/time_infra/TX_TIME_WAKE_DESIGN.md):
  the document now names the authoritative document stack, next slice
  selection rules for socket/network, RTC/device, AIO/io_uring, POSIX mq/SysV
  IPC, and higher-level signal/syscall producers, the standard Package G patch
  shape, scoped verification ladder, and the distinction between architecture
  completeness and implementation completeness.
- Advanced the POSIX mq Package G slice: `notify_message_available_with_post`
  and `notify_space_available_with_post` are now the only POSIX mq
  notification verbs, `step_mq_send_with_post` and
  `step_mq_receive_with_post` keep queue state in the POSIX mq/SysV message
  payload while accepting caller-injected mailbox-ref posting, and the syscall
  `mq_timedsend` / `mq_timedreceive` path injects
  `SyscallCtx::post_mailbox_ref_event`. Remaining IPC Package G work is now
  AIO/io_uring plus SysV msg/sem-style producers rather than POSIX mq
  send/receive readiness.
- Advanced the SysV msg Package G slice: `notify_message_available_with_post`,
  `notify_space_available_with_post`, and `abort_removed_with_post` are now
  the only SysV msg notification verbs; `step_msgsnd_with_post`,
  `step_msgrcv_with_post`, and `step_msgctl_in_ns_with_post` keep message
  queue truth in `MsgQueuePayload` while accepting caller-injected
  mailbox-ref posting; and the syscall `msgsnd` / `msgrcv` / `msgctl` path
  injects `SyscallCtx::post_mailbox_ref_event`.
- Retired the SysV sem no-context direct wrapper names from active Rust:
  `step_semop`, `step_semop_v3`, `step_semctl`, `step_semctl_in_ns`, and
  `step_sem_undo`. The remaining SysV sem public execution surface is
  caller-posting `_with_post`; no-context tests and procfs setup pass explicit
  direct closures, shim syscall paths inject `SyscallCtx` mailbox-ref posting,
  and process exit carries its existing wake-post closure into
  `step_sem_undo_with_post` for SEM_UNDO changed-source publication.
- Retired the TTY no-context direct `step_ingest` wrapper from active Rust.
  TTY ingest now exposes only `step_ingest_with_post`; no-context tests,
  hardware poll, pty write, VFS setup, and shim tests pass explicit
  hint-aware direct closures, while kernel console ingest remains on the
  current-hart injected route. The `time-wake-retired` gate rejects
  `step_ingest(` definitions and calls without treating the `step_ingest`
  module filename as an interface.
- Retired the reactor-local no-context direct coordination wrappers
  `Completion::complete`, `CountdownCompletion::arrive`, and
  `SyncRendezvous::ack` from active Rust. Reactor coordination now exposes
  only `complete_with_post`, `arrive_with_post`, and `ack_with_post`; tests use
  explicit direct mailbox-ref closures or owner-aware reactor post injection.

## Verification

- `git diff --check -- docs/design/02_execution/TIME_WAKE_v1.md docs/design/INDEX.md docs/progress/research/2026-07-05-time-wake-routing-design.md docs/progress/research/2026-07-06-time-wake-design-refactor.md docs/progress/STATUS.md`
  passed.
- `git diff --check -- docs/design/02_execution/TIME_WAKE_v1.md docs/design/01_substrate/HAL_v1.md docs/design/06_devices/DEVICE.md docs/Txv3/08_OBSERVATION_v1.md docs/progress/research/2026-07-06-time-wake-design-refactor.md docs/progress/STATUS.md`
  passed after the final target-state expansion.
- `cargo xtask lint docs` passed. It reported the pre-existing
  stale-vocabulary warning class and completed with `docs lint: ok`.
- `cargo xtask progress validate` passed with `progress records: ok (29
  file(s))`.
- `git diff --check -- docs/design/02_execution/TIME_WAKE_v1.md` passed after
  the complete design-reference expansion.
- `cargo xtask lint docs` passed after the complete design-reference expansion;
  it reported the expected stale-vocabulary warning class.
- `cargo xtask progress validate` passed after the complete
  design-reference expansion with `progress records: ok (29 file(s))`.

Stage2 design implementation-entry pass verification:

- `git diff --check -- docs/stage2-documents/time_infra/TX_TIME_WAKE_DESIGN.md docs/progress/STATUS.md docs/progress/research/2026-07-06-time-wake-design-refactor.md`
  passed.
- `rg -n '[ \t]+$' docs/stage2-documents/time_infra/TX_TIME_WAKE_DESIGN.md docs/progress/STATUS.md docs/progress/research/2026-07-06-time-wake-design-refactor.md`
  returned no hits.
- `cargo xtask progress validate` passed with `progress records: ok (29
  file(s))`.
- `cargo xtask lint docs` passed; it reported the expected retired-term
  warning class and ended with `docs lint: ok`.

Maintainer-handoff closure verification:

- `cargo fmt --check -p tx-subsystems` passed.
- `cargo test -p tx-subsystems --test v3_vfs_waitsource -- --nocapture`
  passed with `3 passed`.
- `cargo check -p tx-subsystems -q` passed with the existing unrelated
  `step_connect.rs` unused `guard` warning.
- The VFS old direct notification audit returned no hits.
- The active Rust retired time/wake interface audit returned no hits.
- Scoped `git diff --check` over the VFS slice and time/wake docs passed.
- Trailing-whitespace scan over the same scoped files returned no hits.
- `cargo xtask progress validate` passed with `progress records: ok (29
  file(s))`.
- `cargo xtask lint docs` passed; it reported the expected retired-term
  warning class and ended with `docs lint: ok`.

POSIX mq mailbox-ref post-seam verification:

- `cargo fmt -p tx-subsystems -p tx-shims` passed.
- `cargo test -p tx-shims dispatch_mq_blocking -- --nocapture` passed with
  the two POSIX mq blocking send/receive tests green.
- `cargo check -p tx-subsystems -q && cargo check -p tx-shims -q` passed with
  the existing unrelated `step_connect.rs` unused `guard` and
  `tx_ext4_bridge.rs` unused `Vec` warnings.
- `cargo fmt --check -p tx-subsystems -p tx-shims` passed.
- The POSIX mq old direct notification audit returned no hits.
- The active Rust retired time/wake interface audit returned no hits.
- Scoped `git diff --check` over the POSIX mq slice and time/wake docs passed.
- Trailing-whitespace scan over the same scoped files returned no hits.
- `cargo xtask progress validate` passed with `progress records: ok (29
  file(s))`.
- `cargo xtask lint docs` passed; it reported the expected retired-term
  warning class and ended with `docs lint: ok`.

SysV msg mailbox-ref post-seam verification:

- `cargo fmt -p tx-subsystems -p tx-shims` passed.
- `cargo test -p tx-subsystems --lib with_post_uses_injected_mailbox_ref_post -- --nocapture`
  passed with the two SysV msg focused post-seam tests green.
- `cargo test -p tx-shims --lib dispatch_sysv_msg -- --nocapture` passed with
  the three SysV msg syscall dispatch tests green.
- `cargo check -p tx-subsystems -q && cargo check -p tx-shims -q` passed with
  the existing unrelated `step_connect.rs` unused `guard` and
  `tx_ext4_bridge.rs` unused `Vec` warnings.
- `cargo fmt --check -p tx-subsystems -p tx-shims` passed.
- The SysV msg old direct notification audit returned no hits.
- The active Rust retired time/wake interface audit returned no hits.
- Scoped `git diff --check` over the SysV msg slice and time/wake docs passed.
- Trailing-whitespace scan over the same scoped files returned no hits.
- `cargo xtask progress validate` passed with `progress records: ok (29
  file(s))`.
- `cargo xtask lint docs` passed; it reported the expected retired-term
  warning class and ended with `docs lint: ok`.

SysV sem direct-wrapper retirement verification:

- Strict grep over `crates` and `boards` for `step_semop`, `step_semop_v3`,
  `step_semctl`, `step_semctl_in_ns`, and `step_sem_undo` returned no active
  Rust hits.
- `cargo test -p tx-subsystems --lib sysv_sem -- --nocapture` passed with
  the eight SysV sem and SEM_UNDO focused tests green.
- `cargo test -p tx-subsystems --lib sysv_sem_undo -- --nocapture` passed with
  the three SEM_UNDO process tests green.
- `cargo test -p tx-fs procfs_sysvipc_files_render_live_sysv_ipc_rows -- --nocapture`
  passed.
- `cargo test -p tx-shims --lib dispatch_sysv_sem -- --nocapture` passed with
  the nine SysV sem syscall dispatch tests green.
- `cargo fmt --check -p tx-subsystems -p tx-shims -p tx-fs -p xtask` passed.
- `cargo test -p xtask lint_invariants_time_wake -- --nocapture` passed.
- `cargo xtask lint invariants time-wake-retired` passed with `0` retired
  sites.

TTY direct-ingest wrapper retirement verification:

- Strict grep over `crates` and `boards` for `step_ingest(` returned no active
  Rust hits.
- `cargo test -p tx-subsystems --lib tty -- --nocapture` passed with 118
  tty-filtered lib tests green.
- `cargo test -p tx-subsystems --test v3_tty_waitsource -- --nocapture`
  passed with both TTY wait-source tests green.
- `cargo test -p tx-subsystems --lib vfs -- --nocapture` passed with 53
  passed and 11 ignored vfs-filtered tests.
- `cargo test -p tx-shims --lib dispatch_read_blocks_until_tty_input_then_returns_byte -- --nocapture`
  passed.
- `cargo test -p tx-kernel dispatch_irq_routes_uart_rx_to_tty_deferred_ingest -- --nocapture`
  passed.
- `cargo fmt --check -p tx-kernel -p tx-subsystems -p tx-shims -p xtask`
  passed.
- `cargo test -p xtask lint_invariants_time_wake -- --nocapture` passed.
- `cargo xtask lint invariants time-wake-retired` passed with `0` retired
  sites.

Reactor-local coordination direct-wrapper retirement verification:

- Strict grep over `crates/tx-reactor` for `pub fn complete`, `pub fn arrive`,
  `pub fn ack`, `.complete(`, `.arrive(`, and `.ack(` returned no hits.
- `cargo test -p tx-reactor --test completion -- --nocapture` passed with all
  eight completion tests green.
- `cargo test -p tx-reactor --test sync_coord -- --nocapture` passed with all
  six sync-rendezvous tests green.
- `cargo fmt --check -p tx-reactor -p xtask` passed.
- `cargo test -p xtask lint_invariants_time_wake -- --nocapture` passed.
- `cargo xtask lint invariants time-wake-retired` passed with `0` retired
  sites.

Final current-status alignment verification:

- `git diff --check -- docs/design/02_execution/TIME_WAKE_v1.md docs/progress/research/2026-07-06-time-wake-design-refactor.md docs/progress/STATUS.md`
  passed.
- `cargo xtask lint docs` passed with the expected retired-term warning class.
- `cargo xtask progress validate` passed with `progress records: ok (29
  file(s))`.

Stage2 complete-design finalization verification:

- scoped `git diff --check` over the touched time/wake docs, progress docs,
  and current Package G pipe files passed.
- trailing-whitespace scan over the same file set returned no hits.
- active-Rust retired-interface audits for the old broad time/timer route and
  the retired direct `ITIMER_REAL` wrappers returned no hits.
- `cargo xtask progress validate` passed with `progress records: ok (29
  file(s))`.
- `cargo xtask lint docs` passed; it reported the expected retired-term warning
  class and ended with `docs lint: ok`.

2026-07-07 thread-future design-status verification:

- `cargo test -p tx-kernel fatal_signal_teardown_uses_injected_mailbox_post -- --nocapture`
  passed.
- `cargo test -p tx-subsystems --test v3_signal_mailbox -- --nocapture`
  passed with the existing unrelated `step_connect.rs` unused-variable warning.
- `cargo check -p tx-kernel -q && cargo check -p tx-subsystems -q` completed
  with existing unrelated warnings in `step_connect.rs`, `tx_ext4_bridge.rs`,
  and `CoreInit::unregister_thread_reactor_task`.
- `cargo xtask progress validate` passed with `progress records: ok (29
  file(s))`.
- `cargo xtask lint docs` passed with the expected retired-term warning class.
- `git diff --check -- docs/design/02_execution/TIME_WAKE_v1.md docs/stage2-documents/time_infra/TX_TIME_WAKE_DESIGN.md docs/progress/STATUS.md docs/progress/research/2026-07-06-time-wake-design-refactor.md`
  passed.
- `rg -n '[ \t]+$' docs/design/02_execution/TIME_WAKE_v1.md docs/stage2-documents/time_infra/TX_TIME_WAKE_DESIGN.md docs/progress/STATUS.md docs/progress/research/2026-07-06-time-wake-design-refactor.md`
  returned no hits.
- The hard old-interface audit over active Rust code returned no hits:
  `rg -n '\bTimeIf\b|TimerQueue|DeadlineFuture|timer_sleep|install_timer_queue|sleep_until_ns|DirectMailboxTimerWakeRouter|\.fire_due\(|timer_queue|fixed_oscomp_time|binding\.name == "rtc"' crates boards --glob '*.rs'`.

2026-07-07 syscall-context signal-post verification:

- `cargo fmt --check -p tx-shims -p tx-subsystems -p tx-kernel` passed.
- `cargo test -p tx-shims dispatch_tkill_uses_syscall_ctx_mailbox_post -- --nocapture`
  passed.
- `cargo test -p tx-shims dispatch_tgkill_uses_syscall_ctx_mailbox_post -- --nocapture`
  passed.
- `cargo check -p tx-shims -q && cargo check -p tx-kernel -q && cargo check -p tx-subsystems -q`
  passed with existing unrelated warnings in `step_connect.rs`,
  `tx_ext4_bridge.rs`, and test-only `tx-kernel` helpers.
- `cargo test -p tx-subsystems --test v3_signal_mailbox -- --nocapture`
  passed.
- `cargo test -p tx-kernel fatal_signal_teardown_uses_injected_mailbox_post -- --nocapture`
  passed.
- The hard old-interface audit over active Rust code returned no hits:
  `rg -n '\bTimeIf\b|TimerQueue|DeadlineFuture|timer_sleep|install_timer_queue|sleep_until_ns|DirectMailboxTimerWakeRouter|\.fire_due\(|timer_queue|fixed_oscomp_time|binding\.name == "rtc"' crates boards --glob '*.rs'`.

Complete design-document closure verification:

- `git diff --check -- docs/progress/STATUS.md docs/progress/research/2026-07-06-time-wake-design-refactor.md`
  passed.
- `cargo xtask progress validate` passed with `progress records: ok (29
  file(s))`.
- `cargo xtask lint docs` passed with the expected retired-term warning class
  and `docs lint: ok`.

Complete design-contract pass verification:

- `cargo xtask progress validate` passed with `progress records: ok (29
  file(s))`.
- `cargo xtask lint docs` passed with the expected retired-term warning class
  and `docs lint: ok`.
- Scoped `git diff --check` over touched tracked progress/index files passed.
- Old-interface hard audit over active Rust returned no matches:
  `rg -n '\bTimeIf\b|TimerQueue|DeadlineFuture|timer_sleep|install_timer_queue|sleep_until_ns|DirectMailboxTimerWakeRouter|\.fire_due\(|timer_queue|fixed_oscomp_time|binding\.name == "rtc"' crates boards --glob '*.rs'`.

Package G delegate-timeout convergence verification:

- `cargo fmt --check -p tx-substrate -p tx-reactor` passed.
- `cargo check -p tx-substrate -q` passed.
- `cargo check -p tx-reactor -q` passed.
- `cargo test -p tx-reactor timer_registry_routes_delegate_timeout_tokens_through_router -- --nocapture`
  passed.
- `cargo test -p tx-reactor delegate_timeout_routes_through_owner_aware_timer_tick -- --nocapture`
  passed.
- `cargo test -p tx-substrate --test v3_agent_token_guard_timer -- --nocapture`
  passed.
- `cargo test -p tx-reactor --test v3_pr7b_timer_routing -- --nocapture`
  passed.

Final document-navigation pass verification:

- `git diff --check -- docs/design/02_execution/TIME_WAKE_v1.md docs/progress/STATUS.md docs/progress/research/2026-07-06-time-wake-design-refactor.md`
  passed.
- `cargo xtask progress validate` passed with `progress records: ok (29
  file(s))`.
- `cargo xtask lint docs` passed with the expected retired-term warning class
  and `docs lint: ok`.
- Old-interface hard audit over active Rust returned no matches:
  `rg -n '\bTimeIf\b|TimerQueue|DeadlineFuture|timer_sleep|install_timer_queue|sleep_until_ns|DirectMailboxTimerWakeRouter|\.fire_due\(|timer_queue|fixed_oscomp_time|binding\.name == "rtc"' crates boards --glob '*.rs'`.

Stage2 Linux reference completion verification:

- `git diff --check -- docs/stage2-documents/time_infra/README.md` passed.
- `cargo xtask progress validate` passed with `progress records: ok (29 file(s))`.
- `cargo xtask lint docs` passed with the expected retired-term warning class
  and `docs lint: ok`.

Implementation blueprint closeout verification:

- `git diff --check -- docs/design/02_execution/TIME_WAKE_v1.md docs/progress/STATUS.md docs/progress/research/2026-07-06-time-wake-design-refactor.md`
  passed.
- `cargo xtask progress validate` passed with `progress records: ok (29
  file(s))`.
- `cargo xtask lint docs` passed with the expected retired-term warning class
  and `docs lint: ok`.
- Old-interface hard audit over active Rust returned no matches:
  `rg -n '\bTimeIf\b|TimerQueue|DeadlineFuture|timer_sleep|install_timer_queue|sleep_until_ns|DirectMailboxTimerWakeRouter|\.fire_due\(|timer_queue|fixed_oscomp_time|binding\.name == "rtc"' crates boards --glob '*.rs'`.

Owner-aware post documentation alignment verification:

- `git diff --check -- docs/design/02_execution/TIME_WAKE_v1.md docs/progress/research/2026-07-06-time-wake-design-refactor.md docs/progress/STATUS.md`
  passed.
- `cargo xtask progress validate` passed with `progress records: ok (29
  file(s))`.
- `cargo xtask lint docs` passed with the expected retired-term warning class
  and `docs lint: ok`.
- Old-interface hard audit over active Rust returned no matches:
  `rg -n '\bTimeIf\b|TimerQueue|DeadlineFuture|timer_sleep|install_timer_queue|sleep_until_ns|DirectMailboxTimerWakeRouter|\.fire_due\(|timer_queue|fixed_oscomp_time|binding\.name == "rtc"' crates boards --glob '*.rs'`.

Hardware RTC backend design closure verification:

- `git diff --check -- docs/design/02_execution/TIME_WAKE_v1.md docs/progress/research/2026-07-06-time-wake-design-refactor.md docs/progress/STATUS.md`
  passed.
- `cargo xtask progress validate` passed with `progress records: ok (29
  file(s))`.
- `cargo xtask lint docs` passed with the expected retired-term warning class
  and `docs lint: ok`.
- Old-interface hard audit over active Rust returned no matches:
  `rg -n '\bTimeIf\b|TimerQueue|DeadlineFuture|timer_sleep|install_timer_queue|sleep_until_ns|DirectMailboxTimerWakeRouter|\.fire_due\(|timer_queue|fixed_oscomp_time|binding\.name == "rtc"' crates boards --glob '*.rs'`.

RV64 goldfish RTC backend verification:

- `cargo fmt --check -p tx-hal-riscv64-qemu-virt` passed.
- `cargo check -p tx-hal-riscv64-qemu-virt -q` passed.
- `cargo test -p tx-hal-riscv64-qemu-virt goldfish -- --nocapture` passed with
  5 focused tests.
- `cargo test -p tx-hal-riscv64-qemu-virt rtc -- --nocapture` passed.
- `cargo check -p tx-kernel -q` passed with existing unrelated warnings.
- Scoped `git diff --check` over the touched RV64 board files passed.
- Old-interface hard audit over active Rust returned no matches.
- `cargo fmt --check -p tx-hal -p tx-kernel -p tx-hal-riscv64-qemu-virt`
  passed after the RTC IRQ publication slice.
- `cargo check -p tx-hal -q` passed.
- `cargo test -p tx-kernel rtc_irq_handler_publishes_alarm_event_to_devfs_rtc_state -- --nocapture`
  passed.
- `cargo test -p tx-kernel install_irq_handlers_publishes_table_to_platform -- --nocapture`
  passed.

LA64 LS7A RTC backend verification:

- `cargo fmt --check -p tx-hal-loongarch64-qemu-virt` passed.
- `cargo check -p tx-hal-loongarch64-qemu-virt -q` passed.
- `cargo test -p tx-hal-loongarch64-qemu-virt ls7a_persistent_clock -- --nocapture`
  passed.
- `cargo test -p tx-hal-loongarch64-qemu-virt qemu_la64_mmio_regions_include_ls7a_rtc -- --nocapture`
  passed.
- `cargo test -p tx-hal-loongarch64-qemu-virt la64_platform_overrides_rtc_irq_constant -- --nocapture`
  passed.
- `cargo test -p tx-hal-loongarch64-qemu-virt irq -- --nocapture` passed.
- `cargo check -p tx-kernel-loongarch64-qemu-virt -q` passed with existing
  unrelated warnings.
- `cargo check -p tx-kernel -q` passed with existing unrelated warnings.

Future-stealing design closure verification:

- `git diff --check -- docs/progress/research/2026-07-06-time-wake-design-refactor.md docs/progress/STATUS.md`
  passed, and a text-level trailing-whitespace check over
  `docs/design/02_execution/TIME_WAKE_v1.md` plus the touched progress files
  passed.
- `cargo xtask progress validate` passed with `progress records: ok (29
  file(s))`.
- `cargo xtask lint docs` passed with the expected stale-vocabulary warning
  class and `docs lint: ok`.
- Old-interface hard audit over active Rust returned no matches:
  `rg -n '\bTimeIf\b|TimerQueue|DeadlineFuture|timer_sleep|install_timer_queue|sleep_until_ns|DirectMailboxTimerWakeRouter|\.fire_due\(|timer_queue|fixed_oscomp_time|binding\.name == "rtc"' crates boards --glob '*.rs'`.

Signal process-producer post-seam verification:

- Added caller-posting variants for the process-level signal producer path:
  `set_thread_zombie_with_post`, `step_exit_group_with_post`,
  `step_exit_group_with_signal_with_post`, `step_kill_process_with_post`, and
  `route_gewalt_with_post`.
- The new seam carries the injected post through catchable process-directed
  signal selection, SIGSTOP/SIGCONT Gewalt fanout, and SIGKILL terminal
  zombify wake hints while keeping default no-context wrappers on direct
  best-effort mailbox posting.
- `cargo fmt --check -p tx-subsystems` passed.
- `cargo test -p tx-subsystems --test v3_signal_mailbox -- --nocapture`
  passed; the test first failed on the missing `step_kill_process_with_post`
  API before implementation.
- `cargo check -p tx-subsystems -q` passed with the existing unrelated
  `step_connect.rs` unused-variable warning.
- Old-interface hard audit over active Rust returned no matches:
  `rg -n '\bTimeIf\b|TimerQueue|DeadlineFuture|timer_sleep|install_timer_queue|sleep_until_ns|DirectMailboxTimerWakeRouter|\.fire_due\(|timer_queue|fixed_oscomp_time|binding\.name == "rtc"' crates boards --glob '*.rs'`.

ITIMER_REAL producer post-seam verification:

- `cargo fmt --check -p tx-shims -p tx-subsystems -p tx-kernel` passed.
- `cargo test -p tx-shims dispatch_itimer_real_boundary_uses_syscall_ctx_mailbox_post -- --nocapture`
  passed.
- `cargo check -p tx-shims -q && cargo check -p tx-kernel -q && cargo check -p tx-subsystems -q`
  passed with existing unrelated warnings in `step_connect.rs`,
  `tx_ext4_bridge.rs`, and `CoreInit::unregister_thread_reactor_task`.
- The old direct itimer/signal wrapper audit over active Rust returned no
  matches:
  `rg -n "maybe_deliver_itimer_signal\\b|fire_itimer_real\\(|deliver_signal_if_handler\\(" crates/tx-shims/src crates/tx-kernel/src crates/tx-subsystems/src --glob '*.rs'`.
- The hard old-interface audit over active Rust returned no matches:
  `rg -n '\bTimeIf\b|TimerQueue|DeadlineFuture|timer_sleep|install_timer_queue|sleep_until_ns|DirectMailboxTimerWakeRouter|\.fire_due\(|timer_queue|fixed_oscomp_time|binding\.name == "rtc"' crates boards --glob '*.rs'`.
- `git diff --check -- crates/tx-shims/src/linux_syscall/time.rs docs/design/02_execution/TIME_WAKE_v1.md docs/stage2-documents/time_infra/TX_TIME_WAKE_DESIGN.md docs/progress/STATUS.md docs/progress/research/2026-07-06-time-wake-design-refactor.md`
  passed.
- `rg -n '[ \t]+$' crates/tx-shims/src/linux_syscall/time.rs docs/design/02_execution/TIME_WAKE_v1.md docs/stage2-documents/time_infra/TX_TIME_WAKE_DESIGN.md docs/progress/STATUS.md docs/progress/research/2026-07-06-time-wake-design-refactor.md`
  returned no hits.
- `cargo xtask progress validate` passed with `progress records: ok (29
  file(s))`.
- `cargo xtask lint docs` passed with the expected retired-term warning class
  and `docs lint: ok`.

Process exit-source mailbox-ref post-seam verification:

- `cargo fmt --check -p tx-subsystems -p tx-shims -p tx-kernel` passed.
- `cargo test -p tx-shims dispatch_exit_group_uses_syscall_ctx_mailbox_ref_post_for_parent_exit_source -- --nocapture`
  passed.
- `cargo check -p tx-subsystems -q && cargo check -p tx-shims -q && cargo check -p tx-kernel -q`
  passed with existing unrelated warnings in `step_connect.rs`,
  `tx_ext4_bridge.rs`, and `CoreInit::unregister_thread_reactor_task`.
- `cargo test -p tx-subsystems --test v3_exit_wait_source -- --nocapture`
  passed.
- `cargo test -p tx-subsystems process::tests::exit_source -- --nocapture`
  passed.
- The old direct itimer/signal wrapper audit over active Rust returned no
  matches:
  `rg -n "maybe_deliver_itimer_signal\\b|fire_itimer_real\\(|deliver_signal_if_handler\\(" crates/tx-shims/src crates/tx-kernel/src crates/tx-subsystems/src --glob '*.rs'`.
- The hard old-interface audit over active Rust returned no matches:
  `rg -n '\bTimeIf\b|TimerQueue|DeadlineFuture|timer_sleep|install_timer_queue|sleep_until_ns|DirectMailboxTimerWakeRouter|\.fire_due\(|timer_queue|fixed_oscomp_time|binding\.name == "rtc"' crates boards --glob '*.rs'`.
- `cargo xtask progress validate` passed with `progress records: ok (29
  file(s))`.
- `cargo xtask lint docs` passed with the expected retired-term warning class
  and `docs lint: ok`.

Complete design document landing-map update:

- Extended
  `docs/stage2-documents/time_infra/TX_TIME_WAKE_DESIGN.md` with a concrete
  landing map for HAL, timekeeper, timer registry, wake substrate, reactor,
  scheduler, RTC device, devfs/RNode, and syscall/script drivers.
- Added the legal dependency graph that keeps `tx-subsystems -> tx-reactor`
  out of the design: scheduler-context callers inject owner-aware post
  closures, while no-context producers publish to wait sources.
- Added active-code retirement audit commands and package exit evidence so a
  package is not considered finished while its old route still exists in
  parallel.
- Added a completeness boundary explaining which Linux-parity families are
  covered now and which are reserved extension points, avoiding another broad
  `TimeIf`/private timer queue/device-path shortcut later.

Futex syscall-context mailbox-ref post-seam update:

- Added `WaitSource::notify_limit_emit_with_owner_post` so scheduler-context
  producers can preserve the existing `notify_limit_emit` cap while routing
  each already-upgraded mailbox through an injected owner-aware post closure.
- Added futex adapter/notification wrappers for exact waiter wake publication:
  `notify_v3_source_limit_emit_with_post` and
  `notify_exact_limit_with_post`.
- Added `step_futex_wake_masked_with_hint_and_post_in` and changed
  `sys_futex` wake helpers to inject
  `SyscallCtx::post_mailbox_ref_event_with_hint`; this covers ordinary
  `FUTEX_WAKE`, `FUTEX_WAKE_BITSET`, and the wake helper used by
  `FUTEX_WAKE_OP`.
- Upgraded the syscall-context mailbox-ref seam to carry scheduler hints:
  `SyscallCtx::post_mailbox_ref_event_with_hint`, reactor
  `post_mailbox_ref_event_with_hint_from_hart`, and kernel
  `post_mailbox_ref_event_with_hint_from_current_hart`. The older
  `with_mailbox_ref_post` test/fallback shape remains available and maps to
  `Normal`.
- Focused verification passed:
  `cargo fmt --check -p tx-substrate -p tx-subsystems -p tx-shims -p
  tx-reactor -p tx-kernel`,
  `cargo test -p tx-shims
  dispatch_futex_wake_uses_syscall_ctx_mailbox_ref_post_with_hint --
  --nocapture`,
  `cargo test -p tx-shims direct_trap_futex_wake_uses_wake_handoff_hint --
  --nocapture`,
  `cargo test -p tx-subsystems --test v3_futex_waitsource -- --nocapture`,
  `cargo test -p tx-substrate wait_source -- --nocapture`, and
  `cargo check -p tx-kernel -q && cargo check -p tx-shims -q && cargo check
  -p tx-subsystems -q && cargo check -p tx-reactor -q && cargo check -p
  tx-substrate -q`.
  The only warnings observed were the existing unrelated
  `step_connect.rs` unused `guard`, `tx_ext4_bridge.rs` unused `Vec`, and
  `CoreInit::unregister_thread_reactor_task` dead-code warnings.

Eventfd syscall-context mailbox-ref post-seam update:

- Added eventfd notification caller-posting verbs:
  `notify_readable_with_post` and `notify_writable_with_post`.
- Added eventfd step-level caller-posting verbs:
  `step_eventfd_read_with_post` and `step_eventfd_write_with_post`; the old
  `step_eventfd_read` / `step_eventfd_write` entry points remain as
  no-context fallback wrappers that call the same surface with direct mailbox
  posting.
- Changed `sys_eventfd_read` / `sys_eventfd_write` to inject
  `SyscallCtx::post_mailbox_ref_event`, so read-side writable publication and
  write-side readable publication can route through the owner-aware syscall
  context when reactor context exists.
- Removed the now-unused eventfd-local direct `notify_v3_source` adapter entry;
  fallback direct posting now flows through the `_with_post` surface.
- Focused verification passed:
  `cargo fmt --check -p tx-subsystems -p tx-shims`,
  `cargo test -p tx-shims
  dispatch_eventfd_write_uses_syscall_ctx_mailbox_ref_post_for_reader_wake --
  --nocapture`,
  `cargo test -p tx-subsystems eventfd -- --nocapture`, and
  `cargo check -p tx-subsystems -q && cargo check -p tx-shims -q`.
  The only warnings observed were the existing unrelated `step_connect.rs`
  unused `guard` and `tx_ext4_bridge.rs` unused `Vec` warnings.

Pipe syscall-context mailbox-ref post-seam update:

- Added pipe wait-source caller-posting helpers:
  `notify_v3_source_with_post`, `notify_readable_with_post`, and
  `notify_writable_with_post`. The no-context `notify_readable` /
  `notify_writable` wrappers now delegate through the same surface with direct
  mailbox posting.
- Added pipe read/write caller-posting helpers:
  `step_read_with_post` and `step_write_with_post`, plus
  `ReadWithHintPostOp` / `WriteWithHintPostOp` for syscall paths that receive
  the hint-aware owner post function pointer from `SyscallCtx`.
- Changed `sys_pipe_read_buffered` and `sys_pipe_write_buffered` to pass
  `ctx.mailbox_ref_post_with_hint`, so pipe reader/writer wait-source
  publication uses the same owner-aware route as futex when real reactor
  context is available. The post is invoked with `MailboxSchedulerHint::Normal`
  because pipe readiness has no futex-style handoff hint.
- Focused verification passed:
  `cargo fmt --check -p tx-subsystems -p tx-shims`,
  `cargo test -p tx-shims
  dispatch_pipe_write_uses_syscall_ctx_mailbox_ref_post_for_reader_wake --
  --nocapture`,
  `cargo test -p tx-subsystems pipe -- --nocapture`, and
  `cargo check -p tx-subsystems -q && cargo check -p tx-shims -q`.
  The only warnings observed were the existing unrelated
  `step_connect.rs` unused `guard` and `tx_ext4_bridge.rs` unused `Vec`
  warnings.

Timerfd syscall-context mailbox-ref post-seam update:

- Added timerfd wait-source caller-posting helpers:
  `notify_v3_source_with_post` and `notify_readable_with_post`. The no-context
  `notify_readable` wrapper delegates through the same surface with direct
  mailbox posting.
- Added `timerfd_settime_with_flags_and_post`, and changed
  `sys_timerfd_settime` to inject `SyscallCtx::post_mailbox_ref_event` for the
  immediate-readable case where arming an already-expired timer publishes
  timerfd readable readiness.
- Fixed timerfd read-side observation for already accumulated expiration
  counts. A one-shot immediate expiration can clear `deadline_ns` while
  increasing `expiration_count`; `step_timerfd_read` now drains that count
  instead of requiring the next `bump_expirations()` call to return nonzero.
- Focused verification passed:
  `cargo fmt --check -p tx-subsystems -p tx-shims`,
  `cargo test -p tx-shims
  dispatch_timerfd_settime_uses_syscall_ctx_mailbox_ref_post_for_readable_wake
  -- --nocapture`,
  `cargo test -p tx-subsystems timerfd -- --nocapture`, and
  `cargo check -p tx-subsystems -q && cargo check -p tx-shims -q`.
  The only warnings observed were the existing unrelated
  `step_connect.rs` unused `guard` and `tx_ext4_bridge.rs` unused `Vec`
  warnings.

Timerfd realtime clock-set mailbox-ref post-seam update:

- Added the wall-clock mutation caller-posting path:
  `WallClock::set_realtime_ns_with_timerfd_post` and
  `set_realtime_ns_with_persistent_and_timerfd_post` let syscall-context
  callers carry `SyscallCtx::post_mailbox_ref_event` through accepted
  `clock_settime(CLOCK_REALTIME)` / `settimeofday` mutations.
- Added timerfd realtime mutation caller-posting helpers:
  `mark_canceled_on_set_with_post`,
  `revalidate_realtime_deadline_on_set_with_post`, and
  `timerfd_clock_was_set_with_post`. The no-context
  `timerfd_clock_was_set` wrapper delegates through the same helper using
  direct mailbox-ref posting.
- This closes the earlier timerfd follow-up without adding a
  `tx-subsystems -> tx-reactor` dependency. The timekeeper still owns
  realtime offset/generation and vvar publication; timerfd still owns
  cancel-on-set, expiration count, and realtime-deadline semantics; the
  syscall caller owns the scheduler-aware post seam.
- Focused verification passed:
  `cargo fmt -p tx-subsystems -p tx-shims`,
  `cargo fmt --check -p tx-subsystems -p tx-shims`,
  `cargo test -p tx-shims
  dispatch_clock_settime_cancel_on_set_uses_syscall_ctx_mailbox_ref_post --
  --nocapture`,
  `cargo test -p tx-shims
  dispatch_timerfd_settime_uses_syscall_ctx_mailbox_ref_post_for_readable_wake
  -- --nocapture`, and
  `cargo test -p tx-subsystems timerfd -- --nocapture`.
- Design/status sync verification passed:
  `cargo check -p tx-subsystems -q && cargo check -p tx-shims -q` completed
  with the existing unrelated `step_connect.rs` unused `guard` and
  `tx_ext4_bridge.rs` unused `Vec` warnings; scoped `git diff --check`
  passed; trailing-whitespace scan returned no hits; the active Rust
  retired-interface audits returned no hits; `cargo xtask progress validate`
  passed; and `cargo xtask lint docs` passed with the expected retired-term
  warning class.

Signalfd process-signal mailbox-ref post-seam update:

- Added signalfd caller-posting readiness verbs:
  `notify_readable_with_post`, `SignalFd::notify_with_post`, and
  `notify_process_signal_with_post`. The signalfd pending queue remains the
  semantic truth; the caller-provided post only controls how delivered
  `SourceFired` mailbox events reach scheduler placement.
- Added signal-layer double-post seams:
  `step_kill_process_with_posts` and `script_deliver_signal_with_posts`.
  The first post closure is still the weak-mailbox `SignalDelivered` route;
  the second is the mailbox-ref wait-source route for signalfd subscribers.
  Single-post compatibility wrappers delegate through the same helpers with
  direct mailbox-ref posting, so the old direct signalfd notification path is
  not kept as a parallel production route.
- Updated syscall-context signal delivery so process-targeted `kill` and the
  thread-targeted script path pass `SyscallCtx::post_mailbox_ref_event` for
  signalfd wait-source publication while preserving the existing
  `SyscallCtx::post_mailbox_event` signal-mailbox route.
- Focused verification passed:
  `cargo fmt --check -p tx-subsystems -p tx-shims`,
  `cargo test -p tx-subsystems --test v3_signal_mailbox -- --nocapture`,
  `cargo test -p tx-shims dispatch_tkill_uses_syscall_ctx_mailbox_post --
  --nocapture`,
  `cargo test -p tx-shims dispatch_tgkill_uses_syscall_ctx_mailbox_post --
  --nocapture`, and
  `cargo check -p tx-subsystems -q && cargo check -p tx-shims -q`.
  The only warnings observed were the existing unrelated
  `step_connect.rs` unused `guard` and `tx_ext4_bridge.rs` unused `Vec`
  warnings.

Userfaultfd pending-fault mailbox-ref post-seam update:

- Added userfaultfd caller-posting readiness verbs:
  `notify_readable_with_post` and `UserfaultFd::push_fault_msg_with_post`.
  The pending-fault queue remains the semantic truth; the caller-provided post
  only controls how delivered `SourceFired` mailbox events reach scheduler
  placement.
- Added VM fault-script plumbing:
  `UfdDispatchTarget::fault_post`,
  `ProcessUfdDispatch::new_with_post`, and
  `AddressSpace::fault_script_for_process_with_post` carry the mailbox-ref post
  function from the production page-fault caller to the userfaultfd pusher.
  No-context `fault_script_for_process` / `push_fault_msg` callers delegate
  through the same helpers with direct mailbox-ref posting.
- Updated the production thread-future page-fault path so a bound task mailbox
  drives `fault_script_for_process_with_post` and injects
  `post_mailbox_ref_event_with_hint_from_current_hart` for userfaultfd readable
  wait-source publication.
- Focused verification passed:
  `cargo fmt --check -p tx-subsystems -p tx-kernel`,
  `cargo test -p tx-subsystems process_fault_push_uses_injected_mailbox_ref_post_for_ufd_readable_wake -- --nocapture`,
  `cargo test -p tx-subsystems --test v3_userfaultfd_e2e -- --nocapture`,
  `cargo test -p tx-subsystems --test v3_userfaultfd_fault_path -- --nocapture`,
  and
  `cargo check -p tx-subsystems -q && cargo check -p tx-kernel -q`.
  The warnings observed were the existing unrelated `step_connect.rs` unused
  `guard`, `tx_ext4_bridge.rs` unused `Vec`, and
  `unregister_thread_reactor_task` dead-code warnings.

TTY readable mailbox-ref post-seam update:

- Added the TTY caller-posting readiness path:
  `notify_readable_with_post` and `step_ingest_with_post` keep
  line-discipline and input-queue mutation in the TTY subsystem while allowing
  callers to inject hint-aware mailbox-ref posting for the readable wait
  source. The no-context `step_ingest` wrapper delegates through the same
  helper with direct mailbox posting rather than keeping a parallel direct
  notification algorithm.
- Updated the production console-ingest path in `tx-kernel` so both SBI
  console polling and IRQ-deferred UART RX draining can route TTY readable
  wait-source publication through
  `post_mailbox_ref_event_with_hint_from_current_hart` when reactor context is
  initialized.
- Focused verification passed:
  `cargo fmt -p tx-subsystems -p tx-kernel`,
  `cargo check -p tx-subsystems -q`,
  `cargo check -p tx-kernel -q`, and
  `cargo test -p tx-subsystems --test v3_tty_waitsource -- --nocapture`.
  The warnings observed were the existing unrelated `step_connect.rs` unused
  `guard`, `tx_ext4_bridge.rs` unused `Vec`, and
  `unregister_thread_reactor_task` dead-code warnings.

VFS/RNode mailbox-ref post-seam update:

- Added VFS caller-posting readiness verbs:
  `notify_readable_with_post`, `notify_writable_with_post`,
  `RNode::fire_read_wait_with_post`, and
  `RNode::fire_write_wait_with_post`. Per-RNode read/write wait-source state
  remains owned by VFS; the caller-provided post only controls how delivered
  `SourceFired` mailbox events reach scheduler placement.
- The no-context `fire_read_wait` / `fire_write_wait` wrappers delegate
  through the same helpers with direct mailbox-ref posting, so the old direct
  VFS notification path is not kept as a parallel algorithm.
- Focused verification passed:
  `cargo fmt -p tx-subsystems`,
  `cargo check -p tx-subsystems -q`, and
  `cargo test -p tx-subsystems --test v3_vfs_waitsource -- --nocapture`.
  The warning observed was the existing unrelated `step_connect.rs` unused
  `guard`.

Remaining-producer design completion update:

- Completed the remaining Package G detail pass in
  [`TX_TIME_WAKE_DESIGN.md`](../../stage2-documents/time_infra/TX_TIME_WAKE_DESIGN.md).
  The new section turns the remaining producer catalog into implementation
  design for SysV sem, socket/network readiness, RTC and generic device
  readiness, AIO/io_uring completion readiness, and higher-level
  signal/syscall producers.
- The design now names the shared interface shapes expected from remaining
  slices: already-upgraded mailbox-ref `_with_post` helpers, hint-aware
  `_with_hint_post` helpers for socket/network, weak-mailbox task-directed
  posting for signal-like events, timer-router callbacks, and worker-carried
  completion publishers for AIO/io_uring.
- Each remaining slice now has an explicit target boundary:
  SysV sem keeps changed sequencing in the semaphore payload; socket/network
  keeps readiness truth in socket/protocol/netdevice state; RTC keeps pending
  event bits in device/devfs state while HAL only exposes persistent-clock
  facts; AIO/io_uring keep completion queue truth in context/ring objects; and
  higher-level signal/syscall producers keep weak-mailbox delivery separate
  from signalfd or lifecycle wait-source readiness.
- Each remaining slice also has a retirement audit and proof gate, so future
  patches can mechanically distinguish architecture completeness from
  implementation completeness. This documentation update does not claim those
  implementation slices have landed.

SysV sem mailbox-ref post-seam update:

- Added the SysV sem caller-posting readiness path:
  `notify_changed_with_post` is now the semaphore changed-source notification
  verb, replacing the old direct `notify_changed` active wrapper.
  `step_semop_v3_with_post` / `step_semop_with_post` keep semaphore value
  mutation and `SEM_UNDO` bookkeeping in the SysV sem payload while allowing
  callers to inject mailbox-ref posting for changed-source waiters.
- Added caller-posting control and exit paths:
  `step_semctl_with_post` / `step_semctl_in_ns_with_post` publish `IPC_RMID`,
  `SETVAL`, and `SETALL` wakes through the injected post, while
  `step_sem_undo_with_post` keeps process-exit undo adjustments on the same
  helper. The default no-context wrappers delegate through the same helpers
  with direct mailbox-ref posting rather than preserving a parallel
  notification algorithm.
- Updated syscall dispatch:
  `sys_semop` / `sys_semtimedop` carry the `SyscallCtx` mailbox-ref post into
  `SysvSemopWaitOp`, preserving the hint-aware production post when present;
  `sys_semctl` injects `SyscallCtx::post_mailbox_ref_event` into the namespace
  wrapper.
- Focused verification passed:
  `cargo fmt -p tx-subsystems -p tx-shims`,
  `cargo test -p tx-subsystems --lib with_post_uses_injected_mailbox_ref_post -- --nocapture`,
  `cargo test -p tx-shims --lib dispatch_sysv_semop_uses_syscall_ctx_mailbox_ref_post_for_changed_wake -- --nocapture`,
  `cargo test -p tx-shims --lib dispatch_sysv_sem -- --nocapture`,
  `cargo check -p tx-subsystems -q && cargo check -p tx-shims -q`, and
  `cargo fmt --check -p tx-subsystems -p tx-shims`.
  The warnings observed were the existing unrelated `step_connect.rs` unused
  `guard` and `tx_ext4_bridge.rs` unused `Vec` warnings. The SysV sem old
  direct notification audit and active Rust retired time/wake interface audit
  both returned no hits.

Complete design-contract closure update:

- Added section 27 to
  [`TX_TIME_WAKE_DESIGN.md`](../../stage2-documents/time_infra/TX_TIME_WAKE_DESIGN.md).
  The new section turns the full document into a concise review contract:
  a one-page architecture graph, non-negotiable boundaries, the distinction
  between architecture completeness, slice completeness, and implementation
  completeness, a patch review algorithm, and the final acceptance bar for the
  overall time/wake refactor.
- The closure explicitly keeps the current implementation state honest:
  architecture is complete because every known time/wake feature has an owner
  row and extension slot, but implementation remains incomplete until
  socket/network, RTC/device, AIO/io_uring, and any remaining higher-level
  producers finish their owner-aware post migration with mechanical audits and
  focused tests.
- Verification passed:
  scoped `git diff --check` over the touched design/progress files,
  trailing-whitespace scan over the same files, `cargo xtask progress
  validate`, and `cargo xtask lint docs`. Docs lint reported the expected
  retired-term warning class and ended with `docs lint: ok`.

RTC/device RawQueue mailbox-ref post-seam update:

- Added the RTC/device caller-posting readiness path:
  `publish_rtc_event_with_post` records pending RTC event bits and publishes
  the RTC event RawQueue through a supplied mailbox-ref post closure. The
  no-context `publish_rtc_event` wrapper delegates through the same helper.
- Hardware RTC IRQ handling now calls `publish_rtc_event_with_post` with the
  kernel current-hart mailbox-ref post and `MailboxSchedulerHint::Normal`.
  HAL remains limited to persistent-clock IRQ acknowledgement and does not own
  devfs/RNode state.
- Emulated RTC alarms now use a timer-substrate RawQueue wake target:
  `DeviceTimerCallback::with_raw_queue_wake` carries the RTC event RawQueue
  into `TimerRegistry::fire_due_with`; after the callback records device
  pending bits, the timer router posts each RawQueue subscriber mailbox through
  its mailbox-ref event route. `ReactorOwnerWakePost` implements that route,
  so emulated RTC alarm wakes get the same owner-aware placement and remote
  IPI behavior as other scheduler-context wake producers.
- Focused verification passed:
  `cargo test -p tx-fs devfs_rtc_event_with_post_uses_injected_mailbox_ref_post -- --nocapture`,
  `cargo test -p tx-fs devfs_rtc_event_readiness_is_pending_state_and_read_consumes_record -- --nocapture`,
  `cargo test -p tx-fs devfs_rtc_emulated_alarm_uses_timer_router_raw_queue_wake -- --nocapture`,
  `cargo test -p tx-kernel rtc_irq_handler_publishes_alarm_event_to_devfs_rtc_state -- --nocapture`,
  `cargo test -p tx-reactor --test v3_timer_surface device_callback_can_publish_wait_source_through_timer_router -- --nocapture`,
  and
  `cargo test -p tx-reactor --test reactor_smoke device_callback_raw_queue_routes_through_owner_aware_timer_tick -- --nocapture`.
  `cargo check -p tx-substrate -q && cargo check -p tx-reactor -q && cargo check -p tx-fs -q && cargo check -p tx-kernel -q`
  and `cargo fmt --check -p tx-substrate -p tx-reactor -p tx-fs -p tx-kernel`
  passed. The warnings observed were the existing unrelated
  `step_connect.rs` unused `guard`, `tx_ext4_bridge.rs` unused `Vec`, and
  test-only `unregister_thread_reactor_task` dead-code warnings. The active
  Rust retired time/wake interface audit returned no hits.

Socket readiness mailbox-ref post-seam update:

- Added the socket readiness caller-posting path:
  `SocketReadiness::fire_recv_with_post`,
  `SocketReadiness::fire_send_with_post`, and
  `SocketReadiness::fire_accept_with_post` are now the only socket readiness
  fire verbs. The old direct `fire_recv`, `fire_send`, and `fire_accept`
  methods were removed rather than kept as compatibility wrappers.
- Retired direct packet-publish wrappers:
  `NetworkPublish::publish_to_with_post` and
  `NetworkPublishTarget::publish_with_post` now cover packet demux, loopback,
  TCP, UDP, ICMP, SCTP, netdevice, netlink, and host-test publication. The old
  direct `publish_to` / `publish` wrappers are absent, so no-context paths
  must explicitly pass the direct mailbox-ref post helper.
- Updated syscall-context producers:
  packet-socket ARP reply publication in `socket.rs` and socket
  `fcntl(F_SETFL)` send-space publication in `fs_basic.rs` now inject
  `SyscallCtx::post_mailbox_ref_event` rather than using a direct socket
  readiness post.
- Focused verification passed:
  `cargo test -p tx-subsystems --lib network_publish_uses_injected_mailbox_ref_post_for_socket_readiness -- --nocapture`,
  `cargo test -p tx-shims --lib dispatch_fcntl_setfl_socket_uses_syscall_ctx_mailbox_ref_post_for_send_space -- --nocapture`,
  `cargo check -p tx-subsystems -q && cargo check -p tx-shims -q`, and
  `cargo fmt -p tx-subsystems -p tx-shims`. The socket readiness
  direct-interface audit returned no hits:
  `rg -n '\bfn fire_(recv|send|accept)\b|\.fire_(recv|send|accept)\(|publish_to\(|\.publish\(\)' crates/tx-subsystems/src/net crates/tx-shims/src crates/tx-kernel/src boards --glob '*.rs'; test $? -eq 1`.
  The active Rust retired time/wake interface audit also returned no hits.
  Existing unrelated warnings remain `step_connect.rs` unused `guard` and
  `tx_ext4_bridge.rs` unused `Vec`.
- Remaining network work:
  the socket readiness sub-slice is complete, but the broader network row still
  has direct `net_delegate_kick_poll` / `net_delegate_kick_tick` callers. The
  next network slice should add `net_delegate_kick_*_with_post` without adding
  a `tx-subsystems -> tx-reactor` dependency.

2026-07-07 complete design-document entry update:

- Strengthened
  [`TX_TIME_WAKE_DESIGN.md`](../../stage2-documents/time_infra/TX_TIME_WAKE_DESIGN.md)
  as the standalone reader-facing complete design document by adding a stable
  complete-contents table for sections 1-27.
- Added a current-state snapshot at the document entry that explicitly
  separates architecture completeness from partial implementation status.
  The snapshot names the remaining implementation rows rather than implying
  completion: network delegate kick, AIO/io_uring completion readiness,
  real-board or firmware-backed RTC witnesses beyond the QEMU profiles, and
  any remaining higher-level wake producers.
- This was documentation-only. It did not change the Package G implementation
  state or close the broader time/wake refactor goal.

2026-07-07 network delegate kick mailbox-ref post-seam update:

- Added the delegate queue caller-posting path:
  `net_delegate_kick_poll_with_post` and
  `net_delegate_kick_tick_with_post` are now the only active delegate queue
  kick verbs. The old direct `net_delegate_kick_poll` /
  `net_delegate_kick_tick` wrappers were removed.
- Kept semantic ownership in `tx-subsystems::net::delegate`: queue bits remain
  delegate state, and subsystem/no-context producers pass
  `net_delegate_direct_mailbox_post` explicitly instead of importing
  `tx-reactor`.
- Routed the scheduler-context boot deadline producer through the kernel
  current-hart post path: `boot_net_deadline_task::<P>` calls
  `net_delegate_kick_tick_with_post` with
  `post_mailbox_ref_event_with_hint_from_current_hart::<P>` and
  `MailboxSchedulerHint::Normal`.
- Migrated the extra virtio-net driver producer in
  `crates/tx-drivers/src/virtio/net.rs` to the same explicit no-context helper
  so the old direct wrappers are absent across `crates/`.
- Focused verification passed:
  `cargo test -p tx-subsystems --lib net_delegate_kick_poll_with_post_uses_injected_mailbox_ref_post -- --nocapture`,
  `cargo test -p tx-subsystems --lib net_delegate_kick_tick_with_post_uses_injected_mailbox_ref_post -- --nocapture`,
  `cargo test -p tx-subsystems --lib net_delegate_reactor_timer_adapter_fires_tick_and_drives_retransmit -- --nocapture`,
  and
  `cargo check -p tx-subsystems -q && cargo check -p tx-drivers -q && cargo check -p tx-kernel -q`.
  Existing unrelated warnings remain `step_connect.rs` unused `guard`,
  `tx_ext4_bridge.rs` unused `Vec`, and the kernel test-only
  `unregister_thread_reactor_task` dead-code warning.

2026-07-07 complete design-document interface dictionary closure:

- Extended
  [`TX_TIME_WAKE_DESIGN.md`](../../stage2-documents/time_infra/TX_TIME_WAKE_DESIGN.md)
  with Appendix A, an implementation-facing interface dictionary and code
  ownership map.
- The appendix names the live interface roles for `MonotonicCounterIf`,
  `DeadlineTimerIf`, `PersistentClockIf`, `TimekeeperIf`, `TimerRegistrar`,
  `TimerRegistry`, `TimerWakeRouter`, `TaskMailbox`, `WaitSource`/`RawQueue`,
  `ReactorOwnerWakePost`, `SyscallCtx` post seams, `RtcDeviceOps`,
  `CharDeviceOps`/RNode binding, and producer-specific `_with_post` seams.
- It also adds a code ownership table, feature-to-path lookup table, and
  design-level retirement audits so future implementation slices can choose
  the correct owner without adding a new layer or reviving old direct routes.
- This was documentation-only. It strengthens the complete design document but
  does not close the remaining implementation rows for AIO/io_uring
  completion readiness, real-board or firmware-backed RTC witnesses beyond the
  QEMU profiles, or any remaining higher-level wake producers.

2026-07-07 signal StepOp direct wrapper retirement:

- Retired the old direct process/thread signal StepOp wrapper names from active
  Rust code. Process-directed delivery now uses `KillProcessWithPostOp`, and
  thread-directed delivery uses `ThreadKillWithPostOp`; `KillProcessOp` and
  `ThreadKillOp` are absent across `crates/tx-subsystems`, `crates/tx-shims`,
  `crates/tx-kernel`, and subsystem integration tests.
- Added a focused `KillProcessWithPostOp` test that binds a task mailbox and
  proves the StepOp uses the injected weak-mailbox post closure rather than a
  hidden direct post. The existing thread-runtime StepOp wrapper tests still
  pass after removing `ThreadKillOp`.
- Focused verification passed:
  `cargo fmt -p tx-subsystems -p tx-shims`,
  `cargo test -p tx-subsystems --lib kill_process_with_post_op_uses_injected_post -- --nocapture`,
  `cargo test -p tx-subsystems --lib thread_runtime::execution::step_op_wraps -- --nocapture`,
  `cargo check -p tx-subsystems -q && cargo check -p tx-shims -q`,
  `cargo fmt --check -p tx-subsystems -p tx-shims`,
  the strict `KillProcessOp|ThreadKillOp` retired-name audit, and the standing
  retired time/timer, signalfd/process, and direct-helper audits. Existing
  unrelated warnings remain `step_connect.rs` unused `guard` and
  `tx_ext4_bridge.rs` unused `Vec`.

2026-07-07 signal mailbox direct helper retirement:

- Retired the old internal `post_signal_mailbox` helper name from active Rust
  code. The only active signal mailbox publication surface is now
  `post_signal_mailbox_with_post`; no-context paths pass an explicit direct
  mailbox-post closure through the same helper.
- Migrated `sync_group_pending_summaries` to call
  `post_signal_mailbox_with_post` directly, preserving the existing best-effort
  fallback behavior while removing the parallel helper name.
- Focused verification passed:
  `cargo fmt -p tx-subsystems -p tx-shims`,
  `cargo test -p tx-subsystems --lib signal::tests::delivery -- --nocapture`,
  `cargo test -p tx-subsystems --test v3_signal_mailbox -- --nocapture`,
  `cargo check -p tx-subsystems -q && cargo check -p tx-shims -q`,
  `cargo fmt --check -p tx-subsystems -p tx-shims`, the strict
  `post_signal_mailbox` retired-name audit, and the standing retired
  time/timer, signalfd/process, direct-helper, and signal StepOp wrapper
  audits. Existing unrelated warnings remain `step_connect.rs` unused `guard`
  and `tx_ext4_bridge.rs` unused `Vec`.

2026-07-07 signal script delivery direct helper retirement:

- Retired the old direct `script_deliver_signal` helper name from active Rust
  code. Tests that need no-reactor signal delivery now use local helpers that
  call `script_deliver_signal_with_post` with an explicit direct mailbox-post
  closure, so the semantic delivery algorithm remains single-path.
- The active design documents now record `script_deliver_signal_with_post` as
  the required signal delivery helper surface for scheduler-context and
  no-context callers alike. This keeps the Package G rule consistent: direct
  fallback is a caller-provided post closure, not a second public wrapper name.
- Focused verification for this slice passed before this documentation
  closeout: strict `script_deliver_signal` retired-name audit,
  `cargo test -p tx-subsystems --lib signal::tests::kill_permission -- --nocapture`,
  `cargo test -p tx-shims --lib dispatch_tkill -- --nocapture`, and
  `cargo test -p tx-shims --lib dispatch_tgkill -- --nocapture`.
- Documentation closeout verification passed:
  `cargo xtask progress validate`; `cargo xtask lint docs` (ok, with expected
  stale-vocabulary warnings for retired terms discussed in active docs);
  `cargo check -p tx-subsystems -q && cargo check -p tx-shims -q`;
  `cargo fmt --check -p tx-subsystems -p tx-shims`; the strict retired-name
  audits for `script_deliver_signal`, time/timer, signalfd/process,
  direct-helper, and signal StepOp wrappers; and scoped `git diff --check`.
  Existing unrelated warnings remain `step_connect.rs` unused `guard` and
  `tx_ext4_bridge.rs` unused `Vec`.

2026-07-07 catchable signal direct wrapper retirement:

- Retired the old direct `post_signal` wrapper from active Rust code.
  Catchable signal publication now has a single semantic surface:
  `post_signal_with_post`. No-context tests pass an explicit direct
  mailbox-post closure through that helper; scheduler-context callers inject
  the owner-aware route through the same seam.
- Updated focused tests and comments so the old helper is no longer an active
  API anchor. `signal::tests::delivery`, `v3_signal_mailbox`, the process
  subject predicate test, the thread-future sigreturn test, and the
  tx-scripts drive signal-interruption tests now use local explicit post
  closures or direct `_with_post` calls.
- The tx-scripts drive test also stopped importing concrete `TimerWheel`
  through `tx_scripts::adapter::wake`; the adapter remains narrow and exposes
  registrar-facing timer surfaces, while the integration fixture imports the
  concrete substrate wheel directly.
- Verification passed:
  `cargo test -p tx-subsystems --lib signal::tests::delivery -- --nocapture`;
  `cargo test -p tx-subsystems --test v3_signal_mailbox -- --nocapture`;
  `cargo test -p tx-subsystems --lib subject_identity_signal_pending_checks_authoritative_pending_state -- --nocapture`;
  `cargo test -p tx-kernel --lib thread_future_sigreturn_recomputes_summary_for_restored_blocked_sigcancel -- --nocapture`;
  `cargo test -p tx-scripts --test drive drive_masked_signal_hint_retries_instead_of_eintr -- --nocapture`;
  `cargo test -p tx-scripts --test drive drive_sa_restart_signal_hint_retries_instead_of_eintr -- --nocapture`;
  `cargo test -p tx-scripts --test drive drive_libc_sigcancel_hint_interrupts_even_with_sa_restart -- --nocapture`;
  `cargo check -p tx-subsystems -q && cargo check -p tx-shims -q && cargo check -p tx-kernel -q`;
  `cargo check -p tx-scripts -q`;
  `cargo fmt --check -p tx-subsystems -p tx-shims -p tx-kernel`;
  `cargo fmt --check -p tx-scripts`;
  the strict `pub fn post_signal|\bpost_signal\(` retired-name audit across
  `crates boards`; and the standing signal direct-wrapper audit. Existing
  unrelated warnings remain
  `step_connect.rs` unused `guard`, `tx_ext4_bridge.rs` unused `Vec`, and
  tx-kernel test-only dead-code warnings.

2026-07-07 RTC direct wrapper retirement:

- Retired the old direct `publish_rtc_event` wrapper from active Rust code.
  RTC event publication now uses `publish_rtc_event_with_post`; no-context
  tests pass an explicit direct mailbox-ref post closure, while the hardware
  RTC IRQ path continues to inject the kernel current-hart owner-aware
  mailbox-ref post.
- The strict direct-wrapper audit
  `rg -n '\bpublish_rtc_event\b|publish_rtc_event\(' crates boards --glob '*.rs'; test $? -eq 1`
  passed. The older discovery audit now reports only `RTC_EVENT_QUEUE` state
  accesses in `tx-fs::devfs`; those are device-state encapsulation follow-up
  work rather than an active direct publish wrapper.
- Verification passed:
  `cargo test -p tx-fs devfs_rtc_event -- --nocapture`;
  `cargo test -p tx-shims rtc -- --nocapture`;
  `cargo check -p tx-fs -q && cargo check -p tx-shims -q`;
  `cargo fmt --check -p tx-fs -p tx-shims`; and the strict
  `publish_rtc_event` retired-name audit. Existing unrelated warnings remain
  `step_connect.rs` unused `guard` and `tx_ext4_bridge.rs` unused `Vec`.

2026-07-07 RTC event-state encapsulation:

- Encapsulated the RTC event wait queue in devfs behind an RTC event-state
  helper. The former raw `RTC_EVENT_QUEUE` static is gone; reset, source-id
  lookup, test queue snapshots, readable clearing after `read(2)`, and event
  publication now go through helper methods.
- The RTC/device audit is now strict for both the direct publish wrapper and
  raw queue/static shape:
  `rg -n '\bpublish_rtc_event\b|publish_rtc_event\(' crates boards --glob '*.rs'; test $? -eq 1`
  and
  `rg -n 'RTC_EVENT_QUEUE|\.fire\(' crates/tx-fs/src/devfs/mod.rs crates/tx-kernel/src/irq.rs --glob '*.rs'; test $? -eq 1`
  both passed.
- Verification passed:
  `cargo test -p tx-fs devfs_rtc_event -- --nocapture`;
  `cargo test -p tx-shims rtc -- --nocapture`;
  `cargo check -p tx-fs -q && cargo check -p tx-shims -q`;
  `cargo fmt --check -p tx-fs -p tx-shims`; and the strict RTC direct
  wrapper plus RTC event-state audits. Existing unrelated warnings remain
  `step_connect.rs` unused `guard` and `tx_ext4_bridge.rs` unused `Vec`.

2026-07-07 generic v3 wait-source direct adapter retirement:

- Retired the remaining old direct `notify_v3_source` adapter wrappers from
  the already-migrated timerfd, pipe, and futex paths. Timerfd and pipe now
  expose `notify_v3_source_with_post`; futex exposes the limit/hint-preserving
  `notify_v3_source_limit_emit_with_post`; none of these adapters keeps a
  direct production shortcut that can bypass caller-provided posting.
- Updated the design completion matrix and final design-level audit in
  [`TX_TIME_WAKE_DESIGN.md`](../../stage2-documents/time_infra/TX_TIME_WAKE_DESIGN.md)
  so the generic v3 wait-source adapter rule is explicit: direct
  `notify_v3_source` wrappers must be absent across active Rust code after the
  slice.
- Verification passed:
  `cargo fmt -p tx-subsystems`;
  `cargo test -p tx-subsystems timerfd -- --nocapture`;
  `cargo test -p tx-subsystems pipe -- --nocapture`;
  `cargo test -p tx-subsystems futex -- --nocapture`;
  `cargo check -p tx-subsystems -q && cargo check -p tx-shims -q`;
  `cargo fmt --check -p tx-subsystems -p tx-shims`; and the strict
  `pub fn notify_v3_source|\bnotify_v3_source\(` audit across
  `crates/tx-subsystems/src`, `crates/tx-shims/src`, `crates/tx-kernel/src`,
  `crates/tx-scripts/src`, and `crates/tx-fs/src`. Existing unrelated warnings
  remain `step_connect.rs` unused `guard` and `tx_ext4_bridge.rs` unused `Vec`.

2026-07-07 page-backed page-ready direct notify retirement:

- Retired the page-backed wait adapter's old direct `notify_source` wrapper.
  Page-ready publication now uses `notify_page_ready_with_post`, which calls
  adapter `notify_source_with_post`; the no-reactor page-cache call sites pass
  explicit direct mailbox-post closures through that same seam.
- Updated [`TIME_WAKE_v1.md`](../../design/02_execution/TIME_WAKE_v1.md) and
  [`TX_TIME_WAKE_DESIGN.md`](../../stage2-documents/time_infra/TX_TIME_WAKE_DESIGN.md)
  so page-backed page-ready waits are part of the Package G completion matrix
  and final strict audit set.
- Verification passed:
  `cargo fmt -p tx-subsystems`;
  `cargo test -p tx-subsystems page_backed -- --nocapture`;
  `cargo check -p tx-subsystems -q`; and strict
  `notify_source\(|tx_substrate::wake::notify\(` audit over
  `crates/tx-subsystems/src/page_backed`, `crates/tx-shims/src`, and
  `crates/tx-kernel/src`. Existing unrelated warning remains
  `step_connect.rs` unused `guard`.

2026-07-07 Chinese design specification completion:

- Expanded
  [`TX_TIME_WAKE_DESIGN_CN.md`](../../stage2-documents/time_infra/TX_TIME_WAKE_DESIGN_CN.md)
  into a standalone Chinese implementation-review specification rather than a
  shorter design entry. The document now opens with a delivery-scope table,
  separates architecture-complete, slice-complete, and implementation-complete
  states, and gives a reading map for architecture, module, implementation,
  and review passes.
- Added the implementation-facing closure sections: concrete landing map and
  legal dependency graph, Package G producer migration catalog, package exit
  evidence, end-to-end acceptance scenarios, failure-boundary triage,
  interface dictionary, code ownership lookup, and the final non-negotiable
  design contract.
- This is documentation-only. It makes the Chinese document independently
  usable for future time/wake implementation review, but implementation
  completion remains blocked on the remaining board/firmware RTC witness rows
  and broader QEMU/real-board SMP stress evidence.

2026-07-07 board RTC profile witnesses:

- Added a focused m1dock mock no-RTC witness in
  `boards/tx-hal-riscv64-m1dock-mock/src/lib.rs`. The mock board now
  explicitly overrides `PersistentClockIf::acknowledge_wake_alarm_irq` to
  return `PersistentClockError::Unsupported`, matching the other absent
  persistent-clock operations, and tests prove read, set-time, set-alarm,
  clear-alarm, ack, and `IrqIf::RTC_IRQ == 0`.
- Re-ran existing focused QEMU RTC board witnesses:
  RV64 `google,goldfish-rtc` read/write/alarm/clear/ack tests and LA64 LS7A
  TOY read/write/alarm/clear tests all passed. Board crate `cargo check`,
  format checks, and `cargo xtask lint invariants time-wake-retired` also
  passed.
- This closes the no-RTC typed-unsupported host evidence row for the current
  m1dock mock profile. It does not close the real-board or firmware-backed RTC
  witness row; that remains required before implementation completion can be
  claimed.

2026-07-07 broad owner-aware producer stress:

- Added
  `broad_owner_aware_producer_stress_routes_remote_wakes` in
  `crates/tx-reactor/tests/reactor_smoke.rs`. The test parks independent tasks
  and wakes them from hart 1 while their owner is hart 0 through six producer
  classes: direct mailbox source event, signal delivery, wait-channel
  publication, delegate timeout, device wait-source timer callback, and device
  RawQueue timer callback.
- Each row must report exactly one owner-aware placement and one remote
  reschedule IPI, then the target hart must re-poll and complete that task.
  This broadens host evidence beyond the existing sequential
  wait-source/timer/delegate mixed-producer witness, while still preserving the
  stricter same-task sequential test.
- Verification passed:
  `cargo test -p tx-reactor --test reactor_smoke broad_owner_aware_producer_stress_routes_remote_wakes -- --nocapture --test-threads=1`;
  `cargo test -p tx-reactor --test reactor_smoke owner_aware -- --nocapture --test-threads=1`;
  `cargo test -p tx-reactor --test reactor_smoke mixed_producer_wakes_repeatedly_route_current_owner -- --nocapture --test-threads=1`;
  `cargo check -p tx-reactor -q`; and
  `cargo xtask lint invariants time-wake-retired`.
- QEMU stress evidence remains open. `cargo xtask build --target rv64-qemu`
  passed. `cargo xtask qemu --target rv64-qemu --profile smoke
  --expect-sentinel --timeout-ms 60000 --smp 4` reached SMP and reactor AP
  markers (`:smp:aps:online`, `:smp:shootdown:ok`, `:smp:ipi:ok`,
  `:reactor:dispatch:ipi:ok`, `:reactor:ap-loop:ok`,
  `:reactor:ap-runqueue:ok`) but trapped before the boot sentinel because the
  smoke profile tried `/init` and hit `PathNotFound`. `cargo xtask test
  busybox-boot --target rv64-qemu --timeout-ms 60000` built the busybox cpio
  and reached the same SMP/reactor AP markers, but also trapped before the
  boot sentinel because `/bin/busybox` lookup returned `PathNotFound` in the
  current dirty tree. `cargo xtask build --target la64-qemu` was blocked by a
  missing local `tools/images/vendor/busybox-loongarch64-musl` include.
 Therefore these QEMU attempts are useful partial boot/SMP evidence, not a
  passing QEMU SMP stress witness.

2026-07-07 rv64 QEMU owner-aware mixed-producer SMP witness:

- Added a boot-time owner-aware wake smoke in `crates/tx-kernel/src/init.rs`.
  The BSP submits an AP-owned reactor task, waits for the AP to park it, then
  wakes the same task from the non-owner hart through three producer classes:
  wait-source publication, timer expiry, and delegate reply. Each stage must
  route through the owner-aware reactor post path, report a remote reschedule
  IPI, and advance only after the AP re-polls and records the matching stage.
- `xtask qemu` now supports `--expect-marker`, and `cargo xtask test smoke`
  / `cargo xtask test busybox-boot` pass
  `txkernel:qemu-riscv64-virt:reactor:owner-wake:smp:ok` as a required marker
  in addition to the boot sentinel. This turns the QEMU SMP wake evidence into
  a lane-level regression check rather than a manual serial grep.
- Verification passed:
  `cargo test -p xtask extra_expected_marker_is_required_with_boot_sentinel -- --nocapture`;
  `cargo test -p xtask qemu_smoke_command_captures_serial_without_block_image -- --nocapture`;
  `cargo xtask test smoke --target rv64-qemu --timeout-ms 60000`;
  `cargo xtask test busybox-boot --target rv64-qemu --timeout-ms 60000`;
  `cargo test -p tx-reactor --test reactor_smoke broad_owner_aware_producer_stress_routes_remote_wakes -- --nocapture --test-threads=1`;
  `cargo check -p tx-kernel -q && cargo check -p xtask -q`;
  `cargo fmt --check -p xtask -p tx-kernel`;
  `cargo xtask lint invariants time-wake-retired`; and
  `cargo xtask lint invariants boot-setup`.
- This closes the previous QEMU mixed-producer SMP witness gap for RV64 smoke
  and busybox boot. Implementation completion still needs a real-board or
  firmware-backed RTC witness beyond QEMU/no-RTC profiles.

2026-07-07 complete design evidence sync:

- Synchronized the reader-facing complete design documents with the current
  evidence boundary. `TX_TIME_WAKE_DESIGN_CN.md`, `TX_TIME_WAKE_DESIGN.md`,
  and `TIME_WAKE_v1.md` now say that RV64 QEMU `smoke` and `busybox-boot`
  require `:reactor:owner-wake:smp:ok` in addition to `:boot:ok`; LA64 or
  real-board SMP stress is extension evidence rather than the remaining
  blocker for the current RV64 lane.
- The remaining implementation-completion evidence row is now stated more
  narrowly: obtain a real-board or firmware-backed RTC witness beyond the
  QEMU goldfish / LA64 LS7A / m1dock no-RTC host profiles, or record an
  explicit external hardware blocker. This documentation sync does not change
  the retired-interface gate or reopen any old time/wake route.

2026-07-07 RTC witness audit and external blocker:

- Audited the current workspace for a real-board or firmware-backed RTC
  runner. `xtask/src/target.rs` exposes only `rv64-qemu`,
  `rv64-m1dock-mock`, and `la64-qemu`; `xtask/src/qemu.rs` runs the m1dock
  profile under QEMU `virt`; and `docs/DEVELOPMENT.md` describes
  `rv64-m1dock-mock` as a QEMU runner used before real SPI MMIO or
  hardware-in-loop exists. A focused search did not find an `xtask`
  flash/JTAG/probe-rs/hardware RTC command or a firmware-backed RTC runner.
- Refreshed all local RTC evidence that can be produced without external
  hardware:
  `cargo test -p tx-hal-riscv64-qemu-virt rtc -- --nocapture`;
  `cargo test -p tx-hal-riscv64-qemu-virt goldfish_persistent_clock -- --nocapture`;
  `cargo test -p tx-hal-loongarch64-qemu-virt rtc -- --nocapture`;
  `cargo test -p tx-hal-loongarch64-qemu-virt ls7a_persistent_clock -- --nocapture`;
  `cargo test -p tx-hal-riscv64-m1dock-mock persistent_clock_absent_returns_typed_unsupported -- --nocapture`;
  `cargo test -p tx-hal-riscv64-m1dock-mock rtc_irq_absent_uses_zero_sentinel -- --nocapture`;
  `cargo test -p tx-fs devfs_rtc_event -- --nocapture`;
  `cargo test -p tx-shims rtc -- --nocapture`; and
  `cargo test -p tx-kernel rtc_irq_handler_publishes_alarm_event_to_devfs_rtc_state -- --nocapture`.
  All passed. Existing unrelated warnings observed: `step_connect.rs` unused
  `guard`, `tx_ext4_bridge.rs` unused `Vec`, and tx-kernel test-only dead-code
  warnings.
- Standing gates also passed:
  `cargo xtask lint invariants time-wake-retired` reported `0` retired sites;
  `cargo xtask progress validate` passed; `cargo xtask lint docs` passed with
  the existing six retired-term warnings; and scoped `git diff --check` over
  the touched time/wake docs and progress files passed.
- External blocker recorded: the current checkout has no executable
  real-board or firmware-backed RTC witness path. Producing that final
  Package H witness requires either hardware access plus a runner, or a new
  firmware-backed RTC platform backend. This blocker is outside the old
  interface-retirement refactor itself; the active retired-interface gate is
  green.

2026-07-07 retired-interface coverage audit:

- Re-read `xtask/src/lint_invariants_time_wake.rs` against the design retired
  interface matrix. The gate covers old broad time/timer names, old
  `TimerWheel::fire_due` direct route patterns, SysV sem direct
  `notify_changed`, signalfd/process exit-source direct wrappers, signal and
  itimer direct helpers, socket/network readiness direct verbs, network
  delegate direct kicks, AIO/io_uring direct completion helpers, RTC direct
  `publish_rtc_event`, raw RTC event queue/fire access, generic v3
  wait-source direct adapters, page-backed direct notify wrappers, and
  `tx-scripts` concrete `TimerWheel` adapter imports.
- Ran global active-Rust audits over `crates` and `boards` for direct
  definitions/calls of the retired names. The audit found no callable old
  interface or old definition. Remaining name hits are `_with_post`
  replacements, documentation comments, or test-local helper names such as
  `post_signal_for_test`, none of which are active old public interfaces.
- This coverage audit supports the current completion boundary: all named old
  time/wake active interfaces are retired mechanically; the only remaining
  unclosed design row is the external real-board or firmware-backed RTC
  witness.

2026-07-07 old-name residue cleanup:

- Removed non-interface old-name residue from active Rust sources so manual
  grep now matches the stricter reading of the retirement goal. Test helpers
  named `post_signal_for_test` and `notify_process_signal_direct_for_test`
  were renamed to direct-post descriptions. Comments and assertion text that
  described active paths through retired helper names were rewritten to refer
  to catchable-signal posting or `_with_post` wrapper routes instead.
- A strict active-Rust grep over `crates` and `boards` for
  `post_signal`, `notify_v3_source`, `publish_rtc_event`, `push_completion`,
  `push_cqe`, `fire_recv`, `fire_send`, `fire_accept`,
  `post_signal_for_test`, and `notify_process_signal_direct_for_test`
  returned no hits. This is stricter than the xtask gate, which already
  ignored comments and `_with_post` replacements.
- Verification passed:
  `cargo test -p tx-subsystems --lib signal::tests::delivery -- --nocapture`;
  `cargo test -p tx-subsystems --test v3_signal_mailbox -- --nocapture`;
  `cargo test -p tx-subsystems --test v3_signalfd -- --nocapture`;
  `cargo test -p tx-scripts --test drive signal_hint -- --nocapture`;
  `cargo test -p tx-scripts --test drive drive_libc_sigcancel_hint_interrupts_even_with_sa_restart -- --nocapture`;
  `cargo test -p tx-substrate --test wake_verbs -- --nocapture`;
  `cargo test -p tx-shims --lib dispatch_kill_self_with_sigterm_succeeds -- --nocapture`;
  `cargo test -p tx-subsystems --test v3_signal_interrupt_wake -- --nocapture`;
  `cargo fmt --check -p tx-substrate -p tx-subsystems -p tx-shims -p tx-scripts`;
  `cargo xtask lint invariants time-wake-retired`; and
  `cargo xtask progress validate`.

2026-07-07 old-name residue gate:

- Strengthened `cargo xtask lint invariants time-wake-retired` with a second
  raw-line scan over active Rust under `crates` and `boards`. The original
  group scans still strip comments and focus on callable old routes; the new
  residue scan rejects high-risk retired direct names even in comments,
  strings, and test helper names. This turns the manual grep from the previous
  cleanup into a standing gate.
- The residue scan intentionally stays out of `docs/` and `xtask/` so design
  documents can continue to discuss retired terms and the linter can contain
  its own pattern names. It also uses exact identifier matching, so intended
  `_with_post` replacements such as `post_signal_with_post` and
  `notify_v3_source_with_post` are allowed.
- Verification passed:
  `cargo fmt --check -p xtask`;
  `cargo test -p xtask lint_invariants_time_wake -- --nocapture`;
  strict active-Rust grep over `crates` and `boards`; and
  `cargo xtask lint invariants time-wake-retired`, which stayed at zero
  retired sites.

2026-07-07 full retired-name residue gate:

- Expanded the active-Rust residue scan from the initial high-risk direct-name
  subset to the full retired-name matrix used by the design: core
  `TimeIf`/`TimerQueue`/`DeadlineFuture` and timer helper names,
  signal/itimer direct wrapper names, signalfd/process direct wrapper names,
  socket/network readiness verbs, network delegate direct kicks,
  AIO/io_uring direct completion helpers, RTC direct/raw queue names, generic
  wait-source/page-backed direct notify names, and the old test helper names.
- A strict grep over `crates` and `boards` for that full matrix returned no
  hits. `cargo xtask lint invariants time-wake-retired` now enforces that
  result as part of the standing gate while still allowing docs and the linter
  implementation to discuss retired terms.
- Verification passed:
  `cargo fmt --check -p xtask`;
  `cargo test -p xtask lint_invariants_time_wake -- --nocapture`;
  full retired-name grep over `crates` and `boards`; and
  `cargo xtask lint invariants time-wake-retired`.

2026-07-07 complete Chinese design spec expansion:

- Expanded `docs/stage2-documents/time_infra/TX_TIME_WAKE_DESIGN_CN.md` from a
  full architecture guide into a more implementation-ready design spec. The
  added sections cover requirement-to-owner traceability, cross-module
  invariants, timer/wait/timerfd/RTC/task wake state machines, interface
  stability and retirement policy, error and unsupported semantics, lock order
  and linearization points, end-to-end test coverage, and a concrete
  implementation checklist.
- The expansion does not change the active Rust interface state: the
  retired-interface evidence remains governed by `cargo xtask lint invariants
  time-wake-retired`, and the remaining non-documentation blocker remains the
  external Package H real-board or firmware-backed RTC witness.
- Verification for this documentation slice is tracked through
  `git diff --check -- docs/stage2-documents/time_infra/TX_TIME_WAKE_DESIGN_CN.md docs/progress/STATUS.md docs/progress/research/2026-07-06-time-wake-design-refactor.md`,
  `cargo xtask progress validate`, and `cargo xtask lint docs`.

2026-07-07 eventfd direct read/write wrapper retirement:

- Retired the eventfd no-context direct wrappers `step_eventfd_read` and
  `step_eventfd_write` from active Rust. Eventfd production syscalls were
  already using `step_eventfd_read_with_post` /
  `step_eventfd_write_with_post` with `SyscallCtx::post_mailbox_ref_event`;
  the remaining subsystem tests and StepOp wrappers now pass an explicit
  direct post closure through the same `_with_post` helpers.
- Expanded `cargo xtask lint invariants time-wake-retired` with an eventfd
  direct read/write wrapper group plus active-Rust old-name residue patterns,
  while keeping `_with_post` suffixes legal.
- Synchronized `TIME_WAKE_v1.md`, `TX_TIME_WAKE_DESIGN_CN.md`, and
  `TX_TIME_WAKE_DESIGN.md` so eventfd now matches the stricter target state:
  no public direct wrapper, explicit no-context closure only.
- Verification passed:
  strict grep over `crates` and `boards` for `step_eventfd_read` /
  `step_eventfd_write`;
  `cargo test -p tx-subsystems eventfd -- --nocapture`;
  `cargo test -p tx-shims eventfd -- --nocapture`;
  `cargo test -p xtask lint_invariants_time_wake -- --nocapture`;
  `cargo fmt --check -p tx-subsystems -p xtask`;
  `cargo xtask lint invariants time-wake-retired`; and scoped
  `git diff --check`.

2026-07-07 VFS/RNode direct readiness wrapper retirement:

- Retired the VFS/RNode no-context direct readiness wrappers
  `fire_read_wait` and `fire_write_wait` from active Rust. The RNode readiness
  surface now exposes only `fire_read_wait_with_post` /
  `fire_write_wait_with_post`, and no-context tests pass an explicit direct
  mailbox-ref post closure through those helpers.
- Expanded `cargo xtask lint invariants time-wake-retired` with a VFS/RNode
  direct readiness wrapper group plus active-Rust old-name residue patterns.
  Active Rust comments and assertions in `v3_vfs_waitsource.rs` were rewritten
  to describe read/write-wait publication rather than retired wrapper names, so
  strict grep matches the linter's zero-residue model.
- Synchronized `TIME_WAKE_v1.md`, `TX_TIME_WAKE_DESIGN_CN.md`, and
  `TX_TIME_WAKE_DESIGN.md` so VFS/RNode readiness matches the stricter target
  state: explicit `_with_post` only, with no public direct wrappers.
- Verification passed:
  strict grep over `crates` and `boards` for `fire_read_wait` /
  `fire_write_wait`;
  `cargo test -p tx-subsystems --test v3_vfs_waitsource -- --nocapture`;
  `cargo test -p xtask lint_invariants_time_wake -- --nocapture`;
  `cargo fmt --check -p tx-subsystems -p xtask`;
  `cargo xtask lint invariants time-wake-retired`; and scoped
  `git diff --check`.

2026-07-07 POSIX mq direct send/receive wrapper retirement:

- Retired the POSIX mq no-context direct wrappers `step_mq_send` and
  `step_mq_receive` from active Rust. The syscall paths were already using
  `step_mq_send_with_post` / `step_mq_receive_with_post` with
  `SyscallCtx::post_mailbox_ref_event`; the remaining procfs fdinfo setup now
  passes an explicit direct mailbox post closure through
  `step_mq_send_with_post`.
- Expanded `cargo xtask lint invariants time-wake-retired` with a POSIX mq
  direct send/receive wrapper group plus active-Rust old-name residue patterns.
  `_with_post` suffixes remain legal.
- Synchronized `TIME_WAKE_v1.md`, `TX_TIME_WAKE_DESIGN_CN.md`, and
  `TX_TIME_WAKE_DESIGN.md` so POSIX mq now matches the stricter target state:
  explicit `_with_post` only, no public direct wrappers.
- Verification passed:
  strict grep over `crates` and `boards` for `step_mq_send` /
  `step_mq_receive`;
  `cargo test -p tx-subsystems --lib posix_mq -- --nocapture`;
  `cargo test -p tx-fs procfs_fdinfo_renders_posix_mq_attributes --
  --nocapture`;
  `cargo test -p tx-shims --lib mq -- --nocapture`;
  `cargo test -p xtask lint_invariants_time_wake -- --nocapture`;
  `cargo fmt --check -p tx-subsystems -p tx-fs -p xtask`;
  `cargo xtask lint invariants time-wake-retired`; and scoped
  `git diff --check`.

2026-07-08 Chinese complete design finalization:

- Completed the reader-facing Chinese design handoff without changing active
  Rust code. `TX_TIME_WAKE_DESIGN_CN.md` now adds the final interface
  blueprint for HAL traits, `TimekeeperIf`, timer registry/router, wait-source
  caller-posting, RTC device/devfs ops, `SyscallCtx`, and worker injected-post
  seams.
- Added the reusable VFS-to-HAL layering template for later device/VFS/HAL
  refactors: hardware capability -> typed subsystem ops -> semantic state ->
  devfs/RNode projection -> wait-source publication -> owner-aware scheduler
  placement.
- Added the final review posture: the document is architecture-complete and
  implementation-ready, but implementation completion still depends on
  retired-interface gates, focused producer tests, QEMU/board witnesses, and
  progress closeout evidence.
- Verification passed: `cargo xtask progress validate`; `cargo xtask lint
  docs` with the existing retired-term stale-vocabulary warnings; and scoped
  `git diff --check` for the changed docs/progress files.

2026-07-08 pipe/userfaultfd direct wrapper retirement:

- Retired the remaining pipe direct read/write wake wrappers from active Rust.
  The pipe module no longer exposes public `step_read` / `step_write` wrappers
  or the old `ReadOp` / `WriteOp` StepOp wrappers. VFS and tests now call
  `step_read_with_post` / `step_write_with_post` with an explicit direct
  closure when no scheduler context exists. Syscall pipe read/write StepOps now
  require a concrete hint-aware post function; `tx-shims` chooses the injected
  `SyscallCtx::mailbox_ref_post_with_hint` when present, otherwise an explicit
  local direct post function at the syscall construction site.
- Retired the remaining userfaultfd direct pending-fault wrappers from active
  Rust. `UserfaultFd::push_fault_msg` and
  `AddressSpace::fault_script_for_process` are gone; all pending-fault readable
  publication enters through `push_fault_msg_with_post` and
  `fault_script_for_process_with_post`. `ProcessUfdDispatch` now has only
  `new_with_post`, and `UfdDispatchTarget` carries a required post function
  instead of `fault_post: Option`, so the direct fallback cannot hide in the VM
  dispatcher.
- Extended `cargo xtask lint invariants time-wake-retired` with scoped pipe
  and userfaultfd groups. Pipe definitions are checked under the pipe/test
  roots, while VFS/shim callsites only reject `crate::pipe::step_read(` /
  `crate::pipe::step_write(` to avoid false positives against `OpenFile` and
  TTY `step_read` / `step_write`. Userfaultfd checks reject the old direct
  wrappers, `ProcessUfdDispatch::new(`, and the retired `fault_post:
  None/Some` shape. Linter unit tests pin that `_with_post` replacements do
  not match.
- Synchronized `TIME_WAKE_v1.md`, `TX_TIME_WAKE_DESIGN.md`, and
  `TX_TIME_WAKE_DESIGN_CN.md` so pipe and userfaultfd rows now state the
  stricter target: old public direct wrappers are retired, and no-context
  callers must pass explicit direct closures through the same `_with_post`
  entrypoints.
- Verification passed:
  strict active Rust greps for pipe old names and userfaultfd old names returned
  no hits;
  `cargo test -p tx-subsystems --test v3_pipe_waitsource -- --nocapture`;
  `cargo test -p tx-subsystems --test v3_userfaultfd_e2e -- --nocapture`;
  `cargo test -p tx-shims --lib
  dispatch_epoll_pwait_reports_userfaultfd_pending_fault_readable --
  --nocapture`;
  `cargo test -p tx-shims --test v3_userfaultfd_ioctl_reply -- --nocapture`;
  `cargo test -p xtask lint_invariants_time_wake -- --nocapture`;
  `cargo fmt --check -p tx-subsystems -p tx-shims -p xtask`;
  and `cargo xtask lint invariants time-wake-retired`.
- Known unrelated residual: a broader `cargo test -p tx-subsystems --lib pipe
  -- --nocapture` run still fails
  `pipe_user_gift_read_copies_bytes_and_releases_gift` with the existing gift
  accounting mismatch. The focused `v3_pipe_waitsource` target that covers this
  wake-interface slice passed.

2026-07-08 complete design gate sync:

- Synchronized the active design contract and both stage2 handoff documents
  after the final pipe/userfaultfd retirement. `TIME_WAKE_v1.md`,
  `TX_TIME_WAKE_DESIGN.md`, and `TX_TIME_WAKE_DESIGN_CN.md` now all describe
  the same target state: old reactor-local `complete` / `arrive` / `ack`,
  pipe `step_read` / `step_write` plus `ReadOp` / `WriteOp`, and userfaultfd
  `push_fault_msg` / `fault_script_for_process` direct wrappers are retired;
  no-context callers must pass explicit direct closures through `_with_post`
  seams instead of relying on reusable public direct wrappers.
- Filled the missing English stage2 appendix coverage. Appendix A.4 now lists
  the pipe and userfaultfd scoped grep tripwires next to the canonical
  `cargo xtask lint invariants time-wake-retired` gate, matching the earlier
  active design gate and Chinese command block.
- Re-audited the Package G producer catalog against
  `xtask/src/lint_invariants_time_wake.rs`. Every row that says an old direct
  wrapper is retired has a corresponding linter group or active-Rust old-name
  residue check. Rows for futex, delegate, and RTC intentionally do not claim
  that every direct/no-reactor path is gone: futex keeps exact wait-source
  publication through an explicit post function, delegate no-context paths are
  still explicit helper/test seams, and RTC pending device state remains the
  semantic source until waiter drain.
- Verification for this sync slice: contradiction grep over the three design
  docs found no live "old fallback remains" wording beyond explicit retired
  descriptions and grep commands; scoped active-Rust pipe and userfaultfd
  greps returned no hits; `cargo xtask lint invariants time-wake-retired`
  passed with `0` retired sites; `cargo xtask lint docs` passed with the
  existing seven stale-vocabulary warnings about retired terms; `cargo xtask
  progress validate` passed; and scoped `git diff --check` passed for the
  changed time/wake design and progress files.

2026-07-08 completion audit and focused witness repair:

- Re-ran the completion audit from the Package A-H exit criteria and Package G
  producer catalog instead of relying only on the final pipe/userfaultfd doc
  sync. The explicit retired-name requirement is mechanically covered by
  `cargo xtask lint invariants time-wake-retired`, and a strict active-Rust grep
  for core old names (`TimeIf`, `TimerQueue`, `DeadlineFuture`,
  `timer_sleep`, `install_timer_queue`, `sleep_until_ns`,
  `DirectMailboxTimerWakeRouter`, router-free `.fire_due(`, `timer_queue`,
  `fixed_oscomp_time`, and `binding.name == "rtc"`) returned no active
  `crates`/`boards` hits.
- The audit exposed two host focused test failures caused by parallel test
  shared state. `wall_clock` tests reset the shared `TEST_NS` monotonic
  counter while sibling tests were running, so
  `realtime_is_monotonic_plus_offset_and_set_bumps_generation` could observe a
  20s/30s counter and fail its local `< 6s` assertion. `v3_timer_surface`
  device-callback tests reset and incremented the shared `DEVICE_TIMER_FIRES`
  counter concurrently, so
  `dropping_device_callback_guard_cancels_before_fire` could observe another
  test's callback payload. Both groups now guard their test-only shared
  globals with local mutexes; production timekeeper/timer code was not changed
  for this repair.
- Verification passed after the repair:
  `cargo test -p tx-subsystems --lib wall_clock::tests:: -- --nocapture`;
  `cargo test -p tx-reactor --test v3_timer_surface -- --nocapture`;
  `cargo fmt --check -p tx-subsystems -p tx-reactor`;
  `cargo xtask lint invariants time-wake-retired`; and
  `cargo test -p xtask lint_invariants_time_wake -- --nocapture`.
- Additional local Package A-H evidence gathered in this audit:
  `cargo test -p tx-reactor --test reactor_smoke mixed_producer_wakes_repeatedly_route_current_owner -- --nocapture`;
  `cargo test -p tx-reactor --test reactor_smoke broad_owner_aware_producer_stress_routes_remote_wakes -- --nocapture --test-threads=1`;
  `cargo test -p tx-shims --lib time_syscalls -- --nocapture`;
  `cargo test -p tx-shims --lib timerfd_dispatch -- --nocapture`;
  `cargo test -p tx-shims --lib ioctl_dispatch -- --nocapture`;
  `cargo test -p tx-fs devfs_rtc -- --nocapture`;
  `cargo test -p tx-hal-riscv64-qemu-virt goldfish -- --nocapture`;
  `cargo test -p tx-hal-loongarch64-qemu-virt ls7a -- --nocapture`;
  `cargo test -p tx-hal-riscv64-m1dock-mock persistent_clock_absent -- --nocapture`;
  `cargo test -p tx-hal-riscv64-m1dock-mock rtc_irq_absent -- --nocapture`;
  `cargo test -p tx-kernel rtc_irq -- --nocapture`;
  `cargo test -p tx-shims --lib dispatch_utimensat_updates_tmpfs_inode_timestamps -- --nocapture`;
  and `cargo test -p tx-shims --lib stat_family -- --nocapture`.
- The audit result is stronger but still not a full goal-completion proof for
  the original objective because Package H explicitly asks for real-board or
  firmware-backed RTC evidence. The current workspace has QEMU/host board
  evidence and no-RTC typed unsupported evidence, but no hardware-in-loop or
  firmware RTC runner.

2026-07-08 rv64 QEMU owner-wake witness refresh:

- Refreshed the local RV64 QEMU marker evidence after the completion audit with
  the exact design-document commands:
  `cargo xtask test smoke --target rv64-qemu --timeout-ms 60000` and
  `cargo xtask test busybox-boot --target rv64-qemu --timeout-ms 60000`.
- Both runs rebuilt the RV64 QEMU kernel and image lane, launched QEMU with
  `-smp 4`, and passed the sentinel assertion. The observed markers were
  `txkernel:qemu-riscv64-virt:boot:ok` plus
  `txkernel:qemu-riscv64-virt:reactor:owner-wake:smp:ok` for both the smoke
  initramfs lane and the busybox initramfs lane.
- This refresh closes the local RV64 QEMU owner-wake marker witness for the
  current tree. It does not close the external Package H blocker: a
  real-board or firmware-backed RTC witness beyond QEMU goldfish, LA64 LS7A,
  and m1dock no-RTC profiles is still missing.

2026-07-08 retired gate process-exit coverage:

- Audited the Package G process-exit/signalfd row against
  `xtask/src/lint_invariants_time_wake.rs` and found one mechanical coverage
  gap: the design already names `notify_child_zombified_with_post` as the
  active child-zombie wake route, but the retired-name gate only rejected
  `fire_exit_source` and did not reject a regression to the old
  `notify_child_zombified` name.
- Tightened the linter by adding `notify_child_zombified` to both the scoped
  signalfd/process direct-wrapper group and the global active-Rust old-name
  residue scan. The linter unit tests now prove the old name matches while
  `notify_child_zombified_with_post` does not.
- Synchronized `TIME_WAKE_v1.md`, `TX_TIME_WAKE_DESIGN.md`, and
  `TX_TIME_WAKE_DESIGN_CN.md` so their human-readable grep tripwires include
  `notify_child_zombified` next to `fire_exit_source`.
- Verification passed:
  strict `rg -n '\bnotify_child_zombified\b' crates boards --glob '*.rs'`
  returned no hits;
  `cargo test -p xtask lint_invariants_time_wake -- --nocapture`; and
  `cargo xtask lint invariants time-wake-retired` with `0` retired sites.

2026-07-09 complete Chinese design handoff:

- Updated
  [`TX_TIME_WAKE_DESIGN_CN.md`](../../stage2-documents/time_infra/TX_TIME_WAKE_DESIGN_CN.md)
  as the reader-facing complete design document for the current time/wake
  architecture. The added front-matter summary ties four review lines together:
  Linux-visible semantics, Tx owner rows, interface/state contracts, and
  migration/proof gates.
- The document now states at the entry point that every future time/wake
  feature or bug must identify its Linux/POSIX semantic, semantic owner, lower
  interface, upper caller, wake route, no-context fallback, and retired-name
  audit before implementation. This makes the later owner tables, state
  machines, Package A-H plan, `_with_post` producer catalog, VFS-to-HAL layering
  template, and Appendix A regression tripwires usable as one complete handoff.
- This was a documentation closeout only. It does not close implementation
  evidence gaps, especially the external Package H real-board or
  firmware-backed RTC witness beyond the local QEMU/no-RTC profiles.

2026-07-09 route_gewalt direct wrapper retirement:

- Retired the remaining bare `route_gewalt` old-name surface from active Rust
  after auditing the signal Package G row. The callable Gewalt process-control
  helper is now `route_gewalt_with_post`; no-context tests pass an explicit
  direct weak-mailbox post closure through that helper.
- Removed active Rust comments, test names, and assertion messages that still
  used the retired bare name, so the strict old-name residue scan can treat
  `route_gewalt` as zero-tolerance without false positives.
- Tightened `xtask/src/lint_invariants_time_wake.rs` by adding
  `route_gewalt` to the signal helper retired-pattern group and the global
  active-Rust old-name residue group. The linter unit tests now prove the old
  name matches while `route_gewalt_with_post` does not.
- Synchronized `TIME_WAKE_v1.md`, `TX_TIME_WAKE_DESIGN.md`, and
  `TX_TIME_WAKE_DESIGN_CN.md` so their human-readable tripwires name
  `route_gewalt` alongside the other retired signal direct wrappers.
- Verification passed:
  strict `rg -n '\broute_gewalt\b' crates boards --glob '*.rs'`;
  `cargo test -p tx-subsystems route_gewalt_with_post -- --nocapture`;
  `cargo test -p tx-subsystems --test v3_signal_mailbox -- --nocapture`;
  `cargo fmt --check -p tx-subsystems -p tx-kernel -p tx-substrate -p xtask`;
  `cargo test -p xtask lint_invariants_time_wake -- --nocapture`;
  `cargo xtask lint invariants time-wake-retired`;
  `cargo xtask lint docs`; and
  `cargo xtask progress validate`.
- This improves the Package G retired-interface proof. It does not close the
  external Package H real-board or firmware-backed RTC witness gap.

2026-07-09 wall_clock raw wrapper contract sync:

- Synchronized the active contract
  [`TIME_WAKE_v1.md`](../../design/02_execution/TIME_WAKE_v1.md), the English
  stage2 handoff
  [`TX_TIME_WAKE_DESIGN.md`](../../stage2-documents/time_infra/TX_TIME_WAKE_DESIGN.md),
  and the Chinese complete-design handoff
  [`TX_TIME_WAKE_DESIGN_CN.md`](../../stage2-documents/time_infra/TX_TIME_WAKE_DESIGN_CN.md)
  with the current `wall_clock` facade shape.
- The documented target is now strict: `TimekeeperIf` / `timekeeper()` is the
  only public semantic clock facade for upper layers. Public raw
  `wall_clock::*` runtime free functions and a public `WallClock` storage type
  are retired active interfaces; private same-module helpers and cfg-test reset
  hooks are not public runtime API.
- The Package B evidence row now requires clock syscalls, VFS timestamp paths,
  VVAR publication, timerfd realtime revalidation, and realtime mutation to use
  `TimekeeperIf`, with no public raw wrapper surface left behind. The stage2
  regression tripwires include the strict wall-clock raw public wrapper grep,
  and the Chinese requirements matrix adds the anti-fork facade proof row.
- This is a contract/documentation sync for a code shape already represented
  by `xtask/src/lint_invariants_time_wake.rs` through the
  `wall-clock raw public compatibility wrappers` audit group. It does not
  close the external Package H real-board or firmware-backed RTC witness gap.
- Verification passed:
  strict `rg -n 'pub struct WallClock|pub fn (monotonic_now_ns|realtime_now_ns|set_realtime_ns|seed_realtime_ns|seed_realtime_from_persistent|generation|realtime_offset_ns|set_clock_params|monotonic_deadline_from_realtime_ns|snapshot_for_vvar|publish_vvar)' crates/tx-subsystems/src/wall_clock.rs`;
  `cargo test -p xtask lint_invariants_time_wake -- --nocapture`;
  `cargo xtask lint invariants time-wake-retired`;
  `cargo xtask lint docs`;
  `cargo xtask progress validate`; and scoped `git diff --check`.

2026-07-09 step_kill_process direct wrapper retirement:

- Retired the bare `step_kill_process` active-Rust old-name surface after the
  Package G signal process-producer audit found it was not yet covered by the
  linter. The public process-directed signal producer surface is now the
  caller-posting `step_kill_process_with_post` /
  `step_kill_process_with_posts` pair.
- Updated remaining no-reactor signal tests to preserve the same direct-post
  behavior through explicit closures rather than importing a parallel direct
  wrapper. This covered the interrupt-wake, thread-eligibility, signalfd, and
  mailbox integration surfaces.
- Removed active Rust comments, doc comments, panic messages, and test text
  that still named the bare helper, so the global old-name residue scan can be
  zero-tolerance across both code and comments.
- Tightened `xtask/src/lint_invariants_time_wake.rs` by adding
  `step_kill_process` to the signal helper retired-pattern group and the
  global active-Rust old-name residue group. The linter unit tests now prove
  the old name matches while `step_kill_process_with_post` and
  `step_kill_process_with_posts` do not.
- Synchronized `TIME_WAKE_v1.md`, `TX_TIME_WAKE_DESIGN.md`, and
  `TX_TIME_WAKE_DESIGN_CN.md` so their human-readable retired-interface
  lists and grep tripwires name `step_kill_process` alongside the other
  retired signal direct wrappers.
- Verification passed:
  strict `rg -n '\bstep_kill_process\b' crates boards --glob '*.rs'`;
  `cargo fmt --check -p tx-subsystems -p tx-shims -p tx-substrate -p xtask`;
  `cargo check -p tx-subsystems -p tx-shims -q`;
  `cargo test -p xtask lint_invariants_time_wake -- --nocapture`;
  `cargo test -p tx-subsystems --test v3_signalfd -- --nocapture`;
  `cargo test -p tx-subsystems --test v3_signal_interrupt_wake -- --nocapture`;
  `cargo test -p tx-subsystems --test v3_signal_eligibility -- --nocapture`;
  `cargo test -p tx-subsystems --test v3_signal_mailbox -- --nocapture`;
  `cargo xtask lint invariants time-wake-retired`;
  `cargo xtask lint docs`; and
  `cargo xtask progress validate`.
- This improves the Package G retired-interface proof. It does not close the
  external Package H real-board or firmware-backed RTC witness gap.

2026-07-09 DelegateRegistry direct wrapper retirement:

- Retired the public no-context delegate transition wrapper family after the
  Package G audit found `DelegateRegistry::mark_replied`, `mark_timed_out`,
  `mark_canceled`, `mark_agent_died`, and `mark_endpoint_died` were still
  callable direct surfaces outside the `_with_post` seam.
- `DelegateRegistry` now exposes the token CAS transition surface through
  `mark_replied_with_post`, `mark_timed_out_with_post`,
  `mark_canceled_with_post`, `mark_agent_died_with_post`, and
  `mark_endpoint_died_with_post`. The registry remains the linearization
  point; the caller must choose the wake publication route.
- No-context substrate/reactor tests and `AgentTokenGuard::drop` preserve the
  same direct mailbox behavior by passing explicit direct closures through the
  `_with_post` methods. UFFD ioctl reply paths now inject
  `SyscallCtx::post_mailbox_event`, so syscall contexts can route the
  resulting `AgentReplied` wake through the owner-aware post hook when present.
- `xtask/src/lint_invariants_time_wake.rs` has a new delegate-registry retired
  group that rejects old method definitions and method calls while allowing the
  `_with_post` forms.
- Verification passed:
  direct method-call grep over `crates` and `boards` has only doc-comment
  mentions; function-def grep over `agent.rs` has no hits;
  `cargo fmt --check -p tx-substrate -p tx-reactor -p tx-subsystems -p
  tx-shims -p xtask`;
  `cargo check -p tx-substrate -p tx-reactor -p tx-subsystems -p tx-shims -q`;
  `cargo test -p xtask lint_invariants_time_wake -- --nocapture`;
  `cargo xtask lint invariants time-wake-retired`;
  `cargo test -p tx-substrate --test v3_pr7b_mailbox_integration -- --nocapture`;
  `cargo test -p tx-substrate --test v3_pr7_delegate_runtime -- --nocapture`;
  `cargo test -p tx-substrate --test v3_endpoint_scope_abandonment -- --nocapture`;
  `cargo test -p tx-substrate --test v3_agent_token_guard_timer -- --nocapture`;
  `cargo test -p tx-reactor --test v3_pr7b_timer_routing -- --nocapture`;
  `cargo test -p tx-subsystems --test v3_userfaultfd_fault_path -- --nocapture`;
  `cargo test -p tx-subsystems --test v3_userfaultfd_e2e -- --nocapture`;
  and `cargo test -p tx-shims --test v3_userfaultfd_ioctl_reply -- --nocapture`.
- This improves the Package G retired-interface proof. It still does not close
  the external Package H real-board or firmware-backed RTC witness gap.

2026-07-09 DelegateRegistry old-name residue gate:

- Tightened the DelegateRegistry direct-wrapper retirement from callable API
  removal to zero active-Rust old-name residue. The global residue scan in
  `xtask/src/lint_invariants_time_wake.rs` now rejects `mark_replied`,
  `mark_timed_out`, `mark_canceled`, `mark_agent_died`, and
  `mark_endpoint_died` anywhere under `crates` and `boards`, not just method
  definitions or method calls in the scoped delegate group.
- Active Rust comments and test strings now describe the delegate
  reply/timeout/cancel/agent-death/endpoint-death transition roles instead of
  spelling the retired callable names. The `_with_post` API names remain valid
  and are explicitly tested as negative cases.
- Verification passed: strict
  `rg -n '\bmark_(replied|timed_out|canceled|agent_died|endpoint_died)\b' crates boards --glob '*.rs'; test $? -eq 1`;
  `cargo test -p xtask lint_invariants_time_wake -- --nocapture`;
  `cargo xtask lint invariants time-wake-retired`; and
  `cargo fmt --check -p xtask` after formatting.
- This improves Package G zero-residue evidence. It still does not close the
  external Package H real-board or firmware-backed RTC witness gap.

2026-07-09 SysV msgctl direct wrapper retirement:

- Audited the SysV message queue Package G row and found one remaining public
  no-context wake-producing surface: `step_msgctl` / `step_msgctl_in_ns` still
  wrapped `step_msgctl*_with_post` with direct mailbox posting, and `IPC_RMID`
  uses that path to abort sender and receiver waiters.
- Added `step_msgctl` and `step_msgctl_in_ns` to the SysV msg scoped retired
  group and the global active-Rust old-name residue group. The new gate first
  failed with 25 retired sites, proving it caught the live wrappers and test
  callsites.
- Removed the two direct wrappers from `crates/tx-subsystems/src/ipc/sysv_msg/execution.rs`.
  No-context tests now call `step_msgctl_with_post` /
  `step_msgctl_in_ns_with_post` with explicit direct mailbox-ref post closures;
  syscall `sys_msgctl` was already using the `_with_post` seam with
  `SyscallCtx::post_mailbox_ref_event`.
- Verification passed after the removal: strict grep for `step_msgctl(` /
  `step_msgctl_in_ns(` under the SysV msg and syscall/test paths returned no
  hits; `cargo xtask lint invariants time-wake-retired` reported zero retired
  sites; `cargo test -p tx-subsystems sysv_msg -- --nocapture` passed the six
  filtered SysV msg lib tests; and
  `cargo test -p xtask lint_invariants_time_wake -- --nocapture` passed.
- This improves Package G IPC retirement evidence. It still does not close the
  external Package H real-board or firmware-backed RTC witness gap.

## Next Step

Continue auditing any newly discovered old-name residue under the same
zero-tolerance `time-wake-retired` gate. For full objective completion,
continue only after hardware access, a hardware-in-loop runner, or a
firmware-backed RTC backend becomes available. Future wake producers must use
the same owner-aware scheduler boundary as timer expiry where scheduler context
exists, and any no-context path must be an explicit `_with_post` caller rather
than a public direct wrapper.

## Blockers

No documentation blocker. The named Package G wake-producer direct-interface
retirement rows are mechanically audited, including the pipe, userfaultfd, and
route_gewalt late leftovers. External evidence blocker: current workspace has
no real-board or firmware-backed RTC runner, so the final Package H board
witness cannot be produced locally.
