# LTP Network Layered Bring-Up Plan

Date: 2026-05-21

## Question

For txKernel's OSComp work, what does "LTP network" actually contain, how
should it be layered, and when do we need to confront the still-thin NIC,
netns, veth, and packet-control-plane surfaces?

## Sources Inspected

- Local LTP tree:
  `/home/msp/learning/rustOS/testsuits-for-oskernel/ltp-full-20240524`
- OSComp LTP runner:
  `/home/msp/learning/rustOS/testsuits-for-oskernel/scripts/ltp/ltp_testcode.sh`
- LTP network runner:
  `ltp-full-20240524/testscripts/network.sh`
- LTP network environment helpers:
  `ltp-full-20240524/testcases/lib/tst_net.sh`
- LTP network C helpers:
  `ltp-full-20240524/include/tst_net.h`,
  `ltp-full-20240524/include/tst_safe_net.h`,
  `ltp-full-20240524/lib/tst_net.c`
- Runtest lists:
  `ltp-full-20240524/runtest/syscalls`,
  `ltp-full-20240524/runtest/net.*`,
  `ltp-full-20240524/runtest/net_stress.*`,
  `ltp-full-20240524/runtest/can`,
  selected `runtest/containers` entries.

## Executive Summary

Do not start by running upstream `testscripts/network.sh`. In its default
single-host mode, LTP creates a separate network namespace, moves a veth pair
into it, configures addresses/routes with `ip`, and uses `tst_ns_exec` as the
remote host. That means even "localhost-style" LTP network suites quickly
become tests of netns, veth, rtnetlink/ioctl, sysfs/procfs, iproute tooling,
and interface lifecycle, not just TCP/UDP loopback correctness.

The practical first target is still LTP's socket syscall slice under
`runtest/syscalls`, but that slice must be split further. Some cases only test
ABI and errno behavior; some require TCP listen/connect/accept and readiness;
some require AF_UNIX socketpair/pathname behavior; some require AF_PACKET,
multicast, netfilter, IPv6, SCTP, or user+net namespace setup. Treating all
`socket*` cases as one layer will blur very different implementation surfaces.

The first "real NIC" gate should come after loopback socket syscalls are stable
and after txKernel has enough interface-control visibility for `ip`,
`ifconfig`, `SIOCGIFCONF`, `SIOCGIFFLAGS`, `SIOCGIFINDEX`, `/sys/class/net`,
and route dumps. Before that, failures in `network.sh` are likely to be harness
environment gaps rather than TCP/UDP dataplane bugs.

## Network Suite Inventory

Non-comment case counts in the upstream LTP runtest lists:

| Runtest file | Cases | Early relevance |
| --- | ---: | --- |
| `runtest/syscalls` socket subset | 67 | First focused target |
| `net.features` | 62 | Later; advanced kernel networking features |
| `net.ipv6` | 11 | Later; IPv6 command tests |
| `net.ipv6_lib` | 6 | Middle/later; IPv6 libc and API behavior |
| `net.multicast` | 4 | Later; multicast membership/options/control plane |
| `net.nfs` | 113 | Out of first network-stack target |
| `net.rpc_tests` | 51 | Out of first target; rpcbind/portmapper/services |
| `net.sctp` | 41 | Out of first target unless SCTP is chartered |
| `net.tcp_cmds` | 17 | Later; ping/ip/tcpdump/netstat/tooling |
| `net.tirpc_tests` | 41 | Out of first target; TI-RPC services |
| `net_stress.appl` | 10 | Later; SSH/DNS/HTTP/FTP daemons |
| `net_stress.broken_ip` | 11 | Later; raw malformed packet injection |
| `net_stress.interface` | 25 | Later; interface add/del/up/down/MTU/route |
| `net_stress.route` | 14 | Later; routing table churn |
| `net_stress.multicast` | 24 | Later; multicast stress |
| `net_stress.ipsec_*` | 509 | Out of first target; IPsec/xfrm/setkey |
| `can` | 3 | Out of TCP/IP stack |

The socket syscall subset currently includes:

```text
accept01 accept02 accept03 accept4_01
bind01 bind02 bind03 bind04 bind05 bind06
connect01 connect02
getpeername01 getsockname01
getsockopt01 getsockopt02
listen01
recv01 recvfrom01 recvmsg01 recvmsg02 recvmsg03 recvmmsg01
send01 send02 sendmsg01 sendmsg02 sendmsg03 sendmmsg01 sendmmsg02
sendto01 sendto02 sendto03
setsockopt01 setsockopt02 setsockopt03 setsockopt04 setsockopt05
setsockopt06 setsockopt07 setsockopt08 setsockopt09 setsockopt10
socket01 socket02 socketcall01 socketcall02 socketcall03
socketpair01 socketpair02 sockioctl01
```

`sendfile02` through `sendfile09` also use socket outputs in places, but they
mix file/VFS and socket behavior. Keep them off the first network-only slice
unless a specific OSComp failure points there.

## LTP Network Environment Shape

LTP has two broad network modes:

1. Single host, no `RHOST` set:
   `tst_net.sh` sets `TST_USE_NETNS=yes`, creates `ltp_ns`, creates a veth
   pair, moves one side into the namespace, configures IPv4/IPv6 addresses and
   routes, then runs "remote" commands through namespace execution.

2. Two host, `RHOST` set:
   LTP uses SSH/root remote execution and expects real local/remote interface
   configuration plus service setup.

For txKernel, the first mode is better than requiring a second machine, but it
is not cheap. It requires:

- `unshare`/namespace setup helpers and a usable `/proc/self/{uid_map,gid_map}`
  shape for tests that create user+net namespaces.
- Veth device creation/move, link up/down, route setup, and address assignment.
- Enough rtnetlink and socket ioctl behavior for `ip`, `ifconfig`, `route`,
  and LTP helper probes.
- `/sys/class/net`, `/proc/sys/net`, and selected `/proc/net` visibility.
- ICMP/raw/packet behavior for ping, tcpdump, malformed-packet, and multicast
  witnesses.

This is why the plan below separates loopback TCP/UDP correctness from the
later interface-control and NIC gates.

## Layered Plan

### Layer 0: Harness And Inventory

Goal: make LTP network work selectable and reproducible without running every
LTP binary.

Use `cargo xtask oscomp slim-sdcard --suite ltp-musl --ltp-cases ...` to build
case-specific images. Keep separate data directories for each layer so serial
logs and judge outputs are not overwritten.

Suggested first batch shape:

```sh
cargo xtask oscomp slim-sdcard \
  --suite ltp-musl \
  --ltp-cases socket01,socket02,listen01,getsockname01,getsockopt01,setsockopt01 \
  --output target/oscomp/ltp-net-layer1/sdcard-rv.img \
  --size-mb 512
cargo xtask oscomp qemu \
  --target rv64-qemu \
  --data target/oscomp/ltp-net-layer1 \
  --boot-suite ltp
```

Success criteria:

- The focused image runs only the selected cases.
- Each failing case is classified as socket ABI, loopback TCP/UDP,
  readiness/wait, AF_UNIX, control-plane, or out-of-scope protocol.
- No kernel behavior branches on LTP binary names, paths, argv, fixed payloads,
  or benchmark labels.

### Layer 1: Pure Socket ABI And Errno Semantics

Goal: make socket creation, flags, invalid fd handling, user-copy errors, and
basic option errno behavior Linux-compatible before chasing dataplane bugs.

Primary cases:

- `socket01`: domain/type/protocol validity and expected success families.
- `socket02`: `SOCK_CLOEXEC` and `SOCK_NONBLOCK` flag propagation.
- `listen01`: EBADF/ENOTSOCK and UDP `listen()` error behavior.
- `getsockname01`: EBADF/ENOTSOCK/EFAULT/EINVAL shape on a bound socket.
- `getsockopt01`: invalid level/name/pointer/length/fd semantics.
- `setsockopt01`: invalid level/name/pointer/length/fd semantics.
- `accept01` and `accept03`: invalid accept inputs and non-socket fd shapes.

Likely implementation surfaces:

- `crates/tx-shims/src/linux_syscall/socket.rs`
- fd table and user-copy errno mapping
- existing socket object type checks

Do not include yet:

- `socketpair01/02`, because passing them correctly requires AF_UNIX
  socketpair behavior, not AF_INET loopback.
- `setsockopt02+`, because they quickly move into AF_PACKET, netfilter,
  buffer-force, UDP UFO, TLS ULP, or namespace/CVE territory.

### Layer 2: IPv4 Loopback Bind And Local Endpoint Semantics

Goal: verify local address/port ownership, wildcard/exact conflicts,
autobind, address naming, and principled unsupported-family errors.

Candidate cases:

- `bind01`: AF_INET bind errors, ANY:0 success, non-local address
  `EADDRNOTAVAIL`, invalid length, non-socket fd.
- `bind02`: privileged-port bind semantics. This is socket-adjacent but also
  credential/capability behavior; include only if the credential path is ready.
- `getpeername01`: `ENOTCONN` and invalid argument behavior. It also uses
  AF_UNIX socketpair for some variants, so classify failures carefully.
- `sockioctl01`: early interface ioctl witness for `SIOCGIFCONF`,
  `SIOCGIFFLAGS`, and invalid `SIOCSIFFLAGS`.

Use caution with:

- `bind04` and `bind05`: they are useful for TCP/UDP bind behavior but the
  complete LTP cases also include AF_UNIX, IPv6, SCTP, and UDPLITE variants.
  They are not ideal first witnesses unless unsupported families TCONF cleanly
  or txKernel is ready to implement those sibling surfaces.

Likely implementation surfaces:

- `crates/tx-subsystems/src/net/execution/step_bind.rs`
- loopback endpoint tables and port allocation
- socket shim sockaddr validation
- minimal interface ioctl projection

### Layer 3: TCP Listen/Connect/Accept Lifecycle

Goal: make the TCP lifecycle reliable enough for LTP and for lmbench
`lat_tcp`, `lat_connect`, `bw_tcp`, netperf, and iperf3.

Primary cases:

- `connect01`: forked TCP server, `select`, `listen`, `accept`, `read`, and
  error cases such as `ECONNREFUSED`, `EISCONN`, and `EAFNOSUPPORT`.
- `accept4_01`: accepted fd flags for `SOCK_CLOEXEC` and `SOCK_NONBLOCK`.
- `send01`: TCP and UDP send errors, `EPIPE` after shutdown, UDP `EMSGSIZE`,
  unsupported `MSG_OOB`.
- `sendto01`: connected TCP success plus sendto-specific invalid address and
  length errors.
- `recv01` and `recvfrom01`: TCP receive, `select` readiness, invalid buffer,
  `MSG_OOB`, and `MSG_ERRQUEUE` behavior.

Key semantic areas:

- SYN/listen backlog and loopback handshake progress.
- Nonblocking connect and `EINPROGRESS`/completion visibility.
- `accept` wakeups and fd lifetime across fork.
- close/shutdown/EOF and writer/reader wakeups.
- socket readiness in `select`/`poll`/`epoll`.

Refactor discussion required if the fix needs broad changes to shared TCP
state, wait-source ownership, fd close accounting, or protocol lifecycle
abstractions.

### Layer 4: UDP And Message-Vector Semantics

Goal: cover datagram batching and iovec surfaces after basic TCP/UDP send/recv
is trustworthy.

Primary cases:

- `sendmmsg02`: error-only sendmmsg behavior.
- `sendmmsg01`: two-message UDP sendmmsg to loopback.
- `recvmmsg01`: recvmmsg errors, timeout shape, and UDP receive path.
- `sendmsg01`, `sendmsg02`, `sendmsg03`: message header/iovec validation and
  sendmsg behavior. Expect AF_UNIX or ancillary-data blockers in some paths.
- `recvmsg01`: rich recvmsg validation, but it also pulls AF_UNIX
  `SCM_RIGHTS` behavior into the case.

Later within this layer:

- `send02`: `MSG_MORE` behavior for TCP and UDP. It is valuable, but it can
  expose buffering/coalescing and fairness issues; do it after normal
  send/recv is stable.

Defer:

- `recvmsg02`: IPv6 UDP and `MSG_PEEK`/truncation behavior.
- `recvmsg03`: AF_RDS; should TCONF or principled unsupported-family error.

### Layer 5: AF_UNIX Socket Track

Goal: unblock LTP socket cases that are not TCP/IP but are mixed into the same
socket syscall bucket.

Cases:

- `socketpair01`, `socketpair02`: AF_UNIX socketpair and flag behavior.
- `bind03`: AF_UNIX pathname bind/rebind semantics.
- AF_UNIX portions of `bind04`, `bind05`, `getpeername01`, `recvmsg01`, and
  `getsockopt02`.

This is a cross-cutting dependency for "LTP socket syscalls" but not evidence
that IPv4 TCP/UDP loopback is broken. Keep it tracked separately so network
card work does not absorb AF_UNIX pathname and credential work accidentally.

### Layer 6: Interface Control Plane And Visibility

Goal: make userspace network tools see enough interface state to run simple
LTP network commands without requiring advanced protocols.

Minimum gates:

- `lo` appears with sane flags, MTU, addresses, and index.
- `/sys/class/net` exposes the interface names and essential attributes used
  by LTP helpers.
- `/proc/net` and `/proc/sys/net` have the entries that LTP probes need, or
  principled missing-feature behavior where Linux would allow skip.
- Socket ioctls:
  `SIOCGIFCONF`, `SIOCGIFFLAGS`, `SIOCGIFINDEX`, and basic invalid
  `SIOCSIFFLAGS` behavior.
- Rtnetlink dumps are sufficient for `ip link`, `ip addr`, and `ip route`
  read-only queries.

Useful witnesses:

- `sockioctl01`
- selected `ip`/`ifconfig` shell probes
- later, small `net.tcp_cmds` cases that only inspect state

This layer is the bridge between loopback sockets and the full `network.sh`
environment.

### Layer 7: Netns/Veth Single-Host LTP Mode

Goal: make LTP's default single-host "remote" setup viable.

Required pieces:

- `unshare(CLONE_NEWNET)` and user namespace helpers where required by tests.
- `/proc/self/uid_map`, `gid_map`, and `setgroups` behavior for helper setup.
- veth pair creation, move to namespace, link up/down, MTU, address, route.
- namespace execution helper behavior and mounted `/sys` view.
- route and address configuration through netlink or compatible ioctls.

First witnesses after the gate:

- a tiny custom command file using `tst_net.sh` setup only
- `network.sh -t` with a custom file for a small `net.tcp_cmds` subset
- `ping01`/`ping02` only after ICMP is ready

This is the first layer where the thin NIC/control-plane situation becomes a
central blocker. Until this layer, most failures should be loopback socket or
ABI semantics, not hardware driver behavior.

### Layer 8: Packet, ICMP, Raw Socket, And Tooling

Goal: support command-level networking cases that go beyond TCP/UDP loopback.

Surfaces:

- ICMP echo for ping and trace-style tools.
- AF_PACKET and raw socket receive/transmit.
- ARP/neighbor table visibility and updates.
- tcpdump-style packet capture.
- routing table behavior for route and tracepath/traceroute.

Likely LTP groups:

- parts of `net.tcp_cmds`
- parts of `net_stress.broken_ip`
- parts of `net_stress.interface`
- parts of `net_stress.route`

This layer should come after read-only interface visibility is already stable;
otherwise tool failures will be hard to interpret.

### Layer 9: Advanced Network Features

Goal: only after the core TCP/UDP/control-plane work is stable, decide which
Linux feature surfaces are worth implementing or cleanly reporting unsupported.

Groups:

- `net.features`: BBR, DCTCP, TCP Fast Open, bind-no-port, busy poll, VLAN,
  VXLAN, MACVLAN, MACVTAP, MACSEC, IPVLAN, GRE, GUE, FOU, GENEVE, SIT, MPLS,
  packet fanout, wireguard.
- `net.multicast` and `net_stress.multicast`: multicast membership, filters,
  flood/query, IGMP/MLD-like behavior.
- `setsockopt02` through `setsockopt10`: AF_PACKET rings, netfilter compat,
  UDP UFO, TLS ULP, packet version/ring races.

Most of these are feature commitments, not small compatibility patches. They
should be explicitly chartered before implementation.

### Layer 10: Service/Protocol Suites To Defer

Defer until the user explicitly wants these surfaces:

- `net.nfs`: NFS client/server/services.
- `net.rpc_tests`, `net.tirpc_tests`: rpcbind/portmapper/TI-RPC.
- `net.sctp`: SCTP protocol family.
- `net_stress.ipsec_*`: IPsec/xfrm/setkey/vti and stress matrix.
- `net_stress.appl`: SSH/DNS/HTTP/FTP daemon integration.
- `can`: CAN/vcan, outside TCP/IP.
- Full IPv6 command suites, unless IPv6 becomes a goal.

Unsupported protocols should return principled Linux-compatible errors or
TCONF-friendly setup failures. Do not fake success to move score numbers.

## First Three Milestones

Milestone A: Focused socket ABI image

- Run: `socket01,socket02,listen01,getsockname01,getsockopt01,setsockopt01`.
- Expected work: shim errno/flag/user-copy corrections only.
- Exit: all cases pass or each failure is assigned to Layer 2+ with source
  evidence.

Milestone B: IPv4 loopback lifecycle image

- Run: `bind01,connect01,accept01,accept03,accept4_01`.
- Expected work: endpoint ownership, backlog, TCP handshake/readiness, accepted
  fd flags.
- Exit: TCP server/client tests behave without relying on benchmark-specific
  timing or port constants.

Milestone C: TCP/UDP data image

- Run: `send01,sendto01,recv01,recvfrom01,sendmmsg02,sendmmsg01,recvmmsg01`.
- Expected work: UDP `EMSGSIZE`, shutdown/EPIPE, `MSG_DONTWAIT`,
  iovec/message-vector validation, receive wakeups.
- Exit: existing OSComp `libctest-network` and `lmbench-network` remain green,
  and the new LTP cases are either passing or blocked by named non-network
  prerequisites.

## Verification Discipline

For each milestone:

1. Read the failing LTP source before editing.
2. Add or adjust a focused host unit test when the behavior is owned by
   `tx-subsystems/src/net/`.
3. Run the slim LTP image for the selected cases.
4. Re-run the existing targeted OSComp network regression most likely to
   overlap:
   - `libctest-network` for ABI/socket formatting.
   - `lmbench-network` for TCP/UDP loopback lifecycle and throughput.
5. Update `docs/progress/STATUS.md` with changed behavior, verification,
   next step, and blockers.

## Anti-Hardcoding Policy

LTP cases are witnesses, not kernel feature flags. Acceptable fixes implement
general Linux socket semantics: correct errno, address validation, port
ownership, readiness, message truncation, close/shutdown state, option state,
and unsupported-family reporting.

Do not branch on:

- LTP test names or binary paths.
- argv patterns.
- fixed benchmark labels.
- exact payload strings.
- one-off ports used by a test.
- judge marker strings.

If a correct fix requires broad changes to socket identity, protocol state,
wait-source ownership, fd-table sharing, or interface-control infrastructure,
pause and discuss the refactor plan before changing code.
