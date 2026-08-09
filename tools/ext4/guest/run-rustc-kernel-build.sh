#!/bin/sh
set -eu

: "${TX_EXT4_TEST_DEVICE:?missing TEST role device}"
: "${TX_EXT4_WORKLOAD_DEVICE:?missing WORKLOAD role device}"
: "${TX_EXT4_TEST_MOUNT:=/mnt/ext4-test}"
: "${TX_EXT4_WORKLOAD_MOUNT:=/mnt/ext4-workload}"
: "${TX_EXT4_TOOLCHAIN_ROOT:=$TX_EXT4_WORKLOAD_MOUNT/toolchain}"
: "${TX_EXT4_CARGO_HOME:=$TX_EXT4_TEST_MOUNT/cargo-home}"
: "${TX_EXT4_CARGO_TARGET_DIR:=$TX_EXT4_TEST_MOUNT/target}"
: "${TX_EXT4_MUSL_LOADER:=/lib/ld-musl-riscv64.so.1}"
: "${TX_EXT4_RUSTC_WRAPPER:=$TX_EXT4_TEST_MOUNT/rustc-via-musl-loader}"
export TX_EXT4_MUSL_LOADER TX_EXT4_TOOLCHAIN_ROOT

if [ -r /proc/swaps ] && [ "$(wc -l < /proc/swaps)" -ne 1 ]; then
    echo "swap must be disabled" >&2
    exit 1
fi
test -b "$TX_EXT4_TEST_DEVICE" && test -b "$TX_EXT4_WORKLOAD_DEVICE"
mkdir -p "$TX_EXT4_TEST_MOUNT" "$TX_EXT4_WORKLOAD_MOUNT"
if ! test -f "$TX_EXT4_TEST_MOUNT/source/Cargo.lock"; then
    mount -t ext4 -o rw "$TX_EXT4_TEST_DEVICE" "$TX_EXT4_TEST_MOUNT" || mount -t ext4 -o remount,rw "$TX_EXT4_TEST_MOUNT"
fi
if ! test -f "$TX_EXT4_WORKLOAD_MOUNT/toolchain/bin/rustc"; then
    mount -t ext4 -o ro "$TX_EXT4_WORKLOAD_DEVICE" "$TX_EXT4_WORKLOAD_MOUNT"
fi
trap '/bin/busybox umount "$TX_EXT4_WORKLOAD_MOUNT" || true; /bin/busybox umount "$TX_EXT4_TEST_MOUNT" || true' EXIT
test -f "$TX_EXT4_TEST_MOUNT/source/Cargo.lock"
test -f "$TX_EXT4_TOOLCHAIN_ROOT/lib/libtx-rustc-resolv-preload.so"
test -x "$TX_EXT4_TOOLCHAIN_ROOT/bin/rustc"
test -x "$TX_EXT4_TOOLCHAIN_ROOT/bin/cargo"
test -x "$TX_EXT4_MUSL_LOADER"
test -f "$TX_EXT4_RUSTC_WRAPPER"
echo TX_GUEST_RUST_STAGE=preflight
PATH="$TX_EXT4_TOOLCHAIN_ROOT/bin:/bin" \
LD_PRELOAD="/lib/libgcompat.so.0:$TX_EXT4_TOOLCHAIN_ROOT/lib/libtx-rustc-resolv-preload.so${LD_PRELOAD:+:$LD_PRELOAD}" \
"$TX_EXT4_MUSL_LOADER" "$TX_EXT4_TOOLCHAIN_ROOT/bin/rustc" -vV
echo TX_GUEST_RUST_STAGE=rustc-version-ok
PATH="$TX_EXT4_TOOLCHAIN_ROOT/bin:/bin" \
LD_PRELOAD="/lib/libgcompat.so.0:$TX_EXT4_TOOLCHAIN_ROOT/lib/libtx-rustc-resolv-preload.so${LD_PRELOAD:+:$LD_PRELOAD}" \
CARGO_HOME="$TX_EXT4_CARGO_HOME" CARGO_TARGET_DIR="$TX_EXT4_CARGO_TARGET_DIR" \
CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0 RUSTC="$TX_EXT4_RUSTC_WRAPPER" \
"$TX_EXT4_MUSL_LOADER" "$TX_EXT4_TOOLCHAIN_ROOT/bin/cargo" build --release -vv -p tx-kernel-riscv64-qemu-virt --manifest-path "$TX_EXT4_TEST_MOUNT/source/Cargo.toml" --target riscv64gc-unknown-none-elf --offline --frozen
echo TX_GUEST_RUST_STAGE=cargo-build-ok
sync
