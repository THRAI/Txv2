# Research: Alpine TCC lightweight compile smoke

**Date:** 2026-06-27

## Question

Can we replace the Alpine GCC startup probe with a lighter TCC-based compiler
test so txKernel can validate the guest compiler path without the 170M-270M GCC
rootfs and large-initramfs blocker?

## Result

Yes, for a first-stage object-compilation smoke, a deliberately
freestanding link-and-exec smoke, and a libc-backed musl link/run smoke using
explicit CRT objects from an ext4-mounted TCC development image. The checked-in witnesses are
`tools/shell-tests/alpine-tcc-compile-smoke.txt` and
`tools/shell-tests/alpine-tcc-freestanding-run-smoke.txt`,
`tools/shell-tests/alpine-tcc-musl-ext4-explicit-crt-smoke.txt`, and
`tools/shell-tests/alpine-tcc-musl-ext4-default-link-fails.txt`.

The smallest useful rootfs was built with:

```sh
TX_ALPINE_ROOTFS=target/rootfs/alpine-rv64-tcc-qemu \
TX_ALPINE_PACKAGES="busybox openrc tcc" \
  tools/images/fetch-alpine-rv64.sh

TX_ALPINE_ROOTFS=target/rootfs/alpine-rv64-tcc-qemu \
  cargo xtask image cpio --profile alpine --target rv64-qemu
```

That rootfs is 8.8M and the generated cpio is `20940 blocks` (about 10M).
It avoids the large Alpine initramfs materialisation failure seen with GCC.

Guest verification passed:

```sh
timeout 180s cargo xtask shell-test --target rv64-qemu \
  --profile alpine \
  --script tools/shell-tests/alpine-tcc-compile-smoke.txt
```

The witness reached the Alpine shell, ran `/usr/bin/tcc -v`, wrote a tiny
`/tmp/hello.c`, compiled it with:

```sh
/usr/bin/tcc -c /tmp/hello.c -o /tmp/hello.o
```

and confirmed `test -s /tmp/hello.o` with `tcc-object-status:0`.

The freestanding link-and-exec witness also passed from the same minimal rootfs:

```sh
timeout 180s cargo xtask shell-test --target rv64-qemu \
  --profile alpine \
  --script tools/shell-tests/alpine-tcc-freestanding-run-smoke.txt
```

It writes a tiny `_start` program, links it with:

```sh
/usr/bin/tcc -nostdlib /tmp/exit42.c -o /tmp/exit42
```

and then runs `/tmp/exit42`, which performs a raw RISC-V `exit(42)` ecall. The
guest observed `tcc-run-status:42`, proving the minimal path through TCC link,
ELF exec, userspace syscall entry, and process exit.

The ext4-backed musl development image was built from:

```sh
TX_ALPINE_ROOTFS=target/rootfs/alpine-rv64-tcc-dev-qemu \
TX_ALPINE_PACKAGES="busybox openrc tcc musl-dev tcc-dev" \
  tools/images/fetch-alpine-rv64.sh
```

and staged into `target/images/alpine-tcc-dev-root-rv64-qemu.ext4` with a 4K
ext4 block size. The 4K block size is required by the current `tx-ext4` pager;
a first 1K-block image reached `:block:ext4-superblock:ok` but failed
`mount_ext4_read_write` and printed `:mount:sdcard:ext4:err`. The working image
was created with:

```sh
/opt/homebrew/Cellar/e2fsprogs/1.47.4/sbin/mkfs.ext4 \
  -F -b 4096 -I 256 -L TXTCCDEV \
  -d target/rootfs/alpine-tcc-dev-ext4-stage \
  target/images/alpine-tcc-dev-root-rv64-qemu.ext4
```

`xtask shell-test` now has an explicit RV64-only extra-disk hook so Alpine can
boot from the lightweight initramfs while mounting the TCC development rootfs as
`/musl`:

```sh
timeout 220s cargo xtask shell-test --target rv64-qemu \
  --profile alpine \
  --extra-rv64-ext4 target/images/alpine-tcc-dev-root-rv64-qemu.ext4 \
  --script tools/shell-tests/alpine-tcc-musl-ext4-explicit-crt-smoke.txt
```

That witness observed `:mount:sdcard:ext4:ok`, linked a small `stdio` program
with TCC using explicit musl CRT objects and `-lc`:

```sh
/musl/lib/ld-musl-riscv64.so.1 \
  --library-path /musl/lib:/musl/usr/lib \
  /musl/usr/bin/tcc -nostdlib \
  /musl/usr/lib/crt1.o /musl/usr/lib/crti.o \
  /tmp/tccmain.c /musl/usr/lib/crtn.o \
  -I/musl/usr/include -L/musl/lib -L/musl/usr/lib -lc \
  -o /tmp/tccmain
```

and then ran `/tmp/tccmain`, printing `tcc-musl-linked` and returning
`explicit-crt-run-status:7`.

## Boundaries

The default TCC libc-backed link is still not green. Adding `musl-dev tcc-dev`
supplies the musl CRT/libc files, but when carried in cpio it grows the Alpine
initramfs to `78161 blocks` and reproduces
`txkernel:qemu-riscv64-virt:initramfs:fail:materialize_anon` before the shell
prompt. Moving the same rootfs to ext4 avoids that boot blocker and proves TCC
plus musl can link and execute with explicit CRT objects.

The remaining default-link gap is TCC packaging/search-path specific:
`tools/shell-tests/alpine-tcc-musl-ext4-default-link-fails.txt` shows the simple
link:

```sh
/musl/lib/ld-musl-riscv64.so.1 \
  --library-path /musl/lib:/musl/usr/lib \
  /musl/usr/bin/tcc -B/musl/usr/lib \
  -I/musl/usr/include -L/musl/usr/lib \
  /tmp/tccmain.c -o /tmp/tccmain
```

still reports `crt1.o`, `crti.o`, `libtcc1.a`, and `crtn.o` as not found. The
Alpine riscv64 `tcc-dev` package does not contain `libtcc1.a`; it provides
`libtcc.h`, `libtcc.pc`, `bcheck.o`, `bt-exe.o`, `bt-log.o`, `runmain.o`, and
TCC headers under `/usr/lib/tcc/include`.

## Next Step

Keep the TCC object smoke, freestanding link-and-exec smoke, explicit-CRT musl
link/run smoke, and default-link negative witness as the lightweight compiler
canaries. Next choices are:

1. package or synthesize the missing TCC private runtime (`libtcc1.a`) and fix
   TCC's `/musl` startup-file search path so the default `tcc file.c -o file`
   shape works; or
2. first fix the large-initramfs `materialize_anon` path if compiler
   development images must also boot from cpio instead of ext4.
