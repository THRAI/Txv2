# 2026-05-20 lmbench network targeted boot-suite

## Context

After the OSComp libc-test network ABI slice passed, the next network-only
target was lmbench's local networking binaries:

- `lat_udp`
- `lat_tcp`
- `lat_connect`
- `bw_tcp`

The goal was to run those binaries directly under the OSComp image without
expanding into non-network syscall work. The local lmbench source already
defines `NO_PORTMAPPER`, so this pass used fixed local ports rather than adding
rpcbind/portmapper support. The targeted suite also uses numeric `127.0.0.1`
instead of `localhost` to avoid resolver and `/etc/hosts` dependencies.

## Changes

- Added `tx.oscomp=lmbench-network`, a targeted boot-suite command that starts
  each lmbench server, runs the matching client with small iteration counts,
  checks for expected output, and then shuts the server down.
- Made the suite check output patterns instead of trusting process exit status.
  This matters because `bw_tcp` can print throughput while returning an
  unreliable status through the lmbench wrapper path.
- Added pipe readiness to `pselect6` for lmbench benchmp control pipes:
  readable readers wake on buffered bytes or EOF, and writable writers wake
  while the pipe has capacity or when EPIPE would be reported.
- Made finite-timeout socket `ppoll` park on socket wait sources and deadlines.
  The old immediate scan behavior was not enough for lmbench's polling paths.
- Restricted zone registry maintenance scans to the highest registered zone
  index. The first lmbench run reached a bogus never-registered high slot during
  registry bucket flushing.
- Added fallible live-cap cloning and used it for socket table lookups and
  snapshots so stale socket caps are skipped rather than panicking during
  loopback and poll walks.
- Added a generic direct UDP loopback delivery path. The path still uses the
  socket table's UDP ingress lookup and endpoint matching, but avoids
  serializing loopback UDP packets when the sender and receiver are both local.
- Added a narrower UDP syscall fast path for inline loopback sends: sendto can
  deliver directly through the network subsystem without waking the net
  delegate for work already consumed synchronously. Receive-side staging now
  uses queued socket IO length and skips a full poll-ready pass when receive
  bytes are already queued.
- Matched the targeted suite's timing environment to lmbench's own
  `scripts/lmbench` defaults by exporting `TIMING_O=0` and `LOOP_O=0` alongside
  `ENOUGH=1000000`. This avoids unrelated timing-overhead calibration while the
  benchmark still runs the normal UDP send/recv round-trip loop.
- Tightened TCP close/write-before-exit behavior. Socket close now flushes
  already queued TCP loopback bytes before withdrawing connection table entries
  and marking the peer broken; the peer can read those bytes before observing
  EOF.

## Findings

- `lat_udp` now prints `UDP latency using 127.0.0.1` in the targeted QEMU run.
- `lat_tcp` now prints `TCP latency using 127.0.0.1`.
- `lat_connect` now prints `TCP/IP connection cost to 127.0.0.1`.
- `bw_tcp -m 1`, `bw_tcp -m 64`, and `bw_tcp -m 1024` now print `MB/sec`
  throughput lines.
- `bw_tcp -S` no longer prints `control nbytes: No error information`; the
  one-byte control message written immediately before process exit is now
  delivered to the accepted peer before close teardown.
- Before the timing environment was aligned, `lat_udp` printed `Recv timed
  out`; trap trace showed repeated UDP `sendto -> 4` and `recvfrom -> 4`
  progress on both sides, so the failure was not the same class as the earlier
  missing UDP loopback delivery bug.
- The stale-cap fix is defensive: it prevents panics by skipping non-live caps,
  but it does not yet compact or purge stale socket-table entries.

## Verification

- `cargo fmt --check`
- `cargo test -p tx-substrate maintenance_ignores_never_registered_high_slots -- --test-threads=1`
- `cargo test -p tx-substrate static_zone_registration_is_idempotent -- --test-threads=1`
- `cargo test -p tx-shims dispatch_pselect_pipe_read_ready_after_write -- --test-threads=1`
- `cargo test -p tx-shims dispatch_udp_connect_autobinds_and_reaches_wildcard_bound_peer -- --test-threads=1`
- `cargo test -p tx-shims dispatch_ping_socket_sendto_recvfrom_loopback_echo_reply -- --test-threads=1`
- `cargo test -p tx-subsystems udp_loopback_direct_send_kernel_bytes_reaches_receiver -- --test-threads=1`
- `cargo test -p tx-subsystems udp_loopback_inline_send_can_defer_delegate_poll_kick -- --test-threads=1`
- `cargo test -p tx-subsystems udp_loopback_sendto_reaches_wildcard_bound_receiver -- --test-threads=1`
- `cargo test -p tx-subsystems udp_loopback_connected_send_reaches_bound_receiver -- --test-threads=1`
- `cargo test -p tx-subsystems tcp_socket_close_marks_connected_peer_broken -- --test-threads=1`
- `cargo test -p tx-subsystems tcp_socket_close_flushes_queued_bytes_to_peer_before_eof -- --test-threads=1`
- `cargo test -p tx-subsystems net_delegate_poll_drives_tcp_fin_to_peer_eof -- --test-threads=1`
- `cargo build -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf`
- `cargo xtask oscomp submit --target rv64-qemu`
- `timeout 240s cargo xtask oscomp qemu --target rv64-qemu --boot-suite lmbench-network`
- `cargo xtask progress validate`
- `cargo xtask lint docs`

The targeted QEMU serial result ended without a kernel trap:

- `lat_udp`: success, latency line printed
- `lat_tcp`: success, latency line printed
- `lat_connect`: success, connection-cost line printed
- `bw_tcp_1`: success, throughput line printed
- `bw_tcp_64`: success, throughput line printed
- `bw_tcp_1024`: success, throughput line printed
- `bw_tcp -S`: no control-channel error line
- userspace exit status was zero

## Next

Keep the next pass network-scoped:

- try larger `bw_tcp` message sizes to expose buffer, scheduling, and
  fairness issues;
- decide later whether `lat_rpc` and HTTP-shaped tests belong in the core
  network-stack pass or a higher-layer compatibility pass;
- later, add socket-table stale-entry cleanup so the table does not accumulate
  dead caps that snapshots must skip.
