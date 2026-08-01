#!/usr/bin/env bash
# wget-real.sh — drive busybox wget against the REAL internet from the guest.
#
# Unlike the loopback/host-server probes, this exercises real DNS, real RTT,
# real redirects, real CA chains and sustained multi-MB receive — and it judges
# by sha256 against host-computed baselines, not by exit code (a truncated
# download still exits 0).
#
# Usage: bash .v6work/wget-real.sh
set -u
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
K="${TX_KERNEL:-$ROOT/target/riscv64gc-unknown-none-elf/debug/tx-kernel-riscv64-qemu-virt}"
IMG="$ROOT/local-images/alpine-linux-riscv64-ext4fs.img"
WORK="$ROOT/.v6work/run"; mkdir -p "$WORK"
SERIAL="$WORK/wget-real.log"
PCAP="$WORK/wget-real.pcap"

cleanup() { [ -f "$WORK/wget-disk.img" ] && : > "$WORK/wget-disk.img"; }
trap cleanup EXIT

[ -f "$K" ] || { echo "FATAL: kernel missing"; exit 2; }
echo "kernel : $K ($(stat -c%y "$K" | cut -d. -f1))"

PORT=$(( (RANDOM % 2000) + 26000 ))
( cd "$ROOT/../../../../../tmp/x" 2>/dev/null || cd /tmp/claude-1000/-home-msp-learning-Txv2--claude-worktrees-ipv6-external/e383cdfe-ae81-4e07-8e01-cedeec9c726b/scratchpad/srv; python3 -m http.server "$PORT" --bind 0.0.0.0 ) >/dev/null 2>&1 &
HTTPSRV=$!
trap '[ -n "${HTTPSRV:-}" ] && kill $HTTPSRV 2>/dev/null; [ -f "$WORK/wget-disk.img" ] && : > "$WORK/wget-disk.img"' EXIT
sleep 1
sed -i "s/__PORT__/$PORT/g" "$ROOT/.v6work/guest-wget.sh"
echo "host http srv: port $PORT"
cp -f "$IMG" "$WORK/wget-disk.img"
debugfs -w -R "write $ROOT/.v6work/guest-wget.sh /tx-run.sh" "$WORK/wget-disk.img" >/dev/null 2>&1
[ -f "$ROOT/.v6work/ipcbench" ] && debugfs -w -R "write $ROOT/.v6work/ipcbench /ipcbench" "$WORK/wget-disk.img" >/dev/null 2>&1

echo "booting (timeout ${TXTIMEOUT:-420}s) — real-internet wget battery..."
timeout "${TXTIMEOUT:-420}" qemu-system-riscv64 -machine virt -kernel "$K" -m 1G -nographic -smp 1 -bios default \
  -global virtio-mmio.force-legacy=false \
  -drive "file=$WORK/wget-disk.img,if=none,format=raw,id=x0,file.locking=off" \
  -device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0 \
  -device virtio-net-device,netdev=net,bus=virtio-mmio-bus.1 \
  -netdev user,id=net,ipv4=on,ipv6=on \
  -object "filter-dump,id=d,netdev=net,file=$PCAP" \
  -no-reboot -rtc base=utc -append "tx.runsh=/musl/tx-run.sh console=ttyS0" > "$SERIAL" 2>&1

echo ""
grep -a "^W:" "$SERIAL" || echo "(no tagged output — see $SERIAL)"
echo ""
echo "serial: $SERIAL"
echo "pcap  : $PCAP"
