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

## THE BATCH BLOCKER — recvfrom01 (fix this first; it stalls the whole batch)

The net syscall batch runs alphabetically and **hangs at `recvfrom01`**, blocking every
test after it. Root-cause analysis already done:

- `recvfrom01.c` and `recv01.c` are the **same old-style multi-process LTP test**:
  `start_server()` → `tst_fork()` → child `do_child()` does `accept()` + `write(newfd,"hoser\n",6)`;
  the parent's `setup1()` does `connect()` then **`poll()`s up to 2 s** for that data
  ("Wait for something to be readable, else we won't detect EFAULT").
- **`recv01` PASSES all cases** with this identical architecture → fork, TCP loopback,
  connect, accept, data delivery, poll-with-timeout, blocking recv, and
  `EFAULT`-on-bad-buffer **all work**.
- recvfrom01 prints case 1 + case 2 TPASS (testno 0,1 = `setup0`, bad-fd / non-socket — no
  connect) then **hangs at case 3 = testno 2 = the first `setup1` case**. The case table:
  - testno 2: `from=(struct sockaddr*)-1`, salen=&fromlen, retval 0, ENOTSOCK, "invalid socket buffer"  ← **HANGS HERE**
  - testno 3: fromlen=-1, EINVAL, "invalid socket addr length"
  - testno 4: buf=(void*)-1, EFAULT, "invalid recv buffer"
  - testno 5/6: MSG_OOB EINVAL / MSG_ERRQUEUE EAGAIN
- The only thing testno 2 adds over recv01 is the **bad source-address out-pointer
  `from=(sockaddr*)-1`** (= `0xffffffffffffffff`), written back by
  `write_sockaddr_endpoint(ctx, args[4], args[5], source)` at
  `crates/tx-shims/src/linux_syscall/socket.rs:1058`.
  That fn (`socket/helpers.rs:1120`) uses **faultable `bootstrap_copy_to_user`** — and
  recv01 proved that path returns clean `EFAULT` for `(void*)-1`. So the writeback alone
  "should" not livelock.

**Two candidate causes — disambiguate empirically before coding:**
1. **(addr-writeback fault)** `bootstrap_copy_to_user` to `0xffff…` livelocks specifically
   when reached *after* a successful recv (vs recv01's pre-recv buffer copy), echoing the
   known page-fault-retry livelock class (see memory `route4-livelock=page-table-UAF`).
2. **(blocking recv)** the recvfrom *blocks* waiting for "hoser\n" because in the
   recvfrom01 timing/ordering the data isn't delivered, and the framework's watchdog
   (now uses the re-homed `setitimer`/itimer deadline → `wait_on_socket_or_itimer`) only
   wakes at the framework timeout, not 2 s.

**Diagnostic step (do this first):** run recvfrom01 **alone** with a ~300 s timeout and
capture serial:
```
cp target/riscv64gc-unknown-none-elf/debug/tx-kernel-riscv64-qemu-virt target/oscomp/submit/kernel-rv
timeout 320 make oscomp-qemu-rv64 OSCOMP_GROUPS=ltp-runtest:syscalls:recvfrom01 > /tmp/rf.log 2>&1
grep -nE "TPASS|TFAIL|TBROK|no message ready|recvfrom" /tmp/rf.log
```
- If it prints **"client setup1 failed - no message ready in 2 sec" (TBROK)** → it's the
  **poll/data-delivery** path (candidate 2). Compare recvfrom01's setup1 vs recv01's setup1
  for any real difference; check multi-process loopback timing.
- If it **hangs silently** in the recvfrom call (no TBROK) → it's the **recvfrom-specific
  addr-writeback** (candidate 1). Fix: validate/short-circuit the `from` pointer, or make
  the writeback's fault return `EFAULT` without retrying. Inspect `recvfrom_impl`
  (`socket.rs:899`) ordering: it should be safe to write the src addr to a bad pointer and
  get EFAULT, same as recv's buffer copy.
- If it eventually unblocks at ~framework-timeout → confirms candidate 2 + itimer is the
  only thing waking it (still a FAIL; recvfrom must not block here).

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

Run the recvfrom01 disambiguation command above, read the serial log, decide candidate 1 vs
2, fix it, re-run recvfrom01 to green, then continue the full net + SCTP batch comparison
against the baselines. Keep `test_assoc_shutdown` green as a regression guard each round.
Do a `docs/progress/STATUS.md` catch-up before declaring done.
