#!/usr/bin/env bash
# pcsample.sh — run the slow HTTPS download and sample the guest PC through the
# QEMU monitor, then cluster the samples by symbol.
#
# Answers the one question the pcap cannot: during the multi-second stalls where
# our side sends nothing at all, is the guest BUSY (and where), or IDLE (a lost
# wakeup)? Needs no kernel change, unlike a syscall counter.
set -u
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
K="$ROOT/target/riscv64gc-unknown-none-elf/debug/tx-kernel-riscv64-qemu-virt"
IMG="$ROOT/local-images/alpine-linux-riscv64-ext4fs.img"
SP=/tmp/claude-1000/-home-msp-learning-Txv2--claude-worktrees-ipv6-external/e383cdfe-ae81-4e07-8e01-cedeec9c726b/scratchpad
WORK="$ROOT/.v6work/run"; mkdir -p "$WORK"
MON="$WORK/mon.sock"
SERIAL="$WORK/pcsample.log"
SAMPLES="$WORK/pc.txt"
TMO="${TXTIMEOUT:-400}"
TLSPORT=$(( (RANDOM % 2000) + 30000 ))

TLSPID=""; QPID=""
cleanup() {
  [ -n "$TLSPID" ] && kill "$TLSPID" 2>/dev/null
  [ -n "$QPID" ] && kill "$QPID" 2>/dev/null
  [ -f "$WORK/pc-disk.img" ] && : > "$WORK/pc-disk.img"
  [ -S "$MON" ] && : > "$MON" 2>/dev/null
}
trap cleanup EXIT

python3 "$SP/tlssrv.py" "$TLSPORT" >/dev/null 2>&1 & TLSPID=$!
sleep 1

cat > "$WORK/pc-run.sh" <<EOF
BB=/musl/bin/busybox
export PATH=/musl/usr/bin:/musl/bin:/usr/bin:/bin HOME=/musl/root
echo "W:begin:[https 522KB under PC sampling]"
S=\$(\$BB date +%s)
\$BB timeout 300 wget --no-check-certificate -T 280 -O /tmp/big \\
  https://10.0.2.2:$TLSPORT/APKINDEX.tar.gz >/tmp/big.err 2>&1; RC=\$?
E=\$(\$BB date +%s)
echo "W:https:[rc=\$RC bytes=\$(\$BB wc -c < /tmp/big 2>/dev/null) secs=\$((E-S))]"
echo "W:end:[]"
EOF
cp -f "$IMG" "$WORK/pc-disk.img"
debugfs -w -R "write $WORK/pc-run.sh /tx-run.sh" "$WORK/pc-disk.img" >/dev/null 2>&1

rm -f "$MON" 2>/dev/null
timeout "$TMO" qemu-system-riscv64 -machine virt -kernel "$K" -m 1G -nographic -smp 1 -bios default \
  -global virtio-mmio.force-legacy=false \
  -drive "file=$WORK/pc-disk.img,if=none,format=raw,id=x0,file.locking=off" \
  -device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0 \
  -device virtio-net-device,netdev=net,bus=virtio-mmio-bus.1 \
  -netdev user,id=net,ipv4=on,ipv6=on \
  -monitor "unix:$MON,server,nowait" \
  -no-reboot -rtc base=utc -append "tx.runsh=/musl/tx-run.sh console=ttyS0" > "$SERIAL" 2>&1 &
QPID=$!

# Wait for the download to actually start before sampling.
for _ in $(seq 1 60); do grep -aq "W:begin" "$SERIAL" 2>/dev/null && break; sleep 2; done
echo "sampling PC (monitor $MON)..."
: > "$SAMPLES"
python3 - "$MON" "$SAMPLES" "${SAMPLE_SECS:-180}" <<'PY'
import socket, sys, time
mon, out, dur = sys.argv[1], sys.argv[2], float(sys.argv[3])
end = time.time() + dur
n = 0
with open(out, "a") as f:
    while time.time() < end:
        try:
            s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            s.settimeout(3); s.connect(mon)
            time.sleep(0.15)
            s.recv(65536)
            s.sendall(b"info registers\n")
            time.sleep(0.25)
            data = s.recv(262144).decode("utf-8", "replace")
            s.close()
            for line in data.splitlines():
                if "pc " in line:
                    f.write(line.strip() + "\n"); n += 1; break
            f.flush()
        except Exception:
            time.sleep(0.5)
        time.sleep(0.3)
print("samples:", n)
PY
wait "$QPID" 2>/dev/null
echo ""
grep -a "^W:" "$SERIAL" || true
echo ""
echo "=== PC 采样聚类 ==="
grep -oE "pc +[0-9a-fx]+" "$SAMPLES" 2>/dev/null | awk '{print $2}' | sort | uniq -c | sort -rn | head -20 \
  | while read -r cnt pc; do
      sym=$(addr2line -f -e "$K" "$pc" 2>/dev/null | head -1)
      printf '%5d  %s  %s\n' "$cnt" "$pc" "${sym:-?}"
    done
echo ""
echo "serial: $SERIAL"
echo "samples: $SAMPLES ($(wc -l < "$SAMPLES" 2>/dev/null) 条)"
