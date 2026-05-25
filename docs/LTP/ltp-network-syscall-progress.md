# LTP syscall network progress

Date: 2026-05-25
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
| Basic send/recv | `send01,send02,sendto01,sendto02,sendto03,recv01,recvfrom01` | `32/34` | `target/oscomp/ltp-net-b2-sendrecv.txt` |
| msg/mmsg | `sendmsg01,sendmsg02,sendmsg03,recvmsg01,recvmsg02,recvmsg03,sendmmsg01,sendmmsg02,recvmmsg01` | `34/38` | `target/oscomp/ltp-net-b3-msg.txt` |
| bind/connect/accept | `bind01,bind02,bind03,bind04,bind05,bind06,connect01,connect02,accept01,accept02,accept03,accept4_01,getpeername01` | `66/81` | `target/oscomp/ltp-net-b4-bind-connect-accept-after-udplite.txt` |
| socketpair/socketcall | `socketpair01,socketpair02,socketcall01,socketcall02,socketcall03` | `14/17` | `target/oscomp/ltp-net-b5-socketpair-socketcall.txt` |
| setsockopt tail | `setsockopt02,setsockopt03,setsockopt04,setsockopt05,setsockopt06,setsockopt07,setsockopt08,setsockopt09,setsockopt10` | `4/11` | `target/oscomp/ltp-net-b6-setsockopt-tail-after-kconfig-file.txt` |
| kconfig blocker probe | `bind06,sendto03,sendmsg03,setsockopt05,setsockopt06,setsockopt07,setsockopt08,setsockopt09,setsockopt10` | `0/9` | `target/oscomp/ltp-net-kconfig-after-config-file.txt` |

The first six result rows cover all 50 named syscall-network cases. The current
local judge subcase total is `190/221`. Treat that as a split-batch progress
score, not as "50/50 cases passed". The kconfig blocker probe is a focused
diagnostic subset and is not added to that total.

There is also a partial full-list probe:

- `target/oscomp/ltp-net-50-after-bind-fixes.txt`: local judge reports
  `101/120`, but the boot argument was truncated around `send01+s`.
  Treat this as evidence for the cases that actually ran, not as a 50-case
  aggregate score.

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
- send/recv basics: `send01`, `send02`, `sendto01`, `recv01`, `recvfrom01`
- message vectors: `sendmsg01`, `sendmsg02`, `recvmsg01`, `sendmmsg01`,
  `sendmmsg02`
- AF_UNIX/socketpair: `socketpair01`, `socketpair02`, `getpeername01`
- bind/connect/accept: `bind01`, `bind02`, `bind03`, `connect01`,
  `accept01`, `accept02`
- UDP-Lite: IPv4 `bind05` UDP-Lite loopback and wildcard datagram subcases
  pass through the UDP-like datagram path
- packet/socket options: `setsockopt02`, `setsockopt04`

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
- LTP can now parse `/boot/config-6.1.0-txkernel`. The config is conservative:
  it exposes `CONFIG_NET_NS=y` but marks unsupported `CONFIG_USER_NS`, TLS, and
  netfilter match/target support as not set. This changes the old kconfig
  failures from `TBROK: Cannot parse kernel .config` into honest `TCONF`
  configuration skips.

## Known Gaps And Interpretation

| Case | Current observation | Interpretation |
| --- | --- | --- |
| `bind04` | AF_UNIX pathname, abstract stream, abstract seqpacket, and IPv4 TCP subcases pass; SCTP remains `TCONF` | AF_UNIX `SOCK_SEQPACKET` is implemented for LTP's local semantics. Remaining point is unsupported SCTP. |
| `bind05` | AF_UNIX, IPv4 UDP, and IPv4 UDP-Lite datagram communication pass; IPv6 datagram variants stop at `EAFNOSUPPORT` | IPv4 UDP-Lite now uses the UDP-like datagram path. Remaining gap is IPv6. |
| `bind06` | Parses `/boot/config-6.1.0-txkernel`, then `TCONF` because `CONFIG_USER_NS=y` is not satisfied | User namespace support is a direct setup blocker; do not fake it as network behavior. |
| `connect02` | `socket(10, 1, 6) failed: EAFNOSUPPORT` | IPv6 TCP setup path; defer unless IPv6 is in scope. |
| `accept03` | Network accept errno cases mostly run, but many setup probes for pidfd/fanotify/inotify/perf/bpf/new mount APIs are missing or TCONF | Direct blocker is broader fd/syscall surface, not TCP accept dataplane. |
| `accept4_01` | Main accept4 subcases pass; legacy socketcall check is not available on RV64 | Architecture surface; do not fake legacy `socketcall` on RV64. |
| `sendto02` | SCTP not supported, `TCONF` | Protocol family blocker; do not fake SCTP. |
| `sendto03` | Kconfig probe shows `TCONF` because `CONFIG_USER_NS=y` is not satisfied | User namespace setup blocker before the packet/UDP race body. |
| `sendmsg03` | Kconfig probe shows `TCONF` because `CONFIG_USER_NS=y` is not satisfied | User namespace setup blocker before the raw IPv4 race body. |
| `recvmsg02` | IPv6 UDP setup fails with `EAFNOSUPPORT` | IPv6 blocker. |
| `recvmsg03` | RDS not supported, `TCONF` | Protocol family blocker; do not fake RDS. |
| `recvmmsg01` musl | First EBADF subcase passes, then userspace SIGSEGV before the bad-msgvec syscall | Known OSComp musl wrapper issue; kernel semantics have raw/glibc witnesses in `docs/progress/research/2026-05-21-recvmmsg-musl-wrapper-blocker.md` and `2026-05-21-ltp-glibc-sendmsg-witness.md`. |
| `setsockopt03` | One 32-bit compat-only subcase is `TCONF`; supported subcase passes | Expected on RV64 unless compat mode is chartered. |
| `setsockopt05..07,09` | Parse kernel config, then `TCONF` because `CONFIG_USER_NS=y` is not satisfied | User namespace setup blocker before the packet/UDP race bodies. |
| `setsockopt08` | Parse kernel config, then `TCONF` because netfilter match/target and `CONFIG_USER_NS=y` are not satisfied | Netfilter plus user namespace blocker; current `IPT_SO_SET_REPLACE` remains unsupported. |
| `setsockopt10` | Parse kernel config, then `TCONF` because `CONFIG_TLS` is not satisfied | TLS ULP is not implemented; do not fake it. |
| `socketcall01..03` | RV64 has no legacy `socketcall` syscall | Architecture surface; expect unsupported/TCONF-style behavior, not an RV64 kernel bug. |

## Recommended Next Runs

Use short timeouts first. If a focused run gives no useful serial/LTP output by
30s, stop and inspect the last emitted line before increasing the timeout.

### 1. Decide whether to charter user namespace setup

The old `.config` parse blocker is fixed. The shared blocker for
`bind06`, `sendto03`, `sendmsg03`, `setsockopt05..07`, and `setsockopt09` is
now `CONFIG_USER_NS=y` plus `tst_setup_netns()`:

- `unshare(CLONE_NEWUSER)`
- `unshare(CLONE_NEWNET)`
- writes to `/proc/self/setgroups`, `/proc/self/uid_map`, and
  `/proc/self/gid_map`

That is broader process/credential/procfs namespace work. If this is chartered,
write the refactor plan first: affected `proc` syscalls, process namespace
state, procfs map files, risks, and verification. A no-op user namespace just
to get past LTP setup would be misleading.

### 2. Reconfirm b4 after accept-surface or protocol changes

```sh
cargo xtask oscomp slim-sdcard \
  --suite ltp-musl \
  --ltp-cases bind01,bind02,bind03,bind04,bind05,bind06,connect01,connect02,accept01,accept02,accept03,accept4_01,getpeername01 \
  --output target/oscomp/ltp-net-b4-bind-connect-accept-data/sdcard-rv.img \
  --size-mb 512

timeout 30s cargo xtask oscomp qemu \
  --target rv64-qemu \
  --data target/oscomp/ltp-net-b4-bind-connect-accept-data \
  --boot-suite ltp

cp target/oscomp/os_serial_out_rv.txt \
  target/oscomp/ltp-net-b4-bind-connect-accept-after-next-fix.txt

python3 tools/oscomp-judge.py \
  target/oscomp/ltp-net-b4-bind-connect-accept-after-next-fix.txt \
  target/oscomp/testdata
```

### 3. Only after split batches are fresh, make an aggregate

Avoid passing the 50-case comma list through a path that truncates boot args.
Prefer split batch logs and sum the judge outputs until the runner has a
short-name batch or another non-truncating selection path.

## Timeout And Logging Discipline

- Start focused LTP/QEMU runs with `timeout 30s`.
- Increase to `60s`, `120s`, then `300s` only when the previous run showed
  forward progress or the case is known to be slow.
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
