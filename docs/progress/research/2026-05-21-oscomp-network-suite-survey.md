# OSComp Network Suite Survey

Date: 2026-05-21

## Question

Which OSComp test groups under
`/home/msp/learning/rustOS/testsuits-for-oskernel`, excluding LTP and the known
network-focused groups `lmbench`, `libctest`, `netperf`, and `iperf`, still need
network-stack support?

## Findings

- Current OSComp scripts with real IPv4 TCP/UDP test coverage remain the known
  groups: `lmbench`, `libctest`, `netperf`, and `iperf`.
- `cyclictest` invokes `hackbench -l 100000000`; this uses
  `socketpair(AF_UNIX, SOCK_STREAM)` by default, not IPv4 TCP/UDP. It is
  socket-adjacent syscall coverage, not a loopback network-stack benchmark.
- `busybox` contains many networking applets in source, but
  `scripts/busybox/busybox_cmd.txt` currently runs only core/file/process
  commands and no `ping`, `wget`, `nc`, `ifconfig`, `route`, `ip`, `telnet`,
  `httpd`, `udhcp`, or related network applets.
- `iozone` contains distributed/PIT network code and builds `pit_server`, but
  `scripts/iozone/iozone_testcode.sh` runs only local file benchmark modes. It
  does not pass the distributed/network flags `-+m`, `-+t`, `-+H`, or `-+P`.
- `basic`, `libc-bench`, `lua`, and `UnixBench` showed no real socket/network
  API use in their current OSComp scripts or relevant sources.

## Evidence

- OSComp source README lists the groups as `basic`, `busybox`, `lua`,
  `libctest`, `iozone`, `unixbench`, `iperf`, `libcbench`, `lmbench`,
  `netperf`, `cyclictest`, and `LTP`.
- Txv2's current default musl script chain maps: `basic`, `busybox`,
  `libctest`, `libcbench`, `lua`, `lmbench`, `iozone`, `netperf`, `iperf`,
  `cyclictest`, and `ltp`.
- `scripts/test_all.sh` includes `netperf_testcode.sh` and
  `iperf_testcode.sh`, while LTP is commented out there.
- `scripts/busybox/busybox_cmd.txt` contains only non-network commands.
- `scripts/iozone/iozone_testcode.sh` uses commands such as
  `./iozone -a -r 1k -s 4m` and `./iozone -t 4 ...`; no distributed/network
  options are present.
- `rt-tests-2.7/src/hackbench/hackbench.c` defaults to AF_UNIX socketpairs;
  AF_INET loopback is used only when `--inet`/`-i` is passed, and the OSComp
  `cyclictest_testcode.sh` does not pass it.

## Practical Priority

1. Keep treating `lmbench`, `netperf`, and `iperf` as the next real network
   stack targets after the already targeted `libctest-network` work.
2. Do not spend network-stack time on current `busybox` or `iozone` OSComp
   scripts unless the command list or iozone flags change.
3. Track `cyclictest`/`hackbench` as socket syscall and scheduler pressure,
   especially `socketpair`, `poll`, fork/exit, and signal cleanup. It is not
   TCP/UDP loopback work unless OSComp switches hackbench to `-i`.
