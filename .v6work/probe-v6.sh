#!/usr/bin/env bash
# probe-v6.sh — Phase-0 evidence harness for the IPv6 external dataplane.
#
# Boots the rv64 guest under QEMU with slirp IPv6 enabled, runs a tagged probe
# script inside the guest (every line prefixed `^V6:`), and dumps the host-side
# pcap so TX/RX can be judged from the wire, not from guesses.
#
# Two SEPARATE host servers so the v4 control lane and the v6 lane share no
# socket-option variable:
#   * PORT4 — AF_INET  bound 0.0.0.0 (identical to tools/verify-git-net.sh)
#   * PORT6 — AF_INET6 bound ::      (v6only, no v4-mapped ambiguity)
# Both speak HTTP (TCP) and a UDP echo on the same port number.
#
# Usage: bash .v6work/probe-v6.sh [<guest-script>]
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
K="$ROOT/target/riscv64gc-unknown-none-elf/debug/tx-kernel-riscv64-qemu-virt"
IMG="$ROOT/local-images/alpine-linux-riscv64-ext4fs.img"
WORK="$ROOT/.v6work/run"
GUEST_SRC="${1:-$ROOT/.v6work/guest-v6.sh}"
PORT4=$(( (RANDOM % 2000) + 21000 ))
PORT6=$(( PORT4 + 1 ))
SERIAL="$WORK/serial.log"
PCAP="$WORK/net.pcap"
SRV4_PID=""; SRV6_PID=""

cleanup() {
  [ -n "$SRV4_PID" ] && kill "$SRV4_PID" 2>/dev/null
  [ -n "$SRV6_PID" ] && kill "$SRV6_PID" 2>/dev/null
  # NEVER rm: truncate the 723MB image copy instead.
  [ -f "$WORK/disk.img" ] && : > "$WORK/disk.img"
}
trap cleanup EXIT

mkdir -p "$WORK"
[ -f "$IMG" ] || { echo "FATAL: image missing: $IMG"; exit 2; }
[ -f "$K" ]   || { echo "FATAL: kernel missing: $K"; exit 2; }

echo "== IPv6 probe =="
echo "kernel : $K"
echo "ports  : v4=$PORT4 v6=$PORT6"

cat > "$WORK/srv.py" <<'PYEOF'
import http.server, socket, socketserver, sys, threading

FAM = socket.AF_INET6 if sys.argv[1] == "v6" else socket.AF_INET
BIND = "::" if sys.argv[1] == "v6" else "0.0.0.0"
PORT = int(sys.argv[2])
TAG = sys.argv[1].upper()

class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = ("HTTPOK-%s-%s\n" % (TAG, self.path)).encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/plain")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, *a):
        sys.stderr.write("HTTP %s %s from %s\n" % (TAG, self.path, self.client_address[0]))

class S(socketserver.ThreadingTCPServer):
    address_family = FAM
    allow_reuse_address = True

def udp_echo():
    s = socket.socket(FAM, socket.SOCK_DGRAM)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind((BIND, PORT))
    while True:
        data, addr = s.recvfrom(2048)
        sys.stderr.write("UDP %s %r from %s\n" % (TAG, data[:40], addr[0]))
        s.sendto(b"UDPOK-" + TAG.encode() + b":" + data, addr)

threading.Thread(target=udp_echo, daemon=True).start()
S((BIND, PORT), H).serve_forever()
PYEOF
python3 "$WORK/srv.py" v4 "$PORT4" >"$WORK/srv4.log" 2>&1 & SRV4_PID=$!
python3 "$WORK/srv.py" v6 "$PORT6" >"$WORK/srv6.log" 2>&1 & SRV6_PID=$!
sleep 1
curl -s --max-time 3 "http://127.0.0.1:$PORT4/hostcheck" >/dev/null && echo "host   : v4 server UP" || echo "host   : v4 server DOWN"
curl -s --max-time 3 "http://[::1]:$PORT6/hostcheck"     >/dev/null && echo "host   : v6 server UP" || echo "host   : v6 server DOWN"

sed "s/__PORT4__/$PORT4/g; s/__PORT6__/$PORT6/g" "$GUEST_SRC" > "$WORK/tx-run.sh"

# Optional host-side probe: fires N seconds after boot (used to drive slirp
# hostfwd INTO a guest listener). Result lands in $WORK/hostprobe.log.
if [ -n "${HOST_PROBE_CMD:-}" ]; then
  ( sleep "${HOST_PROBE_DELAY:-70}"; eval "${HOST_PROBE_CMD}" ) >"$WORK/hostprobe.log" 2>&1 &
fi

cp -f "$IMG" "$WORK/disk.img"
debugfs -w -R "write $WORK/tx-run.sh /tx-run.sh" "$WORK/disk.img" >/dev/null 2>&1

echo "booting guest (timeout ${TXTIMEOUT:-260}s)..."
timeout "${TXTIMEOUT:-260}" qemu-system-riscv64 -machine virt -kernel "$K" -m 1G -nographic -smp 1 -bios default \
  -global virtio-mmio.force-legacy=false \
  -drive "file=$WORK/disk.img,if=none,format=raw,id=x0,file.locking=off" \
  -device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0 \
  -device virtio-net-device,netdev=net,bus=virtio-mmio-bus.1 \
  -netdev "user,id=net,${NETDEV_OPTS:-ipv4=on,ipv6=on}" \
  -object "filter-dump,id=d,netdev=net,file=$PCAP" \
  -no-reboot -rtc base=utc -append "tx.runsh=/musl/tx-run.sh console=ttyS0" > "$SERIAL" 2>&1

echo ""
echo "== tagged guest output =="
grep -a "^V6:" "$SERIAL" || echo "(none — see $SERIAL)"
echo ""
if [ -f "$WORK/hostprobe.log" ]; then
  echo "== host-side probe (into the guest) =="
  sed 's/^/  hp| /' "$WORK/hostprobe.log"
  echo ""
fi
echo "== host server logs =="
sed 's/^/  v4| /' "$WORK/srv4.log" 2>/dev/null | head -10
sed 's/^/  v6| /' "$WORK/srv6.log" 2>/dev/null | head -10
echo ""
echo "serial: $SERIAL"
echo "pcap  : $PCAP   (tcpdump -r ... -nn)"
