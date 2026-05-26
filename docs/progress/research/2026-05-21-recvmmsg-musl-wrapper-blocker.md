# LTP recvmmsg01 Musl Wrapper Blocker

Date: 2026-05-21

## Question

Why does the focused OSComp `ltp-musl` `recvmmsg01` case die with SIGSEGV
after the first EBADF subcase, and is this a txKernel `sys_recvmmsg` or network
stack bug?

## Finding

This is not a kernel-side `sys_recvmmsg` semantic failure. The failing musl
libc variant never reaches the kernel for the bad-`msgvec` subcase.

LTP's `recvmmsg01.c` intentionally builds a bad user pointer with
`tst_get_bad_addr(NULL)`, which maps a `PROT_NONE` page. The test expects
`recvmmsg(fd, bad_msgvec, ...)` to enter the kernel and return `EFAULT`.

In the OSComp musl payload, the libc `recvmmsg` wrapper pre-clears
`struct mmsghdr` padding before issuing `ecall`. Trap tracing mapped the
faulting user PC to musl `recvmmsg` at the instruction that stores zero to
`bad_addr + 44`. Because the page is `PROT_NONE`, userspace takes SIGSEGV
before the second `recvmmsg` syscall is issued. A kernel-only patch cannot
change this path without incorrectly weakening VM protection.

## Evidence

- Focused `ltp-musl` image for `sendmmsg02,sendmmsg01,recvmmsg01`:
  `sendmmsg02` and `sendmmsg01` pass all subcases; `recvmmsg01` passes the
  initial EBADF subcase and then dies in userspace.
- Trap trace for the failing run:
  the second `recvmmsg` syscall does not appear; the next event is a user
  store page fault at musl `recvmmsg`, with `a6` holding the bad `msgvec`
  pointer.
- Disassembly of OSComp musl `recvmmsg`:
  the wrapper stores to offsets inside each `mmsghdr` before `ecall` to adapt
  musl's userspace structure layout to the Linux kernel ABI layout.
- Raw syscall witness:
  a static RV64 probe calling syscall 243/269 directly passes EBADF, bad
  `msgvec` EFAULT, invalid timeout EINVAL, bad timeout pointer EFAULT, and a
  valid UDP `sendmmsg`/`recvmmsg` round trip.
- Glibc payload availability:
  the canonical OSComp RV64 image contains `/glibc/ltp/testcases/bin/recvmmsg01`
  and glibc libraries. Disassembly of glibc `recvmmsg` shows a direct `ecall`
  without pre-writing `msgvec`, so the glibc variant should be a valid
  libc-level witness once txKernel's OSComp boot/rootfs path can run it.

## Current Policy

Treat the raw syscall witness as the kernel semantic witness for
`sys_recvmmsg`. Do not special-case this musl LTP case in kernel code, and do
not make `PROT_NONE` mappings writable to let the musl wrapper continue.

If libc-level validation is required, discuss and add a focused glibc OSComp
boot/rootfs path instead of changing network-stack semantics.

## Verification Commands

```sh
cargo fmt --check
cargo test -p tx-shims --lib dispatch_sendmmsg_recvmmsg_udp_loopback_batch_round_trips -- --test-threads=1
cargo test -p tx-shims --lib dispatch_sendmmsg_recvmmsg_error_order_matches_socket_abi -- --test-threads=1
cargo build -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf
cargo xtask oscomp submit --target rv64-qemu
timeout 180s cargo xtask oscomp qemu --target rv64-qemu --data target/oscomp/ltp-net-layer4-mmsg --boot-suite ltp
timeout 180s cargo xtask oscomp qemu --target rv64-qemu --data target/oscomp/ltp-recvmmsg-raw --boot-suite ltp
cargo xtask progress validate
```
