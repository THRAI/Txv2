# Shell-prompt roadmap — 2026-05-07 progress

**Date:** 2026-05-07
**Plan:** [`docs/progress/plans/2026-05-07-shell-prompt-roadmap.md`](../plans/2026-05-07-shell-prompt-roadmap.md)
**Status:** Mid-flight. 8 of 11 slices landed in a single session;
slices 9–11 deferred (see rationale below).

## What landed

8 sequential slices on top of the fd-ops slice (which closed
2026-05-07 morning). Each on its own branch, each with a verbose
commit message documenting decisions and carryovers:

| # | Branch | Commit | Coverage delta |
|---|---|---|---|
| 1 | `feat/pipe-lifecycle-hook` | `b2d22f8` | +4 lifecycle tests; closes Wave 3 carryover |
| 2 | `feat/vm-syscalls` | `1af0729` | +16 mmap-family dispatch tests |
| 3 | `feat/futex` | `324fd3a` | +9 futex + 7 dispatch (musl libc unblock) |
| 4 | `feat/time-syscalls` | `f48f05f` | +15 clock/gettimeofday/times/nanosleep tests |
| 5 | `feat/ioctl` | `bf8bc70` | +13 TTY ioctl dispatch tests |
| 6 | `feat/stat-family` | `4b7fd12` | +18 stat/cwd/getdents/umask tests |
| 7 | `feat/fcntl-misc` | `f09e358` | +20 fcntl/kill/uname/prlimit/getrandom tests |
| 8 | `feat/file-mutation` | `2b7768c` | +25 file-mutation tests |

**Workspace test count:** 984 (pre-Slice-1) → 1111 (post-Slice-8).
**Net new tests:** ~127 across 8 slices.
**Net new syscall arms:** ~52 (more than doubling the pre-roadmap surface).
**Net new subsystems:** 1 (`tx_subsystems::futex`).

All slices verified: `cargo build --workspace --lib --tests` clean
(no warnings) + `cargo test --workspace --lib --tests
-- --test-threads=1` zero failures at every commit boundary.

## Decisions

- **Slice 2 (user-VA sweep) re-ordered to Slice 9.** Original plan put
  user-VA first; investigation showed no production RV64
  `UserAccessIf` impl exists, so migrating the 18
  `TODO(phase-userva)` markers without it would have just turned
  every user-pointer-using syscall into hard EFAULT against the
  bake-in fixture. Re-ordered to allow Slices 2–8 to ship under
  the existing bootstrap kernel-buffer exemption.
- **Slice 9 (user-VA sweep) deferred to a future infrastructure
  slice.** A proper RV64 `UserAccessIf` needs fixup-table consumer
  + trap-shell fault redirect + asm-level copy primitives. The
  HAL surface (`FixupEntry`, `UserAccessIf` default impl) exists
  but no consumer wires it. Multi-day infrastructure work that
  exceeded session budget.
- **Slices 10 + 11 deferred to a session with external deps in
  place.** Busybox bake-in needs `$TX_BUSYBOX` (a real
  static-musl-built busybox); QEMU shell smoke needs riscv64
  cross-toolchain + QEMU 7.x configured. Kernel-side code for
  Slice 10 is small (~50 LOC for `register_busybox_into_tmpfs` +
  build.rs). Final integration step is independent of further
  kernel work.
- **Carryovers tracked per-slice** in each commit message:
  - Slice 4: non-zero nanosleep needs timer-fire wiring (out of
    Slice 4 scope; future timer-channel slice).
  - Slice 6: `fchdir` needs DEntry hint on OpenFile (separate
    slice); `AT_SYMLINK_NOFOLLOW` needs walker-side support.
  - Slice 7: `F_SETFL` needs interior-mutable OpenFile flags;
    real per-thread signals; real `rt_sigreturn` against
    `SignalFrameIf::restore_signal_frame`.
  - Slice 8: `utimensat` needs `FsOps::set_times` hook;
    `RENAME_EXCHANGE` not implemented; tmpfs `link` already
    `-ENOSYS` (Phase 3b carryover).

## What's next

The two-step integration to actually print a shell prompt:

1. **Slice 10**: build.rs + `register_busybox_into_tmpfs` helper +
   conditional bake-in wiring. ~50 LOC kernel + build glue.
2. **Slice 11**: `cargo xtask qemu-shell-smoke` end-to-end test.
   Sends `echo hello\n`, watches for `hello`, watches for the
   `:userspace:exited:0` sentinel after `exit`.

Independently, the **proper Slice 9** (RV64 `UserAccessIf` + 18
`TODO(phase-userva)` migrations) lifts the kernel from
"works for bake-in busybox" to "works for any musl program." Not
required for the day-1 prompt deliverable, but required before LTP
serious coverage.

## Verification

- `cargo build --workspace --lib --tests`: clean.
- `cargo test --workspace --lib --tests -- --test-threads=1`: 1111 / 0.
- `cargo xtask progress validate`: ok.
- 8 commits on top of the fd-ops slice, each on its own branch,
  each verified independently.

## Branch tally

```
2b7768c  feat/file-mutation         shell-prompt slice 8
f09e358  feat/fcntl-misc            shell-prompt slice 7
4b7fd12  feat/stat-family           shell-prompt slice 6
bf8bc70  feat/ioctl                 shell-prompt slice 5
f48f05f  feat/time-syscalls         shell-prompt slice 4
324fd3a  feat/futex                 shell-prompt slice 3
1af0729  feat/vm-syscalls           shell-prompt slice 2
b2d22f8  feat/pipe-lifecycle-hook   shell-prompt slice 1
a490dc0  feat/fd-ops                fd-ops slice (8 commits earlier)
```

(Each branch is a strict superset of the prior; merging any one
into main brings along everything below it.)
