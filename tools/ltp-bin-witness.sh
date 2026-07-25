#!/bin/bash
# Four-lane official-form witness runner for LTP bin files.
#
# Boots one QEMU per lane with `tx.boot.mode=ltp
# tx.oscomp.groups=ltp-bin:<libc>:<files>`
# (the official ltp_testcode.sh no-args shape, see exec.rs), captures the
# colored serial log, and scores it with the REAL per-lane judge via
# tools/oscomp-judge.py. Each lane gets its own image copy so lanes can
# run in parallel without sharing a writable ext4.
#
# Usage: ltp-bin-witness.sh <timeout_secs> <lanes> <files(+joined)> [tag]
#   lanes: comma list from rv.musl rv.glibc la.musl la.glibc, or 'all'
#   files: e.g. getaddrinfo_01+in6_01+in6_02+asapi_02
#
# Logs + judge output: target/oscomp/ltp-bin/<tag>-<lane>.{log,judge}
set -u
cd "$(dirname "$0")/.." || exit 1
TMO="${1:?timeout_secs}"; LANES="${2:?lanes}"; FILES="${3:?files}"; TAG="${4:-witness}"
[ "$LANES" = all ] && LANES="rv.musl,rv.glibc,la.musl,la.glibc"
OUTDIR=target/oscomp/ltp-bin
mkdir -p "$OUTDIR"

EXTRA_CMDLINE="${LTP_BIN_EXTRA_CMDLINE:-}"

run_lane() {
  local lane="$1"
  local arch="${lane%%.*}" libc="${lane##*.}"
  local img="$OUTDIR/sd-$TAG-$lane.img"
  local log="$OUTDIR/$TAG-$lane.log"
  local cmdline="tx.boot.mode=ltp tx.oscomp.groups=ltp-bin:$libc:$FILES${EXTRA_CMDLINE:+ $EXTRA_CMDLINE}"
  cp "target/oscomp/testdata/sdcard-${arch}.img" "$img" || return 1
  if [ "$arch" = rv ]; then
    ( timeout -s KILL "$TMO" qemu-system-riscv64 -machine virt \
      -kernel target/oscomp/submit/kernel-rv \
      -m 1G -nographic -smp 1 -bios default \
      -drive file="$img",if=none,format=raw,id=x0,file.locking=off \
      -device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0 \
      -no-reboot \
      -device virtio-net-device,netdev=net -netdev user,id=net \
      -rtc base=utc \
      -append "$cmdline" 2>&1 | stdbuf -o0 tr -d '\000\r' > "$log" ) &
  else
    ( timeout -s KILL "$TMO" qemu-system-loongarch64 \
      -kernel target/oscomp/submit/kernel-la \
      -m 1G -nographic -smp 1 \
      -drive file="$img",if=none,format=raw,id=x0,file.locking=off \
      -device virtio-blk-pci,drive=x0 \
      -no-reboot \
      -device virtio-net-pci,netdev=net0 -netdev user,id=net0 \
      -rtc base=utc \
      -append "$cmdline" 2>&1 | stdbuf -o0 tr -d '\000\r' > "$log" ) &
  fi
  local runner=$!
  # grep gate: the boot never powers off on its own — kill this lane's
  # QEMU (matched by its unique image path) once the GROUP END marker
  # lands, so fast tests don't burn the whole timeout.
  local waited=0
  while kill -0 "$runner" 2>/dev/null && [ "$waited" -lt "$TMO" ]; do
    sleep 5; waited=$((waited+5))
    if grep -aq "OS COMP TEST GROUP END ltp-" "$log" 2>/dev/null; then
      sleep 2
      pkill -f "file=$img" 2>/dev/null
      break
    fi
  done
  pkill -f "file=$img" 2>/dev/null
  wait "$runner" 2>/dev/null
  {
    echo "=== $lane (files=$FILES) ==="
    python3 tools/oscomp-judge.py "$log" target/oscomp/testdata 2>/dev/null \
      | sed -n '/^\[ltp-/,/^$/p'
  } > "$OUTDIR/$TAG-$lane.judge"
}

pids=()
for lane in $(printf '%s' "$LANES" | tr , ' '); do
  run_lane "$lane" &
  pids+=("$!")
done
rc=0
for pid in "${pids[@]}"; do wait "$pid" || rc=1; done
for lane in $(printf '%s' "$LANES" | tr , ' '); do
  cat "$OUTDIR/$TAG-$lane.judge" 2>/dev/null
done
exit "$rc"
