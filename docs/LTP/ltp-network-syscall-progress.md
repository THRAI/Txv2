# LTP syscall network progress

Date: 2026-05-26
Branch: `feature-network`

This is the current working ledger for the 50 OSComp LTP socket/network syscall
cases from `runtest/syscalls`. These are not the upstream LTP `net.*` suites.
Do not use `LTP_BATCH=net` as the progress signal: ordinary batches filter
these prefixes, so `net 0` means "manual only", not "no network tests".

## Current Summary

Reliable focused results already in `target/oscomp`:

| Scope | Cases | Judge | Log |
| --- | --- | ---: | --- |
| Basic socket/listen/options | `socket01,socket02,listen01,getsockname01,getsockopt01,getsockopt02,setsockopt01` | `40/40` | `target/oscomp/ltp-net-b1-basic.txt` |
| Basic send/recv | `send01,send02,sendto01,sendto02,sendto03,recv01,recvfrom01` | `34/35` | `target/oscomp/ltp-net-b2-sendrecv-after-userns-packet.txt` |
| msg/mmsg | `sendmsg01,sendmsg02,sendmsg03,recvmsg01,recvmsg02,recvmsg03,sendmmsg01,sendmmsg02,recvmmsg01` | `36/38` | `target/oscomp/ltp-net-b3-msg-after-yield-queue.txt` |
| bind/connect/accept | `bind01,bind02,bind03,bind04,bind05,bind06,connect01,connect02,accept01,accept02,accept03,accept4_01,getpeername01` | `74/86` | `target/oscomp/ltp-net-b4-after-bind06-current.txt` |
| socketpair/socketcall | `socketpair01,socketpair02,socketcall01,socketcall02,socketcall03` | `14/17` | `target/oscomp/ltp-net-b5-socketpair-socketcall.txt` |
| setsockopt tail | `setsockopt02,setsockopt03,setsockopt04,setsockopt05,setsockopt06,setsockopt07,setsockopt08,setsockopt09,setsockopt10` | `10/11` | `target/oscomp/ltp-net-b6-after-tls-ulp.txt` |
| IPv6 UDP focused | `bind05,recvmsg02` | `15/15` | `target/oscomp/ltp-net-ipv6-udp.txt` |
| IPv6 dual-stack TCP focused | `connect02` | `1/1` | `target/oscomp/ltp-net-ipv6-connect02.txt` |
| accept tail focused | `accept03,accept4_01,getpeername01` | `28/39` | `target/oscomp/ltp-net-accept-tail-after-connect02.txt` |
| accept03 fd-provider focused | `accept03` | `20/23` | `target/oscomp/ltp-accept03-after-mount-api-fds.txt` |
| old kconfig blocker probe | `bind06,sendto03,sendmsg03,setsockopt05,setsockopt06,setsockopt07,setsockopt08,setsockopt09,setsockopt10` | `0/9` | `target/oscomp/ltp-net-kconfig-after-config-file.txt` |
| userns packet send focused | `sendto03` | `2/2` | `target/oscomp/ltp-sendto03-userns-after-packet-send.txt` |
| userns packet MTU focused | `setsockopt05` | `1/1` | `target/oscomp/ltp-setsockopt05-userns-after-mtu.txt` |
| userns packet reserve focused | `setsockopt07` | `1/1` | `target/oscomp/ltp-setsockopt07-userns-after-reserve2.txt` |
| userns netfilter replace focused | `setsockopt08` | `1/1` | `target/oscomp/ltp-setsockopt08-netfilter-minimal.txt` |
| userns packet fanout focused | `setsockopt09` | `1/1` | `target/oscomp/ltp-setsockopt09-userns.txt` |
| userns packet bind race focused | `bind06` | `1/1` | `target/oscomp/ltp-bind06-current330.txt` |
| fuzzy raw send focused | `sendmsg03` | `1/1` | `target/oscomp/ltp-sendmsg03-after-yield-queue-maxruntime10.txt` |
| fuzzy packet ring focused | `setsockopt06` | `1/1` | `target/oscomp/ltp-setsockopt06-after-yield-queue-maxruntime20.txt` |
| TCP TLS ULP focused | `setsockopt10` | `1/1` | `target/oscomp/ltp-setsockopt10-tls-ulp-rebuilt.txt` |

The first six result rows cover all 50 named syscall-network cases. The current
local judge subcase total is now `208/227` after the b2/b3/b4/b6 refreshes.
Treat that as a split-batch progress score, not as "50/50 cases passed". The
kconfig blocker probe and the focused IPv6/accept/userns rows overlap the split
batches, so they are not added to that total.
The focused `accept03` pidfd+memfd+fsnotify+mount-api-provider runs prove
seven additional b4 points, but the split total stays `208/227` until the full
b4 row is refreshed. Supporting ABI witnesses: `memfd_create02` reports
`10/14` in `target/oscomp/ltp-memfd-create02-basic.txt`, and
`inotify_init1_01,inotify_init1_02` report `8/8` in
`target/oscomp/ltp-inotify-init1-basic.txt`. The memfd skipped subcases are
the deliberately unsupported seal/hugetlb flag paths; fsnotify watch/mark
event production remains future work. `fsopen`/`fspick`/`open_tree` are only
scoped fd providers here; full `fsconfig`/`fsmount`/`move_mount` topology
semantics remain deferred.

Recent userns-gated probes changed the interpretation of the old kconfig row:
`CONFIG_USER_NS=y`, procfs `uid_map`/`gid_map`/`setgroups`, loopback MTU ioctl,
packet `PACKET_VNET_HDR`, AF_PACKET `sendto(sockaddr_ll)`, raw IPv4
`IP_HDRINCL`, packet reserve/ring validation, and malformed legacy
`IPT_SO_SET_REPLACE` validation now have focused witnesses.
`sendmsg03` and `setsockopt06` now have complete focused witnesses after the
userspace `sched_yield()` scheduler placement fix and the focused
`LTP_MAX_RUNTIME` runner knob. These passes prove the earlier timeouts were not
network-stack table scans. `sendto03`, `sendmsg03`, `bind06`, and
`setsockopt05..10` are now reflected in refreshed split rows. The b6
`setsockopt06` row remains clean after using `LTP_MAX_RUNTIME=30` for that
case only; the earlier `LTP_MAX_RUNTIME=20` b6 run produced a `TWARN` after
TPASS.

There is also a partial full-list probe:

- `target/oscomp/ltp-net-50-after-bind-fixes.txt`: local judge reports
  `101/120`, but the boot argument was truncated around `send01+s`.
  Treat this as evidence for the cases that actually ran, not as a 50-case
  aggregate score.
- `target/oscomp/ltp-net-b4-after-fsnotify-fds.txt`: 360s refresh attempt
  timed out after entering the known slow `connect02`, before accept03 ran.
  Local judge reports `39/40` for the completed prefix through `connect01`;
  use `target/oscomp/ltp-accept03-after-fsnotify-fds.txt` as the fsnotify
  accept03 witness instead of treating this partial log as the b4 aggregate.

Score with:

```sh
python3 tools/oscomp-judge.py target/oscomp/<log>.txt target/oscomp/testdata
```

`FAIL LTP CASE <case> : 0` can be a legacy OSComp end marker. Trust the local
judge and the `TPASS/TFAIL/TBROK/TCONF/TWARN` lines, not the marker by itself.

## Cases With Good Focused Coverage

These have focused passing coverage or pass all supported subcases in the logs
above:

- ABI/options: `socket01`, `socket02`, `listen01`, `getsockname01`,
  `getsockopt01`, `getsockopt02`, `setsockopt01`
- send/recv basics: `send01`, `send02`, `sendto01`, `sendto03`, `recv01`,
  `recvfrom01`
- message vectors: `sendmsg01`, `sendmsg02`, `sendmsg03`, `recvmsg01`,
  `sendmmsg01`, `sendmmsg02`
- AF_UNIX/socketpair: `socketpair01`, `socketpair02`, `getpeername01`
- bind/connect/accept: `bind01`, `bind02`, `bind03`, `bind06`, `connect01`,
  `accept01`, `accept02`
- UDP-Lite: IPv4 `bind05` UDP-Lite loopback and wildcard datagram subcases
  pass through the UDP-like datagram path
- IPv6 UDP/UDP-Lite: `bind05` IPv6 loopback and wildcard datagram subcases pass;
  `recvmsg02` passes `recvmsg(..., MSG_PEEK)` with IPv6 source-address
  writeback
- IPv6 dual-stack TCP: focused `connect02` passes through IPv4 client to IPv6
  wildcard listener, `IPV6_ADDRFORM`, and `connect(AF_UNSPEC)` reset/rebind
- packet/socket options: `setsockopt02`, `setsockopt04`, `setsockopt05`,
  `setsockopt06`, `setsockopt07`, `setsockopt09`, `setsockopt10`
- legacy netfilter validation: `setsockopt08` malformed
  `IPT_SO_SET_REPLACE` returns `EINVAL`

Do not keep rerunning these alone unless a later change touches their owning
surface. Use them as regression witnesses after related fixes.

## Runner Notes

- `tools/build-slim-sdcard.py` now preserves the input LTP case order when
  generating `ltp_testcode.sh`. This fixed nondeterministic focused batches
  caused by converting the case list to a `set`.
- On this machine, the `make oscomp-local-rv64` Docker path failed before QEMU
  with `unknown shorthand flag: 'f' in -f`. Use the explicit `cargo xtask
  oscomp slim-sdcard` plus `cargo xtask oscomp qemu` path until that local
  Docker/compose issue is fixed.
- `cargo xtask oscomp qemu` reuses `target/oscomp/submit/kernel-rv`; it does
  not rebuild or resubmit the kernel. After changing kernel code, run
  `cargo xtask build --target rv64-qemu` and
  `cargo xtask oscomp submit --target rv64-qemu` before judging a QEMU result.
- RV64 now has a local `make oscomp-qemu-rv64-smp2` runner with
  `OSCOMP_OUT_RV_SMP2`. Use it for focused fuzzy-sync probes that need LTP to
  see more than one CPU while avoiding the `-smp 4` boot-hart-3 sensitivity
  seen in the logs below. For slim focused images, keep judging against
  `target/oscomp/testdata`.
- Focused LTP cases can pass `LTP_MAX_RUNTIME=N`, which the guest converts to
  the official LTP `-I N` integer max-runtime option. This is useful for
  bounding fuzzy witnesses such as `sendmsg03` and `setsockopt06`.
  `LTP_MAX_RUNTIME_CASES=a,b` limits the `-I` suffix to named cases; use that
  for mixed split batches. Do not use fractional `LTP_RUNTIME_MUL` here: the
  guest-side parse produced an overflow-like timeout (`596523h 14m 37s`) in the
  probe. Do not apply `LTP_MAX_RUNTIME` across ordinary b2-style batches: a
  diagnostic run forced `send01` to repeat until EBADF drift and EMFILE.
- LTP can now parse `/boot/config-6.1.0-txkernel`. The config exposes
  `CONFIG_NET_NS=y`, `CONFIG_USER_NS=y`, and the minimal legacy x_tables
  match/target surface needed to validate malformed `IPT_SO_SET_REPLACE`.
  It also exposes `CONFIG_TLS=y` after the constrained TCP TLS ULP metadata
  path landed for `setsockopt10`.

## Known Gaps And Interpretation

| Case | Current observation | Interpretation |
| --- | --- | --- |
| `bind04` | AF_UNIX pathname, abstract stream, abstract seqpacket, and IPv4 TCP subcases pass; SCTP remains `TCONF` | AF_UNIX `SOCK_SEQPACKET` is implemented for LTP's local semantics. Remaining point is unsupported SCTP. |
| `bind05` | AF_UNIX, IPv4 UDP, IPv4 UDP-Lite, IPv6 UDP, and IPv6 UDP-Lite datagram communication pass | Current focused and b4 logs show `14/14`; use this as the IPv6 UDP/UDP-Lite regression witness. |
| `bind06` | Focused 330s run reaches the AF_PACKET bind/ioctl race body, exits by LTP execution time, and passes `1/1` in `target/oscomp/ltp-bind06-current330.txt`; refreshed b4 reports `bind06 1/1` and `74/86` in `target/oscomp/ltp-net-b4-after-bind06-current.txt` | Use the focused log for direct regression and the refreshed b4 log for aggregate score movement. |
| `connect02` | Focused log passes `1/1`; the official case is slow and silent because it loops 1000 times | Use `target/oscomp/ltp-net-ipv6-connect02.txt` as the focused regression witness. A 30s outer timeout is too short for this case even though LTP's internal timeout is 30 guest seconds. |
| `accept03` | Focused `accept03` reports `20/23` in `target/oscomp/ltp-accept03-after-mount-api-fds.txt`: pidfd, fanotify, inotify, memfd, fsopen, fspick, and open_tree now return the expected generic-fd errno (`ENOTSOCK`, except open_tree `EBADF`); perf/bpf/memfd_secret providers remain `TCONF` | Direct blocker is broader fd/syscall surface, not TCP accept dataplane. The new mount API work is intentionally fd-provider-only: full `fsconfig`/`fsmount`/`move_mount` semantics are still a mount-subsystem project. |
| `accept4_01` | Focused tail log reports `8/9`: libc and `__NR_accept4` variants pass all close-on-exec/nonblock subcases; legacy socketcall check is not available on RV64 | Architecture surface; do not fake legacy `socketcall` on RV64. |
| `sendto02` | SCTP not supported, `TCONF` | Protocol family blocker; do not fake SCTP. |
| `sendto03` | Refreshed b2 reports `2/2` in `target/oscomp/ltp-net-b2-sendrecv-after-userns-packet.txt` after AF_PACKET `sendto(sockaddr_ll)`, `PACKET_VNET_HDR`, and packet ring setup support | Use the refreshed b2 log for split movement and `target/oscomp/ltp-sendto03-userns-after-packet-send.txt` as the focused regression witness. |
| `sendmsg03` | Refreshed b3 reports `1/1` in `target/oscomp/ltp-net-b3-msg-after-yield-queue.txt`; focused `LTP_MAX_RUNTIME=10` run also passes `1/1` in `target/oscomp/ltp-sendmsg03-after-yield-queue-maxruntime10.txt` | Not a network-table/linear-scan bottleneck. The raw `IP_HDRINCL` fast path validates four iovecs and returns `EOPNOTSUPP`; it does not enter packet routing or socket-table scans. The old timeout was LTP fzsync plus scheduler placement: userspace `sched_yield()` was requeued to `Preempted` behind hot userspace `New` work. Userspace yields now requeue to the `New` tail. |
| `recvmsg02` | Focused log passes `1/1`: `recvmsg(..., MSG_PEEK)` receives the IPv6 UDP datagram and preserves the datagram | Use `target/oscomp/ltp-net-ipv6-udp.txt` and b3 `35/38` as regression witnesses. |
| `recvmsg03` | RDS not supported, `TCONF` | Protocol family blocker; do not fake RDS. |
| `recvmmsg01` musl | First EBADF subcase passes, then userspace SIGSEGV before the bad-msgvec syscall | Known OSComp musl wrapper issue; kernel semantics have raw/glibc witnesses in `docs/progress/research/2026-05-21-recvmmsg-musl-wrapper-blocker.md` and `2026-05-21-ltp-glibc-sendmsg-witness.md`. |
| `setsockopt03` | One 32-bit compat-only subcase is `TCONF`; supported subcase passes | Expected on RV64 unless compat mode is chartered. |
| `setsockopt05` | Focused log passes `1/1` after loopback MTU ioctl and namespace-relative `CAP_NET_ADMIN` checks | Use `target/oscomp/ltp-setsockopt05-userns-after-mtu.txt` as the focused regression witness. |
| `setsockopt06` | Focused `LTP_MAX_RUNTIME=20` run passes `1/1` in `target/oscomp/ltp-setsockopt06-after-yield-queue-maxruntime20.txt`; refreshed b6 with `LTP_MAX_RUNTIME=30 LTP_MAX_RUNTIME_CASES=setsockopt06` reports `1/1` in `target/oscomp/ltp-net-b6-setsockopt-tail-maxruntime30.txt` | Not a packet-table scan. The hot socket paths are fixed-size sockopt reads/mutations plus per-loop AF_PACKET create/close. The old timeout was dominated by LTP fzsync delay bias and userspace yield placement, not by packet registry complexity. Use the clean focused log for direct regression and the b6 log for aggregate score. |
| `setsockopt07` | Focused log passes `1/1` after `PACKET_RESERVE`/active `PACKET_RX_RING` validation was aligned | Use `target/oscomp/ltp-setsockopt07-userns-after-reserve2.txt` as the focused regression witness. |
| `setsockopt08` | Focused log passes `1/1`: malformed `IPT_SO_SET_REPLACE` returns `EINVAL` with the x_tables kconfig surface advertised | This is validation coverage, not full iptables table installation. Structurally complete replace requests still return `EOPNOTSUPP`. |
| `setsockopt09` | Focused log passes `1/1` with userns/netns setup and current packet fanout semantics | Use `target/oscomp/ltp-setsockopt09-userns.txt` as the focused regression witness. |
| `setsockopt10` | Focused log passes `1/1`, and refreshed b6 reports `setsockopt10 1/1`: `TCP_ULP` accepts `"tls"` on connected TCP, `SOL_TLS/TLS_TX` records TX setup metadata, `connect(AF_UNSPEC)` preserves the ULP state, and `listen()` returns `EINVAL` after rebind | This is the Linux CVE-2023-0461 guard needed by LTP, not TLS record encryption. Use `target/oscomp/ltp-setsockopt10-tls-ulp-rebuilt.txt` as the focused regression witness and `target/oscomp/ltp-net-b6-after-tls-ulp.txt` for aggregate score. |
| `socketcall01..03` | RV64 has no legacy `socketcall` syscall | Architecture surface; expect unsupported/TCONF-style behavior, not an RV64 kernel bug. |

## Recommended Next Runs

Use short timeouts first. If a focused run gives no useful serial/LTP output by
30s, stop and inspect the last emitted line before increasing the timeout.

### 1. Next score candidates

The b2, b3, b4, and b6 split rows are fresh. The remaining score blockers are
now mostly explicit unsupported or broader non-network surfaces:

- `sendto02`: SCTP unsupported.
- `recvmsg03`: RDS unsupported.
- `socketcall01..03`: legacy socketcall is not an RV64 syscall surface.
- `accept03`: pidfd, memfd, fanotify, inotify, fsopen, fspick, and open_tree
  fds are now implemented for generic fd classification; remaining broad
  descriptor providers are perf, bpf, and memfd_secret, all outside TCP accept
  dataplane.
- `recvmmsg01` musl: known userspace wrapper SIGSEGV after the raw EBADF
  subcase passes.

For regression refreshes, use:

```sh
timeout 360s make oscomp-qemu-rv64 \
  OSCOMP_LTP=send01,send02,sendto01,sendto02,sendto03,recv01,recvfrom01 \
  OSCOMP_OUT_RV=target/oscomp/ltp-net-b2-sendrecv-after-userns-packet.txt

timeout 240s make oscomp-qemu-rv64 \
  OSCOMP_LTP=sendmsg01,sendmsg02,sendmsg03,recvmsg01,recvmsg02,recvmsg03,sendmmsg01,sendmmsg02,recvmmsg01 \
  LTP_MAX_RUNTIME=10 LTP_MAX_RUNTIME_CASES=sendmsg03 \
  OSCOMP_OUT_RV=target/oscomp/ltp-net-b3-msg-after-yield-queue.txt

timeout 300s make oscomp-qemu-rv64 \
  OSCOMP_LTP=setsockopt02,setsockopt03,setsockopt04,setsockopt05,setsockopt06,setsockopt07,setsockopt08,setsockopt09,setsockopt10 \
  LTP_MAX_RUNTIME=30 LTP_MAX_RUNTIME_CASES=setsockopt06 \
  OSCOMP_OUT_RV=target/oscomp/ltp-net-b6-after-tls-ulp.txt
```

Build and submit the RV64 kernel before these if code changed:

```sh
cargo xtask build --target rv64-qemu
cargo xtask oscomp submit --target rv64-qemu --submit target/oscomp/submit
```

On this local machine, prefer the explicit `cargo xtask oscomp slim-sdcard` +
`cargo xtask oscomp qemu` path if the Docker compose wrapper still fails before
QEMU.

### 3. Reconfirm b4 after accept-surface or protocol changes

```sh
cargo xtask oscomp slim-sdcard \
  --suite ltp-musl \
  --ltp-cases bind01,bind02,bind03,bind04,bind05,bind06,connect01,connect02,accept01,accept02,accept03,accept4_01,getpeername01 \
  --output target/oscomp/ltp-net-b4-bind-connect-accept-data/sdcard-rv.img \
  --size-mb 512

timeout 150s cargo xtask oscomp qemu \
  --target rv64-qemu \
  --data target/oscomp/ltp-net-b4-bind-connect-accept-data \
  --boot-suite ltp

cp target/oscomp/os_serial_out_rv.txt \
  target/oscomp/ltp-net-b4-bind-connect-accept-after-next-fix.txt

python3 tools/oscomp-judge.py \
  target/oscomp/ltp-net-b4-bind-connect-accept-after-next-fix.txt \
  target/oscomp/testdata
```

Use a longer timeout here only because `connect02` is a known slow/silent
1000-iteration case. For focused debugging that does not include `connect02`,
go back to the 30s-first ladder.

### 4. Only after split batches are fresh, make an aggregate

Avoid passing the 50-case comma list through a path that truncates boot args.
Prefer split batch logs and sum the judge outputs until the runner has a
short-name batch or another non-truncating selection path.

## Timeout And Logging Discipline

- Start focused LTP/QEMU runs with `timeout 30s`.
- Increase to `60s`, `120s`, then `300s` only when the previous run showed
  forward progress or the case is known to be slow.
- For focused fuzzy witnesses, prefer `LTP_MAX_RUNTIME=N` over unbounded long
  outer timeouts. It uses LTP's official integer `-I` option and keeps the
  result marker path exercised.
- A 30s silent run is a hang clue. Do not paper it over with a long timeout.
- Always write `OSCOMP_OUT_RV=target/oscomp/<descriptive-name>.txt`.
- After every debug fix or blocker classification, write the detailed debug
  note under `msp/debug-logs/YYYY-MM-DD-short-title.md`:
  - command and log path
  - judge score
  - first failing case and LTP source path
  - root cause
  - semantic fix or reason for deferral
  - host unit tests and focused LTP/OSComp witnesses

Do not `git add msp/`. Update `docs/progress/STATUS.md` only with concise
summary-level changes. Use this file for the network syscall score ledger; keep
long debug transcripts, rebase notes, and case-by-case root-cause write-ups in
`msp/debug-logs/` unless the user explicitly asks for a repo-tracked research
note.
