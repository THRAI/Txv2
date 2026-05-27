# 2026-05-22 Netperf UDP_RR Signal ABI Regression

## Context

After LTP socket work, the focused OSComp `netperf-udp-rr` witness exposed two
signal/timer issues rather than a UDP data-path regression:

- musl `netperf-udp-rr` timed out after SIGALRM because the cooperative
  itimer delivery could land at a syscall boundary and the following blocking
  `recvfrom` no longer had an active timer deadline to interrupt it.
- glibc `netperf-udp-rr-glibc` trapped at user PC `0x2006` because
  `rt_sigaction` decoded the RV64 kernel action as if it contained a
  `sa_restorer` word. On Linux RV64, `asm-generic/signal.h` is included without
  `SA_RESTORER`, so the kernel-facing layout is `handler, flags, mask`. The
  third word was SIGALRM's mask bit (`0x2000`), not a return trampoline.

## Fix

- `setitimer(ITIMER_REAL)` now reports the previous live timer in `old_value`.
- delivered SIGALRM keeps a one-shot interrupt token so the next blocking socket
  wait can return `-EINTR` even when delivery happened at a cooperative syscall
  boundary.
- the compatibility signal trampoline page is marked executable when the shim
  uses the on-stack `rt_sigreturn` trampoline.
- RV64 `rt_sigaction` now uses the 24-byte kernel layout
  `sa_handler, sa_flags, sa_mask` and ignores the absent restorer field.

## Verification

- `cargo fmt --check`
- `cargo test -p tx-shims --lib dispatch_setitimer_reports_previous_real_timer_remaining -- --test-threads=1`
- `cargo test -p tx-shims --lib itimer_real_sigalrm_handler_round_trip_restores_context -- --test-threads=1`
- `cargo test -p tx-shims --lib itimer_real_sigalrm_ignores_rv64_sigaction_mask_as_restorer -- --test-threads=1`
- `cargo test -p tx-shims --lib dispatch_rt_sigaction_install_then_query_round_trip -- --test-threads=1`
- `cargo test -p tx-subsystems --lib udp_loopback_netperf_rr_ephemeral_collision_shape -- --test-threads=1`
- `cargo build -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf`
- `cargo xtask oscomp submit --target rv64-qemu`
- `timeout 45s cargo xtask oscomp qemu --target rv64-qemu --boot-suite netperf-udp-rr-glibc`
- `timeout 45s cargo xtask oscomp qemu --target rv64-qemu --boot-suite netperf-udp-rr`

## Next Step

Continue network regression in short focused batches: `libctest-network`,
`lmbench-network`, full `netperf` slices, and `iperf3` for both musl/glibc where
boot-suite selectors exist. Avoid long waits unless a focused failure needs a
bounded diagnostic run.
