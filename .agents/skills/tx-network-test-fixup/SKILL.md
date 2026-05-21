---
name: tx-network-test-fixup
description: Use when triaging or fixing txKernel network-stack test failures in OSComp, LTP socket syscall cases, lmbench, libctest, netperf, iperf/iperf3, busybox/alpine network probes, or any TCP/UDP/AF_INET/AF_PACKET/AF_UNIX socket behavior. Trigger on network benchmark failures, socket ABI mismatches, loopback TCP/UDP bugs, readiness/poll issues on sockets, network namespace/procfs socket issues, or questions about whether a failure is truly network-related. Enforces semantic fixes over test-specific hardcoding and requires discussing broader refactors before undertaking them.
---

# tx-network-test-fixup

Use this skill for focused network-test work: OSComp `libctest-network`,
`lmbench-network`, `netperf`, `iperf3`, LTP socket syscall cases, and shell
network probes. Keep the boundary narrow: fix network semantics, not every
unrelated userspace prerequisite discovered along the way.

## Read First

- `docs/progress/research/2026-05-21-oscomp-network-suite-survey.md`
- `docs/progress/research/2026-05-21-ltp-network-prep.md`
- `docs/progress/STATUS.md` newest network entries
- Socket syscall shim: `crates/tx-shims/src/linux_syscall/socket.rs`
- Socket I/O shim: `crates/tx-shims/src/linux_syscall/io.rs`
- Network subsystem: `crates/tx-subsystems/src/net/`
- Loopback tests: `crates/tx-subsystems/src/net/tests/loopback_tests.rs`
- OSComp targeting: `crates/tx-kernel/src/init/exec.rs`, `xtask/src/oscomp.rs`

Load `tx-ltp-syscall` as well when the witness is an LTP syscall case.

## Core Rules

- Do not hardcode a test name, executable name, argv pattern, magic payload,
  benchmark label, or one-off port to pass a test. A fixed port in a test
  harness is fine; kernel behavior must follow Linux socket semantics.
- Prefer principled Linux-compatible errors for unsupported surfaces:
  `EAFNOSUPPORT`, `EPROTONOSUPPORT`, `ENOPROTOOPT`, `EOPNOTSUPP`,
  `EADDRINUSE`, `EADDRNOTAVAIL`, `ENOTCONN`, `EPIPE`, etc.
- If a small local fix does not fit the model, pause and discuss a refactor
  plan with the user before doing it. Name the behavior gap, affected modules,
  risks, and verification plan.
- Do not expand a network task into non-network syscall work unless that
  syscall is a direct blocker for the selected network witness. Record other
  blockers instead.
- Treat tests as witnesses, not specifications. Read the test source or script
  to understand the Linux behavior being exercised, then implement the behavior.

## Triage Workflow

1. Identify the exact witness:
   - command run
   - serial log path
   - test source or shell script
   - expected pass marker
   - observed failure marker, errno, hang point, or trap
2. Classify the failure:
   - libc/ABI formatting: `inet_pton`, `inet_ntop`, DNS helpers
   - socket creation/options: `socket`, `getsockopt`, `setsockopt`, flags
   - address binding/name APIs: `bind`, `getsockname`, `getpeername`
   - UDP datagram path: autobind, delivery, truncation, flags
   - TCP path: connect/listen/accept, close/EOF, backlog, reuse
   - readiness/wait: blocking I/O, poll/ppoll/select/epoll wakeups
   - lifecycle: forked sockets, fd close accounting, SIGCHLD cleanup
   - control plane: netns, procfs, rtnetlink, AF_PACKET
   - out of scope: IPv6, SCTP, DCCP, IPsec, NFS/RPC, iptables/nft unless the
     user explicitly charters that surface
3. Build the smallest faithful reproducer:
   - prefer host unit tests for subsystem semantics
   - use slim OSComp/LTP images for selected case lists
   - use targeted boot suites before full OSComp
4. Fix the semantic layer that owns the behavior:
   - shim only for ABI decoding/copying/errno mapping
   - `tx-subsystems/src/net/` for socket/protocol state
   - process/fd code only for lifecycle/fork/close semantics
   - reactor/wait code only for readiness and wakeup semantics
5. Verify at three levels when feasible:
   - focused unit test
   - targeted QEMU witness
   - regression target already known to pass

## Recommended Witness Order

For OSComp network:

1. `libctest-network`
2. `lmbench-network`
3. `netperf`
4. `iperf3`

For LTP network bring-up, start with syscall cases rather than upstream
`runtest/net.*`:

1. `socket01`, `socket02`
2. `bind01`, `bind02`, `listen01`
3. `getsockname01`, `getpeername01`, `getsockopt01`, `setsockopt01`
4. `connect01`, `connect02`, `accept01`, `accept02`, `accept03`
5. `send01`, `send02`, `sendto01`, `recv01`, `recvfrom01`
6. `sendmsg*`, `recvmsg*`, `sendmmsg*`, `recvmmsg*`
7. poll/select/epoll socket readiness cases

Leave `runtest/net.tcp_cmds`, `net.features`, `net.sctp`, `net.ipv6`,
`net.nfs`, RPC, netfilter, and virtualization/network-driver tests for later.

## Refactor Boundary

A refactor discussion is required when the fix needs any of these:

- changing socket identity/payload ownership
- changing wait-source or reactor scheduling semantics
- changing fd-table sharing, fork inheritance, or close accounting broadly
- introducing a protocol-state abstraction shared by TCP and UDP
- adding network control-plane infrastructure such as rtnetlink or routing
- changing OSComp harness generation beyond selecting a focused test suite

The discussion should include the smallest alternate patch considered, why it
does not fit, the proposed module boundaries, and the tests that will prove the
refactor preserved existing OSComp network behavior.

## Verification Commands

Useful focused commands:

```sh
cargo fmt --check
cargo check -p tx-shims
cargo test -p tx-subsystems --lib udp_loopback_sendto_reaches_wildcard_bound_receiver -- --test-threads=1
cargo test -p tx-shims --lib dispatch_rt_sigtimedwait -- --test-threads=1
timeout 240s cargo xtask oscomp qemu --target rv64-qemu --boot-suite libctest-network
timeout 240s cargo xtask oscomp qemu --target rv64-qemu --boot-suite lmbench-network
cargo xtask oscomp slim-sdcard --suite ltp-musl --ltp-cases socket01,socket02,bind01 --output target/oscomp/ltp-net-syscalls/sdcard-rv.img --size-mb 512
```

For trap or panic logs, use:

```sh
cargo xtask fault-decode --target rv64-qemu --serial target/oscomp/os_serial_out_rv.txt
```

## Completion Checklist

- The final answer names the exact test witness that now passes.
- The fix is semantic and not test-specific.
- Existing targeted network suites still pass or any skipped verification is
  explicitly explained.
- `docs/progress/STATUS.md` records what changed, verification, next step, and
  blockers.
- `cargo xtask progress validate` passes after progress edits.
