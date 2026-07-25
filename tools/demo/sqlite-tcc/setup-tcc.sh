#!/bin/sh
# Guest-side setup for the SQLite + TCC demo.

set -eu

TCC_DEV="${TCC_DEV:-/dev/block/vda}"
TCC_MOUNT="${TCC_MOUNT:-/mnt/tcc}"

mkdir -p "$TCC_MOUNT" /usr/bin /usr /lib

if [ ! -x "$TCC_MOUNT/usr/bin/tcc" ]; then
    mount -t ext4 "$TCC_DEV" "$TCC_MOUNT"
fi

if [ ! -x "$TCC_MOUNT/usr/bin/tcc" ]; then
    echo "setup-tcc: missing $TCC_MOUNT/usr/bin/tcc after mount" >&2
    exit 1
fi
if [ ! -f "$TCC_MOUNT/usr/lib/libtcc.so" ]; then
    echo "setup-tcc: missing $TCC_MOUNT/usr/lib/libtcc.so after mount" >&2
    exit 1
fi

mkdir -p "$TCC_MOUNT/tmp"

ln -sf "$TCC_MOUNT/usr/include" /usr/include
ln -sf "$TCC_MOUNT/usr/lib/tcc" /usr/lib/tcc
ln -sf "$TCC_MOUNT/usr/lib/crt1.o" /usr/lib/crt1.o
ln -sf "$TCC_MOUNT/usr/lib/crti.o" /usr/lib/crti.o
ln -sf "$TCC_MOUNT/usr/lib/crtn.o" /usr/lib/crtn.o
ln -sf "$TCC_MOUNT/usr/lib/libc.a" /usr/lib/libc.a
ln -sf "$TCC_MOUNT/usr/lib/libc.so" /usr/lib/libc.so
ln -sf "$TCC_MOUNT/usr/lib/libtcc.so" /usr/lib/libtcc.so
ln -sf "$TCC_MOUNT/lib/ld-musl-riscv64.so.1" /lib/ld-musl-riscv64.so.1
ln -sf "$TCC_MOUNT/usr/bin/tcc" /usr/bin/tcc
ln -sf "$TCC_MOUNT/usr/bin/tcc" /usr/bin/cc
ln -sfn "$TCC_MOUNT/tmp" /mnt/tmp
ln -sfn "$TCC_MOUNT" /tcc

echo "setup-tcc: mounted $TCC_DEV at $TCC_MOUNT"
echo "setup-tcc: tcc=/usr/bin/tcc"
echo "setup-tcc: disk=/tcc"
echo "setup-tcc: tmp=/mnt/tmp"
