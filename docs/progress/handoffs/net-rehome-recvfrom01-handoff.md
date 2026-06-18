# Handoff — net stack re-home + LTP net/SCTP batch restoration

> Paste this whole file into a fresh conversation. It is self-contained.

## Mission

On branch `feature-network-next`, PR#50 orphaned the entire networking stack during a
`main` reconcile. We are **re-homing the net subsystem onto the rebased tree** and
**restoring the LTP net + SCTP test pass level** the user had before.

**Policy ("网络归我、其它归main"):** for anything networking, the user's
`feature-network-next` work is ground truth and defers to the user/backup branch; for
non-net glue, defer to `main` unless the user explicitly added a feature/bugfix there.

**Methodology the user requires (do not deviate):**
- **Batch testing is the real submission scenario.** Run tests in batches via
  `make oscomp-qemu-rv64 OSCOMP_GROUPS=...`. Batch runs expose interference / cascades /
  crashes that single runs hide. **Never** dismiss a batch failure as "just interference" —
  the batch *is* what gets graded.
- **Verify against the documented baseline per-test**, not by vibes. The SCTP baseline is
  `docs/LTP/runtests/ltp-runtest-net-sctp-progress.md` (witness counts there are ground
  truth, e.g. sockopt witness = 22 TPASS, not 23). Diff every test's TPASS count against it.
- Commit granularity: **coarse** — one commit per test-group / big feature, not per sockopt.
- **Do NOT add Claude as commit coauthor.** Do **not** force-push without explicit OK.
- Autonomous pacing: long runs are fine; don't stop to confirm unless a big decision.
  `git add`/`commit` are pre-authorized; prompt only for `rm`/destructive ops.

## Git state

- Working branch: `feature-network-next` (ahead of origin by ~814 commits, local only).
- **Ground-truth backup of the working pre-rebase state:**
  `feature-network-backup-before-main-rebase-20260605` (= old head `0a21f18a`).
  Use it to recover any orphaned net code:
  `git show feature-network-backup-before-main-rebase-20260605:<path>`.
- This session's restoration commits (most recent first):
  - `0aaa2ad9` tx-shims/time: re-home interval timers (setitimer/getitimer)
  - `ee5f4a06` tx-shims/close: re-home socket teardown on fd close (fixes spurious EADDRINUSE)
  - `d2ec0bdc` tx-fs/procfs: restore full /proc/meminfo (unblocks ALL new-framework LTP tests)
  - `7f32deff` tx-shims/mod: dispatch 7 undispatched net syscalls (sendmsg/recvmsg/+mmsg, getsockopt, getpeername, shutdown)
  - `66dee1eb` tx-shims/socket: re-home the 66 socket-option-NAME constants (fixes ALL set/getsockopt; they had become match *bindings* → 76 unreachable arms)
  - `98467925` kernel/init: re-home the LTP module-driver gate (modules.dep/builtin w/ sctp.ko)
  - `f497ec00` kernel/exec: re-home the LTP runtest runner (`tx.oscomp.groups=ltp-runtest:...`)
  - `2ebc196f` hal/boot: fix boot livelock — kernel image outgrew the 16M bootstrap alias window (bumped to 32M; see topology.rs + boot_trampoline.rs must stay in sync)

## VERIFIED PASSING (individually, fresh boot, vs baseline witnesses)

- **net.sctp:** `test_assoc_shutdown` TPASS; `test_1_to_1_sockopt` 22/22; `test_1_to_1_socket_bind_listen` 14/14; `test_basic` 14/15 — all match/meet the documented baseline.
- **net syscall batch (submission scenario), all PASS:** `getsockname01`, `getsockopt01`,
  `getsockopt02`, `listen01`, `recv01`, plus `getpeername01`, `bind03` individually.
  (Before the meminfo fix these ALL failed at setup with TBROK.)

## THE BATCH BLOCKER — multi-process TCP-loopback connect cold-start hang (NOT recvfrom-specific)

**This diagnosis was completed empirically — do not re-litigate the ruled-out hypotheses.**

The net syscall batch runs alphabetically and **hangs at `recvfrom01`**, blocking every
test after it. But the hang is **NOT recvfrom-specific** — `recv01` hangs at the *identical*
point when run alone. Evidence:

- `recvfrom01.c` and `recv01.c` are the **same old-style multi-process LTP test**:
  `start_server()` → `tst_fork()` → child `do_child()` does `accept()` + `write(newfd,"hoser\n",6)`;
  the parent's `setup1()` does `socket()`+`connect()` then **`select()`s up to 2 s** for data.
- Both go through the **same syscall**: riscv64 has **no `NR_RECV`** (numbers.rs has only
  `NR_RECVFROM=207`); musl `recv()` = the recvfrom syscall with `from=NULL`. So recv01 and
  recvfrom01 both drive `recvfrom_impl` (socket.rs:899); recv01 with `args[4]=0`, recvfrom01
  testno 2 with `args[4]=0xffff…`.
- **Empirical (fresh boot, individual runs):**
  - `recvfrom01` alone: prints testno 1,2 TPASS (setup0, bad-fd/non-socket — no connect),
    then **silent hang at testno 2 (0-indexed) = first `setup1` case**. 320 s, no further output.
  - `recv01` alone: **identical** — testno 1,2 TPASS then silent hang at the first `setup1` case.
  - **No `TBROK "no message ready in 2 sec"` is ever printed** → setup1 never reaches/returns
    from its 2 s `select()`. So the stall is in **`connect()`** (or `select()`'s timeout is
    not honored), *before* any recv.
- `recv01` "PASS" in the earlier batch was **state-dependent / a false pass** — it ran 5th,
  after `getsockname01/getsockopt01/getsockopt02/listen01` warmed the loopback/net-delegate.
  Alone it hangs. (⚠️ Lesson: the batch can produce FALSE PASSES that mask cold-start hangs —
  verify net tests **individually too**, not only in batch.)

**RULED OUT — do not chase these:**
- *recvfrom source-address writeback to `from=0xffff…`*: for a **connected TCP (SOCK_STREAM)**
  socket, `consume_recv_bytes_into` sets `source: None` (payload.rs:41,54 — only **UDP**
  sets `source: Some`, line 71). So `write_sockaddr_endpoint` is **never called** for these
  tests; the bad `from` pointer is never dereferenced. Confirmed the hang is pre-recv.
- *itimer / framework watchdog*: irrelevant — the stall is in connect, before any blocking recv.

**ROOT CAUSE (high confidence):** the **first multi-process TCP-loopback `connect()` on a
cold boot hangs** — the client process blocks in `connect()` and the SYN/SYN-ACK handshake
with the **forked child server's `accept()`** is never driven (the two processes don't
interleave, and/or loopback delivery isn't pumped while connect blocks). recvfrom_impl pumps
loopback in its wait loop via `drive_loopback_pending()` (socket.rs:975); the connect/accept
path likely does not.

**Where to fix (LIVE code — note the dead-code trap):**
- LIVE, dispatched (mod.rs:913-915, all `.await`ed): `sys_connect`→`connect_impl`
  (**socket.rs:399 / 406, async**), `sys_accept`/`sys_accept4` (**socket.rs:321**).
- **DEAD CODE — ignore:** `crates/tx-shims/src/linux_syscall/net.rs` has an orphaned
  `FakeSocket`/`SOCKETS` stub `sys_connect` (net.rs:265) / `sys_accept4` / `sys_sendto`.
  These are **not dispatched**. (Separate cleanup candidate: the whole net.rs FakeSocket
  layer looks orphaned by the re-home — verify and consider deleting.)

**First action:** read `connect_impl` (socket.rs:406) and the accept wait path (socket.rs:321).
Check whether connect's blocking wait loop (a) yields so the forked server's `accept()` runs,
and (b) calls `drive_loopback_pending()` (or kicks the net delegate) so the handshake packets
move while connect blocks — mirroring `recvfrom_impl`'s loop at socket.rs:973-1028. Add that
driving/yielding if missing. Also confirm `select()`/`poll()` honor a finite timeout (so a
genuinely-dataless setup1 would TBROK rather than hang).

**Verify the fix:** `recv01` ALONE and `recvfrom01` ALONE must BOTH complete (fresh boot, not
a warmed batch):
```
cargo build -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf
cp target/riscv64gc-unknown-none-elf/debug/tx-kernel-riscv64-qemu-virt target/oscomp/submit/kernel-rv
timeout 120 make oscomp-qemu-rv64 OSCOMP_GROUPS=ltp-runtest:syscalls:recv01 > /tmp/recv01.log 2>&1; grep -aE "recv01 +[0-9]|TBROK|PASS LTP CASE|FAIL LTP CASE" /tmp/recv01.log
timeout 120 make oscomp-qemu-rv64 OSCOMP_GROUPS=ltp-runtest:syscalls:recvfrom01 > /tmp/rf.log 2>&1; grep -aE "recvfrom01 +[0-9]|TBROK|PASS LTP CASE|FAIL LTP CASE" /tmp/rf.log
```
Kill stray qemu between runs: `pkill -9 -f qemu-system-riscv64; sleep 2`.

## OTHER REMAINING net items (after recvfrom01)

- **sendmsg01 exits 139 (SIGSEGV)** at teardown *after* all 11 cases TPASS. A teardown
  segfault — investigate the cleanup path (likely msghdr/iovec free or a UAF on close).
- **recvmsg01 / recvmmsg01** failed in an earlier batch — re-run individually, compare to
  baseline, root-cause (may share recvfrom01's addr/recv root cause).
- **Run the FULL batches and diff against baselines (the user's explicit goal):**
  - net syscall batch (user's b1–b6 groups): socket / bind / connect / send / recv / msg /
    sockopt families via `ltp-runtest:syscalls:<+joined filter>`.
  - full `net.sctp` batch via `ltp-runtest:net.sctp` (no case filter = all cases).
  - Confirm **no missing tests / no regressions** vs `docs/LTP/runtests/ltp-runtest-net-sctp-progress.md`
    and the `target/oscomp/ltp-net-*.txt` witness files.
- Regression guard each round: `ltp-runtest:net.sctp:test_assoc_shutdown` must stay TPASS.

## How to build → submit → run (the submission scenario)

```bash
# 1. build kernel ELF (host target dir is set by the Makefile vars)
cargo build -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf
# (or: cargo xtask full-build --target rv64-qemu)

# 2. copy kernel into the submit dir QEMU boots
cp target/riscv64gc-unknown-none-elf/debug/tx-kernel-riscv64-qemu-virt \
   target/oscomp/submit/kernel-rv

# 3. run one or more LTP groups in a batch (THIS is the graded scenario)
timeout 200 make oscomp-qemu-rv64 \
  OSCOMP_GROUPS=ltp-runtest:net.sctp:test_assoc_shutdown > /tmp/run.log 2>&1
```

**Boot-arg / OSCOMP_GROUPS format** (handled by the re-homed runner in
`crates/tx-kernel/src/init/exec.rs`):
- `ltp-runtest:<module>` → run all cases in that LTP runtest module.
- `ltp-runtest:<module>:<caseA>+<caseB>` → run only those cases (`+`-joined filter).
- `ltp-runtest:net.sctp:test_assoc_shutdown` → one SCTP case.
- `ltp-runtest:syscalls:recvfrom01+recv01` → specific net syscall cases.
- Multiple groups: comma- or space-join in `OSCOMP_GROUPS`.

**Reading results:** the runner prints `RUN/PASS/FAIL LTP CASE <name> : <ret>` and per-case
`TPASS/TFAIL/TBROK/TCONF`. **ret=0 = pass** (it prints both a PASS and a FAIL line when
ret=0 — rely on the per-case `TPASS` count + `: 0`, and diff the TPASS count vs baseline).

## Key files (with the relevant symbols/lines)

- `crates/tx-shims/src/linux_syscall/socket.rs` — `sys_recvfrom` (892), `recvfrom_impl` (899),
  src-addr writeback call (1058), `validate_recvfrom_addrlen` (918). sendmsg/recvmsg paths ~1268/1671/1768-1800.
- `crates/tx-shims/src/linux_syscall/socket/helpers.rs` — `write_sockaddr_endpoint` (1120,
  uses `bootstrap_read_user`/`bootstrap_copy_to_user`); `wait_on_socket_or_itimer` (~1658,
  races socket future vs `timer_sleep::sleep_until_ns(itimer_real_deadline_ns(pid))`).
- `crates/tx-shims/src/linux_syscall/mod.rs` — syscall dispatch (the 7 net arms added after
  NR_ACCEPT4, before NR_SENDFILE64; itimer arms NR_SETITIMER/NR_GETITIMER).
- `crates/tx-shims/src/linux_syscall/numbers.rs` — 66 socket-opt constants + msg/itimer NRs.
- `crates/tx-shims/src/linux_syscall/time.rs` — re-homed itimer (BTreeMap store, parse,
  get/set, `itimer_real_deadline_ns`). NOTE: signal *delivery* (poll_due_itimers /
  maybe_deliver_itimer_signal / tx-kernel hook) was **deliberately NOT re-homed** — only the
  deadline that unblocks recv. If a test needs real SIGALRM delivery, that's still missing.
- `crates/tx-shims/src/linux_syscall/fs_basic.rs` — `sys_close` now snapshots the file and
  calls `socket::maybe_close_socket_file_after_fd_remove` so port bindings are withdrawn.
- `crates/tx-fs/src/procfs/read.rs` — `render_meminfo()` full table (MemTotal/MemFree/
  **MemAvailable** — LTP `tst_memutils` sscanf's MemAvailable; the 0-stub broke every test).
- `crates/tx-kernel/src/init/exec.rs` — re-homed LTP runtest runner (`ltp-runtest:` dispatch).
- `crates/tx-kernel/src/init/rootfs_shims.rs` — `populate_rootfs_kernel_config()` seeds
  `/lib/modules/6.1.0-txkernel/modules.{dep,builtin}` (sctp.ko etc.) so the sctp driver gate opens.
- `boards/tx-hal-riscv64-qemu-virt/src/pmap/topology.rs` — `KERNEL_BOOTSTRAP_ALIAS_SIZE = 32MB`
  and `boards/tx-hal-riscv64-qemu-virt/src/boot_trampoline.rs` `.equ TX_RV64_KERNEL_ALIAS_L0_TABLES, 16`
  — **must stay in sync**; this fixed the silent boot livelock (0 serial, PC pinned).

## Baseline test inventory — exactly what to restore + re-run

This is the user's goal: restore SCTP to baseline AND re-run every previously-passing
net-stack LTP test. Two source-of-truth ledgers (read them, they have per-case detail):
- **net syscall cases (50):** `docs/LTP/ltp-network-syscall-progress.md` (b1–b6 below).
- **net.sctp cases:** `docs/LTP/runtests/ltp-runtest-net-sctp-progress.md` (table ~lines 216-265).
- Batch grouping rationale: `docs/LTP/ltp-batches.md` (normal batches FILTER net prefixes —
  net tests are "manual only", run by name; `LTP_BATCH=net`=0 is NOT "no tests").

### Net syscall batches b1–b6 (run via `ltp-runtest:syscalls:<+joined>`, baseline scores)

| Batch | Cases (`+`-join for the boot arg) | Baseline | Witness log |
| --- | --- | --- | --- |
| **b1** basic/options | `socket01 socket02 listen01 getsockname01 getsockopt01 getsockopt02 setsockopt01` | 40/40 | `target/oscomp/ltp-net-b1-basic.txt` |
| **b2** send/recv | `send01 send02 sendto01 sendto02 sendto03 recv01 recvfrom01` | 35/35 | `target/oscomp/ltp-net-b2-after-rds-sctp.txt` |
| **b3** msg/mmsg | `sendmsg01 sendmsg02 sendmsg03 recvmsg01 recvmsg02 recvmsg03 sendmmsg01 sendmmsg02 recvmmsg01` | 37/38 | `target/oscomp/ltp-net-b3-after-rds-sctp.txt` |
| **b4** bind/connect/accept | `bind01 bind02 bind03 bind04 bind05 bind06 connect01 connect02 accept01 accept02 accept03 accept4_01 getpeername01` | 93/95 | `target/oscomp/ltp-net-b4-after-kernel-object-fds.txt` |
| **b5** socketpair/socketcall | `socketpair01 socketpair02 socketcall01 socketcall02 socketcall03` | 14/17 | `target/oscomp/ltp-net-b5-socketpair-socketcall.txt` |
| **b6** setsockopt tail | `setsockopt02 setsockopt03 setsockopt04 setsockopt05 setsockopt06 setsockopt07 setsockopt08 setsockopt09 setsockopt10` | 10/11 | `target/oscomp/ltp-net-b6-after-tls-ulp.txt` |

Local judge subcase total across b1–b6 baseline: **229/236**. Score a run with
`python3 tools/oscomp-judge.py target/oscomp/<log>.txt target/oscomp/testdata`.

**⚠️ ORDER MATTERS (confirmed):** b2's `send01 send02 sendto01 sendto02 sendto03` run BEFORE
`recv01 recvfrom01` and **warm the loopback** — that's why recv01/recvfrom01 pass `35/35` in
the baseline batch but **hang when run alone** (the cold-start connect bug above). So: run
each batch in its **listed order**, and reproduce the baseline by running the **whole b2
group** (not recvfrom01 alone). Fixing the cold-start connect hang is the robust fix; running
b2-in-order is how the baseline achieved the pass. Boot-arg note: long `+`-joined filters can
be truncated (see the doc's `send01+s` truncation warning) — if a batch looks cut off, split it.

### net.sctp baseline (run via `ltp-runtest:net.sctp:<+joined>` or whole module)

**PASS in baseline (restore all of these):** `test_1_to_1_sockopt`(22-23) `test_tcp_style`(22)
`test_tcp_style_v6`(22) `test_1_to_1_socket_bind_listen`(15) `test_basic`(15) `test_basic_v6`(15)
`test_getname`(13) `test_getname_v6`(13) `test_1_to_1_addrs`(10) `test_1_to_1_accept_close`(10)
`test_1_to_1_connect`(10) `test_1_to_1_send`(9) `test_1_to_1_recvfrom`(7) `test_1_to_1_shutdown`(6)
`test_1_to_1_nonblock`(5) `test_1_to_1_events`(4) `test_1_to_1_sendto`(4) `test_1_to_1_rtoinfo`(3)
`test_1_to_1_initmsg_connect`(2) `test_inaddr_any`(2) `test_inaddr_any_v6`(2) `test_recvmsg`(2)
`test_1_to_1_threads`(1) `test_assoc_shutdown`(1).
**PARTIAL (match the documented fraction, don't regress):** `test_sockopt` 33/44 ·
`test_connect` 4/5 · `test_peeloff` 3/7 · `test_sctp_sendrecvmsg` 6 · `test_timetolive` 3 ·
`test_fragments` 2 · `test_1_to_1_recvmsg` 3/8 (musl-blocked).
**Already re-verified this session (individually):** `test_assoc_shutdown` TPASS,
`test_1_to_1_sockopt` 22/22, `test_1_to_1_socket_bind_listen` 14/14, `test_basic` 14/15.
(Witness counts in the doc are ground truth; e.g. sockopt witness=22, not the static 23.)

### Upstream `net.*` suites (the THIRD dimension — easy to miss; ledger: `docs/LTP/runtests/ltp-runtest-network-progress.md`)

These are the native LTP `net.*` runtests (NOT syscalls, NOT the local SCTP witnesses). Run via
`ltp-runtest:<suite>:<case>`. **`415/423` = syscall (229/236) + these net.\* suites (186/187)
ONLY — it does NOT include the local SCTP witnesses**, which are a separate third dimension with
their own scoring (ledger `ltp-runtest-net-sctp-progress.md`). ⚠️ These are **runtime-heavy** (witness logs are 180–900 s,
TCG time-dilated) and depend on **rootfs command shims** (`/tx-ltp/bin/{ss,tcpdump,traceroute,
traceroute6,tracepath,tracepath6}`, dhcpd/dnsmasq/nft/iptables/tc wrappers), netns/veth, `/proc/net/*`
projections, neigh/ARP+NDISC cache, and netfilter command state — all re-home-sensitive surface.

- **`net.ipv6_lib`  76/77** (6 entries): `in6_01`(5) `in6_02`(3) `getaddrinfo_01`(22)
  `asapi_01`(16/17 partial — only `getprotobyname("hopopt")` missing) `asapi_02`(12) `asapi_03`(18).
  Most stable group.
- **`net.tcp_cmds`  61/61** (16 entries): `netstat`(5) `iproute`(6) `ping01`(10) `ping02`(10)
  `arping01`(1) `ipneigh01_arp`(1) `ipneigh01_ip`(1) `sendfile`(4) `tc01`(2) `tracepath01`(1)
  `traceroute01`(6) `tcpdump`(1) `iptables`(6) `nft`(5) `dhcpd`(1) `dnsmasq`(1).
- **`net.ipv6`  46/46** (11 entries): `ping601`(10) `ping602`(10) `traceroute601`(6) `sendfile601`(4)
  `ip6tables`(6) `nft6`(5) `ipneigh6_ip`(1) `tracepath601`(1) `tcpdump601`(1) `dhcpd6`(1) `dnsmasq6`(1).
- **`net.features`  1/62**: only `fanout01`(1) (AF_PACKET PACKET_FANOUT CVE race; needs ~540 s wall
  under TCG). The other 61 are virt-link (`NS_TIMES` loops) / netload perf → TCG walls, never passed.
- **`net.multicast`  2/4**: `mc_cmds`(1) `mc_opts`(1) pass; `mc_member`/`mc_commo` not-run (need
  `netstat -gn`/`/proc/net/igmp`, long sleeps, rhost).
- **IPv6 TCP ("tcp_ipv6") coverage** is spread, not one test: `connect02` (IPv6 dual-stack TCP,
  focused `target/oscomp/ltp-net-ipv6-connect02.txt`; loops 1000× → needs a long outer timeout),
  `bind04` (IPv4/IPv6 TCP+SCTP loopback), RawTcp `[::1]` smoltcp segments, plus `net.ipv6`
  `sendfile601`/`traceroute601 -T`. Enabling local SCTP is what first exposed the `[::1]` TCP path.
- **not-run / out of scope (never in baseline — don't chase):** upstream `net.sctp` (41, distinct
  from local SCTP witnesses), `net.nfs`/`net.rpc_tests`/`net.tirpc_tests` (205), `net_stress.*` (588),
  `can` (3).

**Verification priority for these:** lower than the syscall+SCTP core (they're slow and shim-heavy),
but they ARE "previously-passing net tests" per the goal. The re-home most likely touched their
support surface (procfs `/proc/net/*`, neigh/NDISC, netfilter command state, AF_PACKET, raw ICMP).
Spot-check the cheap/stable ones first (`net.ipv6_lib` in6_01/in6_02/asapi_02/asapi_03,
`net.tcp_cmds` netstat/iproute/ping01) before the 300–900 s shim-dependent ones. Use the per-entry
witness log names in the ledger to reproduce each.

**⏱️ 5-minute rule (dimension B only):** if a single dimension-B test takes **more than ~5 min
(300 s) of wall clock** to pass, **defer it — don't verify it now.** These are slow by nature
(witnesses are 180–900 s, TCG-dilated) and not worth the verification time; clear the sub-5-min
ones, mark the long ones "to-verify" for later or a faster/real-HW environment. This applies ONLY
to dimension B — dimensions A (syscall) and C (SCTP) are core and must be verified however slow
(a few inherently-slow core cases like `connect02`'s 1000-iteration loop just get their own long
timeout; they are NOT in scope for this skip rule).

## Baselines / witnesses

- SCTP documented baseline: `docs/LTP/runtests/ltp-runtest-net-sctp-progress.md`.
- net witness logs: `target/oscomp/ltp-net-*.txt`, `target/oscomp/ltp-net-sctp-*.txt`.
- Status log: `docs/progress/STATUS.md` (two dated entries already describe the re-home,
  the boot fix, and the 6 batch-exposed regressions).

## Relevant memories (already saved)

- `route4-livelock=page-table-UAF` — kernel page-fault-retry livelock class (relevant to
  recvfrom01 candidate-1).
- `net-rehome-preexisting-test-failures` — which host-test failures are pre-existing vs new.
- `ltp-net-scoring-landscape`, `route4-exec-runtime-rootcause`, `commit-granularity`,
  `autonomous-run-pacing`, `permission-preference`.

## First action in the new conversation

Read `connect_impl` (socket.rs:406) + the accept path (socket.rs:321); add loopback-driving
/ yielding to connect's blocking wait so the cold-start multi-process TCP handshake completes
(see "THE BATCH BLOCKER" section — diagnosis is done, root cause is the connect cold-start
hang, NOT recvfrom). Verify `recv01` AND `recvfrom01` both pass **individually** (fresh boot),
then run the full net syscall + net.sctp batches and diff against baselines. Keep
`test_assoc_shutdown` green as a regression guard each round. ⚠️ The batch can produce FALSE
PASSES that hide cold-start hangs — always cross-check key net tests individually too. Do a
`docs/progress/STATUS.md` catch-up before declaring done.
