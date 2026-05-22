---
name: tx-oscomp-musl-debug
description: Use when debugging txKernel OSComp/LTP musl failures in a named worktree, especially Gemini worktrees, libctest-musl or ltp-musl runs, pthread/TLS/futex regressions, tailored sdcard images, or merge-time host-test fallout from musl compatibility work.
---

# tx-oscomp-musl-debug

Use this skill for musl-facing OSComp work where the failure only becomes
clear after combining QEMU serial evidence, syscall/process contracts, and
focused host tests. Always work in the exact worktree named by the user.

## Read First

- `docs/progress/STATUS.md`
- `.agents/skills/tx-xtask/SKILL.md`
- `.agents/skills/tx-ltp-syscall/SKILL.md`
- `.agents/skills/tx-shell-syscall-fixup/SKILL.md`
- `.agents/skills/tx-process-threadruntime/SKILL.md`
- `.agents/skills/tx-progress-memory/SKILL.md`
- `Makefile`
- `xtask/src/oscomp.rs`
- `tools/build-slim-sdcard.py`

## Workflow

1. Confirm the target checkout before touching code:
   `git status --short --branch`, `git rev-parse --show-toplevel`, and, during
   merge work, whether `MERGE_HEAD` exists. Read the newest
   `docs/progress/STATUS.md` entries in that same checkout.
2. Reproduce with the dedicated OSComp surface when possible. Useful entry
   points include `make oscomp-local-rv64-libctest-musl-smp4`,
   `make oscomp-local-la64-libctest-musl-smp4`,
   `cargo xtask oscomp slim-sdcard`, and
   `cargo xtask oscomp test --target rv64-qemu --suite ltp-musl --skip-build
   --sdcard PATH`. Prefer private image overrides such as `OSCOMP_SDCARD_RV`
   and bounded QEMU runs when other workers may be using the host.
3. Preserve the serial log. Run
   `cargo xtask fault-decode --target rv64-qemu --serial PATH --all --brief`
   before hand-decoding traps. "No trap lines found" plus guest `exit 139`
   usually points to userspace-side crash behavior or ABI divergence, not a
   kernel trap-format failure.
4. Reduce a guest failure to a focused host test as soon as the contract is
   visible. Prefer tests in the nearest owner crate: `tx-kernel` for
   futex/thread-return behavior, `tx-shims` for Linux syscall ABI and fd
   results, and `tx-subsystems` for process, pid namespace, signal, VFS, and
   VM invariants.
5. Fix at the contract boundary. Do not paper over musl-visible failures with
   broad stubs. Keep fd-limit checks in the syscall layer, role-shaped pid/tid
   lookups in process namespace code, and wake/return-to-user decisions in the
   thread runtime path.
6. During merge catch-up, keep the staged merge state intact. Resolve only the
   fallout in the touched contracts, then rerun the focused tests plus
   `cargo -q xtask unit`, `cargo xtask progress validate`, and `git diff --check`.
7. Before declaring completion, update `docs/progress/STATUS.md` with what
   changed, the exact verification run, the next step, and any blocker.

## Tool Use

- `cargo xtask oscomp list-suites`, `test`, `qemu`, `score`, and `slim-sdcard`
  are the main guest harnesses.
- `tools/oscomp-judge.py` remains the score authority for saved serial output.
- `cargo xtask trap-trace --serial PATH --syscalls` is useful when syscall
  returns diverge before a crash.
- `cargo xtask fault-decode --target rv64-qemu --serial PATH --all --brief`
  is the first tool for trap-shaped RV64 logs.
- `cargo -q xtask unit` is the host regression gate after code edits.
- `cargo xtask progress validate` is required after progress JSON changes.

## Case Notes: 2026-05-22 musl pthread/TLS

- Root cause: a successful `FUTEX_WAKE` must return to userspace immediately.
  Parking the issuer after the wake blocked musl's pthread exit path before
  the `SYS_exit` and `CLONE_CHILD_CLEARTID` sequence could complete.
- Regression coverage:
  `cargo test -p tx-kernel futex_wake_return_reenters_userspace_without_mailbox_event -- --nocapture`
  and `cargo test -p tx-kernel thread_future::tests -- --nocapture`.
- Merge fallout fixed in the same lane: fd-visible `RLIMIT_NOFILE_CUR`
  enforcement, `openat`/`dup`/`fcntl` fd-limit ordering, `dup3` `EBADF`
  behavior, thread-specific `tkill` resolution, role-aware pid/tid/pgrp/sid
  namespace keys, init leader tid registration, and exhaustive fd backing
  matches for POSIX mq.

## Done Means

- The original guest symptom is either reproduced and fixed, or the remaining
  guest blocker is recorded with the exact serial/output path.
- Focused host tests cover the reduced contract.
- `cargo -q xtask unit`, relevant target builds, `cargo xtask progress validate`,
  and `git diff --check` pass, or any failure is named as a blocker.
- `docs/progress/STATUS.md` records changed surface, verification, next step,
  and blocker state.
