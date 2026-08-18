#!/usr/bin/env bash
# Fixed-topology regression fixture behind tools/verify-git-net-la64.sh (la64-qemu).
#
# CRITICAL VALIDATION-SCOPE WARNING:
# This fixture boots a tmpfs root, mounts the competition ext4 image only as
# the `/musl` sidecar, and performs every Git filesystem operation below
# `/home`, which is tmpfs.  It is useful for Git/network/TLS/DNS/IRQ behavior,
# but its score MUST NOT be cited as evidence that a direct-root ext4 RW mount
# works, that JBD2 commit/replay/checkpoint is correct, or that any Git data
# survives a reboot.  Persistent-storage acceptance requires a separate
# direct-root ext4 RW -> write/clone -> fsync/sync -> reboot same image -> RO
# verification loop.
#
# Sets up a real git server on the host, boots the guest kernel under QEMU, runs
# the full git pipeline inside the guest, and prints PASS/FAIL per check with the
# raw evidence. Nothing is faked: every result is the guest's own git output,
# and the raw serial log is left on disk for inspection.
#
# Usage:   bash tools/verify-git-net-la64.sh
#          TX_GIT_NET_TIMEOUT=360 bash tools/verify-git-net-la64.sh
#          TX_REQUIRE_NET_IRQ=1 bash tools/verify-git-net-la64.sh
# Needs:   python3, openssl, git, qemu-system-loongarch64, debugfs (e2fsprogs) on the host.
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../../.." && pwd)"
K="$ROOT/target/loongarch64-unknown-none-softfloat/debug/tx-kernel-loongarch64-qemu-virt"
IMG="$ROOT/local-images/alpine-linux-loongarch64-ext4fs.img"
WORK="$(mktemp -d /tmp/verifygit-XXXXXX)"
HTTP_PORT=$(( (RANDOM % 2000) + 19000 ))
HTTPS_PORT=$(( HTTP_PORT + 1 ))
MARKER="VERIFY-MARKER-$$-$(date +%s)"
PUSHMARK="PUSH-MARKER-$$"
SERIAL="$WORK/serial.log"
HTTP_PID=""; HTTPS_PID=""
QEMU_TIMEOUT="${TX_GIT_NET_TIMEOUT:-300}"
REQUIRE_NET_IRQ="${TX_REQUIRE_NET_IRQ:-0}"
EXTRA_CMDLINE="${TX_EXTRA_CMDLINE:-}"
if [ "$REQUIRE_NET_IRQ" = "1" ]; then
  EXTRA_CMDLINE="${EXTRA_CMDLINE:+$EXTRA_CMDLINE }tx.net.irq_report=1"
fi

cleanup() {
  [ -n "$HTTP_PID" ] && kill "$HTTP_PID" 2>/dev/null
  [ -n "$HTTPS_PID" ] && kill "$HTTPS_PID" 2>/dev/null
  rm -f "$WORK/disk.img"
}
trap cleanup EXIT

say()  { printf '%s\n' "$*"; }
pass=0; fail=0
check() { # check "<name>" "<condition-result 0/1>" "<evidence>"
  if [ "$2" = "0" ]; then printf '  [PASS] %-22s %s\n' "$1" "$3"; pass=$((pass+1));
  else printf '  [FAIL] %-22s %s\n' "$1" "$3"; fail=$((fail+1)); fi
}

say "== Txv2 git verification =="
say "worktree : $ROOT"
[ -f "$IMG" ] || { say "FATAL: image not found: $IMG"; exit 2; }
if [ ! -f "$K" ]; then
  say "kernel ELF missing — building (cargo xtask build --target la64-qemu)..."
  ( cd "$ROOT" && cargo xtask build --target la64-qemu ) >"$WORK/build.log" 2>&1 \
    || { say "FATAL: build failed (see $WORK/build.log)"; exit 2; }
fi
say "kernel   : $K"
say "ports    : http=$HTTP_PORT https=$HTTPS_PORT   marker=$MARKER"
say ""

# --- host: a real bare repo seeded with a known marker, + self-signed cert ---
SH="$WORK/srv"; mkdir -p "$SH"
git init -q --bare -b master "$SH/test.git"
git -C "$SH/test.git" config http.receivepack true
SEED="$WORK/seed"; git init -q -b master "$SEED"
git -C "$SEED" config user.email v@v; git -C "$SEED" config user.name v
printf '%s\n' "$MARKER" > "$SEED/README"
git -C "$SEED" add -A; git -C "$SEED" commit -qm "C1 base"
git -C "$SEED" remote add origin "$SH/test.git"; git -C "$SEED" push -q origin master
openssl req -x509 -newkey rsa:2048 -keyout "$SH/key.pem" -out "$SH/cert.pem" -days 5 -nodes \
  -subj "/CN=txverify.test" \
  -addext "subjectAltName=DNS:txverify.test,IP:127.0.0.1" >/dev/null 2>&1

# --- host: minimal smart-HTTP(S) git server (shells out to git-http-backend) ---
cat > "$WORK/srv.py" <<'PYEOF'
import http.server, subprocess, os, sys, ssl
ROOT, PORT = sys.argv[1], int(sys.argv[2])
CERT = sys.argv[3] if len(sys.argv) > 3 else None
for c in ("/usr/lib/git-core/git-http-backend", "/usr/libexec/git-core/git-http-backend"):
    if os.path.exists(c):
        BACKEND = c; break
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self): self.cgi()
    def do_POST(self): self.cgi()
    def log_message(self, *a): pass
    def cgi(self):
        p, _, q = self.path.partition("?")
        env = dict(os.environ, GIT_HTTP_EXPORT_ALL="1", GIT_PROJECT_ROOT=ROOT, PATH_INFO=p,
                   QUERY_STRING=q, REQUEST_METHOD=self.command, REMOTE_USER="git",
                   REMOTE_ADDR="127.0.0.1", CONTENT_TYPE=self.headers.get("Content-Type", ""),
                   GIT_PROTOCOL=self.headers.get("Git-Protocol", ""))
        cl = self.headers.get("Content-Length")
        body = self.rfile.read(int(cl)) if cl else b""
        out = subprocess.run([BACKEND], input=body, env=env, capture_output=True).stdout
        hdr, sep, b = out.partition(b"\r\n\r\n")
        if not sep: hdr, sep, b = out.partition(b"\n\n")
        st, hs = 200, []
        for line in hdr.split(b"\n"):
            line = line.strip()
            if not line: continue
            k, _, v = line.partition(b":"); k, v = k.strip(), v.strip()
            if k.lower() == b"status":
                try: st = int(v.split()[0])
                except Exception: pass
            else: hs.append((k.decode(), v.decode()))
        self.send_response(st)
        for k, v in hs: self.send_header(k, v)
        self.send_header("Content-Length", str(len(b))); self.end_headers(); self.wfile.write(b)
srv = http.server.ThreadingHTTPServer(("0.0.0.0", PORT), H)
if CERT:
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER); ctx.load_cert_chain(CERT, sys.argv[4])
    srv.socket = ctx.wrap_socket(srv.socket, server_side=True)
srv.serve_forever()
PYEOF
python3 "$WORK/srv.py" "$SH" "$HTTP_PORT" >/dev/null 2>&1 & HTTP_PID=$!
python3 "$WORK/srv.py" "$SH" "$HTTPS_PORT" "$SH/cert.pem" "$SH/key.pem" >/dev/null 2>&1 & HTTPS_PID=$!
sleep 2
git ls-remote "http://127.0.0.1:$HTTP_PORT/test.git" >/dev/null 2>&1 && say "host: http server up" || say "host: http server DOWN"
git -c http.sslVerify=true -c http.sslCAInfo="$SH/cert.pem" ls-remote "https://127.0.0.1:$HTTPS_PORT/test.git" >/dev/null 2>&1 && say "host: https server up" || say "host: https server DOWN"

# --- guest script (ports/marker substituted in below) ---
cat > "$WORK/tx-run.sh" <<'GUESTEOF'
BB=/musl/bin/busybox
IP=/sbin/ip
[ -x "$IP" ] || IP=/usr/sbin/ip
WORKROOT="/home/txverify-$$"
# Deliberately tmpfs: do not reinterpret these results as an ext4 durability
# witness.  See the validation-scope warning at the top of this fixture.
export GIT_PAGER=cat HOME="$WORKROOT" GIT_EXEC_PATH=/musl/usr/libexec/git-core GIT_TEMPLATE_DIR= \
       GIT_CURL_VERBOSE=1 PATH=/musl/usr/bin:/musl/bin:/usr/bin:/bin
G="git -c gc.auto=0 -c maintenance.auto=false -c user.email=g@g -c user.name=guest"

# The portable network path deliberately leaves addresses and the FIB to
# userspace. Discover the sole physical interface and obtain the QEMU-user
# lease before exercising DNS or Git; never restore kernel-side SLIRP literals.
iface=""
iface_count=0
for net_path in /sys/class/net/*; do
    [ -e "$net_path" ] || continue
    candidate="${net_path##*/}"
    if [ "$candidate" != "lo" ]; then
        iface="$candidate"
        iface_count=$((iface_count + 1))
    fi
done
if [ "$iface_count" -gt 1 ]; then
    echo "VG:network:[fail reason=ambiguous-interface count=$iface_count]"
elif [ -z "$iface" ] || [ ! -e "/sys/class/net/$iface" ]; then
    echo "VG:network:[fail reason=no-non-loopback-interface]"
elif [ ! -x "$IP" ]; then
    echo "VG:network:[fail reason=missing-ip-command]"
elif [ ! -x /sbin/udhcpc ]; then
    echo "VG:network:[fail reason=missing-udhcpc]"
else
    "$IP" link set dev "$iface" up >/tmp/vg-link.log 2>&1
    link_rc=$?
    /sbin/udhcpc -f -q -n -t 3 -T 3 -i "$iface" \
        -s /usr/share/udhcpc/default.script >/tmp/vg-dhcp.log 2>&1
    dhcp_rc=$?
    lease="$("$IP" -4 addr show dev "$iface" 2>/dev/null | $BB awk '/inet / { split($2, n, "/"); print n[1]; exit }')"
    gateway="$("$IP" -4 route show default 2>/dev/null | $BB awk '/^default / { print $3; exit }')"
    if [ "$link_rc" -eq 0 ] && [ "$dhcp_rc" -eq 0 ] \
        && [ -n "$lease" ] && [ -n "$gateway" ]; then
        echo "VG:network:[ready iface=$iface lease=$lease gateway=$gateway]"
    else
        dhcp_last="$($BB tail -n 1 /tmp/vg-dhcp.log 2>/dev/null)"
        echo "VG:network:[fail reason=dhcp iface=$iface link_rc=$link_rc dhcp_rc=$dhcp_rc lease=$lease gateway=$gateway last=$dhcp_last]"
    fi
fi

$BB mkdir -p /etc "$WORKROOT" 2>/dev/null
echo "nameserver 10.0.2.3" > /etc/resolv.conf
echo "10.0.2.2 txverify.test" >> /etc/hosts
echo "VG:git_version:[$(git --version 2>&1 | $BB head -1)]"
# Task1: local init/add/commit/log with a real tracked file (cp avoids the shell ext4-write bug)
$BB mkdir -p "$WORKROOT/t1"
echo "LOCAL-FILE-CONTENT" > /tmp/lf; $BB cp /tmp/lf "$WORKROOT/t1/tracked.txt"
$G init -q "$WORKROOT/t1" >/dev/null 2>&1
$G -C "$WORKROOT/t1" add tracked.txt
$G -C "$WORKROOT/t1" commit -qm "LOCAL-COMMIT-MARKER" >/dev/null 2>&1
echo "VG:local_log:[$($G -C "$WORKROOT/t1" log --oneline 2>&1 | $BB head -1)]"
echo "VG:local_content:[$($G -C "$WORKROOT/t1" cat-file -p HEAD:tracked.txt 2>&1)]"
echo "VG:http_clone_out:[$($BB timeout 60 $G clone http://txverify.test:__HTTP_PORT__/test.git "$WORKROOT/H" 2>&1 | $BB tail -1)]"
echo "VG:http_readme:[$($G -C "$WORKROOT/H" cat-file -p HEAD:README 2>&1)]"
echo "VG:https_clone_out:[$($BB timeout 90 $G -c http.sslVerify=true -c http.sslCAInfo=/musl/txverify-cert.pem clone https://txverify.test:__HTTPS_PORT__/test.git "$WORKROOT/S" 2>&1 | $BB tail -1)]"
echo "VG:https_readme:[$($G -C "$WORKROOT/S" cat-file -p HEAD:README 2>&1)]"
# push (empty commit avoids the shell ext4-write-content bug; tests push transport)
$G -C "$WORKROOT/H" commit -q --allow-empty -m "__PUSHMARK__" >/dev/null 2>&1
echo "VG:push_out:[$($BB timeout 60 $G -C "$WORKROOT/H" push origin master 2>&1 | $BB tail -1)]"
# pull: S was cloned before the push above → pulling must fast-forward to the pushed commit
echo "VG:pull_out:[$($BB timeout 60 $G -C "$WORKROOT/S" -c pull.ff=only -c http.sslVerify=true -c http.sslCAInfo=/musl/txverify-cert.pem pull 2>&1 | $BB tail -1)]"
echo "VG:pull_log:[$($G -C "$WORKROOT/S" log --oneline 2>&1 | $BB head -1)]"
# DNS: query the SLIRP DNS directly. Do not couple this witness to whether an
# unrelated public host accepts or rejects a random high TCP port.
$BB timeout 30 $BB nslookup github.com 10.0.2.3 >/tmp/dns.txt 2>&1
dns_rc=$?
echo "VG:dns:[rc=$dns_rc $($BB grep -ai '^Name:.*github.com' /tmp/dns.txt 2>&1 | $BB head -1)]"
echo "VG:END"
GUESTEOF
sed -i "s/__HTTP_PORT__/$HTTP_PORT/g; s/__HTTPS_PORT__/$HTTPS_PORT/g; s/__PUSHMARK__/$PUSHMARK/g" "$WORK/tx-run.sh"

# --- boot the guest ---
cp -f "$IMG" "$WORK/disk.img" \
  || { say "FATAL: cannot create temporary guest disk: $WORK/disk.img"; exit 2; }
debugfs -w -R "write $WORK/tx-run.sh /tx-run.sh" "$WORK/disk.img" >/dev/null 2>&1
debugfs -w -R "write $SH/cert.pem /txverify-cert.pem" "$WORK/disk.img" >/dev/null 2>&1
say "booting guest under QEMU (≈60-120s)..."
timeout "$QEMU_TIMEOUT" qemu-system-loongarch64 -machine virt -cpu la464 -kernel "$K" -m 1152M -nographic -smp 1 \
  -drive "file=$WORK/disk.img,if=none,format=raw,id=x0,file.locking=off" \
  -device virtio-blk-pci-non-transitional,drive=x0,addr=1 \
  -device virtio-net-pci,netdev=net,addr=2 -netdev user,id=net \
  -no-reboot -rtc base=utc \
  -fw_cfg "name=opt/tx.cmdline,string=tx.runsh=/musl/tx-run.sh tx.net.mode=dhcp console=ttyS0${EXTRA_CMDLINE:+ $EXTRA_CMDLINE}" \
  -append "tx.runsh=/musl/tx-run.sh tx.net.mode=dhcp console=ttyS0${EXTRA_CMDLINE:+ $EXTRA_CMDLINE}" > "$SERIAL" 2>&1
say ""

g() { grep -a "^VG:$1:" "$SERIAL" 2>/dev/null | sed "s/^VG:$1://" | head -1; }
say "== results =="
v=$(g network);       case "$v" in "[ready "*) check "Network DHCP/FIB" 0 "$v";; *) check "Network DHCP/FIB" 1 "${v:-<no network line>}";; esac
v=$(g git_version);   case "$v" in *git\ version*) check "Task0 git binary" 0 "$v";; *) check "Task0 git binary" 1 "$v";; esac
v=$(g local_log);     case "$v" in *LOCAL-COMMIT-MARKER*) check "Task1 init/add/commit" 0 "$v";; *) check "Task1 init/add/commit" 1 "$v";; esac
v=$(g local_content); case "$v" in *LOCAL-FILE-CONTENT*) check "Task1 file content" 0 "$v";; *) check "Task1 file content" 1 "$v";; esac
v=$(g http_readme);   case "$v" in *$MARKER*) check "Task2 clone (HTTP)" 0 "$v";; *) check "Task2 clone (HTTP)" 1 "$v";; esac
v=$(g https_readme);  case "$v" in *$MARKER*) check "Task2 clone (HTTPS/TLS)" 0 "$v";; *) check "Task2 clone (HTTPS/TLS)" 1 "$v";; esac
# push: verify the host bare repo actually received the guest's commit
if git -C "$SH/test.git" log --format=%s 2>/dev/null | grep -q "$PUSHMARK"; then
  check "Task2 push" 0 "host bare repo received '$PUSHMARK'"
else
  check "Task2 push" 1 "push_out=$(g push_out)"
fi
v=$(g pull_log);      case "$v" in *$PUSHMARK*) check "Task2 pull" 0 "$v";; *) check "Task2 pull" 1 "pull_out=$(g pull_out)";; esac
v=$(g dns);           case "$v" in *rc=0*github.com*) check "DNS resolution" 0 "$v";; *) check "DNS resolution" 1 "${v:-<no resolve line>}";; esac
if [ "$REQUIRE_NET_IRQ" = "1" ]; then
  irq_line=$(grep -a '^txkernel:.*:irq:net:' "$SERIAL" 2>/dev/null | tail -1)
  if printf '%s\n' "$irq_line" | awk -F: '
      NF >= 12 && $5 == "claims" && ($6 + 0) > 0 &&
      $7 == "completions" && $6 == $8 &&
      $9 == "wrong-hart" && ($10 + 0) == 0 &&
      $11 == "missing-device" && ($12 + 0) == 0 { ok = 1 }
      END { exit(ok ? 0 : 1) }
    '; then
    check "NET_IRQ claim/complete" 0 "$irq_line"
  else
    check "NET_IRQ claim/complete" 1 "${irq_line:-<no irq report>}"
  fi
fi

say ""
say "== summary: $pass passed, $fail failed =="
say "raw guest serial: $SERIAL   (grep '^VG:' for the tagged checks)"
[ "$fail" = "0" ]
