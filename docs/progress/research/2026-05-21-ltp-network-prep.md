# LTP Network Prep Notes

Date: 2026-05-21

## Question

What preparation is needed before using OSComp's LTP payload to advance
txKernel network support?

## Findings

- OSComp's `scripts/ltp/ltp_testcode.sh` directly iterates every file under
  `ltp/testcases/bin`. That is too broad for network bring-up: it mixes network
  with hundreds of unrelated syscall, fs, vm, ipc, sched, and security cases.
- LTP's upstream network runner is `testscripts/network.sh`, which builds a
  command file from `runtest/net.*` and invokes `ltp-pan`. Those suites mostly
  expect a Linux-like networking environment: `ip`, `ifconfig`, `route`,
  `iptables`/`nft`, `tc`, `sysctl`, module/driver checks, remote-host helpers,
  and environment such as `LHOST_IFACES`/`RHOST_IFACES`.
- The lowest-risk first LTP network slice is not `runtest/net.*`; it is the
  syscall cases in `runtest/syscalls` for socket APIs:
  `socket*`, `bind*`, `connect*`, `accept*`, `listen*`, `send*`, `sendto*`,
  `sendmsg*`, `recv*`, `recvfrom*`, `recvmsg*`, `getsockname`,
  `getpeername`, `getsockopt`, `setsockopt`, `socketpair`, and `sockioctl`.
- Txv2 already has `cargo xtask oscomp slim-sdcard --suite ltp-musl
  --ltp-cases ...`, which can build a small SD card containing only selected
  LTP syscall case binaries plus LTP infrastructure. It also generates a focused
  `ltp_testcode.sh` that preserves the OSComp judge markers.

## Recommended Bring-Up Order

For the detailed layer-by-layer plan, see
`docs/progress/research/2026-05-21-ltp-network-layered-plan.md`.

1. Prepare a focused `ltp-net-syscalls` SD card using selected syscall cases,
   and run it with `--boot-suite ltp`.
2. Start with creation/error semantics: `socket01`, `socket02`, `bind01`,
   `bind02`, `listen01`, `getsockname01`, `getpeername01`, `getsockopt01`,
   `setsockopt01`.
3. Move to local TCP/UDP behavior: `connect01`, `connect02`, `accept01`,
   `accept02`, `accept03`, `send01`, `send02`, `sendto01`, `recv01`,
   `recvfrom01`.
4. Then handle message-vector and multiplexing edges: `sendmsg*`, `recvmsg*`,
   `sendmmsg*`, `recvmmsg*`, `poll`/`ppoll`/`epoll` socket interactions.
5. Treat `runtest/net.tcp_cmds` and `runtest/net.features` as later work.
   Those suites need userspace tools, netlink/interface control, sysctl, driver
   presence, and often remote-host semantics.

## Not A Good First Target

- `net.ipv6`, `net.ipv6_lib`: require IPv6 semantics and options.
- `net.sctp`, `net_stress.ipsec_*`, `dccp`, `mpls`, `vxlan`, `wireguard`,
  `iptables`/`nft`: unsupported protocol/module/control-plane space.
- `net.nfs`, `net.rpc_tests`, `net.tirpc_tests`: RPC/NFS stack and services.
- `net_stress.appl`: expects application daemons such as SSH/DNS/HTTP/FTP.

## Practical Command Shape

Use a separate data directory so the canonical OSComp image is not overwritten:

```sh
mkdir -p target/oscomp/ltp-net-syscalls
cargo xtask oscomp slim-sdcard \
  --suite ltp-musl \
  --ltp-cases socket01,socket02,bind01,listen01,getsockname01 \
  --output target/oscomp/ltp-net-syscalls/sdcard-rv.img \
  --size-mb 512
cargo xtask oscomp qemu \
  --target rv64-qemu \
  --data target/oscomp/ltp-net-syscalls \
  --boot-suite ltp
```

For scoring with the local helper, copy the judge files from
`target/oscomp/testdata` into the focused data directory or inspect
`target/oscomp/os_serial_out_rv.txt` directly.

## Policy

Do not hardcode individual LTP test names into syscall behavior. Use LTP cases
as witnesses for Linux-compatible socket semantics and keep unsupported protocol
families returning principled errors such as `EAFNOSUPPORT`,
`EPROTONOSUPPORT`, or `ENOPROTOOPT`.
