#!/usr/bin/env bash
# suite-run.sh — run one OSComp suite against one kernel, scored by the real judge.
#
# Two things `cargo xtask oscomp test` cannot do, both mandatory here:
#   * `tx.oscomp.observe=0` — without it the bench-observe dump fires mid-suite
#     and `system_off`s the guest, truncating the Summary block into a false
#     regression (this is exactly what a first attempt produced: 14/102).
#   * boot against a COPY of the sdcard image — the suite mounts it writable,
#     so running straight off the shared testdata image mutates it.
#
# Usage: bash .v6work/suite-run.sh <suite> <rv|la> <kernel-elf> <tag>
#   e.g. bash .v6work/suite-run.sh netperf-musl rv target/oscomp/submit/kernel-rv head
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SUITE="${1:?suite}"; ARCH="${2:?rv|la}"; KERNEL="${3:?kernel elf}"; TAG="${4:?tag}"
DATA="$ROOT/target/oscomp/testdata"
WORK="$ROOT/.v6work/suites"; mkdir -p "$WORK"
IMG="$WORK/sd-$TAG-$SUITE-$ARCH.img"
LOG="$WORK/$TAG-$SUITE-$ARCH.log"
TMO="${SUITE_TIMEOUT:-600}"

cleanup() { [ -f "$IMG" ] && : > "$IMG"; }   # never rm; truncate the 4GB copy
trap cleanup EXIT

[ -f "$KERNEL" ] || { echo "FATAL: kernel missing: $KERNEL"; exit 2; }
[ -f "$DATA/sdcard-$ARCH.img" ] || { echo "FATAL: sdcard image missing"; exit 2; }

cp -f "$DATA/sdcard-$ARCH.img" "$IMG"
# `tx.oscomp.groups=` is the group SELECTOR (exec.rs oscomp_groups_from_cmdline).
# `tx.oscomp=<suite>` is NOT one — passing it silently boots the whole default
# group list (basic/busybox/libctest/libcbench/lua/lmbench/...), so the log ends
# up with no netperf/iperf section at all while the judge still prints a plausible
# looking score. That is how a first attempt "scored" netperf without ever
# running it.
CMDLINE="tx.oscomp.groups=$SUITE tx.oscomp.observe=0 console=ttyS0"

if [ "$ARCH" = rv ]; then
  timeout -s KILL "$TMO" qemu-system-riscv64 -machine virt -kernel "$KERNEL" \
    -m 1G -nographic -smp 1 -bios default \
    -drive "file=$IMG,if=none,format=raw,id=x0,file.locking=off" \
    -device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0 \
    -no-reboot -device virtio-net-device,netdev=net -netdev user,id=net \
    -rtc base=utc -append "$CMDLINE" 2>&1 | stdbuf -o0 tr -d '\000\r' > "$LOG"
else
  timeout -s KILL "$TMO" qemu-system-loongarch64 -kernel "$KERNEL" \
    -m 1G -nographic -smp 1 \
    -drive "file=$IMG,if=none,format=raw,id=x0,file.locking=off" \
    -device virtio-blk-pci,drive=x0 \
    -no-reboot -device virtio-net-pci,netdev=net0 -netdev user,id=net0 \
    -rtc base=utc -append "$CMDLINE" \
    -fw_cfg "name=opt/tx.cmdline,string=$CMDLINE" 2>&1 | stdbuf -o0 tr -d '\000\r' > "$LOG"
fi

TGT=rv64-qemu; [ "$ARCH" = la ] && TGT=la64-qemu
echo "== $TAG / $SUITE / $ARCH =="
( cd "$ROOT" && cargo -q xtask oscomp score --target "$TGT" --input "$LOG" --suite "$SUITE" 2>&1 ) \
  | grep -avE "^(warning|note|help|error: unused|  *\||  *=|[0-9]+ \|)" | tail -20
echo "log: $LOG"
