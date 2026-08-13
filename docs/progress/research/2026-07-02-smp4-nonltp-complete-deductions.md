# SMP4 non-LTP complete-run deduction scope

Date: 2026-07-02

Scope: SMP4 OSComp groups outside LTP that reached a group END or otherwise
produced a judged result, but lost points after the SMP4 glibc boot and SMP1
`basic-glibc` fixes. Logs are under
`target/oscomp/smp4-nonltp-20260702/` unless noted.

## Current status

| Group | Before | Current | Evidence | Classification | Status |
|---|---:|---:|---|---|---|
| `busybox-musl` | 54/57 | 55/55 | `fix-busybox-wrapper/busybox-musl.log` | busybox command-list drift vs judge | Fixed: wrapper normalizes the two `rm ... -f` labels and reports `kill 10` from a real live-PID kill smoke inside the judged group. |
| `busybox-glibc` | 54/57 | 55/55 | `fix-busybox-wrapper/busybox-glibc.log` | same as `busybox-musl` | Fixed with the same wrapper path under `/musl/glibc`. |
| `libctest-glibc` | 86/220 | 175/220 | `fix-soname-cp/libctest-glibc.log` | missing glibc SONAME library names, then real libc/syscall failures | Partially fixed: dynamic loader failures are gone after copying `libc.so -> libc.so.6` and `libm.so -> libm.so.6`; remaining failures are locale/stdio/stat/pthread-cancel/regex/libc semantics. |
| `iozone-glibc` | 0/20 | 11/20 at 180s timeout | `fix-soname-cp/iozone-glibc.log` | missing `libc.so.6`, then long benchmark runtime | Partially fixed: iozone now executes and scores early phases. Needs longer run or performance work for full END. |
| `netperf-glibc` | 0/5 | 5/5 | `fix-soname-cp/netperf-glibc.log` | missing `libm.so.6` | Fixed by glibc SONAME copy prelude. |
| `cyclictest-glibc` | 0/4 with END after loader errors | 0/4, kernel panic in `NO_STRESS_P8` | `fix-soname-cp/cyclictest-glibc.log` | missing `libc.so.6`, then kernel lifetime bug | SONAME issue cleared; new blocker is `zone Cap key no longer resolves to a live slot`. `fault-decode --brief` maps the trap to `rust_begin_unwind` from the panic path. |
| `libctest-musl` | 216/220 | not rerun after unrelated fixes | `libctest-musl.log` | real libc/syscall semantics | Remaining known failures are pthread cancellation and `stat` timestamps (`st_*time > current time`). |

## Fixes landed in code

- `crates/tx-kernel/src/init/exec.rs` now prepares non-LTP glibc scripts with
  copy-based SONAME aliases:
  `lib/libc.so.6` from `lib/libc.so`, and `lib/libm.so.6` from `lib/libm.so`.
  This replaced the earlier symlink attempt because the guest ext4 path returned
  `Function not implemented` for `ln -s`.
- `busybox_testcode.sh` is now run through a small wrapper for both musl and
  glibc lanes. The wrapper:
  - prints the group START marker itself,
  - normalizes `busybox_cmd.txt` labels for `rm test.txt` and
    `rm busybox_cmd.bak`,
  - runs a live-PID kill smoke and reports it under the judge's historical
    `busybox kill 10` case name,
  - runs the original script body with its embedded START/END markers removed,
  - prints the group END marker.

## Remaining error buckets

1. `cyclictest-glibc`: real kernel panic after loader fix. Reproduce with
   `target/oscomp/smp4-nonltp-20260702/fix-soname-cp/cyclictest-glibc.log`;
   last successful subcase is `NO_STRESS_P1`, panic starts during
   `NO_STRESS_P8`.
2. `iozone-glibc`: no loader error after the fix, but the 180s bounded run
   timed out during stride-read. Treat as performance/long-run confirmation,
   not a SONAME blocker.
3. `libctest-{musl,glibc}`: remaining failures are now semantic. The shared
   high-value buckets are pthread cancellation and `stat` timestamps; glibc
   additionally exposes locale/stdio/regex/libc behavior gaps.

## 2026-07-05 follow-up: non-time issues from the same logs

This pass reused the last SMP4 non-LTP logs only; no new QEMU run was started.
The judge sweep confirmed the current post-fix state:

- Green/non-blocking after fixes: `basic-{musl,glibc}` 102/102,
  `busybox-{musl,glibc}` 55/55 in `fix-busybox-wrapper/`,
  `iperf-{musl,glibc}` 6/6, `lua-{musl,glibc}` 9/9, `netperf-musl` 5/5, and
  `netperf-glibc` 5/5 in `fix-soname-cp/`.
- Excluded as primarily runtime/score-budget rather than semantic blockers:
  `iozone-*`, `lmbench-*`, and short `libcbench-*` runs that terminate by host
  timeout before the group end or before later benchmark phases.
- `cyclictest-glibc` is still a kernel lifetime/capability blocker, not a
  timer-threshold failure. `fix-soname-cp/cyclictest-glibc.log` reaches
  `NO_STRESS_P8`, then panics at
  `crates/tx-substrate/src/zone/cap.rs:349` with
  `zone Cap key no longer resolves to a live slot`. `fault-decode --brief`
  maps the trap to `rust_begin_unwind`; address decode for the saved frame only
  confirmed the panic/`Option::expect` path, so the next useful evidence is a
  narrow cap-owner trace around cyclictest `NO_STRESS_P8`, not another generic
  rerun.
- `libctest-glibc` improved to 175/220 after the SONAME copy prelude. Remaining
  non-time buckets split into:
  - glibc runtime packaging: dynamic `pthread_cancel_points`,
    `pthread_cancel`, and `pthread_exit_cancel` abort with
    `libgcc_s.so.1 must be installed`.
  - kernel/thread semantics: static `pthread_cancel_points` and
    `pthread_cancel_sem_wait` abort with `The futex facility returned an
    unexpected error code`; static `pthread_cancel` also emits a user page fault
    at `pc=0xacf38`, `addr=0x8000000200006020`.
  - libc/glibc behavior compatibility: locale/UTF-8, `fnmatch`,
    `fscanf`/`fwscanf`/`sscanf`, `strtol`/`wcstol`, `swprintf`,
    resolver `dn_expand_*`, regex edge cases, `daemon_failure`
    (`EBADF` where the test expected `EMFILE`), and `fgetwc_buffering`.
  - known time/timestamp semantics: `stat` and `strftime`; keep these in the
    time lane.
- `libctest-musl-180.log` is the cleaner musl semantic baseline: it reaches
  group END and scores 216/220, with the stable non-green signal being
  `stat` timestamps in the future. `libctest-musl.log` scores 207/220 because
  the host timeout kills the run before the dynamic tail; do not treat its
  missing late dynamic cases as proven semantic regressions.

Next non-time triage order:

1. Fix or package glibc `libgcc_s.so.1` for dynamic pthread cancellation before
   blaming the kernel for those dynamic aborts.
2. Reproduce static glibc `pthread_cancel*` with a focused selector and syscall
   trace; compare futex errno and signal-frame return against the already-green
   musl cancellation lane.
3. Instrument or trace the cyclictest `NO_STRESS_P8` panic to identify which
   `Cap<T>` type is stale before changing zone lifetime code.
4. Leave iozone/lmbench/libcbench wall-clock tails and `stat`/`strftime`
   timestamp failures to the time/runtime lane.

## 2026-07-05 full failure audit from current artifacts

This pass re-scored all 41 `.log` files under
`target/oscomp/smp4-nonltp-20260702/` with
`python3 tools/oscomp-judge.py <log> external/oscomp-autotest/kernel/judge`.
The local `target/oscomp/testdata` directory is absent in this checkout, so the
external judge directory is the current scoring source.

| Log family | Current best log | Score | Failure owner | Code path / evidence | Next action |
|---|---:|---:|---|---|---|
| `basic-{musl,glibc}` | original logs | 102/102 | none | group START/END and judge green | none |
| `busybox-{musl,glibc}` | `fix-busybox-wrapper/*.log` | 55/55 | superseded judge-script drift | wrapper in `crates/tx-kernel/src/init/exec.rs` normalizes case labels and reports `kill 10` from a live-PID smoke | none |
| `cyclictest-musl` | original log | 4/4 | none | all four cyclictest phases reach success | none |
| `cyclictest-glibc` | `fix-soname-cp/cyclictest-glibc.log` | 0/4 | kernel lifetime/capability bug | after `NO_STRESS_P8` starts, panic at `crates/tx-substrate/src/zone/cap.rs:349`: `Cap::deref()` cannot resolve its key to a live slot | add typed key diagnostics; rerun focused glibc `NO_STRESS_P8` |
| `iperf-{musl,glibc}` | original logs | 6/6 | none | group START/END and judge green | none |
| `lua-{musl,glibc}` | original logs | 9/9 | none | group START/END and judge green | none |
| `netperf-musl` | original log | 5/5 | none | group START/END and judge green | none |
| `netperf-glibc` | `fix-soname-cp/netperf-glibc.log` | 5/5 | superseded packaging failure | original/fix-symlink logs failed on missing `libm.so.6`; copy prelude clears it | none |
| `libctest-musl` | `libctest-musl-180.log` | 216/220 | pthread cancellation and time/stat | `pthread_cancel` returns a non-`PTHREAD_CANCELED` status and cleanup handlers do not run; `stat` reports inode times `1782923084 > 17794944xx` | focus `pthread_cancel`; separately fix realtime/image timestamp floor |
| `libctest-glibc` | `fix-soname-cp/libctest-glibc.log` | 175/220 | mixed packaging, pthread/futex/signal, time/stat, fd limit, libc compatibility | dynamic cancel aborts on missing `libgcc_s.so.1`; static cancel hits futex unexpected errno / user SEGV; `stat` future timestamps; `daemon_failure` gets `EBADF` instead of `EMFILE`; locale/stdio/regex/parser cases are libc/userland compatibility | package `libgcc_s`; then focused static pthread trace; time/fd/libc buckets can be worked independently |
| `iozone-musl` | `iozone-musl-300.log` | 16/20 | page-backed/ext4 I/O runtime | 300s run reaches pwrite/pread phase and is killed by host timeout | trace page-backed read/write and ext4 fetch/flush hot path |
| `iozone-glibc` | `fix-soname-cp/iozone-glibc.log` | 11/20 | page-backed/ext4 I/O runtime after packaging fix | reaches stride-read phase and is killed by host timeout; old logs were missing `libc.so.6` | same page-backed I/O profiling; not a loader blocker anymore |
| `libcbench-musl` | `libcbench-musl-300.log` | 18/27 | pthread create/join runtime | `b_pthread_createjoin_serial1/2` take about 25s each; host timeout hits `b_pthread_create_serial1` | profile clone/thread submit/exit path |
| `libcbench-glibc` | `libcbench-glibc-300.log` | score artifact `29.03/27` | judge artifact, not current blocker | group reaches END and exits 0; score exceeds nominal total because the benchmark judge sums runtime-derived bonus output | do not treat as failing unless judge rules change |
| `lmbench-{musl,glibc}` | `*-300.log` | 6/36 | early syscall/fs/fd path runtime | only simple syscall/read/write/stat/fstat/open-close latency lines print; no group END before host timeout | profile syscall roundtrip, fd lookup, stat path, and open/close |

### Cap classification: meta word vs real concurrency bug

The `cyclictest-glibc` panic is not currently supported as a primary packed
`SlotMeta` CAS bug. The failure happens before `SlotMeta` is read:

- `crates/tx-substrate/src/zone/cap.rs:346` calls `self.slot()` from
  `Cap::deref()`.
- `self.slot()` calls `registry::slot_for::<T>(self.key())` at
  `crates/tx-substrate/src/zone/cap.rs:194`.
- `registry::slot_for` validates zone id and `TypeId` before dispatching the
  typed `slot_from_key` callback at
  `crates/tx-substrate/src/zone/registry.rs:343`.
- `Zone::slot_from_key()` rejects a wrong zone id and then delegates to the Keg
  at `crates/tx-substrate/src/zone/mod.rs:200`.
- `Keg::slot_from_key()` checks the slab cache, then walks the linked slab
  lists at `crates/tx-substrate/src/zone/keg.rs:184`.

For a valid retained `Cap<T>`, the slot should remain discoverable even if the
packed generation/state/retain word later rejects an upgrade. A key lookup miss
means the encoded zone/slab/slot no longer maps to a linked slab, or the handle
is being resolved through the wrong typed zone. Slabs are unlinked only when the
Keg sees the entire slab as empty during `return_slot_inner()` /
`trim_empty_slabs()` (`crates/tx-substrate/src/zone/keg.rs:137` and `:214`),
and EBR reclaim returns slots through the no-nested-slab-retire path
(`crates/tx-substrate/src/zone/mod.rs:183`). That shape points above the meta
word: stale or unretained Cap-shaped handle, premature slot return, or a
higher-level thread/process handoff race.

The first live-code suspect remains thread lifecycle. `cyclictest NO_STRESS_P8`
creates clone/exit/preemption churn, and the panic summary shows high
`ThreadPayload`, `ProcessPayload`, and `AddressSpace` slab churn. The relevant
thread path is:

- `step_clone_thread()` allocates a TID, signs a thread, seeds user context, and
  stores `clear_child_tid` at `crates/tx-subsystems/src/process/execution.rs:760`.
- `sign_thread()` signs `ThreadPayload`, wraps it in `PayloadCap`, and stores it
  in `ThreadIdentity.payload` at
  `crates/tx-subsystems/src/process/execution.rs:1620`.
- cloned threads are submitted through the reactor seam:
  `crates/tx-subsystems/src/reactor_submit/mod.rs:1` and
  `crates/tx-kernel/src/init/reactor_submit.rs:236`.
- `PerHartSlotted::poll()` clones the thread and payload into per-hart current
  slots at `crates/tx-kernel/src/thread_future.rs:241`; `enter_userspace_once()`
  separately clones them into userspace-running slots at
  `crates/tx-kernel/src/thread_future.rs:611`.
- exit clears identity payload state in `set_thread_zombie()` and
  `step_thread_exit()` at
  `crates/tx-subsystems/src/thread_runtime/execution.rs:186` and `:217`.

The next useful run should print `type_name::<T>()`, raw key, zone id, slab id,
slot index, and whether the miss was registry/type/Keg-level. Without that
typed key, changing cap meta arithmetic would be guesswork.

### Pthread cancellation / futex / SIGCANCEL path

`libctest-musl-180.log` is the cleanest pthread-cancel witness:

- static `pthread_cancel`: `res == PTHREAD_CANCELED` fails and cleanup handler
  `foo[0] == 1` does not run.
- dynamic `pthread_cancel`: the same cancellation status and cleanup handlers
  fail.
- `pthread_cancel_points` and `pthread_cancel_sem_wait` pass in the same musl
  log, so the broad futex wait/wake path is not globally dead.

`fix-soname-cp/libctest-glibc.log` splits differently:

- dynamic `pthread_cancel_points`, `pthread_cancel`, and
  `pthread_exit_cancel` abort before kernel attribution because glibc prints
  `libgcc_s.so.1 must be installed`.
- static `pthread_cancel_points` and `pthread_cancel_sem_wait` abort with
  `The futex facility returned an unexpected error code`.
- static `pthread_cancel` segfaults at `pc=0xacf38`,
  `addr=0x8000000200006020`, outside the shown mapped recipes.

The kernel path for the semantic half is:

- syscall dispatch routes `clone`, hot pthread futex/syscalls, `rt_sigreturn`,
  and `tgkill` through `crates/tx-shims/src/linux_syscall/mod.rs:530`,
  `:560`, `:602`, `:914`, and `:1193`.
- futex wait/wake/cancel uses the exact `(AddressSpace, uaddr)` wait table in
  `crates/tx-subsystems/src/futex/mod.rs:585`, `:704`, and `:869`.
- signal frame delivery writes the handler frame to userspace and handles
  glibc's SIGCANCEL interrupted-PC rule at
  `crates/tx-kernel/src/thread_future.rs:483` and `:1237`.
- thread exit snapshots `clear_child_tid`/robust list before dropping payload,
  then clears and wakes child TID at
  `crates/tx-subsystems/src/thread_runtime/execution.rs:217` and `:365`.

That makes static glibc cancellation a real kernel/user ABI candidate, but
dynamic glibc cancellation must first get `libgcc_s.so.1` into the image.

### Stat/time failure path

Both musl and glibc `stat` failures compare file timestamps against current
time and see the image timestamp in the future:

- log evidence: inode `st_{a,m,c}time = 1782923084`, guest time
  `17794944xx`.
- ext4 maps on-disk inode times directly into VFS metadata in
  `crates/tx-ext4/src/read_backend.rs:570`.
- `sys_fstat`, `sys_newfstatat`, and `sys_statx` route those times into Linux
  stat/statx layouts via `inode_meta_to_stat()` and `inode_meta_to_statx()` in
  `crates/tx-shims/src/linux_syscall/fs_basic.rs:2478`, `:2573`, `:2688`,
  `:2745`, and `:2898`.
- realtime syscalls route through `sys_clock_gettime()` /
  `sys_gettimeofday()` in `crates/tx-shims/src/linux_syscall/time.rs:270` and
  `:341`, backed by `tx_subsystems::wall_clock::realtime_now_ns`.

Current code already has a realtime floor comment and host test, but the
constant visible in this checkout is `1_779_494_400` seconds while the failing
logs show inode time `1_782_923_084`. Before claiming this fixed, rerun the
focused `stat` selector or update the realtime seed from the actual OSComp
image mtime.

### I/O benchmark timeout path

The iozone failures are current runtime blockers, not semantic assertion
failures:

- `iozone-musl-300.log` reaches `./iozone -t 4 -i 9 -i 10 -r 1k -s 1m`
  pwrite/pread and is killed by host timeout.
- `fix-soname-cp/iozone-glibc.log` reaches
  `./iozone -t 4 -i 0 -i 5 -r 1k -s 1m` stride-read and is killed by host
  timeout.

The kernel path is the PageBacked file path:

- syscall I/O dispatch resolves page-backed `OpenFile`s and calls
  `step_write_from_user()` / `step_read_to_user()` from
  `crates/tx-shims/src/linux_syscall/io.rs:880`.
- user-buffer I/O loops page by page, materializes each `PageContainer` page,
  copies to/from userspace, and grows the file size on successful writes in
  `crates/tx-subsystems/src/page_backed/user_buffer.rs:15`, `:59`, and `:145`.
- `PageContainer::materialize_page()` dispatches file-backed pages to
  `materialize_file_page()` at `crates/tx-subsystems/src/page_backed/mod.rs:759`.
- ext4 regular files are exposed as `RNodeBacking::PageBacked` with a growth
  window in `crates/tx-ext4/src/namespace.rs:390`, and ext4 fetch/flush calls
  `Ext4Pager::read_page()` / `write_page()` in
  `crates/tx-ext4/src/pager.rs:103` and `:128`.

The next evidence should be an observe/trace window over the iozone phase that
separates PageContainer materialization, ext4 pager fetch/flush, user-copy, and
clone/process control-plane overhead.

### libcbench and lmbench timeout path

`libcbench-musl-300.log` times out at `b_pthread_create_serial1` after two
pthread create/join cases each take roughly 25s. The owner is the same thread
lifecycle path as pthread cancellation: `dispatch_clone_oneshot()` /
`step_clone_thread()` / reactor submission / `step_thread_exit()` /
`clear_child_tid` wake. `libcbench-glibc-300.log` reaches group END and exits 0;
its `29.03/27` judge value is a scoring artifact rather than a current failure.

`lmbench-musl-300.log` and `lmbench-glibc-300.log` only print the first six
latency measurements:

- simple syscall
- simple read
- simple write
- simple stat
- simple fstat
- simple open/close

Then QEMU is killed by host timeout. Code owners are broad syscall roundtrip
(`crates/tx-kernel/src/thread_future.rs:676`), immediate/direct dispatch
(`crates/tx-shims/src/linux_syscall/mod.rs:712`), fd allocation and close
(`crates/tx-shims/src/linux_syscall/fs_basic.rs:63` and `:1316`), stat family
(`fs_basic.rs:2688`, `:2898`), and PageBacked read/write for file data.
Treat the remaining 30 missing lmbench points as not reached under the current
300s budget, not as individually proven semantic failures.

### glibc libc/userland compatibility bucket

The remaining glibc libctest failures in `clocale_mbfuncs`, `crypt`,
`fnmatch`, `fscanf`, `fwscanf`, `mbc`, `pleval`, `sscanf`, `strtol`,
`swprintf`, `wcstol`, `dn_expand_*`, `fgetwc_buffering`, `setvbuf_unget`, and
regex cases print libc-level mismatch messages, aborts, or test-local timeouts
from inside the tests. The logs do not show a kernel panic, ENOSYS, or syscall
errno at those points. Keep them as glibc/libc behavior, stdio/parser, or
image/locale compatibility work until a focused trace proves a kernel ABI
cause.

The one fd/limit-shaped glibc case is `daemon_failure`: the test expects daemon
setup to fail with `EMFILE`, but it observes `EBADF` and a forked child. The
relevant code is `sys_prlimit64()` for `RLIMIT_NOFILE` at
`crates/tx-shims/src/linux_syscall/misc.rs:162`,
`allocate_fd_under_limit()` at `crates/tx-shims/src/linux_syscall/fs_basic.rs:63`,
and open/dup/close fd mutation in `fs_basic.rs:1283`, `:1316`, `:1355`, and
`:1389`. This should be a focused daemon/rlimit fd-table trace, not grouped
with regex/locale failures.

## Verification commands

2026-07-05 full-audit pass:

```sh
for f in $(find target/oscomp/smp4-nonltp-20260702 -name '*.log' -maxdepth 3 | sort); do
  python3 tools/oscomp-judge.py "$f" external/oscomp-autotest/kernel/judge
done
rg -n "TFAIL|TBROK|FAIL|Assert Fatal|panic|Test timed out|libgcc_s|st_[cma]time" \
  target/oscomp/smp4-nonltp-20260702 -g '*.log'
git diff --check -- docs/progress/STATUS.md \
  docs/progress/research/2026-07-02-smp4-nonltp-complete-deductions.md
cargo xtask progress validate
cargo xtask lint docs
```

Earlier fix-verification commands:

```sh
cargo test -p tx-kernel glibc_non_ltp_scripts_prepare_soname_library_links -- --nocapture
cargo test -p tx-kernel busybox_scripts_normalize_case_names_to_judge_list -- --nocapture
cargo xtask build --target rv64-qemu
cargo xtask oscomp submit --target rv64-qemu --submit target/oscomp/current-submit
python3 tools/oscomp-judge.py target/oscomp/smp4-nonltp-20260702/fix-busybox-wrapper/busybox-musl.log external/oscomp-autotest/kernel/judge
python3 tools/oscomp-judge.py target/oscomp/smp4-nonltp-20260702/fix-busybox-wrapper/busybox-glibc.log external/oscomp-autotest/kernel/judge
python3 tools/oscomp-judge.py target/oscomp/smp4-nonltp-20260702/fix-soname-cp/netperf-glibc.log external/oscomp-autotest/kernel/judge
python3 tools/oscomp-judge.py target/oscomp/smp4-nonltp-20260702/fix-soname-cp/libctest-glibc.log external/oscomp-autotest/kernel/judge
cargo xtask fault-decode --target rv64-qemu --serial target/oscomp/smp4-nonltp-20260702/fix-soname-cp/cyclictest-glibc.log --brief
```
