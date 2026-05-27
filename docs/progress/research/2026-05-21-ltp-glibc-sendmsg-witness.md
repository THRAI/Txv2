# LTP glibc sendmsg/sendmmsg/recvmmsg witness

Date: 2026-05-21

## Question

Can OSComp's glibc LTP payload validate `sendmsg(2)`, `sendmmsg(2)`, and
`recvmmsg(2)` without the musl wrapper faults seen in the musl `sendmsg01`
final cmsg case and musl `recvmmsg01` bad-msgvec case?

## Findings

- The canonical OSComp RV64 image contains a `/glibc` LTP tree with
  `sendmsg01`, `sendmmsg01`, `sendmmsg02`, and `recvmmsg01` plus glibc dynamic
  loader and libc. A focused `ltp-glibc` slim image can run these binaries
  after adding glibc loader/lib shims and a `/musl/glibc` boot path.
- glibc's RV64 `_Fork` issues `clone(SIGCHLD | CLONE_CHILD_SETTID |
  CLONE_CHILD_CLEARTID, 0, NULL, NULL, ctid)`. txKernel previously rejected
  that fork-like clone shape with `EINVAL`, so glibc LTP tests broke before
  reaching socket syscalls. Accepting these standard clone flags, writing the
  child tid, and recording `clear_child_tid` is a libc ABI prerequisite rather
  than a network-test workaround.
- `sendmsg01` initially passed all 15 socket subcases but returned a warning
  from LTP tmpdir cleanup. Root cause: `open(path, O_DIRECTORY | O_NOFOLLOW)`
  on a regular temp file must fail `ENOTDIR`; txKernel ignored
  `O_DIRECTORY`, so LTP tried to recurse into a regular file before cleanup.
- The focused LTP script generator was also printing `FAIL LTP CASE ... : 0`
  unconditionally. The generator now prints `PASS` for zero exit status and
  `FAIL` otherwise.
- `recvmmsg01` is now validated at libc level with glibc. Unlike the musl
  wrapper path, glibc reaches the kernel for the bad message-vector case, so
  LTP observes the intended `EFAULT` result instead of taking a userspace
  protection fault before `ecall`.
- Focused LTP scripts now keep the human-readable `PASS LTP CASE ... : 0`
  line and also emit the official OSComp legacy `FAIL LTP CASE ... : 0` line as
  the judge's end-of-case marker. The local `tools/oscomp-judge.py` also
  normalizes older PASS-only focused logs before invoking the unmodified OSComp
  LTP judge.

## Result

Focused glibc witness command:

```sh
cargo xtask oscomp slim-sdcard \
  --suite ltp-glibc \
  --ltp-cases sendmsg01,sendmmsg01,sendmmsg02 \
  --output target/oscomp/ltp-glibc-msg/sdcard-rv.img \
  --size-mb 512
timeout 180s cargo xtask oscomp qemu \
  --target rv64-qemu \
  --data target/oscomp/ltp-glibc-msg \
  --boot-suite ltp-glibc
```

Final serial result:

- `PASS LTP CASE sendmsg01 : 0`
- `PASS LTP CASE sendmmsg02 : 0`
- `PASS LTP CASE sendmmsg01 : 0`
- `txkernel:qemu-riscv64-virt:userspace:exited:0`

Follow-up `recvmmsg01` libc-level witness command:

```sh
cargo xtask oscomp slim-sdcard \
  --suite ltp-glibc \
  --ltp-cases recvmmsg01 \
  --output target/oscomp/ltp-glibc-recvmmsg/sdcard-rv.img \
  --size-mb 512
timeout 180s cargo xtask oscomp qemu \
  --target rv64-qemu \
  --data target/oscomp/ltp-glibc-recvmmsg \
  --boot-suite ltp-glibc
```

Final serial result:

- `recvmmsg01`: 10 passed, 0 failed, 0 broken, 0 warnings
- `PASS LTP CASE recvmmsg01 : 0`
- `FAIL LTP CASE recvmmsg01 : 0` as the OSComp LTP judge end marker
- `txkernel:qemu-riscv64-virt:userspace:exited:0`
- `cargo xtask oscomp score --target rv64-qemu --suite ltp-glibc` reports
  `ltp-glibc 10/10`.

## Interpretation

`sendmsg01`, `sendmmsg01`, `sendmmsg02`, and `recvmmsg01` are now valid glibc
LTP witnesses for txKernel socket semantics. The earlier musl `sendmsg01` final
cmsg case and musl `recvmmsg01` bad-msgvec case are still musl-wrapper issues:
they fault in userspace before syscall entry, while glibc reaches the kernel and
observes `EFAULT` correctly.
